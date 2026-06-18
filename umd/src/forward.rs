//! d3d10umddi device-funcs → D3D11 COM forwarders (pure Rust via the windows
//! crate). Each func reads its bindgen DDI arg struct, translates to a
//! windows-crate COM call on the DXVK `ID3D11Device`/`ID3D11DeviceContext`, and
//! stores the returned COM interface in the runtime-allocated DDI handle.
//!
//! DDI Usage/BindFlags/MiscFlags mirror the D3D11 API bit values (passthrough).
//! Resource/view handles store the raw COM pointer (8 bytes) in pDrvPrivate;
//! CalcPrivate*Size returns 8. Errors on VOID-returning Create* are dropped for
//! now (TODO: report via the device error callback) — a failed create leaves a
//! null handle.

use core::ffi::c_void;
use core::mem::ManuallyDrop;

use windows::core::{IUnknown, Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

use crate::ddi;
use crate::device_funcs::HeliosDevice;
use crate::log_line;

type Hdevice = ddi::D3D10DDI_HDEVICE;

// --- handle <-> COM helpers -------------------------------------------------

unsafe fn d3d11_device(h: Hdevice) -> Option<ManuallyDrop<ID3D11Device>> {
    let hd = h.pDrvPrivate as *const HeliosDevice;
    if hd.is_null() {
        return None;
    }
    let p = (*hd).dxvk.d3d11_device_ptr();
    if p == 0 {
        return None;
    }
    Some(ManuallyDrop::new(ID3D11Device::from_raw(p as *mut c_void)))
}

unsafe fn d3d11_context(h: Hdevice) -> Option<ManuallyDrop<ID3D11DeviceContext>> {
    let hd = h.pDrvPrivate as *const HeliosDevice;
    if hd.is_null() {
        return None;
    }
    let p = (*hd).dxvk.d3d11_context_ptr();
    if p == 0 {
        return None;
    }
    Some(ManuallyDrop::new(ID3D11DeviceContext::from_raw(p as *mut c_void)))
}

/// Store a COM interface's raw pointer (ownership transferred) in a DDI handle.
unsafe fn store_com<T: Interface>(handle_priv: *mut c_void, obj: T) {
    *(handle_priv as *mut *mut c_void) = obj.into_raw();
}

/// Borrow the COM interface stored in a DDI handle (does not take ownership).
unsafe fn load_com<T: Interface>(handle_priv: *mut c_void) -> Option<ManuallyDrop<T>> {
    if handle_priv.is_null() {
        return None;
    }
    let raw = *(handle_priv as *const *mut c_void);
    if raw.is_null() {
        return None;
    }
    Some(ManuallyDrop::new(T::from_raw(raw)))
}

/// Release the COM interface stored in a DDI handle.
unsafe fn release_com(handle_priv: *mut c_void) {
    if handle_priv.is_null() {
        return;
    }
    let raw = *(handle_priv as *const *mut c_void);
    if !raw.is_null() {
        // Reconstitute owning ref and drop it.
        drop(IUnknown::from_raw(raw));
        *(handle_priv as *mut *mut c_void) = core::ptr::null_mut();
    }
}

fn cpu_access(usage: u32) -> u32 {
    // D3D11_USAGE: DEFAULT=0, IMMUTABLE=1, DYNAMIC=2, STAGING=3.
    match usage {
        2 => D3D11_CPU_ACCESS_WRITE.0 as u32,
        3 => (D3D11_CPU_ACCESS_WRITE.0 | D3D11_CPU_ACCESS_READ.0) as u32,
        _ => 0,
    }
}

// --- CalcPrivate*Size (all store one COM pointer) ---------------------------

unsafe extern "C" fn calc_size_resource(
    _h: Hdevice,
    _a: *const ddi::D3D11DDIARG_CREATERESOURCE,
) -> u64 {
    8
}
unsafe extern "C" fn calc_size_rtv(
    _h: Hdevice,
    _a: *const ddi::D3D10DDIARG_CREATERENDERTARGETVIEW,
) -> u64 {
    8
}

// --- Resources --------------------------------------------------------------

const RES_BUFFER: ddi::D3D10DDIRESOURCE_TYPE =
    ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_BUFFER;
const RES_BUFFEREX: ddi::D3D10DDIRESOURCE_TYPE =
    ddi::D3D10DDIRESOURCE_TYPE_D3D11DDIRESOURCE_BUFFEREX;
const RES_TEX2D: ddi::D3D10DDIRESOURCE_TYPE =
    ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_TEXTURE2D;
const RES_TEXCUBE: ddi::D3D10DDIRESOURCE_TYPE =
    ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_TEXTURECUBE;

unsafe extern "C" fn create_resource(
    h: Hdevice,
    arg: *const ddi::D3D11DDIARG_CREATERESOURCE,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    _h_rt: ddi::D3D10DDI_HRTRESOURCE,
) {
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let mip0 = if a.pMipInfoList.is_null() {
        ddi::D3D10DDI_MIPINFO {
            TexelWidth: 0,
            TexelHeight: 0,
            TexelDepth: 0,
            PhysicalWidth: 0,
            PhysicalHeight: 0,
            PhysicalDepth: 0,
        }
    } else {
        *a.pMipInfoList
    };

    // Build initial-data array if provided (one entry per subresource).
    let num_sub = (a.MipLevels.max(1) * a.ArraySize.max(1)) as usize;
    let mut init: Vec<D3D11_SUBRESOURCE_DATA> = Vec::new();
    if !a.pInitialDataUP.is_null() {
        for i in 0..num_sub {
            let up = &*a.pInitialDataUP.add(i);
            init.push(D3D11_SUBRESOURCE_DATA {
                pSysMem: up.pSysMem,
                SysMemPitch: up.SysMemPitch,
                SysMemSlicePitch: up.SysMemSlicePitch,
            });
        }
    }
    let init_ptr = if init.is_empty() {
        None
    } else {
        Some(init.as_ptr())
    };

    let cpu = cpu_access(a.Usage);

    match a.ResourceDimension {
        RES_BUFFER | RES_BUFFEREX => {
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: mip0.TexelWidth,
                Usage: D3D11_USAGE(a.Usage as i32),
                BindFlags: a.BindFlags,
                CPUAccessFlags: cpu,
                MiscFlags: a.MiscFlags,
                StructureByteStride: a.ByteStride,
            };
            let mut buf: Option<ID3D11Buffer> = None;
            match device.CreateBuffer(&desc, init_ptr, Some(&mut buf)) {
                Ok(()) => {
                    if let Some(b) = buf {
                        if let Ok(res) = b.cast::<ID3D11Resource>() {
                            store_com(h_resource.pDrvPrivate, res);
                        }
                    }
                }
                Err(e) => log_line(&format!("DDI create_resource(buffer) failed: {e:?}")),
            }
        }
        RES_TEX2D | RES_TEXCUBE => {
            let mut misc = a.MiscFlags;
            if a.ResourceDimension == RES_TEXCUBE {
                misc |= D3D11_RESOURCE_MISC_TEXTURECUBE.0 as u32;
            }
            let desc = D3D11_TEXTURE2D_DESC {
                Width: mip0.TexelWidth,
                Height: mip0.TexelHeight,
                MipLevels: a.MipLevels,
                ArraySize: a.ArraySize.max(1),
                Format: DXGI_FORMAT(a.Format as i32),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: a.SampleDesc.Count,
                    Quality: a.SampleDesc.Quality,
                },
                Usage: D3D11_USAGE(a.Usage as i32),
                BindFlags: a.BindFlags,
                CPUAccessFlags: cpu,
                MiscFlags: misc,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            match device.CreateTexture2D(&desc, init_ptr, Some(&mut tex)) {
                Ok(()) => {
                    if let Some(t) = tex {
                        if let Ok(res) = t.cast::<ID3D11Resource>() {
                            store_com(h_resource.pDrvPrivate, res);
                        }
                    }
                }
                Err(e) => log_line(&format!("DDI create_resource(tex2d) failed: {e:?}")),
            }
        }
        other => log_line(&format!("DDI create_resource: unhandled dimension {other}")),
    }
}

