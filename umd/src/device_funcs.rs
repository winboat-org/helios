//! D3D11 device object + device-funcs table fill (Gate 5b, Milestone 1).
//!
//! The OS D3D11 runtime drives `D3D11CreateDevice` into our adapter `CreateDevice`
//! DDI, which must hand back a fully-populated `D3D11DDI_DEVICEFUNCS` table (152
//! entries) and return S_OK. A null entry the runtime calls = crash, so we fill
//! **all** entries with a safe stub and specialise the few whose return value
//! matters. This is the minimal honest device that lets the runtime accept the
//! device (Milestone 1 = D3D11CreateDevice S_OK → DWM stops fail-fasting); real
//! rendering DDIs come later, backed by the DXVK device this object holds.
//!
//! ABI note: every device DDI takes `D3D10DDI_HDEVICE` (one pointer) as its first
//! arg and the x64 calling convention is caller-clean, so a uniform
//! `extern "C" fn(usize) -> usize` stub transmuted into each slot reads only the
//! first arg, ignores the rest, and returns in RAX — valid for the void / HRESULT
//! / SIZE_T return shapes alike.

use crate::bridge;
use crate::ddi;
use crate::log_line;

/// Per-device UMD state, constructed in-place in the runtime-allocated private
/// device memory (size = [`device_private_size`]). Owns the DXVK device the cxx
/// bridge created on the Helios venus adapter.
pub struct HeliosDevice {
    pub dxvk: cxx::UniquePtr<bridge::ffi::HeliosDxvkDevice>,
}

pub fn device_private_size() -> usize {
    core::mem::size_of::<HeliosDevice>()
}

/// Uniform stub signature (one machine word in, one out).
type UniformFn = unsafe extern "C" fn(usize) -> usize;

/// No-op DDI stub: returns 0 (S_OK for HRESULT funcs; ignored for void funcs).
unsafe extern "C" fn ddi_noop(_a: usize) -> usize {
    0
}

/// CalcPrivate*Size stub: return a small nonzero, pointer-aligned size so the
/// runtime's driver-private object allocation is valid. Our Create* stubs never
/// write into it and no other stub reads it, so the exact size is immaterial.
unsafe extern "C" fn ddi_calc_size(_a: usize) -> usize {
    256
}

/// Real DestroyDevice: drop the in-place HeliosDevice (releasing the DXVK device).
/// The runtime owns the backing memory, so we only run the destructor.
unsafe extern "C" fn ddi_destroy_device(h_device: ddi::D3D10DDI_HDEVICE) {
    log_line("DDI: DestroyDevice");
    if !h_device.pDrvPrivate.is_null() {
        core::ptr::drop_in_place(h_device.pDrvPrivate as *mut HeliosDevice);
    }
}

/// Fill all 152 entries of a `D3D11DDI_DEVICEFUNCS` table with safe stubs, then
/// specialise the entries whose behaviour matters for device creation.
///
/// # Safety
/// `funcs` must point to a writable `D3D11DDI_DEVICEFUNCS` (the runtime's table,
/// selected when Interface == D3D11_0_DDI_INTERFACE_VERSION).
pub unsafe fn fill_d3d11_device_funcs(funcs: *mut ddi::D3D11DDI_DEVICEFUNCS) {
    // Every field is a pointer-sized Option<fn>; bulk-fill with the no-op.
    let n = core::mem::size_of::<ddi::D3D11DDI_DEVICEFUNCS>() / core::mem::size_of::<usize>();
    let slots = funcs as *mut Option<UniformFn>;
    for i in 0..n {
        *slots.add(i) = Some(ddi_noop);
    }

    let f = &mut *funcs;

    // CalcPrivate*Size funcs must return a valid nonzero size.
    macro_rules! calc {
        ($($field:ident),* $(,)?) => {$(
            f.$field = core::mem::transmute::<UniformFn, _>(ddi_calc_size as UniformFn);
        )*};
    }
    calc!(
        pfnCalcPrivateResourceSize,
        pfnCalcPrivateOpenedResourceSize,
        pfnCalcPrivateShaderResourceViewSize,
        pfnCalcPrivateRenderTargetViewSize,
        pfnCalcPrivateDepthStencilViewSize,
        pfnCalcPrivateElementLayoutSize,
        pfnCalcPrivateBlendStateSize,
        pfnCalcPrivateDepthStencilStateSize,
        pfnCalcPrivateRasterizerStateSize,
        pfnCalcPrivateShaderSize,
        pfnCalcPrivateGeometryShaderWithStreamOutput,
        pfnCalcPrivateSamplerSize,
        pfnCalcPrivateQuerySize,
        pfnCalcDeferredContextHandleSize,
        pfnCalcPrivateDeferredContextSize,
        pfnCalcPrivateCommandListSize,
        pfnCalcPrivateTessellationShaderSize,
        pfnCalcPrivateUnorderedAccessViewSize,
    );

    // Real cleanup on device teardown (matching signature, no transmute).
    f.pfnDestroyDevice = Some(ddi_destroy_device);

    // Override stubs with the real D3D11 COM forwarders.
    crate::forward::install(funcs);
}

/// Fill the DXGI base DDI table (presentation/resource base funcs) the runtime
/// hands us in the CREATEDEVICE args. All stubbed for Milestone 1 (no present).
///
/// # Safety
/// `funcs` must point to a writable `DXGI_DDI_BASE_FUNCTIONS`, or be null.
pub unsafe fn fill_dxgi_base_funcs(funcs: *mut ddi::DXGI_DDI_BASE_FUNCTIONS) {
    if funcs.is_null() {
        return;
    }
    let n = core::mem::size_of::<ddi::DXGI_DDI_BASE_FUNCTIONS>() / core::mem::size_of::<usize>();
    let slots = funcs as *mut Option<UniformFn>;
    for i in 0..n {
        *slots.add(i) = Some(ddi_noop);
    }
}
