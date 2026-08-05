//! L8 — present.
//!
//! Owns 3 slots: `pfnGetPresentPrivateDriverDataSize` on the device-core table
//! and `pfnBlt` / `pfnPresent` on the command-list table.
//!
//! ⛔ **Not parallelisable, and it lands after L3a/L3b, not beside them**
//! (`PARALLEL.md` §8): it touches the `HeliosPresentRenderCmd` identity channel
//! shared with the KMD **and** with the D3D11 driver.
//!
//! ⛔ `DECISIONS.md` D13: that channel is `helios_protocol`'s
//! (`HeliosPresentRenderCmd`, `HeliosPresentPrivateData`), reused verbatim. The
//! KMD decodes it (`kmd_render/src/device.rs:46`), so a second D3D12 spelling
//! would be a second thing the KMD has to recognise.
//!
//! ⚠ Present private data **never reaches `DxgkDdiPresent` on DMA flips** — it
//! rides the Render command (64th session, permanent). And ⛔ never reintroduce
//! a producer-side CPU present gate: owner directive, 2026-07-29
//! (`ARCHITECTURE.md` §12 rule 13, `DECISIONS.md` §7.9).
//!
//! ⚠ There is **no DXGI table** to fill: `D3D12DDI_TABLE_TYPE_DXGI` was never
//! requested across 20 flip-model presents, and present arrives on the
//! command-list table (`D12-G5`).
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

/// Install L8's one device-core slot, `pfnGetPresentPrivateDriverDataSize`.
///
/// Chain position: `FenceSlots` -> `PresentSlots` on the device-core table.
pub(crate) fn install_core(
    mut filling: Filling<'_, DeviceCoreTable, stage::FenceSlots>,
) -> Filling<'_, DeviceCoreTable, stage::PresentSlots> {
    // Touching the table here is what makes the borrow real rather than a
    // formality, and it is what a landing lane replaces with typed field
    // assignments: `f.pfn... = Some(handler);`, each checked by the compiler
    // against the bindgen signature (`PARALLEL.md` §7).
    let _table = filling.table();
    filling.advance()
}

/// Install L8's two command-list slots, `pfnBlt` and `pfnPresent`.
///
/// Chain position: `CopySlots` -> `PresentSlots` on the command-list table.
pub(crate) fn install_cmdlist(
    mut filling: Filling<'_, CommandListTable, stage::CopySlots>,
) -> Filling<'_, CommandListTable, stage::PresentSlots> {
    // Touching the table here is what makes the borrow real rather than a
    // formality, and it is what a landing lane replaces with typed field
    // assignments: `f.pfn... = Some(handler);`, each checked by the compiler
    // against the bindgen signature (`PARALLEL.md` §7).
    let _table = filling.table();
    filling.advance()
}

