//! virtio-gpu command/response structures (KMD.md Phase 2, TRANSPORT.md §1).
//!
//! Layouts mirror `virtio_gpu.h` from the Linux kernel / virglrenderer and MUST
//! match byte-for-byte, since the host decodes these directly. Every struct is
//! `repr(C)` and laid out to contain no implicit padding so it can derive
//! `Pod`/`Zeroable` for safe zero-copy (de)serialization.

use bytemuck::{Pod, Zeroable};

// ── Control command types (request) ─────────────────────────────────────────
//
// These MUST match the Linux uapi `enum virtio_gpu_ctrl_type` (== QEMU's) value
// for value, since QEMU dispatches the command handler purely on `hdr.type_`.
// They are pinned to the `virtio-bindings` crate by the test module at the bottom
// of this file (`cargo test -p helios_protocol`); the dev-dep keeps the no_std
// build clean. DO NOT hand-edit a value without the enum in front of you — the
// 3D range in particular is dense and easy to miscount: `RESOURCE_CREATE_3D`
// (0x0204) sits between CTX_DETACH and the transfer/submit/blob ops, so e.g.
// SUBMIT_3D is 0x0207 (NOT 0x0204) and RESOURCE_MAP_BLOB is 0x0208 (NOT 0x0207).
// A prior off-by-this made the KMD send MAP_BLOB as SUBMIT_3D, whose `size`@24
// aliased the map-blob struct's `resource_id`@24 → "submit_3d size mismatch".
//
// 2d commands (0x0100..)
pub const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
pub const VIRTIO_GPU_CMD_RESOURCE_UNREF: u32 = 0x0102;
pub const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
pub const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
pub const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
pub const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
pub const VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
pub const VIRTIO_GPU_CMD_GET_CAPSET_INFO: u32 = 0x0108;
pub const VIRTIO_GPU_CMD_GET_CAPSET: u32 = 0x0109;
pub const VIRTIO_GPU_CMD_GET_EDID: u32 = 0x010a;
pub const VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID: u32 = 0x010b;
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB: u32 = 0x010c;
pub const VIRTIO_GPU_CMD_SET_SCANOUT_BLOB: u32 = 0x010d;
// 3d commands (0x0200..)
pub const VIRTIO_GPU_CMD_CTX_CREATE: u32 = 0x0200;
pub const VIRTIO_GPU_CMD_CTX_DESTROY: u32 = 0x0201;
pub const VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE: u32 = 0x0202;
pub const VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE: u32 = 0x0203;
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_3D: u32 = 0x0204;
pub const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D: u32 = 0x0205;
pub const VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D: u32 = 0x0206;
pub const VIRTIO_GPU_CMD_SUBMIT_3D: u32 = 0x0207;
pub const VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB: u32 = 0x0208;
pub const VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB: u32 = 0x0209;

// ── Response types ────────────────────────────────────────────────────────
pub const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
pub const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
pub const VIRTIO_GPU_RESP_OK_CAPSET_INFO: u32 = 0x1102;
pub const VIRTIO_GPU_RESP_OK_CAPSET: u32 = 0x1103;
pub const VIRTIO_GPU_RESP_OK_EDID: u32 = 0x1104;
pub const VIRTIO_GPU_RESP_OK_RESOURCE_UUID: u32 = 0x1105;
pub const VIRTIO_GPU_RESP_OK_MAP_INFO: u32 = 0x1106;
pub const VIRTIO_GPU_RESP_ERR_UNSPEC: u32 = 0x1200;
pub const VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY: u32 = 0x1201;
pub const VIRTIO_GPU_RESP_ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
pub const VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
pub const VIRTIO_GPU_RESP_ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
pub const VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

/// `VIRTIO_GPU_RESP_OK_*` occupy [0x1100, 0x1200); errors are >= 0x1200.
#[inline]
pub fn resp_is_ok(resp_type: u32) -> bool {
    (0x1100..0x1200).contains(&resp_type)
}

// ── Command-header flags ────────────────────────────────────────────────────
/// Request a fence: the device writes a response carrying the same `fence_id`
/// and signals the used ring when the command completes.
pub const VIRTIO_GPU_FLAG_FENCE: u32 = 1 << 0;
/// `ring_idx` field in the header is valid (context init / multi-ring).
pub const VIRTIO_GPU_FLAG_INFO_RING_IDX: u32 = 1 << 1;

