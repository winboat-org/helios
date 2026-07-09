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

use crate::adapter::AdapterContext;
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
        // SAFETY: terminates the current system thread; does not return.
        let _ = unsafe { PsTerminateSystemThread(STATUS_SUCCESS) };
        return;
    }
    indicate_child_status(adapter, true);

    // Steady state: re-indicate on each config-change wake; between wakes, flush
    // the selected scanout blob. This thread is PASSIVE_LEVEL and joined before
    // teardown, so it is the right place for the blocking virtio control roundtrip.
    loop {
        let mut timeout: LARGE_INTEGER = unsafe { core::mem::zeroed() };
        timeout.QuadPart = -160_000; // 16 ms relative (100 ns units)
                                     // SAFETY: timed wait on the initialized hpd_event.
        let wait_status = unsafe {
            KeWaitForSingleObject(adapter.hpd_event.get() as PVOID, 0, 0, 0, &mut timeout)
        };
        if adapter.hpd_stop.load(Ordering::Acquire) != 0 {
            // SAFETY: terminates the current system thread; does not return.
            let _ = unsafe { PsTerminateSystemThread(STATUS_SUCCESS) };
            return;
        }
        if wait_status == STATUS_TIMEOUT {
            let _ = adapter.refresh_active_scanout();
            continue;
        }
        // A virtio display event may have changed the mode; re-indicate connected
        // so the OS re-reads the monitor (QueryChildStatus/QueryDeviceDescriptor).
        indicate_child_status(adapter, true);
    }
}
