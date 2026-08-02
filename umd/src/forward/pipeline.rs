//! Pipeline binding and the draw/dispatch entry points: render targets,
//! viewports, scissors, topology, the seven draws, stream-out, and compute.
//!
//! Moved verbatim out of `forward.rs` by T8/R1107.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(crate) unsafe extern "C" fn set_render_targets(
    h: Hdevice,
    rtvs: *const ddi::D3D10DDI_HRENDERTARGETVIEW,
    num_views: u32,
    _clear_slots: u32,
    dsv: ddi::D3D10DDI_HDEPTHSTENCILVIEW,
    uavs: *const ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    uav_counts: *const u32,
    uav_start: u32,
    num_uavs: u32,
    uav_range_start: u32,
    uav_range_size: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    // The RTV array was dereferenced unconditionally in the same function that
    // checks `uavs.is_null()` and logs a `uavs_null=` field.
    let rtv_slice = DdiSlice::new(rtvs, num_views);
    let mut views: Vec<Option<ID3D11RenderTargetView>> = Vec::with_capacity(num_views as usize);
    let mut rt0 = (0, 0, 0, 0, 0);
    let mut rt_nonnull = 0u32;
    let mut rt_missing = 0u32;
    for i in 0..num_views as usize {
        // An absent slot is spelled as a null handle rather than a null
        // pointer, so the readers below stay on the typed accessors.
        let h_rtv = rtv_slice
            .as_ref()
            .and_then(|s| s.get(i))
            .copied()
            .unwrap_or(ddi::D3D10DDI_HRENDERTARGETVIEW {
                pDrvPrivate: core::ptr::null_mut(),
            });
        if i == 0 {
            rt0 = rtv_info(h_rtv);
        }
        let view = load_rtv(h_rtv).map(|m| (*m).clone());
        if view.is_some() {
            rt_nonnull += 1;
        } else if !h_rtv.pDrvPrivate.is_null() {
            rt_missing += 1;
        }
        views.push(view);
    }
    if let Some(dev) = helios_device(h) {
        let bindings = &dev.owned.bindings;
        bindings.current_rt0_alloc.store(rt0.0, Ordering::Relaxed);
        bindings.current_rt0_width.store(rt0.1, Ordering::Relaxed);
        bindings.current_rt0_height.store(rt0.2, Ordering::Relaxed);
        bindings.current_rt0_format.store(rt0.3, Ordering::Relaxed);
    }
    let n = OM_LOG_COUNT.next();
    if n < 1024 || rt_missing != 0 || rt0.0 != 0 {
        trace_line!(
            "DDI OMSetRenderTargets num={} rt_nonnull={} rt_missing={} rt0_alloc=0x{:x} rt0={}x{} fmt={} dsv_raw=0x{:x} uav_start={} num_uavs={} uav_range={}:{}",
            num_views,
            rt_nonnull,
            rt_missing,
            rt0.0,
            rt0.1,
            rt0.2,
            rt0.3,
            handle_com_raw(dsv),
            uav_start,
            num_uavs,
            uav_range_start,
            uav_range_size
        );
    }
    let depth = load_com::<ID3D11DepthStencilView>(dsv).map(|m| (*m).clone());
    if num_uavs != 0 {
        let uav_slice = DdiSlice::new(uavs, num_uavs);
        let mut uav_views: Vec<Option<ID3D11UnorderedAccessView>> =
            Vec::with_capacity(num_uavs as usize);
        let mut uav_nonnull = 0u32;
        let mut uav_missing = 0u32;
        for i in 0..num_uavs as usize {
            uav_views.push(match uav_slice.as_ref().and_then(|s| s.get(i)) {
                Some(handle) => {
                    let p = handle.pDrvPrivate;
                    let view = load_com::<ID3D11UnorderedAccessView>(*handle).map(|m| (*m).clone());
                    if view.is_some() {
                        uav_nonnull += 1;
                    } else if !p.is_null() {
                        uav_missing += 1;
                    }
                    view
                }
                None => None,
            });
        }
        if n < 1024 || uav_missing != 0 || uav_slice.is_none() {
            trace_line!(
                "DDI OMSetRenderTargets UAV summary start={} num={} nonnull={} missing={} uavs_null={} counts_ptr={}",
                uav_start,
                num_uavs,
                uav_nonnull,
                uav_missing,
                uav_slice.is_none(),
                !uav_counts.is_null()
            );
        }
        context.OMSetRenderTargetsAndUnorderedAccessViews(
            Some(&views),
            depth.as_ref(),
            uav_start,
            num_uavs,
            Some(uav_views.as_ptr()),
            if uav_counts.is_null() {
                None
            } else {
                Some(uav_counts)
            },
        );
    } else {
        context.OMSetRenderTargets(Some(&views), depth.as_ref());
    }
}

