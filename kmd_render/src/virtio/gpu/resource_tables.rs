//! The bounded resource, blob and context tables, and the host-visible window
//! offset allocator.
//!
//! Moved verbatim out of `virtio/gpu.rs` by T8/R1103; `gpu.rs` became
//! `gpu/mod.rs` in the same commit so this module can name `VirtioGpu`'s
//! private fields (a child sees its parent's private items; a sibling does
//! not).
//!
//! Every method here is field-disjoint from the control queue and the fence
//! tables -- verified, not assumed: none of the 32 touches `transport`,
//! `control`, `inflight`, `parked`, `dma_pool`, `fence_waiters`,
//! `fence_events`, `wddm_pending` or `scanout_refresh_watermark`, and there is
//! no `self.<method>()` call across the boundary in either direction.

use super::*;

impl VirtioGpu {
    // ── Table helpers (Gate 2 → C3/M3.4 phased flows) ────────────────────────
    //
    // The control round-trips themselves live in `virtio::ctrl` (PASSIVE
    // waits); these lock-context helpers keep the bounded tables consistent
    // across the multi-phase flows. Reservation counters guarantee `push`
    // never exceeds the capacity reserved at init (no realloc under the
    // spinlock), even with concurrent multi-phase creates.

    /// Allocate a fresh guest context id (namespace owned by the KMD).
    pub fn alloc_ctx_id(&mut self) -> u32 {
        let id = self.next_ctx_id;
        self.next_ctx_id = self.next_ctx_id.wrapping_add(1);
        id
    }

    /// Allocate a fresh guest resource id (namespace owned by the KMD).
    pub fn alloc_resource_id(&mut self) -> u32 {
        let id = self.next_resource_id;
        self.next_resource_id = self.next_resource_id.wrapping_add(1);
        id
    }

