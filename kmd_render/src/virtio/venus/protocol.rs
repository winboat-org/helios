//! Venus wire constants: command ids, `VkStructureType` values, the memory,
//! format, usage and layout bit sets, and the ring shared-memory layout.
//!
//! Moved verbatim out of `virtio/venus.rs` by T8/R1104. Adapter-free by
//! construction -- nothing here names `AdapterContext`, `VenusClient` or a
//! mapping. The `Writer` the review also assigns to this module already lives
//! in `kmd_logic` (T0) with seven host tests, so it is not moved here.

// ── venus command type ids (VkCommandTypeEXT) ────────────────────────────────
// Verified against vn_protocol_driver_defines.h.
pub(crate) const CMD_CREATE_INSTANCE: u32 = 0;
pub(crate) const CMD_ENUMERATE_PHYSICAL_DEVICES: u32 = 2;
pub(crate) const CMD_GET_PHYSICAL_DEVICE_MEMORY_PROPERTIES: u32 = 8;
pub(crate) const CMD_CREATE_DEVICE: u32 = 11;
pub(crate) const CMD_QUEUE_SUBMIT: u32 = 18;
pub(crate) const CMD_FREE_MEMORY: u32 = 22;
pub(crate) const CMD_BIND_IMAGE_MEMORY: u32 = 29;
pub(crate) const CMD_BIND_BUFFER_MEMORY: u32 = 28;
pub(crate) const CMD_GET_BUFFER_MEMORY_REQUIREMENTS_2: u32 = 145;
pub(crate) const CMD_GET_IMAGE_MEMORY_REQUIREMENTS: u32 = 31;
pub(crate) const CMD_CREATE_FENCE: u32 = 35;
pub(crate) const CMD_DESTROY_FENCE: u32 = 36;
pub(crate) const CMD_WAIT_FOR_FENCES: u32 = 39;
pub(crate) const CMD_CREATE_BUFFER: u32 = 50;
pub(crate) const CMD_DESTROY_BUFFER: u32 = 51;
pub(crate) const CMD_DESTROY_IMAGE: u32 = 55;
pub(crate) const CMD_GET_IMAGE_SUBRESOURCE_LAYOUT: u32 = 56;
pub(crate) const CMD_CREATE_COMMAND_POOL: u32 = 85;
pub(crate) const CMD_DESTROY_COMMAND_POOL: u32 = 86;
pub(crate) const CMD_ALLOCATE_COMMAND_BUFFERS: u32 = 88;
pub(crate) const CMD_BEGIN_COMMAND_BUFFER: u32 = 90;
pub(crate) const CMD_END_COMMAND_BUFFER: u32 = 91;
pub(crate) const CMD_COPY_IMAGE: u32 = 113;
pub(crate) const CMD_BLIT_IMAGE: u32 = 114;
pub(crate) const CMD_COPY_IMAGE_TO_BUFFER: u32 = 116;
pub(crate) const CMD_PIPELINE_BARRIER: u32 = 126;
pub(crate) const CMD_GET_DEVICE_QUEUE_2: u32 = 155;
pub(crate) const CMD_SET_REPLY_COMMAND_STREAM_MESA: u32 = 178;
pub(crate) const CMD_CREATE_RING_MESA: u32 = 188;
pub(crate) const CMD_NOTIFY_RING_MESA: u32 = 190;

/// `VK_COMMAND_GENERATE_REPLY_BIT_EXT` — set in a command's flags word to request
/// a reply written into the previously-set reply command stream.

// ── Vulkan structure-type ids (VkStructureType) ──────────────────────────────
pub(crate) const ST_INSTANCE_CREATE_INFO: i32 = 1;
pub(crate) const ST_DEVICE_QUEUE_CREATE_INFO: i32 = 2;
pub(crate) const ST_DEVICE_CREATE_INFO: i32 = 3;
pub(crate) const ST_SUBMIT_INFO: i32 = 4;
pub(crate) const ST_BUFFER_CREATE_INFO: i32 = 12;
pub(crate) const ST_EXTERNAL_MEMORY_BUFFER_CREATE_INFO: i32 = 1000072000;
pub(crate) const ST_MEMORY_DEDICATED_REQUIREMENTS: i32 = 1000127000;
pub(crate) const ST_BUFFER_MEMORY_REQUIREMENTS_INFO_2: i32 = 1000146000;
pub(crate) const ST_MEMORY_REQUIREMENTS_2: i32 = 1000146003;
pub(crate) const ST_FENCE_CREATE_INFO: i32 = 8;
pub(crate) const ST_COMMAND_POOL_CREATE_INFO: i32 = 39;
pub(crate) const ST_COMMAND_BUFFER_ALLOCATE_INFO: i32 = 40;
pub(crate) const ST_COMMAND_BUFFER_BEGIN_INFO: i32 = 42;
pub(crate) const ST_BUFFER_MEMORY_BARRIER: i32 = 44;
pub(crate) const ST_IMAGE_MEMORY_BARRIER: i32 = 45;
pub(crate) const ST_DEVICE_QUEUE_INFO_2: i32 = 1000145003;
pub(crate) const ST_RING_CREATE_INFO_MESA: i32 = 1000384000;
pub(crate) const ST_DEVICE_QUEUE_TIMELINE_INFO_MESA: i32 = 1000384005;

