//! UNSAMPLED scanout-bind trace — the instrument for "why does the bind stay
//! pinned to one resource during a fullscreen app?".
//!
//! Every other scanout diagnostic in this driver is *sampled*: `Sc*`, `PB*` and
//! `Rf*` are written on the 1st call and every 600th, or behind
//! [`crate::diag::sample_tick`]. Registry values also persist across boots. A
//! single `reg query` of any of them therefore proves nothing about what
//! happened during a 50 s workload, and four wrong root-cause mechanisms were
//! derived from over-reading exactly those values (see `tmp/perf/HANDOFF.md`).
//!
//! So this module separates the two things that were conflated:
//!
//!   * ACCUMULATION is unsampled and lock-free — plain atomic stores, legal at
//!     any IRQL (the `SetVidPnSourceAddress` DIRQL half writes here), with no
//!     registry transaction on the path. Every call is recorded.
//!   * PUBLICATION is throttled — [`dump`] mirrors the whole accumulated state
//!     into the service key from ONE PASSIVE site on a fixed cadence.
//!
//! What it records:
//!
//!   * [`RING`] — the last 16 [`crate::ddi::display::program_vidpn_source`]
//!     calls with the exact `hAllocation`, source resource id, target resource
//!     id, outcome and refusal. Answers "was the DDI even called, and what did
//!     it name?".
//!   * [`SOURCE_HISTOGRAM`] — every distinct source resource id Windows named,
//!     with its call count, for the whole boot. Answers "did Windows only ever
//!     hand us one primary, or did it rotate and we refused?".
//!   * [`FLUSH_HISTOGRAM`] — every distinct resource id a `RESOURCE_FLUSH`
//!     actually named. Answers "which resource is the host being told to
//!     re-read?" — the direct guest-side counterpart of the QEMU
//!     `virtio_gpu_cmd_res_flush` histogram in the handoff.
//!   * the entry/deferral/bind counters below.
//!
//! Everything here is diagnostic: nothing in this module gates, delays or
//! changes a scanout decision.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Last-N `program_vidpn_source` calls kept in memory. 16 is what one dump can
/// publish without turning the throttled snapshot into a registry storm (4
/// values per entry).
const RING_LEN: usize = 16;

/// Distinct resource ids tracked per histogram. DWM rotates a 3-buffer chain
/// and an app's swapchain is typically 2-4 deep, so 8 covers both plus the
/// adapter-owned LINEAR target with room to show a transition.
const HIST_LEN: usize = 8;

/// One recorded `program_vidpn_source` call.
struct Slot {
    /// `(seq << 16) | (reject << 8) | flags`. See [`Flags`] and
    /// [`RejectCode`].
    packed: AtomicU32,
    /// `WindowsPrimary::resource_id` — the venus resource behind the exact
    /// allocation Windows named. 0 if the handle did not resolve.
    source_resource: AtomicU32,
    /// `ScanoutTarget::resource_id()` — what would be/was bound. 0 if the call
    /// refused before a target existed.
    target_resource: AtomicU32,
    /// Low 32 bits of `hAllocation`. Distinguishes allocations; not a pointer
    /// anyone dereferences.
    alloc: AtomicU32,
}

impl Slot {
    const NEW: Self = Self {
        packed: AtomicU32::new(0),
        source_resource: AtomicU32::new(0),
        target_resource: AtomicU32::new(0),
        alloc: AtomicU32::new(0),
    };

    fn clear(&self) {
        self.packed.store(0, Ordering::Relaxed);
        self.source_resource.store(0, Ordering::Relaxed);
        self.target_resource.store(0, Ordering::Relaxed);
        self.alloc.store(0, Ordering::Relaxed);
    }
}

/// One distinct-value counter.
struct HistSlot {
    value: AtomicU32,
    count: AtomicU32,
}

impl HistSlot {
    const NEW: Self = Self {
        value: AtomicU32::new(0),
        count: AtomicU32::new(0),
    };
}

/// A distinct-value histogram over resource ids.
///
/// Deliberately NOT a ring: the question it answers ("is the same resource
/// named every time?") needs whole-run coverage, and a run is thousands of
/// calls. Slot claiming is a CAS, so a lost race duplicates a value across two
/// slots — the counts still sum correctly and the distinct set is still right,
/// which is all the answer needs.
pub(crate) struct Histogram {
    slots: [HistSlot; HIST_LEN],
    /// Calls whose value found neither a matching nor a free slot. Nonzero
    /// means the distinct set below is incomplete.
    overflow: AtomicU32,
    /// Total calls, including overflowed ones. The sum of the slot counts is
    /// only equal to this while `overflow` is 0.
    total: AtomicU32,
}

impl Histogram {
    const fn new() -> Self {
        Self {
            slots: [HistSlot::NEW; HIST_LEN],
            overflow: AtomicU32::new(0),
            total: AtomicU32::new(0),
        }
    }

    /// Record one occurrence of `value`. Lock-free; legal at any IRQL.
    ///
    /// `value == 0` is recorded like any other, because "the flush named
    /// resource 0" is itself news.
    pub(crate) fn note(&self, value: u32) {
        self.total.fetch_add(1, Ordering::Relaxed);
        // A claimed slot is identified by count != 0, not by value != 0, so a
        // genuine `value == 0` gets a slot instead of colliding with "free".
        let mut i = 0;
        while i < HIST_LEN {
            let slot = &self.slots[i];
            if slot.count.load(Ordering::Relaxed) != 0
                && slot.value.load(Ordering::Relaxed) == value
            {
                slot.count.fetch_add(1, Ordering::Relaxed);
                return;
            }
            i += 1;
        }
        let mut i = 0;
        while i < HIST_LEN {
            let slot = &self.slots[i];
            if slot
                .count
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                slot.value.store(value, Ordering::Release);
                return;
            }
            i += 1;
        }
        self.overflow.fetch_add(1, Ordering::Relaxed);
    }

    fn reset(&self) {
        let mut i = 0;
        while i < HIST_LEN {
            self.slots[i].count.store(0, Ordering::Relaxed);
            self.slots[i].value.store(0, Ordering::Relaxed);
            i += 1;
        }
        self.overflow.store(0, Ordering::Relaxed);
        self.total.store(0, Ordering::Relaxed);
    }

    /// Mirror into `<prefix>R<n>` / `<prefix>C<n>` plus `<prefix>Tot` and
    /// `<prefix>Ovf`. PASSIVE_LEVEL only. `prefix` must be 2 ASCII bytes.
    fn dump(&self, prefix: [u8; 2]) {
        let mut i = 0;
        while i < HIST_LEN {
            let digit = b'0' + i as u8;
            crate::diag::record_named_bytes(
                &[prefix[0], prefix[1], b'R', digit],
                self.slots[i].value.load(Ordering::Relaxed),
            );
            crate::diag::record_named_bytes(
                &[prefix[0], prefix[1], b'C', digit],
                self.slots[i].count.load(Ordering::Relaxed),
            );
            i += 1;
        }
        crate::diag::record_named_bytes(
            &[prefix[0], prefix[1], b'T', b'o', b't'],
            self.total.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            &[prefix[0], prefix[1], b'O', b'v', b'f'],
            self.overflow.load(Ordering::Relaxed),
        );
    }
}

