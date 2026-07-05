//! PASSIVE-level control-command orchestration (C3/M3.4).
//!
//! Every virtio-gpu control verb (ctx/blob/map/attach/unref) is a multi-phase
//! flow here: table phase(s) under the device spinlock ([`VirtioGpu`] helpers)
//! interleaved with a host round-trip whose WAIT happens at PASSIVE_LEVEL on a
//! stack [`SyncWaitBlock`] KEVENT — never a DISPATCH spin under the spinlock.
//! The waits use adaptive slices and re-drain the used ring on each slice, so
//! they are interrupt-driven when interrupts flow and degrade to ~ms-latency
//! polling when they do not (bring-up, lost interrupts).
//!
//! Why this exists (2026-07-04 evidence): the host processes the virtio ctrl
//! queue serially, and a venus `RESOURCE_CREATE_BLOB(blob_id)` can legally
//! block host-side waiting for the vkr ring to execute the referenced
//! `vkAllocateMemory` — so ANY control command can take seconds under a
//! validate-slow host. The old model burned a ~1 s DISPATCH spin per waiter
//! under the device spinlock and then poisoned the transport; this model waits
//! properly and, on timeout, abandons only its own in-flight slot.
//!
//! IRQL: every function in this module MUST be called at PASSIVE_LEVEL.

use core::mem::size_of;
use core::ptr::NonNull;

use alloc::vec::Vec;
use bytemuck::{bytes_of, Zeroable};
use wdk_sys::ntddk::{KeDelayExecutionThread, KeWaitForSingleObject};
use wdk_sys::{LARGE_INTEGER, PVOID, STATUS_SUCCESS};

use super::gpu::{
    BlobMapBegin, BlobMapFinish, BlobMapPrep, BlobRemapBegin, FenceWaitPrep, InFlight,
    SyncWaitBlock, CTRL_TIMEOUT_COUNT, FENCE_WAIT_TIMEOUTS, MAX_PARKED, SUBMIT_META_BYTES,
};
use super::hal::DmaBuffer;
use super::VirtioError;
use crate::adapter::AdapterContext;
use core::sync::atomic::Ordering;
use helios_protocol::{
    resp_is_ok, VirtioGpuCtrlHdr, VirtioGpuCtxCreate, VirtioGpuCtxDestroy, VirtioGpuCtxResource,
    VirtioGpuResourceCreateBlob, VirtioGpuResourceMapBlob, VirtioGpuResourceUnmapBlob,
    VirtioGpuResourceUnref, VirtioGpuRespMapInfo, VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE,
    VIRTIO_GPU_CMD_CTX_CREATE, VIRTIO_GPU_CMD_CTX_DESTROY, VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE,
    VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB, VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB,
    VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB, VIRTIO_GPU_CMD_RESOURCE_UNREF,
    VIRTIO_GPU_MAP_CACHE_MASK,
};

/// `KernelMode` (`KPROCESSOR_MODE`).
const KERNEL_MODE: i8 = 0;
/// `Executive` (`KWAIT_REASON`).
const EXECUTIVE: i32 = 0;

/// Default PASSIVE wait budget for one synchronous control round-trip. Sized
/// for a validate-slow host whose ctrl queue is momentarily blocked behind a
/// wait-for-mem-alloc blob create; beyond this the host is genuinely wedged
/// and the command fails loudly (`VirtioError::Timeout`).
const SYNC_ROUNDTRIP_TIMEOUT_MS: u64 = 30_000;
/// Backpressure retry budget when the control queue / in-flight tables are
/// full (1 ms PASSIVE sleep per retry).
const ENQUEUE_RETRY_MAX: u32 = 5_000;
/// Hard cap on a single WAIT_FENCE escape (the ICD's own forward-progress
/// deadline fires far earlier; this only bounds kernel-side thread residency).
const WAIT_FENCE_MAX_MS: u64 = 120_000;
/// Bound on waiting out another mapper's in-flight RESOURCE_MAP_BLOB.
const MAP_BUSY_RETRY_MAX: u32 = 30_000;

