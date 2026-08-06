//! The virtio-gpu device object, built on the `virtio-drivers` PCI transport.
//!
//! `VirtioGpu` owns the `PciTransport` (discovers/maps the virtio config
//! regions) and the control `VirtQueue`, and layers the virtio-gpu command
//! protocol (`helios_protocol`) on top. Built by `init` from
//! `DxgkDdiStartDevice` and stored in `AdapterContext::virtio`.
//!
//! Bring-up (all in `init`, at PASSIVE_LEVEL):
//!   M1 — `DxgkConfigAccess` → `PciRoot` → `PciTransport::new::<WdkHal,_>`
//!   M2 — feature negotiation via the `Transport` trait
//!   M3 — control `VirtQueue::<WdkHal>` setup + DRIVER_OK
//!   M4 — `GET_DISPLAY_INFO` polled round-trip (Phase-2 smoke test)
//!
//! ## C3/M3.4 async transport (2026-07-04)
//!
//! Every control-queue command is a tracked [`InFlight`] entry that OWNS its
//! device-visible DMA buffers until the device returns its descriptor chain on
//! the used ring ([`VirtioGpu::drain_used`], token-matched `peek_used` →
//! `pop_used` — ported from the proven System-class phase4e model in
//! `kmd/src/virtio/gpu.rs`). Nothing in this module ever waits:
//!
//!   * Fenced `SUBMIT_3D` is ASYNC ([`VirtioGpu::enqueue_async_submit`]): the
//!     KMD assigns a globally-monotonic WIRE fence id, queues the descriptors,
//!     notifies, and returns. Completion signals any registered
//!     [`FenceWaiter`] (KEVENT) and advances the WDDM pending FIFO.
//!   * Synchronous verbs (ctx/blob/map) are enqueued with an optional
//!     [`SyncWaitBlock`] waiter ([`VirtioGpu::enqueue_sync`]); the waiter
//!     blocks at PASSIVE_LEVEL in `virtio::ctrl`, NEVER at DISPATCH under the
//!     device spinlock (the 2026-07-04 Escape-convoy root cause).
//!   * Completed entries are parked (their `DmaBuffer`s are PASSIVE-only to
//!     free) and reaped by PASSIVE callers via [`VirtioGpu::begin_parked_reap`].
//!
//! The used-ring consumer is [`VirtioGpu::drain_used`], called from the
//! interrupt DPC (`ddi/interrupt.rs`) and opportunistically (under the same
//! spinlock) by enqueue paths and by `virtio::ctrl`'s wait slices, so waits
//! survive a lost interrupt with only slice-granularity latency.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use bytemuck::Zeroable;
use helios_kmd_logic::scanout_read_ledger::LedgerTicket;
use helios_kmd_logic::scanout_refresh::{Marker as ScanoutRefreshMarker, State as RefreshState};
use helios_protocol::{
    HELIOS_OPTIONAL_FEATURES, HELIOS_REQUIRED_FEATURES, VIRTIO_GPU_CMD_GET_DISPLAY_INFO,
    VIRTIO_GPU_CMD_SUBMIT_3D, VIRTIO_GPU_FLAG_FENCE, VIRTIO_GPU_FLAG_INFO_RING_IDX,
    VirtioGpuCmdSubmit, VirtioGpuCtrlHdr, VirtioGpuRespDisplayInfo, VirtioGpuSetScanoutBlob,
    resp_is_ok,
};
use virtio_drivers::queue::VirtQueue;
use virtio_drivers::transport::pci::PciTransport;
use virtio_drivers::transport::pci::bus::{DeviceFunction, PciRoot};
use virtio_drivers::transport::{DeviceStatus, Transport};
use wdk_sys::ntddk::{
    KeInitializeEvent, KeQueryInterruptTimePrecise, KeSetEvent, ObDereferenceObjectDeferDelete,
};
use wdk_sys::{KEVENT, PVOID};

mod resource_tables;

use super::config::DxgkConfigAccess;
use super::hal::{DmaBuffer, DmaSpan, WdkHal};
use super::pci_caps::{HostVisibleWindow, map_isr_status_register, scan_host_visible_window};

// R1103: the telemetry atomics moved to `super::counters`. Re-exported here so
// all 53+ external `gpu::<COUNTER>` paths keep compiling unchanged; narrowing
// the re-export is a follow-up, not part of the move.
use super::VirtioError;
pub use super::counters::*;
use crate::dxgk::DXGKRNL_INTERFACE;
use crate::virtio::venus::{
    OptimalPresentImageDesc, PreparedPresentBltSubmission, PresentDestinationDesc,
};

/// Control queue index (virtio-gpu controlq = 0; cursorq = 1 is unused).
const CTRL_QUEUE: u16 = 0;
/// Control-queue ring size — power of two, conservatively ≤ the device's max.
const CTRL_QUEUE_SIZE: usize = 64;
/// One page of contiguous DMA scratch for `init`'s inline polled round-trip.
const SCRATCH_BYTES: usize = 4096;
/// Busy-poll bound for `init`'s inline GET_DISPLAY_INFO round-trip — the ONLY
/// polled wait left (PASSIVE, pre-interrupt, single-threaded bring-up; every
/// runtime wait is a PASSIVE KEVENT wait in `virtio::ctrl`). Each iteration is
/// a volatile used-ring read + `spin_loop` (~10 ns) → bound ≈ 1 s.
const CTRL_POLL_SPINS: u64 = 100_000_000;

/// Windowed Present requests are fixed-capacity because they retain a KMD read
/// lease until host completion. Exceeding this is a loud Present refusal, never
/// an untracked copy whose snapshot slot DXVK may overwrite.
const MAX_WINDOWED_BLT_PENDING: usize = 64;

// The READ LEDGER has exactly one slot for every admissible WindowedBlt reader
// plus the globally serialized direct flush reader. Keep this assertion at the
// bound that governs the WindowedBlt issuer, not as a distant sizing comment.
const _: () = {
    assert!(helios_protocol::HELIOS_READ_LEDGER_SLOTS == MAX_WINDOWED_BLT_PENDING + 1);
};

// ── Host-visible blob mapping (Gate 5a Stage 2b, venus-over-Escape) ──────────
// Ported (synchronous variant) from the proven System-class `kmd/src/virtio/gpu.rs`.
// The venus ICD allocates HOST3D blobs (ALLOC_BLOB) and maps them into its address
// space (MAP_BLOB) over `DxgkDdiEscape`; the KMD picks a window offset, issues
// `RESOURCE_MAP_BLOB`, and the Escape handler maps `host_visible.base + offset`
// into the calling process with `MmMapLockedPagesSpecifyCache` — the zero-copy BAR
// model (no WDDM memory segment / GpuMmu; see GATE5_STAGE2_ALLOC_DESIGN.md).

/// Page granularity for blob window offsets/sizes.
const BLOB_PAGE: u64 = 4096;
/// Max concurrently-tracked blobs.
///
/// SIZING (2026-07-03 exhaustion incident): a live desktop legitimately holds
/// hundreds of blobs at once — every venus ring/reply/fence shmem of every
/// process, every host-visible DXVK memory chunk, every exported/shared
/// surface, and every KMD-standard GDI redirection surface is one slot. The
/// old cap of 256 filled after ~2 h of desktop churn, at which point every
/// new venus consumer failed guest-side (`vkCreateInstance` →
/// VK_ERROR_OUT_OF_HOST_MEMORY for new processes; dwm lost its device → no
/// IddCx swapchain offers). 8192 slots ≈ 448 KiB of non-paged pool, reserved
/// once at init. Exhaustion is now counted (`BLOB_FULL_REJECTS`) and visible
/// via `HELIOS_ESCAPE_QUERY_STATS`; hitting the new cap indicates a leak, not
/// a workload.
pub(crate) const MAX_BLOBS: usize = 8192;
/// Max live virtio resources. This covers both escape blobs and KMD/WDDM standard
/// allocations, so teardown can suppress duplicate RESOURCE_UNREF commands. Must
/// be ≥ MAX_BLOBS (every blob is a live resource; non-blob resources add more).
pub(super) const MAX_RESOURCES: usize = 16384;
/// Max concurrently-tracked virtio-gpu contexts (one per live device, generous).
const MAX_CONTEXTS: usize = 1024;
/// Max coalescing free ranges in the window allocator's free list. Overflow
/// drops the freed range (leaks window offset space) — counted in
/// `WINDOW_RANGE_DROPS`.
const MAX_WINDOW_RANGES: usize = 1024;
/// Per-map size cap (also bounds the `IoAllocateMdl` ULONG length on the caller).
const MAX_BLOB_MAP_BYTES: u64 = 256 << 20;

// Rounds `n` up to the next `BLOB_PAGE` multiple (saturating). The body moved to
// `helios_kmd_logic` (host unit-tested, no `wdk-sys` edge); `BLOB_PAGE` stays
// here because the window allocator still uses it directly at `map_blob_prepare`.
use helios_kmd_logic::round_up_page;

/// Result of the under-lock phase of MAP_BLOB ([`VirtioGpu::map_blob_prepare`]): the
/// guest-physical range to map and the host's requested caching. The user-space
/// mapping (MDL + `MmMapLockedPagesSpecifyCache`) is built by the Escape handler at
/// PASSIVE_LEVEL, OUTSIDE the virtio spinlock.
#[derive(Clone, Copy)]
pub struct BlobMapPrep {
    /// Guest-physical base of the resource's mapping inside the host-visible window.
    pub gpa: u64,
    /// Page-rounded length to map, in bytes.
    pub size: u64,
    /// Host caching nibble (`VIRTIO_GPU_MAP_CACHE_*`) from `RESP_OK_MAP_INFO`.
    pub map_cache: u32,
}

/// One tracked blob resource.
/// A device that can own escape-tracked state: dxgkrnl's `DXGKARG_ESCAPE.hDevice`
/// for the escaping device, proven non-null.
///
/// The guest and kernel ownership domains used to share one `usize`
/// representation, and 0 belonged to BOTH: it was a legal (forgeable) escape
/// handle value AND the kernel's "KMD-owned, invisible to escape reclaim"
/// sentinel. `NonZeroUsize` separates them — `Option<DeviceOwner>` costs no
/// extra bytes (niche-optimised), `None` means KMD-owned, and an escape verb
/// that takes a `DeviceOwner` cannot be handed the kernel sentinel at all
/// (k-capsescape-01).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DeviceOwner(core::num::NonZeroUsize);

impl DeviceOwner {
    /// `None` for a null handle — the caller must refuse rather than substitute.
    pub fn new(raw: usize) -> Option<Self> {
        core::num::NonZeroUsize::new(raw).map(Self)
    }

    /// The opaque handle value, for tables that are not owner-typed (the
    /// user-mapping table) and for diagnostics.
    pub fn raw(self) -> usize {
        self.0.get()
    }
}

/// A context id resolved through the tracking table against its owner.
///
/// The wire-facing context calls take this rather than a raw `u32`, so a
/// guest-supplied id cannot reach the host without the table lookup. This is a
/// real guarantee only because context tracking is mandatory
/// (`VirtioGpu::reserve_context_slot`): with best-effort tracking the resolver
/// would have to fall back to trusting unknown ids, and the type would merely
/// relocate the check.
#[derive(Clone, Copy)]
pub struct OwnedCtx {
    id: u32,
}

impl OwnedCtx {
    /// The wire context id. Only reachable from a successful table lookup.
    pub fn id(self) -> u32 {
        self.id
    }
}

/// Which slots a blob lookup may match.
///
/// The two "no owner" concepts are deliberately NOT the same value: `Any` is a
/// wildcard used by kernel paths that resolve by resource id alone (the paging
/// engine, the Present blit, the scan-out diagnostics), while
/// `Exactly(None)` means specifically the KMD-owned slots. Collapsing them
/// would either break the kernel lookups or hand escapes a wildcard.
#[derive(Clone, Copy)]
pub enum OwnerFilter {
    Any,
    Exactly(Option<DeviceOwner>),
}

#[derive(Clone, Copy)]
struct BlobSlot {
    /// The owner that allocated this blob: `Some(device)` for an escape-owned
    /// blob, `None` for KMD-owned (venus infrastructure, or a blob adopted by a
    /// WDDM allocation). `DxgkDdiDestroyDevice` reclaims every
    /// blob tagged with the destroyed handle, so a crashing/forgetful ICD (e.g.
    /// the crash-looping LogonUI, or any process that skips RELEASE_BLOB) cannot
    /// leak the bounded blob table (`MAX_BLOBS`) and false-trip later allocations
    /// with `STATUS_INSUFFICIENT_RESOURCES`.
    owner: Option<DeviceOwner>,
    ctx_id: u32,
    resource_id: u32,
    /// Blob size in bytes (from ALLOC_BLOB; MAP_BLOB needs it to size the MDL).
    size: u64,
    /// RESOURCE_MAP_BLOB succeeded and must be paired with RESOURCE_UNMAP_BLOB.
    mapped: bool,
    /// A RESOURCE_MAP_BLOB round-trip is in flight for this slot (the window
    /// range is reserved; concurrent mappers must wait — see `blob_map_begin`).
    map_pending: bool,
    /// Host caching nibble from RESP_OK_MAP_INFO (valid once `mapped`).
    map_cache: u32,
    /// Host-visible window offset used for RESOURCE_MAP_BLOB.
    map_offset: u64,
    /// Rounded mapped length in the host-visible window.
    map_len: u64,
}

/// A free span in the host-visible window's offset space (bump + coalescing free).
#[derive(Clone, Copy)]
struct WindowRange {
    offset: u64,
    len: u64,
}

/// Point-in-time occupancy snapshot of the bounded tables (see
/// [`VirtioGpu::table_stats`]); consumed by `HELIOS_ESCAPE_QUERY_STATS`.
#[derive(Clone, Copy)]
pub struct TableStats {
    pub blobs_live: u32,
    pub resources_live: u32,
    pub contexts_live: u32,
    /// Bytes allocated in the window offset space (bump high-water minus free list).
    pub window_used: u64,
    /// Total window length (0 if the device exposes no host-visible window).
    pub window_len: u64,
}

/// One tracked virtio-gpu context, tagged with the owning device handle for
/// device-teardown reclamation.
#[derive(Clone, Copy)]
struct ContextSlot {
    /// Owning device, or `None` for the KMD's own persistent venus context.
    owner: Option<DeviceOwner>,
    ctx_id: u32,
}

// ── C3/M3.4 async submission machinery ───────────────────────────────────────

/// Max in-flight control-queue entries. Tokens are descriptor-chain heads, so
/// they are always `< CTRL_QUEUE_SIZE`; each chain uses ≥ 2 descriptors, which
/// caps real concurrency at half this.
pub const MAX_INFLIGHT: usize = CTRL_QUEUE_SIZE;
/// Parked (completed, awaiting PASSIVE free) entry capacity. Enqueues are
/// refused once `parked` crosses [`PARKED_ENQUEUE_GATE`], and one drain can
/// park at most `MAX_INFLIGHT` entries, so this bound is never exceeded.
pub const MAX_PARKED: usize = 4 * MAX_INFLIGHT;
/// Completed command buffers retained for reuse. Count, individual capacity,
/// and total bytes are all bounded: a rare large Venus CS must never pin a
/// correspondingly large physically-contiguous allocation for device lifetime.
const MAX_DMA_POOL: usize = 128;
const MAX_DMA_POOL_BUFFER_BYTES: usize = 64 * 1024;
const MAX_DMA_POOL_BYTES: usize = 2 * 1024 * 1024;
/// Enqueue refusal threshold for the parked table (forces the PASSIVE caller
/// to reap before submitting more).
const PARKED_ENQUEUE_GATE: usize = MAX_PARKED - MAX_INFLIGHT;
/// Max concurrent WAIT_FENCE waiters.
const MAX_FENCE_WAITERS: usize = 64;
/// Max parked fence-event registrations (REGISTER_FENCE_EVENT). Sized for
/// every venus process (dwm + apps + WUDFHost) to park several waits each
/// (retire thread + app fence waits + flip/acquire gates); overflow is
/// counted and the ICD falls back to the blocking-escape wait.
const MAX_FENCE_EVENTS: usize = 256;
/// Registered UMD present streams. The table is allocated once at transport
/// initialization; every register/tag/retire lookup thereafter runs under the
/// transport spinlock without allocating at DISPATCH.
/// ⚠ ALIASES, NOT DEFINITIONS since 2026-08-06. The tagged-boundary and packed-
/// handle ABI now lives in `helios_kmd_logic::present_stream`, because the six
/// tests that cover it could never run inside this `panic = "abort"` cdylib. Two
/// copies of these numbers would let the tested rules and the shipped ones drift.
/// (`INDEX_BITS` has no alias here on purpose: after the delegation nothing in
/// this crate packs or unpacks a handle by hand, and an unused alias is a warning.)
const MAX_PRESENT_STREAMS: usize = helios_kmd_logic::present_stream::MAX_STREAMS;
const PRESENT_STREAM_GENERATION_MAX: u32 = helios_kmd_logic::present_stream::GENERATION_MAX;
/// Bit 63 distinguishes this from the legacy exclusive wire-fence namespace.
pub const PRESENT_STREAM_BOUNDARY_TAG: u64 = helios_kmd_logic::present_stream::BOUNDARY_TAG;
/// Max WDDM submissions pending on venus completion.
const MAX_WDDM_PENDING: usize = 256;
/// Ceiling the `WddmHoldMs` knob is clamped to, IN CODE.
///
/// The hold delays the head of an adapter-global, strictly head-of-line FIFO, so
/// an operator typo (`WddmHoldMs=100000`) must not be able to produce a TDR. 250
/// ms is an eighth of the default `TdrDelay` (2 s) and still five orders of
/// magnitude above the 0.8–1.1 µs `WaitForSingleObject` baseline the experiment
/// reads against — the reading needs no more range than that, and the
/// `MAX_WDDM_PENDING` overflow escape remains the second line of defence.
const WDDM_HOLD_MS_MAX: u32 = 250;
/// `WddmHoldMs`, snapshotted once at transport init. 0 = off (the default), which
/// is the only value the shipping driver runs with.
///
/// ONE read site for this knob, deliberately: `dma_gpu_fence`'s doc records that
/// a second, unread copy of a registry value existed here and was deleted so the
/// two could not disagree. A static rather than a `VirtioGpu` field because the
/// 60 Hz heartbeat in `adapter/kobj.rs` must test it WITHOUT taking `virtio_lock`
/// — it is that heartbeat that provides the hold's release edge.
pub static WDDM_HOLD_MS: AtomicU32 = AtomicU32::new(0);
/// `WddmHeadMs`: how long the head of the WDDM FIFO may stay blocked on a
/// TAGGED-NAMESPACE dependency before that dependency is rebased onto the
/// conservative wire watermark. Snapshotted once at transport init and CLAMPED —
/// see [`WDDM_HEAD_MS_MIN`] / [`WDDM_HEAD_MS_MAX`].
///
/// A static, not a `VirtioGpu` field, for the same reason as [`WDDM_HOLD_MS`]: one
/// read site, and readable without `virtio_lock`.
pub static WDDM_HEAD_MS: AtomicU32 = AtomicU32::new(WDDM_HEAD_MS_DEFAULT);
/// `WddmHeadMs`'s shipping default, in ms, and it is a DECISION (CLAUDE.md rule 8).
///
/// The bound exists because `present_stream_marker_boundary` accepts any nonzero
/// marker value and bounds it in no way, so a guest can name a boundary
/// `present_stream_slot_ready` will never satisfy — and `wddm_pending` is
/// adapter-global and strictly head-of-line, so that blocks EVERY context, DWM's
/// presents included, until either the 256-entry overflow drops 256 fences on the
/// floor or dxgkrnl TDRs the adapter. `KMD_IMPACT.md` §14a.2 K-F2, which also
/// records why the acceptance-side guard (`value <= submitted_value`) is refuted:
/// on the shipping default the marker is delivered BEFORE the frame's
/// `vkQueueSubmit` deliberately, so that test would refuse every legitimate frame.
///
/// 250 ms is picked, not tuned: an eighth of the default `TdrDelay` (2 s), so
/// several rebases fit inside one TDR window, and ~68x the measured 3.7 ms/frame
/// producer floor, so no legitimate frame's producer can reach it without the
/// desktop having visibly frozen already.
///
/// 0 is the same-boot A/B disable and restores the historical unbounded head
/// exactly — which is what makes the rebase's cost measurable rather than argued.
const WDDM_HEAD_MS_DEFAULT: u32 = 250;
/// Ceiling for `WddmHeadMs`, IN CODE. An operator typo (`WddmHeadMs=100000`) must
/// not be able to reinstate the unbounded head and hence the TDR; 1 s is half the
/// default `TdrDelay`, which is the largest bound that can still act before one.
const WDDM_HEAD_MS_MAX: u32 = 1000;
/// FLOOR for a nonzero `WddmHeadMs`, IN CODE, and it protects correctness rather
/// than liveness: a rebase RELEASES a fence whose named producer has not completed,
/// which is the 0ab-B stale/black-frame class. A 1 ms bound would rebase healthy
/// frames continuously. 100 ms is ~27 frames at the measured producer floor.
const WDDM_HEAD_MS_MIN: u32 = 100;
/// Max response bytes a synchronous command may expect (copied into the
/// waiter's [`SyncWaitBlock`]; the largest runtime response is
/// `VirtioGpuRespMapInfo`. `init`'s big GET_DISPLAY_INFO reply stays on the
/// inline polled path and does not ride this machinery).
pub const SYNC_RESP_MAX: usize = 64;
/// Bytes for an async submit's metadata buffer: the device-read SUBMIT_3D
/// header followed by the device-written ctrl response.
pub const SUBMIT_META_BYTES: usize =
    core::mem::size_of::<VirtioGpuCmdSubmit>() + core::mem::size_of::<VirtioGpuCtrlHdr>();
/// Bytes for one DISPATCH-level fast-bind command buffer: the device-read
/// `SET_SCANOUT_BLOB` followed by the device-written ctrl response (ROADMAP
/// defect 0ab-C, D1(ii)).
pub const BIND_CMD_BYTES: usize =
    core::mem::size_of::<VirtioGpuSetScanoutBlob>() + core::mem::size_of::<VirtioGpuCtrlHdr>();
/// Fast-bind command buffers held in the pool.
///
/// A SINGLE buffer was the coverage bottleneck (22.22.220.0, measured): the
/// buffer only returns to the driver at the guest DPC drain, which lags the
/// host's consume by several flip periods under load, so ~18 % of flips found it
/// in flight and fell back to the worker's late bind (`FpBusy` 1776/run against
/// `FpBind` +98 once the skip predicate stopped hiding them). Four covers that
/// lag with the same "one buffer per outstanding command" ownership rule — no
/// sharing, no reuse-before-completion.
///
/// It is BOTH the reserved capacity and the refill bound: `init` fills to it and
/// every return site checks `len` against it, so the pool can never grow past
/// the capacity reserved at PASSIVE and therefore never reallocates under the
/// device spinlock.
const BIND_CMD_POOL: usize = 4;

/// `NotificationEvent` (`EVENT_TYPE` value 0): stays signaled until cleared —
/// the right semantics for one-shot completion events.
const NOTIFICATION_EVENT: i32 = 0;
/// `IO_NO_INCREMENT` priority boost for `KeSetEvent`.
const IO_NO_INCREMENT: i32 = 0;

/// A PASSIVE waiter's completion block. Lives on the waiter's stack; the
/// registered pointer stays valid because the waiter ALWAYS deregisters (or
/// observes completion) under the device spinlock before returning.
pub struct SyncWaitBlock {
    /// Signaled (under the device spinlock) when the entry completes.
    pub event: KEVENT,
    /// Set (Release) before the event is signaled; the waiter reads it
    /// (Acquire) after the wait / under the lock.
    done: AtomicBool,
    /// The device-written response bytes, copied out of the entry's DMA buffer
    /// by `drain_used` before the event is signaled.
    resp: UnsafeCell<[u8; SYNC_RESP_MAX]>,
}

/// Stable adapter-owned notification target for a scanout copy submitted on a
/// GPU-completion ring. The used-ring drain sets `pending` and wakes `event`
/// only after a successful ring-1 SUBMIT_3D completion.
///
/// Fields are PRIVATE and there is exactly one constructor,
/// [`Self::for_adapter`], which derives all four pointers from a single
/// `&AdapterContext`. That is what makes "the four pointers always come from the
/// same adapter" structural rather than assembled field by field.
///
/// It can only be attached to a submission through
/// [`VirtioGpu::enqueue_scanout_submit`], which hard-codes ring 1 — the ring the
/// drain actually honours. A notify on any other ring used to be silently
/// discarded at completion with no counter, and because the drain is the ONLY
/// clear of `vidpn_programming` on the copied-primary path, that stranded the
/// pending Windows primary and its programming ownership indefinitely.
///
/// The four `NonNull`s' validity rests on the adapter outliving the transport,
/// which StopDevice enforces by ordering (`set_virtio(None)` after cancel/join),
/// and on `init_kernel_events` having run before any `KeSetEvent` on `hpd_event`.
/// Neither is encodable — the self-referential lifetime (`VirtioGpu` lives inside
/// the `AdapterContext` it points back into) is what defeats it — so both are
/// documented HERE, once, instead of on four fields.
/// The virtio ring whose used-ring completion represents real host GPU
/// completion, and therefore the only one on which a [`ScanoutNotify`] may be
/// honoured. Ring 0 retires at host DECODE, which is too early to publish pixels.
pub(crate) const SCANOUT_RING_IDX: u32 = 1;

#[derive(Clone, Copy)]
pub struct ScanoutNotify {
    pending: NonNull<AtomicU32>,
    /// Address of the Windows primary whose compatibility copy this submission
    /// performs. Published as displayed only on successful GPU completion.
    displayed_primary: NonNull<AtomicU64>,
    /// Exact SetVidPnSourceAddress programming gate, packed `(seq << 32) | active`.
    /// Cleared after the copy succeeds or fails so VSync can resume with
    /// authoritative state — but only for THIS submission's interval, named by
    /// `ticket`.
    programming: NonNull<AtomicU64>,
    /// The programming generation this submission belongs to, carried BY VALUE
    /// rather than only as a pointer to the gate. A completion that arrives
    /// after a newer interval was raised fails its compare-exchange and counts
    /// `ScStale` instead of clearing a gate that is not its own.
    ticket: crate::adapter::ProgrammingTicket,
    primary_address: u64,
    event: NonNull<KEVENT>,
}

/// Typed completion token for one scan-out `RESOURCE_FLUSH` (ROADMAP defect
/// 0ab-B).
///
/// The generic `AsyncControl` completion plumbing —
/// `completion`/`completion_errors`/`wake_event`/`success_store` — can say THAT
/// a control command finished; it cannot say WHICH presentation the host read
/// while finishing it. That identity is the whole content of the ownership
/// invariant, so it travels as its own value: `covers_epoch` is the presentation
/// epoch the host was bound to when this exact command was enqueued, and a
/// returned response therefore proves the host has read that buffer.
///
/// Carrying the adapter pointer (rather than a `NonNull<AtomicU64>` to the
/// watermark alone) is deliberate: ending a lease also publishes any withheld
/// primary address and wakes the worker that can pop the WDDM pending FIFO, and
/// splitting those across three raw pointers is exactly the class of mistake
/// `ScanoutNotify`'s four-pointer doc paragraph was written about.
///
/// Validity rests on the same fact `ScanoutNotify` documents: the adapter
/// outlives the transport, enforced by ordering in StopDevice (`set_virtio(None)`
/// after cancel/join), and every field this touches is an atomic on the shared
/// `&AdapterContext` that every DDI, ISR and DPC already holds — no `&mut` to it
/// exists anywhere in the driver.
pub struct ScanoutFlushToken {
    adapter: NonNull<crate::adapter::AdapterContext>,
    covers_epoch: u64,
    trace_id: u64,
    /// The venus resource this exact read names — the D4a ledger identity
    /// (FIX-DESIGN-d4a.md §3.2). `covers_epoch` cannot stand in for it: the
    /// MMIO/desktop path reads with `NO_LEASE` epochs, and those reads must
    /// still retire in the ledger.
    resource_id: u32,
    /// Exact generation-qualified ledger claim for this read. An unledgered
    /// direct flush carries `LedgerTicket::NONE`; WindowedBlt refuses one.
    ledger_ticket: LedgerTicket,
    /// `complete` ran; `Drop` must not retire a second time. A plain field,
    /// not an atomic: the token is single-owner and never `Copy`.
    done: bool,
}

static NEXT_SCANOUT_FLUSH_ID: AtomicU64 = AtomicU64::new(0);

impl ScanoutFlushToken {
    /// The ONE construction site, called from
    /// `queue_active_scanout_refresh_locked` with the epoch it snapshotted and
    /// the ledger slot its issue bump claimed.
    pub(crate) fn new(
        adapter: &crate::adapter::AdapterContext,
        covers_epoch: u64,
        resource_id: u32,
        ledger_ticket: LedgerTicket,
    ) -> Self {
        Self {
            adapter: NonNull::from(adapter),
            covers_epoch,
            trace_id: NEXT_SCANOUT_FLUSH_ID
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1),
            resource_id,
            ledger_ticket,
            done: false,
        }
    }

    #[inline]
    pub(crate) fn trace_context(&self) -> (u64, u64, u32) {
        (self.trace_id, self.covers_epoch, self.resource_id)
    }

    /// The ONE ledger-retirement site (`ledger_retire`, §3.2): every
    /// constructed token reaches it exactly once — from [`Self::complete`]
    /// (host OK and host-error arms) or, if `complete` never ran, from `Drop`
    /// (enqueue failure, in-flight table teardown).
    ///
    /// Legal at PASSIVE and at DISPATCH under `virtio_lock`: atomics, the leaf
    /// event lock, `KeSetEvent(Wait = FALSE)` — no allocation, no registry
    /// write, never `wddm_notify_lock`.
    fn retire_ledger(&mut self, via_drop: bool) {
        if self.done {
            return;
        }
        self.done = true;
        // SAFETY: per the type's doc — the adapter outlives every in-flight
        // transport entry, and the call touches only atomics + the leaf lock.
        unsafe { self.adapter.as_ref() }.read_ledger.retire(
            self.ledger_ticket,
            via_drop,
        );
    }

    /// Consume the token when the control response has been drained.
    ///
    /// `ok` is the `VIRTIO_GPU_RESP_*` verdict. A host ERROR still ends the
    /// lease, and that is not a loophole: the command has terminated, so no
    /// future read can originate from it. It is counted as `Cancelled` rather
    /// than `HostRead` precisely so an error storm cannot masquerade as healthy
    /// publication.
    ///
    /// Runs at DISPATCH_LEVEL inside the used-ring drain, holding `virtio_lock`.
    /// It must therefore NOT take `wddm_notify_lock` — the driver's order is the
    /// reverse — which is why every operation it reaches is a monotone atomic.
    pub(crate) fn complete(mut self, ok: bool) {
        crate::ddi::scanout_timeline::note(
            crate::ddi::scanout_timeline::kind::FLUSH_COMPLETE,
            if ok {
                crate::ddi::scanout_timeline::flag::SUCCESS
            } else {
                0
            },
            self.covers_epoch,
            0,
            self.trace_id,
            self.resource_id,
            0,
        );
        // Ledger first, BEFORE the `NO_LEASE` early return below: the desktop
        // path's reads carry no epoch but are still host readbacks the ledger
        // issued, and the D4a invariant is one retirement per issue, no
        // exceptions. (`self.done` then makes the implicit `Drop` at this
        // function's exit a no-op.)
        self.retire_ledger(false);
        crate::ddi::scanout_trace::note_lease_read_done();
        if self.covers_epoch == helios_kmd_logic::scanout_lease::NO_LEASE {
            // Nothing was bound with an epoch when this flush was issued — the
            // MMIO/`FlipOnVSyncMmIo` desktop path, which mints no presentations
            // at all. That is not a stale token, it is a read with nothing to
            // cover, and counting it as one made `LsStal` read 389 of 389 on an
            // idle desktop, i.e. useless for the thing it exists to detect.
            return;
        }
        let reason = if ok {
            crate::ddi::scanout_trace::LeaseEnd::HostRead
        } else {
            crate::ddi::scanout_trace::LeaseEnd::Cancelled
        };
        // SAFETY: per the type's doc — the adapter outlives every in-flight
        // transport entry, and the call touches only its atomics.
        let advanced =
            unsafe { self.adapter.as_ref() }.end_scanout_leases_through(self.covers_epoch, reason);
        if !advanced {
            // The token was at or behind the watermark: a coalesced, duplicated
            // or reordered read. Inert by construction — counted so that
            // "inert" is a measurement rather than an assumption, because a
            // large `LsStal` means the flush path is spending host readbacks on
            // presentations nobody is waiting for.
            crate::ddi::scanout_trace::note_lease_stale();
        }
    }
}

impl Drop for ScanoutFlushToken {
    /// The backstop that makes the D4a liveness matrix rows 4 and 5 true BY
    /// TYPE: a token dropped without `complete` — the enqueue-failure arm
    /// (`resource_flush_async` / `enqueue_async_control` error paths, some at
    /// DISPATCH under `virtio_lock`) and the in-flight table dying with its
    /// `VirtioGpu` (PASSIVE, `set_virtio(None)`) — still retires its ledger
    /// issue, counted `RdDrp`.
    ///
    /// Deliberately NOT the lease-end site: the enqueue-failure caller ends
    /// exactly the epochs it named (`end_scanout_leases_through`, counted
    /// `LsCanc`) and `latch_failed_and_fail_inflight` completes in-flight
    /// tokens explicitly — duplicating that here would double-count the `Ls*`
    /// census. The ledger is the one obligation only the token can discharge.
    fn drop(&mut self) {
        self.retire_ledger(true);
    }
}

impl ScanoutNotify {
    /// The ONE construction site. Reached through
    /// `AdapterContext::scanout_notify`.
    pub(crate) fn for_adapter(
        adapter: &crate::adapter::AdapterContext,
        primary_address: u64,
        ticket: crate::adapter::ProgrammingTicket,
    ) -> Self {
        Self {
            pending: NonNull::from(&adapter.scanout_refresh_pending),
            displayed_primary: NonNull::from(&adapter.last_primary_address),
            programming: NonNull::from(&adapter.vidpn_programming),
            ticket,
            primary_address,
            // SAFETY: hpd_event is embedded in the stable adapter and
            // initialized by init_kernel_events before StartDevice creates any
            // Venus submissions.
            event: unsafe { NonNull::new_unchecked(adapter.hpd_event.get()) },
        }
    }
}