/// Flag bits in the low byte of [`Slot::packed`]. Owner-readable ABI: do not
/// renumber, only append.
pub(crate) mod flags {
    /// The target was already the bound scanout, so no `SET_SCANOUT_BLOB` was
    /// issued. **This bit set on every call while the host sees zero blobs is
    /// the pinned-bind signature.**
    pub const ALREADY_BOUND: u32 = 0x01;
    /// The call came through the PASSIVE worker because the DDI ran above
    /// PASSIVE (dxgkrnl's MMIO-flip path at DIRQL).
    pub const DEFERRED: u32 = 0x02;
    /// `WindowsPrimary::direct_scanout` — the zero-copy arm.
    pub const DIRECT: u32 = 0x04;
    /// `SET_SCANOUT_BLOB` was issued and succeeded on this call.
    pub const BOUND: u32 = 0x08;
    /// Returned `ScanoutOutcome::Programmed` (zero-copy primary published).
    pub const PROGRAMMED: u32 = 0x10;
    /// Returned `ScanoutOutcome::CopyQueued` (LINEAR fallback copy on ring 1).
    pub const COPY_QUEUED: u32 = 0x20;
    /// A direct worker epoch was older than the binding already published.
    pub const SUPERSEDED: u32 = 0x40;
    /// A direct worker carried the bound epoch but a conflicting identity.
    pub const EPOCH_CONFLICT: u32 = 0x80;
}

/// `dxgkddi_set_vidpn_source_address` entries, unsampled. Includes the calls
/// that never reach `program_vidpn_source`.
static DDI_ENTRIES: AtomicU32 = AtomicU32::new(0);
/// Entries taken above PASSIVE and therefore deferred to the worker.
static DDI_DEFERRED: AtomicU32 = AtomicU32::new(0);
/// Entries that ran the programming body inline at PASSIVE.
static DDI_IMMEDIATE: AtomicU32 = AtomicU32::new(0);
/// Entries with a NULL `pSetVidPnSourceAddress`, which return SUCCESS untouched.
static DDI_NULL_ARG: AtomicU32 = AtomicU32::new(0);
/// Entries whose `hAllocation` could not be paired with its address.
static DDI_PAIR_FAILED: AtomicU32 = AtomicU32::new(0);
/// Deferred stores that overwrote a DIFFERENT still-unprocessed handle — the
/// coalescing rate. dxgkrnl flipping faster than the PASSIVE worker drains
/// means intermediate primaries are never programmed at all.
static DDI_COALESCED: AtomicU32 = AtomicU32::new(0);
/// The handle most recently overwritten by [`DDI_COALESCED`], low 32 bits.
static DDI_COALESCED_LOST: AtomicU32 = AtomicU32::new(0);

/// `program_vidpn_source` entries, unsampled.
static PROGRAM_CALLS: AtomicU32 = AtomicU32::new(0);
/// Calls that issued `SET_SCANOUT_BLOB`.
static PROGRAM_BINDS: AtomicU32 = AtomicU32::new(0);
/// Calls that skipped the bind because `already_bound` held.
static PROGRAM_SKIPS: AtomicU32 = AtomicU32::new(0);

/// Ring write cursor. `AtomicUsize` so the modulo is index-typed.
static RING_CURSOR: AtomicUsize = AtomicUsize::new(0);
static RING: [Slot; RING_LEN] = [Slot::NEW; RING_LEN];

/// `dxgkddi_present` entries, by arm. Unsampled: the first run of this
/// instrument showed `SetVidPnSourceAddress` going completely silent for the
/// whole of a fullscreen workload, so the next question is whether dxgkrnl
/// stopped asking us to FLIP as well, or only stopped naming a source address.
static PRESENT_CALLS: AtomicU32 = AtomicU32::new(0);
/// Presents with `DXGK_PRESENTFLAGS.Blt`.
static PRESENT_BLTS: AtomicU32 = AtomicU32::new(0);
/// Presents with `DXGK_PRESENTFLAGS.Flip`.
static PRESENT_FLIPS: AtomicU32 = AtomicU32::new(0);
/// Flips that took the `FlipOnVSyncMmIo` arm (no DMA buffer), i.e. the ones
/// dxgkrnl completes through `SetVidPnSourceAddress`.
static PRESENT_MMIO_FLIPS: AtomicU32 = AtomicU32::new(0);

/// Present markers whose resource was, at that instant, the resource BOUND to
/// scanout 0. Unsampled; `MkTot` is the denominator.
///
/// A present marker means "the app has just finished writing this buffer". If
/// the buffer was simultaneously the bound scan-out, then the app was writing
/// the buffer the host was displaying — and with the host on
/// `-display sdl,gl=on` the scan-out blob is a dmabuf texture with NO CPU
/// readback (`screendump` answers "no surface"), so SDL shows that memory LIVE.
/// A flip-model backbuffer is cleared to black at frame start, which makes
/// "app writes the displayed buffer" and "brief black flash" the same event.
///
/// On a correctly rotating chain this must stay near ZERO: the app renders into
/// a buffer that is not the one on screen. A high rate is defect 0ab's
/// mechanism, measured rather than argued.
static MARKER_WHILE_BOUND: AtomicU32 = AtomicU32::new(0);

/// Flips that arrived on the DMA-BUFFER contract (a real `pDmaBuffer`), which
/// is the path `PresentFlipPrivate` implements. The complement of
/// [`PRESENT_MMIO_FLIPS`]: together they must account for every `VpFlip`.
static PRESENT_DMA_FLIPS: AtomicU32 = AtomicU32::new(0);
/// DMA flips whose record the submit path picked up and armed. `VpDmaF` minus
/// this is flips that reached the scheduler carrying no readable record — it
/// must read 0, because each one is a flip the display never programmed.
static PRESENT_DMA_FLIPS_ARMED: AtomicU32 = AtomicU32::new(0);
/// Distinct source resource ids arriving on the DMA-flip path.
pub(crate) static DMA_FLIP_HISTOGRAM: Histogram = Histogram::new();

