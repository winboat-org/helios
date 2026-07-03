//! `DxgkDdiEscape` — out-of-band ICD → KMD channel (Phase 3, M3.3).
//!
//! The user-mode Vulkan ICD reaches the KMD through `D3DKMTEscape`, not through
//! the WDDM command/GPU-VA path. Every escape buffer begins with a
//! [`HeliosEscapeHeader`] (`helios_protocol::escape`); we validate it and
//! dispatch the Venus control verbs (CTX_CREATE / SUBMIT_VENUS / CTX_DESTROY,
//! plus a trivial WAIT_FENCE for the interim synchronous fence model). Blob verbs
//! (ALLOC_BLOB / MAP_BLOB) arrive in M3.5.
//!
//! TRUST BOUNDARY: `pPrivateDriverData` is guest-supplied. We treat
//! `PrivateDriverDataSize` as the only authoritative length and bounds-check
//! every struct size and embedded offset against it before reading, and we read
//! with `pod_read_unaligned` because the buffer carries no alignment guarantee.

use core::ffi::c_void;
use core::mem::size_of;

use bytemuck::{bytes_of, pod_read_unaligned};
use helios_protocol::{
    HeliosEscapeAllocBlob, HeliosEscapeAttachResource, HeliosEscapeCtxCreate,
    HeliosEscapeCtxDestroy, HeliosEscapeHeader, HeliosEscapeMapBlob, HeliosEscapeQueryStats,
    HeliosEscapeReleaseBlob, HeliosEscapeSubmitVenus, HELIOS_ESCAPE_ALLOC_BLOB,
    HELIOS_ESCAPE_ATTACH_RESOURCE, HELIOS_ESCAPE_CTX_CREATE, HELIOS_ESCAPE_CTX_DESTROY,
    HELIOS_ESCAPE_MAP_BLOB, HELIOS_ESCAPE_QUERY_STATS, HELIOS_ESCAPE_RELEASE_BLOB,
    HELIOS_ESCAPE_SUBMIT_VENUS, HELIOS_ESCAPE_WAIT_FENCE,
};

use super::blob_map::{
    effective_map_cache, map_cache_to_mm, map_io_pages_to_user, unmap_io_pages_from_user,
};
use crate::adapter::AdapterContext;
use crate::dxgk::*;
use crate::virtio::hal::DmaBuffer;

pub unsafe extern "C" fn dxgkddi_escape(
    h_adapter: *mut c_void,
    escape: *const DXGKARG_ESCAPE,
) -> NTSTATUS {
    if h_adapter.is_null() || escape.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: Dxgkrnl passes our adapter context and a valid (const) args struct.
    // We only read fields of `args`; we write only through the buffer it points to.
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let args = unsafe { &*escape };

    let buf_ptr = args.pPrivateDriverData as *mut u8;
    let buf_len = args.PrivateDriverDataSize as usize;
    if buf_ptr.is_null() || buf_len < size_of::<HeliosEscapeHeader>() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: Dxgkrnl guarantees `buf_len` bytes are accessible at `buf_ptr`. This
    // is the trust boundary; every read below is bounds-checked against buf_len.
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr, buf_len) };

    let hdr: HeliosEscapeHeader = pod_read_unaligned(&buf[..size_of::<HeliosEscapeHeader>()]);
    // Reject bad magic/version, and any header that claims to be larger than the
    // buffer the runtime actually gave us.
    if !hdr.is_valid() || hdr.size as usize > buf_len {
        return STATUS_INVALID_PARAMETER;
    }

    // Owner token for blob mappings: dxgkrnl passes our DeviceContext handle (the
    // one we returned from DxgkDdiCreateDevice) as `hDevice`, and hands the SAME
    // handle to DxgkDdiDestroyDevice — so a mapping tagged with it is unmapped at
    // the right time, in the creating process. Blob verbs require a device handle.
    let owner = args.hDevice as usize;

    match hdr.cmd_type {
        HELIOS_ESCAPE_CTX_CREATE => escape_ctx_create(adapter, buf, owner),
        HELIOS_ESCAPE_CTX_DESTROY => escape_ctx_destroy(adapter, buf),
        HELIOS_ESCAPE_SUBMIT_VENUS => escape_submit_venus(adapter, buf),
        HELIOS_ESCAPE_WAIT_FENCE => escape_wait_fence(buf),
        HELIOS_ESCAPE_ALLOC_BLOB => escape_alloc_blob(adapter, buf, owner),
        HELIOS_ESCAPE_MAP_BLOB => escape_map_blob(adapter, buf, owner),
        HELIOS_ESCAPE_RELEASE_BLOB => escape_release_blob(adapter, buf, owner),
        HELIOS_ESCAPE_ATTACH_RESOURCE => escape_attach_resource(adapter, buf),
        HELIOS_ESCAPE_QUERY_STATS => escape_query_stats(adapter, buf),
        // Unknown verbs are rejected.
        _ => STATUS_NOT_IMPLEMENTED,
    }
}

