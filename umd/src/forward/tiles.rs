//! Tiled resources: the WDDM1.3 tile-mapping, tile-copy and tile-pool DDIs,
//! plus the debug markers that sit in the same version block.
//!
//! Moved verbatim out of `forward.rs` by T8/R1107.

use super::*;

pub(crate) fn tile_coord(
    coord: &ddi::D3DWDDM1_3DDI_TILED_RESOURCE_COORDINATE,
) -> D3D11_TILED_RESOURCE_COORDINATE {
    D3D11_TILED_RESOURCE_COORDINATE {
        X: coord.X,
        Y: coord.Y,
        Z: coord.Z,
        Subresource: coord.Subresource,
    }
}

pub(crate) fn tile_region(size: &ddi::D3DWDDM1_3DDI_TILE_REGION_SIZE) -> D3D11_TILE_REGION_SIZE {
    D3D11_TILE_REGION_SIZE {
        NumTiles: size.NumTiles,
        bUseBox: BOOL(size.bUseBox),
        Width: size.Width,
        Height: size.Height,
        Depth: size.Depth,
    }
}

pub(crate) unsafe fn tile_coords(
    ptr: *const ddi::D3DWDDM1_3DDI_TILED_RESOURCE_COORDINATE,
    count: u32,
) -> Vec<D3D11_TILED_RESOURCE_COORDINATE> {
    if ptr.is_null() || count == 0 {
        return Vec::new();
    }
    (0..count)
        .map(|i| tile_coord(&*ptr.add(i as usize)))
        .collect()
}

pub(crate) unsafe fn tile_regions(
    ptr: *const ddi::D3DWDDM1_3DDI_TILE_REGION_SIZE,
    count: u32,
) -> Vec<D3D11_TILE_REGION_SIZE> {
    if ptr.is_null() || count == 0 {
        return Vec::new();
    }
    (0..count)
        .map(|i| tile_region(&*ptr.add(i as usize)))
        .collect()
}

pub(crate) unsafe fn resource_as_buffer(
    h_resource: ddi::D3D10DDI_HRESOURCE,
) -> Option<ManuallyDrop<ID3D11Buffer>> {
    let res = load_resource(h_resource)?;
    (*res).cast::<ID3D11Buffer>().ok().map(ManuallyDrop::new)
}

pub(crate) unsafe extern "C" fn update_tile_mappings(
    h: Hdevice,
    h_tiled_resource: ddi::D3D10DDI_HRESOURCE,
    region_count: u32,
    region_start_coords: *const ddi::D3DWDDM1_3DDI_TILED_RESOURCE_COORDINATE,
    region_sizes: *const ddi::D3DWDDM1_3DDI_TILE_REGION_SIZE,
    h_tile_pool: ddi::D3D10DDI_HRESOURCE,
    range_count: u32,
    range_flags: *const u32,
    tile_pool_start_offsets: *const u32,
    range_tile_counts: *const u32,
    flags: u32,
) {
    let Some(context) = d3d11_context2(h) else {
        return;
    };
    let Some(tiled_resource) = load_resource(h_tiled_resource) else {
        return;
    };
    let coords = tile_coords(region_start_coords, region_count);
    let sizes = tile_regions(region_sizes, region_count);
    let tile_pool = resource_as_buffer(h_tile_pool);
    let tile_pool_ref: Option<&ID3D11Buffer> = tile_pool.as_ref().map(|p| &**p);
    let sizes_ptr = if sizes.is_empty() {
        None
    } else {
        Some(sizes.as_ptr())
    };
    let coords_ptr = if coords.is_empty() {
        None
    } else {
        Some(coords.as_ptr())
    };
    let _ = context.UpdateTileMappings(
        &*tiled_resource,
        region_count,
        coords_ptr,
        sizes_ptr,
        tile_pool_ref,
        range_count,
        (!range_flags.is_null()).then_some(range_flags),
        (!tile_pool_start_offsets.is_null()).then_some(tile_pool_start_offsets),
        (!range_tile_counts.is_null()).then_some(range_tile_counts),
        flags,
    );
}

