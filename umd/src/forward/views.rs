//! View creation and the four view-descriptor translators: RTV, DSV, SRV, UAV,
//! plus samplers and the clear entry points that take a view.
//!
//! Moved verbatim out of `forward.rs` by T8/R1107.

use super::*;

// --- Render target views ----------------------------------------------------

pub(crate) unsafe extern "C" fn create_rtv(
    h: Hdevice,
    arg: *const ddi::D3D10DDIARG_CREATERENDERTARGETVIEW,
    h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW,
    _h_rt: ddi::D3D10DDI_HRTRENDERTARGETVIEW,
) {
    clear_handle(h_rtv);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let Some(res) = load_resource(a.hDrvResource) else {
        log_error!("DDI create_rtv: resource handle empty");
        return;
    };
    let Some(desc) = rtv_desc(a, a.hDrvResource) else {
        log_error!(
            "DDI create_rtv: unsupported resource dimension {} fmt={}",
            a.ResourceDimension,
            a.Format
        );
        return;
    };
    let mut rtv: Option<ID3D11RenderTargetView> = None;
    let created = device.CreateRenderTargetView(&*res, Some(&desc), Some(&mut rtv));
    if let Err(ref e) = created {
        log_error!(
            "DDI create_rtv failed: dim={} fmt={} {e:?}",
            a.ResourceDimension,
            a.Format
        );
    }
    finish_create(h, created, rtv, |v| {
        let n = VIEW_LOG_COUNT.next();
        let allocation = resource_allocation(a.hDrvResource);
        let (width, height) = resource_dimensions(a.hDrvResource);
        if n < 128 {
            trace_line!(
                "DDI create_rtv ok: dim={} fmt={} alloc=0x{:x} {}x{}",
                a.ResourceDimension,
                a.Format,
                allocation,
                width,
                height
            );
        }
        store_rtv(
            h_rtv,
            v,
            resource_com_raw(a.hDrvResource),
            allocation,
            width,
            height,
            a.Format as u32,
        );
    });
}

pub(crate) unsafe fn rtv_desc(
    a: &ddi::D3D10DDIARG_CREATERENDERTARGETVIEW,
    h_res: ddi::D3D10DDI_HRESOURCE,
) -> Option<D3D11_RENDER_TARGET_VIEW_DESC> {
    let format = DXGI_FORMAT(a.Format as i32);
    match a.ResourceDimension {
        RES_BUFFER | RES_BUFFEREX => {
            let b = a.__bindgen_anon_1.Buffer;
            Some(D3D11_RENDER_TARGET_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_RTV_DIMENSION_BUFFER,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Buffer: D3D11_BUFFER_RTV {
                        Anonymous1: D3D11_BUFFER_RTV_0 {
                            FirstElement: b.__bindgen_anon_1.FirstElement,
                        },
                        Anonymous2: D3D11_BUFFER_RTV_1 {
                            NumElements: b.__bindgen_anon_2.NumElements,
                        },
                    },
                },
            })
        }
        RES_TEX1D => {
            let t = a.__bindgen_anon_1.Tex1D;
            Some(match tex1d_shape(t.ArraySize) {
                Tex1DShape::Array => D3D11_RENDER_TARGET_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_RTV_DIMENSION_TEXTURE1DARRAY,
                    Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                        Texture1DArray: D3D11_TEX1D_ARRAY_RTV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                },
                Tex1DShape::Plain => D3D11_RENDER_TARGET_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_RTV_DIMENSION_TEXTURE1D,
                    Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                        Texture1D: D3D11_TEX1D_RTV {
                            MipSlice: t.MipSlice,
                        },
                    },
                },
            })
        }
        RES_TEX2D => {
            let t = a.__bindgen_anon_1.Tex2D;
            Some(
                match tex2d_shape(t.ArraySize, resource_sample_count(h_res)) {
                    Tex2DShape::MsArray => D3D11_RENDER_TARGET_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2DMSARRAY,
                        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                            Texture2DMSArray: D3D11_TEX2DMS_ARRAY_RTV {
                                FirstArraySlice: t.FirstArraySlice,
                                ArraySize: t.ArraySize,
                            },
                        },
                    },
                    Tex2DShape::Ms => D3D11_RENDER_TARGET_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2DMS,
                        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                            Texture2DMS: D3D11_TEX2DMS_RTV {
                                UnusedField_NothingToDefine: 0,
                            },
                        },
                    },
                    Tex2DShape::Array => D3D11_RENDER_TARGET_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2DARRAY,
                        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                            Texture2DArray: D3D11_TEX2D_ARRAY_RTV {
                                MipSlice: t.MipSlice,
                                FirstArraySlice: t.FirstArraySlice,
                                ArraySize: t.ArraySize,
                            },
                        },
                    },
                    Tex2DShape::Plain => D3D11_RENDER_TARGET_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_RTV {
                                MipSlice: t.MipSlice,
                            },
                        },
                    },
                },
            )
        }
        RES_TEX3D => {
            let t = a.__bindgen_anon_1.Tex3D;
            Some(D3D11_RENDER_TARGET_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE3D,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Texture3D: D3D11_TEX3D_RTV {
                        MipSlice: t.MipSlice,
                        FirstWSlice: t.FirstW,
                        WSize: t.WSize,
                    },
                },
            })
        }
        // A cube render target IS a 2D array view; D3D11 has no cube RTV
        // dimension. Deliberately unlike `srv_desc`, which does have
        // TEXTURECUBE / TEXTURECUBEARRAY and branches on `NumCubes`.
        RES_TEXCUBE => {
            let t = a.__bindgen_anon_1.TexCube;
            Some(D3D11_RENDER_TARGET_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Texture2DArray: D3D11_TEX2D_ARRAY_RTV {
                        MipSlice: t.MipSlice,
                        FirstArraySlice: t.FirstArraySlice,
                        ArraySize: t.ArraySize,
                    },
                },
            })
        }
        // Every dimension D3D11 defines is handled above. The caller logs the
        // refusal with the dimension and format; never `unreachable!` —
        // `panic = "abort"` would take DWM down with it.
        _ => None,
    }
}

