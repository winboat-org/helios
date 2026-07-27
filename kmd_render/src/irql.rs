//! IRQL proof tokens (R614).
//!
//! `virtio::ctrl`'s contract — "every function in this module MUST be called at
//! PASSIVE_LEVEL" — used to be one prose comment guarding 34 entry points that
//! sleep ([`KeDelayExecutionThread`]), wait on KEVENTs
//! ([`KeWaitForSingleObject`]) or allocate contiguous DMA memory
//! ([`MmAllocateContiguousMemory`]). Those entry points are reached from DDIs
//! with mixed IRQL contracts, and two of them are documented callable above
//! PASSIVE:
//!
//! * `DxgkDdiSetVidPnSourceAddress` is documented callable at DIRQL, which is
//!   why `ddi::display` splits it into a DIRQL atomics-only half and a PASSIVE
//!   worker continuation. `program_vidpn_source` → `ctrl::set_scanout_blob` is
//!   the call that split exists to keep unreachable from the DIRQL entry.
//! * `DxgkDdiMapCpuHostAperture` is documented PASSIVE but arrives at DISPATCH
//!   in practice (ETW-proven, v71), which is why it carries a runtime IRQL gate
//!   and defers with `STATUS_NO_MEMORY`.
//!
//! Before this module, adding a `ctrl::` call to the DIRQL half of either DDI
//! compiled, linked and shipped, and then either deadlocked at DISPATCH on a
//! KEVENT wait or called `MmAllocateContiguousMemory` above APC_LEVEL. Now it
//! does not compile: the callee needs a [`PassiveLevel`], the DIRQL half has
//! none, and the only way to make one is an `unsafe` block someone has to write.
//!
//! # The honest limit
//!
//! A [`PassiveLevel`] does **not** prove the live IRQL. Only
//! [`KeGetCurrentIrql`] can, and calling it per `ctrl::` call was explicitly out
//! of scope. What the token does is relocate one module-wide prose comment to:
//!
//! 1. **One audited [`PassiveLevel::assume`] per DDI entry point**, each with a
//!    `// SAFETY:` naming that DDI's documented IRQL annotation. Three of them
//!    sit downstream of a real `KeGetCurrentIrql` check that already existed, so
//!    for those the claim is checked rather than asserted.
//! 2. **One structural claim** on the Venus layer: a `VenusClient` method runs
//!    only inside `AdapterContext::with_venus_client`, which required its
//!    caller's token, so the token `VenusRing` stores at bring-up stands in for
//!    the caller's. `with_venus_client` is the only gateway to
//!    `&mut VenusClient` (the field is a private `UnsafeCell` and the only other
//!    accessor moves the client by value); the client that exists *before* it is
//!    installed is reachable only from the bring-up chain, which threads a real
//!    parameter. That is what replaces ~95 threaded parameters with one field.
//!
//! Both are assertions about *provenance*, not about the live IRQL. This is the
//! same trade the codebase already accepts for `WddmNotifyGuard`
//! (`adapter.rs`): the guard proves a lock is held only in the sense that safe
//! code cannot name one without going through the acquire.
//!
//! [`IRQL_ASSUME_BAD`] is what makes a wrong audit evidence instead of a
//! mystery: every mint checks, and a violation is counted and surfaces as the
//! `IrqlBad` registry breadcrumb.
//!
//! There is one PASSIVE-only operation the token cannot reach:
//! `virtio::hal::DmaBuffer`'s `Drop` (`MmFreeContiguousMemory`). `Drop::drop`
//! has a fixed signature, so its contract stays a doc comment on the type.
//!
//! # `_passive` at the leaves
//!
//! At the three true leaves — `ctrl::sleep_ms` (`KeDelayExecutionThread`),
//! `ctrl::wait_block` (`KeWaitForSingleObject`) and `DmaBuffer::new`
//! (`MmAllocateContiguousMemory`) — the token has nothing to forward to, so the
//! parameter is spelled `_passive`. That underscore is not an oversight: the
//! parameter is a *precondition*, not data, and those three are precisely the
//! calls the whole contract is about.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicU32, Ordering};

use wdk_sys::ntddk::KeGetCurrentIrql;

/// `PASSIVE_LEVEL` as returned by `KeGetCurrentIrql`.
///
/// One site for what used to be two module-local copies plus a bare `irql != 0`
/// literal on the SetVidPnSourceAddress split. Deliberately NOT taken from
/// `wdk_sys`: `PASSIVE_LEVEL` is a C preprocessor macro, so bindgen does not
/// emit it and assuming otherwise is a build break, not a fallback.
pub(crate) const PASSIVE_LEVEL_IRQL: u8 = 0;

