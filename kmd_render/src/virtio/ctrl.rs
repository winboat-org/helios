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
use core::sync::atomic::{AtomicU32, AtomicU64};

use bytemuck::{bytes_of, Zeroable};
use wdk_sys::ntddk::{KeDelayExecutionThread, KeWaitForSingleObject};
use wdk_sys::{KEVENT, LARGE_INTEGER, PVOID, STATUS_SUCCESS};

use super::gpu::{
    BlobMapBegin, BlobMapFinish, BlobMapPrep, BlobRemapBegin, DeviceOwner,
    FenceWaitPrep, OwnerFilter, SyncWaitBlock, WaitBlockRef, CTRL_TEARDOWN_ABANDONS,
    CTRL_TIMEOUT_COUNT, SyncOutcome, SyncTicket,
    FENCE_WAIT_TABLE_FULL, FENCE_WAIT_TIMEOUTS, SUBMIT_META_BYTES, TRANSPORT_GONE_AT_WAIT,
};
use super::hal::DmaBuffer;
use super::VirtioError;
use crate::adapter::AdapterContext;
use core::sync::atomic::Ordering;
use helios_protocol::{
    resp_is_ok, VirtioGpuCtrlHdr, VirtioGpuCtxCreate, VirtioGpuCtxDestroy, VirtioGpuCtxResource,
    VirtioGpuRect, VirtioGpuResourceCreateBlob, VirtioGpuResourceFlush, VirtioGpuResourceMapBlob,
    VirtioGpuResourceUnmapBlob, VirtioGpuResourceUnref, VirtioGpuRespMapInfo,
    VirtioGpuSetScanoutBlob, VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE, VIRTIO_GPU_CMD_CTX_CREATE,
    VIRTIO_GPU_CMD_CTX_DESTROY, VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE,
    VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID, VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING,
    VIRTIO_GPU_CMD_RESOURCE_CREATE_2D, VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB,
    VIRTIO_GPU_CMD_RESOURCE_FLUSH, VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB,
    VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB, VIRTIO_GPU_CMD_RESOURCE_UNREF, VIRTIO_GPU_CMD_SET_SCANOUT,
    VIRTIO_GPU_CMD_SET_SCANOUT_BLOB, VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D, VIRTIO_GPU_MAP_CACHE_MASK,
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
/// full. MILLISECONDS, like its three siblings — it used to be a bare retry
/// count that only *happened* to equal 5 s because the sleep is hard-coded to
/// 1 ms.
const ENQUEUE_RETRY_MAX_MS: u64 = 5_000;
/// Hard cap on a single WAIT_FENCE escape (the ICD's own forward-progress
/// deadline fires far earlier; this only bounds kernel-side thread residency).
const WAIT_FENCE_MAX_MS: u64 = 120_000;
/// Bound on waiting out another mapper's in-flight RESOURCE_MAP_BLOB.
/// MILLISECONDS, as above.
const MAP_BUSY_MAX_MS: u64 = 30_000;

/// One PASSIVE retry slice. See [`sleep_ms`] for why this is not really 1 ms.
const RETRY_SLICE_MS: u64 = 1;

/// A retry budget in MILLISECONDS.
///
/// Two of the four sibling constants used to be millisecond budgets and two
/// were bare retry counts that only *happened* to equal 5 s and 30 s because
/// the sleep is hard-coded to 1 ms, and a fifth budget was an unnamed literal.
/// A units mismatch like that makes every reader over-estimate how fast the
/// driver gives up.
///
/// It counts NOMINAL slept milliseconds, not wall clock, which is exactly what
/// the retry counters it replaces did — same numbers, same sleeps, same failure
/// statuses. See [`sleep_ms`] for why nominal and actual differ by up to ~16x.
struct Budget {
    total_ms: u64,
    spent_ms: u64,
}

impl Budget {
    const fn new(total_ms: u64) -> Self {
        Self {
            total_ms,
            spent_ms: 0,
        }
    }

    /// Charge one slice. Returns true once the budget is exhausted, matching
    /// the old `attempts > MAX` test exactly (charge first, then test).
    fn charge_slice(&mut self) -> bool {
        self.spent_ms = self.spent_ms.saturating_add(RETRY_SLICE_MS);
        self.expired()
    }

    fn expired(&self) -> bool {
        self.spent_ms > self.total_ms
    }

    #[allow(dead_code)]
    fn elapsed_ms(&self) -> u64 {
        self.spent_ms
    }
}
static VIRGL_DIAG_BLOB_ID: AtomicU64 = AtomicU64::new(1);

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VirtioGpuResourceCreate2d {
    hdr: VirtioGpuCtrlHdr,
    resource_id: u32,
    format: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VirtioGpuSetScanout {
    hdr: VirtioGpuCtrlHdr,
    r: VirtioGpuRect,
    scanout_id: u32,
    resource_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VirtioGpuTransferToHost2d {
    hdr: VirtioGpuCtrlHdr,
    r: VirtioGpuRect,
    offset: u64,
    resource_id: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VirtioGpuResourceAttachBacking {
    hdr: VirtioGpuCtrlHdr,
    resource_id: u32,
    nr_entries: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VirtioGpuResourceAssignUuid {
    hdr: VirtioGpuCtrlHdr,
    resource_id: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VirtioGpuMemEntry {
    addr: u64,
    length: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VirtioGpuBox3d {
    x: u32,
    y: u32,
    z: u32,
    w: u32,
    h: u32,
    d: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VirtioGpuTransferHost3d {
    hdr: VirtioGpuCtrlHdr,
    box_: VirtioGpuBox3d,
    offset: u64,
    resource_id: u32,
    level: u32,
    stride: u32,
    layer_stride: u32,
}

/// PASSIVE sleep for ~`ms` milliseconds.
pub(crate) fn sleep_ms(ms: u64) {
    let mut interval: LARGE_INTEGER = unsafe { core::mem::zeroed() };
    interval.QuadPart = -((ms.max(1) as i64) * 10_000);
    // SAFETY: PASSIVE_LEVEL relative-timeout sleep.
    let _ = unsafe { KeDelayExecutionThread(KERNEL_MODE, 0, &mut interval) };
}
// ⚠ `KeDelayExecutionThread` with a small relative timeout rounds UP to the
// system timer granularity — ~15.6 ms by default. A `sleep_ms(1)` therefore
// costs up to ~16 ms of thread residency, so a [`Budget`] of N nominal
// milliseconds can be up to ~16N of real time. Every budget in this module is
// nominal for that reason; do not read one as wall clock.

/// Wait on `block` for up to `total_ms`, in adaptive slices (1 ms → 1 s),
/// opportunistically draining the used ring after each slice so a lost
/// interrupt costs only slice latency. Returns whether the block completed.
fn wait_block(adapter: &AdapterContext, block: &WaitBlockRef<'_>, total_ms: u64) -> bool {
    let mut waited: u64 = 0;
    let mut slice: u64 = 1;
    loop {
        // The borrow proves the block is alive for this whole call; only the
        // drain (under the device lock) writes it, through atomics.
        if block.is_done() {
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
                core::ptr::addr_of_mut!((*block.as_ptr().as_ptr()).event) as PVOID,
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

/// Reap completed entries at PASSIVE and retain their DMA buffers for reuse.
/// `MmAllocateContiguousMemory` per tiny Venus submission dominated DWM's
/// command rate; recycling page-backed buffers removes that steady-state cost.
pub fn reap_parked(adapter: &AdapterContext) {
    let work = adapter.with_virtio(|v| v.begin_parked_reap());
    let Ok(Some((mut dead, mut buffers))) = work else {
        return;
    };
    debug_assert!(buffers.capacity() >= dead.len().saturating_mul(2));
    for entry in dead.drain(..) {
        let (meta, venus) = entry.into_dma_buffers();
        buffers.push(meta);
        if let Some(venus) = venus {
            buffers.push(venus);
        }
    }
    // Moving buffers into the pre-reserved pool is allocation-free under the
    // spinlock. Excess buffers are returned and dropped here at PASSIVE.
    //
    // The `else` arm is the two-phase strand: returning here without
    // finish_parked_reap left reap_in_progress true and both pre-reserved
    // spares taken, permanently disabling reaping and then refusing every
    // enqueue at the PARKED_ENQUEUE_GATE. `dead` is already drained, so the
    // abort restores both vectors intact.
    let excess = adapter.with_virtio(move |v| v.recycle_dma_buffers(buffers));
    let Ok(mut excess) = excess else {
        let _ = adapter.with_virtio(move |v| v.abort_parked_reap(dead, alloc::vec::Vec::new()));
        return;
    };
    // Drop only the retained elements at PASSIVE while preserving the vector's
    // allocation for the next reap.
    excess.clear();
    let _ = adapter.with_virtio(move |v| v.finish_parked_reap(dead, excess));
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

    // The wait block is created, initialised and dropped inside `with`, so it
    // is never nameable here: "registered before init" and "moved after init"
    // are not expressible. The abandon-on-timeout epilogue is the closure's
    // last statement, which is what keeps deregistration paired with the frame.
    SyncWaitBlock::with(|block| {
        // Enqueue, with PASSIVE backpressure while the queue is full.
        //
        // `meta` is carried as a loop value, not round-tripped through an
        // `Option`. The enqueue moves it into the closure and the QueueFull arm
        // reinitialises it before the back edge, which Rust's flow-sensitive move
        // checking accepts. A future retry arm that forgets to hand the buffer back
        // is then a *compile* error, where the take-then-expect this replaces was a
        // `KeBugCheck` inside a DDI on the next iteration.
        let mut budget = Budget::new(ENQUEUE_RETRY_MAX_MS);
        let token: SyncTicket = loop {
            let res = adapter.with_virtio(move |v| {
                v.drain_used();
                v.enqueue_sync(meta, in0_len, in1_len, resp_len, block.as_ptr())
            });
            match res {
                Err(_) => return Err(VirtioError::DeviceError), // transport gone
                Ok(Ok(ticket)) => break ticket,
                Ok(Err((m_back, VirtioError::QueueFull))) => {
                    meta = m_back;
                    if budget.charge_slice() {
                        return Err(VirtioError::QueueFull);
                    }
                    reap_parked(adapter);
                    sleep_ms(RETRY_SLICE_MS);
                }
                Ok(Err((_m, e))) => return Err(e), // dropped here at PASSIVE
            }
        };

        // Kept for the refusal breadcrumb: SyncTicket is move-only, so it is
        // consumed by abandon_sync and cannot be read afterwards.
        let token_value = token.raw();
        if !wait_block(adapter, block, timeout_ms) {
            // Final race check + abandonment under the lock.
            // Three outcomes, not two. `unwrap_or(true)` folded Err(DeviceNotFound)
            // - the transport was torn down under us - into "already completed
            // successfully", which skipped the timeout counter and picked the wrong
            // error class. The fake-success half is masked here because all three
            // callers re-validate resp_is_ok on the returned bytes and a zeroed
            // response fails that, but the missing evidence was real.
            match adapter.with_virtio(|v| {
                v.drain_used();
                v.abandon_sync(token, block.as_ptr())
            }) {
                // The drain already signalled us; the response bytes are valid.
                Ok(SyncOutcome::AlreadyCompleted) => {}
                Ok(SyncOutcome::Abandoned) => {
                    CTRL_TIMEOUT_COUNT.fetch_add(1, Ordering::Relaxed);
                    return Err(VirtioError::Timeout);
                }
                // NEW population. The token names an entry that is not this
                // waiter's, so `resp` was never written — do NOT copy it out.
                // The old bool folded this into "already completed" and handed
                // the caller a zeroed buffer.
                Ok(SyncOutcome::NotOurs) => {
                    crate::diag::record_named_bytes(b"CtNotOurs", u32::from(token_value));
                    return Err(VirtioError::DeviceError);
                }
                Err(_) => {
                    CTRL_TEARDOWN_ABANDONS.fetch_add(1, Ordering::Relaxed);
                    return Err(VirtioError::DeviceError);
                }
            }
        }
        block.copy_resp(resp_out);
        Ok(())
    })
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
    owner: Option<DeviceOwner>,
) -> Result<u32, VirtioError> {
    let ctx_id = adapter
        .with_virtio(|v| v.alloc_ctx_id())
        .map_err(|_| VirtioError::DeviceError)?;
    // Reserve the tracking slot BEFORE the host round-trip: tracking is
    // mandatory, so a context this driver cannot track must not be created.
    let reserved = adapter
        .with_virtio(|v| v.reserve_context_slot())
        .map_err(|_| VirtioError::DeviceError)?;
    if !reserved {
        return Err(VirtioError::OutOfMemory);
    }
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
    if let Err(e) = ctrl_roundtrip_ok(adapter, bytes_of(&cmd), None) {
        let _ = adapter.with_virtio(|v| v.cancel_context_reservation());
        return Err(e);
    }
    crate::diag::record(0x0D21_0000 | (ctx_id & 0xFFFF));
    let _ = adapter.with_virtio(|v| v.commit_context(owner, ctx_id));
    Ok(ctx_id)
}

/// Destroy a context and drop its tracking slot, scoped to its owner.
///
/// The untrack and the ownership test are ONE step under the device lock, so a
/// racing CTX_DESTROY for the same id cannot have both callers pass the check.
/// A guest-supplied id that this owner does not own never reaches the wire:
/// before this, CTX_DESTROY took the raw id straight to the host, so process B
/// (or A after a restart that recycled the id) could destroy process A's Venus
/// context and A's next submit referenced a destroyed host context — CS error,
/// fatal decoder state (k-capsescape-02).
pub fn ctx_destroy(
    adapter: &AdapterContext,
    owner: Option<DeviceOwner>,
    ctx_id: u32,
) -> Result<(), VirtioError> {
    let owned = adapter
        .with_virtio(|v| v.untrack_owned_context(owner, ctx_id))
        .map_err(|_| VirtioError::DeviceError)?;
    let Some(ctx_id) = owned else {
        return Err(VirtioError::NotOwned);
    };
    let mut cmd = VirtioGpuCtxDestroy::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_CTX_DESTROY;
    cmd.hdr.ctx_id = ctx_id;
    ctrl_roundtrip_ok(adapter, bytes_of(&cmd), None)
}

/// Untracked teardown of a context this driver created for itself (the
/// persistent venus context, the virgl diagnostic contexts). Owner-scoped to
/// the KMD.
pub fn ctx_destroy_kmd(adapter: &AdapterContext, ctx_id: u32) -> Result<(), VirtioError> {
    ctx_destroy(adapter, None, ctx_id)
}

/// `CTX_DESTROY` every context still owned by `owner` (device teardown).
pub fn destroy_contexts_for_owner(adapter: &AdapterContext, owner: Option<DeviceOwner>) -> u32 {
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
/// Bind a venus blob `resource_id` to scanout 0 (the QEMU gtk/sdl display) via
/// `SET_SCANOUT_BLOB` — the Phase-7 zero-copy display path (DISPLAY.md §8), now
/// driven from the WDDM VidPn scanout DDI. The blob must be a dmabuf-exportable
/// HOST3D resource (the host's venus render-server exports its `dmabuf_fd`, e.g.
/// via ANV); a non-exportable/wrong-layout resource is rejected host-side and
/// surfaces here as `VirtioError::DeviceError` — that IS the export-gate signal.
/// `stride`/`offset` are plane-0 geometry of the LINEAR image. Device-global
/// (`hdr.ctx_id = 0`). PASSIVE_LEVEL only (control round-trip).
pub fn set_scanout_blob(
    adapter: &AdapterContext,
    resource_id: u32,
    width: u32,
    height: u32,
    format: u32,
    stride: u32,
    offset: u32,
) -> Result<(), VirtioError> {
    let mut cmd = VirtioGpuSetScanoutBlob::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_SET_SCANOUT_BLOB;
    cmd.r = VirtioGpuRect {
        x: 0,
        y: 0,
        width,
        height,
    };
    cmd.scanout_id = 0;
    cmd.resource_id = resource_id;
    cmd.width = width;
    cmd.height = height;
    cmd.format = format;
    cmd.strides[0] = stride;
    cmd.offsets[0] = offset;
    ctrl_roundtrip_ok(adapter, bytes_of(&cmd), None)
}

/// Flush scanout 0's bound resource to the host display (`RESOURCE_FLUSH`) — for
/// a GL/blob scanout this is what drives the actual host present. Device-global.
/// PASSIVE_LEVEL only (control round-trip).
pub fn resource_flush(
    adapter: &AdapterContext,
    resource_id: u32,
    width: u32,
    height: u32,
) -> Result<(), VirtioError> {
    let mut cmd = VirtioGpuResourceFlush::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_FLUSH;
    cmd.r = VirtioGpuRect {
        x: 0,
        y: 0,
        width,
        height,
    };
    cmd.resource_id = resource_id;
    ctrl_roundtrip_ok(adapter, bytes_of(&cmd), None)
}

/// Queue a RESOURCE_FLUSH without synchronously waiting for its ctrl response.
/// The used-ring drain validates the response, clears `completion`, and wakes
/// `wake_event`.  This is intentionally limited to scanout refresh: unlike
/// lifecycle commands, the caller does not need response data before it can
/// continue, and blocking here previously imposed the observed ~0.41 s/frame
/// cadence when ctrl interrupts were delayed.
pub fn resource_flush_async(
    adapter: &AdapterContext,
    resource_id: u32,
    width: u32,
    height: u32,
    completion: NonNull<AtomicU32>,
    completion_errors: NonNull<AtomicU32>,
    wake_event: NonNull<KEVENT>,
) -> Result<(), VirtioError> {
    let mut cmd = VirtioGpuResourceFlush::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_FLUSH;
    cmd.r = VirtioGpuRect {
        x: 0,
        y: 0,
        width,
        height,
    };
    cmd.resource_id = resource_id;

    reap_parked(adapter);
    let request = bytes_of(&cmd);
    let response_len = size_of::<VirtioGpuCtrlHdr>();
    let mut meta = DmaBuffer::new(request.len() + response_len).ok_or(VirtioError::OutOfMemory)?;
    meta.as_mut_slice()[..request.len()].copy_from_slice(request);

    let queued = adapter.with_virtio(move |v| {
        v.drain_used();
        v.enqueue_async_control(
            meta,
            request.len(),
            response_len,
            completion,
            completion_errors,
            wake_event,
            None,
            None,
        )
    });
    match queued {
        Ok(Ok(())) => Ok(()),
        Ok(Err((_meta, e))) => Err(e),
        Err(_) => Err(VirtioError::DeviceError),
    }
}

/// Queue SET_SCANOUT_BLOB without waiting for its ctrl response. A successful
/// response publishes `resource_id` through `host_bound` and re-arms
/// `refresh_pending` so the worker flushes the newly-bound image. A rejected
/// bind does not self-resubmit: only a new Windows-selected candidate or a new
/// completion-ordered dirty edge may request another attempt.
pub fn set_scanout_blob_async(
    adapter: &AdapterContext,
    resource_id: u32,
    width: u32,
    height: u32,
    format: u32,
    stride: u32,
    offset: u32,
    completion: NonNull<AtomicU32>,
    completion_errors: NonNull<AtomicU32>,
    host_bound: NonNull<AtomicU32>,
    refresh_pending: NonNull<AtomicU32>,
    wake_event: NonNull<KEVENT>,
) -> Result<(), VirtioError> {
    let mut cmd = VirtioGpuSetScanoutBlob::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_SET_SCANOUT_BLOB;
    cmd.r = VirtioGpuRect {
        x: 0,
        y: 0,
        width,
        height,
    };
    cmd.scanout_id = 0;
    cmd.resource_id = resource_id;
    cmd.width = width;
    cmd.height = height;
    cmd.format = format;
    cmd.strides[0] = stride;
    cmd.offsets[0] = offset;

    reap_parked(adapter);
    let request = bytes_of(&cmd);
    let response_len = size_of::<VirtioGpuCtrlHdr>();
    let mut meta = DmaBuffer::new(request.len() + response_len).ok_or(VirtioError::OutOfMemory)?;
    meta.as_mut_slice()[..request.len()].copy_from_slice(request);

    let queued = adapter.with_virtio(move |v| {
        v.drain_used();
        v.enqueue_async_control(
            meta,
            request.len(),
            response_len,
            completion,
            completion_errors,
            wake_event,
            Some((host_bound, resource_id)),
            Some(refresh_pending),
        )
    });
    match queued {
        Ok(Ok(())) => Ok(()),
        Ok(Err((_meta, e))) => Err(e),
        Err(_) => Err(VirtioError::DeviceError),
    }
}

pub fn set_scanout_2d(
    adapter: &AdapterContext,
    resource_id: u32,
    width: u32,
    height: u32,
) -> Result<(), VirtioError> {
    let mut set = VirtioGpuSetScanout::zeroed();
    set.hdr.type_ = VIRTIO_GPU_CMD_SET_SCANOUT;
    set.r = VirtioGpuRect {
        x: 0,
        y: 0,
        width,
        height,
    };
    set.scanout_id = 0;
    set.resource_id = resource_id;
    ctrl_roundtrip_ok(adapter, bytes_of(&set), None)
}

pub fn resource_unref(adapter: &AdapterContext, resource_id: u32) -> Result<(), VirtioError> {
    let mut cmd = VirtioGpuResourceUnref::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_UNREF;
    cmd.resource_id = resource_id;
    ctrl_roundtrip_ok(adapter, bytes_of(&cmd), None)
}

/// Linux virtio-gpu assigns a resource UUID when a blob is created with
/// `USE_CROSS_DEVICE`. Keep the full 40-byte response buffer because the OK
/// reply carries the UUID after the control header.
pub fn resource_assign_uuid(adapter: &AdapterContext, resource_id: u32) -> Result<(), VirtioError> {
    let mut cmd = VirtioGpuResourceAssignUuid::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID;
    cmd.resource_id = resource_id;

    let mut resp = [0u8; size_of::<VirtioGpuCtrlHdr>() + 16];
    ctrl_roundtrip(
        adapter,
        bytes_of(&cmd),
        None,
        &mut resp,
        SYNC_ROUNDTRIP_TIMEOUT_MS,
    )?;
    let resp_type = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
    if resp_is_ok(resp_type) {
        Ok(())
    } else {
        Err(VirtioError::DeviceError)
    }
}

/// Diagnostic classic virtio-gpu 2D scanout. This avoids GL dmabuf import
/// entirely and proves whether QEMU's display frontend can show scanout 0 on
/// the current host. The backing allocation is intentionally leaked so the host
/// can continue scanning it out after this PASSIVE one-shot returns.
pub fn diagnostic_2d_scanout(
    adapter: &AdapterContext,
    width: u32,
    height: u32,
) -> Result<u32, VirtioError> {
    let pitch = (width as usize)
        .checked_mul(4)
        .ok_or(VirtioError::OutOfMemory)?;
    let size = pitch
        .checked_mul(height as usize)
        .ok_or(VirtioError::OutOfMemory)?;
    if size > u32::MAX as usize {
        return Err(VirtioError::OutOfMemory);
    }

    let mut backing = DmaBuffer::new(size).ok_or(VirtioError::OutOfMemory)?;
    {
        let buf = backing.as_mut_slice();
        let mut y = 0usize;
        while y < height as usize {
            let mut x = 0usize;
            while x < width as usize {
                let color = if x < width as usize / 3 {
                    0xFFFF00FFu32
                } else if x < (width as usize * 2) / 3 {
                    0xFF00FFFFu32
                } else {
                    0xFF00FF00u32
                };
                let off = y * pitch + x * 4;
                buf[off..off + 4].copy_from_slice(&color.to_le_bytes());
                x += 1;
            }
            y += 1;
        }
    }

    let resource_id = adapter
        .with_virtio(|v| v.alloc_resource_id())
        .map_err(|_| VirtioError::DeviceError)?;

    let mut create = VirtioGpuResourceCreate2d::zeroed();
    create.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_CREATE_2D;
    create.resource_id = resource_id;
    create.format = helios_protocol::VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM;
    create.width = width;
    create.height = height;
    ctrl_roundtrip_ok(adapter, bytes_of(&create), None)?;

    let mut attach = VirtioGpuResourceAttachBacking::zeroed();
    attach.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING;
    attach.resource_id = resource_id;
    attach.nr_entries = 1;
    let entry = VirtioGpuMemEntry {
        addr: backing.physical_address(),
        length: size as u32,
        padding: 0,
    };
    ctrl_roundtrip_ok(adapter, bytes_of(&attach), Some(bytes_of(&entry)))?;

    set_scanout_2d(adapter, resource_id, width, height)?;

    let mut xfer = VirtioGpuTransferToHost2d::zeroed();
    xfer.hdr.type_ = VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D;
    xfer.r = VirtioGpuRect {
        x: 0,
        y: 0,
        width,
        height,
    };
    xfer.offset = 0;
    xfer.resource_id = resource_id;
    ctrl_roundtrip_ok(adapter, bytes_of(&xfer), None)?;
    resource_flush(adapter, resource_id, width, height)?;

    core::mem::forget(backing);
    Ok(resource_id)
}

/// Diagnostic guest-memory blob scanout. This still uses SET_SCANOUT_BLOB, but
/// avoids HOST3D/Venus dma-buf export by backing the blob directly with guest
/// RAM. The backing allocation is intentionally leaked while scanned out.
pub fn diagnostic_guest_blob_scanout(
    adapter: &AdapterContext,
    width: u32,
    height: u32,
    format: u32,
    blob_mem: u32,
) -> Result<(u32, u32), VirtioError> {
    let pitch = width.checked_mul(4).ok_or(VirtioError::OutOfMemory)?;
    let size = (pitch as usize)
        .checked_mul(height as usize)
        .ok_or(VirtioError::OutOfMemory)?;
    if size == 0 || size > u32::MAX as usize {
        return Err(VirtioError::OutOfMemory);
    }

    let mut backing = DmaBuffer::new(size).ok_or(VirtioError::OutOfMemory)?;
    {
        let buf = backing.as_mut_slice();
        let mut y = 0usize;
        while y < height as usize {
            let mut x = 0usize;
            while x < width as usize {
                let bar = (x * 4) / (width as usize).max(1);
                let checker = ((x >> 5) ^ (y >> 5)) & 1;
                let color = match bar {
                    0 => 0xFFFF00FFu32,
                    1 => 0xFF00FF00u32,
                    2 => 0xFFFFFFFFu32,
                    _ => {
                        if checker == 0 {
                            0xFF000000u32
                        } else {
                            0xFF404040u32
                        }
                    }
                };
                let off = y * pitch as usize + x * 4;
                buf[off..off + 4].copy_from_slice(&color.to_le_bytes());
                x += 1;
            }
            y += 1;
        }
    }

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

    let mut create = VirtioGpuResourceCreateBlob::zeroed();
    create.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB;
    create.hdr.ctx_id = 0;
    create.resource_id = resource_id;
    create.blob_mem = blob_mem;
    create.blob_flags = helios_protocol::VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE;
    create.nr_entries = 1;
    create.blob_id = 0;
    create.size = size as u64;
    let entry = VirtioGpuMemEntry {
        addr: backing.physical_address(),
        length: size as u32,
        padding: 0,
    };
    if let Err(e) = ctrl_roundtrip_ok(adapter, bytes_of(&create), Some(bytes_of(&entry))) {
        let _ = adapter.with_virtio(|v| v.cancel_resource_reservation());
        return Err(e);
    }

    let _ = adapter.with_virtio(|v| v.commit_resource(resource_id));
    if let Err(e) = set_scanout_blob(adapter, resource_id, width, height, format, pitch, 0) {
        let _ = resource_unref(adapter, resource_id);
        return Err(e);
    }
    core::mem::forget(backing);
    resource_flush(adapter, resource_id, width, height)?;
    Ok((resource_id, pitch))
}

pub fn diagnostic_virgl_host3d_blob(
    adapter: &AdapterContext,
    width: u32,
    height: u32,
    format: u32,
) -> Result<(u32, u64, u32), VirtioError> {
    let pitch = width.checked_mul(4).ok_or(VirtioError::OutOfMemory)?;
    let size = (pitch as u64)
        .checked_mul(height as u64)
        .ok_or(VirtioError::OutOfMemory)?;
    if size == 0 || size > u32::MAX as u64 {
        return Err(VirtioError::OutOfMemory);
    }
    let aligned_size = (size + 0xFFF) & !0xFFF;
    let blob_id = VIRGL_DIAG_BLOB_ID.fetch_add(1, Ordering::Relaxed);
    let ctx_id = ctx_create(adapter, helios_protocol::VIRTIO_GPU_CAPSET_VIRGL, None)?;

    const VIRGL_CCMD_PIPE_RESOURCE_CREATE: u32 = 48;
    const VIRGL_PIPE_RES_CREATE_SIZE: u32 = 11;
    const PIPE_TEXTURE_2D: u32 = 2;
    const VIRGL_BIND_RENDER_TARGET: u32 = 1 << 1;
    const VIRGL_BIND_DISPLAY_TARGET: u32 = 1 << 7;
    const VIRGL_BIND_SCANOUT: u32 = 1 << 18;
    const VIRGL_BIND_SHARED: u32 = 1 << 20;
    const VIRGL_BIND_LINEAR: u32 = 1 << 22;
    const VIRGL_RESOURCE_FLAG_MAP_PERSISTENT: u32 = 1 << 1;
    const VIRGL_RESOURCE_FLAG_MAP_COHERENT: u32 = 1 << 2;

    let bind = VIRGL_BIND_RENDER_TARGET
        | VIRGL_BIND_DISPLAY_TARGET
        | VIRGL_BIND_SCANOUT
        | VIRGL_BIND_SHARED
        | VIRGL_BIND_LINEAR;
    let flags = VIRGL_RESOURCE_FLAG_MAP_PERSISTENT | VIRGL_RESOURCE_FLAG_MAP_COHERENT;
    let mut stream = [0u32; 12];
    stream[0] = VIRGL_CCMD_PIPE_RESOURCE_CREATE | (VIRGL_PIPE_RES_CREATE_SIZE << 16);
    stream[1] = PIPE_TEXTURE_2D;
    stream[2] = format;
    stream[3] = bind;
    stream[4] = width;
    stream[5] = height;
    stream[6] = 1;
    stream[7] = 1;
    stream[8] = 0;
    stream[9] = 0;
    stream[10] = flags;
    stream[11] = blob_id as u32;

    // SAFETY: `stream` is a plain u32 command array; exposing its initialized
    // bytes for SUBMIT_3D preserves alignment and lifetime for this call.
    let bytes = unsafe {
        core::slice::from_raw_parts(
            stream.as_ptr() as *const u8,
            stream.len() * size_of::<u32>(),
        )
    };
    if let Err(e) = submit_3d_sync(adapter, ctx_id, bytes) {
        let _ = ctx_destroy_kmd(adapter, ctx_id);
        return Err(e);
    }

    let blob_flags = helios_protocol::VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE;
    match alloc_blob(
        adapter,
        ctx_id,
        helios_protocol::VIRTIO_GPU_BLOB_MEM_HOST3D,
        blob_flags,
        blob_id,
        aligned_size,
        None,
    ) {
        Ok(resource_id) => Ok((resource_id, blob_id, pitch)),
        Err(e) => {
            let _ = ctx_destroy_kmd(adapter, ctx_id);
            Err(e)
        }
    }
}

pub fn diagnostic_virgl_host3d_guest_scanout(
    adapter: &AdapterContext,
    width: u32,
    height: u32,
    format: u32,
) -> Result<(u32, u64, u32), VirtioError> {
    let pitch = width.checked_mul(4).ok_or(VirtioError::OutOfMemory)?;
    let size = (pitch as u64)
        .checked_mul(height as u64)
        .ok_or(VirtioError::OutOfMemory)?;
    if size == 0 || size > u32::MAX as u64 {
        return Err(VirtioError::OutOfMemory);
    }
    let aligned_size = (size + 0xFFF) & !0xFFF;

    let mut backing = DmaBuffer::new(aligned_size as usize).ok_or(VirtioError::OutOfMemory)?;
    {
        let buf = backing.as_mut_slice();
        let mut y = 0usize;
        while y < height as usize {
            let mut x = 0usize;
            while x < width as usize {
                let bar = (x * 4) / (width as usize).max(1);
                let checker = ((x >> 5) ^ (y >> 5)) & 1;
                let color = match bar {
                    0 => 0xFFFF00FFu32,
                    1 => 0xFF00FF00u32,
                    2 => 0xFFFFFFFFu32,
                    _ => {
                        if checker == 0 {
                            0xFF000000u32
                        } else {
                            0xFF404040u32
                        }
                    }
                };
                let off = y * pitch as usize + x * 4;
                buf[off..off + 4].copy_from_slice(&color.to_le_bytes());
                x += 1;
            }
            y += 1;
        }
    }

    let blob_id = VIRGL_DIAG_BLOB_ID.fetch_add(1, Ordering::Relaxed);
    let ctx_id = ctx_create(adapter, helios_protocol::VIRTIO_GPU_CAPSET_VIRGL, None)?;

    const VIRGL_CCMD_PIPE_RESOURCE_CREATE: u32 = 48;
    const VIRGL_PIPE_RES_CREATE_SIZE: u32 = 11;
    const PIPE_TEXTURE_2D: u32 = 2;
    const VIRGL_BIND_RENDER_TARGET: u32 = 1 << 1;
    const VIRGL_BIND_DISPLAY_TARGET: u32 = 1 << 7;
    const VIRGL_BIND_SCANOUT: u32 = 1 << 18;
    const VIRGL_BIND_SHARED: u32 = 1 << 20;
    const VIRGL_BIND_LINEAR: u32 = 1 << 22;

    let bind = VIRGL_BIND_RENDER_TARGET
        | VIRGL_BIND_DISPLAY_TARGET
        | VIRGL_BIND_SCANOUT
        | VIRGL_BIND_SHARED
        | VIRGL_BIND_LINEAR;
    let mut stream = [0u32; 12];
    stream[0] = VIRGL_CCMD_PIPE_RESOURCE_CREATE | (VIRGL_PIPE_RES_CREATE_SIZE << 16);
    stream[1] = PIPE_TEXTURE_2D;
    stream[2] = format;
    stream[3] = bind;
    stream[4] = width;
    stream[5] = height;
    stream[6] = 1;
    stream[7] = 1;
    stream[8] = 0;
    stream[9] = 0;
    stream[10] = 0;
    stream[11] = blob_id as u32;

    // SAFETY: `stream` is an initialized u32 command array exposed as bytes for
    // the synchronous SUBMIT_3D call.
    let bytes = unsafe {
        core::slice::from_raw_parts(
            stream.as_ptr() as *const u8,
            stream.len() * size_of::<u32>(),
        )
    };
    if let Err(e) = submit_3d_sync(adapter, ctx_id, bytes) {
        let _ = ctx_destroy_kmd(adapter, ctx_id);
        return Err(e);
    }

    let reserved = adapter
        .with_virtio(|v| v.reserve_resource_slot())
        .map_err(|_| VirtioError::DeviceError)?;
    if !reserved {
        let _ = ctx_destroy_kmd(adapter, ctx_id);
        return Err(VirtioError::OutOfMemory);
    }
    let resource_id = match adapter.with_virtio(|v| v.alloc_resource_id()) {
        Ok(id) => id,
        Err(_) => {
            let _ = adapter.with_virtio(|v| v.cancel_resource_reservation());
            let _ = ctx_destroy_kmd(adapter, ctx_id);
            return Err(VirtioError::DeviceError);
        }
    };

    let mut create = VirtioGpuResourceCreateBlob::zeroed();
    create.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB;
    create.hdr.ctx_id = ctx_id;
    create.resource_id = resource_id;
    create.blob_mem = helios_protocol::VIRTIO_GPU_BLOB_MEM_HOST3D_GUEST;
    create.blob_flags = helios_protocol::VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE;
    create.nr_entries = 1;
    create.blob_id = blob_id;
    create.size = aligned_size;
    let entry = VirtioGpuMemEntry {
        addr: backing.physical_address(),
        length: aligned_size as u32,
        padding: 0,
    };
    if let Err(e) = ctrl_roundtrip_ok(adapter, bytes_of(&create), Some(bytes_of(&entry))) {
        let _ = adapter.with_virtio(|v| v.cancel_resource_reservation());
        let _ = ctx_destroy_kmd(adapter, ctx_id);
        return Err(e);
    }
    let _ = adapter.with_virtio(|v| v.commit_resource(resource_id));

    let mut xfer = VirtioGpuTransferHost3d::zeroed();
    xfer.hdr.type_ = helios_protocol::VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D;
    xfer.hdr.ctx_id = ctx_id;
    xfer.box_ = VirtioGpuBox3d {
        x: 0,
        y: 0,
        z: 0,
        w: width,
        h: height,
        d: 1,
    };
    xfer.offset = 0;
    xfer.resource_id = resource_id;
    xfer.level = 0;
    xfer.stride = pitch;
    xfer.layer_stride = pitch.saturating_mul(height);
    if let Err(e) = ctrl_roundtrip_ok(adapter, bytes_of(&xfer), None) {
        let _ = resource_unref(adapter, resource_id);
        return Err(e);
    }

    core::mem::forget(backing);
    Ok((resource_id, blob_id, pitch))
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
    owner: Option<DeviceOwner>,
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
    owner: OwnerFilter,
    resource_id: u32,
) -> Result<BlobMapPrep, VirtioError> {
    let mut busy = Budget::new(MAP_BUSY_MAX_MS);
    loop {
        let begin = adapter
            .with_virtio(|v| v.blob_map_begin(owner, resource_id))
            .map_err(|_| VirtioError::DeviceError)?;
        match begin {
            BlobMapBegin::Mapped(prep) => return Ok(prep),
            BlobMapBegin::Failed(e) => return Err(e),
            BlobMapBegin::Busy => {
                if busy.charge_slice() {
                    return Err(VirtioError::Timeout);
                }
                sleep_ms(RETRY_SLICE_MS);
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
                        let _ = adapter.with_virtio(|v| v.free_window_range_pub(offset, len));
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
    // Two swallows used to live on this line. `.unwrap_or(0)` turned a torn-down
    // transport into "no stale placements", skipping the eviction pass entirely
    // instead of failing the map; and the scan itself silently stopped recording
    // once `stale` was full, so a ninth overlapping mapping was neither unmapped
    // nor reported and the RESOURCE_MAP_BLOB below created the overlapping host
    // window subregion this pass exists to prevent. Both are now hard failures:
    // refusing the aperture map / paging op is strictly better than two host
    // resources sharing one window subregion (k-gputransport-04).
    let n = adapter
        .with_virtio(|v| v.blobs_overlapping(window_offset, map_len, resource_id, &mut stale))
        .map_err(|_| VirtioError::DeviceError)?
        .map_err(|_truncated| VirtioError::DeviceError)?;
    for &res in stale[..n].iter() {
        let _ = resource_unmap_blob(adapter, res);
        let _ = adapter.with_virtio(|v| v.blob_note_unmapped(res));
    }

    let mut busy = Budget::new(MAP_BUSY_MAX_MS);
    loop {
        let begin = adapter
            .with_virtio(|v| v.blob_remap_begin(resource_id, window_offset))
            .map_err(|_| VirtioError::DeviceError)?;
        match begin {
            BlobRemapBegin::Mapped(prep) => return Ok(prep),
            BlobRemapBegin::Failed(e) => return Err(e),
            BlobRemapBegin::Busy => {
                if busy.charge_slice() {
                    return Err(VirtioError::Timeout);
                }
                sleep_ms(RETRY_SLICE_MS);
            }
            BlobRemapBegin::Start { old, len } => {
                if let Some((old_offset, old_len)) = old {
                    // Content-preserving move: unmap the previous placement and
                    // (for KMD-partition offsets only — the free guard ignores
                    // VidMm-partition ones) return its range.
                    let _ = resource_unmap_blob(adapter, resource_id);
                    let _ = adapter.with_virtio(|v| v.free_window_range_pub(old_offset, old_len));
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
    owner: DeviceOwner,
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
pub fn release_blobs_for_owner(adapter: &AdapterContext, owner: Option<DeviceOwner>) -> u32 {
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
pub fn submit_3d_sync(
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

pub fn submit_venus_sync(
    adapter: &AdapterContext,
    ctx_id: u32,
    stream: &[u8],
) -> Result<(), VirtioError> {
    submit_3d_sync(adapter, ctx_id, stream)
}

/// ASYNC venus SUBMIT_3D (the ICD escape path): stage the stream into DMA
/// buffers, enqueue fenced with a fresh KMD wire fence id, and return that id
/// at QUEUE time. Completion is observed via [`wait_fence`].
pub fn submit_venus_async(
    adapter: &AdapterContext,
    owner: Option<DeviceOwner>,
    ctx_id: u32,
    ring_idx: u32,
    stream: &[u8],
) -> Result<u64, VirtioError> {
    if stream.is_empty() {
        return Err(VirtioError::DeviceError);
    }
    // Ownership is resolved under the device lock, the same lock the enqueue
    // below takes, so a foreign command stream cannot reach another process's
    // Venus ring. This costs no extra acquisition on the ~89 us submit path.
    let owned = adapter
        .with_virtio(|v| v.resolve_owned_ctx(owner, ctx_id))
        .map_err(|_| VirtioError::DeviceError)?;
    let Some(owned) = owned else {
        return Err(VirtioError::NotOwned);
    };
    let ctx_id = owned.id();
    reap_parked(adapter);
    let mut meta = adapter
        .with_virtio(|v| v.take_dma_buffer(SUBMIT_META_BYTES))
        .ok()
        .flatten()
        .or_else(|| DmaBuffer::new(SUBMIT_META_BYTES))
        .ok_or(VirtioError::OutOfMemory)?;
    let mut venus = adapter
        .with_virtio(|v| v.take_dma_buffer(stream.len()))
        .ok()
        .flatten()
        .or_else(|| DmaBuffer::new(stream.len()))
        .ok_or(VirtioError::OutOfMemory)?;
    venus.as_mut_slice()[..stream.len()].copy_from_slice(stream);
    let venus_len = stream.len();

    // Both buffers are carried as loop values for the reason given in
    // `ctrl_roundtrip`: this loop has two of them, so an arm that returns only
    // one is exactly the maintenance mistake the take-then-expect pair used to
    // turn into a bugcheck. As loop values it does not compile.
    let mut budget = Budget::new(ENQUEUE_RETRY_MAX_MS);
    loop {
        let res = adapter.with_virtio(move |v| {
            v.drain_used();
            v.enqueue_async_submit(ctx_id, ring_idx, meta, venus, venus_len)
        });
        match res {
            Err(_) => return Err(VirtioError::DeviceError), // transport gone
            Ok(Ok(fence_id)) => return Ok(fence_id),
            Ok(Err((m_back, v_back, VirtioError::QueueFull))) => {
                meta = m_back;
                venus = v_back;
                if budget.charge_slice() {
                    return Err(VirtioError::QueueFull);
                }
                reap_parked(adapter);
                sleep_ms(RETRY_SLICE_MS);
            }
            Ok(Err((_m, _v, e))) => return Err(e), // buffers dropped at PASSIVE
        }
    }
}

/// Nonblocking KMD scanout-copy submission. `stream` is the already encoded
/// Venus vkQueueSubmit command; the outer virtio SUBMIT_3D is fenced on
/// ring_idx=1, whose used-ring completion represents GPU completion. Only a
/// successful completion marks scanout dirty and wakes the refresh worker.
///
/// Unlike the user escape path above, this per-frame display path never sleeps
/// for queue backpressure: one enqueue attempt either succeeds or reports
/// QueueFull to the caller. That keeps SetVidPnSourceAddress out of a hidden
/// multi-second retry loop.
pub fn submit_venus_async_scanout(
    adapter: &AdapterContext,
    ctx_id: u32,
    stream: &[u8],
    primary_address: u64,
    ticket: crate::adapter::ProgrammingTicket,
) -> Result<u64, VirtioError> {
    if stream.is_empty() {
        return Err(VirtioError::DeviceError);
    }
    reap_parked(adapter);
    let meta = DmaBuffer::new(SUBMIT_META_BYTES).ok_or(VirtioError::OutOfMemory)?;
    let mut venus = DmaBuffer::new(stream.len()).ok_or(VirtioError::OutOfMemory)?;
    venus.as_mut_slice()[..stream.len()].copy_from_slice(stream);
    let venus_len = stream.len();
    // One construction site, on the adapter, so all four pointers necessarily
    // come from the same adapter; and `enqueue_scanout_submit` is the only way
    // to attach it, so it necessarily lands on the ring the drain honours.
    let notify = adapter.scanout_notify(primary_address, ticket);

    let queued = adapter.with_virtio(move |v| {
        v.drain_used();
        v.enqueue_scanout_submit(ctx_id, meta, venus, venus_len, notify)
    });
    match queued {
        Ok(Ok(fence_id)) => Ok(fence_id),
        Ok(Err((_meta, _venus, e))) => Err(e),
        Err(_) => Err(VirtioError::DeviceError),
    }
}

/// Nonblocking KMD Present-BLT submission.
///
/// Like the scanout copy path, ring_idx=1 makes used-ring retirement represent
/// GPU completion. Unlike scanout, an ordinary app/DWM BLT must not mark the
/// physical scanout dirty or wake the display refresh worker.
pub fn submit_venus_async_present(
    adapter: &AdapterContext,
    ctx_id: u32,
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

    let queued = adapter.with_virtio(move |v| {
        v.drain_used();
        v.enqueue_async_submit(ctx_id, crate::virtio::gpu::SCANOUT_RING_IDX, meta, venus, venus_len)
    });
    match queued {
        Ok(Ok(fence_id)) => Ok(fence_id),
        Ok(Err((_meta, _venus, e))) => Err(e),
        Err(_) => Err(VirtioError::DeviceError),
    }
}

/// Outcome of a [`wait_fence`] call.
///
/// ⚠ The escape boundary (`ddi/escape.rs`'s `escape_wait_fence`) must keep
/// matching every variant explicitly, with **no wildcard arm**: today it maps
/// three variants to three statuses, so adding a variant is a compile error
/// there instead of a silent collapse into `TimedOut`. That is the whole
/// encoding — `#[non_exhaustive]` is deliberately NOT used, because it only
/// affects downstream crates and would claim a guarantee it cannot provide
/// inside this one.
///
/// A fourth variant also needs a paired ICD change: `escape_wait_fence` reports
/// only `out_completed` 1/0 plus `STATUS_INVALID_PARAMETER`, so a third state
/// has nowhere to go on the wire.
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
    // Scoped exactly as in `ctrl_roundtrip`. Every `return` below is a return
    // from the closure, and the deregistration pairing is unchanged: the four
    // early exits in the registration loop all happen BEFORE `Registered`, so
    // there is no waiter to cancel, and both post-registration exits call
    // `fence_wait_cancel`.
    SyncWaitBlock::with(|block| {
        let mut full_retries = 0u32;
        loop {
            let prep = adapter.with_virtio(|v| {
                v.drain_used();
                v.fence_wait_prepare(fence_id, block.as_ptr())
            });
            match prep {
                Err(_) => return WaitFenceOutcome::Invalid, // transport gone
                Ok(FenceWaitPrep::Complete) => return WaitFenceOutcome::Complete,
                Ok(FenceWaitPrep::Invalid) => return WaitFenceOutcome::Invalid,
                Ok(FenceWaitPrep::TableFull) => {
                    full_retries += 1;
                    if full_retries > 1_000 {
                        // NOT FENCE_WAIT_TIMEOUTS: the host may be perfectly
                        // healthy and all MAX_FENCE_WAITERS slots simply occupied.
                        // The outcome stays TimedOut so the ICD is untouched; only
                        // the evidence is split. Note the budget is nominally 1 s
                        // but KeDelayExecutionThread rounds a 1 ms relative timeout
                        // up to the system timer granularity (~15.6 ms), so this is
                        // up to ~16 s of thread residency.
                        FENCE_WAIT_TABLE_FULL.fetch_add(1, Ordering::Relaxed);
                        return WaitFenceOutcome::TimedOut;
                    }
                    sleep_ms(1);
                }
                Ok(FenceWaitPrep::Registered) => break,
            }
        }

        if timeout_ns == 0 {
            // Poll: deregister immediately; completion may still have raced in.
            return match adapter.with_virtio(|v| v.fence_wait_cancel(block.as_ptr())) {
                Ok(true) => WaitFenceOutcome::Complete,
                Ok(false) => WaitFenceOutcome::TimedOut,
                // Transport gone: the fence did NOT retire. Reporting Complete here
                // made escape_wait_fence write out_completed = 1 and return
                // STATUS_SUCCESS for an unretired wire fence - a direct violation of
                // "never signal a wire fence before host completion". Invalid is
                // already mapped to STATUS_INVALID_PARAMETER and already handled by
                // the ICD.
                Err(_) => {
                    TRANSPORT_GONE_AT_WAIT.fetch_add(1, Ordering::Relaxed);
                    WaitFenceOutcome::Invalid
                }
            };
        }

        let total_ms = (timeout_ns / 1_000_000).max(1).min(WAIT_FENCE_MAX_MS);
        if wait_block(adapter, block, total_ms) {
            return WaitFenceOutcome::Complete;
        }
        match adapter.with_virtio(|v| {
            v.drain_used();
            v.fence_wait_cancel(block.as_ptr())
        }) {
            Ok(true) => WaitFenceOutcome::Complete,
            Ok(false) => {
                FENCE_WAIT_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
                WaitFenceOutcome::TimedOut
            }
            // As in the poll exit above.
            Err(_) => {
                TRANSPORT_GONE_AT_WAIT.fetch_add(1, Ordering::Relaxed);
                WaitFenceOutcome::Invalid
            }
        }
    })
}
