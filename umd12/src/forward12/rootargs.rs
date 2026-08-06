//! L3b — root arguments, descriptor binding, clears.
//!
//! Owns 21 of `COMMAND_LIST_FUNCS_3D_0108`: root arguments 16, clears/discard 5.
//!
//! # ⭐ What this lane is, in one sentence
//!
//! Twenty of the twenty-one slots are **pure pass-throughs**, and that is the
//! spec's own word rather than this driver's convenience: `ResourceBinding.md`
//! says *"Parameters are directly passed through from API to DDI"* over the root
//! signature block (`:5096`), the descriptor-table block (`:5107`) and the root
//! constants block (`:5119`), *"the API passes parameters directly through to
//! the DDI"* over the root buffer-view block (`:5141`), and *"Parameters are
//! passed directly through from API to DDI"* over the whole clear/discard block
//! (`:5232`). So the work here is not translation — it is
//! **handle decode, per-arm validation, and naming every case this driver cannot
//! honour**. The twenty-first, [`clear_root_arguments`], has no API counterpart
//! at all and is refused with its reasoning written down.
//!
//! # ⛔ The fourteen `Set*Root*` slots are SEVEN operations, not fourteen
//!
//! They come in Compute/Graphics pairs that differ **only** in which engine
//! method they reach — `PFND3D12DDI_SET_ROOT_SIGNATURE`,
//! `_SET_ROOT_DESCRIPTOR_TABLE`, `_SET_ROOT_32BIT_CONSTANT`,
//! `_SET_ROOT_32BIT_CONSTANTS_0003` and `_SET_ROOT_BUFFER_VIEW` are each one
//! typedef used by two (or, for the buffer view, six) table members
//! (`d3d12umddi.rs:87117-87131`). Each operation therefore has **one body**,
//! named by [`Pipeline`] and [`RootView`], with thin `extern "C"` wrappers —
//! `queue.rs`'s `FenceOp` is the established shape. Fourteen near-identical
//! bodies is fourteen places for the validation to drift.
//!
//! # ⚠ `D3D12DDI_ROOT_CONSTANTS` — the `DX12.md` §4.3 row 4 hazard is NOT REAL
//!
//! That row, and `DDI_REFERENCE.md` §9.9 behind it, **used to** say the DDI
//! orders its three `UINT`s differently from `D3D12_ROOT_CONSTANTS` — *"the API
//! puts `Num32BitValues` first"* — so a cast would transpose them. **Checked
//! against four independent generators this session and it does not hold**:
//!
//! ```text
//! D3D12DDI_ROOT_CONSTANTS  (bindgen from d3d12umddi.h, d3d12umddi.rs:50350-50354)
//! D3D12_ROOT_CONSTANTS     (windows-rs 0.58 from Win32 metadata, Direct3D12/mod.rs:13817-13821)
//! D3D12_ROOT_CONSTANTS     (Microsoft d3d12.h, LookingGlass/vendor/directx/d3d12.h:1927-1931)
//! D3D12_ROOT_CONSTANTS     (vkd3d's IDL, include/vkd3d_d3d12.idl:1436-1441)
//!     -> all four: { UINT ShaderRegister; UINT RegisterSpace; UINT Num32BitValues; }
//! ```
//!
//! ⭐ **Both documents were already struck, on 2026-08-06 and independently of
//! this lane.** `DX12.md` §4.3 row 4 now reads *"The second hazard this row used
//! to claim was FALSE and is struck"*, and `DDI_REFERENCE.md` §9.9 strikes the
//! paragraph outright and follows it with the two SDK headers side by side plus
//! the windows-rs cross-check. That correction block names **this module doc**
//! as the propagation path it was written to close — so this section is the
//! record of a check that agrees with the live documents, not an open action
//! against them. ⛔ Do not "fix" either document again.
//!
//! ⚠ The struct does not appear in this lane at all — it is a **root-signature
//! creation** shape and belongs to L6, which reached the same conclusion
//! independently and recorded it in `pso.rs`'s `root_signature_to_1_0`, in the
//! `D3D12DDI_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS` arm — the comment beginning
//! *"`DDI_REFERENCE.md` §9.9 warns that `D3D12DDI_ROOT_CONSTANTS` is not
//! field-order-compatible"* — copying by field name so the code is correct
//! either way.
//!
//! ⛔ Cited by function and by the comment's opening words rather than by line
//! span **on purpose**, and this citation is why the rule exists: it used to read
//! `pso.rs:1079-1090`, and the integrator's hand-merge of two lanes' accessors
//! into `pso.rs` pushed the block down ~84 lines, so the span came to name an
//! unrelated bounds check. That matters more than a usual stale line — this is
//! the independent second check the `463154f` correction rests on, so a reader
//! who follows it and finds nothing reads the corroboration as fabricated.
//! ⚠ The raw §10 finding proposed re-citing it against `create_root_signature`;
//! that is the wrong function — the arm is inside `root_signature_to_1_0`, which
//! `create_root_signature` calls.
//!
//! ⛔ The hazard that IS real in the same `DX12.md` row — the descriptor-heap
//! flags colliding on `0x1` — is L5's and is pinned there
//! (`descriptors.rs`'s `DDI_HEAP_FLAG_CPU_VISIBLE` assertion).
//!
//! # ⚠ The spec's `pfnSetDescriptorHeaps` prototype has its arguments SWAPPED
//!
//! `ResourceBinding.md:4440-4443` declares
//! `(D3D12DDI_HCOMMANDLIST, D3D12DDI_HDESCRIPTORHEAP* pDescriptorHeaps, UINT
//! NumDescriptorHeaps)` — pointer first. The **shipping header** is
//! `(D3D12DDI_HCOMMANDLIST, UINT NumDescriptorHeaps,
//! D3D12DDI_HDESCRIPTORHEAP*)` — count first (`d3d12umddi.rs:51951-51957`). The
//! header wins, and it is the header this file is typed against, so the
//! compiler settles it; the note exists because a reader who goes to the spec
//! for the argument order will get it backwards. Same class as the 74th
//! memory's *"spec DDI blocks are not shipping shapes"*.
//!
//! # ⛔ The three-way resource classification, inherited from L5
//!
//! Three of the five clear/discard slots carry a `D3D12DDI_HRESOURCE`, and
//! `descriptors.rs` learned the hard way that a **null** handle and an
//! **unresolvable** handle must not flatten into the same `None`: the two mean
//! different things and want different answers. [`ClearResource`] keeps the
//! three apart. ⚠ Where this lane differs from L5: for a *view creation* a null
//! resource is D3D12's legal null-descriptor form, while for these three the API
//! declares `pResource` required and vkd3d's `DiscardResource` dereferences it
//! without a null check — so a null here is counted and dropped rather than
//! forwarded, and [`L3B_REFUSALS.clear_resource_null`] says so.
//!
//! # ⭐⭐ Every failure in this lane is reported at COMMAND-LIST scope
//!
//! ⛔ **Not `pfnSetErrorCb`.** This lane's 21 slots are all `VOID`-returning
//! recording DDIs, and the S6 Round 2 spine claimed the device-scoped
//! `pfnSetErrorCb` was therefore the only error channel they had. That was
//! **false**: `pfnSetCommandListErrorCb` sits one field below it in
//! `D3D12DDI_CORELAYER_DEVICECALLBACKS_0062`, the same struct this driver
//! already reads `pfnSetCommandListDDITableCb` out of. The runtime's answer to
//! it is *"the runtime will drop all calls into the driver which record commands
//! on the specified command list"* — one list quarantined, the application told
//! at `Close()`, which is D3D12's own recording-error contract — where
//! `pfnSetErrorCb` removes the whole `ID3D12Device`, and the compositor with it
//! when the device is DWM's. [`report_error`] is this lane's one door to it and
//! `device12::set_command_list_error` is behind it.

use helios_umd_common::hr::{Hresult, E_INVALIDARG};
use helios_umd_common::refusals::RefusalCounter;
use helios_umd_common::throttle::LogThrottle;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D12::{
    ID3D12DescriptorHeap, ID3D12Resource, ID3D12RootSignature, D3D12_CLEAR_FLAGS,
    D3D12_CLEAR_FLAG_DEPTH, D3D12_CLEAR_FLAG_STENCIL, D3D12_DISCARD_REGION,
};

use super::descriptors::{api_cpu_handle, api_gpu_handle, engine_heap};
use super::pso;
use super::queue::{self, CommandListState};
use super::resource12;
use super::tables12::{stage, CommandListTable, Filling};
use crate::{ddi12, device12, log_error, note_refusal, trace_line};

// ---------------------------------------------------------------------------
// Log budgets
// ---------------------------------------------------------------------------

/// Budget for this lane's error lines: the first 8, then every 4096th.
///
/// ⛔ Same discipline and the same scar as `queue.rs`'s six budgets and
/// `cmdlist.rs`'s one: `log_error!` is unbounded by construction
/// (`umd_common/src/log.rs:279`) and T2 measured one unbounded UMD log site at
/// ~9k mutex-serialized writes per second. Every slot in this file is on a
/// **per-draw** path, so an unbudgeted line here is not a per-frame writer, it
/// is a per-draw one.
static ERROR_LOG: LogThrottle = LogThrottle::new();

/// A budget of its own for [`clear_root_arguments`], whose line is **expected**
/// rather than exceptional.
///
/// ⚠ Separate from [`ERROR_LOG`] deliberately: that slot fires once per
/// command-list create and once per `pfnResetCommandList`, so sharing one budget
/// would let the expected traffic consume the whole allowance and hide the
/// first occurrence of a real error behind it.
static CLEAR_ROOT_ARGS_LOG: LogThrottle = LogThrottle::new();

/// Returns the occurrence ordinal (0-based) when the line should be emitted.
fn budget(throttle: &LogThrottle) -> Option<usize> {
    throttle.first_n_then_every(8, 4096)
}

// ---------------------------------------------------------------------------
// Error reporting for the VOID slots
// ---------------------------------------------------------------------------

/// Report a **command-list**-scope failure from a recording slot, counting the
/// two cases where there is no way to hear it.
///
/// ⭐ **The channel is `pfnSetCommandListErrorCb`, not `pfnSetErrorCb`** — see
/// `device12::set_command_list_error`, which carries the whole argument. The
/// runtime quarantines the one list and the application learns at `Close()`;
/// nothing else on the device is touched.
///
/// ⚠ It takes `&CommandListState` rather than a bare handle because the callback
/// needs **two** things `queue::CommandListState` carries: `h_device()` to reach
/// the device's callback table, and `h_rt_list()` — the *runtime's* handle for
/// this list — to name what to quarantine. Passing them separately would let a
/// call site pair one list's device with another list's handle.
///
/// ⚠ Its counters are **this lane's**, not `device12`'s, because
/// `PARALLEL.md` §9.1 puts a lane's counters in the lane's file.
/// `cmdlist.rs::report_error` and `copy.rs`'s equivalent are the same function
/// against those lanes' sets.
///
/// ⛔ **The HRESULT is narrowed for you.** `set_command_list_error` runs it
/// through `device12::command_list_error_code`, because the callback accepts
/// exactly three values; call sites pass the code that describes the failure
/// (`E_INVALIDARG` throughout this lane, i.e. *the application recorded
/// something this driver cannot honour*) and do not narrow it themselves.
///
/// ⭐ **What is reported and what is only counted.** Reporting costs the list
/// this call is recording into, so it is used wherever continuing would leave
/// the application rendering from state this driver knows is wrong and could not
/// tell it about: an unresolvable root signature, descriptor heap or resource, a
/// runtime array this driver cannot read, and a null resource on a slot whose
/// API counterpart requires one. What stays **counted only** is the traffic this
/// driver *forwards* correctly — a null root signature, a zero GPU address, a
/// null descriptor-heap entry — plus [`clear_root_arguments`], which is refused
/// on every list create and every `pfnResetCommandList`; reporting there would
/// quarantine every list in the process.
///
/// # Safety
/// `state` must be borrowed from a live `queue::CommandListState`, i.e. one
/// [`list_state`] returned for a handle `pfnDestroyCommandList` has not been
/// called on, whose `h_device()` names a device `device12::create_device`
/// returned `S_OK` for and `pfnDestroyDevice` has not torn down.
unsafe fn report_error(state: &CommandListState, hr: Hresult) {
    // SAFETY: the caller guarantees `state` is live, so `h_device()` is the
    // handle `create_command_list` recorded for a live device; the borrow does
    // not outlive this call.
    let Some(dev) = (unsafe { device12::device(state.h_device()) }) else {
        note_refusal(&L3B_REFUSALS.set_error_no_device);
        return;
    };
    if !device12::set_command_list_error(dev, state.h_rt_list(), hr) {
        note_refusal(&L3B_REFUSALS.set_error_cb_absent);
    }
}

