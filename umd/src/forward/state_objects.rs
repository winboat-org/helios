//! The immutable pipeline state objects: rasterizer, depth-stencil and blend.
//!
//! Moved verbatim out of `forward.rs` by T8/R1107.

use super::*;

// --- Rasterizer / depth-stencil state ---------------------------------------

pub(crate) unsafe extern "C" fn calc_size_raster(
    _h: Hdevice,
    _d: *const ddi::D3D10_DDI_RASTERIZER_DESC,
) -> u64 {
    8
}
pub(crate) unsafe extern "C" fn calc_size_depth(
    _h: Hdevice,
    _d: *const ddi::D3D10_DDI_DEPTH_STENCIL_DESC,
) -> u64 {
    8
}

pub(crate) unsafe extern "C" fn create_rasterizer_state(
    h: Hdevice,
    desc: *const ddi::D3D10_DDI_RASTERIZER_DESC,
    h_rs: ddi::D3D10DDI_HRASTERIZERSTATE,
    _hrt: ddi::D3D10DDI_HRTRASTERIZERSTATE,
) {
    clear_handle(h_rs);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let d = &*desc;
    let rd = D3D11_RASTERIZER_DESC {
        FillMode: D3D11_FILL_MODE(d.FillMode),
        CullMode: D3D11_CULL_MODE(d.CullMode),
        FrontCounterClockwise: windows::Win32::Foundation::BOOL(d.FrontCounterClockwise),
        DepthBias: d.DepthBias,
        DepthBiasClamp: d.DepthBiasClamp,
        SlopeScaledDepthBias: d.SlopeScaledDepthBias,
        DepthClipEnable: windows::Win32::Foundation::BOOL(d.DepthClipEnable),
        ScissorEnable: windows::Win32::Foundation::BOOL(d.ScissorEnable),
        MultisampleEnable: windows::Win32::Foundation::BOOL(d.MultisampleEnable),
        AntialiasedLineEnable: windows::Win32::Foundation::BOOL(d.AntialiasedLineEnable),
    };
    if RASTER_LOG_COUNT.first_n(64).is_some() {
        log_error!(
            "DDI CreateRasterizerState fill={} cull={} front_ccw={} depth_clip={} scissor={} msaa={} aaline={} bias={} slope_bias={:.3}",
            d.FillMode,
            d.CullMode,
            d.FrontCounterClockwise,
            d.DepthClipEnable,
            d.ScissorEnable,
            d.MultisampleEnable,
            d.AntialiasedLineEnable,
            d.DepthBias,
            d.SlopeScaledDepthBias
        );
    }
    let mut rs: Option<ID3D11RasterizerState> = None;
    let created = device.CreateRasterizerState(&rd, Some(&mut rs));
    if let Err(ref e) = created {
        log_error!("DDI CreateRasterizerState failed: {e:?}");
    }
    finish_create(h, created, rs, |s| store_com(h_rs, s));
}

pub(crate) unsafe extern "C" fn set_rasterizer_state(
    h: Hdevice,
    h_rs: ddi::D3D10DDI_HRASTERIZERSTATE,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    if RASTER_LOG_COUNT.first_n(128).is_some() {
        trace_line!("DDI RSSetState raw=0x{:x}", handle_com_raw(h_rs));
    }
    match load_com::<ID3D11RasterizerState>(h_rs) {
        Some(s) => context.RSSetState(&*s),
        None => context.RSSetState(None),
    }
}

pub(crate) unsafe fn cvt_stencilop(
    d: &ddi::D3D10_DDI_DEPTH_STENCILOP_DESC,
) -> D3D11_DEPTH_STENCILOP_DESC {
    D3D11_DEPTH_STENCILOP_DESC {
        StencilFailOp: D3D11_STENCIL_OP(d.StencilFailOp),
        StencilDepthFailOp: D3D11_STENCIL_OP(d.StencilDepthFailOp),
        StencilPassOp: D3D11_STENCIL_OP(d.StencilPassOp),
        StencilFunc: D3D11_COMPARISON_FUNC(d.StencilFunc),
    }
}

pub(crate) unsafe extern "C" fn create_depth_stencil_state(
    h: Hdevice,
    desc: *const ddi::D3D10_DDI_DEPTH_STENCIL_DESC,
    h_ds: ddi::D3D10DDI_HDEPTHSTENCILSTATE,
    _hrt: ddi::D3D10DDI_HRTDEPTHSTENCILSTATE,
) {
    clear_handle(h_ds);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let d = &*desc;
    let dd = D3D11_DEPTH_STENCIL_DESC {
        DepthEnable: windows::Win32::Foundation::BOOL(d.DepthEnable),
        DepthWriteMask: D3D11_DEPTH_WRITE_MASK(d.DepthWriteMask),
        DepthFunc: D3D11_COMPARISON_FUNC(d.DepthFunc),
        StencilEnable: windows::Win32::Foundation::BOOL(d.StencilEnable),
        StencilReadMask: d.StencilReadMask,
        StencilWriteMask: d.StencilWriteMask,
        FrontFace: cvt_stencilop(&d.FrontFace),
        BackFace: cvt_stencilop(&d.BackFace),
    };
    let mut ds: Option<ID3D11DepthStencilState> = None;
    let created = device.CreateDepthStencilState(&dd, Some(&mut ds));
    if let Err(ref e) = created {
        log_error!("DDI CreateDepthStencilState failed: {e:?}");
    }
    finish_create(h, created, ds, |s| store_com(h_ds, s));
}