/// `VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT`.
///
/// Ordinary Helios/DXVK shared images use the renderer's OPAQUE_FD export path;
/// the KMD alias must carry the same external-image handle type even though the
/// actual memory import is named by `VkImportMemoryResourceInfoMESA`.
pub(crate) const EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD: u32 = 0x0000_0001;
/// `VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT`.
pub(crate) const EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF: u32 = 0x0000_0200;

/// External-memory transport used for an OPTIMAL image and its backing blob.
///
/// Keeping this as one value prevents the image-create, memory-export and
/// import contract from drifting apart. Direct-optimal scanout images use the
/// DMA_BUF variant; ordinary UMD images use OPAQUE_FD.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OptimalImageTransport {
    OpaqueFd,
    CrossContextDmaBuf,
}

impl OptimalImageTransport {
    pub(crate) const fn handle_type(self) -> u32 {
        match self {
            Self::OpaqueFd => EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD,
            Self::CrossContextDmaBuf => EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF,
        }
    }
}
pub(crate) const FORMAT_R8G8B8A8_UNORM: u32 = 37;
pub(crate) const FORMAT_R8G8B8A8_SRGB: u32 = 43;
pub(crate) const FORMAT_B8G8R8A8_UNORM: u32 = 44;
pub(crate) const FORMAT_B8G8R8A8_SRGB: u32 = 50;
pub(crate) const FORMAT_A2B10G10R10_UNORM_PACK32: u32 = 64;
pub(crate) const FORMAT_R16G16B16A16_SFLOAT: u32 = 97;
// IMAGE_TILING_LINEAR / IMAGE_TILING_OPTIMAL moved to `helios_kmd_logic` with
// the encoders that write them (R1002), and the 39th session's evidence moved
// with them. In short: LINEAR was defined as 0 (OPTIMAL), so
// create_linear_scanout_image built a TILED image → device-local-only
// memoryTypeBits (0x3, no host-visible) → choose_host_visible_memory_type failed
// (ScanoutDiag=16 SdgErr=2 / SdgLStg=3). Confirmed against Mesa venus on the same
// NVIDIA host: LINEAR→typebits=0xf (scans out), OPTIMAL→typebits=0x3 (no
// host-visible). There is now a host test asserting the two are 1 and 0.
pub(crate) const IMAGE_USAGE_TRANSFER_SRC: u32 = 0x0000_0001;
pub(crate) const IMAGE_USAGE_TRANSFER_DST: u32 = 0x0000_0002;
pub(crate) const IMAGE_USAGE_SAMPLED: u32 = 0x0000_0004;
pub(crate) const IMAGE_USAGE_STORAGE: u32 = 0x0000_0008;
pub(crate) const IMAGE_USAGE_COLOR_ATTACHMENT: u32 = 0x0000_0010;
pub(crate) const BUFFER_USAGE_TRANSFER_SRC: u32 = 0x0000_0001;
pub(crate) const BUFFER_USAGE_TRANSFER_DST: u32 = 0x0000_0002;
pub(crate) const IMAGE_CREATE_MUTABLE_FORMAT: u32 = 0x0000_0008;
pub(crate) const IMAGE_LAYOUT_UNDEFINED: u32 = 0;
pub(crate) const IMAGE_LAYOUT_GENERAL: u32 = 1;
pub(crate) const IMAGE_LAYOUT_PREINITIALIZED: u32 = 8;
pub(crate) const IMAGE_ASPECT_COLOR: u32 = 0x0000_0001;
pub(crate) const QUEUE_FAMILY_IGNORED: u32 = u32::MAX;
pub(crate) const QUEUE_FAMILY_EXTERNAL: u32 = u32::MAX - 1;
// Kept for the older diagnostic call sites. Its numeric value has always been
// VK_QUEUE_FAMILY_EXTERNAL (`~1U`), despite the historical name.
pub(crate) const COMMAND_BUFFER_LEVEL_PRIMARY: u32 = 0;
pub(crate) const COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT: u32 = 0x0000_0001;
pub(crate) const COMMAND_BUFFER_USAGE_SIMULTANEOUS_USE: u32 = 0x0000_0004;
pub(crate) const PIPELINE_STAGE_TOP_OF_PIPE: u32 = 0x0000_0001;
pub(crate) const PIPELINE_STAGE_TRANSFER: u32 = 0x0000_1000;
pub(crate) const PIPELINE_STAGE_BOTTOM_OF_PIPE: u32 = 0x0000_2000;
pub(crate) const PIPELINE_STAGE_HOST: u32 = 0x0000_4000;
pub(crate) const ACCESS_TRANSFER_WRITE: u32 = 0x0000_1000;
pub(crate) const ACCESS_TRANSFER_READ: u32 = 0x0000_0800;
pub(crate) const ACCESS_HOST_READ: u32 = 0x0000_2000;

