//! The four device-funcs table writers.
//!
//! Last in the split because it names every DDI in the crate: `install`,
//! `install_11_1`, `install_wddm1_3` and the three DXGI base-function
//! installers, plus the `Filled*` proof tokens they return.
//!
//! Moved verbatim out of `forward.rs` by T8/R1107.

use super::*;

/// Install typed DXGI base-DDI handlers over the stub fill.
pub unsafe fn install_dxgi(funcs: *mut ddi::DXGI_DDI_BASE_FUNCTIONS) {
    let f = &mut *funcs;
    f.pfnPresent = Some(dxgi_present);
    f.pfnGetGammaCaps = Some(dxgi_get_gamma_caps);
    f.pfnSetDisplayMode = Some(dxgi_set_display_mode);
    f.pfnSetResourcePriority = Some(dxgi_set_resource_priority);
    f.pfnQueryResourceResidency = Some(dxgi_query_resource_residency);
    f.pfnRotateResourceIdentities = Some(dxgi_rotate_resource_identities);
    f.pfnBlt = Some(dxgi_blt);
}

pub unsafe fn install_dxgi_1_1(funcs: *mut ddi::DXGI1_1_DDI_BASE_FUNCTIONS) {
    let f = &mut *funcs;
    f.pfnResolveSharedResource = Some(dxgi_resolve_shared_resource);
}

pub unsafe fn install_dxgi_1_3(funcs: *mut ddi::DXGI1_3_DDI_BASE_FUNCTIONS) {
    let f = &mut *funcs;
    f.pfnBlt1 = Some(dxgi_blt1);
    f.pfnOfferResources = Some(dxgi_offer_resources);
    f.pfnReclaimResources = Some(dxgi_reclaim_resources);
    f.pfnGetMultiplaneOverlayCaps = Some(dxgi_get_mpo_caps);
    f.pfnGetMultiplaneOverlayGroupCaps = Some(dxgi_get_mpo_group_caps);
    f.pfnReserved1 = Some(dxgi_reserved_unsupported);
    f.pfnPresentMultiplaneOverlay = Some(dxgi_present_mpo);
    f.pfnReserved2 = Some(dxgi_reserved_unsupported);
    f.pfnPresent1 = Some(dxgi_present1);
    f.pfnCheckPresentDurationSupport = Some(dxgi_check_present_duration_support);
}

/// Install the implemented forwarders into the device-funcs table (over the
/// stub fill). Uses the real bindgen PFN field types — no transmute.
/// Proof that [`install`] has run over a table: its 10.x-typed forwarders are
/// in place, including the eighteen slots [`install_11_1`] must overwrite.
///
/// R1009. Correctness of every >=11.1 device rested on TEXTUAL CALL ORDER
/// inside `device_funcs.rs`: `install()` writes 10.x-typed handlers into slots
/// that `install_11_1()` must run AFTERWARDS to replace. The 11.1 blend
/// descriptor inserts `LogicOpEnable` mid-struct, so a 10.x reader returns the
/// wrong write mask -- wrong blending for DWM, no counter, no log, only pixels
/// -- and the untyped-shader-create form of the same class has already shipped
/// once (VUID-Input-08733).
///
/// These tokens make the ordering structural. `install_11_1` cannot be called
/// without the value `install` returns, so
/// `install_11_1(f); install(f);` no longer compiles.
#[must_use]
pub struct Filled11_0(());

/// Proof that [`install_11_1`] has replaced the eighteen 10.x-typed slots.
#[must_use]
pub struct Filled11_1(());

/// Proof that [`install_wddm1_3`] has run. Terminal: nothing consumes it, and
/// it exists so the chain reads as one pipeline rather than two links and a
/// loose call. T6/R918 deleted the WDDM2.1 level above it -- the runtime could
/// never negotiate that interface, so there is no `upgrade_wddm2_1`.
#[must_use]
pub struct FilledWddm1_3(());

