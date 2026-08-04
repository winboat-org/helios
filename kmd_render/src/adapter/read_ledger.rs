//! D4a scanout-read acquire (FIX-DESIGN-d4a.md §3): the per-resid READ LEDGER
//! page, its issue/retire bumps, and the persistent retirement-event broadcast.
//!
//! One 4 KiB nonpaged page, adapter-owned, mapped READ-ONLY into user mode by
//! `HELIOS_ESCAPE_MAP_READ_LEDGER`. Its head is `helios_protocol::
//! HeliosReadLedgerPage`: 65 generation-qualified slots of
//! `{resid, generation, issued, retired}`. A host read is in flight iff the
//! stable claim sample has `issued > retired`.
//! The UMD arms a GPU-side wait on X's reuse from exactly that predicate, so
//! every write here is a wire-visible publication: a fresh claim publishes its
//! nonzero generation and counters before publishing its `resid`.
//!
//! The state machine — claim, pin-until-quiescent, wanted-token reclaim
//! arbitration — is `helios_kmd_logic::scanout_read_ledger` (host-tested);
//! this module is its atomic transcription. Serialization facts it relies on:
//!
//!   * Every page mutation is serialized by `mutation`, a dedicated leaf
//!     spinlock. It is taken after any currently-held adapter lock, and no
//!     allocation, wait, pageable operation, registry logging, event-table
//!     lock, or other lock is legal while it is held.
//!   * `retire` is driven exclusively by [`crate::virtio::ScanoutFlushToken`]
//!     — completion or `Drop` — at PASSIVE or at DISPATCH under `virtio_lock`.
//!     It releases `mutation` before broadcasting the retirement event.
//!
//! The page is allocated ONCE (first StartDevice) and freed only in
//! `AdapterContext::drop`: user mappings created through the escape can
//! outlive StopDevice, and dxgkrnl destroys every device (which drains the
//! owner-keyed `MappingTable`) before RemoveDevice.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use helios_kmd_logic::scanout_read_ledger::{LedgerTicket, FREE_RESID, NO_SLOT, SLOT_COUNT};
use helios_protocol::{
    HeliosReadLedgerPage, HeliosReadLedgerSlot, HELIOS_READ_LEDGER_MAGIC, HELIOS_READ_LEDGER_SLOTS,
    HELIOS_READ_LEDGER_VERSION,
};
use wdk_sys::ntddk::{KeSetEvent, MmAllocateContiguousMemory, ObDereferenceObjectDeferDelete};
use wdk_sys::{KEVENT, PHYSICAL_ADDRESS, PVOID};

/// The mapping the escape hands out is the whole page, not just the struct at
/// its head — the remainder is zero and reserved.
pub(crate) const LEDGER_PAGE_BYTES: usize = 4096;

/// Retirement-event registrations per adapter. Sized like the fence-event
/// table's consumers: one auto-reset event per DxvkDevice, a handful of
/// processes (dwm + apps), with headroom for restart churn.
const EVENT_TABLE_LEN: usize = 16;

// ── Counters (§3.4) — unsampled atomics, published by `scanout_trace::dump`,
// zeroed by `scanout_trace::reset` at StartDevice. Deliberately NOT zeroed by
// `ReadLedger::reset`: the page is per-transport-generation state, these are
// per-boot, and an orphaned retirement balances `RdRet` against a pre-reset
// `RdIss` only because the two lifetimes are decoupled.

