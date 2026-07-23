//! Display/VidPn DDIs for the active Helios render+display adapter.
//!
//! Windows identifies the primary through `SetVidPnSourceAddress`; Helios then
//! binds that exact allocation to virtio-gpu scanout. A dedicated LINEAR image
//! remains a compatibility fallback for a primary that is not directly
//! exportable.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use bytemuck::pod_read_unaligned;
use helios_protocol::{HeliosPresentPrivateData, HELIOS_PRESENT_PRIVATE_FLAG_DIRECT_SCANOUT};

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
static PRESENT_SCANOUT_SUCCESS_COUNT: AtomicU32 = AtomicU32::new(0);
pub(crate) static VIDPN_SOURCE_ADDRESS_COUNT: AtomicU32 = AtomicU32::new(0);

fn production_linear_scanout(
    adapter: &AdapterContext,
    width: u32,
    height: u32,
) -> Result<crate::ddi::create_allocation::ScanoutInfo, NTSTATUS> {
    use core::sync::atomic::Ordering;

    let resource_id = adapter.dedicated_scanout_resource.load(Ordering::Acquire);
    let image_id = adapter.dedicated_scanout_image.load(Ordering::Acquire);
    let wh = adapter.primary_scanout_wh.load(Ordering::Relaxed);
    let live = resource_id != 0
        && image_id != 0
        && wh == (((width as u64) << 32) | height as u64)
        && adapter
            .with_virtio(|v| v.resource_is_live(resource_id))
            .unwrap_or(false);
    if live {
        let layout = adapter.primary_scanout_layout.load(Ordering::Relaxed);
        return Ok(crate::ddi::create_allocation::ScanoutInfo {
            resource_id,
            width,
            height,
            pitch: (layout >> 32) as u32,
            dxgi_format: adapter.primary_scanout_dxgi_format.load(Ordering::Relaxed),
            plane_offset: layout as u32 as u64,
            venus_alloc_size: adapter.primary_scanout_alloc_size.load(Ordering::Relaxed),
            memory_type_index: adapter.primary_scanout_memory_type.load(Ordering::Relaxed),
            direct_scanout: false,
        });
    }

    let scanout = match adapter.with_venus_client(|client| {
        client.allocate_linear_scanout_image_blob(adapter, width, height)
    }) {
        Ok(Ok(scanout)) => scanout,
        Ok(Err(_)) => {
            crate::diag::record_named_bytes(b"CpErr", 1);
            return Err(STATUS_NO_MEMORY);
        }
        Err(_) => {
            crate::diag::record_named_bytes(b"CpErr", 2);
            return Err(STATUS_DEVICE_NOT_READY);
        }
    };

    const DXGI_FORMAT_B8G8R8A8_UNORM: u32 = 87;
    adapter.remember_primary_scanout(
        scanout.blob.res_id,
        width,
        height,
        scanout.row_pitch,
        scanout.plane_offset,
        scanout.blob.size,
        scanout.memory_type_index,
        DXGI_FORMAT_B8G8R8A8_UNORM,
    );
    adapter
        .dedicated_scanout_memory
        .store(scanout.blob.blob_id, Ordering::Relaxed);
    adapter
        .dedicated_scanout_image
        .store(scanout.image_id, Ordering::Relaxed);
    adapter
        .dedicated_scanout_resource
        .store(scanout.blob.res_id, Ordering::Release);
    crate::diag::record_named_bytes(b"CpRid", scanout.blob.res_id);
    crate::diag::record_named_bytes(b"CpBid", scanout.blob.blob_id as u32);
    crate::diag::record_named_bytes(b"CpPch", scanout.row_pitch);
    Ok(crate::ddi::create_allocation::ScanoutInfo {
        resource_id: scanout.blob.res_id,
        width,
        height,
        pitch: scanout.row_pitch,
        dxgi_format: DXGI_FORMAT_B8G8R8A8_UNORM,
        plane_offset: scanout.plane_offset as u64,
        venus_alloc_size: scanout.blob.size,
        memory_type_index: scanout.memory_type_index,
        direct_scanout: false,
    })
}

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
    adapter: &AdapterContext,
    resource_id: u32,
    mut width: u32,
    mut height: u32,
    pitch: u32,
    dxgi_format: u32,
    plane_offset: u64,
    venus_alloc_size: u64,
    direct_scanout: bool,
    via: u32,
) -> bool {
    let (mode_w, mode_h) = adapter.display_mode();
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
    if dxgi_format != 0
        && dxgi_format != DXGI_FORMAT_B8G8R8A8_UNORM
        && dxgi_format != DXGI_FORMAT_B8G8R8X8_UNORM
    {
        rec_named(b"PScVia", via);
        rec_named(b"PScRid", resource_id);
        rec_named(b"PScFmt", dxgi_format);
        return false;
    }
    // The exact pPrimaryDesc marker survives in AllocationContext. Ordinary
    // app/intermediate present sources remain rejected; only the Windows-owned
    // desktop primary can enter this asynchronous scanout path.
    if !direct_scanout {
        rec_named(b"PScVia", via);
        rec_named(b"PScRid", resource_id);
        rec_named(b"PScSet", 0xD);
        return false;
    }
    let min_size = plane_offset.saturating_add((stride as u64).saturating_mul(height as u64));
    if width != mode_w
        || height != mode_h
        || stride < width.saturating_mul(4)
        || stride & 3 != 0
        || plane_offset > u32::MAX as u64
        || venus_alloc_size < min_size
    {
        rec_named(b"PScVia", via);
        rec_named(b"PScRid", resource_id);
        rec_named(b"PScWH", (width << 16) | (height & 0xFFFF));
        rec_named(b"PScPch", stride);
        rec_named(b"PScOff", plane_offset as u32);
        rec_named(b"PScSet", 0xE3);
        return false;
    }
    let vformat = helios_protocol::VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM;
    adapter.publish_scanout_candidate(
        resource_id,
        width,
        height,
        vformat,
        stride,
        plane_offset as u32,
    );
    // Do not flush here: DxgkDdiRender captures the current Venus wire-fence
    // watermark. The used-ring DPC emits the coalesced dirty edge only after
    // every preceding Venus submission retires.
    let n = PRESENT_SCANOUT_SUCCESS_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 8 || n & 0x3FF == 0 {
        rec_named(b"PScVia", via);
        rec_named(b"PScRid", resource_id);
        rec_named(b"PScWH", (width << 16) | (height & 0xFFFF));
        rec_named(b"PScPch", stride);
        rec_named(b"PScOff", plane_offset as u32);
        rec_named(b"PScSet", 2);
    }
    true
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
            let _ = issue_present_scanout(
                adapter,
                sc.resource_id,
                sc.width,
                sc.height,
                sc.pitch,
                sc.dxgi_format,
                sc.plane_offset,
                sc.venus_alloc_size,
                sc.direct_scanout,
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
        let direct_scanout = sc.reserved & HELIOS_PRESENT_PRIVATE_FLAG_DIRECT_SCANOUT != 0;
        let required_size = sc
            .plane_offset
            .saturating_add((sc.pitch as u64).saturating_mul(sc.height as u64));
        let _ = issue_present_scanout(
            adapter,
            sc.resource_id,
            sc.width,
            sc.height,
            sc.pitch,
            sc.dxgi_format,
            sc.plane_offset,
            required_size,
            direct_scanout,
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
        let bytes = core::mem::size_of::<helios_protocol::HeliosPresentRefreshCmd>() as UINT;
        if args.DmaSize < bytes {
            PRESENT_LAST_STATUS.store(STATUS_BUFFER_TOO_SMALL as u32, Ordering::Relaxed);
            return STATUS_BUFFER_TOO_SMALL;
        }
        let command = helios_protocol::HeliosPresentRefreshCmd {
            magic: helios_protocol::HELIOS_PRESENT_REFRESH_MAGIC,
            version: helios_protocol::HELIOS_PRESENT_REFRESH_VERSION,
            source_index: DXGK_PRESENT_SOURCE_INDEX,
            destination_index: DXGK_PRESENT_DESTINATION_INDEX,
        };
        unsafe {
            // Keep the DMA record structurally non-empty, and give Render an
            // unambiguous per-present dirty edge for the stable LINEAR target.
            // Never reuse the typed allocation command's HEPR magic here.
            core::ptr::write_unaligned(
                args.pDmaBuffer
                    .cast::<helios_protocol::HeliosPresentRefreshCmd>(),
                command,
            );
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
    let source_address_n = VIDPN_SOURCE_ADDRESS_COUNT
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    let trace_tick = source_address_n == 1 || source_address_n % 600 == 0;
    if trace_tick {
        crate::diag::record_named_bytes(b"VpSA", source_address_n);
    }
    // Remember the primary's physical address so the CRTC_VSYNC heartbeat reports
    // it (dxgkrnl retires the queued flip whose address matches — viogpu3d
    // `m_sourceAddress`).
    if !address.is_null() {
        let phys = unsafe { (*address).PrimaryAddress.QuadPart };
        adapter
            .last_primary_address
            .store(phys as u64, Ordering::Release);
    }

    // Windows names the authoritative desktop primary here. This—not resource
    // dimensions, OM bindings, process name, or an arbitrary Present call—is the
    // only allocation the KMD may bind directly or copy into the fallback image.
    if address.is_null() {
        return STATUS_SUCCESS;
    }
    let h_alloc = unsafe { (*address).hAllocation };
    let Some(source) = (unsafe { crate::ddi::create_allocation::scanout_alloc_info(h_alloc) })
    else {
        crate::diag::record_named_bytes(b"ScRid", 0);
        return STATUS_SUCCESS;
    };
    let (mode_w, mode_h) = adapter.display_mode();
    let width = if source.width != 0 {
        source.width
    } else {
        mode_w
    };
    let height = if source.height != 0 {
        source.height
    } else {
        mode_h
    };
    if trace_tick {
        crate::diag::record_named_bytes(b"ScSrc", source.resource_id);
        crate::diag::record_named_bytes(b"ScWH", (width << 16) | (height & 0xFFFF));
        crate::diag::record_named_bytes(b"ScDir", source.direct_scanout as u32);
    }
    // SAFETY: KeGetCurrentIrql is callable at any IRQL.
    let irql = unsafe { KeGetCurrentIrql() };
    if irql != 0 {
        crate::diag::record_named_bytes(b"ScIrq", irql as u32);
        return STATUS_SUCCESS;
    }
    if width != mode_w || height != mode_h {
        crate::diag::record_named_bytes(b"ScSet", 0xD);
        return STATUS_INVALID_PARAMETER;
    }
    // A UMD-created exact pPrimaryDesc may already have the proven scan-out
    // shape: DMA_BUF-exportable, dedicated device-local memory, and validated
    // extent/metadata. It may be the current plain OPTIMAL export; QEMU validates
    // that opaque native layout against the original blob allocation size.
    // Other primaries retain the adapter-owned LINEAR target + GPU-copy fallback.
    let target = if source.direct_scanout {
        let min_size = source
            .plane_offset
            .saturating_add((source.pitch as u64).saturating_mul(height as u64));
        let valid = source.pitch >= width.saturating_mul(4)
            && source.pitch & 3 == 0
            && source.plane_offset <= u32::MAX as u64
            && source.venus_alloc_size >= min_size
            && matches!(source.dxgi_format, 87 | 88);
        if !valid {
            crate::diag::record_named_bytes(b"ScSet", 0xE3);
            return STATUS_INVALID_PARAMETER;
        }
        source
    } else {
        match production_linear_scanout(adapter, width, height) {
            Ok(target) => target,
            Err(status) => {
                crate::diag::record_named_bytes(b"ScSet", 0xE1);
                return status;
            }
        }
    };
    // Stride MUST match the UMD's actual row pitch (`cross_adapter_pitch`,
    // 256-aligned), NOT `width*4`: for 1896 wide that is 7680 vs 7584, and a wrong
    // stride shears the scan-out so the host reads each row 96 bytes short. Fall
    // back to the same alignment the UMD uses if the allocation carried no pitch.
    let stride = if target.pitch != 0 {
        target.pitch
    } else {
        crate::ddi::create_allocation::cross_adapter_pitch(width)
    };
    // Resolve the scan-out format from the creator's EXACT DXGI format (the KMD
    // D3DDDIFORMAT is lossy — B8G8R8A8 and R8G8B8A8 both collapse to A8R8G8B8).
    // Preserve A-vs-X on the virtio scanout contract: DRM AR24/XR24 both map to
    // BGRA byte storage, but the host import path sees them as distinct formats.
    const DXGI_FORMAT_B8G8R8A8_UNORM: u32 = 87;
    const DXGI_FORMAT_B8G8R8X8_UNORM: u32 = 88;
    let vformat = helios_protocol::VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM;
    if source.dxgi_format != 0
        && source.dxgi_format != DXGI_FORMAT_B8G8R8A8_UNORM
        && source.dxgi_format != DXGI_FORMAT_B8G8R8X8_UNORM
    {
        crate::diag::record_named_bytes(b"ScFmt", source.dxgi_format);
    }
    if crate::ddi::scanout_diag::rebind_if_forced(adapter, 11) {
        return STATUS_SUCCESS;
    }
    let bound_wh = adapter.active_scanout_wh.load(Ordering::Acquire);
    let already_bound = adapter.active_scanout_resource.load(Ordering::Acquire)
        == target.resource_id
        && bound_wh == (((width as u64) << 32) | height as u64);
    if !already_bound || trace_tick {
        crate::diag::record_named_bytes(b"ScRid", target.resource_id);
        crate::diag::record_named_bytes(b"ScPch", stride);
        crate::diag::record_named_bytes(b"ScOff", target.plane_offset as u32);
    }
    if !already_bound {
        let set = crate::virtio::ctrl::set_scanout_blob(
            adapter,
            target.resource_id,
            width,
            height,
            vformat,
            stride,
            target.plane_offset as u32,
        );
        if set.is_err() {
            crate::diag::record_named_bytes(b"ScSet", 0xE);
            return STATUS_DEVICE_NOT_READY;
        }
        // Keep the adapter-owned fallback cache separate from a rotating DWM
        // direct primary. The latter is tracked by active_scanout_resource and
        // dies with its WDDM allocation; publishing it here would overwrite the
        // durable target's cached Venus identity.
        if !target.direct_scanout {
            adapter.remember_primary_scanout(
                target.resource_id,
                width,
                height,
                stride,
                target.plane_offset as u32,
                target.venus_alloc_size,
                target.memory_type_index,
                target.dxgi_format,
            );
        }
        adapter.remember_scanout_blob(target.resource_id, width, height);
        crate::diag::record_named_bytes(b"ScPub", target.resource_id);
    }
    if !already_bound || trace_tick {
        crate::diag::record_named_bytes(b"ScSet", 1);
    }

    if source.resource_id == target.resource_id {
        // True zero-copy primary. Never submit vkCmdCopyImage with the same
        // image as source and destination. SetVidPn only binds/publishes the
        // candidate; the matching Render marker and used-ring retirement are
        // the sole producers of the dirty edge. This prevents the host from
        // sampling an OPTIMAL primary before DWM's Venus work completes.
        if !already_bound {
            crate::diag::record_named_bytes(b"ScCpy", 2);
            crate::diag::record_named_bytes(b"ScFlu", 3);
        }
        return STATUS_SUCCESS;
    }

    let target_image_id = adapter.dedicated_scanout_image.load(Ordering::Acquire);
    if target_image_id == 0 {
        crate::diag::record_named_bytes(b"ScSet", 0xE2);
        return STATUS_DEVICE_NOT_READY;
    }

    // The returned outer wire fence completes in the ring-1 GPU domain. Its DPC
    // callback—not this enqueue—marks scanout dirty and wakes the coalescing
    // async RESOURCE_FLUSH worker, so VNC never samples ahead of the copy.
    match unsafe {
        crate::ddi::create_allocation::submit_primary_scanout_copy(
            adapter,
            h_alloc,
            target_image_id,
            width,
            height,
        )
    } {
        Ok(fence) => {
            if !already_bound {
                crate::diag::record_named_bytes(b"ScCpy", 1);
                crate::diag::record_named_bytes(b"ScFnc", fence as u32);
                crate::diag::record_named_bytes(b"ScFlu", 2); // async completion path
            }
        }
        Err(status) => {
            crate::diag::record_named_bytes(b"ScCpy", 0xE);
            return status;
        }
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
