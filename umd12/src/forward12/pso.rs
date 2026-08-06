//! L6 — pipeline state, root signatures, shaders and immutable sub-state.
//!
//! Owns 38 of `DEVICE_FUNCS_CORE_0109` (`DDI_REFERENCE.md` §3.2 groups (b) 12,
//! (c) 14, (e) 12), split across two chain links. The 14 shader slots of group
//! (c) live in [`super::shaders`]; this file holds groups (b) and (e) and the
//! lane's whole refusal set.
//!
//! # ⭐⭐ The design, in one paragraph
//!
//! D3D12's API folds blend / rasterizer / depth-stencil / element layout into
//! the PSO descriptor, so **there is no engine object for any of them**. The
//! four `pfnCreate*State` slots therefore translate the DDI desc into the
//! matching **API** desc and store it in the runtime-allocated private block;
//! [`create_pipeline_state`] reads them back out by handle and assembles one
//! pipeline (`DDI_REFERENCE.md` §9.9: *"blend / rasterizer / depth-stencil /
//! element-layout are separate driver objects created earlier and referenced by
//! handle; vkd3d wants them inline"*). Only the PSO and the root signature are
//! engine objects, and their handles carry a bare owning COM pointer.
//!
//! # ⛔ The `DepthBias` trap, and how it is resolved
//!
//! `SUBSTRATE.md` §4.5: *"`DepthBias` silently changed from `INT` to `FLOAT` in
//! the DDI rasterizer desc at 0099, and 0102 revs the struct again … at 0110
//! `pfnCreateRasterizerState` receives the 0102 shape, where a `FLOAT DepthBias`
//! sits at the same offset an older `INT` did — a reinterpretation no compiler
//! will flag."* The generated header agrees:
//! `D3D12DDI_RASTERIZER_DESC_0102::DepthBias` is `FLOAT`.
//!
//! ⭐ **Resolved by never converting it.** The API's *legacy*
//! `D3D12_RASTERIZER_DESC.DepthBias` is `INT` — but `D3D12_RASTERIZER_DESC2`'s
//! is `FLOAT`, and vkd3d accepts a `RASTERIZER2` subobject through
//! `ID3D12Device2::CreatePipelineState`
//! (`vkd3d-proton-helios/libs/vkd3d/state.c:2598`). So the DDI's float reaches
//! the engine as a float and there is no rounding to get wrong. See
//! [`GraphicsStream`] for the four other things that choice preserves, one of
//! which — mesh and amplification shaders — is not expressible in the legacy
//! struct at all.
//!
//! # ⚠ Feature level: this lane's half is ready for **12_1**
//!
//! `caps12.rs` ships FL 11_0 as a staging value and `DX12.md` §4.4 makes 12_1
//! the standing target. The two floors 12_1 arms that belong to L6 are
//! **ROVs** and `ConservativeRasterizationTier >= 1`, and both are pure
//! pass-through here: `D3D12DDI_RASTERIZER_DESC_0102::ConservativeRasterizationMode`
//! is forwarded verbatim to `D3D12_RASTERIZER_DESC2::ConservativeRaster`, and
//! ROVs are a shader-model feature that never touches this file — the bytecode
//! is copied, not inspected. ⛔ **This lane does not raise the level**:
//! `caps12.rs` is not its file and the level moves in one commit with all of
//! its floors.

use core::ffi::c_void;
use core::mem::ManuallyDrop;

use helios_umd_common::hr::{Hresult, E_FAIL, E_INVALIDARG, S_OK};
use helios_umd_common::refusals::RefusalCounter;
use helios_umd_common::slot::{Boxed, BoxedHandle, Com, ComHandle, Slot};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::{
    ID3D12Device2, ID3D12PipelineState, ID3D12RootSignature, D3D12_BLEND, D3D12_BLEND_DESC,
    D3D12_BLEND_OP, D3D12_BLEND_OP_ADD, D3D12_BLEND_ONE, D3D12_BLEND_ZERO,
    D3D12_COLOR_WRITE_ENABLE_ALL, D3D12_COMPARISON_FUNC, D3D12_COMPARISON_FUNC_ALWAYS,
    D3D12_COMPARISON_FUNC_LESS, D3D12_COMPUTE_PIPELINE_STATE_DESC,
    D3D12_CONSERVATIVE_RASTERIZATION_MODE, D3D12_CONSERVATIVE_RASTERIZATION_MODE_OFF,
    D3D12_CONSERVATIVE_RASTERIZATION_MODE_ON, D3D12_CULL_MODE, D3D12_CULL_MODE_BACK,
    D3D12_DEFAULT_STENCIL_READ_MASK, D3D12_DEFAULT_STENCIL_WRITE_MASK,
    D3D12_DEPTH_STENCILOP_DESC1, D3D12_DEPTH_STENCIL_DESC2,
    D3D12_DEPTH_WRITE_MASK, D3D12_DEPTH_WRITE_MASK_ALL, D3D12_DESCRIPTOR_RANGE,
    D3D12_DESCRIPTOR_RANGE_TYPE, D3D12_DESCRIPTOR_RANGE_TYPE_SAMPLER, D3D12_FILL_MODE,
    D3D12_FILL_MODE_SOLID, D3D12_FILTER, D3D12_FILTER_ANISOTROPIC,
    D3D12_INDEX_BUFFER_STRIP_CUT_VALUE, D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_0xFFFFFFFF,
    D3D12_INPUT_CLASSIFICATION, D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA,
    D3D12_INPUT_ELEMENT_DESC, D3D12_INPUT_LAYOUT_DESC, D3D12_LINE_RASTERIZATION_MODE,
    D3D12_LINE_RASTERIZATION_MODE_ALIASED, D3D12_LINE_RASTERIZATION_MODE_QUADRILATERAL_NARROW,
    D3D12_LOGIC_OP, D3D12_LOGIC_OP_NOOP, D3D12_PIPELINE_STATE_FLAGS,
    D3D12_PIPELINE_STATE_FLAG_DYNAMIC_DEPTH_BIAS,
    D3D12_PIPELINE_STATE_FLAG_DYNAMIC_INDEX_BUFFER_STRIP_CUT, D3D12_PIPELINE_STATE_STREAM_DESC,
    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE, D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_AS,
    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND, D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2,
    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT,
    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DS, D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_FLAGS,
    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_GS, D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_HS,
    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_IB_STRIP_CUT_VALUE,
    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_INPUT_LAYOUT, D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS,
    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_NODE_MASK, D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY,
    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER2,
    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS,
    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE,
    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC,
    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK,
    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VIEW_INSTANCING,
    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VS, D3D12_PRIMITIVE_TOPOLOGY_TYPE,
    D3D12_PRIMITIVE_TOPOLOGY_TYPE_PATCH, D3D12_RASTERIZER_DESC2, D3D12_RENDER_TARGET_BLEND_DESC,
    D3D12_ROOT_CONSTANTS, D3D12_ROOT_DESCRIPTOR, D3D12_ROOT_DESCRIPTOR_TABLE, D3D12_ROOT_PARAMETER,
    D3D12_ROOT_PARAMETER_0, D3D12_ROOT_PARAMETER_TYPE, D3D12_ROOT_PARAMETER_TYPE_UAV,
    D3D12_ROOT_SIGNATURE_DESC,
    D3D12_ROOT_SIGNATURE_FLAGS, D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT,
    D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED, D3D12_RT_FORMAT_ARRAY,
    D3D12_SHADER_BYTECODE, D3D12_SHADER_VISIBILITY, D3D12_SHADER_VISIBILITY_MESH,
    D3D12_STATIC_BORDER_COLOR, D3D12_STATIC_BORDER_COLOR_OPAQUE_WHITE_UINT,
    D3D12_STATIC_SAMPLER_DESC, D3D12_STENCIL_OP_KEEP, D3D12_TEXTURE_ADDRESS_MODE,
    D3D12_TEXTURE_ADDRESS_MODE_MIRROR_ONCE,
    D3D12_VIEW_INSTANCE_LOCATION, D3D12_VIEW_INSTANCING_DESC, D3D12_VIEW_INSTANCING_FLAGS,
    D3D_ROOT_SIGNATURE_VERSION_1_0,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

use super::shaders;
use super::tables12::{stage, DeviceCoreTable, Filling};
use crate::ddi12;
use crate::{bridge12, device12, log_error, note_refusal, trace_line};

/// How many `log_error!` lines each repeating arm here may emit; per-call detail
/// goes to `trace_line!`, gated on `HKLM\SOFTWARE\Helios!Umd12Trace`.
const LOG_BUDGET: usize = 32;

// ---------------------------------------------------------------------------
// The handle model — payload chosen by the HANDLE TYPE, once
// ---------------------------------------------------------------------------
//
// ⛔ R803 / `ARCHITECTURE.md` §12 rule 7: choosing the payload at the call site
// is what made `load_com::<ID3D11RenderTargetView>(h_rtv)` compile and produce a
// `ManuallyDrop` whose vtable pointer was a struct field — a wild call on first
// use. These two invocations are the only place the choice is made.
//
// ⚠ Every `D3D12DDI_H*` below is a distinct underlying struct: the four D3D10
// aliases (`HSHADER`, `HBLENDSTATE`, `HRASTERIZERSTATE`, `HDEPTHSTENCILSTATE`,
// `HELEMENTLAYOUT`) alias five different `D3D10DDI_*` types and the two engine
// handles are their own, so no two impls here collide.

helios_umd_common::com_handles!(
    crate::ddi12::D3D12DDI_HPIPELINESTATE,
    crate::ddi12::D3D12DDI_HROOTSIGNATURE,
);

helios_umd_common::boxed_handles!(
    crate::ddi12::D3D12DDI_HELEMENTLAYOUT => ElementLayoutState,
    crate::ddi12::D3D12DDI_HBLENDSTATE => BlendState,
    crate::ddi12::D3D12DDI_HDEPTHSTENCILSTATE => DepthStencilState,
    crate::ddi12::D3D12DDI_HRASTERIZERSTATE => RasterizerState,
);

/// The private block behind a `D3D12DDI_HELEMENTLAYOUT`.
///
/// ⚠ `pub` for the same E0446 reason as [`shaders::ShaderState`]: it is named as
/// a public trait's associated type. `forward12` is private, so nothing outside
/// this crate can reach it.
pub struct ElementLayoutState {
    /// The API elements, owned, with `SemanticName` pointing at
    /// [`SEMANTIC_NAME`] — a `'static` NUL-terminated constant, so there is no
    /// second allocation whose lifetime has to be reasoned about.
    pub(crate) elements: Vec<D3D12_INPUT_ELEMENT_DESC>,
}

/// The private block behind a `D3D12DDI_HBLENDSTATE`.
pub struct BlendState {
    pub(crate) desc: D3D12_BLEND_DESC,
}

/// The private block behind a `D3D12DDI_HDEPTHSTENCILSTATE`.
pub struct DepthStencilState {
    pub(crate) desc: D3D12_DEPTH_STENCIL_DESC2,
}

/// The private block behind a `D3D12DDI_HRASTERIZERSTATE`.
pub struct RasterizerState {
    pub(crate) desc: D3D12_RASTERIZER_DESC2,
}

/// ⭐ The fabricated semantic name, shared with [`shaders`]'s synthetic `ISG1`.
///
/// `D3D12DDIARG_INPUT_ELEMENT_DESC` carries an **`InputRegister`** and no name,
/// while `D3D12_INPUT_ELEMENT_DESC` carries a name and an index; vkd3d matches
/// the two by name and then uses the matched signature element's register as the
/// Vulkan vertex-input location (`state.c:5279-5310`, `:5924`). Names are
/// therefore only a matching key between two things this driver writes, and the
/// convention is `umd/src/forward/layout.rs:100-166`'s, unchanged:
/// `TEXCOORD` with the register as the semantic index.
const SEMANTIC_NAME: &[u8] = b"TEXCOORD\0";

/// The slot behind a boxed-payload handle, typed by the handle rather than by
/// this call site.
///
/// # Safety
/// Same precondition as `Slot::from_priv`: the slot must lie inside the private
/// memory the paired `CalcPrivate*Size` sized.
unsafe fn boxed_slot<H: BoxedHandle>(h: H) -> Option<Slot<Boxed<H::State>>> {
    // SAFETY: forwarded; the caller carries the precondition.
    unsafe { Slot::from_priv(h.drv_private()) }
}

/// The slot behind a bare-COM handle.
///
/// ⚠ The *interface* is still named at the call site — which is unavoidable and
/// is not R803's hazard: `ComHandle` already establishes that the word is a COM
/// pointer, so the worst a wrong `T` can do is call the wrong vtable of a real
/// object, not treat a `Box` field as a vtable. Both call sites below name the
/// only interface their slot ever holds.
///
/// # Safety
/// Same precondition as `Slot::from_priv`.
unsafe fn com_slot<H: ComHandle, T: Interface>(h: H) -> Option<Slot<Com<T>>> {
    // SAFETY: forwarded; the caller carries the precondition.
    unsafe { Slot::from_priv(h.drv_private()) }
}

/// Borrow a boxed sub-state payload.
///
/// ⛔ **The D3D12 soundness argument, re-derived and NOT inherited.**
/// `umd_common::slot`'s `Slot::get` returns `&'static S` on an argument that
/// rests on the D3D11 runtime's `CUseCountedObject` ordering, and states in as
/// many words that the equivalent is *"NOT established"* for D3D12;
/// `PARALLEL.md` §9.4 forbids inheriting it. So this does not call `get`. It
/// takes `ptr()` — the route `slot.rs` offers for exactly this case — and
/// carries the lifetime itself, on the same narrower argument
/// `device12::device` uses:
///
/// * the returned reference is **not** `'static`; it borrows for as long as the
///   caller's binding lives, and every caller is one DDI invocation;
/// * the only path that frees the box is the matching `pfnDestroy*State`, and
///   the only reader is [`create_pipeline_state`], which was handed the same
///   handle **by the runtime, in the same call**. A runtime that could destroy a
///   sub-state object concurrently with a `pfnCreatePipelineState` naming it
///   would be freeing an object while passing it as an argument;
/// * ⚠ concurrent reads across free-threaded workers are permitted by `&` and
///   are the expected case. Every payload here is immutable after construction,
///   so there is no interior mutation to protect and deliberately no `&mut`.
///
/// # Safety
/// `h` must be a live handle the matching create stored into, and the returned
/// reference must not outlive the DDI call that obtained it.
unsafe fn boxed_state<'a, H: BoxedHandle>(h: H) -> Option<&'a H::State> {
    // SAFETY: forwarded; the caller carries the precondition.
    let p = unsafe { boxed_slot(h)?.ptr() };
    if p.is_null() {
        return None;
    }
    // SAFETY: non-null per the check, and the argument above establishes that
    // no teardown can overlap this borrow.
    Some(unsafe { &*p })
}

// ── ⚠ L3a's accessor (S6 Round 2, `PARALLEL.md` §4's accessor budget) ───────
//
// ⛔ **Named and monomorphic, and that is the point of it.** `com_slot` above is
// generic over the interface, which is fine *inside* this file where both call
// sites name the only interface their slot ever holds — but exporting a generic
// form would put the payload choice back at a call site in another file, which
// is R803's shape exactly (`ARCHITECTURE.md` §12 rule 7). The payload of
// `D3D12DDI_HPIPELINESTATE` is decided once, here, in the file that owns the
// handle and writes the slot.

/// The engine `ID3D12PipelineState` behind a DDI pipeline-state handle.
///
/// ⭐ `pub(crate)` for exactly one caller: `cmdlist.rs`'s `pfnSetPipelineState`,
/// which is the only DDI outside this file that turns a
/// `D3D12DDI_HPIPELINESTATE` back into an engine object. Same seam, and the same
/// reasoning, as `queue::command_list_state` and `resource12::engine_resource`.
///
/// ⚠ **`ManuallyDrop`, because the slot keeps the reference.** This borrows what
/// [`create_pipeline_state`] stored; dropping the returned value would release
/// the slot's own reference and leave a live handle pointing at a freed object.
/// The wrapper makes that unwritable rather than merely unlikely, and it costs
/// no `AddRef`/`Release` pair on a path an application drives per draw batch.
/// `None` means the slot is empty — i.e. the create refused — which is a case
/// `cmdlist.rs` counts rather than one this function decides.
///
/// # Safety
/// `h_pso`'s `pDrvPrivate` must address the private block
/// `pfnCalcPrivatePipelineStateSize` sized, and the returned value must not
/// outlive the DDI call that obtained it.
pub(crate) unsafe fn engine_pipeline_state(
    h_pso: ddi12::D3D12DDI_HPIPELINESTATE,
) -> Option<ManuallyDrop<ID3D12PipelineState>> {
    // SAFETY: forwarded; the caller carries `Slot::from_priv`'s precondition.
    let slot = unsafe { com_slot::<_, ID3D12PipelineState>(h_pso) }?;
    // SAFETY: as above. `load` reads the word and reports an empty slot as
    // `None` rather than fabricating a reference.
    unsafe { slot.load() }
}

// ── end of L3a's accessor ──────────────────────────────────────────────────

/// Null a boxed handle's slot word without touching what it pointed at.
///
/// ⛔ Every create runs this first, before anything can fail: a failed create
/// must leave a null handle, never stale garbage (`DDI_REFERENCE.md` §7.1, and
/// `umd/src/forward/shaders.rs:68-100`'s `clear_handle` discipline).
///
/// # Safety
/// Same precondition as `Slot::from_priv`.
unsafe fn clear_boxed<H: BoxedHandle>(h: H) {
    // SAFETY: forwarded; clearing touches only the word.
    if let Some(slot) = unsafe { boxed_slot(h) } {
        // SAFETY: as above.
        unsafe { slot.clear() };
    }
}

/// Null a bare-COM handle's slot word.
///
/// # Safety
/// Same precondition as `Slot::from_priv`.
unsafe fn clear_com<H: ComHandle>(h: H) {
    // SAFETY: forwarded; `Slot::clear` is payload-agnostic, so naming
    // `IUnknown` here selects no behaviour — it only satisfies the type
    // parameter, and no reference is loaded or released.
    if let Some(slot) = unsafe { com_slot::<H, windows::core::IUnknown>(h) } {
        // SAFETY: as above.
        unsafe { slot.clear() };
    }
}

/// Report a device-scope failure, counting the case where the runtime gave this
/// device no way to hear it.
///
/// ⭐ Most D3D12 DDIs return `VOID` (`DECISIONS.md` §7.6), so this is the only
/// channel the four sub-state creates and the nine shader creates have.
/// [`device12::set_error`] is the shared half; the counter is L6's, because
/// `PARALLEL.md` §9.1 puts a lane's counters in the lane's file and eleven lanes
/// will call the same helper.
///
/// # Safety
/// `h_device` must be the device handle the DDI being served was called with.
pub(crate) unsafe fn set_error_if_possible(h_device: ddi12::D3D12DDI_HDEVICE, hr: Hresult) {
    // SAFETY: the caller guarantees a live device handle from `create_device`.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&L6_REFUSALS.set_error_no_device);
        return;
    };
    // ⚠ Safe: `device12::set_error` takes `&HeliosD3D12Device`, and the validity
    // of its `um_callbacks` is a type invariant of that struct rather than a
    // caller obligation. L6 submitted it as an `unsafe fn`; the integrator merged
    // the safe shape two other lanes independently arrived at.
    if !device12::set_error(dev, hr) {
        note_refusal(&L6_REFUSALS.set_error_cb_absent);
    }
}