/// PASSIVE sleep for ~`ms` milliseconds.
pub(crate) fn sleep_ms(ms: u64) {
    let mut interval: LARGE_INTEGER = unsafe { core::mem::zeroed() };
    interval.QuadPart = -((ms.max(1) as i64) * 10_000);
    // SAFETY: PASSIVE_LEVEL relative-timeout sleep.
    let _ = unsafe { KeDelayExecutionThread(KERNEL_MODE, 0, &mut interval) };
}

/// Wait on `block` for up to `total_ms`, in adaptive slices (1 ms → 1 s),
/// opportunistically draining the used ring after each slice so a lost
/// interrupt costs only slice latency. Returns whether the block completed.
fn wait_block(adapter: &AdapterContext, block: NonNull<SyncWaitBlock>, total_ms: u64) -> bool {
    let mut waited: u64 = 0;
    let mut slice: u64 = 1;
    loop {
        // SAFETY: `block` is a live stack SyncWaitBlock owned by this call
        // chain; only the drain (under the device lock) writes it.
        if unsafe { block.as_ref() }.is_done() {
            return true;
        }
        if waited >= total_ms {
            return false;
        }
        let this_slice = slice.min(total_ms - waited);
        let mut timeout: LARGE_INTEGER = unsafe { core::mem::zeroed() };
        timeout.QuadPart = -((this_slice.max(1) as i64) * 10_000);
        // SAFETY: the KEVENT was initialized by SyncWaitBlock::init at this
        // address and outlives the wait; PASSIVE_LEVEL.
        let status = unsafe {
            KeWaitForSingleObject(
                core::ptr::addr_of_mut!((*block.as_ptr()).event) as PVOID,
                EXECUTIVE,
                KERNEL_MODE,
                0,
                &mut timeout,
            )
        };
        if status == STATUS_SUCCESS {
            return true;
        }
        waited += this_slice;
        slice = (slice * 2).min(1_000);
        // Interrupt-loss tolerance: drain whatever completed.
        let _ = adapter.with_virtio(|v| v.drain_used());
    }
}

/// Reap completed (parked) entries and free their DMA buffers at PASSIVE.
pub fn reap_parked(adapter: &AdapterContext) {
    let parked = adapter.with_virtio(|v| v.parked_len()).unwrap_or(0);
    if parked == 0 {
        return;
    }
    let fresh: Vec<InFlight> = Vec::with_capacity(MAX_PARKED);
    let dead = adapter.with_virtio(move |v| v.swap_parked(fresh));
    // Dropped here, at PASSIVE_LEVEL, outside the lock.
    drop(dead);
}

/// One synchronous control round-trip: `req` (+ optional second device-read
/// span `extra`) → device → `resp_out`. Blocks at PASSIVE until completion or
/// `timeout_ms`. On timeout the in-flight slot is abandoned (reaped when the
/// completion eventually arrives) — the transport is NOT poisoned.
fn ctrl_roundtrip(
    adapter: &AdapterContext,
    req: &[u8],
    extra: Option<&[u8]>,
    resp_out: &mut [u8],
    timeout_ms: u64,
) -> Result<(), VirtioError> {
    let in0_len = req.len();
    let in1_len = extra.map_or(0, |e| e.len());
    let resp_len = resp_out.len();
    if in0_len == 0 || resp_len == 0 {
        return Err(VirtioError::DeviceError);
    }
    reap_parked(adapter);

    let total = in0_len + in1_len + resp_len;
    let mut meta = DmaBuffer::new(total).ok_or(VirtioError::OutOfMemory)?;
    {
        let m = meta.as_mut_slice();
        m[..in0_len].copy_from_slice(req);
        if let Some(e) = extra {
            m[in0_len..in0_len + in1_len].copy_from_slice(e);
        }
    }

    let mut block = SyncWaitBlock::new_zeroed();
    // SAFETY: `block` is at its final stack address for the whole call.
    unsafe { block.init() };
    let block_ptr = NonNull::from(&mut block);

    // Enqueue, with PASSIVE backpressure while the queue is full.
    let mut meta_slot = Some(meta);
    let mut attempts = 0u32;
    let token = loop {
        let m = meta_slot.take().expect("meta returned on every retry path");
        let res = adapter.with_virtio(move |v| {
            v.drain_used();
            v.enqueue_sync(m, in0_len, in1_len, resp_len, block_ptr)
        });
        match res {
            Err(_) => return Err(VirtioError::DeviceError), // transport gone
            Ok(Ok(token)) => break token,
            Ok(Err((m_back, VirtioError::QueueFull))) => {
                meta_slot = Some(m_back);
                attempts += 1;
                if attempts > ENQUEUE_RETRY_MAX {
                    return Err(VirtioError::QueueFull);
                }
                reap_parked(adapter);
                sleep_ms(1);
            }
            Ok(Err((_m, e))) => return Err(e), // dropped here at PASSIVE
        }
    };

    if !wait_block(adapter, block_ptr, timeout_ms) {
        // Final race check + abandonment under the lock.
        let already_done = adapter
            .with_virtio(|v| {
                v.drain_used();
                v.abandon_sync(token, block_ptr)
            })
            .unwrap_or(true);
        if !already_done {
            CTRL_TIMEOUT_COUNT.fetch_add(1, Ordering::Relaxed);
            return Err(VirtioError::Timeout);
        }
    }
    block.copy_resp(resp_out);
    Ok(())
}