pub(crate) unsafe extern "C" fn copy_tile_mappings(
    h: Hdevice,
    h_dst_resource: ddi::D3D10DDI_HRESOURCE,
    dst_start_coord: *const ddi::D3DWDDM1_3DDI_TILED_RESOURCE_COORDINATE,
    h_src_resource: ddi::D3D10DDI_HRESOURCE,
    src_start_coord: *const ddi::D3DWDDM1_3DDI_TILED_RESOURCE_COORDINATE,
    region_size: *const ddi::D3DWDDM1_3DDI_TILE_REGION_SIZE,
    flags: u32,
) {
    let Some(context) = d3d11_context2(h) else {
        return;
    };
    let Some(dst) = load_resource(h_dst_resource) else {
        return;
    };
    let Some(src) = load_resource(h_src_resource) else {
        return;
    };
    if dst_start_coord.is_null() || src_start_coord.is_null() || region_size.is_null() {
        return;
    }
    let dst_coord = tile_coord(&*dst_start_coord);
    let src_coord = tile_coord(&*src_start_coord);
    let size = tile_region(&*region_size);
    let _ = context.CopyTileMappings(&*dst, &dst_coord, &*src, &src_coord, &size, flags);
}

pub(crate) unsafe extern "C" fn copy_tiles(
    h: Hdevice,
    h_tiled_resource: ddi::D3D10DDI_HRESOURCE,
    region_start_coord: *const ddi::D3DWDDM1_3DDI_TILED_RESOURCE_COORDINATE,
    region_size: *const ddi::D3DWDDM1_3DDI_TILE_REGION_SIZE,
    h_buffer: ddi::D3D10DDI_HRESOURCE,
    buffer_start_offset: u64,
    flags: u32,
) {
    let Some(context) = d3d11_context2(h) else {
        return;
    };
    let Some(tiled_resource) = load_resource(h_tiled_resource) else {
        return;
    };
    let Some(buffer) = resource_as_buffer(h_buffer) else {
        return;
    };
    if region_start_coord.is_null() || region_size.is_null() {
        return;
    }
    let coord = tile_coord(&*region_start_coord);
    let size = tile_region(&*region_size);
    context.CopyTiles(
        &*tiled_resource,
        &coord,
        &size,
        &*buffer,
        buffer_start_offset,
        flags,
    );
}

pub(crate) unsafe extern "C" fn update_tiles(
    h: Hdevice,
    h_dst_resource: ddi::D3D10DDI_HRESOURCE,
    dst_start_coord: *const ddi::D3DWDDM1_3DDI_TILED_RESOURCE_COORDINATE,
    dst_region_size: *const ddi::D3DWDDM1_3DDI_TILE_REGION_SIZE,
    src_tile_data: *const c_void,
    flags: u32,
) {
    let Some(context) = d3d11_context2(h) else {
        return;
    };
    let Some(dst) = load_resource(h_dst_resource) else {
        return;
    };
    if dst_start_coord.is_null() || dst_region_size.is_null() || src_tile_data.is_null() {
        return;
    }
    let coord = tile_coord(&*dst_start_coord);
    let size = tile_region(&*dst_region_size);
    context.UpdateTiles(&*dst, &coord, &size, src_tile_data, flags);
}

