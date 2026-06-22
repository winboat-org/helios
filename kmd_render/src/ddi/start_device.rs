//! Adapter PnP / power lifecycle DDIs and the render-only child queries.
//!
//! Phase 1: StartDevice saves the Dxgkrnl interface and reports a render-only
//! adapter (zero video present sources, zero children). The virtio-gpu hardware
//! bring-up (PCI cap scan, BAR mapping, feature negotiation, virtqueue init) is
//! added in Phase 2 (task #4) where the STUB marker is below.

use alloc::boxed::Box;
use core::ffi::c_void;

use crate::adapter::AdapterContext;
use crate::dxgk::*;

/// Run the in-StartDevice venus self-allocation of the page-table window. Gated so
/// the safe/recovery build boots cleanly: the venus client busy-polls a ring in
/// StartDevice and a protocol bug there can hang/crash boot. Turn ON only when
/// debugging the venus path with ntoseye attached (so a wedge is catchable).
const VENUS_ALLOC_ENABLED: bool = true;

/// `DxgkDdiStartDevice` — bring the adapter online.
pub unsafe extern "C" fn dxgkddi_start_device(
    miniport_device_context: *mut c_void,
    _dxgk_start_info: *mut DXGK_START_INFO,
    dxgkrnl_interface: *mut DXGKRNL_INTERFACE,
    number_of_video_present_sources: *mut u32,
    number_of_children: *mut u32,
) -> NTSTATUS {
    crate::kmsg(c"Helios: StartDevice\n");
    crate::diag::record(0x0B00_0001);

    if miniport_device_context.is_null()
        || dxgkrnl_interface.is_null()
        || number_of_video_present_sources.is_null()
        || number_of_children.is_null()
    {
        return STATUS_INVALID_PARAMETER;
    }

    // SAFETY: Dxgkrnl passes our adapter context and valid out-pointers.
    let adapter = unsafe { &mut *(miniport_device_context as *mut AdapterContext) };

    // Save the callback interface for the driver's lifetime (Copy struct).
    adapter.dxgkrnl = Some(unsafe { *dxgkrnl_interface });
    crate::diag::record(0x0B00_0002);

    // ── Phase 2: bring up the virtio-gpu transport ──────────────────────────
    // VirtioGpu::init reads PCI config + maps BARs through the Dxgkrnl callbacks
    // (DxgkConfigAccess / WdkHal) and discovers the virtio device.
    //
    // Gate 1 is an adapter-load gate, not a render-capability gate. During early
    // WDDM bring-up we keep the adapter startable across boot/restart even when
    // the transport probe fails, record the exact status, and leave `virtio=None`.
    // Later gates must tighten this once allocations/submission advertise usable
    // render capability.
    // Drop any prior transport before re-init (e.g. on a stop/start cycle): its
    // Drop resets the device and frees its rings/scratch. Doing it *before*
    // init keeps the ordering safe — otherwise assigning the new transport would
    // drop the old one (resetting the device) right after init configured it.
    adapter.set_virtio(None);
    // SAFETY: dxgkrnl_interface is valid per the DDI contract (also copied into
    // adapter.dxgkrnl just above); init only borrows it for the call.
    match crate::virtio::VirtioGpu::init(unsafe { &*dxgkrnl_interface }) {
        Ok(gpu) => {
            crate::kmsg(c"Helios: virtio-gpu transport up\n");
            crate::diag::record(0x0B00_0003);
            // Publish the ISR-status register VA for the DIRQL ISR before the
            // transport goes live (capture before `gpu` is moved into set_virtio).
            adapter
                .isr_status
                .store(gpu.isr_status_addr(), core::sync::atomic::Ordering::Release);
            adapter.set_virtio(Some(gpu));

            // ── Venus-backed page-table memory (best-effort) ─────────────────
            // Self-allocate a 16-MiB HOST_VISIBLE|HOST_COHERENT VkDeviceMemory over
            // venus and expose it as a BAR-backed, CPU-coherent region VidMm can
            // register as the page-table segment (VidMm drops a system-RAM segment;
            // it accepts device-BAR memory backed by real host memory). Runs at
            // PASSIVE in this single-threaded StartDevice, so we take the transport
            // WITHOUT the DISPATCH spinlock (MmMapIoSpace + ring busy-polls are
            // illegal under it). On any failure we record diag and leave
            // page_table_window = None — never fail StartDevice (Gate 1 stays
            // start-safe). See virtio::venus.
            // SAFETY: PASSIVE_LEVEL inside StartDevice; Dxgkrnl serializes the
            // lifecycle DDIs and no escape/DPC path touches the transport yet, so
            // the unlocked transport access is single-threaded.
            if !VENUS_ALLOC_ENABLED {
                // venus page-table allocation disabled — boot-safe build.
                adapter.venus_ctx_id = 0;
                adapter.venus_client = None;
                adapter.page_table_window = None;
            } else {
                let venus_result = unsafe {
                    adapter.with_virtio_passive(|v| {
                        // Persistent venus context for the device lifetime (owner 0:
                        // KMD-internal, destroyed explicitly in StopDevice).
                        let ctx_id = v.ctx_create(helios_protocol::VIRTIO_GPU_CAPSET_VENUS, 0)?;
                        let (client, blob) =
                            crate::virtio::venus::allocate_host_visible_blob(v, ctx_id)?;
                        Ok::<_, crate::virtio::VirtioError>((ctx_id, client, blob))
                    })
                };
                match venus_result {
                    Ok(Ok((ctx_id, client, blob))) => {
                        crate::diag::record(0x0B00_0007);
                        adapter.venus_ctx_id = ctx_id;
                        adapter.venus_client = Some(client);
                        adapter.page_table_window = Some((blob.gpa, blob.size));
                    }
                    Ok(Err(e)) => {
                        // venus bring-up failed; transport is up but no page-table window.
                        let status: NTSTATUS = e.into();
                        crate::diag::record(0x0B00_00E7);
                        crate::diag::record(status as u32);
                        adapter.venus_ctx_id = 0;
                        adapter.venus_client = None;
                        adapter.page_table_window = None;
                    }
                    Err(_) => {
                        // Transport not up (should not happen — we just set it).
                        crate::diag::record(0x0B00_00E8);
                        adapter.venus_ctx_id = 0;
                        adapter.venus_client = None;
                        adapter.page_table_window = None;
                    }
                }
            } // end if VENUS_ALLOC_ENABLED
        }
        Err(e) => {
            crate::kmsg(c"Helios: virtio-gpu init FAILED\n");
            let status: NTSTATUS = e.into();
            crate::diag::record(0x0B00_00E0);
            crate::diag::record(status as u32);
            adapter
                .isr_status
                .store(0, core::sync::atomic::Ordering::Release);
            adapter.set_virtio(None);
        }
    }

    // Render-only adapter: no scanout sources, no child devices (no monitors).
    // SAFETY: out-pointers validated non-null above.
    unsafe {
        *number_of_video_present_sources = 0;
        *number_of_children = 0;
    }

    crate::diag::record(0x0B00_0004);
    STATUS_SUCCESS
}

