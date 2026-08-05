//! L3a — recording: draw, fixed-function state, IA/SO/OM.
//!
//! Owns 23 of `COMMAND_LIST_FUNCS_3D_0108`: list lifetime 2, draw 3,
//! fixed-function state 11, IA/SO/OM 5, indirect/bundles 2.
//!
//! ⚠ `_0110` obligations that land here and carry **no cap** (`SUBSTRATE.md`
//! §4.5, `DECISIONS.md` D12): triangle-fan topology is mandatory at 0097+, and
//! `D3D12DDI_PIPELINE_STATE_FLAG_DYNAMIC_*` are **hints** — the driver must
//! still apply the PSO's own depth-bias and IB-strip-cut values on every
//! `pfnSetPipelineState`, which is the *inverse* of the Vulkan mental model a
//! vkd3d-shaped forwarder brings. Whatever this lane cannot honour gets a named
//! refusal counter, not silence.
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

/// Install L3a's 23 command-list slots.
///
/// Chain position: `Stubbed` -> `RecordSlots` on the command-list table.
pub(crate) fn install(
    mut filling: Filling<'_, CommandListTable, stage::Stubbed>,
) -> Filling<'_, CommandListTable, stage::RecordSlots> {
    // Touching the table here is what makes the borrow real rather than a
    // formality, and it is what a landing lane replaces with typed field
    // assignments: `f.pfn... = Some(handler);`, each checked by the compiler
    // against the bindgen signature (`PARALLEL.md` §7).
    let _table = filling.table();
    filling.advance()
}

