//! Minimal in-kernel venus (Vulkan-over-virtio-gpu) client.
//!
//! WHY THIS EXISTS. VidMm drops a system-RAM-backed memory segment, but the WDDM
//! decorative page-table segment must be **device-BAR memory backed by real host
//! memory**. venus host-visible memory, mapped into the host-visible BAR window,
//! is exactly that: the host GPU owns the real allocation, the guest sees it
//! through the SHARED_MEMORY_CFG/HOST_VISIBLE BAR at `host_visible.base + offset`,
//! and it is CPU-coherent. This module self-allocates ONE 16-MiB
//! HOST_VISIBLE|HOST_COHERENT `VkDeviceMemory` over venus at device-init time and
//! returns its guest-physical window address so `query_segments` can report it as
//! the VidMm page-table segment.
//!
//! HOW IT WORKS. venus is the Mesa Vulkan-passthrough protocol: an opaque
//! command stream the host (virglrenderer's venus decoder) executes. We bootstrap
//! the venus command *ring* (a shared-memory FIFO described to the host by
//! `vkCreateRingMESA`, sent directly via `VIRTIO_GPU_CMD_SUBMIT_3D`), then drive
//! the normal Vulkan bring-up — instance, physical device, memory properties,
//! device, allocate-memory — through that ring, reading replies from a second
//! shared-memory blob. All wire encodings are byte-for-byte verified against
//! `icd/mesa/src/virtio/venus-protocol/vn_protocol_driver_*.h` and `vn_ring.c`.
//!
//! IRQL. The entire flow runs at PASSIVE_LEVEL — bring-up from
//! `DxgkDdiStartDevice`, runtime allocation from `DxgkDdiCreateAllocation` /
//! `DxgkDdiDestroyAllocation` under the adapter's PASSIVE venus mutex
//! (`AdapterContext::with_venus_client`). Control-queue submissions ride
//! `virtio::ctrl` (PASSIVE KEVENT waits under the hood); the venus ring-head
//! waits are PASSIVE sleep-polls (short spin burst, then 1 ms sleeps) — the vn
//! ring has no interrupt to wait on (the host writes progress into the ring
//! shmem only), and the old DISPATCH spins under the device spinlock were one
//! of the 2026-07-04 convoy/poison classes. NOTHING here ever holds the virtio
//! spinlock across a wait.

use alloc::vec::Vec;
use core::sync::atomic::{compiler_fence, fence, Ordering};

use helios_protocol::{
    VIRTIO_GPU_BLOB_FLAG_USE_CROSS_DEVICE, VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE,
    VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE, VIRTIO_GPU_BLOB_MEM_HOST3D, VIRTIO_GPU_MAP_CACHE_CACHED,
    VIRTIO_GPU_MAP_CACHE_UNCACHED, VIRTIO_GPU_MAP_CACHE_WC,
};
use wdk_sys::ntddk::{MmMapIoSpace, MmUnmapIoSpace};
use wdk_sys::{_MEMORY_CACHING_TYPE, PHYSICAL_ADDRESS};

use super::ctrl;
use super::VirtioError;
use crate::adapter::AdapterContext;

// ── venus command type ids (VkCommandTypeEXT) ────────────────────────────────
// Verified against vn_protocol_driver_defines.h.
const CMD_CREATE_INSTANCE: u32 = 0;
const CMD_ENUMERATE_PHYSICAL_DEVICES: u32 = 2;
const CMD_GET_PHYSICAL_DEVICE_MEMORY_PROPERTIES: u32 = 8;
const CMD_CREATE_DEVICE: u32 = 11;
const CMD_QUEUE_SUBMIT: u32 = 18;
const CMD_ALLOCATE_MEMORY: u32 = 21;
const CMD_FREE_MEMORY: u32 = 22;
const CMD_BIND_IMAGE_MEMORY: u32 = 29;
const CMD_BIND_BUFFER_MEMORY: u32 = 28;
const CMD_GET_BUFFER_MEMORY_REQUIREMENTS: u32 = 30;
const CMD_GET_IMAGE_MEMORY_REQUIREMENTS: u32 = 31;
const CMD_CREATE_FENCE: u32 = 35;
const CMD_DESTROY_FENCE: u32 = 36;
const CMD_WAIT_FOR_FENCES: u32 = 39;
const CMD_CREATE_BUFFER: u32 = 50;
const CMD_DESTROY_BUFFER: u32 = 51;
const CMD_CREATE_IMAGE: u32 = 54;
const CMD_DESTROY_IMAGE: u32 = 55;
const CMD_GET_IMAGE_SUBRESOURCE_LAYOUT: u32 = 56;
const CMD_CREATE_COMMAND_POOL: u32 = 85;
const CMD_DESTROY_COMMAND_POOL: u32 = 86;
const CMD_ALLOCATE_COMMAND_BUFFERS: u32 = 88;
const CMD_BEGIN_COMMAND_BUFFER: u32 = 90;
const CMD_END_COMMAND_BUFFER: u32 = 91;
const CMD_COPY_IMAGE: u32 = 113;
const CMD_BLIT_IMAGE: u32 = 114;
const CMD_COPY_IMAGE_TO_BUFFER: u32 = 116;
const CMD_CLEAR_COLOR_IMAGE: u32 = 119;
const CMD_PIPELINE_BARRIER: u32 = 126;
const CMD_GET_DEVICE_QUEUE_2: u32 = 155;
const CMD_SET_REPLY_COMMAND_STREAM_MESA: u32 = 178;
const CMD_CREATE_RING_MESA: u32 = 188;
const CMD_NOTIFY_RING_MESA: u32 = 190;
const CMD_SUBMIT_VIRTQUEUE_SEQNO_MESA: u32 = 251;
const CMD_WAIT_VIRTQUEUE_SEQNO_MESA: u32 = 252;

/// `VK_COMMAND_GENERATE_REPLY_BIT_EXT` — set in a command's flags word to request
/// a reply written into the previously-set reply command stream.
const CMD_FLAG_GENERATE_REPLY: u32 = 0x1;

// ── Vulkan structure-type ids (VkStructureType) ──────────────────────────────
const ST_INSTANCE_CREATE_INFO: i32 = 1;
const ST_DEVICE_QUEUE_CREATE_INFO: i32 = 2;
const ST_DEVICE_CREATE_INFO: i32 = 3;
const ST_SUBMIT_INFO: i32 = 4;
const ST_MEMORY_ALLOCATE_INFO: i32 = 5;
const ST_BUFFER_CREATE_INFO: i32 = 12;
const ST_FENCE_CREATE_INFO: i32 = 8;
const ST_IMAGE_CREATE_INFO: i32 = 14;
const ST_COMMAND_POOL_CREATE_INFO: i32 = 39;
const ST_COMMAND_BUFFER_ALLOCATE_INFO: i32 = 40;
const ST_COMMAND_BUFFER_BEGIN_INFO: i32 = 42;
const ST_BUFFER_MEMORY_BARRIER: i32 = 44;
const ST_IMAGE_MEMORY_BARRIER: i32 = 45;
const ST_DEVICE_QUEUE_INFO_2: i32 = 1000145003;
const ST_EXTERNAL_MEMORY_IMAGE_CREATE_INFO: i32 = 1000072001;
const ST_EXPORT_MEMORY_ALLOCATE_INFO: i32 = 1000072002;
const ST_MEMORY_DEDICATED_ALLOCATE_INFO: i32 = 1000127001;
const ST_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO: i32 = 1000158003;
const ST_RING_CREATE_INFO_MESA: i32 = 1000384000;
const ST_DEVICE_QUEUE_TIMELINE_INFO_MESA: i32 = 1000384005;
const ST_IMPORT_MEMORY_RESOURCE_INFO_MESA: i32 = 1000384002;

/// `VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT`.
///
/// Ordinary Helios/DXVK shared images use the renderer's OPAQUE_FD export path;
/// the KMD alias must carry the same external-image handle type even though the
/// actual memory import is named by `VkImportMemoryResourceInfoMESA`.
const EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD: u32 = 0x0000_0001;
/// `VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT`.
const EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF: u32 = 0x0000_0200;

/// External-memory transport used for an OPTIMAL image and its backing blob.
///
/// Keeping this as one value prevents the image-create, memory-export and
/// import contract from drifting apart. Direct-optimal scanout images use the
/// DMA_BUF variant; ordinary UMD images use OPAQUE_FD.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OptimalImageTransport {
    OpaqueFd,
    CrossContextDmaBuf,
}

impl OptimalImageTransport {
    const fn handle_type(self) -> u32 {
        match self {
            Self::OpaqueFd => EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD,
            Self::CrossContextDmaBuf => EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF,
        }
    }
}
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const IMAGE_TYPE_2D: u32 = 1;
const FORMAT_R8G8B8A8_UNORM: u32 = 37;
const FORMAT_R8G8B8A8_SRGB: u32 = 43;
const FORMAT_B8G8R8A8_UNORM: u32 = 44;
const FORMAT_B8G8R8A8_SRGB: u32 = 50;
const FORMAT_A2B10G10R10_UNORM_PACK32: u32 = 64;
const FORMAT_R16G16B16A16_SFLOAT: u32 = 97;
// VK_IMAGE_TILING_OPTIMAL = 0, VK_IMAGE_TILING_LINEAR = 1. This was 0 (OPTIMAL),
// so create_linear_scanout_image built a TILED image → device-local-only
// memoryTypeBits (0x3, no host-visible) → choose_host_visible_memory_type failed
// (ScanoutDiag=16 SdgErr=2 / SdgLStg=3). Confirmed against Mesa venus on the same
// NVIDIA host: LINEAR→typebits=0xf (scans out), OPTIMAL→typebits=0x3 (no host-visible).
const IMAGE_TILING_LINEAR: u32 = 1;
const IMAGE_TILING_DRM_FORMAT_MODIFIER: u32 = 1000158000;
const IMAGE_USAGE_TRANSFER_SRC: u32 = 0x0000_0001;
const IMAGE_USAGE_TRANSFER_DST: u32 = 0x0000_0002;
const IMAGE_USAGE_SAMPLED: u32 = 0x0000_0004;
const IMAGE_USAGE_STORAGE: u32 = 0x0000_0008;
const IMAGE_USAGE_COLOR_ATTACHMENT: u32 = 0x0000_0010;
const BUFFER_USAGE_TRANSFER_DST: u32 = 0x0000_0002;
const IMAGE_CREATE_MUTABLE_FORMAT: u32 = 0x0000_0008;
const SHARING_MODE_EXCLUSIVE: u32 = 0;
const IMAGE_LAYOUT_UNDEFINED: u32 = 0;
const IMAGE_LAYOUT_GENERAL: u32 = 1;
const IMAGE_LAYOUT_PREINITIALIZED: u32 = 8;
const SAMPLE_COUNT_1: u32 = 0x0000_0001;
const IMAGE_ASPECT_COLOR: u32 = 0x0000_0001;
const IMAGE_ASPECT_MEMORY_PLANE_0: u32 = 0x0000_0080;
const QUEUE_FAMILY_IGNORED: u32 = u32::MAX;
const QUEUE_FAMILY_EXTERNAL: u32 = u32::MAX - 1;
// Kept for the older diagnostic call sites. Its numeric value has always been
// VK_QUEUE_FAMILY_EXTERNAL (`~1U`), despite the historical name.
const QUEUE_FAMILY_FOREIGN_EXT: u32 = u32::MAX - 1;
const COMMAND_BUFFER_LEVEL_PRIMARY: u32 = 0;
const COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT: u32 = 0x0000_0001;
const COMMAND_BUFFER_USAGE_SIMULTANEOUS_USE: u32 = 0x0000_0004;
const PIPELINE_STAGE_TOP_OF_PIPE: u32 = 0x0000_0001;
const PIPELINE_STAGE_TRANSFER: u32 = 0x0000_1000;
const PIPELINE_STAGE_BOTTOM_OF_PIPE: u32 = 0x0000_2000;
const ACCESS_TRANSFER_WRITE: u32 = 0x0000_1000;
const ACCESS_TRANSFER_READ: u32 = 0x0000_0800;

// ── VkMemoryPropertyFlags bits we require ────────────────────────────────────
const MEMORY_PROPERTY_DEVICE_LOCAL: u32 = 0x1;
const MEMORY_PROPERTY_HOST_VISIBLE: u32 = 0x2;
const MEMORY_PROPERTY_HOST_COHERENT: u32 = 0x4;

/// VK_MAX_MEMORY_TYPES — the fixed array length the host encodes in the
/// memory-properties reply (`vn_encode_VkPhysicalDeviceMemoryProperties_partial`).
const VK_MAX_MEMORY_TYPES: u32 = 32;
/// VK_MAX_MEMORY_HEAPS — likewise for the heap array.
const VK_MAX_MEMORY_HEAPS: u32 = 16;

// ── Ring layout (vn_ring `struct layout`, 64-byte aligned header fields) ──────
const RING_HEAD_OFFSET: u64 = 0;
const RING_TAIL_OFFSET: u64 = 64;
const RING_STATUS_OFFSET: u64 = 128;
const RING_BUFFER_OFFSET: u64 = 192;
/// 128 KiB — power of two, matching the ICD's default.
const RING_BUFFER_SIZE: u32 = 131072;
const RING_EXTRA_OFFSET: u64 = RING_BUFFER_OFFSET + RING_BUFFER_SIZE as u64; // 131264
const RING_EXTRA_SIZE: u64 = 4;
/// Total ring shmem = 192 + 131072 + 4 = 131268.
const RING_SHMEM_SIZE: u64 = RING_BUFFER_OFFSET + RING_BUFFER_SIZE as u64 + RING_EXTRA_SIZE;
/// Idle timeout reported in the ring-create info (ns); cosmetic for our use.
const RING_IDLE_TIMEOUT_NS: u64 = 1_000_000;

/// Ring status bits (`VkRingStatusFlagsMESA`).
const RING_STATUS_IDLE: u32 = 0x1;
const RING_STATUS_FATAL: u32 = 0x2;

/// Reply shmem size — generous for the small replies we read (largest is the
/// memory-properties reply, ~660 bytes).
const REPLY_SHMEM_SIZE: u64 = 4096;

/// Allocation size of the host-visible page-table memory (16 MiB).
const PAGE_TABLE_ALLOC_SIZE: u64 = 16 * 1024 * 1024;

/// Short spin burst before a ring-head wait falls back to PASSIVE 1 ms sleeps
/// (fast replies stay fast; slow ones cost only sleep latency, never a
/// DISPATCH spin).
const RING_SPIN_BURST: u32 = 50_000;
/// PASSIVE wait budget for the ring head advancing past a published seqno
/// (1 ms sleep-polls). A host that has not consumed the ring in this long is
/// genuinely wedged → the client latches `fatal` ([`FatalReason`]).
///
/// ⚠ OPEN OWNER QUESTION (T4a/R603) — the value is NOT derived from a dxgkrnl
/// deadline, and it is deliberately separate from the 5 s budget every host
/// *fence* wait in this file uses. This wait is reachable from `DxgkDdiPresent`
/// (`ddi/display.rs` → `submit_present_blt` → `ensure_present_image` →
/// `ring_command_reply` → `write_to_ring` → here) while the adapter venus mutex
/// is held, so a caller can in principle block a Present for half a minute —
/// which is several times the Windows default `TdrDdiDelay`, the interval after
/// which dxgkrnl declares a driver hung. If that is right, a real wedge is
/// TDR'd long before this budget expires and the constant bounds post-TDR
/// thread residency rather than preventing a hang.
///
/// It is left at 30 s and NOT shortened here: a bounded wait on a real ring-head
/// watermark is a safety contract, and shortening it without measuring how long
/// a legitimately slow host actually takes would trade a rare stall for a
/// frequent false fatal latch. `VnRingWd` now records the milliseconds actually
/// waited at every expiry, which is the measurement that has to come first.
const RING_WAIT_TIMEOUT_MS: u64 = 30_000;

// The command-stream writer and its capacity live in `helios_kmd_logic`: they
// are pure byte arithmetic with no adapter, handle or wdk-sys edge, and the size
// claim that justifies the buffer is worth a host test. See
// `writer_ext_full_create_device_is_332_bytes` there for the real number — the
// comment that used to sit here said "~120 bytes" and was wrong by 212.
use helios_kmd_logic::Writer;

/// Why the venus ring was declared unusable. Each arm names a registry counter
/// so a wedge is distinguishable from every other `DeviceError` in a post-mortem
/// `reg query`, at the default `DiagLevel=0`.
enum FatalReason {
    /// The host set `RING_STATUS_FATAL` — it rejected something we encoded.
    /// Records `VnRingFt=1`.
    HostStatusFatal,
    /// The ring head never advanced past our seqno within
    /// [`RING_WAIT_TIMEOUT_MS`]. Records `VnRingWd` = milliseconds waited, so
    /// the dump distinguishes "gave up at the budget" from a short stall.
    HeadWaitTimeout { elapsed_ms: u64 },
}

/// Diagnostic breadcrumb base for venus bring-up (0x0D00_00xx).
fn diag(code: u32) {
    crate::diag::record(0x0D00_0000 | (code & 0xFFFF));
}

/// The result of [`allocate_host_visible_blob`]: a venus-backed, BAR-visible,
/// CPU-coherent region for VidMm's page-table segment.
#[derive(Clone, Copy)]
pub struct HostVisibleBlob {
    /// The venus `VkDeviceMemory` id, which is also the virtio-gpu `blob_id`.
    pub blob_id: u64,
    /// The virtio-gpu resource id of the mapped blob (for teardown / unref).
    pub res_id: u32,
    /// Guest-physical base inside the host-visible window (`base + offset`).
    pub gpa: u64,
    /// Page-rounded size mapped into the window.
    pub size: u64,
}

pub struct ScanoutImageBlob {
    pub blob: HostVisibleBlob,
    pub image_id: u64,
    pub memory_type_index: u32,
    pub row_pitch: u32,
    pub plane_offset: u32,
}

/// KMD-owned OPTIMAL image exported as a cross-context DMA_BUF resource.
///
/// Unlike [`ScanoutImageBlob`], this image has no linear row layout.  Its
/// Vulkan image create-info and external-memory transport are the complete
/// storage contract; callers must never reinterpret the backing as a buffer.
pub struct OptimalImageBlob {
    pub blob: HostVisibleBlob,
    pub image_id: u64,
    pub memory_type_index: u32,
}

/// Persistent Vulkan objects for copying one authoritative WDDM primary into
/// the adapter-owned LINEAR scanout image.
///
/// Creation is deliberately expensive and submission deliberately cheap:
/// [`VenusClient::prepare_optimal_scanout_copy`] imports the primary once and
/// records one `SIMULTANEOUS_USE` command buffer; each display tick then calls
/// [`VenusClient::submit_prepared_image_copy`], which only enqueues that already
/// recorded buffer. The object must remain alive until
/// [`VenusClient::destroy_prepared_image_copy`] has drained the queue.
#[derive(Clone, Copy)]
pub struct PreparedImageCopy {
    /// True when preparation created/attached/imported the source objects below.
    /// False for a borrowed KMD-created LINEAR source image.
    pub owns_source_alias: bool,
    /// Virtio-gpu resource attached to the kernel Venus context and imported as
    /// `memory_id`. Zero for a borrowed KMD-created source.
    pub source_resource_id: u32,
    /// KMD-device OPTIMAL alias of the source allocation.
    pub source_image_id: u64,
    /// `VkDeviceMemory` imported through `VkImportMemoryResourceInfoMESA`, or
    /// zero for a borrowed source.
    pub source_memory_id: u64,
    /// KMD-owned OPTIMAL BGRA scratch used only when the Windows-selected
    /// primary format differs from the physical BGRA scanout format.
    pub conversion_image_id: u64,
    pub conversion_memory_id: u64,
    /// Pool retaining the one-time UNDEFINED-to-GENERAL transition for the
    /// conversion image.
    pub conversion_init_pool_id: u64,
    /// Pool owning `command_buffer_id`; retained while submissions may be live.
    pub command_pool_id: u64,
    /// Reusable source-acquire/copy/release command buffer.
    pub command_buffer_id: u64,
    /// Persistent adapter-owned destination image baked into the command buffer.
    pub target_image_id: u64,
    pub width: u32,
    pub height: u32,
}

/// Exact external-allocation contract for recreating one ordinary Helios/DXVK
/// shared OPTIMAL Present image in the kernel Venus device.
///
/// Construction validates the shape once. Cache equality includes every Vulkan
/// memory-requirements input rather than relying on resource-id heuristics.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct OptimalPresentImageDesc {
    resource_id: u32,
    allocation_size: u64,
    memory_type_index: u32,
    width: u32,
    height: u32,
    ddi_bind_flags: u32,
    dxgi_format: u32,
    /// The parsed form of `dxgi_format`, stored at construction rather than
    /// re-derived on every read. `new` already had to prove the format was
    /// convertible; spending that proof on an `is_some` test and then
    /// re-deriving it behind a panicking accessor made a `KeBugCheck` reachable
    /// from any struct literal added inside this module (the fields are private
    /// only *outside* `venus.rs`).
    pixel_format: PresentPixelFormat,
    transport: OptimalImageTransport,
}

impl OptimalPresentImageDesc {
    fn new(
        resource_id: u32,
        allocation_size: u64,
        memory_type_index: u32,
        width: u32,
        height: u32,
        ddi_bind_flags: u32,
        dxgi_format: u32,
        transport: OptimalImageTransport,
    ) -> Option<Self> {
        let pixel_format = PresentPixelFormat::from_dxgi(dxgi_format)?;
        (resource_id != 0 && allocation_size != 0 && width != 0 && height != 0).then_some(Self {
            resource_id,
            allocation_size,
            memory_type_index,
            width,
            height,
            ddi_bind_flags,
            dxgi_format,
            pixel_format,
            transport,
        })
    }

    fn pixel_format(self) -> PresentPixelFormat {
        self.pixel_format
    }

    /// Ordinary UMD-created shared images use the renderer's OPAQUE_FD
    /// transport. Keeping this constructor distinct from the DMA_BUF variant
    /// prevents Present from silently importing one allocation with the other
    /// image-memory contract.
    pub fn new_opaque_fd(
        resource_id: u32,
        allocation_size: u64,
        memory_type_index: u32,
        width: u32,
        height: u32,
        ddi_bind_flags: u32,
        dxgi_format: u32,
    ) -> Option<Self> {
        Self::new(
            resource_id,
            allocation_size,
            memory_type_index,
            width,
            height,
            ddi_bind_flags,
            dxgi_format,
            OptimalImageTransport::OpaqueFd,
        )
    }

    /// KMD GDI textures and direct-optimal scanout images are exported through
    /// DMA_BUF/CROSS_DEVICE so render-server contexts can attach them.
    pub fn new_cross_context_dma_buf(
        resource_id: u32,
        allocation_size: u64,
        memory_type_index: u32,
        width: u32,
        height: u32,
        ddi_bind_flags: u32,
        dxgi_format: u32,
    ) -> Option<Self> {
        Self::new(
            resource_id,
            allocation_size,
            memory_type_index,
            width,
            height,
            ddi_bind_flags,
            dxgi_format,
            OptimalImageTransport::CrossContextDmaBuf,
        )
    }
}

