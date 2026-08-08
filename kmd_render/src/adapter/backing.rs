//! Owned system-memory backing leases for BAR allocations.
//!
//! A paging transfer lends the driver its MDL/PTE mappings only for that
//! operation. Present can happen later, so retaining numerical PFNs is not a
//! lifetime contract. Every range stored here owns a second MDL acquired with
//! `MmProbeAndLockPages`; Windows therefore keeps those pages locked until the
//! range is paged back in, replaced, discarded, or destroyed.

use alloc::vec::Vec;
use core::ffi::c_void;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};

use wdk_sys::{PMDL, PVOID};

use crate::irql::PassiveLevel;
use crate::sync::FallibleArc;

/// Hard ceiling for independently locked system backing on one adapter.
///
/// These leases exist only for KMD standard surfaces that VidMm evicted from
/// Helios's BAR and that Present must continue updating. Locking without a
/// ceiling could turn arbitrary eviction traffic into unbounded nonpageable
/// memory. At the ceiling the paging operation fails and VidMm retains/retries
/// the allocation instead of the driver losing content or pinning more RAM.
const MAX_PINNED_SYSTEM_BACKING_BYTES: u64 = 512 * 1024 * 1024;
/// Bound interval fragmentation as well as pinned bytes. A range is one
/// physically-contiguous run in the virtual-transfer path; ordinary
/// allocations normally need one or a handful, while 4096 still covers a
/// maximally fragmented 16-MiB 4-KiB mapping.
pub(crate) const MAX_SYSTEM_BACKING_RANGES: usize = 4096;

struct PinnedBackingBudget {
    bytes: AtomicU64,
}