// ---------------------------------------------------------------------------
// ⭐ The enum identity proofs
// ---------------------------------------------------------------------------
//
// Every state translation below casts a `D3D12DDI_*` enumerator straight into
// its `D3D12_*` API counterpart. That is only correct because the two
// enumerations are value-identical, and `ARCHITECTURE.md` §12 rule 1 forbids
// taking that on trust: each assertion compares **two generated constants**,
// one out of `d3d12umddi.h` via bindgen and one out of the Win32 metadata via
// the `windows` crate. Neither side is transcribed, and a header revision that
// renumbered either one would fail the host cross-check rather than mis-render
// a frame.
//
// ⚠ One representative enumerator per enum, chosen to be a NON-zero, non-first
// value — a table that agreed only at 0 would pass a first-value check and be
// wrong everywhere else.

const _: () = assert!(
    ddi12::D3D12DDI_FILL_MODE_D3D12DDI_FILL_MODE_SOLID == D3D12_FILL_MODE_SOLID.0,
    "D3D12DDI_FILL_MODE must be value-identical to D3D12_FILL_MODE"
);
const _: () = assert!(
    ddi12::D3D12DDI_CULL_MODE_D3D12DDI_CULL_MODE_BACK == D3D12_CULL_MODE_BACK.0,
    "D3D12DDI_CULL_MODE must be value-identical to D3D12_CULL_MODE"
);
const _: () = assert!(
    ddi12::D3D12DDI_CONSERVATIVE_RASTERIZATION_MODE_D3D12DDI_CONSERVATIVE_RASTERIZATION_MODE_ON
        == D3D12_CONSERVATIVE_RASTERIZATION_MODE_ON.0,
    "D3D12DDI_CONSERVATIVE_RASTERIZATION_MODE must be value-identical to the API enum"
);
const _: () = assert!(
    ddi12::D3D12DDI_LINE_RASTERIZATION_MODE_D3D12DDI_LINE_RASTERIZATION_MODE_QUADRILATERAL_NARROW
        == D3D12_LINE_RASTERIZATION_MODE_QUADRILATERAL_NARROW.0,
    "D3D12DDI_LINE_RASTERIZATION_MODE must be value-identical to D3D12_LINE_RASTERIZATION_MODE"
);
// ⚠ The line above was `D3D12_LINE_RASTERIZATION_MODE(3).0` — a hand-transcribed
// literal, and the ONE assertion of the 21 below that compared this DDI enumerator
// against a number instead of against the API's own generated constant. It asserted
// `DDI_QUADRILATERAL_NARROW == 3` and never compared the two enumerations at all,
// inside the block whose own preamble is *"neither side is transcribed"*.
// `ARCHITECTURE.md` §12 rule 1 in the one place a reader would never look for it.
// The named constant existed the whole time
// (`windows-0.58.0/.../Direct3D12/mod.rs:4778`) and its sibling was already
// imported two lines up.
const _: () = assert!(
    ddi12::D3D12DDI_BLEND_D3D12DDI_BLEND_ZERO == D3D12_BLEND_ZERO.0
        && ddi12::D3D12DDI_BLEND_D3D12DDI_BLEND_ONE == D3D12_BLEND_ONE.0,
    "D3D12DDI_BLEND must be value-identical to D3D12_BLEND"
);
const _: () = assert!(
    ddi12::D3D12DDI_BLEND_OP_D3D12DDI_BLEND_OP_ADD == D3D12_BLEND_OP_ADD.0,
    "D3D12DDI_BLEND_OP must be value-identical to D3D12_BLEND_OP"
);
const _: () = assert!(
    ddi12::D3D12DDI_LOGIC_OP_D3D12DDI_LOGIC_OP_NOOP == D3D12_LOGIC_OP_NOOP.0,
    "D3D12DDI_LOGIC_OP must be value-identical to D3D12_LOGIC_OP"
);
const _: () = assert!(
    ddi12::D3D12DDI_COMPARISON_FUNC_D3D12DDI_COMPARISON_FUNC_ALWAYS
        == D3D12_COMPARISON_FUNC_ALWAYS.0,
    "D3D12DDI_COMPARISON_FUNC must be value-identical to D3D12_COMPARISON_FUNC"
);
const _: () = assert!(
    ddi12::D3D12DDI_DEPTH_WRITE_MASK_D3D12DDI_DEPTH_WRITE_MASK_ALL == D3D12_DEPTH_WRITE_MASK_ALL.0,
    "D3D12DDI_DEPTH_WRITE_MASK must be value-identical to D3D12_DEPTH_WRITE_MASK"
);
const _: () = assert!(
    ddi12::D3D12DDI_STENCIL_OP_D3D12DDI_STENCIL_OP_DECR
        == windows::Win32::Graphics::Direct3D12::D3D12_STENCIL_OP_DECR.0,
    "D3D12DDI_STENCIL_OP must be value-identical to D3D12_STENCIL_OP"
);
const _: () = assert!(
    ddi12::D3D12DDI_INPUT_CLASSIFICATION_D3D12DDI_INPUT_CLASSIFICIATION_PER_INSTANCE_DATA
        == D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA.0,
    "D3D12DDI_INPUT_CLASSIFICATION must be value-identical to D3D12_INPUT_CLASSIFICATION"
);
const _: () = assert!(
    ddi12::D3D12DDI_PRIMITIVE_TOPOLOGY_TYPE_D3D12DDI_PRIMITIVE_TOPOLOGY_TYPE_PATCH
        == D3D12_PRIMITIVE_TOPOLOGY_TYPE_PATCH.0,
    "D3D12DDI_PRIMITIVE_TOPOLOGY_TYPE must be value-identical to the API enum"
);
const _: () = assert!(
    ddi12::D3D12DDI_INDEX_BUFFER_STRIP_CUT_VALUE_D3D12DDI_INDEX_BUFFER_STRIP_CUT_VALUE_0xFFFFFFFF
        == D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_0xFFFFFFFF.0,
    "D3D12DDI_INDEX_BUFFER_STRIP_CUT_VALUE must be value-identical to the API enum"
);
const _: () = assert!(
    ddi12::D3D12DDI_PIPELINE_STATE_FLAGS_D3D12DDI_PIPELINE_STATE_FLAG_DYNAMIC_DEPTH_BIAS
        == D3D12_PIPELINE_STATE_FLAG_DYNAMIC_DEPTH_BIAS.0
        && ddi12::D3D12DDI_PIPELINE_STATE_FLAGS_D3D12DDI_PIPELINE_STATE_FLAG_DYNAMIC_INDEX_BUFFER_STRIP_CUT
            == D3D12_PIPELINE_STATE_FLAG_DYNAMIC_INDEX_BUFFER_STRIP_CUT.0,
    "D3D12DDI_PIPELINE_STATE_FLAGS must be value-identical to D3D12_PIPELINE_STATE_FLAGS"
);
const _: () = assert!(
    ddi12::D3D12DDI_ROOT_PARAMETER_TYPE_D3D12DDI_ROOT_PARAMETER_TYPE_UAV
        == D3D12_ROOT_PARAMETER_TYPE_UAV.0,
    "D3D12DDI_ROOT_PARAMETER_TYPE must be value-identical to D3D12_ROOT_PARAMETER_TYPE"
);
const _: () = assert!(
    ddi12::D3D12DDI_DESCRIPTOR_RANGE_TYPE_D3D12DDI_DESCRIPTOR_RANGE_TYPE_SAMPLER
        == D3D12_DESCRIPTOR_RANGE_TYPE_SAMPLER.0,
    "D3D12DDI_DESCRIPTOR_RANGE_TYPE must be value-identical to D3D12_DESCRIPTOR_RANGE_TYPE"
);
const _: () = assert!(
    ddi12::D3D12DDI_SHADER_VISIBILITY_D3D12DDI_SHADER_VISIBILITY_MESH
        == D3D12_SHADER_VISIBILITY_MESH.0,
    "D3D12DDI_SHADER_VISIBILITY must be value-identical to D3D12_SHADER_VISIBILITY"
);
const _: () = assert!(
    ddi12::D3D12DDI_FILTER_D3D12DDI_FILTER_ANISOTROPIC == D3D12_FILTER_ANISOTROPIC.0,
    "D3D12DDI_FILTER must be value-identical to D3D12_FILTER"
);
const _: () = assert!(
    ddi12::D3D12DDI_TEXTURE_ADDRESS_MODE_D3D12DDI_TEXTURE_ADDRESS_MODE_MIRRORONCE
        == D3D12_TEXTURE_ADDRESS_MODE_MIRROR_ONCE.0,
    "D3D12DDI_TEXTURE_ADDRESS_MODE must be value-identical to D3D12_TEXTURE_ADDRESS_MODE"
);
const _: () = assert!(
    ddi12::D3D12DDI_STATIC_BORDER_COLOR_D3D12DDI_STATIC_BORDER_COLOR_OPAQUE_WHITE_UINT
        == D3D12_STATIC_BORDER_COLOR_OPAQUE_WHITE_UINT.0,
    "D3D12DDI_STATIC_BORDER_COLOR must be value-identical to D3D12_STATIC_BORDER_COLOR"
);
const _: () = assert!(
    ddi12::D3D12DDI_ROOT_SIGNATURE_FLAGS_D3D12DDI_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT
        == D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT.0
        && ddi12::D3D12DDI_ROOT_SIGNATURE_FLAGS_D3D12DDI_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED
            == D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED.0,
    "D3D12DDI_ROOT_SIGNATURE_FLAGS must be value-identical to D3D12_ROOT_SIGNATURE_FLAGS"
);

// ---------------------------------------------------------------------------
// (b) Immutable pipeline sub-state — 12 slots, 4 x calc/create/destroy
// ---------------------------------------------------------------------------

/// The private size every sub-state object needs: one machine word holding the
/// `Box` this driver allocates.
///
/// ⭐ The same answer the D3D11 driver gives for its four sub-state slots
/// (`umd/src/forward/state_objects.rs:9-20`, `:154-160`, all `-> 8`). The
/// translated desc is boxed rather than written in place because the element
/// layout's is variable-length and `CalcPrivate*Size` is answered before the
/// element count is known to be sane.
fn sub_state_private_size() -> ddi12::SIZE_T {
    core::mem::size_of::<*mut c_void>() as ddi12::SIZE_T
}

/// Count a create that was handed a `LibraryReference`.
///
/// ⚠ Every sub-state desc, every shader arg and the PSO arg carry a
/// `D3D12DDI_LIBRARY_REFERENCE_0010 { hLibrary, PipelineIndex }`. A non-null
/// `hLibrary` means the runtime is asking this object to come out of a
/// **pipeline library**, which this lane refuses wholesale (see
/// [`create_pipeline_library`]). Counted here so the refusal upstream is
/// visible at the objects it affects, not only at the library itself.
fn note_library_reference(reference: &ddi12::D3D12DDI_LIBRARY_REFERENCE_0010) {
    if !reference.hLibrary.pDrvPrivate.is_null() {
        note_refusal(&L6_REFUSALS.library_reference_ignored);
    }
}

/// `pfnCalcPrivateElementLayoutSize`.
///
/// # Safety
/// Trivially safe: no argument is dereferenced. `unsafe` because the DDI typedef
/// is.
unsafe extern "C" fn calc_private_element_layout_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _arg: *const ddi12::D3D12DDIARG_CREATEELEMENTLAYOUT_0010,
) -> ddi12::SIZE_T {
    sub_state_private_size()
}

/// The largest input layout this driver will translate.
///
/// D3D12 allows 32 input slots and at most `D3D12_IA_VERTEX_INPUT_STRUCTURE_ELEMENT_COUNT`
/// (32) elements; 512 is generous, and the bound exists so the `Vec` below
/// cannot be sized from an unbounded runtime count.
const MAX_INPUT_ELEMENTS: usize = 512;

/// `pfnCreateElementLayout`.
///
/// ⭐ **No engine object.** D3D12 has no `ID3D12InputLayout`; the layout is a
/// field of the pipeline descriptor. So this translates and stores, and
/// [`create_pipeline_state`] reads it back.
///
/// # Safety
/// `arg`, when non-null, must point at a live `D3D12DDIARG_CREATEELEMENTLAYOUT_0010`
/// whose `pVertexElements` addresses `NumElements` live descs, and `h_layout`
/// must carry the machine word `pfnCalcPrivateElementLayoutSize` sized.
unsafe extern "C" fn create_element_layout(
    h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATEELEMENTLAYOUT_0010,
    h_layout: ddi12::D3D12DDI_HELEMENTLAYOUT,
) {
    // SAFETY: the caller guarantees the slot word; clearing touches only it.
    unsafe { clear_boxed(h_layout) };
    if arg.is_null() {
        note_refusal(&L6_REFUSALS.sub_state_bad_arg);
        // SAFETY: `h_device` is this DDI's device handle.
        unsafe { set_error_if_possible(h_device, E_INVALIDARG) };
        return;
    }
    // SAFETY: non-null per the check; the DDI declares it `_In_ CONST`.
    let a = unsafe { &*arg };
    note_library_reference(&a.LibraryReference);

    let count = a.NumElements as usize;
    if count > MAX_INPUT_ELEMENTS || (count != 0 && a.pVertexElements.is_null()) {
        note_refusal(&L6_REFUSALS.sub_state_bad_arg);
        log_error!("CreateElementLayout: refusing NumElements={count}");
        // SAFETY: as above.
        unsafe { set_error_if_possible(h_device, E_INVALIDARG) };
        return;
    }
    // SAFETY: `count` is bounded above and `pVertexElements` is non-null
    // whenever it is non-zero; the runtime declares the array `_In_` for the
    // call.
    let src = if count == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(a.pVertexElements, count) }
    };

    let mut elements = Vec::with_capacity(count);
    for e in src {
        elements.push(D3D12_INPUT_ELEMENT_DESC {
            // ⭐ The fabricated key. `SEMANTIC_NAME` is `'static`, so this
            // pointer stays valid for the whole process and there is no second
            // allocation to keep alive alongside `elements`.
            SemanticName: windows::core::PCSTR(SEMANTIC_NAME.as_ptr()),
            SemanticIndex: e.InputRegister,
            Format: DXGI_FORMAT(e.Format),
            InputSlot: e.InputSlot,
            AlignedByteOffset: e.AlignedByteOffset,
            InputSlotClass: D3D12_INPUT_CLASSIFICATION(e.InputSlotClass),
            InstanceDataStepRate: e.InstanceDataStepRate,
        });
    }
    trace_line!("CreateElementLayout: {count} element(s)");

    let Some(slot) = (
        // SAFETY: the caller guarantees the runtime-allocated word.
        unsafe { boxed_slot(h_layout) }
    ) else {
        note_refusal(&L6_REFUSALS.sub_state_bad_arg);
        // SAFETY: as above.
        unsafe { set_error_if_possible(h_device, E_INVALIDARG) };
        return;
    };
    // SAFETY: the slot is the word the paired calc-size sized and was cleared
    // above, so nothing is overwritten.
    unsafe { slot.store(ElementLayoutState { elements }) };
}