/// Exact contract for a KMD-created standard Present destination.
///
/// These allocations are byte-addressed, host-visible Venus blobs. The UMD
/// imports them as `VkBuffer` and performs its own pitch-correct
/// buffer-to-private-image staging before sampling. Giving the same memory an
/// OPTIMAL `VkImage` interpretation is invalid and was the `.146` Present
/// failure on NVIDIA.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PresentBufferDesc {
    resource_id: u32,
    allocation_size: u64,
    memory_type_index: u32,
    width: u32,
    height: u32,
    pitch: u32,
    dxgi_format: u32,
    /// Parsed once in `new`, which already binds it to size the row. See
    /// [`OptimalPresentImageDesc::pixel_format`] for why this is a field.
    pixel_format: PresentPixelFormat,
}

impl PresentBufferDesc {
    /// The venus resource this destination names. Used by the deferred probe to
    /// re-validate liveness before sampling (R320).
    pub fn resource_id(&self) -> u32 {
        self.resource_id
    }

    pub fn new(
        resource_id: u32,
        allocation_size: u64,
        memory_type_index: u32,
        width: u32,
        height: u32,
        pitch: u32,
        dxgi_format: u32,
    ) -> Option<Self> {
        let pixel_format = PresentPixelFormat::from_dxgi(dxgi_format)?;
        let row_bytes = width.checked_mul(pixel_format.bytes_per_pixel())?;
        let content_size = u64::from(pitch).checked_mul(u64::from(height))?;
        (resource_id != 0
            && allocation_size != 0
            && width != 0
            && height != 0
            && pitch >= row_bytes
            && pitch % pixel_format.bytes_per_pixel() == 0
            && content_size <= allocation_size)
            .then_some(Self {
                resource_id,
                allocation_size,
                memory_type_index,
                width,
                height,
                pitch,
                dxgi_format,
                pixel_format,
            })
    }

    fn pixel_format(self) -> PresentPixelFormat {
        self.pixel_format
    }
}

/// Exact destination interpretation selected from the KMD allocation's
/// versioned `kind` field.
///
/// CPU-visible standard allocations are pitched byte buffers; the standard GDI
/// texture subtype and ordinary UMD/Venus allocations are OPTIMAL images.
/// Keeping the alternatives in the type system prevents the `.146` class of
/// bugs where the same external memory was accidentally reinterpreted using the
/// wrong Vulkan object type.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PresentDestinationDesc {
    StandardBuffer(PresentBufferDesc),
    OptimalImage(OptimalPresentImageDesc),
}

impl PresentDestinationDesc {
    fn resource_id(self) -> u32 {
        match self {
            Self::StandardBuffer(desc) => desc.resource_id,
            Self::OptimalImage(desc) => desc.resource_id,
        }
    }

    fn width(self) -> u32 {
        match self {
            Self::StandardBuffer(desc) => desc.width,
            Self::OptimalImage(desc) => desc.width,
        }
    }

    fn height(self) -> u32 {
        match self {
            Self::StandardBuffer(desc) => desc.height,
            Self::OptimalImage(desc) => desc.height,
        }
    }

    fn dxgi_format(self) -> u32 {
        match self {
            Self::StandardBuffer(desc) => desc.dxgi_format,
            Self::OptimalImage(desc) => desc.dxgi_format,
        }
    }
}

#[derive(Clone, Copy)]
struct ImportedOptimalImage {
    desc: OptimalPresentImageDesc,
    image_id: u64,
    memory_id: u64,
}

/// One `VkDeviceMemory` allocation created by [`VenusClient::allocate_memory_blob`]
/// and exposed as exactly one HOST3D resource.
///
/// This is the authoritative same-device identity for a KMD standard allocation.
/// Present may borrow this memory for a buffer binding; it must never manufacture
/// a second imported `VkDeviceMemory` for the same resource.
#[derive(Clone, Copy, PartialEq, Eq)]
struct OwnedMemoryBlob {
    resource_id: u32,
    memory_id: u64,
    allocation_size: u64,
    memory_type_index: u32,
}

/// A Present destination buffer bound to KMD-owned memory.
///
/// The absence of an "owned memory" or "attached resource" variant is
/// intentional: dropping this cache entry destroys only `buffer_id`. The
/// AllocationContext remains the sole owner of the memory and virtio resource.
#[derive(Clone, Copy)]
struct BorrowedPresentBuffer {
    desc: PresentBufferDesc,
    buffer_id: u64,
    memory: OwnedMemoryBlob,
}

struct PreparedPresentBlt {
    source_resource_id: u32,
    destination_resource_id: u32,
    command_pool_id: u64,
    command_buffer_id: u64,
    /// Optional KMD-owned conversion scratch. It is an internal command
    /// dependency only; it never substitutes for either Windows allocation.
    conversion_image_id: u64,
    conversion_memory_id: u64,
    conversion_init_pool_id: u64,
    last_wire_fence_id: u64,
    submit_count: u32,
    probe_done: bool,
}

const MAX_PRESENT_IMAGES: usize = 32;
const MAX_PRESENT_BUFFERS: usize = 16;
const MAX_PRESENT_BLITS: usize = 32;
/// Submissions a source/destination pair must complete before the one-shot
/// destination probe is armed for it (`PresentProbe` knob only).
///
/// 8 rather than 1 because the first submissions of a pair are the ones most
/// likely to race DWM's own surface churn: an early sample can catch the
/// destination between the allocation being opened and the first copy actually
/// retiring, which reads as "the copy did not populate the destination" when
/// nothing is wrong. By the 8th submission the pair is steady state.
const PRESENT_PROBE_AFTER_SUBMITS: u32 = 8;
/// Every owned memory blob is also tracked by VirtioGpu's bounded blob table,
/// so the transport's capacity is an exact upper bound rather than a second,
/// divergent resource limit.
const MAX_OWNED_MEMORY_BLOBS: usize = super::gpu::MAX_BLOBS;

/// Complete D3D11/DXGI swapchain format set that Helios can recreate as an
/// exact Vulkan image alias.
///
/// Values are selected only from the creator's retained DXGI format. Typeless,
/// integer, depth, compressed, and XR-bias formats are intentionally excluded:
/// DXVK does not expose them as importable D3D11 swapchain storage.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PresentPixelFormat {
    Rgba8Unorm,
    Rgba8Srgb,
    Bgra8Unorm,
    Bgrx8Unorm,
    Bgra8Srgb,
    Bgrx8Srgb,
    Rgb10a2Unorm,
    Rgba16Float,
}

impl PresentPixelFormat {
    fn from_dxgi(dxgi_format: u32) -> Option<Self> {
        match dxgi_format {
            10 => Some(Self::Rgba16Float),
            24 => Some(Self::Rgb10a2Unorm),
            28 => Some(Self::Rgba8Unorm),
            29 => Some(Self::Rgba8Srgb),
            87 => Some(Self::Bgra8Unorm),
            88 => Some(Self::Bgrx8Unorm),
            91 => Some(Self::Bgra8Srgb),
            93 => Some(Self::Bgrx8Srgb),
            _ => None,
        }
    }

    fn vk_format(self) -> u32 {
        match self {
            Self::Rgba8Unorm => FORMAT_R8G8B8A8_UNORM,
            Self::Rgba8Srgb => FORMAT_R8G8B8A8_SRGB,
            Self::Bgra8Unorm | Self::Bgrx8Unorm => FORMAT_B8G8R8A8_UNORM,
            Self::Bgra8Srgb | Self::Bgrx8Srgb => FORMAT_B8G8R8A8_SRGB,
            Self::Rgb10a2Unorm => FORMAT_A2B10G10R10_UNORM_PACK32,
            Self::Rgba16Float => FORMAT_R16G16B16A16_SFLOAT,
        }
    }

    fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Rgba16Float => 8,
            _ => 4,
        }
    }
}

/// A kernel mapping of a guest-physical sub-range of the host-visible BAR window.
///
/// Wraps `MmMapIoSpace`/`MmUnmapIoSpace` with RAII. The KMD reads/writes the venus
/// ring and reply buffers through this VA. Created and dropped at PASSIVE_LEVEL.
struct KernelMap {
    va: *mut u8,
    size: u64,
}

impl KernelMap {
    /// Map `[gpa, gpa+size)` into kernel VA with caching chosen from the host's
    /// `map_cache` nibble (`VIRTIO_GPU_MAP_CACHE_*`). Returns `None` on failure.
    fn new(gpa: u64, size: u64, map_cache: u32) -> Option<Self> {
        let caching = match map_cache {
            VIRTIO_GPU_MAP_CACHE_CACHED => _MEMORY_CACHING_TYPE::MmCached,
            VIRTIO_GPU_MAP_CACHE_WC => _MEMORY_CACHING_TYPE::MmWriteCombined,
            VIRTIO_GPU_MAP_CACHE_UNCACHED => _MEMORY_CACHING_TYPE::MmNonCached,
            // Unknown / NONE: default to cached. Host-visible venus memory is WB.
            _ => _MEMORY_CACHING_TYPE::MmCached,
        };
        let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
        pa.QuadPart = gpa as i64;
        // SAFETY: PASSIVE_LEVEL; maps a real, host-backed BAR sub-region (the venus
        // blob was RESOURCE_MAP_BLOB'd into exactly this window range, so the pages
        // are backed). Unmapped exactly once in `Drop`.
        let va = unsafe { MmMapIoSpace(pa, size, caching) } as *mut u8;
        if va.is_null() {
            return None;
        }
        Some(Self { va, size })
    }

    /// Zero the whole mapping (volatile, byte by byte — the region is MMIO).
    fn zero(&self) {
        // SAFETY: `va` owns `size` mapped bytes for our lifetime.
        for i in 0..self.size {
            unsafe { core::ptr::write_volatile(self.va.add(i as usize), 0u8) };
        }
    }

    /// Volatile u32 load at byte `offset` (Acquire-ordered for ring head/status).
    fn load_u32_acquire(&self, offset: u64) -> u32 {
        // SAFETY: `offset+4 <= size` is the caller's invariant (all ring header
        // offsets are < RING_SHMEM_SIZE); aligned 4-byte MMIO read.
        let p = unsafe { self.va.add(offset as usize) } as *const u32;
        let v = unsafe { core::ptr::read_volatile(p) };
        // The producer side of vn_ring loads head/status with acquire; a fence
        // after the volatile read gives the same ordering against later reads.
        fence(Ordering::Acquire);
        v
    }

    /// Volatile u32 store at byte `offset` with a full (SeqCst) fence first — the
    /// vn_ring tail-store contract (a full mfence so the host's acquire load is
    /// ordered after our buffer writes).
    fn store_u32_seqcst(&self, offset: u64, val: u32) {
        // Full barrier: all prior buffer writes are visible before the tail store.
        fence(Ordering::SeqCst);
        // SAFETY: `offset+4 <= size`; aligned 4-byte MMIO write.
        let p = unsafe { self.va.add(offset as usize) } as *mut u32;
        unsafe { core::ptr::write_volatile(p, val) };
        fence(Ordering::SeqCst);
    }

    /// Copy `src` into the ring buffer at free-running counter `cur`, splitting at
    /// the power-of-two wrap. `cur` and the buffer mask follow vn_ring exactly.
    fn write_ring_buffer(&self, cur: u32, src: &[u8]) {
        let mask = RING_BUFFER_SIZE - 1;
        let offset = (cur & mask) as u64;
        let first = core::cmp::min(src.len() as u64, RING_BUFFER_SIZE as u64 - offset);
        // SAFETY: buffer base + in-range offsets; first + second == src.len() and
        // both halves are within [RING_BUFFER_OFFSET, RING_EXTRA_OFFSET).
        unsafe {
            let base = self.va.add(RING_BUFFER_OFFSET as usize);
            for i in 0..first {
                core::ptr::write_volatile(base.add((offset + i) as usize), src[i as usize]);
            }
            let rest = src.len() as u64 - first;
            for i in 0..rest {
                core::ptr::write_volatile(base.add(i as usize), src[(first + i) as usize]);
            }
        }
        // Ensure the buffer bytes land before the caller publishes the tail.
        compiler_fence(Ordering::Release);
    }

    /// Volatile byte load at `offset` (for reply decoding out of the reply shmem).
    fn read_byte(&self, offset: u64) -> u8 {
        // SAFETY: caller bounds-checks `offset < size`.
        unsafe { core::ptr::read_volatile(self.va.add(offset as usize)) }
    }
}

impl Drop for KernelMap {
    fn drop(&mut self) {
        if !self.va.is_null() {
            // SAFETY: `va` came from `MmMapIoSpace` in `new`; unmapped once here.
            unsafe { MmUnmapIoSpace(self.va as *mut _, self.size) };
        }
    }
}

// ── Handing the encoded bytes out ───────────────────────────────────

/// The kernel-side half of [`Writer`]: turn a finished stream into bytes, or
/// refuse loudly.
///
/// `helios_kmd_logic` has no `VirtioError` and no `diag` (that is the point of
/// its absent dependency edge), so it reports overflow as `None`. Naming the
/// refusal and picking the error code is this crate's job.
trait EncodedStream {
    /// The encoded bytes, or `DeviceError` if any write overflowed.
    ///
    /// Records `VnEncOvf` = the `VkCommandTypeEXT` that overflowed.
    /// `record_named_bytes` is NOT `DiagLevel`-gated (unlike [`diag`]), so the
    /// refusal is visible in a `reg query` on a production boot. It writes the
    /// registry, so it must run at PASSIVE — which every venus encoder does:
    /// `ring_wait_until` sleeps via `ctrl::sleep_ms`, so the whole ring API is
    /// PASSIVE-only.
    fn as_slice(&self) -> Result<&[u8], VirtioError>;
}

impl EncodedStream for Writer {
    fn as_slice(&self) -> Result<&[u8], VirtioError> {
        match self.finished() {
            Some(bytes) => Ok(bytes),
            None => {
                crate::diag::record_named_bytes(b"VnEncOvf", self.cmd_type());
                Err(VirtioError::DeviceError)
            }
        }
    }
}

// ── A bounds-checked LE reader over the reply shmem ───────────────────────────

/// Reads little-endian scalars out of the host-written reply buffer. The reply is
/// **untrusted** input: every read is bounds-checked against the mapping size and
/// returns `VirtioError::DeviceError` on overrun, so a malformed reply can never
/// index out of bounds.
struct ReplyReader<'a> {
    map: &'a KernelMap,
    /// Absolute byte offset within the mapping (reply lives at offset 0).
    pos: u64,
    /// Hard cap = reply mapping size.
    end: u64,
}

impl<'a> ReplyReader<'a> {
    fn new(map: &'a KernelMap) -> Self {
        Self {
            map,
            pos: 0,
            end: map.size,
        }
    }

    fn read_u32(&mut self) -> Result<u32, VirtioError> {
        if self.pos + 4 > self.end {
            return Err(VirtioError::DeviceError);
        }
        let mut b = [0u8; 4];
        for (i, slot) in b.iter_mut().enumerate() {
            *slot = self.map.read_byte(self.pos + i as u64);
        }
        self.pos += 4;
        Ok(u32::from_le_bytes(b))
    }

    fn read_i32(&mut self) -> Result<i32, VirtioError> {
        Ok(self.read_u32()? as i32)
    }

    fn read_u64(&mut self) -> Result<u64, VirtioError> {
        if self.pos + 8 > self.end {
            return Err(VirtioError::DeviceError);
        }
        let mut b = [0u8; 8];
        for (i, slot) in b.iter_mut().enumerate() {
            *slot = self.map.read_byte(self.pos + i as u64);
        }
        self.pos += 8;
        Ok(u64::from_le_bytes(b))
    }

    /// Skip `n` bytes, bounds-checked.
    fn skip(&mut self, n: u64) -> Result<(), VirtioError> {
        if self.pos + n > self.end {
            return Err(VirtioError::DeviceError);
        }
        self.pos += n;
        Ok(())
    }
}

/// The persistent venus client owned by the adapter for the device lifetime.
///
/// Holds the ring/reply BAR mappings and the live Vulkan object ids. Dropping it
/// unmaps the kernel mappings; the host-side venus objects and blob resources are
/// torn down implicitly when the persistent virtio context is destroyed (the
/// caller destroys the context in StopDevice and unrefs the page-table blob).
pub struct VenusClient {
    /// Guest-assigned ring handle token (reused everywhere the ring is named).
    ring_id: u64,
    /// Ring shmem blob resource id (for unref on teardown).
    ring_res_id: u32,
    /// Reply shmem blob resource id (for unref on teardown).
    reply_res_id: u32,
    /// Kernel mapping of the ring shmem.
    ring_map: KernelMap,
    /// Kernel mapping of the reply shmem.
    reply_map: KernelMap,
    /// Free-running ring producer counter (vn_ring `ring->cur`).
    cur: u32,
    /// Monotonic notify seqno.
    notify_seqno: u32,
    /// Monotonic virtqueue roundtrip seqno (for the reply-shmem warm-up).
    roundtrip_seqno: u64,
    /// Next guest-assigned Vulkan handle id (0 = NULL, so start at 1).
    next_handle: u64,
    /// The persistent venus 3D context id all commands ride.
    ctx_id: u32,
    /// One-shot destination probe ARMED but not yet run (R320). The probe is a
    /// blocking PASSIVE diagnostic — a 5 s fence wait, a host map round-trip
    /// with 1 ms Busy sleeps, MmMapIoSpace, ~196 volatile reads and 7 registry
    /// writes — and it used to run inside `submit_present_blt`, i.e. on the
    /// Present path with the adapter venus mutex HELD. Since the one-shot is per
    /// source/destination PAIR, a session with the knob enabled could pay that
    /// up to MAX_PRESENT_BLITS times, each a potential multi-second stall of the
    /// compositor. It is now recorded here and drained by the PASSIVE display
    /// worker outside the mutex.
    probe_pending: Option<(PresentBufferDesc, u64)>,
    /// venus instance handle.
    instance_id: u64,
    /// venus device handle.
    device_id: u64,
    /// Graphics queue handle from family 0, queue 0.
    queue_id: u64,
    /// HOST_VISIBLE|HOST_COHERENT memory type chosen during bring-up.
    memory_type_index: u32,
    /// Raw VkMemoryPropertyFlags for physical-device memory types.
    memory_type_flags: [u32; VK_MAX_MEMORY_TYPES as usize],
    memory_type_count: u32,
    /// Poison latch: set when a ring wait exhausts [`RING_WAIT_TIMEOUT_MS`] or
    /// the host reports RING_STATUS_FATAL. A wedged/fatal ring never recovers,
    /// and without the latch every subsequent call re-burned the full wait
    /// budget, which is the 2026-07-03 guest wedge. Once set, every ring command
    /// fails fast with `DeviceError`.
    ///
    /// Write it ONLY through [`VenusClient::latch_fatal`], which names the
    /// reason in the registry. (The previous version of this comment claimed the
    /// allocation path reaches these waits "at DISPATCH_LEVEL under the device
    /// spinlock" and cited a `RING_POLL_SPINS` constant that no longer exists.
    /// It is wrong on both counts and it is load-bearing here: `ring_wait_until`
    /// sleeps via `ctrl::sleep_ms`, so these waits are PASSIVE-only, which is
    /// what makes `latch_fatal`'s registry write legal.)
    fatal: bool,
    /// One-time PREINITIALIZED -> GENERAL -> EXTERNAL setup for the persistent
    /// LINEAR scanout target. The pool/buffer remain live because setup is
    /// intentionally submitted without a fence wait; queue order makes every
    /// later copy execute after it.
    copy_target_image_id: u64,
    copy_target_init_pool_id: u64,
    copy_target_init_command_buffer_id: u64,
    /// App/DWM BLT imports and recorded copies. Both vectors are preallocated
    /// and capacity-bounded. Every access is serialized by
    /// AdapterContext::with_venus_client, so setup/submission/teardown cannot
    /// race through incidental call ordering.
    present_images: Vec<ImportedOptimalImage>,
    present_buffers: Vec<BorrowedPresentBuffer>,
    present_blits: Vec<PreparedPresentBlt>,
    /// Exact identities of `allocate_memory_blob` allocations. This registry
    /// turns a standard Present destination into a checked borrow of the
    /// already-live local `VkDeviceMemory`; no resource-id heuristic or
    /// same-device re-import is permitted.
    owned_memory_blobs: Vec<OwnedMemoryBlob>,
}

impl VenusClient {
    /// The HOST_VISIBLE|HOST_COHERENT venus `memoryTypeIndex` every
    /// [`Self::allocate_memory_blob`] allocation uses — recorded into the
    /// allocation identity so cross-process openers import with the creator's
    /// exact memory type.
    pub fn memory_type_index(&self) -> u32 {
        self.memory_type_index
    }

    /// Allocate a fresh guest handle id.
    fn alloc_handle(&mut self) -> u64 {
        let h = self.next_handle;
        self.next_handle += 1;
        h
    }

    /// Latch the ring poison, naming the reason in the registry.
    ///
    /// This is the ONLY writer of `self.fatal`; `grep -n 'self.fatal = true'`
    /// returning just this body is the invariant. Before it, a wedged ring left
    /// zero evidence on a production boot: the three latch sites recorded
    /// nothing, `diag::record` is `DiagLevel`-gated off by default, and the DDI
    /// returned a bare `DeviceError` indistinguishable from every other one —
    /// destroying the post-mortem for exactly the failure the 30 s budget exists
    /// to survive.
    ///
    /// `record_named_bytes` writes the registry and so must run at PASSIVE. All
    /// three callers are on the PASSIVE ring path: `ring_wait_until` sleeps via
    /// `ctrl::sleep_ms` (`KeDelayExecutionThread`), which is only legal there.
    fn latch_fatal(&mut self, reason: FatalReason) {
        self.fatal = true;
        match reason {
            FatalReason::HostStatusFatal => crate::diag::record_named_bytes(b"VnRingFt", 1),
            FatalReason::HeadWaitTimeout { elapsed_ms } => {
                crate::diag::record_named_bytes(b"VnRingWd", elapsed_ms as u32)
            }
        }
    }

    /// Send a direct (non-ring) venus command via `VIRTIO_GPU_CMD_SUBMIT_3D`. Used
    /// for the ring-bootstrap commands (`vkCreateRingMESA`, `vkNotifyRingMESA`,
    /// `vkSubmit/WaitVirtqueueSeqnoMESA`) which must reach the host before / around
    /// the ring being usable. Blocks at PASSIVE (virtio::ctrl KEVENT wait) until
    /// the device acks the command.
    fn submit_direct(&self, adapter: &AdapterContext, stream: &[u8]) -> Result<(), VirtioError> {
        ctrl::submit_venus_sync(adapter, self.ctx_id, stream)
    }

    /// Publish the ring buffer up to `self.cur` (SeqCst tail store), then nudge the
    /// host if the ring reports idle. Returns the seqno of the just-written command
    /// (== the post-write `cur`). Aborts on a fatal ring status.
    fn publish_and_notify(&mut self, adapter: &AdapterContext) -> Result<u32, VirtioError> {
        if self.fatal {
            return Err(VirtioError::DeviceError);
        }
        let seqno = self.cur;
        self.ring_map.store_u32_seqcst(RING_TAIL_OFFSET, seqno);

        let status = self.ring_map.load_u32_acquire(RING_STATUS_OFFSET);
        if status & RING_STATUS_FATAL != 0 {
            self.latch_fatal(FatalReason::HostStatusFatal);
            return Err(VirtioError::DeviceError);
        }
        // vkNotifyRingMESA(ring, seqno, flags=0): wake the host ring dispatch.
        // Sent UNCONDITIONALLY (not gated on the IDLE status bit): the host idles
        // after a 1 ms timeout and the guest's read of the IDLE bit from the ring
        // shmem is racy/coherency-sensitive — always nudging is correct and cheap.
        // vkNotifyRingMESA is a valid DIRECT command (unlike
        // vkWaitVirtqueueSeqnoMESA, which the host rejects off-ring).
        self.notify_seqno = self.notify_seqno.wrapping_add(1);
        let mut w = Writer::new();
        w.header(CMD_NOTIFY_RING_MESA, 0);
        w.u64(self.ring_id);
        w.u32(self.notify_seqno);
        w.u32(0); // VkRingNotifyFlagsMESA
        self.submit_direct(adapter, w.as_slice()?)?;
        Ok(seqno)
    }

