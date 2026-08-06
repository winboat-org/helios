//! L5 — descriptor heaps and views.
//!
//! Owns 15 of `DEVICE_FUNCS_CORE_0109` (group (f), `DDI_REFERENCE.md` §3.2).
//! ⭐ The brief for this lane listed **14**; the fifteenth is
//! `pfnCreateSamplerFeedbackUnorderedAccessView` (`_0075`), which §3.2 lists
//! inside group (f) and which the `_0109` struct places far from its siblings
//! (`umd12/bindgen/cached/d3d12umddi.rs:87681`, between `pfnAddToStateObject`
//! and `pfnCreateAmplificationShader`) because it was appended later. The group
//! count of 15 is correct and it is this lane's.
//!
//! # ⭐ H3's good surprise, re-confirmed from the bindgen output
//!
//! `D3D12DDI_CPU_DESCRIPTOR_HANDLE` is `{ SIZE_T ptr }` (`:50614`) and
//! `D3D12DDI_GPU_DESCRIPTOR_HANDLE` is `{ UINT64 ptr }` (`:50628`) — **opaque
//! driver-chosen scalars**, and `pfnGetDescriptorSizeInBytes` (`:51936`) lets
//! the driver pick the stride. So this lane returns **vkd3d's own handle values
//! and increment verbatim** and keeps no shadow table:
//! `vkd3d-proton-helios/libs/vkd3d/resource.c:9146-9167` hands back
//! `heap->cpu_va` / `heap->gpu_va` and `libs/vkd3d/device.c:6505` the increment,
//! and the runtime's `base + i * stride` arithmetic then lands on vkd3d's own
//! because it *is* vkd3d's stride (`DDI_REFERENCE.md` §9.6).
//!
//! ⚠ `ResourceBinding.md:4399-4403` warns that the runtime may apply "a cheap
//! scale/shift" to handles crossing between app and driver. That does not
//! disturb the plan: a handle seen at the DDI is already in the **driver's**
//! space, and the driver's space is vkd3d's.
//!
//! # ⛔ HAZARD 1 — the `ead692e` struct-return ABI, and it is SAFE here
//!
//! `pfnGetCPU/GPUDescriptorHandleForHeapStart` return the handle **by value**
//! while vkd3d's C implementation returns it through a hidden out-pointer. That
//! is the truncation class that crash-looped dwm and LogonUI at cold boot. Both
//! halves were read at the source this session:
//!
//! * **vkd3d** — `libs/vkd3d/resource.c:9146-9147`:
//!   `static D3D12_CPU_DESCRIPTOR_HANDLE * STDMETHODCALLTYPE`
//!   `d3d12_descriptor_heap_GetCPUDescriptorHandleForHeapStart(`
//!   `ID3D12DescriptorHeap *iface, D3D12_CPU_DESCRIPTOR_HANDLE *descriptor)`
//!   — hidden out-pointer. Same shape at `:9158` for the GPU sibling. It is the
//!   `widl` C convention: `include/vkd3d_d3d12.idl:3193` declares the method
//!   as returning the struct, and widl lowers an aggregate return to a trailing
//!   `__ret` pointer.
//! * **windows-rs 0.58** —
//!   `~/.cargo/registry/src/*/windows-0.58.0/src/Windows/Win32/Graphics/Direct3D12/mod.rs:652`:
//!   `pub GetCPUDescriptorHandleForHeapStart:`
//!   `unsafe extern "system" fn(*mut core::ffi::c_void, *mut D3D12_CPU_DESCRIPTOR_HANDLE),`
//!   and the safe wrapper at `:635` zeroes a local, passes `&mut result__` and
//!   returns it by value.
//!
//! ⇒ **The two conventions agree**, so calling the engine through the `windows`
//! crate's `ID3D12DescriptorHeap` is correct and **no cxx/C++ shim is needed**.
//! The C++ route would also have taken this whole lane off the Linux host
//! cross-check (`PARALLEL.md` §7), which is the same trade `caps12` recorded.
//!
//! ⚠ **The direction still open is the DDI side, not the engine side.**
//! `PFND3D12DDI_GET_CPU_DESCRIPTOR_HANDLE_FOR_HEAP_START` (`:51939`) returns the
//! one-`usize` `#[repr(C)]` struct by value, and a Rust `unsafe extern "C" fn`
//! returns an 8-byte POD in RAX — which is what an MSVC-compiled runtime expects
//! of it. If a handle ever comes back wrong, that is where to look.
//! [`DESCRIPTOR_REFUSALS.heap_cpu_handle_zero`] is the canary: vkd3d always
//! assigns a real host pointer to `cpu_va` (`resource.c:10367-10420`), so a zero
//! CPU handle cannot be the engine's answer and must be a truncation.
//!
//! # ⛔ HAZARD 2 — the descriptor-heap flags collide on `0x1`
//!
//! ```text
//! DDI (d3d12umddi.h:819-823)                    API (d3d12.h:3979-3980)
//! FLAG_NONE           = 0x0                     FLAG_NONE           = 0
//! FLAG_CPU_VISIBLE    = 0x1                     FLAG_SHADER_VISIBLE = 0x1
//! FLAG_SHADER_VISIBLE = 0x2                     (no third value)
//! ```
//!
//! Passing `pArgs->Flags` through therefore **inverts** them with no error
//! (`DX12.md` §4.3 row 4, `DDI_REFERENCE.md` §9.6.1): a DDI `CPU_VISIBLE` heap
//! would become an API *shader-visible* heap. [`api_heap_flags`] translates, and
//! a `const _` below pins the collision so it is machine-visible rather than
//! prose.
//!
//! # ⭐ The L4 seam, and how it was closed
//!
//! Five slots — `pfnCreateShaderResourceView`, `pfnCreateUnorderedAccessView`,
//! `pfnCreateRenderTargetView`, `pfnCreateDepthStencilView` and
//! `pfnCreateSamplerFeedbackUnorderedAccessView` — need the engine
//! `ID3D12Resource` behind a `D3D12DDI_HRESOURCE`. That handle's private payload
//! belongs to **L4** (`forward12/resource12.rs`, groups (g)+(h)); declaring it
//! here would be R803's exact scar — choosing a slot's payload at the call site
//! — and would collide with L4's own `com_handles!`/`boxed_handles!` invocation
//! at merge. So this lane shipped [`engine_resource`] as a one-function seam,
//! a loud `STUB:` with its own counter, and everything below it as the real
//! bodies.
//!
//! ⭐ **L4 landed in the same batch, so the seam is CLOSED** —
//! [`engine_resource`] forwards to `resource12::engine_resource` and carries
//! L4's borrowed `&ID3D12Resource` through [`ViewResource`]. ⛔ The consequence
//! that matters for reading a gate log: `ViewResourceUnavailable` is now
//! **expected 0**, and a hit means a non-null handle this driver failed to
//! resolve — which removes the device.
//!
//! ⛔ **A null `hDrvResource` is NOT that seam.** D3D12's null-descriptor form
//! — `CreateShaderResourceView(nullptr, &desc, handle)` for an unbound table
//! slot — is a legal call the runtime lowers with a null resource handle, and
//! it is answered today, without L4: [`view_resource`] classifies it before
//! [`engine_resource`] can flatten it into the same `None` an unresolvable
//! handle produces. Counting it as a refusal would have met a legal call with
//! `pfnSetErrorCb`, and the runtime's answer to that is *"Removing device due
//! to bad UMD error"*.
//!
//! # ⚠ The substrate ceiling this lane sits on
//!
//! `MaxSamplerDescriptorHeapSize` must be reported >= 4000 at DDI 0102+
//! (`SUBSTRATE.md` §4.5, `VulkanOn12.md:1122`) and the host GPU's
//! `maxSamplerAllocationCount` is **exactly 4000**
//! (`docs/reference/host-vulkan-profile-rtx-pro-6000-blackwell.json:882`) — zero
//! headroom *if* vkd3d allocates one `VkSampler` per descriptor.
//! [`DESCRIPTOR_REFUSALS.sampler_creates`] is the instrument for that open
//! question (`GATES.md` §7.24): it counts every `pfnCreateSampler`, which is the
//! budget consumed under the pessimistic reading.
//!
//! ⭐ And the VulkanOn12 obligation that lands squarely here:
//! **non-normalized sampler coordinates are mandatory at DDI 0100+**, so
//! `pfnCreateSampler` must carry `D3D12DDI_SAMPLER_FLAG_NON_NORMALIZED_COORDINATES`
//! through — which is why it forwards to `ID3D12Device11::CreateSampler2` (the
//! flag-carrying form) and not to the flag-less `ID3D12Device::CreateSampler`.
//! vkd3d honours both flags at `libs/vkd3d/resource.c:8419` and `:8484`.

