//! Venus bring-up: the host-visible ring/reply blob allocation and the
//! `VenusRing -> VenusInstance -> VenusClient` typestate ladder.
//!
//! Moved verbatim out of `virtio/venus.rs` by T8/R1104. The breadcrumb
//! sequence `0x0D00_0001` .. `0x0D00_000D` under `DiagLevel=1` is the proof
//! this moved intact.

use super::ring::*;
use super::*;

// DIVERGES: non-saturating, see T4a. The two other copies of this function moved
// to `helios_kmd_logic::round_up_page`, which saturates; this one wraps to 0 for a
// `size` within 4095 of `u64::MAX`. Callers (`:1102`, `:4288`, `:4380`, `:4465`)
// all pass a host-reported memory requirement, so unifying it would be a real
// behaviour change to a Venus allocation size and needs its own before/after
// evidence — deliberately not folded into the R101 move.
pub(super) fn round_up_page(size: u64) -> u64 {
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
        crate::virtio::gpu::OwnerFilter::Exactly(None),
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
    pub(super) fn bring_up(
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
            crate::virtio::gpu::OwnerFilter::Exactly(None),
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
            crate::virtio::gpu::OwnerFilter::Exactly(None),
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
    pub(super) fn into_instance(
        mut self,
        adapter: &AdapterContext,
    ) -> Result<VenusInstance, VirtioError> {
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
    pub(super) fn get_device_queue(
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
    pub(super) fn into_device(
        mut self,
        adapter: &AdapterContext,
    ) -> Result<VenusClient, VirtioError> {
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
    pub(super) fn create_device_with_ext_ladder(
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
