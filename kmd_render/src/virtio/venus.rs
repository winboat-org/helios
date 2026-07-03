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
//! IRQL. The entire flow runs at PASSIVE_LEVEL from `DxgkDdiStartDevice` (it
//! `MmMapIoSpace`s the ring/reply blobs and busy-polls the ring head). StartDevice
//! is single-threaded device bring-up, so the transport is not touched
//! concurrently: this module takes `&mut VirtioGpu` directly rather than holding
//! the DISPATCH-level virtio spinlock across the long ring polls (the spinlock
//! discipline exists for the concurrent escape/DPC paths, which are not yet live
//! and never run during StartDevice). See `AdapterContext::with_virtio_passive`.

use core::sync::atomic::{compiler_fence, fence, Ordering};

use helios_protocol::{
    VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE, VIRTIO_GPU_BLOB_MEM_HOST3D, VIRTIO_GPU_MAP_CACHE_CACHED,
    VIRTIO_GPU_MAP_CACHE_UNCACHED, VIRTIO_GPU_MAP_CACHE_WC,
};
use wdk_sys::ntddk::{MmMapIoSpace, MmUnmapIoSpace};
use wdk_sys::{_MEMORY_CACHING_TYPE, PHYSICAL_ADDRESS};

use super::gpu::VirtioGpu;
use super::VirtioError;

// ── venus command type ids (VkCommandTypeEXT) ────────────────────────────────
// Verified against vn_protocol_driver_defines.h.
const CMD_CREATE_INSTANCE: u32 = 0;
const CMD_ENUMERATE_PHYSICAL_DEVICES: u32 = 2;
const CMD_GET_PHYSICAL_DEVICE_MEMORY_PROPERTIES: u32 = 8;
const CMD_CREATE_DEVICE: u32 = 11;
const CMD_ALLOCATE_MEMORY: u32 = 21;
const CMD_FREE_MEMORY: u32 = 22;
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
const ST_MEMORY_ALLOCATE_INFO: i32 = 5;
const ST_RING_CREATE_INFO_MESA: i32 = 1000384000;

// ── VkMemoryPropertyFlags bits we require ────────────────────────────────────
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

/// Busy-poll bound for the ring head advancing past a published seqno. Each
/// iteration is a volatile read + a `spin_loop`; the cap protects against a wedged
/// host without hanging StartDevice forever.
const RING_POLL_SPINS: u64 = 100_000_000;

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
    /// HOST_VISIBLE|HOST_COHERENT memory type chosen during bring-up.
    memory_type_index: u32,
    /// Poison latch: set when a ring wait hits its spin bound or the host
    /// reports RING_STATUS_FATAL. A wedged/fatal ring never recovers, and the
    /// allocation path reaches these waits at DISPATCH_LEVEL under the device
    /// spinlock — without the latch every subsequent call re-burned the full
    /// RING_POLL_SPINS budget (~1 s each), which is the 2026-07-03 guest
    /// wedge. Once set, every ring command fails fast with `DeviceError`.
    fatal: bool,
}

impl VenusClient {
    /// Allocate a fresh guest handle id.
    fn alloc_handle(&mut self) -> u64 {
        let h = self.next_handle;
        self.next_handle += 1;
        h
    }

    /// Send a direct (non-ring) venus command via `VIRTIO_GPU_CMD_SUBMIT_3D`. Used
    /// for the ring-bootstrap commands (`vkCreateRingMESA`, `vkNotifyRingMESA`,
    /// `vkSubmit/WaitVirtqueueSeqnoMESA`) which must reach the host before / around
    /// the ring being usable.
    fn submit_direct(&self, gpu: &mut VirtioGpu, stream: &[u8]) -> Result<(), VirtioError> {
        // fence_id 0: the submit is synchronous (polled used-ring) inside
        // `submit_venus`, so we do not need a per-command fence id here.
        gpu.submit_venus(self.ctx_id, 0, 0, stream)
    }

