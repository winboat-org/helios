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
/// Fenced SUBMIT_3D enqueues carrying ring_idx >= 1 (GPU-completion fences —
/// WS1 #4 consumer-side ordering; these retire at host GPU completion, not
/// decode, so they legally stay in flight for the full GPU-work duration).
pub static RING_SUBMIT_COUNT: AtomicU32 = AtomicU32::new(0);
/// ring_idx >= 1 completions drained from the used ring.
pub static RING_COMPLETE_COUNT: AtomicU32 = AtomicU32::new(0);
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