/// `DxgkDdiStopDevice` — quiesce the adapter (inverse of StartDevice).
pub unsafe extern "C" fn dxgkddi_stop_device(miniport_device_context: *mut c_void) -> NTSTATUS {
    crate::kmsg(c"Helios: StopDevice\n");
    if !miniport_device_context.is_null() {
        // SAFETY: our adapter context, handed back from AddDevice.
        let adapter = unsafe { &mut *(miniport_device_context as *mut AdapterContext) };
        // Stop the ISR from touching the (about-to-be-reset) device first.
        adapter
            .isr_status
            .store(0, core::sync::atomic::Ordering::Release);

        // Tear down the venus client + page-table blob + context BEFORE dropping
        // the transport (the unref/detach/destroy commands need the live device).
        // Drop the client first to unmap its ring/reply BAR kernel mappings.
        let venus_ctx = adapter.venus_ctx_id;
        adapter.venus_client = None; // Drop → MmUnmapIoSpace ring + reply mappings.
        adapter.page_table_window = None;
        adapter.venus_ctx_id = 0;
        if venus_ctx != 0 {
            // Best-effort: unref every KMD-internal blob (owner 0) and destroy the
            // venus context. Done under the spinlock (control-queue round-trips).
            let _ = adapter.with_virtio(|v| {
                v.release_blobs_for_owner(0);
                let _ = v.ctx_destroy(venus_ctx);
            });
        }

        // Tear down the virtio transport: VirtioGpu::drop resets the device and
        // frees its rings + scratch. A later StartDevice re-initializes.
        adapter.set_virtio(None);
    }
    STATUS_SUCCESS
}

/// `DxgkDdiRemoveDevice` — free the adapter context allocated in AddDevice.
pub unsafe extern "C" fn dxgkddi_remove_device(miniport_device_context: *mut c_void) -> NTSTATUS {
    crate::kmsg(c"Helios: RemoveDevice\n");
    crate::diag::record(0x0C00_0001);
    if !miniport_device_context.is_null() {
        // SAFETY: this pointer came from Box::into_raw in AddDevice; freed once.
        drop(unsafe { Box::from_raw(miniport_device_context as *mut AdapterContext) });
    }
    crate::diag::record(0x0C00_0002);
    STATUS_SUCCESS
}

