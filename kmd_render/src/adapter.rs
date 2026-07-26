//! Adapter context — one per virtio-gpu device the driver binds to.
//!
//! Allocated in `DxgkDdiAddDevice`, populated in `DxgkDdiStartDevice`, freed in
//! `DxgkDdiRemoveDevice`. Dxgkrnl hands this back to us as the opaque
//! `MiniportDeviceContext` in every subsequent DDI call.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use wdk_sys::ntddk::{
    KeAcquireSpinLockRaiseToDpc, KeInitializeEvent, KeReleaseSpinLock, KeSetEvent,
    KeWaitForSingleObject, MmAllocateContiguousMemory, MmFreeContiguousMemory,
    MmGetPhysicalAddress,
};
use wdk_sys::{KDPC, KEVENT, KSPIN_LOCK, KTIMER, PHYSICAL_ADDRESS, PVOID};

use crate::dxgk::*;
use crate::error::DriverError;
use crate::virtio::VirtioGpu;
use helios_kmd_logic::DisplayMode;

extern "C" {
    /// `extern POBJECT_TYPE *PsThreadType;` (ntddk.h) — the thread object type, for
    /// `ObReferenceObjectByHandle` validation when joining the HPD worker. A data
    /// export (ntoskrnl.lib), not in the wdk-sys function bindings, so declared here
    /// (same pattern as `ExEventObjectType` in `ddi/escape.rs`).
    static PsThreadType: *mut wdk_sys::POBJECT_TYPE;
}

/// Size of the real-RAM-backed segment Helios reports for VidMm's page tables and
/// paging buffers. The host-visible venus BAR (segment 1) is a CpuVisible MEMORY
/// segment only where a blob is RESOURCE_MAP_BLOB'd, so VidMm cannot allocate the
/// system context's paging buffer / page tables there (an access to an unbacked
/// BAR offset faults). VidMm needs a genuinely-backed, physically-contiguous,
/// CpuVisible segment for that bookkeeping — this block. Modest: a few page tables
/// + the 64 KiB paging buffer during bring-up; bump if VidMm's allocation within
/// the segment ever fails.
const PAGING_RAM_SIZE: usize = 8 * 1024 * 1024;

/// A real-RAM-backed region reported to VidMm as a CpuVisible memory segment, used
/// for page-table / paging-buffer storage (see [`PAGING_RAM_SIZE`]).
pub struct PagingRam {
    /// Kernel VA (for free); the region is mapped non-paged by the allocator.
    va: NonNull<u8>,
    /// Guest-physical base — the segment's `CpuTranslatedAddress`.
    pub phys: u64,
    /// Region length in bytes (== reported segment Size/CommitLimit).
    pub size: u64,
}

/// The BAR memory segment (segment 3): the head of the host-visible venus
/// window, reserved as dxgkrnl's CPU-host-aperture region. Blobs are mapped
/// into it at dxgkrnl-chosen aperture offsets by `DxgkDdiMapCpuHostAperture`
/// (`cpu_host_aperture.rs`). See the two-memory-split root cause
/// (HANDOFF_GDI_EXECUTOR_2026_07_05.md ★FINAL).
pub struct BarSegment {
    /// Guest-physical base = the host-visible window base (partition offset 0),
    /// or the probe RAM block's base under `BarSegMode` 5.
    pub gpa: u64,
    /// Partition length in bytes (== reported segment Size/CommitLimit == the
    /// declared `DXGK_CPUHOSTAPERTURE` span == the `reserve_window_prefix`
    /// given to the blob-window allocator).
    pub size: u64,
    /// The WDDM segment id this region is reported as (3 in the default
    /// topology; 2 under `BarSegMode` 10/11 where it replaces/precedes the
    /// paging-RAM segment). All BAR-segment consumers key off this field.
    pub seg_id: u32,
    /// The `BarSegMode` topology this segment was configured under (10 = two
    /// segments, BAR replaces RAM as id 2; 11 = three segments, BAR id 2 +
    /// RAM id 3; anything else = default aperture/RAM/BAR = ids 1/2/3).
    pub topo: u32,
    /// AddAdapter-acceptance probe only (`BarSegMode` 5: RAM-backed region):
    /// the segment is reported but NO allocation is ever placed in it
    /// (`create_allocation` keeps everything on the aperture) and
    /// MapCpuHostAperture refuses it — the aperture region is not the venus
    /// window, so blob maps cannot back it.
    pub probe_only: bool,
}

/// Exact system-memory backing Windows supplied for one allocation in a paging
/// TRANSFER from the BAR segment to segment 0.
///
/// The physical pages come directly from the locked MDL in the transfer
/// request. They remain the allocation's authoritative system backing until
/// Windows issues the inverse transfer or destroys the allocation.
#[derive(Clone)]
pub(crate) struct SystemBackingSnapshot {
    pub resource_id: u32,
    pub blob_offset: u64,
    pub size: u64,
    pub first_page_offset: u32,
    pub pages: Arc<[u64]>,
}

/// Per-adapter resource-id -> Windows system-backing association.
///
/// Entries use `Arc<[u64]>` so Present can take an allocation-free snapshot
/// while the spinlock is held. New page arrays are built before the lock is
/// acquired; the entry vector is pre-reserved and never grows while locked.
pub(crate) struct SystemBackingTable {
    lock: UnsafeCell<KSPIN_LOCK>,
    entries: UnsafeCell<Vec<SystemBackingSnapshot>>,
}

unsafe impl Send for SystemBackingTable {}
unsafe impl Sync for SystemBackingTable {}

/// Ticks the scanout pacing snapshot (R318). One rate for the whole block.
static SCANOUT_PACING_TICKS: AtomicU32 = AtomicU32::new(0);

impl SystemBackingTable {
    const MAX_ENTRIES: usize = 128;

    pub fn new() -> Self {
        Self {
            lock: UnsafeCell::new(0),
            entries: UnsafeCell::new(Vec::with_capacity(Self::MAX_ENTRIES)),
        }
    }

    pub fn replace(&self, backing: SystemBackingSnapshot) -> bool {
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(self.lock.get()) };
        let entries = unsafe { &mut *self.entries.get() };
        let mut old = None;
        let success = if let Some(index) = entries
            .iter()
            .position(|entry| entry.resource_id == backing.resource_id)
        {
            old = Some(core::mem::replace(&mut entries[index], backing));
            true
        } else if entries.len() < entries.capacity() {
            entries.push(backing);
            true
        } else {
            false
        };
        unsafe { KeReleaseSpinLock(self.lock.get(), irql) };
        // Releasing the old Arc can free pool memory; do that after dropping
        // back to the caller's original IRQL.
        drop(old);
        success
    }

    pub fn snapshot(&self, resource_id: u32) -> Option<SystemBackingSnapshot> {
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(self.lock.get()) };
        let result = unsafe { &*self.entries.get() }
            .iter()
            .find(|entry| entry.resource_id == resource_id)
            .cloned();
        unsafe { KeReleaseSpinLock(self.lock.get(), irql) };
        result
    }

    pub fn contains(&self, resource_id: u32) -> bool {
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(self.lock.get()) };
        let result = unsafe { &*self.entries.get() }
            .iter()
            .any(|entry| entry.resource_id == resource_id);
        unsafe { KeReleaseSpinLock(self.lock.get(), irql) };
        result
    }

    pub fn remove(&self, resource_id: u32) {
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(self.lock.get()) };
        let entries = unsafe { &mut *self.entries.get() };
        let removed = entries
            .iter()
            .position(|entry| entry.resource_id == resource_id)
            .map(|index| entries.swap_remove(index));
        unsafe { KeReleaseSpinLock(self.lock.get(), irql) };
        drop(removed);
    }
}