    /// PASSIVE ring-progress wait: run `ready()` until it returns true, with a
    /// short spin burst then 1 ms sleeps, bounded by [`RING_WAIT_TIMEOUT_MS`].
    /// Checks the ring FATAL status each round. Latches `fatal` on timeout.
    fn ring_wait_until(&mut self, ready: impl Fn(&Self) -> bool) -> Result<(), VirtioError> {
        if self.fatal {
            return Err(VirtioError::DeviceError);
        }
        for _ in 0..RING_SPIN_BURST {
            if ready(self) {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        let mut slept_ms: u64 = 0;
        loop {
            if ready(self) {
                return Ok(());
            }
            let status = self.ring_map.load_u32_acquire(RING_STATUS_OFFSET);
            if status & RING_STATUS_FATAL != 0 {
                // Host declared the ring fatal — it never recovers.
                self.latch_fatal(FatalReason::HostStatusFatal);
                return Err(VirtioError::DeviceError);
            }
            if slept_ms >= RING_WAIT_TIMEOUT_MS {
                self.latch_fatal(FatalReason::HeadWaitTimeout {
                    elapsed_ms: slept_ms,
                });
                return Err(VirtioError::DeviceError);
            }
            ctrl::sleep_ms(1);
            slept_ms += 1;
        }
    }

    /// Reserve space and copy `stream` into the ring buffer at `self.cur`,
    /// advancing `cur` (vn_ring producer). Waits (PASSIVE) until the host has
    /// consumed enough to make room (`cur + size - head <= buffer_size`).
    fn write_to_ring(&mut self, stream: &[u8]) -> Result<(), VirtioError> {
        if self.fatal {
            return Err(VirtioError::DeviceError);
        }
        let size = stream.len() as u32;
        if size as u64 > RING_BUFFER_SIZE as u64 {
            return Err(VirtioError::DeviceError);
        }
        // Wait for buffer space (u32 free-running arithmetic, wrap-preserving):
        // occupancy after this write = cur + size - head; must be <= buffer_size.
        let cur = self.cur;
        self.ring_wait_until(move |c| {
            let head = c.ring_map.load_u32_acquire(RING_HEAD_OFFSET);
            cur.wrapping_add(size).wrapping_sub(head) <= RING_BUFFER_SIZE
        })?;
        self.ring_map.write_ring_buffer(self.cur, stream);
        self.cur = self.cur.wrapping_add(size);
        Ok(())
    }

    /// Wait (PASSIVE) until the ring head reaches `seqno` (the host has consumed
    /// and completed the command). Wrap-safe `(i32)(head - seqno) >= 0` compare.
    fn wait_seqno(&mut self, seqno: u32) -> Result<(), VirtioError> {
        self.ring_wait_until(move |c| {
            let head = c.ring_map.load_u32_acquire(RING_HEAD_OFFSET);
            (head.wrapping_sub(seqno)) as i32 >= 0
        })
    }

    /// Issue a command WITHOUT a reply through the ring: write → publish → wait.
    #[allow(dead_code)]
    fn ring_command_noreply(
        &mut self,
        adapter: &AdapterContext,
        stream: &[u8],
    ) -> Result<(), VirtioError> {
        self.write_to_ring(stream)?;
        let seqno = self.publish_and_notify(adapter)?;
        self.wait_seqno(seqno)
    }

    /// Issue a command WITH a reply through the ring. First points the host at the
    /// reply shmem (`vkSetReplyCommandStreamMESA`), then writes the real command
    /// with the GENERATE_REPLY flag, publishes once, and waits. The reply lands at
    /// reply-shmem offset 0; the caller decodes it via a fresh [`ReplyReader`].
    fn ring_command_reply(
        &mut self,
        adapter: &AdapterContext,
        cmd_stream: &[u8],
    ) -> Result<(), VirtioError> {
        // vkSetReplyCommandStreamMESA: point the host at reply shmem [0, size).
        let mut set = Writer::new();
        set.header(CMD_SET_REPLY_COMMAND_STREAM_MESA, 0);
        set.count(true); // simple_pointer(pStream)
        set.u32(self.reply_res_id); // VkCommandStreamDescriptionMESA.resourceId
        set.u64(0); // .offset
        set.u64(REPLY_SHMEM_SIZE); // .size
        self.write_to_ring(set.as_slice()?)?;

        // The real command, with GENERATE_REPLY.
        self.write_to_ring(cmd_stream)?;

        let seqno = self.publish_and_notify(adapter)?;
        self.wait_seqno(seqno)?;
        Ok(())
    }

    /// Warm up the reply shmem so the host maps it before the first reply: a
    /// virtqueue-seqno submit + wait roundtrip (`vkSubmit/WaitVirtqueueSeqnoMESA`,
    /// both direct). seqno is monotonic from 1.
    #[allow(dead_code)]
    fn reply_shmem_roundtrip(&mut self, adapter: &AdapterContext) -> Result<(), VirtioError> {
        self.roundtrip_seqno += 1;
        let seqno = self.roundtrip_seqno;
        let mut sub = Writer::new();
        sub.header(CMD_SUBMIT_VIRTQUEUE_SEQNO_MESA, 0);
        sub.u64(self.ring_id);
        sub.u64(seqno);
        self.submit_direct(adapter, sub.as_slice()?)?;

        let mut wait = Writer::new();
        wait.header(CMD_WAIT_VIRTQUEUE_SEQNO_MESA, 0);
        wait.u64(seqno);
        self.submit_direct(adapter, wait.as_slice()?)
    }

    /// Allocate HOST_VISIBLE|HOST_COHERENT Venus device memory and bind it to a
    /// HOST3D blob. Returns the memory id (`blob_id`) and virtio resource id.
    /// The ring reply wait guarantees `vkAllocateMemory` has EXECUTED before the
    /// blob create references it (guest-side ordering — the host ctrl queue is
    /// never blocked waiting for this client's allocations).
    pub fn allocate_memory_blob(
        &mut self,
        adapter: &AdapterContext,
        size: u64,
        mappable: bool,
        shareable: bool,
    ) -> Result<HostVisibleBlob, VirtioError> {
        if self.owned_memory_blobs.len() >= MAX_OWNED_MEMORY_BLOBS {
            return Err(VirtioError::OutOfMemory);
        }
        let size = round_up_page(size.max(4096));
        let memory_id = self.alloc_handle();
        {
            let mut w = Writer::new();
            w.header(CMD_ALLOCATE_MEMORY, CMD_FLAG_GENERATE_REPLY);
            w.u64(self.device_id);
            w.count(true);
            w.i32(ST_MEMORY_ALLOCATE_INFO);
            if shareable {
                w.count(true);
                w.i32(ST_EXPORT_MEMORY_ALLOCATE_INFO);
                w.count(false);
                w.u32(EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF);
            } else {
                w.count(false);
            }
            w.u64(size);
            w.u32(self.memory_type_index);
            w.count(false);
            w.count(true);
            w.u64(memory_id);
            self.ring_command_reply(adapter, w.as_slice()?)?;

            let mut r = ReplyReader::new(&self.reply_map);
            let cmd = r.read_i32()?;
            if cmd as u32 != CMD_ALLOCATE_MEMORY {
                diag(0x00F6);
                return Err(VirtioError::DeviceError);
            }
            let result = r.read_i32()?;
            if result != 0 {
                diag(0x00F7);
                return Err(VirtioError::DeviceError);
            }
        }

        let mut flags = 0;
        if mappable {
            flags |= VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE;
        }
        if shareable {
            flags |= VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE;
        }
        let res_id = match ctrl::resource_create_blob(
            adapter,
            self.ctx_id,
            VIRTIO_GPU_BLOB_MEM_HOST3D,
            flags,
            memory_id,
            size,
        ) {
            Ok(resource_id) => resource_id,
            Err(e) => {
                // The Vulkan allocation exists but never became an owned blob.
                // Reclaim it through the raw object path; registry-aware
                // `free_memory_blob` is reserved for successfully published
                // allocation identities.
                let _ = self.free_memory_object(adapter, memory_id);
                return Err(e);
            }
        };
        let _ = adapter.with_virtio(|v| v.note_blob_size(res_id, size));
        // Capacity was reserved above, so push cannot allocate. Publish the
        // identity only after both Vulkan allocation and resource creation
        // succeeded.
        self.owned_memory_blobs.push(OwnedMemoryBlob {
            resource_id: res_id,
            memory_id,
            allocation_size: size,
            memory_type_index: self.memory_type_index,
        });
        Ok(HostVisibleBlob {
            blob_id: memory_id,
            res_id,
            gpa: 0,
            size,
        })
    }

    fn choose_host_visible_memory_type(&self, memory_type_bits: u32) -> Option<u32> {
        let mut fallback = None;
        let mut i = 0;
        while i < self.memory_type_count && i < VK_MAX_MEMORY_TYPES {
            if (memory_type_bits & (1u32 << i)) != 0 {
                let flags = self.memory_type_flags[i as usize];
                if (flags & MEMORY_PROPERTY_HOST_VISIBLE) != 0 {
                    if fallback.is_none() {
                        fallback = Some(i);
                    }
                    if (flags & MEMORY_PROPERTY_HOST_COHERENT) != 0 {
                        return Some(i);
                    }
                }
            }
            i += 1;
        }
        fallback
    }

    fn choose_device_local_memory_type(&self, memory_type_bits: u32) -> Option<u32> {
        let mut fallback = None;
        let mut i = 0;
        while i < self.memory_type_count && i < VK_MAX_MEMORY_TYPES {
            if (memory_type_bits & (1u32 << i)) != 0 {
                let flags = self.memory_type_flags[i as usize];
                if fallback.is_none() {
                    fallback = Some(i);
                }
                if (flags & MEMORY_PROPERTY_DEVICE_LOCAL) != 0
                    && (flags & MEMORY_PROPERTY_HOST_VISIBLE) == 0
                {
                    return Some(i);
                }
            }
            i += 1;
        }
        i = 0;
        while i < self.memory_type_count && i < VK_MAX_MEMORY_TYPES {
            if (memory_type_bits & (1u32 << i)) != 0 {
                let flags = self.memory_type_flags[i as usize];
                if (flags & MEMORY_PROPERTY_DEVICE_LOCAL) != 0 {
                    return Some(i);
                }
            }
            i += 1;
        }
        fallback
    }

    fn create_scanout_image(
        &mut self,
        adapter: &AdapterContext,
        width: u32,
        height: u32,
    ) -> Result<u64, VirtioError> {
        let image_id = self.alloc_handle();
        let mut w = Writer::new();
        w.header(CMD_CREATE_IMAGE, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.count(true);
        w.i32(ST_IMAGE_CREATE_INFO);
        // VkExternalMemoryImageCreateInfo -> VkImageDrmFormatModifierListCreateInfoEXT.
        w.count(true);
        w.i32(ST_EXTERNAL_MEMORY_IMAGE_CREATE_INFO);
        w.count(true);
        w.i32(ST_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO);
        w.count(false);
        w.u32(1);
        w.u64(1);
        w.u64(DRM_FORMAT_MOD_LINEAR);
        w.u32(EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF);
        w.u32(0); // flags
        w.u32(IMAGE_TYPE_2D);
        w.u32(FORMAT_B8G8R8A8_UNORM);
        w.u32(width);
        w.u32(height);
        w.u32(1); // depth
        w.u32(1); // mipLevels
        w.u32(1); // arrayLayers
        w.u32(SAMPLE_COUNT_1);
        w.u32(IMAGE_TILING_DRM_FORMAT_MODIFIER);
        w.u32(IMAGE_USAGE_TRANSFER_DST);
        w.u32(SHARING_MODE_EXCLUSIVE);
        w.u32(0); // queueFamilyIndexCount
        w.count(false);
        w.u32(IMAGE_LAYOUT_UNDEFINED);
        w.count(false); // pAllocator
        w.count(true);
        w.u64(image_id);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_CREATE_IMAGE {
            diag(0x0101);
            return Err(VirtioError::DeviceError);
        }
        let result = r.read_i32()?;
        if result != 0 {
            diag(0x0102);
            return Err(VirtioError::DeviceError);
        }
        if r.read_u64()? == 0 || r.read_u64()? == 0 {
            diag(0x0103);
            return Err(VirtioError::DeviceError);
        }
        Ok(image_id)
    }

    fn create_linear_scanout_image(
        &mut self,
        adapter: &AdapterContext,
        width: u32,
        height: u32,
    ) -> Result<u64, VirtioError> {
        let image_id = self.alloc_handle();
        let mut w = Writer::new();
        w.header(CMD_CREATE_IMAGE, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.count(true);
        w.i32(ST_IMAGE_CREATE_INFO);
        // VkExternalMemoryImageCreateInfo only. This matches the Linux KMS probe
        // that reached QEMU's egl-headless dmabuf import on NVIDIA.
        w.count(true);
        w.i32(ST_EXTERNAL_MEMORY_IMAGE_CREATE_INFO);
        w.count(false);
        w.u32(EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF);
        w.u32(0); // flags
        w.u32(IMAGE_TYPE_2D);
        w.u32(FORMAT_B8G8R8A8_UNORM);
        w.u32(width);
        w.u32(height);
        w.u32(1); // depth
        w.u32(1); // mipLevels
        w.u32(1); // arrayLayers
        w.u32(SAMPLE_COUNT_1);
        w.u32(IMAGE_TILING_LINEAR);
        w.u32(IMAGE_USAGE_TRANSFER_SRC | IMAGE_USAGE_TRANSFER_DST);
        w.u32(SHARING_MODE_EXCLUSIVE);
        w.u32(0); // queueFamilyIndexCount
        w.count(false);
        w.u32(IMAGE_LAYOUT_PREINITIALIZED);
        w.count(false); // pAllocator
        w.count(true);
        w.u64(image_id);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_CREATE_IMAGE {
            crate::diag::record_named_bytes(b"SdgLImg", 0xE0);
            diag(0x0120);
            return Err(VirtioError::DeviceError);
        }
        let result = r.read_i32()?;
        if result != 0 {
            // Raw VkResult of the LINEAR external-DMA_BUF image create — this is
            // the most likely NVIDIA-venus rejection point for the CachyOS shape.
            crate::diag::record_named_bytes(b"SdgLImg", result as u32);
            diag(0x0121);
            return Err(VirtioError::DeviceError);
        }
        if r.read_u64()? == 0 || r.read_u64()? == 0 {
            crate::diag::record_named_bytes(b"SdgLImg", 0xE1);
            diag(0x0122);
            return Err(VirtioError::DeviceError);
        }
        Ok(image_id)
    }

    /// Rebuild the exact ordinary Helios/DXVK shared-primary image shape in the
    /// kernel Venus device. `ddi_bind_flags` is the authoritative CreateResource
    /// value retained by the allocation context; deriving usage from geometry
    /// would be a content-corrupting heuristic on NVIDIA.
    fn create_optimal_present_image_alias(
        &mut self,
        adapter: &AdapterContext,
        width: u32,
        height: u32,
        ddi_bind_flags: u32,
        dxgi_format: u32,
        transport: OptimalImageTransport,
    ) -> Result<u64, VirtioError> {
        let vk_format = PresentPixelFormat::from_dxgi(dxgi_format)
            .ok_or(VirtioError::DeviceError)?
            .vk_format();
        // D3D11/DXVK starts every texture with transfer source+destination. The
        // DDI pipeline bits are numerically identical for SRV (0x8) and RTV
        // (0x20); DDI UAV (0x100) is translated to API UAV (0x80), hence the
        // separate test below. PRESENT (0x80) is deliberately not STORAGE.
        const DDI_BIND_SHADER_RESOURCE: u32 = 0x0000_0008;
        const DDI_BIND_RENDER_TARGET: u32 = 0x0000_0020;
        const DDI_BIND_UNORDERED_ACCESS: u32 = 0x0000_0100;
        let mut usage = IMAGE_USAGE_TRANSFER_SRC | IMAGE_USAGE_TRANSFER_DST;
        if ddi_bind_flags & DDI_BIND_SHADER_RESOURCE != 0 {
            usage |= IMAGE_USAGE_SAMPLED;
        }
        if ddi_bind_flags & DDI_BIND_RENDER_TARGET != 0 {
            usage |= IMAGE_USAGE_COLOR_ATTACHMENT;
        }
        if ddi_bind_flags & DDI_BIND_UNORDERED_ACCESS != 0 {
            usage |= IMAGE_USAGE_STORAGE;
        }

        let image_id = self.alloc_handle();
        let mut w = Writer::new();
        w.header(CMD_CREATE_IMAGE, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.count(true);
        w.i32(ST_IMAGE_CREATE_INFO);
        // Imported ordinary UMD resources use OPAQUE_FD. KMD-created GDI
        // textures use DMA_BUF because virglrenderer's render-server proxy can
        // carry DMA_BUF, but cannot attach an OPAQUE_FD to another context.
        w.count(true);
        w.i32(ST_EXTERNAL_MEMORY_IMAGE_CREATE_INFO);
        w.count(false);
        w.u32(transport.handle_type());
        // Shared color images retain MUTABLE_FORMAT but intentionally have no
        // format-list pNext (DXVK suppresses that list to disable per-image
        // compression metadata which cannot survive cross-device imports).
        w.u32(IMAGE_CREATE_MUTABLE_FORMAT);
        w.u32(IMAGE_TYPE_2D);
        w.u32(vk_format);
        w.u32(width);
        w.u32(height);
        w.u32(1); // depth
        w.u32(1); // mipLevels
        w.u32(1); // arrayLayers
        w.u32(SAMPLE_COUNT_1);
        w.u32(0); // VK_IMAGE_TILING_OPTIMAL
        w.u32(usage);
        w.u32(SHARING_MODE_EXCLUSIVE);
        w.u32(0); // queueFamilyIndexCount
        w.count(false);
        w.u32(IMAGE_LAYOUT_UNDEFINED);
        w.count(false); // pAllocator
        w.count(true);
        w.u64(image_id);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_CREATE_IMAGE {
            diag(0x0130);
            return Err(VirtioError::DeviceError);
        }
        let result = r.read_i32()?;
        if result != 0 {
            crate::diag::record_named_bytes(b"CpImgVr", result as u32);
            diag(0x0131);
            return Err(VirtioError::DeviceError);
        }
        if r.read_u64()? == 0 || r.read_u64()? == 0 {
            diag(0x0132);
            return Err(VirtioError::DeviceError);
        }
        Ok(image_id)
    }

    /// Create a private OPTIMAL transfer image used only as an explicit
    /// conversion step between the two allocations supplied by Windows.
    ///
    /// Unlike an imported Present image this has no external-memory pNext and
    /// no virtio resource identity. It cannot be mistaken for, opened as, or
    /// substituted for either the source or redirected destination.
    fn create_present_conversion_image(
        &mut self,
        adapter: &AdapterContext,
        width: u32,
        height: u32,
        vk_format: u32,
    ) -> Result<u64, VirtioError> {
        if width == 0 || height == 0 {
            return Err(VirtioError::DeviceError);
        }
        let image_id = self.alloc_handle();
        let mut w = Writer::new();
        w.header(CMD_CREATE_IMAGE, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.count(true);
        w.i32(ST_IMAGE_CREATE_INFO);
        w.count(false); // pNext: internal image, no external-memory contract
        w.u32(0); // flags
        w.u32(IMAGE_TYPE_2D);
        w.u32(vk_format);
        w.u32(width);
        w.u32(height);
        w.u32(1); // depth
        w.u32(1); // mipLevels
        w.u32(1); // arrayLayers
        w.u32(SAMPLE_COUNT_1);
        w.u32(0); // VK_IMAGE_TILING_OPTIMAL
        w.u32(IMAGE_USAGE_TRANSFER_SRC | IMAGE_USAGE_TRANSFER_DST);
        w.u32(SHARING_MODE_EXCLUSIVE);
        w.u32(0); // queueFamilyIndexCount
        w.count(false);
        w.u32(IMAGE_LAYOUT_UNDEFINED);
        w.count(false); // pAllocator
        w.count(true);
        w.u64(image_id);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        if r.read_i32()? as u32 != CMD_CREATE_IMAGE || r.read_i32()? != 0 {
            return Err(VirtioError::DeviceError);
        }
        if r.read_u64()? == 0 || r.read_u64()? == 0 {
            return Err(VirtioError::DeviceError);
        }
        Ok(image_id)
    }

    /// Allocate non-exported memory dedicated to an internal conversion image.
    fn allocate_present_conversion_memory(
        &mut self,
        adapter: &AdapterContext,
        image_id: u64,
        size: u64,
        memory_type_index: u32,
    ) -> Result<u64, VirtioError> {
        let memory_id = self.alloc_handle();
        let mut w = Writer::new();
        w.header(CMD_ALLOCATE_MEMORY, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.count(true);
        w.i32(ST_MEMORY_ALLOCATE_INFO);
        w.count(true);
        w.i32(ST_MEMORY_DEDICATED_ALLOCATE_INFO);
        w.count(false);
        w.u64(image_id);
        w.u64(0); // buffer
        w.u64(size);
        w.u32(memory_type_index);
        w.count(false); // pAllocator
        w.count(true);
        w.u64(memory_id);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        if r.read_i32()? as u32 != CMD_ALLOCATE_MEMORY || r.read_i32()? != 0 {
            return Err(VirtioError::DeviceError);
        }
        Ok(memory_id)
    }

    /// Transition a newly-created conversion image to GENERAL exactly once.
    /// Queue ordering makes every later reusable Present command execute after
    /// this setup submission; retaining the pool preserves command lifetime.
    fn initialize_present_conversion_image(
        &mut self,
        adapter: &AdapterContext,
        image_id: u64,
    ) -> Result<u64, VirtioError> {
        let pool_id = self.create_command_pool(adapter)?;
        let command_buffer_id = match self.allocate_command_buffer(adapter, pool_id) {
            Ok(id) => id,
            Err(e) => {
                let _ = self.destroy_command_pool(adapter, pool_id);
                return Err(e);
            }
        };
        let record_result = (|| {
            self.begin_command_buffer(adapter, command_buffer_id)?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                image_id,
                PIPELINE_STAGE_TOP_OF_PIPE,
                PIPELINE_STAGE_TRANSFER,
                0,
                ACCESS_TRANSFER_WRITE,
                IMAGE_LAYOUT_UNDEFINED,
                IMAGE_LAYOUT_GENERAL,
                QUEUE_FAMILY_IGNORED,
                QUEUE_FAMILY_IGNORED,
            )?;
            self.end_command_buffer(adapter, command_buffer_id)
        })();
        if let Err(e) = record_result {
            let _ = self.destroy_command_pool(adapter, pool_id);
            return Err(e);
        }
        // On an ambiguous submission failure, leave the pool alive for Venus
        // context teardown. Destroying it could free an in-flight command.
        self.queue_submit_command_buffer(adapter, command_buffer_id, 0)?;
        Ok(pool_id)
    }

    fn create_bound_present_conversion_image(
        &mut self,
        adapter: &AdapterContext,
        width: u32,
        height: u32,
        format: PresentPixelFormat,
    ) -> Result<(u64, u64), VirtioError> {
        let image_id =
            self.create_present_conversion_image(adapter, width, height, format.vk_format())?;
        let (required_size, memory_type_bits) =
            match self.image_memory_requirements(adapter, image_id) {
                Ok(requirements) => requirements,
                Err(e) => {
                    let _ = self.destroy_image_on_ring(adapter, image_id);
                    return Err(e);
                }
            };
        let memory_type_index = match self.choose_device_local_memory_type(memory_type_bits) {
            Some(index) => index,
            None => {
                let _ = self.destroy_image_on_ring(adapter, image_id);
                return Err(VirtioError::DeviceError);
            }
        };
        let memory_id = match self.allocate_present_conversion_memory(
            adapter,
            image_id,
            required_size,
            memory_type_index,
        ) {
            Ok(id) => id,
            Err(e) => {
                let _ = self.destroy_image_on_ring(adapter, image_id);
                return Err(e);
            }
        };
        if let Err(e) = self.bind_image_memory(adapter, image_id, memory_id) {
            let _ = self.destroy_image_on_ring(adapter, image_id);
            let _ = self.free_memory_object(adapter, memory_id);
            return Err(e);
        }
        Ok((image_id, memory_id))
    }

    fn image_memory_requirements(
        &mut self,
        adapter: &AdapterContext,
        image_id: u64,
    ) -> Result<(u64, u32), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_GET_IMAGE_MEMORY_REQUIREMENTS, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.u64(image_id);
        w.count(true);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_GET_IMAGE_MEMORY_REQUIREMENTS {
            diag(0x0104);
            return Err(VirtioError::DeviceError);
        }
        if r.read_u64()? == 0 {
            diag(0x0105);
            return Err(VirtioError::DeviceError);
        }
        let size = r.read_u64()?;
        let _alignment = r.read_u64()?;
        let memory_type_bits = r.read_u32()?;
        Ok((size, memory_type_bits))
    }

    /// Create a plain local buffer for a same-device memory borrow.
    ///
    /// The allocation was created by this Venus device and is not being
    /// imported here. Giving this buffer an external-memory create chain would
    /// assert a second, false ownership/transport contract.
    fn create_local_present_destination_buffer(
        &mut self,
        adapter: &AdapterContext,
        size: u64,
    ) -> Result<u64, VirtioError> {
        let buffer_id = self.alloc_handle();
        let mut w = Writer::new();
        w.header(CMD_CREATE_BUFFER, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.count(true);
        w.i32(ST_BUFFER_CREATE_INFO);
        w.count(false); // pNext: local buffer, no external-memory contract
        w.u32(0); // VkBufferCreateFlags
        w.u64(size);
        w.u32(BUFFER_USAGE_TRANSFER_DST);
        w.u32(SHARING_MODE_EXCLUSIVE);
        w.u32(0); // queueFamilyIndexCount
        w.count(false);
        w.count(false); // pAllocator
        w.count(true);
        w.u64(buffer_id);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_CREATE_BUFFER {
            return Err(VirtioError::DeviceError);
        }
        let result = r.read_i32()?;
        if result != 0 {
            crate::diag::record_named_bytes(b"PBBufVr", result as u32);
            return Err(VirtioError::DeviceError);
        }
        if r.read_u64()? == 0 || r.read_u64()? == 0 {
            return Err(VirtioError::DeviceError);
        }
        Ok(buffer_id)
    }

    fn buffer_memory_requirements(
        &mut self,
        adapter: &AdapterContext,
        buffer_id: u64,
    ) -> Result<(u64, u32), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_GET_BUFFER_MEMORY_REQUIREMENTS, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.u64(buffer_id);
        w.count(true);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_GET_BUFFER_MEMORY_REQUIREMENTS || r.read_u64()? == 0 {
            return Err(VirtioError::DeviceError);
        }
        let size = r.read_u64()?;
        let _alignment = r.read_u64()?;
        let memory_type_bits = r.read_u32()?;
        Ok((size, memory_type_bits))
    }

    fn allocate_dedicated_image_memory(
        &mut self,
        adapter: &AdapterContext,
        image_id: u64,
        size: u64,
        memory_type_index: u32,
    ) -> Result<u64, VirtioError> {
        let memory_id = self.alloc_handle();
        let mut w = Writer::new();
        w.header(CMD_ALLOCATE_MEMORY, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.count(true);
        w.i32(ST_MEMORY_ALLOCATE_INFO);
        // VkExportMemoryAllocateInfo -> VkMemoryDedicatedAllocateInfo.
        w.count(true);
        w.i32(ST_EXPORT_MEMORY_ALLOCATE_INFO);
        w.count(true);
        w.i32(ST_MEMORY_DEDICATED_ALLOCATE_INFO);
        w.count(false);
        w.u64(image_id);
        w.u64(0); // buffer
        w.u32(EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF);
        w.u64(size);
        w.u32(memory_type_index);
        w.count(false);
        w.count(true);
        w.u64(memory_id);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_ALLOCATE_MEMORY {
            diag(0x0106);
            return Err(VirtioError::DeviceError);
        }
        if r.read_i32()? != 0 {
            diag(0x0107);
            return Err(VirtioError::DeviceError);
        }
        Ok(memory_id)
    }

    fn allocate_export_image_memory(
        &mut self,
        adapter: &AdapterContext,
        size: u64,
        memory_type_index: u32,
    ) -> Result<u64, VirtioError> {
        let memory_id = self.alloc_handle();
        let mut w = Writer::new();
        w.header(CMD_ALLOCATE_MEMORY, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.count(true);
        w.i32(ST_MEMORY_ALLOCATE_INFO);
        w.count(true);
        w.i32(ST_EXPORT_MEMORY_ALLOCATE_INFO);
        w.count(false);
        w.u32(EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF);
        w.u64(size);
        w.u32(memory_type_index);
        w.count(false);
        w.count(true);
        w.u64(memory_id);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_ALLOCATE_MEMORY {
            crate::diag::record_named_bytes(b"SdgLMem", 0xE0);
            diag(0x0123);
            return Err(VirtioError::DeviceError);
        }
        let result = r.read_i32()?;
        if result != 0 {
            // Raw VkResult of the exportable-DMA_BUF allocation for the linear
            // scanout image (dedicated-less export alloc).
            crate::diag::record_named_bytes(b"SdgLMem", result as u32);
            diag(0x0124);
            return Err(VirtioError::DeviceError);
        }
        Ok(memory_id)
    }

    /// Import an already-live HOST3D resource into this kernel Venus device.
    /// The resource must first be attached to `self.ctx_id`.
    fn allocate_imported_resource_memory(
        &mut self,
        adapter: &AdapterContext,
        resource_id: u32,
        allocation_size: u64,
        memory_type_index: u32,
    ) -> Result<u64, VirtioError> {
        let memory_id = self.alloc_handle();
        let mut w = Writer::new();
        w.header(CMD_ALLOCATE_MEMORY, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.count(true);
        w.i32(ST_MEMORY_ALLOCATE_INFO);
        // VkImportMemoryResourceInfoMESA
        w.count(true);
        w.i32(ST_IMPORT_MEMORY_RESOURCE_INFO_MESA);
        w.count(false);
        w.u32(resource_id);
        w.u64(allocation_size);
        w.u32(memory_type_index);
        w.count(false); // pAllocator
        w.count(true);
        w.u64(memory_id);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_ALLOCATE_MEMORY {
            diag(0x0133);
            return Err(VirtioError::DeviceError);
        }
        let result = r.read_i32()?;
        if result != 0 {
            crate::diag::record_named_bytes(b"CpMemVr", result as u32);
            diag(0x0134);
            return Err(VirtioError::DeviceError);
        }
        Ok(memory_id)
    }

    fn bind_image_memory(
        &mut self,
        adapter: &AdapterContext,
        image_id: u64,
        memory_id: u64,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_BIND_IMAGE_MEMORY, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.u64(image_id);
        w.u64(memory_id);
        w.u64(0);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_BIND_IMAGE_MEMORY {
            diag(0x0108);
            return Err(VirtioError::DeviceError);
        }
        if r.read_i32()? != 0 {
            diag(0x0109);
            return Err(VirtioError::DeviceError);
        }
        Ok(())
    }

    fn bind_buffer_memory(
        &mut self,
        adapter: &AdapterContext,
        buffer_id: u64,
        memory_id: u64,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_BIND_BUFFER_MEMORY, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.u64(buffer_id);
        w.u64(memory_id);
        w.u64(0);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_BIND_BUFFER_MEMORY || r.read_i32()? != 0 {
            return Err(VirtioError::DeviceError);
        }
        Ok(())
    }

    fn image_subresource_layout(
        &mut self,
        adapter: &AdapterContext,
        image_id: u64,
        aspect_mask: u32,
    ) -> Result<(u64, u64), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_GET_IMAGE_SUBRESOURCE_LAYOUT, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.u64(image_id);
        w.count(true);
        w.u32(aspect_mask);
        w.u32(0);
        w.u32(0);
        w.count(true);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_GET_IMAGE_SUBRESOURCE_LAYOUT {
            diag(0x010A);
            return Err(VirtioError::DeviceError);
        }
        if r.read_u64()? == 0 {
            diag(0x010B);
            return Err(VirtioError::DeviceError);
        }
        let offset = r.read_u64()?;
        let _size = r.read_u64()?;
        let row_pitch = r.read_u64()?;
        let _array_pitch = r.read_u64()?;
        let _depth_pitch = r.read_u64()?;
        Ok((offset, row_pitch))
    }

    fn get_device_queue(&mut self, adapter: &AdapterContext) -> Result<(), VirtioError> {
        let queue_id = self.alloc_handle();
        let mut w = Writer::new();
        w.header(CMD_GET_DEVICE_QUEUE_2, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.count(true); // pQueueInfo
        w.i32(ST_DEVICE_QUEUE_INFO_2);
        w.count(true); // pNext: VkDeviceQueueTimelineInfoMESA
        w.i32(ST_DEVICE_QUEUE_TIMELINE_INFO_MESA);
        w.count(false);
        w.u32(1); // ringIdx; 0 is the renderer's CPU timeline.
        w.u32(0); // flags
        w.u32(0); // queueFamilyIndex
        w.u32(0); // queueIndex
        w.count(true);
        w.u64(queue_id);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_GET_DEVICE_QUEUE_2 {
            diag(0x0110);
            return Err(VirtioError::DeviceError);
        }
        if r.read_u64()? == 0 {
            diag(0x0111);
            return Err(VirtioError::DeviceError);
        }
        let returned = r.read_u64()?;
        self.queue_id = if returned != 0 { returned } else { queue_id };
        Ok(())
    }

    fn create_command_pool(&mut self, adapter: &AdapterContext) -> Result<u64, VirtioError> {
        let pool_id = self.alloc_handle();
        let mut w = Writer::new();
        w.header(CMD_CREATE_COMMAND_POOL, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.count(true);
        w.i32(ST_COMMAND_POOL_CREATE_INFO);
        w.count(false);
        w.u32(0); // flags
        w.u32(0); // queueFamilyIndex
        w.count(false); // pAllocator
        w.count(true);
        w.u64(pool_id);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_CREATE_COMMAND_POOL {
            diag(0x0112);
            return Err(VirtioError::DeviceError);
        }
        if r.read_i32()? != 0 {
            diag(0x0113);
            return Err(VirtioError::DeviceError);
        }
        if r.read_u64()? == 0 {
            diag(0x0114);
            return Err(VirtioError::DeviceError);
        }
        let returned = r.read_u64()?;
        Ok(if returned != 0 { returned } else { pool_id })
    }

    fn destroy_command_pool(
        &mut self,
        adapter: &AdapterContext,
        pool_id: u64,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_DESTROY_COMMAND_POOL, 0);
        w.u64(self.device_id);
        w.u64(pool_id);
        w.count(false);
        self.ring_command_noreply(adapter, w.as_slice()?)
    }

    fn allocate_command_buffer(
        &mut self,
        adapter: &AdapterContext,
        pool_id: u64,
    ) -> Result<u64, VirtioError> {
        let command_buffer_id = self.alloc_handle();
        let mut w = Writer::new();
        w.header(CMD_ALLOCATE_COMMAND_BUFFERS, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.count(true);
        w.i32(ST_COMMAND_BUFFER_ALLOCATE_INFO);
        w.count(false);
        w.u64(pool_id);
        w.u32(COMMAND_BUFFER_LEVEL_PRIMARY);
        w.u32(1);
        w.u64(1); // pCommandBuffers array_size
        w.u64(command_buffer_id);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_ALLOCATE_COMMAND_BUFFERS {
            diag(0x0115);
            return Err(VirtioError::DeviceError);
        }
        if r.read_i32()? != 0 {
            diag(0x0116);
            return Err(VirtioError::DeviceError);
        }
        if r.read_u64()? == 0 {
            diag(0x0117);
            return Err(VirtioError::DeviceError);
        }
        let returned = r.read_u64()?;
        Ok(if returned != 0 {
            returned
        } else {
            command_buffer_id
        })
    }

    fn begin_command_buffer(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: u64,
    ) -> Result<(), VirtioError> {
        self.begin_command_buffer_with_flags(
            adapter,
            command_buffer_id,
            COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT,
        )
    }

    fn begin_command_buffer_with_flags(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: u64,
        usage_flags: u32,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_BEGIN_COMMAND_BUFFER, CMD_FLAG_GENERATE_REPLY);
        w.u64(command_buffer_id);
        w.count(true);
        w.i32(ST_COMMAND_BUFFER_BEGIN_INFO);
        w.count(false);
        w.u32(usage_flags);
        w.count(false);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_BEGIN_COMMAND_BUFFER {
            diag(0x0118);
            return Err(VirtioError::DeviceError);
        }
        if r.read_i32()? != 0 {
            diag(0x0119);
            return Err(VirtioError::DeviceError);
        }
        Ok(())
    }

    fn cmd_image_barrier(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: u64,
        image_id: u64,
        src_stage: u32,
        dst_stage: u32,
        src_access: u32,
        dst_access: u32,
        old_layout: u32,
        new_layout: u32,
        src_queue_family: u32,
        dst_queue_family: u32,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_PIPELINE_BARRIER, 0);
        w.u64(command_buffer_id);
        w.u32(src_stage);
        w.u32(dst_stage);
        w.u32(0); // dependencyFlags
        w.u32(0); // memoryBarrierCount
        w.count(false);
        w.u32(0); // bufferMemoryBarrierCount
        w.count(false);
        w.u32(1); // imageMemoryBarrierCount
        w.u64(1); // pImageMemoryBarriers array_size
        w.i32(ST_IMAGE_MEMORY_BARRIER);
        w.count(false);
        w.u32(src_access);
        w.u32(dst_access);
        w.u32(old_layout);
        w.u32(new_layout);
        w.u32(src_queue_family);
        w.u32(dst_queue_family);
        w.u64(image_id);
        w.u32(IMAGE_ASPECT_COLOR);
        w.u32(0);
        w.u32(1);
        w.u32(0);
        w.u32(1);
        self.ring_command_noreply(adapter, w.as_slice()?)
    }

    fn cmd_buffer_barrier(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: u64,
        buffer_id: u64,
        buffer_size: u64,
        src_stage: u32,
        dst_stage: u32,
        src_access: u32,
        dst_access: u32,
        src_queue_family: u32,
        dst_queue_family: u32,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_PIPELINE_BARRIER, 0);
        w.u64(command_buffer_id);
        w.u32(src_stage);
        w.u32(dst_stage);
        w.u32(0); // dependencyFlags
        w.u32(0); // memoryBarrierCount
        w.count(false);
        w.u32(1); // bufferMemoryBarrierCount
        w.u64(1); // pBufferMemoryBarriers array_size
        w.i32(ST_BUFFER_MEMORY_BARRIER);
        w.count(false);
        w.u32(src_access);
        w.u32(dst_access);
        w.u32(src_queue_family);
        w.u32(dst_queue_family);
        w.u64(buffer_id);
        w.u64(0); // offset
        w.u64(buffer_size);
        w.u32(0); // imageMemoryBarrierCount
        w.count(false);
        self.ring_command_noreply(adapter, w.as_slice()?)
    }

    fn cmd_copy_image(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: u64,
        source_image_id: u64,
        target_image_id: u64,
        width: u32,
        height: u32,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_COPY_IMAGE, 0);
        w.u64(command_buffer_id);
        w.u64(source_image_id);
        w.u32(IMAGE_LAYOUT_GENERAL);
        w.u64(target_image_id);
        w.u32(IMAGE_LAYOUT_GENERAL);
        w.u32(1); // regionCount
        w.u64(1); // pRegions array_size
                  // VkImageCopy.srcSubresource
        w.u32(IMAGE_ASPECT_COLOR);
        w.u32(0); // mipLevel
        w.u32(0); // baseArrayLayer
        w.u32(1); // layerCount
                  // srcOffset
        w.i32(0);
        w.i32(0);
        w.i32(0);
        // dstSubresource
        w.u32(IMAGE_ASPECT_COLOR);
        w.u32(0);
        w.u32(0);
        w.u32(1);
        // dstOffset
        w.i32(0);
        w.i32(0);
        w.i32(0);
        // extent
        w.u32(width);
        w.u32(height);
        w.u32(1);
        self.ring_command_noreply(adapter, w.as_slice()?)
    }

    fn cmd_blit_image(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: u64,
        source_image_id: u64,
        target_image_id: u64,
        width: u32,
        height: u32,
    ) -> Result<(), VirtioError> {
        let width = i32::try_from(width).map_err(|_| VirtioError::DeviceError)?;
        let height = i32::try_from(height).map_err(|_| VirtioError::DeviceError)?;
        let mut w = Writer::new();
        w.header(CMD_BLIT_IMAGE, 0);
        w.u64(command_buffer_id);
        w.u64(source_image_id);
        w.u32(IMAGE_LAYOUT_GENERAL);
        w.u64(target_image_id);
        w.u32(IMAGE_LAYOUT_GENERAL);
        w.u32(1); // regionCount
        w.u64(1); // pRegions array_size
                  // VkImageBlit.srcSubresource
        w.u32(IMAGE_ASPECT_COLOR);
        w.u32(0); // mipLevel
        w.u32(0); // baseArrayLayer
        w.u32(1); // layerCount
        w.u64(2); // srcOffsets array_size
        w.i32(0);
        w.i32(0);
        w.i32(0);
        w.i32(width);
        w.i32(height);
        w.i32(1);
        // VkImageBlit.dstSubresource
        w.u32(IMAGE_ASPECT_COLOR);
        w.u32(0);
        w.u32(0);
        w.u32(1);
        w.u64(2); // dstOffsets array_size
        w.i32(0);
        w.i32(0);
        w.i32(0);
        w.i32(width);
        w.i32(height);
        w.i32(1);
        w.u32(0); // VK_FILTER_NEAREST
        self.ring_command_noreply(adapter, w.as_slice()?)
    }

    fn cmd_copy_image_to_buffer(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: u64,
        source_image_id: u64,
        destination_buffer_id: u64,
        width: u32,
        height: u32,
        pitch: u32,
        bytes_per_pixel: u32,
    ) -> Result<(), VirtioError> {
        if bytes_per_pixel == 0
            || pitch < width.saturating_mul(bytes_per_pixel)
            || pitch % bytes_per_pixel != 0
        {
            return Err(VirtioError::DeviceError);
        }

        let mut w = Writer::new();
        w.header(CMD_COPY_IMAGE_TO_BUFFER, 0);
        w.u64(command_buffer_id);
        w.u64(source_image_id);
        w.u32(IMAGE_LAYOUT_GENERAL);
        w.u64(destination_buffer_id);
        w.u32(1); // regionCount
        w.u64(1); // pRegions array_size
        w.u64(0); // bufferOffset
        w.u32(pitch / bytes_per_pixel); // bufferRowLength in texels
        w.u32(height); // bufferImageHeight in texels
        w.u32(IMAGE_ASPECT_COLOR);
        w.u32(0); // mipLevel
        w.u32(0); // baseArrayLayer
        w.u32(1); // layerCount
        w.i32(0); // imageOffset.x
        w.i32(0); // imageOffset.y
        w.i32(0); // imageOffset.z
        w.u32(width);
        w.u32(height);
        w.u32(1);
        self.ring_command_noreply(adapter, w.as_slice()?)
    }

    fn cmd_clear_color_image(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: u64,
        image_id: u64,
    ) -> Result<(), VirtioError> {
        let one = 1.0f32.to_bits();
        let zero = 0.0f32.to_bits();
        let mut w = Writer::new();
        w.header(CMD_CLEAR_COLOR_IMAGE, 0);
        w.u64(command_buffer_id);
        w.u64(image_id);
        w.u32(IMAGE_LAYOUT_GENERAL);
        w.count(true);
        w.u32(2); // VkClearColorValue union tag: uint32[4]
        w.u64(4); // array_size
        w.u32(one); // R
        w.u32(zero); // G
        w.u32(one); // B
        w.u32(one); // A
        w.u32(1); // rangeCount
        w.u64(1); // pRanges array_size
        w.u32(IMAGE_ASPECT_COLOR);
        w.u32(0);
        w.u32(1);
        w.u32(0);
        w.u32(1);
        self.ring_command_noreply(adapter, w.as_slice()?)
    }

    fn end_command_buffer(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: u64,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_END_COMMAND_BUFFER, CMD_FLAG_GENERATE_REPLY);
        w.u64(command_buffer_id);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_END_COMMAND_BUFFER {
            diag(0x011A);
            return Err(VirtioError::DeviceError);
        }
        if r.read_i32()? != 0 {
            diag(0x011B);
            return Err(VirtioError::DeviceError);
        }
        Ok(())
    }

    fn create_fence(&mut self, adapter: &AdapterContext) -> Result<u64, VirtioError> {
        let fence_id = self.alloc_handle();
        let mut w = Writer::new();
        w.header(CMD_CREATE_FENCE, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.count(true);
        w.i32(ST_FENCE_CREATE_INFO);
        w.count(false);
        w.u32(0); // flags
        w.count(false); // pAllocator
        w.count(true);
        w.u64(fence_id);
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_CREATE_FENCE {
            diag(0x011E);
            return Err(VirtioError::DeviceError);
        }
        if r.read_i32()? != 0 {
            diag(0x011F);
            return Err(VirtioError::DeviceError);
        }
        if r.read_u64()? == 0 {
            diag(0x0120);
            return Err(VirtioError::DeviceError);
        }
        let returned = r.read_u64()?;
        if returned != fence_id {
            diag(0x0121);
            return Err(VirtioError::DeviceError);
        }
        Ok(fence_id)
    }

    fn wait_for_fence(
        &mut self,
        adapter: &AdapterContext,
        fence_id: u64,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_WAIT_FOR_FENCES, CMD_FLAG_GENERATE_REPLY);
        w.u64(self.device_id);
        w.u32(1); // fenceCount
        w.u64(1); // pFences array_size
        w.u64(fence_id);
        w.u32(1); // waitAll
        w.u64(5_000_000_000); // 5 s
        self.ring_command_reply(adapter, w.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_WAIT_FOR_FENCES {
            diag(0x0122);
            return Err(VirtioError::DeviceError);
        }
        let result = r.read_i32()?;
        if result != 0 {
            diag(0x0123);
            return Err(VirtioError::DeviceError);
        }
        Ok(())
    }

    fn destroy_fence(
        &mut self,
        adapter: &AdapterContext,
        fence_id: u64,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_DESTROY_FENCE, 0);
        w.u64(self.device_id);
        w.u64(fence_id);
        w.count(false); // pAllocator
        self.ring_command_noreply(adapter, w.as_slice()?)
    }

    /// Enqueue an empty fence marker. A later wait on this fence completes only
    /// after every previously submitted copy on the ordered graphics queue.
    fn queue_submit_fence_marker(
        &mut self,
        adapter: &AdapterContext,
        fence_id: u64,
    ) -> Result<(), VirtioError> {
        let mut submit = Writer::new();
        submit.header(CMD_QUEUE_SUBMIT, CMD_FLAG_GENERATE_REPLY);
        submit.u64(self.queue_id);
        submit.u32(0); // submitCount
        submit.count(false); // pSubmits
        submit.u64(fence_id);
        self.ring_command_reply(adapter, submit.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        if r.read_i32()? as u32 != CMD_QUEUE_SUBMIT || r.read_i32()? != 0 {
            diag(0x0135);
            return Err(VirtioError::DeviceError);
        }
        Ok(())
    }

    fn queue_submit_command_buffer(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: u64,
        fence_id: u64,
    ) -> Result<(), VirtioError> {
        let mut submit = Writer::new();
        submit.header(CMD_QUEUE_SUBMIT, CMD_FLAG_GENERATE_REPLY);
        submit.u64(self.queue_id);
        submit.u32(1); // submitCount
        submit.u64(1); // pSubmits array_size
        submit.i32(ST_SUBMIT_INFO);
        submit.count(false);
        submit.u32(0); // waitSemaphoreCount
        submit.count(false);
        submit.count(false);
        submit.u32(1); // commandBufferCount
        submit.u64(1);
        submit.u64(command_buffer_id);
        submit.u32(0); // signalSemaphoreCount
        submit.count(false);
        submit.u64(fence_id); // fence
        self.ring_command_reply(adapter, submit.as_slice()?)?;

        let mut r = ReplyReader::new(&self.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_QUEUE_SUBMIT {
            diag(0x011C);
            return Err(VirtioError::DeviceError);
        }
        if r.read_i32()? != 0 {
            diag(0x011D);
            return Err(VirtioError::DeviceError);
        }

        Ok(())
    }

    fn destroy_image_on_ring(
        &mut self,
        adapter: &AdapterContext,
        image_id: u64,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_DESTROY_IMAGE, 0);
        w.u64(self.device_id);
        w.u64(image_id);
        w.count(false);
        self.ring_command_noreply(adapter, w.as_slice()?)
    }

    fn destroy_buffer_on_ring(
        &mut self,
        adapter: &AdapterContext,
        buffer_id: u64,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_DESTROY_BUFFER, 0);
        w.u64(self.device_id);
        w.u64(buffer_id);
        w.count(false);
        self.ring_command_noreply(adapter, w.as_slice()?)
    }

    fn cleanup_imported_source_alias(
        &mut self,
        adapter: &AdapterContext,
        resource_id: u32,
        image_id: u64,
        memory_id: u64,
    ) -> Result<(), VirtioError> {
        if image_id != 0 {
            self.destroy_image_on_ring(adapter, image_id)?;
        }
        if memory_id != 0 {
            self.free_memory_blob(adapter, memory_id)?;
        }
        ctrl::ctx_detach_resource(adapter, self.ctx_id, resource_id)
    }

    fn cleanup_borrowed_present_buffer(
        &mut self,
        adapter: &AdapterContext,
        buffer_id: u64,
    ) -> Result<(), VirtioError> {
        if buffer_id != 0 {
            self.destroy_buffer_on_ring(adapter, buffer_id)?;
        }
        Ok(())
    }

    /// Import one validated ordinary shared OPTIMAL image into the persistent
    /// KMD Venus device. The imported image exactly reproduces the creator's
    /// extent, usage and external-memory allocation contract.
    fn import_optimal_present_image(
        &mut self,
        adapter: &AdapterContext,
        desc: OptimalPresentImageDesc,
    ) -> Result<ImportedOptimalImage, VirtioError> {
        if desc.memory_type_index >= self.memory_type_count {
            return Err(VirtioError::DeviceError);
        }

        ctrl::attach_resource_checked(adapter, self.ctx_id, desc.resource_id)?;
        let image_id = match self.create_optimal_present_image_alias(
            adapter,
            desc.width,
            desc.height,
            desc.ddi_bind_flags,
            desc.dxgi_format,
            desc.transport,
        ) {
            Ok(image_id) => image_id,
            Err(e) => {
                let _ = ctrl::ctx_detach_resource(adapter, self.ctx_id, desc.resource_id);
                return Err(e);
            }
        };

        let (required_size, memory_type_bits) = match self
            .image_memory_requirements(adapter, image_id)
        {
            Ok(requirements) => requirements,
            Err(e) => {
                let _ = self.cleanup_imported_source_alias(adapter, desc.resource_id, image_id, 0);
                return Err(e);
            }
        };
        if required_size > desc.allocation_size
            || (memory_type_bits & (1u32 << desc.memory_type_index)) == 0
        {
            let _ = self.cleanup_imported_source_alias(adapter, desc.resource_id, image_id, 0);
            return Err(VirtioError::DeviceError);
        }

        let memory_id = match self.allocate_imported_resource_memory(
            adapter,
            desc.resource_id,
            desc.allocation_size,
            desc.memory_type_index,
        ) {
            Ok(memory_id) => memory_id,
            Err(e) => {
                let _ = self.cleanup_imported_source_alias(adapter, desc.resource_id, image_id, 0);
                return Err(e);
            }
        };
        if let Err(e) = self.bind_image_memory(adapter, image_id, memory_id) {
            let _ =
                self.cleanup_imported_source_alias(adapter, desc.resource_id, image_id, memory_id);
            return Err(e);
        }

        Ok(ImportedOptimalImage {
            desc,
            image_id,
            memory_id,
        })
    }

    /// Return a cached immutable import, creating it once if needed.
    ///
    /// A resource id observed with a different descriptor is rejected loudly:
    /// reusing an alias with mismatched Vulkan requirements is memory unsafe,
    /// while importing two conflicting shapes would make cache identity
    /// heuristic.
    fn ensure_present_image(
        &mut self,
        adapter: &AdapterContext,
        desc: OptimalPresentImageDesc,
    ) -> Result<ImportedOptimalImage, VirtioError> {
        if let Some(image) = self
            .present_images
            .iter()
            .find(|image| image.desc.resource_id == desc.resource_id)
            .copied()
        {
            return if image.desc == desc {
                Ok(image)
            } else {
                Err(VirtioError::DeviceError)
            };
        }
        if self.present_images.len() >= MAX_PRESENT_IMAGES {
            return Err(VirtioError::OutOfMemory);
        }
        let image = self.import_optimal_present_image(adapter, desc)?;
        self.present_images.push(image);
        Ok(image)
    }

    fn borrow_present_buffer(
        &mut self,
        adapter: &AdapterContext,
        desc: PresentBufferDesc,
    ) -> Result<BorrowedPresentBuffer, VirtioError> {
        if desc.memory_type_index >= self.memory_type_count {
            return Err(VirtioError::DeviceError);
        }

        crate::diag::record_named_bytes(b"PBImRs", desc.resource_id);
        crate::diag::record_named_bytes(b"PBImSt", 1);
        let Some(memory) = self
            .owned_memory_blobs
            .iter()
            .find(|blob| blob.resource_id == desc.resource_id)
            .copied()
        else {
            // PitchedStandardBuffer is a strict local-memory contract. A
            // resource not created by allocate_memory_blob must be represented
            // by another destination type; importing it heuristically here
            // would recreate the failure this path is designed to prevent.
            crate::diag::record_named_bytes(b"PBImSt", 0xE1);
            return Err(VirtioError::DeviceError);
        };
        if memory.allocation_size != desc.allocation_size
            || memory.memory_type_index != desc.memory_type_index
        {
            crate::diag::record_named_bytes(b"PBImSt", 0xE2);
            return Err(VirtioError::DeviceError);
        }

        crate::diag::record_named_bytes(b"PBImSt", 2);
        let buffer_id =
            self.create_local_present_destination_buffer(adapter, desc.allocation_size)?;

        crate::diag::record_named_bytes(b"PBImSt", 3);
        let (required_size, memory_type_bits) =
            match self.buffer_memory_requirements(adapter, buffer_id) {
                Ok(requirements) => requirements,
                Err(e) => {
                    let _ = self.cleanup_borrowed_present_buffer(adapter, buffer_id);
                    return Err(e);
                }
            };
        crate::diag::record_named_bytes(b"PBImRq", required_size as u32);
        crate::diag::record_named_bytes(b"PBImBt", memory_type_bits);
        if required_size > desc.allocation_size
            || (memory_type_bits & (1u32 << desc.memory_type_index)) == 0
        {
            crate::diag::record_named_bytes(b"PBImSt", 0xE3);
            let _ = self.cleanup_borrowed_present_buffer(adapter, buffer_id);
            return Err(VirtioError::DeviceError);
        }

        crate::diag::record_named_bytes(b"PBImSt", 4);
        if let Err(e) = self.bind_buffer_memory(adapter, buffer_id, memory.memory_id) {
            let _ = self.cleanup_borrowed_present_buffer(adapter, buffer_id);
            return Err(e);
        }
        crate::diag::record_named_bytes(b"PBImSt", 0x10);

        Ok(BorrowedPresentBuffer {
            desc,
            buffer_id,
            memory,
        })
    }

    fn ensure_present_buffer(
        &mut self,
        adapter: &AdapterContext,
        desc: PresentBufferDesc,
    ) -> Result<BorrowedPresentBuffer, VirtioError> {
        if let Some(buffer) = self
            .present_buffers
            .iter()
            .find(|buffer| buffer.desc.resource_id == desc.resource_id)
            .copied()
        {
            return if buffer.desc == desc {
                Ok(buffer)
            } else {
                Err(VirtioError::DeviceError)
            };
        }
        if self.present_buffers.len() >= MAX_PRESENT_BUFFERS {
            return Err(VirtioError::OutOfMemory);
        }
        let buffer = self.borrow_present_buffer(adapter, desc)?;
        self.present_buffers.push(buffer);
        Ok(buffer)
    }

    /// Put the persistent KMD LINEAR image into GENERAL layout and external
    /// ownership exactly once. The setup submission is nonblocking and its
    /// command objects remain alive for the Venus-client lifetime; all later
    /// copies use the same ordered queue and therefore execute after setup.
    fn ensure_linear_copy_target_ready(
        &mut self,
        adapter: &AdapterContext,
        target_image_id: u64,
    ) -> Result<(), VirtioError> {
        if self.copy_target_image_id == target_image_id {
            return Ok(());
        }
        if target_image_id == 0 {
            // No target to prepare against. Route through the ungated fault path:
            // diag() is DiagLevel-gated, so on a default boot the old refusal was
            // completely silent and surfaced only as ScCpy=0xE / CpCpy=0xE3.
            diag(0x0136);
            crate::diag::fault(crate::diag::FaultCounter::CpTgtE, 0);
            return Err(VirtioError::DeviceError);
        }
        if self.copy_target_image_id != 0 {
            // RETARGET, not a refusal. The old code failed here permanently, and
            // the failure was reachable on any resolution change: on the fallback
            // copy path production_linear_scanout mints a NEW LINEAR target
            // whenever the cached extent stops matching, and
            // submit_primary_scanout_copy explicitly handles a changed target by
            // destroying the old PreparedImageCopy and preparing a new one. Both
            // prepare entry points then hit this branch, so every subsequent
            // SetVidPnSourceAddress returned STATUS_DEVICE_NOT_READY for the life
            // of the VenusClient. Resize is item 1 of the stability charter.
            //
            // Drain first: the same fence sequence destroy_prepared_image_copy
            // uses, so the host is provably done with the old pool before it is
            // destroyed. This is a bounded wait on a real fence (5 s inside
            // wait_fence), not a sleep - keep it.
            let fence_id = self.create_fence(adapter)?;
            self.queue_submit_fence_marker(adapter, fence_id)?;
            self.wait_for_fence(adapter, fence_id)?;
            self.destroy_fence(adapter, fence_id)?;
            if self.copy_target_init_pool_id != 0 {
                self.destroy_command_pool(adapter, self.copy_target_init_pool_id)?;
            }
            // NOT the old target image: it is owned by dedicated_scanout_image /
            // the adapter's cache, never by this client.
            self.copy_target_image_id = 0;
            self.copy_target_init_pool_id = 0;
            // WRITE-ONLY, see T6/k-venus-17: destroying the pool already freed
            // its command buffer, so this field has no reader. Zeroed here for
            // consistency rather than given an invented read.
            self.copy_target_init_command_buffer_id = 0;
            crate::diag::record_named_bytes(b"CpTgtSw", target_image_id as u32);
            // Fall through to the normal first-time setup below.
        }

        let pool_id = self.create_command_pool(adapter)?;
        let command_buffer_id = match self.allocate_command_buffer(adapter, pool_id) {
            Ok(id) => id,
            Err(e) => {
                let _ = self.destroy_command_pool(adapter, pool_id);
                return Err(e);
            }
        };
        let record_result = (|| {
            self.begin_command_buffer(adapter, command_buffer_id)?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                target_image_id,
                PIPELINE_STAGE_TOP_OF_PIPE,
                PIPELINE_STAGE_TRANSFER,
                0,
                ACCESS_TRANSFER_WRITE,
                IMAGE_LAYOUT_PREINITIALIZED,
                IMAGE_LAYOUT_GENERAL,
                QUEUE_FAMILY_IGNORED,
                QUEUE_FAMILY_IGNORED,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                target_image_id,
                PIPELINE_STAGE_TRANSFER,
                PIPELINE_STAGE_BOTTOM_OF_PIPE,
                ACCESS_TRANSFER_WRITE,
                0,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                0,
                QUEUE_FAMILY_EXTERNAL,
            )?;
            self.end_command_buffer(adapter, command_buffer_id)
        })();
        if let Err(e) = record_result {
            let _ = self.destroy_command_pool(adapter, pool_id);
            return Err(e);
        }

        // Publish lifetime before queue submit: if transport failure makes the
        // submission result ambiguous, retaining the pool is always safe while
        // destroying a possibly-pending command buffer is not.
        self.copy_target_image_id = target_image_id;
        self.copy_target_init_pool_id = pool_id;
        self.copy_target_init_command_buffer_id = command_buffer_id;
        self.queue_submit_command_buffer(adapter, command_buffer_id, 0)
    }

    /// Record the common reusable EXTERNAL-acquire, GENERAL-layout image copy,
    /// and EXTERNAL-release sequence. Both source forms (imported OPTIMAL alias
    /// and borrowed KMD LINEAR image) use this exact ownership protocol.
    fn record_reusable_image_copy(
        &mut self,
        adapter: &AdapterContext,
        source_image_id: u64,
        target_image_id: u64,
        width: u32,
        height: u32,
    ) -> Result<(u64, u64), VirtioError> {
        if source_image_id == 0
            || target_image_id == 0
            || source_image_id == target_image_id
            || width == 0
            || height == 0
        {
            return Err(VirtioError::DeviceError);
        }

        let command_pool_id = self.create_command_pool(adapter)?;
        let command_buffer_id = match self.allocate_command_buffer(adapter, command_pool_id) {
            Ok(id) => id,
            Err(e) => {
                let _ = self.destroy_command_pool(adapter, command_pool_id);
                return Err(e);
            }
        };

        let record_result = (|| {
            self.begin_command_buffer_with_flags(
                adapter,
                command_buffer_id,
                COMMAND_BUFFER_USAGE_SIMULTANEOUS_USE,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                source_image_id,
                PIPELINE_STAGE_TOP_OF_PIPE,
                PIPELINE_STAGE_TRANSFER,
                0,
                ACCESS_TRANSFER_READ,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                QUEUE_FAMILY_EXTERNAL,
                0,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                target_image_id,
                PIPELINE_STAGE_TOP_OF_PIPE,
                PIPELINE_STAGE_TRANSFER,
                0,
                ACCESS_TRANSFER_WRITE,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                QUEUE_FAMILY_EXTERNAL,
                0,
            )?;
            self.cmd_copy_image(
                adapter,
                command_buffer_id,
                source_image_id,
                target_image_id,
                width,
                height,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                source_image_id,
                PIPELINE_STAGE_TRANSFER,
                PIPELINE_STAGE_BOTTOM_OF_PIPE,
                ACCESS_TRANSFER_READ,
                0,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                0,
                QUEUE_FAMILY_EXTERNAL,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                target_image_id,
                PIPELINE_STAGE_TRANSFER,
                PIPELINE_STAGE_BOTTOM_OF_PIPE,
                ACCESS_TRANSFER_WRITE,
                0,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                0,
                QUEUE_FAMILY_EXTERNAL,
            )?;
            self.end_command_buffer(adapter, command_buffer_id)
        })();
        if let Err(e) = record_result {
            let _ = self.destroy_command_pool(adapter, command_pool_id);
            return Err(e);
        }
        Ok((command_pool_id, command_buffer_id))
    }

    /// Reusable external-image BLT for authoritative source/destination formats
    /// that require Vulkan numeric/color conversion.
    fn record_reusable_image_blit(
        &mut self,
        adapter: &AdapterContext,
        source_image_id: u64,
        target_image_id: u64,
        width: u32,
        height: u32,
    ) -> Result<(u64, u64), VirtioError> {
        if source_image_id == 0
            || target_image_id == 0
            || source_image_id == target_image_id
            || width == 0
            || height == 0
        {
            return Err(VirtioError::DeviceError);
        }

        let command_pool_id = self.create_command_pool(adapter)?;
        let command_buffer_id = match self.allocate_command_buffer(adapter, command_pool_id) {
            Ok(id) => id,
            Err(e) => {
                let _ = self.destroy_command_pool(adapter, command_pool_id);
                return Err(e);
            }
        };
        let record_result = (|| {
            self.begin_command_buffer_with_flags(
                adapter,
                command_buffer_id,
                COMMAND_BUFFER_USAGE_SIMULTANEOUS_USE,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                source_image_id,
                PIPELINE_STAGE_TOP_OF_PIPE,
                PIPELINE_STAGE_TRANSFER,
                0,
                ACCESS_TRANSFER_READ,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                QUEUE_FAMILY_EXTERNAL,
                0,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                target_image_id,
                PIPELINE_STAGE_TOP_OF_PIPE,
                PIPELINE_STAGE_TRANSFER,
                0,
                ACCESS_TRANSFER_WRITE,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                QUEUE_FAMILY_EXTERNAL,
                0,
            )?;
            self.cmd_blit_image(
                adapter,
                command_buffer_id,
                source_image_id,
                target_image_id,
                width,
                height,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                source_image_id,
                PIPELINE_STAGE_TRANSFER,
                PIPELINE_STAGE_BOTTOM_OF_PIPE,
                ACCESS_TRANSFER_READ,
                0,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                0,
                QUEUE_FAMILY_EXTERNAL,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                target_image_id,
                PIPELINE_STAGE_TRANSFER,
                PIPELINE_STAGE_BOTTOM_OF_PIPE,
                ACCESS_TRANSFER_WRITE,
                0,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                0,
                QUEUE_FAMILY_EXTERNAL,
            )?;
            self.end_command_buffer(adapter, command_buffer_id)
        })();
        if let Err(e) = record_result {
            let _ = self.destroy_command_pool(adapter, command_pool_id);
            return Err(e);
        }
        Ok((command_pool_id, command_buffer_id))
    }

    /// Convert one authoritative primary into an OPTIMAL BGRA scratch image,
    /// then copy that exact byte layout into the adapter-owned LINEAR scanout.
    ///
    /// Keeping the conversion destination OPTIMAL avoids assuming that the
    /// physical device supports `VK_FORMAT_FEATURE_BLIT_DST_BIT` for a LINEAR
    /// external image. The final copy is format-identical BGRA-to-BGRA.
    fn record_reusable_converted_image_copy(
        &mut self,
        adapter: &AdapterContext,
        source_image_id: u64,
        conversion_image_id: u64,
        target_image_id: u64,
        width: u32,
        height: u32,
    ) -> Result<(u64, u64), VirtioError> {
        if source_image_id == 0
            || conversion_image_id == 0
            || target_image_id == 0
            || source_image_id == conversion_image_id
            || conversion_image_id == target_image_id
            || source_image_id == target_image_id
            || width == 0
            || height == 0
        {
            return Err(VirtioError::DeviceError);
        }

        let command_pool_id = self.create_command_pool(adapter)?;
        let command_buffer_id = match self.allocate_command_buffer(adapter, command_pool_id) {
            Ok(id) => id,
            Err(e) => {
                let _ = self.destroy_command_pool(adapter, command_pool_id);
                return Err(e);
            }
        };
        let record_result = (|| {
            self.begin_command_buffer_with_flags(
                adapter,
                command_buffer_id,
                COMMAND_BUFFER_USAGE_SIMULTANEOUS_USE,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                source_image_id,
                PIPELINE_STAGE_TOP_OF_PIPE,
                PIPELINE_STAGE_TRANSFER,
                0,
                ACCESS_TRANSFER_READ,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                QUEUE_FAMILY_EXTERNAL,
                0,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                conversion_image_id,
                PIPELINE_STAGE_TRANSFER,
                PIPELINE_STAGE_TRANSFER,
                ACCESS_TRANSFER_READ | ACCESS_TRANSFER_WRITE,
                ACCESS_TRANSFER_WRITE,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                QUEUE_FAMILY_IGNORED,
                QUEUE_FAMILY_IGNORED,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                target_image_id,
                PIPELINE_STAGE_TOP_OF_PIPE,
                PIPELINE_STAGE_TRANSFER,
                0,
                ACCESS_TRANSFER_WRITE,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                QUEUE_FAMILY_EXTERNAL,
                0,
            )?;
            self.cmd_blit_image(
                adapter,
                command_buffer_id,
                source_image_id,
                conversion_image_id,
                width,
                height,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                conversion_image_id,
                PIPELINE_STAGE_TRANSFER,
                PIPELINE_STAGE_TRANSFER,
                ACCESS_TRANSFER_WRITE,
                ACCESS_TRANSFER_READ,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                QUEUE_FAMILY_IGNORED,
                QUEUE_FAMILY_IGNORED,
            )?;
            self.cmd_copy_image(
                adapter,
                command_buffer_id,
                conversion_image_id,
                target_image_id,
                width,
                height,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                source_image_id,
                PIPELINE_STAGE_TRANSFER,
                PIPELINE_STAGE_BOTTOM_OF_PIPE,
                ACCESS_TRANSFER_READ,
                0,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                0,
                QUEUE_FAMILY_EXTERNAL,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                target_image_id,
                PIPELINE_STAGE_TRANSFER,
                PIPELINE_STAGE_BOTTOM_OF_PIPE,
                ACCESS_TRANSFER_WRITE,
                0,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                0,
                QUEUE_FAMILY_EXTERNAL,
            )?;
            self.end_command_buffer(adapter, command_buffer_id)
        })();
        if let Err(e) = record_result {
            let _ = self.destroy_command_pool(adapter, command_pool_id);
            return Err(e);
        }
        Ok((command_pool_id, command_buffer_id))
    }

    /// Record one reusable full-surface Present BLT from an imported OPTIMAL
    /// source image into the pitched buffer backing a KMD standard allocation.
    fn record_reusable_present_blt(
        &mut self,
        adapter: &AdapterContext,
        source_image_id: u64,
        destination_buffer_id: u64,
        destination_size: u64,
        width: u32,
        height: u32,
        pitch: u32,
        bytes_per_pixel: u32,
    ) -> Result<(u64, u64), VirtioError> {
        if source_image_id == 0
            || destination_buffer_id == 0
            || destination_size == 0
            || width == 0
            || height == 0
            || bytes_per_pixel == 0
            || pitch < width.saturating_mul(bytes_per_pixel)
            || pitch % bytes_per_pixel != 0
        {
            return Err(VirtioError::DeviceError);
        }

        let command_pool_id = self.create_command_pool(adapter)?;
        let command_buffer_id = match self.allocate_command_buffer(adapter, command_pool_id) {
            Ok(id) => id,
            Err(e) => {
                let _ = self.destroy_command_pool(adapter, command_pool_id);
                return Err(e);
            }
        };

        let record_result = (|| {
            self.begin_command_buffer_with_flags(
                adapter,
                command_buffer_id,
                COMMAND_BUFFER_USAGE_SIMULTANEOUS_USE,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                source_image_id,
                PIPELINE_STAGE_TOP_OF_PIPE,
                PIPELINE_STAGE_TRANSFER,
                0,
                ACCESS_TRANSFER_READ,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                QUEUE_FAMILY_EXTERNAL,
                0,
            )?;
            self.cmd_buffer_barrier(
                adapter,
                command_buffer_id,
                destination_buffer_id,
                destination_size,
                PIPELINE_STAGE_TOP_OF_PIPE,
                PIPELINE_STAGE_TRANSFER,
                0,
                ACCESS_TRANSFER_WRITE,
                QUEUE_FAMILY_EXTERNAL,
                0,
            )?;
            self.cmd_copy_image_to_buffer(
                adapter,
                command_buffer_id,
                source_image_id,
                destination_buffer_id,
                width,
                height,
                pitch,
                bytes_per_pixel,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                source_image_id,
                PIPELINE_STAGE_TRANSFER,
                PIPELINE_STAGE_BOTTOM_OF_PIPE,
                ACCESS_TRANSFER_READ,
                0,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                0,
                QUEUE_FAMILY_EXTERNAL,
            )?;
            self.cmd_buffer_barrier(
                adapter,
                command_buffer_id,
                destination_buffer_id,
                destination_size,
                PIPELINE_STAGE_TRANSFER,
                PIPELINE_STAGE_BOTTOM_OF_PIPE,
                ACCESS_TRANSFER_WRITE,
                0,
                0,
                QUEUE_FAMILY_EXTERNAL,
            )?;
            self.end_command_buffer(adapter, command_buffer_id)
        })();
        if let Err(e) = record_result {
            let _ = self.destroy_command_pool(adapter, command_pool_id);
            return Err(e);
        }
        Ok((command_pool_id, command_buffer_id))
    }

    /// Convert the authoritative source into a KMD-owned scratch image carrying
    /// the exact destination format, then copy those bytes into Windows'
    /// redirected standard allocation.
    fn record_reusable_converted_present_blt(
        &mut self,
        adapter: &AdapterContext,
        source_image_id: u64,
        conversion_image_id: u64,
        destination_buffer_id: u64,
        destination_size: u64,
        width: u32,
        height: u32,
        pitch: u32,
        bytes_per_pixel: u32,
    ) -> Result<(u64, u64), VirtioError> {
        if source_image_id == 0
            || conversion_image_id == 0
            || destination_buffer_id == 0
            || destination_size == 0
            || width == 0
            || height == 0
            || bytes_per_pixel == 0
            || pitch < width.saturating_mul(bytes_per_pixel)
            || pitch % bytes_per_pixel != 0
        {
            return Err(VirtioError::DeviceError);
        }

        let command_pool_id = self.create_command_pool(adapter)?;
        let command_buffer_id = match self.allocate_command_buffer(adapter, command_pool_id) {
            Ok(id) => id,
            Err(e) => {
                let _ = self.destroy_command_pool(adapter, command_pool_id);
                return Err(e);
            }
        };
        let record_result = (|| {
            self.begin_command_buffer_with_flags(
                adapter,
                command_buffer_id,
                COMMAND_BUFFER_USAGE_SIMULTANEOUS_USE,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                source_image_id,
                PIPELINE_STAGE_TOP_OF_PIPE,
                PIPELINE_STAGE_TRANSFER,
                0,
                ACCESS_TRANSFER_READ,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                QUEUE_FAMILY_EXTERNAL,
                0,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                conversion_image_id,
                PIPELINE_STAGE_TRANSFER,
                PIPELINE_STAGE_TRANSFER,
                ACCESS_TRANSFER_READ | ACCESS_TRANSFER_WRITE,
                ACCESS_TRANSFER_WRITE,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                QUEUE_FAMILY_IGNORED,
                QUEUE_FAMILY_IGNORED,
            )?;
            self.cmd_buffer_barrier(
                adapter,
                command_buffer_id,
                destination_buffer_id,
                destination_size,
                PIPELINE_STAGE_TOP_OF_PIPE,
                PIPELINE_STAGE_TRANSFER,
                0,
                ACCESS_TRANSFER_WRITE,
                QUEUE_FAMILY_EXTERNAL,
                0,
            )?;
            self.cmd_blit_image(
                adapter,
                command_buffer_id,
                source_image_id,
                conversion_image_id,
                width,
                height,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                conversion_image_id,
                PIPELINE_STAGE_TRANSFER,
                PIPELINE_STAGE_TRANSFER,
                ACCESS_TRANSFER_WRITE,
                ACCESS_TRANSFER_READ,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                QUEUE_FAMILY_IGNORED,
                QUEUE_FAMILY_IGNORED,
            )?;
            self.cmd_copy_image_to_buffer(
                adapter,
                command_buffer_id,
                conversion_image_id,
                destination_buffer_id,
                width,
                height,
                pitch,
                bytes_per_pixel,
            )?;
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                source_image_id,
                PIPELINE_STAGE_TRANSFER,
                PIPELINE_STAGE_BOTTOM_OF_PIPE,
                ACCESS_TRANSFER_READ,
                0,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                0,
                QUEUE_FAMILY_EXTERNAL,
            )?;
            self.cmd_buffer_barrier(
                adapter,
                command_buffer_id,
                destination_buffer_id,
                destination_size,
                PIPELINE_STAGE_TRANSFER,
                PIPELINE_STAGE_BOTTOM_OF_PIPE,
                ACCESS_TRANSFER_WRITE,
                0,
                0,
                QUEUE_FAMILY_EXTERNAL,
            )?;
            self.end_command_buffer(adapter, command_buffer_id)
        })();
        if let Err(e) = record_result {
            let _ = self.destroy_command_pool(adapter, command_pool_id);
            return Err(e);
        }
        Ok((command_pool_id, command_buffer_id))
    }

    /// Import an authoritative WDDM primary and record its reusable GPU copy to
    /// the adapter-owned LINEAR scanout image.
    ///
    /// This is a setup operation, not a per-frame operation. The caller should
    /// cache the returned object in the allocation context, submit it with
    /// [`Self::submit_prepared_image_copy`], and destroy it only through
    /// [`Self::destroy_prepared_image_copy`].
    pub fn prepare_optimal_scanout_copy(
        &mut self,
        adapter: &AdapterContext,
        source_resource_id: u32,
        source_allocation_size: u64,
        source_memory_type_index: u32,
        width: u32,
        height: u32,
        source_dxgi_format: u32,
        ddi_bind_flags: u32,
        target_image_id: u64,
    ) -> Result<PreparedImageCopy, VirtioError> {
        if source_resource_id == 0
            || source_allocation_size == 0
            || width == 0
            || height == 0
            || target_image_id == 0
            || source_memory_type_index >= self.memory_type_count
        {
            diag(0x0137);
            return Err(VirtioError::DeviceError);
        }
        let source_pixel_format =
            PresentPixelFormat::from_dxgi(source_dxgi_format).ok_or(VirtioError::DeviceError)?;
        let target_pixel_format = PresentPixelFormat::Bgra8Unorm;

        crate::diag::record_named_bytes(b"CpImpSt", 1);
        ctrl::attach_resource_checked(adapter, self.ctx_id, source_resource_id)?;

        crate::diag::record_named_bytes(b"CpImpSt", 2);
        let source_image_id = match self.create_optimal_present_image_alias(
            adapter,
            width,
            height,
            ddi_bind_flags,
            source_dxgi_format,
            OptimalImageTransport::OpaqueFd,
        ) {
            Ok(id) => id,
            Err(e) => {
                let _ = ctrl::ctx_detach_resource(adapter, self.ctx_id, source_resource_id);
                return Err(e);
            }
        };

        crate::diag::record_named_bytes(b"CpImpSt", 3);
        let (required_size, memory_type_bits) =
            match self.image_memory_requirements(adapter, source_image_id) {
                Ok(req) => req,
                Err(e) => {
                    let _ = self.cleanup_imported_source_alias(
                        adapter,
                        source_resource_id,
                        source_image_id,
                        0,
                    );
                    return Err(e);
                }
            };
        crate::diag::record_named_bytes(b"CpReq", required_size as u32);
        crate::diag::record_named_bytes(b"CpBit", memory_type_bits);
        if required_size > source_allocation_size
            || (memory_type_bits & (1u32 << source_memory_type_index)) == 0
        {
            crate::diag::record_named_bytes(b"CpImpSt", 0xE3);
            let _ =
                self.cleanup_imported_source_alias(adapter, source_resource_id, source_image_id, 0);
            return Err(VirtioError::DeviceError);
        }

        crate::diag::record_named_bytes(b"CpImpSt", 4);
        let source_memory_id = match self.allocate_imported_resource_memory(
            adapter,
            source_resource_id,
            source_allocation_size,
            source_memory_type_index,
        ) {
            Ok(id) => id,
            Err(e) => {
                let _ = self.cleanup_imported_source_alias(
                    adapter,
                    source_resource_id,
                    source_image_id,
                    0,
                );
                return Err(e);
            }
        };

        crate::diag::record_named_bytes(b"CpImpSt", 5);
        if let Err(e) = self.bind_image_memory(adapter, source_image_id, source_memory_id) {
            let _ = self.cleanup_imported_source_alias(
                adapter,
                source_resource_id,
                source_image_id,
                source_memory_id,
            );
            return Err(e);
        }

        crate::diag::record_named_bytes(b"CpImpSt", 6);
        if let Err(e) = self.ensure_linear_copy_target_ready(adapter, target_image_id) {
            let _ = self.cleanup_imported_source_alias(
                adapter,
                source_resource_id,
                source_image_id,
                source_memory_id,
            );
            return Err(e);
        }

        crate::diag::record_named_bytes(b"CpImpSt", 7);
        let mut conversion_image_id = 0;
        let mut conversion_memory_id = 0;
        let mut conversion_init_pool_id = 0;
        let requires_conversion =
            source_pixel_format.vk_format() != target_pixel_format.vk_format();
        let (command_pool_id, command_buffer_id) = if requires_conversion {
            let conversion = match self.create_bound_present_conversion_image(
                adapter,
                width,
                height,
                target_pixel_format,
            ) {
                Ok(conversion) => conversion,
                Err(e) => {
                    let _ = self.cleanup_imported_source_alias(
                        adapter,
                        source_resource_id,
                        source_image_id,
                        source_memory_id,
                    );
                    return Err(e);
                }
            };
            conversion_image_id = conversion.0;
            conversion_memory_id = conversion.1;
            let command = match self.record_reusable_converted_image_copy(
                adapter,
                source_image_id,
                conversion_image_id,
                target_image_id,
                width,
                height,
            ) {
                Ok(command) => command,
                Err(e) => {
                    let _ = self.destroy_image_on_ring(adapter, conversion_image_id);
                    let _ = self.free_memory_object(adapter, conversion_memory_id);
                    let _ = self.cleanup_imported_source_alias(
                        adapter,
                        source_resource_id,
                        source_image_id,
                        source_memory_id,
                    );
                    return Err(e);
                }
            };
            conversion_init_pool_id =
                match self.initialize_present_conversion_image(adapter, conversion_image_id) {
                    Ok(pool_id) => pool_id,
                    Err(e) => {
                        // The initializer's submission result is ambiguous.
                        // Its scratch objects must survive until Venus-context
                        // teardown, but the never-submitted reusable command and
                        // source alias are safe to release.
                        let _ = self.destroy_command_pool(adapter, command.0);
                        let _ = self.cleanup_imported_source_alias(
                            adapter,
                            source_resource_id,
                            source_image_id,
                            source_memory_id,
                        );
                        return Err(e);
                    }
                };
            command
        } else {
            match self.record_reusable_image_copy(
                adapter,
                source_image_id,
                target_image_id,
                width,
                height,
            ) {
                Ok(ids) => ids,
                Err(e) => {
                    let _ = self.cleanup_imported_source_alias(
                        adapter,
                        source_resource_id,
                        source_image_id,
                        source_memory_id,
                    );
                    return Err(e);
                }
            }
        };

        crate::diag::record_named_bytes(b"CpImpSt", 0x10);
        Ok(PreparedImageCopy {
            owns_source_alias: true,
            source_resource_id,
            source_image_id,
            source_memory_id,
            conversion_image_id,
            conversion_memory_id,
            conversion_init_pool_id,
            command_pool_id,
            command_buffer_id,
            target_image_id,
            width,
            height,
        })
    }

    /// Record a reusable copy from an existing KMD-created LINEAR primary.
    ///
    /// Unlike [`Self::prepare_optimal_scanout_copy`], this borrows the source
    /// `VkImage`; the owning allocation keeps its image, memory and virtio
    /// resource alive. Teardown drains the queue and destroys only the recorded
    /// command pool.
    pub fn prepare_existing_linear_source_copy(
        &mut self,
        adapter: &AdapterContext,
        source_image_id: u64,
        width: u32,
        height: u32,
        source_dxgi_format: u32,
        target_image_id: u64,
    ) -> Result<PreparedImageCopy, VirtioError> {
        if source_image_id == 0
            || source_image_id == target_image_id
            || width == 0
            || height == 0
            || target_image_id == 0
        {
            return Err(VirtioError::DeviceError);
        }
        let source_pixel_format =
            PresentPixelFormat::from_dxgi(source_dxgi_format).ok_or(VirtioError::DeviceError)?;
        let target_pixel_format = PresentPixelFormat::Bgra8Unorm;
        self.ensure_linear_copy_target_ready(adapter, target_image_id)?;
        let mut conversion_image_id = 0;
        let mut conversion_memory_id = 0;
        let mut conversion_init_pool_id = 0;
        let (command_pool_id, command_buffer_id) =
            if source_pixel_format.vk_format() != target_pixel_format.vk_format() {
                let conversion = self.create_bound_present_conversion_image(
                    adapter,
                    width,
                    height,
                    target_pixel_format,
                )?;
                conversion_image_id = conversion.0;
                conversion_memory_id = conversion.1;
                let command = match self.record_reusable_converted_image_copy(
                    adapter,
                    source_image_id,
                    conversion_image_id,
                    target_image_id,
                    width,
                    height,
                ) {
                    Ok(command) => command,
                    Err(e) => {
                        let _ = self.destroy_image_on_ring(adapter, conversion_image_id);
                        let _ = self.free_memory_object(adapter, conversion_memory_id);
                        return Err(e);
                    }
                };
                conversion_init_pool_id =
                    match self.initialize_present_conversion_image(adapter, conversion_image_id) {
                        Ok(pool_id) => pool_id,
                        Err(e) => {
                            let _ = self.destroy_command_pool(adapter, command.0);
                            return Err(e);
                        }
                    };
                command
            } else {
                self.record_reusable_image_copy(
                    adapter,
                    source_image_id,
                    target_image_id,
                    width,
                    height,
                )?
            };
        Ok(PreparedImageCopy {
            owns_source_alias: false,
            source_resource_id: 0,
            source_image_id,
            source_memory_id: 0,
            conversion_image_id,
            conversion_memory_id,
            conversion_init_pool_id,
            command_pool_id,
            command_buffer_id,
            target_image_id,
            width,
            height,
        })
    }

    /// Nonblocking per-frame enqueue of a pre-recorded primary-to-LINEAR copy.
    /// No fence is created or waited here; `SIMULTANEOUS_USE` makes repeated
    /// submissions legal while older frames are still pending on the same queue.
    pub fn submit_prepared_image_copy(
        &mut self,
        adapter: &AdapterContext,
        copy: &PreparedImageCopy,
        primary_address: u64,
        ticket: crate::adapter::ProgrammingTicket,
    ) -> Result<u64, VirtioError> {
        if copy.command_buffer_id == 0
            || copy.source_image_id == 0
            || copy.target_image_id != self.copy_target_image_id
        {
            return Err(VirtioError::DeviceError);
        }
        // Encode vkQueueSubmit directly into the outer SUBMIT_3D stream. The
        // outer virtio fence uses ring_idx=1, so its completion callback fires
        // only after this Vulkan queue work completes; that callback—not this
        // enqueue—marks scanout dirty and wakes the RESOURCE_FLUSH worker.
        // Keeping VK_COMMAND_GENERATE_REPLY_BIT_EXT clear is essential: there is
        // no reply-shmem transaction on this direct, fire-and-forget path.
        let mut submit = Writer::new();
        submit.header(CMD_QUEUE_SUBMIT, 0);
        submit.u64(self.queue_id);
        submit.u32(1); // submitCount
        submit.u64(1); // pSubmits array_size
        submit.i32(ST_SUBMIT_INFO);
        submit.count(false); // pNext
        submit.u32(0); // waitSemaphoreCount
        submit.count(false); // pWaitSemaphores
        submit.count(false); // pWaitDstStageMask
        submit.u32(1); // commandBufferCount
        submit.u64(1); // pCommandBuffers array_size
        submit.u64(copy.command_buffer_id);
        submit.u32(0); // signalSemaphoreCount
        submit.count(false); // pSignalSemaphores
        submit.u64(0); // fence
        ctrl::submit_venus_async_scanout(
            adapter,
            self.ctx_id,
            submit.as_slice()?,
            primary_address,
            ticket,
        )
    }

    fn encode_command_buffer_submit(&self, command_buffer_id: u64) -> Writer {
        let mut submit = Writer::new();
        submit.header(CMD_QUEUE_SUBMIT, 0);
        submit.u64(self.queue_id);
        submit.u32(1); // submitCount
        submit.u64(1); // pSubmits array_size
        submit.i32(ST_SUBMIT_INFO);
        submit.count(false); // pNext
        submit.u32(0); // waitSemaphoreCount
        submit.count(false); // pWaitSemaphores
        submit.count(false); // pWaitDstStageMask
        submit.u32(1); // commandBufferCount
        submit.u64(1); // pCommandBuffers array_size
        submit.u64(command_buffer_id);
        submit.u32(0); // signalSemaphoreCount
        submit.count(false); // pSignalSemaphores
        submit.u64(0); // fence
        submit
    }

    /// Copy one complete, same-sized Present source into dxgkrnl's exact
    /// destination allocation, converting across the supported D3D11
    /// swapchain-format set when the Vulkan storage formats differ.
    ///
    /// Setup is cached by exact resource descriptors. The steady-state path
    /// only encodes one vkQueueSubmit and performs one nonblocking ring-1
    /// enqueue; no sleep or synchronous ctrl round-trip occurs per frame.
    pub fn submit_present_blt(
        &mut self,
        adapter: &AdapterContext,
        source: OptimalPresentImageDesc,
        destination: PresentDestinationDesc,
    ) -> Result<u64, VirtioError> {
        if source.resource_id == destination.resource_id()
            || source.width != destination.width()
            || source.height != destination.height()
        {
            return Err(VirtioError::DeviceError);
        }
        let source_pixel_format = source.pixel_format();
        let destination_pixel_format = match destination {
            PresentDestinationDesc::StandardBuffer(desc) => desc.pixel_format(),
            PresentDestinationDesc::OptimalImage(desc) => desc.pixel_format(),
        };
        let requires_conversion =
            source_pixel_format.vk_format() != destination_pixel_format.vk_format();

        // A resource has one immutable interpretation for the cache lifetime.
        // Reject role changes explicitly instead of allowing the same backing
        // memory to be attached once as an OPTIMAL image and again as a buffer.
        let source_was_buffer = self
            .present_buffers
            .iter()
            .any(|buffer| buffer.desc.resource_id == source.resource_id);
        let destination_has_conflicting_role = match destination {
            PresentDestinationDesc::StandardBuffer(desc) => self
                .present_images
                .iter()
                .any(|image| image.desc.resource_id == desc.resource_id),
            PresentDestinationDesc::OptimalImage(desc) => self
                .present_buffers
                .iter()
                .any(|buffer| buffer.desc.resource_id == desc.resource_id),
        };
        if source_was_buffer || destination_has_conflicting_role {
            return Err(VirtioError::DeviceError);
        }

        let existing = self.present_blits.iter().position(|blt| {
            blt.source_resource_id == source.resource_id
                && blt.destination_resource_id == destination.resource_id()
        });
        if existing.is_none() && self.present_blits.len() >= MAX_PRESENT_BLITS {
            return Err(VirtioError::OutOfMemory);
        }
        // Validate the complete descriptors on every call, including cache
        // hits. A recycled/mutated resource id can therefore never select a
        // command buffer recorded for a different allocation contract.
        let source_image = self.ensure_present_image(adapter, source)?;
        let (destination_buffer, destination_image) = match destination {
            PresentDestinationDesc::StandardBuffer(desc) => {
                (Some(self.ensure_present_buffer(adapter, desc)?), None)
            }
            PresentDestinationDesc::OptimalImage(desc) => {
                (None, Some(self.ensure_present_image(adapter, desc)?))
            }
        };
        let blt_index = match existing {
            Some(index) => index,
            None => {
                let mut conversion_image_id = 0;
                let mut conversion_memory_id = 0;
                let mut conversion_init_pool_id = 0;
                let command_result = match destination {
                    PresentDestinationDesc::StandardBuffer(desc) => {
                        let destination_buffer =
                            destination_buffer.ok_or(VirtioError::DeviceError)?;
                        if requires_conversion {
                            let conversion = self.create_bound_present_conversion_image(
                                adapter,
                                source.width,
                                source.height,
                                destination_pixel_format,
                            )?;
                            conversion_image_id = conversion.0;
                            conversion_memory_id = conversion.1;
                            let command = match self.record_reusable_converted_present_blt(
                                adapter,
                                source_image.image_id,
                                conversion_image_id,
                                destination_buffer.buffer_id,
                                desc.allocation_size,
                                source.width,
                                source.height,
                                desc.pitch,
                                destination_pixel_format.bytes_per_pixel(),
                            ) {
                                Ok(command) => command,
                                Err(error) => {
                                    // The scratch image has never been submitted,
                                    // so a recording failure is safe to unwind
                                    // completely and cannot leak once per frame.
                                    let _ =
                                        self.destroy_image_on_ring(adapter, conversion_image_id);
                                    let _ = self.free_memory_object(adapter, conversion_memory_id);
                                    return Err(error);
                                }
                            };
                            conversion_init_pool_id = self.initialize_present_conversion_image(
                                adapter,
                                conversion_image_id,
                            )?;
                            Ok(command)
                        } else {
                            self.record_reusable_present_blt(
                                adapter,
                                source_image.image_id,
                                destination_buffer.buffer_id,
                                desc.allocation_size,
                                source.width,
                                source.height,
                                desc.pitch,
                                destination_pixel_format.bytes_per_pixel(),
                            )
                        }
                    }
                    PresentDestinationDesc::OptimalImage(_) => {
                        let destination_image =
                            destination_image.ok_or(VirtioError::DeviceError)?;
                        if requires_conversion {
                            self.record_reusable_image_blit(
                                adapter,
                                source_image.image_id,
                                destination_image.image_id,
                                source.width,
                                source.height,
                            )
                        } else {
                            self.record_reusable_image_copy(
                                adapter,
                                source_image.image_id,
                                destination_image.image_id,
                                source.width,
                                source.height,
                            )
                        }
                    }
                };
                let (command_pool_id, command_buffer_id) = match command_result {
                    Ok(command) => command,
                    Err(error) => {
                        // A submitted conversion initializer may still be in
                        // flight. Retain its objects for context teardown rather
                        // than risking destruction of live Vulkan state.
                        return Err(error);
                    }
                };
                self.present_blits.push(PreparedPresentBlt {
                    source_resource_id: source.resource_id,
                    destination_resource_id: destination.resource_id(),
                    command_pool_id,
                    command_buffer_id,
                    conversion_image_id,
                    conversion_memory_id,
                    conversion_init_pool_id,
                    last_wire_fence_id: 0,
                    submit_count: 0,
                    // Only a standard buffer is CPU-mappable for the diagnostic.
                    probe_done: matches!(destination, PresentDestinationDesc::OptimalImage(_)),
                });
                self.present_blits.len() - 1
            }
        };

        let command_buffer_id = self.present_blits[blt_index].command_buffer_id;
        let submit = self.encode_command_buffer_submit(command_buffer_id);
        let fence_id = ctrl::submit_venus_async_present(adapter, self.ctx_id, submit.as_slice()?)?;
        let run_probe = {
            let blt = &mut self.present_blits[blt_index];
            blt.last_wire_fence_id = fence_id;
            blt.submit_count = blt.submit_count.saturating_add(1);
            if adapter.present_probe()
                && blt.submit_count >= PRESENT_PROBE_AFTER_SUBMITS
                && !blt.probe_done
            {
                // Claim the one-shot before doing any fallible work. Even a
                // failed diagnostic can therefore never turn into a recurring
                // Present-path stall.
                blt.probe_done = true;
                true
            } else {
                false
            }
        };
        if run_probe {
            if let PresentDestinationDesc::StandardBuffer(destination) = destination {
                // ARM ONLY. The probe itself runs on the PASSIVE display worker,
                // outside this mutex and off the Present path (R320).
                self.probe_pending = Some((destination, fence_id));
                adapter
                    .probe_pending
                    .store(1, core::sync::atomic::Ordering::Release);
            }
        }
        Ok(fence_id)
    }

    /// Take the armed one-shot probe, if any. Called under the venus mutex by
    /// the PASSIVE worker, which then runs the probe outside it.
    pub fn take_pending_probe(&mut self) -> Option<(PresentBufferDesc, u64)> {
        self.probe_pending.take()
    }

    /// One-shot diagnostic proving whether the completed Vulkan copy populated
    /// dxgkrnl's host-visible Present destination. This is deliberately not a
    /// correctness mechanism: all failures are loud breadcrumbs, and the
    /// caller's per-pair `probe_done` bit prevents retry loops.
    ///
    /// PASSIVE_LEVEL, and MUST NOT be called with the venus mutex held.
    pub(crate) fn probe_present_destination(
        adapter: &AdapterContext,
        destination: PresentBufferDesc,
        fence_id: u64,
    ) {
        crate::diag::record_named_bytes(b"PBPrF", 1);
        match ctrl::wait_fence(adapter, fence_id, 5_000_000_000) {
            ctrl::WaitFenceOutcome::Complete => {
                crate::diag::record_named_bytes(b"PBPrF", 2);
            }
            ctrl::WaitFenceOutcome::TimedOut => {
                crate::diag::record_named_bytes(b"PBPrF", 0xE1);
                return;
            }
            ctrl::WaitFenceOutcome::Invalid => {
                crate::diag::record_named_bytes(b"PBPrF", 0xE2);
                return;
            }
        }

        let prep = match ctrl::map_blob_prepare(
            adapter,
            super::gpu::OwnerFilter::Any,
            destination.resource_id,
        ) {
            Ok(prep) => prep,
            Err(_) => {
                crate::diag::record_named_bytes(b"PBPrF", 0xE3);
                return;
            }
        };
        if prep.size < destination.allocation_size {
            crate::diag::record_named_bytes(b"PBPrF", 0xE4);
            return;
        }
        let map = match KernelMap::new(prep.gpa, prep.size, prep.map_cache) {
            Some(map) => map,
            None => {
                crate::diag::record_named_bytes(b"PBPrF", 0xE5);
                return;
            }
        };
        crate::diag::record_named_bytes(b"PBPrF", 3);

        const GRID: u64 = 8;
        let bytes_per_pixel = u64::from(destination.pixel_format().bytes_per_pixel());
        let mut nonblack = 0u32;
        let mut rgb_sum = 0u32;
        for gy in 0..GRID {
            let y = u64::from(destination.height - 1) * gy / (GRID - 1);
            for gx in 0..GRID {
                let x = u64::from(destination.width - 1) * gx / (GRID - 1);
                let offset = y * u64::from(destination.pitch) + x * bytes_per_pixel;
                // PresentBufferDesc::new proves the complete pitched image is
                // within allocation_size, and prep.size covers that allocation.
                let b = u32::from(map.read_byte(offset));
                let g = u32::from(map.read_byte(offset + 1));
                let r = u32::from(map.read_byte(offset + 2));
                let pixel_sum = r + g + b;
                rgb_sum = rgb_sum.saturating_add(pixel_sum);
                nonblack += u32::from(pixel_sum != 0);
            }
        }
        let center_offset = u64::from(destination.height / 2) * u64::from(destination.pitch)
            + u64::from(destination.width / 2) * bytes_per_pixel;
        let center = u32::from(map.read_byte(center_offset))
            | (u32::from(map.read_byte(center_offset + 1)) << 8)
            | (u32::from(map.read_byte(center_offset + 2)) << 16)
            | (u32::from(map.read_byte(center_offset + 3)) << 24);
        crate::diag::record_named_bytes(b"PBPrNz", nonblack);
        crate::diag::record_named_bytes(b"PBPrSum", rgb_sum);
        crate::diag::record_named_bytes(b"PBPrCtr", center);
        crate::diag::record_named_bytes(b"PBPrF", 0x10);
    }

    /// Drain and destroy every cached app/DWM Present BLT import.
    ///
    /// Allocation teardown calls this before a backing resource can disappear.
    /// Flushing the bounded cache as one ownership unit avoids detach/refcount
    /// ambiguity when multiple swapchain images share one destination.
    pub fn release_present_blits_for_resource(
        &mut self,
        adapter: &AdapterContext,
        resource_id: u32,
    ) -> Result<(), VirtioError> {
        if !self
            .present_images
            .iter()
            .any(|image| image.desc.resource_id == resource_id)
            && !self
                .present_buffers
                .iter()
                .any(|buffer| buffer.desc.resource_id == resource_id)
        {
            return Ok(());
        }
        if self.present_blits.is_empty()
            && self.present_images.is_empty()
            && self.present_buffers.is_empty()
        {
            return Ok(());
        }

        // Validate completion of every outer ring-1 fence before destroying any
        // baked command object. On failure, retain the complete cache for Venus
        // context teardown rather than partially freeing live objects.
        for blt in &self.present_blits {
            if blt.last_wire_fence_id != 0 {
                match ctrl::wait_fence(adapter, blt.last_wire_fence_id, 5_000_000_000) {
                    ctrl::WaitFenceOutcome::Complete => {}
                    ctrl::WaitFenceOutcome::TimedOut | ctrl::WaitFenceOutcome::Invalid => {
                        return Err(VirtioError::DeviceError);
                    }
                }
            }
        }

        // One queue marker orders all Vulkan work before object destruction.
        let marker = self.create_fence(adapter)?;
        if let Err(e) = self.queue_submit_fence_marker(adapter, marker) {
            return Err(e);
        }
        if let Err(e) = self.wait_for_fence(adapter, marker) {
            return Err(e);
        }
        self.destroy_fence(adapter, marker)?;

        while let Some(blt) = self.present_blits.pop() {
            self.destroy_command_pool(adapter, blt.command_pool_id)?;
            if blt.conversion_init_pool_id != 0 {
                self.destroy_command_pool(adapter, blt.conversion_init_pool_id)?;
            }
            if blt.conversion_image_id != 0 {
                self.destroy_image_on_ring(adapter, blt.conversion_image_id)?;
            }
            if blt.conversion_memory_id != 0 {
                self.free_memory_object(adapter, blt.conversion_memory_id)?;
            }
        }
        while let Some(image) = self.present_images.pop() {
            self.destroy_image_on_ring(adapter, image.image_id)?;
            self.free_memory_blob(adapter, image.memory_id)?;
            // Each resource occurs exactly once in present_images.
            ctrl::ctx_detach_resource(adapter, self.ctx_id, image.desc.resource_id)?;
        }
        while let Some(buffer) = self.present_buffers.pop() {
            self.destroy_buffer_on_ring(adapter, buffer.buffer_id)?;
            // `BorrowedPresentBuffer` carries no ownership capability. The
            // allocation teardown which called us remains solely responsible
            // for freeing `buffer.memory` and unref'ing its resource.
        }
        Ok(())
    }

    /// Drain the ordered copy queue and release every object retained for one
    /// allocation. This may block (up to the existing 5-second fence timeout),
    /// so it belongs only in PASSIVE allocation teardown, never the frame path.
    pub fn destroy_prepared_image_copy(
        &mut self,
        adapter: &AdapterContext,
        copy: PreparedImageCopy,
        last_wire_fence_id: u64,
    ) -> Result<(), VirtioError> {
        // The reusable command buffer is submitted through an outer async
        // SUBMIT_3D. Drain its latest GPU-completion fence before enqueuing the
        // inner Vulkan marker; otherwise the marker could be decoded first on a
        // different Venus dispatch path and let us destroy a still-referenced
        // command pool.
        if last_wire_fence_id != 0 {
            match ctrl::wait_fence(adapter, last_wire_fence_id, 5_000_000_000) {
                ctrl::WaitFenceOutcome::Complete => {}
                ctrl::WaitFenceOutcome::TimedOut | ctrl::WaitFenceOutcome::Invalid => {
                    return Err(VirtioError::DeviceError);
                }
            }
        }
        let fence_id = self.create_fence(adapter)?;
        if let Err(e) = self.queue_submit_fence_marker(adapter, fence_id) {
            // Submission outcome is ambiguous. Keep all referenced objects live;
            // context teardown will reclaim them without a use-after-free.
            return Err(e);
        }
        if let Err(e) = self.wait_for_fence(adapter, fence_id) {
            return Err(e);
        }
        self.destroy_fence(adapter, fence_id)?;
        self.destroy_command_pool(adapter, copy.command_pool_id)?;
        if copy.conversion_init_pool_id != 0 {
            self.destroy_command_pool(adapter, copy.conversion_init_pool_id)?;
        }
        if copy.conversion_image_id != 0 {
            self.destroy_image_on_ring(adapter, copy.conversion_image_id)?;
        }
        if copy.conversion_memory_id != 0 {
            self.free_memory_object(adapter, copy.conversion_memory_id)?;
        }
        if copy.owns_source_alias {
            self.cleanup_imported_source_alias(
                adapter,
                copy.source_resource_id,
                copy.source_image_id,
                copy.source_memory_id,
            )
        } else {
            Ok(())
        }
    }

    /// Diagnostic-only GPU fill for scanout images. The image is KMD-owned and
    /// never enters the production present path; this proves whether QEMU's GL
    /// console consumes pixels written by the host GPU rather than by the guest CPU.
    pub fn gpu_clear_scanout_image(
        &mut self,
        adapter: &AdapterContext,
        image_id: u64,
    ) -> Result<(), VirtioError> {
        crate::diag::record_named_bytes(b"SdgGpS", 1);
        let pool_id = self.create_command_pool(adapter)?;
        crate::diag::record_named_bytes(b"SdgGpP", 1);
        let command_buffer_id = match self.allocate_command_buffer(adapter, pool_id) {
            Ok(command_buffer_id) => {
                crate::diag::record_named_bytes(b"SdgGpB", 1);
                command_buffer_id
            }
            Err(e) => {
                let _ = self.destroy_command_pool(adapter, pool_id);
                return Err(e);
            }
        };
        let result = (|| {
            self.begin_command_buffer(adapter, command_buffer_id)?;
            crate::diag::record_named_bytes(b"SdgGpBeg", 1);
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                image_id,
                PIPELINE_STAGE_TOP_OF_PIPE,
                PIPELINE_STAGE_TRANSFER,
                0,
                ACCESS_TRANSFER_WRITE,
                IMAGE_LAYOUT_UNDEFINED,
                IMAGE_LAYOUT_GENERAL,
                QUEUE_FAMILY_IGNORED,
                QUEUE_FAMILY_IGNORED,
            )?;
            crate::diag::record_named_bytes(b"SdgGpBa", 1);
            self.cmd_clear_color_image(adapter, command_buffer_id, image_id)?;
            crate::diag::record_named_bytes(b"SdgGpClr", 1);
            self.cmd_image_barrier(
                adapter,
                command_buffer_id,
                image_id,
                PIPELINE_STAGE_TRANSFER,
                PIPELINE_STAGE_BOTTOM_OF_PIPE,
                ACCESS_TRANSFER_WRITE,
                0,
                IMAGE_LAYOUT_GENERAL,
                IMAGE_LAYOUT_GENERAL,
                0,
                QUEUE_FAMILY_FOREIGN_EXT,
            )?;
            crate::diag::record_named_bytes(b"SdgGpBb", 1);
            self.end_command_buffer(adapter, command_buffer_id)?;
            crate::diag::record_named_bytes(b"SdgGpEnd", 1);
            let fence_id = if crate::diag::read_config_dword(b"ScanoutDiag", 0) >= 6 {
                let fence_id = self.create_fence(adapter)?;
                crate::diag::record_named_bytes(b"SdgGpF", 1);
                fence_id
            } else {
                0
            };
            self.queue_submit_command_buffer(adapter, command_buffer_id, fence_id)?;
            crate::diag::record_named_bytes(b"SdgGpSub", 1);
            if fence_id != 0 {
                self.wait_for_fence(adapter, fence_id)?;
                crate::diag::record_named_bytes(b"SdgGpW", 1);
            }
            Ok(())
        })();
        result?;
        // Keep the command pool/buffer alive. This diagnostic submits a GPU
        // ownership release to the host scanout path but has no nonblocking
        // completion primitive in KMD yet; destroying immediately can race the
        // queue work and vkQueueWaitIdle poisons the venus decoder on NVIDIA.
        crate::diag::record_named_bytes(b"SdgGpDst", 2);
        Ok(())
    }

    /// Diagnostic-only scanout allocation: DRM_FORMAT_MODIFIER(LINEAR) BGRA image
    /// backed by dedicated, DMA-BUF-exportable memory, exposed as a HOST3D blob.
    pub fn allocate_scanout_image_blob(
        &mut self,
        adapter: &AdapterContext,
        width: u32,
        height: u32,
    ) -> Result<ScanoutImageBlob, VirtioError> {
        let image_id = self.create_scanout_image(adapter, width, height)?;
        let (req_size, memory_type_bits) = self.image_memory_requirements(adapter, image_id)?;
        let diag_mode = crate::diag::read_config_dword(b"ScanoutDiag", 0);
        let cpu_filled_cross_device_blob = diag_mode == 9 || diag_mode == 11;
        let prefer_device_local = diag_mode >= 5 && !cpu_filled_cross_device_blob;
        let memory_type_index = if prefer_device_local {
            self.choose_device_local_memory_type(memory_type_bits)
        } else {
            self.choose_host_visible_memory_type(memory_type_bits)
        }
        .ok_or(VirtioError::DeviceError)?;
        crate::diag::record_named_bytes(b"SdgMt", memory_type_index);
        crate::diag::record_named_bytes(
            b"SdgMf",
            self.memory_type_flags[memory_type_index as usize],
        );
        let alloc_size = round_up_page(req_size.max(4096));
        let memory_id =
            self.allocate_dedicated_image_memory(adapter, image_id, alloc_size, memory_type_index)?;
        self.bind_image_memory(adapter, image_id, memory_id)?;
        let (offset, row_pitch) =
            self.image_subresource_layout(adapter, image_id, IMAGE_ASPECT_MEMORY_PLANE_0)?;
        if row_pitch == 0 || row_pitch > u32::MAX as u64 || offset > u32::MAX as u64 {
            diag(0x010C);
            return Err(VirtioError::DeviceError);
        }

        let mut blob_flags = VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE;
        if !prefer_device_local {
            blob_flags |= VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE;
        }
        if diag_mode >= 8 {
            blob_flags |= VIRTIO_GPU_BLOB_FLAG_USE_CROSS_DEVICE;
        }
        crate::diag::record_named_bytes(b"SdgBFl", blob_flags);
        let res_id = ctrl::resource_create_blob(
            adapter,
            self.ctx_id,
            VIRTIO_GPU_BLOB_MEM_HOST3D,
            blob_flags,
            memory_id,
            alloc_size,
        )?;
        if (blob_flags & VIRTIO_GPU_BLOB_FLAG_USE_CROSS_DEVICE) != 0 && diag_mode == 10 {
            ctrl::resource_assign_uuid(adapter, res_id)?;
            crate::diag::record_named_bytes(b"SdgUuid", 1);
        } else if (blob_flags & VIRTIO_GPU_BLOB_FLAG_USE_CROSS_DEVICE) != 0 {
            crate::diag::record_named_bytes(b"SdgUuid", 2);
        } else {
            crate::diag::record_named_bytes(b"SdgUuid", 0);
        }
        let _ = adapter.with_virtio(|v| v.note_blob_size(res_id, alloc_size));
        Ok(ScanoutImageBlob {
            blob: HostVisibleBlob {
                blob_id: memory_id,
                res_id,
                gpa: 0,
                size: alloc_size,
            },
            image_id,
            memory_type_index,
            row_pitch: row_pitch as u32,
            plane_offset: offset as u32,
        })
    }

    /// Allocate a KMD-owned GDI texture with the storage contract Windows
    /// requested for `D3DKMDT_GDISURFACE_TEXTURE`: an OPTIMAL BGRA image,
    /// device-local dedicated memory, and DMA_BUF/CROSS_DEVICE export.
    ///
    /// The returned resource is attachable by DWM's renderer-server context.
    /// It is deliberately not mappable and has no row pitch; CPU-visible GDI
    /// surface variants continue to use the separate pitched-buffer path.
    pub fn allocate_optimal_gdi_image_blob(
        &mut self,
        adapter: &AdapterContext,
        width: u32,
        height: u32,
        ddi_bind_flags: u32,
        dxgi_format: u32,
    ) -> Result<OptimalImageBlob, VirtioError> {
        if width == 0 || height == 0 || !matches!(dxgi_format, 87 | 88) {
            return Err(VirtioError::DeviceError);
        }

        let image_id = self.create_optimal_present_image_alias(
            adapter,
            width,
            height,
            ddi_bind_flags,
            dxgi_format,
            OptimalImageTransport::CrossContextDmaBuf,
        )?;
        let (required_size, memory_type_bits) =
            match self.image_memory_requirements(adapter, image_id) {
                Ok(requirements) => requirements,
                Err(error) => {
                    let _ = self.destroy_image(adapter, image_id);
                    return Err(error);
                }
            };
        let memory_type_index = match self.choose_device_local_memory_type(memory_type_bits) {
            Some(index) => index,
            None => {
                let _ = self.destroy_image(adapter, image_id);
                return Err(VirtioError::DeviceError);
            }
        };
        let allocation_size = round_up_page(required_size.max(4096));
        let memory_id = match self.allocate_dedicated_image_memory(
            adapter,
            image_id,
            allocation_size,
            memory_type_index,
        ) {
            Ok(id) => id,
            Err(error) => {
                let _ = self.destroy_image(adapter, image_id);
                return Err(error);
            }
        };
        if let Err(error) = self.bind_image_memory(adapter, image_id, memory_id) {
            let _ = self.destroy_image(adapter, image_id);
            let _ = self.free_memory_blob(adapter, memory_id);
            return Err(error);
        }

        let blob_flags = VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE | VIRTIO_GPU_BLOB_FLAG_USE_CROSS_DEVICE;
        let resource_id = match ctrl::resource_create_blob(
            adapter,
            self.ctx_id,
            VIRTIO_GPU_BLOB_MEM_HOST3D,
            blob_flags,
            memory_id,
            allocation_size,
        ) {
            Ok(id) => id,
            Err(error) => {
                let _ = self.destroy_image(adapter, image_id);
                let _ = self.free_memory_blob(adapter, memory_id);
                return Err(error);
            }
        };
        let _ = adapter.with_virtio(|v| v.note_blob_size(resource_id, allocation_size));

        Ok(OptimalImageBlob {
            blob: HostVisibleBlob {
                blob_id: memory_id,
                res_id: resource_id,
                gpa: 0,
                size: allocation_size,
            },
            image_id,
            memory_type_index,
        })
    }

    /// Diagnostic scanout allocation matching the working Linux probe: a plain
    /// LINEAR external DMA_BUF image, host-visible memory, and a HOST3D
    /// MAPPABLE|SHAREABLE blob referencing that memory.
    pub fn allocate_linear_scanout_image_blob(
        &mut self,
        adapter: &AdapterContext,
        width: u32,
        height: u32,
    ) -> Result<ScanoutImageBlob, VirtioError> {
        // Stage breadcrumb: `SdgLStg` holds the stage last ENTERED. On an early
        // `?` return it names the exact Venus call that rejected the CachyOS
        // shared-primary shape (mode 16 / real primary), turning the opaque
        // `SdgErr=2` into a precise failing stage. Companion values: `SdgLReq`
        // (mem-req size), `SdgLBit` (memoryTypeBits), `SdgLTyc` (type count),
        // `SdgLImg`/`SdgLMem` (raw VkResults), `SdgLPch`/`SdgLOff` (layout).
        //   1=create image  2=mem-req  3=choose host-visible type
        //   4=alloc export mem  5=bind  6=subresource layout
        //   7=validate pitch/offset  8=create blob  0x10=done
        crate::diag::record_named_bytes(b"SdgLStg", 1);
        let image_id = self.create_linear_scanout_image(adapter, width, height)?;

        crate::diag::record_named_bytes(b"SdgLStg", 2);
        let (req_size, memory_type_bits) = self.image_memory_requirements(adapter, image_id)?;
        crate::diag::record_named_bytes(b"SdgLReq", req_size as u32);
        crate::diag::record_named_bytes(b"SdgLBit", memory_type_bits);
        crate::diag::record_named_bytes(b"SdgLTyc", self.memory_type_count);

        crate::diag::record_named_bytes(b"SdgLStg", 3);
        let memory_type_index = self
            .choose_host_visible_memory_type(memory_type_bits)
            .ok_or(VirtioError::DeviceError)?;
        crate::diag::record_named_bytes(b"SdgMt", memory_type_index);
        crate::diag::record_named_bytes(
            b"SdgMf",
            self.memory_type_flags[memory_type_index as usize],
        );
        let alloc_size = round_up_page(req_size.max(4096));

        crate::diag::record_named_bytes(b"SdgLStg", 4);
        let memory_id =
            self.allocate_export_image_memory(adapter, alloc_size, memory_type_index)?;

        crate::diag::record_named_bytes(b"SdgLStg", 5);
        self.bind_image_memory(adapter, image_id, memory_id)?;

        crate::diag::record_named_bytes(b"SdgLStg", 6);
        let (offset, row_pitch) =
            self.image_subresource_layout(adapter, image_id, IMAGE_ASPECT_COLOR)?;
        crate::diag::record_named_bytes(b"SdgLPch", row_pitch as u32);
        crate::diag::record_named_bytes(b"SdgLOff", offset as u32);

        crate::diag::record_named_bytes(b"SdgLStg", 7);
        if row_pitch == 0 || row_pitch > u32::MAX as u64 || offset > u32::MAX as u64 {
            diag(0x0125);
            return Err(VirtioError::DeviceError);
        }

        let blob_flags = VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE | VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE;
        crate::diag::record_named_bytes(b"SdgBFl", blob_flags);
        crate::diag::record_named_bytes(b"SdgLStg", 8);
        let res_id = ctrl::resource_create_blob(
            adapter,
            self.ctx_id,
            VIRTIO_GPU_BLOB_MEM_HOST3D,
            blob_flags,
            memory_id,
            alloc_size,
        )?;
        let _ = adapter.with_virtio(|v| v.note_blob_size(res_id, alloc_size));
        crate::diag::record_named_bytes(b"SdgLStg", 0x10);
        Ok(ScanoutImageBlob {
            blob: HostVisibleBlob {
                blob_id: memory_id,
                res_id,
                gpa: 0,
                size: alloc_size,
            },
            image_id,
            memory_type_index,
            row_pitch: row_pitch as u32,
            plane_offset: offset as u32,
        })
    }

    /// Destroy a Venus `VkImage` allocated by the kernel Venus client. Best
    /// effort: allocation teardown must not wedge if the host context is already
    /// being destroyed.
    pub fn destroy_image(
        &mut self,
        adapter: &AdapterContext,
        image_id: u64,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_DESTROY_IMAGE, 0);
        w.u64(self.device_id);
        w.u64(image_id);
        w.count(false);
        self.submit_direct(adapter, w.as_slice()?)
    }

