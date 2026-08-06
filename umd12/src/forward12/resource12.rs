//! L4 — resources, heaps, residency and introspection.
//!
//! Owns 16 of `DEVICE_FUNCS_CORE_0109` (groups (g) 11, (h) 5).
//!
//! ⛔ **`DECISIONS.md` D13 binds this lane hardest.** The allocation private
//! driver data this lane writes and reads is `helios_protocol`'s, reused
//! **verbatim** — `HeliosWddmAllocPrivate` (`'HWDM'`) into
//! `D3DKMTCreateAllocation`'s `pAllocationInfo[i].pPrivateDriverData`, and
//! `HeliosWddmOpenIdentity` (`'HIDN'`), the record the KMD stamps back at
//! `DxgkDdiOpenAllocation` after validating the venus resource is live. Same
//! crate, same struct, same magic, same version, same meta trailer as
//! `umd/src/forward/resource.rs:351-369` and `:1303-1322`. **Not "a compatible
//! layout".** That is what discharges D3c — *"D3D12-created resources must be
//! able to be opened by DWM, using D3D11 and the 11 DDI"* — in code.
//!
//! ⚠ `umd12` does not depend on `helios_protocol` yet; this lane adds it.
//!
//! ⚠ `pfnOpenHeapAndResource` is one of the runtime's nine hard NULL-checks
//! (`DDI_REFERENCE.md` §9.7), and D3c raises it from "must be non-NULL" to "must
//! actually work". Both arg pointers of `pfnCreateHeapAndResource` are
//! independently nullable.
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

/// Install L4's 16 device-core slots.
///
/// Chain position: `QueueSlots` -> `ResourceSlots` on the device-core table.
pub(crate) fn install(
    mut filling: Filling<'_, DeviceCoreTable, stage::QueueSlots>,
) -> Filling<'_, DeviceCoreTable, stage::ResourceSlots> {
    // Touching the table here is what makes the borrow real rather than a
    // formality, and it is what a landing lane replaces with typed field
    // assignments: `f.pfn... = Some(handler);`, each checked by the compiler
    // against the bindgen signature (`PARALLEL.md` §7).
    let _table = filling.table();
    filling.advance()
}

/// L4's refusal counters, printed by `crate::log_refusal_summary` at this
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
/// one — the array is iterated on every summary, so the day L4
/// (resources, heaps, residency and introspection) lands, its counters appear at
/// exactly this position.
pub(crate) static REFUSALS: &[&RefusalCounter] = &[];