use helios_umd_common::com_handles;
use helios_umd_common::hr::{Hresult, E_FAIL, E_INVALIDARG, S_OK};
use helios_umd_common::refusals::RefusalCounter;
use helios_umd_common::slot::{Com, DdiHandle, Slot};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::{
    ID3D12DescriptorHeap, ID3D12Device11, ID3D12Resource, D3D12_BUFFER_RTV, D3D12_BUFFER_SRV,
    D3D12_BUFFER_SRV_FLAGS, D3D12_BUFFER_SRV_FLAG_NONE, D3D12_BUFFER_SRV_FLAG_RAW,
    D3D12_BUFFER_UAV, D3D12_BUFFER_UAV_FLAGS, D3D12_BUFFER_UAV_FLAG_NONE,
    D3D12_BUFFER_UAV_FLAG_RAW, D3D12_COMPARISON_FUNC, D3D12_CONSTANT_BUFFER_VIEW_DESC,
    D3D12_CPU_DESCRIPTOR_HANDLE,
    D3D12_DEPTH_STENCIL_VIEW_DESC, D3D12_DEPTH_STENCIL_VIEW_DESC_0, D3D12_DESCRIPTOR_HEAP_DESC,
    D3D12_DESCRIPTOR_HEAP_FLAGS, D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
    D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, D3D12_DESCRIPTOR_HEAP_TYPE,
    D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
    D3D12_DESCRIPTOR_HEAP_TYPE_RTV, D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, D3D12_DSV_DIMENSION,
    D3D12_DSV_DIMENSION_TEXTURE1D,
    D3D12_DSV_DIMENSION_TEXTURE1DARRAY, D3D12_DSV_DIMENSION_TEXTURE2D,
    D3D12_DSV_DIMENSION_TEXTURE2DARRAY, D3D12_DSV_DIMENSION_TEXTURE2DMS,
    D3D12_DSV_DIMENSION_TEXTURE2DMSARRAY, D3D12_DSV_FLAGS, D3D12_DSV_FLAG_NONE,
    D3D12_DSV_FLAG_READ_ONLY_DEPTH, D3D12_DSV_FLAG_READ_ONLY_STENCIL, D3D12_FILTER,
    // ⬇ L3b: the GPU sibling of `D3D12_CPU_DESCRIPTOR_HANDLE`, for
    // [`api_gpu_handle`]. Type-only, like every other name in this list.
    D3D12_GPU_DESCRIPTOR_HANDLE,
    D3D12_RAYTRACING_ACCELERATION_STRUCTURE_SRV, D3D12_RENDER_TARGET_VIEW_DESC,
    D3D12_RENDER_TARGET_VIEW_DESC_0, D3D12_RESOURCE_DESC, D3D12_RTV_DIMENSION,
    D3D12_RTV_DIMENSION_BUFFER,
    D3D12_RTV_DIMENSION_TEXTURE1D, D3D12_RTV_DIMENSION_TEXTURE1DARRAY,
    D3D12_RTV_DIMENSION_TEXTURE2D, D3D12_RTV_DIMENSION_TEXTURE2DARRAY,
    D3D12_RTV_DIMENSION_TEXTURE2DMS, D3D12_RTV_DIMENSION_TEXTURE2DMSARRAY,
    D3D12_RTV_DIMENSION_TEXTURE3D, D3D12_SAMPLER_DESC2, D3D12_SAMPLER_DESC2_0, D3D12_SAMPLER_FLAGS,
    D3D12_SAMPLER_FLAG_NONE, D3D12_SAMPLER_FLAG_NON_NORMALIZED_COORDINATES,
    D3D12_SAMPLER_FLAG_UINT_BORDER_COLOR, D3D12_SHADER_RESOURCE_VIEW_DESC,
    D3D12_SHADER_RESOURCE_VIEW_DESC_0, D3D12_SRV_DIMENSION_BUFFER,
    D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE, D3D12_SRV_DIMENSION_TEXTURE1D,
    D3D12_SRV_DIMENSION_TEXTURE1DARRAY, D3D12_SRV_DIMENSION_TEXTURE2D,
    D3D12_SRV_DIMENSION_TEXTURE2DARRAY, D3D12_SRV_DIMENSION_TEXTURE2DMS,
    D3D12_SRV_DIMENSION_TEXTURE2DMSARRAY, D3D12_SRV_DIMENSION_TEXTURE3D,
    D3D12_SRV_DIMENSION_TEXTURECUBE, D3D12_SRV_DIMENSION_TEXTURECUBEARRAY, D3D12_TEX1D_ARRAY_DSV,
    D3D12_TEX1D_ARRAY_RTV, D3D12_TEX1D_ARRAY_SRV, D3D12_TEX1D_ARRAY_UAV, D3D12_TEX1D_DSV,
    D3D12_TEX1D_RTV, D3D12_TEX1D_SRV, D3D12_TEX1D_UAV, D3D12_TEX2DMS_ARRAY_DSV,
    D3D12_TEX2DMS_ARRAY_RTV, D3D12_TEX2DMS_ARRAY_SRV, D3D12_TEX2DMS_ARRAY_UAV, D3D12_TEX2DMS_DSV,
    D3D12_TEX2DMS_RTV, D3D12_TEX2DMS_SRV, D3D12_TEX2DMS_UAV, D3D12_TEX2D_ARRAY_DSV,
    D3D12_TEX2D_ARRAY_RTV, D3D12_TEX2D_ARRAY_SRV, D3D12_TEX2D_ARRAY_UAV, D3D12_TEX2D_DSV,
    D3D12_TEX2D_RTV, D3D12_TEX2D_SRV, D3D12_TEX2D_UAV, D3D12_TEX3D_RTV, D3D12_TEX3D_SRV,
    D3D12_TEX3D_UAV, D3D12_TEXCUBE_ARRAY_SRV, D3D12_TEXCUBE_SRV, D3D12_TEXTURE_ADDRESS_MODE,
    D3D12_UAV_DIMENSION_BUFFER,
    D3D12_UAV_DIMENSION_TEXTURE1D, D3D12_UAV_DIMENSION_TEXTURE1DARRAY,
    D3D12_UAV_DIMENSION_TEXTURE2D, D3D12_UAV_DIMENSION_TEXTURE2DARRAY,
    D3D12_UAV_DIMENSION_TEXTURE2DMS, D3D12_UAV_DIMENSION_TEXTURE2DMSARRAY,
    D3D12_UAV_DIMENSION_TEXTURE3D, D3D12_UNORDERED_ACCESS_VIEW_DESC,
    D3D12_UNORDERED_ACCESS_VIEW_DESC_0,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;

use super::tables12::{stage, DeviceCoreTable, Filling};
use crate::device12::{self, HeliosD3D12Device};
use crate::{ddi12, log_error, note_refusal, trace_line};

// ---------------------------------------------------------------------------
// The heap handle's payload
// ---------------------------------------------------------------------------

// ⭐ The payload is decided by the HANDLE TYPE, once, here — never at a call
// site. `umd_common::slot`'s module doc records what the other way cost (R803):
// `load_com::<ID3D11RenderTargetView>(h_rtv)` compiled and produced a
// `ManuallyDrop` whose vtable pointer was a struct field, a wild call on first
// use.
//
// ⭐ `Com` rather than `Boxed`, and that is worth stating: this lane keeps **no
// per-heap state at all** (H3 above), so the whole `Slot<Boxed<S>>::get()`
// soundness question — which `umd_common::slot:304-322` says is established for
// D3D11 and **not** for D3D12, and which `PARALLEL.md` §9.4 forbids inheriting —
// simply does not arise here. `Slot<Com<T>>::load` hands back a `ManuallyDrop`
// borrowed from a machine word inside the runtime's own per-object allocation;
// it dereferences nothing this driver allocated.
com_handles!(crate::ddi12::D3D12DDI_HDESCRIPTORHEAP);

/// The one machine word this driver stores in a descriptor heap's private
/// block: the owning `ID3D12DescriptorHeap*`.
///
/// ⛔ `pfnCalcPrivateDescriptorHeapSize` returns this and nothing else, so it is
/// a constant rather than a function of `pArgs` — unlike
/// `device12::device_private_size`, whose answer legitimately depends on
/// `DEBUGGABLE`. Stated because the two look like they should have the same
/// shape and must not.
///
/// ⚠ Typed `ddi12::SIZE_T` (`= c_ulonglong`, `d3d12umddi.rs:352-353`) rather than
/// `usize`, because that is what `PFND3D12DDI_CALC_PRIVATE_DESCRIPTOR_HEAP_SIZE`
/// returns. The two are the same width on this target - see the assertion beside
/// [`api_cpu_handle`], which is what the descriptor-handle casts rest on.
const HEAP_PRIVATE_SIZE: ddi12::SIZE_T = core::mem::size_of::<usize>() as ddi12::SIZE_T;

/// Borrow the slot behind a descriptor-heap handle.
///
/// # Safety
/// `h_heap.pDrvPrivate`, when non-null, must address the private block the
/// runtime allocated using [`HEAP_PRIVATE_SIZE`] for a heap of this kind.
unsafe fn heap_slot(
    h_heap: ddi12::D3D12DDI_HDESCRIPTORHEAP,
) -> Option<Slot<Com<ID3D12DescriptorHeap>>> {
    // SAFETY: the caller's precondition is `Slot::from_priv`'s precondition
    // verbatim; the null case is folded into `None` by `from_priv` itself.
    unsafe { Slot::from_priv(h_heap.drv_private()) }
}

// ---------------------------------------------------------------------------
// ⬇ L3b's accessor into this file — ONE function (`PARALLEL.md` §4)
// ---------------------------------------------------------------------------

/// The engine `ID3D12DescriptorHeap` behind a DDI descriptor-heap handle,
/// **borrowed**.
///
/// ⭐ `pub(crate)` because it is the seam L3b needs and the only legal route to
/// it: `pfnSetDescriptorHeaps` is handed an array of `D3D12DDI_HDESCRIPTORHEAP`
/// and must give the engine the `ID3D12DescriptorHeap`s behind them. R803's scar
/// is that a slot's payload must be derived from the handle **type**, in one
/// place, never chosen at the call site — this file owns descriptor-heap
/// handles, so this is that one place. Same shape and same reason as
/// `resource12::engine_resource`.
///
/// ⚠ A `ManuallyDrop`, i.e. **borrowed**: [`create_descriptor_heap`] moved the
/// one owning reference into the slot and [`destroy_descriptor_heap`] is what
/// releases it. Dropping the returned value would release the slot's reference;
/// `ManuallyDrop` makes that drop a no-op.
///
/// ⚠ It is not a `&ID3D12DescriptorHeap` the way `resource12::engine_resource`
/// is, and that is a property of the payload rather than a weaker choice: L4's
/// slot holds a `Box<ResourceState>` that *contains* an `ID3D12Resource` value
/// to borrow from, while this slot holds the bare COM pointer itself (see the
/// `com_handles!` comment above) — there is no owned interface value in this
/// driver's memory here.
///
/// ⛔ A null `h_heap.pDrvPrivate` and an empty slot both fold to `None`, so a
/// caller that needs to tell "the runtime named no heap" from "this driver
/// failed to resolve a heap the runtime named" must check `pDrvPrivate` itself
/// **before** calling — exactly as [`view_resource`] does for resources, and for
/// the same reason.
///
/// # Safety
/// As [`heap_slot`]: `h_heap.pDrvPrivate`, when non-null, must address the
/// private block the runtime allocated for a heap of this kind. The returned
/// value must not outlive the DDI call that obtained it.
pub(crate) unsafe fn engine_heap(
    h_heap: ddi12::D3D12DDI_HDESCRIPTORHEAP,
) -> Option<core::mem::ManuallyDrop<ID3D12DescriptorHeap>> {
    // SAFETY: forwarded; the caller carries `heap_slot`'s precondition, and
    // `load` borrows the slot's reference without taking it.
    unsafe { heap_slot(h_heap)?.load() }
}

// ---------------------------------------------------------------------------
// ⬆ end of L3b's accessor
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Log budgets
// ---------------------------------------------------------------------------

/// Bound on one-line-per-event `log_error!` traffic from this lane.
///
/// Same discipline as `caps12`'s: an unbounded `log_error!` on a per-descriptor
/// path writes megabytes per frame, and the per-call channel is `trace_line!`,
/// which is gated on `HKLM\SOFTWARE\Helios!Umd12Trace`.
const LOG_BUDGET: usize = 32;

// ---------------------------------------------------------------------------
// Error reporting for the VOID slots
// ---------------------------------------------------------------------------

/// Report a failure the only way a `VOID` D3D12 DDI can: `pfnSetErrorCb`.
///
/// ⛔ Ten of this lane's fifteen slots return `VOID`, so without this a
/// refused view write is indistinguishable from a successful one and the
/// application renders from a descriptor nobody filled in — precisely the "fake
/// success" `CLAUDE.md` rule 2 forbids. `device12::set_error` returns whether the
/// runtime's callback existed; a missing one is itself counted, because losing
/// the only error channel turns a removed device into corrupt output.
fn report_error(dev: &HeliosD3D12Device, hr: Hresult) {
    if !device12::set_error(dev, hr) {
        note_refusal(&DESCRIPTOR_REFUSALS.set_error_cb_missing);
    }
}

// ---------------------------------------------------------------------------
// Descriptor-heap type and flags translation
// ---------------------------------------------------------------------------

/// DDI heap type -> API heap type, as a **closed** match.
///
/// ⚠ The two enums happen to agree numerically (CBV_SRV_UAV 0, SAMPLER 1, RTV 2,
/// DSV 3 on both sides — DDI at `d3d12umddi.rs:49247-49256`, API at
/// `windows-0.58.0/.../Direct3D12/mod.rs:4342-4346`), and this is still a match
/// and not a cast: `NUM_TYPES` (4) is a legal value of the DDI enum's C type and
/// is not a heap type, and the same numeric agreement is exactly what makes the
/// FLAGS enum below a trap.
fn api_heap_type(t: ddi12::D3D12DDI_DESCRIPTOR_HEAP_TYPE) -> Option<D3D12_DESCRIPTOR_HEAP_TYPE> {
    match t {
        ddi12::D3D12DDI_DESCRIPTOR_HEAP_TYPE_D3D12DDI_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV => {
            Some(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
        }
        ddi12::D3D12DDI_DESCRIPTOR_HEAP_TYPE_D3D12DDI_DESCRIPTOR_HEAP_TYPE_SAMPLER => {
            Some(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER)
        }
        ddi12::D3D12DDI_DESCRIPTOR_HEAP_TYPE_D3D12DDI_DESCRIPTOR_HEAP_TYPE_RTV => {
            Some(D3D12_DESCRIPTOR_HEAP_TYPE_RTV)
        }
        ddi12::D3D12DDI_DESCRIPTOR_HEAP_TYPE_D3D12DDI_DESCRIPTOR_HEAP_TYPE_DSV => {
            Some(D3D12_DESCRIPTOR_HEAP_TYPE_DSV)
        }
        _ => None,
    }
}

/// `D3D12DDI_DESCRIPTOR_HEAP_FLAG_CPU_VISIBLE` (`0x1`).
const DDI_HEAP_FLAG_CPU_VISIBLE: ddi12::D3D12DDI_DESCRIPTOR_HEAP_FLAGS =
    ddi12::D3D12DDI_DESCRIPTOR_HEAP_FLAGS_D3D12DDI_DESCRIPTOR_HEAP_FLAG_CPU_VISIBLE;
/// `D3D12DDI_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE` (`0x2`).
const DDI_HEAP_FLAG_SHADER_VISIBLE: ddi12::D3D12DDI_DESCRIPTOR_HEAP_FLAGS =
    ddi12::D3D12DDI_DESCRIPTOR_HEAP_FLAGS_D3D12DDI_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE;
/// Every DDI heap flag this build's header defines.
const DDI_HEAP_FLAGS_KNOWN: ddi12::D3D12DDI_DESCRIPTOR_HEAP_FLAGS =
    DDI_HEAP_FLAG_CPU_VISIBLE | DDI_HEAP_FLAG_SHADER_VISIBLE;

// ⛔ THE COLLISION, PINNED. The DDI's CPU_VISIBLE and the API's SHADER_VISIBLE
// are the same bit with different meanings, which is why `pArgs->Flags` must
// never be forwarded. Asserting it here makes the hazard a compile-time fact
// rather than a comment that can rot: if a future header ever separates them,
// this assertion fires and the translation below can be simplified deliberately.
const _: () = assert!(DDI_HEAP_FLAG_CPU_VISIBLE == D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE.0);
const _: () = assert!(D3D12_DESCRIPTOR_HEAP_FLAG_NONE.0 == 0);

/// DDI heap flags -> API heap flags.
///
/// ⛔ Never a cast. The API has exactly one flag, `SHADER_VISIBLE`, and the DDI
/// carries two; `CPU_VISIBLE` is DDI-only (the API deleted it, the DDI kept it),
/// so a DDI heap that is CPU-visible and not shader-visible is precisely an API
/// `NONE` heap — the RTV/DSV/staging shape. `DDI_REFERENCE.md` §9.6.1.
fn api_heap_flags(ddi_flags: ddi12::D3D12DDI_DESCRIPTOR_HEAP_FLAGS) -> D3D12_DESCRIPTOR_HEAP_FLAGS {
    if ddi_flags & DDI_HEAP_FLAG_SHADER_VISIBLE != 0 {
        D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE
    } else {
        D3D12_DESCRIPTOR_HEAP_FLAG_NONE
    }
}

// ---------------------------------------------------------------------------
// pfnCalcPrivateDescriptorHeapSize / pfnCreateDescriptorHeap / pfnDestroyDescriptorHeap
// ---------------------------------------------------------------------------

/// `pfnCalcPrivateDescriptorHeapSize`.
///
/// ⚠ Returns [`HEAP_PRIVATE_SIZE`] **unconditionally**, including for a null
/// `pArgs`. That is deliberate and differs from `device12`'s equivalent, which
/// returns 0 on a null arg: there the size is a function of `Flags`, so refusing
/// to size a block nobody described is honest; here it is a constant, and
/// returning 0 would hand `pfnCreateDescriptorHeap` a private block too small
/// for the word it must write. The null arg is counted instead.
///
/// # Safety
/// `arg`, when non-null, must point at a live
/// `D3D12DDIARG_CREATE_DESCRIPTOR_HEAP_0001` for the duration of the call.
unsafe extern "C" fn calc_private_descriptor_heap_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_DESCRIPTOR_HEAP_0001,
) -> ddi12::SIZE_T {
    if arg.is_null() {
        note_refusal(&DESCRIPTOR_REFUSALS.heap_bad_arg);
        return HEAP_PRIVATE_SIZE;
    }
    // SAFETY: non-null per the check above; the DDI declares it `_In_ CONST`, so
    // the runtime guarantees a live, aligned struct for the call.
    let a = unsafe { &*arg };
    trace_line!(
        "CalcPrivateDescriptorHeapSize: Type={} NumDescriptors={} Flags={:#x} NodeMask={:#x} -> {}",
        a.Type,
        a.NumDescriptors,
        a.Flags,
        a.NodeMask,
        HEAP_PRIVATE_SIZE,
    );
    HEAP_PRIVATE_SIZE
}

/// `pfnCreateDescriptorHeap`.
///
/// The driver allocates the descriptor storage **entirely**
/// (`DDI_REFERENCE.md` §9.6: the create arg carries no pointer and no size and
/// no callback hands the driver descriptor memory), which for a forwarder means
/// creating a matching `ID3D12DescriptorHeap` on the engine and letting vkd3d's
/// allocator be the driver's.
///
/// # Safety
/// `arg` must point at a live `D3D12DDIARG_CREATE_DESCRIPTOR_HEAP_0001`, and
/// `h_heap.pDrvPrivate` at the [`HEAP_PRIVATE_SIZE`] writable bytes the runtime
/// allocated for this heap.
unsafe extern "C" fn create_descriptor_heap(
    h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_DESCRIPTOR_HEAP_0001,
    h_heap: ddi12::D3D12DDI_HDESCRIPTORHEAP,
) -> ddi12::HRESULT {
    // SAFETY: the caller guarantees the slot; `heap_slot` folds null into `None`.
    let Some(slot) = (unsafe { heap_slot(h_heap) }) else {
        note_refusal(&DESCRIPTOR_REFUSALS.heap_bad_arg);
        return E_INVALIDARG;
    };
    // ⛔ Empty the slot before anything can fail, so a failed create leaves a
    // null handle rather than whatever the runtime's allocator left there. Every
    // `Create*` DDI in `umd` opens the same way (`umd_common::slot`'s `DdiHandle`
    // doc).
    // SAFETY: as above.
    unsafe { slot.clear() };

    if arg.is_null() {
        note_refusal(&DESCRIPTOR_REFUSALS.heap_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: non-null per the check above; `_In_ CONST` per the DDI.
    let a = unsafe { &*arg };

    // SAFETY: this is a device-scope DDI, so the runtime passes a handle
    // `device12::create_device` returned `S_OK` for; the borrow lives only until
    // the end of this call, which is `device12::device`'s stated precondition.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        return E_INVALIDARG;
    };
    let Some(engine) = dev.engine.d3d12_device() else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        return E_FAIL;
    };

    let Some(heap_type) = api_heap_type(a.Type) else {
        // R911: this arm logs its own line, so it bumps rather than
        // `note_refusal`s -- an already-loud arm must not also print the set.
        DESCRIPTOR_REFUSALS.heap_type_unknown.bump();
        log_error!(
            "CreateDescriptorHeap: unknown D3D12DDI_DESCRIPTOR_HEAP_TYPE {} -> E_INVALIDARG",
            a.Type,
        );
        return E_INVALIDARG;
    };
    if a.Flags & !DDI_HEAP_FLAGS_KNOWN != 0 {
        DESCRIPTOR_REFUSALS.heap_flags_unknown.bump();
        log_error!(
            "CreateDescriptorHeap: Flags {:#x} carry bits outside {:#x}; only the known ones are \
             translated",
            a.Flags,
            DDI_HEAP_FLAGS_KNOWN,
        );
    }
    let desc = D3D12_DESCRIPTOR_HEAP_DESC {
        Type: heap_type,
        NumDescriptors: a.NumDescriptors,
        Flags: api_heap_flags(a.Flags),
        // ⚠ Forwarded, not zeroed. Helios is a single-node adapter
        // (`caps12`'s `pfnQueryNodeMap` writes the identity map for one node),
        // so this is 0 or 1 in practice; a value outside that is the
        // multi-adapter assumption of `ARCHITECTURE.md` §13 UNVERIFIED-11 being
        // reached for real, and it is counted rather than silently clamped.
        NodeMask: a.NodeMask,
    };
    if a.NodeMask & !1 != 0 {
        note_refusal(&DESCRIPTOR_REFUSALS.heap_node_mask_unexpected);
    }

    // SAFETY: `desc` is a live local of exactly the struct the method names, and
    // `engine` is the bridge's BORROWED `ID3D12Device` in a `ManuallyDrop` — the
    // call issues no reference-count change against it and the wrapper is never
    // released. The returned heap IS owned, and ownership moves into the slot
    // below.
    let created = unsafe { engine.CreateDescriptorHeap::<ID3D12DescriptorHeap>(&desc) };
    let heap = match created {
        Ok(heap) => heap,
        Err(err) => {
            DESCRIPTOR_REFUSALS.heap_engine_failed.bump();
            let n = DESCRIPTOR_REFUSALS.heap_engine_failed.get();
            if n <= LOG_BUDGET {
                log_error!(
                    "CreateDescriptorHeap: engine refused type={} num={} flags={:#x} \
                     hr={:#010x} (x{n})",
                    a.Type,
                    a.NumDescriptors,
                    a.Flags,
                    err.code().0 as u32,
                );
            }
            return err.code().0;
        }
    };

    trace_line!(
        "CreateDescriptorHeap: ddiType={} ddiFlags={:#x} -> apiFlags={:#x} num={} ok",
        a.Type,
        a.Flags,
        api_heap_flags(a.Flags).0,
        a.NumDescriptors,
    );

    // SAFETY: the slot is the runtime's private block for this heap, sized by
    // this driver's own `pfnCalcPrivateDescriptorHeapSize`, and it was cleared
    // above so nothing is being overwritten. `store` moves the engine's owning
    // reference into it; `destroy_descriptor_heap` is the matching release.
    unsafe { slot.store(heap) };
    S_OK
}

/// `pfnDestroyDescriptorHeap`.
///
/// # Safety
/// `h_heap` must be a handle [`create_descriptor_heap`] returned `S_OK` for and
/// which has not already been destroyed.
unsafe extern "C" fn destroy_descriptor_heap(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_heap: ddi12::D3D12DDI_HDESCRIPTORHEAP,
) {
    // SAFETY: the caller guarantees a live handle from `create_descriptor_heap`.
    let Some(slot) = (unsafe { heap_slot(h_heap) }) else {
        note_refusal(&DESCRIPTOR_REFUSALS.heap_bad_arg);
        return;
    };
    // SAFETY: as above. `release` is idempotent — it reads the word, releases a
    // non-null one and nulls the slot — so a double destroy is a no-op rather
    // than a double free.
    unsafe { slot.release() };
}

// ---------------------------------------------------------------------------
// pfnGetDescriptorSizeInBytes / the two heap-start handles
// ---------------------------------------------------------------------------

/// `pfnGetDescriptorSizeInBytes` — the driver chooses the stride, and this
/// driver's choice is *vkd3d's*.
///
/// ⛔ A 0 here is unrecoverable: the runtime and the application compute every
/// descriptor address as `heapStart + i * stride`, so a zero stride aliases the
/// whole heap onto its first descriptor with no error anywhere. There is no
/// HRESULT to refuse with, so the counter is the only channel.
///
/// # Safety
/// `h_device` must be a live handle from `device12::create_device`.
unsafe extern "C" fn get_descriptor_size_in_bytes(
    h_device: ddi12::D3D12DDI_HDEVICE,
    heap_type: ddi12::D3D12DDI_DESCRIPTOR_HEAP_TYPE,
) -> ddi12::UINT {
    // SAFETY: device-scope DDI; see `create_descriptor_heap`.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        return 0;
    };
    let Some(engine) = dev.engine.d3d12_device() else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        return 0;
    };
    let Some(api_type) = api_heap_type(heap_type) else {
        note_refusal(&DESCRIPTOR_REFUSALS.heap_type_unknown);
        return 0;
    };
    // SAFETY: borrowed `ID3D12Device`, scalar in and scalar out; no ownership
    // crosses this call.
    let stride = unsafe { engine.GetDescriptorHandleIncrementSize(api_type) };
    if stride == 0 {
        DESCRIPTOR_REFUSALS.descriptor_stride_zero.bump();
        let n = DESCRIPTOR_REFUSALS.descriptor_stride_zero.get();
        if n <= LOG_BUDGET {
            log_error!("GetDescriptorSizeInBytes: engine reports stride 0 for type {heap_type} (x{n})");
        }
    }
    trace_line!("GetDescriptorSizeInBytes: type={heap_type} -> {stride}");
    stride
}