/// `HELIOS_ESCAPE_QUERY_STATS` → read-only snapshot of the bounded resource
/// tables (occupancy under the device lock) and the DISPATCH-safe rejection /
/// high-water counters. Diagnostic observability for the 2026-07-03 blob-table
/// exhaustion class; no device state is modified.
fn escape_query_stats(adapter: &AdapterContext, buf: &mut [u8]) -> NTSTATUS {
    use core::sync::atomic::Ordering;

    use crate::virtio::gpu::{
        ADOPT_DEAD_REJECTS, BLOB_FULL_REJECTS, BLOB_HIGH_WATER, CONTEXT_FULL_DROPS,
        CTRL_TIMEOUT_COUNT, RESOURCE_FULL_REJECTS, RESOURCE_HIGH_WATER, TAKE_LIVE_MISSES,
        WINDOW_RANGE_DROPS,
    };

    let sz = size_of::<HeliosEscapeQueryStats>();
    if buf.len() < sz {
        return STATUS_BUFFER_TOO_SMALL;
    }
    let stats = match adapter.with_virtio(|v| v.table_stats()) {
        Ok(s) => s,
        Err(de) => return de.into(),
    };
    let mut out: HeliosEscapeQueryStats = pod_read_unaligned(&buf[..sz]);
    out.out_window_used = stats.window_used;
    out.out_window_len = stats.window_len;
    out.out_blobs_live = stats.blobs_live;
    out.out_blobs_cap = crate::virtio::gpu::max_blobs() as u32;
    out.out_blobs_high_water = BLOB_HIGH_WATER.load(Ordering::Relaxed);
    out.out_blob_full_rejects = BLOB_FULL_REJECTS.load(Ordering::Relaxed);
    out.out_resources_live = stats.resources_live;
    out.out_resources_cap = crate::virtio::gpu::max_resources() as u32;
    out.out_resources_high_water = RESOURCE_HIGH_WATER.load(Ordering::Relaxed);
    out.out_resource_full_rejects = RESOURCE_FULL_REJECTS.load(Ordering::Relaxed);
    out.out_contexts_live = stats.contexts_live;
    out.out_context_full_drops = CONTEXT_FULL_DROPS.load(Ordering::Relaxed);
    out.out_window_range_drops = WINDOW_RANGE_DROPS.load(Ordering::Relaxed);
    out.out_ctrl_timeouts = CTRL_TIMEOUT_COUNT.load(Ordering::Relaxed);
    out.out_take_live_misses = TAKE_LIVE_MISSES.load(Ordering::Relaxed);
    out.out_adopt_dead_rejects = ADOPT_DEAD_REJECTS.load(Ordering::Relaxed);
    buf[..sz].copy_from_slice(bytes_of(&out));
    STATUS_SUCCESS
}