unsafe extern "C" fn destroy_resource(_h: Hdevice, h_resource: ddi::D3D10DDI_HRESOURCE) {
    release_com(h_resource.pDrvPrivate);
}

// --- Render target views ----------------------------------------------------

unsafe extern "C" fn create_rtv(
    h: Hdevice,
    arg: *const ddi::D3D10DDIARG_CREATERENDERTARGETVIEW,
    h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW,
    _h_rt: ddi::D3D10DDI_HRTRENDERTARGETVIEW,
) {
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let Some(res) = load_com::<ID3D11Resource>(a.hDrvResource.pDrvPrivate) else {
        log_line("DDI create_rtv: resource handle empty");
        return;
    };
    // Let the runtime/driver pick the view desc from the resource by passing the
    // format via a minimal desc (None = whole resource at mip 0).
    let mut rtv: Option<ID3D11RenderTargetView> = None;
    match device.CreateRenderTargetView(&*res, None, Some(&mut rtv)) {
        Ok(()) => {
            if let Some(v) = rtv {
                store_com(h_rtv.pDrvPrivate, v);
            }
        }
        Err(e) => log_line(&format!("DDI create_rtv failed: {e:?}")),
    }
}

unsafe extern "C" fn destroy_rtv(_h: Hdevice, h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW) {
    release_com(h_rtv.pDrvPrivate);
}

unsafe extern "C" fn clear_rtv(
    h: Hdevice,
    h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW,
    color: *mut f32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(rtv) = load_com::<ID3D11RenderTargetView>(h_rtv.pDrvPrivate) else {
        return;
    };
    let rgba: [f32; 4] = if color.is_null() {
        [0.0; 4]
    } else {
        [*color, *color.add(1), *color.add(2), *color.add(3)]
    };
    context.ClearRenderTargetView(&*rtv, &rgba);
}

// --- Copy / Map / Flush -----------------------------------------------------

unsafe extern "C" fn resource_copy(
    h: Hdevice,
    h_dst: ddi::D3D10DDI_HRESOURCE,
    h_src: ddi::D3D10DDI_HRESOURCE,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let (Some(dst), Some(src)) = (
        load_com::<ID3D11Resource>(h_dst.pDrvPrivate),
        load_com::<ID3D11Resource>(h_src.pDrvPrivate),
    ) else {
        return;
    };
    context.CopyResource(&*dst, &*src);
}

unsafe extern "C" fn resource_map(
    h: Hdevice,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    subresource: u32,
    map_type: ddi::D3D10_DDI_MAP,
    _map_flags: u32,
    mapped: *mut ddi::D3D10DDI_MAPPED_SUBRESOURCE,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(res) = load_com::<ID3D11Resource>(h_resource.pDrvPrivate) else {
        return;
    };
    let mut out = D3D11_MAPPED_SUBRESOURCE::default();
    // DDI D3D10_DDI_MAP values match D3D11_MAP (READ=1, WRITE=2, ...).
    match context.Map(
        &*res,
        subresource,
        D3D11_MAP(map_type as i32),
        0,
        Some(&mut out),
    ) {
        Ok(()) => {
            if !mapped.is_null() {
                (*mapped).pData = out.pData;
                (*mapped).RowPitch = out.RowPitch;
                (*mapped).DepthPitch = out.DepthPitch;
            }
        }
        Err(e) => {
            log_line(&format!("DDI resource_map failed: {e:?}"));
            if !mapped.is_null() {
                (*mapped).pData = core::ptr::null_mut();
            }
        }
    }
}

unsafe extern "C" fn resource_unmap(h: Hdevice, h_resource: ddi::D3D10DDI_HRESOURCE, subresource: u32) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(res) = load_com::<ID3D11Resource>(h_resource.pDrvPrivate) else {
        return;
    };
    context.Unmap(&*res, subresource);
}

unsafe extern "C" fn flush(h: Hdevice) {
    if let Some(context) = d3d11_context(h) {
        context.Flush();
    }
}

// --- Shaders ----------------------------------------------------------------

/// DXBC container total byte size (header field at byte offset 24).
unsafe fn dxbc_len(code: *const u32) -> usize {
    *code.add(6) as usize
}

unsafe extern "C" fn calc_size_shader(
    _h: Hdevice,
    _code: *const u32,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) -> u64 {
    8
}

unsafe extern "C" fn create_vertex_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) {
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let bytes = core::slice::from_raw_parts(code as *const u8, dxbc_len(code));
    let mut vs: Option<ID3D11VertexShader> = None;
    match device.CreateVertexShader(bytes, None, Some(&mut vs)) {
        Ok(()) => {
            if let Some(s) = vs {
                store_com(h_shader.pDrvPrivate, s);
            }
        }
        Err(e) => log_line(&format!("DDI create_vertex_shader failed: {e:?}")),
    }
}

unsafe extern "C" fn create_pixel_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) {
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let bytes = core::slice::from_raw_parts(code as *const u8, dxbc_len(code));
    let mut ps: Option<ID3D11PixelShader> = None;
    match device.CreatePixelShader(bytes, None, Some(&mut ps)) {
        Ok(()) => {
            if let Some(s) = ps {
                store_com(h_shader.pDrvPrivate, s);
            }
        }
        Err(e) => log_line(&format!("DDI create_pixel_shader failed: {e:?}")),
    }
}