/// Distinct source resource ids `program_vidpn_source` was asked to bind.
static SOURCE_HISTOGRAM: Histogram = Histogram::new();
/// Distinct source resource ids dxgkrnl named in the FLIP arm of
/// `dxgkddi_present`.
pub(crate) static PRESENT_FLIP_HISTOGRAM: Histogram = Histogram::new();
/// Distinct `DXGKARG_PRESENT.Flags` words, unsampled.
///
/// `PBflag` records the same word but is sampled, so it cannot show that the
/// desktop and a fullscreen app present through DIFFERENT arms — which is what
/// the flip census turned out to hinge on. Bit order (bindgen, WDK
/// 10.0.26100): 0 `Blt`, 1 `ColorFill`, 2 `Flip`, 3 `FlipWithNoWait`,
/// 4 `SrcColorKey`, 5 `DstColorKey`, 6 `LinearToSrgb`, 7 `Rotate`.
pub(crate) static PRESENT_FLAGS_HISTOGRAM: Histogram = Histogram::new();
/// `(FlipInterval << 8) | has_dma_buffer`, unsampled.
///
/// `DXGKARG_PRESENT.Flags` turned out to be IDENTICAL (`Flip|FlipWithNoWait`)
/// for DWM's flips and a fullscreen app's, so the flags are not what selects
/// the arm. What differs is `pDmaBuffer`: NULL for DWM (the `FlipOnVSyncMmIo`
/// contract, completed through `SetVidPnSourceAddress`) and non-NULL for the
/// app (a DMA-buffer flip, which `SetVidPnSourceAddress` never follows). This
/// pairs the surviving candidate — the requested flip interval — with that
/// distinction in one value.
pub(crate) static PRESENT_INTERVAL_HISTOGRAM: Histogram = Histogram::new();
/// Distinct resource ids handed to `resource_flush_async`.
pub(crate) static FLUSH_HISTOGRAM: Histogram = Histogram::new();
/// Distinct resource ids named by the UMD's present marker
/// (`arm_scanout_refresh_after_current_venus`).
///
/// This is the third leg of the comparison the defect needs and the only one
/// that was missing: `Vs*` is what Windows asked us to BIND, `Ff*` is what we
/// told the host to RE-READ, and this is what the app actually PRESENTED. If a
/// fullscreen app rotates four backbuffers here while `Vs*` and `Ff*` each show
/// one id, the pin is upstream of the bind path, not inside it.
pub(crate) static MARKER_HISTOGRAM: Histogram = Histogram::new();

/// Bind-edge refreshes whose Venus work had ALREADY retired when the bind ran,
/// so the flush was issued immediately. Unsampled.
static BIND_REFRESH_READY: AtomicU32 = AtomicU32::new(0);
/// Bind-edge refreshes DEFERRED to the completion DPC because the presented
/// frame's Venus work was still outstanding at bind time. Unsampled.
///
/// This is the defect-0ab denominator: every one of these used to be an
/// immediate RESOURCE_FLUSH of a buffer the host GPU had only cleared, i.e. one
/// black frame on screen until the marker edge repainted it ~10 ms later. A
/// number near `VpBind` is the expected shape under the DMA-buffer flip
/// contract, which arms the programming while the flip's fence is outstanding.
static BIND_REFRESH_DEFERRED: AtomicU32 = AtomicU32::new(0);
/// Bind-edge refreshes armed against the boundary this buffer's own PRESENT
/// MARKER captured (`VirtioGpu::note_present_marker`). Unsampled.
///
/// This is the 0ab-B fix's own denominator: the alternative is to sample the
/// boundary inside the bind, which names a later frame and makes the flush wait
/// past the buffer's recycle. Under a fullscreen DMA-flip workload this should
/// track `VpBind`; a number near zero means the flips are not reaching
/// `arm_dma_flip_programming` and the fix is inert.
static BIND_REFRESH_CARRIED: AtomicU32 = AtomicU32::new(0);
/// Largest number of async Venus fences a DEFERRED bind-edge arm was still
/// waiting on, and the ring of the lowest such fence on the last deferral
/// (0 = host decode, >= 1 = host GPU). Unsampled.
static BIND_WAIT_MAX: AtomicU32 = AtomicU32::new(0);
static BIND_WAIT_LAST: AtomicU32 = AtomicU32::new(0);
static BIND_WAIT_RING: AtomicU32 = AtomicU32::new(0);

/// Bind-edge refreshes that had NO recorded boundary and sampled one. Expected
/// on the MMIO/desktop path, where dxgkrnl retires the flip before calling
/// `SetVidPnSourceAddress`, so "now" is that frame's boundary.
static BIND_REFRESH_SAMPLED: AtomicU32 = AtomicU32::new(0);
/// Bind-edge refreshes that used the boundary their FLIP took and stamped on the
/// allocation, rather than reading the shared mark table (`BeAcar`). Unsampled.
///
/// A subset of `BeCar`. Near zero under a fullscreen DMA-flip workload means
/// D1(i) is inert — the flips are not reaching `arm_dma_flip_programming`.
static BIND_REFRESH_ALLOC_CARRIED: AtomicU32 = AtomicU32::new(0);
/// Binds where the mark TABLE still held a boundary for this buffer that differs
/// from the one the flip carried (`BeOvw`). Unsampled.
///
/// THE OVERWRITE CENSUS, and the number that confirms or kills D1(i)'s
/// mechanism: each one is a bind that would have honoured a boundary belonging
/// to the same buffer's NEXT present — the frame-late wait that put the flush in
/// the post-clear window. Expect it near the old `BeDef` rate if the mechanism is
/// right, and near zero if it is not. Either answer is reportable.
static BIND_REFRESH_OVERWRITTEN: AtomicU32 = AtomicU32::new(0);

// ── The ownership gate on the flush executor (ROADMAP defect 0ab-B, D2) ──────
//
// Both UNSAMPLED, because a dropped refresh is a refusal and this driver's rule
// is that every refusal is countable. Read them against `FfTot` (flushes
// actually issued): the gate is meant to remove the surplus ~30 % of publishes,
// not most of them, and `OgIdn` consuming the desktop's flushes is the 0aa
// regression shape the acceptance battery checks for.

/// Refreshes dropped because the armed frame is not the bound one (`OgIdn`).
static OWNERSHIP_DROP_IDENTITY: AtomicU32 = AtomicU32::new(0);
/// Refreshes dropped because this binding generation was already published and a
/// newer presentation is outstanding (`OgEpo`).
static OWNERSHIP_DROP_EPOCH: AtomicU32 = AtomicU32::new(0);

// ── The D4b snapshot bind (FIX-DESIGN-d4b-snapshot.md §4) ────────────────────
//
// Both UNSAMPLED. `SnSub` against `VpDmaF` is the substitution census — the §7
// prediction is `SnSub ≈ VpDmaF` under a fullscreen DMA-flip workload and ~0 on
// the desktop (MMIO path, no private channel). `SnFbk` is every flagged
// descriptor that failed a Present-time check (plus the structurally-
// unreachable re-validation at a bind site) and fell back to binding the
// flipped allocation; a significant fraction of flips here is the §8 abort
// signal.