/// `pfnDestroyElementLayout`.
///
/// # Safety
/// `h_layout` must be a handle [`create_element_layout`] stored into, destroyed
/// at most once.
unsafe extern "C" fn destroy_element_layout(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_layout: ddi12::D3D12DDI_HELEMENTLAYOUT,
) {
    // SAFETY: the caller guarantees a live slot; `take` empties it, so a second
    // destroy finds `None` rather than double-freeing.
    if let Some(slot) = unsafe { boxed_slot(h_layout) } {
        // SAFETY: as above.
        drop(unsafe { slot.take() });
    }
}

/// `pfnCalcPrivateBlendStateSize`.
///
/// # Safety
/// As [`calc_private_element_layout_size`].
unsafe extern "C" fn calc_private_blend_state_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _desc: *const ddi12::D3D12DDI_BLEND_DESC_0010,
) -> ddi12::SIZE_T {
    sub_state_private_size()
}

/// `pfnCreateBlendState`.
///
/// # Safety
/// `desc`, when non-null, must point at a live `D3D12DDI_BLEND_DESC_0010`, and
/// `h_blend` must carry the machine word the paired calc-size sized.
unsafe extern "C" fn create_blend_state(
    h_device: ddi12::D3D12DDI_HDEVICE,
    desc: *const ddi12::D3D12DDI_BLEND_DESC_0010,
    h_blend: ddi12::D3D12DDI_HBLENDSTATE,
) {
    // SAFETY: the caller guarantees the slot word.
    unsafe { clear_boxed(h_blend) };
    if desc.is_null() {
        note_refusal(&L6_REFUSALS.sub_state_bad_arg);
        // SAFETY: `h_device` is this DDI's device handle.
        unsafe { set_error_if_possible(h_device, E_INVALIDARG) };
        return;
    }
    // SAFETY: non-null per the check; `_In_ CONST`.
    let d = unsafe { &*desc };
    note_library_reference(&d.LibraryReference);

    // ⚠ The eight render-target entries are translated field by field rather
    // than `transmute`d. The two structs happen to agree today, and that is
    // exactly the kind of agreement `ARCHITECTURE.md` §12 rule 1 says not to
    // encode positionally: a field inserted on either side is a silent
    // transposition, while a named copy is a compile error.
    let mut render_target = [D3D12_RENDER_TARGET_BLEND_DESC::default(); 8];
    for (out, src) in render_target.iter_mut().zip(d.RenderTarget.iter()) {
        *out = D3D12_RENDER_TARGET_BLEND_DESC {
            BlendEnable: windows::Win32::Foundation::BOOL(src.BlendEnable),
            LogicOpEnable: windows::Win32::Foundation::BOOL(src.LogicOpEnable),
            SrcBlend: D3D12_BLEND(src.SrcBlend),
            DestBlend: D3D12_BLEND(src.DestBlend),
            BlendOp: D3D12_BLEND_OP(src.BlendOp),
            SrcBlendAlpha: D3D12_BLEND(src.SrcBlendAlpha),
            DestBlendAlpha: D3D12_BLEND(src.DestBlendAlpha),
            BlendOpAlpha: D3D12_BLEND_OP(src.BlendOpAlpha),
            LogicOp: D3D12_LOGIC_OP(src.LogicOp),
            RenderTargetWriteMask: src.RenderTargetWriteMask,
        };
    }
    let state = BlendState {
        desc: D3D12_BLEND_DESC {
            AlphaToCoverageEnable: windows::Win32::Foundation::BOOL(d.AlphaToCoverageEnable),
            IndependentBlendEnable: windows::Win32::Foundation::BOOL(d.IndependentBlendEnable),
            RenderTarget: render_target,
        },
    };
    trace_line!(
        "CreateBlendState: a2c={} independent={} rt0_enable={}",
        d.AlphaToCoverageEnable,
        d.IndependentBlendEnable,
        d.RenderTarget[0].BlendEnable,
    );

    let Some(slot) = (
        // SAFETY: the caller guarantees the runtime-allocated word.
        unsafe { boxed_slot(h_blend) }
    ) else {
        note_refusal(&L6_REFUSALS.sub_state_bad_arg);
        // SAFETY: as above.
        unsafe { set_error_if_possible(h_device, E_INVALIDARG) };
        return;
    };
    // SAFETY: the slot is the word the calc-size sized and was cleared above.
    unsafe { slot.store(state) };
}

/// `pfnDestroyBlendState`.
///
/// # Safety
/// As [`destroy_element_layout`].
unsafe extern "C" fn destroy_blend_state(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_blend: ddi12::D3D12DDI_HBLENDSTATE,
) {
    // SAFETY: as `destroy_element_layout`.
    if let Some(slot) = unsafe { boxed_slot(h_blend) } {
        // SAFETY: as above.
        drop(unsafe { slot.take() });
    }
}

/// `pfnCalcPrivateDepthStencilStateSize`.
///
/// # Safety
/// As [`calc_private_element_layout_size`].
unsafe extern "C" fn calc_private_depth_stencil_state_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _desc: *const ddi12::D3D12DDI_DEPTH_STENCIL_DESC_0095,
) -> ddi12::SIZE_T {
    sub_state_private_size()
}

/// Translate one DDI stencil-op block plus its per-face masks.
fn stencil_op(
    src: &ddi12::D3D12DDI_DEPTH_STENCILOP_DESC,
    read_mask: u8,
    write_mask: u8,
) -> D3D12_DEPTH_STENCILOP_DESC1 {
    D3D12_DEPTH_STENCILOP_DESC1 {
        StencilFailOp: windows::Win32::Graphics::Direct3D12::D3D12_STENCIL_OP(src.StencilFailOp),
        StencilDepthFailOp: windows::Win32::Graphics::Direct3D12::D3D12_STENCIL_OP(
            src.StencilDepthFailOp,
        ),
        StencilPassOp: windows::Win32::Graphics::Direct3D12::D3D12_STENCIL_OP(src.StencilPassOp),
        StencilFunc: D3D12_COMPARISON_FUNC(src.StencilFunc),
        StencilReadMask: read_mask,
        StencilWriteMask: write_mask,
    }
}

/// `pfnCreateDepthStencilState`.
///
/// ⭐ Translates to `D3D12_DEPTH_STENCIL_DESC2`, not the legacy struct, because
/// `_0095` carries three things the legacy one cannot hold:
/// `DepthBoundsTestEnable`, and **separate** front/back stencil read and write
/// masks. Losing either is silently wrong output rather than a failure, which is
/// the class of defect this project has paid for repeatedly.
///
/// ⚠ `FrontEnable` / `BackEnable` have no D3D12 counterpart at any API version —
/// `D3D12_DEPTH_STENCIL_DESC2` has `StencilEnable` alone. They are folded, and
/// the disagreement is counted rather than assumed away.
///
/// # Safety
/// `desc`, when non-null, must point at a live `D3D12DDI_DEPTH_STENCIL_DESC_0095`,
/// and `h_ds` must carry the machine word the paired calc-size sized.
unsafe extern "C" fn create_depth_stencil_state(
    h_device: ddi12::D3D12DDI_HDEVICE,
    desc: *const ddi12::D3D12DDI_DEPTH_STENCIL_DESC_0095,
    h_ds: ddi12::D3D12DDI_HDEPTHSTENCILSTATE,
) {
    // SAFETY: the caller guarantees the slot word.
    unsafe { clear_boxed(h_ds) };
    if desc.is_null() {
        note_refusal(&L6_REFUSALS.sub_state_bad_arg);
        // SAFETY: `h_device` is this DDI's device handle.
        unsafe { set_error_if_possible(h_device, E_INVALIDARG) };
        return;
    }
    // SAFETY: non-null per the check; `_In_ CONST`.
    let d = unsafe { &*desc };
    note_library_reference(&d.LibraryReference);

    // ⚠ `StencilEnable` is the only enable D3D12 has. If the runtime ever
    // disagrees with it per face, the per-face ops this driver forwards are
    // applied where the runtime asked for them not to be. Expected 0; a hit is
    // a real finding about what the runtime puts in these two fields, and this
    // is the only place it could be observed.
    if d.FrontEnable != d.StencilEnable || d.BackEnable != d.StencilEnable {
        note_refusal(&L6_REFUSALS.depth_stencil_face_enable_folded);
    }

    let state = DepthStencilState {
        desc: D3D12_DEPTH_STENCIL_DESC2 {
            DepthEnable: windows::Win32::Foundation::BOOL(d.DepthEnable),
            DepthWriteMask: D3D12_DEPTH_WRITE_MASK(d.DepthWriteMask),
            DepthFunc: D3D12_COMPARISON_FUNC(d.DepthFunc),
            StencilEnable: windows::Win32::Foundation::BOOL(d.StencilEnable),
            FrontFace: stencil_op(
                &d.FrontFace,
                d.FrontFaceStencilReadMask,
                d.FrontFaceStencilWriteMask,
            ),
            BackFace: stencil_op(
                &d.BackFace,
                d.BackFaceStencilReadMask,
                d.BackFaceStencilWriteMask,
            ),
            DepthBoundsTestEnable: windows::Win32::Foundation::BOOL(d.DepthBoundsTestEnable),
        },
    };
    trace_line!(
        "CreateDepthStencilState: depth={} write={} func={} stencil={} bounds={}",
        d.DepthEnable,
        d.DepthWriteMask,
        d.DepthFunc,
        d.StencilEnable,
        d.DepthBoundsTestEnable,
    );

    let Some(slot) = (
        // SAFETY: the caller guarantees the runtime-allocated word.
        unsafe { boxed_slot(h_ds) }
    ) else {
        note_refusal(&L6_REFUSALS.sub_state_bad_arg);
        // SAFETY: as above.
        unsafe { set_error_if_possible(h_device, E_INVALIDARG) };
        return;
    };
    // SAFETY: the slot is the word the calc-size sized and was cleared above.
    unsafe { slot.store(state) };
}

/// `pfnDestroyDepthStencilState`.
///
/// # Safety
/// As [`destroy_element_layout`].
unsafe extern "C" fn destroy_depth_stencil_state(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_ds: ddi12::D3D12DDI_HDEPTHSTENCILSTATE,
) {
    // SAFETY: as `destroy_element_layout`.
    if let Some(slot) = unsafe { boxed_slot(h_ds) } {
        // SAFETY: as above.
        drop(unsafe { slot.take() });
    }
}

/// `pfnCalcPrivateRasterizerStateSize`.
///
/// # Safety
/// As [`calc_private_element_layout_size`].
unsafe extern "C" fn calc_private_rasterizer_state_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _desc: *const ddi12::D3D12DDI_RASTERIZER_DESC_0102,
) -> ddi12::SIZE_T {
    sub_state_private_size()
}

/// `pfnCreateRasterizerState`.
///
/// ⛔ **This is the `DepthBias` slot.** `D3D12DDI_RASTERIZER_DESC_0102::DepthBias`
/// is a `FLOAT` sitting where a pre-0099 `INT` sat, and nothing in the type
/// system connects the two (`SUBSTRATE.md` §4.5). The resolution is in the
/// module doc and it is structural: the value is copied into
/// `D3D12_RASTERIZER_DESC2::DepthBias`, which is **also** a `FLOAT`, so there is
/// no conversion in this driver at all. The legacy `D3D12_RASTERIZER_DESC`, with
/// its `INT DepthBias`, is never constructed anywhere in this crate.
///
/// ⚠ `ScissorEnable` has no counterpart in any `D3D12_RASTERIZER_DESC*`: D3D12
/// makes the scissor test unconditional and supplies rectangles as command-list
/// state. A `FALSE` is counted — see [`L6Refusals::rasterizer_scissor_disabled`].
///
/// # Safety
/// `desc`, when non-null, must point at a live `D3D12DDI_RASTERIZER_DESC_0102`,
/// and `h_rs` must carry the machine word the paired calc-size sized.
unsafe extern "C" fn create_rasterizer_state(
    h_device: ddi12::D3D12DDI_HDEVICE,
    desc: *const ddi12::D3D12DDI_RASTERIZER_DESC_0102,
    h_rs: ddi12::D3D12DDI_HRASTERIZERSTATE,
) {
    // SAFETY: the caller guarantees the slot word.
    unsafe { clear_boxed(h_rs) };
    if desc.is_null() {
        note_refusal(&L6_REFUSALS.sub_state_bad_arg);
        // SAFETY: `h_device` is this DDI's device handle.
        unsafe { set_error_if_possible(h_device, E_INVALIDARG) };
        return;
    }
    // SAFETY: non-null per the check; `_In_ CONST`.
    let d = unsafe { &*desc };
    note_library_reference(&d.LibraryReference);

    if d.ScissorEnable == 0 {
        note_refusal(&L6_REFUSALS.rasterizer_scissor_disabled);
    }

    let state = RasterizerState {
        desc: D3D12_RASTERIZER_DESC2 {
            FillMode: D3D12_FILL_MODE(d.FillMode),
            CullMode: D3D12_CULL_MODE(d.CullMode),
            FrontCounterClockwise: windows::Win32::Foundation::BOOL(d.FrontCounterClockwise),
            // ⛔ FLOAT in, FLOAT out. See the doc comment above.
            DepthBias: d.DepthBias,
            DepthBiasClamp: d.DepthBiasClamp,
            SlopeScaledDepthBias: d.SlopeScaledDepthBias,
            DepthClipEnable: windows::Win32::Foundation::BOOL(d.DepthClipEnable),
            LineRasterizationMode: D3D12_LINE_RASTERIZATION_MODE(d.LineRasterizationMode),
            ForcedSampleCount: d.ForcedSampleCount,
            ConservativeRaster: D3D12_CONSERVATIVE_RASTERIZATION_MODE(
                d.ConservativeRasterizationMode,
            ),
        },
    };
    trace_line!(
        "CreateRasterizerState: fill={} cull={} ccw={} bias={} clamp={} slope={} clip={} \
         scissor={} line_mode={} forced_samples={} conservative={}",
        d.FillMode,
        d.CullMode,
        d.FrontCounterClockwise,
        d.DepthBias,
        d.DepthBiasClamp,
        d.SlopeScaledDepthBias,
        d.DepthClipEnable,
        d.ScissorEnable,
        d.LineRasterizationMode,
        d.ForcedSampleCount,
        d.ConservativeRasterizationMode,
    );

    let Some(slot) = (
        // SAFETY: the caller guarantees the runtime-allocated word.
        unsafe { boxed_slot(h_rs) }
    ) else {
        note_refusal(&L6_REFUSALS.sub_state_bad_arg);
        // SAFETY: as above.
        unsafe { set_error_if_possible(h_device, E_INVALIDARG) };
        return;
    };
    // SAFETY: the slot is the word the calc-size sized and was cleared above.
    unsafe { slot.store(state) };
}

/// `pfnDestroyRasterizerState`.
///
/// # Safety
/// As [`destroy_element_layout`].
unsafe extern "C" fn destroy_rasterizer_state(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_rs: ddi12::D3D12DDI_HRASTERIZERSTATE,
) {
    // SAFETY: as `destroy_element_layout`.
    if let Some(slot) = unsafe { boxed_slot(h_rs) } {
        // SAFETY: as above.
        drop(unsafe { slot.take() });
    }
}

// ---------------------------------------------------------------------------
// (e) Root signatures — 3 slots
// ---------------------------------------------------------------------------

/// The largest root signature this driver will re-serialize.
///
/// ⚠ `DDI_REFERENCE.md` §9.9 constraint 3: *"a driver must accept root
/// signatures larger than the 64-DWORD API limit — up to 128 DWORDs — because
/// the OS injects its own root parameters for shader instrumentation."* A root
/// parameter costs at least one DWORD, so 128 is the true parameter ceiling and
/// 256 is a bound that cannot reject a legal signature while still stopping an
/// unbounded allocation.
const MAX_ROOT_PARAMETERS: usize = 256;

/// `D3D12_MAX_STATIC_SAMPLERS` is 2032; 4096 is the same kind of bound.
const MAX_STATIC_SAMPLERS: usize = 4096;

/// One root parameter may name at most this many descriptor ranges.
const MAX_DESCRIPTOR_RANGES: usize = 4096;

/// `pfnCalcPrivateRootSignatureSize`.
///
/// One machine word: the slot holds a bare owning `ID3D12RootSignature*`.
///
/// # Safety
/// As [`calc_private_element_layout_size`].
unsafe extern "C" fn calc_private_root_signature_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _arg: *const ddi12::D3D12DDIARG_CREATE_ROOT_SIGNATURE_0100,
) -> ddi12::SIZE_T {
    core::mem::size_of::<*mut c_void>() as ddi12::SIZE_T
}

/// The 1.0 API form of one DDI root signature, with the arrays its pointers
/// address kept alive alongside it.
///
/// ⚠ `desc` holds raw pointers into `ranges` and the two vectors below, so this
/// struct exists purely to make the borrow one object with one lifetime rather
/// than four locals a future edit could reorder.
struct RootSignature10 {
    desc: D3D12_ROOT_SIGNATURE_DESC,
    /// ⛔ Never resized after `parameters` is built: `D3D12_ROOT_DESCRIPTOR_TABLE`
    /// entries point into the inner `Vec`s' buffers, and a reallocation of the
    /// OUTER vector would move the inner `Vec` headers but not their buffers —
    /// which is why only the outer one has a stability requirement, and why it
    /// is stated here rather than assumed.
    _ranges: Vec<Vec<D3D12_DESCRIPTOR_RANGE>>,
    _parameters: Vec<D3D12_ROOT_PARAMETER>,
    _samplers: Vec<D3D12_STATIC_SAMPLER_DESC>,
}

