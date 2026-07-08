//! Adapter context — one per virtio-gpu device the driver binds to.
//!
//! Allocated in `DxgkDdiAddDevice`, populated in `DxgkDdiStartDevice`, freed in
//! `DxgkDdiRemoveDevice`. Dxgkrnl hands this back to us as the opaque
//! `MiniportDeviceContext` in every subsequent DDI call.

use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize};

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

pub struct AdapterContext {
    /// Physical device object for the virtio-gpu device.
    pub pdo: PDEVICE_OBJECT,
    /// Dxgkrnl callback interface, saved in StartDevice. `None` until then.
    /// Written once during the (serialized) StartDevice lifecycle DDI.
    pub dxgkrnl: Option<DXGKRNL_INTERFACE>,
    /// Last fence completed by the bring-up scheduler path.
    pub last_completed_fence: AtomicU32,
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
    /// Live host-visible blob → user-VA mappings (Gate 5a Stage 2b). Tagged by the
    /// owning D3D device handle (`DXGKARG_ESCAPE.hDevice`); `DxgkDdiDestroyDevice`
    /// drains and unmaps them. Has its own spinlock, independent of `virtio_lock`,
    /// so teardown works even after the transport is gone.
    pub mappings: crate::mapping::MappingTable,
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
    /// `GdiAccelMode` service-key knob (read once in StartDevice; default 1).
    /// 0 = do not advertise `SupportKernelModeCommandBuffer` (GDI HW accel):
    /// win32k then rasterizes GDI on the CPU into CpuVisible allocations and
    /// the RenderGdi executor goes idle. Retests the 2026-07-02
    /// "LOAD-MANDATORY" bisect, which predates the Option A BAR segment
    /// (ROADMAP WS1 #8); viogpu3d never sets the bit and loads fine.
    pub gdi_accel_mode: u32,
    /// `AllocCached` service-key knob (read once in StartDevice; default 1).
    /// When set, CpuVisible allocations are additionally flagged `Cached` so
    /// dxgkrnl maps CPU views write-back instead of write-combined. The BAR
    /// window is RAM-backed host shmem (x86 cache-coherent for all agents on
    /// the same physical pages); WC reads measured ~200 MB/s in the IDD
    /// readback (36 ms per 7.8 MiB frame, 2026-07-06). 0 = kill switch.
    pub alloc_cached: bool,
    /// `DisplayHalf` service-key knob (REG_DWORD, read once in StartDevice;
    /// default 0 = OFF, the boot-proven render-only surface). When nonzero,
    /// StartDevice advertises ONE video-present source + ONE child video-output
    /// and the VidPn/child DDIs in `ddi::display`/`ddi::vidpn`/`ddi::start_device`
    /// stand up a real (virtual, no-scanout) VidPn output + default monitor,
    /// instead of returning NOT_SUPPORTED. This is priority #1's Option A: give
    /// Helios a genuine presentable output so legacy BLT-model windowed present
    /// (DXUT/FaceWorks, older 3DMark) resolves a real output instead of being
    /// declared DXGI_STATUS_OCCLUDED (WINDOWED_BLT_DESIGN.md §6.2). Default 0
    /// keeps every build bootable and the desktop unchanged; A/B via
    /// `reg add ... /v DisplayHalf /t REG_DWORD /d 1` + `pnputil /restart-device`
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
    /// Physical address of the last primary bound by `SetVidPnSourceAddress`,
    /// reported in each CRTC_VSYNC packet so dxgkrnl can retire the matching queued
    /// flip (viogpu3d `m_sourceAddress`). 0 until the first source-address bind.
    pub last_primary_address: AtomicU64,
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
    /// that thread: once shortly after start (initial indication) and again on every
    /// virtio config-change interrupt (`VIRTIO_GPU_EVENT_DISPLAY`, ISR bit 1 → DPC).
    pub hpd_event: UnsafeCell<KEVENT>,
    /// PsCreateSystemThread handle for the HPD worker (0 = not started). StopDevice
    /// signals `hpd_stop` + `hpd_event`, joins the thread on this handle, then closes it.
    hpd_thread: AtomicUsize,
    /// Tells the HPD worker to terminate (StopDevice / teardown).
    pub hpd_stop: AtomicU32,
    /// Set by the ISR when the virtio config-change bit (ISR status bit 1) fires; the
    /// DPC consumes it and signals `hpd_event` so the worker re-indicates connection.
    pub config_change_pending: AtomicU32,
}

