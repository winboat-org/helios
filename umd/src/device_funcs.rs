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
use core::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

/// One cached dcomp-vehicle present source (road 4): an alias-imported D3D11
/// texture over the producing ICD's frame blob, keyed by venus resid. Owns
/// one COM ref on the imported resource, released on drop (eviction,
/// geometry change, or device teardown).
pub struct PresentSrcEntry {
    pub resid: u32,
    pub width: u32,
    pub height: u32,
    pub dxgi_format: u32,
    /// Owned `ID3D11Resource` COM pointer from `open_ddi_texture2d`.
    pub resource_raw: usize,
}

impl Drop for PresentSrcEntry {
    fn drop(&mut self) {
        use windows::core::Interface;
        if self.resource_raw != 0 {
            // SAFETY: `resource_raw` is the owned COM ref returned by
            // open_ddi_texture2d; from_raw adopts it so drop releases it.
            unsafe {
                drop(
                    windows::Win32::Graphics::Direct3D11::ID3D11Resource::from_raw(
                        self.resource_raw as *mut c_void,
                    ),
                );
            }
        }
    }
}

/// Per-device UMD state, constructed in-place in the runtime-allocated private
/// device memory (size = [`device_private_size`]). Owns the DXVK device the cxx
/// bridge created on the Helios venus adapter.
pub struct HeliosDevice {
    /// Dcomp present-vehicle source cache (declared before `dxvk` so entries
    /// release their D3D11 textures before the bridge device drops). Same
    /// single-threaded-DDI RefCell contract as `ia`.
    pub present_src_cache: core::cell::RefCell<Vec<PresentSrcEntry>>,
    pub dxvk: cxx::UniquePtr<bridge::ffi::HeliosDxvkDevice>,
    pub h_rt_device: ddi::HANDLE,
    pub h_context: ddi::HANDLE,
    pub kt_callbacks: *const ddi::D3DDDI_DEVICECALLBACKS,
    pub dxgi_callbacks: *mut ddi::DXGI_DDI_BASE_CALLBACKS,
    /// Runtime corelayer handle + callbacks (pfnSetErrorCb) so VOID-returning
    /// DDIs can report failures to the runtime instead of leaving null handles.
    pub h_rt_core_layer: *mut core::ffi::c_void,
    pub um_callbacks: *const core::ffi::c_void,
    /// Input-assembler state for lazy `ID3D11InputLayout` creation. The d3d10umddi
    /// `CreateElementLayout` DDI does NOT pass the vertex-shader input-signature
    /// bytecode that `ID3D11Device::CreateInputLayout` requires, so we stash the
    /// element descs + the bound VS bytecode and create the layout lazily at draw.
    pub ia: core::cell::RefCell<IaState>,
    /// Kernel-enforced vehicle flip ordering (forward.rs::flip_wait_setup):
    /// 0 = unprobed, 1 = ready, 2 = disabled (knob off / setup failed —
    /// counted + logged once; the bounded CPU gate serves instead). Same
    /// single-threaded-DDI Cell contract as the rest of the device state.
    pub flip_wait_state: core::cell::Cell<u8>,
    /// Runtime-device monitored fence (D3DKMT_HANDLE) the present context's
    /// queued GPU waits target; CPU-signaled by the bridge when the present
    /// fence reaches the copy's value.
    pub flip_wait_fence: core::cell::Cell<u32>,
    /// Last flip value queued as a GPU wait (monotonic per device).
    pub flip_wait_next_value: core::cell::Cell<u64>,
}

