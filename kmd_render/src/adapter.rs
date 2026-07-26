//! Adapter context — one per virtio-gpu device the driver binds to.
//!
//! Allocated in `DxgkDdiAddDevice`, populated in `DxgkDdiStartDevice`, freed in
//! `DxgkDdiRemoveDevice`. Dxgkrnl hands this back to us as the opaque
//! `MiniportDeviceContext` in every subsequent DDI call.

use alloc::sync::Arc;
use alloc::vec::Vec;

use core::cell::UnsafeCell;
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

pub struct AdapterContext {
    /// Physical device object for the virtio-gpu device.
    pub pdo: PDEVICE_OBJECT,
    /// Dxgkrnl callback interface, saved in StartDevice. `None` until then.
    /// Written once during the (serialized) StartDevice lifecycle DDI.
    pub dxgkrnl: Option<DXGKRNL_INTERFACE>,
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
    /// Real-RAM-backed segment for VidMm page tables / paging buffers (segment 2).
    /// `None` if the contiguous allocation failed (then we fall back to the old
    /// single-segment shape). Allocated in `new`, freed in `Drop`.
    pub paging_ram: Option<PagingRam>,
    /// venus-backed, BAR-visible, CPU-coherent page-table region self-allocated at
    /// StartDevice (`(gpa, size)`). `None` if the venus allocation was unavailable
    /// or failed (StartDevice stays best-effort). When present and the aperture
    /// shape is enabled, `query_segments` reports this as the VidMm page-table
    /// segment (segment id 2) — real device-BAR memory backed by real host memory,
    /// which VidMm accepts where it drops a system-RAM segment. See `venus.rs`.
    pub page_table_window: Option<(u64, u64)>,
    /// BAR memory segment (segment 3) — the head partition of the host-visible
    /// window, reserved as dxgkrnl's CPU-host-aperture region at StartDevice.
    /// `None` if the window is absent/too small; segment 3 is then not
    /// reported and standard allocations stay on the aperture (old behavior).
    pub bar_segment: Option<BarSegment>,
    /// RAM block backing the `BarSegMode` 5 AddAdapter-acceptance probe (the
    /// segment-3 aperture region is then real RAM instead of the BAR window).
    /// Freed in Drop.
    pub bar_probe_ram: Option<PagingRam>,
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
    /// The persistent venus 3D context id (`VIRTIO_GPU_CAPSET_VENUS`) the venus
    /// client rides, created in StartDevice and destroyed in StopDevice. `0` = none.
    pub venus_ctx_id: u32,
    /// `GdiAccelMode` service-key diagnostic rollback knob (read once in
    /// StartDevice; default 0).
    /// 0 = do not advertise `SupportKernelModeCommandBuffer` (GDI HW accel):
    /// win32k then rasterizes GDI on the CPU into CpuVisible allocations and
    /// the RenderGdi executor stays unreachable. The 2026-07-06 mode-0 A/B
    /// overturned the earlier "LOAD-MANDATORY" result and proved the canonical
    /// CPU redirection path; explicit mode 1 remains only for diagnostics until
    /// the obsolete KMD CPU blitter is removed.
    pub gdi_accel_mode: u32,
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
    /// `DisplayHalf` service-key knob (REG_DWORD, read once in StartDevice;
    /// default 0 = OFF, the boot-proven render-only surface). When nonzero,
    /// StartDevice advertises ONE video-present source + ONE child video-output
    /// and the VidPn/child DDIs in `ddi::display`/`ddi::vidpn`/`ddi::start_device`
    /// stand up a real virtual VidPn output + default monitor and drive
    /// virtio-gpu scanout, instead of returning NOT_SUPPORTED. Default 0 keeps
    /// the render-only recovery shape available; production sets it to 1 via
    /// `reg add ... /v DisplayHalf /t REG_DWORD /d 1` + a guest reboot
    /// (re-runs StartDevice → child enumeration) with NO reboot once deployed.
    /// The paired Looking Glass IddCx keeps rendering the desktop as before; the
    /// new Helios monitor is a second, unobserved virtual display (owner-approved,
    /// 35th session). Value mirrored to the `DspH` fixed diag record at StartDevice.
    pub display_half: bool,
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
    pub vidpn_programming: AtomicU32,
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
    /// Scanout-0 mode the display half presents, taken from the host's
    /// `GET_DISPLAY_INFO` (`VirtioGpu::display_mode`) at StartDevice, or 0/0 if the
    /// host reported nothing usable (then [`Self::display_mode`] falls back to
    /// 1920×1080). The VidPn source/target/monitor modes and the generated EDID all
    /// derive from this, so Helios advertises the size QEMU actually wants on
    /// scanout 0 instead of a hardcoded guess.
    pub display_w: u32,
    pub display_h: u32,
    /// EDID served by `DxgkDdiQueryDeviceDescriptor`, generated at StartDevice for
    /// [`Self::display_mode`] via [`crate::ddi::vidpn::build_edid`] (valid checksum,
    /// native detailed timing == the mode). Zeroed until the display half fills it.
    pub edid: [u8; 128],
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
}

// SAFETY: `dxgkrnl` is written only during the device-lifecycle DDIs, which
// Dxgkrnl serializes. `virtio` is interior-mutable but every access goes through
// `virtio_lock` (a kernel spinlock) via `with_virtio`/`set_virtio`, so concurrent
// escape/DPC callers never alias it. This is the genuine lock-guarded state that
// replaces Phase-2's hand-asserted-without-a-lock Send/Sync.
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

