//! Command-submission and TDR DDIs.
//!
//! Render work is still disabled, but the scheduler-facing submission path must
//! be able to retire early paging/null-engine DMA buffers without timing out.

use core::ffi::c_void;
use core::mem::size_of;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::adapter::{AdapterContext, WddmNotifyGuard};
use crate::ddi::present_packet::{PresentSubmissionBoundary, PresentSubmissionPrivate};
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

// ── D3D12 ECL submission record (`HeliosD3D12SubmitCmd`) ─────────────────────
// All three are bumped inside `dxgkddi_render`, which the WDK header declares
// `_IRQL_requires_(PASSIVE_LEVEL)`; they are atomics anyway because the registry
// mirror and any future DISPATCH reader must see them without a lock.

/// Valid D3D12 ECL records seen by `dxgkddi_render`.
///
/// The first nonzero value is the proof that `pfnRenderCb` from
/// `pfnExecuteCommandLists` reaches this driver at all — the thing rung 0 of
/// `D12-G8` could not establish, because `EclNoWddmSubmission = 1` meant no
/// packet was ever built.
pub static D3D12_SUBMIT_RECORDS: AtomicU32 = AtomicU32::new(0);
/// Records carrying `gpu_wire_fence == 0` — the documented plumbing arm
/// ("submit the packet, order it against nothing") and the A/B disable for the
/// boundary itself.
///
/// ⚠ GRADED AS EXPECTED-NONZERO DURING BRING-UP. `D12Zero == D12Rec` means the
/// UMD is submitting packets and naming no boundary yet, which is the intended
/// first step, not a defect. It becomes a finding only once the UMD is supposed
/// to be sampling a real ring-1 fence: `D12Zero` climbing then means the ICD
/// handed it nothing (see `EscSubRing` — a guest that never submits on a
/// GPU-completion ring has no such fence to name).
pub static D3D12_SUBMIT_ZERO_FENCE: AtomicU32 = AtomicU32::new(0);
/// Records whose boundary could not be written into this Render's private data
/// (`merge_fence` refused: null pointer, or a private-data buffer shorter than
/// the 32-byte record).
///
/// Must stay 0: `DxgkDdiCreateContext` reports
/// `PRESENT_DMA_PRIVATE_DATA_BYTES = 88` for EVERY context (`device.rs:425`), so
/// a nonzero value means a context this driver did not size, i.e. the packet was
/// submitted with no boundary and the D3D12 fence is reporting decode.
pub static D3D12_SUBMIT_MERGE_FAILS: AtomicU32 = AtomicU32::new(0);
/// D3D12 records that found ANOTHER D3D12 record already in the same private-data
/// buffer, and merged their fences (largest wins).
///
/// This is the counter for the recycled-buffer hazard, and it is the reason the
/// hazard is closed rather than merely counted: `mark_d3d12` writes on every
/// record and `decode` consumes the D3D12 arm, so a predecessor can now only be
/// an ECL batched into the SAME DMA buffer, or a buffer whose earlier submission
/// never reached SubmitCommand (preempted / TDR-abandoned). Both merge
/// conservatively — the larger wire fence names work that really was enqueued.
///
/// ⚠ It is therefore NOT a failure counter, and it is not expected to be 0. What
/// would have been the defect is `D12Rec` moving while a stale boundary survives
/// into a later submission — which is now unrepresentable, and this counter is
/// how you would see the batching that used to make it look benign.
pub static D3D12_SUBMIT_MERGED: AtomicU32 = AtomicU32::new(0);
/// `'HD12'` records cleared by a `DxgkDdiRender` that did **not** write one —
/// the present-identity Render, or any other Render on a D3D12 WDDM context.
///
/// ⛔ **This closes the half of the recycled-buffer hazard that `D12Merged`'s doc
/// above assumed away.** That doc argues a predecessor "can now only be an ECL
/// batched into the SAME DMA buffer, or a buffer whose earlier submission never
/// reached SubmitCommand" — true, and both merge conservatively *for another ECL*.
/// It does not cover a **non-ECL** Render inheriting the record, which UP-9 made
/// possible by adding a second `pfnRenderCb` on the same context that writes
/// nothing at offset 0. `invalidate_d3d12` removes that inheritance.
///
/// ⚠ **NOT a failure counter, and not expected to be 0.** Nonzero means either
/// the hazard was live (a stale record really was inherited before this landed),
/// or — the benign and probably dominant cause — dxgkrnl batched an ECL and a
/// present identity into one DMA buffer, in which case the cleared boundary was
/// correct for that packet and the packet falls back to the conservative
/// `next_wire_fence` prefix. ⛔ The two are not separable by this counter alone;
/// `D12Clr` climbing in lockstep with present frequency is the batching, while
/// `D12Clr` moving on a run with no D3D12 presents at all is the hazard.
///
/// ⚠ It is the accounting term the `D12Exact` identity in this file's mirror
/// comment does not yet include — see that comment.
pub static D3D12_STALE_RECORD_CLEARED: AtomicU32 = AtomicU32::new(0);

