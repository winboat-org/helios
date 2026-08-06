//! L3c — copy, resolve, barriers and queries.
//!
//! Owns 13 of `COMMAND_LIST_FUNCS_3D_0108`: copy/resolve 7, barriers 2,
//! queries/predication 4.
//!
//! ⭐ **Barriers are ONE path, not two** (`DX12.md` §4.3 row 1): the runtime
//! lowers every `ResourceBarrier` to `pfnBarrier` once the driver reports
//! `EnhancedBarriersSupported = 1`, and *"Legacy barrier DDI's are never
//! invoked"*. The enhanced arm is also the better vkd3d target, because
//! `D3D12_BARRIER_LAYOUT` maps far closer to `VkImageLayout` than
//! `D3D12_RESOURCE_STATES` does. ⛔ The cap stays **0** until `pfnBarrier` is
//! real — at 1 there is no fallback left, and that is L1's line to change, not
//! this lane's.
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
use super::tables12::{CommandListTable};
use helios_umd_common::refusals::RefusalCounter;

/// Install L3c's 13 command-list slots.
///
/// Chain position: `RootArgSlots` -> `CopySlots` on the command-list table.
pub(crate) fn install(
    mut filling: Filling<'_, CommandListTable, stage::RootArgSlots>,
) -> Filling<'_, CommandListTable, stage::CopySlots> {
    // Touching the table here is what makes the borrow real rather than a
    // formality, and it is what a landing lane replaces with typed field
    // assignments: `f.pfn... = Some(handler);`, each checked by the compiler
    // against the bindgen signature (`PARALLEL.md` §7).
    let _table = filling.table();
    filling.advance()
}

/// L3c's refusal counters, printed by `crate::log_refusal_summary` at this
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
/// one — the array is iterated on every summary, so the day L3c
/// (copy, resolve, barriers and queries) lands, its counters appear at
/// exactly this position.
pub(crate) static REFUSALS: &[&RefusalCounter] = &[];
