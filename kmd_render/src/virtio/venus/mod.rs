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
use core::num::NonZeroU64;
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
use crate::irql::PassiveLevel;

mod protocol;
mod ring;
mod commands;
mod present;
mod scanout;

pub(crate) use protocol::*;
use ring::*;
use commands::*;
pub(crate) use present::*;

/// Declare one handle newtype per Vulkan object class.
///
/// One untyped counter minted every handle — images, memory, buffers, pools,
/// command buffers, fences, queues and the device — and every id is a live
/// handle in the SAME host object space. So a swapped argument does not fail
/// loudly: it destroys or rebinds the wrong host object, and surfaces much later
/// as a corrupt frame or a host decoder abort.
/// `cleanup_imported_source_alias(adapter, resource_id, memory_id, image_id)`
/// compiles today and would issue `vkDestroyImage` on a `VkDeviceMemory` handle
/// and `vkFreeMemory` on a `VkImage` handle; the encoder accepts both, only the
/// host notices. Likewise `bind_image_memory(memory_id, image_id)`.
///
/// `NonZeroU64` also retires the "0 means absent" convention and the ~30
/// scattered `if x != 0` guards that implemented it: `Option<VkImageId>` is the
/// same size as the `u64` it replaces, and "not yet valid" stops being encodable
/// as a legal-looking handle the encoder will happily write into the stream.
///
/// The macro exists so eight identical definitions cannot drift; it expands to
/// exactly the three items written below and nothing else.
macro_rules! vk_handle {
    ($(#[$attr:meta])* $name:ident) => {
        $(#[$attr])*
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub struct $name(NonZeroU64);

        impl $name {
            /// The raw handle, for the wire encoder and for the `AtomicU64`
            /// mirrors in `ddi/create_allocation.rs`.
            pub fn get(self) -> u64 {
                self.0.get()
            }

            /// Rebuild from a raw handle — a mirror read, or a value decoded
            /// from a host reply. `0` is not a handle.
            pub fn from_raw(raw: u64) -> Option<Self> {
                NonZeroU64::new(raw).map(Self)
            }
        }

        impl From<$name> for u64 {
            fn from(id: $name) -> u64 {
                id.get()
            }
        }
    };
}

vk_handle!(
    /// `VkImage`.
    VkImageId
);
vk_handle!(
    /// `VkDeviceMemory`. Doubles as the virtio-gpu `blob_id` for KMD-created
    /// blobs — that coupling is real and deliberate, so the raw value still
    /// crosses into `ctrl::resource_create_blob` as a `u64`.
    VkDeviceMemoryId
);
vk_handle!(
    /// `VkBuffer`.
    VkBufferId
);
vk_handle!(
    /// `VkCommandPool`.
    VkCommandPoolId
);
vk_handle!(
    /// `VkCommandBuffer`.
    VkCommandBufferId
);
vk_handle!(
    /// `VkFence`.
    VkFenceId
);
// `VkQueue` and `VkDevice` are the two handles that used to exist in a "not yet
// valid, encoded as 0" state during bring-up. They are newtypes now because
// R608's VenusRing -> VenusInstance -> VenusClient typestate removed that state:
// a VenusClient without a device is unrepresentable, so no Option is needed and
// no encoder has to unwrap one.
vk_handle!(
    /// `VkQueue`. Obtained from `vkGetDeviceQueue`, not minted, but it is still
    /// a guest-assigned handle in the same space.
    VkQueueId
);
vk_handle!(
    /// `VkDevice`.
    VkDeviceId
);

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
    pub image_id: VkImageId,
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
    pub image_id: VkImageId,
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
    pub source_image_id: VkImageId,
    /// `VkDeviceMemory` imported through `VkImportMemoryResourceInfoMESA`.
    /// `None` for a borrowed source, which used to be spelled 0.
    pub source_memory_id: Option<VkDeviceMemoryId>,
    /// KMD-owned OPTIMAL BGRA scratch used only when the Windows-selected
    /// primary format differs from the physical BGRA scanout format. `None`
    /// when no conversion is needed — the common case.
    pub conversion_image_id: Option<VkImageId>,
    pub conversion_memory_id: Option<VkDeviceMemoryId>,
    /// Pool retaining the one-time UNDEFINED-to-GENERAL transition for the
    /// conversion image.
    pub conversion_init_pool_id: Option<VkCommandPoolId>,
    /// Pool owning `command_buffer_id`; retained while submissions may be live.
    pub command_pool_id: VkCommandPoolId,
    /// Reusable source-acquire/copy/release command buffer. Also the publish
    /// word of the `AllocationContext` mirror: a nonzero value there means the
    /// whole snapshot is coherent.
    pub command_buffer_id: VkCommandBufferId,
    /// Persistent adapter-owned destination image baked into the command buffer.
    pub target_image_id: VkImageId,
    /// Geometry of the baked copy, carried for diagnosis of a mismatched
    /// retarget. NOT read by any decision path -- the command buffer already
    /// encodes the extent. Pre-dates T6; surfaced when R906 removed the
    /// crate-wide `dead_code` allow over `mod virtio`, kept because a snapshot
    /// that cannot report its own geometry is harder to debug than one field.
    #[allow(dead_code)]
    pub width: u32,
    #[allow(dead_code)]
    pub height: u32,
}


/// Bring-up stage 2: an instance and a physical device exist on the ring.
///
/// Exists only between `VenusRing::into_instance` and `into_device`. Its only
/// job is to make "we have an instance but not a device yet" a state you cannot
/// call `allocate_memory_blob` from.
struct VenusInstance {
    ring: VenusRing,
    phys_dev_id: NonZeroU64,
}

/// A `memoryTypeIndex` proven to be below the host's reported `memoryTypeCount`.
///
/// The bring-up used to leave this field 0 until stage 6, which is a *legal*
/// index — so "not yet chosen" and "chose type 0" were the same value.
#[derive(Clone, Copy)]
struct MemoryTypeIndex(u32);

/// The persistent venus client owned by the adapter for the device lifetime.
///
/// Holds the ring/reply BAR mappings and the live Vulkan object ids. Dropping it
/// unmaps the kernel mappings; the host-side venus objects and blob resources are
/// torn down implicitly when the persistent virtio context is destroyed (the
/// caller destroys the context in StopDevice and unrefs the page-table blob).
///
/// Reaching this type at all means the full bring-up succeeded: there is no
/// constructor other than [`VenusInstance::into_device`].
pub struct VenusClient {
    /// Stage-1 state. Every ring primitive lives here, so the ~40 methods below
    /// reach it through the small delegating helpers rather than by owning the
    /// fields directly.
    ring: VenusRing,
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
    /// venus device handle. Not `Option`: a `VenusClient` without a device is
    /// unrepresentable, which is the whole point of the typestate.
    device_id: VkDeviceId,
    /// Graphics queue handle from family 0, queue 0.
    queue_id: VkQueueId,
    /// HOST_VISIBLE|HOST_COHERENT memory type chosen during bring-up.
    memory_type_index: MemoryTypeIndex,
    /// Raw VkMemoryPropertyFlags for physical-device memory types.
    memory_type_flags: [u32; VK_MAX_MEMORY_TYPES as usize],
    memory_type_count: u32,
    /// Wire fence of the most recent [`Self::submit_prepared_image_copy`].
    ///
    /// The client submits that fence itself, so it is the only thing that
    /// legitimately knows it. It used to round-trip through an `AtomicU64` in
    /// the caller's `AllocationContext` and come back as a parameter — and the
    /// only writer stored it INSIDE the venus mutex while
    /// `destroy_allocation_ctx` read it OUTSIDE, so a SetVidPnSourceAddress
    /// that had enqueued its outer SUBMIT_3D but whose store this thread had
    /// not yet observed yielded a stale-or-zero fence. With 0 the mandatory
    /// drain was skipped silently and uncounted, the ring marker could be
    /// decoded ahead of the still-pending SUBMIT_3D, and vkDestroyCommandPool
    /// ran against a pool with in-flight work.
    ///
    /// One client-wide field is correct and conservative: `copy_target_image_id`
    /// is a single-slot invariant, wire fence ids are monotonic, and ring-1
    /// submissions retire in order, so draining the highest prepared-copy fence
    /// drains every earlier one. NEVER cleared — waiting on an already-retired
    /// fence returns `Complete` immediately through `fence_wait_prepare`'s
    /// `!in_flight` arm.
    scanout_copy_last_fence: u64,
    /// One-time PREINITIALIZED -> GENERAL -> EXTERNAL setup for the persistent
    /// LINEAR scanout target. The pool/buffer remain live because setup is
    /// intentionally submitted without a fence wait; queue order makes every
    /// later copy execute after it.
    copy_target_image_id: Option<VkImageId>,
    copy_target_init_pool_id: Option<VkCommandPoolId>,
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
        self.memory_type_index.0
    }

    // ── Stage-1 delegates ────────────────────────────────────────────────────
    //
    // The ring primitives belong to VenusRing, which exists before any Vulkan
    // object does. These forwarders exist so the ~40 object methods below read
    // as they did — the alternative was `self.ring.` on 200 lines, which would
    // have buried the actual change.

    /// The persistent venus 3D context id all commands ride.
    fn ctx_id(&self) -> u32 {
        self.ring.ctx_id
    }

    /// The stage-1 PASSIVE proof (R614). See [`VenusRing::passive`] for why a
    /// bring-up-time token is the right provenance for a client method.
    fn passive(&self) -> PassiveLevel {
        self.ring.passive
    }

    fn submit_direct(&self, adapter: &AdapterContext, stream: &[u8]) -> Result<(), VirtioError> {
        self.ring.submit_direct(adapter, stream)
    }

    fn ring_command_noreply(
        &mut self,
        adapter: &AdapterContext,
        stream: &[u8],
    ) -> Result<(), VirtioError> {
        self.ring.ring_command_noreply(adapter, stream)
    }

    /// See [`VenusRing::ring_command_expect`]. Delegating rather than
    /// re-implementing keeps the reader's single construction site on the ring,
    /// where the publish/wait it must follow also lives.
    fn ring_command_expect(
        &mut self,
        adapter: &AdapterContext,
        stream: &[u8],
        check: ReplyCheck,
    ) -> Result<ReplyReader<'_>, VirtioError> {
        self.ring.ring_command_expect(adapter, stream, check)
    }

    /// Mint the next raw handle. Private: every caller goes through one of the
    /// typed constructors below, so a handle cannot be created without deciding
    /// what class of object it names. The counter itself lives on the ring,
    /// which is the stage that exists before any Vulkan object does.
    fn next_raw(&mut self) -> NonZeroU64 {
        self.ring.next_raw()
    }

    fn new_image_id(&mut self) -> VkImageId {
        VkImageId(self.next_raw())
    }

    fn new_memory_id(&mut self) -> VkDeviceMemoryId {
        VkDeviceMemoryId(self.next_raw())
    }

    fn new_buffer_id(&mut self) -> VkBufferId {
        VkBufferId(self.next_raw())
    }

    fn new_command_pool_id(&mut self) -> VkCommandPoolId {
        VkCommandPoolId(self.next_raw())
    }

    fn new_command_buffer_id(&mut self) -> VkCommandBufferId {
        VkCommandBufferId(self.next_raw())
    }

    fn new_fence_id(&mut self) -> VkFenceId {
        VkFenceId(self.next_raw())
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
        passive: PassiveLevel,
        adapter: &AdapterContext,
        destination: PresentBufferDesc,
        fence_id: u64,
    ) {
        crate::diag::record_named_bytes(b"PBPrF", 1);
        match ctrl::wait_fence(passive, adapter, fence_id, 5_000_000_000) {
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
            passive,
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
                // within allocation_size, and prep.size covers that allocation —
                // but that is an argument spread over two files, so take the
                // checked read and breadcrumb the refusal instead.
                let (Some(b), Some(g), Some(r)) = (
                    map.read_byte(offset),
                    map.read_byte(offset + 1),
                    map.read_byte(offset + 2),
                ) else {
                    crate::diag::record_named_bytes(b"PBPrF", 0xE6);
                    return;
                };
                let pixel_sum = u32::from(r) + u32::from(g) + u32::from(b);
                rgb_sum = rgb_sum.saturating_add(pixel_sum);
                nonblack += u32::from(pixel_sum != 0);
            }
        }
        let center_offset = u64::from(destination.height / 2) * u64::from(destination.pitch)
            + u64::from(destination.width / 2) * bytes_per_pixel;
        let (Some(c0), Some(c1), Some(c2), Some(c3)) = (
            map.read_byte(center_offset),
            map.read_byte(center_offset + 1),
            map.read_byte(center_offset + 2),
            map.read_byte(center_offset + 3),
        ) else {
            crate::diag::record_named_bytes(b"PBPrF", 0xE6);
            return;
        };
        let center = u32::from(c0)
            | (u32::from(c1) << 8)
            | (u32::from(c2) << 16)
            | (u32::from(c3) << 24);
        crate::diag::record_named_bytes(b"PBPrNz", nonblack);
        crate::diag::record_named_bytes(b"PBPrSum", rgb_sum);
        crate::diag::record_named_bytes(b"PBPrCtr", center);
        crate::diag::record_named_bytes(b"PBPrF", 0x10);
    }

    /// Destroy a Venus `VkImage` allocated by the kernel Venus client. Best
    /// effort: allocation teardown must not wedge if the host context is already
    /// being destroyed.
    pub fn destroy_image(
        &mut self,
        adapter: &AdapterContext,
        image_id: u64,
    ) -> Result<(), VirtioError> {
        let image_id = VkImageId::from_raw(image_id).ok_or(VirtioError::DeviceError)?;
        let mut w = Writer::new();
        w.header(CMD_DESTROY_IMAGE, 0);
        w.handle(self.device_id);
        w.handle(image_id);
        w.count(false);
        self.submit_direct(adapter, w.as_slice()?)
    }

    /// Enqueue a raw Venus `vkFreeMemory` command.
    fn free_memory_object(
        &mut self,
        adapter: &AdapterContext,
        memory_id: VkDeviceMemoryId,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_FREE_MEMORY, 0);
        w.handle(self.device_id);
        w.handle(memory_id);
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
        let memory_id = VkDeviceMemoryId::from_raw(memory_id).ok_or(VirtioError::DeviceError)?;
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
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
) -> Result<(VenusClient, HostVisibleBlob), VirtioError> {
    diag(0x0001);

    // Stage 1 -> 2 -> 3, each transition consuming the previous value. Every
    // stage is #[inline(never)]: this runs on DxgkDdiStartDevice's stack, which
    // is 24 KB shared with dxgkrnl's own frames above us, and 22.22.181.0
    // proved what an extra kilobyte there costs (0xc0000001 at boot, with no
    // dump and no bugcheck event, and NO reproduction on a live restart-device).
    // Keeping each stage's locals in a transient frame is load-bearing, not
    // style. See tools/kmd-frame-sizes.ps1.
    let ring = VenusRing::bring_up(passive, adapter, ctx_id)?;
    let instance = ring.into_instance(adapter)?;
    let mut client = instance.into_device(adapter)?;

    // ── 8. vkAllocateMemory — 16 MiB of the chosen HOST_VISIBLE|COHERENT type ──
    // The memory handle id we pick IS the virtio-gpu blob_id used below.
    let blob = client.allocate_memory_blob(adapter, PAGE_TABLE_ALLOC_SIZE, true, false)?;
    diag(0x000A);

    // ── 9. Create + map the page-table blob backed by the venus memory id ─────
    let pt_prep = ctrl::map_blob_prepare(
        passive,
        adapter,
        super::gpu::OwnerFilter::Exactly(None),
        blob.res_id,
    )?;
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

impl VenusRing {
    /// Bring-up stage 1: create and map the ring and reply shmem blobs, then
    /// register the ring with the host (`vkCreateRingMESA`).
    ///
    /// Breadcrumbs 0x0002 (ring mapped), 0x0003 (reply mapped), 0x0004 (ring
    /// registered), 0x0005 (no warm-up) — unchanged values, unchanged order.
    #[inline(never)]
    fn bring_up(
        passive: PassiveLevel,
        adapter: &AdapterContext,
        ctx_id: u32,
    ) -> Result<Self, VirtioError> {
        // ── 1. Ring shmem: create blob + map into window + kernel-map + zero ──
        let ring_res_id = ctrl::resource_create_blob(
            passive,
            adapter,
            ctx_id,
            VIRTIO_GPU_BLOB_MEM_HOST3D,
            VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE,
            0, // blob_id 0: ring shmem is host-allocated (no venus mem binding)
            RING_SHMEM_SIZE,
        )?;
        // Track the ring blob (owner 0) so the map below can size the mapping.
        let _ = adapter.with_virtio(|v| v.note_blob_size(ring_res_id, RING_SHMEM_SIZE));
        let ring_prep = ctrl::map_blob_prepare(
            passive,
            adapter,
            super::gpu::OwnerFilter::Exactly(None),
            ring_res_id,
        )?;
        let ring_map = RingMap::new(ring_prep.gpa, ring_prep.size, ring_prep.map_cache)
            .ok_or(VirtioError::MmioMapFailed)?;
        ring_map.zero();
        diag(0x0002);

        // ── 2. Reply shmem: create blob + map + kernel-map + zero ─────────────
        let reply_res_id = ctrl::resource_create_blob(
            passive,
            adapter,
            ctx_id,
            VIRTIO_GPU_BLOB_MEM_HOST3D,
            VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE,
            0,
            REPLY_SHMEM_SIZE,
        )?;
        let _ = adapter.with_virtio(|v| v.note_blob_size(reply_res_id, REPLY_SHMEM_SIZE));
        let reply_prep = ctrl::map_blob_prepare(
            passive,
            adapter,
            super::gpu::OwnerFilter::Exactly(None),
            reply_res_id,
        )?;
        let reply_map = KernelMap::new(reply_prep.gpa, reply_prep.size, reply_prep.map_cache)
            .ok_or(VirtioError::MmioMapFailed)?;
        reply_map.zero();
        diag(0x0003);

        let ring = Self {
            // A distinctive, unique-enough ring token (any 64-bit value works).
            ring_id: 0x4845_4C49_4F53_0001, // "HELIOS\0\x01"
            ring_res_id,
            reply_res_id,
            ring_map,
            reply_map,
            cur: 0,
            notify_seqno: 0,
            next_handle: NonZeroU64::MIN,
            ctx_id,
            passive,
            fatal: false,
        };

        // ── 3. vkCreateRingMESA (direct) — register the ring with the host ────
        {
            let mut w = Writer::new();
            w.header(CMD_CREATE_RING_MESA, 0);
            w.u64(ring.ring_id);
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
            ring.submit_direct(adapter, w.as_slice()?)?;
        }
        diag(0x0004);

        // ── 3b. (no warm-up) ──────────────────────────────────────────────────
        // The host maps the reply shmem when it processes
        // vkSetReplyCommandStreamMESA on the ring, so no separate roundtrip is
        // needed. The previous warm-up used a DIRECT vkWaitVirtqueueSeqnoMESA,
        // which the host rejects ("must be called on ring dispatch") — removed.
        diag(0x0005);
        Ok(ring)
    }

    /// Bring-up stages 4-5: `vkCreateInstance`, then
    /// `vkEnumeratePhysicalDevices` (count call, then array call).
    ///
    /// Consumes the ring, so no caller can keep a handle to the stage that
    /// cannot encode a `VkInstance`.
    #[inline(never)]
    fn into_instance(mut self, adapter: &AdapterContext) -> Result<VenusInstance, VirtioError> {
        // ── 4. vkCreateInstance (ring, reply) ─────────────────────────────────
        let instance_id = self.next_raw();
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
            w.u64(instance_id.get()); // VkInstance handle
            // Reply: [i32 cmd][i32 VkResult][simple_pointer u64][u64 instance]
            self.ring_command_expect(
                adapter,
                w.as_slice()?,
                ReplyCheck::new(CMD_CREATE_INSTANCE)
                    .mismatch(0x00E5)
                    .refuse_result(0x00E6),
            )?;
        }
        diag(0x0006);

        // ── 5. vkEnumeratePhysicalDevices — count, then array (request 1) ─────
        // Count call first (some hosts require it before the array call).
        {
            let mut w = Writer::new();
            w.header(CMD_ENUMERATE_PHYSICAL_DEVICES, CMD_FLAG_GENERATE_REPLY);
            w.u64(instance_id.get()); // VkInstance
            w.count(true); // simple_pointer(pPhysicalDeviceCount)
            w.u32(0); // *pPhysicalDeviceCount = 0
            w.count(false); // pPhysicalDevices NULL → array_size 0
            // We don't strictly need the count value; just validate the header.
            self.ring_command_expect(
                adapter,
                w.as_slice()?,
                ReplyCheck::new(CMD_ENUMERATE_PHYSICAL_DEVICES).mismatch(0x00E7),
            )?;
        }
        // Array call: request up to 1 physical device. Physical-device handles
        // are GUEST-assigned like all venus handles (the host rejects a 0
        // placeholder with "invalid object id 0"), so pre-allocate an id.
        let phys_dev_id = self.next_raw();
        {
            let mut w = Writer::new();
            w.header(CMD_ENUMERATE_PHYSICAL_DEVICES, CMD_FLAG_GENERATE_REPLY);
            w.u64(instance_id.get()); // VkInstance
            w.count(true); // simple_pointer(pPhysicalDeviceCount)
            w.u32(1); // *pPhysicalDeviceCount = 1
            w.count(true); // pPhysicalDevices present → array_size 1 follows
            w.u64(phys_dev_id.get()); // guest-assigned VkPhysicalDevice for slot 0
            // Reply: [i32 cmd][i32 VkResult][sp u64][u32 count][array u64][u64 id×N]
            //
            // The VkResult is DEFERRED to this site, not refused by the helper:
            // VK_INCOMPLETE is an acceptable answer here.
            let mut r = self.ring_command_expect(
                adapter,
                w.as_slice()?,
                ReplyCheck::new(CMD_ENUMERATE_PHYSICAL_DEVICES).mismatch(0x00E8),
            )?;
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
            // Slot 0: the host echoes our guest-assigned id; validate it's
            // present but keep using our `phys_dev_id` for later commands.
            let reply_pd = r.read_u64()?;
            if reply_pd == 0 {
                diag(0x00ED);
                return Err(VirtioError::DeviceError);
            }
        }
        diag(0x0007);
        Ok(VenusInstance {
            ring: self,
            phys_dev_id,
        })
    }
}