/// Ledger issues (`RdIss`). Identity: `RdIss == RdRet` at quiescence.
static RD_ISSUED: AtomicU32 = AtomicU32::new(0);
/// Ledger retirements, any outcome (`RdRet`).
static RD_RETIRED: AtomicU32 = AtomicU32::new(0);
/// Claims refused because all slots were live (`RdOvf`). The read still
/// happens — it just runs unledgered/ungated. Loud, never wedged.
static RD_OVERFLOW: AtomicU32 = AtomicU32::new(0);
/// Retirements performed by the token's `Drop` (`RdDrp`) — enqueue failure or
/// transport teardown. ~0 in health; nonzero is worth eyes, not a bugcheck.
static RD_DROP_RETIRED: AtomicU32 = AtomicU32::new(0);
/// Retirements whose slot no longer named the token's resid (`RdOrp`).
/// Reachable ONLY when Stop/StartDevice zeroed the ledger with the read in
/// flight; movement outside a PnP stop is a bug.
static RD_ORPHANED: AtomicU32 = AtomicU32::new(0);
/// Global claim-generation space was exhausted. This cannot silently alias:
/// the affected read is explicitly unledgered/refused by its caller.
static RD_GENERATION_EXHAUSTED: AtomicU32 = AtomicU32::new(0);
/// MAP_READ_LEDGER refusals: page missing, MDL/map failure, bad op (`RdMapF`).
pub(crate) static RD_MAP_REFUSED: AtomicU32 = AtomicU32::new(0);
/// Event registrations parked (`AqReg`).
static AQ_REGISTERED: AtomicU32 = AtomicU32::new(0);
/// Registration refusals: table full, bad handle, bad op (`AqRgF`).
pub(crate) static AQ_REGISTER_REFUSED: AtomicU32 = AtomicU32::new(0);
/// Broadcast `KeSetEvent` invocations at retirement (`AqSigB`).
static AQ_SIGNALS: AtomicU32 = AtomicU32::new(0);

/// Mirror the ledger counters into the service key. PASSIVE_LEVEL only —
/// called from `scanout_trace::dump`.
pub(crate) fn dump_counters() {
    crate::diag::record_named_bytes(b"RdIss", RD_ISSUED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"RdRet", RD_RETIRED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"RdOvf", RD_OVERFLOW.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"RdDrp", RD_DROP_RETIRED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"RdOrp", RD_ORPHANED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"RdGEx", RD_GENERATION_EXHAUSTED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"RdMapF", RD_MAP_REFUSED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"AqReg", AQ_REGISTERED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"AqRgF", AQ_REGISTER_REFUSED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"AqSigB", AQ_SIGNALS.load(Ordering::Relaxed));
}

