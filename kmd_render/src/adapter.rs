//! Adapter context — one per virtio-gpu device the driver binds to.
//!
//! Allocated in `DxgkDdiAddDevice`, populated in `DxgkDdiStartDevice`, freed in
//! `DxgkDdiRemoveDevice`. Dxgkrnl hands this back to us as the opaque
//! `MiniportDeviceContext` in every subsequent DDI call.

use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicUsize};

use wdk_sys::ntddk::{
    KeAcquireSpinLockRaiseToDpc, KeInitializeEvent, KeReleaseSpinLock, KeSetEvent,
    KeWaitForSingleObject, MmAllocateContiguousMemory, MmFreeContiguousMemory,
    MmGetPhysicalAddress,
};
use wdk_sys::{KEVENT, KSPIN_LOCK, PHYSICAL_ADDRESS, PVOID};

use crate::dxgk::*;
use crate::error::DriverError;
use crate::virtio::VirtioGpu;

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
            venus_client: UnsafeCell::new(None),
            // Zeroed placeholder — the real dispatcher header is written by
            // `init_kernel_events` once the context is at its final address.
            venus_mutex: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            venus_ctx_id: 0,
        })
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

    /// Allocate the real-RAM-backed paging/page-table segment. Called from
    /// StartDevice, after PnP has accepted the display miniport context, so AddDevice
    /// stays a cheap context-allocation step.
    pub(crate) fn alloc_paging_ram() -> Option<PagingRam> {
        // Permit the contiguous block anywhere in the 64-bit physical space.
        let mut highest: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
        highest.QuadPart = i64::MAX;
        // SAFETY: PASSIVE_LEVEL; allocates `PAGING_RAM_SIZE` of physically-
        // contiguous non-paged memory, or null on failure.
        let va = unsafe { MmAllocateContiguousMemory(PAGING_RAM_SIZE as u64, highest) };
        let Some(va) = NonNull::new(va as *mut u8) else {
            crate::diag::record(0x0A00_00E3);
            return None;
        };
        // SAFETY: zero the region so VidMm never reads stale page-table bytes.
        unsafe { core::ptr::write_bytes(va.as_ptr(), 0, PAGING_RAM_SIZE) };
        // SAFETY: `va` is a valid non-paged kernel address.
        let phys = unsafe { MmGetPhysicalAddress(va.as_ptr() as *mut _).QuadPart } as u64;
        crate::diag::record(0x0A00_0003);
        crate::diag::record((phys >> 12).min(0xFFFF_FFFF) as u32);
        Some(PagingRam {
            va,
            phys,
            size: PAGING_RAM_SIZE as u64,
        })
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
        // Free the contiguous paging-RAM segment. RemoveDevice (which drops the
        // boxed AdapterContext) runs at PASSIVE_LEVEL, where MmFreeContiguousMemory
        // is legal.
        if let Some(pr) = self.paging_ram.take() {
            // SAFETY: `va` came from MmAllocateContiguousMemory in `alloc_paging_ram`
            // and is freed exactly once here.
            unsafe { MmFreeContiguousMemory(pr.va.as_ptr() as *mut _) };
        }
    }
}