pub(crate) unsafe extern "C" fn destroy_rtv(_h: Hdevice, h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW) {
    release_rtv(h_rtv);
}

// --- Depth-stencil views ----------------------------------------------------

pub(crate) unsafe extern "C" fn calc_size_dsv(
    _h: Hdevice,
    _a: *const ddi::D3D11DDIARG_CREATEDEPTHSTENCILVIEW,
) -> u64 {
    8
}

pub(crate) unsafe extern "C" fn create_dsv(
    h: Hdevice,
    arg: *const ddi::D3D11DDIARG_CREATEDEPTHSTENCILVIEW,
    h_dsv: ddi::D3D10DDI_HDEPTHSTENCILVIEW,
    _h_rt: ddi::D3D10DDI_HRTDEPTHSTENCILVIEW,
) {
    clear_handle(h_dsv);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let Some(res) = load_resource(a.hDrvResource) else {
        log_error!("DDI create_dsv: resource handle empty");
        return;
    };
    let Some(desc) = dsv_desc(a, a.hDrvResource) else {
        log_error!(
            "DDI create_dsv: unsupported resource dimension {} fmt={}",
            a.ResourceDimension,
            a.Format
        );
        return;
    };
    let mut dsv: Option<ID3D11DepthStencilView> = None;
    let created = device.CreateDepthStencilView(&*res, Some(&desc), Some(&mut dsv));
    if let Err(ref e) = created {
        log_error!(
            "DDI create_dsv failed: dim={} fmt={} flags=0x{:x} {e:?}",
            a.ResourceDimension,
            a.Format,
            a.Flags
        );
    }
    finish_create(h, created, dsv, |v| {
        if VIEW_LOG_COUNT.first_n(128).is_some() {
            trace_line!(
                "DDI create_dsv ok: dim={} fmt={} flags=0x{:x}",
                a.ResourceDimension,
                a.Format,
                a.Flags
            );
        }
        store_com(h_dsv, v);
    });
}