/// Everything `DxgkDdiStartDevice` establishes, as one value published once.
///
/// StartDevice used to take a unique `&mut AdapterContext` that stayed live for
/// the whole function and mutated ~a dozen plain fields through it — while the
/// context pointer had been public to dxgkrnl since AddDevice and THREE
/// concurrent agents build `&AdapterContext` from it. `init_vsync` arms a 16 ms
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
    /// `AllocCached` service-key knob (read once in StartDevice; default 1).
    /// When set, CpuVisible allocations are additionally flagged `Cached` so
    /// dxgkrnl maps CPU views write-back instead of write-combined. The BAR
    /// window is RAM-backed host shmem (x86 cache-coherent for all agents on
    /// the same physical pages); WC reads measured ~200 MB/s in the IDD
    /// readback (36 ms per 7.8 MiB frame, 2026-07-06). 0 = kill switch.
    pub alloc_cached: bool,
    /// `PresentProbe` service-key knob (read once in StartDevice; default 0).
    /// When enabled, each exact Present source/destination pair performs one
    /// bounded, fence-ordered CPU sample of the destination after its eighth
    /// submission. This is a diagnostic only: the steady-state path never
    /// waits or maps a frame, and the per-pair `probe_done` state statically
    /// prevents repeated readbacks.
    pub present_probe: bool,
    /// `ScForceReject` service-key knob (read once in StartDevice;
    /// default 0 = OFF, and absent from the registry in the shipped baseline).
    ///
    /// GATE INSTRUMENT. The T3 gate requires each deferred-programming refusal
    /// exit to be forced with its counter proven to move and `VsCnt` still
    /// advancing — and seven of the eight cannot be provoked naturally on a
    /// healthy box. This makes `program_vidpn_source` take one specific exit so
    /// that can be shown per boot-cycle, with `pnputil /restart-device` between
    /// values instead of eight reboots.
    ///
    ///   1 = BadAlloc   2 = Extent   3 = Layout           4 = Format
    ///   5 = LinearAllocFailed       6 = SetFailed
    ///   7 = NoTarget                8 = CopyFailed
    ///
    /// Costs one atomic-free field read per SetVidPnSourceAddress. Recorded as
    /// `ScFrc` at StartDevice. Candidate for deletion in T6 alongside the other
    /// experiment surfaces once the gate evidence is banked.
    pub forced_reject: u32,
    /// `DisplayHalf` service-key knob (REG_DWORD, read once in StartDevice;
    /// default 0 = OFF, the boot-proven render-only surface). When nonzero,
    /// StartDevice advertises ONE video-present source + ONE child video-output
    /// and the VidPn/child DDIs in `ddi::display`/`ddi::vidpn`/`ddi::start_device`
    /// stand up a real virtual VidPn output + default monitor and drive
    /// virtio-gpu scanout, instead of returning NOT_SUPPORTED. Default 0 keeps
    /// the render-only recovery shape available; production sets it to 1 via
    /// `reg add ... /v DisplayHalf /t REG_DWORD /d 1` + a guest reboot
    /// (re-runs StartDevice → child enumeration) with NO reboot once deployed.
    /// Demoted to 0 before publication if the transport never came up.
    /// Value mirrored to the `DspH` fixed diag record at StartDevice.
    pub display_half: bool,
    /// Scanout-0 mode the display half presents, together with the EDID
    /// generated from it. See [`ScanoutMode`].
    pub scanout_mode: ScanoutMode,
    /// Real-RAM-backed segment for VidMm page tables / paging buffers (segment 2).
    /// `None` if the contiguous allocation failed (then we fall back to the old
    /// single-segment shape). Freed in `AdapterContext::drop`.
    pub paging_ram: Option<PagingRam>,
    /// RAM block backing the `BarSegMode` 5 AddAdapter-acceptance probe (the
    /// segment-3 aperture region is then real RAM instead of the BAR window).
    /// Freed in `AdapterContext::drop`.
    pub bar_probe_ram: Option<PagingRam>,
    /// The half StopDevice clears. `None` between StopDevice and the next
    /// StartDevice.
    transport: UnsafeCell<Option<TransportGeneration>>,
}

/// The service-key knobs, as one small POD so they cross the
/// [`StartedState::boxed`] boundary without a wide argument list.
#[derive(Clone, Copy)]
pub(crate) struct StartedKnobs {
    pub alloc_cached: bool,
    pub present_probe: bool,
    pub forced_reject: u32,
    pub display_half: bool,
}

