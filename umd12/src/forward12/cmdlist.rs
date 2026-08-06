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
//! ⚠ **S6 Round 2: all 23 slots are now filled** — 17 forward into the engine,
//! 4 refuse with a named counter and 2 are the list-lifetime pair the spine
//! landed. `PARALLEL.md` §9.2 does not call this lane done until the *noop* hit
//! counters for these slots read zero under a real workload, which only a run
//! can show; what the source can show is that none of the 23 is a noop any more.
//!
//! # ⭐ The four that do NOT forward, and why each one is honest
//!
//! | slot | counter | why |
//! |---|---|---|
//! | `pfnOMSetDepthBounds` | `L3aDepthBoundsRefused` | `caps12.rs` reports `DepthBoundsTestSupported = 0` |
//! | `pfnSetSamplePositions` | `L3aSamplePositionsRefused` | `ProgrammableSamplePositionsTier = NONE` **and** the engine's own body is a `FIXME(...) stub!` |
//! | `pfnOmSetAlphaBlendFactor` | `L3aAlphaBlendFactorRetired` | RETIRED by Microsoft; `pfnOmSetBlendFactor`'s component `[3]` replaced it |
//! | `pfnExecuteIndirect` | `L3aExecuteIndirectRefused` | its command signature comes from `pfnCreateCommandSignature`, which `queue.rs` refuses |
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
//!   line (R908), so `queue::CommandListState`, its `h_device` and its
//!   `list_type`, and `queue::recorder_allocator` could not be committed without
//!   a caller. These two are that caller, and they are the *right* one: they are
//!   the only command-list slots whose whole content is the objects `queue.rs`
//!   owns.
//! * ⭐ `pfnCloseCommandList` is the slot that **proves the error channel is
//!   needed**. `ID3D12GraphicsCommandList::Close` returns an `HRESULT` and the
//!   DDI returns `VOID` — and the DDI is handed the command-list handle and
//!   *nothing else*, no `hDevice`. Without `CommandListState::h_device` a failed
//!   `Close` would be unreportable, which is `DECISIONS.md` §7.6's problem in
//!   its sharpest form.
//! * ⛔ `pfnResetCommandList` is where the module doc of `queue.rs` says the
//!   bundle question lands: *"UNVERIFIED, and it belongs to whoever writes
//!   `pfnResetCommandList` (L3a): how a BUNDLE allocator is expressed at this
//!   DDI."* **It is answered, and the answer is that it is not expressed at all
//!   and bundles therefore do not work yet.** The DDI carries no bundle bit, the
//!   engine enforces the class pairing in both halves
//!   (`command.c:7378-7382`, `bundle.c:411-427`), so this slot refuses the
//!   mismatch instead of forwarding a call whose failure is known in advance.
//!   The fix is `queue.rs`'s, one allocator per (pool, class). See
//!   [`reset_command_list`].
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
    ID3D12CommandAllocator, ID3D12GraphicsCommandList9, D3D12_COMMAND_LIST_TYPE,
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

/// Report a device-scope failure from a command-list slot, counting the case
/// where there is no way to hear it.
///
/// ⭐ **The command-list table's only error channel.** All 75 of its slots take
/// `D3D12DDI_HCOMMANDLIST` and nothing else, and 74 of the 75 return `VOID`
/// (`DECISIONS.md` §7.6) — so a recording failure can only be reported through
/// the device-scoped `pfnSetErrorCb`, reached through the `h_device` that
/// `queue::CommandListState` exists to carry.
///
/// ⚠ Its counters are **this lane's**, not `device12`'s and not L6's, because
/// `PARALLEL.md` §9.1 puts a lane's counters in the lane's file and every lane
/// that reaches `device12::set_error` will write this same twelve lines.
/// `pso.rs`'s `set_error_if_possible` and `descriptors.rs`'s `report_error` are
/// the same function against those lanes' sets.
///
/// # Safety
/// `h_device` must be the handle `queue::CommandListState` recorded for a live
/// list, i.e. one `device12::create_device` returned `S_OK` for.
unsafe fn report_error(h_device: ddi12::D3D12DDI_HDEVICE, hr: Hresult) {
    // SAFETY: the caller guarantees a live device handle; the borrow does not
    // outlive this call.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&L3A_REFUSALS.set_error_no_device);
        return;
    };
    if !device12::set_error(dev, hr) {
        note_refusal(&L3A_REFUSALS.set_error_cb_absent);
    }
}