/// DMA flips whose validated snapshot descriptor substituted the bind target
/// (`SnSub`). Counted ONCE per flip, at the Present DDI's decision site.
static SNAPSHOT_SUBSTITUTED: AtomicU32 = AtomicU32::new(0);
/// Flagged descriptors rejected -> bound the flipped allocation instead
/// (`SnFbk`).
static SNAPSHOT_FALLBACK: AtomicU32 = AtomicU32::new(0);

// ── The DISPATCH-level fast bind (ROADMAP defect 0ab-C, D1(ii)) ──────────────
//
// All UNSAMPLED, because every one of them answers a question that a sampled
// value cannot: whether the accelerator ran at all, and if not, why. The prior
// says `FpBind` should approach the flip rate (`VpDmaF`) with `FpApply` right
// behind it and `FpBusy` small; `FpBind` near zero with `FpSkip` large means the
// fast path is bailing out and the SKIP HISTOGRAM says on which arm — which is
// the difference between "the fix is inert" and "the fix is wrong", a
// distinction this defect's history has cost several deploy cycles.

/// Fast binds enqueued on the control queue from the flip arm (`FpBind`).
static FAST_BIND_ENQUEUED: AtomicU32 = AtomicU32::new(0);
/// Fast binds whose OK response applied its bookkeeping (`FpApply`).
static FAST_BIND_APPLIED: AtomicU32 = AtomicU32::new(0);
/// Bind applications refused by the wire-order guard — a LATER bind's
/// bookkeeping had already been applied (`FpLate`). Counted at BOTH application
/// sites: the drain's fast-bind arm and the PASSIVE worker's post-bind tail.
static FAST_BIND_LATE: AtomicU32 = AtomicU32::new(0);
/// Fast binds coalesced by either a third unready frontier submission or two
/// completions in the same drain pass (`FpCoal`).
///
/// Kept separate from `FpLate` deliberately: they are both "bookkeeping that did
/// not get applied", but one is bounded frontier/completion coalescing and the
/// other is the wire-order guard refusing a stale application, and summing them
/// would hide whichever is happening — the same reason the `Ls*` end reasons are
/// never summed.
static FAST_BIND_COALESCED: AtomicU32 = AtomicU32::new(0);
/// Fast requests discarded because a newer presentation SET descriptor had
/// already been accepted (`FpSup`). This is intentionally distinct from
/// `FpCoal`, which only counts replacement of the bounded trailing slot.
static FAST_BIND_SUPERSEDED: AtomicU32 = AtomicU32::new(0);
/// Flip arms that found EVERY preallocated command buffer still in flight
/// (`FpBusy`).
///
/// Expected ~0 since 22.22.221.0. With one buffer it was 1776/run (~18 % of
/// flips): a buffer is only returned by the guest's DPC drain, which lags the
/// host's consume by several flip periods under load, so the round-trip is not
/// the relevant timescale — the RETURN is, and the pool covers it.
///
/// ⚠ It is ALSO what failed pool allocations at transport init read as, for
/// every eligible flip. The two are distinguishable without another counter:
/// an empty pool gives `FpBind` = 0 with `FpBusy` = every eligible flip, while
/// real contention gives `FpBind` ≫ `FpBusy`.
static FAST_BIND_BUSY: AtomicU32 = AtomicU32::new(0);
/// Fast binds that never reached the host, or that the host refused (`FpErr`).
/// Nothing is applied for them; the worker's own validate/retry ladder is the
/// recovery path, exactly as it is today.
static FAST_BIND_ERRORS: AtomicU32 = AtomicU32::new(0);
/// Flip arms that declined to fast-bind (`FpSkip`). The reason breakdown is
/// [`FAST_BIND_SKIP_HISTOGRAM`].
static FAST_BIND_SKIPS: AtomicU32 = AtomicU32::new(0);
/// Direct worker requests terminally rejected because a newer epoch already
/// owns the active binding (`WpSup`).
static WORKER_EPOCH_SUPERSEDED: AtomicU32 = AtomicU32::new(0);
/// Direct worker requests terminally rejected because the same epoch named a
/// conflicting active identity (`WpCfl`).
static WORKER_EPOCH_CONFLICT: AtomicU32 = AtomicU32::new(0);
/// Why each `FpSkip` happened, by [`skip`] code.
pub(crate) static FAST_BIND_SKIP_HISTOGRAM: Histogram = Histogram::new();

/// Fast-bind bail-out reasons, recorded in [`FAST_BIND_SKIP_HISTOGRAM`].
///
/// Owner-readable ABI like [`flags`]: append, do not renumber. Codes start at 1
/// so a `0` slot value is distinguishable from an unclaimed slot in the dump.
pub(crate) mod skip {
    /// `DispatchBind=0` — the A/B disable.
    pub const KNOB_OFF: u32 = 1;
    /// `BindFlushMode=1` — the mode-1 diagnostic measures the OLD path, so the
    /// accelerator must not perturb it.
    pub const DIAG_MODE: u32 = 2;
    /// The handle did not resolve, or the primary is not a direct scan-out (the
    /// adapter-owned LINEAR fallback binds from the worker as before).
    pub const NOT_DIRECT: u32 = 3;
    /// The primary failed the direct-scan-out layout validation — the undersize
    /// guard that keeps QEMU from reading past the blob.
    pub const LAYOUT: u32 = 4;
    /// Not the steady bound extent: a mode change, or the very first bind of a
    /// generation. Both belong to the worker, which owns mode-set ordering.
    pub const EXTENT: u32 = 5;
    /// This resource is already the bound scan-out, so there is no rebind to
    /// accelerate.
    pub const ALREADY_BOUND: u32 = 6;
    /// The transport is down.
    pub const NO_TRANSPORT: u32 = 7;
}

// ── Scan-out presentation LEASES (ROADMAP defect 0ab-B) ──────────────────────
//
// The presentation-epoch bookkeeping's own telemetry, all UNSAMPLED. Every one
// of these exists because of a specific way the fix could be inert or wrong and
// look fine: a gate that never fires (`OgIdn` + `OgEpo` ≈ 0) is the 22.22.208.0
// trap; epochs that all end as `LsCanc`/`LsSupe` rather than `LsEndR` mean the
// flush path is racing the defect instead of closing it; `LsStal` rising means
// it is issuing reads nobody is waiting for. Merging the reasons hides all
// three, which is why they are never summed.

/// The reason one presentation epoch's host-reader lease ended. Re-exported so
/// call sites name it without importing the logic crate.
pub(crate) use helios_kmd_logic::scanout_lease::LeaseEnd;