/// `HELIOS_ESCAPE_CTX_CREATE` → create a Venus virtio-gpu context; write the
/// guest-assigned id back into the in/out buffer's `out_ctx_id`.
fn escape_ctx_create(adapter: &AdapterContext, buf: &mut [u8], owner: usize) -> NTSTATUS {
    let sz = size_of::<HeliosEscapeCtxCreate>();
    if buf.len() < sz {
        return STATUS_BUFFER_TOO_SMALL;
    }
    let req: HeliosEscapeCtxCreate = pod_read_unaligned(&buf[..sz]);
    match adapter.with_virtio(|v| v.ctx_create(req.capset_id, owner)) {
        Ok(Ok(ctx_id)) => {
            let mut out = req;
            out.out_ctx_id = ctx_id;
            buf[..sz].copy_from_slice(bytes_of(&out));
            STATUS_SUCCESS
        }
        Ok(Err(ve)) => ve.into(),
        Err(de) => de.into(),
    }
}

/// `HELIOS_ESCAPE_CTX_DESTROY` → tear down a context.
fn escape_ctx_destroy(adapter: &AdapterContext, buf: &mut [u8]) -> NTSTATUS {
    let sz = size_of::<HeliosEscapeCtxDestroy>();
    if buf.len() < sz {
        return STATUS_BUFFER_TOO_SMALL;
    }
    let req: HeliosEscapeCtxDestroy = pod_read_unaligned(&buf[..sz]);
    match adapter.with_virtio(|v| v.ctx_destroy(req.ctx_id)) {
        Ok(Ok(())) => STATUS_SUCCESS,
        Ok(Err(ve)) => ve.into(),
        Err(de) => de.into(),
    }
}

/// `HELIOS_ESCAPE_SUBMIT_VENUS` → forward an opaque Venus command stream to the
/// host. The stream is the `buffer_size` bytes immediately following the 32-byte
/// payload header; we stage it into a contiguous DMA buffer (at PASSIVE_LEVEL,
/// before taking the queue lock) and submit it fenced.
fn escape_submit_venus(adapter: &AdapterContext, buf: &mut [u8]) -> NTSTATUS {
    let hsz = size_of::<HeliosEscapeSubmitVenus>();
    if buf.len() < hsz {
        return STATUS_BUFFER_TOO_SMALL;
    }
    let req: HeliosEscapeSubmitVenus = pod_read_unaligned(&buf[..hsz]);

    // TRUST BOUNDARY: the Venus stream occupies [hsz .. hsz + buffer_size]. Reject
    // empty payloads and any length that overflows or exceeds the buffer.
    let payload = req.buffer_size as usize;
    if payload == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let end = match hsz.checked_add(payload) {
        Some(e) if e <= buf.len() => e,
        _ => return STATUS_INVALID_PARAMETER,
    };
    crate::diag::record(0x0D00_0000 | ((req.buffer_size as u32) & 0xFFFF));
    crate::diag::record(0x0D01_0000 | ((buf.len() as u32) & 0xFFFF));
    if payload >= 4 {
        let w0 = u32::from_le_bytes([buf[hsz], buf[hsz + 1], buf[hsz + 2], buf[hsz + 3]]);
        crate::diag::record(0x0D02_0000 | (w0 & 0xFFFF));
    }
    if payload >= 8 {
        let w1 = u32::from_le_bytes([buf[hsz + 4], buf[hsz + 5], buf[hsz + 6], buf[hsz + 7]]);
        crate::diag::record(0x0D03_0000 | (w1 & 0xFFFF));
    }

    // Copy the stream into device-visible contiguous memory (PASSIVE_LEVEL).
    let mut dma = match DmaBuffer::new(payload) {
        Some(d) => d,
        None => return STATUS_INSUFFICIENT_RESOURCES,
    };
    dma.as_mut_slice().copy_from_slice(&buf[hsz..end]);

    let (ctx_id, fence_id, ring_idx) = (req.ctx_id, req.fence_id, req.ring_idx);
    match adapter.with_virtio(|v| v.submit_venus(ctx_id, fence_id, ring_idx, dma.as_slice())) {
        Ok(Ok(())) => STATUS_SUCCESS,
        Ok(Err(ve)) => ve.into(),
        Err(de) => de.into(),
    }
    // `dma` drops here, at PASSIVE_LEVEL, after the lock has been released.
}