/// Mirror the scheduler private-data handoff evidence at PASSIVE_LEVEL.
pub(crate) fn record_present_handoff_telemetry() {
    use crate::ddi::present_packet::{
        PRESENT_MARKER_LAST_FENCE, PRESENT_MARKER_LAST_SIZE, PRESENT_MARKER_WRITES,
    };

    // THE FOUR SILENT FAILURE COUNTERS (2026-08-05). Each of these was
    // incremented on a real refusal/failure path and then loaded by nobody:
    // no `.load`, no `CounterEntry`, no `HeliosEscapeQueryStats*` field. Their
    // own doc comments claim they exist so the failure "shows up as itself",
    // and CLAUDE.md's rule is that every skipped or refused path gets a named
    // registry counter — so a write-only counter is the rule being violated
    // silently. They are mirrored here rather than added to the escape stats
    // ABI because this site already runs at PASSIVE on the same teardown edge
    // and needs no version bump. **All four must read 0 on a healthy session.**
    crate::diag::record_named_bytes(
        b"WdSigF",
        crate::virtio::gpu::WDDM_SIGNAL_AFTER_FAILURE.load(Ordering::Relaxed),
    );
    crate::diag::record_named_bytes(b"DmaNtfF", DMA_NOTIFY_FAILS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(
        b"TxGone",
        crate::virtio::gpu::TRANSPORT_GONE_AT_WAIT.load(Ordering::Relaxed),
    );
    crate::diag::record_named_bytes(
        b"RclBadH",
        crate::ddi::create_allocation::RECLAIM_BAD_HANDLE.load(Ordering::Relaxed),
    );

    crate::diag::record_named_bytes(b"PmWr", PRESENT_MARKER_WRITES.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmWFn", PRESENT_MARKER_LAST_FENCE.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmWSz", PRESENT_MARKER_LAST_SIZE.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PmHit", PRESENT_MARKER_HITS.load(Ordering::Relaxed));
    // How many WDDM submissions took the exact-boundary watermark (`PresentWmk`).
    // Zero with the knob off is the correct reading; a knob that reads as its
    // default must be distinguishable from a knob that had no effect.
    crate::diag::record_named_bytes(
        b"PwExact",
        crate::virtio::gpu::PRESENT_EXACT_WATERMARK_USED.load(Ordering::Relaxed),
    );
    // A4: D3D12 ECL packets gated on the EXACT wire fence their batch ends at,
    // instead of on the whole prefix below it (the invariant CLAUDE.md's table
    // states verbatim). Expected to equal the number of D3D12 records that named a
    // usable boundary — `D12Rec - D12Zero - D12MrgF - GpuFncClamp - GpuFncGen`. A
    // shortfall means a D3D12 packet took a prefix arm after all.
    crate::diag::record_named_bytes(
        b"D12Exact",
        crate::virtio::gpu::D3D12_EXACT_WATERMARK_USED.load(Ordering::Relaxed),
    );
    // K-F2 (2026-08-06): how far ahead of its own producer a present marker
    // names. Mirrored HERE, beside the lever it protects, because the counting
    // site runs at DISPATCH under `virtio_lock` and a registry write above
    // PASSIVE is a never-violate rule — and here rather than in the escape
    // stats ABI for the reason stated at the top of this function.
    //
    // ⚠ GRADED THE OTHER WAY ROUND from everything above: a nonzero `PsMkAhd`
    // is the EXPECTED steady state and `PsMkAhdHi` should sit near 1, because
    // the UMD hands the marker over before the frame's `vkQueueSubmit` on
    // purpose. `PsMkAhdHi == 0` across a desktop + Fire Strike run is the
    // reading that would refute that and revive the refused K-F2 guard;
    // `PsMkAhdHi` far above DXVK's `MaxNumQueuedCommandBuffers = 32` is a
    // forged marker rather than a frame. Full grading: `virtio/counters.rs`.
    crate::diag::record_named_bytes(
        b"PsMkAhd",
        crate::virtio::gpu::PRESENT_STREAM_MARKER_AHEAD.load(Ordering::Relaxed),
    );
    crate::diag::record_named_bytes(
        b"PsMkAhdHi",
        crate::virtio::gpu::PRESENT_STREAM_MARKER_AHEAD_HIGH_WATER.load(Ordering::Relaxed),
    );
    // The D3D12 ECL arm (KMD_IMPACT §14a.2 K-F4). `D12Rec` is the first proof
    // that `pfnRenderCb` from `pfnExecuteCommandLists` reaches this driver;
    // `D12Zero` is EXPECTED to equal it during bring-up (the documented
    // order-against-nothing arm); `D12MrgF` and `GpuFncClamp` must both stay 0 —
    // the first means a context this driver did not size, the second means the
    // UMD named a fence id this KMD never issued.
    crate::diag::record_named_bytes(b"D12Rec", D3D12_SUBMIT_RECORDS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"D12Zero", D3D12_SUBMIT_ZERO_FENCE.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"D12MrgF", D3D12_SUBMIT_MERGE_FAILS.load(Ordering::Relaxed));
    // NOT a failure counter: records that found a predecessor in the same
    // private-data buffer. Nonzero = several ECLs batched into one DMA buffer, or
    // a buffer whose earlier submission never reached SubmitCommand.
    crate::diag::record_named_bytes(b"D12Merged", D3D12_SUBMIT_MERGED.load(Ordering::Relaxed));
    // Also NOT a failure counter: a stale `'HD12'` record cleared by a Render that
    // did not write one. Its doc carries how to tell the benign cause (dxgkrnl
    // batched an ECL and a present identity together) from the hazard.
    crate::diag::record_named_bytes(
        b"D12Clr",
        D3D12_STALE_RECORD_CLEARED.load(Ordering::Relaxed),
    );
    crate::diag::record_named_bytes(
        b"GpuFncClamp",
        crate::virtio::gpu::GPU_FENCE_CLAMPED.load(Ordering::Relaxed),
    );
    // A6's half of the same rejection: a boundary naming a fence from a FOREIGN
    // transport generation. `GpuFncClamp` provably cannot flag it — its condition
    // is one-sided (`>= next_wire_fence`) and a pre-restart id is far BELOW the
    // live range. Must read 0 on any session without a device restart; a small
    // number after one is the surviving-ICD case; nonzero without a restart means a
    // UMD is sampling fence ids that did not come from this transport.
    crate::diag::record_named_bytes(
        b"GpuFncGen",
        crate::virtio::gpu::GPU_FENCE_FOREIGN_GENERATION.load(Ordering::Relaxed),
    );
    // The same defect on the two usermode-facing predicates: a WAIT_FENCE or
    // REGISTER_FENCE_EVENT naming a fence from a dead transport generation used to
    // be answered Complete. Same grading (0 without a device restart); NOT expected
    // to track `GpuFncGen`, whose callers are different.
    crate::diag::record_named_bytes(
        b"FncIdGen",
        crate::virtio::gpu::FENCE_ID_FOREIGN_GENERATION.load(Ordering::Relaxed),
    );
    // WHY the WDDM FIFO head was not ready. The head paces every fence behind
    // it, so this ratio names what actually paces present retirement.
    crate::diag::record_named_bytes(
        b"WfBWire",
        crate::virtio::gpu::WDDM_HEAD_BLOCKED_WIRE.load(Ordering::Relaxed),
    );
    crate::diag::record_named_bytes(
        b"WfBStrm",
        crate::virtio::gpu::WDDM_HEAD_BLOCKED_STREAM.load(Ordering::Relaxed),
    );
    crate::diag::record_named_bytes(
        b"WfBBlt",
        crate::virtio::gpu::WDDM_HEAD_BLOCKED_BLT.load(Ordering::Relaxed),
    );
    // The fourth arm is not a dependency: it is `WddmHoldMs` delaying an
    // otherwise-ready D3D12 ECL packet on purpose (UV1). Must be 0 unless
    // somebody is running that experiment.
    crate::diag::record_named_bytes(
        b"WfBHold",
        crate::virtio::gpu::WDDM_HEAD_BLOCKED_HOLD.load(Ordering::Relaxed),
    );
    // A5 / K-F2's price tag: heads whose tagged-namespace dependency was rebased
    // onto the conservative wire watermark after `WddmHeadMs`. Each one is a DMA
    // fence released while its named producer may not have completed, taken in
    // preference to the 256-entry overflow (the same lie times 256 plus a lease
    // teardown) or an adapter-wide TDR. MUST read 0 on a healthy session; if it
    // moves, whichever of `WfBStrm`/`WfBBlt` moved with it is the diagnosis.
    crate::diag::record_named_bytes(
        b"WfBReb",
        crate::virtio::gpu::WDDM_HEAD_REBASED.load(Ordering::Relaxed),
    );
    // UV3's instrument (KMD_IMPACT §14a.1). Until now the ring pair was read in
    // exactly ONE place — words 33/34 of the `'HDBG'` report below, which
    // dxgkrnl collects only on a TDR — so "read the ring counters before and
    // after a run" was not executable, and the question they answer is whether
    // anything on this driver retires at host GPU completion rather than decode.
    //
    // `RngSub`/`RngCmp` are adapter-wide and include this driver's own scanout,
    // windowed-BLT and present-BLT copies (all ring 1); `EscSub`/`EscSubRing`
    // are the guest-attributed half, counted where the ICD's value enters. The
    // informative reading is a zero — `EscSub > 0` with `EscSubRing == 0` means
    // the guest never submits on a GPU-completion ring, `EscSub == 0` means this
    // driver saw nothing from that process at all. Both are spelled out beside
    // the statics in `virtio/counters.rs`.
    crate::diag::record_named_bytes(
        b"RngSub",
        crate::virtio::gpu::RING_SUBMIT_COUNT.load(Ordering::Relaxed),
    );
    crate::diag::record_named_bytes(
        b"RngCmp",
        crate::virtio::gpu::RING_COMPLETE_COUNT.load(Ordering::Relaxed),
    );
    crate::diag::record_named_bytes(
        b"EscSub",
        crate::virtio::gpu::ESCAPE_SUBMIT_COUNT.load(Ordering::Relaxed),
    );
    crate::diag::record_named_bytes(
        b"EscSubRing",
        crate::virtio::gpu::ESCAPE_SUBMIT_RING_COUNT.load(Ordering::Relaxed),
    );
    // S-1's instrument (`docs/dx12/PENDING.md` §2). `DxgkDdiCalibrateGpuClock` is
    // the ONLY channel for the GPU timestamp frequency an application divides its
    // timestamp deltas by, and it used to zero-fill and return SUCCESS silently.
    // Mirrored HERE because the DDI is `_IRQL_requires_max_(DISPATCH_LEVEL)` and
    // *"called on timer"* per the WDK header, so it may not write the registry.
    //
    // GRADING: `ClkCal` must MOVE this boot — a zero means the DDI is never
    // entered and the frequency below reaches nobody. `ClkNoGpu` is expected to
    // EQUAL `ClkCal` while no host GPU-clock source exists; a gap means somebody
    // landed one and did not re-grade. `ClkFreq` is the answer itself, in Hz, and
    // is expected to read exactly 1000000000 — a different value means the
    // substrate constant was changed without changing its derivation.
    let (clk_calls, clk_no_gpu, clk_freq_hz) = super::scheduler::gpu_clock_counters();
    crate::diag::record_named_bytes(b"ClkCal", clk_calls);
    crate::diag::record_named_bytes(b"ClkNoGpu", clk_no_gpu);
    crate::diag::record_named_bytes(b"ClkFreq", clk_freq_hz);
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
    // within the next half of the sequence space; equal/backward is stale. The
    // predicate lives in `helios_kmd_logic` so the wrap arithmetic has a host
    // test instead of only this comment.
    let forward = helios_kmd_logic::scanout_lease::fence_is_forward(last, fence);
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
    present_submission: Option<PresentSubmissionBoundary>,
) -> SubmitAck {
    let Ok(dxgkrnl) = adapter.dxgkrnl() else {
        // Effectively unreachable: dxgkrnl is set at StartDevice and never
        // cleared, and SubmitCommand cannot precede it. The submission stays in
        // the FIFO for the DPC either way.
        DMA_NOTIFY_FAILS.fetch_add(1, Ordering::Relaxed);
        return SubmitAck::Accepted;
    };
    let overflows_before = crate::virtio::VirtioGpu::wddm_pending_overflows();
    adapter.with_wddm_notify_lock(|guard| {
        let signal_now = guard
            .with_virtio(|o, v| {
                let gpu_completion_fence = present_submission.and_then(|present| {
                    (present.gpu_fence_id != 0).then_some(present.gpu_fence_id)
                });
                let stream_boundary = present_submission.and_then(|present| {
                    (present.stream_boundary != 0).then_some(present.stream_boundary)
                });
                let blt_token = present_submission
                    .and_then(|present| (present.blt_token != 0).then_some(present.blt_token));
                // ⛔ CORRECTED 2026-08-07. This said "the ONLY thing it may decide
                // is whether the `WddmHoldMs` experiment is allowed to delay this
                // packet". That was true when written (`56483b2`, where the hold
                // was the bit's only consumer) and FALSE four commits later:
                // `1e6b34a` added a second consumer, and it is the load-bearing
                // one. The bit now has exactly two:
                //
                //   1. `wddm_boundary::select` reads it to choose `Kind::Exact`
                //      over `Kind::Prefix` — i.e. it DOES decide what to wait for.
                //      That is the A4 repair and it is deliberate: `Exact` names
                //      the frame's OWN fence, which is what CLAUDE.md's invariant
                //      requires, instead of the whole `next_wire_fence` prefix.
                //      ⚠ "Identity, not a boundary" survives only in the narrow
                //      sense that the bit is not itself a fence VALUE.
                //   2. `wddm_hold_deadline`, the `WddmHoldMs` experiment.
                //
                // The FIFO is adapter-global and head-of-line, so an unscoped hold
                // would stall DWM — which is why (2) is scoped by this bit.
                let d3d12 = present_submission.is_some_and(|present| present.d3d12);
                v.note_wddm_submission(
                    o,
                    fence,
                    is_paging,
                    gpu_completion_fence,
                    stream_boundary,
                    blt_token,
                    d3d12,
                )
            })
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
    if present_submission.is_some_and(|present| present.blt_token != 0) {
        // SubmitCommand is the residency-admission edge. Publish the worker
        // cause before its wake: the exact producer may have terminalized
        // during preemption, so its original wake can already be gone.
        adapter.scanout_retire_wanted.store(1, Ordering::Release);
        adapter.signal_hpd();
    }
    // The pending FIFO overflowed and degraded to the immediate model, dropping
    // every queued entry. Those entries were the only waiters on their scan-out
    // leases, so release them now — outside the notify lock, because the lease
    // state lives on the adapter and its release wakes the display worker.
    // Loud (`LsTear` moves) and exact; not a timeout.
    if crate::virtio::VirtioGpu::wddm_pending_overflows() != overflows_before {
        adapter.release_all_scanout_leases(crate::ddi::scanout_trace::LeaseEnd::Teardown);
    }
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
/// `base` must be readable and writable for `total` bytes for the duration of
/// the call.
///
/// THE PRESENT RECORD IS NOT MUTATED, and that rule stands: replay is resolved by
/// exact pending/terminal membership in `VirtioGpu`, because preemption may
/// resubmit that same documented record after the host copy has already
/// terminalized.
///
/// ⚠ THE D3D12 RECORD IS THE EXCEPTION — `PresentSubmissionPrivate::decode`
/// consumes that one arm, which is why `base` is now `*mut`. The reason is the
/// hazard in the other direction: dxgkrnl recycles these buffers, and a D3D12
/// boundary left behind is read again by the next submission that reuses the
/// buffer, gating it on an already-retired fence. The replay cost is acceptable
/// where the Present path's would not have been: a replayed D3D12 packet finds no
/// record and falls back to `next_wire_fence`, i.e. every transport entry
/// enqueued before it — conservative, always eventually satisfied, never a lie.
unsafe fn decode_present_fence(
    base: *mut u8,
    total: usize,
    kmd_range: core::ops::Range<usize>,
    path: SubmitPath,
) -> Option<PresentSubmissionBoundary> {
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
        PresentSubmissionPrivate::decode(base.cast(), total.min(u32::MAX as usize) as u32)
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

/// Pick up a DMA-BUFFER FLIP record from a submission's private data and arm
/// the scan-out programming for it.
///
/// This is the DMA-flip contract's equivalent of `SetVidPnSourceAddress`: for
/// an IMMEDIATE flip dxgkrnl never calls that DDI, so unless the driver
/// programs the display from HERE the scan-out never follows the flip at all
/// (ROADMAP defect 0aa). Runs at DISPATCH; the programming itself is deferred
/// to the PASSIVE display worker exactly as the MMIO path defers it, but with
/// this flip's DMA fence still outstanding while it happens.
///
/// Mints this flip's PRESENTATION EPOCH before the handle is published to the
/// display worker, so the worker can never bind a presentation whose epoch does
/// not exist yet. The epoch no longer gates the submission's completion —
/// 22.22.217.0 retired that withholding as measured inert — it is the
/// bookkeeping the ownership gate on the flush executor decides with
/// (ROADMAP defect 0ab-B).
///
/// # Safety
/// `base`/`total` describe the kernel-only DMA private-data buffer dxgkrnl
/// supplied for this submission.
unsafe fn arm_dma_flip(adapter: &AdapterContext, base: *mut c_void, total: u32) {
    let Some((h_alloc, primary_address, snapshot)) =
        (unsafe { crate::ddi::present_packet::PresentFlipPrivate::take(base, total) })
    else {
        return;
    };
    // NOTE (0ab-B, 22.22.210.0): capturing the completion boundary HERE was
    // tried and MEASURED NOT TO WORK. dxgkrnl submits a flip about a frame
    // after the app presented, so `next_wire_fence` at this point already
    // covers frame N+1 and the flush still waited a frame too long — the host
    // trace showed it released 60 us before the NEXT flip's bind, every cycle.
    // The boundary is captured at the present marker instead; see
    // `arm_scanout_refresh_after_current_venus`.
    //
    // The epoch this flip presents under. It used to gate the flip's own DMA
    // fence and the CRTC_VSYNC address; both halves were measured inert against
    // the black frames (the 2×2 factorial, 46 681 frames) because the app's
    // clear never travels in a WDDM DMA buffer and therefore waits on no
    // completion this driver controls. It is minted here regardless, because the
    // flush executor's ownership gate decides with it.
    let epoch = adapter.mint_present_epoch();
    if unsafe {
        crate::ddi::display::arm_dma_flip_programming(
            adapter,
            h_alloc,
            primary_address,
            epoch,
            snapshot,
        )
    } {
        crate::ddi::scanout_trace::note_dma_flip_armed();
        return;
    }
    // The handle could not be paired, so nothing will ever bind or flush this
    // epoch. End its lease immediately, or `present_epoch` stays permanently
    // ahead of `bound_epoch` and the ownership gate reads a presentation that
    // can never arrive as one that is still coming (`VpPrF` counts the pairing
    // failure itself).
    adapter.end_scanout_leases_through(epoch, crate::ddi::scanout_trace::LeaseEnd::Cancelled);
}

unsafe fn decode_virtual_present_fence(
    submit: &DXGKARG_SUBMITCOMMANDVIRTUAL,
) -> Option<PresentSubmissionBoundary> {
    let base = submit.pDmaBufferPrivateData.cast::<u8>();
    let total = submit.DmaBufferPrivateDataSize as usize;
    let umd = submit.DmaBufferUmdPrivateDataSize as usize;
    SUBMIT_VIRTUAL_COUNT.fetch_add(1, Ordering::Relaxed);
    // The virtual DDI has no submission start/end window: 0/0 is the truthful
    // report, and it no longer overwrites the legacy path's real offsets.
    unsafe { note_present_private_shape(SubmitPath::Virtual, base, total, umd, 0, 0, umd) };
    unsafe { decode_present_fence(base, total, umd..total, SubmitPath::Virtual) }
}

unsafe fn decode_legacy_present_fence(
    submit: &DXGKARG_SUBMITCOMMAND,
) -> Option<PresentSubmissionBoundary> {
    let base = submit.pDmaBufferPrivateData.cast::<u8>();
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
/// `resource_id` is the allocation the present named, or 0 when the marker
/// carries no identity. It travels with the watermark so the eventual flush
/// names the frame this marker belongs to rather than whatever happens to be
/// bound when it fires — see [`crate::virtio::gpu::VirtioGpu::note_scanout_refresh`].
fn arm_scanout_refresh_after_current_venus(
    adapter: &AdapterContext,
    resource_id: u32,
    stream_marker: Option<crate::adapter::PresentStreamMarker>,
    snapshot_submission: bool,
) {
    // Unsampled: what the app PRESENTED, against `Vs*` (what Windows asked us
    // to bind) and `Ff*` (what we told the host to re-read). Atomics only, so
    // it is legal on this DISPATCH-level path.
    crate::ddi::scanout_trace::MARKER_HISTOGRAM.note(resource_id);
    // …and whether the app was writing the buffer the host is DISPLAYING. See
    // `scanout_trace::MARKER_WHILE_BOUND` for why that is the candidate
    // mechanism for the remaining black-frame flashes (ROADMAP defect 0ab).
    if resource_id != 0
        && adapter
            .active_scanout_resource
            .load(core::sync::atomic::Ordering::Acquire)
            == resource_id
    {
        crate::ddi::scanout_trace::note_marker_while_bound();
    }
    let _ready =
        adapter.arm_present_marker_refresh(resource_id, stream_marker, snapshot_submission);
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
    // Before the completion bookkeeping: a flip carried in this buffer must be
    // armed while its fence is still outstanding, which is the whole point of
    // the DMA-flip contract. It also mints the presentation epoch and takes the
    // frame boundary this flip carries to its bind.
    unsafe {
        arm_dma_flip(
            adapter,
            submit.pDmaBufferPrivateData,
            submit.DmaBufferPrivateDataSize,
        )
    };
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
    unsafe {
        arm_dma_flip(
            adapter,
            submit.pDmaBufferPrivateData,
            submit.DmaBufferPrivateDataSize,
        )
    };
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
    // Every dropped fence was the only waiter on its scan-out presentation
    // lease. Release them all: a lease whose waiter has been discarded would
    // gate the NEXT presentation on a read nobody is accounting for. Done
    // BEFORE the critical section, because ending a lease publishes any withheld
    // primary address and signals the display worker, and neither belongs
    // inside a DISPATCH notification lock.
    adapter.release_all_scanout_leases(crate::ddi::scanout_trace::LeaseEnd::Teardown);
    // Preemption is the sole replayable scheduler outcome: dxgkrnl resubmits
    // the same private record after it re-establishes residency. Reset and
    // timeout abandon the epoch, so their WindowedBlt readers must be settled
    // or retained only through an already-dispatched host response.
    let retain_for_resubmit = matches!(&outcome, AbandonOutcome::Preempted { .. });
    adapter.with_wddm_notify_lock(|guard| {
        let dropped = guard
            .with_virtio(|o, v| {
                if retain_for_resubmit {
                    v.preempt_flush(o)
                } else {
                    v.terminal_abandon_wddm_epoch(o)
                }
            })
            .unwrap_or(0);
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
    adapter.with_wddm_notify_lock(|guard| {
        let _ = guard.with_virtio(|order, v| v.purge_all_present_streams_ordered(order));
    });
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

    // ── The D3D12 ECL arm (`HeliosD3D12SubmitCmd`, 16 B) ────────────────────
    //
    // `helios_umd12.dll` submits this through `pfnRenderCb` during
    // `pfnExecuteCommandLists` so the D3D12 queue's WDDM context carries a DMA
    // packet at all. The record's `gpu_wire_fence` is the completion boundary,
    // and writing it into THIS Render's `pDmaBufferPrivateData` is the whole
    // commit: `dxgkddi_submit_command{,_virtual}` already decode a
    // `PresentSubmissionPrivate` from that same buffer unconditionally
    // (`:871` / `:827`, keyed only on the record's magic — nothing there is
    // Present-specific), and `note_wddm_submission` already turns a nonzero
    // `gpu_fence_id` into `RetireDomain::IncludingGpu` with the exact watermark.
    //
    // ⚠ FIRST WRITE TO `pDmaBufferPrivateData` ON THIS DDI. Verified against the
    // WDK header rather than a doc: `_DXGKARG_RENDER` carries
    // `pDmaBufferPrivateData` + `DmaBufferPrivateDataSize`
    // (`tmp/dx12/sdk/d3dkmddi.h:130-147`). Offset 0, and the pointer is NOT
    // advanced — the shape the Present path already proves, and the shape
    // SubmitCommand's offset-0 fallback (`decode_present_fence`) reads.
    //
    // ⛔ THE WRITE IS UNCONDITIONAL, INCLUDING THE ZERO-FENCE ARM, and that is a
    // correction of this arm's first form (which declined to write when the fence
    // was 0). dxgkrnl RECYCLES DMA private-data buffers between submissions — the
    // reason `PresentFlipPrivate::take` consumes its record — so declining to
    // write leaves the PREVIOUS ECL's boundary at offset 0, and SubmitCommand then
    // gates this packet on a fence that has already retired: the application's
    // `ID3D12Fence` signals early while `D12Rec` moves, `D12MrgF` is 0 and
    // `GpuFncClamp` is 0. The original defect wearing the instrumentation of the
    // fix. Writing every time makes "no boundary" explicit for THIS submission
    // instead of inherited from another one, `mark_d3d12`'s own magic keeps an
    // all-zero payload identifiable (which is also what lets `WddmHoldMs` scope
    // itself), and `decode` consumes the D3D12 arm so a missing write can only
    // fail safe. It does not touch the `PRESENT_MARKER_*` trio; `D12Rec` and
    // `D12Merged` count this arm.
    //
    // The three arms in this function are magic-disjoint by construction, and
    // `protocol/src/wddm.rs`'s const asserts pin the two size relationships that
    // make this one reachable without shadowing the others.
    let mut stamped_d3d12 = false;
    if cmd_len >= size_of::<helios_protocol::HeliosD3D12SubmitCmd>() {
        let mut raw = [0u8; size_of::<helios_protocol::HeliosD3D12SubmitCmd>()];
        // SAFETY: the runtime guarantees `CommandLength` readable bytes at
        // `pCommand` (non-null, checked above) and `cmd_len` covers the record.
        unsafe {
            core::ptr::copy_nonoverlapping(
                args.pCommand as *const u8,
                raw.as_mut_ptr(),
                size_of::<helios_protocol::HeliosD3D12SubmitCmd>(),
            );
        }
        // SAFETY: `raw` is a fully initialized local of exactly the record size;
        // the read is unaligned-safe and cannot leave the local.
        let command = unsafe {
            core::ptr::read_unaligned(raw.as_ptr().cast::<helios_protocol::HeliosD3D12SubmitCmd>())
        };
        if command.is_valid() {
            D3D12_SUBMIT_RECORDS.fetch_add(1, Ordering::Relaxed);
            if command.gpu_wire_fence == 0 {
                D3D12_SUBMIT_ZERO_FENCE.fetch_add(1, Ordering::Relaxed);
            }
            match unsafe {
                PresentSubmissionPrivate::mark_d3d12(
                    args.pDmaBufferPrivateData,
                    args.DmaBufferPrivateDataSize,
                    command.gpu_wire_fence,
                )
            } {
                Ok(crate::ddi::present_packet::D3d12Mark::MergedWithPredecessor) => {
                    D3D12_SUBMIT_MERGED.fetch_add(1, Ordering::Relaxed);
                }
                Ok(crate::ddi::present_packet::D3d12Mark::First) => {}
                Err(_status) => {
                    // COUNT AND IGNORE, never a failing return: a non-SUCCESS
                    // status out of a submit DDI bugchecks
                    // `dxgmms2!VidSchiSendToExecutionQueue` with 0x119/1, which
                    // this file's own `SubmitAck` doc records.
                    // `STATUS_BUFFER_TOO_SMALL` is legal here for the DMA buffer,
                    // but it asks the runtime to GROW and retry — which is not an
                    // answer to a private-data buffer the driver itself sized.
                    D3D12_SUBMIT_MERGE_FAILS.fetch_add(1, Ordering::Relaxed);
                }
            }
            stamped_d3d12 = true;
        }
    }

    // ⛔ THE OTHER HALF OF THE UNCONDITIONAL WRITE ABOVE. A Render that did not
    // stamp a `'HD12'` record must not leave one it found, or it inherits another
    // submission's boundary. `mark_d3d12`'s write covers the ECL arm; this covers
    // every other `pfnRenderCb` on a D3D12 WDDM context — above all UP-9's
    // present-identity Render, which writes nothing at offset 0.
    //
    // Placed here rather than at the end of the function on purpose: in
    // `DxgkDdiRender` the ECL arm is the ONLY writer of offset 0 (the HEPR/HERF
    // arms below merge through `display.rs`'s Present DDI, never through this
    // one), so once that arm has been decided nothing later can re-stamp it.
    //
    // SAFETY: `pDmaBufferPrivateData` / `DmaBufferPrivateDataSize` are the pair
    // dxgkrnl supplied for THIS Render; the helper re-checks null and size before
    // touching a byte, and writes only the 4-byte magic at offset 0.
    if !stamped_d3d12
        && unsafe {
            PresentSubmissionPrivate::invalidate_d3d12(
                args.pDmaBufferPrivateData,
                args.DmaBufferPrivateDataSize,
            )
        }
    {
        D3D12_STALE_RECORD_CLEARED.fetch_add(1, Ordering::Relaxed);
    }

    // The 16-byte HERF prefix is the legacy command.  Zero-fill a local full
    // form so an old UMD's absent tail cannot be read as stale runtime bytes;
    // only a complete 32-byte tail can select a registered stream boundary.
    const PRESENT_REFRESH_PREFIX: usize =
        core::mem::offset_of!(helios_protocol::HeliosPresentRefreshCmd, present_ctx_id);
    if cmd_len >= PRESENT_REFRESH_PREFIX {
        let take = cmd_len.min(size_of::<helios_protocol::HeliosPresentRefreshCmd>());
        let mut raw = [0u8; size_of::<helios_protocol::HeliosPresentRefreshCmd>()];
        unsafe {
            core::ptr::copy_nonoverlapping(args.pCommand as *const u8, raw.as_mut_ptr(), take);
        }
        let command = unsafe {
            core::ptr::read_unaligned(
                raw.as_ptr()
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
                let stream_marker = if take >= size_of::<helios_protocol::HeliosPresentRefreshCmd>()
                    && command.present_ctx_id != 0
                    && command.present_value != 0
                    && command.present_cookie != 0
                {
                    context
                        .as_ref()
                        .and_then(|c| c.creator_process())
                        .map(|creator_process| crate::adapter::PresentStreamMarker {
                            ctx_id: command.present_ctx_id,
                            value: command.present_value,
                            cookie: command.present_cookie,
                            creator_process,
                        })
                } else {
                    None
                };
                if let (Some(context), Some(marker)) = (context.as_ref(), stream_marker) {
                    context.stash_present_stream_marker(marker.ctx_id, marker.value, marker.cookie);
                }
                // HERF carries no resource identity: it is the generic
                // "the bound target is dirty" edge, so it arms with 0 and the
                // flush resolves the bound resource as before.
                arm_scanout_refresh_after_current_venus(adapter, 0, stream_marker, false);
            }
        }
    }

    // `HeliosPresentRenderCmd` grew 48 -> 56 B when the D4b snapshot appended
    // `venus_alloc_size` to its embedded `HeliosPresentPrivateData` (prefix-
    // compatible: nothing before it moved). Decode from the 48-byte PREFIX,
    // which covers everything the MARKER arm consumes — `is_valid()`
    // (magics/versions/resid) and `present.resource_id` — so a pre-snapshot
    // UMD's 48-byte command still arms the marker; the appended tail reads as
    // zero then, and only the snapshot STASH arm below consults it, gated on
    // full 56-byte coverage. On a substituted present `resource_id` already
    // IS the snapshot resid (by design — that is what keys the frame
    // watermark to the bound identity).
    const PRESENT_RENDER_CMD_PREFIX: usize =
        core::mem::offset_of!(helios_protocol::HeliosPresentRenderCmd, present)
            + core::mem::offset_of!(helios_protocol::HeliosPresentPrivateData, venus_alloc_size);
    if cmd_len >= PRESENT_RENDER_CMD_PREFIX {
        let take = cmd_len.min(size_of::<helios_protocol::HeliosPresentRenderCmd>());
        let mut raw = [0u8; size_of::<helios_protocol::HeliosPresentRenderCmd>()];
        // SAFETY: the runtime guarantees `CommandLength` readable bytes at
        // `pCommand`; `take` never exceeds it or the local buffer.
        unsafe {
            core::ptr::copy_nonoverlapping(args.pCommand as *const u8, raw.as_mut_ptr(), take);
        }
        let command = unsafe {
            core::ptr::read_unaligned(
                raw.as_ptr()
                    .cast::<helios_protocol::HeliosPresentRenderCmd>(),
            )
        };
        if command.is_valid() {
            static PRESENT_RENDER_DIAG_COUNT: AtomicU32 = AtomicU32::new(0);
            let diag = PRESENT_RENDER_DIAG_COUNT.fetch_add(1, Ordering::Relaxed) < 4;
            let mut snapshot_submission = false;
            let mut windowed_blt_snapshot = false;
            if h_context.is_null() {
                if diag {
                    crate::diag::record_named_bytes(b"PRset", 0xE1);
                }
            } else {
                let context = unsafe { crate::device::ContextHandleRef::from_raw(h_context) };
                let adapter = context.as_ref().and_then(|c| c.adapter());
                // D4b: the RENDER command is the descriptor's DELIVERY ROUTE.
                // dxgkrnl never forwards the UMD's PresentCb private data to
                // DxgkDdiPresent on flip presents (PBIdOk = "no payload"
                // across three driver generations), so a flagged descriptor
                // is STASHED on the context here and taken by the Present
                // that follows it on this same context/thread
                // (`ContextHandleRef::take_snapshot_stash` holds the pairing
                // and orphan contract). Trusted only when the command covers
                // the full 56-byte form — the 48-byte prefix decode above can
                // legitimately carry the flag bit while the appended
                // `venus_alloc_size` was never written.
                if command.present.reserved & helios_protocol::HELIOS_PRESENT_PRIVATE_FLAG_SNAPSHOT
                    != 0
                {
                    // Snapshot coverage remains the old v1 RenderCmd boundary:
                    // header + PresentPrivateData through venus_alloc_size
                    // (56 bytes).  The separately appended stream tail needs
                    // the new full 72-byte form below.
                    const PRESENT_RENDER_SNAPSHOT_BYTES: usize =
                        core::mem::offset_of!(helios_protocol::HeliosPresentRenderCmd, present)
                            + core::mem::offset_of!(
                                helios_protocol::HeliosPresentPrivateData,
                                present_ctx_id
                            );
                    if take >= PRESENT_RENDER_SNAPSHOT_BYTES {
                        let windowed = command.present.reserved
                            & helios_protocol::HELIOS_PRESENT_PRIVATE_FLAG_WINDOWED_BLT_SNAPSHOT
                            != 0;
                        // The WindowedBlt import consumes the v2 tail's exact
                        // memory type/purpose. A prefix-compatible direct D4b
                        // command may never be reinterpreted as one.
                        if windowed && take < size_of::<helios_protocol::HeliosPresentRenderCmd>() {
                            crate::ddi::scanout_trace::note_snapshot_fallback();
                        } else if let Some(context) = context.as_ref() {
                            context.stash_snapshot(
                                &helios_kmd_logic::snapshot_bind::SnapshotDescriptor {
                                    resource_id: command.present.resource_id,
                                    width: command.present.width,
                                    height: command.present.height,
                                    pitch: command.present.pitch,
                                    dxgi_format: command.present.dxgi_format,
                                    plane_offset: command.present.plane_offset,
                                    venus_alloc_size: command.present.venus_alloc_size,
                                    memory_type_index: command.present.snapshot_memory_type_index,
                                    purpose: command.present.snapshot_purpose,
                                },
                            );
                            snapshot_submission = true;
                            windowed_blt_snapshot = windowed;
                        }
                    } else {
                        // Flagged without coverage of the appended field: the
                        // descriptor cannot be trusted, so the present binds
                        // the flipped allocation — counted, never silent.
                        crate::ddi::scanout_trace::note_snapshot_fallback();
                    }
                }
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
                    // DxgkDdiPresent still selects the exact source from
                    // dxgkrnl's allocation list -- these private bytes are NOT
                    // a second scanout selector, and nothing here binds.
                    //
                    // They ARE the frame's identity, and that is a different
                    // question the driver previously had no answer to. Passing
                    // it makes the dirty edge name the buffer it belongs to, so
                    // the flush cannot land on the previous buffer (stale) or
                    // on one the flip has advanced to but the app has not yet
                    // rendered (black).
                    if let Some(adapter) = adapter {
                        const PRESENT_RENDER_STREAM_BYTES: usize =
                            core::mem::offset_of!(helios_protocol::HeliosPresentRenderCmd, present)
                                + core::mem::offset_of!(
                                    helios_protocol::HeliosPresentPrivateData,
                                    snapshot_memory_type_index
                                );
                        let stream_marker = if take >= PRESENT_RENDER_STREAM_BYTES
                            && private.present_ctx_id != 0
                            && private.present_value != 0
                            && private.present_cookie != 0
                        {
                            context.as_ref().and_then(|c| c.creator_process()).map(
                                |creator_process| crate::adapter::PresentStreamMarker {
                                    ctx_id: private.present_ctx_id,
                                    value: private.present_value,
                                    cookie: private.present_cookie,
                                    creator_process,
                                },
                            )
                        } else {
                            None
                        };
                        // The scanout arm is not enough: VidSch's WDDM fence
                        // must carry this exact boundary too, or it can retire
                        // before a tagged batch has reached the transport.
                        if let (Some(context), Some(marker)) = (context.as_ref(), stream_marker) {
                            context.stash_present_stream_marker(
                                marker.ctx_id,
                                marker.value,
                                marker.cookie,
                            );
                        }
                        if windowed_blt_snapshot {
                            // Keep Render's causal handoff observable without
                            // resolving the marker here: resolution touches the
                            // transport marker table and would mutate its
                            // telemetry before Present owns the transaction.
                            // The raw ctx/value plus registration cookie is the
                            // complete producer identity; Present's ARM event
                            // records the resolved opaque boundary.
                            let (raw_stream, cookie) = match stream_marker {
                                Some(marker) => (
                                    (u64::from(marker.ctx_id) << 32) | u64::from(marker.value),
                                    marker.cookie,
                                ),
                                None => (0, 0),
                            };
                            crate::ddi::scanout_timeline::note(
                                crate::ddi::scanout_timeline::kind::WINDOWED_BLT_STASH,
                                crate::ddi::scanout_timeline::flag::SNAPSHOT,
                                0,
                                raw_stream,
                                cookie,
                                private.resource_id,
                                private.snapshot_memory_type_index,
                            );
                        }
                        if !windowed_blt_snapshot {
                            arm_scanout_refresh_after_current_venus(
                                adapter,
                                private.resource_id,
                                stream_marker,
                                snapshot_submission,
                            );
                        }
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
        output.__bindgen_anon_1.Value = i & 0x00ff_ffff;
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