/// Round-trip expecting a bare `VirtioGpuCtrlHdr` response; checks RESP_OK.
fn ctrl_roundtrip_ok(
    adapter: &AdapterContext,
    req: &[u8],
    extra: Option<&[u8]>,
) -> Result<(), VirtioError> {
    let mut resp = [0u8; size_of::<VirtioGpuCtrlHdr>()];
    ctrl_roundtrip(adapter, req, extra, &mut resp, SYNC_ROUNDTRIP_TIMEOUT_MS)?;
    let resp_type = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
    if resp_is_ok(resp_type) {
        Ok(())
    } else {
        Err(VirtioError::DeviceError)
    }
}

// ── Context lifecycle ────────────────────────────────────────────────────────

/// Create a virtio-gpu 3D context bound to `capset_id` (Venus = 4) and return
/// the guest-assigned context id. `owner` is the D3D device handle recorded for
/// `DxgkDdiDestroyDevice` reclamation (0 = KMD-internal).
pub fn ctx_create(
    adapter: &AdapterContext,
    capset_id: u32,
    owner: usize,
) -> Result<u32, VirtioError> {
    let ctx_id = adapter
        .with_virtio(|v| v.alloc_ctx_id())
        .map_err(|_| VirtioError::DeviceError)?;
    let mut cmd = VirtioGpuCtxCreate::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_CTX_CREATE;
    cmd.hdr.ctx_id = ctx_id;
    // With VIRTIO_GPU_F_CONTEXT_INIT, context_init carries the capset id.
    cmd.context_init = capset_id;
    // A debug name helps host-side (virglrenderer) logs; purely cosmetic.
    const NAME: &[u8] = b"helios";
    cmd.nlen = NAME.len() as u32;
    cmd.debug_name[..NAME.len()].copy_from_slice(NAME);
    crate::diag::record(0x0D20_0000 | (ctx_id & 0xFFFF));
    ctrl_roundtrip_ok(adapter, bytes_of(&cmd), None)?;
    crate::diag::record(0x0D21_0000 | (ctx_id & 0xFFFF));
    let _ = adapter.with_virtio(|v| v.track_context(owner, ctx_id));
    Ok(ctx_id)
}

/// Destroy a context and drop its tracking slot.
pub fn ctx_destroy(adapter: &AdapterContext, ctx_id: u32) -> Result<(), VirtioError> {
    let _ = adapter.with_virtio(|v| v.untrack_context(ctx_id));
    let mut cmd = VirtioGpuCtxDestroy::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_CTX_DESTROY;
    cmd.hdr.ctx_id = ctx_id;
    ctrl_roundtrip_ok(adapter, bytes_of(&cmd), None)
}

/// `CTX_DESTROY` every context still owned by `owner` (device teardown).
pub fn destroy_contexts_for_owner(adapter: &AdapterContext, owner: usize) -> u32 {
    let mut destroyed = 0u32;
    loop {
        let taken = adapter
            .with_virtio(|v| v.take_context_for_owner(owner))
            .unwrap_or(None);
        let Some(ctx_id) = taken else {
            return destroyed;
        };
        let mut cmd = VirtioGpuCtxDestroy::zeroed();
        cmd.hdr.type_ = VIRTIO_GPU_CMD_CTX_DESTROY;
        cmd.hdr.ctx_id = ctx_id;
        let _ = ctrl_roundtrip_ok(adapter, bytes_of(&cmd), None);
        destroyed += 1;
    }
}

