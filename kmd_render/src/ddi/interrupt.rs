//! ISR / DPC DDIs — the C3/M3.4 interrupt-driven used-ring drain.
//!
//! The virtio-gpu device is line-based INTx (`MSISupported=0`), i.e. *level*-
//! triggered: it asserts the shared INTx line when it pushes used-ring entries
//! and keeps it asserted until the driver reads the read-to-clear virtio
//! ISR-status register. The ISR reads that register (deasserting the line),
//! claims the interrupt, and queues the DPC via `DxgkCbQueueDpc`; the DPC
//! drains the used ring under the device spinlock (`VirtioGpu::drain_used` —
//! signaling sync/fence KEVENT waiters) and then completes every WDDM
//! submission whose venus watermark has been reached
//! (`DXGK_INTERRUPT_DMA_COMPLETED` at DIRQL via `signal_dma_completed`).
//!
//! IRQL: the ISR runs at the device's DIRQL — no allocations, no spinlocks, no
//! pageable calls; it touches only the lock-free published ISR-status VA and
//! the saved dxgkrnl callback table. The DPC runs at DISPATCH_LEVEL.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::adapter::AdapterContext;
use crate::dxgk::*;

// ── DIRQL/DISPATCH-safe instrumentation ──────────────────────────────────────
// `diag::record` is PASSIVE-only (RtlWriteRegistryValue), so the ISR/DPC cannot
// touch the registry ring. These atomics are incremented here at DIRQL/DISPATCH
// and dumped into the PASSIVE diag ring at DxgkDdiDestroyDevice.
pub static INT_ROUTINE_COUNT: AtomicU32 = AtomicU32::new(0);
pub static DPC_ROUTINE_COUNT: AtomicU32 = AtomicU32::new(0);
pub static CONTROL_INT_COUNT: AtomicU32 = AtomicU32::new(0);

/// Ask dxgkrnl to run the normal completion DPC after PASSIVE-side lifecycle
/// code changed a WDDM wait predicate.  The caller has already preserved the
/// producer watermark under `wddm_notify_lock`; queuing the DPC only provides
/// the prompt that lets it observe the now-ready head without waiting for an
/// unrelated virtio interrupt.
pub(crate) fn request_wddm_completion_dpc(adapter: &AdapterContext) {
    let Some(dxgkrnl) = adapter.dxgkrnl_opt() else {
        return;
    };
    let Some(queue_dpc) = dxgkrnl.DxgkCbQueueDpc else {
        return;
    };
    // SAFETY: the callback table belongs to this live adapter, and QueueDpc is
    // callable at PASSIVE/DISPATCH/DIRQL. It does not take wddm_notify_lock.
    unsafe { queue_dpc(dxgkrnl.DeviceHandle) };
}