/// `pfnGetCPUDescriptorHandleForHeapStart` — vkd3d's own `cpu_va`, verbatim.
///
/// ⛔ **This is the `ead692e` slot.** See the module doc: the DDI returns the
/// handle by value, vkd3d returns it through a hidden out-pointer, and the
/// `windows` crate's vtable declaration is the hidden-pointer form — so the two
/// agree and no shim is needed. [`DESCRIPTOR_REFUSALS.heap_cpu_handle_zero`] is
/// what would show it if that analysis were wrong: vkd3d assigns `cpu_va` a real
/// host allocation pointer on every path (`libs/vkd3d/resource.c:10367-10420`),
/// so a zero can only be a truncation or an empty slot.
///
/// ⚠ `ResourceBinding.md:4426-4427` also allows a legitimate zero — *"if an
/// allocation is not CPU visible, there is no CPU address, so
/// GetCPUDescriptorHandleForHeapStart must return 0"*. vkd3d has no such heap,
/// so on this substrate the counter stays a pure ABI canary.
///
/// # Safety
/// `h_heap` must be a live handle from [`create_descriptor_heap`].
unsafe extern "C" fn get_cpu_descriptor_handle_for_heap_start(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_heap: ddi12::D3D12DDI_HDESCRIPTORHEAP,
) -> ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE {
    let mut out = ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE { ptr: 0 };
    // SAFETY: the caller guarantees a live handle from `create_descriptor_heap`.
    let Some(slot) = (unsafe { heap_slot(h_heap) }) else {
        note_refusal(&DESCRIPTOR_REFUSALS.heap_bad_arg);
        return out;
    };
    // SAFETY: as above. `load` borrows the slot's reference without taking it.
    let Some(heap) = (unsafe { slot.load() }) else {
        note_refusal(&DESCRIPTOR_REFUSALS.heap_bad_arg);
        return out;
    };
    // SAFETY: `heap` is the engine's live `ID3D12DescriptorHeap`, owned by the
    // slot for as long as the handle is valid. The call writes one `SIZE_T`
    // through the hidden out-pointer the crate's vtable declares and issues no
    // reference-count change.
    out.ptr = unsafe { heap.GetCPUDescriptorHandleForHeapStart() }.ptr as ddi12::SIZE_T;

    if out.ptr == 0 {
        DESCRIPTOR_REFUSALS.heap_cpu_handle_zero.bump();
        let n = DESCRIPTOR_REFUSALS.heap_cpu_handle_zero.get();
        if n <= LOG_BUDGET {
            log_error!(
                "GetCPUDescriptorHandleForHeapStart: engine returned 0 -- vkd3d always assigns a \
                 real cpu_va, so suspect the struct-return ABI (x{n})"
            );
        }
    } else {
        // ⭐ The round-trip evidence `DDI_REFERENCE.md` §9.6 asks for: a known
        // non-zero handle, printed once per heap for the first few heaps, so
        // that a truncated high half is visible in the log rather than only in a
        // crash. Bounded by the counter it shares with the GPU sibling.
        let n = DESCRIPTOR_REFUSALS.heap_start_handles.get();
        DESCRIPTOR_REFUSALS.heap_start_handles.bump();
        if n < LOG_BUDGET {
            log_error!("GetCPUDescriptorHandleForHeapStart -> {:#018x} (x{n})", out.ptr);
        }
    }
    out
}

/// `pfnGetGPUDescriptorHandleForHeapStart` — vkd3d's own `gpu_va`, verbatim.
///
/// ⚠ **A KNOWN DISAGREEMENT, forwarded rather than papered over.**
/// `ResourceBinding.md:4428-4429` says *"All descriptor heaps are GPU visible so
/// there must always be a non-NULL GPU address to return"*, but vkd3d assigns
/// `gpu_va` **only** for shader-visible heaps —
/// `libs/vkd3d/resource.c:10159` sits inside
/// `if (desc->Flags & D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE)` — so an
/// RTV/DSV/staging heap answers 0 here.
///
/// This driver forwards the engine's value and **counts** the zeros rather than
/// inventing a non-zero one, because a synthesised GPU handle would come back to
/// `pfnSetGraphicsRootDescriptorTable` as an address vkd3d cannot decode. The
/// counter is the finding; the alternative is a decision for whoever reads it.
/// UNVERIFIED: whether the D3D12 runtime rejects a zero here at all.
///
/// # Safety
/// `h_heap` must be a live handle from [`create_descriptor_heap`].
unsafe extern "C" fn get_gpu_descriptor_handle_for_heap_start(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_heap: ddi12::D3D12DDI_HDESCRIPTORHEAP,
) -> ddi12::D3D12DDI_GPU_DESCRIPTOR_HANDLE {
    let mut out = ddi12::D3D12DDI_GPU_DESCRIPTOR_HANDLE { ptr: 0 };
    // SAFETY: the caller guarantees a live handle from `create_descriptor_heap`.
    let Some(slot) = (unsafe { heap_slot(h_heap) }) else {
        note_refusal(&DESCRIPTOR_REFUSALS.heap_bad_arg);
        return out;
    };
    // SAFETY: as above.
    let Some(heap) = (unsafe { slot.load() }) else {
        note_refusal(&DESCRIPTOR_REFUSALS.heap_bad_arg);
        return out;
    };
    // SAFETY: as `get_cpu_descriptor_handle_for_heap_start`; one `UINT64`
    // written through the hidden out-pointer, no reference-count change.
    out.ptr = unsafe { heap.GetGPUDescriptorHandleForHeapStart() }.ptr;

    if out.ptr == 0 {
        note_refusal(&DESCRIPTOR_REFUSALS.heap_gpu_handle_zero);
    } else {
        let n = DESCRIPTOR_REFUSALS.heap_start_handles.get();
        DESCRIPTOR_REFUSALS.heap_start_handles.bump();
        if n < LOG_BUDGET {
            log_error!("GetGPUDescriptorHandleForHeapStart -> {:#018x} (x{n})", out.ptr);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The resource seam — L4's, not L5's
// ---------------------------------------------------------------------------

/// The engine `ID3D12Resource` behind a DDI resource handle.
///
/// ⛔ **STUB: the payload of `D3D12DDI_HRESOURCE` is L4's, not L5's.**
/// `PARALLEL.md` §4 gives `forward12/resource12.rs` — groups (g) heaps/resources
/// /residency and (h) resource introspection — to L4, and the `com_handles!` /
/// `boxed_handles!` invocation that says what a resource handle's private word
/// holds is part of that. Two lanes declaring it is a coherence error at merge;
/// this lane *guessing* it is `ARCHITECTURE.md` §12 rule 7 / R803 exactly:
/// picking a slot's payload at the call site produced a `ManuallyDrop` whose
/// vtable pointer was a struct field.
///
/// ⭐ **Wiring L4 in is this function body and nothing else.** Everything below
/// — the four view-descriptor translators, the shape decisions, the engine calls
/// — is the shipping code and is already checked against the bindgen signatures
/// and the `windows` crate's `ID3D12Device` methods. The replacement is
/// `resource12`'s own accessor returning a borrowed `ID3D12Resource`.
///
/// ⚠ Until then [`DESCRIPTOR_REFUSALS.view_resource_unavailable`] is expected
/// **non-zero on any workload that draws**, and every one of those views also
/// raises `pfnSetErrorCb`: a descriptor the driver did not write is not
/// something to render from quietly.
///
/// ⛔ **Never call this with a null handle** — [`view_resource`] is the entry
/// point, and it answers the null case itself. A null private word is the legal
/// D3D12 null-descriptor form, not a failure, and this function cannot tell the
/// two apart: `None` is its only possible answer for a null word whatever L4
/// eventually writes here.
/// ⭐ **WIRED AT INTEGRATION**, and it was exactly this function body.
///
/// L4 shipped `resource12::engine_resource` in the same batch, so the stub never
/// reached a build. Its return type is a **shared reference**, not the
/// `ManuallyDrop<ID3D12Resource>` this lane sketched, and L4's is the stronger
/// choice: the state box keeps the owning reference, so borrowing it as `&`
/// makes releasing it *unwritable*, where a `ManuallyDrop` merely makes it
/// unlikely. The integrator propagated that type through [`ViewResource`] rather
/// than converting back at the seam — converting would have thrown away the one
/// guarantee L4 went out of its way to obtain.
///
/// # Safety
/// As `resource12::engine_resource`: `h_resource` must be a live, non-null
/// resource handle this driver created, and the returned reference must not
/// outlive the DDI call that obtained it. Both hold at every call site, because
/// the only caller is [`view_resource`], which is called from a view-creation
/// DDI with the runtime's own handle and drops the borrow before returning.
unsafe fn engine_resource<'a>(
    h_resource: ddi12::D3D12DDI_HRESOURCE,
) -> Option<&'a ID3D12Resource> {
    // SAFETY: forwarded unchanged; the caller's guarantee above is this
    // function's, and `view_resource` has already rejected a null private word.
    unsafe { crate::forward12::resource12::engine_resource(h_resource) }
}

/// What a view's `hDrvResource` names.
///
/// ⛔ **The null case is not the failure case, and conflating them removes the
/// device.** `CreateShaderResourceView(nullptr, &desc, handle)` is required
/// D3D12 behaviour — an unbound slot in a descriptor table — so the runtime
/// legitimately lowers a view with a null `hDrvResource`, and vkd3d implements
/// it: `d3d12_desc_create_srv` writes the null-descriptor template when
/// `resource == NULL` (`libs/vkd3d/resource.c:7204-7208`),
/// `d3d12_rtv_desc_create_rtv` zeroes the RTV (`:8788-8792`), and the engine
/// hard-requires `VK_EXT_robustness2::nullDescriptor` for exactly this
/// (`SUBSTRATE.md:426`). Answering `pfnSetErrorCb` there would meet a legal call
/// with *"Removing device due to bad UMD error"* (`DDI_REFERENCE.md:2126-2130`).
///
/// So the three answers are kept apart at the one place the distinction is
/// still available — before [`engine_resource`] flattens a null handle and an
/// unresolvable one into the same `None`.
enum ViewResource<'a> {
    /// The runtime named **no** resource: D3D12's null-descriptor form. Legal,
    /// forwarded to the engine as a null `pResource`, counted on
    /// [`DESCRIPTOR_REFUSALS.view_null_resource`] as an instrument.
    Null,
    /// The runtime named a resource and this driver resolved it to the engine's
    /// object, **borrowed** from L4's state box (see [`engine_resource`]).
    Engine(&'a ID3D12Resource),
    /// The runtime named a resource and this driver could **not** resolve it.
    /// The refusal, and the only one of the three that reaches `pfnSetErrorCb`.
    Unresolved,
}

/// Classify a view's `hDrvResource`. See [`ViewResource`].
/// # Safety
/// `h_resource` is the runtime's own handle for this view-creation DDI, and the
/// returned borrow must not outlive that call. See [`engine_resource`].
unsafe fn view_resource<'a>(h_resource: ddi12::D3D12DDI_HRESOURCE) -> ViewResource<'a> {
    if h_resource.pDrvPrivate.is_null() {
        return ViewResource::Null;
    }
    // SAFETY: non-null per the check above, and the caller's guarantee is this
    // function's.
    match unsafe { engine_resource(h_resource) } {
        Some(res) => ViewResource::Engine(res),
        None => ViewResource::Unresolved,
    }
}

/// The resource desc a view translator sees when there is **no** resource.
///
/// ⚠ Zeroed, so [`resource_sample_count`] clamps to 1 and the shape decision
/// takes the non-multisampled branch. That is not a guess this driver can
/// improve on: the DDI's `Tex2D` arm carries no sample count, so with no
/// resource behind the handle there is nothing that distinguishes an MS null
/// descriptor from a plain one. It also does not matter to the substrate —
/// vkd3d's null paths read only whether the dimension is a **buffer**
/// (`d3d12_desc_create_srv_heap`, `libs/vkd3d/resource.c:7285-7301`) and
/// otherwise write a fixed template — and buffer-ness comes from the DDI's
/// `ResourceDimension`, which is present.
fn no_resource_desc() -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC::default()
}

/// Which 1D view shape a `(FirstArraySlice, ArraySize)` pair selects.
///
/// ⭐ Transliterated from the D3D11 driver's `umd/src/forward/state.rs:876-902`,
/// where the same fork was written out by hand in four translators until one of
/// them disagreed. Purpose-scoped enums rather than one `ViewShape` so every
/// translator's `match` stays exhaustive with no catch-all.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tex1DShape {
    Plain,
    Array,
}

/// Which 2D view shape a `(FirstArraySlice, ArraySize, sample count)` triple
/// selects.
///
/// ⛔ The evaluation order is load-bearing and matches all of the D3D11
/// originals (`umd/src/forward/state.rs:904-917`): **MSAA wins over array-ness**,
/// so a multisampled array is `MsArray` and not `Array`. Getting it wrong
/// silently mis-binds an MSAA render target rather than failing. That precedence
/// is untouched by [`needs_array_form`]: the MS-array API structs carry
/// `FirstArraySlice` too, so promoting on the offset never loses it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tex2DShape {
    Plain,
    Array,
    Ms,
    MsArray,
}