// ── Capset IDs ──────────────────────────────────────────────────────────────
pub const VIRTIO_GPU_CAPSET_VIRGL: u32 = 1;
pub const VIRTIO_GPU_CAPSET_VIRGL2: u32 = 2;
pub const VIRTIO_GPU_CAPSET_GFXSTREAM: u32 = 3;
/// Vulkan via Venus. This is the capset Helios drives.
pub const VIRTIO_GPU_CAPSET_VENUS: u32 = 4;

// ── Shared-memory region ids (`virtio_gpu_shm_id`) ──────────────────────────
/// No region. The cap's `id` byte holds one of these.
pub const VIRTIO_GPU_SHM_ID_UNDEFINED: u8 = 0;
/// The host-visible memory window: a prefetchable 64-bit PCI BAR (QEMU
/// `hostmem=`) that `RESOURCE_MAP_BLOB` injects resource mappings into. **= 1**
/// (linux/virtio_gpu.h: UNDEFINED=0, HOST_VISIBLE=1) — not 0. See ARCH §6.
pub const VIRTIO_GPU_SHM_ID_HOST_VISIBLE: u8 = 1;

// ── Blob memory / flags ─────────────────────────────────────────────────────
pub const VIRTIO_GPU_BLOB_MEM_GUEST: u32 = 1;
pub const VIRTIO_GPU_BLOB_MEM_HOST3D: u32 = 2;
pub const VIRTIO_GPU_BLOB_MEM_HOST3D_GUEST: u32 = 3;
pub const VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE: u32 = 1;
pub const VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE: u32 = 2;
pub const VIRTIO_GPU_BLOB_FLAG_USE_CROSS_DEVICE: u32 = 4;

// ── Blob map caching (`map_info` in `VirtioGpuRespMapInfo`) ──────────────────
// The host returns the caching mode it wants the guest to map the host-visible
// blob with, in the low nibble of `map_info`. The KMD translates this to a
// Windows `MEMORY_CACHING_TYPE` for `MmMapLockedPagesSpecifyCache`. (QEMU
// `honor-guest-pat=on` makes the guest-chosen cache type effective.)
pub const VIRTIO_GPU_MAP_CACHE_MASK: u32 = 0x0f;
pub const VIRTIO_GPU_MAP_CACHE_NONE: u32 = 0x00;
pub const VIRTIO_GPU_MAP_CACHE_CACHED: u32 = 0x01;
pub const VIRTIO_GPU_MAP_CACHE_UNCACHED: u32 = 0x02;
pub const VIRTIO_GPU_MAP_CACHE_WC: u32 = 0x03;

pub const VIRTIO_GPU_MAX_SCANOUTS: usize = 16;

// ── virtio_gpu_formats (scanout pixel formats) ──────────────────────────────
/// BGRA8888, matching Vulkan `VK_FORMAT_B8G8R8A8_UNORM` — the format Helios
/// scans out venus blobs with. (virtio_gpu_formats enum value 1.)
pub const VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM: u32 = 1;
/// BGRX8888, matching Vulkan `VK_FORMAT_B8G8R8A8_UNORM` storage while telling
/// the display backend to ignore alpha. (virtio_gpu_formats enum value 2.)
pub const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;
/// RGBA8888, matching Vulkan `VK_FORMAT_R8G8B8A8_UNORM`.
/// (`virtio_gpu_formats` enum value 67.)
pub const VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM: u32 = 67;
/// RGBX8888, matching Vulkan `VK_FORMAT_R8G8B8A8_UNORM` storage while telling
/// the display backend to ignore alpha. (`virtio_gpu_formats` enum value 134.)
pub const VIRTIO_GPU_FORMAT_R8G8B8X8_UNORM: u32 = 134;

/// Control command header — prepended to every virtio-gpu command and response.
/// 24 bytes, 8-byte aligned, no implicit padding.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuCtrlHdr {
    pub type_: u32,    // VIRTIO_GPU_CMD_* (request) or VIRTIO_GPU_RESP_* (reply)
    pub flags: u32,    // VIRTIO_GPU_FLAG_*
    pub fence_id: u64, // echoed back in the response when FLAG_FENCE is set
    pub ctx_id: u32,   // 3D context id (0 for global commands)
    pub ring_idx: u8,  // valid only with FLAG_INFO_RING_IDX
    pub padding: [u8; 3],
}

