//! The paging-transfer system-backing map: exact system-memory pages Windows
//! associates with a BAR allocation through paging TRANSFER requests.
//!
//! Moved verbatim out of `adapter.rs` by T8/R1101.

use alloc::sync::Arc;

/// Exact system-memory backing Windows supplied for one allocation in a paging
/// TRANSFER from the BAR segment to segment 0.
///
/// The physical pages come directly from the locked MDL in the transfer request
/// — locked for THAT OPERATION.
///
/// ⚠ This type does NOT own the frames it names, and nothing in the driver does.
/// `pages` is a snapshot of PFNs; the entry's presence in the table records that
/// WE remember them, not that VidMm still considers them the allocation's
/// backing. `mirror_present_system_backing` re-maps them from `DxgkDdiPresent`,
/// a completely different call from the paging op that supplied them. See the
/// SAFETY note at `copy_blob_system_pages` in `ddi/build_paging_buffer.rs` for
/// the full statement and the owner decision it gates (R715 / k-paging-04).
///
/// The entry is removed on every allocation-teardown exit, so it cannot outlive
/// the allocation — which bounds the damage but does not make the mapping sound.
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
    entries: crate::sync::SpinLock<crate::sync::FixedVec<SystemBackingSnapshot>>,
}

impl SystemBackingTable {
    const MAX_ENTRIES: usize = 128;

    pub fn new() -> Self {
        Self {
            entries: crate::sync::SpinLock::new(crate::sync::FixedVec::with_max(Self::MAX_ENTRIES)),
        }
    }

    pub fn replace(&self, backing: SystemBackingSnapshot) -> bool {
        let mut old = None;
        let success = {
            let mut entries = self.entries.lock();
            let existing = entries
                .as_slice()
                .iter()
                .position(|entry| entry.resource_id == backing.resource_id);
            match existing {
                Some(index) => {
                    old = Some(entries.replace_at(index, backing));
                    true
                }
                // BEHAVIOUR NOTE: the bound is now MAX_ENTRIES, not
                // `Vec::capacity()`. The old check admitted whatever
                // over-allocation the allocator chose above the declared 128,
                // so this can only ever admit FEWER entries — `PgSc`/`PgSe`
                // must be unchanged, and would move if the allocator had been
                // over-allocating.
                None => entries.push(backing),
            }
        };
        // Releasing the old Arc can free pool memory; do that after the guard
        // has dropped us back to the caller's original IRQL.
        drop(old);
        success
    }

    pub fn snapshot(&self, resource_id: u32) -> Option<SystemBackingSnapshot> {
        self.entries
            .lock()
            .as_slice()
            .iter()
            .find(|entry| entry.resource_id == resource_id)
            .cloned()
    }

    pub fn contains(&self, resource_id: u32) -> bool {
        self.entries
            .lock()
            .as_slice()
            .iter()
            .any(|entry| entry.resource_id == resource_id)
    }

    pub fn remove(&self, resource_id: u32) {
        let removed = {
            let mut entries = self.entries.lock();
            entries
                .as_slice()
                .iter()
                .position(|entry| entry.resource_id == resource_id)
                .map(|index| entries.swap_remove(index))
        };
        // Same reason as `replace`: drop the Arc outside the critical section.
        drop(removed);
    }
}
