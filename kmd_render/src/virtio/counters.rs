//! Transport telemetry: every named atomic `HELIOS_ESCAPE_QUERY_STATS` and
//! `DxgkDdiCollectDbgInfo` report, plus the two table-capacity accessors.
//!
//! Moved verbatim out of `virtio/gpu.rs` by T8/R1103, which re-exports this
//! module wholesale so the 53+ external `gpu::<COUNTER>` paths are unchanged.

use core::sync::atomic::{AtomicU32, Ordering};

use super::gpu::{MAX_BLOBS, MAX_RESOURCES};

/// Count of synchronous control-command timeouts (a passive waiter gave up and
/// abandoned its in-flight slot). Unlike the old model this does NOT poison
/// the transport — the slot is reaped when the completion eventually arrives —
/// but nonzero still means the host stopped answering in time. Read by
/// `DxgkDdiCollectDbgInfo` / `HELIOS_ESCAPE_QUERY_STATS` (acceptance: stays 0).
pub static CTRL_TIMEOUT_COUNT: AtomicU32 = AtomicU32::new(0);

// ── C3/M3.4 async-transport telemetry (all DISPATCH-safe atomics) ────────────

/// Async SUBMIT_3D enqueues.
pub static ASYNC_SUBMIT_COUNT: AtomicU32 = AtomicU32::new(0);
/// Async SUBMIT_3D completions drained from the used ring.
pub static ASYNC_COMPLETE_COUNT: AtomicU32 = AtomicU32::new(0);
/// Async SUBMIT_3D completions whose ctrl response was not RESP_OK.
pub static ASYNC_RESP_ERRORS: AtomicU32 = AtomicU32::new(0);
/// WAIT_FENCE waiters registered (fence was still in flight at wait time).
pub static FENCE_WAIT_REGISTERED: AtomicU32 = AtomicU32::new(0);
/// WAIT_FENCE waits that timed out — the host did not complete this fence.
///
/// This is the counter the project reads as fence evidence and dumps into the
/// TDR report, so it must mean exactly one thing. It used to also count
/// [`FENCE_WAIT_TABLE_FULL`]'s condition, where the host is perfectly healthy.
pub static FENCE_WAIT_TIMEOUTS: AtomicU32 = AtomicU32::new(0);
/// WAIT_FENCE waits that gave up because all [`MAX_FENCE_WAITERS`] slots were
/// occupied for the whole retry budget — a *guest* table-size condition, not a
/// host one, and the fix is a bigger table rather than a host investigation.
///
/// `gpu.rs`'s own comment on `MAX_FENCE_WAITERS` says dwm plus several apps plus
/// WUDFHost are expected concurrently, so the 65th waiter is a reachable state.
/// Before the split it incremented [`FENCE_WAIT_TIMEOUTS`], so post-mortem
/// evidence could not distinguish a wedged host from a table that needs
/// resizing — and the TDR report blamed the host.
pub static FENCE_WAIT_TABLE_FULL: AtomicU32 = AtomicU32::new(0);
/// A `ctrl_roundtrip` whose waiter was abandoned because the transport went away
/// mid-wait (StopDevice), rather than because the host timed out. Previously
/// folded into "already completed successfully" by `unwrap_or(true)`.
pub static CTRL_TEARDOWN_ABANDONS: AtomicU32 = AtomicU32::new(0);
/// A `wait_fence` that found the transport gone. Previously reported as
/// `Complete`, which made `escape_wait_fence` tell the ICD that a wire fence had
/// retired when it never did.
pub static TRANSPORT_GONE_AT_WAIT: AtomicU32 = AtomicU32::new(0);
/// Used-ring completions whose token matched no in-flight entry (ring state
/// corrupt → transport latches failed).
pub static DRAIN_BAD_TOKEN: AtomicU32 = AtomicU32::new(0);
/// Enqueue attempts refused because the queue/parked tables were full.
pub static QUEUE_FULL_RETRIES: AtomicU32 = AtomicU32::new(0);
/// WDDM pending-fence FIFO overflows (degraded to immediate completion).
pub static WDDM_PENDING_OVERFLOWS: AtomicU32 = AtomicU32::new(0);
/// High-water of concurrently in-flight control-queue entries.
pub static INFLIGHT_HIGH_WATER: AtomicU32 = AtomicU32::new(0);
/// High-water of parked (completed, awaiting PASSIVE free) entries.
pub static PARKED_HIGH_WATER: AtomicU32 = AtomicU32::new(0);
/// Parked entries force-forgotten because the parked table was full (leaked
/// DMA memory — must stay 0; the enqueue gate makes this unreachable).
pub static PARKED_LEAKS: AtomicU32 = AtomicU32::new(0);
/// WDDM submissions completed by the DPC (real venus-driven fences).
pub static WDDM_FENCE_FROM_DPC: AtomicU32 = AtomicU32::new(0);
/// Why the WDDM FIFO head was not ready when a DPC looked at it. Exactly one
/// of the three moves per blocked look, so their sum is the blocked-look total
/// and their ratio names WHICH dependency paces fence retirement. Mirrored as
/// `WfBWire` / `WfBStrm` / `WfBBlt`.
pub static WDDM_HEAD_BLOCKED_WIRE: AtomicU32 = AtomicU32::new(0);
pub static WDDM_HEAD_BLOCKED_STREAM: AtomicU32 = AtomicU32::new(0);
pub static WDDM_HEAD_BLOCKED_BLT: AtomicU32 = AtomicU32::new(0);
/// The fourth arm, and the only one that is not a real dependency: an otherwise
/// READY D3D12 ECL packet held back by `WddmHoldMs`. Mirrored as `WfBHold`.
///
/// Must be 0 on any shipping deployment — the knob defaults to 0. Nonzero means
/// somebody is running UV1's experiment, and its magnitude is the number of DPC
/// looks the hold absorbed, not a duration.
///
/// WHOSE ACTIVITY INCREMENTS IT: only a packet carrying a `HeliosD3D12SubmitCmd`
/// record, i.e. `helios_umd12.dll`'s. ⭐ **DWM CANNOT MOVE IT** — the hold is
/// scoped by that record's identity precisely because this FIFO is adapter-global
/// and head-of-line, so an unscoped hold would stall the desktop.
///
/// ⛔⛔ **IT IS ALSO UV1'S ARMED-CHECK, AND THE UV1 READING TABLE IS UNSOUND
/// WITHOUT IT.** That table (`diag::knobs::WDDM_HOLD_MS`) grades a flat
/// `WaitForSingleObject` measurement as **UV1 ✗** — *"none of K-F3..K-F9 is the
/// answer, say so loudly and stop"* — which is a conclusion drawn from an
/// ABSENCE. Three unrelated states produce that same flat reading:
///   1. the hold never armed because `WddmHoldMs` was set but the transport was
///      not re-initialised. The knob is snapshotted ONCE, at `VirtioGpu::init`
///      from StartDevice, so **setting the registry value without
///      `pnputil /restart-device` leaves the hold at 0** and every packet
///      completes unheld;
///   2. the hold armed but no D3D12 ECL packet ever reached the FIFO head with
///      its three real dependencies already satisfied, so the fourth arm was
///      never reached (it is deliberately last);
///   3. the hold really did delay packets and the runtime's fence advance is
///      genuinely independent of them — the ONLY state that supports UV1 ✗.
///
/// ⇒ **PRECONDITION: `WfBHold` must have MOVED during the measured window.** If
/// it did not, the run measured nothing and the correct report is "the experiment
/// did not run", not UV1 ✗. Registry values persist across boots (CLAUDE.md rule
/// 6), so it is the movement that counts, never the absolute value.
pub static WDDM_HEAD_BLOCKED_HOLD: AtomicU32 = AtomicU32::new(0);
/// WDDM FIFO heads whose TAGGED-namespace dependency was REBASED onto the
/// conservative wire watermark after blocking for `WddmHeadMs` (default 250 ms).
/// Mirrored as `WfBReb`.
///
/// ⛔ THIS IS A PRICE TAG, NOT A HEALTH METRIC. Each increment is one WDDM DMA
/// fence released while the producer it named had not necessarily completed — the
/// 0ab-B stale/black-frame class, deliberately, because the two alternatives are
/// the 256-entry FIFO overflow (the same lie times 256, plus
/// `release_all_scanout_leases(Teardown)`) and an adapter-wide TDR.
///
/// WHOSE ACTIVITY INCREMENTS IT: any context whose submission named a TAGGED
/// dependency — a present-stream boundary or a windowed-BLT terminal. Both are
/// D3D11 present-path constructs, so **DWM CAN MOVE IT and a D3D12 client
/// normally cannot** (an ECL packet's boundary is a wire fence, and the wire arm
/// is not rebasable).
///
/// GRADING: **must read 0 on a healthy session.** A nonzero value means some
/// context named a boundary that stayed unsatisfiable for a quarter of a second.
/// ⚠ It cannot be nonzero from a wire-fence block: that arm is deliberately not
/// rebasable, so `WfBWire` climbing with `WfBReb` at 0 is the expected shape of an
/// ordinary busy desktop.
///
/// ⛔⛔ **`WfBStrm` / `WfBBlt` CANNOT DIAGNOSE IT, AND AN EARLIER GRADING SAID
/// THEY COULD** — *"the counter that MOVED before this one is the diagnosis, since
/// a rebase is always preceded by blocked looks on exactly one arm"*. Both halves
/// fail. The premise is true only of the SINGLE look that rebased; the counters
/// are session-cumulative, adapter-global counts of blocked DPC looks that climb
/// continuously under DWM, so by the time one `WfBReb` fires each sibling already
/// carries thousands of increments from unrelated heads. "Which one moved" is not
/// a question two monotonic totals can answer without a per-rebase snapshot, and
/// nothing snapshots them.
///
/// ⇒ The arm is recorded at the rebase instead, by
/// [`WDDM_HEAD_REBASED_STREAM`] / [`WDDM_HEAD_REBASED_BLT`]. Those two partition
/// this counter exactly (`WfBRebS + WfBRebB == WfBReb`) and ARE the diagnosis.
pub static WDDM_HEAD_REBASED: AtomicU32 = AtomicU32::new(0);
/// The [`WDDM_HEAD_REBASED`] subset whose blocking arm at the rebasing look was
/// the PRESENT-STREAM boundary. Mirrored as `WfBRebS`.
///
/// WHOSE ACTIVITY: any context with a registered present stream — i.e. the D3D11
/// present path, DWM's included. It is not D3D12-specific.
///
/// ⚠ WHAT IT NAMES, precisely: the arm that was still unsatisfied on the look that
/// EXPIRED the bound, not necessarily the arm that armed it. A head can carry both
/// a stream boundary and a windowed-BLT terminal, and `take_one_ready_wddm` tests
/// stream first; if the stream cleared while the bound was running and the BLT did
/// not, the rebase is attributed to the BLT — which is the more useful of the two,
/// because it is the dependency that was actually abandoned.
pub static WDDM_HEAD_REBASED_STREAM: AtomicU32 = AtomicU32::new(0);
/// The [`WDDM_HEAD_REBASED`] subset whose blocking arm at the rebasing look was
/// the WINDOWED-BLT terminal prefix. Mirrored as `WfBRebB`. Same attribution
/// caveat as [`WDDM_HEAD_REBASED_STREAM`], and the same population — the windowed
/// present path, not D3D12.
pub static WDDM_HEAD_REBASED_BLT: AtomicU32 = AtomicU32::new(0);
/// Fenced SUBMIT_3D enqueues carrying ring_idx >= 1 (GPU-completion fences —
/// WS1 #4 consumer-side ordering; these retire at host GPU completion, not
/// decode, so they legally stay in flight for the full GPU-work duration).
///
/// ⚠ **ADAPTER-WIDE, AND DOMINATED BY THIS DRIVER'S OWN COPIES.** Three
/// producers reach ring 1 without any guest involvement, all via
/// [`SCANOUT_RING_IDX`](super::gpu::SCANOUT_RING_IDX): the scanout copy
/// (`gpu/mod.rs:3442`), the windowed-BLT submit (`:3375`), and — through the
/// GENERIC entry point, which is why an entry-point-name audit misses it —
/// `submit_venus_async_present` (`virtio/ctrl.rs:1541-1547`). So this counter
/// alone can never answer "did the GUEST submit anything that retires at host
/// GPU completion"; DWM compositing produces ring-1 traffic here by itself.
/// [`ESCAPE_SUBMIT_RING_COUNT`] is the attributed half, counted where the guest
/// value enters, and the pair is deliberately not a subtraction: a fourth
/// internal producer would silently corrupt an inferred difference.
pub static RING_SUBMIT_COUNT: AtomicU32 = AtomicU32::new(0);
/// ring_idx >= 1 completions drained from the used ring. Same three internal
/// producers as [`RING_SUBMIT_COUNT`]; `RngSub - RngCmp` is the in-flight
/// ring-1 depth, and a permanent gap means ring-1 work that never retired.
pub static RING_COMPLETE_COUNT: AtomicU32 = AtomicU32::new(0);