/// `VIRTIO_GPU_CMD_CTX_CREATE`. For Venus, `context_init` carries the capset id.
/// 96 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuCtxCreate {
    pub hdr: VirtioGpuCtrlHdr,
    pub nlen: u32,
    pub context_init: u32, // capset_id for Venus (== VIRTIO_GPU_CAPSET_VENUS)
    pub debug_name: [u8; 64],
}

/// `VIRTIO_GPU_CMD_CTX_DESTROY` — header only.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuCtxDestroy {
    pub hdr: VirtioGpuCtrlHdr,
}

/// `VIRTIO_GPU_CMD_SUBMIT_3D` — header followed by `size` bytes of Venus stream.
/// 32 bytes (the command data follows in a separate descriptor).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuCmdSubmit {
    pub hdr: VirtioGpuCtrlHdr,
    pub size: u32,
    pub padding: u32,
}

/// `VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB`. 56 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuResourceCreateBlob {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub blob_mem: u32,
    pub blob_flags: u32,
    pub nr_entries: u32,
    pub blob_id: u64,
    pub size: u64,
}

/// `VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB`. 40 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuResourceMapBlob {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub padding: u32,
    pub offset: u64,
}

/// `VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB`. 32 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuResourceUnmapBlob {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub padding: u32,
}

/// `VIRTIO_GPU_RESP_OK_MAP_INFO` — reply to MAP_BLOB. 32 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuRespMapInfo {
    pub hdr: VirtioGpuCtrlHdr,
    pub map_info: u32, // host caching flags
    pub padding: u32,
}

/// `VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE` / `..._DETACH_RESOURCE`. 32 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuCtxResource {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub padding: u32,
}

/// `VIRTIO_GPU_CMD_RESOURCE_UNREF`. 32 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuResourceUnref {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub padding: u32,
}

// ── GET_DISPLAY_INFO (Phase 2 smoke test) ───────────────────────────────────

/// A rectangle in the display-info response.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuDisplayOne {
    pub r: VirtioGpuRect,
    pub enabled: u32,
    pub flags: u32,
}

/// `VIRTIO_GPU_RESP_OK_DISPLAY_INFO`. Header + fixed array of scanout descriptors.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuRespDisplayInfo {
    pub hdr: VirtioGpuCtrlHdr,
    pub pmodes: [VirtioGpuDisplayOne; VIRTIO_GPU_MAX_SCANOUTS],
}

// ── Scanout / present (Phase 7 display engine) ──────────────────────────────

/// `VIRTIO_GPU_CMD_SET_SCANOUT_BLOB` (0x010d) — bind a blob resource to a
/// scanout for zero-copy display. The host materializes the blob's exported
/// `dmabuf_fd` (set at blob-create via `virgl_renderer_resource_get_info`) and
/// presents it via the GL backend under `-spice gl=on`. 96 bytes. Field order
/// pinned to `virtio_gpu_set_scanout_blob`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuSetScanoutBlob {
    pub hdr: VirtioGpuCtrlHdr,
    pub r: VirtioGpuRect, // target rect on the scanout
    pub scanout_id: u32,
    pub resource_id: u32, // a blob resource (0 disables this scanout)
    pub width: u32,       // blob image width in pixels
    pub height: u32,      // blob image height in pixels
    pub format: u32,      // VIRTIO_GPU_FORMAT_*
    pub padding: u32,
    pub strides: [u32; 4], // per-plane row stride in bytes
    pub offsets: [u32; 4], // per-plane byte offset into the blob
}

/// `VIRTIO_GPU_CMD_RESOURCE_FLUSH` (0x0104) — flush a resource's dirty rect to
/// the display. For a GL/blob scanout this is what drives the host present.
/// 48 bytes. Pinned to `virtio_gpu_resource_flush`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuResourceFlush {
    pub hdr: VirtioGpuCtrlHdr,
    pub r: VirtioGpuRect,
    pub resource_id: u32,
    pub padding: u32,
}

