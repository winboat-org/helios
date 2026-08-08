//! The exact-primary present path: the source/destination descriptors, the
//! three bounded import/borrow caches, the five reusable-command recorders and
//! `submit_present_blt`.
//!
//! Moved verbatim out of `virtio/venus.rs` by T8/R1104.

use super::ring::*;
use super::*;

/// Exact external-allocation contract for recreating one ordinary Helios/DXVK
/// shared OPTIMAL Present image in the kernel Venus device.
///
/// Construction validates the shape once. Cache equality includes every Vulkan
/// memory-requirements input rather than relying on resource-id heuristics.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct OptimalPresentImageDesc {
    pub(super) resource_id: u32,
    pub(super) allocation_size: u64,
    pub(super) memory_type_index: u32,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) ddi_bind_flags: u32,
    pub(super) dxgi_format: u32,
    /// The parsed form of `dxgi_format`, stored at construction rather than
    /// re-derived on every read. `new` already had to prove the format was
    /// convertible; spending that proof on an `is_some` test and then
    /// re-deriving it behind a panicking accessor made a `KeBugCheck` reachable
    /// from any struct literal added inside this module (the fields are private
    /// only *outside* `venus.rs`).
    pub(super) pixel_format: PresentPixelFormat,
    pub(super) transport: OptimalImageTransport,
}