unsafe extern "C" fn destroy_shader(_h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    release_com(h_shader.pDrvPrivate);
}

unsafe extern "C" fn vs_set_shader(h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11VertexShader>(h_shader.pDrvPrivate) {
        Some(s) => context.VSSetShader(&*s, None),
        None => context.VSSetShader(None, None),
    }
}

unsafe extern "C" fn ps_set_shader(h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11PixelShader>(h_shader.pDrvPrivate) {
        Some(s) => context.PSSetShader(&*s, None),
        None => context.PSSetShader(None, None),
    }
}

// --- Output-merger / rasterizer state setters -------------------------------

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn set_render_targets(
    h: Hdevice,
    rtvs: *const ddi::D3D10DDI_HRENDERTARGETVIEW,
    num_views: u32,
    _clear_slots: u32,
    dsv: ddi::D3D10DDI_HDEPTHSTENCILVIEW,
    _uavs: *const ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    _uav_counts: *const u32,
    _uav_start: u32,
    _num_uavs: u32,
    _uav_first: u32,
    _uav_count: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let mut views: Vec<Option<ID3D11RenderTargetView>> = Vec::with_capacity(num_views as usize);
    for i in 0..num_views as usize {
        let p = (*rtvs.add(i)).pDrvPrivate;
        views.push(load_com::<ID3D11RenderTargetView>(p).map(|m| (*m).clone()));
    }
    let depth = load_com::<ID3D11DepthStencilView>(dsv.pDrvPrivate).map(|m| (*m).clone());
    context.OMSetRenderTargets(Some(&views), depth.as_ref());
}

unsafe extern "C" fn set_viewports(
    h: Hdevice,
    num: u32,
    _clear: u32,
    vps: *const ddi::D3D10_DDI_VIEWPORT,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let mut out: Vec<D3D11_VIEWPORT> = Vec::with_capacity(num as usize);
    for i in 0..num as usize {
        let v = &*vps.add(i);
        out.push(D3D11_VIEWPORT {
            TopLeftX: v.TopLeftX as f32,
            TopLeftY: v.TopLeftY as f32,
            Width: v.Width as f32,
            Height: v.Height as f32,
            MinDepth: v.MinDepth,
            MaxDepth: v.MaxDepth,
        });
    }
    context.RSSetViewports(Some(&out));
}

unsafe extern "C" fn ia_set_topology(h: Hdevice, topo: ddi::D3D10_DDI_PRIMITIVE_TOPOLOGY) {
    if let Some(context) = d3d11_context(h) {
        context.IASetPrimitiveTopology(windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY(
            topo as i32,
        ));
    }
}

unsafe extern "C" fn draw(h: Hdevice, vertex_count: u32, start_vertex: u32) {
    if let Some(context) = d3d11_context(h) {
        context.Draw(vertex_count, start_vertex);
    }
}

unsafe extern "C" fn draw_indexed(
    h: Hdevice,
    index_count: u32,
    start_index: u32,
    base_vertex: i32,
) {
    if let Some(context) = d3d11_context(h) {
        context.DrawIndexed(index_count, start_index, base_vertex);
    }
}

// --- Rasterizer / depth-stencil state ---------------------------------------

unsafe extern "C" fn calc_size_raster(
    _h: Hdevice,
    _d: *const ddi::D3D10_DDI_RASTERIZER_DESC,
) -> u64 {
    8
}
unsafe extern "C" fn calc_size_depth(
    _h: Hdevice,
    _d: *const ddi::D3D10_DDI_DEPTH_STENCIL_DESC,
) -> u64 {
    8
}

unsafe extern "C" fn create_rasterizer_state(
    h: Hdevice,
    desc: *const ddi::D3D10_DDI_RASTERIZER_DESC,
    h_rs: ddi::D3D10DDI_HRASTERIZERSTATE,
    _hrt: ddi::D3D10DDI_HRTRASTERIZERSTATE,
) {
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
    let mut rs: Option<ID3D11RasterizerState> = None;
    if device.CreateRasterizerState(&rd, Some(&mut rs)).is_ok() {
        if let Some(s) = rs {
            store_com(h_rs.pDrvPrivate, s);
        }
    }
}

unsafe extern "C" fn set_rasterizer_state(h: Hdevice, h_rs: ddi::D3D10DDI_HRASTERIZERSTATE) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11RasterizerState>(h_rs.pDrvPrivate) {
        Some(s) => context.RSSetState(&*s),
        None => context.RSSetState(None),
    }
}

unsafe fn cvt_stencilop(d: &ddi::D3D10_DDI_DEPTH_STENCILOP_DESC) -> D3D11_DEPTH_STENCILOP_DESC {
    D3D11_DEPTH_STENCILOP_DESC {
        StencilFailOp: D3D11_STENCIL_OP(d.StencilFailOp),
        StencilDepthFailOp: D3D11_STENCIL_OP(d.StencilDepthFailOp),
        StencilPassOp: D3D11_STENCIL_OP(d.StencilPassOp),
        StencilFunc: D3D11_COMPARISON_FUNC(d.StencilFunc),
    }
}

unsafe extern "C" fn create_depth_stencil_state(
    h: Hdevice,
    desc: *const ddi::D3D10_DDI_DEPTH_STENCIL_DESC,
    h_ds: ddi::D3D10DDI_HDEPTHSTENCILSTATE,
    _hrt: ddi::D3D10DDI_HRTDEPTHSTENCILSTATE,
) {
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
    if device.CreateDepthStencilState(&dd, Some(&mut ds)).is_ok() {
        if let Some(s) = ds {
            store_com(h_ds.pDrvPrivate, s);
        }
    }
}

unsafe extern "C" fn set_depth_stencil_state(
    h: Hdevice,
    h_ds: ddi::D3D10DDI_HDEPTHSTENCILSTATE,
    stencil_ref: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11DepthStencilState>(h_ds.pDrvPrivate) {
        Some(s) => context.OMSetDepthStencilState(&*s, stencil_ref),
        None => context.OMSetDepthStencilState(None, stencil_ref),
    }
}

unsafe extern "C" fn destroy_raster_state(_h: Hdevice, h_state: ddi::D3D10DDI_HRASTERIZERSTATE) {
    release_com(h_state.pDrvPrivate);
}

// --- Shader resource views, samplers, constant buffers ----------------------

unsafe extern "C" fn calc_size_srv(
    _h: Hdevice,
    _a: *const ddi::D3D11DDIARG_CREATESHADERRESOURCEVIEW,
) -> u64 {
    8
}