/// Whether a DDI view's `(ArraySize, FirstArraySlice)` pair needs an **array**
/// API dimension.
///
/// ⛔ **THE SECOND PLACE THE D3D11 PRECEDENT DOES NOT TRANSFER** — and unlike
/// `NO_MULTISAMPLED_FORM` above, which is a genuine difference between the two
/// APIs, this one is a **defect in the D3D11 site**.
/// `umd/src/forward/state.rs:896-916` decides array-ness from `ArraySize`
/// alone. That silently drops the slice index for the
/// `{ FirstArraySlice: n, ArraySize: 1 }` shape — one DSV per cascade in a
/// cascaded-shadow-map pass, one RTV per cube face, one SRV per light in a
/// shadow atlas — because the **non-array API structs have no
/// `FirstArraySlice` field at all** (`D3D12_TEX2D_SRV`, `D3D12_TEX2D_RTV`,
/// `D3D12_TEX2D_DSV`, `D3D12_TEX1D_*`, and their UAV siblings). There is
/// nowhere for the offset to go, so every such view addresses layer 0: cascade
/// 1..N render into slice 0 and the lookup samples an unwritten slice, with no
/// counter, no log line and an `S_OK`.
///
/// ⚠ The DDI offers no other signal. It has **one** `Tex1D`/`Tex2D` arm where
/// the API has separate plain and array dimensions — e.g.
/// `D3D12DDIARG_TEX2D_DEPTH_STENCIL_VIEW` is
/// `{ MipSlice, FirstArraySlice, ArraySize }` (`d3d12umddi.rs:50159-50163`) —
/// so these numbers are all the driver gets, and the offset must be part of the
/// predicate rather than an input to the branch that cannot hold it.
///
/// ⚠ `umd/src/forward/state.rs` has the same predicate and the same defect; it
/// is a D3D11 fix that belongs in that file's own change, not here.
const fn needs_array_form(array_size: u32, first_array_slice: u32) -> bool {
    array_size > 1 || first_array_slice > 0
}

const fn tex1d_shape(array_size: u32, first_array_slice: u32) -> Tex1DShape {
    if needs_array_form(array_size, first_array_slice) {
        Tex1DShape::Array
    } else {
        Tex1DShape::Plain
    }
}

const fn tex2d_shape(array_size: u32, first_array_slice: u32, sample_count: u32) -> Tex2DShape {
    match (
        sample_count > 1,
        needs_array_form(array_size, first_array_slice),
    ) {
        (true, true) => Tex2DShape::MsArray,
        (true, false) => Tex2DShape::Ms,
        (false, true) => Tex2DShape::Array,
        (false, false) => Tex2DShape::Plain,
    }
}

/// The resource's sample count, clamped to 1.
///
/// ⛔ D3D11's `resource_sample_count` had to `cast::<ID3D11Texture2D>()` first;
/// D3D12 has one `ID3D12Resource` and one `GetDesc`, so this is the whole of it.
/// ⚠ `GetDesc` returns a 40-byte struct and therefore uses the hidden
/// out-pointer on **both** sides (`windows-0.58.0/.../Direct3D12/mod.rs:3212`
/// declares `*mut D3D12_RESOURCE_DESC`; vkd3d's `d3d12_resource_GetDesc` takes
/// the same) — it is not in the 8-byte struct-return family that makes the two
/// heap-start getters delicate.
fn resource_sample_count(desc: &D3D12_RESOURCE_DESC) -> u32 {
    desc.SampleDesc.Count.max(1)
}

// ⛔ THE ONE PLACE THE D3D11 PRECEDENT DOES NOT TRANSFER, WRITTEN DOWN BECAUSE
// THE OBVIOUS MOVE IS TO COPY IT.
//
// `umd/src/forward/state.rs:919-926` defines `NO_MULTISAMPLED_FORM = 1`, the
// sample count `uav_desc` passes to `tex2d_shape` because *"D3D11 has no
// multisampled UAV -- there is no `D3D11_UAV_DIMENSION_TEXTURE2DMS`"*.
//
// **D3D12 does have one.** `D3D12_UAV_DIMENSION_TEXTURE2DMS` (6) and
// `_TEXTURE2DMSARRAY` (7) both exist, so `uav_desc` below consults the real
// sample count exactly like the other three translators, and there is no
// constant here to import. Copying the D3D11 shape verbatim would have silently
// bound every multisampled UAV as a non-multisampled one.
//
// ---------------------------------------------------------------------------
// Shader resource views
// ---------------------------------------------------------------------------

/// Translate `D3D12DDIARG_CREATE_SHADER_RESOURCE_VIEW_0002` to
/// `D3D12_SHADER_RESOURCE_VIEW_DESC`.
///
/// ⚠ The DDI's union arm is selected by `ResourceDimension`, and reading the
/// wrong arm is silent. The `match` below is the only place that choice is made.
///
/// ⚠ The DDI has **one** `Tex1D`/`Tex2D`/`TexCube` arm each where the API has
/// separate plain/array/MS dimensions, so the API dimension is a function of the
/// DDI arm **plus the resource** — which is why this takes the resource desc and
/// why the resource must be resolved first.
///
/// # Safety
/// `a`'s union arm must be the one `a.ResourceDimension` names, which is the
/// runtime's guarantee.
unsafe fn srv_desc(
    a: &ddi12::D3D12DDIARG_CREATE_SHADER_RESOURCE_VIEW_0002,
    res: &D3D12_RESOURCE_DESC,
) -> Option<D3D12_SHADER_RESOURCE_VIEW_DESC> {
    let format = DXGI_FORMAT(a.Format);
    let samples = resource_sample_count(res);
    let (view_dimension, anonymous) = match a.ResourceDimension {
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_BUFFER => {
            // SAFETY: `ResourceDimension` names the `Buffer` arm.
            let b = unsafe { a.__bindgen_anon_1.Buffer };
            (
                D3D12_SRV_DIMENSION_BUFFER,
                D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_SRV {
                        FirstElement: b.FirstElement,
                        NumElements: b.NumElements,
                        StructureByteStride: b.StructureByteStride,
                        Flags: api_buffer_srv_flags(b.Flags),
                    },
                },
            )
        }
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_TEXTURE1D => {
            // SAFETY: `ResourceDimension` names the `Tex1D` arm.
            let t = unsafe { a.__bindgen_anon_1.Tex1D };
            match tex1d_shape(t.ArraySize, t.FirstArraySlice) {
                Tex1DShape::Array => (
                    D3D12_SRV_DIMENSION_TEXTURE1DARRAY,
                    D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture1DArray: D3D12_TEX1D_ARRAY_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                            ResourceMinLODClamp: t.ResourceMinLODClamp,
                        },
                    },
                ),
                Tex1DShape::Plain => (
                    D3D12_SRV_DIMENSION_TEXTURE1D,
                    D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture1D: D3D12_TEX1D_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                            ResourceMinLODClamp: t.ResourceMinLODClamp,
                        },
                    },
                ),
            }
        }
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_TEXTURE2D => {
            // SAFETY: `ResourceDimension` names the `Tex2D` arm.
            let t = unsafe { a.__bindgen_anon_1.Tex2D };
            match tex2d_shape(t.ArraySize, t.FirstArraySlice, samples) {
                Tex2DShape::MsArray => (
                    D3D12_SRV_DIMENSION_TEXTURE2DMSARRAY,
                    D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2DMSArray: D3D12_TEX2DMS_ARRAY_SRV {
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                ),
                Tex2DShape::Ms => (
                    D3D12_SRV_DIMENSION_TEXTURE2DMS,
                    D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        // ⚠ The API struct's one field is literally named
                        // `UnusedField_NothingToDefine`; a multisampled 2D SRV
                        // carries no parameters at all.
                        Texture2DMS: D3D12_TEX2DMS_SRV {
                            UnusedField_NothingToDefine: 0,
                        },
                    },
                ),
                Tex2DShape::Array => (
                    D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
                    D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2DArray: D3D12_TEX2D_ARRAY_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                            PlaneSlice: t.PlaneSlice,
                            ResourceMinLODClamp: t.ResourceMinLODClamp,
                        },
                    },
                ),
                Tex2DShape::Plain => (
                    D3D12_SRV_DIMENSION_TEXTURE2D,
                    D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2D: D3D12_TEX2D_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                            PlaneSlice: t.PlaneSlice,
                            ResourceMinLODClamp: t.ResourceMinLODClamp,
                        },
                    },
                ),
            }
        }
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_TEXTURE3D => {
            // SAFETY: `ResourceDimension` names the `Tex3D` arm.
            let t = unsafe { a.__bindgen_anon_1.Tex3D };
            (
                D3D12_SRV_DIMENSION_TEXTURE3D,
                D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture3D: D3D12_TEX3D_SRV {
                        MostDetailedMip: t.MostDetailedMip,
                        MipLevels: t.MipLevels,
                        ResourceMinLODClamp: t.ResourceMinLODClamp,
                    },
                },
            )
        }
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_TEXTURECUBE => {
            // SAFETY: `ResourceDimension` names the `TexCube` arm.
            let t = unsafe { a.__bindgen_anon_1.TexCube };
            // ⚠ `NumCubes` and not an array size: the cube-array dimension
            // counts CUBES, and `First2DArrayFace` is in faces. The DDI and the
            // API agree on both, which is why this is a rename and not a
            // multiply.
            //
            // ⛔ The fork is `needs_array_form`'s question in cube units, and
            // for the same reason: `D3D12_TEXCUBE_SRV` is
            // `{ MostDetailedMip, MipLevels, ResourceMinLODClamp }` with **no**
            // `First2DArrayFace`, so a one-cube view of a cube array — the
            // per-light SRV of a point-light shadow atlas,
            // `{ First2DArrayFace: 6, NumCubes: 1 }` — has nowhere to put its
            // offset and would address cube 0 for every light. `NumCubes`
            // alone is therefore not the predicate.
            if t.NumCubes > 1 || t.First2DArrayFace > 0 {
                (
                    D3D12_SRV_DIMENSION_TEXTURECUBEARRAY,
                    D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        TextureCubeArray: D3D12_TEXCUBE_ARRAY_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                            First2DArrayFace: t.First2DArrayFace,
                            NumCubes: t.NumCubes,
                            ResourceMinLODClamp: t.ResourceMinLODClamp,
                        },
                    },
                )
            } else {
                (
                    D3D12_SRV_DIMENSION_TEXTURECUBE,
                    D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        TextureCube: D3D12_TEXCUBE_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                            ResourceMinLODClamp: t.ResourceMinLODClamp,
                        },
                    },
                )
            }
        }
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_RAYTRACING_ACCELERATION_STRUCTURE_0042 => {
            // SAFETY: `ResourceDimension` names the acceleration-structure arm.
            let t = unsafe { a.__bindgen_anon_1.RaytracingAccelerationStructure };
            (
                D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE,
                D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    RaytracingAccelerationStructure: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_SRV {
                        Location: t.Location,
                    },
                },
            )
        }
        _ => return None,
    };
    Some(D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: view_dimension,
        // ⚠ Passed through as a `UINT`: both sides use the same
        // `D3D12_ENCODE_SHADER_4_COMPONENT_MAPPING` packing, and the DDI types
        // the field as a bare `UINT` rather than the
        // `D3D12DDI_SHADER_COMPONENT_MAPPING` enum precisely because it is four
        // 3-bit fields plus a validity bit, not one enumerator.
        Shader4ComponentMapping: a.Shader4ComponentMapping,
        Anonymous: anonymous,
    })
}

/// DDI buffer-SRV flags -> API buffer-SRV flags. One flag on each side, `RAW`.
fn api_buffer_srv_flags(f: ddi12::D3D12DDI_BUFFER_SRV_FLAGS) -> D3D12_BUFFER_SRV_FLAGS {
    if f & ddi12::D3D12DDI_BUFFER_SRV_FLAGS_D3D12DDI_BUFFER_SRV_FLAG_RAW != 0 {
        D3D12_BUFFER_SRV_FLAG_RAW
    } else {
        D3D12_BUFFER_SRV_FLAG_NONE
    }
}

/// DDI buffer-UAV flags -> API buffer-UAV flags. One flag on each side, `RAW`.
fn api_buffer_uav_flags(f: ddi12::D3D12DDI_BUFFER_UAV_FLAGS) -> D3D12_BUFFER_UAV_FLAGS {
    if f & ddi12::D3D12DDI_BUFFER_UAV_FLAGS_D3D12DDI_BUFFER_UAV_FLAG_RAW != 0 {
        D3D12_BUFFER_UAV_FLAG_RAW
    } else {
        D3D12_BUFFER_UAV_FLAG_NONE
    }
}

/// `pfnCreateShaderResourceView`.
///
/// # Safety
/// `arg` must point at a live `D3D12DDIARG_CREATE_SHADER_RESOURCE_VIEW_0002`
/// whose union arm is the one `ResourceDimension` names, and `dest` must be a
/// CPU descriptor handle this driver minted.
unsafe extern "C" fn create_shader_resource_view(
    h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_SHADER_RESOURCE_VIEW_0002,
    dest: ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
) {
    // SAFETY: device-scope DDI; see `create_descriptor_heap`.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        return;
    };
    if arg.is_null() {
        note_refusal(&DESCRIPTOR_REFUSALS.view_bad_arg);
        report_error(dev, E_INVALIDARG);
        return;
    }
    // SAFETY: non-null per the check above; `_In_ CONST` per the DDI.
    let a = unsafe { &*arg };
    let Some(engine) = dev.engine.d3d12_device() else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        report_error(dev, E_FAIL);
        return;
    };

    // ⭐ A raytracing acceleration-structure SRV is the one arm that takes NO
    // resource: D3D12 requires `pResource == NULL` and locates the structure by
    // GPU virtual address instead. It is unreachable while `caps12` reports
    // `RaytracingTier = NOT_SUPPORTED` (`caps12.rs:585`); it is written because
    // the shape is a correctness requirement the day that tier moves, not
    // because it can fire today.
    let is_acceleration_structure = a.ResourceDimension
        == ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_RAYTRACING_ACCELERATION_STRUCTURE_0042;

    let resource = if is_acceleration_structure {
        None
    } else {
        match unsafe { view_resource(a.hDrvResource) } {
            ViewResource::Engine(res) => Some(res),
            // ⭐ The legal null-descriptor form, not a failure — see
            // `ViewResource`. Forwarded with a null `pResource` and counted as
            // an instrument; no `pfnSetErrorCb`.
            ViewResource::Null => {
                DESCRIPTOR_REFUSALS.view_null_resource.bump();
                None
            }
            ViewResource::Unresolved => {
                DESCRIPTOR_REFUSALS.view_resource_unavailable.bump();
                log_view_refusal("CreateShaderResourceView", a.Format, a.ResourceDimension);
                report_error(dev, E_FAIL);
                return;
            }
        }
    };

    // ⚠ Neither an acceleration-structure view nor a null descriptor has a
    // resource to describe. `no_resource_desc` is the honest stand-in and says
    // what it costs: `srv_desc` reads only `SampleDesc.Count`, and the
    // acceleration-structure arm does not read it at all.
    let res_desc = match &resource {
        // SAFETY: `res` is the engine's live `ID3D12Resource`, borrowed from
        // L4's private block for the duration of this call. `GetDesc` writes one
        // `D3D12_RESOURCE_DESC` through the hidden out-pointer the crate's
        // vtable declares and issues no reference-count change.
        Some(res) => unsafe { res.GetDesc() },
        None => no_resource_desc(),
    };

    // SAFETY: the runtime guarantees the union arm matches `ResourceDimension`,
    // which is this function's own precondition.
    let Some(desc) = (unsafe { srv_desc(a, &res_desc) }) else {
        DESCRIPTOR_REFUSALS.view_dimension_unknown.bump();
        log_view_refusal("CreateShaderResourceView", a.Format, a.ResourceDimension);
        report_error(dev, E_INVALIDARG);
        return;
    };

    trace_line!(
        "CreateShaderResourceView: fmt={} ddiDim={} -> apiDim={} dest={:#018x}",
        a.Format,
        a.ResourceDimension,
        desc.ViewDimension.0,
        dest.ptr,
    );
    // SAFETY: `engine` is the bridge's borrowed `ID3D12Device`; `desc` is a live
    // local; `resource` is either the borrowed engine resource or `None` (the
    // acceleration-structure arm, where D3D12 requires NULL); `dest` is a CPU
    // descriptor handle this driver minted from the engine's own heap, so it
    // addresses engine descriptor storage.
    unsafe {
        engine.CreateShaderResourceView(
            resource,
            Some(core::ptr::from_ref(&desc)),
            api_cpu_handle(dest),
        );
    }
}

// ---------------------------------------------------------------------------
// Unordered access views
// ---------------------------------------------------------------------------

