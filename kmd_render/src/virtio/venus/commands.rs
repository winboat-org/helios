//! The Vulkan object encoders: create/destroy/bind for images, buffers,
//! memory, command pools, command buffers and fences, the barrier recorders,
//! and the memory-type choosers that pick a heap for each.
//!
//! Moved verbatim out of `virtio/venus.rs` by T8/R1104.

use super::ring::*;
use super::*;

/// Which side of a transfer an EXTERNAL ownership transition serves.
///
/// R1003. The acquire and release argument lists differed by transposing two
/// pairs of adjacent `u32`s, so both spellings compiled and type-checked and
/// the wrong one is a host-side queue-family ownership violation rather than an
/// error. This is the only thing that actually varies between them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TransferAccess {
    Read,
    Write,
}

impl TransferAccess {
    pub(super) const fn mask(self) -> u32 {
        match self {
            TransferAccess::Read => ACCESS_TRANSFER_READ,
            TransferAccess::Write => ACCESS_TRANSFER_WRITE,
        }
    }
}

/// One `VkImageMemoryBarrier`, by field name rather than by position.
pub(super) struct ImageBarrier {
    image: VkImageId,
    src_stage: u32,
    dst_stage: u32,
    src_access: u32,
    dst_access: u32,
    old_layout: u32,
    new_layout: u32,
    src_queue_family: u32,
    dst_queue_family: u32,
}

impl ImageBarrier {
    /// Take ownership of an image FROM the external (host/DXVK) queue family,
    /// at the top of the pipe, for a transfer read or write.
    pub(super) const fn acquire_from_external(image: VkImageId, access: TransferAccess) -> Self {
        Self {
            image,
            src_stage: PIPELINE_STAGE_TOP_OF_PIPE,
            dst_stage: PIPELINE_STAGE_TRANSFER,
            src_access: 0,
            dst_access: access.mask(),
            old_layout: IMAGE_LAYOUT_GENERAL,
            new_layout: IMAGE_LAYOUT_GENERAL,
            src_queue_family: QUEUE_FAMILY_EXTERNAL,
            dst_queue_family: 0,
        }
    }

    /// Hand ownership back TO the external queue family at the bottom of the
    /// pipe, after a transfer read or write. The exact mirror of
    /// [`Self::acquire_from_external`]: stages, accesses and queue families all
    /// swap, which is why writing it out by hand at ten sites was the hazard.
    pub(super) const fn release_to_external(image: VkImageId, access: TransferAccess) -> Self {
        Self {
            image,
            src_stage: PIPELINE_STAGE_TRANSFER,
            dst_stage: PIPELINE_STAGE_BOTTOM_OF_PIPE,
            src_access: access.mask(),
            dst_access: 0,
            old_layout: IMAGE_LAYOUT_GENERAL,
            new_layout: IMAGE_LAYOUT_GENERAL,
            src_queue_family: 0,
            dst_queue_family: QUEUE_FAMILY_EXTERNAL,
        }
    }

    /// A transfer-to-transfer transition with NO ownership change, for the
    /// scratch conversion image. `QUEUE_FAMILY_IGNORED` on both sides.
    pub(super) const fn internal(image: VkImageId, src_access: u32, dst_access: u32) -> Self {
        Self {
            image,
            src_stage: PIPELINE_STAGE_TRANSFER,
            dst_stage: PIPELINE_STAGE_TRANSFER,
            src_access,
            dst_access,
            old_layout: IMAGE_LAYOUT_GENERAL,
            new_layout: IMAGE_LAYOUT_GENERAL,
            src_queue_family: QUEUE_FAMILY_IGNORED,
            dst_queue_family: QUEUE_FAMILY_IGNORED,
        }
    }

    /// The one-shot transition of a freshly created image into GENERAL.
    /// `old_layout` is whichever layout it was created with -- UNDEFINED for an
    /// internal conversion image, PREINITIALIZED for the LINEAR scan-out image.
    pub(super) const fn initial_to_general(image: VkImageId, old_layout: u32) -> Self {
        Self {
            image,
            src_stage: PIPELINE_STAGE_TOP_OF_PIPE,
            dst_stage: PIPELINE_STAGE_TRANSFER,
            src_access: 0,
            dst_access: ACCESS_TRANSFER_WRITE,
            old_layout,
            new_layout: IMAGE_LAYOUT_GENERAL,
            src_queue_family: QUEUE_FAMILY_IGNORED,
            dst_queue_family: QUEUE_FAMILY_IGNORED,
        }
    }
}

/// One `VkBufferMemoryBarrier`, by field name.
///
/// The steady-state direction is a transfer write followed by an external
/// reader; the initial-release constructor establishes ownership before that
/// cycle begins.
pub(super) struct BufferBarrier {
    buffer: VkBufferId,
    size: u64,
    src_stage: u32,
    dst_stage: u32,
    src_access: u32,
    dst_access: u32,
    src_queue_family: u32,
    dst_queue_family: u32,
}

impl BufferBarrier {
    /// First use of a newly-created exclusive buffer implicitly acquires it for
    /// family 0. Release that initial ownership before any reusable Present
    /// command attempts the matching EXTERNAL -> family-0 acquire.
    pub(super) const fn initial_release_to_external(buffer: VkBufferId, size: u64) -> Self {
        Self {
            buffer,
            size,
            src_stage: PIPELINE_STAGE_TOP_OF_PIPE,
            dst_stage: PIPELINE_STAGE_BOTTOM_OF_PIPE,
            src_access: 0,
            dst_access: 0,
            src_queue_family: 0,
            dst_queue_family: QUEUE_FAMILY_EXTERNAL,
        }
    }

    pub(super) const fn acquire_from_external(buffer: VkBufferId, size: u64) -> Self {
        Self {
            buffer,
            size,
            src_stage: PIPELINE_STAGE_TOP_OF_PIPE,
            dst_stage: PIPELINE_STAGE_TRANSFER,
            src_access: 0,
            dst_access: ACCESS_TRANSFER_WRITE,
            src_queue_family: QUEUE_FAMILY_EXTERNAL,
            dst_queue_family: 0,
        }
    }

    pub(super) const fn release_to_external(buffer: VkBufferId, size: u64) -> Self {
        Self {
            buffer,
            size,
            src_stage: PIPELINE_STAGE_TRANSFER,
            dst_stage: PIPELINE_STAGE_BOTTOM_OF_PIPE,
            src_access: ACCESS_TRANSFER_WRITE,
            dst_access: 0,
            src_queue_family: 0,
            dst_queue_family: QUEUE_FAMILY_EXTERNAL,
        }
    }

