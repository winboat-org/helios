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

use crate::adapter::{AdapterContext, ScanoutGuard};
use crate::ddi::create_allocation::{present_alloc_info, PresentAllocationStorage, ScanoutTarget};
use crate::ddi::present_packet::{
    PresentAllocations, PresentSubmissionPrivate, STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER,
};
use crate::device::ContextHandleRef;
use crate::dxgk::*;
use crate::irql::PassiveLevel;
use crate::virtio::venus::{OptimalPresentImageDesc, PresentBufferDesc, PresentDestinationDesc};
use crate::virtio::VirtioError;
use helios_kmd_logic::ScanoutFormat;
use wdk_sys::ntddk::KeGetCurrentIrql;

pub static PRESENT_COUNT: AtomicU32 = AtomicU32::new(0);
/// Drives the throttle for this DDI's IDENTITY dumps (`diag::sample_tick`).
/// Failure values — PBRet, PBCpy, PBSyWt, PBSyCp and PBFlip's error arms — are
/// never sampled; they stay unconditional.
static PRESENT_TRACE_TICK: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_SRC_COUNT: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_DST_COUNT: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_DMA_SIZE: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_PATCH_SIZE: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_SRC_OPEN_LOW: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_DST_OPEN_LOW: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_FLAGS: AtomicU32 = AtomicU32::new(0);
pub static PRESENT_LAST_STATUS: AtomicU32 = AtomicU32::new(0);
pub(crate) static VIDPN_SOURCE_ADDRESS_COUNT: AtomicU32 = AtomicU32::new(0);

/// Pin `kmd_logic`'s hand-written virtio values to the wire constants.
///
/// `kmd_logic` is deliberately dependency-free (it has to build under a host
/// libtest harness), so it spells the three `VIRTIO_GPU_FORMAT_*` values as
/// literals. This is where they are checked against the single source of truth;
/// a drift is a build failure, not a black scan-out.
const _: () = {
    assert!(ScanoutFormat::Bgra8.virtio() == helios_protocol::VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM);
    assert!(ScanoutFormat::Bgrx8.virtio() == helios_protocol::VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM);
    assert!(ScanoutFormat::Rgba8.virtio() == helios_protocol::VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM);
};

