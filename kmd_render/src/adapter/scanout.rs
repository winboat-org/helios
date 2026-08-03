//! Display refresh policy: which blob is selected for scanout 0, how a
//! coalesced RESOURCE_FLUSH is queued, how an exact allocation is retired from
//! scanout, and the throttled telemetry mirror that goes with them.
//!
//! Moved verbatim out of `adapter.rs` by T8/R1101. The scanout atomics stay
//! declared in [`super`]: `virtio/ctrl.rs` takes `NonNull::from(&adapter.<f>)`
//! for the DISPATCH-level completion callback on three of them, so they must
//! remain address-stable `pub` fields with no accessor wrapper.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::scanout_cadence::{present_marker_action, PresentMarkerAction};
use helios_kmd_logic::scanout_lease::{merge_read_epoch, next_epoch, surplus_republish, NO_LEASE};
use helios_kmd_logic::scanout_retire::{needs_disable, needs_fifo_barrier};
use wdk_sys::ntddk::KeSetEvent;

use crate::ddi::scanout_trace::LeaseEnd;
use crate::irql::PassiveLevel;

use super::{AdapterContext, ScanoutGuard};

/// Ticks the scanout pacing snapshot (R318). One rate for the whole block.
static SCANOUT_PACING_TICKS: AtomicU32 = AtomicU32::new(0);

/// Outcome of trying to queue one coalesced scanout refresh.
pub(crate) enum ScanoutRefreshQueue {
    Queued,
    Busy,
    Failed,
    Unavailable,
    /// The ownership gate refused this refresh: publishing now could only
    /// re-read a binding the app may already own. Distinct from `Unavailable`
    /// so `ScUnav` keeps meaning "a dirty frame was discarded because nothing
    /// was bound"; the `Og*` counters are this arm's census.
    Dropped,
}

/// A complete nonzero marker tail received through a UMD context.  The KMD
/// resolves it while holding the notification/transport lock, where the stream
/// table and the captured legacy boundary are mutually ordered.
#[derive(Clone, Copy)]
pub(crate) struct PresentStreamMarker {
    pub ctx_id: u32,
    pub value: u32,
    pub cookie: u64,
    pub creator_process: usize,
}

impl AdapterContext {
    // ── Presentation-epoch ownership (ROADMAP defect 0ab-B) ─────────────────
    //
    // See the field docs on `AdapterContext` for the invariant and
    // `helios_kmd_logic::scanout_lease` for the executable specification these
    // eight methods implement. Every predicate below is that crate's, so the
    // host tests cover the shipped decisions.

