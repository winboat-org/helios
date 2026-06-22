//! ISR / DPC DDIs.
//!
//! Registered now so the DDI table is complete, but inert until the virtqueue +
//! MSI-X path lands (Phase 2/3). The ISR runs at DIRQL: no allocations, no
//! pageable calls. It will read the virtio ISR status, drain the used ring, and
//! call DxgkCbNotifyInterrupt for each completed fence (see KMD.md Phase 3).

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::adapter::AdapterContext;
use crate::dxgk::*;

// ── DIRQL/DISPATCH-safe instrumentation (Step-2 coherent-fence bring-up) ─────
// `diag::record` is PASSIVE-only (RtlWriteRegistryValue), so the ISR/DPC cannot
// touch the registry ring. These atomics are incremented here at DIRQL/DISPATCH
// and dumped into the PASSIVE diag ring at DxgkDdiDestroyDevice — they tell us
// whether dxgkrnl/VidSch ever delivers a virtio interrupt or schedules our DPC
// during adapter bring-up (the open question behind the VidSchTerminateAdapter
// Code-43: does VidSch exercise the engine at all before it bails?).
pub static INT_ROUTINE_COUNT: AtomicU32 = AtomicU32::new(0);
pub static DPC_ROUTINE_COUNT: AtomicU32 = AtomicU32::new(0);
pub static CONTROL_INT_COUNT: AtomicU32 = AtomicU32::new(0);

/// `DxgkDdiInterruptRoutine` — runs at the device's DIRQL; returns TRUE if the
/// interrupt was ours.
//
// The virtio-gpu device is line-based INTx (`MSISupported=0`), i.e. *level*-
// triggered: it asserts the shared INTx line and keeps it asserted until the
// driver reads the read-to-clear virtio ISR-status register. Our submission path
// is polled (`add_notify_wait_pop` drains the used ring synchronously), so it
// never reads that register — leaving the line asserted after every completed
// command. dxgkrnl re-dispatches the still-asserted line into this routine; the
// previous stub returned FALSE without acking, so the line never dropped and the
// OS interrupt-storm detector disabled the adapter (observed: ~10000 unclaimed
// ISR calls/bring-up → Code 43).
//
// Fix: read the ISR-status register (which DEASSERTS the line) and, if a bit was
// pending, claim the interrupt (return TRUE). We do NOT queue a DPC: the polled
// submission path already owns and drains the used ring, so a DPC drain here
// would double-pop. The register VA is published lock-free by StartDevice; a
// volatile byte read at DIRQL takes no lock (the spinlock would be illegal here).
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
    1 // claimed + acknowledged (line now deasserted)
}

/// `DxgkDdiDpcRoutine` — runs at DISPATCH_LEVEL after the ISR queues a DPC.
pub unsafe extern "C" fn dxgkddi_dpc_routine(_miniport_device_context: *mut c_void) {
    DPC_ROUTINE_COUNT.fetch_add(1, Ordering::Relaxed);
    // No-op: the polled submission path owns the used ring; draining here would
    // double-pop. (Lands when submission moves to the async used-ring model.)
}

/// `DxgkDdiControlInterrupt` — enable/disable a class of GPU interrupts.
// STUB: Phase 3. The OS only ever passes DXGK_INTERRUPT_CRTC_VSYNC here, and
// MSDN requires STATUS_NOT_IMPLEMENTED for any type the driver does not service.
// A render-only adapter (0 video-present sources) drives no VSYNC, so we
// implement none yet — the virtio-gpu used-ring interrupt gating lands in Phase 3.
pub unsafe extern "C" fn dxgkddi_control_interrupt(
    _h_adapter: IN_CONST_HANDLE,
    _interrupt_type: IN_CONST_DXGK_INTERRUPT_TYPE,
    _enable: IN_BOOLEAN,
) -> NTSTATUS {
    CONTROL_INT_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    STATUS_NOT_IMPLEMENTED
}