/// `DxgkDdiDispatchIoRequest` — legacy VRP path; unused by a render-only WDDM
/// adapter.
pub unsafe extern "C" fn dxgkddi_dispatch_io_request(
    _miniport_device_context: *mut c_void,
    _vidpn_source_id: u32,
    _video_request_packet: PVIDEO_REQUEST_PACKET,
) -> NTSTATUS {
    STATUS_NOT_IMPLEMENTED
}

/// `DxgkDdiSetPowerState` — accept power transitions (nothing device-specific to
/// do yet).
pub unsafe extern "C" fn dxgkddi_set_power_state(
    _miniport_device_context: *mut c_void,
    _device_uid: u32,
    _device_power_state: DEVICE_POWER_STATE,
    _action_type: POWER_ACTION::Type,
) -> NTSTATUS {
    STATUS_SUCCESS
}

/// `DxgkDdiQueryChildRelations` — render-only: we expose no child devices.
pub unsafe extern "C" fn dxgkddi_query_child_relations(
    _miniport_device_context: *mut c_void,
    _child_relations: *mut DXGK_CHILD_DESCRIPTOR,
    _child_relations_size: u32,
) -> NTSTATUS {
    // No connectors/monitors → leave the (already-zeroed) array untouched.
    STATUS_SUCCESS
}

/// `DxgkDdiQueryChildStatus` — no children to report status for.
pub unsafe extern "C" fn dxgkddi_query_child_status(
    _miniport_device_context: *mut c_void,
    _child_status: *mut DXGK_CHILD_STATUS,
    _non_destructive_only: BOOLEAN,
) -> NTSTATUS {
    STATUS_SUCCESS
}

/// `DxgkDdiQueryDeviceDescriptor` — no child descriptors (no EDID/monitor).
pub unsafe extern "C" fn dxgkddi_query_device_descriptor(
    _miniport_device_context: *mut c_void,
    _child_uid: u32,
    _device_descriptor: *mut DXGK_DEVICE_DESCRIPTOR,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

// ── Base driver/adapter lifecycle DDIs ──────────────────────────────────────
// These sit in the base (non-version-gated) block of DRIVER_INITIALIZATION_DATA
// and are all present in the MSDN DxgkInitialize sample. dxgkrnl's init path
// (DpiInitializeEx) rejects the init data when they are NULL — leaving them out
// is what made DxgkInitialize return STATUS_REVISION_MISMATCH even after the
// render/GPU-VA DDIs were registered.

/// `DxgkDdiUnload` — driver-wide unload (no device context). Inverse of
/// DriverEntry. All devices have been removed by now, so release the cached BAR
/// MMIO mappings that `WdkHal` reused across stop/start cycles.
pub unsafe extern "C" fn dxgkddi_unload() {
    crate::kmsg(c"Helios: Unload\n");
    crate::virtio::hal::WdkHal::unmap_all();
}

/// `DxgkDdiQueryInterface` — export a driver-defined interface. We expose none.
pub unsafe extern "C" fn dxgkddi_query_interface(
    _miniport_device_context: IN_CONST_PVOID,
    query_interface: IN_PQUERY_INTERFACE,
) -> NTSTATUS {
    // DIAG: log each interface GUID dxgkrnl asks for during AddAdapter. If
    // AddAdapter dies (OBJECT_NAME_NOT_FOUND) right after a query we reject, that
    // interface is the suspect. Marker 0x04000000 then the GUID's Data1.
    crate::diag::record(0x0400_0000);
    if !query_interface.is_null() {
        // SAFETY: non-null per the check; Dxgkrnl provides a valid QUERY_INTERFACE.
        let qi = unsafe { &*query_interface };
        if !qi.InterfaceType.is_null() {
            // SAFETY: InterfaceType points to a GUID for the duration of the call.
            crate::diag::record(unsafe { (*qi.InterfaceType).Data1 });
        }
    }
    STATUS_NOT_SUPPORTED
}

/// `DxgkDdiControlEtwLogging` — enable/disable the driver's ETW logging. We emit
/// none, so this is a no-op.
pub unsafe extern "C" fn dxgkddi_control_etw_logging(
    _enable: IN_BOOLEAN,
    _flags: IN_ULONG,
    _level: IN_UCHAR,
) {
}

/// `DxgkDdiResetDevice` — reset the device to a known state (e.g. before a crash
/// dump). No hardware to quiesce until Phase 2; no-op.
pub unsafe extern "C" fn dxgkddi_reset_device(_miniport_device_context: IN_CONST_PVOID) {}

/// `DxgkDdiNotifyAcpiEvent` — handle a platform ACPI event. We service none.
pub unsafe extern "C" fn dxgkddi_notify_acpi_event(
    _miniport_device_context: IN_CONST_PVOID,
    _event_type: IN_DXGK_EVENT_TYPE,
    _event: IN_ULONG,
    _argument: IN_PVOID,
    _acpi_flags: OUT_PULONG,
) -> NTSTATUS {
    STATUS_NOT_IMPLEMENTED
}
