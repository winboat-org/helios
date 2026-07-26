//! Display/VidPn DDIs for the active Helios render+display adapter.
//!
//! Windows identifies the primary through `SetVidPnSourceAddress`; Helios then
//! binds that exact allocation to virtio-gpu scanout. A dedicated LINEAR image
//! remains a compatibility fallback for a primary that is not directly
//! exportable.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use bytemuck::pod_read_unaligned;
use helios_protocol::{
    HeliosPresentPrivateData, HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY, HELIOS_WDDM_ALLOC_KIND_STANDARD,
};

use crate::adapter::AdapterContext;
use crate::ddi::create_allocation::{present_alloc_info, PresentAllocationStorage};
use crate::ddi::present_packet::{
    PresentAllocations, PresentSubmissionPrivate, STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER,
};
use crate::device::ContextHandleRef;
use crate::dxgk::*;
use crate::virtio::venus::{OptimalPresentImageDesc, PresentBufferDesc, PresentDestinationDesc};
use crate::virtio::VirtioError;
use wdk_sys::ntddk::KeGetCurrentIrql;

pub static PRESENT_COUNT: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_SRC_COUNT: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_DST_COUNT: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_DMA_SIZE: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_PATCH_SIZE: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_SRC_OPEN_LOW: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_DST_OPEN_LOW: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_FLAGS: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_STATUS: AtomicU32 = AtomicU32::new(0);
pub(crate) static VIDPN_SOURCE_ADDRESS_COUNT: AtomicU32 = AtomicU32::new(0);

fn virtio_scanout_format(dxgi_format: u32) -> Option<u32> {
    match dxgi_format {
        0 | 88 => Some(helios_protocol::VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM),
        28 => Some(helios_protocol::VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM),
        87 => Some(helios_protocol::VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM),
        _ => None,
    }
}

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
            primary_segment: 0,
            primary_address: 0,
            primary_flags: 0,
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
        primary_segment: 0,
        primary_address: 0,
        primary_flags: 0,
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
    h_context: IN_CONST_HANDLE,
    present: INOUT_PDXGKARG_PRESENT,
) -> NTSTATUS {
    let status = unsafe { dxgkddi_present_inner(h_context, present) };
    // Fixed-name telemetry survives the steady-state registry ring flood and
    // proves whether a failing UMD pfnPresentCb originated in this DDI.
    crate::diag::record_named_bytes(b"PBRet", status as u32);
    status
}

