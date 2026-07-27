//! Helios IOCTL payload structs (ARCH.md §3; TRANSPORT.md §3).
//!
//! These are the in/out buffer layouts the ICD passes to `DeviceIoControl` on
//! `GUID_DEVINTERFACE_HELIOS`; the IOCTL control code (see [`crate::ioctl`]) is
//! the verb, and the KMD's `EvtIoDeviceControl` validates the WDF-reported
//! buffer lengths against these sizes before reading. All payload structs are
//! `repr(C)`, padding-free (so they derive `Pod`/`Zeroable`), and laid out
//! 8-byte-aligned-first to avoid implicit padding.
//!
//! HISTORICAL NAMING: the `HeliosEscape*` type names and the `HeliosEscapeHeader`
//! date from the abandoned WDDM `D3DKMTEscape` carrier (see ARCH.md §0). The wire
//! layout is unchanged across the pivot, so the names are kept to avoid churn in
//! a byte-ABI crate; the header is now an optional sanity check (the IOCTL code
//! already identifies the verb and WDF validates lengths).
//!
//! NOTE: the field *order* of [`HeliosEscapeSubmitVenus`] differs from the
//! sketch in TRANSPORT.md §3.1 — the 64-bit `fence_id` is placed first so the
//! struct has no implicit padding and is safely `Pod`. This is our own private
//! protocol (not an external ABI), so a clean layout is preferable to matching
//! the doc's sketch verbatim.

use bytemuck::{Pod, Zeroable};

/// `'HELS'` — sanity magic at the start of every escape buffer.
pub const HELIOS_ESCAPE_MAGIC: u32 = 0x4845_4C53;
/// Current escape protocol version.
pub const HELIOS_ESCAPE_VERSION: u32 = 1;

pub const HELIOS_ESCAPE_SUBMIT_VENUS: u32 = 0x0001;
pub const HELIOS_ESCAPE_CTX_CREATE: u32 = 0x0002;
pub const HELIOS_ESCAPE_CTX_DESTROY: u32 = 0x0003;
pub const HELIOS_ESCAPE_ALLOC_BLOB: u32 = 0x0004;
pub const HELIOS_ESCAPE_MAP_BLOB: u32 = 0x0005;
pub const HELIOS_ESCAPE_WAIT_FENCE: u32 = 0x0006;
/// Throwaway Phase-7 go/no-go gate op (DISPLAY.md §8) — present a venus blob
/// resource on scanout 0 for zero-copy display. Not part of the steady-state
/// protocol; removed once the DOD's `HELIOS_PRESENT_BLOB` escape supersedes it.
pub const HELIOS_ESCAPE_PRESENT_BLOB: u32 = 0x0007;
pub const HELIOS_ESCAPE_RELEASE_BLOB: u32 = 0x0008;
pub const HELIOS_ESCAPE_ATTACH_RESOURCE: u32 = 0x0009;
/// Read-only KMD resource-table statistics (blob/resource/context table
/// occupancy, high-waters, rejection counters, host-visible-window usage).
/// Diagnostic observability for the bounded-table exhaustion class
/// (2026-07-03: MAX_BLOBS=256 filled → every new venus ring/shmem/export
/// alloc failed guest-side with STATUS_INSUFFICIENT_RESOURCES).
pub const HELIOS_ESCAPE_QUERY_STATS: u32 = 0x000A;
/// Register a usermode event to be signaled when a wire fence retires
/// (KMD 22.22.54+, PSC WS2: kills the escape-park convoy — no thread ever
/// blocks inside an escape again). Non-blocking; see [`HeliosEscapeFenceEvent`].
pub const HELIOS_ESCAPE_REGISTER_FENCE_EVENT: u32 = 0x000B;
/// Cancel a [`HELIOS_ESCAPE_REGISTER_FENCE_EVENT`] registration that has not
/// signaled yet (wait timed out usermode-side). See [`HeliosEscapeFenceEvent`].
pub const HELIOS_ESCAPE_UNREGISTER_FENCE_EVENT: u32 = 0x000C;
/// Query the optional KMD-owned LINEAR fallback currently bound as the VidPn
/// primary. The DWM UMD may import it as a transfer-only destination when a
/// directly exportable primary is unavailable.
pub const HELIOS_ESCAPE_QUERY_SCANOUT: u32 = 0x000D;