/// Down-convert the DDI's 1.1/1.2-shaped root signature into the 1.0 API struct
/// the engine's serializer takes.
///
/// ⛔ **What is lost, and why that is acceptable.** The bridged engine export is
/// `vkd3d_serialize_root_signature`, which **rejects any version but 1.0**
/// outright (`libs/vkd3d/vkd3d_main.c:464-468`), so 1.0 is not a shortcut — it
/// is the only shape reachable through the entry point `D12-G1`'s third arm
/// proved. Down-converting drops exactly three things:
///
/// * `D3D12DDI_DESCRIPTOR_RANGE_0013::Flags` and
///   `D3D12DDI_ROOT_DESCRIPTOR_0013::Flags`. vkd3d's 1.0 deserializer supplies
///   `DESCRIPTORS_VOLATILE | DATA_VOLATILE` for every range and `DATA_VOLATILE`
///   for every root descriptor (`libs/vkd3d-shader/dxbc.c:355-359`) — the
///   **most conservative** interpretation, and a strict superset of what any
///   `STATIC` flag promises. So the loss is optimisation, not correctness, and
///   `DDI_REFERENCE.md` §9.9 constraint 1 already forbids reconstructing app
///   intent from the defaults the runtime filled in;
/// * `D3D12DDI_STATIC_SAMPLER_0100::Flags` (`UINT_BORDER_COLOR`,
///   `NON_NORMALIZED_COORDINATES`). ⚠ **This one IS a correctness loss** and is
///   counted separately, because a non-normalised static sampler silently
///   samples at the wrong coordinates rather than failing.
///
/// ⇒ The fix is a **versioned bridge entry point** over
/// `vkd3d_serialize_versioned_root_signature`
/// (`vkd3d-proton-helios/include/vkd3d.h:139-140`), which takes a
/// `D3D12_VERSIONED_ROOT_SIGNATURE_DESC` and handles 1.0/1.1/1.2. That is new
/// **C++**, which this lane deliberately does not write (`PARALLEL.md` §7: a
/// bridge module cannot be host-checked, and the whole lane type-checks on Linux
/// today). It is reported instead.
///
/// # Safety
/// `src` must point at a live `D3D12DDI_ROOT_SIGNATURE_0100` whose parameter and
/// sampler arrays are live for the call.
unsafe fn root_signature_to_1_0(src: &ddi12::D3D12DDI_ROOT_SIGNATURE_0100) -> Option<RootSignature10> {
    let n_params = src.NumParameters as usize;
    let n_samplers = src.NumStaticSamplers as usize;
    if n_params > MAX_ROOT_PARAMETERS || n_samplers > MAX_STATIC_SAMPLERS {
        log_error!(
            "CreateRootSignature: refusing NumParameters={n_params} NumStaticSamplers={n_samplers}"
        );
        return None;
    }
    if (n_params != 0 && src.pRootParameters.is_null())
        || (n_samplers != 0 && src.pStaticSamplers.is_null())
    {
        return None;
    }
    // SAFETY: both counts are bounded above and each pointer is non-null
    // whenever its count is non-zero; the runtime declares both arrays `_In_`
    // for the call.
    let (ddi_params, ddi_samplers) = unsafe {
        (
            if n_params == 0 {
                &[][..]
            } else {
                core::slice::from_raw_parts(src.pRootParameters, n_params)
            },
            if n_samplers == 0 {
                &[][..]
            } else {
                core::slice::from_raw_parts(src.pStaticSamplers, n_samplers)
            },
        )
    };

    // Pass 1: every descriptor table's ranges, into an outer vector that is
    // never resized once pass 2 starts taking pointers into it.
    let mut ranges: Vec<Vec<D3D12_DESCRIPTOR_RANGE>> = Vec::with_capacity(n_params);
    for p in ddi_params {
        if p.ParameterType != ddi12::D3D12DDI_ROOT_PARAMETER_TYPE_D3D12DDI_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE
        {
            ranges.push(Vec::new());
            continue;
        }
        // SAFETY: the arm is selected by `ParameterType`, which is the union's
        // own discriminant in this DDI — not by anything this driver chose.
        let table = unsafe { p.__bindgen_anon_1.DescriptorTable };
        let n = table.NumDescriptorRanges as usize;
        if n > MAX_DESCRIPTOR_RANGES || (n != 0 && table.pDescriptorRanges.is_null()) {
            log_error!("CreateRootSignature: refusing NumDescriptorRanges={n}");
            return None;
        }
        // SAFETY: bounded above and non-null whenever non-empty; `_In_` for the
        // call.
        let src_ranges = if n == 0 {
            &[][..]
        } else {
            unsafe { core::slice::from_raw_parts(table.pDescriptorRanges, n) }
        };
        let mut out = Vec::with_capacity(n);
        for r in src_ranges {
            if r.Flags
                != ddi12::D3D12DDI_DESCRIPTOR_RANGE_FLAGS_D3D12DDI_DESCRIPTOR_RANGE_FLAG_0013_NONE
            {
                L6_REFUSALS.root_sig_range_flags_dropped.bump();
            }
            out.push(D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE(r.RangeType),
                // ⚠ `0xFFFFFFFF` here means an unbounded range, legal as the
                // last entry of a table (`DDI_REFERENCE.md` §9.9 constraint 2).
                // It is copied, never arithmetic'd — which is the whole of what
                // that constraint asks for.
                NumDescriptors: r.NumDescriptors,
                BaseShaderRegister: r.BaseShaderRegister,
                RegisterSpace: r.RegisterSpace,
                OffsetInDescriptorsFromTableStart: r.OffsetInDescriptorsFromTableStart,
            });
        }
        ranges.push(out);
    }

    // Pass 2: the parameters, now that no `ranges` element can move.
    let mut parameters = Vec::with_capacity(n_params);
    for (i, p) in ddi_params.iter().enumerate() {
        let anonymous = match p.ParameterType {
            ddi12::D3D12DDI_ROOT_PARAMETER_TYPE_D3D12DDI_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE => {
                D3D12_ROOT_PARAMETER_0 {
                    DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                        NumDescriptorRanges: ranges[i].len() as u32,
                        pDescriptorRanges: ranges[i].as_ptr(),
                    },
                }
            }
            ddi12::D3D12DDI_ROOT_PARAMETER_TYPE_D3D12DDI_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS => {
                // SAFETY: arm selected by `ParameterType`.
                let c = unsafe { p.__bindgen_anon_1.Constants };
                // ⚠ `DDI_REFERENCE.md` §9.9 warns that `D3D12DDI_ROOT_CONSTANTS`
                // is *"not field-order-compatible"* with the API's, putting
                // `Num32BitValues` last where the API puts it first. **Measured
                // otherwise on this SDK**: both generated structs are
                // `{ShaderRegister, RegisterSpace, Num32BitValues}`
                // (`d3d12umddi.rs` `D3D12DDI_ROOT_CONSTANTS`, and the Win32
                // metadata's `D3D12_ROOT_CONSTANTS`), as is vkd3d's
                // `vkd3d_root_constants` (`include/vkd3d_shader.h:781-786`),
                // which matters because `vkd3d_serialize_root_signature` casts
                // the API struct to it. The copy below is by NAME, so it is
                // correct either way — but the doc's claim does not hold here
                // and is reported rather than silently worked around.
                D3D12_ROOT_PARAMETER_0 {
                    Constants: D3D12_ROOT_CONSTANTS {
                        ShaderRegister: c.ShaderRegister,
                        RegisterSpace: c.RegisterSpace,
                        Num32BitValues: c.Num32BitValues,
                    },
                }
            }
            ddi12::D3D12DDI_ROOT_PARAMETER_TYPE_D3D12DDI_ROOT_PARAMETER_TYPE_CBV
            | ddi12::D3D12DDI_ROOT_PARAMETER_TYPE_D3D12DDI_ROOT_PARAMETER_TYPE_SRV
            | ddi12::D3D12DDI_ROOT_PARAMETER_TYPE_D3D12DDI_ROOT_PARAMETER_TYPE_UAV => {
                // SAFETY: arm selected by `ParameterType`.
                let d = unsafe { p.__bindgen_anon_1.Descriptor };
                if d.Flags
                    != ddi12::D3D12DDI_ROOT_DESCRIPTOR_FLAGS_D3D12DDI_ROOT_DESCRIPTOR_FLAG_0013_NONE
                {
                    L6_REFUSALS.root_sig_range_flags_dropped.bump();
                }
                D3D12_ROOT_PARAMETER_0 {
                    Descriptor: D3D12_ROOT_DESCRIPTOR {
                        ShaderRegister: d.ShaderRegister,
                        RegisterSpace: d.RegisterSpace,
                    },
                }
            }
            // ⛔ No `else` that guesses (`DECISIONS.md` §7.4). An unknown
            // parameter type means the union has an arm this driver cannot
            // read, and reading the wrong one is a walk off the end of the
            // runtime's array.
            other => {
                log_error!("CreateRootSignature: unknown root parameter type {other}");
                return None;
            }
        };
        parameters.push(D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE(p.ParameterType),
            Anonymous: anonymous,
            ShaderVisibility: D3D12_SHADER_VISIBILITY(p.ShaderVisibility),
        });
    }

    let mut samplers = Vec::with_capacity(n_samplers);
    for s in ddi_samplers {
        if s.Flags != ddi12::D3D12DDI_SAMPLER_FLAGS_0096_D3D12DDI_SAMPLER_FLAG_NONE {
            L6_REFUSALS.root_sig_sampler_flags_dropped.bump();
            // ⛔ Loud at the moment it moves, not only in the summary. This is
            // the lane's one *correctness* loss (§4 / `SUBSTRATE.md` §4.5): a
            // non-normalised static sampler silently samples at the wrong
            // coordinates, and the process that hits it is exactly the kind that
            // gets killed or wedged before a clean `pfnDestroyDevice` prints the
            // set. A counter nobody can read after the fact is not an instrument.
            let n = L6_REFUSALS.root_sig_sampler_flags_dropped.get();
            if n <= LOG_BUDGET {
                log_error!(
                    "CreateRootSignature: static sampler s{} space{} flags={:#x} DROPPED \
                     (1.0 serialization cannot carry them) (x{n})",
                    s.ShaderRegister,
                    s.RegisterSpace,
                    s.Flags,
                );
            }
        }
        samplers.push(D3D12_STATIC_SAMPLER_DESC {
            Filter: D3D12_FILTER(s.Filter),
            AddressU: D3D12_TEXTURE_ADDRESS_MODE(s.AddressU),
            AddressV: D3D12_TEXTURE_ADDRESS_MODE(s.AddressV),
            AddressW: D3D12_TEXTURE_ADDRESS_MODE(s.AddressW),
            MipLODBias: s.MipLODBias,
            MaxAnisotropy: s.MaxAnisotropy,
            ComparisonFunc: D3D12_COMPARISON_FUNC(s.ComparisonFunc),
            BorderColor: D3D12_STATIC_BORDER_COLOR(s.BorderColor),
            MinLOD: s.MinLOD,
            MaxLOD: s.MaxLOD,
            ShaderRegister: s.ShaderRegister,
            RegisterSpace: s.RegisterSpace,
            ShaderVisibility: D3D12_SHADER_VISIBILITY(s.ShaderVisibility),
        });
    }

    let desc = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: parameters.len() as u32,
        pParameters: parameters.as_ptr(),
        NumStaticSamplers: samplers.len() as u32,
        pStaticSamplers: samplers.as_ptr(),
        Flags: D3D12_ROOT_SIGNATURE_FLAGS(src.Flags),
    };
    Some(RootSignature10 {
        desc,
        _ranges: ranges,
        _parameters: parameters,
        _samplers: samplers,
    })
}

/// The largest serialized root-signature blob this driver will hand back to the
/// engine.
///
/// ⚠ Not decoration: `ID3D12Device::CreateRootSignature`'s generated wrapper
/// does `pblobwithrootsignature.len().try_into().unwrap()` to narrow the length
/// to a `u32`. That `.unwrap()` is inside the `windows` crate and cannot be
/// removed, so the length is bounded **here**, where a failure is a counted
/// refusal rather than a panic in a DDI.
const MAX_ROOT_SIGNATURE_BLOB: usize = 1 << 20;