    /// Reserve a context tracking slot for an in-flight CTX_CREATE.
    ///
    /// TRACKING IS MANDATORY, the same rule `reserve_resource_slot` already
    /// applies to resources: refuse the create when the table is full rather
    /// than creating a context that works but is untracked. Tracking used to be
    /// best-effort, which had two consequences — an untracked context is never
    /// reclaimed at device teardown, and (the reason this changed) an ownership
    /// check against a best-effort table would have to trust an unknown id,
    /// which is not a check at all. With reserve-then-commit, "live but
    /// untracked" cannot exist, so `resolve_owned_ctx` is authoritative.
    ///
    /// Same-boot evidence for the safety of refusing: contexts_live = 9 against
    /// MAX_CONTEXTS = 1024 with context_full_drops = 0 on a full desktop
    /// (escape_owner_probe, 2026-07-26), so no live workload approaches the cap.
    pub fn reserve_context_slot(&mut self) -> bool {
        if self.contexts.len() + self.contexts_reserved >= MAX_CONTEXTS {
            // Keeps its name and its QUERY_STATS field; the event it counts is
            // now "CTX_CREATE refused because the table is full" rather than
            // "tracking silently dropped".
            CONTEXT_FULL_DROPS.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.contexts_reserved += 1;
        true
    }

    /// Commit a reserved context slot once the host has created the context.
    pub fn commit_context(&mut self, owner: Option<DeviceOwner>, ctx_id: u32) {
        self.contexts_reserved = self.contexts_reserved.saturating_sub(1);
        self.contexts.push(ContextSlot { owner, ctx_id });
    }

    /// Release a reserved context slot after a failed create.
    pub fn cancel_context_reservation(&mut self) {
        self.contexts_reserved = self.contexts_reserved.saturating_sub(1);
    }

    /// Resolve `ctx_id` for `owner`, or `None` if it is untracked or tracked by
    /// a DIFFERENT owner.
    pub fn resolve_owned_ctx(&self, owner: Option<DeviceOwner>, ctx_id: u32) -> Option<OwnedCtx> {
        self.contexts
            .iter()
            .find(|c| c.ctx_id == ctx_id && c.owner == owner)
            .map(|c| OwnedCtx { id: c.ctx_id })
    }

    /// Drop a context's tracking slot, but only for its owner. Returns the
    /// resolved id, or `None` if the caller does not own it.
    pub fn untrack_owned_context(
        &mut self,
        owner: Option<DeviceOwner>,
        ctx_id: u32,
    ) -> Option<u32> {
        let idx = self
            .contexts
            .iter()
            .position(|c| c.ctx_id == ctx_id && c.owner == owner)?;
        Some(self.contexts.swap_remove(idx).ctx_id)
    }

    /// Pop one context still owned by `owner` (device-teardown reclamation
    /// iterates this, running the CTX_DESTROY round-trip outside the lock).
    pub fn take_context_for_owner(&mut self, owner: Option<DeviceOwner>) -> Option<u32> {
        let idx = self.contexts.iter().position(|c| c.owner == owner)?;
        Some(self.contexts.swap_remove(idx).ctx_id)
    }

    /// Reserve a live-resource table slot for an in-flight create. The table is
    /// load-bearing (OpenAllocation / ATTACH liveness validation reads it), so
    /// an untracked-but-live resource must never exist: refuse the create when
    /// the table is full instead of creating and dropping the tracking entry.
    pub fn reserve_resource_slot(&mut self) -> bool {
        if self.resources.len() + self.resources_reserved >= MAX_RESOURCES {
            RESOURCE_FULL_REJECTS.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.resources_reserved += 1;
        true
    }

    /// Commit a reserved resource slot with the now-host-live id.
    pub fn commit_resource(&mut self, resource_id: u32) {
        self.resources_reserved = self.resources_reserved.saturating_sub(1);
        self.resources.push(resource_id);
        bump_high_water(&RESOURCE_HIGH_WATER, self.resources.len());
    }

    /// Release a reserved resource slot after a failed create.
    pub fn cancel_resource_reservation(&mut self) {
        self.resources_reserved = self.resources_reserved.saturating_sub(1);
    }

    /// Reserve a blob-table slot for an in-flight ALLOC_BLOB.
    pub fn reserve_blob_slot(&mut self) -> bool {
        if self.blobs.len() + self.blobs_reserved >= MAX_BLOBS {
            BLOB_FULL_REJECTS.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.blobs_reserved += 1;
        true
    }

    /// Commit a reserved blob slot.
    pub fn commit_blob(
        &mut self,
        owner: Option<DeviceOwner>,
        ctx_id: u32,
        resource_id: u32,
        size: u64,
    ) {
        self.blobs_reserved = self.blobs_reserved.saturating_sub(1);
        self.blobs.push(BlobSlot {
            owner,
            ctx_id,
            resource_id,
            size,
            mapped: false,
            map_pending: false,
            map_cache: 0,
            map_offset: 0,
            map_len: 0,
        });
        bump_high_water(&BLOB_HIGH_WATER, self.blobs.len());
    }

    /// Release a reserved blob slot after a failed create.
    pub fn cancel_blob_reservation(&mut self) {
        self.blobs_reserved = self.blobs_reserved.saturating_sub(1);
    }

    /// Pop the blob slot matching (`owner`, `ctx_id`, `resource_id`) — the
    /// RELEASE_BLOB path. The caller unmaps/detaches/unrefs outside the lock
    /// and returns the window range via [`Self::free_window_range_pub`].
    /// Takes a `DeviceOwner`, not an `Option`: the KMD-owned slots are
    /// unreachable from this path by TYPE, which is the whole point — an escape
    /// with a null hDevice used to match every blob the KMD had adopted for a
    /// live WDDM allocation.
    pub fn take_blob_matching(
        &mut self,
        owner: DeviceOwner,
        ctx_id: u32,
        resource_id: u32,
    ) -> Option<(u32, bool, u64, u64)> {
        let idx = self.blobs.iter().position(|s| {
            s.owner == Some(owner) && s.ctx_id == ctx_id && s.resource_id == resource_id
        })?;
        let slot = self.blobs.swap_remove(idx);
        Some((slot.resource_id, slot.mapped, slot.map_offset, slot.map_len))
    }

    /// Pop one blob still owned by `owner` (device-teardown reclamation).
    /// Returns `(ctx_id, resource_id, mapped, map_offset, map_len)`.
    pub fn take_blob_for_owner(
        &mut self,
        owner: Option<DeviceOwner>,
    ) -> Option<(u32, u32, bool, u64, u64)> {
        let idx = self.blobs.iter().position(|s| s.owner == owner)?;
        let slot = self.blobs.swap_remove(idx);
        Some((
            slot.ctx_id,
            slot.resource_id,
            slot.mapped,
            slot.map_offset,
            slot.map_len,
        ))
    }

    /// Whether `resource_id` is alive host-side, per the KMD's authoritative
    /// live-resource table (the KMD owns the resid namespace: every blob create
    /// and every unref goes through it, so this mirrors the host's global
    /// resource table exactly).
    ///
    /// This exists because the host's CTX_ATTACH_RESOURCE path CANNOT be
    /// trusted to report failure: `virgl_renderer_ctx_attach_resource` is void
    /// and silently no-ops on an unknown resource, so QEMU replies OK_NODATA
    /// for an attach that never happened — the exact mechanism behind the
    /// boot-#3 `vkr: failed to import resource: invalid res_id 45` dwm kill.
    /// OpenAllocation and the ATTACH_RESOURCE escape validate against this
    /// table and fail loudly instead.
    pub fn resource_is_live(&self, resource_id: u32) -> bool {
        self.resources.iter().any(|&r| r == resource_id)
    }

    /// Remove a live resource id from the one-shot ownership table.
    ///
    /// Returns true only for the first teardown claimant. Later duplicate release
    /// paths must skip host DETACH/UNREF, because the host has already destroyed
    /// the resource and returns ERR_INVALID_RESOURCE_ID.
    pub fn take_live_resource(&mut self, resource_id: u32) -> bool {
        let Some(idx) = self.resources.iter().position(|&r| r == resource_id) else {
            // Atomic, not diag::record — callers hold the device spinlock
            // (DISPATCH_LEVEL); the registry tracer is PASSIVE-only.
            TAKE_LIVE_MISSES.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        self.resources.swap_remove(idx);
        true
    }

    /// Record a blob's size in the tracking table so a later map can size the
    /// mapping. Used by the in-kernel venus client, which creates its
    /// ring/reply/page-table blobs directly (it owns the resource lifecycle for
    /// the device lifetime rather than per-escape). `owner = 0` marks a
    /// KMD-internal blob (not reclaimed by an escape owner). No-ops (counted)
    /// if the table is full — a later map then fails honestly.
    pub fn note_blob_size(&mut self, resource_id: u32, size: u64) {
        if self.blobs.iter().any(|s| s.resource_id == resource_id) {
            return;
        }
        if self.blobs.len() + self.blobs_reserved >= MAX_BLOBS {
            BLOB_FULL_REJECTS.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // Record with ctx_id 0 / KMD owner: these blobs are not driven by an escape
        // device handle; teardown unrefs them explicitly via the venus client.
        self.blobs.push(BlobSlot {
            owner: None,
            ctx_id: 0,
            resource_id,
            size,
            mapped: false,
            map_pending: false,
            map_cache: 0,
            map_offset: 0,
            map_len: 0,
        });
    }

    /// Look up a blob's tracking state by resource id (any owner). Returns
    /// `(owner, size, mapped)` if the resource is a tracked, host-visible-mappable
    /// blob. Used by the Present blit to decide whether the composition source /
    /// IddCx destination can be CPU-mapped for a coherence copy.
    pub fn blob_lookup(&self, resource_id: u32) -> Option<(Option<DeviceOwner>, u64, bool)> {
        self.blobs
            .iter()
            .find(|s| s.resource_id == resource_id)
            .map(|s| (s.owner, s.size, s.mapped))
    }

    /// Begin mapping a blob into the host-visible window: if already mapped,
    /// return the mapping; otherwise reserve a window range and hand the
    /// RESOURCE_MAP_BLOB round-trip to the caller (PASSIVE, outside this lock),
    /// who then calls [`Self::blob_map_finish`]. [`OwnerFilter::Any`] resolves by
    /// resource id alone (the kernel paths' any-owner lookup); `Exactly` is the
    /// owner-scoped escape path (resource ids can repeat across adapter restarts
    /// while stale clients unwind) and also names the KMD-owned slots as
    /// `Exactly(None)`.
    pub fn blob_map_begin(&mut self, owner: OwnerFilter, resource_id: u32) -> BlobMapBegin {
        let Some(window) = self.host_visible else {
            return BlobMapBegin::Failed(VirtioError::DeviceError);
        };
        let Some(idx) = self.blobs.iter().position(|s| {
            s.resource_id == resource_id
                && match owner {
                    OwnerFilter::Any => true,
                    OwnerFilter::Exactly(o) => s.owner == o,
                }
        }) else {
            return BlobMapBegin::Failed(VirtioError::DeviceError);
        };
        if self.blobs[idx].mapped {
            let s = &self.blobs[idx];
            return BlobMapBegin::Mapped(BlobMapPrep {
                gpa: window.base + s.map_offset,
                size: s.map_len,
                map_cache: s.map_cache,
            });
        }
        if self.blobs[idx].map_pending {
            return BlobMapBegin::Busy;
        }
        let map_len = round_up_page(self.blobs[idx].size);
        if map_len == 0 || map_len > MAX_BLOB_MAP_BYTES {
            return BlobMapBegin::Failed(VirtioError::DeviceError);
        }
        let offset = match self.window.alloc(map_len) {
            Ok(o) => o,
            Err(e) => return BlobMapBegin::Failed(e),
        };
        let s = &mut self.blobs[idx];
        s.map_pending = true;
        s.map_offset = offset;
        s.map_len = map_len;
        BlobMapBegin::Start {
            offset,
            len: map_len,
        }
    }

    /// Finish a [`Self::blob_map_begin`] `Start`: record the host's verdict.
    /// `cache = Some(nibble)` on RESP_OK_MAP_INFO; `None` on host rejection
    /// (the reserved range is returned to the allocator).
    pub fn blob_map_finish(
        &mut self,
        resource_id: u32,
        offset: u64,
        len: u64,
        cache: Option<u32>,
    ) -> BlobMapFinish {
        let Some(idx) = self
            .blobs
            .iter()
            .position(|s| s.resource_id == resource_id && s.map_pending && s.map_offset == offset)
        else {
            // Owner teardown raced the round-trip and took the slot. The caller
            // must undo the host mapping (UNMAP round-trip) and then return the
            // range via `free_window_range_pub`.
            if cache.is_none() {
                self.window.free(offset, len);
                return BlobMapFinish::HostRejected;
            }
            return BlobMapFinish::SlotGone;
        };
        let window_base = self.host_visible.map_or(0, |w| w.base);
        let s = &mut self.blobs[idx];
        s.map_pending = false;
        match cache {
            Some(c) => {
                s.mapped = true;
                s.map_cache = c;
                BlobMapFinish::Done(BlobMapPrep {
                    gpa: window_base + offset,
                    size: len,
                    map_cache: c,
                })
            }
            None => {
                s.map_offset = 0;
                s.map_len = 0;
                self.window.free(offset, len);
                BlobMapFinish::HostRejected
            }
        }
    }

    /// Return a window range to the allocator (PASSIVE flows that unmapped a
    /// blob outside the lock).
    pub fn free_window_range_pub(&mut self, offset: u64, len: u64) {
        self.window.free(offset, len);
    }

    /// Install the VidMm reserve: the first `len` bytes of the host-visible
    /// window belong to the CPU-visible BAR memory segment, so the KMD offset
    /// allocator starts past them and freed ranges inside them are never
    /// recycled by the KMD side.
    ///
    /// Legal ONLY while nothing has been issued. A second call, or one after any
    /// blob has been mapped, is refused and counted rather than silently
    /// stranding every offset already handed out below the new mark — those
    /// ranges could never be recycled, because `WindowAllocator::free` returns
    /// early for `offset < reserve`.
    ///
    /// Returns whether the reserve was installed.
    pub fn configure_window_reserve(&mut self, len: u64) -> bool {
        if !self.window.is_pristine() {
            WINDOW_RECONFIG_REFUSED.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.window.reserve = len;
        self.window.next_offset = len;
        true
    }

    /// Begin a fixed-offset (re)map of a blob at the VidMm-assigned window
    /// offset `offset` (must lie inside the VidMm partition). Unlike
    /// [`Self::blob_map_begin`] the offset is dictated by the caller, and an
    /// existing mapping at a DIFFERENT offset is handed back for unmapping —
    /// blob content is intrinsic to the host memory object, so a remap is
    /// content-preserving. Any-owner resolve (kernel path, like the executor).
    pub fn blob_remap_begin(&mut self, resource_id: u32, offset: u64) -> BlobRemapBegin {
        if self.host_visible.is_none() {
            return BlobRemapBegin::Failed(VirtioError::DeviceError);
        }
        let window_base = self.host_visible.map_or(0, |w| w.base);
        let Some(idx) = self.blobs.iter().position(|s| s.resource_id == resource_id) else {
            return BlobRemapBegin::Failed(VirtioError::DeviceError);
        };
        if self.blobs[idx].map_pending {
            return BlobRemapBegin::Busy;
        }
        if self.blobs[idx].mapped && self.blobs[idx].map_offset == offset {
            let s = &self.blobs[idx];
            return BlobRemapBegin::Mapped(BlobMapPrep {
                gpa: window_base + s.map_offset,
                size: s.map_len,
                map_cache: s.map_cache,
            });
        }
        let map_len = round_up_page(self.blobs[idx].size);
        if map_len == 0 || map_len > MAX_BLOB_MAP_BYTES {
            return BlobRemapBegin::Failed(VirtioError::DeviceError);
        }
        // The target range must sit entirely inside the VidMm partition —
        // anything else would collide with the KMD-side offset allocator.
        if offset % BLOB_PAGE != 0
            || offset
                .checked_add(map_len)
                .map_or(true, |e| e > self.window.reserve)
        {
            return BlobRemapBegin::Failed(VirtioError::DeviceError);
        }
        let old = if self.blobs[idx].mapped {
            Some((self.blobs[idx].map_offset, self.blobs[idx].map_len))
        } else {
            None
        };
        let s = &mut self.blobs[idx];
        s.map_pending = true;
        s.mapped = false;
        s.map_offset = offset;
        s.map_len = map_len;
        BlobRemapBegin::Start { old, len: map_len }
    }

    /// Blobs currently mapped overlapping `[offset, offset+len)` inside the
    /// VidMm partition, EXCLUDING `keep_resource_id`. Such mappings are stale
    /// VidMm placements (an eviction this driver missed or dropped): VidMm
    /// never double-books segment ranges, so before mapping a new blob into
    /// the range the caller must RESOURCE_UNMAP_BLOB each returned id and
    /// clear it via [`Self::blob_note_unmapped`]. Bounded scan under the lock.
    ///
    /// Returns `Err(WindowOverlapTruncated)` if a further overlap is found once
    /// `out` is full. The scan used to stop RECORDING at that point and return
    /// the truncated count, which the caller read as the complete set: a ninth
    /// overlapping mapping was neither unmapped nor reported, and the
    /// RESOURCE_MAP_BLOB that followed created exactly the overlapping host
    /// window subregion the eviction pass exists to prevent — two host resources
    /// through one window subregion, against the blob-window-offset invariant
    /// (k-gputransport-04). The buffer deliberately stays fixed-size: this scan
    /// runs under the device spinlock, where allocation is forbidden.
    pub fn blobs_overlapping(
        &self,
        offset: u64,
        len: u64,
        keep_resource_id: u32,
        out: &mut [u32],
    ) -> Result<usize, WindowOverlapTruncated> {
        let end = offset.saturating_add(len);
        let mut n = 0;
        for s in self.blobs.iter() {
            if s.mapped
                && s.resource_id != keep_resource_id
                && s.map_offset < self.window.reserve
                && s.map_offset < end
                && s.map_offset.saturating_add(s.map_len) > offset
            {
                if n == out.len() {
                    WINDOW_OVERLAP_TRUNCATED.fetch_add(1, Ordering::Relaxed);
                    return Err(WindowOverlapTruncated);
                }
                out[n] = s.resource_id;
                n += 1;
            }
        }
        Ok(n)
    }

    /// The blob currently mapped exactly at window `offset`, if any. Used by
    /// `DxgkDdiUnmapCpuHostAperture`, which names aperture pages but not the
    /// allocation.
    pub fn blob_resid_at_offset(&self, offset: u64) -> Option<u32> {
        self.blobs
            .iter()
            .find(|s| s.mapped && s.map_offset == offset)
            .map(|s| s.resource_id)
    }

    /// Record that a blob's host mapping was torn down outside the normal
    /// release path (stale-placement eviction in `map_blob_at`). No window
    /// range is freed here — VidMm-partition offsets never enter the free list.
    pub fn blob_note_unmapped(&mut self, resource_id: u32) {
        if let Some(s) = self
            .blobs
            .iter_mut()
            .find(|s| s.resource_id == resource_id && s.mapped)
        {
            s.mapped = false;
            s.map_offset = 0;
            s.map_len = 0;
        }
    }

    /// Transfer a blob's lifetime ownership from its escape owner (the D3DKMT
    /// device handle the ICD allocated it under) to the WDDM allocation adopting
    /// it in `DxgkDdiCreateAllocation`. Returns whether the resource is LIVE —
    /// adopting a dead resid must fail the CreateAllocation loudly.
    ///
    /// This closes the res-45 lifetime hole (2026-07-03 boot #3): without the
    /// re-tag, `DxgkDdiDestroyDevice`'s `release_blobs_for_owner` sweep unrefs
    /// the host resource when the CREATING process's device dies, even though
    /// the shared WDDM allocation (and its cross-process openers) still
    /// reference it. Re-tagging to the KMD owner removes it from every escape-owner
    /// reclaim path; from here the allocation destroy path
    /// (`destroy_allocation_ctx` → `forget_allocation_blob` + guarded unref)
    /// owns the lifetime, matching KMD-created standard allocations.
    pub fn adopt_blob_for_allocation(&mut self, resource_id: u32) -> bool {
        if !self.resource_is_live(resource_id) {
            // Atomic, not diag::record — CreateAllocation calls this under the
            // device spinlock (DISPATCH_LEVEL); the registry tracer is PASSIVE-only.
            ADOPT_DEAD_REJECTS.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if let Some(slot) = self.blobs.iter_mut().find(|s| s.resource_id == resource_id) {
            slot.owner = None;
        }
        true
    }

    /// Drop the KMD-internal (KMD-owned) tracking slot for an allocation's blob at
    /// DestroyAllocation time. Returns `Some((mapped, map_offset, map_len))` if
    /// the slot existed — when `mapped`, the caller (PASSIVE, outside the lock)
    /// must run the RESOURCE_UNMAP_BLOB round-trip and then return the range
    /// via [`Self::free_window_range_pub`]. Host detach/unref stays with the
    /// caller (the allocation owns the resource lifetime).
    pub fn forget_allocation_blob(&mut self, resource_id: u32) -> Option<(bool, u64, u64)> {
        let idx = self
            .blobs
            .iter()
            .position(|s| s.owner.is_none() && s.resource_id == resource_id)?;
        let slot = self.blobs.swap_remove(idx);
        Some((slot.mapped, slot.map_offset, slot.map_len))
    }

    /// Current number of tracked blob slots (diagnostics).
    pub fn blob_count(&self) -> usize {
        self.blobs.len()
    }

    /// Point-in-time table occupancy + host-visible-window usage for
    /// `HELIOS_ESCAPE_QUERY_STATS`. Called under the device spinlock; pure reads.
    pub fn table_stats(&self) -> TableStats {
        TableStats {
            blobs_live: self.blobs.len() as u32,
            resources_live: self.resources.len() as u32,
            contexts_live: self.contexts.len() as u32,
            window_used: self.window.used(),
            window_len: self.window.window_len,
        }
    }

    /// The host-visible blob window, or `None` if the device exposes none.
    /// `DxgkDdiQueryAdapterInfo` uses `base`/`len` to describe the CPU-visible
    /// memory segment, and `DxgkDdiBuildPagingBuffer` adds the VidMm-assigned
    /// segment offset to `base` for the user mapping. Gate 5a Stage 2.
    pub fn host_visible(&self) -> Option<HostVisibleWindow> {
        self.host_visible
    }
}
