//! Display/VidPn DDIs required for a complete WDDM table shape.
//!
//! Helios targets render-only WDDM. These callbacks therefore do not implement
//! scanout; they make unsupported display paths explicit instead of leaving a
//! large part of the display-miniport table NULL during bring-up.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use bytemuck::pod_read_unaligned;
use helios_protocol::HeliosPresentPrivateData;

use crate::adapter::AdapterContext;
use crate::ddi::create_allocation::present_alloc_info;
use crate::dxgk::*;
use wdk_sys::ntddk::KeGetCurrentIrql;

/// Write a DWORD to a fixed (non-ring) registry value so a rare DDI's trace
/// survives the `S<idx>` ring's steady-state QueryAdapterInfo flood. PASSIVE only.
fn rec_named(name: &[u8], value: u32) {
    let mut buf = [0u16; 16];
    let n = name.len().min(14);
    let mut i = 0;
    while i < n {
        buf[i] = name[i] as u16;
        i += 1;
    }
    buf[n] = 0;
    crate::diag::record_named(&buf[..=n], value);
}

pub static PRESENT_COUNT: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_SRC_COUNT: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_DST_COUNT: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_DMA_SIZE: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_PATCH_SIZE: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_SRC_OPEN_LOW: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_DST_OPEN_LOW: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_FLAGS: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_STATUS: AtomicU32 = AtomicU32::new(0);

unsafe fn present_private_data(args: &DXGKARG_PRESENT) -> Option<HeliosPresentPrivateData> {
    if args.pPrivateDriverData.is_null()
        || (args.PrivateDriverDataSize as usize) < core::mem::size_of::<HeliosPresentPrivateData>()
    {
        return None;
    }
    let bytes = unsafe {
        core::slice::from_raw_parts(
            args.pPrivateDriverData as *const u8,
            core::mem::size_of::<HeliosPresentPrivateData>(),
        )
    };
    let data: HeliosPresentPrivateData = pod_read_unaligned(bytes);
    data.is_valid().then_some(data)
}

pub(crate) fn issue_present_scanout(
    _adapter: &AdapterContext,
    resource_id: u32,
    mut width: u32,
    mut height: u32,
    pitch: u32,
    dxgi_format: u32,
    plane_offset: u64,
    via: u32,
) {
    let (mode_w, mode_h) = _adapter.display_mode();
    if width == 0 {
        width = mode_w;
    }
    if height == 0 {
        height = mode_h;
    }
    let stride = if pitch != 0 {
        pitch
    } else {
        crate::ddi::create_allocation::cross_adapter_pitch(width)
    };
    const DXGI_FORMAT_B8G8R8A8_UNORM: u32 = 87;
    const DXGI_FORMAT_B8G8R8X8_UNORM: u32 = 88;
    rec_named(b"PScVia", via);
    rec_named(b"PScRid", resource_id);
    rec_named(b"PScWH", (width << 16) | (height & 0xFFFF));
    rec_named(b"PScPch", stride);
    rec_named(b"PScOff", plane_offset as u32);
    if dxgi_format != 0
        && dxgi_format != DXGI_FORMAT_B8G8R8A8_UNORM
        && dxgi_format != DXGI_FORMAT_B8G8R8X8_UNORM
    {
        rec_named(b"PScFmt", dxgi_format);
        return;
    }
    // Present source allocations are often intermediate DWM/render targets, not
    // the committed VidPn primary. Promoting them to scanout produces partial
    // overlays/corruption. Keep Present as a scheduler operation; scanout changes
    // only happen from the primary-address path.
    rec_named(b"PScSet", 0xD);
}