/// `pfnCreateRootSignature` — returns `HRESULT`, so failures need no
/// `pfnSetErrorCb`.
///
/// ⭐ **Root signatures arrive already PARSED and vkd3d wants a serialized DXBC
/// `RTS0` blob**, so this slot re-serializes through the engine's second export
/// (`DDI_REFERENCE.md` §9.9; `bridge12::serialize_root_signature`, proven end to
/// end by `D12-G1`'s third arm).
///
/// # Safety
/// `arg`, when non-null, must point at a live
/// `D3D12DDIARG_CREATE_ROOT_SIGNATURE_0100`, and `h_rs` must carry the machine
/// word `pfnCalcPrivateRootSignatureSize` sized.
unsafe extern "C" fn create_root_signature(
    h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_ROOT_SIGNATURE_0100,
    h_rs: ddi12::D3D12DDI_HROOTSIGNATURE,
) -> ddi12::HRESULT {
    // SAFETY: the caller guarantees the slot word.
    unsafe { clear_com(h_rs) };
    if arg.is_null() {
        note_refusal(&L6_REFUSALS.root_sig_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: non-null per the check; `_In_ CONST`.
    let a = unsafe { &*arg };

    // ⛔ Exhaustive over the two enumerators the header defines, with no `else`
    // that guesses (`DECISIONS.md` §7.4). `DDI_REFERENCE.md` §9.9: there is no
    // version 1.0 at the DDI — the runtime up-converts — and at `_0100` the
    // union has exactly one arm, so both live versions read the same pointer.
    match a.Version {
        ddi12::D3D12DDI_ROOT_SIGNATURE_VERSION_D3D12DDI_ROOT_SIGNATURE_VERSION_1_1
        | ddi12::D3D12DDI_ROOT_SIGNATURE_VERSION_D3D12DDI_ROOT_SIGNATURE_VERSION_1_2 => {}
        other => {
            note_refusal(&L6_REFUSALS.root_sig_version_unknown);
            log_error!("CreateRootSignature: unadvertised version {other} -> E_INVALIDARG");
            return E_INVALIDARG;
        }
    }

    // SAFETY: the union's single arm at `_0100`; every arm is a pointer at
    // offset 0, machine-checked by the bindgen layout assertions.
    let src = unsafe { a.__bindgen_anon_1.pRootSignature_1_2 };
    if src.is_null() {
        note_refusal(&L6_REFUSALS.root_sig_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: non-null per the check; `_In_ CONST` for the call.
    let Some(converted) = (unsafe { root_signature_to_1_0(&*src) }) else {
        note_refusal(&L6_REFUSALS.root_sig_bad_arg);
        return E_INVALIDARG;
    };
    L6_REFUSALS.root_sig_downgraded_to_1_0.bump();

    let mut blob_raw: usize = 0;
    let mut err_raw: usize = 0;
    // SAFETY: `converted.desc` is live for this call and every pointer it holds
    // addresses a vector `converted` owns. `blob_out`/`err_out` are stack
    // locals; the C++ side zeroes both before forwarding and writes them only
    // on success, and both receive OWNED `ID3DBlob*` this function releases.
    let hr = unsafe {
        bridge12::serialize_root_signature(
            core::ptr::from_ref(&converted.desc) as usize,
            D3D_ROOT_SIGNATURE_VERSION_1_0.0 as u32,
            &mut blob_raw,
            &mut err_raw,
        )
    };

    if err_raw != 0 {
        // SAFETY: non-zero, so it is an `ID3DBlob*` the engine AddRef'd for this
        // caller. Adopted as `IUnknown` because only `Release` is wanted.
        drop(unsafe { windows::core::IUnknown::from_raw(err_raw as *mut c_void) });
    }
    if hr < 0 || blob_raw == 0 {
        note_refusal(&L6_REFUSALS.root_sig_serialize_failed);
        log_error!("CreateRootSignature: serialize failed hr={:#010x}", hr as u32);
        return if hr < 0 { hr } else { E_FAIL };
    }
    // SAFETY: `blob_raw` is the OWNED `ID3DBlob*` the bridge produced; adopting
    // it here is the single `from_raw` for this reference and the `blob`
    // binding releases it at end of scope.
    let blob = unsafe { ID3DBlob::from_raw(blob_raw as *mut c_void) };
    // SAFETY: `blob` is a live `ID3DBlob`; both accessors are const on it.
    let (ptr, len) = unsafe { (blob.GetBufferPointer(), blob.GetBufferSize()) };
    if ptr.is_null() || len == 0 || len > MAX_ROOT_SIGNATURE_BLOB {
        note_refusal(&L6_REFUSALS.root_sig_serialize_failed);
        log_error!("CreateRootSignature: serialized blob is {len} bytes -> E_FAIL");
        return E_FAIL;
    }
    // SAFETY: the blob owns `len` readable bytes at `ptr` for as long as `blob`
    // is alive, which is the whole of this scope.
    let bytes = unsafe { core::slice::from_raw_parts(ptr.cast::<u8>(), len) };

    // SAFETY: `h_device` is this DDI's device handle.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&L6_REFUSALS.no_device);
        return E_FAIL;
    };
    let Some(engine) = dev.engine.d3d12_device() else {
        note_refusal(&L6_REFUSALS.no_device);
        return E_FAIL;
    };
    // SAFETY: `engine` is the bridge's BORROWED `ID3D12Device`, never released
    // here; `bytes` is live for the call and its length was bounded above so the
    // generated wrapper's `u32` narrowing cannot fail.
    let created: windows::core::Result<ID3D12RootSignature> =
        unsafe { engine.CreateRootSignature(a.NodeMask, bytes) };
    let rs = match created {
        Ok(rs) => rs,
        Err(e) => {
            note_refusal(&L6_REFUSALS.root_sig_engine_failed);
            log_error!(
                "CreateRootSignature: engine refused hr={:#010x}",
                e.code().0 as u32
            );
            return e.code().0;
        }
    };

    let Some(slot) = (
        // SAFETY: the caller guarantees the runtime-allocated word.
        unsafe { com_slot::<_, ID3D12RootSignature>(h_rs) }
    ) else {
        note_refusal(&L6_REFUSALS.root_sig_bad_arg);
        return E_INVALIDARG;
    };
    let n = L6_REFUSALS.root_sig_downgraded_to_1_0.get();
    if n <= LOG_BUDGET {
        log_error!(
            "CreateRootSignature: {} param(s), {} static sampler(s), flags={:#x}, blob={len} B \
             (x{n})",
            converted.desc.NumParameters,
            converted.desc.NumStaticSamplers,
            converted.desc.Flags.0,
        );
    }
    // SAFETY: the slot is the word the calc-size sized and was cleared at the
    // top; `store` moves the one reference `CreateRootSignature` returned into
    // it, and `destroy_root_signature` is the only path that releases it.
    unsafe { slot.store(rs) };
    S_OK
}

/// `pfnDestroyRootSignature`.
///
/// # Safety
/// `h_rs` must be a handle [`create_root_signature`] stored into, destroyed at
/// most once.
unsafe extern "C" fn destroy_root_signature(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_rs: ddi12::D3D12DDI_HROOTSIGNATURE,
) {
    // SAFETY: the caller guarantees a live slot; `release` is idempotent on an
    // already-cleared one, so a double destroy is a no-op rather than a double
    // free.
    if let Some(slot) = unsafe { com_slot::<_, ID3D12RootSignature>(h_rs) } {
        // SAFETY: as above.
        unsafe { slot.release() };
    }
}

// ---------------------------------------------------------------------------
// (e) Pipeline state — 3 slots
// ---------------------------------------------------------------------------

/// One subobject inside a `D3D12_PIPELINE_STATE_STREAM_DESC`.
///
/// ⛔ **The layout rule, taken from the consumer rather than from a header
/// comment.** vkd3d reads each subobject as `struct { TYPE type; T data; }` with
/// **natural** C alignment and then advances by
/// `align(sizeof(*subobject), sizeof(void*))` (`libs/vkd3d/state.c:2477-2493`).
/// `#[repr(C, align(8))]` reproduces exactly that: the attribute does not move
/// `data` — which stays at `align_up(4, align_of::<T>())`, the natural C offset
/// — and it rounds the struct's SIZE up to the pointer-size multiple the reader
/// steps by. The assertions below pin both halves.
///
/// ⭐ And the consequence that makes [`GraphicsStream`] safe to write as a plain
/// `#[repr(C)]` struct rather than a hand-packed byte buffer: a type's size is
/// always a multiple of its alignment, so every `Sub<T>` is a multiple of 8
/// long, so a `#[repr(C)]` sequence of them has **no interior padding** and each
/// subobject begins exactly where the reader's `align(…, sizeof(void*))` step
/// says it does.
#[repr(C, align(8))]
struct Sub<T> {
    ty: D3D12_PIPELINE_STATE_SUBOBJECT_TYPE,
    data: T,
}

impl<T> Sub<T> {
    fn new(ty: D3D12_PIPELINE_STATE_SUBOBJECT_TYPE, data: T) -> Self {
        Self { ty, data }
    }
}

// The two representative cases, one payload with 4-byte alignment and one with
// 8. If either rule broke, every subobject after the first would be read from
// the wrong offset — silently, because the type tag of the *next* subobject
// would be whatever byte happened to be there.
const _: () = assert!(
    core::mem::offset_of!(Sub<u32>, data) == 4 && core::mem::size_of::<Sub<u32>>() == 8,
    "a 4-byte-aligned subobject payload sits at offset 4 and the record is pointer-size padded"
);
const _: () = assert!(
    core::mem::offset_of!(Sub<D3D12_SHADER_BYTECODE>, data) == 8
        && core::mem::size_of::<Sub<D3D12_SHADER_BYTECODE>>().is_multiple_of(8),
    "an 8-byte-aligned subobject payload sits at offset 8 and the record is pointer-size padded"
);
const _: () = assert!(
    core::mem::size_of::<*mut c_void>() == 8,
    "the subobject stride rule above is `sizeof(void*)`, hard-coded as align(8)"
);

/// The graphics/mesh pipeline stream, in the order vkd3d's parser will walk it.
///
/// ⭐ **Why a stream and not `D3D12_GRAPHICS_PIPELINE_STATE_DESC`.** Five things
/// the legacy struct cannot carry, every one of them state the DDI hands this
/// driver at `_0110`:
///
/// 1. `FLOAT DepthBias` — legacy is `INT` (`SUBSTRATE.md` §4.5's trap);
/// 2. `LineRasterizationMode` — legacy has `MultisampleEnable` /
///    `AntialiasedLineEnable`, which cannot express `QUADRILATERAL_NARROW`;
/// 3. `DepthBoundsTestEnable`;
/// 4. per-face stencil read/write masks;
/// 5. ⛔ **mesh and amplification shaders at all** —
///    `D3D12DDIARG_CREATE_PIPELINE_STATE_0099` carries `hMeshShader` and
///    `hAmplificationShader` and the legacy struct has no field for either, so
///    the legacy route would have to refuse every mesh pipeline.
///
/// ⚠ There is deliberately **no `STREAM_OUTPUT` subobject**: `shaders.rs`
/// declines the stream-output declaration (`GsStreamOutputDropped`), so omitting
/// it here leaves vkd3d's own default of zero entries and the two halves agree.
/// ⚠ There is no `CACHED_PSO` subobject either: this driver declines the shader
/// cache, so it has no blob to offer and the runtime's is not forwarded.
#[repr(C)]
struct GraphicsStream {
    root_signature: Sub<*mut c_void>,
    vs: Sub<D3D12_SHADER_BYTECODE>,
    ps: Sub<D3D12_SHADER_BYTECODE>,
    ds: Sub<D3D12_SHADER_BYTECODE>,
    hs: Sub<D3D12_SHADER_BYTECODE>,
    gs: Sub<D3D12_SHADER_BYTECODE>,
    amplification: Sub<D3D12_SHADER_BYTECODE>,
    mesh: Sub<D3D12_SHADER_BYTECODE>,
    blend: Sub<D3D12_BLEND_DESC>,
    sample_mask: Sub<u32>,
    rasterizer: Sub<D3D12_RASTERIZER_DESC2>,
    depth_stencil: Sub<D3D12_DEPTH_STENCIL_DESC2>,
    input_layout: Sub<D3D12_INPUT_LAYOUT_DESC>,
    ib_strip_cut: Sub<D3D12_INDEX_BUFFER_STRIP_CUT_VALUE>,
    topology: Sub<D3D12_PRIMITIVE_TOPOLOGY_TYPE>,
    rtv_formats: Sub<D3D12_RT_FORMAT_ARRAY>,
    dsv_format: Sub<DXGI_FORMAT>,
    sample_desc: Sub<DXGI_SAMPLE_DESC>,
    node_mask: Sub<u32>,
    view_instancing: Sub<D3D12_VIEW_INSTANCING_DESC>,
    flags: Sub<D3D12_PIPELINE_STATE_FLAGS>,
}

/// An empty bytecode: `BytecodeLength == 0` is how vkd3d decides a stage is
/// absent (`vkd3d_pipeline_state_desc_get_shader_stages`, `state.c:2497-2521`).
fn no_bytecode() -> D3D12_SHADER_BYTECODE {
    D3D12_SHADER_BYTECODE {
        pShaderBytecode: core::ptr::null(),
        BytecodeLength: 0,
    }
}

/// The bytecode a shader handle carries, or the empty one when the handle is
/// null.
///
/// ⭐ Also the lane's two cross-checks on the PSO's own argument struct, which
/// exist because a handle bundle is otherwise forwarded entirely blind:
///
/// * `expected` is the stage of the **field** the handle came out of, and the
///   payload records the **slot** it was created on. A mismatch means the
///   runtime put a pixel shader in `hVertexShader`, which is a finding about the
///   argument struct rather than about this driver;
/// * `h_root_signature` is the PSO's, and the payload records the one the
///   runtime associated with the shader at create time. A disagreement means the
///   pipeline is being built against a different root signature than the shader
///   was compiled for — which vkd3d will notice as a binding mismatch much later
///   and much less legibly.
///
/// Both are counted, never refused on: neither is this driver's contract to
/// enforce, and a first reading has to exist before anything rejects on it (the
/// position `umd/src/adapter.rs:132-149` takes for `AdapterUnrecognised`).
///
/// # Safety
/// `h`, when its `pDrvPrivate` is non-null, must be a live shader handle
/// `shaders`'s create slots stored into, and the returned struct must not
/// outlive the DDI call.
unsafe fn bytecode_of(
    h: ddi12::D3D12DDI_HSHADER,
    expected: shaders::ShaderStage,
    h_root_signature: *mut c_void,
) -> D3D12_SHADER_BYTECODE {
    if h.pDrvPrivate.is_null() {
        return no_bytecode();
    }
    // SAFETY: forwarded; `shaders::shader` carries the re-derived D3D12
    // borrow argument and the caller guarantees liveness.
    let Some(state) = (unsafe { shaders::shader(h) }) else {
        return no_bytecode();
    };
    if state.stage != expected {
        L6_REFUSALS.shader_stage_mismatch.bump();
        let n = L6_REFUSALS.shader_stage_mismatch.get();
        if n <= LOG_BUDGET {
            log_error!(
                "CreatePipelineState: {} slot holds a {} shader (x{n})",
                expected.name(),
                state.stage.name(),
            );
        }
    }
    if state.h_root_signature != h_root_signature {
        L6_REFUSALS.shader_root_signature_mismatch.bump();
        // ⛔ Loud here for the same reason as the stage mismatch above: this is
        // the earliest point at which "the shader was compiled against a
        // different binding layout" is observable, and vkd3d would otherwise
        // surface it much later with nothing pointing back here.
        let n = L6_REFUSALS.shader_root_signature_mismatch.get();
        if n <= LOG_BUDGET {
            log_error!(
                "CreatePipelineState: {} shader was created against root signature {:p}, \
                 pipeline uses {:p} (x{n})",
                expected.name(),
                state.h_root_signature,
                h_root_signature,
            );
        }
    }
    D3D12_SHADER_BYTECODE {
        pShaderBytecode: state.container.as_ptr().cast::<c_void>(),
        BytecodeLength: state.container.len(),
    }
}

/// `pfnCalcPrivatePipelineStateSize`.
///
/// One machine word: the slot holds a bare owning `ID3D12PipelineState*`.
///
/// # Safety
/// As [`calc_private_element_layout_size`].
unsafe extern "C" fn calc_private_pipeline_state_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _arg: *const ddi12::D3D12DDIARG_CREATE_PIPELINE_STATE_0099,
) -> ddi12::SIZE_T {
    core::mem::size_of::<*mut c_void>() as ddi12::SIZE_T
}

/// `pfnCreatePipelineState` — one of the few D3D12 DDIs that returns `HRESULT`.
///
/// ⭐ **Which shader handles are non-null decides the call.** `hComputeShader`
/// selects `CreateComputePipelineState`; anything else is a graphics or mesh
/// pipeline and goes through the stream (`DDI_REFERENCE.md` §9.9).
///
/// ⚠ **What `hRTPipelineState` is for, since ignoring a runtime handle without
/// knowing is how contracts get lost:** it is the runtime's PSO handle for the
/// **shader-cache callbacks** `pfnShaderCacheGetValueCb` /
/// `pfnShaderCacheStoreValueCb`, whose second parameter is a
/// `D3D12DDI_HRTPIPELINESTATE` (`d3d12umddi.h:4248-4270`, runtime->driver table
/// 10). This driver declines the shader-cache extended feature and never calls
/// either, so the handle has no other use and is deliberately not stored — a
/// stored-and-unread field is the T5 anti-pattern.
///
/// # Safety
/// `arg`, when non-null, must point at a live
/// `D3D12DDIARG_CREATE_PIPELINE_STATE_0099` whose handles are live for the call,
/// and `h_pso` must carry the machine word
/// `pfnCalcPrivatePipelineStateSize` sized.
unsafe extern "C" fn create_pipeline_state(
    h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_PIPELINE_STATE_0099,
    h_pso: ddi12::D3D12DDI_HPIPELINESTATE,
    _h_rt_pso: ddi12::D3D12DDI_HRTPIPELINESTATE,
) -> ddi12::HRESULT {
    // SAFETY: the caller guarantees the slot word.
    unsafe { clear_com(h_pso) };
    if arg.is_null() {
        note_refusal(&L6_REFUSALS.pso_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: non-null per the check; `_In_ CONST`.
    let a = unsafe { &*arg };
    note_library_reference(&a.LibraryReference);
    L6_REFUSALS.pso_creates.bump();

    // ⚠ `SUBSTRATE.md` §4.5: the `DYNAMIC_*` PSO flags are HINTS, and the DDI
    // does NOT relieve the driver of applying the PSO's own depth-bias and
    // IB-strip-cut on every `pfnSetPipelineState` — *"a precise inversion of the
    // Vulkan mental model"*, where declaring the state dynamic makes the baked
    // value ignored. The flags are forwarded verbatim (they are the runtime's
    // declaration, not this driver's choice) and the value is forwarded too.
    //
    // ⛔ **The obligation this used to route to L3a is discharged by the ENGINE**,
    // in `d3d12_command_list_SetPipelineState`
    // (`vkd3d-proton-helios/libs/vkd3d/command.c:12711-12733`), which re-applies
    // both. Read this counter's doc before treating a hit as an exposure — the
    // grading was corrected at S6 Round 2 and the line below no longer warns.
    // Counted because "an application used the dynamic-state flags" is a fact
    // worth having, and because it is half of a two-counter reading: a hit here
    // while `pfnSetPipelineState` is still a counting noop IS the exposure.
    let dynamic_mask = ddi12::D3D12DDI_PIPELINE_STATE_FLAGS_D3D12DDI_PIPELINE_STATE_FLAG_DYNAMIC_DEPTH_BIAS
        | ddi12::D3D12DDI_PIPELINE_STATE_FLAGS_D3D12DDI_PIPELINE_STATE_FLAG_DYNAMIC_INDEX_BUFFER_STRIP_CUT;
    if a.Flags & dynamic_mask != 0 {
        L6_REFUSALS.pso_dynamic_state_flag_forwarded.bump();
        // ⚠ Bounded and non-alarming. It was a warning while the obligation was
        // believed to be open; the engine turns out to honour it, so the line's
        // job now is to say which PSOs are on the dynamic path at all — which is
        // what makes `pfnSetPipelineState` still being a noop diagnosable.
        let n = L6_REFUSALS.pso_dynamic_state_flag_forwarded.get();
        if n <= LOG_BUDGET {
            log_error!(
                "CreatePipelineState: DYNAMIC_* flags={:#x} forwarded; the engine re-applies the \
                 baked depth bias and strip-cut at SetPipelineState (x{n})",
                a.Flags & dynamic_mask,
            );
        }
    }

    // SAFETY: `h_device` is this DDI's device handle.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&L6_REFUSALS.no_device);
        return E_FAIL;
    };
    let Some(engine) = dev.engine.d3d12_device() else {
        note_refusal(&L6_REFUSALS.no_device);
        return E_FAIL;
    };
    let root_signature_raw = a.hRootSignature.pDrvPrivate;
    // SAFETY: `hRootSignature`, when non-null, is a handle
    // `create_root_signature` stored into; the slot word is the owning
    // `ID3D12RootSignature*` and is only READ here, never adopted or released.
    let root_signature_com: *mut c_void = if root_signature_raw.is_null() {
        core::ptr::null_mut()
    } else {
        match unsafe { com_slot::<_, ID3D12RootSignature>(a.hRootSignature) } {
            // SAFETY: as above — `word` reads the slot and takes no reference.
            Some(slot) => (unsafe { slot.word() }) as *mut c_void,
            None => core::ptr::null_mut(),
        }
    };

    let created = if !a.hComputeShader.pDrvPrivate.is_null() {
        // SAFETY: the runtime handed this handle in this call, so it is live.
        let cs = unsafe { bytecode_of(a.hComputeShader, shaders::ShaderStage::Compute, root_signature_raw) };
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            // SAFETY: `root_signature_com` is the slot's BORROWED reference;
            // `ManuallyDrop` is what stops the descriptor from releasing a
            // reference it never took, which is `bridge12`'s owned-vs-borrowed
            // rule applied to a struct field.
            pRootSignature: ManuallyDrop::new(if root_signature_com.is_null() {
                None
            } else {
                Some(unsafe { ID3D12RootSignature::from_raw(root_signature_com) })
            }),
            CS: cs,
            NodeMask: a.NodeMask,
            CachedPSO: Default::default(),
            Flags: D3D12_PIPELINE_STATE_FLAGS(a.Flags),
        };
        trace_line!(
            "CreatePipelineState: COMPUTE cs={} B node={:#x} flags={:#x}",
            cs.BytecodeLength,
            a.NodeMask,
            a.Flags,
        );
        // SAFETY: `engine` is the bridge's borrowed device; `desc` is a live
        // local whose bytecode pointer addresses a container the shader handle
        // owns for at least this call.
        unsafe { engine.CreateComputePipelineState::<ID3D12PipelineState>(&desc) }
    } else {
        // SAFETY: `hElementLayout`, when non-null, is a handle
        // `create_element_layout` stored into and live for this call;
        // `boxed_state` carries the re-derived borrow argument.
        let layout = if a.hElementLayout.pDrvPrivate.is_null() {
            None
        } else {
            unsafe { boxed_state(a.hElementLayout) }
        };
        // SAFETY: as above, for the three other sub-state handles.
        let blend = unsafe { sub_state_or_default::<_, BlendState>(a.hBlendState) };
        // SAFETY: as above.
        let raster = unsafe { sub_state_or_default::<_, RasterizerState>(a.hRasterizerState) };
        // SAFETY: as above.
        let depth = unsafe { sub_state_or_default::<_, DepthStencilState>(a.hDepthStencilState) };

        let mut rtv_formats = D3D12_RT_FORMAT_ARRAY {
            RTFormats: [DXGI_FORMAT(0); 8],
            NumRenderTargets: a.NumRenderTargets.min(8),
        };
        for (out, src) in rtv_formats.RTFormats.iter_mut().zip(a.RTVFormats.iter()) {
            *out = DXGI_FORMAT(*src);
        }
        if a.NumRenderTargets > 8 {
            note_refusal(&L6_REFUSALS.pso_render_target_count_clamped);
        }

        // ⚠ Copied element by element rather than pointer-cast. The DDI's
        // `D3D12DDI_VIEW_INSTANCE_LOCATION` and the API's happen to be the same
        // two `UINT`s, and that is precisely the agreement §12 rule 1 says not
        // to encode positionally.
        // SAFETY: `ViewInstancingDesc` is a field of the runtime's live `_In_`
        // argument struct, so its location array is live for this call.
        // ⛔ `None` is a refusal, not an empty set — see the helper's doc.
        let Some(view_locations) = (unsafe { view_instance_locations(&a.ViewInstancingDesc) })
        else {
            return E_INVALIDARG;
        };

        let stream = GraphicsStream {
            root_signature: Sub::new(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE,
                root_signature_com,
            ),
            // SAFETY: every one of these handles was passed in this call and is
            // live for it.
            vs: Sub::new(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VS, unsafe {
                bytecode_of(a.hVertexShader, shaders::ShaderStage::Vertex, root_signature_raw)
            }),
            // SAFETY: as above.
            ps: Sub::new(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, unsafe {
                bytecode_of(a.hPixelShader, shaders::ShaderStage::Pixel, root_signature_raw)
            }),
            // SAFETY: as above.
            ds: Sub::new(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DS, unsafe {
                bytecode_of(a.hDomainShader, shaders::ShaderStage::Domain, root_signature_raw)
            }),
            // SAFETY: as above.
            hs: Sub::new(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_HS, unsafe {
                bytecode_of(a.hHullShader, shaders::ShaderStage::Hull, root_signature_raw)
            }),
            // SAFETY: as above.
            gs: Sub::new(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_GS, unsafe {
                bytecode_of(a.hGeometryShader, shaders::ShaderStage::Geometry, root_signature_raw)
            }),
            // SAFETY: as above.
            amplification: Sub::new(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_AS, unsafe {
                bytecode_of(a.hAmplificationShader, shaders::ShaderStage::Amplification, root_signature_raw)
            }),
            // SAFETY: as above.
            mesh: Sub::new(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, unsafe {
                bytecode_of(a.hMeshShader, shaders::ShaderStage::Mesh, root_signature_raw)
            }),
            blend: Sub::new(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND, blend),
            sample_mask: Sub::new(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK,
                a.SampleMask,
            ),
            rasterizer: Sub::new(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER2, raster),
            depth_stencil: Sub::new(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2, depth),
            input_layout: Sub::new(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_INPUT_LAYOUT,
                D3D12_INPUT_LAYOUT_DESC {
                    pInputElementDescs: layout
                        .map_or(core::ptr::null(), |l| l.elements.as_ptr()),
                    NumElements: layout.map_or(0, |l| l.elements.len() as u32),
                },
            ),
            ib_strip_cut: Sub::new(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_IB_STRIP_CUT_VALUE,
                D3D12_INDEX_BUFFER_STRIP_CUT_VALUE(a.IBStripCutValue),
            ),
            topology: Sub::new(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY,
                D3D12_PRIMITIVE_TOPOLOGY_TYPE(a.PrimitiveTopologyType),
            ),
            rtv_formats: Sub::new(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS,
                rtv_formats,
            ),
            dsv_format: Sub::new(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT,
                DXGI_FORMAT(a.DSVFormat),
            ),
            sample_desc: Sub::new(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC,
                DXGI_SAMPLE_DESC {
                    Count: a.SampleDesc.Count,
                    Quality: a.SampleDesc.Quality,
                },
            ),
            node_mask: Sub::new(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_NODE_MASK, a.NodeMask),
            view_instancing: Sub::new(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VIEW_INSTANCING,
                D3D12_VIEW_INSTANCING_DESC {
                    ViewInstanceCount: view_locations.len() as u32,
                    pViewInstanceLocations: view_locations.as_ptr(),
                    Flags: D3D12_VIEW_INSTANCING_FLAGS(a.ViewInstancingDesc.Flags),
                },
            ),
            flags: Sub::new(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_FLAGS,
                D3D12_PIPELINE_STATE_FLAGS(a.Flags),
            ),
        };
        trace_line!(
            "CreatePipelineState: GRAPHICS vs={} ps={} ds={} hs={} gs={} as={} ms={} rt={} \
             dsv={} samples={} flags={:#x}",
            stream.vs.data.BytecodeLength,
            stream.ps.data.BytecodeLength,
            stream.ds.data.BytecodeLength,
            stream.hs.data.BytecodeLength,
            stream.gs.data.BytecodeLength,
            stream.amplification.data.BytecodeLength,
            stream.mesh.data.BytecodeLength,
            a.NumRenderTargets,
            a.DSVFormat,
            a.SampleDesc.Count,
            a.Flags,
        );

        let stream_desc = D3D12_PIPELINE_STATE_STREAM_DESC {
            SizeInBytes: core::mem::size_of::<GraphicsStream>(),
            pPipelineStateSubobjectStream: core::ptr::from_ref(&stream) as *mut c_void,
        };
        // SAFETY: the engine implements `ID3D12Device2`
        // (`libs/vkd3d/device.c:4639`), so the QI below returns the same object
        // with one extra reference that `device2` releases at end of scope.
        let device2 = match engine.cast::<ID3D12Device2>() {
            Ok(d) => d,
            Err(e) => {
                note_refusal(&L6_REFUSALS.pso_no_device2);
                log_error!(
                    "CreatePipelineState: engine has no ID3D12Device2 hr={:#010x}",
                    e.code().0 as u32
                );
                return e.code().0;
            }
        };
        // SAFETY: `stream` is a live local for the duration of this call, its
        // size is its own `size_of`, and every pointer it holds addresses
        // storage that outlives the call (the shader containers, the element
        // layout's vector, `view_locations`, and the borrowed root signature).
        unsafe { device2.CreatePipelineState::<ID3D12PipelineState>(&stream_desc) }
    };

    let pso = match created {
        Ok(p) => p,
        Err(e) => {
            note_refusal(&L6_REFUSALS.pso_engine_failed);
            let n = L6_REFUSALS.pso_engine_failed.get();
            if n <= LOG_BUDGET {
                log_error!(
                    "CreatePipelineState: engine refused hr={:#010x} (x{n})",
                    e.code().0 as u32
                );
            }
            return e.code().0;
        }
    };

    let Some(slot) = (
        // SAFETY: the caller guarantees the runtime-allocated word.
        unsafe { com_slot::<_, ID3D12PipelineState>(h_pso) }
    ) else {
        note_refusal(&L6_REFUSALS.pso_bad_arg);
        return E_INVALIDARG;
    };
    // SAFETY: the slot is the word the calc-size sized and was cleared at the
    // top; `store` moves the one reference the engine returned into it.
    unsafe { slot.store(pso) };
    S_OK
}

/// A sub-state payload's translated desc, or the **API default** when the handle
/// carries no state.
///
/// ⛔ **Not `Default::default()`, which is `core::mem::zeroed()`** — that was
/// wrong and it was wrong silently. [`GraphicsStream`] ALWAYS emits a `BLEND`, a
/// `RASTERIZER2` and a `DEPTH_STENCIL2` subobject, so whatever this returns
/// OVERRIDES the non-zero defaults vkd3d installs before it walks the stream
/// (`d3d12_init_pipeline_state_desc`, `libs/vkd3d/state.c:2390-2420`: write mask
/// ALL, fill SOLID, cull BACK, `DepthClipEnable` TRUE, depth enable / LESS /
/// write ALL, stencil KEEP+ALWAYS with `D3D12_DEFAULT_STENCIL_*_MASK`). A zeroed
/// desc is not "what vkd3d would have supplied anyway":
///
/// * zeroed blend is `RenderTargetWriteMask == 0` on all eight render targets —
///   a pipeline that writes nothing at all, with `L6PsoCreates` moving normally
///   and no counter in this lane pointing at it;
/// * zeroed rasterizer is worse-shaped still, because `FillMode == 0` and
///   `CullMode == 0` are not legal `D3D12_FILL_MODE` / `D3D12_CULL_MODE`
///   enumerators (SOLID and BACK are both 3), so vkd3d takes its `FIXME` arms —
///   and `DepthClipEnable` is lost with them.
///
/// ⇒ [`SubStateDesc::default_desc`] spells out the same defaults the API
/// documents (`CD3DX12_*_DESC(D3D12_DEFAULT)`) and vkd3d installs, and **both**
/// fallback arms are counted, because a PSO built on defaults is otherwise
/// indistinguishable from one built on the runtime's own state.
///
/// # Safety
/// `h`, when its slot word is non-null, must be a live handle the matching
/// create in this file stored into.
unsafe fn sub_state_or_default<H, S>(h: H) -> S::Desc
where
    H: BoxedHandle<State = S>,
    S: SubStateDesc,
{
    if h.drv_private().is_null() {
        L6_REFUSALS.pso_sub_state_absent.bump();
        let n = L6_REFUSALS.pso_sub_state_absent.get();
        if n <= LOG_BUDGET {
            log_error!(
                "CreatePipelineState: no {} state handle, using the API defaults (x{n})",
                S::NAME,
            );
        }
        return S::default_desc();
    }
    // SAFETY: forwarded; `boxed_state` carries the re-derived borrow argument.
    match unsafe { boxed_state(h) } {
        Some(state) => state.desc(),
        None => {
            // ⛔ The handle exists but its slot word is null, which means the
            // matching `pfnCreate*State` refused and counted `L6SubStateBadArg`.
            // The PSO is still built — refusing it here would turn one bad
            // sub-state into a failed device — but on the API defaults, and the
            // fact is a number rather than a black frame.
            L6_REFUSALS.pso_sub_state_unresolved.bump();
            let n = L6_REFUSALS.pso_sub_state_unresolved.get();
            if n <= LOG_BUDGET {
                log_error!(
                    "CreatePipelineState: {} handle carries no state -- its create refused; \
                     using the API defaults (x{n})",
                    S::NAME,
                );
            }
            S::default_desc()
        }
    }
}

/// The two things [`sub_state_or_default`] needs of a sub-state payload: the API
/// descriptor it wraps, and that descriptor's documented default.
///
/// ⚠ A trait rather than three near-identical helpers, because three helpers is
/// how the fourth one gets written slightly differently.
trait SubStateDesc {
    /// This state's name, for the two defaulting log lines above. ASCII.
    const NAME: &'static str;
    type Desc: Copy;
    fn desc(&self) -> Self::Desc;
    fn default_desc() -> Self::Desc;
}

impl SubStateDesc for BlendState {
    const NAME: &'static str = "blend";
    type Desc = D3D12_BLEND_DESC;
    fn desc(&self) -> D3D12_BLEND_DESC {
        self.desc
    }
    fn default_desc() -> D3D12_BLEND_DESC {
        // ⭐ The field that matters is `RenderTargetWriteMask`: vkd3d sets it to
        // `D3D12_COLOR_WRITE_ENABLE_ALL` on RT0 and a zeroed struct sets it to 0
        // on all eight. All eight are written here rather than only RT0, because
        // `IndependentBlendEnable` is the runtime's to set and a default that is
        // only correct for one value of someone else's field is a trap.
        let rt = D3D12_RENDER_TARGET_BLEND_DESC {
            BlendEnable: windows::Win32::Foundation::BOOL(0),
            LogicOpEnable: windows::Win32::Foundation::BOOL(0),
            SrcBlend: D3D12_BLEND_ONE,
            DestBlend: D3D12_BLEND_ZERO,
            BlendOp: D3D12_BLEND_OP_ADD,
            SrcBlendAlpha: D3D12_BLEND_ONE,
            DestBlendAlpha: D3D12_BLEND_ZERO,
            BlendOpAlpha: D3D12_BLEND_OP_ADD,
            LogicOp: D3D12_LOGIC_OP_NOOP,
            RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        D3D12_BLEND_DESC {
            AlphaToCoverageEnable: windows::Win32::Foundation::BOOL(0),
            IndependentBlendEnable: windows::Win32::Foundation::BOOL(0),
            RenderTarget: [rt; 8],
        }
    }
}

impl SubStateDesc for RasterizerState {
    const NAME: &'static str = "rasterizer";
    type Desc = D3D12_RASTERIZER_DESC2;
    fn desc(&self) -> D3D12_RASTERIZER_DESC2 {
        self.desc
    }
    fn default_desc() -> D3D12_RASTERIZER_DESC2 {
        // ⛔ `FillMode` and `CullMode` have no zero enumerator at all, and
        // `DepthClipEnable` defaults TRUE — three fields a zeroed struct gets
        // wrong in three different ways. `LineRasterizationMode` IS zero
        // (`ALIASED`), spelled out so the reader does not have to know that.
        D3D12_RASTERIZER_DESC2 {
            FillMode: D3D12_FILL_MODE_SOLID,
            CullMode: D3D12_CULL_MODE_BACK,
            FrontCounterClockwise: windows::Win32::Foundation::BOOL(0),
            DepthBias: 0.0,
            DepthBiasClamp: 0.0,
            SlopeScaledDepthBias: 0.0,
            DepthClipEnable: windows::Win32::Foundation::BOOL(1),
            LineRasterizationMode: D3D12_LINE_RASTERIZATION_MODE_ALIASED,
            ForcedSampleCount: 0,
            ConservativeRaster: D3D12_CONSERVATIVE_RASTERIZATION_MODE_OFF,
        }
    }
}

impl SubStateDesc for DepthStencilState {
    const NAME: &'static str = "depth-stencil";
    type Desc = D3D12_DEPTH_STENCIL_DESC2;
    fn desc(&self) -> D3D12_DEPTH_STENCIL_DESC2 {
        self.desc
    }
    fn default_desc() -> D3D12_DEPTH_STENCIL_DESC2 {
        // ⚠ The per-face masks live in `D3D12_DEPTH_STENCILOP_DESC1` at DESC2,
        // which is where vkd3d puts `D3D12_DEFAULT_STENCIL_*_MASK` too. Both are
        // `u32` constants of value 255 narrowed to the struct's `u8`.
        let face = D3D12_DEPTH_STENCILOP_DESC1 {
            StencilFailOp: D3D12_STENCIL_OP_KEEP,
            StencilDepthFailOp: D3D12_STENCIL_OP_KEEP,
            StencilPassOp: D3D12_STENCIL_OP_KEEP,
            StencilFunc: D3D12_COMPARISON_FUNC_ALWAYS,
            StencilReadMask: D3D12_DEFAULT_STENCIL_READ_MASK as u8,
            StencilWriteMask: D3D12_DEFAULT_STENCIL_WRITE_MASK as u8,
        };
        D3D12_DEPTH_STENCIL_DESC2 {
            DepthEnable: windows::Win32::Foundation::BOOL(1),
            DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
            DepthFunc: D3D12_COMPARISON_FUNC_LESS,
            StencilEnable: windows::Win32::Foundation::BOOL(0),
            FrontFace: face,
            BackFace: face,
            DepthBoundsTestEnable: windows::Win32::Foundation::BOOL(0),
        }
    }
}

// The three defaults above are transcribed from a reader, so they are pinned to
// the generated constants rather than to the numbers in the comment: if the
// Win32 metadata ever renumbers one of these enumerators, the assertion fails
// here instead of the pipeline rendering wrong somewhere else.
const _: () = assert!(
    D3D12_FILL_MODE_SOLID.0 == 3 && D3D12_CULL_MODE_BACK.0 == 3,
    "a zeroed rasterizer desc is not a legal one -- FillMode/CullMode have no 0 enumerator"
);
const _: () = assert!(
    D3D12_COLOR_WRITE_ENABLE_ALL.0 == 15
        && D3D12_DEFAULT_STENCIL_READ_MASK == 0xff
        && D3D12_DEFAULT_STENCIL_WRITE_MASK == 0xff,
    "the three default masks a zeroed desc would have cleared"
);

/// The largest view-instancing set this driver will copy.
/// `D3D12_MAX_VIEW_INSTANCE_COUNT` is 4.
const MAX_VIEW_INSTANCES: usize = 4;

/// Copy the DDI's view-instance locations into API structs.
///
/// `Some(vec)` — possibly empty, which is the no-view-instancing case — is a set
/// the caller may forward. **`None` means refuse the pipeline**, and both of its
/// arms are counted.
///
/// ⛔ Dropping a view-instancing set and returning `S_OK` anyway is a
/// view-instanced pipeline silently becoming a single-view one — the app renders
/// one view where it asked for several, with nothing to see but a counter at
/// `pfnDestroyDevice`. Both refusals below are also exactly what the engine
/// itself would do with the same input, which is the strongest argument that
/// refusing is not this driver inventing a rule:
///
/// * `ViewInstanceCount > D3D12_MAX_VIEW_INSTANCE_COUNT` is
///   `ERR("View instance count is too large") -> E_INVALIDARG` in
///   `d3d12_pipeline_state_init_graphics` (`libs/vkd3d/state.c:5486-5493`);
/// * a null `pViewInstanceLocations` with a non-zero count is not a legal
///   descriptor at all — `d3d12_pipeline_state_validate_view_instancing`
///   dereferences `pViewInstanceLocations[i]` unconditionally
///   (`state.c:5347-5349`), so forwarding one would fault inside the engine.
///
/// # Safety
/// `desc`'s `pViewInstanceLocations`, when `ViewInstanceCount` is non-zero, must
/// address that many live locations for the duration of the call.
unsafe fn view_instance_locations(
    desc: &ddi12::D3D12DDI_VIEW_INSTANCING_DESC,
) -> Option<Vec<D3D12_VIEW_INSTANCE_LOCATION>> {
    let count = desc.ViewInstanceCount as usize;
    if count == 0 {
        return Some(Vec::new());
    }
    if count > MAX_VIEW_INSTANCES {
        note_refusal(&L6_REFUSALS.pso_view_instancing_refused);
        log_error!("CreatePipelineState: ViewInstanceCount={count} -> E_INVALIDARG");
        return None;
    }
    if desc.pViewInstanceLocations.is_null() {
        note_refusal(&L6_REFUSALS.pso_view_instancing_bad_arg);
        log_error!(
            "CreatePipelineState: ViewInstanceCount={count} with no locations -> E_INVALIDARG"
        );
        return None;
    }
    // SAFETY: bounded above and non-null per the check; `_In_` for the call.
    let src = unsafe { core::slice::from_raw_parts(desc.pViewInstanceLocations, count) };
    Some(
        src.iter()
            .map(|l| D3D12_VIEW_INSTANCE_LOCATION {
                ViewportArrayIndex: l.ViewportArrayIndex,
                RenderTargetArrayIndex: l.RenderTargetArrayIndex,
            })
            .collect(),
    )
}

/// `pfnDestroyPipelineState`.
///
/// # Safety
/// `h_pso` must be a handle [`create_pipeline_state`] stored into, destroyed at
/// most once.
unsafe extern "C" fn destroy_pipeline_state(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_pso: ddi12::D3D12DDI_HPIPELINESTATE,
) {
    // SAFETY: the caller guarantees a live slot; `release` is idempotent on an
    // already-cleared one.
    if let Some(slot) = unsafe { com_slot::<_, ID3D12PipelineState>(h_pso) } {
        // SAFETY: as above.
        unsafe { slot.release() };
    }
}

// ---------------------------------------------------------------------------
// (e) Pipeline libraries — 6 slots, REFUSED
// ---------------------------------------------------------------------------
//
// ⭐ **A pipeline library is a PSO CACHE, and vkd3d has its own.**
// `DDI_REFERENCE.md` §9.9: *"Pipeline libraries … map to `ID3D12PipelineLibrary`"*
// — an engine object this bridge does not expose, sitting on top of a caching
// layer (`libs/vkd3d/cache.c`) vkd3d drives itself from `VkPipelineCache`. So
// implementing them here would be a second cache in front of a first one, and
// the only thing it would buy is the app's ability to persist a blob across
// runs.
//
// ⛔ **Refusing is checked against the `pfnFillDDITable` lesson, not assumed
// safe.** That lesson — refusing an unknown table type LOST THE DEVICE
// (`lib.rs`'s `FillDDITableUnknownType`) — is the standing warning that
// "refuse" is not automatically the safe direction. It does not transfer here,
// and the reason is structural rather than hopeful: `ID3D12Device1::CreatePipelineLibrary`
// is documented to return `E_NOTIMPL` when the driver does not support
// libraries, and the D3D12 runtime's own contract is that an application then
// falls back to creating PSOs directly. The failure is delivered to the app that
// asked, through an `HRESULT` **the DDI defines** — four of these six slots
// return one — rather than to the device.
//
// ⚠ What makes it visible when it happens: `L6PipelineLibraryRefused` counts the
// requests, and `L6LibraryReferenceIgnored` counts every sub-state, shader or
// PSO create that arrived carrying a `LibraryReference` — i.e. the downstream
// effect, at the objects it affects.

/// `pfnCalcPrivatePipelineLibrarySize`.
///
/// ⚠ Answers **one machine word**, not 0. A driver that refuses `CreateX` must
/// still return a size `CalcPrivateXSize` and `CreateX` agree on: the runtime
/// allocates before it calls, and a zero-byte private region with a create that
/// writes anything is the R702/§12-rule-16 class of heap corruption. Nothing is
/// written into it, but the word exists.
///
/// # Safety
/// As [`calc_private_element_layout_size`].
unsafe extern "C" fn calc_private_pipeline_library_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _arg: *const ddi12::D3D12DDIARG_CREATE_PIPELINE_LIBRARY_0010,
) -> ddi12::SIZE_T {
    core::mem::size_of::<*mut c_void>() as ddi12::SIZE_T
}

/// `pfnCreatePipelineLibrary` — refused, with the `HRESULT` the DDI provides.
///
/// # Safety
/// `h_library`'s `pDrvPrivate`, when non-null, must address the machine word
/// [`calc_private_pipeline_library_size`] sized.
unsafe extern "C" fn create_pipeline_library(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _arg: *const ddi12::D3D12DDIARG_CREATE_PIPELINE_LIBRARY_0010,
    h_library: ddi12::D3D12DDI_HPIPELINELIBRARY,
) -> ddi12::HRESULT {
    if !h_library.pDrvPrivate.is_null() {
        // SAFETY: non-null per the check, and it is the word the paired
        // calc-size sized. Nulling it leaves a refused create with a clear
        // handle rather than stale garbage.
        unsafe { core::ptr::write(h_library.pDrvPrivate.cast::<*mut c_void>(), core::ptr::null_mut()) };
    }
    note_refusal(&L6_REFUSALS.pipeline_library_refused);
    helios_umd_common::hr::E_NOTIMPL
}

/// `pfnDestroyPipelineLibrary` — nothing was ever created, so nothing is freed.
///
/// ⚠ Still counted: reaching this means the runtime believes a library exists,
/// which contradicts [`create_pipeline_library`] having refused every one.
///
/// # Safety
/// Trivially safe: the handle is not dereferenced.
unsafe extern "C" fn destroy_pipeline_library(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _h_library: ddi12::D3D12DDI_HPIPELINELIBRARY,
) {
    note_refusal(&L6_REFUSALS.pipeline_library_unexpected);
}

/// `pfnAddPipelineStateToLibrary` — refused.
///
/// # Safety
/// Trivially safe: no handle is dereferenced.
unsafe extern "C" fn add_pipeline_state_to_library(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _h_library: ddi12::D3D12DDI_HPIPELINELIBRARY,
    _h_pipeline_state: ddi12::D3D12DDI_HPIPELINESTATE,
    _pipeline_index: ddi12::UINT,
) -> ddi12::HRESULT {
    note_refusal(&L6_REFUSALS.pipeline_library_refused);
    helios_umd_common::hr::E_NOTIMPL
}

/// `pfnCalcSerializedLibrarySize` — nothing to serialize.
///
/// ⚠ Returns 0 rather than a placeholder: this is a byte count for a blob that
/// does not exist, and any non-zero answer would make the runtime allocate a
/// buffer and then call [`serialize_library`], which refuses.
///
/// # Safety
/// Trivially safe: no handle is dereferenced.
unsafe extern "C" fn calc_serialized_library_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _h_library: ddi12::D3D12DDI_HPIPELINELIBRARY,
) -> ddi12::SIZE_T {
    note_refusal(&L6_REFUSALS.pipeline_library_refused);
    0
}

/// `pfnSerializeLibrary` — refused.
///
/// # Safety
/// Trivially safe: `p_blob` is not dereferenced, which is the whole of the
/// refusal.
unsafe extern "C" fn serialize_library(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _h_library: ddi12::D3D12DDI_HPIPELINELIBRARY,
    _p_blob: *mut c_void,
) -> ddi12::HRESULT {
    note_refusal(&L6_REFUSALS.pipeline_library_refused);
    helios_umd_common::hr::E_NOTIMPL
}

// ---------------------------------------------------------------------------
// The chain links
// ---------------------------------------------------------------------------

/// Install L6's 24 sub-state / PSO / root-signature / pipeline-library
/// device-core slots.
///
/// Chain position: `DescriptorSlots` -> `PsoSlots` on the device-core table.
pub(crate) fn install(
    mut filling: Filling<'_, DeviceCoreTable, stage::DescriptorSlots>,
) -> Filling<'_, DeviceCoreTable, stage::PsoSlots> {
    let table = filling.table();

    // (b) immutable pipeline sub-state — 12
    table.pfnCalcPrivateElementLayoutSize = Some(calc_private_element_layout_size);
    table.pfnCreateElementLayout = Some(create_element_layout);
    table.pfnDestroyElementLayout = Some(destroy_element_layout);
    table.pfnCalcPrivateBlendStateSize = Some(calc_private_blend_state_size);
    table.pfnCreateBlendState = Some(create_blend_state);
    table.pfnDestroyBlendState = Some(destroy_blend_state);
    table.pfnCalcPrivateDepthStencilStateSize = Some(calc_private_depth_stencil_state_size);
    table.pfnCreateDepthStencilState = Some(create_depth_stencil_state);
    table.pfnDestroyDepthStencilState = Some(destroy_depth_stencil_state);
    table.pfnCalcPrivateRasterizerStateSize = Some(calc_private_rasterizer_state_size);
    table.pfnCreateRasterizerState = Some(create_rasterizer_state);
    table.pfnDestroyRasterizerState = Some(destroy_rasterizer_state);

    // (e) pipeline state, libraries, root signatures — 12
    table.pfnCalcPrivatePipelineStateSize = Some(calc_private_pipeline_state_size);
    table.pfnCreatePipelineState = Some(create_pipeline_state);
    table.pfnDestroyPipelineState = Some(destroy_pipeline_state);
    table.pfnCalcPrivateRootSignatureSize = Some(calc_private_root_signature_size);
    table.pfnCreateRootSignature = Some(create_root_signature);
    table.pfnDestroyRootSignature = Some(destroy_root_signature);
    table.pfnCalcPrivatePipelineLibrarySize = Some(calc_private_pipeline_library_size);
    table.pfnCreatePipelineLibrary = Some(create_pipeline_library);
    table.pfnDestroyPipelineLibrary = Some(destroy_pipeline_library);
    table.pfnAddPipelineStateToLibrary = Some(add_pipeline_state_to_library);
    table.pfnCalcSerializedLibrarySize = Some(calc_serialized_library_size);
    table.pfnSerializeLibrary = Some(serialize_library);

    filling.advance()
}