impl SyncWaitBlock {
    /// Run `f` with a wait block that is zeroed and initialised in place on
    /// THIS frame and is never nameable by the caller.
    ///
    /// The three invariants — initialised before registration, never moved
    /// after, always deregistered before the frame dies — used to be carried by
    /// comments over a `new_zeroed()` -> `unsafe { init() }` ->
    /// `NonNull::from(&mut block)` dance at two call sites. A KEVENT dispatcher
    /// header is self-referential, so a move after `init` corrupts the wait list
    /// silently, and BOTH misuses compiled with zero `unsafe`, because
    /// `enqueue_sync` and `fence_wait_prepare` are safe fns taking a
    /// `NonNull<SyncWaitBlock>`:
    ///
    ///     let mut b = SyncWaitBlock::new_zeroed();
    ///     enqueue_sync(.., NonNull::from(&mut b));     // never init'ed
    ///
    /// and so did returning the block from a helper between `init` and
    /// registration. Neither is expressible now: the value has no name outside
    /// this function.
    ///
    /// Deregistration still depends on the caller's control flow, which is why
    /// the abandon/cancel logic belongs in the closure's own epilogue.
    pub fn with<R>(f: impl FnOnce(&WaitBlockRef<'_>) -> R) -> R {
        let mut block = Self::new_zeroed();
        // SAFETY: `block` is a local of THIS frame, so it is at its final
        // address, and it is never moved afterwards — `f` only ever sees a
        // `WaitBlockRef` borrowing it. It outlives the call to `f`.
        unsafe { block.init() };
        let block_ref = WaitBlockRef {
            ptr: NonNull::from(&mut block),
            _frame: PhantomData,
        };
        f(&block_ref)
    }

    /// A zeroed block. Private: reachable only through [`Self::with`], which is
    /// what makes "registered but never initialised" unrepresentable.
    fn new_zeroed() -> Self {
        // SAFETY: a zeroed KEVENT/AtomicBool/byte-array is a valid *inert*
        // value; `init` initializes the dispatcher header before any use.
        unsafe { core::mem::zeroed() }
    }

    /// Initialize the embedded KEVENT (NotificationEvent, unsignaled).
    ///
    /// # Safety
    /// `self` must be at its final (pinned) address.
    unsafe fn init(&mut self) {
        // SAFETY: valid, stable KEVENT storage per the fn contract.
        unsafe { KeInitializeEvent(&mut self.event, NOTIFICATION_EVENT, 0) };
        self.done.store(false, Ordering::Relaxed);
    }

    /// Copy the response bytes out.
    ///
    /// ⚠ There is deliberately no `is_done` accessor to call first. Reading
    /// `done` never authorized a copy safely — it authorized RESUMING, one
    /// instruction before the drain's `KeSetEvent`, which is the 22.22.218.0
    /// `0xA` (ROADMAP defect 0ab-C, and `ctrl::wait_block`'s doc has the whole
    /// argument). The caller reaches this only after its wait was SATISFIED, so
    /// the drain has finished with the block entirely.
    ///
    /// `done` still carries the release/acquire edge that makes `resp` visible:
    /// it is stored (Release) after the copy and before the signal, and the
    /// wait's own satisfaction is the acquire.
    fn copy_resp(&self, out: &mut [u8]) {
        let n = out.len().min(SYNC_RESP_MAX);
        // SAFETY: `resp` is only written by the drain BEFORE `done` is set
        // (Release) and the event is signaled; this runs after that signal
        // satisfied our wait.
        let src = unsafe { &*self.resp.get() };
        out[..n].copy_from_slice(&src[..n]);
    }
}

/// The only handle a [`SyncWaitBlock::with`] closure gets: a pointer to hand
/// the transport, plus the one read the waiter needs. Borrows the block, so it
/// cannot outlive the frame the block lives on.
///
/// The `is_done` wrapper that used to sit here went with the completion-side
/// poll it existed for (22.22.219.0): exposing "has the drain started writing
/// this block?" to the waiter is what let the waiter leave while the drain was
/// still writing it.
pub struct WaitBlockRef<'a> {
    ptr: NonNull<SyncWaitBlock>,
    _frame: PhantomData<&'a SyncWaitBlock>,
}

impl WaitBlockRef<'_> {
    /// The registration pointer for `enqueue_sync` / `fence_wait_prepare`.
    pub fn as_ptr(&self) -> NonNull<SyncWaitBlock> {
        self.ptr
    }

    pub fn copy_resp(&self, out: &mut [u8]) {
        // SAFETY: as above; `copy_resp`'s own contract covers the ordering.
        unsafe { self.ptr.as_ref() }.copy_resp(out);
    }
}

/// What an in-flight entry is.
enum InFlightKind {
    /// A synchronous control command; `waiter` (if any) is signaled on
    /// completion. `None` = the waiter timed out and abandoned the entry.
    Sync {
        waiter: Option<NonNull<SyncWaitBlock>>,
        /// A synchronous SET_SCANOUT_BLOB already reached the control FIFO.
        /// Its waiter may time out, but an eventual successful response still
        /// changes the host's scanout selection and must remain visible to
        /// DestroyAllocation's lifetime barrier.
        scanout_bind: Option<SyncScanoutBind>,
    },
    /// An async fenced SUBMIT_3D carrying `fence_id` (KMD-assigned wire id).
    /// `ring_idx` 0 = host CPU ring (retires at decode); >= 1 = a per-queue
    /// GPU-completion fence (virglrenderer vkr sync thread) that legally stays
    /// in flight for the full GPU-work duration.
    AsyncVenus {
        fence_id: u64,
        ring_idx: u8,
        scanout_notify: Option<ScanoutNotify>,
        /// Registered present-stream value this normal wire-fence submission
        /// retires.  The stream handle carries its generation, so a stale
        /// completion can never advance a re-registered stream slot.
        present_stream: Option<PresentStreamRetire>,
        /// Exact deferred WindowedBlt transaction whose ring-1 response this
        /// entry represents. It owns no allocation pointers; lookup remains in
        /// the bounded FIFO by token/stream.
        windowed_blt: Option<WindowedBltRetire>,
    },
    /// A fire-and-forget `SET_SCANOUT_BLOB` enqueued by the DISPATCH-level flip
    /// arm (ROADMAP defect 0ab-C, D1(ii)).
    ///
    /// VALUES ONLY, and that is the design rather than an accident: the entry
    /// carries no allocation handle and no pointer into any WDDM object, so its
    /// completion has nothing to dereference and the DestroyAllocation cancel
    /// path (`retire_scanout_allocation_locked`, which CASes the single pending
    /// slot) is untouched by it. The fast path deliberately does NOT claim the
    /// pending slot: hiding the handle from that CAS is the use-after-free this
    /// feature was analysed out of.
    ///
    /// `seq` orders this bind's bookkeeping against the PASSIVE worker's
    /// (`AdapterContext::scanout_bind_wire_seq`); the rest is exactly what
    /// applying that bookkeeping needs — the identity to remember, the epoch to
    /// publish, the physical address the next CRTC_VSYNC reports, and the frame
    /// boundary the flush arm must order against.
    AsyncScanoutBind {
        seq: u64,
        resource_id: u32,
        /// `(width << 32) | height`, as `remember_scanout_blob` wants it.
        wh: u64,
        format: u32,
        stride: u32,
        offset: u32,
        present_epoch: u64,
        primary_address: u64,
        carried_watermark: u64,
    },
    /// A control command whose caller must not wait for the host response.
    /// `completion` is a stable adapter-owned 0/1 gate; the used-ring drain
    /// clears it and wakes the stable worker event.  This lets scanout refresh
    /// coalesce to one outstanding RESOURCE_FLUSH without a synchronous ctrl
    /// round-trip limiting presentation cadence.
    AsyncControl {
        completion: NonNull<AtomicU32>,
        completion_errors: NonNull<AtomicU32>,
        wake_event: NonNull<KEVENT>,
        success_store: Option<(NonNull<AtomicU32>, u32)>,
        resubmit: Option<NonNull<AtomicU32>>,
        /// Which presentation this command's host read covers, when it is a
        /// scan-out `RESOURCE_FLUSH`. `None` for every other async control
        /// command. See [`ScanoutFlushToken`].
        scanout_flush: Option<ScanoutFlushToken>,
    },
}

#[derive(Clone, Copy)]
struct WindowedBltRetire {
    adapter: NonNull<crate::adapter::AdapterContext>,
    token: u64,
    stream_boundary: u64,
}

/// Value-only lifecycle tag for a synchronous `SET_SCANOUT_BLOB`.
///
/// This stays in the transport entry after its stack-resident waiter is
/// abandoned.  The used-ring drain can therefore record a late successful host
/// bind without dereferencing the caller's frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SyncScanoutBind {
    seq: u64,
    resource_id: u32,
    /// Present only for a direct-primary worker SET. Disable/fallback SETs
    /// still carry their resource/sequence into the host-selection ledger but
    /// do not begin a presentation publication transaction.
    request: Option<ScanoutBindRequest>,
}

/// The lifecycle tag reaches the host-selection ledger only for a terminal
/// successful response.  Kept pure so the timeout/late-completion contract has
/// a host-testable edge independent of the WDK transport.
#[inline]
fn terminal_sync_scanout_bind(
    response_ok: bool,
    scanout_bind: Option<SyncScanoutBind>,
) -> Option<SyncScanoutBind> {
    helios_kmd_logic::present_stream::terminal_on_response_ok(response_ok, scanout_bind)
}

/// The stream-side payload attached to one ordinary async Venus submission.
/// Values advance monotonically when the normal wire fence reaches a terminal
/// completion; this is deliberately separate from the wire-fence namespace.
#[derive(Clone, Copy)]
struct PresentStreamRetire {
    handle: u32,
    value: u32,
}

/// One preallocated registered present stream. `creator_process` stores the
/// opaque `hKmdProcess` value associated with the owning `DeviceContext`; it is
/// used only for exact equality and is purged at owner/context teardown.
#[derive(Clone, Copy)]
struct PresentStreamSlot {
    live: bool,
    owner: Option<DeviceOwner>,
    ctx_id: u32,
    ring_idx: u8,
    generation: u32,
    cookie: u64,
    creator_process: usize,
    submitted_value: u32,
    retired_value: u32,
}

impl PresentStreamSlot {
    const EMPTY: Self = Self {
        live: false,
        owner: None,
        ctx_id: 0,
        ring_idx: 0,
        generation: 0,
        cookie: 0,
        creator_process: 0,
        submitted_value: 0,
        retired_value: 0,
    };

    fn handle(self, index: usize) -> u32 {
        // Low six bits are the raw slot index.  Generation is nonzero, so the
        // whole handle is nonzero without making slot 63 carry into bit 6.
        // This keeps the packed handle within the 31 bits reserved by the
        // tagged-boundary ABI.
        helios_kmd_logic::present_stream::slot_handle(self.generation, index)
    }
}

/// Allocate the bounded present-stream registry outside `VirtioGpu::init`'s
/// already-critical boot stack frame.
///
/// This table used to be an inline `[PresentStreamSlot; 64]` field. Because
/// `VirtioGpu` is returned by value through `DxgkDdiStartDevice`, LLVM
/// materialized that 3 KiB array in both frames and took the measured nested
/// boot chain from the 17,936-byte known-good ceiling to 29,264 bytes. On the
/// 24 KiB x64 kernel stack that double-faults before Windows can write a dump,
/// surfacing only as `0xc0000001`/Startup Repair.
///
/// `#[inline(never)]` keeps the small allocation loop transient. The returned
/// boxed slice is fixed-length for the entire transport generation, so no
/// registration, submission, completion, DPC, or teardown path can reallocate.
#[inline(never)]
fn allocate_present_streams() -> Box<[PresentStreamSlot; MAX_PRESENT_STREAMS]> {
    let mut slots = Box::<[PresentStreamSlot; MAX_PRESENT_STREAMS]>::new_uninit();
    let first = slots.as_mut_ptr().cast::<PresentStreamSlot>();
    for index in 0..MAX_PRESENT_STREAMS {
        // SAFETY: `first` addresses the boxed array allocation and every index
        // is in bounds. Each element is written exactly once before
        // `assume_init` below; no reference to an uninitialized slot is made.
        unsafe { first.add(index).write(PresentStreamSlot::EMPTY) };
    }
    // SAFETY: the loop initialized all MAX_PRESENT_STREAMS elements.
    unsafe { slots.assume_init() }
}

/// Bounded completion-ordered scanout work.
///
/// There are intentionally only two records. `earliest` is the first pending
/// producer boundary and is never overwritten by a faster Present stream; it
/// guarantees that a continuously submitting app cannot postpone every dirty
/// edge forever. `latest` coalesces the later work, but retains *its own*
/// resource/boundary pair. Once the earliest completes, it is issued first
/// unless the latest is already safe, in which case the latest supersedes it.
///
/// This lives behind a Box in `VirtioGpu`: StartDevice already has a narrow
/// kernel-stack budget, so even this fixed, small state must not grow its
/// by-value construction frame.
struct ScanoutRefreshState {
    inner: RefreshState,
}

#[inline(never)]
fn allocate_scanout_refresh_state() -> Box<ScanoutRefreshState> {
    Box::new(ScanoutRefreshState {
        inner: RefreshState::new(),
    })
}

impl ScanoutRefreshState {
    fn clear(&mut self) {
        self.inner.clear();
    }

    fn note(&mut self, marker: ScanoutRefreshMarker, ready: bool) -> bool {
        self.inner.note(marker, ready)
    }

    fn earliest(&self) -> Option<ScanoutRefreshMarker> {
        self.inner.earliest()
    }

    fn latest(&self) -> Option<ScanoutRefreshMarker> {
        self.inner.latest()
    }

    fn take_ready(
        &mut self,
        earliest_ready: bool,
        latest_ready: bool,
    ) -> Option<ScanoutRefreshMarker> {
        self.inner.take_ready(earliest_ready, latest_ready)
    }

    fn discard_dead_present_stream_markers<F>(&mut self, stream_live: F)
    where
        F: Fn(u64) -> bool,
    {
        self.inner.discard_cancelled(|marker| {
            decode_present_stream_boundary(marker.boundary()).is_none()
                || stream_live(marker.boundary())
        });
    }
}

/// Readiness for one decoded stream slot.
///
/// A dead or generation-mismatched slot is never success.  Its owner must
/// explicitly discharge any scheduler/scanout wait that still carries the old
/// boundary before the slot is retired; accepting it here would turn a rejected
/// producer into a false `DMA_COMPLETED` edge.
#[inline]
fn present_stream_slot_ready(
    slot: PresentStreamSlot,
    index: usize,
    handle: u32,
    value: u32,
) -> bool {
    helios_kmd_logic::present_stream::slot_ready(
        slot.live,
        slot.generation,
        index,
        handle,
        value,
        slot.retired_value,
    )
}

#[inline]
fn advance_present_stream_retired(retired_value: u32, completed_value: u32) -> u32 {
    helios_kmd_logic::present_stream::advance_retired(retired_value, completed_value)
}

/// Encode a generation-qualified opaque present-stream boundary.
#[inline]
pub fn encode_present_stream_boundary(handle: u32, value: u32) -> u64 {
    helios_kmd_logic::present_stream::encode_boundary(handle, value)
}

/// Decode a tagged boundary.  Legacy wire-fence boundaries are intentionally
/// not accepted here: their ordering relation is unrelated to stream values.
#[inline]
pub fn decode_present_stream_boundary(boundary: u64) -> Option<(u32, u32)> {
    helios_kmd_logic::present_stream::decode_boundary(boundary)
}

/// Capacity of the fixed, allocation-free registered present-stream table.
pub fn max_present_streams() -> usize {
    MAX_PRESENT_STREAMS
}

/// Take the scan-out ownership token out of an in-flight entry, leaving the rest
/// of the entry intact.
///
/// Both drain paths do `match entry.kind { .. }` by value and then PARK the
/// entry (a `DmaBuffer` may not be freed above PASSIVE), so the token — the one
/// non-`Copy` field in `InFlightKind` — has to come out first or `entry` is
/// partially moved. Not making [`ScanoutFlushToken`] `Copy` is deliberate:
/// completing one twice would count a host read that never happened.
fn take_scanout_flush_token(kind: &mut InFlightKind) -> Option<ScanoutFlushToken> {
    match kind {
        InFlightKind::AsyncControl { scanout_flush, .. } => scanout_flush.take(),
        _ => None,
    }
}

/// What one DISPATCH-level fast bind asks for: the wire command's geometry plus
/// the bookkeeping its completion must apply (ROADMAP defect 0ab-C, D1(ii)).
///
/// One `Copy` value rather than eleven arguments, so the enqueue and the
/// in-flight entry cannot disagree about which flip they describe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanoutBindRequest {
    pub resource_id: u32,
    pub width: u32,
    pub height: u32,
    /// `ScanoutFormat::virtio()` — the wire format word.
    pub format: u32,
    pub stride: u32,
    pub offset: u32,
    /// The presentation epoch this flip minted.
    pub present_epoch: u64,
    /// The physical address a later CRTC_VSYNC reports for this primary.
    pub primary_address: u64,
    /// The frame-completion boundary the flip took out of the mark table (0 =
    /// none), which the flush arm orders against.
    pub carried_watermark: u64,
}

/// A fast bind the host has ACCEPTED, waiting for its bookkeeping to be applied
/// outside `virtio_lock` (ROADMAP defect 0ab-C, D1(ii)).
///
/// The drain cannot apply it itself: applying ends in a flush arm that needs
/// `wddm_notify_lock`, and the driver's order is notify → virtio (see
/// `adapter/locks.rs`). So the drain stashes these values and
/// `drain_used_and_complete` — one frame up the same DPC, holding no transport
/// lock — applies them.
#[derive(Clone, Copy)]
pub struct CompletedBind {
    pub seq: u64,
    pub resource_id: u32,
    pub wh: u64,
    pub present_epoch: u64,
    pub primary_address: u64,
    pub carried_watermark: u64,
    pub format: u32,
    pub stride: u32,
    pub offset: u32,
}

#[inline]
pub fn completed_request(bind: CompletedBind) -> ScanoutBindRequest {
    ScanoutBindRequest {
        resource_id: bind.resource_id,
        width: (bind.wh >> 32) as u32,
        height: bind.wh as u32,
        format: bind.format,
        stride: bind.stride,
        offset: bind.offset,
        present_epoch: bind.present_epoch,
        primary_address: bind.primary_address,
        carried_watermark: bind.carried_watermark,
    }
}

/// Heap-owned fixed storage for the fast-bind completion handoff and its one
/// completion-ordered request. Keeping these values out of `VirtioGpu`'s
/// by-value construction path preserves the StartDevice stack ceiling; the box
/// is allocated once at PASSIVE and never resized under a spinlock.
struct FastBindState {
    completed: Option<CompletedBind>,
    /// Exactly one host-visible presentation SET may be outstanding.  It owns
    /// the full request through its SET response and, on success, through that
    /// request's exact RESOURCE_FLUSH response.  This is fixed value-only
    /// state: it never extends a Windows allocation lifetime or allocates under
    /// `virtio_lock`.
    publication_request: Option<ScanoutBindRequest>,
    publication: helios_kmd_logic::scanout_publish_txn::State,
    /// Highest direct presentation epoch whose SET descriptor was accepted by
    /// the control queue. Unlike the host-reader transaction this is never
    /// lowered by a host error, flush terminal response, or resource retirement:
    /// admitting an older descriptor would make control-FIFO scanout move
    /// backward after a newer presentation already reached the host.
    presentation_epoch_floor: u64,
    /// The oldest unresolved producer boundary.  This is a liveness frontier:
    /// do not overwrite it with a newer frame, or a stream producing more
    /// quickly than it retires can starve scanout forever.
    deferred_earliest: Option<ScanoutBindRequest>,
    /// One coalescing slot behind the frontier.  Once the frontier retires we
    /// prefer this newest ready request, retaining it if it is still waiting.
    deferred_latest: Option<ScanoutBindRequest>,
    /// Last SET_SCANOUT_BLOB which the host acknowledged.  It survives the
    /// DPC's value handoff, so DestroyAllocation can issue a real disable
    /// barrier even during the response-to-bookkeeping gap.
    host_accepted_seq: u64,
    host_accepted_resource: u32,
    host_accepted_fast: bool,
    host_accepted_fast_request: Option<ScanoutBindRequest>,
    /// DestroyAllocation sets this while it is establishing its disable
    /// barrier.  A later DISPATCH flip for the same resource is refused before
    /// it can place a new SET behind that barrier.
    retiring_resource: u32,
    /// Global lifecycle barrier while DestroyAllocation resolves the final host
    /// scanout selection. The PASSIVE worker is serialized by `scanout_mutex`,
    /// so only the DISPATCH fast path needs this explicit gate.
    retire_barrier: bool,
    /// Exact request claimed by the synchronous worker. It remains cached after
    /// a successful response so a flip thread preempted between publishing the
    /// pending handle and staging its fast request cannot enqueue the same SET
    /// behind the worker's already-completed one. A distinct worker claim
    /// replaces it.
    sync_worker_owned: Option<ScanoutBindRequest>,
    deferred_worker: Option<ScanoutBindRequest>,
    fast_failure_wake: Option<ScanoutBindRequest>,
}

#[inline(never)]
fn allocate_fast_bind_state() -> Box<FastBindState> {
    Box::new(FastBindState {
        completed: None,
        publication_request: None,
        publication: helios_kmd_logic::scanout_publish_txn::State::new(),
        presentation_epoch_floor: 0,
        deferred_earliest: None,
        deferred_latest: None,
        host_accepted_seq: 0,
        host_accepted_resource: 0,
        host_accepted_fast: false,
        host_accepted_fast_request: None,
        retiring_resource: 0,
        retire_barrier: false,
        sync_worker_owned: None,
        deferred_worker: None,
        fast_failure_wake: None,
    })
}

/// Why a fast bind did not reach the wire. Both arms leave the transport
/// exactly as they found it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FastBindRefusal {
    /// Every preallocated command buffer is still in flight (`FpBusy`).
    Busy,
    /// This direct presentation is at or below an already-accepted SET epoch.
    /// It is terminal, not queue pressure, and must not be retried.
    Superseded,
    /// The command could not be encoded, or the ring refused it (`FpErr`). The
    /// buffer is back in its slot.
    Failed,
}

/// Result of staging or promoting one completion-ordered fast bind. `Deferred`
/// owns no DMA buffer and does not touch the wire sequence; `Queued` is the
/// only outcome that has published a `SET_SCANOUT_BLOB` descriptor.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FastBindDispatch {
    Queued,
    Deferred,
    /// The exact request is already owned or completed by the synchronous
    /// worker. No fast descriptor was published and no fallback is needed.
    Handled,
    /// The request was at/below the descriptor-acceptance floor and was
    /// discarded without publishing another SET.
    Superseded,
    Busy,
    Failed,
}

/// Whether the refresh executor may issue a read for the currently sampled
/// scanout resource.  This is deliberately a tri-state, rather than an
/// `Option<request>`: an active transaction for another resource or in either
/// non-ready phase must block an ordinary atomic-epoch refresh, never fall
/// through as though no transaction existed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PublicationRefresh {
    NoActive,
    ReadyExact(ScanoutBindRequest),
    Blocked,
}

/// Decision for the PASSIVE synchronous fallback.  `Abandoned` is distinct
/// from `Waiting`: retrying a destroyed or explicitly dead producer would
/// retain the programming gate with no future used-ring wake.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WorkerBindDispatch {
    Ready,
    Waiting,
    Abandoned,
    Superseded,
}

/// One outstanding control-queue submission. Owns its device-visible buffers
/// for as long as the device may DMA them (until `pop_used`); afterwards the
/// entry is parked and dropped at PASSIVE_LEVEL (DmaBuffer frees are
/// PASSIVE-only).
pub struct InFlight {
    /// Descriptor-chain head returned by `VirtQueue::add` (the pop_used token).
    token: u16,
    kind: InFlightKind,
    /// `[in0 | in1? | resp]` — request span(s) followed by the device-written
    /// response span, all in one contiguous DMA buffer.
    meta: DmaBuffer,
    /// The exact shape `add` was called with. One value, matched exhaustively,
    /// instead of three independent length fields the drain re-derived by hand.
    chain: Chain,
    resp_len: usize,
    /// Separate device-read venus payload (async submits).
    venus: Option<DmaBuffer>,
}

/// The descriptor-chain shape of one submission.
///
/// `add` and `pop_used` must be handed the SAME buffer list, and virtio-drivers
/// does not validate that: `pop_used` -> `recycle_descriptors` walks the chain
/// against the caller-supplied list and PANICS on a mismatch
/// (`virtio-drivers-0.13.0/src/queue.rs:461,477,479,501`), which under
/// `panic = "abort"` with `wdk_panic` is a `KeBugCheck` from the interrupt DPC
/// while the device spinlock is held.
///
/// The agreement used to be expressed only by three independent length fields
/// plus the comment "exactly the spans `add` was called with", re-derived in
/// four separate blocks of raw-pointer arithmetic. As one exhaustively-matched
/// value it is a compile error to add a shape without teaching both sides.
#[derive(Clone, Copy)]
enum Chain {
    /// `[in0] -> [resp]`. Async control commands.
    Meta1 { in0_len: usize },
    /// `[in0, in1] -> [resp]`. Sync control with a second device-read span.
    Meta2 { in0_len: usize, in1_len: usize },
    /// `[hdr, venus stream] -> [resp]`, the stream in its own buffer.
    MetaPlusVenus { hdr_len: usize, venus_len: usize },
}

impl Chain {
    /// Byte offset of the response span inside `meta`.
    ///
    /// All three shapes place it immediately after the meta-resident request
    /// spans, i.e. at `in0_len + in1_len` — preserved verbatim, because
    /// `drain_used` reads the `VIRTIO_GPU_RESP_*` word and copies the sync
    /// response from exactly this offset.
    fn resp_offset(self) -> usize {
        match self {
            Self::Meta1 { in0_len } => in0_len,
            Self::Meta2 { in0_len, in1_len } => in0_len + in1_len,
            Self::MetaPlusVenus { hdr_len, .. } => hdr_len,
        }
    }

    /// The device-READ spans (up to two) and the device-WRITTEN response span,
    /// or `None` if any of them falls outside its buffer.
    ///
    /// The ONLY producer of the buffer list, so `add` and `pop_used` are handed
    /// literally the same code and the arm selection cannot differ between
    /// them. It also replaces the per-arm ad-hoc `t <= meta.as_slice().len()`
    /// bounds checks: every span is proved in `DmaBuffer::span`.
    fn spans(
        self,
        meta: &DmaBuffer,
        venus: Option<&DmaBuffer>,
        resp_len: usize,
    ) -> Option<([DmaSpan; 2], usize, DmaSpan)> {
        let resp = meta.span(self.resp_offset(), resp_len)?;
        Some(match self {
            Self::Meta1 { in0_len } => ([meta.span(0, in0_len)?, DmaSpan::EMPTY], 1, resp),
            Self::Meta2 { in0_len, in1_len } => (
                [meta.span(0, in0_len)?, meta.span(in0_len, in1_len)?],
                2,
                resp,
            ),
            Self::MetaPlusVenus { hdr_len, venus_len } => (
                [meta.span(0, hdr_len)?, venus?.span(0, venus_len)?],
                2,
                resp,
            ),
        })
    }
}

impl InFlight {
    /// Recover the entry-owned DMA buffers after the device has consumed them.
    /// Called only by the PASSIVE reaper after the entry leaves `parked`.
    pub fn into_dma_buffers(self) -> (DmaBuffer, Option<DmaBuffer>) {
        (self.meta, self.venus)
    }
}

/// A non-`Copy`, non-`Clone` receipt for one registered synchronous submission.
///
/// The token, the `NonNull<SyncWaitBlock>` and the block's own pinned storage
/// are three values the caller had to keep in sync across enqueue / PASSIVE
/// wait / abandon. Making the token move-only means it cannot be reused after
/// the abandonment that consumes it.
pub struct SyncTicket {
    token: u16,
}

impl SyncTicket {
    /// The raw descriptor-chain head, for a diagnostic breadcrumb only. Reading
    /// it does not consume the ticket; `abandon_sync` still does.
    pub fn raw(&self) -> u16 {
        self.token
    }
}

/// What [`VirtioGpu::abandon_sync`] found.
///
/// This replaces a bool whose fall-through returned `true` — "already
/// completed, treat as success" — for EVERY case that was not an exact
/// (token, Sync, same-waiter) match, including a token now owned by a different
/// command and a kind mismatch. The caller then ran `copy_resp` on a block that
/// may never have been written. That was safe only because (a) a token cannot
/// be re-issued until its chain is popped, which implies the old waiter was
/// signalled, and (b) `new_zeroed` leaves `resp` all-zero and 0 is not a
/// RESP_OK code, so `ctrl_roundtrip_ok` still errored out — i.e. correctness
/// rested on a virtio-ring property and a zero-init accident, neither of them
/// stated at that function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// No in-flight entry holds this token: the drain already completed and
    /// signalled it. The response bytes are valid.
    AlreadyCompleted,
    /// The waiter was deregistered before completion. The response bytes were
    /// never written.
    Abandoned,
    /// The token names an entry that is not this waiter's — a NEW error
    /// population, and the one behaviour change in this item. It converts a
    /// silent read of an unwritten buffer into a counted error.
    NotOurs,
}

/// A registered WAIT_FENCE waiter.
struct FenceWaiter {
    fence_id: u64,
    block: NonNull<SyncWaitBlock>,
}

/// A usermode event registered for one-shot signaling at wire-fence retirement
/// (REGISTER_FENCE_EVENT, KMD 22.22.54). `event` is the executive event
/// object's body (a `KEVENT`) from `ObReferenceObjectByHandle` — the escape
/// handler took a reference, so the object outlives the registering process;
/// whoever removes the entry MUST dereference it (the drain uses
/// `ObDereferenceObjectDeferDelete` — a plain deref at DISPATCH could run the
/// object's PASSIVE-only deletion if it drops the last reference).
struct FenceEventEntry {
    fence_id: u64,
    event: NonNull<KEVENT>,
}

/// Result of [`VirtioGpu::fence_event_register`].
pub enum FenceEventReg {
    /// Parked; the drain will KeSetEvent + deref at retirement.
    Registered,
    /// The fence has already retired. NOT parked, no reference kept by the
    /// table — the caller signals + derefs.
    AlreadyComplete,
    /// The id was never assigned by this transport instance.
    Invalid,
    /// Table full (counted) — the caller falls back to the blocking wait.
    TableFull,
    /// This (fence_id, event) pair is already parked (counted, refused).
    Duplicate,
}

/// First wire fence id the NEXT transport instance will hand out.
///
/// Driver-global and monotonic across StartDevice/StopDevice cycles. Starts at
/// 1 because 0 is the "no fence" sentinel every predicate tests for.
static NEXT_WIRE_FENCE_BASE: AtomicU64 = AtomicU64::new(1);

/// WDDM submissions whose fence was gated on their exact live present stream
/// boundary instead of the whole `next_wire_fence` backlog (`PresentWmk=1`).
/// Mirrored as `PwExact`; zero while the knob is off is the correct reading.
pub(crate) static PRESENT_EXACT_WATERMARK_USED: AtomicU32 = AtomicU32::new(0);
/// D3D12 ECL submissions gated on the EXACT wire fence their batch ends at rather
/// than on the prefix below it (A4, `docs/dx12/PENDING.md` §1). Mirrored as
/// `D12Exact`.
///
/// ⚠ NOT KNOB-GATED, and that is deliberate: the prefix was an invariant
/// violation, not a tuning choice, so there is no "restore the superset" arm to
/// keep reachable. The A/B that matters is `D12Zero` — a UMD naming no boundary at
/// all — which is already the documented order-against-nothing lever.
///
/// GRADING: expected to EQUAL the number of D3D12 records that named a usable
/// boundary, i.e. `D12Rec - D12Zero - D12MrgF - GpuFncClamp - GpuFncGen`. Any
/// shortfall means a D3D12 packet took a prefix arm, which is the defect A4 names
/// coming back; `D12Exact > 0` with `EscSubRing == 0` means the ICD is handing the
/// UMD ring-0 fence ids, so the exactness is exact about the wrong domain.
pub(crate) static D3D12_EXACT_WATERMARK_USED: AtomicU32 = AtomicU32::new(0);
/// Gap between one instance's first id and the next instance's.
///
/// Far more than any instance can consume: at the ~10^5 fences a heavy session
/// produces, 2^32 instances' worth of headroom remains, and a u64 counter
/// cannot wrap in the machine's lifetime.
const WIRE_FENCE_INSTANCE_STRIDE: u64 = 1 << 32;

/// The KMD's allocator for offsets inside the host-visible BAR window.
///
/// Its three pieces — the high-water bump, the coalescing free list and the
/// VidMm reserve — were three loose fields of `VirtioGpu`, and the reserve was
/// installed by a plain setter two statements after `set_virtio` in StartDevice.
/// A SECOND `reserve_window_prefix` with a larger len would silently strand
/// every offset already issued below the new mark: `free_window_range` returns
/// early for `offset < reserve`, so those ranges could never be recycled and
/// nothing would say so.
///
/// The reserve is now immutable once any offset has been issued —
/// [`VirtioGpu::configure_window_reserve`] refuses and counts otherwise. It is a
/// guarded setter rather than a literal construct-with-reserve because the
/// reserve is computed from the window length AFTER `VirtioGpu::init` and
/// installed under the device spinlock, where allocating the free list's `Vec`
/// is forbidden.
struct WindowAllocator {
    /// Total bytes of the host-visible window (0 if the device exposes none).
    window_len: u64,
    /// First `reserve` bytes are OWNED BY VIDMM (the CPU-visible BAR memory
    /// segment, `query_adapter_info`): VidMm's segment allocator assigns offsets
    /// there and `BuildPagingBuffer` maps each allocation's blob at the assigned
    /// offset. This allocator never hands out or reclaims offsets below the mark.
    reserve: u64,
    /// Bump high-water.
    next_offset: u64,
    /// Coalescing free list (bounded by MAX_WINDOW_RANGES).
    free_ranges: Vec<WindowRange>,
}

impl WindowAllocator {
    /// PASSIVE only: reserves the free list up front so `alloc`/`free` under the
    /// device spinlock never allocate.
    fn new(window_len: u64) -> Self {
        Self {
            window_len,
            reserve: 0,
            next_offset: 0,
            free_ranges: Vec::with_capacity(MAX_WINDOW_RANGES),
        }
    }

    /// True while no offset has been issued and nothing has been freed — the
    /// only state in which the reserve may still be set.
    fn is_pristine(&self) -> bool {
        self.next_offset == self.reserve && self.free_ranges.is_empty()
    }

    /// Bytes currently handed out.
    fn used(&self) -> u64 {
        let free: u64 = self.free_ranges.iter().map(|r| r.len).sum();
        self.next_offset.saturating_sub(free)
    }

    /// Allocate a page-rounded `len`-byte range: reuse a free range if one fits,
    /// else bump the high-water mark (bounded by `window_len`).
    fn alloc(&mut self, len: u64) -> Result<u64, VirtioError> {
        if let Some(idx) = self.free_ranges.iter().position(|r| r.len >= len) {
            let offset = self.free_ranges[idx].offset;
            if self.free_ranges[idx].len == len {
                self.free_ranges.swap_remove(idx);
            } else {
                self.free_ranges[idx].offset += len;
                self.free_ranges[idx].len -= len;
            }
            return Ok(offset);
        }
        let offset = self.next_offset;
        let end = match offset.checked_add(len) {
            Some(e) if e <= self.window_len => e,
            _ => {
                WINDOW_ALLOC_REJECTS.fetch_add(1, Ordering::Relaxed);
                return Err(VirtioError::OutOfMemory);
            }
        };
        self.next_offset = end;
        Ok(offset)
    }