/// Resolve a command-list handle to its state, counting the one failure.
///
/// ⚠ Deliberately **not** reported: with no state there is neither an
/// `h_device` to reach the callback table through nor an `h_rt_list` to name the
/// list to quarantine, so this is the one failure this table cannot escalate —
/// and the list-scope channel makes that *more* true than the device-scope one
/// did, because it needs both. `cmdlist.rs` records the same.
///
/// # Safety
/// `h_list` must be a handle `queue::create_command_list` returned `S_OK` for
/// and which `pfnDestroyCommandList` has not been called on.
unsafe fn list_state<'a>(h_list: ddi12::D3D12DDI_HCOMMANDLIST) -> Option<&'a CommandListState> {
    // SAFETY: forwarded; the caller carries `command_list_state`'s precondition.
    let state = unsafe { queue::command_list_state(h_list) };
    if state.is_none() {
        note_refusal(&L3B_REFUSALS.command_list_missing);
    }
    state
}

// ---------------------------------------------------------------------------
// ⭐ The rect array — one C type described twice, PINNED rather than assumed
// ---------------------------------------------------------------------------
//
// Four of the five clear/discard slots take `(UINT NumRects, CONST
// D3D12DDI_RECT* pRects)` and the API takes `&[RECT]`. `D3D12DDI_RECT` is a
// typedef of the WDK's `tagRECT` (`d3d12umddi.rs:487-502`, four `LONG`s) and
// windows-rs's `RECT` is four `i32`s (`Win32/Foundation/mod.rs:10978-10983`);
// `LONG` is `c_long`, which is 32-bit on `x86_64-pc-windows-msvc`.
//
// ⛔ This is NOT `ARCHITECTURE.md` §12 rule 1's prohibition on hand-transcribing
// an ABI — both are machine-generated from their own authority, and the
// assertions below check that the two generators agree on size, alignment and
// every field offset, which for a `#[repr(C)]` struct of four scalars is the
// whole of its layout. The alternative, copying each rect into a fresh `Vec` on
// a path an application drives every frame, is an allocation per clear and buys
// no additional proof. Same argument, and the same shape, as
// `descriptors.rs`'s CPU-handle array cast.
const _: () = assert!(core::mem::size_of::<ddi12::D3D12DDI_RECT>() == core::mem::size_of::<RECT>());
const _: () =
    assert!(core::mem::align_of::<ddi12::D3D12DDI_RECT>() == core::mem::align_of::<RECT>());
const _: () =
    assert!(core::mem::offset_of!(ddi12::D3D12DDI_RECT, left) == core::mem::offset_of!(RECT, left));
const _: () =
    assert!(core::mem::offset_of!(ddi12::D3D12DDI_RECT, top) == core::mem::offset_of!(RECT, top));
const _: () = assert!(
    core::mem::offset_of!(ddi12::D3D12DDI_RECT, right) == core::mem::offset_of!(RECT, right)
);
const _: () = assert!(
    core::mem::offset_of!(ddi12::D3D12DDI_RECT, bottom) == core::mem::offset_of!(RECT, bottom)
);
// ⚠ And the offsets above are only meaningful because the fields are not all at
// 0: pinned so a future struct that collapsed to one field could not pass.
const _: () = assert!(core::mem::offset_of!(RECT, bottom) == 12);

/// Borrow the runtime's rect array as the API's, or `None` when the arm is
/// invalid.
///
/// ⛔ Validated **per arm, never per max-union**: a zero count with a null
/// pointer is the legal "whole view / whole subresource" form that every one of
/// these slots accepts (`_In_reads_opt_(NumRects)` in the spec's own
/// prototypes), and a non-zero count with a null pointer never is.
///
/// ⚠ The zero case returns an empty slice rather than building one from the
/// pointer: `slice::from_raw_parts` requires a non-null aligned pointer even for
/// a zero length, so a `(0, NULL)` call would be UB if it went through the
/// general path.
///
/// # Safety
/// `p`, when non-null, must address at least `num` `D3D12DDI_RECT`s for the
/// duration of the call, and the returned borrow must not outlive it.
unsafe fn api_rects<'a>(num: ddi12::UINT, p: *const ddi12::D3D12DDI_RECT) -> Option<&'a [RECT]> {
    if num == 0 {
        return Some(&[]);
    }
    if p.is_null() {
        return None;
    }
    // SAFETY: non-null per the check, and the caller guarantees `num` live
    // elements. The cast reinterprets an array of one C type as an array of the
    // same C type, which the assertions above establish.
    Some(unsafe { core::slice::from_raw_parts(p.cast::<RECT>(), num as usize) })
}

/// Borrow a four-`FLOAT` clear colour, or `None` when the pointer is null.
///
/// # Safety
/// `p`, when non-null, must address four `FLOAT`s for the duration of the call.
unsafe fn api_color_f32<'a>(p: *const ddi12::FLOAT) -> Option<&'a [f32; 4]> {
    if p.is_null() {
        return None;
    }
    // SAFETY: non-null per the check; `ddi12::FLOAT` is `f32`, so the target
    // type is four contiguous `f32`s at the same alignment.
    Some(unsafe { &*p.cast::<[f32; 4]>() })
}

/// Borrow a four-`UINT` clear value, or `None` when the pointer is null.
///
/// # Safety
/// `p`, when non-null, must address four `UINT`s for the duration of the call.
unsafe fn api_color_u32<'a>(p: *const ddi12::UINT) -> Option<&'a [u32; 4]> {
    if p.is_null() {
        return None;
    }
    // SAFETY: non-null per the check; `ddi12::UINT` is `c_uint`, which is `u32`
    // on this target, so the target type is four contiguous `u32`s at the same
    // alignment.
    Some(unsafe { &*p.cast::<[u32; 4]>() })
}

// ---------------------------------------------------------------------------
// The two axes the fourteen root-argument slots vary along
// ---------------------------------------------------------------------------

/// Which of the two root-argument pipelines a slot addresses.
///
/// ⭐ D3D12 keeps **two independent sets of root arguments** per command list,
/// one for graphics and one for compute — vkd3d models them as
/// `list->graphics_bindings` and `list->compute_bindings`
/// (`vkd3d-proton-helios/libs/vkd3d/command.c:14043`, `:14058`) — so this is not
/// a cosmetic pairing. Every operation below is written once and named by this.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pipeline {
    Compute,
    Graphics,
}

impl Pipeline {
    fn name(self) -> &'static str {
        match self {
            Pipeline::Compute => "Compute",
            Pipeline::Graphics => "Graphics",
        }
    }
}

/// Which kind of root descriptor a `PFND3D12DDI_SET_ROOT_BUFFER_VIEW` slot sets.
///
/// ⚠ One DDI typedef backs **six** table members (`d3d12umddi.rs:87126-87131`),
/// so this second axis is what keeps the six from being six copies of one body.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RootView {
    ConstantBuffer,
    ShaderResource,
    UnorderedAccess,
}

impl RootView {
    fn name(self) -> &'static str {
        match self {
            RootView::ConstantBuffer => "ConstantBufferView",
            RootView::ShaderResource => "ShaderResourceView",
            RootView::UnorderedAccess => "UnorderedAccessView",
        }
    }
}

// ---------------------------------------------------------------------------
// Root signature — 2 slots
// ---------------------------------------------------------------------------

/// The body behind `pfnSetComputeRootSignature` and
/// `pfnSetGraphicsRootSignature`.
///
/// ⚠ **A null `hDrvRootSignature` is forwarded as a null root signature, not
/// refused.** `ID3D12GraphicsCommandList::Set{Graphics,Compute}RootSignature`
/// accepts `nullptr` and vkd3d implements it — `set_root_signature` stores
/// `NULL` into the bindings and invalidates every root parameter
/// (`command.c:14021-14034`) — so this is an unbind, and it is counted as an
/// instrument rather than reported.
///
/// ⛔ And counted with `bump`, **not** `note_refusal`: `note_refusal` prints the
/// whole `D3D12 DDI refusals:` set at `log_error!` level on a counter's first
/// hit, and the established triage step is to grep `umd12-<pid>.log` for that
/// line — so a legal unbind would write a refusal record into a frame on which
/// nothing was refused. Same rule, and the same words, as `cmdlist.rs`'s
/// triangle-fan arm: *this arm FORWARDS*. ⚠ It applies here and **not** to this
/// lane's other three forward-and-count arms
/// ([`L3B_REFUSALS.root_table_zero_base`],
/// [`L3B_REFUSALS.descriptor_heap_null_entry`],
/// [`L3B_REFUSALS.root_view_null_address`]): those are graded Expected 0, so a
/// hit *is* a finding and dumping the set is what the dump is for. This one is
/// explicitly not graded.
///
/// ⛔ The counter is **not** graded "expected 0", and the reason it must not be
/// is worth stating here: the DDI prototype marks the parameter `_In_`
/// (`ResourceBinding.md:5100-5102`), but that block puts the identical
/// annotation on by-value scalars that have no null form at all — `_In_ UINT
/// RootParameterIndex` (`:5113`), `_In_ D3D12DDI_GPU_DESCRIPTOR_HANDLE
/// BaseDescriptor` (`:5114`), `_In_ D3D12DDI_GPU_VIRTUAL_ADDRESS
/// BufferLocation` (`:5148`). It is annotation noise, not a nullability claim.
/// See [`L3B_REFUSALS.root_signature_null`] for how the counter reads.
///
/// ⭐ A **redundant** set is free rather than merely harmless: vkd3d's
/// `d3d12_command_list_set_root_signature` early-returns when the incoming
/// signature equals the bound one (`command.c:14024-14025`), so this driver does
/// not need — and must not keep — a shadow copy to elide it. That is the same
/// "hold no state the engine also holds" rule `queue::CommandListState`'s doc
/// states.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`; `h_rs`,
/// when its `pDrvPrivate` is non-null, must be a handle `pso::create_root_signature`
/// returned `S_OK` for and which has not been destroyed.
unsafe fn set_root_signature(
    which: Pipeline,
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_rs: ddi12::D3D12DDI_HROOTSIGNATURE,
) {
    // SAFETY: the caller guarantees a live command-list handle.
    let Some(state) = (unsafe { list_state(h_list) }) else {
        return;
    };

    // ⛔ The null case is classified BEFORE `pso::root_signature` can flatten it
    // into the same `None` an unresolvable handle produces. See that accessor's
    // doc, and `descriptors.rs`'s `view_resource` for the scar.
    let bound = if h_rs.pDrvPrivate.is_null() {
        // ⛔ `bump`, not `note_refusal`: this arm FORWARDS a legal unbind, and
        // `note_refusal` would print the whole `D3D12 DDI refusals:` set on its
        // first hit. See the doc above.
        L3B_REFUSALS.root_signature_null.bump();
        None
    } else {
        // SAFETY: non-null per the check; the caller guarantees the handle came
        // from `pso::create_root_signature`, and the borrow ends with this call.
        match unsafe { pso::root_signature(h_rs) } {
            Some(rs) => Some(rs),
            None => {
                note_refusal(&L3B_REFUSALS.root_signature_missing);
                if let Some(n) = budget(&ERROR_LOG) {
                    log_error!(
                        "Set{}RootSignature: the runtime named root signature {:p} and this \
                         driver could not resolve it -- every subsequent root argument on this \
                         pipeline would bind against the wrong layout (x{})",
                        which.name(),
                        h_rs.pDrvPrivate,
                        n + 1,
                    );
                }
                // SAFETY: `state` is the live state `list_state` just returned
                // for this list, which is `report_error`'s whole precondition.
                unsafe { report_error(state, E_INVALIDARG) };
                return;
            }
        }
    };

    trace_line!(
        "Set{}RootSignature: list={:p} rs={:p}",
        which.name(),
        h_list.pDrvPrivate,
        h_rs.pDrvPrivate,
    );

    // SAFETY: `engine()` borrows the list this box owns; `bound` borrows the
    // slot's reference and outlives the call; `None` is the API's own spelling
    // of an unbound root signature.
    unsafe {
        let param: Option<&ID3D12RootSignature> = bound.as_deref();
        match which {
            Pipeline::Compute => state.engine().SetComputeRootSignature(param),
            Pipeline::Graphics => state.engine().SetGraphicsRootSignature(param),
        }
    }
}

