//! Helios vGPU kernel-mode driver (KMD).
//!
//! A WDDM 3.2 render-only display miniport driver for the virtio-gpu device
//! (VEN_1AF4 & DEV_1050). `DriverEntry` registers our DDI table with Dxgkrnl via
//! `DxgkInitialize`; from there Dxgkrnl drives the device lifecycle through the
//! callbacks below.
//!
//! Implementation status: Gate 1 bring-up. This crate is intentionally separate
//! from the working System-class `kmd` crate. It starts the WDDM render miniport
//! path and bindgen/build plumbing without pretending the render, paging,
//! scheduler, or UMD contracts are implemented.

#![no_std]
#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]

extern crate alloc;

// wdk-panic supplies the kernel `#[panic_handler]` (KeBugCheck); importing the
// crate is sufficient. We never want to panic in release, but if we do, this
// bugchecks rather than corrupting kernel state.
#[cfg(not(test))]
extern crate wdk_panic;

use wdk_alloc::WdkAllocator;

#[cfg(not(test))]
#[global_allocator]
static GLOBAL_ALLOCATOR: WdkAllocator = WdkAllocator;

mod adapter;
mod ddi;
mod device;
// TEMPORARY: post-start bring-up tracer to locate the AddAdapter failure.
mod diag;
mod dxgk;
mod error;
mod mapping;
// Scaffolding: the transport's types/parsers are wired into the StartDevice path
// over Phase-2 milestones M1–M4; allow dead_code until M4 consumes them all.
#[allow(dead_code)]
mod virtio;

use dxgk::*;

/// Emit a line to the kernel debugger / DebugView.
pub(crate) fn kmsg(msg: &core::ffi::CStr) {
    // SAFETY: DbgPrint takes a NUL-terminated C format string; `msg` is
    // NUL-terminated and carries no `%` specifiers, so no varargs are consumed.
    unsafe {
        wdk_sys::ntddk::DbgPrint(msg.as_ptr().cast());
    }
}

/// Driver entry point (named "DriverEntry" so the INF/loader finds it).
///
/// # Safety
/// Called by the OS with valid `driver_object` / `registry_path` per the WDM
/// contract.
#[export_name = "DriverEntry"]
pub unsafe extern "system" fn driver_entry(
    driver_object: PDRIVER_OBJECT,
    registry_path: PUNICODE_STRING,
) -> NTSTATUS {
    kmsg(c"Helios: DriverEntry\n");
    diag::record(0x0D00_0001);

    let mut init = build_ddi_table();
    // SAFETY: pointers are valid for the call; `init` outlives the call on this
    // stack frame, and DxgkInitialize copies what it needs.
    let status = unsafe { DxgkInitialize(driver_object, registry_path, &mut init) };
    diag::record(0x0D00_0002);
    diag::record(status as u32);
    status
}

