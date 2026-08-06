//! L2 — command queues, pools, recorders and command-list lifetime.
//!
//! Owns 17 of `DEVICE_FUNCS_CORE_0109` (group (d)) **and all 7** of
//! `COMMAND_QUEUE_FUNCS_CORE_0001`.
//!
//! ⚠ The three device-side queue entry points are members **27, 28 and 29** of
//! the 124 (`d3d12umddi.h:13488-13490`). ⛔ NOT "slots 38-40" — that was a `sed`
//! line offset inside the struct misread as a member index (`DECISIONS.md`
//! §4.1), and it is exactly the kind of number `noop12`'s offset proof now makes
//! unrepresentable.
//!
//! # ⭐⭐ The pool / recorder / list split, and the mapping this lane chose
//!
//! D3D12's **API** has `ID3D12CommandAllocator` + `ID3D12GraphicsCommandList`.
//! The **DDI** at `_0040` and later has three objects (`DDI_REFERENCE.md` §8.1),
//! and the create args are what decide the mapping:
//!
//! | DDI object | its create args carry | so it maps to |
//! |---|---|---|
//! | pool (`D3D12DDIARG_CREATE_COMMAND_POOL_0040`) | `PoolFlags` — **one enum with one value, `NONE`** | an `ID3D12CommandAllocator`, but its **type is not knowable here** |
//! | recorder (`D3D12DDIARG_CREATE_COMMAND_RECORDER_0040`) | `QueueFlags`, `RecorderFlags` | **no engine object at all** |
//! | list (`D3D12DDIARG_CREATE_COMMAND_LIST_0040`) | `Type` (DIRECT/BUNDLE), `QueueFlags`, `ID`, `CommandListFlags`, `NodeMask` | an `ID3D12GraphicsCommandList` |
//!
//! ⭐ **The pool cannot create its allocator at `pfnCreateCommandPool`**, and
//! that is the one non-obvious consequence of the shape above.
//! `ID3D12Device::CreateCommandAllocator` takes a `D3D12_COMMAND_LIST_TYPE`; the
//! pool's create args are a single flags word that carries no type, no size and
//! no pointer. The only DDI that ever brings a queue class into contact with a
//! pool is `pfnCommandRecorderSetCommandPoolAsTarget(hDevice, hRecorder,
//! hPool)`, whose recorder *does* carry `QueueFlags`. ⇒ **this lane creates the
//! allocator lazily, at the first bind, from the binding recorder's class**, and
//! counts a later bind by a recorder of a *different* class (`PoolTypeMismatch`)
//! rather than silently reusing an allocator that cannot back the new class —
//! an `ID3D12CommandAllocator`'s type is fixed at creation.
//!
//! ⚠ `DDI_REFERENCE.md` §9.3's mapping table writes this row as
//! *"`pfnCreateCommandPool` → `ID3D12Device::CreateCommandAllocator(type)`"* and
//! does not say where `type` comes from. It comes from the recorder, one DDI
//! later; the lazy creation is that gap closed, not a deviation from the table.
//!
//! ⭐ **The recorder has no engine object behind it, and that is a legitimate
//! answer rather than a stub.** vkd3d fuses "the recording engine" into
//! `ID3D12GraphicsCommandList` itself, so a DDI recorder is exactly a
//! driver-side record of *which pool a subsequent `pfnResetCommandList` should
//! draw its allocator from* — which is why `D3D12DDIARG_RESETCOMMANDLIST_0040`
//! carries `hDrvCommandRecorder` and nothing else about memory. §9.3 says the
//! same in one line: *"`pfnCreateCommandRecorder` → no vkd3d object — a
//! Helios-side shadow naming its current pool."*
//!
//! ⭐ **A command list is created with `ID3D12Device4::CreateCommandList1`**, not
//! with `CreateCommandList` + `Close()`. `D3D12DDIARG_CREATE_COMMAND_LIST_0040`
//! names **no pool and no recorder** — those arrive at
//! `pfnResetCommandList` — so at create time there is no allocator to pass, and
//! `CreateCommandList1` is the D3D12 entry point that exists for exactly that:
//! it returns a **closed** list bound to no allocator. §9.3's *"then immediately
//! `Close()`"* describes the same end state reached the long way round, and the
//! long way round is not available here.
//!
//! ⛔ **UNVERIFIED, and it belongs to whoever writes `pfnResetCommandList`
//! (L3a): how a BUNDLE allocator is expressed at this DDI.** A D3D12 bundle
//! list can only be `Reset` against a `D3D12_COMMAND_LIST_TYPE_BUNDLE`
//! allocator, but `D3D12DDIARG_CREATE_COMMAND_RECORDER_0040` carries only
//! `QueueFlags` (3D / COMPUTE / COPY / PAGING / video) and a `RecorderFlags`
//! enum whose single value is `NONE` — **there is no bundle bit anywhere in the
//! recorder's create args**, and the pool's are one flags word. So a recorder
//! that will back a bundle is indistinguishable here from one that will back a
//! DIRECT list, and this lane materialises a DIRECT allocator for it. ⚠ Note the
//! `PoolTypeMismatch` counter cannot catch this: both recorders map to DIRECT,
//! so the classes agree and nothing fires. The symptom would be
//! `ID3D12GraphicsCommandList::Reset` failing for bundles only. Settling it
//! needs a workload that records a bundle, which is `D12-G8`'s territory, not a
//! header question.
//!
//! # ⚠ Where the WDDM context is minted, and why the answer is "here"
//!
//! `PARALLEL.md` §4 and `ARCHITECTURE.md` §1.2 step 19 both put it in this lane
//! — one WDDM context per `ID3D12CommandQueue`, not one per device. That is not
//! taken on faith; three things settle it, and one of them is decisive.
//!
//! 1. ⛔ **Decisive: it can be minted nowhere else, ever.** The runtime enforces
//!    the scoping and says so in its own words —
//!    *"CreateContextCb or CreateContextVirtualCb called outside of queue
//!    creation."* (fullstrings:10597), and
//!    *"The driver must only pass DXGK context handles that were created during
//!    the command queue creation."* (`ResourceHeaps.md:1678`, quoted at
//!    `DDI_REFERENCE.md` §8.2). A lane that skips it does not defer the work; it
//!    makes the object unobtainable for every later lane.
//! 2. **`pfnPresent` has an `hContext` OUT-parameter.**
//!    `D3D12DDI_PRESENT_CONTEXTS_0051::hContext` (`PRESENT.md` §3.2) is a WDDM
//!    context handle the driver reports, and L8 has no other source for it.
//! 3. **Helios' D3D11 driver already proves what the context is FOR here**, and
//!    it is *not* rendering. Grepping every use of `HeliosDevice::context`
//!    (`umd/src/forward.rs:479`, `umd/src/forward/present.rs:786`, `:1126`,
//!    `:2247`, `:1609`) finds present and only present: the handle feeds
//!    `pfnPresentCb`, and the context's command-buffer window carries the
//!    `HeliosPresentRenderCmd` identity record submitted through `pfnRenderCb`.
//!    The actual GPU work never touches it — it goes out-of-band over the ICD's
//!    venus escape. So the D3D12 change really is *cardinality, not kind*
//!    (`DDI_REFERENCE.md` §6.4), and the reason to mint it per queue is that the
//!    runtime will not accept it anywhere else.
//!
//! # ⛔⛔ Legacy `pfnCreateContextCb` — a DECISION, its cost, and the doc-set
//! contradiction it resolves
//!
//! ⚠ **This is not settled doctrine; the doc set disagrees with itself and this
//! file picks a side.** Do not read the choice below as inherited.
//!
//! **What the documents actually say**, both halves:
//!
//! * `DDI_REFERENCE.md` §6.4's contract paragraph and §9.2's forward-mapping line
//!   both name **`pfnCreateContextCb`** — *"`pfnCreateCommandQueue` → one
//!   `ID3D12Device::CreateCommandQueue` **plus** one `pfnCreateContextCb`"*;
//! * `DECISIONS.md` D5 and §9.2's own NodeOrdinal paragraph both write
//!   **`D3DDDICB_CREATECONTEXTVIRTUAL`** for where the D3D12 UMD picks its node;
//! * `PRESENT.md` §12 **U12** (the row at `PRESENT.md:1832`) records the whole
//!   question as **UNVERIFIED**: *"Which context-creation callback the D3D12 UMD
//!   ends up on, and whether `DxgkDdiRender` fires for `pfnRenderCb` on a
//!   `VirtualAddressing` context"*, with D5's virtual reading named in the same
//!   row. ⛔ It is **`PRESENT.md` §12**, not `ARCHITECTURE.md` §13 — that
//!   document's list ends at UNVERIFIED-11 (single physical adapter) and has no
//!   U12. Its settling experiment is unchanged: `RENDER_COUNT`
//!   (`kmd_render/src/ddi/submit_command.rs:996`) moving on the D3D12 path, which
//!   cannot happen until L8's `pfnRenderCb` half lands.
//!
//! **This lane mints the legacy context**, matching D3D11
//! (`umd/src/device_funcs.rs:998-1048`), for one mechanical reason:
//! `DECISIONS.md` P-C carries the per-present identity on a `pfnRenderCb` Render
//! command around `pfnPresent`, landing in the KMD's **PASSIVE**
//! `dxgkddi_render` path with no KMD change — and `pfnRenderCb` writes into the
//! context's command-buffer window, which `D3DDDICB_CREATECONTEXTVIRTUAL` does
//! not have (`KMD_IMPACT.md:314-322`: `NodeOrdinal`, `EngineAffinity`, `Flags`,
//! private data, `hContext`, and no windows at all). ⛔ P-C's rejection of
//! (ii′) is *not* a rejection of virtual contexts — it forbids designing a new
//! `DxgkDdiSubmitCommandVirtual` **decode** of the present identity, because that
//! DDI runs at DISPATCH_LEVEL where the stash machinery's `diag::record*` is
//! illegal (`PRESENT.md` §8.3's table).
//!
//! ## ⛔ THE COST, stated here so no later lane has to discover it
//!
//! **`pKTCallbacks->pfnSubmitCommandCb` is off the table for this driver.**
//! `DDI_REFERENCE.md` §6.4's submission row scopes it to *"(GPU-VA contexts)"*
//! and names `pfnRenderCb` as the legacy alternative on the same line; MS Learn
//! describes it as submitting command buffers *"on contexts that support GPU
//! virtual addressing"* (§8.2). ⇒ the WDDM half of `pfnExecuteCommandLists`
//! (§8.2/§8.3, `ResourceHeaps.md:1678`) must be **`pfnRenderCb` on this same
//! legacy context**, which is the door P-C already needs for the present
//! identity. Costing that work as "the watermark is missing" is wrong by one
//! callback: the callback is decided here.
//!
//! ⚠ **And it is decided here irreversibly.** A context can be minted *only*
//! inside `pfnCreateCommandQueue` (the runtime says so — see the enforcement
//! quotes above), so the class is not something a later lane can change in its
//! own file: switching to `pfnCreateContextVirtualCb`, or minting a second
//! virtual context alongside this one, is a re-open of
//! [`create_command_queue`]. Both are legal inside queue creation —
//! `D3DDDICB_SUBMITCOMMAND::BroadcastContext`'s validation implies several
//! contexts may belong to one queue — but a second context nothing reads would
//! be dead state (`PARALLEL.md` §10) and a second mandatory mint would double the
//! `QueueContextFailed` risk at the very gate this lane is written for. The
//! evidence that would flip it is U12's, not an argument.
//!
//! ⛔ **The three context windows ARE stored — this reverses the earlier round's
//! decision, and the reversal is FB-1** (`KMD_IMPACT.md` §14a.2).
//! `D3DDDICB_CREATECONTEXT` returns the command-buffer / allocation-list /
//! patch-location windows alongside `hContext`, and the D3D11 driver keeps all
//! three because its present writes into them (`umd/src/device_funcs.rs:144-151`,
//! `:1036-1046`). This file used to log and drop them, correctly, on the argument
//! that a stored window nothing loads is the T5 anti-pattern (*an instrument
//! nothing can read is not an instrument*) and that `PARALLEL.md` §10 forbids
//! `#[allow(dead_code)]` on a hand-written line. What changed is not the argument
//! but the premise: there are now **two** readers, and the first of them is on
//! this file's critical path rather than L8's.
//!
//! [`ContextWindows`] holds them, behind a `Mutex`. Every `pfnRenderCb` on this
//! context must then re-latch them from that callback's own out-fields, through
//! **one** shared method — which lands with its first caller rather than ahead of
//! it, because §10 forbids the `#[allow(dead_code)]` the alternative needs and
//! the compiler settled it in one line (`method re_latch is never used`). The two
//! readers, in landing order:
//!
//! 1. the **fence carrier** — `pfnExecuteCommandLists`' WDDM submission
//!    (`KMD_IMPACT.md` §14a.2 K-F1), which is what makes the application's
//!    `ID3D12Fence` order behind the frame's own work at all;
//! 2. the **present identity** — `DECISIONS.md` P-C's `HeliosPresentRenderCmd`
//!    around `pfnPresent` (§14a.3 UP-9), which writes a 72-byte record into the
//!    same command window through the same helper.
//!
//! ⚠ Which is why the latch is general rather than shaped to the first caller:
//! §14a.4 point 2 says in as many words that FB-1 is *"shared by both
//! `pfnRenderCb` users. Land it once, in the fence work."*
//!
//! # ⚠ What `pfnExecuteCommandLists` does NOT do yet
//!
//! `DDI_REFERENCE.md` §8.2/§8.3: the driver must submit to the kernel **during**
//! `pfnExecuteCommandLists`, from the thread that entered the DDI, with a DXGK
//! context minted at queue creation. This lane forwards to the engine and does
//! **not** submit to WDDM, because step 2 of §8.3 — obtaining a monotonic
//! completion watermark for *that* submission — is the piece §8.3 itself records
//! as having no existing answer, and it belongs with the present path.
//! `EclNoWddmSubmission` counts every such forward, so the gap is a number rather
//! than a silence. ⛔ The invariant that governs whoever closes it is unchanged:
//! *never signal a wire fence before host completion.*
//!
//! ⛔ **Two pieces are missing, not one, and the second one is already decided.**
//! The watermark is open. The *callback* is not: this queue carries a **legacy**
//! context, so the submission is `pfnRenderCb` and **not**
//! `pKTCallbacks->pfnSubmitCommandCb` — see the context section above for why
//! that is a property of `pfnCreateCommandQueue` and cannot be chosen in the
//! file that closes `EclNoWddmSubmission`.

use core::ffi::c_void;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use helios_umd_common::hr::{Hresult, E_FAIL, E_INVALIDARG, E_NOTIMPL, S_OK};
use helios_umd_common::refusals::RefusalCounter;
use helios_umd_common::slot::{Boxed, Com, DdiHandle, Slot};
use helios_umd_common::throttle::LogThrottle;
// FB-1. ⚠ The shared type, not a second copy: `umd_common/src/window.rs:6-9`
// records the D3D12 case as the reason it is in the shared crate at all — *"a
// D3D12 forwarder that calls `pfnRenderCb` handles the same pointer/size
// pairs"*. D3b forbids copying from `umd/`, and this is what that leaves.
use helios_umd_common::window::Window;
// ⚠ Imported for `Interface::cast` (the `QueryInterface` that reaches
// `ID3D12Device4`), which is a trait method and therefore invisible to method
// resolution unless the trait is in scope.
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::{
    ID3D12CommandAllocator, ID3D12CommandList, ID3D12CommandQueue, ID3D12Device4,
    ID3D12GraphicsCommandList, D3D12_COMMAND_LIST_FLAG_NONE, D3D12_COMMAND_LIST_TYPE,
    D3D12_COMMAND_LIST_TYPE_COMPUTE, D3D12_COMMAND_LIST_TYPE_COPY,
    D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_COMMAND_QUEUE_DESC,
    D3D12_COMMAND_QUEUE_FLAG_NONE,
};

use super::fence;
use super::tables12::{self, stage, CommandQueueTable, DeviceCoreTable, Filling};
use crate::{ddi12, device12, log_error, note_refusal, trace_line};

// ---------------------------------------------------------------------------
// Handle payloads
// ---------------------------------------------------------------------------

// ⛔ **The payload is a property of the HANDLE TYPE, declared once, here.**
// `ARCHITECTURE.md` §12 rule 7 / R803: choosing the payload at the call site
// compiled and produced a `ManuallyDrop` whose vtable pointer was a struct field
// — a wild call on first use.
//
// ⚠ `D3D12DDI_HCOMMANDLIST` is declared **in this file** even though L3a
// (`cmdlist.rs`) owns the 23 recording slots that read it, because this lane owns
// the list's *lifetime* — `pfnCreateCommandList` is what puts a value in the slot
// and `pfnDestroyCommandList` is what takes it out. One declaration, in the lane
// that writes it.
//
// ⭐ **It was a bare `com_handles!` word until S6 Round 2, and what promoted it
// was not shadow state but an ERROR CHANNEL.** Every one of the 75 command-list
// slots takes `D3D12DDI_HCOMMANDLIST` and **nothing else** — no `hDevice`
// (`pfnDrawInstanced`, `d3d12umddi.h`), and 74 of the 75 return `VOID`. So a
// recording DDI that fails cannot report through its return value and cannot
// reach a callback from its arguments alone: it needs something the create-time
// handles carry. `pfnCreateCommandList` is the one DDI in the list's whole life
// that is handed both the device handle and the runtime's list handle, so it is
// the only place either can be captured. ⇒ [`CommandListState`], and the
// promotion is this lane's to make (`PARALLEL.md` §4 gives it the handle) on
// behalf of L3a/L3b/L3c/L8, which all four need it.
//
// ⛔ **The first version of this comment said the channel was `pfnSetErrorCb`,
// "which is device-scoped", and called it the only one. That was WRONG**, and
// three lanes copied the sentence into 49 call sites before the `PARALLEL.md`
// §10 review opened the header. `pfnSetCommandListErrorCb` sits one field below
// `pfnSetErrorCb` in the same `_0062` struct this file already reads
// `pfnSetCommandListDDITableCb` out of; it quarantines one list where the device
// callback removes the whole device. `device12::set_command_list_error` has the
// full account. That is why [`CommandListState`] carries `h_rt_list` as well as
// `h_device`.
helios_umd_common::boxed_handles!(
    crate::ddi12::D3D12DDI_HCOMMANDLIST => CommandListState,
    crate::ddi12::D3D12DDI_HCOMMANDQUEUE => QueueState,
    crate::ddi12::D3D12DDI_HCOMMANDPOOL_0040 => PoolState,
    crate::ddi12::D3D12DDI_HCOMMANDRECORDER_0040 => RecorderState,
);

/// The private-block size every `CalcPrivate*` in this lane returns.
///
/// ⛔ **One machine word, and never 0.** Same rule and same reasoning as
/// `fence::PRIVATE_SLOT_SIZE`: `umd_common::slot`'s encoding is one word inside
/// driver-sized private memory, the shipping D3D11 driver answers 8 from every
/// real `CalcPrivate*Size` (`umd/src/forward/queries.rs:9-14`), and a 0 would
/// make the runtime hand the paired `Create` a zero-byte region to write through
/// — `umd/src/device_funcs.rs:708-723`, where a transient 0 produced heap
/// corruption surfacing as a wild call inside a 3DMark worker.
const PRIVATE_SLOT_SIZE: usize = core::mem::size_of::<*mut c_void>();

