//! Scheduler, engine-status, and platform DDIs required by modern WDDM.
//!
//! These are conservative bring-up implementations. They do not advertise
//! preemption, hardware queues, or telemetry features, but they give Dxgkrnl
//! valid answers for the mandatory scheduler control points around the single
//! render node exposed by `DxgkDdiGetNodeMetadata`.

use alloc::boxed::Box;
use core::mem::size_of;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::adapter::AdapterContext;
use crate::ddi::present_packet::{
    PresentAllocations, PresentPayload, STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER,
};
use crate::device::DeviceHandleRef;
use crate::dxgk::*;

const HW_CONTEXT_MAGIC: u32 = 0x4843_5458; // "HCTX"
const HW_QUEUE_MAGIC: u32 = 0x4851_5545; // "HQUE"

#[repr(C)]
struct HwContext {
    magic: u32,
    adapter: *mut AdapterContext,
}

#[repr(C)]
struct HwQueue {
    magic: u32,
    adapter: *mut AdapterContext,
}

static PRESENT_HWQ_COUNT: AtomicU32 = AtomicU32::new(0);

unsafe fn hw_queue_adapter(h_hw_queue: IN_CONST_HANDLE) -> Option<*mut AdapterContext> {
    if h_hw_queue.is_null() {
        return None;
    }

    // SAFETY: Dxgkrnl passes back the opaque hHwQueue value returned from
    // DxgkDdiCreateHwQueue. The magic is the first field in both structs.
    let queue = unsafe { &*(h_hw_queue as *const HwQueue) };
    if queue.magic == HW_QUEUE_MAGIC && !queue.adapter.is_null() {
        return Some(queue.adapter);
    }

    // Some WDDM documentation names this first parameter generically as a
    // context handle. Accept our HW context too so diagnostics keep working if
    // the OS routes this callback through hHwContext on a different build.
    // SAFETY: Same opaque-handle contract; HwContext also starts with magic.
    let context = unsafe { &*(h_hw_queue as *const HwContext) };
    if context.magic == HW_CONTEXT_MAGIC && !context.adapter.is_null() {
        return Some(context.adapter);
    }

    None
}

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
    // Engine reset aborts the node's outstanding submissions: drop the pending
    // venus-gated WDDM fences (dxgkrnl resubmits what it still wants done).
    // The queue mutation requires the notification-lock proof token, so no DMA
    // completion from the abandoned scheduler epoch can escape concurrently.
    let _ = crate::ddi::abandon_pending_submissions(
        adapter,
        crate::ddi::AbandonOutcome::ReportLastAborted {
            out: &mut reset.LastAbortedFenceId,
        },
    );
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

    // SAFETY: h_device is the DeviceContext pointer returned from
    // DxgkDdiCreateDevice for this adapter. The checked traversal is the only
    // route to the adapter now that the back-pointer is private.
    let Some(device) = (unsafe { DeviceHandleRef::from_raw(h_device) }) else {
        return STATUS_INVALID_PARAMETER;
    };
    if device.adapter().is_none() {
        return STATUS_INVALID_PARAMETER;
    }

    let ctx = Box::new(HwContext {
        magic: HW_CONTEXT_MAGIC,
        // Stored, not borrowed: `HwContext` outlives this call.
        adapter: device.adapter_ptr(),
    });
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

    // SAFETY: h_hw_context is the HwContext pointer returned from
    // DxgkDdiCreateHwContext.
    let hw_context = unsafe { &*(h_hw_context as *const HwContext) };
    if hw_context.magic != HW_CONTEXT_MAGIC || hw_context.adapter.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    let args = unsafe { &mut *args };
    let queue = Box::new(HwQueue {
        magic: HW_QUEUE_MAGIC,
        adapter: hw_context.adapter,
    });
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
    h_hw_queue: IN_CONST_HANDLE,
    args: INOUT_PDXGKARG_PRESENT,
) -> NTSTATUS {
    crate::diag::record(0x0700_0007);
    PRESENT_HWQ_COUNT.fetch_add(1, Ordering::Relaxed);
    crate::diag::record_named_bytes(b"PHQcall", PRESENT_HWQ_COUNT.load(Ordering::Relaxed));

    if h_hw_queue.is_null() || args.is_null() {
        crate::diag::record_named_bytes(b"PHQst", STATUS_INVALID_PARAMETER as u32);
        return STATUS_INVALID_PARAMETER;
    }

    let Some(_adapter_ptr) = (unsafe { hw_queue_adapter(h_hw_queue) }) else {
        crate::diag::record_named_bytes(b"PHQst", STATUS_INVALID_PARAMETER as u32);
        return STATUS_INVALID_PARAMETER;
    };
    let args = unsafe { &mut *args };
    let present_flags = unsafe { args.Flags.__bindgen_anon_1.Value };
    crate::diag::record_named_bytes(b"PHQflag", present_flags);
    crate::diag::record_named_bytes(
        b"PHQcnt",
        (args.NumSrcAllocations << 16) | (args.NumDstAllocations & 0xFFFF),
    );

    // Same one-place union decode as DxgkDdiPresent.
    // SAFETY: `args` is dxgkrnl's present struct.
    let payload = unsafe { PresentPayload::decode(args) };
    let Some(allocation_list) = payload.allocation_list() else {
        // FlipWithMultiPlaneOverlay — unreachable without the MPO3 KMD interface.
        crate::diag::record_named_bytes(b"PHQmpo", 1);
        return STATUS_NOT_SUPPORTED;
    };
    let has_list = allocation_list.is_present();
    // SAFETY: `allocation_list` came from `PresentPayload::decode`.
    let present_allocations = unsafe { PresentAllocations::from_allocation_list(allocation_list) };
    crate::diag::record_named_bytes(b"PHQalst", u32::from(has_list));

    if (present_flags & (1 << 2)) == 0 && has_list {
        let src_handle = present_allocations
            .source()
            .map(|allocation| allocation.handle())
            .unwrap_or(core::ptr::null_mut());
        let dst_handle = present_allocations
            .destination()
            .map(|allocation| allocation.handle())
            .unwrap_or(core::ptr::null_mut());
        crate::diag::record_named_bytes(b"PHQsrcH", (src_handle as usize as u32) & 0xFFFF);
        crate::diag::record_named_bytes(b"PHQdstH", (dst_handle as usize as u32) & 0xFFFF);

        // Ordinary allocation-list presents identify application/DWM work for
        // VidSch residency and patching only. They must never rebind scanout:
        // SetVidPnSourceAddress is the authoritative desktop-primary identity,
        // and the typed refresh command below only dirties that durable target.
        crate::diag::record_named_bytes(b"PHQsrc", 0);
    }

    // PresentToHwQueue queues NOTHING, so the token is acquired immediately
    // before the write. That adjacency is the documentation: there is no host
    // work here for the validate-before-submit ordering to protect.
    let capacity = match present_allocations.validate_patch_capacity(args) {
        Ok(capacity) => capacity,
        Err(status) => {
            crate::diag::record_named_bytes(b"PHQst", status as u32);
            return status;
        }
    };
    if let Err(status) = unsafe { present_allocations.write_patch_references(capacity, args) } {
        crate::diag::record_named_bytes(b"PHQst", status as u32);
        return status;
    }

    if !args.pDmaBuffer.is_null() {
        const PRESENT_NOP_DWORDS: usize = 4;
        let bytes = (PRESENT_NOP_DWORDS * core::mem::size_of::<u32>()) as UINT;
        if args.DmaSize < bytes {
            crate::diag::record_named_bytes(
                b"PHQst",
                STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER as u32,
            );
            return STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER;
        }
        let dma = args.pDmaBuffer as *mut u32;
        unsafe {
            // SAFETY: DmaSize was checked for the four DWORD no-op record.
            *dma.add(0) = 0x5150_4548; // "HEPQ"
            *dma.add(1) = DXGK_PRESENT_SOURCE_INDEX;
            *dma.add(2) = DXGK_PRESENT_DESTINATION_INDEX;
            *dma.add(3) = present_flags;
            args.pDmaBuffer = (args.pDmaBuffer as *mut u8).add(bytes as usize).cast();
        }
        args.MultipassOffset = 0;
    }

    crate::diag::record_named_bytes(b"PHQst", STATUS_SUCCESS as u32);
    STATUS_SUCCESS
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