pub(crate) unsafe fn dsv_desc(
    a: &ddi::D3D11DDIARG_CREATEDEPTHSTENCILVIEW,
    h_res: ddi::D3D10DDI_HRESOURCE,
) -> Option<D3D11_DEPTH_STENCIL_VIEW_DESC> {
    let format = DXGI_FORMAT(a.Format as i32);
    match a.ResourceDimension {
        RES_TEX1D => {
            let t = a.__bindgen_anon_1.Tex1D;
            Some(match tex1d_shape(t.ArraySize) {
                Tex1DShape::Array => D3D11_DEPTH_STENCIL_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_DSV_DIMENSION_TEXTURE1DARRAY,
                    Flags: a.Flags,
                    Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                        Texture1DArray: D3D11_TEX1D_ARRAY_DSV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                },
                Tex1DShape::Plain => D3D11_DEPTH_STENCIL_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_DSV_DIMENSION_TEXTURE1D,
                    Flags: a.Flags,
                    Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                        Texture1D: D3D11_TEX1D_DSV {
                            MipSlice: t.MipSlice,
                        },
                    },
                },
            })
        }
        RES_TEX2D => {
            let t = a.__bindgen_anon_1.Tex2D;
            Some(
                match tex2d_shape(t.ArraySize, resource_sample_count(h_res)) {
                    Tex2DShape::MsArray => D3D11_DEPTH_STENCIL_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2DMSARRAY,
                        Flags: a.Flags,
                        Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                            Texture2DMSArray: D3D11_TEX2DMS_ARRAY_DSV {
                                FirstArraySlice: t.FirstArraySlice,
                                ArraySize: t.ArraySize,
                            },
                        },
                    },
                    Tex2DShape::Ms => D3D11_DEPTH_STENCIL_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2DMS,
                        Flags: a.Flags,
                        Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                            Texture2DMS: D3D11_TEX2DMS_DSV {
                                UnusedField_NothingToDefine: 0,
                            },
                        },
                    },
                    Tex2DShape::Array => D3D11_DEPTH_STENCIL_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2DARRAY,
                        Flags: a.Flags,
                        Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                            Texture2DArray: D3D11_TEX2D_ARRAY_DSV {
                                MipSlice: t.MipSlice,
                                FirstArraySlice: t.FirstArraySlice,
                                ArraySize: t.ArraySize,
                            },
                        },
                    },
                    Tex2DShape::Plain => D3D11_DEPTH_STENCIL_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2D,
                        Flags: a.Flags,
                        Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_DSV {
                                MipSlice: t.MipSlice,
                            },
                        },
                    },
                },
            )
        }
        // As in `rtv_desc`: a cube depth-stencil view is a 2D array view.
        RES_TEXCUBE => {
            let t = a.__bindgen_anon_1.TexCube;
            Some(D3D11_DEPTH_STENCIL_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2DARRAY,
                Flags: a.Flags,
                Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                    Texture2DArray: D3D11_TEX2D_ARRAY_DSV {
                        MipSlice: t.MipSlice,
                        FirstArraySlice: t.FirstArraySlice,
                        ArraySize: t.ArraySize,
                    },
                },
            })
        }
        // No BUFFER and no TEX3D arm, and that is CORRECT, not missing: D3D11
        // defines neither `D3D11_DSV_DIMENSION_BUFFER` nor
        // `..._TEXTURE3D` — a buffer and a volume texture cannot be
        // depth-stencil targets. The caller logs the refused dimension.
        _ => None,
    }
}

pub(crate) unsafe extern "C" fn destroy_dsv(_h: Hdevice, h_dsv: ddi::D3D10DDI_HDEPTHSTENCILVIEW) {
    release_com(h_dsv);
}

pub(crate) unsafe extern "C" fn clear_rtv(
    h: Hdevice,
    h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW,
    color: *mut f32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(rtv) = load_rtv(h_rtv) else {
        return;
    };
    let rgba: [f32; 4] = if color.is_null() {
        [0.0; 4]
    } else {
        [*color, *color.add(1), *color.add(2), *color.add(3)]
    };
    {
        let (allocation, width, height, format, _resource_raw) = rtv_info(h_rtv);
        if allocation != 0 || width != 0 || height != 0 || format != 0 {
            if let Some(n) = CLEAR_RTV_LOG_COUNT.first_n_then_every_from_one(64, 512) {
                log_error!(
                    "DDI ClearRenderTargetView #{} alloc=0x{:x} {}x{} fmt={} rgba=({:.3},{:.3},{:.3},{:.3})",
                    n + 1,
                    allocation,
                    width,
                    height,
                    format,
                    rgba[0],
                    rgba[1],
                    rgba[2],
                    rgba[3]
                );
            }
        }
    }
    context.ClearRenderTargetView(&*rtv, &rgba);
}

