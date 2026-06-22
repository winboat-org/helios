//! Command-submission and TDR DDIs.
//!
//! Render work is still disabled, but the scheduler-facing submission path must
//! be able to retire early paging/null-engine DMA buffers without timing out.

use core::ffi::c_void;
use core::mem::size_of;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::adapter::AdapterContext;
use crate::dxgk::_DXGK_INTERRUPT_TYPE::DXGK_INTERRUPT_DMA_COMPLETED;
use crate::dxgk::*;

// ── DISPATCH-safe instrumentation (Step-2 coherent-fence bring-up) ───────────
// `dxgkddi_submit_command` runs at DISPATCH_LEVEL (and the render path may too),
// where `diag::record` (PASSIVE-only) is illegal. We trace via these atomics and
// mirror them into the registry ring at DxgkDdiDestroyDevice (see
// `diag_dump_engine_atomics`). The decisive question this answers: does VidSch
// drive SubmitCommand / Render at all before `VidSchTerminateAdapter` fires right
// after CreateContext? (If none of these advance, the engine path is downstream
// of the Code-43 blocker and the real cause is in the caps/context config.)
pub static SUBMIT_COUNT: AtomicU32 = AtomicU32::new(0);
pub static SUBMIT_PAGING_COUNT: AtomicU32 = AtomicU32::new(0);
pub static SUBMIT_LAST_FENCE: AtomicU32 = AtomicU32::new(0);
pub static RENDER_COUNT: AtomicU32 = AtomicU32::new(0);
pub static PATCH_COUNT: AtomicU32 = AtomicU32::new(0);
pub static PREEMPT_COUNT: AtomicU32 = AtomicU32::new(0);