    /// Publish the ring buffer up to `self.cur` (SeqCst tail store), then nudge the
    /// host if the ring reports idle. Returns the seqno of the just-written command
    /// (== the post-write `cur`). Aborts on a fatal ring status.
    fn publish_and_notify(&mut self, gpu: &mut VirtioGpu) -> Result<u32, VirtioError> {
        if self.fatal {
            return Err(VirtioError::DeviceError);
        }
        let seqno = self.cur;
        self.ring_map.store_u32_seqcst(RING_TAIL_OFFSET, seqno);

        let status = self.ring_map.load_u32_acquire(RING_STATUS_OFFSET);
        if status & RING_STATUS_FATAL != 0 {
            // No diag() — DISPATCH-reachable (see wait_seqno). Latch fatal.
            self.fatal = true;
            return Err(VirtioError::DeviceError);
        }
        // vkNotifyRingMESA(ring, seqno, flags=0): wake the host ring dispatch.
        // Sent UNCONDITIONALLY (not gated on the IDLE status bit): the host idles
        // after a 1 ms timeout and the guest's read of the IDLE bit from the ring
        // shmem is racy/coherency-sensitive — always nudging is correct and cheap
        // for this one-shot bring-up. vkNotifyRingMESA is a valid DIRECT command
        // (unlike vkWaitVirtqueueSeqnoMESA, which the host rejects off-ring).
        self.notify_seqno = self.notify_seqno.wrapping_add(1);
        let mut w = Writer::new();
        w.header(CMD_NOTIFY_RING_MESA, 0);
        w.u64(self.ring_id);
        w.u32(self.notify_seqno);
        w.u32(0); // VkRingNotifyFlagsMESA
        self.submit_direct(gpu, w.as_slice())?;
        Ok(seqno)
    }

    /// Reserve space and copy `stream` into the ring buffer at `self.cur`,
    /// advancing `cur` (vn_ring producer). Spins until the host has consumed enough
    /// to make room (`cur + size - head <= buffer_size`), bounded by RING_POLL_SPINS.
    fn write_to_ring(&mut self, stream: &[u8]) -> Result<(), VirtioError> {
        if self.fatal {
            return Err(VirtioError::DeviceError);
        }
        let size = stream.len() as u32;
        if size as u64 > RING_BUFFER_SIZE as u64 {
            return Err(VirtioError::DeviceError);
        }
        // Wait for buffer space (u32 free-running arithmetic, wrap-preserving).
        let mut spins = 0u64;
        loop {
            let head = self.ring_map.load_u32_acquire(RING_HEAD_OFFSET);
            // occupancy after this write = cur + size - head; must be <= buffer_size.
            if self.cur.wrapping_add(size).wrapping_sub(head) <= RING_BUFFER_SIZE {
                break;
            }
            spins += 1;
            if spins >= RING_POLL_SPINS {
                // A ring that stopped draining is a wedged host — latch fatal
                // so later calls fail fast instead of re-spinning (this path
                // runs at DISPATCH under the device spinlock).
                self.fatal = true;
                return Err(VirtioError::DeviceError);
            }
            core::hint::spin_loop();
        }
        self.ring_map.write_ring_buffer(self.cur, stream);
        self.cur = self.cur.wrapping_add(size);
        Ok(())
    }

    /// Poll the ring head until it reaches `seqno` (the host has consumed and
    /// completed the command). Wrap-safe `(i32)(head - seqno) >= 0` compare.
    fn wait_seqno(&mut self, seqno: u32) -> Result<(), VirtioError> {
        if self.fatal {
            return Err(VirtioError::DeviceError);
        }
        let mut spins = 0u64;
        loop {
            let head = self.ring_map.load_u32_acquire(RING_HEAD_OFFSET);
            if (head.wrapping_sub(seqno)) as i32 >= 0 {
                return Ok(());
            }
            let status = self.ring_map.load_u32_acquire(RING_STATUS_OFFSET);
            if status & RING_STATUS_FATAL != 0 {
                // Host declared the ring fatal — it never recovers. NOTE: no
                // diag() here; this loop runs at DISPATCH under the device
                // spinlock (alloc path) where the registry tracer is illegal.
                self.fatal = true;
                return Err(VirtioError::DeviceError);
            }
            spins += 1;
            if spins >= RING_POLL_SPINS {
                self.fatal = true;
                return Err(VirtioError::DeviceError);
            }
            core::hint::spin_loop();
        }
    }