/// `HELIOS_ESCAPE_WAIT_FENCE` → interim synchronous fence model.
///
/// `submit_venus` blocks on the used ring until the device acknowledges the
/// fenced command, so any fence the ICD asks to wait on has already completed by
/// the time SUBMIT_VENUS returned. We only validate the request shape and report
/// success; the real KEVENT-backed wait arrives with async submission in M3.4.
fn escape_wait_fence(buf: &[u8]) -> NTSTATUS {
    if buf.len() < size_of::<helios_protocol::HeliosEscapeWaitFence>() {
        return STATUS_BUFFER_TOO_SMALL;
    }
    STATUS_SUCCESS
}

/// `HELIOS_ESCAPE_ALLOC_BLOB` → create a HOST3D virtio-gpu blob (create + attach)
/// and record its size; write the guest-assigned `out_resource_id` back.
fn escape_alloc_blob(adapter: &AdapterContext, buf: &mut [u8], owner: usize) -> NTSTATUS {
    let sz = size_of::<HeliosEscapeAllocBlob>();
    if buf.len() < sz {
        return STATUS_BUFFER_TOO_SMALL;
    }
    let req: HeliosEscapeAllocBlob = pod_read_unaligned(&buf[..sz]);
    // DIAG: 0x0E04_HHHH = ALLOC_BLOB's owning handle (low 16 bits), to confirm it
    // matches the handle DxgkDdiDestroyDevice reclaims under (0x0E01_HHHH).
    crate::diag::record(0x0E04_0000 | ((owner as u32) & 0xFFFF));
    match adapter.with_virtio(|v| {
        v.alloc_blob(
            req.ctx_id,
            req.blob_mem,
            req.blob_flags,
            req.blob_id,
            req.size,
            owner,
        )
    }) {
        Ok(Ok(resource_id)) => {
            let mut out = req;
            out.out_resource_id = resource_id;
            buf[..sz].copy_from_slice(bytes_of(&out));
            STATUS_SUCCESS
        }
        Ok(Err(ve)) => ve.into(),
        Err(de) => de.into(),
    }
}