// ── Guest-attributed SUBMIT_VENUS traffic (2026-08-06, KMD_IMPACT §14a.1 UV3) ─
//
// WHY THESE EXIST. UV3 — "does vkd3d's venus work retire at host GPU completion
// or at decode?" — was documented as pre-checkable with zero code by reading
// [`RING_SUBMIT_COUNT`] before and after a run. It is not: that counter is
// adapter-wide and dominated by this driver's own scanout/BLT copies (above),
// and until this commit it was READ in exactly one place — words 33/34 of the
// `'HDBG'` report `DxgkDdiCollectDbgInfo` emits, which dxgkrnl calls only on a
// TDR. Reading it required provoking the failure it was supposed to predict.
//
// These two count only submissions that arrived through the guest's
// `HELIOS_ESCAPE_SUBMIT_VENUS`, attributed at `submit_venus_async_inner`
// (`virtio/ctrl.rs`), which is reachable from nowhere else (`ddi/escape.rs:1233`
// and `:1242` are its only callers).
//
// ⛔⛔ HOW TO READ THEM: **ONLY AS DELTAS AGAINST A CONTROL ARM. THE ZERO FORMS
// ARE UNSATISFIABLE READINGS AND ANY RULE PHRASED ON ONE IS UNFALSIFIABLE.**
//
// WHOSE ACTIVITY INCREMENTS THEM. Every client of this adapter's, DWM's
// included. "Guest-attributed" means attributed to the guest SIDE of the wire —
// `submit_venus_async_inner` counts each `HELIOS_ESCAPE_SUBMIT_VENUS` from
// whichever process made it — NOT attributed to a process. They are plain
// statics in the driver image with no reset at StartDevice, so a value is
// cumulative over every device generation since the image loaded, and
// `pnputil /restart-device` does not zero it.
//
// ⛔ AN EARLIER REVISION OF THIS COMMENT SAID *"the informative reading is a
// ZERO"* AND GAVE `EscSub > 0 && EscSubRing == 0` AND `EscSub == 0` AS DECISION
// RULES. Neither branch can ever be taken on a live desktop, so their absence
// would have been read as evidence for the other branch. The mechanism that
// forecloses them is the SHIPPING D3D11 PRESENT PATH:
// `vn_signal_win32_external_semaphore` (`icd/mesa/src/virtio/vulkan/vn_queue.c`)
// builds a `vn_renderer_submit_batch` with `ring_idx =
// sem->external_payload.ring_idx`, which `vn_queue_submission_prepare` sets to
// the submitting `VkQueue`'s own `ring_idx`; every `VkQueue` acquires
// its ring through `vn_instance_acquire_ring_idx` (`vn_device.c`), and ring 0 is
// RESERVED for the CPU timeline (`vn_instance.c` seeds `ring_idx_used_mask` with
// bit 0), so that value is always >= 1. When the payload carries a ring seqno
// the batch also carries `cs_size != 0`, and `helios_submit`
// (`vn_renderer_helios.c`) turns exactly such a batch into ONE
// `HELIOS_ESCAPE_SUBMIT_VENUS`. ⇒ each DWM present through a shared/win32
// semaphore adds ~1 to `EscSub` AND ~1 to `EscSubRing`, with no D3D12 client on
// the box at all.
//
// ⭐ THE PROCEDURE THAT DOES WORK — an idle-desktop delta and a probe delta over
// the SAME wall-clock window, subtracted:
//   * `Δ EscSubRing` GROWS when a D3D12 workload is added ⇒ vkd3d's work does
//     reach a GPU-completion ring. This is the reading that would refute
//     "vkd3d's work is ring-0 only", and it is the whole reason these exist.
//   * `Δ EscSubRing` UNCHANGED while `Δ EscSub` grows ⇒ the D3D12 client submits
//     through this transport but never on a GPU-completion ring.
//     `RetireDomain::IncludingGpu` (implemented as "no in-flight entry below the
//     watermark, whatever its ring") is then gating on host DECODE, not GPU
//     completion, and a D3D12 fence built on it would report decode and lie.
//   * NEITHER delta grows ⇒ this driver saw nothing new from that process. That
//     is the shape that fits a 0.8–1.1 µs fence wait: a plain `vkQueueSubmit`'s
//     command stream never touches virtio (it rides the shared ring), and the
//     only virtio submit a frame can produce by itself is the
//     `vkNotifyRingMESA` doorbell — hardcoded `ring_idx = 0` (`/* CPU ring */`
//     in `vn_renderer_util.h`'s `vn_renderer_submit_simple`) and emitted only when
//     the host ring advertises IDLE and the `VN_RING_IDLE_TIMEOUT_NS` limiter has
//     expired (`vn_ring.c`). A ring busier than 1 ms emits no doorbell and
//     therefore no wire fence at all.
//   * `Δ RngSub` climbing while `Δ EscSubRing` stays put ⇒ all NEW ring-1 work is
//     this driver's own compositor copies. That is the state the old single
//     counter could not distinguish from success.
//
// ⚠ The control arm is not optional and it is not arithmetic: there is no
// constant to subtract, because DWM's per-present contribution scales with how
// much the desktop composited during the window. Two windows of equal wall-clock
// length with the desktop otherwise untouched is the whole method.
//
// ⚠ [`ASYNC_SUBMIT_COUNT`] is NOT the guest total and cannot stand in for
// `EscSub`: it is bumped inside `enqueue_submit_inner`, so it already includes
// all four entry points, scanout and BLT copies among them. It is mirrored
// separately as `AsSub`.