/// Header for all escape commands. 16 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapeHeader {
    pub magic: u32,    // == HELIOS_ESCAPE_MAGIC
    pub cmd_type: u32, // one of HELIOS_ESCAPE_*
    pub version: u32,  // == HELIOS_ESCAPE_VERSION
    pub size: u32,     // total escape buffer size in bytes (header + payload + data)
}

impl HeliosEscapeHeader {
    pub const fn new(cmd_type: u32, size: u32) -> Self {
        Self {
            magic: HELIOS_ESCAPE_MAGIC,
            cmd_type,
            version: HELIOS_ESCAPE_VERSION,
            size,
        }
    }

    /// Validate magic + version. The KMD calls this before trusting `cmd_type`.
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.magic == HELIOS_ESCAPE_MAGIC && self.version == HELIOS_ESCAPE_VERSION
    }
}

/// `HELIOS_ESCAPE_SUBMIT_VENUS` — followed by `buffer_size` bytes of Venus
/// command stream. 40 bytes (header included).
///
/// `ring_idx` is the venus per-queue host timeline this submission targets (0 =
/// the CPU/primary ring). The KMD forwards it as `VIRTIO_GPU_FLAG_INFO_RING_IDX`
/// + `ctrl_hdr.ring_idx` on the SUBMIT_3D so the host routes the fence to the
/// matching context+ring timeline (`virgl_renderer_context_create_fence`) — which
/// is what venus waits on for a queue (vkQueueWaitIdle). Without it the host
/// signals only the global fence and the per-queue wait never completes.
///
/// C3/M3.4 async transport: `fence_id` is IN/OUT. The caller's value is
/// ignored (kept for wire compat with the old synchronous KMD, which forwarded
/// it verbatim); the KMD assigns a globally-monotonic WIRE fence id, submits
/// the venus stream fenced with it, and writes it back here. The escape
/// returns at QUEUE time — completion is observed via `WAIT_FENCE` on the
/// returned wire id.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapeSubmitVenus {
    pub hdr: HeliosEscapeHeader,
    /// in: ICD-local fence id (legacy KMD forwards it) / out: KMD-assigned wire
    /// fence id (async KMD overwrites it — wait on THIS value).
    pub fence_id: u64,
    pub ctx_id: u32,
    pub buffer_size: u32,
    pub ring_idx: u32,
    pub _pad: u32,
}

/// `HELIOS_ESCAPE_CTX_CREATE`. The KMD fills `out_ctx_id` with the guest-assigned
/// virtio-gpu context id. 24 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapeCtxCreate {
    pub hdr: HeliosEscapeHeader,
    pub capset_id: u32,  // in:  VIRTIO_GPU_CAPSET_VENUS
    pub out_ctx_id: u32, // out: assigned context id
}

/// `HELIOS_ESCAPE_CTX_DESTROY`. 24 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapeCtxDestroy {
    pub hdr: HeliosEscapeHeader,
    pub ctx_id: u32,
    pub padding: u32,
}

/// `HELIOS_ESCAPE_ALLOC_BLOB`. The KMD allocates a virtio-gpu blob resource and
/// returns its id. 48 bytes.
///
/// `blob_id` is the venus device-memory id that backs a HOST3D mappable blob: the
/// ICD's `bo_ops.create_from_device_memory(size, mem_id)` passes the venus memory
/// id here, and the KMD forwards it as `VirtioGpuResourceCreateBlob.blob_id` so
/// virglrenderer's venus context can bind the blob to that `VkDeviceMemory` (see
/// ARCH.md §5/§6). A standalone scratch blob (no venus backing) passes `blob_id = 0`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapeAllocBlob {
    pub hdr: HeliosEscapeHeader,
    pub size: u64,            // in:  blob size in bytes
    pub blob_id: u64,         // in:  venus device-memory id backing the blob (0 = none)
    pub blob_flags: u32,      // in:  VIRTIO_GPU_BLOB_FLAG_*
    pub blob_mem: u32,        // in:  VIRTIO_GPU_BLOB_MEM_*
    pub ctx_id: u32,          // in:  owning context
    pub out_resource_id: u32, // out: assigned resource id
}

