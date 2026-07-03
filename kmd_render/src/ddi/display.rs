//! Display/VidPn DDIs required for a complete WDDM table shape.
//!
//! Helios targets render-only WDDM. These callbacks therefore do not implement
//! scanout; they make unsupported display paths explicit instead of leaving a
//! large part of the display-miniport table NULL during bring-up.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::adapter::AdapterContext;
use crate::ddi::create_allocation::present_alloc_info;
use crate::dxgk::*;

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
    if !allocation_list.is_null() {
        let src = unsafe { allocation_list.add(DXGK_PRESENT_SOURCE_INDEX as usize) };
        let dst = unsafe { allocation_list.add(DXGK_PRESENT_DESTINATION_INDEX as usize) };
        PRESENT_LAST_SRC_OPEN_LOW.store(
            (*src).hDeviceSpecificAllocation as usize as u32,
            Ordering::Relaxed,
        );
        PRESENT_LAST_DST_OPEN_LOW.store(
            (*dst).hDeviceSpecificAllocation as usize as u32,
            Ordering::Relaxed,
        );

        // Present-blit feasibility trace (read-only). Resolve the composition
        // source + IddCx destination surfaces to their venus resource ids /
        // geometry, and report whether each is a tracked host-visible-mappable
        // blob the KMD could CPU-map for a coherence copy. Fixed value names so
        // the data survives the diag ring flood; read live from the service key.
        let adapter = unsafe { &*(_adapter as *const AdapterContext) };
        let src_info = unsafe { present_alloc_info((*src).hDeviceSpecificAllocation) };
        let dst_info = unsafe { present_alloc_info((*dst).hDeviceSpecificAllocation) };
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
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_set_pointer_shape(
    _adapter: IN_CONST_HANDLE,
    shape: IN_CONST_PDXGKARG_SETPOINTERSHAPE,
) -> NTSTATUS {
    crate::diag::record(0x1300_0003);
    if !shape.is_null() {
        crate::diag::record(0x1311_0000 | unsafe { (*shape).VidPnSourceId & 0xFFFF });
    }
    STATUS_NOT_SUPPORTED
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
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_recommend_functional_vidpn(
    _adapter: IN_CONST_HANDLE,
    _recommend: IN_CONST_PDXGKARG_RECOMMENDFUNCTIONALVIDPN_CONST,
) -> NTSTATUS {
    crate::diag::record(0x1300_0005);
    STATUS_NOT_SUPPORTED
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
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_set_vidpn_source_visibility(
    _adapter: IN_CONST_HANDLE,
    visibility: IN_CONST_PDXGKARG_SETVIDPNSOURCEVISIBILITY,
) -> NTSTATUS {
    crate::diag::record(0x1300_0007);
    if !visibility.is_null() {
        crate::diag::record(0x1314_0000 | unsafe { (*visibility).VidPnSourceId & 0xFFFF });
    }
    STATUS_NOT_SUPPORTED
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
    STATUS_NOT_SUPPORTED
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
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_set_vidpn_source_address(
    _adapter: IN_CONST_HANDLE,
    address: IN_CONST_PDXGKARG_SETVIDPNSOURCEADDRESS,
) -> NTSTATUS {
    crate::diag::record(0x1300_000A);
    if !address.is_null() {
        crate::diag::record(0x1319_0000 | unsafe { (*address).VidPnSourceId & 0xFFFF });
    }
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_recommend_monitor_modes(
    _adapter: IN_CONST_HANDLE,
    _recommend: IN_CONST_PDXGKARG_RECOMMENDMONITORMODES_CONST,
) -> NTSTATUS {
    crate::diag::record(0x1300_000B);
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_query_vidpn_hw_capability(
    _adapter: IN_CONST_HANDLE,
    _caps: INOUT_PDXGKARG_QUERYVIDPNHWCAPABILITY,
) -> NTSTATUS {
    crate::diag::record(0x1300_000C);
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_get_scan_line(
    _adapter: IN_CONST_HANDLE,
    scan_line: INOUT_PDXGKARG_GETSCANLINE,
) -> NTSTATUS {
    crate::diag::record(0x1300_000D);
    if !scan_line.is_null() {
        crate::diag::record(0x131A_0000 | unsafe { (*scan_line).VidPnTargetId & 0xFFFF });
    }
    STATUS_NOT_SUPPORTED
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