/// Fenced SUBMIT_3D enqueues accepted from a guest `HELIOS_ESCAPE_SUBMIT_VENUS`,
/// any ring. Counted only on the arm the transport accepted, so it is directly
/// comparable with [`ASYNC_SUBMIT_COUNT`] rather than counting refusals.
pub static ESCAPE_SUBMIT_COUNT: AtomicU32 = AtomicU32::new(0);
/// The `ring_idx >= 1` subset of [`ESCAPE_SUBMIT_COUNT`] — guest-originated work
/// that retires at host GPU completion rather than at decode.
///
/// The guest value is counted before `enqueue_submit_inner`'s
/// `ring_idx.min(u8::MAX)` clamp, which cannot turn a nonzero ring into zero, so
/// this subset is exactly the set of submits that carried
/// `VIRTIO_GPU_FLAG_INFO_RING_IDX` onto the wire.
pub static ESCAPE_SUBMIT_RING_COUNT: AtomicU32 = AtomicU32::new(0);
/// Guest-supplied completion boundaries REPLACED by `next_wire_fence` because
/// they were zero-or-beyond the fences this driver has actually assigned.
///
/// The clamp itself is old and correct — *"a malformed/stale private marker must
/// not manufacture an impossible future dependency"* — and until 2026-08-06 it
/// was silent, which made the one lossy step on the boundary path invisible: the
/// WDDM fence then reports a watermark the writer never named. Both writers reach
/// it, `PresentSubmissionPrivate`'s BLT fence and `HeliosD3D12SubmitCmd`'s
/// `gpu_wire_fence`, so it is not named after either.
///
/// ⛔ WHOSE ACTIVITY INCREMENTS IT: **BOTH WRITERS' — SO DWM'S D3D11 PRESENTS
/// MOVE IT WITH NO D3D12 CLIENT ON THE BOX.** The rejection is decided in
/// `wddm_boundary::select` BEFORE the `d3d12` bit is consulted, so a
/// `PresentSubmissionPrivate` BLT marker naming a stale id lands here exactly as a
/// `HeliosD3D12SubmitCmd` would. An earlier grading said a nonzero value means
/// *"the UMD named a fence id this KMD never issued"* — there is no "the UMD"
/// here, and reading it as a D3D12 finding attributes the present path's clamps
/// to `helios_umd12.dll`.
///
/// ⚠ NOT RESET AT StartDevice (a plain image-lifetime static), while the standard
/// deploy is `pnputil /restart-device`. A value therefore spans every device
/// generation since the image loaded, and CLAUDE.md rule 6 applies in full:
/// verify it MOVES within the window you are attributing before reading anything
/// into it.
///
/// GRADING, honestly: **a DELTA over a window in which only the D3D12 client
/// changed** is the only attributable form. A nonzero delta then means that client
/// sampled a fence id this KMD has not issued (a stale sample, or a value from
/// another transport generation), and the packet fell back to waiting on the whole
/// backlog instead of its own work — slower, never wedged. `RngSub`/`EscSubRing`
/// (as deltas, see their block above) say whether such a fence could exist at all.
/// ⛔ An absolute reading cannot separate the two writers and must not be quoted
/// as a D3D12 number.
pub static GPU_FENCE_CLAMPED: AtomicU32 = AtomicU32::new(0);
/// Guest-supplied completion boundaries REJECTED because they name a fence from a
/// FOREIGN transport generation — below this instance's `wire_fence_base`.
///
/// A6 (`docs/dx12/PENDING.md` §1). [`GPU_FENCE_CLAMPED`]'s condition is
/// `id == 0 || id >= next_wire_fence`, a one-sided bound, and every StartDevice
/// strides the id range up by 2^32 — so a fence sampled before a
/// StopDevice/StartDevice cycle sits BILLIONS below the new range, passes that
/// bound, matches no in-flight entry, and satisfies the dependency instantly. The
/// old code could not tell that apart from a boundary that had genuinely retired:
/// the WDDM fence completed early and `GpuFncClamp` stayed at 0.
///
/// ⚠ Exactly one of the two counters moves per rejected boundary; they are not
/// summable into "bad boundaries" without double-counting neither, but a nonzero
/// reading in either means the packet fell back to the conservative
/// `next_wire_fence` prefix rather than to the boundary the writer named.
///
/// ⛔ WHOSE ACTIVITY INCREMENTS IT: **BOTH WRITERS' — DWM'S D3D11 PRESENT BLT
/// MARKER REACHES THIS ARM TOO**, for the same reason as [`GPU_FENCE_CLAMPED`]:
/// the generation test runs inside `wddm_boundary::select` before the `d3d12` bit
/// is looked at. A survivor of a device restart is far more likely to be DWM (it
/// is the longest-lived D3D client on the box and it is not restarted by
/// `pnputil /restart-device`) than a freshly launched D3D12 probe.
///
/// ⛔⛔ GRADING, CORRECTED — **"0 on any session without a device restart" IS NOT
/// EVALUABLE FROM THIS COUNTER.** It is a plain image-lifetime static with NO
/// reset at StartDevice, and the standard deploy IS `pnputil /restart-device`. So
/// after the first restart in a boot the value is permanently nonzero and stays
/// nonzero for every later reading, whether or not the session being graded had a
/// restart of its own. The counter cannot tell you which generation its
/// increments came from.
///
/// ⇒ The evaluable forms:
///   * a DELTA across a window with NO device restart in it — must be 0. Nonzero
///     there means some client is sampling fence ids that did not come from this
///     transport, which is the different and worse finding;
///   * a DELTA across a window that DID contain a restart — a small number is the
///     surviving-client case the striding comment at `VirtioGpu::init` describes,
///     and it is expected;
///   * the absolute value — says only "at least one restart has happened since
///     this driver image loaded", which the boot log already says better.
pub static GPU_FENCE_FOREIGN_GENERATION: AtomicU32 = AtomicU32::new(0);
/// `WAIT_FENCE` / `REGISTER_FENCE_EVENT` refusals of a fence id from a FOREIGN
/// transport generation — the same one-sided-bound defect as
/// [`GPU_FENCE_FOREIGN_GENERATION`], on the two usermode-facing predicates.
///
/// Before this counter existed those calls returned `Complete` / `AlreadyComplete`
/// for such an id: the ICD was told a wire fence had retired when its whole
/// transport generation was gone, which is [`TRANSPORT_GONE_AT_WAIT`]'s failure
/// dressed as success. Its own counter rather than [`FENCE_EVENT_INVALID`], which
/// already pools several unrelated rejections.
///
/// ⛔ WHOSE ACTIVITY INCREMENTS IT: **ANY PROCESS'S ICD, DWM'S ABOVE ALL.** The
/// two increment sites are `VirtioGpu::fence_wait_prepare` and
/// `VirtioGpu::fence_event_register` — the `HELIOS_ESCAPE_WAIT_FENCE` and
/// `REGISTER_FENCE_EVENT` escapes — which every venus client makes constantly and
/// which have nothing to do with the WDDM submission path or with D3D12. An
/// earlier grading said a nonzero value means *"a UMD is sampling fence ids from
/// somewhere other than this transport"*; the population is "every waiter on the
/// adapter", and DWM's DXVK is the busiest member of it.
///
/// ⛔⛔ GRADING: **the same correction as [`GPU_FENCE_FOREIGN_GENERATION`] —
/// "0 without a device restart" IS NOT EVALUABLE from the counter.** No reset at
/// StartDevice + `pnputil /restart-device` as the standard deploy ⇒ permanently
/// nonzero after the first restart in a boot. Only a delta over a restart-free
/// window can be graded, and only that delta must be 0.
///
/// ⚠ It is NOT expected to track [`GPU_FENCE_FOREIGN_GENERATION`]'s value: the two
/// paths have different callers (the WDDM boundary vs the ICD's own waits), and a
/// client that survives a restart typically has many parked waits and no WDDM
/// submissions of its own.
pub static FENCE_ID_FOREIGN_GENERATION: AtomicU32 = AtomicU32::new(0);
/// Fire-and-forget control commands queued by PASSIVE workers (currently the
/// scanout RESOURCE_FLUSH path).  These own their DMA buffers until the normal
/// used-ring drain retires them, but never park a stack waiter.
pub static ASYNC_CTRL_COUNT: AtomicU32 = AtomicU32::new(0);
/// Completed fire-and-forget control commands.
pub static ASYNC_CTRL_COMPLETE_COUNT: AtomicU32 = AtomicU32::new(0);
/// Fire-and-forget control responses that were not VIRTIO_GPU_RESP_OK_*.
pub static ASYNC_CTRL_RESP_ERRORS: AtomicU32 = AtomicU32::new(0);
/// Reused PASSIVE-allocated command buffers served from the bounded DMA pool.
pub static DMA_POOL_HITS: AtomicU32 = AtomicU32::new(0);
/// Command buffers that required a fresh PASSIVE allocation.
pub static DMA_POOL_MISSES: AtomicU32 = AtomicU32::new(0);
/// Completed buffers not cached because they exceeded a pool bound.
pub static DMA_POOL_DROPS: AtomicU32 = AtomicU32::new(0);
/// Current bytes retained in the DMA pool (page-rounded capacities).
pub static DMA_POOL_CACHED_BYTES: AtomicU32 = AtomicU32::new(0);