pub unsafe fn install(funcs: *mut ddi::D3D11DDI_DEVICEFUNCS) -> Filled11_0 {
    let f = &mut *funcs;
    f.pfnCalcPrivateResourceSize = Some(calc_size_resource);
    f.pfnCalcPrivateOpenedResourceSize = Some(calc_size_opened_resource);
    f.pfnCreateResource = Some(create_resource);
    f.pfnOpenResource = Some(open_resource);
    f.pfnDestroyResource = Some(destroy_resource);
    f.pfnCalcPrivateRenderTargetViewSize = Some(calc_size_rtv);
    f.pfnCreateRenderTargetView = Some(create_rtv);
    f.pfnDestroyRenderTargetView = Some(destroy_rtv);
    f.pfnClearRenderTargetView = Some(clear_rtv);
    f.pfnCalcPrivateDepthStencilViewSize = Some(calc_size_dsv);
    f.pfnCreateDepthStencilView = Some(create_dsv);
    f.pfnDestroyDepthStencilView = Some(destroy_dsv);
    f.pfnClearDepthStencilView = Some(clear_dsv);
    f.pfnResourceCopy = Some(resource_copy);
    f.pfnResourceCopyRegion = Some(resource_copy_region);
    f.pfnResourceConvert = Some(resource_copy);
    f.pfnResourceConvertRegion = Some(resource_copy_region);
    f.pfnResourceResolveSubresource = Some(resource_resolve_subresource);
    f.pfnResourceIsStagingBusy = Some(resource_is_staging_busy);
    f.pfnResourceMap = Some(resource_map);
    f.pfnResourceUnmap = Some(resource_unmap);
    f.pfnDynamicIABufferMapNoOverwrite = Some(resource_map);
    f.pfnDynamicIABufferUnmap = Some(resource_unmap);
    f.pfnDynamicConstantBufferMapDiscard = Some(resource_map);
    f.pfnDynamicIABufferMapDiscard = Some(resource_map);
    f.pfnDynamicConstantBufferUnmap = Some(resource_unmap);
    f.pfnDynamicResourceMapDiscard = Some(resource_map);
    f.pfnDynamicResourceUnmap = Some(resource_unmap);
    f.pfnStagingResourceMap = Some(resource_map);
    f.pfnStagingResourceUnmap = Some(resource_unmap);
    f.pfnShaderResourceViewReadAfterWriteHazard = Some(srv_read_after_write_hazard);
    f.pfnResourceReadAfterWriteHazard = Some(resource_read_after_write_hazard);
    f.pfnFlush = Some(flush);

    // Shaders + pipeline.
    f.pfnCalcPrivateShaderSize = Some(calc_size_shader);
    f.pfnCreateVertexShader = Some(create_vertex_shader);
    f.pfnCreateGeometryShader = Some(create_geometry_shader);
    f.pfnCreatePixelShader = Some(create_pixel_shader);
    f.pfnCalcPrivateGeometryShaderWithStreamOutput = Some(calc_size_geometry_shader_so);
    f.pfnCreateGeometryShaderWithStreamOutput = Some(create_geometry_shader_so);
    f.pfnCalcPrivateTessellationShaderSize = Some(calc_size_tess_shader);
    f.pfnCreateHullShader = Some(create_hull_shader);
    f.pfnCreateDomainShader = Some(create_domain_shader);
    f.pfnCreateComputeShader = Some(create_compute_shader);
    f.pfnDestroyShader = Some(destroy_shader);
    f.pfnVsSetShader = Some(vs_set_shader);
    f.pfnPsSetShader = Some(ps_set_shader);
    f.pfnGsSetShader = Some(gs_set_shader);
    f.pfnHsSetShader = Some(hs_set_shader);
    f.pfnDsSetShader = Some(ds_set_shader);
    f.pfnCsSetShader = Some(cs_set_shader);
    f.pfnPsSetShaderWithIfaces = Some(ps_set_shader_with_ifaces);
    f.pfnVsSetShaderWithIfaces = Some(vs_set_shader_with_ifaces);
    f.pfnGsSetShaderWithIfaces = Some(gs_set_shader_with_ifaces);
    f.pfnHsSetShaderWithIfaces = Some(hs_set_shader_with_ifaces);
    f.pfnDsSetShaderWithIfaces = Some(ds_set_shader_with_ifaces);
    f.pfnCsSetShaderWithIfaces = Some(cs_set_shader_with_ifaces);
    f.pfnSetRenderTargets = Some(set_render_targets);
    f.pfnSetViewports = Some(set_viewports);
    f.pfnSetScissorRects = Some(set_scissor_rects);
    f.pfnIaSetTopology = Some(ia_set_topology);
    f.pfnDraw = Some(draw);
    f.pfnDrawIndexed = Some(draw_indexed);
    f.pfnDrawInstanced = Some(draw_instanced);
    f.pfnDrawIndexedInstanced = Some(draw_indexed_instanced);
    f.pfnDrawAuto = Some(draw_auto);
    f.pfnDrawInstancedIndirect = Some(draw_instanced_indirect);
    f.pfnDrawIndexedInstancedIndirect = Some(draw_indexed_instanced_indirect);
    f.pfnSoSetTargets = Some(so_set_targets);
    f.pfnSetTextFilterSize = Some(set_text_filter_size);

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
    f.pfnGsSetConstantBuffers = Some(gs_set_constant_buffers);
    f.pfnHsSetConstantBuffers = Some(hs_set_constant_buffers);
    f.pfnDsSetConstantBuffers = Some(ds_set_constant_buffers);
    f.pfnCsSetConstantBuffers = Some(cs_set_constant_buffers);
    f.pfnPsSetShaderResources = Some(ps_set_shader_resources);
    f.pfnVsSetShaderResources = Some(vs_set_shader_resources);
    f.pfnGsSetShaderResources = Some(gs_set_shader_resources);
    f.pfnHsSetShaderResources = Some(hs_set_shader_resources);
    f.pfnDsSetShaderResources = Some(ds_set_shader_resources);
    f.pfnCsSetShaderResources = Some(cs_set_shader_resources);
    f.pfnPsSetSamplers = Some(ps_set_samplers);
    f.pfnVsSetSamplers = Some(vs_set_samplers);
    f.pfnGsSetSamplers = Some(gs_set_samplers);
    f.pfnHsSetSamplers = Some(hs_set_samplers);
    f.pfnDsSetSamplers = Some(ds_set_samplers);
    f.pfnCsSetSamplers = Some(cs_set_samplers);
    f.pfnResourceUpdateSubresourceUP = Some(resource_update_subresource);
    f.pfnDefaultConstantBufferUpdateSubresourceUP = Some(resource_update_subresource);
    f.pfnGenMips = Some(gen_mips);
    f.pfnCheckFormatSupport = Some(check_format_support);
    f.pfnCheckMultisampleQualityLevels = Some(check_multisample_quality_levels);
    f.pfnCheckCounterInfo = Some(check_counter_info);
    f.pfnCheckCounter = Some(check_counter);

    // Queries and predication.
    f.pfnCalcPrivateQuerySize = Some(calc_size_query);
    f.pfnCreateQuery = Some(create_query);
    f.pfnDestroyQuery = Some(destroy_query);
    f.pfnQueryBegin = Some(query_begin);
    f.pfnQueryEnd = Some(query_end);
    f.pfnQueryGetData = Some(query_get_data);
    f.pfnSetPredication = Some(set_predication);

    // D3D11 UAV/compute paths.
    f.pfnCalcPrivateUnorderedAccessViewSize = Some(calc_size_uav);
    f.pfnCreateUnorderedAccessView = Some(create_uav);
    f.pfnDestroyUnorderedAccessView = Some(destroy_uav);
    f.pfnClearUnorderedAccessViewUint = Some(clear_uav_uint);
    f.pfnClearUnorderedAccessViewFloat = Some(clear_uav_float);
    f.pfnCsSetUnorderedAccessViews = Some(cs_set_uavs);
    f.pfnCopyStructureCount = Some(copy_structure_count);
    f.pfnDispatch = Some(dispatch);
    f.pfnDispatchIndirect = Some(dispatch_indirect);
    f.pfnSetResourceMinLOD = Some(set_resource_min_lod);

    // Deferred contexts + command lists (Phase C). Real slots, installed
    // unconditionally — the UmdCommandLists knob gates only the THREADING
    // caps bit that invites the runtime to call them (the size family is
    // installed by `install_calc_and_lifecycle` beside the other Calc*).
    f.pfnCreateDeferredContext = Some(create_deferred_context);
    f.pfnCreateCommandList = Some(create_command_list);
    f.pfnDestroyCommandList = Some(destroy_command_list);
    f.pfnCommandListExecute = Some(command_list_execute);
    f.pfnAbandonCommandList = Some(abandon_command_list);
    f.pfnRecycleCommandList = Some(recycle_command_list);
    f.pfnRecycleCreateCommandList = Some(recycle_create_command_list);
    f.pfnRecycleCreateDeferredContext = Some(recycle_create_deferred_context);
    f.pfnRecycleDestroyCommandList = Some(destroy_command_list);

    // Input layouts (lazy), vertex/index buffers, blend state.
    f.pfnCalcPrivateElementLayoutSize = Some(calc_size_element_layout);
    f.pfnCreateElementLayout = Some(create_element_layout);
    f.pfnDestroyElementLayout = Some(destroy_element_layout);
    f.pfnIaSetInputLayout = Some(ia_set_input_layout);
    f.pfnIaSetVertexBuffers = Some(ia_set_vertex_buffers);
    f.pfnIaSetIndexBuffer = Some(ia_set_index_buffer);
    f.pfnCalcPrivateBlendStateSize = Some(calc_size_blend);
    f.pfnCreateBlendState = Some(create_blend_state);
    f.pfnSetBlendState = Some(set_blend_state);
    f.pfnDestroyBlendState = Some(destroy_blend_state);
    Filled11_0(())
}

