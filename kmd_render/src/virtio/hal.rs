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
use core::sync::atomic::{AtomicU32, Ordering};

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

    /// Guest-physical base address suitable for virtio descriptor payloads.
    pub fn physical_address(&self) -> u64 {
        self.pa as u64
    }

    /// Page-rounded allocation capacity. Reuse callers may request any logical
    /// length up to this value without another contiguous-memory allocation.
    pub fn capacity(&self) -> usize {
        self.pages * PAGE_SIZE
    }

    /// Prepare a completed buffer for a new command. Returns false when the new
    /// logical length does not fit. Callers overwrite every device-read byte;
    /// the device overwrites every response byte, so no clearing is required.
    pub fn reset(&mut self, len: usize) -> bool {
        if len == 0 || len > self.capacity() {
            return false;
        }
        self.len = len;
        true
    }

    /// A bounds-checked device-visible sub-range, or `None` if it does not fit.
    ///
    /// This is the only constructor of a [`DmaSpan`], which makes it the one
    /// place `offset + len <= self.len` is proved. It used to be re-derived per
    /// enqueue arm as an ad-hoc `t <= meta.as_slice().len()` before four
    /// separate blocks of raw span arithmetic.
    pub fn span(&self, offset: usize, len: usize) -> Option<DmaSpan> {
        if offset.checked_add(len)? > self.len {
            return None;
        }
        Some(DmaSpan {
            // SAFETY: offset <= self.len, just proved, so the result is within
            // the allocation (one-past-the-end is legal to form).
            base: unsafe { self.ptr.as_ptr().add(offset) },
            len,
        })
    }
}

/// A bounds-checked sub-range of a [`DmaBuffer`], produced only by
/// [`DmaBuffer::span`].
///
/// This is the type `Hal::share`'s real precondition is about. Its SAFETY
/// comment establishes only "valid kernel memory", but what the identity
/// mapping actually relies on is that every buffer reaching `add`/`pop_used`
/// is a sub-span of a `DmaBuffer` — i.e. `MmAllocateContiguousMemory`-backed
/// and physically contiguous across its whole length, so one `share` yields one
/// descriptor. Making this the only way to name such a span puts the check at
/// the producer instead of restating it as an assumption at the consumer.
#[derive(Clone, Copy)]
pub struct DmaSpan {
    base: *mut u8,
    len: usize,
}

impl DmaSpan {
    /// The unused second slot of a one-read-span chain. Zero length, so the
    /// dangling base is never dereferenced (`from_raw_parts` with len 0 requires
    /// only a non-null, aligned pointer).
    pub const EMPTY: Self = Self {
        base: NonNull::<u8>::dangling().as_ptr(),
        len: 0,
    };

    /// # Safety
    /// The owning [`DmaBuffer`] must outlive the returned slice, and the device
    /// must not write this span concurrently. Both hold for a device-READ span
    /// of an entry that owns its buffers until `pop_used`.
    pub unsafe fn as_slice<'a>(&self) -> &'a [u8] {
        unsafe { core::slice::from_raw_parts(self.base, self.len) }
    }

    /// # Safety
    /// As [`DmaSpan::as_slice`], except that this IS the span the device writes:
    /// the aliasing between our `&mut` and the device's DMA is inherent to
    /// virtio and cannot be encoded. The entry owns the buffer until `pop_used`
    /// consumes the completion, which is the synchronisation that makes the
    /// read-back well defined.
    pub unsafe fn as_mut_slice<'a>(&self) -> &'a mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.base, self.len) }
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
                DMA_ALLOC_FAILS.fetch_add(1, Ordering::Relaxed);
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
        // The `virtio_drivers::Hal` signature has no failure channel, so the
        // dangling sentinel survives HERE and only here. Every Helios-side
        // caller must go through `try_mmio_map` instead.
        // SAFETY: same contract as this trait method.
        unsafe { try_mmio_map(paddr, size) }.unwrap_or(NonNull::dangling())
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

/// Contiguous-DMA allocation failures. The sibling `gpu.rs` maintains ~41 named
/// counters under the same loud-failure rule; these three were kmsg-only, which
/// is invisible without a kernel debugger attached.
pub(crate) static DMA_ALLOC_FAILS: AtomicU32 = AtomicU32::new(0);
/// MMIO tracking-cache overflows. The mapping stays valid but untracked, so
/// `unmap_all` can never release it: a permanent system-PTE leak, now visible.
pub(crate) static MMIO_CACHE_FULL: AtomicU32 = AtomicU32::new(0);

/// Number of `MmMapIoSpace` failures seen by [`try_mmio_map`]. Reported through
/// the ungated `diag::fault` path by the ISR-register mapping, because on this
/// INTx device a missing ISR ack is not a benign degrade: the line stays
/// asserted and Windows' interrupt-storm detector Code-43s the adapter.
pub(crate) static MMIO_MAP_FAILS: AtomicU32 = AtomicU32::new(0);

/// Map a device MMIO region, or `None` if `MmMapIoSpace` fails. PASSIVE_LEVEL.
///
/// This is the fallible form of [`WdkHal::mmio_phys_to_virt`]. The trait method
/// cannot fail, so it returns `NonNull::dangling()` — address 0x1 — which a
/// caller converting to `usize` cannot distinguish from a real mapping. That is
/// how a failed ISR-region map became a *success* breadcrumb and then a
/// `read_volatile(1)` fault at PASSIVE inside StartDevice, where a documented
/// degrade path (`0x0B00_00E6`, "no ISR cap") already existed.
///
/// # Safety
/// `paddr`/`size` must describe a real device MMIO region. The mapping is
/// non-cached and is cached for the driver's lifetime (reclaimed by `unmap_all`).
pub(crate) unsafe fn try_mmio_map(paddr: PhysAddr, size: usize) -> Option<NonNull<u8>> {
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
        return NonNull::new(va as *mut u8);
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
            MMIO_MAP_FAILS.fetch_add(1, Ordering::Relaxed);
            return None;
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
        return NonNull::new(va as *mut u8);
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
        MMIO_CACHE_FULL.fetch_add(1, Ordering::Relaxed);
    }
    Some(mapped)
}