    /// Return a range: drop the high-water mark if it abuts, else coalesce into
    /// an adjacent free range, else record a new free range (or silently leak if
    /// the bounded free list is full — bring-up acceptable).
    fn free(&mut self, offset: u64, len: u64) {
        if len == 0 {
            return;
        }
        // VidMm-partition offsets are owned by VidMm's segment allocator — they
        // must never enter the KMD free list (a later KMD-side map would collide
        // with a VidMm placement). Every release path funnels here, so this one
        // guard covers DestroyAllocation/ReleaseBlob/teardown of VidMm-placed
        // blobs uniformly. PRESERVED VERBATIM: it is what keeps VidMm-partition
        // offsets out of the KMD free list.
        if offset < self.reserve {
            return;
        }
        if offset.checked_add(len) == Some(self.next_offset) {
            self.next_offset = offset;
            while let Some(idx) = self
                .free_ranges
                .iter()
                .position(|r| r.offset.checked_add(r.len) == Some(self.next_offset))
            {
                let r = self.free_ranges.swap_remove(idx);
                self.next_offset = r.offset;
            }
            return;
        }
        for range in &mut self.free_ranges {
            if range.offset.checked_add(range.len) == Some(offset) {
                range.len += len;
                return;
            }
            if offset.checked_add(len) == Some(range.offset) {
                range.offset = offset;
                range.len += len;
                return;
            }
        }
        if self.free_ranges.len() < MAX_WINDOW_RANGES {
            self.free_ranges.push(WindowRange { offset, len });
        } else {
            WINDOW_RANGE_DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Which retirement domain a wait is against.
///
/// The nine-line doc this replaces explained at length that the wait is ring-0
/// only and why counting ring >= 1 fences would be wrong — while sitting on a
/// function whose `wait_gpu: bool` parameter did exactly that, undocumented,
/// and whose three callers picked the mode three different ways (two hardcode
/// true, one derives it from `gpu_completion_fence.is_some()`, one replays a
/// stored value). Both values genuinely occur.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RetireDomain {
    /// Ring-0 only: host DECODE retirement.
    ///
    /// This is the domain the WDDM pending FIFO's contract was built on. It
    /// exists to order DMA_COMPLETED behind the venus escape traffic queued
    /// before it, and ring-0 fences retire at decode. ring >= 1 fences (WS1 #4)
    /// retire at host GPU COMPLETION and stay in flight for the full GPU-work
    /// duration, so counting them here would couple every WDDM DMA fence
    /// (GDI/paging pacing) to unrelated multi-ms GPU work. Consumers that need
    /// GPU completion wait on those fences explicitly (WAIT_FENCE).
    DecodeOnly,
    /// Every ring: the caller genuinely needs host GPU completion, e.g. the
    /// direct-primary refresh marker ordering on a Venus completion watermark.
    IncludingGpu,
}

/// How a [`WddmPending::watermark`] is compared against the in-flight wire fences.
///
/// ⛔ THE DISTINCTION IS A CLAUDE.md INVARIANT, not a tuning choice: *"a WDDM fence
/// may wait on the frame's OWN boundary, never on the whole `next_wire_fence`
/// backlog."* A prefix wait is satisfied only when EVERY async fence below the
/// watermark has retired — every ring, every process, DWM's ring-1 scanout copies
/// included — so it delays the fence by the whole pipeline depth and is the
/// over-wait `PRESENT_EXACT_WATERMARK_USED` had to relax away on the present path
/// (measured: dxgkrnl blocks the presenting thread at its 3-deep present queue,
/// 21 % of presents, 2.45 ms each).
#[derive(Clone, Copy, PartialEq, Eq)]
enum WireBoundary {
    /// `watermark` is EXCLUSIVE and a PREFIX: every async fence strictly below it
    /// must have retired. Conservative, always eventually satisfied, never a lie,
    /// and the fallback whenever a boundary cannot be trusted.
    Prefix,
    /// `watermark` names ONE wire fence and ONLY that fence must have retired.
    ///
    /// The frame's own boundary. See the D3D12 arm in
    /// [`VirtioGpu::note_wddm_submission`] for why exactness is sound there and
    /// why it is not applied to the legacy Present arm.
    Exact,
}

/// A WDDM submission whose `DXGK_INTERRUPT_DMA_COMPLETED` is gated on venus
/// completion: it may signal once every async wire fence `< watermark` has
/// retired (and strictly in FIFO order — SubmissionFenceIds are watermarks to
/// dxgkrnl, so they must complete monotonically).
/// One WDDM submission waiting for its Venus watermark.
///
/// ⚠ IT CARRIED A SECOND HALF UNTIL 22.22.217.0 — the presentation epoch whose
/// host `RESOURCE_FLUSH` had to complete before dxgkrnl could hand the
/// allocation back to DXGI (ROADMAP defect 0ab-B). The theory was sound and the
/// measurement was not: a 2×2 factorial over 46 681 frames moved whole-flush
/// black by nothing in any cell, because the app's clear of a reclaimed buffer
/// never travels in a WDDM DMA buffer and so waits on no completion this driver
/// controls. `watermark` — "has the app finished WRITING this frame?" — is the
/// only question a WDDM completion can answer, and it is the one asked here
/// again. The epochs live on, deciding the flush executor's ownership gate.
struct WddmPending {
    fence: u32,
    /// Legacy normal-wire producer boundary (possibly the KMD scanout-copy
    /// ring-1 fence).  This namespace is intentionally separate from
    /// `stream_boundary` below.
    watermark: u64,
    /// Whether `watermark` is a prefix bound or the one fence this packet's own
    /// work ends at. See [`WireBoundary`].
    wire_boundary: WireBoundary,
    domain: RetireDomain,
    /// Optional generation-qualified registered-stream marker carried by the
    /// KMD private DMA record.  A WDDM completion requires BOTH boundaries.
    stream_boundary: Option<u64>,
    /// Exact WindowedBlt request admitted by this scheduler submission. A
    /// token is not comparable across streams; readiness tests membership in
    /// the bounded terminal set, never a global numeric watermark.
    blt_token: Option<u64>,
    /// Preserved exact stream identity for `blt_token`. `stream_boundary` can
    /// be discharged after generation death, but a dispatched copy still has
    /// to wait for the terminal pair rather than `(token, None)`.
    blt_stream_boundary: Option<u64>,
    /// `WddmHoldMs`: interrupt-time deadline (100 ns units) before which this
    /// entry may not complete, or 0 for every ordinary submission.
    ///
    /// ⚠ THE ONLY ARTIFICIAL DELAY IN THIS STRUCT, and the only field here that
    /// does not describe real work. It is set exclusively for a packet that
    /// carried a `HeliosD3D12SubmitCmd`, because this FIFO is adapter-global and
    /// head-of-line: attaching a hold to anything else stalls DWM. It exists to
    /// answer UV1 — whether dxgkrnl orders the D3D12 runtime's monitored-fence
    /// signal behind our DMA packets — and nothing in the shipping path reads it,
    /// since the knob defaults to 0.
    hold_until_100ns: u64,
    /// `WddmHeadMs`: absolute interrupt-time deadline (100 ns units) after which
    /// this entry's TAGGED-namespace dependencies are rebased onto the conservative
    /// wire watermark. 0 until the first blocked look at this entry AS HEAD arms it.
    ///
    /// ⚠ Armed lazily, and from the HEAD position only, because the bound is on how
    /// long the head may block. An entry that sits in the FIFO behind legitimately
    /// slow predecessors has not blocked on anything of its own.
    head_deadline_100ns: u64,
    /// Set once this entry has been rebased. A second rebase could only make the
    /// dependency STRICTER — it would install a NEWER `next_wire_fence` — so it is
    /// forbidden rather than merely pointless.
    rebased: bool,
}

/// One WDDM submission represents every WindowedBlt terminal for the same
/// generation-qualified stream through `max_token`.  Tokens have no ordering
/// relation across streams, so the decoded stream handle is part of the
/// representation rather than an optimization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WindowedBltTerminalPrefix {
    max_token: u64,
    max_boundary: u64,
}

impl WindowedBltTerminalPrefix {
    fn new(max_token: u64, max_boundary: u64) -> Option<Self> {
        let _ = decode_present_stream_boundary(max_boundary)?;
        (max_token != 0).then_some(Self {
            max_token,
            max_boundary,
        })
    }

    fn contains(self, token: u64, boundary: u64) -> bool {
        helios_kmd_logic::windowed_blt_token::prefix_contains(
            self.max_token,
            self.max_boundary,
            token,
            boundary,
        )
    }
}

/// One complete two-phase WindowedBlt transaction. It is created by Present
/// while holding scanout -> venus -> virtio, but cannot submit until the exact
/// same stream/token appears in SubmitCommand after residency is effective.
#[derive(Clone, Copy)]
pub(crate) struct WindowedBltPending {
    adapter: NonNull<crate::adapter::AdapterContext>,
    pub(crate) token: u64,
    pub(crate) stream_boundary: u64,
    pub(crate) source_resource_id: u32,
    pub(crate) destination_resource_id: u32,
    pub(crate) source: OptimalPresentImageDesc,
    pub(crate) destination: PresentDestinationDesc,
    pub(crate) prepared: PreparedPresentBltSubmission,
    pub(crate) ledger_ticket: LedgerTicket,
    pub(crate) admitted: bool,
    pub(crate) dispatched: bool,
    pub(crate) ring_complete: bool,
    pub(crate) ledger_retired: bool,
    /// `false` after the WDDM overflow path has discarded the DMA completion
    /// that formerly owned this token. The host copy may still need to reach
    /// ring completion, but it must not retain a terminal membership no WDDM
    /// submission can ever consume.
    pub(crate) wddm_completion_required: bool,
    pub(crate) system_backing: bool,
    pub(crate) mirror_claimed: bool,
}

/// Preallocated FIFO plus an exact terminal-membership table. Keeping terminal
/// identity as `(token, stream)` avoids the unsound cross-context conclusion
/// that a larger token means an unrelated stream has completed.
struct WindowedBltState {
    pending: VecDeque<WindowedBltPending>,
    /// Submission-order admission queue. Present order is not a residency
    /// order across contexts, so only SubmitCommand may push here.
    ready: VecDeque<u64>,
    terminal: VecDeque<(u64, u64)>,
    next_token: u64,
}

impl WindowedBltState {
    fn new() -> Self {
        Self {
            pending: VecDeque::with_capacity(MAX_WINDOWED_BLT_PENDING),
            ready: VecDeque::with_capacity(MAX_WINDOWED_BLT_PENDING),
            terminal: VecDeque::with_capacity(MAX_WINDOWED_BLT_PENDING),
            next_token: 1,
        }
    }

    fn issue_token(&mut self) -> Option<u64> {
        // Terminal identities stay resident until their WDDM fence is
        // delivered/requeued. Count both populations, otherwise a blocked
        // WDDM head could silently evict a newer terminal identity.
        if self.pending.len() + self.terminal.len() >= MAX_WINDOWED_BLT_PENDING {
            return None;
        }
        let token = self.next_token;
        self.next_token = self.next_token.checked_add(1)?;
        Some(token)
    }

    fn terminal_contains(&self, token: u64, stream_boundary: u64) -> bool {
        self.terminal
            .iter()
            .any(|&(known, stream)| known == token && stream == stream_boundary)
    }

    /// A merged private record is complete only when its own terminal exists
    /// AND no still-pending request from that same stream can belong to the
    /// represented token prefix. A numeric token from another stream never
    /// participates in either part of this test.
    fn terminal_prefix_ready(&self, prefix: WindowedBltTerminalPrefix) -> bool {
        self.terminal_contains(prefix.max_token, prefix.max_boundary)
            && !self.pending.iter().any(|request| {
                request.wddm_completion_required
                    && prefix.contains(request.token, request.stream_boundary)
            })
    }

    /// Discard exactly the terminal identities represented by a successfully
    /// delivered WDDM DMA completion (or by an overflow that abandoned it).
    /// Entries for an interleaved stream stay resident even if their numeric
    /// tokens are lower.
    fn consume_terminal_prefix(&mut self, prefix: WindowedBltTerminalPrefix) {
        self.terminal
            .retain(|&(token, boundary)| !prefix.contains(token, boundary));
    }
}

/// A WDDM submission popped from the pending FIFO whose `DMA_COMPLETED` has not
/// yet been delivered.
///
/// Deliberately **not** `Copy`, and consumed by exactly two operations
/// ([`WddmReady::delivered`] and [`VirtioGpu::requeue_wddm_front`]). While this
/// value is alive the entry is in no queue at all: the fence is out of
/// `wddm_pending` and the completed watermark still points below it. Dropping it
/// without doing either loses the fence permanently, VidSch never sees it retire,
/// and the only symptom is a TDR. The `Copy` derive it used to have was what made
/// the old `[WddmReady; 8]` batch array possible, which is why the one-at-a-time
/// taker is a precondition of this encoding rather than a stylistic choice.
#[must_use = "a popped WDDM fence must be delivered or requeued, never dropped"]
pub struct WddmReady {
    pending: WddmPending,
    /// Kept as a compact prefix until the interrupt callback succeeds. Taking
    /// a WDDM entry never removes terminals, so requeue cannot lose a batch.
    terminal_prefix: Option<WindowedBltTerminalPrefix>,
}

/// The outcome of one attempt to retire the head of the WDDM pending FIFO.
///
/// It used to be `Option<WddmReady>`, which collapsed "nothing queued", "the
/// app's own work is still running" and "the host has not read the frame we
/// published" into one `None`. The last of those is the only one the caller can
/// do something about — it can ask for the read it is waiting on — and it is
/// also the only one whose rate proves the 0ab-B gate is live rather than inert.
#[must_use = "a Ready outcome carries a fence that must be delivered or requeued"]
pub enum WddmTake {
    /// The FIFO is empty.
    Empty,
    /// The head's producer (Venus) watermark has not been reached.
    BlockedOnProducer,
    /// Deliver `DXGK_INTERRUPT_DMA_COMPLETED` for this submission.
    Ready(WddmReady),
}

impl WddmReady {
    pub fn fence(&self) -> u32 {
        self.pending.fence
    }

    pub(crate) fn terminal_prefix(&self) -> Option<WindowedBltTerminalPrefix> {
        self.terminal_prefix
    }

    /// Consume the token after `DMA_COMPLETED` was delivered successfully.
    /// Exists so the success path *states* that it consumed the fence rather
    /// than letting it fall out of scope.
    pub fn delivered(self) {}
}

/// Result of [`VirtioGpu::fence_wait_prepare`].
pub enum FenceWaitPrep {
    /// The fence already completed (or the id predates the tracked window).
    Complete,
    /// Registered; wait on the block's event.
    Registered,
    /// The id was never assigned by this transport instance.
    Invalid,
    /// Waiter table full — retry after a short PASSIVE sleep.
    TableFull,
}

/// Result of [`VirtioGpu::blob_map_begin`].
pub enum BlobMapBegin {
    /// Already mapped — here is the existing mapping.
    Mapped(BlobMapPrep),
    /// Range reserved; the caller must run the RESOURCE_MAP_BLOB round-trip and
    /// then call [`VirtioGpu::blob_map_finish`].
    Start { offset: u64, len: u64 },
    /// Another mapper's round-trip is in flight — retry after a PASSIVE sleep.
    Busy,
    /// Unknown resource / no host-visible window / size out of range / window
    /// exhausted.
    Failed(VirtioError),
}

/// Result of [`VirtioGpu::blob_remap_begin`] (fixed-offset, VidMm-dictated maps
/// for the CPU-visible BAR memory segment — `build_paging_buffer.rs`).
pub enum BlobRemapBegin {
    /// Already mapped at exactly the requested offset — nothing to do.
    Mapped(BlobMapPrep),
    /// Reserved. The caller must (1) RESOURCE_UNMAP_BLOB if `old` is
    /// `Some((offset, len))` and return that range via
    /// [`VirtioGpu::free_window_range_pub`] (a no-op for VidMm-partition
    /// offsets), (2) RESOURCE_MAP_BLOB at the new offset, then (3) call
    /// [`VirtioGpu::blob_map_finish`] with the new offset.
    Start { old: Option<(u64, u64)>, len: u64 },
    /// Another mapper's round-trip is in flight — retry after a PASSIVE sleep.
    Busy,
    /// Unknown resource / out-of-partition target / bad size.
    Failed(VirtioError),
}

/// Result of [`VirtioGpu::blob_map_finish`].
pub enum BlobMapFinish {
    /// Mapping recorded.
    Done(BlobMapPrep),
    /// The host rejected the map (range returned to the allocator).
    HostRejected,
    /// The slot vanished mid-map (owner teardown raced the round-trip): the
    /// caller must issue RESOURCE_UNMAP_BLOB and return the range via
    /// [`VirtioGpu::free_window_range_pub`].
    SlotGone,
}

/// An initialized virtio-gpu transport.
pub struct VirtioGpu {
    /// The virtio-modern PCI transport (owns the mapped cfg-region VAs).
    transport: PciTransport,
    /// Control virtqueue (queue 0) — all GPU commands ride this.
    control: VirtQueue<WdkHal, CTRL_QUEUE_SIZE>,
    /// Next virtio-gpu 3D context id to hand out (guest-assigned; 0 is the
    /// reserved global context, so we start at 1). Phase 3.
    next_ctx_id: u32,
    /// Next virtio-gpu resource id to hand out (0 is reserved). Phase 3 (M3.5).
    next_resource_id: u32,
    /// Host-visible blob window (SHARED_MEMORY_CFG/HOST_VISIBLE BAR), discovered
    /// in `init`. `None` if the device exposes no host-visible window — the WDDM
    /// blob-map path is then unavailable (Stage 2 fails honestly). Gate 5a Stage 2.
    host_visible: Option<HostVisibleWindow>,
    /// Mapped kernel VA of the virtio ISR-status register (read-to-clear), or 0 if
    /// the device exposes no ISR cap. `DxgkDdiInterruptRoutine` reads this at DIRQL
    /// to acknowledge the line-based INTx (the device is `MSISupported=0`). See
    /// [`map_isr_status_register`].
    isr_status_va: usize,
    /// Tracked blobs (resource_id → size/mapping state). Heap-reserved to MAX_BLOBS
    /// at init so `push` under the spinlock never reallocates (the 0x7F lesson).
    blobs: Vec<BlobSlot>,
    /// Blob-table slots reserved by in-flight (multi-phase) creates, counted
    /// against MAX_BLOBS so a burst of concurrent creates cannot overshoot the
    /// reserved capacity (push under the spinlock must never reallocate).
    blobs_reserved: usize,
    /// Every host-live virtio resource id created through this transport.
    /// Removal is one-shot and gates CTX_DETACH_RESOURCE/RESOURCE_UNREF, avoiding
    /// qemu `RESOURCE_UNREF: resource does not exist` errors from duplicate DDI
    /// teardown paths.
    resources: Vec<u32>,
    /// Live-resource slots reserved by in-flight creates (see `blobs_reserved`).
    resources_reserved: usize,
    /// Context tracking slots reserved by in-flight CTX_CREATEs. Tracking is
    /// MANDATORY (see `reserve_context_slot`), so a context is reserved before
    /// the wire round-trip and committed after it.
    contexts_reserved: usize,
    /// Live virtio-gpu contexts, tagged with the owning device handle, so
    /// `DxgkDdiDestroyDevice` can `CTX_DESTROY` any context an ICD created but did
    /// not tear down (crash / skipped CTX_DESTROY) — otherwise leaked contexts
    /// accumulate host-side state and eventually wedge the render server. Reserved
    /// to MAX_CONTEXTS at init (no realloc under the spinlock).
    contexts: Vec<ContextSlot>,
    /// Offsets inside the host-visible BAR window. See [`WindowAllocator`].
    window: WindowAllocator,
    /// In-flight control-queue entries (token-matched; capacity MAX_INFLIGHT,
    /// reserved at init — pushes never reallocate under the spinlock).
    inflight: Vec<InFlight>,
    /// Completed entries awaiting a PASSIVE reap (`swap_parked`). Capacity
    /// MAX_PARKED, reserved at init.
    parked: Vec<InFlight>,
    /// Empty pre-reserved vectors swapped through the PASSIVE reaper. This
    /// removes two kernel-heap allocations from every tiny Venus completion.
    parked_spare: Vec<InFlight>,
    reap_buffers_spare: Vec<DmaBuffer>,
    reap_in_progress: bool,
    /// PASSIVE-reaped DMA buffers ready for another command. Accessed under the
    /// existing virtio spinlock, but allocation/free never occurs there.
    dma_pool: Vec<DmaBuffer>,
    dma_pool_bytes: usize,
    /// The command buffers the DISPATCH-level fast bind may use (ROADMAP defect
    /// 0ab-C, D1(ii)), allocated at transport init and recycled by the drain
    /// forever after.
    ///
    /// TAKING A BUFFER IS THE GATE — no companion counter, because a counter and
    /// a buffer can disagree and these cannot. An empty pool means
    /// [`BIND_CMD_POOL`] binds are already outstanding (the flip arm counts
    /// `FpBusy` and falls through to the worker's bind, i.e. today's behaviour),
    /// or that the allocations failed at init, in which case the accelerator is
    /// simply off for this transport generation.
    ///
    /// Depth 4 rather than 1 since 22.22.221.0: a buffer is only returned by the
    /// guest's DPC drain, which lags the host's consume by several flip periods
    /// under load, so one buffer left ~18 % of flips uncovered. See
    /// [`BIND_CMD_POOL`].
    ///
    /// ⚠ A `Vec` RATHER THAN A `[Option<DmaBuffer>; 4]`, and that is a measured
    /// requirement, not a preference. Before `VirtioGpu` became heap-returned,
    /// it was built in `DxgkDdiStartDevice`'s frame, which the T3 kernel-stack
    /// overflow left with ~160 bytes of headroom; the inline array's 128 bytes
    /// took the boot chain to 18128 against the 17936-byte known-good ceiling
    /// (`tools/kmd-frame-sizes.ps1`, and it fails the build gate). Three words
    /// of `Vec` header put the buffers on the heap instead.
    ///
    /// Capacity is reserved ONCE at init and `len` never exceeds
    /// [`BIND_CMD_POOL`], so the pushes below never reallocate under the device
    /// spinlock — the same discipline `inflight`/`parked`/`dma_pool` follow, and
    /// for the same reason (the 0x7F lesson).
    bind_cmd_pool: Vec<DmaBuffer>,
    /// Fixed fast-bind handoff state. It owns both the newest host-accepted
    /// bind awaiting application and the single unready producer-bound request.
    /// See [`FastBindState`].
    fast_bind: Box<FastBindState>,
    /// Registered WAIT_FENCE waiters (capacity MAX_FENCE_WAITERS).
    fence_waiters: Vec<FenceWaiter>,
    /// Usermode events awaiting wire-fence retirement (capacity
    /// MAX_FENCE_EVENTS, reserved at init — pushes never reallocate under the
    /// spinlock). Entries hold an object reference each.
    fence_events: Vec<FenceEventEntry>,
    /// Registered present streams. Fixed-size heap storage keeps the large
    /// table out of the StartDevice/VirtioGpu::init stack chain while remaining
    /// allocation-free on registration, tagging, completion, and DISPATCH
    /// marker-readiness paths.
    present_streams: Box<[PresentStreamSlot; MAX_PRESENT_STREAMS]>,
    /// Fresh opaque registration capability.  Zero is never issued.
    next_present_stream_cookie: u64,
    /// Next wire fence id to assign (globally monotonic, starts at 1; 0 is
    /// never a valid wire fence).
    next_wire_fence: u64,
    /// FIRST wire fence id THIS transport generation may hand out — i.e.
    /// `next_wire_fence` as of `init`, before anything was assigned.
    ///
    /// ⛔ IT EXISTS BECAUSE AN UPPER BOUND IS NOT A GENERATION CHECK (A6,
    /// `docs/dx12/PENDING.md` §1). Every id space check in this transport was
    /// `id != 0 && id < next_wire_fence`, and [`NEXT_WIRE_FENCE_BASE`] strides the
    /// range by `WIRE_FENCE_INSTANCE_STRIDE` at every StartDevice — so an id
    /// sampled by a usermode client BEFORE a StopDevice/StartDevice cycle is
    /// billions below the new instance's range and satisfies that test trivially,
    /// while naming a fence this instance never issued. The in-flight scan then
    /// finds nothing at or below it and the dependency is satisfied INSTANTLY.
    /// For a wait that is merely a hint that is harmless; for a WDDM DMA fence it
    /// is a completion reported before the work exists.
    ///
    /// ⚠ `fence_wait_prepare` / `fence_event_register` (`:4390`, `:4433`) share the
    /// same one-sided predicate and are deliberately NOT changed here: their
    /// failure mode is a usermode wait that returns early to the process that
    /// forged the id, and the striding comment above records that arm as the
    /// intended behaviour for a client which survived a device restart. The WDDM
    /// arm is different in kind because dxgkrnl schedules the whole desktop on it.
    wire_fence_base: u64,
    /// WDDM submissions pending on venus completion, FIFO (capacity
    /// MAX_WDDM_PENDING, reserved at init).
    wddm_pending: VecDeque<WddmPending>,
    /// Bounded two-phase WindowedBlt transactions. Both deques reserve at
    /// StartDevice, so Present/Submit/DPC mutations never allocate under the
    /// virtio spinlock.
    windowed_blt: WindowedBltState,
    /// Bounded completion-ordered DWM/primary dirty state.  Its oldest
    /// outstanding marker is retained for liveness while one later marker is
    /// coalesced with exact resource identity; boxed to keep StartDevice's
    /// by-value initialization frame below the kernel-stack headroom.
    /// `DmaGpuFence` (default 1). Retire ordinary (non-paging) WDDM DMA fences
    /// on host GPU COMPLETION instead of host DECODE.
    ///
    /// THE CONTRACT: dxgkrnl reads a DMA fence as "the GPU is finished with this
    /// work", and schedules everything downstream on that — including when a
    /// compositor may read a surface an app has just rendered. Retiring at
    /// decode reports completion while the host GPU is still executing, so
    /// dxgkrnl can advance a flip, or let dwm compose an app's window, over a
    /// buffer that is still being written. That is the black/torn frame, and it
    /// is confined to the region being drawn, which is why it presented as
    /// "only inside the app's window" and as Explorer's late top band.
    ///
    /// It was invisible while the UMD's present path blocked on GPU completion
    /// before publishing a present (`PresentOrder=0`): that made the fence's lie
    /// harmless by ensuring the work really was done before dxgkrnl saw it, at
    /// the cost of removing all CPU/GPU overlap (Fire Strike GT1 158 -> 136 fps,
    /// Combined 25.9 -> 18.8).
    ///
    /// The cost this trades against is the one `RetireDomain::DecodeOnly`'s doc
    /// names: ring >= 1 fences stay in flight for the whole GPU-work duration,
    /// so DMA fences now retire later. That is what the fences are supposed to
    /// mean. 0 restores the old behaviour as the A/B lever.
    ///
    /// This is the ONE reader. `AdapterKnobs` used to carry a second, unread
    /// copy of the same registry value; it was deleted 2026-08-05 so the two
    /// cannot disagree.
    dma_gpu_fence: bool,
    /// `PresentWmk`: gate a WDDM submission that carries a LIVE present stream
    /// boundary on that exact boundary alone, instead of additionally on the
    /// whole `next_wire_fence` backlog. Default 1 since 22.22.244.0; `0` is the
    /// same-boot A/B disable that restores the historical superset.
    /// See the watermark arm in [`Self::note_wddm_submission`].
    present_exact_watermark: bool,
    scanout_refresh: Box<ScanoutRefreshState>,
    /// Ring-corruption latch: set when the used ring returns a token we do not
    /// track or `pop_used` fails structurally. The ring state is then
    /// untrustworthy and every subsequent command fails fast. NOTE: unlike the
    /// old model, a slow host does NOT set this — waiter timeouts abandon
    /// their entry and the transport keeps working.
    failed: bool,
    /// Scanout-0 preferred size `(width, height)` reported by the host in the
    /// `GET_DISPLAY_INFO` reply at `init` (`pmodes[0]`), or `None` if the host
    /// reported nothing usable. The display half uses this as the VidPn mode +
    /// generated-EDID native resolution so we present the size QEMU actually wants
    /// on scanout 0 (instead of a hardcoded guess). Read once by StartDevice.
    display_mode: Option<(u32, u32)>,
}

impl VirtioGpu {
    /// Bring the virtio-gpu device online and prove it with `GET_DISPLAY_INFO`.
    /// `passive` is threaded only to reach `DmaBuffer::new` for the
    /// GET_DISPLAY_INFO scratch page; the rest of bring-up is MMIO and PCI
    /// config access. It is a by-value ZST, so it costs neither a register nor a
    /// stack slot in this measured 3.0 KB frame — see `crate::irql`.
    pub fn init(
        passive: crate::irql::PassiveLevel,
        dxgkrnl: &DXGKRNL_INTERFACE,
    ) -> Result<Box<Self>, VirtioError> {
        // ── M1: discover the device + map BARs through Dxgkrnl ──────────────
        // A miniport doesn't own the bus, so config space is reached via the
        // Dxgkrnl callbacks; the DeviceFunction is a formality (DxgkConfigAccess
        // ignores it and addresses our own device via the DeviceHandle).
        let access = DxgkConfigAccess::new(dxgkrnl);
        let mut root = PciRoot::new(access);
        let device_function = DeviceFunction {
            bus: 0,
            device: 0,
            function: 0,
        };
        let mut transport = PciTransport::new::<WdkHal, _>(&mut root, device_function)
            .map_err(|_| VirtioError::DeviceError)?;

        // ── M2: feature negotiation (VirtIO 1.2 spec §3.1.1) ────────────────
        transport.set_status(DeviceStatus::empty()); // reset
        let mut spins = 0u32;
        while !transport.get_status().is_empty() && spins < 100_000 {
            spins += 1;
            core::hint::spin_loop();
        }
        // The bound was previously indistinguishable from success: the loop fell
        // through to ACKNOWLEDGE either way, so a device that never cleared its
        // status was driven through the whole init as if it had reset. Every
        // later assumption in this function rests on that reset. The sibling
        // GET_DISPLAY_INFO poll below already returns Err on its own bound.
        if !transport.get_status().is_empty() {
            crate::diag::fault(crate::diag::FaultCounter::StVioR, spins);
            return Err(VirtioError::DeviceError);
        }
        transport.set_status(DeviceStatus::ACKNOWLEDGE);
        transport.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);

        let offered = transport.read_device_features();
        let accepted = offered & (HELIOS_REQUIRED_FEATURES | HELIOS_OPTIONAL_FEATURES);
        transport.write_driver_features(accepted);
        transport.set_status(
            DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER | DeviceStatus::FEATURES_OK,
        );
        if !transport.get_status().contains(DeviceStatus::FEATURES_OK)
            || accepted & HELIOS_REQUIRED_FEATURES != HELIOS_REQUIRED_FEATURES
        {
            transport.set_status(DeviceStatus::FAILED);
            return Err(VirtioError::FeatureRejected);
        }

        // ── M3: control virtqueue (queue 0), then DRIVER_OK ─────────────────
        // Spell out the error arm instead of `map_err(...)?`. In the measured
        // dev KMD, the combinator materialized and copied the queue-sized
        // `Result` through several init-frame slots. There is no error payload
        // to preserve here, and an early return still drops `transport` before
        // DRIVER_OK exactly as the combinator did.
        let mut control = match VirtQueue::<WdkHal, CTRL_QUEUE_SIZE>::new(
            &mut transport,
            CTRL_QUEUE,
            /* indirect */ false,
            /* event_idx */ false,
        ) {
            Ok(control) => control,
            Err(_) => return Err(VirtioError::DeviceError),
        };
        // Runtime ctrl completion is interrupt-driven. Be explicit instead of
        // relying on the freshly-zeroed avail.flags value: bit 0 clear asks the
        // device to interrupt after it adds a used element.
        control.set_dev_notify(true);
        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );

        // ── M4: GET_DISPLAY_INFO polled round-trip (smoke test) ─────────────
        // Request + response live in one contiguous page so each buffer is
        // physically contiguous for the device (our Hal::share is identity — no
        // bounce buffer). Halves are disjoint (split_at_mut): request is read by
        // the device, response is written by it. The page is a local RAII
        // `DmaBuffer` — the runtime paths own per-command buffers instead
        // (C3/M3.4), so no shared scratch survives init.
        // The fast bind's command buffers, minted HERE because `DmaBuffer::new`
        // is `MmAllocateContiguousMemory` (PASSIVE-only) and the path that uses
        // them runs at DISPATCH under the device spinlock. A `None` is not a
        // failure: that slot is simply never available, and a fully empty pool
        // degrades to every flip arm counting `FpBusy`, which is exactly the
        // pre-0ab-C behaviour.
        //
        // The capacity is reserved here, at PASSIVE, so every later `push` under
        // the device spinlock is reallocation-free. A short pool (an allocation
        // that failed) is a degraded accelerator, never an error.
        let mut bind_cmd_pool = Vec::with_capacity(BIND_CMD_POOL);
        for _ in 0..BIND_CMD_POOL {
            let Some(buf) = DmaBuffer::new(passive, BIND_CMD_BYTES) else {
                break;
            };
            bind_cmd_pool.push(buf);
        }

        let mut scratch = DmaBuffer::new(passive, SCRATCH_BYTES).ok_or(VirtioError::OutOfMemory)?;
        let buf = scratch.as_mut_slice();
        let (req_buf, resp_buf) = buf.split_at_mut(SCRATCH_BYTES / 2);

        let hdr_len = core::mem::size_of::<VirtioGpuCtrlHdr>();
        let resp_len = core::mem::size_of::<VirtioGpuRespDisplayInfo>();
        let mut req = VirtioGpuCtrlHdr::zeroed();
        req.type_ = VIRTIO_GPU_CMD_GET_DISPLAY_INFO;
        req_buf[..hdr_len].copy_from_slice(bytemuck::bytes_of(&req));

        // Bounded inline round-trip (`Self` does not exist yet, so the
        // `ctrl_queue_bounded_roundtrip` helper is unavailable): a host that
        // never answers GET_DISPLAY_INFO must fail StartDevice cleanly, not
        // hang it forever. PASSIVE_LEVEL, no spinlock held.
        {
            let inputs: &[&[u8]] = &[&req_buf[..hdr_len]];
            let outputs: &mut [&mut [u8]] = &mut [&mut resp_buf[..resp_len]];
            // SAFETY: the scratch-page buffers stay valid for the whole block;
            // on timeout we bail out of init and never reuse this queue.
            let token =
                unsafe { control.add(inputs, outputs) }.map_err(|_| VirtioError::DeviceError)?;
            if control.should_notify() {
                transport.notify(CTRL_QUEUE);
            }
            let mut spins = 0u64;
            while !control.can_pop() {
                spins += 1;
                if spins >= CTRL_POLL_SPINS {
                    return Err(VirtioError::DeviceError);
                }
                core::hint::spin_loop();
            }
            // SAFETY: same buffers as `add`, still valid; `can_pop()` was true.
            unsafe { control.pop_used(token, inputs, outputs) }
                .map_err(|_| VirtioError::DeviceError)?;
        }