/// Deferred input-assembler binding state (see [`HeliosDevice::ia`]).
#[derive(Default)]
pub struct IaState {
    /// VS COM pointer (as `usize`) → its DXBC input-signature bytecode.
    pub vs_bytecode: std::collections::HashMap<usize, Vec<u8>>,
    /// VS COM pointer → the flattened DDI signature words it was created with
    /// ([n_in, n_out, (sysval, reg, mask, comptype, stream) × …]); used to
    /// recompile input-class variants (see `resolve_vs_input_variant`).
    pub vs_sig_words: std::collections::HashMap<usize, Vec<u32>>,
    /// (VS COM pointer, layout input-class key) → variant VS COM pointer,
    /// recompiled with the layout's per-register numeric classes. Variants
    /// live until device teardown (bounded: shaders × distinct class sets).
    pub vs_variants: std::collections::HashMap<(usize, u64), usize>,
    /// The VS COM pointer most recently handed to DXVK's VSSetShader (may be
    /// a variant; `current_vs` stays the runtime's own binding).
    pub bound_vs_com: usize,
    /// Currently-bound vertex shader's COM pointer.
    pub current_vs: usize,
    /// Currently-bound pixel shader's COM pointer.
    pub current_ps: usize,
    /// Currently-bound geometry shader's COM pointer.
    pub current_gs: usize,
    /// Currently-bound hull shader's COM pointer.
    pub current_hs: usize,
    /// Currently-bound domain shader's COM pointer.
    pub current_ds: usize,
    /// Currently-bound compute shader's COM pointer.
    pub current_cs: usize,
    /// Currently-bound primitive topology and first IA buffer state, for draw
    /// diagnostics on complex D3D11 content.
    pub current_topology: u32,
    pub current_vb0: usize,
    pub current_vb0_stride: u32,
    pub current_vb0_offset: u32,
    pub current_ib: usize,
    pub current_ib_format: u32,
    pub current_ib_offset: u32,
    /// Allocation behind RTV slot 0, for live composition diagnostics.
    pub current_rt0_alloc: u32,
    /// Dimensions/format behind RTV slot 0, for live composition diagnostics.
    pub current_rt0_width: u32,
    pub current_rt0_height: u32,
    pub current_rt0_format: u32,
    /// Currently-bound element layout's `LayoutData` raw pointer (0 = none).
    pub current_layout: usize,
    /// Cache of created input layouts keyed by (layout_ptr, vs_ptr) → owned
    /// `ID3D11InputLayout` raw pointer.
    pub layout_cache: std::collections::HashMap<(usize, usize), usize>,
}

pub fn device_private_size() -> usize {
    core::mem::size_of::<HeliosDevice>()
}

/// Uniform stub signature (one machine word in, one out).
type UniformFn = unsafe extern "C" fn(usize) -> usize;

static DEVICE_NOOP_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static DXGI_NOOP_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static WDDM13_TABLE_AUDIT_COUNT: AtomicUsize = AtomicUsize::new(0);
static DXGI13_TABLE_AUDIT_COUNT: AtomicUsize = AtomicUsize::new(0);

#[link(name = "kernel32")]
extern "system" {
    fn RtlCaptureStackBackTrace(
        frames_to_skip: u32,
        frames_to_capture: u32,
        back_trace: *mut *mut c_void,
        back_trace_hash: *mut u32,
    ) -> u16;
}

unsafe fn log_backtrace(tag: &str) {
    let mut frames = [core::ptr::null_mut::<c_void>(); 32];
    let captured = RtlCaptureStackBackTrace(
        0,
        frames.len() as u32,
        frames.as_mut_ptr(),
        core::ptr::null_mut(),
    );
    let mut out = String::new();
    for i in 0..captured as usize {
        out.push_str(&format!(" #{i}=0x{:x}", frames[i] as usize));
    }
    log_line(&format!("{tag} stack{out}"));
}

/// No-op DDI stub: returns 0 (S_OK for HRESULT funcs; ignored for void funcs).
unsafe extern "C" fn ddi_noop_device(_a: usize) -> usize {
    let n = DEVICE_NOOP_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 512 {
        if n == 0 {
            log_backtrace("DDI noop(device)");
        } else {
            log_line(&format!("DDI noop(device) hit={n}"));
        }
    }
    0
}

/// DXGI base no-op DDI stub. Kept separate so Present-adjacent missing funcs are
/// distinguishable from D3D11 device-func misses.
unsafe extern "C" fn ddi_noop_dxgi(_a: usize) -> usize {
    let n = DXGI_NOOP_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 256 {
        if n == 0 {
            log_backtrace("DDI noop(dxgi)");
        } else {
            log_line(&format!("DDI noop(dxgi) hit={n}"));
        }
    }
    0
}

/// CalcPrivate*Size stub: return a small nonzero, pointer-aligned size so the
/// runtime's driver-private object allocation is valid. Our Create* stubs never
/// write into it and no other stub reads it, so the exact size is immaterial.
unsafe extern "C" fn ddi_calc_size(_a: usize) -> usize {
    256
}

unsafe extern "C" fn ddi_relocate_device_funcs(
    _h_device: ddi::D3D10DDI_HDEVICE,
    funcs: *mut ddi::D3D11DDI_DEVICEFUNCS,
) {
    log_line("DDI RelocateDeviceFuncs(D3D11)");
    if !funcs.is_null() {
        fill_d3d11_device_funcs(funcs);
    }
}

unsafe extern "C" fn ddi_relocate_device_funcs_11_1(
    _h_device: ddi::D3D10DDI_HDEVICE,
    funcs: *mut ddi::D3D11_1DDI_DEVICEFUNCS,
) {
    log_line("DDI RelocateDeviceFuncs(D3D11.1)");
    if !funcs.is_null() {
        fill_d3d11_1_device_funcs(funcs);
    }
}

