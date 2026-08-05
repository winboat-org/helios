//! The counting-noop fill for the three driver-side DDI tables — **S6-0's keystone**.
//!
//! `PARALLEL.md` §3: stubbing all 206 device/command-list/queue slots *before*
//! any lane starts converts the whole of S6 from **additive** to
//! **substitutive**. No lane ever adds a slot; it replaces a stub in its own
//! file. (The other 8 of the 214 are the adapter table, which `adapter12` fills
//! for real at S5.)
//!
//! # ⭐ Per-SLOT counters, not per-table, and that is the point
//!
//! `PARALLEL.md` §9.2 makes *"its noop hit counters read **zero** for its slots
//! under a real workload"* a per-lane definition of done, and `CONFORMANCE.md`'s
//! charter is *drive the noop-DDI hit counters to zero*. Neither is executable
//! against one counter per table. So every slot gets its own counter, its own
//! name, and a one-shot first-hit line — which also answers, for free, the
//! question `DDI_REFERENCE.md` §14 exists to estimate: **which slots does a real
//! workload actually call?**
//!
//! # How 206 distinct counting stubs exist without 206 hand-written functions
//!
//! One const-generic function, [`slot_noop`], monomorphised per `(table, slot)`.
//! ⭐ The slot ordinal is **not** written by hand and is not a list position: it
//! is `offset_of!(Table, field) / size_of::<usize>()`, computed by the compiler
//! from the bindgen struct. So the ordinal a counter reports is *derived from the
//! ABI*, and a mis-ordered name list cannot mis-attribute a hit — it fails the
//! order assertion below instead.
//!
//! ```text
//! slot_noop::<TABLE_DEVICE_CORE, { offset_of!(CORE_0109, pfnCreateResource)/8 }>
//! ```
//!
//! # ⭐ The ABI-order proof, which is the real deliverable of this file
//!
//! For each table the macro emits, at compile time and on every build of either
//! platform:
//!
//! * `OFFSETS.len() == size_of::<Table>() / size_of::<usize>()` — the list is
//!   neither short nor long;
//! * `OFFSETS[i] == i * size_of::<usize>()` for every `i` — the list is in
//!   **exactly** the header's field order, with no gaps and no duplicates.
//!
//! A misspelled name is a compile error (the field does not exist); a duplicated,
//! reordered, missing or extra name breaks one of the two assertions. That is
//! what makes it honest to say the slot lists were *extracted* from
//! `umd12/bindgen/cached/d3d12umddi.rs` rather than transcribed: the extraction
//! is re-checked by the compiler on every build.
//!
//! ⚠ This is the `DECISIONS.md` §4.1 scar in machine-checkable form. That table
//! records *"slots 38-40"* as a wrong answer — a `sed` line offset inside the
//! struct misread as a member index — which *"would make a size-derived table
//! installer write into the wrong three slots."*
//!
//! # ⛔ Where the byte count comes from
//!
//! Never from `size_of::<T>()`. `pfnFillDDITable` passes a `SIZE_T`
//! (`d3d12umddi.h:2527-2528`) and it **moves with the negotiated version** —
//! 992/600/56 at `_0110`, 768/464/56 at `_0040` (`DDI_REFERENCE.md` §2.2). The
//! R702 class is 24H2 passing 576 bytes for a 592-byte `DRIVERCAPS` and the
//! D3D11 driver writing past it. `tables12` honours the argument; the only place
//! `size_of::<T>()` is legitimate is [`stubbed_table`], which fills a **local**
//! this crate owns.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use helios_umd_common::noop::{log_backtrace, UniformFn};

use crate::log_error;

/// `D3D12DDI_TABLE_TYPE_DEVICE_CORE`, as a flat-array table index.
///
/// ⚠ Deliberately **not** the `D3D12DDI_TABLE_TYPE` enumerator value: those are
/// 0, 1, 2 today, but they are the runtime's numbering of 25 table types
/// (`DDI_REFERENCE.md` §2.1) and only three of them are ours. Using our own
/// dense index keeps [`HITS`] at 206 entries instead of a sparse 28.
pub(crate) const TABLE_DEVICE_CORE: usize = 0;
/// `D3D12DDI_TABLE_TYPE_COMMAND_LIST_3D`, as a flat-array table index.
pub(crate) const TABLE_COMMAND_LIST: usize = 1;
/// `D3D12DDI_TABLE_TYPE_COMMAND_QUEUE_3D`, as a flat-array table index.
pub(crate) const TABLE_COMMAND_QUEUE: usize = 2;
const TABLE_COUNT: usize = 3;