unsafe extern "C" fn create_srv(
    h: Hdevice,
    arg: *const ddi::D3D11DDIARG_CREATESHADERRESOURCEVIEW,
    h_srv: ddi::D3D10DDI_HSHADERRESOURCEVIEW,
    _hrt: ddi::D3D10DDI_HRTSHADERRESOURCEVIEW,
) {
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let Some(res) = load_com::<ID3D11Resource>(a.hDrvResource.pDrvPrivate) else {
        return;
    };
    let mut srv: Option<ID3D11ShaderResourceView> = None;
    match device.CreateShaderResourceView(&*res, None, Some(&mut srv)) {
        Ok(()) => {
            if let Some(v) = srv {
                store_com(h_srv.pDrvPrivate, v);
            }
        }
        Err(e) => log_line(&format!("DDI create_srv failed: {e:?}")),
    }
}

unsafe extern "C" fn destroy_srv(_h: Hdevice, h_srv: ddi::D3D10DDI_HSHADERRESOURCEVIEW) {
    release_com(h_srv.pDrvPrivate);
}

unsafe extern "C" fn calc_size_sampler(_h: Hdevice, _d: *const ddi::D3D10_DDI_SAMPLER_DESC) -> u64 {
    8
}

unsafe extern "C" fn create_sampler(
    h: Hdevice,
    desc: *const ddi::D3D10_DDI_SAMPLER_DESC,
    h_sampler: ddi::D3D10DDI_HSAMPLER,
    _hrt: ddi::D3D10DDI_HRTSAMPLER,
) {
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let d = &*desc;
    let sd = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER(d.Filter),
        AddressU: D3D11_TEXTURE_ADDRESS_MODE(d.AddressU),
        AddressV: D3D11_TEXTURE_ADDRESS_MODE(d.AddressV),
        AddressW: D3D11_TEXTURE_ADDRESS_MODE(d.AddressW),
        MipLODBias: d.MipLODBias,
        MaxAnisotropy: d.MaxAnisotropy,
        ComparisonFunc: D3D11_COMPARISON_FUNC(d.ComparisonFunc),
        BorderColor: d.BorderColor,
        MinLOD: d.MinLOD,
        MaxLOD: d.MaxLOD,
    };
    let mut s: Option<ID3D11SamplerState> = None;
    if device.CreateSamplerState(&sd, Some(&mut s)).is_ok() {
        if let Some(o) = s {
            store_com(h_sampler.pDrvPrivate, o);
        }
    }
}

unsafe extern "C" fn destroy_sampler(_h: Hdevice, h_sampler: ddi::D3D10DDI_HSAMPLER) {
    release_com(h_sampler.pDrvPrivate);
}

unsafe fn collect_buffers(start: u32, num: u32, h: *const ddi::D3D10DDI_HRESOURCE) -> Vec<Option<ID3D11Buffer>> {
    let _ = start;
    let mut v = Vec::with_capacity(num as usize);
    for i in 0..num as usize {
        let p = (*h.add(i)).pDrvPrivate;
        v.push(load_com::<ID3D11Resource>(p).and_then(|r| (*r).cast::<ID3D11Buffer>().ok()));
    }
    v
}
unsafe fn collect_srvs(num: u32, h: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW) -> Vec<Option<ID3D11ShaderResourceView>> {
    let mut v = Vec::with_capacity(num as usize);
    for i in 0..num as usize {
        let p = (*h.add(i)).pDrvPrivate;
        v.push(load_com::<ID3D11ShaderResourceView>(p).map(|m| (*m).clone()));
    }
    v
}
unsafe fn collect_samplers(num: u32, h: *const ddi::D3D10DDI_HSAMPLER) -> Vec<Option<ID3D11SamplerState>> {
    let mut v = Vec::with_capacity(num as usize);
    for i in 0..num as usize {
        let p = (*h.add(i)).pDrvPrivate;
        v.push(load_com::<ID3D11SamplerState>(p).map(|m| (*m).clone()));
    }
    v
}

unsafe extern "C" fn ps_set_constant_buffers(h: Hdevice, start: u32, num: u32, bufs: *const ddi::D3D10DDI_HRESOURCE) {
    if let Some(c) = d3d11_context(h) {
        c.PSSetConstantBuffers(start, Some(&collect_buffers(start, num, bufs)));
    }
}
unsafe extern "C" fn vs_set_constant_buffers(h: Hdevice, start: u32, num: u32, bufs: *const ddi::D3D10DDI_HRESOURCE) {
    if let Some(c) = d3d11_context(h) {
        c.VSSetConstantBuffers(start, Some(&collect_buffers(start, num, bufs)));
    }
}
unsafe extern "C" fn ps_set_shader_resources(h: Hdevice, start: u32, num: u32, srvs: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW) {
    if let Some(c) = d3d11_context(h) {
        c.PSSetShaderResources(start, Some(&collect_srvs(num, srvs)));
    }
}
unsafe extern "C" fn vs_set_shader_resources(h: Hdevice, start: u32, num: u32, srvs: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW) {
    if let Some(c) = d3d11_context(h) {
        c.VSSetShaderResources(start, Some(&collect_srvs(num, srvs)));
    }
}
unsafe extern "C" fn ps_set_samplers(h: Hdevice, start: u32, num: u32, samplers: *const ddi::D3D10DDI_HSAMPLER) {
    if let Some(c) = d3d11_context(h) {
        c.PSSetSamplers(start, Some(&collect_samplers(num, samplers)));
    }
}
unsafe extern "C" fn vs_set_samplers(h: Hdevice, start: u32, num: u32, samplers: *const ddi::D3D10DDI_HSAMPLER) {
    if let Some(c) = d3d11_context(h) {
        c.VSSetSamplers(start, Some(&collect_samplers(num, samplers)));
    }
}

unsafe extern "C" fn resource_update_subresource(
    h: Hdevice,
    h_res: ddi::D3D10DDI_HRESOURCE,
    subresource: u32,
    box_: *const ddi::D3D10_DDI_BOX,
    data: *const c_void,
    row_pitch: u32,
    depth_pitch: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(res) = load_com::<ID3D11Resource>(h_res.pDrvPrivate) else {
        return;
    };
    let bx;
    let bx_ptr = if box_.is_null() {
        None
    } else {
        let b = &*box_;
        bx = D3D11_BOX {
            left: b.left as u32,
            top: b.top as u32,
            front: b.front as u32,
            right: b.right as u32,
            bottom: b.bottom as u32,
            back: b.back as u32,
        };
        Some(&bx as *const D3D11_BOX)
    };
    context.UpdateSubresource(&*res, subresource, bx_ptr, data, row_pitch, depth_pitch);
}