    /// Enqueue a raw Venus `vkFreeMemory` command.
    fn free_memory_object(
        &mut self,
        adapter: &AdapterContext,
        memory_id: u64,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_FREE_MEMORY, 0);
        w.u64(self.device_id);
        w.u64(memory_id);
        w.count(false); // pAllocator = NULL
        self.ring_command_noreply(adapter, w.as_slice()?)
    }

    /// Free a Venus `VkDeviceMemory`, removing its local blob identity only
    /// after the free command has been accepted by the ring.
    ///
    /// A memory object borrowed by a cached Present buffer cannot be freed.
    /// Allocation teardown must first drain `release_present_blits_for_resource`;
    /// this check turns that lifetime contract into an enforced invariant.
    pub fn free_memory_blob(
        &mut self,
        adapter: &AdapterContext,
        memory_id: u64,
    ) -> Result<(), VirtioError> {
        if self
            .present_buffers
            .iter()
            .any(|buffer| buffer.memory.memory_id == memory_id)
        {
            crate::diag::record_named_bytes(b"PBFree", 0xE1);
            return Err(VirtioError::DeviceError);
        }
        let owned_index = self
            .owned_memory_blobs
            .iter()
            .position(|blob| blob.memory_id == memory_id);
        self.free_memory_object(adapter, memory_id)?;
        if let Some(index) = owned_index {
            self.owned_memory_blobs.swap_remove(index);
        }
        Ok(())
    }
}