impl OptimalPresentImageDesc {
    pub(super) fn new(
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

    pub(super) fn pixel_format(self) -> PresentPixelFormat {
        self.pixel_format
    }

    pub(crate) fn resource_id(self) -> u32 {
        self.resource_id
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
    pub(super) resource_id: u32,
    pub(super) allocation_size: u64,
    pub(super) memory_type_index: u32,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) pitch: u32,
    pub(super) dxgi_format: u32,
    /// Parsed once in `new`, which already binds it to size the row. See
    /// [`OptimalPresentImageDesc::pixel_format`] for why this is a field.
    pub(super) pixel_format: PresentPixelFormat,
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

    pub(super) fn pixel_format(self) -> PresentPixelFormat {
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
    pub(crate) fn resource_id(self) -> u32 {
        match self {
            Self::StandardBuffer(desc) => desc.resource_id,
            Self::OptimalImage(desc) => desc.resource_id,
        }
    }

    pub(super) fn width(self) -> u32 {
        match self {
            Self::StandardBuffer(desc) => desc.width,
            Self::OptimalImage(desc) => desc.width,
        }
    }

    pub(super) fn height(self) -> u32 {
        match self {
            Self::StandardBuffer(desc) => desc.height,
            Self::OptimalImage(desc) => desc.height,
        }
    }

    /// The DXGI format of either backing kind. Unused today: every caller has
    /// the concrete variant in hand. Pre-dates T6; kept as the one place the
    /// two variants' format fields are unified. R906.
    #[allow(dead_code)]
    pub(super) fn dxgi_format(self) -> u32 {
        match self {
            Self::StandardBuffer(desc) => desc.dxgi_format,
            Self::OptimalImage(desc) => desc.dxgi_format,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct ImportedOptimalImage {
    pub(super) desc: OptimalPresentImageDesc,
    pub(super) image_id: VkImageId,
    pub(super) memory_id: VkDeviceMemoryId,
}

/// One `VkDeviceMemory` allocation created by a Venus blob allocator and
/// exposed as exactly one HOST3D resource.
///
/// This is the authoritative same-device identity for a KMD standard allocation.
/// Present may borrow this memory for a buffer binding; it must never manufacture
/// a second imported `VkDeviceMemory` for the same resource.
#[derive(PartialEq, Eq)]
pub(super) struct OwnedMemoryBlob {
    pub(super) resource_id: u32,
    pub(super) memory_id: VkDeviceMemoryId,
    pub(super) allocation_size: u64,
    pub(super) memory_type_index: u32,
    /// A buffer created before memory selection so its actual compatibility
    /// mask can choose the heap.  Ownership moves to `present_buffers` on the
    /// first Present; if that never happens, `free_memory_blob` destroys it.
    pub(super) prepared_present_buffer: Option<VkBufferId>,
    /// One-shot initial family-0 -> EXTERNAL release objects. These are normally
    /// destroyed before allocation publication. They remain recorded only when
    /// submission/wait/cleanup failed ambiguously, in which case context
    /// teardown—not local rollback—must reclaim them safely.
    pub(super) initial_release_pool_id: Option<VkCommandPoolId>,
    pub(super) initial_release_fence_id: Option<VkFenceId>,
}

/// A Present destination buffer bound to KMD-owned memory.
///
/// The absence of an "owned memory" or "attached resource" variant is
/// intentional: dropping this cache entry destroys only `buffer_id`. The
/// AllocationContext remains the sole owner of the memory and virtio resource.
#[derive(Clone, Copy)]
pub(super) struct BorrowedPresentBuffer {
    pub(super) desc: PresentBufferDesc,
    pub(super) buffer_id: VkBufferId,
    pub(super) memory_id: VkDeviceMemoryId,
}

pub(super) struct PreparedPresentBlt {
    pub(super) source_resource_id: u32,
    pub(super) destination_resource_id: u32,
    pub(super) command_pool_id: VkCommandPoolId,
    pub(super) command_buffer_id: VkCommandBufferId,
    /// Optional KMD-owned conversion scratch. It is an internal command
    /// dependency only; it never substitutes for either Windows allocation.
    /// `None` rather than 0: absent scratch is now a state of the type.
    pub(super) conversion_image_id: Option<VkImageId>,
    pub(super) conversion_memory_id: Option<VkDeviceMemoryId>,
    pub(super) conversion_init_pool_id: Option<VkCommandPoolId>,
    /// A virtio WIRE fence id, NOT a `VkFence` — deliberately still a bare u64.
    pub(super) last_wire_fence_id: u64,
    pub(super) submit_count: u32,
    pub(super) probe_done: bool,
}

/// PASSIVE-time cache preparation result. It contains only stable cache
/// identity and the exact destination contract; actual ring-1 submission is a
/// separate later operation after SubmitCommand has admitted residency.
#[derive(Clone, Copy)]
pub(crate) struct PreparedPresentBltSubmission {
    blt_index: usize,
    command_buffer_id: VkCommandBufferId,
    destination: PresentDestinationDesc,
}

impl PreparedPresentBlt {
    /// Destroy the host objects this record owns, handing the record BACK on
    /// failure.
    ///
    /// The loop this replaces called `pop()` first and then `?`-returned after
    /// it, so a mid-loop failure forgot the cache record while its host objects
    /// were still alive. `release_present_blits_for_resource` goes to real
    /// trouble to be all-or-nothing — it drains every wire fence, then submits
    /// and waits a queue marker before touching anything, and its own comment
    /// says "retain the complete cache ... rather than partially freeing live
    /// objects" — so the loops violated exactly the contract the prologue
    /// establishes. Consuming `self` and returning it in the error makes the
    /// forgotten-record state unrepresentable rather than merely avoided.
    pub(super) fn release(
        self,
        client: &mut VenusClient,
        adapter: &AdapterContext,
    ) -> Result<(), (Self, VirtioError)> {
        if let Err(e) = client.destroy_command_pool(adapter, self.command_pool_id) {
            return Err((self, e));
        }
        if let Some(pool_id) = self.conversion_init_pool_id {
            if let Err(e) = client.destroy_command_pool(adapter, pool_id) {
                return Err((self, e));
            }
        }
        if let Some(image_id) = self.conversion_image_id {
            if let Err(e) = client.destroy_image_on_ring(adapter, image_id) {
                return Err((self, e));
            }
        }
        if let Some(memory_id) = self.conversion_memory_id {
            if let Err(e) = client.free_memory_object(adapter, memory_id) {
                return Err((self, e));
            }
        }
        Ok(())
    }
}

impl ImportedOptimalImage {
    /// Destroy the host objects this import owns, handing the record back on
    /// failure. See [`PreparedPresentBlt::release`].
    ///
    /// This is the record whose loss was worst: `OwnedMemoryBlob`'s own doc
    /// forbids ever manufacturing a second imported `VkDeviceMemory` for one
    /// resource, and a forgotten import does exactly that. The surviving
    /// allocation's next Present re-enters `import_optimal_present_image`,
    /// which calls `attach_resource_checked` on a resource that is still
    /// attached (`ctx_attach_resource` does no dedup) plus a fresh
    /// `allocate_imported_resource_memory` — a second VkDeviceMemory naming the
    /// same backing.
    pub(super) fn release(
        self,
        client: &mut VenusClient,
        adapter: &AdapterContext,
    ) -> Result<(), (Self, VirtioError)> {
        if let Err(e) = client.destroy_image_on_ring(adapter, self.image_id) {
            return Err((self, e));
        }
        if let Err(e) = client.free_memory_blob(adapter, self.memory_id.get()) {
            return Err((self, e));
        }
        // Each resource occurs exactly once in present_images.
        if let Err(e) = ctrl::ctx_detach_resource(
            client.passive(),
            adapter,
            client.ctx_id(),
            self.desc.resource_id,
        ) {
            return Err((self, e));
        }
        Ok(())
    }
}

impl BorrowedPresentBuffer {
    /// Destroy the `VkBuffer` this record owns, handing the record back on
    /// failure. See [`PreparedPresentBlt::release`].
    ///
    /// Only the buffer: `BorrowedPresentBuffer` carries no ownership
    /// capability, and the allocation teardown that called us remains solely
    /// responsible for freeing `memory` and unref'ing its resource.
    pub(super) fn release(
        self,
        client: &mut VenusClient,
        adapter: &AdapterContext,
    ) -> Result<(), (Self, VirtioError)> {
        match client.destroy_buffer_on_ring(adapter, self.buffer_id) {
            Ok(()) => Ok(()),
            Err(e) => Err((self, e)),
        }
    }
}

pub(super) const MAX_PRESENT_IMAGES: usize = 32;
pub(super) const MAX_PRESENT_BUFFERS: usize = 16;
pub(super) const MAX_PRESENT_BLITS: usize = 32;
/// Submissions a source/destination pair must complete before the one-shot
/// destination probe is armed for it (`PresentProbe` knob only).
///
/// 8 rather than 1 because the first submissions of a pair are the ones most
/// likely to race DWM's own surface churn: an early sample can catch the
/// destination between the allocation being opened and the first copy actually
/// retiring, which reads as "the copy did not populate the destination" when
/// nothing is wrong. By the 8th submission the pair is steady state.
pub(super) const PRESENT_PROBE_AFTER_SUBMITS: u32 = 8;
/// Every owned memory blob is also tracked by VirtioGpu's bounded blob table,
/// so the transport's capacity is an exact upper bound rather than a second,
/// divergent resource limit.
pub(super) const MAX_OWNED_MEMORY_BLOBS: usize = crate::virtio::gpu::MAX_BLOBS;

/// Complete D3D11/DXGI swapchain format set that Helios can recreate as an
/// exact Vulkan image alias.
///
/// Values are selected only from the creator's retained DXGI format. Typeless,
/// integer, depth, compressed, and XR-bias formats are intentionally excluded:
/// DXVK does not expose them as importable D3D11 swapchain storage.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PresentPixelFormat {
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
    pub(super) fn from_dxgi(dxgi_format: u32) -> Option<Self> {
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

    pub(super) fn vk_format(self) -> u32 {
        match self {
            Self::Rgba8Unorm => FORMAT_R8G8B8A8_UNORM,
            Self::Rgba8Srgb => FORMAT_R8G8B8A8_SRGB,
            Self::Bgra8Unorm | Self::Bgrx8Unorm => FORMAT_B8G8R8A8_UNORM,
            Self::Bgra8Srgb | Self::Bgrx8Srgb => FORMAT_B8G8R8A8_SRGB,
            Self::Rgb10a2Unorm => FORMAT_A2B10G10R10_UNORM_PACK32,
            Self::Rgba16Float => FORMAT_R16G16B16A16_SFLOAT,
        }
    }

    pub(super) fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Rgba16Float => 8,
            _ => 4,
        }
    }
}

impl VenusClient {
    /// Import one validated ordinary shared OPTIMAL image into the persistent
    /// KMD Venus device. The imported image exactly reproduces the creator's
    /// extent, usage and external-memory allocation contract.
    pub(super) fn import_optimal_present_image(
        &mut self,
        adapter: &AdapterContext,
        desc: OptimalPresentImageDesc,
    ) -> Result<ImportedOptimalImage, VirtioError> {
        if desc.memory_type_index >= self.memory_type_count {
            return Err(VirtioError::DeviceError);
        }

        ctrl::attach_resource_checked(self.passive(), adapter, self.ctx_id(), desc.resource_id)?;
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
                let _ = ctrl::ctx_detach_resource(
                    self.passive(),
                    adapter,
                    self.ctx_id(),
                    desc.resource_id,
                );
                return Err(e);
            }
        };

        let (required_size, memory_type_bits) = match self
            .image_memory_requirements(adapter, image_id)
        {
            Ok(requirements) => requirements,
            Err(e) => {
                let _ =
                    self.cleanup_imported_source_alias(adapter, desc.resource_id, image_id, None);
                return Err(e);
            }
        };
        if required_size > desc.allocation_size
            || (memory_type_bits & (1u32 << desc.memory_type_index)) == 0
        {
            let _ = self.cleanup_imported_source_alias(adapter, desc.resource_id, image_id, None);
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
                let _ =
                    self.cleanup_imported_source_alias(adapter, desc.resource_id, image_id, None);
                return Err(e);
            }
        };
        if let Err(e) = self.bind_image_memory(adapter, image_id, memory_id) {
            let _ = self.cleanup_imported_source_alias(
                adapter,
                desc.resource_id,
                image_id,
                Some(memory_id),
            );
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
    pub(super) fn ensure_present_image(
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

    pub(super) fn borrow_present_buffer(
        &mut self,
        desc: PresentBufferDesc,
    ) -> Result<BorrowedPresentBuffer, VirtioError> {
        if desc.memory_type_index >= self.memory_type_count {
            return Err(VirtioError::DeviceError);
        }

        crate::diag::record_named_bytes(b"PBImRs", desc.resource_id);
        crate::diag::record_named_bytes(b"PBImSt", 1);
        let Some(memory_index) = self
            .owned_memory_blobs
            .iter()
            .position(|blob| blob.resource_id == desc.resource_id)
        else {
            // PitchedStandardBuffer is a strict local-memory contract. A
            // resource not created by allocate_memory_blob must be represented
            // by another destination type; importing it heuristically here
            // would recreate the failure this path is designed to prevent.
            crate::diag::record_named_bytes(b"PBImSt", 0xE1);
            return Err(VirtioError::DeviceError);
        };
        if self.owned_memory_blobs[memory_index].allocation_size != desc.allocation_size
            || self.owned_memory_blobs[memory_index].memory_type_index != desc.memory_type_index
        {
            crate::diag::record_named_bytes(b"PBImSt", 0xE2);
            return Err(VirtioError::DeviceError);
        }

        let memory_id = self.owned_memory_blobs[memory_index].memory_id;
        if let Some(buffer_id) = self.owned_memory_blobs[memory_index]
            .prepared_present_buffer
            .take()
        {
            // Transfer the sole buffer-ownership capability from the memory
            // registry to the Present cache.  It was created, requirement-
            // checked and bound before the blob was published.
            crate::diag::record_named_bytes(b"PBImSt", 0x10);
            return Ok(BorrowedPresentBuffer {
                desc,
                buffer_id,
                memory_id,
            });
        }

        // Present-buffer memory is always exported as a dedicated allocation
        // for the exact prepared VkBuffer. Binding it to a newly-created buffer
        // is forbidden even when every VkBufferCreateInfo field matches. If the
        // prepared capability is absent and the cache did not find it above,
        // the ownership state is inconsistent; refuse instead of violating the
        // dedicated-allocation contract.
        crate::diag::record_named_bytes(b"PBImSt", 0xE3);
        Err(VirtioError::DeviceError)
    }

    pub(super) fn ensure_present_buffer(
        &mut self,
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
        let buffer = self.borrow_present_buffer(desc)?;
        self.present_buffers.push(buffer);
        Ok(buffer)
    }

    /// Record the common reusable EXTERNAL-acquire, GENERAL-layout image copy,
    /// and EXTERNAL-release sequence. Both source forms (imported OPTIMAL alias
    /// and borrowed KMD LINEAR image) use this exact ownership protocol.
    /// The reusable-command-buffer lifecycle every `record_reusable_*` shares:
    /// create a pool, allocate a buffer, begin it SIMULTANEOUS_USE, run `body`,
    /// end it, and destroy the pool on any failure along the way.
    ///
    /// R1003(b). All five recorders repeated those twenty-five lines, including
    /// the two distinct unwind paths (allocate-failed and record-failed).
    ///
    /// ⚠ DELIBERATE DEVIATION from the review's design. It specifies one
    /// `record_reusable_transfer(plan: TransferPlan)` with a five-variant enum
    /// carrying each recorder's body, and names its own risk as "where a barrier
    /// could be dropped for one variant". Extracting only the LIFECYCLE removes
    /// the same duplication without ever merging the bodies, so that risk does
    /// not arise: each recorder's barrier sequence stays verbatim at its own
    /// site, where it can still be diffed against the pre-change source. The
    /// per-variant validation predicates stay at their sites too -- notably
    /// `record_reusable_present_blt`'s `pitch >= width * bpp` guard, which is
    /// meaningful only for the buffer destinations.
    pub(super) fn record_reusable(
        &mut self,
        adapter: &AdapterContext,
        body: impl FnOnce(&mut Self, VkCommandBufferId) -> Result<(), VirtioError>,
    ) -> Result<(VkCommandPoolId, VkCommandBufferId), VirtioError> {
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
            body(self, command_buffer_id)?;
            self.end_command_buffer(adapter, command_buffer_id)
        })();
        if let Err(e) = record_result {
            let _ = self.destroy_command_pool(adapter, command_pool_id);
            return Err(e);
        }
        Ok((command_pool_id, command_buffer_id))
    }

    pub(super) fn record_reusable_image_copy(
        &mut self,
        adapter: &AdapterContext,
        source_image_id: VkImageId,
        target_image_id: VkImageId,
        width: u32,
        height: u32,
    ) -> Result<(VkCommandPoolId, VkCommandBufferId), VirtioError> {
        // The `== 0` arms of this guard are gone: VkImageId is NonZeroU64, so
        // a null source or target is no longer expressible here.
        if source_image_id == target_image_id || width == 0 || height == 0 {
            return Err(VirtioError::DeviceError);
        }

        self.record_reusable(adapter, |s, command_buffer_id| {
            s.cmd_acquire_image_from_external(
                adapter,
                command_buffer_id,
                source_image_id,
                TransferAccess::Read,
            )?;
            s.cmd_acquire_image_from_external(
                adapter,
                command_buffer_id,
                target_image_id,
                TransferAccess::Write,
            )?;
            s.cmd_copy_image(
                adapter,
                command_buffer_id,
                source_image_id,
                target_image_id,
                width,
                height,
            )?;
            s.cmd_release_image_to_external(
                adapter,
                command_buffer_id,
                source_image_id,
                TransferAccess::Read,
            )?;
            s.cmd_release_image_to_external(
                adapter,
                command_buffer_id,
                target_image_id,
                TransferAccess::Write,
            )?;
            Ok(())
        })
    }

    /// Reusable external-image BLT for authoritative source/destination formats
    /// that require Vulkan numeric/color conversion.
    pub(super) fn record_reusable_image_blit(
        &mut self,
        adapter: &AdapterContext,
        source_image_id: VkImageId,
        target_image_id: VkImageId,
        width: u32,
        height: u32,
    ) -> Result<(VkCommandPoolId, VkCommandBufferId), VirtioError> {
        // The `== 0` arms of this guard are gone: VkImageId is NonZeroU64, so
        // a null source or target is no longer expressible here.
        if source_image_id == target_image_id || width == 0 || height == 0 {
            return Err(VirtioError::DeviceError);
        }

        self.record_reusable(adapter, |s, command_buffer_id| {
            s.cmd_acquire_image_from_external(
                adapter,
                command_buffer_id,
                source_image_id,
                TransferAccess::Read,
            )?;
            s.cmd_acquire_image_from_external(
                adapter,
                command_buffer_id,
                target_image_id,
                TransferAccess::Write,
            )?;
            s.cmd_blit_image(
                adapter,
                command_buffer_id,
                source_image_id,
                target_image_id,
                width,
                height,
            )?;
            s.cmd_release_image_to_external(
                adapter,
                command_buffer_id,
                source_image_id,
                TransferAccess::Read,
            )?;
            s.cmd_release_image_to_external(
                adapter,
                command_buffer_id,
                target_image_id,
                TransferAccess::Write,
            )?;
            Ok(())
        })
    }

    /// Convert one authoritative primary into an OPTIMAL BGRA scratch image,
    /// then copy that exact byte layout into the adapter-owned LINEAR scanout.
    ///
    /// Keeping the conversion destination OPTIMAL avoids assuming that the
    /// physical device supports `VK_FORMAT_FEATURE_BLIT_DST_BIT` for a LINEAR
    /// external image. The final copy is format-identical BGRA-to-BGRA.
    pub(super) fn record_reusable_converted_image_copy(
        &mut self,
        adapter: &AdapterContext,
        source_image_id: VkImageId,
        conversion_image_id: VkImageId,
        target_image_id: VkImageId,
        width: u32,
        height: u32,
    ) -> Result<(VkCommandPoolId, VkCommandBufferId), VirtioError> {
        // The three `== 0` arms are unrepresentable now; the distinctness and
        // extent arms are the ones that still carry information.
        if source_image_id == conversion_image_id
            || conversion_image_id == target_image_id
            || source_image_id == target_image_id
            || width == 0
            || height == 0
        {
            return Err(VirtioError::DeviceError);
        }

        self.record_reusable(adapter, |s, command_buffer_id| {
            s.cmd_acquire_image_from_external(
                adapter,
                command_buffer_id,
                source_image_id,
                TransferAccess::Read,
            )?;
            s.cmd_internal_image_barrier(
                adapter,
                command_buffer_id,
                conversion_image_id,
                ACCESS_TRANSFER_READ | ACCESS_TRANSFER_WRITE,
                ACCESS_TRANSFER_WRITE,
            )?;
            s.cmd_acquire_image_from_external(
                adapter,
                command_buffer_id,
                target_image_id,
                TransferAccess::Write,
            )?;
            s.cmd_blit_image(
                adapter,
                command_buffer_id,
                source_image_id,
                conversion_image_id,
                width,
                height,
            )?;
            s.cmd_internal_image_barrier(
                adapter,
                command_buffer_id,
                conversion_image_id,
                ACCESS_TRANSFER_WRITE,
                ACCESS_TRANSFER_READ,
            )?;
            s.cmd_copy_image(
                adapter,
                command_buffer_id,
                conversion_image_id,
                target_image_id,
                width,
                height,
            )?;
            s.cmd_release_image_to_external(
                adapter,
                command_buffer_id,
                source_image_id,
                TransferAccess::Read,
            )?;
            s.cmd_release_image_to_external(
                adapter,
                command_buffer_id,
                target_image_id,
                TransferAccess::Write,
            )?;
            Ok(())
        })
    }

    /// Record one reusable full-surface Present BLT from an imported OPTIMAL
    /// source image into the pitched buffer backing a KMD standard allocation.
    pub(super) fn record_reusable_present_blt(
        &mut self,
        adapter: &AdapterContext,
        source_image_id: VkImageId,
        destination_buffer_id: VkBufferId,
        destination_size: u64,
        width: u32,
        height: u32,
        pitch: u32,
        bytes_per_pixel: u32,
    ) -> Result<(VkCommandPoolId, VkCommandBufferId), VirtioError> {
        // The two handle `== 0` arms are unrepresentable now.
        if destination_size == 0
            || width == 0
            || height == 0
            || bytes_per_pixel == 0
            || pitch < width.saturating_mul(bytes_per_pixel)
            || pitch % bytes_per_pixel != 0
        {
            return Err(VirtioError::DeviceError);
        }

        self.record_reusable(adapter, |s, command_buffer_id| {
            s.cmd_acquire_image_from_external(
                adapter,
                command_buffer_id,
                source_image_id,
                TransferAccess::Read,
            )?;
            s.cmd_acquire_buffer_from_external(
                adapter,
                command_buffer_id,
                destination_buffer_id,
                destination_size,
            )?;
            s.cmd_copy_image_to_buffer(
                adapter,
                command_buffer_id,
                source_image_id,
                destination_buffer_id,
                width,
                height,
                pitch,
                bytes_per_pixel,
            )?;
            s.cmd_release_image_to_external(
                adapter,
                command_buffer_id,
                source_image_id,
                TransferAccess::Read,
            )?;
            s.cmd_release_buffer_to_external(
                adapter,
                command_buffer_id,
                destination_buffer_id,
                destination_size,
            )?;
            Ok(())
        })
    }

    /// Convert the authoritative source into a KMD-owned scratch image carrying
    /// the exact destination format, then copy those bytes into Windows'
    /// redirected standard allocation.
    pub(super) fn record_reusable_converted_present_blt(
        &mut self,
        adapter: &AdapterContext,
        source_image_id: VkImageId,
        conversion_image_id: VkImageId,
        destination_buffer_id: VkBufferId,
        destination_size: u64,
        width: u32,
        height: u32,
        pitch: u32,
        bytes_per_pixel: u32,
    ) -> Result<(VkCommandPoolId, VkCommandBufferId), VirtioError> {
        // The three handle `== 0` arms are unrepresentable now.
        if destination_size == 0
            || width == 0
            || height == 0
            || bytes_per_pixel == 0
            || pitch < width.saturating_mul(bytes_per_pixel)
            || pitch % bytes_per_pixel != 0
        {
            return Err(VirtioError::DeviceError);
        }

        self.record_reusable(adapter, |s, command_buffer_id| {
            s.cmd_acquire_image_from_external(
                adapter,
                command_buffer_id,
                source_image_id,
                TransferAccess::Read,
            )?;
            s.cmd_internal_image_barrier(
                adapter,
                command_buffer_id,
                conversion_image_id,
                ACCESS_TRANSFER_READ | ACCESS_TRANSFER_WRITE,
                ACCESS_TRANSFER_WRITE,
            )?;
            s.cmd_acquire_buffer_from_external(
                adapter,
                command_buffer_id,
                destination_buffer_id,
                destination_size,
            )?;
            s.cmd_blit_image(
                adapter,
                command_buffer_id,
                source_image_id,
                conversion_image_id,
                width,
                height,
            )?;
            s.cmd_internal_image_barrier(
                adapter,
                command_buffer_id,
                conversion_image_id,
                ACCESS_TRANSFER_WRITE,
                ACCESS_TRANSFER_READ,
            )?;
            s.cmd_copy_image_to_buffer(
                adapter,
                command_buffer_id,
                conversion_image_id,
                destination_buffer_id,
                width,
                height,
                pitch,
                bytes_per_pixel,
            )?;
            s.cmd_release_image_to_external(
                adapter,
                command_buffer_id,
                source_image_id,
                TransferAccess::Read,
            )?;
            s.cmd_release_buffer_to_external(
                adapter,
                command_buffer_id,
                destination_buffer_id,
                destination_size,
            )?;
            Ok(())
        })
    }

    /// Copy one complete, same-sized Present source into dxgkrnl's exact
    /// destination allocation, converting across the supported D3D11
    /// swapchain-format set when the Vulkan storage formats differ.
    ///
    /// Setup is cached by exact resource descriptors. The steady-state path
    /// only encodes one vkQueueSubmit and performs one nonblocking ring-1
    /// enqueue; no sleep or synchronous ctrl round-trip occurs per frame.
    pub fn prepare_present_blt(
        &mut self,
        adapter: &AdapterContext,
        source: OptimalPresentImageDesc,
        destination: PresentDestinationDesc,
    ) -> Result<PreparedPresentBltSubmission, VirtioError> {
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
                (Some(self.ensure_present_buffer(desc)?), None)
            }
            PresentDestinationDesc::OptimalImage(desc) => {
                (None, Some(self.ensure_present_image(adapter, desc)?))
            }
        };
        let blt_index = match existing {
            Some(index) => index,
            None => {
                let mut conversion_image_id = None;
                let mut conversion_memory_id = None;
                let mut conversion_init_pool_id = None;
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
                            conversion_image_id = Some(conversion.0);
                            conversion_memory_id = Some(conversion.1);
                            let command = match self.record_reusable_converted_present_blt(
                                adapter,
                                source_image.image_id,
                                conversion.0,
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
                                    let _ = self.destroy_image_on_ring(adapter, conversion.0);
                                    let _ = self.free_memory_object(adapter, conversion.1);
                                    return Err(error);
                                }
                            };
                            conversion_init_pool_id = Some(
                                self.initialize_present_conversion_image(adapter, conversion.0)?,
                            );
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
        Ok(PreparedPresentBltSubmission {
            blt_index,
            command_buffer_id,
            destination,
        })
    }

