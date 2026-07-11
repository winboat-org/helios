//! Adapter PnP / power lifecycle DDIs and display child queries.
//!
//! `StartDevice` saves the Dxgkrnl interface, initializes virtio-gpu, and reports
//! either the production one-source/one-child display topology or the explicit
//! knob-off render-only recovery topology.

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

    // Service-key knobs, read once per StartDevice (same iteration model as
    // `BarSegMode`: `reg add` + `devcon restart` re-runs this without a
    // reboot). See the field docs in `adapter.rs`.
    adapter.gdi_accel_mode = crate::diag::read_config_dword(b"GdiAccelMode", 1);
    adapter.alloc_cached = crate::diag::read_config_dword(b"AllocCached", 1) != 0;
    adapter.display_half = crate::diag::read_config_dword(b"DisplayHalf", 0) != 0;
    crate::diag::record_named_bytes(b"GdiM", adapter.gdi_accel_mode);
    crate::diag::record_named_bytes(b"AlcC", adapter.alloc_cached as u32);
    crate::diag::record_named_bytes(b"DspH", adapter.display_half as u32);

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

    // Source/child count. Default (render-only): 0 scanout sources, 0 children.
    // With the `DisplayHalf` knob on: one video-present source + one child
    // video-output, with the VidPn/child DDIs driving virtio-gpu scanout.
    // SAFETY: out-pointers validated non-null above.
    unsafe {
        if adapter.display_half {
            *number_of_video_present_sources = crate::ddi::vidpn::NUM_VIDPN_SOURCES;
            *number_of_children = crate::ddi::vidpn::NUM_CHILDREN;
        } else {
            *number_of_video_present_sources = 0;
            *number_of_children = 0;
        }
    }

    if adapter.display_half {
        // Adopt the host's scanout-0 size (GET_DISPLAY_INFO, captured at transport
        // init) as the VidPn mode + generated-EDID native resolution, so Helios
        // presents the size QEMU actually wants on scanout 0. Falls back to the
        // default in `display_mode()` if the host reported nothing usable.
        if let Ok(Some((w, h))) = adapter.with_virtio(|v| v.display_mode()) {
            adapter.display_w = w;
            adapter.display_h = h;
        }
        let (w, h) = adapter.display_mode();
        adapter.edid = crate::ddi::vidpn::build_edid(w, h);
        crate::diag::record_named_bytes(b"DspMd", (w << 16) | (h & 0xFFFF));
        crate::ddi::scanout_diag::maybe_run(adapter);

        // Arm the CRTC_VSYNC heartbeat: without a free-running VSync, dxgkrnl never
        // retires a flip and so never issues SetVidPnSourceAddress (viogpu3d
        // FlipThread analog). `dxgkrnl` was saved above so the DPC can synthesize
        // interrupts. Never armed on the render-only surface.
        // SAFETY: `adapter` is the final boxed context (dxgkrnl holds it as the
        // miniport device context); the dxgkrnl interface is saved. PASSIVE_LEVEL.
        unsafe { adapter.init_vsync() };

        // Start the HPD worker: it indicates the child connected shortly after this
        // StartDevice returns (DxgkCbIndicateChildStatus is forbidden during it) and
        // on every virtio config-change interrupt, so the OS marks the VidPn target
        // available and builds a source→target path.
        // SAFETY: final boxed context; dxgkrnl saved. PASSIVE_LEVEL.
        unsafe { adapter.init_hpd() };
    }

    crate::diag::record(0x0B00_0004);
    STATUS_SUCCESS
}