/// Zero the ledger counters. StartDevice only (`scanout_trace::reset`) —
/// registry values persist across boots, so a counter must be seen to MOVE.
pub(crate) fn reset_counters() {
    for counter in [
        &RD_ISSUED,
        &RD_RETIRED,
        &RD_OVERFLOW,
        &RD_DROP_RETIRED,
        &RD_ORPHANED,
        &RD_GENERATION_EXHAUSTED,
        &RD_MAP_REFUSED,
        &AQ_REGISTERED,
        &AQ_REGISTER_REFUSED,
        &AQ_SIGNALS,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
}

/// The KMD's atomic view of one [`HeliosReadLedgerSlot`]. The `u64` fields are
/// explicitly aligned by the wire layout, and the const block below pins the
/// complete byte identity instead of asserting it in prose.
#[repr(C)]
pub(crate) struct LedgerSharedSlot {
    resid: AtomicU32,
    /// Layout-mirror padding before the naturally aligned shared `u64`s.
    #[allow(dead_code)]
    pad0: AtomicU32,
    generation: AtomicU64,
    issued: AtomicU64,
    retired: AtomicU64,
}

/// The KMD's atomic view of the mapped [`HeliosReadLedgerPage`]. The header
/// and reserved words are WRITE-only KMD-side (the mapped reader consumes
/// them), which is all `dead_code` sees.
#[repr(C)]
pub(crate) struct LedgerSharedPage {
    #[allow(dead_code)]
    magic: AtomicU32,
    #[allow(dead_code)]
    version: AtomicU32,
    #[allow(dead_code)]
    slot_count: AtomicU32,
    #[allow(dead_code)]
    reserved0: AtomicU32,
    slots: [LedgerSharedSlot; HELIOS_READ_LEDGER_SLOTS],
    #[allow(dead_code)]
    slot_overflow: AtomicU32,
    #[allow(dead_code)]
    reserved1: [AtomicU32; 3],
}

const _: () = {
    // The logic crate's slot count is a dependency-free duplicate; pin it.
    assert!(SLOT_COUNT == HELIOS_READ_LEDGER_SLOTS);
    // The atomic view must be byte-identical to the wire struct the UMD reads.
    assert!(
        core::mem::size_of::<LedgerSharedSlot>() == core::mem::size_of::<HeliosReadLedgerSlot>()
    );
    assert!(
        core::mem::size_of::<LedgerSharedPage>() == core::mem::size_of::<HeliosReadLedgerPage>()
    );
    assert!(
        core::mem::offset_of!(LedgerSharedPage, slots)
            == core::mem::offset_of!(HeliosReadLedgerPage, slots)
    );
    assert!(
        core::mem::offset_of!(LedgerSharedPage, slot_overflow)
            == core::mem::offset_of!(HeliosReadLedgerPage, slot_overflow)
    );
    assert!(core::mem::size_of::<LedgerSharedPage>() <= LEDGER_PAGE_BYTES);
    // The token stores the slot index in a u8; NO_SLOT must stay outside it.
    assert!(SLOT_COUNT < NO_SLOT as usize);
};

/// One parked retirement-event registration. Both fields are opaque `usize`
/// (same rationale as `mapping::Mapping`): the table then needs no `unsafe
/// impl Send`, and the KEVENT pointer is only ever rebuilt at a use site that
/// states why it is still alive (the table holds an object reference).
#[derive(Clone, Copy)]
struct EventEntry {
    /// The registering D3D device handle (`DeviceOwner::raw`), for the
    /// destroy-device reclaim the one-shot fence-event table lacks.
    owner: usize,
    /// `*mut KEVENT` from `ObReferenceObjectByHandle`; never 0 for a live
    /// entry. The entry OWNS one object reference: whoever removes the entry
    /// must dereference (DeferDelete — removal can run at DISPATCH).
    event: usize,
}

/// Outcome of [`ReadLedger::register_event`].
pub(crate) enum ScanoutEventReg {
    /// Parked; the table now owns the caller's object reference.
    Registered,
    /// This (owner, event) pair is already parked. Idempotent success: the
    /// caller must drop the reference IT took for this call; the parked one
    /// stays live.
    AlreadyRegistered,
    /// No free entry (counted `AqRgF`). The caller runs ungated — loud, never
    /// wedged.
    TableFull,
}

/// KMD-private state behind the read-ledger mutation leaf lock. `next_generation`
/// belongs to the adapter rather than a transport generation: reset clears the
/// mapped claims but never rewinds it.
struct LedgerPrivate {
    next_generation: u64,
    retire_wanted: [bool; SLOT_COUNT],
}

impl LedgerPrivate {
    const fn new() -> Self {
        Self {
            next_generation: 1,
            retire_wanted: [false; SLOT_COUNT],
        }
    }

    fn mint_generation(&mut self) -> Option<u64> {
        if self.next_generation == 0 {
            return None;
        }
        let generation = self.next_generation;
        // `0` is the exhausted sentinel. The final nonzero generation is still
        // usable; the following mint is the checked, loud refusal.
        self.next_generation = generation.checked_add(1).unwrap_or(0);
        Some(generation)
    }
}

/// The D4a read ledger + retirement-event table, owned by `AdapterContext`.
pub(crate) struct ReadLedger {
    /// Kernel VA of the one nonpaged ledger page; 0 until the first
    /// StartDevice allocates it. Published with Release AFTER the page is
    /// zeroed and its header written, so an Acquire reader of a nonzero value
    /// sees an initialized page. Never cleared while the adapter lives.
    page_va: AtomicUsize,
    /// ⚠ LEAF LOCK (`read_ledger_mutation_lock`): acquired after any other
    /// adapter lock and released before event broadcast. It serializes issue,
    /// allocation-retire, reset, and retirement so a quiescent slot can never
    /// be recycled underneath an old ticket. Nothing may be acquired while it
    /// is held; its state is values only and never allocates.
    mutation: crate::sync::SpinLock<LedgerPrivate>,
    /// ⚠ LEAF LOCK (`scanout_event_lock`): acquired after any other lock this
    /// driver holds — the broadcast runs at DISPATCH under `virtio_lock` from
    /// the used-ring drain — and NOTHING may be acquired while holding it.
    /// Held only for table ops plus the ≤16 `KeSetEvent(Wait = FALSE)` calls
    /// of a broadcast. See `adapter/locks.rs` for the documented order.
    events: crate::sync::SpinLock<[Option<EventEntry>; EVENT_TABLE_LEN]>,
}

impl ReadLedger {
    pub(crate) const fn new() -> Self {
        Self {
            page_va: AtomicUsize::new(0),
            mutation: crate::sync::SpinLock::new(LedgerPrivate::new()),
            events: crate::sync::SpinLock::new([None; EVENT_TABLE_LEN]),
        }
    }

    /// The live page, if StartDevice allocated one.
    fn page(&self) -> Option<&LedgerSharedPage> {
        let va = self.page_va.load(Ordering::Acquire);
        if va == 0 {
            return None;
        }
        // SAFETY: `va` was published by `init_page` after zeroing + header
        // init, is page-aligned (MmAllocateContiguousMemory), and is freed
        // only in `AdapterContext::drop` — after every DDI, DPC and token that
        // can reach this accessor is gone. All mutation goes through the
        // atomics of the returned view.
        Some(unsafe { &*(va as *const LedgerSharedPage) })
    }

    /// Kernel VA + size of the page for the MAP escape; `None` until allocated.
    pub(crate) fn page_for_mapping(&self) -> Option<(usize, usize)> {
        let va = self.page_va.load(Ordering::Acquire);
        if va == 0 {
            return None;
        }
        Some((va, LEDGER_PAGE_BYTES))
    }

    /// Allocate the page on first StartDevice; later starts keep the existing
    /// one (user mappings from a previous generation may still reference it).
    /// PASSIVE_LEVEL. `RdPgF` is (re)written on EVERY start — 0 healthy, 1
    /// failed — because registry values persist across boots and this value is
    /// deliberately not in `reset_counters` (which runs AFTER this and would
    /// erase a fresh failure). Allocation failure leaves the feature off:
    /// every consumer handles `page_va == 0`.
    pub(crate) fn init_page(&self) {
        if self.page_va.load(Ordering::Acquire) != 0 {
            crate::diag::record_named_bytes(b"RdPgF", 0);
            return;
        }
        // SAFETY: PHYSICAL_ADDRESS is a POD union; all-zero is a valid value
        // (the same construction `alloc_contiguous_ram` uses).
        let mut highest: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
        highest.QuadPart = i64::MAX;
        // SAFETY: PASSIVE_LEVEL (StartDevice). One page of physically
        // contiguous nonpaged RAM, page-aligned by the API's contract, or null.
        let va = unsafe { MmAllocateContiguousMemory(LEDGER_PAGE_BYTES as u64, highest) };
        let Some(va) = NonNull::new(va as *mut u8) else {
            crate::diag::record_named_bytes(b"RdPgF", 1);
            return;
        };
        // SAFETY: freshly allocated, unpublished; zero so no reader — kernel
        // or the user mapping — can see stale pool bytes.
        unsafe { core::ptr::write_bytes(va.as_ptr(), 0, LEDGER_PAGE_BYTES) };
        // SAFETY: unpublished page of the asserted-compatible layout; header
        // written before the Release publication of the VA below.
        let page = unsafe { &*(va.as_ptr() as *const LedgerSharedPage) };
        Self::write_header(page);
        self.page_va.store(va.as_ptr() as usize, Ordering::Release);
        crate::diag::record_named_bytes(b"RdPgF", 0);
    }

    /// Reclaim the page VA for freeing. `AdapterContext::drop` only — by then
    /// every device is destroyed, so no user mapping and no token remains.
    pub(crate) fn take_page_va(&self) -> usize {
        self.page_va.swap(0, Ordering::AcqRel)
    }

    fn write_header(page: &LedgerSharedPage) {
        page.version
            .store(HELIOS_READ_LEDGER_VERSION, Ordering::Release);
        page.slot_count
            .store(HELIOS_READ_LEDGER_SLOTS as u32, Ordering::Release);
        page.reserved0.store(0, Ordering::Release);
        // Magic LAST: a mapped reader treats a non-magic page as feature-off,
        // so it never consumes a half-written header.
        page.magic
            .store(HELIOS_READ_LEDGER_MAGIC, Ordering::Release);
    }

    /// Issue one host reader. Mutation is serialized at every IRQL by the
    /// dedicated leaf lock; callers may hold higher adapter locks, but this
    /// routine never allocates, waits, logs, or acquires another lock.
    pub(crate) fn issue(&self, resid: u32) -> LedgerTicket {
        if resid == FREE_RESID {
            return LedgerTicket::NONE;
        }
        let mut private = self.mutation.lock();
        let Some(page) = self.page() else {
            return LedgerTicket::NONE;
        };
        let mut i = 0;
        while i < SLOT_COUNT {
            let slot = &page.slots[i];
            if slot.resid.load(Ordering::Acquire) == resid {
                let generation = slot.generation.load(Ordering::Acquire);
                if generation == 0 {
                    // A mapped claim with a zero ticket is protocol corruption.
                    // Keep it loud and refuse rather than mint an alias.
                    RD_GENERATION_EXHAUSTED.fetch_add(1, Ordering::Relaxed);
                    return LedgerTicket::NONE;
                }
                slot.issued.fetch_add(1, Ordering::Release);
                RD_ISSUED.fetch_add(1, Ordering::Relaxed);
                return LedgerTicket {
                    slot: i as u8,
                    resid,
                    generation,
                };
            }
            i += 1;
        }
        // Required preference order: an unclaimed slot wins before a quiescent
        // live claim is recycled.
        let mut i = 0;
        while i < SLOT_COUNT {
            let slot = &page.slots[i];
            if slot.resid.load(Ordering::Acquire) == FREE_RESID {
                return Self::claim_fresh(&mut private, slot, i, resid);
            }
            i += 1;
        }
        // Allocation lifetime is deliberately irrelevant here. A slot is
        // recyclable when and only when no reader is active.
        let mut i = 0;
        while i < SLOT_COUNT {
            let slot = &page.slots[i];
            if slot.issued.load(Ordering::Acquire) == slot.retired.load(Ordering::Acquire) {
                return Self::claim_fresh(&mut private, slot, i, resid);
            }
            i += 1;
        }
        page.slot_overflow.fetch_add(1, Ordering::AcqRel);
        RD_OVERFLOW.fetch_add(1, Ordering::Relaxed);
        LedgerTicket::NONE
    }

    /// Retire one exact ticket. The mutation lock is released before event
    /// broadcast; the two leaf locks never nest.
    pub(crate) fn retire(&self, ticket: LedgerTicket, via_drop: bool) {
        if !ticket.is_claimed() {
            return;
        }
        let retired = {
            let mut private = self.mutation.lock();
            let Some(page) = self.page() else {
                RD_ORPHANED.fetch_add(1, Ordering::Relaxed);
                return;
            };
            let i = ticket.slot as usize;
            if i >= SLOT_COUNT {
                RD_ORPHANED.fetch_add(1, Ordering::Relaxed);
                return;
            }
            RD_RETIRED.fetch_add(1, Ordering::Relaxed);
            if via_drop {
                RD_DROP_RETIRED.fetch_add(1, Ordering::Relaxed);
            }
            let slot = &page.slots[i];
            if slot.resid.load(Ordering::Acquire) != ticket.resid
                || slot.generation.load(Ordering::Acquire) != ticket.generation
            {
                RD_ORPHANED.fetch_add(1, Ordering::Relaxed);
                false
            } else {
                slot.retired.fetch_add(1, Ordering::Release);
                let issued = slot.issued.load(Ordering::Acquire);
                let retired = slot.retired.load(Ordering::Acquire);
                if private.retire_wanted[i] && issued == retired {
                    private.retire_wanted[i] = false;
                    Self::reclaim_slot(slot);
                }
                true
            }
        };
        if retired {
            // Event signaling stays outside the mutation lock. The consumer is
            // level-triggered and will acquire-load the fully published claim.
            self.broadcast();
        }
    }

    /// Mark allocation teardown. An active reader pins the claim until its
    /// exact ticket retires; a quiescent claim is immediately released.
    pub(crate) fn note_alloc_retired(&self, resid: u32) {
        if resid == FREE_RESID {
            return;
        }
        let mut private = self.mutation.lock();
        let Some(page) = self.page() else {
            return;
        };
        let mut i = 0;
        while i < SLOT_COUNT {
            let s = &page.slots[i];
            if s.resid.load(Ordering::Acquire) != resid {
                i += 1;
                continue;
            }
            private.retire_wanted[i] = true;
            let issued = s.issued.load(Ordering::Acquire);
            let retired = s.retired.load(Ordering::Acquire);
            if issued == retired {
                private.retire_wanted[i] = false;
                Self::reclaim_slot(s);
            }
            return;
        }
    }

    /// Disassociate the ticket before clearing its fields. A reader that began
    /// on the old resid either observes the free marker or catches a changed
    /// generation when a same-resid replacement has already published.
    fn reclaim_slot(s: &LedgerSharedSlot) {
        s.resid.store(FREE_RESID, Ordering::Release);
        s.issued.store(0, Ordering::Release);
        s.retired.store(0, Ordering::Release);
        s.generation.store(0, Ordering::Release);
    }

    /// Publish a fresh claim only after its nonzero generation and initial
    /// counters exist. The final `resid` release is the mapped-reader commit.
    fn claim_fresh(
        private: &mut LedgerPrivate,
        slot: &LedgerSharedSlot,
        index: usize,
        resid: u32,
    ) -> LedgerTicket {
        let Some(generation) = private.mint_generation() else {
            RD_GENERATION_EXHAUSTED.fetch_add(1, Ordering::Relaxed);
            return LedgerTicket::NONE;
        };
        slot.resid.store(FREE_RESID, Ordering::Release);
        slot.generation.store(generation, Ordering::Release);
        slot.retired.store(0, Ordering::Release);
        slot.issued.store(1, Ordering::Release);
        slot.resid.store(resid, Ordering::Release);
        private.retire_wanted[index] = false;
        RD_ISSUED.fetch_add(1, Ordering::Relaxed);
        LedgerTicket {
            slot: index as u8,
            resid,
            generation,
        }
    }

    /// Drop every piece of per-transport-generation ledger state: signal +
    /// dereference all event registrations, zero the slots and the overflow
    /// word, rewrite the header. PASSIVE_LEVEL
    /// (`reset_display_publication_state`, Stop/StartDevice).
    ///
    /// A token still in flight when this runs is generation-orphaned. Reset
    /// never rewinds the private source, so it cannot touch a new same-resid
    /// claim after the next StartDevice.
    pub(crate) fn reset(&self) {
        // Take the registrations out under the leaf lock, signal + deref
        // OUTSIDE it: the signal wakes level-triggered consumers into a
        // re-read, and the deref must not run under a spinlock we hold.
        let drained = self.drain_events(None);
        for entry in drained.iter().flatten() {
            // SAFETY: the entry owned one object reference; the KEVENT is
            // therefore alive. Wait = FALSE; DeferDelete because dropping the
            // last reference with a plain deref above PASSIVE would run the
            // object's PASSIVE-only deletion, and this path's IRQL contract
            // should not silently depend on its two callers.
            unsafe {
                KeSetEvent(entry.event as *mut KEVENT, 0, 0);
                ObDereferenceObjectDeferDelete(entry.event as PVOID);
            }
        }
        let mut private = self.mutation.lock();
        let Some(page) = self.page() else {
            return;
        };
        let mut i = 0;
        while i < SLOT_COUNT {
            private.retire_wanted[i] = false;
            Self::reclaim_slot(&page.slots[i]);
            i += 1;
        }
        page.slot_overflow.store(0, Ordering::Release);
        // The header survives the zeroing so mapped readers keep the feature
        // on across a PnP stop/start. `next_generation` deliberately remains
        // preserved in `LedgerPrivate`; reset never aliases an old ticket.
        Self::write_header(page);
    }

    /// Park one referenced usermode event for persistent retirement signaling.
    /// The table takes ownership of the caller's object reference on
    /// [`ScanoutEventReg::Registered`] — every other outcome leaves it with
    /// the caller. PASSIVE (escape).
    pub(crate) fn register_event(&self, owner: usize, event: NonNull<KEVENT>) -> ScanoutEventReg {
        let raw = event.as_ptr() as usize;
        let mut table = self.events.lock();
        let mut free = None;
        let mut i = 0;
        while i < EVENT_TABLE_LEN {
            match table[i] {
                Some(e) if e.owner == owner && e.event == raw => {
                    return ScanoutEventReg::AlreadyRegistered;
                }
                None if free.is_none() => free = Some(i),
                _ => {}
            }
            i += 1;
        }
        match free {
            Some(i) => {
                table[i] = Some(EventEntry { owner, event: raw });
                AQ_REGISTERED.fetch_add(1, Ordering::Relaxed);
                ScanoutEventReg::Registered
            }
            None => {
                AQ_REGISTER_REFUSED.fetch_add(1, Ordering::Relaxed);
                ScanoutEventReg::TableFull
            }
        }
    }

    /// Remove one registration and drop the reference the table held. Returns
    /// false if nothing matched. PASSIVE (escape).
    pub(crate) fn unregister_event(&self, owner: usize, event: NonNull<KEVENT>) -> bool {
        let raw = event.as_ptr() as usize;
        let removed = {
            let mut table = self.events.lock();
            let mut found = false;
            let mut i = 0;
            while i < EVENT_TABLE_LEN {
                if matches!(table[i], Some(e) if e.owner == owner && e.event == raw) {
                    table[i] = None;
                    found = true;
                    break;
                }
                i += 1;
            }
            found
        };
        if removed {
            // SAFETY: the table owned one reference to this live KEVENT; deref
            // outside the lock, DeferDelete for the same reason as `reset`.
            unsafe { ObDereferenceObjectDeferDelete(event.as_ptr() as PVOID) };
        }
        removed
    }

    /// Dereference (NO signal — the process is gone) every registration owned
    /// by `owner`. `dxgkddi_destroy_device`, beside `release_blobs_for_owner`;
    /// this is the reclaim the one-shot fence-event table lacks. PASSIVE.
    pub(crate) fn reclaim_events_for_owner(&self, owner: usize) {
        let drained = self.drain_events(Some(owner));
        for entry in drained.iter().flatten() {
            // SAFETY: the entry owned one object reference; DeferDelete so the
            // possible last-reference deletion never runs at a raised IRQL.
            unsafe { ObDereferenceObjectDeferDelete(entry.event as PVOID) };
        }
    }

    /// Drop every remaining event reference without signaling. `AdapterContext
    /// ::drop` only — the table should already be empty (StopDevice reset +
    /// per-device reclaim), so this is the belt for a registration that landed
    /// between the last reset and RemoveDevice. PASSIVE.
    pub(crate) fn drop_all_event_references(&self) {
        let drained = self.drain_events(None);
        for entry in drained.iter().flatten() {
            // SAFETY: the entry owned one object reference; DeferDelete as in
            // every other removal path.
            unsafe { ObDereferenceObjectDeferDelete(entry.event as PVOID) };
        }
    }

    /// Take out every entry (or every entry of one owner) under the leaf lock.
    /// The caller signals/derefs OUTSIDE the lock.
    fn drain_events(&self, owner: Option<usize>) -> [Option<EventEntry>; EVENT_TABLE_LEN] {
        let mut out = [None; EVENT_TABLE_LEN];
        let mut table = self.events.lock();
        let mut i = 0;
        while i < EVENT_TABLE_LEN {
            let matches_owner = match (table[i], owner) {
                (Some(_), None) => true,
                (Some(e), Some(o)) => e.owner == o,
                (None, _) => false,
            };
            if matches_owner {
                out[i] = table[i].take();
            }
            i += 1;
        }
        out
    }

    /// Signal every registered event. Runs under the leaf lock at up to
    /// DISPATCH — `KeSetEvent(Wait = FALSE)` for ≤16 entries, nothing else.
    fn broadcast(&self) {
        let table = self.events.lock();
        let mut i = 0;
        while i < EVENT_TABLE_LEN {
            if let Some(e) = table[i] {
                // SAFETY: the entry holds an object reference, so the KEVENT is
                // alive regardless of the registering process's fate; Wait =
                // FALSE keeps this legal at DISPATCH under the leaf lock.
                unsafe { KeSetEvent(e.event as *mut KEVENT, 0, 0) };
                AQ_SIGNALS.fetch_add(1, Ordering::Relaxed);
            }
            i += 1;
        }
    }
}