/// The state behind a recording slot's one and only argument, counting the
/// handle that did not resolve.
///
/// ⭐ Seventeen of this file's 21 recording slots open with exactly these four
/// lines — the other four (`pfnOMSetDepthBounds`, `pfnSetSamplePositions`,
/// `pfnOmSetAlphaBlendFactor`, `pfnExecuteIndirect`) refuse outright and never
/// touch the handle. Writing them seventeen times is how the seventeenth ends up
/// without the counter — `queue.rs`'s `lock_target` is the same move for the
/// same reason.
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
/// the engine has already rejected. `DDI_REFERENCE.md` §9.12's warning that
/// `pfnSetErrorCb` removes the device is the reason the *other* refusals in this
/// crate stay counted-only; a list that cannot be closed is exactly the case the
/// callback is for.
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
    let Err(err) = (unsafe { state.engine().Close() }) else {
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
    // SAFETY: `h_device()` is the handle `create_command_list` recorded for this
    // list, so it is a live device handle.
    unsafe { report_error(state.h_device(), hr) };
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
/// rather than merely counting: `pfnResetCommandList` returns `VOID`, so
/// `pfnSetErrorCb` is the only way to tell the runtime that the list it is about
/// to record into is not usable, and a silent count would hand the application a
/// null dereference inside its own process instead. This is a real correctness
/// failure of this driver, not a capability it declines to advertise, which is
/// the distinction `DDI_REFERENCE.md` §9.12 draws for the callback.
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
        // SAFETY: a live device handle, as `close_command_list`.
        unsafe { report_error(state.h_device(), E_INVALIDARG) };
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
        note_refusal(&L3A_REFUSALS.reset_flags_ignored);
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
                // SAFETY: a live device handle, as above.
                unsafe { report_error(state.h_device(), E_INVALIDARG) };
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
                // SAFETY: a live device handle, as above.
                unsafe { report_error(state.h_device(), E_FAIL) };
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
        // SAFETY: a live device handle, as above. ⛔ Reported, not merely
        // counted: the list is left unreset, and recording into an unreset
        // bundle dereferences a NULL allocator inside the engine
        // (bundle.c:253-267), so the runtime has to be told the list is unusable.
        unsafe { report_error(state.h_device(), E_INVALIDARG) };
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
    // SAFETY: a live device handle, as above.
    unsafe { report_error(state.h_device(), hr) };
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
        // SAFETY: `h_device()` is the handle `create_command_list` recorded for
        // this list, so it is a live device handle.
        unsafe { report_error(state.h_device(), E_INVALIDARG) };
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
        // SAFETY: a live device handle, as `ia_set_topology`.
        unsafe { report_error(state.h_device(), E_INVALIDARG) };
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
        // SAFETY: a live device handle, as `ia_set_topology`.
        unsafe { report_error(state.h_device(), E_INVALIDARG) };
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
        note_refusal(&L3A_REFUSALS.blend_factor_defaulted);
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

/// `pfnOMSetDepthBounds` — **REFUSED**, `L3aDepthBoundsRefused`.
///
/// ⛔ `caps12.rs:571` reports `DepthBoundsTestSupported = 0`, so no pipeline
/// this driver advertises can perform a depth-bounds test and there is nothing
/// for a bounds value to affect. Same shape and same reasoning as
/// `queue::update_tile_mappings`: counted, and deliberately **not** raised
/// through `pfnSetErrorCb`, because a hit means a caps inconsistency somewhere
/// else and removing the device would not fix it.
///
/// ⚠ **The engine is not the limit here** — `d3d12_command_list_OMSetDepthBounds`
/// is fully implemented (`command.c:18047-18061`). So the whole cost of turning
/// this on is `caps12.rs` reporting the cap and this body forwarding two floats;
/// the counter is what says whether any workload ever wanted it.
///
/// # Safety
/// Trivially safe: no argument is dereferenced. Declared `unsafe` because the
/// DDI's PFN typedef is.
unsafe extern "C" fn om_set_depth_bounds(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    _min: ddi12::FLOAT,
    _max: ddi12::FLOAT,
) {
    note_refusal(&L3A_REFUSALS.depth_bounds_refused);
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
        // SAFETY: a live device handle, as `ia_set_topology`. The state was
        // dropped, which is a correctness failure and not a declined capability.
        unsafe { report_error(state.h_device(), E_FAIL) };
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
        // SAFETY: a live device handle, as `om_set_front_and_back_stencil_ref`.
        unsafe { report_error(state.h_device(), E_FAIL) };
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
        // SAFETY: a live device handle, as `ia_set_topology`.
        unsafe { report_error(state.h_device(), E_INVALIDARG) };
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
/// ⛔ The bound is `D3D12_SO_BUFFER_SLOT_COUNT` (4), and the null check is not
/// optional here the way it is for vertex buffers: vkd3d's `SOSetTargets`
/// dereferences `views[i]` with **no** null test
/// (`command.c:14641-14660`), so a non-zero count with a null array would fault
/// inside the engine.
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
    if start >= limit || n > limit - start || (n != 0 && views.is_null()) {
        note_refusal(&L3A_REFUSALS.so_targets_bad_arg);
        if let Some(k) = record_budget() {
            log_error!(
                "SOSetTargets: StartSlot={start_slot} NumViews={num_views} pViews={views:p} -- \
                 refused against the {limit} stream-output slots D3D12 defines (x{})",
                k + 1,
            );
        }
        // SAFETY: a live device handle, as `ia_set_topology`.
        unsafe { report_error(state.h_device(), E_INVALIDARG) };
        return;
    }
    let slice: Option<&[D3D12_STREAM_OUTPUT_BUFFER_VIEW]> = if views.is_null() {
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
        // SAFETY: a live device handle, as `ia_set_topology`.
        unsafe { report_error(state.h_device(), E_INVALIDARG) };
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
        // SAFETY: a live device handle, as `ia_set_topology`.
        unsafe { report_error(state.h_device(), E_INVALIDARG) };
        return;
    };
    let Some(list9) = engine_list9(state) else {
        // SAFETY: a live device handle, as `om_set_front_and_back_stencil_ref`.
        unsafe { report_error(state.h_device(), E_FAIL) };
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
/// ⚠ **This slot cannot work while `pfnResetCommandList` refuses bundles.** A
/// bundle this driver never reset has an empty command chain, so the forward is
/// a no-op; see [`reset_command_list`] for why, and for whose file the fix is
/// in.
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
        // SAFETY: a live device handle, as `ia_set_topology`.
        unsafe { report_error(state.h_device(), E_INVALIDARG) };
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

/// `pfnExecuteIndirect` — **REFUSED**, `L3aExecuteIndirectRefused`.
///
/// # ⛔ It is refused because its command signature does not exist
///
/// `ID3D12GraphicsCommandList::ExecuteIndirect` takes an
/// `ID3D12CommandSignature`, and this driver has none:
/// `queue::create_command_signature` returns `E_NOTIMPL` and counts
/// `CommandSignatureRefused`, so every `D3D12DDI_HCOMMANDSIGNATURE` that reaches
/// this slot carries a null slot word. ⛔ **The two are one capability and they
/// land together or not at all** — a lane that implemented this half alone would
/// have nothing to pass as the first argument.
///
/// ⚠ `queue.rs` routed the create's missing half here by name: the
/// `D3D12DDI_INDIRECT_ARGUMENT_DESC` -> `D3D12_INDIRECT_ARGUMENT_DESC`
/// translation *"deserves the lane that also implements `pfnExecuteIndirect`
/// (L3a) and can test it"*. This lane's answer is that it cannot test it, for a
/// structural reason rather than a scheduling one: `pfnCreateCommandSignature`
/// lives in `queue.rs`, which `PARALLEL.md` §4 gives to L2, and the accessor
/// budget for this round is one function in `pso.rs`. Implementing the create
/// from here would be the second declaration of a handle's payload that
/// `DECISIONS.md` D13 exists to prevent. **The pair is one commit, in `queue.rs`
/// plus this file, and it is the integrator's to schedule.**
///
/// ⚠ Nothing on the critical path is lost: `DDI_REFERENCE.md` §14.2's 99-slot
/// minimum-viable list contains neither this slot nor the command-signature
/// triple.
///
/// # Safety
/// Trivially safe: no argument is dereferenced. The two
/// `D3D12DDIARG_BUFFER_PLACEMENT`s arrive **by value** and hold nothing that is
/// freed, so ignoring them leaks nothing.
unsafe extern "C" fn execute_indirect(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    _h_signature: ddi12::D3D12DDI_HCOMMANDSIGNATURE,
    _max_command_count: ddi12::UINT,
    _argument_buffer: ddi12::D3D12DDIARG_BUFFER_PLACEMENT,
    _count_buffer: ddi12::D3D12DDIARG_BUFFER_PLACEMENT,
) {
    note_refusal(&L3A_REFUSALS.execute_indirect_refused);
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
    /// ⚠ It is deliberately **not** reported through `pfnSetErrorCb`: with no
    /// state there is no `h_device` to report against, which is the one failure
    /// this table cannot escalate.
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
    /// ⛔ **Expected 0 — and GUARANTEED non-zero the moment a workload records a
    /// BUNDLE, in which case it is a defect report about this driver rather than
    /// a measurement.** ⚠ Its grading changed in the recording lane, and the reason
    /// is the standing lesson that a counter's grading is a claim which goes
    /// stale: it previously read *"a non-zero reading without `ResetEngineFailed`
    /// moving is the good outcome — it would mean vkd3d does not enforce the class
    /// pairing"*, and the engine source settles that without a run. It enforces,
    /// in both halves — `command.c:7378-7382` for a regular list,
    /// `bundle.c:239-245` + `bundle.c:411-427` (vtable identity) for a bundle — so
    /// the outcome that grading hoped for cannot happen and the pair can never
    /// move apart. See [`reset_command_list`]; the fix is one allocator per
    /// (pool, class) in `queue.rs`, and until it lands a bundle cannot be
    /// recorded at all.
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
    /// A command-list slot needed to report a device-scope error and the device
    /// handle did not resolve. **Expected 0.**
    set_error_no_device: RefusalCounter,
    /// A command-list slot needed `pfnSetErrorCb` and there was none.
    ///
    /// ⛔ **Expected 0.** It is the first member of
    /// `D3D12DDI_CORELAYER_DEVICECALLBACKS_0062` and the only error channel this
    /// whole table has, so a hit means a recording failure the runtime will never
    /// learn about.
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
    blend_factor_defaulted: RefusalCounter,
    /// `pfnSetPipelineState` was given a non-null `D3D12DDI_HPIPELINESTATE`
    /// whose slot is empty, i.e. L6's `pfnCreatePipelineState` refused it, and
    /// `NULL` was forwarded to the engine instead.
    ///
    /// ⛔ **Expected 0, and read it beside `L6PsoEngineFailed`** — this counter
    /// is the *consequence* of that one, one DDI later, and it is the point at
    /// which a failed PSO create turns into a draw that cannot work. It is
    /// deliberately not reported through `pfnSetErrorCb`: L6 already reported
    /// the create, and removing the device twice for one failure would hide
    /// which half was first.
    pipeline_state_unresolved: RefusalCounter,
    /// `pfnOMSetDepthBounds` — refused, coherently with
    /// `DepthBoundsTestSupported = 0` in `caps12.rs`.
    ///
    /// ⚠ **Expected 0**, and a hit is a finding about the *caps*, not about this
    /// slot: it would mean an application reached a depth-bounds path on a
    /// driver that reports no support. ⭐ The engine implements the call fully,
    /// so if this ever moves the fix is to flip the cap and forward two floats.
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
    /// driver is not the one it links against. It **is** reported through
    /// `pfnSetErrorCb`: unlike the caps refusals above, this is state the
    /// application set and the driver silently lost.
    list9_unavailable: RefusalCounter,
    /// `pfnIASetIndexBufferStripCutValue` named none of the three values the
    /// enumeration defines. **Expected 0.**
    index_buffer_strip_cut_unknown: RefusalCounter,
    /// `pfnIASetVertexBuffers` addressed a slot outside
    /// `D3D12_IA_VERTEX_INPUT_RESOURCE_SLOT_COUNT`. **Expected 0** — vkd3d makes
    /// the same check and answers with a `WARN` and a silent return, so this
    /// counter is what makes the dropped binding visible.
    vertex_buffers_bad_arg: RefusalCounter,
    /// `pfnSOSetTargets` addressed a slot outside `D3D12_SO_BUFFER_SLOT_COUNT`,
    /// or passed a non-zero count with a null array. **Expected 0** — and the
    /// null case is not merely invalid, it would fault inside the engine, which
    /// dereferences the array with no null test.
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
    /// `pfnExecuteIndirect` — refused, because `pfnCreateCommandSignature`
    /// refuses.
    ///
    /// ⛔ **Expected 0 while the pair is unimplemented, and it is the trigger for
    /// implementing them.** The two are one capability: this slot's first
    /// argument is the object that create would have built, so neither half is
    /// useful alone. A non-zero reading means a workload actually issues
    /// indirect draws, which is the evidence that would justify the
    /// `D3D12DDI_INDIRECT_ARGUMENT_DESC` translation `queue.rs` deferred.
    execute_indirect_refused: RefusalCounter,
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
/// wrote them; the recording lane **appended** its seventeen — 27 entries in all
/// — and reordered nothing, so every pre-recording `D3D12 DDI refusals:` line is
/// still a byte-for-byte prefix of a post-recording one.
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
];