    /// Mint the presentation epoch for one DMA-buffer flip.
    ///
    /// Called from `DxgkDdiSubmitCommand`'s flip arm at DISPATCH_LEVEL, BEFORE
    /// the flip's handle is published to the display worker — so the worker can
    /// never bind a presentation whose epoch has not been minted.
    ///
    /// A CAS loop rather than `fetch_add` because [`next_epoch`] SATURATES: a
    /// wrap to 0 would read as `NO_LEASE` and silently ungate every flip. It
    /// terminates on x86_64 (`lock cmpxchg` has no spurious failure and some
    /// caller always wins), and the contention it can see is at most the
    /// per-node serialisation dxgkrnl already imposes on SubmitCommand.
    pub(crate) fn mint_present_epoch(&self) -> u64 {
        let mut current = self.scanout_present_epoch.load(Ordering::Acquire);
        loop {
            let minted = next_epoch(current);
            match self.scanout_present_epoch.compare_exchange_weak(
                current,
                minted,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    crate::ddi::scanout_trace::note_lease_minted();
                    return minted;
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// The display worker has published `epoch`'s buffer to the host.
    ///
    /// `superseded_previous` must be true ONLY when this programming issued a
    /// `SET_SCANOUT_BLOB` that changed which RESOURCE is bound. Then, and only
    /// then, every older epoch's lease ends: QEMU's control queue is strictly
    /// FIFO, so a returned `SET_SCANOUT_BLOB` proves every earlier-enqueued
    /// `RESOURCE_FLUSH` has already completed, and a flush enqueued afterwards
    /// cannot read a resource that is no longer the scan-out. That is a proof
    /// that NO READ REMAINS — not a claim that a read happened, which is the
    /// distinction 22.22.207.0 collapsed.
    ///
    /// ⚠ A re-bind of the SAME resource (an extent change) must NOT supersede:
    /// the buffer is still the scan-out, so a later flush still reads it, and
    /// releasing its lease would hand the app a buffer the host is about to
    /// read — the defect itself. Nor may a re-present of an already-bound buffer
    /// supersede, for the same reason. Both cases still advance the epoch below,
    /// because they are new presentations that need their own read.
    pub(crate) fn publish_bound_epoch(&self, epoch: u64, superseded_previous: bool) {
        // Whether the binding this call publishes is epoch-tracked at all —
        // written on BOTH arms, because the MMIO/desktop contract's `NO_LEASE`
        // is exactly the case the ownership gate must not act on. See the field
        // doc on `AdapterContext::scanout_epoch_tracked`.
        self.scanout_epoch_tracked
            .store((epoch != NO_LEASE) as u32, Ordering::Release);
        if epoch == NO_LEASE {
            return;
        }
        if superseded_previous {
            let _ = self.end_scanout_leases_through(
                self.scanout_bound_epoch.load(Ordering::Acquire),
                LeaseEnd::Superseded,
            );
        }
        // fetch_max, not store: the display worker is the only writer today, but
        // a backwards step here would silently un-cover an epoch a flush token
        // has already been issued against.
        self.scanout_bound_epoch.fetch_max(epoch, Ordering::AcqRel);
    }

    /// End every presentation lease at or below `epoch`, for `reason`.
    ///
    /// Returns whether the watermark actually advanced. Monotone and idempotent:
    /// a release that arrives out of order, twice, or behind a supersede is
    /// inert rather than a backwards step. Legal at any IRQL — the used-ring
    /// drain calls it under `virtio_lock`, where taking `wddm_notify_lock` would
    /// invert the driver's lock order.
    ///
    /// A no-op return is deliberately NOT counted as `LsStal` here: several
    /// terminal paths release redundantly on purpose (a second reject, a
    /// teardown after a supersede), and folding those into the stale-token
    /// counter would make the one number that says "the flush path is issuing
    /// reads nobody waits for" unreadable. Only [`ScanoutFlushToken::complete`]
    /// counts a stale token.
    pub(crate) fn end_scanout_leases_through(&self, epoch: u64, reason: LeaseEnd) -> bool {
        if epoch == NO_LEASE {
            return false;
        }
        let previous = self.scanout_read_epoch.fetch_max(epoch, Ordering::AcqRel);
        if merge_read_epoch(previous, epoch) == previous {
            return false;
        }
        crate::ddi::scanout_trace::note_lease_end(reason);
        // The WDDM pending FIFO may have a releasable head — a lease end no
        // longer gates one (22.22.217.0 retired the withholding), but this is
        // still the cheapest wake for a drain that the caller cannot run itself
        // (the used-ring drain holds `virtio_lock`).
        self.scanout_retire_wanted.store(1, Ordering::Release);
        // SAFETY: hpd_event is initialized in place and stable for the adapter
        // lifetime; KeSetEvent(Wait=FALSE) is legal through DISPATCH_LEVEL.
        unsafe { KeSetEvent(self.hpd_event.get(), 0, 0) };
        true
    }

    /// End every lease this transport generation ever minted.
    ///
    /// The teardown/reset escape. NOT a timeout: it runs only where the host can
    /// no longer be reading anything — transport failure, preemption/TDR epoch,
    /// StopDevice, or an allocation being retired out of scan-out.
    pub(crate) fn release_all_scanout_leases(&self, reason: LeaseEnd) {
        // Every one of this function's callers means "the epoch-tracked binding
        // is gone" — transport failure, TDR epoch, StopDevice, an allocation
        // retiring out of scan-out, or a copy-fallback bind that publishes no
        // epoch at all. Turning the ownership gate off with them keeps a stale
        // `present > bound` from surviving into an untracked binding.
        self.scanout_epoch_tracked.store(0, Ordering::Release);
        let _ = self
            .end_scanout_leases_through(self.scanout_present_epoch.load(Ordering::Acquire), reason);
    }

    /// Publish the physical address a later CRTC_VSYNC reports for this bind.
    ///
    /// ⚠ 22.22.217.0 RETIRED THE WITHHOLDING that used to live here. The address
    /// was held back until the presentation's host read finished, on the theory
    /// that `PresentFlipPrivate`'s address is the second edge that returns a
    /// buffer to DXGI (`DXGK_INTERRUPT_DMA_COMPLETED` being the first). Both
    /// halves were then MEASURED INERT against the black frames — the 2×2
    /// factorial moved whole-flush black by nothing in any cell — because the
    /// app's clear never travels in a WDDM DMA buffer and so waits on no
    /// completion this driver controls. What replaced it is the ownership gate
    /// on the flush executor, which fixes the PUBLISH TIME rather than the
    /// buffer's lifetime. `LsPub` stays as the publication census.
    pub(crate) fn publish_bound_primary(&self, address: u64) {
        self.publish_displayed_primary(super::ProgrammedPrimary::after_scanout_bind(address));
        crate::ddi::scanout_trace::note_lease_primary_published();
    }

    /// Mark already-completed scanout contents dirty. The normal copied path
    /// does this from the ring-1 GPU-completion DPC; the direct-primary
    /// zero-copy case has no KMD GPU submission, so SetVidPn uses this after
    /// Windows has handed it the completed primary.
    pub fn request_scanout_refresh(&self) {
        self.request_scanout_refresh_for(0);
    }

    /// Request a refresh that must flush `resource_id` specifically (0 = the
    /// currently bound target, the identity-free HERF edge).
    ///
    /// Last writer wins, matching the watermark's own coalescing: markers
    /// coalesce to the newest, and it is the newest frame's buffer that has to
    /// reach the host.
    pub fn request_scanout_refresh_for(&self, resource_id: u32) {
        self.pending_refresh_resource
            .store(resource_id, Ordering::Release);
        self.scanout_refresh_pending.store(1, Ordering::Release);
        // SAFETY: hpd_event is initialized in place and stable for the adapter
        // lifetime; KeSetEvent(Wait=FALSE) is legal through DISPATCH_LEVEL.
        unsafe { KeSetEvent(self.hpd_event.get(), 0, 0) };
    }

    /// The marker and bind edges publish their pending identity while holding
    /// the same notification lock that serializes their watermark state.  The
    /// event is non-blocking and legal at DISPATCH, so keeping it in that
    /// critical section closes the old unlock-to-store window where a later
    /// PRESENT could overwrite a bind edge's exact pending resource.
    pub(crate) fn request_scanout_refresh_for_locked(
        &self,
        _guard: &super::WddmNotifyGuard<'_>,
        resource_id: u32,
    ) {
        self.request_scanout_refresh_for(resource_id);
    }

    /// Arm from the UMD's PRESENT MARKER, recording this frame's completion
    /// boundary for the buffer it named so the bind edge can use it.
    ///
    /// The marker is the last point in the pipeline at which "every Venus
    /// command submitted so far" still means "this frame and nothing after it":
    /// the app records it inside its own Present, before it can have submitted
    /// the next frame's work. Everything downstream — the flip's DMA
    /// submission, and the bind in the PASSIVE display worker — is at least a
    /// frame later at a fullscreen frame rate, which is what made both earlier
    /// 0ab-B attempts wait a frame too long and read a re-cleared buffer.
    pub(crate) fn arm_present_marker_refresh(
        &self,
        resource_id: u32,
        stream_marker: Option<PresentStreamMarker>,
    ) -> bool {
        self.with_wddm_notify_lock(|guard| {
            // A marker for a future flip records its exact completion boundary
            // below, but cannot name a host read yet. Only the current bind's
            // identity (or the explicitly identity-free HERF edge) may create
            // a pending refresh here. This is identity-only: no PID, timing,
            // buffer-count, or creation-order inference participates.
            let action = present_marker_action(
                resource_id,
                self.active_scanout_resource.load(Ordering::Acquire),
            );
            let ready = guard
                .with_virtio(|order, v| {
                    let watermark = stream_marker
                        .and_then(|marker| {
                            v.present_stream_marker_boundary(
                                marker.ctx_id,
                                marker.value,
                                marker.cookie,
                                marker.creator_process,
                            )
                        })
                        // A missing, partial, or rejected stream tail is
                        // exact legacy behavior: capture the current normal
                        // wire boundary instead of manufacturing a dependency.
                        .unwrap_or_else(|| v.wire_fence_watermark());
                    if resource_id != 0 {
                        self.record_frame_watermark(resource_id, watermark);
                    }
                    if action == PresentMarkerAction::QueueImmediate {
                        v.note_scanout_refresh_at(order, resource_id, watermark)
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            if ready {
                self.request_scanout_refresh_for_locked(guard, resource_id);
            }
            ready
        })
    }

    /// Take this flip's recorded frame boundary at FLIP-ARM time, for the
    /// allocation to carry to its own bind (ROADMAP defect 0ab-B, D1(i)).
    ///
    /// THE MARK-OVERWRITE WINDOW, which is what this exists to close. The mark
    /// table holds ONE boundary per resource ([`Self::record_frame_watermark`]
    /// replaces a same-resource entry), so a bind landing more than two frame
    /// periods after its present finds the mark already overwritten by the SAME
    /// buffer's NEXT present and waits for a frame it does not belong to. That
    /// fits the measurement exactly: at 1:1 bind:present, 41 % of binds still
    /// deferred, and deferred binds are the 6–12 ms / 34–60 %-black bucket.
    ///
    /// dxgkrnl submits a flip about a frame after the app presented and always
    /// before that overwrite, so this is the last point at which the mark still
    /// belongs to the frame being flipped.
    ///
    /// Returns 0 when no marker named this buffer. Runs at DISPATCH from the
    /// flip arm (`submit_command::arm_dma_flip` ->
    /// `display::arm_dma_flip_programming`), which is legal: the drain DPC
    /// already takes this lock at DISPATCH. It is a short, SECOND acquisition,
    /// taken before `note_and_maybe_signal`'s own scope and deliberately not
    /// folded into it — that scope also raises to the device DIRQL.
    pub(crate) fn take_flip_frame_watermark(&self, resource_id: u32) -> u64 {
        if resource_id == 0 {
            return 0;
        }
        self.with_wddm_notify_lock(|_guard| self.take_frame_watermark(resource_id))
            .unwrap_or(0)
    }

    /// Arm the BIND edge against the boundary this buffer's own present marker
    /// captured, falling back to "now" when no marker named it (the
    /// MMIO/desktop path, where dxgkrnl retires the flip before calling us, so
    /// "now" IS that frame's boundary).
    ///
    /// `carried_watermark` is the mark this flip took at arm time and stamped on
    /// its allocation (0 = none); it is preferred over the table because the
    /// table entry may since have been overwritten by the same buffer's next
    /// present — see [`Self::take_flip_frame_watermark`].
    ///
    /// Returns `(ready, used_frame_boundary)`; the second value is counted as
    /// `BeCar`/`BeSmp` so an inert fix is visible rather than inferred.
    pub(crate) fn arm_bind_refresh(
        &self,
        resource_id: u32,
        carried_watermark: u64,
    ) -> (bool, bool) {
        self.with_wddm_notify_lock(|guard| {
            let (ready, carried) =
                self.arm_bind_refresh_locked(guard, resource_id, carried_watermark);
            if ready {
                self.request_scanout_refresh_for_locked(guard, resource_id);
            }
            (ready, carried)
        })
    }

    /// The body of [`Self::arm_bind_refresh`], for a caller that already holds
    /// `wddm_notify_lock`.
    ///
    /// Split out for the used-ring drain's fast-bind application (ROADMAP defect
    /// 0ab-C, D1(ii)), which runs at DISPATCH inside the notify scope
    /// `drain_used_and_complete` already opens. Taking the notify lock a second
    /// time from in there would self-deadlock on a non-recursive spinlock; the
    /// PASSIVE wrapper above keeps its signature and its `ready` tail, and that
    /// caller mirrors the tail itself (the same shape as the existing
    /// `take_ready_scanout_refresh` handling next to it).
    pub(crate) fn arm_bind_refresh_locked(
        &self,
        guard: &super::WddmNotifyGuard<'_>,
        resource_id: u32,
        carried_watermark: u64,
    ) -> (bool, bool) {
        guard
            .with_virtio(|order, v| {
                // Taken even when the flip carried a mark: leaving it behind
                // would let a later bind with no fresh present honour a
                // boundary that has already been used, which is an unordered
                // flush (defect 0ab-A). Its value is also the overwrite
                // census — the number that confirms or kills the mechanism
                // above rather than assuming it.
                let table = self.take_frame_watermark(resource_id);
                let watermark = if carried_watermark != 0 {
                    crate::ddi::scanout_trace::note_bind_watermark_allocation();
                    if table.is_some_and(|t| t != carried_watermark) {
                        crate::ddi::scanout_trace::note_bind_watermark_overwritten();
                    }
                    carried_watermark
                } else {
                    table.unwrap_or_else(|| v.wire_fence_watermark())
                };
                // The per-flip packet may carry a marker after its context or
                // device was torn down.  Rebase that explicitly cancelled
                // stream boundary onto ordinary wire order; `!live` is never
                // treated as a successful producer retirement.
                let watermark = v
                    .rebase_dead_present_stream_boundary(watermark)
                    .unwrap_or(watermark);
                let ready = v.note_scanout_refresh_at(order, resource_id, watermark);
                if !ready {
                    // What is this deferral actually waiting for? Recorded
                    // because two boundary variants have now been falsified
                    // by inference alone (ROADMAP 0ab-B).
                    let (count, ring) = v.outstanding_below(watermark);
                    crate::ddi::scanout_trace::note_bind_wait(count, ring);
                }
                (ready, carried_watermark != 0 || table.is_some())
            })
            .unwrap_or((false, false))
    }

    /// Remember `watermark` as `resource_id`'s frame boundary, replacing any
    /// older one for the same buffer (a re-present of the same buffer is a
    /// newer frame, and it is the newer frame the display will show).
    ///
    /// Slot reuse takes a free slot first, then the OLDEST insertion ordinal —
    /// the buffer that has gone longest without being presented — so a buffer
    /// that stopped rotating cannot pin a slot.  The ordinal is separate from
    /// the boundary because registered-stream and legacy wire values have no
    /// shared numeric ordering.
    ///
    /// Caller must hold `wddm_notify_lock`.
    fn record_frame_watermark(&self, resource_id: u32, watermark: u64) {
        let ordinal = self
            .frame_watermark_next_ordinal
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let mut victim = 0usize;
        for i in 0..self.frame_watermark_resource.len() {
            let id = self.frame_watermark_resource[i].load(Ordering::Relaxed);
            if id == resource_id {
                self.frame_watermark_fence[i].store(watermark, Ordering::Relaxed);
                self.frame_watermark_ordinal[i].store(ordinal, Ordering::Relaxed);
                return;
            }
            if id == 0 {
                victim = i;
            } else if self.frame_watermark_resource[victim].load(Ordering::Relaxed) != 0
                && self.frame_watermark_ordinal[i].load(Ordering::Relaxed)
                    < self.frame_watermark_ordinal[victim].load(Ordering::Relaxed)
            {
                victim = i;
            }
        }
        self.frame_watermark_fence[victim].store(watermark, Ordering::Relaxed);
        self.frame_watermark_ordinal[victim].store(ordinal, Ordering::Relaxed);
        self.frame_watermark_resource[victim].store(resource_id, Ordering::Relaxed);
    }

    /// Take (and clear) the boundary recorded for `resource_id`.
    ///
    /// Cleared on read so a bind not backed by a fresh present cannot re-use a
    /// boundary that has already been honoured — that would be an unordered
    /// flush, which is defect 0ab-A.
    ///
    /// Caller must hold `wddm_notify_lock`.
    fn take_frame_watermark(&self, resource_id: u32) -> Option<u64> {
        if resource_id == 0 {
            return None;
        }
        for i in 0..self.frame_watermark_resource.len() {
            if self.frame_watermark_resource[i].load(Ordering::Relaxed) == resource_id {
                self.frame_watermark_resource[i].store(0, Ordering::Relaxed);
                self.frame_watermark_ordinal[i].store(0, Ordering::Relaxed);
                return Some(self.frame_watermark_fence[i].load(Ordering::Relaxed));
            }
        }
        None
    }

    // ── Wire-order guard for bind bookkeeping (ROADMAP defect 0ab-C, D1(ii)) ──

    /// Mint the next `SET_SCANOUT_BLOB` wire-order sequence.
    ///
    /// MUST be called inside the same `with_virtio` critical section as the
    /// enqueue it names — that is the whole guarantee: the control queue is
    /// FIFO, so minting under the lock that publishes the descriptor makes
    /// sequence order equal wire order, and therefore equal the order the host
    /// applies the binds in. Minting outside the lock would order the two
    /// enqueue sites by nothing at all.
    ///
    /// Never returns 0: 0 is "this application named no wire bind" at the guard
    /// (an already-bound re-present, which issues no command and must keep
    /// today's unguarded behaviour).
    ///
    /// `resource_id` is the resource the command being enqueued names (0 for the
    /// scan-out disable) and is published with the sequence, in the same lock
    /// hold, as [`AdapterContext::scanout_bind_wire_resource`] — the WIRE view
    /// of "what is bound", which is what the flip arm's skip test needs.
    pub(crate) fn mint_scanout_bind_seq(&self, resource_id: u32) -> u64 {
        // Stored BEFORE the sequence is handed out, so the two are published
        // together under one `virtio_lock` hold; see the field doc for why
        // Relaxed is the right ordering here.
        self.scanout_bind_wire_resource
            .store(resource_id, Ordering::Relaxed);
        self.scanout_bind_wire_seq
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
    }

    /// Claim the right to apply the bookkeeping for bind `seq`.
    ///
    /// True when `seq` is newer than every application that has run, which
    /// `fetch_max` decides and publishes in one operation. False means a LATER
    /// bind's bookkeeping has already been applied, so this one must apply
    /// nothing: its identity, its supersede decision and its flush arm all
    /// describe a binding the host has already moved on from.
    pub(crate) fn adopt_scanout_bind_seq(&self, seq: u64) -> bool {
        // 0 names no wire bind, and `fetch_max(0)` would silently succeed for
        // the very first application of a generation.
        if seq == 0 {
            return false;
        }
        let applied = self
            .scanout_bind_applied_seq
            .fetch_max(seq, Ordering::AcqRel);
        applied < seq
    }

    /// Remember the blob currently selected for scanout 0.
    ///
    /// THE CONTRACT POINT IS "after a host-accepted `SET_SCANOUT_BLOB`", not
    /// "at PASSIVE": the PASSIVE worker calls it once its synchronous
    /// round-trip returns OK, and the used-ring drain's fast-bind application
    /// calls it at DISPATCH once that command's response returns OK
    /// (ROADMAP defect 0ab-C, D1(ii)). Three plain atomic stores, so the IRQL
    /// never mattered — what matters is that the host really is bound, and that
    /// the caller holds the newest bind sequence (see
    /// [`Self::adopt_scanout_bind_seq`]).
    pub fn remember_scanout_blob(&self, resource_id: u32, width: u32, height: u32) {
        let wh = ((width as u64) << 32) | height as u64;
        self.active_scanout_wh
            .store(wh, core::sync::atomic::Ordering::Release);
        self.active_scanout_resource
            .store(resource_id, core::sync::atomic::Ordering::Release);
        // Every caller has a host-accepted SET_SCANOUT_BLOB for exactly this
        // resource behind it, which is what makes this store the truth rather
        // than an intention.
        self.host_bound_scanout_resource
            .store(resource_id, core::sync::atomic::Ordering::Release);
    }

    /// Publish the exact Venus import identity of the KMD-owned LINEAR primary.
    /// `resource_id` is stored last so an acquire reader never combines a new id
    /// with stale geometry or allocation parameters.
    pub fn remember_primary_scanout(
        &self,
        resource_id: u32,
        width: u32,
        height: u32,
        pitch: u32,
        plane_offset: u32,
        alloc_size: u64,
        memory_type_index: u32,
        dxgi_format: u32,
    ) {
        use core::sync::atomic::Ordering;
        // Odd: readers must not use the fields between these two bumps.
        self.primary_scanout_seq.fetch_add(1, Ordering::Release);
        self.primary_scanout_wh
            .store(((width as u64) << 32) | height as u64, Ordering::Relaxed);
        self.primary_scanout_layout.store(
            ((pitch as u64) << 32) | plane_offset as u64,
            Ordering::Relaxed,
        );
        self.primary_scanout_alloc_size
            .store(alloc_size, Ordering::Relaxed);
        self.primary_scanout_memory_type
            .store(memory_type_index, Ordering::Relaxed);
        self.primary_scanout_dxgi_format
            .store(dxgi_format, Ordering::Relaxed);
        self.primary_scanout_generation
            .fetch_add(1, Ordering::Relaxed);
        self.primary_scanout_resource
            .store(resource_id, Ordering::Release);
        // Even: the set is coherent again.
        self.primary_scanout_seq.fetch_add(1, Ordering::Release);
    }

    /// Remove a published primary identity only if it still names `resource_id`.
    pub fn forget_primary_scanout(&self, resource_id: u32) {
        use core::sync::atomic::Ordering;
        // The dedicated LINEAR scanout belongs to the adapter, not to any
        // transient WDDM allocation that imports its Venus resource id.  DWM
        // creates and destroys several device/allocation generations during
        // startup; letting one of those destroys clear this identity leaves
        // scanout 0 bound to live-but-unpublishable stale pixels.
        if resource_id != 0
            && self.dedicated_scanout_resource.load(Ordering::Acquire) == resource_id
        {
            return;
        }
        self.primary_scanout_seq.fetch_add(1, Ordering::Release);
        if self
            .primary_scanout_resource
            .compare_exchange(resource_id, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.primary_scanout_generation
                .fetch_add(1, Ordering::Relaxed);
        }
        self.primary_scanout_seq.fetch_add(1, Ordering::Release);
    }

    /// Queue one non-blocking RESOURCE_FLUSH for the selected scanout.  The
    /// the exact-primary copy's ring-1 completion DPC sets the dirty bit and
    /// wakes the worker only after the Venus GPU copy has completed. One
    /// in-flight command is the backpressure boundary.
    pub(crate) fn queue_active_scanout_refresh(
        &self,
        passive: PassiveLevel,
    ) -> ScanoutRefreshQueue {
        let outcome = self.with_scanout_lifecycle(passive, |lock| {
            self.queue_active_scanout_refresh_locked(lock)
        });
        // R318: the pacing snapshot runs OUTSIDE `scanout_mutex`. It used to run
        // inside it — 32 synchronous registry transactions every 16 queued
        // refreshes, roughly 3.75 bursts per second at 60 Hz, on the PASSIVE
        // display worker while holding the lock DestroyAllocation must acquire
        // to retire a primary. Every counter read is an independent atomic, so
        // sampling them outside the lock changes no value any consumer compares
        // against another.
        if matches!(outcome, ScanoutRefreshQueue::Queued) {
            self.pacing_snapshot();
        }
        outcome
    }

    /// Low-rate mirror of the DISPATCH/DIRQL-updated pacing counters, which
    /// otherwise become visible only at device teardown.
    ///
    /// ONE rate now (~600 refreshes, about 10 s at 60 Hz): the 16-period set is
    /// folded in. `RfFail`, `RfUnb` and `ScDead` are deliberately NOT here —
    /// those stay loud and in place at their own sites. (`RbFail` was here
    /// until T6/R902 deleted the async bind arm that produced it.)
    fn pacing_snapshot(&self) {
        use core::sync::atomic::Ordering;

        let n = self.scanout_refresh_count.load(Ordering::Relaxed);
        let resource_id = self.active_scanout_resource.load(Ordering::Acquire);
        let wh = self.active_scanout_wh.load(Ordering::Relaxed);
        let width = (wh >> 32) as u32;
        let height = wh as u32;
        if !crate::diag::sample_tick(&SCANOUT_PACING_TICKS) {
            return;
        }

        crate::diag::record_named_bytes(b"RfRid", resource_id);
        crate::diag::record_named_bytes(b"RfWH", (width << 16) | (height & 0xFFFF));
        crate::diag::record_named_bytes(b"RfCnt", n);
        crate::diag::record_named_bytes(
            b"RfFail",
            self.scanout_refresh_fail.load(Ordering::Relaxed),
        );
        // Live proof that ctrl completions are reaching the real IRQ/DPC
        // path; these atomics otherwise become visible only at teardown.
        crate::diag::record_named_bytes(
            b"IrqN",
            crate::ddi::interrupt::INT_ROUTINE_COUNT.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"DpcN",
            crate::ddi::interrupt::DPC_ROUTINE_COUNT.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"RfDone",
            crate::virtio::gpu::ASYNC_CTRL_COMPLETE_COUNT.load(Ordering::Relaxed),
        );

        crate::diag::record_named_bytes(b"VsCnt", self.vsync_count.load(Ordering::Relaxed));
        crate::diag::record_named_bytes(b"VsEn", self.vsync_enabled.load(Ordering::Relaxed));
        crate::diag::record_named_bytes(
            b"SaCnt",
            crate::ddi::VIDPN_SOURCE_ADDRESS_COUNT.load(Ordering::Relaxed),
        );
        let primary_address = self.last_primary_address.load(Ordering::Relaxed);
        crate::diag::record_named_bytes(b"SaLo", primary_address as u32);
        crate::diag::record_named_bytes(b"SaHi", (primary_address >> 32) as u32);
        crate::diag::record_named_bytes(
            b"AsSub",
            crate::virtio::gpu::ASYNC_SUBMIT_COUNT.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"AsDone",
            crate::virtio::gpu::ASYNC_COMPLETE_COUNT.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"WfDone",
            crate::virtio::gpu::WDDM_FENCE_FROM_DPC.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"WtOut",
            crate::virtio::gpu::FENCE_WAIT_TIMEOUTS.load(Ordering::Relaxed),
        );
        // R604: the condition split out of WtOut. Mirrored here rather than only
        // into the CollectDbgInfo report, because otherwise the only way to read
        // it would be to provoke a TDR — and the point of the split is that this
        // condition is NOT a host fault and should be visible without one.
        // R615: fences discarded by an engine reset / preemption / TDR epoch.
        // Mirrored from HERE, a PASSIVE site, because all three abandon call
        // sites run at DISPATCH_LEVEL and a registry write above PASSIVE is a
        // never-violate rule.
        crate::diag::record_named_bytes(
            b"AbnDrop",
            crate::ddi::ABANDONED_FENCES.load(Ordering::Relaxed),
        );
        // R619/k-gputransport-14: a nonzero value means someone tried to move the
        // VidMm window partition after offsets had been issued.
        crate::diag::record_named_bytes(
            b"WnRcf",
            crate::virtio::gpu::WINDOW_RECONFIG_REFUSED.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"WtTbl",
            crate::virtio::gpu::FENCE_WAIT_TABLE_FULL.load(Ordering::Relaxed),
        );
        // R614: mint sites whose `// SAFETY:` IRQL claim was false, packed as
        // (count << 8) | last_irql. Mirrored from HERE for the same reason
        // AbnDrop is: `PassiveLevel::assume` runs at whatever IRQL its caller
        // was at — that is the point of it — and a registry write above PASSIVE
        // is a never-violate rule. Must read 0; see `crate::irql`.
        crate::diag::record_named_bytes(
            b"IrqlBad",
            crate::irql::IRQL_ASSUME_BAD.load(Ordering::Relaxed),
        );
        // ROADMAP defect 0ac: `with_virtio` entries whose `self` did not survive
        // the spinlock acquire. Mirrored from HERE for the same reason as the
        // two above — the tripwire itself runs at DISPATCH. Must read 0.
        crate::diag::record_named_bytes(b"WvTorn", super::WITH_VIRTIO_TORN.load(Ordering::Relaxed));
        crate::diag::record_named_bytes(
            b"CtOut",
            crate::virtio::gpu::CTRL_TIMEOUT_COUNT.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"DpHit",
            crate::virtio::gpu::DMA_POOL_HITS.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"DpMis",
            crate::virtio::gpu::DMA_POOL_MISSES.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"DpDrp",
            crate::virtio::gpu::DMA_POOL_DROPS.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"DpByt",
            crate::virtio::gpu::DMA_POOL_CACHED_BYTES.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"DmStl",
            crate::ddi::DMA_STALE_SKIP_COUNT.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"QfRet",
            crate::virtio::gpu::QUEUE_FULL_RETRIES.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"IfHi",
            crate::virtio::gpu::INFLIGHT_HIGH_WATER.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"PkHi",
            crate::virtio::gpu::PARKED_HIGH_WATER.load(Ordering::Relaxed),
        );
        // R505: the nine deferred-programming refusal counters. Flushed from
        // HERE — a PASSIVE, already-throttled site — and never from the refusal
        // path itself, which would be a registry write per refused frame.
        crate::ddi::display::record_scanout_reject_counters();
        crate::ddi::record_present_handoff_telemetry();
    }

    /// Scanout refresh implementation. The caller holds `scanout_mutex`, which
    /// prevents a matching WDDM allocation from being unbound/unref'd between
    /// the liveness check and control-queue submission.
    fn queue_active_scanout_refresh_locked(&self, lock: &ScanoutGuard<'_>) -> ScanoutRefreshQueue {
        use core::sync::atomic::Ordering;

        // This is the production path only. Diagnostic fills issue their own
        // explicit one-shot flushes; never query the registry on every frame.
        let resource_id = self.active_scanout_resource.load(Ordering::Acquire);
        let wh = self.active_scanout_wh.load(Ordering::Relaxed);
        // A newer present may publish while we sample the companion field.
        // Retry from the worker rather than combine two primary identities.
        if self.active_scanout_resource.load(Ordering::Acquire) != resource_id {
            return ScanoutRefreshQueue::Busy;
        }

        // THE CENSUS the ownership gate below acts on. It was count-only until
        // 22.22.217.0 — the two earlier attempts to gate on it were built,
        // deployed and measured not to move the artifact, but the 2×2 factorial
        // then measured WHY: both ran at a cadence where a different producer
        // dominated, and the surplus-flush population is real (~30 % of
        // publishes, 43.4 % of them black, against 2.6 % for first reads).
        // Keep the counters: the rate is still how far the bind path lags the
        // present path, and `RfWait` minus `OgIdn` is what the gate refused.
        let armed = self.pending_refresh_resource.load(Ordering::Acquire);
        if armed != 0 && armed != resource_id {
            let n = self
                .scanout_refresh_unbound
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1);
            if n == 1 || (n % 600) == 0 {
                crate::diag::record_named_bytes(b"RfWait", n);
                crate::diag::record_named_bytes(b"RfWantR", armed);
                crate::diag::record_named_bytes(b"RfHaveR", resource_id);
            }
        }
        let width = (wh >> 32) as u32;
        let height = wh as u32;
        if !self.display_half() || resource_id == 0 || width == 0 || height == 0 {
            // Nothing is bound, so nothing can be read — and a presentation lease
            // that can never be satisfied would wedge the flip queue into a TDR.
            // Loud (`LsCanc`), exact, and not a timeout: at this point the host
            // holds no scan-out at all.
            self.release_all_scanout_leases(LeaseEnd::Cancelled);
            return ScanoutRefreshQueue::Unavailable;
        }
        let live = self
            .with_virtio(|v| v.resource_is_live(resource_id))
            .unwrap_or(false);
        if !live {
            // The resource is gone, so no read of it exists or can be issued.
            self.release_all_scanout_leases(LeaseEnd::Cancelled);
            // Only clear the identity we sampled. A newer Windows primary may
            // have been published concurrently by the Present path.
            let _ = self.active_scanout_resource.compare_exchange(
                resource_id,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            if self
                .host_bound_scanout_resource
                .compare_exchange(resource_id, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                crate::diag::record_named_bytes(b"ScDead", resource_id);
            }
            return ScanoutRefreshQueue::Unavailable;
        }
        if self.scanout_flush_inflight.load(Ordering::Acquire) != 0 {
            return ScanoutRefreshQueue::Busy;
        }

        // KEEP THE REFUSAL, and make it loud. Deleting the whole arm -- as the
        // original finding proposed -- would let control fall through to
        // `resource_flush_async` and issue RESOURCE_FLUSH against a resource
        // the host has never bound to scanout 0, reachable on exactly the
        // StopDevice/StartDevice path that `init_hpd` creates.
        if self.host_bound_scanout_resource.load(Ordering::Acquire) != resource_id {
            crate::diag::record_named_bytes(b"RfUnb", resource_id);
            // The host is bound to something else (or to nothing): this refresh
            // will never be issued, so no lease may go on waiting for it.
            self.release_all_scanout_leases(LeaseEnd::Cancelled);
            return ScanoutRefreshQueue::Unavailable;
        }

        // ── THE OWNERSHIP GATE (ROADMAP defect 0ab-B, D2) ────────────────────
        //
        // Placed HERE, after every "is there a live, host-bound scan-out at
        // all?" arm: those refuse for a different reason and must keep their own
        // outcome, and this gate's whole argument is that a BETTER-TIMED publish
        // of this binding exists. It runs before the in-flight CAS, so a drop
        // takes no transport state.
        //
        // IDENTITY: the armed frame is not the bound one. Publishing the ACTIVE
        // buffer cannot show the armed buffer's content, so this read can only
        // re-publish a binding the app may already have reclaimed. The armed
        // buffer's own bind edge publishes it when it binds; if its flip was
        // coalesced away, no publish of it is possible at all. Clearing the
        // armed id is what keeps the drop self-healing: the next identity-free
        // edge flushes whatever is bound.
        if armed != 0 && armed != resource_id {
            self.pending_refresh_resource.store(0, Ordering::Release);
            crate::ddi::scanout_trace::note_ownership_drop_identity();
            return ScanoutRefreshQueue::Dropped;
        }
        // EPOCH: this binding generation was already published AND a newer
        // presentation has been minted, so the successor's bind edge owns the
        // next publish and a re-read now races the app's clear. The predicate
        // (including the `tracked` operand that keeps the desktop path out of
        // it) lives in `helios_kmd_logic` with its host tests.
        if surplus_republish(
            self.scanout_epoch_tracked.load(Ordering::Acquire) != 0,
            self.scanout_read_epoch.load(Ordering::Acquire),
            self.scanout_bound_epoch.load(Ordering::Acquire),
            self.scanout_present_epoch.load(Ordering::Acquire),
        ) {
            self.pending_refresh_resource.store(0, Ordering::Release);
            crate::ddi::scanout_trace::note_ownership_drop_epoch();
            return ScanoutRefreshQueue::Dropped;
        }

        if self
            .scanout_flush_inflight
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return ScanoutRefreshQueue::Busy;
        }

        // UNSAMPLED, and recorded at the exact site that names the resource:
        // this is the guest-side counterpart of QEMU's
        // `virtio_gpu_cmd_res_flush` histogram, so the two can be compared
        // without inferring anything. `RfRid` cannot answer this — it is behind
        // `sample_tick`, and a stale `RfRid` reading like a smoking gun for 26 s
        // while binds rotated is one of the four wrong mechanisms in the
        // handoff. Counted BEFORE the submit: the question is which resource
        // the flush path selected, not which submissions the ring accepted.
        crate::ddi::scanout_trace::FLUSH_HISTOGRAM.note(resource_id);
        // THE READ LEDGER ISSUE (D4a, FIX-DESIGN-d4a.md §3.2). Bumped beside
        // the histogram, under the same `scanout_mutex` hold, BEFORE the
        // enqueue attempt: from here the invariant is one retirement per
        // issue, carried by the token below — its `complete` on the drain, or
        // its `Drop` on every path that never reaches completion (including
        // the enqueue-failure arm underneath, which drops the token inside
        // `resource_flush_async`).
        let ledger_slot = self.read_ledger.issue(resource_id);
        // THE LEASE TOKEN. Snapshot the presentation the host is bound to right
        // now: when this exact command's response returns, the host has read
        // that buffer, and therefore every presentation at or below that epoch.
        // Snapshotted at ISSUE, not at completion — the read happens somewhere
        // inside the command, and only the issue point is provably after the
        // binding was published.
        let covers_epoch = self.scanout_bound_epoch.load(Ordering::Acquire);
        crate::ddi::scanout_trace::note_lease_read_queued();
        let result = crate::virtio::ctrl::resource_flush_async(
            lock.passive(),
            self,
            resource_id,
            width,
            height,
            NonNull::from(&self.scanout_flush_inflight),
            NonNull::from(&self.scanout_refresh_fail),
            // SAFETY: hpd_event is an embedded, in-place initialized KEVENT and
            // the adapter outlives every transport entry that holds this pointer.
            unsafe { NonNull::new_unchecked(self.hpd_event.get()) },
            crate::virtio::ScanoutFlushToken::new(self, covers_epoch, resource_id, ledger_slot),
        );
        if result.is_err() {
            self.scanout_flush_inflight.store(0, Ordering::Release);
            // The command never reached the ring, so the read it would have
            // performed does not exist. End exactly the epochs it named — not
            // more — so a later presentation stays gated.
            self.end_scanout_leases_through(covers_epoch, LeaseEnd::Cancelled);
            let failed = self
                .scanout_refresh_fail
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1);
            if failed == 1 || (failed % 60) == 0 {
                crate::diag::record_named_bytes(b"RfRid", resource_id);
                crate::diag::record_named_bytes(b"RfFail", failed);
            }
            return ScanoutRefreshQueue::Failed;
        }

        // The armed identity is satisfied: this flush carries that exact frame.
        // Clearing it under the lock keeps a later identity-free HERF edge from
        // being gated on an id that has already been published.
        self.pending_refresh_resource.store(0, Ordering::Release);
        // Count the refresh here (under the lock, where the identity is stable);
        // the TELEMETRY SNAPSHOT is taken by the caller AFTER the lock is
        // released — see `queue_active_scanout_refresh` (R318).
        self.scanout_refresh_count.fetch_add(1, Ordering::Relaxed);
        ScanoutRefreshQueue::Queued
    }

    /// Retire one exact Windows allocation/resource identity from scanout 0.
    ///
    /// Returns false only when the mandatory host unbind could not be
    /// confirmed. The caller must then retain the host resource until device
    /// teardown rather than RESOURCE_UNREF a blob QEMU may still sample.
    pub(crate) fn retire_scanout_allocation(
        &self,
        passive: PassiveLevel,
        allocation_handle: usize,
        resource_id: u32,
    ) -> bool {
        self.with_scanout_lifecycle(passive, |lock| {
            self.retire_scanout_allocation_locked(lock, allocation_handle, resource_id)
        })
    }

    /// Body of [`Self::retire_scanout_allocation`]. Separate so the critical
    /// section's contents are a named function taking the lock token rather than
    /// an inline closure; the generated critical section is identical.
    fn retire_scanout_allocation_locked(
        &self,
        lock: &ScanoutGuard<'_>,
        allocation_handle: usize,
        resource_id: u32,
    ) -> bool {
        use core::sync::atomic::Ordering;

        // SetVidPnSourceAddress can publish its exact KMD allocation handle
        // at DIRQL for later PASSIVE processing. DestroyAllocation owns the
        // same exact pointer; cancel it while serialized with the worker's
        // swap-and-dereference before the Box can be freed.
        if allocation_handle != 0
            && self
                .pending_vidpn_allocation
                .compare_exchange(allocation_handle, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            // We just cancelled a deferred SetVidPnSourceAddress, so the HPD
            // worker's swap() will now yield 0 and
            // process_deferred_vidpn_source_address will return None -
            // meaning nobody reaches any of the ten sites that clear
            // `vidpn_programming`. A gate left at 1 makes vsync_dpc_routine
            // early-return on every 16 ms tick before it increments
            // vsync_count, so CRTC_VSYNC stops, dxgkrnl never retires the
            // queued flip, and it therefore never issues the next
            // SetVidPnSourceAddress that would re-arm the gate. The display
            // is wedged for the rest of the boot.
            //
            // Both conditions below are load-bearing. SetVidPnSourceAddress
            // runs at DIRQL and does NOT take the scanout lifecycle lock, so
            // a NEWER program can raise the gate immediately after our CAS -
            // re-read `pending` and only act while it is still 0. And
            // compare_exchange(1, 0) rather than store(0) keeps us from
            // clearing a gate we never observed set, which is the stale-clear
            // window that would let the VSync DPC report a primary address
            // the host has not sampled yet.
            if self.pending_vidpn_allocation.load(Ordering::Acquire) == 0
                && self.cancel_programming_gate()
            {
                crate::diag::record_named_bytes(b"VpCncl", allocation_handle as u32);
            }
        }
        if resource_id == 0 {
            return true;
        }
        // Freeze the DISPATCH bind producer before resolving the final host
        // selection. The PASSIVE worker is already excluded by `scanout_mutex`.
        // Any SET issued before this point is ahead of the pure-query FIFO
        // barrier below; no SET can be inserted between that proof and a
        // necessary scanout-disable.
        let begin = self.with_virtio(|v| v.begin_scanout_resource_retire(resource_id));
        let Ok((accepted_before, host_before)) = begin else {
            return false;
        };
        let wire_before = self.scanout_bind_wire_seq.load(Ordering::Acquire);
        // D4a (FIX-DESIGN-d4a.md §3.1): the ledger slot dies with its backing
        // allocation — reclaimed now if no read is in flight, else pinned
        // (retire-wanted) until the in-flight token's retirement equalizes the
        // counters. BEFORE the active/host-bound early returns below: a slot
        // can outlive this resource's turn on scanout 0, and resource ids are
        // never recycled, so an unreclaimed slot is a leak of one of eight.
        self.read_ledger.note_alloc_retired(resource_id);

        let retired = (|| {
            // A successful pure-query response proves every earlier async SET
            // reached a terminal host response. Successful SETs advanced the
            // host-accepted selection in `drain_used`; failed SETs deliberately
            // leave that selection unchanged. This is the missing distinction
            // between "A was once accepted" and "A is still final".
            let final_host = if needs_fifo_barrier(wire_before, accepted_before) {
                if crate::virtio::ctrl::ctrl_fifo_barrier(lock.passive(), self).is_err() {
                    crate::diag::record_named_bytes(b"ScRet", 0xB);
                    crate::diag::record_named_bytes(b"ScDead", resource_id);
                    return false;
                }
                self.with_virtio(|v| v.host_accepted_scanout_bind())
                    .map(|(_, resource)| resource)
                    .unwrap_or(host_before)
            } else {
                host_before
            };

            if !needs_disable(resource_id, final_host) {
                // A newer successful bind (or an earlier disable) is the lifetime
                // barrier. Sending SET(0) here would instead queue it BEHIND that
                // newer bind and permanently blank scanout. Clear only guest views
                // that still name A; a B application wholly before this closure is
                // preserved, and one after it republishes B.
                self.with_wddm_notify_lock(|_| {
                    let host_was_a = self
                        .host_bound_scanout_resource
                        .compare_exchange(resource_id, 0, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok();
                    let active_was_a = self
                        .active_scanout_resource
                        .compare_exchange(resource_id, 0, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok();
                    if active_was_a {
                        self.active_scanout_wh.store(0, Ordering::Release);
                    }
                    if self
                        .pending_refresh_resource
                        .compare_exchange(resource_id, 0, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        self.scanout_refresh_pending.store(0, Ordering::Release);
                    }
                    if host_was_a || active_was_a {
                        let reason = if final_host == 0 {
                            LeaseEnd::Cancelled
                        } else {
                            LeaseEnd::Superseded
                        };
                        if final_host == 0 {
                            self.scanout_epoch_tracked.store(0, Ordering::Release);
                        }
                        let _ = self.end_scanout_leases_through(
                            self.scanout_bound_epoch.load(Ordering::Acquire),
                            reason,
                        );
                    }
                });
                crate::diag::record_named_bytes(b"ScRet", resource_id);
                return true;
            }

            // A is still the final host selection, so disable scanout while the
            // global fast-bind gate is held. Its response is both the host-reader
            // lifetime barrier and the newest bind sequence; no newer B can be
            // trapped in FIFO order A -> B -> 0.
            let unbound = crate::virtio::ctrl::set_scanout_blob(
                lock.passive(),
                self,
                0,
                0,
                0,
                0,
                0,
                0,
            );
            let Ok(unbind_seq) = unbound else {
                crate::diag::record_named_bytes(b"ScRet", 0xE);
                crate::diag::record_named_bytes(b"ScDead", resource_id);
                return false;
            };
            let _ = self.with_virtio(|v| v.note_host_accepted_scanout_bind(unbind_seq, 0));

            self.with_wddm_notify_lock(|_| {
                if self.adopt_scanout_bind_seq(unbind_seq) {
                    self.host_bound_scanout_resource.store(0, Ordering::Release);
                    self.active_scanout_resource.store(0, Ordering::Release);
                    self.active_scanout_wh.store(0, Ordering::Release);
                    self.scanout_epoch_tracked.store(0, Ordering::Release);
                    let _ = self.end_scanout_leases_through(
                        self.scanout_bound_epoch.load(Ordering::Acquire),
                        LeaseEnd::Cancelled,
                    );
                    self.scanout_refresh_pending.store(0, Ordering::Release);
                    self.pending_refresh_resource.store(0, Ordering::Release);
                }
            });
            crate::diag::record_named_bytes(b"ScRet", resource_id);
            true
        })();

        // Paired structurally with `begin_scanout_resource_retire`: every exit
        // from the decision closure reaches this epilogue. Wake the retained
        // newest WDDM handle so the normal worker can bind it after the lifecycle
        // mutex is released.
        let _ = self.with_virtio(|v| v.finish_scanout_resource_retire());
        self.signal_hpd();
        retired
    }
}