pub(crate) unsafe extern "C" fn clear_dsv(
    h: Hdevice,
    h_dsv: ddi::D3D10DDI_HDEPTHSTENCILVIEW,
    flags: u32,
    depth: f32,
    stencil: u8,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(dsv) = load_com::<ID3D11DepthStencilView>(h_dsv) else {
        return;
    };
    context.ClearDepthStencilView(&*dsv, flags, depth, stencil);
}

// --- Shader resource views, samplers, constant buffers ----------------------

pub(crate) unsafe extern "C" fn calc_size_srv(
    _h: Hdevice,
    _a: *const ddi::D3D11DDIARG_CREATESHADERRESOURCEVIEW,
) -> u64 {
    8
}

pub(crate) unsafe extern "C" fn create_srv(
    h: Hdevice,
    arg: *const ddi::D3D11DDIARG_CREATESHADERRESOURCEVIEW,
    h_srv: ddi::D3D10DDI_HSHADERRESOURCEVIEW,
    _hrt: ddi::D3D10DDI_HRTSHADERRESOURCEVIEW,
) {
    clear_handle(h_srv);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let Some(res) = load_resource(a.hDrvResource) else {
        log_error!(
            "DDI create_srv: resource handle empty dim={} fmt={} hpriv={:p}",
            a.ResourceDimension,
            a.Format,
            a.hDrvResource.pDrvPrivate
        );
        return;
    };
    let Some(desc) = srv_desc(a, a.hDrvResource) else {
        log_error!(
            "DDI create_srv: unsupported resource dimension {} fmt={}",
            a.ResourceDimension,
            a.Format
        );
        return;
    };
    let mut srv: Option<ID3D11ShaderResourceView> = None;
    let created = device.CreateShaderResourceView(&*res, Some(&desc), Some(&mut srv));
    if let Err(ref e) = created {
        log_error!(
            "DDI create_srv failed: dim={} fmt={} {e:?}",
            a.ResourceDimension,
            a.Format
        );
    }
    finish_create(h, created, srv, |v| {
        let allocation = resource_allocation(a.hDrvResource);
        let n = SRV_CREATE_LOG_COUNT.next();
        if n < 1024 || allocation != 0 {
            let (width, height) = resource_dimensions(a.hDrvResource);
            trace_line!(
                "DDI create_srv ok: hpriv={:p} alloc=0x{:x} dim={} fmt={} {}x{}",
                h_srv.pDrvPrivate,
                allocation,
                a.ResourceDimension,
                a.Format,
                width,
                height
            );
        }
        store_com(h_srv, v);
    });
}

