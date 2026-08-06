//! Adapter PnP / power lifecycle DDIs.
//!
//! `StartDevice` saves the Dxgkrnl interface, initializes virtio-gpu, and
//! reports either the production one-source/one-child display topology or the
//! explicit knob-off render-only recovery topology.
//!
//! Moved verbatim out of `ddi/lifecycle.rs` by T8/R1102.

use alloc::boxed::Box;
use core::ffi::c_void;

use crate::adapter::AdapterContext;
use crate::dxgk::*;

use super::bar_segment::{build_segment_table, setup_bar_segment};

/// Same-boot zeroing for the PRODUCTION LINEAR-scanout ladder (T6/R901).
///
/// These eight names are written by `venus::create_linear_scanout_image` /
/// `allocate_linear_scanout_image_blob` -- the KMD's real display fallback,
/// which stays -- but their only zeroing used to live inside the deleted
/// `scanout_diag::maybe_run`. Registry value names PERSIST ACROSS BOOTS, so
/// without this a stale prior-boot `SdgLStg = 0x10` survives into a boot where
/// the fallback is never taken and reads as though it had been: exactly the
/// cross-boot counter trap the evidence rules forbid.
///
/// ⚠ `#[inline(never)]` is a STACK BUDGET decision, not style -- the same rule
/// `bring_up_venus` below is annotated for. The eight-element name array is ~64
/// bytes of locals, and inlined into `dxgkddi_start_device` it measurably grew
/// that frame (8424 -> 8456 bytes measured with `tools/kmd-frame-sizes.ps1`),
/// eating 32 of the 352 bytes of headroom on the 24 KB kernel boot stack. In its
/// own transient frame -- which does not overlap `VirtioGpu::init` -- it costs
/// the budget nothing. Overflow here is `0xc0000001`/Startup Repair with no dump
/// and no bugcheck event, and it does NOT reproduce on a live devcon restart.
#[inline(never)]
fn zero_linear_scanout_breadcrumbs() {
    for name in [
        b"SdgLStg", b"SdgLReq", b"SdgLBit", b"SdgLTyc", b"SdgLImg", b"SdgLMem", b"SdgLPch",
        b"SdgLOff",
    ] {
        crate::diag::record_named_bytes(name, 0);
    }
}