pub(crate) unsafe fn tiled_barrier_child(
    handle_type: ddi::D3D11DDI_HANDLETYPE,
    handle: *mut c_void,
) -> Option<ID3D11DeviceChild> {
    match handle_type {
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_RESOURCE => {
            let res = load_resource_at(handle)?;
            (*res).cast::<ID3D11DeviceChild>().ok()
        }
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_SHADERRESOURCEVIEW => {
            let view = load_com_at::<ID3D11ShaderResourceView>(handle)?;
            (*view).cast::<ID3D11DeviceChild>().ok()
        }
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_RENDERTARGETVIEW => {
            let view = load_rtv_at(handle)?;
            (*view).cast::<ID3D11DeviceChild>().ok()
        }
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_DEPTHSTENCILVIEW => {
            let view = load_com_at::<ID3D11DepthStencilView>(handle)?;
            (*view).cast::<ID3D11DeviceChild>().ok()
        }
        ddi::D3D11DDI_HANDLETYPE_D3D11DDI_HT_UNORDEREDACCESSVIEW => {
            let view = load_com_at::<ID3D11UnorderedAccessView>(handle)?;
            (*view).cast::<ID3D11DeviceChild>().ok()
        }
        _ => None,
    }
}

pub(crate) unsafe extern "C" fn tiled_resource_barrier(
    h: Hdevice,
    before_type: ddi::D3D11DDI_HANDLETYPE,
    before: *mut c_void,
    after_type: ddi::D3D11DDI_HANDLETYPE,
    after: *mut c_void,
) {
    let Some(context) = d3d11_context2(h) else {
        return;
    };
    let before_child = tiled_barrier_child(before_type, before);
    let after_child = tiled_barrier_child(after_type, after);
    context.TiledResourceBarrier(before_child.as_ref(), after_child.as_ref());
}

pub(crate) unsafe extern "C" fn get_mip_packing(
    h: Hdevice,
    h_tiled_resource: ddi::D3D10DDI_HRESOURCE,
    packed_mips: *mut u32,
    tiles_for_packed_mips: *mut u32,
) {
    if !packed_mips.is_null() {
        *packed_mips = 0;
    }
    if !tiles_for_packed_mips.is_null() {
        *tiles_for_packed_mips = 0;
    }
    let Some(device) = d3d11_device2(h) else {
        return;
    };
    let Some(resource) = load_resource(h_tiled_resource) else {
        return;
    };
    let mut total_tiles = 0u32;
    let mut packed = D3D11_PACKED_MIP_DESC::default();
    let mut shape = D3D11_TILE_SHAPE::default();
    let mut subresource_count = 0u32;
    device.GetResourceTiling(
        &*resource,
        Some(&mut total_tiles),
        Some(&mut packed),
        Some(&mut shape),
        Some(&mut subresource_count),
        0,
        core::ptr::null_mut(),
    );
    if !packed_mips.is_null() {
        *packed_mips = packed.NumPackedMips as u32;
    }
    if !tiles_for_packed_mips.is_null() {
        *tiles_for_packed_mips = packed.NumTilesForPackedMips;
    }
}

pub(crate) unsafe extern "C" fn resize_tile_pool(
    h: Hdevice,
    h_tile_pool: ddi::D3D10DDI_HRESOURCE,
    new_size: u64,
) {
    let Some(context) = d3d11_context2(h) else {
        return;
    };
    let Some(tile_pool) = resource_as_buffer(h_tile_pool) else {
        return;
    };
    let _ = context.ResizeTilePool(&*tile_pool, new_size);
}

pub(crate) static WDDM13_MARKER_LOG_COUNT: LogThrottle = LogThrottle::new();

pub(crate) unsafe extern "C" fn set_marker(h: Hdevice) {
    if let Some(n) = WDDM13_MARKER_LOG_COUNT.first_n_then_every(16, 1024) {
        log_error!("WDDM1.3 SetMarker h={:p} hit={}", h.pDrvPrivate, n + 1);
    }
}

pub(crate) unsafe extern "C" fn set_marker_mode(
    h: Hdevice,
    marker_type: ddi::D3DWDDM1_3DDI_MARKER_TYPE,
    flags: u32,
) {
    if let Some(n) = WDDM13_MARKER_LOG_COUNT.first_n_then_every(16, 1024) {
        log_error!(
            "WDDM1.3 SetMarkerMode h={:p} type={} flags=0x{:x} hit={}",
            h.pDrvPrivate,
            marker_type,
            flags,
            n + 1
        );
    }
}