fn production_linear_scanout(
    adapter: &AdapterContext,
    lock: &ScanoutGuard<'_>,
    width: u32,
    height: u32,
) -> Result<ScanoutTarget, ScanoutReject> {
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
        return ScanoutTarget::adapter_linear(
            resource_id,
            width,
            height,
            (layout >> 32) as u32,
            layout as u32 as u64,
            adapter.primary_scanout_alloc_size.load(Ordering::Relaxed),
            adapter.primary_scanout_memory_type.load(Ordering::Relaxed),
            adapter.primary_scanout_dxgi_format.load(Ordering::Relaxed),
        );
    }

    // Through the scanout token: this is one of the two Venus acquisitions that
    // run under `scanout_mutex`, and going through the guard is what makes the
    // scanout-before-venus lock order structural here.
    let scanout = match lock.with_venus_client(|client| {
        client.allocate_linear_scanout_image_blob(adapter, width, height)
    }) {
        Ok(Ok(scanout)) => scanout,
        Ok(Err(_)) => {
            crate::diag::record_named_bytes(b"CpErr", 1);
            return Err(ScanoutReject::LinearAllocFailed(STATUS_NO_MEMORY));
        }
        Err(_) => {
            crate::diag::record_named_bytes(b"CpErr", 2);
            return Err(ScanoutReject::LinearAllocFailed(STATUS_DEVICE_NOT_READY));
        }
    };

    adapter.remember_primary_scanout(
        scanout.blob.res_id,
        width,
        height,
        scanout.row_pitch,
        scanout.plane_offset,
        scanout.blob.size,
        scanout.memory_type_index,
        ScanoutFormat::Bgra8.dxgi(),
    );
    adapter
        .dedicated_scanout_memory
        .store(scanout.blob.blob_id, Ordering::Relaxed);
    adapter
        .dedicated_scanout_image
        .store(scanout.image_id.get(), Ordering::Relaxed);
    adapter
        .dedicated_scanout_resource
        .store(scanout.blob.res_id, Ordering::Release);
    crate::diag::record_named_bytes(b"CpRid", scanout.blob.res_id);
    crate::diag::record_named_bytes(b"CpBid", scanout.blob.blob_id as u32);
    crate::diag::record_named_bytes(b"CpPch", scanout.row_pitch);
    ScanoutTarget::adapter_linear(
        scanout.blob.res_id,
        width,
        height,
        scanout.row_pitch,
        scanout.plane_offset as u64,
        scanout.blob.size,
        scanout.memory_type_index,
        ScanoutFormat::Bgra8.dxgi(),
    )
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

    // Present-path IDENTITY trace, SAMPLED (R316). These are per-call dumps of
    // flags / counts / sizes that mattered during bring-up; at 60 Hz they were
    // ~8 synchronous kernel registry writes per frame before the surface block
    // below added ~40 more. One tick gates the whole DDI's identity output, so a
    // sampled frame is internally consistent, and `DiagLevel >= 1` restores the
    // per-call cadence.
    let sample = crate::diag::sample_tick(&PRESENT_TRACE_TICK);
    if sample {
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
    }

    let allocation_list = unsafe { args.__bindgen_anon_1.pAllocationList };
    let present_allocations = unsafe { PresentAllocations::from_allocation_list(allocation_list) };
    let present_private = unsafe { present_private_data(args) };
    if sample {
        crate::diag::record_named_bytes(b"PBpdsz", args.PrivateDriverDataSize);
        crate::diag::record_named_bytes(b"PBkpsz", args.DmaBufferPrivateDataSize);
    }
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

        // Present-blit feasibility trace (read-only), SAMPLED. Resolves the
        // composition source + destination surfaces to their venus resource ids
        // and geometry, and reports whether each is a tracked
        // host-visible-mappable blob. 38 registry writes plus two blob_lookup
        // round-trips under the device lock — per present, for values that change
        // only when the surface set changes. Note this block runs for the FLIP
        // shape too (the flip arm reads src_info, resolved from the allocation
        // list), so it was genuinely per-frame.
        if sample {
        // Trace-only identity, resolved ONLY here — inside the sampling gate.
        let src_diag = unsafe { crate::ddi::create_allocation::present_alloc_diag(src_handle) };
        let dst_diag = unsafe { crate::ddi::create_allocation::present_alloc_diag(dst_handle) };
        crate::diag::record_named_bytes(b"PBsrcH", (src_handle as usize as u32) & 0xFFFF);
        crate::diag::record_named_bytes(b"PBdstH", (dst_handle as usize as u32) & 0xFFFF);
        if let Some(s) = src_info {
            if let Some(dg) = src_diag {
                crate::diag::record_named_bytes(b"PBsRtA", dg.runtime_allocation);
            }
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
            if let Some(dg) = src_diag {
                crate::diag::record_named_bytes(b"PBsStd", dg.standard_allocation_type);
                crate::diag::record_named_bytes(b"PBsGdi", dg.standard_gdi_surface_type);
                crate::diag::record_named_bytes(b"PBsOF", dg.open_flags);
                crate::diag::record_named_bytes(b"PBsRA", u32::from(dg.resource_associated));
                crate::diag::record_named_bytes(b"PBsAPS", dg.allocation_private_size);
                crate::diag::record_named_bytes(b"PBsRPS", dg.resource_private_size);
            }
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
            if let Some(dg) = dst_diag {
                crate::diag::record_named_bytes(b"PBdRtA", dg.runtime_allocation);
            }
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
            if let Some(dg) = dst_diag {
                crate::diag::record_named_bytes(b"PBdStd", dg.standard_allocation_type);
                crate::diag::record_named_bytes(b"PBdGdi", dg.standard_gdi_surface_type);
                crate::diag::record_named_bytes(b"PBdOF", dg.open_flags);
                crate::diag::record_named_bytes(b"PBdRA", u32::from(dg.resource_associated));
                crate::diag::record_named_bytes(b"PBdAPS", dg.allocation_private_size);
                crate::diag::record_named_bytes(b"PBdRPS", dg.resource_private_size);
            }
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

            // SAFETY: `DxgkDdiPresent` is documented "IRQL: PASSIVE_LEVEL" (WDK
            // DXGKDDI_PRESENT) — it is a pageable DDI, and the BLT arm below
            // waits on a wire fence and maps blob bytes, neither of which is
            // legal above PASSIVE. Note this is the BLT arm only; the MMIO-flip
            // arm generates no DMA buffer and is completed through
            // SetVidPnSourceAddress, whose DIRQL half holds no token at all.
            let passive = unsafe { crate::irql::PassiveLevel::assume() };
            let copy = adapter.with_venus_client(passive, |client| {
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
                match crate::virtio::ctrl::wait_fence(passive, adapter, gpu_fence, 5_000_000_000) {
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
                        passive,
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
            // NOT sampled: PBCpy is the value a failed Present is read from
            // (its 0xE1..0xE6 arms), so its success arm has to keep the same
            // cadence or "last PBCpy" stops meaning "what the last BLT did".
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
        // Flip identity, SAMPLED (the 0xE1/0xE2 failure arms above stay
        // unconditional — those are the values a failed Present is read from).
        if sample {
            crate::diag::record_named_bytes(b"PBsrc", source.resource_id);
            crate::diag::record_named_bytes(b"PBsw", source.width);
            crate::diag::record_named_bytes(b"PBsh", source.height);
            crate::diag::record_named_bytes(b"PBsDir", u32::from(source.direct_scanout));
        }

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
        if sample {
            crate::diag::record_named_bytes(b"PBIdOk", private_match);
        }

        // It must not program scanout here: dxgkrnl subsequently names the
        // allocation that actually reached the VidPn source through
        // SetVidPnSourceAddress. DWM can legitimately compose this Present
        // source into a different managed primary, so publishing both creates
        // two competing selectors and lets retirement of the transient source
        // tear down the real desktop scanout.
        if sample {
            crate::diag::record_named_bytes(b"PBFlip", 1);
        }

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
    !p.is_null() && unsafe { (*p).display_half() }
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
    if p.is_null() || !unsafe { (*p).display_half() } {
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
    if p.is_null() || !unsafe { (*p).display_half() } {
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
    if p.is_null() || !unsafe { (*p).display_half() } {
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
    if p.is_null() || !unsafe { (*p).display_half() } {
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

    // Dxgkrnl's MMIO-flip path invokes this DDI under DxgkCbSynchronizeExecution
    // at DIRQL. At that IRQL it is illegal to write registry diagnostics, wait on
    // the Venus mutex, or submit synchronous virtio control commands, so the
    // atomics-only half is split out below and the PASSIVE continuation is
    // separate: the two halves cannot be interleaved by accident.
    if !unsafe {
        set_vidpn_source_address_dirql(
            adapter,
            h_alloc,
            primary_segment,
            primary_address,
            primary_flags,
        )
    } {
        return STATUS_INVALID_PARAMETER;
    }

    if unsafe { KeGetCurrentIrql() } != crate::ddi::PASSIVE_LEVEL_IRQL {
        // Deferred: the timer DPC wakes the PASSIVE display worker, which
        // consumes `pending_vidpn_allocation` and adopts the raised gate.
        //
        // ⚠ R614: this arm holds NO `PassiveLevel`, and that is now structural
        // rather than a convention. `program_vidpn_source` -> `ctrl::set_scanout_blob`
        // is unreachable from here because the token does not exist on this side
        // of the branch — which is the exact invalid sequence the item was
        // written for ("compiles, links, ships, and either deadlocks at DISPATCH
        // on a KEVENT wait or calls MmAllocateContiguousMemory above APC_LEVEL").
        adapter
            .pending_vidpn_allocation
            .store(h_alloc as usize, Ordering::Release);
        return STATUS_SUCCESS;
    }

    // SAFETY: `DxgkDdiSetVidPnSourceAddress` is documented callable up to DIRQL
    // (dxgkrnl's MMIO-flip path invokes it under DxgkCbSynchronizeExecution), so
    // the annotation proves nothing here — the runtime check immediately above
    // does. This mint is downstream of it and must stay there.
    let passive = unsafe { crate::irql::PassiveLevel::assume() };
    unsafe { apply_vidpn_source_address(passive, adapter, h_alloc) }
}

/// The atomics-only half of `SetVidPnSourceAddress`, legal at DIRQL.
///
/// Pairs the exact address with the exact allocation and RAISES the programming
/// gate; nothing here writes the registry, waits, or touches the transport.
/// Returns false if the handle could not be paired, in which case the gate is
/// NOT raised.
///
/// The matching lower is `ProgrammingInterval`'s drop inside
/// [`apply_vidpn_source_address_locked`], or the ring-1 completion DPC after a
/// `transfer_to_completion()`. The raise cannot itself hold a
/// `ProgrammingInterval`: the two halves run in different call stacks at
/// different IRQLs, so a token spanning them would be a flag again.
///
/// # Safety
/// `h_alloc` is the live KMD allocation handle dxgkrnl passed to the DDI.
unsafe fn set_vidpn_source_address_dirql(
    adapter: &AdapterContext,
    h_alloc: HANDLE,
    primary_segment: u32,
    primary_address: u64,
    primary_flags: u32,
) -> bool {
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
        return false;
    }
    // This exact Windows handoff is now pending display-engine programming.
    // Keep the periodic VSync path from reporting the preceding physical
    // address while the DIRQL callback is deferred to PASSIVE_LEVEL.
    //
    // Raising bumps the generation and sets the active flag in ONE publication,
    // so a completion can tell which interval it belongs to.
    let _ticket = adapter.raise_programming_gate();
    true
}

/// PASSIVE continuation of SetVidPnSourceAddress.
///
/// The allocation handle is the exact identity supplied by Windows. No
/// process, geometry, creation-order, or timing classification is involved.
pub(crate) fn process_deferred_vidpn_source_address(
    passive: PassiveLevel,
    adapter: &AdapterContext,
) {
    let status = adapter.with_scanout_lifecycle(passive, |lock| {
        let raw = adapter.pending_vidpn_allocation.swap(0, Ordering::AcqRel);
        if raw == 0 {
            return None;
        }
        Some(unsafe { apply_deferred_vidpn_source_address_locked(adapter, lock, raw as HANDLE) })
    });
    if let Some(status) = status {
        crate::diag::record_named_bytes(b"VpDSt", status as u32);
    }
}

/// The DEFERRED wrapper: same programming body, plus the forward-progress
/// contract the DIRQL path needs.
///
/// On the deferred path the OS was told SUCCESS before any programming happened.
/// Every failure exit then cleared the gate, recorded one overwritten value and
/// returned — without updating `last_primary_address` and without re-arming the
/// pending handle (the worker's swap already zeroed it). Since
/// `last_primary_address` is exactly what the CRTC_VSYNC packet carries, the
/// heartbeat kept reporting the PREVIOUSLY displayed address forever: the flip
/// queued for the failed primary could never retire, dxgkrnl stopped issuing new
/// source addresses, and the desktop froze.
///
/// ⚠ The freeze is derived from the driver's own documented model
/// (`adapter.rs`'s `last_primary_address` contract and `start_device.rs`'s VSync
/// DPC), NOT from a hardware observation.
///
/// A retryable refusal now re-arms the exact handle and keeps the gate raised;
/// the VSync DPC's `pending_vidpn_allocation != 0` branch signals the worker,
/// which is the existing wake path. The re-arm is safe against DestroyAllocation
/// because it happens inside `with_scanout_lifecycle` — the same lock
/// `retire_scanout_allocation_locked`'s cancel CAS and the worker's swap hold, so
/// a destroy racing this either cancels before we re-arm or observes the re-armed
/// handle and cancels it.
///
/// # Safety
/// `h_alloc` is the exact allocation handle Windows published at DIRQL.
unsafe fn apply_deferred_vidpn_source_address_locked(
    adapter: &AdapterContext,
    lock: &ScanoutGuard<'_>,
    h_alloc: HANDLE,
) -> NTSTATUS {
    let interval = crate::adapter::ProgrammingInterval::adopt(&adapter.vidpn_programming);
    match unsafe { program_vidpn_source(adapter, lock, h_alloc, interval.ticket()) } {
        Ok(ScanoutOutcome::Programmed) => {
            clear_retry_state();
            STATUS_SUCCESS
        }
        Ok(ScanoutOutcome::CopyQueued) => {
            clear_retry_state();
            interval.transfer_to_completion();
            STATUS_SUCCESS
        }
        Err(reject) => {
            reject.report();
            if reject.retryable() {
                match note_retry_attempt(h_alloc) {
                    RetryDecision::Again => {
                        // Re-arm the EXACT handle and keep the gate raised. Note
                        // the store is inside the scanout lock, so it cannot race
                        // the DestroyAllocation cancel.
                        adapter
                            .pending_vidpn_allocation
                            .store(h_alloc as usize, Ordering::Release);
                        interval.retain_for_retry();
                    }
                    // Budget exhausted. Drop the interval: the gate clears and
                    // the heartbeat resumes with the truthful OLD address, which
                    // is a visibly stale desktop rather than a frozen one.
                    RetryDecision::GaveUp => {}
                }
            } else {
                // Permanent for this allocation. Retrying would hold the gate
                // and suppress CRTC_VSYNC for nothing.
                clear_retry_state();
            }
            reject.status()
        }
    }
}

/// Program the Windows-selected primary. PASSIVE_LEVEL only.
unsafe fn apply_vidpn_source_address(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    h_alloc: HANDLE,
) -> NTSTATUS {
    adapter.with_scanout_lifecycle(passive, |lock| unsafe {
        apply_vidpn_source_address_locked(adapter, lock, h_alloc)
    })
}

/// Why one deferred programming attempt refused to program the primary.
///
/// Every unhappy exit of [`program_vidpn_source`] produces one of these, and
/// each variant owns BOTH its registry breadcrumb and its counter — so adding a
/// refusal without a counter stops compiling. Before this, seven of the eight
/// were visible only as `VpDSt`, a single overwritten DWORD, and the eighth (an
/// `hAllocation` that does not resolve) returned STATUS_SUCCESS with no counter
/// at all: the OS believed the primary was programmed and the only trace,
/// `ScRid=0`, was overwritten by the next successful bind.
///
/// The two variants that carry an NTSTATUS do so because the current code
/// propagates the callee's status rather than choosing one; collapsing them to
/// a fixed status would change what the DDI returns.
#[derive(Clone, Copy)]
pub(crate) enum ScanoutReject {
    /// `hAllocation` did not resolve to a backed KMD allocation.
    BadAlloc,
    /// The primary's extent does not match the mode we advertise.
    Extent,
    /// A direct-scan-out primary whose pitch/offset/size/format is not
    /// exportable — the guard that keeps QEMU from reading past the blob.
    Layout,
    /// No virtio wire encoding for this DXGI format.
    Format(u32),
    /// The adapter-owned LINEAR fallback could not be allocated.
    LinearAllocFailed(NTSTATUS),
    /// The host refused SET_SCANOUT_BLOB.
    SetFailed,
    /// The LINEAR target has no Venus image to copy into.
    NoTarget,
    /// The primary-to-LINEAR GPU copy could not be submitted.
    CopyFailed(NTSTATUS),
}

/// Refusal counters, one per [`ScanoutReject`] variant. All must read 0 across a
/// healthy session — a nonzero value is a primary Windows named that Helios did
/// not program. Flushed from the existing periodic PASSIVE tick, never from the
/// refusal path itself: a registry write per refusal would be exactly the
/// per-frame tax T1b removed elsewhere.
static SC_BAD_ALLOC: AtomicU32 = AtomicU32::new(0);
static SC_BAD_EXTENT: AtomicU32 = AtomicU32::new(0);
static SC_BAD_LAYOUT: AtomicU32 = AtomicU32::new(0);
static SC_BAD_FORMAT: AtomicU32 = AtomicU32::new(0);
static SC_LINEAR_ERR: AtomicU32 = AtomicU32::new(0);
static SC_SET_ERR: AtomicU32 = AtomicU32::new(0);
static SC_NO_TARGET: AtomicU32 = AtomicU32::new(0);
static SC_COPY_ERR: AtomicU32 = AtomicU32::new(0);
/// The HPD worker's `ScanoutRefreshQueue::Unavailable` arm, which used to drop
/// the dirty bit with only a comment.
pub(crate) static SC_UNAVAILABLE: AtomicU32 = AtomicU32::new(0);

impl ScanoutReject {
    /// Reproduce the exact (name, value) breadcrumb this refusal emitted before
    /// R505. These pairs are owner debugging ABI — do not renumber them.
    fn record(self) {
        match self {
            Self::BadAlloc => crate::diag::record_named_bytes(b"ScRid", 0),
            Self::Extent => crate::diag::record_named_bytes(b"ScSet", 0xD),
            Self::Layout => crate::diag::record_named_bytes(b"ScSet", 0xE3),
            Self::Format(f) => crate::diag::record_named_bytes(b"ScFmt", f),
            Self::LinearAllocFailed(_) => crate::diag::record_named_bytes(b"ScSet", 0xE1),
            Self::SetFailed => crate::diag::record_named_bytes(b"ScSet", 0xE),
            Self::NoTarget => crate::diag::record_named_bytes(b"ScSet", 0xE2),
            Self::CopyFailed(_) => crate::diag::record_named_bytes(b"ScCpy", 0xE),
        }
    }

    fn counter(self) -> &'static AtomicU32 {
        match self {
            Self::BadAlloc => &SC_BAD_ALLOC,
            Self::Extent => &SC_BAD_EXTENT,
            Self::Layout => &SC_BAD_LAYOUT,
            Self::Format(_) => &SC_BAD_FORMAT,
            Self::LinearAllocFailed(_) => &SC_LINEAR_ERR,
            Self::SetFailed => &SC_SET_ERR,
            Self::NoTarget => &SC_NO_TARGET,
            Self::CopyFailed(_) => &SC_COPY_ERR,
        }
    }

    /// The NTSTATUS this refusal returned before R505.
    ///
    /// `BadAlloc` keeps STATUS_SUCCESS deliberately: returning
    /// STATUS_INVALID_PARAMETER there is a separate, hardware-proven decision,
    /// not part of this commit.
    fn status(self) -> NTSTATUS {
        match self {
            Self::BadAlloc => STATUS_SUCCESS,
            Self::Extent | Self::Layout => STATUS_INVALID_PARAMETER,
            Self::Format(_) => STATUS_NOT_SUPPORTED,
            Self::LinearAllocFailed(status) | Self::CopyFailed(status) => status,
            Self::SetFailed | Self::NoTarget => STATUS_DEVICE_NOT_READY,
        }
    }

    /// Emit the breadcrumb and bump the counter. One call so the two can never
    /// drift apart at a call site.
    fn report(self) {
        self.record();
        self.counter().fetch_add(1, Ordering::Relaxed);
    }

    /// Whether programming this exact allocation again could plausibly succeed.
    ///
    /// Classified per variant deliberately — there is no blanket answer.
    /// Transport and allocation failures are transient (the host was busy, the
    /// LINEAR target could not be minted yet, the ring was full). Validation
    /// rejects are permanent FOR THAT ALLOCATION: its extent, layout and format
    /// will not change, so retrying only burns the budget and holds the gate.
    fn retryable(self) -> bool {
        match self {
            Self::LinearAllocFailed(_) | Self::SetFailed | Self::NoTarget | Self::CopyFailed(_) => {
                true
            }
            Self::BadAlloc | Self::Extent | Self::Layout | Self::Format(_) => false,
        }
    }
}

/// Retry attempts allowed for one primary before the gate is dropped and the
/// heartbeat resumes with the truthful old address.
///
/// A few, not hundreds: the gate suppresses CRTC_VSYNC for its whole duration,
/// and the VSync DPC re-signals the worker every ~16 ms, so this is a bounded
/// ~64 ms stall in the worst case rather than an unbounded one.
const SCANOUT_RETRY_BUDGET: u32 = 4;

/// Retry bookkeeping for the deferred path. Touched only under `scanout_mutex`
/// (the deferred continuation holds it), so plain atomics need no extra
/// ordering discipline beyond being atomics.
static RETRY_HANDLE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static RETRY_ATTEMPTS: AtomicU32 = AtomicU32::new(0);
/// Retryable refusals that were re-armed (diag `ScRetry`).
static SC_RETRY: AtomicU32 = AtomicU32::new(0);
/// Primaries abandoned after exhausting [`SCANOUT_RETRY_BUDGET`] (diag `ScGaveUp`).
static SC_GAVE_UP: AtomicU32 = AtomicU32::new(0);

enum RetryDecision {
    /// Re-arm the handle and keep the gate raised.
    Again,
    /// Budget exhausted: drop the gate, count loudly, stop retrying.
    GaveUp,
}

/// Charge one attempt against `h_alloc`'s budget. A different handle starts a
/// fresh budget — a new primary is not the old one's retry.
fn note_retry_attempt(h_alloc: HANDLE) -> RetryDecision {
    use core::sync::atomic::Ordering::Relaxed;
    let handle = h_alloc as usize;
    let attempts = if RETRY_HANDLE.swap(handle, Relaxed) == handle {
        RETRY_ATTEMPTS.fetch_add(1, Relaxed).wrapping_add(1)
    } else {
        RETRY_ATTEMPTS.store(1, Relaxed);
        1
    };
    if attempts > SCANOUT_RETRY_BUDGET {
        RETRY_HANDLE.store(0, Relaxed);
        RETRY_ATTEMPTS.store(0, Relaxed);
        SC_GAVE_UP.fetch_add(1, Relaxed);
        RetryDecision::GaveUp
    } else {
        SC_RETRY.fetch_add(1, Relaxed);
        RetryDecision::Again
    }
}

/// Forget any in-progress retry: this primary either programmed or failed
/// permanently, so the next retryable refusal starts from a full budget.
fn clear_retry_state() {
    use core::sync::atomic::Ordering::Relaxed;
    RETRY_HANDLE.store(0, Relaxed);
    RETRY_ATTEMPTS.store(0, Relaxed);
}

/// What a successful deferred programming did, and therefore who owns the gate.
enum ScanoutOutcome {
    /// The exact Windows primary is bound and its address published. The gate
    /// is lowered on return.
    Programmed,
    /// A primary-to-LINEAR copy is queued on ring 1. Its used-ring completion
    /// DPC publishes the displayed address and clears the gate.
    CopyQueued,
}

/// Mirror the refusal counters into the registry. Called from the existing
/// periodic PASSIVE snapshot, which already runs under a `sample_tick`.
pub(crate) fn record_scanout_reject_counters() {
    use core::sync::atomic::Ordering::Relaxed;
    crate::diag::record_named_bytes(b"ScBadAlc", SC_BAD_ALLOC.load(Relaxed));
    crate::diag::record_named_bytes(b"ScBadExt", SC_BAD_EXTENT.load(Relaxed));
    crate::diag::record_named_bytes(b"ScBadLay", SC_BAD_LAYOUT.load(Relaxed));
    crate::diag::record_named_bytes(b"ScBadFmt", SC_BAD_FORMAT.load(Relaxed));
    crate::diag::record_named_bytes(b"ScLinErr", SC_LINEAR_ERR.load(Relaxed));
    crate::diag::record_named_bytes(b"ScSetErr", SC_SET_ERR.load(Relaxed));
    crate::diag::record_named_bytes(b"ScNoTgt", SC_NO_TARGET.load(Relaxed));
    crate::diag::record_named_bytes(b"ScCpyErr", SC_COPY_ERR.load(Relaxed));
    crate::diag::record_named_bytes(b"ScUnav", SC_UNAVAILABLE.load(Relaxed));
    crate::diag::record_named_bytes(b"ScRetry", SC_RETRY.load(Relaxed));
    crate::diag::record_named_bytes(b"ScGaveUp", SC_GAVE_UP.load(Relaxed));
    // R509: gate-generation health. Both must read 0 on a normal boot.
    crate::diag::record_named_bytes(
        b"ScStale",
        crate::adapter::GATE_STALE_CLEARS.load(Relaxed),
    );
    crate::diag::record_named_bytes(
        b"ScGateCx",
        crate::adapter::GATE_RAISE_CAS_GIVEUPS.load(Relaxed),
    );
}

/// Zero every refusal counter. StartDevice only.
///
/// Registry counter values persist across boots, so a counter that is merely
/// *present* proves nothing; these are reset so "it moved this boot" is the
/// readable fact.
pub(crate) fn reset_scanout_reject_counters() {
    use core::sync::atomic::Ordering::Relaxed;
    for c in [
        &SC_BAD_ALLOC,
        &SC_BAD_EXTENT,
        &SC_BAD_LAYOUT,
        &SC_BAD_FORMAT,
        &SC_LINEAR_ERR,
        &SC_SET_ERR,
        &SC_NO_TARGET,
        &SC_COPY_ERR,
        &SC_UNAVAILABLE,
        &SC_RETRY,
        &SC_GAVE_UP,
        &crate::adapter::GATE_STALE_CLEARS,
        &crate::adapter::GATE_RAISE_CAS_GIVEUPS,
    ] {
        c.store(0, Relaxed);
    }
    clear_retry_state();
    record_scanout_reject_counters();
}

/// Apply one exact Windows source allocation while serialized against
/// DestroyAllocation retirement of the same KMD allocation/resource identity.
///
/// This is the thin outer half: it owns the programming interval, turns the
/// single `Err` into its breadcrumb + counter + NTSTATUS, and is the ONE place
/// the gate is handed to the completion DPC. All the programming logic lives in
/// [`program_vidpn_source`], whose only unhappy exit is a `ScanoutReject`.
unsafe fn apply_vidpn_source_address_locked(
    adapter: &AdapterContext,
    lock: &ScanoutGuard<'_>,
    h_alloc: HANDLE,
) -> NTSTATUS {
    // Adopt the gate the DIRQL half raised. It drops at the end of THIS
    // function, i.e. after the reject breadcrumb below — preserving the
    // record-then-lower order every arm had before R504.
    let interval = crate::adapter::ProgrammingInterval::adopt(&adapter.vidpn_programming);
    match unsafe { program_vidpn_source(adapter, lock, h_alloc, interval.ticket()) } {
        Ok(ScanoutOutcome::Programmed) => STATUS_SUCCESS,
        Ok(ScanoutOutcome::CopyQueued) => {
            // THE one hand-off. The copy is queued on ring 1 and its completion
            // DPC owns the gate from here: it publishes the displayed address on
            // success and clears the gate either way. Dropping the interval
            // instead would clear the gate mid-programming and let a CRTC_VSYNC
            // report the OLD address as authoritative.
            interval.transfer_to_completion();
            STATUS_SUCCESS
        }
        Err(reject) => {
            // No retry on this path: the DDI ran at PASSIVE, so the refusal's
            // NTSTATUS reaches dxgkrnl directly and is the truthful answer.
            // Nothing was deferred, so there is nothing to re-arm.
            reject.report();
            reject.status()
        }
    }
}

/// The programming body. One happy shape, one unhappy shape.
unsafe fn program_vidpn_source(
    adapter: &AdapterContext,
    lock: &ScanoutGuard<'_>,
    h_alloc: HANDLE,
    ticket: crate::adapter::ProgrammingTicket,
) -> Result<ScanoutOutcome, ScanoutReject> {
    crate::diag::record(0x1300_000A);
    // GATE INSTRUMENT (T3), 0 in production. Read ONCE per call; each `forced ==
    // N` below sits alongside the REAL condition at its own exit site, so the
    // code preceding that exit still runs and it is the genuine exit that is
    // exercised — not a short circuit at the top of the function.
    let forced = adapter.forced_reject();
    let source_address_n = VIDPN_SOURCE_ADDRESS_COUNT
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    let trace_tick = source_address_n == 1 || source_address_n % 600 == 0;
    if trace_tick {
        crate::diag::record_named_bytes(b"VpSA", source_address_n);
    }

    let source = match unsafe { crate::ddi::create_allocation::scanout_alloc_info(h_alloc) } {
        Some(source) if forced != 1 => source,
        _ => return Err(ScanoutReject::BadAlloc),
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
    if width != mode_w || height != mode_h || forced == 2 {
        return Err(ScanoutReject::Extent);
    }
    // A UMD-created exact pPrimaryDesc may already have the proven scan-out
    // shape: DMA_BUF-exportable, dedicated device-local memory, and validated
    // extent/metadata. It may be the current plain OPTIMAL export; QEMU validates
    // that opaque native layout against the original blob allocation size.
    // Other primaries retain the adapter-owned LINEAR target + GPU-copy fallback.
    //
    // The arm IS the constructor. Both validate, resolve the pitch and resolve
    // the wire format identically; a `ScanoutTarget` that exists is a target the
    // host can be told about, and it carries no primary address, so the fallback
    // arm cannot supply one to `publish_displayed_primary`.
    if forced == 3 {
        return Err(ScanoutReject::Layout);
    }
    if forced == 4 {
        return Err(ScanoutReject::Format(source.dxgi_format));
    }
    if forced == 5 {
        return Err(ScanoutReject::LinearAllocFailed(STATUS_NO_MEMORY));
    }
    let target = if source.direct_scanout {
        ScanoutTarget::from_direct_primary(&source, width, height)?
    } else {
        production_linear_scanout(adapter, lock, width, height)?
    };
    // Read the geometry from the VALIDATED target from here on, not from the
    // loose locals: the two are equal by construction (both constructors take
    // the resolved extent) and using the target makes it the single source.
    let bound_wh = adapter.active_scanout_wh.load(Ordering::Acquire);
    let already_bound = adapter.active_scanout_resource.load(Ordering::Acquire)
        == target.resource_id()
        && bound_wh == (((target.width() as u64) << 32) | target.height() as u64);
    if !already_bound || trace_tick {
        crate::diag::record_named_bytes(b"ScRid", target.resource_id());
        crate::diag::record_named_bytes(b"ScPch", target.pitch());
        crate::diag::record_named_bytes(b"ScOff", target.plane_offset());
    }
    if !already_bound {
        let set = crate::virtio::ctrl::set_scanout_blob(
            lock.passive(),
            adapter,
            target.resource_id(),
            target.width(),
            target.height(),
            target.format().virtio(),
            target.pitch(),
            target.plane_offset(),
        );
        if set.is_err() || forced == 6 {
            return Err(ScanoutReject::SetFailed);
        }
        // Keep the adapter-owned fallback cache separate from a rotating DWM
        // direct primary. The latter is tracked by active_scanout_resource and
        // dies with its WDDM allocation; publishing it here would overwrite the
        // durable target's cached Venus identity.
        //
        // Branches on the SOURCE's flag, which is equivalent: the direct arm is
        // taken only when it is set, and the LINEAR arm only when it is clear.
        if !source.direct_scanout {
            adapter.remember_primary_scanout(
                target.resource_id(),
                target.width(),
                target.height(),
                target.pitch(),
                target.plane_offset(),
                target.venus_alloc_size(),
                target.memory_type_index(),
                target.dxgi_format(),
            );
        }
        adapter.remember_scanout_blob(target.resource_id(), target.width(), target.height());
        crate::diag::record_named_bytes(b"ScPub", target.resource_id());
    }
    if !already_bound || trace_tick {
        crate::diag::record_named_bytes(b"ScSet", 1);
    }

    if source.resource_id == target.resource_id() {
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
        // next CRTC_VSYNC retire the preceding flip — and only here can a
        // `ProgrammedPrimary` be minted, which is the only thing
        // `publish_displayed_primary` accepts.
        adapter.publish_displayed_primary(crate::adapter::ProgrammedPrimary::after_scanout_bind(
            source.primary_address,
        ));
        // The caller's `interval` drops AFTER this, clearing the gate once the
        // matching physical address is published — the order the next VSync DPC
        // depends on (it acquires the gate before sampling
        // `last_primary_address`).
        return Ok(ScanoutOutcome::Programmed);
    }

    let target_image_id = adapter.dedicated_scanout_image.load(Ordering::Acquire);
    if target_image_id == 0 || forced == 7 {
        return Err(ScanoutReject::NoTarget);
    }

    // The returned outer wire fence completes in the ring-1 GPU domain. Its DPC
    // callback—not this enqueue—marks scanout dirty and wakes the coalescing
    // async RESOURCE_FLUSH worker, so VNC never samples ahead of the copy.
    if forced == 8 {
        return Err(ScanoutReject::CopyFailed(STATUS_DEVICE_NOT_READY));
    }
    match unsafe {
        crate::ddi::create_allocation::submit_primary_scanout_copy(
            adapter,
            lock,
            h_alloc,
            &source,
            target_image_id,
            width,
            height,
            ticket,
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
            // Nothing was queued, so no DPC will ever run for this interval;
            // the caller drops it and the gate clears, keeping VSync alive.
            return Err(ScanoutReject::CopyFailed(status));
        }
    }
    Ok(ScanoutOutcome::CopyQueued)
}

pub unsafe extern "C" fn dxgkddi_recommend_monitor_modes(
    _adapter: IN_CONST_HANDLE,
    _recommend: IN_CONST_PDXGKARG_RECOMMENDMONITORMODES_CONST,
) -> NTSTATUS {
    crate::diag::record(0x1300_000B);
    let p = _adapter as *const AdapterContext;
    if p.is_null() || !unsafe { (*p).display_half() } {
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