/// `HELIOS_ESCAPE_MAP_BLOB` → map a host-visible blob's pages into the calling
/// process and return the user VA (the zero-copy BAR model). Two-phase like the
/// System-class IOCTL: `map_blob_prepare` runs the `RESOURCE_MAP_BLOB` round-trip
/// under the virtio spinlock (DISPATCH), then we build the MDL + map into user
/// space at PASSIVE_LEVEL, in this thread's (the ICD's) process. The mapping is
/// tagged with the owning device handle and unmapped at DxgkDdiDestroyDevice.
fn escape_map_blob(adapter: &AdapterContext, buf: &mut [u8], owner: usize) -> NTSTATUS {
    let sz = size_of::<HeliosEscapeMapBlob>();
    if buf.len() < sz {
        return STATUS_BUFFER_TOO_SMALL;
    }
    let req: HeliosEscapeMapBlob = pod_read_unaligned(&buf[..sz]);
    if owner == 0 || req.resource_id == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    // Reject a second map of an already-mapped resource (would claim a second
    // window offset + leave a duplicate host mapping). The ICD maps each blob once.
    if adapter.mappings.contains(owner, req.resource_id) {
        return STATUS_INVALID_DEVICE_REQUEST;
    }

    // Phase 1 — under the virtio spinlock (DISPATCH): RESOURCE_MAP_BLOB at a fresh
    // window offset; returns the guest-physical range + host caching.
    let prep = match adapter.with_virtio(|v| v.map_blob_prepare_for_owner(owner, req.resource_id)) {
        Ok(Ok(p)) => p,
        Ok(Err(ve)) => return ve.into(),
        Err(de) => return de.into(),
    };
    // `IoAllocateMdl` length is a ULONG (u32); the per-map cap (gpu.rs) bounds this.
    if prep.size == 0 || prep.size > u32::MAX as u64 {
        return STATUS_INVALID_PARAMETER;
    }

    // Phase 2 — at PASSIVE_LEVEL, in the caller's process, holding NO lock.
    let eff_cache = effective_map_cache(req.map_cache, prep.map_cache);
    let cache = map_cache_to_mm(eff_cache);
    // SAFETY: PASSIVE_LEVEL Escape in the ICD's process; `prep` names a valid
    // host-injected window range from RESOURCE_MAP_BLOB.
    let (user_va, mdl) = match unsafe { map_io_pages_to_user(prep.gpa, prep.size, cache) } {
        Some(x) => x,
        None => return STATUS_INSUFFICIENT_RESOURCES,
    };

    // Phase 3 — record for handle-close teardown. Table full → undo immediately.
    if !adapter
        .mappings
        .insert(owner, req.resource_id, user_va, mdl as usize)
    {
        // SAFETY: still in the owning process at PASSIVE; pair returned just above.
        unsafe { unmap_io_pages_from_user(user_va, mdl) };
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    let mut out = req;
    out.out_user_va = user_va;
    out.map_cache = eff_cache;
    buf[..sz].copy_from_slice(bytes_of(&out));
    STATUS_SUCCESS
}

/// `HELIOS_ESCAPE_RELEASE_BLOB` → unmap this device's user view (if any), then
/// detach + unref the blob. Symmetric to MAP_BLOB; runs in the owning process.
fn escape_release_blob(adapter: &AdapterContext, buf: &[u8], owner: usize) -> NTSTATUS {
    let sz = size_of::<HeliosEscapeReleaseBlob>();
    if buf.len() < sz {
        return STATUS_BUFFER_TOO_SMALL;
    }
    let req: HeliosEscapeReleaseBlob = pod_read_unaligned(&buf[..sz]);
    if req.ctx_id == 0 || req.resource_id == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    // The user VA can only be unmapped in the process/device that created it.
    if let Some((user_va, mdl)) = adapter.mappings.take_for_resource(owner, req.resource_id) {
        // SAFETY: PASSIVE, in the creating process; pair from a prior MAP_BLOB.
        unsafe { unmap_io_pages_from_user(user_va, mdl as wdk_sys::PMDL) };
    }
    match adapter.with_virtio(|v| v.release_blob_for_owner(owner, req.ctx_id, req.resource_id)) {
        Ok(Ok(())) => STATUS_SUCCESS,
        Ok(Err(ve)) => ve.into(),
        Err(de) => de.into(),
    }
}

/// `HELIOS_ESCAPE_ATTACH_RESOURCE` → attach a live resource id to a Venus context
/// without taking ownership. Used by the DXVK/Mesa shared-resource import path:
/// the resource was created by another device/context and must be visible in the
/// importing context before `VkImportMemoryResourceInfoMESA` reaches virglrenderer.
fn escape_attach_resource(adapter: &AdapterContext, buf: &[u8]) -> NTSTATUS {
    let sz = size_of::<HeliosEscapeAttachResource>();
    if buf.len() < sz {
        return STATUS_BUFFER_TOO_SMALL;
    }
    let req: HeliosEscapeAttachResource = pod_read_unaligned(&buf[..sz]);
    if req.ctx_id == 0 || req.resource_id == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    // C1: validate liveness against the KMD's authoritative resource table
    // BEFORE sending the attach. The host path cannot be trusted to fail:
    // `virgl_renderer_ctx_attach_resource` is void and silently no-ops on an
    // unknown resource (QEMU still replies OK_NODATA), so without this check an
    // attach of a dead resid "succeeds" and the importer's next
    // `vkAllocateMemory` poisons its whole venus ring (host `invalid res_id`
    // → CS error → fatal decoder state — the boot-#3 dwm kill).
    match adapter.with_virtio(|v| {
        if !v.resource_is_live(req.resource_id) {
            crate::diag::record(0x0E09_0000 | (req.resource_id & 0xFFFF));
            return Err(crate::virtio::VirtioError::DeviceError);
        }
        v.ctx_attach_resource(req.ctx_id, req.resource_id)
    }) {
        Ok(Ok(())) => STATUS_SUCCESS,
        Ok(Err(ve)) => ve.into(),
        Err(de) => de.into(),
    }
}
