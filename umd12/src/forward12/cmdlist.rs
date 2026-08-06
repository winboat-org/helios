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
//! ⚠ **All 23 slots are filled** — as of S-4, **18** forward into the engine,
//! 3 refuse with a named counter and 2 are the list-lifetime pair the spine
//! landed. `PARALLEL.md` §9.2 does not call this lane done until the *noop* hit
//! counters for these slots read zero under a real workload, which only a run
//! can show; what the source can show is that none of the 23 is a noop any more.
//!
//! # ⭐ The three that do NOT forward, and why each one is honest
//!
//! | slot | counter | why |
//! |---|---|---|
//! | `pfnOMSetDepthBounds` | `L3aDepthBoundsDefaultDropped` / `L3aDepthBoundsRefused` | `caps12.rs` reports `DepthBoundsTestSupported = 0`. ⛔ **Two counters, because the runtime calls this slot after EVERY reset** — see [`om_set_depth_bounds`] |
//! | `pfnSetSamplePositions` | `L3aSamplePositionsRefused` | `ProgrammableSamplePositionsTier = NONE` **and** the engine's own body is a `FIXME(...) stub!` |
//! | `pfnOmSetAlphaBlendFactor` | `L3aAlphaBlendFactorRetired` | RETIRED by Microsoft; `pfnOmSetBlendFactor`'s component `[3]` replaced it |
//!
//! ⭐ **`pfnExecuteIndirect` LEFT that table with S-4** and now forwards; the
//! refusal moved one DDI earlier, to `queue::create_command_signature`, which is
//! the only place it can be honest — vkd3d accepts a state-template signature on a
//! guest without `VK_EXT_device_generated_commands` and then **silently skips**
//! every `ExecuteIndirect` through it (`command.c:26447-26453`, `:17811-17818`).
//! Refusing here instead would be the "succeed at create, fail at submit" shape
//! the bundle lesson already cost this lane once.
//!
//! # ⭐ The two exceptions, and why they landed with the Round 2 spine
//!
//! `pfnCloseCommandList` and `pfnResetCommandList` are the list-*lifetime* pair,
//! and both of them are consumers of state that lives in **`queue.rs`** — which
//! is why they landed in the same commit that created it, exactly as
//! `misc.rs`'s three slots landed with the caps sweep that needed them. They are
//! still **L3a's slots, in L3a's file, under L3a's ownership**
//! (`PARALLEL.md` §4); the lane that takes the other 21 inherits them and does
//! not write them again.
//!
//! * ⛔ **Together they are what makes the spine's accessors real rather than
//!   dead.** `PARALLEL.md` §10 forbids `#[allow(dead_code)]` on a hand-written
//!   line (R908), so `queue::CommandListState`, its `h_device`, its `h_rt_list`
//!   and its `list_type`, and `queue::recorder_allocator` could not be committed
//!   without a caller. These two are that caller, and they are the *right* one:
//!   they are the only command-list slots whose whole content is the objects
//!   `queue.rs` owns.
//! * ⭐ `pfnCloseCommandList` is the slot that **proves the error channel is
//!   needed**. `ID3D12GraphicsCommandList::Close` returns an `HRESULT` and the
//!   DDI returns `VOID` — and the DDI is handed the command-list handle and
//!   *nothing else*. Without `CommandListState::h_rt_list` a failed `Close`
//!   would be unreportable, which is `DECISIONS.md` §7.6's problem in its
//!   sharpest form. ⛔ It is `h_rt_list`, **not** `h_device`: see
//!   [`report_error`] for why every failure in this file is a *list*-scoped
//!   report and what the earlier device-scoped one cost.
//! * ⛔ `pfnResetCommandList` is where the module doc of `queue.rs` says the
//!   bundle question lands: *"UNVERIFIED, and it belongs to whoever writes
//!   `pfnResetCommandList` (L3a): how a BUNDLE allocator is expressed at this
//!   DDI."* **It is answered, and the answer is that it is not expressed at all
//!   and bundles therefore do not work yet.** The DDI carries no bundle bit and
//!   the engine enforces the class pairing in both halves
//!   (`command.c:7378-7382`, `bundle.c:411-427`).
//!   ⚠ **The refusal moved one DDI earlier than this file first placed it.**
//!   Refusing at *reset* meant `ID3D12Device::CreateCommandList` had already
//!   returned success, so the application held a bundle it could never record
//!   into — the "succeed at create, fail at submit" shape. `queue.rs`'s
//!   `create_command_list` now refuses `Type == BUNDLE` up front
//!   (`L2BundleListRefused`), so the application gets a failed create it can act
//!   on. This slot's mismatch arm remains as the tripwire for the narrower
//!   DIRECT/COMPUTE/COPY disagreement. The fix that lifts both is `queue.rs`'s,
//!   one allocator per (pool, class). See [`reset_command_list`].
//!
//! # ⭐ What Round 1 routed here and the engine had already done
//!
//! `queue.rs` and `pso.rs` both record an obligation for this lane:
//! `pfnSetPipelineState` must re-apply the PSO's baked depth bias and IB strip
//! cut even when the PSO declares them dynamic. ⛔ **It is discharged by
//! forwarding**, and re-implementing it here would double-apply it —
//! `vkd3d-proton-helios/libs/vkd3d/command.c:12711-12733`, inside
//! `d3d12_command_list_SetPipelineState`, with the comment *"For any optionally
//! dynamic state, we need to re-apply the corresponding static state that the
//! PSO was created with."* The grading of
//! `L6PsoDynamicStateFlagForwarded` was corrected against that source when this
//! file landed; read its doc in `pso.rs` before treating a non-zero reading as
//! an exposure.

use helios_umd_common::hr::{Hresult, E_FAIL, E_INVALIDARG};
use helios_umd_common::refusals::RefusalCounter;
use helios_umd_common::slot::DdiHandle;
use helios_umd_common::throttle::LogThrottle;
// ⚠ Imported for `Interface::cast` — the `QueryInterface` that reaches
// `ID3D12GraphicsCommandList9`. A trait method, so invisible to method
// resolution unless the trait is in scope (same import, same reason, as
// `queue.rs`).
use windows::core::Interface;
use windows::Win32::Foundation::{BOOL, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY, D3D_PRIMITIVE_TOPOLOGY_16_CONTROL_POINT_PATCHLIST,
    D3D_PRIMITIVE_TOPOLOGY_1_CONTROL_POINT_PATCHLIST,
    D3D_PRIMITIVE_TOPOLOGY_32_CONTROL_POINT_PATCHLIST, D3D_PRIMITIVE_TOPOLOGY_LINELIST,
    D3D_PRIMITIVE_TOPOLOGY_LINELIST_ADJ, D3D_PRIMITIVE_TOPOLOGY_LINESTRIP,
    D3D_PRIMITIVE_TOPOLOGY_LINESTRIP_ADJ, D3D_PRIMITIVE_TOPOLOGY_POINTLIST,
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLEFAN, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST_ADJ, D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP_ADJ, D3D_PRIMITIVE_TOPOLOGY_UNDEFINED,
};
use windows::Win32::Graphics::Direct3D12::{
    ID3D12CommandAllocator, ID3D12GraphicsCommandList9, ID3D12Resource, D3D12_COMMAND_LIST_TYPE,
    D3D12_COMMAND_LIST_TYPE_BUNDLE, D3D12_CPU_DESCRIPTOR_HANDLE,
    D3D12_IA_VERTEX_INPUT_RESOURCE_SLOT_COUNT, D3D12_INDEX_BUFFER_STRIP_CUT_VALUE,
    D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_0xFFFF, D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_0xFFFFFFFF,
    D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_DISABLED, D3D12_INDEX_BUFFER_VIEW,
    D3D12_SIMULTANEOUS_RENDER_TARGET_COUNT, D3D12_SO_BUFFER_SLOT_COUNT,
    D3D12_STREAM_OUTPUT_BUFFER_VIEW, D3D12_VERTEX_BUFFER_VIEW,
    D3D12_VIEWPORT, D3D12_VIEWPORT_AND_SCISSORRECT_OBJECT_COUNT_PER_PIPELINE,
};

use super::pso;
use super::queue::{self, CommandListState, RecorderAllocator};
use super::tables12::{stage, CommandListTable, Filling};
use crate::{ddi12, device12, log_error, note_refusal, trace_line};

/// Budget for this lane's list-lifetime lines: the first 8, then every 4096th.
///
/// ⛔ Same discipline and the same scar as `queue.rs`'s six budgets:
/// `log_error!` is unbounded by construction (`umd_common/src/log.rs:279`) and
/// T2 measured one unbounded UMD log site at ~9k mutex-serialized writes per
/// second. A list is reset **once per frame per list**, so an unbudgeted line
/// here is a per-frame writer.
static LIST_LOG: LogThrottle = LogThrottle::new();

/// Budget for the **recording** arms: the first 8, then every 4096th.
///
/// ⛔ A second budget rather than a share of [`LIST_LOG`], for the reason
/// `queue.rs` gives for having six: a burst is a property of the path that
/// produces it. A list is reset once per frame; `pfnRsSetViewports` and
/// `pfnOMSetRenderTargets` run several times per frame *each*, so folding the
/// two families into one countdown would let one bad recording path suppress
/// the lifetime evidence that explains it.
static RECORD_LOG: LogThrottle = LogThrottle::new();

/// Returns the occurrence ordinal (0-based) when the line should be emitted.
fn budget() -> Option<usize> {
    LIST_LOG.first_n_then_every(8, 4096)
}

/// [`budget`] for the recording arms.
fn record_budget() -> Option<usize> {
    RECORD_LOG.first_n_then_every(8, 4096)
}

/// Report a **command-list**-scope failure from a recording slot, counting the
/// two cases where there is no way to hear it.
///
/// # ⛔ This reports through `pfnSetCommandListErrorCb`, NOT `pfnSetErrorCb`
///
/// The first half of the old claim here was true — all 75 command-list slots
/// take `D3D12DDI_HCOMMANDLIST` and nothing else, and 74 of the 75 return `VOID`
/// (`DECISIONS.md` §7.6). ⛔ **The conclusion drawn from it was false.** This
/// file, and two sibling lanes, carried the sentence *"so a recording failure
/// can only be reported through the device-scoped `pfnSetErrorCb`"* into 49 call
/// sites before the `PARALLEL.md` §10 review opened
/// `D3D12DDI_CORELAYER_DEVICECALLBACKS_0062` and found the list-scoped callback
/// **one field below** the device-scoped one — in the same struct this driver
/// already reads `pfnSetCommandListDDITableCb` out of.
///
/// The difference is the whole reason this function was rewritten:
///
/// | callback | what the runtime does |
/// |---|---|
/// | `pfnSetErrorCb` | removes the whole `ID3D12Device` — every list, queue, PSO, heap and resource on it, and the compositor if it is DWM's |
/// | `pfnSetCommandListErrorCb` | *"the runtime will drop all calls into the driver which record commands on the specified command list"* (`tmp/dx12/specs/d3d/CPUEfficiency.md:2143-2158`) — **one** list is quarantined and the application learns at `Close()` |
///
/// ⚠ **The second is D3D12's own recording-error contract**, not a weaker
/// version of the first: a recording error is defined to surface at `Close()`,
/// and this callback is what implements that. Answering a malformed viewport
/// count by removing the device was never a stricter reading of the rule.
///
/// ⛔ The HRESULT is narrowed by `device12::command_list_error_code`, which
/// `device12::set_command_list_error` calls internally — the callback takes only
/// `E_OUTOFMEMORY`, `D3DDDIERR_DEVICEREMOVED` and `D3DDDIERROR_APPLICATIONERROR`
/// — so a call site here passes whatever HRESULT actually describes the failure
/// and the log line at that site is where the detail lives.
///
/// ⚠ Its counters are **this lane's**, not `device12`'s and not L6's, because
/// `PARALLEL.md` §9.1 puts a lane's counters in the lane's file and every lane
/// that reaches `device12::set_command_list_error` will write these same lines.
/// `pso.rs`'s `set_error_if_possible` and `descriptors.rs`'s `report_error` are
/// the same function against those lanes' sets. ⚠ Those two lanes own DEVICE
/// tables, where `pfnSetErrorCb` remains the right callback — there is no list to
/// quarantine. The choice is made by which table the slot is in, not by how bad
/// the failure looks.
///
/// # Safety
/// `state` must be a `CommandListState` borrowed for the duration of the current
/// DDI call — i.e. one [`recording_list`] or `queue::command_list_state` has just
/// returned. The borrow those hand back has an unbounded lifetime, so liveness is
/// a caller obligation rather than something `&CommandListState` proves.
unsafe fn report_error(state: &CommandListState, hr: Hresult) {
    // SAFETY: the caller guarantees `state` is live, and `h_device` is the handle
    // `create_command_list` recorded for it — a device that outlives its lists.
    // The borrow does not outlive this call.
    let Some(dev) = (unsafe { device12::device(state.h_device()) }) else {
        note_refusal(&L3A_REFUSALS.set_error_no_device);
        return;
    };
    if !device12::set_command_list_error(dev, state.h_rt_list(), hr) {
        note_refusal(&L3A_REFUSALS.set_error_cb_absent);
    }
}

/// The state behind a recording slot's one and only argument, counting the
/// handle that did not resolve.
///
/// ⭐ Eighteen of this file's 21 recording slots open with exactly these four
/// lines — the other three (`pfnOMSetDepthBounds`, `pfnSetSamplePositions`,
/// `pfnOmSetAlphaBlendFactor`) refuse outright and never touch the handle.
/// Writing them eighteen times is how the eighteenth ends up without the counter
/// — `queue.rs`'s `lock_target` is the same move for the same reason.
/// ⚠ `pfnExecuteIndirect` joined the eighteen with S-4.
///
/// # Safety
/// As [`queue::command_list_state`]: `h_list` must be a handle
/// `queue::create_command_list` returned `S_OK` for, and the returned reference
/// must not outlive the DDI call that obtained it.
unsafe fn recording_list<'a>(h_list: ddi12::D3D12DDI_HCOMMANDLIST) -> Option<&'a CommandListState> {
    // SAFETY: forwarded unchanged; the caller carries the precondition.
    let state = unsafe { queue::command_list_state(h_list) };
    if state.is_none() {
        note_refusal(&L3A_REFUSALS.command_list_missing);
    }
    state
}

/// The engine list at the `ID3D12GraphicsCommandList9` revision, for the three
/// slots whose API entry points live above the base interface.
///
/// ⭐ **One `cast` covers all three revisions this lane needs.** `…List9`
/// derives from `…List8` derives from `…List1`, so this single `QueryInterface`
/// reaches `OMSetFrontAndBackStencilRef` (8), `RSSetDepthBias` (9) and
/// `IASetIndexBufferStripCutValue` (9). vkd3d answers all eleven
/// `IID_ID3D12GraphicsCommandList*` with the same object
/// (`vkd3d-proton-helios/libs/vkd3d/command.c:6680-6690`), so the QI is a GUID
/// compare plus one interlocked increment that the returned wrapper releases.
///
/// ⚠ Per call rather than cached, and the reason is `PARALLEL.md` §4 rather
/// than performance: the cache's home would be `queue::CommandListState`, which
/// is another lane's file. `resource12::engine_device10` took the same decision
/// for the same reason and recorded it.
///
/// Returns `None` **only** if the engine is not the vkd3d this driver links,
/// which is why the counter's expected reading is 0.
fn engine_list9(state: &CommandListState) -> Option<ID3D12GraphicsCommandList9> {
    match state.engine().cast::<ID3D12GraphicsCommandList9>() {
        Ok(list9) => Some(list9),
        Err(err) => {
            note_refusal(&L3A_REFUSALS.list9_unavailable);
            if let Some(n) = record_budget() {
                log_error!(
                    "L3a: the engine command list does not expose ID3D12GraphicsCommandList9 \
                     hr={:#010x} -- the dynamic depth-bias, strip-cut and front/back stencil-ref \
                     state has been dropped (x{})",
                    err.code().0 as u32,
                    n + 1,
                );
            }
            None
        }
    }
}

// ---------------------------------------------------------------------------
// List lifetime — 2 slots
// ---------------------------------------------------------------------------