/// Drain used control-queue entries, apply any completed DISPATCH-level scan-out
/// bind, and retire any WDDM fences whose Venus watermark is now complete.
/// Normally called from the device DPC; the scanout worker may also call it at
/// PASSIVE_LEVEL as a bounded fallback while one fire-and-forget RESOURCE_FLUSH
/// is outstanding. Keeping fence retirement here prevents an opportunistic drain
/// from consuming a Venus completion without notifying VidSch.
///
/// The bind application lives HERE rather than in `drain_used` because of the
/// lock order — see the comment on it below.
pub(crate) fn drain_used_and_complete(adapter: &AdapterContext) {
    let _ = adapter.with_virtio(|v| v.drain_used());

    // A producer completion may have made the one deferred fast bind safe.
    // Promotion and sequence minting share this virtio-lock hold, so the host
    // sees `SET_SCANOUT_BLOB` only after the exact carried boundary retired and
    // its bookkeeping sequence still equals control-FIFO order.
    let deferred_fast_bind = adapter.with_virtio(|v| {
        v.service_deferred_scanout_bind(|resource_id| adapter.mint_scanout_bind_seq(resource_id))
    });
    match deferred_fast_bind {
        Ok(Some(crate::virtio::FastBindDispatch::Queued)) => {
            crate::ddi::scanout_trace::note_fast_bind_enqueued()
        }
        Ok(Some(crate::virtio::FastBindDispatch::Busy)) => {
            crate::ddi::scanout_trace::note_fast_bind_busy()
        }
        Ok(Some(crate::virtio::FastBindDispatch::Failed)) => {
            crate::ddi::scanout_trace::note_fast_bind_error()
        }
        Ok(Some(crate::virtio::FastBindDispatch::Deferred))
        | Ok(Some(crate::virtio::FastBindDispatch::Handled))
        | Ok(Some(crate::virtio::FastBindDispatch::Superseded))
        | Ok(None)
        | Err(_) => {}
    }

    if let Some(request) = adapter
        .with_virtio(|v| v.take_fast_failure_wake())
        .ok()
        .flatten()
    {
        // A worker held behind this fast request is the ordinary synchronous
        // recovery path. Release only the exact waiter and wake it on failure.
        let _ = adapter.with_virtio(|v| v.release_fast_owned_worker(request));
        adapter.signal_hpd();
    }

    // The synchronous VidPN worker uses the identical producer boundary.  Its
    // handle remains in the existing pending slot; waking it only after this
    // exact boundary retires prevents `set_scanout_blob` from racing Venus.
    if adapter
        .with_virtio(|v| v.take_ready_worker_scanout_bind())
        .unwrap_or(false)
    {
        adapter.signal_hpd();
    }

    // ── The DISPATCH fast bind's application (ROADMAP defect 0ab-C, D1(ii)) ──
    //
    // A SECOND, short `with_virtio`, and the separation is the point: the drain
    // above holds `virtio_lock`, while applying a bind ends in a flush arm that
    // needs `wddm_notify_lock` — the driver's order is notify → virtio (see
    // `adapter/locks.rs` and `end_scanout_leases_through`), and inverting it is
    // a DIRQL deadlock with no attributable bugcheck. So the drain stashes
    // VALUES and this frame, holding no transport lock, applies them.
    let fast_bind = adapter
        .with_virtio(|v| v.take_completed_bind())
        .ok()
        .flatten();
    let fast_terminal_request = fast_bind.map(crate::virtio::gpu::completed_request);
    adapter.with_wddm_notify_lock(|guard| {
        // `drain_used` runs under only `virtio_lock`, so a rejected tagged
        // submit can only invalidate its stream there.  Discharge the stale
        // scheduler/scanout waits here, under the required notify→virtio order;
        // they remain gated on their ordinary wire watermark rather than being
        // mistaken for a successful stream retirement.
        let _ = guard.with_virtio(|order, v| v.discharge_dead_present_stream_waits(order));

        // Apply the accepted bind and arm its flush edge as ONE notify-ordered
        // transition.  In particular, `active_scanout_resource` is not exposed
        // to PRESENT classification until its carried boundary is in the
        // refresh state.  Otherwise a same-resource re-present could insert W2
        // after seeing the active identity, then this older bind insert W1;
        // `RefreshState::note(W1, ready)` correctly treats a ready *newer*
        // marker as a replacement and would incorrectly clear W2.  The notify
        // lock supplies the required W1-before-W2 order.
        //
        // This block is atomics plus `KeSetEvent(Wait = FALSE)` only: it remains
        // DISPATCH-safe and takes no transport lock until `arm_bind_refresh`
        // reaches its witnessed notify -> virtio critical section below.
        //
        // The bind edge uses the boundary the FLIP carried (D1(i)) rather than a
        // sample taken now — the whole point of the 22.22.217.0 ordering,
        // preserved by carrying the mark through the in-flight entry. The
        // notification scope performs both the decision and pending-identity
        // publication, so a later PRESENT cannot replace this bind edge after it
        // has selected its exact resource.
        //
        // No liveness re-check here on purpose: the flush executor re-validates
        // (`resource_is_live` + the `RfUnb` arm) before it issues any read, so a
        // resource that died between the bind and now self-heals exactly as
        // today's stale states do — and host-side FIFO means our bind always
        // precedes any unref of the same resource.
        if let Some(bind) = fast_bind {
            if adapter.adopt_scanout_bind_seq(bind.seq) {
                // The wire-order guard belongs inside the same notify-ordered
                // transition as every identity/epoch publication.
                // Sampled under the same ordering scope as the active-identity
                // publication and boundary insertion.  A rebind to a DIFFERENT
                // resource ends every older epoch's lease — the control queue is
                // FIFO, so a returned SET_SCANOUT_BLOB proves every earlier flush
                // completed.
                let previous = adapter.host_bound_scanout_resource.load(Ordering::Acquire);
                let superseded = previous != 0 && previous != bind.resource_id;
                adapter.remember_scanout_blob(
                    bind.resource_id,
                    (bind.wh >> 32) as u32,
                    bind.wh as u32,
                );
                adapter.publish_bound_epoch(bind.present_epoch, superseded);
                adapter.publish_bound_primary(bind.primary_address);
                crate::ddi::scanout_timeline::note(
                    crate::ddi::scanout_timeline::kind::BIND_APPLY,
                    crate::ddi::scanout_timeline::flag::SUCCESS,
                    bind.present_epoch,
                    bind.carried_watermark,
                    bind.seq,
                    bind.resource_id,
                    previous,
                );
                crate::ddi::scanout_trace::note_fast_bind_applied();

                let (ready, carried) = adapter.arm_bind_refresh_locked(
                    guard,
                    bind.resource_id,
                    bind.carried_watermark,
                );
                if carried {
                    crate::ddi::scanout_trace::note_bind_watermark_carried();
                } else {
                    crate::ddi::scanout_trace::note_bind_watermark_sampled();
                }
                crate::ddi::scanout_trace::note_bind_refresh(ready);
                if ready {
                    adapter.request_scanout_refresh_for_locked(guard, bind.resource_id);
                }
            } else {
                // The host accepted this SET, but a later bind's bookkeeping
                // already owns the displayed identity. It cannot arm a flush
                // for a resource the host has moved on from; resolve only this
                // full request's transaction, never a resource-wide guess.
                crate::ddi::scanout_trace::note_fast_bind_late();
                let _ = guard.with_virtio(|_, v| {
                    v.cancel_publication_exact(bind.resource_id, bind.present_epoch)
                });
            }
        }

        // Carries the armed resource through: the refresh must flush the frame
        // its marker belonged to, not whatever is bound when the worker runs.
        let refresh_ready = guard
            .with_virtio(|o, v| v.take_ready_scanout_refresh(o))
            .ok()
            .flatten();
        if let Some(marker) = refresh_ready {
            crate::ddi::scanout_timeline::note(
                crate::ddi::scanout_timeline::kind::REFRESH_PROMOTE,
                crate::ddi::scanout_timeline::flag::READY,
                adapter.scanout_bound_epoch.load(Ordering::Acquire),
                marker.boundary(),
                0,
                marker.resource_id(),
                0,
            );
            adapter.request_scanout_refresh_for_locked(guard, marker.resource_id());
        }

        // One at a time, so a failed notification can put its fence back. The
        // old batch popped up to eight entries BEFORE attempting any delivery
        // and then discarded every status with `let _ =`. A failed
        // DxgkCbSynchronizeExecution therefore left the fence in no queue at
        // all, with the completed watermark still below it and no counter
        // recording the loss (DMA_SYNC_STATUS_LOW/DMA_SYNC_RET are
        // last-value-wins, so a later successful notify erased the only trace).
        // On an idle desktop VidSch then never sees that fence retire and
        // escalates to TDR.
        loop {
            // No lease watermark is read here any more. Until 22.22.217.0 a
            // submission also waited for the host to READ the buffer it
            // published, and a blocked head ran a liveness pump that asked the
            // display worker for that read. Both are gone: the withholding was
            // measured inert against the black frames (the 2×2 factorial), and
            // the pump was itself a black-frame producer — a flush issued to
            // satisfy a lease republishes whatever is bound NOW, which is an
            // older buffer the app may already have reclaimed (measured: the
            // 1–3 ms bucket 0.6 % → 5.0 %, duplicate-content reads 3.5 % → 9.1 %
            // on 22.22.215.0). The ordering the frames actually need is the
            // ownership gate on the flush executor, not a completion policy.
            let taken = guard
                .with_virtio(|o, v| v.take_one_ready_wddm(o))
                .unwrap_or(crate::virtio::WddmTake::Empty);
            let ready = match taken {
                crate::virtio::WddmTake::Ready(ready) => ready,
                crate::virtio::WddmTake::Empty | crate::virtio::WddmTake::BlockedOnProducer => {
                    break;
                }
            };
            let Some(dxgkrnl) = adapter.dxgkrnl_opt() else {
                // No callback table: we cannot deliver and must not drop it.
                let _ = guard.with_virtio(|o, v| v.requeue_wddm_front(o, ready));
                break;
            };
            // SAFETY: the WDDM notification lock is held; the helper
            // raises to DIRQL for the callback without re-locking.
            let status = unsafe {
                super::submit_command::signal_dma_completed(guard, dxgkrnl, ready.fence())
            };
            if status == STATUS_SUCCESS {
                let terminal_prefix = ready.terminal_prefix();
                ready.delivered();
                if let Some(prefix) = terminal_prefix {
                    // Consume the complete same-stream WindowedBlt prefix
                    // only after dxgkrnl accepted the DMA completion. A
                    // callback failure requeues the WDDM entry without
                    // having to reconstruct terminal membership.
                    let _ = guard.with_virtio(|_o, v| {
                        v.consume_windowed_blt_terminal_prefix(prefix)
                    });
                }
                continue;
            }
            super::submit_command::DMA_NOTIFY_FAILS.fetch_add(1, Ordering::Relaxed);
            let _ = guard.with_virtio(|o, v| v.requeue_wddm_front(o, ready));
            // Retry on the next DPC rather than spinning here: the failure is a
            // stop/rebalance window, so give dxgkrnl a chance to make progress.
            if let Some(queue_dpc) = dxgkrnl.DxgkCbQueueDpc {
                // SAFETY: DxgkCbQueueDpc is callable at <= DIRQL with a valid
                // DeviceHandle; we hold the notify lock, which it does not take.
                unsafe { queue_dpc(dxgkrnl.DeviceHandle) };
            }
            break;
        }
    });
    if let Some(request) = fast_terminal_request {
        // A worker deliberately held behind this accepted fast bind can now
        // rerun/re-evaluate after either an applied or stale terminal outcome.
        // A stale accepted bind must not leave its exact waiter pinned forever.
        let _ = adapter.with_virtio(|v| v.release_fast_owned_worker(request));
        adapter.signal_hpd();
    }
}