/// `HELIOS_ESCAPE_MAP_BLOB`. Maps a blob into the calling process; the KMD maps
/// the host-visible pages with `MmMapLockedPagesSpecifyCache(UserMode)` and
/// returns the resulting **user VA** (not a GPA — see ARCH.md §3, §6). 32 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapeMapBlob {
    pub hdr: HeliosEscapeHeader,
    pub out_user_va: u64, // out: user-mode virtual address of the mapping
    pub resource_id: u32, // in:  blob to map
    pub map_cache: u32,   // in/out: requested/effective VIRTIO_GPU_MAP_CACHE_*
}

/// `HELIOS_ESCAPE_RELEASE_BLOB`. Unmaps this file object's user view, then
/// detaches and unrefs the virtio-gpu blob resource. 32 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapeReleaseBlob {
    pub hdr: HeliosEscapeHeader,
    pub ctx_id: u32,
    pub resource_id: u32,
    pub flags: u32,
    pub padding: u32,
}

/// `HELIOS_ESCAPE_ATTACH_RESOURCE`. Attach an existing virtio-gpu resource id to
/// a Venus context before importing it with `VkImportMemoryResourceInfoMESA`.
/// Input-only. 24 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapeAttachResource {
    pub hdr: HeliosEscapeHeader,
    pub ctx_id: u32,
    pub resource_id: u32,
}

/// `HELIOS_ESCAPE_WAIT_FENCE`. 40 bytes (v2 shape — see below).
///
/// C3/M3.4 async transport: SUBMIT_VENUS no longer blocks until host
/// completion, so this escape is a REAL wait — the KMD blocks (PASSIVE, KEVENT)
/// until the wire fence completes on the virtio used ring or `timeout_ns`
/// elapses, and reports the outcome in `out_completed`.
///
/// `fence_id` is the KMD-assigned WIRE fence id returned by SUBMIT_VENUS (the
/// KMD writes the assigned id back into `HeliosEscapeSubmitVenus.fence_id`).
/// ICD-local fence counters collide across processes; wire ids are globally
/// monotonic and never reused.
///
/// DEPLOY-ORDER COMPAT: callers MUST initialize `out_completed = 1`. The old
/// (synchronous-transport) KMD validates the shape and returns SUCCESS without
/// writing the buffer — the pre-set 1 then correctly reports "complete"
/// (synchronous submits were host-complete by return). The new KMD accepts the
/// legacy 32-byte shape too (waits, but can only report timeout via a failure
/// NTSTATUS).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapeWaitFence {
    pub hdr: HeliosEscapeHeader,
    pub fence_id: u64,
    pub timeout_ns: u64,
    /// out: 1 = fence complete, 0 = timed out. Pre-set to 1 by the caller (see
    /// the deploy-order note above).
    pub out_completed: u32,
    pub _pad: u32,
}

/// The pre-async 32-byte `HELIOS_ESCAPE_WAIT_FENCE` shape: a strict PREFIX of
/// [`HeliosEscapeWaitFence`], without the `out_completed`/`_pad` tail.
///
/// Declared as a struct so the KMD stops re-encoding it as literal byte offsets
/// (`buf[16..24]` / `buf[24..32]`). Those literals were correct only by
/// coincidence: inserting a field before `timeout_ns` here — or reordering the
/// hand-written C copy in `vn_renderer_helios.c`, whose only cross-language guard
/// is a size assert — left both size asserts passing while the KMD waited on a
/// garbage timeout with no diagnostic.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapeWaitFenceLegacy {
    pub hdr: HeliosEscapeHeader,
    pub fence_id: u64,
    pub timeout_ns: u64,
}

/// `HELIOS_ESCAPE_PRESENT_BLOB` — throwaway Phase-7 gate op (DISPLAY.md §8). Bind
/// a venus blob `resource_id` to scanout 0 (`SET_SCANOUT_BLOB` + `RESOURCE_FLUSH`)
/// so the host displays it zero-copy under `-spice gl=on`. Input-only. 40 bytes.
///
/// `stride`/`offset` are plane-0 geometry (from `vkGetImageSubresourceLayout` on
/// the LINEAR swapchain-like image) the host needs to interpret the exported
/// dmabuf; the KMD forwards them as `SET_SCANOUT_BLOB.strides[0]/offsets[0]`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapePresentBlob {
    pub hdr: HeliosEscapeHeader,
    pub resource_id: u32, // in: the venus blob resource to scan out
    pub width: u32,       // in: image width in pixels
    pub height: u32,      // in: image height in pixels
    pub format: u32,      // in: VIRTIO_GPU_FORMAT_*
    pub stride: u32,      // in: plane-0 row pitch in bytes
    pub offset: u32,      // in: plane-0 byte offset into the blob
}

