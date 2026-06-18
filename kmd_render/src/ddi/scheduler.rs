//! Scheduler, engine-status, and platform DDIs required by modern WDDM.
//!
//! These are conservative bring-up implementations. They do not advertise
//! preemption, hardware queues, or telemetry features, but they give Dxgkrnl
//! valid answers for the mandatory scheduler control points around the single
//! render node exposed by `DxgkDdiGetNodeMetadata`.

use alloc::boxed::Box;
use core::mem::size_of;

use crate::adapter::AdapterContext;
use crate::dxgk::*;

struct HwContext;

struct HwQueue;

pub unsafe extern "C" fn dxgkddi_query_dependent_engine_group(
    _h_adapter: IN_CONST_HANDLE,
    query: INOUT_DXGKARG_QUERYDEPENDENTENGINEGROUP,
) -> NTSTATUS {
    if query.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    let query = unsafe { &mut *query };
    if query.NodeOrdinal != 0 || query.EngineOrdinal != 0 {
        return STATUS_INVALID_PARAMETER;
    }

    query.DependentNodeOrdinalMask = 0;
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_query_engine_status(
    _h_adapter: IN_CONST_HANDLE,
    query: INOUT_PDXGKARG_QUERYENGINESTATUS,
) -> NTSTATUS {
    if query.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    let query = unsafe { &mut *query };
    if query.NodeOrdinal != 0 || query.EngineOrdinal != 0 {
        return STATUS_INVALID_PARAMETER;
    }

    unsafe {
        core::ptr::write_bytes(
            &mut query.EngineStatus as *mut _ as *mut u8,
            0,
            size_of::<DXGK_ENGINESTATUS>(),
        );
    }
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_reset_engine(
    h_adapter: IN_CONST_HANDLE,
    reset: INOUT_PDXGKARG_RESETENGINE,
) -> NTSTATUS {
    if h_adapter.is_null() || reset.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    let reset = unsafe { &mut *reset };
    if reset.NodeOrdinal != 0 || reset.EngineOrdinal != 0 {
        return STATUS_INVALID_PARAMETER;
    }

    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    reset.LastAbortedFenceId = adapter
        .last_completed_fence
        .load(core::sync::atomic::Ordering::Acquire) as UINT;
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_create_hw_context(
    h_device: IN_CONST_HANDLE,
    args: INOUT_PDXGKARG_CREATEHWCONTEXT,
) -> NTSTATUS {
    crate::diag::record(0x0700_0001);
    if h_device.is_null() || args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    let args = unsafe { &mut *args };
    if args.NodeOrdinal != 0 || args.EngineAffinity != 0 {
        return STATUS_INVALID_PARAMETER;
    }

    let ctx = Box::new(HwContext);
    args.hHwContext = Box::into_raw(ctx) as HANDLE;
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_destroy_hw_context(h_hw_context: IN_CONST_HANDLE) -> NTSTATUS {
    crate::diag::record(0x0700_0002);
    if !h_hw_context.is_null() {
        drop(unsafe { Box::from_raw(h_hw_context as *mut HwContext) });
    }
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_create_hw_queue(
    h_hw_context: IN_CONST_HANDLE,
    args: INOUT_PDXGKARG_CREATEHWQUEUE,
) -> NTSTATUS {
    crate::diag::record(0x0700_0003);
    if h_hw_context.is_null() || args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    let args = unsafe { &mut *args };
    let queue = Box::new(HwQueue);
    args.hHwQueue = Box::into_raw(queue) as HANDLE;
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_destroy_hw_queue(h_hw_queue: IN_CONST_HANDLE) -> NTSTATUS {
    crate::diag::record(0x0700_0004);
    if !h_hw_queue.is_null() {
        drop(unsafe { Box::from_raw(h_hw_queue as *mut HwQueue) });
    }
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_submit_command_to_hw_queue(
    _h_adapter: IN_CONST_HANDLE,
    args: IN_CONST_PDXGKARG_SUBMITCOMMANDTOHWQUEUE,
) -> NTSTATUS {
    crate::diag::record(0x0700_0005);
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_switch_to_hw_context_list(
    _h_adapter: IN_CONST_HANDLE,
    args: IN_CONST_PDXGKARG_SWITCHTOHWCONTEXTLIST,
) -> NTSTATUS {
    crate::diag::record(0x0700_0006);
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    let args = unsafe { &*args };
    if args.NodeOrdinal != 0 || args.EngineOrdinal != 0 {
        return STATUS_INVALID_PARAMETER;
    }

    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_present_to_hw_queue(
    _h_context: IN_CONST_HANDLE,
    args: INOUT_PDXGKARG_PRESENT,
) -> NTSTATUS {
    crate::diag::record(0x0700_0007);
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_cancel_command(
    _h_adapter: IN_CONST_HANDLE,
    cancel: IN_CONST_PDXGKARG_CANCELCOMMAND,
) -> NTSTATUS {
    if cancel.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_calibrate_gpu_clock(
    _h_adapter: IN_CONST_HANDLE,
    _node_ordinal: UINT32,
    _engine_ordinal: UINT32,
    clock_data: OUT_PDXGKARG_CALIBRATEGPUCLOCK,
) -> NTSTATUS {
    if clock_data.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    unsafe {
        core::ptr::write_bytes(
            clock_data as *mut u8,
            0,
            size_of::<DXGKARG_CALIBRATEGPUCLOCK>(),
        );
    }
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_format_history_buffer(
    _h_adapter: IN_CONST_HANDLE,
    args: *mut DXGKARG_FORMATHISTORYBUFFER,
) -> NTSTATUS {
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    let args = unsafe { &mut *args };
    args.NumTimestamps = 0;
    args.Offset = 0;
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_set_stable_power_state(
    _h_adapter: IN_CONST_HANDLE,
    args: IN_CONST_PDXGKARG_SETSTABLEPOWERSTATE,
) {
    if args.is_null() {
        return;
    }
}

pub unsafe extern "C" fn dxgkddi_set_virtual_machine_data(
    _h_adapter: IN_CONST_HANDLE,
    args: IN_CONST_PDXGKARG_SETVIRTUALMACHINEDATA,
) -> NTSTATUS {
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_power_runtime_set_device_handle(
    _h_adapter: IN_CONST_HANDLE,
    _power_handle: HANDLE,
) -> NTSTATUS {
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_power_runtime_control_request(
    _h_adapter: IN_CONST_HANDLE,
    _power_control_code: LPCGUID,
    _input_buffer: PVOID,
    _input_buffer_size: SIZE_T,
    _output_buffer: PVOID,
    _output_buffer_size: SIZE_T,
    bytes_returned: PSIZE_T,
) -> NTSTATUS {
    if !bytes_returned.is_null() {
        unsafe { *bytes_returned = 0 };
    }
    STATUS_NOT_SUPPORTED
}