// ── Fence-event table telemetry (REGISTER_FENCE_EVENT, KMD 22.22.54) ────────
// The usermode-event replacement for blocking WAIT_FENCE escapes (PSC WS2:
// parked escapes convoy the process's SUBMIT_VENUS escapes at the dxgkrnl
// escape layer). All named per the loud-failure rule; read by QUERY_STATS v2.

/// REGISTERs parked in the table (event will be signaled by the drain).
pub static FENCE_EVENT_REGISTERS: AtomicU32 = AtomicU32::new(0);
/// Events signaled at wire-fence retirement (drain path, DISPATCH).
pub static FENCE_EVENT_SIGNALS: AtomicU32 = AtomicU32::new(0);
/// REGISTERs answered ALREADY_COMPLETE (fence had retired; signaled inline).
pub static FENCE_EVENT_ALREADY_COMPLETE: AtomicU32 = AtomicU32::new(0);
/// REGISTERs refused because the fence-event table was full.
pub static FENCE_EVENT_OVERFLOWS: AtomicU32 = AtomicU32::new(0);
/// REGISTERs refused as duplicates of a parked (fence_id, event) pair.
pub static FENCE_EVENT_DUP_REJECTS: AtomicU32 = AtomicU32::new(0);
/// REGISTER/UNREGISTER rejections: fence id never assigned / handle failed
/// ObReferenceObjectByHandle (bumped by the escape handler at PASSIVE).
pub static FENCE_EVENT_INVALID: AtomicU32 = AtomicU32::new(0);
/// UNREGISTERs that found and removed a parked registration.
pub static FENCE_EVENT_CANCELS: AtomicU32 = AtomicU32::new(0);
/// Registrations dropped UNSIGNALED at transport teardown (loud: a waiter's
/// deadline will expire and its unregister will report NOT_FOUND with an
/// unsignaled event — the ICD must treat that as failure, not completion).
pub static FENCE_EVENT_TEARDOWN_DROPS: AtomicU32 = AtomicU32::new(0);
/// WDDM fences signalled immediately because the transport had already latched
/// its ring-corruption failure. Each one also cleared the pending FIFO, so a
/// nonzero value means the driver chose a TDR-visible completion over an
/// undrainable queue.
pub static WDDM_SIGNAL_AFTER_FAILURE: AtomicU32 = AtomicU32::new(0);
/// Parked reaps abandoned between `begin_parked_reap` and `finish_parked_reap`.
/// Exists so a future strand shows up as itself rather than as a generic
/// `QUEUE_FULL_RETRIES` climb.
pub static REAP_ABANDONED: AtomicU32 = AtomicU32::new(0);
/// High-water of `fence_events.len()` since driver start.
pub static FENCE_EVENT_HIGH_WATER: AtomicU32 = AtomicU32::new(0);

