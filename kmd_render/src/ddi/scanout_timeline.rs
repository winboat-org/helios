//! Fixed, always-on scanout ordering timeline.
//!
//! The writer is callable at DIRQL/DISPATCH/PASSIVE: it reserves one fixed
//! slot, records scalar values, and release-commits the sequence last. No hot
//! path allocates, waits, takes a lock, or writes diagnostics to the registry.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use wdk_sys::ntddk::KeQueryInterruptTimePrecise;

/// A power of two makes indexing one mask operation at every IRQL. The ring is
/// 2 MiB (`32768 * 64`) in static nonpaged driver image storage. The correlated
/// capture records every boundary plus explicit retry/drop outcomes, so this
/// retains a complete Combined segment without relying on aggregate counters.
pub const SLOT_COUNT: usize = 32_768;
const SLOT_MASK: u64 = SLOT_COUNT as u64 - 1;
pub const TIMESTAMP_UNIT_100NS: u32 = 1;

/// Event kinds. The ABI is intentionally numeric so the C collector can stay
/// independent of the Rust enum.
pub mod kind {
    pub const PRODUCER_MARKER: u32 = 1;
    pub const SNAPSHOT_SUBMIT: u32 = 2;
    pub const DMA_FLIP_ARM: u32 = 3;
    pub const WORKER_STAGE: u32 = 4;
    pub const FAST_SET_PUBLISH: u32 = 5;
    pub const FAST_SET_COMPLETE: u32 = 6;
    pub const BIND_APPLY: u32 = 7;
    pub const BIND_REFRESH_ARM: u32 = 8;
    pub const REFRESH_PROMOTE: u32 = 9;
    pub const FLUSH_PUBLISH: u32 = 10;
    pub const FLUSH_COMPLETE: u32 = 11;
    pub const VBLANK_EPOCH: u32 = 12;
    pub const SYNC_SET_PUBLISH: u32 = 13;
    pub const SYNC_SET_RETURN: u32 = 14;
    pub const DEFERRED_REPLACED: u32 = 15;
    pub const REFRESH_PENDING: u32 = 16;
    pub const REFRESH_DROPPED: u32 = 17;
    /// Every successfully delivered CRTC_VSYNC callback. Unlike
    /// `VBLANK_EPOCH`, this is an actual heartbeat sample rather than a
    /// transition-only registry mirror.
    pub const VBLANK_TICK: u32 = 18;
    /// A deferred fast presentation was at/below the accepted descriptor epoch
    /// floor and was discarded without a SET publication.
    pub const DEFERRED_SUPERSEDED: u32 = 19;
    pub const WINDOWED_BLT_ARM: u32 = 20;
    pub const WINDOWED_BLT_ADMIT: u32 = 21;
    pub const WINDOWED_BLT_SUBMIT: u32 = 22;
    pub const WINDOWED_BLT_RING_COMPLETE: u32 = 23;
    pub const WINDOWED_BLT_MIRROR: u32 = 24;
    pub const WINDOWED_BLT_TERMINAL: u32 = 25;
    /// Render accepted and stashed the typed WindowedBlt snapshot before the
    /// following Present converts its registered producer marker to a fence.
    pub const WINDOWED_BLT_STASH: u32 = 26;
    /// `DxgkDdiPresent` returned. `identity` is total DDI residence in 100 ns
    /// units, `aux` is the exact DXGK present flags, and SUCCESS names the
    /// returned status. This is attribution only; it never gates presentation.
    pub const PRESENT_RETURN: u32 = 27;
}

/// Shared flags; event-specific outcomes occupy the low bits.
pub mod flag {
    pub const SNAPSHOT: u32 = 1 << 16;
    pub const ALREADY_BOUND: u32 = 1 << 17;
    pub const READY: u32 = 1 << 18;
    pub const WAITING: u32 = 1 << 19;
    pub const ABANDONED: u32 = 1 << 20;
    pub const SUCCESS: u32 = 1 << 21;
    pub const REPLACED: u32 = 1 << 22;
    pub const FAST_FRONTIER: u32 = 1 << 23;
    pub const PENDING_SLOT: u32 = 1 << 24;
    pub const DROPPED: u32 = 1 << 25;
    pub const SUPERSEDED: u32 = 1 << 26;
    pub const EPOCH_CONFLICT: u32 = 1 << 27;
}

/// `aux` values for refresh queue outcomes. `identity` is the exact armed
/// resource (or zero for an identity-free edge); `resource_id` is the active
/// scanout resource sampled by the queue.
pub mod refresh_outcome {
    pub const ACTIVE_CHANGED: u32 = 1;
    pub const FLUSH_INFLIGHT: u32 = 2;
    pub const FLUSH_GATE_RACE: u32 = 3;
    pub const ENQUEUE_FAILED: u32 = 4;
    pub const UNAVAILABLE: u32 = 5;
    pub const RESOURCE_DEAD: u32 = 6;
    pub const HOST_UNBOUND: u32 = 7;
    pub const OWNERSHIP_IDENTITY: u32 = 8;
    pub const OWNERSHIP_EPOCH: u32 = 9;
}