/// Build the `DRIVER_INITIALIZATION_DATA` DDI table.
fn build_ddi_table() -> DRIVER_INITIALIZATION_DATA {
    // SAFETY: an all-zero DRIVER_INITIALIZATION_DATA is valid and means "no DDI
    // registered"; we then fill in the callbacks we support.
    let mut data: DRIVER_INITIALIZATION_DATA = unsafe { core::mem::zeroed() };

    // Advertise the interface level that matches the implemented table. The WDK
    // 10.0.26100 `DXGKDDI_INTERFACE_VERSION` macro is WDDM 3.2; exposing that
    // while most WDDM 2.x/3.x-only callback blocks are null can pass
    // DxgkInitialize and still fail the adapter bring-up path before StartDevice.
    //
    // Helios followed the viogpu3d-style WDDM 1.3 render/display miniport shape
    // during bring-up. The WDDM-sync-redesign M1 raises it to a coherent WDDM 3.2 +
    // GpuMmu surface (gated by `RAISE_WDDM_3_2_GPUMMU`) so the OS enables monitored
    // fences. The interface version is the primary lever (the OS infers the WDDM
    // level from it + the advertised caps); it is bumped together with the matching
    // `DXGK_DRIVERCAPS.WDDMVersion`, the GpuMmu `MemoryManagementCaps` bits, and
    // `GetNodeMetadata.GpuMmuSupported` in `query_adapter_info.rs`. WDDM 3.2 (not
    // 2.0) is the target: a 2.0 adapter was previously rejected as too old for the
    // 24H2 user-mode driver (STATUS_REVISION_MISMATCH), and the struct ABI we
    // bindgen is already the 26100/WDDM-3.2 shape.
    data.Version = if crate::ddi::query_adapter_info::RAISE_WDDM_3_2_GPUMMU {
        DXGKDDI_INTERFACE_VERSION_WDDM3_2
    } else {
        DXGKDDI_INTERFACE_VERSION_WDDM1_3
    };

    // ── PnP / power lifecycle (Phase 1, real) ──────────────────────────────
    data.DxgkDdiAddDevice = Some(ddi::dxgkddi_add_device);
    data.DxgkDdiStartDevice = Some(ddi::dxgkddi_start_device);
    data.DxgkDdiStopDevice = Some(ddi::dxgkddi_stop_device);
    data.DxgkDdiRemoveDevice = Some(ddi::dxgkddi_remove_device);
    data.DxgkDdiDispatchIoRequest = Some(ddi::dxgkddi_dispatch_io_request);
    data.DxgkDdiSetPowerState = Some(ddi::dxgkddi_set_power_state);

    // ── Base driver/adapter lifecycle DDIs ──────────────────────────────────
    // Non-version-gated base-block DDIs that the MSDN DxgkInitialize sample
    // always provides. dxgkrnl's DpiInitializeEx rejects the init data (with
    // STATUS_REVISION_MISMATCH) when these are NULL, even though the version
    // field itself is accepted — this is the second gate past the DDI-presence
    // check that surfaced once the render/GPU-VA DDIs were registered.
    data.DxgkDdiUnload = Some(ddi::dxgkddi_unload);
    data.DxgkDdiQueryInterface = Some(ddi::dxgkddi_query_interface);
    data.DxgkDdiControlEtwLogging = Some(ddi::dxgkddi_control_etw_logging);
    data.DxgkDdiResetDevice = Some(ddi::dxgkddi_reset_device);
    data.DxgkDdiNotifyAcpiEvent = Some(ddi::dxgkddi_notify_acpi_event);

    // ── Child/adapter queries (Phase 1, real — render-only: no children) ────
    data.DxgkDdiQueryChildRelations = Some(ddi::dxgkddi_query_child_relations);
    data.DxgkDdiQueryChildStatus = Some(ddi::dxgkddi_query_child_status);
    data.DxgkDdiQueryDeviceDescriptor = Some(ddi::dxgkddi_query_device_descriptor);
    // Child container id — only meaningful when the DisplayHalf child video
    // output is advertised; returns NOT_SUPPORTED otherwise (Option A, #1).
    data.DxgkDdiGetChildContainerId = Some(ddi::dxgkddi_get_child_container_id);
    data.DxgkDdiQueryAdapterInfo = Some(ddi::dxgkddi_query_adapter_info);
    data.DxgkDdiStopDeviceAndReleasePostDisplayOwnership =
        Some(ddi::dxgkddi_stop_device_and_release_post_display_ownership);
    data.DxgkDdiSystemDisplayEnable = Some(ddi::dxgkddi_system_display_enable);
    data.DxgkDdiSystemDisplayWrite = Some(ddi::dxgkddi_system_display_write);

    // ── Interrupt path (registered now; ISR wired in Phase 2/3) ─────────────
    data.DxgkDdiInterruptRoutine = Some(ddi::dxgkddi_interrupt_routine);
    data.DxgkDdiDpcRoutine = Some(ddi::dxgkddi_dpc_routine);

    // ── Device / context / process (Phase 1 device alloc; rest stubbed) ─────
    data.DxgkDdiCreateDevice = Some(device::dxgkddi_create_device);
    data.DxgkDdiDestroyDevice = Some(device::dxgkddi_destroy_device);
    data.DxgkDdiCreateContext = Some(device::dxgkddi_create_context);
    data.DxgkDdiDestroyContext = Some(device::dxgkddi_destroy_context);
    data.DxgkDdiCreateProcess = Some(device::dxgkddi_create_process);
    data.DxgkDdiDestroyProcess = Some(device::dxgkddi_destroy_process);

    // ── Memory management (registered, but fails honestly until implemented) ─
    data.DxgkDdiCreateAllocation = Some(ddi::dxgkddi_create_allocation);
    data.DxgkDdiDestroyAllocation = Some(ddi::dxgkddi_destroy_allocation);
    data.DxgkDdiBuildPagingBuffer = Some(ddi::dxgkddi_build_paging_buffer);
    data.DxgkDdiMapCpuHostAperture = Some(ddi::dxgkddi_map_cpu_host_aperture);
    data.DxgkDdiUnmapCpuHostAperture = Some(ddi::dxgkddi_unmap_cpu_host_aperture);

    // ── Command submission (registered, but not advertised as usable yet) ────
    data.DxgkDdiSubmitCommand = Some(ddi::dxgkddi_submit_command);
    data.DxgkDdiSubmitCommandVirtual = Some(ddi::dxgkddi_submit_command_virtual);
    data.DxgkDdiPreemptCommand = Some(ddi::dxgkddi_preempt_command);
    data.DxgkDdiResetFromTimeout = Some(ddi::dxgkddi_reset_from_timeout);
    data.DxgkDdiRestartFromTimeout = Some(ddi::dxgkddi_restart_from_timeout);
    data.DxgkDdiQueryDependentEngineGroup = Some(ddi::dxgkddi_query_dependent_engine_group);
    data.DxgkDdiQueryEngineStatus = Some(ddi::dxgkddi_query_engine_status);
    data.DxgkDdiResetEngine = Some(ddi::dxgkddi_reset_engine);
    data.DxgkDdiCreateHwContext = Some(ddi::dxgkddi_create_hw_context);
    data.DxgkDdiDestroyHwContext = Some(ddi::dxgkddi_destroy_hw_context);
    data.DxgkDdiCreateHwQueue = Some(ddi::dxgkddi_create_hw_queue);
    data.DxgkDdiDestroyHwQueue = Some(ddi::dxgkddi_destroy_hw_queue);
    data.DxgkDdiSubmitCommandToHwQueue = Some(ddi::dxgkddi_submit_command_to_hw_queue);
    data.DxgkDdiSwitchToHwContextList = Some(ddi::dxgkddi_switch_to_hw_context_list);
    data.DxgkDdiPresentToHwQueue = Some(ddi::dxgkddi_present_to_hw_queue);
    data.DxgkDdiCancelCommand = Some(ddi::dxgkddi_cancel_command);
    data.DxgkDdiCalibrateGpuClock = Some(ddi::dxgkddi_calibrate_gpu_clock);
    data.DxgkDdiFormatHistoryBuffer = Some(ddi::dxgkddi_format_history_buffer);
    data.DxgkDdiPowerRuntimeControlRequest = Some(ddi::dxgkddi_power_runtime_control_request);
    data.DxgkDdiPowerRuntimeSetDeviceHandle = Some(ddi::dxgkddi_power_runtime_set_device_handle);
    data.DxgkDdiSetStablePowerState = Some(ddi::dxgkddi_set_stable_power_state);
    data.DxgkDdiSetVirtualMachineData = Some(ddi::dxgkddi_set_virtual_machine_data);

    // ── Private diagnostics/control only; not the render ABI ────────────────
    data.DxgkDdiEscape = Some(ddi::dxgkddi_escape);

    // ── Display/VidPn paths. Registered explicitly, but all return unsupported
    // while StartDevice reports zero sources and children.
    data.DxgkDdiPresent = Some(ddi::dxgkddi_present);
    data.DxgkDdiSetPointerPosition = Some(ddi::dxgkddi_set_pointer_position);
    data.DxgkDdiSetPointerShape = Some(ddi::dxgkddi_set_pointer_shape);
    data.DxgkDdiIsSupportedVidPn = Some(ddi::dxgkddi_is_supported_vidpn);
    data.DxgkDdiRecommendFunctionalVidPn = Some(ddi::dxgkddi_recommend_functional_vidpn);
    data.DxgkDdiEnumVidPnCofuncModality = Some(ddi::dxgkddi_enum_vidpn_cofunc_modality);
    data.DxgkDdiSetVidPnSourceVisibility = Some(ddi::dxgkddi_set_vidpn_source_visibility);
    data.DxgkDdiCommitVidPn = Some(ddi::dxgkddi_commit_vidpn);
    data.DxgkDdiUpdateActiveVidPnPresentPath = Some(ddi::dxgkddi_update_active_vidpn_present_path);
    data.DxgkDdiSetVidPnSourceAddress = Some(ddi::dxgkddi_set_vidpn_source_address);
    data.DxgkDdiRecommendMonitorModes = Some(ddi::dxgkddi_recommend_monitor_modes);
    data.DxgkDdiQueryVidPnHWCapability = Some(ddi::dxgkddi_query_vidpn_hw_capability);
    data.DxgkDdiGetScanLine = Some(ddi::dxgkddi_get_scan_line);
    // MANDATORY once a monitor target is advertised (Option A, #1): a NULL slot
    // fails StartAdapter with Code 43 (StartAdapter_DxgkDdiUpdateMonitorLinkInfoIsNull).
    data.DxgkDdiUpdateMonitorLinkInfo = Some(ddi::dxgkddi_update_monitor_link_info);
    data.DxgkDdiExchangePreStartInfo = Some(ddi::dxgkddi_exchange_pre_start_info);

    // ── Render-path & GPU-VA DDIs. They are registered so the table shape is
    // explicit, but return unsupported until the corresponding capability is
    // implemented and advertised.
    data.DxgkDdiRender = Some(ddi::dxgkddi_render);
    data.DxgkDdiRenderKm = Some(ddi::dxgkddi_render_km);
    data.DxgkDdiRenderGdi = Some(ddi::dxgkddi_render_gdi);
    data.DxgkDdiPatch = Some(ddi::dxgkddi_patch);
    data.DxgkDdiOpenAllocation = Some(ddi::dxgkddi_open_allocation);
    data.DxgkDdiCloseAllocation = Some(ddi::dxgkddi_close_allocation);
    data.DxgkDdiDescribeAllocation = Some(ddi::dxgkddi_describe_allocation);
    data.DxgkDdiGetStandardAllocationDriverData =
        Some(ddi::dxgkddi_get_standard_allocation_driver_data);
    data.DxgkDdiGetNodeMetadata = Some(ddi::dxgkddi_get_node_metadata);
    data.DxgkDdiSetRootPageTable = Some(ddi::dxgkddi_set_root_page_table);
    data.DxgkDdiGetRootPageTableSize = Some(ddi::dxgkddi_get_root_page_table_size);
    data.DxgkDdiCollectDbgInfo = Some(ddi::dxgkddi_collect_dbg_info);
    data.DxgkDdiControlInterrupt = Some(ddi::dxgkddi_control_interrupt);
    data.DxgkDdiQueryCurrentFence = Some(ddi::dxgkddi_query_current_fence);

    data
}
