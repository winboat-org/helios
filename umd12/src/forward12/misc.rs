//! L9 — the tail: meta-commands, state objects, RT, work graphs, VRS, mesh, scheduling groups, multi-adapter, policy.
//!
//! Owns 28 of `DEVICE_FUNCS_CORE_0109` (groups (k) 4, (l) 3, (m) 6, (n) 13,
//! (o) 2) and 16 of `COMMAND_LIST_FUNCS_3D_0108` (markers/protection 4, meta 2,
//! RT 5, VRS 2, mesh 1, work graphs 2). 44 slots, the largest lane and the
//! cheapest.
//!
//! ⭐ **Mostly refuse-and-count**, which is why `PARALLEL.md` §4 calls it the
//! natural first task for a new agent: nearly every slot here is behind a cap
//! this driver reports as unsupported, so the honest body is a named refusal —
//! and a refusal with a counter is a finished slot, not a stub.
//!
//! ⚠ The exception to watch: `DDI_REFERENCE.md` §14.1.1 splits "may a slot be
//! NULL" into three different questions — retired, optional-feature, reserved —
//! and finds the **command-list table has no opt-out mechanism at all**. So the
//! command-list half of this lane is non-NULL-or-nothing regardless of caps.
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
use super::tables12::{CommandListTable, DeviceCoreTable};

/// Install L9's 28 device-core slots.
///
/// Chain position: `PresentSlots` -> `MiscSlots` on the device-core table.
pub(crate) fn install_core(
    mut filling: Filling<'_, DeviceCoreTable, stage::PresentSlots>,
) -> Filling<'_, DeviceCoreTable, stage::MiscSlots> {
    // Touching the table here is what makes the borrow real rather than a
    // formality, and it is what a landing lane replaces with typed field
    // assignments: `f.pfn... = Some(handler);`, each checked by the compiler
    // against the bindgen signature (`PARALLEL.md` §7).
    let _table = filling.table();
    filling.advance()
}

/// Install L9's 16 command-list slots.
///
/// Chain position: `PresentSlots` -> `MiscSlots` on the command-list table.
pub(crate) fn install_cmdlist(
    mut filling: Filling<'_, CommandListTable, stage::PresentSlots>,
) -> Filling<'_, CommandListTable, stage::MiscSlots> {
    // Touching the table here is what makes the borrow real rather than a
    // formality, and it is what a landing lane replaces with typed field
    // assignments: `f.pfn... = Some(handler);`, each checked by the compiler
    // against the bindgen signature (`PARALLEL.md` §7).
    let _table = filling.table();
    filling.advance()
}