// DIVERGES: non-saturating, see T4a. The two other copies of this function moved
// to `helios_kmd_logic::round_up_page`, which saturates; this one wraps to 0 for a
// `size` within 4095 of `u64::MAX`. Callers (`:1102`, `:4288`, `:4380`, `:4465`)
// all pass a host-reported memory requirement, so unifying it would be a real
// behaviour change to a Venus allocation size and needs its own before/after
// evidence — deliberately not folded into the R101 move.
fn round_up_page(size: u64) -> u64 {
    (size + 4095) & !4095
}

/// Run the entire venus bring-up and self-allocate a 16-MiB HOST_VISIBLE|
/// HOST_COHERENT `VkDeviceMemory`, exposed as a BAR-backed, CPU-coherent region.
///
/// `ctx_id` MUST be a live venus (`VIRTIO_GPU_CAPSET_VENUS`) context the caller
/// created and keeps alive for the device lifetime. On success returns the
/// [`VenusClient`] (kept alive by the caller so the ring/reply mappings persist)
/// and the [`HostVisibleBlob`] describing the page-table region.
///
/// Runs at PASSIVE_LEVEL during StartDevice, after `set_virtio` installs the
/// transport (all round-trips ride `virtio::ctrl`'s PASSIVE waits).
pub fn allocate_host_visible_blob(
    adapter: &AdapterContext,
    ctx_id: u32,
) -> Result<(VenusClient, HostVisibleBlob), VirtioError> {
    diag(0x0001);

    // ── 1. Ring shmem: create blob + map into window + kernel-map + zero ──────
    let ring_res_id = ctrl::resource_create_blob(
        adapter,
        ctx_id,
        VIRTIO_GPU_BLOB_MEM_HOST3D,
        VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE,
        0, // blob_id 0: ring shmem is host-allocated (no venus mem binding)
        RING_SHMEM_SIZE,
    )?;
    // Track the ring blob (owner 0) so the map below can size the mapping.
    let _ = adapter.with_virtio(|v| v.note_blob_size(ring_res_id, RING_SHMEM_SIZE));
    let ring_prep = ctrl::map_blob_prepare(adapter, super::gpu::OwnerFilter::Exactly(None), ring_res_id)?;
    let ring_map = KernelMap::new(ring_prep.gpa, ring_prep.size, ring_prep.map_cache)
        .ok_or(VirtioError::MmioMapFailed)?;
    ring_map.zero();
    diag(0x0002);

    // ── 2. Reply shmem: create blob + map + kernel-map + zero ─────────────────
    let reply_res_id = ctrl::resource_create_blob(
        adapter,
        ctx_id,
        VIRTIO_GPU_BLOB_MEM_HOST3D,
        VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE,
        0,
        REPLY_SHMEM_SIZE,
    )?;
    let _ = adapter.with_virtio(|v| v.note_blob_size(reply_res_id, REPLY_SHMEM_SIZE));
    let reply_prep = ctrl::map_blob_prepare(adapter, super::gpu::OwnerFilter::Exactly(None), reply_res_id)?;
    let reply_map = KernelMap::new(reply_prep.gpa, reply_prep.size, reply_prep.map_cache)
        .ok_or(VirtioError::MmioMapFailed)?;
    reply_map.zero();
    diag(0x0003);

    let mut client = VenusClient {
        // A distinctive, unique-enough ring token (any 64-bit value works).
        ring_id: 0x4845_4C49_4F53_0001, // "HELIOS\0\x01"
        ring_res_id,
        reply_res_id,
        ring_map,
        reply_map,
        cur: 0,
        notify_seqno: 0,
        roundtrip_seqno: 0,
        next_handle: 1,
        ctx_id,
        instance_id: 0,
        device_id: 0,
        queue_id: 0,
        memory_type_index: 0,
        memory_type_flags: [0; VK_MAX_MEMORY_TYPES as usize],
        memory_type_count: 0,
        fatal: false,
        copy_target_image_id: 0,
        copy_target_init_pool_id: 0,
        copy_target_init_command_buffer_id: 0,
        present_images: Vec::with_capacity(MAX_PRESENT_IMAGES),
        present_buffers: Vec::with_capacity(MAX_PRESENT_BUFFERS),
        present_blits: Vec::with_capacity(MAX_PRESENT_BLITS),
        probe_pending: None,
        owned_memory_blobs: Vec::with_capacity(MAX_OWNED_MEMORY_BLOBS),
    };

    // ── 3. vkCreateRingMESA (direct) — register the ring with the host ────────
    {
        let mut w = Writer::new();
        w.header(CMD_CREATE_RING_MESA, 0);
        w.u64(client.ring_id);
        w.count(true); // simple_pointer(pCreateInfo)
                       // VkRingCreateInfoMESA:
        w.i32(ST_RING_CREATE_INFO_MESA); // sType
        w.u64(0); // pNext (encoded as simple_pointer NULL = u64 0)
        w.u32(0); // flags
        w.u32(ring_res_id); // resourceId
        w.u64(0); // offset
        w.u64(RING_SHMEM_SIZE); // size
        w.u64(RING_IDLE_TIMEOUT_NS); // idleTimeout
        w.u64(RING_HEAD_OFFSET); // headOffset
        w.u64(RING_TAIL_OFFSET); // tailOffset
        w.u64(RING_STATUS_OFFSET); // statusOffset
        w.u64(RING_BUFFER_OFFSET); // bufferOffset
        w.u64(RING_BUFFER_SIZE as u64); // bufferSize
        w.u64(RING_EXTRA_OFFSET); // extraOffset
        w.u64(RING_EXTRA_SIZE); // extraSize
        client.submit_direct(adapter, w.as_slice()?)?;
    }
    diag(0x0004);

    // ── 3b. (no warm-up) ──────────────────────────────────────────────────────
    // The host maps the reply shmem when it processes vkSetReplyCommandStreamMESA
    // on the ring, so no separate roundtrip is needed. The previous warm-up used a
    // DIRECT vkWaitVirtqueueSeqnoMESA, which the host rejects ("must be called on
    // ring dispatch") — removed.
    diag(0x0005);

    // ── 4. vkCreateInstance (ring, reply) ─────────────────────────────────────
    let instance_id = client.alloc_handle();
    client.instance_id = instance_id;
    {
        let mut w = Writer::new();
        w.header(CMD_CREATE_INSTANCE, CMD_FLAG_GENERATE_REPLY);
        w.count(true); // simple_pointer(pCreateInfo)
                       // VkInstanceCreateInfo:
        w.i32(ST_INSTANCE_CREATE_INFO); // sType
        w.u64(0); // pNext NULL
        w.u32(0); // flags
        w.count(false); // simple_pointer(pApplicationInfo) NULL
        w.u32(0); // enabledLayerCount
        w.count(false); // ppEnabledLayerNames array_size 0
        w.u32(0); // enabledExtensionCount
        w.count(false); // ppEnabledExtensionNames array_size 0
        w.count(false); // simple_pointer(pAllocator) NULL
        w.count(true); // simple_pointer(pInstance)
        w.u64(instance_id); // VkInstance handle
        client.ring_command_reply(adapter, w.as_slice()?)?;
    }
    // Reply: [i32 cmd][i32 VkResult][simple_pointer u64][u64 instance]
    {
        let mut r = ReplyReader::new(&client.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_CREATE_INSTANCE {
            diag(0x00E5);
            return Err(VirtioError::DeviceError);
        }
        let result = r.read_i32()?;
        if result != 0 {
            diag(0x00E6);
            return Err(VirtioError::DeviceError);
        }
    }
    diag(0x0006);

    // ── 5. vkEnumeratePhysicalDevices — count, then array (request 1) ─────────
    // Count call first (some hosts require it before the array call).
    {
        let mut w = Writer::new();
        w.header(CMD_ENUMERATE_PHYSICAL_DEVICES, CMD_FLAG_GENERATE_REPLY);
        w.u64(instance_id); // VkInstance
        w.count(true); // simple_pointer(pPhysicalDeviceCount)
        w.u32(0); // *pPhysicalDeviceCount = 0
        w.count(false); // pPhysicalDevices NULL → array_size 0
        client.ring_command_reply(adapter, w.as_slice()?)?;
        // We don't strictly need the count value; just validate the reply header.
        let mut r = ReplyReader::new(&client.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_ENUMERATE_PHYSICAL_DEVICES {
            diag(0x00E7);
            return Err(VirtioError::DeviceError);
        }
    }
    // Array call: request up to 1 physical device. Physical-device handles are
    // GUEST-assigned like all venus handles (the host rejects a 0 placeholder with
    // "invalid object id 0"), so pre-allocate an id for the slot.
    let phys_dev_id = client.alloc_handle();
    {
        let mut w = Writer::new();
        w.header(CMD_ENUMERATE_PHYSICAL_DEVICES, CMD_FLAG_GENERATE_REPLY);
        w.u64(instance_id); // VkInstance
        w.count(true); // simple_pointer(pPhysicalDeviceCount)
        w.u32(1); // *pPhysicalDeviceCount = 1
        w.count(true); // pPhysicalDevices present → array_size 1 follows
        w.u64(phys_dev_id); // guest-assigned VkPhysicalDevice id for slot 0
        client.ring_command_reply(adapter, w.as_slice()?)?;

        // Reply: [i32 cmd][i32 VkResult][sp u64][u32 count][array_size u64][u64 id×N]
        let mut r = ReplyReader::new(&client.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_ENUMERATE_PHYSICAL_DEVICES {
            diag(0x00E8);
            return Err(VirtioError::DeviceError);
        }
        let result = r.read_i32()?;
        // VK_INCOMPLETE (5) is acceptable (more devices than we asked for).
        if result != 0 && result != 5 {
            diag(0x00E9);
            return Err(VirtioError::DeviceError);
        }
        let sp_count = r.read_u64()?; // simple_pointer(pCount)
        if sp_count == 0 {
            diag(0x00EA);
            return Err(VirtioError::DeviceError);
        }
        let count = r.read_u32()?;
        if count == 0 {
            diag(0x00EB);
            return Err(VirtioError::DeviceError);
        }
        let arr = r.read_u64()?; // array_size
        if arr == 0 {
            diag(0x00EC);
            return Err(VirtioError::DeviceError);
        }
        // Slot 0: the host echoes our guest-assigned id; validate it's present but
        // keep using our `phys_dev_id` for subsequent commands.
        let reply_pd = r.read_u64()?;
        if reply_pd == 0 {
            diag(0x00ED);
            return Err(VirtioError::DeviceError);
        }
    }
    diag(0x0007);

    // ── 6. vkGetPhysicalDeviceMemoryProperties — pick a HOST_VISIBLE|COHERENT type ─
    let memory_type_index;
    {
        let mut w = Writer::new();
        w.header(
            CMD_GET_PHYSICAL_DEVICE_MEMORY_PROPERTIES,
            CMD_FLAG_GENERATE_REPLY,
        );
        w.u64(phys_dev_id); // VkPhysicalDevice
        w.count(true); // simple_pointer(pMemoryProperties)
                       // partial-encoded struct: array_size(32) then array_size(16).
        w.u64(VK_MAX_MEMORY_TYPES as u64);
        w.u64(VK_MAX_MEMORY_HEAPS as u64);
        client.ring_command_reply(adapter, w.as_slice()?)?;

        // Reply (NO VkResult): [i32 cmd][sp u64][u32 typeCount][array u64]
        //   [ (u32 propertyFlags, u32 heapIndex) × 32 ]
        //   [u32 heapCount][array u64][ (u64 size, u32 flags) × 16 ]
        let mut r = ReplyReader::new(&client.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_GET_PHYSICAL_DEVICE_MEMORY_PROPERTIES {
            diag(0x00EE);
            return Err(VirtioError::DeviceError);
        }
        let sp = r.read_u64()?;
        if sp == 0 {
            diag(0x00EF);
            return Err(VirtioError::DeviceError);
        }
        let type_count = r.read_u32()?;
        let type_arr = r.read_u32()?; // array_size low 32 (always 32; read full u64)
        let _type_arr_hi = r.read_u32()?;
        // Validate the encoded array length is the fixed VK_MAX_MEMORY_TYPES.
        if type_arr != VK_MAX_MEMORY_TYPES || type_count > VK_MAX_MEMORY_TYPES {
            diag(0x00F0);
            return Err(VirtioError::DeviceError);
        }
        let mut chosen: Option<u32> = None;
        client.memory_type_count = type_count;
        for i in 0..VK_MAX_MEMORY_TYPES {
            let property_flags = r.read_u32()?;
            let _heap_index = r.read_u32()?;
            client.memory_type_flags[i as usize] = property_flags;
            if chosen.is_none()
                && i < type_count
                && property_flags & MEMORY_PROPERTY_HOST_VISIBLE != 0
                && property_flags & MEMORY_PROPERTY_HOST_COHERENT != 0
            {
                chosen = Some(i);
            }
        }
        // Heap array is not needed; leave it unread (reply is one-shot).
        match chosen {
            Some(idx) => memory_type_index = idx,
            None => {
                diag(0x00F1);
                return Err(VirtioError::DeviceError);
            }
        }
    }
    client.memory_type_index = memory_type_index;
    diag(0x0008);

    // ── 7. vkCreateDevice — one queue, family 0, priority 1.0 ─────────────────
    // Device-extension ladder. The proven scanout/export shape
    // (/tmp/vk-dmabuf-scanout.c, the CachyOS NVIDIA egl-headless success) needs
    // only the external-memory + DMA_BUF trio; the DRM-modifier image path
    // (scanout_diag modes 4-11) additionally wants image_drm_format_modifier +
    // image_format_list. Previously this was all-or-nothing: if the host
    // rejected the full 5-ext set, we dropped straight to a ZERO-ext device,
    // which then silently rejects every DMA_BUF export op (the whole scanout
    // path dies with no visible reason, since the S-ring is off at DiagLevel 0).
    // Now we step down full → export-trio → none, so linear scanout (mode 16)
    // still gets its export exts even when the modifier exts are unavailable.
    // The tier that stuck (and any knock-down VkResult) is recorded to fixed
    // registry names so a `reg query` reveals it without the S-ring.
    const EXT_FULL: [&[u8]; 5] = [
        b"VK_KHR_external_memory\0",
        b"VK_KHR_external_memory_fd\0",
        b"VK_KHR_image_format_list\0",
        b"VK_EXT_external_memory_dma_buf\0",
        b"VK_EXT_image_drm_format_modifier\0",
    ];
    const EXT_EXPORT: [&[u8]; 3] = [
        b"VK_KHR_external_memory\0",
        b"VK_KHR_external_memory_fd\0",
        b"VK_EXT_external_memory_dma_buf\0",
    ];
    let scanout_diag = crate::diag::read_config_dword(b"ScanoutDiag", 0);
    // Production DisplayHalf needs only the export trio for its dedicated plain
    // LINEAR DMA_BUF image. The modifier/image-format-list tier remains strictly
    // diagnostic; never enable it merely because real scanout is active.
    let want_scanout_exts = client.ctx_id != 0 && (adapter.display_half() || scanout_diag >= 4);
    // Clear the knock-down VkResult so a clean full-tier success leaves it 0 and
    // a prior boot's value can't be mistaken for this boot's (names persist).
    crate::diag::record_named_bytes(b"SdgDevR", 0);
    // Tier 0 = full, 1 = export-only, 2 = none. Production (scanout off) starts
    // at 2, exactly the old render-only, no-ext behaviour.
    let mut ext_tier: u32 = if scanout_diag >= 4 {
        0
    } else if want_scanout_exts {
        1
    } else {
        2
    };
    loop {
        let exts: &[&[u8]] = match ext_tier {
            0 => &EXT_FULL,
            1 => &EXT_EXPORT,
            _ => &[],
        };
        let device_id = client.alloc_handle();
        client.device_id = device_id;
        let mut w = Writer::new();
        w.header(CMD_CREATE_DEVICE, CMD_FLAG_GENERATE_REPLY);
        w.u64(phys_dev_id); // VkPhysicalDevice
        w.count(true); // simple_pointer(pCreateInfo)
                       // VkDeviceCreateInfo:
        w.i32(ST_DEVICE_CREATE_INFO); // sType
        w.u64(0); // pNext NULL
        w.u32(0); // flags
        w.u32(1); // queueCreateInfoCount
        w.count(true); // array_size(1) for pQueueCreateInfos
                       // VkDeviceQueueCreateInfo[0]:
        w.i32(ST_DEVICE_QUEUE_CREATE_INFO); // sType
        w.u64(0); // pNext NULL
        w.u32(0); // flags
        w.u32(0); // queueFamilyIndex
        w.u32(1); // queueCount
        w.count(true); // array_size(1) for pQueuePriorities
        w.f32(1.0); // priority
                    // back to VkDeviceCreateInfo:
        w.u32(0); // enabledLayerCount
        w.count(false); // ppEnabledLayerNames array_size 0
        if exts.is_empty() {
            w.u32(0); // enabledExtensionCount
            w.count(false); // ppEnabledExtensionNames array_size 0
        } else {
            w.u32(exts.len() as u32);
            w.u64(exts.len() as u64);
            for ext in exts {
                w.u64(ext.len() as u64);
                w.bytes_padded(ext);
            }
        }
        w.count(false); // simple_pointer(pEnabledFeatures) NULL
        w.count(false); // simple_pointer(pAllocator) NULL
        w.count(true); // simple_pointer(pDevice)
        w.u64(device_id); // VkDevice handle
        client.ring_command_reply(adapter, w.as_slice()?)?;

        // Reply: [i32 cmd][i32 VkResult][sp u64][u64 device]
        let mut r = ReplyReader::new(&client.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_CREATE_DEVICE {
            diag(0x00F2);
            return Err(VirtioError::DeviceError);
        }
        let result = r.read_i32()?;
        if result == 0 {
            // Which extension tier the device actually got. mode 16 needs >= 1
            // (export trio); a value of 2 means NO export exts → scanout can't
            // work and SdgLImg/SdgLMem will show the rejection downstream.
            crate::diag::record_named_bytes(b"SdgDevX", ext_tier);
            break;
        }
        // Record the VkResult that knocked this tier down before stepping down.
        crate::diag::record_named_bytes(b"SdgDevR", result as u32);
        if ext_tier < 2 {
            diag(0x00F4);
            ext_tier += 1;
            continue;
        }
        // If this fails, the host may require a VkDeviceQueueTimelineInfoMESA
        // pNext on the queue-create — see the handover notes.
        diag(0x00F3);
        return Err(VirtioError::DeviceError);
    }
    diag(0x0009);

    client.get_device_queue(adapter)?;
    diag(0x000D);

    // ── 8. vkAllocateMemory — 16 MiB of the chosen HOST_VISIBLE|COHERENT type ──
    // The memory handle id we pick IS the virtio-gpu blob_id used below.
    let blob = client.allocate_memory_blob(adapter, PAGE_TABLE_ALLOC_SIZE, true, false)?;
    diag(0x000A);

    // ── 9. Create + map the page-table blob backed by the venus memory id ─────
    let pt_prep = ctrl::map_blob_prepare(adapter, super::gpu::OwnerFilter::Exactly(None), blob.res_id)?;
    diag(0x000B);

    let blob = HostVisibleBlob {
        blob_id: blob.blob_id,
        res_id: blob.res_id,
        gpa: pt_prep.gpa,
        size: pt_prep.size,
    };
    diag(0x000C);
    Ok((client, blob))
}