// ── Resource / blob lifecycle ────────────────────────────────────────────────

/// Attach a resource to a 3D context (`CTX_ATTACH_RESOURCE`).
pub fn ctx_attach_resource(
    adapter: &AdapterContext,
    ctx_id: u32,
    resource_id: u32,
) -> Result<(), VirtioError> {
    let mut cmd = VirtioGpuCtxResource::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE;
    cmd.hdr.ctx_id = ctx_id;
    cmd.resource_id = resource_id;
    ctrl_roundtrip_ok(adapter, bytes_of(&cmd), None)
}

/// Detach a resource from a 3D context.
pub fn ctx_detach_resource(
    adapter: &AdapterContext,
    ctx_id: u32,
    resource_id: u32,
) -> Result<(), VirtioError> {
    let mut cmd = VirtioGpuCtxResource::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE;
    cmd.hdr.ctx_id = ctx_id;
    cmd.resource_id = resource_id;
    ctrl_roundtrip_ok(adapter, bytes_of(&cmd), None)
}

/// Drop the host's reference to a resource.
pub fn resource_unref(adapter: &AdapterContext, resource_id: u32) -> Result<(), VirtioError> {
    let mut cmd = VirtioGpuResourceUnref::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_UNREF;
    cmd.resource_id = resource_id;
    ctrl_roundtrip_ok(adapter, bytes_of(&cmd), None)
}

/// Attach an EXISTING live resource id to a context without taking ownership
/// (the DXVK/Mesa shared-resource import path). C1: liveness is validated
/// against the KMD's authoritative table BEFORE sending — the host attach path
/// cannot be trusted to fail (`virgl_renderer_ctx_attach_resource` is void and
/// silently no-ops on an unknown resource; QEMU still replies OK_NODATA).
pub fn attach_resource_checked(
    adapter: &AdapterContext,
    ctx_id: u32,
    resource_id: u32,
) -> Result<(), VirtioError> {
    let live = adapter
        .with_virtio(|v| v.resource_is_live(resource_id))
        .map_err(|_| VirtioError::DeviceError)?;
    if !live {
        crate::diag::record(0x0E09_0000 | (resource_id & 0xFFFF));
        return Err(VirtioError::DeviceError);
    }
    ctx_attach_resource(adapter, ctx_id, resource_id)
}

/// Create a HOST3D virtio-gpu blob resource in venus context `ctx_id`,
/// referencing venus device-memory `blob_id`, and attach it to the context.
/// Returns the guest-assigned resource id. Mirrors the proven System-class
/// `kmd::alloc_blob` sequence (create_blob → ctx_attach_resource); the
/// live-resource table slot is reserved up front so an untracked-but-live
/// resource can never exist.
pub fn resource_create_blob(
    adapter: &AdapterContext,
    ctx_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
) -> Result<u32, VirtioError> {
    let reserved = adapter
        .with_virtio(|v| v.reserve_resource_slot())
        .map_err(|_| VirtioError::DeviceError)?;
    if !reserved {
        return Err(VirtioError::OutOfMemory);
    }
    let resource_id = match adapter.with_virtio(|v| v.alloc_resource_id()) {
        Ok(id) => id,
        Err(_) => {
            let _ = adapter.with_virtio(|v| v.cancel_resource_reservation());
            return Err(VirtioError::DeviceError);
        }
    };
    let mut cmd = VirtioGpuResourceCreateBlob::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB;
    cmd.hdr.ctx_id = ctx_id;
    cmd.resource_id = resource_id;
    cmd.blob_mem = blob_mem;
    cmd.blob_flags = blob_flags;
    cmd.nr_entries = 0;
    cmd.blob_id = blob_id;
    cmd.size = size;
    if let Err(e) = ctrl_roundtrip_ok(adapter, bytes_of(&cmd), None) {
        let _ = adapter.with_virtio(|v| v.cancel_resource_reservation());
        return Err(e);
    }
    if let Err(e) = ctx_attach_resource(adapter, ctx_id, resource_id) {
        // The resource exists host-side but could not attach: drop it so it
        // does not leak untracked.
        let _ = resource_unref(adapter, resource_id);
        let _ = adapter.with_virtio(|v| v.cancel_resource_reservation());
        return Err(e);
    }
    let _ = adapter.with_virtio(|v| v.commit_resource(resource_id));
    Ok(resource_id)
}

