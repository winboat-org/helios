//! Command-submission and TDR DDIs.
//!
//! Render work is still disabled, but the scheduler-facing submission path must
//! be able to retire early paging/null-engine DMA buffers without timing out.

use core::ffi::c_void;
use core::mem::size_of;
use core::sync::atomic::Ordering;

use crate::adapter::AdapterContext;
use crate::dxgk::_DXGK_INTERRUPT_TYPE::DXGK_INTERRUPT_DMA_COMPLETED;
use crate::dxgk::*;

pub unsafe extern "C" fn dxgkddi_submit_command_virtual(
    _h_adapter: *mut c_void,
    _submit_command: *const DXGKARG_SUBMITCOMMANDVIRTUAL,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

/// `DxgkDdiSubmitCommand` — submit a DMA buffer to the GPU. Critically, this is
/// also how Dxgkrnl queues *paging* buffers (built by DxgkDdiBuildPagingBuffer,
/// with `hDevice == NULL`); since we register paging, this slot must be present.
// Runs at DISPATCH_LEVEL. Once this path is reachable, it must not fail in
// normal operation; until then, the driver must not advertise render capability.
pub unsafe extern "C" fn dxgkddi_submit_command(
    h_adapter: IN_CONST_HANDLE,
    submit_command: IN_CONST_PDXGKARG_SUBMITCOMMAND,
) -> NTSTATUS {
    if h_adapter.is_null() || submit_command.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    // Phase-1 scheduler bring-up: complete the submitted DMA buffer immediately.
    // This is intentionally a null engine, used to keep dxgkrnl's paging path
    // from timing out before real Venus/DXVK/VKD3D command submission exists.
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let submit = unsafe { &*submit_command };
    let fence = submit.SubmissionFenceId;
    adapter.last_completed_fence.store(fence, Ordering::Release);

    let dxgkrnl = match adapter.dxgkrnl() {
        Ok(interface) => interface,
        Err(_) => return STATUS_DEVICE_NOT_READY,
    };

    let mut interrupt = unsafe { core::mem::zeroed::<DXGKARGCB_NOTIFY_INTERRUPT_DATA>() };
    interrupt.InterruptType = DXGK_INTERRUPT_DMA_COMPLETED;
    let completed = unsafe { interrupt.__bindgen_anon_1.DmaCompleted.as_mut() };
    completed.SubmissionFenceId = fence;
    completed.NodeOrdinal = 0;
    completed.EngineOrdinal = 0;

    if let Some(notify_interrupt) = dxgkrnl.DxgkCbNotifyInterrupt {
        unsafe { notify_interrupt(dxgkrnl.DeviceHandle, &mut interrupt) };
    } else {
        return STATUS_DEVICE_NOT_READY;
    }

    if let Some(queue_dpc) = dxgkrnl.DxgkCbQueueDpc {
        let _ = unsafe { queue_dpc(dxgkrnl.DeviceHandle) };
    }

    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_preempt_command(
    _h_adapter: *mut c_void,
    _preempt_command: *const DXGKARG_PREEMPTCOMMAND,
) -> NTSTATUS {
    STATUS_NOT_IMPLEMENTED
}

/// `DxgkDdiResetFromTimeout` — TDR recovery (no engines to reset yet).
pub unsafe extern "C" fn dxgkddi_reset_from_timeout(_h_adapter: *mut c_void) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

/// `DxgkDdiRestartFromTimeout` — resume after TDR.
pub unsafe extern "C" fn dxgkddi_restart_from_timeout(_h_adapter: *mut c_void) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

// ── Render-path DDIs. ───────────────────────────────────────────────────────

/// `DxgkDdiRender` — record/patch a DMA buffer from a command buffer.
pub unsafe extern "C" fn dxgkddi_render(
    _h_context: IN_CONST_HANDLE,
    _render: INOUT_PDXGKARG_RENDER,
) -> NTSTATUS {
    STATUS_NOT_IMPLEMENTED
}

/// `DxgkDdiRenderKm` — kernel-mode (GDI) render path.
pub unsafe extern "C" fn dxgkddi_render_km(
    _h_context: IN_CONST_HANDLE,
    _render: INOUT_PDXGKARG_RENDER,
) -> NTSTATUS {
    STATUS_NOT_IMPLEMENTED
}

/// `DxgkDdiPatch` — patch allocation references in a DMA buffer.
pub unsafe extern "C" fn dxgkddi_patch(
    _h_adapter: IN_CONST_HANDLE,
    _patch: IN_CONST_PDXGKARG_PATCH,
) -> NTSTATUS {
    STATUS_NOT_IMPLEMENTED
}

/// `DxgkDdiQueryCurrentFence` — report the last fence the GPU completed.
pub unsafe extern "C" fn dxgkddi_query_current_fence(
    h_adapter: IN_CONST_HANDLE,
    query_current_fence: INOUT_PDXGKARG_QUERYCURRENTFENCE,
) -> NTSTATUS {
    if h_adapter.is_null() || query_current_fence.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let query = unsafe { &mut *query_current_fence };
    unsafe {
        core::ptr::write_bytes(
            query as *mut _ as *mut u8,
            0,
            size_of::<DXGKARG_QUERYCURRENTFENCE>(),
        );
    }
    query.CurrentFence = adapter.last_completed_fence.load(Ordering::Acquire);
    query.NodeOrdinal = 0;
    query.EngineOrdinal = 0;
    STATUS_SUCCESS
}

/// `DxgkDdiCollectDbgInfo` — dump driver debug state on a TDR/bugcheck.
pub unsafe extern "C" fn dxgkddi_collect_dbg_info(
    _h_adapter: IN_CONST_HANDLE,
    _collect_dbg_info: IN_CONST_PDXGKARG_COLLECTDBGINFO,
) -> NTSTATUS {
    STATUS_NOT_IMPLEMENTED
}
