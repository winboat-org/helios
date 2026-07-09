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
const CMD_GET_IMAGE_MEMORY_REQUIREMENTS: u32 = 31;
const CMD_CREATE_FENCE: u32 = 35;
const CMD_WAIT_FOR_FENCES: u32 = 39;
const CMD_CREATE_IMAGE: u32 = 54;
const CMD_DESTROY_IMAGE: u32 = 55;
const CMD_GET_IMAGE_SUBRESOURCE_LAYOUT: u32 = 56;
const CMD_CREATE_COMMAND_POOL: u32 = 85;
const CMD_DESTROY_COMMAND_POOL: u32 = 86;
const CMD_ALLOCATE_COMMAND_BUFFERS: u32 = 88;
const CMD_BEGIN_COMMAND_BUFFER: u32 = 90;
const CMD_END_COMMAND_BUFFER: u32 = 91;
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
const ST_FENCE_CREATE_INFO: i32 = 8;
const ST_IMAGE_CREATE_INFO: i32 = 14;
const ST_COMMAND_POOL_CREATE_INFO: i32 = 39;
const ST_COMMAND_BUFFER_ALLOCATE_INFO: i32 = 40;
const ST_COMMAND_BUFFER_BEGIN_INFO: i32 = 42;
const ST_IMAGE_MEMORY_BARRIER: i32 = 45;
const ST_DEVICE_QUEUE_INFO_2: i32 = 1000145003;
const ST_EXTERNAL_MEMORY_IMAGE_CREATE_INFO: i32 = 1000072001;
const ST_EXPORT_MEMORY_ALLOCATE_INFO: i32 = 1000072002;
const ST_MEMORY_DEDICATED_ALLOCATE_INFO: i32 = 1000127001;
const ST_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO: i32 = 1000158003;
const ST_RING_CREATE_INFO_MESA: i32 = 1000384000;
const ST_DEVICE_QUEUE_TIMELINE_INFO_MESA: i32 = 1000384005;

/// `VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT`.
const EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF: u32 = 0x0000_0200;
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const IMAGE_TYPE_2D: u32 = 1;
const FORMAT_B8G8R8A8_UNORM: u32 = 44;
// VK_IMAGE_TILING_OPTIMAL = 0, VK_IMAGE_TILING_LINEAR = 1. This was 0 (OPTIMAL),
// so create_linear_scanout_image built a TILED image → device-local-only
// memoryTypeBits (0x3, no host-visible) → choose_host_visible_memory_type failed
// (ScanoutDiag=16 SdgErr=2 / SdgLStg=3). Confirmed against Mesa venus on the same
// NVIDIA host: LINEAR→typebits=0xf (scans out), OPTIMAL→typebits=0x3 (no host-visible).
const IMAGE_TILING_LINEAR: u32 = 1;
const IMAGE_TILING_DRM_FORMAT_MODIFIER: u32 = 1000158000;
const IMAGE_USAGE_TRANSFER_SRC: u32 = 0x0000_0001;
const IMAGE_USAGE_TRANSFER_DST: u32 = 0x0000_0002;
const SHARING_MODE_EXCLUSIVE: u32 = 0;
const IMAGE_LAYOUT_UNDEFINED: u32 = 0;
const IMAGE_LAYOUT_GENERAL: u32 = 1;
const IMAGE_LAYOUT_PREINITIALIZED: u32 = 8;
const SAMPLE_COUNT_1: u32 = 0x0000_0001;
const IMAGE_ASPECT_COLOR: u32 = 0x0000_0001;
const IMAGE_ASPECT_MEMORY_PLANE_0: u32 = 0x0000_0080;
const QUEUE_FAMILY_IGNORED: u32 = u32::MAX;
const QUEUE_FAMILY_FOREIGN_EXT: u32 = u32::MAX - 1;
const COMMAND_BUFFER_LEVEL_PRIMARY: u32 = 0;
const COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT: u32 = 0x0000_0001;
const PIPELINE_STAGE_TOP_OF_PIPE: u32 = 0x0000_0001;
const PIPELINE_STAGE_TRANSFER: u32 = 0x0000_1000;
const PIPELINE_STAGE_BOTTOM_OF_PIPE: u32 = 0x0000_2000;
const ACCESS_TRANSFER_WRITE: u32 = 0x0000_1000;

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
/// genuinely wedged → the client latches `fatal`.
const RING_WAIT_TIMEOUT_MS: u64 = 30_000;