pub(crate) unsafe fn srv_desc(
    a: &ddi::D3D11DDIARG_CREATESHADERRESOURCEVIEW,
    h_res: ddi::D3D10DDI_HRESOURCE,
) -> Option<D3D11_SHADER_RESOURCE_VIEW_DESC> {
    let format = DXGI_FORMAT(a.Format as i32);
    match a.ResourceDimension {
        RES_BUFFER => {
            let b = a.__bindgen_anon_1.Buffer;
            Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_SRV_DIMENSION_BUFFER,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D11_BUFFER_SRV {
                        Anonymous1: D3D11_BUFFER_SRV_0 {
                            FirstElement: b.__bindgen_anon_1.FirstElement,
                        },
                        Anonymous2: D3D11_BUFFER_SRV_1 {
                            NumElements: b.__bindgen_anon_2.NumElements,
                        },
                    },
                },
            })
        }
        RES_BUFFEREX => {
            let b = a.__bindgen_anon_1.BufferEx;
            Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_SRV_DIMENSION_BUFFEREX,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    BufferEx: D3D11_BUFFEREX_SRV {
                        FirstElement: b.__bindgen_anon_1.FirstElement,
                        NumElements: b.__bindgen_anon_2.NumElements,
                        Flags: b.Flags,
                    },
                },
            })
        }
        RES_TEX1D => {
            let t = a.__bindgen_anon_1.Tex1D;
            Some(match tex1d_shape(t.ArraySize) {
                Tex1DShape::Array => D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURE1DARRAY,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture1DArray: D3D11_TEX1D_ARRAY_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                },
                Tex1DShape::Plain => D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURE1D,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture1D: D3D11_TEX1D_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                        },
                    },
                },
            })
        }
        RES_TEX2D => {
            let t = a.__bindgen_anon_1.Tex2D;
            Some(
                match tex2d_shape(t.ArraySize, resource_sample_count(h_res)) {
                    Tex2DShape::MsArray => D3D11_SHADER_RESOURCE_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2DMSARRAY,
                        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                            Texture2DMSArray: D3D11_TEX2DMS_ARRAY_SRV {
                                FirstArraySlice: t.FirstArraySlice,
                                ArraySize: t.ArraySize,
                            },
                        },
                    },
                    Tex2DShape::Ms => D3D11_SHADER_RESOURCE_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2DMS,
                        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                            Texture2DMS: D3D11_TEX2DMS_SRV {
                                UnusedField_NothingToDefine: 0,
                            },
                        },
                    },
                    Tex2DShape::Array => D3D11_SHADER_RESOURCE_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2DARRAY,
                        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                            Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                                MostDetailedMip: t.MostDetailedMip,
                                MipLevels: t.MipLevels,
                                FirstArraySlice: t.FirstArraySlice,
                                ArraySize: t.ArraySize,
                            },
                        },
                    },
                    Tex2DShape::Plain => D3D11_SHADER_RESOURCE_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_SRV {
                                MostDetailedMip: t.MostDetailedMip,
                                MipLevels: t.MipLevels,
                            },
                        },
                    },
                },
            )
        }
        RES_TEX3D => {
            let t = a.__bindgen_anon_1.Tex3D;
            Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_SRV_DIMENSION_TEXTURE3D,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture3D: D3D11_TEX3D_SRV {
                        MostDetailedMip: t.MostDetailedMip,
                        MipLevels: t.MipLevels,
                    },
                },
            })
        }
        // The one dimension where SRV genuinely differs from RTV/DSV: D3D11
        // has real cube SRV dimensions, so this branches on `NumCubes` rather
        // than collapsing to a 2D array view. Deliberately NOT routed through
        // `tex1d_shape`/`tex2d_shape` — the discriminant is a cube count, not
        // an array size, and conflating them is the confusion those two
        // functions exist to prevent.
        RES_TEXCUBE => {
            let t = a.__bindgen_anon_1.TexCube;
            if t.NumCubes > 1 {
                Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURECUBEARRAY,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        TextureCubeArray: D3D11_TEXCUBE_ARRAY_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                            First2DArrayFace: t.First2DArrayFace,
                            NumCubes: t.NumCubes,
                        },
                    },
                })
            } else {
                Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURECUBE,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        TextureCube: D3D11_TEXCUBE_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                        },
                    },
                })
            }
        }
        _ => None,
    }
}

pub(crate) unsafe extern "C" fn destroy_srv(_h: Hdevice, h_srv: ddi::D3D10DDI_HSHADERRESOURCEVIEW) {
    release_com(h_srv);
}

pub(crate) unsafe extern "C" fn gen_mips(h: Hdevice, h_srv: ddi::D3D10DDI_HSHADERRESOURCEVIEW) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(srv) = load_com::<ID3D11ShaderResourceView>(h_srv) else {
        return;
    };
    context.GenerateMips(&*srv);
}

pub(crate) unsafe extern "C" fn calc_size_uav(
    _h: Hdevice,
    _a: *const ddi::D3D11DDIARG_CREATEUNORDEREDACCESSVIEW,
) -> u64 {
    8
}

