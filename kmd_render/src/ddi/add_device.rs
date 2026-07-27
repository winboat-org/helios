//! `DxgkDdiAddDevice` — allocate the adapter context for a discovered device.
//!
//! Reference: https://learn.microsoft.com/windows-hardware/drivers/ddi/dispmprt/nc-dispmprt-dxgkddi_add_device

use core::ffi::c_void;

use crate::adapter::AdapterContext;
use crate::dxgk::*;

pub unsafe extern "C" fn dxgkddi_add_device(
    // The DDI hands us the PDO; nothing in this driver retains or uses it.
    // Every path to the OS goes through the DXGKRNL_INTERFACE callback table
    // saved at StartDevice. T6/R917 deleted the `AdapterContext::pdo` field
    // that stored it and was never read.
    _physical_device_object: PDEVICE_OBJECT,
    miniport_device_context: *mut *mut c_void,
) -> NTSTATUS {
    crate::kmsg(c"Helios: AddDevice\n");
    crate::diag::record(0x0A00_0001);

    if miniport_device_context.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    // Match the display-miniport contract and the reference viogpu3d driver:
    // leave a deterministic NULL out-pointer on every failure path after this.
    unsafe { *miniport_device_context = core::ptr::null_mut() };

    // One call: allocate at the final heap address AND initialize the embedded
    // kernel dispatcher objects there. `create` never hands back an
    // `AdapterContext` by value, so a caller cannot skip the in-place init and
    // cannot move the context afterwards. The context is leaked to a raw
    // pointer; Dxgkrnl returns it to us on every DDI and we reclaim it in
    // DxgkDdiRemoveDevice.
    let raw = AdapterContext::create();
    // SAFETY: miniport_device_context is a valid out-pointer per the DDI contract.
    unsafe { *miniport_device_context = raw.as_ptr() as *mut c_void };

    crate::diag::record(0x0A00_0002);
    STATUS_SUCCESS
}