    /// Make transfer writes available to CPU reads after the submission fence
    /// is waited. Queue-family release barriers ignore their destination
    /// synchronization scope, so this must be a separate ordinary barrier.
    pub(super) const fn transfer_write_to_host_read(buffer: VkBufferId, size: u64) -> Self {
        Self {
            buffer,
            size,
            src_stage: PIPELINE_STAGE_TRANSFER,
            dst_stage: PIPELINE_STAGE_HOST,
            src_access: ACCESS_TRANSFER_WRITE,
            dst_access: ACCESS_HOST_READ,
            src_queue_family: QUEUE_FAMILY_IGNORED,
            dst_queue_family: QUEUE_FAMILY_IGNORED,
        }
    }
}

impl VenusClient {
    fn allocate_memory_object_with_type(
        &mut self,
        adapter: &AdapterContext,
        size: u64,
        shareable: bool,
        memory_type_index: u32,
    ) -> Result<VkDeviceMemoryId, VirtioError> {
        let memory_id = self.new_memory_id();
        let w = encode_memory_allocate(
            self.device_id.into(),
            memory_id.into(),
            &MemoryAllocateSpec {
                pnext: if shareable {
                    MemoryPNext::Export {
                        handle_type: EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF,
                    }
                } else {
                    MemoryPNext::None
                },
                size,
                memory_type_index,
            },
        );
        self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_ALLOCATE_MEMORY)
                .mismatch(0x00F6)
                .refuse_result(0x00F7),
        )?;
        Ok(memory_id)
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
        let memory_id = self.allocate_memory_object_with_type(
            adapter,
            size,
            shareable,
            self.memory_type_index.0,
        )?;

        let mut flags = 0;
        if mappable {
            flags |= VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE;
        }
        if shareable {
            flags |= VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE;
        }
        let res_id = match ctrl::resource_create_blob(
            self.passive(),
            adapter,
            self.ctx_id(),
            VIRTIO_GPU_BLOB_MEM_HOST3D,
            flags,
            // The VkDeviceMemory handle IS the virtio blob_id for a
            // KMD-created blob. That coupling is deliberate, so the raw value
            // crosses the transport boundary here.
            memory_id.get(),
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
            memory_type_index: self.memory_type_index.0,
            prepared_present_buffer: None,
            initial_release_pool_id: None,
            initial_release_fence_id: None,
        });
        Ok(HostVisibleBlob {
            blob_id: memory_id.get(),
            res_id,
            gpa: 0,
            size,
        })
    }

    /// Select memory for a KMD standard destination from the requirements of
    /// the exact Vulkan buffer that will consume it, then retain that buffer for
    /// Present.  This avoids both the old device-global heap guess and a second
    /// create/query cycle on the first frame.
    pub fn allocate_present_buffer_blob(
        &mut self,
        adapter: &AdapterContext,
        requested_size: u64,
    ) -> Result<(HostVisibleBlob, u32), VirtioError> {
        if self.owned_memory_blobs.len() >= MAX_OWNED_MEMORY_BLOBS {
            return Err(VirtioError::OutOfMemory);
        }
        let requested_size = round_up_page(requested_size.max(4096));
        if requested_size == 0 {
            return Err(VirtioError::OutOfMemory);
        }

        let mut buffer_id = self.create_present_destination_buffer(adapter, requested_size)?;
        let (mut required_size, mut alignment, mut memory_type_bits, mut dedicated_hint) =
            match self.buffer_memory_requirements(adapter, buffer_id) {
                Ok(requirements) => requirements,
                Err(e) => {
                    let _ = self.destroy_buffer_on_ring(adapter, buffer_id);
                    return Err(e);
                }
            };
        let align = alignment.max(4096);
        let base = requested_size.max(required_size);
        let remainder = base % align;
        let allocation_size = match if remainder == 0 {
            Some(base)
        } else {
            base.checked_add(align - remainder)
        } {
            Some(size) => size,
            None => {
                let _ = self.destroy_buffer_on_ring(adapter, buffer_id);
                return Err(VirtioError::OutOfMemory);
            }
        };

        // VkMemoryRequirements::size may round a buffer up beyond its logical
        // size.  Recreate once at that aligned size so the buffer range, the
        // VkDeviceMemory allocation, the HOST3D blob and the identity exported
        // to the UMD all describe one exact byte extent.
        if allocation_size != requested_size {
            if let Err(e) = self.destroy_buffer_on_ring(adapter, buffer_id) {
                return Err(e);
            }
            buffer_id = self.create_present_destination_buffer(adapter, allocation_size)?;
            (required_size, alignment, memory_type_bits, dedicated_hint) =
                match self.buffer_memory_requirements(adapter, buffer_id) {
                    Ok(requirements) => requirements,
                    Err(e) => {
                        let _ = self.destroy_buffer_on_ring(adapter, buffer_id);
                        return Err(e);
                    }
                };
            if required_size > allocation_size || alignment == 0 || allocation_size % alignment != 0
            {
                let _ = self.destroy_buffer_on_ring(adapter, buffer_id);
                return Err(VirtioError::DeviceError);
            }
        }

        let choice = self
            .choose_cached_host_visible_memory_type(memory_type_bits)
            .ok_or(VirtioError::DeviceError);
        let memory_type_index = match choice {
            Ok(choice) => Self::accept_memory_type(choice),
            Err(e) => {
                let _ = self.destroy_buffer_on_ring(adapter, buffer_id);
                return Err(e);
            }
        };
        crate::diag::record_named_bytes(b"PBCBt", memory_type_bits);
        crate::diag::record_named_bytes(b"PBCMt", memory_type_index);
        crate::diag::record_named_bytes(
            b"PBCMf",
            self.memory_type_flags[memory_type_index as usize],
        );
        crate::diag::record_named_bytes(b"PBCDh", u32::from(dedicated_hint));
        crate::diag::record_named_bytes(b"PBCMd", 1);

        let memory_id = match self.allocate_present_buffer_memory(
            adapter,
            buffer_id,
            allocation_size,
            memory_type_index,
        ) {
            Ok(memory_id) => memory_id,
            Err(e) => {
                let _ = self.destroy_buffer_on_ring(adapter, buffer_id);
                return Err(e);
            }
        };
        if let Err(e) = self.bind_buffer_memory(adapter, buffer_id, memory_id) {
            let _ = self.destroy_buffer_on_ring(adapter, buffer_id);
            let _ = self.free_memory_object(adapter, memory_id);
            return Err(e);
        }

        // A reusable Present starts with EXTERNAL -> family-0 acquire. Give the
        // newly-created exclusive buffer a real initial family-0 -> EXTERNAL
        // release first; otherwise the first acquire has no matching release.
        let (initial_pool_id, initial_command_buffer_id) =
            match self.record_initial_present_buffer_release(adapter, buffer_id, allocation_size) {
                Ok(setup) => setup,
                Err(e) => {
                    let _ = self.destroy_buffer_on_ring(adapter, buffer_id);
                    let _ = self.free_memory_object(adapter, memory_id);
                    return Err(e);
                }
            };
        let initial_fence_id = match self.create_fence(adapter) {
            Ok(fence) => fence,
            Err(e) => {
                let _ = self.destroy_command_pool(adapter, initial_pool_id);
                let _ = self.destroy_buffer_on_ring(adapter, buffer_id);
                let _ = self.free_memory_object(adapter, memory_id);
                return Err(e);
            }
        };

        let res_id = match ctrl::resource_create_blob(
            self.passive(),
            adapter,
            self.ctx_id(),
            VIRTIO_GPU_BLOB_MEM_HOST3D,
            VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE | VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE,
            memory_id.get(),
            allocation_size,
        ) {
            Ok(resource_id) => resource_id,
            Err(e) => {
                let _ = self.destroy_fence(adapter, initial_fence_id);
                let _ = self.destroy_command_pool(adapter, initial_pool_id);
                let _ = self.destroy_buffer_on_ring(adapter, buffer_id);
                let _ = self.free_memory_object(adapter, memory_id);
                return Err(e);
            }
        };
        let _ = adapter.with_virtio(|v| v.note_blob_size(res_id, allocation_size));
        self.owned_memory_blobs.push(OwnedMemoryBlob {
            resource_id: res_id,
            memory_id,
            allocation_size,
            memory_type_index,
            prepared_present_buffer: Some(buffer_id),
            // Publish every possibly-live setup object before queue submission.
            // A transport failure can make submission ambiguous, so destroying
            // an untracked pool/buffer in that arm would race host execution.
            initial_release_pool_id: Some(initial_pool_id),
            initial_release_fence_id: Some(initial_fence_id),
        });
        let owned_index = self.owned_memory_blobs.len() - 1;

        self.queue_submit_command_buffer(
            adapter,
            initial_command_buffer_id,
            Some(initial_fence_id),
        )?;
        self.wait_for_fence(adapter, initial_fence_id)?;
        self.destroy_fence(adapter, initial_fence_id)?;
        self.owned_memory_blobs[owned_index].initial_release_fence_id = None;
        self.destroy_command_pool(adapter, initial_pool_id)?;
        self.owned_memory_blobs[owned_index].initial_release_pool_id = None;

        // Publish the cross-device ownership state only after the one-time
        // family-0 -> EXTERNAL release has reached its fence. From this point a
        // DWM consumer claim and every KMD writer start serialize under the
        // adapter's virtio lock.
        let registration = adapter
            .with_virtio(|v| v.register_present_buffer(res_id))
            .map_err(|_| VirtioError::DeviceError)
            .and_then(|result| result);
        if let Err(error) = registration {
            // The initial release fence completed, so no Vulkan work can still
            // reference this unpublished object. Reclaim the blob/resource and
            // then the dedicated buffer+memory in normal teardown order. If a
            // transport operation is ambiguous, retain the owned record for
            // Venus-context teardown rather than manufacture a UAF.
            let first_teardown = adapter
                .with_virtio(|v| v.take_live_resource(res_id))
                .unwrap_or(false);
            if first_teardown
                && ctrl::ctx_detach_resource(self.passive(), adapter, self.ctx_id(), res_id).is_ok()
                && ctrl::resource_unref(self.passive(), adapter, res_id).is_ok()
            {
                let _ = self.free_memory_blob(adapter, memory_id.get());
            }
            return Err(error);
        }

        Ok((
            HostVisibleBlob {
                blob_id: memory_id.get(),
                res_id,
                gpa: 0,
                size: allocation_size,
            },
            memory_type_index,
        ))
    }

    /// See [`helios_kmd_logic::choose_host_visible_memory_type`] — the rule is a
    /// pure function of `memory_type_flags`/`memory_type_count`, so it lives
    /// where it can be host-tested.
    pub(super) fn choose_host_visible_memory_type(
        &self,
        memory_type_bits: u32,
    ) -> Option<MemoryTypeChoice> {
        helios_kmd_logic::choose_host_visible_memory_type(
            &self.memory_type_flags,
            self.memory_type_count,
            memory_type_bits,
        )
    }

    /// See [`helios_kmd_logic::choose_cached_host_visible_memory_type`].
    pub(super) fn choose_cached_host_visible_memory_type(
        &self,
        memory_type_bits: u32,
    ) -> Option<MemoryTypeChoice> {
        helios_kmd_logic::choose_cached_host_visible_memory_type(
            &self.memory_type_flags,
            self.memory_type_count,
            memory_type_bits,
        )
    }

    /// See [`helios_kmd_logic::choose_device_local_memory_type`].
    pub(super) fn choose_device_local_memory_type(
        &self,
        memory_type_bits: u32,
    ) -> Option<MemoryTypeChoice> {
        helios_kmd_logic::choose_device_local_memory_type(
            &self.memory_type_flags,
            self.memory_type_count,
            memory_type_bits,
        )
    }

    /// Accept a memory-type choice, naming a downgrade in the registry.
    ///
    /// Every selector call site goes through here, so "we asked for
    /// DEVICE_LOCAL and settled for whatever was allowed" can no longer happen
    /// without a breadcrumb. `VnMtDown` records the index that was taken.
    pub(super) fn accept_memory_type(choice: MemoryTypeChoice) -> u32 {
        if let MemoryTypeChoice::Downgraded(index) = choice {
            crate::diag::record_named_bytes(b"VnMtDown", index);
        }
        choice.index()
    }

    pub(super) fn create_linear_scanout_image(
        &mut self,
        adapter: &AdapterContext,
        width: u32,
        height: u32,
    ) -> Result<VkImageId, VirtioError> {
        let image_id = self.new_image_id();
        let w = encode_image_create(
            self.device_id.into(),
            image_id.into(),
            &ImageCreateSpec {
                // VkExternalMemoryImageCreateInfo only. This matches the Linux
                // KMS probe that reached QEMU's egl-headless dmabuf import on
                // NVIDIA.
                pnext: ImagePNext::ExternalMemory {
                    handle_type: EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF,
                },
                flags: 0,
                format: FORMAT_B8G8R8A8_UNORM,
                width,
                height,
                tiling: IMAGE_TILING_LINEAR,
                usage: IMAGE_USAGE_TRANSFER_SRC | IMAGE_USAGE_TRANSFER_DST,
                initial_layout: IMAGE_LAYOUT_PREINITIALIZED,
            },
        );
        // The raw VkResult of the LINEAR external-DMA_BUF image create is the
        // most likely NVIDIA-venus rejection point for the CachyOS shape, which
        // is why `SdgLImg` carries it verbatim.
        let mut r = self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_CREATE_IMAGE)
                .mismatch(0x0120)
                .mismatch_marks(b"SdgLImg")
                .refuse_result(0x0121)
                .result_marks(b"SdgLImg"),
        )?;
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
    pub(super) fn create_optimal_present_image_alias(
        &mut self,
        adapter: &AdapterContext,
        width: u32,
        height: u32,
        ddi_bind_flags: u32,
        dxgi_format: u32,
        transport: OptimalImageTransport,
    ) -> Result<VkImageId, VirtioError> {
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

        let image_id = self.new_image_id();
        let w = encode_image_create(
            self.device_id.into(),
            image_id.into(),
            &ImageCreateSpec {
                // Imported ordinary UMD resources use OPAQUE_FD. KMD-created
                // GDI textures use DMA_BUF because virglrenderer's
                // render-server proxy can carry DMA_BUF, but cannot attach an
                // OPAQUE_FD to another context.
                pnext: ImagePNext::ExternalMemory {
                    handle_type: transport.handle_type(),
                },
                // Shared color images retain MUTABLE_FORMAT but intentionally
                // have no format-list pNext (DXVK suppresses that list to
                // disable per-image compression metadata which cannot survive
                // cross-device imports).
                flags: IMAGE_CREATE_MUTABLE_FORMAT,
                format: vk_format,
                width,
                height,
                tiling: IMAGE_TILING_OPTIMAL,
                usage,
                initial_layout: IMAGE_LAYOUT_UNDEFINED,
            },
        );
        let mut r = self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_CREATE_IMAGE)
                .mismatch(0x0130)
                .refuse_result(0x0131)
                .result_marks(b"CpImgVr"),
        )?;
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
    pub(super) fn create_present_conversion_image(
        &mut self,
        adapter: &AdapterContext,
        width: u32,
        height: u32,
        vk_format: u32,
    ) -> Result<VkImageId, VirtioError> {
        if width == 0 || height == 0 {
            return Err(VirtioError::DeviceError);
        }
        let image_id = self.new_image_id();
        let w = encode_image_create(
            self.device_id.into(),
            image_id.into(),
            &ImageCreateSpec {
                // Internal image, no external-memory contract.
                pnext: ImagePNext::None,
                flags: 0,
                format: vk_format,
                width,
                height,
                tiling: IMAGE_TILING_OPTIMAL,
                usage: IMAGE_USAGE_TRANSFER_SRC | IMAGE_USAGE_TRANSFER_DST,
                initial_layout: IMAGE_LAYOUT_UNDEFINED,
            },
        );
        let mut r = self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_CREATE_IMAGE).refuse_result_undiagnosed(),
        )?;
        if r.read_u64()? == 0 || r.read_u64()? == 0 {
            return Err(VirtioError::DeviceError);
        }
        Ok(image_id)
    }

    /// Allocate non-exported memory dedicated to an internal conversion image.
    pub(super) fn allocate_present_conversion_memory(
        &mut self,
        adapter: &AdapterContext,
        image_id: VkImageId,
        size: u64,
        memory_type_index: u32,
    ) -> Result<VkDeviceMemoryId, VirtioError> {
        let memory_id = self.new_memory_id();
        let w = encode_memory_allocate(
            self.device_id.into(),
            memory_id.into(),
            &MemoryAllocateSpec {
                pnext: MemoryPNext::Dedicated {
                    image: image_id.into(),
                },
                size,
                memory_type_index,
            },
        );
        self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_ALLOCATE_MEMORY).refuse_result_undiagnosed(),
        )?;
        Ok(memory_id)
    }

    /// Transition a newly-created conversion image to GENERAL exactly once.
    /// Queue ordering makes every later reusable Present command execute after
    /// this setup submission; retaining the pool preserves command lifetime.
    pub(super) fn initialize_present_conversion_image(
        &mut self,
        adapter: &AdapterContext,
        image_id: VkImageId,
    ) -> Result<VkCommandPoolId, VirtioError> {
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
            self.cmd_initial_image_transition(
                adapter,
                command_buffer_id,
                image_id,
                IMAGE_LAYOUT_UNDEFINED,
            )?;
            self.end_command_buffer(adapter, command_buffer_id)
        })();
        if let Err(e) = record_result {
            let _ = self.destroy_command_pool(adapter, pool_id);
            return Err(e);
        }
        // On an ambiguous submission failure, leave the pool alive for Venus
        // context teardown. Destroying it could free an in-flight command.
        self.queue_submit_command_buffer(adapter, command_buffer_id, None)?;
        Ok(pool_id)
    }

    /// Record, but do not submit, the initial ownership release for a freshly
    /// bound KMD Present buffer. The caller publishes the setup lifetime before
    /// submission and waits its fence before exposing the blob to Windows.
    pub(super) fn record_initial_present_buffer_release(
        &mut self,
        adapter: &AdapterContext,
        buffer_id: VkBufferId,
        size: u64,
    ) -> Result<(VkCommandPoolId, VkCommandBufferId), VirtioError> {
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
            self.cmd_buffer_barrier(
                adapter,
                command_buffer_id,
                BufferBarrier::initial_release_to_external(buffer_id, size),
            )?;
            self.end_command_buffer(adapter, command_buffer_id)
        })();
        if let Err(e) = record_result {
            let _ = self.destroy_command_pool(adapter, pool_id);
            return Err(e);
        }
        Ok((pool_id, command_buffer_id))
    }

    pub(super) fn create_bound_present_conversion_image(
        &mut self,
        adapter: &AdapterContext,
        width: u32,
        height: u32,
        format: PresentPixelFormat,
    ) -> Result<(VkImageId, VkDeviceMemoryId), VirtioError> {
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
            Some(choice) => Self::accept_memory_type(choice),
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

    pub(super) fn image_memory_requirements(
        &mut self,
        adapter: &AdapterContext,
        image_id: VkImageId,
    ) -> Result<(u64, u32), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_GET_IMAGE_MEMORY_REQUIREMENTS, CMD_FLAG_GENERATE_REPLY);
        w.handle(self.device_id);
        w.handle(image_id);
        w.count(true);
        // No VkResult in this reply shape: word 1 is the simple-pointer.
        let mut r = self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_GET_IMAGE_MEMORY_REQUIREMENTS).mismatch(0x0104),
        )?;
        if r.read_u64()? == 0 {
            diag(0x0105);
            return Err(VirtioError::DeviceError);
        }
        let size = r.read_u64()?;
        let _alignment = r.read_u64()?;
        let memory_type_bits = r.read_u32()?;
        Ok((size, memory_type_bits))
    }

    /// Create the KMD side of a present-staging buffer.
    ///
    /// DXVK imports the same HOST3D payload into an exactly matching OPAQUE_FD,
    /// SRC|DST, exclusive buffer. Querying this buffer therefore gives the
    /// memory mask for the precise dedicated export/import contract on both
    /// sides, rather than assuming independently shaped buffers accept the same
    /// heap.
    pub(super) fn create_present_destination_buffer(
        &mut self,
        adapter: &AdapterContext,
        size: u64,
    ) -> Result<VkBufferId, VirtioError> {
        let buffer_id = self.new_buffer_id();
        let mut w = Writer::new();
        w.header(CMD_CREATE_BUFFER, CMD_FLAG_GENERATE_REPLY);
        w.handle(self.device_id);
        w.count(true);
        w.i32(ST_BUFFER_CREATE_INFO);
        w.count(true); // pNext: VkExternalMemoryBufferCreateInfo
        w.i32(ST_EXTERNAL_MEMORY_BUFFER_CREATE_INFO);
        w.count(false);
        w.u32(EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD);
        w.u32(0); // VkBufferCreateFlags
        w.u64(size);
        // KMD writes this buffer and the UMD imports the same memory as its
        // transfer source.  Asking for both usages makes the compatibility mask
        // valid for both consumers, not only for the first one created.
        w.u32(BUFFER_USAGE_TRANSFER_SRC | BUFFER_USAGE_TRANSFER_DST);
        w.u32(SHARING_MODE_EXCLUSIVE);
        w.u32(0); // queueFamilyIndexCount
        w.count(false);
        w.count(false); // pAllocator
        w.count(true);
        w.handle(buffer_id);
        let mut r = self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_CREATE_BUFFER)
                .refuse_result_undiagnosed()
                .result_marks(b"PBBufVr"),
        )?;
        if r.read_u64()? == 0 || r.read_u64()? == 0 {
            return Err(VirtioError::DeviceError);
        }
        Ok(buffer_id)
    }

    pub(super) fn buffer_memory_requirements(
        &mut self,
        adapter: &AdapterContext,
        buffer_id: VkBufferId,
    ) -> Result<(u64, u64, u32, bool), VirtioError> {
        let mut w = Writer::new();
        w.header(
            CMD_GET_BUFFER_MEMORY_REQUIREMENTS_2,
            CMD_FLAG_GENERATE_REPLY,
        );
        w.handle(self.device_id);
        // pInfo: VkBufferMemoryRequirementsInfo2.
        w.count(true);
        w.i32(ST_BUFFER_MEMORY_REQUIREMENTS_INFO_2);
        w.count(false);
        w.handle(buffer_id);
        // pMemoryRequirements: VkMemoryRequirements2 with an output
        // VkMemoryDedicatedRequirements chained to it. Partial request
        // encoding carries the structure identities but no output fields.
        w.count(true);
        w.i32(ST_MEMORY_REQUIREMENTS_2);
        w.count(true);
        w.i32(ST_MEMORY_DEDICATED_REQUIREMENTS);
        w.count(false);
        let mut r = self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_GET_BUFFER_MEMORY_REQUIREMENTS_2),
        )?;
        // No VkResult: the reply starts with the output simple-pointer and the
        // complete structure chain requested above.
        if r.read_u64()? == 0 {
            return Err(VirtioError::DeviceError);
        }
        if r.read_u32()? != ST_MEMORY_REQUIREMENTS_2 as u32 || r.read_u64()? == 0 {
            return Err(VirtioError::DeviceError);
        }
        if r.read_u32()? != ST_MEMORY_DEDICATED_REQUIREMENTS as u32 || r.read_u64()? != 0 {
            return Err(VirtioError::DeviceError);
        }
        let prefers_dedicated = r.read_u32()? != 0;
        let requires_dedicated = r.read_u32()? != 0;
        let size = r.read_u64()?;
        let alignment = r.read_u64()?;
        let memory_type_bits = r.read_u32()?;
        if alignment == 0 {
            return Err(VirtioError::DeviceError);
        }
        Ok((
            size,
            alignment,
            memory_type_bits,
            prefers_dedicated || requires_dedicated,
        ))
    }

    fn allocate_present_buffer_memory(
        &mut self,
        adapter: &AdapterContext,
        buffer_id: VkBufferId,
        size: u64,
        memory_type_index: u32,
    ) -> Result<VkDeviceMemoryId, VirtioError> {
        let memory_id = self.new_memory_id();
        let w = encode_memory_allocate(
            self.device_id.into(),
            memory_id.into(),
            &MemoryAllocateSpec {
                // The memory immediately becomes an exported HOST3D blob and
                // DXVK imports that payload with a dedicated-buffer chain.
                // Make both sides explicit and identical regardless of whether
                // the host merely prefers or strictly requires dedication.
                pnext: MemoryPNext::ExportDedicatedBuffer {
                    handle_type: EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD,
                    buffer: buffer_id.into(),
                },
                size,
                memory_type_index,
            },
        );
        self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_ALLOCATE_MEMORY)
                .mismatch(0x00F6)
                .refuse_result(0x00F7),
        )?;
        Ok(memory_id)
    }

    pub(super) fn allocate_dedicated_image_memory(
        &mut self,
        adapter: &AdapterContext,
        image_id: VkImageId,
        size: u64,
        memory_type_index: u32,
    ) -> Result<VkDeviceMemoryId, VirtioError> {
        let memory_id = self.new_memory_id();
        let w = encode_memory_allocate(
            self.device_id.into(),
            memory_id.into(),
            &MemoryAllocateSpec {
                // VkExportMemoryAllocateInfo -> VkMemoryDedicatedAllocateInfo.
                pnext: MemoryPNext::ExportDedicated {
                    handle_type: EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF,
                    image: image_id.into(),
                },
                size,
                memory_type_index,
            },
        );
        self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_ALLOCATE_MEMORY)
                .mismatch(0x0106)
                .refuse_result(0x0107),
        )?;
        Ok(memory_id)
    }

    pub(super) fn allocate_export_image_memory(
        &mut self,
        adapter: &AdapterContext,
        size: u64,
        memory_type_index: u32,
    ) -> Result<VkDeviceMemoryId, VirtioError> {
        let memory_id = self.new_memory_id();
        let w = encode_memory_allocate(
            self.device_id.into(),
            memory_id.into(),
            &MemoryAllocateSpec {
                pnext: MemoryPNext::Export {
                    handle_type: EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF,
                },
                size,
                memory_type_index,
            },
        );
        // `SdgLMem` carries the raw VkResult of the exportable-DMA_BUF
        // allocation for the linear scanout image (dedicated-less export
        // alloc).
        self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_ALLOCATE_MEMORY)
                .mismatch(0x0123)
                .mismatch_marks(b"SdgLMem")
                .refuse_result(0x0124)
                .result_marks(b"SdgLMem"),
        )?;
        Ok(memory_id)
    }

    /// Import an already-live HOST3D resource into this kernel Venus device.
    /// The resource must first be attached to `self.ctx_id`.
    pub(super) fn allocate_imported_resource_memory(
        &mut self,
        adapter: &AdapterContext,
        resource_id: u32,
        allocation_size: u64,
        memory_type_index: u32,
    ) -> Result<VkDeviceMemoryId, VirtioError> {
        let memory_id = self.new_memory_id();
        let w = encode_memory_allocate(
            self.device_id.into(),
            memory_id.into(),
            &MemoryAllocateSpec {
                pnext: MemoryPNext::ImportResource { resource_id },
                size: allocation_size,
                memory_type_index,
            },
        );
        self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_ALLOCATE_MEMORY)
                .mismatch(0x0133)
                .refuse_result(0x0134)
                .result_marks(b"CpMemVr"),
        )?;
        Ok(memory_id)
    }

    pub(super) fn bind_image_memory(
        &mut self,
        adapter: &AdapterContext,
        image_id: VkImageId,
        memory_id: VkDeviceMemoryId,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_BIND_IMAGE_MEMORY, CMD_FLAG_GENERATE_REPLY);
        w.handle(self.device_id);
        w.handle(image_id);
        w.handle(memory_id);
        w.u64(0);
        self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_BIND_IMAGE_MEMORY)
                .mismatch(0x0108)
                .refuse_result(0x0109),
        )?;
        Ok(())
    }

    pub(super) fn bind_buffer_memory(
        &mut self,
        adapter: &AdapterContext,
        buffer_id: VkBufferId,
        memory_id: VkDeviceMemoryId,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_BIND_BUFFER_MEMORY, CMD_FLAG_GENERATE_REPLY);
        w.handle(self.device_id);
        w.handle(buffer_id);
        w.handle(memory_id);
        w.u64(0);
        self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_BIND_BUFFER_MEMORY).refuse_result_undiagnosed(),
        )?;
        Ok(())
    }

    pub(super) fn image_subresource_layout(
        &mut self,
        adapter: &AdapterContext,
        image_id: VkImageId,
        aspect_mask: u32,
    ) -> Result<(u64, u64), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_GET_IMAGE_SUBRESOURCE_LAYOUT, CMD_FLAG_GENERATE_REPLY);
        w.handle(self.device_id);
        w.handle(image_id);
        w.count(true);
        w.u32(aspect_mask);
        w.u32(0);
        w.u32(0);
        w.count(true);
        // No VkResult in this reply shape: word 1 is the simple-pointer.
        let mut r = self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_GET_IMAGE_SUBRESOURCE_LAYOUT).mismatch(0x010A),
        )?;
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

    pub(super) fn create_command_pool(
        &mut self,
        adapter: &AdapterContext,
    ) -> Result<VkCommandPoolId, VirtioError> {
        let pool_id = self.new_command_pool_id();
        let mut w = Writer::new();
        w.header(CMD_CREATE_COMMAND_POOL, CMD_FLAG_GENERATE_REPLY);
        w.handle(self.device_id);
        w.count(true);
        w.i32(ST_COMMAND_POOL_CREATE_INFO);
        w.count(false);
        w.u32(0); // flags
        w.u32(0); // queueFamilyIndex
        w.count(false); // pAllocator
        w.count(true);
        w.handle(pool_id);
        let mut r = self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_CREATE_COMMAND_POOL)
                .mismatch(0x0112)
                .refuse_result(0x0113),
        )?;
        if r.read_u64()? == 0 {
            diag(0x0114);
            return Err(VirtioError::DeviceError);
        }
        // The host may substitute its own handle; adopt it when it does. This
        // "nonzero wins" rule is unchanged — see the k-venus-13 note on the
        // three inconsistent echo checks in this file.
        let returned = r.read_u64()?;
        Ok(VkCommandPoolId::from_raw(returned).unwrap_or(pool_id))
    }

    pub(super) fn destroy_command_pool(
        &mut self,
        adapter: &AdapterContext,
        pool_id: VkCommandPoolId,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_DESTROY_COMMAND_POOL, 0);
        w.handle(self.device_id);
        w.handle(pool_id);
        w.count(false);
        self.ring_command_noreply(adapter, w.as_slice()?)
    }

    pub(super) fn allocate_command_buffer(
        &mut self,
        adapter: &AdapterContext,
        pool_id: VkCommandPoolId,
    ) -> Result<VkCommandBufferId, VirtioError> {
        let command_buffer_id = self.new_command_buffer_id();
        let mut w = Writer::new();
        w.header(CMD_ALLOCATE_COMMAND_BUFFERS, CMD_FLAG_GENERATE_REPLY);
        w.handle(self.device_id);
        w.count(true);
        w.i32(ST_COMMAND_BUFFER_ALLOCATE_INFO);
        w.count(false);
        w.handle(pool_id);
        w.u32(COMMAND_BUFFER_LEVEL_PRIMARY);
        w.u32(1);
        w.u64(1); // pCommandBuffers array_size
        w.handle(command_buffer_id);
        let mut r = self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_ALLOCATE_COMMAND_BUFFERS)
                .mismatch(0x0115)
                .refuse_result(0x0116),
        )?;
        if r.read_u64()? == 0 {
            diag(0x0117);
            return Err(VirtioError::DeviceError);
        }
        let returned = r.read_u64()?;
        Ok(VkCommandBufferId::from_raw(returned).unwrap_or(command_buffer_id))
    }

    pub(super) fn begin_command_buffer(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: VkCommandBufferId,
    ) -> Result<(), VirtioError> {
        self.begin_command_buffer_with_flags(
            adapter,
            command_buffer_id,
            COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT,
        )
    }

    pub(super) fn begin_command_buffer_with_flags(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: VkCommandBufferId,
        usage_flags: u32,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_BEGIN_COMMAND_BUFFER, CMD_FLAG_GENERATE_REPLY);
        w.handle(command_buffer_id);
        w.count(true);
        w.i32(ST_COMMAND_BUFFER_BEGIN_INFO);
        w.count(false);
        w.u32(usage_flags);
        w.count(false);
        self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_BEGIN_COMMAND_BUFFER)
                .mismatch(0x0118)
                .refuse_result(0x0119),
        )?;
        Ok(())
    }

    /// Which side of a transfer an EXTERNAL queue-family ownership transition is
    /// for. Not a bare `u32`: the access mask and the stage pair have to move
    /// together, and this is the only thing that varies between them.
    pub(super) fn cmd_acquire_image_from_external(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: VkCommandBufferId,
        image: VkImageId,
        access: TransferAccess,
    ) -> Result<(), VirtioError> {
        self.cmd_image_barrier(
            adapter,
            command_buffer_id,
            ImageBarrier::acquire_from_external(image, access),
        )
    }

    pub(super) fn cmd_release_image_to_external(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: VkCommandBufferId,
        image: VkImageId,
        access: TransferAccess,
    ) -> Result<(), VirtioError> {
        self.cmd_image_barrier(
            adapter,
            command_buffer_id,
            ImageBarrier::release_to_external(image, access),
        )
    }

    /// A `QUEUE_FAMILY_IGNORED` transition on a scratch conversion image: a
    /// third barrier kind an acquire/release pair cannot express, because no
    /// ownership changes hands.
    pub(super) fn cmd_internal_image_barrier(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: VkCommandBufferId,
        image: VkImageId,
        src_access: u32,
        dst_access: u32,
    ) -> Result<(), VirtioError> {
        self.cmd_image_barrier(
            adapter,
            command_buffer_id,
            ImageBarrier::internal(image, src_access, dst_access),
        )
    }

    /// The one-shot transition of a freshly created image into GENERAL, from
    /// whichever initial layout it was created with. Also IGNORED/IGNORED.
    pub(super) fn cmd_initial_image_transition(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: VkCommandBufferId,
        image: VkImageId,
        old_layout: u32,
    ) -> Result<(), VirtioError> {
        self.cmd_image_barrier(
            adapter,
            command_buffer_id,
            ImageBarrier::initial_to_general(image, old_layout),
        )
    }

    pub(super) fn cmd_acquire_buffer_from_external(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: VkCommandBufferId,
        buffer: VkBufferId,
        size: u64,
    ) -> Result<(), VirtioError> {
        self.cmd_buffer_barrier(
            adapter,
            command_buffer_id,
            BufferBarrier::acquire_from_external(buffer, size),
        )
    }

    pub(super) fn cmd_release_buffer_to_external(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: VkCommandBufferId,
        buffer: VkBufferId,
        size: u64,
    ) -> Result<(), VirtioError> {
        self.cmd_buffer_barrier(
            adapter,
            command_buffer_id,
            BufferBarrier::transfer_write_to_host_read(buffer, size),
        )?;
        self.cmd_buffer_barrier(
            adapter,
            command_buffer_id,
            BufferBarrier::release_to_external(buffer, size),
        )
    }

    /// Encode one `vkCmdPipelineBarrier` carrying a single image barrier.
    ///
    /// PRIVATE, and taking a struct rather than eleven positionals. The
    /// positional form was the hazard R1003 names: six adjacent same-typed
    /// `u32`s where transposing `src_access`/`dst_access` or
    /// `src_queue_family`/`dst_queue_family` turns an ACQUIRE into a RELEASE.
    /// Both spellings compiled, both type-checked, and the wrong one is a
    /// host-side queue-family ownership violation rather than an error.
    /// Callers go through the named constructors above.
    pub(super) fn cmd_image_barrier(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: VkCommandBufferId,
        b: ImageBarrier,
    ) -> Result<(), VirtioError> {
        let ImageBarrier {
            image: image_id,
            src_stage,
            dst_stage,
            src_access,
            dst_access,
            old_layout,
            new_layout,
            src_queue_family,
            dst_queue_family,
        } = b;
        let mut w = Writer::new();
        w.header(CMD_PIPELINE_BARRIER, 0);
        w.handle(command_buffer_id);
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
        w.handle(image_id);
        w.u32(IMAGE_ASPECT_COLOR);
        w.u32(0);
        w.u32(1);
        w.u32(0);
        w.u32(1);
        self.ring_command_noreply(adapter, w.as_slice()?)
    }

    /// Encode one `vkCmdPipelineBarrier` carrying a single buffer barrier.
    /// PRIVATE, for the same reason as [`Self::cmd_image_barrier`].
    pub(super) fn cmd_buffer_barrier(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: VkCommandBufferId,
        b: BufferBarrier,
    ) -> Result<(), VirtioError> {
        let BufferBarrier {
            buffer: buffer_id,
            size: buffer_size,
            src_stage,
            dst_stage,
            src_access,
            dst_access,
            src_queue_family,
            dst_queue_family,
        } = b;
        let mut w = Writer::new();
        w.header(CMD_PIPELINE_BARRIER, 0);
        w.handle(command_buffer_id);
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
        w.handle(buffer_id);
        w.u64(0); // offset
        w.u64(buffer_size);
        w.u32(0); // imageMemoryBarrierCount
        w.count(false);
        self.ring_command_noreply(adapter, w.as_slice()?)
    }

    pub(super) fn cmd_copy_image(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: VkCommandBufferId,
        source_image_id: VkImageId,
        target_image_id: VkImageId,
        width: u32,
        height: u32,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_COPY_IMAGE, 0);
        w.handle(command_buffer_id);
        w.handle(source_image_id);
        w.u32(IMAGE_LAYOUT_GENERAL);
        w.handle(target_image_id);
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

    pub(super) fn cmd_blit_image(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: VkCommandBufferId,
        source_image_id: VkImageId,
        target_image_id: VkImageId,
        width: u32,
        height: u32,
    ) -> Result<(), VirtioError> {
        let width = i32::try_from(width).map_err(|_| VirtioError::DeviceError)?;
        let height = i32::try_from(height).map_err(|_| VirtioError::DeviceError)?;
        let mut w = Writer::new();
        w.header(CMD_BLIT_IMAGE, 0);
        w.handle(command_buffer_id);
        w.handle(source_image_id);
        w.u32(IMAGE_LAYOUT_GENERAL);
        w.handle(target_image_id);
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

    pub(super) fn cmd_copy_image_to_buffer(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: VkCommandBufferId,
        source_image_id: VkImageId,
        destination_buffer_id: VkBufferId,
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
        w.handle(command_buffer_id);
        w.handle(source_image_id);
        w.u32(IMAGE_LAYOUT_GENERAL);
        w.handle(destination_buffer_id);
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

    pub(super) fn end_command_buffer(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: VkCommandBufferId,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_END_COMMAND_BUFFER, CMD_FLAG_GENERATE_REPLY);
        w.handle(command_buffer_id);
        self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_END_COMMAND_BUFFER)
                .mismatch(0x011A)
                .refuse_result(0x011B),
        )?;
        Ok(())
    }

    pub(super) fn create_fence(
        &mut self,
        adapter: &AdapterContext,
    ) -> Result<VkFenceId, VirtioError> {
        let fence_id = self.new_fence_id();
        let mut w = Writer::new();
        w.header(CMD_CREATE_FENCE, CMD_FLAG_GENERATE_REPLY);
        w.handle(self.device_id);
        w.count(true);
        w.i32(ST_FENCE_CREATE_INFO);
        w.count(false);
        w.u32(0); // flags
        w.count(false); // pAllocator
        w.count(true);
        w.handle(fence_id);
        let mut r = self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_CREATE_FENCE)
                .mismatch(0x011E)
                .refuse_result(0x011F),
        )?;
        if r.read_u64()? == 0 {
            diag(0x0120);
            return Err(VirtioError::DeviceError);
        }
        let returned = r.read_u64()?;
        if returned != fence_id.get() {
            diag(0x0121);
            return Err(VirtioError::DeviceError);
        }
        Ok(fence_id)
    }

    pub(super) fn wait_for_fence(
        &mut self,
        adapter: &AdapterContext,
        fence_id: VkFenceId,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_WAIT_FOR_FENCES, CMD_FLAG_GENERATE_REPLY);
        w.handle(self.device_id);
        w.u32(1); // fenceCount
        w.u64(1); // pFences array_size
        w.handle(fence_id);
        w.u32(1); // waitAll
        w.u64(5_000_000_000); // 5 s
        self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_WAIT_FOR_FENCES)
                .mismatch(0x0122)
                .refuse_result(0x0123),
        )?;
        Ok(())
    }

    pub(super) fn destroy_fence(
        &mut self,
        adapter: &AdapterContext,
        fence_id: VkFenceId,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_DESTROY_FENCE, 0);
        w.handle(self.device_id);
        w.handle(fence_id);
        w.count(false); // pAllocator
        self.ring_command_noreply(adapter, w.as_slice()?)
    }

    /// Enqueue an empty fence marker. A later wait on this fence completes only
    /// after every previously submitted copy on the ordered graphics queue.
    pub(super) fn queue_submit_fence_marker(
        &mut self,
        adapter: &AdapterContext,
        fence_id: VkFenceId,
    ) -> Result<(), VirtioError> {
        let mut submit = Writer::new();
        submit.header(CMD_QUEUE_SUBMIT, CMD_FLAG_GENERATE_REPLY);
        submit.handle(self.queue_id);
        submit.u32(0); // submitCount
        submit.count(false); // pSubmits
        submit.handle(fence_id);
        // Both arms record the SAME code here, unlike every other site --
        // preserved verbatim rather than tidied into two.
        self.ring_command_expect(
            adapter,
            submit.as_slice()?,
            ReplyCheck::new(CMD_QUEUE_SUBMIT)
                .mismatch(0x0135)
                .refuse_result(0x0135),
        )?;
        Ok(())
    }

    pub(super) fn queue_submit_command_buffer(
        &mut self,
        adapter: &AdapterContext,
        command_buffer_id: VkCommandBufferId,
        fence_id: Option<VkFenceId>,
    ) -> Result<(), VirtioError> {
        let mut submit = Writer::new();
        submit.header(CMD_QUEUE_SUBMIT, CMD_FLAG_GENERATE_REPLY);
        submit.handle(self.queue_id);
        submit.u32(1); // submitCount
        submit.u64(1); // pSubmits array_size
        submit.i32(ST_SUBMIT_INFO);
        submit.count(false);
        submit.u32(0); // waitSemaphoreCount
        submit.count(false);
        submit.count(false);
        submit.u32(1); // commandBufferCount
        submit.u64(1);
        submit.handle(command_buffer_id);
        submit.u32(0); // signalSemaphoreCount
        submit.count(false);
        // VK_NULL_HANDLE when no fence is wanted: `None` is the only way to
        // write a zero here now, and it has to be spelled out.
        submit.u64(fence_id.map_or(0, VkFenceId::get)); // fence
        self.ring_command_expect(
            adapter,
            submit.as_slice()?,
            ReplyCheck::new(CMD_QUEUE_SUBMIT)
                .mismatch(0x011C)
                .refuse_result(0x011D),
        )?;

        Ok(())
    }

    pub(super) fn destroy_image_on_ring(
        &mut self,
        adapter: &AdapterContext,
        image_id: VkImageId,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_DESTROY_IMAGE, 0);
        w.handle(self.device_id);
        w.handle(image_id);
        w.count(false);
        self.ring_command_noreply(adapter, w.as_slice()?)
    }

    pub(super) fn destroy_buffer_on_ring(
        &mut self,
        adapter: &AdapterContext,
        buffer_id: VkBufferId,
    ) -> Result<(), VirtioError> {
        let mut w = Writer::new();
        w.header(CMD_DESTROY_BUFFER, 0);
        w.handle(self.device_id);
        w.handle(buffer_id);
        w.count(false);
        self.ring_command_noreply(adapter, w.as_slice()?)
    }

    pub(super) fn cleanup_imported_source_alias(
        &mut self,
        adapter: &AdapterContext,
        resource_id: u32,
        image_id: VkImageId,
        // `None` on the partial-construction paths where the image exists but
        // its memory allocation never succeeded. That was a bare 0 before, and
        // it was indistinguishable from a caller that simply forgot the
        // argument — which is the swap this whole item exists to prevent.
        memory_id: Option<VkDeviceMemoryId>,
    ) -> Result<(), VirtioError> {
        self.destroy_image_on_ring(adapter, image_id)?;
        if let Some(memory_id) = memory_id {
            self.free_memory_blob(adapter, memory_id.get())?;
        }
        ctrl::ctx_detach_resource(self.passive(), adapter, self.ctx_id(), resource_id)
    }

    pub(super) fn encode_command_buffer_submit(
        &self,
        command_buffer_id: VkCommandBufferId,
    ) -> Writer {
        let mut submit = Writer::new();
        submit.header(CMD_QUEUE_SUBMIT, 0);
        submit.handle(self.queue_id);
        submit.u32(1); // submitCount
        submit.u64(1); // pSubmits array_size
        submit.i32(ST_SUBMIT_INFO);
        submit.count(false); // pNext
        submit.u32(0); // waitSemaphoreCount
        submit.count(false); // pWaitSemaphores
        submit.count(false); // pWaitDstStageMask
        submit.u32(1); // commandBufferCount
        submit.u64(1); // pCommandBuffers array_size
        submit.handle(command_buffer_id);
        submit.u32(0); // signalSemaphoreCount
        submit.count(false); // pSignalSemaphores
        submit.u64(0); // fence
        submit
    }
}