/// Stand up the persistent venus context + page-table blob. Returns the venus
/// context id, or 0 on any failure.
///
/// It used to also return the blob's `(gpa, size)` window, which was stored in
/// the transport generation and read by nobody — `query_segments` deliberately
/// reports `paging_ram` instead, because QuerySegment4 runs BEFORE this
/// allocation exists. R510 annotated that field write-only and left the
/// deletion to a dead-code commit with its own reachability evidence; this is
/// that commit (2026-08-05). The blob itself is still allocated and still owned
/// by the venus client — only the unread copy of its address is gone.
///
/// ⚠ `#[inline(never)]` is a STACK BUDGET decision, not style. `VenusClient` and
/// the blob descriptors are large locals, and StartDevice's frame is already
/// shared with `VirtioGpu::init`'s 3.0 KB one on a 24 KB kernel stack. Keeping
/// these in their own transient frame — which does not overlap
/// `VirtioGpu::init` — is what keeps the nested peak inside the budget. See
/// `StartedState::boxed` for the boot failure this class of growth caused.
#[inline(never)]
fn bring_up_venus(passive: crate::irql::PassiveLevel, adapter: &AdapterContext) -> u32 {
    // Persistent venus context for the device lifetime (owner 0: KMD-internal,
    // destroyed explicitly in StopDevice).
    let venus_result = crate::virtio::ctrl::ctx_create(
        passive,
        adapter,
        helios_protocol::VIRTIO_GPU_CAPSET_VENUS,
        None,
    )
    .and_then(|ctx_id| {
        let (client, _blob) =
            crate::virtio::venus::allocate_host_visible_blob(passive, adapter, ctx_id)?;
        Ok((ctx_id, client))
    });
    match venus_result {
        Ok((ctx_id, client)) => {
            crate::diag::record(0x0B00_0007);
            adapter.set_venus_client(Some(client));
            ctx_id
        }
        Err(e) => {
            // venus bring-up failed; transport is up but no page-table window.
            let status: NTSTATUS = e.into();
            crate::diag::record(0x0B00_00E7);
            crate::diag::record(status as u32);
            crate::diag::fault(crate::diag::FaultCounter::StVnu, status as u32);
            adapter.set_venus_client(None);
            0
        }
    }
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

    // SHARED borrow only. The context pointer has been public to dxgkrnl since
    // AddDevice, and before this function returns the DIRQL ISR, the VSync timer
    // DPC and the HPD worker all build `&AdapterContext` from the same address —
    // `set_virtio(Some(gpu))` below enables the device mid-function, and
    // `start_vsync`/`init_hpd` at the end start the other two. A unique `&mut`
    // spanning that was an unambiguous Stacked-Borrows violation.
    //
    // Everything StartDevice establishes is therefore built as LOCALS and
    // published once, near the end, through `publish_started`.
    //
    // SAFETY: Dxgkrnl passes our adapter context and valid out-pointers.
    let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };

    // Clear the start edge FIRST. On a stop/start cycle the flag survives from
    // the previous start (the context does), and `init_hpd` below spawns a fresh
    // worker — which would see a stale 1, skip the wait entirely and indicate
    // child status while THIS StartDevice is still running. That is precisely
    // what the wait exists to prevent.
    adapter
        .start_complete
        .store(0, core::sync::atomic::Ordering::Release);

    // NOT copied here. `dxgkrnl_interface` is 576 bytes and this function's
    // stack frame is shared with `VirtioGpu::init`'s 3.0 KB one on a 24 KB
    // kernel stack — see `StartedState::boxed`. The pointer is carried to the
    // publication site and dereferenced straight into the heap allocation.
    crate::diag::record(0x0B00_0002);

    // EVERY service-key knob, read once per StartDevice (`reg add` + `devcon
    // restart` re-runs this without a reboot), together with the breadcrumbs that
    // mirror them. The descriptor writers and the caps path take this value, so
    // none of them can reach the registry themselves. See `AdapterKnobs`.
    let mut knobs = crate::adapter::AdapterKnobs::read_at_start();

    // Registry values persist across boots, so a stale nonzero fault counter is
    // indistinguishable from a fault on THIS boot. Zero the whole set once here,
    // before anything can fail, so the gate's "verify movement, not presence"
    // rule applies to every counter below.
    crate::diag::reset_fault_counters();

    // Carried over from a previous start on this same context, if any: these
    // blocks are allocated once and freed only in Drop, and today's code gets
    // that by leaving the fields untouched across StopDevice. Publish-once would
    // otherwise leak them and allocate again.
    // SAFETY: StartDevice, PASSIVE, serialized by dxgkrnl against every other
    // lifecycle DDI; the blocks are republished below in the new state.
    let mut paging_ram = unsafe { adapter.take_paging_ram() };
    if paging_ram.is_none() {
        paging_ram = AdapterContext::alloc_paging_ram();
    }
    // D4a read-ledger page: same once-per-adapter lifetime as the RAM blocks
    // above (user mappings of it survive a PnP stop), allocated on the HEAP —
    // never a StartDevice-chain stack frame (the 17936 B boot-chain ceiling).
    // Idempotent; failure is counted (`RdPgF`) and only latches the acquire
    // feature off, never the adapter.
    adapter.read_ledger.init_page();

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
    // Non-zero only if init below fails, so the display-half demotion can report
    // the status that actually killed the transport rather than a bare flag.
    let mut transport_fail_status: u32 = 0;
    // The transport generation, built as locals and installed after publication.
    let mut bar_segment = None;
    let mut venus_ctx_id = 0u32;
    // SAFETY: dxgkrnl_interface is valid per the DDI contract (also copied into
    // the `dxgkrnl` local above); init only borrows it for the call.
    // SAFETY: `DxgkDdiStartDevice` is documented "IRQL: PASSIVE_LEVEL" (WDK
    // DXGKDDI_START_DEVICE). It is also the deepest stack in the driver — this
    // token threads down through `bring_up_venus` -> `allocate_host_visible_blob`
    // -> `VenusRing::bring_up`, which is why it is a by-value ZST and not a
    // reference: see `crate::irql` and tools/kmd-frame-sizes.ps1.
    let passive = unsafe { crate::irql::PassiveLevel::assume() };
    match crate::virtio::VirtioGpu::init(passive, unsafe { &*dxgkrnl_interface }) {
        Ok(gpu) => {
            crate::kmsg(c"Helios: virtio-gpu transport up\n");
            crate::diag::record(0x0B00_0003);
            let host_visible_bytes = gpu.host_visible().map(|window| window.len);
            // Publish the ISR-status register VA for the DIRQL ISR before the
            // transport goes live (capture before `gpu` is moved into set_virtio).
            adapter
                .isr_status
                .store(gpu.isr_status_addr(), core::sync::atomic::Ordering::Release);
            adapter.set_virtio(Some(gpu));

            // An explicit VidMmVramMB registry value remains authoritative.
            // When it is absent, use the exact virtio shared-memory capability
            // length rather than a compiled 4-GiB default or the padded PCI BAR.
            super::bar_segment::resolve_vidmm_vram_mb(&mut knobs, host_visible_bytes);

            // ── BAR memory segment / CPU host aperture ──────────────────────
            // Reserve the window head BEFORE any blob map can allocate a
            // window offset, and before dxgkrnl queries segments.
            // Two-memory-split fix (Option A).
            bar_segment = setup_bar_segment(adapter, &knobs);

            // ── Venus-backed page-table memory (best-effort) ─────────────────
            // Self-allocate a 16-MiB HOST_VISIBLE|HOST_COHERENT VkDeviceMemory over
            // venus and expose it as a BAR-backed, CPU-coherent region VidMm can
            // register as the page-table segment (VidMm drops a system-RAM segment;
            // it accepts device-BAR memory backed by real host memory). PASSIVE
            // inside StartDevice; the flows ride `virtio::ctrl` (locked enqueues +
            // PASSIVE waits), so they coexist with the interrupt DPC, which may
            // already be live. On any failure we record diag and leave the
            // venus context id 0 — never fail StartDevice (Gate 1 stays
            // start-safe). See virtio::venus.
            venus_ctx_id = bring_up_venus(passive, adapter);
        }
        Err(e) => {
            crate::kmsg(c"Helios: virtio-gpu init FAILED\n");
            let status: NTSTATUS = e.into();
            crate::diag::record(0x0B00_00E0);
            crate::diag::record(status as u32);
            crate::diag::fault(crate::diag::FaultCounter::StVio, status as u32);
            transport_fail_status = status as u32;
            adapter
                .isr_status
                .store(0, core::sync::atomic::Ordering::Release);
            adapter.set_virtio(None);
            super::bar_segment::resolve_vidmm_vram_mb(&mut knobs, None);
        }
    }

    // Gate 1 keeps the adapter startable without a transport (render-only
    // recovery), but the display half has no such licence: with virtio=None
    // nothing can ever reach a scanout. Left enabled it reports one source and
    // one child, arms the CRTC_VSYNC heartbeat, and has the HPD worker tell the
    // OS the monitor is CONNECTED - so the OS commits a path to a target that
    // can never receive a frame. No allocation ever gets a resource id, so every
    // SetVidPnSourceAddress exits with ScRid=0 and STATUS_SUCCESS: a permanently
    // blank monitor whose only diagnostic was a DiagLevel-gated breadcrumb.
    //
    // Turning the flag OFF - rather than merely reporting zero sources - is
    // required because ~20 display DDIs branch on the flag itself. StartDevice
    // still returns STATUS_SUCCESS: the render-only recovery shape is preserved.
    if knobs.display_half && adapter.with_virtio(|_| ()).is_err() {
        knobs.display_half = false;
        crate::diag::fault(
            crate::diag::FaultCounter::StNoTx,
            if transport_fail_status != 0 {
                transport_fail_status
            } else {
                1
            },
        );
        crate::diag::record_named_bytes(b"DspH", 0);
    }

    // Source/child count. Default (render-only): 0 scanout sources, 0 children.
    // With the `DisplayHalf` knob on: one video-present source + one child
    // video-output, with the VidPn/child DDIs driving virtio-gpu scanout.
    // SAFETY: out-pointers validated non-null above.
    unsafe {
        if knobs.display_half {
            *number_of_video_present_sources = crate::ddi::vidpn::NUM_VIDPN_SOURCES;
            *number_of_children = crate::ddi::vidpn::NUM_CHILDREN;
        } else {
            *number_of_video_present_sources = 0;
            *number_of_children = 0;
        }
    }

    // Defensive: a StopDevice on this same context should already have done
    // this, but a start that inherits a latched gate or a stale resource id from
    // a previous transport generation is unrecoverable, so pay for it twice.
    adapter.reset_display_publication_state();
    // R505: zero the deferred-programming refusal counters and write the zeros
    // through. Registry counter values persist across boots, so without this a
    // reader cannot tell a counter that is merely PRESENT from one that moved
    // this boot.
    crate::ddi::display::reset_scanout_reject_counters();
    // Same rule for the unsampled scanout-bind trace: its whole purpose is that
    // a value read after a workload describes THAT workload.
    crate::ddi::scanout_trace::reset(adapter);

    // The scan-out mode and its EDID, resolved BEFORE publication because
    // `StartedState` is published exactly once.
    let mut host_mode = None;
    if knobs.display_half {
        // Adopt the host's scanout-0 size (GET_DISPLAY_INFO, captured at transport
        // init) as the VidPn mode + generated-EDID native resolution, so Helios
        // presents the size QEMU actually wants on scanout 0. Falls back to the
        // default in `display_mode()` if the host reported nothing usable.
        //
        // The two failure arms are NOT the same thing and neither is benign: the
        // fallback fabricates a mode, so the OS is handed an EDID for a monitor
        // whose size we invented. Distinguish them - `Err` means the transport is
        // gone (and therefore nothing can ever scan out), `Ok(None)` means the
        // host answered but reported nothing usable.
        match adapter.with_virtio(|v| v.display_mode()) {
            Ok(Some((w, h))) => {
                host_mode = Some((w, h));
            }
            Ok(None) => {
                crate::diag::fault(crate::diag::FaultCounter::StMdB, 1);
            }
            Err(e) => {
                let status: NTSTATUS = e.into();
                crate::diag::fault(crate::diag::FaultCounter::StTxG, status as u32);
            }
        }
    }
    // ONE value: the constructor validates the extent and generates the matching
    // EDID, so the two cannot disagree. The render-only surface advertises no
    // monitor, so its EDID is zeroed and QueryDeviceDescriptor answers
    // NOT_SUPPORTED before ever reading it.
    let scanout_mode = if knobs.display_half {
        crate::adapter::ScanoutMode::adopt(host_mode)
    } else {
        crate::adapter::ScanoutMode::render_only()
    };

    // ── The reported segment table, built ONCE from the same locals every other
    // consumer will read. `query_segments` renders this; it no longer re-derives
    // a table of its own from live adapter state, which is what let the reported
    // topology and `bar_segment` disagree (k-capsescape-04).
    //
    // Ordering is proven, not assumed: on a DiagLevel=1 boot the ring shows
    // StartDevice entry/exit (0x0B00_0001 .. 0x0B00_0004) completing before the
    // first QueryAdapterInfo (0x0100_0001), so the table always exists by the
    // time QUERYSEGMENT4 runs.
    let segment_table = build_segment_table(
        &mut bar_segment,
        paging_ram.as_ref().map(|r| (r.phys, r.size)),
        &knobs,
    );

    // ── Publish. Everything above was a local; from here the adapter answers. ──
    // SAFETY: StartDevice, PASSIVE_LEVEL, serialized by dxgkrnl; published once
    // per start, and every reader reaches it through the Acquire in `started()`.
    // `boxed` builds on the HEAP in its own (transient) frame — never bind its
    // value here, only the Box, or the 832-byte temporary comes back.
    unsafe {
        adapter.publish_started(crate::adapter::StartedState::boxed(
            dxgkrnl_interface,
            knobs,
            scanout_mode,
            paging_ram,
            segment_table,
        ));
        adapter.set_transport_generation(Some(crate::adapter::TransportGeneration {
            bar_segment,
            venus_ctx_id,
        }));
    }

    if knobs.display_half {
        crate::diag::record_named_bytes(b"DspMd", adapter.display_mode_packed());

        zero_linear_scanout_breadcrumbs();

        // Arm the CRTC_VSYNC heartbeat: without a free-running VSync, dxgkrnl never
        // retires a flip and so never issues SetVidPnSourceAddress (viogpu3d
        // FlipThread analog). `dxgkrnl` was saved above so the DPC can synthesize
        // interrupts. Never armed on the render-only surface.
        // SAFETY: `adapter` is the final boxed context (dxgkrnl holds it as the
        // miniport device context); the started state — including the callback
        // table the DPC needs — is published above. PASSIVE_LEVEL.
        unsafe { adapter.start_vsync() };

        // Start the HPD worker: it indicates the child connected shortly after this
        // StartDevice returns (DxgkCbIndicateChildStatus is forbidden during it) and
        // on every virtio config-change interrupt, so the OS marks the VidPn target
        // available and builds a source→target path.
        // SAFETY: final boxed context; dxgkrnl saved. PASSIVE_LEVEL.
        unsafe { adapter.init_hpd() };
    }

    crate::diag::record(0x0B00_0004);
    // LAST action: the real edge the HPD worker waits on. Its prologue used to
    // approximate "StartDevice has returned" with a 500 ms delay; that delay is
    // now only a bounded fallback (`HpdStTo` counts it firing). Safe to signal
    // even when the worker was never started — nothing else waits on this.
    adapter.signal_start_complete();
    STATUS_SUCCESS
}

