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

use wdk_sys::ntddk::KeSetEvent;

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
}

impl AdapterContext {
    /// Mark already-completed scanout contents dirty. The normal copied path
    /// does this from the ring-1 GPU-completion DPC; the direct-primary
    /// zero-copy case has no KMD GPU submission, so SetVidPn uses this after
    /// Windows has handed it the completed primary.
    pub fn request_scanout_refresh(&self) {
        self.scanout_refresh_pending.store(1, Ordering::Release);
        // SAFETY: hpd_event is initialized in place and stable for the adapter
        // lifetime; KeSetEvent(Wait=FALSE) is legal through DISPATCH_LEVEL.
        unsafe { KeSetEvent(self.hpd_event.get(), 0, 0) };
    }

    /// Remember the blob currently selected for scanout 0. PASSIVE callers bind
    /// it via SET_SCANOUT_BLOB first, then publish it here for dirty-driven
    /// RESOURCE_FLUSH after completed copies.
    pub fn remember_scanout_blob(&self, resource_id: u32, width: u32, height: u32) {
        let wh = ((width as u64) << 32) | height as u64;
        self.active_scanout_wh
            .store(wh, core::sync::atomic::Ordering::Release);
        self.active_scanout_resource
            .store(resource_id, core::sync::atomic::Ordering::Release);
        // Existing callers invoke this only after a synchronous successful
        // SET_SCANOUT_BLOB (diagnostic/bootstrap paths).
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
        let width = (wh >> 32) as u32;
        let height = wh as u32;
        if !self.display_half() || resource_id == 0 || width == 0 || height == 0 {
            return ScanoutRefreshQueue::Unavailable;
        }
        let live = self
            .with_virtio(|v| v.resource_is_live(resource_id))
            .unwrap_or(false);
        if !live {
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
            return ScanoutRefreshQueue::Unavailable;
        }

        if self
            .scanout_flush_inflight
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return ScanoutRefreshQueue::Busy;
        }

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
        );
        if result.is_err() {
            self.scanout_flush_inflight.store(0, Ordering::Release);
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

        let was_active = self
            .active_scanout_resource
            .compare_exchange(resource_id, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        let was_host_bound =
            self.host_bound_scanout_resource.load(Ordering::Acquire) == resource_id;
        if !was_active && !was_host_bound {
            return true;
        }
        if !was_host_bound {
            // The retiring allocation was only a newer desired candidate;
            // a different resource is still bound on the host. Clearing
            // the candidate above is sufficient. Sending scanout-disable
            // here would blank the unrelated Windows-selected primary.
            crate::diag::record_named_bytes(b"ScRet", resource_id);
            return true;
        }

        // QEMU's virtio-gpu SET_SCANOUT_BLOB contract treats resource_id=0
        // as scanout disable before any resource lookup. Because this
        // synchronous command is queued after all earlier async scanout
        // commands, its response is the lifetime barrier before UNREF.
        let unbound =
            crate::virtio::ctrl::set_scanout_blob(lock.passive(), self, 0, 0, 0, 0, 0, 0).is_ok();
        if !unbound {
            crate::diag::record_named_bytes(b"ScRet", 0xE);
            crate::diag::record_named_bytes(b"ScDead", resource_id);
            return false;
        }

        self.host_bound_scanout_resource.store(0, Ordering::Release);
        self.scanout_refresh_pending.store(0, Ordering::Release);
        crate::diag::record_named_bytes(b"ScRet", resource_id);

        // The DISPATCH Present path can publish a newer exact Windows
        // primary without taking this PASSIVE lock. If that happened while
        // the old resource was retiring, make the new identity drive a
        // fresh bind instead of treating the just-issued unbind as final.
        if self.active_scanout_resource.load(Ordering::Acquire) != 0 {
            self.request_scanout_refresh();
        }
        true
    }
}