/// Registration outcome values for [`HeliosEscapeFenceEvent::out_state`].
///
/// REGISTER: the event handle was referenced and parked in the KMD's
/// fence-event table; the KMD signals it (KeSetEvent from the retirement DPC)
/// when the wire fence completes, then drops its reference (one-shot — the
/// registration is consumed by the signal).
pub const HELIOS_FENCE_EVENT_REGISTERED: u32 = 0;
/// REGISTER: the fence had ALREADY retired. The event was signaled immediately
/// and NOT registered (no reference kept). The caller must not wait.
pub const HELIOS_FENCE_EVENT_ALREADY_COMPLETE: u32 = 1;
/// REGISTER probe (`fence_id == 0 && event_handle == 0`): the KMD supports
/// fence events. Old KMDs fail the whole escape with STATUS_NOT_IMPLEMENTED —
/// that is the capability signal.
pub const HELIOS_FENCE_EVENT_PROBE_ACK: u32 = 2;
/// UNREGISTER: the registration was found and removed; the event was NOT
/// signaled by the KMD (and never will be — the reference is dropped).
pub const HELIOS_FENCE_EVENT_CANCELLED: u32 = 3;
/// UNREGISTER: no matching registration. Either the fence retired (the event
/// was signaled — check the event state) or nothing was ever registered.
pub const HELIOS_FENCE_EVENT_NOT_FOUND: u32 = 4;

/// `HELIOS_ESCAPE_REGISTER_FENCE_EVENT` / `HELIOS_ESCAPE_UNREGISTER_FENCE_EVENT`.
/// 40 bytes. NON-BLOCKING both ways — this is the replacement for parking a
/// thread inside a blocking WAIT_FENCE escape (PSC WS2: parked escapes convoy
/// the process's SUBMIT_VENUS escapes at the dxgkrnl escape layer, measured
/// 2.9 ms → µs when no parks).
///
/// REGISTER: `event_handle` is a usermode event HANDLE (CreateEvent) in the
/// CALLING process; the KMD resolves it via ObReferenceObjectByHandle
/// (EVENT_MODIFY_STATE, UserMode) so a bogus handle fails loudly and the KMD's
/// reference keeps the object alive across process death. The check "already
/// retired?" and the table insert are atomic against retirement (device
/// spinlock) — a wakeup can never be lost. One-shot: signal auto-unregisters.
/// The caller must reset the event before registering (manual-reset events
/// stay signaled).
///
/// UNREGISTER (after a usermode wait timeout): same `{fence_id, event_handle}`
/// pair. `HELIOS_FENCE_EVENT_CANCELLED` = the KMD did not and will not signal;
/// `HELIOS_FENCE_EVENT_NOT_FOUND` + a signaled event = the fence retired.
///
/// Capability probe: REGISTER with `fence_id == 0 && event_handle == 0` →
/// `HELIOS_FENCE_EVENT_PROBE_ACK` on a supporting KMD; the escape itself fails
/// (STATUS_NOT_IMPLEMENTED) on older KMDs.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapeFenceEvent {
    pub hdr: HeliosEscapeHeader,
    /// in: wire fence id (from `HeliosEscapeSubmitVenus.fence_id` writeback).
    pub fence_id: u64,
    /// in: usermode event handle, zero-extended to 64 bits.
    pub event_handle: u64,
    /// out: one of `HELIOS_FENCE_EVENT_*`.
    pub out_state: u32,
    pub _pad: u32,
}

/// `HELIOS_ESCAPE_QUERY_SCANOUT` — read-only identity of the optional KMD-owned
/// LINEAR fallback selected by `SetVidPnSourceAddress`. The allocation size
/// and memory type are the exact `vkAllocateMemory` identity required by a
/// Venus `VkImportMemoryResourceInfoMESA` import.  A zero `out_resource_id`
/// means no production primary is currently bound. 64 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapeQueryScanout {
    pub hdr: HeliosEscapeHeader,
    pub out_alloc_size: u64,
    pub out_resource_id: u32,
    pub out_width: u32,
    pub out_height: u32,
    /// Exact DXGI format recorded for the primary (normally B8G8R8X8_UNORM).
    pub out_dxgi_format: u32,
    pub out_pitch: u32,
    pub out_plane_offset: u32,
    pub out_memory_type_index: u32,
    /// Changes whenever the active primary identity is published or removed.
    pub out_generation: u32,
    pub reserved: [u32; 2],
}