impl PinnedBackingBudget {
    fn try_reserve(&self, bytes: u64) -> bool {
        let mut current = self.bytes.load(Ordering::Relaxed);
        loop {
            let Some(next) = current.checked_add(bytes) else {
                return false;
            };
            if next > MAX_PINNED_SYSTEM_BACKING_BYTES {
                return false;
            }
            match self.bytes.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    fn release(&self, bytes: u64) {
        self.bytes.fetch_sub(bytes, Ordering::AcqRel);
    }
}

extern "C" {
    /// `MmProbeAndLockPages` raises. The C shim converts that raise to NULL,
    /// maps the newly locked MDL once in KernelMode, and returns both values.
    fn helios_lock_system_buffer_seh(
        virtual_address: PVOID,
        length: u32,
        mapped_system_address: *mut PVOID,
    ) -> PMDL;
    /// Releases the persistent system mapping, unlocks the pages, and frees the
    /// owned MDL. Must run at PASSIVE_LEVEL for pageable backing.
    fn helios_unlock_system_buffer(mdl: PMDL);
}

/// An independent memory-manager lease on one exact system-backing byte range.
pub(crate) struct SystemBackingLease {
    mdl: PMDL,
    system_va: NonNull<u8>,
    size: u64,
    charged_bytes: u64,
    budget: FallibleArc<PinnedBackingBudget>,
}

// The MDL and its system mapping are kernel-global objects. The only mutation
// route requires the backing table's content guard, and final release is
// constrained to PASSIVE callers by the table's API contract.
unsafe impl Send for SystemBackingLease {}
unsafe impl Sync for SystemBackingLease {}

impl SystemBackingLease {
    /// Lock and persistently map `size` ordinary-RAM bytes beginning at `va`.
    ///
    /// # Safety
    /// `va..va+size` must be a valid system mapping of ordinary RAM for this
    /// call. The caller must be at PASSIVE_LEVEL and may hold no spinlock.
    unsafe fn acquire(
        _passive: PassiveLevel,
        va: *mut u8,
        size: u64,
        budget: &FallibleArc<PinnedBackingBudget>,
    ) -> Option<Self> {
        let length = u32::try_from(size).ok().filter(|length| *length != 0)?;
        if va.is_null() {
            return None;
        }
        let page_offset = (va as usize & 0xFFF) as u64;
        let charged_bytes = page_offset
            .checked_add(size)?
            .checked_add(0xFFF)?
            .checked_div(4096)?
            .checked_mul(4096)?;
        let budget = budget.try_clone()?;
        if !budget.try_reserve(charged_bytes) {
            return None;
        }
        let mut system_va: PVOID = core::ptr::null_mut();
        // SAFETY: the byte range and PASSIVE obligation are this function's
        // contract; the shim catches the only raising operation.
        let mdl =
            unsafe { helios_lock_system_buffer_seh(va.cast::<c_void>(), length, &mut system_va) };
        if mdl.is_null() {
            budget.release(charged_bytes);
            return None;
        }
        let Some(system_va) = NonNull::new(system_va.cast::<u8>()) else {
            // The shim promises these succeed/fail together, but retain a
            // defensive unwind if that ABI is ever changed independently.
            unsafe { helios_unlock_system_buffer(mdl) };
            budget.release(charged_bytes);
            return None;
        };
        Some(Self {
            mdl,
            system_va,
            size,
            charged_bytes,
            budget,
        })
    }

    /// Copy a subrange of this lease from the corresponding blob bytes.
    unsafe fn copy_from(&self, lease_offset: u64, size: u64, source: *const u8) -> bool {
        let Some(end) = lease_offset.checked_add(size) else {
            return false;
        };
        let Ok(lease_offset) = usize::try_from(lease_offset) else {
            return false;
        };
        let Ok(size) = usize::try_from(size) else {
            return false;
        };
        if source.is_null() || end > self.size {
            return false;
        }
        // SAFETY: the checked range lies inside the persistent MDL mapping;
        // caller proves `source` covers `size` bytes and owns the content lock.
        unsafe {
            core::ptr::copy_nonoverlapping(source, self.system_va.as_ptr().add(lease_offset), size)
        };
        true
    }
}

impl Drop for SystemBackingLease {
    fn drop(&mut self) {
        // SAFETY: `mdl` came from helios_lock_system_buffer_seh and is released
        // exactly once. Table snapshots are dropped only by PASSIVE callbacks.
        unsafe { helios_unlock_system_buffer(self.mdl) };
        self.budget.release(self.charged_bytes);
    }
}

/// One allocation-relative range backed by an owned system-memory lease.
///
/// Each stored range owns an exact-size lease. A partial page-in replaces the
/// surviving pieces with newly probed exact leases before releasing the old
/// whole-range lease, so removed pages become pageable immediately on commit.
pub(crate) struct SystemBackingRange {
    blob_offset: u64,
    size: u64,
    lease: FallibleArc<SystemBackingLease>,
}

impl SystemBackingRange {
    /// Acquire an owned lease before the paging operation releases its view.
    ///
    /// # Safety
    /// Same byte-range and PASSIVE contract as [`SystemBackingLease::acquire`].
    unsafe fn acquire(
        passive: PassiveLevel,
        blob_offset: u64,
        size: u64,
        system_va: *mut u8,
        budget: &FallibleArc<PinnedBackingBudget>,
    ) -> Option<Self> {
        blob_offset.checked_add(size)?;
        let lease = unsafe { SystemBackingLease::acquire(passive, system_va, size, budget) }?;
        Some(Self {
            blob_offset,
            size,
            lease: FallibleArc::try_new(lease).ok()?,
        })
    }

    fn end(&self) -> Option<u64> {
        self.blob_offset.checked_add(self.size)
    }

    fn try_clone(&self) -> Option<Self> {
        Some(Self {
            blob_offset: self.blob_offset,
            size: self.size,
            lease: self.lease.try_clone()?,
        })
    }

    fn relock_slice(&self, passive: PassiveLevel, blob_offset: u64, size: u64) -> Option<Self> {
        let end = blob_offset.checked_add(size)?;
        if size == 0 || blob_offset < self.blob_offset || end > self.end()? {
            return None;
        }
        let lease_offset = blob_offset - self.blob_offset;
        let lease_offset = usize::try_from(lease_offset).ok()?;
        // Take a fresh, exact MDL lease for the surviving interval. The old
        // whole-range lease is released after the table transaction commits,
        // so a partial page-in really unlocks the removed pages instead of an
        // shared slice accidentally keeping the entire old MDL pinned.
        unsafe {
            Self::acquire(
                passive,
                blob_offset,
                size,
                self.lease.system_va.as_ptr().add(lease_offset),
                &self.lease.budget,
            )
        }
    }

    /// Copy this allocation-relative range out of `blob`.
    unsafe fn copy_from_blob(&self, blob: *const u8, blob_size: u64) -> bool {
        let Some(end) = self.end() else {
            return false;
        };
        let Ok(blob_offset) = usize::try_from(self.blob_offset) else {
            return false;
        };
        if end > blob_size || blob.is_null() {
            return false;
        }
        // SAFETY: the blob range was checked and the caller holds the backing
        // content guard for the entire snapshot copy.
        unsafe { self.lease.copy_from(0, self.size, blob.add(blob_offset)) }
    }

    unsafe fn copy_blob_intersection(
        &self,
        blob: *const u8,
        blob_size: u64,
        update_offset: u64,
        update_size: u64,
    ) -> bool {
        let Some(range_end) = self.end() else {
            return false;
        };
        let Some(update_end) = update_offset.checked_add(update_size) else {
            return false;
        };
        let start = self.blob_offset.max(update_offset);
        let end = range_end.min(update_end);
        if start >= end {
            return true;
        }
        let size = end - start;
        if end > blob_size || blob.is_null() {
            return false;
        }
        let lease_offset = start - self.blob_offset;
        let Ok(start) = usize::try_from(start) else {
            return false;
        };
        // SAFETY: the intersection lies in both the checked blob and lease
        // ranges; the caller owns the table content transaction.
        unsafe { self.lease.copy_from(lease_offset, size, blob.add(start)) }
    }
}

/// Immutable, allocation-wide snapshot used by one Present mirror.
pub(crate) struct SystemBackingSnapshot<'guard> {
    ranges: FallibleArc<Vec<SystemBackingRange>>,
    // A snapshot may hold the final MDL owner, whose Drop invokes PASSIVE-only
    // memory-manager APIs. Tie it to the serialized PASSIVE transaction so it
    // cannot escape into a caller that later drops it at arbitrary IRQL.
    _guard: PhantomData<&'guard ()>,
}

impl SystemBackingSnapshot<'_> {
    pub(crate) unsafe fn copy_from_blob(&self, blob: *const u8, blob_size: u64) -> bool {
        self.ranges
            .iter()
            .all(|range| unsafe { range.copy_from_blob(blob, blob_size) })
    }