impl VenusInstance {
    /// `vkGetDeviceQueue2` for family 0, queue 0 on ring 1.
    ///
    /// Takes the device id as an argument rather than reading a field: at this
    /// point the device exists but the `VenusClient` that will own it does not,
    /// which is exactly the window the old two-phase init left writable.
    fn get_device_queue(
        &mut self,
        adapter: &AdapterContext,
        device_id: VkDeviceId,
    ) -> Result<VkQueueId, VirtioError> {
        let queue_id = self.ring.next_raw();
        let mut w = Writer::new();
        w.header(CMD_GET_DEVICE_QUEUE_2, CMD_FLAG_GENERATE_REPLY);
        w.handle(device_id);
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
        w.u64(queue_id.get());
        // No VkResult in this reply shape: word 1 is the simple-pointer.
        let mut r = self.ring.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_GET_DEVICE_QUEUE_2).mismatch(0x0110),
        )?;
        if r.read_u64()? == 0 {
            diag(0x0111);
            return Err(VirtioError::DeviceError);
        }
        // The host may substitute its own handle; adopt it when it does.
        let returned = r.read_u64()?;
        Ok(VkQueueId::from_raw(returned).unwrap_or(VkQueueId(queue_id)))
    }

    /// Bring-up stages 6-7 plus the queue: memory properties, the CreateDevice
    /// extension ladder, and `vkGetDeviceQueue2`.
    ///
    /// The ladder computes the device id in a LOCAL and returns it from the
    /// loop; it can no longer write a half-built client mid-retry. Each attempt
    /// still mints a FRESH handle — reusing one across tiers would make a retry
    /// collide with the host's record of the failed device.
    #[inline(never)]
    fn into_device(mut self, adapter: &AdapterContext) -> Result<VenusClient, VirtioError> {
        // ── 6. vkGetPhysicalDeviceMemoryProperties — pick HOST_VISIBLE|COHERENT
        let mut memory_type_flags = [0u32; VK_MAX_MEMORY_TYPES as usize];
        let memory_type_count;
        let memory_type_index;
        {
            let mut w = Writer::new();
            w.header(
                CMD_GET_PHYSICAL_DEVICE_MEMORY_PROPERTIES,
                CMD_FLAG_GENERATE_REPLY,
            );
            w.u64(self.phys_dev_id.get()); // VkPhysicalDevice
            w.count(true); // simple_pointer(pMemoryProperties)
                           // partial-encoded struct: array_size(32) then array_size(16).
            w.u64(VK_MAX_MEMORY_TYPES as u64);
            w.u64(VK_MAX_MEMORY_HEAPS as u64);
            // Reply (NO VkResult): [i32 cmd][sp u64][u32 typeCount][array u64]
            //   [ (u32 propertyFlags, u32 heapIndex) × 32 ]
            //   [u32 heapCount][array u64][ (u64 size, u32 flags) × 16 ]
            let mut r = self.ring.ring_command_expect(
                adapter,
                w.as_slice()?,
                ReplyCheck::new(CMD_GET_PHYSICAL_DEVICE_MEMORY_PROPERTIES).mismatch(0x00EE),
            )?;
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
            memory_type_count = type_count;
            for i in 0..VK_MAX_MEMORY_TYPES {
                let property_flags = r.read_u32()?;
                let _heap_index = r.read_u32()?;
                memory_type_flags[i as usize] = property_flags;
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
                // The index is < type_count by construction of the loop guard,
                // which is what MemoryTypeIndex asserts.
                Some(idx) => memory_type_index = MemoryTypeIndex(idx),
                None => {
                    diag(0x00F1);
                    return Err(VirtioError::DeviceError);
                }
            }
        }
        diag(0x0008);

        // ── 7. vkCreateDevice — one queue, family 0, priority 1.0 ─────────────
        let device_id = self.create_device_with_ext_ladder(adapter)?;
        diag(0x0009);

        let queue_id = self.get_device_queue(adapter, device_id)?;
        diag(0x000D);

        Ok(VenusClient {
            ring: self.ring,
            probe_pending: None,
            scanout_copy_last_fence: 0,
            device_id,
            queue_id,
            memory_type_index,
            memory_type_flags,
            memory_type_count,
            copy_target_image_id: None,
            copy_target_init_pool_id: None,
            present_images: Vec::with_capacity(MAX_PRESENT_IMAGES),
            present_buffers: Vec::with_capacity(MAX_PRESENT_BUFFERS),
            present_blits: Vec::with_capacity(MAX_PRESENT_BLITS),
            owned_memory_blobs: Vec::with_capacity(MAX_OWNED_MEMORY_BLOBS),
        })
    }

    /// The CreateDevice extension ladder: export-trio → none.
    ///
    /// The proven scanout/export shape (`/tmp/vk-dmabuf-scanout.c`, the CachyOS
    /// NVIDIA egl-headless success) needs only the external-memory + DMA_BUF
    /// trio, which is what production asks for. The ladder exists because a
    /// zero-ext device silently rejects every DMA_BUF export op — the whole
    /// scanout path dies with no visible reason, since the S-ring is off at
    /// DiagLevel 0 — so stepping down has to be visible in `SdgDevX`/`SdgDevR`
    /// rather than inferred.
    ///
    /// T6/R901 removed the third tier above it (the 5-ext set adding
    /// `VK_EXT_image_drm_format_modifier` + `VK_KHR_image_format_list`), which
    /// only a `ScanoutDiag >= 4` registry value ever selected. Tier NUMBERING is
    /// deliberately unchanged — 1 = export-trio, 2 = none — so `SdgDevX` means
    /// the same thing before and after.
    ///
    /// The tier that stuck (`SdgDevX`) and every knock-down VkResult
    /// (`SdgDevR`) go to fixed registry names so a `reg query` reveals them
    /// without the S-ring. Both are owner bring-up ABI: do not collapse the
    /// per-attempt `SdgDevR` record into a single write.
    #[inline(never)]
    fn create_device_with_ext_ladder(
        &mut self,
        adapter: &AdapterContext,
    ) -> Result<VkDeviceId, VirtioError> {
        const EXT_EXPORT: [&[u8]; 3] = [
            b"VK_KHR_external_memory\0",
            b"VK_KHR_external_memory_fd\0",
            b"VK_EXT_external_memory_dma_buf\0",
        ];
        // ⚠ THE MODIFIER TIER IS GONE, AND THAT IS THE POINT (T6/R901).
        // `EXT_FULL` additionally requested `VK_KHR_image_format_list` and
        // `VK_EXT_image_drm_format_modifier` on the ONE production `VkDevice`
        // every render / scanout / GDI path then uses -- and a leftover
        // `ScanoutDiag >= 4` in the registry selected it. That is the
        // 38th-session global-modifier-enable regression class: enabling those
        // extensions inflated the memory requirements of ordinary shared
        // OPTIMAL imports, producing valid undersized-import refusals, DWM
        // failures, and NVIDIA Xid 31 when the undersize guard was bypassed.
        // With the tier deleted there is NO configuration -- registry or
        // otherwise -- in which the production device is created with them.
        //
        // Production DisplayHalf needs only the export trio for its dedicated
        // plain LINEAR DMA_BUF image.
        let want_scanout_exts = self.ring.ctx_id != 0 && adapter.display_half();
        // Clear the knock-down VkResult so a clean first-tier success leaves it
        // 0 and a prior boot's value can't be mistaken for this boot's (names
        // persist across boots).
        crate::diag::record_named_bytes(b"SdgDevR", 0);
        // Tier 1 = export-only, 2 = none. Render-only starts at 2, exactly the
        // old no-ext behaviour. Tier numbering is UNCHANGED so `SdgDevX` keeps
        // its meaning across the deletion: a DisplayHalf boot still reads 1.
        let mut ext_tier: u32 = if want_scanout_exts { 1 } else { 2 };
        loop {
            let exts: &[&[u8]] = match ext_tier {
                1 => &EXT_EXPORT,
                _ => &[],
            };
            // A FRESH handle per attempt. Reusing one across tiers would make a
            // retry collide with the host's record of the failed device.
            let device_id = VkDeviceId(self.ring.next_raw());
            let mut w = Writer::new();
            w.header(CMD_CREATE_DEVICE, CMD_FLAG_GENERATE_REPLY);
            w.u64(self.phys_dev_id.get()); // VkPhysicalDevice
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
            w.handle(device_id); // VkDevice handle
            // Reply: [i32 cmd][i32 VkResult][sp u64][u64 device]
            //
            // The VkResult is DEFERRED to this site, not refused by the helper:
            // a non-zero result steps the extension ladder down a tier rather
            // than failing the bring-up.
            let mut r = self.ring.ring_command_expect(
                adapter,
                w.as_slice()?,
                ReplyCheck::new(CMD_CREATE_DEVICE).mismatch(0x00F2),
            )?;
            let result = r.read_i32()?;
            if result == 0 {
                // Which extension tier the device actually got. mode 16 needs
                // >= 1 (export trio); a value of 2 means NO export exts →
                // scanout can't work and SdgLImg/SdgLMem will show the
                // rejection downstream.
                crate::diag::record_named_bytes(b"SdgDevX", ext_tier);
                return Ok(device_id);
            }
            // Record the VkResult that knocked this tier down before stepping.
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
    }
}
