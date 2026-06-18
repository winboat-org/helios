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

use windows::core::{IUnknown, Interface};
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
}
