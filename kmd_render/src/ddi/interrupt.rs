//! ISR / DPC DDIs.
//!
//! Registered now so the DDI table is complete, but inert until the virtqueue +
//! MSI-X path lands (Phase 2/3). The ISR runs at DIRQL: no allocations, no
//! pageable calls. It will read the virtio ISR status, drain the used ring, and
//! call DxgkCbNotifyInterrupt for each completed fence (see KMD.md Phase 3).

use core::ffi::c_void;

use crate::dxgk::*;

/// `DxgkDdiInterruptRoutine` — returns TRUE if the interrupt was ours.
pub unsafe extern "C" fn dxgkddi_interrupt_routine(
    _miniport_device_context: *mut c_void,
    _message_number: u32,
) -> BOOLEAN {
    // No MSI-X wired yet → never claim the interrupt.
    0
}

/// `DxgkDdiDpcRoutine` — runs at DISPATCH_LEVEL after the ISR queues a DPC.
pub unsafe extern "C" fn dxgkddi_dpc_routine(_miniport_device_context: *mut c_void) {
    // Nothing to do until we process completions.
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
    STATUS_NOT_IMPLEMENTED
}