unsafe extern "C" fn ddi_relocate_device_funcs_wddm1_3(
    _h_device: ddi::D3D10DDI_HDEVICE,
    funcs: *mut ddi::D3DWDDM1_3DDI_DEVICEFUNCS,
) {
    log_line("DDI RelocateDeviceFuncs(WDDM1.3)");
    if !funcs.is_null() {
        fill_wddm1_3_device_funcs(funcs);
    }
}

unsafe extern "C" fn ddi_relocate_device_funcs_wddm2_1(
    _h_device: ddi::D3D10DDI_HDEVICE,
    funcs: *mut ddi::D3DWDDM2_1DDI_DEVICEFUNCS,
) {
    log_line("DDI RelocateDeviceFuncs(WDDM2.1)");
    if !funcs.is_null() {
        fill_wddm2_1_device_funcs(funcs);
    }
}

unsafe fn audit_wddm1_3_device_funcs(tag: &str, funcs: *mut ddi::D3DWDDM1_3DDI_DEVICEFUNCS) {
    let hit = WDDM13_TABLE_AUDIT_COUNT.fetch_add(1, Ordering::Relaxed);
    if hit >= 32 {
        return;
    }

    let n = core::mem::size_of::<ddi::D3DWDDM1_3DDI_DEVICEFUNCS>() / core::mem::size_of::<usize>();
    let slots = funcs as *const usize;
    log_line(&format!(
        "{tag}: WDDM1.3 funcs table={funcs:p} slots={n} audit={}",
        hit + 1
    ));

    const EXT_NAMES: [&str; 9] = [
        "UpdateTileMappings",
        "CopyTileMappings",
        "CopyTiles",
        "UpdateTiles",
        "TiledResourceBarrier",
        "GetMipPacking",
        "ResizeTilePool",
        "SetMarker",
        "SetMarkerMode",
    ];
    for (offset, name) in EXT_NAMES.iter().enumerate() {
        let index = 155 + offset;
        if index < n {
            log_line(&format!(
                "{tag}: WDDM1.3 slot[{index:03}] {name}=0x{:016x}",
                *slots.add(index)
            ));
        }
    }

    let mut bad = 0usize;
    for i in 0..n {
        let value = *slots.add(i);
        if value == 0 || value < 0x0000_0001_0000_0000 {
            bad += 1;
            if bad <= 16 {
                log_line(&format!(
                    "{tag}: WDDM1.3 suspicious slot[{i:03}]=0x{value:016x}"
                ));
            }
        }
    }
    if bad != 0 {
        log_line(&format!("{tag}: WDDM1.3 suspicious slot count={bad}"));
    }
}

unsafe fn audit_dxgi_1_3_base_funcs(tag: &str, funcs: *mut ddi::DXGI1_3_DDI_BASE_FUNCTIONS) {
    let hit = DXGI13_TABLE_AUDIT_COUNT.fetch_add(1, Ordering::Relaxed);
    if hit >= 32 {
        return;
    }

    let n = core::mem::size_of::<ddi::DXGI1_3_DDI_BASE_FUNCTIONS>() / core::mem::size_of::<usize>();
    let slots = funcs as *const usize;
    log_line(&format!(
        "{tag}: DXGI1.3 funcs table={funcs:p} slots={n} audit={}",
        hit + 1
    ));

    const NAMES: [&str; 18] = [
        "Present",
        "GetGammaCaps",
        "SetDisplayMode",
        "SetResourcePriority",
        "QueryResourceResidency",
        "RotateResourceIdentities",
        "Blt",
        "ResolveSharedResource",
        "Blt1",
        "OfferResources",
        "ReclaimResources",
        "GetMultiplaneOverlayCaps",
        "GetMultiplaneOverlayGroupCaps",
        "Reserved1",
        "PresentMultiplaneOverlay",
        "Reserved2",
        "Present1",
        "CheckPresentDurationSupport",
    ];
    for (i, name) in NAMES.iter().enumerate() {
        if i < n {
            log_line(&format!(
                "{tag}: DXGI1.3 slot[{i:02}] {name}=0x{:016x}",
                *slots.add(i)
            ));
        }
    }

    let mut bad = 0usize;
    for i in 0..n {
        let value = *slots.add(i);
        if value == 0 || value < 0x0000_0001_0000_0000 {
            bad += 1;
            if bad <= 16 {
                log_line(&format!(
                    "{tag}: DXGI1.3 suspicious slot[{i:02}]=0x{value:016x}"
                ));
            }
        }
    }
    if bad != 0 {
        log_line(&format!("{tag}: DXGI1.3 suspicious slot count={bad}"));
    }
}

