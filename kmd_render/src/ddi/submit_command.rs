//! Command-submission and TDR DDIs.
//!
//! Render work is still disabled, but the scheduler-facing submission path must
//! be able to retire early paging/null-engine DMA buffers without timing out.

use core::ffi::c_void;
use core::mem::size_of;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::adapter::{AdapterContext, WddmNotifyGuard};
use crate::ddi::present_packet::PresentSubmissionPrivate;
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
pub static DMA_NOTIFY_COUNT: AtomicU32 = AtomicU32::new(0);
pub static DMA_QUEUE_DPC_COUNT: AtomicU32 = AtomicU32::new(0);
pub static DMA_SYNC_STATUS_LOW: AtomicU32 = AtomicU32::new(0);
pub static DMA_SYNC_RET: AtomicU32 = AtomicU32::new(0);
/// `DXGK_INTERRUPT_DMA_COMPLETED` deliveries that failed and whose fence was
/// therefore put back at the head of the pending FIFO for a later DPC.
///
/// This is the counter that did not exist: `DMA_SYNC_STATUS_LOW`/`DMA_SYNC_RET`
/// are last-value-wins, so a later successful notify erased the only trace that
/// a fence had been lost. Nonzero means the retry path ran; a *rising* value on
/// an otherwise healthy boot means dxgkrnl is repeatedly refusing the
/// synchronized callback.
pub static DMA_NOTIFY_FAILS: AtomicU32 = AtomicU32::new(0);
/// Older DMA_COMPLETED packets suppressed after a newer watermark won the
/// cross-CPU notification race. The newer watermark implicitly retires them.
pub static DMA_STALE_SKIP_COUNT: AtomicU32 = AtomicU32::new(0);

// Present private-data handoff diagnostics. These are atomics because both
// SubmitCommand entry points run at DISPATCH_LEVEL; a throttled PASSIVE scanout
// telemetry site mirrors them to the registry.
pub static SUBMIT_VIRTUAL_COUNT: AtomicU32 = AtomicU32::new(0);
pub static SUBMIT_LEGACY_COUNT: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_MARKER_HITS: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_MARKER_SCAN_HITS: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_MARKER_LAST_OFFSET: AtomicU32 = AtomicU32::new(u32::MAX);
/// Private-data shape of the LAST submission ON EACH PATH.
///
/// These used to be ONE shared set written by both SubmitCommand entry points,
/// so `PmTot`/`PmUmd`/`PmSta`/`PmEnd`/`PmB0`/`PmX0` described whichever DDI ran
/// last and a mixed workload (the GpuMmu path uses the virtual DDI, paging
/// buffers arrive on the legacy one) produced a self-contradictory registry
/// snapshot — e.g. a legacy start/end offset paired with the virtual path's
/// UMD size. The existing names stay bound to the LEGACY path, which is the one
/// that reports real start/end offsets; the virtual path gets `PmV*`
/// (k-ctrlsubmit-17).
pub struct PresentPrivateShape {
    pub total: AtomicU32,
    pub umd: AtomicU32,
    pub start: AtomicU32,
    pub end: AtomicU32,
    pub base_word: AtomicU32,
    pub expected_word: AtomicU32,
}

impl PresentPrivateShape {
    const fn new() -> Self {
        Self {
            total: AtomicU32::new(0),
            umd: AtomicU32::new(0),
            start: AtomicU32::new(0),
            end: AtomicU32::new(0),
            base_word: AtomicU32::new(0),
            expected_word: AtomicU32::new(0),
        }
    }
}

pub static PRESENT_PRIVATE_LEGACY: PresentPrivateShape = PresentPrivateShape::new();
pub static PRESENT_PRIVATE_VIRTUAL: PresentPrivateShape = PresentPrivateShape::new();

/// Which SubmitCommand entry point a decode came from. Makes it impossible for
/// the two decoders to share destination globals again.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SubmitPath {
    Virtual,
    Legacy,
}

impl SubmitPath {
    fn shape(self) -> &'static PresentPrivateShape {
        match self {
            SubmitPath::Virtual => &PRESENT_PRIVATE_VIRTUAL,
            SubmitPath::Legacy => &PRESENT_PRIVATE_LEGACY,
        }
    }
}
static PRESENT_MARKER_SCAN_ATTEMPTS: AtomicU32 = AtomicU32::new(0);

