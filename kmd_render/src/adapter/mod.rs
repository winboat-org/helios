//! Adapter context — one per virtio-gpu device the driver binds to.
//!
//! Allocated in `DxgkDdiAddDevice`, populated in `DxgkDdiStartDevice`, freed in
//! `DxgkDdiRemoveDevice`. Dxgkrnl hands this back to us as the opaque
//! `MiniportDeviceContext` in every subsequent DDI call.

use alloc::boxed::Box;

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use wdk_sys::ntddk::{
    KeAcquireSpinLockRaiseToDpc, KeReleaseSpinLock, KeSetEvent, MmFreeContiguousMemory,
};
use wdk_sys::{KDPC, KEVENT, KSPIN_LOCK, KTIMER};

use crate::dxgk::*;
use crate::error::NotStarted;
use crate::virtio::VirtioGpu;
use helios_kmd_logic::DisplayMode;

mod backing;
mod kobj;
mod locks;
mod read_ledger;
mod scanout;
mod segments;

pub(crate) use backing::{SystemBackingSnapshot, SystemBackingTable};
pub(crate) use locks::{NotifyOrdered, ScanoutGuard, WddmNotifyGuard, WITH_VIRTIO_TORN};
pub(crate) use read_ledger::{
    dump_counters as read_ledger_dump_counters, reset_counters as read_ledger_reset_counters,
    ReadLedger, ScanoutEventReg, AQ_REGISTER_REFUSED, RD_MAP_REFUSED,
};
pub(crate) use scanout::{PresentStreamMarker, ScanoutRefreshQueue};
pub(crate) use segments::{BarSegment, PagingRam};

/// Everything `DxgkDdiStartDevice` establishes, as one value published once.
///
/// StartDevice used to take a unique `&mut AdapterContext` that stayed live for
/// the whole function and mutated ~a dozen plain fields through it — while the
/// context pointer had been public to dxgkrnl since AddDevice and THREE
/// concurrent agents build `&AdapterContext` from it. `start_vsync` arms the
/// fixed-phase one-shot
/// timer and `init_hpd` starts a thread that both immediately take `&self` from
/// the same address while the outer `&mut` is still in scope, and
/// `set_virtio(Some(gpu))` enables the device so the DIRQL ISR can fire
/// mid-function. That is an unambiguous Stacked-Borrows violation.
///
/// Split by LIFETIME, not by topic:
///
///   * the *sticky* half (everything outside `transport`) survives StopDevice.
///     This is load-bearing and must not be "tidied up": StopDevice today does
///     NOT clear the knobs, the mode or the EDID, and about two dozen sites
///     branch on `display_half`. Clearing them at StopDevice would flip all of
///     those from SUCCESS-shaped answers to NOT_SUPPORTED between StopDevice and
///     RemoveDevice, which is exactly what the restart-device leg of the
///     regression gate exercises.
///   * `transport`, which StopDevice clears, holds everything whose meaning dies
///     with the transport generation that produced it.
pub(crate) struct StartedState {
    /// Dxgkrnl callback interface, copied out of dxgkrnl's buffer at
    /// StartDevice. Read lock-free by the ISR and both DPCs; publishing it as
    /// part of this struct is what makes that visibility structural.
    pub dxgkrnl: DXGKRNL_INTERFACE,
    /// Every service-key knob, snapshotted once at StartDevice. See
    /// [`AdapterKnobs`].
    pub knobs: AdapterKnobs,
    /// The segment table this adapter REPORTS, built once in StartDevice from
    /// the same `BarSegment` value every other consumer reads.
    ///
    /// `query_segments` renders this; it does not re-derive one. That is what
    /// makes the count reported on the descriptor-NULL call and the descriptors
    /// written on the second call the same immutable value — the premise the old
    /// SAFETY comment stated as its conclusion.
    pub segment_table: crate::ddi::segment_table::SegmentTable,
    /// Scanout-0 mode the display half presents, together with the EDID
    /// generated from it. See [`ScanoutMode`].
    pub scanout_mode: ScanoutMode,
    /// Real-RAM-backed segment for VidMm page tables / paging buffers (segment 2).
    /// `None` if the contiguous allocation failed (then we fall back to the old
    /// single-segment shape). Freed in `AdapterContext::drop`.
    pub paging_ram: Option<PagingRam>,
    /// The half StopDevice clears. `None` between StopDevice and the next
    /// StartDevice.
    transport: UnsafeCell<Option<TransportGeneration>>,
}

/// Every service-key knob this driver reads, snapshotted ONCE per StartDevice.
///
/// One concept, one mechanism. Before this existed there were three:
/// `AllocCached`/`PresentProbe`/`ScForceReject`/`DisplayHalf` were snapshotted
/// here at StartDevice; `DirectFlipCaps` was re-read from the registry FOUR
/// times per AddAdapter (the caps path plus once inside each of the three
/// aperture descriptor writers); and `BarSegFlags`/`BarSegBaseMB` were read
/// *inside* `write_bar_knob_descriptor`, which also performed two synchronous
/// registry WRITES on every descriptor write — ungated by `DiagLevel`, because
/// `record_named` has no such gate. A function named "write this descriptor"
/// did blocking registry I/O in both directions.
///
/// Passing `&AdapterKnobs` to the descriptor writers removes their ability to do
/// I/O at all: they become pure functions of their arguments. It also closes a
/// real (if test-VM-only) inconsistency — `reg add ... /v DirectFlipCaps /d 1`
/// executed BETWEEN the DRIVERCAPS query and the segment query of a
/// `pnputil /restart-device` produced `SupportDirectFlip = 0` in the caps surface
/// while the aperture descriptor set the DirectFlip segment flag, an internally
/// inconsistent surface no diag record could explain because `0x01D7` bit 2 read 0.
///
/// Every field name here corresponds to a [`crate::diag::knobs`] entry, which is
/// where the ≤14-byte lookup-buffer rule is enforced at compile time.
/// Reported device-local VidMm capacity in MiB when `VidMmVramMB` is absent.
/// Named rather than inlined because it appears in both [`AdapterKnobs::DEFAULTS`]
/// and the registry read, and the two silently disagreeing is exactly the class
/// of bug this module exists to prevent. See the field's doc for the evidence.
pub(crate) const VIDMM_VRAM_MB_DEFAULT: u32 = 4096;

#[derive(Clone, Copy)]
pub(crate) struct AdapterKnobs {
    /// `AllocCached` (default 1). When set, CpuVisible allocations are
    /// additionally flagged `Cached` so dxgkrnl maps CPU views write-back
    /// instead of write-combined. The BAR window is RAM-backed host shmem (x86
    /// cache-coherent for all agents on the same physical pages); WC reads
    /// measured ~200 MB/s in the IDD readback (36 ms per 7.8 MiB frame,
    /// 2026-07-06). 0 = kill switch.
    pub alloc_cached: bool,
    /// `BindFlushMode` (default 0 = completion-ordered). 1 flushes immediately
    /// at the bind with no ordering — the A/B that separates "the buffer is not
    /// ready when we bind it" from "it is ready and our boundary is wrong".
    /// See `crate::diag::knobs::BIND_FLUSH_MODE`.
    pub bind_flush_immediate: bool,
    /// `DispatchBind` (default 1 = ON). The DISPATCH-level fast bind
    /// (ROADMAP defect 0ab-C, D1(ii)): the flip arm enqueues this flip's
    /// `SET_SCANOUT_BLOB` itself instead of waiting for the PASSIVE display
    /// worker to get to it, which measured a bimodal 1–3 ms / 10–14 ms bind
    /// cadence against a 4.8 ms flip cadence at 210 fps — every stall pushing
    /// some bind two frame periods past its present, onto a buffer the app had
    /// already re-cleared.
    ///
    /// A PURE ACCELERATOR: the worker still consumes `pending_vidpn_allocation`
    /// and still binds; the wire is FIFO, so whichever enqueue happens first
    /// decides the host-side bind moment and the later one is an idempotent
    /// re-bind of the same resource. 0 restores the worker-only cadence as the
    /// same-boot A/B lever. See `crate::diag::knobs::DISPATCH_BIND`.
    pub dispatch_bind: bool,
    /// `PresentProbe` (default 0). When enabled, each exact Present
    /// source/destination pair performs one bounded, fence-ordered CPU sample of
    /// the destination after its eighth submission. Diagnostic only: the
    /// steady-state path never waits or maps a frame, and the per-pair
    /// `probe_done` state statically prevents repeated readbacks.
    pub present_probe: bool,
    /// `DisplayHalf` (default 1 = ON — the render+display miniport IS the
    /// product; the hardware-accelerated desktop shipped on it). When nonzero,
    /// StartDevice advertises ONE video-present source + ONE child
    /// video-output and the VidPn/child DDIs in
    /// `ddi::display`/`ddi::vidpn`/`ddi::child` stand up a real virtual
    /// VidPn output + default monitor and drive virtio-gpu scanout, instead of
    /// returning NOT_SUPPORTED. 0 restores the boot-era render-only surface as
    /// the escape hatch (it was the default until 0ab-C closed and the display
    /// half became unconditional production). Demoted to 0 before publication
    /// if the transport never came up. Mirrored to `DspH`.
    pub display_half: bool,
    /// `DirectFlipCaps` (default 0 = deny direct flip everywhere — the truthful
    /// surface; see the `SupportDirectFlip` comment in `query_driver_caps`).
    /// Nonzero restores the legacy bring-up advertisement, in BOTH the adapter
    /// cap and the aperture segment flags, which is the point of reading it once.
    pub direct_flip: bool,
    /// `CrossAdaptCaps` (default 0). Nonzero advertises
    /// `DXGK_VIDMMCAPS.CrossAdapterResource` (tier-1 cross-adapter copy support).
    /// The compile-time `DECLARE_CROSS_ADAPTER_RESOURCE` this used to be OR'd
    /// with was a `const false` and went with T6/R905; this knob is now the only
    /// way to set the bit.
    pub cross_adapter: bool,
    /// `BarSegFlags` (default 0x1C = CacheCoherent | SupportsCpuHostAperture |
    /// SupportsCachedCpuHostAperture). The BAR descriptor's flag word.
    pub bar_seg_flags: u32,
    /// `BarSegBaseMB` (default 0). The BAR descriptor's GPU-physical
    /// `BaseAddress` in MiB; a nonzero value tests the BaseAddress-overlap
    /// hypothesis.
    pub bar_seg_base_mb: u32,
    /// `BarSegMode` (default 10). Parsed into
    /// `crate::ddi::bar_segment::BarSegTopology`; kept raw here so the coerced
    /// value can be reported.
    pub bar_seg_mode: u32,
    /// `VidMmVramMB` (**default 4096 since 22.22.252.0**). A valid nonzero value
    /// makes the existing BAR-backed memory segment report this device-local
    /// capacity and opts device-local Venus tracking allocations into that
    /// local segment. Non-local Vulkan heaps remain in the aperture/shared
    /// budget. The programmable CPU aperture stays capped separately; tracking
    /// allocations never map their identity blobs through it.
    ///
    /// The default was 0 (no local segment) while the heap-aware accounting was
    /// being brought up. 4096 is the configuration that work actually validated
    /// — Task Manager shows a 4.0 GiB dedicated capacity, and direct-KMT and
    /// native-Vulkan four-by-64 MiB probes each measured exactly +256.00 MiB in
    /// the selected segment with no movement in the other and a clean return to
    /// baseline after destroy (ROADMAP, "VidMm / Task Manager validation").
    /// Shipping 0 meant no install ever got the configuration that was proven.
    /// Re-confirmed on 22.22.251.0 before this flip: `VidVram=4096`,
    /// `VidVBad=0`, adapter problem code 0, GT1 221.37 fps, all scanout gates
    /// clean. Out-of-range values are refused into `VidVBad` and fall back to
    /// the legacy topology rather than silently collapsing.
    pub vidmm_vram_mb: u32,
}

