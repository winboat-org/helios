//! L2 — command queues, pools, recorders and command-list lifetime.
//!
//! Owns 17 of `DEVICE_FUNCS_CORE_0109` (group (d)) **and all 7** of
//! `COMMAND_QUEUE_FUNCS_CORE_0001`.
//!
//! ⭐ This is where the **WDDM context is minted** — one per `ID3D12CommandQueue`,
//! not one per device (`DDI_REFERENCE.md` §6.3, R2 §1.3), through
//! `pfnCreateContextCb` off `pKTCallbacks`. `umd/src/device_funcs.rs:1046-1094`
//! is the same call at a different cardinality.
//!
//! ⚠ The three device-side entry points are members **27, 28 and 29** of the 124
//! (`d3d12umddi.h:13488-13490`). ⛔ NOT "slots 38-40" — that was a `sed` line
//! offset inside the struct misread as a member index (`DECISIONS.md` §4.1), and
//! it is exactly the kind of number `noop12`'s offset proof now makes
//! unrepresentable.
//!
//! ⚠ `pfnCreateCommandList` needs the runtime table handle the fill stashed:
//! `tables12::command_list_rt_table(index)`. It cannot be recovered any other
//! way (`DDI_REFERENCE.md` §2.2).
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
use super::tables12::{CommandQueueTable, DeviceCoreTable};

/// Install L2's 17 device-core slots.
///
/// Chain position: `CapsSlots` -> `QueueSlots` on the device-core table.
pub(crate) fn install_core(
    mut filling: Filling<'_, DeviceCoreTable, stage::CapsSlots>,
) -> Filling<'_, DeviceCoreTable, stage::QueueSlots> {
    // Touching the table here is what makes the borrow real rather than a
    // formality, and it is what a landing lane replaces with typed field
    // assignments: `f.pfn... = Some(handler);`, each checked by the compiler
    // against the bindgen signature (`PARALLEL.md` §7).
    let _table = filling.table();
    filling.advance()
}

/// Install all 7 command-queue slots.
///
/// Chain position: `Stubbed` -> `QueueSlots` on the command-queue table.
pub(crate) fn install_queue(
    mut filling: Filling<'_, CommandQueueTable, stage::Stubbed>,
) -> Filling<'_, CommandQueueTable, stage::QueueSlots> {
    // Touching the table here is what makes the borrow real rather than a
    // formality, and it is what a landing lane replaces with typed field
    // assignments: `f.pfn... = Some(handler);`, each checked by the compiler
    // against the bindgen signature (`PARALLEL.md` §7).
    let _table = filling.table();
    filling.advance()
}

