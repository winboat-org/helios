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
use crate::device::DeviceHandleRef;
use crate::dxgk::*;

const HW_TAG_CONTEXT: u32 = 0x4843_5458; // "HCTX"

/// The one opaque object this driver hands dxgkrnl for hardware scheduling.
///
/// `HwContext` and `HwQueue` used to be two layout-identical `#[repr(C)]`
/// structs, and `hw_queue_adapter` read the tag TWICE off the same address --
/// forming a `&HwQueue`, testing its magic, then forming a `&HwContext` over
/// the same bytes and testing again. That was benign only BECAUSE the layouts
/// matched, which no comment said; the SAFETY note claimed merely "the same
/// opaque-handle contract".
///
/// ⚠ `tag` is deliberately a plain `u32` and NOT a `#[repr(u32)]` enum: it is
/// read out of OS-supplied memory, and an out-of-range discriminant would be UB
/// at construction rather than a value we can reject.
#[repr(C)]
struct HwHandle {
    tag: u32,
    adapter: *mut AdapterContext,
}

static PRESENT_HWQ_COUNT: AtomicU32 = AtomicU32::new(0);

/// Validate a caller-supplied hardware-scheduling handle, reading the tag ONCE.
///
/// # Safety
/// `h` is an opaque handle this driver previously returned from
/// `DxgkDdiCreateHwContext`, or null. The deref that reads `tag` cannot be
/// avoided -- the tag lives INSIDE the memory being validated -- but it is now
/// a single read of a `#[repr(C)]` prefix instead of two speculative ones.
unsafe fn hw_handle(h: IN_CONST_HANDLE) -> Option<&'static HwHandle> {
    if h.is_null() {
        return None;
    }
    // SAFETY: per the contract above; `HwHandle` is `#[repr(C)]` with `tag`
    // first, so the read is in-bounds for any handle we minted.
    let handle = unsafe { &*(h as *const HwHandle) };
    (handle.tag == HW_TAG_CONTEXT && !handle.adapter.is_null()).then_some(handle)
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

    let ctx = Box::new(HwHandle {
        tag: HW_TAG_CONTEXT,
        // Stored, not borrowed: `HwHandle` outlives this call.
        adapter: device.adapter_ptr(),
    });
    args.hHwContext = Box::into_raw(ctx) as HANDLE;
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_destroy_hw_context(h_hw_context: IN_CONST_HANDLE) -> NTSTATUS {
    crate::diag::record(0x0700_0002);
    if !h_hw_context.is_null() {
        drop(unsafe { Box::from_raw(h_hw_context as *mut HwHandle) });
    }
    STATUS_SUCCESS
}

/// REFUSE AT THE FIRST STEP. This driver advertises no hardware-scheduling
/// capability anywhere -- a repo-wide search for `HwSched` / `HardwareSchedul` /
/// `HwQueueSupported` finds nothing, and `query_adapter_info.rs` sets only
/// `MultiEngineAware | PreemptionAware`.
///
/// It used to hand back a magic-tagged `Box`ed queue and succeed, while
/// `SubmitCommandToHwQueue` returned `STATUS_NOT_SUPPORTED` -- the worst
/// possible pairing, because the scheduler has already committed to the queue
/// by the time the submission fails, and this file's own comment documents that
/// shape as VidSch bugcheck `0x119`/Arg1=2. Failing here means no queue handle
/// exists, so a submission against one is unrepresentable.
///
/// Evidence gate (ROADMAP 7g(b)): `PHQcall` is ABSENT from the service key
/// entirely -- `PresentToHwQueue` has never been called in the key's lifetime.
pub unsafe extern "C" fn dxgkddi_create_hw_queue(
    _h_hw_context: IN_CONST_HANDLE,
    _args: INOUT_PDXGKARG_CREATEHWQUEUE,
) -> NTSTATUS {
    crate::diag::record(0x0700_0003);
    crate::diag::record_named_bytes(b"HwQRef", 1);
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_destroy_hw_queue(_h_hw_queue: IN_CONST_HANDLE) -> NTSTATUS {
    crate::diag::record(0x0700_0004);
    // Nothing to free: `CreateHwQueue` never hands out an object. The slot stays
    // REGISTERED because this driver has been bitten twice by absent/null DDI
    // slots (`DxgkDdiUpdateMonitorLinkInfo` Code 43, `DdiRenderGdi` null-pointer
    // bugcheck), so unregistering is a separate behaviour change.
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

/// REFUSE, and count. Consistent with `CreateHwQueue` above: no queue handle is
/// ever handed out, so nothing can legitimately present to one.
///
/// This used to return SUCCESS after 8+ named registry writes per call, a
/// four-DWORD "HEPQ" DMA record and a `PresentAllocations` patch-reference copy
/// -- succeeding at present while the paired submit failed, which is the
/// inconsistency the whole item is about.
///
/// `PHQcall` stays: it is the reachability instrument, and after this change a
/// non-zero reading means the OS IS routing work here and the owner needs to
/// know. Every other `PHQ*` value goes.
pub unsafe extern "C" fn dxgkddi_present_to_hw_queue(
    h_hw_queue: IN_CONST_HANDLE,
    _args: INOUT_PDXGKARG_PRESENT,
) -> NTSTATUS {
    crate::diag::record(0x0700_0007);
    PRESENT_HWQ_COUNT.fetch_add(1, Ordering::Relaxed);
    crate::diag::record_named_bytes(b"PHQcall", PRESENT_HWQ_COUNT.load(Ordering::Relaxed));
    // Read the tag once, purely so a nonzero PHQcall can be attributed to a
    // handle we actually minted rather than to a stray pointer.
    // SAFETY: an opaque handle this driver returned, or null; `hw_handle`
    // null-checks and tag-checks before trusting it.
    let ours = unsafe { hw_handle(h_hw_queue) }.is_some();
    crate::diag::record_named_bytes(b"PHQours", u32::from(ours));
    crate::diag::record_named_bytes(b"PHQst", STATUS_NOT_SUPPORTED as u32);
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
