//! L7 — fences and query heaps.
//!
//! Owns 6 of `DEVICE_FUNCS_CORE_0109` (groups (i) 3, (j) 3).
//!
//! ⭐ **A D3D12 fence object IS a pair of GPU virtual addresses**
//! (`DDI_REFERENCE.md` §10.1), and there are exactly **two** fence operations,
//! both queue-level: `pfnSignalFence` and `pfnWaitForFence` on the command-queue
//! table (L2's). ⛔ There is **no** CPU-signal DDI and **no** CPU-wait DDI
//! (§10.3) — a reading that looks like a missing slot and is not.
//!
//! `DECISIONS.md` §6 downgraded the monitored-fence risk to MEDIUM; the residual
//! probe is G-fence.
//!
//! ⚠ **S6-0: this lane has not landed.** Its slots carry the per-slot counting
//! noops `forward12::noop12` installed, so they are non-NULL and every hit is
//! named, counted and printed by `D3D12 noop DDI hits:`. `PARALLEL.md` §9.2 does
//! not call this lane done until those counters read **zero** under a real
//! workload.
//!
//! The `install` below is not scaffolding: it is a live link in the sequencer's
//! chain (`tables12`), and the chain does not compile without it. What is empty
//! is its body.

use super::tables12::{stage, Filling};
use super::tables12::{DeviceCoreTable};
use helios_umd_common::refusals::RefusalCounter;

/// Install L7's 6 device-core slots.
///
/// Chain position: `ShaderSlots` -> `FenceSlots` on the device-core table.
pub(crate) fn install(
    mut filling: Filling<'_, DeviceCoreTable, stage::ShaderSlots>,
) -> Filling<'_, DeviceCoreTable, stage::FenceSlots> {
    // Touching the table here is what makes the borrow real rather than a
    // formality, and it is what a landing lane replaces with typed field
    // assignments: `f.pfn... = Some(handler);`, each checked by the compiler
    // against the bindgen signature (`PARALLEL.md` §7).
    let _table = filling.table();
    filling.advance()
}

/// L7's refusal counters, printed by `crate::log_refusal_summary` at this
/// lane's position in `lib.rs`'s `UMD12_REFUSAL_SETS`.
///
/// ⭐ **Declared here rather than in `lib.rs` so this lane's diff against the
/// crate root is empty.** Every one of the eleven S6 lanes needs counters
/// (`PARALLEL.md` §9.1: *every skipped or refused path gets a named counter*),
/// and one flat array in `lib.rs` would have been the split's hottest merge
/// point — §5's shared-file table does not even list `lib.rs`. Same move
/// `forward12::tables12` makes for the 206 slots: name all eleven up front and
/// the lanes become substitutive instead of additive.
///
/// ⛔ **Append only.** Counter order inside a set, and set order in
/// `UMD12_REFUSAL_SETS`, are both the evidence contract: `D3D12 DDI refusals:`
/// lines get diffed across builds.
///
/// ⚠ Empty until this lane lands. That is a readable state and not a dead
/// one — the array is iterated on every summary, so the day L7
/// (fences and query heaps) lands, its counters appear at
/// exactly this position.
pub(crate) static REFUSALS: &[&RefusalCounter] = &[];
