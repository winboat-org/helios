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
//! # ⭐ What `pfnPresent` actually is, and why it needs no KMD work
//!
//! `PFND3D12DDI_PRESENT_0051` **outputs** handles. It is not a submission: the
//! driver's whole job is to name the kernel objects the runtime should then hand
//! `D3DKMTPresent` — the back buffer's `D3DKMT_HANDLE`, the WDDM context to submit
//! on, and a few scalars. Nothing here builds a packet, and nothing here touches
//! the scanout.
//!
//! ⭐ **A windowed DWM-composited present never reaches `DxgkDdiPresent` at all**,
//! which is measured rather than assumed: `PRESENT_FLAGS_HISTOGRAM`
//! (`kmd_render/src/ddi/scanout_trace.rs`) is unsampled and non-overflowing, only
//! `0x1` and `0xC` have ever arrived, and `RedirectedFlip` has zero occurrences in
//! `kmd_render`. So the windowed path this lane targets needs **no**
//! `DIRECT_SCANOUT` bit, no host stride agreement and none of the 0ab machinery.
//! Fullscreen flip is a later, separate piece of work.
//!
//! # ⛔ What is still missing, and it is one accessor in another lane's file
//!
//! `D3D12DDI_PRESENT_CONTEXTS_0051::hContext` must be the WDDM context the queue
//! created in `pfnCreateCommandQueue` — `QueueState::h_context`, a **private field
//! of a struct in `forward12::queue.rs`**, which this lane does not own and must
//! not edit. There is no `pub(crate)` accessor for it and no `pub(crate)`
//! `queue_state`, so [`present`] below fills every field it can, refuses with
//! `PresentQueueContextUnavailable`, and writes no handles — because a present
//! descriptor naming a back buffer with no context to submit it on is a request
//! dxgkrnl cannot serve, and filling it halfway would be a silent wrong answer
//! rather than a loud missing one.
//!
//! ⇒ The one-line change that completes it, for the lane that owns `queue.rs`:
//! a `pub(crate) fn present_context(&self) -> *mut c_void` on `QueueState` plus
//! `pub(crate)` on `queue_state`, or a single
//! `pub(crate) unsafe fn present_context(h: D3D12DDI_HCOMMANDQUEUE) -> Option<*mut c_void>`.

use helios_umd_common::refusals::RefusalCounter;

use windows::core::Interface;

use super::tables12::{stage, Filling};
use super::tables12::{CommandListTable, DeviceCoreTable};
use super::{identity12, resource12};
use crate::{ddi12, log_error, note_refusal};

/// How many times any one bounded evidence line may repeat, per site.
///
/// Same idiom and same reason as `resource12`'s: the counter is unbounded, the log
/// line is not. Present is a per-frame path, so an unbounded logger here would be
/// the T2 measurement all over again at 200 Hz.
const LOG_BUDGET: usize = 32;

/// `pfnGetPresentPrivateDriverDataSize` — **0 bytes, and that is the answer rather
/// than a placeholder.**
///
/// The runtime asks how many bytes of driver-private data to carry alongside a
/// present. This driver needs none, and the reason is a measurement this project
/// paid for twice:
///
/// ⛔ **Present private data never reaches `DxgkDdiPresent` on a DMA flip** (64th
/// session, recorded as PERMANENT in `DECISIONS.md`'s D4b chain) — it rides the
/// **Render** command instead. So the identity channel is `HeliosPresentRenderCmd`
/// on `pfnRenderCb`, not a present private-data trailer, and asking the runtime to
/// allocate a trailer nothing reads would be bytes per frame in exchange for
/// nothing.
///
/// ⚠ `KMD_IMPACT.md` §14a.3 UP-8 describes *"0, with a 72-byte arm behind a knob for
/// U6's arrival half"* — U6 being the open question of whether the runtime ever
/// delivers that trailer to the kernel. The knob arm is **not** implemented here:
/// the knobs live in `knobs12.rs`, which this lane does not own, and a knob whose ON
/// arm nothing consumes would be a configuration with no stated meaning. The census
/// counter below is what says how often the runtime asks.
///
/// # Safety
/// `_p_present` is not dereferenced. Declared `unsafe` because the DDI's PFN typedef
/// is.
unsafe extern "C" fn get_present_private_driver_data_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _p_present: *const ddi12::D3D12DDIARG_PRESENT_0001,
) -> ddi12::UINT {
    L8_REFUSALS.present_private_data_size_queries.bump();
    0
}