unsafe extern "C" fn check_format_support(h: Hdevice, fmt: ddi::DXGI_FORMAT, out: *mut u32) {
    let mut caps: u32 = 0;
    if let Some(device) = d3d11_device(h) {
        if let Ok(c) = device.CheckFormatSupport(DXGI_FORMAT(fmt as i32)) {
            caps = c;
        }
    }
    if !out.is_null() {
        *out = caps;
    }
}
unsafe extern "C" fn destroy_depth_state(_h: Hdevice, h_state: ddi::D3D10DDI_HDEPTHSTENCILSTATE) {
    release_com(h_state.pDrvPrivate);
}

/// In-process offscreen clear+readback through the real forwarders (no install,
/// no DXGI, no runtime): create a render-target texture, clear it, copy to a
/// staging texture, map and read back pixel 0. Returns 0 on PASS. `h` is the
/// device handle whose pDrvPrivate is a constructed HeliosDevice.
pub unsafe fn selftest_offscreen_clear(h: Hdevice) -> i32 {
    let fmt_bgra: ddi::DXGI_FORMAT = 87; // DXGI_FORMAT_B8G8R8A8_UNORM
    let mip = ddi::D3D10DDI_MIPINFO {
        TexelWidth: 64,
        TexelHeight: 64,
        TexelDepth: 1,
        PhysicalWidth: 64,
        PhysicalHeight: 64,
        PhysicalDepth: 1,
    };

    let mut rt_desc = ddi::D3D11DDIARG_CREATERESOURCE::default();
    rt_desc.pMipInfoList = &mip;
    rt_desc.ResourceDimension = RES_TEX2D;
    rt_desc.Format = fmt_bgra;
    rt_desc.Usage = 0; // DEFAULT
    rt_desc.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
    rt_desc.MipLevels = 1;
    rt_desc.ArraySize = 1;
    rt_desc.SampleDesc.Count = 1;

    let mut rt_priv = 0u64;
    let h_rt = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut rt_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &rt_desc, h_rt, Default::default());
    if rt_priv == 0 {
        log_line("selftest_offscreen_clear: RT create failed");
        return 1;
    }

    let mut rtv_desc = ddi::D3D10DDIARG_CREATERENDERTARGETVIEW::default();
    rtv_desc.hDrvResource = h_rt;
    rtv_desc.Format = fmt_bgra;
    rtv_desc.ResourceDimension = RES_TEX2D;
    let mut rtv_priv = 0u64;
    let h_rtv = ddi::D3D10DDI_HRENDERTARGETVIEW {
        pDrvPrivate: &mut rtv_priv as *mut u64 as *mut c_void,
    };
    create_rtv(h, &rtv_desc, h_rtv, Default::default());
    if rtv_priv == 0 {
        log_line("selftest_offscreen_clear: RTV create failed");
        return 2;
    }

    let mut color = [0.25f32, 0.5, 0.75, 1.0];
    clear_rtv(h, h_rtv, color.as_mut_ptr());

    let mut st_desc = ddi::D3D11DDIARG_CREATERESOURCE::default();
    st_desc.pMipInfoList = &mip;
    st_desc.ResourceDimension = RES_TEX2D;
    st_desc.Format = fmt_bgra;
    st_desc.Usage = 3; // STAGING
    st_desc.BindFlags = 0;
    st_desc.MipLevels = 1;
    st_desc.ArraySize = 1;
    st_desc.SampleDesc.Count = 1;
    let mut st_priv = 0u64;
    let h_st = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut st_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &st_desc, h_st, Default::default());
    if st_priv == 0 {
        log_line("selftest_offscreen_clear: staging create failed");
        return 3;
    }

    resource_copy(h, h_st, h_rt);
    flush(h);

    let mut mapped = ddi::D3D10DDI_MAPPED_SUBRESOURCE::default();
    resource_map(h, h_st, 0, 1 /* D3D10_DDI_MAP_READ */, 0, &mut mapped);
    let mut result = 4;
    if !mapped.pData.is_null() {
        let px = mapped.pData as *const u8;
        let (b, g, r, a) = (*px, *px.add(1), *px.add(2), *px.add(3));
        log_line(&format!(
            "selftest_offscreen_clear: readback BGRA = {b} {g} {r} {a} (want ~191 128 64 255)"
        ));
        let ok = (b as i32 - 191).abs() <= 2
            && (g as i32 - 128).abs() <= 2
            && (r as i32 - 64).abs() <= 2
            && a == 255;
        result = if ok { 0 } else { 5 };
        resource_unmap(h, h_st, 0);
    }

    destroy_resource(h, h_st);
    destroy_rtv(h, h_rtv);
    destroy_resource(h, h_rt);
    log_line(&format!("selftest_offscreen_clear: result={result} (0=PASS)"));
    result
}

unsafe fn compile_hlsl(src: &[u8], entry: &[u8], target: &[u8]) -> Option<ID3DBlob> {
    let mut blob: Option<ID3DBlob> = None;
    let mut errs: Option<ID3DBlob> = None;
    let hr = D3DCompile(
        src.as_ptr() as *const c_void,
        src.len(),
        PCSTR::null(),
        None,
        None,
        PCSTR(entry.as_ptr()),
        PCSTR(target.as_ptr()),
        0,
        0,
        &mut blob,
        Some(&mut errs),
    );
    if hr.is_err() {
        if let Some(e) = errs {
            let msg = core::slice::from_raw_parts(
                e.GetBufferPointer() as *const u8,
                e.GetBufferSize(),
            );
            log_line(&format!("D3DCompile error: {}", String::from_utf8_lossy(msg)));
        }
        return None;
    }
    blob
}

/// Synthesize + create a tex2d via the create_resource forwarder. Returns the
/// resource handle private storage (caller owns it).
unsafe fn make_tex2d(h: Hdevice, priv_: &mut u64, usage: u32, bind: u32) -> ddi::D3D10DDI_HRESOURCE {
    let mip = ddi::D3D10DDI_MIPINFO {
        TexelWidth: 64,
        TexelHeight: 64,
        TexelDepth: 1,
        PhysicalWidth: 64,
        PhysicalHeight: 64,
        PhysicalDepth: 1,
    };
    let mut desc = ddi::D3D11DDIARG_CREATERESOURCE::default();
    desc.pMipInfoList = &mip;
    desc.ResourceDimension = RES_TEX2D;
    desc.Format = 87; // BGRA
    desc.Usage = usage;
    desc.BindFlags = bind;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.SampleDesc.Count = 1;
    let hr = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: priv_ as *mut u64 as *mut c_void,
    };
    create_resource(h, &desc, hr, Default::default());
    hr
}

