//! `DxgkDdiEscape` — out-of-band ICD → KMD channel (Phase 3, M3.3 → C3/M3.4).
//!
//! The user-mode Vulkan ICD reaches the KMD through `D3DKMTEscape`, not through
//! the WDDM command/GPU-VA path. Every escape buffer begins with a
//! [`HeliosEscapeHeader`] (`helios_protocol::escape`); we validate it and
//! dispatch the Venus control verbs.
//!
//! C3/M3.4: SUBMIT_VENUS is ASYNC — it queues the stream, writes the assigned
//! wire fence id back into the escape buffer, and returns; WAIT_FENCE is a
//! real PASSIVE KEVENT wait on that wire id. All other verbs are synchronous
//! flows through `virtio::ctrl` (PASSIVE waits — never a DISPATCH spin under
//! the device spinlock).
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
    HeliosEscapeCtxDestroy, HeliosEscapeFenceEvent, HeliosEscapeHeader, HeliosEscapeMapBlob,
    HeliosEscapeMapReadLedger, HeliosEscapeQueryScanout, HeliosEscapeQueryStats,
    HeliosEscapeQueryStatsV2, HeliosEscapeQueryStatsV3, HeliosEscapeQueryStatsV4,
    HeliosEscapeReleaseBlob,
    HeliosEscapeScanoutEvent, HeliosEscapeSubmitVenus, HeliosEscapeWaitFence,
    HeliosEscapePresentStream,
    HeliosEscapeWaitFenceLegacy, HELIOS_ESCAPE_ALLOC_BLOB, HELIOS_ESCAPE_ATTACH_RESOURCE,
    HELIOS_ESCAPE_CTX_CREATE, HELIOS_ESCAPE_CTX_DESTROY, HELIOS_ESCAPE_MAP_BLOB,
    HELIOS_ESCAPE_MAP_READ_LEDGER, HELIOS_ESCAPE_QUERY_SCANOUT, HELIOS_ESCAPE_QUERY_STATS,
    HELIOS_ESCAPE_PRESENT_STREAM,
    HELIOS_ESCAPE_REGISTER_FENCE_EVENT, HELIOS_ESCAPE_RELEASE_BLOB, HELIOS_ESCAPE_SCANOUT_EVENT,
    HELIOS_ESCAPE_SUBMIT_VENUS, HELIOS_ESCAPE_UNREGISTER_FENCE_EVENT, HELIOS_ESCAPE_WAIT_FENCE,
    HELIOS_FENCE_EVENT_ALREADY_COMPLETE, HELIOS_FENCE_EVENT_CANCELLED,
    HELIOS_FENCE_EVENT_NOT_FOUND, HELIOS_FENCE_EVENT_PROBE_ACK, HELIOS_FENCE_EVENT_REGISTERED,
    HELIOS_SCANOUT_ACQ_NOT_FOUND, HELIOS_SCANOUT_ACQ_OK, HELIOS_SCANOUT_ACQ_OP_MAP,
    HELIOS_SCANOUT_ACQ_OP_PROBE, HELIOS_SCANOUT_ACQ_OP_REGISTER, HELIOS_SCANOUT_ACQ_OP_UNMAP,
    HELIOS_SCANOUT_ACQ_OP_UNREGISTER, HELIOS_SCANOUT_ACQ_PROBE_ACK,
    HELIOS_SCANOUT_ACQ_TABLE_FULL, HELIOS_SCANOUT_CAP_READ_LEDGER,
    HELIOS_SCANOUT_CAP_SNAPSHOT_BIND, HELIOS_SCANOUT_CAP_ASYNC_PRESENT_STREAM,
    HELIOS_PRESENT_STREAM_OP_REGISTER, HELIOS_PRESENT_STREAM_OP_UNREGISTER,
};

use super::blob_map::{
    effective_map_cache, map_cache_to_mm, map_io_pages_to_user,
    map_nonpaged_page_to_user_readonly, unmap_io_pages_from_user,
};
use crate::adapter::AdapterContext;
use crate::dxgk::*;
use crate::irql::PassiveLevel;
use crate::virtio::ctrl;
use crate::virtio::gpu::{DeviceOwner, OwnerFilter};

/// Ownership-bearing escape verbs refused because `hDevice` was NULL
/// (registry-visible as `EscNoDev`).
///
/// NULL collides with the kernel's owner-0 "KMD-owned" sentinel, so accepting it
/// let a caller name blob slots the KMD adopted for live WDDM allocations — the
/// DWM primary and every shared UMD surface. No in-tree caller does this: the
/// Mesa ICD always sets `esc.hDevice`, and dxgkrnl resolves a non-zero handle in
/// the caller's own process handle table, so it cannot be forged across
/// processes. This must read 0 in normal operation.
static ESCAPE_NO_DEVICE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Escape refusals that used to be silent. The project rule is that every
/// skipped or refused path gets a named counter; without these an ICD/KMD
/// protocol skew was invisible in QUERY_STATS — the unknown-verb arm, the
/// magic/version/size rejection and all twelve short-buffer arms counted
/// nothing (k-capsescape-10).
pub(crate) static ESCAPE_BAD_HEADER: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);
pub(crate) static ESCAPE_UNKNOWN_VERB: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);
pub(crate) static ESCAPE_SHORT_BUFFER: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);
/// Verbs that failed because the transport was gone, rather than fabricating a
/// content answer that reads as "nothing published yet".
pub(crate) static ESCAPE_DEVICE_GONE: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

/// Refuse a verb whose caller buffer is shorter than the payload it needs.
/// One place, so the twelve arms cannot drift.
fn refuse_short_buffer() -> NTSTATUS {
    ESCAPE_SHORT_BUFFER.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    STATUS_BUFFER_TOO_SMALL
}

/// A guest escape buffer, bound to the wire struct `T` that describes it.
///
/// # Why this exists
///
/// The peer is a separately-built binary — `vn_renderer_helios.c` carries
/// hand-written C copies of every struct in `helios_protocol::escape`, and their
/// only cross-language guard is a size `_Static_assert`. Field-order drift is
/// caught by neither compiler. Four magic-value mechanisms stood in for a
/// version field that already exists:
///
///   * `buf[16..24]` / `buf[24..32]` re-encoded `HeliosEscapeWaitFence`'s layout
///     as literal offsets. Insert a field before `timeout_ns`, or reorder the C
///     copy, and the size asserts still pass while the KMD waits on a garbage
///     timeout with no diagnostic.
///   * `legacy = buf.len() < sz` and `v2 = buf.len() >= sz2` made the runtime's
///     BUFFER LENGTH the protocol version rather than the caller's declared
///     struct size, so any caller whose scratch buffer happened to be large
///     enough was served a newer layout than it declared.
///   * `hdr.size`, validated once in the dispatcher, was then never consulted by
///     any arm — the only per-message declared length was dead.
///
/// `EscapeBuf` makes the struct definition the only layout authority: an arm
/// touches the guest buffer through `read`/`write_back` or not at all, and both
/// bounds — what the caller DECLARED (`hdr.size`) and what the runtime actually
/// supplied (`buf.len()`) — are checked once, at construction.
///
/// # The SUBMIT_VENUS exception
///
/// `SUBMIT_VENUS` sets `hdr.size = sizeof(struct helios_escape_submit_venus)` =
/// 40 while `PrivateDriverDataSize` covers 40 PLUS the Venus command stream. So
/// [`Self::trailing`] is bounded by `buf.len()`, NEVER by `hdr.size` — bounding
/// the stream on the declared header size breaks every submit.
struct EscapeBuf<'a, T: bytemuck::Pod> {
    buf: &'a mut [u8],
    _wire: core::marker::PhantomData<T>,
}

impl<'a, T: bytemuck::Pod> EscapeBuf<'a, T> {
    /// Bind `buf` to `T`, requiring room for `T` in BOTH the declared and the
    /// actual length.
    ///
    /// The `hdr.size` half is a new refusal condition, and a no-op for every
    /// known sender: the ICD's `helios_hdr_init` sets `hdr.size = sizeof(req)`
    /// for every verb (`vn_renderer_helios.c:1534`). A caller that declares less
    /// than it asks the KMD to read is refused rather than served.
    fn new(buf: &'a mut [u8], hdr: &HeliosEscapeHeader) -> Result<Self, NTSTATUS> {
        if buf.len() < size_of::<T>() || (hdr.size as usize) < size_of::<T>() {
            return Err(refuse_short_buffer());
        }
        Ok(Self {
            buf,
            _wire: core::marker::PhantomData,
        })
    }