/// `DxgkDdiStopDevice` — quiesce the adapter (inverse of StartDevice).
pub unsafe extern "C" fn dxgkddi_stop_device(miniport_device_context: *mut c_void) -> NTSTATUS {
    crate::kmsg(c"Helios: StopDevice\n");
    if !miniport_device_context.is_null() {
        // SHARED borrow, for the same reason StartDevice takes one: the ISR and
        // the DPCs can still build `&AdapterContext` from this pointer while this
        // function runs, and it does not stop being true just because we are
        // tearing down.
        // SAFETY: our adapter context, handed back from AddDevice.
        let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };
        // Stop the ISR from touching the (about-to-be-reset) device first.
        //
        // ⚠ ASYMMETRY, recorded rather than changed (k-ctrlsubmit-12): this
        // clears the ISR's gate but NOT `started_published`, so the boxed
        // StartedState — including the DXGKRNL_INTERFACE both DPCs read
        // lock-free — stays published across the stop. That is deliberate on
        // one count (a stop/start cycle carries the contiguous RAM blocks
        // forward through it) and unexamined on another: a DPC already queued
        // when this runs can still resolve `started()` for a stopped device.
        // The DPC's actual work all goes through `with_virtio`, which is
        // `Err(DeviceNotFound)` once `set_virtio(None)` runs below, so the
        // window is currently harmless. Clearing the publication properly needs
        // the take-and-republish dance `take_paging_ram` already performs, and
        // that is a lifecycle change with its own reboot-level gate — not a
        // T4a minor item.
        adapter
            .isr_status
            .store(0, core::sync::atomic::Ordering::Release);
        // Cancel the display-half VSync heartbeat + join the HPD worker before
        // teardown (both idempotent; no-ops when the render-only surface never
        // started them). stop_hpd blocks until the worker exits so it can't touch
        // the (about-to-be-torn-down) context.
        adapter.stop_vsync();
        adapter.stop_hpd();
        // AFTER stop_hpd, so the worker can no longer re-publish into the state
        // we are about to clear. Every scanout identity below belongs to the
        // transport generation being torn down; carrying it into the next
        // StartDevice is how a latched gate kills CRTC_VSYNC and how a recycled
        // resource id gets bound as the cached scan-out target.
        adapter.reset_display_publication_state();

        // SAFETY: `DxgkDdiStopDevice` is documented "IRQL: PASSIVE_LEVEL" (WDK
        // DXGKDDI_STOP_DEVICE); the teardown below unrefs blobs and destroys the
        // venus context, both control round-trips against the still-live device.
        let passive_stop = unsafe { crate::irql::PassiveLevel::assume() };

        // Tear down the venus client + page-table blob + context BEFORE dropping
        // the transport (the unref/detach/destroy commands need the live device).
        // Drop the client first to unmap its ring/reply BAR kernel mappings.
        let venus_ctx = adapter.venus_ctx_id();
        adapter.set_venus_client(None); // Drop → MmUnmapIoSpace ring + reply mappings.
        if venus_ctx != 0 {
            // Best-effort: unref every KMD-internal blob (owner 0) and destroy the
            // venus context (PASSIVE flows through virtio::ctrl).
            // The KMD-owned sweep — `None` here means exactly the KMD's own blobs, not
            // "every owner".
            let _ = crate::virtio::ctrl::release_blobs_for_owner(passive_stop, adapter, None);
            let _ = crate::virtio::ctrl::ctx_destroy_kmd(passive_stop, adapter, venus_ctx);
        }
        // Free any parked completed entries at PASSIVE before the transport
        // (and the buffers still in flight inside it) is dropped.
        crate::virtio::ctrl::reap_parked(passive_stop, adapter);

        // Tear down the virtio transport: VirtioGpu::drop resets the device and
        // frees its rings (plus any in-flight/parked entry buffers). A later
        // StartDevice re-initializes.
        adapter.set_virtio(None);

        // Drop the whole transport generation in one store — `bar_segment` and
        // `venus_ctx_id` together, since both are meaningless in the next
        // generation.
        //
        // The STICKY half is deliberately left alone. StopDevice has never
        // cleared the knobs, the mode or the EDID, and about two dozen sites
        // branch on `display_half`; clearing it here would flip all of them from
        // SUCCESS-shaped answers to NOT_SUPPORTED between StopDevice and
        // RemoveDevice. That is a behaviour change, not a tidy-up.
        // SAFETY: StopDevice, PASSIVE_LEVEL, serialized by dxgkrnl against
        // StartDevice and against every DDI that reads the generation.
        unsafe { adapter.set_transport_generation(None) };
    }
    STATUS_SUCCESS
}

