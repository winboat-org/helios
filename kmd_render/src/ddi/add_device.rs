//! `DxgkDdiAddDevice` — allocate the adapter context for a discovered device.
//!
//! Reference: https://learn.microsoft.com/windows-hardware/drivers/ddi/dispmprt/nc-dispmprt-dxgkddi_add_device

use alloc::boxed::Box;
use core::ffi::c_void;

use crate::adapter::AdapterContext;
use crate::dxgk::*;

pub unsafe extern "C" fn dxgkddi_add_device(
    physical_device_object: PDEVICE_OBJECT,
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

    let ctx = match AdapterContext::new(physical_device_object) {
        Ok(c) => c,
        Err(e) => return e.into_ntstatus(),
    };

    // Leak the context to a raw pointer; Dxgkrnl returns it to us on every DDI
    // and we reclaim it in DxgkDdiRemoveDevice.
    let raw = Box::into_raw(Box::new(ctx)) as *mut c_void;
    // SAFETY: miniport_device_context is a valid out-pointer per the DDI contract.
    unsafe { *miniport_device_context = raw };

    crate::diag::record(0x0A00_0002);
    STATUS_SUCCESS
}
