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
//! ⚠ **S6 Round 2: this lane has not landed, with TWO exceptions.** Everything
//! else carries the per-slot counting noops `forward12::noop12` installed, so it
//! is non-NULL and every hit is named, counted and printed by `D3D12 noop DDI
//! hits:`. `PARALLEL.md` §9.2 does not call this lane done until those counters
//! read **zero** under a real workload.
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
//! * ⚠ `pfnResetCommandList` is where the module doc of `queue.rs` says the
//!   bundle question lands: *"UNVERIFIED, and it belongs to whoever writes
//!   `pfnResetCommandList` (L3a): how a BUNDLE allocator is expressed at this
//!   DDI."* It is answered here as far as this DDI allows — by **naming the
//!   mismatch before making the call** — and left open where it is genuinely
//!   open. See [`reset_command_list`].
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
use windows::Win32::Graphics::Direct3D12::{ID3D12CommandAllocator, D3D12_COMMAND_LIST_TYPE};

use super::queue::{self, RecorderAllocator};
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

/// Returns the occurrence ordinal (0-based) when the line should be emitted.
fn budget() -> Option<usize> {
    LIST_LOG.first_n_then_every(8, 4096)
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
/// # ⛔ The bundle question, answered as far as this DDI allows
///
/// `queue.rs`'s module doc records it as UNVERIFIED and routes it here: a
/// `D3D12_COMMAND_LIST_TYPE_BUNDLE` list can only be `Reset` against a **BUNDLE**
/// allocator, `ID3D12CommandAllocator`'s class is fixed at creation, and
/// `D3D12DDIARG_CREATE_COMMAND_RECORDER_0040` carries only `QueueFlags`
/// (3D / COMPUTE / COPY / PAGING / video) with **no bundle bit anywhere** — so
/// the recorder behind a bundle is indistinguishable from one behind a DIRECT
/// list and `queue.rs` materialises a DIRECT allocator for it.
///
/// What this slot adds is that the mismatch is **named before the call**, using
/// `CommandListState::list_type`, so the symptom is `ResetListTypeMismatch` and a
/// line that says *bundle* rather than an opaque `E_INVALIDARG` out of the
/// engine. ⛔ The call is still made: whether vkd3d enforces the class pairing is
/// not something this driver should assume, and a run that succeeds anyway is
/// evidence the counter would otherwise hide. The fix, if the engine does
/// enforce it, is one allocator per (pool, class) — which is the same fix
/// `queue.rs`'s `PoolTypeMismatch` already names, arrived at from the other
/// direction.
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

    // ⛔ The class check, before the call. See the doc above. ⚠ The allocator's
    // class is **carried** rather than asked for: `ID3D12CommandAllocator`
    // exposes no `GetType`, which is why `queue::PoolAllocator` pairs the two.
    if allocator_type != state.list_type() {
        note_refusal(&L3A_REFUSALS.reset_list_type_mismatch);
        if let Some(n) = budget() {
            log_error!(
                "ResetCommandList: this list is class {} and its recorder's allocator is class \
                 {} -- D3D12DDIARG_CREATE_COMMAND_RECORDER_0040 carries no bundle bit, so a \
                 bundle's recorder is indistinguishable from a DIRECT one; forwarding anyway to \
                 measure whether the engine enforces the pairing (x{})",
                state.list_type().0,
                allocator_type.0,
                n + 1,
            );
        }
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
// Install
// ---------------------------------------------------------------------------

/// Install L3a's 23 command-list slots.
///
/// Chain position: `Stubbed` -> `RecordSlots` on the command-list table.
pub(crate) fn install(
    mut filling: Filling<'_, CommandListTable, stage::Stubbed>,
) -> Filling<'_, CommandListTable, stage::RecordSlots> {
    let table = filling.table();
    // ⚠ 2 of this lane's 23. The other 21 keep their counting noops; see the
    // module doc for why these two could not wait for the rest of the lane.
    table.pfnCloseCommandList = Some(close_command_list);
    table.pfnResetCommandList = Some(reset_command_list);
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
    /// The list's class and its recorder's allocator's class disagree, and the
    /// `Reset` was forwarded anyway.
    ///
    /// ⚠ **Expected non-zero exactly when a workload records a BUNDLE**, and it
    /// is the instrument for `queue.rs`'s named UNVERIFIED: the DDI's recorder
    /// create args carry no bundle bit, so this driver cannot mint a BUNDLE
    /// allocator for one. A non-zero reading *without* `ResetEngineFailed`
    /// moving is the good outcome — it would mean vkd3d does not enforce the
    /// class pairing. The two moving together is the finding, and the fix is one
    /// allocator per (pool, class).
    reset_list_type_mismatch: RefusalCounter,
    /// `ID3D12GraphicsCommandList::Reset` failed, and the failure **was** raised
    /// to the runtime. **Expected 0.** ⚠ Read it beside
    /// `ResetListTypeMismatch`: together they are the bundle answer, and alone it
    /// is an allocator the GPU is not done with, which is the application's
    /// obligation rather than this driver's.
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
/// ⚠ Holds the two list-lifetime slots' counters only, because those are the two
/// slots that have landed. The lane that takes the other 21 **appends** to this
/// array and never reorders it.
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
];
