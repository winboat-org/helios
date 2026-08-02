//! Hot-plug-detect (HPD) worker for the display half.
//!
//! `DxgkCbIndicateChildStatus` tells the OS the child video-output is *connected*
//! — the transition that makes the VidPn target *available* so dxgkrnl will build a
//! source→target present path (without it the OS negotiates cleanly but commits
//! 0-path VidPns → 0 display paths, 36th-session symptom). It is PASSIVE-only and
//! MUST NOT be called during `DxgkDdiStartDevice`, so a dedicated system thread does
//! it — the viogpu3d `ThreadWorkRoutine`/`ConfigChanged` analog
//! (`viogpu_adapter.cpp:1580,1599`). The thread indicates once shortly after start
//! (an initial connect), then re-indicates on each virtio config-change interrupt
//! (`VIRTIO_GPU_EVENT_DISPLAY`, ISR status bit 1 → DPC → `signal_hpd`).

use core::ffi::c_void;
use core::sync::atomic::Ordering;

use crate::adapter::{AdapterContext, ScanoutRefreshQueue};
use crate::dxgk::*;
use wdk_sys::ntddk::{KeWaitForSingleObject, PsTerminateSystemThread};

const STATUS_TIMEOUT: NTSTATUS = 0x0000_0102;

/// Bounded fallback for the prologue's wait on "StartDevice has returned".
///
/// This is a SAFETY BOUND, not the mechanism. The mechanism is
/// `AdapterContext::signal_start_complete`, which StartDevice calls as its last
/// action; this only caps how long the worker sits idle if that edge is somehow
/// missed, so the first `Connected=1` still reaches the OS. Keeping the bound is
/// deliberate — the defect was a delay standing in for an event, not the
/// existence of a timeout.
const START_COMPLETE_FALLBACK_100NS: i64 = -5_000_000; // 500 ms, relative

/// Bounded lost-interrupt fallback while one async ctrl command owns descriptors.
///
/// NOT the R515 defect: the KEVENT (ISR -> DPC -> drain -> signal) is the real
/// wake source and this only covers a delayed device interrupt.
const CTRL_INFLIGHT_POLL_100NS: i64 = -40_000; // 4 ms, relative

/// Retry delay after a loud scanout-refresh enqueue failure. Also a real bound,
/// not a stand-in: the failure has no wake source of its own.
const REFRESH_RETRY_100NS: i64 = -160_000; // 16 ms, relative

/// Indicate the single child video-output's connection state to the OS. PASSIVE.
fn indicate_child_status(adapter: &AdapterContext, connected: bool) {
    let Some(dxgkrnl) = adapter.dxgkrnl_opt() else {
        return;
    };
    let Some(indicate) = dxgkrnl.DxgkCbIndicateChildStatus else {
        return;
    };
    // SAFETY: an all-zero DXGK_CHILD_STATUS is a valid starting point.
    let mut status: DXGK_CHILD_STATUS = unsafe { core::mem::zeroed() };
    status.Type = _DXGK_CHILD_STATUS_TYPE::StatusConnection;
    status.ChildUid = crate::ddi::vidpn::CHILD_UID;
    // Plain union write (safe): the HotPlug arm is the one StatusConnection uses.
    status.__bindgen_anon_1.HotPlug.Connected = connected as u8;
    // SAFETY: live callback interface; `status` is a fully-initialized child-status
    // packet valid for the synchronous call. PASSIVE_LEVEL (worker thread).
    let st = unsafe { indicate(dxgkrnl.DeviceHandle, &mut status) };
    crate::diag::record_named_bytes(b"HpdI", ((connected as u32) << 16) | (st as u32 & 0xFFFF));
    HPD_INDICATE_COUNT.fetch_add(1, Ordering::Relaxed);
    crate::diag::record_named_bytes(b"HpdN", HPD_INDICATE_COUNT.load(Ordering::Relaxed));
    // Emitted alongside HpdN so the two are always read together: a nonzero
    // HpdStTo means the first indication was driven by the fallback, not the
    // start edge.
    crate::diag::record_named_bytes(b"HpdStTo", HPD_START_EDGE_TIMEOUTS.load(Ordering::Relaxed));
}