    /// The request, read unaligned — the guest buffer carries no alignment
    /// guarantee.
    fn read(&self) -> T {
        pod_read_unaligned(&self.buf[..size_of::<T>()])
    }

    /// Write the reply back over the request.
    fn write_back(&mut self, value: &T) {
        self.buf[..size_of::<T>()].copy_from_slice(bytes_of(value));
    }

    /// Everything after `T`, bounded by the length the RUNTIME supplied.
    ///
    /// This is SUBMIT_VENUS's command stream. See the type docs for why the
    /// bound is `buf.len()` and not `hdr.size`.
    fn trailing(&self) -> &[u8] {
        &self.buf[size_of::<T>()..]
    }
}

/// The transport is gone (StopDevice tore it down). A real device-lost answer:
/// `with_virtio` returns `NotStarted`, which maps to
/// STATUS_DEVICE_DOES_NOT_EXIST — not a content answer that reads as "nothing
/// published yet".
fn escape_device_gone(de: crate::error::NotStarted) -> NTSTATUS {
    ESCAPE_DEVICE_GONE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    de.into()
}

/// `D3DDDI_ESCAPEFLAGS` bit positions, verified against the generated bitfield
/// order in `tmp/dxgk_bindings.rs:14066-14103`. `__bindgen_anon_1.Value` is the
/// union's `UINT` view, so a named mask against `.Value` is the stable,
/// layout-independent way to test a documented bit — the same convention
/// `query_driver_caps` uses for the cap unions.
///
///   bit 0 = HardwareAccess          bit 1 = DeviceStatusQuery
///   bit 2 = ChangeFrameLatency      bit 3 = NoAdapterSynchronization
const ESCAPE_FLAG_HARDWARE_ACCESS: u32 = 1 << 0;
const ESCAPE_FLAG_NO_ADAPTER_SYNC: u32 = 1 << 3;

/// Escapes seen with `HardwareAccess` / `NoAdapterSynchronization` set.
///
/// The KMD never inspected `DXGKARG_ESCAPE.Flags` at all — the "callers must use
/// HardwareAccess = 0" contract lived entirely in the Mesa ICD, behind the
/// `HELIOS_ESCAPE_HW` environment kill switch. Counting is deliberately all this
/// does for now: refusing is a behaviour change that must be evidence-gated on
/// these reading 0 across a desktop session plus a game run, and that evidence
/// can only come from the image that first counts them.
static ESCAPE_HW_ACCESS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static ESCAPE_NO_ADAPTER_SYNC: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

/// Count one flagged escape and mirror it to the registry on a bounded cadence.
///
/// `record_named*` is an UNGATED synchronous registry write and this is the
/// hottest DDI in the driver — the whole ICD command path rides it — so a
/// per-escape breadcrumb would be a per-submit registry write. First observation
/// plus every 64th, the same throttle `refuse_foreign_context` uses. Both names
/// are zeroed by `reset_fault_counters` at StartDevice, so "absent" and "0" are
/// not the same reading and the gate's verify-movement rule applies.
fn count_escape_flag(counter: &core::sync::atomic::AtomicU32, which: crate::diag::FaultCounter) {
    let n = counter.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
    if n == 1 || n % 64 == 0 {
        crate::diag::fault(which, n);
    }
}

/// QUERY_SCANOUT reads that gave up after SEQ_READ_ATTEMPTS because a publisher
/// held the descriptor throughout (registry-visible as `QsRetry`). Expected 0 in
/// steady state; it can only move under a mode-change loop.
static QUERY_SCANOUT_RETRY_GIVEUPS: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

/// Context verbs refused because the caller's device does not own the ctx_id
/// (registry-visible as `EscCtxOwn`). Must read 0: every in-tree caller uses the
/// context it created.
static ESCAPE_FOREIGN_CTX: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Refuse a context verb naming a context this device does not own.
fn refuse_foreign_context() -> NTSTATUS {
    use core::sync::atomic::Ordering;
    let n = ESCAPE_FOREIGN_CTX.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n % 64 == 0 {
        crate::diag::record_named_bytes(b"EscCtxOwn", n);
    }
    STATUS_INVALID_DEVICE_REQUEST
}

/// Refuse an ownership-bearing verb that arrived with no device handle.
fn refuse_no_device() -> NTSTATUS {
    use core::sync::atomic::Ordering;
    let n = ESCAPE_NO_DEVICE.fetch_add(1, Ordering::Relaxed) + 1;
    // PASSIVE (DxgkDdiEscape); first occurrence plus every 64th, so a caller
    // cannot turn a refusal into a registry-write storm.
    if n == 1 || n % 64 == 0 {
        crate::diag::record_named_bytes(b"EscNoDev", n);
    }
    STATUS_INVALID_PARAMETER
}

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

    // Read the flags word ONCE, at PASSIVE, outside any lock, before any
    // dispatch. Until now `pPrivateDriverData`/`PrivateDriverDataSize`/`hDevice`
    // were the only fields of `args` this DDI ever read.
    // SAFETY: `D3DDDI_ESCAPEFLAGS` is a union of a bitfield struct and a `UINT`
    // `Value`; reading the `Value` view of a plain POD union field the runtime
    // filled in is a valid read of initialized memory.
    let flags = unsafe { args.Flags.__bindgen_anon_1.Value };
    if flags & ESCAPE_FLAG_HARDWARE_ACCESS != 0 {
        count_escape_flag(&ESCAPE_HW_ACCESS, crate::diag::FaultCounter::EscHwA);
    }
    if flags & ESCAPE_FLAG_NO_ADAPTER_SYNC != 0 {
        count_escape_flag(&ESCAPE_NO_ADAPTER_SYNC, crate::diag::FaultCounter::EscNoSy);
    }

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
        ESCAPE_BAD_HEADER.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        return STATUS_INVALID_PARAMETER;
    }

    // Owner token for blob mappings: dxgkrnl passes our DeviceContext handle (the
    // one we returned from DxgkDdiCreateDevice) as `hDevice`, and hands the SAME
    // handle to DxgkDdiDestroyDevice — so a mapping tagged with it is unmapped at
    // the right time, in the creating process. Blob verbs require a device handle.
    //
    // ZERO IS NOT A NEUTRAL VALUE. It is the kernel's "KMD/allocation-owned,
    // removed from every escape reclaim path" sentinel: `adopt_blob_for_allocation`
    // re-tags a blob to the KMD owner when the KMD takes it over for a WDDM
    // allocation. `hDevice` is optional at the D3DKMTEscape API, so NULL is the
    // one owner value a caller can forge — and it used to collide with that
    // sentinel (k-capsescape-01).
    //
    // The token is minted ONCE, here, and every ownership-bearing verb takes a
    // `DeviceOwner` rather than a `usize`, so the null case has to be answered at
    // this one site and cannot reach a slot lookup at all.
    let owner = crate::virtio::gpu::DeviceOwner::new(args.hDevice as usize);

    // SAFETY: `DxgkDdiEscape` is documented "IRQL: PASSIVE_LEVEL" (WDK
    // d3dkmddi.h / DXGKDDI_ESCAPE), and it is the DDI the whole ICD command path
    // rides — every verb below either round-trips the virtio control queue or
    // waits on a wire fence, so a DISPATCH arrival here would already be a
    // deadlock rather than a new one. Counted by `IrqlBad` if that ever changes.
    let passive = unsafe { crate::irql::PassiveLevel::assume() };

    match hdr.cmd_type {
        HELIOS_ESCAPE_CTX_CREATE => match owner {
            Some(owner) => escape_ctx_create(passive, adapter, buf, &hdr, owner),
            None => refuse_no_device(),
        },
        HELIOS_ESCAPE_CTX_DESTROY => match owner {
            Some(owner) => escape_ctx_destroy(passive, adapter, buf, &hdr, owner),
            None => refuse_no_device(),
        },
        HELIOS_ESCAPE_SUBMIT_VENUS => match owner {
            Some(owner) => escape_submit_venus(passive, adapter, buf, &hdr, owner),
            None => refuse_no_device(),
        },
        HELIOS_ESCAPE_PRESENT_STREAM => match owner {
            Some(owner) => {
                // `hDevice` is the live DeviceContext whose documented
                // `hKmdProcess` association supplies registration identity.
                let creator_process = unsafe { crate::device::DeviceHandleRef::from_raw(args.hDevice) }
                    .map(|device| device.creator_process());
                match creator_process {
                    Some(process) => escape_present_stream(adapter, buf, &hdr, owner, process),
                    None => STATUS_INVALID_PARAMETER,
                }
            }
            None => refuse_no_device(),
        },
        HELIOS_ESCAPE_WAIT_FENCE => escape_wait_fence(passive, adapter, buf, &hdr),
        HELIOS_ESCAPE_ALLOC_BLOB => match owner {
            Some(owner) => escape_alloc_blob(passive, adapter, buf, &hdr, owner),
            None => refuse_no_device(),
        },
        HELIOS_ESCAPE_MAP_BLOB => match owner {
            Some(owner) => escape_map_blob(passive, adapter, buf, &hdr, owner),
            None => refuse_no_device(),
        },
        HELIOS_ESCAPE_RELEASE_BLOB => match owner {
            Some(owner) => escape_release_blob(passive, adapter, buf, &hdr, owner),
            None => refuse_no_device(),
        },
        HELIOS_ESCAPE_ATTACH_RESOURCE => escape_attach_resource(passive, adapter, buf, &hdr),
        HELIOS_ESCAPE_QUERY_STATS => escape_query_stats(adapter, buf, &hdr),
        HELIOS_ESCAPE_QUERY_SCANOUT => escape_query_scanout(adapter, buf, &hdr),
        HELIOS_ESCAPE_REGISTER_FENCE_EVENT => escape_register_fence_event(adapter, buf, &hdr),
        HELIOS_ESCAPE_UNREGISTER_FENCE_EVENT => escape_unregister_fence_event(adapter, buf, &hdr),
        // D4a scanout acquire (FIX-DESIGN-d4a.md §3.3). Ownership-bearing: the
        // ledger mapping and the event registrations are reclaimed by the
        // caller's DestroyDevice, so an owner-less request has no reclaim path
        // and is refused — the probe therefore also needs a device handle,
        // which every UMD caller (pfnEscapeCb, device-scoped) has.
        HELIOS_ESCAPE_MAP_READ_LEDGER => match owner {
            Some(owner) => escape_map_read_ledger(passive, adapter, buf, &hdr, owner),
            None => refuse_no_device(),
        },
        HELIOS_ESCAPE_SCANOUT_EVENT => match owner {
            Some(owner) => escape_scanout_event(adapter, buf, &hdr, owner),
            None => refuse_no_device(),
        },
        // Unknown verbs are rejected — and counted, because an unhandled verb is
        // how an ICD/KMD protocol skew presents (HELIOS_ESCAPE_PRESENT_BLOB
        // = 0x0007 exists in the protocol and lands here).
        _ => {
            ESCAPE_UNKNOWN_VERB.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            STATUS_NOT_IMPLEMENTED
        }
    }
}