/// `HELIOS_ESCAPE_ALLOC_BLOB` — create a HOST3D blob (create + attach) and
/// record it in the blob table. Returns the resource id.
pub fn alloc_blob(
    adapter: &AdapterContext,
    ctx_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
    owner: usize,
) -> Result<u32, VirtioError> {
    if size == 0 {
        return Err(VirtioError::DeviceError);
    }
    let reserved = adapter
        .with_virtio(|v| v.reserve_blob_slot())
        .map_err(|_| VirtioError::DeviceError)?;
    if !reserved {
        return Err(VirtioError::OutOfMemory);
    }
    match resource_create_blob(adapter, ctx_id, blob_mem, blob_flags, blob_id, size) {
        Ok(resource_id) => {
            let _ = adapter.with_virtio(|v| v.commit_blob(owner, ctx_id, resource_id, size));
            Ok(resource_id)
        }
        Err(e) => {
            let _ = adapter.with_virtio(|v| v.cancel_blob_reservation());
            Err(e)
        }
    }
}

/// `RESOURCE_MAP_BLOB` round-trip; returns the host caching nibble.
fn resource_map_blob_roundtrip(
    adapter: &AdapterContext,
    resource_id: u32,
    offset: u64,
) -> Result<u32, VirtioError> {
    let mut cmd = VirtioGpuResourceMapBlob::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB;
    cmd.resource_id = resource_id;
    cmd.offset = offset;
    let mut resp = [0u8; size_of::<VirtioGpuRespMapInfo>()];
    ctrl_roundtrip(
        adapter,
        bytes_of(&cmd),
        None,
        &mut resp,
        SYNC_ROUNDTRIP_TIMEOUT_MS,
    )?;
    let resp_type = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
    if !resp_is_ok(resp_type) {
        return Err(VirtioError::DeviceError);
    }
    // VirtioGpuRespMapInfo = { hdr: VirtioGpuCtrlHdr, map_info: u32, .. }.
    let off = size_of::<VirtioGpuCtrlHdr>();
    let map_info = u32::from_le_bytes([resp[off], resp[off + 1], resp[off + 2], resp[off + 3]]);
    Ok(map_info & VIRTIO_GPU_MAP_CACHE_MASK)
}

/// Tear down a blob's host-visible mapping.
pub fn resource_unmap_blob(adapter: &AdapterContext, resource_id: u32) -> Result<(), VirtioError> {
    let mut cmd = VirtioGpuResourceUnmapBlob::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB;
    cmd.resource_id = resource_id;
    ctrl_roundtrip_ok(adapter, bytes_of(&cmd), None)
}

/// Map a blob into the host-visible window (idempotent — returns the existing
/// mapping if present). `owner = Some(o)` is the owner-scoped escape path;
/// `None` resolves by resource id alone (the GDI executor / kernel path).
pub fn map_blob_prepare(
    adapter: &AdapterContext,
    owner: Option<usize>,
    resource_id: u32,
) -> Result<BlobMapPrep, VirtioError> {
    let mut busy_retries = 0u32;
    loop {
        let begin = adapter
            .with_virtio(|v| v.blob_map_begin(owner, resource_id))
            .map_err(|_| VirtioError::DeviceError)?;
        match begin {
            BlobMapBegin::Mapped(prep) => return Ok(prep),
            BlobMapBegin::Failed(e) => return Err(e),
            BlobMapBegin::Busy => {
                busy_retries += 1;
                if busy_retries > MAP_BUSY_RETRY_MAX {
                    return Err(VirtioError::Timeout);
                }
                sleep_ms(1);
            }
            BlobMapBegin::Start { offset, len } => {
                let cache = resource_map_blob_roundtrip(adapter, resource_id, offset);
                let cache_ok = cache.as_ref().ok().copied();
                let fin = adapter
                    .with_virtio(|v| v.blob_map_finish(resource_id, offset, len, cache_ok))
                    .map_err(|_| VirtioError::DeviceError)?;
                return match fin {
                    BlobMapFinish::Done(prep) => Ok(prep),
                    BlobMapFinish::HostRejected => {
                        Err(cache.err().unwrap_or(VirtioError::DeviceError))
                    }
                    BlobMapFinish::SlotGone => {
                        // Owner teardown raced the map: undo the host mapping
                        // and return the reserved range.
                        let _ = resource_unmap_blob(adapter, resource_id);
                        let _ =
                            adapter.with_virtio(|v| v.free_window_range_pub(offset, len));
                        Err(VirtioError::DeviceError)
                    }
                };
            }
        }
    }
}