    pub(crate) unsafe fn copy_blob_range(
        &self,
        blob: *const u8,
        blob_size: u64,
        update_offset: u64,
        update_size: u64,
    ) -> bool {
        self.ranges.iter().all(|range| unsafe {
            range.copy_blob_intersection(blob, blob_size, update_offset, update_size)
        })
    }
}

struct SystemBackingEntry {
    resource_id: u32,
    ranges: FallibleArc<Vec<SystemBackingRange>>,
}

/// Per-adapter resource-id -> owned system-backing associations.
pub(crate) struct SystemBackingTable {
    /// Serialize whole software content transactions. This is a sleeping lock:
    /// multi-megabyte copies never run under a spinlock or raised IRQL.
    content_mutex: Option<crate::sync::PassiveMutex>,
    entries: crate::sync::SpinLock<crate::sync::FixedVec<SystemBackingEntry>>,
    /// Shared by every lease so the bound applies before probing, including
    /// temporary relocks during a partial-range transaction.
    budget: Option<FallibleArc<PinnedBackingBudget>>,
}

impl SystemBackingTable {
    const MAX_ALLOCATIONS: usize = 128;

    pub fn new(passive: PassiveLevel) -> Self {
        Self {
            content_mutex: crate::sync::PassiveMutex::try_new(passive),
            entries: crate::sync::SpinLock::new(crate::sync::FixedVec::with_max(
                Self::MAX_ALLOCATIONS,
            )),
            budget: FallibleArc::try_new(PinnedBackingBudget {
                bytes: AtomicU64::new(0),
            })
            .ok(),
        }
    }

    /// Serialize a complete backing-content transaction at PASSIVE_LEVEL.
    pub fn serialize(&self, passive: PassiveLevel) -> Option<SystemBackingGuard<'_>> {
        Some(SystemBackingGuard {
            table: self,
            _mutex: self.content_mutex.as_ref()?.lock(passive)?,
        })
    }
}

/// Proof that the caller owns this exact table's PASSIVE content transaction.
pub(crate) struct SystemBackingGuard<'a> {
    table: &'a SystemBackingTable,
    _mutex: crate::sync::PassiveMutexGuard<'a>,
}