/// `HELIOS_ESCAPE_QUERY_STATS` — read-only snapshot of the KMD's bounded
/// resource tables and rejection counters. Output-only (the caller sends a
/// zeroed body); 88 bytes. Values are point-in-time under the device lock for
/// the `live` fields and monotonic since driver start for the counters.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapeQueryStats {
    pub hdr: HeliosEscapeHeader,
    /// Bytes currently allocated in the host-visible window's offset space
    /// (bump high-water minus the coalesced free list).
    pub out_window_used: u64,
    /// Total host-visible window length (QEMU `hostmem=`), 0 if absent.
    pub out_window_len: u64,
    pub out_blobs_live: u32,
    pub out_blobs_cap: u32,
    pub out_blobs_high_water: u32,
    /// ALLOC_BLOB / note_blob_size rejections because the blob table was full.
    pub out_blob_full_rejects: u32,
    pub out_resources_live: u32,
    pub out_resources_cap: u32,
    pub out_resources_high_water: u32,
    /// resource_create_blob rejections because the live-resource table was full.
    pub out_resource_full_rejects: u32,
    pub out_contexts_live: u32,
    /// Context tracking slots dropped because the context table was full (the
    /// context still works but is not auto-reclaimed at DestroyDevice).
    pub out_context_full_drops: u32,
    /// Freed window ranges dropped because the free list was full (leaked
    /// window offset space).
    pub out_window_range_drops: u32,
    /// Virtio control-queue round-trip timeouts (transport poison latch).
    pub out_ctrl_timeouts: u32,
    /// take_live_resource calls for an id not in the table (duplicate teardown).
    pub out_take_live_misses: u32,
    /// adopt_blob_for_allocation rejections of a dead resource id.
    pub out_adopt_dead_rejects: u32,
}

/// `HELIOS_ESCAPE_QUERY_STATS` v2 (KMD 22.22.54+): the v1 struct plus the
/// fence-event table counters. The KMD accepts BOTH sizes — a v1 caller gets
/// the v1 prefix, a v2 caller against an old KMD gets its extension fields
/// back untouched (pre-zero them; fence-event support is detected via the
/// REGISTER probe, not via this struct).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapeQueryStatsV2 {
    pub v1: HeliosEscapeQueryStats,
    /// Registrations currently parked in the fence-event table.
    pub out_fence_events_live: u32,
    pub out_fence_events_high_water: u32,
    /// Successful REGISTERs (parked in the table), monotonic.
    pub out_fence_event_registers: u32,
    /// Events signaled at wire-fence retirement (DPC), monotonic.
    pub out_fence_event_signals: u32,
    /// REGISTERs answered ALREADY_COMPLETE (signaled inline, not parked).
    pub out_fence_event_already_complete: u32,
    /// REGISTERs refused because the table was full.
    pub out_fence_event_overflows: u32,
    /// REGISTERs refused as duplicates of a parked (fence_id, event) pair.
    pub out_fence_event_dup_rejects: u32,
    /// REGISTER/UNREGISTER rejected: bad fence id / unreferencable handle.
    pub out_fence_event_invalid: u32,
    /// UNREGISTERs that found and removed a parked registration.
    pub out_fence_event_cancels: u32,
    /// Registrations dropped (dereferenced UNSIGNALED) at transport teardown.
    pub out_fence_event_teardown_drops: u32,
    /// Live user-VA blob mappings (adapter-global MappingTable).
    pub out_mappings_live: u32,
    /// MappingTable capacity (MAX_MAPPINGS).
    pub out_mappings_cap: u32,
    pub out_mappings_high_water: u32,
    /// MAP_BLOB refusals because the mapping table was full (the 2026-07-06
    /// Doom "Cannot map buffer BU_STATIC" level-load fatal).
    pub out_mapping_full_rejects: u32,
    /// map_io_pages_to_user failures (MDL alloc / user-map raise).
    pub out_map_pages_fails: u32,
    /// alloc_window_range refusals (window space exhausted / fragmented).
    pub out_window_alloc_rejects: u32,
}