/// `pfnSetComputeRootSignature` -> `ID3D12GraphicsCommandList::SetComputeRootSignature`.
///
/// # Safety
/// As [`set_root_signature`].
unsafe extern "C" fn set_compute_root_signature(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_rs: ddi12::D3D12DDI_HROOTSIGNATURE,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is the body's.
    unsafe { set_root_signature(Pipeline::Compute, h_list, h_rs) }
}

/// `pfnSetGraphicsRootSignature` -> `ID3D12GraphicsCommandList::SetGraphicsRootSignature`.
///
/// # Safety
/// As [`set_root_signature`].
unsafe extern "C" fn set_graphics_root_signature(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_rs: ddi12::D3D12DDI_HROOTSIGNATURE,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is the body's.
    unsafe { set_root_signature(Pipeline::Graphics, h_list, h_rs) }
}

// ---------------------------------------------------------------------------
// Root descriptor tables — 2 slots
// ---------------------------------------------------------------------------

/// The body behind `pfnSetComputeRootDescriptorTable` and
/// `pfnSetGraphicsRootDescriptorTable`.
///
/// ⭐ **The GPU handle crosses verbatim, and that is the whole of L5's H3
/// decision arriving here.** `descriptors.rs` keeps no shadow descriptor table:
/// it returns vkd3d's own `gpu_va` from `pfnGetGPUDescriptorHandleForHeapStart`
/// and vkd3d's own increment from `pfnGetDescriptorSizeInBytes`, so the
/// `base + i * stride` the runtime computed before calling this slot is already
/// an address vkd3d can decode. [`api_gpu_handle`] is a type change and nothing
/// else, and no arithmetic on a GPU handle happens anywhere in this driver.
///
/// ⛔ **A zero base descriptor is forwarded and counted**, and it is the
/// downstream half of `descriptors.rs`'s `DescriptorHeapGpuHandleZero`: vkd3d
/// assigns `gpu_va` only for shader-visible heaps
/// (`libs/vkd3d/resource.c:10159`), so a table bound at address 0 is a table in
/// a heap vkd3d never gave an address to. Refusing it would silently drop a bind
/// the application needs; forwarding it lets the engine be the judge and leaves
/// the counter as the evidence. Read [`L3B_REFUSALS.root_table_zero_base`]
/// beside L5's counter — one moving without the other means the zero came from
/// somewhere else.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`, and
/// `base` must be a GPU descriptor handle this driver minted, i.e. one derived
/// from a `pfnGetGPUDescriptorHandleForHeapStart` answer.
unsafe fn set_root_descriptor_table(
    which: Pipeline,
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    root_parameter_index: ddi12::UINT,
    base: ddi12::D3D12DDI_GPU_DESCRIPTOR_HANDLE,
) {
    // SAFETY: the caller guarantees a live command-list handle.
    let Some(state) = (unsafe { list_state(h_list) }) else {
        return;
    };
    if base.ptr == 0 {
        note_refusal(&L3B_REFUSALS.root_table_zero_base);
    }
    trace_line!(
        "Set{}RootDescriptorTable: idx={} base={:#018x}",
        which.name(),
        root_parameter_index,
        base.ptr,
    );
    let api_base = api_gpu_handle(base);
    // SAFETY: `engine()` borrows the list this box owns; both arguments cross by
    // value and the handle is converted field by field.
    unsafe {
        match which {
            Pipeline::Compute => state
                .engine()
                .SetComputeRootDescriptorTable(root_parameter_index, api_base),
            Pipeline::Graphics => state
                .engine()
                .SetGraphicsRootDescriptorTable(root_parameter_index, api_base),
        }
    }
}

/// `pfnSetComputeRootDescriptorTable` -> the engine's compute form.
///
/// # Safety
/// As [`set_root_descriptor_table`].
unsafe extern "C" fn set_compute_root_descriptor_table(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    root_parameter_index: ddi12::UINT,
    base: ddi12::D3D12DDI_GPU_DESCRIPTOR_HANDLE,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is the body's.
    unsafe { set_root_descriptor_table(Pipeline::Compute, h_list, root_parameter_index, base) }
}

/// `pfnSetGraphicsRootDescriptorTable` -> the engine's graphics form.
///
/// # Safety
/// As [`set_root_descriptor_table`].
unsafe extern "C" fn set_graphics_root_descriptor_table(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    root_parameter_index: ddi12::UINT,
    base: ddi12::D3D12DDI_GPU_DESCRIPTOR_HANDLE,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is the body's.
    unsafe { set_root_descriptor_table(Pipeline::Graphics, h_list, root_parameter_index, base) }
}

// ---------------------------------------------------------------------------
// Root 32-bit constants — 4 slots
// ---------------------------------------------------------------------------

/// The body behind `pfnSetComputeRoot32BitConstant` and
/// `pfnSetGraphicsRoot32BitConstant`.
///
/// ⚠ Three scalars in, three scalars out, no pointer and nothing to validate —
/// which is why this one has no refusal arm of its own beyond the shared
/// command-list lookup. Stated rather than left as an absence: an empty
/// validation block on a DDI that takes runtime data is normally a defect.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`.
unsafe fn set_root_32bit_constant(
    which: Pipeline,
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    root_parameter_index: ddi12::UINT,
    src_data: ddi12::UINT,
    dest_offset_in_32bit_values: ddi12::UINT,
) {
    // SAFETY: the caller guarantees a live command-list handle.
    let Some(state) = (unsafe { list_state(h_list) }) else {
        return;
    };
    trace_line!(
        "Set{}Root32BitConstant: idx={} off={}",
        which.name(),
        root_parameter_index,
        dest_offset_in_32bit_values,
    );
    // SAFETY: `engine()` borrows the list this box owns; every argument is a
    // by-value `UINT`.
    unsafe {
        match which {
            Pipeline::Compute => state.engine().SetComputeRoot32BitConstant(
                root_parameter_index,
                src_data,
                dest_offset_in_32bit_values,
            ),
            Pipeline::Graphics => state.engine().SetGraphicsRoot32BitConstant(
                root_parameter_index,
                src_data,
                dest_offset_in_32bit_values,
            ),
        }
    }
}

/// `pfnSetComputeRoot32BitConstant` -> the engine's compute form.
///
/// # Safety
/// As [`set_root_32bit_constant`].
unsafe extern "C" fn set_compute_root_32bit_constant(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    root_parameter_index: ddi12::UINT,
    src_data: ddi12::UINT,
    dest_offset_in_32bit_values: ddi12::UINT,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is the body's.
    unsafe {
        set_root_32bit_constant(
            Pipeline::Compute,
            h_list,
            root_parameter_index,
            src_data,
            dest_offset_in_32bit_values,
        )
    }
}

/// `pfnSetGraphicsRoot32BitConstant` -> the engine's graphics form.
///
/// # Safety
/// As [`set_root_32bit_constant`].
unsafe extern "C" fn set_graphics_root_32bit_constant(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    root_parameter_index: ddi12::UINT,
    src_data: ddi12::UINT,
    dest_offset_in_32bit_values: ddi12::UINT,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is the body's.
    unsafe {
        set_root_32bit_constant(
            Pipeline::Graphics,
            h_list,
            root_parameter_index,
            src_data,
            dest_offset_in_32bit_values,
        )
    }
}