/// Draw a `SV_VertexID` triangle (no vertex buffer / input layout) into an
/// offscreen RT and read back the center pixel. Returns 0 on PASS.
pub unsafe fn selftest_triangle(h: Hdevice) -> i32 {
    let vs_src = b"float4 VS(uint id:SV_VertexID):SV_Position{float2 uv=float2((id<<1)&2,id&2);return float4(uv*2-1,0,1);}\0";
    let ps_src = b"float4 PS():SV_Target{return float4(1,0,0,1);}\0"; // red
    let Some(vsb) = compile_hlsl(vs_src, b"VS\0", b"vs_5_0\0") else { return 10; };
    let Some(psb) = compile_hlsl(ps_src, b"PS\0", b"ps_5_0\0") else { return 11; };

    let mut rt_priv = 0u64;
    let h_rt = make_tex2d(h, &mut rt_priv, 0, D3D11_BIND_RENDER_TARGET.0 as u32);
    if rt_priv == 0 {
        return 12;
    }
    let mut rtv_desc = ddi::D3D10DDIARG_CREATERENDERTARGETVIEW::default();
    rtv_desc.hDrvResource = h_rt;
    rtv_desc.Format = 87;
    rtv_desc.ResourceDimension = RES_TEX2D;
    let mut rtv_priv = 0u64;
    let h_rtv = ddi::D3D10DDI_HRENDERTARGETVIEW {
        pDrvPrivate: &mut rtv_priv as *mut u64 as *mut c_void,
    };
    create_rtv(h, &rtv_desc, h_rtv, Default::default());
    if rtv_priv == 0 {
        return 13;
    }

    let mut vs_priv = 0u64;
    let h_vs = ddi::D3D10DDI_HSHADER {
        pDrvPrivate: &mut vs_priv as *mut u64 as *mut c_void,
    };
    create_vertex_shader(
        h,
        vsb.GetBufferPointer() as *const u32,
        h_vs,
        Default::default(),
        core::ptr::null(),
    );
    let mut ps_priv = 0u64;
    let h_ps = ddi::D3D10DDI_HSHADER {
        pDrvPrivate: &mut ps_priv as *mut u64 as *mut c_void,
    };
    create_pixel_shader(
        h,
        psb.GetBufferPointer() as *const u32,
        h_ps,
        Default::default(),
        core::ptr::null(),
    );
    if vs_priv == 0 || ps_priv == 0 {
        log_line("selftest_triangle: shader create failed");
        return 14;
    }
    vs_set_shader(h, h_vs);
    ps_set_shader(h, h_ps);

    // CULL_NONE rasterizer so the (back-facing) triangle isn't culled — also
    // validates create_rasterizer_state + set_rasterizer_state.
    let mut rs_desc = ddi::D3D10_DDI_RASTERIZER_DESC::default();
    rs_desc.FillMode = 3; // SOLID
    rs_desc.CullMode = 1; // NONE
    rs_desc.DepthClipEnable = 1;
    let mut rs_priv = 0u64;
    let h_rs = ddi::D3D10DDI_HRASTERIZERSTATE {
        pDrvPrivate: &mut rs_priv as *mut u64 as *mut c_void,
    };
    create_rasterizer_state(h, &rs_desc, h_rs, Default::default());
    set_rasterizer_state(h, h_rs);

    let mut black = [0.0f32, 0.0, 0.0, 1.0];
    clear_rtv(h, h_rtv, black.as_mut_ptr());
    set_render_targets(
        h,
        &h_rtv,
        1,
        0,
        Default::default(),
        core::ptr::null(),
        core::ptr::null(),
        0,
        0,
        0,
        0,
    );
    let vp = ddi::D3D10_DDI_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: 64.0,
        Height: 64.0,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    };
    set_viewports(h, 1, 0, &vp);
    ia_set_topology(h, 4 /* TRIANGLELIST */);
    draw(h, 3, 0);
    flush(h);

    // Read back the center pixel.
    let mut st_priv = 0u64;
    let h_st = make_tex2d(h, &mut st_priv, 3 /* STAGING */, 0);
    if st_priv == 0 {
        return 15;
    }
    resource_copy(h, h_st, h_rt);
    flush(h);
    let mut mapped = ddi::D3D10DDI_MAPPED_SUBRESOURCE::default();
    resource_map(h, h_st, 0, 1, 0, &mut mapped);
    let mut result = 16;
    if !mapped.pData.is_null() {
        let off = 32 * mapped.RowPitch as usize + 32 * 4;
        let px = (mapped.pData as *const u8).add(off);
        let (b, g, r, a) = (*px, *px.add(1), *px.add(2), *px.add(3));
        log_line(&format!(
            "selftest_triangle: center BGRA = {b} {g} {r} {a} (want red ~0 0 255 255)"
        ));
        result = if r > 250 && g < 5 && b < 5 && a == 255 { 0 } else { 17 };
        resource_unmap(h, h_st, 0);
    }
    destroy_resource(h, h_st);
    destroy_shader(h, h_ps);
    destroy_shader(h, h_vs);
    destroy_rtv(h, h_rtv);
    destroy_resource(h, h_rt);
    log_line(&format!("selftest_triangle: result={result} (0=PASS)"));
    result
}

/// Diagnostic: create an immutable constant buffer with known content, copy it
/// to a staging buffer, and read it back. Tells us whether buffer-content upload
/// works (red back) or not (zero back) — independent of shader binding.
pub unsafe fn selftest_cb_readback(h: Hdevice) -> i32 {
    let red: [f32; 4] = [1.0, 0.25, 0.5, 1.0];
    let mip = ddi::D3D10DDI_MIPINFO {
        TexelWidth: 16,
        ..Default::default()
    };
    let init = ddi::D3D10_DDIARG_SUBRESOURCE_UP {
        pSysMem: red.as_ptr() as *mut c_void,
        SysMemPitch: 16,
        SysMemSlicePitch: 16,
    };
    let mut cb = ddi::D3D11DDIARG_CREATERESOURCE::default();
    cb.pMipInfoList = &mip;
    cb.pInitialDataUP = &init;
    cb.ResourceDimension = RES_BUFFER;
    cb.Usage = 1; // IMMUTABLE
    cb.BindFlags = D3D11_BIND_CONSTANT_BUFFER.0 as u32;
    cb.MipLevels = 1;
    cb.ArraySize = 1;
    let mut cb_priv = 0u64;
    let h_cb = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut cb_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &cb, h_cb, Default::default());

    let mut st = ddi::D3D11DDIARG_CREATERESOURCE::default();
    st.pMipInfoList = &mip;
    st.ResourceDimension = RES_BUFFER;
    st.Usage = 3; // STAGING
    st.BindFlags = 0;
    st.MipLevels = 1;
    st.ArraySize = 1;
    let mut st_priv = 0u64;
    let h_st = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut st_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &st, h_st, Default::default());
    if cb_priv == 0 || st_priv == 0 {
        log_line(&format!("cb_readback: create failed cb={cb_priv:#x} st={st_priv:#x}"));
        return 1;
    }
    resource_copy(h, h_st, h_cb);
    flush(h);
    let mut mapped = ddi::D3D10DDI_MAPPED_SUBRESOURCE::default();
    resource_map(h, h_st, 0, 1, 0, &mut mapped);
    if mapped.pData.is_null() {
        log_line("cb_readback: map failed");
        return 2;
    }
    let f = mapped.pData as *const f32;
    log_line(&format!(
        "cb_readback: floats = {} {} {} {} (want 1 0.25 0.5 1)",
        *f, *f.add(1), *f.add(2), *f.add(3)
    ));
    resource_unmap(h, h_st, 0);
    destroy_resource(h, h_st);
    destroy_resource(h, h_cb);
    0
}