/// Presentation epochs minted (`LsMint`).
static LEASE_MINTED: AtomicU32 = AtomicU32::new(0);
/// `RESOURCE_FLUSH` reads queued carrying a lease token (`LsQued`).
static LEASE_READ_QUEUED: AtomicU32 = AtomicU32::new(0);
/// Lease tokens whose control response returned, success or error (`LsRead`).
static LEASE_READ_DONE: AtomicU32 = AtomicU32::new(0);
/// Epochs ended by a completed host read — the healthy reason (`LsEndR`).
static LEASE_END_READ: AtomicU32 = AtomicU32::new(0);
/// Epochs ended because a later bind proved no read remains (`LsSupe`).
static LEASE_END_SUPERSEDED: AtomicU32 = AtomicU32::new(0);
/// Epochs ended by an enqueue failure, host error or allocation retire (`LsCanc`).
static LEASE_END_CANCELLED: AtomicU32 = AtomicU32::new(0);
/// Epochs ended by transport failure, preemption/TDR, reset or stop (`LsTear`).
static LEASE_END_TEARDOWN: AtomicU32 = AtomicU32::new(0);
/// Completions at or behind the watermark: coalesced, duplicated or reordered
/// tokens. Inert by construction, counted so "inert" is measured (`LsStal`).
static LEASE_STALE: AtomicU32 = AtomicU32::new(0);
/// Primary addresses published to the CRTC_VSYNC heartbeat at a bind (`LsPub`).
static LEASE_PRIMARY_PUBLISHED: AtomicU32 = AtomicU32::new(0);

/// Value names this module wrote until 22.22.217.0 and no longer produces.
///
/// Zeroed once at StartDevice and never written again. Registry values persist
/// across boots, so a retired counter left at its last value reads exactly like
/// a live one — which is the misreading this whole module exists to prevent.
/// `LsBlk`/`LsRel` were the withholding's decision census, `LsPump`/`LsWait` the
/// liveness pump's, and `LsPubG` the CAS give-up in the withheld-address
/// publication; all four mechanisms are gone.
const RETIRED_VALUE_NAMES: [&[u8]; 7] = [
    b"LsBlk", b"LsRel", b"LsPump", b"LsWait", b"LsPubG",
    // 0ab-C close-out: the dead PresentCb-private channel's diagnostics.
    b"PBIdOk", b"PBpdsz",
];

/// Note one minted presentation epoch. Any IRQL.
pub(crate) fn note_lease_minted() {
    LEASE_MINTED.fetch_add(1, Ordering::Relaxed);
}

/// Note one `RESOURCE_FLUSH` issued with a lease token. PASSIVE (flush path).
pub(crate) fn note_lease_read_queued() {
    LEASE_READ_QUEUED.fetch_add(1, Ordering::Relaxed);
}

/// Note one lease token completing. Any IRQL — runs in the used-ring drain.
pub(crate) fn note_lease_read_done() {
    LEASE_READ_DONE.fetch_add(1, Ordering::Relaxed);
}

/// Note one epoch's lease ending, by reason. Any IRQL.
pub(crate) fn note_lease_end(reason: LeaseEnd) {
    match reason {
        LeaseEnd::HostRead => &LEASE_END_READ,
        LeaseEnd::Superseded => &LEASE_END_SUPERSEDED,
        LeaseEnd::Cancelled => &LEASE_END_CANCELLED,
        LeaseEnd::Teardown => &LEASE_END_TEARDOWN,
    }
    .fetch_add(1, Ordering::Relaxed);
}

/// Note a completion whose token was at or behind the watermark. Any IRQL.
pub(crate) fn note_lease_stale() {
    LEASE_STALE.fetch_add(1, Ordering::Relaxed);
}

/// Note a bind publishing its primary address to the VSync heartbeat. Any IRQL.
pub(crate) fn note_lease_primary_published() {
    LEASE_PRIMARY_PUBLISHED.fetch_add(1, Ordering::Relaxed);
}

/// Note a refresh dropped by the ownership gate's IDENTITY arm. Any IRQL.
pub(crate) fn note_ownership_drop_identity() {
    OWNERSHIP_DROP_IDENTITY.fetch_add(1, Ordering::Relaxed);
}

