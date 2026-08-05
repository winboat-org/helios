//! L3b — root arguments, descriptor binding, clears.
//!
//! Owns 21 of `COMMAND_LIST_FUNCS_3D_0108`: root arguments 16, clears/discard 5.
//!
//! ⚠ `D3D12DDI_ROOT_CONSTANTS` orders its three `UINT`s **differently** from the
//! API's `D3D12_ROOT_CONSTANTS`, so a cast transposes them — one of the two
//! silent ABI hazards `DX12.md` §4.3 row 4 names, and neither is catchable by
//! the compiler.
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

/// Install L3b's 21 command-list slots.
///
/// Chain position: `RecordSlots` -> `RootArgSlots` on the command-list table.
pub(crate) fn install(
    mut filling: Filling<'_, CommandListTable, stage::RecordSlots>,
) -> Filling<'_, CommandListTable, stage::RootArgSlots> {
    // Touching the table here is what makes the borrow real rather than a
    // formality, and it is what a landing lane replaces with typed field
    // assignments: `f.pfn... = Some(handler);`, each checked by the compiler
    // against the bindgen signature (`PARALLEL.md` §7).
    let _table = filling.table();
    filling.advance()
}