/// Mirror the DISPATCH-safe engine tracers into the PASSIVE diag ring. Call ONLY
/// from a PASSIVE DDI (DxgkDdiDestroyDevice). Codes (continuing the 0x0F.. space
/// used by `build_paging_buffer::diag_dump_gpummu_atomics`):
///   0x0F06_NNNN = SubmitCommand call count
///   0x0F07_FFFF = last SubmissionFenceId (low 16)
///   0x0F08_NNNN = paging-submit count (Flags.Paging == 1)
///   0x0F09_NNNN = Render call count
///   0x0F0A_NNNN = Patch call count
///   0x0F0B_NNNN = PreemptCommand call count
///   0x0F0C_NNNN = InterruptRoutine delivery count
///   0x0F0D_NNNN = DpcRoutine count
///   0x0F0E_NNNN = ControlInterrupt count
pub fn diag_dump_engine_atomics() {
    crate::diag::record(0x0F06_0000 | (SUBMIT_COUNT.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x0F07_0000 | (SUBMIT_LAST_FENCE.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x0F08_0000 | (SUBMIT_PAGING_COUNT.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x0F09_0000 | (RENDER_COUNT.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x0F0A_0000 | (PATCH_COUNT.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x0F0B_0000 | (PREEMPT_COUNT.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(
        0x0F0C_0000 | (super::interrupt::INT_ROUTINE_COUNT.load(Ordering::Relaxed) & 0xFFFF),
    );
    crate::diag::record(
        0x0F0D_0000 | (super::interrupt::DPC_ROUTINE_COUNT.load(Ordering::Relaxed) & 0xFFFF),
    );
    crate::diag::record(
        0x0F0E_0000 | (super::interrupt::CONTROL_INT_COUNT.load(Ordering::Relaxed) & 0xFFFF),
    );
}

/// Context handed to [`notify_dma_completed_routine`] across the
/// `DxgkCbSynchronizeExecution` boundary (it runs at the device's DIRQL).
struct NotifyDmaCompletedCtx {
    dxgkrnl: *const DXGKRNL_INTERFACE,
    interrupt: *mut DXGKARGCB_NOTIFY_INTERRUPT_DATA,
}

/// Runs at the device's interrupt IRQL (DIRQL), synchronized with the ISR — the
/// only level at which `DxgkCbNotifyInterrupt` may be called. Mirrors viogpu3d's
/// `NotifyRoutine` (`viogpu_adapter.cpp:50-72`).
unsafe extern "C" fn notify_dma_completed_routine(context: *mut c_void) -> BOOLEAN {
    if context.is_null() {
        return 0;
    }
    // SAFETY: `context` is the `NotifyDmaCompletedCtx` we passed to
    // DxgkCbSynchronizeExecution; valid for the duration of that synchronous call.
    let ctx = unsafe { &*(context as *const NotifyDmaCompletedCtx) };
    let dxgkrnl = unsafe { &*ctx.dxgkrnl };
    if let Some(notify_interrupt) = dxgkrnl.DxgkCbNotifyInterrupt {
        // SAFETY: at DIRQL (raised by DxgkCbSynchronizeExecution); `interrupt`
        // points to a fully-initialized DMA_COMPLETED packet, live for this call.
        unsafe { notify_interrupt(dxgkrnl.DeviceHandle, ctx.interrupt) };
    }
    1 // TRUE
}

/// Signal `DXGK_INTERRUPT_DMA_COMPLETED` for `fence` at the correct IRQL: fill the
/// packet, hand it to `DxgkCbNotifyInterrupt` from inside a `DxgkCbSynchronizeExecution`
/// callback (which raises to the device's DIRQL), then `DxgkCbQueueDpc` so dxgkrnl
/// drains the packet and advances the software fence. Callable at <= DIRQL.
unsafe fn signal_dma_completed(dxgkrnl: &DXGKRNL_INTERFACE, fence: u32) -> NTSTATUS {
    let mut interrupt = unsafe { core::mem::zeroed::<DXGKARGCB_NOTIFY_INTERRUPT_DATA>() };
    interrupt.InterruptType = DXGK_INTERRUPT_DMA_COMPLETED;
    // SAFETY: bindgen lowered the per-type union to __BindgenUnionField accessors;
    // DmaCompleted is the correct arm for DXGK_INTERRUPT_DMA_COMPLETED.
    let completed = unsafe { interrupt.__bindgen_anon_1.DmaCompleted.as_mut() };
    completed.SubmissionFenceId = fence;
    completed.NodeOrdinal = 0;
    completed.EngineOrdinal = 0;

    let ctx = NotifyDmaCompletedCtx {
        dxgkrnl: dxgkrnl as *const DXGKRNL_INTERFACE,
        interrupt: &mut interrupt as *mut DXGKARGCB_NOTIFY_INTERRUPT_DATA,
    };

    if let Some(sync) = dxgkrnl.DxgkCbSynchronizeExecution {
        let mut ret: BOOLEAN = 0;
        // SAFETY: valid DeviceHandle; the routine + context live for the call.
        unsafe {
            sync(
                dxgkrnl.DeviceHandle,
                Some(notify_dma_completed_routine),
                &ctx as *const _ as *mut c_void,
                0,
                &mut ret,
            );
        }
    } else {
        return STATUS_DEVICE_NOT_READY;
    }

    if let Some(queue_dpc) = dxgkrnl.DxgkCbQueueDpc {
        // SAFETY: valid DeviceHandle.
        let _ = unsafe { queue_dpc(dxgkrnl.DeviceHandle) };
    }
    STATUS_SUCCESS
}

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

    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let submit = unsafe { &*submit_command };
    let fence = submit.SubmissionFenceId;

    // Instrument: which submissions does VidSch actually drive during bring-up?
    SUBMIT_COUNT.fetch_add(1, Ordering::Relaxed);
    SUBMIT_LAST_FENCE.store(fence, Ordering::Relaxed);
    // Flags.Paging is bit 0 of the flags word; read it via the union's `Value`
    // arm (the bitfield accessor lives behind the same union).
    // SAFETY: `Value` is a plain UINT view of the (valid) flags union.
    let is_paging = (unsafe { submit.Flags.__bindgen_anon_1.Value } & 1) != 0;
    if is_paging {
        SUBMIT_PAGING_COUNT.fetch_add(1, Ordering::Relaxed);
    }

    // Coherent completion model (Step-2 gate scope):
    //
    //   * Paging / null-engine buffers (Flags.Paging == 1, or any buffer carrying
    //     no venus stream) have no host work — the decorative GpuMmu has nothing
    //     to program — so we complete the fence directly. This keeps dxgkrnl's
    //     paging path from timing out and is the only submission VidSch issues
    //     during adapter bring-up.
    //   * Venus render buffers (produced by a UMD via DxgkDdiRender, with a
    //     HeliosWddmCmdBuf at the head) would be forwarded to the host here, then
    //     completed. That forwarding is DEFERRED until a UMD actually drives the
    //     render path: it cannot be exercised during VidSch init (which submits
    //     only paging/null buffers), and the venus stream sits in the GPU-VA /
    //     physical DMA buffer (no CPU VA in DXGKARG_SUBMITCOMMAND), so wiring it
    //     correctly belongs with the UMD bring-up, not this scheduler-gate pass.
    //     See WDDM_FAKE_VIDMM_RESEARCH.md §A6.9 / §C.1 Option 1.
    //
    // In all cases the fence is signaled via DxgkCbNotifyInterrupt at the device's
    // DIRQL (DxgkCbSynchronizeExecution), NOT directly at DISPATCH_LEVEL — the
    // earlier direct call was an IRQL contract violation (A6.5/A6.6).
    adapter.last_completed_fence.store(fence, Ordering::Release);

    let dxgkrnl = match adapter.dxgkrnl() {
        Ok(interface) => interface,
        Err(_) => return STATUS_DEVICE_NOT_READY,
    };
    // SAFETY: dxgkrnl is the live callback interface; signal at correct IRQL.
    unsafe { signal_dma_completed(dxgkrnl, fence) }
}

pub unsafe extern "C" fn dxgkddi_preempt_command(
    _h_adapter: *mut c_void,
    _preempt_command: *const DXGKARG_PREEMPTCOMMAND,
) -> NTSTATUS {
    PREEMPT_COUNT.fetch_add(1, Ordering::Relaxed);
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

/// `DxgkDdiRender` — record a DMA buffer from a UMD command buffer.
///
/// Our UMD command buffer already begins with a `HeliosWddmCmdBuf` followed by the
/// opaque venus stream (`protocol/src/wddm.rs`), so "recording" is a straight copy
/// of `pCommand` into `pDmaBuffer`; there are no guest GPU-VAs to translate
/// (decorative GpuMmu — the host owns the real MMU), so the patch-location list is
/// passed through untouched and the matching `DxgkDdiPatch` is a no-op. The venus
/// forwarding itself happens at submit/complete time (see `dxgkddi_submit_command`).
///
/// NOTE: not exercised during VidSch adapter bring-up (no UMD/app is rendering
/// yet); present so the render-capable DDI contract is real rather than a
/// NOT_IMPLEMENTED stub.
pub unsafe extern "C" fn dxgkddi_render(
    _h_context: IN_CONST_HANDLE,
    render: INOUT_PDXGKARG_RENDER,
) -> NTSTATUS {
    RENDER_COUNT.fetch_add(1, Ordering::Relaxed);
    if render.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let args = unsafe { &mut *render };
    if args.pCommand.is_null() || args.pDmaBuffer.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let cmd_len = args.CommandLength as usize;
    let dma_cap = args.DmaSize as usize;
    if cmd_len == 0 || cmd_len > dma_cap {
        // Buffer too small for the recorded command: ask the runtime to grow it.
        return STATUS_BUFFER_TOO_SMALL;
    }
    // SAFETY: the runtime guarantees `pCommand` has `CommandLength` readable bytes
    // and `pDmaBuffer` has `DmaSize` writable bytes; we copy at most `cmd_len`
    // (<= DmaSize) and the ranges do not overlap (distinct allocations).
    unsafe {
        core::ptr::copy_nonoverlapping(
            args.pCommand as *const u8,
            args.pDmaBuffer as *mut u8,
            cmd_len,
        );
    }
    STATUS_SUCCESS
}

/// `DxgkDdiRenderKm` — kernel-mode (GDI) render path.
pub unsafe extern "C" fn dxgkddi_render_km(
    _h_context: IN_CONST_HANDLE,
    _render: INOUT_PDXGKARG_RENDER,
) -> NTSTATUS {
    STATUS_NOT_IMPLEMENTED
}

/// `DxgkDdiPatch` — patch allocation references in a DMA buffer.
///
/// No-op success, like viogpu3d (`viogpu_command.cpp:289-298`): the decorative
/// GpuMmu has no guest GPU-VAs to patch (venus addresses resources by opaque id,
/// the host owns the real MMU), so there is nothing to fix up. Must return SUCCESS
/// (not NOT_IMPLEMENTED) for a render-capable adapter.
pub unsafe extern "C" fn dxgkddi_patch(
    _h_adapter: IN_CONST_HANDLE,
    patch: IN_CONST_PDXGKARG_PATCH,
) -> NTSTATUS {
    PATCH_COUNT.fetch_add(1, Ordering::Relaxed);
    if patch.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    STATUS_SUCCESS
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