impl AdapterKnobs {
    /// The value each knob takes when absent from the service key.
    ///
    /// Load-bearing, and it must not drift: with every knob absent this has to
    /// equal what [`Self::read`] produces, which is what makes the emitted
    /// descriptors byte-identical to the old per-writer `read_config_dword`
    /// calls. Kept as a named const because it is also the documented answer to
    /// "what does this driver do with no registry configuration at all".
    #[allow(dead_code)]
    pub const DEFAULTS: Self = Self {
        alloc_cached: true,
        bind_flush_immediate: false,
        dispatch_bind: true,
        present_probe: false,
        display_half: true,
        direct_flip: false,
        cross_adapter: false,
        bar_seg_flags: 0x1C,
        bar_seg_base_mb: 0,
        bar_seg_mode: 10,
        vidmm_vram_mb: VIDMM_VRAM_MB_DEFAULT,
    };

    /// Read every knob once. PASSIVE_LEVEL.
    ///
    /// Called twice per device lifetime: at AddAdapter (so the AddAdapter-time
    /// caps and segment queries answer from real registry values — a
    /// `DirectFlipCaps=1` A/B must still take effect before StartDevice runs,
    /// exactly as it did when each writer read the registry itself) and again in
    /// StartDevice through [`Self::read_at_start`], so `reg add` +
    /// `pnputil /restart-device` still picks up a change with no reboot.
    pub fn read() -> Self {
        use crate::diag::{knobs, read_config_dword};
        Self {
            alloc_cached: read_config_dword(knobs::ALLOC_CACHED, 1) != 0,
            bind_flush_immediate: read_config_dword(knobs::BIND_FLUSH_MODE, 0) == 1,
            dispatch_bind: read_config_dword(knobs::DISPATCH_BIND, 1) != 0,
            present_probe: read_config_dword(knobs::PRESENT_PROBE, 0) != 0,
            display_half: read_config_dword(knobs::DISPLAY_HALF, 1) != 0,
            direct_flip: read_config_dword(knobs::DIRECT_FLIP_CAPS, 0) != 0,
            cross_adapter: read_config_dword(knobs::CROSS_ADAPT_CAPS, 0) != 0,
            bar_seg_flags: read_config_dword(knobs::BAR_SEG_FLAGS, 0x1C),
            bar_seg_base_mb: read_config_dword(knobs::BAR_SEG_BASE_MB, 0),
            bar_seg_mode: read_config_dword(knobs::BAR_SEG_MODE, 10),
            vidmm_vram_mb: read_config_dword(knobs::VIDMM_VRAM_MB, VIDMM_VRAM_MB_DEFAULT),
        }
    }

    /// [`Self::read`] plus the fixed-name breadcrumbs that mirror the knobs.
    /// StartDevice only, so they cost one registry write per start — the
    /// `BarF`/`BarB` pair used to be written on every descriptor write, from
    /// inside `write_bar_knob_descriptor`, ungated by `DiagLevel`. Names and
    /// values unchanged.
    pub fn read_at_start() -> Self {
        let knobs = Self::read();
        crate::diag::record_named_bytes(b"AlcC", knobs.alloc_cached as u32);
        crate::diag::record_named_bytes(b"BndFM", knobs.bind_flush_immediate as u32);
        crate::diag::record_named_bytes(b"DspBnd", knobs.dispatch_bind as u32);
        crate::diag::record_named_bytes(b"PBPrEn", knobs.present_probe as u32);
        crate::diag::record_named_bytes(b"DspH", knobs.display_half as u32);
        crate::diag::record_named_bytes(b"BarF", knobs.bar_seg_flags);
        crate::diag::record_named_bytes(b"BarB", knobs.bar_seg_base_mb);
        crate::diag::record_named_bytes(b"BarM", knobs.bar_seg_mode);
        crate::diag::record_named_bytes(b"VidVram", knobs.vidmm_vram_mb);
        crate::diag::record_named_bytes(b"VidVBad", 0);
        knobs
    }
}

impl StartedState {
    /// Build the sticky half DIRECTLY ON THE HEAP.
    ///
    /// ⚠ STACK BUDGET — this is not a style preference. `DxgkDdiStartDevice`
    /// calls `VirtioGpu::init`, whose frame is 3.0 KB even after the queue/error
    /// reductions, and the x64 kernel stack is 24 KB total. Building this
    /// 832-byte struct (which embeds a 576-byte `DXGKRNL_INTERFACE`) in
    /// StartDevice's frame — and passing it
    /// there by value, which an unoptimised build materialises several times —
    /// took StartDevice from 8824 to 9688 bytes and the nested pair to ~18.8 KB.
    /// That overflowed the kernel stack during boot, where dxgkrnl's own frames
    /// above us are deeper than on a live `devcon` restart: an early double
    /// fault with no dump, presenting as `0xc0000001` at the recovery screen.
    ///
    /// `#[inline(never)]` is load-bearing: it keeps the temporary in THIS
    /// frame, which is transient and does not overlap `VirtioGpu::init`.
    /// Callers must never bind the value — only the `Box`.
    ///
    /// The dxgkrnl interface is taken as a POINTER and dereferenced here, so the
    /// 576-byte copy goes straight into the heap allocation instead of living in
    /// StartDevice for the whole call.
    ///
    /// The transport half always starts empty and is installed separately by
    /// [`AdapterContext::set_transport_generation`], so "a state published with a
    /// stale transport generation" is unrepresentable.
    ///
    /// # Safety
    /// `dxgkrnl` must point to the live `DXGKRNL_INTERFACE` dxgkrnl passed to
    /// `DxgkDdiStartDevice`, valid for this call.
    #[inline(never)]
    pub(crate) unsafe fn boxed(
        dxgkrnl: *const DXGKRNL_INTERFACE,
        knobs: AdapterKnobs,
        scanout_mode: ScanoutMode,
        paging_ram: Option<PagingRam>,
        segment_table: crate::ddi::segment_table::SegmentTable,
    ) -> Box<Self> {
        Box::new(Self {
            // SAFETY: per the fn contract; a plain POD copy of dxgkrnl's buffer.
            dxgkrnl: unsafe { *dxgkrnl },
            knobs,
            scanout_mode,
            paging_ram,
            segment_table,
            transport: UnsafeCell::new(None),
        })
    }
}

/// The scan-out mode the display half presents, and the EDID that describes it.
///
/// The two used to be three independent fields — `display_w`, `display_h` and a
/// 128-byte array — whose mutual consistency ("every VidPn mode + the generated
/// EDID derive from this so they stay cofunctional") was a comment. A future
/// write to the extent without regenerating the EDID would produce a monitor
/// whose detailed timing disagrees with the modes the VidPn DDIs enumerate:
/// the mismatch class that produced the mode-set retry loops.
///
/// The only constructor generates the EDID from the mode, so there is no way to
/// obtain a `ScanoutMode` whose EDID disagrees with its extent.
#[derive(Clone, Copy)]
pub(crate) struct ScanoutMode {
    mode: DisplayMode,
    edid: [u8; 128],
}

impl ScanoutMode {
    /// Adopt the host's extent if usable, else the 1920×1080 fallback, and
    /// generate the matching EDID.
    ///
    /// `host` is `VirtioGpu::display_mode`'s answer — note that method and
    /// `AdapterContext::display_mode` are different methods with the same name;
    /// only the latter reads this value.
    pub(crate) fn adopt(host: Option<(u32, u32)>) -> Self {
        let mode = host
            .and_then(|(w, h)| DisplayMode::from_host(w, h))
            .unwrap_or(DEFAULT_SCANOUT_EXTENT);
        Self {
            mode,
            edid: crate::ddi::vidpn::build_edid(mode.width(), mode.height()),
        }
    }

    /// A zeroed-EDID mode for the render-only surface, where no monitor is
    /// advertised and `QueryDeviceDescriptor` answers NOT_SUPPORTED.
    pub(crate) fn render_only() -> Self {
        Self {
            mode: DEFAULT_SCANOUT_EXTENT,
            edid: [0u8; 128],
        }
    }

    pub(crate) fn edid(&self) -> &[u8; 128] {
        &self.edid
    }
}

/// The 1920×1080 fallback as an already-validated value, so nothing on the
/// display path needs an `unwrap` on a constant that cannot fail.
///
/// `DisplayMode::FALLBACK` is a total const expression with no panic path — see
/// its doc — and `kmd_logic`'s host tests pin it to the documented extent and to
/// `vidpn::DEFAULT_MODE_*`.
const DEFAULT_SCANOUT_EXTENT: DisplayMode = DisplayMode::FALLBACK;

/// The two fallback constants must not drift apart.
const _: () = {
    assert!(crate::ddi::vidpn::DEFAULT_MODE_WIDTH == helios_kmd_logic::FALLBACK_DISPLAY_WIDTH);
    assert!(crate::ddi::vidpn::DEFAULT_MODE_HEIGHT == helios_kmd_logic::FALLBACK_DISPLAY_HEIGHT);
};

/// State whose meaning dies with the transport generation that produced it.
///
/// Every resource id in here is meaningless in the next generation, whose ids
/// restart at 1 and whose liveness test is bare membership — which is how a
/// recycled id used to be accepted as the cached LINEAR scan-out target.
pub(crate) struct TransportGeneration {
    /// BAR memory segment — the head partition of the host-visible
    /// window, reserved as dxgkrnl's CPU-host-aperture region at StartDevice.
    /// `None` if the window is absent/too small; the BAR segment is then not
    /// reported and standard allocations stay on the aperture (old behavior).
    pub bar_segment: Option<BarSegment>,
    /// The persistent venus 3D context id (`VIRTIO_GPU_CAPSET_VENUS`) the venus
    /// client rides, created in StartDevice and destroyed in StopDevice. `0` = none.
    pub venus_ctx_id: u32,
}