/// Map a blob at the FIXED window offset VidMm assigned (the CPU-visible BAR
/// memory segment, `build_paging_buffer.rs`). Inverts the normal order: instead
/// of the KMD allocator picking the offset, the blob is placed where VidMm put
/// the allocation, so CPU raster (through the segment's CpuTranslatedAddress),
/// the GDI executor, and the host all address the same bytes.
///
/// A pre-existing mapping at another offset is torn down first (blob content is
/// intrinsic to the host memory object — a remap is content-preserving), and
/// any STALE other-blob mapping overlapping the target range (an eviction this
/// driver missed) is unmapped so host window subregions never overlap.
/// PASSIVE_LEVEL only (host round-trips).
pub fn map_blob_at(
    adapter: &AdapterContext,
    resource_id: u32,
    window_offset: u64,
) -> Result<BlobMapPrep, VirtioError> {
    // Evict stale overlapping placements before reserving our own slot.
    let (_, blob_size, _) = adapter
        .with_virtio(|v| v.blob_lookup(resource_id))
        .map_err(|_| VirtioError::DeviceError)?
        .ok_or(VirtioError::DeviceError)?;
    let map_len = blob_size.saturating_add(4095) & !4095;
    let mut stale = [0u32; 8];
    let n = adapter
        .with_virtio(|v| v.blobs_overlapping(window_offset, map_len, resource_id, &mut stale))
        .unwrap_or(0);
    for &res in stale[..n].iter() {
        let _ = resource_unmap_blob(adapter, res);
        let _ = adapter.with_virtio(|v| v.blob_note_unmapped(res));
    }

    let mut busy_retries = 0u32;
    loop {
        let begin = adapter
            .with_virtio(|v| v.blob_remap_begin(resource_id, window_offset))
            .map_err(|_| VirtioError::DeviceError)?;
        match begin {
            BlobRemapBegin::Mapped(prep) => return Ok(prep),
            BlobRemapBegin::Failed(e) => return Err(e),
            BlobRemapBegin::Busy => {
                busy_retries += 1;
                if busy_retries > MAP_BUSY_RETRY_MAX {
                    return Err(VirtioError::Timeout);
                }
                sleep_ms(1);
            }
            BlobRemapBegin::Start { old, len } => {
                if let Some((old_offset, old_len)) = old {
                    // Content-preserving move: unmap the previous placement and
                    // (for KMD-partition offsets only — the free guard ignores
                    // VidMm-partition ones) return its range.
                    let _ = resource_unmap_blob(adapter, resource_id);
                    let _ = adapter
                        .with_virtio(|v| v.free_window_range_pub(old_offset, old_len));
                }
                let cache = resource_map_blob_roundtrip(adapter, resource_id, window_offset);
                let cache_ok = cache.as_ref().ok().copied();
                let fin = adapter
                    .with_virtio(|v| v.blob_map_finish(resource_id, window_offset, len, cache_ok))
                    .map_err(|_| VirtioError::DeviceError)?;
                return match fin {
                    BlobMapFinish::Done(prep) => Ok(prep),
                    BlobMapFinish::HostRejected => {
                        Err(cache.err().unwrap_or(VirtioError::DeviceError))
                    }
                    BlobMapFinish::SlotGone => {
                        let _ = resource_unmap_blob(adapter, resource_id);
                        Err(VirtioError::DeviceError)
                    }
                };
            }
        }
    }
}