pub(crate) unsafe extern "C" fn set_depth_stencil_state(
    h: Hdevice,
    h_ds: ddi::D3D10DDI_HDEPTHSTENCILSTATE,
    stencil_ref: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11DepthStencilState>(h_ds) {
        Some(s) => context.OMSetDepthStencilState(&*s, stencil_ref),
        None => context.OMSetDepthStencilState(None, stencil_ref),
    }
}

pub(crate) unsafe extern "C" fn destroy_raster_state(
    _h: Hdevice,
    h_state: ddi::D3D10DDI_HRASTERIZERSTATE,
) {
    release_com(h_state);
}

pub(crate) unsafe extern "C" fn destroy_depth_state(
    _h: Hdevice,
    h_state: ddi::D3D10DDI_HDEPTHSTENCILSTATE,
) {
    release_com(h_state);
}

// --- Input layouts (lazy, via the VS input signature) -----------------------

// --- Blend state ------------------------------------------------------------

pub(crate) unsafe extern "C" fn calc_size_blend(
    _h: Hdevice,
    _d: *const ddi::D3D10_1_DDI_BLEND_DESC,
) -> u64 {
    8
}

pub(crate) unsafe extern "C" fn create_blend_state(
    h: Hdevice,
    desc: *const ddi::D3D10_1_DDI_BLEND_DESC,
    h_bs: ddi::D3D10DDI_HBLENDSTATE,
    _hrt: ddi::D3D10DDI_HRTBLENDSTATE,
) {
    clear_handle(h_bs);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let d = &*desc;
    let mut rt: [D3D11_RENDER_TARGET_BLEND_DESC; 8] = Default::default();
    for i in 0..8 {
        let s = &d.RenderTarget[i];
        rt[i] = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: windows::Win32::Foundation::BOOL(s.BlendEnable),
            SrcBlend: D3D11_BLEND(s.SrcBlend),
            DestBlend: D3D11_BLEND(s.DestBlend),
            BlendOp: D3D11_BLEND_OP(s.BlendOp),
            SrcBlendAlpha: D3D11_BLEND(s.SrcBlendAlpha),
            DestBlendAlpha: D3D11_BLEND(s.DestBlendAlpha),
            BlendOpAlpha: D3D11_BLEND_OP(s.BlendOpAlpha),
            RenderTargetWriteMask: s.RenderTargetWriteMask,
        };
    }
    let bd = D3D11_BLEND_DESC {
        AlphaToCoverageEnable: windows::Win32::Foundation::BOOL(d.AlphaToCoverageEnable),
        IndependentBlendEnable: windows::Win32::Foundation::BOOL(d.IndependentBlendEnable),
        RenderTarget: rt,
    };
    let mut bs: Option<ID3D11BlendState> = None;
    let created = device.CreateBlendState(&bd, Some(&mut bs));
    if let Err(ref e) = created {
        log_error!("DDI CreateBlendState failed: {e:?}");
    }
    finish_create(h, created, bs, |s| store_com(h_bs, s));
}

/// D3D11.1-interface `pfnCalcPrivateBlendStateSize` (same 8-byte COM-pointer
/// slot as `calc_size_blend`; only the desc type differs).
pub(crate) unsafe extern "C" fn calc_size_blend_11_1(
    _h: Hdevice,
    _d: *const ddi::D3D11_1_DDI_BLEND_DESC,
) -> u64 {
    8
}