pub struct AdapterContext {
    /// Service-key knobs as read at **AddAdapter**, for the window before
    /// StartDevice has published its own snapshot.
    ///
    /// That window is not hypothetical: dxgkrnl issues the DRIVERCAPS and
    /// QUERYSEGMENT queries during AddAdapter, so this is what those answer
    /// from. Written once in [`Self::new`], before the context is published, and
    /// never mutated — which is why it needs no `UnsafeCell` and no ordering,
    /// unlike everything below it. Reads go through [`Self::knobs`].
    initial_knobs: AdapterKnobs,
    /// `NbSegment` as reported on the descriptor-NULL call of the two-call
    /// QUERYSEGMENT protocol — the number of descriptor slots dxgkrnl then
    /// allocates. The descriptor pass clamps to this, because only dxgkrnl knows
    /// how many slots it allocated and this is the number we told it.
    reported_segment_count: AtomicU32,
    /// Everything StartDevice establishes, published ONCE. See [`StartedState`].
    ///
    /// Reached only through [`Self::started`]; there is no `&mut` path to it, so
    /// "mutate a started field from a DDI" does not compile and "read `dxgkrnl`
    /// before StartDevice" does not compile either (there is no `StartedState` to
    /// borrow).
    /// BOXED. The state is 832 bytes and embeds a 576-byte `DXGKRNL_INTERFACE`;
    /// keeping it behind a pointer is what stops it — and its by-value
    /// construction temporaries — from landing on `DxgkDdiStartDevice`'s stack
    /// frame. See `StartedState::boxed`.
    started: UnsafeCell<Option<Box<StartedState>>>,
    /// Publication flag for `started`. A reader that observes 1 with Acquire has
    /// necessarily seen every store StartDevice made into the state — which makes
    /// the callback-table visibility ordering structural for ALL THREE readers,
    /// not just the ISR. Two of them (the device DPC and the VSync DPC) read the
    /// table with no `isr_status` guard at all and used to rest on statement
    /// order plus a comment.
    started_published: AtomicU32,
    /// Last fence completed by the bring-up scheduler path.
    last_completed_fence: AtomicU32,
    /// Serializes DMA_COMPLETED notification and its monotonic fence update.
    /// A DPC can take an older ready fence out of the virtio FIFO while a new
    /// SubmitCommand concurrently takes the immediate-completion path; without
    /// this lock the newer fence can reach VidSch first and the delayed older
    /// notify bugchecks 0x119/1 (invalid fence id).
    wddm_notify_lock: UnsafeCell<KSPIN_LOCK>,
    /// Mapped kernel VA of the virtio ISR-status register (read-to-clear), or 0
    /// until StartDevice wires it. `DxgkDdiInterruptRoutine` reads this at DIRQL to
    /// acknowledge the level-triggered INTx line (the device is `MSISupported=0`);
    /// without it the line stays asserted → interrupt storm → Windows disables the
    /// adapter (Code 43). Set once in StartDevice, read lock-free in the ISR — an
    /// atomic (not behind `virtio_lock`) because the ISR runs at DIRQL and cannot
    /// take the spinlock.
    pub isr_status: AtomicUsize,
    /// Serializes ALL access to `virtio` (the control virtqueue + the shared
    /// scratch page). Held by escape submissions at PASSIVE_LEVEL and, from M3.4,
    /// by the used-ring DPC at DISPATCH_LEVEL — a spinlock (not a mutex) is
    /// mandatory because the DPC path cannot block. `0` is the initialized +
    /// unlocked state of a `KSPIN_LOCK`, so no explicit `KeInitializeSpinLock` is
    /// required (same rationale as the BAR-mapping cache in `virtio::hal`).
    virtio_lock: UnsafeCell<KSPIN_LOCK>,
    /// The virtio-gpu transport, brought up in `DxgkDdiStartDevice` (Phase 2).
    /// Guarded by `virtio_lock`; `None` until StartDevice (and after StopDevice).
    virtio: UnsafeCell<Option<Box<VirtioGpu>>>,
    /// PASSIVE-level serialization for scanout selection versus allocation
    /// destruction. A Windows primary can be replaced while an asynchronous
    /// SET_SCANOUT_BLOB/RESOURCE_FLUSH is outstanding; destruction must first
    /// retire that exact resource from scanout 0 and drain the control queue
    /// before RESOURCE_UNREF. This event is an in-place synchronization mutex,
    /// separate from the DISPATCH-safe virtio spinlock because the protected
    /// operations may perform synchronous host round-trips.
    scanout_mutex: UnsafeCell<KEVENT>,
    /// Live host-visible blob → user-VA mappings (Gate 5a Stage 2b). Tagged by the
    /// owning D3D device handle (`DXGKARG_ESCAPE.hDevice`); `DxgkDdiDestroyDevice`
    /// drains and unmaps them. Has its own spinlock, independent of `virtio_lock`,
    /// so teardown works even after the transport is gone.
    pub mappings: crate::mapping::MappingTable,
    /// D4a scanout-read acquire (FIX-DESIGN-d4a.md §3): the per-resid READ
    /// LEDGER page + retirement-event table. Adapter-owned, NOT transport-owned
    /// — the page must survive transport swaps because user mappings of it
    /// (tracked in `mappings`) can outlive StopDevice. The page itself is a
    /// separate heap allocation (first StartDevice), never inline here: the
    /// context is built by value inside `create` and the boot chain's stack
    /// budget has no room for a 4 KiB array (the T3 lesson).
    pub(crate) read_ledger: ReadLedger,
    /// Exact paging-process system-memory leaf PTEs supplied by VidMm for
    /// virtual content transfers. This is the software VA-walk state used by
    /// `DxgkDdiBuildPagingBuffer`, independent of the decorative hardware page
    /// tables that venus never reads.
    pub paging_pte_shadow: crate::ddi::PagingPteShadow,
    /// Exact system-memory pages Windows associates with a BAR allocation
    /// through paging TRANSFER requests.
    pub(crate) system_backings: SystemBackingTable,
    /// The persistent venus client (ring/reply BAR mappings + Vulkan ids) kept
    /// alive for the device lifetime so the page-table blob stays mapped. `None`
    /// until/unless the StartDevice venus bring-up succeeds. Its `Drop` unmaps the
    /// ring/reply kernel mappings; cleared (dropped) in StopDevice. Guarded by
    /// [`Self::venus_mutex`] — a PASSIVE mutex, NOT the virtio spinlock: client
    /// operations block on host round-trips / ring progress (C3/M3.4) and must
    /// never hold a spinlock across those waits.
    venus_client: UnsafeCell<Option<crate::virtio::venus::VenusClient>>,
    /// PASSIVE mutex serializing `venus_client` access: a SynchronizationEvent
    /// (auto-clearing) that starts signaled. Initialized IN PLACE by
    /// [`Self::init_kernel_events`] after the context reaches its final heap
    /// address (a KEVENT's dispatcher header is self-referential once
    /// initialized — it must never be moved afterwards).
    venus_mutex: UnsafeCell<KEVENT>,
    /// A one-shot Present destination probe is armed inside the venus client and
    /// waiting for the PASSIVE display worker to drain it (R320). Only ever set
    /// when the `PresentProbe` knob is on.
    pub probe_pending: AtomicU32,
    /// VSync heartbeat timer for the display half. A fixed-phase 60 Hz one-shot
    /// prefers a system-allocated `EX_TIMER_HIGH_RESOLUTION` timer and falls
    /// back to this embedded `SynchronizationTimer`/DPC pair only if that
    /// allocation failed. Both sources synthesize
    /// `DXGK_INTERRUPT_CRTC_VSYNC` so dxgkrnl advances the flip queue and issues
    /// `SetVidPnSourceAddress` — the heartbeat a render-only adapter structurally
    /// lacks (the viogpu3d FlipThread analog, `viogpu_vidpn.cpp:1977`). Both are
    /// zeroed here and initialized in place by [`Self::init_kernel_events`]
    /// before publication (a KTIMER/KDPC is self-referential once initialized —
    /// never move it afterwards), then only armed/cancelled by display lifecycle
    /// paths. Only armed when `display_half`.
    pub vsync_timer: UnsafeCell<KTIMER>,
    pub vsync_dpc: UnsafeCell<KDPC>,
    /// `PEX_TIMER` returned by `ExAllocateTimer`, stored as an integer so the
    /// stable adapter context remains `Sync`. Zero means allocation failed and
    /// the embedded `KTIMER` fallback is active for this adapter's lifetime.
    /// The pointer is deleted exactly once from `Drop` at final RemoveDevice,
    /// after `stop_vsync` has cancelled it.
    pub vsync_ex_timer: AtomicUsize,
    /// Interrupt-time deadline (100 ns units) of the one-shot tick currently
    /// armed. Advancing this fixed phase avoids both the old 16 ms/62.5 Hz mode
    /// mismatch and cumulative DPC-latency drift.
    pub vsync_deadline_100ns: AtomicU64,
    /// CRTC_VSYNC delivery gate: default 1 once the display half arms the timer,
    /// toggled by `DxgkDdiControlInterrupt(DXGK_INTERRUPT_CRTC_VSYNC, enable)`.
    /// The DPC only synthesizes an interrupt while this is nonzero.
    pub vsync_enabled: AtomicU32,
    /// Count of CRTC_VSYNC interrupts synthesized this boot (diag `ScVs`).
    pub vsync_count: AtomicU32,
    /// Physical address of the last primary actually programmed for display,
    /// reported in each CRTC_VSYNC packet so dxgkrnl can retire the matching
    /// queued flip (viogpu3d `m_sourceAddress`). Direct scanout publishes only
    /// after SET_SCANOUT_BLOB succeeds; the copy fallback publishes from the
    /// ring-1 GPU-completion DPC. 0 until the first completed source switch.
    pub last_primary_address: AtomicU64,
    /// Exact WDDM allocation handle supplied by `SetVidPnSourceAddress` when
    /// dxgkrnl invokes that DDI from its synchronized MMIO-flip path at DIRQL.
    /// DIRQL may only publish this pointer-sized identity. The periodic VSync
    /// DPC wakes the PASSIVE display worker, which consumes the newest handle
    /// and performs the Venus import/copy plus host scanout programming.
    pub pending_vidpn_allocation: AtomicUsize,
    /// Per-buffer completion boundary, captured when the app PRESENTED that
    /// buffer, for the bind edge to arm its refresh against. `(resource,
    /// boundary)`; a zero resource is a free slot.  Boundary bit 63 selects a
    /// registered present stream, otherwise it is the legacy wire namespace.
    ///
    /// Eight slots (4 -> 8 for D4b): the four D4b snapshot-ring resids and the
    /// desktop's rotation coexist across fullscreen transitions, where four
    /// held only a whole single rotation (fullscreen rotates two scan-out
    /// buffers, the desktop three). Touched ONLY from inside
    /// `with_wddm_notify_lock`, which is the same lock that guards the marker
    /// itself, so the pair is never observed torn.
    ///
    /// It lives here rather than in `VirtioGpu` because that struct is built on
    /// the `DxgkDdiStartDevice` stack: adding 64 bytes to it cost 2448 bytes of
    /// boot-chain frame (17488 -> 19936, over the 17936 ceiling) — see the T3
    /// kernel-stack-overflow lesson. (`AdapterContext` itself is heap — growing
    /// these arrays adds no boot-chain frame.)
    pub frame_watermark_resource: [AtomicU32; 8],
    pub frame_watermark_fence: [AtomicU64; 8],
    /// Independent insertion order for frame-watermark eviction.  Tagged
    /// stream and legacy wire boundaries are deliberately incomparable, so a
    /// numeric boundary value may never select a victim.
    pub frame_watermark_ordinal: [AtomicU64; 8],
    pub frame_watermark_next_ordinal: AtomicU64,
    /// ── Scan-out presentation-epoch ownership (ROADMAP defect 0ab-B) ────────
    ///
    /// The display-consumer half of "when may Windows have this allocation
    /// back?". A Helios scan-out is not continuous: the host reads the bound
    /// DMA-BUF exactly once per `RESOURCE_FLUSH`, so a presented buffer must be
    /// immutable from the moment it is published to the host until that read has
    /// finished. `helios_kmd_logic::scanout_lease` is the executable
    /// specification and holds the whole argument; these three atomics are its
    /// implementation.
    ///
    /// They are atomics rather than one lock-guarded struct because the three
    /// edges sit under three different locks: the mint is on the
    /// `DxgkDdiSubmitCommand` DISPATCH path, the bind and the flush issue are on
    /// the PASSIVE display worker under `scanout_mutex`, and the flush
    /// completion is in the used-ring drain under `virtio_lock` — which must
    /// NOT take `wddm_notify_lock`, because the established order is the reverse
    /// and inverting it is a DIRQL deadlock (`adapter/locks.rs`). Every
    /// transition is monotone, so `fetch_add`/`fetch_max` is exact.
    ///
    /// Minter. Each DMA-buffer flip takes the next value; 0 is reserved for
    /// "this submission is gated on no host read".
    pub scanout_present_epoch: AtomicU64,
    /// The epoch whose buffer the host is bound to right now. Advanced by the
    /// display worker only after the binding has actually been published
    /// (a returned `SET_SCANOUT_BLOB`, or an already-bound re-present).
    pub scanout_bound_epoch: AtomicU64,
    /// Lease-end watermark: every epoch `<= this` has ended its host-reader
    /// lease, whether by a completed read, a supersede, a cancellation or
    /// teardown. Only ever moves forward.
    pub scanout_read_epoch: AtomicU64,
    /// 1 while the CURRENT binding was published with a presentation epoch (the
    /// DMA-buffer flip contract), 0 when it was not (the MMIO/`FlipOnVSyncMmIo`
    /// desktop contract, and every path that released its leases wholesale).
    ///
    /// The ownership gate's third operand, and it is not optional: the MMIO path
    /// mints no presentations, so `scanout_present_epoch` freezes at whatever
    /// the last DMA-flip app left behind. A stale `present > bound` would then
    /// hold forever and drop every desktop refresh — a frozen desktop (defect
    /// 0aa). Every failure direction of this flag is "gate off", i.e. the
    /// pre-22.22.217.0 behaviour. See `helios_kmd_logic::scanout_lease::
    /// surplus_republish`.
    pub scanout_epoch_tracked: AtomicU32,
    /// Set when a lease ends from a context that cannot itself pop the WDDM
    /// pending FIFO (the used-ring drain holds `virtio_lock`). The HPD worker
    /// consumes it and runs one `drain_used_and_complete`.
    pub scanout_retire_wanted: AtomicU32,
    /// ── Wire-order guard for scan-out bind bookkeeping (defect 0ab-C, D1(ii)) ─
    ///
    /// Minted at every `SET_SCANOUT_BLOB` enqueue, INSIDE the same `with_virtio`
    /// as the enqueue itself — so the sequence is totally ordered consistently
    /// with the control queue's FIFO order, which is the order the host applies
    /// the binds in.
    ///
    /// It exists because the two enqueue sites apply their guest-side
    /// bookkeeping at completely different times: the DISPATCH fast bind applies
    /// from the used-ring drain, while the PASSIVE worker applies after its
    /// synchronous round-trip returns. A worker bind that started BEFORE a fast
    /// bind can therefore finish its bookkeeping AFTER it (the common case at
    /// 210 fps, where a flip arms mid-round-trip) and stomp the newer identity
    /// with an older one.
    pub scanout_bind_wire_seq: AtomicU64,
    /// The highest bind sequence whose bookkeeping has been applied. Advanced by
    /// whichever application runs; one whose sequence does not advance it is
    /// STALE and applies nothing (`FpLate`).
    pub scanout_bind_applied_seq: AtomicU64,
    /// The resource the NEWEST ENQUEUED bind names — the identity the host will
    /// hold once the control queue drains, as opposed to
    /// `active_scanout_resource`, which is the identity already APPLIED.
    ///
    /// Written in the same store-pair as the sequence above, at every mint site,
    /// and every mint site holds `virtio_lock`. That lock — not the ordering
    /// annotation — is what makes the value coherent with the wire, so `Relaxed`
    /// is sufficient on both the store and the fast path's read: the read is an
    /// advisory skip decision whose worst outcomes are a redundant (host-
    /// idempotent) bind or one missed acceleration, and the enqueue that follows
    /// re-takes the lock and mints its own sequence.
    ///
    /// 0 = no bind enqueued this transport generation, or the last one was the
    /// scan-out DISABLE (`SET_SCANOUT_BLOB` with resource 0), which is exactly
    /// the state in which nothing may be treated as already bound.
    pub scanout_bind_wire_resource: AtomicU32,
    /// Nonzero while the exact primary supplied by `SetVidPnSourceAddress` is
    /// being programmed for scanout. A CRTC_VSYNC must not report the preceding
    /// primary again during this interval: dxgkrnl treats that notification as
    /// the display engine's authoritative completion state and can retire the
    /// newly queued flip before its PASSIVE host bind/copy finishes.
    /// Packed as `(seq << 32) | active` — see [`gate_pack`]. Widened from a bare
    /// flag by R509 so "raise" and "clear only MY interval" are single atomic
    /// operations rather than two independent stores.
    pub vidpn_programming: AtomicU64,
    /// Active virtio scanout-0 blob selected by the display half. The PASSIVE
    /// display worker flushes it only after a completed primary-to-LINEAR GPU
    /// copy marks scanout dirty.
    pub active_scanout_resource: AtomicU32,
    pub active_scanout_wh: AtomicU64,
    /// The resource the host has actually bound to scanout 0.
    ///
    /// The ONLY rebind that runs is the SYNCHRONOUS `ctrl::set_scanout_blob`
    /// from `display.rs`, under `with_scanout_lifecycle`. The async coalescing
    /// design this field's doc used to describe -- `set_scanout_blob_async`
    /// driven from `queue_active_scanout_refresh_locked`, with an in-flight
    /// gate and companion `active_scanout_layout`/`_format` -- was 45 lines of
    /// code that could never execute, because nothing in the crate ever stored
    /// a non-zero stride or format. T6/R902 deleted it; this word and
    /// `active_scanout_resource`/`_wh` are what remain.
    pub host_bound_scanout_resource: AtomicU32,
    /// Import identity of the optional KMD-owned LINEAR fallback. DWM may query
    /// this through `HELIOS_ESCAPE_QUERY_SCANOUT` when the primary cannot be
    /// bound directly. The resource id is the publish word: writers store every
    /// companion field first, then release it.
    pub primary_scanout_resource: AtomicU32,
    pub primary_scanout_wh: AtomicU64,
    /// Row pitch (high 32) and plane offset (low 32).
    pub primary_scanout_layout: AtomicU64,
    pub primary_scanout_alloc_size: AtomicU64,
    pub primary_scanout_memory_type: AtomicU32,
    pub primary_scanout_dxgi_format: AtomicU32,
    pub primary_scanout_generation: AtomicU32,
    /// Seqlock over the whole `primary_scanout_*` set: odd while a publisher is
    /// mid-update, even when the fields are coherent. The publisher's
    /// store-id-last ordering defends a FIRST publish; this defends a
    /// REPUBLISH, where a reader could otherwise combine the old resource id
    /// with the new geometry (k-capsescape-11). Atomic-field based on purpose —
    /// a classic memcpy seqlock over an `UnsafeCell<T>` is a data race under the
    /// Rust memory model, UB even when the sequence check discards the value.
    pub primary_scanout_seq: AtomicU32,
    /// Adapter-owned production LINEAR target. Unlike the bootstrap standard
    /// primary allocation, this resource is never reclaimed by VidMm while DWM
    /// replaces the primary with its private OPTIMAL render target.
    pub dedicated_scanout_resource: AtomicU32,
    /// Kernel-Venus object identities backing `dedicated_scanout_resource`.
    /// The image is the destination of the KMD copy issued for the exact
    /// allocation selected by `SetVidPnSourceAddress`.
    pub dedicated_scanout_image: AtomicU64,
    pub dedicated_scanout_memory: AtomicU64,
    pub scanout_refresh_count: AtomicU32,
    pub scanout_refresh_fail: AtomicU32,
    /// Dirty/coalescing state for real scanout refresh. A completion-ordered
    /// primary marker sets `scanout_refresh_pending` and wakes the HPD/scanout
    /// worker.
    /// At most one fire-and-forget RESOURCE_FLUSH is outstanding; its used-ring
    /// completion clears `scanout_flush_inflight` and wakes the same worker.
    pub scanout_refresh_pending: AtomicU32,
    /// The venus resource the pending dirty edge belongs to, or 0 for the
    /// identity-free HERF edge ("flush whatever is bound").
    ///
    /// The watermark says WHEN a refresh may fire; this says WHAT it must
    /// flush. Without it the flush read `active_scanout_resource` at fire time,
    /// which is a different frame whenever the deferred+coalesced
    /// `SetVidPnSourceAddress` bind has not caught up — publishing the previous
    /// buffer (stale) or one the flip advanced to but the app had not yet
    /// rendered (black).
    pub pending_refresh_resource: AtomicU32,
    /// Refreshes deferred because the bind had not reached the armed resource.
    /// Expected to be small and non-growing in steady state; a rising rate means
    /// the bind path is lagging the present path, not that frames are lost (the
    /// dirty bit is re-armed and the bind completion wakes the worker).
    pub scanout_refresh_unbound: AtomicU32,
    pub scanout_flush_inflight: AtomicU32,
    /// 1 while the fixed-phase one-shot is armed (quiesce/StopDevice clear it).
    pub vsync_armed: AtomicU32,
    /// HPD worker event. `DxgkCbIndicateChildStatus` — which tells the OS the child
    /// video-output is *connected*, the transition that makes the target available
    /// for a VidPn path — is PASSIVE-only and MUST NOT be called during StartDevice,
    /// so a dedicated system thread ([`crate::ddi::hpd::hpd_thread_routine`], the
    /// viogpu3d ThreadWorkRoutine analog) does it. This SynchronizationEvent wakes
    /// that thread: once shortly after start, on virtio config changes, after a
    /// completion-ordered scanout marker, and after async RESOURCE_FLUSH completion.
    pub hpd_event: UnsafeCell<KEVENT>,
    /// Set by the HPD worker immediately before `PsTerminateSystemThread`, at
    /// BOTH of its exit sites. `stop_hpd` waits on this rather than depending on
    /// `ObReferenceObjectByHandle` succeeding: a NotificationEvent stays
    /// signalled, so the join cannot miss it and cannot be defeated by a failed
    /// handle-to-object lookup.
    pub hpd_exited: UnsafeCell<KEVENT>,
    /// PsCreateSystemThread handle for the HPD worker (0 = not started). StopDevice
    /// signals `hpd_stop` + `hpd_event`, joins the thread on this handle, then closes it.
    hpd_thread: AtomicUsize,
    /// Tells the HPD worker to terminate (StopDevice / teardown).
    pub hpd_stop: AtomicU32,
    /// Set when `stop_hpd` could NOT prove the worker exited (the thread-object
    /// reference failed, or the bounded join timed out). RemoveDevice must then
    /// leak this context rather than free memory a live worker still touches.
    pub hpd_worker_leaked: AtomicU32,
    /// Set by the ISR when the virtio config-change bit (ISR status bit 1) fires;
    /// the DPC signals `hpd_event`, then the PASSIVE worker consumes this bit and
    /// re-indicates connection.
    pub config_change_pending: AtomicU32,
    /// 1 once `DxgkDdiStartDevice` has returned.
    ///
    /// `DxgkCbIndicateChildStatus` is forbidden DURING StartDevice, and the HPD
    /// worker is a thread StartDevice itself spawned — so "StartDevice has
    /// returned" cannot be a compile-time fact. It used to be approximated by a
    /// 500 ms relative wait, i.e. a delay standing in for an event, with nothing
    /// actually observing the return. This is the real edge: StartDevice sets it
    /// and signals `hpd_event`, which demotes the timeout to a documented
    /// fallback rather than the mechanism.
    pub start_complete: AtomicU32,
}

