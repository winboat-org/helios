//! Host-visible blob → user-VA mapping registry (Phase 4c teardown bookkeeping).
//!
//! `IOCTL_HELIOS_MAP_BLOB` maps a host-visible blob's pages into the calling user
//! process with `MmMapLockedPagesSpecifyCache(UserMode)` (ioctl.rs). The resulting
//! `(user_va, MDL)` pair MUST be unmapped in that same process's context before the
//! process tears down, or the kernel bugchecks `0x76 PROCESS_HAS_LOCKED_PAGES`. So
//! each successful map is recorded here, and `EvtFileCleanup` (which runs in the
//! closing process at PASSIVE_LEVEL) drains the table, unmapping each entry.
//!
//! This lives in [`crate::adapter::AdapterContext`] — NOT in `VirtioGpu` — so the
//! teardown is independent of the virtio transport: the MDLs describe BAR I/O
//! pages plus a user VA and remain valid (and MUST still be torn down) even if
//! `EvtDeviceReleaseHardware` already dropped the transport. Guarded by its own
//! spinlock so the PASSIVE-level record / drain paths are serialized without the
//! virtio lock.
//!
//! Each entry is **tagged with the owning `WDFFILEOBJECT`** (as an opaque `usize`).
//! `EvtFileCleanup` runs per-file-object — one fires for *each* closed handle, not
//! only the last — and a user mapping is valid only in the process that created it,
//! so cleanup must unmap exactly that file object's mappings ([`MappingTable::drain_for`]),
//! never another open handle's (which would unmap a foreign process's VA → 0x76 /
//! corruption).
//!
//! The backing `Vec` is reserved to [`MAX_MAPPINGS`] once at construction
//! (PASSIVE_LEVEL); `insert` only `push`es within that reserved capacity, so it
//! never reallocates and is safe to call under the spinlock (DISPATCH_LEVEL).
//! (Same heap-reserve discipline as `virtio::gpu::BlobTable` — see the 0x7F
//! kernel-stack-overflow lesson; here the array would be heap-resident anyway,
//! but the no-realloc-under-lock invariant still matters.)

use core::sync::atomic::{AtomicU32, Ordering};

/// Maximum concurrently-mapped host-visible blobs per ADAPTER (the table is
/// adapter-global: dwm + WUDFHost + every game share it). Matches MAX_BLOBS —
/// every live mappable blob may legally be mapped. The original 256 (sized to
/// the pre-2026-07-03 MAX_BLOBS) was the 2026-07-06 Doom level-load fatal:
/// the desktop held ~223 mappings, the load burst past the remaining ~33 →
/// MAP_BLOB → STATUS_INSUFFICIENT_RESOURCES → vkMapMemory returned no address
/// → idTech "Cannot map buffer with usage BU_STATIC" FatalError (probe-proven:
/// map-and-hold refused at exactly held=33 with 0xC000009A).
const MAX_MAPPINGS: usize = 8192;

/// `insert` refusals because the table was at capacity (each one is a failed
/// user MAP_BLOB — loud-failure rule; reported via QUERY_STATS v2).
pub static MAPPING_FULL_REJECTS: AtomicU32 = AtomicU32::new(0);
/// High-water of live mappings since driver start.
pub static MAPPINGS_HIGH_WATER: AtomicU32 = AtomicU32::new(0);
/// `MAX_MAPPINGS` for QUERY_STATS reporting.
pub const MAX_MAPPINGS_CAP: u32 = MAX_MAPPINGS as u32;

/// Outcome of [`MappingTable::insert_unique`].
///
/// `Duplicate` and `Full` are both "the caller must unmap the view it just
/// created", but they are different failures and the caller reports them
/// differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertResult {
    Inserted,
    /// `(owner, resource_id)` already had a live mapping. Authoritative: this
    /// answer was produced under the same lock acquisition as the push.
    Duplicate,
    /// The table is at [`MAX_MAPPINGS`].
    Full,
}

/// One recorded user-space blob mapping.
#[derive(Clone, Copy)]
struct Mapping {
    /// The owning `WDFFILEOBJECT` (as an opaque `usize`) — the handle whose
    /// `EvtFileCleanup` must unmap this entry, in the process that created it.
    owner: usize,
    /// The virtio-gpu resource id mapped (for the double-map guard / diagnostics).
    resource_id: u32,
    /// User-mode VA returned by `MmMapLockedPagesSpecifyCache`.
    user_va: u64,
    /// `*mut MDL` (as `usize`; never 0 for a live entry) describing the mapped
    /// host-visible pages. Stored as `usize` so `Mapping` is trivially `Copy`/POD
    /// and the table needs no `unsafe impl Send` beyond the enclosing context's.
    mdl: usize,
}

/// Registry of live host-visible blob mappings, guarded by its own spinlock.
pub struct MappingTable {
    /// Live mappings. Capacity reserved to `MAX_MAPPINGS` at construction;
    /// `FixedVec` has no growth path, so nothing here can allocate under the
    /// spinlock.
    entries: crate::sync::SpinLock<crate::sync::FixedVec<Mapping>>,
}