unsafe fn dxgkddi_present_inner(
    h_context: IN_CONST_HANDLE,
    present: INOUT_PDXGKARG_PRESENT,
) -> NTSTATUS {
    PRESENT_COUNT.fetch_add(1, Ordering::Relaxed);
    if present.is_null() {
        PRESENT_LAST_STATUS.store(STATUS_INVALID_PARAMETER as u32, Ordering::Relaxed);
        return STATUS_INVALID_PARAMETER;
    }

    // `pfnPresentCb` drives this DDI. Validate the exact allocations supplied by
    // dxgkrnl. BLTs produce a scheduler submission below; MMIO flips do not
    // generate a DMA buffer and are completed through SetVidPnSourceAddress.
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
    crate::diag::record_named_bytes(b"PBcall", PRESENT_COUNT.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PBflag", present_flags);
    crate::diag::record_named_bytes(
        b"PBcnt",
        (args.NumSrcAllocations << 16) | (args.NumDstAllocations & 0xFFFF),
    );
    crate::diag::record_named_bytes(
        b"PBalst",
        if unsafe { args.__bindgen_anon_1.pAllocationList }.is_null() {
            0
        } else {
            1
        },
    );
    crate::diag::record_named_bytes(b"PBDma", args.DmaSize);
    crate::diag::record_named_bytes(b"PBPatch", args.PatchLocationListOutSize);

    let allocation_list = unsafe { args.__bindgen_anon_1.pAllocationList };
    let present_allocations = unsafe { PresentAllocations::from_allocation_list(allocation_list) };
    let present_private = unsafe { present_private_data(args) };
    crate::diag::record_named_bytes(b"PBpdsz", args.PrivateDriverDataSize);
    crate::diag::record_named_bytes(b"PBkpsz", args.DmaBufferPrivateDataSize);
    let present_context = unsafe { ContextHandleRef::from_raw(h_context) };
    let adapter = present_context.as_ref().and_then(ContextHandleRef::adapter);
    let src_handle = present_allocations
        .source()
        .map(|allocation| allocation.handle())
        .unwrap_or(core::ptr::null_mut());
    let dst_handle = present_allocations
        .destination()
        .map(|allocation| allocation.handle())
        .unwrap_or(core::ptr::null_mut());
    let src_info = unsafe { present_alloc_info(src_handle) };
    let dst_info = unsafe { present_alloc_info(dst_handle) };
    if !allocation_list.is_null() {
        PRESENT_LAST_SRC_OPEN_LOW.store(src_handle as usize as u32, Ordering::Relaxed);
        PRESENT_LAST_DST_OPEN_LOW.store(dst_handle as usize as u32, Ordering::Relaxed);

        // Present-blit feasibility trace (read-only). Resolve the composition
        // source + IddCx destination surfaces to their venus resource ids /
        // geometry, and report whether each is a tracked host-visible-mappable
        // blob the KMD could CPU-map for a coherence copy. Fixed value names so
        // the data survives the diag ring flood; read live from the service key.
        crate::diag::record_named_bytes(b"PBsrcH", (src_handle as usize as u32) & 0xFFFF);
        crate::diag::record_named_bytes(b"PBdstH", (dst_handle as usize as u32) & 0xFFFF);
        if let Some(s) = src_info {
            crate::diag::record_named_bytes(b"PBsRtA", s.runtime_allocation);
            crate::diag::record_named_bytes(b"PBsrc", s.resource_id);
            crate::diag::record_named_bytes(b"PBsw", s.width);
            crate::diag::record_named_bytes(b"PBsh", s.height);
            crate::diag::record_named_bytes(b"PBsPch", s.pitch);
            crate::diag::record_named_bytes(b"PBsFmt", s.dxgi_format);
            crate::diag::record_named_bytes(b"PBsD3F", s.format);
            crate::diag::record_named_bytes(b"PBsBnd", s.bind_flags);
            crate::diag::record_named_bytes(b"PBsSto", s.storage as u32);
            crate::diag::record_named_bytes(b"PBsKnd", s.kind);
            crate::diag::record_named_bytes(b"PBsMt", s.memory_type_index);
            crate::diag::record_named_bytes(b"PBsSz", s.venus_alloc_size as u32);
            crate::diag::record_named_bytes(b"PBsStd", s.standard_allocation_type);
            crate::diag::record_named_bytes(b"PBsGdi", s.standard_gdi_surface_type);
            crate::diag::record_named_bytes(b"PBsOF", s.open_flags);
            crate::diag::record_named_bytes(b"PBsRA", u32::from(s.resource_associated));
            crate::diag::record_named_bytes(b"PBsAPS", s.allocation_private_size);
            crate::diag::record_named_bytes(b"PBsRPS", s.resource_private_size);
            let lk = adapter
                .and_then(|adapter| adapter.with_virtio(|v| v.blob_lookup(s.resource_id)).ok());
            // 0=untracked, else 0x1_0000 | (mapped<<8) | (size in 4KiB pages, low byte)
            let code = match lk {
                Some(Some((_owner, size, mapped))) => {
                    0x0001_0000 | ((mapped as u32) << 8) | ((size / 4096) as u32 & 0xFF)
                }
                _ => 0,
            };
            crate::diag::record_named_bytes(b"PBstrk", code);
        } else {
            crate::diag::record_named_bytes(b"PBsrc", 0);
        }
        if let Some(d) = dst_info {
            crate::diag::record_named_bytes(b"PBdRtA", d.runtime_allocation);
            crate::diag::record_named_bytes(b"PBdst", d.resource_id);
            crate::diag::record_named_bytes(b"PBdw", d.width);
            crate::diag::record_named_bytes(b"PBdh", d.height);
            crate::diag::record_named_bytes(b"PBdPch", d.pitch);
            crate::diag::record_named_bytes(b"PBdFmt", d.dxgi_format);
            crate::diag::record_named_bytes(b"PBdD3F", d.format);
            crate::diag::record_named_bytes(b"PBdBnd", d.bind_flags);
            crate::diag::record_named_bytes(b"PBdSto", d.storage as u32);
            crate::diag::record_named_bytes(b"PBdKnd", d.kind);
            crate::diag::record_named_bytes(b"PBdMt", d.memory_type_index);
            crate::diag::record_named_bytes(b"PBdSz", d.venus_alloc_size as u32);
            crate::diag::record_named_bytes(b"PBdStd", d.standard_allocation_type);
            crate::diag::record_named_bytes(b"PBdGdi", d.standard_gdi_surface_type);
            crate::diag::record_named_bytes(b"PBdOF", d.open_flags);
            crate::diag::record_named_bytes(b"PBdRA", u32::from(d.resource_associated));
            crate::diag::record_named_bytes(b"PBdAPS", d.allocation_private_size);
            crate::diag::record_named_bytes(b"PBdRPS", d.resource_private_size);
            let lk = adapter
                .and_then(|adapter| adapter.with_virtio(|v| v.blob_lookup(d.resource_id)).ok());
            let code = match lk {
                Some(Some((_owner, size, mapped))) => {
                    0x0001_0000 | ((mapped as u32) << 8) | ((size / 4096) as u32 & 0xFF)
                }
                _ => 0,
            };
            crate::diag::record_named_bytes(b"PBdtrk", code);
        } else {
            crate::diag::record_named_bytes(b"PBdst", 0);
        }

        // DXGK_PRESENTFLAGS.Blt is bit 0. Dxgkrnl has already resolved both
        // fixed allocation-list entries to our typed open handles. Perform the
        // actual full-surface source -> destination copy before emitting the
        // scheduler marker; a no-op Present leaves DWM's shared render target
        // black even though the application's source rendered correctly.
        if present_flags & 1 != 0 {
            let bytes = core::mem::size_of::<helios_protocol::HeliosPresentRefreshCmd>() as UINT;
            if args.pDmaBuffer.is_null() || args.DmaSize < bytes {
                PRESENT_LAST_STATUS.store(
                    STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER as u32,
                    Ordering::Relaxed,
                );
                return STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER;
            }
            if args.pDmaBufferPrivateData.is_null()
                || (args.DmaBufferPrivateDataSize as usize)
                    < core::mem::size_of::<PresentSubmissionPrivate>()
            {
                PRESENT_LAST_STATUS.store(
                    STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER as u32,
                    Ordering::Relaxed,
                );
                return STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER;
            }
            if let Err(status) = present_allocations.validate_patch_capacity(args) {
                PRESENT_LAST_STATUS.store(status as u32, Ordering::Relaxed);
                return status;
            }

            let (Some(adapter), Some(source), Some(destination)) = (adapter, src_info, dst_info)
            else {
                crate::diag::record_named_bytes(b"PBCpy", 0xE1);
                PRESENT_LAST_STATUS.store(STATUS_INVALID_PARAMETER as u32, Ordering::Relaxed);
                return STATUS_INVALID_PARAMETER;
            };
            let source_dxgi_format = source.resolved_dxgi_format();
            let destination_dxgi_format = destination.resolved_dxgi_format();
            let (Some(source_dxgi_format), Some(destination_dxgi_format)) =
                (source_dxgi_format, destination_dxgi_format)
            else {
                crate::diag::record_named_bytes(b"PBCpy", 0xE2);
                PRESENT_LAST_STATUS.store(STATUS_INVALID_PARAMETER as u32, Ordering::Relaxed);
                return STATUS_INVALID_PARAMETER;
            };
            if source.kind != HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY {
                crate::diag::record_named_bytes(b"PBCpy", 0xE6);
                PRESENT_LAST_STATUS.store(STATUS_INVALID_PARAMETER as u32, Ordering::Relaxed);
                return STATUS_INVALID_PARAMETER;
            }
            let source_desc = match source.storage {
                PresentAllocationStorage::OptimalCrossContextImage => {
                    OptimalPresentImageDesc::new_cross_context_dma_buf(
                        source.resource_id,
                        source.venus_alloc_size,
                        source.memory_type_index,
                        source.width,
                        source.height,
                        source.bind_flags,
                        source_dxgi_format,
                    )
                }
                PresentAllocationStorage::OptimalOpaqueFdImage => {
                    OptimalPresentImageDesc::new_opaque_fd(
                        source.resource_id,
                        source.venus_alloc_size,
                        source.memory_type_index,
                        source.width,
                        source.height,
                        source.bind_flags,
                        source_dxgi_format,
                    )
                }
                PresentAllocationStorage::PitchedStandardBuffer => None,
            };
            let destination_desc = match destination.storage {
                PresentAllocationStorage::PitchedStandardBuffer
                    if destination.kind == HELIOS_WDDM_ALLOC_KIND_STANDARD =>
                {
                    PresentBufferDesc::new(
                        destination.resource_id,
                        destination.venus_alloc_size,
                        destination.memory_type_index,
                        destination.width,
                        destination.height,
                        destination.pitch,
                        destination_dxgi_format,
                    )
                    .map(PresentDestinationDesc::StandardBuffer)
                }
                PresentAllocationStorage::OptimalCrossContextImage => {
                    OptimalPresentImageDesc::new_cross_context_dma_buf(
                        destination.resource_id,
                        destination.venus_alloc_size,
                        destination.memory_type_index,
                        destination.width,
                        destination.height,
                        destination.bind_flags,
                        destination_dxgi_format,
                    )
                    .map(PresentDestinationDesc::OptimalImage)
                }
                PresentAllocationStorage::OptimalOpaqueFdImage => {
                    OptimalPresentImageDesc::new_opaque_fd(
                        destination.resource_id,
                        destination.venus_alloc_size,
                        destination.memory_type_index,
                        destination.width,
                        destination.height,
                        destination.bind_flags,
                        destination_dxgi_format,
                    )
                    .map(PresentDestinationDesc::OptimalImage)
                }
                PresentAllocationStorage::PitchedStandardBuffer => None,
            };
            let (Some(source_desc), Some(destination_desc)) = (source_desc, destination_desc)
            else {
                crate::diag::record_named_bytes(b"PBCpy", 0xE2);
                PRESENT_LAST_STATUS.store(STATUS_INVALID_PARAMETER as u32, Ordering::Relaxed);
                return STATUS_INVALID_PARAMETER;
            };
            if source.width != destination.width || source.height != destination.height {
                crate::diag::record_named_bytes(b"PBCpy", 0xE3);
                PRESENT_LAST_STATUS.store(STATUS_INVALID_PARAMETER as u32, Ordering::Relaxed);
                return STATUS_INVALID_PARAMETER;
            }

            let copy = adapter.with_venus_client(|client| {
                client.submit_present_blt(adapter, source_desc, destination_desc)
            });
            let gpu_fence = match copy {
                Ok(Ok(fence)) => fence,
                Ok(Err(VirtioError::OutOfMemory | VirtioError::QueueFull)) => {
                    crate::diag::record_named_bytes(b"PBCpy", 0xE4);
                    PRESENT_LAST_STATUS.store(STATUS_NO_MEMORY as u32, Ordering::Relaxed);
                    return STATUS_NO_MEMORY;
                }
                Ok(Err(_)) | Err(_) => {
                    crate::diag::record_named_bytes(b"PBCpy", 0xE5);
                    PRESENT_LAST_STATUS.store(STATUS_DEVICE_NOT_READY as u32, Ordering::Relaxed);
                    return STATUS_DEVICE_NOT_READY;
                }
            };
            crate::diag::record_named_bytes(
                b"PBConv",
                u32::from(source_dxgi_format != destination_dxgi_format),
            );
            // Windows can page a lockable standard staging destination from
            // the BAR/Venus allocation into system memory and keep DWM's CPU
            // view there. BuildPagingBuffer records that exact MDL-page
            // association by resource id. Once this Venus copy completes,
            // mirror into those pages before Present retires; otherwise later
            // frames update only the stale BAR blob.
            let has_system_backing = adapter.system_backings.contains(destination.resource_id);
            if has_system_backing {
                match crate::virtio::ctrl::wait_fence(adapter, gpu_fence, 5_000_000_000) {
                    crate::virtio::ctrl::WaitFenceOutcome::Complete => {
                        crate::diag::record_named_bytes(b"PBSyWt", 1);
                    }
                    crate::virtio::ctrl::WaitFenceOutcome::TimedOut => {
                        crate::diag::record_named_bytes(b"PBSyWt", 0xE1);
                        PRESENT_LAST_STATUS
                            .store(STATUS_DEVICE_NOT_READY as u32, Ordering::Relaxed);
                        return STATUS_DEVICE_NOT_READY;
                    }
                    crate::virtio::ctrl::WaitFenceOutcome::Invalid => {
                        crate::diag::record_named_bytes(b"PBSyWt", 0xE2);
                        PRESENT_LAST_STATUS
                            .store(STATUS_DEVICE_NOT_READY as u32, Ordering::Relaxed);
                        return STATUS_DEVICE_NOT_READY;
                    }
                }
            }
            if has_system_backing {
                match unsafe {
                    crate::ddi::build_paging_buffer::mirror_present_system_backing(
                        adapter,
                        destination.resource_id,
                    )
                } {
                    Some(true) => crate::diag::record_named_bytes(b"PBSyCp", 1),
                    // Windows may page the allocation back to the BAR between
                    // the pre-check and completed fence. With no system
                    // backing, the Venus destination is authoritative again.
                    None => crate::diag::record_named_bytes(b"PBSyCp", 2),
                    Some(false) => {
                        crate::diag::record_named_bytes(b"PBSyCp", 0xE1);
                        PRESENT_LAST_STATUS
                            .store(STATUS_DEVICE_NOT_READY as u32, Ordering::Relaxed);
                        return STATUS_DEVICE_NOT_READY;
                    }
                }
            } else {
                crate::diag::record_named_bytes(b"PBSyCp", 0);
            }
            // Capacity was checked before host work was queued, so this cannot
            // fail. Merge preserves the newest fence if dxgkrnl batches more
            // than one Present into the same DMA private-data buffer.
            if let Err(status) = unsafe {
                PresentSubmissionPrivate::merge_fence(
                    args.pDmaBufferPrivateData,
                    args.DmaBufferPrivateDataSize,
                    gpu_fence,
                )
            } {
                crate::diag::record_named_bytes(b"PBCpy", 0xE6);
                PRESENT_LAST_STATUS.store(status as u32, Ordering::Relaxed);
                return status;
            }
            crate::diag::record_named_bytes(b"PBCpy", 1);
            crate::diag::record_named_bytes(b"PBFnc", gpu_fence as u32);
        }
    }

    if present_flags & (1 << 2) != 0 {
        if adapter.is_none() {
            PRESENT_LAST_STATUS.store(STATUS_INVALID_PARAMETER as u32, Ordering::Relaxed);
            return STATUS_INVALID_PARAMETER;
        }
        // DXGK_PRESENTFLAGS.Flip is an allocation-identity handoff, not a
        // no-op. The source slot contains the exact
        // hDeviceSpecificAllocation that dxgkrnl opened on this device. Select
        // scanout only from that Windows-owned handle and the immutable
        // private-data snapshot captured by OpenAllocation. In particular, do
        // not let the UMD command payload independently select a resource.
        let Some(source) = src_info else {
            crate::diag::record_named_bytes(b"PBFlip", 0xE1);
            PRESENT_LAST_STATUS.store(STATUS_INVALID_PARAMETER as u32, Ordering::Relaxed);
            return STATUS_INVALID_PARAMETER;
        };
        let Some(dxgi_format) = source.resolved_dxgi_format() else {
            crate::diag::record_named_bytes(b"PBFlip", 0xE2);
            PRESENT_LAST_STATUS.store(STATUS_INVALID_PARAMETER as u32, Ordering::Relaxed);
            return STATUS_INVALID_PARAMETER;
        };
        crate::diag::record_named_bytes(b"PBsrc", source.resource_id);
        crate::diag::record_named_bytes(b"PBsw", source.width);
        crate::diag::record_named_bytes(b"PBsh", source.height);
        crate::diag::record_named_bytes(b"PBsDir", u32::from(source.direct_scanout));

        // The driver-private Present payload is retained only as a diagnostic
        // cross-check. It must never override the identity that Windows placed
        // in the Present allocation list.
        let private_match = present_private
            .map(|private| {
                private.resource_id == source.resource_id
                    && private.width == source.width
                    && private.height == source.height
                    && private.dxgi_format == dxgi_format
                    && private.plane_offset == source.plane_offset
            })
            .map(u32::from)
            .unwrap_or(2);
        crate::diag::record_named_bytes(b"PBIdOk", private_match);

        // It must not program scanout here: dxgkrnl subsequently names the
        // allocation that actually reached the VidPn source through
        // SetVidPnSourceAddress. DWM can legitimately compose this Present
        // source into a different managed primary, so publishing both creates
        // two competing selectors and lets retirement of the transient source
        // tear down the real desktop scanout.
        crate::diag::record_named_bytes(b"PBFlip", 1);

        // FlipOnVSyncMmIo explicitly requires DxgkDdiPresent to generate no DMA
        // buffer. In that contract dxgkrnl passes pDmaBuffer == NULL and later
        // supplies the authoritative allocation/address to
        // SetVidPnSourceAddress. There is consequently no DMA address to patch:
        // the allocation-list source above is validation/identity input only.
        // Rejecting this zero-sized call as a depleted buffer makes the UMD's
        // otherwise valid pfnPresentCb fail before the VidPn handoff can occur.
        if args.pDmaBuffer.is_null() {
            crate::diag::record_named_bytes(b"PBMmio", 1);
            args.MultipassOffset = 0;
            PRESENT_LAST_STATUS.store(STATUS_SUCCESS as u32, Ordering::Relaxed);
            return STATUS_SUCCESS;
        }
    }

    if let Err(status) = unsafe { present_allocations.write_patch_references(args) } {
        PRESENT_LAST_STATUS.store(status as u32, Ordering::Relaxed);
        return status;
    }

    if !args.pDmaBuffer.is_null() {
        let bytes = core::mem::size_of::<helios_protocol::HeliosPresentRefreshCmd>() as UINT;
        if args.DmaSize < bytes {
            PRESENT_LAST_STATUS.store(
                STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER as u32,
                Ordering::Relaxed,
            );
            return STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER;
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
    let p = _adapter as *const AdapterContext;
    if p.is_null() || !unsafe { (*p).display_half } {
        return STATUS_NOT_SUPPORTED;
    }
    let adapter = unsafe { &*p };
    // Windows names the authoritative desktop primary here. This—not resource
    // dimensions, OM bindings, process name, or an arbitrary Present call—is the
    // only allocation the KMD may bind directly or copy into the fallback image.
    if address.is_null() {
        return STATUS_SUCCESS;
    }
    let h_alloc = unsafe { (*address).hAllocation };
    let primary_segment = unsafe { (*address).PrimarySegment };
    let primary_address = unsafe { (*address).PrimaryAddress.QuadPart as u64 };
    let primary_flags = unsafe { (*address).Flags.__bindgen_anon_1.Value };
    // Pair the exact address with the exact allocation before deferring. The
    // VSync DPC must continue reporting the previously displayed address until
    // the PASSIVE worker has actually programmed this primary.
    if !unsafe {
        crate::ddi::create_allocation::set_vidpn_primary_address(
            h_alloc,
            primary_segment,
            primary_address,
            primary_flags,
        )
    } {
        return STATUS_INVALID_PARAMETER;
    }
    // This exact Windows handoff is now pending display-engine programming.
    // Keep the periodic VSync path from reporting the preceding physical
    // address while the DIRQL callback is deferred to PASSIVE_LEVEL.
    adapter.vidpn_programming.store(1, Ordering::Release);
    // Dxgkrnl's MMIO-flip path invokes this DDI under
    // DxgkCbSynchronizeExecution at DIRQL. At that IRQL it is illegal to write
    // registry diagnostics, wait on the Venus mutex, or submit synchronous
    // virtio control commands. Preserve the exact Windows-supplied allocation
    // identity and let the timer DPC wake the PASSIVE display worker.
    let irql = unsafe { KeGetCurrentIrql() };
    if irql != 0 {
        adapter
            .pending_vidpn_allocation
            .store(h_alloc as usize, Ordering::Release);
        return STATUS_SUCCESS;
    }

    unsafe { apply_vidpn_source_address(adapter, h_alloc) }
}

/// PASSIVE continuation of SetVidPnSourceAddress.
///
/// The allocation handle is the exact identity supplied by Windows. No
/// process, geometry, creation-order, or timing classification is involved.
pub(crate) fn process_deferred_vidpn_source_address(adapter: &AdapterContext) {
    let status = adapter.with_scanout_lifecycle(|| {
        let raw = adapter.pending_vidpn_allocation.swap(0, Ordering::AcqRel);
        if raw == 0 {
            return None;
        }
        Some(unsafe { apply_vidpn_source_address_locked(adapter, raw as HANDLE) })
    });
    if let Some(status) = status {
        crate::diag::record_named_bytes(b"VpDSt", status as u32);
    }
}

/// Program the Windows-selected primary. PASSIVE_LEVEL only.
unsafe fn apply_vidpn_source_address(adapter: &AdapterContext, h_alloc: HANDLE) -> NTSTATUS {
    adapter
        .with_scanout_lifecycle(|| unsafe { apply_vidpn_source_address_locked(adapter, h_alloc) })
}

/// Apply one exact Windows source allocation while serialized against
/// DestroyAllocation retirement of the same KMD allocation/resource identity.
unsafe fn apply_vidpn_source_address_locked(adapter: &AdapterContext, h_alloc: HANDLE) -> NTSTATUS {
    crate::diag::record(0x1300_000A);
    let source_address_n = VIDPN_SOURCE_ADDRESS_COUNT
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    let trace_tick = source_address_n == 1 || source_address_n % 600 == 0;
    if trace_tick {
        crate::diag::record_named_bytes(b"VpSA", source_address_n);
    }

    let Some(source) = (unsafe { crate::ddi::create_allocation::scanout_alloc_info(h_alloc) })
    else {
        crate::diag::record_named_bytes(b"ScRid", 0);
        adapter.vidpn_programming.store(0, Ordering::Release);
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
        crate::diag::record_named_bytes(b"SaSeg", source.primary_segment);
        crate::diag::record_named_bytes(b"SaLo", source.primary_address as u32);
        crate::diag::record_named_bytes(b"SaHi", (source.primary_address >> 32) as u32);
        crate::diag::record_named_bytes(b"SaFlg", source.primary_flags);
    }
    if width != mode_w || height != mode_h {
        crate::diag::record_named_bytes(b"ScSet", 0xD);
        adapter.vidpn_programming.store(0, Ordering::Release);
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
            && matches!(source.dxgi_format, 28 | 87 | 88);
        if !valid {
            crate::diag::record_named_bytes(b"ScSet", 0xE3);
            adapter.vidpn_programming.store(0, Ordering::Release);
            return STATUS_INVALID_PARAMETER;
        }
        source
    } else {
        match production_linear_scanout(adapter, width, height) {
            Ok(target) => target,
            Err(status) => {
                crate::diag::record_named_bytes(b"ScSet", 0xE1);
                adapter.vidpn_programming.store(0, Ordering::Release);
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
    // Preserve the exact allocation storage format on the standard virtio
    // scanout contract. The target can differ from the Windows source when the
    // KMD-owned compatibility copy is selected.
    let Some(vformat) = virtio_scanout_format(target.dxgi_format) else {
        crate::diag::record_named_bytes(b"ScFmt", target.dxgi_format);
        adapter.vidpn_programming.store(0, Ordering::Release);
        return STATUS_NOT_SUPPORTED;
    };
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
            adapter.vidpn_programming.store(0, Ordering::Release);
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
        // SET_SCANOUT_BLOB has completed successfully, so this exact Windows
        // primary is now what the host pixel pipeline reads. Only now may the
        // next CRTC_VSYNC retire the preceding flip.
        adapter
            .last_primary_address
            .store(source.primary_address, Ordering::Release);
        // Release after publishing the matching physical address. The next
        // VSync DPC acquires this flag before sampling last_primary_address.
        adapter.vidpn_programming.store(0, Ordering::Release);
        return STATUS_SUCCESS;
    }

    let target_image_id = adapter.dedicated_scanout_image.load(Ordering::Acquire);
    if target_image_id == 0 {
        crate::diag::record_named_bytes(b"ScSet", 0xE2);
        adapter.vidpn_programming.store(0, Ordering::Release);
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
            source.primary_address,
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
            adapter.vidpn_programming.store(0, Ordering::Release);
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
