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

/// Size cap for the VidMm-owned head partition of the host-visible window (the
/// CPU-visible BAR memory segment, id 3). The window is 8 GiB on the current
/// QEMU config (`hostmem=8G`); 1 GiB comfortably holds the CPU-rasterized
/// GDI/shadow/staging/shared-primary standard allocations (a full-screen
/// surface is ~8 MiB) while leaving the rest to the KMD/ICD blob allocator.
const BAR_SEGMENT_MAX_BYTES: u64 = 1 << 30;

/// Configure the segment-3 shape per the `BarSegMode` registry DWORD (service
/// key; read once per StartDevice, so experiments iterate via `reg add` +
/// `devcon restart` — AddAdapter re-runs without a rebuild/reboot). The knob
/// exists because BOTH initial shapes (classic CpuVisible 22.22.45,
/// CpuHostAperture 22.22.46) were rejected at AddAdapter right after the
/// segment queries, and each blind retry costs an owner reboot.
///
///   0  = no BAR segment (baseline recovery shape — always binds)
///   1  = 3 segments (aperture/RAM/BAR id 3) — REJECTED by dxgmms: a
///        SupportsCpuHostAperture segment must be the LAST segment, so ANY
///        segment after the RAM cpu-host segment fails AddAdapter with
///        "Invalid flags specified for segment #2" (ETW AzureTriage, 2026-07-05)
///   2  = 3 segments, BAR id 3, 64 MiB   (historic size-bisect arm; rejected)
///   5  = 3 segments, RAM probe id 3     (historic backing-bisect arm; rejected)
///   10 = 2 segments: aperture + BAR as SEGMENT ID 2, paging-RAM segment
///        dropped (it was vestigial: page tables live in system segment 0,
///        paging buffers in the aperture). **THE PRODUCTION SHAPE** — binds,
///        and with GDI surfaces in this device segment win32k routes their
///        rasterization through DxgkDdiRenderGdi (the executor writes the
///        blob bytes dwm samples) instead of CPU raster into aperture pages:
///        the two-memory-split fix. First full desktop 2026-07-05 20:53.
///   11 = 3 segments swapped: aperture + BAR id 2 + RAM id 3 (rejected —
///        confirms the must-be-last rule; the BAR cpu-host seg isn't last)
fn setup_bar_segment(
    adapter: &mut crate::adapter::AdapterContext,
) -> Option<crate::adapter::BarSegment> {
    let mode = crate::diag::read_config_dword(b"BarSegMode", 10);
    crate::diag::record_named_bytes(b"BarM", mode);
    if mode == 0 {
        return None;
    }
    let seg_id = if mode == 10 || mode == 11 { 2 } else { 3 };
    if mode == 5 {
        // RAM-backed acceptance probe: is the rejection about the BAR GPA?
        let ram = crate::adapter::AdapterContext::alloc_contiguous_ram(16 << 20)?;
        let (gpa, size) = (ram.phys, ram.size);
        adapter.bar_probe_ram = Some(ram);
        crate::diag::record(0x0B00_0008);
        crate::diag::record(((size >> 20) & 0xFFFF_FFFF) as u32);
        return Some(crate::adapter::BarSegment {
            gpa,
            size,
            seg_id,
            topo: mode,
            probe_only: true,
        });
    }
    let window = adapter.with_virtio(|v| v.host_visible()).ok().flatten()?;
    let size = match mode {
        2 => 64 << 20,
        // 1, 10, 11, or any unknown value → the default partition size.
        _ => (window.len / 2).min(BAR_SEGMENT_MAX_BYTES) & !4095,
    };
    if size < (16 << 20) || size > window.len {
        crate::diag::record(0x0B00_00E8);
        return None;
    }
    // The KMD blob-window allocator must never hand out offsets inside the
    // aperture region (dxgkrnl's CPU-host-aperture allocator owns them).
    let _ = adapter.with_virtio(|v| v.reserve_window_prefix(size));
    crate::diag::record(0x0B00_0008);
    crate::diag::record(((size >> 20) & 0xFFFF_FFFF) as u32);
    Some(crate::adapter::BarSegment {
        gpa: window.base,
        size,
        seg_id,
        topo: mode,
        probe_only: false,
    })
}

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

    if adapter.paging_ram.is_none() {
        adapter.paging_ram = AdapterContext::alloc_paging_ram();
    }

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

            // ── BAR memory segment / CPU host aperture (segment 3) ───────────
            // Reserve the window head BEFORE any blob map can allocate a
            // window offset, and before dxgkrnl queries segments.
            // Two-memory-split fix (Option A).
            adapter.bar_segment = setup_bar_segment(adapter);

            // ── Venus-backed page-table memory (best-effort) ─────────────────
            // Self-allocate a 16-MiB HOST_VISIBLE|HOST_COHERENT VkDeviceMemory over
            // venus and expose it as a BAR-backed, CPU-coherent region VidMm can
            // register as the page-table segment (VidMm drops a system-RAM segment;
            // it accepts device-BAR memory backed by real host memory). PASSIVE
            // inside StartDevice; the flows ride `virtio::ctrl` (locked enqueues +
            // PASSIVE waits), so they coexist with the interrupt DPC, which may
            // already be live. On any failure we record diag and leave
            // page_table_window = None — never fail StartDevice (Gate 1 stays
            // start-safe). See virtio::venus.
            if !VENUS_ALLOC_ENABLED {
                // venus page-table allocation disabled — boot-safe build.
                adapter.venus_ctx_id = 0;
                adapter.set_venus_client(None);
                adapter.page_table_window = None;
            } else {
                // Persistent venus context for the device lifetime (owner 0:
                // KMD-internal, destroyed explicitly in StopDevice).
                let venus_result = crate::virtio::ctrl::ctx_create(
                    adapter,
                    helios_protocol::VIRTIO_GPU_CAPSET_VENUS,
                    0,
                )
                .and_then(|ctx_id| {
                    let (client, blob) =
                        crate::virtio::venus::allocate_host_visible_blob(adapter, ctx_id)?;
                    Ok((ctx_id, client, blob))
                });
                match venus_result {
                    Ok((ctx_id, client, blob)) => {
                        crate::diag::record(0x0B00_0007);
                        adapter.venus_ctx_id = ctx_id;
                        adapter.set_venus_client(Some(client));
                        adapter.page_table_window = Some((blob.gpa, blob.size));
                    }
                    Err(e) => {
                        // venus bring-up failed; transport is up but no page-table window.
                        let status: NTSTATUS = e.into();
                        crate::diag::record(0x0B00_00E7);
                        crate::diag::record(status as u32);
                        adapter.venus_ctx_id = 0;
                        adapter.set_venus_client(None);
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
        adapter.set_venus_client(None); // Drop → MmUnmapIoSpace ring + reply mappings.
        adapter.page_table_window = None;
        adapter.venus_ctx_id = 0;
        if venus_ctx != 0 {
            // Best-effort: unref every KMD-internal blob (owner 0) and destroy the
            // venus context (PASSIVE flows through virtio::ctrl).
            let _ = crate::virtio::ctrl::release_blobs_for_owner(adapter, 0);
            let _ = crate::virtio::ctrl::ctx_destroy(adapter, venus_ctx);
        }
        // Free any parked completed entries at PASSIVE before the transport
        // (and the buffers still in flight inside it) is dropped.
        crate::virtio::ctrl::reap_parked(adapter);

        // Tear down the virtio transport: VirtioGpu::drop resets the device and
        // frees its rings (plus any in-flight/parked entry buffers). A later
        // StartDevice re-initializes.
        adapter.set_virtio(None);
        adapter.bar_segment = None;
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
    vidpn_source_id: u32,
    _video_request_packet: PVIDEO_REQUEST_PACKET,
) -> NTSTATUS {
    crate::diag::record(0x0A10_0000 | (vidpn_source_id & 0xFFFF));
    STATUS_SUCCESS
}

/// `DxgkDdiSetPowerState` — accept power transitions (nothing device-specific to
/// do yet).
pub unsafe extern "C" fn dxgkddi_set_power_state(
    _miniport_device_context: *mut c_void,
    device_uid: u32,
    device_power_state: DEVICE_POWER_STATE,
    action_type: POWER_ACTION::Type,
) -> NTSTATUS {
    crate::diag::record(0x0A11_0000 | (device_uid & 0xFFFF));
    crate::diag::record(0x0A12_0000 | ((device_power_state as u32) & 0xFFFF));
    crate::diag::record(0x0A13_0000 | ((action_type as u32) & 0xFFFF));
    STATUS_SUCCESS
}

/// `DxgkDdiQueryChildRelations` — render-only: we expose no child devices.
pub unsafe extern "C" fn dxgkddi_query_child_relations(
    _miniport_device_context: *mut c_void,
    _child_relations: *mut DXGK_CHILD_DESCRIPTOR,
    child_relations_size: u32,
) -> NTSTATUS {
    crate::diag::record(0x1200_0001);
    crate::diag::record(0x1201_0000 | (child_relations_size & 0xFFFF));
    // No connectors/monitors → leave the (already-zeroed) array untouched.
    STATUS_SUCCESS
}

/// `DxgkDdiQueryChildStatus` — no children to report status for.
pub unsafe extern "C" fn dxgkddi_query_child_status(
    _miniport_device_context: *mut c_void,
    child_status: *mut DXGK_CHILD_STATUS,
    non_destructive_only: BOOLEAN,
) -> NTSTATUS {
    crate::diag::record(0x1200_0002);
    if !child_status.is_null() {
        crate::diag::record(0x1202_0000 | unsafe { (*child_status).ChildUid & 0xFFFF });
    }
    crate::diag::record(0x1203_0000 | ((non_destructive_only as u32) & 0xFFFF));
    STATUS_SUCCESS
}

/// `DxgkDdiQueryDeviceDescriptor` — no child descriptors (no EDID/monitor).
pub unsafe extern "C" fn dxgkddi_query_device_descriptor(
    _miniport_device_context: *mut c_void,
    child_uid: u32,
    _device_descriptor: *mut DXGK_DEVICE_DESCRIPTOR,
) -> NTSTATUS {
    crate::diag::record(0x1200_0003);
    crate::diag::record(0x1204_0000 | (child_uid & 0xFFFF));
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
    event_type: IN_DXGK_EVENT_TYPE,
    event: IN_ULONG,
    _argument: IN_PVOID,
    acpi_flags: OUT_PULONG,
) -> NTSTATUS {
    crate::diag::record(0x0A14_0000 | ((event_type as u32) & 0xFFFF));
    crate::diag::record(0x0A15_0000 | (event & 0xFFFF));
    if !acpi_flags.is_null() {
        unsafe { *acpi_flags = 0 };
    }
    STATUS_SUCCESS
}