impl SystemBackingGuard<'_> {
    pub(crate) unsafe fn acquire_range(
        &self,
        passive: PassiveLevel,
        blob_offset: u64,
        size: u64,
        system_va: *mut u8,
    ) -> Option<SystemBackingRange> {
        unsafe {
            SystemBackingRange::acquire(
                passive,
                blob_offset,
                size,
                system_va,
                self.table.budget.as_ref()?,
            )
        }
    }

    pub(crate) fn snapshot(&self, resource_id: u32) -> Option<SystemBackingSnapshot<'_>> {
        self.table
            .entries
            .lock()
            .as_slice()
            .iter()
            .find(|entry| entry.resource_id == resource_id)
            .and_then(|entry| {
                Some(SystemBackingSnapshot {
                    ranges: entry.ranges.try_clone()?,
                    _guard: PhantomData,
                })
            })
    }

    /// Replace exactly `[blob_offset, blob_offset + size)` with the supplied
    /// leases. Existing ranges outside it survive, including both halves of a
    /// range split by a partial transfer.
    pub(crate) fn replace_range(
        &self,
        passive: PassiveLevel,
        resource_id: u32,
        blob_offset: u64,
        size: u64,
        mut replacements: Vec<SystemBackingRange>,
    ) -> bool {
        let Some(replace_end) = blob_offset.checked_add(size) else {
            return false;
        };
        if size == 0 {
            return false;
        }
        replacements.sort_unstable_by_key(|range| range.blob_offset);
        let mut cursor = blob_offset;
        for range in &replacements {
            if range.size == 0 || range.blob_offset != cursor {
                return false;
            }
            let Some(end) = range.end() else {
                return false;
            };
            cursor = end;
        }
        if cursor != replace_end {
            return false;
        }

        let old = self.snapshot(resource_id);
        let old_len = old.as_ref().map_or(0, |snapshot| snapshot.ranges.len());
        let Some(capacity) = old_len
            .checked_add(replacements.len())
            .and_then(|n| n.checked_add(2))
        else {
            return false;
        };
        let mut next = Vec::new();
        if next.try_reserve_exact(capacity).is_err() {
            return false;
        }
        if let Some(old) = old {
            for range in old.ranges.iter() {
                let Some(range_end) = range.end() else {
                    return false;
                };
                if range_end <= blob_offset || range.blob_offset >= replace_end {
                    let Some(range) = range.try_clone() else {
                        return false;
                    };
                    next.push(range);
                    continue;
                }
                if range.blob_offset < blob_offset {
                    let Some(left) = range.relock_slice(
                        passive,
                        range.blob_offset,
                        blob_offset - range.blob_offset,
                    ) else {
                        return false;
                    };
                    next.push(left);
                }
                if range_end > replace_end {
                    let Some(right) =
                        range.relock_slice(passive, replace_end, range_end - replace_end)
                    else {
                        return false;
                    };
                    next.push(right);
                }
            }
        }
        next.append(&mut replacements);
        next.sort_unstable_by_key(|range| range.blob_offset);
        self.store(resource_id, next)
    }

    /// Remove only the named allocation-relative range after a partial page-in.
    pub(crate) fn remove_range(
        &self,
        passive: PassiveLevel,
        resource_id: u32,
        blob_offset: u64,
        size: u64,
    ) -> bool {
        let Some(remove_end) = blob_offset.checked_add(size) else {
            return false;
        };
        if size == 0 {
            return true;
        }
        let Some(old) = self.snapshot(resource_id) else {
            return true;
        };
        let Some(capacity) = old.ranges.len().checked_add(1) else {
            return false;
        };
        let mut next = Vec::new();
        if next.try_reserve_exact(capacity).is_err() {
            return false;
        }
        for range in old.ranges.iter() {
            let Some(range_end) = range.end() else {
                return false;
            };
            if range_end <= blob_offset || range.blob_offset >= remove_end {
                let Some(range) = range.try_clone() else {
                    return false;
                };
                next.push(range);
                continue;
            }
            if range.blob_offset < blob_offset {
                let Some(left) =
                    range.relock_slice(passive, range.blob_offset, blob_offset - range.blob_offset)
                else {
                    return false;
                };
                next.push(left);
            }
            if range_end > remove_end {
                let Some(right) = range.relock_slice(passive, remove_end, range_end - remove_end)
                else {
                    return false;
                };
                next.push(right);
            }
        }
        self.store(resource_id, next)
    }

    /// Remove every system-backing range for one allocation.
    pub(crate) fn remove(&self, resource_id: u32) {
        let removed = {
            let mut entries = self.table.entries.lock();
            entries
                .as_slice()
                .iter()
                .position(|entry| entry.resource_id == resource_id)
                .map(|index| entries.swap_remove(index))
        };
        // Releasing the last shared owner may unmap and unlock pages; do it after the
        // spinlock has restored the caller to PASSIVE_LEVEL.
        drop(removed);
    }

    fn store(&self, resource_id: u32, ranges: Vec<SystemBackingRange>) -> bool {
        if ranges.is_empty() {
            self.remove(resource_id);
            return true;
        }
        if ranges.len() > MAX_SYSTEM_BACKING_RANGES {
            return false;
        }
        let Some(new_bytes) = ranges.iter().try_fold(0u64, |sum, range| {
            sum.checked_add(range.lease.charged_bytes)
        }) else {
            return false;
        };
        for pair in ranges.windows(2) {
            if pair[0].end().is_none_or(|end| end > pair[1].blob_offset) {
                return false;
            }
        }
        if new_bytes > MAX_PINNED_SYSTEM_BACKING_BYTES {
            return false;
        }
        let Ok(ranges) = FallibleArc::try_new(ranges) else {
            return false;
        };
        let new_entry = SystemBackingEntry {
            resource_id,
            ranges,
        };
        let mut old = None;
        let mut rejected = None;
        let success = {
            let mut entries = self.table.entries.lock();
            match entries
                .as_slice()
                .iter()
                .position(|entry| entry.resource_id == resource_id)
            {
                Some(index) => {
                    old = Some(entries.replace_at(index, new_entry));
                    true
                }
                None => match entries.try_push(new_entry) {
                    Ok(()) => true,
                    Err(entry) => {
                        rejected = Some(entry);
                        false
                    }
                },
            }
        };
        drop(old);
        drop(rejected);
        success
    }
}