pub(crate) unsafe extern "C" fn set_viewports(
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
    let n = VIEWPORT_LOG_COUNT.next();
    if n < 64 || num == 0 {
        if let Some(v) = out.first() {
            trace_line!(
                "DDI RSSetViewports num={} clear={} first=({},{} {}x{} depth={:.3}..{:.3})",
                num,
                _clear,
                v.TopLeftX,
                v.TopLeftY,
                v.Width,
                v.Height,
                v.MinDepth,
                v.MaxDepth
            );
        } else {
            trace_line!("DDI RSSetViewports num={} clear={} empty", num, _clear);
        }
    }
    context.RSSetViewports(Some(&out));
}

pub(crate) unsafe extern "C" fn set_scissor_rects(
    h: Hdevice,
    num: u32,
    _clear: u32,
    rects: *const ddi::D3D10_DDI_RECT,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let mut out: Vec<RECT> = Vec::with_capacity(num as usize);
    if !rects.is_null() {
        for i in 0..num as usize {
            let r = &*rects.add(i);
            out.push(RECT {
                left: r.left,
                top: r.top,
                right: r.right,
                bottom: r.bottom,
            });
        }
    }
    let n = SCISSOR_LOG_COUNT.next();
    if n < 64 || num == 0 {
        if let Some(r) = out.first() {
            trace_line!(
                "DDI RSSetScissorRects num={} clear={} first=({},{}-{}, {})",
                num,
                _clear,
                r.left,
                r.top,
                r.right,
                r.bottom
            );
        } else {
            trace_line!(
                "DDI RSSetScissorRects num={} clear={} empty rects_null={}",
                num,
                _clear,
                rects.is_null()
            );
        }
    }
    context.RSSetScissorRects(Some(&out));
}

pub(crate) unsafe extern "C" fn set_text_filter_size(_h: Hdevice, _w: u32, _hgt: u32) {
    note_ddi_refusal(&DDI_REFUSALS.text_filter_size_ignored);
}

pub(crate) unsafe extern "C" fn ia_set_topology(
    h: Hdevice,
    topo: ddi::D3D10_DDI_PRIMITIVE_TOPOLOGY,
) {
    if let Some(dev) = helios_device(h) {
        dev.owned
            .bindings
            .current_topology
            .store(topo as u32, Ordering::Relaxed);
    }
    if IA_BIND_LOG_COUNT.first_n(64).is_some() {
        trace_line!("DDI IASetTopology topo={}", topo as u32);
    }
    if let Some(context) = d3d11_context(h) {
        context.IASetPrimitiveTopology(windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY(
            topo as i32,
        ));
    }
}

pub(crate) unsafe fn log_draw_state(
    h: Hdevice,
    kind: &str,
    count0: u32,
    start0: u32,
    count1: u32,
    start1: u32,
) {
    // Gate the WHOLE function, not just the write: this runs from all seven
    // draw entry points, and with tracing off it used to pay an atomic
    // fetch_add on every draw plus a bindings read and a heap-allocating
    // 21-argument `format!` on the first 1024 draws and every 1024th after —
    // through the unconditional mutex-serialised writer. A game issuing 2000
    // draws per frame at 60 fps meant ~120 file writes per second from the draw
    // path after warm-up, and 1024 back-to-back writes during the first frames,
    // which is exactly the startup window idle-to-active latency is measured in.
    if !crate::trace_enabled() {
        return;
    }
    let n = DRAW_LOG_COUNT.next();
    if n >= 1024 && (n % 1024) != 0 {
        return;
    }
    let Some(dev) = helios_device(h) else {
        return;
    };
    let b = &dev.owned.bindings;
    trace_line!(
        "DDI {kind}: a={} b={} c={} d={} topo={} vb0=0x{:x}/{}+{} ib=0x{:x}/fmt{}+{} vs=0x{:x} ps=0x{:x} gs=0x{:x} hs=0x{:x} ds=0x{:x} rt0_alloc=0x{:x} rt0={}x{} fmt={} layout=0x{:x}",
        count0,
        start0,
        count1,
        start1,
        b.current_topology.load(Ordering::Relaxed),
        b.current_vb0.load(Ordering::Relaxed),
        b.current_vb0_stride.load(Ordering::Relaxed),
        b.current_vb0_offset.load(Ordering::Relaxed),
        b.current_ib.load(Ordering::Relaxed),
        b.current_ib_format.load(Ordering::Relaxed),
        b.current_ib_offset.load(Ordering::Relaxed),
        b.current_vs.load(Ordering::Relaxed),
        b.current_ps.load(Ordering::Relaxed),
        b.current_gs.load(Ordering::Relaxed),
        b.current_hs.load(Ordering::Relaxed),
        b.current_ds.load(Ordering::Relaxed),
        b.current_rt0_alloc.load(Ordering::Relaxed),
        b.current_rt0_width.load(Ordering::Relaxed),
        b.current_rt0_height.load(Ordering::Relaxed),
        b.current_rt0_format.load(Ordering::Relaxed),
        b.current_layout.load(Ordering::Relaxed)
    );
}