/// Install L6's 14 shader device-core slots.
///
/// Chain position: `PsoSlots` -> `ShaderSlots` on the device-core table.
///
/// ⚠ A one-line delegate rather than a `pub(crate) use`, because the chain link
/// `tables12` names is `pso::install_shaders` and `tables12` is a shared file a
/// lane may not edit (`PARALLEL.md` §5). The bodies live in [`super::shaders`],
/// which `ARCHITECTURE.md` §12 rule 8 anticipated: `umd`'s `forward.rs` reached
/// 10 744 lines before it was split.
pub(crate) fn install_shaders(
    filling: Filling<'_, DeviceCoreTable, stage::PsoSlots>,
) -> Filling<'_, DeviceCoreTable, stage::ShaderSlots> {
    shaders::install(filling)
}

// ---------------------------------------------------------------------------
// The lane's refusal set
// ---------------------------------------------------------------------------

/// L6's refusal counters — for **both** files of the lane.
///
/// ⛔ **Append only.** Counter order inside a set, and set order in `lib.rs`'s
/// `UMD12_REFUSAL_SETS`, are both the evidence contract: `D3D12 DDI refusals:`
/// lines get diffed across builds.
pub(crate) struct L6Refusals {
    // ── sub-state (b) ──────────────────────────────────────────────────────
    /// A sub-state create was handed a null desc, a null handle slot, or a
    /// count this driver refuses. Expected 0 — the runtime supplies all of them.
    pub(crate) sub_state_bad_arg: RefusalCounter,
    /// `D3D12DDI_RASTERIZER_DESC_0102::ScissorEnable` was **FALSE** and was
    /// dropped, because no `D3D12_RASTERIZER_DESC*` has the field: D3D12 makes
    /// the scissor test unconditional and supplies rectangles as command-list
    /// state.
    ///
    /// ⚠ Expected 0, and the reading itself is the finding: nothing in the doc
    /// set records which constant the runtime puts in a D3D11-vestigial field,
    /// and this is the only place it could be observed. A high reading means
    /// apps are asking for a scissor-less pipeline and getting a scissored one.
    pub(crate) rasterizer_scissor_disabled: RefusalCounter,
    /// `FrontEnable`/`BackEnable` disagreed with `StencilEnable` and were
    /// folded into it, because `D3D12_DEPTH_STENCIL_DESC2` has only the one
    /// enable. ⚠ Expected 0; a hit means per-face stencil ops are being applied
    /// where the runtime asked for them not to be.
    pub(crate) depth_stencil_face_enable_folded: RefusalCounter,
    /// An object arrived carrying a non-null `LibraryReference::hLibrary`, i.e.
    /// the runtime asked for it to come out of a pipeline library this lane
    /// refuses. ⚠ Expected 0 while `L6PipelineLibraryRefused` is also 0; the
    /// two move together.
    pub(crate) library_reference_ignored: RefusalCounter,