// ── VkMemoryPropertyFlags bits we require ────────────────────────────────────
//
// Defined once in `helios_kmd_logic` alongside the two selectors that read them
// (`choose_host_visible_memory_type` / `choose_device_local_memory_type`), which
// live there because they are pure functions of the host's reported flag array
// and so can carry a host test. VK_MAX_MEMORY_TYPES is the fixed array length
// the host encodes in the memory-properties reply
// (`vn_encode_VkPhysicalDeviceMemoryProperties_partial`), and both the reply
// decoder here and the selectors there must agree on it.
pub(crate) use helios_kmd_logic::{
    MEMORY_PROPERTY_HOST_COHERENT, MEMORY_PROPERTY_HOST_VISIBLE, VK_MAX_MEMORY_TYPES,
};
/// VK_MAX_MEMORY_HEAPS — likewise for the heap array.
pub(crate) const VK_MAX_MEMORY_HEAPS: u32 = 16;

// ── Ring layout (vn_ring `struct layout`, 64-byte aligned header fields) ──────
pub(crate) const RING_HEAD_OFFSET: u64 = 0;
pub(crate) const RING_TAIL_OFFSET: u64 = 64;
pub(crate) const RING_STATUS_OFFSET: u64 = 128;
pub(crate) const RING_BUFFER_OFFSET: u64 = 192;
/// 128 KiB — power of two, matching the ICD's default.
pub(crate) const RING_BUFFER_SIZE: u32 = 131072;
pub(crate) const RING_EXTRA_OFFSET: u64 = RING_BUFFER_OFFSET + RING_BUFFER_SIZE as u64; // 131264
pub(crate) const RING_EXTRA_SIZE: u64 = 4;
/// Total ring shmem = 192 + 131072 + 4 = 131268.
pub(crate) const RING_SHMEM_SIZE: u64 =
    RING_BUFFER_OFFSET + RING_BUFFER_SIZE as u64 + RING_EXTRA_SIZE;
/// Idle timeout reported in the ring-create info (ns); cosmetic for our use.
pub(crate) const RING_IDLE_TIMEOUT_NS: u64 = 1_000_000;

/// Ring status bits (`VkRingStatusFlagsMESA`).
pub(crate) const RING_STATUS_FATAL: u32 = 0x2;

/// Reply shmem size — generous for the small replies we read (largest is the
/// memory-properties reply, ~660 bytes).
pub(crate) const REPLY_SHMEM_SIZE: u64 = 4096;

/// Allocation size of the host-visible page-table memory (16 MiB).
pub(crate) const PAGE_TABLE_ALLOC_SIZE: u64 = 16 * 1024 * 1024;

/// Short spin burst before a ring-head wait falls back to PASSIVE 1 ms sleeps
/// (fast replies stay fast; slow ones cost only sleep latency, never a
/// DISPATCH spin).
pub(crate) const RING_SPIN_BURST: u32 = 50_000;
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
pub(crate) const RING_WAIT_TIMEOUT_MS: u64 = 30_000;

// The command-stream writer and its capacity live in `helios_kmd_logic`: they
// are pure byte arithmetic with no adapter, handle or wdk-sys edge, and the size
// claim that justifies the buffer is worth a host test. See
// `writer_ext_full_create_device_is_332_bytes` there for the real number — the
// comment that used to sit here said "~120 bytes" and was wrong by 212.
pub(crate) use helios_kmd_logic::{
    encode_image_create, encode_memory_allocate, ImageCreateSpec, ImagePNext, MemoryAllocateSpec,
    MemoryPNext, MemoryTypeChoice, Writer, CMD_ALLOCATE_MEMORY, CMD_CREATE_IMAGE,
    CMD_FLAG_GENERATE_REPLY, IMAGE_TILING_LINEAR, IMAGE_TILING_OPTIMAL, SHARING_MODE_EXCLUSIVE,
};

/// Why the venus ring was declared unusable. Each arm names a registry counter
/// so a wedge is distinguishable from every other `DeviceError` in a post-mortem
/// `reg query`, at the default `DiagLevel=0`.
pub(crate) enum FatalReason {
    /// The host set `RING_STATUS_FATAL` — it rejected something we encoded.
    /// Records `VnRingFt=1`.
    HostStatusFatal,
    /// The ring head never advanced past our seqno within
    /// [`RING_WAIT_TIMEOUT_MS`]. Records `VnRingWd` = milliseconds waited, so
    /// the dump distinguishes "gave up at the budget" from a short stall.
    HeadWaitTimeout { elapsed_ms: u64 },
}

/// Written into reply word 0 before every reply-generating ring command, so an
/// unanswered reply cannot decode as the previous one. Not a legal
/// `VkCommandTypeEXT`, so it fails every caller's existing command-type check.
pub(crate) const REPLY_POISON: u32 = 0xFFFF_FFFF;

/// Diagnostic breadcrumb base for venus bring-up (0x0D00_00xx).
pub(crate) fn diag(code: u32) {
    crate::diag::record(0x0D00_0000 | (code & 0xFFFF));
}

// ── Guest-assigned Vulkan object handles ──────────────────────────────────────