/// Read-only snapshot of the production LINEAR primary currently published by
/// `SetVidPnSourceAddress`. The diagnostic resource is deliberately excluded:
/// consumers must never mistake color bars for the DWM copy destination.
fn escape_query_scanout(
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
) -> NTSTATUS {
    use core::sync::atomic::Ordering;

    let mut wire = match EscapeBuf::<HeliosEscapeQueryScanout>::new(buf, hdr) {
        Ok(w) => w,
        Err(st) => return st,
    };
    let mut out = wire.read();

    // SEQLOCK READ. Loading the resource id first and everything else Relaxed
    // was safe only against a FIRST publish; a republish landing between the
    // loads returned generation N's id with generation N+1's
    // pitch/plane_offset/memory_type, and out_generation (read last, Relaxed)
    // could not expose the tear. The retry is BOUNDED — this reader is a PASSIVE
    // escape but the publishers can run at raised IRQL on the VidPn path, so an
    // unbounded spin would be a new wedge class.
    let mut snapshot = None;
    for _ in 0..helios_kmd_logic::SEQ_READ_ATTEMPTS {
        let before = adapter.primary_scanout_seq.load(Ordering::Acquire);
        let resource_id = adapter.primary_scanout_resource.load(Ordering::Relaxed);
        let wh = adapter.primary_scanout_wh.load(Ordering::Relaxed);
        let layout = adapter.primary_scanout_layout.load(Ordering::Relaxed);
        let alloc_size = adapter.primary_scanout_alloc_size.load(Ordering::Relaxed);
        let dxgi_format = adapter.primary_scanout_dxgi_format.load(Ordering::Relaxed);
        let memory_type = adapter.primary_scanout_memory_type.load(Ordering::Relaxed);
        let generation = adapter.primary_scanout_generation.load(Ordering::Relaxed);
        let after = adapter.primary_scanout_seq.load(Ordering::Acquire);
        if helios_kmd_logic::seq_read(before, after) == helios_kmd_logic::SeqRead::Stable {
            snapshot = Some((
                resource_id,
                wh,
                layout,
                alloc_size,
                dxgi_format,
                memory_type,
                generation,
            ));
            break;
        }
    }
    let Some((resource_id, wh, layout, alloc_size, dxgi_format, memory_type, generation)) =
        snapshot
    else {
        // A publisher held the descriptor for every attempt. Report "no primary"
        // rather than a torn one; the consumer polls.
        let n = QUERY_SCANOUT_RETRY_GIVEUPS.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 || n % 64 == 0 {
            crate::diag::record_named_bytes(b"QsRetry", n);
        }
        return STATUS_DEVICE_BUSY;
    };

    // A torn-down transport is DEVICE-LOST, not "no primary published yet".
    // Reporting the latter with STATUS_SUCCESS made the consumer keep polling
    // instead of surfacing the real failing stage.
    let live = if resource_id == 0 {
        false
    } else {
        match adapter.with_virtio(|v| v.resource_is_live(resource_id)) {
            Ok(live) => live,
            Err(de) => return escape_device_gone(de),
        }
    };
    if !live {
        out.out_alloc_size = 0;
        out.out_resource_id = 0;
        out.out_width = 0;
        out.out_height = 0;
        out.out_dxgi_format = 0;
        out.out_pitch = 0;
        out.out_plane_offset = 0;
        out.out_memory_type_index = 0;
        out.out_generation = generation;
        out.reserved = [0; 2];
        wire.write_back(&out);
        return STATUS_SUCCESS;
    }

    out.out_alloc_size = alloc_size;
    out.out_resource_id = resource_id;
    out.out_width = (wh >> 32) as u32;
    out.out_height = wh as u32;
    out.out_dxgi_format = dxgi_format;
    out.out_pitch = (layout >> 32) as u32;
    out.out_plane_offset = layout as u32;
    out.out_memory_type_index = memory_type;
    out.out_generation = generation;
    out.reserved = [0; 2];
    wire.write_back(&out);
    STATUS_SUCCESS
}

// ── Fence events (KMD 22.22.54, PSC WS2) ────────────────────────────────────
// Usermode replacement for blocking WAIT_FENCE escapes: register an event,
// wait in usermode, cancel on timeout. No thread ever parks inside an escape,
// so the dxgkrnl escape layer never convoys this process's SUBMIT_VENUS
// escapes behind a wait again (measured 24th session: 2.9 ms → µs).

/// `EVENT_MODIFY_STATE` — the only access the KMD needs (KeSetEvent).
const EVENT_MODIFY_STATE: u32 = 0x0002;
/// `UserMode` (`KPROCESSOR_MODE`) — the handle is validated against the
/// CALLER's handle table with user-mode access checks (trust boundary).
const USER_MODE: i8 = 1;

extern "C" {
    /// `extern POBJECT_TYPE *ExEventObjectType;` (wdm.h) — the executive event
    /// object type, for ObReferenceObjectByHandle type validation. Not in the
    /// wdk-sys ntddk bindings (data export, ntoskrnl.lib), so declared here.
    static ExEventObjectType: *mut wdk_sys::POBJECT_TYPE;
}