/// Mirror present-path tracers into the PASSIVE registry ring.
///
/// Codes:
///   0x1320_NNNN = Present call count
///   0x1321_NNNN = last NumSrcAllocations
///   0x1322_NNNN = last NumDstAllocations
///   0x1323_NNNN = last DmaSize
///   0x1324_NNNN = last PatchLocationListOutSize
///   0x1325_NNNN = low 16 bits of source hDeviceSpecificAllocation
///   0x1326_NNNN = low 16 bits of destination hDeviceSpecificAllocation
///   0x1327_NNNN = last present flags low 16
///   0x1328_NNNN = last returned NTSTATUS low 16
pub fn diag_dump_present_atomics() {
    crate::diag::record(0x1320_0000 | (PRESENT_COUNT.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x1321_0000 | (PRESENT_LAST_SRC_COUNT.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x1322_0000 | (PRESENT_LAST_DST_COUNT.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x1323_0000 | (PRESENT_LAST_DMA_SIZE.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x1324_0000 | (PRESENT_LAST_PATCH_SIZE.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x1325_0000 | (PRESENT_LAST_SRC_OPEN_LOW.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x1326_0000 | (PRESENT_LAST_DST_OPEN_LOW.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x1327_0000 | (PRESENT_LAST_FLAGS.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x1328_0000 | (PRESENT_LAST_STATUS.load(Ordering::Relaxed) & 0xFFFF));
}

pub unsafe extern "C" fn dxgkddi_present(
    _adapter: IN_CONST_HANDLE,
    present: INOUT_PDXGKARG_PRESENT,
) -> NTSTATUS {
    PRESENT_COUNT.fetch_add(1, Ordering::Relaxed);
    if present.is_null() {
        PRESENT_LAST_STATUS.store(STATUS_INVALID_PARAMETER as u32, Ordering::Relaxed);
        return STATUS_INVALID_PARAMETER;
    }

    // `pfnPresentCb` drives this DDI. Returning NOT_SUPPORTED makes dxgkrnl
    // accept the user-mode present call but leaves the cross-adapter/IddCx
    // backing unchanged. Mirror viogpu3d's shape here: validate the present,
    // declare the source/destination allocation references to VidSch, and emit a
    // tiny no-op DMA record so the normal submit/fence path can retire it.
    let args = unsafe { &mut *present };
    PRESENT_LAST_SRC_COUNT.store(args.NumSrcAllocations, Ordering::Relaxed);
    PRESENT_LAST_DST_COUNT.store(args.NumDstAllocations, Ordering::Relaxed);
    PRESENT_LAST_DMA_SIZE.store(args.DmaSize, Ordering::Relaxed);
    PRESENT_LAST_PATCH_SIZE.store(args.PatchLocationListOutSize, Ordering::Relaxed);
    let present_flags = unsafe { args.Flags.__bindgen_anon_1.Value };
    PRESENT_LAST_FLAGS.store(present_flags, Ordering::Relaxed);

    // Unconditional present-path trace (fixed value names survive the ring flood):
    // is DxgkDdiPresent even the hook for the IddCx composition present, and with
    // what flags / src+dst counts / allocation-list presence? PBcall counts calls;
    // its absence means the IddCx present does NOT route through this DDI.
    rec_named(b"PBcall", PRESENT_COUNT.load(Ordering::Relaxed));
    rec_named(b"PBflag", present_flags);
    rec_named(
        b"PBcnt",
        (args.NumSrcAllocations << 16) | (args.NumDstAllocations & 0xFFFF),
    );
    rec_named(
        b"PBalst",
        if unsafe { args.__bindgen_anon_1.pAllocationList }.is_null() {
            0
        } else {
            1
        },
    );

    if (unsafe { args.Flags.__bindgen_anon_1.Value } & (1 << 2)) != 0 {
        PRESENT_LAST_STATUS.store(STATUS_SUCCESS as u32, Ordering::Relaxed);
        return STATUS_SUCCESS;
    }

    let allocation_list = unsafe { args.__bindgen_anon_1.pAllocationList };
    let present_private = unsafe { present_private_data(args) };
    rec_named(b"PBpdsz", args.PrivateDriverDataSize);
    if !allocation_list.is_null() {
        let src = unsafe { allocation_list.add(DXGK_PRESENT_SOURCE_INDEX as usize) };
        let dst = unsafe { allocation_list.add(DXGK_PRESENT_DESTINATION_INDEX as usize) };
        let dst_handle = unsafe { (*dst).hDeviceSpecificAllocation };
        PRESENT_LAST_SRC_OPEN_LOW.store(
            (*src).hDeviceSpecificAllocation as usize as u32,
            Ordering::Relaxed,
        );
        PRESENT_LAST_DST_OPEN_LOW.store(dst_handle as usize as u32, Ordering::Relaxed);

        // Present-blit feasibility trace (read-only). Resolve the composition
        // source + IddCx destination surfaces to their venus resource ids /
        // geometry, and report whether each is a tracked host-visible-mappable
        // blob the KMD could CPU-map for a coherence copy. Fixed value names so
        // the data survives the diag ring flood; read live from the service key.
        let adapter = unsafe { &*(_adapter as *const AdapterContext) };
        let src_scanout = unsafe {
            crate::ddi::create_allocation::present_scanout_alloc_info(
                (*src).hDeviceSpecificAllocation,
            )
        };
        rec_named(
            b"PBsrcH",
            ((*src).hDeviceSpecificAllocation as usize as u32) & 0xFFFF,
        );
        rec_named(b"PBdstH", (dst_handle as usize as u32) & 0xFFFF);
        let src_info = unsafe { present_alloc_info((*src).hDeviceSpecificAllocation) };
        let dst_info = unsafe { present_alloc_info(dst_handle) };
        if let Some(sc) = src_scanout {
            issue_present_scanout(
                adapter,
                sc.resource_id,
                sc.width,
                sc.height,
                sc.pitch,
                sc.dxgi_format,
                sc.plane_offset,
                1,
            );
        }
        if let Some(s) = src_info {
            rec_named(b"PBsrc", s.resource_id);
            rec_named(b"PBsw", s.width);
            rec_named(b"PBsh", s.height);
            let lk = adapter.with_virtio(|v| v.blob_lookup(s.resource_id));
            // 0=untracked, else 0x1_0000 | (mapped<<8) | (size in 4KiB pages, low byte)
            let code = match lk {
                Ok(Some((_owner, size, mapped))) => {
                    0x0001_0000 | ((mapped as u32) << 8) | ((size / 4096) as u32 & 0xFF)
                }
                _ => 0,
            };
            rec_named(b"PBstrk", code);
        } else {
            rec_named(b"PBsrc", 0);
        }
        if let Some(d) = dst_info {
            rec_named(b"PBdst", d.resource_id);
            rec_named(b"PBdw", d.width);
            rec_named(b"PBdh", d.height);
            let lk = adapter.with_virtio(|v| v.blob_lookup(d.resource_id));
            let code = match lk {
                Ok(Some((_owner, size, mapped))) => {
                    0x0001_0000 | ((mapped as u32) << 8) | ((size / 4096) as u32 & 0xFF)
                }
                _ => 0,
            };
            rec_named(b"PBdtrk", code);
        } else {
            rec_named(b"PBdst", 0);
        }
    } else if let Some(sc) = present_private {
        let adapter = unsafe { &*(_adapter as *const AdapterContext) };
        rec_named(b"PBsrc", sc.resource_id);
        rec_named(b"PBsw", sc.width);
        rec_named(b"PBsh", sc.height);
        issue_present_scanout(
            adapter,
            sc.resource_id,
            sc.width,
            sc.height,
            sc.pitch,
            sc.dxgi_format,
            sc.plane_offset,
            2,
        );
    }

    if !args.pPatchLocationListOut.is_null() && args.PatchLocationListOutSize >= 2 {
        let patch = args.pPatchLocationListOut;
        unsafe {
            core::ptr::write_bytes(patch, 0, 2);

            (*patch).AllocationIndex = DXGK_PRESENT_DESTINATION_INDEX;
            (*patch).__bindgen_anon_1.Value = 1;
            (*patch).DriverId = 1;
            (*patch).AllocationOffset = 0;
            (*patch).PatchOffset = 0;
            (*patch).SplitOffset = 0;

            let patch1 = patch.add(1);
            (*patch1).AllocationIndex = DXGK_PRESENT_SOURCE_INDEX;
            (*patch1).__bindgen_anon_1.Value = 2;
            (*patch1).DriverId = 2;
            (*patch1).AllocationOffset = 0;
            (*patch1).PatchOffset = 0;
            (*patch1).SplitOffset = 0;

            args.pPatchLocationListOut = patch.add(2);
        }
    }

    if !args.pDmaBuffer.is_null() {
        const PRESENT_NOP_DWORDS: usize = 4;
        let bytes = (PRESENT_NOP_DWORDS * core::mem::size_of::<u32>()) as UINT;
        if args.DmaSize < bytes {
            PRESENT_LAST_STATUS.store(STATUS_BUFFER_TOO_SMALL as u32, Ordering::Relaxed);
            return STATUS_BUFFER_TOO_SMALL;
        }
        let dma = args.pDmaBuffer as *mut u32;
        unsafe {
            // "HEPR" + source/destination allocation indices. SubmitCommand is
            // still a null engine, but the non-empty DMA buffer is structurally
            // important for the scheduler present path.
            *dma.add(0) = 0x5250_4548;
            *dma.add(1) = DXGK_PRESENT_SOURCE_INDEX;
            *dma.add(2) = DXGK_PRESENT_DESTINATION_INDEX;
            *dma.add(3) = args.Flags.__bindgen_anon_1.Value;
            args.pDmaBuffer = (args.pDmaBuffer as *mut u8).add(bytes as usize).cast();
        }
        args.MultipassOffset = 0;
    }

    PRESENT_LAST_STATUS.store(STATUS_SUCCESS as u32, Ordering::Relaxed);
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_set_pointer_position(
    _adapter: IN_CONST_HANDLE,
    position: IN_CONST_PDXGKARG_SETPOINTERPOSITION,
) -> NTSTATUS {
    crate::diag::record(0x1300_0002);
    if !position.is_null() {
        crate::diag::record(0x1310_0000 | unsafe { (*position).VidPnSourceId & 0xFFFF });
    }
    // SetPointerPosition's legal set does NOT include STATUS_NOT_SUPPORTED — an
    // illegal return here is logged as a driver bug during the modeset (AzureTriage,
    // 36th session). With the display half up, accept the (software-cursor) position
    // as a no-op; render-only never receives this call.
    if unsafe { display_half_on(_adapter) } {
        STATUS_SUCCESS
    } else {
        STATUS_NOT_SUPPORTED
    }
}

pub unsafe extern "C" fn dxgkddi_set_pointer_shape(
    _adapter: IN_CONST_HANDLE,
    shape: IN_CONST_PDXGKARG_SETPOINTERSHAPE,
) -> NTSTATUS {
    crate::diag::record(0x1300_0003);
    if !shape.is_null() {
        crate::diag::record(0x1311_0000 | unsafe { (*shape).VidPnSourceId & 0xFFFF });
    }
    // As with SetPointerPosition: NOT_SUPPORTED is illegal for this DDI. Accept as a
    // no-op with the display half up (the OS software-composes the cursor).
    if unsafe { display_half_on(_adapter) } {
        STATUS_SUCCESS
    } else {
        STATUS_NOT_SUPPORTED
    }
}

/// True when the `DisplayHalf` knob was on at StartDevice (Option A, #1). The
/// VidPn DDIs stay NOT_SUPPORTED (render-only) unless this returns true.
///
/// # Safety
/// `h` is the miniport adapter handle dxgkrnl passes to a display DDI.
unsafe fn display_half_on(h: IN_CONST_HANDLE) -> bool {
    let p = h as *const AdapterContext;
    !p.is_null() && unsafe { (*p).display_half }
}

pub unsafe extern "C" fn dxgkddi_is_supported_vidpn(
    _adapter: IN_CONST_HANDLE,
    is_supported: INOUT_PDXGKARG_ISSUPPORTEDVIDPN,
) -> NTSTATUS {
    crate::diag::record(0x1300_0004);
    if !is_supported.is_null() {
        crate::diag::record(
            0x1312_0000 | unsafe { ((*is_supported).hDesiredVidPn as usize as u32) & 0xFFFF },
        );
    }
    let p = _adapter as *const AdapterContext;
    if p.is_null() || !unsafe { (*p).display_half } {
        return STATUS_GRAPHICS_INVALID_VIDPN;
    }
    if is_supported.is_null() {
        return STATUS_GRAPHICS_INVALID_VIDPN;
    }
    // Diagnostic: latch the max path count across every VidPn the OS validates.
    // `VpISp`>=1 ⇒ the OS DOES propose a 1-path VidPn we accept — so an empty COMMIT
    // (VpCN=0) means the OS rejects activation *after* our TRUE (flip/scanout/MPO
    // side), not a topology-synthesis gap. `VpISp`=0 ⇒ it only ever asks about the
    // empty VidPn (a topology/target problem persists).
    let adapter = unsafe { &*p };
    let pc =
        unsafe { crate::ddi::vidpn::topology_path_count(adapter, (*is_supported).hDesiredVidPn) };
    if pc != u32::MAX {
        static MAX_PC: AtomicU32 = AtomicU32::new(0);
        MAX_PC.fetch_max(pc, Ordering::Relaxed);
        crate::diag::record_named_bytes(b"VpISp", MAX_PC.load(Ordering::Relaxed));
    }
    // A single source + single target adapter can only ever be handed the
    // trivial (or empty) VidPn, so accept it.
    unsafe { (*is_supported).IsVidPnSupported = 1 };
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_recommend_functional_vidpn(
    _adapter: IN_CONST_HANDLE,
    _recommend: IN_CONST_PDXGKARG_RECOMMENDFUNCTIONALVIDPN_CONST,
) -> NTSTATUS {
    crate::diag::record(0x1300_0005);
    if !unsafe { display_half_on(_adapter) } {
        return STATUS_NOT_SUPPORTED;
    }
    // Decline: let the OS synthesize the simple one-path VidPn it then validates
    // via IsSupportedVidPn (enumerating-child-devices-of-a-display-adapter.md).
    crate::ddi::vidpn::STATUS_GRAPHICS_NO_RECOMMENDED_FUNCTIONAL_VIDPN
}

pub unsafe extern "C" fn dxgkddi_enum_vidpn_cofunc_modality(
    _adapter: IN_CONST_HANDLE,
    enum_modality: IN_CONST_PDXGKARG_ENUMVIDPNCOFUNCMODALITY_CONST,
) -> NTSTATUS {
    crate::diag::record(0x1300_0006);
    if !enum_modality.is_null() {
        crate::diag::record(
            0x1313_0000 | unsafe { (*enum_modality).EnumPivotType as u32 & 0xFFFF },
        );
    }
    let p = _adapter as *const AdapterContext;
    if p.is_null() || !unsafe { (*p).display_half } {
        return STATUS_NOT_SUPPORTED;
    }
    let adapter = unsafe { &*p };
    unsafe { crate::ddi::vidpn::enum_cofunc_modality(adapter, enum_modality) }
}

pub unsafe extern "C" fn dxgkddi_set_vidpn_source_visibility(
    _adapter: IN_CONST_HANDLE,
    visibility: IN_CONST_PDXGKARG_SETVIDPNSOURCEVISIBILITY,
) -> NTSTATUS {
    crate::diag::record(0x1300_0007);
    if !visibility.is_null() {
        crate::diag::record(0x1314_0000 | unsafe { (*visibility).VidPnSourceId & 0xFFFF });
    }
    // No scanout: visibility is a no-op we simply accept.
    if unsafe { display_half_on(_adapter) } {
        STATUS_SUCCESS
    } else {
        STATUS_NOT_SUPPORTED
    }
}

pub unsafe extern "C" fn dxgkddi_commit_vidpn(
    _adapter: IN_CONST_HANDLE,
    commit: IN_CONST_PDXGKARG_COMMITVIDPN_CONST,
) -> NTSTATUS {
    crate::diag::record(0x1300_0008);
    if !commit.is_null() {
        crate::diag::record(0x1315_0000 | unsafe { (*commit).AffectedVidPnSourceId & 0xFFFF });
        crate::diag::record(0x1316_0000 | unsafe { (*commit).Flags.PathPoweredOff() & 0xFFFF });
    }
    let p = _adapter as *const AdapterContext;
    if p.is_null() || !unsafe { (*p).display_half } {
        return STATUS_NOT_SUPPORTED;
    }
    crate::diag::record_named_bytes(b"VpCM", 1);
    // Surface the CPU-host-aperture counters from a PASSIVE point on the mode-set
    // path: the raised-IRQL Map path records its atomics (`ChIq`/`ChEi`/`ChIa`/
    // `ChId`/`ChMc`) but cannot write the registry itself. This proves on hardware
    // whether `MapCpuHostAperture` is driven at DISPATCH during activation (the
    // suspected 0xC0000001 source, ETW-confirmed v71).
    crate::ddi::diag_dump_cpu_host_atomics();
    // Inspect + validate the committed VidPn and record whether the OS pinned a
    // source mode on our source (the mode-set-retry-loop resolver — a bare
    // `return SUCCESS` that never checks the pin is exactly viogpu3d's "commit but
    // light nothing" failure). Scanout itself is issued from SetVidPnSourceAddress.
    let adapter = unsafe { &*p };
    crate::ddi::vidpn::legalize_vidpn(unsafe {
        crate::ddi::vidpn::commit_vidpn(adapter, commit as *const DXGKARG_COMMITVIDPN)
    })
}

pub unsafe extern "C" fn dxgkddi_update_active_vidpn_present_path(
    _adapter: IN_CONST_HANDLE,
    path: IN_CONST_PDXGKARG_UPDATEACTIVEVIDPNPRESENTPATH_CONST,
) -> NTSTATUS {
    crate::diag::record(0x1300_0009);
    if !path.is_null() {
        crate::diag::record(
            0x1317_0000 | unsafe { (*path).VidPnPresentPathInfo.VidPnSourceId & 0xFFFF },
        );
        crate::diag::record(
            0x1318_0000 | unsafe { (*path).VidPnPresentPathInfo.VidPnTargetId & 0xFFFF },
        );
    }
    if unsafe { display_half_on(_adapter) } {
        STATUS_SUCCESS
    } else {
        STATUS_NOT_SUPPORTED
    }
}

pub unsafe extern "C" fn dxgkddi_set_vidpn_source_address(
    _adapter: IN_CONST_HANDLE,
    address: IN_CONST_PDXGKARG_SETVIDPNSOURCEADDRESS,
) -> NTSTATUS {
    crate::diag::record(0x1300_000A);
    if !address.is_null() {
        crate::diag::record(0x1319_0000 | unsafe { (*address).VidPnSourceId & 0xFFFF });
    }
    let p = _adapter as *const AdapterContext;
    if p.is_null() || !unsafe { (*p).display_half } {
        return STATUS_NOT_SUPPORTED;
    }
    let adapter = unsafe { &*p };
    crate::diag::record_named_bytes(b"VpSA", 1);
    // Remember the primary's physical address so the CRTC_VSYNC heartbeat reports
    // it (dxgkrnl retires the queued flip whose address matches — viogpu3d
    // `m_sourceAddress`).
    if !address.is_null() {
        let phys = unsafe { (*address).PrimaryAddress.QuadPart };
        adapter
            .last_primary_address
            .store(phys as u64, Ordering::Release);
    }

    // Scan out the DWM-composed primary to virtio-gpu scanout 0 = the QEMU
    // gtk/sdl display (SET_SCANOUT_BLOB + RESOURCE_FLUSH), so the desktop appears
    // in QEMU's own window and the Helios monitor is a real presentable output.
    // The control round-trip WAITS, so only at PASSIVE_LEVEL — a DISPATCH-level
    // VSync flip records the geometry and defers (a later PASSIVE source-address
    // rebinds). Export gate (DISPLAY.md §8): a non-exportable primary → ScSet=0xE.
    if address.is_null() {
        return STATUS_SUCCESS;
    }
    let h_alloc = unsafe { (*address).hAllocation };
    let Some(sc) = (unsafe { crate::ddi::create_allocation::scanout_alloc_info(h_alloc) }) else {
        crate::diag::record_named_bytes(b"ScRid", 0);
        return STATUS_SUCCESS;
    };
    let (mode_w, mode_h) = adapter.display_mode();
    let width = if sc.width != 0 { sc.width } else { mode_w };
    let height = if sc.height != 0 { sc.height } else { mode_h };
    crate::diag::record_named_bytes(b"ScRid", sc.resource_id);
    crate::diag::record_named_bytes(b"ScWH", (width << 16) | (height & 0xFFFF));
    // SAFETY: KeGetCurrentIrql is callable at any IRQL.
    let irql = unsafe { KeGetCurrentIrql() };
    if irql != 0 {
        crate::diag::record_named_bytes(b"ScIrq", irql as u32);
        return STATUS_SUCCESS;
    }
    // Stride MUST match the UMD's actual row pitch (`cross_adapter_pitch`,
    // 256-aligned), NOT `width*4`: for 1896 wide that is 7680 vs 7584, and a wrong
    // stride shears the scan-out so the host reads each row 96 bytes short. Fall
    // back to the same alignment the UMD uses if the allocation carried no pitch.
    let stride = if sc.pitch != 0 {
        sc.pitch
    } else {
        crate::ddi::create_allocation::cross_adapter_pitch(width)
    };
    crate::diag::record_named_bytes(b"ScPch", stride);
    // Resolve the scan-out format from the creator's EXACT DXGI format (the KMD
    // D3DDDIFORMAT is lossy — B8G8R8A8 and R8G8B8A8 both collapse to A8R8G8B8).
    // Preserve A-vs-X on the virtio scanout contract: DRM AR24/XR24 both map to
    // BGRA byte storage, but the host import path sees them as distinct formats.
    const DXGI_FORMAT_B8G8R8A8_UNORM: u32 = 87;
    const DXGI_FORMAT_B8G8R8X8_UNORM: u32 = 88;
    let vformat = if sc.dxgi_format == DXGI_FORMAT_B8G8R8X8_UNORM {
        helios_protocol::VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM
    } else {
        helios_protocol::VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM
    };
    if sc.dxgi_format != 0
        && sc.dxgi_format != DXGI_FORMAT_B8G8R8A8_UNORM
        && sc.dxgi_format != DXGI_FORMAT_B8G8R8X8_UNORM
    {
        crate::diag::record_named_bytes(b"ScFmt", sc.dxgi_format);
    }
    crate::diag::record_named_bytes(b"ScOff", sc.plane_offset as u32);
    if crate::ddi::scanout_diag::rebind_if_forced(adapter, 11) {
        return STATUS_SUCCESS;
    }
    let set = crate::virtio::ctrl::set_scanout_blob(
        adapter,
        sc.resource_id,
        width,
        height,
        vformat,
        stride,
        sc.plane_offset as u32,
    );
    crate::diag::record_named_bytes(b"ScSet", if set.is_ok() { 1 } else { 0xE });
    if set.is_ok() {
        adapter.remember_scanout_blob(sc.resource_id, width, height);
        let flush = crate::virtio::ctrl::resource_flush(adapter, sc.resource_id, width, height);
        crate::diag::record_named_bytes(b"ScFlu", if flush.is_ok() { 1 } else { 0xE });
    }
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_recommend_monitor_modes(
    _adapter: IN_CONST_HANDLE,
    _recommend: IN_CONST_PDXGKARG_RECOMMENDMONITORMODES_CONST,
) -> NTSTATUS {
    crate::diag::record(0x1300_000B);
    let p = _adapter as *const AdapterContext;
    if p.is_null() || !unsafe { (*p).display_half } {
        return STATUS_NOT_SUPPORTED;
    }
    let adapter = unsafe { &*p };
    // Clamp to the DDI's legal return set: an out-of-contract NTSTATUS makes
    // dxgkrnl discard every VidPn (AzureTriage; 36th-session 0-paths root cause).
    crate::ddi::vidpn::legalize_vidpn(unsafe {
        crate::ddi::vidpn::recommend_monitor_modes(adapter, _recommend)
    })
}

pub unsafe extern "C" fn dxgkddi_query_vidpn_hw_capability(
    _adapter: IN_CONST_HANDLE,
    caps: INOUT_PDXGKARG_QUERYVIDPNHWCAPABILITY,
) -> NTSTATUS {
    crate::diag::record(0x1300_000C);
    if !unsafe { display_half_on(_adapter) } {
        return STATUS_NOT_SUPPORTED;
    }
    if caps.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // Advertise no HW interpolation/enhancements: the OS handles scaling/rotation
    // in software (there is no real scanout engine to do it).
    unsafe {
        core::ptr::write_bytes(
            core::ptr::addr_of_mut!((*caps).VidPnHWCaps) as *mut u8,
            0,
            core::mem::size_of::<D3DKMDT_VIDPN_HW_CAPABILITY>(),
        );
    }
    STATUS_SUCCESS
}

/// `DxgkDdiUpdateMonitorLinkInfo` — MANDATORY (non-null) once the adapter reports
/// a monitor target: dxgkrnl's StartAdapter fails the whole adapter with
/// `StartAdapter_DxgkDdiUpdateMonitorLinkInfoIsNull` (Code 43 FAILED_POST_START,
/// AzureTriage-confirmed 2026-07-08) if this slot is NULL. The virtual monitor
/// exposes no special link capabilities (no HDR/DSC/link-rate constraints), so we
/// accept and leave the caller's `MonitorLinkInfo` unchanged.
pub unsafe extern "C" fn dxgkddi_update_monitor_link_info(
    _adapter: IN_CONST_HANDLE,
    link_info: INOUT_PDXGKARG_UPDATEMONITORLINKINFO,
) -> NTSTATUS {
    crate::diag::record(0x1300_0010);
    if !link_info.is_null() {
        crate::diag::record(0x131D_0000 | unsafe { (*link_info).VideoPresentTargetId & 0xFFFF });
    }
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_get_scan_line(
    _adapter: IN_CONST_HANDLE,
    scan_line: INOUT_PDXGKARG_GETSCANLINE,
) -> NTSTATUS {
    crate::diag::record(0x1300_000D);
    if !scan_line.is_null() {
        crate::diag::record(0x131A_0000 | unsafe { (*scan_line).VidPnTargetId & 0xFFFF });
    }
    // NOT_SUPPORTED is illegal for GetScanLine. With the display half up, report a
    // benign "in vertical blank" (no real scanout engine to read a line from).
    if unsafe { display_half_on(_adapter) } {
        if !scan_line.is_null() {
            unsafe {
                (*scan_line).InVerticalBlank = 1;
                (*scan_line).ScanLine = 0;
            }
        }
        STATUS_SUCCESS
    } else {
        STATUS_NOT_SUPPORTED
    }
}

pub unsafe extern "C" fn dxgkddi_stop_device_and_release_post_display_ownership(
    _miniport_device_context: *mut c_void,
    target_id: D3DDDI_VIDEO_PRESENT_TARGET_ID,
    _display_info: PDXGK_DISPLAY_INFORMATION,
) -> NTSTATUS {
    crate::diag::record(0x1300_000E);
    crate::diag::record(0x131B_0000 | (target_id & 0xFFFF));
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_system_display_enable(
    _miniport_device_context: *mut c_void,
    target_id: D3DDDI_VIDEO_PRESENT_TARGET_ID,
    _flags: PDXGKARG_SYSTEM_DISPLAY_ENABLE_FLAGS,
    _width: *mut UINT,
    _height: *mut UINT,
    _color_format: *mut D3DDDIFORMAT,
) -> NTSTATUS {
    crate::diag::record(0x1300_000F);
    crate::diag::record(0x131C_0000 | (target_id & 0xFFFF));
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_system_display_write(
    _miniport_device_context: *mut c_void,
    _source: *mut c_void,
    _source_width: UINT,
    _source_height: UINT,
    _source_stride: UINT,
    _position_x: UINT,
    _position_y: UINT,
) {
}

pub unsafe extern "C" fn dxgkddi_exchange_pre_start_info(
    _adapter: IN_CONST_HANDLE,
    pre_start_info: IN_OUT_PDXGK_PRE_START_INFO,
) -> NTSTATUS {
    crate::diag::record(0x0E00_0001);
    if pre_start_info.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    crate::diag::record(0x0E00_0002);
    STATUS_SUCCESS
}