/// Diagnostic: full draw of a triangle whose PS reads a constant buffer, with
/// probes to localize the "CB content never reaches the shader" bug. Returns 0
/// when the center pixel is the CB-supplied green.
///
/// Probes, in order:
///  1. CB created with immutable green initial-data (proven-good content path).
///  2. After `ps_set_constant_buffers`, call `PSGetConstantBuffers` and log
///     whether DXVK reports our buffer bound — splits "our bind didn't register"
///     from "bound but shader reads garbage".
///  3. After the draw, copy the CB to staging and read it back — confirms the
///     resource still holds green at draw time (rules out it being clobbered).
pub unsafe fn selftest_triangle_cb(h: Hdevice) -> i32 {
    let vs_src = b"float4 VS(uint id:SV_VertexID):SV_Position{float2 uv=float2((id<<1)&2,id&2);return float4(uv*2-1,0,1);}\0";
    // PS reads col from the constant buffer at b0.
    let ps_src = b"cbuffer C:register(b0){float4 col;} float4 PS():SV_Target{return col;}\0";
    let Some(vsb) = compile_hlsl(vs_src, b"VS\0", b"vs_5_0\0") else { return 30; };
    let Some(psb) = compile_hlsl(ps_src, b"PS\0", b"ps_5_0\0") else { return 31; };

    let mut rt_priv = 0u64;
    let h_rt = make_tex2d(h, &mut rt_priv, 0, D3D11_BIND_RENDER_TARGET.0 as u32);
    if rt_priv == 0 {
        return 32;
    }
    let mut rtv_desc = ddi::D3D10DDIARG_CREATERENDERTARGETVIEW::default();
    rtv_desc.hDrvResource = h_rt;
    rtv_desc.Format = 87;
    rtv_desc.ResourceDimension = RES_TEX2D;
    let mut rtv_priv = 0u64;
    let h_rtv = ddi::D3D10DDI_HRENDERTARGETVIEW {
        pDrvPrivate: &mut rtv_priv as *mut u64 as *mut c_void,
    };
    create_rtv(h, &rtv_desc, h_rtv, Default::default());
    if rtv_priv == 0 {
        return 33;
    }

    // Constant buffer: 16 bytes, immutable, content = green (0,1,0,1).
    let green: [f32; 4] = [0.0, 1.0, 0.0, 1.0];
    let mip = ddi::D3D10DDI_MIPINFO {
        TexelWidth: 16,
        ..Default::default()
    };
    let init = ddi::D3D10_DDIARG_SUBRESOURCE_UP {
        pSysMem: green.as_ptr() as *mut c_void,
        SysMemPitch: 16,
        SysMemSlicePitch: 16,
    };
    let mut cb = ddi::D3D11DDIARG_CREATERESOURCE::default();
    cb.pMipInfoList = &mip;
    cb.pInitialDataUP = &init;
    cb.ResourceDimension = RES_BUFFER;
    cb.Usage = 1; // IMMUTABLE
    cb.BindFlags = D3D11_BIND_CONSTANT_BUFFER.0 as u32;
    cb.MipLevels = 1;
    cb.ArraySize = 1;
    let mut cb_priv = 0u64;
    let h_cb = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut cb_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &cb, h_cb, Default::default());
    if cb_priv == 0 {
        log_line("selftest_triangle_cb: CB create failed");
        return 34;
    }
    log_line(&format!("selftest_triangle_cb: CB COM ptr = {cb_priv:#x}"));

    // Shaders.
    let mut vs_priv = 0u64;
    let h_vs = ddi::D3D10DDI_HSHADER {
        pDrvPrivate: &mut vs_priv as *mut u64 as *mut c_void,
    };
    create_vertex_shader(h, vsb.GetBufferPointer() as *const u32, h_vs, Default::default(), core::ptr::null());
    let mut ps_priv = 0u64;
    let h_ps = ddi::D3D10DDI_HSHADER {
        pDrvPrivate: &mut ps_priv as *mut u64 as *mut c_void,
    };
    create_pixel_shader(h, psb.GetBufferPointer() as *const u32, h_ps, Default::default(), core::ptr::null());
    if vs_priv == 0 || ps_priv == 0 {
        return 35;
    }
    vs_set_shader(h, h_vs);
    ps_set_shader(h, h_ps);

    // CULL_NONE so the triangle isn't culled.
    let mut rs_desc = ddi::D3D10_DDI_RASTERIZER_DESC::default();
    rs_desc.FillMode = 3;
    rs_desc.CullMode = 1;
    rs_desc.DepthClipEnable = 1;
    let mut rs_priv = 0u64;
    let h_rs = ddi::D3D10DDI_HRASTERIZERSTATE {
        pDrvPrivate: &mut rs_priv as *mut u64 as *mut c_void,
    };
    create_rasterizer_state(h, &rs_desc, h_rs, Default::default());
    set_rasterizer_state(h, h_rs);

    // Bind the constant buffer to PS slot b0.
    ps_set_constant_buffers(h, 0, 1, &h_cb);

    // PROBE 2: ask DXVK what it now has bound at PS b0.
    if let Some(c) = d3d11_context(h) {
        let mut bound: [Option<ID3D11Buffer>; 1] = [None];
        c.PSGetConstantBuffers(0, Some(&mut bound));
        match &bound[0] {
            Some(b) => {
                let raw = b.as_raw() as usize;
                log_line(&format!(
                    "selftest_triangle_cb: PSGetConstantBuffers[0] = {raw:#x} (CB was {cb_priv:#x}, match={})",
                    raw == cb_priv as usize
                ));
            }
            None => log_line("selftest_triangle_cb: PSGetConstantBuffers[0] = NULL (bind did NOT register!)"),
        }
    }

    let mut black = [0.0f32, 0.0, 0.0, 1.0];
    clear_rtv(h, h_rtv, black.as_mut_ptr());
    set_render_targets(h, &h_rtv, 1, 0, Default::default(), core::ptr::null(), core::ptr::null(), 0, 0, 0, 0);
    let vp = ddi::D3D10_DDI_VIEWPORT {
        TopLeftX: 0.0, TopLeftY: 0.0, Width: 64.0, Height: 64.0, MinDepth: 0.0, MaxDepth: 1.0,
    };
    set_viewports(h, 1, 0, &vp);
    ia_set_topology(h, 4 /* TRIANGLELIST */);
    draw(h, 3, 0);
    flush(h);

    // PROBE 3: CB content still green at draw time?
    let mut cbst = ddi::D3D11DDIARG_CREATERESOURCE::default();
    cbst.pMipInfoList = &mip;
    cbst.ResourceDimension = RES_BUFFER;
    cbst.Usage = 3; // STAGING
    cbst.MipLevels = 1;
    cbst.ArraySize = 1;
    let mut cbst_priv = 0u64;
    let h_cbst = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut cbst_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &cbst, h_cbst, Default::default());
    if cbst_priv != 0 {
        resource_copy(h, h_cbst, h_cb);
        flush(h);
        let mut m = ddi::D3D10DDI_MAPPED_SUBRESOURCE::default();
        resource_map(h, h_cbst, 0, 1, 0, &mut m);
        if !m.pData.is_null() {
            let f = m.pData as *const f32;
            log_line(&format!(
                "selftest_triangle_cb: CB content at draw time = {} {} {} {} (want 0 1 0 1)",
                *f, *f.add(1), *f.add(2), *f.add(3)
            ));
            resource_unmap(h, h_cbst, 0);
        }
        destroy_resource(h, h_cbst);
    }

    // Read back the center pixel.
    let mut st_priv = 0u64;
    let h_st = make_tex2d(h, &mut st_priv, 3, 0);
    if st_priv == 0 {
        return 36;
    }
    resource_copy(h, h_st, h_rt);
    flush(h);
    let mut mapped = ddi::D3D10DDI_MAPPED_SUBRESOURCE::default();
    resource_map(h, h_st, 0, 1, 0, &mut mapped);
    let mut result = 37;
    if !mapped.pData.is_null() {
        let off = 32 * mapped.RowPitch as usize + 32 * 4;
        let px = (mapped.pData as *const u8).add(off);
        let (b, g, r, a) = (*px, *px.add(1), *px.add(2), *px.add(3));
        log_line(&format!(
            "selftest_triangle_cb: center BGRA = {b} {g} {r} {a} (want green ~0 255 0 255)"
        ));
        result = if g > 250 && r < 5 && b < 5 && a == 255 { 0 } else { 38 };
        resource_unmap(h, h_st, 0);
    }
    destroy_resource(h, h_st);
    destroy_shader(h, h_ps);
    destroy_shader(h, h_vs);
    destroy_resource(h, h_cb);
    destroy_rtv(h, h_rtv);
    destroy_resource(h, h_rt);
    log_line(&format!("selftest_triangle_cb: result={result} (0=PASS)"));
    result
}

