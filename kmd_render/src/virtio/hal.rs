//! `virtio_drivers::Hal` backed by Windows kernel primitives.
//!
//! virtio-drivers calls these (static — no `&self`) to allocate the DMA-coherent
//! ring/command memory and to map device BARs. We satisfy them with
//! `MmAllocateContiguousMemory` (physically contiguous, non-paged) +
//! `MmGetPhysicalAddress`, and a cached `MmMapIoSpace` for MMIO.
//!
//! BAR-mapping lifetime: the `Hal` contract has NO unmap counterpart for
//! `mmio_phys_to_virt` and `PciTransport` never exposes the mapped VAs, so a
//! naive impl would leak a system PTE per BAR region on every StartDevice. We
//! instead keep a process-wide cache keyed by physical address and REUSE
//! mappings: a device's BAR physical addresses are stable across stop/start, so
//! after the first init every lookup hits the cache and no new mappings accrue.
//! The whole cache is released in [`WdkHal::unmap_all`] from `DxgkDdiUnload`.
//!
//! CAVEAT: the `Hal` contract has no failure channel for `dma_alloc` /
//! `mmio_phys_to_virt`. On failure we log + return a dangling pointer; a BAR-map
//! failure then faults *inside* `PciTransport::new` (virtio-drivers dereferences
//! the config region) rather than surfacing as a clean StartDevice error code.
//! A guest that cannot get a few contiguous pages / map a BAR at init is already
//! lost, so this is acceptable.

use core::cell::UnsafeCell;
use core::ptr::NonNull;

use virtio_drivers::{BufferDirection, Hal, PhysAddr};

/// An owned, physically-contiguous, page-aligned DMA buffer.
///
/// Wraps `WdkHal::dma_alloc`/`dma_dealloc` with RAII so command paths can stage
/// a payload (e.g. a Venus command stream copied out of the escape buffer) into
/// device-visible contiguous memory. Because the backing memory is contiguous,
/// a single `Hal::share` (identity, no IOMMU) yields one descriptor for the
/// whole buffer.
///
/// IRQL: both `new` (MmAllocateContiguousMemory) and `drop`
/// (MmFreeContiguousMemory) require PASSIVE_LEVEL — allocate/free a `DmaBuffer`
/// outside any spinlock, never from the DPC/ISR path.
pub struct DmaBuffer {
    pa: PhysAddr,
    ptr: NonNull<u8>,
    pages: usize,
    len: usize,
}

impl DmaBuffer {
    /// Allocate a zeroed contiguous buffer of at least `len` bytes. Returns
    /// `None` on allocation failure or `len == 0`.
    pub fn new(len: usize) -> Option<Self> {
        if len == 0 {
            return None;
        }
        let pages = len.div_ceil(PAGE_SIZE);
        let (pa, ptr) = WdkHal::dma_alloc(pages, BufferDirection::Both);
        if pa == 0 {
            return None;
        }
        Some(Self {
            pa,
            ptr,
            pages,
            len,
        })
    }

    /// The buffer as a byte slice of its requested length.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` owns `pages * PAGE_SIZE >= len` valid bytes for our lifetime.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// The buffer as a mutable byte slice of its requested length.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above; `&mut self` guarantees exclusive access.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for DmaBuffer {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`pages` came from `WdkHal::dma_alloc` in `new` and are
        // freed exactly once. PASSIVE_LEVEL (see the type-level IRQL note).
        unsafe { WdkHal::dma_dealloc(self.pa, self.ptr, self.pages) };
    }
}
use wdk_sys::ntddk::{
    KeAcquireSpinLockRaiseToDpc, KeReleaseSpinLock, MmAllocateContiguousMemory,
    MmFreeContiguousMemory, MmGetPhysicalAddress, MmMapIoSpace, MmUnmapIoSpace,
};
use wdk_sys::{_MEMORY_CACHING_TYPE, KSPIN_LOCK, PHYSICAL_ADDRESS};

const PAGE_SIZE: usize = 4096;
/// Distinct BAR sub-regions a virtio device maps (common/notify/ISR/device cfg),
/// with headroom. A single virtio-gpu uses ≤ 5.
const MAX_MMIO: usize = 16;

/// One cached BAR MMIO mapping.
#[derive(Clone, Copy)]
struct Mapping {
    paddr: usize,
    va: usize,
    size: usize,
}

/// Process-wide cache of BAR MMIO mappings, keyed by physical address. See the
/// module docs for why we cache+reuse rather than map-per-init.
struct MmioCache {
    /// `0` is the initialized + unlocked state of a `KSPIN_LOCK`, so the static
    /// needs no explicit `KeInitializeSpinLock`.
    lock: UnsafeCell<KSPIN_LOCK>,
    entries: UnsafeCell<[Option<Mapping>; MAX_MMIO]>,
}

// SAFETY: every access to `entries` is serialized by `lock` (a kernel spinlock).
// The lock is only acquired from PASSIVE-level call sites (StartDevice init via
// mmio_phys_to_virt; DxgkDdiUnload via unmap_all) and raises to DISPATCH_LEVEL
// for the brief critical section; the Mm* map/unmap calls run outside the lock.
// `Mapping` is Copy/POD.
unsafe impl Sync for MmioCache {}

static MMIO_CACHE: MmioCache = MmioCache {
    lock: UnsafeCell::new(0),
    entries: UnsafeCell::new([None; MAX_MMIO]),
};

/// Zero-sized `Hal` type parameter for the virtio transport.
pub struct WdkHal;

