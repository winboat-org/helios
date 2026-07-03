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
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapeSubmitVenus {
    pub hdr: HeliosEscapeHeader,
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

/// `HELIOS_ESCAPE_WAIT_FENCE`. 32 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEscapeWaitFence {
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

const _: () = {
    assert!(core::mem::size_of::<HeliosEscapeHeader>() == 16);
    assert!(core::mem::size_of::<HeliosEscapeSubmitVenus>() == 40);
    assert!(core::mem::size_of::<HeliosEscapeCtxCreate>() == 24);
    assert!(core::mem::size_of::<HeliosEscapeAllocBlob>() == 48);
    assert!(core::mem::size_of::<HeliosEscapeMapBlob>() == 32);
    assert!(core::mem::size_of::<HeliosEscapeReleaseBlob>() == 32);
    assert!(core::mem::size_of::<HeliosEscapeAttachResource>() == 24);
    assert!(core::mem::size_of::<HeliosEscapeWaitFence>() == 32);
    assert!(core::mem::size_of::<HeliosEscapePresentBlob>() == 40);
};