/// Translate `D3D12DDIARG_CREATE_UNORDERED_ACCESS_VIEW_0002`.
///
/// # Safety
/// As [`srv_desc`].
unsafe fn uav_desc(
    a: &ddi12::D3D12DDIARG_CREATE_UNORDERED_ACCESS_VIEW_0002,
    res: &D3D12_RESOURCE_DESC,
) -> Option<D3D12_UNORDERED_ACCESS_VIEW_DESC> {
    let samples = resource_sample_count(res);
    let (view_dimension, anonymous) = match a.ResourceDimension {
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_BUFFER => {
            // SAFETY: `ResourceDimension` names the `Buffer` arm.
            let b = unsafe { a.__bindgen_anon_1.Buffer };
            (
                D3D12_UAV_DIMENSION_BUFFER,
                D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_UAV {
                        FirstElement: b.FirstElement,
                        NumElements: b.NumElements,
                        StructureByteStride: b.StructureByteStride,
                        CounterOffsetInBytes: b.CounterOffsetInBytes,
                        Flags: api_buffer_uav_flags(b.Flags),
                    },
                },
            )
        }
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_TEXTURE1D => {
            // SAFETY: `ResourceDimension` names the `Tex1D` arm.
            let t = unsafe { a.__bindgen_anon_1.Tex1D };
            match tex1d_shape(t.ArraySize, t.FirstArraySlice) {
                Tex1DShape::Array => (
                    D3D12_UAV_DIMENSION_TEXTURE1DARRAY,
                    D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Texture1DArray: D3D12_TEX1D_ARRAY_UAV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                ),
                Tex1DShape::Plain => (
                    D3D12_UAV_DIMENSION_TEXTURE1D,
                    D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Texture1D: D3D12_TEX1D_UAV {
                            MipSlice: t.MipSlice,
                        },
                    },
                ),
            }
        }
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_TEXTURE2D => {
            // SAFETY: `ResourceDimension` names the `Tex2D` arm.
            let t = unsafe { a.__bindgen_anon_1.Tex2D };
            match tex2d_shape(t.ArraySize, t.FirstArraySlice, samples) {
                Tex2DShape::MsArray => (
                    D3D12_UAV_DIMENSION_TEXTURE2DMSARRAY,
                    D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Texture2DMSArray: D3D12_TEX2DMS_ARRAY_UAV {
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                ),
                Tex2DShape::Ms => (
                    D3D12_UAV_DIMENSION_TEXTURE2DMS,
                    D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Texture2DMS: D3D12_TEX2DMS_UAV {
                            UnusedField_NothingToDefine: 0,
                        },
                    },
                ),
                Tex2DShape::Array => (
                    D3D12_UAV_DIMENSION_TEXTURE2DARRAY,
                    D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Texture2DArray: D3D12_TEX2D_ARRAY_UAV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                            PlaneSlice: t.PlaneSlice,
                        },
                    },
                ),
                Tex2DShape::Plain => (
                    D3D12_UAV_DIMENSION_TEXTURE2D,
                    D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Texture2D: D3D12_TEX2D_UAV {
                            MipSlice: t.MipSlice,
                            PlaneSlice: t.PlaneSlice,
                        },
                    },
                ),
            }
        }
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_TEXTURE3D => {
            // SAFETY: `ResourceDimension` names the `Tex3D` arm.
            let t = unsafe { a.__bindgen_anon_1.Tex3D };
            (
                D3D12_UAV_DIMENSION_TEXTURE3D,
                D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Texture3D: D3D12_TEX3D_UAV {
                        MipSlice: t.MipSlice,
                        FirstWSlice: t.FirstW,
                        WSize: t.WSize,
                    },
                },
            )
        }
        _ => return None,
    };
    Some(D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT(a.Format),
        ViewDimension: view_dimension,
        Anonymous: anonymous,
    })
}

/// `pfnCreateUnorderedAccessView`.
///
/// ⚠ The DDI carries the UAV **counter** resource inside the `Buffer` arm
/// (`D3D12DDIARG_BUFFER_UNORDERED_ACCESS_VIEW::hDrvCounterResource`) while the
/// API takes it as a separate `pCounterResource` argument. A null counter handle
/// is legal and normal.
///
/// ⛔ A null UAV counter descriptor is the live D3D12 robustness defect recorded
/// in the 72nd memory: it faults the host GPU context and the guest hangs. This
/// slot therefore keeps the counter resource and the `COUNTER` flag together —
/// it never fabricates one and never drops one.
///
/// # Safety
/// As [`create_shader_resource_view`].
unsafe extern "C" fn create_unordered_access_view(
    h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_UNORDERED_ACCESS_VIEW_0002,
    dest: ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
) {
    // SAFETY: device-scope DDI; see `create_descriptor_heap`.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        return;
    };
    if arg.is_null() {
        note_refusal(&DESCRIPTOR_REFUSALS.view_bad_arg);
        report_error(dev, E_INVALIDARG);
        return;
    }
    // SAFETY: non-null per the check above; `_In_ CONST` per the DDI.
    let a = unsafe { &*arg };
    let Some(engine) = dev.engine.d3d12_device() else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        report_error(dev, E_FAIL);
        return;
    };
    let resource = match unsafe { view_resource(a.hDrvResource) } {
        ViewResource::Engine(res) => Some(res),
        // ⭐ The legal null-descriptor form — see `ViewResource`.
        ViewResource::Null => {
            DESCRIPTOR_REFUSALS.view_null_resource.bump();
            None
        }
        ViewResource::Unresolved => {
            DESCRIPTOR_REFUSALS.view_resource_unavailable.bump();
            log_view_refusal("CreateUnorderedAccessView", a.Format, a.ResourceDimension);
            report_error(dev, E_FAIL);
            return;
        }
    };
    let res_desc = match &resource {
        // SAFETY: borrowed engine resource; `GetDesc` writes through the hidden
        // out-pointer and changes no reference count.
        Some(res) => unsafe { res.GetDesc() },
        None => no_resource_desc(),
    };

    // SAFETY: the runtime guarantees the union arm matches `ResourceDimension`.
    let Some(desc) = (unsafe { uav_desc(a, &res_desc) }) else {
        DESCRIPTOR_REFUSALS.view_dimension_unknown.bump();
        log_view_refusal("CreateUnorderedAccessView", a.Format, a.ResourceDimension);
        report_error(dev, E_INVALIDARG);
        return;
    };

    // ⚠ Only the buffer arm has a counter; every other arm's union member would
    // be a read of unrelated bytes.
    let counter = if a.ResourceDimension == ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_BUFFER {
        // SAFETY: `ResourceDimension` names the `Buffer` arm.
        let counter_handle = unsafe { a.__bindgen_anon_1.Buffer.hDrvCounterResource };
        match unsafe { view_resource(counter_handle) } {
            ViewResource::Engine(res) => Some(res),
            // ⚠ No counter named at all: legal and normal, and NOT counted --
            // `view_null_resource` is about the view's own resource, and folding
            // "this UAV has no counter" into it would make that instrument
            // unreadable. This split is the one the lane already had; it is
            // spelled through `view_resource` so that classification lives in
            // exactly one place.
            ViewResource::Null => None,
            // ⛔ **Refuse, do not drop.** The runtime NAMED a counter resource and
            // this driver could not resolve it. Writing the descriptor anyway
            // produces exactly the null UAV counter the 72nd memory records
            // faulting the host GPU context and hanging the guest — and this
            // slot's own doc twelve lines above promises it *"never drops one"*.
            //
            // ⚠ It was written as bump-and-continue, and the counter's own
            // declaration recorded the vkd3d consequence — which made it worse,
            // not better: the lane wrote down that a dropped counter hangs the
            // guest and then chose the silent arm for it, while choosing
            // `pfnSetErrorCb` for the strictly *less* dangerous unresolvable-view
            // case four lines above. The two arms now agree, and they agree on
            // the side where the failure is visible: a removed device is a
            // diagnosable event, a hung guest is not.
            ViewResource::Unresolved => {
                note_refusal(&DESCRIPTOR_REFUSALS.uav_counter_unavailable);
                log_view_refusal("CreateUnorderedAccessView counter", a.Format, a.ResourceDimension);
                report_error(dev, E_FAIL);
                return;
            }
        }
    } else {
        None
    };

    trace_line!(
        "CreateUnorderedAccessView: fmt={} ddiDim={} -> apiDim={} counter={} dest={:#018x}",
        a.Format,
        a.ResourceDimension,
        desc.ViewDimension.0,
        u32::from(counter.is_some()),
        dest.ptr,
    );
    // SAFETY: as `create_shader_resource_view`; both resources are borrowed and
    // neither wrapper is released here.
    unsafe {
        engine.CreateUnorderedAccessView(
            resource,
            counter,
            Some(core::ptr::from_ref(&desc)),
            api_cpu_handle(dest),
        );
    }
}

// ---------------------------------------------------------------------------
// Render target views
// ---------------------------------------------------------------------------

/// Translate `D3D12DDIARG_CREATE_RENDER_TARGET_VIEW_0002`.
///
/// ⚠ The DDI's `TexCube` arm has no API counterpart — there is no
/// `D3D12_RTV_DIMENSION_TEXTURECUBE` — and it does not need one: the DDI struct
/// is `{ MipSlice, FirstArraySlice, ArraySize }`, already in 2D-array
/// coordinates, so it maps onto the 2D array (or MS array) dimension directly.
///
/// # Safety
/// As [`srv_desc`].
unsafe fn rtv_desc(
    a: &ddi12::D3D12DDIARG_CREATE_RENDER_TARGET_VIEW_0002,
    res: &D3D12_RESOURCE_DESC,
) -> Option<D3D12_RENDER_TARGET_VIEW_DESC> {
    let samples = resource_sample_count(res);
    let (view_dimension, anonymous) = match a.ResourceDimension {
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_BUFFER => {
            // SAFETY: `ResourceDimension` names the `Buffer` arm.
            let b = unsafe { a.__bindgen_anon_1.Buffer };
            (
                D3D12_RTV_DIMENSION_BUFFER,
                D3D12_RENDER_TARGET_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_RTV {
                        FirstElement: b.FirstElement,
                        NumElements: b.NumElements,
                    },
                },
            )
        }
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_TEXTURE1D => {
            // SAFETY: `ResourceDimension` names the `Tex1D` arm.
            let t = unsafe { a.__bindgen_anon_1.Tex1D };
            match tex1d_shape(t.ArraySize, t.FirstArraySlice) {
                Tex1DShape::Array => (
                    D3D12_RTV_DIMENSION_TEXTURE1DARRAY,
                    D3D12_RENDER_TARGET_VIEW_DESC_0 {
                        Texture1DArray: D3D12_TEX1D_ARRAY_RTV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                ),
                Tex1DShape::Plain => (
                    D3D12_RTV_DIMENSION_TEXTURE1D,
                    D3D12_RENDER_TARGET_VIEW_DESC_0 {
                        Texture1D: D3D12_TEX1D_RTV {
                            MipSlice: t.MipSlice,
                        },
                    },
                ),
            }
        }
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_TEXTURE2D => {
            // SAFETY: `ResourceDimension` names the `Tex2D` arm.
            let t = unsafe { a.__bindgen_anon_1.Tex2D };
            rtv_tex2d(t.MipSlice, t.FirstArraySlice, t.ArraySize, t.PlaneSlice, samples)
        }
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_TEXTURE3D => {
            // SAFETY: `ResourceDimension` names the `Tex3D` arm.
            let t = unsafe { a.__bindgen_anon_1.Tex3D };
            (
                D3D12_RTV_DIMENSION_TEXTURE3D,
                D3D12_RENDER_TARGET_VIEW_DESC_0 {
                    Texture3D: D3D12_TEX3D_RTV {
                        MipSlice: t.MipSlice,
                        FirstWSlice: t.FirstW,
                        WSize: t.WSize,
                    },
                },
            )
        }
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_TEXTURECUBE => {
            // SAFETY: `ResourceDimension` names the `TexCube` arm.
            let t = unsafe { a.__bindgen_anon_1.TexCube };
            // ⚠ No `PlaneSlice` in the cube arm; a cube face is not a plane, so
            // 0 is the value and not a dropped field.
            rtv_tex2d(t.MipSlice, t.FirstArraySlice, t.ArraySize, 0, samples)
        }
        _ => return None,
    };
    Some(D3D12_RENDER_TARGET_VIEW_DESC {
        Format: DXGI_FORMAT(a.Format),
        ViewDimension: view_dimension,
        Anonymous: anonymous,
    })
}

/// The 2D RTV shape fork, shared by the `Tex2D` and `TexCube` DDI arms because
/// they carry the same four numbers.
fn rtv_tex2d(
    mip_slice: u32,
    first_array_slice: u32,
    array_size: u32,
    plane_slice: u32,
    samples: u32,
) -> (
    D3D12_RTV_DIMENSION,
    D3D12_RENDER_TARGET_VIEW_DESC_0,
) {
    match tex2d_shape(array_size, first_array_slice, samples) {
        Tex2DShape::MsArray => (
            D3D12_RTV_DIMENSION_TEXTURE2DMSARRAY,
            D3D12_RENDER_TARGET_VIEW_DESC_0 {
                Texture2DMSArray: D3D12_TEX2DMS_ARRAY_RTV {
                    FirstArraySlice: first_array_slice,
                    ArraySize: array_size,
                },
            },
        ),
        Tex2DShape::Ms => (
            D3D12_RTV_DIMENSION_TEXTURE2DMS,
            D3D12_RENDER_TARGET_VIEW_DESC_0 {
                Texture2DMS: D3D12_TEX2DMS_RTV {
                    UnusedField_NothingToDefine: 0,
                },
            },
        ),
        Tex2DShape::Array => (
            D3D12_RTV_DIMENSION_TEXTURE2DARRAY,
            D3D12_RENDER_TARGET_VIEW_DESC_0 {
                Texture2DArray: D3D12_TEX2D_ARRAY_RTV {
                    MipSlice: mip_slice,
                    FirstArraySlice: first_array_slice,
                    ArraySize: array_size,
                    PlaneSlice: plane_slice,
                },
            },
        ),
        Tex2DShape::Plain => (
            D3D12_RTV_DIMENSION_TEXTURE2D,
            D3D12_RENDER_TARGET_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_RTV {
                    MipSlice: mip_slice,
                    PlaneSlice: plane_slice,
                },
            },
        ),
    }
}

/// `pfnCreateRenderTargetView`.
///
/// # Safety
/// As [`create_shader_resource_view`].
unsafe extern "C" fn create_render_target_view(
    h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_RENDER_TARGET_VIEW_0002,
    dest: ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
) {
    // SAFETY: device-scope DDI; see `create_descriptor_heap`.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        return;
    };
    if arg.is_null() {
        note_refusal(&DESCRIPTOR_REFUSALS.view_bad_arg);
        report_error(dev, E_INVALIDARG);
        return;
    }
    // SAFETY: non-null per the check above; `_In_ CONST` per the DDI.
    let a = unsafe { &*arg };
    let Some(engine) = dev.engine.d3d12_device() else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        report_error(dev, E_FAIL);
        return;
    };
    let resource = match unsafe { view_resource(a.hDrvResource) } {
        ViewResource::Engine(res) => Some(res),
        // ⭐ The legal null-descriptor form — see `ViewResource`.
        ViewResource::Null => {
            DESCRIPTOR_REFUSALS.view_null_resource.bump();
            None
        }
        ViewResource::Unresolved => {
            DESCRIPTOR_REFUSALS.view_resource_unavailable.bump();
            log_view_refusal("CreateRenderTargetView", a.Format, a.ResourceDimension);
            report_error(dev, E_FAIL);
            return;
        }
    };
    let res_desc = match &resource {
        // SAFETY: borrowed engine resource; hidden out-pointer, no refcount
        // change.
        Some(res) => unsafe { res.GetDesc() },
        None => no_resource_desc(),
    };

    // SAFETY: the runtime guarantees the union arm matches `ResourceDimension`.
    let Some(desc) = (unsafe { rtv_desc(a, &res_desc) }) else {
        DESCRIPTOR_REFUSALS.view_dimension_unknown.bump();
        log_view_refusal("CreateRenderTargetView", a.Format, a.ResourceDimension);
        report_error(dev, E_INVALIDARG);
        return;
    };

    trace_line!(
        "CreateRenderTargetView: fmt={} ddiDim={} -> apiDim={} dest={:#018x}",
        a.Format,
        a.ResourceDimension,
        desc.ViewDimension.0,
        dest.ptr,
    );
    // SAFETY: as `create_shader_resource_view`.
    unsafe {
        engine.CreateRenderTargetView(
            resource,
            Some(core::ptr::from_ref(&desc)),
            api_cpu_handle(dest),
        );
    }
}

// ---------------------------------------------------------------------------
// Depth stencil views
// ---------------------------------------------------------------------------