/// `HELIOS_ESCAPE_RELEASE_BLOB` — unmap (if mapped) + detach + unref a blob and
/// drop its tracking slot, returning its window range to the free list.
pub fn release_blob_for_owner(
    adapter: &AdapterContext,
    owner: usize,
    ctx_id: u32,
    resource_id: u32,
) -> Result<(), VirtioError> {
    let taken = adapter
        .with_virtio(|v| v.take_blob_matching(owner, ctx_id, resource_id))
        .map_err(|_| VirtioError::DeviceError)?;
    let Some((res, mapped, map_offset, map_len)) = taken else {
        return Ok(());
    };
    if mapped {
        let _ = resource_unmap_blob(adapter, res);
        let _ = adapter.with_virtio(|v| v.free_window_range_pub(map_offset, map_len));
    }
    let first_teardown = adapter
        .with_virtio(|v| v.take_live_resource(res))
        .unwrap_or(false);
    if first_teardown {
        let _ = ctx_detach_resource(adapter, ctx_id, res);
        resource_unref(adapter, res)
    } else {
        Ok(())
    }
}

/// Reclaim every blob still owned by `owner` (a destroyed D3D device handle):
/// unmap (if mapped), detach, unref, and return the window range. KMD-side
/// safety net for an ICD that crashes or skips RELEASE_BLOB. Returns the count.
pub fn release_blobs_for_owner(adapter: &AdapterContext, owner: usize) -> u32 {
    let mut reclaimed = 0u32;
    loop {
        let taken = adapter
            .with_virtio(|v| v.take_blob_for_owner(owner))
            .unwrap_or(None);
        let Some((ctx_id, res, mapped, map_offset, map_len)) = taken else {
            return reclaimed;
        };
        if mapped {
            let _ = resource_unmap_blob(adapter, res);
            let _ = adapter.with_virtio(|v| v.free_window_range_pub(map_offset, map_len));
        }
        let first_teardown = adapter
            .with_virtio(|v| v.take_live_resource(res))
            .unwrap_or(false);
        if first_teardown {
            let _ = ctx_detach_resource(adapter, ctx_id, res);
            let _ = resource_unref(adapter, res);
        }
        reclaimed += 1;
    }
}

/// Drop the KMD-internal (owner-0) blob slot for an allocation at
/// DestroyAllocation time, unmapping the window mapping the GDI executor may
/// have opened. Returns `true` if a live mapping was unmapped here (the caller
/// must not send a second host unmap for the same resource).
pub fn forget_allocation_blob(adapter: &AdapterContext, resource_id: u32) -> bool {
    let taken = adapter
        .with_virtio(|v| v.forget_allocation_blob(resource_id))
        .unwrap_or(None);
    let Some((mapped, map_offset, map_len)) = taken else {
        return false;
    };
    if mapped {
        let _ = resource_unmap_blob(adapter, resource_id);
        let _ = adapter.with_virtio(|v| v.free_window_range_pub(map_offset, map_len));
        return true;
    }
    false
}

// ── Venus submission ─────────────────────────────────────────────────────────

/// SYNCHRONOUS venus SUBMIT_3D (in-kernel venus client's direct commands —
/// small ring-bootstrap/notify streams). Blocks at PASSIVE until the device
/// acks the command on the used ring (decode-level; the client's real waits
/// are its ring-head polls). `fence_id` stays 0 (parity with the proven
/// System-class `submit_direct` shape).
pub fn submit_venus_sync(
    adapter: &AdapterContext,
    ctx_id: u32,
    stream: &[u8],
) -> Result<(), VirtioError> {
    if stream.is_empty() {
        return Err(VirtioError::DeviceError);
    }
    let mut cmd = helios_protocol::VirtioGpuCmdSubmit::zeroed();
    cmd.hdr.type_ = helios_protocol::VIRTIO_GPU_CMD_SUBMIT_3D;
    cmd.hdr.flags = helios_protocol::VIRTIO_GPU_FLAG_FENCE;
    cmd.hdr.ctx_id = ctx_id;
    cmd.size = stream.len() as u32;
    // The stream rides a second device-read descriptor (kept split so the host
    // never mis-parses the submit header as another control command).
    ctrl_roundtrip_ok(adapter, bytes_of(&cmd), Some(stream))
}