/// KTIMER DPC (DISPATCH_LEVEL): synthesize a `DXGK_INTERRUPT_CRTC_VSYNC` for the
/// display half's single target every timer tick (~16 ms), so dxgkrnl advances the
/// flip queue and issues `SetVidPnSourceAddress` (viogpu3d `FlipThread`/`:1977`).
/// Reads only atomics + the saved callback table, so it tolerates a torn-down
/// transport (StopDevice cancels the timer but a queued DPC may still run once).
pub unsafe extern "C" fn vsync_dpc_routine(
    _dpc: *mut KDPC,
    context: *mut c_void,
    _arg1: *mut c_void,
    _arg2: *mut c_void,
) {
    use core::sync::atomic::Ordering;
    if context.is_null() {
        return;
    }
    // SAFETY: `context` is the adapter pointer passed to KeInitializeDpc; valid
    // for the device lifetime (freed only in RemoveDevice, after the timer is
    // cancelled in StopDevice).
    let adapter = unsafe { &*(context as *const AdapterContext) };
    if !adapter.display_half || adapter.vsync_enabled.load(Ordering::Acquire) == 0 {
        return;
    }
    let Some(dxgkrnl) = adapter.dxgkrnl.as_ref() else {
        return;
    };
    let phys = adapter.last_primary_address.load(Ordering::Acquire) as i64;
    // SAFETY: live callback interface; signal_crtc_vsync raises to DIRQL internally
    // via DxgkCbSynchronizeExecution and delivers the CRTC_VSYNC packet.
    let _ = unsafe {
        crate::ddi::submit_command::signal_crtc_vsync(dxgkrnl, phys, crate::ddi::vidpn::CHILD_UID)
    };
    adapter.vsync_count.fetch_add(1, Ordering::Relaxed);
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
        // Cancel the display-half VSync heartbeat + join the HPD worker before
        // teardown (both idempotent; no-ops when the render-only surface never
        // started them). stop_hpd blocks until the worker exits so it can't touch
        // the (about-to-be-torn-down) context.
        adapter.cancel_vsync();
        adapter.stop_hpd();

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

/// `DxgkDdiQueryChildRelations` — enumerate the adapter's child devices.
///
/// Render-only (DisplayHalf off): expose no child devices. DisplayHalf on
/// (Option A): report ONE `TypeVideoOutput` child so the OS can build a VidPn
/// target + attach the default monitor — the presentable output legacy BLT
/// windowed present needs. The array dxgkrnl passes is NUL-terminated (its last
/// entry stays zeroed), so the usable count is `size/stride - 1` (viogpu shape).
pub unsafe extern "C" fn dxgkddi_query_child_relations(
    miniport_device_context: *mut c_void,
    child_relations: *mut DXGK_CHILD_DESCRIPTOR,
    child_relations_size: u32,
) -> NTSTATUS {
    crate::diag::record(0x1200_0001);
    crate::diag::record(0x1201_0000 | (child_relations_size & 0xFFFF));

    if miniport_device_context.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl hands back our AdapterContext.
    let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };
    if !adapter.display_half || child_relations.is_null() {
        // No connectors/monitors → leave the (already-zeroed) array untouched.
        return STATUS_SUCCESS;
    }

    let stride = core::mem::size_of::<DXGK_CHILD_DESCRIPTOR>() as u32;
    // Two-call contract: the array is NUL-terminated, so a size of exactly
    // (count+1)*stride is expected; require room for our one child + terminator.
    if stride == 0 || child_relations_size < stride.saturating_mul(2) {
        crate::diag::record(0x1205_00E0);
        return STATUS_INVALID_PARAMETER;
    }

    // SAFETY: index 0 is within the caller-provided array (checked above). We
    // fully initialize the single video-output child; the terminator entry the
    // OS provided stays zeroed.
    unsafe {
        let d = &mut *child_relations.add(crate::ddi::vidpn::CHILD_INDEX as usize);
        core::ptr::write_bytes(d as *mut _ as *mut u8, 0, stride as usize);
        d.ChildDeviceType = _DXGK_CHILD_DEVICE_TYPE::TypeVideoOutput;
        // AlwaysConnected (NOT Interruptible) is deliberate + load-bearing: the OS
        // synthesizes its initial 1-path VidPn as StartAdapter completes, and for an
        // Interruptible target the target PDO only exists once the driver has
        // asserted DxgkCbIndicateChildStatus(connected) — which our HPD worker does
        // ~500 ms LATER, after the OS has already committed the empty "display
        // nothing" topology. AlwaysConnected creates the target PDO unconditionally
        // at StartDevice (no race), so the OS can pair source0→target0 immediately.
        // Correct for a virtual monitor that never unplugs
        // (enumerating-child-devices-of-a-display-adapter.md:25-27,41-48).
        d.ChildCapabilities.HpdAwareness =
            _DXGK_CHILD_DEVICE_HPD_AWARENESS::HpdAwarenessAlwaysConnected;
        // `Type` is a real (Copy) bindgen union; write the VideoOutput arm's
        // fields directly (each write is a union place-expression, unsafe).
        // HD15 (analog VGA) — NOT VOT_OTHER — is deliberate: per
        // `forced-versus-connected-targets.md`, ONLY analog target types are
        // "forceable", and a target can be enabled (→ a present path is created)
        // only if a monitor is *connected* OR the target is *forceable*. A
        // non-forceable VOT_OTHER target whose virtual-monitor connection the OS
        // doesn't fully recognize is never given a path → 0-path VidPn commits
        // (36th-session root cause). viogpu3d's non-VGA output is likewise VOT_HD15.
        d.ChildCapabilities.Type.VideoOutput.InterfaceTechnology =
            _D3DKMDT_VIDEO_OUTPUT_TECHNOLOGY::D3DKMDT_VOT_HD15;
        d.ChildCapabilities
            .Type
            .VideoOutput
            .MonitorOrientationAwareness = _D3DKMDT_MONITOR_ORIENTATION_AWARENESS::D3DKMDT_MOA_NONE;
        d.ChildCapabilities.Type.VideoOutput.SupportsSdtvModes = 0;
        d.AcpiUid = 0;
        d.ChildUid = crate::ddi::vidpn::CHILD_UID;
    }
    crate::diag::record(0x1205_0001);
    STATUS_SUCCESS
}