/// One table's identity for the readout: its name and where its slots start in
/// the flat [`HITS`] array.
struct TableInfo {
    name: &'static str,
    base: usize,
    slots: &'static [&'static str],
}

/// Emit one table's `NAMES` / `STUBS` and the two order assertions.
///
/// See the module doc. The `$table_id` is the flat table index, and the slot
/// ordinal each stub carries is computed by the compiler from `offset_of!`.
macro_rules! ddi_noop_table {
    (
        $(#[$meta:meta])*
        $name:ident, $table:ty, $table_id:ident, [ $($slot:ident),* $(,)? ]
    ) => {
        $(#[$meta])*
        pub(crate) mod $name {
            use super::{slot_noop, UniformFn};
            use crate::ddi12;

            /// The slot names, in header order — the readout's labels.
            pub(crate) const NAMES: &[&str] = &[ $( stringify!($slot) ),* ];

            /// One counting noop per slot, each carrying its own ABI-derived
            /// ordinal. Installed into a driver-owned local by
            /// [`super::stubbed_table`], never written straight into the
            /// runtime's buffer.
            pub(crate) const STUBS: &[UniformFn] = &[ $(
                slot_noop::<
                    { super::$table_id },
                    { ::core::mem::offset_of!($table, $slot)
                        / ::core::mem::size_of::<usize>() },
                >
            ),* ];

            /// ⭐ THE ABI-ORDER PROOF. See the module doc.
            const OFFSETS: &[usize] = &[ $( ::core::mem::offset_of!($table, $slot) ),* ];
            const _: () = {
                assert!(
                    OFFSETS.len()
                        == ::core::mem::size_of::<$table>() / ::core::mem::size_of::<usize>(),
                    "slot list length does not match the bindgen struct's slot count"
                );
                let mut i = 0;
                while i < OFFSETS.len() {
                    assert!(
                        OFFSETS[i] == i * ::core::mem::size_of::<usize>(),
                        "slot list is not in the bindgen struct's field order"
                    );
                    i += 1;
                }
            };
            const _: () = assert!(NAMES.len() == STUBS.len());
        }
    };
}


// ── The three driver-side DDI tables, 206 slots ────────────────────────────
//
// ⭐ The slot lists below were EXTRACTED from `umd12/bindgen/cached/d3d12umddi.rs`
// by parsing the struct bodies, and the `const _` assertions inside the macro
// re-check that extraction on every build: a misspelled name does not compile,
// and a duplicated, reordered, missing or extra name fails the order proof.
//
// 124 + 75 + 7 = 206, plus the 8 adapter slots `adapter12` fills = **214**
// (`DECISIONS.md` §4.1, the canonical count table).

ddi_noop_table! {
    /// `D3D12DDI_DEVICE_FUNCS_CORE_0109` — **124** slots.
    device_core, ddi12::D3D12DDI_DEVICE_FUNCS_CORE_0109, TABLE_DEVICE_CORE, [
        pfnCheckFormatSupport, pfnCheckMultisampleQualityLevels, pfnGetMipPacking,
        pfnCalcPrivateElementLayoutSize, pfnCreateElementLayout, pfnDestroyElementLayout,
        pfnCalcPrivateBlendStateSize, pfnCreateBlendState, pfnDestroyBlendState,
        pfnCalcPrivateDepthStencilStateSize, pfnCreateDepthStencilState,
        pfnDestroyDepthStencilState, pfnCalcPrivateRasterizerStateSize,
        pfnCreateRasterizerState, pfnDestroyRasterizerState, pfnCalcPrivateShaderSize,
        pfnCreateVertexShader, pfnCreatePixelShader, pfnCreateGeometryShader,
        pfnCreateComputeShader, pfnCalcPrivateGeometryShaderWithStreamOutput,
        pfnCreateGeometryShaderWithStreamOutput, pfnCalcPrivateTessellationShaderSize,
        pfnCreateHullShader, pfnCreateDomainShader, pfnDestroyShader,
        pfnCalcPrivateCommandQueueSize, pfnCreateCommandQueue, pfnDestroyCommandQueue,
        pfnCalcPrivateCommandPoolSize, pfnCreateCommandPool, pfnDestroyCommandPool,
        pfnResetCommandPool, pfnCalcPrivatePipelineStateSize, pfnCreatePipelineState,
        pfnDestroyPipelineState, pfnCalcPrivateCommandListSize, pfnCreateCommandList,
        pfnDestroyCommandList, pfnCalcPrivateFenceSize, pfnCreateFence, pfnDestroyFence,
        pfnCalcPrivateDescriptorHeapSize, pfnCreateDescriptorHeap, pfnDestroyDescriptorHeap,
        pfnGetDescriptorSizeInBytes, pfnGetCPUDescriptorHandleForHeapStart,
        pfnGetGPUDescriptorHandleForHeapStart, pfnCreateShaderResourceView,
        pfnCreateConstantBufferView, pfnCreateSampler, pfnCreateUnorderedAccessView,
        pfnCreateRenderTargetView, pfnCreateDepthStencilView, pfnCalcPrivateRootSignatureSize,
        pfnCreateRootSignature, pfnDestroyRootSignature, pfnMapHeap, pfnUnmapHeap,
        pfnCalcPrivateHeapAndResourceSizes, pfnCreateHeapAndResource, pfnDestroyHeapAndResource,
        pfnMakeResident, pfnEvict, pfnCalcPrivateOpenedHeapAndResourceSizes,
        pfnOpenHeapAndResource, pfnCopyDescriptors, pfnCopyDescriptorsSimple,
        pfnCalcPrivateQueryHeapSize, pfnCreateQueryHeap, pfnDestroyQueryHeap,
        pfnCalcPrivateCommandSignatureSize, pfnCreateCommandSignature,
        pfnDestroyCommandSignature, pfnCheckResourceVirtualAddress,
        pfnCheckResourceAllocationInfo, pfnCheckSubresourceInfo,
        pfnCheckExistingResourceAllocationInfo, pfnOfferResources, pfnReclaimResources,
        pfnGetImplicitPhysicalAdapterMask, pfnGetPresentPrivateDriverDataSize, pfnQueryNodeMap,
        pfnRetrieveShaderComment, pfnCheckResourceAllocationHandle,
        pfnCalcPrivatePipelineLibrarySize, pfnCreatePipelineLibrary, pfnDestroyPipelineLibrary,
        pfnAddPipelineStateToLibrary, pfnCalcSerializedLibrarySize, pfnSerializeLibrary,
        pfnGetDebugAllocationInfo, pfnCalcPrivateCommandRecorderSize, pfnCreateCommandRecorder,
        pfnDestroyCommandRecorder, pfnCommandRecorderSetCommandPoolAsTarget,
        pfnCalcPrivateSchedulingGroupSize, pfnCreateSchedulingGroup, pfnDestroySchedulingGroup,
        pfnEnumerateMetaCommands, pfnEnumerateMetaCommandParameters,
        pfnCalcPrivateMetaCommandSize, pfnCreateMetaCommand, pfnDestroyMetaCommand,
        pfnGetMetaCommandRequiredParameterInfo, pfnCalcPrivateStateObjectSize,
        pfnCreateStateObject, pfnDestroyStateObject,
        pfnGetRaytracingAccelerationStructurePrebuildInfo, pfnCheckDriverMatchingIdentifier,
        pfnGetShaderIdentifier, pfnGetShaderStackSize, pfnGetPipelineStackSize,
        pfnSetPipelineStackSize, pfnSetBackgroundProcessingMode,
        pfnCalcPrivateAddToStateObjectSize, pfnAddToStateObject,
        pfnCreateSamplerFeedbackUnorderedAccessView, pfnCreateAmplificationShader,
        pfnCreateMeshShader, pfnCalcPrivateMeshShaderSize, pfnImplicitShaderCacheControl,
        pfnGetProgramIdentifier, pfnGetWorkGraphMemoryRequirements,
    ]
}

ddi_noop_table! {
    /// `D3D12DDI_COMMAND_LIST_FUNCS_3D_0108` — **75** slots.
    command_list, ddi12::D3D12DDI_COMMAND_LIST_FUNCS_3D_0108, TABLE_COMMAND_LIST, [
        pfnCloseCommandList, pfnResetCommandList, pfnDrawInstanced, pfnDrawIndexedInstanced,
        pfnDispatch, pfnClearUnorderedAccessViewUint, pfnClearUnorderedAccessViewFloat,
        pfnClearRenderTargetView, pfnClearDepthStencilView, pfnDiscardResource,
        pfnCopyTextureRegion, pfnResourceCopy, pfnCopyTiles, pfnCopyBufferRegion,
        pfnResourceResolveSubresource, pfnExecuteBundle, pfnExecuteIndirect, pfnResourceBarrier,
        pfnBlt, pfnPresent, pfnBeginQuery, pfnEndQuery, pfnResolveQueryData, pfnSetPredication,
        pfnIaSetTopology, pfnRsSetViewports, pfnRsSetScissorRects, pfnOmSetBlendFactor,
        pfnOmSetStencilRef, pfnSetPipelineState, pfnSetDescriptorHeaps,
        pfnSetComputeRootSignature, pfnSetGraphicsRootSignature,
        pfnSetComputeRootDescriptorTable, pfnSetGraphicsRootDescriptorTable,
        pfnSetComputeRoot32BitConstant, pfnSetGraphicsRoot32BitConstant,
        pfnSetComputeRoot32BitConstants, pfnSetGraphicsRoot32BitConstants,
        pfnSetComputeRootConstantBufferView, pfnSetGraphicsRootConstantBufferView,
        pfnSetComputeRootShaderResourceView, pfnSetGraphicsRootShaderResourceView,
        pfnSetComputeRootUnorderedAccessView, pfnSetGraphicsRootUnorderedAccessView,
        pfnIASetIndexBuffer, pfnIASetVertexBuffers, pfnSOSetTargets, pfnOMSetRenderTargets,
        pfnSetMarker, pfnClearRootArguments, pfnAtomicCopyBufferRegion, pfnOMSetDepthBounds,
        pfnSetSamplePositions, pfnResourceResolveSubresourceRegion,
        pfnSetProtectedResourceSession, pfnWriteBufferImmediate, pfnSetViewInstanceMask,
        pfnInitializeMetaCommand, pfnExecuteMetaCommand,
        pfnBuildRaytracingAccelerationStructure,
        pfnEmitRaytracingAccelerationStructurePostbuildInfo,
        pfnCopyRaytracingAccelerationStructure, pfnSetPipelineState1, pfnDispatchRays,
        pfnRSSetShadingRate, pfnRSSetShadingRateImage, pfnDispatchMesh, pfnBarrier,
        pfnOmSetAlphaBlendFactor, pfnOmSetFrontAndBackStencilRef, pfnRSSetDepthBias,
        pfnIASetIndexBufferStripCutValue, pfnSetProgram, pfnDispatchGraph,
    ]
}

ddi_noop_table! {
    /// `D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001` — **7** slots, two of which the
    /// header itself names `pfnUnused` / `pfnUnused2`. They are stubbed like
    /// every other slot: "unused" is the header's word for them, and a NULL
    /// there is still a NULL the runtime could call through.
    command_queue, ddi12::D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001, TABLE_COMMAND_QUEUE, [
        pfnExecuteCommandLists, pfnUnused, pfnUnused2, pfnUpdateTileMappings,
        pfnCopyTileMappings, pfnSignalFence, pfnWaitForFence,
    ]
}

// ── The flat hit array ─────────────────────────────────────────────────────

const DEVICE_CORE_BASE: usize = 0;
const COMMAND_LIST_BASE: usize = DEVICE_CORE_BASE + device_core::NAMES.len();
const COMMAND_QUEUE_BASE: usize = COMMAND_LIST_BASE + command_list::NAMES.len();
/// 124 + 75 + 7. Derived, then checked against the canonical count table.
pub(crate) const TOTAL_SLOTS: usize = COMMAND_QUEUE_BASE + command_queue::NAMES.len();

// `DECISIONS.md` §4.1 is the only trustworthy count table in this directory, and
// it says 8 + 124 + 75 + 7 = 214. The 8 are `adapter12`'s. If this ever fails,
// the SDK header moved and §4.1 is what has to be re-derived — not this line.
const _: () = assert!(TOTAL_SLOTS == 206);

static HITS: [AtomicU32; TOTAL_SLOTS] = [const { AtomicU32::new(0) }; TOTAL_SLOTS];

static TABLES: [TableInfo; TABLE_COUNT] = [
    TableInfo {
        name: "DEVICE_FUNCS_CORE_0109",
        base: DEVICE_CORE_BASE,
        slots: device_core::NAMES,
    },
    TableInfo {
        name: "COMMAND_LIST_FUNCS_3D_0108",
        base: COMMAND_LIST_BASE,
        slots: command_list::NAMES,
    },
    TableInfo {
        name: "COMMAND_QUEUE_FUNCS_CORE_0001",
        base: COMMAND_QUEUE_BASE,
        slots: command_queue::NAMES,
    },
];

/// Hits on a slot this build has no name for — i.e. past the end of the
/// bindgen struct, in a table the runtime sized LARGER than this SDK header
/// describes.
///
/// ⚠ Non-zero here is a real finding, not noise: it means the runtime
/// negotiated a table shape newer than the header `ddi12` was generated from,
/// and every slot past our knowledge is being served by a stub that does
/// nothing. `tables12` fills that tail deliberately (a NULL there is a crash
/// inside the runtime), and this is how it stays visible rather than silent.
static UNKNOWN_SLOT_HITS: AtomicUsize = AtomicUsize::new(0);

/// How many first-hit backtraces the whole process may spend.
///
/// A backtrace names the *runtime call* that reached an unimplemented slot,
/// which is the difference between "this DDI is missing" and "this DDI is
/// missing **and something actually calls it**". Capturing and formatting 32
/// frames is itself a measured cost (`umd_common::noop`), and with 206 slots an
/// uncapped budget would be a log flood on the first real workload — so the
/// budget buys the first few, which are the informative ones.
const BACKTRACE_BUDGET: usize = 8;
static BACKTRACES_SPENT: AtomicUsize = AtomicUsize::new(0);

/// The uniform counting stub, monomorphised per `(table, slot)`.
///
/// ⛔ **No panic path.** Every lookup is `.get()`; an out-of-range pair falls
/// into [`UNKNOWN_SLOT_HITS`] instead of indexing. A panic in any DDI is a
/// silent graphics deadlock, and this crate is `panic = "abort"`, so it is a
/// dead compositor.
///
/// Returning `0` is correct for both shapes of slot on this ABI: the
/// `HRESULT`-returning ones read it as `S_OK` and the `VOID` ones ignore it
/// (`umd_common::noop::UniformFn`).
///
/// ⚠ Answering `S_OK` from an unimplemented slot is *not* the loud failure
/// CLAUDE.md rule 2 asks for, and it is a deliberate, bounded exception for S6-0
/// only: a stub table exists so `D3D12CreateDevice` can be reached at all, and a
/// failure HRESULT from an arbitrary slot would abort device creation before any
/// lane could measure anything. The counter is what keeps it honest — every hit
/// is named, counted and printed. **Each lane replaces its slots with real
/// bodies whose refusals are real refusals**, and `PARALLEL.md` §9.2 does not
/// call a lane done until its counters read zero.
unsafe extern "C" fn slot_noop<const TABLE: usize, const SLOT: usize>(_arg: usize) -> usize {
    note_slot_hit(TABLE, SLOT);
    0
}

/// The stub for slots past the end of this header's struct. See
/// [`UNKNOWN_SLOT_HITS`].
pub(crate) unsafe extern "C" fn unknown_slot_noop(_arg: usize) -> usize {
    let n = UNKNOWN_SLOT_HITS.fetch_add(1, Ordering::Relaxed);
    if n == 0 {
        log_error!(
            "noop DDI: a slot PAST the end of every table this build knows was called. The \
             runtime negotiated a table larger than d3d12umddi.h describes; refresh the bindings."
        );
    }
    0
}

/// Count one slot hit and, on its first, say which slot it was.
fn note_slot_hit(table: usize, slot: usize) {
    let (Some(info), _) = (TABLES.get(table), ()) else {
        UNKNOWN_SLOT_HITS.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let (Some(name), Some(hits)) = (info.slots.get(slot), HITS.get(info.base + slot)) else {
        UNKNOWN_SLOT_HITS.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if hits.fetch_add(1, Ordering::Relaxed) != 0 {
        return;
    }
    log_error!("noop DDI first hit: {}::{name}", info.name);
    if BACKTRACES_SPENT.fetch_add(1, Ordering::Relaxed) < BACKTRACE_BUDGET {
        // SAFETY: `log_backtrace` calls `RtlCaptureStackBackTrace`, which walks
        // the caller's stack. This runs on an ordinary user-mode DDI thread with
        // no unwind in progress — the crate is `panic = "abort"`, so there is no
        // unwinding anywhere in this module.
        unsafe { log_backtrace(&format!("noop DDI {}::{name}", info.name)) };
    }
}

/// Build a fully typed DDI table whose every slot is a counting noop.
///
/// ⭐ This is what lets a lane write `f.pfnCreateCommandQueue = Some(handler)`
/// with the **compiler checking the signature** (`PARALLEL.md` §7: on a
/// transcription job against someone else's ABI, the compiler is the
/// specification) while every slot the lane does not fill is still non-NULL.
///
/// ⚠ `size_of::<T>()` is correct **here and only here**: `T` is a driver-owned
/// local, not the runtime's buffer. The runtime's buffer is written by
/// `tables12`, bounded by `pfnFillDDITable`'s `SIZE_T`.
///
/// # Safety
/// `T` must be a `#[repr(C)]` struct of exactly `stubs.len()` pointer-sized
/// `Option<fn>` fields — which the `ddi_noop_table!` assertions establish at
/// compile time for the three tables in this module, and which the caller
/// re-asserts at its own call site. Every byte of the returned `T` is written
/// before `assume_init`.
pub(crate) unsafe fn stubbed_table<T>(stubs: &[UniformFn]) -> T {
    let mut local = core::mem::MaybeUninit::<T>::uninit();
    let base = local.as_mut_ptr().cast::<Option<UniformFn>>();
    for (index, stub) in stubs.iter().enumerate() {
        // SAFETY: the caller guarantees `T` is `stubs.len()` pointer-sized
        // fn-pointer slots, so every index in this loop is inside `local`.
        unsafe { base.add(index).write(Some(*stub)) };
    }
    // SAFETY: the loop above wrote every one of `T`'s slots.
    unsafe { local.assume_init() }
}

/// Log every slot that was hit, and nothing about the ones that were not.
///
/// ⭐ This is the instrument `PARALLEL.md` §9.2 and `CONFORMANCE.md` are written
/// against, and T5's lesson is why it exists as a *readout* and not merely as an
/// array of atomics: *an instrument nothing can read is not an instrument* —
/// three of the four R806/R809 scan-out counters were process-global atomics
/// nothing ever loaded, so ROADMAP's own instruction to read them after a gate
/// run was not executable.
///
/// Called from `adapter12::close_adapter`, beside the refusal summary.
pub(crate) fn log_noop_hits() {
    let mut line = String::with_capacity(512);
    let mut hit_slots = 0usize;
    for info in TABLES.iter() {
        for (slot, name) in info.slots.iter().enumerate() {
            let Some(hits) = HITS.get(info.base + slot) else {
                continue;
            };
            let n = hits.load(Ordering::Relaxed);
            if n == 0 {
                continue;
            }
            hit_slots += 1;
            line.push(' ');
            line.push_str(name);
            line.push('=');
            line.push_str(&n.to_string());
        }
    }
    log_error!(
        "D3D12 noop DDI hits: slots={hit_slots}/{TOTAL_SLOTS} unknown={}{line}",
        UNKNOWN_SLOT_HITS.load(Ordering::Relaxed),
    );
}