/// `pfnCloseCommandList` -> `ID3D12GraphicsCommandList::Close()`.
///
/// ⛔ **A failed `Close` is reported, not merely counted**, and it is the one
/// place in this file where that distinction is not a judgement call: D3D12's
/// contract is that every recording error a driver detected surfaces *here*, at
/// `Close`, and an application that is told `S_OK` will go on to submit a list
/// the engine has already rejected.
///
/// ⭐ **`pfnSetCommandListErrorCb` is the callback that contract is built on**,
/// and this is its textbook case: the runtime drops every later recording call
/// into *this* list and hands the application the failure at `Close`
/// ([`report_error`]). `DDI_REFERENCE.md` §9.12's warning that `pfnSetErrorCb`
/// removes the whole device is why the *caps* refusals in this file stay
/// counted-only — it has never been what a recording report has to cost, and
/// this file no longer pays it.
///
/// ⚠ There is no "already closed" arm. vkd3d answers a redundant `Close` with a
/// failure HRESULT of its own and that lands in the same counter — which is the
/// honest place for it, because this driver keeps no recording flag (see
/// `queue::CommandListState`'s doc for why a forwarder holding a second copy of
/// the engine's state is the thing to avoid).
///
/// # Safety
/// `h_list` must be a handle `queue::create_command_list` returned `S_OK` for and
/// which `pfnDestroyCommandList` has not been called on.
unsafe extern "C" fn close_command_list(h_list: ddi12::D3D12DDI_HCOMMANDLIST) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { queue::command_list_state(h_list) }) else {
        note_refusal(&L3A_REFUSALS.command_list_missing);
        return;
    };
    // SAFETY: `engine()` borrows the list this box owns; `Close` takes no
    // arguments and returns an HRESULT.
    let result = unsafe { state.engine().Close() };
    // ⚠ Traced on BOTH outcomes, and it is the SUCCESS one that was missing: a
    // closed list that never reaches `pfnExecuteCommandLists`, and one that
    // reaches it having recorded nothing, are the two readings
    // `tmp/dx12/gates/G8-r0/RESULT.md` could not separate. `pDrvPrivate` is the
    // join key with the `ExecuteCommandLists` line, which prints the same word
    // per entry.
    trace_line!(
        "CloseCommandList: list={:p} hr={:#010x}",
        h_list.drv_private(),
        result.as_ref().err().map_or(0u32, |e| e.code().0 as u32),
    );
    let Err(err) = result else {
        return;
    };
    let hr = err.code().0;
    note_refusal(&L3A_REFUSALS.close_engine_failed);
    if let Some(n) = budget() {
        log_error!(
            "CloseCommandList: engine Close failed hr={:#010x} (x{})",
            hr as u32,
            n + 1,
        );
    }
    // SAFETY: `state` is the borrow `command_list_state` returned above and does
    // not outlive this call.
    unsafe { report_error(state, hr) };
}