/// `pfnPresent` — name the kernel objects the runtime should present with.
///
/// # ⭐ It is an OUT-parameter DDI, and that is the whole shape of it
///
/// `D12-G5` established the argument list; the four out-structs are
/// `D3D12DDI_PRESENT_0051` (the allocations and the scalars),
/// `D3D12DDI_PRESENT_CONTEXTS_0051` (the WDDM context) and
/// `D3D12DDI_PRESENT_HWQUEUES_0051` (hardware queues, which this driver has none of
/// — `pfnCreateHwQueue` is refused with `HwQRef`). Nothing is submitted from here.
///
/// The values this driver answers with, each with its reason:
///
/// | field | value | why |
/// |---|---|---|
/// | `BroadcastSrcAllocation[0]` | the back buffer's `D3DKMT_HANDLE` | the UP-5 allocation, out of the `identity12` table |
/// | `BroadcastDstAllocation[0]` | 0 | there is no destination allocation: a windowed present's destination is DWM's, named by the runtime and not by this driver |
/// | `AddedGpuWork` | `FALSE` | ⛔ this DDI records no GPU work. `TRUE` would tell the runtime to expect a submission that never happens |
/// | `BackBufferMultiplicity` | 1 | see the counter's note — **unpriced**, and instrumented for exactly that reason |
/// | `SyncIntervalOverrideValid` | `FALSE` | the driver does not override the application's sync interval; `SyncIntervalOverride` is therefore left alone |
/// | `_CONTEXTS.hContext` | the queue's WDDM context | ⛔ **unavailable — see the module doc** |
/// | `_CONTEXTS.BroadcastContextCount` | 0 | one adapter, one context; broadcast is a multi-GPU shape |
/// | `_HWQUEUES.BroadcastQueueCount` | 0 | this driver refuses hardware queues |
///
/// # ⛔ Two runtime-shape hazards, both measured rather than reasoned
///
/// 1. **`pContexts` and `pHwQueues` are `_Out_opt_`.** `D12-G5` saw `pHwQ`
///    **non-NULL at `_0040` and NULL at `_0110`**, so a driver that writes through
///    them unconditionally faults on one generation of the runtime and not the
///    other. Every write below is behind its own null check.
/// 2. **`hDstResource` was measured NULL only on the WARP control arm**, which has
///    no display adapter — so its NULL-ness is *not* established for this driver,
///    and a non-NULL value would mean the runtime is asking for a
///    resource-to-resource present this lane does not implement. It is refused
///    loudly with its own counter rather than ignored, because ignoring it would
///    present the back buffer to the wrong destination.
///
/// # Safety
/// `p_present`, when non-null, must be a live `D3D12DDIARG_PRESENT_0001` with
/// `SurfacesToPresent` valid entries at `phSurfacesToPresent`. The three out-structs
/// must be live for the call when non-null. `h_queue`'s private block, when
/// non-null, must be one `pfnCreateCommandQueue` wrote.
unsafe extern "C" fn present(
    _h_command_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_queue: ddi12::D3D12DDI_HCOMMANDQUEUE,
    p_present: *const ddi12::D3D12DDIARG_PRESENT_0001,
    p_out: *mut ddi12::D3D12DDI_PRESENT_0051,
    p_contexts: *mut ddi12::D3D12DDI_PRESENT_CONTEXTS_0051,
    p_hw_queues: *mut ddi12::D3D12DDI_PRESENT_HWQUEUES_0051,
) {
    // ⛔ The out-structs are cleared FIRST, every one that exists, before anything
    // that can refuse. The runtime allocated them and this driver does not know what
    // it left in them; a refusal that returned early without zeroing would hand back
    // whatever was there — and `BroadcastSrcAllocation` is 65 `D3DKMT_HANDLE`s, so
    // "whatever was there" is 65 chances to name a kernel object at random.
    // SAFETY: each pointer is either null — `as_mut` then yields `None` and nothing
    // is written — or a live out-struct the runtime allocated for this call, per the
    // caller's guarantee. `Default` for each of the three is bindgen's `write_bytes`
    // zero fill, so the whole struct is initialised by the assignment and no field is
    // left holding what the runtime had there.
    unsafe {
        if let Some(out) = p_out.as_mut() {
            *out = ddi12::D3D12DDI_PRESENT_0051::default();
        }
        if let Some(contexts) = p_contexts.as_mut() {
            *contexts = ddi12::D3D12DDI_PRESENT_CONTEXTS_0051::default();
        }
        if let Some(hw_queues) = p_hw_queues.as_mut() {
            *hw_queues = ddi12::D3D12DDI_PRESENT_HWQUEUES_0051::default();
        }
    }

    // SAFETY: `_In_ CONST` and live for the call when non-null, per the caller.
    let Some(arg) = (unsafe { p_present.as_ref() }) else {
        note_refusal(&L8_REFUSALS.present_bad_arg);
        return;
    };
    if p_out.is_null() {
        // The allocations out-struct is the one output the runtime cannot proceed
        // without, so its absence is a distinct fault from a null argument struct.
        // ⚠ Tested for nullness rather than bound: nothing below this point writes
        // through it — see the tail — so a binding would be an unused `&mut` and the
        // only thing that matters here is that the refusal is attributed.
        note_refusal(&L8_REFUSALS.present_no_out_struct);
        return;
    }

    // ⛔ A destination resource means a present this lane does not implement. Loud,
    // with its own counter: the measured NULL came from WARP, which has no display
    // adapter, so nothing establishes that this driver will only ever see NULL.
    if !arg.hDstResource.pDrvPrivate.is_null() {
        note_refusal(&L8_REFUSALS.present_dst_resource_refused);
        log_error!(
            "L8: pfnPresent with hDstResource={:p} subresource={} -- a resource-to-resource \
             present is not implemented; refusing rather than presenting the back buffer to the \
             wrong destination (flags={:#x} vidpn={} surfaces={})",
            arg.hDstResource.pDrvPrivate,
            arg.DstSubResourceIndex,
            // SAFETY: the union's `Value` arm is a plain `UINT` overlaying the
            // bitfield; reading it is defined for any initialised bit pattern.
            unsafe { arg.Flags.__bindgen_anon_1.Value },
            arg.VidPnSourceID,
            arg.SurfacesToPresent,
        );
        return;
    }

    // The source surface. ⚠ Exactly one is read: `BroadcastSrcAllocation` is a
    // multi-GPU broadcast array and this adapter is single-node, so entries above 0
    // have no meaning here. More than one arriving is counted rather than silently
    // truncated.
    if arg.SurfacesToPresent == 0 || arg.phSurfacesToPresent.is_null() {
        note_refusal(&L8_REFUSALS.present_bad_arg);
        log_error!(
            "L8: pfnPresent with SurfacesToPresent={} phSurfacesToPresent={:p}",
            arg.SurfacesToPresent,
            arg.phSurfacesToPresent,
        );
        return;
    }
    if arg.SurfacesToPresent > 1 {
        note_refusal(&L8_REFUSALS.present_extra_surfaces_ignored);
    }
    // SAFETY: `phSurfacesToPresent` is non-null and `SurfacesToPresent >= 1` per the
    // checks above, so element 0 is within the array the runtime supplied.
    let surface = unsafe { &*arg.phSurfacesToPresent };

    // SAFETY: the runtime names a resource handle it obtained from this driver's own
    // create, so the block is ours; the borrow ends inside this function.
    let Some(engine) = (unsafe { resource12::engine_resource(surface.hSurface) }) else {
        note_refusal(&L8_REFUSALS.present_source_unresolved);
        return;
    };
    let Some(identity) = identity12::lookup(engine.as_raw() as usize) else {
        // ⛔ The back buffer has no WDDM allocation, so there is nothing to present.
        // Distinct from `PresentSourceUnresolved`: the resource IS this driver's, and
        // it was not adopted — which means the create did not see
        // `D3D12DDI_HEAP_FLAG_PRIMARY`. Read this against `HeapPrimaryVenusExport`.
        note_refusal(&L8_REFUSALS.present_source_not_adopted);
        log_error!(
            "L8: pfnPresent source resource {:#x} has no kernel allocation -- it was never \
             adopted, so the runtime has nothing to present. Check HeapPrimaryVenusExport \
             against IdentityRecorded (subresource={})",
            engine.as_raw() as usize,
            surface.SubResourceIndex,
        );
        return;
    };

    // ⛔⛔ THE ONE MISSING FIELD. `QueueState::h_context` is private to
    // `forward12::queue.rs`, which this lane does not own — see the module doc for
    // the exact accessor that completes this. Refused here, before any handle is
    // written, because a present descriptor naming a back buffer with no context to
    // submit it on is a request dxgkrnl cannot serve.
    //
    // ⚠ The queue handle is validated for non-nullness so that when the accessor
    // lands the check is already the right one, and so that "the runtime passed no
    // queue" and "this driver cannot read the queue's context" are different
    // counters.
    if h_queue.pDrvPrivate.is_null() {
        note_refusal(&L8_REFUSALS.present_bad_arg);
        return;
    }
    note_refusal(&L8_REFUSALS.present_queue_context_unavailable);
    let n = L8_REFUSALS.present_queue_context_unavailable.get();
    if n <= LOG_BUDGET {
        log_error!(
            "L8: pfnPresent REFUSED -- the queue's WDDM context is unreachable from this lane \
             (QueueState::h_context is private to forward12::queue.rs and has no accessor), so \
             no present descriptor can be completed. Everything else was ready: alloc={:#x} \
             venus_res_id={} {}x{} fmt={} queue={:p} (x{n})",
            identity.h_allocation,
            identity.venus_res_id,
            identity.geometry.width,
            identity.geometry.height,
            identity.geometry.dxgi_format,
            h_queue.pDrvPrivate,
        );
    }
    // ⛔ Every out-struct is left as the zeroed one written at the top: no allocation
    // handle, and `BackBufferMultiplicity` deliberately NOT set to 1 either. Writing
    // the scalars while withholding the handles would produce a descriptor that looks
    // half-valid; an all-zero descriptor is unmistakably "the driver answered
    // nothing", which is what `METHOD.md` §5's *"trusting a zero"* asks a reader to be
    // able to tell apart.
}