/// Mint sites whose `// SAFETY:` claim was FALSE on this machine, packed with
/// the last offending IRQL as `(count << 8) | irql`.
///
/// Mirrored to the registry as `IrqlBad` by the PASSIVE telemetry flush in
/// `adapter.rs` — NOT from [`PassiveLevel::assume`] itself, because that runs at
/// whatever IRQL the caller was at and `record_named_bytes` is a synchronous
/// `RtlWriteRegistryValue`, i.e. PASSIVE-only. Same mirroring pattern as
/// `AbnDrop`, `WtTbl` and `WnRcf`.
///
/// **It must read 0.** A nonzero value is not a transport fault or a host fault:
/// it means a documented-PASSIVE DDI arrived above PASSIVE and the audit at that
/// mint site is wrong. The low byte names the IRQL it arrived at, which is the
/// first thing a post-mortem wants; the count is `value >> 8`.
///
/// Packed into one DWORD rather than two names because a second registry name
/// that only ever reads 0 alongside this one is noise, and the codebase already
/// packs two facts into one breadcrumb (`HpdI`, `DspMd`).
pub(crate) static IRQL_ASSUME_BAD: AtomicU32 = AtomicU32::new(0);

/// Proof that the calling thread is at `PASSIVE_LEVEL`.
///
/// Zero-sized, so a by-value parameter costs neither a register nor a stack
/// slot. That matters: this token threads through `DxgkDdiStartDevice` →
/// `bring_up_venus` → `allocate_host_visible_blob` → `VenusRing::bring_up`, and
/// that chain had 368 bytes of headroom against the 17936-byte known-good
/// ceiling when this landed (22.22.181.0 shipped 18800 and would not boot). The
/// review asked for `&PassiveLevel`; a reference to a ZST is a real 8-byte
/// pointer per frame, so by value is the only correct choice here.
///
/// `Copy` is deliberate: the token proves *provenance*, not exclusivity, and it
/// is unconstructible outside this module regardless. `PhantomData<*const ()>`
/// keeps it `!Send`/`!Sync` — an IRQL is a property of a thread, so a token must
/// never cross one.
#[derive(Clone, Copy)]
pub(crate) struct PassiveLevel {
    _not_send: PhantomData<*const ()>,
}

impl PassiveLevel {
    /// Assert that this thread is at `PASSIVE_LEVEL`.
    ///
    /// This is the irreducibly unsafe half of the token: one call per DDI entry
    /// point (plus the HPD worker thread body), each with a `// SAFETY:` naming
    /// the DDI's documented IRQL annotation. A mint whose justification is "it
    /// seems to work" is the failure mode this exists to prevent — that is
    /// laundering, and it is worse than no token at all.
    ///
    /// # Why this returns a token even when the check fails
    ///
    /// A wrong IRQL is counted into [`IRQL_ASSUME_BAD`] and the token is handed
    /// over anyway. That is not a swallow: the refusal a violation needs is a
    /// *legal NTSTATUS for the DDI that was entered*, and this constructor
    /// cannot know one. `DxgkDdiMapCpuHostAperture` must defer with
    /// `STATUS_NO_MEMORY` and never `STATUS_UNSUCCESSFUL` (out of its legal set,
    /// so dxgkrnl discards the whole VidPn); `DxgkDdiBuildPagingBuffer`'s
    /// content gate deliberately keeps `STATUS_SUCCESS`; the display DDI defers
    /// to a worker. All three already own that decision at the DDI, above their
    /// mint. `IrqlBad` moving is the signal that a fourth DDI needs one too —
    /// and returning a `Result` here would instead have put an unreachable
    /// error arm at all twelve audited sites and a new refusal population in a
    /// tranche whose whole claim is "identical counters, identical desktop".
    ///
    /// # Safety
    /// The caller must be running at `PASSIVE_LEVEL`, and must say why in a
    /// `// SAFETY:` comment that cites evidence — a DDI's documented IRQL
    /// annotation, a runtime `KeGetCurrentIrql` gate immediately above, or a
    /// system-thread body.
    #[inline]
    pub(crate) unsafe fn assume() -> Self {
        // SAFETY: KeGetCurrentIrql is callable at any IRQL.
        let irql = unsafe { KeGetCurrentIrql() };
        if irql != PASSIVE_LEVEL_IRQL {
            // Relaxed: this is a counter read by a registry flush on another
            // thread, not a synchronization edge. A lost increment under a
            // simultaneous violation on two CPUs is irrelevant — the first
            // nonzero read is the whole signal.
            // Clamp the count explicitly rather than letting `<< 8` discard the
            // high bits: 0xFF_FFFF violations is already far past "escalate".
            let prior = IRQL_ASSUME_BAD.load(Ordering::Relaxed) >> 8;
            let count = prior.saturating_add(1).min(0x00FF_FFFF);
            IRQL_ASSUME_BAD.store((count << 8) | u32::from(irql), Ordering::Relaxed);
        }
        Self {
            _not_send: PhantomData,
        }
    }
}
