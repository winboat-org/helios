//! The base (non-version-gated) block of `DRIVER_INITIALIZATION_DATA`.
//!
//! Moved verbatim out of `ddi/start_device.rs` by T8/R1102.

use crate::dxgk::*;

// ── Base driver/adapter lifecycle DDIs ──────────────────────────────────────
// These sit in the base (non-version-gated) block of DRIVER_INITIALIZATION_DATA
// and are all present in the MSDN DxgkInitialize sample. dxgkrnl's init path
// (DpiInitializeEx) rejects the init data when they are NULL — leaving them out
// is what made DxgkInitialize return STATUS_REVISION_MISMATCH even after the
// render/GPU-VA DDIs were registered.

/// `DxgkDdiUnload` — driver-wide unload (no device context). Inverse of
/// DriverEntry. All devices have been removed by now, so release the cached BAR
/// MMIO mappings that `WdkHal` reused across stop/start cycles.
pub unsafe extern "C" fn dxgkddi_unload() {
    crate::kmsg(c"Helios: Unload\n");
    crate::virtio::hal::WdkHal::unmap_all();
}

/// `DxgkDdiQueryInterface` — export a driver-defined interface. We expose none.
pub unsafe extern "C" fn dxgkddi_query_interface(
    _miniport_device_context: IN_CONST_PVOID,
    query_interface: IN_PQUERY_INTERFACE,
) -> NTSTATUS {
    // DIAG: log each interface GUID dxgkrnl asks for during AddAdapter. If
    // AddAdapter dies (OBJECT_NAME_NOT_FOUND) right after a query we reject, that
    // interface is the suspect. Marker 0x04000000 then the GUID's Data1.
    crate::diag::record(0x0400_0000);
    if !query_interface.is_null() {
        // SAFETY: non-null per the check; Dxgkrnl provides a valid QUERY_INTERFACE.
        let qi = unsafe { &*query_interface };
        if !qi.InterfaceType.is_null() {
            // SAFETY: InterfaceType points to a GUID for the duration of the call.
            crate::diag::record(unsafe { (*qi.InterfaceType).Data1 });
        }
    }
    STATUS_NOT_SUPPORTED
}

/// `DxgkDdiControlEtwLogging` — enable/disable the driver's ETW logging. We emit
/// none, so this is a no-op.
pub unsafe extern "C" fn dxgkddi_control_etw_logging(
    _enable: IN_BOOLEAN,
    _flags: IN_ULONG,
    _level: IN_UCHAR,
) {
}

/// `DxgkDdiResetDevice` — reset the device to a known state (e.g. before a crash
/// dump).
///
/// M2 (T8/R1102): deliberately a no-op, and the reason is not "Phase 2 has not
/// happened yet" — this driver is long past Phase 2. Dxgkrnl calls this on the
/// crash-dump path to quiesce anything that would corrupt the dump write, and
/// Helios programs no such hardware: there is no display-engine MMIO state and
/// no DMA engine of our own. Presentation is host-side (virtio-gpu SCANOUT_BLOB
/// over the control queue) and every guest-visible mapping is either the venus
/// ring or a blob window, none of which a dump write touches. A reset here
/// would have to tear down the transport, which is exactly what must NOT happen
/// while the dump path still needs the device to answer.
pub unsafe extern "C" fn dxgkddi_reset_device(_miniport_device_context: IN_CONST_PVOID) {}

/// `DxgkDdiNotifyAcpiEvent` — handle a platform ACPI event. We service none.
pub unsafe extern "C" fn dxgkddi_notify_acpi_event(
    _miniport_device_context: IN_CONST_PVOID,
    event_type: IN_DXGK_EVENT_TYPE,
    event: IN_ULONG,
    _argument: IN_PVOID,
    acpi_flags: OUT_PULONG,
) -> NTSTATUS {
    crate::diag::record(0x0A14_0000 | ((event_type as u32) & 0xFFFF));
    crate::diag::record(0x0A15_0000 | (event & 0xFFFF));
    if !acpi_flags.is_null() {
        unsafe { *acpi_flags = 0 };
    }
    STATUS_SUCCESS
}