impl WdkHal {
    /// Release every cached BAR mapping. Call exactly once, from
    /// `DxgkDdiUnload`, after all devices have been removed (so nothing still
    /// references the mappings).
    pub fn unmap_all() {
        let lock = MMIO_CACHE.lock.get();
        // SAFETY: spinlock-guarded; swap the table out under the lock, then
        // unmap each entry at PASSIVE_LEVEL outside the lock.
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(lock) };
        let taken = unsafe { core::mem::replace(&mut *MMIO_CACHE.entries.get(), [None; MAX_MMIO]) };
        unsafe { KeReleaseSpinLock(lock, irql) };
        for m in taken.iter().flatten() {
            // SAFETY: `va` was returned by `MmMapIoSpace` in `mmio_phys_to_virt`.
            unsafe { MmUnmapIoSpace(m.va as *mut _, m.size as u64) };
        }
    }
}

// SAFETY: the implementations below uphold the `Hal` contract — `dma_alloc`
// returns page-aligned, zeroed, physically-contiguous non-paged memory whose
// physical address is reported for the device; `share`/`unshare` are identity
// (no IOMMU/bounce in this guest); `mmio_phys_to_virt` maps a real BAR region.
unsafe impl Hal for WdkHal {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        let bytes = pages * PAGE_SIZE;
        // Permit DMA anywhere in the 64-bit physical address space.
        let mut highest: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
        highest.QuadPart = i64::MAX;
        // SAFETY: PASSIVE_LEVEL; allocates `bytes` of physically-contiguous
        // non-paged memory (page-aligned), or null on failure.
        let va = unsafe { MmAllocateContiguousMemory(bytes as u64, highest) };
        match NonNull::new(va as *mut u8) {
            Some(p) => {
                // SAFETY: `p` owns `bytes` freshly-allocated bytes.
                unsafe { core::ptr::write_bytes(p.as_ptr(), 0, bytes) };
                // SAFETY: `va` is a valid non-paged kernel address.
                let phys = unsafe { MmGetPhysicalAddress(va).QuadPart };
                (phys as PhysAddr, p)
            }
            None => {
                crate::kmsg(c"Helios: virtio dma_alloc FAILED\n");
                (0, NonNull::dangling())
            }
        }
    }

    unsafe fn dma_dealloc(_paddr: PhysAddr, vaddr: NonNull<u8>, _pages: usize) -> i32 {
        // SAFETY: `vaddr` was returned by `dma_alloc`'s MmAllocateContiguousMemory.
        unsafe { MmFreeContiguousMemory(vaddr.as_ptr() as *mut _) };
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
        // Physical addresses fit in usize on x64; cache + compare as usize.
        let paddr = paddr as usize;
        let lock = MMIO_CACHE.lock.get();

        // Fast path: already mapped this BAR region?
        // SAFETY: brief spinlock-guarded read of the cache.
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(lock) };
        let hit = unsafe { &*MMIO_CACHE.entries.get() }
            .iter()
            .flatten()
            .find(|m| m.paddr == paddr && m.size >= size)
            .map(|m| m.va);
        unsafe { KeReleaseSpinLock(lock, irql) };
        if let Some(va) = hit {
            return NonNull::new(va as *mut u8).unwrap_or(NonNull::dangling());
        }

        // Miss: map at PASSIVE_LEVEL (MmMapIoSpace requires PASSIVE, so no lock
        // is held here).
        let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
        pa.QuadPart = paddr as i64;
        // SAFETY: maps a device BAR region; non-cached, as required for MMIO.
        let va = unsafe { MmMapIoSpace(pa, size as u64, _MEMORY_CACHING_TYPE::MmNonCached) };
        let mapped = match NonNull::new(va as *mut u8) {
            Some(p) => p,
            None => {
                crate::kmsg(c"Helios: virtio MmMapIoSpace FAILED\n");
                return NonNull::dangling();
            }
        };

        // Insert, double-checking for a concurrent map of the same region.
        // SAFETY: spinlock-guarded mutation of the cache.
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(lock) };
        let entries = unsafe { &mut *MMIO_CACHE.entries.get() };
        if let Some(va) = entries
            .iter()
            .flatten()
            .find(|m| m.paddr == paddr && m.size >= size)
            .map(|m| m.va)
        {
            // Lost the race — another thread mapped it. Drop our duplicate.
            unsafe { KeReleaseSpinLock(lock, irql) };
            unsafe { MmUnmapIoSpace(mapped.as_ptr() as *mut _, size as u64) };
            return NonNull::new(va as *mut u8).unwrap_or(NonNull::dangling());
        }
        let full = if let Some(slot) = entries.iter_mut().find(|e| e.is_none()) {
            *slot = Some(Mapping {
                paddr,
                va: mapped.as_ptr() as usize,
                size,
            });
            false
        } else {
            true
        };
        unsafe { KeReleaseSpinLock(lock, irql) };
        if full {
            // Not expected for one virtio device (≤5 regions, 16 slots). The
            // mapping stays valid + usable, it just won't be reclaimed by
            // unmap_all. Logged after releasing the lock.
            crate::kmsg(c"Helios: virtio MMIO cache full\n");
        }
        mapped
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> PhysAddr {
        // No IOMMU/bounce buffer: the device DMAs guest-physical memory directly.
        // Buffers handed to the queue are always `dma_alloc`'d (contiguous), so a
        // single physical base is valid for the whole buffer.
        // SAFETY: `buffer` points to valid kernel memory for the duration.
        let phys = unsafe { MmGetPhysicalAddress(buffer.as_ptr() as *mut _).QuadPart };
        phys as PhysAddr
    }

    unsafe fn unshare(_paddr: PhysAddr, _buffer: NonNull<[u8]>, _direction: BufferDirection) {
        // Nothing to revoke without an IOMMU.
    }
}