/// `DxgkDdiInterruptRoutine` — runs at the device's DIRQL; returns TRUE if the
/// interrupt was ours.
//
// Read the ISR-status register (which DEASSERTS the level-triggered line), and
// if a bit was pending, claim the interrupt and queue the DPC. Without the
// read-to-clear, the line stays asserted and Windows' interrupt-storm detector
// disables the adapter (observed pre-fix: ~10000 unclaimed ISR calls → Code 43).
// The register VA is published lock-free by StartDevice (`isr_status`, Release)
// BEFORE the transport goes live, and `adapter.dxgkrnl` is written before that
// — so a nonzero `isr_status` implies a valid callback table.
pub unsafe extern "C" fn dxgkddi_interrupt_routine(
    miniport_device_context: *mut c_void,
    _message_number: u32,
) -> BOOLEAN {
    if miniport_device_context.is_null() {
        return 0;
    }
    // SAFETY: dxgkrnl passes our AdapterContext as the miniport device context;
    // it is valid for the device's lifetime and `isr_status` is an atomic.
    let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };
    let isr_va = adapter.isr_status.load(Ordering::Acquire);
    if isr_va == 0 {
        // Transport not up yet (or torn down): not in a position to claim it.
        return 0;
    }
    // SAFETY: `isr_va` is the mapped MMIO VA of the 1-byte read-to-clear
    // ISR-status register, published by StartDevice; the read clears + deasserts.
    let status = unsafe { core::ptr::read_volatile(isr_va as *const u8) };
    if status == 0 {
        // Shared line, but no virtio interrupt pending — not ours.
        return 0;
    }
    INT_ROUTINE_COUNT.fetch_add(1, Ordering::Relaxed);
    // Bit 1 = configuration change: the virtio-gpu raises it on a
    // VIRTIO_GPU_EVENT_DISPLAY (monitor connect / mode change). Latch it for the
    // DPC, which wakes the HPD worker to (re-)indicate the child connected — the
    // viogpu3d ISR_REASON_CHANGE path (`viogpu_adapter.cpp:1531`).
    if status & 0x2 != 0 {
        adapter.config_change_pending.store(1, Ordering::Release);
    }
    // Bit 0 = used-ring progress (drain), bit 1 = config change: either needs the
    // DPC. (The ISR-status read above already deasserted the line for both.)
    if status & 0x3 != 0 {
        if let Some(dxgkrnl) = adapter.dxgkrnl_opt() {
            if let Some(queue_dpc) = dxgkrnl.DxgkCbQueueDpc {
                // SAFETY: DxgkCbQueueDpc is callable from the ISR at DIRQL;
                // DeviceHandle is the live dxgkrnl device handle.
                unsafe { queue_dpc(dxgkrnl.DeviceHandle) };
            }
        }
    }
    1 // claimed + acknowledged (line now deasserted)
}