/// Install L8's one device-core slot, `pfnGetPresentPrivateDriverDataSize`.
///
/// Chain position: `FenceSlots` -> `PresentSlots` on the device-core table.
pub(crate) fn install_core(
    mut filling: Filling<'_, DeviceCoreTable, stage::FenceSlots>,
) -> Filling<'_, DeviceCoreTable, stage::PresentSlots> {
    let table = filling.table();
    table.pfnGetPresentPrivateDriverDataSize = Some(get_present_private_driver_data_size);
    filling.advance()
}

/// Install L8's command-list slots.
///
/// Chain position: `CopySlots` -> `PresentSlots` on the command-list table.
///
/// ⚠ **`pfnBlt` is deliberately left on its counting noop.** It is L8's third slot
/// and it is a *different* present model — the legacy DXGI blt path, which
/// `docs/archive/WINDOWED_BLT_DESIGN` and the 34th session's
/// `DXGI_STATUS_OCCLUDED` finding are about. Nothing in `PENDING.md` §S-3 asks for
/// it, no measured D3D12 present has taken it, and installing an untested handler
/// beside the one that matters would put two present models in review at once. Its
/// noop counter is what says whether it is ever entered.
pub(crate) fn install_cmdlist(
    mut filling: Filling<'_, CommandListTable, stage::CopySlots>,
) -> Filling<'_, CommandListTable, stage::PresentSlots> {
    let table = filling.table();
    table.pfnPresent = Some(present);
    filling.advance()
}