// SAFETY: `dxgkrnl` is written only during the device-lifecycle DDIs, which
// Dxgkrnl serializes. `virtio` is interior-mutable but every access goes through
// `virtio_lock` (a kernel spinlock) via `with_virtio`/`set_virtio`, so concurrent
// escape/DPC callers never alias it. This is the genuine lock-guarded state that
// replaces Phase-2's hand-asserted-without-a-lock Send/Sync.
unsafe impl Send for AdapterContext {}
unsafe impl Sync for AdapterContext {}

impl AdapterContext {
    pub fn new(pdo: PDEVICE_OBJECT) -> Result<Self, DriverError> {
        Ok(Self {
            pdo,
            dxgkrnl: None,
            last_completed_fence: AtomicU32::new(0),
            isr_status: AtomicUsize::new(0),
            virtio_lock: UnsafeCell::new(0),
            virtio: UnsafeCell::new(None),
            mappings: crate::mapping::MappingTable::new(),
            paging_ram: None,
            page_table_window: None,
            bar_segment: None,
            bar_probe_ram: None,
            venus_client: UnsafeCell::new(None),
            // Zeroed placeholder — the real dispatcher header is written by
            // `init_kernel_events` once the context is at its final address.
            venus_mutex: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            venus_ctx_id: 0,
            gdi_accel_mode: 1,
            alloc_cached: true,
            display_half: false,
            // Zeroed placeholders — the real KTIMER/KDPC dispatcher state is
            // written by `init_vsync` once the context is at its final address.
            vsync_timer: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            vsync_dpc: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            vsync_enabled: AtomicU32::new(0),
            vsync_count: AtomicU32::new(0),
            last_primary_address: AtomicU64::new(0),
            vsync_armed: AtomicU32::new(0),
            display_w: 0,
            display_h: 0,
            edid: [0u8; 128],
            // Zeroed placeholder — the real KEVENT is written by init_kernel_events.
            hpd_event: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            hpd_thread: AtomicUsize::new(0),
            hpd_stop: AtomicU32::new(0),
            config_change_pending: AtomicU32::new(0),
        })
    }

    /// Start the HPD worker thread (StartDevice, display half). PASSIVE_LEVEL.
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
        }
    }

    /// Wake the HPD worker to re-indicate connection (from the interrupt DPC at
    /// DISPATCH_LEVEL — KeSetEvent with Wait=FALSE is legal there).
    pub fn signal_hpd(&self) {
        // SAFETY: hpd_event was initialized in place by init_kernel_events.
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
        if st == STATUS_SUCCESS && !obj.is_null() {
            // SAFETY: waiting on the ETHREAD dispatcher object at PASSIVE_LEVEL.
            unsafe {
                let _ = KeWaitForSingleObject(obj, 0, 0, 0, core::ptr::null_mut());
                wdk_sys::ntddk::ObfDereferenceObject(obj);
            }
        }
        // SAFETY: closing the thread handle we created.
        let _ = unsafe { wdk_sys::ntddk::ZwClose(h as wdk_sys::HANDLE) };
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
        if self.vsync_armed.swap(1, core::sync::atomic::Ordering::AcqRel) != 0 {
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
        // HPD worker wake event: SynchronizationEvent (auto-clears on a satisfied
        // wait), initially unsignaled — the worker's own timeout drives the first
        // indication; later signals come from the config-change DPC.
        // SAFETY: per the fn contract; stable in-place KEVENT storage.
        unsafe { KeInitializeEvent(self.hpd_event.get(), 1, 0) };
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