/// ASYNC venus SUBMIT_3D (the ICD escape path): stage the stream into DMA
/// buffers, enqueue fenced with a fresh KMD wire fence id, and return that id
/// at QUEUE time. Completion is observed via [`wait_fence`].
pub fn submit_venus_async(
    adapter: &AdapterContext,
    ctx_id: u32,
    ring_idx: u32,
    stream: &[u8],
) -> Result<u64, VirtioError> {
    if stream.is_empty() {
        return Err(VirtioError::DeviceError);
    }
    reap_parked(adapter);
    let meta = DmaBuffer::new(SUBMIT_META_BYTES).ok_or(VirtioError::OutOfMemory)?;
    let mut venus = DmaBuffer::new(stream.len()).ok_or(VirtioError::OutOfMemory)?;
    venus.as_mut_slice()[..stream.len()].copy_from_slice(stream);
    let venus_len = stream.len();

    let mut meta_slot = Some(meta);
    let mut venus_slot = Some(venus);
    let mut attempts = 0u32;
    loop {
        let m = meta_slot.take().expect("meta returned on every retry path");
        let vn = venus_slot.take().expect("venus returned on every retry path");
        let res = adapter.with_virtio(move |v| {
            v.drain_used();
            v.enqueue_async_submit(ctx_id, ring_idx, m, vn, venus_len)
        });
        match res {
            Err(_) => return Err(VirtioError::DeviceError), // transport gone
            Ok(Ok(fence_id)) => return Ok(fence_id),
            Ok(Err((m_back, v_back, VirtioError::QueueFull))) => {
                meta_slot = Some(m_back);
                venus_slot = Some(v_back);
                attempts += 1;
                if attempts > ENQUEUE_RETRY_MAX {
                    return Err(VirtioError::QueueFull);
                }
                reap_parked(adapter);
                sleep_ms(1);
            }
            Ok(Err((_m, _v, e))) => return Err(e), // buffers dropped at PASSIVE
        }
    }
}

/// Outcome of a [`wait_fence`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitFenceOutcome {
    /// The wire fence has completed (host-visible-complete).
    Complete,
    /// `timeout_ns` elapsed first (or this was a poll and it is still pending).
    TimedOut,
    /// The id was never assigned / the transport is gone.
    Invalid,
}

/// Wait (PASSIVE, KEVENT) until wire fence `fence_id` completes or
/// `timeout_ns` elapses. `timeout_ns == 0` is a poll.
pub fn wait_fence(adapter: &AdapterContext, fence_id: u64, timeout_ns: u64) -> WaitFenceOutcome {
    let mut block = SyncWaitBlock::new_zeroed();
    // SAFETY: `block` is at its final stack address for the whole call.
    unsafe { block.init() };
    let block_ptr = NonNull::from(&mut block);

    let mut full_retries = 0u32;
    loop {
        let prep = adapter.with_virtio(|v| {
            v.drain_used();
            v.fence_wait_prepare(fence_id, block_ptr)
        });
        match prep {
            Err(_) => return WaitFenceOutcome::Invalid, // transport gone
            Ok(FenceWaitPrep::Complete) => return WaitFenceOutcome::Complete,
            Ok(FenceWaitPrep::Invalid) => return WaitFenceOutcome::Invalid,
            Ok(FenceWaitPrep::TableFull) => {
                full_retries += 1;
                if full_retries > 1_000 {
                    FENCE_WAIT_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
                    return WaitFenceOutcome::TimedOut;
                }
                sleep_ms(1);
            }
            Ok(FenceWaitPrep::Registered) => break,
        }
    }

    if timeout_ns == 0 {
        // Poll: deregister immediately; completion may still have raced in.
        let completed = adapter
            .with_virtio(|v| v.fence_wait_cancel(block_ptr))
            .unwrap_or(true);
        return if completed {
            WaitFenceOutcome::Complete
        } else {
            WaitFenceOutcome::TimedOut
        };
    }

    let total_ms = (timeout_ns / 1_000_000).max(1).min(WAIT_FENCE_MAX_MS);
    if wait_block(adapter, block_ptr, total_ms) {
        return WaitFenceOutcome::Complete;
    }
    let completed = adapter
        .with_virtio(|v| {
            v.drain_used();
            v.fence_wait_cancel(block_ptr)
        })
        .unwrap_or(true);
    if completed {
        WaitFenceOutcome::Complete
    } else {
        FENCE_WAIT_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
        WaitFenceOutcome::TimedOut
    }
}
