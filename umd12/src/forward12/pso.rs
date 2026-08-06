//! L6 — pipeline state, root signatures, shaders and sub-state.
//!
//! Owns 38 of `DEVICE_FUNCS_CORE_0109` (groups (b) 12, (c) 14, (e) 12), split
//! across two chain links so the sub-state and shader halves can land
//! independently.
//!
//! ⭐ Root signatures arrive **already parsed** as `D3D12DDI_ROOT_SIGNATURE`,
//! while vkd3d's `CreateRootSignature` wants a serialized DXBC `RTS0` blob — so
//! this lane **re-serializes**, through the second Helios engine export,
//! `helios_vkd3d_serialize_root_signature`. That export exists precisely because
//! `D3D12SerializeRootSignature` lives in the `d3d12.dll` runtime a UMD may not
//! load, and it is already bridged (`bridge12::serialize_root_signature`, proven
//! end to end by `D12-G1`'s third arm).
//!
//! ⚠ Shaders arrive as **raw DXIL, never DXBC** (measured, P1). ⛔ There is no
//! length parameter anywhere in the shader-create DDIs
//! (`DDI_REFERENCE.md` §12.2).
//!
//! ⚠ `DepthBias` silently changed from `INT` to `FLOAT` in the DDI rasterizer
//! desc at 0099, and 0102 revs the struct again — at `_0110`
//! `pfnCreateRasterizerState` receives the 0102 shape, where a `FLOAT DepthBias`
//! sits at the same offset an older `INT` did. **A reinterpretation no compiler
//! will flag** (`SUBSTRATE.md` §4.5).
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

/// Install L6's PSO / root-signature / sub-state device-core slots.
///
/// Chain position: `DescriptorSlots` -> `PsoSlots` on the device-core table.
pub(crate) fn install(
    mut filling: Filling<'_, DeviceCoreTable, stage::DescriptorSlots>,
) -> Filling<'_, DeviceCoreTable, stage::PsoSlots> {
    // Touching the table here is what makes the borrow real rather than a
    // formality, and it is what a landing lane replaces with typed field
    // assignments: `f.pfn... = Some(handler);`, each checked by the compiler
    // against the bindgen signature (`PARALLEL.md` §7).
    let _table = filling.table();
    filling.advance()
}

/// Install L6's shader-create device-core slots (`shaders.rs`'s half of the lane).
///
/// Chain position: `PsoSlots` -> `ShaderSlots` on the device-core table.
pub(crate) fn install_shaders(
    mut filling: Filling<'_, DeviceCoreTable, stage::PsoSlots>,
) -> Filling<'_, DeviceCoreTable, stage::ShaderSlots> {
    // Touching the table here is what makes the borrow real rather than a
    // formality, and it is what a landing lane replaces with typed field
    // assignments: `f.pfn... = Some(handler);`, each checked by the compiler
    // against the bindgen signature (`PARALLEL.md` §7).
    let _table = filling.table();
    filling.advance()
}

/// L6's refusal counters, printed by `crate::log_refusal_summary` at this
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
/// one — the array is iterated on every summary, so the day L6
/// (pipeline state, root signatures, shaders and sub-state) lands, its counters appear at
/// exactly this position.
pub(crate) static REFUSALS: &[&RefusalCounter] = &[];