/// `DxgkDdiDpcRoutine` — runs at DISPATCH_LEVEL after the ISR (or a
/// `signal_dma_completed` notify pair) queues a DPC.
pub unsafe extern "C" fn dxgkddi_dpc_routine(miniport_device_context: *mut c_void) {
    DPC_ROUTINE_COUNT.fetch_add(1, Ordering::Relaxed);
    if miniport_device_context.is_null() {
        return;
    }
    // SAFETY: our AdapterContext, valid for the device's lifetime.
    let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };

    // A latched config-change (ISR bit 1): wake the HPD worker to re-indicate the
    // child connected. KeSetEvent (Wait=FALSE) is legal at DISPATCH_LEVEL.
    if adapter.config_change_pending.load(Ordering::Acquire) != 0 {
        adapter.signal_hpd();
    }

    // Let dxgkrnl process any interrupt data queued by DxgkCbNotifyInterrupt
    // (the WDDM fence completions signaled below re-queue this DPC, and this
    // call drains their packets — the viogpu3d NotifyDpcRoutine ordering).
    if let Some(dxgkrnl) = adapter.dxgkrnl_opt() {
        if let Some(notify_dpc) = dxgkrnl.DxgkCbNotifyDpc {
            // SAFETY: DISPATCH_LEVEL DPC context; live device handle.
            unsafe { notify_dpc(dxgkrnl.DeviceHandle) };
        }
    }

    // Drain the used ring, wake ctrl/fence waiters, and retire every WDDM
    // submission whose Venus watermark has been reached.
    drain_used_and_complete(adapter);
}