/// Real DestroyDevice: drop the in-place HeliosDevice (releasing the DXVK device).
/// The runtime owns the backing memory, so we only run the destructor.
unsafe extern "C" fn ddi_destroy_device(h_device: ddi::D3D10DDI_HDEVICE) {
    log_line("DDI: DestroyDevice");
    if !h_device.pDrvPrivate.is_null() {
        let dev = &mut *(h_device.pDrvPrivate as *mut HeliosDevice);
        if !dev.h_context.is_null() && !dev.kt_callbacks.is_null() {
            if let Some(destroy_context_cb) = (*dev.kt_callbacks).pfnDestroyContextCb {
                let arg = ddi::D3DDDICB_DESTROYCONTEXT {
                    hContext: dev.h_context,
                };
                let hr = destroy_context_cb(dev.h_rt_device, &arg);
                log_line(&format!(
                    "DDI DestroyDevice: DestroyContext hContext={:p} hr=0x{:08x}",
                    dev.h_context, hr as u32
                ));
            }
            dev.h_context = core::ptr::null_mut();
        }
        core::ptr::drop_in_place(h_device.pDrvPrivate as *mut HeliosDevice);
    }
}

pub unsafe fn create_runtime_context(dev: &mut HeliosDevice) {
    if dev.kt_callbacks.is_null() {
        log_line("CreateDevice: no KT callbacks for CreateContext");
        return;
    }
    let Some(create_context_cb) = (*dev.kt_callbacks).pfnCreateContextCb else {
        log_line("CreateDevice: pfnCreateContextCb missing");
        return;
    };

    let mut arg = ddi::D3DDDICB_CREATECONTEXT::default();
    arg.NodeOrdinal = 0;
    arg.EngineAffinity = 0;
    let hr = create_context_cb(dev.h_rt_device, &mut arg);
    log_line(&format!(
        "CreateDevice: CreateContext hr=0x{:08x} hContext={:p} cmd={:p}/{} allocList={:p}/{} patchList={:p}/{}",
        hr as u32,
        arg.hContext,
        arg.pCommandBuffer,
        arg.CommandBufferSize,
        arg.pAllocationList,
        arg.AllocationListSize,
        arg.pPatchLocationList,
        arg.PatchLocationListSize
    ));
    if hr == 0 {
        dev.h_context = arg.hContext;
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
        *slots.add(i) = Some(ddi_noop_device);
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
    f.pfnRelocateDeviceFuncs = Some(ddi_relocate_device_funcs);

    // Override stubs with the real D3D11 COM forwarders.
    crate::forward::install(funcs);
}

/// Fill a D3D11.1 device-funcs table. The D3D11.1 layout is an extension of the
/// D3D11.0 prefix, so the implemented forwarders can be installed through the
/// D3D11.0 view after the whole larger table has been stub-filled.
pub unsafe fn fill_d3d11_1_device_funcs(funcs: *mut ddi::D3D11_1DDI_DEVICEFUNCS) {
    let n = core::mem::size_of::<ddi::D3D11_1DDI_DEVICEFUNCS>() / core::mem::size_of::<usize>();
    let slots = funcs as *mut Option<UniformFn>;
    for i in 0..n {
        *slots.add(i) = Some(ddi_noop_device);
    }

    let f = &mut *(funcs as *mut ddi::D3D11DDI_DEVICEFUNCS);

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
        pfnCheckDeferredContextHandleSizes,
        pfnCalcDeferredContextHandleSize,
        pfnCalcPrivateDeferredContextSize,
        pfnCalcPrivateCommandListSize,
        pfnCalcPrivateTessellationShaderSize,
        pfnCalcPrivateUnorderedAccessViewSize,
    );

    f.pfnDestroyDevice = Some(ddi_destroy_device);
    (*funcs).pfnRelocateDeviceFuncs = Some(ddi_relocate_device_funcs_11_1);
    crate::forward::install(f);
    crate::forward::install_11_1(funcs);
}