// SAFETY (rewritten by R510 to name what is actually here now):
//
//   * `started` is an `UnsafeCell<Option<StartedState>>` written exactly once, by
//     StartDevice, and published with a Release store to `started_published`.
//     Every reader goes through `started()`, whose Acquire load pairs with it, so
//     no reader can observe a partially-built state. There is no `&mut` path to
//     it after publication — StartDevice no longer forms `&mut AdapterContext` at
//     all, which is what makes the ISR, both DPCs and the HPD worker building
//     `&AdapterContext` from the same pointer sound rather than a Stacked-Borrows
//     violation.
//   * `StartedState::transport` is interior-mutable and written only by
//     StartDevice and StopDevice, which Dxgkrnl serializes against each other and
//     against every DDI that reads it.
//   * `virtio` is interior-mutable but every access goes through `virtio_lock`
//     (a kernel spinlock) via `with_virtio`/`set_virtio`, so concurrent
//     escape/DPC callers never alias it.
//   * `venus_client` is guarded by the PASSIVE `venus_mutex`.
//
// This is the genuine lock-guarded state that replaces Phase-2's
// hand-asserted-without-a-lock Send/Sync.
unsafe impl Send for AdapterContext {}
unsafe impl Sync for AdapterContext {}

/// Identity of ONE programming interval.
///
/// The generation half of the packed `vidpn_programming` word. Nothing used to
/// identify which interval a completion belonged to: because the DIRQL half
/// takes no lock, a second `SetVidPnSourceAddress` can raise the gate for
/// interval N+1 while copy N is still outstanding, and copy N's completion then
/// cleared the gate belonging to N+1 — after which the next CRTC_VSYNC reports
/// addr(N) although N+1's programming has not run. Stated precisely: addr(N) is
/// truthful for what the host is scanning out, so dxgkrnl retires N rather than
/// the wrong flip; what breaks is the gate's no-stale-report contract, and one
/// flip's completion is signalled early relative to the newer queued flip.
/// The field is private and there is no public constructor: a ticket can only
/// come from [`AdapterContext::raise_programming_gate`] or be copied from one
/// that did, so a clear cannot be performed against a made-up generation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProgrammingTicket(u32);