/// Maximum venus stream we build for any single direct/ring command. The largest
/// is `vkCreateDevice` (~120 bytes); 512 is comfortable headroom.
const MAX_CMD_BYTES: usize = 512;

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

// ── A tiny little-endian byte writer for building venus command streams ───────

/// Fixed-capacity LE writer. All venus scalars are 4-byte aligned in the stream;
/// `size_t`/`VkDeviceSize`/handle/array_size are 8 bytes, `u32`/`VkResult`/
/// `VkStructureType`/`VkFlags`/`VkCommandTypeEXT` are 4 bytes.
struct Writer {
    buf: [u8; MAX_CMD_BYTES],
    len: usize,
}

impl Writer {
    fn new() -> Self {
        Self {
            buf: [0u8; MAX_CMD_BYTES],
            len: 0,
        }
    }

    fn u32(&mut self, v: u32) {
        let b = v.to_le_bytes();
        self.buf[self.len..self.len + 4].copy_from_slice(&b);
        self.len += 4;
    }

    fn i32(&mut self, v: i32) {
        self.u32(v as u32);
    }

    fn u64(&mut self, v: u64) {
        let b = v.to_le_bytes();
        self.buf[self.len..self.len + 8].copy_from_slice(&b);
        self.len += 8;
    }

    /// A f32 priority value (encoded as its IEEE-754 bits).
    fn f32(&mut self, v: f32) {
        self.u32(v.to_bits());
    }

    /// `vn_encode_simple_pointer` / `vn_encode_array_size`: a u64 count (1 present,
    /// 0 absent / empty array).
    fn count(&mut self, present: bool) {
        self.u64(if present { 1 } else { 0 });
    }

    fn bytes_padded(&mut self, bytes: &[u8]) {
        let padded = (bytes.len() + 3) & !3;
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        for i in bytes.len()..padded {
            self.buf[self.len + i] = 0;
        }
        self.len += padded;
    }

    /// The command header: `VkCommandTypeEXT | VkCommandFlagsEXT`.
    fn header(&mut self, cmd_type: u32, flags: u32) {
        self.u32(cmd_type);
        self.u32(flags);
    }

    fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len]
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
    /// Poison latch: set when a ring wait hits its spin bound or the host
    /// reports RING_STATUS_FATAL. A wedged/fatal ring never recovers, and the
    /// allocation path reaches these waits at DISPATCH_LEVEL under the device
    /// spinlock — without the latch every subsequent call re-burned the full
    /// RING_POLL_SPINS budget (~1 s each), which is the 2026-07-03 guest
    /// wedge. Once set, every ring command fails fast with `DeviceError`.
    fatal: bool,
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
            self.fatal = true;
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
        self.submit_direct(adapter, w.as_slice())?;
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
                self.fatal = true;
                return Err(VirtioError::DeviceError);
            }
            if slept_ms >= RING_WAIT_TIMEOUT_MS {
                self.fatal = true;
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
        self.write_to_ring(set.as_slice())?;

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
        self.submit_direct(adapter, sub.as_slice())?;

        let mut wait = Writer::new();
        wait.header(CMD_WAIT_VIRTQUEUE_SEQNO_MESA, 0);
        wait.u64(seqno);
        self.submit_direct(adapter, wait.as_slice())
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
            self.ring_command_reply(adapter, w.as_slice())?;

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
        let res_id = ctrl::resource_create_blob(
            adapter,
            self.ctx_id,
            VIRTIO_GPU_BLOB_MEM_HOST3D,
            flags,
            memory_id,
            size,
        )?;
        let _ = adapter.with_virtio(|v| v.note_blob_size(res_id, size));
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
        self.ring_command_reply(adapter, w.as_slice())?;

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
        self.ring_command_reply(adapter, w.as_slice())?;

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
        self.ring_command_reply(adapter, w.as_slice())?;

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
        self.ring_command_reply(adapter, w.as_slice())?;

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
        self.ring_command_reply(adapter, w.as_slice())?;

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
        self.ring_command_reply(adapter, w.as_slice())?;

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
        self.ring_command_reply(adapter, w.as_slice())?;

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
        self.ring_command_reply(adapter, w.as_slice())?;

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
        self.ring_command_reply(adapter, w.as_slice())?;

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
        self.ring_command_noreply(adapter, w.as_slice())
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
        self.ring_command_reply(adapter, w.as_slice())?;

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
        let mut w = Writer::new();
        w.header(CMD_BEGIN_COMMAND_BUFFER, CMD_FLAG_GENERATE_REPLY);
        w.u64(command_buffer_id);
        w.count(true);
        w.i32(ST_COMMAND_BUFFER_BEGIN_INFO);
        w.count(false);
        w.u32(COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT);
        w.count(false);
        self.ring_command_reply(adapter, w.as_slice())?;

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
        self.ring_command_noreply(adapter, w.as_slice())
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
        self.ring_command_noreply(adapter, w.as_slice())
    }

    fn end_command_buffer(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: u64,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_END_COMMAND_BUFFER, CMD_FLAG_GENERATE_REPLY);
        w.u64(command_buffer_id);
        self.ring_command_reply(adapter, w.as_slice())?;

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
        self.ring_command_reply(adapter, w.as_slice())?;

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
        self.ring_command_reply(adapter, w.as_slice())?;

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

    fn queue_submit_clear(
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
        self.ring_command_reply(adapter, submit.as_slice())?;

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
            self.queue_submit_clear(adapter, command_buffer_id, fence_id)?;
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
        self.submit_direct(adapter, w.as_slice())
    }

    /// Free a venus `VkDeviceMemory` allocated by [`Self::allocate_memory_blob`]
    /// (`vkFreeMemory` over the ring — cmd 22, wire shape per
    /// `vn_encode_vkFreeMemory`: device id, memory id, null pAllocator). The
    /// caller unrefs the blob RESOURCE separately, before this, so the host drops
    /// the blob's reference on the memory first.
    pub fn free_memory_blob(
        &mut self,
        adapter: &AdapterContext,
        memory_id: u64,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_FREE_MEMORY, 0);
        w.u64(self.device_id);
        w.u64(memory_id);
        w.count(false); // pAllocator = NULL
        self.ring_command_noreply(adapter, w.as_slice())
    }
}

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
    let ring_prep = ctrl::map_blob_prepare(adapter, Some(0), ring_res_id)?;
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
    let reply_prep = ctrl::map_blob_prepare(adapter, Some(0), reply_res_id)?;
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
        client.submit_direct(adapter, w.as_slice())?;
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
        client.ring_command_reply(adapter, w.as_slice())?;
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
        client.ring_command_reply(adapter, w.as_slice())?;
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
        client.ring_command_reply(adapter, w.as_slice())?;

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
        client.ring_command_reply(adapter, w.as_slice())?;

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
    let want_scanout_exts = crate::diag::read_config_dword(b"ScanoutDiag", 0) >= 4;
    // Clear the knock-down VkResult so a clean full-tier success leaves it 0 and
    // a prior boot's value can't be mistaken for this boot's (names persist).
    crate::diag::record_named_bytes(b"SdgDevR", 0);
    // Tier 0 = full, 1 = export-only, 2 = none. Production (scanout off) starts
    // at 2, exactly the old render-only, no-ext behaviour.
    let mut ext_tier: u32 = if want_scanout_exts { 0 } else { 2 };
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
        client.ring_command_reply(adapter, w.as_slice())?;

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
    let pt_prep = ctrl::map_blob_prepare(adapter, Some(0), blob.res_id)?;
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