/// Mirror the scheduler private-data handoff evidence at PASSIVE_LEVEL.
pub(crate) fn record_present_handoff_telemetry() {
    use crate::ddi::present_packet::{
        PRESENT_MARKER_LAST_FENCE, PRESENT_MARKER_LAST_SIZE, PRESENT_MARKER_WRITES,
    };

    crate::diag::record_named_bytes(b"PmWr", PRESENT_MARKER_WRITES.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmWFn", PRESENT_MARKER_LAST_FENCE.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmWSz", PRESENT_MARKER_LAST_SIZE.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmHit", PRESENT_MARKER_HITS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmScan", PRESENT_MARKER_SCAN_HITS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmOff", PRESENT_MARKER_LAST_OFFSET.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmVir", SUBMIT_VIRTUAL_COUNT.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmLeg", SUBMIT_LEGACY_COUNT.load(Ordering::Relaxed));
    // Legacy path keeps the original names (it is the one with real start/end
    // offsets); the virtual path reports the same six values under PmV*.
    let legacy = &PRESENT_PRIVATE_LEGACY;
    crate::diag::record_named_bytes(b"PmTot", legacy.total.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmUmd", legacy.umd.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmSta", legacy.start.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmEnd", legacy.end.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmB0", legacy.base_word.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmX0", legacy.expected_word.load(Ordering::Relaxed));
    let virt = &PRESENT_PRIVATE_VIRTUAL;
    crate::diag::record_named_bytes(b"PmVTot", virt.total.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmVUmd", virt.umd.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmVSta", virt.start.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmVEnd", virt.end.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmVB0", virt.base_word.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmVX0", virt.expected_word.load(Ordering::Relaxed));
}

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
///   0x0F0F_NNNN = DMA-complete NotifyInterrupt count
///   0x0F10_NNNN = DMA-complete QueueDpc count
///   0x0F11_NNNN = DxgkCbSynchronizeExecution status low 16
///   0x0F12_NNNN = DxgkCbSynchronizeExecution return BOOLEAN
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
    crate::diag::record(0x0F0F_0000 | (DMA_NOTIFY_COUNT.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x0F10_0000 | (DMA_QUEUE_DPC_COUNT.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x0F11_0000 | (DMA_SYNC_STATUS_LOW.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x0F12_0000 | (DMA_SYNC_RET.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x0F18_0000 | (DMA_STALE_SKIP_COUNT.load(Ordering::Relaxed) & 0xFFFF));
    // C3/M3.4 async-transport atoms:
    //   0x0F13_NNNN = async SUBMIT_3D enqueues   0x0F14_NNNN = completions
    //   0x0F15_NNNN = WDDM fences completed from the DPC
    //   0x0F16_NNNN = WAIT_FENCE timeouts        0x0F17_NNNN = sync cmd timeouts
    crate::diag::record(
        0x0F13_0000 | (crate::virtio::gpu::ASYNC_SUBMIT_COUNT.load(Ordering::Relaxed) & 0xFFFF),
    );
    crate::diag::record(
        0x0F14_0000 | (crate::virtio::gpu::ASYNC_COMPLETE_COUNT.load(Ordering::Relaxed) & 0xFFFF),
    );
    crate::diag::record(
        0x0F15_0000 | (crate::virtio::gpu::WDDM_FENCE_FROM_DPC.load(Ordering::Relaxed) & 0xFFFF),
    );
    crate::diag::record(
        0x0F16_0000 | (crate::virtio::gpu::FENCE_WAIT_TIMEOUTS.load(Ordering::Relaxed) & 0xFFFF),
    );
    crate::diag::record(
        0x0F17_0000 | (crate::virtio::gpu::CTRL_TIMEOUT_COUNT.load(Ordering::Relaxed) & 0xFFFF),
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
        DMA_NOTIFY_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(queue_dpc) = dxgkrnl.DxgkCbQueueDpc {
        // viogpu3d queues the DPC from the synchronized interrupt routine, while
        // still at the device DIRQL. Keep that ordering so dxgkrnl sees the
        // notify+DPC pair as one interrupt-completion event.
        unsafe { queue_dpc(dxgkrnl.DeviceHandle) };
        DMA_QUEUE_DPC_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    1 // TRUE
}

/// Deliver a prepared `DXGKARGCB_NOTIFY_INTERRUPT_DATA` packet at the correct
/// IRQL: hand it to `DxgkCbNotifyInterrupt` from inside a
/// `DxgkCbSynchronizeExecution` callback (which raises to the device's DIRQL),
/// then `DxgkCbQueueDpc` so dxgkrnl drains the packet. Callable at <= DIRQL.
unsafe fn notify_at_dirql(
    dxgkrnl: &DXGKRNL_INTERFACE,
    interrupt: &mut DXGKARGCB_NOTIFY_INTERRUPT_DATA,
) -> NTSTATUS {
    let ctx = NotifyDmaCompletedCtx {
        dxgkrnl: dxgkrnl as *const DXGKRNL_INTERFACE,
        interrupt: interrupt as *mut DXGKARGCB_NOTIFY_INTERRUPT_DATA,
    };

    if let Some(sync) = dxgkrnl.DxgkCbSynchronizeExecution {
        let mut ret: BOOLEAN = 0;
        // SAFETY: valid DeviceHandle; the routine + context live for the call.
        let status = unsafe {
            sync(
                dxgkrnl.DeviceHandle,
                Some(notify_dma_completed_routine),
                &ctx as *const _ as *mut c_void,
                0,
                &mut ret,
            )
        };
        DMA_SYNC_STATUS_LOW.store(status as u32, Ordering::Relaxed);
        DMA_SYNC_RET.store(ret as u32, Ordering::Relaxed);
        if status != STATUS_SUCCESS {
            return status;
        }
        if ret == 0 {
            return STATUS_DEVICE_NOT_READY;
        }
    } else {
        return STATUS_DEVICE_NOT_READY;
    }
    STATUS_SUCCESS
}

/// Signal `DXGK_INTERRUPT_DMA_COMPLETED` for `fence` (see [`notify_at_dirql`]),
/// with the adapter's WDDM notification lock ALREADY HELD -- the only door.
///
/// Queue arbitration and the callback must be one critical section; otherwise a
/// DPC can pop fence N, a concurrent submit can observe an empty FIFO and report
/// N+1 first, and VidSch bugchecks 0x119/1.
///
/// T6/R914 deleted the sibling wrapper that took `&AdapterContext` and acquired
/// the lock itself. It had zero callers and was not re-exported, and the invalid
/// sequence it invited is real: `with_wddm_notify_lock` uses
/// `KeAcquireSpinLockRaiseToDpc` and a `KSPIN_LOCK` is not recursive, so the
/// first caller to reach for it from INSIDE the guard hard-hangs a CPU at
/// DISPATCH. Requiring a `&WddmNotifyGuard` removes the footgun; it does not
/// remove the class, since a hand-written nested `with_wddm_notify_lock` is
/// still writable.
pub(crate) unsafe fn signal_dma_completed(
    guard: &WddmNotifyGuard<'_>,
    dxgkrnl: &DXGKRNL_INTERFACE,
    fence: u32,
) -> NTSTATUS {
    let last = guard.completed_fence();
    // Sequence comparison remains correct across u32 wrap: a forward id is
    // within the next half of the sequence space; equal/backward is stale.
    let forward = fence != last && fence.wrapping_sub(last) < 0x8000_0000;
    if !forward {
        DMA_STALE_SKIP_COUNT.fetch_add(1, Ordering::Relaxed);
        return STATUS_SUCCESS;
    }

    let mut interrupt = unsafe { core::mem::zeroed::<DXGKARGCB_NOTIFY_INTERRUPT_DATA>() };
    interrupt.InterruptType = DXGK_INTERRUPT_DMA_COMPLETED;
    // SAFETY: bindgen lowered the per-type union to __BindgenUnionField accessors;
    // DmaCompleted is the correct arm for DXGK_INTERRUPT_DMA_COMPLETED.
    let completed = unsafe { interrupt.__bindgen_anon_1.DmaCompleted.as_mut() };
    completed.SubmissionFenceId = fence;
    completed.NodeOrdinal = 0;
    completed.EngineOrdinal = 0;
    // SAFETY: fully-initialized packet, live for the call.
    let status = unsafe { notify_at_dirql(dxgkrnl, &mut interrupt) };
    if status == STATUS_SUCCESS {
        guard.set_completed_fence(fence);
    }
    status
}

/// Synthesize a `DXGK_INTERRUPT_CRTC_VSYNC` for the display half's single target
/// (viogpu3d FlipThread analog, `viogpu_vidpn.cpp:1977-1983`). `physical_address`
/// is the primary currently bound via `SetVidPnSourceAddress` (0 before the first
/// bind); dxgkrnl retires the queued flip whose address matches. `target_id` is the
/// video-present target the VSync belongs to. Callable at <= DIRQL (the DPC path).
pub(crate) unsafe fn signal_crtc_vsync(
    dxgkrnl: &DXGKRNL_INTERFACE,
    physical_address: i64,
    target_id: u32,
) -> NTSTATUS {
    let mut interrupt = unsafe { core::mem::zeroed::<DXGKARGCB_NOTIFY_INTERRUPT_DATA>() };
    interrupt.InterruptType = _DXGK_INTERRUPT_TYPE::DXGK_INTERRUPT_CRTC_VSYNC;
    // SAFETY: CrtcVsync is the correct union arm for DXGK_INTERRUPT_CRTC_VSYNC.
    let vsync = unsafe { interrupt.__bindgen_anon_1.CrtcVsync.as_mut() };
    vsync.VidPnTargetId = target_id;
    vsync.PhysicalAddress.QuadPart = physical_address;
    // SAFETY: fully-initialized packet, live for the call.
    unsafe { notify_at_dirql(dxgkrnl, &mut interrupt) }
}

/// Signal `DXGK_INTERRUPT_DMA_PREEMPTED` (see [`notify_at_dirql`]): the node's
/// pending submissions are released back to the scheduler, which resubmits the
/// incomplete ones later.
unsafe fn signal_dma_preempted_locked(
    guard: &WddmNotifyGuard<'_>,
    dxgkrnl: &DXGKRNL_INTERFACE,
    preempt_fence: u32,
) -> NTSTATUS {
    let mut interrupt = unsafe { core::mem::zeroed::<DXGKARGCB_NOTIFY_INTERRUPT_DATA>() };
    interrupt.InterruptType = _DXGK_INTERRUPT_TYPE::DXGK_INTERRUPT_DMA_PREEMPTED;
    // SAFETY: DmaPreempted is the correct arm for DXGK_INTERRUPT_DMA_PREEMPTED.
    let preempted = unsafe { interrupt.__bindgen_anon_1.DmaPreempted.as_mut() };
    preempted.PreemptionFenceId = preempt_fence;
    preempted.LastCompletedFenceId = guard.completed_fence();
    preempted.NodeOrdinal = 0;
    preempted.EngineOrdinal = 0;
    // SAFETY: fully-initialized packet, live for the call.
    unsafe { notify_at_dirql(dxgkrnl, &mut interrupt) }
}

/// Common submission handling (C3/M3.4): record the WDDM fence behind the venus
/// work outstanding at submit time. Signals `DMA_COMPLETED` immediately only
/// when nothing gates it (no async venus in flight, FIFO empty — e.g. paging
/// during bring-up) or the transport is down; otherwise the interrupt DPC
/// completes it once every async venus submission queued before it has retired
/// (the real venus-driven WDDM fence — WDDM_FAKE_VIDMM_RESEARCH §C).
/// The only thing `note_and_maybe_signal` can tell a SubmitCommand DDI.
///
/// This type exists so a transport or notification status physically cannot
/// become the DDI's return value. Both DDIs used to `return
/// note_and_maybe_signal(..)` verbatim, so a failed
/// `DxgkCbSynchronizeExecution` in a stop/rebalance window - the exact failure
/// R209 turns into a retry on the DPC path - was returned to VidSch as
/// STATUS_DEVICE_NOT_READY, and this file's own record says a non-SUCCESS return
/// here bugchecks dxgmms2!VidSchiSendToExecutionQueue with 0x119
/// VIDEO_SCHEDULER_INTERNAL_ERROR Arg1=2. CLAUDE.md's DDI rule says the same
/// thing in general: an illegal NTSTATUS is itself logged by dxgkrnl as a driver
/// bug. A failed notify has to be handled where it can be retried, not escalated.
enum SubmitAck {
    Accepted,
}

fn note_and_maybe_signal(
    adapter: &AdapterContext,
    fence: u32,
    is_paging: bool,
    gpu_completion_fence: Option<u64>,
) -> SubmitAck {
    let Ok(dxgkrnl) = adapter.dxgkrnl() else {
        // Effectively unreachable: dxgkrnl is set at StartDevice and never
        // cleared, and SubmitCommand cannot precede it. The submission stays in
        // the FIFO for the DPC either way.
        DMA_NOTIFY_FAILS.fetch_add(1, Ordering::Relaxed);
        return SubmitAck::Accepted;
    };
    adapter.with_wddm_notify_lock(|guard| {
        let signal_now = guard
            .with_virtio(|o, v| v.note_wddm_submission(o, fence, is_paging, gpu_completion_fence))
            // Transport down (bring-up / teardown): no venus work can gate it.
            .unwrap_or(true);
        if signal_now {
            // SAFETY: the notification lock is held and dxgkrnl is live.
            let status = unsafe { signal_dma_completed(guard, dxgkrnl, fence) };
            if status != STATUS_SUCCESS {
                // Same handling as the DPC path in R209: count it and leave the
                // retirement to a later DPC rather than failing the submission.
                DMA_NOTIFY_FAILS.fetch_add(1, Ordering::Relaxed);
                if let Some(queue_dpc) = dxgkrnl.DxgkCbQueueDpc {
                    // SAFETY: callable at <= DIRQL with a valid DeviceHandle;
                    // it does not take the notify lock we hold.
                    unsafe { queue_dpc(dxgkrnl.DeviceHandle) };
                }
            }
        }
    });
    SubmitAck::Accepted
}

/// The ONE present-marker decoder, shared by both SubmitCommand entry points.
///
/// `kmd_range` is the half of the private data the KMD owns: `umd..total` on the
/// virtual path (dxgkrnl reports a UMD prefix size) and `start..end` on the
/// legacy one (it reports an explicit submission window). Both then fall back to
/// decoding the whole buffer, and finally to the bounded evidence-only scan.
///
/// # Safety
/// `base` must be readable for `total` bytes for the duration of the call.
unsafe fn decode_present_fence(
    base: *const u8,
    total: usize,
    kmd_range: core::ops::Range<usize>,
    path: SubmitPath,
) -> Option<u64> {
    if !base.is_null() && kmd_range.start <= kmd_range.end && kmd_range.end <= total {
        let size = kmd_range.end - kmd_range.start;
        if let Some(fence) = unsafe {
            PresentSubmissionPrivate::decode(base.add(kmd_range.start).cast(), size as u32)
        } {
            PRESENT_MARKER_HITS.fetch_add(1, Ordering::Relaxed);
            PRESENT_MARKER_LAST_OFFSET.store(kmd_range.start as u32, Ordering::Relaxed);
            return Some(fence);
        }
    }
    if let Some(fence) = unsafe {
        PresentSubmissionPrivate::decode(base as *const c_void, total.min(u32::MAX as usize) as u32)
    } {
        PRESENT_MARKER_HITS.fetch_add(1, Ordering::Relaxed);
        PRESENT_MARKER_LAST_OFFSET.store(0, Ordering::Relaxed);
        return Some(fence);
    }
    let _ = path;
    unsafe { diagnostic_scan_present_private(base, total) };
    None
}

/// Record the private-data shape for `path`. Six relaxed stores plus two
/// length-checked unaligned reads, per submission, at DISPATCH — their only
/// purpose is the registry mirror, and they are now per-path so the mirror is
/// self-consistent.
unsafe fn note_present_private_shape(
    path: SubmitPath,
    base: *const u8,
    total: usize,
    umd: usize,
    start: usize,
    end: usize,
    expected_at: usize,
) {
    let shape = path.shape();
    shape
        .total
        .store(total.min(u32::MAX as usize) as u32, Ordering::Relaxed);
    shape
        .umd
        .store(umd.min(u32::MAX as usize) as u32, Ordering::Relaxed);
    shape
        .start
        .store(start.min(u32::MAX as usize) as u32, Ordering::Relaxed);
    shape
        .end
        .store(end.min(u32::MAX as usize) as u32, Ordering::Relaxed);
    shape.base_word.store(
        unsafe { diagnostic_private_word(base, total, 0) },
        Ordering::Relaxed,
    );
    shape.expected_word.store(
        unsafe { diagnostic_private_word(base, total, expected_at) },
        Ordering::Relaxed,
    );
}

unsafe fn decode_virtual_present_fence(submit: &DXGKARG_SUBMITCOMMANDVIRTUAL) -> Option<u64> {
    let base = submit.pDmaBufferPrivateData as *const u8;
    let total = submit.DmaBufferPrivateDataSize as usize;
    let umd = submit.DmaBufferUmdPrivateDataSize as usize;
    SUBMIT_VIRTUAL_COUNT.fetch_add(1, Ordering::Relaxed);
    // The virtual DDI has no submission start/end window: 0/0 is the truthful
    // report, and it no longer overwrites the legacy path's real offsets.
    unsafe { note_present_private_shape(SubmitPath::Virtual, base, total, umd, 0, 0, umd) };
    unsafe { decode_present_fence(base, total, umd..total, SubmitPath::Virtual) }
}

unsafe fn decode_legacy_present_fence(submit: &DXGKARG_SUBMITCOMMAND) -> Option<u64> {
    let base = submit.pDmaBufferPrivateData as *const u8;
    let total = submit.DmaBufferPrivateDataSize as usize;
    let start = submit.DmaBufferPrivateDataSubmissionStartOffset as usize;
    let end = submit.DmaBufferPrivateDataSubmissionEndOffset as usize;
    SUBMIT_LEGACY_COUNT.fetch_add(1, Ordering::Relaxed);
    unsafe { note_present_private_shape(SubmitPath::Legacy, base, total, 0, start, end, start) };
    unsafe { decode_present_fence(base, total, start..end, SubmitPath::Legacy) }
}

/// Read one diagnostic word without extending the trusted private-data range.
unsafe fn diagnostic_private_word(base: *const u8, total: usize, offset: usize) -> u32 {
    if base.is_null() || offset > total || total - offset < size_of::<u32>() {
        return 0;
    }
    unsafe { core::ptr::read_unaligned(base.add(offset).cast::<u32>()) }
}

/// Bounded evidence-only scan. A discovered offset is reported but never used
/// to gate a WDDM fence; correctness must use one explicit documented offset.
unsafe fn diagnostic_scan_present_private(base: *const u8, total: usize) {
    if crate::ddi::present_packet::PRESENT_MARKER_WRITES.load(Ordering::Relaxed) == 0
        || PRESENT_MARKER_SCAN_ATTEMPTS.fetch_add(1, Ordering::Relaxed) >= 256
    {
        return;
    }
    let size = total.min(u32::MAX as usize) as u32;
    if let Some(offset) =
        unsafe { PresentSubmissionPrivate::diagnostic_find_offset(base.cast(), size) }
    {
        PRESENT_MARKER_SCAN_HITS.fetch_add(1, Ordering::Relaxed);
        PRESENT_MARKER_LAST_OFFSET.store(offset, Ordering::Relaxed);
    }
}

/// Order one coalesced host refresh after every Venus command submitted before
/// the UMD's marker. The guard required by `note_scanout_refresh` statically
/// enforces the scheduler/transport lock order instead of relying on callers.
fn arm_scanout_refresh_after_current_venus(adapter: &AdapterContext) {
    let ready = adapter.with_wddm_notify_lock(|guard| {
        guard
            .with_virtio(|o, v| v.note_scanout_refresh(o))
            .unwrap_or(false)
    });
    if ready {
        adapter.request_scanout_refresh();
    }
}

/// `DxgkDdiSubmitCommandVirtual` — submit a DMA buffer addressed by GPU virtual
/// address. Because Helios declares the GpuMmu model (`VirtualAddressingSupported`
/// + `GpuMmuSupported`), VidSch routes a GpuMmu context's command buffers HERE, not
/// to `DxgkDdiSubmitCommand`. Leaving it `STATUS_NOT_SUPPORTED` was fine only while
/// no render work was ever submitted; once `DxgkDdiRenderGdi` produces a real render
/// DMA buffer, `dxgmms2!VidSchiSendToExecutionQueue` submits it here, gets
/// NOT_SUPPORTED (0xC00000BB), and bugchecks **0x119 (VIDEO_SCHEDULER_INTERNAL_ERROR)
/// Arg1=2** ("driver failed upon submission of a command") — observed live.
///
/// There is no guest GPU to program (the host owns the real MMU; venus addresses
/// by resource id — the actual work rides the venus Escape channel), but since
/// C3/M3.4 the fence is NOT lied about: it queues behind the venus work
/// outstanding at submit time and completes from the interrupt DPC. Runs at
/// DISPATCH_LEVEL.
pub unsafe extern "C" fn dxgkddi_submit_command_virtual(
    h_adapter: *mut c_void,
    submit_command: *const DXGKARG_SUBMITCOMMANDVIRTUAL,
) -> NTSTATUS {
    if h_adapter.is_null() || submit_command.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let submit = unsafe { &*submit_command };
    let fence = submit.SubmissionFenceId;

    SUBMIT_COUNT.fetch_add(1, Ordering::Relaxed);
    SUBMIT_LAST_FENCE.store(fence, Ordering::Relaxed);
    // SAFETY: `Value` is a plain UINT view of the (valid) flags union.
    let is_paging = (unsafe { submit.Flags.__bindgen_anon_1.Value } & 1) != 0;
    if is_paging {
        SUBMIT_PAGING_COUNT.fetch_add(1, Ordering::Relaxed);
    }

    let present_fence = unsafe { decode_virtual_present_fence(submit) };
    // The submission is accepted regardless of how the notification went; a
    // non-SUCCESS return here bugchecks dxgmms2 with 0x119 Arg1=2.
    let SubmitAck::Accepted = note_and_maybe_signal(adapter, fence, is_paging, present_fence);
    STATUS_SUCCESS
}

/// `DxgkDdiSubmitCommand` — submit a DMA buffer to the GPU. Critically, this is
/// also how Dxgkrnl queues *paging* buffers (built by DxgkDdiBuildPagingBuffer,
/// with `hDevice == NULL`); since we register paging, this slot must be present.
// Runs at DISPATCH_LEVEL. Same C3/M3.4 completion model as SubmitCommandVirtual.
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

    SUBMIT_COUNT.fetch_add(1, Ordering::Relaxed);
    SUBMIT_LAST_FENCE.store(fence, Ordering::Relaxed);
    // Flags.Paging is bit 0 of the flags word; read it via the union's `Value`
    // arm (the bitfield accessor lives behind the same union).
    // SAFETY: `Value` is a plain UINT view of the (valid) flags union.
    let is_paging = (unsafe { submit.Flags.__bindgen_anon_1.Value } & 1) != 0;
    if is_paging {
        SUBMIT_PAGING_COUNT.fetch_add(1, Ordering::Relaxed);
    }

    let present_fence = unsafe { decode_legacy_present_fence(submit) };
    // As above: accepted regardless of the notification outcome.
    let SubmitAck::Accepted = note_and_maybe_signal(adapter, fence, is_paging, present_fence);
    STATUS_SUCCESS
}

/// Cumulative count of pending WDDM fences discarded by a scheduler epoch —
/// engine reset, preemption, or TDR recovery.
///
/// Exactly the number a TDR post-mortem wants, and before R615 nothing recorded
/// it: all three sites discarded `preempt_flush`'s return with `let _`.
pub static ABANDONED_FENCES: AtomicU32 = AtomicU32::new(0);

/// What the caller owes VidSch after the pending fences are dropped.
///
/// The three TDR-adjacent DDIs perform the SAME "take the notify lock, drop
/// every pending WDDM fence" step and then do three different things
/// afterwards, with the shared step named nowhere. Making the difference an
/// exhaustive value forces any future TDR-adjacent DDI to declare which
/// notification it owes; today the choice is invisible.
pub(crate) enum AbandonOutcome<'a> {
    /// `DxgkDdiResetFromTimeout`: dxgkrnl owns the post-reset fence state and
    /// wants no packet.
    Silent,
    /// `DxgkDdiPreemptCommand`: acknowledge with a `DMA_PREEMPTED` packet.
    Preempted {
        dxgkrnl: &'a DXGKRNL_INTERFACE,
        fence: u32,
    },
    /// `DxgkDdiResetEngine`: report the completed watermark.
    ReportLastAborted { out: &'a mut UINT },
}

/// Drop every pending WDDM fence and settle what is owed to VidSch, in ONE
/// notification critical section.
///
/// The one-critical-section rule is the load-bearing part and it used to be
/// documented only inside `DxgkDdiPreemptCommand`, where a reader of
/// `DxgkDdiResetEngine` would never see it: preemption participates in the same
/// VidSch fence stream as DMA_COMPLETED, so if the FIFO is cleared and the
/// watermark sampled in one section but the packet is built in another, a
/// completion DPC can advance `last_completed_fence` in between and make the
/// preemption packet claim the preemption fence itself as already completed.
/// Dxgkrnl rejects that one-fence leap with bugcheck 0x119/1 (observed:
/// expected 0x17a, received 0x17b).
///
/// Returns the number of fences dropped and the status to report.
///
/// ⚠ The count goes to an ATOMIC ONLY, never to `record_named_bytes` as the
/// review proposed: all three callers run at DISPATCH_LEVEL, and a registry
/// write above PASSIVE is one of the project's never-violate rules. The
/// `AbnDrop` mirror is written from the PASSIVE telemetry flush in `adapter.rs`,
/// the same way `WtOut` and `WtTbl` are.
pub(crate) fn abandon_pending_submissions(
    adapter: &AdapterContext,
    outcome: AbandonOutcome<'_>,
) -> (u32, NTSTATUS) {
    adapter.with_wddm_notify_lock(|guard| {
        let dropped = guard.with_virtio(|o, v| v.preempt_flush(o)).unwrap_or(0);
        if dropped != 0 {
            ABANDONED_FENCES.fetch_add(dropped, Ordering::Relaxed);
        }
        let status = match outcome {
            AbandonOutcome::Silent => STATUS_SUCCESS,
            AbandonOutcome::Preempted { dxgkrnl, fence } => {
                // SAFETY: the WDDM notification lock serializes this packet with
                // every DMA_COMPLETED packet; the callback interface is live and
                // delivery is raised to DIRQL by notify_at_dirql.
                unsafe { signal_dma_preempted_locked(guard, dxgkrnl, fence) }
            }
            AbandonOutcome::ReportLastAborted { out } => {
                // Written INSIDE the guard, exactly as before. Do NOT change the
                // value: whether DXGKARG_RESETENGINE wants the completed
                // watermark or the first aborted fence is an OPEN QUESTION
                // against the WDK header, deliberately not resolved here.
                *out = guard.completed_fence() as UINT;
                STATUS_SUCCESS
            }
        };
        (dropped, status)
    })
}

/// `DxgkDdiPreemptCommand` — VidSch wants the node's pending submissions back
/// (TDR probe or priority scheduling). We cannot abort host venus work, but we
/// CAN release the pending WDDM fences: drop them (the scheduler resubmits the
/// incomplete DMA buffers later; the venus work keeps executing and the fresh
/// submissions re-queue behind whatever is still outstanding) and acknowledge
/// with `DMA_PREEMPTED`. Without this ack, a validate-slow venus fence
/// (> TdrDelay) escalates straight to ResetFromTimeout. Runs at DISPATCH_LEVEL.
pub unsafe extern "C" fn dxgkddi_preempt_command(
    h_adapter: *mut c_void,
    preempt_command: *const DXGKARG_PREEMPTCOMMAND,
) -> NTSTATUS {
    PREEMPT_COUNT.fetch_add(1, Ordering::Relaxed);
    if h_adapter.is_null() || preempt_command.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let preempt = unsafe { &*preempt_command };

    let dxgkrnl = match adapter.dxgkrnl() {
        Ok(interface) => interface,
        Err(_) => return STATUS_DEVICE_NOT_READY,
    };
    // The one-critical-section rationale now lives on
    // `abandon_pending_submissions`, where DxgkDdiResetEngine's reader can see
    // it too.
    abandon_pending_submissions(
        adapter,
        AbandonOutcome::Preempted {
            dxgkrnl,
            fence: preempt.PreemptionFenceId,
        },
    )
    .1
}

/// `DxgkDdiResetFromTimeout` — TDR recovery. There is no hardware engine state
/// to reset (the host owns the GPU); drop every pending WDDM fence so dxgkrnl's
/// post-reset accounting starts clean (it discards outstanding submissions).
pub unsafe extern "C" fn dxgkddi_reset_from_timeout(h_adapter: *mut c_void) -> NTSTATUS {
    if h_adapter.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    // Prevent a DPC from taking a fence out of the pending FIFO while reset is
    // discarding that same scheduler epoch.  Dxgkrnl owns the post-reset fence
    // state; no completion from the abandoned epoch may escape concurrently.
    let _ = abandon_pending_submissions(adapter, AbandonOutcome::Silent);
    // Consume transport_failed(), which had zero callers repo-wide: a TDR
    // against a latched ring is the loop this tranche exists to break, and
    // without this the only evidence was a DiagLevel-gated breadcrumb. Reported
    // here and mirrored on change only, so a TDR storm cannot become a registry
    // write storm.
    let failed = adapter
        .with_virtio(|v| v.transport_failed())
        .unwrap_or(false);
    if failed {
        let bad = crate::virtio::gpu::DRAIN_BAD_TOKEN.load(Ordering::Relaxed);
        if RING_FAIL_REPORTED.swap(bad, Ordering::Relaxed) != bad {
            crate::diag::fault(crate::diag::FaultCounter::StRing, bad);
        }
    }
    STATUS_SUCCESS
}

/// Last `DRAIN_BAD_TOKEN` value reported through `StRing`, so the ring-failure
/// report is written on change rather than on every TDR.
static RING_FAIL_REPORTED: AtomicU32 = AtomicU32::new(u32::MAX);

/// `DxgkDdiRestartFromTimeout` — resume after TDR.
pub unsafe extern "C" fn dxgkddi_restart_from_timeout(h_adapter: *mut c_void) -> NTSTATUS {
    if h_adapter.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    STATUS_SUCCESS
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
    h_context: IN_CONST_HANDLE,
    render: INOUT_PDXGKARG_RENDER,
) -> NTSTATUS {
    RENDER_COUNT.fetch_add(1, Ordering::Relaxed);
    if render.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let args = unsafe { &mut *render };
    if args.pDmaBuffer.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let cmd_len = args.CommandLength as usize;
    let dma_cap = args.DmaSize as usize;
    if cmd_len > 0 && args.pCommand.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    if cmd_len > dma_cap {
        // Buffer too small for the recorded command: ask the runtime to grow it.
        return STATUS_BUFFER_TOO_SMALL;
    }

    if cmd_len >= size_of::<helios_protocol::HeliosPresentRefreshCmd>() {
        let command = unsafe {
            core::ptr::read_unaligned(
                args.pCommand
                    .cast::<helios_protocol::HeliosPresentRefreshCmd>(),
            )
        };
        if command.is_valid() {
            // The allocation identity was fixed once by SetVidPnSourceAddress.
            // Ordinary presents only dirty that durable target; they must never
            // select a resource from stale bytes beyond the 16-byte command.
            //
            // The two-back-pointer chain used to be walked inline here with a
            // hand-written `!is_null()` pair; `ContextHandleRef` is the same
            // traversal, checked once, in the module that owns the fields.
            let context = unsafe { crate::device::ContextHandleRef::from_raw(h_context) };
            if let Some(adapter) = context.as_ref().and_then(|c| c.adapter()) {
                arm_scanout_refresh_after_current_venus(adapter);
            }
        }
    }

    if cmd_len >= size_of::<helios_protocol::HeliosPresentRenderCmd>() {
        let command = unsafe {
            core::ptr::read_unaligned(
                args.pCommand
                    .cast::<helios_protocol::HeliosPresentRenderCmd>(),
            )
        };
        if command.is_valid() {
            static PRESENT_RENDER_DIAG_COUNT: AtomicU32 = AtomicU32::new(0);
            let diag = PRESENT_RENDER_DIAG_COUNT.fetch_add(1, Ordering::Relaxed) < 4;
            if h_context.is_null() {
                if diag {
                    crate::diag::record_named_bytes(b"PRset", 0xE1);
                }
            } else {
                let context = unsafe { crate::device::ContextHandleRef::from_raw(h_context) };
                let adapter = context.as_ref().and_then(|c| c.adapter());
                if adapter.is_none() {
                    if diag {
                        crate::diag::record_named_bytes(b"PRset", 0xE2);
                    }
                } else {
                    let private = command.present;
                    if diag {
                        crate::diag::record_named_bytes(b"PRsrc", private.resource_id);
                        crate::diag::record_named_bytes(b"PRset", 2);
                    }
                    // DxgkDdiPresent selects the exact source from
                    // dxgkrnl's allocation list. This command is only the
                    // completion/dirty edge for the Venus work submitted
                    // before pfnPresentCb; its private bytes are not a second
                    // scanout selector.
                    if let Some(adapter) = adapter {
                        arm_scanout_refresh_after_current_venus(adapter);
                    }
                }
            }
        }
    }

    if args.PatchLocationListInSize > args.PatchLocationListOutSize {
        return STATUS_BUFFER_TOO_SMALL;
    }
    if args.PatchLocationListInSize != 0
        && (args.pPatchLocationListIn.is_null() || args.pPatchLocationListOut.is_null())
    {
        return STATUS_INVALID_PARAMETER;
    }

    for i in 0..args.PatchLocationListInSize {
        let input = unsafe { &*args.pPatchLocationListIn.add(i as usize) };
        let output = unsafe { &mut *args.pPatchLocationListOut.add(i as usize) };
        unsafe { core::ptr::write_bytes(output as *mut _, 0, 1) };
        output.AllocationIndex = input.AllocationIndex;
        output.AllocationOffset = 0;
        output.PatchOffset = 0;
        output.SplitOffset = 0;
        unsafe {
            output.__bindgen_anon_1.Value = i & 0x00ff_ffff;
        }
    }
    args.PatchLocationListOutSize = args.PatchLocationListInSize;
    if !args.pPatchLocationListOut.is_null() {
        args.pPatchLocationListOut = unsafe {
            args.pPatchLocationListOut
                .add(args.PatchLocationListInSize as usize)
        };
    }

    if cmd_len > 0 {
        // SAFETY: the runtime guarantees `pCommand` has `CommandLength` readable
        // bytes and `pDmaBuffer` has `DmaSize` writable bytes; we copy at most
        // `cmd_len` (<= DmaSize) and the ranges do not overlap.
        unsafe {
            core::ptr::copy_nonoverlapping(
                args.pCommand as *const u8,
                args.pDmaBuffer as *mut u8,
                cmd_len,
            );
        }
    }
    args.pDmaBuffer = unsafe { (args.pDmaBuffer as *mut u8).add(cmd_len) as *mut c_void };
    args.MultipassOffset = 0;
    STATUS_SUCCESS
}

/// `DxgkDdiRenderKm` — kernel-mode (GDI hardware-acceleration) render path.
///
/// dxgkrnl drives this when GDI renders to a cross-adapter / GDI-accelerated surface
/// — gated by `DXGK_PRESENTATIONCAPS::SupportKernelModeCommandBuffer` (which we
/// advertise, mandatory for Code-0 load) together with `CrossAdapterResource`
/// (`gdi-hardware-acceleration.md`: GDI-HW-accel KMDs MUST implement
/// CreateAllocation + GetStandardAllocationDriverData + RenderKm). The OS passes an
/// array of `DXGK_RENDERKM_COMMAND` ops in `pCommand`; the driver must translate
/// them into a DMA buffer + patch-location list, **advance the in/out pointers**,
/// and return SUCCESS. Returning `STATUS_NOT_IMPLEMENTED` leaves `pDmaBuffer`
/// unadvanced and the submission output unfilled, after which
/// `dxgkrnl!ADAPTER_RENDER::DdiRenderGdi` calls a null function pointer
/// (observed live: `DdiRenderGdi+0x140` `call rax`, rax=0 → 0xC0000005).
///
/// Decorative-GpuMmu model: the host GPU (venus) owns real rendering by resource id,
/// so we do not lower GDI ops to GPU instructions here. We record the opaque command
/// bytes into the DMA buffer (so `DxgkDdiSubmitCommand` has a non-empty buffer to
/// retire) and advance the DMA write pointer; there are no guest GPU-VAs to patch
/// (matching `DxgkDdiPatch`'s no-op), so the out patch list stays at its base (0
/// entries). `SubmitCommand` drives the fence. NOTE: this makes the path structurally
/// complete (no crash); pixel-correct GDI lowering is a later step — DWM's own
/// composition is D3D (the UMD `DxgkDdiRender` path), not this GDI path.
pub unsafe extern "C" fn dxgkddi_render_km(
    _h_context: IN_CONST_HANDLE,
    render: INOUT_PDXGKARG_RENDER,
) -> NTSTATUS {
    RENDER_COUNT.fetch_add(1, Ordering::Relaxed);
    if render.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let args = unsafe { &mut *render };
    if args.pDmaBuffer.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let cmd_len = args.CommandLength as usize;
    let dma_cap = args.DmaSize as usize;
    // Ask the runtime to grow the DMA buffer if the command does not fit, rather
    // than truncating (mirrors `dxgkddi_render`).
    if cmd_len > dma_cap {
        return STATUS_BUFFER_TOO_SMALL;
    }
    if cmd_len > 0 && !args.pCommand.is_null() {
        // SAFETY: runtime guarantees `CommandLength` readable bytes at `pCommand`
        // and `DmaSize` (>= cmd_len) writable bytes at `pDmaBuffer`; distinct buffers.
        unsafe {
            core::ptr::copy_nonoverlapping(
                args.pCommand as *const u8,
                args.pDmaBuffer as *mut u8,
                cmd_len,
            );
        }
    }
    // Advance the DMA write pointer past the recorded bytes so the runtime sees a
    // non-empty buffer to submit. No GPU-VA patches → leave pPatchLocationListOut at
    // its base. Single pass → MultipassOffset 0.
    // SAFETY: advancing within the `DmaSize`-byte buffer (cmd_len <= dma_cap).
    args.pDmaBuffer = unsafe { (args.pDmaBuffer as *mut u8).add(cmd_len) as *mut c_void };
    args.MultipassOffset = 0;
    STATUS_SUCCESS
}

/// `DxgkDdiRenderGdi` — GDI hardware-acceleration render path
/// (`PDXGKDDI_RENDERGDI`, args `DXGKARG_RENDERGDI`). This is a SEPARATE DDI from
/// `DxgkDdiRender` and `DxgkDdiRenderKm` — and the one dxgkrnl's
/// `ADAPTER_RENDER::DdiRenderGdi` actually invokes (through a CFG-guarded indirect
/// call). Leaving the `DxgkDdiRenderGdi` field null (we previously registered only
/// Render + RenderKm) made that call land on a null pointer and bugcheck
/// (kernel `0xC0000005`, `DdiRenderGdi+0x140`, observed live), which is why this
/// entry point stays registered and answers SUCCESS even though the driver no
/// longer opts into GDI hardware acceleration at all: as of 22.22.180.0
/// `DXGK_PRESENTATIONCAPS::SupportKernelModeCommandBuffer` is hard-coded 0
/// (`query_adapter_info`), so dxgkrnl routes GDI through win32k's CPU redirection
/// path and never drives this DDI. The KMD CPU raster executor that used to run
/// here (`gdi_blit.rs`) was deleted with it.
///
/// Body (identical in shape to `dxgkddi_render_km`, which is why T7 dedups them):
/// record the opaque command bytes into the DMA buffer so `DxgkDdiSubmitCommand`
/// has a non-empty buffer to retire, advance the DMA write pointer, single pass,
/// no GPU-VA patches → SUCCESS.
pub unsafe extern "C" fn dxgkddi_render_gdi(
    _h_context: IN_CONST_HANDLE,
    render_gdi: INOUT_PDXGKARG_RENDERGDI,
) -> NTSTATUS {
    RENDER_COUNT.fetch_add(1, Ordering::Relaxed);
    if render_gdi.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let args = unsafe { &mut *render_gdi };
    if args.pDmaBuffer.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let cmd_len = args.CommandLength as usize;
    let dma_cap = args.DmaSize as usize;
    // Ask the runtime to grow the DMA buffer rather than truncating the stream.
    if cmd_len > dma_cap {
        return STATUS_BUFFER_TOO_SMALL;
    }
    if cmd_len > 0 && !args.pCommand.is_null() {
        // SAFETY: runtime guarantees CommandLength readable bytes at pCommand and
        // DmaSize (>= cmd_len) writable bytes at pDmaBuffer; distinct buffers.
        unsafe {
            core::ptr::copy_nonoverlapping(
                args.pCommand as *const u8,
                args.pDmaBuffer as *mut u8,
                cmd_len,
            );
        }
    }
    // SAFETY: advancing within the DmaSize-byte buffer (cmd_len <= dma_cap).
    args.pDmaBuffer = unsafe { (args.pDmaBuffer as *mut u8).add(cmd_len) as *mut c_void };
    args.MultipassOffset = 0;
    STATUS_SUCCESS
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
    query.CurrentFence = adapter.completed_fence();
    query.NodeOrdinal = 0;
    query.EngineOrdinal = 0;
    STATUS_SUCCESS
}

/// `DxgkDdiCollectDbgInfo` — dump driver debug state on a TDR/bugcheck.
///
/// Contract notes (this fires DURING TDR dump collection, possibly at
/// HIGH_LEVEL during a bugcheck, so returning STATUS_NOT_IMPLEMENTED here
/// marked the driver as misbehaving in the 2026-07-02 ETW capture):
/// - May be called at any IRQL; must not block, allocate, take locks, or touch
///   pageable code/data. Only the DISPATCH-safe atomics are read.
/// - The OS-provided buffer must be written in full (unused tail zeroed).
pub unsafe extern "C" fn dxgkddi_collect_dbg_info(
    h_adapter: IN_CONST_HANDLE,
    collect_dbg_info: IN_CONST_PDXGKARG_COLLECTDBGINFO,
) -> NTSTATUS {
    if collect_dbg_info.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl guarantees the argument struct is valid for the call.
    let args = unsafe { &*collect_dbg_info };
    if args.pBuffer.is_null() || args.BufferSize == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let buf_len = args.BufferSize as usize;

    // SAFETY: dxgkrnl guarantees BufferSize writable non-paged bytes at
    // pBuffer for the duration of the call. Zero the whole buffer first so
    // the report is fully written regardless of how much we fill.
    unsafe {
        core::ptr::write_bytes(args.pBuffer as *mut u8, 0, buf_len);
    }

    // Fixed-shape DWORD report: magic + version + reason + engine counters.
    // Decoded offline from the TDR minidump's driver-private section.
    let last_fence = if h_adapter.is_null() {
        0
    } else {
        // SAFETY: dxgkrnl passes the adapter context handle it got from
        // DxgkDdiAddDevice; valid for the adapter's lifetime. Atomic load only.
        let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
        adapter.completed_fence()
    };
    let report: [u32; 38] = [
        0x4844_4247, // 'HDBG'
        6,           // report version (6: + FENCE_WAIT_TABLE_FULL at index 37)
        args.Reason,
        SUBMIT_COUNT.load(Ordering::Relaxed),
        SUBMIT_LAST_FENCE.load(Ordering::Relaxed),
        SUBMIT_PAGING_COUNT.load(Ordering::Relaxed),
        RENDER_COUNT.load(Ordering::Relaxed),
        PATCH_COUNT.load(Ordering::Relaxed),
        PREEMPT_COUNT.load(Ordering::Relaxed),
        DMA_NOTIFY_COUNT.load(Ordering::Relaxed),
        DMA_QUEUE_DPC_COUNT.load(Ordering::Relaxed),
        last_fence,
        // Nonzero = synchronous control commands timed out their PASSIVE wait
        // budget (host stopped answering in time) — no longer a transport
        // poison, but still the likely reason for the TDR this dump belongs to.
        crate::virtio::gpu::CTRL_TIMEOUT_COUNT.load(Ordering::Relaxed),
        // v2: bounded-table telemetry (the 2026-07-03 MAX_BLOBS exhaustion class).
        crate::virtio::gpu::BLOB_HIGH_WATER.load(Ordering::Relaxed),
        crate::virtio::gpu::BLOB_FULL_REJECTS.load(Ordering::Relaxed),
        crate::virtio::gpu::RESOURCE_HIGH_WATER.load(Ordering::Relaxed),
        crate::virtio::gpu::RESOURCE_FULL_REJECTS.load(Ordering::Relaxed),
        crate::virtio::gpu::CONTEXT_FULL_DROPS.load(Ordering::Relaxed),
        crate::virtio::gpu::WINDOW_RANGE_DROPS.load(Ordering::Relaxed),
        crate::virtio::gpu::TAKE_LIVE_MISSES.load(Ordering::Relaxed),
        crate::virtio::gpu::ADOPT_DEAD_REJECTS.load(Ordering::Relaxed),
        // v3: C3/M3.4 async-transport telemetry.
        crate::virtio::gpu::ASYNC_SUBMIT_COUNT.load(Ordering::Relaxed),
        crate::virtio::gpu::ASYNC_COMPLETE_COUNT.load(Ordering::Relaxed),
        crate::virtio::gpu::ASYNC_RESP_ERRORS.load(Ordering::Relaxed),
        crate::virtio::gpu::FENCE_WAIT_REGISTERED.load(Ordering::Relaxed),
        crate::virtio::gpu::FENCE_WAIT_TIMEOUTS.load(Ordering::Relaxed),
        crate::virtio::gpu::DRAIN_BAD_TOKEN.load(Ordering::Relaxed),
        crate::virtio::gpu::QUEUE_FULL_RETRIES.load(Ordering::Relaxed),
        crate::virtio::gpu::WDDM_PENDING_OVERFLOWS.load(Ordering::Relaxed),
        crate::virtio::gpu::INFLIGHT_HIGH_WATER.load(Ordering::Relaxed),
        crate::virtio::gpu::PARKED_HIGH_WATER.load(Ordering::Relaxed),
        crate::virtio::gpu::PARKED_LEAKS.load(Ordering::Relaxed),
        crate::virtio::gpu::WDDM_FENCE_FROM_DPC.load(Ordering::Relaxed),
        // v4: ring_idx >= 1 GPU-completion fences (WS1 #4).
        crate::virtio::gpu::RING_SUBMIT_COUNT.load(Ordering::Relaxed),
        crate::virtio::gpu::RING_COMPLETE_COUNT.load(Ordering::Relaxed),
        // v5 (R315): the CONTROL-path response errors — a host-rejected
        // SET_SCANOUT_BLOB or RESOURCE_FLUSH, i.e. the loud-failure counter for
        // the direct-primary display path. It appeared in no report at all,
        // while its submit-path sibling (ASYNC_RESP_ERRORS) was already here.
        crate::virtio::gpu::ASYNC_CTRL_RESP_ERRORS.load(Ordering::Relaxed),
        // DDI-level CpuHostAperture unmaps, for map/unmap pairing.
        crate::ddi::cpu_host_aperture::CPU_HOST_UNMAP_COUNT.load(Ordering::Relaxed),
        // v6 (R604): split out of FENCE_WAIT_TIMEOUTS (word 25), which now means
        // only "the host did not complete this fence". This word means "all 64
        // waiter slots were taken", a guest table-size condition. The report is
        // decoded offline BY INDEX, so word 25 keeps its meaning and the new
        // word is appended — never renumbered — and the version word above moves
        // with the array in the same commit.
        crate::virtio::gpu::FENCE_WAIT_TABLE_FULL.load(Ordering::Relaxed),
    ];
    let report_bytes = size_of::<[u32; 38]>();
    let copy_len = core::cmp::min(report_bytes, buf_len);
    // SAFETY: copy_len <= BufferSize (writable, checked above) and
    // copy_len <= size_of report (readable local array).
    unsafe {
        core::ptr::copy_nonoverlapping(
            report.as_ptr() as *const u8,
            args.pBuffer as *mut u8,
            copy_len,
        );
    }
    STATUS_SUCCESS
}
