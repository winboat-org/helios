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
//! | pool (`D3D12DDIARG_CREATE_COMMAND_POOL_0040`) | `PoolFlags` — **one enum with one value, `NONE`** | one lazily-created `ID3D12CommandAllocator` per list class |
//! | recorder (`D3D12DDIARG_CREATE_COMMAND_RECORDER_0040`) | `QueueFlags`, `RecorderFlags` | **no engine object at all** |
//! | list (`D3D12DDIARG_CREATE_COMMAND_LIST_0040`) | `Type` (DIRECT/BUNDLE), `QueueFlags`, `ID`, `CommandListFlags`, `NodeMask` | an `ID3D12GraphicsCommandList` |
//!
//! ⭐ **The pool cannot create its allocator at `pfnCreateCommandPool`**, and
//! that is the one non-obvious consequence of the shape above.
//! `ID3D12Device::CreateCommandAllocator` takes a `D3D12_COMMAND_LIST_TYPE`; the
//! pool's create args are a single flags word that carries no type, no size and
//! no pointer. The only DDI that ever brings a queue class into contact with a
//! pool with the authoritative list class is `pfnResetCommandList`: the recorder
//! names its current pool and the list being reset names DIRECT, BUNDLE, COMPUTE
//! or COPY. ⇒ **this lane creates that pool's allocator for the exact list class
//! lazily at first reset.** A pool can consequently serve different classes
//! without ever passing a mismatched allocator to the engine.
//!
//! ⚠ `DDI_REFERENCE.md` §9.3's mapping table writes this row as
//! *"`pfnCreateCommandPool` → `ID3D12Device::CreateCommandAllocator(type)`"* and
//! does not say where `type` comes from. It comes from the list being reset,
//! after its recorder names the pool; the per-class lazy creation is that gap
//! closed, not a deviation from the table.
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
//! ⭐ **Bundles use the same rule.** The recorder carries no bundle bit, but the
//! command list's `Type` does. At reset, that authoritative type selects the
//! pool's BUNDLE allocator slot. Time Spy also proved why the recorder cannot be
//! the class source: it binds a DIRECT-compatible recorder and later resets a
//! COMPUTE list through it.
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
//! # Runtime admission and exact completion
//!
//! Each ECL commits retained engine work and a generation-qualified stream value
//! to the queue worker. On the entering DDI thread, the UMD submits HE12 v2 on
//! the exact runtime context, then queues an unnamed CPU event with
//! SignalAtSubmission | EnqueueCpuEvent. The worker waits for that admission
//! before executing, and signals the registered stream after all preceding
//! queue work. The KMD withholds DMA completion until that exact value retires.
//!
//! Present producer callbacks use the same admission pair, including when a
//! runtime Queue::Wait precedes Present without another ECL. A queue operation
//! mutex keeps each Render/event pair contiguous on its context. The separate
//! kernel execution tail survives Present batching and preemption replay.
//!
//! Software fence values, CPU signals and shared opens remain runtime-owned.
//! Native GPU-VA fence placements and queue fence DDIs are explicitly refused.
//! See docs/dx12/EXECUTION_SYNC.md for the contract and runtime acceptance gaps.

use core::ffi::c_void;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use helios_umd_common::hr::{Hresult, E_FAIL, E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, S_OK};
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
    ID3D12CommandAllocator, ID3D12CommandList, ID3D12CommandQueue, ID3D12CommandSignature,
    ID3D12Device, ID3D12Device4, ID3D12GraphicsCommandList, D3D12_COMMAND_LIST_FLAG_NONE,
    D3D12_COMMAND_LIST_TYPE, D3D12_COMMAND_LIST_TYPE_BUNDLE, D3D12_COMMAND_LIST_TYPE_COMPUTE,
    D3D12_COMMAND_LIST_TYPE_COPY, D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_COMMAND_QUEUE_DESC,
    D3D12_COMMAND_QUEUE_FLAG_NONE, D3D12_COMMAND_SIGNATURE_DESC, D3D12_INDIRECT_ARGUMENT_DESC,
    D3D12_INDIRECT_ARGUMENT_DESC_0_0, D3D12_INDIRECT_ARGUMENT_DESC_0_1,
    D3D12_INDIRECT_ARGUMENT_DESC_0_3, D3D12_INDIRECT_ARGUMENT_DESC_0_4,
    D3D12_INDIRECT_ARGUMENT_DESC_0_5, D3D12_INDIRECT_ARGUMENT_TYPE,
    D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT, D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT_BUFFER_VIEW,
    D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH, D3D12_INDIRECT_ARGUMENT_TYPE_DRAW,
    D3D12_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED, D3D12_INDIRECT_ARGUMENT_TYPE_INDEX_BUFFER_VIEW,
    D3D12_INDIRECT_ARGUMENT_TYPE_SHADER_RESOURCE_VIEW,
    D3D12_INDIRECT_ARGUMENT_TYPE_UNORDERED_ACCESS_VIEW,
    D3D12_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW,
};

use super::fence;
use super::pso;
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

// ⭐ **S-4: `D3D12DDI_HCOMMANDSIGNATURE` now HAS a payload, and it is one bare
// owning COM word.** Until `pfnCreateCommandSignature` was implemented this file
// deliberately declared none — the handle carried nothing, and a marker impl
// saying otherwise would have been a claim about an object that was never built.
// It is built now, so the declaration lands with it, in the lane that owns the
// handle (`PARALLEL.md` §4) and reaches `Slot::from_priv` through
// `DdiHandle::drv_private` rather than by reading `pDrvPrivate` at each site.
//
// ⚠ `com_handles!`, not `boxed_handles!`: an `ID3D12CommandSignature` needs no
// shadow state. Everything the DDI's create args carry is either forwarded into
// the engine object or refused at create, so there is nothing left for the driver
// to remember — which is the opposite of `D3D12DDI_HFENCE`, whose watermark is the
// whole reason that one is boxed.
helios_umd_common::com_handles!(crate::ddi12::D3D12DDI_HCOMMANDSIGNATURE,);

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
/// AGENTS.md: *validate every runtime-supplied size before reading.* The DDI
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
    /// The allocation-list window dxgkrnl supplied with the legacy context.
    /// D3D12 submissions in this driver deliberately use `NumAllocations = 0`:
    /// residency is owned by the D3D12 runtime and the actual present source is
    /// returned through `D3D12DDI_PRESENT_0051`. The window is still latched and
    /// re-latched as part of the indivisible runtime window set.
    allocations: Option<Window<ddi12::D3DDDI_ALLOCATIONLIST>>,
    /// The patch-location list. Helios' GpuMmu is decorative — the host owns the
    /// real MMU and there are no guest GPU-VAs to patch, which is why
    /// `kmd_render`'s `dxgkddi_render` passes the list straight through and
    /// `DxgkDdiPatch` is a no-op.
    patches: Option<Window<ddi12::D3DDDI_PATCHLOCATIONLIST>>,
}

impl ContextWindows {
    /// Latch the windows `pfnCreateContextCb` just returned.
    ///
    /// ⚠ Unconditional, unlike [`Self::re_latch`]: at create there is nothing to
    /// keep, so a null pointer here means "this context has no such window" rather
    /// than "keep what you have".
    fn from_create_context(arg: &ddi12::D3DDDICB_CREATECONTEXT) -> Self {
        Self {
            command: Window::new(arg.pCommandBuffer, arg.CommandBufferSize),
            allocations: Window::new(arg.pAllocationList, arg.AllocationListSize),
            patches: Window::new(arg.pPatchLocationList, arg.PatchLocationListSize),
        }
    }