pub(crate) unsafe extern "C" fn create_uav(
    h: Hdevice,
    arg: *const ddi::D3D11DDIARG_CREATEUNORDEREDACCESSVIEW,
    h_uav: ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    _hrt: ddi::D3D11DDI_HRTUNORDEREDACCESSVIEW,
) {
    clear_handle(h_uav);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let Some(res) = load_resource(a.hDrvResource) else {
        return;
    };
    let Some(desc) = uav_desc(a) else {
        log_error!(
            "DDI create_uav: unsupported resource dimension {} fmt={}",
            a.ResourceDimension,
            a.Format
        );
        return;
    };
    let mut uav: Option<ID3D11UnorderedAccessView> = None;
    let created = device.CreateUnorderedAccessView(&*res, Some(&desc), Some(&mut uav));
    match created {
        Ok(()) => {}
        Err(ref e) => {
            let detail = match a.ResourceDimension {
                RES_BUFFER | RES_BUFFEREX => {
                    let b = a.__bindgen_anon_1.Buffer;
                    format!(
                        "buffer first={} num={} flags=0x{:x}",
                        b.FirstElement, b.NumElements, b.Flags
                    )
                }
                RES_TEX1D => {
                    let t = a.__bindgen_anon_1.Tex1D;
                    format!(
                        "tex1d mip={} first={} array={}",
                        t.MipSlice, t.FirstArraySlice, t.ArraySize
                    )
                }
                RES_TEX2D => {
                    let t = a.__bindgen_anon_1.Tex2D;
                    format!(
                        "tex2d mip={} first={} array={}",
                        t.MipSlice, t.FirstArraySlice, t.ArraySize
                    )
                }
                RES_TEX3D => {
                    let t = a.__bindgen_anon_1.Tex3D;
                    format!(
                        "tex3d mip={} first_w={} wsize={}",
                        t.MipSlice, t.FirstW, t.WSize
                    )
                }
                _ => String::from("unknown"),
            };
            log_error!(
                "DDI create_uav failed: dim={} fmt={} {} {e:?}",
                a.ResourceDimension,
                a.Format,
                detail
            );
        }
    }
    finish_create(h, created, uav, |v| {
        if VIEW_LOG_COUNT.first_n(256).is_some() {
            trace_line!(
                "DDI create_uav ok: dim={} fmt={} alloc=0x{:x}",
                a.ResourceDimension,
                a.Format,
                resource_allocation(a.hDrvResource)
            );
        }
        store_com(h_uav, v);
    });
}

pub(crate) unsafe fn uav_desc(
    a: &ddi::D3D11DDIARG_CREATEUNORDEREDACCESSVIEW,
) -> Option<D3D11_UNORDERED_ACCESS_VIEW_DESC> {
    let format = DXGI_FORMAT(a.Format as i32);
    match a.ResourceDimension {
        RES_BUFFER | RES_BUFFEREX => {
            let b = a.__bindgen_anon_1.Buffer;
            Some(D3D11_UNORDERED_ACCESS_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
                Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Buffer: D3D11_BUFFER_UAV {
                        FirstElement: b.FirstElement,
                        NumElements: b.NumElements,
                        Flags: b.Flags,
                    },
                },
            })
        }
        RES_TEX1D => {
            let t = a.__bindgen_anon_1.Tex1D;
            Some(match tex1d_shape(t.ArraySize) {
                Tex1DShape::Array => D3D11_UNORDERED_ACCESS_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_UAV_DIMENSION_TEXTURE1DARRAY,
                    Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Texture1DArray: D3D11_TEX1D_ARRAY_UAV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                },
                Tex1DShape::Plain => D3D11_UNORDERED_ACCESS_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_UAV_DIMENSION_TEXTURE1D,
                    Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Texture1D: D3D11_TEX1D_UAV {
                            MipSlice: t.MipSlice,
                        },
                    },
                },
            })
        }
        RES_TEX2D => {
            let t = a.__bindgen_anon_1.Tex2D;
            match tex2d_shape(t.ArraySize, NO_MULTISAMPLED_FORM) {
                Tex2DShape::Array => Some(D3D11_UNORDERED_ACCESS_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2DARRAY,
                    Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Texture2DArray: D3D11_TEX2D_ARRAY_UAV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                }),
                Tex2DShape::Plain => Some(D3D11_UNORDERED_ACCESS_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_UAV {
                            MipSlice: t.MipSlice,
                        },
                    },
                }),
                // Unreachable by construction, and stated rather than
                // dropped into a catch-all: `NO_MULTISAMPLED_FORM` is 1, so
                // `tex2d_shape` cannot return either MS shape here. If a
                // multisampled UAV ever becomes representable, this is the
                // arm the compiler will point at.
                Tex2DShape::Ms | Tex2DShape::MsArray => None,
            }
        }
        RES_TEX3D => {
            let t = a.__bindgen_anon_1.Tex3D;
            Some(D3D11_UNORDERED_ACCESS_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_UAV_DIMENSION_TEXTURE3D,
                Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Texture3D: D3D11_TEX3D_UAV {
                        MipSlice: t.MipSlice,
                        FirstWSlice: t.FirstW,
                        WSize: t.WSize,
                    },
                },
            })
        }
        // No TEXCUBE arm, and that is correct: D3D11 has no cube UAV
        // dimension. A cube resource reaches a compute shader as a 2D array
        // UAV, which the runtime asks for as RES_TEX2D.
        _ => None,
    }
}