// ── Registered async present-stream telemetry ───────────────────────────────
/// Successful one-time stream registrations.
pub static PRESENT_STREAM_REGISTERS: AtomicU32 = AtomicU32::new(0);
/// Tagged Venus submissions accepted onto a live stream.
pub static PRESENT_STREAM_TAGS: AtomicU32 = AtomicU32::new(0);
/// Complete nonzero marker tails observed by the KMD.
pub static PRESENT_STREAM_MARKERS: AtomicU32 = AtomicU32::new(0);
/// Terminal Venus completions that monotonically advanced a stream value.
pub static PRESENT_STREAM_RETIRES: AtomicU32 = AtomicU32::new(0);
/// Registration/tag/marker refusal or malformed-boundary count.
pub static PRESENT_STREAM_REJECTS: AtomicU32 = AtomicU32::new(0);
/// Current live stream count.  Updated only under the transport lock.
pub static PRESENT_STREAM_LIVE: AtomicU32 = AtomicU32::new(0);
/// High-water of the preallocated stream table occupancy.
pub static PRESENT_STREAM_HIGH_WATER: AtomicU32 = AtomicU32::new(0);

// ── K-F2: how far ahead of the producer a marker names (2026-08-06) ──────────
//
// ⚠ NOT A REFUSAL COUNTER. `present_stream_marker_boundary` accepts every
// marker it accepted before these existed; both are pure observation, mirrored
// as `PsMkAhd` / `PsMkAhdHi`.
//
// WHY THEY EXIST. `KMD_IMPACT.md` §14a.2 K-F2 asked for the tag path's own
// comparison — refuse unless `value <= slot.submitted_value` — as a guard
// against a guest naming a boundary that can never be satisfied. That guard is
// REFUTED, and these two counters are what makes the refutation measurable
// instead of argued: on the shipping default the marker is delivered BEFORE the
// frame's `vkQueueSubmit`, deliberately. `UmdAsyncPresentStream` is absent = ON
// (`umd/src/knobs.rs:129`), and the eligible arm then SKIPS
// `HeliosWaitFrameSubmitted` (`umd/src/forward/present.rs:1479-1528`) precisely
// because the marker is what carries the dependency; the value is minted on the
// app thread (`umd/bridge/dxvk_bridge.cpp:1316`) while the tag that advances
// `submitted_value` rides DXVK's submission thread
// (`d3d11_context_imm.cpp:1150-1162` is `EmitCs` only → `vn_queue.c:1994` →
// `:1736-1744`).
//
// ⇒ a NONZERO count is the EXPECTED steady state, and `PsMkAhdHi` should sit at
// or near 1. That is the opposite grading from every other counter in this
// block.
//
// ⭐ THE READING THAT WOULD REFUTE THE REFUTATION: `PsMkAhdHi == 0` across a
// desktop + Fire Strike run means no marker ever ran ahead — the tag reliably
// beats the marker — and the acceptance-side guard K-F2 asked for was viable
// after all. That inversion is the entire reason these are worth a deploy.
//
// ⚠ Registry values persist across boots (CLAUDE.md rule 6): verify both MOVE
// this boot before reading anything into them, and read them AFTER a desktop +
// Fire Strike run, not after a bare boot — an idle desktop presents little and
// a zero then means "nothing measured", not "nothing ahead".