    /// Preserve the legacy Present path for an untyped source. It uses the
    /// same prepared cache record as WindowedBlt snapshots, but submits through
    /// the ordinary ring-1 path because no deferred token/reader transaction
    /// exists for it.
    pub fn submit_present_blt(
        &mut self,
        adapter: &AdapterContext,
        source: OptimalPresentImageDesc,
        destination: PresentDestinationDesc,
    ) -> Result<u64, VirtioError> {
        let prepared = self.prepare_present_blt(adapter, source, destination)?;
        self.validate_prepared_present_blt(prepared)?;
        let submit = self.encode_command_buffer_submit(prepared.command_buffer_id);
        let present_buffer_write = match prepared.destination {
            PresentDestinationDesc::StandardBuffer(destination) => Some(destination.resource_id),
            PresentDestinationDesc::OptimalImage(_) => None,
        };
        let fence_id = ctrl::submit_venus_async_present(
            self.passive(),
            adapter,
            self.ctx_id(),
            submit.as_slice()?,
            present_buffer_write,
        )?;
        self.note_prepared_present_blt_submit(adapter, prepared, fence_id);
        Ok(fence_id)
    }

    fn validate_prepared_present_blt(
        &self,
        prepared: PreparedPresentBltSubmission,
    ) -> Result<(), VirtioError> {
        let Some(blt) = self.present_blits.get(prepared.blt_index) else {
            return Err(VirtioError::DeviceError);
        };
        if blt.command_buffer_id != prepared.command_buffer_id
            || blt.destination_resource_id != prepared.destination.resource_id()
        {
            return Err(VirtioError::DeviceError);
        }
        Ok(())
    }