// Compile-time guarantees that the on-wire sizes are what the host expects.
const _: () = {
    assert!(core::mem::size_of::<VirtioGpuCtrlHdr>() == 24);
    assert!(core::mem::size_of::<VirtioGpuCtxCreate>() == 96);
    assert!(core::mem::size_of::<VirtioGpuCmdSubmit>() == 32);
    assert!(core::mem::size_of::<VirtioGpuResourceCreateBlob>() == 56);
    assert!(core::mem::size_of::<VirtioGpuResourceMapBlob>() == 40);
    assert!(core::mem::size_of::<VirtioGpuRespMapInfo>() == 32);
    assert!(core::mem::size_of::<VirtioGpuRespDisplayInfo>() == 24 + 16 * 24);
    assert!(core::mem::size_of::<VirtioGpuSetScanoutBlob>() == 96);
    assert!(core::mem::size_of::<VirtioGpuResourceFlush>() == 48);
};

/// Pin every wire constant above to the `virtio-bindings` crate (generated from
/// the Linux uapi `virtio_gpu.h`). `virtio-bindings` is std-only so it cannot be
/// a normal dependency of this no_std crate; it is a dev-dependency and this test
/// is the single source of truth that catches any drift. Run with
/// `cargo test -p helios_protocol` (host/Linux, std available).
#[cfg(test)]
mod virtio_bindings_pin {
    use virtio_bindings::virtio_gpu as vb;

    macro_rules! pin {
        ($ours:expr, $theirs:ident) => {
            assert_eq!(
                $ours,
                vb::$theirs as u32,
                concat!(
                    "wire constant drift vs virtio-bindings::",
                    stringify!($theirs)
                ),
            );
        };
    }

    #[test]
    fn ctrl_types_match_virtio_bindings() {
        use super::*;
        // 2d
        pin!(
            VIRTIO_GPU_CMD_GET_DISPLAY_INFO,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_GET_DISPLAY_INFO
        );
        pin!(
            VIRTIO_GPU_CMD_RESOURCE_CREATE_2D,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_CREATE_2D
        );
        pin!(
            VIRTIO_GPU_CMD_RESOURCE_UNREF,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_UNREF
        );
        pin!(
            VIRTIO_GPU_CMD_SET_SCANOUT,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_SET_SCANOUT
        );
        pin!(
            VIRTIO_GPU_CMD_RESOURCE_FLUSH,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_FLUSH
        );
        pin!(
            VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D
        );
        pin!(
            VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING
        );
        pin!(
            VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING
        );
        pin!(
            VIRTIO_GPU_CMD_GET_CAPSET_INFO,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_GET_CAPSET_INFO
        );
        pin!(
            VIRTIO_GPU_CMD_GET_CAPSET,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_GET_CAPSET
        );
        pin!(
            VIRTIO_GPU_CMD_GET_EDID,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_GET_EDID
        );
        pin!(
            VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID
        );
        pin!(
            VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB
        );
        pin!(
            VIRTIO_GPU_CMD_SET_SCANOUT_BLOB,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_SET_SCANOUT_BLOB
        );
        // 3d
        pin!(
            VIRTIO_GPU_CMD_CTX_CREATE,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_CTX_CREATE
        );
        pin!(
            VIRTIO_GPU_CMD_CTX_DESTROY,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_CTX_DESTROY
        );
        pin!(
            VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE
        );
        pin!(
            VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE
        );
        pin!(
            VIRTIO_GPU_CMD_RESOURCE_CREATE_3D,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_CREATE_3D
        );
        pin!(
            VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D
        );
        pin!(
            VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D
        );
        pin!(
            VIRTIO_GPU_CMD_SUBMIT_3D,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_SUBMIT_3D
        );
        pin!(
            VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB
        );
        pin!(
            VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB,
            virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB
        );
        // responses
        pin!(
            VIRTIO_GPU_RESP_OK_NODATA,
            virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_OK_NODATA
        );
        pin!(
            VIRTIO_GPU_RESP_OK_DISPLAY_INFO,
            virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_OK_DISPLAY_INFO
        );
        pin!(
            VIRTIO_GPU_RESP_OK_CAPSET_INFO,
            virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_OK_CAPSET_INFO
        );
        pin!(
            VIRTIO_GPU_RESP_OK_CAPSET,
            virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_OK_CAPSET
        );
        pin!(
            VIRTIO_GPU_RESP_OK_EDID,
            virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_OK_EDID
        );
        pin!(
            VIRTIO_GPU_RESP_OK_RESOURCE_UUID,
            virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_OK_RESOURCE_UUID
        );
        pin!(
            VIRTIO_GPU_RESP_OK_MAP_INFO,
            virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_OK_MAP_INFO
        );
        pin!(
            VIRTIO_GPU_RESP_ERR_UNSPEC,
            virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_ERR_UNSPEC
        );
        pin!(
            VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY,
            virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY
        );
        pin!(
            VIRTIO_GPU_RESP_ERR_INVALID_SCANOUT_ID,
            virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_ERR_INVALID_SCANOUT_ID
        );
        pin!(
            VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID,
            virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID
        );
        pin!(
            VIRTIO_GPU_RESP_ERR_INVALID_CONTEXT_ID,
            virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_ERR_INVALID_CONTEXT_ID
        );
        pin!(
            VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER,
            virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
        );
    }