/// `DxgkDdiRemoveDevice` — free the adapter context allocated in AddDevice.
pub unsafe extern "C" fn dxgkddi_remove_device(miniport_device_context: *mut c_void) -> NTSTATUS {
    crate::kmsg(c"Helios: RemoveDevice\n");
    crate::diag::record(0x0C00_0001);
    if !miniport_device_context.is_null() {
        // SAFETY: our adapter context; only read here.
        let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };
        if adapter.hpd_worker_may_be_running() {
            // stop_hpd could not prove the worker exited, and the worker
            // dereferences this context. Leak it deliberately: a permanent
            // allocation leak is strictly better than freeing memory a live
            // PASSIVE thread is still touching. StHpdX already recorded why.
            crate::diag::record(0x0C00_00E1);
        } else {
            // SAFETY: this pointer came from Box::into_raw in AddDevice; freed once.
            drop(unsafe { Box::from_raw(miniport_device_context as *mut AdapterContext) });
        }
    }
    crate::diag::record(0x0C00_0002);
    STATUS_SUCCESS
}

/// `DxgkDdiDispatchIoRequest` — legacy VRP path; unused by a render-only WDDM
/// adapter.
pub unsafe extern "C" fn dxgkddi_dispatch_io_request(
    _miniport_device_context: *mut c_void,
    vidpn_source_id: u32,
    video_request_packet: PVIDEO_REQUEST_PACKET,
) -> NTSTATUS {
    crate::diag::record(0x0A10_0000 | (vidpn_source_id & 0xFFFF));
    // Returning STATUS_SUCCESS without touching the VRP's StatusBlock tells the
    // caller the request was serviced and leaves it to read whatever was in the
    // block. We service no VRP, so say so in the block the contract puts it in.
    // A WDDM display miniport is effectively never called here, so this is
    // honesty rather than a live bug - and StVrp is how we would find out
    // otherwise.
    if !video_request_packet.is_null() {
        // SAFETY: dxgkrnl owns the packet for the duration of the call; the
        // StatusBlock pointer is part of the same contract and is only written
        // after a null check.
        unsafe {
            let vrp = &*video_request_packet;
            crate::diag::fault(crate::diag::FaultCounter::StVrp, vrp.IoControlCode);
            if !vrp.StatusBlock.is_null() {
                // VP_STATUS is a Win32 error code, NOT an NTSTATUS:
                // ERROR_INVALID_FUNCTION is the video-port convention for "this
                // miniport does not implement this IOCTL".
                const ERROR_INVALID_FUNCTION: i32 = 1;
                (*vrp.StatusBlock).__bindgen_anon_1.Status = ERROR_INVALID_FUNCTION;
                (*vrp.StatusBlock).Information = 0;
            }
        }
    }
    STATUS_SUCCESS
}