/// Install D3D11.1-specific handlers whose signatures differ from the D3D11.0
/// prefix or only exist in the D3D11.1 table.
pub unsafe fn install_11_1(
    base: Filled11_0,
    funcs: *mut ddi::D3D11_1DDI_DEVICEFUNCS,
) -> Filled11_1 {
    // Consumed by value: this is the whole point. The 10.x handlers must
    // already be in the table for these overrides to be overrides.
    let Filled11_0(()) = base;
    let f = &mut *funcs;
    f.pfnVsSetConstantBuffers = Some(vs_set_constant_buffers1);
    f.pfnPsSetConstantBuffers = Some(ps_set_constant_buffers1);
    f.pfnGsSetConstantBuffers = Some(gs_set_constant_buffers1);
    f.pfnHsSetConstantBuffers = Some(hs_set_constant_buffers1);
    f.pfnDsSetConstantBuffers = Some(ds_set_constant_buffers1);
    f.pfnCsSetConstantBuffers = Some(cs_set_constant_buffers1);
    f.pfnFlush = Some(flush_11_1);
    f.pfnResourceCopyRegion = Some(resource_copy_region_11_1);
    f.pfnResourceConvertRegion = Some(resource_copy_region_11_1);
    f.pfnResourceUpdateSubresourceUP = Some(resource_update_subresource_11_1);
    f.pfnDefaultConstantBufferUpdateSubresourceUP = Some(resource_update_subresource_11_1);
    f.pfnDiscard = Some(discard_11_1);
    // Only >=11.1 tables have this slot, so `install()` (written against the
    // 11.0 shape) cannot set it and it was left on the uniform no-op stub.
    // Nothing gated it either: D3D11_1DDI_D3D11_OPTIONS_DATA carries only
    // OutputMergerLogicOp and AssignDebugBinarySupport, so every >=11.1 device
    // — dwm negotiates WDDM1.3 — exposed the feature with a handler that never
    // wrote the caller's MAPPED_SUBRESOURCE.
    f.pfnDynamicConstantBufferMapNoOverwrite = Some(dynamic_cb_map_no_overwrite);
    f.pfnCheckDirectFlipSupport = Some(check_direct_flip_support_11_1);
    f.pfnClearView = Some(clear_view_11_1);
    // The >=11.1 tables pass D3D11_1_DDI_BLEND_DESC (LogicOpEnable/LogicOp
    // inserted mid-struct) — `install()`'s 10.1-desc handlers misread the
    // write mask (see create_blend_state_11_1). NOTE the 11.1 rasterizer desc
    // only APPENDS ForcedSampleCount, so the shared 10.x reader stays valid
    // for pfnCreateRasterizerState.
    f.pfnCalcPrivateBlendStateSize = Some(calc_size_blend_11_1);
    f.pfnCreateBlendState = Some(create_blend_state_11_1);
    // The >=11.1 shader creates carry TYPED signature entries
    // (D3D11_1DDIARG_SIGNATURE_ENTRY2.RegisterComponentType); forward them so
    // dxbc-spv declares correctly-typed shader I/O instead of assuming
    // float32 for everything. Hull/domain use a different 11.1 tessellation
    // signatures struct, so override those ABI-specific slots as well.
    f.pfnCreateVertexShader = Some(create_vertex_shader_11_1);
    f.pfnCreatePixelShader = Some(create_pixel_shader_11_1);
    f.pfnCreateGeometryShader = Some(create_geometry_shader_11_1);
    f.pfnCalcPrivateTessellationShaderSize = Some(calc_size_tess_shader_11_1);
    f.pfnCreateHullShader = Some(create_hull_shader_11_1);
    f.pfnCreateDomainShader = Some(create_domain_shader_11_1);
    Filled11_1(())
}

pub unsafe fn install_wddm1_3(
    level_11_1: Filled11_1,
    funcs: *mut ddi::D3DWDDM1_3DDI_DEVICEFUNCS,
) -> FilledWddm1_3 {
    let Filled11_1(()) = level_11_1;
    let f = &mut *funcs;
    f.pfnCheckMultisampleQualityLevels = Some(check_multisample_quality_levels_wddm1_3);
    f.pfnUpdateTileMappings = Some(update_tile_mappings);
    f.pfnCopyTileMappings = Some(copy_tile_mappings);
    f.pfnCopyTiles = Some(copy_tiles);
    f.pfnUpdateTiles = Some(update_tiles);
    f.pfnTiledResourceBarrier = Some(tiled_resource_barrier);
    f.pfnGetMipPacking = Some(get_mip_packing);
    f.pfnResizeTilePool = Some(resize_tile_pool);
    f.pfnSetMarker = Some(set_marker);
    f.pfnSetMarkerMode = Some(set_marker_mode);
    FilledWddm1_3(())
}