    /// Issue a command WITHOUT a reply through the ring: write → publish → wait.
    #[allow(dead_code)]
    fn ring_command_noreply(
        &mut self,
        gpu: &mut VirtioGpu,
        stream: &[u8],
    ) -> Result<(), VirtioError> {
        self.write_to_ring(stream)?;
        let seqno = self.publish_and_notify(gpu)?;
        self.wait_seqno(seqno)
    }

    /// Issue a command WITH a reply through the ring. First points the host at the
    /// reply shmem (`vkSetReplyCommandStreamMESA`), then writes the real command
    /// with the GENERATE_REPLY flag, publishes once, and waits. The reply lands at
    /// reply-shmem offset 0; the caller decodes it via a fresh [`ReplyReader`].
    fn ring_command_reply(
        &mut self,
        gpu: &mut VirtioGpu,
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

        let seqno = self.publish_and_notify(gpu)?;
        self.wait_seqno(seqno)?;
        Ok(())
    }

    /// Warm up the reply shmem so the host maps it before the first reply: a
    /// virtqueue-seqno submit + wait roundtrip (`vkSubmit/WaitVirtqueueSeqnoMESA`,
    /// both direct). seqno is monotonic from 1.
    fn reply_shmem_roundtrip(&mut self, gpu: &mut VirtioGpu) -> Result<(), VirtioError> {
        self.roundtrip_seqno += 1;
        let seqno = self.roundtrip_seqno;
        let mut sub = Writer::new();
        sub.header(CMD_SUBMIT_VIRTQUEUE_SEQNO_MESA, 0);
        sub.u64(self.ring_id);
        sub.u64(seqno);
        self.submit_direct(gpu, sub.as_slice())?;

        let mut wait = Writer::new();
        wait.header(CMD_WAIT_VIRTQUEUE_SEQNO_MESA, 0);
        wait.u64(seqno);
        self.submit_direct(gpu, wait.as_slice())
    }