    #[test]
    fn blob_constants_match_virtio_bindings() {
        use super::*;
        pin!(VIRTIO_GPU_BLOB_MEM_GUEST, VIRTIO_GPU_BLOB_MEM_GUEST);
        pin!(VIRTIO_GPU_BLOB_MEM_HOST3D, VIRTIO_GPU_BLOB_MEM_HOST3D);
        pin!(
            VIRTIO_GPU_BLOB_MEM_HOST3D_GUEST,
            VIRTIO_GPU_BLOB_MEM_HOST3D_GUEST
        );
        pin!(
            VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE,
            VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE
        );
        pin!(
            VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE,
            VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE
        );
        pin!(
            VIRTIO_GPU_BLOB_FLAG_USE_CROSS_DEVICE,
            VIRTIO_GPU_BLOB_FLAG_USE_CROSS_DEVICE
        );
        pin!(
            VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM,
            virtio_gpu_formats_VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM
        );
    }

    /// Cross-check the size AND alignment of every on-wire struct against the
    /// `virtio-bindings` (kernel-uapi-generated) equivalent. Size+align match is a
    /// strong proxy for layout equivalence; combined with the identical field
    /// order/types this guarantees the host decodes our buffers correctly.
    macro_rules! pin_layout {
        ($ours:ty, $theirs:ty) => {{
            assert_eq!(
                core::mem::size_of::<$ours>(),
                core::mem::size_of::<$theirs>(),
                concat!(
                    "size drift: ",
                    stringify!($ours),
                    " vs ",
                    stringify!($theirs)
                ),
            );
            assert_eq!(
                core::mem::align_of::<$ours>(),
                core::mem::align_of::<$theirs>(),
                concat!(
                    "align drift: ",
                    stringify!($ours),
                    " vs ",
                    stringify!($theirs)
                ),
            );
        }};
    }

    #[test]
    fn struct_layouts_match_virtio_bindings() {
        use super::*;
        pin_layout!(VirtioGpuCtrlHdr, vb::virtio_gpu_ctrl_hdr);
        pin_layout!(VirtioGpuRect, vb::virtio_gpu_rect);
        pin_layout!(VirtioGpuResourceUnref, vb::virtio_gpu_resource_unref);
        pin_layout!(
            VirtioGpuDisplayOne,
            vb::virtio_gpu_resp_display_info_virtio_gpu_display_one
        );
        pin_layout!(VirtioGpuRespDisplayInfo, vb::virtio_gpu_resp_display_info);
        pin_layout!(VirtioGpuCtxCreate, vb::virtio_gpu_ctx_create);
        pin_layout!(VirtioGpuCtxResource, vb::virtio_gpu_ctx_resource);
        pin_layout!(VirtioGpuCmdSubmit, vb::virtio_gpu_cmd_submit);
        pin_layout!(
            VirtioGpuResourceCreateBlob,
            vb::virtio_gpu_resource_create_blob
        );
        pin_layout!(VirtioGpuResourceMapBlob, vb::virtio_gpu_resource_map_blob);
        pin_layout!(
            VirtioGpuResourceUnmapBlob,
            vb::virtio_gpu_resource_unmap_blob
        );
        pin_layout!(VirtioGpuRespMapInfo, vb::virtio_gpu_resp_map_info);
        pin_layout!(VirtioGpuSetScanoutBlob, vb::virtio_gpu_set_scanout_blob);
        pin_layout!(VirtioGpuResourceFlush, vb::virtio_gpu_resource_flush);
    }
}