/// Install the implemented forwarders into the device-funcs table (over the
/// stub fill). Uses the real bindgen PFN field types — no transmute.
pub unsafe fn install(funcs: *mut ddi::D3D11DDI_DEVICEFUNCS) {
    let f = &mut *funcs;
    f.pfnCalcPrivateResourceSize = Some(calc_size_resource);
    f.pfnCreateResource = Some(create_resource);
    f.pfnDestroyResource = Some(destroy_resource);
    f.pfnCalcPrivateRenderTargetViewSize = Some(calc_size_rtv);
    f.pfnCreateRenderTargetView = Some(create_rtv);
    f.pfnDestroyRenderTargetView = Some(destroy_rtv);
    f.pfnClearRenderTargetView = Some(clear_rtv);
    f.pfnResourceCopy = Some(resource_copy);
    f.pfnResourceMap = Some(resource_map);
    f.pfnResourceUnmap = Some(resource_unmap);
    f.pfnFlush = Some(flush);

    // Shaders + pipeline.
    f.pfnCalcPrivateShaderSize = Some(calc_size_shader);
    f.pfnCreateVertexShader = Some(create_vertex_shader);
    f.pfnCreatePixelShader = Some(create_pixel_shader);
    f.pfnDestroyShader = Some(destroy_shader);
    f.pfnVsSetShader = Some(vs_set_shader);
    f.pfnPsSetShader = Some(ps_set_shader);
    f.pfnSetRenderTargets = Some(set_render_targets);
    f.pfnSetViewports = Some(set_viewports);
    f.pfnIaSetTopology = Some(ia_set_topology);
    f.pfnDraw = Some(draw);
    f.pfnDrawIndexed = Some(draw_indexed);

    // Rasterizer + depth-stencil state.
    f.pfnCalcPrivateRasterizerStateSize = Some(calc_size_raster);
    f.pfnCreateRasterizerState = Some(create_rasterizer_state);
    f.pfnSetRasterizerState = Some(set_rasterizer_state);
    f.pfnDestroyRasterizerState = Some(destroy_raster_state);
    f.pfnCalcPrivateDepthStencilStateSize = Some(calc_size_depth);
    f.pfnCreateDepthStencilState = Some(create_depth_stencil_state);
    f.pfnSetDepthStencilState = Some(set_depth_stencil_state);
    f.pfnDestroyDepthStencilState = Some(destroy_depth_state);

    // SRVs, samplers, constant buffers, updates, format support.
    f.pfnCalcPrivateShaderResourceViewSize = Some(calc_size_srv);
    f.pfnCreateShaderResourceView = Some(create_srv);
    f.pfnDestroyShaderResourceView = Some(destroy_srv);
    f.pfnCalcPrivateSamplerSize = Some(calc_size_sampler);
    f.pfnCreateSampler = Some(create_sampler);
    f.pfnDestroySampler = Some(destroy_sampler);
    f.pfnPsSetConstantBuffers = Some(ps_set_constant_buffers);
    f.pfnVsSetConstantBuffers = Some(vs_set_constant_buffers);
    f.pfnPsSetShaderResources = Some(ps_set_shader_resources);
    f.pfnVsSetShaderResources = Some(vs_set_shader_resources);
    f.pfnPsSetSamplers = Some(ps_set_samplers);
    f.pfnVsSetSamplers = Some(vs_set_samplers);
    f.pfnResourceUpdateSubresourceUP = Some(resource_update_subresource);
    f.pfnDefaultConstantBufferUpdateSubresourceUP = Some(resource_update_subresource);
    f.pfnCheckFormatSupport = Some(check_format_support);
}