/// Present markers whose `value` named producer work the stream had not yet
/// submitted (`value > slot.submitted_value`), counted at
/// `present_stream_marker_boundary` with no change to what it returns.
pub static PRESENT_STREAM_MARKER_AHEAD: AtomicU32 = AtomicU32::new(0);
/// High-water of `value - submitted_value` (saturating —
/// `helios_kmd_logic::present_stream::marker_lookahead`).
///
/// This is the number that SIZES any future bound, and the reason a bound
/// cannot be picked from an armchair: the legitimate ceiling is set by how many
/// presents DXVK can leave published-but-unsubmitted, which its own
/// `MaxNumQueuedCommandBuffers = 32` (`dxvk-helios/src/dxvk/dxvk_limits.h:17`)
/// caps — so ~32 is the predicted high-water for the D3D11 path and anything
/// far above it is a forgery rather than a frame.
pub static PRESENT_STREAM_MARKER_AHEAD_HIGH_WATER: AtomicU32 = AtomicU32::new(0);

// ── DISPATCH-safe resource-table telemetry ──────────────────────────────────
// All updated under the device spinlock (DISPATCH_LEVEL), so they must be
// atomics, never `diag::record` (RtlWriteRegistryValue is PASSIVE-only — the
// same latent-IRQL class the 2026-07-03 audit removed from the venus client).
// Read by `DxgkDdiCollectDbgInfo` and `HELIOS_ESCAPE_QUERY_STATS`.