    // ── shaders (c), bumped from `shaders.rs` ──────────────────────────────
    /// How many shader creates this process has served. ⚠ Not a refusal — it
    /// bounds this lane's log budget and is the denominator every other shader
    /// counter is read against. `D12-G7` reached `pfnCreateVertexShader` and
    /// `pfnCreateComputeShader` inside `D3D12CreateDevice`, so a non-zero
    /// reading on any run that creates a device is the expected shape.
    pub(crate) shader_creates: RefusalCounter,
    /// A shader create was handed a null arg or a null handle slot. Expected 0.
    pub(crate) shader_bad_arg: RefusalCounter,
    /// The bytecode did not describe its own length: neither a DXBC container
    /// with a plausible total size nor a raw stream with a plausible dword
    /// count. The shader is not created. ⛔ Expected 0 — a hit means the blob
    /// format changed under the reader ported from
    /// `umd/src/forward/shaders.rs:13-39`.
    pub(crate) shader_length_unknown: RefusalCounter,
    /// The bytecode arrived as a **DXBC container** rather than the raw stream
    /// `D12-G5` measured. ⚠ Expected 0, and it is the instrument for
    /// `DDI_REFERENCE.md` §12.2's *"the DXBC-container branch is dead on this
    /// DDI"* — kept precisely so that if one ever does arrive the fact is
    /// recorded rather than assumed away.
    pub(crate) shader_dxbc_container_seen: RefusalCounter,
    /// Dword 0's DXIL program kind did not match the create slot the bytecode
    /// arrived on. ⛔ Expected 0: §12.2 measured it matching in every sample, so
    /// a hit is a real finding about which slot the runtime called.
    pub(crate) shader_program_kind_mismatch: RefusalCounter,
    /// An IO signature array was longer than `MAX_SIGNATURE_ENTRIES` and was
    /// encoded as empty. ⛔ Expected 0 — D3D has at most 32 registers per stage.
    pub(crate) shader_signature_count_refused: RefusalCounter,
    /// A mesh shader's **per-primitive** output signature was dropped: a DXBC
    /// container has one output signature part and the per-primitive set has no
    /// part to live in. ⚠ Expected 0 until mesh shaders are used; a hit means
    /// vkd3d is validating a mesh pipeline's IO against half its outputs.
    pub(crate) mesh_primitive_signature_dropped: RefusalCounter,
    /// A shader handle in `D3D12DDIARG_CREATE_PIPELINE_STATE_0099` was created
    /// on a different stage's slot than the field it arrived in. ⛔ Expected 0 —
    /// this is a claim about the runtime's own argument struct, and a hit means
    /// the pipeline is being built with a shader in the wrong stage.
    pub(crate) shader_stage_mismatch: RefusalCounter,
    /// A shader's create-time `hRootSignature` disagreed with the PSO's.
    ///
    /// ⚠ Expected 0. It is legal for a shader to be created with a null root
    /// signature and used in a pipeline that has one — but this driver builds
    /// the pipeline from the PSO's, so a disagreement is the earliest point at
    /// which "the shader was compiled against a different binding layout" is
    /// observable. vkd3d would otherwise surface it much later, as a binding
    /// mismatch with no line pointing here.
    pub(crate) shader_root_signature_mismatch: RefusalCounter,
    /// `pfnCreateGeometryShaderWithStreamOutput` created the geometry shader and
    /// **dropped the stream-output declaration**, exactly as the shipping D3D11
    /// driver does (`umd/src/forward/shaders.rs:676-684`). ⚠ Expected 0. When it
    /// moves: `SOSetTargets` binds buffers that are never written and `DrawAuto`
    /// reads zero vertices, so the app renders nothing.
    pub(crate) gs_stream_output_dropped: RefusalCounter,