        let resp: &VirtioGpuRespDisplayInfo = bytemuck::from_bytes(&resp_buf[..resp_len]);
        if !resp_is_ok(resp.hdr.type_) {
            return Err(VirtioError::DeviceError);
        }
        crate::kmsg(c"Helios: virtio-gpu GET_DISPLAY_INFO OK\n");
        // Remember scanout 0's host-preferred size for the display half's VidPn
        // mode + generated EDID. QEMU reports it in `pmodes[0].r` even before a
        // scanout is bound; take it only when both dimensions look sane (a
        // 0×0 / not-yet-configured scanout falls back to the default in
        // StartDevice). Recorded so the host's report is visible live.
        let m0 = resp.pmodes[0].r;
        let display_mode =
            if m0.width >= 320 && m0.height >= 240 && m0.width <= 16384 && m0.height <= 16384 {
                Some((m0.width, m0.height))
            } else {
                None
            };
        crate::diag::record_named_bytes(
            b"DpInf",
            (m0.width.min(0xFFFF) << 16) | (m0.height & 0xFFFF),
        );

        // Discover the host-visible blob window (a fresh config accessor — the
        // original `access` was moved into `PciRoot` above; `DxgkConfigAccess` is
        // a cheap Copy of the device handle + callbacks). Gate 5a Stage 2.
        let host_visible = scan_host_visible_window(&DxgkConfigAccess::new(dxgkrnl));
        crate::diag::record(if host_visible.is_some() {
            0x0B00_0005
        } else {
            0x0B00_00E5
        });

        // Locate + map the ISR-status register so the (real) ISR can read-to-clear
        // the level-triggered INTx line and stop the unhandled-interrupt storm.
        let isr_status_va = map_isr_status_register(&DxgkConfigAccess::new(dxgkrnl));
        crate::diag::record(if isr_status_va != 0 {
            0x0B00_0006
        } else {
            0x0B00_00E6
        });
        // A map failure here is NOT a benign degrade on this INTx device: with no
        // ISR ack the level-triggered line stays asserted and Windows' interrupt
        // storm detector Code-43s the adapter. Report it ungated.
        let mmio_fails = crate::virtio::hal::MMIO_MAP_FAILS.load(Ordering::Relaxed);
        if mmio_fails != 0 {
            crate::diag::fault(crate::diag::FaultCounter::StIsr, mmio_fails);
        }

        // This transport generation's wire-fence range, claimed ONCE and stored in
        // two fields: `next_wire_fence` moves, `wire_fence_base` does not. Both
        // must come from the same `fetch_add`, so it happens here rather than
        // inside the struct literal below.
        let wire_fence_base =
            NEXT_WIRE_FENCE_BASE.fetch_add(WIRE_FENCE_INSTANCE_STRIDE, Ordering::Relaxed);

        // `VirtioGpu` contains the control virtqueue and many ownership tables.
        // Return it heap-owned so StartDevice never reserves a second by-value
        // copy of this large state while `init`'s own frame is live.
        let gpu = Box::new(Self {
            transport,
            control,
            next_ctx_id: 1,
            next_resource_id: 1,
            host_visible,
            isr_status_va,
            blobs: Vec::with_capacity(MAX_BLOBS),
            blobs_reserved: 0,
            resources: Vec::with_capacity(MAX_RESOURCES),
            resources_reserved: 0,
            contexts_reserved: 0,
            contexts: Vec::with_capacity(MAX_CONTEXTS),
            window: WindowAllocator::new(host_visible.map_or(0, |w| w.len)),
            inflight: Vec::with_capacity(MAX_INFLIGHT),
            parked: Vec::with_capacity(MAX_PARKED),
            parked_spare: Vec::with_capacity(MAX_PARKED),
            reap_buffers_spare: Vec::with_capacity(2 * MAX_PARKED),
            reap_in_progress: false,
            dma_pool: Vec::with_capacity(MAX_DMA_POOL),
            dma_pool_bytes: 0,
            bind_cmd_pool,
            fast_bind: allocate_fast_bind_state(),
            fence_waiters: Vec::with_capacity(MAX_FENCE_WAITERS),
            fence_events: Vec::with_capacity(MAX_FENCE_EVENTS),
            present_streams: allocate_present_streams(),
            next_present_stream_cookie: 1,
            // NOT 1. Wire fence ids arrive from an untrusted usermode buffer at
            // the WAIT_FENCE escape, and `fence_wait_prepare` /
            // `fence_event_register` / the WDDM boundary arm all decide against
            // the ordinal predicate `id < next_wire_fence && not in-flight`.
            // Restarting the id space at 1 on every transport init lets a stale id
            // from a PREVIOUS instance — an ICD that survived a `pnputil
            // /restart-device` still holding fences — ALIAS a live id of the new
            // instance, so a waiter could park against completely unrelated work
            // that happens to occupy the same number. Striding the base by
            // `WIRE_FENCE_INSTANCE_STRIDE` at each init makes the id ranges
            // disjoint, which removes the aliasing. Behaviour within one instance
            // is unchanged; the predicate is sound there, because next_wire_fence
            // is bumped only after `control.add` succeeds, in the same spinlock
            // section as the `inflight` push.
            //
            // ⛔ THIS COMMENT USED TO CLAIM the stride *"moves those ids into the
            // `>= next_wire_fence` Invalid arm"*. THAT IS BACKWARDS and it was
            // load-bearing: the stride moves the base UP, so a pre-restart id is
            // billions BELOW the live range and lands in the `< next_wire_fence`
            // arm — the one that reads "already complete". The stride buys
            // disjointness, never rejection. Rejection needs the two-sided bound
            // against `wire_fence_base`, which is why that field exists (A6).
            next_wire_fence: wire_fence_base,
            wire_fence_base,
            wddm_pending: VecDeque::with_capacity(MAX_WDDM_PENDING),
            windowed_blt: WindowedBltState::new(),
            // Snapshotted at transport init like every other knob, so
            // `reg add` + `pnputil /restart-device` flips it with no reboot.
            dma_gpu_fence: crate::diag::read_config_dword(crate::diag::knobs::DMA_GPU_FENCE, 1)
                != 0,
            present_exact_watermark: crate::diag::read_config_dword(
                crate::diag::knobs::PRESENT_EXACT_WATERMARK,
                1,
            ) != 0,
            scanout_refresh: allocate_scanout_refresh_state(),
            failed: false,
            display_mode,
        });
        // `WddmHoldMs` (UV1's instrument). Snapshotted here with every other knob
        // so `reg add` + `pnputil /restart-device` applies it with no reboot, and
        // CLAMPED here rather than trusted: see `WDDM_HOLD_MS_MAX`.
        WDDM_HOLD_MS.store(
            crate::diag::read_config_dword(crate::diag::knobs::WDDM_HOLD_MS, 0)
                .min(WDDM_HOLD_MS_MAX),
            Ordering::Relaxed,
        );
        // `WddmHeadMs` (K-F2 / A5's consumer-side head bound). Snapshotted with
        // every other knob, and CLAMPED IN BOTH DIRECTIONS rather than trusted: too
        // large reinstates the unbounded head and hence the TDR, too small turns a
        // last-resort rebase into a continuous early-fence generator (0ab-B). 0
        // stays 0 — it is the A/B disable and must remain reachable.
        let head_ms =
            crate::diag::read_config_dword(crate::diag::knobs::WDDM_HEAD_MS, WDDM_HEAD_MS_DEFAULT);
        WDDM_HEAD_MS.store(
            helios_kmd_logic::wddm_head_bound::clamp_bound_ms(
                head_ms,
                WDDM_HEAD_MS_MIN,
                WDDM_HEAD_MS_MAX,
            ),
            Ordering::Relaxed,
        );
        // (The old Gate-2 venus ctx self-test is gone: the StartDevice venus
        // client bring-up right after transport init exercises the full context
        // + blob lifecycle for real.)

        // Read-to-clear the ISR-status register once: the GET_DISPLAY_INFO
        // round-trip above completed via the polled path, which never touches
        // this register, so the device may still be asserting INTx from that
        // completion. Clear it now (PASSIVE) so the line starts deasserted
        // before the interrupt-driven runtime paths take over.
        if gpu.isr_status_va != 0 {
            // SAFETY: `isr_status_va` is the mapped MMIO VA of the 1-byte
            // read-to-clear ISR-status register; a volatile read clears it.
            let _ = unsafe { core::ptr::read_volatile(gpu.isr_status_va as *const u8) };
        }