pub unsafe fn fill_wddm1_3_device_funcs(funcs: *mut ddi::D3DWDDM1_3DDI_DEVICEFUNCS) {
    let n = core::mem::size_of::<ddi::D3DWDDM1_3DDI_DEVICEFUNCS>() / core::mem::size_of::<usize>();
    let slots = funcs as *mut Option<UniformFn>;
    for i in 0..n {
        *slots.add(i) = Some(ddi_noop_device);
    }

    let f = &mut *(funcs as *mut ddi::D3D11DDI_DEVICEFUNCS);

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
        pfnCheckDeferredContextHandleSizes,
        pfnCalcDeferredContextHandleSize,
        pfnCalcPrivateDeferredContextSize,
        pfnCalcPrivateCommandListSize,
        pfnCalcPrivateTessellationShaderSize,
        pfnCalcPrivateUnorderedAccessViewSize,
    );

    f.pfnDestroyDevice = Some(ddi_destroy_device);
    (*funcs).pfnRelocateDeviceFuncs = Some(ddi_relocate_device_funcs_wddm1_3);
    crate::forward::install(f);
    crate::forward::install_11_1(funcs as *mut ddi::D3D11_1DDI_DEVICEFUNCS);
    crate::forward::install_wddm1_3(funcs);
    audit_wddm1_3_device_funcs("FillDeviceFuncs", funcs);
}

pub unsafe fn fill_wddm2_1_device_funcs(funcs: *mut ddi::D3DWDDM2_1DDI_DEVICEFUNCS) {
    let n = core::mem::size_of::<ddi::D3DWDDM2_1DDI_DEVICEFUNCS>() / core::mem::size_of::<usize>();
    let slots = funcs as *mut Option<UniformFn>;
    for i in 0..n {
        *slots.add(i) = Some(ddi_noop_device);
    }

    let f = &mut *(funcs as *mut ddi::D3D11DDI_DEVICEFUNCS);

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
        pfnCheckDeferredContextHandleSizes,
        pfnCalcDeferredContextHandleSize,
        pfnCalcPrivateDeferredContextSize,
        pfnCalcPrivateCommandListSize,
        pfnCalcPrivateTessellationShaderSize,
        pfnCalcPrivateUnorderedAccessViewSize,
    );

    f.pfnDestroyDevice = Some(ddi_destroy_device);
    (*funcs).pfnRelocateDeviceFuncs = Some(ddi_relocate_device_funcs_wddm2_1);
    crate::forward::install(f);
    crate::forward::install_11_1(funcs as *mut ddi::D3D11_1DDI_DEVICEFUNCS);
    crate::forward::install_wddm1_3(funcs as *mut ddi::D3DWDDM1_3DDI_DEVICEFUNCS);
    crate::forward::install_wddm2_1(funcs);
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
        *slots.add(i) = Some(ddi_noop_dxgi);
    }
    // Real (benign) present so LogonUI/DWM don't fail-fast on present.
    crate::forward::install_dxgi(funcs);
}

/// Fill the DXGI 1.1 base table. This is required for D3D11.1 device creation
/// because the table adds pfnResolveSharedResource after the D3D10-era prefix.
pub unsafe fn fill_dxgi_1_1_base_funcs(funcs: *mut ddi::DXGI1_1_DDI_BASE_FUNCTIONS) {
    if funcs.is_null() {
        return;
    }
    let n = core::mem::size_of::<ddi::DXGI1_1_DDI_BASE_FUNCTIONS>() / core::mem::size_of::<usize>();
    let slots = funcs as *mut Option<UniformFn>;
    for i in 0..n {
        *slots.add(i) = Some(ddi_noop_dxgi);
    }
    crate::forward::install_dxgi(funcs as *mut ddi::DXGI_DDI_BASE_FUNCTIONS);
    crate::forward::install_dxgi_1_1(funcs);
}

/// Fill the DXGI 1.3 base table required by WDDM1.3 devices. DWM can call the
/// later Present1/MPO/residency slots immediately after CreateDevice; handing it
/// only the DXGI 1.1 prefix leaves uninitialized callback pointers past slot 7.
pub unsafe fn fill_dxgi_1_3_base_funcs(funcs: *mut ddi::DXGI1_3_DDI_BASE_FUNCTIONS) {
    if funcs.is_null() {
        return;
    }
    let n = core::mem::size_of::<ddi::DXGI1_3_DDI_BASE_FUNCTIONS>() / core::mem::size_of::<usize>();
    let slots = funcs as *mut Option<UniformFn>;
    for i in 0..n {
        *slots.add(i) = Some(ddi_noop_dxgi);
    }
    crate::forward::install_dxgi(funcs as *mut ddi::DXGI_DDI_BASE_FUNCTIONS);
    crate::forward::install_dxgi_1_1(funcs as *mut ddi::DXGI1_1_DDI_BASE_FUNCTIONS);
    crate::forward::install_dxgi_1_3(funcs);
    audit_dxgi_1_3_base_funcs("FillDXGIBaseFuncs", funcs);
}