/// Count of child-status indications this boot (diag `HpdN`).
static HPD_INDICATE_COUNT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Times the prologue's bounded fallback fired instead of the real start edge
/// (diag `HpdStTo`). Must read 0 on a healthy boot: a nonzero value means
/// StartDevice's `signal_start_complete` did not reach the worker, and the first
/// `Connected=1` was as late as it used to be.
static HPD_START_EDGE_TIMEOUTS: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

/// The HPD worker thread body (`PsCreateSystemThread`). Runs at PASSIVE_LEVEL for
/// the device's lifetime; `AdapterContext::stop_hpd` signals `hpd_stop` + the wake
/// event and joins it before teardown.
///
/// # Safety
/// `context` is the `AdapterContext` pointer passed to `PsCreateSystemThread`,
/// valid until `stop_hpd` joins this thread.
pub unsafe extern "C" fn hpd_thread_routine(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: the adapter context is alive until StopDevice joins this thread.
    let adapter = unsafe { &*(context as *const AdapterContext) };
    // SAFETY: this is a `PsCreateSystemThread` body. A system worker thread runs
    // at PASSIVE_LEVEL for its whole life — it is not entered from a DDI, an ISR
    // or a DPC — and every wait below is a `KeWaitForSingleObject` that would
    // already be illegal otherwise. This is the one mint in the driver whose
    // justification is structural rather than a documented DDI annotation, which
    // is why it is minted ONCE here and passed down rather than re-asserted per
    // callee.
    let passive = unsafe { crate::irql::PassiveLevel::assume() };

    // ── Phase 1: wait for StartDevice to return ─────────────────────────────
    //
    // `DxgkCbIndicateChildStatus` is forbidden DURING StartDevice, so the first
    // indication must wait for it to return. That used to be a bare 500 ms
    // relative wait — a delay standing in for an event, with nothing observing
    // the return at all. StartDevice now sets `start_complete` and signals
    // `hpd_event` as its last action, so this wait has a REAL wake source and
    // `START_COMPLETE_FALLBACK_100NS` is only the bound that keeps a missed edge
    // from parking the worker forever.
    //
    // Looped because `hpd_event` is a SynchronizationEvent shared with the
    // scanout/config-change paths: a wake that is not the start edge must not be
    // mistaken for one. The loop is bounded by the same fallback per iteration
    // and by `hpd_stop`.
    while adapter.start_complete.load(Ordering::Acquire) == 0 {
        if adapter.hpd_stop.load(Ordering::Acquire) != 0 {
            break;
        }
        let mut timeout: LARGE_INTEGER = unsafe { core::mem::zeroed() };
        timeout.QuadPart = START_COMPLETE_FALLBACK_100NS;
        // SAFETY: waiting on the initialized hpd_event with a relative timeout.
        let st = unsafe {
            KeWaitForSingleObject(
                adapter.hpd_event.get() as PVOID,
                0, // Executive
                0, // KernelMode
                0, // non-alertable
                &mut timeout,
            )
        };
        if st == STATUS_TIMEOUT {
            // The bound fired. Proceed anyway — a late indication is far better
            // than none — and count it, because it means the edge was missed.
            HPD_START_EDGE_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
            break;
        }
    }
    if adapter.hpd_stop.load(Ordering::Acquire) != 0 {
        // Publish "worker exited" BEFORE terminating, so stop_hpd's join does
        // not depend on ObReferenceObjectByHandle succeeding.
        // SAFETY: initialized NotificationEvent on the adapter, which outlives
        // this thread by the leak rule in stop_hpd.
        unsafe { wdk_sys::ntddk::KeSetEvent(adapter.hpd_exited.get(), 0, 0) };
        // SAFETY: terminates the current system thread; does not return.
        let _ = unsafe { PsTerminateSystemThread(STATUS_SUCCESS) };
        return;
    }
    // SWAP BEFORE indicating, not store(0) after.
    //
    // The old order was: indicate, then unconditionally store 0. An ISR that set
    // `config_change_pending` while `DxgkCbIndicateChildStatus` was in flight had
    // its bit DISCARDED — and because `hpd_event` is a SynchronizationEvent, the
    // DPC's signal then satisfied the loop's first wait, whose own swap returned
    // 0, so nothing was indicated. That host mode change was never re-reported to
    // the OS.
    //
    // Swapping first means a bit set DURING the indication survives to the next
    // iteration and is acted on there. The steady-state loop already had this
    // right; only this one-shot prologue did not.
    adapter.config_change_pending.swap(0, Ordering::AcqRel);
    indicate_child_status(adapter, true);

    // Steady state is event/dirty driven. A real virtio display-change wakes us
    // to re-indicate the child; a completed primary GPU copy wakes us to queue
    // exactly one asynchronous RESOURCE_FLUSH. While that one command is in
    // flight, use a short used-ring poll only as interrupt-loss tolerance. This
    // never emits another flush by itself, so an idle desktop produces no ctrl
    // spam and a delayed synchronous response cannot cap presentation at 2.4 Hz.
    let mut reported_fail = 0u32;
    loop {
        // One in-flight class since T6/R902 deleted the async bind: the
        // composition `flush_inflight || bind_inflight` reduces to the flush
        // term, because `scanout_bind_inflight`'s only non-zero writer was the
        // CAS inside the deleted arm.
        let ctrl_inflight = adapter.scanout_flush_inflight.load(Ordering::Acquire) != 0;
        let retry_pending =
            adapter.scanout_refresh_pending.load(Ordering::Acquire) != 0 && !ctrl_inflight;
        let mut timeout: LARGE_INTEGER = unsafe { core::mem::zeroed() };
        let timeout_ptr = if ctrl_inflight {
            timeout.QuadPart = CTRL_INFLIGHT_POLL_100NS;
            &mut timeout
        } else if retry_pending {
            timeout.QuadPart = REFRESH_RETRY_100NS;
            &mut timeout
        } else {
            core::ptr::null_mut()
        };
        // SAFETY: wait on the initialized event; NULL timeout means sleep until
        // config change, scanout dirty, completion, or StopDevice.
        let wait_status = unsafe {
            KeWaitForSingleObject(adapter.hpd_event.get() as PVOID, 0, 0, 0, timeout_ptr)
        };
        if adapter.hpd_stop.load(Ordering::Acquire) != 0 {
            // Publish "worker exited" BEFORE terminating, so stop_hpd's join
            // does not depend on ObReferenceObjectByHandle succeeding.
            // SAFETY: initialized NotificationEvent on the adapter, which
            // outlives this thread by the leak rule in stop_hpd.
            unsafe { wdk_sys::ntddk::KeSetEvent(adapter.hpd_exited.get(), 0, 0) };
            // SAFETY: terminates the current system thread; does not return.
            let _ = unsafe { PsTerminateSystemThread(STATUS_SUCCESS) };
            return;
        }

        // The KEVENT is the primary completion path (ISR -> DPC -> drain ->
        // signal). If that device interrupt is delayed, poll only while one
        // async flush owns descriptors; also do one poll when a new dirty frame
        // arrives behind it. This frees the coalescing gate without waiting for
        // the old exponential synchronous-roundtrip slices.
        if (wait_status == STATUS_TIMEOUT && ctrl_inflight)
            || (adapter.scanout_flush_inflight.load(Ordering::Acquire) != 0
                && adapter.scanout_refresh_pending.load(Ordering::Acquire) != 0)
            // A presentation lease ended somewhere that could not pop the WDDM
            // pending FIFO itself — the used-ring drain holds `virtio_lock`, and
            // taking `wddm_notify_lock` under it inverts the driver's lock order
            // (ROADMAP defect 0ab-B). This is that work's PASSIVE home, and it
            // is a real edge rather than a poll: the same store that sets the
            // flag signals the event we just woke on.
            || adapter.scanout_retire_wanted.swap(0, Ordering::AcqRel) != 0
        {
            crate::ddi::interrupt::drain_used_and_complete(adapter);
        }

        // The ISR owns setting this bit; the PASSIVE worker consumes it after
        // the DPC's wake so a scanout-completion wake cannot masquerade as HPD.
        if adapter.config_change_pending.swap(0, Ordering::AcqRel) != 0 {
            indicate_child_status(adapter, true);
        }

        // Consume only the allocation identity supplied by Windows through
        // SetVidPnSourceAddress. The DDI can be called at DIRQL, where neither
        // Venus waits nor registry diagnostics are legal; this worker is the
        // PASSIVE continuation for that exact callback.
        crate::ddi::display::process_deferred_vidpn_source_address(passive, adapter);

        // Publish the unsampled scanout-bind trace. This is the ONE PASSIVE
        // site that mirrors it; accumulation happens at DIRQL/DISPATCH with
        // atomics only. Throttled inside `dump_periodic` — a dump is ~120
        // registry writes, so it must never run per frame. Placed after the
        // deferred programming so a dump reflects the bind that just ran.
        crate::ddi::scanout_trace::dump_periodic(adapter);

        // T6/R901 deleted the `ScanoutDiag` forced-rebind experiment that used
        // to run here. T1a had already moved it off
        // `apply_vidpn_source_address_locked`, where both of its `return true`
        // arms short-circuited the production publication path -- skipping the
        // `vidpn_programming.store(0)` every sibling exit performs -- while the
        // DDI returned STATUS_SUCCESS to dxgkrnl as if the Windows primary had
        // been programmed. The gate stayed at 1, the VSync heartbeat stopped,
        // and the Windows primary was never bound again for the rest of the boot.
        // Deleting the experiment removes that failure mode by construction
        // rather than by keeping it on a worker that cannot lie to the OS.
        //
        // It also removed a synchronous `RtlQueryRegistryValues` per programmed
        // primary, which contradicted `adapter.rs`'s own "never query the
        // registry on every frame".

        // Drain an armed one-shot Present probe (R320). Taking the record is a
        // quick operation under the venus mutex; the probe itself — a 5 s fence
        // wait, a host map round-trip with 1 ms sleeps, MmMapIoSpace, ~196
        // volatile reads and 7 registry writes — runs HERE, at PASSIVE, with no
        // lock held and off the Present path entirely.
        if adapter.probe_pending.swap(0, Ordering::AcqRel) != 0 {
            let pending = adapter
                .with_venus_client(passive, |client| client.take_pending_probe())
                .ok()
                .flatten();
            if let Some((destination, fence_id)) = pending {
                // The probe now samples LATER than the fence retirement it waits
                // for, so the destination may have been destroyed in between.
                // Re-validate liveness by resource id before touching it; the
                // record deliberately carries the id, never a raw pointer.
                let live = adapter
                    .with_virtio(|v| v.resource_is_live(destination.resource_id()))
                    .unwrap_or(false);
                if live {
                    crate::virtio::venus::VenusClient::probe_present_destination(
                        passive,
                        adapter,
                        destination,
                        fence_id,
                    );
                } else {
                    crate::diag::record_named_bytes(b"PBPrF", 0xE6);
                }
            }
        }

        if adapter.scanout_refresh_pending.swap(0, Ordering::AcqRel) != 0 {
            match adapter.queue_active_scanout_refresh(passive) {
                ScanoutRefreshQueue::Queued => {}
                ScanoutRefreshQueue::Busy | ScanoutRefreshQueue::Failed => {
                    // Preserve the first dirty frame. Busy completion wakes us;
                    // an enqueue failure gets the bounded retry timeout above.
                    adapter.scanout_refresh_pending.store(1, Ordering::Release);
                }
                // No bound scanout: there is nothing meaningful to flush. A
                // later completed copy will publish a fresh dirty edge. This
                // DROPS the dirty bit, so it is counted (`ScUnav`) rather than
                // being a comment — a rising count here means dirty frames are
                // being discarded because nothing is bound.
                ScanoutRefreshQueue::Unavailable => {
                    crate::ddi::display::SC_UNAVAILABLE.fetch_add(1, Ordering::Relaxed);
                }
                // The ownership gate refused this edge (ROADMAP defect 0ab-B).
                // Deliberately NOT re-armed: re-arming would reissue the same
                // refused flush every wakeup, and the whole argument for the
                // drop is that a better-timed publisher exists — the incoming
                // buffer's own bind edge. Censused as `OgIdn`/`OgEpo`.
                ScanoutRefreshQueue::Dropped => {}
            }
        }

        let failed = adapter.scanout_refresh_fail.load(Ordering::Relaxed);
        if failed != reported_fail && (failed == 1 || (failed != 0 && (failed % 60) == 0)) {
            crate::diag::record_named_bytes(b"RfFail", failed);
            reported_fail = failed;
        }
    }
}