        // `scratch` (the init round-trip page) drops here — the descriptor
        // chain it backed was popped above, so the device no longer references
        // it.
        drop(scratch);
        Ok(gpu)
    }

    // ── C3/M3.4 queue machinery ──────────────────────────────────────────────
    //
    // Every method here runs under the AdapterContext virtio spinlock at
    // DISPATCH_LEVEL and NEVER waits, allocates, or frees DMA memory. Waiting
    // happens in `virtio::ctrl` (PASSIVE, KEVENT); freeing happens when a
    // PASSIVE caller reaps the parked list.

    /// Ring-corruption latch (a wedged-slow host does NOT set this).
    pub fn transport_failed(&self) -> bool {
        self.failed
    }

    /// Enqueue a synchronous control command. `meta` = `[in0 | in1? | resp]`
    /// (one contiguous DMA buffer); the entry owns it until completion. The
    /// waiter's block is signaled (resp copied in) by [`Self::drain_used`].
    /// On `QueueFull` the buffer is handed back for a PASSIVE retry.
    /// The device-facing half of every enqueue: the corruption latch, the
    /// capacity gate, the descriptor spans, and `control.add` with its two
    /// distinct failure arms.
    ///
    /// R1005. All three entry points repeated this verbatim -- the `failed`
    /// check, `inflight.len() >= MAX_INFLIGHT || parked.len() >=
    /// PARKED_ENQUEUE_GATE` with its QUEUE_FULL_RETRIES bump, the
    /// `chain.spans(..)` refusal, and the `QueueFull` vs corruption-latch split
    /// on `add`.
    ///
    /// The DmaBuffers are BORROWED, not consumed: the three entry points hand
    /// them back to their callers in two different tuple shapes
    /// (`(DmaBuffer, VirtioError)` and `(DmaBuffer, DmaBuffer, VirtioError)`),
    /// and threading that through a shared signature would need either a panic
    /// or a third shape nobody wants. Each caller keeps ownership and maps this
    /// bare `VirtioError` into its own tuple.
    ///
    /// # Safety of the spans
    /// The device-visible slices handed to `control.add` alias `meta`/`venus`,
    /// which the caller moves into the `InFlight` entry on the success path;
    /// the entry owns them until the matching `pop_used`. Moving a `DmaBuffer`
    /// moves the owning struct, not the DMA bytes. The borrows end at `add`.
    fn enqueue_core(
        &mut self,
        chain: Chain,
        meta: &DmaBuffer,
        venus: Option<&DmaBuffer>,
        resp_len: usize,
    ) -> Result<u16, VirtioError> {
        if self.failed {
            return Err(VirtioError::DeviceError);
        }
        if self.inflight.len() >= MAX_INFLIGHT || self.parked.len() >= PARKED_ENQUEUE_GATE {
            QUEUE_FULL_RETRIES.fetch_add(1, Ordering::Relaxed);
            return Err(VirtioError::QueueFull);
        }
        let Some((reads, count, resp)) = chain.spans(meta, venus, resp_len) else {
            return Err(VirtioError::DeviceError);
        };
        // SAFETY: see the function's Safety note.
        let added = unsafe {
            let reads = [reads[0].as_slice(), reads[1].as_slice()];
            self.control
                .add(&reads[..count], &mut [resp.as_mut_slice()])
        };
        match added {
            Ok(token) => Ok(token),
            Err(virtio_drivers::Error::QueueFull) => {
                QUEUE_FULL_RETRIES.fetch_add(1, Ordering::Relaxed);
                Err(VirtioError::QueueFull)
            }
            Err(_) => {
                self.latch_failed_and_fail_inflight();
                Err(VirtioError::DeviceError)
            }
        }
    }

    /// Publish the in-flight entry, THEN ring the device doorbell.
    ///
    /// R1005, and k-gputransport-17. The order is the point, and it was not
    /// uniform: `enqueue_async_control` and `enqueue_submit_inner` pushed first
    /// and notified last, each carrying a comment about a fast host completing
    /// into the ISR/DPC on another CPU -- while `enqueue_sync` notified FIRST
    /// and pushed afterwards. A rule stated in prose, in two copies, and
    /// violated in the third.
    ///
    /// The comment was also misstated. The hazard it describes is not live:
    /// `drain_used` is the only used-ring consumer and every one of its call
    /// sites runs inside `adapter.with_virtio`, so it takes the same spinlock
    /// this call is already holding and cannot interleave between the doorbell
    /// and the push. THE REAL INVARIANT is that the token must be in `inflight`
    /// before this spinlock is released. Having one implementation makes that
    /// structural instead of repeated, so a future change that moves
    /// `drain_used` off `virtio_lock` cannot reintroduce the race in exactly
    /// one of three places.
    fn publish_then_notify(&mut self, entry: InFlight) {
        self.inflight.push(entry);
        bump_high_water(&INFLIGHT_HIGH_WATER, self.inflight.len());
        if self.control.should_notify() {
            self.transport.notify(CTRL_QUEUE);
        }
    }

    pub fn enqueue_sync<F>(
        &mut self,
        meta: DmaBuffer,
        in0_len: usize,
        in1_len: usize,
        resp_len: usize,
        waiter: NonNull<SyncWaitBlock>,
        scanout_bind: Option<(u32, Option<ScanoutBindRequest>)>,
        mint_sequence: F,
    ) -> Result<(SyncTicket, Option<u64>), (DmaBuffer, VirtioError)>
    where
        F: FnOnce(u32) -> u64,
    {
        // The shape is decided ONCE, here, and carried on the entry; the drain
        // no longer re-derives it from `in1_len > 0`.
        let chain = if in1_len > 0 {
            Chain::Meta2 { in0_len, in1_len }
        } else {
            Chain::Meta1 { in0_len }
        };
        // Validate BEFORE the capacity gate, exactly as the old summed-total
        // check did: a malformed request must report DeviceError even when the
        // queue also happens to be full. The `t <= meta.as_slice().len()` test
        // this replaces now lives in DmaBuffer::span, per span rather than on
        // the sum. `enqueue_core` re-checks `failed` first, as this path did.
        if in0_len == 0 || resp_len == 0 || resp_len > SYNC_RESP_MAX {
            return Err((meta, VirtioError::DeviceError));
        }
        // A synchronous worker reserves its request before reaching this point.
        // Enforce the monotonic descriptor floor BEFORE the transaction-busy
        // gate, so an old request is terminally superseded rather than sleeping
        // and retrying until the current host-reader transaction ends.
        if scanout_bind.is_some_and(|(_, request)| {
            request.is_some_and(|request| self.presentation_epoch_is_superseded(request))
        }) {
            return Err((meta, VirtioError::PresentationSuperseded));
        }
        // This second check is the reader-lifecycle wire invariant. Refuse
        // before descriptor add so a pre-wire error clears only the worker
        // reservation, never an active host-reader transaction.
        // A presentation transaction also fences adapter-owned fallback SETs:
        // they change QEMU's selected reader just as surely as a direct
        // presentation does.  SET(0) is the one deliberate exception; its
        // caller has already proved FIFO retirement for the named allocation
        // and it is the explicit terminal unbind for this transaction.
        if scanout_bind.is_some_and(|(resource_id, _)| resource_id != 0)
            && self.publication_active()
        {
            return Err((meta, VirtioError::PublicationBusy));
        }
        let token = match self.enqueue_core(chain, &meta, None, resp_len) {
            Ok(token) => token,
            Err(e) => return Err((meta, e)),
        };
        // ⚠ This path used to notify BEFORE pushing; it now publishes first,
        // like the other two. Unobservable -- both happen inside one hold of
        // `virtio_lock`, which `drain_used` also takes -- and deliberate.
        // The tag is assembled after `enqueue_core` accepted the descriptor but
        // before publication/notification, under this same virtio-lock hold.
        // Therefore no sequence is minted for a refused command and no late
        // completion can observe an untagged accepted SET.
        let scanout_bind = scanout_bind.map(|(resource_id, request)| SyncScanoutBind {
            seq: mint_sequence(resource_id),
            resource_id,
            request,
        });
        if let Some(bind) = scanout_bind {
            if let Some(request) = bind.request {
                let claimed = self.claim_publication(request, bind.seq);
                debug_assert!(claimed, "accepted sync SET without publication transaction");
                self.note_presentation_set_accepted(request);
            }
        }
        let scanout_bind_seq = scanout_bind.map(|bind| bind.seq);
        self.publish_then_notify(InFlight {
            token,
            kind: InFlightKind::Sync {
                waiter: Some(waiter),
                scanout_bind,
            },
            meta,
            chain,
            resp_len,
            venus: None,
        });
        Ok((SyncTicket { token }, scanout_bind_seq))
    }

    /// Enqueue a control command without a blocking waiter.  Completion still
    /// consumes and validates the device response in [`Self::drain_used`], owns
    /// `meta` until then, clears the adapter-owned `completion` gate, and wakes
    /// `wake_event`.  The pointed-to objects must remain live until transport
    /// teardown; the scanout caller uses fields embedded in `AdapterContext`,
    /// whose lifetime encloses the virtio transport.
    pub fn enqueue_async_control(
        &mut self,
        meta: DmaBuffer,
        in0_len: usize,
        resp_len: usize,
        completion: NonNull<AtomicU32>,
        completion_errors: NonNull<AtomicU32>,
        wake_event: NonNull<KEVENT>,
        success_store: Option<(NonNull<AtomicU32>, u32)>,
        resubmit: Option<NonNull<AtomicU32>>,
        scanout_flush: Option<ScanoutFlushToken>,
    ) -> Result<(), (DmaBuffer, VirtioError)> {
        let chain = Chain::Meta1 { in0_len };
        // Validated before the capacity gate, as in enqueue_sync.
        if in0_len == 0 || resp_len == 0 || resp_len > SYNC_RESP_MAX {
            return Err((meta, VirtioError::DeviceError));
        }
        // Arm before accepting the descriptor, while this virtio-lock hold
        // still excludes `drain_used`.  If `enqueue_core` refuses it below we
        // roll this pre-publication arm back to SetSucceeded; after acceptance
        // the phase is already FlushInFlight before publication/doorbell, so a
        // very fast completion can never observe the old phase.
        let armed_publication = if let Some(flush) = scanout_flush.as_ref() {
            let (_, present_epoch, resource_id) = flush.trace_context();
            match self.publication_refresh_for(resource_id) {
                // The lock was released between the refresh worker's initial
                // classification and this enqueue. Reclassify here: a newly
                // claimed SET must not let the old flush slip behind it.
                PublicationRefresh::NoActive => None,
                PublicationRefresh::ReadyExact(request)
                    if request.present_epoch == present_epoch =>
                {
                    Some((resource_id, present_epoch))
                }
                PublicationRefresh::ReadyExact(_) | PublicationRefresh::Blocked => {
                    return Err((meta, VirtioError::QueueFull));
                }
            }
        } else {
            None
        };
        if let Some((resource_id, present_epoch)) = armed_publication {
            if !self.arm_publication_flush(resource_id, present_epoch) {
                // No descriptor has reached the queue.  Return a normal
                // pre-wire failure; the caller's exact cancellation path owns
                // the unchanged SetSucceeded transaction.
                crate::ddi::scanout_trace::note_fast_bind_error();
                return Err((meta, VirtioError::DeviceError));
            }
        }
        let token = match self.enqueue_core(chain, &meta, None, resp_len) {
            Ok(token) => token,
            Err(e) => {
                if let Some((resource_id, present_epoch)) = armed_publication {
                    let rolled_back = self.rollback_publication_flush(resource_id, present_epoch);
                    debug_assert!(
                        rolled_back,
                        "refused exact flush did not roll back transaction"
                    );
                }
                return Err((meta, e));
            }
        };
        ASYNC_CTRL_COUNT.fetch_add(1, Ordering::Relaxed);
        if let Some(flush) = scanout_flush.as_ref() {
            let (flush_id, covers_epoch, resource_id) = flush.trace_context();
            // Record descriptor acceptance while `virtio_lock` still excludes
            // `drain_used`, and before the doorbell below. Emitting this after
            // returning to the PASSIVE caller let a fast host response record
            // FLUSH_COMPLETE first even though the wire order was correct.
            crate::ddi::scanout_timeline::note(
                crate::ddi::scanout_timeline::kind::FLUSH_PUBLISH,
                crate::ddi::scanout_timeline::flag::SUCCESS,
                covers_epoch,
                0,
                flush_id,
                resource_id,
                0,
            );
        }
        self.publish_then_notify(InFlight {
            token,
            kind: InFlightKind::AsyncControl {
                completion,
                completion_errors,
                wake_event,
                success_store,
                resubmit,
                scanout_flush,
            },
            meta,
            chain,
            resp_len,
            venus: None,
        });
        Ok(())
    }

    /// Enqueue the DISPATCH-level fast bind: one fire-and-forget
    /// `SET_SCANOUT_BLOB` for the flip being armed (ROADMAP defect 0ab-C,
    /// D1(ii)).
    ///
    /// DISPATCH_LEVEL by construction — a `&mut VirtioGpu` exists only inside
    /// `with_virtio`, i.e. under the device spinlock. Nothing here allocates,
    /// waits, or writes the registry: the command buffer is the preallocated
    /// singleton, and the wire command is written straight into it.
    ///
    /// `seq` must have been minted under THIS lock hold (see
    /// `AdapterContext::mint_scanout_bind_seq`), which is what makes sequence
    /// order equal control-queue order.
    ///
    /// On any refusal the buffer goes back in its slot, so a failure costs
    /// nothing but the counter: the PASSIVE worker's own bind is still armed and
    /// is still the recovery path, exactly as it is with the accelerator off.
    pub fn enqueue_scanout_bind_async(
        &mut self,
        mut buf: DmaBuffer,
        req: &ScanoutBindRequest,
        mint_sequence: impl FnOnce(u32) -> u64,
    ) -> Result<(), FastBindRefusal> {
        // Enforce the monotonic descriptor floor at the wire boundary too:
        // ready/immediate and deferred promotions share this function, and a
        // later accepted epoch must make every older request terminal.
        if self.presentation_epoch_is_superseded(*req) {
            self.return_bind_cmd_buffer(buf);
            return Err(FastBindRefusal::Superseded);
        }
        // The caller stages under this same lock, but keep the refusal local to
        // the wire publication too: no second descriptor may be accepted while
        // the first SET still owns its exact host-reader transaction.
        if self.publication_active() {
            self.return_bind_cmd_buffer(buf);
            return Err(FastBindRefusal::Busy);
        }
        let in0_len = core::mem::size_of::<VirtioGpuSetScanoutBlob>();
        let resp_len = core::mem::size_of::<VirtioGpuCtrlHdr>();
        // The buffer is recycled across binds, so its logical length is reset
        // per use exactly like the DMA pool's; capacity was proved at init.
        if !buf.reset(in0_len + resp_len) {
            self.return_bind_cmd_buffer(buf);
            return Err(FastBindRefusal::Failed);
        }
        {
            // Written IN the device-visible buffer, not staged on this DISPATCH
            // stack: `try_from_bytes_mut` is the checked cast (it returns Err
            // rather than panicking on a bad size/alignment, and a panic in this
            // path would be a bugcheck inside SubmitCommand). The DMA buffer is
            // page-aligned, so the alignment arm cannot fire in practice.
            let Ok(cmd) = bytemuck::try_from_bytes_mut::<VirtioGpuSetScanoutBlob>(
                &mut buf.as_mut_slice()[..in0_len],
            ) else {
                self.return_bind_cmd_buffer(buf);
                return Err(FastBindRefusal::Failed);
            };
            super::ctrl::fill_set_scanout_blob(
                cmd,
                req.resource_id,
                req.width,
                req.height,
                req.format,
                req.stride,
                req.offset,
            );
        }
        let chain = Chain::Meta1 { in0_len };
        let token = match self.enqueue_core(chain, &buf, None, resp_len) {
            Ok(token) => token,
            Err(_) => {
                self.return_bind_cmd_buffer(buf);
                return Err(FastBindRefusal::Failed);
            }
        };
        // `enqueue_core` has now accepted the descriptor but it is still not
        // visible to the device. Mint/publish the wire identity immediately
        // before `publish_then_notify`, so an enqueue failure cannot leave a
        // false resource identity that suppresses the PASSIVE fallback.
        let seq = mint_sequence(req.resource_id);
        // `enqueue_core` accepted this descriptor while this lock stayed held,
        // so no other producer can claim the fixed transaction slot between the
        // readiness check above and this publication. Keep the full request —
        // resource plus epoch alone is not enough to reconstruct a late worker
        // bind's geometry/address safely.
        let claimed = self.claim_publication(*req, seq);
        debug_assert!(claimed, "accepted fast SET without publication transaction");
        self.note_presentation_set_accepted(*req);
        self.publish_then_notify(InFlight {
            token,
            kind: InFlightKind::AsyncScanoutBind {
                seq,
                resource_id: req.resource_id,
                wh: ((req.width as u64) << 32) | req.height as u64,
                format: req.format,
                stride: req.stride,
                offset: req.offset,
                present_epoch: req.present_epoch,
                primary_address: req.primary_address,
                carried_watermark: req.carried_watermark,
            },
            meta: buf,
            chain,
            resp_len,
            venus: None,
        });
        crate::ddi::scanout_timeline::note(
            crate::ddi::scanout_timeline::kind::FAST_SET_PUBLISH,
            0,
            req.present_epoch,
            req.carried_watermark,
            seq,
            req.resource_id,
            // Auxiliary only: exact primary address remains the flip-arm
            // identity, never this truncated diagnostic field.
            req.primary_address as u32,
        );
        Ok(())
    }

    #[inline]
    fn publication_key(request: ScanoutBindRequest) -> helios_kmd_logic::scanout_publish_txn::Key {
        helios_kmd_logic::scanout_publish_txn::Key {
            resource_id: request.resource_id,
            present_epoch: request.present_epoch,
        }
    }

    #[inline]
    fn presentation_epoch_admission(
        &self,
        request: ScanoutBindRequest,
    ) -> helios_kmd_logic::scanout_presentation_epoch::Admission {
        if request.resource_id == 0 {
            helios_kmd_logic::scanout_presentation_epoch::Admission::Untracked
        } else {
            helios_kmd_logic::scanout_presentation_epoch::decide(
                self.fast_bind.presentation_epoch_floor,
                request.present_epoch,
            )
        }
    }

    #[inline]
    fn presentation_epoch_is_superseded(&self, request: ScanoutBindRequest) -> bool {
        self.presentation_epoch_admission(request)
            == helios_kmd_logic::scanout_presentation_epoch::Admission::Superseded
    }

    /// Advance the monotonic admission floor only after `enqueue_core` accepted
    /// this direct presentation SET descriptor. Completion/error/retirement do
    /// not alter it: the host FIFO has already seen this epoch's position.
    fn note_presentation_set_accepted(&mut self, request: ScanoutBindRequest) {
        if self.presentation_epoch_admission(request)
            == helios_kmd_logic::scanout_presentation_epoch::Admission::Newer
        {
            self.fast_bind.presentation_epoch_floor =
                helios_kmd_logic::scanout_presentation_epoch::advance_after_accept(
                    self.fast_bind.presentation_epoch_floor,
                    request.present_epoch,
                );
        }
    }

    #[inline]
    pub fn publication_active(&self) -> bool {
        self.fast_bind.publication.active().is_some()
    }

    /// Claim the fixed host-reader transaction after a SET descriptor was
    /// accepted.  The key/sequence state is policy-tested in `kmd_logic`; this
    /// side stores the complete value needed to apply a late sync response.
    fn claim_publication(&mut self, request: ScanoutBindRequest, seq: u64) -> bool {
        if !self
            .fast_bind
            .publication
            .claim(Self::publication_key(request), seq)
        {
            return false;
        }
        self.fast_bind.publication_request = Some(request);
        true
    }

    fn complete_publication_set(
        &mut self,
        request: ScanoutBindRequest,
        seq: u64,
        ok: bool,
    ) -> bool {
        if self.fast_bind.publication_request != Some(request)
            || !self
                .fast_bind
                .publication
                .complete_set(Self::publication_key(request), seq, ok)
        {
            return false;
        }
        if !ok {
            self.fast_bind.publication_request = None;
        }
        true
    }

    /// The exact flush was accepted after the bind application armed it.
    pub fn arm_publication_flush(&mut self, resource_id: u32, present_epoch: u64) -> bool {
        let key = helios_kmd_logic::scanout_publish_txn::Key {
            resource_id,
            present_epoch,
        };
        self.fast_bind.publication_request.is_some_and(|request| {
            Self::publication_key(request) == key && self.fast_bind.publication.arm_flush(key)
        })
    }

    fn rollback_publication_flush(&mut self, resource_id: u32, present_epoch: u64) -> bool {
        let key = helios_kmd_logic::scanout_publish_txn::Key {
            resource_id,
            present_epoch,
        };
        self.fast_bind.publication_request.is_some_and(|request| {
            Self::publication_key(request) == key && self.fast_bind.publication.rollback_flush(key)
        })
    }

    /// Terminal host-read response.  A mismatched flush is deliberately inert:
    /// it may not release the publication that owns another resource/epoch.
    pub fn complete_publication_flush(&mut self, resource_id: u32, present_epoch: u64) -> bool {
        let key = helios_kmd_logic::scanout_publish_txn::Key {
            resource_id,
            present_epoch,
        };
        if !self.fast_bind.publication.complete_flush(key) {
            return false;
        }
        self.fast_bind.publication_request = None;
        true
    }

    /// Exact terminal path where no flush can or will exist: a stale accepted
    /// bind, host-unbound/dead refresh, enqueue failure, or cancelled worker.
    pub fn cancel_publication_exact(&mut self, resource_id: u32, present_epoch: u64) -> bool {
        let key = helios_kmd_logic::scanout_publish_txn::Key {
            resource_id,
            present_epoch,
        };
        if !self.fast_bind.publication.cancel_exact(key) {
            return false;
        }
        self.fast_bind.publication_request = None;
        true
    }

    /// Snapshot the full active publication only when it names this exact
    /// bound resource. The caller uses its epoch to create a coherent flush
    /// token; sampling `active_scanout_resource` and `scanout_bound_epoch`
    /// separately can otherwise pair a newly stored resource with an older
    /// epoch during bind application.
    pub fn publication_request_for(&self, resource_id: u32) -> Option<ScanoutBindRequest> {
        self.fast_bind
            .publication_request
            .filter(|request| request.resource_id == resource_id)
    }

    /// Classify a prospective refresh against the one exact host-reader
    /// transaction.  Only a SetSucceeded transaction for this exact request is
    /// allowed to create the first flush; every other active state retains the
    /// queue gate until its terminal response/cancellation.
    pub fn publication_refresh_for(&self, resource_id: u32) -> PublicationRefresh {
        let Some(transaction) = self.fast_bind.publication.active() else {
            return PublicationRefresh::NoActive;
        };
        let Some(request) = self.fast_bind.publication_request else {
            return PublicationRefresh::Blocked;
        };
        if transaction.key != Self::publication_key(request) || request.resource_id != resource_id {
            return PublicationRefresh::Blocked;
        }
        if transaction.phase == helios_kmd_logic::scanout_publish_txn::Phase::SetSucceeded {
            PublicationRefresh::ReadyExact(request)
        } else {
            PublicationRefresh::Blocked
        }
    }

    /// A successful scanout-disable is an explicit terminal unbind for the
    /// retiring resource. Resource ids are allocation identities and are not
    /// recycled while this transport generation is live; the full request stays
    /// in the slot until this exact lifecycle barrier confirms the unbind.
    pub fn cancel_publication_for_retirement(&mut self, resource_id: u32) -> bool {
        let Some(request) = self.fast_bind.publication_request else {
            return false;
        };
        if request.resource_id != resource_id {
            return false;
        }
        self.cancel_publication_exact(request.resource_id, request.present_epoch)
    }

    /// Stage a fast bind behind its exact producer boundary, or enqueue it now
    /// when that boundary is already retired. This includes a validated D4b
    /// snapshot: keeping SET publication producer-ordered prevents a successor
    /// SET from overtaking this epoch's producer-gated exact refresh. Both
    /// decisions and the eventual descriptor publication run under
    /// `virtio_lock`; the stored request is values only, so it does not extend a
    /// WDDM allocation lifetime.
    pub fn stage_scanout_bind<F>(
        &mut self,
        request: ScanoutBindRequest,
        mint_sequence: F,
    ) -> FastBindDispatch
    where
        F: FnOnce(u32) -> u64,
    {
        // An exact synchronous worker owner has already claimed the same
        // descriptor path. Preserve that ownership even though its accepted
        // epoch now equals the floor: it is not a second SET attempt and must
        // not have its successful reservation cleared by fast staging.
        if self.fast_bind.sync_worker_owned == Some(request) {
            return FastBindDispatch::Handled;
        }
        if self.presentation_epoch_is_superseded(request) {
            if self.fast_bind.sync_worker_owned == Some(request) {
                self.fast_bind.sync_worker_owned = None;
            }
            if self.fast_bind.deferred_worker == Some(request) {
                self.fast_bind.deferred_worker = None;
            }
            self.note_fast_bind_epoch_superseded(request);
            return FastBindDispatch::Superseded;
        }
        if self.fast_bind.retire_barrier
            || request.resource_id == self.fast_bind.retiring_resource
            || !self.scanout_bind_boundary_live(request.carried_watermark)
        {
            // A tagged producer was explicitly torn down. It must not be
            // rebased into a successful fast bind; the normal worker remains
            // the recovery path and will apply its own cancellation contract.
            return FastBindDispatch::Failed;
        }
        if self.publication_active() || !self.scanout_boundary_ready(request.carried_watermark) {
            // A ready successor must not overtake a SET whose exact host-reader
            // transaction is still live. Retain the bounded oldest+latest
            // frontier exactly as for a producer-unready request; completion of
            // the active transaction is the only promotion edge.
            if self.fast_bind.deferred_earliest.is_none() {
                self.fast_bind.deferred_earliest = Some(request);
            } else if let Some(replaced) = self.fast_bind.deferred_latest.replace(request) {
                // The first slot is a liveness frontier.  Only its trailing
                // companion coalesces, so a 5--10 ms producer cannot be
                // perpetually replaced by 4.5 ms presents.
                crate::ddi::scanout_trace::note_fast_bind_coalesced();
                crate::ddi::scanout_timeline::note(
                    crate::ddi::scanout_timeline::kind::DEFERRED_REPLACED,
                    crate::ddi::scanout_timeline::flag::REPLACED
                        | crate::ddi::scanout_timeline::flag::FAST_FRONTIER,
                    replaced.present_epoch,
                    replaced.carried_watermark,
                    replaced.primary_address,
                    replaced.resource_id,
                    request.resource_id,
                );
            }
            return FastBindDispatch::Deferred;
        }
        self.enqueue_ready_scanout_bind(request, mint_sequence)
    }

    /// Re-evaluate the bounded earliest+latest frontier after a used-ring
    /// retirement.
    /// Called by the completion DPC after `drain_used`, so a ready producer
    /// binds before any exact refresh is armed from the bind response.
    pub fn service_deferred_scanout_bind<F>(&mut self, mint_sequence: F) -> Option<FastBindDispatch>
    where
        F: FnOnce(u32) -> u64,
    {
        // The floor moves when a SET descriptor is accepted, while these two
        // slots may have been retained much earlier for producer completion.
        // Prune before the transaction-active return so a 1055-style stale
        // frontier cannot survive the newer 1056 descriptor's lifetime.
        let superseded = self.discard_superseded_fast_bind_frontier();
        if self.fast_bind.retire_barrier {
            return superseded.then_some(FastBindDispatch::Superseded);
        }
        if self.publication_active() {
            return superseded.then_some(FastBindDispatch::Superseded);
        }
        let discarded = self.discard_invalid_fast_bind_frontier();
        let Some(earliest) = self.fast_bind.deferred_earliest else {
            return if superseded {
                Some(FastBindDispatch::Superseded)
            } else {
                discarded.then_some(FastBindDispatch::Failed)
            };
        };
        if !self.scanout_boundary_ready(earliest.carried_watermark) {
            return None;
        }

        // The frontier proved progress. Issue its exact oldest request first,
        // and retain any live successor regardless of whether its producer is
        // ready yet. If the successor is already ready, this SET's completion
        // queues the next DPC that issues it; dropping it here would silently
        // erase a producer that already proved readiness.
        let latest = self.fast_bind.deferred_latest.take();
        self.fast_bind.deferred_earliest = latest.filter(|request| {
            request.resource_id != self.fast_bind.retiring_resource
                && self.scanout_bind_boundary_live(request.carried_watermark)
        });
        let request = earliest;
        if self.fast_bind.sync_worker_owned == Some(request) {
            return Some(FastBindDispatch::Handled);
        }
        let dispatched = self.enqueue_ready_scanout_bind(request, mint_sequence);
        if matches!(
            dispatched,
            FastBindDispatch::Busy | FastBindDispatch::Failed
        ) {
            // The selected ready request belongs to the existing synchronous
            // worker recovery path now. Do not put it back behind an unready
            // frontier: that would duplicate/reorder it on a later DPC.
            self.fast_bind.deferred_worker = Some(request);
        }
        Some(dispatched)
    }

    /// Freeze all new fast SETs while one exact resource is retired, and cancel
    /// pre-wire requests. Returns the newest host-accepted bind sequence and
    /// resource; a control-FIFO barrier lets the caller turn that into the final
    /// host selection before deciding whether scanout-disable is necessary.
    pub fn begin_scanout_resource_retire(&mut self, resource_id: u32) -> (u64, u32) {
        self.fast_bind.retiring_resource = resource_id;
        self.fast_bind.retire_barrier = true;
        if self
            .fast_bind
            .deferred_worker
            .is_some_and(|request| request.resource_id == resource_id)
        {
            self.fast_bind.deferred_worker = None;
        }
        if self
            .fast_bind
            .sync_worker_owned
            .is_some_and(|request| request.resource_id == resource_id)
        {
            self.fast_bind.sync_worker_owned = None;
        }
        // The existing WDDM pending slot is the authoritative newest fallback.
        // No value-only request may survive the lifecycle barrier and later bind
        // behind that worker, so discard both frontier positions, not just the
        // retiring identity.
        self.fast_bind.deferred_earliest = None;
        self.fast_bind.deferred_latest = None;
        if self
            .fast_bind
            .fast_failure_wake
            .is_some_and(|request| request.resource_id == resource_id)
        {
            self.fast_bind.fast_failure_wake = None;
        }
        (
            self.fast_bind.host_accepted_seq,
            self.fast_bind.host_accepted_resource,
        )
    }

    /// Snapshot the host selection established by successful bind responses.
    /// Call after a successful control-FIFO barrier when an issued bind was not
    /// yet represented by the snapshot from `begin_scanout_resource_retire`.
    pub fn host_accepted_scanout_bind(&self) -> (u64, u32) {
        (
            self.fast_bind.host_accepted_seq,
            self.fast_bind.host_accepted_resource,
        )
    }

    /// Re-open the DISPATCH fast path after retirement has either established a
    /// safe final selection or conservatively retained the host resource.
    pub fn finish_scanout_resource_retire(&mut self) {
        self.fast_bind.retire_barrier = false;
    }

    /// Remove the oldest frontier and promote the sole coalesced successor.
    /// All callers are under `virtio_lock`; this is fixed-state movement only.
    fn promote_latest_frontier(&mut self) {
        self.fast_bind.deferred_earliest = self.fast_bind.deferred_latest.take();
    }

    /// Remove every dead/retiring request from the two-slot frontier without
    /// orphaning a surviving latest request. The loop is statically bounded by
    /// the two physical slots and performs only fixed-state movement.
    fn discard_invalid_fast_bind_frontier(&mut self) -> bool {
        let mut discarded = false;
        for _ in 0..2 {
            let Some(request) = self.fast_bind.deferred_earliest else {
                break;
            };
            if request.resource_id != self.fast_bind.retiring_resource
                && self.scanout_bind_boundary_live(request.carried_watermark)
            {
                break;
            }
            self.promote_latest_frontier();
            discarded = true;
        }
        if self.fast_bind.deferred_latest.is_some_and(|request| {
            request.resource_id == self.fast_bind.retiring_resource
                || !self.scanout_bind_boundary_live(request.carried_watermark)
        }) {
            self.fast_bind.deferred_latest = None;
            discarded = true;
        }
        discarded
    }

    /// Drop every deferred fast request at or below the accepted presentation
    /// descriptor floor. The fixed two-slot frontier remains ordered: if the
    /// old head is stale and the trailing request is newer, it is promoted to
    /// the head in this same virtio-lock hold.
    fn discard_superseded_fast_bind_frontier(&mut self) -> bool {
        let earliest = self.fast_bind.deferred_earliest.take();
        let latest = self.fast_bind.deferred_latest.take();
        let mut discarded = false;
        let mut kept_earliest = None;
        let mut kept_latest = None;

        for request in [earliest, latest].into_iter().flatten() {
            if self.presentation_epoch_is_superseded(request) {
                self.note_fast_bind_epoch_superseded(request);
                discarded = true;
            } else if kept_earliest.is_none() {
                kept_earliest = Some(request);
            } else {
                kept_latest = Some(request);
            }
        }
        self.fast_bind.deferred_earliest = kept_earliest;
        self.fast_bind.deferred_latest = kept_latest;
        discarded
    }

    /// One explicit diagnostic edge per request discarded by the monotonic
    /// descriptor floor. It is neither the trailing-slot coalescing counter nor
    /// a host-reader cancellation: this request never reached the control FIFO.
    fn note_fast_bind_epoch_superseded(&self, request: ScanoutBindRequest) {
        crate::ddi::scanout_trace::note_fast_bind_superseded();
        crate::ddi::scanout_timeline::note(
            crate::ddi::scanout_timeline::kind::DEFERRED_SUPERSEDED,
            crate::ddi::scanout_timeline::flag::SUPERSEDED
                | crate::ddi::scanout_timeline::flag::FAST_FRONTIER,
            request.present_epoch,
            request.carried_watermark,
            request.primary_address,
            request.resource_id,
            self.fast_bind.presentation_epoch_floor as u32,
        );
    }

    /// Record a SET_SCANOUT_BLOB response as host-visible before its DPC
    /// bookkeeping is handed off.  The sequence is control-FIFO order.
    pub fn note_host_accepted_scanout_bind(&mut self, seq: u64, resource_id: u32) {
        if seq >= self.fast_bind.host_accepted_seq {
            self.fast_bind.host_accepted_seq = seq;
            self.fast_bind.host_accepted_resource = resource_id;
            self.fast_bind.host_accepted_fast = false;
            self.fast_bind.host_accepted_fast_request = None;
        }
    }

    fn note_host_accepted_fast_scanout_bind(&mut self, seq: u64, request: ScanoutBindRequest) {
        if seq >= self.fast_bind.host_accepted_seq {
            self.fast_bind.host_accepted_seq = seq;
            self.fast_bind.host_accepted_resource = request.resource_id;
            self.fast_bind.host_accepted_fast = true;
            self.fast_bind.host_accepted_fast_request = Some(request);
        }
    }

    /// Gate the PASSIVE worker's synchronous fallback on the same exact
    /// producer boundary as the fast path. The worker retains the WDDM handle
    /// and is woken by the completion DPC; this state holds values only.
    ///
    /// `reserve_sync_set` is false only when the caller has already observed
    /// this target bound. It still performs every liveness, fast-owner, and
    /// producer-boundary check, but does not claim a synchronous SET that the
    /// caller will deliberately not issue.
    pub fn stage_worker_scanout_bind(
        &mut self,
        request: ScanoutBindRequest,
        reserve_sync_set: bool,
    ) -> WorkerBindDispatch {
        if self.fast_bind.retire_barrier
            || request.resource_id == self.fast_bind.retiring_resource
            || !self.resource_is_live(request.resource_id)
        {
            if self.fast_bind.deferred_worker == Some(request) {
                self.fast_bind.deferred_worker = None;
            }
            return WorkerBindDispatch::Abandoned;
        }
        if !self.scanout_bind_boundary_live(request.carried_watermark) {
            if self.fast_bind.deferred_worker == Some(request) {
                self.fast_bind.deferred_worker = None;
            }
            return WorkerBindDispatch::Abandoned;
        }
        if self.presentation_epoch_is_superseded(request) {
            if self.fast_bind.deferred_worker == Some(request) {
                self.fast_bind.deferred_worker = None;
            }
            if self.fast_bind.sync_worker_owned == Some(request) {
                self.fast_bind.sync_worker_owned = None;
            }
            return WorkerBindDispatch::Superseded;
        }
        // A host-visible SET (fast or synchronous) owns the exact reader
        // transaction until its response and, on success, its exact flush
        // terminate. The worker must wait behind that transaction even when its
        // own producer is ready; otherwise it could publish a later SET before
        // the host has read the first binding.
        if self.publication_active()
            || self.fast_bind.sync_worker_owned == Some(request)
            || self.fast_owns_request(request)
        {
            self.fast_bind.deferred_worker = Some(request);
            return WorkerBindDispatch::Waiting;
        }
        if self.scanout_boundary_ready(request.carried_watermark) {
            // Claim under the same transport lock the fast stage uses. This
            // closes the pre-existing-HPD race where both producers observed no
            // owner and published duplicate SETs for one exact flip.
            // A spurious worker wake can re-check a request before the DPC has
            // consumed it; a ready request has no reason to remain deferred.
            if self.fast_bind.deferred_worker == Some(request) {
                self.fast_bind.deferred_worker = None;
            }
            if reserve_sync_set {
                self.fast_bind.sync_worker_owned = Some(request);
            }
            return WorkerBindDispatch::Ready;
        }
        self.fast_bind.deferred_worker = Some(request);
        WorkerBindDispatch::Waiting
    }

    /// True once the retained PASSIVE fallback can be retried without binding
    /// ahead of its producer.  Consumed by the DPC, which wakes the worker.
    pub fn take_ready_worker_scanout_bind(&mut self) -> bool {
        let Some(request) = self.fast_bind.deferred_worker else {
            return false;
        };
        // `service_deferred_scanout_bind` may have published the fast command
        // earlier in this same DPC. Re-check here so the worker never wakes to
        // place a synchronous SET behind that exact command.
        if self.publication_active() || self.fast_owns_request(request) {
            return false;
        }
        if self.presentation_epoch_is_superseded(request) {
            // Wake the PASSIVE worker to consume the stale handle through its
            // terminal Superseded outcome, which lowers/reassigns the existing
            // WDDM programming gate without issuing another SET.
            self.fast_bind.deferred_worker = None;
            if self.fast_bind.sync_worker_owned == Some(request) {
                self.fast_bind.sync_worker_owned = None;
            }
            return true;
        }
        if request.resource_id == self.fast_bind.retiring_resource
            || !self.resource_is_live(request.resource_id)
            || !self.scanout_bind_boundary_live(request.carried_watermark)
        {
            self.fast_bind.deferred_worker = None;
            return false;
        }
        if !self.scanout_boundary_ready(request.carried_watermark) {
            return false;
        }
        self.fast_bind.deferred_worker = None;
        true
    }

    pub fn release_fast_owned_worker(&mut self, request: ScanoutBindRequest) {
        if self.fast_bind.host_accepted_fast_request == Some(request) {
            // Keep the host resource/sequence for DestroyAllocation's lifetime
            // barrier; only clear its *worker suppression* role after DPC
            // publication made `already_bound` authoritative.
            self.fast_bind.host_accepted_fast = false;
            self.fast_bind.host_accepted_fast_request = None;
        }
        if self.fast_bind.deferred_worker == Some(request) {
            self.fast_bind.deferred_worker = None;
        }
    }

    /// A synchronous SET failed before it could become the terminal owner of
    /// this exact request. Successful requests intentionally stay cached in
    /// `sync_worker_owned` until the next distinct worker claim.
    pub fn release_failed_sync_worker_bind(&mut self, request: ScanoutBindRequest) {
        if self.fast_bind.sync_worker_owned == Some(request) {
            self.fast_bind.sync_worker_owned = None;
        }
    }

    pub fn take_fast_failure_wake(&mut self) -> Option<ScanoutBindRequest> {
        self.fast_bind.fast_failure_wake.take()
    }

    fn fast_owns_request(&self, request: ScanoutBindRequest) -> bool {
        self.fast_bind.publication_request == Some(request)
            || self
                .fast_bind
                .completed
                .is_some_and(|bind| completed_request(bind) == request)
            || (self.fast_bind.host_accepted_fast
                && self.fast_bind.host_accepted_fast_request == Some(request))
            || self.inflight.iter().any(|entry| match entry.kind {
                InFlightKind::AsyncScanoutBind {
                    resource_id,
                    wh,
                    format,
                    stride,
                    offset,
                    present_epoch,
                    primary_address,
                    carried_watermark,
                    ..
                } => {
                    ScanoutBindRequest {
                        resource_id,
                        width: (wh >> 32) as u32,
                        height: wh as u32,
                        format,
                        stride,
                        offset,
                        present_epoch,
                        primary_address,
                        carried_watermark,
                    } == request
                }
                _ => false,
            })
    }

    fn enqueue_ready_scanout_bind<F>(
        &mut self,
        request: ScanoutBindRequest,
        mint_sequence: F,
    ) -> FastBindDispatch
    where
        F: FnOnce(u32) -> u64,
    {
        // See `stage_scanout_bind`: this exact owner is a previously accepted
        // synchronous descriptor, not a deferred attempt to publish another
        // SET at the now-equal floor.
        if self.fast_bind.sync_worker_owned == Some(request) {
            return FastBindDispatch::Handled;
        }
        if self.presentation_epoch_is_superseded(request) {
            if self.fast_bind.sync_worker_owned == Some(request) {
                self.fast_bind.sync_worker_owned = None;
            }
            if self.fast_bind.deferred_worker == Some(request) {
                self.fast_bind.deferred_worker = None;
            }
            self.note_fast_bind_epoch_superseded(request);
            return FastBindDispatch::Superseded;
        }
        if self.publication_active() {
            return FastBindDispatch::Deferred;
        }
        if self.fast_bind.retire_barrier
            || request.resource_id == self.fast_bind.retiring_resource
            || !self.resource_is_live(request.resource_id)
        {
            return FastBindDispatch::Failed;
        }
        let Some(buffer) = self.take_bind_cmd_buffer() else {
            return FastBindDispatch::Busy;
        };
        match self.enqueue_scanout_bind_async(buffer, &request, mint_sequence) {
            Ok(()) => FastBindDispatch::Queued,
            Err(FastBindRefusal::Busy) => FastBindDispatch::Busy,
            Err(FastBindRefusal::Superseded) => {
                self.note_fast_bind_epoch_superseded(request);
                FastBindDispatch::Superseded
            }
            Err(FastBindRefusal::Failed) => FastBindDispatch::Failed,
        }
    }

    /// Take a fast-bind command buffer, or `None` when all [`BIND_CMD_POOL`] of
    /// them are already in flight. The take IS the gate — see
    /// [`Self::bind_cmd_pool`].
    pub fn take_bind_cmd_buffer(&mut self) -> Option<DmaBuffer> {
        self.bind_cmd_pool.pop()
    }

    /// Put a fast-bind command buffer back, for the enqueue's failure arms.
    ///
    /// Cannot fail in practice: the buffer came out of this pool under the lock
    /// the caller still holds, so the pool is one short of full. The `len` test
    /// is what keeps the push inside the reserved capacity — a reallocation
    /// under the device spinlock is the 0x7F class of bug — and the impossible
    /// arm LEAKS rather than frees, because `DmaBuffer::drop` is
    /// `MmFreeContiguousMemory` and that is PASSIVE-only; the same policy
    /// `PARKED_LEAKS` follows. `FpErr` rising with no host errors is then the
    /// visible signature of the accounting having broken.
    fn return_bind_cmd_buffer(&mut self, buf: DmaBuffer) {
        if self.bind_cmd_pool.len() < BIND_CMD_POOL {
            self.bind_cmd_pool.push(buf);
            return;
        }
        crate::ddi::scanout_trace::note_fast_bind_error();
        core::mem::forget(buf);
    }

    /// Take the newest host-accepted fast bind, for application outside this
    /// lock. See [`CompletedBind`] for why the drain cannot apply it itself.
    pub fn take_completed_bind(&mut self) -> Option<CompletedBind> {
        self.fast_bind.completed.take()
    }

    /// Enqueue an ASYNC fenced SUBMIT_3D and return the KMD-assigned wire
    /// fence id. Returns at queue time — completion arrives on the used ring
    /// (interrupt DPC), which signals WAIT_FENCE waiters and advances the WDDM
    /// pending FIFO. `meta` carries `[SUBMIT_3D hdr | ctrl resp]`; `venus` is
    /// the opaque stream (second device-read descriptor — kept split so the
    /// host never mis-parses the submit header as another control command).
    pub fn enqueue_async_submit(
        &mut self,
        ctx_id: u32,
        ring_idx: u32,
        meta: DmaBuffer,
        venus: DmaBuffer,
        venus_len: usize,
    ) -> Result<u64, (DmaBuffer, DmaBuffer, VirtioError)> {
        self.enqueue_submit_inner(ctx_id, ring_idx, meta, venus, venus_len, None, None, None)
    }

    /// Ring-1 submission belonging to an already admitted WindowedBlt FIFO
    /// entry. Failure is terminalized by the caller while it still owns the
    /// request; a successful used-ring response resolves it by exact token.
    pub fn enqueue_async_submit_windowed_blt(
        &mut self,
        adapter: &crate::adapter::AdapterContext,
        ctx_id: u32,
        meta: DmaBuffer,
        venus: DmaBuffer,
        venus_len: usize,
        token: u64,
        stream_boundary: u64,
    ) -> Result<u64, (DmaBuffer, DmaBuffer, VirtioError)> {
        let known = self.windowed_blt.pending.iter().any(|request| {
            request.token == token
                && request.stream_boundary == stream_boundary
                && request.admitted
                && request.dispatched
        });
        if !known {
            return Err((meta, venus, VirtioError::DeviceError));
        }
        self.enqueue_submit_inner(
            ctx_id,
            SCANOUT_RING_IDX,
            meta,
            venus,
            venus_len,
            None,
            None,
            Some(WindowedBltRetire {
                adapter: NonNull::from(adapter),
                token,
                stream_boundary,
            }),
        )
    }

    /// Enqueue a tagged ICD submit.  Validation and the descriptor add happen
    /// under the same transport lock, so CTX_DESTROY cannot leave an accepted
    /// queue entry carrying a stream that was concurrently purged.
    pub fn enqueue_async_submit_present_stream(
        &mut self,
        owner: DeviceOwner,
        ctx_id: u32,
        ring_idx: u32,
        cookie: u64,
        value: u32,
        meta: DmaBuffer,
        venus: DmaBuffer,
        venus_len: usize,
    ) -> Result<u64, (DmaBuffer, DmaBuffer, VirtioError)> {
        let retire = match self.prepare_present_stream_tag(owner, ctx_id, ring_idx, cookie, value) {
            Ok(retire) => retire,
            Err(error) => return Err((meta, venus, error)),
        };
        self.enqueue_submit_inner(
            ctx_id,
            ring_idx,
            meta,
            venus,
            venus_len,
            None,
            Some(retire),
            None,
        )
    }

    /// Enqueue the scan-out copy: an ASYNC fenced SUBMIT_3D on ring 1 carrying
    /// the notification target whose completion publishes the displayed primary
    /// and clears the programming gate.
    ///
    /// The ring is NOT a parameter. It used to be, independently of the notify,
    /// while the drain only honoured a notify on ring 1 — so
    /// `enqueue_async_submit(ctx, 0, .., Some(notify))` compiled, completed
    /// through the `ring_idx != 1` path, and silently discarded the notify with
    /// no counter and no error. Because that drain is the ONLY clear of
    /// `vidpn_programming` on the copied-primary path, the pending primary and
    /// its programming ownership remained stranded for the rest of the boot.
    /// Making the ring an implicit property of this entry point takes the
    /// `(ring, notify)` mismatch out of the type space entirely.
    pub fn enqueue_scanout_submit(
        &mut self,
        ctx_id: u32,
        meta: DmaBuffer,
        venus: DmaBuffer,
        venus_len: usize,
        notify: ScanoutNotify,
    ) -> Result<u64, (DmaBuffer, DmaBuffer, VirtioError)> {
        self.enqueue_submit_inner(
            ctx_id,
            SCANOUT_RING_IDX,
            meta,
            venus,
            venus_len,
            Some(notify),
            None,
            None,
        )
    }

    /// Shared body of the two entry points above. Private: the notify/ring
    /// pairing is theirs to decide, not a caller's.
    fn enqueue_submit_inner(
        &mut self,
        ctx_id: u32,
        ring_idx: u32,
        mut meta: DmaBuffer,
        venus: DmaBuffer,
        venus_len: usize,
        scanout_notify: Option<ScanoutNotify>,
        present_stream: Option<PresentStreamRetire>,
        windowed_blt: Option<WindowedBltRetire>,
    ) -> Result<u64, (DmaBuffer, DmaBuffer, VirtioError)> {
        let hdr_len = core::mem::size_of::<VirtioGpuCmdSubmit>();
        let resp_len = core::mem::size_of::<VirtioGpuCtrlHdr>();
        if self.failed {
            return Err((meta, venus, VirtioError::DeviceError));
        }
        if venus_len == 0
            || venus_len > venus.as_slice().len()
            || hdr_len + resp_len > meta.as_slice().len()
        {
            return Err((meta, venus, VirtioError::DeviceError));
        }
        let fence_id = self.next_wire_fence;
        let mut cmd = VirtioGpuCmdSubmit::zeroed();
        cmd.hdr.type_ = VIRTIO_GPU_CMD_SUBMIT_3D;
        cmd.hdr.flags = VIRTIO_GPU_FLAG_FENCE;
        cmd.hdr.fence_id = fence_id;
        cmd.hdr.ctx_id = ctx_id;
        if ring_idx != 0 {
            cmd.hdr.flags |= VIRTIO_GPU_FLAG_INFO_RING_IDX;
            cmd.hdr.ring_idx = ring_idx.min(u8::MAX as u32) as u8;
        }
        cmd.size = venus_len as u32;
        meta.as_mut_slice()[..hdr_len].copy_from_slice(bytemuck::bytes_of(&cmd));

        let chain = Chain::MetaPlusVenus { hdr_len, venus_len };
        let token = match self.enqueue_core(chain, &meta, Some(&venus), resp_len) {
            Ok(token) => token,
            Err(e) => return Err((meta, venus, e)),
        };
        if let Some(retire) = present_stream {
            self.commit_present_stream_tag(retire, ring_idx);
        }
        // Stays BETWEEN a successful `add` and the publish: the wire fence id
        // is only spent once the device has actually taken the descriptor.
        self.next_wire_fence += 1;
        let ring = cmd.hdr.ring_idx;
        ASYNC_SUBMIT_COUNT.fetch_add(1, Ordering::Relaxed);
        if ring != 0 {
            RING_SUBMIT_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        self.publish_then_notify(InFlight {
            token,
            kind: InFlightKind::AsyncVenus {
                fence_id,
                ring_idx: ring,
                scanout_notify,
                present_stream,
                windowed_blt,
            },
            meta,
            chain,
            resp_len,
            venus: Some(venus),
        });
        Ok(fence_id)
    }

    /// Set the ring-corruption latch and fail every in-flight entry exactly
    /// once. Call at the moment the latch is set, never later, and always under
    /// the device spinlock.
    ///
    /// Before this, latching `failed` made `drain_used` return immediately and
    /// left every entry in `inflight` forever. `async_retired_up_to` then
    /// reported false for every watermark above those stuck ids,
    /// `note_wddm_submission` never returned "signal now" and `take_ready_wddm`
    /// never popped, so DMA_COMPLETED was never delivered again. ResetFromTimeout
    /// cleared the FIFO but neither the latch nor the stuck entries, so dxgkrnl
    /// resubmitted with fresh ids into the same wedge: a TDR loop. And the
    /// `AsyncControl` entries that own `scanout_flush_inflight` were abandoned
    /// with their completion gates still set, so `queue_active_scanout_refresh`
    /// returned Busy forever and the HPD worker spun its 4 ms lost-interrupt
    /// poll for the rest of the boot. (`scanout_bind_inflight` was the other
    /// such gate until T6/R902 deleted the async bind.)
    ///
    /// Everything here mirrors the success path's ordering exactly, because a
    /// mistake in the Sync-waiter sequence is a use-after-free of a stack block.
    fn latch_failed_and_fail_inflight(&mut self) {
        self.failed = true;
        // Neither a host-accepted completion nor a deferred producer boundary
        // survives a terminal transport failure. Clear both value-only slots so
        // no later DPC can publish bookkeeping from this generation.
        self.fast_bind.completed = None;
        self.fast_bind.publication_request = None;
        self.fast_bind.publication = helios_kmd_logic::scanout_publish_txn::State::new();
        self.fast_bind.deferred_earliest = None;
        self.fast_bind.deferred_latest = None;
        self.fast_bind.host_accepted_seq = 0;
        self.fast_bind.host_accepted_resource = 0;
        self.fast_bind.host_accepted_fast = false;
        self.fast_bind.retiring_resource = 0;
        self.fast_bind.retire_barrier = false;
        self.fast_bind.sync_worker_owned = None;
        self.fast_bind.deferred_worker = None;
        self.fast_bind.fast_failure_wake = None;
        self.fast_bind.host_accepted_fast_request = None;
        while let Some(mut entry) = self.inflight.pop() {
            // Taken out BEFORE the match, so `entry` itself is never partially
            // moved and can still be parked below. `ScanoutFlushToken` is
            // deliberately not `Copy`: completing one twice would count a second
            // host read that never happened.
            let scanout_flush = take_scanout_flush_token(&mut entry.kind);
            match entry.kind {
                InFlightKind::Sync { waiter, .. } => {
                    if let Some(block) = waiter {
                        // No response is copied on purpose: `SyncWaitBlock::new_zeroed`
                        // zeroes `resp`, and `resp_is_ok(0)` is false, so every
                        // waiter observes failure rather than a stale success.
                        //
                        // SAFETY: identical to the success path's Sync arm in
                        // `drain_used`, and audited with it for the 22.22.218.0
                        // `0xA` (ROADMAP defect 0ab-C). The waiter has exactly
                        // two exits and both keep the block alive across these
                        // accesses: the SIGNAL, where the kernel's stack-event
                        // contract forbids resuming before `KeSetEvent` is done
                        // with the dispatcher object; and the TIMEOUT, whose
                        // `abandon_sync` runs under THIS lock and therefore
                        // either cleared `waiter` before us (it is still `Some`,
                        // so it did not) or runs after this arm completes. No
                        // lock-free `done` poll can authorize an exit any more —
                        // that fast path is what made this pattern unsound.
                        // `done` (Release) BEFORE KeSetEvent, both inside the
                        // critical section, for the second exit's half of that
                        // argument.
                        unsafe {
                            let b = block.as_ptr();
                            (*b).done.store(true, Ordering::Release);
                            KeSetEvent(&mut (*b).event, IO_NO_INCREMENT, 0);
                        }
                    }
                }
                InFlightKind::AsyncControl {
                    completion,
                    completion_errors,
                    wake_event,
                    ..
                } => {
                    // The transport is dead: this command has terminated and no
                    // host read can originate from it any more. End its lease
                    // (counted `LsCanc`), or the flip that presented that buffer
                    // never retires and VidSch escalates to a TDR — the same
                    // wedge class this function's own doc paragraph describes.
                    if let Some(token) = scanout_flush {
                        token.complete(false);
                    }
                    // Clear the gate and wake the worker, so the display's
                    // coalescing gates unstick instead of reading Busy forever.
                    // No `success_store`, no `resubmit`: nothing succeeded.
                    // SAFETY: all three name stable AdapterContext fields whose
                    // lifetime encloses this transport entry.
                    unsafe {
                        completion_errors.as_ref().fetch_add(1, Ordering::Relaxed);
                        completion.as_ref().store(0, Ordering::Release);
                        KeSetEvent(wake_event.as_ptr(), IO_NO_INCREMENT, 0);
                    }
                }
                InFlightKind::AsyncScanoutBind { .. } => {
                    // The transport is dead, so this bind never reached the
                    // host: apply NO bookkeeping (that would publish an identity
                    // the host does not have) and stash nothing. Counted as an
                    // error like every other way a fast bind can fail to land.
                    //
                    // The preallocated command buffer parks with the entry
                    // below, and the slot stays empty for the rest of this
                    // transport generation — the next StartDevice builds a fresh
                    // `VirtioGpu` with a fresh buffer. That costs one page after
                    // a transport death, which is not a state anything else
                    // survives either.
                    crate::ddi::scanout_trace::note_fast_bind_error();
                }
                InFlightKind::AsyncVenus {
                    scanout_notify,
                    present_stream: _,
                    windowed_blt,
                    ..
                } => {
                    // A transport latch is an epoch abort, not a producer
                    // retirement. `purge_all_present_streams` below explicitly
                    // cancels every remaining stream; advancing a marker here
                    // would manufacture a successful boundary for work the
                    // host never accepted.
                    if let Some(notify) = scanout_notify {
                        // Publish nothing as displayed - the copy did not happen -
                        // but DO clear the programming gate so this failed
                        // primary does not retain ownership indefinitely.
                        // Ticketed: if a newer interval was raised meanwhile,
                        // that gate is not ours to lower.
                        // SAFETY: stable AdapterContext fields; the adapter owns
                        // this transport and outlives every in-flight entry.
                        unsafe {
                            crate::adapter::clear_programming_gate(
                                notify.programming.as_ref(),
                                notify.ticket,
                            );
                            KeSetEvent(notify.event.as_ptr(), IO_NO_INCREMENT, 0);
                        }
                    }
                    if let Some(retire) = windowed_blt {
                        // The host can no longer read this request after a
                        // terminal transport latch. End the exact reader and
                        // WDDM gate rather than leaking a snapshot slot.
                        self.terminal_windowed_blt(
                            unsafe { retire.adapter.as_ref() },
                            retire.token,
                            retire.stream_boundary,
                            false,
                        );
                    }
                }
            }
            // Park, never free: the host may still be DMAing into these buffers,
            // and DmaBuffer frees are PASSIVE-only. Same policy as the success
            // path.
            if self.parked.len() < MAX_PARKED {
                self.parked.push(entry);
                bump_high_water(&PARKED_HIGH_WATER, self.parked.len());
            } else {
                PARKED_LEAKS.fetch_add(1, Ordering::Relaxed);
                core::mem::forget(entry);
            }
        }

        // No wire fence can ever retire now, so release every parked waiter and
        // usermode event rather than leaving them blocked on a dead transport.
        while let Some(w) = self.fence_waiters.pop() {
            // SAFETY: registered blocks stay valid until deregistration, which
            // happens under this same lock.
            unsafe {
                let b = w.block.as_ptr();
                (*b).done.store(true, Ordering::Release);
                KeSetEvent(&mut (*b).event, IO_NO_INCREMENT, 0);
            }
        }
        // A failed transport cannot satisfy an outstanding stream boundary.
        // Clear the table and any coalesced marker so a future generation never
        // reads an old handle as live.
        self.purge_all_present_streams();
        self.abort_windowed_blt_for_terminal_transport();
        while let Some(e) = self.fence_events.pop() {
            // SAFETY: the entry holds an object reference taken by the escape
            // handler. The deref MUST be deferred: dropping the last reference
            // with a plain deref at DISPATCH would run the object's PASSIVE-only
            // deletion.
            unsafe {
                KeSetEvent(e.event.as_ptr(), IO_NO_INCREMENT, 0);
                ObDereferenceObjectDeferDelete(e.event.as_ptr() as PVOID);
            }
            FENCE_EVENT_SIGNALS.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Drain every completed entry off the used ring: pop the descriptor chain
    /// (token-matched), signal sync/fence waiters, and park the entry for a
    /// PASSIVE reap. The ONLY used-ring consumer (interrupt DPC + opportunistic
    /// callers under the same spinlock).
    pub fn drain_used(&mut self) {
        if self.failed {
            return;
        }
        loop {
            let Some(token) = self.control.peek_used() else {
                return;
            };
            let Some(idx) = self.inflight.iter().position(|e| e.token == token) else {
                // A completion we do not track: the ring state is corrupt.
                DRAIN_BAD_TOKEN.fetch_add(1, Ordering::Relaxed);
                self.latch_failed_and_fail_inflight();
                return;
            };
            // Rebuild the spans through the SAME producer `add` used, so the
            // two lists cannot drift. The result is copied out so no borrow of
            // `self.inflight` is held across the `self.control` call.
            let (spans, resp_len) = {
                let e = &self.inflight[idx];
                (
                    e.chain.spans(&e.meta, e.venus.as_ref(), e.resp_len),
                    e.resp_len,
                )
            };
            // The entry's own spans were proved at enqueue and nothing has
            // resized the buffers since, so this cannot fail; treat it as a
            // corrupt entry rather than assuming.
            let Some((reads, count, resp)) = spans else {
                DRAIN_BAD_TOKEN.fetch_add(1, Ordering::Relaxed);
                self.latch_failed_and_fail_inflight();
                return;
            };
            // SAFETY: exactly the spans `add` was called with; the entry still
            // owns both buffers.
            let popped = unsafe {
                let read_slices = [reads[0].as_slice(), reads[1].as_slice()];
                self.control
                    .pop_used(token, &read_slices[..count], &mut [resp.as_mut_slice()])
            };
            if popped.is_err() {
                self.latch_failed_and_fail_inflight();
                return;
            }
            let mut entry = self.inflight.swap_remove(idx);
            // As in `latch_failed_and_fail_inflight`: take the ownership token
            // out before the `match entry.kind` moves the other fields, so the
            // entry stays whole for the park below.
            let scanout_flush = take_scanout_flush_token(&mut entry.kind);
            let resp_base = {
                // SAFETY: the resp span is within the entry-owned meta buffer.
                unsafe { resp.as_slice() }.as_ptr()
            };
            // First u32 of the device-written response = VIRTIO_GPU_RESP_*.
            // SAFETY: as above; unaligned because the offset is command-shaped.
            let resp_type = unsafe { core::ptr::read_unaligned(resp_base as *const u32) };
            match entry.kind {
                InFlightKind::Sync {
                    waiter,
                    scanout_bind,
                } => {
                    let response_ok = resp_is_ok(resp_type);
                    let waiter_abandoned = waiter.is_none();
                    if let Some(bind) = terminal_sync_scanout_bind(response_ok, scanout_bind) {
                        // This runs even after `abandon_sync` detached the
                        // stack waiter. A successful SET remains host-visible,
                        // so lifecycle retirement must learn its exact wire
                        // identity before deciding whether a disable is needed.
                        self.note_host_accepted_scanout_bind(bind.seq, bind.resource_id);
                        if let Some(request) = bind.request {
                            let terminal =
                                self.complete_publication_set(request, bind.seq, response_ok);
                            debug_assert!(
                                terminal,
                                "sync SET terminal response mismatched transaction"
                            );
                            if response_ok && waiter_abandoned && terminal {
                                // The PASSIVE caller timed out and detached its
                                // stack waiter. The host nevertheless bound this
                                // exact request, so the DPC must apply its full
                                // geometry/epoch and arm the matching flush.
                                self.fast_bind.completed = Some(CompletedBind {
                                    seq: bind.seq,
                                    resource_id: request.resource_id,
                                    wh: ((request.width as u64) << 32) | request.height as u64,
                                    present_epoch: request.present_epoch,
                                    primary_address: request.primary_address,
                                    carried_watermark: request.carried_watermark,
                                    format: request.format,
                                    stride: request.stride,
                                    offset: request.offset,
                                });
                            }
                        }
                    } else if let Some(bind) = scanout_bind {
                        // A direct presentation host-error is terminal for its
                        // exact transaction. Wake the retained worker recovery
                        // path; it may re-stage only after this clear.
                        if let Some(request) = bind.request {
                            let terminal = self.complete_publication_set(request, bind.seq, false);
                            debug_assert!(terminal, "sync SET error mismatched transaction");
                            self.fast_bind.fast_failure_wake = Some(request);
                        }
                    }
                    if let Some(block) = waiter {
                        // THE WRITE SITE THE 22.22.218.0 `0xA` RACED, and the
                        // ordering below is now correct only because
                        // `ctrl::wait_block` no longer has a lock-free `done`
                        // fast path. Keep it that way: `done` is stored one
                        // instruction before the signal, so any exit authorized
                        // by `done` lets the waiter pop the frame these three
                        // accesses are still writing (ROADMAP defect 0ab-C).
                        //
                        // SAFETY: the block outlives every access here, and the
                        // argument is now about the waiter's TWO exits rather
                        // than about deregistration:
                        //   * the SIGNAL — the waiter is inside
                        //     `KeWaitForSingleObject` on `event`, and the
                        //     kernel's stack-event contract says it cannot
                        //     resume before `KeSetEvent` has finished with the
                        //     dispatcher object;
                        //   * the TIMEOUT — `abandon_sync` runs under THIS lock,
                        //     so it either clears `waiter` before this arm runs
                        //     (`waiter` is still `Some`, so it did not) or runs
                        //     after the whole arm and reports AlreadyCompleted;
                        //     its own frame is alive across that call.
                        // Response copied BEFORE the Release store on `done`;
                        // KeSetEvent is DISPATCH-safe (Wait=FALSE). All three
                        // stay inside the critical section for the second exit's
                        // half of the argument.
                        unsafe {
                            let b = block.as_ptr();
                            let n = resp_len.min(SYNC_RESP_MAX);
                            core::ptr::copy_nonoverlapping(
                                resp_base,
                                (*b).resp.get() as *mut u8,
                                n,
                            );
                            (*b).done.store(true, Ordering::Release);
                            KeSetEvent(&mut (*b).event, IO_NO_INCREMENT, 0);
                        }
                    }
                }
                InFlightKind::AsyncScanoutBind {
                    seq,
                    resource_id,
                    wh,
                    format,
                    stride,
                    offset,
                    present_epoch,
                    primary_address,
                    carried_watermark,
                } => {
                    // NO adapter bookkeeping here, on purpose. This runs under
                    // `virtio_lock`, and applying ends in a flush arm that needs
                    // `wddm_notify_lock` — the reverse of the driver's order,
                    // which `end_scanout_leases_through` documents and which is
                    // a DIRQL deadlock, not a lock-contention slowdown. Stash
                    // the values; `drain_used_and_complete` applies them one
                    // frame up, with no transport lock held.
                    let set_ok = resp_is_ok(resp_type);
                    crate::ddi::scanout_timeline::note(
                        crate::ddi::scanout_timeline::kind::FAST_SET_COMPLETE,
                        if set_ok {
                            crate::ddi::scanout_timeline::flag::SUCCESS
                        } else {
                            0
                        },
                        present_epoch,
                        carried_watermark,
                        seq,
                        resource_id,
                        resp_type,
                    );
                    if set_ok {
                        let request = ScanoutBindRequest {
                            resource_id,
                            width: (wh >> 32) as u32,
                            height: wh as u32,
                            format,
                            stride,
                            offset,
                            present_epoch,
                            primary_address,
                            carried_watermark,
                        };
                        let terminal = self.complete_publication_set(request, seq, true);
                        debug_assert!(
                            terminal,
                            "fast SET terminal response mismatched transaction"
                        );
                        self.note_host_accepted_fast_scanout_bind(seq, request);
                        if self.fast_bind.completed.is_some() {
                            // Two binds completed in one drain pass. The newest
                            // is the identity the host is left with, so it wins
                            // — the same coalescing the pending-flip slot does,
                            // and counted for the same reason (`VpCoal` is that
                            // slot's; this is `FpCoal`).
                            crate::ddi::scanout_trace::note_fast_bind_coalesced();
                        }
                        self.fast_bind.completed = Some(CompletedBind {
                            seq,
                            resource_id,
                            wh,
                            present_epoch,
                            primary_address,
                            carried_watermark,
                            format,
                            stride,
                            offset,
                        });
                    } else {
                        // The host refused the bind. Nothing is bound to this
                        // resource, so nothing may be remembered or published;
                        // the PASSIVE worker's own validate/retry ladder is the
                        // recovery path, unchanged.
                        crate::ddi::scanout_trace::note_fast_bind_error();
                        let request = ScanoutBindRequest {
                            resource_id,
                            width: (wh >> 32) as u32,
                            height: wh as u32,
                            format,
                            stride,
                            offset,
                            present_epoch,
                            primary_address,
                            carried_watermark,
                        };
                        let terminal = self.complete_publication_set(request, seq, false);
                        debug_assert!(terminal, "fast SET error mismatched transaction");
                        self.fast_bind.fast_failure_wake = Some(request);
                    }
                }
                InFlightKind::AsyncControl {
                    completion,
                    completion_errors,
                    wake_event,
                    success_store,
                    resubmit,
                    ..
                } => {
                    ASYNC_CTRL_COMPLETE_COUNT.fetch_add(1, Ordering::Relaxed);
                    let response_ok = resp_is_ok(resp_type);
                    // THE CONSUMER EDGE (ROADMAP defect 0ab-B). QEMU's
                    // `RESOURCE_FLUSH` handler is synchronous — the Vulkan
                    // readback submits, waits on its fence, copies the staging
                    // bytes and only then does this response go on the used ring
                    // — so this exact point is "the host has finished reading
                    // the buffer we published". Ending the lease HERE, before
                    // the completion gate is cleared below, means the WDDM pop
                    // that follows in the same DPC already sees it.
                    if let Some(token) = scanout_flush {
                        let (_, covers_epoch, resource_id) = token.trace_context();
                        let exact_publication = self
                            .publication_request_for(resource_id)
                            .is_some_and(|request| request.present_epoch == covers_epoch);
                        token.complete(response_ok);
                        // Success completes the exact host-read transaction.
                        // An error is an explicit exact cancellation: it must
                        // not masquerade as a successful reader completion,
                        // but the returned command cannot create a future read
                        // and therefore may release this request's gate.
                        if exact_publication {
                            let terminal = if response_ok {
                                self.complete_publication_flush(resource_id, covers_epoch)
                            } else {
                                self.cancel_publication_exact(resource_id, covers_epoch)
                            };
                            debug_assert!(
                                terminal,
                                "exact flush terminal response mismatched transaction"
                            );
                        }
                    }
                    if !response_ok {
                        ASYNC_CTRL_RESP_ERRORS.fetch_add(1, Ordering::Relaxed);
                        // SAFETY: adapter-owned atomic; see enqueue contract.
                        unsafe { completion_errors.as_ref() }.fetch_add(1, Ordering::Relaxed);
                    } else if let Some((target, value)) = success_store {
                        // Publish which scanout the host accepted before the
                        // worker consumes the follow-up dirty edge.
                        unsafe { target.as_ref() }.store(value, Ordering::Release);
                    }
                    // Publish completion before waking the coalescing worker.
                    // SAFETY: both pointers refer to stable AdapterContext
                    // fields whose lifetime encloses this transport entry.
                    unsafe {
                        // A rejected SET_SCANOUT_BLOB must not become a
                        // self-sustaining retry loop. New exact-primary/dirty
                        // publication is the only retry trigger; successful
                        // binds re-arm once to issue their first flush.
                        if response_ok {
                            if let Some(pending) = resubmit {
                                pending.as_ref().store(1, Ordering::Release);
                            }
                        }
                        completion.as_ref().store(0, Ordering::Release);
                        KeSetEvent(wake_event.as_ptr(), IO_NO_INCREMENT, 0);
                    }
                }
                InFlightKind::AsyncVenus {
                    fence_id,
                    ring_idx,
                    scanout_notify,
                    present_stream,
                    windowed_blt,
                } => {
                    ASYNC_COMPLETE_COUNT.fetch_add(1, Ordering::Relaxed);
                    if ring_idx != 0 {
                        RING_COMPLETE_COUNT.fetch_add(1, Ordering::Relaxed);
                    }
                    let response_ok = resp_is_ok(resp_type);
                    if !response_ok {
                        ASYNC_RESP_ERRORS.fetch_add(1, Ordering::Relaxed);
                    }
                    // Only a successful host response retires this stream
                    // value.  A rejected tagged submit invalidates the stream
                    // instead; the next ordered WDDM pass explicitly discharges
                    // its waits onto their ordinary wire watermark.  Treating a
                    // rejection as retirement would make `DMA_COMPLETED` claim
                    // that the producer ran when the host said it did not.
                    if let Some(retire) = present_stream {
                        if response_ok {
                            self.retire_present_stream_value(retire);
                        } else {
                            self.fail_present_stream_value(retire);
                        }
                        // An admitted WindowedBlt may have been waiting only
                        // for this exact producer edge. Signal its PASSIVE
                        // worker; readiness remains checked again under lock.
                        self.wake_ready_windowed_blt();
                    }
                    if let Some(retire) = windowed_blt {
                        // SAFETY: every token stores the stable adapter that
                        // constructed it; StopDevice drains/cancels inflight
                        // entries before destroying that adapter.
                        self.complete_windowed_blt_ring(
                            unsafe { retire.adapter.as_ref() },
                            retire.token,
                            retire.stream_boundary,
                            response_ok,
                        );
                    }
                    // ring_idx=1 is the GPU-completion domain. Queue the host
                    // display refresh only after the copy has really completed;
                    // a decode-level or failed submit must never publish pixels.
                    // A notify can now only exist on this ring
                    // (`enqueue_scanout_submit`), so the test is a belt-and-braces
                    // check rather than the sole guard it used to be.
                    if ring_idx == SCANOUT_RING_IDX as u8 {
                        if let Some(notify) = scanout_notify {
                            // SAFETY: both pointers name stable AdapterContext
                            // fields; the adapter owns this transport and outlives
                            // every in-flight entry.
                            unsafe {
                                if response_ok {
                                    notify
                                        .displayed_primary
                                        .as_ref()
                                        .store(notify.primary_address, Ordering::Release);
                                    notify.pending.as_ref().store(1, Ordering::Release);
                                }
                                // Ticketed clear, unconditional on response_ok
                                // exactly as before: a failed copy must still
                                // release OUR interval or VSync stops. What it
                                // must NOT do is release a NEWER interval that a
                                // second SetVidPnSourceAddress raised while this
                                // copy was outstanding — that is the stale clear
                                // this ticket exists to reject.
                                crate::adapter::clear_programming_gate(
                                    notify.programming.as_ref(),
                                    notify.ticket,
                                );
                                KeSetEvent(notify.event.as_ptr(), IO_NO_INCREMENT, 0);
                            }
                        }
                    }
                    // Wake every waiter registered on this wire fence.
                    let mut j = 0;
                    while j < self.fence_waiters.len() {
                        if self.fence_waiters[j].fence_id == fence_id {
                            let w = self.fence_waiters.swap_remove(j);
                            // SAFETY: registered blocks stay valid until
                            // deregistration (`fence_wait_cancel`), which removes
                            // them from this list under the same lock.
                            unsafe {
                                let b = w.block.as_ptr();
                                (*b).done.store(true, Ordering::Release);
                                KeSetEvent(&mut (*b).event, IO_NO_INCREMENT, 0);
                            }
                        } else {
                            j += 1;
                        }
                    }
                    // Signal + consume every usermode fence-event registration
                    // on this wire fence (one-shot). Runs at DISPATCH under the
                    // device spinlock: KeSetEvent (Wait=FALSE) is legal, and the
                    // deref MUST be ObDereferenceObjectDeferDelete — dropping
                    // the LAST reference with a plain deref at DISPATCH would
                    // run the object's PASSIVE-only deletion (the registering
                    // process may have exited and closed its handle).
                    let mut j = 0;
                    while j < self.fence_events.len() {
                        if self.fence_events[j].fence_id == fence_id {
                            let e = self.fence_events.swap_remove(j);
                            // SAFETY: the entry holds an object reference taken
                            // by the escape handler, so `event` is a live KEVENT
                            // regardless of the registering process's fate.
                            unsafe {
                                KeSetEvent(e.event.as_ptr(), IO_NO_INCREMENT, 0);
                                ObDereferenceObjectDeferDelete(e.event.as_ptr() as PVOID);
                            }
                            FENCE_EVENT_SIGNALS.fetch_add(1, Ordering::Relaxed);
                        } else {
                            j += 1;
                        }
                    }
                }
            }
            // The fast bind's buffer is RETAINED, not parked: it is one of the
            // preallocated pool buffers, the device is done with it (`pop_used`
            // returned), and nothing is freed here — which is the only reason
            // parking exists (`DmaBuffer::drop` is PASSIVE-only). Returning it
            // to the pool is also what re-arms the accelerator, and doing it
            // HERE rather than at a PASSIVE reap is the point of the pool: the
            // return is what a flip arm finds, so it must happen as early as the
            // completion does. `venus.is_none()` is belt-and-braces — this kind
            // is created with `venus: None`.
            if matches!(entry.kind, InFlightKind::AsyncScanoutBind { .. }) && entry.venus.is_none()
            {
                if self.bind_cmd_pool.len() < BIND_CMD_POOL {
                    let (meta, _none) = entry.into_dma_buffers();
                    // Inside the reserved capacity by the test above, so no
                    // reallocation under the spinlock.
                    self.bind_cmd_pool.push(meta);
                    continue;
                }
                // IMPOSSIBLE: at most BIND_CMD_POOL binds can be outstanding,
                // and each one holds the buffer it popped, so a completing bind
                // always has room to return to. Reaching this means the pool
                // accounting has broken (a buffer returned twice, or one that
                // never came from here). Park the entry like any other — never
                // free at DISPATCH — and count it: `FpErr` rising while
                // `FpBind`/`FpApply` look healthy and the host reports no errors
                // is the signature.
                crate::ddi::scanout_trace::note_fast_bind_error();
            }
            // Park the entry for a PASSIVE reap (DmaBuffer frees are
            // PASSIVE-only).
            if self.parked.len() < MAX_PARKED {
                self.parked.push(entry);
                bump_high_water(&PARKED_HIGH_WATER, self.parked.len());
            } else {
                // Unreachable given PARKED_ENQUEUE_GATE; never reallocate or
                // free under the spinlock — leak loudly instead.
                PARKED_LEAKS.fetch_add(1, Ordering::Relaxed);
                core::mem::forget(entry);
            }
        }
    }

    /// Number of completed entries awaiting a PASSIVE reap.
    ///
    /// Unused today: `PARKED_LEAKS` is surfaced through the escape's
    /// QUERY_STATS instead, since it has no registry mirror. Kept as the typed
    /// accessor for that population. Pre-dates T6; surfaced when R906 removed
    /// the crate-wide `dead_code` allow over `mod virtio`.
    #[allow(dead_code)]
    pub fn parked_len(&self) -> usize {
        self.parked.len()
    }

    /// Begin one PASSIVE reap by swapping in the empty pre-reserved parked
    /// vector and lending the pre-reserved DMA-buffer scratch vector. A second
    /// caller returns None and leaves the active reaper to finish.
    pub fn begin_parked_reap(&mut self) -> Option<(Vec<InFlight>, Vec<DmaBuffer>)> {
        if self.reap_in_progress || self.parked.is_empty() {
            return None;
        }
        self.reap_in_progress = true;
        let fresh = core::mem::take(&mut self.parked_spare);
        debug_assert!(fresh.capacity() >= MAX_PARKED);
        let dead = core::mem::replace(&mut self.parked, fresh);
        let buffers = core::mem::take(&mut self.reap_buffers_spare);
        debug_assert!(buffers.capacity() >= 2 * MAX_PARKED);
        Some((dead, buffers))
    }

    /// Return the emptied pre-reserved reap vectors after excess DMA buffers
    /// were dropped at PASSIVE_LEVEL.
    pub fn finish_parked_reap(&mut self, mut entries: Vec<InFlight>, mut buffers: Vec<DmaBuffer>) {
        // These were debug_assert-only, i.e. absent from the release driver
        // (kmd_render sets no debug-assertions). They guard the capacity the
        // `parked.push` path relies on to never reallocate under the spinlock,
        // so enforce them for real: clear, and replace any vector whose capacity
        // fell below its reserve with a freshly reserved one. Both allocations
        // happen HERE, at PASSIVE, never under the lock.
        entries.clear();
        buffers.clear();
        if entries.capacity() < MAX_PARKED {
            entries = Vec::with_capacity(MAX_PARKED);
        }
        if buffers.capacity() < 2 * MAX_PARKED {
            buffers = Vec::with_capacity(2 * MAX_PARKED);
        }
        self.parked_spare = entries;
        self.reap_buffers_spare = buffers;
        self.reap_in_progress = false;
    }

    /// Undo [`Self::begin_parked_reap`] on a failure path, restoring both spares
    /// and clearing the in-progress flag.
    ///
    /// Without this, an early return between begin and finish stranded
    /// `reap_in_progress` at true AND left both pre-reserved spares taken, so
    /// reaping was permanently disabled and every later enqueue hit the
    /// `PARKED_ENQUEUE_GATE` refusal. Latent today only because the sole
    /// reachable early return is a concurrent StopDevice, after which the whole
    /// `VirtioGpu` is replaced - but that is a property of today's callers, not
    /// of the protocol.
    pub fn abort_parked_reap(&mut self, entries: Vec<InFlight>, buffers: Vec<DmaBuffer>) {
        REAP_ABANDONED.fetch_add(1, Ordering::Relaxed);
        self.finish_parked_reap(entries, buffers);
    }

    /// Take one already-allocated DMA buffer whose page capacity covers `len`.
    /// The caller allocates a new buffer at PASSIVE only when this returns None.
    pub fn take_dma_buffer(&mut self, len: usize) -> Option<DmaBuffer> {
        // `reset(0)` fails, and the only failure return below dropped the
        // DmaBuffer INSIDE the `with_virtio` closure — i.e. under
        // KeAcquireSpinLockRaiseToDpc — where `DmaBuffer::drop` calls
        // MmFreeContiguousMemory, which hal.rs states is PASSIVE-only. Every
        // sibling API hands buffers back through `Err((DmaBuffer, VirtioError))`
        // precisely so the free happens at PASSIVE; this was the one path that
        // broke the convention. Reject len == 0 up front, and swap-remove only
        // after the capacity filter has proved `reset` will succeed — which
        // also closes the accounting hole where dma_pool_bytes and
        // DMA_POOL_CACHED_BYTES were decremented before the failure return.
        if len == 0 {
            DMA_POOL_MISSES.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let Some((idx, _)) = self
            .dma_pool
            .iter()
            .enumerate()
            .filter(|(_, buf)| buf.capacity() >= len)
            .min_by_key(|(_, buf)| buf.capacity())
        else {
            DMA_POOL_MISSES.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let mut buf = self.dma_pool.swap_remove(idx);
        self.dma_pool_bytes = self.dma_pool_bytes.saturating_sub(buf.capacity());
        DMA_POOL_CACHED_BYTES.store(self.dma_pool_bytes as u32, Ordering::Relaxed);
        // Infallible here: the filter proved capacity >= len, and len > 0.
        if !buf.reset(len) {
            DMA_POOL_MISSES.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        DMA_POOL_HITS.fetch_add(1, Ordering::Relaxed);
        Some(buf)
    }

    /// Move eligible completed buffers into the bounded pool without allocation.
    /// Any excess remains in `buffers` and is returned for PASSIVE-level drop.
    pub fn recycle_dma_buffers(&mut self, mut buffers: Vec<DmaBuffer>) -> Vec<DmaBuffer> {
        let mut i = 0;
        while i < buffers.len() {
            let capacity = buffers[i].capacity();
            let eligible = capacity <= MAX_DMA_POOL_BUFFER_BYTES
                && self.dma_pool.len() < MAX_DMA_POOL
                && self.dma_pool_bytes.saturating_add(capacity) <= MAX_DMA_POOL_BYTES;
            if !eligible {
                DMA_POOL_DROPS.fetch_add(1, Ordering::Relaxed);
                i += 1;
                continue;
            }
            let buf = buffers.swap_remove(i);
            self.dma_pool_bytes += capacity;
            self.dma_pool.push(buf);
        }
        DMA_POOL_CACHED_BYTES.store(self.dma_pool_bytes as u32, Ordering::Relaxed);
        buffers
    }

    /// Abandon a timed-out synchronous entry: detach its waiter so the eventual
    /// completion signals nobody (the entry itself is reaped when it completes).
    /// Returns `true` if the entry had ALREADY completed — the wait raced the
    /// drain and the caller should treat it as success.
    pub fn abandon_sync(
        &mut self,
        ticket: SyncTicket,
        block: NonNull<SyncWaitBlock>,
    ) -> SyncOutcome {
        for e in self.inflight.iter_mut() {
            if e.token != ticket.token {
                continue;
            }
            if let InFlightKind::Sync { waiter, .. } = &mut e.kind {
                if *waiter == Some(block) {
                    *waiter = None;
                    return SyncOutcome::Abandoned;
                }
                // The token is ours but the waiter is a DIFFERENT block, or the
                // entry is not a Sync at all. Either way this waiter's response
                // buffer was never written.
                return SyncOutcome::NotOurs;
            }
            return SyncOutcome::NotOurs;
        }
        SyncOutcome::AlreadyCompleted
    }

    // ── Wire-fence table (WAIT_FENCE) ────────────────────────────────────────

    /// Prepare a wait on wire fence `fence_id`, registering `block` if the
    /// fence is still in flight. Runs under the device spinlock — the
    /// in-flight check and the registration are atomic with respect to
    /// [`Self::drain_used`], so a completion can never fall between them.
    ///
    /// Completion predicate (System-class phase4e model): wire ids are
    /// assigned by this transport, monotonic and never reused, and every
    /// assigned id lives in `inflight` until its used-ring completion — so
    /// `id < next_wire_fence && not in-flight` ⇒ complete.
    ///
    /// ⚠ "ASSIGNED BY THIS TRANSPORT" IS THE LOAD-BEARING WORD, and until
    /// 2026-08-06 nothing tested it: the range check was one-sided, and StartDevice
    /// strides the id space up by 2^32, so an id from a PREVIOUS generation is
    /// below the range and reached the `Complete` arm. The ICD was then told a wire
    /// fence had retired when its whole transport generation was gone — exactly
    /// [`TRANSPORT_GONE_AT_WAIT`]'s failure, reported as success.
    pub fn fence_wait_prepare(
        &mut self,
        fence_id: u64,
        block: NonNull<SyncWaitBlock>,
    ) -> FenceWaitPrep {
        // A failed transport can never retire a fence, so parking a PASSIVE
        // waiter against one is a guaranteed timeout at best.
        if self.failed || fence_id == 0 || fence_id >= self.next_wire_fence {
            return FenceWaitPrep::Invalid;
        }
        if fence_id < self.wire_fence_base {
            FENCE_ID_FOREIGN_GENERATION.fetch_add(1, Ordering::Relaxed);
            return FenceWaitPrep::Invalid;
        }
        let in_flight = self.inflight.iter().any(|e| match e.kind {
            InFlightKind::AsyncVenus { fence_id: f, .. } => f == fence_id,
            _ => false,
        });
        if !in_flight {
            return FenceWaitPrep::Complete;
        }
        if self.fence_waiters.len() >= MAX_FENCE_WAITERS {
            return FenceWaitPrep::TableFull;
        }
        self.fence_waiters.push(FenceWaiter { fence_id, block });
        FENCE_WAIT_REGISTERED.fetch_add(1, Ordering::Relaxed);
        FenceWaitPrep::Registered
    }

    /// Deregister a timed-out fence waiter. Returns `true` if the fence had
    /// ALREADY completed (the drain signaled + removed the waiter first).
    pub fn fence_wait_cancel(&mut self, block: NonNull<SyncWaitBlock>) -> bool {
        if let Some(i) = self.fence_waiters.iter().position(|w| w.block == block) {
            self.fence_waiters.swap_remove(i);
            false
        } else {
            true
        }
    }

    // ── Fence-event table (REGISTER_FENCE_EVENT, KMD 22.22.54) ──────────────

    /// Park `event` for one-shot signaling when wire fence `fence_id` retires.
    /// Runs under the device spinlock — the completion check and the insert
    /// are atomic against [`Self::drain_used`], so a wakeup can never be lost
    /// (same predicate as [`Self::fence_wait_prepare`]: assigned ids live in
    /// `inflight` until their used-ring completion).
    ///
    /// Ownership: on `Registered` the TABLE owns the caller's object
    /// reference (released by the drain / unregister / teardown). On every
    /// other outcome the caller still owns it and must deref.
    pub fn fence_event_register(&mut self, fence_id: u64, event: NonNull<KEVENT>) -> FenceEventReg {
        // As in fence_wait_prepare. `Invalid` leaves the object reference with
        // the caller, per the ownership contract documented on this function —
        // including the foreign-generation arm below, which is why it returns
        // `Invalid` rather than `AlreadyComplete`: the caller must still deref.
        if self.failed || fence_id == 0 || fence_id >= self.next_wire_fence {
            return FenceEventReg::Invalid;
        }
        if fence_id < self.wire_fence_base {
            FENCE_ID_FOREIGN_GENERATION.fetch_add(1, Ordering::Relaxed);
            return FenceEventReg::Invalid;
        }
        let in_flight = self.inflight.iter().any(|e| match e.kind {
            InFlightKind::AsyncVenus { fence_id: f, .. } => f == fence_id,
            _ => false,
        });
        if !in_flight {
            FENCE_EVENT_ALREADY_COMPLETE.fetch_add(1, Ordering::Relaxed);
            return FenceEventReg::AlreadyComplete;
        }
        if self
            .fence_events
            .iter()
            .any(|e| e.fence_id == fence_id && e.event == event)
        {
            FENCE_EVENT_DUP_REJECTS.fetch_add(1, Ordering::Relaxed);
            return FenceEventReg::Duplicate;
        }
        if self.fence_events.len() >= MAX_FENCE_EVENTS {
            FENCE_EVENT_OVERFLOWS.fetch_add(1, Ordering::Relaxed);
            return FenceEventReg::TableFull;
        }
        self.fence_events.push(FenceEventEntry { fence_id, event });
        bump_high_water(&FENCE_EVENT_HIGH_WATER, self.fence_events.len());
        FENCE_EVENT_REGISTERS.fetch_add(1, Ordering::Relaxed);
        FenceEventReg::Registered
    }

    /// Remove a parked (fence_id, event) registration. Returns `true` if it
    /// was found and removed — the TABLE's object reference transfers back to
    /// the caller (who must deref it); `false` = no such entry (the drain
    /// consumed it — the event was signaled — or it was never parked).
    pub fn fence_event_unregister(&mut self, fence_id: u64, event: NonNull<KEVENT>) -> bool {
        if let Some(i) = self
            .fence_events
            .iter()
            .position(|e| e.fence_id == fence_id && e.event == event)
        {
            self.fence_events.swap_remove(i);
            FENCE_EVENT_CANCELS.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Current fence-event table occupancy (QUERY_STATS v2).
    pub fn fence_events_live(&self) -> u32 {
        self.fence_events.len() as u32
    }

    // ── Registered async present streams ───────────────────────────────────

    /// Reserve one bounded, owner-scoped stream for an ICD context.  The
    /// process association is the exact opaque `hKmdProcess` handle dxgkrnl
    /// supplied to both devices. It is compared byte-for-byte with the UMD
    /// marker context's handle, never dereferenced and never inferred from a
    /// PID or the current thread/process.
    pub fn register_present_stream(
        &mut self,
        owner: DeviceOwner,
        ctx_id: u32,
        creator_process: usize,
    ) -> Result<u64, VirtioError> {
        if self.failed || ctx_id == 0 || creator_process == 0 {
            PRESENT_STREAM_REJECTS.fetch_add(1, Ordering::Relaxed);
            return Err(VirtioError::DeviceError);
        }
        if self.resolve_owned_ctx(Some(owner), ctx_id).is_none() {
            PRESENT_STREAM_REJECTS.fetch_add(1, Ordering::Relaxed);
            return Err(VirtioError::NotOwned);
        }
        // One stream per owned Venus context.  That is what makes a batched
        // PresentSubmissionPrivate record compactly merge same-context marker
        // values without inventing an ordering between two stream namespaces.
        if self
            .present_streams
            .iter()
            .any(|slot| slot.live && slot.owner == Some(owner) && slot.ctx_id == ctx_id)
        {
            PRESENT_STREAM_REJECTS.fetch_add(1, Ordering::Relaxed);
            return Err(VirtioError::DeviceError);
        }
        let Some((index, _)) = self
            .present_streams
            .iter()
            .enumerate()
            .find(|(_, slot)| !slot.live && slot.generation < PRESENT_STREAM_GENERATION_MAX)
        else {
            PRESENT_STREAM_REJECTS.fetch_add(1, Ordering::Relaxed);
            return Err(VirtioError::OutOfMemory);
        };
        let Some(next_cookie) = self.next_present_stream_cookie.checked_add(1) else {
            // Do not wrap an opaque capability into an old registration's
            // value. Exhaustion after 2^64 registrations is a loud failure.
            PRESENT_STREAM_REJECTS.fetch_add(1, Ordering::Relaxed);
            return Err(VirtioError::OutOfMemory);
        };
        let generation = self.present_streams[index].generation + 1;
        let cookie = self.next_present_stream_cookie;
        self.next_present_stream_cookie = next_cookie;
        self.present_streams[index] = PresentStreamSlot {
            live: true,
            owner: Some(owner),
            ctx_id,
            ring_idx: 0,
            generation,
            cookie,
            creator_process,
            submitted_value: 0,
            retired_value: 0,
        };
        let live = PRESENT_STREAM_LIVE.fetch_add(1, Ordering::Relaxed) as usize + 1;
        bump_high_water(&PRESENT_STREAM_HIGH_WATER, live);
        PRESENT_STREAM_REGISTERS.fetch_add(1, Ordering::Relaxed);
        Ok(cookie)
    }

    /// Remove exactly one stream capability.  Wrong owner/context/cookie is a
    /// refusal, not an idempotent wildcard unregister.
    pub fn unregister_present_stream(
        &mut self,
        order: &crate::adapter::NotifyOrdered<'_>,
        owner: DeviceOwner,
        ctx_id: u32,
        cookie: u64,
    ) -> bool {
        let found = self.present_streams.iter().position(|slot| {
            slot.live && slot.owner == Some(owner) && slot.ctx_id == ctx_id && slot.cookie == cookie
        });
        let Some(index) = found else {
            PRESENT_STREAM_REJECTS.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        self.retire_present_stream_slot(index);
        let _ = self.discharge_dead_present_stream_waits(order);
        self.cancel_dead_undispatched_windowed_blt();
        true
    }

    /// Purge all streams whose owning ICD device is gone.  Used before that
    /// DeviceContext releases the object reference the rows borrow.
    pub fn purge_present_streams_for_owner(
        &mut self,
        order: &crate::adapter::NotifyOrdered<'_>,
        owner: Option<DeviceOwner>,
    ) -> u32 {
        let mut count = 0;
        for index in 0..self.present_streams.len() {
            if self.present_streams[index].live && self.present_streams[index].owner == owner {
                self.retire_present_stream_slot(index);
                count += 1;
            }
        }
        let _ = self.discharge_dead_present_stream_waits(order);
        self.cancel_dead_undispatched_windowed_blt();
        count
    }

    /// Context destruction is an exact lifecycle boundary: every stream on the
    /// destroyed owned context is invalid before the host CTX_DESTROY roundtrip.
    pub fn purge_present_streams_for_context(
        &mut self,
        order: &crate::adapter::NotifyOrdered<'_>,
        owner: Option<DeviceOwner>,
        ctx_id: u32,
    ) -> u32 {
        let mut count = 0;
        for index in 0..self.present_streams.len() {
            let slot = self.present_streams[index];
            if slot.live && slot.owner == owner && slot.ctx_id == ctx_id {
                self.retire_present_stream_slot(index);
                count += 1;
            }
        }
        let _ = self.discharge_dead_present_stream_waits(order);
        self.cancel_dead_undispatched_windowed_blt();
        count
    }

    /// Purge all registrations for this transport generation (failure/reset).
    pub fn purge_all_present_streams(&mut self) {
        for index in 0..self.present_streams.len() {
            if self.present_streams[index].live {
                self.retire_present_stream_slot(index);
            }
        }
        self.scanout_refresh.clear();
        // A retained fast bind carrying a tagged boundary may no longer be
        // promoted once every stream generation has been invalidated.
        for deferred in [
            &mut self.fast_bind.deferred_earliest,
            &mut self.fast_bind.deferred_latest,
        ] {
            if deferred.is_some_and(|request| {
                decode_present_stream_boundary(request.carried_watermark).is_some()
            }) {
                *deferred = None;
            }
        }
    }

    /// Reset under scheduler ordering: first invalidate every registration,
    /// then explicitly discharge any WDDM/scanout wait that named it.  The
    /// plain variant is reserved for terminal transport teardown, where the
    /// WDDM FIFO has already been abandoned with the scheduler epoch.
    pub fn purge_all_present_streams_ordered(&mut self, order: &crate::adapter::NotifyOrdered<'_>) {
        self.purge_all_present_streams();
        let _ = self.discharge_dead_present_stream_waits(order);
        self.cancel_dead_undispatched_windowed_blt();
    }

    fn retire_present_stream_slot(&mut self, index: usize) {
        let generation = self.present_streams[index].generation;
        self.present_streams[index] = PresentStreamSlot {
            generation,
            ..PresentStreamSlot::EMPTY
        };
        let _ = PRESENT_STREAM_LIVE.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |live| {
            live.checked_sub(1)
        });
    }

    fn present_stream_handle_live(&self, handle: u32) -> bool {
        let Some(index) = Self::present_stream_index(handle) else {
            return false;
        };
        let slot = self.present_streams[index];
        slot.live && slot.handle(index) == handle
    }

    fn present_stream_boundary_live(&self, boundary: u64) -> bool {
        Self::present_stream_boundary_live_in(&self.present_streams[..], boundary)
    }

    fn present_stream_boundary_live_in(
        present_streams: &[PresentStreamSlot],
        boundary: u64,
    ) -> bool {
        let Some((handle, _)) = decode_present_stream_boundary(boundary) else {
            return false;
        };
        let Some(index) = Self::present_stream_index(handle) else {
            return false;
        };
        let slot = present_streams[index];
        slot.live && slot.handle(index) == handle
    }

    /// Explicitly remove scheduler/scanout waits whose registration was
    /// retired.  This requires the WDDM notification ordering proof because it
    /// mutates the pending `DMA_COMPLETED` FIFO; it never treats a dead stream
    /// as retired.  A discharged WDDM entry still waits for its ordinary wire
    /// watermark, which covers the transport work submitted before that WDDM
    /// buffer.
    pub fn discharge_dead_present_stream_waits(
        &mut self,
        _order: &crate::adapter::NotifyOrdered<'_>,
    ) -> u32 {
        let mut discharged = 0u32;
        for index in 0..self.wddm_pending.len() {
            let Some(boundary) = self.wddm_pending[index].stream_boundary else {
                continue;
            };
            if decode_present_stream_boundary(boundary).is_some()
                && !Self::present_stream_boundary_live_in(&self.present_streams[..], boundary)
            {
                let blt = self.wddm_pending[index]
                    .blt_token
                    .zip(self.wddm_pending[index].blt_stream_boundary);
                if let Some((token, blt_boundary)) = blt {
                    let max_dispatched = self
                        .windowed_blt
                        .pending
                        .iter()
                        .find(|request| {
                            request.token == token && request.stream_boundary == blt_boundary
                        })
                        .is_some_and(|request| request.dispatched);
                    if !max_dispatched {
                        if let Some(prefix) = WindowedBltTerminalPrefix::new(token, blt_boundary) {
                            // This WDDM entry owned the whole merged prefix.
                            // An earlier member may already be in-flight even
                            // though the max member is not, so abandon also
                            // detaches those dispatched readers from the now
                            // unreachable terminal membership.
                            self.abandon_windowed_blt_wddm_prefix(prefix);
                        }
                        self.wddm_pending[index].blt_token = None;
                        self.wddm_pending[index].blt_stream_boundary = None;
                    }
                }
                // The ordinary producer stream is cancelled, never treated as
                // complete. A dispatched BLT retains its separate exact
                // token/stream key above until its ring response terminalizes.
                self.wddm_pending[index].stream_boundary = None;
                discharged += 1;
            }
        }
        let streams = &self.present_streams[..];
        self.scanout_refresh
            .discard_dead_present_stream_markers(|boundary| {
                Self::present_stream_boundary_live_in(streams, boundary)
            });
        let _ = self.discard_invalid_fast_bind_frontier();
        discharged
    }

    /// A carried marker can outlive context/device teardown in the per-flip
    /// private packet.  Rebase that *explicitly cancelled* marker onto the
    /// ordinary current-wire boundary; do not make the dead marker read as a
    /// completed producer.
    pub fn rebase_dead_present_stream_boundary(&self, boundary: u64) -> Option<u64> {
        decode_present_stream_boundary(boundary)
            .filter(|_| !self.present_stream_boundary_live(boundary))
            .map(|_| self.wire_fence_watermark())
    }

    fn present_stream_index(handle: u32) -> Option<usize> {
        helios_kmd_logic::present_stream::handle_index(handle)
    }

    /// Turn one complete marker tail into an opaque, generation-qualified
    /// boundary. The UMD device must carry the exact same opaque
    /// `hKmdProcess` association as the ICD device that registered the stream.
    pub fn present_stream_marker_boundary(
        &self,
        ctx_id: u32,
        value: u32,
        cookie: u64,
        creator_process: usize,
    ) -> Option<u64> {
        if ctx_id == 0 || value == 0 || cookie == 0 || creator_process == 0 {
            PRESENT_STREAM_REJECTS.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let found = self.present_streams.iter().enumerate().find(|(_, slot)| {
            slot.live
                && slot.ctx_id == ctx_id
                && slot.cookie == cookie
                && slot.creator_process == creator_process
        });
        let Some((index, slot)) = found else {
            PRESENT_STREAM_REJECTS.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        // INSTRUMENT ONLY — nothing below refuses, and the boundary returned is
        // byte-identical to the one this function returned before these two
        // counters existed (K-F2, 2026-08-06).
        //
        // WHY THE DIRECTION HERE IS INVERTED FROM THE TAG PATH'S. Both siblings
        // bound their value against the same field — `prepare_present_stream_tag`
        // (`:4770`) and `commit_present_stream_tag` (`:4790`) reject
        // `value <= slot.submitted_value` — because they are the PRODUCER
        // advancing the stream, and an advance that is not strictly ahead is
        // either a replay or a forgery. This is the CONSUMER side, and copying
        // that comparison inverted would refuse every legitimate frame: the UMD
        // hands the marker over BEFORE the frame's `vkQueueSubmit` on purpose
        // (`umd/src/forward/present.rs:1479-1528` skips its own submitted-gate
        // exactly because this marker carries the dependency instead), while the
        // tag that moves `submitted_value` rides DXVK's submission thread. So
        // "ahead" is the normal state, not a defect, and a refusal here would
        // fall back to `wire_fence_watermark()` — which does not cover an
        // unsubmitted frame at all (ROADMAP defect 0ab-B).
        //
        // What IS still unguarded is MAGNITUDE: a guest naming a value tens of
        // thousands of frames out gets a live boundary `present_stream_slot_ready`
        // can never satisfy. These two counters measure the legitimate
        // magnitude so a bound can be chosen from data rather than assumption;
        // grading and the reading that would refute all of this are in
        // `virtio/counters.rs` beside the statics.
        let lookahead =
            helios_kmd_logic::present_stream::marker_lookahead(value, slot.submitted_value);
        if lookahead != 0 {
            PRESENT_STREAM_MARKER_AHEAD.fetch_add(1, Ordering::Relaxed);
            bump_high_water(&PRESENT_STREAM_MARKER_AHEAD_HIGH_WATER, lookahead as usize);
        }
        PRESENT_STREAM_MARKERS.fetch_add(1, Ordering::Relaxed);
        Some(encode_present_stream_boundary(slot.handle(index), value))
    }

    fn prepare_present_stream_tag(
        &self,
        owner: DeviceOwner,
        ctx_id: u32,
        ring_idx: u32,
        cookie: u64,
        value: u32,
    ) -> Result<PresentStreamRetire, VirtioError> {
        if self.failed
            || ctx_id == 0
            || cookie == 0
            || value == 0
            || ring_idx == 0
            || ring_idx > u8::MAX as u32
        {
            PRESENT_STREAM_REJECTS.fetch_add(1, Ordering::Relaxed);
            return Err(VirtioError::DeviceError);
        }
        let Some((index, slot)) = self.present_streams.iter().enumerate().find(|(_, slot)| {
            slot.live && slot.owner == Some(owner) && slot.ctx_id == ctx_id && slot.cookie == cookie
        }) else {
            PRESENT_STREAM_REJECTS.fetch_add(1, Ordering::Relaxed);
            return Err(VirtioError::NotOwned);
        };
        if (slot.ring_idx != 0 && slot.ring_idx != ring_idx as u8) || value <= slot.submitted_value
        {
            PRESENT_STREAM_REJECTS.fetch_add(1, Ordering::Relaxed);
            return Err(VirtioError::DeviceError);
        }
        Ok(PresentStreamRetire {
            handle: slot.handle(index),
            value,
        })
    }

    fn commit_present_stream_tag(&mut self, retire: PresentStreamRetire, ring_idx: u32) {
        let Some(index) = Self::present_stream_index(retire.handle) else {
            PRESENT_STREAM_REJECTS.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let slot = &mut self.present_streams[index];
        if !slot.live
            || slot.handle(index) != retire.handle
            || (slot.ring_idx != 0 && slot.ring_idx != ring_idx as u8)
            || retire.value <= slot.submitted_value
        {
            PRESENT_STREAM_REJECTS.fetch_add(1, Ordering::Relaxed);
            return;
        }
        slot.ring_idx = ring_idx as u8;
        slot.submitted_value = retire.value;
        PRESENT_STREAM_TAGS.fetch_add(1, Ordering::Relaxed);
    }

    fn retire_present_stream_value(&mut self, retire: PresentStreamRetire) {
        let Some(index) = Self::present_stream_index(retire.handle) else {
            return;
        };
        let slot = &mut self.present_streams[index];
        let advanced = advance_present_stream_retired(slot.retired_value, retire.value);
        if slot.live && slot.handle(index) == retire.handle && advanced != slot.retired_value {
            slot.retired_value = advanced;
            PRESENT_STREAM_RETIRES.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A host-rejected tagged submit poisons this exact generation.  The
    /// scheduler/scanout waits are discharged separately under
    /// `wddm_notify_lock`; this helper only removes the capability while the
    /// used-ring drain holds `virtio_lock`.
    fn fail_present_stream_value(&mut self, retire: PresentStreamRetire) {
        let Some(index) = Self::present_stream_index(retire.handle) else {
            return;
        };
        if self.present_streams[index].live
            && self.present_streams[index].handle(index) == retire.handle
        {
            self.retire_present_stream_slot(index);
            PRESENT_STREAM_REJECTS.fetch_add(1, Ordering::Relaxed);
        }
    }

    // ── WDDM pending-fence FIFO (SubmitCommand → DPC completion) ─────────────

    /// Capture the exact Venus ordering boundary for a scanout dirty marker.
    ///
    /// The `NotifyOrdered` token is mintable only inside
    /// `WddmNotifyGuard::with_virtio`, so reaching this method proves that THIS
    /// adapter's `wddm_notify` lock was taken before its `virtio` lock and is
    /// still held.
    ///
    /// `resource_id` is the allocation this dirty edge BELONGS TO — the exact
    /// venus resource the present named — or 0 when the marker carries no
    /// identity (the generic HERF refresh, whose meaning is "whatever is
    /// currently bound").
    ///
    /// Carrying it is the fix for a real ordering defect. The watermark alone
    /// says WHEN it is safe to flush; it never said WHAT to flush, so
    /// `queue_active_scanout_refresh_locked` read `active_scanout_resource` at
    /// flush time instead. Those are different frames: the bind is deferred
    /// (`SetVidPnSourceAddress` stashes `pending_vidpn_allocation` at DIRQL for
    /// a PASSIVE worker, which coalesces), so a refresh armed for frame N could
    /// fire against the previous buffer — a stale frame — or against a buffer
    /// the flip had already advanced to but the app had not yet rendered, which
    /// is a BLACK frame. Host tracing showed both shapes directly: binds whose
    /// content was never flushed, and repeated flushes of one bind.
    ///
    /// The old producer-side CPU gate hid this by removing all overlap: with
    /// the app blocked until its own GPU work completed, bind and flush landed
    /// in the same quiescent window and the bound buffer was always fully
    /// rendered. That is why restoring CPU/GPU overlap exposed it.
    /// The wire-fence boundary as it stands right now: every Venus command
    /// submitted so far carries a fence BELOW this value.
    ///
    /// Read by a caller that knows WHICH FRAME it is arming for, so the
    /// boundary can be captured when that frame is PRESENTED and carried to the
    /// bind edge — see [`Self::note_scanout_refresh_at`].
    pub fn wire_fence_watermark(&self) -> u64 {
        self.next_wire_fence
    }

    /// Arm the marker against an EXPLICIT completion boundary instead of "now".
    ///
    /// WHY THIS EXISTS (ROADMAP defect 0ab-B, measured 2026-07-29). Sampling
    /// `next_wire_fence` inside the bind edge names the wrong frame. The bind
    /// runs in the PASSIVE display worker, ~10 ms after the flip was submitted,
    /// and at 183 fps the app has pushed frame N+1 into the ring by then — so
    /// the flush for frame N waited for N+1 to complete, one whole frame too
    /// long. With two rotating buffers the app has had buffer N handed back and
    /// CLEARED it for N+2 by the time QEMU reads it: an entirely black frame,
    /// re-cleared rather than half-drawn.
    ///
    /// The QEMU-side per-flush oracle measured exactly that split over one GT1
    /// run (`tools/scanout_oracle_report.py`): flushes landing 1–3 ms after
    /// their bind were 1.0 % black, those landing 9–12 ms after were 41.1 %,
    /// and 611 of 619 black frames landed within 0.5 ms of the NEXT bind.
    ///
    /// A watermark captured when the frame was submitted is that frame's own
    /// completion boundary, and nothing later. Still an ordering, never a
    /// stall.
    pub fn note_scanout_refresh_at(
        &mut self,
        _order: &crate::adapter::NotifyOrdered<'_>,
        resource_id: u32,
        watermark: u64,
    ) -> bool {
        // A newer exact marker whose producer is already retired is a complete
        // replacement for an older wait. Waking it now avoids stranding it on
        // an idle used-ring; the executor still refuses an unaccepted or stale
        // binding before it can issue a host read.
        let ready = self.scanout_boundary_ready(watermark);
        self.scanout_refresh
            .note(ScanoutRefreshMarker::new(resource_id, watermark), ready)
    }

    /// Consume a completion-ordered scanout marker after the used-ring drain.
    /// Must be called under the same statically witnessed notification lock as
    /// [`Self::note_scanout_refresh`].
    ///
    /// Returns the full armed marker on success, preserving the exact producer
    /// boundary through promotion and into the diagnostic timeline.
    pub fn take_ready_scanout_refresh(
        &mut self,
        _order: &crate::adapter::NotifyOrdered<'_>,
    ) -> Option<ScanoutRefreshMarker> {
        let earliest = self.scanout_refresh.earliest()?;
        let earliest_ready = self.scanout_boundary_ready(earliest.boundary());
        let latest_ready = self
            .scanout_refresh
            .latest()
            .is_some_and(|marker| self.scanout_boundary_ready(marker.boundary()));
        let marker = self
            .scanout_refresh
            .take_ready(earliest_ready, latest_ready)?;
        Some(marker)
    }

    /// How many async Venus fences below `watermark` are still outstanding, and
    /// the ring of the lowest one (0 = host DECODE, >= 1 = host GPU).
    ///
    /// Diagnostic for defect 0ab-B: it says WHAT a deferred bind-edge arm is
    /// waiting for. One or two GPU-ring fences means the app's own next frames;
    /// a large count, or a decode-ring fence, means the boundary is blocked by
    /// something that has nothing to do with this frame.
    pub fn outstanding_below(&self, watermark: u64) -> (u32, u8) {
        if let Some((handle, _)) = decode_present_stream_boundary(watermark) {
            let ring = Self::present_stream_index(handle)
                .map(|index| self.present_streams[index].ring_idx)
                .unwrap_or(0);
            return (u32::from(!self.scanout_boundary_ready(watermark)), ring);
        }
        let mut count = 0u32;
        let mut lowest = u64::MAX;
        let mut lowest_ring = 0u8;
        for e in self.inflight.iter() {
            if let InFlightKind::AsyncVenus {
                fence_id, ring_idx, ..
            } = e.kind
            {
                if fence_id < watermark {
                    count += 1;
                    if fence_id < lowest {
                        lowest = fence_id;
                        lowest_ring = ring_idx;
                    }
                }
            }
        }
        (count, lowest_ring)
    }

    /// Whether every async wire fence `< watermark` in `domain` has retired.
    fn async_retired_up_to(&self, watermark: u64, domain: RetireDomain) -> bool {
        watermark == 0
            || !self.inflight.iter().any(|e| match e.kind {
                InFlightKind::AsyncVenus {
                    fence_id, ring_idx, ..
                } => {
                    fence_id < watermark
                        && match domain {
                            RetireDomain::IncludingGpu => true,
                            RetireDomain::DecodeOnly => ring_idx == 0,
                        }
                }
                _ => false,
            })
    }

    /// Whether the ONE async wire fence `fence_id` has retired.
    ///
    /// ⛔ THE FRAME'S OWN BOUNDARY, and the point of A4. Where
    /// [`Self::async_retired_up_to`] asks *"has everything below this retired"* —
    /// a prefix over every ring and every process — this asks only about the fence
    /// the submitting UMD actually named. Every id below `next_wire_fence` was
    /// genuinely assigned AND enqueued (the counter is bumped only after
    /// `control.add` succeeds, in the same spinlock section as the `inflight`
    /// push), so an id that is not in flight has necessarily retired: absence is a
    /// completion proof here, not an unknown.
    ///
    /// ⚠ NO `RetireDomain` FILTER, deliberately. A domain filter over a SINGLE
    /// named fence could only ever fake readiness — "ring 1, so ignore it" — never
    /// add safety, and the exact arm is constructed exclusively with
    /// `IncludingGpu` anyway (`gpu_completion_fence.is_some()` forces that domain
    /// one screen above the watermark selection). Taking the domain as a parameter
    /// and ignoring it would have been the trap.
    fn async_exact_retired(&self, fence_id: u64) -> bool {
        fence_id == 0
            || !self.inflight.iter().any(|e| match e.kind {
                InFlightKind::AsyncVenus {
                    fence_id: in_flight,
                    ..
                } => in_flight == fence_id,
                _ => false,
            })
    }

    /// Evaluate one WDDM entry's wire-fence dependency under its own
    /// interpretation. The ONLY caller shape for a `WddmPending`; the bare
    /// [`Self::async_retired_up_to`] keeps its three existing non-WDDM callers.
    fn wire_boundary_ready(
        &self,
        watermark: u64,
        domain: RetireDomain,
        boundary: WireBoundary,
    ) -> bool {
        match boundary {
            WireBoundary::Prefix => self.async_retired_up_to(watermark, domain),
            WireBoundary::Exact => self.async_exact_retired(watermark),
        }
    }

    /// Readiness for the two intentionally incomparable boundary namespaces.
    /// A tagged stream boundary is never fed to the wire-fence `< watermark`
    /// scan: it is ready only when the same generation-qualified stream has
    /// retired at least the marker value.  A vanished stream is *not* ready;
    /// lifecycle code must explicitly discharge the associated WDDM/scanout
    /// wait under the notification lock before its fence can progress.
    fn scanout_boundary_ready(&self, boundary: u64) -> bool {
        let Some((handle, value)) = decode_present_stream_boundary(boundary) else {
            return self.async_retired_up_to(boundary, RetireDomain::IncludingGpu);
        };
        let Some(index) = Self::present_stream_index(handle) else {
            return false;
        };
        present_stream_slot_ready(self.present_streams[index], index, handle, value)
    }

    /// Liveness companion to [`Self::scanout_boundary_ready`] for a bind that
    /// has not yet reached the control FIFO. Ordinary wire watermarks are live
    /// for this transport generation; tagged boundaries require the exact live
    /// stream generation and are discarded on teardown rather than treated as
    /// producer completion.
    fn scanout_bind_boundary_live(&self, boundary: u64) -> bool {
        decode_present_stream_boundary(boundary)
            .map_or(true, |_| self.present_stream_boundary_live(boundary))
    }

    /// Reserve one exact WindowedBlt reader before Present returns. Caller
    /// holds the scanout lifecycle lock and has already prepared the immutable
    /// Venus cache record under the Venus mutex; this final mutation happens
    /// under virtio only and never submits host work.
    pub fn queue_windowed_blt(
        &mut self,
        adapter: &crate::adapter::AdapterContext,
        source: OptimalPresentImageDesc,
        destination: PresentDestinationDesc,
        prepared: PreparedPresentBltSubmission,
        stream_boundary: u64,
        system_backing: bool,
    ) -> Result<u64, VirtioError> {
        if self.failed
            || !self.present_stream_boundary_live(stream_boundary)
            || source.resource_id() == 0
            || source.resource_id() == destination.resource_id()
        {
            return Err(VirtioError::DeviceError);
        }
        let Some(token) = self.windowed_blt.issue_token() else {
            return Err(VirtioError::OutOfMemory);
        };
        // Unlike legacy D4a, an unledgered WindowedBlt would let DXVK reuse a
        // snapshot without an exact reader lifetime. Refuse loudly instead.
        let ledger_ticket = adapter.read_ledger.issue(source.resource_id());
        if !ledger_ticket.is_claimed() {
            return Err(VirtioError::OutOfMemory);
        }
        self.windowed_blt.pending.push_back(WindowedBltPending {
            adapter: NonNull::from(adapter),
            token,
            stream_boundary,
            source_resource_id: source.resource_id(),
            destination_resource_id: destination.resource_id(),
            source,
            destination,
            prepared,
            ledger_ticket,
            admitted: false,
            dispatched: false,
            ring_complete: false,
            ledger_retired: false,
            wddm_completion_required: true,
            system_backing,
            mirror_claimed: false,
        });
        crate::ddi::scanout_timeline::note(
            crate::ddi::scanout_timeline::kind::WINDOWED_BLT_ARM,
            crate::ddi::scanout_timeline::flag::SNAPSHOT,
            0,
            stream_boundary,
            token,
            source.resource_id(),
            destination.resource_id(),
        );
        Ok(token)
    }

    /// Promote only requests represented by the same SubmitCommand private
    /// record. Cross-stream token comparison is prohibited: the boundary's
    /// generation-qualified handle is the identity; a numeric token alone is
    /// never a scheduler admission proof.
    fn admit_windowed_blt_prefix(&mut self, stream_boundary: u64, max_token: u64) {
        if max_token == 0 || !self.present_stream_boundary_live(stream_boundary) {
            return;
        }
        let Some((handle, _)) = decode_present_stream_boundary(stream_boundary) else {
            return;
        };
        for request in self.windowed_blt.pending.iter_mut() {
            let same_stream = decode_present_stream_boundary(request.stream_boundary)
                .is_some_and(|(candidate, _)| candidate == handle);
            if same_stream && !request.admitted && request.token <= max_token {
                request.admitted = true;
                // Capacity is coupled to pending and reserved at init.
                self.windowed_blt.ready.push_back(request.token);
                crate::ddi::scanout_timeline::note(
                    crate::ddi::scanout_timeline::kind::WINDOWED_BLT_ADMIT,
                    crate::ddi::scanout_timeline::flag::READY,
                    0,
                    request.stream_boundary,
                    request.token,
                    request.source_resource_id,
                    request.destination_resource_id,
                );
            }
        }
    }

    /// Worker-side selection. Both scheduler admission and the exact producer
    /// boundary are mandatory; a retired producer without residency admission
    /// remains inert in `pending`.
    pub fn take_ready_windowed_blt(&mut self) -> Option<WindowedBltPending> {
        let token = *self.windowed_blt.ready.front()?;
        let index = self
            .windowed_blt
            .pending
            .iter()
            .position(|request| request.token == token)?;
        let boundary = self.windowed_blt.pending[index].stream_boundary;
        if !self.windowed_blt.pending[index].admitted
            || self.windowed_blt.pending[index].dispatched
            || !self.scanout_boundary_ready(boundary)
        {
            return None;
        }
        self.windowed_blt.pending[index].dispatched = true;
        self.windowed_blt.ready.pop_front();
        Some(self.windowed_blt.pending[index])
    }

    fn wake_ready_windowed_blt(&self) {
        let ready = self.windowed_blt.pending.iter().find(|request| {
            request.admitted
                && !request.dispatched
                && self.scanout_boundary_ready(request.stream_boundary)
        });
        if let Some(request) = ready {
            // SAFETY: pending requests hold the adapter that owns this live
            // transport; teardown cancels the FIFO before freeing it.
            unsafe { request.adapter.as_ref() }.signal_hpd();
        }
    }

    fn terminal_windowed_blt(
        &mut self,
        adapter: &crate::adapter::AdapterContext,
        token: u64,
        stream_boundary: u64,
        ok: bool,
    ) {
        let Some(index) = self.windowed_blt.pending.iter().position(|request| {
            request.token == token && request.stream_boundary == stream_boundary
        }) else {
            return;
        };
        let Some(request) = self.windowed_blt.pending.remove(index) else {
            return;
        };
        if !request.ledger_retired {
            adapter
                .read_ledger
                .retire(request.ledger_ticket, !ok);
        }
        crate::ddi::scanout_timeline::note(
            crate::ddi::scanout_timeline::kind::WINDOWED_BLT_TERMINAL,
            if ok {
                crate::ddi::scanout_timeline::flag::SUCCESS
            } else {
                0
            },
            0,
            stream_boundary,
            token,
            request.source_resource_id,
            request.destination_resource_id,
        );
        if request.wddm_completion_required {
            debug_assert!(self.windowed_blt.terminal.len() < MAX_WINDOWED_BLT_PENDING);
            self.windowed_blt
                .terminal
                .push_back((token, stream_boundary));
        }
        adapter.scanout_retire_wanted.store(1, Ordering::Release);
        adapter.signal_hpd();
    }

    /// Completion of the ring-1 copy. The reader lease ends exactly here. A
    /// system-backed destination remains non-terminal until the PASSIVE mirror
    /// finishes, while an ordinary destination terminalizes immediately.
    pub fn complete_windowed_blt_ring(
        &mut self,
        adapter: &crate::adapter::AdapterContext,
        token: u64,
        stream_boundary: u64,
        ok: bool,
    ) {
        let Some(request) =
            self.windowed_blt.pending.iter_mut().find(|request| {
                request.token == token && request.stream_boundary == stream_boundary
            })
        else {
            return;
        };
        request.ring_complete = true;
        if !request.ledger_retired {
            adapter
                .read_ledger
                .retire(request.ledger_ticket, !ok);
            request.ledger_retired = true;
        }
        crate::ddi::scanout_timeline::note(
            crate::ddi::scanout_timeline::kind::WINDOWED_BLT_RING_COMPLETE,
            if ok {
                crate::ddi::scanout_timeline::flag::SUCCESS
            } else {
                0
            },
            0,
            stream_boundary,
            token,
            request.source_resource_id,
            request.destination_resource_id,
        );
        if !ok || !request.system_backing {
            self.terminal_windowed_blt(adapter, token, stream_boundary, ok);
        } else {
            // The worker owns the preallocated mirror continuation.
            adapter.signal_hpd();
        }
    }

    /// PASSIVE mirror completion; `None` means Windows repaged back to BAR and
    /// therefore the Venus destination is authoritative again.
    pub fn complete_windowed_blt_mirror(
        &mut self,
        adapter: &crate::adapter::AdapterContext,
        token: u64,
        stream_boundary: u64,
        ok: bool,
    ) {
        self.terminal_windowed_blt(adapter, token, stream_boundary, ok);
    }

    /// Present-private write failed after reservation. No scheduler submission
    /// can carry this request, so retire its exact reader and remove it without
    /// manufacturing a terminal WDDM token.
    pub fn cancel_windowed_blt(
        &mut self,
        adapter: &crate::adapter::AdapterContext,
        token: u64,
        stream_boundary: u64,
    ) {
        let Some(index) = self.windowed_blt.pending.iter().position(|request| {
            request.token == token && request.stream_boundary == stream_boundary
        }) else {
            return;
        };
        let Some(request) = self.windowed_blt.pending.remove(index) else {
            return;
        };
        if !request.ledger_retired {
            adapter
                .read_ledger
                .retire(request.ledger_ticket, true);
        }
        self.windowed_blt.ready.retain(|known| *known != token);
        adapter.signal_hpd();
    }

    /// Drop the WDDM completion ownership of one merged same-stream prefix.
    /// This is used only when VidSch has terminally abandoned that FIFO entry
    /// (overflow): undispatched work is cancelled because its residency proof
    /// vanished, while submitted host work stays pinned through ring completion
    /// but cannot leave an unreachable terminal membership behind.
    fn cancel_undispatched_windowed_blt_prefix(&mut self, prefix: WindowedBltTerminalPrefix) {
        while let Some((token, boundary, adapter)) =
            self.windowed_blt.pending.iter().find_map(|request| {
                (!request.dispatched && prefix.contains(request.token, request.stream_boundary))
                    .then_some((request.token, request.stream_boundary, request.adapter))
            })
        {
            // SAFETY: an undispatched FIFO request has no host ring entry and
            // stores the live adapter that owns this transport.
            self.cancel_windowed_blt(unsafe { adapter.as_ref() }, token, boundary);
        }
    }

    /// A dead present-stream registration revokes every undispatched request
    /// in its exact prefix.  Its terminal identities are unreachable too, but
    /// a dispatched request stays pinned until its ring response: that host
    /// reader is real even though VidSch can no longer consume its fence.
    fn discard_unreachable_windowed_blt_prefix(&mut self, prefix: WindowedBltTerminalPrefix) {
        self.consume_windowed_blt_terminal_prefix(prefix);
        self.cancel_undispatched_windowed_blt_prefix(prefix);
    }

    /// Lifecycle teardown may invalidate a stream before any WDDM submission
    /// names a queued request.  Walk those unadmitted FIFO entries explicitly;
    /// relying only on `wddm_pending` would strand their reader leases forever.
    fn cancel_dead_undispatched_windowed_blt(&mut self) {
        while let Some((token, boundary, adapter)) =
            self.windowed_blt.pending.iter().find_map(|request| {
                (!request.dispatched && !self.present_stream_boundary_live(request.stream_boundary))
                    .then_some((request.token, request.stream_boundary, request.adapter))
            })
        {
            // SAFETY: an undispatched request has no host reader and retains
            // its owning adapter until this exact cancellation retires it.
            self.cancel_windowed_blt(unsafe { adapter.as_ref() }, token, boundary);
        }
    }

    fn abandon_windowed_blt_wddm_prefix(&mut self, prefix: WindowedBltTerminalPrefix) {
        self.discard_unreachable_windowed_blt_prefix(prefix);

        for request in self.windowed_blt.pending.iter_mut() {
            if request.dispatched && prefix.contains(request.token, request.stream_boundary) {
                request.wddm_completion_required = false;
            }
        }
    }

    /// ResetEngine/ResetFromTimeout abandon the entire scheduler epoch.  This
    /// differs from preemption: dxgkrnl will not replay those DMA buffers, so
    /// every undispatched request loses its residency proof. Submitted host
    /// copies keep their reader until ring completion but no longer retain a
    /// terminal WDDM membership that the reset destroyed.
    pub fn terminal_abandon_wddm_epoch(
        &mut self,
        _order: &crate::adapter::NotifyOrdered<'_>,
    ) -> u32 {
        let n = self.wddm_pending.len() as u32;
        while let Some(pending) = self.wddm_pending.pop_front() {
            if let Some(prefix) = pending
                .blt_token
                .zip(pending.blt_stream_boundary)
                .and_then(|(token, boundary)| WindowedBltTerminalPrefix::new(token, boundary))
            {
                self.abandon_windowed_blt_wddm_prefix(prefix);
            }
        }
        while let Some((token, boundary, adapter)) =
            self.windowed_blt.pending.iter().find_map(|request| {
                (!request.dispatched).then_some((
                    request.token,
                    request.stream_boundary,
                    request.adapter,
                ))
            })
        {
            // SAFETY: not dispatched means no host ring entry can retain it.
            self.cancel_windowed_blt(unsafe { adapter.as_ref() }, token, boundary);
        }
        for request in self.windowed_blt.pending.iter_mut() {
            request.wddm_completion_required = false;
        }
        self.windowed_blt.ready.clear();
        self.windowed_blt.terminal.clear();
        n
    }

    /// Terminal transport death is stronger than scheduler preemption: after
    /// the transport latches failed (or its device status is reset), no host
    /// reader can complete. Retire EVERY exact WindowedBlt ledger issue now and
    /// discard terminal memberships because the WDDM FIFO has no consumer.
    fn abort_windowed_blt_for_terminal_transport(&mut self) {
        self.windowed_blt.ready.clear();
        self.windowed_blt.terminal.clear();
        while let Some(request) = self.windowed_blt.pending.pop_front() {
            if !request.ledger_retired {
                // SAFETY: every queued request was created with this live
                // adapter; transport destruction is below its lifecycle.
                unsafe { request.adapter.as_ref() }.read_ledger.retire(
                    request.ledger_ticket,
                    true,
                );
            }
            crate::ddi::scanout_timeline::note(
                crate::ddi::scanout_timeline::kind::WINDOWED_BLT_TERMINAL,
                0,
                0,
                request.stream_boundary,
                request.token,
                request.source_resource_id,
                request.destination_resource_id,
            );
        }
    }

    /// Clear every WDDM FIFO entry after an overflow and detach the exact
    /// WindowedBlt prefix each entry used to own. `current` is the submission
    /// that discovered the full FIFO and is never enqueued, but its newly
    /// admitted prefix must be abandoned too.
    fn overflow_wddm_pending(&mut self, current: Option<WindowedBltTerminalPrefix>) {
        while let Some(pending) = self.wddm_pending.pop_front() {
            if let (Some(token), Some(boundary)) = (pending.blt_token, pending.blt_stream_boundary)
            {
                if let Some(prefix) = WindowedBltTerminalPrefix::new(token, boundary) {
                    self.abandon_windowed_blt_wddm_prefix(prefix);
                }
            }
        }
        if let Some(prefix) = current {
            self.abandon_windowed_blt_wddm_prefix(prefix);
        }
    }

    /// Allocation teardown cancels every transaction that names this exact
    /// snapshot source or DXGI destination before cache/resource destruction.
    /// Each request becomes a terminal cancellation so an already admitted
    /// WDDM fence cannot wait forever on a resource that no longer exists.
    pub fn cancel_windowed_blt_for_resource(
        &mut self,
        adapter: &crate::adapter::AdapterContext,
        resource_id: u32,
    ) {
        while let Some((token, boundary, admitted)) =
            self.windowed_blt.pending.iter().find_map(|request| {
                (!request.dispatched
                    && (request.source_resource_id == resource_id
                        || request.destination_resource_id == resource_id))
                    .then_some((request.token, request.stream_boundary, request.admitted))
            })
        {
            if admitted {
                self.terminal_windowed_blt(adapter, token, boundary, false);
            } else {
                self.cancel_windowed_blt(adapter, token, boundary);
            }
        }
    }

    /// Called only after Venus cache drain proved every matching submitted
    /// command has reached a terminal ring response. System-backed requests
    /// may still be awaiting their PASSIVE mirror; teardown cancels those now
    /// (their reader was already retired at ring completion). A still-dispatched
    /// request is a hard refusal so callers retain the backing rather than
    /// destroying memory an in-flight host GPU command can read.
    pub fn finish_windowed_blt_teardown_for_resource(
        &mut self,
        adapter: &crate::adapter::AdapterContext,
        resource_id: u32,
    ) -> bool {
        if self.windowed_blt.pending.iter().any(|request| {
            request.dispatched
                && !request.ring_complete
                && (request.source_resource_id == resource_id
                    || request.destination_resource_id == resource_id)
        }) {
            return false;
        }
        while let Some((token, boundary)) = self.windowed_blt.pending.iter().find_map(|request| {
            (request.ring_complete
                && (request.source_resource_id == resource_id
                    || request.destination_resource_id == resource_id))
                .then_some((request.token, request.stream_boundary))
        }) {
            self.terminal_windowed_blt(adapter, token, boundary, false);
        }
        true
    }

    /// Claim exactly one ring-complete system backing for PASSIVE mirroring.
    /// The claim bit makes a lost HPD wake harmless without a polling loop.
    pub fn take_windowed_blt_mirror(&mut self) -> Option<WindowedBltPending> {
        let request = self.windowed_blt.pending.iter_mut().find(|request| {
            request.ring_complete && request.system_backing && !request.mirror_claimed
        })?;
        request.mirror_claimed = true;
        Some(*request)
    }

    pub fn windowed_blt_terminal(&self, token: u64, stream_boundary: u64) -> bool {
        self.windowed_blt.terminal_contains(token, stream_boundary)
    }

    fn windowed_blt_terminal_prefix_ready(&self, token: u64, stream_boundary: u64) -> bool {
        WindowedBltTerminalPrefix::new(token, stream_boundary)
            .is_some_and(|prefix| self.windowed_blt.terminal_prefix_ready(prefix))
    }

    /// Called only after the matching WDDM DMA completion was delivered, or
    /// when its WDDM FIFO entry was terminally abandoned. The compact prefix
    /// removes every same-stream token it represented and nothing else.
    pub(crate) fn consume_windowed_blt_terminal_prefix(
        &mut self,
        prefix: WindowedBltTerminalPrefix,
    ) {
        self.windowed_blt.consume_terminal_prefix(prefix);
    }

    /// Record a WDDM submission (`SubmissionFenceId = fence`). Returns `true`
    /// if the caller should signal DMA_COMPLETED immediately (no venus work
    /// outstanding, nothing queued ahead); otherwise the interrupt DPC
    /// completes it via [`Self::take_ready_wddm`] once every async submission
    /// queued before it has retired. Paging buffers carry no venus work
    /// (watermark 0) but still queue FIFO behind earlier render submissions —
    /// SubmissionFenceIds are watermarks to dxgkrnl and must complete
    /// monotonically.
    pub fn note_wddm_submission(
        &mut self,
        _order: &crate::adapter::NotifyOrdered<'_>,
        fence: u32,
        paging: bool,
        gpu_completion_fence: Option<u64>,
        stream_boundary: Option<u64>,
        blt_token: Option<u64>,
        d3d12: bool,
    ) -> bool {
        if self.failed {
            // Nothing will ever retire, so queueing this fence guarantees a TDR.
            // Signal it now - and clear the FIFO in the SAME critical section:
            // dxgkrnl requires monotonic SubmissionFenceId completion, so
            // signalling the newest fence while older ones stay queued would
            // break the invariant.
            WDDM_SIGNAL_AFTER_FAILURE.fetch_add(1, Ordering::Relaxed);
            self.wddm_pending.clear();
            // A failure latch aborted every WindowedBlt reader before this
            // path can be reached. Clear any stale membership defensively: no
            // WDDM FIFO entry remains that could consume it.
            self.windowed_blt.terminal.clear();
            return true;
        }
        // A non-paging DMA fence must mean "the GPU is finished", because that
        // is what dxgkrnl schedules on. Retiring it at DECODE reports completion
        // with host GPU work still executing, which lets dxgkrnl advance a flip
        // or release an app's surface to the compositor mid-write. Paging keeps
        // the decode domain: it moves memory rather than producing pixels, and
        // coupling it to unrelated GPU work is the pacing cost the DecodeOnly
        // doc warns about with none of the ordering benefit.
        // A stale stream record is an explicit cancellation, never a satisfied
        // producer.  Preserve the ordinary wire watermark below, which still
        // orders every transport entry before this WDDM buffer.
        let raw_stream_boundary = stream_boundary;
        let stream_boundary =
            raw_stream_boundary.filter(|boundary| self.present_stream_boundary_live(*boundary));
        let mut blt_token = blt_token.filter(|token| *token != 0);
        let mut blt_stream_boundary = raw_stream_boundary
            .filter(|boundary| decode_present_stream_boundary(*boundary).is_some());
        if !matches!((blt_token, blt_stream_boundary), (Some(_), Some(_))) {
            // WindowedBlt identity is all-or-none. A token without its exact
            // generation-qualified stream (or a stream tail without a token)
            // cannot name a terminal, so it must not become a permanently
            // blocked `(Some, None)` FIFO head.
            blt_token = None;
            blt_stream_boundary = None;
        }
        if let (Some(token), Some(boundary)) = (blt_token, blt_stream_boundary) {
            let exact_pending = self
                .windowed_blt
                .pending
                .iter()
                .any(|request| request.token == token && request.stream_boundary == boundary);
            let exact_terminal = self.windowed_blt.terminal_contains(token, boundary);
            let prefix = WindowedBltTerminalPrefix::new(token, boundary);
            let already_owned_by_wddm = prefix.is_some()
                && self.wddm_pending.iter().any(|pending| {
                    pending
                        .blt_token
                        .zip(pending.blt_stream_boundary)
                        .and_then(|(max_token, max_boundary)| {
                            WindowedBltTerminalPrefix::new(max_token, max_boundary)
                        })
                        .is_some_and(|owner| owner.contains(token, boundary))
                });
            if !helios_kmd_logic::windowed_blt_token::can_attach_dependency(
                exact_pending,
                exact_terminal,
                already_owned_by_wddm,
            ) {
                // A stale/recycled private record has neither an exact live
                // transaction nor a retained terminal. A duplicate WDDM
                // owner is equally invalid. Preemption is intentionally not
                // one-shot: a host copy may terminalize before dxgkrnl
                // resubmits its private buffer, in which case `exact_terminal`
                // is the authoritative replay proof.
                crate::ddi::scanout_timeline::note(
                    crate::ddi::scanout_timeline::kind::WINDOWED_BLT_TERMINAL,
                    0,
                    0,
                    boundary,
                    token,
                    0,
                    0,
                );
                blt_token = None;
                blt_stream_boundary = None;
            } else if stream_boundary.is_none() {
                // The generation died between Present and SubmitCommand.
                // Cancel every undispatched transaction represented by this
                // merged same-stream prefix. A dispatched exact max stays
                // pinned to its ring response; on the first SubmitCommand
                // there cannot be one, but do not turn that invariant into a
                // use-after-free if lifecycle ordering changes.
                let max_dispatched = self
                    .windowed_blt
                    .pending
                    .iter()
                    .find(|request| request.token == token && request.stream_boundary == boundary)
                    .is_some_and(|request| request.dispatched);
                if !max_dispatched {
                    if let Some(prefix) = prefix {
                        // This dead SubmitCommand cannot own the merged
                        // prefix. Earlier members can already be in flight,
                        // so abandonment also detaches their eventual ring
                        // responses from unreachable WDDM terminal state.
                        self.abandon_windowed_blt_wddm_prefix(prefix);
                    }
                    blt_token = None;
                    blt_stream_boundary = None;
                }
            }
        }
        if let (Some(boundary), Some(token)) = (stream_boundary, blt_token) {
            self.admit_windowed_blt_prefix(boundary, token);
        }
        let domain = if stream_boundary.is_some() || gpu_completion_fence.is_some() {
            RetireDomain::IncludingGpu
        } else if paging || !self.dma_gpu_fence {
            RetireDomain::DecodeOnly
        } else {
            RetireDomain::IncludingGpu
        };
        let (watermark, wire_boundary) = if paging {
            (0, WireBoundary::Prefix)
        } else if let Some(gpu_fence_id) = gpu_completion_fence {
            // THE DECISION TABLE IS `helios_kmd_logic::wddm_boundary::select`, and
            // it is there rather than here because A4 and A6 are both defects OF
            // THIS TABLE and this crate cannot host a test (`panic = "abort"`
            // cdylib). `wddm_boundary_tests` is the oracle; this frame only maps
            // its verdict onto counters and onto the transport's own enum.
            //
            // `[wire_fence_base, next_wire_fence)` is exactly "issued by this
            // transport generation": ids stride up by 2^32 at every StartDevice,
            // and `next_wire_fence` is bumped only after `control.add` succeeds,
            // in the same spinlock section as the `inflight` push.
            use helios_kmd_logic::wddm_boundary as boundary;
            let selection = boundary::select(
                gpu_fence_id,
                self.wire_fence_base,
                self.next_wire_fence,
                d3d12,
            );
            match selection.rejection {
                // A malformed/stale private marker must not manufacture an
                // impossible future dependency. Conservatively gate on all work
                // actually enqueued before this WDDM submission.
                //
                // COUNTED since the D3D12 arm exists (2026-08-06). This clamp was
                // silent, and it is the one place a guest-supplied boundary is
                // quietly replaced by a different one: the fence then reports a
                // watermark nobody asked for. Both writers reach it — Present's BLT
                // marker and `HeliosD3D12SubmitCmd` — so it is deliberately NOT
                // named after either.
                boundary::Rejection::OutOfRange => {
                    GPU_FENCE_CLAMPED.fetch_add(1, Ordering::Relaxed);
                }
                // FOREIGN GENERATION (A6). Same conservative fallback, its own
                // counter: the two conditions are diagnosed differently — an id
                // ahead of the range is a stale or forged sample inside this
                // generation, an id below it is a survivor of a device restart —
                // and one counter for both would have hidden the whole A6 class
                // inside a number that reads as the known-benign clamp.
                //
                // ⛔ NO OWNER CHECK BESIDE IT, AND THAT IS A DECISION. The check
                // would be "was this id issued to the process/context that
                // submitted this DMA buffer", and it is not worth its cost:
                //  * it cannot be answered from existing state —
                //    `InFlightKind::AsyncVenus` records `fence_id`/`ring_idx` and
                //    no owner, so it means a new field in the transport's hottest
                //    DISPATCH-time table that every enqueue, drain and reap must
                //    maintain;
                //  * the harm it would prevent is strictly weaker than one the
                //    contract already grants. A fence id here is only ever a WAIT
                //    TARGET, never something this arm signals, so naming another
                //    process's id can only stall the naming context or — if that id
                //    already retired — complete without waiting for the namer's own
                //    work, which lands on the namer's own pixels. A hostile guest
                //    already has the cheaper form: `gpu_wire_fence = 0` is the
                //    DOCUMENTED order-against-nothing arm (`D12Zero`);
                //  * nothing another process owns becomes reachable — the boundary
                //    selects a wait, and every other client's WDDM fence keeps its
                //    own entry with its own watermark.
                // ⇒ generation is checked because a cross-generation id breaks the
                // predicate's soundness for the OS scheduler; ownership is not,
                // because it only re-describes a self-harm the DDI already permits.
                boundary::Rejection::ForeignGeneration => {
                    GPU_FENCE_FOREIGN_GENERATION.fetch_add(1, Ordering::Relaxed);
                }
                boundary::Rejection::Accepted => {}
            }
            let wire_boundary = match selection.kind {
                boundary::Kind::Prefix => WireBoundary::Prefix,
                // ⛔⛔ THE EXACT D3D12 BOUNDARY (A4). CLAUDE.md's invariant table:
                // *"A WDDM fence may wait on the frame's OWN boundary, never on the
                // whole `next_wire_fence` backlog."* Until 2026-08-06 this arm
                // produced `gpu_fence_id + 1` as a PREFIX, so a D3D12 packet waited
                // for every async wire fence below the named one to retire — every
                // ring, every process, DWM's ring-1 scanout copies included. That is
                // the exact superset the present path had to relax away
                // (`PRESENT_EXACT_WATERMARK_USED`), reintroduced on the new arm.
                //
                // WHY EXACTNESS IS SOUND HERE. The WDDM DMA buffers on this driver
                // carry no GPU commands at all — this one carries a
                // `HeliosD3D12SubmitCmd` record and nothing else — so the only thing
                // DMA_COMPLETED can truthfully report is that the work this
                // submission named has finished. `pfnExecuteCommandLists` submits
                // the batch's Vulkan work through the ICD and hands us the ring-1
                // wire fence it ends at, and `mark_d3d12` takes the MAX over records
                // batched into the same private-data buffer, so the named id
                // subsumes every earlier submission of this packet. Waiting on
                // anything else is waiting on another process's frames.
                //
                // ⚠ WHY THE LEGACY PRESENT ARM KEEPS THE PREFIX (the same
                // `gpu_completion_fence` field, written by Present's BLT marker).
                // Two reasons, and neither is "exactness would not work there":
                // (1) that arm IS the shipping, measured desktop configuration —
                // every accepted present-path measurement, `PresentWmk`'s
                // +3.7…+4.3 % paired GT1 delta included, was taken with the prefix
                // on the wire-fence boundary, and CLAUDE.md rule 8 forbids shipping
                // a default nobody measured; (2) A4 is a defect report about THIS
                // arm, and widening the repair to the desktop path would mean the
                // first D3D12 deploy could not attribute a present regression.
                // `D12Exact` against `PwExact` keeps the two answerable separately.
                boundary::Kind::Exact => {
                    D3D12_EXACT_WATERMARK_USED.fetch_add(1, Ordering::Relaxed);
                    WireBoundary::Exact
                }
            };
            (selection.watermark, wire_boundary)
        } else if stream_boundary.is_some() && self.present_exact_watermark {
            // EXACT PRESENT WATERMARK (2026-08-04). `next_wire_fence` is "every
            // transport entry enqueued before this WDDM buffer" — a superset
            // that includes work belonging to LATER frames, because the DXVK CS
            // thread runs ahead of the presenting thread and dxgkrnl submits a
            // flip about a frame after the app presented (the same over-wait
            // `arm_dma_flip`'s 0ab-B note already recorded from the flush side).
            //
            // A submission that carries a LIVE stream boundary already states
            // its exact dependency: `stream_ready` below is that frame's own
            // producer completion, in the generation-qualified stream namespace.
            // The WDDM DMA buffers on this driver carry no GPU commands at all —
            // a Render marker or a flip record — so the frame the marker names
            // IS the work this fence reports. Keeping the superset on top of it
            // only delays the fence by the pipeline depth, which is what makes
            // dxgkrnl block the presenting thread at its 3-deep present queue
            // (ETW `BlockThread` Reason=2; 21% of presents, 2.45 ms each).
            //
            // The relaxation is deliberately NOT applied when the boundary was
            // filtered out as stale above: a dead generation is a cancellation,
            // not a satisfied producer, and keeps the ordinary wire watermark.
            PRESENT_EXACT_WATERMARK_USED.fetch_add(1, Ordering::Relaxed);
            (0, WireBoundary::Prefix)
        } else {
            (self.next_wire_fence, WireBoundary::Prefix)
        };
        // UV1's instrument (`WddmHoldMs`, KMD_IMPACT §14a.1). Scoped to D3D12 ECL
        // packets by the record's identity, never by timing or by which context
        // happened to submit: this FIFO is adapter-global and strictly
        // head-of-line, so a hold that could attach to a DWM present would stall
        // the whole desktop. Default 0 makes every line below inert.
        let hold_until_100ns = if d3d12 { self.wddm_hold_deadline() } else { 0 };
        let stream_ready =
            stream_boundary.map_or(true, |boundary| self.scanout_boundary_ready(boundary));
        let blt_ready = match (blt_token, blt_stream_boundary) {
            (Some(token), Some(boundary)) => {
                self.windowed_blt_terminal_prefix_ready(token, boundary)
            }
            (None, _) => true,
            _ => false,
        };
        if self.wddm_pending.is_empty()
            && self.wire_boundary_ready(watermark, domain, wire_boundary)
            && stream_ready
            && blt_ready
            // A held packet must not take the immediate-signal path: that is the
            // exact case the experiment measures (a packet with nothing real to
            // wait for), so the hold has to be able to reach it.
            && hold_until_100ns == 0
            // A WindowedBlt terminal can predate a preempted DMA replay. It
            // still must enter the FIFO so a failed NotifyInterrupt leaves a
            // retryable owner; only the DPC's successful callback consumes
            // the exact terminal prefix.
            && blt_token.is_none()
        {
            return true;
        }
        if self.wddm_pending.len() >= MAX_WDDM_PENDING {
            // Degrade to the old immediate model for this fence — signaling the
            // newest (monotonically largest) fence implicitly completes the
            // queued older ones, so drop them too. Loud and counted.
            //
            // ⚠ THE "PRACTICALLY UNREACHABLE (VidSch queues far fewer than 256)"
            // LINE THAT USED TO BE HERE IS RETIRED (A5, 2026-08-06). It was an
            // assumption about dxgkrnl's queue depths — closed source, and this
            // FIFO is adapter-global across every context, so no single queue depth
            // bounds it — and the D3D12 arm adds a writer at
            // `pfnExecuteCommandLists` frequency rather than at present frequency.
            // What actually keeps this path away is now stated and measured:
            // `WddmHeadMs` bounds how long a head may block on a boundary that may
            // be unsatisfiable, so 256 outstanding entries requires 256 genuinely
            // in-flight producers. If this counter ever moves, `WfBWire`/`WfBReb`
            // say whether the host stopped retiring or the bound was disabled.
            //
            // ⚠ The caller still releases every outstanding scan-out lease when
            // this returns true after an overflow. The leases no longer gate a
            // retirement, but they DO decide the flush executor's ownership
            // gate, and an epoch whose presentation was just dropped on the
            // floor must not read as one that is still coming.
            // `note_wddm_submission` cannot do it itself — the lease state lives
            // on the adapter and this runs under `virtio_lock`, inside
            // `wddm_notify_lock`.
            WDDM_PENDING_OVERFLOWS.fetch_add(1, Ordering::Relaxed);
            let current = match (blt_token, blt_stream_boundary) {
                (Some(token), Some(boundary)) => WindowedBltTerminalPrefix::new(token, boundary),
                _ => None,
            };
            self.overflow_wddm_pending(current);
            return true;
        }
        self.wddm_pending.push_back(WddmPending {
            fence,
            watermark,
            wire_boundary,
            domain,
            stream_boundary,
            blt_token,
            blt_stream_boundary,
            hold_until_100ns,
            head_deadline_100ns: 0,
            rebased: false,
        });
        false
    }

    /// The interrupt-time deadline a held D3D12 packet may not complete before,
    /// or 0 when `WddmHoldMs` is off (the default) or the clock is unusable.
    ///
    /// The clamp is in CODE, not in the operator's registry value: an unbounded
    /// hold on a head-of-line FIFO is a TDR, and a typo must not be able to cause
    /// one. `WDDM_HOLD_MS_MAX` is far below the default `TdrDelay` of 2 s while
    /// still five orders of magnitude above the 0.8–1.1 µs fence-wait baseline the
    /// experiment reads against, so nothing about the reading needs the extra
    /// range.
    fn wddm_hold_deadline(&self) -> u64 {
        let ms = WDDM_HOLD_MS.load(Ordering::Relaxed);
        if ms == 0 {
            return 0;
        }
        let mut qpc_timestamp = 0;
        // SAFETY: `KeQueryInterruptTimePrecise` is a scalar time read legal at any
        // IRQL (the same call this driver already makes at DIRQL in
        // `ddi/display.rs` and from the vsync DPC); it takes no lock and cannot
        // re-enter this transport.
        let now = unsafe { KeQueryInterruptTimePrecise(&mut qpc_timestamp) };
        // 100 ns units. A saturating add keeps an exhausted interrupt-time
        // representation from wrapping into a deadline already in the past.
        now.saturating_add(ms as u64 * 10_000)
    }

    /// Whether the last [`Self::note_wddm_submission`] overflowed the pending
    /// FIFO, and the caller therefore owes a lease release.
    ///
    /// A counter read rather than a second return value, because the overflow is a
    /// rare last resort and threading a tuple through the one call site would put
    /// it in everyone's way. The caller compares before and after;
    /// `WDDM_PENDING_OVERFLOWS` is cumulative and monotonic.
    ///
    /// ⚠ "PRACTICALLY UNREACHABLE" WAS THE OLD WORDING AND IT IS RETIRED (A5): it
    /// rested on an assumption about dxgkrnl's queue depths, and this FIFO is
    /// adapter-global across every context. What keeps it away is now
    /// `WddmHeadMs` — see the overflow arm in [`Self::note_wddm_submission`].
    pub fn wddm_pending_overflows() -> u32 {
        WDDM_PENDING_OVERFLOWS.load(Ordering::Relaxed)
    }

    /// Pop the head-of-FIFO WDDM submission once its venus watermark has been
    /// reached — the app has finished writing the frame. The DPC signals
    /// DMA_COMPLETED for it OUTSIDE the device spinlock.
    ///
    /// The presentation-lease half that also gated this until 22.22.217.0 is
    /// gone; see [`WddmPending`] for the measurement that retired it.
    ///
    /// Strictly head-of-line. A blocked head is never bypassed: dxgkrnl treats
    /// `SubmissionFenceId` as a watermark and requires monotonic completion, so
    /// skipping ahead is bugcheck 0x119/1.
    pub fn take_one_ready_wddm(&mut self, _order: &crate::adapter::NotifyOrdered<'_>) -> WddmTake {
        // TWO PASSES AT MOST, and the loop exists for LIVENESS, not for retrying:
        // `rebase_blocked_head` can succeed at most once per entry
        // (`WddmPending::rebased`), and the `pass` guard bounds it again in case
        // that ever breaks. Without the second pass a rebase would return
        // `BlockedOnProducer`, which ENDS the DPC's drain loop
        // (`ddi/interrupt.rs`), so a head this driver just released would then sit
        // there waiting for an unrelated wake edge — and the whole point of the
        // bound is that no such edge is guaranteed.
        let mut pass = 0u8;
        loop {
            pass += 1;
            let (
                watermark,
                wire_boundary,
                domain,
                stream_boundary,
                blt_token,
                blt_stream_boundary,
                hold_until,
            ) = {
                let Some(head) = self.wddm_pending.front() else {
                    return WddmTake::Empty;
                };
                (
                    head.watermark,
                    head.wire_boundary,
                    head.domain,
                    head.stream_boundary,
                    head.blt_token,
                    head.blt_stream_boundary,
                    head.hold_until_100ns,
                )
            };
            // Evaluated as three named conditions rather than one `||` chain: the
            // FIFO is strictly ordered, so whatever paces its head paces every
            // WDDM fence behind it, and "blocked" without saying ON WHAT is not an
            // instrument. Exactly one counter moves per blocked look.
            if !self.wire_boundary_ready(watermark, domain, wire_boundary) {
                WDDM_HEAD_BLOCKED_WIRE.fetch_add(1, Ordering::Relaxed);
                // ⛔ DELIBERATELY NOT REBASABLE, and this is the load-bearing half
                // of `WddmHeadMs`'s scope. There is nothing to rebase a wire
                // dependency ONTO: the rebase target IS the conservative wire
                // prefix at the current `next_wire_fence`, which for a Prefix head
                // is the same class of wait and for an Exact head is STRICTER (it
                // still includes the named fence, plus everything below it). A wire
                // arm that never clears means the host has stopped retiring work
                // altogether, and the honest outcome of a wedged host is a TDR, not
                // a fence this driver signals on its behalf.
                return WddmTake::BlockedOnProducer;
            }
            if !stream_boundary.map_or(true, |boundary| self.scanout_boundary_ready(boundary)) {
                WDDM_HEAD_BLOCKED_STREAM.fetch_add(1, Ordering::Relaxed);
                if pass == 1 && self.rebase_blocked_head() {
                    continue;
                }
                return WddmTake::BlockedOnProducer;
            }
            if !match (blt_token, blt_stream_boundary) {
                (Some(token), Some(boundary)) => {
                    self.windowed_blt_terminal_prefix_ready(token, boundary)
                }
                (None, _) => true,
                _ => false,
            } {
                WDDM_HEAD_BLOCKED_BLT.fetch_add(1, Ordering::Relaxed);
                if pass == 1 && self.rebase_blocked_head() {
                    continue;
                }
                return WddmTake::BlockedOnProducer;
            }
            // FOURTH ARM, AND DELIBERATELY LAST (`WddmHoldMs`, UV1). Placed after
            // the three real dependencies so `WfBHold` can only mean "an otherwise
            // READY packet was artificially delayed" — which is the experiment's
            // signal — and so the meaning of the other three counters is unchanged.
            // Inert unless the knob is on AND the head is a D3D12 ECL packet.
            //
            // ⚠ NOT REBASABLE EITHER, and the two knobs therefore do not interact:
            // a hold is not a dependency, so `WddmHeadMs` has nothing to rebase it
            // onto, and letting the head bound cut a hold short would make UV1's
            // experiment measure `WddmHeadMs` instead of dxgkrnl. The hold is also
            // reached only when the three real arms are already satisfied, so it can
            // never be the reason a deadline was armed.
            if hold_until != 0 {
                let mut qpc_timestamp = 0;
                // SAFETY: scalar any-IRQL time read, as in `wddm_hold_deadline`.
                let now = unsafe { KeQueryInterruptTimePrecise(&mut qpc_timestamp) };
                if now < hold_until {
                    WDDM_HEAD_BLOCKED_HOLD.fetch_add(1, Ordering::Relaxed);
                    return WddmTake::BlockedOnProducer;
                }
            }
            let Some(pending) = self.wddm_pending.pop_front() else {
                return WddmTake::Empty;
            };
            let terminal_prefix = match (blt_token, blt_stream_boundary) {
                (Some(token), Some(boundary)) => WindowedBltTerminalPrefix::new(token, boundary),
                _ => None,
            };
            WDDM_FENCE_FROM_DPC.fetch_add(1, Ordering::Relaxed);
            return WddmTake::Ready(WddmReady {
                pending,
                terminal_prefix,
            });
        }
    }

    /// CONSUMER-SIDE LIVENESS FOR THE FIFO HEAD (`WddmHeadMs`; `KMD_IMPACT.md`
    /// §14a.2 K-F2 and `docs/dx12/PENDING.md` §1 A5). Arms a deadline on the first
    /// blocked look at this head, and once it expires REBASES the head's
    /// tagged-namespace dependencies onto the conservative wire watermark. Returns
    /// `true` only on the call that actually rebases.
    ///
    /// # ⚠ THE REBASE RELEASES A FENCE WHOSE NAMED PRODUCER MAY NOT HAVE COMPLETED
    ///
    /// That is a LIE, stated plainly because it is one. A present-stream boundary
    /// names a specific producer completion, and on the shipping default the marker
    /// is delivered BEFORE the frame's `vkQueueSubmit` on purpose
    /// (`PsMkAhd`/`PsMkAhdHi` measure exactly that), so the wire prefix this rebases
    /// onto need not cover it. The frame dxgkrnl then hands onward can be one the
    /// app has not finished writing — the 0ab-B stale/black-frame class.
    ///
    /// # Why that trade was made anyway
    ///
    /// `present_stream_marker_boundary` bounds a marker's value in NO way, so a
    /// guest can name a boundary `present_stream_slot_ready` will never satisfy —
    /// and an acceptance-side bound cannot fix it (K-F2 records why: legitimate
    /// lookahead reaches DXVK's `MaxNumQueuedCommandBuffers = 32`, and a forged
    /// value whose process then stops presenting is unsatisfiable at any bound). The
    /// FIFO is ADAPTER-GLOBAL and strictly head-of-line, so such a head blocks every
    /// context including DWM's. The two alternatives are both worse:
    ///
    ///  * the 256-entry overflow escape, which completes 256 queued fences at once
    ///    while their host work is still running AND forces
    ///    `release_all_scanout_leases(Teardown)` — the same lie times 256, plus the
    ///    whole adapter's presentation state;
    ///  * an adapter-wide TDR, i.e. every D3D device in the system lost.
    ///
    /// ⇒ one bounded, counted, per-entry release beats both. `WfBReb` is the price
    /// tag: it must read 0 on a healthy session.
    ///
    /// # What it does NOT touch
    ///
    /// Only the tagged namespaces. The wire arm is not rebasable (see the WIRE arm
    /// in [`Self::take_one_ready_wddm`]), the hold arm is not a dependency, and the
    /// entry's `fence` and FIFO position are untouched — dxgkrnl requires monotonic
    /// `SubmissionFenceId` completion and this must never become a bypass.
    fn rebase_blocked_head(&mut self) -> bool {
        let ms = WDDM_HEAD_MS.load(Ordering::Relaxed);
        if ms == 0 {
            return false;
        }
        let mut qpc_timestamp = 0;
        // SAFETY: `KeQueryInterruptTimePrecise` is a scalar time read legal at any
        // IRQL, as in `wddm_hold_deadline`; it takes no lock and cannot re-enter
        // this transport.
        let now = unsafe { KeQueryInterruptTimePrecise(&mut qpc_timestamp) };
        // Read before the mutable borrow of the FIFO below.
        let rebase_watermark = self.next_wire_fence;
        let abandoned_prefix = {
            let Some(head) = self.wddm_pending.front_mut() else {
                return false;
            };
            // The arm/wait/expire state machine is
            // `helios_kmd_logic::wddm_head_bound::look`, tested there: this crate
            // cannot host a test, and an off-by-one or an unsaturated add here would
            // release fences for a reason that has nothing to do with a producer.
            use helios_kmd_logic::wddm_head_bound as bound;
            match bound::look(ms, now, head.head_deadline_100ns, head.rebased) {
                bound::Action::Disabled | bound::Action::Wait | bound::Action::AlreadyRebased => {
                    return false;
                }
                bound::Action::Arm(deadline) => {
                    // FIRST blocked look at this entry as head.
                    head.head_deadline_100ns = deadline;
                    return false;
                }
                bound::Action::Rebase => {}
            }
            head.rebased = true;
            head.stream_boundary = None;
            head.watermark = rebase_watermark;
            // The rebase target is the LEGACY watermark in every sense: an
            // exclusive prefix over every ring, which is "everything this transport
            // had enqueued at the moment of the rebase". Conservative, and always
            // eventually satisfied unless the host itself has stopped.
            head.wire_boundary = WireBoundary::Prefix;
            head.domain = RetireDomain::IncludingGpu;
            let prefix = head
                .blt_token
                .zip(head.blt_stream_boundary)
                .and_then(|(token, boundary)| WindowedBltTerminalPrefix::new(token, boundary));
            head.blt_token = None;
            head.blt_stream_boundary = None;
            prefix
        };
        if let Some(prefix) = abandoned_prefix {
            // The FIFO entry OWNED this terminal prefix; dropping the dependency
            // without detaching the ownership would strand the reader leases the
            // exact same way the overflow path documents. Same helper, same reason.
            self.abandon_windowed_blt_wddm_prefix(prefix);
        }
        WDDM_HEAD_REBASED.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Put a popped-but-undelivered submission back at the head of the FIFO.
    ///
    /// dxgkrnl requires monotonic SubmissionFenceId completion, so the entry
    /// must go back where it came from — `push_front` of the entry just popped
    /// is the only correct form. It also cannot exceed the
    /// `VecDeque::with_capacity(MAX_WDDM_PENDING)` reserve, so nothing
    /// reallocates under the device spinlock.
    pub fn requeue_wddm_front(
        &mut self,
        _order: &crate::adapter::NotifyOrdered<'_>,
        ready: WddmReady,
    ) {
        // Terminal membership remains in the FIFO until the notification
        // succeeds, so requeue only restores the WDDM entry. A failed callback
        // cannot lose a merged same-stream prefix.
        self.wddm_pending.push_front(ready.pending);
    }

    /// Preemption: drop every pending WDDM submission (dxgkrnl resubmits the
    /// unfinished DMA buffers with fresh fence ids after the preempt completes;
    /// the underlying venus work keeps executing host-side). Returns the count
    /// dropped.
    pub fn preempt_flush(&mut self, _order: &crate::adapter::NotifyOrdered<'_>) -> u32 {
        let n = self.wddm_pending.len() as u32;
        self.wddm_pending.clear();
        // Scheduler residency admission was revoked. A request that has not
        // reached ring 1 must await the resubmitted DMA buffer's exact token;
        // dispatching it merely because its producer happened to retire would
        // race destination paging.
        for request in self.windowed_blt.pending.iter_mut() {
            if !request.dispatched {
                request.admitted = false;
            }
        }
        self.windowed_blt.ready.clear();
        n
    }

    /// Scanout-0 preferred `(width, height)` from the host's `GET_DISPLAY_INFO` at
    /// init, or `None` if the host reported nothing usable. The display half drives
    /// its VidPn mode + generated EDID from this so it presents the size QEMU wants.
    pub fn display_mode(&self) -> Option<(u32, u32)> {
        self.display_mode
    }

    /// Mapped kernel VA of the virtio ISR-status register (read-to-clear), or 0 if
    /// the device exposes no ISR cap. `DxgkDdiStartDevice` copies this into the
    /// `AdapterContext` so the DIRQL ISR can acknowledge the INTx line lock-free.
    pub fn isr_status_addr(&self) -> usize {
        self.isr_status_va
    }
}

// The `#[cfg(test)] mod present_stream_tests` that used to sit HERE was moved to
// `helios_kmd_logic::present_stream_boundary_tests` on 2026-08-06, together with
// the pure helpers it covered. FIVE tests (not six, as `docs/dx12/PENDING.md` §6
// said), none of which had ever executed: this crate is a `panic = "abort"`
// cdylib whose `build.rs` runs bindgen and shells to `rc.exe`, so a libtest
// harness cannot exist here at all — CLAUDE.md's invariant table says exactly
// that. Do not reintroduce tests in this file; add them to `kmd_logic`.

impl Drop for VirtioGpu {
    fn drop(&mut self) {
        // Quiesce the device before ending reader leases: unlike ordinary
        // scheduler reset, transport Drop is terminal and no host DMA may
        // retain a snapshot once this reset returns.
        self.transport.set_status(DeviceStatus::empty());
        self.abort_windowed_blt_for_terminal_transport();
        // Stop/StartDevice destroys this transport generation.  Clear stream
        // registrations before resource ids or device owner tokens can be
        // recycled by the next generation.
        self.purge_all_present_streams();
        // Drop any fence-event registrations still parked: dereference WITHOUT
        // signaling (the fences will never retire on a dead transport, and a
        // signal here would report fake completion — the waiter's own deadline
        // fires instead, and its unregister sees NOT_FOUND with an UNSIGNALED
        // event, which the ICD treats as failure). PASSIVE_LEVEL, outside the
        // device lock, so the deferred-delete variant is not required — but it
        // is unconditionally legal, and using it keeps a single deref path.
        for e in self.fence_events.drain(..) {
            FENCE_EVENT_TEARDOWN_DROPS.fetch_add(1, Ordering::Relaxed);
            // SAFETY: the entry owns an object reference taken at registration.
            unsafe { ObDereferenceObjectDeferDelete(e.event.as_ptr() as PVOID) };
        }

        // The reset above quiesced the device before the in-flight/parked entry
        // buffers free with this struct.
        //
        // The BAR MMIO mappings made inside `PciTransport` are intentionally NOT
        // freed here: `WdkHal` caches them by physical address and reuses them on
        // the next StartDevice (the BARs are stable across stop/start), so there
        // is no per-cycle leak. The cache is released wholesale in
        // `DxgkDdiUnload` via `WdkHal::unmap_all`.
    }
}
