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

/// Indicate the single child video-output's connection state to the OS. PASSIVE.
fn indicate_child_status(adapter: &AdapterContext, connected: bool) {
    let Some(dxgkrnl) = adapter.dxgkrnl.as_ref() else {
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
}

/// Count of child-status indications this boot (diag `HpdN`).
static HPD_INDICATE_COUNT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

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

    // First indication: wait briefly so StartDevice has certainly returned
    // (DxgkCbIndicateChildStatus is forbidden during it), then indicate connected.
    // A boot config-change wakes us early; otherwise the timeout drives it.
    let mut initial_timeout: LARGE_INTEGER = unsafe { core::mem::zeroed() };
    initial_timeout.QuadPart = -5_000_000; // 500 ms relative (100 ns units)
                                           // SAFETY: waiting on the initialized hpd_event with a relative timeout.
    let _ = unsafe {
        KeWaitForSingleObject(
            adapter.hpd_event.get() as PVOID,
            0, // Executive
            0, // KernelMode
            0, // non-alertable
            &mut initial_timeout,
        )
    };
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
    indicate_child_status(adapter, true);
    // The initial timed/event wake already covered any config change that raced
    // StartDevice. Do not carry that bit into the steady-state event mux.
    adapter.config_change_pending.store(0, Ordering::Release);

    // Steady state is event/dirty driven. A real virtio display-change wakes us
    // to re-indicate the child; a completed primary GPU copy wakes us to queue
    // exactly one asynchronous RESOURCE_FLUSH. While that one command is in
    // flight, use a short used-ring poll only as interrupt-loss tolerance. This
    // never emits another flush by itself, so an idle desktop produces no ctrl
    // spam and a delayed synchronous response cannot cap presentation at 2.4 Hz.
    let mut reported_fail = 0u32;
    loop {
        let flush_inflight = adapter.scanout_flush_inflight.load(Ordering::Acquire) != 0;
        let bind_inflight = adapter.scanout_bind_inflight.load(Ordering::Acquire) != 0;
        let ctrl_inflight = flush_inflight || bind_inflight;
        let retry_pending =
            adapter.scanout_refresh_pending.load(Ordering::Acquire) != 0 && !ctrl_inflight;
        let mut timeout: LARGE_INTEGER = unsafe { core::mem::zeroed() };
        let timeout_ptr = if ctrl_inflight {
            timeout.QuadPart = -40_000; // 4 ms: bounded lost-interrupt fallback.
            &mut timeout
        } else if retry_pending {
            timeout.QuadPart = -160_000; // 16 ms retry after a loud enqueue failure.
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
            || ((adapter.scanout_flush_inflight.load(Ordering::Acquire) != 0
                || adapter.scanout_bind_inflight.load(Ordering::Acquire) != 0)
                && adapter.scanout_refresh_pending.load(Ordering::Acquire) != 0)
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
        crate::ddi::display::process_deferred_vidpn_source_address(adapter);

        // The ScanoutDiag forced-rebind experiment. It lives HERE, and not in
        // apply_vidpn_source_address_locked where it used to sit, because there
        // it short-circuited the production publication path: both of its
        // `return true` arms skipped every remaining exit - including the
        // vidpn_programming.store(0) that all sibling exits perform - while the
        // DDI returned STATUS_SUCCESS to dxgkrnl as if the Windows primary had
        // been programmed. The gate stayed at 1, the VSync heartbeat stopped,
        // and the Windows primary was never bound again for the rest of the boot.
        //
        // This worker is PASSIVE and owns no dxgkrnl return value, so the
        // experiment can no longer lie to the OS. `rebind_if_forced` is
        // unchanged and keeps its own `ScanoutDiag >= 2 && display_half` gate,
        // so every SdgR* counter keeps its exact name and value.
        let _ = crate::ddi::scanout_diag::rebind_if_forced(adapter, 11);

        // Drain an armed one-shot Present probe (R320). Taking the record is a
        // quick operation under the venus mutex; the probe itself — a 5 s fence
        // wait, a host map round-trip with 1 ms sleeps, MmMapIoSpace, ~196
        // volatile reads and 7 registry writes — runs HERE, at PASSIVE, with no
        // lock held and off the Present path entirely.
        if adapter.probe_pending.swap(0, Ordering::AcqRel) != 0 {
            let pending = adapter
                .with_venus_client(|client| client.take_pending_probe())
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
            match adapter.queue_active_scanout_refresh() {
                ScanoutRefreshQueue::Queued => {}
                ScanoutRefreshQueue::Busy | ScanoutRefreshQueue::Failed => {
                    // Preserve the first dirty frame. Busy completion wakes us;
                    // an enqueue failure gets the bounded retry timeout above.
                    adapter.scanout_refresh_pending.store(1, Ordering::Release);
                }
                // No bound scanout: there is nothing meaningful to flush. A
                // later completed copy will publish a fresh dirty edge.
                ScanoutRefreshQueue::Unavailable => {}
            }
        }

        let failed = adapter.scanout_refresh_fail.load(Ordering::Relaxed);
        if failed != reported_fail && (failed == 1 || (failed != 0 && (failed % 60) == 0)) {
            crate::diag::record_named_bytes(b"RfFail", failed);
            reported_fail = failed;
        }
    }
}