pub(crate) unsafe extern "C" fn draw(h: Hdevice, vertex_count: u32, start_vertex: u32) {
    bind_input_layout(h);
    log_draw_state(h, "Draw", vertex_count, start_vertex, 0, 0);
    if let Some(context) = d3d11_context(h) {
        context.Draw(vertex_count, start_vertex);
    }
}

pub(crate) unsafe extern "C" fn draw_indexed(
    h: Hdevice,
    index_count: u32,
    start_index: u32,
    base_vertex: i32,
) {
    bind_input_layout(h);
    log_draw_state(
        h,
        "DrawIndexed",
        index_count,
        start_index,
        base_vertex as u32,
        0,
    );
    if let Some(context) = d3d11_context(h) {
        context.DrawIndexed(index_count, start_index, base_vertex);
    }
}

pub(crate) unsafe extern "C" fn draw_instanced(
    h: Hdevice,
    vertex_count_per_instance: u32,
    instance_count: u32,
    start_vertex: u32,
    start_instance: u32,
) {
    bind_input_layout(h);
    log_draw_state(
        h,
        "DrawInstanced",
        vertex_count_per_instance,
        start_vertex,
        instance_count,
        start_instance,
    );
    if let Some(context) = d3d11_context(h) {
        context.DrawInstanced(
            vertex_count_per_instance,
            instance_count,
            start_vertex,
            start_instance,
        );
    }
}

pub(crate) unsafe extern "C" fn draw_indexed_instanced(
    h: Hdevice,
    index_count_per_instance: u32,
    instance_count: u32,
    start_index: u32,
    base_vertex: i32,
    start_instance: u32,
) {
    bind_input_layout(h);
    log_draw_state(
        h,
        "DrawIndexedInstanced",
        index_count_per_instance,
        start_index,
        instance_count,
        start_instance,
    );
    if let Some(context) = d3d11_context(h) {
        context.DrawIndexedInstanced(
            index_count_per_instance,
            instance_count,
            start_index,
            base_vertex,
            start_instance,
        );
    }
}

pub(crate) unsafe extern "C" fn draw_auto(h: Hdevice) {
    bind_input_layout(h);
    log_draw_state(h, "DrawAuto", 0, 0, 0, 0);
    if let Some(context) = d3d11_context(h) {
        context.DrawAuto();
    }
}

pub(crate) unsafe extern "C" fn draw_instanced_indirect(
    h: Hdevice,
    h_args: ddi::D3D10DDI_HRESOURCE,
    aligned_byte_offset: u32,
) {
    bind_input_layout(h);
    log_draw_state(h, "DrawInstancedIndirect", aligned_byte_offset, 0, 0, 0);
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let (alloc, kind, width, height, depth, fmt) = resource_summary(h_args);
    let Some(res) = load_resource(h_args) else {
        log_error!(
            "DDI DrawInstancedIndirect skipped: args missing alloc=0x{alloc:x} offset={aligned_byte_offset}"
        );
        return;
    };
    let Ok(buf) = (*res).cast::<ID3D11Buffer>() else {
        log_error!(
            "DDI DrawInstancedIndirect skipped: args not buffer kind={kind} dims={}x{}x{} fmt={} alloc=0x{alloc:x} offset={aligned_byte_offset}",
            width, height, depth, fmt
        );
        return;
    };
    if DRAW_LOG_COUNT.first_n(2048).is_some() {
        trace_line!(
            "DDI DrawInstancedIndirect args: alloc=0x{alloc:x} bytes={width} offset={aligned_byte_offset}"
        );
    }
    context.DrawInstancedIndirect(&buf, aligned_byte_offset);
}

