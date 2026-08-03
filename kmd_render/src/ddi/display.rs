//! Display/VidPn DDIs for the active Helios render+display adapter.
//!
//! Windows identifies the primary through `SetVidPnSourceAddress`; Helios then
//! binds that exact allocation to virtio-gpu scanout. A dedicated LINEAR image
//! remains a compatibility fallback for a primary that is not directly
//! exportable.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use helios_protocol::{HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY, HELIOS_WDDM_ALLOC_KIND_STANDARD};

use crate::adapter::{AdapterContext, ScanoutGuard};
use crate::ddi::create_allocation::{present_alloc_info, PresentAllocationStorage, ScanoutTarget};
use crate::ddi::present_packet::{
    PatchCapacity, PresentAllocations, PresentPayload, PresentSubmissionPrivate,
    STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER,
};
use crate::device::ContextHandleRef;
use crate::dxgk::*;
use crate::irql::PassiveLevel;
use crate::virtio::venus::{OptimalPresentImageDesc, PresentBufferDesc, PresentDestinationDesc};
use crate::virtio::VirtioError;
use helios_kmd_logic::snapshot_bind::SnapshotDescriptor;
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

// RETIRED (0ab-C close-out): the `DXGKARG_PRESENT.pPrivateDriverData` decode
// and its `PBIdOk` cross-check. Measured fact that retired it: dxgkrnl never
// forwarded the UMD's PresentCb private data to DxgkDdiPresent on the DMA-flip
// path — `PBIdOk` read 2 ("no payload") across three driver generations
// (c5/c6/c7, ROADMAP 0ab-C). The ONE per-present channel to the flip arm is
// the inline `HeliosPresentRenderCmd` via `DxgkDdiRender`, stashed per-context
// and taken below (D4b). The UMD stopped writing the dead field in the same
// change.

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
    // Unsampled arm census. `PBflag` records the same bits but is behind
    // `sample_tick`, so it cannot answer "did dxgkrnl keep issuing flips while
    // it stopped naming source addresses?" — which is the open question.
    crate::ddi::scanout_trace::note_present(
        present_flags,
        args.FlipInterval as u32,
        !args.pDmaBuffer.is_null(),
        present_flags & 1 != 0,
        present_flags & (1 << 2) != 0,
    );

    // Decode the payload union arm ONCE, from the flags, before anything reads
    // it. Both this DDI and PresentToHwQueue used to pick `pAllocationList`
    // implicitly out of a three-arm union.
    // SAFETY: `args` is dxgkrnl's present struct; the arm its flags name is the
    // one it initialised.
    let payload = unsafe { PresentPayload::decode(args) };
    let Some(allocation_list) = payload.allocation_list() else {
        // FlipWithMultiPlaneOverlay: unreachable, because the driver does not
        // register the MPO3 KMD interface. Refuse rather than reinterpret an MPO
        // struct as an allocation array.
        crate::diag::record_named_bytes(b"PBmpo", 1);
        PRESENT_LAST_STATUS.store(STATUS_NOT_SUPPORTED as u32, Ordering::Relaxed);
        return STATUS_NOT_SUPPORTED;
    };
    let payload_has_list = allocation_list.is_present();
    // SAFETY: `allocation_list` came from `PresentPayload::decode`, so it is the
    // fixed present allocation array.
    let present_allocations = unsafe { PresentAllocations::from_allocation_list(allocation_list) };

    // The patch-capacity proof, acquired before any host GPU work on the BLT
    // path and consumed by the single write below.
    let mut patch_capacity: Option<PatchCapacity> = None;

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
        crate::diag::record_named_bytes(b"PBalst", u32::from(payload_has_list));
        crate::diag::record_named_bytes(b"PBDma", args.DmaSize);
        crate::diag::record_named_bytes(b"PBPatch", args.PatchLocationListOutSize);
    }

    if sample {
        crate::diag::record_named_bytes(b"PBkpsz", args.DmaBufferPrivateDataSize);
    }
    let present_context = unsafe { ContextHandleRef::from_raw(h_context) };
    let adapter = present_context.as_ref().and_then(ContextHandleRef::adapter);
    // D4b: take (read + CLEAR) this context's stashed snapshot descriptor —
    // the one `DxgkDdiRender` decoded from the UMD's `HeliosPresentRenderCmd`
    // immediately before this present. Taken UNCONDITIONALLY, on every arm
    // (flip, MMIO, BLT): the clear is the orphan bound, so a stash whose
    // present failed cannot leak past the next present on this context. Only
    // the DMA-flip arm below consumes it, after per-arm validation.
    let stashed_snapshot = present_context
        .as_ref()
        .and_then(ContextHandleRef::take_snapshot_stash);
    // The stream marker has the same Render→Present pairing and orphan bound as
    // the snapshot.  Resolve it here, while the UMD context still supplies the
    // documented KMD-process identity, then write the opaque boundary into the
    // DMA private record consumed by SubmitCommand.  A bad/revoked marker is
    // deliberately `None`: the scheduler follows its exact legacy current-wire
    // rule rather than treating an untrusted tail as a future dependency.
    let stashed_stream_marker = present_context
        .as_ref()
        .and_then(ContextHandleRef::take_present_stream_marker_stash);
    let present_stream_boundary = stashed_stream_marker.and_then(|(ctx_id, value, cookie)| {
        let creator_process = present_context
            .as_ref()
            .and_then(ContextHandleRef::creator_process)?;
        let adapter = adapter?;
        adapter
            .with_virtio(|v| {
                v.present_stream_marker_boundary(ctx_id, value, cookie, creator_process)
            })
            .ok()
            .flatten()
    });
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
    if payload_has_list {
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
            // BEFORE any host GPU work: an insufficient-buffer retry must not
            // be able to duplicate the BLT below. The token is carried to the
            // single write site rather than dropped, so the ordering is a
            // type-level fact on this path and not a convention.
            match present_allocations.validate_patch_capacity(args) {
                Ok(capacity) => patch_capacity = Some(capacity),
                Err(status) => {
                    PRESENT_LAST_STATUS.store(status as u32, Ordering::Relaxed);
                    return status;
                }
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
        // Unsampled: which buffer dxgkrnl asked to flip TO, for the whole run.
        // Compared against `Vs*` (which of those it then named through
        // SetVidPnSourceAddress), a difference localises the break to the
        // flip-retirement contract rather than to the bind path.
        crate::ddi::scanout_trace::PRESENT_FLIP_HISTOGRAM.note(source.resource_id);
        // Flip identity, SAMPLED (the 0xE1/0xE2 failure arms above stay
        // unconditional — those are the values a failed Present is read from).
        if sample {
            crate::diag::record_named_bytes(b"PBsrc", source.resource_id);
            crate::diag::record_named_bytes(b"PBsw", source.width);
            crate::diag::record_named_bytes(b"PBsh", source.height);
            crate::diag::record_named_bytes(b"PBsDir", u32::from(source.direct_scanout));
        }

        // The FLIP itself (epoch stamp, VidMm physical address, CRTC_VSYNC
        // retirement) always belongs to the allocation-list source; the ONLY
        // per-present override is the D4b snapshot BIND-TARGET descriptor
        // taken from the Render-command stash above, after per-arm validation.
        // (The old `PBIdOk` cross-check against the PresentCb private payload
        // is retired with the channel — see the note at the top of this file.)

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
            crate::ddi::scanout_trace::note_present_mmio_flip();
            args.MultipassOffset = 0;
            PRESENT_LAST_STATUS.store(STATUS_SUCCESS as u32, Ordering::Relaxed);
            return STATUS_SUCCESS;
        }

        // DMA-BUFFER FLIP. dxgkrnl gave us a DMA buffer, which means it will
        // NOT call SetVidPnSourceAddress for this flip — the driver programs
        // the display when the buffer executes, and the submission fence is
        // what tells dxgkrnl the flip happened. Record the exact allocation and
        // the address dxgkrnl assigned it; `submit_command` picks both up and
        // arms the same deferred programming the MMIO path arms, and the flip's
        // DMA fence then retires behind it instead of ahead of it.
        //
        // The allocation list is the ONLY source for that address here (see
        // `PresentAllocation::physical_address`), which is why it is captured
        // now rather than resolved later.
        let flip_source = match present_allocations.source() {
            Some(allocation) => allocation,
            None => {
                crate::diag::record_named_bytes(b"PBFlip", 0xE4);
                PRESENT_LAST_STATUS.store(STATUS_INVALID_PARAMETER as u32, Ordering::Relaxed);
                return STATUS_INVALID_PARAMETER;
            }
        };
        // The allocation list carries the DEVICE-SPECIFIC open handle; the
        // scan-out path keys on the GLOBAL one. Bridge them through the venus
        // resource id, which is the only identity both sides hold honestly —
        // see `create_allocation::SCANOUT_ALLOCS`. Measured before this
        // existed: `VpDmaF=165, VpDmaA=0, VpPrF=165`, i.e. every DMA flip
        // failed to pair because the open handle is not an `AllocationContext*`.
        let Some(flip_allocation) =
            crate::ddi::create_allocation::scanout_allocation_for_resource(source.resource_id)
        else {
            crate::diag::record_named_bytes(b"PBFlip", 0xE6);
            PRESENT_LAST_STATUS.store(STATUS_INVALID_PARAMETER as u32, Ordering::Relaxed);
            return STATUS_INVALID_PARAMETER;
        };
        // D4b SNAPSHOT SUBSTITUTION, decided HERE and carried by value. The
        // candidate arrived through the RENDER command's per-context stash
        // (taken above) — NOT through `pPrivateDriverData`, which dxgkrnl does
        // not forward to this DDI on flip presents (`PBIdOk` = "no payload"
        // across three driver generations). Every field the stash claims is
        // validated per-arm against the ALLOCATION-LIST source (extent) and
        // the direct-scan-out layout rules including the undersize guard
        // (`helios_kmd_logic::snapshot_bind`). Any failure falls back to
        // binding the flipped allocation exactly as today, counted `SnFbk`; a
        // validated descriptor counts `SnSub` ONCE per flip, at this single
        // decision site.
        let snapshot = stashed_snapshot.and_then(|candidate| {
            match helios_kmd_logic::snapshot_bind::validate(
                &candidate,
                source.width,
                source.height,
            ) {
                Ok(()) => {
                    crate::ddi::scanout_trace::note_snapshot_substituted();
                    Some(candidate)
                }
                Err(_) => {
                    crate::ddi::scanout_trace::note_snapshot_fallback();
                    None
                }
            }
        });
        if let Err(status) = unsafe {
            crate::ddi::present_packet::PresentFlipPrivate::write(
                args.pDmaBufferPrivateData,
                args.DmaBufferPrivateDataSize,
                flip_allocation,
                flip_source.physical_address(),
                snapshot,
            )
        } {
            crate::diag::record_named_bytes(b"PBFlip", 0xE5);
            PRESENT_LAST_STATUS.store(status as u32, Ordering::Relaxed);
            return status;
        }
        crate::ddi::scanout_trace::note_present_dma_flip(
            source.resource_id,
            flip_source.segment_id(),
        );
    }

    // The BLT path carried its token here; every other path queues nothing, so
    // acquiring immediately before the write is correct and says so.
    let capacity = match patch_capacity {
        Some(capacity) => capacity,
        None => match present_allocations.validate_patch_capacity(args) {
            Ok(capacity) => capacity,
            Err(status) => {
                PRESENT_LAST_STATUS.store(status as u32, Ordering::Relaxed);
                return status;
            }
        },
    };
    if let Err(status) = unsafe { present_allocations.write_patch_references(capacity, args) } {
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
            // KMD-generated desktop marker: no UMD stream association.
            // Zero-fill the appended v1-compatible tail explicitly so an old
            // consumer sees its exact 16-byte prefix and a new one follows the
            // legacy current-wire path.
            present_ctx_id: 0,
            present_value: 0,
            present_cookie: 0,
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

    if let Some(boundary) = present_stream_boundary {
        if let Err(status) = unsafe {
            PresentSubmissionPrivate::merge_stream_boundary(
                args.pDmaBufferPrivateData,
                args.DmaBufferPrivateDataSize,
                boundary,
            )
        } {
            PRESENT_LAST_STATUS.store(status as u32, Ordering::Relaxed);
            return status;
        }
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
    // Unsampled, atomics-only, and FIRST — before any refusal below can hide the
    // fact that dxgkrnl called at all. `VpEnt` not moving during a workload is
    // itself the answer to "why is the bind pinned"; every sampled `Sc*` value
    // is silent about it. Legal here: this is a plain atomic increment with no
    // registry transaction, and the DDI can arrive at DIRQL.
    crate::ddi::scanout_trace::note_ddi_entry();
    // Windows names the authoritative desktop primary here. This—not resource
    // dimensions, OM bindings, process name, or an arbitrary Present call—is the
    // only allocation the KMD may bind directly or copy into the fallback image.
    if address.is_null() {
        crate::ddi::scanout_trace::note_ddi_null_arg();
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
        crate::ddi::scanout_trace::note_ddi_pair_failed();
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
        crate::ddi::scanout_trace::note_ddi_split(true);
        // `swap`, not `store`, purely so the overwritten value is observable:
        // this slot holds ONE pending handle, so dxgkrnl flipping faster than
        // the PASSIVE worker drains silently discards every intermediate
        // primary. That coalescing is by design, but its RATE is exactly the
        // measurement missing from "the bind stays pinned" — and it was
        // previously unobservable, because a discarded handle leaves no trace
        // anywhere. The stored value and its ordering are unchanged.
        let previous = adapter
            .pending_vidpn_allocation
            .swap(h_alloc as usize, Ordering::AcqRel);
        if previous != 0 && previous != h_alloc as usize {
            crate::ddi::scanout_trace::note_ddi_coalesced(previous);
        }
        return STATUS_SUCCESS;
    }

    // SAFETY: `DxgkDdiSetVidPnSourceAddress` is documented callable up to DIRQL
    // (dxgkrnl's MMIO-flip path invokes it under DxgkCbSynchronizeExecution), so
    // the annotation proves nothing here — the runtime check immediately above
    // does. This mint is downstream of it and must stay there.
    crate::ddi::scanout_trace::note_ddi_split(false);
    let passive = unsafe { crate::irql::PassiveLevel::assume() };
    unsafe { apply_vidpn_source_address(passive, adapter, h_alloc) }
}

/// Arm the deferred scan-out programming for the DMA-BUFFER FLIP contract.
///
/// The MMIO path reaches the same state through
/// [`set_vidpn_source_address_dirql`]; this is the entry point for flips
/// dxgkrnl never calls `SetVidPnSourceAddress` for, and it is called from the
/// submit path when the flip's DMA buffer is handed to the scheduler.
///
/// The DIFFERENCE THAT MATTERS is not in here, it is in the caller: on the MMIO
/// path the DDI returns STATUS_SUCCESS immediately and dxgkrnl treats that as
/// the flip having happened, so the programming this arms lands after dxgkrnl
/// has already moved on. Here the flip's DMA fence is still outstanding, so
/// dxgkrnl is still waiting when the programming runs.
///
/// Returns false if the handle could not be paired, in which case the gate is
/// NOT raised.
///
/// # Safety
/// `h_alloc` is the allocation handle dxgkrnl placed in the present allocation
/// list and this driver copied into the kernel-only DMA private data.
pub(crate) unsafe fn arm_dma_flip_programming(
    adapter: &AdapterContext,
    h_alloc: HANDLE,
    primary_address: u64,
    present_epoch: u64,
    snapshot: Option<SnapshotDescriptor>,
) -> bool {
    // Segment and flags are the MMIO path's `DXGKARG_SETVIDPNSOURCEADDRESS`
    // fields, which this contract has no equivalent of. The segment is only
    // ever echoed back in diagnostics, and the flags word carries no bit this
    // driver reads, so pairing 0 for both is honest rather than invented.
    //
    // `present_epoch` rides on the allocation for a load-bearing reason:
    // `pending_vidpn_allocation` is a single slot that coalesces (`VpCoal`), so
    // a parallel "newest epoch" atomic would let the worker pair one flip's
    // handle with a different flip's epoch.
    //
    // The frame boundary rides along for that same reason AND for its own: the
    // mark table holds one entry per resource, so by the time the PASSIVE worker
    // binds, the same buffer's next present may already have replaced it. Taken
    // HERE — the last point at which the mark still belongs to the frame being
    // flipped — and consumed at the bind (ROADMAP defect 0ab-B, D1(i)).
    //
    // KEYED BY THE BOUND IDENTITY (the one-line key alignment that preserves
    // 0ab-A across D4b): the marker records the watermark under the UMD's
    // private-data resid, which on a substituted flip is the SNAPSHOT resid —
    // so a substituted flip takes by the descriptor's resid, and everything
    // downstream (`arm_bind_refresh`, the D2 identity arm, `RfUnb`, epochs,
    // leases, the D4a ledger) self-aligns because armed = bound = snapshot.
    let frame_watermark = adapter.take_flip_frame_watermark(match snapshot {
        Some(snap) => snap.resource_id,
        None => unsafe { crate::ddi::create_allocation::allocation_resource_id(h_alloc) },
    });
    if !unsafe {
        crate::ddi::create_allocation::set_vidpn_primary_address(
            h_alloc,
            0,
            primary_address,
            0,
            present_epoch,
            frame_watermark,
            snapshot,
        )
    } {
        crate::ddi::scanout_trace::note_ddi_pair_failed();
        return false;
    }
    let _ticket = adapter.raise_programming_gate();
    let previous = adapter
        .pending_vidpn_allocation
        .swap(h_alloc as usize, Ordering::AcqRel);
    if previous != 0 && previous != h_alloc as usize {
        crate::ddi::scanout_trace::note_ddi_coalesced(previous);
    }
    // Everything above is UNCHANGED, and that is the design: the fast bind is a
    // pure accelerator. The slot is armed, the gate is raised and the worker
    // will run exactly as it does today — the only difference is that the host
    // may already be bound by the time it gets there, in which case its
    // `already_bound` arm short-circuits the wire bind (`VpSkip` rises, which is
    // expected and benign).
    unsafe {
        fast_bind_from_flip(
            adapter,
            h_alloc,
            primary_address,
            present_epoch,
            frame_watermark,
            snapshot,
        )
    };
    // Fast staging gets the first chance to publish or retain this exact
    // boundary. Only then wake the PASSIVE fallback, so it can observe the
    // fast completion rather than race a duplicate synchronous SET.
    adapter.signal_hpd();
    true
}

/// Enqueue this flip's `SET_SCANOUT_BLOB` NOW, from the flip arm, instead of
/// waiting for the PASSIVE display worker to get to it (ROADMAP defect 0ab-C,
/// D1(ii)).
///
/// WHY. The worker's bind cadence was measured bimodal — 44–48 % of binds 1–3 ms
/// apart, 19–24 % 10–14 ms apart, 10–11 % beyond 20 ms — against a 4.8 ms flip
/// cadence at GT2's 210 fps. Every stall pushes some bind two or more frame
/// periods past its present, and by the time it lands the app has had the buffer
/// back and re-cleared it: 370/391 of a run's black flushes sat in the 1–3 ms
/// bind-age bucket of FIRST reads, i.e. the read was prompt and the BIND was
/// late. Enqueueing here makes the host-side bind moment ≈ flip + ctrl transit,
/// independent of the worker's stalls, and the control queue is FIFO so the
/// earlier enqueue wins even when the worker later races through its own path.
///
/// WHAT IT IS NOT. It does not claim `pending_vidpn_allocation` (that would hide
/// the handle from `retire_scanout_allocation_locked`'s cancel CAS — a
/// use-after-free), it does not touch the `vidpn_programming` gate, it does not
/// wait, and it never becomes the only binder: every bail-out below simply
/// leaves today's behaviour in place, counted.
///
/// IRQL: DISPATCH_LEVEL. Reached from `DxgkDdiSubmitCommand`'s flip arm, which
/// already takes `wddm_notify_lock` here (`take_flip_frame_watermark`), so
/// taking the virtio spinlock is equally legal. The MMIO/DIRQL arm
/// (`set_vidpn_source_address_dirql`) deliberately gets NO fast path: device
/// DIRQL cannot take a DISPATCH spinlock, and the desktop contract has no
/// measured black-frame defect to fix.
///
/// # Safety
/// `h_alloc` is the live KMD allocation handle from the flip's private data —
/// the same one the caller has just paired an address with.
unsafe fn fast_bind_from_flip(
    adapter: &AdapterContext,
    h_alloc: HANDLE,
    primary_address: u64,
    present_epoch: u64,
    frame_watermark: u64,
    snapshot: Option<SnapshotDescriptor>,
) {
    use crate::ddi::scanout_trace::skip;

    let knobs = adapter.knobs();
    if !knobs.dispatch_bind {
        crate::ddi::scanout_trace::note_fast_bind_skip(skip::KNOB_OFF);
        return;
    }
    if knobs.bind_flush_immediate {
        // Mode 1 is the diagnostic that measures the OLD bind→flush ordering.
        // Accelerating the bind underneath it would silently change what that
        // A/B answers.
        crate::ddi::scanout_trace::note_fast_bind_skip(skip::DIAG_MODE);
        return;
    }
    // SAFETY: per this function's contract — the handle dxgkrnl placed in the
    // present allocation list, which the caller has already resolved once.
    let source = match unsafe { crate::ddi::create_allocation::scanout_alloc_info(h_alloc) } {
        Some(source) if source.direct_scanout => source,
        _ => {
            crate::ddi::scanout_trace::note_fast_bind_skip(skip::NOT_DIRECT);
            return;
        }
    };
    // D4b: a carried snapshot descriptor substitutes the BIND TARGET, by value
    // (`from_snapshot_descriptor` re-runs the same layout validation the
    // Present arm already passed). Structurally-unreachable failure falls back
    // to the flipped allocation, counted — the §6 "descriptor fails" row.
    let substituted = snapshot.and_then(|snap| {
        match ScanoutTarget::from_snapshot_descriptor(&snap) {
            Ok(target) => Some(target),
            Err(_) => {
                crate::ddi::scanout_trace::note_snapshot_fallback();
                None
            }
        }
    });
    // The SAME validation the worker runs, through the SAME constructor: the
    // pitch/offset/size/format checks are the undersize guard that keeps QEMU
    // from reading past the blob (the 38th-session Xid-31 lesson), and they are
    // not restated here in any weakened form.
    let target = match substituted {
        Some(target) => target,
        None => match ScanoutTarget::from_direct_primary(&source, source.width, source.height) {
            Ok(target) => target,
            Err(_) => {
                crate::ddi::scanout_trace::note_fast_bind_skip(skip::LAYOUT);
                return;
            }
        },
    };
    // The STEADY-STATE test, and it is also the mode check. `active_scanout_wh`
    // is only ever written by a bind that already proved its extent equals the
    // advertised mode, so an extent equal to it is equal to the mode — while a
    // first bind (0) and a mode change (a different extent) both fall through to
    // the worker, which owns mode-set ordering and the LINEAR fallback.
    let wh = ((target.width() as u64) << 32) | target.height() as u64;
    if wh == 0 || adapter.active_scanout_wh.load(Ordering::Acquire) != wh {
        crate::ddi::scanout_trace::note_fast_bind_skip(skip::EXTENT);
        return;
    }
    // "Already bound" means the newest ENQUEUED bind already names this
    // resource — the identity the host will hold when this flip's bind would
    // land — NOT the identity already applied.
    //
    // Comparing against the applied `active_scanout_resource` (22.22.219.0) was
    // measured to skip ~1-in-3 eligible flips, flat across the scene: at 2-deep
    // pipelining the applied identity runs 1–2 flips behind, so the very flip
    // this path exists to accelerate looks already bound (`FsC0` 2140/2192 per
    // run, every skip reason 6; ROADMAP defect 0ab-C). Those uncovered flips
    // rode only the worker's bind, whose lateness climbs late-scene — the
    // residual black.
    //
    // Do not use `scanout_bind_wire_resource == resource_id` as a skip here.
    // It says only that W1 was published, not that W1's carried producer
    // boundary includes this W2 frame. Skipping W2 can leave the newest marker
    // unarmed forever when W1 consumes the mark-table entry.
    let request = crate::virtio::ScanoutBindRequest {
        resource_id: target.resource_id(),
        width: target.width(),
        height: target.height(),
        format: target.format().virtio(),
        stride: target.pitch(),
        offset: target.plane_offset(),
        present_epoch,
        primary_address,
        // The SAME boundary the caller stamped on the allocation for the
        // worker's own bind (D1(i)); whichever bind lands first orders its flush
        // against this frame, not against a later sample.
        carried_watermark: frame_watermark,
    };
    let outcome = adapter.with_virtio(|v| {
        // The producer boundary is part of the request, not a new sample. An
        // unready request occupies the fixed fast-bind slot until the used-ring
        // completion DPC promotes it; no buffer, wire sequence, wait, or host
        // command is consumed before then.
        v.stage_scanout_bind(request, |resource_id| {
            adapter.mint_scanout_bind_seq(resource_id)
        })
    });
    match outcome {
        Ok(crate::virtio::FastBindDispatch::Queued) => {
            crate::ddi::scanout_trace::note_fast_bind_enqueued()
        }
        Ok(crate::virtio::FastBindDispatch::Deferred)
        | Ok(crate::virtio::FastBindDispatch::Handled) => {}
        Ok(crate::virtio::FastBindDispatch::Busy) => {
            crate::ddi::scanout_trace::note_fast_bind_busy()
        }
        Ok(crate::virtio::FastBindDispatch::Failed) => {
            crate::ddi::scanout_trace::note_fast_bind_error()
        }
        Err(_) => crate::ddi::scanout_trace::note_fast_bind_skip(skip::NO_TRANSPORT),
    }
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
            // NO_LEASE: on the MMIO/`FlipOnVSyncMmIo` contract dxgkrnl retires
            // the flip BEFORE calling this DDI, so there is no fence of ours
            // left to gate and nothing for a presentation lease to protect. It
            // is also the contract DWM's 3-deep desktop chain uses, where the
            // 0ab-B window has never been observed and where withholding the
            // address from CRTC_VSYNC could cost a whole refresh interval.
            helios_kmd_logic::scanout_lease::NO_LEASE,
            // No carried frame boundary either: dxgkrnl retired this flip before
            // calling us, so "now" at the bind IS this frame's boundary and
            // there is nothing earlier to carry.
            0,
            // No snapshot on the MMIO/desktop contract — and passing `None`
            // ZEROES any stamp a previous DMA flip of this allocation left, so
            // a stale descriptor can never leak into a desktop bind.
            None,
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
/// (`adapter/mod.rs`'s `last_primary_address` contract and `adapter/kobj.rs`'s VSync
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
    match unsafe { program_vidpn_source(adapter, lock, h_alloc, interval.ticket(), true) } {
        Ok(ScanoutOutcome::Programmed) => {
            clear_retry_state();
            // A newer DIRQL handoff can raise its generation and fill the slot
            // between this worker's swap and `ProgrammingInterval::adopt`. If it
            // is pending now, this token belongs to that newer interval and must
            // stay raised for its worker pass.
            if adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0 {
                interval.retain_for_retry();
            }
            STATUS_SUCCESS
        }
        Ok(ScanoutOutcome::CopyQueued) => {
            clear_retry_state();
            release_leases_for_copy_fallback(adapter);
            if adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0 {
                // Do not hand a newer interval's ticket to this older copy DPC.
                // The pending worker owns the gate; the copy completion's clear
                // would otherwise race that primary's programming.
                interval.retain_for_retry();
            } else {
                interval.transfer_to_completion();
            }
            STATUS_SUCCESS
        }
        Ok(ScanoutOutcome::Deferred) => {
            // A newer DIRQL flip may have filled the one pending slot after this
            // worker took `h_alloc`. Never overwrite that exact newer handle,
            // but keep the programming gate raised for whichever handle now owns
            // forward progress. Dropping `interval` on the CAS-failure arm can
            // clear the generation the newer flip just raised and make VSync
            // report a primary that has not been programmed yet.
            let _ = adapter.pending_vidpn_allocation.compare_exchange(
                0,
                h_alloc as usize,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            interval.retain_for_retry();
            STATUS_SUCCESS
        }
        Err(reject) => {
            reject.report();
            if reject.retryable() {
                match note_retry_attempt(h_alloc) {
                    RetryDecision::Again => {
                        // Re-arm the exact handle only while the slot is empty.
                        // The DIRQL producer does not take the scanout lifecycle
                        // lock, so a plain store here could overwrite a newer
                        // Windows allocation. Either way the nonzero slot owns
                        // the still-raised programming gate.
                        let _ = adapter.pending_vidpn_allocation.compare_exchange(
                            0,
                            h_alloc as usize,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        interval.retain_for_retry();
                    }
                    // Budget exhausted. Drop the interval: the gate clears and
                    // the heartbeat resumes with the truthful OLD address, which
                    // is a visibly stale desktop rather than a frozen one.
                    RetryDecision::GaveUp => {
                        release_leases_for_reject(adapter);
                        if adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0 {
                            interval.retain_for_retry();
                        }
                    }
                }
            } else {
                // Permanent for this allocation. Retrying would hold the gate
                // and suppress CRTC_VSYNC for nothing.
                clear_retry_state();
                release_leases_for_reject(adapter);
                if adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0 {
                    interval.retain_for_retry();
                }
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
    /// Its exact producer stream was destroyed or its resource is retiring.
    ProducerAbandoned,
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
            Self::ProducerAbandoned => crate::diag::record_named_bytes(b"ScSet", 0xE4),
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
            Self::ProducerAbandoned => &SC_UNAVAILABLE,
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
            Self::ProducerAbandoned => STATUS_SUCCESS,
        }
    }

    /// Small dense code for the unsampled ring record (`Vp<n>A` bits 8..15).
    ///
    /// Nonzero for every variant, because 0 is the ring's "reached an outcome"
    /// value. Owner debugging ABI like [`Self::record`]: append, do not
    /// renumber.
    fn trace_code(self) -> u32 {
        match self {
            Self::BadAlloc => 1,
            Self::Extent => 2,
            Self::Layout => 3,
            Self::Format(_) => 4,
            Self::LinearAllocFailed(_) => 5,
            Self::SetFailed => 6,
            Self::NoTarget => 7,
            Self::CopyFailed(_) => 8,
            Self::ProducerAbandoned => 9,
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
            Self::BadAlloc
            | Self::Extent
            | Self::Layout
            | Self::Format(_)
            | Self::ProducerAbandoned => false,
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
    /// The direct fallback retained its exact WDDM handle until the producer
    /// boundary retires. The completion DPC wakes the existing worker.
    Deferred,
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
    crate::diag::record_named_bytes(
        b"ScAlcFul",
        crate::ddi::create_allocation::SCANOUT_ALLOC_FULL.load(Relaxed),
    );
    crate::diag::record_named_bytes(b"ScRetry", SC_RETRY.load(Relaxed));
    crate::diag::record_named_bytes(b"ScGaveUp", SC_GAVE_UP.load(Relaxed));
    // R509: gate-generation health. Both must read 0 on a normal boot.
    crate::diag::record_named_bytes(b"ScStale", crate::adapter::GATE_STALE_CLEARS.load(Relaxed));
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
    match unsafe { program_vidpn_source(adapter, lock, h_alloc, interval.ticket(), false) } {
        Ok(ScanoutOutcome::Programmed) => {
            if adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0 {
                interval.retain_for_retry();
            }
            STATUS_SUCCESS
        }
        Ok(ScanoutOutcome::CopyQueued) => {
            release_leases_for_copy_fallback(adapter);
            // THE one hand-off. The copy is queued on ring 1 and its completion
            // DPC owns the gate from here: it publishes the displayed address on
            // success and clears the gate either way. Dropping the interval
            // instead would clear the gate mid-programming and let a CRTC_VSYNC
            // report the OLD address as authoritative.
            if adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0 {
                interval.retain_for_retry();
            } else {
                interval.transfer_to_completion();
            }
            STATUS_SUCCESS
        }
        Ok(ScanoutOutcome::Deferred) => {
            // Preserve a newer exact pending allocation if DIRQL published one
            // while this PASSIVE call was running, and in both CAS arms retain
            // the gate for the handle that now owns forward progress.
            let _ = adapter.pending_vidpn_allocation.compare_exchange(
                0,
                h_alloc as usize,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            interval.retain_for_retry();
            STATUS_SUCCESS
        }
        Err(reject) => {
            // No retry on this path: the DDI ran at PASSIVE, so the refusal's
            // NTSTATUS reaches dxgkrnl directly and is the truthful answer.
            // Nothing was deferred, so there is nothing to re-arm.
            reject.report();
            release_leases_for_reject(adapter);
            if adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0 {
                interval.retain_for_retry();
            }
            reject.status()
        }
    }
}

/// Terminal programming refusal: nothing was bound, so nothing can be read
/// (ROADMAP defect 0ab-B).
///
/// Every presentation minted up to now is unreadable — the display is already in
/// its degraded state and each refusal carries its own `ScanoutReject` counter —
/// so holding the flip fences on reads that cannot happen would trade a stale
/// frame for a TDR and a frozen desktop. This is an exact terminal event, NOT a
/// timeout: it runs only on a refusal that will not be retried, and it moves
/// `LsCanc`, so a boot where it is the dominant lease-end reason is visible
/// rather than looking healthy.
fn release_leases_for_reject(adapter: &AdapterContext) {
    adapter.release_all_scanout_leases(crate::ddi::scanout_trace::LeaseEnd::Cancelled);
}

/// The adapter-owned LINEAR fallback queued a GPU copy instead of binding the
/// app's own buffer.
///
/// SCOPE LIMIT, stated rather than hidden: on this path the consumer of the
/// app's buffer is the KMD's ring-1 `vkCmdCopyImage`, whose completion this
/// driver does not key to a presentation epoch — so the lease gate does not
/// cover it and the path behaves exactly as it did in 22.22.212.0. That is
/// deliberate: 0ab-B was measured entirely on the DIRECT primary (a fullscreen
/// app's own DMA-BUF), the fallback has no measured black-frame defect, and
/// leaving the lease open here would hang the flip fence instead. Counted as
/// `LsCanc` so the rate is visible.
fn release_leases_for_copy_fallback(adapter: &AdapterContext) {
    adapter.release_all_scanout_leases(crate::ddi::scanout_trace::LeaseEnd::Cancelled);
}

/// Scratch for the unsampled ring record, filled in as [`program_vidpn_source_inner`]
/// learns each field.
///
/// It exists so the record is emitted from ONE place. The body has eight early
/// `return Err(...)` exits, and an instrument that each of them has to remember
/// to call is an instrument that silently omits exactly the refusal being
/// hunted.
struct ProgramTrace {
    source_resource: u32,
    target_resource: u32,
    flags: u32,
}

/// The programming body. One happy shape, one unhappy shape.
///
/// `deferred` says which wrapper called: the PASSIVE worker draining a DIRQL
/// handoff, or the DDI running inline at PASSIVE. It is recorded, never acted
/// on.
unsafe fn program_vidpn_source(
    adapter: &AdapterContext,
    lock: &ScanoutGuard<'_>,
    h_alloc: HANDLE,
    ticket: crate::adapter::ProgrammingTicket,
    deferred: bool,
) -> Result<ScanoutOutcome, ScanoutReject> {
    use crate::ddi::scanout_trace::flags;

    let mut trace = ProgramTrace {
        source_resource: 0,
        target_resource: 0,
        flags: if deferred { flags::DEFERRED } else { 0 },
    };
    let result =
        unsafe { program_vidpn_source_inner(adapter, lock, h_alloc, ticket, &mut trace) };
    let reject = match result {
        Ok(ScanoutOutcome::Programmed) => {
            trace.flags |= flags::PROGRAMMED;
            0
        }
        Ok(ScanoutOutcome::CopyQueued) => {
            trace.flags |= flags::COPY_QUEUED;
            0
        }
        Ok(ScanoutOutcome::Deferred) => 0,
        Err(reject) => reject.trace_code(),
    };
    crate::ddi::scanout_trace::note_program(
        h_alloc as usize,
        trace.source_resource,
        trace.target_resource,
        trace.flags,
        reject,
    );
    result
}

unsafe fn program_vidpn_source_inner(
    adapter: &AdapterContext,
    lock: &ScanoutGuard<'_>,
    h_alloc: HANDLE,
    ticket: crate::adapter::ProgrammingTicket,
    trace: &mut ProgramTrace,
) -> Result<ScanoutOutcome, ScanoutReject> {
    use crate::ddi::scanout_trace::flags;

    crate::diag::record(0x1300_000A);
    let source_address_n = VIDPN_SOURCE_ADDRESS_COUNT
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    let trace_tick = source_address_n == 1 || source_address_n % 600 == 0;
    if trace_tick {
        crate::diag::record_named_bytes(b"VpSA", source_address_n);
    }

    let source = match unsafe { crate::ddi::create_allocation::scanout_alloc_info(h_alloc) } {
        Some(source) => source,
        None => return Err(ScanoutReject::BadAlloc),
    };
    trace.source_resource = source.resource_id;
    if source.direct_scanout {
        trace.flags |= flags::DIRECT;
    }
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
    //
    // D4b: a flip that carried a validated snapshot descriptor (stamped on the
    // allocation beside the epoch/watermark by `set_vidpn_primary_address`)
    // binds the SNAPSHOT here too, so the worker and the DISPATCH fast bind
    // substitute consistently for the same flip. Constructor failure — only
    // reachable from a torn stamp, since the descriptor validated at Present —
    // falls back to the primary itself, counted (`SnFbk`).
    let target = if source.direct_scanout {
        let substituted = source.snapshot.and_then(|snap| {
            match ScanoutTarget::from_snapshot_descriptor(&snap) {
                Ok(target) => Some(target),
                Err(_) => {
                    crate::ddi::scanout_trace::note_snapshot_fallback();
                    None
                }
            }
        });
        match substituted {
            Some(target) => target,
            None => ScanoutTarget::from_direct_primary(&source, width, height)?,
        }
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
    trace.target_resource = target.resource_id();
    if already_bound {
        trace.flags |= flags::ALREADY_BOUND;
    }
    if !already_bound || trace_tick {
        crate::diag::record_named_bytes(b"ScRid", target.resource_id());
        crate::diag::record_named_bytes(b"ScPch", target.pitch());
        crate::diag::record_named_bytes(b"ScOff", target.plane_offset());
    }
    // Filled only by the host-accepted direct bind application. The arm itself
    // runs inside the notify-ordered transition below; reporting stays outside
    // the spinlock because it does not participate in the ordering proof.
    let mut bind_refresh = None;
    if !already_bound {
        let worker_request = source.direct_scanout.then_some(crate::virtio::ScanoutBindRequest {
            resource_id: target.resource_id(),
            width: target.width(),
            height: target.height(),
            format: target.format().virtio(),
            stride: target.pitch(),
            offset: target.plane_offset(),
            present_epoch: source.present_epoch,
            primary_address: source.primary_address,
            carried_watermark: source.frame_watermark,
        });
        if let Some(request) = worker_request {
            let worker = adapter
                .with_virtio(|v| v.stage_worker_scanout_bind(request))
                .unwrap_or(crate::virtio::WorkerBindDispatch::Abandoned);
            match worker {
                crate::virtio::WorkerBindDispatch::Ready => {}
                crate::virtio::WorkerBindDispatch::Waiting => {
                    // Keep the exact WDDM allocation in the existing pending
                    // slot. The used-ring DPC wakes this worker only after the
                    // producer boundary retires; no synchronous SET can
                    // overtake Venus.
                    return Ok(ScanoutOutcome::Deferred);
                }
                crate::virtio::WorkerBindDispatch::Abandoned => {
                    return Err(ScanoutReject::ProducerAbandoned);
                }
            }
        }
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
        let Ok(bind_seq) = set else {
            if let Some(request) = worker_request {
                let _ = adapter.with_virtio(|v| v.release_failed_sync_worker_bind(request));
            }
            return Err(ScanoutReject::SetFailed);
        };
        // Persist the host-visible selection before any later bookkeeping.
        // DestroyAllocation consults this under virtio_lock so it can disable a
        // resource even if a DPC/worker handoff has not yet published it.
        let _ = adapter.with_virtio(|v| {
            v.note_host_accepted_scanout_bind(bind_seq, target.resource_id())
        });
        trace.flags |= flags::BOUND;
        // THE WIRE-ORDER GUARD. This bookkeeping runs at PASSIVE after the
        // round-trip returned, so it can chronologically FOLLOW the application
        // of a bind that was enqueued after it — the ordinary case at a
        // fullscreen frame rate, where a flip arms the DISPATCH fast bind while
        // this round-trip is still outstanding. Stomping the newer identity with
        // this older one would leave `host_bound_scanout_resource` naming a
        // buffer the host is no longer reading, which the flush executor's
        // `RfUnb` arm then refuses for the rest of the binding's life.
        let adopted = adapter.with_wddm_notify_lock(|guard| {
            let adopted = adapter.adopt_scanout_bind_seq(bind_seq);
            if adopted {
                // Sample BEFORE `remember_scanout_blob` overwrites it. Only a
                // RESOURCE change supersedes an older presentation's lease; a
                // same-resource rebind can still be read by a later flush.
                let previous = adapter.host_bound_scanout_resource.load(Ordering::Acquire);
                // Keep the adapter-owned fallback cache separate from a rotating
                // DWM direct primary.  All identity publication is serialized
                // with the DPC's fast-bind transition.
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
                adapter.remember_scanout_blob(
                    target.resource_id(),
                    target.width(),
                    target.height(),
                );
                if source.direct_scanout {
                    let superseded = previous != 0 && previous != target.resource_id();
                    adapter.publish_bound_epoch(source.present_epoch, superseded);
                    adapter.publish_bound_primary(source.primary_address);

                    let refresh = if adapter.knobs().bind_flush_immediate {
                        adapter.request_scanout_refresh_for_locked(guard, target.resource_id());
                        (true, false)
                    } else {
                        let refresh = adapter.arm_bind_refresh_locked(
                            guard,
                            target.resource_id(),
                            source.frame_watermark,
                        );
                        if refresh.0 {
                            adapter
                                .request_scanout_refresh_for_locked(guard, target.resource_id());
                        }
                        refresh
                    };
                    bind_refresh = Some(refresh);
                }
            }
            adopted
        });
        if adopted {
            // Registry diagnostics are PASSIVE-only. The notify closure above
            // runs at DISPATCH_LEVEL and must remain atomics/event/transport
            // state only.
            crate::diag::record_named_bytes(b"ScPub", target.resource_id());
        } else {
            crate::ddi::scanout_trace::note_fast_bind_late();
        }
    }

    if already_bound && source.direct_scanout {
        // A fast bind can replace the resource after the optimistic loads that
        // formed `already_bound`. Revalidate under the same notify lock used by
        // every bind application before publishing this presentation's epoch and
        // primary. If it changed, the newer bind already published the truthful
        // address; this stale worker must publish nothing.
        let _still_bound = adapter.with_wddm_notify_lock(|_| {
            let still_bound = adapter.active_scanout_resource.load(Ordering::Acquire)
                == target.resource_id()
                && adapter.active_scanout_wh.load(Ordering::Acquire)
                    == (((target.width() as u64) << 32) | target.height() as u64);
            if still_bound {
                adapter.publish_bound_epoch(source.present_epoch, false);
                adapter.publish_bound_primary(source.primary_address);
            }
            still_bound
        });
    }
    if !already_bound || trace_tick {
        crate::diag::record_named_bytes(b"ScSet", 1);
    }

    // The direct arm is the zero-copy arm. This used to test
    // `source.resource_id == target.resource_id()`, which was equivalent while
    // the direct constructor could only name the source's own resource; a D4b
    // snapshot target is ALSO zero-copy (the venus-queue blit already produced
    // its content), so the predicate is now the arm itself.
    if source.direct_scanout {
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
        // A BIND IS ITSELF A DIRTY EDGE. SET_SCANOUT_BLOB changes which resource
        // the host pixel pipeline reads; it does not make the host read it. A
        // buffer that has just become the scan-out has never been fetched, so
        // until something flushes it the display shows whatever the host last
        // read — stale content, or nothing.
        //
        // The comment above ("the matching Render marker and used-ring
        // retirement are the sole producers of the dirty edge") was true only
        // while the bind never moved: one desktop primary was bound once and
        // DWM's per-frame markers dirtied it forever after. Once
        // `FlipImmediateMmIo` made the bind rotate per flip, that stopped
        // holding, and the QEMU trace showed the consequence exactly
        // (2026-07-29, `virtio_gpu_cmd_*` via QMP):
        //
        //     .539 res_flush 0x121     <- flushes the OLD binding
        //     .542 set_scanout_blob 0x128   <- new binding, nobody reads it
        //     .666 res_flush 0x128     <- 124 ms later
        //     .669 set_scanout_blob 0x121   <- and again
        //     .761 res_flush 0x121     <- 92 ms later
        //
        // i.e. roughly every other flip left a freshly bound buffer on screen
        // unread for ~100 ms. That is the residual black/stale frame.
        //
        // COMPLETION-ORDERED, and that is defect 0ab (2026-07-29). The edge
        // itself is right; firing it HERE was not.
        //
        // The claim this arm shipped with — "the buffer's own Venus work is
        // already retired when the bind runs, 425 of 425 binds" — was measured
        // on the MMIO contract, where dxgkrnl retired the flip before calling
        // `SetVidPnSourceAddress`. The DMA-buffer contract deliberately moved
        // the arming EARLIER: `arm_dma_flip` runs in `DxgkDdiSubmitCommand`
        // *with the flip's DMA fence still outstanding*, and the app's real work
        // never travels in that DMA buffer at all — it goes to the host over the
        // Venus escape channel. So at bind time the frame is SUBMITTED, not
        // COMPLETE, and an immediate `request_scanout_refresh_for` tells QEMU to
        // read a buffer whose GPU work has only got as far as its clear.
        //
        // Measured on the host, Fire Strike Combined, KMD 22.22.205.0 (QEMU
        // `virtio_gpu_cmd_*` over QMP + an RFB sampler on the VNC surface, using
        // 3DMark's fps bar as a "did this frame finish?" oracle):
        //
        //   bind -> this flush            0.2 ms   (submission)
        //   bind -> the marker-edge flush 10.2 ms  (completion; the frame's GPU time)
        //   VNC frame sampled BETWEEN the two, gap >= 10 ms:  48/60  = 80 % BLACK
        //   VNC frame sampled AFTER the marker-edge flush:    32/287 = 11 %
        //
        // i.e. the display is black for the ~10 ms this flush opens up, and the
        // marker edge repaints it. That is the flash, and it is why a
        // producer-side CPU stall only REDUCED it: stalling the app shrinks the
        // window instead of ordering the read.
        //
        // Arming through the Venus watermark keeps everything the bind edge
        // exists for — a bind that changed the binding is still guaranteed its
        // own flush, naming its own resource (defect 0aa) — and costs no CPU
        // wait: the flush is issued from the completion DPC via
        // `take_ready_scanout_refresh`.
        // The host-accepted bind was adopted, published and armed as one
        // notify-ordered transition above. Keeping the arm there is what makes
        // it impossible for a newer bind/unbind to land between identity
        // publication and this exact frame boundary. Only the atomics-only
        // census is intentionally outside that spinlock.
        if let Some((ready, carried)) = bind_refresh {
            if carried {
                crate::ddi::scanout_trace::note_bind_watermark_carried();
            } else {
                crate::ddi::scanout_trace::note_bind_watermark_sampled();
            }
            crate::ddi::scanout_trace::note_bind_refresh(ready);
        }
        // ── THE OWNERSHIP EDGE (ROADMAP defect 0ab-B) ───────────────────────
        //
        // The host is now bound to this presentation, so a `RESOURCE_FLUSH`
        // issued from here on reads THIS buffer. Publishing the epoch is what
        // makes the next flush token mean something; doing it after the
        // `set_scanout_blob` round-trip (and not before) is what makes the token
        // provably cover a binding the host has actually accepted.
        //
        // A rebind to a different resource also ends every older epoch's lease:
        // QEMU's control queue is strictly FIFO, so a returned SET_SCANOUT_BLOB
        // proves every earlier flush completed and no later one can read the old
        // resource. That is the escape for a binding that never got a read of
        // its own (measured: ~350 of 4127 bind intervals) — not a substitute for
        // the read, which is the distinction 22.22.207.0 collapsed.
        //
        // Epoch and primary publication also happened inside that transition.
        // For an already-bound re-present the separate locked revalidation above
        // publishes them without minting a fictitious wire sequence. A stale
        // worker publishes neither; the newer accepted bind owns the heartbeat.
        // The caller's `interval` drops AFTER this, clearing the gate once the
        // matching physical address is published — the order the next VSync DPC
        // depends on (it acquires the gate before sampling
        // `last_primary_address`).
        return Ok(ScanoutOutcome::Programmed);
    }

    let target_image_id = adapter.dedicated_scanout_image.load(Ordering::Acquire);
    if target_image_id == 0 {
        return Err(ScanoutReject::NoTarget);
    }

    // The returned outer wire fence completes in the ring-1 GPU domain. Its DPC
    // callback—not this enqueue—marks scanout dirty and wakes the coalescing
    // async RESOURCE_FLUSH worker, so VNC never samples ahead of the copy.
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
    // 0x0E10_* = ExchangePreStartInfo. Entry was 0x0E00_0001, which device.rs
    // also records for DestroyDevice entry; the 0x0E00_* block is the
    // device-teardown family (0x0E01 owning handle, 0x0E02 blob-table size,
    // 0x0E03 reclaim counts, 0x0E04 ALLOC_BLOB owner), so this one moves out of
    // it rather than the other way round.
    crate::diag::record(0x0E10_0001);
    if pre_start_info.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    crate::diag::record(0x0E10_0002);
    STATUS_SUCCESS
}