/// L8's counters.
struct L8Refusals {
    /// `pfnGetPresentPrivateDriverDataSize` calls served, all answering 0. ⚠ **Not a
    /// refusal — the census**, and the only instrument that says whether the runtime
    /// asks at all. Non-zero with `PresentBadArg` at 0 and no present hits would say
    /// the runtime sizes a trailer for presents it never issues to this driver.
    present_private_data_size_queries: RefusalCounter,
    /// `pfnPresent` with a null argument struct, a null/empty surface list, or a null
    /// queue handle. ⛔ Expected 0: the DDI declares its argument non-optional.
    present_bad_arg: RefusalCounter,
    /// `pfnPresent` with a null `D3D12DDI_PRESENT_0051*`. ⛔ Expected 0 — it is the
    /// one output the runtime cannot proceed without, so its own counter rather than
    /// `PresentBadArg`'s aggregate.
    present_no_out_struct: RefusalCounter,
    /// `pfnPresent` arrived with a **non-NULL `hDstResource`**. ⛔ Expected 0, and
    /// refused: a resource-to-resource present is not implemented. ⚠ Its NULL-ness
    /// is *not* established for this driver — the measurement came from the WARP
    /// control arm, which has no display adapter — so a hit is new information about
    /// the runtime rather than a driver bug.
    present_dst_resource_refused: RefusalCounter,
    /// More than one surface arrived and only element 0 was considered. ⚠ Expected 0
    /// on a single-node adapter; `BroadcastSrcAllocation`'s other 64 entries are a
    /// multi-GPU shape.
    present_extra_surfaces_ignored: RefusalCounter,
    /// The presented surface's handle did not resolve to an engine resource. ⛔
    /// Expected 0: the runtime presents a resource it created through this driver.
    present_source_unresolved: RefusalCounter,
    /// The presented surface resolved but has **no kernel allocation**, i.e. it was
    /// never adopted. ⛔ Expected 0 once a real swapchain runs, and non-zero says the
    /// create never saw `D3D12DDI_HEAP_FLAG_PRIMARY` — read it against
    /// `HeapPrimaryVenusExport` and `IdentityRecorded`, not against this counter
    /// alone.
    present_source_not_adopted: RefusalCounter,
    /// ⛔⛔ **The blocker.** Every `pfnPresent` that got as far as needing the queue's
    /// WDDM context and could not read it, because `QueueState::h_context` is private
    /// to another lane's file. Expected non-zero on every present until that accessor
    /// exists, and expected **0** afterwards; it is the counter that says whether the
    /// one-line change landed.
    present_queue_context_unavailable: RefusalCounter,
}