impl MappingTable {
    /// Reserve the backing buffer up front (PASSIVE_LEVEL). After this, `insert`
    /// up to `MAX_MAPPINGS` entries performs no allocation.
    pub fn new() -> Self {
        Self {
            entries: crate::sync::SpinLock::new(crate::sync::FixedVec::with_max(MAX_MAPPINGS)),
        }
    }

    /// True if `resource_id` already has a live mapping for `owner`. Used by
    /// `MAP_BLOB` to reject a duplicate map from the same D3DKMT device handle.
    pub fn contains(&self, owner: usize, resource_id: u32) -> bool {
        self.entries
            .lock()
            .as_slice()
            .iter()
            .any(|m| m.owner == owner && m.resource_id == resource_id)
    }

    /// Record a freshly-created mapping owned by `owner`, refusing a duplicate
    /// under the SAME lock acquisition that inserts.
    ///
    /// The duplicate-map guard used to be check-then-act: `contains` (lock 1),
    /// then `map_blob_prepare` + MDL + user map, then `insert` (lock 2).
    /// `DxgkDdiEscape` is not serialised by dxgkrnl, so two threads on one
    /// device handle could both pass `contains` and both `insert`. The
    /// consequence is NOT a moved window offset — `map_blob_prepare` is
    /// idempotent and `blob_map_begin` never re-places a mapped blob, so both
    /// threads get the same offset — it is a DUPLICATE REGISTRATION: two MDLs
    /// and two user VAs over one blob, two entries with the same
    /// `(owner, resource_id)`. A later `RELEASE_BLOB` then pops one via
    /// `take_for_resource` while `release_blob_for_owner` tears down the host
    /// window mapping, leaving the second user VA live over a window offset that
    /// is no longer backed — silent content loss until `DestroyDevice` reclaims
    /// it.
    ///
    /// The scan and the push are one critical section now, so the commit-time
    /// answer is authoritative. The `push` stays within the reserved capacity,
    /// so it makes no allocator call and is safe under the spinlock.
    pub fn insert_unique(
        &self,
        owner: usize,
        resource_id: u32,
        user_va: u64,
        mdl: usize,
    ) -> InsertResult {
        let mut entries = self.entries.lock();
        if entries
            .as_slice()
            .iter()
            .any(|m| m.owner == owner && m.resource_id == resource_id)
        {
            return InsertResult::Duplicate;
        }
        if !entries.push(Mapping {
            owner,
            resource_id,
            user_va,
            mdl,
        }) {
            MAPPING_FULL_REJECTS.fetch_add(1, Ordering::Relaxed);
            return InsertResult::Full;
        }
        let n = entries.len() as u32;
        if MAPPINGS_HIGH_WATER.load(Ordering::Relaxed) < n {
            MAPPINGS_HIGH_WATER.store(n, Ordering::Relaxed);
        }
        InsertResult::Inserted
        // The two early returns above are the point of the guard: each one used
        // to be an assignment into `result` followed by a shared
        // KeReleaseSpinLock, because an early `return` there would have leaked
        // the lock. Release is now a Drop obligation.
    }

    /// Current live-mapping count (QUERY_STATS v2).
    pub fn live(&self) -> u32 {
        self.entries.lock().len() as u32
    }

    /// Pop up to `out.len()` of `owner`'s mappings per acquisition, returning how
    /// many were written.
    ///
    /// The one-at-a-time `take_one_for` this replaces took and released the
    /// spinlock once per entry, so
    /// draining a process holding N mappings costs N acquisitions and O(N^2)
    /// comparisons — and MAX_MAPPINGS is 8192 because a DOOM level load really
    /// does hold thousands. The caller still unmaps OUTSIDE the lock, which is
    /// mandatory (`MmUnmapLockedPages` is PASSIVE-only, the lock raises to
    /// DISPATCH); batching only changes how many entries one acquisition
    /// harvests.
    ///
    /// `out` is a caller-provided stack array so nothing allocates here, and it
    /// is deliberately small — a large on-stack collection of ALL entries is
    /// what the one-at-a-time version was avoiding, and that reason still holds.
    pub fn drain_for(&self, owner: usize, out: &mut [(u64, usize)]) -> usize {
        if out.is_empty() {
            return 0;
        }
        let mut entries = self.entries.lock();
        let mut n = 0;
        let mut i = 0;
        while i < entries.len() && n < out.len() {
            if entries.as_slice()[i].owner == owner {
                // swap_remove moves the last element into slot i, so do NOT
                // advance — it has not been tested yet.
                let m = entries.swap_remove(i);
                out[n] = (m.user_va, m.mdl);
                n += 1;
            } else {
                i += 1;
            }
        }
        n
    }

    /// Pop the mapping for `resource_id` owned by `owner`, if one exists. Used by
    /// explicit BO release while the process is still alive.
    pub fn take_for_resource(&self, owner: usize, resource_id: u32) -> Option<(u64, usize)> {
        let mut entries = self.entries.lock();
        let index = entries
            .as_slice()
            .iter()
            .position(|m| m.owner == owner && m.resource_id == resource_id)?;
        let m = entries.swap_remove(index);
        Some((m.user_va, m.mdl))
    }
}