/// `HELIOS_ESCAPE_QUERY_STATS` v3 (KMD 22.22.180+): the v2 struct plus the
/// counters T1b made reportable — the escape-refusal family (an ICD/KMD
/// protocol skew used to be invisible), the control-path response errors that
/// are the loud-failure signal for the direct-primary display path, the
/// DDI-level CPU-host-aperture unmap count, and the three hal.rs failures.
///
/// APPENDED, never overlaid on v2's fields: existing v1/v2 probes must keep
/// parsing byte-identically, which is why this does not reuse reserved space.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapeQueryStatsV3 {
    pub v2: HeliosEscapeQueryStatsV2,
    /// Escapes refused for a bad magic/version, or a header claiming to be
    /// larger than the buffer the runtime supplied.
    pub out_escape_bad_header: u32,
    /// Escapes naming a verb this KMD does not implement (an ICD/KMD skew).
    pub out_escape_unknown_verb: u32,
    /// Verbs refused because the caller's buffer was shorter than the payload
    /// the verb requires.
    pub out_escape_short_buffer: u32,
    /// Verbs that failed because the transport was gone (StopDevice), rather
    /// than producing a content answer that reads as "nothing published yet".
    pub out_escape_device_gone: u32,
    /// Ownership-bearing verbs refused for a NULL hDevice (owner-0 forgery).
    pub out_escape_no_device: u32,
    /// Context verbs refused because the caller's device does not own the id.
    pub out_escape_foreign_ctx: u32,
    /// Async control responses the host rejected — SET_SCANOUT_BLOB /
    /// RESOURCE_FLUSH failures. The loud-failure counter for the direct-primary
    /// display path, previously in no report at all.
    pub out_async_ctrl_resp_errors: u32,
    /// `DxgkDdiUnmapCpuHostAperture` calls (DDI level, for map/unmap pairing
    /// against ChMc — the internal unmap count ChUn is not the same thing).
    pub out_cpu_host_unmap_count: u32,
    /// `dma_alloc` failures in the virtio HAL (was kmsg-only).
    pub out_dma_alloc_fails: u32,
    /// `MmMapIoSpace` failures in the virtio HAL (was kmsg-only).
    pub out_mmio_map_fails: u32,
    /// MMIO tracking-cache overflows: the mapping stays valid but untracked, so
    /// `unmap_all` can never release it — a permanent system-PTE leak, now at
    /// least visible.
    pub out_mmio_cache_full: u32,
    /// QUERY_SCANOUT reads abandoned after the bounded seqlock retry.
    pub out_query_scanout_retries: u32,
}

const _: () = {
    assert!(core::mem::size_of::<HeliosEscapeHeader>() == 16);
    assert!(core::mem::size_of::<HeliosEscapeQueryStats>() == 88);
    assert!(core::mem::size_of::<HeliosEscapeSubmitVenus>() == 40);
    assert!(core::mem::size_of::<HeliosEscapeCtxCreate>() == 24);
    assert!(core::mem::size_of::<HeliosEscapeAllocBlob>() == 48);
    assert!(core::mem::size_of::<HeliosEscapeMapBlob>() == 32);
    assert!(core::mem::size_of::<HeliosEscapeReleaseBlob>() == 32);
    assert!(core::mem::size_of::<HeliosEscapeAttachResource>() == 24);
    assert!(core::mem::size_of::<HeliosEscapeWaitFence>() == 40);
    assert!(core::mem::size_of::<HeliosEscapeWaitFenceLegacy>() == 32);
    // The legacy shape must stay a strict PREFIX of the current one — that is
    // what makes reading the common fields out of a 32-byte buffer correct.
    assert!(
        core::mem::offset_of!(HeliosEscapeWaitFence, fence_id)
            == core::mem::offset_of!(HeliosEscapeWaitFenceLegacy, fence_id)
    );
    assert!(
        core::mem::offset_of!(HeliosEscapeWaitFence, timeout_ns)
            == core::mem::offset_of!(HeliosEscapeWaitFenceLegacy, timeout_ns)
    );
    assert!(core::mem::size_of::<HeliosEscapePresentBlob>() == 40);
    assert!(core::mem::size_of::<HeliosEscapeFenceEvent>() == 40);
    assert!(core::mem::size_of::<HeliosEscapeQueryScanout>() == 64);
    assert!(core::mem::size_of::<HeliosEscapeQueryStatsV2>() == 152);
    // v2 (152) + 12 u32 = 200. The v2 prefix must stay byte-identical.
    assert!(core::mem::size_of::<HeliosEscapeQueryStatsV3>() == 200);
};