static L8_REFUSALS: L8Refusals = L8Refusals {
    present_private_data_size_queries: RefusalCounter::new("PresentPrivateDataSizeQueries"),
    present_bad_arg: RefusalCounter::new("PresentBadArg"),
    present_no_out_struct: RefusalCounter::new("PresentNoOutStruct"),
    present_dst_resource_refused: RefusalCounter::new("PresentDstResourceRefused"),
    present_extra_surfaces_ignored: RefusalCounter::new("PresentExtraSurfacesIgnored"),
    present_source_unresolved: RefusalCounter::new("PresentSourceUnresolved"),
    present_source_not_adopted: RefusalCounter::new("PresentSourceNotAdopted"),
    present_queue_context_unavailable: RefusalCounter::new("PresentQueueContextUnavailable"),
};

/// L8's refusal counters, printed by `crate::log_refusal_summary` at this lane's
/// position in `lib.rs`'s `UMD12_REFUSAL_SETS`.
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
pub(crate) static REFUSALS: &[&RefusalCounter] = &[
    &L8_REFUSALS.present_private_data_size_queries,
    &L8_REFUSALS.present_bad_arg,
    &L8_REFUSALS.present_no_out_struct,
    &L8_REFUSALS.present_dst_resource_refused,
    &L8_REFUSALS.present_extra_surfaces_ignored,
    &L8_REFUSALS.present_source_unresolved,
    &L8_REFUSALS.present_source_not_adopted,
    &L8_REFUSALS.present_queue_context_unavailable,
];