    fn note_prepared_present_blt_submit(
        &mut self,
        adapter: &AdapterContext,
        prepared: PreparedPresentBltSubmission,
        fence_id: u64,
    ) {
        let blt = &mut self.present_blits[prepared.blt_index];
        blt.last_wire_fence_id = fence_id;
        blt.submit_count = blt.submit_count.saturating_add(1);
        let run_probe = if adapter.present_probe()
            && blt.submit_count >= PRESENT_PROBE_AFTER_SUBMITS
            && !blt.probe_done
        {
            // Claim the one-shot before doing any fallible work. Even a failed
            // diagnostic can therefore never recur on the Present path.
            blt.probe_done = true;
            true
        } else {
            false
        };
        if run_probe {
            if let PresentDestinationDesc::StandardBuffer(destination) = prepared.destination {
                self.probe_pending = Some((destination, fence_id));
                adapter
                    .probe_pending
                    .store(1, core::sync::atomic::Ordering::Release);
            }
        }
    }

    /// Submit a cache-prepared BLT only after the exact Present token was
    /// admitted by SubmitCommand. This must never be called from Present.
    pub fn submit_prepared_present_blt(
        &mut self,
        adapter: &AdapterContext,
        prepared: PreparedPresentBltSubmission,
        token: u64,
        stream_boundary: u64,
    ) -> Result<u64, VirtioError> {
        self.validate_prepared_present_blt(prepared)?;
        let command_buffer_id = prepared.command_buffer_id;
        let submit = self.encode_command_buffer_submit(command_buffer_id);
        let fence_id = ctrl::submit_venus_async_windowed_blt(
            self.passive(),
            adapter,
            self.ctx_id(),
            submit.as_slice()?,
            token,
            stream_boundary,
        )?;
        self.note_prepared_present_blt_submit(adapter, prepared, fence_id);
        Ok(fence_id)
    }