/// The body behind `pfnSetComputeRoot32BitConstants` and
/// `pfnSetGraphicsRoot32BitConstants`.
///
/// ⛔ The one runtime-supplied buffer in the fourteen root-argument slots, and
/// it is validated **per arm**: a zero count with a null pointer is a legal
/// no-op, a non-zero count with a null pointer never is. The engine reads
/// `Num32BitValuesToSet * 4` bytes through this pointer, so an unchecked null is
/// a fault inside vkd3d with this driver's frame on the stack.
///
/// ⚠ The bytes are **not** copied. The API's
/// `Set{Compute,Graphics}Root32BitConstants` takes the same
/// `const void*`, the runtime owns the buffer for the duration of the DDI call,
/// and vkd3d memcpys it into its own root-argument shadow before returning
/// (`d3d12_command_list_set_root_constants`'s `memcpy`,
/// `libs/vkd3d/command.c:14239`) — so
/// borrowing is correct and a copy here would be a per-draw allocation.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`, and
/// `p_src_data`, when `num_32bit_values_to_set` is non-zero, must address at
/// least that many 32-bit values for the duration of the call.
unsafe fn set_root_32bit_constants(
    which: Pipeline,
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    root_parameter_index: ddi12::UINT,
    num_32bit_values_to_set: ddi12::UINT,
    p_src_data: *const core::ffi::c_void,
    dest_offset_in_32bit_values: ddi12::UINT,
) {
    // SAFETY: the caller guarantees a live command-list handle.
    let Some(state) = (unsafe { list_state(h_list) }) else {
        return;
    };
    if num_32bit_values_to_set != 0 && p_src_data.is_null() {
        note_refusal(&L3B_REFUSALS.root_constants_bad_arg);
        if let Some(n) = budget(&ERROR_LOG) {
            log_error!(
                "Set{}Root32BitConstants: idx={} asks for {} value(s) from a NULL pSrcData \
                 (x{})",
                which.name(),
                root_parameter_index,
                num_32bit_values_to_set,
                n + 1,
            );
        }
        // SAFETY: `state` is this list's live state, as `set_root_signature`.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    }
    if num_32bit_values_to_set == 0 {
        // Nothing to write. Not an error and not counted: the runtime is
        // allowed to ask for an empty update, and the engine would treat it the
        // same way.
        return;
    }
    trace_line!(
        "Set{}Root32BitConstants: idx={} n={} off={}",
        which.name(),
        root_parameter_index,
        num_32bit_values_to_set,
        dest_offset_in_32bit_values,
    );
    // SAFETY: `engine()` borrows the list this box owns; the caller guarantees
    // the buffer holds `num_32bit_values_to_set` values, which is non-zero and
    // paired with a non-null pointer by the checks above.
    unsafe {
        match which {
            Pipeline::Compute => state.engine().SetComputeRoot32BitConstants(
                root_parameter_index,
                num_32bit_values_to_set,
                p_src_data,
                dest_offset_in_32bit_values,
            ),
            Pipeline::Graphics => state.engine().SetGraphicsRoot32BitConstants(
                root_parameter_index,
                num_32bit_values_to_set,
                p_src_data,
                dest_offset_in_32bit_values,
            ),
        }
    }
}

/// `pfnSetComputeRoot32BitConstants` -> the engine's compute form.
///
/// # Safety
/// As [`set_root_32bit_constants`].
unsafe extern "C" fn set_compute_root_32bit_constants(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    root_parameter_index: ddi12::UINT,
    num_32bit_values_to_set: ddi12::UINT,
    p_src_data: *const core::ffi::c_void,
    dest_offset_in_32bit_values: ddi12::UINT,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is the body's.
    unsafe {
        set_root_32bit_constants(
            Pipeline::Compute,
            h_list,
            root_parameter_index,
            num_32bit_values_to_set,
            p_src_data,
            dest_offset_in_32bit_values,
        )
    }
}

/// `pfnSetGraphicsRoot32BitConstants` -> the engine's graphics form.
///
/// # Safety
/// As [`set_root_32bit_constants`].
unsafe extern "C" fn set_graphics_root_32bit_constants(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    root_parameter_index: ddi12::UINT,
    num_32bit_values_to_set: ddi12::UINT,
    p_src_data: *const core::ffi::c_void,
    dest_offset_in_32bit_values: ddi12::UINT,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is the body's.
    unsafe {
        set_root_32bit_constants(
            Pipeline::Graphics,
            h_list,
            root_parameter_index,
            num_32bit_values_to_set,
            p_src_data,
            dest_offset_in_32bit_values,
        )
    }
}

// ---------------------------------------------------------------------------
// Root buffer views — 6 slots
// ---------------------------------------------------------------------------

/// The body behind all six `PFND3D12DDI_SET_ROOT_BUFFER_VIEW` slots.
///
/// `D3D12DDI_GPU_VIRTUAL_ADDRESS` is a `UINT64` (`d3d12umddi.rs:47915`) and the
/// API takes a `u64`, so the address crosses unchanged — as `ResourceBinding.md`
/// says of the whole group: *"the API passes parameters directly through to the
/// DDI"* (`:5142`).
///
/// ⚠ **A zero address is forwarded and counted.** D3D12 permits a null root
/// descriptor for a parameter the executing shader does not access, so refusing
/// would break a legal application; but a null UAV descriptor reaching the host
/// is the family that produced the 72nd memory's *"null UAV counter descriptor
/// faulting the host GPU context"*, so it does not pass unnamed either.
/// [`L3B_REFUSALS.root_view_null_address`] is the instrument, and its expected
/// reading is 0.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`.
unsafe fn set_root_buffer_view(
    which: Pipeline,
    kind: RootView,
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    root_parameter_index: ddi12::UINT,
    buffer_location: ddi12::D3D12DDI_GPU_VIRTUAL_ADDRESS,
) {
    // SAFETY: the caller guarantees a live command-list handle.
    let Some(state) = (unsafe { list_state(h_list) }) else {
        return;
    };
    if buffer_location == 0 {
        note_refusal(&L3B_REFUSALS.root_view_null_address);
    }
    trace_line!(
        "Set{}Root{}: idx={} va={:#018x}",
        which.name(),
        kind.name(),
        root_parameter_index,
        buffer_location,
    );
    // SAFETY: `engine()` borrows the list this box owns; both arguments are
    // by-value scalars.
    unsafe {
        match (which, kind) {
            (Pipeline::Compute, RootView::ConstantBuffer) => state
                .engine()
                .SetComputeRootConstantBufferView(root_parameter_index, buffer_location),
            (Pipeline::Graphics, RootView::ConstantBuffer) => state
                .engine()
                .SetGraphicsRootConstantBufferView(root_parameter_index, buffer_location),
            (Pipeline::Compute, RootView::ShaderResource) => state
                .engine()
                .SetComputeRootShaderResourceView(root_parameter_index, buffer_location),
            (Pipeline::Graphics, RootView::ShaderResource) => state
                .engine()
                .SetGraphicsRootShaderResourceView(root_parameter_index, buffer_location),
            (Pipeline::Compute, RootView::UnorderedAccess) => state
                .engine()
                .SetComputeRootUnorderedAccessView(root_parameter_index, buffer_location),
            (Pipeline::Graphics, RootView::UnorderedAccess) => state
                .engine()
                .SetGraphicsRootUnorderedAccessView(root_parameter_index, buffer_location),
        }
    }
}

/// `pfnSetComputeRootConstantBufferView` -> the engine's matching form.
///
/// # Safety
/// As [`set_root_buffer_view`].
unsafe extern "C" fn set_compute_root_constant_buffer_view(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    root_parameter_index: ddi12::UINT,
    buffer_location: ddi12::D3D12DDI_GPU_VIRTUAL_ADDRESS,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is the body's.
    unsafe {
        set_root_buffer_view(
            Pipeline::Compute,
            RootView::ConstantBuffer,
            h_list,
            root_parameter_index,
            buffer_location,
        )
    }
}

/// `pfnSetGraphicsRootConstantBufferView` -> the engine's matching form.
///
/// # Safety
/// As [`set_root_buffer_view`].
unsafe extern "C" fn set_graphics_root_constant_buffer_view(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    root_parameter_index: ddi12::UINT,
    buffer_location: ddi12::D3D12DDI_GPU_VIRTUAL_ADDRESS,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is the body's.
    unsafe {
        set_root_buffer_view(
            Pipeline::Graphics,
            RootView::ConstantBuffer,
            h_list,
            root_parameter_index,
            buffer_location,
        )
    }
}

/// `pfnSetComputeRootShaderResourceView` -> the engine's matching form.
///
/// # Safety
/// As [`set_root_buffer_view`].
unsafe extern "C" fn set_compute_root_shader_resource_view(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    root_parameter_index: ddi12::UINT,
    buffer_location: ddi12::D3D12DDI_GPU_VIRTUAL_ADDRESS,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is the body's.
    unsafe {
        set_root_buffer_view(
            Pipeline::Compute,
            RootView::ShaderResource,
            h_list,
            root_parameter_index,
            buffer_location,
        )
    }
}

/// `pfnSetGraphicsRootShaderResourceView` -> the engine's matching form.
///
/// # Safety
/// As [`set_root_buffer_view`].
unsafe extern "C" fn set_graphics_root_shader_resource_view(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    root_parameter_index: ddi12::UINT,
    buffer_location: ddi12::D3D12DDI_GPU_VIRTUAL_ADDRESS,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is the body's.
    unsafe {
        set_root_buffer_view(
            Pipeline::Graphics,
            RootView::ShaderResource,
            h_list,
            root_parameter_index,
            buffer_location,
        )
    }
}

/// `pfnSetComputeRootUnorderedAccessView` -> the engine's matching form.
///
/// # Safety
/// As [`set_root_buffer_view`].
unsafe extern "C" fn set_compute_root_unordered_access_view(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    root_parameter_index: ddi12::UINT,
    buffer_location: ddi12::D3D12DDI_GPU_VIRTUAL_ADDRESS,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is the body's.
    unsafe {
        set_root_buffer_view(
            Pipeline::Compute,
            RootView::UnorderedAccess,
            h_list,
            root_parameter_index,
            buffer_location,
        )
    }
}

/// `pfnSetGraphicsRootUnorderedAccessView` -> the engine's matching form.
///
/// # Safety
/// As [`set_root_buffer_view`].
unsafe extern "C" fn set_graphics_root_unordered_access_view(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    root_parameter_index: ddi12::UINT,
    buffer_location: ddi12::D3D12DDI_GPU_VIRTUAL_ADDRESS,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is the body's.
    unsafe {
        set_root_buffer_view(
            Pipeline::Graphics,
            RootView::UnorderedAccess,
            h_list,
            root_parameter_index,
            buffer_location,
        )
    }
}

// ---------------------------------------------------------------------------
// Descriptor heaps — 1 slot
// ---------------------------------------------------------------------------

/// How many heaps one `pfnSetDescriptorHeaps` may bind before this driver
/// refuses.
///
/// ⛔ **This is a bound, not a guess, and it exists so the array can live on the
/// stack.** D3D12 permits at most one shader-visible heap of each shader-visible
/// type — `ResourceBinding.md:4436-4437`: *"The runtime validates that at most
/// one of any given shader visible descriptor heap type can be set"* — and there
/// are two such types (CBV/SRV/UAV and SAMPLER), so the real ceiling is **2**.
/// Eight is 4x headroom. The alternative, `Vec::with_capacity(NumDescriptorHeaps)`,
/// takes a `UINT` straight from the runtime into an allocation size: at
/// `0xFFFFFFFF` that is a 32 GiB request, and this crate is `panic = "abort"`,
/// so an allocation failure is a dead compositor rather than an error.
/// [`L3B_REFUSALS.descriptor_heaps_too_many`] says what to do if it ever moves.
const MAX_DESCRIPTOR_HEAPS: usize = 8;

/// `pfnSetDescriptorHeaps` -> `ID3D12GraphicsCommandList::SetDescriptorHeaps`.
///
/// ⚠ **`NumDescriptorHeaps == 0` is an unbind, not a no-op**, and it is
/// forwarded as one: *"this call replaces all previously set descriptor heaps
/// (even if it doesn't set any or all of them). So for example if
/// NumDescriptorHeaps is 0, that would be unbinding all descriptor heaps"*
/// (`ResourceBinding.md:4433-4435`). Returning early on a zero count would leave
/// the previous heaps bound.
///
/// ⛔ **A single unresolvable heap refuses the WHOLE call**, rather than
/// forwarding the rest. Because the call replaces the entire set, a partial
/// forward would silently *unbind* a heap the application still expects to be
/// bound — a worse and less attributable failure than the refusal.
///
/// ⚠ The engine's reference is **cloned** into the outgoing array, which is one
/// `AddRef`/`Release` pair per heap per call. That is deliberate: the safe
/// wrapper takes `&[Option<ID3D12DescriptorHeap>]`, `engine_heap` hands back a
/// borrow, and manufacturing an array of `Option<ManuallyDrop<_>>` to pass in
/// its place would be a layout claim about `Option`'s niche optimisation — an
/// ABI assumption to save two atomics on a per-command-list path.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`, and
/// `heaps`, when `num` is non-zero, must address at least `num`
/// `D3D12DDI_HDESCRIPTORHEAP`s for the duration of the call.
unsafe extern "C" fn set_descriptor_heaps(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    num: ddi12::UINT,
    heaps: *mut ddi12::D3D12DDI_HDESCRIPTORHEAP,
) {
    // SAFETY: the caller guarantees a live command-list handle.
    let Some(state) = (unsafe { list_state(h_list) }) else {
        return;
    };
    // ⛔ Per arm: a zero count with a null array is the legal unbind, a non-zero
    // count with a null array never is.
    if num != 0 && heaps.is_null() {
        note_refusal(&L3B_REFUSALS.descriptor_heaps_bad_arg);
        if let Some(n) = budget(&ERROR_LOG) {
            log_error!(
                "SetDescriptorHeaps: NumDescriptorHeaps={num} with a NULL array (x{})",
                n + 1,
            );
        }
        // SAFETY: `state` is this list's live state, as `set_root_signature`.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    }
    if num as usize > MAX_DESCRIPTOR_HEAPS {
        note_refusal(&L3B_REFUSALS.descriptor_heaps_too_many);
        if let Some(n) = budget(&ERROR_LOG) {
            log_error!(
                "SetDescriptorHeaps: {num} heaps exceeds this driver's bound of \
                 {MAX_DESCRIPTOR_HEAPS}; D3D12 allows at most one shader-visible heap per \
                 shader-visible type, so raise MAX_DESCRIPTOR_HEAPS only with evidence (x{})",
                n + 1,
            );
        }
        // SAFETY: this list's live state, as above.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    }

    // ⚠ `from_fn` rather than `[None; N]`: `ID3D12DescriptorHeap` is not `Copy`,
    // so the array-repeat form does not compile for it.
    let mut bound: [Option<ID3D12DescriptorHeap>; MAX_DESCRIPTOR_HEAPS] =
        core::array::from_fn(|_| None);

    for (i, out) in bound.iter_mut().enumerate().take(num as usize) {
        // SAFETY: `i < num` and the caller guarantees `num` live elements, so
        // this read is in bounds. The DDI hands a `*mut` and this driver only
        // reads through it.
        let h_heap = unsafe { *heaps.add(i) };
        if h_heap.pDrvPrivate.is_null() {
            // The runtime named no heap in this slot. vkd3d accepts a NULL entry
            // (`libs/vkd3d/command.c:14006-14011` skips it), so it is forwarded
            // as `None` rather than refused.
            note_refusal(&L3B_REFUSALS.descriptor_heap_null_entry);
            continue;
        }
        // SAFETY: non-null per the check; the caller guarantees the handle came
        // from `descriptors::create_descriptor_heap`, and the borrow ends inside
        // this loop iteration.
        let Some(heap) = (unsafe { engine_heap(h_heap) }) else {
            note_refusal(&L3B_REFUSALS.descriptor_heap_missing);
            if let Some(n) = budget(&ERROR_LOG) {
                log_error!(
                    "SetDescriptorHeaps: heap {i} of {num} ({:p}) did not resolve; refusing the \
                     whole call, because this DDI replaces the entire bound set and a partial \
                     forward would unbind a heap the application still expects (x{})",
                    h_heap.pDrvPrivate,
                    n + 1,
                );
            }
            // SAFETY: this list's live state, as above.
            unsafe { report_error(state, E_INVALIDARG) };
            return;
        };
        // ⚠ The clone is the `AddRef`; `bound` releases it on the way out. See
        // the doc above for why the borrow is not passed through directly.
        *out = Some((*heap).clone());
    }

    trace_line!("SetDescriptorHeaps: list={:p} n={num}", h_list.pDrvPrivate);

    // SAFETY: `engine()` borrows the list this box owns, and the slice's
    // elements are owned references live for the duration of the call.
    unsafe { state.engine().SetDescriptorHeaps(&bound[..num as usize]) };
}

// ---------------------------------------------------------------------------
// Clearing root arguments — 1 slot, and the only refusal in this lane
// ---------------------------------------------------------------------------

/// `pfnClearRootArguments` — **refused and counted**; there is no API
/// counterpart narrow enough to forward to.
///
/// # What the DDI asks for
///
/// *"This DDI zero-initializes root arguments. The purpose is to ensure that
/// applications cannot leak root arguments (root constants, root views,
/// descriptor tables) from 1 command list to the next. The runtime calls this
/// DDI when creating a new command list, during ID3D12CommandList::Reset, and
/// during ID3D12CommandList::ClearState. Note that there are separate DDI calls
/// to clear other command list state (vertex buffers, render targets, PSO,
/// etc)."* — `ResourceBinding.md:5287-5295`.
///
/// # ⛔ Why forwarding to `ClearState` is WRONG, measured rather than argued
///
/// The narrowest engine operation that zeroes root arguments is
/// `ID3D12GraphicsCommandList::ClearState`, and vkd3d implements it as
/// `d3d12_command_list_reset_api_state(list, pipeline_state)`
/// (`vkd3d-proton-helios/libs/vkd3d/command.c:7399-7411`) — the **whole** API
/// state: PSO, render targets, viewports, scissors, blend factor, stencil ref,
/// index buffer, and the two root-argument binding sets.
///
/// `DDI_REFERENCE.md` §14.0 (`D12-G5`'s measured per-slot call counts), in the
/// bullet that begins *"`pfnResetCommandList` is followed by a fixed 15-call
/// state-reset block"*, records the measured call order: that block begins with
/// `pfnSetPipelineState` and **ends with `pfnClearRootArguments`**. ⚠ Cited by
/// section and by its opening words rather than by line span on purpose — the
/// span this lane was written against moved 30 lines the same afternoon, when a
/// correction block was inserted earlier in the file. So forwarding
/// `ClearState` here would discard the
/// pipeline state, render targets and viewports that the preceding fourteen
/// calls had just set — on every single reset, silently, as wrong pixels. That
/// is the `ARCHITECTURE.md` §12 rule 9 failure shape (*"wrong blending for DWM,
/// no counter, no log, only pixels"*) arrived at from a different direction.
///
/// ⛔ `SetGraphicsRootSignature(NULL)` is not the narrower alternative either:
/// vkd3d's `set_root_signature` early-returns when the signature is unchanged
/// (`command.c:14024-14025`), so it clears nothing unless it also **drops the
/// bound root signature**, which this DDI explicitly must not depend on
/// (*"this DDI should apply the same operation regardless of the currently set
/// root signature"*).
///
/// # ⭐ What is therefore already discharged, and what is left
///
/// Two of the DDI's three call sites are covered by the forward this driver
/// already performs elsewhere:
///
/// * **command-list creation** — `d3d12_command_list_init` opens with
///   `memset(list, 0, sizeof(*list))` (`command.c:22126`, `:22131`), so both
///   `vkd3d_pipeline_bindings` are zero before the runtime can call this slot;
/// * **`Reset`** — `cmdlist.rs::reset_command_list` forwards to
///   `ID3D12GraphicsCommandList::Reset`, and vkd3d's `Reset` calls
///   `d3d12_command_list_reset_state` (`command.c:7392-7393`), which resets both
///   binding sets.
///
/// ⚠ **The residual, named:** a mid-list `ID3D12GraphicsCommandList::ClearState`.
/// There, vkd3d's root bindings keep the application's previous root arguments
/// instead of being zeroed. The exposure is bounded — after `ClearState` the
/// application's own next draw is reading root arguments D3D12 defines as
/// undefined — but it is real, and it is what [`L3B_REFUSALS.clear_root_arguments_not_forwarded`]
/// is the instrument for. Closing it needs an engine entry point that resets the
/// two `vkd3d_pipeline_bindings` and nothing else; that is a vkd3d change and a
/// `DECISIONS.md` D4 export, not something this file can do.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`. The body
/// reads nothing through it beyond the state lookup.
unsafe extern "C" fn clear_root_arguments(h_list: ddi12::D3D12DDI_HCOMMANDLIST) {
    // SAFETY: the caller guarantees a live command-list handle. The lookup is
    // kept even though nothing is forwarded, so that a stale list handle is
    // attributed to `L3bCommandListMissing` here as it is everywhere else in
    // this file rather than reading as a silent success.
    if unsafe { list_state(h_list) }.is_none() {
        return;
    }
    // R911: this arm logs its own line, so it bumps rather than `note_refusal`s.
    L3B_REFUSALS.clear_root_arguments_not_forwarded.bump();
    if let Some(n) = budget(&CLEAR_ROOT_ARGS_LOG) {
        log_error!(
            "ClearRootArguments: not forwarded -- the narrowest engine operation that zeroes \
             root arguments is ClearState, which also drops the PSO, render targets and \
             viewports that the runtime's own 15-call reset block set immediately before this \
             call. Creation and Reset are already covered by the engine's own list reset; the \
             residual is a mid-list ID3D12GraphicsCommandList::ClearState (x{})",
            n + 1,
        );
    }
}

// ---------------------------------------------------------------------------
// The resource seam for the clear/discard slots
// ---------------------------------------------------------------------------

/// What a clear or discard slot's `hDrvResource` names.
///
/// ⛔ **The three answers are kept apart at the one place the distinction is
/// still available** — before `resource12::engine_resource` flattens a null
/// handle and an unresolvable one into the same `None`. `descriptors.rs`'s
/// `ViewResource` is the same shape, and the reason is that the two carry
/// different information: a null is the runtime declining to name a resource, an
/// unresolvable handle is this driver losing one it was given.
///
/// ⚠ **Where this lane's grading differs from L5's.** For view *creation* a null
/// resource is D3D12's legal null-descriptor form. For these three slots it is
/// not: `ID3D12GraphicsCommandList::ClearUnorderedAccessView{Uint,Float}` and
/// `::DiscardResource` all declare `pResource` required, and vkd3d's
/// `DiscardResource` dereferences it without a null check —
/// `impl_from_ID3D12Resource(resource)` feeds `d3d12_resource_is_texture`, whose
/// body is `resource->desc.Dimension != ...` with no guard
/// (`libs/vkd3d/vkd3d_private.h:1217-1220`). So [`Null`](ClearResource::Null) is
/// counted and dropped here rather than forwarded.
///
/// ⛔ Note the `_In_` annotation is deliberately **not** cited as the evidence:
/// [`L3B_REFUSALS.root_signature_null`]'s re-grading established that
/// `ResourceBinding.md` puts `_In_` on by-value scalars with no null form at
/// all, so it says nothing about nullability. The API's own requirement and the
/// engine's unguarded dereference are what carry this.
enum ClearResource<'a> {
    /// The runtime named **no** resource. Counted on
    /// [`L3B_REFUSALS.clear_resource_null`], **not** forwarded, and reported at
    /// list scope; see the type doc for why this is not L5's legal
    /// null-descriptor form.
    Null,
    /// The runtime named a resource and this driver resolved it to the engine's
    /// object, **borrowed** from L4's state box.
    Engine(&'a ID3D12Resource),
    /// The runtime named a resource and this driver could **not** resolve it.
    Unresolved,
}

/// Classify a clear or discard slot's `hDrvResource`. See [`ClearResource`].
///
/// # Safety
/// `h_resource` is the runtime's own handle for this DDI call, and the returned
/// borrow must not outlive that call — as `resource12::engine_resource`.
unsafe fn clear_resource<'a>(h_resource: ddi12::D3D12DDI_HRESOURCE) -> ClearResource<'a> {
    if h_resource.pDrvPrivate.is_null() {
        return ClearResource::Null;
    }
    // SAFETY: non-null per the check, and the caller's guarantee is this
    // function's.
    match unsafe { resource12::engine_resource(h_resource) } {
        Some(res) => ClearResource::Engine(res),
        None => ClearResource::Unresolved,
    }
}

/// Resolve a clear or discard slot's resource, counting and reporting the two
/// failures so the call sites stay one `let else`.
///
/// ⭐ **Both failures are reported now, and the null one is a policy change.**
/// It used to be counted-only, and its reason was written down: *"the cost of
/// being wrong in one direction is a dropped clear (wrong pixels), and in the
/// other it is 'Removing device due to bad UMD error' on a call that turns out
/// to be legal"*. The right-hand cost is what changed — [`report_error`] now
/// quarantines the one list and the application learns at `Close()` — so the
/// asymmetry that bought silence is gone, and what is left is this driver
/// dropping a clear the application asked for without telling it. See
/// [`L3B_REFUSALS.clear_resource_null`] for the reversal condition.
///
/// # Safety
/// As [`clear_resource`]; `state` must be the list's own live state, which is
/// [`report_error`]'s precondition.
unsafe fn clear_resource_or_refuse<'a>(
    slot: &str,
    state: &CommandListState,
    h_resource: ddi12::D3D12DDI_HRESOURCE,
) -> Option<&'a ID3D12Resource> {
    // SAFETY: forwarded; the caller carries `clear_resource`'s precondition.
    match unsafe { clear_resource(h_resource) } {
        ClearResource::Engine(res) => Some(res),
        ClearResource::Null => {
            note_refusal(&L3B_REFUSALS.clear_resource_null);
            if let Some(n) = budget(&ERROR_LOG) {
                log_error!(
                    "{slot}: hDrvResource is NULL. This slot's API counterpart declares \
                     pResource as required, and the engine dereferences it, so the call is \
                     dropped rather than forwarded -- and reported on THIS COMMAND LIST, which \
                     costs the list and not the device. If this ever turns out to be a legal \
                     lowering, revert the report and leave the counter (x{})",
                    n + 1,
                );
            }
            // SAFETY: the caller guarantees `state` is this list's own live
            // state, which is `report_error`'s precondition.
            unsafe { report_error(state, E_INVALIDARG) };
            None
        }
        ClearResource::Unresolved => {
            note_refusal(&L3B_REFUSALS.clear_resource_missing);
            if let Some(n) = budget(&ERROR_LOG) {
                log_error!(
                    "{slot}: the runtime named resource {:p} and this driver could not resolve \
                     it (x{})",
                    h_resource.pDrvPrivate,
                    n + 1,
                );
            }
            // SAFETY: the caller guarantees `state` is this list's own live
            // state, which is `report_error`'s precondition.
            unsafe { report_error(state, E_INVALIDARG) };
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Clears and discard — 5 slots
// ---------------------------------------------------------------------------

/// Which of the two `pfnClearUnorderedAccessView*` slots is being served, and
/// where its four clear values are.
///
/// ⚠ The pointer lives in the enum rather than a borrow, because the null check
/// belongs after the command-list lookup: without the list's state there is
/// nothing to report a null through — [`report_error`] needs both of the handles
/// that state carries.
#[derive(Clone, Copy)]
enum UavClear {
    Uint(*const ddi12::UINT),
    Float(*const ddi12::FLOAT),
}

impl UavClear {
    fn name(self) -> &'static str {
        match self {
            UavClear::Uint(_) => "ClearUnorderedAccessViewUint",
            UavClear::Float(_) => "ClearUnorderedAccessViewFloat",
        }
    }

    /// Borrow the four clear values, or `None` when the pointer is null.
    ///
    /// ⛔ The null check and the dereference are the **same** step on purpose.
    /// Checking `is_null()` in one place and dereferencing in another leaves a
    /// second `None` arm on the forward path that can only be reached if the
    /// two disagree — an unreachable branch whose only possible body is a
    /// silent `return`, i.e. a dropped clear with no counter.
    ///
    /// # Safety
    /// The pointer, when non-null, must address four 32-bit values for the
    /// duration of the call, and the returned borrow must not outlive it.
    unsafe fn values<'a>(self) -> Option<UavClearValues<'a>> {
        match self {
            // SAFETY: forwarded; `api_color_u32` folds the null case into `None`
            // and the caller guarantees four values behind a non-null pointer.
            UavClear::Uint(p) => unsafe { api_color_u32(p) }.map(UavClearValues::Uint),
            // SAFETY: as above, for `FLOAT`.
            UavClear::Float(p) => unsafe { api_color_f32(p) }.map(UavClearValues::Float),
        }
    }
}

/// The four clear values, borrowed from the runtime's own memory.
enum UavClearValues<'a> {
    Uint(&'a [u32; 4]),
    Float(&'a [f32; 4]),
}

/// The body behind `pfnClearUnorderedAccessViewUint` and
/// `pfnClearUnorderedAccessViewFloat`.
///
/// ⭐ **Both handles cross, because the API takes both.** The DDI carries a GPU
/// handle *in the currently bound heap* and a CPU handle for the same
/// descriptor, and so does
/// `ID3D12GraphicsCommandList::ClearUnorderedAccessView{Uint,Float}` — so
/// forwarding both is what the signature requires, and neither is dropped.
///
/// ⚠ **They are not equally load-bearing, and a debugger needs to know which.**
/// The functional decode is entirely the **CPU** handle plus the resource:
/// `d3d12_desc_decode_metadata(list->device, cpu_handle.ptr)`
/// (`libs/vkd3d/command.c:16018`) and `impl_from_ID3D12Resource(resource)`
/// (`:16019`), both required before the clear proceeds (`:16021`). The **GPU**
/// handle is read nowhere in either engine body outside the `TRACE` line
/// (`:16008`) — checked over the whole of
/// `d3d12_command_list_ClearUnorderedAccessViewUint` (`:15995-16157`) and
/// `..._Float` (`:16158-16264`). So passing it is contract compliance rather
/// than an engine requirement, and a UAV clear that produces nothing is a
/// CPU-handle-or-resource question, never a GPU-handle one. ⛔ That also bounds
/// what a future descriptor-model change has to preserve: the GPU handle's
/// value is currently unobserved by vkd3d, the CPU handle's is not.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`; the values
/// pointer, when non-null, must address four 32-bit values; `p_rects`, when
/// `num_rects` is non-zero, must address that many rects.
unsafe fn clear_unordered_access_view(
    values: UavClear,
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    gpu_handle: ddi12::D3D12DDI_GPU_DESCRIPTOR_HANDLE,
    cpu_handle: ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
    h_resource: ddi12::D3D12DDI_HRESOURCE,
    num_rects: ddi12::UINT,
    p_rects: *const ddi12::D3D12DDI_RECT,
) {
    // SAFETY: the caller guarantees a live command-list handle.
    let Some(state) = (unsafe { list_state(h_list) }) else {
        return;
    };
    // SAFETY: the caller guarantees four values behind a non-null pointer;
    // `values()` folds the null case into `None` at the same step as the read.
    let Some(resolved) = (unsafe { values.values() }) else {
        note_refusal(&L3B_REFUSALS.clear_bad_arg);
        if let Some(n) = budget(&ERROR_LOG) {
            log_error!("{}: NULL clear values (x{})", values.name(), n + 1);
        }
        // SAFETY: `state` is this list's live state, as `set_root_signature`.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };
    // SAFETY: the caller guarantees `num_rects` live elements behind a non-null
    // pointer; `api_rects` validates the pairing before reading.
    let Some(rects) = (unsafe { api_rects(num_rects, p_rects) }) else {
        note_refusal(&L3B_REFUSALS.clear_bad_arg);
        if let Some(n) = budget(&ERROR_LOG) {
            log_error!(
                "{}: NumRects={num_rects} with a NULL pRects (x{})",
                values.name(),
                n + 1,
            );
        }
        // SAFETY: this list's live state, as above.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };
    // SAFETY: `state` is this list's own live state, which is what
    // `clear_resource_or_refuse` -- and `report_error` behind it -- requires.
    let Some(resource) = (unsafe { clear_resource_or_refuse(values.name(), state, h_resource) })
    else {
        return;
    };

    trace_line!(
        "{}: gpu={:#018x} cpu={:#018x} rects={num_rects}",
        values.name(),
        gpu_handle.ptr,
        cpu_handle.ptr,
    );

    let api_gpu = api_gpu_handle(gpu_handle);
    let api_cpu = api_cpu_handle(cpu_handle);
    // SAFETY: `engine()` borrows the list this box owns; `resource` is L4's
    // borrowed engine object; both handles cross by value; `resolved` and
    // `rects` borrow the runtime's own memory for the duration of the call.
    unsafe {
        match resolved {
            UavClearValues::Uint(v) => {
                state
                    .engine()
                    .ClearUnorderedAccessViewUint(api_gpu, api_cpu, resource, v, rects);
            }
            UavClearValues::Float(v) => {
                state
                    .engine()
                    .ClearUnorderedAccessViewFloat(api_gpu, api_cpu, resource, v, rects);
            }
        }
    }
}

/// `pfnClearUnorderedAccessViewUint` -> the engine's `Uint` form.
///
/// # Safety
/// As [`clear_unordered_access_view`].
unsafe extern "C" fn clear_unordered_access_view_uint(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    gpu_handle: ddi12::D3D12DDI_GPU_DESCRIPTOR_HANDLE,
    cpu_handle: ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
    h_resource: ddi12::D3D12DDI_HRESOURCE,
    values: *const ddi12::UINT,
    num_rects: ddi12::UINT,
    p_rects: *const ddi12::D3D12DDI_RECT,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is the body's.
    unsafe {
        clear_unordered_access_view(
            UavClear::Uint(values),
            h_list,
            gpu_handle,
            cpu_handle,
            h_resource,
            num_rects,
            p_rects,
        )
    }
}

/// `pfnClearUnorderedAccessViewFloat` -> the engine's `Float` form.
///
/// # Safety
/// As [`clear_unordered_access_view`].
unsafe extern "C" fn clear_unordered_access_view_float(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    gpu_handle: ddi12::D3D12DDI_GPU_DESCRIPTOR_HANDLE,
    cpu_handle: ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
    h_resource: ddi12::D3D12DDI_HRESOURCE,
    values: *const ddi12::FLOAT,
    num_rects: ddi12::UINT,
    p_rects: *const ddi12::D3D12DDI_RECT,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is the body's.
    unsafe {
        clear_unordered_access_view(
            UavClear::Float(values),
            h_list,
            gpu_handle,
            cpu_handle,
            h_resource,
            num_rects,
            p_rects,
        )
    }
}

/// `pfnClearRenderTargetView` -> `ID3D12GraphicsCommandList::ClearRenderTargetView`.
///
/// ⚠ **No resource handle, on either side.** The DDI carries only the RTV's CPU
/// descriptor handle and so does the API: the descriptor already names its
/// resource, and vkd3d decodes it from the handle
/// (`d3d12_rtv_desc_from_cpu_handle`). So this slot has no
/// [`ClearResource`] classification and cannot have one.
///
/// ⭐ This is one of the 15 command-list slots `DDI_REFERENCE.md` §14.2 lists as
/// needing a real body for a first frame.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`; `colour`
/// must address four `FLOAT`s; `p_rects`, when `num_rects` is non-zero, must
/// address that many rects.
unsafe extern "C" fn clear_render_target_view(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    cpu_handle: ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
    colour: *const ddi12::FLOAT,
    num_rects: ddi12::UINT,
    p_rects: *const ddi12::D3D12DDI_RECT,
) {
    // SAFETY: the caller guarantees a live command-list handle.
    let Some(state) = (unsafe { list_state(h_list) }) else {
        return;
    };
    // SAFETY: the caller guarantees four `FLOAT`s behind a non-null pointer.
    let Some(rgba) = (unsafe { api_color_f32(colour) }) else {
        note_refusal(&L3B_REFUSALS.clear_bad_arg);
        if let Some(n) = budget(&ERROR_LOG) {
            log_error!("ClearRenderTargetView: NULL colour (x{})", n + 1);
        }
        // SAFETY: `state` is this list's live state, as `set_root_signature`.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };
    // SAFETY: as `clear_unordered_access_view`.
    let Some(rects) = (unsafe { api_rects(num_rects, p_rects) }) else {
        note_refusal(&L3B_REFUSALS.clear_bad_arg);
        if let Some(n) = budget(&ERROR_LOG) {
            log_error!(
                "ClearRenderTargetView: NumRects={num_rects} with a NULL pRects (x{})",
                n + 1,
            );
        }
        // SAFETY: this list's live state, as above.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };

    trace_line!(
        "ClearRenderTargetView: cpu={:#018x} rects={num_rects}",
        cpu_handle.ptr,
    );

    // SAFETY: `engine()` borrows the list this box owns; the handle crosses by
    // value; `rgba` and `rects` borrow the runtime's own memory for the call.
    // ⚠ `None` rather than `Some(&[])` for the empty case: both mean "the whole
    // view" to the engine, and `None` is the API's own spelling of it.
    unsafe {
        state.engine().ClearRenderTargetView(
            api_cpu_handle(cpu_handle),
            rgba,
            (!rects.is_empty()).then_some(rects),
        );
    }
}

/// The two bits `D3D12_CLEAR_FLAGS` defines, as a mask over the DDI's bare
/// `UINT`.
///
/// ⛔ **This is the one enum-shaped translation in this lane that CANNOT be
/// pinned with `PARALLEL.md` §3 rule 7's two-generated-constants assertion,
/// because there is only one generated constant.** `d3d12umddi.h` defines no
/// `D3D12DDI_CLEAR_FLAGS` at all — the parameter is an unnamed `UINT`
/// (`d3d12umddi.rs:52122-52132`; `grep CLEAR_FLAG` over the whole 5.4 MB
/// bindgen output finds nothing on the DDI side). The evidence that it carries
/// the API's values is the spec's own sentence over the whole clear block —
/// *"Parameters are passed directly through from API to DDI"*
/// (`ResourceBinding.md:5232`) — and vkd3d reading exactly
/// `D3D12_CLEAR_FLAG_DEPTH` / `_STENCIL` out of it
/// (`libs/vkd3d/command.c:15151-15155`). Recorded as a translation this driver
/// **assumes** rather than proves, with a counter under it.
const DDI_CLEAR_FLAGS_KNOWN: ddi12::UINT =
    D3D12_CLEAR_FLAG_DEPTH.0 as ddi12::UINT | D3D12_CLEAR_FLAG_STENCIL.0 as ddi12::UINT;

/// `pfnClearDepthStencilView` -> `ID3D12GraphicsCommandList::ClearDepthStencilView`.
///
/// ⚠ Unknown flag bits are **masked off and counted** rather than forwarded. The
/// DDI's parameter is a bare `UINT` with no enum behind it (see
/// [`DDI_CLEAR_FLAGS_KNOWN`]), so a bit this build does not know about is a
/// header revision this driver has not been taught; forwarding it would hand the
/// engine a `D3D12_CLEAR_FLAGS` value the API never defined.
///
/// ⚠ `Flags == 0` is **not** counted. vkd3d answers it with *"Not clearing any
/// aspects"* and returns (`libs/vkd3d/command.c:15159-15163`), which is the same
/// no-op the API defines, so there is nothing here this driver refuses.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`; `p_rects`,
/// when `num_rects` is non-zero, must address that many rects.
unsafe extern "C" fn clear_depth_stencil_view(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    cpu_handle: ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
    flags: ddi12::UINT,
    depth: ddi12::FLOAT,
    stencil: ddi12::UINT8,
    num_rects: ddi12::UINT,
    p_rects: *const ddi12::D3D12DDI_RECT,
) {
    // SAFETY: the caller guarantees a live command-list handle.
    let Some(state) = (unsafe { list_state(h_list) }) else {
        return;
    };
    if flags & !DDI_CLEAR_FLAGS_KNOWN != 0 {
        // R911: this arm logs its own line, so it bumps rather than
        // `note_refusal`s.
        L3B_REFUSALS.clear_depth_stencil_flags_unknown.bump();
        if let Some(n) = budget(&ERROR_LOG) {
            log_error!(
                "ClearDepthStencilView: Flags {flags:#x} carry bits outside \
                 {DDI_CLEAR_FLAGS_KNOWN:#x}; only the known ones are forwarded (x{})",
                n + 1,
            );
        }
    }
    // SAFETY: as `clear_unordered_access_view`.
    let Some(rects) = (unsafe { api_rects(num_rects, p_rects) }) else {
        note_refusal(&L3B_REFUSALS.clear_bad_arg);
        if let Some(n) = budget(&ERROR_LOG) {
            log_error!(
                "ClearDepthStencilView: NumRects={num_rects} with a NULL pRects (x{})",
                n + 1,
            );
        }
        // SAFETY: `state` is this list's live state, as `set_root_signature`.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };

    trace_line!(
        "ClearDepthStencilView: cpu={:#018x} flags={flags:#x} rects={num_rects}",
        cpu_handle.ptr,
    );

    // ⚠ The mask, not a cast: see `DDI_CLEAR_FLAGS_KNOWN`.
    let api_flags = D3D12_CLEAR_FLAGS((flags & DDI_CLEAR_FLAGS_KNOWN) as i32);
    // SAFETY: `engine()` borrows the list this box owns; the handle and the two
    // scalars cross by value; `rects` borrows the runtime's own array. ⚠ The
    // API's rect parameter here is a plain slice rather than an `Option`, so the
    // empty case is the empty slice.
    unsafe {
        state.engine().ClearDepthStencilView(
            api_cpu_handle(cpu_handle),
            api_flags,
            depth,
            stencil,
            rects,
        );
    }
}

/// `pfnDiscardResource` -> `ID3D12GraphicsCommandList::DiscardResource`.
///
/// ⚠ **A null `pArgs` is legal and means "the whole resource"** — the spec
/// declares it `_In_opt_` (`ResourceBinding.md:5276-5278`) and the API's `pRegion` is
/// optional with the same meaning, so it is forwarded as `None` and not counted.
///
/// ⛔ `D3D12DDIARG_DISCARD_RESOURCE_0003` and `D3D12_DISCARD_REGION` have the
/// same four fields in the same order, and the copy below is still written out
/// **by field**: `PARALLEL.md` §10's ABI lens exists because a cast between two
/// independently-generated structs is a claim no reader can check, and the four
/// named assignments cost nothing on a path this rare.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`; `arg`, when
/// non-null, must point at a live `D3D12DDIARG_DISCARD_RESOURCE_0003` whose
/// `pRects` addresses at least `NumRects` rects.
unsafe extern "C" fn discard_resource(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_resource: ddi12::D3D12DDI_HRESOURCE,
    arg: *const ddi12::D3D12DDIARG_DISCARD_RESOURCE_0003,
) {
    // SAFETY: the caller guarantees a live command-list handle.
    let Some(state) = (unsafe { list_state(h_list) }) else {
        return;
    };
    // SAFETY: `state` is this list's own live state, which is what
    // `clear_resource_or_refuse` -- and `report_error` behind it -- requires.
    let Some(resource) =
        (unsafe { clear_resource_or_refuse("DiscardResource", state, h_resource) })
    else {
        return;
    };

    // ⚠ The region is built into a local that outlives the call below; the API
    // takes a raw pointer to it.
    let region;
    let p_region = if arg.is_null() {
        None
    } else {
        // SAFETY: non-null per the check; the DDI declares it `_In_opt_ CONST`,
        // so a non-null pointer addresses one live struct for the call.
        let a = unsafe { &*arg };
        // ⛔ Per arm, before the array is handed on: a non-zero count with a
        // null pointer is the read `CLAUDE.md`'s validation rule exists for.
        if a.NumRects != 0 && a.pRects.is_null() {
            note_refusal(&L3B_REFUSALS.clear_bad_arg);
            if let Some(n) = budget(&ERROR_LOG) {
                log_error!(
                    "DiscardResource: NumRects={} with a NULL pRects (x{})",
                    a.NumRects,
                    n + 1,
                );
            }
            // SAFETY: `state` is this list's live state, as `set_root_signature`.
            unsafe { report_error(state, E_INVALIDARG) };
            return;
        }
        region = D3D12_DISCARD_REGION {
            NumRects: a.NumRects,
            // ⚠ The cast is the rect-layout proof at the top of this file, and
            // nothing else: the pointer is the runtime's own array, borrowed for
            // the call. A null one is paired with `NumRects == 0` by the check
            // above, and the engine does not read it then.
            pRects: a.pRects.cast::<RECT>(),
            FirstSubresource: a.FirstSubresource,
            NumSubresources: a.NumSubresources,
        };
        Some(core::ptr::from_ref(&region))
    };

    trace_line!(
        "DiscardResource: res={:p} region={}",
        h_resource.pDrvPrivate,
        u8::from(p_region.is_some()),
    );

    // SAFETY: `engine()` borrows the list this box owns; `resource` is L4's
    // borrowed engine object; `p_region`, when `Some`, points at `region`, which
    // is live for this call.
    unsafe { state.engine().DiscardResource(resource, p_region) };
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// Install L3b's 21 command-list slots.
///
/// Chain position: `RecordSlots` -> `RootArgSlots` on the command-list table.
pub(crate) fn install(
    mut filling: Filling<'_, CommandListTable, stage::RecordSlots>,
) -> Filling<'_, CommandListTable, stage::RootArgSlots> {
    let table = filling.table();
    // root arguments — 16
    table.pfnSetDescriptorHeaps = Some(set_descriptor_heaps);
    table.pfnSetComputeRootSignature = Some(set_compute_root_signature);
    table.pfnSetGraphicsRootSignature = Some(set_graphics_root_signature);
    table.pfnSetComputeRootDescriptorTable = Some(set_compute_root_descriptor_table);
    table.pfnSetGraphicsRootDescriptorTable = Some(set_graphics_root_descriptor_table);
    table.pfnSetComputeRoot32BitConstant = Some(set_compute_root_32bit_constant);
    table.pfnSetGraphicsRoot32BitConstant = Some(set_graphics_root_32bit_constant);
    table.pfnSetComputeRoot32BitConstants = Some(set_compute_root_32bit_constants);
    table.pfnSetGraphicsRoot32BitConstants = Some(set_graphics_root_32bit_constants);
    table.pfnSetComputeRootConstantBufferView = Some(set_compute_root_constant_buffer_view);
    table.pfnSetGraphicsRootConstantBufferView = Some(set_graphics_root_constant_buffer_view);
    table.pfnSetComputeRootShaderResourceView = Some(set_compute_root_shader_resource_view);
    table.pfnSetGraphicsRootShaderResourceView = Some(set_graphics_root_shader_resource_view);
    table.pfnSetComputeRootUnorderedAccessView = Some(set_compute_root_unordered_access_view);
    table.pfnSetGraphicsRootUnorderedAccessView = Some(set_graphics_root_unordered_access_view);
    table.pfnClearRootArguments = Some(clear_root_arguments);
    // clears and discard — 5
    table.pfnClearUnorderedAccessViewUint = Some(clear_unordered_access_view_uint);
    table.pfnClearUnorderedAccessViewFloat = Some(clear_unordered_access_view_float);
    table.pfnClearRenderTargetView = Some(clear_render_target_view);
    table.pfnClearDepthStencilView = Some(clear_depth_stencil_view);
    table.pfnDiscardResource = Some(discard_resource);
    filling.advance()
}

// ---------------------------------------------------------------------------
// Refusal counters
// ---------------------------------------------------------------------------

/// L3b's refusal counters. One instance, [`L3B_REFUSALS`]; the set that prints
/// them is [`REFUSALS`].
pub(crate) struct L3bRefusals {
    /// A slot in this lane could not resolve its `D3D12DDI_HCOMMANDLIST` to a
    /// live `queue::CommandListState`. **Expected 0** — the runtime only records
    /// into a list `pfnCreateCommandList` returned `S_OK` for.
    ///
    /// ⚠ Deliberately **not** reported: with no state there is neither an
    /// `h_device` to reach the callback table through nor an `h_rt_list` to name
    /// the list to quarantine, so this is the one failure this table cannot
    /// escalate. `cmdlist.rs`'s counter of the same name is the same case in
    /// that lane's slots.
    command_list_missing: RefusalCounter,
    /// A slot needed to report a command-list error and the `h_device` its
    /// `CommandListState` recorded did not resolve to a live device.
    /// **Expected 0** — the state cannot outlive the device that created it, so
    /// a hit means a list state survived `pfnDestroyDevice`.
    set_error_no_device: RefusalCounter,
    /// A slot needed `pfnSetCommandListErrorCb` and the runtime's callback table
    /// did not carry one.
    ///
    /// ⛔ **Expected 0**, and re-graded: this used to count an absent
    /// `pfnSetErrorCb`, whose absence would have meant *a device that never
    /// dies*. It now counts an absent `pfnSetCommandListErrorCb`, one field
    /// below it in `D3D12DDI_CORELAYER_DEVICECALLBACKS_0062` — the same struct
    /// `forward12::queue::set_command_list_ddi_table` already reads
    /// `pfnSetCommandListDDITableCb` out of — and the consequence is different:
    /// **a recording failure the runtime never learns about.** The list is not
    /// quarantined, the application's `Close()` succeeds, and it renders from
    /// state this driver already knows is wrong. ⚠ Two absent callbacks in one
    /// struct this driver otherwise reads successfully means the negotiated
    /// table is not the `_0062` shape this driver assumes, so a hit is a
    /// version-negotiation finding before it is anything else.
    set_error_cb_absent: RefusalCounter,
    /// `pfnSetDescriptorHeaps` was given a non-zero count with a null array.
    /// **Expected 0.** ⚠ A **zero** count with a null array is the legal unbind
    /// and is not counted here.
    descriptor_heaps_bad_arg: RefusalCounter,
    /// `pfnSetDescriptorHeaps` asked for more heaps than
    /// [`MAX_DESCRIPTOR_HEAPS`], and the call was refused.
    ///
    /// ⛔ **Expected 0, and a hit is a decision rather than a bug fix.** D3D12
    /// allows at most one shader-visible heap of each shader-visible type, so
    /// the real ceiling is 2 and the bound is 8. A hit means either the runtime
    /// binds more than the spec describes, or this driver is being handed a
    /// count it should not trust — raise the constant only with the run that
    /// produced the hit attached, never to make a counter stop moving.
    descriptor_heaps_too_many: RefusalCounter,
    /// One entry of a `pfnSetDescriptorHeaps` array had a null `pDrvPrivate`,
    /// and a null was forwarded in its place.
    ///
    /// ⚠ **Not a refusal.** vkd3d skips a NULL entry
    /// (`libs/vkd3d/command.c:14006-14011`), so this is a legal shape rather
    /// than a failure — but the runtime names heaps it asked this driver to
    /// create, so the expected reading is still **0** and a hit is a finding
    /// about the runtime's lowering.
    descriptor_heap_null_entry: RefusalCounter,
    /// A `pfnSetDescriptorHeaps` entry named a **non-null** heap handle this
    /// driver could not resolve, and **the whole call was refused**.
    ///
    /// ⛔ **Expected 0.** The refusal is whole-call rather than per-entry
    /// because this DDI replaces the entire bound set: forwarding the survivors
    /// would unbind a heap the application still expects, which is a worse
    /// failure than the one being reported. Every hit also reports on the list,
    /// so the runtime quarantines it and the application fails at `Close()`
    /// rather than drawing from a descriptor table nobody bound.
    descriptor_heap_missing: RefusalCounter,
    /// `pfnSet{Compute,Graphics}RootSignature` was given a null handle, and a
    /// null root signature was forwarded (an unbind).
    ///
    /// ⚠ **Not a refusal**, and legal at the API —
    /// `SetGraphicsRootSignature(nullptr)` is accepted and vkd3d implements it:
    /// `d3d12_command_list_set_root_signature` stores the `NULL` and
    /// invalidates every root parameter (`libs/vkd3d/command.c:14021-14034`).
    ///
    /// ⚠ **Expected 0 OR non-zero — this counter is not graded.** A non-zero
    /// reading means an application unbound a root signature, which is legal
    /// D3D12 and which this driver forwards correctly; it is a **finding only
    /// if it correlates with wrong rendering**. Do not open an investigation on
    /// the reading alone.
    ///
    /// ⛔ Which is why the call site uses `bump`, not `note_refusal`: an
    /// ungraded forward must not print the `D3D12 DDI refusals:` set into a log
    /// whose triage step is to grep for exactly that line. It is still readable
    /// — `crate::log_refusal_summary` dumps the whole set at adapter close and
    /// device teardown regardless.
    ///
    /// ⛔ It was graded "expected 0" on the DDI prototype's `_In_`
    /// (`ResourceBinding.md:5100-5102`) and that grading was **wrong**: the same
    /// spec block annotates by-value scalars with no null form the same way —
    /// `_In_ UINT RootParameterIndex` (`:5113`), `_In_
    /// D3D12DDI_GPU_DESCRIPTOR_HANDLE BaseDescriptor` (`:5114`), `_In_
    /// D3D12DDI_GPU_VIRTUAL_ADDRESS BufferLocation` (`:5148`) — so `_In_` here
    /// says nothing about nullability. The inconsistency was visible inside
    /// this very struct: `root_view_null_address` sits behind `:5148`'s
    /// identical annotation and was never graded from it.
    root_signature_null: RefusalCounter,
    /// `pfnSet{Compute,Graphics}RootSignature` named a **non-null** handle this
    /// driver could not resolve. ⛔ **Expected 0**, and reported: every root
    /// argument set afterwards on that pipeline would be interpreted against
    /// whatever layout is still bound, which is silently wrong rendering rather
    /// than a failure the application can see.
    root_signature_missing: RefusalCounter,
    /// A root descriptor table was bound at GPU address 0.
    ///
    /// ⚠ **Not a refusal — it is forwarded** — and it is the **downstream half
    /// of L5's `DescriptorHeapGpuHandleZero`**. vkd3d assigns a heap's `gpu_va`
    /// only for shader-visible heaps (`libs/vkd3d/resource.c:10159`), so a table
    /// bound at 0 is a table in a heap the engine never gave an address to.
    /// **Expected 0.** ⭐ Read the two together: this one moving while L5's stays
    /// at 0 means the zero came from somewhere other than a heap start, and both
    /// moving is the known disagreement L5 documented — `ResourceBinding.md`
    /// says every heap must have a non-NULL GPU address at the DDI and vkd3d
    /// disagrees.
    root_table_zero_base: RefusalCounter,
    /// `pfnSet{Compute,Graphics}Root32BitConstants` asked for a non-zero number
    /// of values from a null `pSrcData`. **Expected 0**; the call is refused and
    /// reported, because the engine would read `Num32BitValuesToSet * 4` bytes
    /// through that pointer.
    root_constants_bad_arg: RefusalCounter,
    /// A root CBV/SRV/UAV was set to GPU virtual address 0, and it was
    /// **forwarded**.
    ///
    /// ⚠ **Not a refusal.** D3D12 permits a null root descriptor for a parameter
    /// the executing shader does not access, so refusing would break a legal
    /// application. **Expected 0** in practice, and named because a null
    /// descriptor reaching the host is the family that produced the 72nd
    /// memory's null UAV counter descriptor faulting the host GPU context. A
    /// non-zero reading alongside a host-side fault is where to look first.
    root_view_null_address: RefusalCounter,
    /// A clear or discard slot was given a runtime array or value pointer it
    /// could not read: a non-zero `NumRects` with a null `pRects`, or a null
    /// clear-value pointer. **Expected 0**; the call is refused and reported.
    clear_bad_arg: RefusalCounter,
    /// A clear or discard slot was given a **null** `hDrvResource`, and the call
    /// was dropped.
    ///
    /// ⛔ **Expected 0, and this is NOT L5's legal null-descriptor form.** For a
    /// view *creation* a null resource is required D3D12 behaviour; for
    /// `ClearUnorderedAccessView{Uint,Float}` and `DiscardResource` the API
    /// declares `pResource` required, and vkd3d's `DiscardResource` dereferences
    /// it without a null check — `d3d12_resource_is_texture` is
    /// `resource->desc.Dimension != ...` with no guard
    /// (`libs/vkd3d/vkd3d_private.h:1217-1220`).
    ///
    /// ⭐ **It IS reported now, and that is a change from S6 Round 2.** The old
    /// doc justified silence with *"the cost of being wrong ... is 'Removing
    /// device due to bad UMD error' on a call that turns out to be legal"*, and
    /// that cost is gone: this lane reports through
    /// `pfnSetCommandListErrorCb`, which quarantines the one list and surfaces
    /// at `Close()`. What remains is a clear the application asked for, dropped,
    /// with nothing told to the application — which is the failure shape this
    /// project's rules forbid outright.
    ///
    /// ⚠ **The reversal condition, named.** vkd3d's ClearUAV bodies *do* guard
    /// (`if (!resource_impl || !metadata.view) return;`,
    /// `libs/vkd3d/command.c:16018-16022`), so if a null resource on those two
    /// slots ever turns out to be a legal lowering, this driver's drop already
    /// matches what the engine would have done and only the report is wrong:
    /// take the `report_error` call out of `clear_resource_or_refuse`'s `Null`
    /// arm, keep the counter, and attach the run. ⛔ Do not extend that to
    /// `DiscardResource` — there the null would fault inside the engine.
    clear_resource_null: RefusalCounter,
    /// A clear or discard slot named a **non-null** resource handle this driver
    /// could not resolve. ⛔ **Expected 0**; refused and reported, because a
    /// clear that did not happen is state the application will render from.
    clear_resource_missing: RefusalCounter,
    /// `pfnClearDepthStencilView` was given flag bits outside
    /// `D3D12_CLEAR_FLAG_DEPTH | _STENCIL`, and they were masked off.
    ///
    /// ⛔ **Expected 0**, and it is the instrument for the one assumption in
    /// this lane that could not be pinned by an assertion: `d3d12umddi.h`
    /// defines no `D3D12DDI_CLEAR_FLAGS`, so that the bare `UINT` carries the
    /// API's values rests on the spec's *"parameters are passed directly
    /// through"* plus vkd3d reading the same two bits. A hit means either the
    /// header grew a third bit or that assumption is wrong, and the two are
    /// distinguishable from the logged value.
    ///
    /// ⚠ **Reviewed against the list-scope error channel and deliberately still
    /// counted only.** The known bits are forwarded, so the app gets the
    /// depth/stencil clear it asked for and loses only an aspect D3D12 does not
    /// define; failing the whole list over a bit a future header added would be
    /// a worse answer than the partial clear.
    clear_depth_stencil_flags_unknown: RefusalCounter,
    /// `pfnClearRootArguments` was called and **not forwarded**.
    ///
    /// ⚠⚠ **Expected LARGE and NON-ZERO** — roughly once per command-list
    /// creation plus once per `pfnResetCommandList`, because the measured
    /// 15-call state-reset block in `DDI_REFERENCE.md` §14.0 (the
    /// *"`pfnResetCommandList` is followed by a fixed 15-call state-reset
    /// block"* bullet) ends with this slot. A **zero** reading is the finding
    /// here, not a non-zero
    /// one: it would mean the runtime does not call the slot at all and this
    /// whole refusal is dead weight.
    ///
    /// ⛔ What it does **not** measure is the exposure. Two of the DDI's three
    /// call sites — command-list creation and `Reset` — are already discharged
    /// by vkd3d's own list reset, which `cmdlist.rs::reset_command_list`
    /// forwards to. The residual is a mid-list
    /// `ID3D12GraphicsCommandList::ClearState`, which this counter cannot
    /// distinguish; see [`clear_root_arguments`] for why forwarding
    /// `ClearState` here would be worse than refusing, and for what closing the
    /// gap would take.
    ///
    /// ⛔ **And it must never be reported**, cheap channel or not. It fires on
    /// every command-list create and every `pfnResetCommandList`, so a
    /// `pfnSetCommandListErrorCb` here would quarantine every list the process
    /// ever records into — the device-scope outcome by a slower road. The
    /// counter and its own log line are the whole instrument.
    clear_root_arguments_not_forwarded: RefusalCounter,
}

pub(crate) static L3B_REFUSALS: L3bRefusals = L3bRefusals {
    command_list_missing: RefusalCounter::new("L3bCommandListMissing"),
    set_error_no_device: RefusalCounter::new("L3bSetErrorNoDevice"),
    set_error_cb_absent: RefusalCounter::new("L3bSetErrorCbAbsent"),
    descriptor_heaps_bad_arg: RefusalCounter::new("L3bDescriptorHeapsBadArg"),
    descriptor_heaps_too_many: RefusalCounter::new("L3bDescriptorHeapsTooMany"),
    descriptor_heap_null_entry: RefusalCounter::new("L3bDescriptorHeapNullEntry"),
    descriptor_heap_missing: RefusalCounter::new("L3bDescriptorHeapMissing"),
    root_signature_null: RefusalCounter::new("L3bRootSignatureNull"),
    root_signature_missing: RefusalCounter::new("L3bRootSignatureMissing"),
    root_table_zero_base: RefusalCounter::new("L3bRootTableZeroBase"),
    root_constants_bad_arg: RefusalCounter::new("L3bRootConstantsBadArg"),
    root_view_null_address: RefusalCounter::new("L3bRootViewNullAddress"),
    clear_bad_arg: RefusalCounter::new("L3bClearBadArg"),
    clear_resource_null: RefusalCounter::new("L3bClearResourceNull"),
    clear_resource_missing: RefusalCounter::new("L3bClearResourceMissing"),
    clear_depth_stencil_flags_unknown: RefusalCounter::new("L3bClearDepthStencilFlagsUnknown"),
    clear_root_arguments_not_forwarded: RefusalCounter::new("L3bClearRootArgumentsNotForwarded"),
};

/// L3b's refusal counters, printed by `crate::log_refusal_summary` at this
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
    &L3B_REFUSALS.command_list_missing,
    &L3B_REFUSALS.set_error_no_device,
    &L3B_REFUSALS.set_error_cb_absent,
    &L3B_REFUSALS.descriptor_heaps_bad_arg,
    &L3B_REFUSALS.descriptor_heaps_too_many,
    &L3B_REFUSALS.descriptor_heap_null_entry,
    &L3B_REFUSALS.descriptor_heap_missing,
    &L3B_REFUSALS.root_signature_null,
    &L3B_REFUSALS.root_signature_missing,
    &L3B_REFUSALS.root_table_zero_base,
    &L3B_REFUSALS.root_constants_bad_arg,
    &L3B_REFUSALS.root_view_null_address,
    &L3B_REFUSALS.clear_bad_arg,
    &L3B_REFUSALS.clear_resource_null,
    &L3B_REFUSALS.clear_resource_missing,
    &L3B_REFUSALS.clear_depth_stencil_flags_unknown,
    &L3B_REFUSALS.clear_root_arguments_not_forwarded,
];