/// `DxgkDdiSetPowerState` — accept power transitions (nothing device-specific to
/// do yet).
pub unsafe extern "C" fn dxgkddi_set_power_state(
    miniport_device_context: *mut c_void,
    device_uid: u32,
    device_power_state: DEVICE_POWER_STATE,
    action_type: POWER_ACTION::Type,
) -> NTSTATUS {
    crate::diag::record(0x0A11_0000 | (device_uid & 0xFFFF));
    crate::diag::record(0x0A12_0000 | ((device_power_state as u32) & 0xFFFF));
    crate::diag::record(0x0A13_0000 | ((action_type as u32) & 0xFFFF));
    crate::diag::record_named_bytes(
        b"PwrSt",
        (((device_power_state as u32) & 0xFFFF) << 16) | ((action_type as u32) & 0xFFFF),
    );

    if miniport_device_context.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: our adapter context, handed back from AddDevice.
    let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };

    // Before this, a D3 transition was accepted with no action at all, so the
    // ~16 ms KTIMER kept synthesising CRTC_VSYNC through DxgkCbNotifyInterrupt
    // for a source dxgkrnl had powered down - unless dxgkrnl happened to have
    // called DxgkDdiControlInterrupt(CRTC_VSYNC, FALSE) first, which is a brake
    // entirely under its control, not ours.
    //
    // Treat ANY non-D0 state as a timer quiesce and re-arm on D0 when the
    // display half is up. These transitions preserve `vsync_enabled`, whose
    // sole owner after StartDevice is ControlInterrupt at up to DIRQL; a power
    // resume must not silently reverse a prior CRTC_VSYNC disable.
    //
    // Compared against the bindgen discriminant rather than a hand-written
    // integer, so a WDK header change cannot silently invert this.
    if device_power_state == _DEVICE_POWER_STATE::PowerDeviceD0 {
        if adapter.display_half() {
            // SAFETY: the context is the final boxed adapter (dxgkrnl holds it
            // as the miniport device context) and dxgkrnl was saved at
            // StartDevice. PASSIVE_LEVEL.
            unsafe { adapter.resume_vsync() };
        }
    } else {
        adapter.quiesce_vsync();
    }
    STATUS_SUCCESS
}
