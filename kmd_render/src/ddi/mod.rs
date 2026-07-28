//! WDDM DDI entry points, grouped by subsystem. `lib.rs` wires these into the
//! `DRIVER_INITIALIZATION_DATA` table.

// `PASSIVE_LEVEL_IRQL` now lives with the proof token that shares its subject
// (`crate::irql`), so the constant and the type that means "we are at that
// level" cannot drift apart. Re-exported here because the four existing
// `crate::ddi::PASSIVE_LEVEL_IRQL` users are all in this subtree.
pub(crate) use crate::irql::PASSIVE_LEVEL_IRQL;

mod add_device;
mod bar_segment;
mod base;
mod blob_map;
mod build_paging_buffer;
mod child;
pub(crate) mod cpu_host_aperture;
pub(crate) mod create_allocation;
pub(crate) mod display;
mod escape;
mod gpummu;
mod lifecycle;
pub(crate) mod hpd;
pub(crate) mod interrupt;
mod present_packet;
pub(crate) mod query_adapter_info;
mod scheduler;
pub(crate) mod segment_table;
pub(crate) mod submit_command;
pub(crate) mod vidpn;
pub(crate) mod wddm_surface;

pub use add_device::dxgkddi_add_device;
pub use blob_map::unmap_io_pages_from_user;
pub(crate) use build_paging_buffer::PagingPteShadow;
pub use build_paging_buffer::{
    diag_dump_gpummu_atomics, dxgkddi_build_paging_buffer, dxgkddi_get_root_page_table_size,
    dxgkddi_set_root_page_table,
};
pub use cpu_host_aperture::{
    diag_dump_cpu_host_atomics, dxgkddi_map_cpu_host_aperture, dxgkddi_unmap_cpu_host_aperture,
};
pub use create_allocation::{
    dxgkddi_close_allocation, dxgkddi_create_allocation, dxgkddi_describe_allocation,
    dxgkddi_destroy_allocation, dxgkddi_get_standard_allocation_driver_data,
    dxgkddi_open_allocation,
};
pub(crate) use display::VIDPN_SOURCE_ADDRESS_COUNT;
pub use display::{
    diag_dump_present_atomics, dxgkddi_commit_vidpn, dxgkddi_enum_vidpn_cofunc_modality,
    dxgkddi_exchange_pre_start_info, dxgkddi_get_scan_line, dxgkddi_is_supported_vidpn,
    dxgkddi_present, dxgkddi_query_vidpn_hw_capability, dxgkddi_recommend_functional_vidpn,
    dxgkddi_recommend_monitor_modes, dxgkddi_set_pointer_position, dxgkddi_set_pointer_shape,
    dxgkddi_set_vidpn_source_address, dxgkddi_set_vidpn_source_visibility,
    dxgkddi_stop_device_and_release_post_display_ownership, dxgkddi_system_display_enable,
    dxgkddi_system_display_write, dxgkddi_update_active_vidpn_present_path,
    dxgkddi_update_monitor_link_info,
};
pub use escape::dxgkddi_escape;
pub use interrupt::{dxgkddi_control_interrupt, dxgkddi_dpc_routine, dxgkddi_interrupt_routine};
pub use query_adapter_info::{dxgkddi_get_node_metadata, dxgkddi_query_adapter_info};
pub use scheduler::{
    dxgkddi_calibrate_gpu_clock, dxgkddi_cancel_command, dxgkddi_create_hw_context,
    dxgkddi_create_hw_queue, dxgkddi_destroy_hw_context, dxgkddi_destroy_hw_queue,
    dxgkddi_format_history_buffer, dxgkddi_power_runtime_control_request,
    dxgkddi_power_runtime_set_device_handle, dxgkddi_present_to_hw_queue,
    dxgkddi_query_dependent_engine_group, dxgkddi_query_engine_status, dxgkddi_reset_engine,
    dxgkddi_set_stable_power_state, dxgkddi_set_virtual_machine_data,
    dxgkddi_submit_command_to_hw_queue, dxgkddi_switch_to_hw_context_list,
};
pub use base::{
    dxgkddi_control_etw_logging, dxgkddi_notify_acpi_event, dxgkddi_query_interface,
    dxgkddi_reset_device, dxgkddi_unload,
};
pub use child::{
    dxgkddi_get_child_container_id, dxgkddi_query_child_relations, dxgkddi_query_child_status,
    dxgkddi_query_device_descriptor,
};
pub use lifecycle::{
    dxgkddi_dispatch_io_request, dxgkddi_remove_device, dxgkddi_set_power_state,
    dxgkddi_start_device, dxgkddi_stop_device,
};
pub use submit_command::{
    diag_dump_engine_atomics, dxgkddi_collect_dbg_info, dxgkddi_patch, dxgkddi_preempt_command,
    dxgkddi_query_current_fence, dxgkddi_render, dxgkddi_render_gdi, dxgkddi_render_km,
    dxgkddi_reset_from_timeout, dxgkddi_restart_from_timeout, dxgkddi_submit_command,
    dxgkddi_submit_command_virtual,
};
pub(crate) use submit_command::{
    abandon_pending_submissions, record_present_handoff_telemetry, AbandonOutcome,
    ABANDONED_FENCES, DMA_STALE_SKIP_COUNT,
};