// ---------------------------------------------------------------------------
// Log budgets
// ---------------------------------------------------------------------------

// ⛔ **`log_error!` is unbounded by construction** — `umd_common/src/log.rs:279`
// formats and writes on **every** call — and T2 measured what one unbounded UMD
// log site costs: ~9k mutex-serialized writes per second from a single per-call
// line (`umd/src/device_funcs.rs:713-723`). Every site in this file that can
// repeat with the workload goes through one of these budgets: the first 8, then
// every 4096th, with the ordinal printed so a suppressed burst is still visible
// as a jump.
//
// ⚠ Grouped **per object family** rather than per call site. A burst is a
// property of the path that produces it (every queue create failing, every
// submit failing), not of which of that path's four lines fired, and one budget
// per family keeps the failure legible instead of interleaving four independent
// countdowns.

/// Budget for the queue-lifetime lines, including the `CreateContext` capture.
static QUEUE_LOG: LogThrottle = LogThrottle::new();
/// Budget for the command-pool lines.
static POOL_LOG: LogThrottle = LogThrottle::new();
/// Budget for the command-recorder lines.
static RECORDER_LOG: LogThrottle = LogThrottle::new();
/// Budget for the command-list lifetime lines.
static LIST_LOG: LogThrottle = LogThrottle::new();
/// Budget for the `pfnExecuteCommandLists` lines.
static ECL_LOG: LogThrottle = LogThrottle::new();
/// Budget for the `pfnSignalFence` / `pfnWaitForFence` lines.
static FENCE_OP_LOG: LogThrottle = LogThrottle::new();

/// The one budget shape this lane uses: the first 8, then every 4096th.
///
/// Returns the occurrence ordinal (0-based) when the line should be emitted.
fn budget(t: &LogThrottle) -> Option<usize> {
    t.first_n_then_every(8, 4096)
}

/// Sanity bound on `pfnExecuteCommandLists`' `Count`.
///
/// CLAUDE.md: *validate every runtime-supplied size before reading.* The DDI
/// declares the array `_In_reads_(Count)` and the runtime is the authority, so
/// this is not a semantic cap — no D3D12 rule limits how many lists one
/// `ExecuteCommandLists` may carry. It bounds the allocation a corrupt count
/// would demand, and its counter says if a real workload ever approached it.
const MAX_EXECUTE_COMMAND_LISTS: usize = 65_536;

/// The three runtime-owned buffer windows a **legacy** WDDM context carries.
///
/// ⭐ **FB-1** (`KMD_IMPACT.md` §14a.2). `D3DDDICB_CREATECONTEXT` hands them out
/// with `hContext`, and every `pfnRenderCb` on that context hands back
/// replacements in its own out-fields — so they are not constants, they are a
/// rotating resource dxgkrnl owns and lends. `D3DDDICB_CREATECONTEXTVIRTUAL` has
/// none of them (`KMD_IMPACT.md:314-322`), which is the mechanical reason this
/// lane mints a legacy context; see the module doc.
///
/// ⛔ **Each window is one value, never a pointer beside a size.**
/// `helios_umd_common::window::Window` exists for exactly this and its own doc
/// says so: pre-R808 the D3D11 driver held six independent `Cell`s and *"a
/// pointer could be updated without its size"*
/// (`umd_common/src/window.rs:11-21`). `Option<Window<T>>` makes both halves of
/// that unrepresentable — absent, or a non-null pointer with the capacity that
/// describes it.
struct ContextWindows {
    /// The legacy command buffer `pfnRenderCb` records from and recycles.
    command: Option<Window<c_void>>,
    /// The allocation list. ⚠ Empty for the fence carrier (K-F1 submits
    /// `NumAllocations = 0`) and **mandatory** for a DXGI present, which is where
    /// VidMm gets the residency it keeps live across the pending operation —
    /// `umd/src/forward/present.rs:772-777` has that argument.
    allocations: Option<Window<ddi12::D3DDDI_ALLOCATIONLIST>>,
    /// The patch-location list. Helios' GpuMmu is decorative — the host owns the
    /// real MMU and there are no guest GPU-VAs to patch, which is why
    /// `dxgkddi_render` passes the list straight through and `DxgkDdiPatch` is a
    /// no-op (`kmd_render/src/ddi/submit_command.rs:981-987`).
    patches: Option<Window<ddi12::D3DDDI_PATCHLOCATIONLIST>>,
}

impl ContextWindows {
    /// Latch the windows `pfnCreateContextCb` just returned.
    ///
    /// ⚠ Unconditional, unlike the re-latch that follows a `pfnRenderCb`: at
    /// create there is nothing to keep, so a null pointer here means "this context
    /// has no such window" rather than "keep what you have".
    fn from_create_context(arg: &ddi12::D3DDDICB_CREATECONTEXT) -> Self {
        Self {
            command: Window::new(arg.pCommandBuffer, arg.CommandBufferSize),
            allocations: Window::new(arg.pAllocationList, arg.AllocationListSize),
            patches: Window::new(arg.pPatchLocationList, arg.PatchLocationListSize),
        }
    }
}

/// `(pointer, capacity)` for a window that may be absent.
///
/// A null pointer with a zero capacity is how the runtime itself spells "no
/// window" (`umd_common/src/window.rs:34-37`), so flattening `None` to that pair
/// loses nothing and keeps every caller — the bounds check and the trace line —
/// off `Option` gymnastics.
fn window_parts<T>(w: &Option<Window<T>>) -> (*mut T, u32) {
    match w {
        Some(w) => (w.ptr.as_ptr(), w.capacity),
        None => (core::ptr::null_mut(), 0),
    }
}

/// Per-`ID3D12CommandQueue` shadow state (`DDI_REFERENCE.md` §9.1 row 1).
///
/// ⚠ **`pub`, not `pub(crate)`, and that is forced rather than chosen.**
/// `BoxedHandle` is a `pub` trait in `helios_umd_common`, so an associated type
/// less visible than the trait is E0446 (*crate-private type in public
/// interface*). It escapes nowhere: `forward12` and `queue` are both
/// `pub(crate) mod` inside a `cdylib` that exports no Rust API. Same shape as the
/// D3D11 side's `pub struct ResourceState` (`umd/src/forward/state.rs:24`), and
/// `umd_common/src/slot.rs:94-97`'s *"the associated type may be a type private
/// to the implementing crate"* is about module reachability, not about this
/// keyword. Every field below is private, so nothing about the layout escapes.
pub struct QueueState {
    /// The device this queue belongs to. ⚠ Every queue-table slot is `VOID`, so
    /// `pfnSetErrorCb` is their only error channel and it is **device**-scoped —
    /// there is no per-queue error callback. This is how a queue slot reaches it.
    h_device: ddi12::D3D12DDI_HDEVICE,
    /// The runtime's handle for this queue: the token `pfnCreateContextCb` and
    /// `pfnDestroyContextCb` both take (`DDI_REFERENCE.md` §7.3(3)).
    h_rt_queue: ddi12::D3D12DDI_HRTCOMMANDQUEUE,
    /// The engine queue. **Owned** — dropping this state releases it.
    engine_queue: ID3D12CommandQueue,
    /// The WDDM context minted for this queue, or null when the mint was refused
    /// (which fails the create, so a live `QueueState` always has one).
    ///
    /// ⛔ **This is a LEGACY (`pfnCreateContextCb`) context, and that decides how
    /// this queue submits.** `pKTCallbacks->pfnSubmitCommandCb` is scoped to
    /// GPU-VA contexts (`DDI_REFERENCE.md` §6.4's submission row, §8.2), so the
    /// WDDM half of `pfnExecuteCommandLists` — and P-C's per-present identity —
    /// go through **`pfnRenderCb`** on this handle. The class can only be chosen
    /// inside `pfnCreateCommandQueue`, so it is not a later lane's to pick; the
    /// module doc has the doc-set contradiction this resolves and the U12
    /// evidence that would flip it.
    h_context: *mut c_void,
    /// The three runtime-owned buffer windows [`h_context`](Self::h_context)
    /// arrived with, re-latched after every `pfnRenderCb` on it (**FB-1**).
    ///
    /// # ⛔ What serializes this, and why a `Mutex` is the DDI's requirement
    /// rather than this driver's taste
    ///
    /// A D3D12 DDI is free-threaded and `pfnExecuteCommandLists` may be entered
    /// from any thread the application likes. The window behind it is **not**
    /// free-threaded: it is one runtime-owned region that a submission writes
    /// into and that `pfnRenderCb` then *replaces*, so two concurrent submissions
    /// on one queue would write the same bytes and race to install two different
    /// successors. The D3D11.3 functional spec states the underlying rule —
    /// *"only a single thread can be working against a HCONTEXT at a time"* —
    /// quoted at `DDI_REFERENCE.md` §8.2 as the first of the three obligations
    /// `ResourceHeaps.md:1678` adds. ⇒ the lock is how this driver honours a
    /// contract, not how it tidies a field.
    ///
    /// ⛔ **The guard therefore spans write → `pfnRenderCb` → re-latch**, and
    /// that is the one place in this file where a lock is deliberately held
    /// across a call back into the runtime. The accessor block below states the
    /// opposite rule for [`RecorderState::target`] — *"no lock is ever held
    /// across a call back into the runtime or into the engine"* — and that rule
    /// is right for that lock and wrong for this one: releasing between the write
    /// and the re-latch is exactly the window in which a second thread writes a
    /// buffer dxgkrnl has already rotated away, which is the corruption the
    /// "replace as a unit" rule exists to make unrepresentable.
    ///
    /// ⚠ **Deadlock argument, stated because holding a lock across a callback
    /// demands one.** `pfnRenderCb` is a dxgkrnl thunk; it does not call back
    /// into this driver's DDI table, so it cannot re-enter
    /// `pfnExecuteCommandLists` or any other holder of this lock, and no holder
    /// of this lock takes a second lock. The lock order is therefore a single
    /// element and no cycle is expressible.
    ///
    /// ⚠ Poisoning is treated as liveness ([`lock_windows`]), for the reason
    /// [`RecorderState::target`] gives: this crate is `panic = "abort"`
    /// (`umd12/Cargo.toml:146`, `:150`), so no lock in it can be poisoned, and
    /// `PARALLEL.md` §9.3 forbids `.unwrap()` on runtime data regardless.
    windows: Mutex<ContextWindows>,
}

/// The allocator a pool ends up backed by, and the class it was created for.
///
/// One `OnceLock` payload rather than two fields, so the class can never
/// disagree with the allocator it describes: a race between two binding threads
/// resolves to one winner and the loser's allocator is dropped whole.
struct PoolAllocator {
    allocator: ID3D12CommandAllocator,
    list_type: D3D12_COMMAND_LIST_TYPE,
}

/// Per-command-pool shadow state.
///
/// ⚠ Interior mutability with no lock: D3D12 DDIs are free-threaded and
/// `OnceLock` gives a lock-free read path plus a fallible single initialisation,
/// which is exactly the shape lazy allocator creation needs. `.set()` failing
/// means another thread won; the loser's allocator is released by dropping it.
pub struct PoolState {
    allocator: OnceLock<PoolAllocator>,
}

/// Per-command-list shadow state.
///
/// ⛔ **The first two fields are the reason this type exists.** See the
/// `boxed_handles!` comment above: a command-list DDI is handed the list handle
/// and nothing else, so a recording slot can only report a failure through
/// handles captured at create. `h_rt_list` is what
/// `device12::set_command_list_error` takes — the correct, list-scoped channel —
/// and `h_device` is how the device that owns the callback table is found.
/// `pfnCreateCommandList` is the only DDI in the list's life that is told
/// either.
///
/// ⚠ **Nothing else is shadowed, deliberately.** A forwarder is at its most
/// correct when it holds no state the engine also holds: the current PSO, the
/// bound descriptor heaps, the topology and the recording/closed flag all live
/// inside vkd3d's own `d3d12_command_list` and a second copy here could only
/// disagree with it. ⭐ The one obligation that *looked* like it needed shadow
/// state — `SUBSTRATE.md` §4.5's *"the `DYNAMIC_*` PSO flags do not relieve the
/// driver of applying the PSO's own depth-bias and IB-strip-cut on every
/// `pfnSetPipelineState`"* — is discharged by the engine; see
/// [`super::pso::L6Refusals::pso_dynamic_state_flag_forwarded`], which carries
/// the source citation.
///
/// ⚠ `pub` for the same E0446 reason as [`QueueState`]: it is named as the
/// associated type of the `pub` `BoxedHandle` trait. Both fields are private.
pub struct CommandListState {
    /// The device this list was created against — the only error channel a
    /// `VOID`-returning recording slot has.
    h_device: ddi12::D3D12DDI_HDEVICE,
    /// The engine list. **Owned** — dropping this state releases it.
    engine: ID3D12GraphicsCommandList,
    /// The **runtime's** handle for this list.
    ///
    /// ⭐ Stored for `pfnSetCommandListErrorCb`, which is what a recording slot
    /// must use instead of the device-scoped `pfnSetErrorCb` — see
    /// `device12::set_command_list_error`, whose doc records that the spine
    /// originally claimed no such channel existed. The callback takes this
    /// handle and nothing else identifies the list to the runtime, so like
    /// `h_device` it can only be captured here, at `pfnCreateCommandList`.
    h_rt_list: ddi12::D3D12DDI_HRTCOMMANDLIST,

    /// The class this list was created as, from `Type` + `QueueFlags`.
    ///
    /// ⭐ Kept for exactly one reader, and it is the instrument for the open
    /// question this lane's module doc names: **how a BUNDLE allocator is
    /// expressed at this DDI.** `pfnResetCommandList` must reset against an
    /// allocator of the *list's* class, `ID3D12CommandAllocator`'s class is fixed
    /// at creation, and `D3D12DDIARG_CREATE_COMMAND_RECORDER_0040` carries no
    /// bundle bit — so a bundle list and its recorder disagree here and nowhere
    /// else. Without this field the symptom is an opaque engine `E_INVALIDARG`
    /// from `Reset`; with it, `cmdlist.rs` names the mismatch before making the
    /// call.
    list_type: D3D12_COMMAND_LIST_TYPE,
}

impl CommandListState {
    /// The engine command list, borrowed for the caller's DDI call.
    ///
    /// ⚠ A shared reference rather than a `ManuallyDrop<ID3D12GraphicsCommandList>`:
    /// the box keeps the owning reference, and borrowing it as `&` makes
    /// releasing it *unwritable* where a `ManuallyDrop` merely makes it
    /// unlikely. Same choice, and the same reasoning, as
    /// `resource12::engine_resource`.
    pub(crate) fn engine(&self) -> &ID3D12GraphicsCommandList {
        &self.engine
    }

    /// The device handle. ⚠ Used to *find* the corelayer callback table, not to
    /// scope the error: a command-list failure is reported through
    /// `device12::set_command_list_error` with [`Self::h_rt_list`], never through
    /// the device-scoped `device12::set_error`.
    pub(crate) fn h_device(&self) -> ddi12::D3D12DDI_HDEVICE {
        self.h_device
    }

    /// The class this list was created as. See the field doc — its only reader is
    /// `pfnResetCommandList`'s allocator-class check.
    pub(crate) fn list_type(&self) -> D3D12_COMMAND_LIST_TYPE {
        self.list_type
    }

    /// The runtime's handle for this list, for `pfnSetCommandListErrorCb`.
    pub(crate) fn h_rt_list(&self) -> ddi12::D3D12DDI_HRTCOMMANDLIST {
        self.h_rt_list
    }
}

/// What a recorder is currently pointed at: the pool's identity, and an **owned**
/// reference to the allocator that pool is backed by.
///
/// ⛔ **The owned reference is the whole point, and it replaces a raw
/// `AtomicPtr<c_void>` that could not be made safe.** Until S6 Round 2 this lane
/// stored only the pool's `pDrvPrivate` and never dereferenced it — the one
/// reader printed it as `{:p}`. `pfnResetCommandList` (L3a) has to go the other
/// way, from the recorder to a live `ID3D12CommandAllocator`, and re-deriving
/// `PoolState` from a stored `pDrvPrivate` would read memory **the runtime owns
/// and frees at `pfnDestroyCommandPool`**. That the runtime "would not" destroy a
/// pool a recorder still targets is a claim about someone else's object
/// lifetimes, and CLAUDE.md rule 4 wants an invariant, not a claim. Holding a
/// reference makes the freed-pool read *unrepresentable*: the allocator outlives
/// the pool's private block because this box owns a reference to it.
///
/// ⚠ `pool` is a `usize`, not a pointer: it is **identity only**, for the trace
/// lines and the rebind check, and is never dereferenced. Storing it as an
/// integer says so in the type.
struct RecorderTarget {
    pool: usize,
    allocator: ID3D12CommandAllocator,
    /// The class that allocator was **created** for.
    ///
    /// ⛔ Carried rather than re-derived, because it cannot be re-derived:
    /// `ID3D12CommandAllocator` exposes **no `GetType`** — the D3D12 API has no
    /// way to ask an allocator its class, which is why [`PoolAllocator`] pairs
    /// the two in the first place. `pfnResetCommandList`'s bundle check is the
    /// reader (`cmdlist.rs`), and without this field it would have nothing to
    /// compare the list's class against.
    ///
    /// ⚠ It is the **allocator's** class, not the binding recorder's. The two
    /// differ exactly when `PoolTypeMismatch` fires, and it is the allocator's
    /// that `ID3D12GraphicsCommandList::Reset` is judged against.
    list_type: D3D12_COMMAND_LIST_TYPE,
}

/// Per-command-recorder shadow state. There is no engine object — see the module
/// doc.
pub struct RecorderState {
    /// The engine list class this recorder records for, derived once from
    /// `D3D12DDIARG_CREATE_COMMAND_RECORDER_0040::QueueFlags`.
    list_type: D3D12_COMMAND_LIST_TYPE,
    /// The pool this recorder last targeted, and that pool's allocator.
    ///
    /// ⚠ A `Mutex` rather than the `OnceLock`+atomic pair the pool uses, because
    /// unlike a pool's allocator this is **rebindable**: the runtime may point a
    /// recorder at a different pool at any time, so there is no
    /// initialise-once shape to exploit. The critical section is one `Option`
    /// swap and one `AddRef`, on a path the runtime drives once per
    /// `ID3D12GraphicsCommandList::Reset`.
    ///
    /// ⛔ A poisoned lock is treated as a live one (`unwrap_or_else(|e|
    /// e.into_inner())`): this crate is `panic = "abort"`, so no lock in it can
    /// actually be poisoned, and `.unwrap()` on runtime data is forbidden
    /// (`PARALLEL.md` §9.3). The recovery arm is what keeps the forbidden call
    /// out of the file rather than a claim that it could not fire.
    target: Mutex<Option<RecorderTarget>>,
}

// ---------------------------------------------------------------------------
// Slot accessors
// ---------------------------------------------------------------------------