/// Resolve a guest-supplied event handle to a referenced KEVENT. PASSIVE, in
/// the calling process (DxgkDdiEscape runs in the caller's context — the same
/// contract MAP_BLOB relies on). Returns `None` (counted) on any failure.
fn reference_user_event(event_handle: u64) -> Option<core::ptr::NonNull<wdk_sys::KEVENT>> {
    use core::sync::atomic::Ordering;
    let mut object: wdk_sys::PVOID = core::ptr::null_mut();
    // SAFETY: PASSIVE_LEVEL escape in the caller's process. UserMode access
    // mode makes the object manager validate the handle, its type
    // (ExEventObjectType) and EVENT_MODIFY_STATE access; on success we hold a
    // reference that keeps the KEVENT alive until we deref it.
    let status = unsafe {
        wdk_sys::ntddk::ObReferenceObjectByHandle(
            event_handle as wdk_sys::HANDLE,
            EVENT_MODIFY_STATE,
            *ExEventObjectType,
            USER_MODE,
            &mut object,
            core::ptr::null_mut(),
        )
    };
    if status != STATUS_SUCCESS || object.is_null() {
        crate::virtio::gpu::FENCE_EVENT_INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    core::ptr::NonNull::new(object as *mut wdk_sys::KEVENT)
}

/// Drop an event reference at PASSIVE (registration-failure / unregister
/// paths; the DISPATCH drain path uses ObDereferenceObjectDeferDelete).
fn dereference_user_event(event: core::ptr::NonNull<wdk_sys::KEVENT>) {
    // SAFETY: `event` holds a reference we own; PASSIVE_LEVEL.
    unsafe { wdk_sys::ntddk::ObfDereferenceObject(event.as_ptr() as wdk_sys::PVOID) };
}

/// `HELIOS_ESCAPE_REGISTER_FENCE_EVENT` — park a usermode event for one-shot
/// signaling at wire-fence retirement. Non-blocking. `fence_id == 0 &&
/// event_handle == 0` is the capability probe (PROBE_ACK; old KMDs fail the
/// escape with STATUS_NOT_IMPLEMENTED at the dispatcher).
fn escape_register_fence_event(
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
) -> NTSTATUS {
    use crate::virtio::gpu::FenceEventReg;
    use core::sync::atomic::Ordering;

    let mut wire = match EscapeBuf::<HeliosEscapeFenceEvent>::new(buf, hdr) {
        Ok(w) => w,
        Err(st) => return st,
    };
    let req = wire.read();
    let mut write_state = |state: u32| {
        let mut out = req;
        out.out_state = state;
        wire.write_back(&out);
    };

    if req.fence_id == 0 && req.event_handle == 0 {
        write_state(HELIOS_FENCE_EVENT_PROBE_ACK);
        return STATUS_SUCCESS;
    }
    if req.fence_id == 0 || req.event_handle == 0 {
        crate::virtio::gpu::FENCE_EVENT_INVALID.fetch_add(1, Ordering::Relaxed);
        return STATUS_INVALID_PARAMETER;
    }
    let Some(event) = reference_user_event(req.event_handle) else {
        return STATUS_INVALID_PARAMETER;
    };

    // The completion check and the table insert are one atomic step against
    // the retirement drain (device spinlock) — no lost-wakeup window.
    let reg = adapter.with_virtio(|v| v.fence_event_register(req.fence_id, event));
    match reg {
        Ok(FenceEventReg::Registered) => {
            // The table now owns the reference; the drain signals + derefs.
            write_state(HELIOS_FENCE_EVENT_REGISTERED);
            STATUS_SUCCESS
        }
        Ok(FenceEventReg::AlreadyComplete) => {
            // Signal-or-report immediately: do both, so even a caller that
            // skips out_state cannot miss the wakeup. PASSIVE KeSetEvent.
            // SAFETY: we still own the reference; the KEVENT is live.
            unsafe { wdk_sys::ntddk::KeSetEvent(event.as_ptr(), 0, 0) };
            dereference_user_event(event);
            write_state(HELIOS_FENCE_EVENT_ALREADY_COMPLETE);
            STATUS_SUCCESS
        }
        Ok(FenceEventReg::Invalid) => {
            dereference_user_event(event);
            crate::virtio::gpu::FENCE_EVENT_INVALID.fetch_add(1, Ordering::Relaxed);
            STATUS_INVALID_PARAMETER
        }
        Ok(FenceEventReg::TableFull) => {
            // Counted in fence_event_register; the ICD falls back to the
            // blocking-escape wait for this one.
            dereference_user_event(event);
            STATUS_INSUFFICIENT_RESOURCES
        }
        Ok(FenceEventReg::Duplicate) => {
            dereference_user_event(event);
            STATUS_INVALID_DEVICE_REQUEST
        }
        Err(de) => {
            dereference_user_event(event);
            de.into()
        }
    }
}

/// `HELIOS_ESCAPE_UNREGISTER_FENCE_EVENT` — cancel a parked registration after
/// a usermode wait timeout. CANCELLED = removed (the KMD will not signal);
/// NOT_FOUND = the drain consumed it (event signaled) or nothing was parked —
/// the caller disambiguates by the event's own state, so a teardown-purged
/// registration (unsignaled event) reads as failure, never fake completion.
fn escape_unregister_fence_event(
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
) -> NTSTATUS {
    use core::sync::atomic::Ordering;

    let mut wire = match EscapeBuf::<HeliosEscapeFenceEvent>::new(buf, hdr) {
        Ok(w) => w,
        Err(st) => return st,
    };
    let req = wire.read();
    if req.fence_id == 0 || req.event_handle == 0 {
        crate::virtio::gpu::FENCE_EVENT_INVALID.fetch_add(1, Ordering::Relaxed);
        return STATUS_INVALID_PARAMETER;
    }
    let Some(event) = reference_user_event(req.event_handle) else {
        return STATUS_INVALID_PARAMETER;
    };

    // Same class: NOT_FOUND is documented as "the drain consumed it", so a
    // teardown must not be reported through it.
    let removed = match adapter.with_virtio(|v| v.fence_event_unregister(req.fence_id, event)) {
        Ok(removed) => removed,
        Err(de) => {
            dereference_user_event(event);
            return escape_device_gone(de);
        }
    };
    if removed {
        // The table's reference transfers back to us: drop it plus our lookup
        // reference.
        dereference_user_event(event);
    }
    dereference_user_event(event);

    let mut out = req;
    out.out_state = if removed {
        HELIOS_FENCE_EVENT_CANCELLED
    } else {
        HELIOS_FENCE_EVENT_NOT_FOUND
    };
    wire.write_back(&out);
    STATUS_SUCCESS
}

// ── D4a scanout acquire (FIX-DESIGN-d4a.md §3.3) ────────────────────────────
// Two PASSIVE, non-blocking verbs: map the read-only ledger page, and park a
// PERSISTENT retirement event. Probe-ack idiom on both — an old KMD fails the
// escape STATUS_NOT_IMPLEMENTED from the unknown-verb arm, and that is the
// capability signal the UMD latches the feature OFF on.

/// Count one MAP_READ_LEDGER refusal (`RdMapF`) and return the given status.
fn refuse_read_ledger_map(status: NTSTATUS) -> NTSTATUS {
    crate::adapter::RD_MAP_REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    status
}

/// Count one scanout-event refusal (`AqRgF`) and return the given status.
fn refuse_scanout_event(status: NTSTATUS) -> NTSTATUS {
    crate::adapter::AQ_REGISTER_REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    status
}

/// `HELIOS_ESCAPE_MAP_READ_LEDGER` — map the D4a read-ledger page READ-ONLY
/// into the calling process (op MAP), drop that mapping (op UNMAP), or answer
/// the capability probe (op PROBE).
///
/// The mapping rides the owner-keyed [`crate::mapping::MappingTable`] under
/// [`crate::mapping::READ_LEDGER_MAPPING_ID`], so process-death reclaim is the
/// existing `DxgkDdiDestroyDevice` drain — nothing new can leak. One mapping
/// per device: a repeat MAP returns the recorded VA rather than a second view.
fn escape_map_read_ledger(
    _passive: PassiveLevel,
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
    owner: DeviceOwner,
) -> NTSTATUS {
    // `_passive` is the precondition `map_nonpaged_page_to_user_readonly` and
    // `unmap_io_pages_from_user` state (PASSIVE, caller's process); nothing in
    // this body forwards it.
    let mut wire = match EscapeBuf::<HeliosEscapeMapReadLedger>::new(buf, hdr) {
        Ok(w) => w,
        Err(st) => return st,
    };
    let req = wire.read();
    let mut reply = |user_va: u64, size: u32, state: u32| {
        let mut out = req;
        out.out_user_va = user_va;
        out.out_size = size;
        out.out_state = state;
        wire.write_back(&out);
        STATUS_SUCCESS
    };

    match req.op {
        // PROBE's `out_size` is the capability bitmask (D4b): READ_LEDGER for
        // the page this escape maps, SNAPSHOT_BIND for honoring
        // `HELIOS_PRESENT_PRIVATE_FLAG_SNAPSHOT`. A .222 KMD replied 0 here —
        // the ACK alone reads as "read ledger only", so skew is safe in both
        // directions and the UMD never substitutes against a KMD without the
        // bit.
        HELIOS_SCANOUT_ACQ_OP_PROBE => reply(
            0,
            HELIOS_SCANOUT_CAP_READ_LEDGER
                | HELIOS_SCANOUT_CAP_SNAPSHOT_BIND
                | HELIOS_SCANOUT_CAP_ASYNC_PRESENT_STREAM,
            HELIOS_SCANOUT_ACQ_PROBE_ACK,
        ),
        HELIOS_SCANOUT_ACQ_OP_MAP => {
            let Some((kernel_va, size)) = adapter.read_ledger.page_for_mapping() else {
                // StartDevice could not allocate the page (counted `RdPgF`
                // there); the feature is off for this boot and the caller must
                // latch it off too — a failing status, never a fake mapping.
                return refuse_read_ledger_map(STATUS_INSUFFICIENT_RESOURCES);
            };
            if let Some(existing) = adapter
                .mappings
                .find_user_va(owner.raw(), crate::mapping::READ_LEDGER_MAPPING_ID)
            {
                return reply(existing, size as u32, HELIOS_SCANOUT_ACQ_OK);
            }
            // SAFETY: PASSIVE escape in the caller's process; the page is a
            // page-aligned nonpaged allocation that outlives every mapping
            // (freed only in AdapterContext::drop, after DestroyDevice drained
            // this table).
            let mapped =
                unsafe { map_nonpaged_page_to_user_readonly(kernel_va as *mut u8, size as u64) };
            let Some((user_va, mdl)) = mapped else {
                return refuse_read_ledger_map(STATUS_INSUFFICIENT_RESOURCES);
            };
            match adapter.mappings.insert_unique(
                owner.raw(),
                crate::mapping::READ_LEDGER_MAPPING_ID,
                user_va,
                mdl as usize,
            ) {
                crate::mapping::InsertResult::Inserted => {
                    reply(user_va, size as u32, HELIOS_SCANOUT_ACQ_OK)
                }
                crate::mapping::InsertResult::Duplicate => {
                    // Raced a concurrent MAP on this device: keep the recorded
                    // view, undo ours.
                    // SAFETY: same process, PASSIVE; the pair we just created.
                    unsafe { unmap_io_pages_from_user(user_va, mdl) };
                    match adapter
                        .mappings
                        .find_user_va(owner.raw(), crate::mapping::READ_LEDGER_MAPPING_ID)
                    {
                        Some(existing) => reply(existing, size as u32, HELIOS_SCANOUT_ACQ_OK),
                        // The racing mapping was unmapped again meanwhile; the
                        // caller retries rather than being served a stale VA.
                        None => refuse_read_ledger_map(STATUS_DEVICE_BUSY),
                    }
                }
                crate::mapping::InsertResult::Full => {
                    // SAFETY: as above — the view must not outlive its record.
                    unsafe { unmap_io_pages_from_user(user_va, mdl) };
                    refuse_read_ledger_map(STATUS_INSUFFICIENT_RESOURCES)
                }
            }
        }
        HELIOS_SCANOUT_ACQ_OP_UNMAP => {
            match adapter
                .mappings
                .take_for_resource(owner.raw(), crate::mapping::READ_LEDGER_MAPPING_ID)
            {
                Some((user_va, mdl)) => {
                    // SAFETY: PASSIVE, in the process that created the mapping
                    // (the escape runs in the caller's context, and the owner
                    // key is that process's device handle).
                    unsafe { unmap_io_pages_from_user(user_va, mdl as wdk_sys::PMDL) };
                    reply(0, 0, HELIOS_SCANOUT_ACQ_OK)
                }
                None => reply(0, 0, HELIOS_SCANOUT_ACQ_NOT_FOUND),
            }
        }
        _ => refuse_read_ledger_map(STATUS_INVALID_PARAMETER),
    }
}

/// `HELIOS_ESCAPE_SCANOUT_EVENT` — register/unregister a PERSISTENT per-device
/// usermode auto-reset event the KMD signals on EVERY scanout-read retirement
/// (`ReadLedger::retire` broadcast), or answer the capability probe.
///
/// Clones REGISTER_FENCE_EVENT's reference discipline
/// (`ObReferenceObjectByHandle`, EVENT_MODIFY_STATE, UserMode) with the two
/// fixes that table lacks: entries are owner-tagged and reclaimed at
/// DestroyDevice (`ReadLedger::reclaim_events_for_owner`), and Stop/Start
/// signals + releases them (`ReadLedger::reset`). Unlike 0x000B this
/// registration SURVIVES its signals; the consumer is level-triggered.
fn escape_scanout_event(
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
    owner: DeviceOwner,
) -> NTSTATUS {
    use crate::adapter::ScanoutEventReg;

    let mut wire = match EscapeBuf::<HeliosEscapeScanoutEvent>::new(buf, hdr) {
        Ok(w) => w,
        Err(st) => return st,
    };
    let req = wire.read();
    let mut reply = |state: u32| {
        let mut out = req;
        out.out_state = state;
        wire.write_back(&out);
        STATUS_SUCCESS
    };

    match req.op {
        HELIOS_SCANOUT_ACQ_OP_PROBE => reply(HELIOS_SCANOUT_ACQ_PROBE_ACK),
        HELIOS_SCANOUT_ACQ_OP_REGISTER => {
            if req.event_handle == 0 {
                return refuse_scanout_event(STATUS_INVALID_PARAMETER);
            }
            // NB: a failed resolve also ticks the fence-event family's
            // `FENCE_EVENT_INVALID` inside `reference_user_event`; `AqRgF` is
            // this verb's own census.
            let Some(event) = reference_user_event(req.event_handle) else {
                return refuse_scanout_event(STATUS_INVALID_PARAMETER);
            };
            match adapter.read_ledger.register_event(owner.raw(), event) {
                ScanoutEventReg::Registered => {
                    // The table now owns the reference; retirement broadcasts
                    // signal it, and removal paths dereference it.
                    reply(HELIOS_SCANOUT_ACQ_OK)
                }
                ScanoutEventReg::AlreadyRegistered => {
                    // Idempotent: the parked reference stays, this call's goes.
                    dereference_user_event(event);
                    reply(HELIOS_SCANOUT_ACQ_OK)
                }
                ScanoutEventReg::TableFull => {
                    // Counted `AqRgF` in register_event. SUCCESS with the
                    // TABLE_FULL state: the caller reads it and runs ungated —
                    // loud (the counter), never wedged.
                    dereference_user_event(event);
                    reply(HELIOS_SCANOUT_ACQ_TABLE_FULL)
                }
            }
        }
        HELIOS_SCANOUT_ACQ_OP_UNREGISTER => {
            if req.event_handle == 0 {
                return refuse_scanout_event(STATUS_INVALID_PARAMETER);
            }
            // Resolve the handle only to IDENTIFY the object; the lookup
            // reference is dropped below either way (the fence-event
            // unregister's exact discipline).
            let Some(event) = reference_user_event(req.event_handle) else {
                return refuse_scanout_event(STATUS_INVALID_PARAMETER);
            };
            let removed = adapter.read_ledger.unregister_event(owner.raw(), event);
            dereference_user_event(event);
            reply(if removed {
                HELIOS_SCANOUT_ACQ_OK
            } else {
                HELIOS_SCANOUT_ACQ_NOT_FOUND
            })
        }
        _ => refuse_scanout_event(STATUS_INVALID_PARAMETER),
    }
}

/// `HELIOS_ESCAPE_QUERY_STATS` → read-only snapshot of the bounded resource
/// tables (occupancy under the device lock) and the DISPATCH-safe rejection /
/// high-water counters. Diagnostic observability for the 2026-07-03 blob-table
/// exhaustion class; no device state is modified. Accepts BOTH struct sizes:
/// a v1 (88-byte) caller gets the v1 fields, a v2 (22.22.54+) caller also gets
/// the fence-event table counters.
fn escape_query_stats(
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
) -> NTSTATUS {
    use core::sync::atomic::Ordering;

    use crate::virtio::gpu::{
        ADOPT_DEAD_REJECTS, BLOB_FULL_REJECTS, BLOB_HIGH_WATER, CONTEXT_FULL_DROPS,
        CTRL_TIMEOUT_COUNT, FENCE_EVENT_ALREADY_COMPLETE, FENCE_EVENT_CANCELS,
        FENCE_EVENT_DUP_REJECTS, FENCE_EVENT_HIGH_WATER, FENCE_EVENT_INVALID,
        FENCE_EVENT_OVERFLOWS, FENCE_EVENT_REGISTERS, FENCE_EVENT_SIGNALS,
        FENCE_EVENT_TEARDOWN_DROPS, PRESENT_STREAM_HIGH_WATER, PRESENT_STREAM_LIVE,
        PRESENT_STREAM_MARKERS, PRESENT_STREAM_REGISTERS, PRESENT_STREAM_REJECTS,
        PRESENT_STREAM_RETIRES, PRESENT_STREAM_TAGS, RESOURCE_FULL_REJECTS,
        RESOURCE_HIGH_WATER, TAKE_LIVE_MISSES, WINDOW_RANGE_DROPS,
    };

    let sz = size_of::<HeliosEscapeQueryStats>();
    let sz2 = size_of::<HeliosEscapeQueryStatsV2>();
    let sz3 = size_of::<HeliosEscapeQueryStatsV3>();
    let sz4 = size_of::<HeliosEscapeQueryStatsV4>();
    if buf.len() < sz {
        return refuse_short_buffer();
    }
    // VERSION SELECTION OFF `hdr.size` — the length the caller DECLARED — with
    // `buf.len()` still bounding every write, because the buffer we may touch is
    // the one the runtime actually supplied. Declared behaviour change, and a
    // no-op for every known sender (`helios_hdr_init` sets hdr.size =
    // sizeof(req)). Previously a caller that declared V1 but happened to pass a
    // buffer >= sizeof(V3) was served the V3 layout, writing fields into bytes it
    // never agreed were part of the message.
    let declared = hdr.size as usize;
    let v2 = declared >= sz2 && buf.len() >= sz2;
    let v3 = declared >= sz3 && buf.len() >= sz3;
    let v4 = declared >= sz4 && buf.len() >= sz4;
    let (stats, fence_events_live) =
        match adapter.with_virtio(|v| (v.table_stats(), v.fence_events_live())) {
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
    if !v2 {
    // NOT an EscapeBuf arm: this verb writes one of FOUR layouts over the
        // same buffer, so there is no single wire type to bind it to. The bounds
    // are the explicit `sz`/`sz2`/`sz3`/`sz4` checks above, and the version is now
        // selected by the caller's declared `hdr.size` rather than by how big a
        // scratch buffer it happened to pass.
        buf[..sz].copy_from_slice(bytes_of(&out));
        return STATUS_SUCCESS;
    }
    let mut out2: HeliosEscapeQueryStatsV2 = pod_read_unaligned(&buf[..sz2]);
    out2.v1 = out;
    out2.out_fence_events_live = fence_events_live;
    out2.out_fence_events_high_water = FENCE_EVENT_HIGH_WATER.load(Ordering::Relaxed);
    out2.out_fence_event_registers = FENCE_EVENT_REGISTERS.load(Ordering::Relaxed);
    out2.out_fence_event_signals = FENCE_EVENT_SIGNALS.load(Ordering::Relaxed);
    out2.out_fence_event_already_complete = FENCE_EVENT_ALREADY_COMPLETE.load(Ordering::Relaxed);
    out2.out_fence_event_overflows = FENCE_EVENT_OVERFLOWS.load(Ordering::Relaxed);
    out2.out_fence_event_dup_rejects = FENCE_EVENT_DUP_REJECTS.load(Ordering::Relaxed);
    out2.out_fence_event_invalid = FENCE_EVENT_INVALID.load(Ordering::Relaxed);
    out2.out_fence_event_cancels = FENCE_EVENT_CANCELS.load(Ordering::Relaxed);
    out2.out_fence_event_teardown_drops = FENCE_EVENT_TEARDOWN_DROPS.load(Ordering::Relaxed);
    out2.out_mappings_live = adapter.mappings.live();
    out2.out_mappings_cap = crate::mapping::MAX_MAPPINGS_CAP;
    out2.out_mappings_high_water = crate::mapping::MAPPINGS_HIGH_WATER.load(Ordering::Relaxed);
    out2.out_mapping_full_rejects = crate::mapping::MAPPING_FULL_REJECTS.load(Ordering::Relaxed);
    out2.out_map_pages_fails = crate::virtio::gpu::MAP_PAGES_FAILS.load(Ordering::Relaxed);
    out2.out_window_alloc_rejects =
        crate::virtio::gpu::WINDOW_ALLOC_REJECTS.load(Ordering::Relaxed);
    if !v3 {
        buf[..sz2].copy_from_slice(bytes_of(&out2));
        return STATUS_SUCCESS;
    }
    // V3 is APPENDED after v2, so a v1/v2 probe keeps parsing byte-identically.
    let mut out3: HeliosEscapeQueryStatsV3 = pod_read_unaligned(&buf[..sz3]);
    out3.v2 = out2;
    out3.out_escape_bad_header = ESCAPE_BAD_HEADER.load(Ordering::Relaxed);
    out3.out_escape_unknown_verb = ESCAPE_UNKNOWN_VERB.load(Ordering::Relaxed);
    out3.out_escape_short_buffer = ESCAPE_SHORT_BUFFER.load(Ordering::Relaxed);
    out3.out_escape_device_gone = ESCAPE_DEVICE_GONE.load(Ordering::Relaxed);
    out3.out_escape_no_device = ESCAPE_NO_DEVICE.load(Ordering::Relaxed);
    out3.out_escape_foreign_ctx = ESCAPE_FOREIGN_CTX.load(Ordering::Relaxed);
    // R315: write-only counters that no report carried. ASYNC_CTRL_RESP_ERRORS
    // is the loud-failure counter for the direct-primary display path (a
    // host-rejected SET_SCANOUT_BLOB or RESOURCE_FLUSH) and appeared nowhere.
    out3.out_async_ctrl_resp_errors =
        crate::virtio::gpu::ASYNC_CTRL_RESP_ERRORS.load(Ordering::Relaxed);
    out3.out_cpu_host_unmap_count =
        crate::ddi::cpu_host_aperture::CPU_HOST_UNMAP_COUNT.load(Ordering::Relaxed);
    out3.out_dma_alloc_fails = crate::virtio::hal::DMA_ALLOC_FAILS.load(Ordering::Relaxed);
    out3.out_mmio_map_fails = crate::virtio::hal::MMIO_MAP_FAILS.load(Ordering::Relaxed);
    out3.out_mmio_cache_full = crate::virtio::hal::MMIO_CACHE_FULL.load(Ordering::Relaxed);
    out3.out_query_scanout_retries = QUERY_SCANOUT_RETRY_GIVEUPS.load(Ordering::Relaxed);
    if !v4 {
        buf[..sz3].copy_from_slice(bytes_of(&out3));
        return STATUS_SUCCESS;
    }
    let mut out4: HeliosEscapeQueryStatsV4 = pod_read_unaligned(&buf[..sz4]);
    out4.v3 = out3;
    out4.out_present_streams_live = PRESENT_STREAM_LIVE.load(Ordering::Relaxed);
    out4.out_present_streams_cap = crate::virtio::gpu::max_present_streams() as u32;
    out4.out_present_streams_high_water = PRESENT_STREAM_HIGH_WATER.load(Ordering::Relaxed);
    out4.out_present_stream_registers = PRESENT_STREAM_REGISTERS.load(Ordering::Relaxed);
    out4.out_present_stream_tags = PRESENT_STREAM_TAGS.load(Ordering::Relaxed);
    out4.out_present_stream_markers = PRESENT_STREAM_MARKERS.load(Ordering::Relaxed);
    out4.out_present_stream_retires = PRESENT_STREAM_RETIRES.load(Ordering::Relaxed);
    out4.out_present_stream_rejects = PRESENT_STREAM_REJECTS.load(Ordering::Relaxed);
    buf[..sz4].copy_from_slice(bytes_of(&out4));
    STATUS_SUCCESS
}

/// `HELIOS_ESCAPE_CTX_CREATE` → create a Venus virtio-gpu context; write the
/// guest-assigned id back into the in/out buffer's `out_ctx_id`.
fn escape_ctx_create(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
    owner: DeviceOwner,
) -> NTSTATUS {
    let mut wire = match EscapeBuf::<HeliosEscapeCtxCreate>::new(buf, hdr) {
        Ok(w) => w,
        Err(st) => return st,
    };
    let req = wire.read();
    match ctrl::ctx_create(passive, adapter, req.capset_id, Some(owner)) {
        Ok(ctx_id) => {
            let mut out = req;
            out.out_ctx_id = ctx_id;
            wire.write_back(&out);
            STATUS_SUCCESS
        }
        Err(ve) => ve.into(),
    }
}

/// `HELIOS_ESCAPE_PRESENT_STREAM` — one-time stream lifecycle.  The register
/// reply is an opaque KMD cookie; unknown KMDs never reach this function and
/// reject the verb from the dispatcher, which is the UMD's capability gate.
fn escape_present_stream(
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
    owner: DeviceOwner,
    creator_process: usize,
) -> NTSTATUS {
    let mut wire = match EscapeBuf::<HeliosEscapePresentStream>::new(buf, hdr) {
        Ok(wire) => wire,
        Err(status) => return status,
    };
    let req = wire.read();
    match req.op {
        HELIOS_PRESENT_STREAM_OP_REGISTER => match adapter.with_virtio(|v| {
            v.register_present_stream(owner, req.ctx_id, creator_process)
        }) {
            Ok(Ok(cookie)) => {
                let mut out = req;
                out.cookie = cookie;
                wire.write_back(&out);
                STATUS_SUCCESS
            }
            Ok(Err(crate::virtio::VirtioError::NotOwned)) => refuse_foreign_context(),
            Ok(Err(error)) => error.into(),
            Err(error) => error.into(),
        },
        HELIOS_PRESENT_STREAM_OP_UNREGISTER => {
            let status = match adapter.with_wddm_notify_lock(|guard| {
                guard.with_virtio(|order, v| {
                    v.unregister_present_stream(order, owner, req.ctx_id, req.cookie)
                })
            }) {
                Ok(true) => STATUS_SUCCESS,
                // The caller must prove the exact owner+ctx+cookie tuple. Do
                // not make unregister a wildcard cleanup primitive.
                Ok(false) => STATUS_INVALID_PARAMETER,
                Err(error) => error.into(),
            };
            if status == STATUS_SUCCESS {
                crate::ddi::interrupt::request_wddm_completion_dpc(adapter);
            }
            status
        }
        _ => STATUS_INVALID_PARAMETER,
    }
}

/// `HELIOS_ESCAPE_CTX_DESTROY` → tear down a context this device owns.
fn escape_ctx_destroy(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
    owner: DeviceOwner,
) -> NTSTATUS {
    let wire = match EscapeBuf::<HeliosEscapeCtxDestroy>::new(buf, hdr) {
        Ok(w) => w,
        Err(st) => return st,
    };
    let req = wire.read();
    match ctrl::ctx_destroy(passive, adapter, Some(owner), req.ctx_id) {
        Ok(()) => STATUS_SUCCESS,
        Err(crate::virtio::VirtioError::NotOwned) => refuse_foreign_context(),
        Err(ve) => ve.into(),
    }
}

/// `HELIOS_ESCAPE_SUBMIT_VENUS` → ASYNC (C3/M3.4). The stream is the
/// `buffer_size` bytes immediately following the 40-byte payload header;
/// `virtio::ctrl` stages it into contiguous DMA memory and queues it fenced
/// with a KMD-assigned wire fence id, which is written back into the escape
/// buffer's `fence_id` for the ICD to wait on. Returns at QUEUE time — the
/// caller's ~seconds-long host round-trip no longer serializes every other
/// escape under the dxgkrnl adapter lock (the 2026-07-04 WUDFHost/IddCx
/// deadline-collision root cause).
fn escape_submit_venus(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
    owner: DeviceOwner,
) -> NTSTATUS {
    let mut wire = match EscapeBuf::<HeliosEscapeSubmitVenus>::new(buf, hdr) {
        Ok(w) => w,
        Err(st) => return st,
    };
    let req = wire.read();

    // TRUST BOUNDARY, and THE ONE PLACE `hdr.size` MUST NOT BE THE BOUND.
    // SUBMIT_VENUS sets hdr.size = sizeof(HeliosEscapeSubmitVenus) = 40 while
    // PrivateDriverDataSize covers 40 PLUS the Venus command stream, so the
    // stream is bounded by what the RUNTIME supplied — `wire.trailing()`, i.e.
    // `buf.len()`. Bounding it on the declared header size breaks every submit.
    // Reject empty payloads and any length that exceeds that trailing region.
    let stream = wire.trailing();
    let payload = req.buffer_size as usize;
    if payload == 0 || payload > stream.len() {
        return STATUS_INVALID_PARAMETER;
    }

    // `present_value32 == 0` is byte-for-byte legacy behavior: the incoming
    // fence id remains ignored and the KMD assigns a normal wire fence.  A
    // tagged submit reinterprets only the INPUT fence id as the registered
    // stream capability; writeback below still returns that same normal fence.
    let queued = if req.present_value32 == 0 {
        ctrl::submit_venus_async(
            passive,
            adapter,
            Some(owner),
            req.ctx_id,
            req.ring_idx,
            &stream[..payload],
        )
    } else {
        ctrl::submit_venus_async_present_stream(
            passive,
            adapter,
            owner,
            req.ctx_id,
            req.ring_idx,
            req.fence_id,
            req.present_value32,
            &stream[..payload],
        )
    };
    match queued {
        Ok(wire_fence) => {
            // Report the assigned wire fence id back (in/out escape buffer).
            let mut out = req;
            out.fence_id = wire_fence;
            wire.write_back(&out);
            STATUS_SUCCESS
        }
        Err(crate::virtio::VirtioError::NotOwned) => refuse_foreign_context(),
        Err(ve) => ve.into(),
    }
}

/// `HELIOS_ESCAPE_WAIT_FENCE` → REAL wait (C3/M3.4): block (PASSIVE, KEVENT)
/// until the wire fence completes on the used ring or `timeout_ns` elapses.
/// The outcome is reported in `out_completed` (1 = complete, 0 = timeout) with
/// STATUS_SUCCESS — informational NTSTATUS pass-through from DxgkDdiEscape is
/// not contractual, so the payload carries the verdict. The legacy 32-byte
/// shape (old ICD) is still accepted: it waits, but can only report a timeout
/// via a failure status.
fn escape_wait_fence(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
) -> NTSTATUS {
    // VERSION SELECTION OFF `hdr.size`, not off `buf.len()`. Declared behaviour
    // change, and a no-op for every known sender: the ICD's `helios_hdr_init`
    // sets `hdr.size = sizeof(req)` (`vn_renderer_helios.c:1534`), so a caller
    // that declares the 40-byte shape gets it and one that declares 32 gets the
    // legacy shape. Previously the runtime's BUFFER LENGTH was the protocol
    // version, so any caller whose scratch buffer happened to be >= 40 was served
    // the newer layout even when it declared the older one — and would then have
    // its `out_completed` written in bytes it never agreed were part of the
    // message.
    //
    // The literal offsets this replaces (`buf[16..24]`, `buf[24..32]`) were a
    // third copy of the struct layout, correct only by coincidence: the peer's
    // hand-written C copy has nothing but a size assert guarding its field order.
    let current = match EscapeBuf::<HeliosEscapeWaitFence>::new(buf, hdr) {
        Ok(w) => Some(w),
        Err(_) => None,
    };
    let (fence_id, timeout_ns) = match &current {
        Some(w) => {
            let req = w.read();
            (req.fence_id, req.timeout_ns)
        }
        None => {
            // Legacy 32-byte shape (old ICD): waits, but can only report a
            // timeout through a failure NTSTATUS. Read through the declared
            // prefix struct rather than byte offsets.
            let legacy = match EscapeBuf::<HeliosEscapeWaitFenceLegacy>::new(buf, hdr) {
                Ok(w) => w,
                Err(st) => return st,
            };
            let req = legacy.read();
            let outcome = ctrl::wait_fence(passive, adapter, req.fence_id, req.timeout_ns);
            return match outcome {
                ctrl::WaitFenceOutcome::Complete => STATUS_SUCCESS,
                ctrl::WaitFenceOutcome::TimedOut => wdk_sys::STATUS_IO_TIMEOUT,
                ctrl::WaitFenceOutcome::Invalid => STATUS_INVALID_PARAMETER,
            };
        }
    };

    let outcome = ctrl::wait_fence(passive, adapter, fence_id, timeout_ns);
    // `current` is Some on this path — the None arm returned above.
    let Some(mut wire) = current else {
        return STATUS_INVALID_PARAMETER;
    };
    let mut out = wire.read();
    match outcome {
        ctrl::WaitFenceOutcome::Complete => out.out_completed = 1,
        ctrl::WaitFenceOutcome::TimedOut => out.out_completed = 0,
        ctrl::WaitFenceOutcome::Invalid => return STATUS_INVALID_PARAMETER,
    }
    wire.write_back(&out);
    STATUS_SUCCESS
}

/// `HELIOS_ESCAPE_ALLOC_BLOB` → create a HOST3D virtio-gpu blob (create + attach)
/// and record its size; write the guest-assigned `out_resource_id` back.
fn escape_alloc_blob(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
    owner: DeviceOwner,
) -> NTSTATUS {
    let mut wire = match EscapeBuf::<HeliosEscapeAllocBlob>::new(buf, hdr) {
        Ok(w) => w,
        Err(st) => return st,
    };
    let req = wire.read();
    // DIAG: 0x0E04_HHHH = ALLOC_BLOB's owning handle (low 16 bits), to confirm it
    // matches the handle DxgkDdiDestroyDevice reclaims under (0x0E01_HHHH).
    crate::diag::record(0x0E04_0000 | ((owner.raw() as u32) & 0xFFFF));
    match ctrl::alloc_blob(
        passive,
        adapter,
        req.ctx_id,
        req.blob_mem,
        req.blob_flags,
        req.blob_id,
        req.size,
        Some(owner),
    ) {
        Ok(resource_id) => {
            let mut out = req;
            out.out_resource_id = resource_id;
            wire.write_back(&out);
            STATUS_SUCCESS
        }
        Err(ve) => ve.into(),
    }
}

/// `HELIOS_ESCAPE_MAP_BLOB` → map a host-visible blob's pages into the calling
/// process and return the user VA (the zero-copy BAR model). Two-phase like the
/// System-class IOCTL: `map_blob_prepare` runs the `RESOURCE_MAP_BLOB` round-trip
/// under the virtio spinlock (DISPATCH), then we build the MDL + map into user
/// space at PASSIVE_LEVEL, in this thread's (the ICD's) process. The mapping is
/// tagged with the owning device handle and unmapped at DxgkDdiDestroyDevice.
fn escape_map_blob(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
    owner: DeviceOwner,
) -> NTSTATUS {
    let mut wire = match EscapeBuf::<HeliosEscapeMapBlob>::new(buf, hdr) {
        Ok(w) => w,
        Err(st) => return st,
    };
    let req = wire.read();
    if req.resource_id == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    // Cheap pre-check: reject a second map of an already-mapped resource before
    // paying for the host round-trip. NOT authoritative — DxgkDdiEscape is not
    // serialised by dxgkrnl, so two threads on one device handle can both pass
    // here. `insert_unique` below is the answer that counts.
    if adapter.mappings.contains(owner.raw(), req.resource_id) {
        return STATUS_INVALID_DEVICE_REQUEST;
    }

    // Phase 1 — the RESOURCE_MAP_BLOB flow (PASSIVE waits in virtio::ctrl):
    // reserves a window offset, round-trips the map, returns the
    // guest-physical range + host caching.
    let prep = match ctrl::map_blob_prepare(
        passive,
        adapter,
        OwnerFilter::Exactly(Some(owner)),
        req.resource_id,
    ) {
        Ok(p) => p,
        Err(ve) => return ve.into(),
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
        None => {
            crate::virtio::gpu::MAP_PAGES_FAILS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return STATUS_INSUFFICIENT_RESOURCES;
        }
    };

    // Phase 3 — record for handle-close teardown, refusing a duplicate under the
    // same lock acquisition that inserts. Either failure undoes the view we just
    // created, exactly as the table-full path always did.
    match adapter
        .mappings
        .insert_unique(owner.raw(), req.resource_id, user_va, mdl as usize)
    {
        crate::mapping::InsertResult::Inserted => {}
        crate::mapping::InsertResult::Duplicate => {
            // A concurrent MAP_BLOB for the same (owner, resource) won the race.
            // Both threads got the SAME window offset (map_blob_prepare is
            // idempotent and blob_map_begin never re-places a mapped blob), so
            // this view is redundant, not wrong — unmap it and refuse with the
            // same status the cheap pre-check uses.
            //
            // MapDup must stay 0: a nonzero value means a legitimate ICD really
            // does map a blob twice concurrently, and that path is doing a
            // wasted map/unmap plus a host round-trip.
            crate::diag::record_named_bytes(b"MapDup", req.resource_id);
            // SAFETY: still in the owning process at PASSIVE; pair returned just above.
            unsafe { unmap_io_pages_from_user(user_va, mdl) };
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        crate::mapping::InsertResult::Full => {
            // SAFETY: still in the owning process at PASSIVE; pair returned just above.
            unsafe { unmap_io_pages_from_user(user_va, mdl) };
            return STATUS_INSUFFICIENT_RESOURCES;
        }
    }

    let mut out = req;
    out.out_user_va = user_va;
    out.map_cache = eff_cache;
    wire.write_back(&out);
    STATUS_SUCCESS
}

/// `HELIOS_ESCAPE_RELEASE_BLOB` → unmap this device's user view (if any), then
/// detach + unref the blob. Symmetric to MAP_BLOB; runs in the owning process.
fn escape_release_blob(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
    owner: DeviceOwner,
) -> NTSTATUS {
    let wire = match EscapeBuf::<HeliosEscapeReleaseBlob>::new(buf, hdr) {
        Ok(w) => w,
        Err(st) => return st,
    };
    // THE VERB THAT DESTROYS STATE: with a `usize` owner, a null hDevice matched
    // the slots the KMD adopts for live WDDM allocations — pop, unmap,
    // take_live_resource, unref, all behind the allocation's back, after which
    // DestroyAllocation finds nothing and the host logs "invalid res_id" (the
    // boot-#3 DWM kill class). `DeviceOwner` makes that unrepresentable.
    let req = wire.read();
    if req.ctx_id == 0 || req.resource_id == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    // The user VA can only be unmapped in the process/device that created it.
    if let Some((user_va, mdl)) = adapter
        .mappings
        .take_for_resource(owner.raw(), req.resource_id)
    {
        // SAFETY: PASSIVE, in the creating process; pair from a prior MAP_BLOB.
        unsafe { unmap_io_pages_from_user(user_va, mdl as wdk_sys::PMDL) };
    }
    match ctrl::release_blob_for_owner(passive, adapter, owner, req.ctx_id, req.resource_id) {
        Ok(()) => STATUS_SUCCESS,
        Err(ve) => ve.into(),
    }
}

/// `HELIOS_ESCAPE_ATTACH_RESOURCE` → attach a live resource id to a Venus context
/// without taking ownership. Used by the DXVK/Mesa shared-resource import path:
/// the resource was created by another device/context and must be visible in the
/// importing context before `VkImportMemoryResourceInfoMESA` reaches virglrenderer.
fn escape_attach_resource(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
) -> NTSTATUS {
    let wire = match EscapeBuf::<HeliosEscapeAttachResource>::new(buf, hdr) {
        Ok(w) => w,
        Err(st) => return st,
    };
    let req = wire.read();
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
    match ctrl::attach_resource_checked(passive, adapter, req.ctx_id, req.resource_id) {
        Ok(()) => STATUS_SUCCESS,
        Err(ve) => ve.into(),
    }
}