/// High-water of `blobs.len()` since driver start.
pub static BLOB_HIGH_WATER: AtomicU32 = AtomicU32::new(0);
/// ALLOC_BLOB / note_blob_size attempts rejected because the blob table was
/// full. Nonzero = the 2026-07-03 exhaustion class is live again.
pub static BLOB_FULL_REJECTS: AtomicU32 = AtomicU32::new(0);
/// High-water of `resources.len()` since driver start.
pub static RESOURCE_HIGH_WATER: AtomicU32 = AtomicU32::new(0);
/// resource_create_blob attempts rejected because the live-resource table was full.
pub static RESOURCE_FULL_REJECTS: AtomicU32 = AtomicU32::new(0);
/// Context tracking slots dropped because the context table was full.
pub static CONTEXT_FULL_DROPS: AtomicU32 = AtomicU32::new(0);
/// Freed window ranges dropped because the free list was full (leaked offsets).
pub static WINDOW_RANGE_DROPS: AtomicU32 = AtomicU32::new(0);
/// `configure_window_reserve` refusals — offsets had already been issued. Must
/// stay 0: a nonzero value means someone tried to move the VidMm partition out
/// from under live mappings.
pub static WINDOW_RECONFIG_REFUSED: AtomicU32 = AtomicU32::new(0);
/// `take_live_resource` misses (duplicate-teardown suppressions). Replaces the
/// in-lock `diag::record(0x0D20_00E0)` breadcrumb.
pub static TAKE_LIVE_MISSES: AtomicU32 = AtomicU32::new(0);
/// `adopt_blob_for_allocation` rejections of a dead resource id. Replaces the
/// in-lock `diag::record(0x0D20_00E2)` breadcrumb.
pub static ADOPT_DEAD_REJECTS: AtomicU32 = AtomicU32::new(0);
/// `alloc_window_range` refusals (host-visible window offset space exhausted /
/// fragmented past the request). Each one fails a user MAP_BLOB with
/// STATUS_INSUFFICIENT_RESOURCES — loud-failure rule (2026-07-06 Doom triage:
/// three uncounted refusal sites all mapped to the same 0xC000009A).
pub static WINDOW_ALLOC_REJECTS: AtomicU32 = AtomicU32::new(0);
/// `map_io_pages_to_user` failures (MDL alloc / MmMapLockedPagesSpecifyCache
/// raise caught by the SEH shim). Bumped by the escape handler at PASSIVE.
pub static MAP_PAGES_FAILS: AtomicU32 = AtomicU32::new(0);
/// Stale-overlap scans that found more overlapping window placements than the
/// caller's fixed buffer could hold. Nonzero means an eviction pass ran against
/// an incomplete list and the map that followed it was REFUSED rather than
/// allowed to create an overlapping host window subregion.
pub static WINDOW_OVERLAP_TRUNCATED: AtomicU32 = AtomicU32::new(0);

/// The stale-overlap scan could not report every overlapping placement.
///
/// A distinct type rather than a `usize` the caller may ignore: acting on a
/// truncated list is what creates two host resources through one window
/// subregion.
#[derive(Clone, Copy, Debug)]
pub struct WindowOverlapTruncated;

/// Raise `hw` to at least `n` (relaxed; approximate under concurrency is fine
/// for telemetry).
pub fn bump_high_water(hw: &AtomicU32, n: usize) {
    let n = n as u32;
    if hw.load(Ordering::Relaxed) < n {
        hw.store(n, Ordering::Relaxed);
    }
}

/// Table capacity accessors for `HELIOS_ESCAPE_QUERY_STATS` (the consts are
/// module-private).
pub fn max_blobs() -> usize {
    MAX_BLOBS
}
pub fn max_resources() -> usize {
    MAX_RESOURCES
}