// ⭐ One named accessor per handle type, never a generic `state::<S>(h)`: a
// generic form would put the payload type back at the call site, which is the
// R803 shape the `BoxedHandle` marker exists to remove.
//
// ⛔⛔ **THE D3D12 SOUNDNESS ARGUMENT, RE-DERIVED — it is NOT inherited from
// D3D11.** `umd_common/src/slot.rs:304-322` states plainly that
// `Slot<Boxed<S>>::get() -> &'static S` rests on the D3D11 runtime's
// `CUseCountedObject` first-created/last-destroyed ordering, that *"`d3d12umddi`
// has no THREADING cap and no `CUseCountedObject` statement has been located for
// it"*, and that whoever first calls it from `umd12` *"owes the equivalent
// derivation … or must reach for `Slot::ptr()` and carry the lifetime
// themselves."* `PARALLEL.md` §9.4 repeats it.
//
// **This lane takes the second door.** None of the accessors below calls `get()`.
// Each takes `Slot::ptr()` and hands back a reference whose lifetime is the
// caller's own binding — one DDI invocation — exactly as `device12::device` does
// for the device handle. The argument is then narrower than D3D11's and does not
// depend on `CUseCountedObject`:
//
//   * the returned reference is **not** `'static`;
//   * the only path that drops a box is that object's `pfnDestroy*`, and these
//     are COM objects whose Destroy DDI the runtime issues when the
//     application's last `Release` retires the runtime-side object. An
//     application that releases an `ID3D12CommandQueue` while another of its
//     threads is calling `ExecuteCommandLists` on it has already destroyed the
//     *runtime's* object under that call; no driver can defend against it and
//     none is expected to;
//   * ⚠ concurrent **reads** across free-threaded workers are permitted by `&`
//     and are the expected case. The two fields that change after construction
//     are `PoolState::allocator`, a `OnceLock` because a pool's allocator is
//     created once and never replaced, and `RecorderState::target`, a
//     `Mutex<Option<RecorderTarget>>` because a recorder's target is
//     **rebindable** and so has no initialise-once shape to exploit. There is
//     deliberately no `&mut` accessor.
//
//     ⛔ The `Mutex` carries one argument the atomic it replaced could not:
//     `recorder_allocator` hands back an **owned** `ID3D12CommandAllocator`
//     clone, taken under the lock, which is what makes a concurrent
//     `pfnCommandRecorderSetCommandPoolAsTarget` unable to release the allocator
//     a `pfnResetCommandList` is about to reset against. ⚠ Every holder —
//     `bind_target`, `unbind_target`, `target_pool_identity`,
//     `recorder_allocator` — releases the guard before returning, so no lock is
//     ever held across a call back into the runtime or into the engine.
//
//     ⚠ The premise the compiler never checks, stated because it is a premise:
//     `RecorderTarget` holds a windows-rs COM interface, which carries no
//     `unsafe impl Send`/`Sync`. No auto-trait obligation is ever raised, because
//     every `&RecorderState` in this file is conjured from `Slot::ptr()` inside
//     `unsafe` rather than obtained from a `Sync` container — so the sharing
//     rests on D3D12's free-threading contract, not on a bound Rust enforced.

/// The queue state behind a DDI queue handle, borrowed for the caller's DDI call.
///
/// # Safety
/// `h` must be a handle [`create_command_queue`] returned `S_OK` for and
/// [`destroy_command_queue`] has not been called on, and the returned reference
/// must not outlive the DDI call that obtained it.
unsafe fn queue_state<'a>(h: ddi12::D3D12DDI_HCOMMANDQUEUE) -> Option<&'a QueueState> {
    // SAFETY: the caller guarantees a live handle, so its slot lies inside the
    // private block `calc_private_command_queue_size` sized.
    let slot = unsafe { Slot::<Boxed<QueueState>>::from_priv(h.drv_private()) }?;
    // SAFETY: same precondition; `ptr` reads the word and reports an empty slot
    // as null rather than fabricating a reference.
    let p = unsafe { slot.ptr() };
    if p.is_null() {
        return None;
    }
    // SAFETY: non-null per the check, and the box it points at was written by
    // `create_command_queue` and is dropped only by `destroy_command_queue`. See
    // the re-derived argument above for why no borrow can overlap that drop.
    Some(unsafe { &*p })
}

/// Take a queue's context-window lock, treating a poisoned lock as a live one.
///
/// ⛔ `unwrap_or_else(|e| e.into_inner())`, never `.unwrap()` — the same shape and
/// the same argument as [`lock_target`]: this crate is `panic = "abort"`, so the
/// poisoned arm cannot fire, and `PARALLEL.md` §9.3 forbids `.unwrap()` on runtime
/// data regardless.
///
/// ⚠ **The caller holds this guard across the whole submission**, not just across
/// the field write. [`QueueState::windows`] carries the contract argument and the
/// deadlock argument for that; read it before shortening the critical section.
fn lock_windows(queue: &QueueState) -> MutexGuard<'_, ContextWindows> {
    queue
        .windows
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The pool state behind a DDI pool handle. Same argument as [`queue_state`].
///
/// # Safety
/// As [`queue_state`], for a handle [`create_command_pool`] returned `S_OK` for.
unsafe fn pool_state<'a>(h: ddi12::D3D12DDI_HCOMMANDPOOL_0040) -> Option<&'a PoolState> {
    // SAFETY: as `queue_state`.
    let slot = unsafe { Slot::<Boxed<PoolState>>::from_priv(h.drv_private()) }?;
    // SAFETY: as `queue_state`.
    let p = unsafe { slot.ptr() };
    if p.is_null() {
        return None;
    }
    // SAFETY: as `queue_state`.
    Some(unsafe { &*p })
}

/// The recorder state behind a DDI recorder handle. Same argument as
/// [`queue_state`].
///
/// # Safety
/// As [`queue_state`], for a handle [`create_command_recorder`] returned `S_OK`
/// for.
unsafe fn recorder_state<'a>(
    h: ddi12::D3D12DDI_HCOMMANDRECORDER_0040,
) -> Option<&'a RecorderState> {
    // SAFETY: as `queue_state`.
    let slot = unsafe { Slot::<Boxed<RecorderState>>::from_priv(h.drv_private()) }?;
    // SAFETY: as `queue_state`.
    let p = unsafe { slot.ptr() };
    if p.is_null() {
        return None;
    }
    // SAFETY: as `queue_state`.
    Some(unsafe { &*p })
}

/// The command-list state behind a DDI command-list handle. Same argument as
/// [`queue_state`].
///
/// ⭐ **`pub(crate)`, and it is the seam the whole command-list table stands
/// on.** L3a (`cmdlist.rs`), L3b (`rootargs.rs`), L3c (`copy.rs`) and L8
/// (`present12.rs`) between them own 72 of the 75 command-list slots, every one
/// of which starts by turning this handle into an engine list and a device to
/// report against. R803's scar is that the payload must be derived from the
/// handle **type** in one place rather than decoded at each call site; this lane
/// owns the handle, so this is that one place — the same shape
/// `resource12::engine_resource` takes for `D3D12DDI_HRESOURCE`.
///
/// # Safety
/// As [`queue_state`], for a handle [`create_command_list`] returned `S_OK` for.
pub(crate) unsafe fn command_list_state<'a>(
    h: ddi12::D3D12DDI_HCOMMANDLIST,
) -> Option<&'a CommandListState> {
    // SAFETY: as `queue_state`.
    let slot = unsafe { Slot::<Boxed<CommandListState>>::from_priv(h.drv_private()) }?;
    // SAFETY: as `queue_state`.
    let p = unsafe { slot.ptr() };
    if p.is_null() {
        return None;
    }
    // SAFETY: as `queue_state`.
    Some(unsafe { &*p })
}

// ---------------------------------------------------------------------------
// Queue-class translation
// ---------------------------------------------------------------------------