    /// Allocate HOST_VISIBLE|HOST_COHERENT Venus device memory and bind it to a
    /// HOST3D blob. Returns the memory id (`blob_id`) and virtio resource id.
    pub fn allocate_memory_blob(
        &mut self,
        gpu: &mut VirtioGpu,
        size: u64,
        mappable: bool,
    ) -> Result<HostVisibleBlob, VirtioError> {
        let size = round_up_page(size.max(4096));
        let memory_id = self.alloc_handle();
        {
            let mut w = Writer::new();
            w.header(CMD_ALLOCATE_MEMORY, CMD_FLAG_GENERATE_REPLY);
            w.u64(self.device_id);
            w.count(true);
            w.i32(ST_MEMORY_ALLOCATE_INFO);
            w.u64(0);
            w.u64(size);
            w.u32(self.memory_type_index);
            w.count(false);
            w.count(true);
            w.u64(memory_id);
            self.ring_command_reply(gpu, w.as_slice())?;

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

        let flags = if mappable {
            VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE
        } else {
            0
        };
        let res_id = gpu.resource_create_blob(
            self.ctx_id,
            VIRTIO_GPU_BLOB_MEM_HOST3D,
            flags,
            memory_id,
            size,
        )?;
        gpu.note_blob_size(res_id, size);
        Ok(HostVisibleBlob {
            blob_id: memory_id,
            res_id,
            gpa: 0,
            size,
        })
    }

    /// Free a venus `VkDeviceMemory` allocated by [`Self::allocate_memory_blob`]
    /// (`vkFreeMemory` over the ring — cmd 22, wire shape per
    /// `vn_encode_vkFreeMemory`: device id, memory id, null pAllocator). The
    /// caller unrefs the blob RESOURCE separately, before this, so the host drops
    /// the blob's reference on the memory first. DISPATCH-safe (fixed-buffer ring
    /// write + bounded seqno poll).
    pub fn free_memory_blob(
        &mut self,
        gpu: &mut VirtioGpu,
        memory_id: u64,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_FREE_MEMORY, 0);
        w.u64(self.device_id);
        w.u64(memory_id);
        w.count(false); // pAllocator = NULL
        self.ring_command_noreply(gpu, w.as_slice())
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
/// Runs at PASSIVE_LEVEL during StartDevice; takes `&mut VirtioGpu` directly (no
/// spinlock) because StartDevice is single-threaded — see the module docs.
pub fn allocate_host_visible_blob(
    gpu: &mut VirtioGpu,
    ctx_id: u32,
) -> Result<(VenusClient, HostVisibleBlob), VirtioError> {
    diag(0x0001);

    // ── 1. Ring shmem: create blob + map into window + kernel-map + zero ──────
    let ring_res_id = gpu.resource_create_blob(
        ctx_id,
        VIRTIO_GPU_BLOB_MEM_HOST3D,
        VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE,
        0, // blob_id 0: ring shmem is host-allocated (no venus mem binding)
        RING_SHMEM_SIZE,
    )?;
    // Track the ring blob so map_blob_prepare can size the mapping.
    gpu.note_blob_size(ring_res_id, RING_SHMEM_SIZE);
    let ring_prep = gpu.map_blob_prepare(ring_res_id)?;
    let ring_map = KernelMap::new(ring_prep.gpa, ring_prep.size, ring_prep.map_cache)
        .ok_or(VirtioError::MmioMapFailed)?;
    ring_map.zero();
    diag(0x0002);

    // ── 2. Reply shmem: create blob + map + kernel-map + zero ─────────────────
    let reply_res_id = gpu.resource_create_blob(
        ctx_id,
        VIRTIO_GPU_BLOB_MEM_HOST3D,
        VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE,
        0,
        REPLY_SHMEM_SIZE,
    )?;
    gpu.note_blob_size(reply_res_id, REPLY_SHMEM_SIZE);
    let reply_prep = gpu.map_blob_prepare(reply_res_id)?;
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
        memory_type_index: 0,
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
        client.submit_direct(gpu, w.as_slice())?;
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
        client.ring_command_reply(gpu, w.as_slice())?;
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
        client.ring_command_reply(gpu, w.as_slice())?;
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
        client.ring_command_reply(gpu, w.as_slice())?;

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
        client.ring_command_reply(gpu, w.as_slice())?;

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
        for i in 0..VK_MAX_MEMORY_TYPES {
            let property_flags = r.read_u32()?;
            let _heap_index = r.read_u32()?;
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
    let device_id = client.alloc_handle();
    client.device_id = device_id;
    {
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
        w.u32(0); // enabledExtensionCount
        w.count(false); // ppEnabledExtensionNames array_size 0
        w.count(false); // simple_pointer(pEnabledFeatures) NULL
        w.count(false); // simple_pointer(pAllocator) NULL
        w.count(true); // simple_pointer(pDevice)
        w.u64(device_id); // VkDevice handle
        client.ring_command_reply(gpu, w.as_slice())?;

        // Reply: [i32 cmd][i32 VkResult][sp u64][u64 device]
        let mut r = ReplyReader::new(&client.reply_map);
        let cmd = r.read_i32()?;
        if cmd as u32 != CMD_CREATE_DEVICE {
            diag(0x00F2);
            return Err(VirtioError::DeviceError);
        }
        let result = r.read_i32()?;
        if result != 0 {
            // If this fails, the host may require a VkDeviceQueueTimelineInfoMESA
            // pNext on the queue-create — see the handover notes.
            diag(0x00F3);
            return Err(VirtioError::DeviceError);
        }
    }
    diag(0x0009);

    // ── 8. vkAllocateMemory — 16 MiB of the chosen HOST_VISIBLE|COHERENT type ──
    // The memory handle id we pick IS the virtio-gpu blob_id used below.
    let blob = client.allocate_memory_blob(gpu, PAGE_TABLE_ALLOC_SIZE, true)?;
    diag(0x000A);

    // ── 9. Create + map the page-table blob backed by the venus memory id ─────
    let pt_prep = gpu.map_blob_prepare(blob.res_id)?;
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
