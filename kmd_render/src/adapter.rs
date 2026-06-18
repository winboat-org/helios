//! Adapter context — one per virtio-gpu device the driver binds to.
//!
//! Allocated in `DxgkDdiAddDevice`, populated in `DxgkDdiStartDevice`, freed in
//! `DxgkDdiRemoveDevice`. Dxgkrnl hands this back to us as the opaque
//! `MiniportDeviceContext` in every subsequent DDI call.

use core::cell::UnsafeCell;
use core::sync::atomic::AtomicU32;

use wdk_sys::ntddk::{KeAcquireSpinLockRaiseToDpc, KeReleaseSpinLock};
use wdk_sys::KSPIN_LOCK;

use crate::dxgk::*;
use crate::error::DriverError;
use crate::virtio::VirtioGpu;

pub struct AdapterContext {
    /// Physical device object for the virtio-gpu device.
    pub pdo: PDEVICE_OBJECT,
    /// Dxgkrnl callback interface, saved in StartDevice. `None` until then.
    /// Written once during the (serialized) StartDevice lifecycle DDI.
    pub dxgkrnl: Option<DXGKRNL_INTERFACE>,
    /// Last fence completed by the bring-up scheduler path.
    pub last_completed_fence: AtomicU32,
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
            virtio_lock: UnsafeCell::new(0),
            virtio: UnsafeCell::new(None),
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
}