impl StartedState {
    /// Build the sticky half DIRECTLY ON THE HEAP.
    ///
    /// ⚠ STACK BUDGET — this is not a style preference. `DxgkDdiStartDevice`
    /// calls `VirtioGpu::init`, whose own frame is ~9.1 KB, and the x64 kernel
    /// stack is 24 KB total. Building this 832-byte struct (which embeds a
    /// 576-byte `DXGKRNL_INTERFACE`) in StartDevice's frame — and passing it
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
        knobs: StartedKnobs,
        scanout_mode: ScanoutMode,
        paging_ram: Option<PagingRam>,
        bar_probe_ram: Option<PagingRam>,
    ) -> Box<Self> {
        Box::new(Self {
            // SAFETY: per the fn contract; a plain POD copy of dxgkrnl's buffer.
            dxgkrnl: unsafe { *dxgkrnl },
            alloc_cached: knobs.alloc_cached,
            present_probe: knobs.present_probe,
            forced_reject: knobs.forced_reject,
            display_half: knobs.display_half,
            scanout_mode,
            paging_ram,
            bar_probe_ram,
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
    /// venus-backed, BAR-visible, CPU-coherent page-table region self-allocated at
    /// StartDevice (`(gpa, size)`). `None` if the venus allocation was unavailable
    /// or failed (StartDevice stays best-effort). When present and the aperture
    /// shape is enabled, `query_segments` reports this as the VidMm page-table
    /// segment (segment id 2) — real device-BAR memory backed by real host memory,
    /// which VidMm accepts where it drops a system-RAM segment. See `venus.rs`.
    ///
    /// ⚠ WRITE-ONLY as of R510, which is the first time that is visible: the only
    /// consumer would be `query_segments`, and it deliberately reports
    /// `paging_ram` instead, because QuerySegment4 runs BEFORE StartDevice's venus
    /// allocation. Kept and annotated rather than deleted — a deletion is a T6
    /// dead-code commit with its own reachability evidence, not a side effect of
    /// this refactor.
    #[allow(dead_code)]
    pub page_table_window: Option<(u64, u64)>,
    /// BAR memory segment (segment 3) — the head partition of the host-visible
    /// window, reserved as dxgkrnl's CPU-host-aperture region at StartDevice.
    /// `None` if the window is absent/too small; segment 3 is then not
    /// reported and standard allocations stay on the aperture (old behavior).
    pub bar_segment: Option<BarSegment>,
    /// The persistent venus 3D context id (`VIRTIO_GPU_CAPSET_VENUS`) the venus
    /// client rides, created in StartDevice and destroyed in StopDevice. `0` = none.
    pub venus_ctx_id: u32,
}

pub struct AdapterContext {
    /// Physical device object for the virtio-gpu device.
    pub pdo: PDEVICE_OBJECT,
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
    virtio: UnsafeCell<Option<VirtioGpu>>,
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
    /// VSync heartbeat timer for the display half. A `SynchronizationTimer` fired
    /// every ~16 ms (`vsync_dpc`) whose DPC synthesizes `DXGK_INTERRUPT_CRTC_VSYNC`
    /// so dxgkrnl advances the flip queue and issues `SetVidPnSourceAddress` — the
    /// heartbeat a render-only adapter structurally lacks (the viogpu3d FlipThread
    /// analog, `viogpu_vidpn.cpp:1977`). Both are zeroed here and initialized in
    /// place by [`Self::init_vsync`] at StartDevice (a KTIMER/KDPC is
    /// self-referential once initialized — never move it afterwards); the timer is
    /// cancelled at StopDevice. Only armed when `display_half`.
    pub vsync_timer: UnsafeCell<KTIMER>,
    pub vsync_dpc: UnsafeCell<KDPC>,
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
    /// Row pitch (high 32), plane offset (low 32), and virtio format for the
    /// desired scanout. `active_scanout_resource` is the publish word.
    pub active_scanout_layout: AtomicU64,
    pub active_scanout_format: AtomicU32,
    /// Host-accepted binding and one-command async SET_SCANOUT_BLOB gate. A
    /// rotating DWM primary only publishes the newest desired resource; the
    /// worker coalesces intermediate flips without blocking in a ctrl roundtrip.
    pub host_bound_scanout_resource: AtomicU32,
    pub scanout_bind_inflight: AtomicU32,
    pub scanout_bind_fail: AtomicU32,
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
    /// Diagnostic scanout blob selected by `ScanoutDiag >= 2`. This is a
    /// KMD-owned, CPU-filled color-bars blob used only to prove whether QEMU can
    /// display any blob from this miniport after boot.
    pub diag_scanout_resource: AtomicU32,
    pub diag_scanout_wh: AtomicU64,
    /// Diagnostic scanout row pitch (high 32) and byte offset (low 32).
    pub diag_scanout_layout: AtomicU64,
    pub scanout_refresh_count: AtomicU32,
    pub scanout_refresh_fail: AtomicU32,
    /// Dirty/coalescing state for real scanout refresh. A completion-ordered
    /// primary marker sets `scanout_refresh_pending` and wakes the HPD/scanout
    /// worker.
    /// At most one fire-and-forget RESOURCE_FLUSH is outstanding; its used-ring
    /// completion clears `scanout_flush_inflight` and wakes the same worker.
    pub scanout_refresh_pending: AtomicU32,
    pub scanout_flush_inflight: AtomicU32,
    /// 1 once [`Self::init_vsync`] has armed the timer (StopDevice cancels once).
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

/// Proof that this adapter's WDDM notification spinlock is currently held.
///
/// The fields are private and the type is neither `Copy` nor `Clone`, so safe
/// code can obtain a usable reference only inside [`AdapterContext::with_wddm_notify_lock`].
/// Fence-queue mutation and VidSch fence notifications accept this token rather
/// than relying on a caller-side comment or naming convention.
pub(crate) struct WddmNotifyGuard<'a> {
    adapter: &'a AdapterContext,
}

/// Proof that `wddm_notify_lock` was taken BEFORE `virtio_lock`, on the SAME
/// adapter, and is still held.
///
/// Mintable only inside [`WddmNotifyGuard::with_virtio`], which is the only way
/// to reach the six [`VirtioGpu`] methods whose contract depends on the notify
/// lock. That is what the plain `&WddmNotifyGuard` parameter used to claim and
/// did not deliver: a guard carries no relationship to the `&mut VirtioGpu`
/// being mutated, so `adapter_a.with_wddm_notify_lock(|g| adapter_b.with_virtio(
/// |v| v.note_wddm_submission(g, ..)))` compiled and mutated B's fence FIFO
/// under A's lock.
///
/// Honest gap: a caller that already holds `virtio_lock` and calls
/// `guard.with_virtio` recursively acquires it and self-deadlocks. Rust cannot
/// see that, so it stays a comment.
pub(crate) struct NotifyOrdered<'a> {
    _lock: PhantomData<&'a ()>,
}

impl WddmNotifyGuard<'_> {
    pub(crate) fn completed_fence(&self) -> u32 {
        self.adapter.last_completed_fence.load(Ordering::Acquire)
    }

    pub(crate) fn set_completed_fence(&self, fence: u32) {
        self.adapter
            .last_completed_fence
            .store(fence, Ordering::Release);
    }

    /// Run `f` against this adapter's transport with the notify lock already
    /// held, handing it the [`NotifyOrdered`] token the ordered transport
    /// methods require.
    ///
    /// The closure receives no `&AdapterContext`, so it cannot re-enter the
    /// notify lock; and because the same `&self` mints both borrows, the
    /// cross-adapter call above stops compiling.
    ///
    /// ⚠ Do NOT grow this into a `with_notify_then_virtio` that holds both locks
    /// for one closure: `drain_used_and_complete` deliberately makes several
    /// separate `with_virtio` calls inside one notify scope and runs
    /// `request_scanout_refresh()` and `signal_dma_completed_locked()` (which
    /// raises to the device DIRQL via `DxgkCbSynchronizeExecution`) between
    /// them. Folding those into one transport critical section would change
    /// frame-path timing.
    pub(crate) fn with_virtio<R>(
        &self,
        f: impl FnOnce(&NotifyOrdered<'_>, &mut VirtioGpu) -> R,
    ) -> Result<R, DriverError> {
        self.adapter.with_virtio(|v| {
            let order = NotifyOrdered { _lock: PhantomData };
            f(&order, v)
        })
    }
}

/// Proof that this adapter's PASSIVE scanout-lifecycle mutex is currently held.
///
/// The fields are private and the type is neither `Copy` nor `Clone`, so safe
/// code can obtain a reference only inside
/// [`AdapterContext::with_scanout_lifecycle`]. Every helper whose contract is
/// "call only under `scanout_mutex`" takes `&ScanoutGuard<'_>` instead of
/// carrying a `_locked` suffix and a doc comment.
///
/// The lock order this token sits at the head of is
/// `scanout_mutex -> venus_mutex -> virtio_lock`; [`Self::with_venus_client`]
/// is the enforced path for the middle step.
///
/// ⚠ This does NOT make recursion unrepresentable: the guard is handed to the
/// very closure that could call a re-acquiring wrapper. Callers must still not
/// invoke [`AdapterContext::with_scanout_lifecycle`],
/// [`AdapterContext::queue_active_scanout_refresh`] or
/// [`AdapterContext::retire_scanout_allocation`] from inside a guarded closure —
/// `scanout_mutex` is a non-recursive `SynchronizationEvent` and a re-entry is a
/// permanent PASSIVE self-deadlock of the HPD worker with no bugcheck and no
/// counter. `request_scanout_refresh` is deliberately token-free: it only sets a
/// bit and signals an event, so it is legal from inside the lock and from
/// DISPATCH.
pub(crate) struct ScanoutGuard<'a> {
    adapter: &'a AdapterContext,
    /// Makes the guard `!Send`: the guarded work never crosses threads.
    _not_send: PhantomData<*const ()>,
}

impl ScanoutGuard<'_> {
    /// The scanout-lifecycle-ordered path to the Venus client.
    ///
    /// Forwards to [`AdapterContext::with_venus_client`]. Calling it through the
    /// guard is what makes `scanout_mutex`-before-`venus_mutex` a signature
    /// rather than a comment at the two sites that run under the scanout lock.
    /// `AdapterContext::with_venus_client` stays public because eleven of its
    /// thirteen callers legitimately hold no scanout lock.
    pub(crate) fn with_venus_client<R>(
        &self,
        f: impl FnOnce(&mut crate::virtio::venus::VenusClient) -> R,
    ) -> Result<R, DriverError> {
        self.adapter.with_venus_client(f)
    }
}

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
/// the used-ring DPC. Every future early return in that function was a potential
/// permanent display stop: `vsync_dpc_routine` returns early on every 16 ms tick
/// while the gate is set, so no CRTC_VSYNC is delivered and dxgkrnl never
/// retires the queued flip. Two exits already got this wrong (T1a's k-display-01
/// and k-display-03).
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