/// `aux` values for [`kind::VBLANK_TICK`]. This is deliberately an always-on
/// scalar in the existing trace ABI: a capture can prove whether Ex's high
/// resolution allocation succeeded without per-frame registry writes.
pub mod vblank_source {
    /// `ExAllocateTimer(EX_TIMER_HIGH_RESOLUTION)` succeeded for this adapter.
    pub const EX_HIGH_RESOLUTION: u32 = 1;
    /// `ExAllocateTimer` failed, so the embedded `KTIMER` fallback delivered
    /// this tick instead.
    pub const KTIMER_FALLBACK: u32 = 0;
}

/// One stable reader-visible event. `identity` is a full Windows allocation
/// handle/address at flip time, a bind wire sequence for SET events, a unique
/// flush id, or the exact armed resource for a refresh outcome. `aux` carries
/// the second resource/status/outcome code.
#[derive(Clone, Copy)]
pub struct Event {
    pub sequence: u64,
    pub timestamp_100ns: u64,
    pub present_epoch: u64,
    pub carried_watermark: u64,
    pub identity: u64,
    pub resource_id: u32,
    pub aux: u32,
    pub kind: u32,
    pub flags: u32,
}

#[repr(C, align(64))]
struct Slot {
    // Commit is the publication word and is written last with Release.
    commit: AtomicU64,
    timestamp_100ns: AtomicU64,
    present_epoch: AtomicU64,
    carried_watermark: AtomicU64,
    identity: AtomicU64,
    resource_id: AtomicU32,
    aux: AtomicU32,
    kind: AtomicU32,
    flags: AtomicU32,
}

impl Slot {
    const fn new() -> Self {
        Self {
            commit: AtomicU64::new(0),
            timestamp_100ns: AtomicU64::new(0),
            present_epoch: AtomicU64::new(0),
            carried_watermark: AtomicU64::new(0),
            identity: AtomicU64::new(0),
            resource_id: AtomicU32::new(0),
            aux: AtomicU32::new(0),
            kind: AtomicU32::new(0),
            flags: AtomicU32::new(0),
        }
    }
}

const _: () = assert!(core::mem::size_of::<Slot>() == 64);

static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static RING: [Slot; SLOT_COUNT] = [const { Slot::new() }; SLOT_COUNT];

/// Record one causal boundary. Safe at every IRQL documented for
/// `KeQueryInterruptTimePrecise`; the call is a scalar time read, not a wait.
#[inline]
pub fn note(
    kind: u32,
    flags: u32,
    present_epoch: u64,
    carried_watermark: u64,
    identity: u64,
    resource_id: u32,
    aux: u32,
) {
    let sequence = NEXT_SEQUENCE
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    let slot = &RING[(sequence & SLOT_MASK) as usize];
    // Invalidate a previous wrapped record before changing any payload. A
    // reader that began on the old sequence must fail its final validation,
    // never accept a mixture of old commit and new fields.
    slot.commit.store(0, Ordering::Release);
    // SAFETY: documented by WDK as callable at any IRQL. It returns 100 ns
    // interrupt-time units and fills the valid QPC output pointer without a
    // wait or allocation. The ring reports the returned 100 ns time unit.
    let mut qpc_timestamp = 0u64;
    let timestamp_100ns = unsafe { KeQueryInterruptTimePrecise(&mut qpc_timestamp) };
    slot.timestamp_100ns
        .store(timestamp_100ns, Ordering::Relaxed);
    slot.present_epoch.store(present_epoch, Ordering::Relaxed);
    slot.carried_watermark
        .store(carried_watermark, Ordering::Relaxed);
    slot.identity.store(identity, Ordering::Relaxed);
    slot.resource_id.store(resource_id, Ordering::Relaxed);
    slot.aux.store(aux, Ordering::Relaxed);
    slot.kind.store(kind, Ordering::Relaxed);
    slot.flags.store(flags, Ordering::Relaxed);
    slot.commit.store(sequence, Ordering::Release);
}

#[inline]
pub fn cursor() -> u64 {
    NEXT_SEQUENCE.load(Ordering::Acquire)
}

#[inline]
pub const fn capacity() -> u32 {
    SLOT_COUNT as u32
}

/// Copy exactly `sequence` if it remains retained and was not concurrently
/// overwritten. The caller can count a `None` as one loss.
pub fn read(sequence: u64) -> Option<Event> {
    let cursor = cursor();
    if sequence == 0 || sequence > cursor || cursor.saturating_sub(sequence) >= SLOT_COUNT as u64 {
        return None;
    }
    let slot = &RING[(sequence & SLOT_MASK) as usize];
    if slot.commit.load(Ordering::Acquire) != sequence {
        return None;
    }
    let event = Event {
        sequence,
        timestamp_100ns: slot.timestamp_100ns.load(Ordering::Relaxed),
        present_epoch: slot.present_epoch.load(Ordering::Relaxed),
        carried_watermark: slot.carried_watermark.load(Ordering::Relaxed),
        identity: slot.identity.load(Ordering::Relaxed),
        resource_id: slot.resource_id.load(Ordering::Relaxed),
        aux: slot.aux.load(Ordering::Relaxed),
        kind: slot.kind.load(Ordering::Relaxed),
        flags: slot.flags.load(Ordering::Relaxed),
    };
    // A writer may have replaced the slot while the payload was copied.
    (slot.commit.load(Ordering::Acquire) == sequence).then_some(event)
}