    /// Drain and destroy the cached app/DWM Present BLT records belonging to one
    /// resource.
    ///
    /// Allocation teardown calls this before a backing resource can disappear.
    ///
    /// It used to drain EVERY cached entry regardless of `resource_id`, despite
    /// the entry gate below testing only for `resource_id` — so tearing down A
    /// destroyed B's objects too, and a failure partway through left B alive
    /// with its cache record gone. Scoping makes the gate and the fast path
    /// honest: what this function touches is now exactly what it says.
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
                match ctrl::wait_fence(
                    self.passive(),
                    adapter,
                    blt.last_wire_fence_id,
                    5_000_000_000,
                ) {
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

        // Blits FIRST, and scoped by BOTH ends. A blit's command buffer is
        // baked against its source image and its destination image/buffer, so
        // releasing either end must take the blit with it — the set is derived
        // from the resources actually being released, not from a single-sided
        // compare on the blit. Several swapchain sources may share one DWM
        // destination, so a one-sided test would leave a command buffer baked
        // against a destroyed image.
        let mut i = 0;
        while i < self.present_blits.len() {
            let touches = self.present_blits[i].source_resource_id == resource_id
                || self.present_blits[i].destination_resource_id == resource_id;
            if !touches {
                i += 1;
                continue;
            }
            // swap_remove puts the last element in slot i, so do NOT advance —
            // the element that just moved here has not been tested yet.
            let blt = self.present_blits.swap_remove(i);
            if let Err((blt, error)) = blt.release(self, adapter) {
                // Reinsert: MAX_PRESENT_BLITS capacity is preallocated, so this
                // push cannot allocate under any lock. The record must go back
                // because its host objects may still exist — the whole point of
                // the consuming signature is that "forgot the record while the
                // object survives" has nowhere to be written.
                self.present_blits.push(blt);
                crate::diag::record_named_bytes(b"PBTdErr", resource_id);
                return Err(error);
            }
        }
        let mut i = 0;
        while i < self.present_images.len() {
            if self.present_images[i].desc.resource_id != resource_id {
                i += 1;
                continue;
            }
            let image = self.present_images.swap_remove(i);
            if let Err((image, error)) = image.release(self, adapter) {
                self.present_images.push(image);
                crate::diag::record_named_bytes(b"PBTdErr", resource_id);
                return Err(error);
            }
        }
        let mut i = 0;
        while i < self.present_buffers.len() {
            if self.present_buffers[i].desc.resource_id != resource_id {
                i += 1;
                continue;
            }
            let buffer = self.present_buffers.swap_remove(i);
            if let Err((buffer, error)) = buffer.release(self, adapter) {
                self.present_buffers.push(buffer);
                crate::diag::record_named_bytes(b"PBTdErr", resource_id);
                return Err(error);
            }
        }
        Ok(())
    }
}