/// Translate `D3D12DDI_COMMAND_QUEUE_FLAGS` into the engine list class.
///
/// ⛔ **Translated, never forwarded.** `DDI_REFERENCE.md` §9.6.1 is the scar: the
/// descriptor-heap DDI and API flag enums collide on `0x1` with *different
/// meanings*, the member types make the compiler blind to it, and the result was
/// the wrong object with no error. These two enums are unrelated by construction
/// (`D3D12DDI_COMMAND_QUEUE_FLAGS` is a bitmask, `D3D12_COMMAND_LIST_TYPE` is an
/// ordinal), so the translation is mandatory rather than merely prudent.
///
/// ⚠ `_PAGING` and the three video classes are refused rather than folded into
/// DIRECT. `DECISIONS.md` D5 maps every queue class onto WDDM **NodeOrdinal 0**
/// because Helios advertises one engine node — that is a scheduling decision and
/// costs parallelism, not correctness. Backing a video queue with a 3D
/// `ID3D12CommandAllocator` would be a different thing entirely: an answer the
/// engine cannot honour, offered by a driver whose caps report no video support.
fn engine_list_type(
    queue_flags: ddi12::D3D12DDI_COMMAND_QUEUE_FLAGS,
) -> Option<D3D12_COMMAND_LIST_TYPE> {
    use ddi12::{
        D3D12DDI_COMMAND_QUEUE_FLAGS_D3D12DDI_COMMAND_QUEUE_FLAG_3D as DDI_3D,
        D3D12DDI_COMMAND_QUEUE_FLAGS_D3D12DDI_COMMAND_QUEUE_FLAG_COMPUTE as DDI_COMPUTE,
        D3D12DDI_COMMAND_QUEUE_FLAGS_D3D12DDI_COMMAND_QUEUE_FLAG_COPY as DDI_COPY,
    };
    // ⚠ Matched in `3D`-before-`COMPUTE`-before-`COPY` order because the field is
    // a bitmask: a queue that claims both `3D` and `COPY` is a DIRECT queue, and
    // a DIRECT `ID3D12CommandAllocator` can back copy work while the reverse is
    // false. Widening beats narrowing when the answer must be a single ordinal.
    if queue_flags & DDI_3D != 0 {
        Some(D3D12_COMMAND_LIST_TYPE_DIRECT)
    } else if queue_flags & DDI_COMPUTE != 0 {
        Some(D3D12_COMMAND_LIST_TYPE_COMPUTE)
    } else if queue_flags & DDI_COPY != 0 {
        Some(D3D12_COMMAND_LIST_TYPE_COPY)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// (d) Command queues — 3 slots
// ---------------------------------------------------------------------------

/// `pfnCalcPrivateCommandQueueSize`.
///
/// # Safety
/// `arg`, when non-null, must point at a live
/// `D3D12DDIARG_CREATECOMMANDQUEUE_0050` for the duration of the call.
unsafe extern "C" fn calc_private_command_queue_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATECOMMANDQUEUE_0050,
) -> ddi12::SIZE_T {
    if arg.is_null() {
        note_refusal(&L2_REFUSALS.queue_bad_arg);
    }
    // ⛔ Answered unconditionally — see `PRIVATE_SLOT_SIZE`.
    PRIVATE_SLOT_SIZE as ddi12::SIZE_T
}

/// `pfnCreateCommandQueue` — one `ID3D12CommandQueue` **plus** one WDDM context
/// (`DDI_REFERENCE.md` §9.2). Read the module doc before changing the context
/// half.
///
/// # Safety
/// `h_device` must be a live handle from `device12::create_device`; `arg` must
/// point at a live `D3D12DDIARG_CREATECOMMANDQUEUE_0050`; `h_queue`'s
/// `pDrvPrivate` must address the private block
/// [`calc_private_command_queue_size`] sized; `h_rt_queue` must be the runtime's
/// handle for this queue.
unsafe extern "C" fn create_command_queue(
    h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATECOMMANDQUEUE_0050,
    h_queue: ddi12::D3D12DDI_HCOMMANDQUEUE,
    h_rt_queue: ddi12::D3D12DDI_HRTCOMMANDQUEUE,
) -> ddi12::HRESULT {
    // SAFETY: the caller guarantees the slot lies in the sized private block.
    let Some(slot) = (unsafe { Slot::<Boxed<QueueState>>::from_priv(h_queue.drv_private()) })
    else {
        note_refusal(&L2_REFUSALS.queue_bad_arg);
        return E_INVALIDARG;
    };
    // ⛔ Clear first, so every refusal below leaves a null slot rather than
    // whatever the runtime's allocator left there.
    // SAFETY: as above.
    unsafe { slot.clear() };

    if arg.is_null() {
        note_refusal(&L2_REFUSALS.queue_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: non-null per the check; the DDI declares it `_In_ CONST`.
    let a = unsafe { &*arg };

    let Some(list_type) = engine_list_type(a.QueueFlags) else {
        note_refusal(&L2_REFUSALS.queue_class_unsupported);
        if let Some(n) = budget(&QUEUE_LOG) {
            log_error!(
                "CreateCommandQueue: QueueFlags={:#x} names no 3D/COMPUTE/COPY class this driver \
                 backs -> E_INVALIDARG (x{})",
                a.QueueFlags,
                n + 1,
            );
        }
        return E_INVALIDARG;
    };

    // ⚠ Three inputs accepted-and-counted rather than refused. None of them can
    // change what this driver builds, and refusing on any of them would fail a
    // queue create over a hint:
    //   * GLOBAL_REALTIME_PRIORITY is a scheduling request against a
    //     software-scheduled adapter that has one engine node;
    //   * a scheduling group is L9's `pfnCreateSchedulingGroup`, still a counting
    //     noop, so there is no group object to join;
    //   * `NodeMask` beyond the single node Helios advertises is the
    //     `ARCHITECTURE.md` §13 UNVERIFIED-11 multi-adapter surface.
    if a.QueueCreationFlags
        != ddi12::D3D12DDI_COMMAND_QUEUE_CREATION_FLAGS_D3D12DDI_COMMAND_QUEUE_CREATION_FLAG_NONE
    {
        note_refusal(&L2_REFUSALS.queue_creation_flags_ignored);
    }
    if !a.SchedulingGroup.pDrvPrivate.is_null() {
        note_refusal(&L2_REFUSALS.queue_scheduling_group_ignored);
    }
    if a.NodeMask > 1 {
        note_refusal(&L2_REFUSALS.queue_node_mask_ignored);
    }

    // SAFETY: this is a device-scope DDI, so the runtime passes a handle
    // `create_device` returned `S_OK` for; the borrow lives only until the end of
    // this call, which is `device12::device`'s stated precondition.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&L2_REFUSALS.queue_no_device);
        return E_FAIL;
    };
    let Some(engine) = dev.engine.d3d12_device() else {
        note_refusal(&L2_REFUSALS.queue_no_device);
        return E_FAIL;
    };

    // ⚠ `Priority: 0` is `D3D12_COMMAND_QUEUE_PRIORITY_NORMAL`, and it is the
    // only honest answer: the DDI carries no priority (`D3D12DDIARG_CREATECOMMANDQUEUE_0050`
    // has `QueueCreationFlags`, not a priority), and the runtime keeps priority
    // for itself unless a driver answers
    // `D3D12DDICAPS_TYPE_0023_UMD_BASED_COMMAND_QUEUE_PRIORITY`, which this
    // driver does not.
    let desc = D3D12_COMMAND_QUEUE_DESC {
        Type: list_type,
        Priority: 0,
        Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
        NodeMask: a.NodeMask,
    };
    // SAFETY: `engine` is the bridge's live borrowed `ID3D12Device`, `desc` is a
    // live local for the call, and the out-param is the wrapper's own.
    let created = unsafe { engine.CreateCommandQueue::<ID3D12CommandQueue>(&desc) };
    let engine_queue = match created {
        Ok(q) => q,
        Err(e) => {
            note_refusal(&L2_REFUSALS.queue_engine_failed);
            if let Some(n) = budget(&QUEUE_LOG) {
                log_error!(
                    "CreateCommandQueue: engine CreateCommandQueue(type={}) failed hr={:#010x} \
                     (x{})",
                    list_type.0,
                    e.code().0 as u32,
                    n + 1,
                );
            }
            return E_FAIL;
        }
    };

    // ── The WDDM context — the only place it can ever be minted ──────────────
    // SAFETY: `dev` is the live device borrowed above, `h_rt_queue` is the
    // runtime's handle for the queue being created, and this call site is
    // literally inside `pfnCreateCommandQueue` — which is
    // `create_wddm_context`'s whole precondition, and the one the runtime
    // enforces from its side.
    let (h_context, windows) = match unsafe { create_wddm_context(dev, h_rt_queue) } {
        Ok(pair) => pair,
        Err(hr) => {
            // ⚠ The engine queue is dropped here, releasing it: a queue that can
            // never present or submit is not a queue, and CLAUDE.md rule 2 is
            // loud failure over fake success. See the module doc for why this
            // cannot be deferred to a later lane.
            drop(engine_queue);
            return hr;
        }
    };

    // SAFETY: the slot lies in the sized private block and is currently null
    // (cleared above); `store` boxes the state and moves the box into it, so the
    // slot owns both the box and, through it, the engine queue's reference.
    unsafe {
        slot.store(QueueState {
            h_device,
            h_rt_queue,
            engine_queue,
            h_context,
            // FB-1. ⚠ The handle and its windows are latched from the SAME
            // `D3DDDICB_CREATECONTEXT` and travel together from here on: a
            // window paired with another context's handle would have
            // `pfnRenderCb` record into memory dxgkrnl never lent this context.
            windows: Mutex::new(windows),
        });
    }
    S_OK
}

/// Mint this queue's WDDM context through the **corelayer** callback.
///
/// ⚠ The corelayer `pfnCreateContextCb` takes `D3D12DDI_HRTCOMMANDQUEUE`, not a
/// device handle (`d3d12umddi.h:2556-2559`) — that is the runtime associating the
/// context with the queue, which is exactly the association
/// `ResourceHeaps.md:1678` then requires of every kernel submission on it.
/// ⛔ Not `pKTCallbacks->pfnCreateContextCb`: the kernel table's version is
/// device-scoped and the runtime rejects a context created outside queue
/// creation (`DDI_REFERENCE.md` §6.4).
///
/// ⛔⛔ **`pfnCreateContextCb`, not `pfnCreateContextVirtualCb` — read the module
/// doc's context section before changing this line.** The choice is a decision
/// this lane records against a doc set that contradicts itself (`§6.4`/`§9.2`
/// name legacy, `DECISIONS.md` D5 and §9.2's node paragraph name
/// `D3DDDICB_CREATECONTEXTVIRTUAL`, `PRESENT.md` §12 U12 records the question as
/// open), and it carries a consequence for every later lane: a legacy context
/// submits with `pfnRenderCb`, **never** with `pfnSubmitCommandCb`.
///
/// ⭐ **Returns the handle AND its three windows** (FB-1). They are one value
/// because they are meaningful only together — `umd/src/device_funcs.rs:132-151`
/// (R808) made the same group one value for the same reason, after seven fields
/// *"used to become meaningful together or not at all, depending on one `hr` the
/// caller never saw"*.
///
/// # Safety
/// `dev` must be a live device and `h_rt_queue` the runtime's handle for the
/// queue currently being created — the callback is only legal inside
/// `pfnCreateCommandQueue`.
unsafe fn create_wddm_context(
    dev: &device12::HeliosD3D12Device,
    h_rt_queue: ddi12::D3D12DDI_HRTCOMMANDQUEUE,
) -> Result<(*mut c_void, ContextWindows), ddi12::HRESULT> {
    if dev.um_callbacks.is_null() {
        note_refusal(&L2_REFUSALS.queue_context_failed);
        if budget(&QUEUE_LOG).is_some() {
            log_error!("CreateCommandQueue: no corelayer callbacks for CreateContext");
        }
        return Err(E_FAIL);
    }
    // SAFETY: `um_callbacks` was null-checked in `create_device` before the
    // device was constructed and is the runtime's `_0062` table, which outlives
    // the device.
    let Some(create_context_cb) = (unsafe { (*dev.um_callbacks).pfnCreateContextCb }) else {
        note_refusal(&L2_REFUSALS.queue_context_failed);
        if budget(&QUEUE_LOG).is_some() {
            log_error!("CreateCommandQueue: corelayer pfnCreateContextCb missing");
        }
        return Err(E_FAIL);
    };

    // ⚠ `NodeOrdinal = 0, EngineAffinity = 0`, exactly as the D3D11 driver
    // (`umd/src/device_funcs.rs:1011-1012`). Helios advertises one node
    // (`DXGK_ENGINE_TYPE_3D`, `NbAsymetricProcessingNodes = 1`), so every queue
    // class maps to node 0 — `DECISIONS.md` D5's "no extra engine nodes" in its
    // DDI form (`DDI_REFERENCE.md` §9.2).
    let mut arg = ddi12::D3DDDICB_CREATECONTEXT {
        NodeOrdinal: 0,
        EngineAffinity: 0,
        ..Default::default()
    };
    // SAFETY: a non-null callback from the runtime's own table, given the
    // runtime's queue handle and a fully initialised out-struct local. The
    // runtime writes `hContext` and the three windows into it.
    let hr = unsafe { create_context_cb(h_rt_queue, &mut arg) };

    // ⚠ KEPT VERBATIM, and it still reads `arg` rather than the latched
    // [`ContextWindows`] even though FB-1 now stores them. Two reasons, both
    // load-bearing: this is *the only capture of what dxgkrnl hands a D3D12 queue
    // on this adapter*, which is contract data no document in `docs/dx12/` holds;
    // and it fires on the FAILURE path too, where there is nothing to latch. A
    // version that read the stored windows could only run after the refusals
    // below and would lose exactly the case worth having. ⚠ It carries the budget
    // even though queue creates are rare: "rare" is a property of the
    // applications measured so far, not of the DDI.
    if let Some(n) = budget(&QUEUE_LOG) {
        log_error!(
            "CreateCommandQueue: CreateContext hr={:#010x} hContext={:p} cmd={:p}/{} \
             allocList={:p}/{} patchList={:p}/{} (x{})",
            hr as u32,
            arg.hContext,
            arg.pCommandBuffer,
            arg.CommandBufferSize,
            arg.pAllocationList,
            arg.AllocationListSize,
            arg.pPatchLocationList,
            arg.PatchLocationListSize,
            n + 1,
        );
    }

    // ⛔ `hr < 0`, not `hr != S_OK`, and the difference is a fake success. This arm
    // decides the create FAILED: it counts it, and `create_command_queue` releases
    // the engine queue and returns this value as `pfnCreateCommandQueue`'s own
    // HRESULT. A non-negative non-`S_OK` value (`S_FALSE` and friends) is a
    // SUCCESS code, so returning it verbatim would hand the runtime a successful
    // queue create whose private slot is null and whose engine queue has already
    // been dropped. So the classification and the returned value must agree:
    // anything this arm treats as failure leaves as a failure. `resource12.rs`'s
    // `return if hr < 0 { hr } else { E_FAIL };` is the same normalisation one
    // file over, and the two now match.
    if hr < 0 {
        note_refusal(&L2_REFUSALS.queue_context_failed);
        return Err(hr);
    }
    if hr != S_OK {
        // A success code this driver did not expect from `pfnCreateContextCb`.
        // Not fatal — the context may be perfectly usable — but it is counted,
        // because "the callback answered something other than S_OK" is exactly
        // the kind of fact that is invisible until it matters.
        note_refusal(&L2_REFUSALS.queue_context_failed);
    }
    if arg.hContext.is_null() {
        // The whole group becomes meaningful at once or the call failed —
        // `umd/src/device_funcs.rs:1028-1034` learned this: an `S_OK` with a null
        // `hContext` left six companion fields set and every consumer to discover
        // it five checks deep.
        note_refusal(&L2_REFUSALS.queue_context_failed);
        if budget(&QUEUE_LOG).is_some() {
            log_error!("CreateCommandQueue: CreateContext returned S_OK with a null hContext");
        }
        return Err(E_FAIL);
    }
    // FB-1. ⚠ Latched here and not at the call site: `arg` is this function's
    // local and dies with it, so the only place the group can be captured is the
    // one place that owns the out-struct.
    Ok((arg.hContext, ContextWindows::from_create_context(&arg)))
}

/// `pfnDestroyCommandQueue`.
///
/// # Safety
/// `h_queue` must be a handle [`create_command_queue`] returned `S_OK` for and
/// which has not already been destroyed.
unsafe extern "C" fn destroy_command_queue(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_queue: ddi12::D3D12DDI_HCOMMANDQUEUE,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_queue`.
    let Some(slot) = (unsafe { Slot::<Boxed<QueueState>>::from_priv(h_queue.drv_private()) })
    else {
        note_refusal(&L2_REFUSALS.queue_bad_arg);
        return;
    };
    // SAFETY: the slot holds either null or the one box `create_command_queue`
    // moved in; `take` empties it, so a second destroy is a no-op rather than a
    // double free.
    let Some(state) = (unsafe { slot.take() }) else {
        note_refusal(&L2_REFUSALS.queue_bad_arg);
        return;
    };

    // ⚠ The context goes first: it is the object the *runtime* tracks against
    // this queue, and tearing it down while the box still owns the engine queue
    // keeps the two teardowns independently attributable in the log.
    // SAFETY: `state` is the live box this call took out of the slot, so its
    // `h_rt_queue` and `h_context` are the pair `create_wddm_context` produced.
    unsafe { destroy_wddm_context(&state) };

    // Dropping the box releases the engine queue's single reference.
    drop(state);
}

/// Release this queue's WDDM context.
///
/// # Safety
/// `state` must be a live `QueueState` whose `h_context` came from
/// [`create_wddm_context`] and has not been destroyed.
unsafe fn destroy_wddm_context(state: &QueueState) {
    if state.h_context.is_null() {
        return;
    }
    // SAFETY: `h_device` is the device this queue was created against, and the
    // borrow lives only until the end of this function.
    let Some(dev) = (unsafe { device12::device(state.h_device) }) else {
        note_refusal(&L2_REFUSALS.queue_context_destroy_failed);
        return;
    };
    if dev.um_callbacks.is_null() {
        note_refusal(&L2_REFUSALS.queue_context_destroy_failed);
        return;
    }
    // SAFETY: as `create_wddm_context`.
    let Some(destroy_context_cb) = (unsafe { (*dev.um_callbacks).pfnDestroyContextCb }) else {
        note_refusal(&L2_REFUSALS.queue_context_destroy_failed);
        return;
    };
    let arg = ddi12::D3DDDICB_DESTROYCONTEXT {
        hContext: state.h_context,
    };
    // SAFETY: a non-null callback from the runtime's own table, given the
    // runtime's queue handle and the context handle it minted for it.
    let hr = unsafe { destroy_context_cb(state.h_rt_queue, &arg) };
    if hr != S_OK {
        note_refusal(&L2_REFUSALS.queue_context_destroy_failed);
        if budget(&QUEUE_LOG).is_some() {
            log_error!("DestroyCommandQueue: DestroyContext hr={:#010x}", hr as u32);
        }
    }
}

// ---------------------------------------------------------------------------
// (d) Command pools — 4 slots
// ---------------------------------------------------------------------------

/// `pfnCalcPrivateCommandPoolSize`.
///
/// # Safety
/// `arg`, when non-null, must point at a live
/// `D3D12DDIARG_CREATE_COMMAND_POOL_0040` for the duration of the call.
unsafe extern "C" fn calc_private_command_pool_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_COMMAND_POOL_0040,
) -> ddi12::SIZE_T {
    if arg.is_null() {
        note_refusal(&L2_REFUSALS.pool_bad_arg);
    }
    // ⛔ Answered unconditionally — see `PRIVATE_SLOT_SIZE`.
    PRIVATE_SLOT_SIZE as ddi12::SIZE_T
}

/// `pfnCreateCommandPool`.
///
/// ⚠ **Creates no engine object.** `D3D12DDIARG_CREATE_COMMAND_POOL_0040` is one
/// flags word with a single legal value; the allocator this pool becomes is
/// created at the first `pfnCommandRecorderSetCommandPoolAsTarget`, because that
/// is the first DDI that names a queue class for it. Module doc, first section.
///
/// # Safety
/// `h_pool`'s `pDrvPrivate` must address the private block
/// [`calc_private_command_pool_size`] sized.
unsafe extern "C" fn create_command_pool(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_COMMAND_POOL_0040,
    h_pool: ddi12::D3D12DDI_HCOMMANDPOOL_0040,
) -> ddi12::HRESULT {
    // SAFETY: the caller guarantees the slot lies in the sized private block.
    let Some(slot) = (unsafe { Slot::<Boxed<PoolState>>::from_priv(h_pool.drv_private()) }) else {
        note_refusal(&L2_REFUSALS.pool_bad_arg);
        return E_INVALIDARG;
    };
    // SAFETY: as above.
    unsafe { slot.clear() };

    if arg.is_null() {
        note_refusal(&L2_REFUSALS.pool_bad_arg);
        return E_INVALIDARG;
    }
    // ⚠ `PoolFlags` is deliberately not examined: `D3D12DDI_COMMAND_POOL_FLAGS`
    // has exactly one enumerator, `NONE = 0`, so there is nothing to branch on
    // and a check against a one-value enum would be a claim about a future
    // header rather than about this one.

    // SAFETY: the slot lies in the sized private block and is currently null.
    unsafe {
        slot.store(PoolState {
            allocator: OnceLock::new(),
        });
    }
    S_OK
}

/// `pfnResetCommandPool` -> `ID3D12CommandAllocator::Reset()`.
///
/// Returns `VOID`. ⚠ A reset before any recorder ever targeted this pool has no
/// allocator to reset and is counted rather than reported: there is no allocator
/// because there was no work, so nothing is wrong yet — the failure, if any,
/// arrives at `pfnResetCommandList`, which is L3a's slot and where a missing
/// allocator is actually fatal.
///
/// # Safety
/// `h_pool` must be a handle [`create_command_pool`] returned `S_OK` for.
unsafe extern "C" fn reset_command_pool(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_pool: ddi12::D3D12DDI_HCOMMANDPOOL_0040,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_pool`.
    let Some(pool) = (unsafe { pool_state(h_pool) }) else {
        note_refusal(&L2_REFUSALS.pool_bad_arg);
        return;
    };
    let Some(backing) = pool.allocator.get() else {
        note_refusal(&L2_REFUSALS.pool_reset_no_allocator);
        return;
    };
    // SAFETY: `backing.allocator` is the live allocator this pool owns; `Reset`
    // takes no arguments and returns an HRESULT.
    if let Err(e) = unsafe { backing.allocator.Reset() } {
        note_refusal(&L2_REFUSALS.pool_reset_engine_failed);
        if let Some(n) = budget(&POOL_LOG) {
            log_error!(
                "ResetCommandPool: engine Reset failed hr={:#010x} (x{})",
                e.code().0 as u32,
                n + 1,
            );
        }
    }
}

/// `pfnDestroyCommandPool`.
///
/// # Safety
/// `h_pool` must be a handle [`create_command_pool`] returned `S_OK` for and
/// which has not already been destroyed.
unsafe extern "C" fn destroy_command_pool(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_pool: ddi12::D3D12DDI_HCOMMANDPOOL_0040,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_pool`.
    let Some(slot) = (unsafe { Slot::<Boxed<PoolState>>::from_priv(h_pool.drv_private()) }) else {
        note_refusal(&L2_REFUSALS.pool_bad_arg);
        return;
    };
    // SAFETY: the slot holds either null or the one box `create_command_pool`
    // moved in. Dropping the box drops the `OnceLock`, releasing the allocator.
    let Some(state) = (unsafe { slot.take() }) else {
        note_refusal(&L2_REFUSALS.pool_bad_arg);
        return;
    };
    drop(state);
}

// ---------------------------------------------------------------------------
// (d) Command recorders — 4 slots
// ---------------------------------------------------------------------------

/// `pfnCalcPrivateCommandRecorderSize`.
///
/// # Safety
/// `arg`, when non-null, must point at a live
/// `D3D12DDIARG_CREATE_COMMAND_RECORDER_0040` for the duration of the call.
unsafe extern "C" fn calc_private_command_recorder_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_COMMAND_RECORDER_0040,
) -> ddi12::SIZE_T {
    if arg.is_null() {
        note_refusal(&L2_REFUSALS.recorder_bad_arg);
    }
    // ⛔ Answered unconditionally — see `PRIVATE_SLOT_SIZE`.
    PRIVATE_SLOT_SIZE as ddi12::SIZE_T
}

/// `pfnCreateCommandRecorder` — driver-side only, no engine object.
///
/// # Safety
/// `h_recorder`'s `pDrvPrivate` must address the private block
/// [`calc_private_command_recorder_size`] sized.
unsafe extern "C" fn create_command_recorder(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_COMMAND_RECORDER_0040,
    h_recorder: ddi12::D3D12DDI_HCOMMANDRECORDER_0040,
) -> ddi12::HRESULT {
    // SAFETY: the caller guarantees the slot lies in the sized private block.
    let Some(slot) =
        (unsafe { Slot::<Boxed<RecorderState>>::from_priv(h_recorder.drv_private()) })
    else {
        note_refusal(&L2_REFUSALS.recorder_bad_arg);
        return E_INVALIDARG;
    };
    // SAFETY: as above.
    unsafe { slot.clear() };

    if arg.is_null() {
        note_refusal(&L2_REFUSALS.recorder_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: non-null per the check; the DDI declares it `_In_ CONST`.
    let a = unsafe { &*arg };

    let Some(list_type) = engine_list_type(a.QueueFlags) else {
        note_refusal(&L2_REFUSALS.recorder_class_unsupported);
        if let Some(n) = budget(&RECORDER_LOG) {
            log_error!(
                "CreateCommandRecorder: QueueFlags={:#x} names no 3D/COMPUTE/COPY class this \
                 driver backs -> E_INVALIDARG (x{})",
                a.QueueFlags,
                n + 1,
            );
        }
        return E_INVALIDARG;
    };
    // ⚠ `RecorderFlags` has one enumerator, `NONE = 0`, so it carries nothing to
    // branch on — same reasoning as `PoolFlags` in `create_command_pool`.

    // SAFETY: the slot lies in the sized private block and is currently null.
    unsafe {
        slot.store(RecorderState {
            list_type,
            target: Mutex::new(None),
        });
    }
    S_OK
}

/// `pfnCommandRecorderSetCommandPoolAsTarget` — bind a pool to a recorder, and
/// materialise the pool's `ID3D12CommandAllocator` on first use.
///
/// ⭐ **This is the only DDI where a queue class meets a pool**, which is why the
/// allocator is created here rather than at `pfnCreateCommandPool`. Module doc,
/// first section.
///
/// Returns `VOID`. A failed allocator creation is counted and logged but **not**
/// raised through `pfnSetErrorCb`: the binding this slot exists to record *is*
/// recorded either way, and removing the device over a deferred allocation
/// failure would report a transient as fatal. Its consumer, L3a's
/// `pfnResetCommandList`, is where a missing allocator is actually fatal and is
/// where it must be reported.
///
/// # Safety
/// `h_device` must be a live device handle, `h_recorder` a live recorder handle
/// and `h_pool` a live pool handle.
unsafe extern "C" fn command_recorder_set_command_pool_as_target(
    h_device: ddi12::D3D12DDI_HDEVICE,
    h_recorder: ddi12::D3D12DDI_HCOMMANDRECORDER_0040,
    h_pool: ddi12::D3D12DDI_HCOMMANDPOOL_0040,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_recorder`.
    let Some(recorder) = (unsafe { recorder_state(h_recorder) }) else {
        note_refusal(&L2_REFUSALS.recorder_bad_arg);
        return;
    };
    // SAFETY: the caller guarantees a live handle from `create_command_pool`.
    let Some(pool) = (unsafe { pool_state(h_pool) }) else {
        // ⛔ The recorder DID resolve, so it may still be pointing at a previous
        // pool. See `unbind_target`: a bind that failed must leave no binding.
        unbind_target(recorder);
        note_refusal(&L2_REFUSALS.pool_bad_arg);
        return;
    };

    // ⚠ The previous binding is read rather than overwritten blind: reporting
    // *what changed* is what makes this field an instrument rather than
    // write-only state, and the trace line below is the readout.
    let previous = target_pool_identity(recorder);
    trace_line!(
        "CommandRecorderSetCommandPoolAsTarget: recorder={:p} pool {:#x} -> {:p}",
        h_recorder.pDrvPrivate,
        previous,
        h_pool.pDrvPrivate,
    );

    // Already backed? Then the only things left to do are to check that the class
    // the allocator was created for still matches this recorder's, and to adopt
    // it as this recorder's target.
    if let Some(backing) = pool.allocator.get() {
        if backing.list_type != recorder.list_type {
            note_refusal(&L2_REFUSALS.pool_type_mismatch);
            if let Some(n) = budget(&POOL_LOG) {
                log_error!(
                    "CommandRecorderSetCommandPoolAsTarget: pool is backed by a type-{} allocator \
                     and this recorder records type {} -- an ID3D12CommandAllocator's type is \
                     fixed at creation, so the existing allocator is kept (x{})",
                    backing.list_type.0,
                    recorder.list_type.0,
                    n + 1,
                );
            }
        }
        // ⚠ Bound even on the mismatch path, and deliberately: the binding this
        // slot exists to record happened, the mismatch is counted, and
        // `pfnResetCommandList` failing against an allocator of the wrong class
        // is a far more legible symptom than a recorder that silently still
        // names its previous pool.
        bind_target(recorder, h_pool.drv_private() as usize, backing);
        return;
    }

    // SAFETY: device-scope DDI; the borrow lives only until the end of the call.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        unbind_target(recorder);
        note_refusal(&L2_REFUSALS.pool_no_device);
        return;
    };
    let Some(engine) = dev.engine.d3d12_device() else {
        unbind_target(recorder);
        note_refusal(&L2_REFUSALS.pool_no_device);
        return;
    };
    // SAFETY: `engine` is the bridge's live borrowed `ID3D12Device`; the call
    // takes one by-value enum and returns an owned interface.
    let created =
        unsafe { engine.CreateCommandAllocator::<ID3D12CommandAllocator>(recorder.list_type) };
    let allocator = match created {
        Ok(a) => a,
        Err(e) => {
            unbind_target(recorder);
            note_refusal(&L2_REFUSALS.pool_allocator_engine_failed);
            if let Some(n) = budget(&POOL_LOG) {
                log_error!(
                    "CommandRecorderSetCommandPoolAsTarget: engine \
                     CreateCommandAllocator(type={}) failed hr={:#010x} (x{})",
                    recorder.list_type.0,
                    e.code().0 as u32,
                    n + 1,
                );
            }
            return;
        }
    };
    // ⚠ A losing `set` means another thread bound this pool first; its allocator
    // is the pool's and ours is dropped (released) here. That is the whole
    // free-threaded story for this field — no lock, no retry, no leak.
    if pool
        .allocator
        .set(PoolAllocator {
            allocator,
            list_type: recorder.list_type,
        })
        .is_err()
    {
        trace_line!("CommandRecorderSetCommandPoolAsTarget: lost the allocator init race");
    }
    // ⛔ Read back through `pool.allocator` rather than binding the local: on the
    // losing side of the race the local is not the pool's allocator, and binding
    // it would give this recorder a reference to an object the pool does not own
    // and `pfnResetCommandPool` will never reset.
    let Some(backing) = pool.allocator.get() else {
        // Unreachable: `set` either stored ours or found another thread's, so the
        // `OnceLock` is initialised either way. Counted rather than asserted —
        // this crate is `panic = "abort"`.
        unbind_target(recorder);
        note_refusal(&L2_REFUSALS.pool_allocator_engine_failed);
        return;
    };
    bind_target(recorder, h_pool.drv_private() as usize, backing);
}

/// This recorder's current pool identity, or 0.
///
/// ⚠ Identity only — the value is never dereferenced. See [`RecorderTarget`].
fn target_pool_identity(recorder: &RecorderState) -> usize {
    lock_target(recorder).as_ref().map_or(0, |t| t.pool)
}

/// Forget whatever this recorder was pointed at.
///
/// ⛔ **Called from every failure arm of
/// `pfnCommandRecorderSetCommandPoolAsTarget`, and that is not tidiness.** The
/// slot's contract is *"this recorder now targets this pool"*; if it cannot be
/// honoured, leaving the PREVIOUS binding in place is worse than leaving none,
/// because `recorder_allocator` then answers `Ready` with an allocator belonging
/// to a pool the runtime did not name — one that may already back another
/// recording list, and one that `pfnResetCommandPool` on the *new* pool will
/// never reset. L3a's `L3aResetNoAllocator`, whose doc grades it *"Expected 0,
/// and a hit is a real finding about DDI ordering"*, is the loud-failure path
/// built for exactly this and is bypassed unless the stale target is cleared.
fn unbind_target(recorder: &RecorderState) {
    *lock_target(recorder) = None;
}

/// Point a recorder at a pool, taking its own reference to that pool's allocator.
///
/// The `clone` is one `AddRef`, balanced when this recorder is rebound or
/// destroyed. It is what makes [`recorder_allocator`] unable to touch a
/// destroyed pool's private block — see [`RecorderTarget`].
fn bind_target(recorder: &RecorderState, pool: usize, backing: &PoolAllocator) {
    *lock_target(recorder) = Some(RecorderTarget {
        pool,
        allocator: backing.allocator.clone(),
        list_type: backing.list_type,
    });
}

/// Take a recorder's target lock, treating a poisoned lock as a live one.
///
/// ⛔ `unwrap_or_else(|e| e.into_inner())`, never `.unwrap()`. A `Mutex` is
/// poisoned only by a panic while it is held, and this crate is `panic = "abort"`
/// — so the poisoned arm cannot fire. `PARALLEL.md` §9.3 forbids `.unwrap()` on
/// runtime data regardless, and writing the recovery is how that stays true
/// without an argument at the call site.
fn lock_target(recorder: &RecorderState) -> MutexGuard<'_, Option<RecorderTarget>> {
    recorder
        .target
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What [`recorder_allocator`] found when a `pfnResetCommandList` asked a
/// recorder for the allocator to reset against.
///
/// ⭐ Three distinguishable failures rather than one `Option`, because they are
/// three different findings and L3a counts them apart: a handle the runtime
/// invented, a recorder the runtime never bound, and a pool whose allocator
/// creation failed at `pfnCommandRecorderSetCommandPoolAsTarget`.
pub(crate) enum RecorderAllocator {
    /// The recorder names a pool and that pool is backed.
    Ready {
        /// **Owned** — one `AddRef` the caller releases by dropping.
        allocator: ID3D12CommandAllocator,
        /// The class that allocator was created for. See
        /// [`RecorderTarget::list_type`]: the API cannot be asked, so it is
        /// carried.
        list_type: D3D12_COMMAND_LIST_TYPE,
    },
    /// The recorder handle did not resolve to a live [`RecorderState`].
    NoRecorder,
    /// No `pfnCommandRecorderSetCommandPoolAsTarget` has ever run on it, so
    /// there is no pool and no allocator.
    NoPoolBound,
}

/// The allocator `pfnResetCommandList` must reset a list against.
///
/// ⭐ **`pub(crate)` because L3a's `pfnResetCommandList` is its only caller and
/// the chain it walks is entirely this lane's private state** — recorder ->
/// bound pool -> `ID3D12CommandAllocator`. `PARALLEL.md` §4 gives L3a the slot
/// and this lane the three objects, and this function is that seam, in the file
/// that owns the objects. ⛔ It returns an **owned** reference rather than a
/// borrow because the target sits behind a `Mutex`; see [`RecorderState::target`].
///
/// # Safety
/// As [`recorder_state`], for a handle [`create_command_recorder`] returned
/// `S_OK` for.
pub(crate) unsafe fn recorder_allocator(
    h_recorder: ddi12::D3D12DDI_HCOMMANDRECORDER_0040,
) -> RecorderAllocator {
    // SAFETY: forwarded unchanged; the caller's guarantee is `recorder_state`'s.
    let Some(recorder) = (unsafe { recorder_state(h_recorder) }) else {
        return RecorderAllocator::NoRecorder;
    };
    match lock_target(recorder).as_ref() {
        Some(target) => RecorderAllocator::Ready {
            allocator: target.allocator.clone(),
            list_type: target.list_type,
        },
        None => RecorderAllocator::NoPoolBound,
    }
}

/// `pfnDestroyCommandRecorder`.
///
/// The log line is this lane's readout of `RecorderState::target`: *did the
/// runtime ever bind a pool to this recorder?* is a real triage question, and one
/// line per recorder teardown is the cheapest place to answer it.
///
/// # Safety
/// `h_recorder` must be a handle [`create_command_recorder`] returned `S_OK` for
/// and which has not already been destroyed.
unsafe extern "C" fn destroy_command_recorder(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_recorder: ddi12::D3D12DDI_HCOMMANDRECORDER_0040,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_recorder`.
    let Some(slot) =
        (unsafe { Slot::<Boxed<RecorderState>>::from_priv(h_recorder.drv_private()) })
    else {
        note_refusal(&L2_REFUSALS.recorder_bad_arg);
        return;
    };
    // SAFETY: the slot holds either null or the one box
    // `create_command_recorder` moved in.
    let Some(state) = (unsafe { slot.take() }) else {
        note_refusal(&L2_REFUSALS.recorder_bad_arg);
        return;
    };
    trace_line!(
        "DestroyCommandRecorder: recorder={:p} type={} lastPool={:#x}",
        h_recorder.pDrvPrivate,
        state.list_type.0,
        target_pool_identity(&state),
    );
    // Dropping the box drops the target, releasing this recorder's own reference
    // to its pool's allocator.
    drop(state);
}

// ---------------------------------------------------------------------------
// (d) Command lists — 3 slots
// ---------------------------------------------------------------------------

/// `pfnCalcPrivateCommandListSize`.
///
/// # Safety
/// `arg`, when non-null, must point at a live
/// `D3D12DDIARG_CREATE_COMMAND_LIST_0040` for the duration of the call.
unsafe extern "C" fn calc_private_command_list_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_COMMAND_LIST_0040,
) -> ddi12::SIZE_T {
    if arg.is_null() {
        note_refusal(&L2_REFUSALS.command_list_bad_arg);
    }
    // ⛔ Answered unconditionally — see `PRIVATE_SLOT_SIZE`.
    PRIVATE_SLOT_SIZE as ddi12::SIZE_T
}

/// `pfnCreateCommandList`.
///
/// # ⭐ `pfnSetCommandListDDITableCb` is mandatory here, and index 0 is the answer
///
/// The runtime says so in its own words:
///
/// > `Driver didn't call pfnSetCommandListDDITableCb or called it with invalid
/// > D3D12DDI_HRTTABLE at command list creation, defaulting to stubbed DDIs.`
/// > — strings:30
///
/// **What happens if the index is wrong**, stated because the brief asks and
/// because the failure is quiet: an `HRTTABLE` the runtime does not recognise
/// (including the `0` a never-stashed index would yield) makes it install *its
/// own* stubs over this list — the list is created, every recording DDI silently
/// goes to the runtime instead of to this driver, and the only signal is that
/// debug string. Nothing crashes; the list simply records nothing.
///
/// **Which index.** `D12-G5` measured the runtime filling
/// `D3D12DDI_TABLE_TYPE_COMMAND_LIST_3D` **twice** at device creation, indices 0
/// and 1, with two distinct handles (`0x3E0`, `0x638`), and WARP then calling
/// `pfnSetCommandListDDITableCb(hRTCommandList, 0x3E0)` — index **0** — on every
/// command-list create (`DDI_REFERENCE.md` §2.2, §9.3, §15.1 #9). Index 1 exists
/// so a driver can *swap* a list's table later: §9.3's design is a **recording**
/// table installed after a successful `pfnResetCommandList` and a
/// **closed/erroring** table installed after `pfnCloseCommandList`, which is how
/// a forwarder avoids an `if (!recording)` check at the top of all 75 recording
/// entry points. Helios fills both indices with the same content today
/// (`tables12::fill_command_list` does not vary by index), so index 1 would be
/// observationally identical — and picking it would be a claim this driver
/// cannot defend. ⇒ index 0, matching the only measurement.
///
/// # Safety
/// `h_device` must be a live device handle; `arg` must point at a live
/// `D3D12DDIARG_CREATE_COMMAND_LIST_0040`; `h_list`'s `pDrvPrivate` must address
/// the private block [`calc_private_command_list_size`] sized; `h_rt_list` must
/// be the runtime's handle for this list.
unsafe extern "C" fn create_command_list(
    h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_COMMAND_LIST_0040,
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_rt_list: ddi12::D3D12DDI_HRTCOMMANDLIST,
) -> ddi12::HRESULT {
    // SAFETY: the caller guarantees the slot lies in the sized private block.
    let Some(slot) =
        (unsafe { Slot::<Boxed<CommandListState>>::from_priv(h_list.drv_private()) })
    else {
        note_refusal(&L2_REFUSALS.command_list_bad_arg);
        return E_INVALIDARG;
    };
    // SAFETY: as above.
    unsafe { slot.clear() };

    if arg.is_null() {
        note_refusal(&L2_REFUSALS.command_list_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: non-null per the check; the DDI declares it `_In_ CONST`.
    let a = unsafe { &*arg };

    // ⛔⛔ **A BUNDLE is REFUSED HERE, at create, and that is the only honest
    // place for it.** `D3D12DDIARG_CREATE_COMMAND_RECORDER_0040` is
    // `{ QueueFlags, RecorderFlags }` and `D3D12DDI_COMMAND_QUEUE_FLAGS`
    // enumerates NONE / 3D / COMPUTE / COPY / PAGING / VIDEO_* with **no bundle
    // bit** — so `engine_list_type` can only ever answer DIRECT, COMPUTE or
    // COPY, and this driver can never mint a BUNDLE `ID3D12CommandAllocator`.
    // An `ID3D12CommandAllocator`'s class is fixed at creation and the engine
    // enforces the pairing unconditionally (`libs/vkd3d/bundle.c:239-245`,
    // `:411-427`), so a bundle list accepted here is a list whose
    // `pfnResetCommandList` is **guaranteed** to fail.
    //
    // ⛔ Accepting it and failing later is the "succeed at create, fail at
    // submit" shape this project forbids. Worse, concretely: the reset arm's
    // only channel is an error callback, so every bundle a legal application
    // records would have taken the device — the compositor's, if DWM is the
    // application. Refusing at create instead fails
    // `ID3D12Device::CreateCommandList` with an HRESULT the application
    // receives and can act on, which is what a driver that lacks a capability
    // is supposed to do.
    //
    // ⚠ **This is a real capability gap and the counter is not a formality.**
    // Bundles are core D3D12 with no cap to decline them, so a workload that
    // uses them loses work here. The fix is one `ID3D12CommandAllocator` per
    // (pool, class) — `CreateCommandAllocator(BUNDLE)` on first use by a bundle
    // list — and the commit that lands it deletes this arm. Until then the
    // refusal is visible, counted, and survivable.
    if a.Type == ddi12::D3D12DDI_COMMAND_LIST_TYPE_D3D12DDI_COMMAND_LIST_TYPE_BUNDLE {
        note_refusal(&L2_REFUSALS.bundle_list_refused);
        if let Some(n) = budget(&LIST_LOG) {
            log_error!(
                "CreateCommandList: BUNDLE refused -- D3D12DDIARG_CREATE_COMMAND_RECORDER_0040 \
                 carries no bundle bit, so this driver can never mint a BUNDLE command \
                 allocator and the paired ResetCommandList could not succeed (x{})",
                n + 1,
            );
        }
        return E_INVALIDARG;
    }

    // ⚠ `Type` is only DIRECT or BUNDLE (`d3d12umddi.h:1425-1429`); COMPUTE and
    // COPY are expressed through `QueueFlags`, not through a list type
    // (`DDI_REFERENCE.md` §8.1). BUNDLE is refused above, so the remaining
    // answer comes from `QueueFlags` alone.
    let list_type = {
        match engine_list_type(a.QueueFlags) {
            Some(t) => t,
            None => {
                note_refusal(&L2_REFUSALS.command_list_class_unsupported);
                if let Some(n) = budget(&LIST_LOG) {
                    log_error!(
                        "CreateCommandList: Type={} QueueFlags={:#x} names no class this driver \
                         backs -> E_INVALIDARG (x{})",
                        a.Type,
                        a.QueueFlags,
                        n + 1,
                    );
                }
                return E_INVALIDARG;
            }
        }
    };

    // ⚠ `D3D12DDI_COMMAND_LIST_FLAGS` carries two marker hints
    // (`ENABLE_MARKERS`, `_0010_ENABLE_FULLPIPELINE_MARKERS`) that the **API**
    // `D3D12_COMMAND_LIST_FLAGS` has no counterpart for — it defines only `NONE`.
    // They are debug-tooling hints, so they are dropped and counted rather than
    // refused.
    if a.CommandListFlags
        != ddi12::D3D12DDI_COMMAND_LIST_FLAGS_D3D12DDI_COMMAND_LIST_FLAG_NONE
    {
        note_refusal(&L2_REFUSALS.command_list_flags_ignored);
    }

    // SAFETY: device-scope DDI; the borrow lives only until the end of the call.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&L2_REFUSALS.command_list_no_device);
        return E_FAIL;
    };
    let Some(engine) = dev.engine.d3d12_device() else {
        note_refusal(&L2_REFUSALS.command_list_no_device);
        return E_FAIL;
    };
    // ⭐ `ID3D12Device4` for `CreateCommandList1` — the entry point that returns a
    // **closed** list bound to no allocator, which is the only shape the DDI's
    // create args can describe (module doc). `cast` is a `QueryInterface` on the
    // engine's own object; it costs one vtable call and one AddRef that the
    // returned wrapper releases.
    let engine4 = match engine.cast::<ID3D12Device4>() {
        Ok(d) => d,
        Err(e) => {
            note_refusal(&L2_REFUSALS.command_list_engine_failed);
            if let Some(n) = budget(&LIST_LOG) {
                log_error!(
                    "CreateCommandList: engine has no ID3D12Device4 (CreateCommandList1) \
                     hr={:#010x} (x{})",
                    e.code().0 as u32,
                    n + 1,
                );
            }
            return E_FAIL;
        }
    };

    // SAFETY: all three arguments are by-value scalars; the out-param is the
    // wrapper's own.
    let created = unsafe {
        engine4.CreateCommandList1::<ID3D12GraphicsCommandList>(
            a.NodeMask,
            list_type,
            D3D12_COMMAND_LIST_FLAG_NONE,
        )
    };
    let list = match created {
        Ok(l) => l,
        Err(e) => {
            note_refusal(&L2_REFUSALS.command_list_engine_failed);
            if let Some(n) = budget(&LIST_LOG) {
                log_error!(
                    "CreateCommandList: engine CreateCommandList1(type={}) failed hr={:#010x} \
                     (x{})",
                    list_type.0,
                    e.code().0 as u32,
                    n + 1,
                );
            }
            return E_FAIL;
        }
    };

    // ── The mandatory DDI-table callback ────────────────────────────────────
    // SAFETY: `dev` is the live device borrowed above and `h_rt_list` is the
    // runtime's handle for the command list being created — this call site is
    // inside `pfnCreateCommandList`, which is `set_command_list_ddi_table`'s
    // precondition and the moment the runtime requires the call.
    if !unsafe { set_command_list_ddi_table(dev, h_rt_list) } {
        // ⚠ Not fatal, and that is deliberate: the runtime's own answer to a
        // missing or invalid call is to install its stubs, not to fail the
        // create. Failing here would turn a recoverable mis-wiring into a dead
        // device, and the counter already names it.
        if let Some(n) = budget(&LIST_LOG) {
            log_error!(
                "CreateCommandList: this list will record through the RUNTIME's stub table, not \
                 this driver's (x{})",
                n + 1,
            );
        }
    }

    // SAFETY: the slot lies in the sized private block and is currently null;
    // `store` boxes the state and moves the box into it, so the slot owns both
    // the box and, through it, the single reference `CreateCommandList1`
    // returned.
    unsafe {
        slot.store(CommandListState {
            h_device,
            h_rt_list,
            engine: list,
            list_type,
        });
    }
    S_OK
}

/// Hand the runtime this list's DDI table, from command-list table index 0.
///
/// Returns `false` when the table could not be handed over, which the caller
/// logs — see [`create_command_list`]'s doc for what the runtime then does.
///
/// # Safety
/// `dev` must be a live device and `h_rt_list` the runtime's handle for the
/// command list currently being created.
unsafe fn set_command_list_ddi_table(
    dev: &device12::HeliosD3D12Device,
    h_rt_list: ddi12::D3D12DDI_HRTCOMMANDLIST,
) -> bool {
    // ⭐ Index 0 — the only index `D12-G5` ever saw a driver use. See
    // `create_command_list`'s doc, and `tables12::command_list_rt_table`.
    let handle = tables12::command_list_rt_table(0);
    if handle == 0 {
        note_refusal(&L2_REFUSALS.command_list_rt_table_missing);
        return false;
    }
    if dev.um_callbacks.is_null() {
        note_refusal(&L2_REFUSALS.command_list_ddi_table_cb_missing);
        return false;
    }
    // SAFETY: `um_callbacks` was null-checked in `create_device` and is the
    // runtime's `_0062` table, which outlives the device.
    let Some(cb) = (unsafe { (*dev.um_callbacks).pfnSetCommandListDDITableCb }) else {
        note_refusal(&L2_REFUSALS.command_list_ddi_table_cb_missing);
        return false;
    };
    let h_rt_table = ddi12::D3D12DDI_HRTTABLE {
        handle: handle as *mut c_void,
    };
    // SAFETY: a non-null callback from the runtime's own table, given the
    // runtime's own command-list handle and the `D3D12DDI_HRTTABLE` the runtime
    // itself passed to `pfnFillDDITable` for command-list table index 0.
    unsafe { cb(h_rt_list, h_rt_table) };
    true
}

/// `pfnDestroyCommandList`.
///
/// # Safety
/// `h_list` must be a handle [`create_command_list`] returned `S_OK` for and
/// which has not already been destroyed.
unsafe extern "C" fn destroy_command_list(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(slot) =
        (unsafe { Slot::<Boxed<CommandListState>>::from_priv(h_list.drv_private()) })
    else {
        note_refusal(&L2_REFUSALS.command_list_bad_arg);
        return;
    };
    // SAFETY: the slot holds either null or the one box `create_command_list`
    // moved in; `take` empties it, so a second destroy is a no-op rather than a
    // double free. Dropping the box releases the engine list's single reference.
    let Some(state) = (unsafe { slot.take() }) else {
        note_refusal(&L2_REFUSALS.command_list_bad_arg);
        return;
    };
    drop(state);
}

// ---------------------------------------------------------------------------
// (d) Command signatures — 3 slots, REFUSED
// ---------------------------------------------------------------------------

/// `pfnCalcPrivateCommandSignatureSize`.
///
/// Answers the ordinary one-word size even though [`create_command_signature`]
/// refuses, so the runtime allocates a well-formed slot that the create leaves
/// null and the destroy finds empty. ⛔ Answering 0 here would be worse than
/// refusing: it hands the paired create a zero-byte region (see
/// [`PRIVATE_SLOT_SIZE`]).
///
/// # Safety
/// `arg`, when non-null, must point at a live
/// `D3D12DDIARG_CREATE_COMMAND_SIGNATURE_0001` for the duration of the call.
unsafe extern "C" fn calc_private_command_signature_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_COMMAND_SIGNATURE_0001,
) -> ddi12::SIZE_T {
    if arg.is_null() {
        note_refusal(&L2_REFUSALS.command_signature_bad_arg);
    }
    PRIVATE_SLOT_SIZE as ddi12::SIZE_T
}

/// `pfnCreateCommandSignature` — **REFUSED**, `CommandSignatureRefused`.
///
/// ⛔ **Refused rather than half-implemented, for one reason that is not about
/// effort.** `ID3D12Device::CreateCommandSignature` needs two things this lane
/// cannot produce correctly today:
///
/// * a translation of `D3D12DDI_INDIRECT_ARGUMENT_DESC` into
///   `D3D12_INDIRECT_ARGUMENT_DESC`. These are two independently versioned
///   tagged unions, and `DDI_REFERENCE.md` §9.6.1 is the standing evidence that
///   a DDI enum and its API twin can collide on a value with different meanings
///   while the member types keep the compiler silent. That translation deserves
///   the lane that also implements `pfnExecuteIndirect` (L3a) and can test it;
/// * `D3D12DDIARG_CREATE_COMMAND_SIGNATURE_0001::hRootSignature`, whose payload
///   type is **L6's** to declare (`pso.rs`). `DECISIONS.md` D13's discipline is
///   that a handle's payload has one declaration, in the lane that owns it, so
///   reading it from here would be the second declaration.
///
/// ⚠ Nothing is lost this round: its only consumer is `pfnExecuteIndirect`,
/// which is L3a's and still a counting noop, and `DDI_REFERENCE.md` §14.2's
/// 99-slot minimum-viable list does not include the command-signature triple.
///
/// # Safety
/// `h_signature`'s `pDrvPrivate`, when non-null, must address the private block
/// [`calc_private_command_signature_size`] sized.
unsafe extern "C" fn create_command_signature(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _arg: *const ddi12::D3D12DDIARG_CREATE_COMMAND_SIGNATURE_0001,
    h_signature: ddi12::D3D12DDI_HCOMMANDSIGNATURE,
) -> ddi12::HRESULT {
    // Leave a null slot behind, so the paired destroy has a defined thing to
    // find. ⚠ `Slot<Com<…>>` only to reach `clear`, which is payload-agnostic:
    // nothing is ever stored here. ⚠ And `pDrvPrivate` is read directly rather
    // than through `DdiHandle::drv_private`, because this lane deliberately
    // declares **no** payload for `D3D12DDI_HCOMMANDSIGNATURE` — the handle
    // carries nothing, and a marker impl saying otherwise would be a claim about
    // an object that is never built.
    // SAFETY: the caller guarantees the slot lies in the sized private block.
    if let Some(slot) =
        unsafe { Slot::<Com<ID3D12GraphicsCommandList>>::from_priv(h_signature.pDrvPrivate) }
    {
        // SAFETY: as above; `clear` writes one null word and touches nothing it
        // pointed at.
        unsafe { slot.clear() };
    } else {
        note_refusal(&L2_REFUSALS.command_signature_bad_arg);
    }
    note_refusal(&L2_REFUSALS.command_signature_refused);
    E_NOTIMPL
}

/// `pfnDestroyCommandSignature`.
///
/// Nothing was ever stored, so this only counts. A hit means the runtime
/// destroyed a signature after [`create_command_signature`] refused it, which is
/// legal and worth knowing.
///
/// # Safety
/// `_h_signature` must be a handle the runtime associated with a
/// `pfnCreateCommandSignature` call on this device.
unsafe extern "C" fn destroy_command_signature(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _h_signature: ddi12::D3D12DDI_HCOMMANDSIGNATURE,
) {
    note_refusal(&L2_REFUSALS.command_signature_destroy_unexpected);
}

// ---------------------------------------------------------------------------
// The command-queue table — 7 slots
// ---------------------------------------------------------------------------

/// `pfnExecuteCommandLists` — the **only** submission entry point in the
/// baseline set (`DDI_REFERENCE.md` §5), and the one queue slot `D12-G5` ever
/// saw called.
///
/// ⚠ Read the module doc's last section before adding the WDDM half.
///
/// # Safety
/// `h_queue` must be a live queue handle; `lists` must address `count` readable
/// `D3D12DDI_HCOMMANDLIST`s, each a live handle from [`create_command_list`].
unsafe extern "C" fn execute_command_lists(
    h_queue: ddi12::D3D12DDI_HCOMMANDQUEUE,
    count: ddi12::UINT,
    lists: *const ddi12::D3D12DDI_HCOMMANDLIST,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_queue`.
    let Some(queue) = (unsafe { queue_state(h_queue) }) else {
        note_refusal(&L2_REFUSALS.execute_command_lists_bad_arg);
        return;
    };
    let n = count as usize;
    if n == 0 {
        // Legal and degenerate: nothing to submit, nothing to report.
        return;
    }
    // ⛔ Validate the runtime-supplied count and pointer BEFORE reading the
    // array. CLAUDE.md's rule, and the bound is `MAX_EXECUTE_COMMAND_LISTS`.
    if lists.is_null() || n > MAX_EXECUTE_COMMAND_LISTS {
        note_refusal(&L2_REFUSALS.execute_command_lists_bad_arg);
        // ⚠ `k`, not `n`: `n` is the list count in this function and shadowing it
        // with a log ordinal is how a line ends up reporting the wrong number.
        if let Some(k) = budget(&ECL_LOG) {
            log_error!(
                "ExecuteCommandLists: Count={count} pCommandLists={lists:p} -- refused (x{})",
                k + 1,
            );
        }
        return;
    }

    // ⚠ The per-list trace detail is formatted only when the trace gate is
    // ALREADY open. This is per-submit traffic and the loop below is on it; a
    // `String` built unconditionally would be R420's cost with none of its
    // evidence.
    let tracing = crate::log::trace_enabled();
    let mut traced_lists = String::new();

    // ⚠ One `AddRef`/`Release` pair per list per submit, on purpose. The engine's
    // wrapper takes `&[Option<ID3D12CommandList>]`, i.e. owned references, while
    // each slot only *lends* one; cloning is the encoding of that difference
    // that cannot be got wrong. The alternative — reinterpreting the slot words
    // as an `Option<Interface>` array — is a layout assumption about someone
    // else's crate on a path nobody has measured.
    let mut engine_lists: Vec<Option<ID3D12CommandList>> = Vec::with_capacity(n);
    for i in 0..n {
        // SAFETY: `lists` is non-null and `i < n <= count`, so this element is
        // inside the array the DDI declares `_In_reads_(Count)`.
        let h = unsafe { *lists.add(i) };
        // SAFETY: the caller guarantees each entry is a live handle from
        // `create_command_list`, so its slot lies in the sized private block.
        let state = unsafe { command_list_state(h) };
        let Some(state) = state else {
            note_refusal(&L2_REFUSALS.execute_command_lists_list_missing);
            // ⚠ `k`, not `n` — see the refusal above.
            if let Some(k) = budget(&ECL_LOG) {
                log_error!(
                    "ExecuteCommandLists: entry {i} of {count} carries no engine command list \
                     (x{})",
                    k + 1,
                );
            }
            return;
        };
        // `ID3D12GraphicsCommandList` derefs to its COM base `ID3D12CommandList`
        // (single inheritance, the `windows` crate's own `interface_hierarchy!`);
        // `clone` is the AddRef the vector's drop then balances.
        engine_lists.push(Some((**state.engine()).clone()));
        if tracing {
            // ⭐ Both identities, because neither alone answers the question
            // `tmp/dx12/gates/G8-r0/RESULT.md` asked and never got an instrument
            // for: `pDrvPrivate` ties this entry back to the `CreateCommandList`
            // / `ResetCommandList` / `CloseCommandList` lines for the SAME list,
            // and the engine pointer is what a vkd3d log names. Without the
            // pair, "the recorded commands reached the submitted list" is an
            // inference across two logs that do not share a vocabulary.
            // ⚠ The deref is to the COM base `ID3D12CommandList` — the exact
            // interface pushed above — so the printed pointer is the one handed
            // to the engine, not a sibling QI of it.
            traced_lists.push_str(&format!(
                " [{i}]priv={:p},list={:p}",
                h.drv_private(),
                (**state.engine()).as_raw(),
            ));
        }
    }

    // ⚠ Emitted BEFORE the forward, deliberately: if the engine call ever wedges
    // (it did, in teardown, in the `G8-r0-settle` round), this line is the last
    // thing that says what was being submitted when it did.
    //
    // ⭐ FB-1's reading, on the same line rather than a second one: the three
    // latched context windows AS THEY STAND ON ENTRY to this submit. That is the
    // WDDM submission's whole precondition — a command window and its capacity —
    // and printing it here makes "the window rotated" visible as a changing
    // pointer across submits, which is the only direct evidence that
    // `pfnRenderCb`'s re-latch is doing anything. ⚠ Formatted only when the trace
    // gate is already open, like `traced_lists` above; the lock is taken and
    // released inside the guard scope, so the tracing arm cannot hold it into the
    // engine forward.
    if tracing {
        let windows = lock_windows(queue);
        let (cmd, cmd_cap) = window_parts(&windows.command);
        let (alloc, alloc_cap) = window_parts(&windows.allocations);
        let (patch, patch_cap) = window_parts(&windows.patches);
        drop(windows);
        trace_line!(
            "ExecuteCommandLists: Count={count} queue={:p} ctx={:p} cmd={cmd:p}/{cmd_cap} \
             allocList={alloc:p}/{alloc_cap} patchList={patch:p}/{patch_cap}{traced_lists}",
            queue.engine_queue.as_raw(),
            queue.h_context,
        );
    }

    // SAFETY: `engine_lists` is a live slice of owned interfaces for the whole
    // call, and `engine_queue` is the live queue this state owns.
    unsafe { queue.engine_queue.ExecuteCommandLists(&engine_lists) };

    // ⭐ `bump`, not `note_refusal`: the `EclNoWddmSubmission` line immediately
    // below already emits this set's summary on its first hit, and R911 is
    // explicit that an already-loud arm must not emit it a second time for one
    // event — here that would be the whole ~300-counter line twice per submit's
    // first occurrence. The count is still readable, because it is inside the
    // very summary that call prints.
    L2_REFUSALS.ecl_forwarded.bump();

    // ⛔ The WDDM half is NOT here. `ResourceHeaps.md:1678` requires a kernel
    // submission during this DDI, on this thread, with a context minted at queue
    // creation — the context exists (see the module doc), the watermark does not.
    // ⚠ And the callback is already fixed: this queue's context is LEGACY, so the
    // submission that belongs here is `pfnRenderCb`, not
    // `pKTCallbacks->pfnSubmitCommandCb` (`DDI_REFERENCE.md` §6.4's submission
    // row). Counted on every forward so the gap is a number.
    note_refusal(&L2_REFUSALS.ecl_no_wddm_submission);

    // ⛔⛔ DIAGNOSTIC ARM — INERT BY DEFAULT (`Umd12EclDelayUs` absent = 0 = no
    // delay, so a run with no knob set is byte-identical to the build that never
    // heard of it). It is a producer-side CPU stall, which
    // `umd/src/knobs.rs:31-43` forbids as a *fix*; it is legal here only as a
    // MEASUREMENT with a question attached, and it must be DELETED by the commit
    // that lands the `pfnRenderCb` WDDM submission this DDI is missing.
    //
    // The question — *where does the runtime's fence advance become downstream
    // of this driver?* `D12-G8` rung 0's fence completes with no causal
    // dependency on the engine's work. Delay only this DDI, and read the probe's
    // own `WaitForSingleObject signalled in N us`:
    //   N >= the delay ⇒ the advance is downstream of THIS DDI returning, which
    //                    is exactly where the submission goes. The best case.
    //   N ~ 1 µs, and the `pfnSignalFence` arm reads the same ⇒ the runtime
    //                    advances the fence independently of both DDIs, so the
    //                    submission's precondition is in doubt and must be
    //                    settled directly, in the kernel, by holding a DMA
    //                    packet and watching the app's wait.
    // `knobs12::UMD12_ECL_DELAY_US` carries the full table and the citations.
    let ecl_delay_us = crate::knobs12::umd12_ecl_delay_us();
    if ecl_delay_us != 0 {
        note_refusal(&L2_REFUSALS.ecl_delayed);
        std::thread::sleep(Duration::from_micros(u64::from(ecl_delay_us)));
    }
}

/// `pfnUnused` — the header's own name for queue-table slot 1.
///
/// ⛔ **A NULL is not the answer here even though WARP writes one.**
/// `DDI_REFERENCE.md` §14.1 states the rule for this slot in one line: *"a stub
/// costs nothing and turns 'the header lied' into a counter instead of a jump
/// through a null pointer"*, and §14.1.1 classifies it as **RESERVED** — a slot
/// that never had a function, as against `cl[69]`'s **RETIRED** and
/// `core[121]`'s **OPTIONAL FEATURE**.
///
/// **What a driver legitimately puts here**, and why this shape: the field is a
/// bare `void*`, so the header states no signature at all and none can be
/// written. A nullary `unsafe extern "C" fn` is nevertheless safe to *enter*
/// under the Microsoft x64 ABI whatever the caller believed it was calling — the
/// caller owns the stack and the shadow space, arguments live in volatile
/// registers this body never reads, and there is nothing to clean up on return.
/// So the worst case if the header ever lies is a caller reading an
/// uninitialised `RAX`, against a guaranteed access violation for the NULL. The
/// counter is what makes that case visible instead of silent, and it is a real
/// instrument rather than a decoration: it moves the first time this slot is
/// ever called by anything.
unsafe extern "C" fn queue_unused_slot() {
    note_refusal(&L2_REFUSALS.queue_unused_slot_called);
}

/// `pfnUnused2` — queue-table slot 2. Separate function, separate counter, for
/// the one reason that matters: a shared body could not say *which* of the two
/// the runtime called, and that is the entire content of the observation.
unsafe extern "C" fn queue_unused2_slot() {
    note_refusal(&L2_REFUSALS.queue_unused2_slot_called);
}

/// `pfnUpdateTileMappings` — **REFUSED**, `TileMappingsRefused`.
///
/// ⛔ This driver reports `TiledResourcesTier = NOT_SUPPORTED` (`caps12.rs`), so
/// no tiled resource can exist for this DDI to remap. Refusing is the coherent
/// answer, and it is the same shape `caps12::get_mip_packing` takes for the same
/// reason: counted, and deliberately **not** raised through `pfnSetErrorCb`,
/// because a hit means a caps inconsistency somewhere else and removing the
/// device would not fix it. ⚠ No log line either: the counter is the readout,
/// and this DDI is per-remap traffic that a budgeted line would only half cover.
///
/// ⚠ **`DX12.md` §4.4 makes `TiledResourcesTier >= 2` a feature-level 12_1
/// floor**, and it lands with these two slots plus `pfnCopyTiles`,
/// `pfnGetMipPacking` and the reserved-resource arm of `pfnCreateHeapAndResource`.
/// ⛔ The tier is **UMD-only** — Vulkan sparse binding, which the guest supports
/// end to end (`DECISIONS.md` §2) — and **not** a KMD dependency. That claim was
/// made twice and falsified twice; do not cost the feature level as if the KMD
/// were on its critical path.
///
/// # Safety
/// The arguments are the runtime's and this body reads none of them.
unsafe extern "C" fn update_tile_mappings(
    _h_queue: ddi12::D3D12DDI_HCOMMANDQUEUE,
    _h_resource: ddi12::D3D12DDI_HRESOURCE,
    _num_regions: ddi12::UINT,
    _region_start_coords: *const ddi12::D3D12DDI_TILED_RESOURCE_COORDINATE,
    _region_sizes: *const ddi12::D3D12DDI_TILE_REGION_SIZE,
    _h_heap: ddi12::D3D12DDI_HHEAP,
    _num_ranges: ddi12::UINT,
    _range_flags: *const ddi12::D3D12DDI_TILE_RANGE_FLAGS,
    _heap_start_offsets: *const ddi12::UINT,
    _range_tile_counts: *const ddi12::UINT,
    _flags: ddi12::D3D12DDI_TILE_MAPPING_FLAGS,
) {
    note_refusal(&L2_REFUSALS.tile_mappings_refused);
}

/// `pfnCopyTileMappings` — **REFUSED**, `TileMappingsRefused`. Same reasoning as
/// [`update_tile_mappings`]; same counter, because the two are one capability.
///
/// # Safety
/// The arguments are the runtime's and this body reads none of them.
unsafe extern "C" fn copy_tile_mappings(
    _h_queue: ddi12::D3D12DDI_HCOMMANDQUEUE,
    _h_dst_resource: ddi12::D3D12DDI_HRESOURCE,
    _dst_start_coord: *const ddi12::D3D12DDI_TILED_RESOURCE_COORDINATE,
    _h_src_resource: ddi12::D3D12DDI_HRESOURCE,
    _src_start_coord: *const ddi12::D3D12DDI_TILED_RESOURCE_COORDINATE,
    _region_size: *const ddi12::D3D12DDI_TILE_REGION_SIZE,
    _flags: ddi12::D3D12DDI_TILE_MAPPING_FLAGS,
) {
    note_refusal(&L2_REFUSALS.tile_mappings_refused);
}

/// What a fence operation is: the two queue slots differ only in which engine
/// method they reach, so they share [`fence_operation`] and name themselves here.
#[derive(Clone, Copy)]
enum FenceOp {
    Signal,
    Wait,
}

impl FenceOp {
    fn name(self) -> &'static str {
        match self {
            FenceOp::Signal => "SignalFence",
            FenceOp::Wait => "WaitForFence",
        }
    }
}

/// The body behind `pfnSignalFence` and `pfnWaitForFence`.
///
/// ⭐ **`PhysicalAdapterMask` is an OUT parameter** — `d3d12umddi.h:2716` marks it
/// `// Out:` and `DDI_REFERENCE.md` §10.2 spells out what it means: the *driver*
/// tells the runtime which adapters the operation must be broadcast to. On
/// single-adapter Helios that is `1`, and it is written on **every** path,
/// including the refusals, because the runtime reads the field back regardless
/// and leaving its own struct untouched turns "we could not answer" into whatever
/// was there.
///
/// ⚠ These forward to the **engine** fence, not to the kernel.
/// `DDI_REFERENCE.md` §10.3: there is no CPU-signal and no CPU-wait DDI, and
/// these two are *ordering instructions to the driver's own pipeline* — the
/// kernel-side monitored-fence signal/wait is the runtime's. §14.0 also measured
/// that WARP was **never** called here across 20 frames of
/// `ID3D12CommandQueue::Signal` + `SetEventOnCompletion`, so a zero reading on
/// these two is the expected shape rather than evidence they do not work. ⛔ It
/// is equally not evidence that they *do*: a zero reading proves nothing about a
/// path, and WARP is one software-scheduled implementation rather than the
/// contract.
///
/// ⛔⛔ **The two directions are NOT symmetric, and that asymmetry is the whole
/// shape of this function.** A signal can only advance a timeline, so forwarding
/// it is always safe. A **wait** on the engine fence for a value the engine
/// timeline can never reach does not fail — it blocks that vkd3d queue forever.
/// `fence::FenceState`'s module doc has the two reachable ways the shadow gets
/// behind the runtime's fence (a `CreateFence` initial value and a CPU
/// `ID3D12Fence::Signal`, neither of which reaches this DDI). ⇒ a wait above the
/// watermark this driver has itself signalled is **not forwarded**: it is counted
/// as `FenceWaitNotForwarded` and left to the ordering §10.3 says the runtime
/// performs itself. Read `fence.rs`'s module doc before changing either arm — the
/// cost of that choice is named there too.
///
/// # Safety
/// `h_queue` must be a live queue handle and `op_arg` must address one writable
/// `D3D12DDIARG_FENCE_OPERATION` the runtime owns.
unsafe fn fence_operation(
    which: FenceOp,
    h_queue: ddi12::D3D12DDI_HCOMMANDQUEUE,
    op_arg: *mut ddi12::D3D12DDIARG_FENCE_OPERATION,
) {
    if op_arg.is_null() {
        note_refusal(&L2_REFUSALS.fence_op_bad_arg);
        return;
    }
    // SAFETY: non-null per the check; the DDI declares it a writable pointer to
    // one struct the runtime owns for the duration of the call.
    let op = unsafe { &mut *op_arg };
    // ⛔ Written before anything can fail — see the doc above.
    op.PhysicalAdapterMask = 1;

    // SAFETY: the caller guarantees a live handle from `create_command_queue`.
    let Some(queue) = (unsafe { queue_state(h_queue) }) else {
        note_refusal(&L2_REFUSALS.fence_op_bad_arg);
        return;
    };
    // SAFETY: `op.Fence` is the handle the runtime associated with a
    // `pfnCreateFence` on this device; `fence_state` borrows the fence's state
    // for the rest of this call only.
    let Some(fence_state) = (unsafe { fence::fence_state(op.Fence) }) else {
        note_refusal(&L2_REFUSALS.fence_op_fence_missing);
        return;
    };

    // ⚠ Emitted before either arm can return, so a refused wait is as visible as
    // a forwarded one. `pDrvPrivate` is what ties this line to L7's
    // `CreateFence: valueVA=… monitoredVA=… flags=…` for the SAME fence — the
    // runtime hands the driver no other name for it (`FENCE-BRIDGE-DESIGN.md`
    // §1.3), so without it two fences in one process are indistinguishable here.
    trace_line!(
        "{}: value={} fence={:p}",
        which.name(),
        op.Value,
        op.Fence.drv_private(),
    );

    match which {
        // ⚠ The watermark is raised BEFORE the engine call — `note_signal`'s doc
        // has the ordering argument.
        FenceOp::Signal => fence_state.note_signal(op.Value),
        FenceOp::Wait => {
            if !fence_state.signal_reachable(op.Value) {
                note_refusal(&L2_REFUSALS.fence_wait_not_forwarded);
                if let Some(n) = budget(&FENCE_OP_LOG) {
                    log_error!(
                        "WaitForFence: value={} is above this driver's signalled watermark -- not \
                         forwarded, because an engine wait for a value the engine timeline cannot \
                         reach never completes (x{})",
                        op.Value,
                        n + 1,
                    );
                }
                return;
            }
        }
    }

    // SAFETY: both take a borrowed fence and a by-value `u64`; the queue and the
    // fence are live for the call.
    let result = unsafe {
        match which {
            FenceOp::Signal => queue.engine_queue.Signal(fence_state.engine(), op.Value),
            FenceOp::Wait => queue.engine_queue.Wait(fence_state.engine(), op.Value),
        }
    };
    let Err(e) = result else {
        // ⭐⭐ THE SUCCESS PATH, counted — and it was not, until the F1 round.
        // `tmp/dx12/gates/G8-r0/RESULT.md` claimed *"the queue-table `Signal`
        // path ran"* on the strength of `FenceOpEngineFailed = FenceOpBadArg =
        // FenceWaitNotForwarded = 0`, i.e. three ZERO readings, while this
        // branch incremented nothing and logged nothing. A zero reading is not
        // evidence a path works — this file says so 40 lines above, and
        // `fence.rs:61-64` says it again — so the claim was unsupported and the
        // whole A-vs-C decision (`FENCE-BRIDGE-DESIGN.md` §5) rests on it.
        note_refusal(match which {
            FenceOp::Signal => &L2_REFUSALS.fence_signal_forwarded,
            FenceOp::Wait => &L2_REFUSALS.fence_wait_forwarded,
        });
        return;
    };

    let hr = e.code().0;
    note_refusal(&L2_REFUSALS.fence_op_engine_failed);
    if let Some(n) = budget(&FENCE_OP_LOG) {
        log_error!(
            "{}: engine failed value={} hr={:#010x} (x{})",
            which.name(),
            op.Value,
            hr as u32,
            n + 1,
        );
    }
    // ⭐ This slot returns `VOID`, so `pfnSetErrorCb` is the only channel it has
    // (`DECISIONS.md` §7.6) and it is **device**-scoped: there is no per-queue
    // error callback in `D3D12DDI_CORELAYER_DEVICECALLBACKS_0062`. A queue whose
    // fence ordering silently did not happen is a correctness failure the
    // application must be told about, which is what separates this from the
    // tiled-resource refusals above.
    // SAFETY: `h_device` is the device this queue was created against; the
    // borrow lives only until the end of this block.
    let reported = unsafe { device12::device(queue.h_device) }
        .is_some_and(|dev| device12::set_error(dev, hr));
    if !reported {
        note_refusal(&L2_REFUSALS.queue_set_error_unavailable);
    }
}

/// `pfnSignalFence`.
///
/// # Safety
/// As [`fence_operation`].
unsafe extern "C" fn signal_fence(
    h_queue: ddi12::D3D12DDI_HCOMMANDQUEUE,
    op_arg: *mut ddi12::D3D12DDIARG_FENCE_OPERATION,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is `fence_operation`'s.
    unsafe { fence_operation(FenceOp::Signal, h_queue, op_arg) }

    // ⛔⛔ DIAGNOSTIC ARM — INERT BY DEFAULT (`Umd12FenceSignalDelayUs` absent =
    // 0 = no delay, so a run with no knob set is byte-identical to the build
    // that never heard of it). It is a producer-side CPU stall, which
    // `umd/src/knobs.rs:31-43` forbids as a *fix*; it is legal here only as a
    // MEASUREMENT with a question attached, and it must be DELETED by the commit
    // that lands the `pfnRenderCb` WDDM submission.
    //
    // The question — *where does the runtime's fence advance become downstream
    // of this driver?* `D12-G8` rung 0's fence completes with no causal
    // dependency on the engine's work (`tmp/dx12/gates/G8-r0-settle/`: the app's
    // wait returns in 1.1 µs, the surface is 0/65536 exact at T+0 and
    // 65536/65536 at +2000 ms through the same mapping). Delay only THIS DDI,
    // and read the probe's own `WaitForSingleObject signalled in N us`:
    //   N >= the delay ⇒ the advance is downstream of `pfnSignalFence`
    //                    RETURNING, so the submission must be in place by then;
    //   N ~ 1 µs       ⇒ it is not this DDI, and the `Umd12EclDelayUs` arm says
    //                    whether it is `pfnExecuteCommandLists` or neither.
    // ⚠ `FenceSignalForwarded = 0` makes this arm's reading UNOBSERVABLE rather
    // than negative: a delay on a DDI the runtime never enters cannot be seen,
    // and that absence is itself a fact the submission design must accommodate.
    // ⛔ Placed AFTER the whole forward and OUTSIDE any early return, because the
    // thing under test is when this DDI *returns to the runtime*, not what
    // happened inside it. `knobs12::UMD12_FENCE_SIGNAL_DELAY_US` carries the
    // reading table and the citations.
    let delay_us = crate::knobs12::umd12_fence_signal_delay_us();
    if delay_us != 0 {
        note_refusal(&L2_REFUSALS.fence_signal_delayed);
        std::thread::sleep(Duration::from_micros(u64::from(delay_us)));
    }
}

/// `pfnWaitForFence`.
///
/// # Safety
/// As [`fence_operation`].
unsafe extern "C" fn wait_for_fence(
    h_queue: ddi12::D3D12DDI_HCOMMANDQUEUE,
    op_arg: *mut ddi12::D3D12DDIARG_FENCE_OPERATION,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is `fence_operation`'s.
    unsafe { fence_operation(FenceOp::Wait, h_queue, op_arg) }
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// Install L2's 17 device-core slots.
///
/// Chain position: `CapsSlots` -> `QueueSlots` on the device-core table.
pub(crate) fn install_core(
    mut filling: Filling<'_, DeviceCoreTable, stage::CapsSlots>,
) -> Filling<'_, DeviceCoreTable, stage::QueueSlots> {
    let table = filling.table();
    // command queues — 3
    table.pfnCalcPrivateCommandQueueSize = Some(calc_private_command_queue_size);
    table.pfnCreateCommandQueue = Some(create_command_queue);
    table.pfnDestroyCommandQueue = Some(destroy_command_queue);
    // command pools — 4
    table.pfnCalcPrivateCommandPoolSize = Some(calc_private_command_pool_size);
    table.pfnCreateCommandPool = Some(create_command_pool);
    table.pfnDestroyCommandPool = Some(destroy_command_pool);
    table.pfnResetCommandPool = Some(reset_command_pool);
    // command lists — 3
    table.pfnCalcPrivateCommandListSize = Some(calc_private_command_list_size);
    table.pfnCreateCommandList = Some(create_command_list);
    table.pfnDestroyCommandList = Some(destroy_command_list);
    // command recorders — 4
    table.pfnCalcPrivateCommandRecorderSize = Some(calc_private_command_recorder_size);
    table.pfnCreateCommandRecorder = Some(create_command_recorder);
    table.pfnDestroyCommandRecorder = Some(destroy_command_recorder);
    table.pfnCommandRecorderSetCommandPoolAsTarget =
        Some(command_recorder_set_command_pool_as_target);
    // command signatures — 3, refused
    table.pfnCalcPrivateCommandSignatureSize = Some(calc_private_command_signature_size);
    table.pfnCreateCommandSignature = Some(create_command_signature);
    table.pfnDestroyCommandSignature = Some(destroy_command_signature);
    filling.advance()
}

/// Install all 7 command-queue slots.
///
/// Chain position: `Stubbed` -> `QueueSlots` on the command-queue table.
pub(crate) fn install_queue(
    mut filling: Filling<'_, CommandQueueTable, stage::Stubbed>,
) -> Filling<'_, CommandQueueTable, stage::QueueSlots> {
    let table = filling.table();
    table.pfnExecuteCommandLists = Some(execute_command_lists);
    // ⚠ These two are `void*` in the header, not a typed `Option<fn>`, so the
    // compiler cannot check them against a signature — there is none. The cast
    // is what a bare `void*` slot requires; see `queue_unused_slot`'s doc for why
    // a counting stub rather than a NULL.
    table.pfnUnused = queue_unused_slot as *mut c_void;
    table.pfnUnused2 = queue_unused2_slot as *mut c_void;
    table.pfnUpdateTileMappings = Some(update_tile_mappings);
    table.pfnCopyTileMappings = Some(copy_tile_mappings);
    table.pfnSignalFence = Some(signal_fence);
    table.pfnWaitForFence = Some(wait_for_fence);
    filling.advance()
}

// ---------------------------------------------------------------------------
// Refusal counters
// ---------------------------------------------------------------------------

/// L2's refusal counters. One instance, [`L2_REFUSALS`]; the set that prints
/// them is [`REFUSALS`].
pub(crate) struct L2Refusals {
    /// A queue slot was called with a null arg or a null `pDrvPrivate`, or a
    /// destroy hit an already-empty slot. **Expected 0.**
    queue_bad_arg: RefusalCounter,
    /// A queue slot could not reach the engine: the `hDevice` did not resolve, or
    /// the bridge carries no `ID3D12Device`. **Expected 0.**
    queue_no_device: RefusalCounter,
    /// `D3D12DDIARG_CREATECOMMANDQUEUE_0050::QueueFlags` named no 3D/COMPUTE/COPY
    /// class, so the create was refused. **Expected 0** on a graphics workload; a
    /// hit means an application asked for a paging or video queue, which this
    /// driver's caps do not offer.
    queue_class_unsupported: RefusalCounter,
    /// `ID3D12Device::CreateCommandQueue` on the engine failed.
    queue_engine_failed: RefusalCounter,
    /// A queue asked for `GLOBAL_REALTIME_PRIORITY` and did not get it.
    /// ⚠ May legitimately be non-zero; it is a scheduling hint against a
    /// software-scheduled adapter with one engine node.
    queue_creation_flags_ignored: RefusalCounter,
    /// A queue named a scheduling group and was created outside it. ⚠ Expected
    /// non-zero only once L9's `pfnCreateSchedulingGroup` stops being a counting
    /// noop; until then no group object exists to join.
    queue_scheduling_group_ignored: RefusalCounter,
    /// A queue asked for a `NodeMask` beyond the single node Helios advertises.
    /// ⛔ Expected 0 — a hit means `ARCHITECTURE.md` §13 UNVERIFIED-11's
    /// multi-adapter surface has been reached for real.
    queue_node_mask_ignored: RefusalCounter,
    /// The corelayer `pfnCreateContextCb` was missing, failed, or returned `S_OK`
    /// with a null `hContext`, and the queue create was **failed**.
    ///
    /// ⛔ **Expected 0, and this is the counter to read first if D3D12 dies at
    /// `CreateCommandQueue`.** The WDDM context can be minted nowhere else (the
    /// runtime enforces it: *"CreateContextCb or CreateContextVirtualCb called
    /// outside of queue creation."*), and a queue without one can never present
    /// or submit — so this lane fails loudly rather than handing back a queue
    /// that looks alive. The softening edit, if a gate ever needs it, is to log
    /// and continue with a null `h_context` instead of returning here.
    queue_context_failed: RefusalCounter,
    /// The corelayer `pfnDestroyContextCb` was missing or failed at queue
    /// teardown, so a WDDM context was leaked. **Expected 0.**
    queue_context_destroy_failed: RefusalCounter,
    /// A queue-table slot needed `pfnSetErrorCb` and there was none. **Expected
    /// 0** — it is the first member of `D3D12DDI_CORELAYER_DEVICECALLBACKS_0062`
    /// and the only error channel a `VOID` DDI has, so a hit means an error the
    /// runtime will never learn about.
    queue_set_error_unavailable: RefusalCounter,
    /// A pool slot was called with a null arg or a null `pDrvPrivate`, or a
    /// destroy hit an already-empty slot. **Expected 0.**
    pool_bad_arg: RefusalCounter,
    /// A pool slot could not reach the engine. **Expected 0.**
    pool_no_device: RefusalCounter,
    /// `ID3D12Device::CreateCommandAllocator` failed at the first bind, so this
    /// pool has no backing store. **Expected 0.**
    pool_allocator_engine_failed: RefusalCounter,
    /// A recorder of one class targeted a pool already backed by an allocator of
    /// another, and the existing allocator was kept.
    ///
    /// ⛔ **Expected 0, and a hit is a real finding** — it would mean the
    /// runtime reuses one command pool across queue classes, which this lane's
    /// lazy-allocator mapping (module doc) cannot honour because an
    /// `ID3D12CommandAllocator`'s type is fixed at creation. The fix, if it
    /// fires, is one allocator per (pool, class) rather than one per pool.
    pool_type_mismatch: RefusalCounter,
    /// `pfnResetCommandPool` ran on a pool no recorder had ever targeted, so
    /// there was no allocator to reset. ⚠ May legitimately be non-zero: a pool
    /// created and reset before any recording is a no-op, not a fault.
    pool_reset_no_allocator: RefusalCounter,
    /// `ID3D12CommandAllocator::Reset` failed. ⚠ May legitimately be non-zero:
    /// D3D12 requires the GPU to be done with the allocator's lists first, and
    /// that is the application's obligation, not the driver's.
    pool_reset_engine_failed: RefusalCounter,
    /// A recorder slot was called with a null arg or a null `pDrvPrivate`, or a
    /// destroy hit an already-empty slot. **Expected 0.**
    recorder_bad_arg: RefusalCounter,
    /// `D3D12DDIARG_CREATE_COMMAND_RECORDER_0040::QueueFlags` named no
    /// 3D/COMPUTE/COPY class. **Expected 0**, same reasoning as
    /// `QueueClassUnsupported`.
    recorder_class_unsupported: RefusalCounter,
    /// A command-list slot was called with a null arg or a null `pDrvPrivate`,
    /// **or a destroy hit an already-empty slot**. **Expected 0.**
    ///
    /// ⚠ The second condition was added when `pfnDestroyCommandList` started
    /// taking a box rather than releasing a bare COM word, and it is the same
    /// second condition `PoolBadArg` and `RecorderBadArg` already document — this
    /// was the one of the three that was missed. It fires when a create FAILED
    /// (leaving the slot cleared) and the runtime then destroyed the handle
    /// anyway, which is legal. ⇒ **read `CommandListEngineFailed`,
    /// `CommandListClassUnsupported` and `L2BundleListRefused` first**: a hit
    /// here with one of those non-zero is the runtime cleaning up after a refused
    /// create, not a bad pointer.
    command_list_bad_arg: RefusalCounter,
    /// A command-list slot could not reach the engine. **Expected 0.**
    command_list_no_device: RefusalCounter,
    /// `D3D12DDIARG_CREATE_COMMAND_LIST_0040`'s `Type`/`QueueFlags` pair named no
    /// class this driver backs. **Expected 0.**
    command_list_class_unsupported: RefusalCounter,
    /// The engine has no `ID3D12Device4`, or `CreateCommandList1` failed.
    ///
    /// ⛔ **Expected 0, and the first thing to read if D3D12 dies at
    /// `CreateCommandList`.** `CreateCommandList1` is the only entry point that
    /// produces a closed list bound to no allocator, which is the only shape the
    /// DDI's create args can describe (module doc). vkd3d-proton implements
    /// `ID3D12Device4`; that it does is UNVERIFIED here, because the engine
    /// submodule is not checked out on the host that wrote this lane.
    command_list_engine_failed: RefusalCounter,
    /// A command list carried `D3D12DDI_COMMAND_LIST_FLAGS` marker hints the API
    /// enum has no counterpart for, and they were dropped. ⚠ Expected non-zero
    /// under a debug layer or a PIX capture; they are tooling hints, not
    /// behaviour.
    command_list_flags_ignored: RefusalCounter,
    /// `tables12::command_list_rt_table(0)` was still 0 at command-list creation,
    /// so `pfnSetCommandListDDITableCb` could not be called with a valid handle.
    ///
    /// ⛔ **Expected 0, and a hit is not cosmetic**: the runtime's answer is to
    /// install *its own* stubs over the list, so every recording DDI silently
    /// bypasses this driver and the list records nothing. The handle cannot be
    /// recovered any other way (`DDI_REFERENCE.md` §2.2).
    command_list_rt_table_missing: RefusalCounter,
    /// The corelayer `pfnSetCommandListDDITableCb` was missing. **Expected 0** —
    /// it is the third member of `D3D12DDI_CORELAYER_DEVICECALLBACKS_0062` and
    /// the runtime requires the call (strings:30). Same consequence as
    /// `CommandListRtTableMissing`.
    command_list_ddi_table_cb_missing: RefusalCounter,
    /// A command-signature slot was called with a null arg or a null
    /// `pDrvPrivate`. **Expected 0.**
    command_signature_bad_arg: RefusalCounter,
    /// `pfnCreateCommandSignature` was refused with `E_NOTIMPL`.
    ///
    /// ⚠ **Expected non-zero on any workload that uses `ExecuteIndirect`**, and
    /// that is the honest state: the translation needs L3a's
    /// `D3D12DDI_INDIRECT_ARGUMENT_DESC` work and L6's root-signature payload.
    /// It is not on `DDI_REFERENCE.md` §14.2's 99-slot minimum-viable list.
    command_signature_refused: RefusalCounter,
    /// `pfnDestroyCommandSignature` ran for a signature this driver never
    /// created. ⚠ Expected to track `CommandSignatureRefused`; it is the
    /// runtime cleaning up after a refused create, not a fault.
    command_signature_destroy_unexpected: RefusalCounter,
    /// `pfnCreateCommandList` was asked for a BUNDLE and refused.
    ///
    /// ⚠ **Expected 0 on a workload that records no bundles, and NON-ZERO — with
    /// lost application work — on one that does.** This is a real capability gap,
    /// not an instrument: `D3D12DDIARG_CREATE_COMMAND_RECORDER_0040` carries no
    /// bundle bit, so no BUNDLE `ID3D12CommandAllocator` can be minted and the
    /// paired `pfnResetCommandList` could never succeed. Refusing at create is
    /// the survivable end of that: the application gets a failed
    /// `ID3D12Device::CreateCommandList` instead of a removed device.
    ///
    /// ⛔ It is the counter that says whether the one-allocator-per-(pool, class)
    /// fix is worth doing yet. A non-zero reading on a real workload is what
    /// promotes it from a named gap to scheduled work.
    bundle_list_refused: RefusalCounter,
    /// `pfnExecuteCommandLists` was called with an unresolvable queue, a null
    /// array, or a count above `MAX_EXECUTE_COMMAND_LISTS`. **Expected 0.**
    execute_command_lists_bad_arg: RefusalCounter,
    /// An entry of an `pfnExecuteCommandLists` array carried no engine command
    /// list, and the whole submit was refused rather than partially forwarded.
    /// **Expected 0** — a list the runtime submits is a list this driver created.
    execute_command_lists_list_missing: RefusalCounter,
    /// A submission was forwarded to the engine with **no WDDM submission behind
    /// it**: no `pfnSubmitCommandCb`, no `pfnRenderCb`, no DMA fence.
    ///
    /// ⛔ **Expected non-zero on every frame, and it is this lane's largest
    /// deliberate gap.** `ResourceHeaps.md:1678` requires a kernel submission
    /// during this DDI, on the entering thread, against a queue-creation context
    /// — the context exists, the monotonic completion watermark
    /// (`DDI_REFERENCE.md` §8.3 step 2) does not, and §8.3 records that as the
    /// one piece with no existing answer. The lane that closes it must not signal
    /// a wire fence before host completion.
    ///
    /// ⚠ **Exactly one piece is open, and it is the watermark.** The *callback*
    /// is decided: `create_command_queue` mints a legacy context, so this
    /// submission is `pfnRenderCb` and `pKTCallbacks->pfnSubmitCommandCb` — which
    /// §6.4 scopes to GPU-VA contexts — is not reachable from here without
    /// re-opening queue creation.
    ecl_no_wddm_submission: RefusalCounter,
    /// The queue table's `pfnUnused` was actually called. ⛔ **Expected 0** — the
    /// header names it unused (`DDI_REFERENCE.md` §14.1.1 classifies it
    /// RESERVED). A hit is the header being wrong, which is exactly what the stub
    /// exists to turn into a number.
    queue_unused_slot_called: RefusalCounter,
    /// The queue table's `pfnUnused2` was actually called. **Expected 0**, same
    /// reasoning.
    queue_unused2_slot_called: RefusalCounter,
    /// `pfnUpdateTileMappings` or `pfnCopyTileMappings` was called and refused.
    ///
    /// ⛔ **Expected 0** while `caps12` reports `TiledResourcesTier =
    /// NOT_SUPPORTED`: a hit means the runtime reached a tiled-resource path on a
    /// driver that advertises no tier, which is a caps inconsistency elsewhere.
    /// ⚠ It becomes the *implementation* marker for `DX12.md` §4.4's feature
    /// level 12_1 floor, which needs `TiledResourcesTier >= 2`.
    tile_mappings_refused: RefusalCounter,
    /// `pfnSignalFence` / `pfnWaitForFence` with a null
    /// `D3D12DDIARG_FENCE_OPERATION` or an unresolvable queue. **Expected 0.**
    fence_op_bad_arg: RefusalCounter,
    /// A fence operation named a `D3D12DDI_HFENCE` with no `fence::FenceState`
    /// behind it. **Expected 0** — L7's `pfnCreateFence` either stores one or
    /// fails.
    fence_op_fence_missing: RefusalCounter,
    /// `ID3D12CommandQueue::Signal` or `::Wait` on the engine failed, and the
    /// failure was raised to the runtime through `pfnSetErrorCb`. **Expected 0.**
    fence_op_engine_failed: RefusalCounter,
    /// `pfnWaitForFence` asked for a `Value` above the watermark this driver has
    /// itself signalled on that fence, so the wait was **not forwarded** to the
    /// engine.
    ///
    /// ⚠ **May legitimately be non-zero, and it is the instrument that tells the
    /// two cases apart.** Three things land here: a `CreateFence(InitialValue)`
    /// the DDI never delivers, a CPU `ID3D12Fence::Signal` that
    /// `DDI_REFERENCE.md` §10.3 says never reaches the driver, and a legal
    /// wait-before-signal. The first two are waits the runtime already considers
    /// satisfied and forwarding them would hang the engine queue forever; the
    /// third is real ordering this driver is dropping until §10.4's
    /// `pfnWaitForSynchronizationObjectFromGpuCb` half exists. ⛔ A large count
    /// next to visible cross-queue corruption is the third case and is a finding;
    /// see `fence.rs`'s module doc.
    fence_wait_not_forwarded: RefusalCounter,
    /// `pfnSignalFence` reached its engine forward and
    /// `ID3D12CommandQueue::Signal` returned success.
    ///
    /// ⚠ **Expected NON-ZERO once any D3D12 workload signals a fence. A zero
    /// here means the runtime never enters this slot at all** — a fact the
    /// `pfnRenderCb` WDDM submission design has to accommodate, because nothing
    /// this driver does *inside* `pfnSignalFence` can then be part of the
    /// ordering (`tmp/dx12/FENCE-BRIDGE-DESIGN.md` §5 step 2). It is also
    /// `DDI_REFERENCE.md` §14.0's WARP reading, which measured WARP never
    /// entering this slot across 20 frames of `ID3D12CommandQueue::Signal` +
    /// `SetEventOnCompletion`.
    ///
    /// ⭐ **Why a success counter exists at all**, against this file's own
    /// convention that counters name refusals: until it did, `pfnSignalFence`
    /// succeeding was **invisible**. `fence_operation` incremented nothing and
    /// logged nothing on its success path, so `tmp/dx12/gates/G8-r0/RESULT.md`'s
    /// claim that *"the queue-table `Signal` path ran"* rested on three **zero**
    /// readings — `FenceOpEngineFailed`, `FenceOpBadArg`, `FenceWaitNotForwarded`
    /// — and this project has twice written, in this very file
    /// (`queue.rs`'s `fence_operation` doc) and in `fence.rs:61-64`, that a zero
    /// reading is not evidence a path works.
    fence_signal_forwarded: RefusalCounter,
    /// `pfnWaitForFence` reached its engine forward and
    /// `ID3D12CommandQueue::Wait` returned success. Same grading and the same
    /// reason for existing as [`Self::fence_signal_forwarded`].
    ///
    /// ⚠ Expected non-zero only on a workload that waits on a value this driver
    /// has itself signalled: everything above that watermark is refused by
    /// `FenceWaitNotForwarded` before it can reach the forward. ⇒ read the two
    /// together — `FenceWaitForwarded = 0` with `FenceWaitNotForwarded > 0` is
    /// the shadow-fence policy working, not a dead slot.
    fence_wait_forwarded: RefusalCounter,
    /// `pfnExecuteCommandLists` forwarded a submit to the engine's
    /// `ID3D12CommandQueue::ExecuteCommandLists`.
    ///
    /// ⚠ **Expected non-zero on any workload that submits.** It is the
    /// denominator for `EclNoWddmSubmission` — which today tracks it exactly,
    /// one for one, because every forward is missing the WDDM half — and it is
    /// what makes that ratio a fact rather than an assumption once the WDDM half
    /// lands and the two counts diverge.
    ///
    /// ⭐ Same reason for existing as [`Self::fence_signal_forwarded`]: a submit
    /// that reached the engine left no trace of its own, so "the engine was
    /// given the work" was inferred from the pixels rather than counted.
    ecl_forwarded: RefusalCounter,
    /// `Umd12FenceSignalDelayUs` was non-zero and `pfnSignalFence` slept before
    /// returning. **Expected 0** on any run that did not deliberately set the
    /// knob; see `knobs12::UMD12_FENCE_SIGNAL_DELAY_US` for the question it is
    /// attached to and the commit that must delete it.
    fence_signal_delayed: RefusalCounter,
    /// `Umd12EclDelayUs` was non-zero and `pfnExecuteCommandLists` slept before
    /// returning. **Expected 0** on any run that did not deliberately set the
    /// knob; see `knobs12::UMD12_ECL_DELAY_US`.
    ecl_delayed: RefusalCounter,
}

pub(crate) static L2_REFUSALS: L2Refusals = L2Refusals {
    queue_bad_arg: RefusalCounter::new("QueueBadArg"),
    queue_no_device: RefusalCounter::new("QueueNoDevice"),
    queue_class_unsupported: RefusalCounter::new("QueueClassUnsupported"),
    queue_engine_failed: RefusalCounter::new("QueueEngineFailed"),
    queue_creation_flags_ignored: RefusalCounter::new("QueueCreationFlagsIgnored"),
    queue_scheduling_group_ignored: RefusalCounter::new("QueueSchedulingGroupIgnored"),
    queue_node_mask_ignored: RefusalCounter::new("QueueNodeMaskIgnored"),
    queue_context_failed: RefusalCounter::new("QueueContextFailed"),
    queue_context_destroy_failed: RefusalCounter::new("QueueContextDestroyFailed"),
    queue_set_error_unavailable: RefusalCounter::new("QueueSetErrorUnavailable"),
    pool_bad_arg: RefusalCounter::new("PoolBadArg"),
    pool_no_device: RefusalCounter::new("PoolNoDevice"),
    pool_allocator_engine_failed: RefusalCounter::new("PoolAllocatorEngineFailed"),
    pool_type_mismatch: RefusalCounter::new("PoolTypeMismatch"),
    pool_reset_no_allocator: RefusalCounter::new("PoolResetNoAllocator"),
    pool_reset_engine_failed: RefusalCounter::new("PoolResetEngineFailed"),
    recorder_bad_arg: RefusalCounter::new("RecorderBadArg"),
    recorder_class_unsupported: RefusalCounter::new("RecorderClassUnsupported"),
    command_list_bad_arg: RefusalCounter::new("CommandListBadArg"),
    command_list_no_device: RefusalCounter::new("CommandListNoDevice"),
    command_list_class_unsupported: RefusalCounter::new("CommandListClassUnsupported"),
    command_list_engine_failed: RefusalCounter::new("CommandListEngineFailed"),
    command_list_flags_ignored: RefusalCounter::new("CommandListFlagsIgnored"),
    command_list_rt_table_missing: RefusalCounter::new("CommandListRtTableMissing"),
    command_list_ddi_table_cb_missing: RefusalCounter::new("CommandListDdiTableCbMissing"),
    command_signature_bad_arg: RefusalCounter::new("CommandSignatureBadArg"),
    command_signature_refused: RefusalCounter::new("CommandSignatureRefused"),
    command_signature_destroy_unexpected: RefusalCounter::new("CommandSignatureDestroyUnexpected"),
    bundle_list_refused: RefusalCounter::new("L2BundleListRefused"),
    execute_command_lists_bad_arg: RefusalCounter::new("ExecuteCommandListsBadArg"),
    execute_command_lists_list_missing: RefusalCounter::new("ExecuteCommandListsListMissing"),
    ecl_no_wddm_submission: RefusalCounter::new("EclNoWddmSubmission"),
    queue_unused_slot_called: RefusalCounter::new("QueueUnusedSlotCalled"),
    queue_unused2_slot_called: RefusalCounter::new("QueueUnused2SlotCalled"),
    tile_mappings_refused: RefusalCounter::new("TileMappingsRefused"),
    fence_op_bad_arg: RefusalCounter::new("FenceOpBadArg"),
    fence_op_fence_missing: RefusalCounter::new("FenceOpFenceMissing"),
    fence_op_engine_failed: RefusalCounter::new("FenceOpEngineFailed"),
    fence_wait_not_forwarded: RefusalCounter::new("FenceWaitNotForwarded"),
    fence_signal_forwarded: RefusalCounter::new("FenceSignalForwarded"),
    fence_wait_forwarded: RefusalCounter::new("FenceWaitForwarded"),
    ecl_forwarded: RefusalCounter::new("EclForwarded"),
    fence_signal_delayed: RefusalCounter::new("FenceSignalDelayed"),
    ecl_delayed: RefusalCounter::new("EclDelayed"),
};

/// L2's refusal counters, printed by `crate::log_refusal_summary` at this
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
pub(crate) static REFUSALS: &[&RefusalCounter] = &[
    &L2_REFUSALS.queue_bad_arg,
    &L2_REFUSALS.queue_no_device,
    &L2_REFUSALS.queue_class_unsupported,
    &L2_REFUSALS.queue_engine_failed,
    &L2_REFUSALS.queue_creation_flags_ignored,
    &L2_REFUSALS.queue_scheduling_group_ignored,
    &L2_REFUSALS.queue_node_mask_ignored,
    &L2_REFUSALS.queue_context_failed,
    &L2_REFUSALS.queue_context_destroy_failed,
    &L2_REFUSALS.queue_set_error_unavailable,
    &L2_REFUSALS.pool_bad_arg,
    &L2_REFUSALS.pool_no_device,
    &L2_REFUSALS.pool_allocator_engine_failed,
    &L2_REFUSALS.pool_type_mismatch,
    &L2_REFUSALS.pool_reset_no_allocator,
    &L2_REFUSALS.pool_reset_engine_failed,
    &L2_REFUSALS.recorder_bad_arg,
    &L2_REFUSALS.recorder_class_unsupported,
    &L2_REFUSALS.command_list_bad_arg,
    &L2_REFUSALS.command_list_no_device,
    &L2_REFUSALS.command_list_class_unsupported,
    &L2_REFUSALS.command_list_engine_failed,
    &L2_REFUSALS.command_list_flags_ignored,
    &L2_REFUSALS.command_list_rt_table_missing,
    &L2_REFUSALS.command_list_ddi_table_cb_missing,
    &L2_REFUSALS.command_signature_bad_arg,
    &L2_REFUSALS.command_signature_refused,
    &L2_REFUSALS.command_signature_destroy_unexpected,
    &L2_REFUSALS.execute_command_lists_bad_arg,
    &L2_REFUSALS.execute_command_lists_list_missing,
    &L2_REFUSALS.ecl_no_wddm_submission,
    &L2_REFUSALS.queue_unused_slot_called,
    &L2_REFUSALS.queue_unused2_slot_called,
    &L2_REFUSALS.tile_mappings_refused,
    &L2_REFUSALS.fence_op_bad_arg,
    &L2_REFUSALS.fence_op_fence_missing,
    &L2_REFUSALS.fence_op_engine_failed,
    &L2_REFUSALS.fence_wait_not_forwarded,
    // ⛔ APPENDED, S6 Round 2. It was first written into the middle of this array,
    // between `CommandSignatureDestroyUnexpected` and `ExecuteCommandListsBadArg`,
    // where it read tidily beside the other create-time refusals -- and shifted
    // the nine counters after it in every `D3D12 DDI refusals:` line this driver
    // will ever print. That is precisely what the append-only rule above exists
    // to stop, and the rule was violated in the same commit that quotes it.
    // ⇒ new counters go HERE, at the end, however badly they group.
    &L2_REFUSALS.bundle_list_refused,
    // ⛔ APPENDED, the F1 fence-bridge instrument round. Three SUCCESS counters
    // and two knob-firing counters, at the end for the same reason
    // `L2BundleListRefused` is: the `D3D12 DDI refusals:` line is diffed across
    // builds and inserting shifts every counter after the insertion point.
    &L2_REFUSALS.fence_signal_forwarded,
    &L2_REFUSALS.fence_wait_forwarded,
    &L2_REFUSALS.ecl_forwarded,
    &L2_REFUSALS.fence_signal_delayed,
    &L2_REFUSALS.ecl_delayed,
];

// ⚠ `Hresult` is imported for the `E_*`/`S_OK` constants this file returns; the
// DDI's own `HRESULT` (bindgen's `c_long`) is the declared return type and the
// two are the same `i32` (`umd_common/src/hr.rs:31-34`).
const _: () = assert!(core::mem::size_of::<Hresult>() == core::mem::size_of::<ddi12::HRESULT>());
