//! The KMD-owned LINEAR scan-out fallback: preparing and submitting the copy
//! from the exact Windows primary into a scan-out target, and allocating the
//! OPTIMAL/LINEAR image blobs that back it.
//!
//! Moved verbatim out of `virtio/venus.rs` by T8/R1104.

use super::ring::*;
use super::*;

impl VenusClient {
    /// Put the persistent KMD LINEAR image into GENERAL layout and external
    /// ownership exactly once. The setup submission is nonblocking and its
    /// command objects remain alive for the Venus-client lifetime; all later
    /// copies use the same ordered queue and therefore execute after setup.
    pub(super) fn ensure_linear_copy_target_ready(
        &mut self,
        adapter: &AdapterContext,
        target_image_id: VkImageId,
    ) -> Result<(), VirtioError> {
        if self.copy_target_image_id == Some(target_image_id) {
            return Ok(());
        }
        // The old "target_image_id == 0" refusal (diag 0x0136 + FaultCounter
        // CpTgtE) is GONE, not dropped: VkImageId is NonZeroU64, so a caller
        // cannot reach here with a null target. The counter stays defined for
        // any pre-existing service-key value; nothing increments it now.
        if self.copy_target_image_id.is_some() {
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
            if let Some(pool_id) = self.copy_target_init_pool_id {
                self.destroy_command_pool(adapter, pool_id)?;
            }
            // NOT the old target image: it is owned by dedicated_scanout_image /
            // the adapter's cache, never by this client.
            self.copy_target_image_id = None;
            self.copy_target_init_pool_id = None;
            // WRITE-ONLY, see T6/k-venus-17: destroying the pool already freed
            // its command buffer, so this field has no reader. Cleared here for
            // consistency rather than given an invented read.
            crate::diag::record_named_bytes(b"CpTgtSw", target_image_id.get() as u32);
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
            self.cmd_initial_image_transition(
                adapter,
                command_buffer_id,
                target_image_id,
                IMAGE_LAYOUT_PREINITIALIZED,
            )?;
            self.cmd_release_image_to_external(
                adapter,
                command_buffer_id,
                target_image_id,
                TransferAccess::Write,
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
        self.copy_target_image_id = Some(target_image_id);
        self.copy_target_init_pool_id = Some(pool_id);
        self.queue_submit_command_buffer(adapter, command_buffer_id, None)
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
        // The pub surface still speaks raw u64 (R607 commit 2 converts it); this
        // is the one place the "0 is not a handle" rule is enforced, and it
        // reproduces the `target_image_id == 0` arm of the old guard exactly.
        let Some(target_image_id) = VkImageId::from_raw(target_image_id) else {
            diag(0x0137);
            return Err(VirtioError::DeviceError);
        };
        if source_resource_id == 0
            || source_allocation_size == 0
            || width == 0
            || height == 0
            || source_memory_type_index >= self.memory_type_count
        {
            diag(0x0137);
            return Err(VirtioError::DeviceError);
        }
        let source_pixel_format =
            PresentPixelFormat::from_dxgi(source_dxgi_format).ok_or(VirtioError::DeviceError)?;
        let target_pixel_format = PresentPixelFormat::Bgra8Unorm;

        crate::diag::record_named_bytes(b"CpImpSt", 1);
        ctrl::attach_resource_checked(self.passive(), adapter, self.ctx_id(), source_resource_id)?;

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
                let _ = ctrl::ctx_detach_resource(
                    self.passive(),
                    adapter,
                    self.ctx_id(),
                    source_resource_id,
                );
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
                        None,
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
            let _ = self.cleanup_imported_source_alias(
                adapter,
                source_resource_id,
                source_image_id,
                None,
            );
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
                    None,
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
                Some(source_memory_id),
            );
            return Err(e);
        }

        crate::diag::record_named_bytes(b"CpImpSt", 6);
        if let Err(e) = self.ensure_linear_copy_target_ready(adapter, target_image_id) {
            let _ = self.cleanup_imported_source_alias(
                adapter,
                source_resource_id,
                source_image_id,
                Some(source_memory_id),
            );
            return Err(e);
        }

        crate::diag::record_named_bytes(b"CpImpSt", 7);
        let mut conversion_image_id = None;
        let mut conversion_memory_id = None;
        let mut conversion_init_pool_id = None;
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
                        Some(source_memory_id),
                    );
                    return Err(e);
                }
            };
            conversion_image_id = Some(conversion.0);
            conversion_memory_id = Some(conversion.1);
            let command = match self.record_reusable_converted_image_copy(
                adapter,
                source_image_id,
                conversion.0,
                target_image_id,
                width,
                height,
            ) {
                Ok(command) => command,
                Err(e) => {
                    let _ = self.destroy_image_on_ring(adapter, conversion.0);
                    let _ = self.free_memory_object(adapter, conversion.1);
                    let _ = self.cleanup_imported_source_alias(
                        adapter,
                        source_resource_id,
                        source_image_id,
                        Some(source_memory_id),
                    );
                    return Err(e);
                }
            };
            conversion_init_pool_id =
                match self.initialize_present_conversion_image(adapter, conversion.0) {
                    Ok(pool_id) => Some(pool_id),
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
                            Some(source_memory_id),
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
                        Some(source_memory_id),
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
            source_memory_id: Some(source_memory_id),
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
        // Same pub-boundary conversion as prepare_optimal_scanout_copy: the two
        // `== 0` arms of the old guard become the two `from_raw` refusals.
        let (Some(source_image_id), Some(target_image_id)) = (
            VkImageId::from_raw(source_image_id),
            VkImageId::from_raw(target_image_id),
        ) else {
            return Err(VirtioError::DeviceError);
        };
        if source_image_id == target_image_id || width == 0 || height == 0 {
            return Err(VirtioError::DeviceError);
        }
        let source_pixel_format =
            PresentPixelFormat::from_dxgi(source_dxgi_format).ok_or(VirtioError::DeviceError)?;
        let target_pixel_format = PresentPixelFormat::Bgra8Unorm;
        self.ensure_linear_copy_target_ready(adapter, target_image_id)?;
        let mut conversion_image_id = None;
        let mut conversion_memory_id = None;
        let mut conversion_init_pool_id = None;
        let (command_pool_id, command_buffer_id) =
            if source_pixel_format.vk_format() != target_pixel_format.vk_format() {
                let conversion = self.create_bound_present_conversion_image(
                    adapter,
                    width,
                    height,
                    target_pixel_format,
                )?;
                conversion_image_id = Some(conversion.0);
                conversion_memory_id = Some(conversion.1);
                let command = match self.record_reusable_converted_image_copy(
                    adapter,
                    source_image_id,
                    conversion.0,
                    target_image_id,
                    width,
                    height,
                ) {
                    Ok(command) => command,
                    Err(e) => {
                        let _ = self.destroy_image_on_ring(adapter, conversion.0);
                        let _ = self.free_memory_object(adapter, conversion.1);
                        return Err(e);
                    }
                };
                conversion_init_pool_id =
                    match self.initialize_present_conversion_image(adapter, conversion.0) {
                        Ok(pool_id) => Some(pool_id),
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
            source_memory_id: None,
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
        // The two null checks are gone: both handles are NonZeroU64 now.
        if Some(copy.target_image_id) != self.copy_target_image_id {
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
        submit.handle(copy.command_buffer_id);
        submit.u32(0); // signalSemaphoreCount
        submit.count(false); // pSignalSemaphores
        submit.u64(0); // fence
        let fence = ctrl::submit_venus_async_scanout(
            self.passive(),
            adapter,
            self.ctx_id(),
            submit.as_slice()?,
            primary_address,
            ticket,
        )?;
        // Remember it here, under the venus mutex the caller already holds, so
        // the drain in destroy_prepared_image_copy cannot read a stale value
        // published from a different critical section.
        self.scanout_copy_last_fence = fence;
        Ok(fence)
    }

    /// Drain the ordered copy queue and release every object retained for one
    /// allocation. This may block (up to the existing 5-second fence timeout),
    /// so it belongs only in PASSIVE allocation teardown, never the frame path.
    pub fn destroy_prepared_image_copy(
        &mut self,
        adapter: &AdapterContext,
        copy: PreparedImageCopy,
    ) -> Result<(), VirtioError> {
        // The reusable command buffer is submitted through an outer async
        // SUBMIT_3D. Drain its latest GPU-completion fence before enqueuing the
        // inner Vulkan marker; otherwise the marker could be decoded first on a
        // different Venus dispatch path and let us destroy a still-referenced
        // command pool.
        //
        // The fence comes from `self`, not from a caller-supplied parameter, so
        // both the write and this read happen under the venus mutex by
        // construction — which is the invariant the paragraph above asserts.
        if self.scanout_copy_last_fence != 0 {
            match ctrl::wait_fence(
                self.passive(),
                adapter,
                self.scanout_copy_last_fence,
                5_000_000_000,
            ) {
                ctrl::WaitFenceOutcome::Complete => {}
                ctrl::WaitFenceOutcome::TimedOut | ctrl::WaitFenceOutcome::Invalid => {
                    return Err(VirtioError::DeviceError);
                }
            }
        } else {
            // Nothing was ever submitted through this client, so there is
            // nothing to drain. Distinguishable from "drain skipped by
            // accident", which is what a 0 parameter used to look like.
            crate::diag::record_named_bytes(b"CpNoDrn", copy.source_resource_id);
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
        if let Some(pool_id) = copy.conversion_init_pool_id {
            self.destroy_command_pool(adapter, pool_id)?;
        }
        if let Some(image_id) = copy.conversion_image_id {
            self.destroy_image_on_ring(adapter, image_id)?;
        }
        if let Some(memory_id) = copy.conversion_memory_id {
            self.free_memory_object(adapter, memory_id)?;
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
                    let _ = self.destroy_image(adapter, image_id.get());
                    return Err(error);
                }
            };
        let memory_type_index = match self.choose_device_local_memory_type(memory_type_bits) {
            Some(choice) => Self::accept_memory_type(choice),
            None => {
                let _ = self.destroy_image(adapter, image_id.get());
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
                let _ = self.destroy_image(adapter, image_id.get());
                return Err(error);
            }
        };
        if let Err(error) = self.bind_image_memory(adapter, image_id, memory_id) {
            let _ = self.destroy_image(adapter, image_id.get());
            let _ = self.free_memory_blob(adapter, memory_id.get());
            return Err(error);
        }

        let blob_flags = VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE | VIRTIO_GPU_BLOB_FLAG_USE_CROSS_DEVICE;
        let resource_id = match ctrl::resource_create_blob(
            self.passive(),
            adapter,
            self.ctx_id(),
            VIRTIO_GPU_BLOB_MEM_HOST3D,
            blob_flags,
            memory_id.get(),
            allocation_size,
        ) {
            Ok(id) => id,
            Err(error) => {
                let _ = self.destroy_image(adapter, image_id.get());
                let _ = self.free_memory_blob(adapter, memory_id.get());
                return Err(error);
            }
        };
        let _ = adapter.with_virtio(|v| v.note_blob_size(resource_id, allocation_size));

        Ok(OptimalImageBlob {
            blob: HostVisibleBlob {
                blob_id: memory_id.get(),
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
        let memory_type_index = Self::accept_memory_type(
            self.choose_host_visible_memory_type(memory_type_bits)
                .ok_or(VirtioError::DeviceError)?,
        );
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
            self.passive(),
            adapter,
            self.ctx_id(),
            VIRTIO_GPU_BLOB_MEM_HOST3D,
            blob_flags,
            memory_id.get(),
            alloc_size,
        )?;
        let _ = adapter.with_virtio(|v| v.note_blob_size(res_id, alloc_size));
        crate::diag::record_named_bytes(b"SdgLStg", 0x10);
        Ok(ScanoutImageBlob {
            blob: HostVisibleBlob {
                blob_id: memory_id.get(),
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
}
