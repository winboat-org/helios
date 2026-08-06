//! L5 — descriptor heaps and views.
//!
//! Owns 15 of `DEVICE_FUNCS_CORE_0109` (group (f)).
//!
//! ⭐ H3's good surprise: `D3D12DDI_CPU_DESCRIPTOR_HANDLE{SIZE_T}` and
//! `_GPU_{UINT64}` are **opaque driver-chosen scalars**, and
//! `pfnGetDescriptorSizeInBytes` lets the driver pick the stride — so this lane
//! returns **vkd3d's own handle values and stride verbatim** and needs no shadow
//! table.
//!
//! ⛔ Two hazards that are silent, and both have shipped before in this project:
//! `pfnGetCPU/GPUDescriptorHandleForHeapStart` return **by value** while vkd3d's
//! C implementation returns via a hidden pointer — the `ead692e` truncation
//! class that crash-looped dwm and LogonUI at cold boot. And descriptor-heap
//! **flags collide on `0x1` with different meanings** between the DDI and the
//! API, so passing `Flags` through inverts them (`DX12.md` §4.3 row 4).
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

/// Install L5's 15 device-core slots.
///
/// Chain position: `ResourceSlots` -> `DescriptorSlots` on the device-core table.
pub(crate) fn install(
    mut filling: Filling<'_, DeviceCoreTable, stage::ResourceSlots>,
) -> Filling<'_, DeviceCoreTable, stage::DescriptorSlots> {
    // Touching the table here is what makes the borrow real rather than a
    // formality, and it is what a landing lane replaces with typed field
    // assignments: `f.pfn... = Some(handler);`, each checked by the compiler
    // against the bindgen signature (`PARALLEL.md` §7).
    let _table = filling.table();
    filling.advance()
}

/// L5's refusal counters, printed by `crate::log_refusal_summary` at this
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
/// one — the array is iterated on every summary, so the day L5
/// (descriptor heaps and views) lands, its counters appear at
/// exactly this position.
pub(crate) static REFUSALS: &[&RefusalCounter] = &[];
