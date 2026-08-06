//! The D3D12 DDI surface: 206 device / command-list / queue slots across
//! eleven lanes, plus the sequencer and the counting-noop fill that make the
//! eleven safe to write concurrently.
//!
//! (The other 8 of the 214 are the adapter table, which `crate::adapter12`
//! filled for real at S5.)
//!
//! # The split IS the lane split
//!
//! `ARCHITECTURE.md` §5 specified this module layout before any of it existed,
//! *"for exactly this reason"*, and `PARALLEL.md` §4 assigns one lane per file.
//! Each lane owns its file exclusively; no lane edits another's, and no lane
//! edits [`tables12`] — S6-0 wrote the whole install chain, so a lane's diff
//! against the sequencer is empty (`PARALLEL.md` §5).
//!
//! | file | lane | slots |
//! |---|---|---:|
//! | [`noop12`] | *spine* — the per-slot counting noops and the ABI-order proof | 206 |
//! | [`tables12`] | *spine* — `pfnFillDDITable` and the install chain | — |
//! | [`queue`] | L2 queue · pool · recorder · list lifetime | 17 + 7 |
//! | [`cmdlist`] | L3a recording: draw, fixed function, IA/SO/OM | 23 |
//! | [`rootargs`] | L3b root arguments, descriptor binding, clears | 21 |
//! | [`copy`] | L3c copy, resolve, barriers, queries | 13 |
//! | [`resource12`] | L4 resources, heaps, residency, introspection | 16 |
//! | [`descriptors`] | L5 descriptor heaps and views | 15 |
//! | [`pso`] | L6 PSO, root signatures, shaders, sub-state | 38 |
//! | [`fence`] | L7 fences and query heaps | 6 |
//! | [`present12`] | L8 present | 1 + 2 |
//! | [`misc`] | L9 the tail | 28 + 16 |
//!
//! L1 (caps) is `crate::caps12`, not here: its 43 `D3D12DDICAPS_TYPE` answers
//! hang off `pfnGetCaps` on the **adapter** table, and only 3 of its slots are
//! on the device-core table.
//!
//! ⚠ **`ARCHITECTURE.md` §12 rule 8 is why this is eleven files on day one, not
//! one file that gets split later.** `umd`'s `forward.rs` reached **10 744
//! lines** before T8/R1107 broke it into 19 modules.
//!
//! ⚠ Every lane's slot count above comes from `PARALLEL.md` §4, which derives
//! them from `DECISIONS.md` §4.1. They are not re-derived here, and they are not
//! compiler-checked until each lane lands — at which point its typed field
//! assignments make the grouping self-evident. Until then the *totals* are
//! checked: `noop12`'s macro proves 124 + 75 + 7 against the bindgen structs on
//! every build.

pub(crate) mod noop12;
pub(crate) mod tables12;

pub(crate) mod cmdlist;
pub(crate) mod copy;
pub(crate) mod descriptors;
pub(crate) mod fence;
pub(crate) mod misc;
pub(crate) mod present12;
pub(crate) mod pso;
pub(crate) mod queue;
pub(crate) mod resource12;
pub(crate) mod rootargs;
// ⚠ L6's second file. `PARALLEL.md` §4 gives L6 `pso.rs` **and** `shaders.rs`,
// and `ARCHITECTURE.md` §12 rule 8 is why it is two files on day one rather
// than one that gets split later. The lane's chain links and its whole refusal
// set stay in [`pso`]; this module holds the 14 shader-create slots and the
// DXBC container the engine needs and the DDI does not supply.
pub(crate) mod shaders;
// ⚠ L4's second file, on the same footing as [`shaders`]: `KMD_IMPACT.md`
// §14a.3's UP-4 resource->identity table. It belongs to L4 because
// `pfnCreateHeapAndResource` is its only writer and `pfnDestroyHeapAndResource`
// its only remover, and it is a separate file because L8's present path is its
// intended reader -- so the seam runs between the table and the DDI handlers
// rather than between two lanes' files. It holds no DDI slot; the lane counts in
// the table above are unchanged.
pub(crate) mod identity12;