/// `pfnResetCommandList` -> `ID3D12GraphicsCommandList::Reset(allocator, NULL)`.
///
/// # ⭐ Where the allocator comes from, and why it is not in this DDI's args
///
/// `D3D12DDIARG_RESETCOMMANDLIST_0040` is `{ hDrvCommandRecorder, ID,
/// CommandListFlags }` — **no pool and no allocator**. The recorder is the
/// indirection: `pfnCommandRecorderSetCommandPoolAsTarget` bound it to a pool,
/// and that pool's `ID3D12CommandAllocator` is what this call resets against.
/// `queue::recorder_allocator` is that walk, in the lane that owns all three
/// objects (`PARALLEL.md` §4), and it hands back an **owned** reference so this
/// call cannot touch a pool the runtime has since destroyed.
///
/// ⚠ **`pInitialState` is `NULL`, and that is the DDI's shape rather than a
/// simplification.** The API's `Reset(pAllocator, pInitialState)` takes an
/// optional PSO; this DDI carries none, because the runtime lowers the
/// application's initial state as a separate `pfnSetPipelineState` immediately
/// after. Passing `None` is what a driver with no PSO in its arguments can say.
///
/// # ⛔ The bundle question, ANSWERED — and the answer is that bundles do not
/// work on this driver yet
///
/// `queue.rs`'s module doc records it as UNVERIFIED and routes it here: a
/// `D3D12_COMMAND_LIST_TYPE_BUNDLE` list can only be `Reset` against a **BUNDLE**
/// allocator, `ID3D12CommandAllocator`'s class is fixed at creation, and
/// `D3D12DDIARG_CREATE_COMMAND_RECORDER_0040` is `{ QueueFlags, RecorderFlags }`
/// (`d3d12umddi.rs:66130-66133`) — 3D / COMPUTE / COPY / PAGING / video and a
/// flags word whose only enumerator is `NONE`, so **no bundle bit anywhere**.
/// The recorder behind a bundle is indistinguishable from one behind a DIRECT
/// list, and `queue.rs` materialises a DIRECT allocator for it.
///
/// ⛔ **The engine settles the rest without a run, and the mismatch is therefore
/// refused here rather than forwarded.** Both halves of vkd3d enforce the class
/// pairing unconditionally:
///
/// * a regular list — `d3d12_command_list_Reset` opens with
///   `if (!allocator_impl || allocator_impl->type != list->type) return
///   E_INVALIDARG;` (`vkd3d-proton-helios/libs/vkd3d/command.c:7378-7382`);
/// * a bundle — `d3d12_bundle_Reset` resolves the allocator through
///   `d3d12_bundle_allocator_from_iface`, which returns NULL for any allocator
///   whose `lpVtbl` is not `d3d12_bundle_allocator_vtbl`, and answers
///   `E_INVALIDARG` (`bundle.c:239-245`, `bundle.c:411-427`).
///
/// So forwarding a mismatch is a call whose failure is known in advance: it
/// buys no measurement, and it charges the failure to `ResetEngineFailed`, which
/// is graded **Expected 0** and exists to catch the *unforeseen* failure. The
/// arm now counts, logs, reports and returns.
///
/// ⚠ **What this does NOT fix: bundles still cannot record.** A refused `Reset`
/// leaves `d3d12_bundle::allocator` NULL, and every bundle recording method goes
/// through `d3d12_bundle_add_command`, which dereferences it with no null test
/// (`bundle.c:253-267` into `bundle.c:29-57`). That is why this arm **reports**
/// rather than merely counting: `pfnResetCommandList` returns `VOID`, so the
/// error callback is the only way to tell the runtime that the list it is about
/// to record into is not usable, and a silent count would hand the application a
/// null dereference inside its own process instead. This is a real correctness
/// failure of this driver, not a capability it declines to advertise, which is
/// the distinction `DDI_REFERENCE.md` §9.12 draws for the callback.
///
/// ⭐ **And the callback is `pfnSetCommandListErrorCb`, which fits this failure
/// exactly** — see [`report_error`]. The runtime's documented response is to
/// *"drop all calls into the driver which record commands on the specified
/// command list"*, i.e. precisely the calls that would otherwise reach
/// `d3d12_bundle_add_command` with a NULL allocator. The device-scoped
/// `pfnSetErrorCb` this arm used to call also prevented the dereference, by
/// destroying the whole `ID3D12Device` the unusable list belonged to.
///
/// ⛔ **The fix is one allocator per (pool, class) in `queue.rs`** — the same fix
/// `queue.rs`'s `PoolTypeMismatch` already names, arrived at from the other
/// direction — i.e. `CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_BUNDLE)` on
/// first use by a bundle list. It cannot be done from this file: the bundle
/// holds its allocator as a **raw, un-`AddRef`ed** pointer (`bundle.c:436`), so
/// the allocator must outlive the bundle, and the only object with that lifetime
/// is the pool — which lives in `queue.rs`, another lane's file
/// (`PARALLEL.md` §4). A side table keyed by list handle here would have no
/// destroy hook, because `pfnDestroyCommandList` is `queue.rs`'s too.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`; `arg`, when
/// non-null, must point at a live `D3D12DDIARG_RESETCOMMANDLIST_0040` whose
/// `hDrvCommandRecorder` is a live handle from `queue::create_command_recorder`.
unsafe extern "C" fn reset_command_list(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    arg: *const ddi12::D3D12DDIARG_RESETCOMMANDLIST_0040,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { queue::command_list_state(h_list) }) else {
        note_refusal(&L3A_REFUSALS.command_list_missing);
        return;
    };
    if arg.is_null() {
        note_refusal(&L3A_REFUSALS.reset_bad_arg);
        // SAFETY: `state` is live for this DDI call, as `close_command_list`.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    }
    // SAFETY: non-null per the check; the DDI declares it `_In_ CONST`.
    let a = unsafe { &*arg };

    // ⚠ Dropped and counted, not refused — the same two marker hints
    // (`ENABLE_MARKERS`, `_0010_ENABLE_FULLPIPELINE_MARKERS`) that
    // `queue::create_command_list` drops, arriving a second time at reset. The
    // API's `D3D12_COMMAND_LIST_FLAGS` defines only `NONE`, so there is nothing
    // to forward them to.
    if a.CommandListFlags != ddi12::D3D12DDI_COMMAND_LIST_FLAGS_D3D12DDI_COMMAND_LIST_FLAG_NONE {
        // ⛔ `bump`, not `note_refusal`, and the reason is the counter's own
        // grading: it is *expected non-zero under a debug layer or a PIX
        // capture*, which is exactly the configuration triage runs in.
        // `note_refusal` prints the whole `D3D12 DDI refusals:` set on a
        // counter's first hit, so the old form wrote a full refusal record on
        // the FIRST reset of every debug-layer run — for two marker bits this
        // driver knowingly tolerates. Same hazard the depth-bounds and
        // stream-output splits were made to remove, in the one arm that had it
        // by construction rather than by accident.
        L3A_REFUSALS.reset_flags_ignored.bump();
        let n = L3A_REFUSALS.reset_flags_ignored.get();
        if n <= 8 {
            log_error!(
                "ResetCommandList: CommandListFlags={:#x} carries marker hints the API enum has \
                 no counterpart for; dropped (x{n})",
                a.CommandListFlags,
            );
        }
    }

    // SAFETY: the caller guarantees `hDrvCommandRecorder` is a live recorder
    // handle; the returned allocator is owned by this call.
    let (allocator, allocator_type): (ID3D12CommandAllocator, D3D12_COMMAND_LIST_TYPE) =
        match unsafe { queue::recorder_allocator(a.hDrvCommandRecorder) } {
            RecorderAllocator::Ready {
                allocator,
                list_type,
            } => (allocator, list_type),
            RecorderAllocator::NoRecorder => {
                note_refusal(&L3A_REFUSALS.reset_recorder_missing);
                // SAFETY: `state` is live for this DDI call, as above.
                unsafe { report_error(state, E_INVALIDARG) };
                return;
            }
            RecorderAllocator::NoPoolBound => {
                note_refusal(&L3A_REFUSALS.reset_no_allocator);
                if let Some(n) = budget() {
                    log_error!(
                        "ResetCommandList: the recorder has never been bound to a command pool, \
                         so there is no ID3D12CommandAllocator to reset against (x{})",
                        n + 1,
                    );
                }
                // SAFETY: `state` is live for this DDI call, as above.
                unsafe { report_error(state, E_FAIL) };
                return;
            }
        };

    // ⛔ The class check, and it REFUSES rather than falling through. See the doc
    // above: both `d3d12_command_list_Reset` (command.c:7378-7382) and
    // `d3d12_bundle_Reset` (bundle.c:411-427) reject a mismatched allocator
    // unconditionally, so forwarding is a call whose E_INVALIDARG is known in
    // advance and whose only effect is to charge `ResetEngineFailed` — a counter
    // graded Expected 0 for the failures nobody predicted.
    //
    // ⚠ The allocator's class is **carried** rather than asked for:
    // `ID3D12CommandAllocator` exposes no `GetType`, which is why
    // `queue::PoolAllocator` pairs the two.
    if allocator_type != state.list_type() {
        note_refusal(&L3A_REFUSALS.reset_list_type_mismatch);
        if let Some(n) = budget() {
            log_error!(
                "ResetCommandList: this list is class {} and its recorder's allocator is class \
                 {} -- the engine rejects a mismatched allocator unconditionally, so the Reset \
                 is refused here rather than forwarded. For a BUNDLE list this is structural: \
                 D3D12DDIARG_CREATE_COMMAND_RECORDER_0040 carries no bundle bit, so this driver \
                 never mints a BUNDLE allocator, and the fix is one allocator per (pool, class) \
                 in the queue lane (x{})",
                state.list_type().0,
                allocator_type.0,
                n + 1,
            );
        }
        // SAFETY: `state` is live for this DDI call, as above. ⛔ Reported, not merely
        // counted: the list is left unreset, and recording into an unreset
        // bundle dereferences a NULL allocator inside the engine
        // (bundle.c:253-267), so the runtime has to be told the list is unusable.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    }

    trace_line!(
        "ResetCommandList: list={:p} type={} id={} allocatorType={}",
        h_list.pDrvPrivate,
        state.list_type().0,
        a.ID,
        allocator_type.0,
    );

    // SAFETY: `allocator` is an owned reference live for this call, `engine()`
    // borrows the list this box owns, and `None` is the DDI's own answer for the
    // initial pipeline state — it carries none.
    let Err(err) = (unsafe { state.engine().Reset(&allocator, None) }) else {
        return;
    };
    let hr = err.code().0;
    note_refusal(&L3A_REFUSALS.reset_engine_failed);
    if let Some(n) = budget() {
        log_error!(
            "ResetCommandList: engine Reset failed hr={:#010x} listType={} allocatorType={} \
             (x{})",
            hr as u32,
            state.list_type().0,
            allocator_type.0,
            n + 1,
        );
    }
    // SAFETY: `state` is live for this DDI call, as above.
    unsafe { report_error(state, hr) };
}

// ---------------------------------------------------------------------------
// Draw — 3 slots
// ---------------------------------------------------------------------------
//
// ⚠ **No `trace_line!` on these three, deliberately.** `trace_line!` skips its
// `format!` when the knob is off, but it is still a branch on the hottest three
// entry points in the whole DDI, and R420 is the standing record of what putting
// logging on a draw path costs. Every argument here is a scalar the engine
// records verbatim, so a trace would add nothing a PIX capture does not already
// have.

/// `pfnDrawInstanced` -> `ID3D12GraphicsCommandList::DrawInstanced`.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`.
unsafe extern "C" fn draw_instanced(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    vertex_count_per_instance: ddi12::UINT,
    instance_count: ddi12::UINT,
    start_vertex_location: ddi12::UINT,
    start_instance_location: ddi12::UINT,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    // SAFETY: `engine()` borrows the list this box owns; all four arguments are
    // by-value `UINT`s the engine records without dereferencing.
    unsafe {
        state.engine().DrawInstanced(
            vertex_count_per_instance,
            instance_count,
            start_vertex_location,
            start_instance_location,
        );
    }
}

/// `pfnDrawIndexedInstanced` -> `ID3D12GraphicsCommandList::DrawIndexedInstanced`.
///
/// ⚠ `BaseVertexLocation` is `INT`, not `UINT`, on **both** sides — it is a
/// signed offset added to every index. The DDI typedef and the API entry point
/// agree, so it crosses unconverted; a `UINT` here would silently turn every
/// negative base vertex into a ~4-billion-element offset.
///
/// # Safety
/// As [`draw_instanced`].
unsafe extern "C" fn draw_indexed_instanced(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    index_count_per_instance: ddi12::UINT,
    instance_count: ddi12::UINT,
    start_index_location: ddi12::UINT,
    base_vertex_location: ddi12::INT,
    start_instance_location: ddi12::UINT,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    // SAFETY: as `draw_instanced`; five by-value scalars.
    unsafe {
        state.engine().DrawIndexedInstanced(
            index_count_per_instance,
            instance_count,
            start_index_location,
            base_vertex_location,
            start_instance_location,
        );
    }
}

/// `pfnDispatch` -> `ID3D12GraphicsCommandList::Dispatch`.
///
/// # Safety
/// As [`draw_instanced`].
unsafe extern "C" fn dispatch(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    thread_group_count_x: ddi12::UINT,
    thread_group_count_y: ddi12::UINT,
    thread_group_count_z: ddi12::UINT,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    // SAFETY: as `draw_instanced`; three by-value scalars.
    unsafe {
        state
            .engine()
            .Dispatch(thread_group_count_x, thread_group_count_y, thread_group_count_z);
    }
}

// ---------------------------------------------------------------------------
// ⭐ The array-element layout proofs
// ---------------------------------------------------------------------------
//
// Six of this lane's slots take a `D3D12DDI_*` struct by pointer where the API
// entry point takes its `D3D12_*` twin — five of them as an ARRAY. Both sides are
// machine-generated — bindgen from `d3d12umddi.h`, windows-rs from the Win32
// metadata — so `ARCHITECTURE.md` §12 rule 1 is satisfied by *comparing the two
// generators* rather than by trusting either. That is exactly the argument
// `descriptors.rs`'s `D3D12DDI_CPU_DESCRIPTOR_HANDLE` section makes for
// `pfnCopyDescriptors`, and these assertions are its siblings.
//
// ⭐ **Why cast the array instead of building a `Vec`.** These five run several
// times per frame per command list, and an element-wise copy would be a heap
// allocation on a recording path for a struct that is already bit-identical.
// `queue.rs::execute_command_lists` copies instead — and says why: there the
// elements are COM *references* whose ownership differs between the two sides,
// which is a semantic difference and not a layout one. Here there is no
// ownership, only bytes.
//
// ⚠ Size, alignment and every field offset, per struct — for a `#[repr(C)]`
// struct that is the whole of its layout. The field NAMES are checked too, by
// `offset_of!` failing to compile on a name that does not exist.

const _: () = assert!(
    core::mem::size_of::<ddi12::D3D12DDI_VIEWPORT>() == core::mem::size_of::<D3D12_VIEWPORT>()
        && core::mem::align_of::<ddi12::D3D12DDI_VIEWPORT>()
            == core::mem::align_of::<D3D12_VIEWPORT>()
        && core::mem::offset_of!(ddi12::D3D12DDI_VIEWPORT, TopLeftX)
            == core::mem::offset_of!(D3D12_VIEWPORT, TopLeftX)
        && core::mem::offset_of!(ddi12::D3D12DDI_VIEWPORT, TopLeftY)
            == core::mem::offset_of!(D3D12_VIEWPORT, TopLeftY)
        && core::mem::offset_of!(ddi12::D3D12DDI_VIEWPORT, Width)
            == core::mem::offset_of!(D3D12_VIEWPORT, Width)
        && core::mem::offset_of!(ddi12::D3D12DDI_VIEWPORT, Height)
            == core::mem::offset_of!(D3D12_VIEWPORT, Height)
        && core::mem::offset_of!(ddi12::D3D12DDI_VIEWPORT, MinDepth)
            == core::mem::offset_of!(D3D12_VIEWPORT, MinDepth)
        && core::mem::offset_of!(ddi12::D3D12DDI_VIEWPORT, MaxDepth)
            == core::mem::offset_of!(D3D12_VIEWPORT, MaxDepth),
    "D3D12DDI_VIEWPORT must be layout-identical to D3D12_VIEWPORT"
);

// ⚠ `D3D12DDI_RECT` is bindgen's `tagRECT`; the API side is windows-rs' `RECT`,
// which is what `D3D12_RECT` is a type alias for. Two generators, one C struct.
const _: () = assert!(
    core::mem::size_of::<ddi12::D3D12DDI_RECT>() == core::mem::size_of::<RECT>()
        && core::mem::align_of::<ddi12::D3D12DDI_RECT>() == core::mem::align_of::<RECT>()
        && core::mem::offset_of!(ddi12::D3D12DDI_RECT, left) == core::mem::offset_of!(RECT, left)
        && core::mem::offset_of!(ddi12::D3D12DDI_RECT, top) == core::mem::offset_of!(RECT, top)
        && core::mem::offset_of!(ddi12::D3D12DDI_RECT, right) == core::mem::offset_of!(RECT, right)
        && core::mem::offset_of!(ddi12::D3D12DDI_RECT, bottom)
            == core::mem::offset_of!(RECT, bottom),
    "D3D12DDI_RECT must be layout-identical to D3D12_RECT"
);

const _: () = assert!(
    core::mem::size_of::<ddi12::D3D12DDI_VERTEX_BUFFER_VIEW>()
        == core::mem::size_of::<D3D12_VERTEX_BUFFER_VIEW>()
        && core::mem::align_of::<ddi12::D3D12DDI_VERTEX_BUFFER_VIEW>()
            == core::mem::align_of::<D3D12_VERTEX_BUFFER_VIEW>()
        && core::mem::offset_of!(ddi12::D3D12DDI_VERTEX_BUFFER_VIEW, BufferLocation)
            == core::mem::offset_of!(D3D12_VERTEX_BUFFER_VIEW, BufferLocation)
        && core::mem::offset_of!(ddi12::D3D12DDI_VERTEX_BUFFER_VIEW, SizeInBytes)
            == core::mem::offset_of!(D3D12_VERTEX_BUFFER_VIEW, SizeInBytes)
        && core::mem::offset_of!(ddi12::D3D12DDI_VERTEX_BUFFER_VIEW, StrideInBytes)
            == core::mem::offset_of!(D3D12_VERTEX_BUFFER_VIEW, StrideInBytes),
    "D3D12DDI_VERTEX_BUFFER_VIEW must be layout-identical to D3D12_VERTEX_BUFFER_VIEW"
);

const _: () = assert!(
    core::mem::size_of::<ddi12::D3D12DDI_STREAM_OUTPUT_BUFFER_VIEW>()
        == core::mem::size_of::<D3D12_STREAM_OUTPUT_BUFFER_VIEW>()
        && core::mem::align_of::<ddi12::D3D12DDI_STREAM_OUTPUT_BUFFER_VIEW>()
            == core::mem::align_of::<D3D12_STREAM_OUTPUT_BUFFER_VIEW>()
        && core::mem::offset_of!(ddi12::D3D12DDI_STREAM_OUTPUT_BUFFER_VIEW, BufferLocation)
            == core::mem::offset_of!(D3D12_STREAM_OUTPUT_BUFFER_VIEW, BufferLocation)
        && core::mem::offset_of!(ddi12::D3D12DDI_STREAM_OUTPUT_BUFFER_VIEW, SizeInBytes)
            == core::mem::offset_of!(D3D12_STREAM_OUTPUT_BUFFER_VIEW, SizeInBytes)
        && core::mem::offset_of!(
            ddi12::D3D12DDI_STREAM_OUTPUT_BUFFER_VIEW,
            BufferFilledSizeLocation
        ) == core::mem::offset_of!(D3D12_STREAM_OUTPUT_BUFFER_VIEW, BufferFilledSizeLocation),
    "D3D12DDI_STREAM_OUTPUT_BUFFER_VIEW must be layout-identical to its API twin"
);

// ⚠ The index-buffer view crosses by POINTER rather than as an array, but the
// same proof is what makes the pointer cast lossless — and it is the one struct
// here with a `DXGI_FORMAT` field, which bindgen renders as `c_int` and
// windows-rs as a newtype over `i32`.
const _: () = assert!(
    core::mem::size_of::<ddi12::D3D12DDI_INDEX_BUFFER_VIEW>()
        == core::mem::size_of::<D3D12_INDEX_BUFFER_VIEW>()
        && core::mem::align_of::<ddi12::D3D12DDI_INDEX_BUFFER_VIEW>()
            == core::mem::align_of::<D3D12_INDEX_BUFFER_VIEW>()
        && core::mem::offset_of!(ddi12::D3D12DDI_INDEX_BUFFER_VIEW, BufferLocation)
            == core::mem::offset_of!(D3D12_INDEX_BUFFER_VIEW, BufferLocation)
        && core::mem::offset_of!(ddi12::D3D12DDI_INDEX_BUFFER_VIEW, SizeInBytes)
            == core::mem::offset_of!(D3D12_INDEX_BUFFER_VIEW, SizeInBytes)
        && core::mem::offset_of!(ddi12::D3D12DDI_INDEX_BUFFER_VIEW, Format)
            == core::mem::offset_of!(D3D12_INDEX_BUFFER_VIEW, Format),
    "D3D12DDI_INDEX_BUFFER_VIEW must be layout-identical to D3D12_INDEX_BUFFER_VIEW"
);

// ⚠ Re-proved here rather than borrowed from `descriptors.rs`: that file's
// assertions are `const _` items private to it, and `pfnOMSetRenderTargets`
// casts the same array in THIS file. A proof that lives one file away from the
// cast it licenses is a proof nobody re-checks when the cast moves.
const _: () = assert!(
    core::mem::size_of::<ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE>()
        == core::mem::size_of::<D3D12_CPU_DESCRIPTOR_HANDLE>()
        && core::mem::align_of::<ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE>()
            == core::mem::align_of::<D3D12_CPU_DESCRIPTOR_HANDLE>()
        && core::mem::offset_of!(ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE, ptr)
            == core::mem::offset_of!(D3D12_CPU_DESCRIPTOR_HANDLE, ptr),
    "D3D12DDI_CPU_DESCRIPTOR_HANDLE must be layout-identical to D3D12_CPU_DESCRIPTOR_HANDLE"
);

// ---------------------------------------------------------------------------
// ⭐ The primitive-topology identity proof, and the triangle-fan obligation
// ---------------------------------------------------------------------------
//
// `D3D12DDI_PRIMITIVE_TOPOLOGY` (a bindgen `c_int`) and `D3D_PRIMITIVE_TOPOLOGY`
// (a windows-rs newtype over `i32`) are value-identical — but the enumeration is
// NOT contiguous: it runs `0..=6`, then `10..=13`, then `33..=64`, with two real
// holes. So [`api_topology`] is a range check whose six bounds are the GENERATED
// DDI constants, and the assertions below pin each of those constants to its
// API twin. ⛔ Not one representative: a bound that is wrong makes the check
// itself wrong, so every bound is proved, plus every interior value of the two
// short runs and the midpoint of the long one.
//
// ⭐⭐ **`TRIANGLEFAN` (6) is the one that matters, and it is the reason this is a
// proof rather than a cast.** `SUBSTRATE.md` §4.5: DDI 0097 revived it and made
// it **mandatory at 0097+ with no cap**, so negotiating `_0110` signs this driver
// up for it and there is nothing to decline. The forwarding chain is intact end
// to end and was checked rather than assumed:
//
//   * the API enumerator exists — `D3D_PRIMITIVE_TOPOLOGY_TRIANGLEFAN = 6`
//     (`windows-0.58.0/.../Direct3D/mod.rs:539`), same value as the DDI's;
//   * vkd3d translates it — `vk_topology_from_d3d12_topology` maps
//     `D3D_PRIMITIVE_TOPOLOGY_TRIANGLEFAN` to `VK_PRIMITIVE_TOPOLOGY_TRIANGLE_FAN`
//     (`vkd3d-proton-helios/libs/vkd3d/state.c:4332-4333`), with no cap check and
//     no `FIXME` arm;
//   * the substrate has it — triangle fan is CORE Vulkan 1.0 and is withdrawn
//     only by `VK_KHR_portability_subset::triangleFans`, which this host does not
//     expose (`docs/reference/host-vulkan-profile-rtx-pro-6000-blackwell.json`
//     contains no portability-subset entry at all).
//
// ⇒ nothing is emulated and nothing is refused; the DDI's 6 becomes Vulkan's
// `TRIANGLE_FAN`. [`L3A_REFUSALS.topology_triangle_fan`] counts the uses anyway,
// because "the mandatory 0097 obligation was exercised and did not fault" is a
// measurement `SUBSTRATE.md` §4.5 asks for and a zero reading would leave
// untaken. ⚠ It is NOT a refusal counter.

const _: () = assert!(
    ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_UNDEFINED
        == D3D_PRIMITIVE_TOPOLOGY_UNDEFINED.0
        && ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_POINTLIST
            == D3D_PRIMITIVE_TOPOLOGY_POINTLIST.0
        && ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_LINELIST
            == D3D_PRIMITIVE_TOPOLOGY_LINELIST.0
        && ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_LINESTRIP
            == D3D_PRIMITIVE_TOPOLOGY_LINESTRIP.0
        && ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_TRIANGLELIST
            == D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST.0
        && ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP
            == D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP.0
        && ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_TRIANGLEFAN
            == D3D_PRIMITIVE_TOPOLOGY_TRIANGLEFAN.0,
    "D3D12DDI_PRIMITIVE_TOPOLOGY's 0..=6 run must be value-identical to D3D_PRIMITIVE_TOPOLOGY's"
);
const _: () = assert!(
    ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_LINELIST_ADJ
        == D3D_PRIMITIVE_TOPOLOGY_LINELIST_ADJ.0
        && ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_LINESTRIP_ADJ
            == D3D_PRIMITIVE_TOPOLOGY_LINESTRIP_ADJ.0
        && ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_TRIANGLELIST_ADJ
            == D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST_ADJ.0
        && ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP_ADJ
            == D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP_ADJ.0,
    "the adjacency run must be value-identical"
);
const _: () = assert!(
    ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_1_CONTROL_POINT_PATCHLIST
        == D3D_PRIMITIVE_TOPOLOGY_1_CONTROL_POINT_PATCHLIST.0
        && ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_16_CONTROL_POINT_PATCHLIST
            == D3D_PRIMITIVE_TOPOLOGY_16_CONTROL_POINT_PATCHLIST.0
        && ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_32_CONTROL_POINT_PATCHLIST
            == D3D_PRIMITIVE_TOPOLOGY_32_CONTROL_POINT_PATCHLIST.0,
    "the patchlist run must be value-identical at both ends and in the middle"
);

/// Translate a DDI topology into the API's, or `None` for a value outside the
/// three runs this enumeration actually defines.
///
/// ⛔ Range-checked rather than cast, and the engine's real behaviour is why. A
/// blind cast of, say, 7 or 20 — the two holes — reaches vkd3d's
/// `vk_topology_from_d3d12_topology` default arm, which is
/// `FIXME("Unhandled primitive topology %#x.\n"); return
/// VK_PRIMITIVE_TOPOLOGY_POINT_LIST;` (`state.c:4375-4377`). ⚠ That is
/// **graceful, and that is the problem**: the value is silently clamped, the
/// draw succeeds, and the application gets points where it asked for something
/// else with nothing anywhere saying so. Six integer comparisons on a per-frame
/// path turn fake success into a counter.
///
/// ⚠ `VK_PRIMITIVE_TOPOLOGY_MAX_ENUM` is **not** what happens here. It is the
/// default arm of `vk_topology_type_from_d3d12_topology_type` (`state.c:4312-4314`),
/// a different function taking `D3D12_PRIMITIVE_TOPOLOGY_TYPE`; an earlier
/// version of this comment cited it for this path.
fn api_topology(t: ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY) -> Option<D3D_PRIMITIVE_TOPOLOGY> {
    use ddi12::{
        D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_1_CONTROL_POINT_PATCHLIST as PATCH_1,
        D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_32_CONTROL_POINT_PATCHLIST as PATCH_32,
        D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_LINELIST_ADJ as LINELIST_ADJ,
        D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_TRIANGLEFAN as TRIANGLEFAN,
        D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP_ADJ as TRISTRIP_ADJ,
        D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_UNDEFINED as UNDEFINED,
    };
    let known = (UNDEFINED..=TRIANGLEFAN).contains(&t)
        || (LINELIST_ADJ..=TRISTRIP_ADJ).contains(&t)
        || (PATCH_1..=PATCH_32).contains(&t);
    known.then_some(D3D_PRIMITIVE_TOPOLOGY(t))
}

// ---------------------------------------------------------------------------
// Fixed-function state — 11 slots
// ---------------------------------------------------------------------------

/// The blend factor the API documents for a NULL `pBlendFactor`.
///
/// ⛔ Substituted here rather than forwarded as NULL, and this is not defensive
/// programming: `d3d12_command_list_OMSetBlendFactor` opens with an
/// unconditional `memcmp(dyn_state->blend_constants, blend_factor, 16)`
/// (`vkd3d-proton-helios/libs/vkd3d/command.c:12538-12550`), so a NULL reaches
/// the engine as a null dereference inside the compositor's process. D3D12
/// documents NULL as *"the runtime uses or stores a blend factor equal to
/// {1,1,1,1}"*, so the substitution is the documented value and not a guess.
const DEFAULT_BLEND_FACTOR: [f32; 4] = [1.0, 1.0, 1.0, 1.0];

/// The D3D12 limit on `pfnRsSetViewports` / `pfnRsSetScissorRects`, read from
/// the API's own generated constant rather than written as 16.
const MAX_VIEWPORTS_AND_SCISSORS: usize =
    D3D12_VIEWPORT_AND_SCISSORRECT_OBJECT_COUNT_PER_PIPELINE as usize;

/// The depth-bounds range D3D12 defines as the default, and therefore the exact
/// value the runtime's per-reset state block restores.
///
/// ⛔ Named rather than written inline at the one comparison, because the two
/// halves of [`om_set_depth_bounds`]'s partition are only correct *together*:
/// this is what makes "the runtime's unconditional default write" and "an
/// application asking for depth bounds" separable at all. `DepthBoundsTest.md`
/// states it for the API — *"The default values are 0 and 1 for the Min and Max,
/// respectively"* — and again, verbatim, for the DDI. ⚠ There is no generated
/// constant for it on either side; the spec text is the whole authority, which is
/// why it is quoted here rather than cited.
const DEPTH_BOUNDS_DEFAULT_MIN: ddi12::FLOAT = 0.0;
/// The upper half of [`DEPTH_BOUNDS_DEFAULT_MIN`]'s range.
const DEPTH_BOUNDS_DEFAULT_MAX: ddi12::FLOAT = 1.0;

/// `pfnIaSetTopology` -> `ID3D12GraphicsCommandList::IASetPrimitiveTopology`.
///
/// See the identity proof above for why the value is range-checked and for the
/// triangle-fan chain. ⚠ `UNDEFINED` is forwarded rather than dropped: vkd3d
/// answers it with a `WARN` and an early return (`command.c:12451-12455`), which
/// is the engine's decision to make and not this driver's to pre-empt.
///
/// ⛔ **An out-of-range value is dropped AND reported**, unlike the caps
/// refusals in this file, and the engine's graceful answer is the reason rather
/// than an objection to it: `vk_topology_from_d3d12_topology` clamps an unknown
/// enumerator to `VK_PRIMITIVE_TOPOLOGY_POINT_LIST` with a `FIXME`
/// (`state.c:4375-4377`), so forwarding would draw the wrong primitive and
/// return success. The enumeration is closed at `_0110` and every one of its run
/// bounds is pinned to the API's generated constant above, so a value outside it
/// is a malformed call in the same class as `pfnRsSetViewports`' non-zero count
/// with a null array — which this file also reports.
///
/// # Safety
/// As [`draw_instanced`].
unsafe extern "C" fn ia_set_topology(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    topology: ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    let Some(api) = api_topology(topology) else {
        note_refusal(&L3A_REFUSALS.topology_unknown);
        if let Some(n) = record_budget() {
            log_error!(
                "IaSetTopology: {} is outside the three runs D3D12DDI_PRIMITIVE_TOPOLOGY defines \
                 (0..=6, 10..=13, 33..=64) -- dropped rather than forwarded, because the engine \
                 would clamp it to VK_PRIMITIVE_TOPOLOGY_POINT_LIST and draw points with no \
                 error (x{})",
                topology,
                n + 1,
            );
        }
        // SAFETY: `state` is the borrow `recording_list` returned above and does
        // not outlive this call.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };
    if topology == ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY_D3D12DDI_PRIMITIVE_TOPOLOGY_TRIANGLEFAN {
        // ⚠ Not a refusal — the instrument for `SUBSTRATE.md` §4.5's mandatory
        // 0097 obligation. ⛔ `bump`, not `note_refusal`: `note_refusal` prints
        // the whole `D3D12 DDI refusals:` set on a counter's first hit, and this
        // arm FORWARDS — a successful triangle fan must not look like a refusal
        // in the log. Same shape as `descriptors.rs`'s `sampler_creates`.
        L3A_REFUSALS.topology_triangle_fan.bump();
    }
    // SAFETY: `engine()` borrows the list this box owns; the topology is a
    // by-value enumerator this function has just proved is in range.
    unsafe { state.engine().IASetPrimitiveTopology(api) };
}

/// `pfnRsSetViewports` -> `ID3D12GraphicsCommandList::RSSetViewports`.
///
/// ⛔ The count and the pointer are validated **before** the array is read, and
/// per arm: a zero count with a null pointer is the legal "unbind every
/// viewport", a non-zero count with a null pointer never is.
///
/// ⚠ A count above `D3D12_VIEWPORT_AND_SCISSORRECT_OBJECT_COUNT_PER_PIPELINE`
/// is refused whole rather than clamped. D3D12 forbids it, so the array behind
/// it is not one this driver should read any part of; vkd3d clamps with a
/// `FIXME_ONCE` and a clamp here would only hide that the runtime asked.
///
/// # Safety
/// As [`draw_instanced`], and `viewports` must address at least `count`
/// readable `D3D12DDI_VIEWPORT`s for the duration of the call.
unsafe extern "C" fn rs_set_viewports(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    count: ddi12::UINT,
    viewports: *const ddi12::D3D12DDI_VIEWPORT,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    let n = count as usize;
    if n > MAX_VIEWPORTS_AND_SCISSORS || (n != 0 && viewports.is_null()) {
        note_refusal(&L3A_REFUSALS.viewports_bad_arg);
        if let Some(k) = record_budget() {
            log_error!(
                "RsSetViewports: Count={count} pViewports={viewports:p} -- refused (x{})",
                k + 1,
            );
        }
        // SAFETY: `state` is live for this DDI call, as `ia_set_topology`.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    }
    // ⚠ `&[]` for the empty case rather than `from_raw_parts(null, 0)`, which is
    // undefined even at length 0 — the pointer must still be non-null and
    // aligned.
    let views: &[D3D12_VIEWPORT] = if n == 0 {
        &[]
    } else {
        // SAFETY: non-null and bounded per the check above, and the caller
        // guarantees `count` readable elements. The two structs are
        // layout-identical by the assertion above, so this reinterprets an array
        // of one C type as an array of the same C type.
        unsafe { core::slice::from_raw_parts(viewports.cast::<D3D12_VIEWPORT>(), n) }
    };
    trace_line!("RsSetViewports: n={n}");
    // SAFETY: `views` is a live slice for the whole call; the wrapper's
    // `len().try_into()` cannot fail because `n <= 16`.
    unsafe { state.engine().RSSetViewports(views) };
}

/// `pfnRsSetScissorRects` -> `ID3D12GraphicsCommandList::RSSetScissorRects`.
///
/// # Safety
/// As [`rs_set_viewports`], for `D3D12DDI_RECT`s.
unsafe extern "C" fn rs_set_scissor_rects(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    count: ddi12::UINT,
    rects: *const ddi12::D3D12DDI_RECT,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    let n = count as usize;
    if n > MAX_VIEWPORTS_AND_SCISSORS || (n != 0 && rects.is_null()) {
        note_refusal(&L3A_REFUSALS.scissor_rects_bad_arg);
        if let Some(k) = record_budget() {
            log_error!(
                "RsSetScissorRects: Count={count} pRects={rects:p} -- refused (x{})",
                k + 1,
            );
        }
        // SAFETY: `state` is live for this DDI call, as `ia_set_topology`.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    }
    let api_rects: &[RECT] = if n == 0 {
        &[]
    } else {
        // SAFETY: as `rs_set_viewports`, against the `D3D12DDI_RECT` assertion.
        unsafe { core::slice::from_raw_parts(rects.cast::<RECT>(), n) }
    };
    trace_line!("RsSetScissorRects: n={n}");
    // SAFETY: as `rs_set_viewports`.
    unsafe { state.engine().RSSetScissorRects(api_rects) };
}

/// `pfnOmSetBlendFactor` -> `ID3D12GraphicsCommandList::OMSetBlendFactor`.
///
/// ⛔ A NULL pointer becomes [`DEFAULT_BLEND_FACTOR`] rather than a forwarded
/// NULL — read that constant's doc, the engine would dereference it.
///
/// # Safety
/// As [`draw_instanced`], and `factor`, when non-null, must address four
/// readable `FLOAT`s — which is what the DDI's `const FLOAT[4]` declares.
unsafe extern "C" fn om_set_blend_factor(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    factor: *const ddi12::FLOAT,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    let value: &[f32; 4] = if factor.is_null() {
        // ⚠ Not a refusal, and ⛔ `bump` rather than `note_refusal` for the same
        // reason as the triangle-fan and `SoTargetsNullArray` arms: this arm
        // FORWARDS the value D3D12 documents for a NULL, so nothing was refused
        // and it must not print the `D3D12 DDI refusals:` set. ⚠ This slot is in
        // the runtime's own per-reset state block too — `D-triangle.log:301`
        // reads `cl[27] pfnOmSetBlendFactor 7` against 7 resets — so if the
        // runtime lowers its default as NULL, the first hit is frame 1.
        L3A_REFUSALS.blend_factor_defaulted.bump();
        &DEFAULT_BLEND_FACTOR
    } else {
        // SAFETY: non-null per the check. The length is NOT carried by the type
        // — `PFND3D12DDI_OM_SETBLENDFACTOR` is `fn(HCOMMANDLIST, *const FLOAT)`
        // (`d3d12umddi.rs:51586-51587`) — it comes from this fn's `# Safety`
        // precondition, which the DDI's declared `_In_ CONST FLOAT[4]` is what
        // guarantees. `[f32; 4]` is exactly four consecutive `f32` with the same
        // alignment as `f32`, so this borrows the runtime's own storage in place
        // rather than copying it.
        unsafe { &*factor.cast::<[f32; 4]>() }
    };
    // SAFETY: `value` addresses four live floats for the duration of the call.
    unsafe { state.engine().OMSetBlendFactor(Some(value)) };
}

/// `pfnOmSetStencilRef` -> `ID3D12GraphicsCommandList::OMSetStencilRef`.
///
/// # Safety
/// As [`draw_instanced`].
unsafe extern "C" fn om_set_stencil_ref(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    stencil_ref: ddi12::UINT,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    // SAFETY: one by-value `UINT`.
    unsafe { state.engine().OMSetStencilRef(stencil_ref) };
}

/// `pfnSetPipelineState` -> `ID3D12GraphicsCommandList::SetPipelineState`.
///
/// # ⛔ The PSO's baked depth bias and strip cut are NOT re-applied here
///
/// `SUBSTRATE.md` §4.5 obliges a `_0110` driver to re-apply the PSO's own
/// depth-bias and IB-strip-cut values on every `pfnSetPipelineState` even when
/// the PSO declared them dynamic — the inverse of the Vulkan model. **The engine
/// already does it**, inside `d3d12_command_list_SetPipelineState`
/// (`vkd3d-proton-helios/libs/vkd3d/command.c:12711-12733`, *"For any optionally
/// dynamic state, we need to re-apply the corresponding static state that the
/// PSO was created with."*), and doing it a second time here would overwrite
/// whatever `pfnRSSetDepthBias` set for this list between the two calls. The
/// module doc and `pso.rs`'s `L6PsoDynamicStateFlagForwarded` both carry this;
/// it is repeated at the slot because this is where it would be re-added.
///
/// # ⚠ A NULL handle is legal and is not counted
///
/// `D3D12DDIARG_RESETCOMMANDLIST_0040` carries no PSO, so the runtime lowers the
/// application's `Reset(pAllocator, pInitialState)` as a `pfnSetPipelineState`
/// immediately afterwards — and `pInitialState` is optional. A handle with a
/// null `pDrvPrivate` is that case, and it forwards as `None`, which is exactly
/// what the API call would have been.
///
/// ⛔ A **non-null** handle whose slot is empty is a different thing: L6's
/// `pfnCreatePipelineState` refused it. It is counted, and it still forwards as
/// `None` — unbinding, so the next draw fails in the engine, rather than
/// silently leaving the previous pipeline bound and drawing the wrong thing.
///
/// # Safety
/// As [`draw_instanced`]; `h_pso`, when its `pDrvPrivate` is non-null, must
/// address the private block `pfnCalcPrivatePipelineStateSize` sized.
unsafe extern "C" fn set_pipeline_state(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_pso: ddi12::D3D12DDI_HPIPELINESTATE,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    let pso = if h_pso.pDrvPrivate.is_null() {
        None
    } else {
        // SAFETY: the caller guarantees the slot lies inside the private block
        // L6's `pfnCalcPrivatePipelineStateSize` sized. The borrow does not
        // outlive this call, and `ManuallyDrop` is what keeps the slot's own
        // reference from being released here.
        let loaded = unsafe { pso::engine_pipeline_state(h_pso) };
        if loaded.is_none() {
            note_refusal(&L3A_REFUSALS.pipeline_state_unresolved);
            if let Some(n) = record_budget() {
                log_error!(
                    "SetPipelineState: pso={:p} carries no engine pipeline -- its create refused; \
                     forwarding NULL so the next draw fails in the engine rather than drawing \
                     with the previous pipeline (x{})",
                    h_pso.pDrvPrivate,
                    n + 1,
                );
            }
        }
        loaded
    };
    // SAFETY: `pso`, when present, borrows the slot's live reference for this
    // call; `None` is the API's own encoding of "no pipeline".
    unsafe { state.engine().SetPipelineState(pso.as_deref()) };
}

/// `pfnOMSetDepthBounds` — **counted, in two arms.**
///
/// ⛔ `caps12.rs:571` reports `DepthBoundsTestSupported = 0`, so no pipeline this
/// driver advertises can perform a depth-bounds test and there is nothing for a
/// bounds value to affect. Neither arm forwards.
///
/// # ⛔ Two arms, because the runtime calls this slot after EVERY reset
///
/// ⚠ **A single counter here was graded backwards and could not have been read.**
/// `DDI_REFERENCE.md:3499-3504` measured `pfnResetCommandList` as being followed
/// by a fixed 15-call state-reset block *"whether or not the application touches
/// that state"*, and `pfnOMSetDepthBounds` is one of the fifteen:
/// `tmp/dx12/gates/G5/D-triangle.log:293` reads `cl[1] pfnResetCommandList 7` and
/// `:310` reads `cl[52] pfnOMSetDepthBounds 7` — identical counts, in a triangle
/// sample that never uses depth bounds. So one counter was guaranteed non-zero on
/// any workload that records a command list, while its doc graded it *"Expected
/// 0, and a hit is a finding about the caps"*. Because `note_refusal` prints the
/// whole `D3D12 DDI refusals:` set on a counter's first hit
/// (`umd12/src/lib.rs:556-560`), frame 1 of the first gate run wrote a refusal
/// record and sent triage after a caps bug that cannot exist.
///
/// ⭐ **The partition is exact, not a heuristic.** The depth-bounds default is
/// `[0.0, 1.0]` — `tmp/dx12/specs/d3d/DepthBoundsTest.md` states it twice, once
/// for the API (*"The default values are 0 and 1 for the Min and Max,
/// respectively"*) and once for the DDI — so that is the value the reset block
/// restores and `min == 0.0 && max == 1.0` separates the runtime's unconditional
/// default write from an application request with no tolerance and no guessing.
///
/// * the default: `L3aDepthBoundsDefaultDropped`, graded **expected non-zero**,
///   ~1 per `pfnResetCommandList`. It changes nothing, so dropping it is exact.
/// * anything else: `L3aDepthBoundsRefused`, graded **Expected 0**, and it logs.
///
/// ⛔ Split exactly as L9 split `pfnRSSetShadingRate` — `L9ShadingRateDefaultDropped`
/// / `L9ShadingRateRefused` in `misc.rs`, which cites the same measured block —
/// including `note_refusal` on both arms: the first-hit summary is what makes
/// either counter readable at all, and once the two are separated the line it
/// prints says which of them moved.
///
/// ⭐ **Reading both floats is the point of the body**, not incidental: as `_min`
/// and `_max` they were dropped arguments with no counter, so nothing could tell
/// the two cases apart. NaN is not a special case — the API converts NaNs to 0
/// before the DDI sees them, and a NaN that arrived anyway compares unequal to
/// both bounds and lands in the refused arm, which is where an unexplained value
/// belongs.
///
/// ⚠ **Neither arm is reported to the runtime**, and that policy was re-checked
/// against [`report_error`]'s repoint rather than inherited. For the default arm
/// it is not close: it fires once per reset, so a report would quarantine every
/// command list this driver ever hands back. For the refused arm the reason is
/// the older one, and it survives the repoint because it was never about the
/// cost: a declined *capability* is not a lost recording. This driver publishes
/// `DepthBoundsTestSupported = 0` in caps, and `DepthBoundsTest.md`'s "Runtime
/// Code" section describes the runtime deciding **from that cap** to remove the
/// command list when the DDI is unsupported — so the fact is already published
/// where the runtime reads it, and reporting would answer it a second time from
/// the wrong end.
///
/// ⚠ **The engine is not the limit here** — `d3d12_command_list_OMSetDepthBounds`
/// is fully implemented (`command.c:18047-18061`). So the whole cost of turning
/// this on is `caps12.rs` reporting the cap and this body forwarding two floats;
/// `L3aDepthBoundsRefused` is what says whether any workload ever wanted it.
///
/// # Safety
/// Trivially safe: no argument is dereferenced. Declared `unsafe` because the
/// DDI's PFN typedef is.
unsafe extern "C" fn om_set_depth_bounds(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    min: ddi12::FLOAT,
    max: ddi12::FLOAT,
) {
    if min == DEPTH_BOUNDS_DEFAULT_MIN && max == DEPTH_BOUNDS_DEFAULT_MAX {
        note_refusal(&L3A_REFUSALS.depth_bounds_default_dropped);
        return;
    }
    note_refusal(&L3A_REFUSALS.depth_bounds_refused);
    if let Some(n) = record_budget() {
        log_error!(
            "OMSetDepthBounds: [{min}, {max}] is not the [0, 1] default the runtime's state-reset \
             block restores, and this driver reports DepthBoundsTestSupported=0 -- dropped (x{})",
            n + 1,
        );
    }
}

/// `pfnSetSamplePositions` — **REFUSED**, `L3aSamplePositionsRefused`.
///
/// ⛔ **Two independent reasons, and the second one is decisive.**
///
/// 1. `caps12.rs:572` reports `ProgrammableSamplePositionsTier = NONE`, so no
///    application should reach this slot at all.
/// 2. ⛔ The engine's own body is a stub — `d3d12_command_list_SetSamplePositions`
///    is `FIXME("... stub!\n")` and nothing else
///    (`vkd3d-proton-helios/libs/vkd3d/command.c:18063-18068`). Forwarding would
///    be fake success: the call would return, the positions would be discarded,
///    and no counter anywhere would say so.
///
/// ⚠ The array is never read, which is why this body validates neither
/// `num_samples_per_pixel` nor `num_pixels` — there is no read to guard.
///
/// # Safety
/// Trivially safe: no argument is dereferenced.
unsafe extern "C" fn set_sample_positions(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    _num_samples_per_pixel: ddi12::UINT,
    _num_pixels: ddi12::UINT,
    _sample_positions: *mut ddi12::D3D12DDI_SAMPLE_POSITION,
) {
    note_refusal(&L3A_REFUSALS.sample_positions_refused);
}

/// `pfnOmSetAlphaBlendFactor` — **RETIRED**, `L3aAlphaBlendFactorRetired`.
///
/// ⛔ **Never implement this.** It is `cl[69]`, one of the four slots `D12-G5`
/// measured WARP leaving NULL, and `DDI_REFERENCE.md` §14.1.1 classifies it as
/// **RETIRED** rather than optional: `VulkanOn12.md:270` says *"a previous
/// version of this spec referred to `pfnOmSetAlphaBlendFactor` to assign the
/// alpha blend factor. This function is no longer valid, but its entry has been
/// retained and is marked as unused in D3D."* The replacement is the existing
/// [`om_set_blend_factor`], whose component `[3]` is the constant for
/// `D3D12DDI_BLEND_ALPHA_FACTOR` / `_INV_ALPHA_FACTOR`.
///
/// ⚠ A counting stub rather than the NULL WARP writes, because §14.1's rule is
/// that *"a stub costs nothing and turns 'the header lied' into a counter
/// instead of a jump through a null pointer"*. **Expected 0**, and a hit is a
/// genuine finding about the runtime rather than about this driver.
///
/// # Safety
/// Trivially safe: no argument is dereferenced.
unsafe extern "C" fn om_set_alpha_blend_factor(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    _factor: ddi12::FLOAT,
) {
    note_refusal(&L3A_REFUSALS.alpha_blend_factor_retired);
}

/// `pfnOmSetFrontAndBackStencilRef` ->
/// `ID3D12GraphicsCommandList8::OMSetFrontAndBackStencilRef`.
///
/// # Safety
/// As [`draw_instanced`].
unsafe extern "C" fn om_set_front_and_back_stencil_ref(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    front: ddi12::UINT,
    back: ddi12::UINT,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    let Some(list9) = engine_list9(state) else {
        // SAFETY: `state` is live for this DDI call, as `ia_set_topology`. The state was
        // dropped, which is a correctness failure and not a declined capability.
        unsafe { report_error(state, E_FAIL) };
        return;
    };
    // SAFETY: `list9` is an owned reference to the same engine list, live for
    // this call; two by-value `UINT`s.
    unsafe { list9.OMSetFrontAndBackStencilRef(front, back) };
}

/// `pfnRSSetDepthBias` -> `ID3D12GraphicsCommandList9::RSSetDepthBias`.
///
/// # ⭐ The `DepthBias` trap, and why this slot has nothing to convert
///
/// `SUBSTRATE.md` §4.5's ABI trap is that `DepthBias` changed from `INT` to
/// `FLOAT` in the DDI rasterizer desc at 0099, *at the same offset*. This DDI
/// carries the 0099 shape — `PFND3D12DDI_SET_DEPTH_BIAS_STATE_0099` is three
/// `FLOAT`s — and `ID3D12GraphicsCommandList9::RSSetDepthBias` takes three
/// `FLOAT`s. ⛔ So the float crosses as a float and there is no rounding to get
/// wrong. That is the same resolution `pso.rs` reached for the *static* half by
/// emitting a `RASTERIZER2` subobject (whose `DepthBias` is also `FLOAT`) rather
/// than the legacy `D3D12_RASTERIZER_DESC` with its `INT`: **neither half of
/// this driver ever converts a depth bias.**
///
/// # Safety
/// As [`draw_instanced`].
unsafe extern "C" fn rs_set_depth_bias(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    depth_bias: ddi12::FLOAT,
    depth_bias_clamp: ddi12::FLOAT,
    slope_scaled_depth_bias: ddi12::FLOAT,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    let Some(list9) = engine_list9(state) else {
        // SAFETY: `state` is live for this DDI call, as `om_set_front_and_back_stencil_ref`.
        unsafe { report_error(state, E_FAIL) };
        return;
    };
    // SAFETY: `list9` is live for this call; three by-value `FLOAT`s, forwarded
    // unconverted.
    unsafe { list9.RSSetDepthBias(depth_bias, depth_bias_clamp, slope_scaled_depth_bias) };
}

// ---------------------------------------------------------------------------
// IA / SO / OM — 5 slots
// ---------------------------------------------------------------------------

/// Translate `D3D12DDI_INDEX_BUFFER_STRIP_CUT_VALUE` into the API's.
///
/// ⛔ By `match` over the three generated enumerators, never by cast — the
/// `DDI_REFERENCE.md` §9.6.1 scar is a DDI enum and its API twin colliding on a
/// value with different meanings while the member types keep the compiler
/// silent. `pso.rs` proves these three are value-identical; this still matches,
/// because the proof licenses the *values* and an out-of-range fourth value has
/// no meaning on either side.
fn api_strip_cut(
    v: ddi12::D3D12DDI_INDEX_BUFFER_STRIP_CUT_VALUE,
) -> Option<D3D12_INDEX_BUFFER_STRIP_CUT_VALUE> {
    use ddi12::{
        D3D12DDI_INDEX_BUFFER_STRIP_CUT_VALUE_D3D12DDI_INDEX_BUFFER_STRIP_CUT_VALUE_0xFFFF as CUT_16,
        D3D12DDI_INDEX_BUFFER_STRIP_CUT_VALUE_D3D12DDI_INDEX_BUFFER_STRIP_CUT_VALUE_0xFFFFFFFF as CUT_32,
        D3D12DDI_INDEX_BUFFER_STRIP_CUT_VALUE_D3D12DDI_INDEX_BUFFER_STRIP_CUT_VALUE_DISABLED as CUT_OFF,
    };
    match v {
        CUT_OFF => Some(D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_DISABLED),
        CUT_16 => Some(D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_0xFFFF),
        CUT_32 => Some(D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_0xFFFFFFFF),
        _ => None,
    }
}

/// `pfnIASetIndexBuffer` -> `ID3D12GraphicsCommandList::IASetIndexBuffer`.
///
/// ⚠ A NULL `pDesc` is legal and forwards as `None`: D3D12 defines it as
/// unbinding the index buffer, and vkd3d handles it explicitly
/// (`command.c:14484-14491`). It is not counted, because nothing was refused.
///
/// # Safety
/// As [`draw_instanced`]; `desc`, when non-null, must address one readable
/// `D3D12DDI_INDEX_BUFFER_VIEW`.
unsafe extern "C" fn ia_set_index_buffer(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    desc: *const ddi12::D3D12DDI_INDEX_BUFFER_VIEW,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    // ⚠ The pointer is CAST, never dereferenced here: the two structs are
    // layout-identical by the assertion above, so the engine reads the runtime's
    // own storage and this driver copies nothing.
    let view = (!desc.is_null()).then_some(desc.cast::<D3D12_INDEX_BUFFER_VIEW>());
    // SAFETY: `view`, when present, is the runtime's own pointer to one live
    // view of an identical layout; `None` is the API's "unbind".
    unsafe { state.engine().IASetIndexBuffer(view) };
}

/// `pfnIASetVertexBuffers` -> `ID3D12GraphicsCommandList::IASetVertexBuffers`.
///
/// ⛔ `StartSlot + NumViews` is checked against
/// `D3D12_IA_VERTEX_INPUT_RESOURCE_SLOT_COUNT` **without overflowing**, which is
/// why the comparison is written as a subtraction against the limit rather than
/// as `start + n > limit`. vkd3d makes the same check the same way
/// (`command.c:14549-14554`) and answers a violation with a `WARN` and an early
/// return; here it is a counter.
///
/// ⚠ A null `pViews` forwards as `None`, which vkd3d documents as *"Native
/// drivers appear to ignore this call. Buffer bindings are kept as-is."* — so it
/// is a legal no-op and is not counted.
///
/// # Safety
/// As [`draw_instanced`], and `views`, when non-null, must address at least
/// `num_views` readable `D3D12DDI_VERTEX_BUFFER_VIEW`s.
unsafe extern "C" fn ia_set_vertex_buffers(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    start_slot: ddi12::UINT,
    num_views: ddi12::UINT,
    views: *const ddi12::D3D12DDI_VERTEX_BUFFER_VIEW,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    let start = start_slot as usize;
    let n = num_views as usize;
    let limit = D3D12_IA_VERTEX_INPUT_RESOURCE_SLOT_COUNT as usize;
    if start >= limit || n > limit - start {
        note_refusal(&L3A_REFUSALS.vertex_buffers_bad_arg);
        if let Some(k) = record_budget() {
            log_error!(
                "IASetVertexBuffers: StartSlot={start_slot} NumViews={num_views} is outside the \
                 {limit} vertex-input slots D3D12 defines -- refused (x{})",
                k + 1,
            );
        }
        // SAFETY: `state` is live for this DDI call, as `ia_set_topology`.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    }
    let slice: Option<&[D3D12_VERTEX_BUFFER_VIEW]> = if views.is_null() {
        None
    } else if n == 0 {
        Some(&[])
    } else {
        // SAFETY: non-null and bounded per the checks above; the caller
        // guarantees `num_views` readable elements, and the two structs are
        // layout-identical by the assertion above.
        Some(unsafe { core::slice::from_raw_parts(views.cast::<D3D12_VERTEX_BUFFER_VIEW>(), n) })
    };
    trace_line!("IASetVertexBuffers: start={start_slot} n={num_views}");
    // SAFETY: the slice, when present, is live for the whole call.
    unsafe { state.engine().IASetVertexBuffers(start_slot, slice) };
}

/// `pfnSOSetTargets` -> `ID3D12GraphicsCommandList::SOSetTargets`.
///
/// ⛔ The bound is `D3D12_SO_BUFFER_SLOT_COUNT` (4), and a range outside it is
/// refused and reported, exactly as in [`ia_set_vertex_buffers`].
///
/// # ⭐ A null `pViews` with a non-zero count FORWARDS, and does not remove
/// anything
///
/// ⚠ It used to be folded into the range check and answered with an error
/// report, which the `PARALLEL.md` §10 review caught as an asymmetry: the twin
/// slot one screen up takes the **identical** `(HCOMMANDLIST, StartSlot,
/// NumViews, pViews)` shape, sits in the **same** measured 15-call per-reset
/// block (`DDI_REFERENCE.md:3499-3504`), and treats that shape as a legal no-op
/// on vkd3d's own authority — *"Native drivers appear to ignore this call"*
/// (`vkd3d-proton-helios/libs/vkd3d/command.c:14556-14558`). One slot cannot
/// answer a shape with a driver error while its twin forwards it.
///
/// ⚠ **Reachability is UNPROVEN** — `tmp/dx12/gates/G5/D-triangle.log:307` shows
/// `cl[47] pfnSOSetTargets 7` against 7 `pfnResetCommandList` calls, so this slot
/// does fire once per reset, but the trace records slot names and not arguments.
/// If the runtime ever lowers "no stream-output targets" as `(0, N, NULL)`, the
/// old code answered the first reset of the first frame with an error report.
///
/// ⛔ **Forwarding is strictly safer in both directions**, which is why it is not
/// a coin toss. `SOSetTargets(start, None)` reaches the engine as
/// `view_count = 0, views = NULL` (windows-rs `map_or(0, len)`), so the loop that
/// dereferences `views[i]` with **no** null test (`command.c:14641-14660`) runs
/// zero times — the fault the old check existed to prevent is prevented by the
/// count, not by the refusal — and a possibly-legal unbind stops being fatal.
///
/// ⚠ Stream output needs `VK_EXT_transform_feedback`; without it vkd3d prints a
/// `FIXME` and returns, which is the engine's answer to give and is not
/// something this driver can see from here. UNVERIFIED on this substrate.
///
/// # Safety
/// As [`ia_set_vertex_buffers`], for `D3D12DDI_STREAM_OUTPUT_BUFFER_VIEW`s.
unsafe extern "C" fn so_set_targets(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    start_slot: ddi12::UINT,
    num_views: ddi12::UINT,
    views: *const ddi12::D3D12DDI_STREAM_OUTPUT_BUFFER_VIEW,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    let start = start_slot as usize;
    let n = num_views as usize;
    let limit = D3D12_SO_BUFFER_SLOT_COUNT as usize;
    if start >= limit || n > limit - start {
        note_refusal(&L3A_REFUSALS.so_targets_bad_arg);
        if let Some(k) = record_budget() {
            log_error!(
                "SOSetTargets: StartSlot={start_slot} NumViews={num_views} is outside the {limit} \
                 stream-output slots D3D12 defines -- refused (x{})",
                k + 1,
            );
        }
        // SAFETY: `state` is live for this DDI call, as `ia_set_topology`.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    }
    let slice: Option<&[D3D12_STREAM_OUTPUT_BUFFER_VIEW]> = if views.is_null() {
        if n != 0 {
            // ⚠ Not a refusal, and ⛔ `bump` rather than `note_refusal` for the
            // same reason as the triangle-fan arm: this arm FORWARDS, and
            // `note_refusal` would print the whole `D3D12 DDI refusals:` set at
            // error level on its first hit. If the runtime does lower the null
            // form, that first hit is the first reset of the first frame — a
            // refusal record for a call this driver honoured.
            L3A_REFUSALS.so_targets_null_array.bump();
        }
        None
    } else if n == 0 {
        Some(&[])
    } else {
        // SAFETY: non-null and bounded per the check; layout-identical by the
        // assertion above.
        Some(unsafe {
            core::slice::from_raw_parts(views.cast::<D3D12_STREAM_OUTPUT_BUFFER_VIEW>(), n)
        })
    };
    trace_line!("SOSetTargets: start={start_slot} n={num_views}");
    // SAFETY: the slice, when present, is live for the whole call.
    unsafe { state.engine().SOSetTargets(start_slot, slice) };
}

/// `pfnOMSetRenderTargets` -> `ID3D12GraphicsCommandList::OMSetRenderTargets`.
///
/// ⭐ **Both descriptor arrays cross as POINTERS, not as slices**, and that is
/// what makes `RTsSingleHandleToDescriptorRange` safe to forward without
/// interpreting it: when it is TRUE the array holds exactly **one** handle and
/// the engine strides internally, when FALSE it holds `NumRenderTargetDescriptors`
/// of them. A slice would force this driver to decide which, and getting that
/// wrong is a read past the end of the runtime's array. Passing the pointer
/// leaves the decision where the flag is defined.
///
/// ⚠ `pDepthStencilDescriptor` is legitimately NULL — that is "no depth buffer"
/// — and is not counted.
///
/// # Safety
/// As [`draw_instanced`]. `render_targets`, when non-null, must address one
/// handle if `rts_single_handle` is TRUE and at least `num_render_targets`
/// otherwise; `depth_stencil`, when non-null, must address one.
unsafe extern "C" fn om_set_render_targets(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    num_render_targets: ddi12::UINT,
    render_targets: *const ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
    rts_single_handle: ddi12::BOOL,
    depth_stencil: *const ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    let n = num_render_targets as usize;
    if n > D3D12_SIMULTANEOUS_RENDER_TARGET_COUNT as usize || (n != 0 && render_targets.is_null()) {
        note_refusal(&L3A_REFUSALS.render_targets_bad_arg);
        if let Some(k) = record_budget() {
            log_error!(
                "OMSetRenderTargets: NumRenderTargetDescriptors={num_render_targets} \
                 pRenderTargetDescriptors={render_targets:p} -- refused (x{})",
                k + 1,
            );
        }
        // SAFETY: `state` is live for this DDI call, as `ia_set_topology`.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    }
    let rtvs =
        (!render_targets.is_null()).then_some(render_targets.cast::<D3D12_CPU_DESCRIPTOR_HANDLE>());
    let dsv =
        (!depth_stencil.is_null()).then_some(depth_stencil.cast::<D3D12_CPU_DESCRIPTOR_HANDLE>());
    trace_line!(
        "OMSetRenderTargets: n={num_render_targets} single={rts_single_handle} dsv={}",
        !depth_stencil.is_null(),
    );
    // SAFETY: both pointers are the runtime's own, cast between two structs the
    // assertion above proves layout-identical; the BOOL is forwarded as the
    // `i32` both sides declare it to be.
    unsafe {
        state
            .engine()
            .OMSetRenderTargets(num_render_targets, rtvs, BOOL(rts_single_handle), dsv);
    }
}

/// `pfnIASetIndexBufferStripCutValue` ->
/// `ID3D12GraphicsCommandList9::IASetIndexBufferStripCutValue`.
///
/// ⚠ This is the **dynamic** half of `SUBSTRATE.md` §4.5's strip-cut obligation.
/// The static half — re-applying the PSO's own value on `pfnSetPipelineState` —
/// is the engine's, and [`set_pipeline_state`]'s doc says why this file must not
/// do it as well.
///
/// # Safety
/// As [`draw_instanced`].
unsafe extern "C" fn ia_set_index_buffer_strip_cut_value(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    strip_cut: ddi12::D3D12DDI_INDEX_BUFFER_STRIP_CUT_VALUE,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    let Some(api) = api_strip_cut(strip_cut) else {
        note_refusal(&L3A_REFUSALS.index_buffer_strip_cut_unknown);
        if let Some(n) = record_budget() {
            log_error!(
                "IASetIndexBufferStripCutValue: {strip_cut} names none of the three values \
                 D3D12DDI_INDEX_BUFFER_STRIP_CUT_VALUE defines (x{})",
                n + 1,
            );
        }
        // SAFETY: `state` is live for this DDI call, as `ia_set_topology`.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };
    let Some(list9) = engine_list9(state) else {
        // SAFETY: `state` is live for this DDI call, as `om_set_front_and_back_stencil_ref`.
        unsafe { report_error(state, E_FAIL) };
        return;
    };
    // SAFETY: `list9` is live for this call; one translated by-value enumerator.
    unsafe { list9.IASetIndexBufferStripCutValue(api) };
}

// ---------------------------------------------------------------------------
// Indirect and bundles — 2 slots
// ---------------------------------------------------------------------------

/// `pfnExecuteBundle` -> `ID3D12GraphicsCommandList::ExecuteBundle`.
///
/// ⭐ **The second `D3D12DDI_HCOMMANDLIST` resolves through the same accessor as
/// the first.** `queue::command_list_state` is the only legal route to an engine
/// list, and this is the one slot in the table that needs it twice.
///
/// ⚠ **The bundle's class is checked here even though vkd3d checks it too.**
/// `d3d12_command_list_ExecuteBundle` answers a non-bundle with
/// `WARN("Command list %p not a bundle.")` and a silent return
/// (`command.c:13831-13845`) — a dropped draw batch with no counter. Checking
/// `CommandListState::list_type` first turns that into
/// `L3aExecuteBundleNotBundle`.
///
/// ⚠ The call **is** still forwarded here, unlike the class mismatch in
/// [`reset_command_list`], and the difference is what forwarding costs: this
/// entry point returns `VOID` and resolves the bundle by vtable identity
/// (`d3d12_bundle_from_iface`), so a wrong guess by this driver is answered with
/// a `WARN` and nothing else. A disagreement between the two sides about which
/// lists are bundles is worth measuring exactly because measuring it is free.
///
/// ⚠ **This slot cannot work while this driver refuses bundles**, and the
/// refusal is now at `queue::create_command_list` rather than at
/// `pfnResetCommandList` — so in practice the runtime never reaches here with a
/// bundle handle at all, because the application's `CreateCommandList` failed.
/// ⛔ Which makes `L3aExecuteBundleMissing` / `L3aExecuteBundleNotBundle` the
/// instruments for *"the runtime submitted a bundle anyway"*, a question worth
/// keeping open; `L2BundleListRefused` is where the lost work is counted.
///
/// # Safety
/// As [`draw_instanced`], for **both** handles.
unsafe extern "C" fn execute_bundle(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_bundle: ddi12::D3D12DDI_HCOMMANDLIST,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    // SAFETY: the caller guarantees the bundle handle is equally live.
    let Some(bundle) = (unsafe { queue::command_list_state(h_bundle) }) else {
        note_refusal(&L3A_REFUSALS.execute_bundle_missing);
        if let Some(n) = record_budget() {
            log_error!(
                "ExecuteBundle: bundle={:p} carries no engine command list -- the bundle's \
                 recorded commands have been dropped (x{})",
                h_bundle.pDrvPrivate,
                n + 1,
            );
        }
        // SAFETY: `state` is live for this DDI call, as `ia_set_topology`.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };
    if bundle.list_type() != D3D12_COMMAND_LIST_TYPE_BUNDLE {
        note_refusal(&L3A_REFUSALS.execute_bundle_not_bundle);
        if let Some(n) = record_budget() {
            log_error!(
                "ExecuteBundle: the executed list is class {} rather than BUNDLE -- forwarding \
                 anyway, and the engine will drop it with a WARN if it agrees (x{})",
                bundle.list_type().0,
                n + 1,
            );
        }
    }
    // SAFETY: both lists are live for this call; the engine takes a borrowed
    // reference and does not keep it.
    unsafe { state.engine().ExecuteBundle(bundle.engine()) };
}

/// The `{ID3D12Resource, offset}` pair one `D3D12DDIARG_BUFFER_PLACEMENT` names.
///
/// ⛔ **The union has exactly one member and it is the UMD's arm.**
/// `D3D12DDIARG_BUFFER_PLACEMENT` is `{ BaseAddress: union { UMD:
/// D3D12DDIARG_HRESOURCE_PLACEMENT } }` and that inner struct is
/// `{ hResource, Offset }` (`umd12/bindgen/cached/d3d12umddi.rs:48461-48464`,
/// `:48485-48494`) — so there is no discriminant to check and no other arm to
/// mistake it for. The API's `ExecuteIndirect` takes exactly this pair as two
/// separate parameters, which is why the translation is a projection and not a
/// conversion.
///
/// ⛔ **Three outcomes, not two, and an `Option` would have collapsed the two that
/// matter.** *"The runtime named no buffer"* is legal for the count buffer and
/// illegal for the argument buffer; *"the runtime named one and this driver could not
/// resolve it"* is a lifetime bug in **both** positions. `pso::root_signature`'s doc
/// carries the same warning for root signatures, and `descriptors.rs`'s scar is what
/// happens when the two are conflated: a legal call answered with an error report.
enum PlacementRef<'a> {
    /// `hResource` was null — the runtime named no buffer.
    Absent,
    /// A live engine resource and the placement's byte offset into it.
    Bound(&'a ID3D12Resource, u64),
    /// `hResource` was non-null and carried no engine resource.
    Unresolved,
}

/// Project one `D3D12DDIARG_BUFFER_PLACEMENT` onto the
/// `(ID3D12Resource, UINT64 offset)` pair the API's `ExecuteIndirect` takes.
///
/// # Safety
/// `placement` must be an initialised `D3D12DDIARG_BUFFER_PLACEMENT` the runtime
/// passed by value, and its `hResource`, when non-null, must be a live handle from
/// `resource12`'s create. The returned reference must not outlive the DDI call.
unsafe fn buffer_placement<'a>(
    placement: &ddi12::D3D12DDIARG_BUFFER_PLACEMENT,
) -> PlacementRef<'a> {
    // SAFETY: the union has a single member, so reading `UMD` is reading the only
    // thing the runtime can have written; the value is initialised per the caller's
    // precondition.
    let umd = unsafe { placement.BaseAddress.UMD };
    if umd.hResource.pDrvPrivate.is_null() {
        return PlacementRef::Absent;
    }
    // SAFETY: a non-null `pDrvPrivate` on a resource handle the runtime passed to a
    // recording DDI is a handle `resource12`'s create sized and wrote; the borrow
    // does not outlive this call.
    match unsafe { super::resource12::engine_resource(umd.hResource) } {
        Some(resource) => PlacementRef::Bound(resource, umd.Offset),
        None => PlacementRef::Unresolved,
    }
}

/// `pfnExecuteIndirect` — **IMPLEMENTED**, `L3aExecuteIndirectForwarded`.
///
/// # ⭐ It forwards, and both halves of the old blocker are discharged
///
/// The old body was a **silent counted noop** justified by
/// `queue::create_command_signature` returning `E_NOTIMPL`. That create now builds a
/// real `ID3D12CommandSignature` for the four native action classes and refuses the
/// state-template classes **at create** (`queue.rs`'s S-4 block has why refusing at
/// create rather than here is the only honest split — vkd3d would otherwise accept
/// the signature and silently skip every call through it). ⇒ every signature that
/// reaches this slot is one this driver built and can execute.
///
/// The two objects this needed are both reached through their owning lane's single
/// accessor, so no handle payload is declared twice (`DECISIONS.md` D13):
/// `queue::engine_command_signature` for `D3D12DDI_HCOMMANDSIGNATURE` and
/// `resource12::engine_resource` for the two `D3D12DDIARG_BUFFER_PLACEMENT`s.
///
/// # ⛔ The §14.2 argument this slot used to make is INVALID and is not repeated
///
/// Its previous doc closed with *"`DDI_REFERENCE.md` §14.2's 99-slot minimum-viable
/// list contains neither this slot nor the command-signature triple"*. §14.0 of the
/// same document forbids that inference outright: *"treat a slot in 99-but-not-70 as
/// 'not exercised yet', never as 'not needed'."* The list was being read as licence
/// for the reading it rules out, in two files at once.
///
/// # ⚠ What is forwarded verbatim, and why nothing is validated here
///
/// `MaxCommandCount`, both offsets and the count buffer go through unchanged. vkd3d
/// owns every check that matters and duplicating one would create a second authority
/// able to drift: it early-returns on `max_command_count == 0`
/// (`command.c:17777-17778`) and refuses a count buffer when the guest lacks
/// `drawIndirectCount` (`:17781-17785`) — which this guest **has**, alongside
/// `multiDrawIndirect` (`docs/dx12/research/guest-vulkaninfo-full.txt:1237`,
/// `:1632`).
///
/// ⛔ A **null argument buffer** is the one thing refused here, and the reason is
/// re-derived rather than assumed: `impl_from_ID3D12Resource` *does* fold a null
/// interface to a null `struct d3d12_resource *` (`vkd3d_private.h:1317-1318`), so
/// the cast itself is safe — but `arg_impl` is then **dereferenced with no null
/// test** on the ordinary path, at `scratch.buffer = arg_impl->res.vk_buffer`
/// (`command.c:17880-17882`) and inside every `vkCmdDraw*Indirect*` call
/// (`:17897-17905`). ⇒ forwarding a null is an access violation inside the engine,
/// not a dropped draw. A null **count** buffer is fine — the engine tests
/// `count_impl` everywhere it uses it.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`; `h_signature` a
/// live handle from `queue::create_command_signature`; the two
/// `D3D12DDIARG_BUFFER_PLACEMENT`s arrive **by value** and their `hResource`s, when
/// non-null, must be live resource handles.
unsafe extern "C" fn execute_indirect(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_signature: ddi12::D3D12DDI_HCOMMANDSIGNATURE,
    max_command_count: ddi12::UINT,
    argument_buffer: ddi12::D3D12DDIARG_BUFFER_PLACEMENT,
    count_buffer: ddi12::D3D12DDIARG_BUFFER_PLACEMENT,
) {
    // SAFETY: forwarded unchanged; the caller carries `recording_list`'s
    // precondition.
    let Some(state) = (unsafe { recording_list(h_list) }) else {
        return;
    };
    // SAFETY: the caller guarantees a live signature handle, so its slot lies in the
    // private block `pfnCalcPrivateCommandSignatureSize` sized.
    let Some(signature) = (unsafe { queue::engine_command_signature(h_signature) }) else {
        // ⛔ Reported, not merely counted, and that is a change of channel the S-4
        // commit is entitled to make: with the create implemented, a signature that
        // does not resolve is no longer "the application already got `E_NOTIMPL` from
        // `CreateCommandSignature`" — the old doc's reason for staying silent — it is
        // this driver losing an object it built. Recording something the driver then
        // drops is exactly what `pfnSetCommandListErrorCb` quarantines.
        note_refusal(&L3A_REFUSALS.execute_indirect_signature_missing);
        if let Some(n) = record_budget() {
            log_error!(
                "ExecuteIndirect: hCommandSignature={:p} carries no engine signature -- the \
                 indirect draw is DROPPED (x{})",
                h_signature.drv_private(),
                n + 1,
            );
        }
        // SAFETY: `state` is live for this call — `recording_list` just returned it.
        unsafe { report_error(state, E_FAIL) };
        return;
    };
    // ⛔ The argument buffer is MANDATORY, and refusing a missing one is not
    // defensive: the engine's cast folds null to null, but it then dereferences
    // `arg_impl` with no null test at `command.c:17880-17882` and in every
    // `vkCmdDraw*Indirect*` call (`:17897-17905`), so forwarding a null is an access
    // violation inside the engine rather than a dropped draw. See this slot's doc.
    //
    // SAFETY: the placement arrived by value from the runtime and is initialised;
    // its `hResource`, when non-null, is a live resource handle.
    let (arg_resource, arg_offset) = match unsafe { buffer_placement(&argument_buffer) } {
        PlacementRef::Bound(r, o) => (r, o),
        PlacementRef::Absent | PlacementRef::Unresolved => {
            note_refusal(&L3A_REFUSALS.execute_indirect_arg_buffer_missing);
            if let Some(n) = record_budget() {
                log_error!(
                    "ExecuteIndirect: argument buffer resolves to nothing -- refused, because the \
                     engine dereferences it unconditionally (x{})",
                    n + 1,
                );
            }
            // SAFETY: `state` is live for this call.
            unsafe { report_error(state, E_INVALIDARG) };
            return;
        }
    };
    // ⚠ An ABSENT count buffer is LEGAL and is the common case — it selects
    // `vkCmdDrawIndirect` over `vkCmdDrawIndirectCount` — so it is the ordinary path
    // and touches no counter. Only `Unresolved` is a finding.
    //
    // SAFETY: as above.
    let (count_resource, count_offset) = match unsafe { buffer_placement(&count_buffer) } {
        PlacementRef::Absent => (None, 0),
        PlacementRef::Bound(r, o) => (Some(r), o),
        PlacementRef::Unresolved => {
            note_refusal(&L3A_REFUSALS.execute_indirect_count_buffer_missing);
            if let Some(n) = record_budget() {
                log_error!(
                    "ExecuteIndirect: count buffer handle is non-null but carries no engine \
                     resource -- refused rather than executed with MaxCommandCount (x{})",
                    n + 1,
                );
            }
            // ⛔ Refused, not degraded. Dropping the count buffer would run all
            // `MaxCommandCount` commands instead of the `N <= MaxCommandCount` the
            // application asked for — extra draws with garbage arguments, i.e. wrong
            // pixels, which is strictly worse than a reported failure.
            // SAFETY: as above.
            unsafe { report_error(state, E_INVALIDARG) };
            return;
        }
    };

    // SAFETY: the list, the signature and both resources are live for this call and
    // the engine takes borrowed references it does not keep; every scalar is passed
    // by value.
    unsafe {
        state.engine().ExecuteIndirect(
            &*signature,
            max_command_count,
            arg_resource,
            arg_offset,
            count_resource,
            count_offset,
        );
    }
    note_refusal(&L3A_REFUSALS.execute_indirect_forwarded);
    trace_line!(
        "ExecuteIndirect: max={max_command_count} arg={arg_offset} countBuf={} count={count_offset}",
        count_resource.is_some(),
    );
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// Install L3a's 23 command-list slots.
///
/// Chain position: `Stubbed` -> `RecordSlots` on the command-list table.
pub(crate) fn install(
    mut filling: Filling<'_, CommandListTable, stage::Stubbed>,
) -> Filling<'_, CommandListTable, stage::RecordSlots> {
    let table = filling.table();
    // list lifetime — 2 (landed with the Round 2 spine; see the module doc)
    table.pfnCloseCommandList = Some(close_command_list);
    table.pfnResetCommandList = Some(reset_command_list);
    // draw — 3
    table.pfnDrawInstanced = Some(draw_instanced);
    table.pfnDrawIndexedInstanced = Some(draw_indexed_instanced);
    table.pfnDispatch = Some(dispatch);
    // fixed-function state — 11
    table.pfnIaSetTopology = Some(ia_set_topology);
    table.pfnRsSetViewports = Some(rs_set_viewports);
    table.pfnRsSetScissorRects = Some(rs_set_scissor_rects);
    table.pfnOmSetBlendFactor = Some(om_set_blend_factor);
    table.pfnOmSetStencilRef = Some(om_set_stencil_ref);
    table.pfnSetPipelineState = Some(set_pipeline_state);
    table.pfnOMSetDepthBounds = Some(om_set_depth_bounds);
    table.pfnSetSamplePositions = Some(set_sample_positions);
    table.pfnOmSetAlphaBlendFactor = Some(om_set_alpha_blend_factor);
    table.pfnOmSetFrontAndBackStencilRef = Some(om_set_front_and_back_stencil_ref);
    table.pfnRSSetDepthBias = Some(rs_set_depth_bias);
    // IA / SO / OM — 5
    table.pfnIASetIndexBuffer = Some(ia_set_index_buffer);
    table.pfnIASetVertexBuffers = Some(ia_set_vertex_buffers);
    table.pfnSOSetTargets = Some(so_set_targets);
    table.pfnOMSetRenderTargets = Some(om_set_render_targets);
    table.pfnIASetIndexBufferStripCutValue = Some(ia_set_index_buffer_strip_cut_value);
    // indirect and bundles — 2
    table.pfnExecuteBundle = Some(execute_bundle);
    table.pfnExecuteIndirect = Some(execute_indirect);
    filling.advance()
}

// ---------------------------------------------------------------------------
// Refusal counters
// ---------------------------------------------------------------------------

/// L3a's refusal counters. One instance, [`L3A_REFUSALS`]; the set that prints
/// them is [`REFUSALS`].
pub(crate) struct L3aRefusals {
    /// A command-list slot could not resolve its `D3D12DDI_HCOMMANDLIST` to a
    /// live `queue::CommandListState`. **Expected 0** — the runtime only records
    /// into a list `pfnCreateCommandList` returned `S_OK` for.
    ///
    /// ⚠ It is the one failure in this file that **cannot** be reported at all:
    /// with no `CommandListState` there is neither the `h_rt_list` the
    /// list-scoped callback takes nor the `h_device` the device-scoped one does,
    /// so both channels are out of reach. Every other refusal here has a choice.
    command_list_missing: RefusalCounter,
    /// `ID3D12GraphicsCommandList::Close` failed, and the failure **was** raised
    /// to the runtime. **Expected 0**; a hit means the engine rejected something
    /// recorded into this list and the application is being told so at the only
    /// point D3D12 defines for it.
    ///
    /// ⚠ **Read it beside `ResetListTypeMismatch`.** A list whose `Reset` this
    /// driver refused never entered the engine's recording state, so its paired
    /// `Close` is a guaranteed failure too — `E_FAIL` from
    /// `d3d12_bundle_Close`'s `!is_recording` arm (`bundle.c:392-402`). The two
    /// counters moving together is *one* defect, not two, and this file cannot
    /// tell the cases apart without holding a second copy of the engine's
    /// recording flag, which `queue::CommandListState`'s doc says not to do.
    close_engine_failed: RefusalCounter,
    /// `pfnResetCommandList` with a null `D3D12DDIARG_RESETCOMMANDLIST_0040`.
    /// **Expected 0** — the DDI declares it `_In_ CONST` and never optional.
    reset_bad_arg: RefusalCounter,
    /// A reset carried `D3D12DDI_COMMAND_LIST_FLAGS` marker hints the API enum
    /// has no counterpart for, and they were dropped. ⚠ Expected non-zero under
    /// a debug layer or a PIX capture; they are tooling hints, not behaviour.
    /// Tracks `queue.rs`'s `CommandListFlagsIgnored`, which counts the same two
    /// bits arriving at *create* rather than at reset.
    ///
    /// ⛔ Its arm uses `bump()` and logs its own budgeted line, **because** of
    /// the grading above: `note_refusal` prints the whole `D3D12 DDI refusals:`
    /// set on a counter's first hit, so a counter expected to fire under the
    /// debug layer must not be on that path — the debug layer is the
    /// configuration triage runs in, and a refusal record on frame 1 for a
    /// tolerated hint sends the reader somewhere there is nothing to find.
    /// ⚠ **A counter's grading and its call site have to agree**: expected
    /// non-zero means `bump`, expected 0 means `note_refusal`. This one was
    /// graded one way and called the other.
    reset_flags_ignored: RefusalCounter,
    /// `D3D12DDIARG_RESETCOMMANDLIST_0040::hDrvCommandRecorder` did not resolve
    /// to a live `queue::RecorderState`. **Expected 0.**
    reset_recorder_missing: RefusalCounter,
    /// The recorder resolved but has never been bound to a command pool, so
    /// there is no `ID3D12CommandAllocator` to reset against.
    ///
    /// ⛔ **Expected 0, and a hit is a real finding about DDI ordering**, not a
    /// transient: `queue.rs` creates a pool's allocator lazily at the first
    /// `pfnCommandRecorderSetCommandPoolAsTarget` precisely because that is the
    /// only DDI where a queue class meets a pool, and this counter is what says
    /// the runtime issued a reset before ever making that binding. It pairs with
    /// `PoolResetNoAllocator`, which is the *benign* half of the same shape — a
    /// pool reset before any recording is a no-op, a **list** reset before any
    /// binding is not.
    reset_no_allocator: RefusalCounter,
    /// The list's class and its recorder's allocator's class disagree, so the
    /// `Reset` was **refused** — not forwarded — and raised to the runtime.
    ///
    /// ⛔ **Expected 0, and the one reachable case it now covers is narrow.**
    ///
    /// ⚠⚠ **THIS GRADING HAS BEEN WRONG TWICE, and the second time it went stale
    /// inside the very merge that corrected the first.** Worth stating in full,
    /// because the pattern is the point:
    ///
    /// 1. It first read *"a non-zero reading without `ResetEngineFailed` moving
    ///    is the good outcome — it would mean vkd3d does not enforce the class
    ///    pairing"*. The engine settles that without a run: it enforces in both
    ///    halves (`command.c:7378-7382` for a regular list, `bundle.c:239-245`
    ///    and `:411-427` by vtable identity for a bundle), so the hoped-for
    ///    outcome cannot happen.
    /// 2. It was then regraded to *"GUARANTEED non-zero the moment a workload
    ///    records a BUNDLE"*. That was true when written and false by the end of
    ///    the same merge: `queue::create_command_list` now refuses
    ///    `Type == BUNDLE` outright (`L2BundleListRefused`), so no bundle list
    ///    can exist to reach `pfnResetCommandList` at all.
    ///
    /// ⇒ **The surviving reachable case is the narrow one**: a DIRECT / COMPUTE /
    /// COPY list whose recorder is bound to a pool already backed by an allocator
    /// of a *different* one of those three classes. It pairs with `queue.rs`'s
    /// `PoolTypeMismatch`, which sees the same disagreement from the binding side.
    /// ⛔ **For bundles, read `L2BundleListRefused` instead** — that is where the
    /// lost work now shows up, one DDI earlier. This counter stays as the
    /// tripwire that says the refusal at create is doing its job.
    reset_list_type_mismatch: RefusalCounter,
    /// `ID3D12GraphicsCommandList::Reset` was forwarded and failed, and the
    /// failure **was** raised to the runtime.
    ///
    /// ⛔ **Expected 0**, and it is only honestly Expected 0 because
    /// `ResetListTypeMismatch` now returns before the call: a class mismatch is a
    /// guaranteed `E_INVALIDARG` and used to land here, which made the grading
    /// unreachable. What is left is the *unforeseen* failure — chiefly an
    /// allocator the GPU is not done with, which is the application's obligation
    /// rather than this driver's.
    reset_engine_failed: RefusalCounter,
    /// A command-list slot needed to report a failure, and the `h_device` its
    /// `CommandListState` recorded did not resolve to a live device.
    ///
    /// ⛔ **Expected 0**, and the name is older than what it counts: the report
    /// itself is now *list*-scoped, but it is still reached by way of the device,
    /// because `device12::set_command_list_error` needs the device's callback
    /// table to find the callback. ⚠ The name is kept because counter names are
    /// the evidence contract — `D3D12 DDI refusals:` lines are diffed across
    /// builds — and renaming it would break every comparison to buy a word.
    set_error_no_device: RefusalCounter,
    /// A command-list slot needed `pfnSetCommandListErrorCb` and the runtime's
    /// `D3D12DDI_CORELAYER_DEVICECALLBACKS_0062` did not carry one.
    ///
    /// ⛔ **Expected 0, and its meaning changed with its call site.** It used to
    /// count a missing `pfnSetErrorCb`; it now counts a missing
    /// `pfnSetCommandListErrorCb`, the field immediately below it in the same
    /// struct. The consequence is different and *smaller*, which is the point: a
    /// hit is a recording failure the runtime never learns about — this driver
    /// dropped or refused something and the application will be told its
    /// `Close()` succeeded — where before a hit meant a device that never died.
    ///
    /// ⚠ Read it beside `SetErrorNoDevice`: that one is "no device to ask", this
    /// one is "asked, and the runtime published no such callback". Only the
    /// second is a statement about the runtime.
    set_error_cb_absent: RefusalCounter,
    // ── appended by the recording lane; ⛔ append only, never reorder ────────
    /// `pfnIaSetTopology` was handed a value outside the three runs
    /// `D3D12DDI_PRIMITIVE_TOPOLOGY` defines (`0..=6`, `10..=13`, `33..=64`), and
    /// the call was dropped rather than forwarded.
    ///
    /// ⛔ **Expected 0.** The enumeration is closed at DDI `_0110` and this
    /// driver pins every one of its run bounds against the API's generated
    /// constants, so a hit means the runtime passed a value neither header
    /// defines. ⚠ Dropping is the safe direction because the engine's answer is
    /// *graceful*, not because it is violent: `vk_topology_from_d3d12_topology`
    /// clamps an unknown value to `VK_PRIMITIVE_TOPOLOGY_POINT_LIST` behind a
    /// `FIXME` (`state.c:4375-4377`), i.e. it would draw points and report
    /// success. An earlier grading of this counter said the engine hands Vulkan
    /// `VK_PRIMITIVE_TOPOLOGY_MAX_ENUM`; that belongs to a different function
    /// (`state.c:4312-4314`) and the consequence was overstated.
    topology_unknown: RefusalCounter,
    /// How many times a workload asked for `TRIANGLEFAN`.
    ///
    /// ⚠ **Not a refusal.** It is the instrument for `SUBSTRATE.md` §4.5's one
    /// mandatory, cap-less `_0110` obligation that lands in this lane: triangle
    /// fans were revived at DDI 0097 and cannot be declined. The forward is
    /// native the whole way down — vkd3d maps it to
    /// `VK_PRIMITIVE_TOPOLOGY_TRIANGLE_FAN` and the host exposes no
    /// `VK_KHR_portability_subset` to withdraw it — so a **non-zero** reading
    /// with no fault is the good outcome and the evidence that the obligation
    /// has actually been exercised. A zero reading means untested, not broken.
    topology_triangle_fan: RefusalCounter,
    /// `pfnRsSetViewports` arrived with a count above
    /// `D3D12_VIEWPORT_AND_SCISSORRECT_OBJECT_COUNT_PER_PIPELINE`, or with a
    /// non-zero count and a null array. **Expected 0** — both are outside what
    /// D3D12 permits an application to ask for.
    viewports_bad_arg: RefusalCounter,
    /// `pfnRsSetScissorRects`, same two cases as `ViewportsBadArg`. **Expected
    /// 0.**
    scissor_rects_bad_arg: RefusalCounter,
    /// `pfnOmSetBlendFactor` arrived with a null `FLOAT[4]` and the API's
    /// documented default `{1,1,1,1}` was substituted.
    ///
    /// ⚠ **May legitimately be non-zero, and the substitution is not optional.**
    /// D3D12 defines a NULL blend factor as that default, but the engine's body
    /// opens with an unconditional 16-byte `memcmp` against the pointer, so
    /// forwarding the NULL would fault inside the caller's process. A hit says
    /// the runtime does pass NULL through to the DDI, which is a fact about the
    /// runtime nothing in `docs/dx12/` records.
    ///
    /// ⛔ **Not a refusal, and bumped with `bump` rather than `note_refusal`.**
    /// The documented value is applied and the call forwards, so nothing was
    /// refused; and this slot is in the runtime's per-reset state block
    /// (`D-triangle.log:301`, `cl[27] pfnOmSetBlendFactor 7` against 7 resets),
    /// so a `note_refusal` here would print the whole `D3D12 DDI refusals:` set
    /// on frame 1 of any run in which the runtime lowers its default as NULL.
    blend_factor_defaulted: RefusalCounter,
    /// `pfnSetPipelineState` was given a non-null `D3D12DDI_HPIPELINESTATE`
    /// whose slot is empty, i.e. L6's `pfnCreatePipelineState` refused it, and
    /// `NULL` was forwarded to the engine instead.
    ///
    /// ⛔ **Expected 0, and read it beside `L6PsoEngineFailed`** — this counter
    /// is the *consequence* of that one, one DDI later, and it is the point at
    /// which a failed PSO create turns into a draw that cannot work.
    ///
    /// ⚠ **Still not reported after the §10 review re-checked the policy**, but
    /// for a corrected reason. `pfnCreatePipelineState` is one of the few DDIs
    /// that **returns an HRESULT**, and L6 returns the engine's
    /// (`pso.rs`, `L6PsoEngineFailed`) rather than calling any error callback —
    /// so the runtime failed the application's `CreateGraphicsPipelineState` and
    /// the application has no `ID3D12PipelineState` to bind. Reaching this slot
    /// with a non-null handle for a PSO that was never created is therefore a
    /// runtime-level impossibility, which is what makes Expected 0 a real
    /// grading; quarantining a list for it would add a second answer to a
    /// question already answered at the API boundary.
    pipeline_state_unresolved: RefusalCounter,
    /// `pfnOMSetDepthBounds` with a range that is **not** the `[0, 1]` default —
    /// refused, coherently with `DepthBoundsTestSupported = 0` in `caps12.rs`.
    ///
    /// ⚠ **Expected 0**, and it is only honestly Expected 0 because
    /// `DepthBoundsDefaultDropped` now takes the default range: this slot is one
    /// of the fifteen the runtime issues after every `pfnResetCommandList`
    /// (`DDI_REFERENCE.md:3499-3504`), so a single counter here was guaranteed
    /// non-zero on any workload that records a list and this grading was
    /// unreachable. ⛔ **A counter's grading is a claim and it goes stale like any
    /// other** — that is the standing lesson, and this is the third counter in
    /// this crate to be caught by it.
    ///
    /// A hit is now what the old grading meant to say: a finding about the
    /// *caps*, not about this slot — an application reached a depth-bounds path
    /// on a driver that reports no support. ⭐ The engine implements the call
    /// fully, so if this ever moves the fix is to flip the cap and forward two
    /// floats.
    depth_bounds_refused: RefusalCounter,
    /// `pfnSetSamplePositions` — refused for two reasons:
    /// `ProgrammableSamplePositionsTier = NONE`, and the engine's own body is a
    /// `FIXME(... stub!)`.
    ///
    /// ⛔ **Expected 0**, and unlike `DepthBoundsRefused` a non-zero reading does
    /// **not** make this a cap flip away from working — the second reason has to
    /// be fixed in vkd3d first, and forwarding meanwhile would be fake success.
    sample_positions_refused: RefusalCounter,
    /// `pfnOmSetAlphaBlendFactor` — the RETIRED slot, `cl[69]`.
    ///
    /// ⛔ **Expected 0, and it is an instrument about the RUNTIME.** Microsoft
    /// withdrew the function and kept the table entry
    /// (`DDI_REFERENCE.md` §14.1.1, `VulkanOn12.md:270`), and `D12-G5` measured
    /// WARP leaving the slot NULL. A hit would mean the runtime calls a slot its
    /// own specification says is no longer valid — which is worth far more as a
    /// number than the crash a NULL would have produced.
    alpha_blend_factor_retired: RefusalCounter,
    /// The engine command list did not answer `QueryInterface` for
    /// `ID3D12GraphicsCommandList9`, so a dynamic depth-bias, strip-cut or
    /// front/back stencil-ref update was dropped.
    ///
    /// ⛔ **Expected 0.** vkd3d answers every `IID_ID3D12GraphicsCommandList*`
    /// from 0 to 10 with the same object, so a hit means the engine behind this
    /// driver is not the one it links against. It **is** reported, through
    /// `pfnSetCommandListErrorCb`: unlike the caps refusals above, this is state
    /// the application set and the driver silently lost, so the list that lost it
    /// must not go on to be submitted as if it were correct.
    list9_unavailable: RefusalCounter,
    /// `pfnIASetIndexBufferStripCutValue` named none of the three values the
    /// enumeration defines. **Expected 0.**
    index_buffer_strip_cut_unknown: RefusalCounter,
    /// `pfnIASetVertexBuffers` addressed a slot outside
    /// `D3D12_IA_VERTEX_INPUT_RESOURCE_SLOT_COUNT`. **Expected 0** — vkd3d makes
    /// the same check and answers with a `WARN` and a silent return, so this
    /// counter is what makes the dropped binding visible.
    vertex_buffers_bad_arg: RefusalCounter,
    /// `pfnSOSetTargets` addressed a slot outside `D3D12_SO_BUFFER_SLOT_COUNT`.
    /// **Expected 0** — the same range check, made the same non-overflowing way,
    /// as `VertexBuffersBadArg`.
    ///
    /// ⚠ **It no longer counts a non-zero count with a null array**; that moved
    /// to `SoTargetsNullArray`, which forwards. Read the two together when
    /// comparing this line against a build from before the §10 review: a
    /// pre-review non-zero reading here could have been either case.
    so_targets_bad_arg: RefusalCounter,
    /// `pfnOMSetRenderTargets` arrived with a count above
    /// `D3D12_SIMULTANEOUS_RENDER_TARGET_COUNT`, or with a non-zero count and a
    /// null array. **Expected 0.**
    render_targets_bad_arg: RefusalCounter,
    /// `pfnExecuteBundle`'s second `D3D12DDI_HCOMMANDLIST` did not resolve, so a
    /// whole bundle's recorded commands were dropped. **Expected 0** — the
    /// runtime only executes a bundle `pfnCreateCommandList` returned `S_OK`
    /// for.
    execute_bundle_missing: RefusalCounter,
    /// The list handed to `pfnExecuteBundle` was created as something other than
    /// `D3D12_COMMAND_LIST_TYPE_BUNDLE`, and the call was forwarded anyway.
    ///
    /// ⛔ **Expected 0.** This driver's idea of which lists are bundles comes
    /// only from `D3D12DDIARG_CREATE_COMMAND_LIST_0040::Type`
    /// (`D3D12DDIARG_CREATE_COMMAND_RECORDER_0040` carries no bundle bit), so a
    /// hit means the runtime asked for a list to be executed as a bundle that
    /// this driver did not create as one.
    ///
    /// ⚠ It is **not** the instrument for the (pool, class) question any more —
    /// an earlier grading said it was, and `bundle.c:239-245` settles that
    /// question without a run. What is left is a genuine disagreement about one
    /// list's class, which is worth a number because the engine answers it with
    /// a `WARN` and a dropped draw batch.
    execute_bundle_not_bundle: RefusalCounter,
    /// ⛔ **RETIRED BY S-4, and kept only so the `D3D12 DDI refusals:` line does
    /// not shift.** It counted the blanket `pfnExecuteIndirect` noop, which is
    /// gone: the slot forwards now, and its outcomes are
    /// `L3aExecuteIndirectForwarded` plus three per-cause refusals appended at the
    /// end of [`REFUSALS`].
    ///
    /// ⛔ **Expected 0 forever.** Nothing increments it. Not deleted because
    /// removing an entry from [`REFUSALS`] shifts every counter after it, and that
    /// array is the evidence contract diffed across builds.
    ///
    /// ⚠ Its old grading also carried a claim that is now false — *"the
    /// application's `ID3D12Device::CreateCommandSignature` already failed and it
    /// holds no signature to pass here"* — which is exactly why the new
    /// `L3aExecuteIndirectSignatureMissing` **does** report: with the create
    /// implemented, an unresolvable signature is this driver losing an object it
    /// built, not the API boundary having already answered.
    execute_indirect_refused: RefusalCounter,
    // ── appended by the §10 error-channel repair; ⛔ append only ─────────────
    /// `pfnSOSetTargets` arrived with a non-zero `NumViews` and a **null**
    /// `pViews`, and the call was **forwarded** as `SOSetTargets(start, None)`.
    ///
    /// ⚠ **Not a refusal**, and nothing was dropped that the twin slot would have
    /// kept: `pfnIASetVertexBuffers` answers the identical shape by forwarding
    /// `None` on vkd3d's own authority (*"Native drivers appear to ignore this
    /// call"*, `command.c:14556-14558`), and this counter exists so the two can
    /// stop disagreeing without the disagreement becoming invisible.
    ///
    /// ⛔ **Expected 0, and a NON-zero reading is the valuable one** — it would be
    /// the first evidence that the runtime lowers "no stream-output targets" as
    /// `(0, N, NULL)`, which `tmp/dx12/gates/G5/D-triangle.log` cannot show
    /// because it records slot names and not arguments. Until it moves, the null
    /// form is UNPROVEN rather than absent. ⚠ Read it beside `SoTargetsBadArg`,
    /// which used to absorb this case and answered it with an error report.
    so_targets_null_array: RefusalCounter,
    /// `pfnOMSetDepthBounds` with the `[0, 1]` default range — dropped.
    ///
    /// ⚠ **Expected NON-zero, at roughly one per `pfnResetCommandList`**, and a
    /// **zero** reading is the finding: it would mean the runtime's fixed 15-call
    /// state-reset block (`DDI_REFERENCE.md:3499-3504`) is not what
    /// `tmp/dx12/gates/G5/D-triangle.log:293`/`:310` measured — 7 resets, 7
    /// `pfnOMSetDepthBounds`, in a sample that never uses depth bounds.
    ///
    /// ⛔ It exists so `DepthBoundsRefused` can be graded Expected 0 and mean it.
    /// The default range changes nothing on a driver that reports
    /// `DepthBoundsTestSupported = 0`, so dropping it is exact rather than
    /// approximate — the same partition, for the same measured reason, as
    /// `L9ShadingRateDefaultDropped`.
    depth_bounds_default_dropped: RefusalCounter,
    // ── appended by S-4 (`pfnExecuteIndirect` implemented); ⛔ append only ────
    /// ⭐ **S-4's success counter: an indirect draw/dispatch was forwarded to the
    /// engine.**
    ///
    /// ⛔ **Read it beside `CommandSignatureCreated`** (L2's). A non-zero
    /// `CommandSignatureCreated` with a **zero** here is `METHOD.md` saturation
    /// criterion 6 exactly — *implemented but never exercised* — and it is the only
    /// pair of numbers that can show it: the signature create happens at engine
    /// startup, the execute happens per frame, and either can be reached without the
    /// other.
    ///
    /// ⚠ Non-zero here means the native path ran. It does **not** mean the scene is
    /// right: `CommandSignatureStateTemplateRefused` is where the root-argument
    /// classes an engine also wanted went, and those draws are absent.
    execute_indirect_forwarded: RefusalCounter,
    /// `pfnExecuteIndirect`'s `D3D12DDI_HCOMMANDSIGNATURE` carried no engine
    /// signature, so the indirect draw was dropped **and reported** through
    /// `pfnSetCommandListErrorCb`.
    ///
    /// ⛔ **Expected 0** — `queue::create_command_signature` either stores an object
    /// or fails, and the runtime only records against a signature the create
    /// returned `S_OK` for. A hit means this driver lost an object it built, which is
    /// a lifetime bug rather than a capability gap.
    execute_indirect_signature_missing: RefusalCounter,
    /// `pfnExecuteIndirect`'s **argument** buffer placement named no resource, so the
    /// call was refused and reported.
    ///
    /// ⛔ **Expected 0, and refusing is not defensive.**
    /// `d3d12_command_list_ExecuteIndirect`'s cast folds a null interface to a null
    /// `struct d3d12_resource *` (`vkd3d_private.h:1317-1318`) and then dereferences
    /// it with no test at `command.c:17880-17882` and in every `vkCmdDraw*Indirect*`
    /// call (`:17897-17905`), so forwarding a null is an access violation inside the
    /// engine — a crashed process rather than a dropped draw.
    execute_indirect_arg_buffer_missing: RefusalCounter,
    /// `pfnExecuteIndirect`'s **count** buffer handle was non-null and carried no
    /// engine resource, so the call was refused and reported.
    ///
    /// ⛔ **Expected 0.** ⚠ A *null* count-buffer handle is legal and common — it
    /// selects `vkCmdDrawIndirect` over `vkCmdDrawIndirectCount` — and does **not**
    /// touch this counter. What is counted is a named buffer this driver could not
    /// resolve, and it is refused rather than degraded: executing without the count
    /// buffer would run all `MaxCommandCount` commands instead of the `N <=
    /// MaxCommandCount` the application asked for, i.e. extra draws with garbage
    /// arguments.
    execute_indirect_count_buffer_missing: RefusalCounter,
}

pub(crate) static L3A_REFUSALS: L3aRefusals = L3aRefusals {
    command_list_missing: RefusalCounter::new("L3aCommandListMissing"),
    close_engine_failed: RefusalCounter::new("L3aCloseEngineFailed"),
    reset_bad_arg: RefusalCounter::new("L3aResetBadArg"),
    reset_flags_ignored: RefusalCounter::new("L3aResetFlagsIgnored"),
    reset_recorder_missing: RefusalCounter::new("L3aResetRecorderMissing"),
    reset_no_allocator: RefusalCounter::new("L3aResetNoAllocator"),
    reset_list_type_mismatch: RefusalCounter::new("L3aResetListTypeMismatch"),
    reset_engine_failed: RefusalCounter::new("L3aResetEngineFailed"),
    set_error_no_device: RefusalCounter::new("L3aSetErrorNoDevice"),
    set_error_cb_absent: RefusalCounter::new("L3aSetErrorCbAbsent"),
    topology_unknown: RefusalCounter::new("L3aTopologyUnknown"),
    topology_triangle_fan: RefusalCounter::new("L3aTopologyTriangleFan"),
    viewports_bad_arg: RefusalCounter::new("L3aViewportsBadArg"),
    scissor_rects_bad_arg: RefusalCounter::new("L3aScissorRectsBadArg"),
    blend_factor_defaulted: RefusalCounter::new("L3aBlendFactorDefaulted"),
    pipeline_state_unresolved: RefusalCounter::new("L3aPipelineStateUnresolved"),
    depth_bounds_refused: RefusalCounter::new("L3aDepthBoundsRefused"),
    sample_positions_refused: RefusalCounter::new("L3aSamplePositionsRefused"),
    alpha_blend_factor_retired: RefusalCounter::new("L3aAlphaBlendFactorRetired"),
    list9_unavailable: RefusalCounter::new("L3aList9Unavailable"),
    index_buffer_strip_cut_unknown: RefusalCounter::new("L3aIndexBufferStripCutUnknown"),
    vertex_buffers_bad_arg: RefusalCounter::new("L3aVertexBuffersBadArg"),
    so_targets_bad_arg: RefusalCounter::new("L3aSoTargetsBadArg"),
    render_targets_bad_arg: RefusalCounter::new("L3aRenderTargetsBadArg"),
    execute_bundle_missing: RefusalCounter::new("L3aExecuteBundleMissing"),
    execute_bundle_not_bundle: RefusalCounter::new("L3aExecuteBundleNotBundle"),
    execute_indirect_refused: RefusalCounter::new("L3aExecuteIndirectRefused"),
    so_targets_null_array: RefusalCounter::new("L3aSoTargetsNullArray"),
    depth_bounds_default_dropped: RefusalCounter::new("L3aDepthBoundsDefaultDropped"),
    execute_indirect_forwarded: RefusalCounter::new("L3aExecuteIndirectForwarded"),
    execute_indirect_signature_missing: RefusalCounter::new("L3aExecuteIndirectSignatureMissing"),
    execute_indirect_arg_buffer_missing: RefusalCounter::new("L3aExecuteIndirectArgBufferMissing"),
    execute_indirect_count_buffer_missing: RefusalCounter::new(
        "L3aExecuteIndirectCountBufferMissing",
    ),
};

/// L3a's refusal counters, printed by `crate::log_refusal_summary` at this
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
/// ⚠ The first ten are the list-lifetime pair's, in the order the Round 2 spine
/// wrote them; the recording lane **appended** its seventeen, and the §10
/// error-channel repair appended two more — 29 entries in all. Nothing was
/// reordered at either step, so every earlier `D3D12 DDI refusals:` line is still
/// a byte-for-byte prefix of a later one. ⛔ That is why `SoTargetsNullArray` and
/// `DepthBoundsDefaultDropped` are at the END rather than beside the counters
/// they split off from, which would read better and would break the diff.
pub(crate) static REFUSALS: &[&RefusalCounter] = &[
    &L3A_REFUSALS.command_list_missing,
    &L3A_REFUSALS.close_engine_failed,
    &L3A_REFUSALS.reset_bad_arg,
    &L3A_REFUSALS.reset_flags_ignored,
    &L3A_REFUSALS.reset_recorder_missing,
    &L3A_REFUSALS.reset_no_allocator,
    &L3A_REFUSALS.reset_list_type_mismatch,
    &L3A_REFUSALS.reset_engine_failed,
    &L3A_REFUSALS.set_error_no_device,
    &L3A_REFUSALS.set_error_cb_absent,
    &L3A_REFUSALS.topology_unknown,
    &L3A_REFUSALS.topology_triangle_fan,
    &L3A_REFUSALS.viewports_bad_arg,
    &L3A_REFUSALS.scissor_rects_bad_arg,
    &L3A_REFUSALS.blend_factor_defaulted,
    &L3A_REFUSALS.pipeline_state_unresolved,
    &L3A_REFUSALS.depth_bounds_refused,
    &L3A_REFUSALS.sample_positions_refused,
    &L3A_REFUSALS.alpha_blend_factor_retired,
    &L3A_REFUSALS.list9_unavailable,
    &L3A_REFUSALS.index_buffer_strip_cut_unknown,
    &L3A_REFUSALS.vertex_buffers_bad_arg,
    &L3A_REFUSALS.so_targets_bad_arg,
    &L3A_REFUSALS.render_targets_bad_arg,
    &L3A_REFUSALS.execute_bundle_missing,
    &L3A_REFUSALS.execute_bundle_not_bundle,
    &L3A_REFUSALS.execute_indirect_refused,
    &L3A_REFUSALS.so_targets_null_array,
    &L3A_REFUSALS.depth_bounds_default_dropped,
    // ⛔ APPENDED, S-4 (`pfnExecuteIndirect` implemented). One success counter and
    // three per-cause refusals. ⚠ `L3aExecuteIndirectRefused` keeps its position
    // above and is now **dead** — expected 0 forever — because removing an array
    // entry shifts every counter after it. Its doc says so.
    &L3A_REFUSALS.execute_indirect_forwarded,
    &L3A_REFUSALS.execute_indirect_signature_missing,
    &L3A_REFUSALS.execute_indirect_arg_buffer_missing,
    &L3A_REFUSALS.execute_indirect_count_buffer_missing,
];