impl WddmNotifyGuard<'_> {
    pub(crate) fn completed_fence(&self) -> u32 {
        self.adapter.last_completed_fence.load(Ordering::Acquire)
    }

    pub(crate) fn set_completed_fence(&self, fence: u32) {
        self.adapter
            .last_completed_fence
            .store(fence, Ordering::Release);
    }
}

/// Outcome of trying to queue one coalesced scanout refresh.
pub(crate) enum ScanoutRefreshQueue {
    Queued,
    Busy,
    Failed,
    Unavailable,
}

impl AdapterContext {
    pub fn new(pdo: PDEVICE_OBJECT) -> Result<Self, DriverError> {
        Ok(Self {
            pdo,
            dxgkrnl: None,
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
            paging_ram: None,
            page_table_window: None,
            bar_segment: None,
            bar_probe_ram: None,
            venus_client: UnsafeCell::new(None),
            // Zeroed placeholder — the real dispatcher header is written by
            // `init_kernel_events` once the context is at its final address.
            venus_mutex: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            venus_ctx_id: 0,
            gdi_accel_mode: 0,
            alloc_cached: true,
            present_probe: false,
            display_half: false,
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
            vidpn_programming: AtomicU32::new(0),
            display_w: 0,
            display_h: 0,
            edid: [0u8; 128],
            // Zeroed placeholder — the real KEVENT is written by init_kernel_events.
            hpd_event: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            hpd_exited: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            hpd_thread: AtomicUsize::new(0),
            hpd_stop: AtomicU32::new(0),
            hpd_worker_leaked: AtomicU32::new(0),
            config_change_pending: AtomicU32::new(0),
        })
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
        let was_programming = self.vidpn_programming.load(Ordering::Acquire);
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
        self.primary_scanout_resource.store(0, Ordering::Release);
        self.primary_scanout_wh.store(0, Ordering::Release);
        self.primary_scanout_layout.store(0, Ordering::Release);
        self.primary_scanout_alloc_size.store(0, Ordering::Release);
        self.primary_scanout_memory_type.store(0, Ordering::Release);
        self.primary_scanout_dxgi_format.store(0, Ordering::Release);
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
    pub fn display_mode(&self) -> (u32, u32) {
        if self.display_w >= 320 && self.display_h >= 240 {
            (self.display_w, self.display_h)
        } else {
            (
                crate::ddi::vidpn::DEFAULT_MODE_WIDTH,
                crate::ddi::vidpn::DEFAULT_MODE_HEIGHT,
            )
        }
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
        if self
            .primary_scanout_resource
            .compare_exchange(resource_id, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.primary_scanout_generation
                .fetch_add(1, Ordering::Relaxed);
        }
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
        self.with_scanout_lifecycle(|| self.queue_active_scanout_refresh_locked())
    }

    /// Scanout refresh implementation. The caller holds `scanout_mutex`, which
    /// prevents a matching WDDM allocation from being unbound/unref'd between
    /// the liveness check and control-queue submission.
    fn queue_active_scanout_refresh_locked(&self) -> ScanoutRefreshQueue {
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
        if !self.display_half || resource_id == 0 || width == 0 || height == 0 {
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

        let n = self
            .scanout_refresh_count
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        // Registry writes are synchronous and must not become a once-per-second
        // frame-path tax at 60 Hz. Refresh live telemetry every ~10 seconds.
        if n == 1 || (n % 600) == 0 {
            crate::diag::record_named_bytes(b"RfRid", resource_id);
            crate::diag::record_named_bytes(b"RfWH", (width << 16) | (height & 0xFFFF));
            crate::diag::record_named_bytes(b"RfCnt", n);
            crate::diag::record_named_bytes(
                b"RfFail",
                self.scanout_refresh_fail.load(Ordering::Relaxed),
            );
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
        }
        // Low-rate live pacing telemetry. These counters are updated at
        // DISPATCH/DIRQL and otherwise become visible only at device teardown;
        // one PASSIVE snapshot per 16 queued scanout operations is cheap enough
        // to keep while making a 2.4-fps stall diagnosable in the live boot.
        if n == 1 || (n % 16) == 0 {
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
            crate::ddi::record_present_handoff_telemetry();
        }
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
    /// DISPATCH-safe transport spinlock. Callers must not invoke it recursively.
    pub(crate) fn with_scanout_lifecycle<R>(&self, f: impl FnOnce() -> R) -> R {
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
        let result = f();
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
        use core::sync::atomic::Ordering;

        self.with_scanout_lifecycle(|| {
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
                    && self
                        .vidpn_programming
                        .compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
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
        })
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

    /// Borrow the Dxgkrnl interface, or fail if StartDevice has not run yet.
    pub fn dxgkrnl(&self) -> Result<&DXGKRNL_INTERFACE, DriverError> {
        self.dxgkrnl.as_ref().ok_or(DriverError::DeviceNotFound)
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
        self.paging_ram.as_ref().map(|p| (p.phys, p.size))
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
        if let Some(pr) = self.paging_ram.take() {
            // SAFETY: `va` came from MmAllocateContiguousMemory in `alloc_paging_ram`
            // and is freed exactly once here.
            unsafe { MmFreeContiguousMemory(pr.va.as_ptr() as *mut _) };
        }
        if let Some(pr) = self.bar_probe_ram.take() {
            // SAFETY: same contract as paging_ram (alloc_contiguous_ram), freed once.
            unsafe { MmFreeContiguousMemory(pr.va.as_ptr() as *mut _) };
        }
    }
}