// The pack/unpack rules live in `helios_kmd_logic` — pure functions of their
// arguments, so they carry host unit tests for the transitions this gate's
// correctness depends on.
pub(crate) use helios_kmd_logic::{gate_active, gate_pack, gate_seq};

/// Bound on the DIRQL raise's CAS loop. A CAS loop is legal at DIRQL — bounded,
/// no allocation, no callbacks — but only if it is genuinely bounded.
const GATE_RAISE_CAS_ATTEMPTS: u32 = 8;

/// A completion or drop tried to clear a gate that is no longer its own
/// interval's (diag `ScStale`). Must read 0 on a normal boot: it proves the
/// DIRQL/PASSIVE interleave does not occur today.
pub(crate) static GATE_STALE_CLEARS: AtomicU32 = AtomicU32::new(0);
/// The DIRQL raise exhausted its bounded CAS budget and published
/// unconditionally (diag `ScGateCx`).
pub(crate) static GATE_RAISE_CAS_GIVEUPS: AtomicU32 = AtomicU32::new(0);

/// Ownership of one raised `vidpn_programming` interval.
///
/// The gate is raised at exactly one place — the DIRQL half of
/// `SetVidPnSourceAddress` — and used to be lowered at nine hand-written
/// `store(0)` sites inside one 196-line function, plus one asynchronous site in
/// the used-ring DPC. Every future early return in that function can leave a
/// programming interval stranded, retaining an allocation/producer handoff that
/// no worker will complete. The VSync DPC continues reporting the last actually
/// displayed primary while that occurs; it must not turn a PASSIVE producer
/// delay into missing CRTC_VSYNC notifications.
///
/// The token is *adopted*, not constructed at the raise site: the raise happens
/// in the DIRQL DDI and the lower happens in the PASSIVE worker's call stack, so
/// a token that spanned the deferral would just be a flag again. Inside the
/// PASSIVE continuation the nine exits collapse to one compiler-inserted drop.
///
/// What it canNOT express: the DIRQL-set/PASSIVE-clear split itself, and the
/// DestroyAllocation cancel path — that stays an explicit, counter-backed
/// hand-off in `retire_scanout_allocation_locked` (`VpCncl`).
#[must_use]
pub(crate) struct ProgrammingInterval<'a> {
    gate: &'a AtomicU64,
    /// The generation this interval owns. Its drop clears ONLY this one.
    ticket: ProgrammingTicket,
    /// Makes the interval `!Send`: it is lowered on the thread that adopted it.
    _not_send: PhantomData<*const ()>,
}

impl<'a> ProgrammingInterval<'a> {
    /// Take ownership of the already-raised gate for the duration of this scope,
    /// capturing the generation it currently carries.
    ///
    /// Honest residual: the DIRQL half takes no lock, so a raise for a NEWER
    /// primary can land between the worker's `pending` swap and this adopt. The
    /// interval then holds the newer ticket while programming the older handle,
    /// and its drop clears the newer generation. That window is inherent to the
    /// DIRQL/PASSIVE split and is NOT what this ticket closes — what it closes is
    /// the COMPLETION side, where a copy's DPC used to clear whatever gate it
    /// found. The DPC now carries the ticket captured here, by value, so a stale
    /// completion fails its CAS and counts instead of clobbering.
    pub(crate) fn adopt(gate: &'a AtomicU64) -> Self {
        let ticket = ProgrammingTicket(gate_seq(gate.load(Ordering::Acquire)));
        Self {
            gate,
            ticket,
            _not_send: PhantomData,
        }
    }

    /// The generation this interval owns, for handing to the completion DPC.
    pub(crate) fn ticket(&self) -> ProgrammingTicket {
        self.ticket
    }

    /// Hand the interval to the ring-1 GPU-completion DPC, which clears the gate
    /// when the scan-out copy retires (`gpu.rs`, through the notify's raw
    /// `NonNull<AtomicU32>`).
    ///
    /// This is one of the two legitimate ways for the gate to outlive this
    /// scope, and making it a named call means the hand-off is greppable instead
    /// of being an absence. The compiler cannot prove the DPC ever runs, so a
    /// lost completion still leaves the gate raised; that residual is what
    /// R509's generation tag turns into a detectable mismatch.
    pub(crate) fn transfer_to_completion(self) {
        core::mem::forget(self);
    }

    /// Keep the gate raised because this exact primary will be programmed again.
    ///
    /// The other disposition, distinctly named so the two can never be confused
    /// at a call site: nothing was queued and no DPC will clear this gate — the
    /// caller has re-armed `pending_vidpn_allocation` and the VSync DPC's
    /// `pending != 0` branch will signal the worker to retry. Only legal inside
    /// a BOUNDED retry budget; exhausting it must drop the interval instead, or
    /// the display stops.
    pub(crate) fn retain_for_retry(self) {
        core::mem::forget(self);
    }
}

/// A primary the host has actually accepted for scan-out.
///
/// Constructible only from [`Self::after_scanout_bind`], which the programming
/// path reaches only after `SET_SCANOUT_BLOB` has succeeded for that exact
/// source, and it is the ONLY argument
/// [`AdapterContext::publish_displayed_primary`] takes. So "a failed programming
/// publishes no address" is a property of the signature: the failure type
/// (`ScanoutReject`) cannot produce one of these.
///
/// Honest limit: `last_primary_address` stays crate-visible because `ctrl.rs`
/// takes a `NonNull` to it for the completion DPC, so the field can still be
/// stored to directly from inside the crate. The guarantee covers the KMD-side
/// publication; R509 gives the DPC side its own ticket check.
pub(crate) struct ProgrammedPrimary {
    address: u64,
}

