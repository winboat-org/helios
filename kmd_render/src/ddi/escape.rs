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
    HeliosEscapeCtxCreate, HeliosEscapeCtxDestroy, HeliosEscapeHeader, HeliosEscapeSubmitVenus,
    HELIOS_ESCAPE_CTX_CREATE, HELIOS_ESCAPE_CTX_DESTROY, HELIOS_ESCAPE_SUBMIT_VENUS,
    HELIOS_ESCAPE_WAIT_FENCE,
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

    match hdr.cmd_type {
        HELIOS_ESCAPE_CTX_CREATE => escape_ctx_create(adapter, buf),
        HELIOS_ESCAPE_CTX_DESTROY => escape_ctx_destroy(adapter, buf),
        HELIOS_ESCAPE_SUBMIT_VENUS => escape_submit_venus(adapter, buf),
        HELIOS_ESCAPE_WAIT_FENCE => escape_wait_fence(buf),
        // ALLOC_BLOB / MAP_BLOB land in M3.5; unknown verbs are rejected.
        _ => STATUS_NOT_IMPLEMENTED,
    }
}

/// `HELIOS_ESCAPE_CTX_CREATE` → create a Venus virtio-gpu context; write the
/// guest-assigned id back into the in/out buffer's `out_ctx_id`.
fn escape_ctx_create(adapter: &AdapterContext, buf: &mut [u8]) -> NTSTATUS {
    let sz = size_of::<HeliosEscapeCtxCreate>();
    if buf.len() < sz {
        return STATUS_BUFFER_TOO_SMALL;
    }
    let req: HeliosEscapeCtxCreate = pod_read_unaligned(&buf[..sz]);
    match adapter.with_virtio(|v| v.ctx_create(req.capset_id)) {
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

    // Copy the stream into device-visible contiguous memory (PASSIVE_LEVEL).
    let mut dma = match DmaBuffer::new(payload) {
        Some(d) => d,
        None => return STATUS_INSUFFICIENT_RESOURCES,
    };
    dma.as_mut_slice().copy_from_slice(&buf[hsz..end]);

    let (ctx_id, fence_id) = (req.ctx_id, req.fence_id);
    match adapter.with_virtio(|v| v.submit_venus(ctx_id, fence_id, dma.as_slice())) {
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