    /// Re-latch all three windows from a **successful** `pfnRenderCb`.
    ///
    /// ⭐ **The one shared re-latch, and that is the point of it** — both
    /// `pfnRenderCb` users take this method rather than open-coding six field
    /// updates each (`KMD_IMPACT.md` §14a.2 FB-1, §14a.4 point 2). The two rules
    /// below are copied from the shipping D3D11 site
    /// (`umd/src/forward/present.rs:868-897`) and each one is a corruption this
    /// project has already reasoned about once:
    ///
    /// * **Each window is replaced as a unit**, so a new pointer can never be
    ///   stored against the old capacity. That is why `Window` is one value and
    ///   not two fields (`umd_common/src/window.rs:14-17`).
    /// * **A returned pointer with a zero size means "keep what you have", not
    ///   "here is an empty buffer".** The D3D11 comment says exactly that, and the
    ///   `!= 0` guards are what implement it: dxgkrnl fills the `pNew*` group only
    ///   when it actually rotated a buffer, so treating an unrotated submission as
    ///   "the runtime took my window away" would leave the next submit with
    ///   nothing to record into. ⚠ The shape reads like a missing `else`. It is
    ///   not one.
    ///
    /// ⛔ Called **only** on `hr >= 0`, and **only** with
    /// [`QueueState::windows`]' guard held from before the payload write — see
    /// that field for why the critical section cannot be narrower. On a failure
    /// the out-fields promise nothing, and re-latching from them would install a
    /// window dxgkrnl never lent.
    fn re_latch(&mut self, render: &ddi12::D3DDDICB_RENDER) {
        if render.NewCommandBufferSize != 0 {
            if let Some(w) = Window::new(render.pNewCommandBuffer, render.NewCommandBufferSize) {
                self.command = Some(w);
            }
        }
        if render.NewAllocationListSize != 0 {
            if let Some(w) = Window::new(render.pNewAllocationList, render.NewAllocationListSize) {
                self.allocations = Some(w);
            }
        }
        if render.NewPatchLocationListSize != 0 {
            if let Some(w) = Window::new(
                render.pNewPatchLocationList,
                render.NewPatchLocationListSize,
            ) {
                self.patches = Some(w);
            }
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
    /// Orders admission, worker commit and Render across concurrent ECL calls.
    execution: Mutex<()>,
}

/// Engine allocators materialised for one DDI command pool, indexed by command
/// list class.
///
/// `D3D12DDIARG_CREATE_COMMAND_POOL_0040` carries no class, and the recorder's
/// `QueueFlags` describes queue compatibility rather than the class of the list
/// that will later name that recorder at `pfnResetCommandList`. Time Spy proves
/// those can differ: a COMPUTE list arrived through a recorder whose queue class
/// was DIRECT. The list itself is therefore the first authoritative class, so
/// each class is initialised independently on first reset.
#[derive(Default)]
struct AllocatorGenerations {
    current: Option<ID3D12CommandAllocator>,
    retired: Vec<ID3D12CommandAllocator>,
}

impl AllocatorGenerations {
    /// Native completion and engine retirement are independent. Reuse only an
    /// allocator with no execution references. Otherwise retain the old storage
    /// and change generations. No GPU wait, timeline query or early release.
    fn reset(&mut self, engine: &ID3D12Device, list_type: D3D12_COMMAND_LIST_TYPE) -> Result<(), Hresult> {
        let Some(current) = self.current.as_ref() else { return Ok(()); };
        // SAFETY: the class mutex serializes reset/selection; current is owned.
        let hr = unsafe { crate::bridge12::try_reset_allocator(current.as_raw() as usize) };
        if hr < 0 { return Err(hr); }
        if hr == S_OK {
            note_refusal(&L2_REFUSALS.pool_generation_reset);
            return Ok(());
        }
        // Reserve ownership storage before creating or exchanging anything.
        self.retired.try_reserve(1).map_err(|_| E_OUTOFMEMORY)?;
        let mut reusable = None;
        for (index, allocator) in self.retired.iter().enumerate() {
            // SAFETY: retired generation is owned, no longer selected for new
            // recording, and its storage is reset only after execution refs retire.
            let hr = unsafe { crate::bridge12::try_reset_allocator(allocator.as_raw() as usize) };
            if hr < 0 { return Err(hr); }
            if hr == S_OK { reusable = Some(index); break; }
        }
        let replacement = if let Some(index) = reusable {
            note_refusal(&L2_REFUSALS.pool_generation_reused);
            self.retired.swap_remove(index)
        } else {
            // SAFETY: live owning device; list class was validated by slot().
            let allocator = unsafe { engine.CreateCommandAllocator::<ID3D12CommandAllocator>(list_type) }
                .map_err(|e| e.code().0)?;
            note_refusal(&L2_REFUSALS.pool_generation_created);
            allocator
        };
        if let Some(previous) = self.current.replace(replacement) {
            self.retired.push(previous);
        }
        note_refusal(&L2_REFUSALS.pool_generation_rotated);
        Ok(())
    }
}

struct PoolAllocators {
    direct: Mutex<AllocatorGenerations>,
    bundle: Mutex<AllocatorGenerations>,
    compute: Mutex<AllocatorGenerations>,
    copy: Mutex<AllocatorGenerations>,
}

impl PoolAllocators {
    fn new() -> Self {
        Self {
            direct: Mutex::new(AllocatorGenerations::default()),
            bundle: Mutex::new(AllocatorGenerations::default()),
            compute: Mutex::new(AllocatorGenerations::default()),
            copy: Mutex::new(AllocatorGenerations::default()),
        }
    }

    fn slot(&self, list_type: D3D12_COMMAND_LIST_TYPE) -> Option<&Mutex<AllocatorGenerations>> {
        match list_type {
            D3D12_COMMAND_LIST_TYPE_DIRECT => Some(&self.direct),
            D3D12_COMMAND_LIST_TYPE_BUNDLE => Some(&self.bundle),
            D3D12_COMMAND_LIST_TYPE_COMPUTE => Some(&self.compute),
            D3D12_COMMAND_LIST_TYPE_COPY => Some(&self.copy),
            _ => None,
        }
    }

    fn slots(&self) -> [(D3D12_COMMAND_LIST_TYPE, &Mutex<AllocatorGenerations>); 4] {
        [
            (D3D12_COMMAND_LIST_TYPE_DIRECT, &self.direct),
            (D3D12_COMMAND_LIST_TYPE_BUNDLE, &self.bundle),
            (D3D12_COMMAND_LIST_TYPE_COMPUTE, &self.compute),
            (D3D12_COMMAND_LIST_TYPE_COPY, &self.copy),
        ]
    }
}

/// Per-command-pool shadow state.
///
/// The `Arc` lets a recorder retain the pool's allocator set without ever
/// dereferencing runtime-owned pool private memory after `pfnDestroyCommandPool`.
/// Per-class mutexes serialize lazy creation and generation exchange. Recorders
/// retain this set and obtain an owned reference to its current generation.
pub struct PoolState {
    allocators: Arc<PoolAllocators>,
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
    /// ⭐ Kept so `pfnResetCommandList` can select the bound pool's allocator for
    /// the exact list class. The recorder's queue compatibility flags are not
    /// authoritative: Time Spy sends a COMPUTE list through a DIRECT-compatible
    /// recorder, and only this field preserves the class the engine requires.
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
/// reference to that pool's per-class allocator set.
///
/// ⛔ **The owned reference is the whole point, and it replaces a raw
/// `AtomicPtr<c_void>` that could not be made safe.** Until S6 Round 2 this lane
/// stored only the pool's `pDrvPrivate` and never dereferenced it. Re-deriving
/// `PoolState` from that identity at reset would read memory **the runtime owns
/// and frees at `pfnDestroyCommandPool`**. The `Arc` makes that freed-pool read
/// unrepresentable while still deferring allocator-class selection until the
/// command list supplies the authoritative class.
///
/// ⚠ `pool` is a `usize`, not a pointer: it is **identity only**, for the trace
/// lines and the rebind check, and is never dereferenced. Storing it as an
/// integer says so in the type.
struct RecorderTarget {
    pool: usize,
    allocators: Arc<PoolAllocators>,
}

/// Per-command-recorder shadow state. There is no engine object — see the module
/// doc.
pub struct RecorderState {
    /// The queue compatibility class from
    /// `D3D12DDIARG_CREATE_COMMAND_RECORDER_0040::QueueFlags`.
    ///
    /// ⛔ This is diagnostic state, not an allocator class. Time Spy supplies a
    /// DIRECT-compatible recorder for a COMPUTE list, proving that choosing an
    /// allocator from this value is wrong. `pfnResetCommandList` supplies the
    /// list class that selects the allocator.
    queue_type: D3D12_COMMAND_LIST_TYPE,
    /// The pool this recorder last targeted, and that pool's allocator.
    ///
    /// A `Mutex` protects the recorder target because
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
//     are the mutex-protected generations inside `PoolState::allocators`
//     and `RecorderState::target`, a
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
//     ever held across a call back into the runtime. Class locks serialize
//     engine allocator reset and creation; the engine does not acquire these
//     frontend locks or invoke runtime callbacks from those operations.
//
//     ⚠ The premise the compiler never checks, stated because it is a premise:
//     `RecorderTarget` holds an `Arc` whose allocator slots contain windows-rs
//     COM interfaces, which carry no
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

/// The WDDM context one queue submits on — **L8's one seam into this file.**
///
/// ⭐ It returns the handle and not the [`QueueState`], and that is the whole
/// point of it. `pfnPresent` needs exactly one field of this file's state —
/// `D3D12DDI_PRESENT_CONTEXTS_0051::hContext` must be the context
/// `pfnCreateCommandQueue` minted (`KMD_IMPACT.md` §14a.3 UP-7) — and handing
/// out a `&QueueState` instead would export three invariants that belong here:
/// that [`QueueState::windows`]' guard spans write → `pfnRenderCb` → re-latch,
/// that the windows rotate under that guard, and that a submission must be made
/// on the thread inside the owning DDI. [`submit_present_identity`] is the other
/// seam, for the same reason.
///
/// ⚠ **Two handle resolutions per present, deliberately.** L8 calls this and then
/// [`submit_present_identity`], each of which resolves the handle again — two
/// pointer loads. The alternative is one call returning a borrow of the state,
/// which is the thing the paragraph above rules out. And the order matters more
/// than the loads do: L8 must know the context exists *before* it decides to
/// submit, because a refused present has to write nothing at all.
///
/// `None` means the handle did not resolve to a live queue, or its context is
/// null — which `create_wddm_context` makes unreachable by failing the queue
/// create, so a `None` is a finding rather than a state to work around.
///
/// # Safety
/// As [`queue_state`]. The returned handle is dxgkrnl's and is only valid while
/// the queue lives, i.e. for the DDI call that obtained it.
pub(crate) unsafe fn present_context(h: ddi12::D3D12DDI_HCOMMANDQUEUE) -> Option<*mut c_void> {
    // SAFETY: forwarded unchanged to `queue_state`'s identical precondition.
    let queue = unsafe { queue_state(h) }?;
    (!queue.h_context.is_null()).then_some(queue.h_context)
}

/// Enqueue the exact resource boundary on this queue's engine worker.
/// # Safety
/// As queue_state; resource is a live engine object and allocation is its
/// identity12 allocation. No queue state borrow escapes this function.
pub(crate) unsafe fn publish_present_producer(
    h: ddi12::D3D12DDI_HCOMMANDQUEUE,
    resource: usize,
    allocation: u32,
) -> Option<(u32, u32, u64)> {
    // SAFETY: the caller supplies a live runtime queue private block.
    let queue = unsafe { queue_state(h) }?;
    // Admission also matters for Queue::Wait followed directly by Present,
    // without an intervening ECL. Give this callback its own exact packet.
    // SAFETY: the live queue retains its creating device.
    let dev = unsafe { device12::device(queue.h_device) }?;
    let _execution = queue
        .execution
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // SAFETY: unnamed manual-reset event, initially unsignaled.
    let admission =
        match unsafe { windows::Win32::System::Threading::CreateEventW(None, true, false, None) } {
            Ok(event) => AdmissionEvent(event),
            Err(error) => {
                report_ecl_submit_error(queue, error.code().0);
                return None;
            }
        };
    // SAFETY: exact live queue/resource/allocation; the worker duplicates the event.
    let boundary = unsafe {
        crate::bridge12::publish_producer(
            queue.engine_queue.as_raw() as usize,
            resource,
            allocation,
            admission.0 .0 as usize,
        )
    }?;
    // ⛔ WIRE FENCE WITHDRAWN (2026-09-13): the D3D12 record carries
    // `gpu_wire_fence = 0`, i.e. the pre-lever retire domain. The sampled Venus
    // wire fence was measured not to fix the early fence (allocator failed 2 of 2
    // with it and 2 of 3 without) and `EXECUTION_SYNC.md` rejects it by design: a
    // sampled wire fence can precede worker execution. The gate this packet needs
    // is the registered producer stream (`ctx`/`value`/`cookie` above), which the
    // engine signals on ALL_COMMANDS after execution. See KMD_IMPACT §14a.2.
    let gpu_wire_fence = 0u64;
    // SAFETY: Present's entering DDI thread, exact queue context and complete record.
    let outcome = unsafe {
        submit_wddm_render(
            dev,
            queue,
            &ecl_submit_command(boundary, gpu_wire_fence),
            "Present producer",
        )
    };
    let result = match outcome {
        // SAFETY: signal directly follows this packet under the context operation lock.
        WddmSubmit::Submitted => unsafe { enqueue_runtime_admission(dev, queue, &admission) },
        WddmSubmit::Refused(hr) => Err(hr),
        WddmSubmit::Unavailable => Err(E_NOTIMPL),
    };
    if let Err(hr) = result {
        report_ecl_submit_error(queue, hr);
        return None;
    }
    note_refusal(&L2_REFUSALS.present_producer_admitted);
    Some(boundary)
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
unsafe extern "system" fn calc_private_command_queue_size(
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
unsafe extern "system" fn create_command_queue(
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
            // never present or submit is not a queue, and AGENTS.md rule 2 is
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
            execution: Mutex::new(()),
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
unsafe extern "system" fn destroy_command_queue(
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

    // DestroyContextCb drains queued context work. Keep worker admission live
    // during that call: an unsignaled event can still belong to valid work that
    // the runtime has not submitted yet. The DDI queue slot is already empty,
    // so no new operation can acquire this queue through it.
    // SAFETY: `state` is the live box this call took out of the slot, so its
    // `h_rt_queue` and `h_context` are the pair `create_wddm_context` produced.
    let hr = unsafe { destroy_wddm_context(&state) };
    // A successful context destroy has drained all admitted packets. Failure
    // must cancel remaining admissions and remove the engine device before
    // Release joins its workers; neither case fabricates stream completion.
    // SAFETY: the box still owns the engine queue and its registered stream.
    unsafe { crate::bridge12::cancel_execution(state.engine_queue.as_raw() as usize, hr) };

    // Dropping the box releases the engine queue's single reference.
    drop(state);
}

/// Release this queue's WDDM context.
///
/// # Safety
/// `state` must be a live `QueueState` whose `h_context` came from
/// [`create_wddm_context`] and has not been destroyed.
unsafe fn destroy_wddm_context(state: &QueueState) -> ddi12::HRESULT {
    if state.h_context.is_null() {
        return S_OK;
    }
    // SAFETY: `h_device` is the device this queue was created against, and the
    // borrow lives only until the end of this function.
    let Some(dev) = (unsafe { device12::device(state.h_device) }) else {
        note_refusal(&L2_REFUSALS.queue_context_destroy_failed);
        return E_FAIL;
    };
    if dev.um_callbacks.is_null() {
        note_refusal(&L2_REFUSALS.queue_context_destroy_failed);
        return E_FAIL;
    }
    // SAFETY: as `create_wddm_context`.
    let Some(destroy_context_cb) = (unsafe { (*dev.um_callbacks).pfnDestroyContextCb }) else {
        note_refusal(&L2_REFUSALS.queue_context_destroy_failed);
        return E_FAIL;
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
    hr
}

// ---------------------------------------------------------------------------
// (d) Command pools — 4 slots
// ---------------------------------------------------------------------------

/// `pfnCalcPrivateCommandPoolSize`.
///
/// # Safety
/// `arg`, when non-null, must point at a live
/// `D3D12DDIARG_CREATE_COMMAND_POOL_0040` for the duration of the call.
unsafe extern "system" fn calc_private_command_pool_size(
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
/// flags word with a single legal value and carries no allocator class. The
/// allocator for each class is created on first `pfnResetCommandList`, where the
/// list finally supplies that authoritative class.
///
/// # Safety
/// `h_pool`'s `pDrvPrivate` must address the private block
/// [`calc_private_command_pool_size`] sized.
unsafe extern "system" fn create_command_pool(
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
            allocators: Arc::new(PoolAllocators::new()),
        });
    }
    S_OK
}

/// `pfnResetCommandPool` -> `ID3D12CommandAllocator::Reset()`.
///
/// Returns `VOID`. Every class materialised for this DDI pool is reset. A reset
/// before any list first used the pool has no engine allocator and is counted as
/// the benign no-op it is.
///
/// # Safety
/// `h_pool` must be a handle [`create_command_pool`] returned `S_OK` for.
unsafe extern "system" fn reset_command_pool(
    h_device: ddi12::D3D12DDI_HDEVICE,
    h_pool: ddi12::D3D12DDI_HCOMMANDPOOL_0040,
) {
    // SAFETY: the runtime supplies live device and pool handles for this call.
    let Some(pool) = (unsafe { pool_state(h_pool) }) else {
        note_refusal(&L2_REFUSALS.pool_bad_arg);
        return;
    };
    // SAFETY: device is borrowed for this DDI only.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&L2_REFUSALS.pool_no_device);
        return;
    };
    let mut any = false;
    let mut error = None;
    for (list_type, slot) in pool.allocators.slots() {
        let Ok(mut generations) = slot.lock() else { error = Some(E_FAIL); break; };
        if generations.current.is_none() { continue; }
        any = true;
        let Some(engine) = dev.engine.d3d12_device() else { error = Some(E_FAIL); break; };
        if let Err(hr) = generations.reset(&engine, list_type) {
            error = Some(hr);
            break;
        }
    }
    // No allocator/recorder mutex is held across the runtime error callback.
    if let Some(hr) = error {
        L2_REFUSALS.pool_reset_engine_failed.bump();
        if !device12::set_error(dev, hr) {
            L2_REFUSALS.pool_reset_error_unavailable.bump();
        }
        return;
    }
    if !any { note_refusal(&L2_REFUSALS.pool_reset_no_allocator); }
}

/// `pfnDestroyCommandPool`.
///
/// # Safety
/// `h_pool` must be a handle [`create_command_pool`] returned `S_OK` for and
/// which has not already been destroyed.
unsafe extern "system" fn destroy_command_pool(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_pool: ddi12::D3D12DDI_HCOMMANDPOOL_0040,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_pool`.
    let Some(slot) = (unsafe { Slot::<Boxed<PoolState>>::from_priv(h_pool.drv_private()) }) else {
        note_refusal(&L2_REFUSALS.pool_bad_arg);
        return;
    };
    // SAFETY: the slot holds either null or the one box `create_command_pool`
    // moved in. Dropping the box releases its `Arc`; recorder targets may keep
    // the allocator set alive without touching this runtime-owned private block.
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
unsafe extern "system" fn calc_private_command_recorder_size(
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
unsafe extern "system" fn create_command_recorder(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_COMMAND_RECORDER_0040,
    h_recorder: ddi12::D3D12DDI_HCOMMANDRECORDER_0040,
) -> ddi12::HRESULT {
    // SAFETY: the caller guarantees the slot lies in the sized private block.
    let Some(slot) = (unsafe { Slot::<Boxed<RecorderState>>::from_priv(h_recorder.drv_private()) })
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

    let Some(queue_type) = engine_list_type(a.QueueFlags) else {
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
            queue_type,
            target: Mutex::new(None),
        });
    }
    S_OK
}

/// `pfnCommandRecorderSetCommandPoolAsTarget` — bind a pool to a recorder.
///
/// The recorder's `QueueFlags` is not the allocator class. Time Spy's exact DDI
/// stream binds a DIRECT-compatible recorder and later resets a COMPUTE list
/// through it. This call therefore captures the pool's owned allocator set; the
/// list class selects and lazily creates the member at `pfnResetCommandList`.
///
/// Returns `VOID`; there is no engine call and therefore no deferred failure at
/// this point.
///
/// # Safety
/// `h_device` must be a live device handle, `h_recorder` a live recorder handle
/// and `h_pool` a live pool handle.
unsafe extern "system" fn command_recorder_set_command_pool_as_target(
    _h_device: ddi12::D3D12DDI_HDEVICE,
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

    bind_target(recorder, h_pool.drv_private() as usize, &pool.allocators);
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

/// Point a recorder at a pool, retaining the allocator set independently of the
/// runtime-owned pool private block.
fn bind_target(recorder: &RecorderState, pool: usize, allocators: &Arc<PoolAllocators>) {
    *lock_target(recorder) = Some(RecorderTarget {
        pool,
        allocators: Arc::clone(allocators),
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
/// Distinguishable failures rather than one `Option`, so L3a can report the
/// reset failure through the command-list error callback without guessing why
/// no allocator exists.
pub(crate) enum RecorderAllocator {
    /// The recorder names a pool and that pool is backed.
    Ready {
        /// **Owned** — one `AddRef` the caller releases by dropping.
        allocator: ID3D12CommandAllocator,
        /// The class requested by the command list and used at creation. The API
        /// cannot be asked for an allocator's class, so it is carried.
        list_type: D3D12_COMMAND_LIST_TYPE,
    },
    /// The recorder handle did not resolve to a live [`RecorderState`].
    NoRecorder,
    /// No `pfnCommandRecorderSetCommandPoolAsTarget` has ever run on it, so
    /// there is no pool and no allocator.
    NoPoolBound,
    /// The device or engine device needed for lazy creation was unavailable.
    NoDevice,
    /// The command list named a class this allocator set does not support.
    UnsupportedClass,
    /// `ID3D12Device::CreateCommandAllocator` failed.
    EngineFailed,
}

/// The allocator `pfnResetCommandList` must reset a list against.
///
/// ⭐ **`pub(crate)` because L3a's `pfnResetCommandList` is its only caller and
/// the chain it walks is entirely this lane's private state** — recorder ->
/// bound pool -> per-class `ID3D12CommandAllocator`. `PARALLEL.md` §4 gives L3a
/// the slot and this lane the three objects, and this function is that seam, in
/// the file that owns the objects. ⛔ It clones the target's `Arc` while holding
/// the recorder mutex, then releases that mutex before entering the engine.
///
/// # Safety
/// As [`recorder_state`], for a handle [`create_command_recorder`] returned
/// `S_OK` for.
pub(crate) unsafe fn recorder_allocator(
    h_device: ddi12::D3D12DDI_HDEVICE,
    h_recorder: ddi12::D3D12DDI_HCOMMANDRECORDER_0040,
    list_type: D3D12_COMMAND_LIST_TYPE,
) -> RecorderAllocator {
    // SAFETY: forwarded unchanged; the caller's guarantee is `recorder_state`'s.
    let Some(recorder) = (unsafe { recorder_state(h_recorder) }) else {
        return RecorderAllocator::NoRecorder;
    };
    let allocators = match lock_target(recorder).as_ref() {
        Some(target) => Arc::clone(&target.allocators),
        None => return RecorderAllocator::NoPoolBound,
    };
    let Some(slot) = allocators.slot(list_type) else {
        return RecorderAllocator::UnsupportedClass;
    };
    let Ok(mut generations) = slot.lock() else {
        note_refusal(&L2_REFUSALS.pool_allocator_engine_failed);
        return RecorderAllocator::EngineFailed;
    };
    if let Some(allocator) = &generations.current {
        return RecorderAllocator::Ready { allocator: allocator.clone(), list_type };
    }

    // SAFETY: device-scope lookup; the borrow lives only for this call.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&L2_REFUSALS.pool_no_device);
        return RecorderAllocator::NoDevice;
    };
    let Some(engine) = dev.engine.d3d12_device() else {
        note_refusal(&L2_REFUSALS.pool_no_device);
        return RecorderAllocator::NoDevice;
    };
    // SAFETY: `engine` is the live bridge device and `list_type` is the exact
    // class carried by the DDI command list being reset.
    let allocator =
        match unsafe { engine.CreateCommandAllocator::<ID3D12CommandAllocator>(list_type) } {
            Ok(allocator) => allocator,
            Err(e) => {
                note_refusal(&L2_REFUSALS.pool_allocator_engine_failed);
                if let Some(n) = budget(&POOL_LOG) {
                    log_error!(
                        "ResetCommandList: engine CreateCommandAllocator(type={}) failed \
                     hr={:#010x} (x{})",
                        list_type.0,
                        e.code().0 as u32,
                        n + 1,
                    );
                }
                return RecorderAllocator::EngineFailed;
            }
        };
    generations.current = Some(allocator.clone());
    RecorderAllocator::Ready { allocator, list_type }
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
unsafe extern "system" fn destroy_command_recorder(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_recorder: ddi12::D3D12DDI_HCOMMANDRECORDER_0040,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_recorder`.
    let Some(slot) = (unsafe { Slot::<Boxed<RecorderState>>::from_priv(h_recorder.drv_private()) })
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
        "DestroyCommandRecorder: recorder={:p} queueType={} lastPool={:#x}",
        h_recorder.pDrvPrivate,
        state.queue_type.0,
        target_pool_identity(&state),
    );
    // Dropping the box drops the target's `Arc` reference to the pool allocators.
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
unsafe extern "system" fn calc_private_command_list_size(
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
unsafe extern "system" fn create_command_list(
    h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_COMMAND_LIST_0040,
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_rt_list: ddi12::D3D12DDI_HRTCOMMANDLIST,
) -> ddi12::HRESULT {
    // SAFETY: the caller guarantees the slot lies in the sized private block.
    let Some(slot) = (unsafe { Slot::<Boxed<CommandListState>>::from_priv(h_list.drv_private()) })
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

    // `Type` distinguishes DIRECT-style lists from BUNDLE; COMPUTE and COPY are
    // expressed by `QueueFlags`. Bundle is authoritative by itself, while the
    // other DDI type must be translated through the queue flags. The resulting
    // API class is also the class that lazily selects this pool's allocator at
    // reset, so the two engine objects cannot disagree.
    let list_type = if a.Type == ddi12::D3D12DDI_COMMAND_LIST_TYPE_D3D12DDI_COMMAND_LIST_TYPE_BUNDLE
    {
        Some(D3D12_COMMAND_LIST_TYPE_BUNDLE)
    } else if a.Type == ddi12::D3D12DDI_COMMAND_LIST_TYPE_D3D12DDI_COMMAND_LIST_TYPE_DIRECT {
        engine_list_type(a.QueueFlags)
    } else {
        None
    };
    let Some(list_type) = list_type else {
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
    };

    // ⚠ `D3D12DDI_COMMAND_LIST_FLAGS` carries two marker hints
    // (`ENABLE_MARKERS`, `_0010_ENABLE_FULLPIPELINE_MARKERS`) that the **API**
    // `D3D12_COMMAND_LIST_FLAGS` has no counterpart for — it defines only `NONE`.
    // They are debug-tooling hints, so they are dropped and counted rather than
    // refused.
    if a.CommandListFlags != ddi12::D3D12DDI_COMMAND_LIST_FLAGS_D3D12DDI_COMMAND_LIST_FLAG_NONE {
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
unsafe extern "system" fn destroy_command_list(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(slot) = (unsafe { Slot::<Boxed<CommandListState>>::from_priv(h_list.drv_private()) })
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
// (d) Command signatures: translate the DDI contract and let the engine select
//     native DGC or its isolated fallback. Unsupported paths fail at creation.
// ---------------------------------------------------------------------------

/// Admission and translation for one indirect argument. Mesh and incrementing
/// constants require tiers this native UMD does not report; DXR stays separate.
enum IndirectArgClass {
    Forwarded(D3D12_INDIRECT_ARGUMENT_TYPE),
    StateTemplate,
    Raytracing,
    Unknown,
}

/// Classify one DDI indirect-argument type.
///
/// ⛔ **Translated, never cast.** All twelve `D3D12DDI_INDIRECT_ARGUMENT_TYPE`
/// enumerators are value-identical to their `D3D12_INDIRECT_ARGUMENT_TYPE` twins in
/// this SDK — and `DDI_REFERENCE.md` §9.6.1 is the standing evidence that a DDI enum
/// and its API twin can agree on a *value* while disagreeing on its meaning, with
/// the compiler silent because the member types match. Writing the arms out is what
/// makes the agreement something the compiler re-checks when either header moves.
/// ⚠ It also encodes the *classification*, which is not in either header at all.
fn indirect_argument_class(t: ddi12::D3D12DDI_INDIRECT_ARGUMENT_TYPE) -> IndirectArgClass {
    use ddi12::{
        D3D12DDI_INDIRECT_ARGUMENT_TYPE_D3D12DDI_INDIRECT_ARGUMENT_TYPE_CONSTANT as DDI_CONSTANT,
        D3D12DDI_INDIRECT_ARGUMENT_TYPE_D3D12DDI_INDIRECT_ARGUMENT_TYPE_CONSTANT_BUFFER_VIEW as DDI_CBV,
        D3D12DDI_INDIRECT_ARGUMENT_TYPE_D3D12DDI_INDIRECT_ARGUMENT_TYPE_DISPATCH as DDI_DISPATCH,
        D3D12DDI_INDIRECT_ARGUMENT_TYPE_D3D12DDI_INDIRECT_ARGUMENT_TYPE_DISPATCH_MESH as DDI_DISPATCH_MESH,
        D3D12DDI_INDIRECT_ARGUMENT_TYPE_D3D12DDI_INDIRECT_ARGUMENT_TYPE_DISPATCH_RAYS as DDI_DISPATCH_RAYS,
        D3D12DDI_INDIRECT_ARGUMENT_TYPE_D3D12DDI_INDIRECT_ARGUMENT_TYPE_DRAW as DDI_DRAW,
        D3D12DDI_INDIRECT_ARGUMENT_TYPE_D3D12DDI_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED as DDI_DRAW_INDEXED,
        D3D12DDI_INDIRECT_ARGUMENT_TYPE_D3D12DDI_INDIRECT_ARGUMENT_TYPE_INCREMENTING_CONSTANT as DDI_INCR_CONSTANT,
        D3D12DDI_INDIRECT_ARGUMENT_TYPE_D3D12DDI_INDIRECT_ARGUMENT_TYPE_INDEX_BUFFER_VIEW as DDI_IBV,
        D3D12DDI_INDIRECT_ARGUMENT_TYPE_D3D12DDI_INDIRECT_ARGUMENT_TYPE_SHADER_RESOURCE_VIEW as DDI_SRV,
        D3D12DDI_INDIRECT_ARGUMENT_TYPE_D3D12DDI_INDIRECT_ARGUMENT_TYPE_UNORDERED_ACCESS_VIEW as DDI_UAV,
        D3D12DDI_INDIRECT_ARGUMENT_TYPE_D3D12DDI_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW as DDI_VBV,
    };
    match t {
        DDI_DRAW => IndirectArgClass::Forwarded(D3D12_INDIRECT_ARGUMENT_TYPE_DRAW),
        DDI_DRAW_INDEXED => IndirectArgClass::Forwarded(D3D12_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED),
        DDI_DISPATCH => IndirectArgClass::Forwarded(D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH),
        DDI_DISPATCH_MESH | DDI_INCR_CONSTANT => IndirectArgClass::StateTemplate,
        DDI_DISPATCH_RAYS => IndirectArgClass::Raytracing,
        DDI_CONSTANT => IndirectArgClass::Forwarded(D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT),
        DDI_CBV => IndirectArgClass::Forwarded(D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT_BUFFER_VIEW),
        DDI_SRV => IndirectArgClass::Forwarded(D3D12_INDIRECT_ARGUMENT_TYPE_SHADER_RESOURCE_VIEW),
        DDI_UAV => IndirectArgClass::Forwarded(D3D12_INDIRECT_ARGUMENT_TYPE_UNORDERED_ACCESS_VIEW),
        DDI_VBV => IndirectArgClass::Forwarded(D3D12_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW),
        DDI_IBV => IndirectArgClass::Forwarded(D3D12_INDIRECT_ARGUMENT_TYPE_INDEX_BUFFER_VIEW),
        // ⚠ Not an `else` that picks the largest arm (`DECISIONS.md` §7.4): a type
        // this header does not name is refused, never guessed at.
        _ => IndirectArgClass::Unknown,
    }
}

/// Translate the active union arm after the enum has been classified. The DDI
/// and API structs have separate generated definitions; do not transmute them.
fn translate_indirect_argument(
    input: &ddi12::D3D12DDI_INDIRECT_ARGUMENT_DESC,
    ty: D3D12_INDIRECT_ARGUMENT_TYPE,
) -> D3D12_INDIRECT_ARGUMENT_DESC {
    let mut output = D3D12_INDIRECT_ARGUMENT_DESC {
        Type: ty,
        ..Default::default()
    };
    match ty {
        D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT => {
            // SAFETY: the caller classified Type as CONSTANT; read only that arm.
            let value = unsafe { input.__bindgen_anon_1.Constant };
            output.Anonymous.Constant = D3D12_INDIRECT_ARGUMENT_DESC_0_1 {
                RootParameterIndex: value.RootParameterIndex,
                DestOffsetIn32BitValues: value.DestOffsetIn32BitValues,
                Num32BitValuesToSet: value.Num32BitValuesToSet,
            };
        }
        D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT_BUFFER_VIEW => {
            // SAFETY: Type is CONSTANT_BUFFER_VIEW, whose arm contains one UINT.
            let value = unsafe { input.__bindgen_anon_1.ConstantBufferView };
            output.Anonymous.ConstantBufferView = D3D12_INDIRECT_ARGUMENT_DESC_0_0 {
                RootParameterIndex: value.RootParameterIndex,
            };
        }
        D3D12_INDIRECT_ARGUMENT_TYPE_SHADER_RESOURCE_VIEW => {
            // SAFETY: Type is SHADER_RESOURCE_VIEW; no other union arm is read.
            let value = unsafe { input.__bindgen_anon_1.ShaderResourceView };
            output.Anonymous.ShaderResourceView = D3D12_INDIRECT_ARGUMENT_DESC_0_3 {
                RootParameterIndex: value.RootParameterIndex,
            };
        }
        D3D12_INDIRECT_ARGUMENT_TYPE_UNORDERED_ACCESS_VIEW => {
            // SAFETY: Type is UNORDERED_ACCESS_VIEW; no other union arm is read.
            let value = unsafe { input.__bindgen_anon_1.UnorderedAccessView };
            output.Anonymous.UnorderedAccessView = D3D12_INDIRECT_ARGUMENT_DESC_0_4 {
                RootParameterIndex: value.RootParameterIndex,
            };
        }
        D3D12_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW => {
            // SAFETY: Type is VERTEX_BUFFER_VIEW, whose arm contains the slot.
            let value = unsafe { input.__bindgen_anon_1.VertexBuffer };
            output.Anonymous.VertexBuffer = D3D12_INDIRECT_ARGUMENT_DESC_0_5 { Slot: value.Slot };
        }
        // Action commands and INDEX_BUFFER_VIEW have no payload in this desc.
        _ => {}
    }
    output
}

/// `pfnCalcPrivateCommandSignatureSize`.
///
/// One machine word: the slot holds a bare owning `ID3D12CommandSignature*`.
/// ⛔ Answered unconditionally, and never 0 — see [`PRIVATE_SLOT_SIZE`]. A 0 would
/// hand the paired create a zero-byte region to write the slot word through.
///
/// # Safety
/// `arg`, when non-null, must point at a live
/// `D3D12DDIARG_CREATE_COMMAND_SIGNATURE_0001` for the duration of the call.
unsafe extern "system" fn calc_private_command_signature_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_COMMAND_SIGNATURE_0001,
) -> ddi12::SIZE_T {
    if arg.is_null() {
        note_refusal(&L2_REFUSALS.command_signature_bad_arg);
    }
    PRIVATE_SLOT_SIZE as ddi12::SIZE_T
}

/// The engine `ID3D12CommandSignature` behind a DDI signature handle, borrowed for
/// the caller's DDI call.
///
/// ⭐ **L3a's door into this lane**, and it exists so the payload of
/// `D3D12DDI_HCOMMANDSIGNATURE` is decoded in exactly one place — the
/// `com_handles!` invocation at the top of this file (`ARCHITECTURE.md` §12 rule 7 /
/// R803). `pfnExecuteIndirect` lives in `cmdlist.rs` and needs the object this
/// lane's create built; the same shape [`command_list_state`] takes for
/// `D3D12DDI_HCOMMANDLIST` and `fence::engine_query_heap` takes for
/// `D3D12DDI_HQUERYHEAP`.
///
/// ⚠ A `ManuallyDrop`, i.e. **borrowed**: [`create_command_signature`] moved the one
/// owning reference into the slot and [`destroy_command_signature`] releases it.
///
/// ⛔ A null `pDrvPrivate` and an empty slot both fold to `None`, and the caller
/// cannot tell them apart from here. That is deliberate and safe for this handle:
/// unlike a root signature, there is no legal "the runtime named no command
/// signature" call — `pfnExecuteIndirect` without one is meaningless — so both cases
/// are the same refusal.
///
/// # Safety
/// `h` must be a handle [`create_command_signature`] returned `S_OK` for and
/// [`destroy_command_signature`] has not been called on, and the returned value must
/// not outlive the DDI call that obtained it.
pub(crate) unsafe fn engine_command_signature(
    h: ddi12::D3D12DDI_HCOMMANDSIGNATURE,
) -> Option<core::mem::ManuallyDrop<ID3D12CommandSignature>> {
    // SAFETY: the caller guarantees a live handle, so its slot lies inside the
    // private block `calc_private_command_signature_size` sized.
    let slot = unsafe { Slot::<Com<ID3D12CommandSignature>>::from_priv(h.drv_private()) }?;
    // SAFETY: same precondition; `load` reads the slot word and reports an empty
    // slot as `None` rather than fabricating an interface.
    unsafe { slot.load() }
}

/// Create an engine signature from an explicit per-arm DDI translation.
/// Root-state and IA signatures require native DGC in the engine. Mesh,
/// incrementing constants and indirect ray dispatch remain explicitly refused.
/// Engine validation errors are preserved, and every failure leaves a null slot.
///
/// # Safety
/// The runtime supplies a live device, a readable create descriptor and its
/// declared argument array. The output handle addresses the private block sized
/// by `calc_private_command_signature_size`; non-null root handles are live.
unsafe extern "system" fn create_command_signature(
    h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_COMMAND_SIGNATURE_0001,
    h_signature: ddi12::D3D12DDI_HCOMMANDSIGNATURE,
) -> ddi12::HRESULT {
    // SAFETY: the runtime supplied the sized private output block.
    let Some(slot) =
        (unsafe { Slot::<Com<ID3D12CommandSignature>>::from_priv(h_signature.drv_private()) })
    else {
        note_refusal(&L2_REFUSALS.command_signature_bad_arg);
        return E_INVALIDARG;
    };
    // SAFETY: the block is writable and contains no prior live signature.
    unsafe { slot.clear() };
    if arg.is_null() {
        note_refusal(&L2_REFUSALS.command_signature_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: arg is non-null and the DDI declares a readable create descriptor.
    let a = unsafe { &*arg };
    let count = a.NumArgumentDescs as usize;
    if a.pArgumentDescs.is_null()
        || count == 0
        || count
            > isize::MAX as usize / core::mem::size_of::<ddi12::D3D12DDI_INDIRECT_ARGUMENT_DESC>()
        || a.NodeMask > 1
    {
        note_refusal(&L2_REFUSALS.command_signature_bad_arg);
        return E_INVALIDARG;
    }
    let mut api_descs = Vec::new();
    if api_descs.try_reserve_exact(count).is_err() {
        // Allocation failure must reach the HRESULT return without building
        // the allocating refusal summary. The normal summary reads this atomic.
        L2_REFUSALS.command_signature_translation_oom.bump();
        return E_OUTOFMEMORY;
    }
    let mut requires_root = false;
    for i in 0..count {
        // SAFETY: count's byte extent is representable, the array is non-null,
        // and the runtime guarantees every declared element is readable.
        let input = unsafe { &*a.pArgumentDescs.add(i) };
        let ty = match indirect_argument_class(input.Type) {
            IndirectArgClass::Forwarded(ty) => ty,
            IndirectArgClass::StateTemplate => {
                note_refusal(&L2_REFUSALS.command_signature_state_template_refused);
                return E_NOTIMPL;
            }
            IndirectArgClass::Raytracing => {
                note_refusal(&L2_REFUSALS.command_signature_raytracing_refused);
                return E_NOTIMPL;
            }
            IndirectArgClass::Unknown => {
                note_refusal(&L2_REFUSALS.command_signature_arg_type_unknown);
                return E_INVALIDARG;
            }
        };
        requires_root |= matches!(
            ty,
            D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT
                | D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT_BUFFER_VIEW
                | D3D12_INDIRECT_ARGUMENT_TYPE_SHADER_RESOURCE_VIEW
                | D3D12_INDIRECT_ARGUMENT_TYPE_UNORDERED_ACCESS_VIEW
        );
        api_descs.push(translate_indirect_argument(input, ty));
    }
    // SAFETY: device-scope DDI; this borrow remains inside the current call.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&L2_REFUSALS.command_signature_no_device);
        return E_FAIL;
    };
    let Some(engine) = dev.engine.d3d12_device() else {
        note_refusal(&L2_REFUSALS.command_signature_no_device);
        return E_FAIL;
    };
    let root_signature = if a.hRootSignature.pDrvPrivate.is_null() {
        None
    } else {
        if !requires_root {
            note_refusal(&L2_REFUSALS.command_signature_root_sig_unexpected);
        }
        // SAFETY: the non-null root handle names a live private block from L6.
        let resolved = unsafe { pso::root_signature(a.hRootSignature) };
        if resolved.is_none() {
            note_refusal(&L2_REFUSALS.command_signature_root_sig_unresolved);
            return E_INVALIDARG;
        }
        resolved
    };
    let desc = D3D12_COMMAND_SIGNATURE_DESC {
        ByteStride: a.ByteStride,
        NumArgumentDescs: a.NumArgumentDescs,
        pArgumentDescs: api_descs.as_ptr(),
        NodeMask: a.NodeMask,
    };
    let mut signature: Option<ID3D12CommandSignature> = None;
    // SAFETY: the translated array and borrowed root live through this call;
    // vkd3d copies the descriptors and returns one owned COM reference.
    if let Err(e) =
        unsafe { engine.CreateCommandSignature(&desc, root_signature.as_deref(), &mut signature) }
    {
        if e.code().0 == E_OUTOFMEMORY {
            // As above, neither note_refusal nor formatted logging is safe
            // while recovering from sustained allocation exhaustion.
            L2_REFUSALS.command_signature_engine_failed.bump();
            return E_OUTOFMEMORY;
        }
        if e.code().0 == E_NOTIMPL {
            note_refusal(&L2_REFUSALS.command_signature_state_template_refused);
        } else {
            note_refusal(&L2_REFUSALS.command_signature_engine_failed);
        }
        if let Some(n) = budget(&QUEUE_LOG) {
            log_error!(
                "CreateCommandSignature: engine refused stride={} arguments={} hr={:#010x} (x{})",
                a.ByteStride,
                count,
                e.code().0 as u32,
                n + 1,
            );
        }
        return e.code().0;
    }
    let Some(signature) = signature else {
        note_refusal(&L2_REFUSALS.command_signature_engine_failed);
        return E_FAIL;
    };
    // SAFETY: the writable slot is null; it now owns the returned reference.
    unsafe { slot.store(signature) };
    note_refusal(&L2_REFUSALS.command_signature_created);
    S_OK
}

/// `pfnDestroyCommandSignature`.
///
/// # Safety
/// `h_signature` must be a handle the runtime associated with a
/// `pfnCreateCommandSignature` call on this device.
unsafe extern "system" fn destroy_command_signature(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_signature: ddi12::D3D12DDI_HCOMMANDSIGNATURE,
) {
    // SAFETY: the caller guarantees a handle from `pfnCreateCommandSignature`.
    let Some(slot) =
        (unsafe { Slot::<Com<ID3D12CommandSignature>>::from_priv(h_signature.drv_private()) })
    else {
        note_refusal(&L2_REFUSALS.command_signature_bad_arg);
        return;
    };
    // SAFETY: the slot holds either null — a create this driver refused, which is
    // legal and is what `CommandSignatureDestroyUnexpected` used to count — or the
    // one reference `create_command_signature` moved in. `release` is idempotent on
    // null.
    unsafe { slot.release() };
}

// ---------------------------------------------------------------------------
// The WDDM submission — K-F1
// ---------------------------------------------------------------------------

/// Exact registered worker boundary for an ECL or Present producer packet.
/// `gpu_wire_fence` is **always 0** since 2026-09-13: the sampled wire-fence lever
/// was withdrawn (see the note at each producer), so the field is carried for the
/// record's version-3 shape and read by the KMD as "no boundary", i.e. exactly
/// version-2 behaviour. The gate this packet needs is the registered producer
/// stream (`ctx`/`value`/`cookie`), not a wire fence - `EXECUTION_SYNC.md`
/// "Contract and ordering", and `docs/dx12/KMD_IMPACT.md` section 14a.2.
fn ecl_submit_command(
    boundary: (u32, u32, u64),
    gpu_wire_fence: u64,
) -> helios_protocol::HeliosD3D12SubmitCmd {
    helios_protocol::HeliosD3D12SubmitCmd {
        magic: helios_protocol::HELIOS_D3D12_SUBMIT_MAGIC,
        version: helios_protocol::HELIOS_D3D12_SUBMIT_VERSION,
        ctx_id: boundary.0,
        value: boundary.1,
        cookie: boundary.2,
        gpu_wire_fence,
    }
}

/// The three distinguishable outcomes of one [`submit_wddm_render`] attempt.
///
/// ⚠ `pub(crate)` since UP-9, because [`submit_present_identity`] hands it to L8 —
/// which needs all three arms apart for exactly the reason below: they choose
/// different error channels, and collapsing any two would decide the severity of a
/// refused present by accident.
///
/// Missing prerequisites and callback rejection remain distinguishable for
/// diagnosis. Both cancel execution and report an error to the runtime.
pub(crate) enum WddmSubmit {
    /// The packet went in and the windows were re-latched from its out-fields.
    Submitted,
    /// **This driver could not make a packet** — no `pfnRenderCb`, no context, no
    /// command window, or a window smaller than the payload. The caller counts
    /// the failure and cancels the queue with E_NOTIMPL.
    Unavailable,
    /// **`pfnRenderCb` refused a packet this driver did make**, carrying dxgkrnl's
    /// own HRESULT. The caller cancels execution and passes that error through
    /// `pfnSetErrorCb`.
    Refused(ddi12::HRESULT),
}

/// Submit one runtime-owned WDDM command buffer on this queue's legacy context.
///
/// ⭐⭐ **The single `pfnRenderCb` call site in this driver, shared by both of its
/// users** (`KMD_IMPACT.md` §14a.2 FB-1, §14a.4 point 2): K-F1's fence carrier
/// today, and §14a.3 UP-9's present identity record next. It is the same
/// consolidation `umd/src/forward/present.rs:781`'s `submit_runtime_submission`
/// makes for D3D11, where both present variants go through one function for one
/// reason — six window updates and a bounds check written twice are six chances
/// to diverge.
///
/// `command` is written at `CommandOffset = 0` with `CommandLength =
/// size_of::<T>()`. `NumPatchLocations` is always **0**: Helios' GpuMmu is
/// decorative, so there is nothing to patch.
///
/// `NumAllocations` is always **0**. Both records are metadata: the ECL packet names
/// a completion boundary and the present packet names a venus resource. The D3D12
/// runtime owns residency through `pfnMakeResidentCb` and carries the actual source
/// allocation in `D3D12DDI_PRESENT_0051::BroadcastSrcAllocation`; unlike D3D11's
/// copy submission, neither Render packet reads or writes an allocation.
///
/// ⛔ This was settled live rather than inferred from the D3D11 template. On the
/// same D3D12 context, 900 zero-allocation ECL Render callbacks succeeded while the
/// first otherwise-identical 80-byte HEPR callback with one allocation-list entry
/// was refused by dxgkrnl with `E_FAIL` before `DxgkDdiRender`. An empty list makes
/// the metadata submission match what it actually does and leaves residency on the
/// two D3D12 channels that own it.
///
/// ⛔ **The list is written inside this function and under the same guard as the
/// command**, which is why it cannot be a caller's job: [`QueueState::windows`]
/// requires one thread per `HCONTEXT` across write → `pfnRenderCb` → re-latch, and
/// a caller that wrote the list before taking the guard would be writing a buffer
/// dxgkrnl may already have rotated away.
///
/// ⚠ **Generic over the record, deliberately, and this is what makes it shareable.**
/// One typed `write_unaligned` per command type is exactly the D3D11 shape
/// (`umd/src/forward/present.rs:824`, where *"a variant that writes the wrong
/// command is no longer representable"*), and it keeps the ABI struct's layout
/// where it is declared instead of hand-assembling bytes here —
/// `ARCHITECTURE.md` §12 rule 1: never hand-transcribe an ABI struct.
///
/// Every FAILURE arm counts itself before returning; the [`WddmSubmit`] it hands
/// back is what decides the caller's *error channel*, and the two are not the same
/// question.
///
/// ⛔ **The SUCCESS counter is the caller's, and that is an attribution decision.**
/// This function used to bump `EclWddmSubmitted` itself, which was exact while
/// `pfnExecuteCommandLists` was the only caller and becomes a confounded number the
/// moment a present shares it — the `METHOD.md` instrument-attribution lens, three
/// instances of which this project has already paid for in the KMD's counters. The
/// ECL arm's documented arithmetic (`EclForwarded == EclWddmSubmitted +
/// EclNoWddmSubmission`) only stays checkable if presents are not in the sum. ⇒ each
/// caller counts its own `Submitted`.
///
/// ⚠ The six *cause* counters (`EclSubmitNoKtCb`, `EclSubmitNoRenderCb`,
/// `EclSubmitNoContext`, `EclSubmitNoCmdWindow`, `EclSubmitWindowSmall`,
/// `EclSubmitRenderFailed`) stay **shared**, and their `Ecl` names are now legacy:
/// every one of them is a fact about dxgkrnl's callback table or about the windows on
/// *this queue's context*, which is the same context both callers submit on, so a hit
/// means the same thing whichever DDI produced it. `label` is what says which. Their
/// docs carry the widening; the names were kept because renaming a counter changes
/// every `D3D12 DDI refusals:` line it appears in.
///
/// # Safety
/// `dev` must be the live device `queue` was created against, and `queue` a live
/// [`QueueState`] whose `h_context` this thread may submit on. ⛔ The caller must
/// be inside the DDI that owns the submission, on the thread that entered it:
/// `ResourceHeaps.md:1678` requires both (`DDI_REFERENCE.md` §8.2 obligations 1
/// and 2), so this must never be handed to a worker.
///
/// `T` must be a `#[repr(C)]` plain-old-data record with no padding and no
/// pointers — every one of its bytes is copied into a buffer the kernel reads.
/// ⚠ The guarantee is enforced where the records are declared, not here:
/// `helios_protocol`'s wire structs derive `bytemuck::Pod`, which is exactly that
/// property, and `umd12` cannot name the bound because it does not depend on
/// `bytemuck` (`umd12/Cargo.toml` takes `helios_protocol` alone).
unsafe fn submit_wddm_render<T: Copy>(
    dev: &device12::HeliosD3D12Device,
    queue: &QueueState,
    command_record: &T,
    label: &'static str,
) -> WddmSubmit {
    if dev.kt_callbacks.is_null() {
        // ⚠ Expected unreachable: `create_device` refuses a null `pKTCallbacks`
        // before the device exists. Counted rather than asserted, because "the
        // table this driver's whole kernel surface hangs off was absent" is worth
        // a number if it ever happens.
        note_refusal(&L2_REFUSALS.ecl_submit_no_kt_callbacks);
        return WddmSubmit::Unavailable;
    }
    // SAFETY: non-null per the check above. `kt_callbacks` is the runtime's
    // `D3DDDI_DEVICECALLBACKS`, stored by `create_device` and never reassigned,
    // for a table the runtime keeps alive at least as long as the device.
    let Some(render_cb) = (unsafe { (*dev.kt_callbacks).pfnRenderCb }) else {
        // ⛔ The kernel table is 65 entries of `Option<fn>` and dxgkrnl fills the
        // ones it supports. A missing `pfnRenderCb` would mean this adapter's
        // legacy submission path does not exist, which would invalidate the whole
        // legacy-context decision — so it is its own counter, not folded into the
        // one above.
        note_refusal(&L2_REFUSALS.ecl_submit_render_cb_missing);
        if let Some(n) = budget(&ECL_LOG) {
            log_error!("{label}: pKTCallbacks->pfnRenderCb is absent (x{})", n + 1,);
        }
        return WddmSubmit::Unavailable;
    };
    if queue.h_context.is_null() {
        // Expected unreachable for the same reason as `kt_callbacks`:
        // `create_wddm_context` fails the queue create on a null `hContext`.
        note_refusal(&L2_REFUSALS.ecl_submit_no_context);
        return WddmSubmit::Unavailable;
    }

    // ⛔ THE LOCK IS TAKEN HERE AND HELD TO THE END OF THIS FUNCTION — across the
    // payload write, across `pfnRenderCb`, and across the re-latch.
    // [`QueueState::windows`] carries the contract argument (one thread per
    // HCONTEXT) and the deadlock argument. Narrowing it is the bug.
    let mut windows = lock_windows(queue);

    let (command, capacity) = window_parts(&windows.command);
    if command.is_null() {
        note_refusal(&L2_REFUSALS.ecl_submit_no_command_window);
        if let Some(n) = budget(&ECL_LOG) {
            log_error!(
                "{label}: context {:p} has no command window -- cannot submit (x{})",
                queue.h_context,
                n + 1,
            );
        }
        return WddmSubmit::Unavailable;
    }
    // ⛔ Validate the RUNTIME's capacity against our length, per-arm, before
    // writing. AGENTS.md's rule, and the reason it is not a formality: the window
    // is dxgkrnl's and its size is dxgkrnl's choice — a driver that assumes
    // "surely at least 16 bytes" is one whose first out-of-bounds write lands in
    // the kernel's own command buffer.
    let record_size = core::mem::size_of::<T>();
    let Ok(command_length) = u32::try_from(record_size) else {
        note_refusal(&L2_REFUSALS.ecl_submit_window_too_small);
        return WddmSubmit::Unavailable;
    };
    if capacity < command_length {
        note_refusal(&L2_REFUSALS.ecl_submit_window_too_small);
        if let Some(n) = budget(&ECL_LOG) {
            log_error!(
                "{label}: command window {command:p} holds {capacity} bytes, need \
                 {command_length} -- not submitting (x{})",
                n + 1,
            );
        }
        return WddmSubmit::Unavailable;
    }

    // SAFETY: `command` is the runtime's command-buffer window, non-null and proven
    // to hold at least `size_of::<T>()` bytes by the two checks above, and it can
    // never overlap `command_record` — one is dxgkrnl's buffer and the other the
    // caller's local. ⛔ `write_unaligned`, never a plain store: dxgkrnl promises
    // the window a SIZE and no alignment, and a typed store through a
    // `*mut T` cast would be UB the moment the runtime hands back an odd pointer.
    // Same call and same reason as `umd/src/forward/present.rs:824`.
    unsafe { command.cast::<T>().write_unaligned(*command_record) };

    let mut render = ddi12::D3DDDICB_RENDER {
        CommandLength: command_length,
        CommandOffset: 0,
        NumAllocations: 0,
        NumPatchLocations: 0,
        hContext: queue.h_context,
        ..Default::default()
    };
    // SAFETY: a non-null callback out of the runtime's own kernel table, given a
    // fully initialised out-struct local and this queue's own context handle. The
    // runtime reads the four fields set above and writes the `pNew*` group, which
    // the re-latch below consumes. Nothing is transferred.
    let hr = unsafe { render_cb(dev.h_rt_device.handle, &mut render) };

    if hr < 0 {
        // ⛔ `hr < 0`, not `hr != S_OK` — the same normalisation
        // `create_wddm_context` uses and for the same reason: a non-negative
        // non-`S_OK` value is a SUCCESS code, and treating `S_FALSE` as a failure
        // here would report a device error for a submission that happened.
        // ⛔ And NO re-latch on this path: the out-fields promise nothing.
        // RenderCb may legally return E_OUTOFMEMORY after the engine operation
        // was committed. Reach the caller's cancellation/error path without
        // allocating a diagnostic summary or formatted message first.
        if hr == helios_umd_common::hr::E_OUTOFMEMORY {
            L2_REFUSALS.ecl_submit_render_failed.bump();
        } else {
            note_refusal(&L2_REFUSALS.ecl_submit_render_failed);
            if let Some(n) = budget(&ECL_LOG) {
                log_error!(
                    "{label}: pfnRenderCb(ctx={:p}, len={command_length}) failed \
                     hr={:#010x} (x{})",
                    queue.h_context,
                    hr as u32,
                    n + 1,
                );
            }
        }
        return WddmSubmit::Refused(hr);
    }

    windows.re_latch(&render);
    // ⛔ No success counter here — see this function's doc. The caller owns it.
    trace_line!(
        "{label}: pfnRenderCb ok ctx={:p} len={command_length} allocations=0 \
         queued={} next_cmd={:p}/{}",
        queue.h_context,
        render.QueuedBufferCount,
        render.pNewCommandBuffer,
        render.NewCommandBufferSize,
    );
    WddmSubmit::Submitted
}

/// Submit one present's identity record on the queue's WDDM context — **UP-9, and
/// L8's second seam into this file** ([`present_context`] is the first).
///
/// ⭐ It exists rather than L8 calling [`submit_wddm_render`] directly for the same
/// reason [`present_context`] returns a handle: `submit_wddm_render` needs a
/// `&QueueState`, and handing one across the module boundary would export
/// [`QueueState::windows`]' guard discipline — the guard that must span write →
/// `pfnRenderCb` → re-latch — to a file that does not own it.
///
/// ⛔ **The caller decides what a failure means, and this function decides
/// nothing.** It reports the [`WddmSubmit`] verbatim; `report_present_submit_error`
/// is the channel for the one arm that must reach the runtime.
///
/// ⚠ `WddmSubmit::Unavailable` is returned for a queue handle that did not resolve,
/// which is *not* the same shape as [`submit_wddm_render`]'s other unavailable arms —
/// but it is the same fact for the caller (no packet went in, nothing to report to the
/// runtime), so it takes the same variant with its own counter.
///
/// # Safety
/// `h_queue` must be a handle [`create_command_queue`] returned `S_OK` for.
/// ⛔ The caller must be inside `pfnPresent` on the thread that entered it —
/// [`submit_wddm_render`]'s obligations 1 and 2, forwarded unchanged.
pub(crate) unsafe fn submit_present_identity(
    h_queue: ddi12::D3D12DDI_HCOMMANDQUEUE,
    record: &helios_protocol::HeliosPresentRenderCmd,
) -> WddmSubmit {
    // SAFETY: forwarded to `queue_state`'s identical precondition; the borrow ends
    // inside this function.
    let Some(queue) = (unsafe { queue_state(h_queue) }) else {
        note_refusal(&L2_REFUSALS.present_submit_no_queue);
        return WddmSubmit::Unavailable;
    };
    // SAFETY: `queue.h_device` is the device handle this queue was created against
    // and the queue is live, so the device is.
    let Some(dev) = (unsafe { device12::device(queue.h_device) }) else {
        // Expected unreachable — a live queue implies a live device — and its own
        // counter rather than the queue one, because the two would need different
        // fixes.
        note_refusal(&L2_REFUSALS.present_submit_no_device);
        return WddmSubmit::Unavailable;
    };
    // SAFETY: `dev` is the live device `queue` was created against, `queue` is live
    // for this call, and the caller guarantees we are inside `pfnPresent` on the
    // entering thread — `submit_wddm_render`'s three obligations.
    // Keep an ECL's Render + admission signal contiguous on this context.
    let _execution = queue
        .execution
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // SAFETY: runtime-owned context and windows, on the entering Present thread.
    let outcome = unsafe { submit_wddm_render(dev, queue, record, "Present identity") };
    if matches!(outcome, WddmSubmit::Submitted) {
        // ⭐ Counted HERE and not in L8, because this function is present-scoped by
        // construction — nothing else calls it — so the counter cannot be confounded
        // the way `EclWddmSubmitted` would have been. L8 keeps its own refusals.
        note_refusal(&L2_REFUSALS.present_identity_submitted);
    }
    outcome
}

/// Report a **refused** present-identity submission to the runtime.
///
/// ⛔ **`pfnSetErrorCb`, and the argument is [`report_ecl_submit_error`]'s three
/// reasons re-derived for a slot that has a command-list handle in scope** — which is
/// exactly the situation in which this project got the channel wrong 49 times, so it
/// is argued rather than copied:
///
/// 1. ⛔ **`pfnSetCommandListErrorCb` quarantines RECORDING, and a present records
///    nothing.** Its documented effect is *"the runtime will drop all calls into the
///    driver which record commands on the specified command list"*. `pfnPresent` is
///    handed an `hCommandList`, but this driver writes nothing into it —
///    `AddedGpuWork` is FALSE precisely because nothing is recorded — so dropping
///    future recording calls on it reports the failure to nobody.
/// 2. **What failed is not one list's recording.** It is the queue's kernel
///    submission of this frame's identity, on the queue's own WDDM context.
/// 3. **There is no per-queue error callback in `_0062`**, so the device callback is
///    the only channel left. Same conclusion as `fence_operation` and
///    [`report_ecl_submit_error`], down to reusing `QueueSetErrorUnavailable` when
///    the channel itself is absent.
///
/// # ⛔ Why the severity is device removal and no knob softens it
///
/// `pfnRenderCb` refused a packet this driver built, on the context every later
/// present on this queue will use. That does not get better next frame, and
/// `METHOD.md` §2 Phase 4 consequence 1 forbids a knob whose default exists to keep a
/// run alive. ⚠ It is deliberately **not** the severity of `Unavailable`: that arm is
/// *this driver* failing to make a packet, which is the same state the ECL path
/// declines to raise, and it leaves the frame without an identity rather than without
/// a device.
///
/// # Safety
/// `h_queue` must be a handle [`create_command_queue`] returned `S_OK` for.
pub(crate) unsafe fn report_present_submit_error(
    h_queue: ddi12::D3D12DDI_HCOMMANDQUEUE,
    hr: ddi12::HRESULT,
) {
    // SAFETY: forwarded to `queue_state`'s identical precondition.
    let Some(queue) = (unsafe { queue_state(h_queue) }) else {
        note_refusal(&L2_REFUSALS.queue_set_error_unavailable);
        return;
    };
    report_ecl_submit_error(queue, hr);
}

struct AdmissionEvent(windows::Win32::Foundation::HANDLE);
impl Drop for AdmissionEvent {
    fn drop(&mut self) {
        // SAFETY: this owns exactly one CreateEvent handle. The worker and
        // dxgkrnl retain their own references before this owner is dropped.
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// Queue a one-shot CPU event immediately AFTER this ECL's Render packet.
/// SignalAtSubmission signals when that packet is submitted, before completion;
/// therefore its own pending completion cannot cycle with the worker's wait.
/// Prior runtime fence waits must be satisfied before the packet is submitted.
/// Contract: d3dumddi D3DDDICB_SIGNALSYNCHRONIZATIONOBJECT2 and SIGNALFLAGS.
/// # Safety
/// Live runtime device/queue, called on the entering ECL or Present DDI thread.
unsafe fn enqueue_runtime_admission(
    dev: &device12::HeliosD3D12Device,
    queue: &QueueState,
    event: &AdmissionEvent,
) -> Result<(), ddi12::HRESULT> {
    if dev.kt_callbacks.is_null() {
        return Err(E_NOTIMPL);
    }
    // SAFETY: the device keeps its runtime-owned callback table alive.
    let callback =
        unsafe { (*dev.kt_callbacks).pfnSignalSynchronizationObject2Cb }.ok_or(E_NOTIMPL)?;
    let mut args = ddi12::D3DDDICB_SIGNALSYNCHRONIZATIONOBJECT2 {
        hContext: queue.h_context,
        ..Default::default()
    };
    args.Flags.__bindgen_anon_1.Value = 3; // SignalAtSubmission | EnqueueCpuEvent
    args.__bindgen_anon_1.CpuEventHandle = event.0 .0;
    // SAFETY: ObjectCount=0, valid owned event, no broadcasts, exact runtime context.
    let hr = unsafe { callback(dev.h_rt_device.handle, &args) };
    if hr < 0 {
        Err(hr)
    } else {
        Ok(())
    }
}

fn report_ecl_submit_error(queue: &QueueState, hr: ddi12::HRESULT) {
    // SAFETY: QueueState owns this engine queue throughout error propagation.
    unsafe { crate::bridge12::cancel_execution(queue.engine_queue.as_raw() as usize, hr) };

    // SAFETY: `h_device` is the device this queue was created against; the borrow
    // lives only until the end of this statement.
    let reported =
        unsafe { device12::device(queue.h_device) }.is_some_and(|dev| device12::set_error(dev, hr));
    if !reported {
        // An unavailable error channel must not reallocate on OOM recovery.
        if hr == helios_umd_common::hr::E_OUTOFMEMORY {
            L2_REFUSALS.queue_set_error_unavailable.bump();
        } else {
            note_refusal(&L2_REFUSALS.queue_set_error_unavailable);
        }
    }
}

// ---------------------------------------------------------------------------
// The command-queue table — 7 slots
// ---------------------------------------------------------------------------

/// `pfnExecuteCommandLists` — the **only** submission entry point in the
/// baseline set (`DDI_REFERENCE.md` §5), and the one queue slot `D12-G5` ever
/// saw called.
///
/// Commits the exact worker boundary, submits it on the runtime context, and
/// queues admission before returning. Failure cancels the engine operation.
///
/// # Safety
/// `h_queue` must be a live queue handle; `lists` must address `count` readable
/// `D3D12DDI_HCOMMANDLIST`s, each a live handle from [`create_command_list`].
unsafe extern "system" fn execute_command_lists(
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
    // array. AGENTS.md's rule, and the bound is `MAX_EXECUTE_COMMAND_LISTS`.
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

    let _execution = queue
        .execution
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // SAFETY: the queue carries its live device and exact runtime context.
    let Some(dev) = (unsafe { device12::device(queue.h_device) }) else {
        note_refusal(&L2_REFUSALS.ecl_no_wddm_submission);
        report_ecl_submit_error(queue, E_FAIL);
        return;
    };
    // The worker must wait until the matching Render packet is admitted. Create
    // its initially unsignaled event now; enqueue the runtime signal after Render.
    // SAFETY: unnamed manual-reset event, no borrowed security attributes.
    let admission =
        match unsafe { windows::Win32::System::Threading::CreateEventW(None, true, false, None) } {
            Ok(event) => AdmissionEvent(event),
            Err(error) => {
                note_refusal(&L2_REFUSALS.ecl_admission_failed);
                report_ecl_submit_error(queue, error.code().0);
                return;
            }
        };
    let commands: Vec<usize> = engine_lists
        .iter()
        .flatten()
        .map(|list| list.as_raw() as usize)
        .collect();
    // SAFETY: owned list interfaces and event outlive this call. The engine
    // duplicates the event and retains allocator/command-buffer lifetimes.
    let boundary = match unsafe {
        crate::bridge12::execute(
            queue.engine_queue.as_raw() as usize,
            &commands,
            admission.0 .0 as usize,
        )
    } {
        Ok(boundary) => boundary,
        Err(hr) => {
            note_refusal(&L2_REFUSALS.ecl_worker_failed);
            report_ecl_submit_error(queue, hr);
            return;
        }
    };
    L2_REFUSALS.ecl_forwarded.bump();
    L2_REFUSALS.ecl_exact_boundary.bump();
    // ⛔ WIRE FENCE WITHDRAWN (2026-09-13): the D3D12 record carries
    // `gpu_wire_fence = 0`, i.e. the pre-lever retire domain. The sampled Venus
    // wire fence was measured not to fix the early fence (allocator failed 2 of 2
    // with it and 2 of 3 without) and `EXECUTION_SYNC.md` rejects it by design: a
    // sampled wire fence can precede worker execution. The gate this packet needs
    // is the registered producer stream (`ctx`/`value`/`cookie` above), which the
    // engine signals on ALL_COMMANDS after execution. See KMD_IMPACT §14a.2.
    let gpu_wire_fence = 0u64;
    // SAFETY: required runtime callback stays on the entering DDI thread.
    match unsafe {
        submit_wddm_render(
            dev,
            queue,
            &ecl_submit_command(boundary, gpu_wire_fence),
            "ExecuteCommandLists",
        )
    } {
        WddmSubmit::Submitted => {
            L2_REFUSALS.ecl_wddm_submitted.bump();
            // SAFETY: same entering thread and exact context as the Render above.
            // SignalAtSubmission is essential: a completion event here deadlocks.
            match unsafe { enqueue_runtime_admission(dev, queue, &admission) } {
                Ok(()) => note_refusal(&L2_REFUSALS.ecl_admission_queued),
                Err(hr) => {
                    note_refusal(&L2_REFUSALS.ecl_admission_failed);
                    report_ecl_submit_error(queue, hr);
                }
            }
        }
        WddmSubmit::Refused(hr) => {
            note_refusal(&L2_REFUSALS.ecl_no_wddm_submission);
            report_ecl_submit_error(queue, hr);
        }
        WddmSubmit::Unavailable => {
            note_refusal(&L2_REFUSALS.ecl_no_wddm_submission);
            report_ecl_submit_error(queue, E_NOTIMPL);
        }
    }

    // Existing diagnostic return delay; never used as completion evidence.
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
/// The WDK declares a reserved `void*`, so no callable signature exists.
/// Count and terminate if it is ever invoked: returning would invent a stack
/// cleanup convention, which corrupts the caller on x86.
unsafe extern "system" fn queue_unused_slot() -> ! {
    note_refusal(&L2_REFUSALS.queue_unused_slot_called);
    std::process::abort();
}

/// `pfnUnused2` — queue-table slot 2. Separate function, separate counter, for
/// the one reason that matters: a shared body could not say *which* of the two
/// the runtime called, and that is the entire content of the observation.
unsafe extern "system" fn queue_unused2_slot() -> ! {
    note_refusal(&L2_REFUSALS.queue_unused2_slot_called);
    std::process::abort();
}

#[path = "tiles.rs"]
pub(crate) mod tiles;

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

/// Native queue fence operations cannot be implemented by inventing an engine
/// fence: CreateFence gives no runtime sync-object handle, CPU mapping or initial
/// value. Software runtime waits/signals are enforced at the ECL context edges.
/// If a workload enters these native DDIs, fail explicitly instead of dropping
/// a wait or forwarding it to an unrelated timeline.
/// # Safety
/// Live queue and one writable runtime fence-operation argument.
unsafe fn fence_operation(
    which: FenceOp,
    h_queue: ddi12::D3D12DDI_HCOMMANDQUEUE,
    op_arg: *mut ddi12::D3D12DDIARG_FENCE_OPERATION,
) {
    note_refusal(match which {
        FenceOp::Signal => &L2_REFUSALS.fence_signal_entered,
        FenceOp::Wait => &L2_REFUSALS.fence_wait_entered,
    });
    if op_arg.is_null() {
        note_refusal(&L2_REFUSALS.fence_op_bad_arg);
        return;
    }
    // SAFETY: checked writable argument; PhysicalAdapterMask is an output.
    let op = unsafe { &mut *op_arg };
    op.PhysicalAdapterMask = 1;
    // SAFETY: live queue and fence handles supplied by the runtime.
    let Some(queue) = (unsafe { queue_state(h_queue) }) else {
        note_refusal(&L2_REFUSALS.fence_op_bad_arg);
        return;
    };
    // SAFETY: runtime supplies a live fence private block for this operation.
    let valid =
        unsafe { fence::fence_state(op.Fence) }.is_some_and(|f| f.belongs_to(queue.h_device));
    if !valid {
        note_refusal(&L2_REFUSALS.fence_op_fence_missing);
        report_ecl_submit_error(queue, E_INVALIDARG);
        return;
    }
    note_refusal(&L2_REFUSALS.fence_native_refused);
    if budget(&FENCE_OP_LOG).is_some() {
        log_error!(
            "{}: native fence operation value={} is unsupported on the software context",
            which.name(),
            op.Value
        );
    }
    report_ecl_submit_error(queue, E_NOTIMPL);
}

/// `pfnSignalFence`.
///
/// # Safety
/// As [`fence_operation`].
unsafe extern "system" fn signal_fence(
    h_queue: ddi12::D3D12DDI_HCOMMANDQUEUE,
    op_arg: *mut ddi12::D3D12DDIARG_FENCE_OPERATION,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is `fence_operation`'s.
    unsafe { fence_operation(FenceOp::Signal, h_queue, op_arg) }

    // ⛔⛔ DIAGNOSTIC ARM — INERT BY DEFAULT (`Umd12FenceSignalDelayUs` absent =
    // 0 = no delay, so a run with no knob set is byte-identical to the build
    // that never heard of it). It is a producer-side CPU stall, which
    // `umd/src/knobs.rs:31-43` forbids as a *fix*; it is legal here only as a
    // MEASUREMENT with a question attached.
    //
    // ⛔ **KEPT BY K-F1. The earlier instruction — "it must be DELETED by the
    // commit that lands the `pfnRenderCb` WDDM submission" — is SUPERSEDED**, for
    // the reason `knobs12::UMD12_ECL_DELAY_US` records in full: the submission has
    // landed and the question below is still open, because a submission that
    // exists is not evidence that dxgkrnl orders anything behind it
    // (`KMD_IMPACT.md` §14a.1 UV1). Both delay arms retire with UV1, not with the
    // callback. ⚠ This arm in particular gains value once the submission is in:
    // `FenceSignalForwarded` is still expected to be 0, so a reading here would
    // mean the runtime started entering a DDI it never entered before.
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
unsafe extern "system" fn wait_for_fence(
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
    table.pfnUpdateTileMappings = Some(tiles::update_tile_mappings);
    table.pfnCopyTileMappings = Some(tiles::copy_tile_mappings);
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
    /// `ID3D12Device::CreateCommandAllocator` failed at a class's first reset,
    /// so that pool has no backing allocator for the class. **Expected 0.**
    pool_allocator_engine_failed: RefusalCounter,
    /// ⛔ **RETIRED.** This counted a recorder class disagreeing with the single
    /// allocator formerly attached to a pool. Pools now own one allocator slot
    /// per list class and the list selects it at reset, so nothing increments
    /// this counter. It remains only to preserve the append-only evidence line.
    pool_type_mismatch: RefusalCounter,
    /// `pfnResetCommandPool` ran before any class allocator had been created.
    /// ⚠ May legitimately be non-zero: a pool created and reset before any
    /// recording is a no-op, not a fault.
    pool_reset_no_allocator: RefusalCounter,
    /// `ID3D12CommandAllocator::Reset` failed. ⚠ May legitimately be non-zero:
    /// D3D12 requires the GPU to be done with the allocator's lists first, and
    /// that is the application's obligation, not the driver's.
    pool_generation_reset: RefusalCounter,
    pool_generation_rotated: RefusalCounter,
    pool_generation_created: RefusalCounter,
    pool_generation_reused: RefusalCounter,
    pool_reset_error_unavailable: RefusalCounter,
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
    /// ⛔ **RETIRED BY S-4, and kept only so the `D3D12 DDI refusals:` line does not
    /// shift.** It counted the blanket `pfnCreateCommandSignature` → `E_NOTIMPL`;
    /// that blanket refusal is gone, replaced by four per-cause counters
    /// (`CommandSignatureStateTemplateRefused`, `…RaytracingRefused`,
    /// `…ArgTypeUnknown`, `…EngineFailed`) plus the success counter
    /// `CommandSignatureCreated`.
    ///
    /// ⛔ **Expected 0 forever now.** Nothing increments it. It is not deleted
    /// because removing an entry from [`REFUSALS`] shifts every counter after it,
    /// and that array is the evidence contract diffed across builds — the same rule
    /// that forces new counters to the end.
    command_signature_refused: RefusalCounter,
    /// ⛔ **RE-GRADED BY S-4.** It used to mean *"the runtime destroyed a signature
    /// after the create refused it"* and was expected to track
    /// `CommandSignatureRefused`. `pfnDestroyCommandSignature` now releases a real
    /// engine object, so that arm folded into the destroy's idempotent
    /// `Slot::release` and this counter is **no longer incremented at all**.
    ///
    /// ⛔ **Expected 0 forever.** Kept for the append-only reason above. ⚠ A destroy
    /// after a refused create is still legal and still silent — the slot was cleared
    /// by the create, so `release` finds null and does nothing.
    command_signature_destroy_unexpected: RefusalCounter,
    /// ⛔ **RETIRED.** Bundle lists now create normally and select a BUNDLE
    /// allocator from the bound pool at reset. Nothing increments this counter;
    /// it remains only to preserve the append-only evidence line.
    bundle_list_refused: RefusalCounter,
    /// `pfnExecuteCommandLists` was called with an unresolvable queue, a null
    /// array, or a count above `MAX_EXECUTE_COMMAND_LISTS`. **Expected 0.**
    execute_command_lists_bad_arg: RefusalCounter,
    /// An entry of an `pfnExecuteCommandLists` array carried no engine command
    /// list, and the whole submit was refused rather than partially forwarded.
    /// **Expected 0** — a list the runtime submits is a list this driver created.
    execute_command_lists_list_missing: RefusalCounter,
    /// ECL could not submit its required HE12 packet. Execution is cancelled; expected zero.
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
    /// Retired counter, retained at its diagnostic index; always zero in HE12 v2.
    fence_op_engine_failed: RefusalCounter,
    /// Retired counter, retained at its diagnostic index; always zero in HE12 v2.
    fence_wait_not_forwarded: RefusalCounter,
    /// Retired counter, retained at its diagnostic index; always zero in HE12 v2.
    fence_signal_forwarded: RefusalCounter,
    /// Retired counter, retained at its diagnostic index; always zero in HE12 v2.
    fence_wait_forwarded: RefusalCounter,
    /// Retained engine ECL committed to the worker FIFO; not proof of GPU completion.
    ecl_forwarded: RefusalCounter,
    ecl_admission_queued: RefusalCounter,
    ecl_admission_failed: RefusalCounter,
    ecl_worker_failed: RefusalCounter,
    ecl_exact_boundary: RefusalCounter,
    fence_native_refused: RefusalCounter,
    present_producer_admitted: RefusalCounter,
    /// `Umd12FenceSignalDelayUs` was non-zero and `pfnSignalFence` slept before
    /// returning. **Expected 0** on any run that did not deliberately set the
    /// knob; see `knobs12::UMD12_FENCE_SIGNAL_DELAY_US` for the question it is
    /// attached to. ⚠ That doc used to name "the commit that must delete it" —
    /// K-F1 was that commit and deliberately kept both delay arms; they retire
    /// with `KMD_IMPACT.md` §14a.1's UV1 instead.
    fence_signal_delayed: RefusalCounter,
    /// `Umd12EclDelayUs` was non-zero and `pfnExecuteCommandLists` slept before
    /// returning. **Expected 0** on any run that did not deliberately set the
    /// knob; see `knobs12::UMD12_ECL_DELAY_US`.
    ecl_delayed: RefusalCounter,
    /// ECL HE12 Render accepted by the runtime. EclAdmissionQueued counts the following event; neither proves runtime fence correctness.
    ecl_wddm_submitted: RefusalCounter,
    // ⚠⚠ THE SIX COUNTERS BELOW ARE SHARED BY BOTH `pfnRenderCb` USERS SINCE UP-9,
    // and their `Ecl` names are LEGACY. Every one of them is a fact about dxgkrnl's
    // callback table or about the windows on *this queue's context* — the same
    // context `pfnExecuteCommandLists` and `pfnPresent` both submit on — so a hit
    // means the same thing whichever DDI produced it, and the fix is the same. The
    // `submit_wddm_render` log line's `label` is what says which caller saw it. The
    // names were kept rather than corrected because renaming a counter changes every
    // `D3D12 DDI refusals:` line it appears in, and the widening is recorded here
    // instead. ⛔ Do NOT read any of them as ECL-specific.
    //
    /// `pKTCallbacks` was null when the WDDM submission needed it. **Expected 0** —
    /// `create_device` refuses a null `pKTCallbacks` before the device exists, so a
    /// hit means the table went away under a live device.
    ecl_submit_no_kt_callbacks: RefusalCounter,
    /// `pKTCallbacks->pfnRenderCb` was absent. **Expected 0, and a hit is a
    /// finding rather than a fault**: it would mean this adapter offers no legacy
    /// submission path, which invalidates the legacy-context decision taken in
    /// `create_command_queue` (module doc) and makes `PRESENT.md` §12 U12's
    /// alternative — a virtual context and `pfnSubmitCommandCb` — the only route.
    ecl_submit_render_cb_missing: RefusalCounter,
    /// The queue's `h_context` was null at submission time. **Expected 0**:
    /// `create_wddm_context` fails the queue create on a null `hContext`, so a live
    /// `QueueState` always has one.
    ecl_submit_no_context: RefusalCounter,
    /// The context carried **no command-buffer window**, so there was nowhere to
    /// record the submission.
    ///
    /// ⛔ **Expected 0, and this is the counter to read first if
    /// `EclWddmSubmitted` is 0**: a legacy `D3DDDICB_CREATECONTEXT` is supposed to
    /// return one, and the `CreateCommandQueue: CreateContext … cmd=…/…` capture
    /// line says what dxgkrnl actually handed this adapter. A non-zero reading
    /// would mean the whole legacy-context submission model is unavailable here,
    /// not that a frame was dropped.
    ecl_submit_no_command_window: RefusalCounter,
    /// The command window was **smaller than the payload**, so nothing was
    /// written. **Expected 0** — K-F1's payload is `HeliosD3D12SubmitCmd`, **16
    /// bytes** (`protocol/src/wddm.rs`, and `ecl_submit_command`'s doc has why 16
    /// is the *minimum* the KMD can recognise) — and a hit means dxgkrnl's window
    /// is smaller than that, which is a fact about the adapter worth having on its
    /// own line rather than folded into "no window".
    ///
    /// ⛔ **The "4 bytes" this doc used to claim was stale** and dated from the
    /// draft in which the record was a bare magic word; it survived the commit
    /// that made the payload 16.
    ecl_submit_window_too_small: RefusalCounter,
    /// `pfnRenderCb` returned a failure HRESULT for a packet this driver built.
    ///
    /// ⛔ **Expected 0, and a hit REMOVES the `ID3D12Device`** — it is raised to the
    /// runtime through `pfnSetErrorCb` unconditionally, because the ordering did not
    /// happen and will not happen for any later frame. `report_ecl_submit_error`
    /// carries the argument for the channel and the severity, and why the knob that
    /// briefly softened it was backed out (`docs/dx12/METHOD.md` §2 Phase 4).
    ///
    /// ⚠ Read it beside `QueueSetErrorUnavailable`: a non-zero count there means the
    /// failure could not even be reported, which is strictly worse than a removed
    /// device — a queue whose fence ordering silently did not happen.
    ecl_submit_render_failed: RefusalCounter,
    /// Retired counter, retained at its diagnostic index; always zero in HE12 v2.
    ecl_drain_failed: RefusalCounter,
    /// Retired counter, retained at its diagnostic index; always zero in HE12 v2.
    ecl_fence_sampled: RefusalCounter,
    /// Retired counter, retained at its diagnostic index; always zero in HE12 v2.
    ecl_fence_no_icd: RefusalCounter,
    /// Retired counter, retained at its diagnostic index; always zero in HE12 v2.
    ecl_fence_no_export: RefusalCounter,
    /// Retired counter, retained at its diagnostic index; always zero in HE12 v2.
    ecl_fence_refused: RefusalCounter,
    /// Retired counter, retained at its diagnostic index; always zero in HE12 v2.
    ecl_fence_disabled: RefusalCounter,
    /// Retired counter, retained at its diagnostic index; always zero in HE12 v2.
    ecl_fence_status_bad: RefusalCounter,
    /// Retired counter, retained at its diagnostic index; always zero in HE12 v2.
    ecl_drain_disabled: RefusalCounter,
    /// Retired counter, retained at its diagnostic index; always zero in HE12 v2.
    ecl_fence_no_drain: RefusalCounter,
    /// ⭐ **S-2's entry instrument: the runtime entered `pfnSignalFence`.** Counted as
    /// the function's first statement, above the null-argument check, so it counts
    /// *entries* and not successes.
    ///
    /// ⛔ **This is the number that settles *"`pfnSignalFence` is never called"***
    /// (`KMD_IMPACT.md` §14a.5, the 83rd session). That claim rested on
    /// `FenceSignalForwarded = 0` plus *"no trace line ever emitted"* — and the trace
    /// line is gated on `Umd12Trace`, **off by default**, so half the evidence did not
    /// exist on a default run. The other counters cannot substitute: `FenceOpBadArg`,
    /// `FenceOpFenceMissing` and `FenceOpEngineFailed` are shared between the two
    /// directions, so no arithmetic over them recovers a per-direction entry count.
    ///
    /// ⚠ `FenceSignalEntered > 0` with `FenceSignalForwarded == 0` is a completely
    /// different finding from both being 0, and before this counter the two were
    /// indistinguishable.
    fence_signal_entered: RefusalCounter,
    /// Entry to the native queue Wait DDI, before validation. Valid entries are explicitly refused with FenceNativeRefused.
    fence_wait_entered: RefusalCounter,
    /// Retired counter, retained at its diagnostic index; always zero in HE12 v2.
    fence_wait_runtime_owned: RefusalCounter,
    /// A real engine signature was created. Expected to move in indirect
    /// workloads. Aggregate create/forward counts do not prove each signature
    /// class executed; read them with class-specific readback tests and refusals.
    command_signature_created: RefusalCounter,
    /// A signature needs an unreported native tier or an engine translation
    /// returned E_NOTIMPL. Expected zero for supported root-state workloads;
    /// VBV/IBV on Venus remain a known contract gap. A hit is never success.
    command_signature_state_template_refused: RefusalCounter,
    /// A command signature named `DISPATCH_RAYS` and was refused `E_NOTIMPL`.
    ///
    /// ⛔ **Expected 0** while `caps12` reports no raytracing tier: no raytracing
    /// pipeline can exist for an indirect dispatch to reach, so a hit is a caps
    /// inconsistency elsewhere rather than a missing forward. ⚠ Its own counter
    /// because vkd3d *does* treat `DISPATCH_RAYS` as an action command — the refusal
    /// is this driver's caps talking, not the engine's capability.
    command_signature_raytracing_refused: RefusalCounter,
    /// A `D3D12DDI_INDIRECT_ARGUMENT_DESC::Type` was a value this build's
    /// `d3d12umddi.h` does not name, and the create was refused `E_INVALIDARG`.
    /// **Expected 0**; a hit means the header this build was generated from is older
    /// than the runtime asking.
    command_signature_arg_type_unknown: RefusalCounter,
    /// A command-signature slot could not reach the engine. **Expected 0** — it is a
    /// device-scope DDI and a device exists by construction.
    command_signature_no_device: RefusalCounter,
    /// Engine creation failed with an error other than E_NOTIMPL, or returned
    /// success without an object. Zero for valid workloads; expected E_INVALIDARG
    /// failures are counted by deliberate malformed-signature tests. OOM bumps
    /// the atomic without formatting; a later normal summary reads its value.
    command_signature_engine_failed: RefusalCounter,
    /// A signature without root arguments supplied a root signature. It is
    /// forwarded unchanged and rejected by the engine. Zero for valid inputs.
    command_signature_root_sig_unexpected: RefusalCounter,
    /// A named root handle has no engine object. Expected zero: a live root
    /// create stores an object or fails, and absence must not become a null root.
    command_signature_root_sig_unresolved: RefusalCounter,
    /// ⛔ Retired append-only telemetry slot. D3D12 Render callbacks in this driver
    /// carry metadata only and submit `NumAllocations = 0`; the D3D12 runtime owns
    /// residency and `pfnPresent` returns the source allocation separately. Kept in
    /// its historical position so refusal-summary ordering does not change.
    wddm_alloc_list_unavailable: RefusalCounter,
    /// ⭐ **UP-9's success counter: a present identity record went in.**
    ///
    /// ⛔ Its own counter rather than `EclWddmSubmitted`, deliberately — see
    /// [`submit_wddm_render`]'s doc.
    ///
    /// ⭐ **The arithmetic is the check, and `PresentEntered` (L8's set) is what makes
    /// it one**: `PresentEntered` must equal `PresentIdentitySubmitted` plus
    /// `PresentIdentityRefused` plus `PresentIdentityUnavailable` plus every L8
    /// refusal that returns before the submission. A `D3D12 DDI refusals:` line where
    /// that does not hold is reporting something other than what this code does. ⚠ It
    /// had no left-hand side until `PresentEntered` was added, which is exactly the
    /// state `EclForwarded` exists to prevent on the ECL path.
    present_identity_submitted: RefusalCounter,
    /// A present identity submission was attempted on a queue handle that did not
    /// resolve to a live `QueueState`. **Expected 0**: L8 resolves the same handle
    /// through [`present_context`] two statements earlier, so a hit means the queue
    /// was destroyed between them — a lifetime finding, not a missing feature.
    present_submit_no_queue: RefusalCounter,
    /// A present identity submission found no live device behind its queue.
    /// **Expected 0** — a live queue implies a live device — and its own counter
    /// rather than [`Self::present_submit_no_queue`]'s because the two would need
    /// different fixes.
    present_submit_no_device: RefusalCounter,
    /// Successfully committed native mapping operations; not GPU completion.
    tile_mappings_forwarded: RefusalCounter,
    /// Mapping HE12 Render and admission event both queued; not GPU completion.
    tile_mappings_admitted: RefusalCounter,
    /// Fallible allocation of the translated signature array failed. Expected
    /// zero normally; E_OUTOFMEMORY is returned with no object on exhaustion.
    /// Atomic-only on the error path; readable in the next normal summary.
    command_signature_translation_oom: RefusalCounter,
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
    pool_generation_reset: RefusalCounter::new("PoolGenerationReset"),
    pool_generation_rotated: RefusalCounter::new("PoolGenerationRotated"),
    pool_generation_created: RefusalCounter::new("PoolGenerationCreated"),
    pool_generation_reused: RefusalCounter::new("PoolGenerationReused"),
    pool_reset_error_unavailable: RefusalCounter::new("PoolResetErrorUnavailable"),
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
    ecl_admission_queued: RefusalCounter::new("EclAdmissionQueued"),
    ecl_admission_failed: RefusalCounter::new("EclAdmissionFailed"),
    ecl_worker_failed: RefusalCounter::new("EclWorkerFailed"),
    ecl_exact_boundary: RefusalCounter::new("EclExactBoundary"),
    fence_native_refused: RefusalCounter::new("FenceNativeRefused"),
    present_producer_admitted: RefusalCounter::new("PresentProducerAdmitted"),
    fence_signal_delayed: RefusalCounter::new("FenceSignalDelayed"),
    ecl_delayed: RefusalCounter::new("EclDelayed"),
    ecl_wddm_submitted: RefusalCounter::new("EclWddmSubmitted"),
    ecl_submit_no_kt_callbacks: RefusalCounter::new("EclSubmitNoKtCb"),
    ecl_submit_render_cb_missing: RefusalCounter::new("EclSubmitNoRenderCb"),
    ecl_submit_no_context: RefusalCounter::new("EclSubmitNoContext"),
    ecl_submit_no_command_window: RefusalCounter::new("EclSubmitNoCmdWindow"),
    ecl_submit_window_too_small: RefusalCounter::new("EclSubmitWindowSmall"),
    ecl_submit_render_failed: RefusalCounter::new("EclSubmitRenderFailed"),
    ecl_drain_failed: RefusalCounter::new("EclDrainFailed"),
    // ⛔ RETIRED 2026-09-13 with the wire-fence lever: these report 0 forever, kept
    // only because the exported refusal list is an ordered evidence contract and
    // renumbering it would break captures that predate the withdrawal. Delete the
    // whole block when the stream-edge gate lands and the list is re-cut anyway.
    ecl_fence_sampled: RefusalCounter::new("EclFenceSampled"),
    ecl_fence_no_icd: RefusalCounter::new("EclFenceNoIcd"),
    ecl_fence_no_export: RefusalCounter::new("EclFenceNoExport"),
    ecl_fence_refused: RefusalCounter::new("EclFenceRefused"),
    ecl_fence_disabled: RefusalCounter::new("EclFenceDisabled"),
    ecl_fence_status_bad: RefusalCounter::new("EclFenceStatusBad"),
    ecl_drain_disabled: RefusalCounter::new("EclDrainDisabled"),
    ecl_fence_no_drain: RefusalCounter::new("EclFenceNoDrain"),
    fence_signal_entered: RefusalCounter::new("FenceSignalEntered"),
    fence_wait_entered: RefusalCounter::new("FenceWaitEntered"),
    fence_wait_runtime_owned: RefusalCounter::new("FenceWaitRuntimeOwned"),
    command_signature_created: RefusalCounter::new("CommandSignatureCreated"),
    command_signature_state_template_refused: RefusalCounter::new(
        "CommandSignatureStateTemplateRefused",
    ),
    command_signature_raytracing_refused: RefusalCounter::new("CommandSignatureRaytracingRefused"),
    command_signature_arg_type_unknown: RefusalCounter::new("CommandSignatureArgTypeUnknown"),
    command_signature_no_device: RefusalCounter::new("CommandSignatureNoDevice"),
    command_signature_engine_failed: RefusalCounter::new("CommandSignatureEngineFailed"),
    command_signature_root_sig_unexpected: RefusalCounter::new("CommandSignatureRootSigUnexpected"),
    command_signature_root_sig_unresolved: RefusalCounter::new("CommandSignatureRootSigUnresolved"),
    wddm_alloc_list_unavailable: RefusalCounter::new("WddmAllocListUnavailable"),
    present_identity_submitted: RefusalCounter::new("PresentIdentitySubmitted"),
    present_submit_no_queue: RefusalCounter::new("PresentSubmitNoQueue"),
    present_submit_no_device: RefusalCounter::new("PresentSubmitNoDevice"),
    tile_mappings_forwarded: RefusalCounter::new("TileMappingsForwarded"),
    tile_mappings_admitted: RefusalCounter::new("TileMappingsAdmitted"),
    command_signature_translation_oom: RefusalCounter::new("CommandSignatureTranslationOom"),
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
    &L2_REFUSALS.pool_generation_reset,
    &L2_REFUSALS.pool_generation_rotated,
    &L2_REFUSALS.pool_generation_created,
    &L2_REFUSALS.pool_generation_reused,
    &L2_REFUSALS.pool_reset_error_unavailable,
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
    // ⛔ APPENDED, K-F1 (the `pfnRenderCb` WDDM submission). One success counter,
    // six per-cause refusals and the drain, at the end for the same reason as
    // every block above: `D3D12 DDI refusals:` lines are diffed across builds and
    // inserting shifts every counter after the insertion point.
    &L2_REFUSALS.ecl_wddm_submitted,
    &L2_REFUSALS.ecl_submit_no_kt_callbacks,
    &L2_REFUSALS.ecl_submit_render_cb_missing,
    &L2_REFUSALS.ecl_submit_no_context,
    &L2_REFUSALS.ecl_submit_no_command_window,
    &L2_REFUSALS.ecl_submit_window_too_small,
    &L2_REFUSALS.ecl_submit_render_failed,
    &L2_REFUSALS.ecl_drain_failed,
    // ⛔ APPENDED, the GPU-completion boundary commit. Six, and none of them may be
    // folded together: a zero fence is a LEGAL record value, so only the REASON is a
    // finding, and one shared counter would produce a number nobody can attribute.
    &L2_REFUSALS.ecl_fence_sampled,
    &L2_REFUSALS.ecl_fence_no_icd,
    &L2_REFUSALS.ecl_fence_no_export,
    &L2_REFUSALS.ecl_fence_refused,
    &L2_REFUSALS.ecl_fence_disabled,
    &L2_REFUSALS.ecl_fence_status_bad,
    // ⛔ APPENDED, A1's containment. Two: the arm that was taken, and the boundary
    // that arm cannot produce. Same append-only rule as every block above.
    &L2_REFUSALS.ecl_drain_disabled,
    &L2_REFUSALS.ecl_fence_no_drain,
    // ⛔ APPENDED, S-2. Two ENTRY counters (the only per-direction instrument for
    // "did the runtime enter this slot", which no arithmetic over the shared
    // `FenceOp*` counters can recover) and the benign half of the dropped-wait
    // split. `FenceWaitNotForwarded` keeps its position above and its NAME, and
    // only its meaning narrowed — moving it would shift eleven counters.
    &L2_REFUSALS.fence_signal_entered,
    &L2_REFUSALS.fence_wait_entered,
    &L2_REFUSALS.fence_wait_runtime_owned,
    // ⛔ APPENDED, S-4. One success counter and seven per-cause refusals for
    // `pfnCreateCommandSignature`. ⚠ `CommandSignatureRefused` and
    // `CommandSignatureDestroyUnexpected` keep their positions above and are now
    // **dead** — expected 0 forever — because removing an array entry shifts every
    // counter after it, and this array is diffed across builds. Their docs say so.
    &L2_REFUSALS.command_signature_created,
    &L2_REFUSALS.command_signature_state_template_refused,
    &L2_REFUSALS.command_signature_raytracing_refused,
    &L2_REFUSALS.command_signature_arg_type_unknown,
    &L2_REFUSALS.command_signature_no_device,
    &L2_REFUSALS.command_signature_engine_failed,
    &L2_REFUSALS.command_signature_root_sig_unexpected,
    &L2_REFUSALS.command_signature_root_sig_unresolved,
    // ⛔ APPENDED, UP-9 (the present identity `pfnRenderCb`). One shared-window
    // refusal and three present-scoped outcomes, at the end for the same reason as
    // every block above. ⚠ `EclWddmSubmitted` keeps its position and its name, and
    // only its SCOPE narrowed -- it now counts `pfnExecuteCommandLists` submissions
    // alone, because `submit_wddm_render` stopped bumping it when UP-9 became its
    // second caller. Its doc carries the re-grading.
    &L2_REFUSALS.wddm_alloc_list_unavailable,
    &L2_REFUSALS.present_identity_submitted,
    &L2_REFUSALS.present_submit_no_queue,
    &L2_REFUSALS.present_submit_no_device,
    &L2_REFUSALS.tile_mappings_forwarded,
    &L2_REFUSALS.tile_mappings_admitted,
    &L2_REFUSALS.ecl_admission_queued,
    &L2_REFUSALS.ecl_admission_failed,
    &L2_REFUSALS.ecl_worker_failed,
    &L2_REFUSALS.ecl_exact_boundary,
    &L2_REFUSALS.fence_native_refused,
    &L2_REFUSALS.present_producer_admitted,
    &L2_REFUSALS.command_signature_translation_oom,
];

// ⚠ `Hresult` is imported for the `E_*`/`S_OK` constants this file returns; the
// DDI's own `HRESULT` (bindgen's `c_long`) is the declared return type and the
// two are the same `i32` (`umd_common/src/hr.rs:31-34`).
const _: () = assert!(core::mem::size_of::<Hresult>() == core::mem::size_of::<ddi12::HRESULT>());