impl ProgrammedPrimary {
    /// Only call this once the host has accepted the bind for this exact source.
    pub(crate) fn after_scanout_bind(address: u64) -> Self {
        Self { address }
    }
}

impl Drop for ProgrammingInterval<'_> {
    fn drop(&mut self) {
        // Release, and last: every arm records its diag breadcrumb before this
        // runs, and the same-resource success arm stores `last_primary_address`
        // first. Drop-at-end-of-scope preserves both orders.
        //
        // Ticketed: clears MY generation or nothing. A newer DIRQL raise means
        // the gate is no longer mine to lower, and lowering it would let a
        // CRTC_VSYNC report a primary whose programming has not run.
        clear_programming_gate(self.gate, self.ticket);
    }
}

/// Clear the gate iff it still carries `ticket`'s generation and is active.
///
/// Returns true if this call did the clearing. A mismatch increments `ScStale`
/// rather than silently clobbering — the ticket is a value, so a stale ticket is
/// *detectable* rather than impossible, and that is the honest limit of the
/// encoding.
///
/// Safe at any IRQL: one `compare_exchange` on an `AtomicU64` (lock-free on x64),
/// no allocation, no callbacks.
pub(crate) fn clear_programming_gate(gate: &AtomicU64, ticket: ProgrammingTicket) -> bool {
    let cleared = gate
        .compare_exchange(
            gate_pack(ticket.0, true),
            gate_pack(ticket.0, false),
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok();
    if !cleared {
        GATE_STALE_CLEARS.fetch_add(1, Ordering::Relaxed);
    }
    cleared
}

impl AdapterContext {
    /// Allocate a fully initialised adapter context and return only a pointer to
    /// it.
    ///
    /// This is the ONLY way to obtain an `AdapterContext`, and it deliberately
    /// never returns `Self`. `new` builds five self-referential kernel
    /// dispatcher objects as zeroed placeholders (`scanout_mutex`,
    /// `venus_mutex`, `vsync_timer`, `vsync_dpc`, `hpd_event`), so the real
    /// headers can only be written once the context is at its final heap
    /// address. As two public steps, this compiled:
    ///
    ///   let ctx = AdapterContext::new()?;
    ///   let boxed = Box::new(ctx);
    ///   /* no init_kernel_events */
    ///   *out = Box::into_raw(boxed);
    ///
    /// and produced an adapter whose first `with_venus_client` would
    /// `KeWaitForSingleObject` on an uninitialised KEVENT — typically an
    /// unrecoverable hang, with no diagnostic. Equally,
    /// `let moved = *Box::from_raw(raw);` after init moved initialised,
    /// self-referential dispatcher objects.
    ///
    /// Folding the two phases into one private constructor makes the skip
    /// unrepresentable for safe callers, and returning only `NonNull` — never
    /// `Self` — is what makes the no-move invariant hold: a caller who never
    /// obtains the value cannot move it. `PhantomPinned` is deliberately NOT
    /// used as the guarantee; `!Unpin` affects only the `Pin` APIs and would not
    /// stop `Box::new(ctx)` or `*Box::from_raw(raw)` from compiling.
    ///
    /// Infallible on purpose. `new` was `Result` but had no fallible operation,
    /// so its `Err` arm was dead code; `Box::new` is not fallible today, and an
    /// `Option` here would imply an allocation-failure path that does not exist.
    /// If one is wanted later that is a `Box::try_new` change, not a signature
    /// change now.
    ///
    /// Takes no PDO. AddAdapter is handed one, and this context used to store
    /// it in a `pub pdo` field that NOTHING ever read -- every path to the OS
    /// goes through the `DXGKRNL_INTERFACE` callback table saved at
    /// StartDevice, not through the device object. T6/R917.
    pub(crate) fn create() -> NonNull<AdapterContext> {
        let raw = Box::into_raw(Box::new(Self::new()));
        // Kernel dispatcher objects must be initialized at the context's FINAL
        // address — a KEVENT's header is self-referential.
        // SAFETY: `raw` is the final heap address, freshly allocated, and no
        // other thread can see it yet.
        unsafe { (*raw).init_kernel_events() };
        // SAFETY: `Box::into_raw` never returns null.
        unsafe { NonNull::new_unchecked(raw) }
    }

    /// Private: an `AdapterContext` by value is only ever a transient inside
    /// [`Self::create`], before the in-place dispatcher init runs.
    fn new() -> Self {
        Self {
            // PASSIVE_LEVEL: `create` is called from AddAdapter. Read here so the
            // AddAdapter-time caps/segment queries — which run BEFORE
            // StartDevice — answer from the registry rather than from
            // `DEFAULTS`. Written once, before this context is published, so it
            // needs no interior mutability and no synchronisation.
            initial_knobs: AdapterKnobs::read(),
            reported_segment_count: AtomicU32::new(
                crate::ddi::segment_table::SegmentTable::MAX as u32,
            ),
            started: UnsafeCell::new(None),
            started_published: AtomicU32::new(0),
            last_completed_fence: AtomicU32::new(0),
            wddm_notify_lock: UnsafeCell::new(0),
            isr_status: AtomicUsize::new(0),
            virtio_lock: UnsafeCell::new(0),
            virtio: UnsafeCell::new(None),
            // Zeroed placeholder — initialized in place by init_kernel_events.
            scanout_mutex: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            mappings: crate::mapping::MappingTable::new(),
            read_ledger: ReadLedger::new(),
            paging_pte_shadow: crate::ddi::PagingPteShadow::new(),
            system_backings: SystemBackingTable::new(),
            venus_client: UnsafeCell::new(None),
            // Zeroed placeholder — the real dispatcher header is written by
            // `init_kernel_events` once the context is at its final address.
            venus_mutex: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            probe_pending: AtomicU32::new(0),
            // Zeroed placeholders — initialized by `init_kernel_events` once
            // the context reaches its final address, before publication.
            vsync_timer: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            vsync_dpc: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            vsync_ex_timer: AtomicUsize::new(0),
            vsync_deadline_100ns: AtomicU64::new(0),
            vsync_enabled: AtomicU32::new(0),
            vsync_count: AtomicU32::new(0),
            last_primary_address: AtomicU64::new(0),
            active_scanout_resource: AtomicU32::new(0),
            active_scanout_wh: AtomicU64::new(0),
            host_bound_scanout_resource: AtomicU32::new(0),
            primary_scanout_resource: AtomicU32::new(0),
            primary_scanout_wh: AtomicU64::new(0),
            primary_scanout_layout: AtomicU64::new(0),
            primary_scanout_alloc_size: AtomicU64::new(0),
            primary_scanout_memory_type: AtomicU32::new(0),
            primary_scanout_dxgi_format: AtomicU32::new(0),
            primary_scanout_generation: AtomicU32::new(0),
            primary_scanout_seq: AtomicU32::new(0),
            dedicated_scanout_resource: AtomicU32::new(0),
            dedicated_scanout_image: AtomicU64::new(0),
            dedicated_scanout_memory: AtomicU64::new(0),
            scanout_refresh_count: AtomicU32::new(0),
            scanout_refresh_fail: AtomicU32::new(0),
            scanout_refresh_pending: AtomicU32::new(0),
            pending_refresh_resource: AtomicU32::new(0),
            scanout_refresh_unbound: AtomicU32::new(0),
            scanout_flush_inflight: AtomicU32::new(0),
            vsync_armed: AtomicU32::new(0),
            pending_vidpn_allocation: AtomicUsize::new(0),
            frame_watermark_resource: [
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
            ],
            frame_watermark_fence: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            frame_watermark_ordinal: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            frame_watermark_next_ordinal: AtomicU64::new(0),
            scanout_present_epoch: AtomicU64::new(helios_kmd_logic::scanout_lease::NO_LEASE),
            scanout_bound_epoch: AtomicU64::new(helios_kmd_logic::scanout_lease::NO_LEASE),
            scanout_read_epoch: AtomicU64::new(helios_kmd_logic::scanout_lease::NO_LEASE),
            scanout_epoch_tracked: AtomicU32::new(0),
            scanout_retire_wanted: AtomicU32::new(0),
            scanout_bind_wire_seq: AtomicU64::new(0),
            scanout_bind_applied_seq: AtomicU64::new(0),
            scanout_bind_wire_resource: AtomicU32::new(0),
            vidpn_programming: AtomicU64::new(0),
            // Zeroed placeholder — the real KEVENT is written by init_kernel_events.
            hpd_event: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            hpd_exited: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            hpd_thread: AtomicUsize::new(0),
            hpd_stop: AtomicU32::new(0),
            hpd_worker_leaked: AtomicU32::new(0),
            config_change_pending: AtomicU32::new(0),
            start_complete: AtomicU32::new(0),
        }
    }

    /// Publish "StartDevice has returned" and wake the HPD worker.
    ///
    /// The last thing `DxgkDdiStartDevice` does. Before this, the worker's
    /// prologue simply waited 500 ms and hoped; now the wait has a real wake
    /// source and the timeout is only a fallback.
    pub(crate) fn signal_start_complete(&self) {
        self.start_complete.store(1, Ordering::Release);
        // SAFETY: hpd_event was initialized in place by init_kernel_events;
        // KeSetEvent(Wait=FALSE) is legal through DISPATCH_LEVEL.
        unsafe { KeSetEvent(self.hpd_event.get(), 0, 0) };
    }

    /// Drop every piece of display publication state that is only meaningful
    /// for the transport generation that produced it. PASSIVE_LEVEL.
    ///
    /// StopDevice tears the transport down but the `AdapterContext` itself
    /// survives (it is freed only in RemoveDevice), so without this the whole
    /// publication state machine carries into the next StartDevice. Two ways
    /// that wedges the display:
    ///
    /// 1. A gate raised at DIRQL immediately before the stop is never cleared,
    ///    because `stop_hpd` makes the worker exit before it runs its deferred
    ///    continuation. The next StartDevice must not inherit that stale
    ///    programming ownership into a new transport generation.
    /// 2. Every surviving resource id is meaningless in the new generation,
    ///    whose ids restart at 1 and whose liveness test is bare membership. A
    ///    recycled id can then be accepted as the cached LINEAR scan-out target
    ///    and the desktop copied into an unrelated blob, while the copy is
    ///    submitted against a Venus image from a destroyed context.
    ///
    /// Note `pnputil /restart-device` does NOT reproduce either sequence: it
    /// re-runs AddDevice, which allocates a fresh zeroed context. The carry-over
    /// path is a PnP stop/start on the same context.
    ///
    /// This is a hand-written list and its failure mode is a future field nobody
    /// adds to it. The durable encoding is the transport-owned
    /// `Option<ScanoutBinding>` (T3), after which dropping the transport
    /// structurally drops every identity derived from it and this collapses to
    /// one slot store. Keep it in ONE function so T3 has a single site to replace.
    pub fn reset_display_publication_state(&self) {
        use core::sync::atomic::Ordering;

        // Capture before zeroing: a nonzero pre-reset gate is the evidence that
        // sequence 1 above was live, and the pre-reset resource id identifies
        // the generation being abandoned.
        // Report the ACTIVE FLAG, not the packed word, so `StRst` keeps the
        // exact 0/1 value it has always had (R509 widened the field).
        let was_programming = gate_active(self.vidpn_programming.load(Ordering::Acquire)) as u32;
        let was_resource = self.active_scanout_resource.load(Ordering::Acquire);

        self.vidpn_programming.store(0, Ordering::Release);
        self.pending_vidpn_allocation.store(0, Ordering::Release);
        for slot in &self.frame_watermark_resource {
            slot.store(0, Ordering::Release);
        }
        for slot in &self.frame_watermark_ordinal {
            slot.store(0, Ordering::Release);
        }
        self.frame_watermark_next_ordinal
            .store(0, Ordering::Release);
        self.active_scanout_resource.store(0, Ordering::Release);
        self.active_scanout_wh.store(0, Ordering::Release);
        self.host_bound_scanout_resource.store(0, Ordering::Release);
        self.last_primary_address.store(0, Ordering::Release);
        self.dedicated_scanout_resource.store(0, Ordering::Release);
        self.dedicated_scanout_image.store(0, Ordering::Release);
        self.dedicated_scanout_memory.store(0, Ordering::Release);
        // Third mutator of the descriptor: same odd/even discipline.
        self.primary_scanout_seq.fetch_add(1, Ordering::Release);
        self.primary_scanout_resource.store(0, Ordering::Release);
        self.primary_scanout_wh.store(0, Ordering::Release);
        self.primary_scanout_layout.store(0, Ordering::Release);
        self.primary_scanout_alloc_size.store(0, Ordering::Release);
        self.primary_scanout_memory_type.store(0, Ordering::Release);
        self.primary_scanout_dxgi_format.store(0, Ordering::Release);
        self.primary_scanout_seq.fetch_add(1, Ordering::Release);
        self.scanout_refresh_pending.store(0, Ordering::Release);
        self.scanout_flush_inflight.store(0, Ordering::Release);
        // Every presentation this generation published dies with the binding:
        // release the leases so nothing carried over gates a fresh epoch, and
        // reset the minter with them (the counters are what record how many
        // there were — see `Ls*`). Order matters: end the leases while the
        // minter still names them, THEN zero the sequence.
        self.end_scanout_leases_through(
            self.scanout_present_epoch.load(Ordering::Acquire),
            crate::ddi::scanout_trace::LeaseEnd::Teardown,
        );
        self.scanout_present_epoch
            .store(helios_kmd_logic::scanout_lease::NO_LEASE, Ordering::Release);
        self.scanout_bound_epoch
            .store(helios_kmd_logic::scanout_lease::NO_LEASE, Ordering::Release);
        self.scanout_read_epoch
            .store(helios_kmd_logic::scanout_lease::NO_LEASE, Ordering::Release);
        // Nothing is bound, so no binding is epoch-tracked: the ownership gate
        // stays off until a DMA-flip presentation publishes one again.
        self.scanout_epoch_tracked.store(0, Ordering::Release);
        self.scanout_retire_wanted.store(0, Ordering::Release);
        // The bind sequence is meaningful only within one transport generation:
        // it orders enqueues on a control queue that is about to be destroyed.
        // Both halves are zeroed together, so the next generation's first bind
        // (sequence 1) adopts rather than reading as stale behind an inherited
        // watermark.
        self.scanout_bind_wire_seq.store(0, Ordering::Release);
        self.scanout_bind_applied_seq.store(0, Ordering::Release);
        // With it, the resource the last generation's newest enqueue named: a
        // resource id from a dead transport means nothing, and leaving it set
        // would make the next generation's first flip of that recycled id look
        // already bound.
        self.scanout_bind_wire_resource.store(0, Ordering::Release);
        // Consumers that cache a primary identity compare generations, so bump
        // it rather than zeroing it: a wrapped-to-equal generation would let a
        // stale cache look current.
        self.primary_scanout_generation
            .fetch_add(1, Ordering::AcqRel);
        // D4a: the read ledger's slots name this generation's resource ids and
        // its event registrations signal this generation's retire stream —
        // both die with the binding (signal+deref all, zero the slots; the
        // page itself and the RD counters survive, see `ReadLedger::reset`).
        // A flush token still in flight retires as an orphan (`RdOrp`), which
        // the reclaim rules make inert by construction.
        self.read_ledger.reset();

        crate::diag::record_named_bytes(b"StRst", was_programming);
        crate::diag::record_named_bytes(b"StRstR", was_resource);
    }

    /// The display half's scanout-0 mode `(width, height)`: the host-reported size
    /// if usable, else the 1920×1080 fallback. Every VidPn mode + the generated
    /// EDID derive from this so they stay mutually consistent (cofunctional).
    /// A field read now: the minimum-size check ran ONCE, in
    /// [`ScanoutMode::adopt`], instead of re-running here on every call through
    /// bare literals. Returns the same `(u32, u32)` tuple as before, so its five
    /// consumers are untouched.
    pub fn display_mode(&self) -> (u32, u32) {
        self.started()
            .map_or(DEFAULT_SCANOUT_EXTENT, |s| s.scanout_mode.mode)
            .into()
    }

    /// The packed `(w << 16) | h` the `DspMd` breadcrumb reports.
    pub(crate) fn display_mode_packed(&self) -> u32 {
        self.started()
            .map_or(DEFAULT_SCANOUT_EXTENT, |s| s.scanout_mode.mode)
            .packed()
    }

    /// Raise the programming gate for a NEW interval and return its ticket.
    ///
    /// Runs at DIRQL. A bounded CAS loop is legal there — no allocation, no
    /// callbacks, and `GATE_RAISE_CAS_ATTEMPTS` caps the spin. Incrementing the
    /// generation and setting the active flag in ONE publication is the point:
    /// as two independent stores there was no way for a completion to tell which
    /// interval it belonged to.
    pub(crate) fn raise_programming_gate(&self) -> ProgrammingTicket {
        let mut current = self.vidpn_programming.load(Ordering::Acquire);
        for _ in 0..GATE_RAISE_CAS_ATTEMPTS {
            let seq = gate_seq(current).wrapping_add(1);
            match self.vidpn_programming.compare_exchange_weak(
                current,
                gate_pack(seq, true),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return ProgrammingTicket(seq),
                Err(observed) => current = observed,
            }
        }
        // Budget exhausted. Publish unconditionally — that is exactly what the
        // pre-R509 bare `store(1)` did on every call — and count it, because a
        // contended raise means some other agent is racing the gate.
        let seq = gate_seq(current).wrapping_add(1);
        self.vidpn_programming
            .store(gate_pack(seq, true), Ordering::Release);
        GATE_RAISE_CAS_GIVEUPS.fetch_add(1, Ordering::Relaxed);
        ProgrammingTicket(seq)
    }

    /// Clear whichever interval is currently active, whoever owns it.
    ///
    /// This is the DestroyAllocation cancel path: it is not clearing its OWN
    /// interval, it is abandoning someone else's because the allocation that
    /// interval names is being destroyed. Semantics are exactly the pre-R509
    /// `compare_exchange(1, 0)`: only clear a gate we OBSERVED set, and fail if
    /// it changed under us.
    pub(crate) fn cancel_programming_gate(&self) -> bool {
        let current = self.vidpn_programming.load(Ordering::Acquire);
        if !gate_active(current) {
            return false;
        }
        self.vidpn_programming
            .compare_exchange(
                current,
                gate_pack(gate_seq(current), false),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Mint the notification target for one scan-out copy.
    ///
    /// Deliberately NOT a `#[repr(C)] ScanoutNotifyBlock` embedded in the
    /// adapter: `scanout_refresh_pending`, `last_primary_address`,
    /// `vidpn_programming` and `hpd_event` have readers all over the crate, so
    /// nesting them would turn a transport-API fix into a crate-wide rename.
    /// One construction site buys the same same-adapter guarantee at a fraction
    /// of the blast radius.
    pub(crate) fn scanout_notify(
        &self,
        primary_address: u64,
        ticket: ProgrammingTicket,
    ) -> crate::virtio::ScanoutNotify {
        crate::virtio::ScanoutNotify::for_adapter(self, primary_address, ticket)
    }

    /// Publish the address the CRTC_VSYNC packet reports as the display
    /// engine's authoritative state.
    ///
    /// Takes a [`ProgrammedPrimary`] and nothing else, which is what makes "a
    /// failed programming publishes no address" structural. Before this, every
    /// failure exit left `last_primary_address` naming the PREVIOUSLY displayed
    /// primary, so the heartbeat kept reporting it forever: the flip queued for
    /// the failed primary could never retire, dxgkrnl stopped issuing new source
    /// addresses, and the desktop froze with two overwritten DWORDs as the only
    /// trace — a failure indistinguishable from a hang.
    pub(crate) fn publish_displayed_primary(&self, primary: ProgrammedPrimary) {
        self.last_primary_address
            .store(primary.address, Ordering::Release);
    }

    /// The state StartDevice established, or `None` before it ran.
    ///
    /// The `Acquire` pairs with the `Release` in [`Self::publish_started`], so a
    /// caller that gets `Some` has necessarily observed every field — including
    /// the multi-hundred-byte `DXGKRNL_INTERFACE` the ISR and both DPCs read
    /// lock-free.
    pub(crate) fn started(&self) -> Option<&StartedState> {
        if self.started_published.load(Ordering::Acquire) == 0 {
            return None;
        }
        // SAFETY: the slot is written exactly once, by `publish_started`, before
        // the flag above is set with Release. Observing the flag with Acquire
        // therefore happens-after that write, and nothing ever takes a `&mut` to
        // the slot afterwards — the transport half has its own interior
        // mutability and its own serialization (StartDevice/StopDevice, which
        // dxgkrnl serializes).
        unsafe { (*self.started.get()).as_deref() }
    }

    /// Take the contiguous RAM blocks out of a PREVIOUS start's state so the
    /// next one can carry them forward.
    ///
    /// These blocks are allocated once and freed only in `Drop`; a stop/start
    /// cycle on the same context must reuse them, not leak them and allocate
    /// again. Today's code gets this by leaving the fields untouched across
    /// StopDevice, which publish-once would otherwise lose.
    ///
    /// # Safety
    /// PASSIVE_LEVEL, from `DxgkDdiStartDevice` only, which dxgkrnl serializes.
    pub(crate) unsafe fn take_paging_ram(&self) -> Option<PagingRam> {
        // SAFETY: per the fn contract — StartDevice is serialized against every
        // other lifecycle DDI, and nothing reads `paging_ram` between the take
        // and the republish inside the same call.
        unsafe { (*self.started.get()).as_deref_mut() }.and_then(|s| s.paging_ram.take())
    }

    /// Publish the started state. StartDevice only, exactly once per start.
    ///
    /// # Safety
    /// Must be called at PASSIVE_LEVEL from `DxgkDdiStartDevice`, which dxgkrnl
    /// serializes against every other lifecycle DDI, and only while
    /// [`Self::started`] is `None` for this start.
    pub(crate) unsafe fn publish_started(&self, state: Box<StartedState>) {
        // Takes the Box, so only a pointer moves through this call — the 832-byte
        // value is never copied through a caller's frame.
        // SAFETY: per the fn contract — StartDevice is serialized, and no reader
        // can observe the slot until the Release store below.
        unsafe { *self.started.get() = Some(state) };
        self.started_published.store(1, Ordering::Release);
    }

    /// Borrow the Dxgkrnl interface, or fail if StartDevice has not run yet.
    pub fn dxgkrnl(&self) -> Result<&DXGKRNL_INTERFACE, NotStarted> {
        self.dxgkrnl_opt().ok_or(NotStarted)
    }

    /// The Dxgkrnl callback table, or `None` before StartDevice.
    ///
    /// Replaces the direct `adapter.dxgkrnl.as_ref()` field reads. Every caller
    /// now goes through the published slot, so the ordering that makes the table
    /// safe to read is the Acquire in [`Self::started`] rather than statement
    /// order plus a comment.
    pub fn dxgkrnl_opt(&self) -> Option<&DXGKRNL_INTERFACE> {
        self.started().map(|s| &s.dxgkrnl)
    }

    /// The service-key knob snapshot, or [`AdapterKnobs::DEFAULTS`] before
    /// StartDevice has published one.
    ///
    /// Every knob reader in the driver goes through here, so "a query landing
    /// before StartDevice sees the documented defaults" is one branch in one
    /// place rather than a per-accessor `map_or` whose fallback could drift from
    /// the `read_config_dword` default it is supposed to mirror.
    ///
    /// Returned by value: `AdapterKnobs` is a small `Copy` POD, and handing out a
    /// reference would tie every caller's borrow to the published slot.
    pub(crate) fn knobs(&self) -> AdapterKnobs {
        self.started().map_or(self.initial_knobs, |s| s.knobs)
    }

    /// Latch the `NbSegment` reported on the descriptor-NULL call.
    ///
    /// `Relaxed` is sufficient and correct: both calls of the two-call protocol
    /// arrive on the same dxgkrnl-serialized PASSIVE path, so this is a plain
    /// carry between them, not a synchronisation edge.
    pub(crate) fn latch_reported_segment_count(&self, count: u32) {
        self.reported_segment_count
            .store(count, core::sync::atomic::Ordering::Relaxed);
    }

    /// The count latched by [`Self::latch_reported_segment_count`], i.e. the
    /// number of descriptor slots dxgkrnl allocated. `SegmentTable::MAX` before
    /// any NULL call, so a descriptor pass that somehow arrived first is bounded
    /// by the table's own capacity rather than by zero.
    pub(crate) fn reported_segment_count(&self) -> u32 {
        self.reported_segment_count
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    /// The reported segment table, or `None` before StartDevice has published one.
    ///
    /// `None` is not reachable in practice — a DiagLevel=1 ring shows StartDevice
    /// completing before the first QueryAdapterInfo — but the query path handles
    /// it explicitly rather than assuming, and falls back to the aperture-only
    /// shape it would otherwise have emitted.
    pub(crate) fn segment_table(&self) -> Option<crate::ddi::segment_table::SegmentTable> {
        self.started().map(|s| s.segment_table)
    }

    /// `DisplayHalf`: whether the display DDIs answer for real. False before
    /// StartDevice and on the render-only recovery shape.
    pub fn display_half(&self) -> bool {
        self.knobs().display_half
    }

    /// `AllocCached`. Defaults to TRUE before StartDevice, matching the value the
    /// field was constructed with.
    pub fn alloc_cached(&self) -> bool {
        self.knobs().alloc_cached
    }

    /// `PresentProbe`. Defaults to false before StartDevice.
    pub fn present_probe(&self) -> bool {
        self.knobs().present_probe
    }

    /// The EDID served by `DxgkDdiQueryDeviceDescriptor`.
    ///
    /// Generated from — and therefore always consistent with — the extent
    /// `display_mode()` reports: they are one value.
    pub fn edid(&self) -> Option<&[u8; 128]> {
        self.started().map(|s| s.scanout_mode.edid())
    }

    /// The current transport generation's state, or `None` between StopDevice
    /// and the next StartDevice.
    pub(crate) fn transport_generation(&self) -> Option<&TransportGeneration> {
        // SAFETY: the cell is written only by StartDevice and StopDevice, which
        // dxgkrnl serializes against each other and against every DDI that could
        // read it. The reference borrows `self`, so it cannot outlive the
        // adapter.
        self.started()
            .and_then(|s| unsafe { (*s.transport.get()).as_ref() })
    }

    /// Install the transport generation. StartDevice only.
    ///
    /// # Safety
    /// PASSIVE_LEVEL, from `DxgkDdiStartDevice`, which dxgkrnl serializes.
    pub(crate) unsafe fn set_transport_generation(&self, generation: Option<TransportGeneration>) {
        let Some(state) = self.started() else {
            return;
        };
        // SAFETY: per the fn contract.
        unsafe { *state.transport.get() = generation };
    }

    /// The venus 3D context id for this transport generation, or 0.
    pub fn venus_ctx_id(&self) -> u32 {
        self.transport_generation().map_or(0, |t| t.venus_ctx_id)
    }

    /// The BAR memory segment for this transport generation, if any.
    pub(crate) fn bar_segment(&self) -> Option<&BarSegment> {
        self.transport_generation()
            .and_then(|t| t.bar_segment.as_ref())
    }

    /// Lock-free observation for query/diagnostic paths. Mutation is exposed
    /// only through [`WddmNotifyGuard`], so advancing the scheduler watermark
    /// statically requires ownership of the notification-lock proof.
    pub(crate) fn completed_fence(&self) -> u32 {
        self.last_completed_fence.load(Ordering::Acquire)
    }

    /// Install (or clear) the virtio transport under the lock.
    ///
    /// The previous transport, if any, is dropped *after* the lock is released:
    /// `VirtioGpu::drop` resets the device and frees contiguous memory, both of
    /// which are PASSIVE_LEVEL-only — they must not run at the DISPATCH_LEVEL the
    /// spinlock raises to. MUST be called at PASSIVE_LEVEL (StartDevice /
    /// StopDevice, which Dxgkrnl serializes).
    pub fn set_virtio(&self, new: Option<Box<VirtioGpu>>) {
        // SAFETY: `virtio_lock` is a valid KSPIN_LOCK; the critical section only
        // swaps the Option in/out of the cell (no allocation, no device I/O).
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(self.virtio_lock.get()) };
        let old = core::mem::replace(unsafe { &mut *self.virtio.get() }, new);
        unsafe { KeReleaseSpinLock(self.virtio_lock.get(), irql) };
        // Dropped here, at PASSIVE_LEVEL, outside the lock.
        drop(old);
    }

    /// The real-RAM paging/page-table segment backing, if it was allocated.
    /// `query_segments` reports it as segment 2 (`(phys_base, size)`).
    pub fn paging_ram(&self) -> Option<(u64, u64)> {
        self.started()
            .and_then(|s| s.paging_ram.as_ref())
            .map(|p| (p.phys, p.size))
    }

    /// Install or clear the persistent KMD Venus client, under the PASSIVE
    /// venus mutex (so an in-flight `with_venus_client` cannot be raced by
    /// StopDevice teardown). Device-lifecycle callers only; the previous client
    /// (if any) drops OUTSIDE the mutex, at PASSIVE_LEVEL.
    pub fn set_venus_client(&self, client: Option<crate::virtio::venus::VenusClient>) {
        self.acquire_venus_mutex();
        // SAFETY: the venus mutex gives exclusive access to the cell.
        let old = core::mem::replace(unsafe { &mut *self.venus_client.get() }, client);
        self.release_venus_mutex();
        drop(old);
    }
}

impl Drop for AdapterContext {
    fn drop(&mut self) {
        // Cancel + drain the VSync heartbeat and join the HPD worker before this
        // context's memory (which embeds the KTIMER/KDPC/KEVENT the worker touches)
        // is freed, in case StopDevice was skipped. No-ops if never started.
        // PASSIVE_LEVEL (RemoveDevice).
        self.stop_vsync();
        self.delete_vsync_ex_timer();
        self.stop_hpd();
        // Free the contiguous paging-RAM segment. RemoveDevice (which drops the
        // boxed AdapterContext) runs at PASSIVE_LEVEL, where MmFreeContiguousMemory
        // is legal.
        // The RAM blocks now live in `StartedState`. `&mut self` here is genuinely
        // unique (RemoveDevice owns the box), so reaching into the cell is sound.
        // SAFETY: exclusive access via `&mut self`; RemoveDevice runs after the
        // VSync timer is cancelled and the HPD worker joined, so no other agent
        // holds a reference into this context.
        if let Some(state) = unsafe { (*self.started.get()).as_deref_mut() } {
            if let Some(pr) = state.paging_ram.take() {
                // SAFETY: `va` came from MmAllocateContiguousMemory in
                // `alloc_paging_ram` and is freed exactly once here.
                unsafe { MmFreeContiguousMemory(pr.va.as_ptr() as *mut _) };
            }
        }
        // D4a read-ledger page: allocated once at the first StartDevice, kept
        // across stop/start cycles (user mappings of it may outlive a stop),
        // freed exactly here. Every device was destroyed before RemoveDevice,
        // so no user mapping and no flush token can still reference it. Any
        // event registration left in the table holds an object reference and
        // must be dropped with the adapter (deref only — nothing to signal).
        self.read_ledger.drop_all_event_references();
        let ledger_va = self.read_ledger.take_page_va();
        if ledger_va != 0 {
            // SAFETY: came from MmAllocateContiguousMemory in
            // `ReadLedger::init_page`; freed exactly once, at PASSIVE_LEVEL.
            unsafe { MmFreeContiguousMemory(ledger_va as *mut _) };
        }
    }
}