/// Outcome of trying to queue one coalesced scanout refresh.
pub(crate) enum ScanoutRefreshQueue {
    Queued,
    Busy,
    Failed,
    Unavailable,
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
    ///   let ctx = AdapterContext::new(pdo)?;
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
    pub(crate) fn create(pdo: PDEVICE_OBJECT) -> NonNull<AdapterContext> {
        let raw = Box::into_raw(Box::new(Self::new(pdo)));
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
    fn new(pdo: PDEVICE_OBJECT) -> Self {
        Self {
            pdo,
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
            paging_pte_shadow: crate::ddi::PagingPteShadow::new(),
            system_backings: SystemBackingTable::new(),
            venus_client: UnsafeCell::new(None),
            // Zeroed placeholder — the real dispatcher header is written by
            // `init_kernel_events` once the context is at its final address.
            venus_mutex: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            probe_pending: AtomicU32::new(0),
            // Zeroed placeholders — the real KTIMER/KDPC dispatcher state is
            // written by `init_vsync` once the context is at its final address.
            vsync_timer: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            vsync_dpc: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            vsync_enabled: AtomicU32::new(0),
            vsync_count: AtomicU32::new(0),
            last_primary_address: AtomicU64::new(0),
            active_scanout_resource: AtomicU32::new(0),
            active_scanout_wh: AtomicU64::new(0),
            active_scanout_layout: AtomicU64::new(0),
            active_scanout_format: AtomicU32::new(0),
            host_bound_scanout_resource: AtomicU32::new(0),
            scanout_bind_inflight: AtomicU32::new(0),
            scanout_bind_fail: AtomicU32::new(0),
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
            diag_scanout_resource: AtomicU32::new(0),
            diag_scanout_wh: AtomicU64::new(0),
            diag_scanout_layout: AtomicU64::new(0),
            scanout_refresh_count: AtomicU32::new(0),
            scanout_refresh_fail: AtomicU32::new(0),
            scanout_refresh_pending: AtomicU32::new(0),
            scanout_flush_inflight: AtomicU32::new(0),
            vsync_armed: AtomicU32::new(0),
            pending_vidpn_allocation: AtomicUsize::new(0),
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

    /// Start the HPD worker thread (StartDevice, display half). PASSIVE_LEVEL.
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
    ///    continuation. The next StartDevice arms the timer and every
    ///    `vsync_dpc_routine` tick early-returns on the inherited gate, so
    ///    `VsCnt` never advances again.
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
        self.active_scanout_resource.store(0, Ordering::Release);
        self.active_scanout_wh.store(0, Ordering::Release);
        self.active_scanout_layout.store(0, Ordering::Release);
        self.active_scanout_format.store(0, Ordering::Release);
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
        self.diag_scanout_resource.store(0, Ordering::Release);
        self.diag_scanout_wh.store(0, Ordering::Release);
        self.diag_scanout_layout.store(0, Ordering::Release);
        self.scanout_refresh_pending.store(0, Ordering::Release);
        self.scanout_flush_inflight.store(0, Ordering::Release);
        self.scanout_bind_inflight.store(0, Ordering::Release);
        // Consumers that cache a primary identity compare generations, so bump
        // it rather than zeroing it: a wrapped-to-equal generation would let a
        // stale cache look current.
        self.primary_scanout_generation
            .fetch_add(1, Ordering::AcqRel);

        crate::diag::record_named_bytes(b"StRst", was_programming);
        crate::diag::record_named_bytes(b"StRstR", was_resource);
    }

    /// Idempotent-ish: does nothing if a thread handle is already stored.
    ///
    /// # Safety
    /// `self` must be at its final heap address and `dxgkrnl` already saved.
    pub unsafe fn init_hpd(&self) {
        use core::sync::atomic::Ordering;
        if self.hpd_thread.load(Ordering::Acquire) != 0 {
            return;
        }
        self.hpd_stop.store(0, Ordering::Release);
        self.scanout_refresh_pending.store(0, Ordering::Release);
        self.scanout_flush_inflight.store(0, Ordering::Release);
        self.scanout_bind_inflight.store(0, Ordering::Release);
        self.host_bound_scanout_resource.store(0, Ordering::Release);
        let mut handle: wdk_sys::HANDLE = core::ptr::null_mut();
        const THREAD_ALL_ACCESS: u32 = 0x001F_FFFF;
        // SAFETY: PASSIVE_LEVEL; a kernel system thread in the system process
        // running `hpd_thread_routine` with this stable context as its argument.
        let st = unsafe {
            wdk_sys::ntddk::PsCreateSystemThread(
                &mut handle,
                THREAD_ALL_ACCESS,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                Some(crate::ddi::hpd::hpd_thread_routine),
                self as *const _ as PVOID,
            )
        };
        if st == STATUS_SUCCESS && !handle.is_null() {
            self.hpd_thread.store(handle as usize, Ordering::Release);
        } else {
            crate::diag::record(0x0B00_00E7);
            crate::diag::fault(crate::diag::FaultCounter::StHpd, st as u32);
        }
    }

    /// Wake the HPD worker to re-indicate connection (from the interrupt DPC at
    /// DISPATCH_LEVEL — KeSetEvent with Wait=FALSE is legal there).
    pub fn signal_hpd(&self) {
        // SAFETY: hpd_event was initialized in place by init_kernel_events.
        unsafe { KeSetEvent(self.hpd_event.get(), 0, 0) };
    }

    /// Mark already-completed scanout contents dirty. The normal copied path
    /// does this from the ring-1 GPU-completion DPC; the direct-primary
    /// zero-copy case has no KMD GPU submission, so SetVidPn uses this after
    /// Windows has handed it the completed primary.
    pub fn request_scanout_refresh(&self) {
        self.scanout_refresh_pending.store(1, Ordering::Release);
        // SAFETY: hpd_event is initialized in place and stable for the adapter
        // lifetime; KeSetEvent(Wait=FALSE) is legal through DISPATCH_LEVEL.
        unsafe { KeSetEvent(self.hpd_event.get(), 0, 0) };
    }

    /// Stop + join the HPD worker before teardown (StopDevice / Drop). Idempotent.
    /// PASSIVE_LEVEL — it blocks on the worker's exit.
    pub fn stop_hpd(&self) {
        use core::sync::atomic::Ordering;
        let h = self.hpd_thread.swap(0, Ordering::AcqRel);
        if h == 0 {
            return;
        }
        self.hpd_stop.store(1, Ordering::Release);
        // SAFETY: initialized event; wake the worker so it observes hpd_stop.
        unsafe { KeSetEvent(self.hpd_event.get(), 0, 0) };
        // Join: reference the thread object from its handle, wait for it to exit,
        // deref, then close the handle. Without the join, RemoveDevice could free
        // this context while the worker still runs → UAF.
        const SYNCHRONIZE: u32 = 0x0010_0000;
        const KERNEL_MODE: i8 = 0;
        let mut obj: PVOID = core::ptr::null_mut();
        // SAFETY: `h` is a live thread handle from PsCreateSystemThread; PsThreadType
        // validates it. On success we hold a reference to the ETHREAD.
        let st = unsafe {
            wdk_sys::ntddk::ObReferenceObjectByHandle(
                h as wdk_sys::HANDLE,
                SYNCHRONIZE,
                *PsThreadType,
                KERNEL_MODE,
                &mut obj,
                core::ptr::null_mut(),
            )
        };
        // The join is the ONLY thing keeping RemoveDevice from freeing a context
        // the worker still dereferences. Two ways it used to fail silently:
        //
        // 1. If ObReferenceObjectByHandle failed, the wait was skipped entirely,
        //    the handle was closed, and stop_hpd returned () - success-shaped.
        //    StopDevice then dropped the transport and RemoveDevice freed the
        //    box while the worker was still touching adapter.hpd_event and the
        //    scanout fields. A use-after-free with no breadcrumb.
        // 2. The wait passed a NULL Timeout - the only unbounded wait in this
        //    file - while the worker can be parked in a synchronous host
        //    round-trip (set_scanout_blob) or under the venus mutex. A wedged
        //    host hung PnP stop forever, again with no counter.
        //
        // Both now mean "a worker may still be running", which is recorded and
        // latched so the free path can consult it.
        // 5 s, relative (negative = relative to now, in 100 ns units). Long
        // enough for the worst observed set_scanout_blob round-trip, short
        // enough that PnP stop does not hang indefinitely.
        const JOIN_TIMEOUT_100NS: i64 = -50_000_000;

        // Primary join: the worker's own "I exited" NotificationEvent, set at
        // both of its exit sites immediately before PsTerminateSystemThread.
        // This does NOT depend on the handle-to-object lookup below succeeding,
        // which is the failure that used to skip the wait entirely. A
        // NotificationEvent stays signalled, so the join cannot miss it.
        let mut timeout: wdk_sys::LARGE_INTEGER = unsafe { core::mem::zeroed() };
        timeout.QuadPart = JOIN_TIMEOUT_100NS;
        // SAFETY: initialized in place by init_kernel_events; PASSIVE_LEVEL.
        let exited =
            unsafe { KeWaitForSingleObject(self.hpd_exited.get() as PVOID, 0, 0, 0, &mut timeout) };
        let mut joined = exited == STATUS_SUCCESS;

        if st == STATUS_SUCCESS && !obj.is_null() {
            // Secondary: the thread object itself. The exit event is set just
            // BEFORE PsTerminateSystemThread, so this closes the remaining
            // window between the two - the worker is not yet fully torn down
            // when it signals.
            let mut timeout: wdk_sys::LARGE_INTEGER = unsafe { core::mem::zeroed() };
            timeout.QuadPart = JOIN_TIMEOUT_100NS;
            // SAFETY: waiting on the ETHREAD dispatcher object at PASSIVE_LEVEL.
            let wait = unsafe { KeWaitForSingleObject(obj, 0, 0, 0, &mut timeout) };
            // SAFETY: releasing the reference taken above.
            unsafe { wdk_sys::ntddk::ObfDereferenceObject(obj) };
            joined = wait == STATUS_SUCCESS;
        }
        if !joined {
            // Trading a hang (or a UAF) for a permanent allocation leak plus a
            // live worker. That is the correct trade for a kernel driver, but it
            // must be counted: on a healthy host StHpdX never moves.
            self.hpd_worker_leaked.store(1, Ordering::Release);
            crate::diag::fault(crate::diag::FaultCounter::StHpdX, st as u32);
        }
        // SAFETY: closing the thread handle we created.
        let _ = unsafe { wdk_sys::ntddk::ZwClose(h as wdk_sys::HANDLE) };
    }

    /// True if [`Self::stop_hpd`] could not prove the worker exited, so this
    /// context must never be freed. Consulted by `dxgkddi_remove_device`.
    pub fn hpd_worker_may_be_running(&self) -> bool {
        self.hpd_worker_leaked
            .load(core::sync::atomic::Ordering::Acquire)
            != 0
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

    /// Whether a programming interval is currently outstanding.
    ///
    /// The VSync DPC's gate: while this is true it must not report
    /// `last_primary_address`, because a newer primary is mid-programming.
    pub(crate) fn programming_active(&self) -> bool {
        gate_active(self.vidpn_programming.load(Ordering::Acquire))
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

    /// Remember the blob currently selected for scanout 0. PASSIVE callers bind
    /// it via SET_SCANOUT_BLOB first, then publish it here for dirty-driven
    /// RESOURCE_FLUSH after completed copies.
    pub fn remember_scanout_blob(&self, resource_id: u32, width: u32, height: u32) {
        let wh = ((width as u64) << 32) | height as u64;
        self.active_scanout_wh
            .store(wh, core::sync::atomic::Ordering::Release);
        self.active_scanout_resource
            .store(resource_id, core::sync::atomic::Ordering::Release);
        // Existing callers invoke this only after a synchronous successful
        // SET_SCANOUT_BLOB (diagnostic/bootstrap paths).
        self.host_bound_scanout_resource
            .store(resource_id, core::sync::atomic::Ordering::Release);
    }

    /// Publish the exact Venus import identity of the KMD-owned LINEAR primary.
    /// `resource_id` is stored last so an acquire reader never combines a new id
    /// with stale geometry or allocation parameters.
    pub fn remember_primary_scanout(
        &self,
        resource_id: u32,
        width: u32,
        height: u32,
        pitch: u32,
        plane_offset: u32,
        alloc_size: u64,
        memory_type_index: u32,
        dxgi_format: u32,
    ) {
        use core::sync::atomic::Ordering;
        // Odd: readers must not use the fields between these two bumps.
        self.primary_scanout_seq.fetch_add(1, Ordering::Release);
        self.primary_scanout_wh
            .store(((width as u64) << 32) | height as u64, Ordering::Relaxed);
        self.primary_scanout_layout.store(
            ((pitch as u64) << 32) | plane_offset as u64,
            Ordering::Relaxed,
        );
        self.primary_scanout_alloc_size
            .store(alloc_size, Ordering::Relaxed);
        self.primary_scanout_memory_type
            .store(memory_type_index, Ordering::Relaxed);
        self.primary_scanout_dxgi_format
            .store(dxgi_format, Ordering::Relaxed);
        self.primary_scanout_generation
            .fetch_add(1, Ordering::Relaxed);
        self.primary_scanout_resource
            .store(resource_id, Ordering::Release);
        // Even: the set is coherent again.
        self.primary_scanout_seq.fetch_add(1, Ordering::Release);
    }

    /// Remove a published primary identity only if it still names `resource_id`.
    pub fn forget_primary_scanout(&self, resource_id: u32) {
        use core::sync::atomic::Ordering;
        // The dedicated LINEAR scanout belongs to the adapter, not to any
        // transient WDDM allocation that imports its Venus resource id.  DWM
        // creates and destroys several device/allocation generations during
        // startup; letting one of those destroys clear this identity leaves
        // scanout 0 bound to live-but-unpublishable stale pixels.
        if resource_id != 0
            && self.dedicated_scanout_resource.load(Ordering::Acquire) == resource_id
        {
            return;
        }
        self.primary_scanout_seq.fetch_add(1, Ordering::Release);
        if self
            .primary_scanout_resource
            .compare_exchange(resource_id, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.primary_scanout_generation
                .fetch_add(1, Ordering::Relaxed);
        }
        self.primary_scanout_seq.fetch_add(1, Ordering::Release);
    }

    /// Remember the KMD-owned diagnostic blob. The production scanout can still
    /// change in mode 1; mode 2 callers rebind this blob after each OS scanout
    /// attempt so the host display should show color bars if blob scanout works.
    pub fn remember_diag_scanout_blob(
        &self,
        resource_id: u32,
        width: u32,
        height: u32,
        pitch: u32,
        offset: u32,
    ) {
        let wh = ((width as u64) << 32) | height as u64;
        let layout = ((pitch as u64) << 32) | offset as u64;
        self.diag_scanout_wh
            .store(wh, core::sync::atomic::Ordering::Release);
        self.diag_scanout_layout
            .store(layout, core::sync::atomic::Ordering::Release);
        self.diag_scanout_resource
            .store(resource_id, core::sync::atomic::Ordering::Release);
    }

    /// Queue one non-blocking RESOURCE_FLUSH for the selected scanout.  The
    /// the exact-primary copy's ring-1 completion DPC sets the dirty bit and
    /// wakes the worker only after the Venus GPU copy has completed. One
    /// in-flight command is the backpressure boundary.
    pub(crate) fn queue_active_scanout_refresh(&self) -> ScanoutRefreshQueue {
        let outcome =
            self.with_scanout_lifecycle(|lock| self.queue_active_scanout_refresh_locked(lock));
        // R318: the pacing snapshot runs OUTSIDE `scanout_mutex`. It used to run
        // inside it — 32 synchronous registry transactions every 16 queued
        // refreshes, roughly 3.75 bursts per second at 60 Hz, on the PASSIVE
        // display worker while holding the lock DestroyAllocation must acquire
        // to retire a primary. Every counter read is an independent atomic, so
        // sampling them outside the lock changes no value any consumer compares
        // against another.
        if matches!(outcome, ScanoutRefreshQueue::Queued) {
            self.pacing_snapshot();
        }
        outcome
    }

    /// Low-rate mirror of the DISPATCH/DIRQL-updated pacing counters, which
    /// otherwise become visible only at device teardown.
    ///
    /// ONE rate now (~600 refreshes, about 10 s at 60 Hz): the 16-period set is
    /// folded in. `RfFail`, `RbFail`/`RfUnb` and `ScDead` are deliberately NOT
    /// here — those stay loud and in place at their own sites.
    fn pacing_snapshot(&self) {
        use core::sync::atomic::Ordering;

        let n = self.scanout_refresh_count.load(Ordering::Relaxed);
        let resource_id = self.active_scanout_resource.load(Ordering::Acquire);
        let wh = self.active_scanout_wh.load(Ordering::Relaxed);
        let width = (wh >> 32) as u32;
        let height = wh as u32;
        if !crate::diag::sample_tick(&SCANOUT_PACING_TICKS) {
            return;
        }

        crate::diag::record_named_bytes(b"RfRid", resource_id);
        crate::diag::record_named_bytes(b"RfWH", (width << 16) | (height & 0xFFFF));
        crate::diag::record_named_bytes(b"RfCnt", n);
        crate::diag::record_named_bytes(
            b"RfFail",
            self.scanout_refresh_fail.load(Ordering::Relaxed),
        );
        // R315: the BIND-side failure counter, emitted from the SAME periodic
        // block as the flush-side one. Its only other writer is the enqueue
        // failure path above, which T6's k-lifecycle-02 shows is statically
        // unreachable — so a host-REJECTED SET_SCANOUT_BLOB (counted through
        // the DPC's completion_errors into scanout_bind_fail) used to leave
        // the display silently frozen with no counter movement visible over
        // SSH.
        crate::diag::record_named_bytes(b"RbFail", self.scanout_bind_fail.load(Ordering::Relaxed));
        // Live proof that ctrl completions are reaching the real IRQ/DPC
        // path; these atomics otherwise become visible only at teardown.
        crate::diag::record_named_bytes(
            b"IrqN",
            crate::ddi::interrupt::INT_ROUTINE_COUNT.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"DpcN",
            crate::ddi::interrupt::DPC_ROUTINE_COUNT.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"RfDone",
            crate::virtio::gpu::ASYNC_CTRL_COMPLETE_COUNT.load(Ordering::Relaxed),
        );

        crate::diag::record_named_bytes(b"VsCnt", self.vsync_count.load(Ordering::Relaxed));
        crate::diag::record_named_bytes(b"VsEn", self.vsync_enabled.load(Ordering::Relaxed));
        crate::diag::record_named_bytes(
            b"SaCnt",
            crate::ddi::VIDPN_SOURCE_ADDRESS_COUNT.load(Ordering::Relaxed),
        );
        let primary_address = self.last_primary_address.load(Ordering::Relaxed);
        crate::diag::record_named_bytes(b"SaLo", primary_address as u32);
        crate::diag::record_named_bytes(b"SaHi", (primary_address >> 32) as u32);
        crate::diag::record_named_bytes(
            b"AsSub",
            crate::virtio::gpu::ASYNC_SUBMIT_COUNT.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"AsDone",
            crate::virtio::gpu::ASYNC_COMPLETE_COUNT.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"WfDone",
            crate::virtio::gpu::WDDM_FENCE_FROM_DPC.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"WtOut",
            crate::virtio::gpu::FENCE_WAIT_TIMEOUTS.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"CtOut",
            crate::virtio::gpu::CTRL_TIMEOUT_COUNT.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"DpHit",
            crate::virtio::gpu::DMA_POOL_HITS.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"DpMis",
            crate::virtio::gpu::DMA_POOL_MISSES.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"DpDrp",
            crate::virtio::gpu::DMA_POOL_DROPS.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"DpByt",
            crate::virtio::gpu::DMA_POOL_CACHED_BYTES.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"DmStl",
            crate::ddi::DMA_STALE_SKIP_COUNT.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"QfRet",
            crate::virtio::gpu::QUEUE_FULL_RETRIES.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"IfHi",
            crate::virtio::gpu::INFLIGHT_HIGH_WATER.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"PkHi",
            crate::virtio::gpu::PARKED_HIGH_WATER.load(Ordering::Relaxed),
        );
        // R505: the nine deferred-programming refusal counters. Flushed from
        // HERE — a PASSIVE, already-throttled site — and never from the refusal
        // path itself, which would be a registry write per refused frame.
        crate::ddi::display::record_scanout_reject_counters();
        crate::ddi::record_present_handoff_telemetry();
    }

    /// Scanout refresh implementation. The caller holds `scanout_mutex`, which
    /// prevents a matching WDDM allocation from being unbound/unref'd between
    /// the liveness check and control-queue submission.
    fn queue_active_scanout_refresh_locked(
        &self,
        _lock: &ScanoutGuard<'_>,
    ) -> ScanoutRefreshQueue {
        use core::sync::atomic::Ordering;

        // This is the production path only. Diagnostic fills issue their own
        // explicit one-shot flushes; never query the registry on every frame.
        let resource_id = self.active_scanout_resource.load(Ordering::Acquire);
        let wh = self.active_scanout_wh.load(Ordering::Relaxed);
        let layout = self.active_scanout_layout.load(Ordering::Relaxed);
        let format = self.active_scanout_format.load(Ordering::Relaxed);
        // A newer present may publish while we sample the companion fields.
        // Retry from the worker rather than combine two primary identities.
        if self.active_scanout_resource.load(Ordering::Acquire) != resource_id {
            return ScanoutRefreshQueue::Busy;
        }
        let width = (wh >> 32) as u32;
        let height = wh as u32;
        let stride = (layout >> 32) as u32;
        let offset = layout as u32;
        if !self.display_half() || resource_id == 0 || width == 0 || height == 0 {
            return ScanoutRefreshQueue::Unavailable;
        }
        let live = self
            .with_virtio(|v| v.resource_is_live(resource_id))
            .unwrap_or(false);
        if !live {
            // Only clear the identity we sampled. A newer Windows primary may
            // have been published concurrently by the Present path.
            let _ = self.active_scanout_resource.compare_exchange(
                resource_id,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            if self
                .host_bound_scanout_resource
                .compare_exchange(resource_id, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                crate::diag::record_named_bytes(b"ScDead", resource_id);
            }
            return ScanoutRefreshQueue::Unavailable;
        }
        if self.scanout_bind_inflight.load(Ordering::Acquire) != 0
            || self.scanout_flush_inflight.load(Ordering::Acquire) != 0
        {
            return ScanoutRefreshQueue::Busy;
        }

        if self.host_bound_scanout_resource.load(Ordering::Acquire) != resource_id {
            if stride == 0 || format == 0 {
                return ScanoutRefreshQueue::Unavailable;
            }
            if self
                .scanout_bind_inflight
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return ScanoutRefreshQueue::Busy;
            }
            let result = crate::virtio::ctrl::set_scanout_blob_async(
                self,
                resource_id,
                width,
                height,
                format,
                stride,
                offset,
                NonNull::from(&self.scanout_bind_inflight),
                NonNull::from(&self.scanout_bind_fail),
                NonNull::from(&self.host_bound_scanout_resource),
                NonNull::from(&self.scanout_refresh_pending),
                // SAFETY: embedded initialized event; adapter outlives entry.
                unsafe { NonNull::new_unchecked(self.hpd_event.get()) },
            );
            if result.is_err() {
                self.scanout_bind_inflight.store(0, Ordering::Release);
                let failed = self
                    .scanout_bind_fail
                    .fetch_add(1, Ordering::Relaxed)
                    .wrapping_add(1);
                if failed == 1 || (failed % 60) == 0 {
                    crate::diag::record_named_bytes(b"RbRid", resource_id);
                    crate::diag::record_named_bytes(b"RbFail", failed);
                }
                return ScanoutRefreshQueue::Failed;
            }
            return ScanoutRefreshQueue::Queued;
        }

        if self
            .scanout_flush_inflight
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return ScanoutRefreshQueue::Busy;
        }

        let result = crate::virtio::ctrl::resource_flush_async(
            self,
            resource_id,
            width,
            height,
            NonNull::from(&self.scanout_flush_inflight),
            NonNull::from(&self.scanout_refresh_fail),
            // SAFETY: hpd_event is an embedded, in-place initialized KEVENT and
            // the adapter outlives every transport entry that holds this pointer.
            unsafe { NonNull::new_unchecked(self.hpd_event.get()) },
        );
        if result.is_err() {
            self.scanout_flush_inflight.store(0, Ordering::Release);
            let failed = self
                .scanout_refresh_fail
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1);
            if failed == 1 || (failed % 60) == 0 {
                crate::diag::record_named_bytes(b"RfRid", resource_id);
                crate::diag::record_named_bytes(b"RfFail", failed);
            }
            return ScanoutRefreshQueue::Failed;
        }

        // Count the refresh here (under the lock, where the identity is stable);
        // the TELEMETRY SNAPSHOT is taken by the caller AFTER the lock is
        // released — see `queue_active_scanout_refresh` (R318).
        self.scanout_refresh_count.fetch_add(1, Ordering::Relaxed);
        ScanoutRefreshQueue::Queued
    }

    /// Arm the display-half VSync heartbeat: initialize the embedded KDPC/KTIMER
    /// in place and start a periodic ~16 ms `SynchronizationTimer` whose DPC
    /// (`vsync_dpc_routine`) synthesizes `DXGK_INTERRUPT_CRTC_VSYNC`. Idempotent
    /// (no-op if already armed). PASSIVE_LEVEL only (StartDevice).
    ///
    /// # Safety
    /// `self` must be at its final heap address (dxgkrnl holds it as the miniport
    /// device context) and `dxgkrnl` must already be saved (StartDevice ordering).
    pub unsafe fn init_vsync(&self) {
        use wdk_sys::ntddk::{KeInitializeDpc, KeInitializeTimerEx, KeSetTimerEx};
        if self
            .vsync_armed
            .swap(1, core::sync::atomic::Ordering::AcqRel)
            != 0
        {
            return;
        }
        self.vsync_enabled
            .store(1, core::sync::atomic::Ordering::Release);
        // SAFETY: the KDPC/KTIMER live in this stable boxed context; the DPC
        // context is the adapter pointer, valid for the device lifetime.
        unsafe {
            KeInitializeDpc(
                self.vsync_dpc.get(),
                Some(crate::ddi::vsync_dpc_routine),
                self as *const _ as PVOID,
            );
            KeInitializeTimerEx(
                self.vsync_timer.get(),
                wdk_sys::_TIMER_TYPE::SynchronizationTimer,
            );
            // Relative due time -16 ms (100 ns units); Period 16 ms (recurring).
            let mut due: wdk_sys::LARGE_INTEGER = core::mem::zeroed();
            due.QuadPart = -160_000;
            KeSetTimerEx(self.vsync_timer.get(), due, 16, self.vsync_dpc.get());
        }
    }

    /// Cancel the VSync heartbeat timer (StopDevice / teardown). Idempotent.
    /// PASSIVE_LEVEL. After the flush returns, no VSync DPC is running or queued —
    /// so it is safe for a subsequent RemoveDevice to free this context.
    pub fn cancel_vsync(&self) {
        use core::sync::atomic::Ordering;
        if self.vsync_armed.swap(0, Ordering::AcqRel) == 0 {
            return;
        }
        self.vsync_enabled.store(0, Ordering::Release);
        // SAFETY: the timer was initialized by `init_vsync`; KeCancelTimer is
        // callable at <= DISPATCH_LEVEL and safe on an idle timer. KeFlushQueuedDpcs
        // (PASSIVE_LEVEL only — StopDevice is PASSIVE) then drains any DPC the timer
        // already queued on another CPU before we return, closing the free-after-DPC
        // window against RemoveDevice.
        unsafe {
            wdk_sys::ntddk::KeCancelTimer(self.vsync_timer.get());
            wdk_sys::ntddk::KeFlushQueuedDpcs();
        }
    }

    /// Initialize the embedded kernel dispatcher objects. MUST be called once,
    /// after the context reaches its final (heap) address and before any DDI
    /// can run — `dxgkddi_add_device` calls it right after boxing.
    ///
    /// # Safety
    /// `self` must be at its final address and not yet visible to any other
    /// thread.
    pub unsafe fn init_kernel_events(&self) {
        // SAFETY: per the fn contract; SynchronizationEvent (type 1), initially
        // signaled (the mutex starts free).
        unsafe { KeInitializeEvent(self.venus_mutex.get(), 1, 1) };
        // Same synchronization-event mutex shape as `venus_mutex`, but with a
        // distinct lock order and purpose: scanout lifecycle operations never
        // hold this while acquiring it recursively.
        unsafe { KeInitializeEvent(self.scanout_mutex.get(), 1, 1) };
        // HPD worker wake event: SynchronizationEvent (auto-clears on a satisfied
        // wait), initially unsignaled — the worker's own timeout drives the first
        // indication; later signals come from the config-change DPC.
        // SAFETY: per the fn contract; stable in-place KEVENT storage.
        unsafe { KeInitializeEvent(self.hpd_event.get(), 1, 0) };
        // Worker-exited latch: NotificationEvent (type 0) so it STAYS signalled
        // once set, initially unsignaled. A synchronization event would be
        // consumed by the first waiter and a second stop_hpd would block.
        // SAFETY: per the fn contract; stable in-place KEVENT storage.
        unsafe { KeInitializeEvent(self.hpd_exited.get(), 0, 0) };
    }

    /// Acquire the PASSIVE venus mutex (blocks; PASSIVE_LEVEL only).
    fn acquire_venus_mutex(&self) {
        // SAFETY: the event was initialized in place by `init_kernel_events`;
        // an infinite Executive/KernelMode wait at PASSIVE_LEVEL. The
        // SynchronizationEvent auto-clears on a satisfied wait (mutex acquire).
        let _ = unsafe {
            KeWaitForSingleObject(
                self.venus_mutex.get() as PVOID,
                0, // Executive
                0, // KernelMode
                0, // non-alertable
                core::ptr::null_mut(),
            )
        };
    }

    /// Release the PASSIVE venus mutex.
    fn release_venus_mutex(&self) {
        // SAFETY: initialized event; KeSetEvent with Wait=FALSE is callable at
        // <= DISPATCH_LEVEL (we are at PASSIVE).
        unsafe { KeSetEvent(self.venus_mutex.get(), 0, 0) };
    }

    /// Serialize a PASSIVE scanout operation against exact-resource retirement.
    ///
    /// The closure may block on virtio/Venus work, so this cannot use the
    /// DISPATCH-safe transport spinlock. Callers must not invoke it recursively —
    /// see [`ScanoutGuard`] for what the token does and does not prove.
    pub(crate) fn with_scanout_lifecycle<R>(
        &self,
        f: impl FnOnce(&ScanoutGuard<'_>) -> R,
    ) -> R {
        // SAFETY: initialized in place by `init_kernel_events`; all callers are
        // PASSIVE-level display worker or allocation-lifecycle paths.
        let _ = unsafe {
            KeWaitForSingleObject(
                self.scanout_mutex.get() as PVOID,
                0, // Executive
                0, // KernelMode
                0, // non-alertable
                core::ptr::null_mut(),
            )
        };
        let guard = ScanoutGuard {
            adapter: self,
            _not_send: PhantomData,
        };
        let result = f(&guard);
        // SAFETY: release the synchronization-event mutex acquired above.
        unsafe { KeSetEvent(self.scanout_mutex.get(), 0, 0) };
        result
    }

    /// Retire one exact Windows allocation/resource identity from scanout 0.
    ///
    /// Returns false only when the mandatory host unbind could not be
    /// confirmed. The caller must then retain the host resource until device
    /// teardown rather than RESOURCE_UNREF a blob QEMU may still sample.
    pub(crate) fn retire_scanout_allocation(
        &self,
        allocation_handle: usize,
        resource_id: u32,
    ) -> bool {
        self.with_scanout_lifecycle(|lock| {
            self.retire_scanout_allocation_locked(lock, allocation_handle, resource_id)
        })
    }

    /// Body of [`Self::retire_scanout_allocation`]. Separate so the critical
    /// section's contents are a named function taking the lock token rather than
    /// an inline closure; the generated critical section is identical.
    fn retire_scanout_allocation_locked(
        &self,
        _lock: &ScanoutGuard<'_>,
        allocation_handle: usize,
        resource_id: u32,
    ) -> bool {
        use core::sync::atomic::Ordering;

        // SetVidPnSourceAddress can publish its exact KMD allocation handle
        // at DIRQL for later PASSIVE processing. DestroyAllocation owns the
        // same exact pointer; cancel it while serialized with the worker's
        // swap-and-dereference before the Box can be freed.
        if allocation_handle != 0
            && self
                .pending_vidpn_allocation
                .compare_exchange(allocation_handle, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            // We just cancelled a deferred SetVidPnSourceAddress, so the HPD
            // worker's swap() will now yield 0 and
            // process_deferred_vidpn_source_address will return None -
            // meaning nobody reaches any of the ten sites that clear
            // `vidpn_programming`. A gate left at 1 makes vsync_dpc_routine
            // early-return on every 16 ms tick before it increments
            // vsync_count, so CRTC_VSYNC stops, dxgkrnl never retires the
            // queued flip, and it therefore never issues the next
            // SetVidPnSourceAddress that would re-arm the gate. The display
            // is wedged for the rest of the boot.
            //
            // Both conditions below are load-bearing. SetVidPnSourceAddress
            // runs at DIRQL and does NOT take the scanout lifecycle lock, so
            // a NEWER program can raise the gate immediately after our CAS -
            // re-read `pending` and only act while it is still 0. And
            // compare_exchange(1, 0) rather than store(0) keeps us from
            // clearing a gate we never observed set, which is the stale-clear
            // window that would let the VSync DPC report a primary address
            // the host has not sampled yet.
            if self.pending_vidpn_allocation.load(Ordering::Acquire) == 0
                && self.cancel_programming_gate()
            {
                crate::diag::record_named_bytes(b"VpCncl", allocation_handle as u32);
            }
        }
        if resource_id == 0 {
            return true;
        }

        let was_active = self
            .active_scanout_resource
            .compare_exchange(resource_id, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        let was_host_bound =
            self.host_bound_scanout_resource.load(Ordering::Acquire) == resource_id;
        if !was_active && !was_host_bound {
            return true;
        }
        if !was_host_bound {
            // The retiring allocation was only a newer desired candidate;
            // a different resource is still bound on the host. Clearing
            // the candidate above is sufficient. Sending scanout-disable
            // here would blank the unrelated Windows-selected primary.
            crate::diag::record_named_bytes(b"ScRet", resource_id);
            return true;
        }

        // QEMU's virtio-gpu SET_SCANOUT_BLOB contract treats resource_id=0
        // as scanout disable before any resource lookup. Because this
        // synchronous command is queued after all earlier async scanout
        // commands, its response is the lifetime barrier before UNREF.
        let unbound = crate::virtio::ctrl::set_scanout_blob(self, 0, 0, 0, 0, 0, 0).is_ok();
        if !unbound {
            crate::diag::record_named_bytes(b"ScRet", 0xE);
            crate::diag::record_named_bytes(b"ScDead", resource_id);
            return false;
        }

        self.host_bound_scanout_resource.store(0, Ordering::Release);
        self.scanout_refresh_pending.store(0, Ordering::Release);
        crate::diag::record_named_bytes(b"ScRet", resource_id);

        // The DISPATCH Present path can publish a newer exact Windows
        // primary without taking this PASSIVE lock. If that happened while
        // the old resource was retiring, make the new identity drive a
        // fresh bind instead of treating the just-issued unbind as final.
        if self.active_scanout_resource.load(Ordering::Acquire) != 0 {
            self.request_scanout_refresh();
        }
        true
    }

    /// Run `f` against the persistent venus client under the PASSIVE venus
    /// mutex. Returns `DeviceNotFound` if no client is installed. PASSIVE_LEVEL
    /// only — `f` may block on host round-trips (`virtio::ctrl`) and venus ring
    /// progress.
    pub fn with_venus_client<R>(
        &self,
        f: impl FnOnce(&mut crate::virtio::venus::VenusClient) -> R,
    ) -> Result<R, DriverError> {
        self.acquire_venus_mutex();
        // SAFETY: the venus mutex gives exclusive access to the cell.
        let result = match unsafe { &mut *self.venus_client.get() } {
            Some(client) => Ok(f(client)),
            None => Err(DriverError::DeviceNotFound),
        };
        self.release_venus_mutex();
        result
    }

    /// Allocate a zeroed, physically-contiguous non-paged RAM block. PASSIVE.
    pub(crate) fn alloc_contiguous_ram(size: usize) -> Option<PagingRam> {
        // Permit the contiguous block anywhere in the 64-bit physical space.
        let mut highest: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
        highest.QuadPart = i64::MAX;
        // SAFETY: PASSIVE_LEVEL; allocates `size` bytes of physically-
        // contiguous non-paged memory, or null on failure.
        let va = unsafe { MmAllocateContiguousMemory(size as u64, highest) };
        let Some(va) = NonNull::new(va as *mut u8) else {
            crate::diag::record(0x0A00_00E3);
            crate::diag::fault(crate::diag::FaultCounter::StRam, size as u32);
            return None;
        };
        // SAFETY: zero the region so VidMm never reads stale bytes.
        unsafe { core::ptr::write_bytes(va.as_ptr(), 0, size) };
        // SAFETY: `va` is a valid non-paged kernel address.
        let phys = unsafe { MmGetPhysicalAddress(va.as_ptr() as *mut _).QuadPart } as u64;
        crate::diag::record(0x0A00_0003);
        crate::diag::record((phys >> 12).min(0xFFFF_FFFF) as u32);
        Some(PagingRam {
            va,
            phys,
            size: size as u64,
        })
    }

    /// Allocate the real-RAM-backed paging/page-table segment. Called from
    /// StartDevice, after PnP has accepted the display miniport context, so AddDevice
    /// stays a cheap context-allocation step.
    pub(crate) fn alloc_paging_ram() -> Option<PagingRam> {
        Self::alloc_contiguous_ram(PAGING_RAM_SIZE)
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

    /// As [`Self::take_paging_ram`], for the `BarSegMode` 5 probe block.
    ///
    /// # Safety
    /// Same contract as [`Self::take_paging_ram`].
    pub(crate) unsafe fn take_bar_probe_ram(&self) -> Option<PagingRam> {
        // SAFETY: per the fn contract.
        unsafe { (*self.started.get()).as_deref_mut() }.and_then(|s| s.bar_probe_ram.take())
    }

    /// Free a contiguous RAM block that is being replaced rather than carried
    /// forward. PASSIVE_LEVEL.
    pub(crate) fn free_contiguous_ram(ram: PagingRam) {
        // SAFETY: `va` came from MmAllocateContiguousMemory and is freed once.
        unsafe { MmFreeContiguousMemory(ram.va.as_ptr() as *mut _) };
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
    pub fn dxgkrnl(&self) -> Result<&DXGKRNL_INTERFACE, DriverError> {
        self.dxgkrnl_opt().ok_or(DriverError::DeviceNotFound)
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

    /// `DisplayHalf`: whether the display DDIs answer for real. False before
    /// StartDevice and on the render-only recovery shape.
    pub fn display_half(&self) -> bool {
        self.started().is_some_and(|s| s.display_half)
    }

    /// `AllocCached`. Defaults to TRUE before StartDevice, matching the value the
    /// field was constructed with.
    pub fn alloc_cached(&self) -> bool {
        self.started().map_or(true, |s| s.alloc_cached)
    }

    /// `PresentProbe`. Defaults to false before StartDevice.
    pub fn present_probe(&self) -> bool {
        self.started().is_some_and(|s| s.present_probe)
    }

    /// `ScForceReject` — the T3 gate instrument. 0 = off (the shipped
    /// default and the only value present in a production registry).
    pub(crate) fn forced_reject(&self) -> u32 {
        self.started().map_or(0, |s| s.forced_reject)
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

    /// The venus-backed page-table window for this transport generation.
    ///
    /// See the field doc: no live consumer today.
    #[allow(dead_code)]
    pub fn page_table_window(&self) -> Option<(u64, u64)> {
        self.transport_generation().and_then(|t| t.page_table_window)
    }

    /// Lock-free observation for query/diagnostic paths. Mutation is exposed
    /// only through [`WddmNotifyGuard`], so advancing the scheduler watermark
    /// statically requires ownership of the notification-lock proof.
    pub(crate) fn completed_fence(&self) -> u32 {
        self.last_completed_fence.load(Ordering::Acquire)
    }

    /// Serialize one scheduler notification at DISPATCH_LEVEL. The closure
    /// must not wait or allocate; it may raise further to the device DIRQL via
    /// `DxgkCbSynchronizeExecution`. The closure receives an unforgeable proof
    /// token required by every operation whose contract depends on this lock.
    pub(crate) fn with_wddm_notify_lock<R>(&self, f: impl FnOnce(&WddmNotifyGuard<'_>) -> R) -> R {
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(self.wddm_notify_lock.get()) };
        let guard = WddmNotifyGuard { adapter: self };
        let result = f(&guard);
        unsafe { KeReleaseSpinLock(self.wddm_notify_lock.get(), irql) };
        result
    }

    /// Install (or clear) the virtio transport under the lock.
    ///
    /// The previous transport, if any, is dropped *after* the lock is released:
    /// `VirtioGpu::drop` resets the device and frees contiguous memory, both of
    /// which are PASSIVE_LEVEL-only — they must not run at the DISPATCH_LEVEL the
    /// spinlock raises to. MUST be called at PASSIVE_LEVEL (StartDevice /
    /// StopDevice, which Dxgkrnl serializes).
    pub fn set_virtio(&self, new: Option<VirtioGpu>) {
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

    /// Run `f` against the live virtio transport while holding `virtio_lock`.
    ///
    /// Returns `DeviceNotFound` if the transport is not currently up. `f` runs at
    /// DISPATCH_LEVEL (spinlock held): it must not allocate or call pageable code.
    /// Stage any payload (e.g. a Venus stream) into a `DmaBuffer` *before* calling
    /// this, then pass a slice of it into `f`.
    pub fn with_virtio<R>(&self, f: impl FnOnce(&mut VirtioGpu) -> R) -> Result<R, DriverError> {
        // SAFETY: spinlock-guarded exclusive access to the cell's contents for the
        // duration of the critical section.
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(self.virtio_lock.get()) };
        let result = match unsafe { &mut *self.virtio.get() } {
            Some(v) => Ok(f(v)),
            None => Err(DriverError::DeviceNotFound),
        };
        unsafe { KeReleaseSpinLock(self.virtio_lock.get(), irql) };
        result
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
        self.cancel_vsync();
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
            if let Some(pr) = state.bar_probe_ram.take() {
                // SAFETY: same contract as paging_ram (alloc_contiguous_ram),
                // freed once.
                unsafe { MmFreeContiguousMemory(pr.va.as_ptr() as *mut _) };
            }
        }
    }
}