pub(crate) unsafe extern "C" fn destroy_uav(
    _h: Hdevice,
    h_uav: ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
) {
    release_com(h_uav);
}

pub(crate) unsafe extern "C" fn clear_uav_uint(
    h: Hdevice,
    h_uav: ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    values: *const u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(uav) = load_com::<ID3D11UnorderedAccessView>(h_uav) else {
        return;
    };
    let v = if values.is_null() {
        [0u32; 4]
    } else {
        [*values, *values.add(1), *values.add(2), *values.add(3)]
    };
    context.ClearUnorderedAccessViewUint(&*uav, &v);
}

pub(crate) unsafe extern "C" fn clear_uav_float(
    h: Hdevice,
    h_uav: ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    values: *const f32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(uav) = load_com::<ID3D11UnorderedAccessView>(h_uav) else {
        return;
    };
    let v = if values.is_null() {
        [0.0f32; 4]
    } else {
        [*values, *values.add(1), *values.add(2), *values.add(3)]
    };
    context.ClearUnorderedAccessViewFloat(&*uav, &v);
}

pub(crate) unsafe extern "C" fn cs_set_uavs(
    h: Hdevice,
    start: u32,
    num: u32,
    uavs: *const ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    counts: *const u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let slice = DdiSlice::new(uavs, num);
    let mut out: Vec<Option<ID3D11UnorderedAccessView>> = Vec::with_capacity(num as usize);
    let mut nonnull = 0u32;
    let mut missing = 0u32;
    for i in 0..num as usize {
        out.push(match slice.as_ref().and_then(|s| s.get(i)) {
            Some(handle) => {
                let p = handle.pDrvPrivate;
                let view = load_com::<ID3D11UnorderedAccessView>(*handle).map(|m| (*m).clone());
                if view.is_some() {
                    nonnull += 1;
                } else if !p.is_null() {
                    missing += 1;
                }
                view
            }
            None => None,
        });
    }
    let n = UAV_BIND_LOG_COUNT.next();
    if n < 1024 || missing != 0 || slice.is_none() {
        trace_line!(
            "DDI CSSetUnorderedAccessViews start={} num={} nonnull={} missing={} uavs_null={} counts_ptr={}",
            start,
            num,
            nonnull,
            missing,
            slice.is_none(),
            !counts.is_null()
        );
    }
    context.CSSetUnorderedAccessViews(
        start,
        num,
        Some(out.as_ptr()),
        if counts.is_null() { None } else { Some(counts) },
    );
}

pub(crate) unsafe extern "C" fn copy_structure_count(
    h: Hdevice,
    h_dst: ddi::D3D10DDI_HRESOURCE,
    dst_offset: u32,
    h_src: ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(dst) = load_resource(h_dst).and_then(|r| (*r).cast::<ID3D11Buffer>().ok()) else {
        return;
    };
    let Some(src) = load_com::<ID3D11UnorderedAccessView>(h_src) else {
        return;
    };
    context.CopyStructureCount(&dst, dst_offset, &*src);
}

pub(crate) unsafe extern "C" fn calc_size_sampler(
    _h: Hdevice,
    _d: *const ddi::D3D10_DDI_SAMPLER_DESC,
) -> u64 {
    8
}

pub(crate) unsafe extern "C" fn create_sampler(
    h: Hdevice,
    desc: *const ddi::D3D10_DDI_SAMPLER_DESC,
    h_sampler: ddi::D3D10DDI_HSAMPLER,
    _hrt: ddi::D3D10DDI_HRTSAMPLER,
) {
    clear_handle(h_sampler);
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
    let created = device.CreateSamplerState(&sd, Some(&mut s));
    if let Err(ref e) = created {
        log_error!("DDI CreateSamplerState failed: {e:?}");
    }
    finish_create(h, created, s, |o| store_com(h_sampler, o));
}

pub(crate) unsafe extern "C" fn destroy_sampler(_h: Hdevice, h_sampler: ddi::D3D10DDI_HSAMPLER) {
    release_com(h_sampler);
}