    // ── root signatures (e) ────────────────────────────────────────────────
    /// A root-signature create was handed a null arg, a null parsed signature,
    /// a count this driver refuses, or an unreadable root parameter. Expected 0.
    pub(crate) root_sig_bad_arg: RefusalCounter,
    /// `D3D12DDIARG_CREATE_ROOT_SIGNATURE_0100::Version` was neither `1_1` nor
    /// `1_2`. ⛔ Expected 0 — those are the only two enumerators the header
    /// defines, and `DDI_REFERENCE.md` §9.9 records that the runtime
    /// up-converts 1.0 before the driver sees it.
    pub(crate) root_sig_version_unknown: RefusalCounter,
    /// A root signature was serialized as **version 1.0** because that is the
    /// only version the bridged engine export accepts
    /// (`libs/vkd3d/vkd3d_main.c:464-468`).
    ///
    /// ⚠ **Expected non-zero — one per root signature — and that is not a
    /// fault.** It is the denominator for the two flag-drop counters below and
    /// the number that says how much a versioned bridge entry point would buy.
    pub(crate) root_sig_downgraded_to_1_0: RefusalCounter,
    /// A descriptor range or root descriptor carried non-`NONE` flags that the
    /// 1.0 serialization cannot express. ⚠ Expected non-zero on any app that
    /// uses 1.1 root signatures. The loss is **optimisation only**: vkd3d's 1.0
    /// deserializer supplies `DESCRIPTORS_VOLATILE | DATA_VOLATILE`, a strict
    /// superset of what any `STATIC` flag promises.
    pub(crate) root_sig_range_flags_dropped: RefusalCounter,
    /// A static sampler carried `D3D12DDI_SAMPLER_FLAGS_0096` bits
    /// (`UINT_BORDER_COLOR`, `NON_NORMALIZED_COORDINATES`) that the 1.0
    /// serialization cannot express.
    ///
    /// ⛔ **Unlike the range flags, this IS a correctness loss** — a
    /// non-normalised static sampler silently samples at the wrong coordinates.
    /// Expected 0 today; a non-zero reading is the trigger for the versioned
    /// bridge entry point.
    pub(crate) root_sig_sampler_flags_dropped: RefusalCounter,
    /// The engine's serializer returned a failure, or an empty blob. Expected 0.
    pub(crate) root_sig_serialize_failed: RefusalCounter,
    /// `ID3D12Device::CreateRootSignature` refused the re-serialized blob.
    /// ⛔ Expected 0, and a hit is the sharpest possible signal that the
    /// down-conversion above is wrong: the engine parsed what this driver wrote.
    pub(crate) root_sig_engine_failed: RefusalCounter,

    // ── pipeline state (e) ─────────────────────────────────────────────────
    /// How many pipeline-state creates this process has served. ⚠ Not a
    /// refusal; the denominator for the counters below. `D12-G7` measured the
    /// runtime building **two** graphics PSOs inside `D3D12CreateDevice`.
    pub(crate) pso_creates: RefusalCounter,
    /// `pfnCreatePipelineState` was handed a null arg or a null handle slot.
    /// Expected 0.
    pub(crate) pso_bad_arg: RefusalCounter,
    /// A PSO declared `DYNAMIC_DEPTH_BIAS` or `DYNAMIC_INDEX_BUFFER_STRIP_CUT`
    /// and the flag was forwarded to the engine.
    ///
    /// ⚠ **An instrument, not an exposure — REGRADED at S6 Round 2, against the
    /// engine's source.** It first read: *"this counter is a cross-lane
    /// obligation … while L3a's `pfnSetPipelineState` does not re-apply the baked
    /// values, a non-zero reading means those pipelines render with whatever
    /// depth bias was last set."* The premise — `SUBSTRATE.md` §4.5's *"the
    /// `DYNAMIC_*` flags are HINTS and the DDI still requires the PSO's own
    /// depth-bias and strip-cut to be applied on every `pfnSetPipelineState`, the
    /// precise inverse of Vulkan's dynamic-state rule"* — is correct and
    /// unchanged. ⛔ **The conclusion was wrong: vkd3d already does it**, in
    /// `d3d12_command_list_SetPipelineState` itself
    /// (`vkd3d-proton-helios/libs/vkd3d/command.c:12711-12733`):
    ///
    /// > *"For any optionally dynamic state, we need to re-apply the
    /// > corresponding static state that the PSO was created with."*
    ///
    /// — re-applying `rs_desc.depthBias{ConstantFactor,Clamp,SlopeFactor}`
    /// whenever `explicit_dynamic_states & VKD3D_DYNAMIC_STATE_DEPTH_BIAS` (which
    /// is the bit `state.c:6064` sets from this very flag), and
    /// `index_buffer_strip_cut_value` unconditionally for graphics pipelines
    /// (`command.c:12728-12733`). ⇒ **forwarding `pfnSetPipelineState` discharges
    /// the obligation**, and re-applying it in `cmdlist.rs` as well would issue
    /// the state twice.
    ///
    /// ⛔ **So the reading to be alarmed by is not this one.** A non-zero count
    /// here is just "an application used the dynamic-state flags". What would be
    /// a finding is this counter moving while `pfnSetPipelineState` is **still a
    /// counting noop** — because then nothing forwards and nothing re-applies.
    /// `D3D12 noop DDI hits:`'s `pfnSetPipelineState` entry is the other half of
    /// that reading, and the two must be read together.
    ///
    /// ⚠ ⭐ Regraded rather than deleted, and the reason is the S6 Round 1
    /// lesson in its own words: *a counter's grading is a claim, and it goes
    /// stale like any other.* Two counters were caught mis-graded in one merge
    /// there; this is the third, caught by reading the engine instead of the
    /// document that predicted it.
    pub(crate) pso_dynamic_state_flag_forwarded: RefusalCounter,
    /// `NumRenderTargets` exceeded the eight `D3D12_RT_FORMAT_ARRAY` holds and
    /// was clamped. ⛔ Expected 0 — `D3D12DDIARG_CREATE_PIPELINE_STATE_0099`'s
    /// own `RTVFormats` array is 8 long.
    pub(crate) pso_render_target_count_clamped: RefusalCounter,
    /// A PSO asked for more view instances than `D3D12_MAX_VIEW_INSTANCE_COUNT`
    /// and the whole set was dropped. ⛔ Expected 0.
    pub(crate) pso_view_instancing_refused: RefusalCounter,
    /// The engine's `ID3D12Device` did not answer `ID3D12Device2`. ⛔ Expected 0
    /// — `libs/vkd3d/device.c:4639` lists `IID_ID3D12Device2` — and a hit means
    /// every graphics pipeline fails, because the stream encoding has no
    /// fallback.
    pub(crate) pso_no_device2: RefusalCounter,
    /// The engine refused to create the pipeline. ⚠ Expected 0 on a healthy
    /// run; when it moves, the engine has already logged its own reason and the
    /// HRESULT is returned to the runtime, which is why this arm returns the
    /// engine's code rather than a substitute.
    pub(crate) pso_engine_failed: RefusalCounter,

    // ── pipeline libraries (e) ─────────────────────────────────────────────
    /// A pipeline-library slot was called and refused with `E_NOTIMPL`. ⚠
    /// Expected 0 on dwm and on most apps; non-zero means an application is
    /// asking for a persistent PSO cache and falling back to direct creation.
    pub(crate) pipeline_library_refused: RefusalCounter,
    /// `pfnDestroyPipelineLibrary` on a library that was never created. ⛔
    /// Expected 0 — [`create_pipeline_library`] refuses every one, so reaching
    /// the destroy means the runtime tracked a library this driver does not
    /// have.
    pub(crate) pipeline_library_unexpected: RefusalCounter,

    // ── the error channel ──────────────────────────────────────────────────
    /// A `VOID` slot needed to report a failure and could not resolve the
    /// device. Expected 0.
    pub(crate) set_error_no_device: RefusalCounter,
    /// A `VOID` slot needed to report a failure and the runtime supplied no
    /// `pfnSetErrorCb`. ⛔ Expected 0, and a hit is serious: it is the only
    /// channel a `VOID` slot has, so losing it turns an error into corrupt
    /// output instead of a removed device.
    pub(crate) set_error_cb_absent: RefusalCounter,
    /// An `HRESULT`-returning slot could not reach the device or the engine.
    /// ⚠ Expected 0 — these are device-scope DDIs and a device exists by
    /// construction — but counted because "unreachable by construction" is a
    /// claim about a cross-FFI contract and this is where it would be observed
    /// breaking.
    pub(crate) no_device: RefusalCounter,

    // ── appended after the first review pass ───────────────────────────────
    // ⛔ These three belong topically to the (b) and (e) groups above and are
    // NOT filed there, because the ⛔ on this struct is append-only and the
    // printed order is the evidence contract. Read them with their groups.
    /// A graphics PSO named a **null** blend / rasterizer / depth-stencil
    /// handle, so that subobject was written from the API defaults
    /// ([`SubStateDesc::default_desc`]) rather than from runtime state.
    ///
    /// ⚠ Expected 0: the D3D12 runtime folds all three into every graphics PSO
    /// descriptor, so it should always have created the objects first. Not a
    /// fault when it moves — the defaults are the documented ones — but it is
    /// the first thing to check if a pipeline renders with state nobody set.
    pub(crate) pso_sub_state_absent: RefusalCounter,
    /// A graphics PSO named a **non-null** blend / rasterizer / depth-stencil
    /// handle whose private block was empty, i.e. whose `pfnCreate*State`
    /// refused. The API defaults were used.
    ///
    /// ⛔ Expected 0, and it moves in lock-step with `L6SubStateBadArg`: the
    /// pipeline is being built from state the app never asked for.
    pub(crate) pso_sub_state_unresolved: RefusalCounter,
    /// `ViewInstanceCount` was non-zero with a **null** `pViewInstanceLocations`
    /// and the pipeline was refused with `E_INVALIDARG`.
    ///
    /// ⛔ Expected 0 — the descriptor is not legal, and vkd3d dereferences that
    /// array unconditionally (`state.c:5347-5349`), so forwarding it would fault
    /// inside the engine. The sibling arm is `L6PsoViewInstancingRefused`.
    pub(crate) pso_view_instancing_bad_arg: RefusalCounter,
}

pub(crate) static L6_REFUSALS: L6Refusals = L6Refusals {
    sub_state_bad_arg: RefusalCounter::new("L6SubStateBadArg"),
    rasterizer_scissor_disabled: RefusalCounter::new("L6RasterizerScissorDisabled"),
    depth_stencil_face_enable_folded: RefusalCounter::new("L6DepthStencilFaceEnableFolded"),
    library_reference_ignored: RefusalCounter::new("L6LibraryReferenceIgnored"),
    shader_creates: RefusalCounter::new("L6ShaderCreates"),
    shader_bad_arg: RefusalCounter::new("L6ShaderBadArg"),
    shader_length_unknown: RefusalCounter::new("L6ShaderLengthUnknown"),
    shader_dxbc_container_seen: RefusalCounter::new("L6ShaderDxbcContainerSeen"),
    shader_program_kind_mismatch: RefusalCounter::new("L6ShaderProgramKindMismatch"),
    shader_signature_count_refused: RefusalCounter::new("L6ShaderSignatureCountRefused"),
    mesh_primitive_signature_dropped: RefusalCounter::new("L6MeshPrimitiveSignatureDropped"),
    shader_stage_mismatch: RefusalCounter::new("L6ShaderStageMismatch"),
    shader_root_signature_mismatch: RefusalCounter::new("L6ShaderRootSignatureMismatch"),
    gs_stream_output_dropped: RefusalCounter::new("L6GsStreamOutputDropped"),
    root_sig_bad_arg: RefusalCounter::new("L6RootSigBadArg"),
    root_sig_version_unknown: RefusalCounter::new("L6RootSigVersionUnknown"),
    root_sig_downgraded_to_1_0: RefusalCounter::new("L6RootSigDowngradedTo10"),
    root_sig_range_flags_dropped: RefusalCounter::new("L6RootSigRangeFlagsDropped"),
    root_sig_sampler_flags_dropped: RefusalCounter::new("L6RootSigSamplerFlagsDropped"),
    root_sig_serialize_failed: RefusalCounter::new("L6RootSigSerializeFailed"),
    root_sig_engine_failed: RefusalCounter::new("L6RootSigEngineFailed"),
    pso_creates: RefusalCounter::new("L6PsoCreates"),
    pso_bad_arg: RefusalCounter::new("L6PsoBadArg"),
    pso_dynamic_state_flag_forwarded: RefusalCounter::new("L6PsoDynamicStateFlagForwarded"),
    pso_render_target_count_clamped: RefusalCounter::new("L6PsoRenderTargetCountClamped"),
    pso_view_instancing_refused: RefusalCounter::new("L6PsoViewInstancingRefused"),
    pso_no_device2: RefusalCounter::new("L6PsoNoDevice2"),
    pso_engine_failed: RefusalCounter::new("L6PsoEngineFailed"),
    pipeline_library_refused: RefusalCounter::new("L6PipelineLibraryRefused"),
    pipeline_library_unexpected: RefusalCounter::new("L6PipelineLibraryUnexpected"),
    set_error_no_device: RefusalCounter::new("L6SetErrorNoDevice"),
    set_error_cb_absent: RefusalCounter::new("L6SetErrorCbAbsent"),
    no_device: RefusalCounter::new("L6NoDevice"),
    pso_sub_state_absent: RefusalCounter::new("L6PsoSubStateAbsent"),
    pso_sub_state_unresolved: RefusalCounter::new("L6PsoSubStateUnresolved"),
    pso_view_instancing_bad_arg: RefusalCounter::new("L6PsoViewInstancingBadArg"),
};

/// L6's refusal counters, printed by `crate::log_refusal_summary` at this
/// lane's position in `lib.rs`'s `UMD12_REFUSAL_SETS`.
///
/// ⛔ **Append only**, in declaration order — see [`L6Refusals`].
pub(crate) static REFUSALS: &[&RefusalCounter] = &[
    &L6_REFUSALS.sub_state_bad_arg,
    &L6_REFUSALS.rasterizer_scissor_disabled,
    &L6_REFUSALS.depth_stencil_face_enable_folded,
    &L6_REFUSALS.library_reference_ignored,
    &L6_REFUSALS.shader_creates,
    &L6_REFUSALS.shader_bad_arg,
    &L6_REFUSALS.shader_length_unknown,
    &L6_REFUSALS.shader_dxbc_container_seen,
    &L6_REFUSALS.shader_program_kind_mismatch,
    &L6_REFUSALS.shader_signature_count_refused,
    &L6_REFUSALS.mesh_primitive_signature_dropped,
    &L6_REFUSALS.shader_stage_mismatch,
    &L6_REFUSALS.shader_root_signature_mismatch,
    &L6_REFUSALS.gs_stream_output_dropped,
    &L6_REFUSALS.root_sig_bad_arg,
    &L6_REFUSALS.root_sig_version_unknown,
    &L6_REFUSALS.root_sig_downgraded_to_1_0,
    &L6_REFUSALS.root_sig_range_flags_dropped,
    &L6_REFUSALS.root_sig_sampler_flags_dropped,
    &L6_REFUSALS.root_sig_serialize_failed,
    &L6_REFUSALS.root_sig_engine_failed,
    &L6_REFUSALS.pso_creates,
    &L6_REFUSALS.pso_bad_arg,
    &L6_REFUSALS.pso_dynamic_state_flag_forwarded,
    &L6_REFUSALS.pso_render_target_count_clamped,
    &L6_REFUSALS.pso_view_instancing_refused,
    &L6_REFUSALS.pso_no_device2,
    &L6_REFUSALS.pso_engine_failed,
    &L6_REFUSALS.pipeline_library_refused,
    &L6_REFUSALS.pipeline_library_unexpected,
    &L6_REFUSALS.set_error_no_device,
    &L6_REFUSALS.set_error_cb_absent,
    &L6_REFUSALS.no_device,
    &L6_REFUSALS.pso_sub_state_absent,
    &L6_REFUSALS.pso_sub_state_unresolved,
    &L6_REFUSALS.pso_view_instancing_bad_arg,
];