/// DDI DSV flags -> API DSV flags.
///
/// ⚠ These two enums **do** agree numerically (READ_ONLY_DEPTH 1,
/// READ_ONLY_STENCIL 2 on both sides), and this is still a translation: it is
/// the same shape as the heap-flags pair one function apart, and the reader
/// should not have to know which of the two was the trap.
fn api_dsv_flags(f: ddi12::D3D12DDI_CREATE_DEPTH_STENCIL_VIEW_FLAGS) -> D3D12_DSV_FLAGS {
    let mut out = D3D12_DSV_FLAG_NONE;
    if f & ddi12::D3D12DDI_CREATE_DEPTH_STENCIL_VIEW_FLAGS_D3D12DDI_CREATE_DSV_FLAG_READ_ONLY_DEPTH
        != 0
    {
        out |= D3D12_DSV_FLAG_READ_ONLY_DEPTH;
    }
    if f & ddi12::D3D12DDI_CREATE_DEPTH_STENCIL_VIEW_FLAGS_D3D12DDI_CREATE_DSV_FLAG_READ_ONLY_STENCIL
        != 0
    {
        out |= D3D12_DSV_FLAG_READ_ONLY_STENCIL;
    }
    out
}

/// Translate `D3D12DDIARG_CREATE_DEPTH_STENCIL_VIEW`.
///
/// ⚠ The DDI union has only `Tex1D`, `Tex2D` and `TexCube` — there is no buffer
/// and no 3D depth-stencil view on either side.
///
/// # Safety
/// As [`srv_desc`].
unsafe fn dsv_desc(
    a: &ddi12::D3D12DDIARG_CREATE_DEPTH_STENCIL_VIEW,
    res: &D3D12_RESOURCE_DESC,
) -> Option<D3D12_DEPTH_STENCIL_VIEW_DESC> {
    let samples = resource_sample_count(res);
    let (view_dimension, anonymous) = match a.ResourceDimension {
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_TEXTURE1D => {
            // SAFETY: `ResourceDimension` names the `Tex1D` arm.
            let t = unsafe { a.__bindgen_anon_1.Tex1D };
            match tex1d_shape(t.ArraySize, t.FirstArraySlice) {
                Tex1DShape::Array => (
                    D3D12_DSV_DIMENSION_TEXTURE1DARRAY,
                    D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
                        Texture1DArray: D3D12_TEX1D_ARRAY_DSV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                ),
                Tex1DShape::Plain => (
                    D3D12_DSV_DIMENSION_TEXTURE1D,
                    D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
                        Texture1D: D3D12_TEX1D_DSV {
                            MipSlice: t.MipSlice,
                        },
                    },
                ),
            }
        }
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_TEXTURE2D => {
            // SAFETY: `ResourceDimension` names the `Tex2D` arm.
            let t = unsafe { a.__bindgen_anon_1.Tex2D };
            dsv_tex2d(t.MipSlice, t.FirstArraySlice, t.ArraySize, samples)
        }
        ddi12::D3D12DDI_RESOURCE_DIMENSION_D3D12DDI_RD_TEXTURECUBE => {
            // SAFETY: `ResourceDimension` names the `TexCube` arm.
            let t = unsafe { a.__bindgen_anon_1.TexCube };
            dsv_tex2d(t.MipSlice, t.FirstArraySlice, t.ArraySize, samples)
        }
        _ => return None,
    };
    Some(D3D12_DEPTH_STENCIL_VIEW_DESC {
        Format: DXGI_FORMAT(a.Format),
        ViewDimension: view_dimension,
        Flags: api_dsv_flags(a.Flags),
        Anonymous: anonymous,
    })
}

/// The 2D DSV shape fork, shared by the `Tex2D` and `TexCube` DDI arms.
fn dsv_tex2d(
    mip_slice: u32,
    first_array_slice: u32,
    array_size: u32,
    samples: u32,
) -> (
    D3D12_DSV_DIMENSION,
    D3D12_DEPTH_STENCIL_VIEW_DESC_0,
) {
    match tex2d_shape(array_size, first_array_slice, samples) {
        Tex2DShape::MsArray => (
            D3D12_DSV_DIMENSION_TEXTURE2DMSARRAY,
            D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
                Texture2DMSArray: D3D12_TEX2DMS_ARRAY_DSV {
                    FirstArraySlice: first_array_slice,
                    ArraySize: array_size,
                },
            },
        ),
        Tex2DShape::Ms => (
            D3D12_DSV_DIMENSION_TEXTURE2DMS,
            D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
                Texture2DMS: D3D12_TEX2DMS_DSV {
                    UnusedField_NothingToDefine: 0,
                },
            },
        ),
        Tex2DShape::Array => (
            D3D12_DSV_DIMENSION_TEXTURE2DARRAY,
            D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
                Texture2DArray: D3D12_TEX2D_ARRAY_DSV {
                    MipSlice: mip_slice,
                    FirstArraySlice: first_array_slice,
                    ArraySize: array_size,
                },
            },
        ),
        Tex2DShape::Plain => (
            D3D12_DSV_DIMENSION_TEXTURE2D,
            D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_DSV {
                    MipSlice: mip_slice,
                },
            },
        ),
    }
}

/// `pfnCreateDepthStencilView`.
///
/// # Safety
/// As [`create_shader_resource_view`].
unsafe extern "C" fn create_depth_stencil_view(
    h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_DEPTH_STENCIL_VIEW,
    dest: ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
) {
    // SAFETY: device-scope DDI; see `create_descriptor_heap`.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        return;
    };
    if arg.is_null() {
        note_refusal(&DESCRIPTOR_REFUSALS.view_bad_arg);
        report_error(dev, E_INVALIDARG);
        return;
    }
    // SAFETY: non-null per the check above; `_In_ CONST` per the DDI.
    let a = unsafe { &*arg };
    let Some(engine) = dev.engine.d3d12_device() else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        report_error(dev, E_FAIL);
        return;
    };
    let resource = match unsafe { view_resource(a.hDrvResource) } {
        ViewResource::Engine(res) => Some(res),
        // ⭐ The legal null-descriptor form — see `ViewResource`.
        ViewResource::Null => {
            DESCRIPTOR_REFUSALS.view_null_resource.bump();
            None
        }
        ViewResource::Unresolved => {
            DESCRIPTOR_REFUSALS.view_resource_unavailable.bump();
            log_view_refusal("CreateDepthStencilView", a.Format, a.ResourceDimension);
            report_error(dev, E_FAIL);
            return;
        }
    };
    let res_desc = match &resource {
        // SAFETY: borrowed engine resource; hidden out-pointer, no refcount
        // change.
        Some(res) => unsafe { res.GetDesc() },
        None => no_resource_desc(),
    };

    // SAFETY: the runtime guarantees the union arm matches `ResourceDimension`.
    let Some(desc) = (unsafe { dsv_desc(a, &res_desc) }) else {
        DESCRIPTOR_REFUSALS.view_dimension_unknown.bump();
        log_view_refusal("CreateDepthStencilView", a.Format, a.ResourceDimension);
        report_error(dev, E_INVALIDARG);
        return;
    };

    trace_line!(
        "CreateDepthStencilView: fmt={} ddiDim={} ddiFlags={:#x} -> apiDim={} dest={:#018x}",
        a.Format,
        a.ResourceDimension,
        a.Flags,
        desc.ViewDimension.0,
        dest.ptr,
    );
    // SAFETY: as `create_shader_resource_view`.
    unsafe {
        engine.CreateDepthStencilView(
            resource,
            Some(core::ptr::from_ref(&desc)),
            api_cpu_handle(dest),
        );
    }
}

// ---------------------------------------------------------------------------
// Constant buffer views
// ---------------------------------------------------------------------------

/// `pfnCreateConstantBufferView`.
///
/// ⭐ The one view DDI that names **no resource at all**: the DDI desc is
/// `{ BufferLocation: D3D12DDI_GPU_VIRTUAL_ADDRESS, SizeInBytes, Padding }`
/// (`d3d12umddi.rs:51830-51834`) and the API's is the same minus the padding, so
/// this slot is complete without L4.
///
/// ⚠ `Padding` is dropped deliberately: the API struct has no counterpart and
/// the header's name says it is alignment, not data.
///
/// # Safety
/// `arg` must point at a live `D3D12DDI_CONSTANT_BUFFER_VIEW_DESC`, and `dest`
/// at a CPU descriptor handle this driver minted.
unsafe extern "C" fn create_constant_buffer_view(
    h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDI_CONSTANT_BUFFER_VIEW_DESC,
    dest: ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
) {
    // SAFETY: device-scope DDI; see `create_descriptor_heap`.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        return;
    };
    let Some(engine) = dev.engine.d3d12_device() else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        report_error(dev, E_FAIL);
        return;
    };

    // ⚠ A null desc is LEGAL here and means "write a null descriptor": the
    // `windows` wrapper types the parameter `Option<*const …>` for exactly that
    // reason, and D3D12 defines a CBV with no desc as the null CBV. So this is
    // forwarded rather than refused, and it is not counted as a bad argument.
    let desc = if arg.is_null() {
        None
    } else {
        // SAFETY: non-null per the check above; `_In_ CONST` per the DDI.
        let a = unsafe { &*arg };
        trace_line!(
            "CreateConstantBufferView: loc={:#018x} size={} dest={:#018x}",
            a.BufferLocation,
            a.SizeInBytes,
            dest.ptr,
        );
        Some(D3D12_CONSTANT_BUFFER_VIEW_DESC {
            BufferLocation: a.BufferLocation,
            SizeInBytes: a.SizeInBytes,
        })
    };

    // SAFETY: `engine` is the bridge's borrowed `ID3D12Device`; `desc`, when
    // present, is a live local whose address does not escape the call; `dest`
    // addresses engine descriptor storage this driver minted.
    unsafe {
        engine.CreateConstantBufferView(
            desc.as_ref().map(core::ptr::from_ref),
            api_cpu_handle(dest),
        );
    }
}

// ---------------------------------------------------------------------------
// Samplers
// ---------------------------------------------------------------------------

// ⭐ THE THREE SAMPLER SUB-ENUMS ARE VALUE-IDENTICAL, AND THAT IS PINNED.
//
// `D3D12DDI_FILTER`, `D3D12DDI_TEXTURE_ADDRESS_MODE` and
// `D3D12DDI_COMPARISON_FUNC` are the same numbering as their API counterparts.
// The filter enum in particular is a bitfield encoding (min/mag/mip filter bits
// plus a 2-bit reduction type at shift 7 -- `d3d12umddi.rs:154-155` defines
// `D3D12DDI_FILTER_REDUCTION_TYPE_MASK = 3` and `_SHIFT = 7`, the same values
// `D3D12_FILTER_REDUCTION_TYPE_*` uses), so the assertions below bracket every
// reduction type and both filter extremes rather than listing forty
// enumerators one by one.
//
// ⚠ What this proves and does not: it proves the two encodings share the same
// origin and the same four reduction-type bases. It does not enumerate every
// intermediate value. That is the same standard `caps12.rs:1830` applies to the
// multisample-quality flags, and the alternative -- a forty-line pair table --
// buys nothing a bitfield encoding does not already give.
const _: () = assert!(
    ddi12::D3D12DDI_FILTER_D3D12DDI_FILTER_MIN_MAG_MIP_POINT
        == windows::Win32::Graphics::Direct3D12::D3D12_FILTER_MIN_MAG_MIP_POINT.0
);
const _: () = assert!(
    ddi12::D3D12DDI_FILTER_D3D12DDI_FILTER_MIN_MAG_MIP_LINEAR
        == windows::Win32::Graphics::Direct3D12::D3D12_FILTER_MIN_MAG_MIP_LINEAR.0
);
const _: () = assert!(
    ddi12::D3D12DDI_FILTER_D3D12DDI_FILTER_ANISOTROPIC
        == windows::Win32::Graphics::Direct3D12::D3D12_FILTER_ANISOTROPIC.0
);
const _: () = assert!(
    ddi12::D3D12DDI_FILTER_D3D12DDI_FILTER_COMPARISON_MIN_MAG_MIP_POINT
        == windows::Win32::Graphics::Direct3D12::D3D12_FILTER_COMPARISON_MIN_MAG_MIP_POINT.0
);
const _: () = assert!(
    ddi12::D3D12DDI_FILTER_D3D12DDI_FILTER_MINIMUM_ANISOTROPIC
        == windows::Win32::Graphics::Direct3D12::D3D12_FILTER_MINIMUM_ANISOTROPIC.0
);
const _: () = assert!(
    ddi12::D3D12DDI_FILTER_D3D12DDI_FILTER_MAXIMUM_ANISOTROPIC
        == windows::Win32::Graphics::Direct3D12::D3D12_FILTER_MAXIMUM_ANISOTROPIC.0
);
const _: () = assert!(
    ddi12::D3D12DDI_TEXTURE_ADDRESS_MODE_D3D12DDI_TEXTURE_ADDRESS_MODE_WRAP
        == windows::Win32::Graphics::Direct3D12::D3D12_TEXTURE_ADDRESS_MODE_WRAP.0
);
const _: () = assert!(
    ddi12::D3D12DDI_TEXTURE_ADDRESS_MODE_D3D12DDI_TEXTURE_ADDRESS_MODE_MIRRORONCE
        == windows::Win32::Graphics::Direct3D12::D3D12_TEXTURE_ADDRESS_MODE_MIRROR_ONCE.0
);
const _: () = assert!(
    ddi12::D3D12DDI_COMPARISON_FUNC_D3D12DDI_COMPARISON_FUNC_NEVER
        == windows::Win32::Graphics::Direct3D12::D3D12_COMPARISON_FUNC_NEVER.0
);
const _: () = assert!(
    ddi12::D3D12DDI_COMPARISON_FUNC_D3D12DDI_COMPARISON_FUNC_ALWAYS
        == windows::Win32::Graphics::Direct3D12::D3D12_COMPARISON_FUNC_ALWAYS.0
);

/// `D3D12DDI_SAMPLER_FLAG_UINT_BORDER_COLOR` (`0x1`).
const DDI_SAMPLER_FLAG_UINT_BORDER_COLOR: ddi12::D3D12DDI_SAMPLER_FLAGS_0096 =
    ddi12::D3D12DDI_SAMPLER_FLAGS_0096_D3D12DDI_SAMPLER_FLAG_UINT_BORDER_COLOR;
/// `D3D12DDI_SAMPLER_FLAG_NON_NORMALIZED_COORDINATES` (`0x2`).
const DDI_SAMPLER_FLAG_NON_NORMALIZED_COORDINATES: ddi12::D3D12DDI_SAMPLER_FLAGS_0096 =
    ddi12::D3D12DDI_SAMPLER_FLAGS_0096_D3D12DDI_SAMPLER_FLAG_NON_NORMALIZED_COORDINATES;
/// Every DDI sampler flag this build's header defines.
const DDI_SAMPLER_FLAGS_KNOWN: ddi12::D3D12DDI_SAMPLER_FLAGS_0096 =
    DDI_SAMPLER_FLAG_UINT_BORDER_COLOR | DDI_SAMPLER_FLAG_NON_NORMALIZED_COORDINATES;

/// DDI sampler flags -> API sampler flags, translated bit by bit.
fn api_sampler_flags(f: ddi12::D3D12DDI_SAMPLER_FLAGS_0096) -> D3D12_SAMPLER_FLAGS {
    let mut out = D3D12_SAMPLER_FLAG_NONE;
    if f & DDI_SAMPLER_FLAG_UINT_BORDER_COLOR != 0 {
        out |= D3D12_SAMPLER_FLAG_UINT_BORDER_COLOR;
    }
    if f & DDI_SAMPLER_FLAG_NON_NORMALIZED_COORDINATES != 0 {
        out |= D3D12_SAMPLER_FLAG_NON_NORMALIZED_COORDINATES;
    }
    out
}