/// D3D11.1-interface `pfnCreateBlendState`. Every device-funcs table from
/// D3D11.1 up passes `D3D11_1_DDI_BLEND_DESC`, whose per-RT struct INSERTS
/// `LogicOpEnable` after `BlendEnable` and `LogicOp` before
/// `RenderTargetWriteMask` — it is NOT prefix-compatible with the 10.1 desc.
/// Installing the 10.1 reader here made the mask field land on the 11.1
/// desc's `BlendOpAlpha` (default D3D11_BLEND_OP_ADD = 1), so every blend
/// state — including the runtime's defaults — carried RenderTargetWriteMask =
/// RED-only and every draw on the stack wrote just the R channel (the
/// black/red-tinted composition class; minimal repro
/// tools/d3d11_shared_draw_probe.cpp, 2026-07-03).
pub(crate) unsafe extern "C" fn create_blend_state_11_1(
    h: Hdevice,
    desc: *const ddi::D3D11_1_DDI_BLEND_DESC,
    h_bs: ddi::D3D10DDI_HBLENDSTATE,
    _hrt: ddi::D3D10DDI_HRTBLENDSTATE,
) {
    clear_handle(h_bs);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let Ok(device1) = device.cast::<ID3D11Device1>() else {
        log_error!("DDI create_blend_state_11_1: ID3D11Device1 cast failed");
        return;
    };
    let d = &*desc;
    let mut rt: [D3D11_RENDER_TARGET_BLEND_DESC1; 8] = Default::default();
    for i in 0..8 {
        let s = &d.RenderTarget[i];
        rt[i] = D3D11_RENDER_TARGET_BLEND_DESC1 {
            BlendEnable: windows::Win32::Foundation::BOOL(s.BlendEnable),
            LogicOpEnable: windows::Win32::Foundation::BOOL(s.LogicOpEnable),
            SrcBlend: D3D11_BLEND(s.SrcBlend),
            DestBlend: D3D11_BLEND(s.DestBlend),
            BlendOp: D3D11_BLEND_OP(s.BlendOp),
            SrcBlendAlpha: D3D11_BLEND(s.SrcBlendAlpha),
            DestBlendAlpha: D3D11_BLEND(s.DestBlendAlpha),
            BlendOpAlpha: D3D11_BLEND_OP(s.BlendOpAlpha),
            // The API logic-op enum mirrors the DDI enum value-for-value.
            LogicOp: D3D11_LOGIC_OP(s.LogicOp),
            RenderTargetWriteMask: s.RenderTargetWriteMask,
        };
    }
    let bd = D3D11_BLEND_DESC1 {
        AlphaToCoverageEnable: windows::Win32::Foundation::BOOL(d.AlphaToCoverageEnable),
        IndependentBlendEnable: windows::Win32::Foundation::BOOL(d.IndependentBlendEnable),
        RenderTarget: rt,
    };
    let mut bs: Option<ID3D11BlendState1> = None;
    let created = device1.CreateBlendState1(&bd, Some(&mut bs));
    if created.is_err() {
        log_error!("DDI create_blend_state_11_1: CreateBlendState1 failed");
    }
    // `set_blend_state` loads an ID3D11BlendState from this slot; store the
    // base interface. A failed QI is a create failure like any other — folding
    // it into `None` routes it through the same report instead of dropping it.
    let base = match bs {
        Some(s) => match s.cast::<ID3D11BlendState>() {
            Ok(b) => Some(b),
            Err(e) => {
                log_error!("DDI create_blend_state_11_1: ID3D11BlendState cast failed: {e:?}");
                None
            }
        },
        None => None,
    };
    finish_create(h, created, base, |b| store_com(h_bs, b));
}

pub(crate) unsafe extern "C" fn set_blend_state(
    h: Hdevice,
    h_bs: ddi::D3D10DDI_HBLENDSTATE,
    factor: *const f32,
    sample_mask: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let f = if factor.is_null() {
        [1.0f32; 4]
    } else {
        [*factor, *factor.add(1), *factor.add(2), *factor.add(3)]
    };
    match load_com::<ID3D11BlendState>(h_bs) {
        Some(s) => context.OMSetBlendState(&*s, Some(&f), sample_mask),
        None => context.OMSetBlendState(None, Some(&f), sample_mask),
    }
}

pub(crate) unsafe extern "C" fn destroy_blend_state(_h: Hdevice, h_bs: ddi::D3D10DDI_HBLENDSTATE) {
    release_com(h_bs);
}

// --- Dcomp present vehicle (road 4 unit 2) ----------------------------------
//
// The ICD (mesa WSI) presents a Vulkan frame through a D3D11 composition
// swapchain it owns (the "vehicle"): it publishes its frame's
// (resid -> pid, fenceId, value) in the WS1 #4 seqlock table, hands
// (resid, value, geometry, allocation identity) to
// `helios_umd_set_present_source` — stored per-THREAD below — and calls
// Present() on the vehicle ON THE SAME THREAD. The next dxgi_present on
// that thread consumes the slot: instead of the normal src->dst copy it
// alias-imports the ICD frame by resid (cached per resid), image-copies it
// into hSurfaceToPresent's DXVK texture (the copy-time consumer wait orders
// the copy against the ICD's GPU writes via the published slot), publishes
// the BACKBUFFER slot with THIS device's own fence (correct: the vehicle
// wrote the backbuffer; dwm's consumer wait needs zero changes), then mints
// the token via pfnPresentCb exactly as any flip-model present.