pub(crate) unsafe extern "C" fn draw_indexed_instanced_indirect(
    h: Hdevice,
    h_args: ddi::D3D10DDI_HRESOURCE,
    aligned_byte_offset: u32,
) {
    bind_input_layout(h);
    log_draw_state(
        h,
        "DrawIndexedInstancedIndirect",
        aligned_byte_offset,
        0,
        0,
        0,
    );
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let (alloc, kind, width, height, depth, fmt) = resource_summary(h_args);
    let Some(res) = load_resource(h_args) else {
        log_error!(
            "DDI DrawIndexedInstancedIndirect skipped: args missing alloc=0x{alloc:x} offset={aligned_byte_offset}"
        );
        return;
    };
    let Ok(buf) = (*res).cast::<ID3D11Buffer>() else {
        log_error!(
            "DDI DrawIndexedInstancedIndirect skipped: args not buffer kind={kind} dims={}x{}x{} fmt={} alloc=0x{alloc:x} offset={aligned_byte_offset}",
            width, height, depth, fmt
        );
        return;
    };
    if DRAW_LOG_COUNT.first_n(2048).is_some() {
        trace_line!(
            "DDI DrawIndexedInstancedIndirect args: alloc=0x{alloc:x} bytes={width} offset={aligned_byte_offset}"
        );
    }
    context.DrawIndexedInstancedIndirect(&buf, aligned_byte_offset);
}

pub(crate) unsafe extern "C" fn so_set_targets(
    h: Hdevice,
    num: u32,
    _clear: u32,
    buffers: *const ddi::D3D10DDI_HRESOURCE,
    offsets: *const u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    // Was: skip the fill loop for a null array but still pass `num` and
    // `out.as_ptr()`. `out.len()` was then 0 with `out.capacity() == num`, so
    // DXVK read `num` uninitialised words as ID3D11Buffer* and AddRef'd them.
    // `offsets` was passed as `Some(..)` with no null check while every sibling
    // path checked theirs.
    let out = collect_slots(DdiSlice::new(buffers, num), num, |handle| {
        load_resource(*handle).and_then(|r| (*r).cast::<ID3D11Buffer>().ok())
    });
    context.SOSetTargets(
        num,
        Some(out.as_ptr()),
        if offsets.is_null() {
            None
        } else {
            Some(offsets)
        },
    );
}

// --- Compute ---------------------------------------------------------------

pub(crate) unsafe extern "C" fn dispatch(h: Hdevice, x: u32, y: u32, z: u32) {
    if DISPATCH_LOG_COUNT.first_n_then_every(1024, 1024).is_some() {
        if let Some(dev) = helios_device(h) {
            let b = &dev.owned.bindings;
            trace_line!(
                "DDI Dispatch x={} y={} z={} cs=0x{:x} rt0_alloc=0x{:x} rt0={}x{} fmt={}",
                x,
                y,
                z,
                b.current_cs.load(Ordering::Relaxed),
                b.current_rt0_alloc.load(Ordering::Relaxed),
                b.current_rt0_width.load(Ordering::Relaxed),
                b.current_rt0_height.load(Ordering::Relaxed),
                b.current_rt0_format.load(Ordering::Relaxed)
            );
        }
    }
    if let Some(context) = d3d11_context(h) {
        context.Dispatch(x, y, z);
    }
}

pub(crate) unsafe extern "C" fn dispatch_indirect(
    h: Hdevice,
    h_args: ddi::D3D10DDI_HRESOURCE,
    aligned_byte_offset: u32,
) {
    if DISPATCH_LOG_COUNT.first_n_then_every(1024, 1024).is_some() {
        trace_line!(
            "DDI DispatchIndirect args_alloc=0x{:x} offset={}",
            resource_allocation(h_args),
            aligned_byte_offset
        );
    }
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(buf) = load_resource(h_args).and_then(|r| (*r).cast::<ID3D11Buffer>().ok()) else {
        return;
    };
    context.DispatchIndirect(&buf, aligned_byte_offset);
}

pub(crate) unsafe extern "C" fn set_resource_min_lod(
    h: Hdevice,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    min_lod: f32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(res) = load_resource(h_resource) else {
        return;
    };
    context.SetResourceMinLOD(&*res, min_lod);
}