/// `pfnCreateSampler` at the `_0096` revision.
///
/// ⭐ Forwards to **`ID3D12Device11::CreateSampler2`**, not
/// `ID3D12Device::CreateSampler`, because the `_0096` DDI desc carries `Flags`
/// and a border colour that may be `UINT`, and only the `2` form can express
/// either. Non-normalized coordinates are a **mandatory** DDI 0100+ behaviour
/// (`SUBSTRATE.md` §4.5), so dropping the flag would be declining an obligation
/// that carries no cap. vkd3d honours both flags:
/// `libs/vkd3d/resource.c:8419` (uint border) and `:8484`
/// (`unnormalizedCoordinates`).
///
/// ⚠ The `QueryInterface` is per call. It is a GUID-compare chain plus an
/// AddRef/Release inside vkd3d, against a `CreateSampler2` that builds or looks
/// up a `VkSampler`; measuring before caching it in a device field would be the
/// order of work `CLAUDE.md` rule 7 asks for, and no measurement exists yet.
///
/// # Safety
/// `arg` must point at a live `D3D12DDIARG_CREATE_SAMPLER_0096` whose
/// `pSamplerDesc` is a live `D3D12DDI_SAMPLER_DESC_0096`, and `dest` at a CPU
/// descriptor handle this driver minted.
unsafe extern "C" fn create_sampler(
    h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_SAMPLER_0096,
    dest: ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
) {
    // SAFETY: device-scope DDI; see `create_descriptor_heap`.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        return;
    };
    if arg.is_null() {
        note_refusal(&DESCRIPTOR_REFUSALS.view_bad_arg);
        report_error(dev, E_INVALIDARG);
        return;
    }
    // SAFETY: non-null per the check above; `_In_ CONST` per the DDI.
    let a = unsafe { &*arg };
    if a.pSamplerDesc.is_null() {
        note_refusal(&DESCRIPTOR_REFUSALS.view_bad_arg);
        report_error(dev, E_INVALIDARG);
        return;
    }
    // SAFETY: non-null per the check above; the DDI declares the inner pointer
    // `_In_ CONST` for the duration of the call.
    let s = unsafe { &*a.pSamplerDesc };

    let Some(engine) = dev.engine.d3d12_device() else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        report_error(dev, E_FAIL);
        return;
    };
    // ⚠ `cast` is a real `QueryInterface`, so the result is an OWNED reference
    // and is released when it drops at the end of this function. The borrowed
    // `ManuallyDrop<ID3D12Device>` it came from is untouched.
    let Ok(engine11) = engine.cast::<ID3D12Device11>() else {
        DESCRIPTOR_REFUSALS.sampler_no_device11.bump();
        log_error!(
            "CreateSampler: engine does not expose ID3D12Device11; CreateSampler2 is the only \
             form that carries D3D12DDI_SAMPLER_FLAGS -> refusing"
        );
        report_error(dev, E_FAIL);
        return;
    };

    if s.Flags & !DDI_SAMPLER_FLAGS_KNOWN != 0 {
        note_refusal(&DESCRIPTOR_REFUSALS.sampler_flags_unknown);
    }

    // ⚠ The border-colour union arm is selected by the UINT_BORDER_COLOR flag,
    // and both arms are four 32-bit words. Reading the arm the flag names keeps
    // the choice explicit instead of relying on the layouts coinciding.
    let border = if s.Flags & DDI_SAMPLER_FLAG_UINT_BORDER_COLOR != 0 {
        D3D12_SAMPLER_DESC2_0 {
            // SAFETY: the flag names the `UintBorderColor` arm.
            UintBorderColor: unsafe { s.__bindgen_anon_1.UintBorderColor },
        }
    } else {
        D3D12_SAMPLER_DESC2_0 {
            // SAFETY: the flag's absence names the `FloatBorderColor` arm.
            FloatBorderColor: unsafe { s.__bindgen_anon_1.FloatBorderColor },
        }
    };

    let desc = D3D12_SAMPLER_DESC2 {
        Filter: D3D12_FILTER(s.Filter),
        AddressU: D3D12_TEXTURE_ADDRESS_MODE(s.AddressU),
        AddressV: D3D12_TEXTURE_ADDRESS_MODE(s.AddressV),
        AddressW: D3D12_TEXTURE_ADDRESS_MODE(s.AddressW),
        MipLODBias: s.MipLODBias,
        MaxAnisotropy: s.MaxAnisotropy,
        ComparisonFunc: D3D12_COMPARISON_FUNC(s.ComparisonFunc),
        Anonymous: border,
        MinLOD: s.MinLOD,
        MaxLOD: s.MaxLOD,
        Flags: api_sampler_flags(s.Flags),
    };

    trace_line!(
        "CreateSampler: filter={} addr=({},{},{}) cmp={} flags={:#x} dest={:#018x}",
        s.Filter,
        s.AddressU,
        s.AddressV,
        s.AddressW,
        s.ComparisonFunc,
        s.Flags,
        dest.ptr,
    );
    // ⚠ Not a refusal, and bumped HERE rather than on entry so the number is
    // exact: it is the instrument for `GATES.md` §7.24, where the host GPU's
    // `maxSamplerAllocationCount` is exactly 4000 and this counts the sampler
    // descriptors actually handed to the engine -- the budget consumed if vkd3d
    // allocates one `VkSampler` per descriptor write.
    DESCRIPTOR_REFUSALS.sampler_creates.bump();
    // SAFETY: `engine11` is a live owned reference to the engine's device;
    // `desc` is a live local whose address does not escape the call; `dest`
    // addresses engine descriptor storage this driver minted.
    unsafe { engine11.CreateSampler2(core::ptr::from_ref(&desc), api_cpu_handle(dest)) };
}

// ---------------------------------------------------------------------------
// Sampler feedback
// ---------------------------------------------------------------------------

/// `pfnCreateSamplerFeedbackUnorderedAccessView` — the fifteenth slot of group
/// (f), refused.
///
/// ⛔ Two independent reasons, and both are recorded so the refusal is not
/// mistaken for laziness:
///
/// 1. `caps12.rs:594` reports `SamplerFeedbackTier = NOT_SUPPORTED`, so no
///    sampler-feedback resource can legally exist for this to describe. Same
///    position `caps12`'s `pfnGetMipPacking` takes for tiled resources: a slot
///    that cannot be reached legally still must not leave the runtime's
///    descriptor holding whatever was there.
/// 2. It takes **two** `D3D12DDI_HRESOURCE` handles, which is [`engine_resource`]
///    twice over.
///
/// ⚠ Expected **0**. A hit means the runtime reached a sampler-feedback path on
/// a driver that reports no tier, which is a caps inconsistency elsewhere.
///
/// # Safety
/// `dest` must be a CPU descriptor handle this driver minted.
unsafe extern "C" fn create_sampler_feedback_unordered_access_view(
    h_device: ddi12::D3D12DDI_HDEVICE,
    _h_targeted_resource: ddi12::D3D12DDI_HRESOURCE,
    _h_feedback_resource: ddi12::D3D12DDI_HRESOURCE,
    _dest: ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
) {
    note_refusal(&DESCRIPTOR_REFUSALS.sampler_feedback_refused);
    // SAFETY: device-scope DDI; see `create_descriptor_heap`.
    if let Some(dev) = unsafe { device12::device(h_device) } {
        report_error(dev, E_FAIL);
    } else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
    }
}

// ---------------------------------------------------------------------------
// Descriptor copies
// ---------------------------------------------------------------------------

// ⭐ THE TWO CPU-HANDLE STRUCTS ARE ONE C TYPE DESCRIBED TWICE, AND THAT IS
// PINNED RATHER THAN ASSUMED.
//
// `D3D12DDI_CPU_DESCRIPTOR_HANDLE` (bindgen, from `d3d12umddi.h`) and
// `D3D12_CPU_DESCRIPTOR_HANDLE` (windows-rs, from the Win32 metadata) are both
// `#[repr(C)]` one-field structs over a machine word. `pfnCopyDescriptors`
// hands the driver ARRAYS of the DDI form and `ID3D12Device::CopyDescriptors`
// takes arrays of the API form.
//
// ⛔ This is NOT `ARCHITECTURE.md` §12 rule 1's prohibition. That rule forbids
// hand-transcribing an ABI; both of these are machine-generated from their own
// authority, and the assertions below check that the two generators agree on
// size, alignment and the single field's offset -- which for a `#[repr(C)]`
// struct with one field is the whole of its layout. The alternative, copying
// each array element into a fresh `Vec` on a path an application drives every
// frame, is an allocation per descriptor batch and buys no additional proof.
const _: () = assert!(
    core::mem::size_of::<ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE>()
        == core::mem::size_of::<D3D12_CPU_DESCRIPTOR_HANDLE>()
);
const _: () = assert!(
    core::mem::align_of::<ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE>()
        == core::mem::align_of::<D3D12_CPU_DESCRIPTOR_HANDLE>()
);
const _: () = assert!(core::mem::offset_of!(ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE, ptr) == 0);
const _: () = assert!(core::mem::offset_of!(D3D12_CPU_DESCRIPTOR_HANDLE, ptr) == 0);
// ⚠ And the reason the assertions above are not redundant with "both are one
// machine word": the two generators do not even agree on the Rust integer TYPE.
// bindgen renders C `SIZE_T` as `c_ulonglong` (`d3d12umddi.rs:352-353`, so `u64`)
// while windows-rs renders it as `usize`. They are the same width on
// `x86_64-pc-windows-msvc` -- pinned here -- which is what makes both the `as`
// conversions below and the array casts above lossless and layout-preserving.
const _: () = assert!(core::mem::size_of::<usize>() == core::mem::size_of::<ddi12::SIZE_T>());

/// One DDI CPU descriptor handle as the API's, by field.
///
/// Used wherever a handle crosses **by value**; the array form is the pointer
/// cast above, guarded by the same assertions.
///
/// ⚠ `pub(crate)` for L3b: the five clear/discard slots and
/// `pfnClearRenderTargetView`/`pfnClearDepthStencilView` all take a
/// `D3D12DDI_CPU_DESCRIPTOR_HANDLE` by value and must hand the engine the API
/// form. Re-deriving the conversion there would put a second copy of the
/// assertions above in a second file, which is exactly how two claims about one
/// ABI drift apart.
pub(crate) fn api_cpu_handle(
    h: ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
) -> D3D12_CPU_DESCRIPTOR_HANDLE {
    D3D12_CPU_DESCRIPTOR_HANDLE {
        ptr: h.ptr as usize,
    }
}

// ⭐ AND THE GPU SIBLING, PINNED THE SAME WAY. `D3D12DDI_GPU_DESCRIPTOR_HANDLE`
// (bindgen, `d3d12umddi.rs:50628`) and `D3D12_GPU_DESCRIPTOR_HANDLE`
// (windows-rs, from the Win32 metadata) are both `#[repr(C)]` one-field structs
// over a 64-bit scalar. `pfnSet{Compute,Graphics}RootDescriptorTable` and the
// two `pfnClearUnorderedAccessView*` slots hand one across by value.
//
// ⚠ Unlike the CPU pair, the two generators DO agree on the Rust integer type
// here (`UINT64` -> `u64` on one side, `u64` on the other), so no `as`
// conversion is involved and the assertions are the whole proof. They are kept
// anyway, and separate from the CPU pair's: the CPU pair's `usize`-vs-`u64`
// assertion says nothing about this struct, and a header revision that widened
// or padded one side would otherwise be caught by nothing.
const _: () = assert!(
    core::mem::size_of::<ddi12::D3D12DDI_GPU_DESCRIPTOR_HANDLE>()
        == core::mem::size_of::<D3D12_GPU_DESCRIPTOR_HANDLE>()
);
const _: () = assert!(
    core::mem::align_of::<ddi12::D3D12DDI_GPU_DESCRIPTOR_HANDLE>()
        == core::mem::align_of::<D3D12_GPU_DESCRIPTOR_HANDLE>()
);
const _: () = assert!(core::mem::offset_of!(ddi12::D3D12DDI_GPU_DESCRIPTOR_HANDLE, ptr) == 0);
const _: () = assert!(core::mem::offset_of!(D3D12_GPU_DESCRIPTOR_HANDLE, ptr) == 0);

/// One DDI GPU descriptor handle as the API's, by field.
///
/// ⭐ The value crossing here is **vkd3d's own `gpu_va`**, handed back
/// unchanged by [`get_gpu_descriptor_handle_for_heap_start`] and then advanced
/// by the runtime with this driver's own increment — see this module's doc. So
/// this conversion is a type change and nothing else, and it must stay one:
/// arithmetic on a GPU handle anywhere in this driver would be a second
/// descriptor model beside the engine's.
pub(crate) fn api_gpu_handle(
    h: ddi12::D3D12DDI_GPU_DESCRIPTOR_HANDLE,
) -> D3D12_GPU_DESCRIPTOR_HANDLE {
    D3D12_GPU_DESCRIPTOR_HANDLE { ptr: h.ptr }
}

/// `pfnCopyDescriptors`.
///
/// ⛔ Every runtime-supplied count is validated **per arm** before its array is
/// read: a non-zero range count with a null array pointer is the read that
/// `CLAUDE.md`'s "validate every runtime/guest-supplied size & offset before
/// reading" exists for. The two `*RangeSizes` arrays are legitimately optional
/// (a null one means "one descriptor per range"), and that is the only null this
/// slot accepts.
///
/// # Safety
/// The four array pointers, when non-null, must each address at least the number
/// of elements their paired count states, for the duration of the call.
unsafe extern "C" fn copy_descriptors(
    h_device: ddi12::D3D12DDI_HDEVICE,
    num_dest_ranges: ddi12::UINT,
    dest_range_starts: *const ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
    dest_range_sizes: *const ddi12::UINT,
    num_src_ranges: ddi12::UINT,
    src_range_starts: *const ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
    src_range_sizes: *const ddi12::UINT,
    heap_type: ddi12::D3D12DDI_DESCRIPTOR_HEAP_TYPE,
) {
    // SAFETY: device-scope DDI; see `create_descriptor_heap`.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        return;
    };
    let Some(engine) = dev.engine.d3d12_device() else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        report_error(dev, E_FAIL);
        return;
    };
    let Some(api_type) = api_heap_type(heap_type) else {
        note_refusal(&DESCRIPTOR_REFUSALS.heap_type_unknown);
        report_error(dev, E_INVALIDARG);
        return;
    };
    // ⛔ Per arm, not per max-union: a zero count with a null pointer is a legal
    // no-op, a non-zero count with a null pointer is not.
    if (num_dest_ranges != 0 && dest_range_starts.is_null())
        || (num_src_ranges != 0 && src_range_starts.is_null())
    {
        note_refusal(&DESCRIPTOR_REFUSALS.copy_descriptors_bad_arg);
        report_error(dev, E_INVALIDARG);
        return;
    }
    if num_dest_ranges == 0 || num_src_ranges == 0 {
        // Nothing to copy. Not an error and not counted: the runtime is allowed
        // to ask for an empty copy.
        return;
    }

    trace_line!(
        "CopyDescriptors: type={heap_type} destRanges={num_dest_ranges} srcRanges={num_src_ranges}"
    );
    // SAFETY: the caller guarantees each non-null array holds at least its
    // paired count of elements; the two handle structs are layout-identical by
    // the assertions above, so the casts reinterpret arrays of one C type as
    // arrays of the same C type. The optional size arrays are forwarded as
    // `None` when null, which is what both DDI and API mean by their absence.
    unsafe {
        engine.CopyDescriptors(
            num_dest_ranges,
            dest_range_starts.cast::<D3D12_CPU_DESCRIPTOR_HANDLE>(),
            (!dest_range_sizes.is_null()).then_some(dest_range_sizes),
            num_src_ranges,
            src_range_starts.cast::<D3D12_CPU_DESCRIPTOR_HANDLE>(),
            (!src_range_sizes.is_null()).then_some(src_range_sizes),
            api_type,
        );
    }
}

/// `pfnCopyDescriptorsSimple`.
///
/// # Safety
/// `dest` and `src` must each be the start of at least `num_descriptors`
/// descriptors this driver minted, in a heap of `heap_type`.
unsafe extern "C" fn copy_descriptors_simple(
    h_device: ddi12::D3D12DDI_HDEVICE,
    num_descriptors: ddi12::UINT,
    dest: ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
    src: ddi12::D3D12DDI_CPU_DESCRIPTOR_HANDLE,
    heap_type: ddi12::D3D12DDI_DESCRIPTOR_HEAP_TYPE,
) {
    // SAFETY: device-scope DDI; see `create_descriptor_heap`.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        return;
    };
    let Some(engine) = dev.engine.d3d12_device() else {
        note_refusal(&DESCRIPTOR_REFUSALS.no_device);
        report_error(dev, E_FAIL);
        return;
    };
    let Some(api_type) = api_heap_type(heap_type) else {
        note_refusal(&DESCRIPTOR_REFUSALS.heap_type_unknown);
        report_error(dev, E_INVALIDARG);
        return;
    };
    if num_descriptors == 0 {
        return;
    }
    trace_line!(
        "CopyDescriptorsSimple: type={heap_type} n={num_descriptors} dest={:#018x} src={:#018x}",
        dest.ptr,
        src.ptr,
    );
    // SAFETY: both handles cross by value and are converted field by field; the
    // caller guarantees each addresses at least `num_descriptors` descriptors in
    // a heap of this type.
    unsafe {
        engine.CopyDescriptorsSimple(
            num_descriptors,
            api_cpu_handle(dest),
            api_cpu_handle(src),
            api_type,
        );
    }
}

// ---------------------------------------------------------------------------
// Shared logging
// ---------------------------------------------------------------------------