/// `DxgkDdiQueryChildStatus` — report HPD state of a child device.
///
/// DisplayHalf on: the single video-output child is always connected (the
/// virtual monitor never unplugs). Off: nothing to report.
pub unsafe extern "C" fn dxgkddi_query_child_status(
    miniport_device_context: *mut c_void,
    child_status: *mut DXGK_CHILD_STATUS,
    non_destructive_only: BOOLEAN,
) -> NTSTATUS {
    crate::diag::record(0x1200_0002);
    if !child_status.is_null() {
        crate::diag::record(0x1202_0000 | unsafe { (*child_status).ChildUid & 0xFFFF });
    }
    crate::diag::record(0x1203_0000 | ((non_destructive_only as u32) & 0xFFFF));

    if miniport_device_context.is_null() || child_status.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: our AdapterContext.
    let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };
    if !adapter.display_half {
        return STATUS_SUCCESS;
    }

    // SAFETY: non-null per the check; dxgkrnl provides a writable DXGK_CHILD_STATUS.
    let status = unsafe { &mut *child_status };
    match status.Type {
        _DXGK_CHILD_STATUS_TYPE::StatusConnection => {
            // Plain union write (safe): report the output as connected.
            status.__bindgen_anon_1.HotPlug.Connected = 1;
            crate::diag::record(0x1206_0001);
            STATUS_SUCCESS
        }
        // We reported MonitorOrientationAwareness = NONE, so the OS must not query
        // rotation status; anything else is not serviced.
        _ => STATUS_NOT_SUPPORTED,
    }
}

/// `DxgkDdiQueryDeviceDescriptor` — return the child monitor's descriptor (EDID).
///
/// DisplayHalf on: we ship no EDID, so report CHILD_DESCRIPTOR_NOT_SUPPORTED and
/// let the OS synthesize a default monitor (its modes come from
/// `DxgkDdiRecommendMonitorModes`). Off: no child descriptors at all.
pub unsafe extern "C" fn dxgkddi_query_device_descriptor(
    miniport_device_context: *mut c_void,
    child_uid: u32,
    device_descriptor: *mut DXGK_DEVICE_DESCRIPTOR,
) -> NTSTATUS {
    crate::diag::record(0x1200_0003);
    crate::diag::record(0x1204_0000 | (child_uid & 0xFFFF));

    if miniport_device_context.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: our AdapterContext.
    let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };
    if !adapter.display_half {
        return STATUS_NOT_SUPPORTED;
    }
    if device_descriptor.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // Serve the EDID generated at StartDevice for the host's scanout-0 mode in the
    // OS-requested chunk (viogpu3d's `QueryDeviceDescriptor`). A REAL monitor (vs
    // the EDID-less "default monitor") is what makes the OS build a presentable
    // target — the 35th session's CHILD_DESCRIPTOR_NOT_SUPPORTED default-monitor
    // path is a suspect for the mode-set retry loop (WINDOWED_BLT_DESIGN §6.3).
    let edid = &adapter.edid;
    // SAFETY: non-null per the check; dxgkrnl provides a writable descriptor.
    let dd = unsafe { &mut *device_descriptor };
    let offset = dd.DescriptorOffset as usize;
    if offset >= edid.len() {
        return crate::ddi::vidpn::STATUS_MONITOR_NO_MORE_DESCRIPTOR_DATA;
    }
    let len = (dd.DescriptorLength as usize).min(edid.len() - offset);
    if dd.DescriptorBuffer.is_null() || len == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: `DescriptorBuffer` is a writable buffer of at least DescriptorLength
    // bytes; `len` is clamped to both it and the remaining EDID.
    unsafe {
        core::ptr::copy_nonoverlapping(
            edid.as_ptr().add(offset),
            dd.DescriptorBuffer as *mut u8,
            len,
        );
    }
    dd.DescriptorLength = len as u32;
    crate::diag::record(0x120E_0000 | (len as u32 & 0xFFFF));
    STATUS_SUCCESS
}

/// `DxgkDdiGetChildContainerId` — return a stable container id for a child.
///
/// The OS groups a display's devnodes by container id. We hand back a fixed,
/// driver-defined GUID for our single video-output child so the monitor devnode
/// binds cleanly. Only meaningful when the display half is active.
pub unsafe extern "C" fn dxgkddi_get_child_container_id(
    miniport_device_context: *mut c_void,
    child_uid: u32,
    container_id: *mut DXGK_CHILD_CONTAINER_ID,
) -> NTSTATUS {
    crate::diag::record(0x1200_0004);
    crate::diag::record(0x1207_0000 | (child_uid & 0xFFFF));

    if miniport_device_context.is_null() || container_id.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: our AdapterContext.
    let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };
    if !adapter.display_half {
        return STATUS_NOT_SUPPORTED;
    }
    // SAFETY: dxgkrnl provides a writable DXGK_CHILD_CONTAINER_ID.
    unsafe {
        let cid = &mut *container_id;
        core::ptr::write_bytes(
            cid as *mut _ as *mut u8,
            0,
            core::mem::size_of::<DXGK_CHILD_CONTAINER_ID>(),
        );
        cid.ContainerId = crate::ddi::vidpn::HELIOS_MONITOR_CONTAINER_ID;
    }
    STATUS_SUCCESS
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