/// `DxgkDdiControlInterrupt` — enable/disable a class of GPU interrupts. Called at
/// up to DIRQL, so this path touches only atomics (no registry / pageable calls).
//
// The OS drives CRTC_VSYNC here. The display half (DisplayHalf on) services it: it
// toggles the free-running VSync heartbeat's delivery gate and returns SUCCESS.
// A render-only adapter (0 video-present sources) drives no VSYNC → NOT_IMPLEMENTED
// (MSDN requires that for any type the driver does not service); the virtio
// used-ring interrupt is not an OS-controlled class.
pub unsafe extern "C" fn dxgkddi_control_interrupt(
    h_adapter: IN_CONST_HANDLE,
    interrupt_type: IN_CONST_DXGK_INTERRUPT_TYPE,
    enable: IN_BOOLEAN,
) -> NTSTATUS {
    CONTROL_INT_COUNT.fetch_add(1, Ordering::Relaxed);
    let p = h_adapter as *const AdapterContext;
    if !p.is_null()
        && interrupt_type == _DXGK_INTERRUPT_TYPE::DXGK_INTERRUPT_CRTC_VSYNC
        // SAFETY: dxgkrnl hands our AdapterContext; `display_half` is a plain bool
        // set once at StartDevice.
        && unsafe { (*p).display_half() }
    {
        // SAFETY: valid for the device lifetime.
        let adapter = unsafe { &*p };
        adapter
            .vsync_enabled
            .store((enable != 0) as u32, Ordering::Release);
        return STATUS_SUCCESS;
    }
    STATUS_NOT_IMPLEMENTED
}
