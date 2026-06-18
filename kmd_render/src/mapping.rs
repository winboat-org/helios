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
//! so cleanup must unmap exactly that file object's mappings ([`take_one_for`]),
//! never another open handle's (which would unmap a foreign process's VA → 0x76 /
//! corruption).
//!
//! The backing `Vec` is reserved to [`MAX_MAPPINGS`] once at construction
//! (PASSIVE_LEVEL); `insert` only `push`es within that reserved capacity, so it
//! never reallocates and is safe to call under the spinlock (DISPATCH_LEVEL).
//! (Same heap-reserve discipline as `virtio::gpu::BlobTable` — see the 0x7F
//! kernel-stack-overflow lesson; here the array would be heap-resident anyway,
//! but the no-realloc-under-lock invariant still matters.)

use core::cell::UnsafeCell;

use alloc::vec::Vec;

use wdk_sys::ntddk::{KeAcquireSpinLockRaiseToDpc, KeReleaseSpinLock};
use wdk_sys::KSPIN_LOCK;

/// Maximum concurrently-mapped host-visible blobs per device. Generous for
/// bring-up; the ICD's mapped working set (command rings + a few host-visible
/// device-memory BOs) is far smaller. Table-full → `MAP_BLOB` fails cleanly.
const MAX_MAPPINGS: usize = 256;

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
    /// `0` is the initialized + unlocked state of a `KSPIN_LOCK`, so no explicit
    /// `KeInitializeSpinLock` is needed (same rationale as `virtio::hal`).
    lock: UnsafeCell<KSPIN_LOCK>,
    /// Live mappings. Capacity reserved to `MAX_MAPPINGS` at construction;
    /// `push`/`pop` within that capacity never (de)allocate.
    entries: UnsafeCell<Vec<Mapping>>,
}

// SAFETY: every access to `entries` is serialized by `lock` (a kernel spinlock).
// `Mapping` is Copy/POD (the `usize` MDL is an opaque token, dereferenced only by
// the ioctl unmap path at PASSIVE_LEVEL in the owning process).
unsafe impl Send for MappingTable {}
unsafe impl Sync for MappingTable {}

impl MappingTable {
    /// Reserve the backing buffer up front (PASSIVE_LEVEL). After this, `insert`
    /// up to `MAX_MAPPINGS` entries performs no allocation.
    pub fn new() -> Self {
        Self {
            lock: UnsafeCell::new(0),
            entries: UnsafeCell::new(Vec::with_capacity(MAX_MAPPINGS)),
        }
    }

    /// True if `resource_id` already has a live mapping. Used by `MAP_BLOB` to
    /// reject a duplicate map (which would leak a window offset / desync the host).
    pub fn contains(&self, resource_id: u32) -> bool {
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(self.lock.get()) };
        // SAFETY: spinlock-guarded read of the entries.
        let found = unsafe { &*self.entries.get() }
            .iter()
            .any(|m| m.resource_id == resource_id);
        unsafe { KeReleaseSpinLock(self.lock.get(), irql) };
        found
    }

    /// Record a freshly-created mapping owned by `owner` (the request's
    /// `WDFFILEOBJECT`). Returns `false` (without allocating) if the table is at
    /// capacity — the caller then unmaps the just-created mapping. The `push` stays
    /// within the reserved capacity, so it makes no allocator call and is safe under
    /// the spinlock.
    pub fn insert(&self, owner: usize, resource_id: u32, user_va: u64, mdl: usize) -> bool {
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(self.lock.get()) };
        // SAFETY: spinlock-guarded exclusive access to the entries.
        let entries = unsafe { &mut *self.entries.get() };
        let ok = if entries.len() < MAX_MAPPINGS {
            entries.push(Mapping {
                owner,
                resource_id,
                user_va,
                mdl,
            });
            true
        } else {
            false
        };
        unsafe { KeReleaseSpinLock(self.lock.get(), irql) };
        ok
    }

    /// Pop one live mapping OWNED BY `owner` for teardown, returning `(user_va, mdl)`.
    /// `EvtFileCleanup` loops until this returns `None`, unmapping each entry at
    /// PASSIVE_LEVEL *outside* the lock (`MmUnmapLockedPages` requires PASSIVE; the
    /// lock raises to DISPATCH). Popping one at a time avoids a large on-stack
    /// collection of all entries; `swap_remove` is O(1) and never reallocates, so it
    /// stays spinlock-safe. Entries owned by OTHER open handles are left untouched.
    pub fn take_one_for(&self, owner: usize) -> Option<(u64, usize)> {
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(self.lock.get()) };
        // SAFETY: spinlock-guarded exclusive access to the entries.
        let entries = unsafe { &mut *self.entries.get() };
        let popped = entries.iter().position(|m| m.owner == owner).map(|i| {
            let m = entries.swap_remove(i);
            (m.user_va, m.mdl)
        });
        unsafe { KeReleaseSpinLock(self.lock.get(), irql) };
        popped
    }

    /// Pop the mapping for `resource_id` owned by `owner`, if one exists. Used by
    /// explicit BO release while the process is still alive.
    pub fn take_for_resource(&self, owner: usize, resource_id: u32) -> Option<(u64, usize)> {
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(self.lock.get()) };
        // SAFETY: spinlock-guarded exclusive access to the entries.
        let entries = unsafe { &mut *self.entries.get() };
        let popped = entries
            .iter()
            .position(|m| m.owner == owner && m.resource_id == resource_id)
            .map(|i| {
                let m = entries.swap_remove(i);
                (m.user_va, m.mdl)
            });
        unsafe { KeReleaseSpinLock(self.lock.get(), irql) };
        popped
    }
}