/// One bounded line per refused view creation, carrying the two fields that
/// identify which view the workload wanted.
///
/// ⭐ The evidence half of the L4 seam: even while [`engine_resource`] refuses,
/// the log answers *"which view dimensions and formats does this workload
/// create"*, which is exactly what the next round needs and what a silent
/// refusal would have cost.
///
/// ⚠ Its callers `bump()` their counter rather than `note_refusal`, because this
/// line makes the arm loud already and R911 forbids an already-loud arm from
/// also printing the whole set. The set is still read: `device12`'s teardown and
/// `adapter12::close_adapter` both call `crate::log_refusal_summary`.
fn log_view_refusal(
    slot: &str,
    format: ddi12::DXGI_FORMAT,
    resource_dimension: ddi12::D3D12DDI_RESOURCE_DIMENSION,
) {
    let n = DESCRIPTOR_REFUSALS.view_resource_unavailable.get()
        + DESCRIPTOR_REFUSALS.view_dimension_unknown.get();
    if n <= LOG_BUDGET {
        log_error!("{slot}: fmt={format} ddiDim={resource_dimension} -> refused (x{n})");
    }
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// Install L5's 15 device-core slots.
///
/// Chain position: `ResourceSlots` -> `DescriptorSlots` on the device-core
/// table.
pub(crate) fn install(
    mut filling: Filling<'_, DeviceCoreTable, stage::ResourceSlots>,
) -> Filling<'_, DeviceCoreTable, stage::DescriptorSlots> {
    let table = filling.table();
    table.pfnCalcPrivateDescriptorHeapSize = Some(calc_private_descriptor_heap_size);
    table.pfnCreateDescriptorHeap = Some(create_descriptor_heap);
    table.pfnDestroyDescriptorHeap = Some(destroy_descriptor_heap);
    table.pfnGetDescriptorSizeInBytes = Some(get_descriptor_size_in_bytes);
    table.pfnGetCPUDescriptorHandleForHeapStart = Some(get_cpu_descriptor_handle_for_heap_start);
    table.pfnGetGPUDescriptorHandleForHeapStart = Some(get_gpu_descriptor_handle_for_heap_start);
    table.pfnCreateShaderResourceView = Some(create_shader_resource_view);
    table.pfnCreateConstantBufferView = Some(create_constant_buffer_view);
    table.pfnCreateSampler = Some(create_sampler);
    table.pfnCreateUnorderedAccessView = Some(create_unordered_access_view);
    table.pfnCreateRenderTargetView = Some(create_render_target_view);
    table.pfnCreateDepthStencilView = Some(create_depth_stencil_view);
    table.pfnCopyDescriptors = Some(copy_descriptors);
    table.pfnCopyDescriptorsSimple = Some(copy_descriptors_simple);
    table.pfnCreateSamplerFeedbackUnorderedAccessView =
        Some(create_sampler_feedback_unordered_access_view);
    filling.advance()
}

// ---------------------------------------------------------------------------
// Refusal counters
// ---------------------------------------------------------------------------

/// L5's refusal counters.
///
/// ⛔ **Append only.** Counter order inside a set, and set order in `lib.rs`'s
/// `UMD12_REFUSAL_SETS`, are both the evidence contract: `D3D12 DDI refusals:`
/// lines get diffed across builds.
struct DescriptorRefusals {
    /// A descriptor-heap DDI arrived with a null `pArgs` or a null
    /// `hDrvHeap.pDrvPrivate`. Expected 0 - the runtime supplies both, and it
    /// allocates the private block from this driver's own
    /// `pfnCalcPrivateDescriptorHeapSize`.
    heap_bad_arg: RefusalCounter,
    /// A device-scope slot in this lane could not reach the engine: the
    /// `hDevice` did not resolve, or the bridge carries no `ID3D12Device`.
    /// Expected 0 - these are device-scope DDIs and a device exists by
    /// construction. Counted anyway because "unreachable by construction" is a
    /// claim about a cross-FFI contract and this is where it would break.
    no_device: RefusalCounter,
    /// A `D3D12DDI_DESCRIPTOR_HEAP_TYPE` outside the four this header defines.
    /// Expected 0; `NUM_TYPES` (4) is a sentinel and not a heap type.
    heap_type_unknown: RefusalCounter,
    /// `pArgs->Flags` carried bits outside `CPU_VISIBLE | SHADER_VISIBLE`.
    /// Expected 0 - and a hit means the DDI grew a flag this translation does
    /// not know about, which is the one way the API/DDI collision could get
    /// worse rather than better.
    heap_flags_unknown: RefusalCounter,
    /// A heap was requested with a `NodeMask` other than 0 or 1. ⛔ Expected 0
    /// on this guest: Helios is a single-node adapter. A hit is
    /// `ARCHITECTURE.md` §13 UNVERIFIED-11 being reached for real.
    heap_node_mask_unexpected: RefusalCounter,
    /// `ID3D12Device::CreateDescriptorHeap` returned a failure HRESULT; the
    /// engine's code is returned to the runtime unchanged. Expected 0.
    heap_engine_failed: RefusalCounter,
    /// `pfnGetDescriptorSizeInBytes` answered 0. ⛔ Expected 0 and
    /// unrecoverable if not: every descriptor address the runtime and the
    /// application compute is `heapStart + i * stride`, so a zero stride aliases
    /// a whole heap onto its first descriptor with no error anywhere.
    descriptor_stride_zero: RefusalCounter,
    /// `pfnGetCPUDescriptorHandleForHeapStart` answered 0.
    ///
    /// ⛔ **The `ead692e` canary.** vkd3d assigns `cpu_va` a real host pointer on
    /// every creation path (`libs/vkd3d/resource.c:10367-10420`), so a zero
    /// cannot be the engine's answer: it is an empty slot or a struct-return ABI
    /// truncation. Expected 0. See this module's doc for why the analysis says
    /// the two conventions agree, and what to re-check if this ever moves.
    heap_cpu_handle_zero: RefusalCounter,
    /// `pfnGetGPUDescriptorHandleForHeapStart` answered 0.
    ///
    /// ⚠ **Expected NON-ZERO, once per non-shader-visible heap**, and it is a
    /// known disagreement rather than a fault: `ResourceBinding.md:4428-4429`
    /// says every heap must have a non-NULL GPU address at the DDI, while vkd3d
    /// assigns `gpu_va` only under
    /// `if (desc->Flags & D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE)`
    /// (`libs/vkd3d/resource.c:10159`). The engine's value is forwarded rather
    /// than a non-zero one invented, because a synthesised handle would come
    /// back to `pfnSetGraphicsRootDescriptorTable` as an address vkd3d cannot
    /// decode. UNVERIFIED: whether the runtime rejects a zero here at all.
    heap_gpu_handle_zero: RefusalCounter,
    /// How many heap-start handles have been logged, CPU and GPU together. ⚠
    /// Not a refusal - it bounds the round-trip evidence lines that make a
    /// truncated handle visible in the log rather than only in a crash.
    heap_start_handles: RefusalCounter,
    /// A view-creation DDI arrived with a null `pArgs`, or a null inner
    /// descriptor pointer. Expected 0. ⚠ It does **not** count
    /// `pfnCreateConstantBufferView` with a null desc, which is the legal
    /// null-descriptor form and is forwarded.
    view_bad_arg: RefusalCounter,
    /// A view could not be created because the engine `ID3D12Resource` behind
    /// its **non-null** `D3D12DDI_HRESOURCE` could not be resolved.
    ///
    /// ⚠ A *null* handle is not this: that is D3D12's null-descriptor form,
    /// it is legal, and it is counted on `ViewNullResource` and forwarded.
    ///
    /// ⛔ **Expected 0.** ⚠ It was written as *"expected NON-ZERO on any workload
    /// that draws, until L4 lands"*, which was true for exactly as long as this
    /// lane was authored in isolation: **L4 landed in the same batch** and
    /// [`engine_resource`] forwards to `resource12::engine_resource` now. So the
    /// counter's meaning inverted at merge, and a grading left behind is worse
    /// than no grading — this is the readout for the run the merge exists to
    /// enable, and "expected non-zero" would have read a device-removal count as
    /// normal traffic.
    ///
    /// What a hit means now: the runtime named a **non-null** resource handle
    /// whose private word `resource12` did not write — a lifetime or ownership
    /// defect, not a missing lane. Every one of these also raises
    /// `pfnSetErrorCb`, so **the device is removed** rather than rendering from a
    /// descriptor nobody wrote.
    view_resource_unavailable: RefusalCounter,
    /// A UAV named a counter resource whose engine `ID3D12Resource` could not be
    /// resolved, so **the view was refused** and `pfnSetErrorCb` raised.
    ///
    /// ⛔ **Expected 0**, and counted apart from `ViewResourceUnavailable`
    /// because the two have the same cause and different consequences: an
    /// unresolvable *view* resource produces a descriptor nobody wrote, while an
    /// unresolvable *counter* would produce the null UAV counter descriptor the
    /// 72nd memory records **faulting the host GPU context and hanging the
    /// guest** — an unrecoverable, undiagnosable failure rather than a removed
    /// device.
    ///
    /// ⚠ This arm used to bump-and-continue, with that consequence written down
    /// right here. Recording a hazard is not mitigating it: the review pass
    /// found the slot's own doc promising it *"never drops one"* while the code
    /// dropped one. Both arms refuse now.
    uav_counter_unavailable: RefusalCounter,
    /// A view's `ResourceDimension` named no arm this translation handles.
    /// Expected 0 - the six DDI dimensions are all covered, so a hit means the
    /// header grew one.
    view_dimension_unknown: RefusalCounter,
    /// How many sampler descriptors this process has handed to the engine
    /// (bumped at the forward, so a refused call is not in it). ⚠ Not a
    /// refusal - it is the instrument for `GATES.md` §7.24: the host GPU's
    /// `VkPhysicalDeviceLimits::maxSamplerAllocationCount` is exactly 4000
    /// (`docs/reference/host-vulkan-profile-rtx-pro-6000-blackwell.json:882`)
    /// and `MaxSamplerDescriptorHeapSize` must be reported >= 4000, so this
    /// number is the budget consumed if vkd3d allocates one `VkSampler` per
    /// descriptor.
    sampler_creates: RefusalCounter,
    /// `ID3D12Device::QueryInterface(IID_ID3D12Device11)` failed, so
    /// `CreateSampler2` - the only form carrying `D3D12DDI_SAMPLER_FLAGS` - was
    /// unreachable and the sampler was refused. ⚠ Expected 0: vkd3d accepts
    /// `IID_ID3D12Device11` at `libs/vkd3d/device.c:4648`. A hit means the
    /// engine's interface set moved.
    sampler_no_device11: RefusalCounter,
    /// A sampler desc carried `Flags` bits outside the two `_0096` defines.
    /// Expected 0.
    sampler_flags_unknown: RefusalCounter,
    /// `pfnCreateSamplerFeedbackUnorderedAccessView` was called. ⛔ Expected 0:
    /// `caps12.rs:594` reports `SamplerFeedbackTier = NOT_SUPPORTED`, so no
    /// sampler-feedback resource can legally exist. A hit is a caps
    /// inconsistency elsewhere, not something this slot can fix.
    sampler_feedback_refused: RefusalCounter,
    /// `pfnCopyDescriptors` was given a non-zero range count with a null range
    /// array. Expected 0. ⚠ The two `*RangeSizes` arrays are legitimately
    /// optional and their absence is not counted here.
    copy_descriptors_bad_arg: RefusalCounter,
    /// A `VOID` slot in this lane needed to report a failure and the runtime's
    /// `pfnSetErrorCb` was absent. ⛔ Expected 0, and a hit is worse than the
    /// failure it could not report: `DECISIONS.md` §7.6 makes that callback the
    /// only error channel a `VOID` D3D12 DDI has, so losing it turns a removed
    /// device into corrupt output.
    set_error_cb_missing: RefusalCounter,
    /// A view was created with **no** resource: D3D12's null-descriptor form,
    /// which the runtime lowers as a null `hDrvResource`.
    ///
    /// ⚠ **Not a refusal, and expected non-zero** on any application that binds
    /// a descriptor table with an unbound slot. It is forwarded to the engine
    /// with a null `pResource` — vkd3d writes its null-descriptor template
    /// (`libs/vkd3d/resource.c:7204-7208`) and requires
    /// `VK_EXT_robustness2::nullDescriptor` for it (`SUBSTRATE.md:426`) — and it
    /// never raises `pfnSetErrorCb`. Counted apart from
    /// `ViewResourceUnavailable` because the two look identical at
    /// `engine_resource` and mean opposite things: one is a legal call, the
    /// other is this driver failing to resolve a resource the runtime named.
    /// Appended after `DescriptorSetErrorCbMissing` because the declaration
    /// order of this set is the evidence contract.
    view_null_resource: RefusalCounter,
}

static DESCRIPTOR_REFUSALS: DescriptorRefusals = DescriptorRefusals {
    heap_bad_arg: RefusalCounter::new("DescriptorHeapBadArg"),
    no_device: RefusalCounter::new("DescriptorSlotNoDevice"),
    heap_type_unknown: RefusalCounter::new("DescriptorHeapTypeUnknown"),
    heap_flags_unknown: RefusalCounter::new("DescriptorHeapFlagsUnknown"),
    heap_node_mask_unexpected: RefusalCounter::new("DescriptorHeapNodeMaskUnexpected"),
    heap_engine_failed: RefusalCounter::new("DescriptorHeapEngineFailed"),
    descriptor_stride_zero: RefusalCounter::new("DescriptorStrideZero"),
    heap_cpu_handle_zero: RefusalCounter::new("DescriptorHeapCpuHandleZero"),
    heap_gpu_handle_zero: RefusalCounter::new("DescriptorHeapGpuHandleZero"),
    heap_start_handles: RefusalCounter::new("DescriptorHeapStartHandles"),
    view_bad_arg: RefusalCounter::new("ViewBadArg"),
    view_resource_unavailable: RefusalCounter::new("ViewResourceUnavailable"),
    uav_counter_unavailable: RefusalCounter::new("UavCounterUnavailable"),
    view_dimension_unknown: RefusalCounter::new("ViewDimensionUnknown"),
    sampler_creates: RefusalCounter::new("SamplerCreates"),
    sampler_no_device11: RefusalCounter::new("SamplerNoDevice11"),
    sampler_flags_unknown: RefusalCounter::new("SamplerFlagsUnknown"),
    sampler_feedback_refused: RefusalCounter::new("SamplerFeedbackRefused"),
    copy_descriptors_bad_arg: RefusalCounter::new("CopyDescriptorsBadArg"),
    set_error_cb_missing: RefusalCounter::new("DescriptorSetErrorCbMissing"),
    view_null_resource: RefusalCounter::new("ViewNullResource"),
};

/// L5's refusal counters, printed by `crate::log_refusal_summary` at this lane's
/// position in `lib.rs`'s `UMD12_REFUSAL_SETS`.
///
/// ⭐ Declared here rather than in `lib.rs` so this lane's diff against the crate
/// root is empty (`PARALLEL.md` §5 does not even list `lib.rs` among the shared
/// files, which is what made it the split's hottest unlisted merge point).
///
/// ⛔ Declaration order here is the print order and is the evidence contract.
pub(crate) static REFUSALS: &[&RefusalCounter] = &[
    &DESCRIPTOR_REFUSALS.heap_bad_arg,
    &DESCRIPTOR_REFUSALS.no_device,
    &DESCRIPTOR_REFUSALS.heap_type_unknown,
    &DESCRIPTOR_REFUSALS.heap_flags_unknown,
    &DESCRIPTOR_REFUSALS.heap_node_mask_unexpected,
    &DESCRIPTOR_REFUSALS.heap_engine_failed,
    &DESCRIPTOR_REFUSALS.descriptor_stride_zero,
    &DESCRIPTOR_REFUSALS.heap_cpu_handle_zero,
    &DESCRIPTOR_REFUSALS.heap_gpu_handle_zero,
    &DESCRIPTOR_REFUSALS.heap_start_handles,
    &DESCRIPTOR_REFUSALS.view_bad_arg,
    &DESCRIPTOR_REFUSALS.view_resource_unavailable,
    &DESCRIPTOR_REFUSALS.uav_counter_unavailable,
    &DESCRIPTOR_REFUSALS.view_dimension_unknown,
    &DESCRIPTOR_REFUSALS.sampler_creates,
    &DESCRIPTOR_REFUSALS.sampler_no_device11,
    &DESCRIPTOR_REFUSALS.sampler_flags_unknown,
    &DESCRIPTOR_REFUSALS.sampler_feedback_refused,
    &DESCRIPTOR_REFUSALS.copy_descriptors_bad_arg,
    &DESCRIPTOR_REFUSALS.set_error_cb_missing,
    &DESCRIPTOR_REFUSALS.view_null_resource,
];