/// Note a refresh dropped by the ownership gate's EPOCH arm. Any IRQL.
pub(crate) fn note_ownership_drop_epoch() {
    OWNERSHIP_DROP_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// Note one flip whose snapshot descriptor validated and substituted the bind
/// target. Any IRQL (the Present DDI's flip arm).
pub(crate) fn note_snapshot_substituted() {
    SNAPSHOT_SUBSTITUTED.fetch_add(1, Ordering::Relaxed);
}

/// Note one flagged snapshot descriptor rejected — the bind fell back to the
/// flipped allocation. Any IRQL.
pub(crate) fn note_snapshot_fallback() {
    SNAPSHOT_FALLBACK.fetch_add(1, Ordering::Relaxed);
}

/// Note one fast bind reaching the control queue. DISPATCH (the flip arm).
pub(crate) fn note_fast_bind_enqueued() {
    FAST_BIND_ENQUEUED.fetch_add(1, Ordering::Relaxed);
}

/// Note one fast bind whose completion applied its bookkeeping. Any IRQL —
/// runs in the used-ring drain's DPC.
pub(crate) fn note_fast_bind_applied() {
    FAST_BIND_APPLIED.fetch_add(1, Ordering::Relaxed);
}

/// Note one bind application refused by the wire-order guard. Any IRQL.
pub(crate) fn note_fast_bind_late() {
    FAST_BIND_LATE.fetch_add(1, Ordering::Relaxed);
}

/// Note a fast bind coalesced by the bounded frontier or same-drain completion
/// handoff. Any IRQL — runs under `virtio_lock`.
pub(crate) fn note_fast_bind_coalesced() {
    FAST_BIND_COALESCED.fetch_add(1, Ordering::Relaxed);
}

/// Record an epoch-floor discard of a fast/deferred presentation request. The
/// periodic PASSIVE trace flush exposes this as `FpSup`; incrementing itself is
/// safe under the virtio lock at DISPATCH.
pub(crate) fn note_fast_bind_superseded() {
    FAST_BIND_SUPERSEDED.fetch_add(1, Ordering::Relaxed);
}

/// Note a flip arm that found the command buffer still in flight. Any IRQL.
pub(crate) fn note_fast_bind_busy() {
    FAST_BIND_BUSY.fetch_add(1, Ordering::Relaxed);
}

/// Note a fast bind that failed to enqueue or that the host refused. Any IRQL.
pub(crate) fn note_fast_bind_error() {
    FAST_BIND_ERRORS.fetch_add(1, Ordering::Relaxed);
}

/// Note a flip arm that declined to fast-bind, by [`skip`] reason. Any IRQL.
pub(crate) fn note_fast_bind_skip(reason: u32) {
    FAST_BIND_SKIPS.fetch_add(1, Ordering::Relaxed);
    FAST_BIND_SKIP_HISTOGRAM.note(reason);
}

/// Record an older direct worker request that must not rebind or publish a
/// primary. Any IRQL.
pub(crate) fn note_worker_epoch_superseded() {
    WORKER_EPOCH_SUPERSEDED.fetch_add(1, Ordering::Relaxed);
}

/// Record a same-epoch conflicting direct worker request. Any IRQL.
pub(crate) fn note_worker_epoch_conflict() {
    WORKER_EPOCH_CONFLICT.fetch_add(1, Ordering::Relaxed);
}

/// Ticks [`dump_periodic`].
static DUMP_TICKS: AtomicU32 = AtomicU32::new(0);

/// Dump cadence, in HPD-worker wakeups.
///
/// The worker wakes once per dirty edge plus once per control completion, so
/// during a workload this is roughly 128 frames — about one dump per second at
/// the 155 flush/s the handoff measured, and no dumps at all while idle (where
/// there is nothing to observe). One dump is ~120 registry writes, so the
/// cadence is the whole cost control: it must stay well above the per-frame
/// rate this driver spent T1b removing.
const DUMP_EVERY: u32 = 128;

/// Note one `dxgkddi_present` entry and which arms its flags select. Any IRQL.
pub(crate) fn note_present(flags: u32, flip_interval: u32, has_dma: bool, blt: bool, flip: bool) {
    PRESENT_CALLS.fetch_add(1, Ordering::Relaxed);
    PRESENT_FLAGS_HISTOGRAM.note(flags);
    PRESENT_INTERVAL_HISTOGRAM.note((flip_interval << 8) | u32::from(has_dma));
    if blt {
        PRESENT_BLTS.fetch_add(1, Ordering::Relaxed);
    }
    if flip {
        PRESENT_FLIPS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Note a flip that took the no-DMA-buffer `FlipOnVSyncMmIo` arm. Any IRQL.
pub(crate) fn note_present_mmio_flip() {
    PRESENT_MMIO_FLIPS.fetch_add(1, Ordering::Relaxed);
}

/// Note that a present marker named the currently BOUND scan-out resource.
/// Any IRQL — see [`MARKER_WHILE_BOUND`].
pub(crate) fn note_marker_while_bound() {
    MARKER_WHILE_BOUND.fetch_add(1, Ordering::Relaxed);
}

/// Note a flip that arrived on the DMA-buffer contract. Any IRQL.
pub(crate) fn note_present_dma_flip(resource_id: u32, _segment_id: u32) {
    PRESENT_DMA_FLIPS.fetch_add(1, Ordering::Relaxed);
    DMA_FLIP_HISTOGRAM.note(resource_id);
}

/// Note one bind-edge refresh and whether its Venus work had already retired.
/// Any IRQL — see [`BIND_REFRESH_DEFERRED`].
pub(crate) fn note_bind_refresh(ready: bool) {
    if ready {
        BIND_REFRESH_READY.fetch_add(1, Ordering::Relaxed);
    } else {
        BIND_REFRESH_DEFERRED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Note what a DEFERRED bind-edge arm is waiting on. Any IRQL.
pub(crate) fn note_bind_wait(count: u32, ring: u8) {
    BIND_WAIT_MAX.fetch_max(count, Ordering::Relaxed);
    BIND_WAIT_LAST.store(count, Ordering::Relaxed);
    BIND_WAIT_RING.store(ring as u32, Ordering::Relaxed);
}

/// Note a bind-edge refresh armed against the flip's carried boundary — the
/// 0ab-B ordering. Any IRQL.
pub(crate) fn note_bind_watermark_carried() {
    BIND_REFRESH_CARRIED.fetch_add(1, Ordering::Relaxed);
}

/// Note a bind that used the boundary its FLIP stamped on the allocation, rather
/// than the shared mark table. Any IRQL — see [`BIND_REFRESH_ALLOC_CARRIED`].
pub(crate) fn note_bind_watermark_allocation() {
    BIND_REFRESH_ALLOC_CARRIED.fetch_add(1, Ordering::Relaxed);
}

/// Note a bind whose table entry disagreed with the flip's carried boundary —
/// the overwrite census. Any IRQL — see [`BIND_REFRESH_OVERWRITTEN`].
pub(crate) fn note_bind_watermark_overwritten() {
    BIND_REFRESH_OVERWRITTEN.fetch_add(1, Ordering::Relaxed);
}

/// Note a bind-edge refresh that had no carried boundary and sampled one.
/// Any IRQL.
pub(crate) fn note_bind_watermark_sampled() {
    BIND_REFRESH_SAMPLED.fetch_add(1, Ordering::Relaxed);
}

/// Note that the submit path read a DMA flip's record and armed the
/// programming for it. Any IRQL.
pub(crate) fn note_dma_flip_armed() {
    PRESENT_DMA_FLIPS_ARMED.fetch_add(1, Ordering::Relaxed);
}

/// Note a `dxgkddi_set_vidpn_source_address` entry before anything else can
/// refuse it. Any IRQL.
pub(crate) fn note_ddi_entry() {
    DDI_ENTRIES.fetch_add(1, Ordering::Relaxed);
}

/// Note the NULL-argument early return. Any IRQL.
pub(crate) fn note_ddi_null_arg() {
    DDI_NULL_ARG.fetch_add(1, Ordering::Relaxed);
}

/// Note that the handle could not be paired with its address. Any IRQL.
pub(crate) fn note_ddi_pair_failed() {
    DDI_PAIR_FAILED.fetch_add(1, Ordering::Relaxed);
}

/// Note which half of the IRQL split this entry took. Any IRQL.
pub(crate) fn note_ddi_split(deferred: bool) {
    if deferred {
        DDI_DEFERRED.fetch_add(1, Ordering::Relaxed);
    } else {
        DDI_IMMEDIATE.fetch_add(1, Ordering::Relaxed);
    }
}

/// Note that a deferred store replaced `previous`, a different handle the
/// worker had not consumed yet. Any IRQL.
pub(crate) fn note_ddi_coalesced(previous: usize) {
    DDI_COALESCED.fetch_add(1, Ordering::Relaxed);
    DDI_COALESCED_LOST.store(previous as u32, Ordering::Relaxed);
}

/// Record one completed `program_vidpn_source` call. Any IRQL — atomics only.
///
/// `reject` is 0 for a call that reached an outcome; otherwise
/// [`crate::ddi::display::ScanoutReject::trace_code`].
pub(crate) fn note_program(
    h_alloc: usize,
    source_resource: u32,
    target_resource: u32,
    flags: u32,
    reject: u32,
) {
    let seq = PROGRAM_CALLS
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    if flags & flags::BOUND != 0 {
        PROGRAM_BINDS.fetch_add(1, Ordering::Relaxed);
    }
    if flags & flags::ALREADY_BOUND != 0 {
        PROGRAM_SKIPS.fetch_add(1, Ordering::Relaxed);
    }
    SOURCE_HISTOGRAM.note(source_resource);

    let slot = &RING[RING_CURSOR.fetch_add(1, Ordering::Relaxed) % RING_LEN];
    // Payload first, `packed` last: `packed` is nonzero for every recorded call
    // (the sequence number starts at 1), so a reader that sees it also sees the
    // three payload words for the same call.
    slot.source_resource
        .store(source_resource, Ordering::Relaxed);
    slot.target_resource
        .store(target_resource, Ordering::Relaxed);
    slot.alloc.store(h_alloc as u32, Ordering::Relaxed);
    slot.packed.store(
        (seq << 16) | ((reject & 0xFF) << 8) | (flags & 0xFF),
        Ordering::Release,
    );
}

/// Mirror the whole trace into the service key. PASSIVE_LEVEL only.
///
/// Value names: `Vp<hex><field>` for the ring (`A` packed, `B` source resource,
/// `C` target resource, `D` allocation), `VsR<n>`/`VsC<n>` for the source
/// histogram, `FfR<n>`/`FfC<n>` for the flush histogram, and the `Vp*` scalars
/// below.
pub(crate) fn dump(adapter: &crate::adapter::AdapterContext) {
    // The live display-engine state, sampled at the same instant as the
    // counters. A raised gate now means a newer primary is still owned by the
    // deferred programming path; `VpVsN` must continue advancing while VSync
    // truthfully reports the last published address. A frozen `VpVsN` is a
    // separate interrupt-heartbeat failure, not an intended gate effect.
    crate::diag::record_named_bytes(
        b"VpGate",
        adapter.vidpn_programming.load(Ordering::Acquire) as u32,
    );
    crate::diag::record_named_bytes(b"VpVsN", adapter.vsync_count.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"VpVsEn", adapter.vsync_enabled.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(
        b"VpPend",
        adapter.pending_vidpn_allocation.load(Ordering::Acquire) as u32,
    );
    crate::diag::record_named_bytes(
        b"VpLpa",
        adapter.last_primary_address.load(Ordering::Relaxed) as u32,
    );
    crate::diag::record_named_bytes(b"VpPres", PRESENT_CALLS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"VpBlt", PRESENT_BLTS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"VpFlip", PRESENT_FLIPS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"VpMmio", PRESENT_MMIO_FLIPS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"MkBound", MARKER_WHILE_BOUND.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"VpDmaF", PRESENT_DMA_FLIPS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"VpDmaA", PRESENT_DMA_FLIPS_ARMED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"VpEnt", DDI_ENTRIES.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"VpDef", DDI_DEFERRED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"VpImm", DDI_IMMEDIATE.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"VpNul", DDI_NULL_ARG.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"VpPrF", DDI_PAIR_FAILED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"VpCoal", DDI_COALESCED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"VpCoalH", DDI_COALESCED_LOST.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"VpPrgN", PROGRAM_CALLS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"VpBind", PROGRAM_BINDS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"VpSkip", PROGRAM_SKIPS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"BeRdy", BIND_REFRESH_READY.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"BeDef", BIND_REFRESH_DEFERRED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"BeCar", BIND_REFRESH_CARRIED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"BeSmp", BIND_REFRESH_SAMPLED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(
        b"BeAcar",
        BIND_REFRESH_ALLOC_CARRIED.load(Ordering::Relaxed),
    );
    crate::diag::record_named_bytes(b"BeOvw", BIND_REFRESH_OVERWRITTEN.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"OgIdn", OWNERSHIP_DROP_IDENTITY.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"OgEpo", OWNERSHIP_DROP_EPOCH.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"SnSub", SNAPSHOT_SUBSTITUTED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"SnFbk", SNAPSHOT_FALLBACK.load(Ordering::Relaxed));
    // The fast bind. `FpBind` against `VpDmaF` is "did the accelerator run?";
    // `FpApply` against `FpBind` is "did its bookkeeping land?"; `FpLate` is the
    // wire-order guard's whole census, at both application sites.
    crate::diag::record_named_bytes(b"FpBind", FAST_BIND_ENQUEUED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FpApply", FAST_BIND_APPLIED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FpLate", FAST_BIND_LATE.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FpCoal", FAST_BIND_COALESCED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FpSup", FAST_BIND_SUPERSEDED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FpBusy", FAST_BIND_BUSY.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FpErr", FAST_BIND_ERRORS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FpSkip", FAST_BIND_SKIPS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"WpSup", WORKER_EPOCH_SUPERSEDED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"WpCfl", WORKER_EPOCH_CONFLICT.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(
        b"FpSeq",
        adapter.scanout_bind_wire_seq.load(Ordering::Acquire) as u32,
    );
    crate::diag::record_named_bytes(
        b"FpAppSq",
        adapter.scanout_bind_applied_seq.load(Ordering::Acquire) as u32,
    );
    crate::diag::record_named_bytes(b"BeWMax", BIND_WAIT_MAX.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"BeWLst", BIND_WAIT_LAST.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"BeWRng", BIND_WAIT_RING.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"VpCur", RING_CURSOR.load(Ordering::Relaxed) as u32);

    // The presentation-epoch machine. `LsMint` is the presentation rate,
    // `LsQued`/`LsRead` the read rate, and `LsEndR` against `LsSupe`/`LsCanc`
    // says whether epochs are ending because the host READ the buffer or because
    // something gave up on it. (The withholding's own census — `LsBlk`/`LsRel`,
    // `LsPump`/`LsWait`, `LsPubG` — went with the mechanism in 22.22.217.0; see
    // `RETIRED_VALUE_NAMES`.)
    crate::diag::record_named_bytes(b"LsMint", LEASE_MINTED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"LsQued", LEASE_READ_QUEUED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"LsRead", LEASE_READ_DONE.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"LsEndR", LEASE_END_READ.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"LsSupe", LEASE_END_SUPERSEDED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"LsCanc", LEASE_END_CANCELLED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"LsTear", LEASE_END_TEARDOWN.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"LsStal", LEASE_STALE.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"LsPub", LEASE_PRIMARY_PUBLISHED.load(Ordering::Relaxed));
    // The live watermarks, sampled at the same instant as the tallies: a stuck
    // lease reads directly as `LsEpoc` far above `LsRdEp` and does not have to
    // be inferred from deltas.
    crate::diag::record_named_bytes(
        b"LsEpoc",
        adapter.scanout_present_epoch.load(Ordering::Acquire) as u32,
    );
    crate::diag::record_named_bytes(
        b"LsRdEp",
        adapter.scanout_read_epoch.load(Ordering::Acquire) as u32,
    );
    crate::diag::record_named_bytes(
        b"LsBnEp",
        adapter.scanout_bound_epoch.load(Ordering::Acquire) as u32,
    );
    // Whether the CURRENT binding is epoch-tracked, i.e. whether the ownership
    // gate's epoch arm is armed at all. A desktop-only session reads 0 here and
    // must: `OgEpo` climbing with no epoch-tracked binding would be a stale
    // flag, which is the one way that gate could starve DWM's freshness.
    crate::diag::record_named_bytes(
        b"LsTrk",
        adapter.scanout_epoch_tracked.load(Ordering::Acquire),
    );

    let mut i = 0;
    while i < RING_LEN {
        let slot = &RING[i];
        let hex = hex_digit(i as u32);
        // Acquire on `packed` pairs with the Release store in `note_program`.
        let packed = slot.packed.load(Ordering::Acquire);
        crate::diag::record_named_bytes(&[b'V', b'p', hex, b'A'], packed);
        crate::diag::record_named_bytes(
            &[b'V', b'p', hex, b'B'],
            slot.source_resource.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            &[b'V', b'p', hex, b'C'],
            slot.target_resource.load(Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            &[b'V', b'p', hex, b'D'],
            slot.alloc.load(Ordering::Relaxed),
        );
        i += 1;
    }

    SOURCE_HISTOGRAM.dump([b'V', b's']);
    FLUSH_HISTOGRAM.dump([b'F', b'f']);
    FAST_BIND_SKIP_HISTOGRAM.dump([b'F', b's']);
    MARKER_HISTOGRAM.dump([b'M', b'k']);
    DMA_FLIP_HISTOGRAM.dump([b'D', b'f']);
    PRESENT_FLIP_HISTOGRAM.dump([b'P', b'b']);
    PRESENT_FLAGS_HISTOGRAM.dump([b'F', b'l']);
    PRESENT_INTERVAL_HISTOGRAM.dump([b'F', b'i']);

    // The D4a read-ledger census (`Rd*`/`Aq*` — FIX-DESIGN-d4a.md §3.4).
    // Identity for every run: `RdIss == RdRet` at quiescence, `RdIss <= FfTot`
    // (every ledger issue is a flush the histogram above counted).
    crate::adapter::read_ledger_dump_counters();
}

/// Throttled [`dump`] for the HPD worker loop. PASSIVE_LEVEL only.
pub(crate) fn dump_periodic(adapter: &crate::adapter::AdapterContext) {
    let n = DUMP_TICKS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    if n == 1 || n % DUMP_EVERY == 0 {
        dump(adapter);
    }
}

/// Zero the whole trace and publish the zeros. StartDevice only.
///
/// Registry values persist across boots, so without this a `VpBind` from an
/// earlier boot is indistinguishable from one written during the run being
/// measured — the exact confusion this module exists to remove.
pub(crate) fn reset(adapter: &crate::adapter::AdapterContext) {
    for counter in [
        &PRESENT_CALLS,
        &PRESENT_BLTS,
        &PRESENT_FLIPS,
        &PRESENT_MMIO_FLIPS,
        &MARKER_WHILE_BOUND,
        &PRESENT_DMA_FLIPS,
        &PRESENT_DMA_FLIPS_ARMED,
        &DDI_ENTRIES,
        &DDI_DEFERRED,
        &DDI_IMMEDIATE,
        &DDI_NULL_ARG,
        &DDI_PAIR_FAILED,
        &DDI_COALESCED,
        &DDI_COALESCED_LOST,
        &PROGRAM_CALLS,
        &PROGRAM_BINDS,
        &PROGRAM_SKIPS,
        &LEASE_MINTED,
        &LEASE_READ_QUEUED,
        &LEASE_READ_DONE,
        &LEASE_END_READ,
        &LEASE_END_SUPERSEDED,
        &LEASE_END_CANCELLED,
        &LEASE_END_TEARDOWN,
        &LEASE_STALE,
        &LEASE_PRIMARY_PUBLISHED,
        &BIND_REFRESH_READY,
        &BIND_REFRESH_DEFERRED,
        &BIND_REFRESH_CARRIED,
        &BIND_REFRESH_SAMPLED,
        &BIND_REFRESH_ALLOC_CARRIED,
        &BIND_REFRESH_OVERWRITTEN,
        &BIND_WAIT_MAX,
        &BIND_WAIT_LAST,
        &BIND_WAIT_RING,
        &OWNERSHIP_DROP_IDENTITY,
        &OWNERSHIP_DROP_EPOCH,
        &SNAPSHOT_SUBSTITUTED,
        &SNAPSHOT_FALLBACK,
        &FAST_BIND_ENQUEUED,
        &FAST_BIND_APPLIED,
        &FAST_BIND_LATE,
        &FAST_BIND_COALESCED,
        &FAST_BIND_SUPERSEDED,
        &FAST_BIND_BUSY,
        &FAST_BIND_ERRORS,
        &FAST_BIND_SKIPS,
        &WORKER_EPOCH_SUPERSEDED,
        &WORKER_EPOCH_CONFLICT,
        &DUMP_TICKS,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
    for name in RETIRED_VALUE_NAMES {
        crate::diag::record_named_bytes(name, 0);
    }
    RING_CURSOR.store(0, Ordering::Relaxed);
    let mut i = 0;
    while i < RING_LEN {
        RING[i].clear();
        i += 1;
    }
    SOURCE_HISTOGRAM.reset();
    FLUSH_HISTOGRAM.reset();
    FAST_BIND_SKIP_HISTOGRAM.reset();
    MARKER_HISTOGRAM.reset();
    DMA_FLIP_HISTOGRAM.reset();
    PRESENT_FLIP_HISTOGRAM.reset();
    PRESENT_FLAGS_HISTOGRAM.reset();
    PRESENT_INTERVAL_HISTOGRAM.reset();
    // The read-ledger counters are per-boot like everything above; the ledger
    // PAGE's zeroing is separate (`ReadLedger::reset`, per transport
    // generation) — decoupled so a token orphaned across a PnP stop still
    // balances `RdRet` against its pre-stop `RdIss`.
    crate::adapter::read_ledger_reset_counters();
    dump(adapter);
}

/// Lowercase hex digit for a ring index. `RING_LEN` is 16, so the caller's
/// index is always < 16; anything else would be a bug in this file, and `'?'`
/// makes that visible in the registry instead of aliasing a real slot.
const fn hex_digit(n: u32) -> u8 {
    match n {
        0..=9 => b'0' + n as u8,
        10..=15 => b'a' + (n - 10) as u8,
        _ => b'?',
    }
}
