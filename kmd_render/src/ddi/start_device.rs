//! The VSync heartbeat DPC.
//!
//! All that remains of this module after T8/R1102 split the PnP/power, base,
//! child and BAR-segment families out. The DPC itself moves to
//! `adapter/kobj.rs` in the next commit, beside the timer that arms it, and
//! this file goes away with it.

use core::ffi::c_void;

use crate::adapter::AdapterContext;
use crate::dxgk::*;

/// KTIMER DPC (DISPATCH_LEVEL): synthesize a `DXGK_INTERRUPT_CRTC_VSYNC` for the
/// display half's single target every timer tick (~16 ms), so dxgkrnl advances the
/// flip queue and issues `SetVidPnSourceAddress` (viogpu3d `FlipThread`/`:1977`).
/// Reads only atomics + the saved callback table, so it tolerates a torn-down
/// transport (StopDevice cancels the timer but a queued DPC may still run once).
pub unsafe extern "C" fn vsync_dpc_routine(
    _dpc: *mut KDPC,
    context: *mut c_void,
    _arg1: *mut c_void,
    _arg2: *mut c_void,
) {
    use core::sync::atomic::Ordering;
    if context.is_null() {
        return;
    }
    // SAFETY: `context` is the adapter pointer passed to KeInitializeDpc; valid
    // for the device lifetime (freed only in RemoveDevice, after the timer is
    // cancelled in StopDevice).
    let adapter = unsafe { &*(context as *const AdapterContext) };
    if !adapter.display_half() || adapter.vsync_enabled.load(Ordering::Acquire) == 0 {
        return;
    }
    let Some(dxgkrnl) = adapter.dxgkrnl_opt() else {
        return;
    };
    // SetVidPnSourceAddress can hand us a new exact primary from inside the
    // preceding synchronized VSync callback. Its host bind/copy continues at
    // PASSIVE_LEVEL. Do not send another VSync carrying the old address while
    // that display-engine operation is outstanding; the next notification must
    // describe the primary that is actually programmed.
    if adapter.programming_active() {
        if adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0 {
            adapter.signal_hpd();
        }
        return;
    }
    let phys = adapter.last_primary_address.load(Ordering::Acquire) as i64;
    // SAFETY: live callback interface; signal_crtc_vsync raises to DIRQL internally
    // via DxgkCbSynchronizeExecution and delivers the CRTC_VSYNC packet.
    let _ = unsafe {
        crate::ddi::submit_command::signal_crtc_vsync(dxgkrnl, phys, crate::ddi::vidpn::CHILD_UID)
    };
    // SetVidPnSourceAddress may run inside the synchronized MMIO-flip callback
    // above at DIRQL. It can only publish the exact hAllocation there. Back at
    // this timer DPC's DISPATCH_LEVEL, wake the PASSIVE worker that is allowed
    // to take the Venus mutex and issue the host scanout commands.
    if adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0 {
        adapter.signal_hpd();
    }
    adapter.vsync_count.fetch_add(1, Ordering::Relaxed);
}
