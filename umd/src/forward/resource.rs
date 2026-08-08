//! Resource create / open / destroy / resolve, and WDDM allocation.
//!
//! `make_resident`, `allocate_wddm_resource`, `finish_wddm_tex2d`,
//! `create_resource`, `open_resource` and the shared-resource resolvers.
//!
//! Moved verbatim out of `forward.rs` by T8/R1107.

use super::*;

// --- CalcPrivate*Size (all store one COM pointer) ---------------------------

pub(crate) unsafe extern "C" fn calc_size_resource(
    _h: Hdevice,
    _a: *const ddi::D3D11DDIARG_CREATERESOURCE,
) -> u64 {
    8
}
pub(crate) unsafe extern "C" fn calc_size_rtv(
    _h: Hdevice,
    _a: *const ddi::D3D10DDIARG_CREATERENDERTARGETVIEW,
) -> u64 {
    8
}

// --- Resources --------------------------------------------------------------

pub(crate) const RES_BUFFER: ddi::D3D10DDIRESOURCE_TYPE =
    ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_BUFFER;
pub(crate) const RES_BUFFEREX: ddi::D3D10DDIRESOURCE_TYPE =
    ddi::D3D10DDIRESOURCE_TYPE_D3D11DDIRESOURCE_BUFFEREX;
pub(crate) const RES_TEX2D: ddi::D3D10DDIRESOURCE_TYPE =
    ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_TEXTURE2D;
pub(crate) const RES_TEX1D: ddi::D3D10DDIRESOURCE_TYPE =
    ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_TEXTURE1D;
pub(crate) const RES_TEX3D: ddi::D3D10DDIRESOURCE_TYPE =
    ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_TEXTURE3D;
pub(crate) const RES_TEXCUBE: ddi::D3D10DDIRESOURCE_TYPE =
    ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_TEXTURECUBE;

/// The resource dimensions `create_resource` implements, as a closed set.
///
/// `D3D10DDIRESOURCE_TYPE` is a bindgen integer, so the conversion stays
/// fallible and exactly one counted catch-all survives at the conversion. The
/// guarantee is that no KNOWN dimension can be silently dropped by a match that
/// quietly grew a hole, not that the integer domain becomes closed.
#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) enum ResourceDimension {
    Buffer,
    Texture1D,
    Texture2D,
    Texture3D,
}

impl ResourceDimension {
    pub(crate) fn from_ddi(dimension: ddi::D3D10DDIRESOURCE_TYPE) -> Option<Self> {
        match dimension {
            RES_BUFFER | RES_BUFFEREX => Some(Self::Buffer),
            RES_TEX1D => Some(Self::Texture1D),
            RES_TEX2D | RES_TEXCUBE => Some(Self::Texture2D),
            RES_TEX3D => Some(Self::Texture3D),
            _ => None,
        }
    }
}

/// Add one allocation to the WDDM 2.x device residency list.
///
/// E_PENDING is completed with a blocking monitored-fence wait through the
/// runtime callback. No command referencing the allocation may be submitted
/// before that fence reaches `PagingFenceValue`.
pub(crate) unsafe fn make_resident(
    dev: &crate::device_funcs::HeliosDevice,
    handle: ddi::D3DKMT_HANDLE,
) -> Result<ResidentAllocation, i32> {
    const E_PENDING: i32 = 0x8000_000Au32 as i32;

    let Some(handle) = core::num::NonZeroU32::new(handle) else {
        log_error!("WDDM residency: zero allocation handle");
        return Err(E_INVALIDARG);
    };
    let Some(queue) = dev.paging_queue else {
        log_error!("WDDM residency: device has no paging queue");
        return Err(E_FAIL);
    };
    if dev.kt_callbacks.is_null() {
        log_error!("WDDM residency: no runtime callbacks");
        return Err(E_FAIL);
    }
    let Some(make_resident_cb) = (*dev.kt_callbacks).pfnMakeResidentCb else {
        log_error!("WDDM residency: pfnMakeResidentCb missing");
        return Err(E_FAIL);
    };
    let Some(evict_cb) = (*dev.kt_callbacks).pfnEvictCb else {
        // Do not acquire a residency reference that cannot be balanced.
        log_error!("WDDM residency: pfnEvictCb missing");
        return Err(E_FAIL);
    };

    let allocation = handle.get();
    let mut arg = ddi::D3DDDI_MAKERESIDENT::default();
    arg.hPagingQueue = queue.handle.get();
    arg.NumAllocations = 1;
    arg.AllocationList = &allocation;
    let hr = make_resident_cb(dev.h_rt_device, &mut arg);
    trace_line!(
        "WDDM residency: MakeResident alloc=0x{:x} hr=0x{:08x} fence={} trim={}",
        allocation,
        hr as u32,
        arg.PagingFenceValue,
        arg.NumBytesToTrim
    );
    if hr != 0 && hr != E_PENDING {
        log_error!(
            "WDDM residency: MakeResident FAILED alloc=0x{:x} hr=0x{:08x} trim={}",
            allocation,
            hr as u32,
            arg.NumBytesToTrim
        );
        return Err(hr);
    }

    let resident = ResidentAllocation {
        handle,
        h_rt_device: dev.h_rt_device,
        evict_cb,
    };
    if hr == E_PENDING {
        let Some(wait_cb) = (*dev.kt_callbacks).pfnWaitForSynchronizationObjectFromCpuCb else {
            log_error!("WDDM residency: E_PENDING but CPU fence-wait callback is missing");
            drop(resident);
            return Err(E_FAIL);
        };
        let sync_object = queue.sync_object.get();
        let fence_value = arg.PagingFenceValue;
        let mut wait = ddi::D3DDDICB_WAITFORSYNCHRONIZATIONOBJECTFROMCPU::default();
        wait.ObjectCount = 1;
        wait.ObjectHandleArray = &sync_object;
        wait.FenceValueArray = &fence_value;
        // hAsyncEvent == NULL selects the blocking, non-polling form.
        let wait_hr = wait_cb(dev.h_rt_device, &wait);
        trace_line!(
            "WDDM residency: wait alloc=0x{:x} fence={} observed={} hr=0x{:08x}",
            allocation,
            fence_value,
            queue.fence_value_cpu.as_ptr().read_volatile(),
            wait_hr as u32
        );
        if wait_hr != 0 {
            log_error!(
                "WDDM residency: paging-fence wait FAILED alloc=0x{:x} fence={} hr=0x{:08x}",
                allocation,
                fence_value,
                wait_hr as u32
            );
            drop(resident);
            return Err(wait_hr);
        }
    }

    Ok(resident)
}

pub(crate) unsafe fn deallocate_standalone(
    dev: &crate::device_funcs::HeliosDevice,
    allocation: ddi::D3DKMT_HANDLE,
) {
    if dev.kt_callbacks.is_null() {
        return;
    }
    let Some(deallocate_cb) = (*dev.kt_callbacks).pfnDeallocateCb else {
        return;
    };
    let mut allocation = allocation;
    let mut arg = ddi::D3DDDICB_DEALLOCATE {
        hResource: core::ptr::null_mut(),
        NumAllocations: 1,
        HandleList: &mut allocation,
    };
    let hr = deallocate_cb(dev.h_rt_device, &mut arg);
    log_error!(
        "DDI allocate rollback: alloc=0x{:x} hr=0x{:08x}",
        allocation,
        hr as u32
    );
}

pub(crate) unsafe fn allocate_wddm_resource(
    h: Hdevice,
    a: &ddi::D3D11DDIARG_CREATERESOURCE,
    mip0: &ddi::D3D10DDI_MIPINFO,
    h_rt: ddi::D3D10DDI_HRTRESOURCE,
    // `Some` = venus-backed (the KMD adopts our allocation), `None` = plain
    // KMD-backed standard allocation. This one `Option` replaces the three
    // independent `backing_blob_id != 0` conjunctions.
    backing: Option<VenusBacking>,
    // Kept separate from `scanout` on purpose -- see alloc.rs.
    direct_scanout_primary: bool,
    // Scan-out primary metadata. LINEAR paths use the queried COLOR row pitch;
    // direct OPTIMAL uses a logical scanout stride while QEMU validates the
    // opaque allocation with its exact Vulkan allocation size.
    scanout: Option<ScanoutGeometry>,
) -> Result<(Option<ResidentAllocation>, ddi::D3DKMT_HANDLE), i32> {
    const DDI_BIND_PRESENT: u32 = 0x0000_0080;

    let needs_allocation = needs_wddm_texture_allocation(a);
    if !needs_allocation {
        return Ok((None, 0));
    }

    let Some(dev) = helios_device(h) else {
        return Err(E_FAIL);
    };
    if dev.kt_callbacks.is_null() {
        log_error!("DDI allocate_wddm_resource: no KT callbacks");
        return Err(E_FAIL);
    }
    let Some(allocate_cb) = (*dev.kt_callbacks).pfnAllocateCb else {
        log_error!("DDI allocate_wddm_resource: pfnAllocateCb missing");
        return Err(E_FAIL);
    };

    let venus_ctx_id = dev.dxvk.venus_context_id();
    if venus_ctx_id == 0 {
        log_error!("DDI allocate_wddm_resource: no Venus context id");
    }

    const CROSS_ADAPTER_PITCH_ALIGN: u32 = 256;
    let raw_pitch = mip0
        .TexelWidth
        .saturating_mul(dxgi_bytes_per_pixel(a.Format as u32));
    let pitch =
        raw_pitch.saturating_add(CROSS_ADAPTER_PITCH_ALIGN - 1) & !(CROSS_ADAPTER_PITCH_ALIGN - 1);
    // A LINEAR scan-out primary reports its exact COLOR row pitch; use it verbatim so
    // `SET_SCANOUT_BLOB` reads rows at the true host stride instead of the
    // cross-adapter guess (a wrong stride shears the scanned-out image).
    let pitch = match scanout {
        Some(g) => g.pitch.get(),
        None => pitch,
    };
    let linear_size = (pitch as u64)
        .saturating_mul(mip0.TexelHeight.max(1) as u64)
        .max(4096);
    // A live backing with a zero blob_size still falls back to the linear size:
    // blob_id is the only field that gates a mode.
    let size = match backing {
        Some(b) if b.blob_size != 0 => b.blob_size,
        _ => linear_size,
    };

    // pPrimaryDesc is the runtime's authoritative primary classification.
    // A dedicated-copy source is intentionally OPTIMAL and has no scanout
    // pitch, but it still must be a WDDM primary or DXGI rejects every Flip
    // before DxgkDdiPresent with "Source of Flip must be primary".
    let marks_scanout_primary = !a.pPrimaryDesc.is_null();
    let meta_misc_flags = if direct_scanout_primary {
        a.MiscFlags | HELIOS_WDDM_ALLOC_MISC_PRIMARY | HELIOS_WDDM_ALLOC_MISC_DIRECT_SCANOUT
    } else if marks_scanout_primary {
        a.MiscFlags | HELIOS_WDDM_ALLOC_MISC_PRIMARY
    } else {
        a.MiscFlags
    };

    let mut private = RuntimeAllocPrivate {
        alloc: HeliosWddmAllocPrivate::new(
            if backing.is_some() {
                HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY
            } else {
                HELIOS_WDDM_ALLOC_KIND_STANDARD
            },
            venus_ctx_id,
            backing.map_or(0, |b| b.blob_id.get()),
            size,
            backing
                .and_then(|b| b.global_vidmm_tracker)
                .map_or(VIRTIO_GPU_BLOB_MEM_HOST3D, |tracker| tracker.cookie),
            if backing.is_some() {
                // DXVK render targets are normally backed by device-local Venus
                // memory. virglrenderer rejects USE_MAPPABLE for non-host-visible
                // memory ("mem cannot support mappable blob"). They still must
                // be shareable so the host can export/import the backing memory.
                VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE
                    | if backing.is_some_and(|b| b.global_vidmm_tracker.is_some()) {
                        HELIOS_WDDM_BLOB_FLAG_GLOBAL_VIDMM_TRACKER
                    } else {
                        0
                    }
            } else {
                VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE
            },
            // On the typed GLOBAL_VIDMM_TRACKER shape this field carries the
            // system-wide KMT share handle instead of a cache policy. The KMD
            // and protocol helper interpret it only under that private bit.
            backing
                .and_then(|b| b.global_vidmm_tracker)
                .map_or(VIRTIO_GPU_MAP_CACHE_CACHED, |tracker| tracker.global_share),
            // The venus resource id for the KMD to adopt. Was patched into the
            // struct after construction because the constructor could not
            // express the field; it is a parameter now, so the record reaches
            // the kernel fully initialised by one expression. R805.
            backing.map_or(0, |b| b.adopt_resource_id()),
        ),
        meta: HeliosWddmAllocMeta {
            width: mip0.TexelWidth,
            height: mip0.TexelHeight,
            format: dxgi_to_d3dddi_format(a.Format as u32),
            pitch,
            bind_flags: a.BindFlags,
            misc_flags: meta_misc_flags,
            // C1 identity: the creating vkAllocateMemory's exact parameters for
            // adopted venus-backed resources (a cross-process opener must import
            // with them). Zero for KMD-backed standard allocations — the KMD
            // fills them at CreateAllocation from its kernel venus client.
            venus_alloc_size: backing.map_or(0, |b| b.alloc_size),
            memory_type_index: backing.map_or(0, |b| b.memory_type_index),
            // Carry the creator's EXACT DXGI format so a cross-process opener
            // rebuilds the image with the same bpp/layout instead of a squashed
            // BGRA (the `format` field below is a lossy D3DDDIFORMAT for the
            // KMD's DescribeAllocation). `a.Format` is already a DXGI_FORMAT.
            dxgi_format: a.Format as u32,
            // Scan-out primary's real memory-plane-0 offset (0 for everything
            // else); the KMD adds it to the blob base in SET_SCANOUT_BLOB.
            plane_offset: scanout.map_or(0, |g| g.plane_offset),
        },
    };
    let pre_private_alloc = private.alloc;
    let pre_private_meta = private.meta;

    let pre_n = WDDM_ALLOC_LOG_COUNT.peek();
    if pre_n < 128 {
        log_error!(
            "DDI allocate_wddm_resource pre: blob=0x{:x} res_id={} ctx={} kind={} size={} alloc_size={} mti={} {}x{} fmt={} bind=0x{:x} misc=0x{:x} primary_desc={}",
            private.alloc.blob_id,
            private.alloc.adopt_resource_id,
            private.alloc.ctx_id,
            private.alloc.kind,
            private.alloc.size,
            backing.map_or(0, |b| b.alloc_size),
            backing.map_or(0, |b| b.memory_type_index),
            mip0.TexelWidth,
            mip0.TexelHeight,
            a.Format,
            a.BindFlags,
            a.MiscFlags,
            !a.pPrimaryDesc.is_null()
        );
    }

    let mut allocation_info = ddi::D3DDDI_ALLOCATIONINFO2::default();
    let private_ptr = (&mut private as *mut RuntimeAllocPrivate).cast();
    let private_size = core::mem::size_of::<RuntimeAllocPrivate>() as u32;
    allocation_info.pPrivateDriverData = private_ptr;
    allocation_info.PrivateDriverDataSize = private_size;
    let is_present = (a.BindFlags & DDI_BIND_PRESENT) != 0;
    let is_primary_allocation = !a.pPrimaryDesc.is_null();
    allocation_info.VidPnSourceId = if !a.pPrimaryDesc.is_null() {
        (*a.pPrimaryDesc).VidPnSourceId
    } else {
        0
    };
    // A pPrimaryDesc resource is a real WDDM primary regardless of whether its
    // backing is directly scannable or copied into the KMD-owned LINEAR target.
    allocation_info.Flags.Value = if is_primary_allocation { 1 } else { 0 };
    let mut alloc = ddi::D3DDDICB_ALLOCATE::default();
    // pfnAllocateCb expects the runtime resource handle for the resource whose
    // surfaces are being allocated. Shared resources additionally return an
    // hKMResource, but the association itself is not optional for present-only
    // allocations.
    alloc.pPrivateDriverData = private_ptr;
    alloc.PrivateDriverDataSize = private_size;
    alloc.hResource = h_rt.handle;
    alloc.NumAllocations = 1;
    alloc.__bindgen_anon_1.pAllocationInfo2 = &mut allocation_info;

    let hr = allocate_cb(dev.h_rt_device, &mut alloc);
    let h_allocation = allocation_info.hAllocation;
    let post_private_alloc = private.alloc;
    let post_private_meta = private.meta;
    let post_open_identity = unsafe { read_open_identity(private_ptr, private_size) };
    let n = WDDM_ALLOC_LOG_COUNT.next();
    if n < 128 || hr != 0 {
        log_error!(
            "DDI allocate_wddm_resource: hr=0x{:08x} alloc=0x{:x} km=0x{:x} rt={:p} assoc={:p} info={} rpriv={} size={} pitch={} blob=0x{:x} res_id={} ctx={} kind={} primary={} present={} vidpn={} {}x{} fmt={} bind=0x{:x} misc=0x{:x}",
            hr as u32,
            h_allocation,
            alloc.hKMResource,
            h_rt.handle,
            alloc.hResource,
            allocation_info.PrivateDriverDataSize,
            alloc.PrivateDriverDataSize,
            size,
            pitch,
            backing.map_or(0, |b| b.blob_id.get()),
            private.alloc.adopt_resource_id,
            private.alloc.ctx_id,
            private.alloc.kind,
            is_primary_allocation,
            is_present,
            allocation_info.VidPnSourceId,
            mip0.TexelWidth,
            mip0.TexelHeight,
            a.Format,
            a.BindFlags,
            a.MiscFlags
        );
        if pre_private_alloc.adopt_resource_id != post_private_alloc.adopt_resource_id
            || pre_private_alloc.kind != post_private_alloc.kind
            || pre_private_alloc.ctx_id != post_private_alloc.ctx_id
            || pre_private_alloc.blob_id != post_private_alloc.blob_id
            || pre_private_meta.venus_alloc_size != post_private_meta.venus_alloc_size
            || pre_private_meta.memory_type_index != post_private_meta.memory_type_index
        {
            log_error!(
                "DDI allocate_wddm_resource private mutated: pre blob=0x{:x} res_id={} ctx={} kind={} vas={} mti={} -> post blob=0x{:x} res_id={} ctx={} kind={} vas={} mti={}",
                pre_private_alloc.blob_id,
                pre_private_alloc.adopt_resource_id,
                pre_private_alloc.ctx_id,
                pre_private_alloc.kind,
                pre_private_meta.venus_alloc_size,
                pre_private_meta.memory_type_index,
                post_private_alloc.blob_id,
                post_private_alloc.adopt_resource_id,
                post_private_alloc.ctx_id,
                post_private_alloc.kind,
                post_private_meta.venus_alloc_size,
                post_private_meta.memory_type_index
            );
        }
        if let Some((ident, meta)) = post_open_identity {
            let meta = meta.unwrap_or_default();
            log_error!(
                "DDI allocate_wddm_resource callback private: hRT={:p} hKM=0x{:x} \
                 hAlloc=0x{:x} identity=res_id:{} kind:{} ctx:{} blob_size:{} \
                 venus_size:{} mem_type:{} meta={}x{} pitch:{} d3dfmt:{} \
                 dxgifmt:{} bind:0x{:x} misc:0x{:x}",
                h_rt.handle,
                alloc.hKMResource,
                h_allocation,
                ident.resource_id,
                ident.kind,
                ident.ctx_id,
                ident.blob_size,
                ident.venus_alloc_size,
                ident.memory_type_index,
                meta.width,
                meta.height,
                meta.pitch,
                meta.format,
                meta.dxgi_format,
                meta.bind_flags,
                meta.misc_flags,
            );
        }
    }
    if hr != 0 {
        return Err(hr);
    }

    match unsafe { make_resident(dev, h_allocation) } {
        Ok(resident) => Ok((Some(resident), alloc.hKMResource)),
        Err(resident_hr) => {
            // pfnAllocateCb succeeded, so this UMD owns the allocation even
            // though residency failed. Roll it back before surfacing the
            // failure; no partially initialized ResourceState is created.
            unsafe { deallocate_standalone(dev, h_allocation) };
            Err(resident_hr)
        }
    }
}

/// Common tail for a freshly created 2D texture resource (normal or scan-out
/// primary): record the venus backing identity, make the paired WDDM/KMD
/// allocation (carrying the scan-out row pitch + plane offset for a primary),
/// transfer venus-resource ownership to that allocation, KMT-stamp the DXVK
/// resource, and store it in the DDI handle. `scanout_pitch`/`scanout_offset`
/// are either the LINEAR COLOR layout or the direct-OPTIMAL logical scanout
/// metadata; they are 0/0 for non-primary resources.
pub(crate) unsafe fn finish_wddm_tex2d(
    h: Hdevice,
    a: &ddi::D3D11DDIARG_CREATERESOURCE,
    mip0: &ddi::D3D10DDI_MIPINFO,
    h_rt: ddi::D3D10DDI_HRTRESOURCE,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    res: ID3D11Resource,
    direct_scanout_primary: bool,
    scanout: Option<ScanoutGeometry>,
) {
    let (memory, memory_size, memory_offset, resource_id) = dxvk_resource_memory_info(h, &res);
    let needs_importable = needs_wddm_texture_allocation(a);
    let (backing_blob_id, backing_blob_size, backing_resource_id) = if memory != 0
        && memory_offset == 0
        && memory <= u32::MAX as u64
    {
        (memory, memory_size, resource_id)
    } else {
        // Only resources that get a WDDM allocation (shared / keyed-mutex /
        // present / primary) NEED an importable backing — for those a
        // suballocated DXVK memory means a cross-process opener sees a
        // disconnected KMD blob (two-memory split), so shout. Private
        // textures are suballocated by design (18th session).
        if memory != 0 && needs_importable {
            log_error!(
                    "DDI create_resource(tex2d): SHARED RESOURCE WITHOUT IMPORTABLE BACKING memory=0x{:x} res_id={} size={} offset={} bind=0x{:x} misc=0x{:x}",
                    memory, resource_id, memory_size, memory_offset, a.BindFlags, a.MiscFlags
                );
        } else if memory != 0 {
            trace_line!(
                "DDI create_resource(tex2d): private suballocated memory=0x{:x} size={} offset={}",
                memory,
                memory_size,
                memory_offset
            );
        }
        (0, 0, 0)
    };
    // C1: record the creating vkAllocateMemory's exact size + memory type into
    // the allocation trailer so cross-process openers import with them.
    let (mut venus_alloc_size, mut memory_type_index, mut global_vidmm_tracker) =
        (0u64, 0u32, 0u64);
    if backing_resource_id != 0 {
        if let Some(dev) = helios_device(h) {
            if !dev.dxvk.get_resource_alloc_identity(
                res.as_raw() as usize,
                &mut venus_alloc_size,
                &mut memory_type_index,
                &mut global_vidmm_tracker,
            ) {
                log_error!(
                    "DDI create_resource(tex2d): no venus alloc identity for res_id={}",
                    backing_resource_id
                );
            }
        }
    }
    let backing = VenusBacking::new(
        backing_blob_id,
        backing_blob_size,
        backing_resource_id,
        venus_alloc_size,
        memory_type_index,
        global_vidmm_tracker,
    );
    // A shared/present/primary texture must bind its WDDM allocation to the
    // exact exportable Venus resource that backs the DXVK image. Falling back
    // to a fresh KMD blob would create two disconnected allocations; treating
    // a bare VkDeviceMemory id as a blob id is worse, because virglrenderer
    // rejects the non-exportable memory and destroys the Venus context.
    if needs_importable && backing.is_none() {
        log_error!(
            "DDI create_resource(tex2d): SHARED RESOURCE WITHOUT IMPORTABLE BACKING memory=0x{:x} res_id={} size={} offset={} bind=0x{:x} misc=0x{:x} -> refused",
            memory,
            resource_id,
            memory_size,
            memory_offset,
            a.BindFlags,
            a.MiscFlags
        );
        set_runtime_error(h, E_OUTOFMEMORY);
        return;
    }
    let (allocation, km_resource) =
        match allocate_wddm_resource(h, a, mip0, h_rt, backing, direct_scanout_primary, scanout) {
            Ok(allocation) => allocation,
            Err(hr) => {
                log_error!(
                    "DDI create_resource(tex2d): WDDM allocation/residency failed hr=0x{:08x}",
                    hr as u32
                );
                set_runtime_error(h, hr);
                return;
            }
        };
    let allocation_handle = allocation
        .as_ref()
        .map(ResidentAllocation::handle)
        .unwrap_or(0);
    if allocation_handle != 0 && backing_resource_id != 0 {
        if let Some(dev) = helios_device(h) {
            // SAFETY: `res` is the live resource this DDI just created.
            if !unsafe { dev.dxvk.transfer_resource_ownership(res.as_raw() as usize) } {
                log_error!(
                    "DDI create_resource(tex2d): ownership transfer failed res_id={}",
                    backing_resource_id
                );
            }
        }
    }
    trace_line!(
        "DDI create_resource(tex2d): before KMT stamp km=0x{:x} alloc=0x{:x} blob=0x{:x} res_id={} blob_size={}",
        km_resource,
        allocation_handle,
        backing_blob_id,
        backing_resource_id,
        backing_blob_size
    );
    stamp_dxvk_resource_kmt_handles(h, &res, allocation_handle, km_resource);
    let snapshot_source = core::num::NonZeroU32::new(backing_resource_id).and_then(|resource_id| {
        (venus_alloc_size != 0 && mip0.TexelWidth != 0 && mip0.TexelHeight != 0).then_some(
            SnapshotSourceDesc {
                resource_id: resource_id.get(),
                venus_alloc_size,
                memory_type_index,
                width: mip0.TexelWidth,
                height: mip0.TexelHeight,
                dxgi_format: a.Format as u32,
            },
        )
    });
    // Only the exact runtime-designated primary may identify itself through
    // Present private data. Ordinary resource identity stays in
    // `snapshot_source` above, never in this rotation-coupled field.
    let present_private = match (
        direct_scanout_primary,
        core::num::NonZeroU32::new(backing_resource_id),
        scanout,
    ) {
        (true, Some(resource_id), Some(geometry)) => HeliosPresentPrivateData {
            plane_offset: geometry.plane_offset,
            magic: HELIOS_PRESENT_PRIVATE_MAGIC,
            version: HELIOS_PRESENT_PRIVATE_VERSION,
            resource_id: resource_id.get(),
            width: mip0.TexelWidth,
            height: mip0.TexelHeight,
            pitch: geometry.pitch.get(),
            dxgi_format: a.Format as u32,
            reserved: HELIOS_PRESENT_PRIVATE_FLAG_DIRECT_SCANOUT,
            venus_alloc_size: 0,
            // The direct-primary creation path has no relationship to the
            // registered present signal. The per-present eligible path alone
            // fills this appended stream-correlation tail.
            present_ctx_id: 0,
            present_value: 0,
            present_cookie: 0,
            snapshot_memory_type_index: 0,
            snapshot_purpose: HELIOS_PRESENT_SNAPSHOT_PURPOSE_NONE,
        },
        _ => empty_present_private(),
    };
    if RESOURCE_LOG_COUNT.first_n(128).is_some() {
        trace_line!(
            "DDI create_resource(tex2d) ok after-stamp-call: {}x{} fmt={} usage={} bind=0x{:x} misc=0x{:x} sample={}x{}",
            mip0.TexelWidth, mip0.TexelHeight, a.Format, a.Usage, a.BindFlags,
            a.MiscFlags, a.SampleDesc.Count, a.SampleDesc.Quality
        );
    }
    store_resource(
        h_resource,
        res,
        allocation,
        km_resource,
        h_rt.handle,
        AllocationOwnership::CreatedByUmd, // via pfnAllocateCb above
        present_private,
        snapshot_source,
    );
    if present_private.is_valid() {
        unsafe { remember_direct_scanout_allocation(h, allocation_handle, present_private) };
    }
}

pub(crate) unsafe extern "C" fn create_resource(
    h: Hdevice,
    arg: *const ddi::D3D11DDIARG_CREATERESOURCE,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    h_rt: ddi::D3D10DDI_HRTRESOURCE,
) {
    clear_handle(h_resource);
    if arg.is_null() {
        log_error!("DDI CreateResource identity: null args");
        set_runtime_error(h, E_INVALIDARG);
        return;
    }
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let mip0 = if a.pMipInfoList.is_null() {
        ddi::D3D10DDI_MIPINFO {
            TexelWidth: 0,
            TexelHeight: 0,
            TexelDepth: 0,
            PhysicalWidth: 0,
            PhysicalHeight: 0,
            PhysicalDepth: 0,
        }
    } else {
        *a.pMipInfoList
    };

    // Build initial-data array if provided (one entry per subresource).
    let num_sub = (a.MipLevels.max(1) * a.ArraySize.max(1)) as usize;
    if let Some(identity_n) =
        CREATE_RESOURCE_IDENTITY_LOG_COUNT.first_n_then_every_from_one(512, 2048)
    {
        trace_line!(
            "DDI CreateResource identity: #{} hDrv={:p} hRT={:p} hDevice={:p} \
             dim={} fmt={} texel={}x{}x{} physical={}x{}x{} usage={} map=0x{:x} \
             bind=0x{:x} misc=0x{:x} mips={} array={} sample={}x{} byte_stride={} \
             decoder_type={} texture_layout={} initial={:p}/{} primary={:p}",
            identity_n,
            h_resource.pDrvPrivate,
            h_rt.handle,
            h.pDrvPrivate,
            a.ResourceDimension,
            a.Format,
            mip0.TexelWidth,
            mip0.TexelHeight,
            mip0.TexelDepth,
            mip0.PhysicalWidth,
            mip0.PhysicalHeight,
            mip0.PhysicalDepth,
            a.Usage,
            a.MapFlags,
            a.BindFlags,
            a.MiscFlags,
            a.MipLevels,
            a.ArraySize,
            a.SampleDesc.Count,
            a.SampleDesc.Quality,
            a.ByteStride,
            a.DecoderBufferType,
            a.TextureLayout,
            a.pInitialDataUP,
            if a.pInitialDataUP.is_null() {
                0
            } else {
                num_sub
            },
            a.pPrimaryDesc,
        );
        if !a.pPrimaryDesc.is_null() {
            let primary = &*a.pPrimaryDesc;
            let mode = &primary.ModeDesc;
            trace_line!(
                "DDI CreateResource primary: #{} hRT={:p} flags=0x{:x} vidpn={} \
                 mode={}x{} fmt={} refresh={}/{} scanline={} rotation={} scaling={} \
                 driver_flags=0x{:x}",
                identity_n,
                h_rt.handle,
                primary.Flags,
                primary.VidPnSourceId,
                mode.Width,
                mode.Height,
                mode.Format,
                mode.RefreshRate.Numerator,
                mode.RefreshRate.Denominator,
                mode.ScanlineOrdering,
                mode.Rotation,
                mode.Scaling,
                primary.DriverFlags,
            );
        }
        if !a.pInitialDataUP.is_null() {
            for subresource in 0..num_sub.min(16) {
                let up = &*a.pInitialDataUP.add(subresource);
                trace_line!(
                    "DDI CreateResource initial: #{} subresource={} sysmem={:p} \
                     row_pitch={} slice_pitch={}",
                    identity_n,
                    subresource,
                    up.pSysMem,
                    up.SysMemPitch,
                    up.SysMemSlicePitch,
                );
            }
        }
    }
    let mut init: Vec<D3D11_SUBRESOURCE_DATA> = Vec::new();
    if !a.pInitialDataUP.is_null() {
        for i in 0..num_sub {
            let up = &*a.pInitialDataUP.add(i);
            init.push(D3D11_SUBRESOURCE_DATA {
                pSysMem: up.pSysMem,
                SysMemPitch: up.SysMemPitch,
                SysMemSlicePitch: up.SysMemSlicePitch,
            });
        }
    }
    let init_ptr = if init.is_empty() {
        None
    } else {
        Some(init.as_ptr())
    };

    let cpu = cpu_access(a.Usage);

    let Some(dimension) = ResourceDimension::from_ddi(a.ResourceDimension) else {
        // The DDI returns void and `clear_handle` already nulled the slot, so
        // logging and returning told the runtime S_OK with a null driver
        // resource. Every later `load_resource` then returns None and the view
        // create / Map / Copy silently does nothing — "nothing draws", not an
        // error. E_INVALIDARG is in CreateTexture*'s documented return set.
        note_ddi_refusal(&DDI_REFUSALS.unhandled_resource_dimension);
        log_error!(
            "DDI create_resource: unhandled dimension {}",
            a.ResourceDimension
        );
        set_runtime_error(h, E_INVALIDARG);
        return;
    };
    match dimension {
        ResourceDimension::Buffer => {
            let bind = api_bind_flags(a.BindFlags);
            let misc = api_misc_flags(a.MiscFlags, a.BindFlags, true);
            if bind != a.BindFlags || misc != a.MiscFlags || !a.pPrimaryDesc.is_null() {
                log_error!(
                    "DDI create_resource(buffer): normalize bind 0x{:x}->0x{:x} misc 0x{:x}->0x{:x} primary={}",
                    a.BindFlags,
                    bind,
                    a.MiscFlags,
                    misc,
                    !a.pPrimaryDesc.is_null()
                );
            }
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: mip0.TexelWidth,
                Usage: D3D11_USAGE(a.Usage as i32),
                BindFlags: bind,
                CPUAccessFlags: cpu,
                MiscFlags: misc,
                StructureByteStride: a.ByteStride,
            };
            let (allocation, km_resource) =
                match allocate_wddm_resource(h, a, &mip0, h_rt, None, false, None) {
                    Ok(allocation) => allocation,
                    Err(hr) => {
                        log_error!(
                        "DDI create_resource(buffer): WDDM allocation/residency failed hr=0x{:08x}",
                        hr as u32
                    );
                        set_runtime_error(h, hr);
                        return;
                    }
                };
            let allocation_handle = allocation
                .as_ref()
                .map(ResidentAllocation::handle)
                .unwrap_or(0);
            let mut buf: Option<ID3D11Buffer> = None;
            let created = device.CreateBuffer(&desc, init_ptr, Some(&mut buf));
            if let Err(ref e) = created {
                log_error!("DDI create_resource(buffer) failed: {e:?}");
            }
            let res = match buf {
                Some(b) => match b.cast::<ID3D11Resource>() {
                    Ok(r) => Some(r),
                    Err(e) => {
                        log_error!(
                            "DDI create_resource(buffer): cast to ID3D11Resource failed: {e:?}"
                        );
                        None
                    }
                },
                None => {
                    if created.is_ok() {
                        log_error!(
                            "DDI create_resource(buffer): DXVK CreateBuffer returned no buffer"
                        );
                    }
                    None
                }
            };
            let mut stored = false;
            finish_create(h, created, res, |res| {
                stored = true;
                stamp_dxvk_resource_kmt_handles(h, &res, allocation_handle, km_resource);
                if RESOURCE_LOG_COUNT.first_n(128).is_some() {
                    log_error!(
                        "DDI create_resource(buffer) ok: bytes={} fmt={} usage={} bind=0x{:x} misc=0x{:x}",
                        mip0.TexelWidth, a.Format, a.Usage, bind, misc
                    );
                }
                store_resource(
                    h_resource,
                    res,
                    allocation,
                    km_resource,
                    h_rt.handle,
                    AllocationOwnership::CreatedByUmd, // via pfnAllocateCb above
                    empty_present_private(),
                    None,
                );
            });
            if !stored && allocation_handle != 0 {
                // The buffer arm allocates first and creates second, so a failed
                // CreateBuffer (or a missing object, or a failed cast) leaves a
                // kernel allocation nobody can reach: the closure's
                // `Option<ResidentAllocation>` has been dropped by now — evicting
                // the residency reference — but pfnDeallocateCb was never called,
                // and `release_resource` cannot recover it later because
                // `clear_handle` already nulled pDrvPrivate at DDI entry, so
                // DestroyResource reads a null state pointer and returns. The
                // handle would stay associated with the runtime resource until
                // device teardown.
                //
                // Evict-then-deallocate is the order `release_resource`
                // documents, and dropping the closure above already did the
                // evict half. Same call `allocate_wddm_resource` makes for its
                // own rollback.
                if let Some(dev) = helios_device(h) {
                    deallocate_standalone(dev, allocation_handle);
                }
            }
        }
        ResourceDimension::Texture2D => {
            let bind = api_bind_flags(a.BindFlags);
            let mut misc = api_misc_flags(a.MiscFlags, a.BindFlags, false);
            if a.ResourceDimension == RES_TEXCUBE {
                misc |= D3D11_RESOURCE_MISC_TEXTURECUBE.0 as u32;
            }
            if a.MiscFlags != 0 {
                log_error!(
                    "DDI misc translation v2: ddi_misc=0x{:x} ddi_bind=0x{:x} api_misc=0x{:x}",
                    a.MiscFlags,
                    a.BindFlags,
                    misc
                );
            }
            if bind != a.BindFlags
                || (misc & !D3D11_RESOURCE_MISC_TEXTURECUBE.0 as u32) != a.MiscFlags
                || !a.pPrimaryDesc.is_null()
            {
                log_error!(
                    "DDI create_resource(tex2d): {}x{} fmt={} usage={} bind 0x{:x}->0x{:x} misc 0x{:x}->0x{:x} primary={} sample={}x{}",
                    mip0.TexelWidth,
                    mip0.TexelHeight,
                    a.Format,
                    a.Usage,
                    a.BindFlags,
                    bind,
                    a.MiscFlags,
                    misc,
                    !a.pPrimaryDesc.is_null(),
                    a.SampleDesc.Count,
                    a.SampleDesc.Quality
                );
            }
            let desc = D3D11_TEXTURE2D_DESC {
                Width: mip0.TexelWidth,
                Height: mip0.TexelHeight,
                MipLevels: a.MipLevels,
                ArraySize: a.ArraySize.max(1),
                Format: DXGI_FORMAT(a.Format as i32),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: a.SampleDesc.Count,
                    Quality: a.SampleDesc.Quality,
                },
                Usage: D3D11_USAGE(a.Usage as i32),
                BindFlags: bind,
                CPUAccessFlags: cpu,
                MiscFlags: misc,
            };
            // Windows' pPrimaryDesc is the authoritative, non-heuristic marker
            // for a scan-out primary. The supported 32-bit Windows primary
            // formats become dedicated OPTIMAL DMA_BUF exports.
            let is_scanout = !a.pPrimaryDesc.is_null() && matches!(a.Format as u32, 28 | 87 | 88);
            let mut handled = false;
            if is_scanout {
                // The QEMU fork reconstructs this exact same-driver OPTIMAL
                // DMA_BUF with the original blob allocation size. This avoids a
                // guest copy and any virtio protocol field or global modifier
                // extension. The host display backend currently reads it back.
                // The wrapper adopts the reference and returns the metadata
                // with it, so `rp`/`off` cannot be read on the failure path.
                // R813.
                let created = helios_device(h).and_then(|dev| {
                    dev.dxvk.create_scanout_texture2d(
                        mip0.TexelWidth,
                        mip0.TexelHeight,
                        a.Format as u32,
                        bind,
                        misc,
                        false,
                    )
                });
                let (rp, off) = created.as_ref().map_or((0, 0), |(_, p, o)| (*p, *o));
                // BEHAVIOUR CHANGE (R806 sub-commit 2): a zero row pitch is a
                // failed scan-out-primary create, not a primary with no
                // geometry. Previously only `raw != 0` was checked, so a
                // non-zero resource with `rp == 0` would stamp
                // HELIOS_WDDM_ALLOC_MISC_PRIMARY | MISC_DIRECT_SCANOUT into the
                // KMD meta while finish_wddm_tex2d's present_private gate
                // failed -- a direct scan-out primary in the kernel that the
                // UMD never registered in direct_scanout_allocations and could
                // never identify through PresentCb private data. Nothing
                // detected that split state.
                //
                // Not reachable through today's bridge: create_ddi_scanout_
                // texture2d returns 0 for a zero width/height and otherwise
                // computes a non-zero pitch, so raw != 0 implies rp != 0. This
                // closes the cross-FFI contract dependency rather than a live
                // bug, which is why the counter is expected to stay 0.
                let geometry = ScanoutGeometry::new(rp as u32, off);
                // One match over the pair, so the created resource is moved
                // into exactly one arm and the refusal arm still owns it (and
                // therefore still releases it).
                match (created, geometry) {
                    (Some((res, _, _)), Some(geometry)) => {
                        log_error!(
                        "DDI create_resource(tex2d): direct scan-out primary {}x{} fmt={} logicalPitch={} offset={} (OPTIMAL DMA_BUF)",
                        mip0.TexelWidth, mip0.TexelHeight, a.Format, rp, off
                    );
                        finish_wddm_tex2d(h, a, &mip0, h_rt, h_resource, res, true, Some(geometry));
                    }
                    (created, _) => {
                        // Loud failure over fake success: do NOT fall back to a plain
                        // primary — that reintroduces the black scan-out as a "working"
                        // desktop. A failure here is a real direct-scanout regression.
                        if let Some((res, _, _)) = created {
                            // The new arm: the bridge handed back a resource but no
                            // usable stride. Dropping the adopted wrapper releases
                            // it -- nothing else will. R813 removed the manual
                            // IUnknown::from_raw this used to need.
                            SCANOUT_PRIMARY_ZERO_PITCH.fetch_add(1, Ordering::Relaxed);
                            let raw = res.as_raw() as usize;
                            drop(res);
                            log_error!(
                            "DDI create_resource(tex2d): SCAN-OUT PRIMARY ZERO PITCH {}x{} fmt={} raw=0x{:x} offset={} -> refused (zero_pitch={})",
                            mip0.TexelWidth,
                            mip0.TexelHeight,
                            a.Format,
                            raw,
                            off,
                            SCANOUT_PRIMARY_ZERO_PITCH.load(Ordering::Relaxed)
                        );
                        }
                        log_error!(
                        "DDI create_resource(tex2d): SCAN-OUT PRIMARY CREATE FAILED {}x{} fmt={} bind=0x{:x} -> no primary (optimal/dmabuf rejected?)",
                        mip0.TexelWidth, mip0.TexelHeight, a.Format, bind
                    );
                        // The loudness has to reach the runtime, not stop at the log
                        // file: this DDI returns void, so pfnSetErrorCb is the only
                        // way CreateTexture2D fails instead of handing the caller
                        // S_OK with a null driver resource. E_OUTOFMEMORY is in
                        // CreateTexture2D's documented return set; the bridge
                        // returns 0 with no HRESULT to map through.
                        set_runtime_error(h, E_OUTOFMEMORY);
                    }
                }
                handled = true;
            }
            if !handled {
                let mut tex: Option<ID3D11Texture2D> = None;
                // The five existing tex2d trace gates are the same predicate;
                // name it once so the restructured arm cannot drift between them.
                let big = mip0.TexelWidth >= 1024 || mip0.TexelHeight >= 576 || misc != 0;
                if big {
                    log_error!(
                        "DDI create_resource(tex2d): calling DXVK CreateTexture2D {}x{} fmt={} bind=0x{:x} misc=0x{:x} init={} hrt={:p} mips={} array={} usage={} cpu=0x{:x} sample={}x{}",
                        mip0.TexelWidth,
                        mip0.TexelHeight,
                        a.Format,
                        bind,
                        misc,
                        init_ptr.is_some(),
                        h_rt.handle,
                        desc.MipLevels,
                        desc.ArraySize,
                        a.Usage,
                        cpu,
                        a.SampleDesc.Count,
                        a.SampleDesc.Quality
                    );
                }
                let created = device.CreateTexture2D(&desc, init_ptr, Some(&mut tex));
                match created {
                    Ok(()) => {
                        if big {
                            log_error!(
                                "DDI create_resource(tex2d): DXVK CreateTexture2D returned S_OK tex_present={}",
                                tex.is_some()
                            );
                        }
                    }
                    Err(ref e) => log_error!("DDI create_resource(tex2d) failed: {e:?}"),
                }
                let res = match tex {
                    Some(t) => match t.cast::<ID3D11Resource>() {
                        Ok(r) => {
                            if big {
                                log_error!("DDI create_resource(tex2d): cast to ID3D11Resource OK");
                            }
                            Some(r)
                        }
                        Err(_) => {
                            if big {
                                log_error!(
                                    "DDI create_resource(tex2d): cast to ID3D11Resource failed"
                                );
                            }
                            None
                        }
                    },
                    None => {
                        if big && created.is_ok() {
                            log_error!(
                                "DDI create_resource(tex2d): DXVK CreateTexture2D returned no texture"
                            );
                        }
                        None
                    }
                };
                finish_create(h, created, res, |res| {
                    finish_wddm_tex2d(h, a, &mip0, h_rt, h_resource, res, false, None);
                });
            }
        }
        ResourceDimension::Texture1D => {
            // Same shape as the tex3d arm: create first, then allocate, then
            // store — no fallible step between the allocation and the store, so
            // it does not need R407's rollback. All four view translators
            // already handle RES_TEX1D; this arm is what stops
            // `CreateTexture1D` from being an outright failure now that the
            // catch-all reports.
            let bind = api_bind_flags(a.BindFlags);
            let misc = api_misc_flags(a.MiscFlags, a.BindFlags, false);
            log_error!(
                "DDI create_resource(tex1d): {} fmt={} usage={} bind 0x{:x}->0x{:x} misc 0x{:x}->0x{:x} init={} mips={} array={}",
                mip0.TexelWidth,
                a.Format,
                a.Usage,
                a.BindFlags,
                bind,
                a.MiscFlags,
                misc,
                init_ptr.is_some(),
                a.MipLevels,
                a.ArraySize
            );
            let desc = D3D11_TEXTURE1D_DESC {
                Width: mip0.TexelWidth,
                MipLevels: a.MipLevels,
                ArraySize: a.ArraySize.max(1),
                Format: DXGI_FORMAT(a.Format as i32),
                Usage: D3D11_USAGE(a.Usage as i32),
                BindFlags: bind,
                CPUAccessFlags: cpu,
                MiscFlags: misc,
            };
            let mut tex: Option<ID3D11Texture1D> = None;
            let created = device.CreateTexture1D(&desc, init_ptr, Some(&mut tex));
            if let Err(ref e) = created {
                log_error!("DDI create_resource(tex1d) failed: {e:?}");
            }
            let res = match tex {
                Some(t) => match t.cast::<ID3D11Resource>() {
                    Ok(r) => Some(r),
                    Err(_) => {
                        log_error!("DDI create_resource(tex1d): cast to ID3D11Resource failed");
                        None
                    }
                },
                None => {
                    if created.is_ok() {
                        log_error!(
                            "DDI create_resource(tex1d): DXVK CreateTexture1D returned no texture"
                        );
                    }
                    None
                }
            };
            finish_create(h, created, res, |res| {
                let (allocation, km_resource) = match allocate_wddm_resource(
                    h, a, &mip0, h_rt, None, false, None,
                ) {
                    Ok(allocation) => allocation,
                    Err(hr) => {
                        log_error!(
                                "DDI create_resource(tex1d): WDDM allocation/residency failed hr=0x{:08x}",
                                hr as u32
                            );
                        set_runtime_error(h, hr);
                        return;
                    }
                };
                let allocation_handle = allocation
                    .as_ref()
                    .map(ResidentAllocation::handle)
                    .unwrap_or(0);
                stamp_dxvk_resource_kmt_handles(h, &res, allocation_handle, km_resource);
                log_error!(
                    "DDI create_resource(tex1d) ok: {} fmt={} bind=0x{:x} misc=0x{:x}",
                    mip0.TexelWidth,
                    a.Format,
                    bind,
                    misc
                );
                store_resource(
                    h_resource,
                    res,
                    allocation,
                    km_resource,
                    h_rt.handle,
                    AllocationOwnership::CreatedByUmd,
                    empty_present_private(),
                    None,
                );
            });
        }
        ResourceDimension::Texture3D => {
            let bind = api_bind_flags(a.BindFlags);
            let misc = api_misc_flags(a.MiscFlags, a.BindFlags, false);
            log_error!(
                "DDI create_resource(tex3d): {}x{}x{} fmt={} usage={} bind 0x{:x}->0x{:x} misc 0x{:x}->0x{:x} init={} mips={}",
                mip0.TexelWidth,
                mip0.TexelHeight,
                mip0.TexelDepth,
                a.Format,
                a.Usage,
                a.BindFlags,
                bind,
                a.MiscFlags,
                misc,
                init_ptr.is_some(),
                a.MipLevels
            );
            let desc = D3D11_TEXTURE3D_DESC {
                Width: mip0.TexelWidth,
                Height: mip0.TexelHeight,
                Depth: mip0.TexelDepth.max(1),
                MipLevels: a.MipLevels,
                Format: DXGI_FORMAT(a.Format as i32),
                Usage: D3D11_USAGE(a.Usage as i32),
                BindFlags: bind,
                CPUAccessFlags: cpu,
                MiscFlags: misc,
            };
            let mut tex: Option<ID3D11Texture3D> = None;
            let created = device.CreateTexture3D(&desc, init_ptr, Some(&mut tex));
            if let Err(ref e) = created {
                log_error!("DDI create_resource(tex3d) failed: {e:?}");
            }
            let res = match tex {
                Some(t) => match t.cast::<ID3D11Resource>() {
                    Ok(r) => Some(r),
                    Err(_) => {
                        log_error!("DDI create_resource(tex3d): cast to ID3D11Resource failed");
                        None
                    }
                },
                None => {
                    if created.is_ok() {
                        log_error!(
                            "DDI create_resource(tex3d): DXVK CreateTexture3D returned no texture"
                        );
                    }
                    None
                }
            };
            finish_create(h, created, res, |res| {
                // The allocation-failure arm below already reported through
                // set_runtime_error; `return` leaves the closure, and this is
                // the last statement of create_resource, so that is the same
                // exit it was before.
                let (allocation, km_resource) = match allocate_wddm_resource(
                    h, a, &mip0, h_rt, None, false, None,
                ) {
                    Ok(allocation) => allocation,
                    Err(hr) => {
                        log_error!(
                                "DDI create_resource(tex3d): WDDM allocation/residency failed hr=0x{:08x}",
                                hr as u32
                            );
                        set_runtime_error(h, hr);
                        return;
                    }
                };
                let allocation_handle = allocation
                    .as_ref()
                    .map(ResidentAllocation::handle)
                    .unwrap_or(0);
                stamp_dxvk_resource_kmt_handles(h, &res, allocation_handle, km_resource);
                log_error!(
                    "DDI create_resource(tex3d) ok: {}x{}x{} fmt={} bind=0x{:x} misc=0x{:x}",
                    mip0.TexelWidth,
                    mip0.TexelHeight,
                    mip0.TexelDepth,
                    a.Format,
                    bind,
                    misc
                );
                store_resource(
                    h_resource,
                    res,
                    allocation,
                    km_resource,
                    h_rt.handle,
                    AllocationOwnership::CreatedByUmd,
                    empty_present_private(),
                    None,
                );
            });
        }
    }
}

pub(crate) unsafe extern "C" fn destroy_resource(h: Hdevice, h_resource: ddi::D3D10DDI_HRESOURCE) {
    release_resource(h, h_resource);
}

pub(crate) unsafe extern "C" fn open_resource(
    h: Hdevice,
    arg: *const ddi::D3D10DDIARG_OPENRESOURCE,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    h_rt: ddi::D3D10DDI_HRTRESOURCE,
) {
    clear_handle(h_resource);

    if arg.is_null() {
        log_error!("DDI open_resource: null args");
        set_runtime_error(h, E_INVALIDARG);
        return;
    }

    let a = &*arg;
    let info2 = unsafe { a.__bindgen_anon_1.pOpenAllocationInfo2 };
    let mut allocation: ddi::D3DKMT_HANDLE = 0;
    // C1 identity ABI: the KMD wrote a versioned HeliosWddmOpenIdentity record
    // into the open-time private data in DxgkDdiOpenAllocation, after
    // validating the backing venus resource is LIVE. Prefer the per-allocation
    // buffer; fall back to the resource-level one. No adopt-id heuristics.
    let mut identity =
        unsafe { read_opened_allocation(a.pPrivateDriverData, a.PrivateDriverDataSize) };
    // Distinguishes "no identity anywhere" from "identity present, trailer
    // absent" in the refusal below; the second is a producer/KMD bug that used
    // to be swallowed by a 1x1 default.
    let mut identity_without_trailer = matches!(
        unsafe { read_open_identity(a.pPrivateDriverData, a.PrivateDriverDataSize) },
        Some((_, None))
    );
    if a.NumAllocations != 0 && info2.is_null() {
        log_error!("DDI open_resource FAILED: allocation array is null");
        set_runtime_error(h, E_INVALIDARG);
        return;
    }
    for index in 0..a.NumAllocations as usize {
        let info = &*info2.add(index);
        if index == 0 {
            allocation = info.hAllocation;
        }
        let allocation_identity =
            unsafe { read_open_identity(info.pPrivateDriverData, info.PrivateDriverDataSize) };
        // Only a candidate that carries the meta TRAILER can be selected — the
        // type says so now. The KMD stamps the identity into both the
        // per-allocation and the resource-level private buffer
        // (create_allocation.rs `write_open_identity`), but the resource-level
        // copy for KMD standard allocations is the pristine
        // GetStandardAllocationDriverData output whose size the KMD does not
        // choose: it can hold a valid 48-byte identity with no 16-byte trailer,
        // and selecting it loses the real geometry.
        match allocation_identity {
            Some((ident, Some(meta))) => {
                if identity.is_none() {
                    identity = Some(OpenedAllocation { ident, meta });
                }
            }
            Some((_, None)) => identity_without_trailer = true,
            None => {}
        }
        if let Some((ident, meta)) = allocation_identity {
            let meta = meta.unwrap_or_default();
            log_error!(
                "DDI OpenResource allocation: index={} hDrv={:p} hRT={:p} hKM=0x{:x} \
                 hAlloc=0x{:x} private={:p}/{} gpuva=0x{:x} res_id={} kind={} ctx={} \
                 blob_size={} venus_size={} mem_type={} {}x{} pitch={} d3dfmt={} \
                 dxgifmt={} bind=0x{:x} misc=0x{:x}",
                index,
                h_resource.pDrvPrivate,
                h_rt.handle,
                a.hKMResource.handle,
                info.hAllocation,
                info.pPrivateDriverData,
                info.PrivateDriverDataSize,
                info.GpuVirtualAddress,
                ident.resource_id,
                ident.kind,
                ident.ctx_id,
                ident.blob_size,
                ident.venus_alloc_size,
                ident.memory_type_index,
                meta.width,
                meta.height,
                meta.pitch,
                meta.format,
                meta.dxgi_format,
                meta.bind_flags,
                meta.misc_flags,
            );
        } else {
            log_error!(
                "DDI OpenResource allocation: index={} hDrv={:p} hRT={:p} hKM=0x{:x} \
                 hAlloc=0x{:x} private={:p}/{} gpuva=0x{:x} identity=missing",
                index,
                h_resource.pDrvPrivate,
                h_rt.handle,
                a.hKMResource.handle,
                info.hAllocation,
                info.pPrivateDriverData,
                info.PrivateDriverDataSize,
                info.GpuVirtualAddress,
            );
        }
    }
    log_error!(
        "DDI OpenResource resource: hDrv={:p} hRT={:p} num_alloc={} hKM=0x{:x} private={:p}/{}",
        h_resource.pDrvPrivate,
        h_rt.handle,
        a.NumAllocations,
        a.hKMResource.handle,
        a.pPrivateDriverData,
        a.PrivateDriverDataSize,
    );

    // A shared open without a KMD identity record cannot alias the real
    // surface. The old metadata-texture fallback fabricated a blank texture
    // here and stamped it with the real KMT handles — draws "succeeded" and
    // the shared content stayed black forever (audit U-B2). Fail loudly
    // instead so the producer-side bug gets found.
    //
    // The trailer is part of that record, not an optional extra: it carries the
    // width/height/pitch/format the import is built from. Defaulting it to
    // 1x1 BGRA produced exactly the audited failure — a real-KMT-stamped,
    // resident, storable alias whose geometry was wrong. Because that alias is
    // UNDERSIZE relative to the true allocation, the oversize guard that caught
    // the 38th-session import regression cannot see it either.
    let Some(opened) = identity else {
        if identity_without_trailer {
            log_error!(
                "DDI open_resource FAILED: venus identity carries no meta trailer (hKM={:?} alloc=0x{:x}) -> E_FAIL",
                a.hKMResource, allocation
            );
        } else {
            log_error!(
                "DDI open_resource FAILED: no venus identity record (hKM={:?} alloc=0x{:x}) -> E_FAIL",
                a.hKMResource, allocation
            );
        }
        set_runtime_error(h, E_FAIL);
        return;
    };
    let ident = opened.ident;
    let meta = opened.meta;

    if a.hKMResource.handle == 0 {
        log_error!(
            "DDI open_resource FAILED: no hKMResource (res_id={}) -> E_FAIL",
            ident.resource_id
        );
        set_runtime_error(h, E_FAIL);
        return;
    }

    let Some(dev) = helios_device(h) else {
        log_error!("DDI open_resource FAILED: no Helios device -> E_FAIL");
        set_runtime_error(h, E_FAIL);
        return;
    };

    let open_bind = api_bind_flags(meta.bind_flags);
    let open_misc = meta.misc_flags
        & (D3D11_RESOURCE_MISC_SHARED.0 as u32
            | D3D11_RESOURCE_MISC_RESOURCE_CLAMP.0 as u32
            | D3D11_RESOURCE_MISC_GDI_COMPATIBLE.0 as u32);
    // Prefer the identity's venus alloc size; the meta trailer carries the
    // creator-recorded copy for KMD standard allocations.
    let venus_alloc_size = if ident.venus_alloc_size != 0 {
        ident.venus_alloc_size
    } else {
        meta.venus_alloc_size
    };
    // Rebuild the image with the creator's EXACT DXGI format. Falls back to the
    // lossy D3DDDIFORMAT translation only when the creator did not record a DXGI
    // format (0 = legacy trailer / KMD standard allocation, which are BGRA). The
    // old unconditional `d3dddi_to_dxgi_format(meta.format)` collapsed every
    // surface to BGRA — an A8/R8 mask then rebuilt as 4bpp needed 4x the memory
    // and its (correctly sized) import was refused as "undersized".
    let open_dxgi_format = if meta.dxgi_format != 0 {
        meta.dxgi_format
    } else {
        d3dddi_to_dxgi_format(meta.format).0 as u32
    };
    let cross_context_optimal = ident.kind == HELIOS_WDDM_ALLOC_KIND_STANDARD
        && meta.misc_flags & HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE != 0;
    let dedicated_present_buffer = ident.dedicated_present_buffer();
    log_error!(
        "DDI open_resource identity: res_id={} alloc_size={} mem_type={} kind={} ctx={} meta_bind=0x{:x} meta_misc=0x{:x} open_bind=0x{:x} open_misc=0x{:x} dxgi_fmt={} d3dddi_fmt={}",
        ident.resource_id, venus_alloc_size, ident.memory_type_index, ident.kind, ident.ctx_id,
        meta.bind_flags, meta.misc_flags, open_bind, open_misc, open_dxgi_format, meta.format
    );
    // Ordinary shared images always retain their creator's OPTIMAL contract.
    // Production scanout uses a separate KMD-owned plain-LINEAR target; never
    // rebuild DWM imports as DRM-modifier images (the .38 regression).
    let opened = dev.dxvk.open_texture2d(
        meta.width.max(1),
        meta.height.max(1),
        open_dxgi_format,
        open_bind,
        open_misc,
        a.hKMResource.handle,
        ident.resource_id,
        venus_alloc_size,
        ident.memory_type_index,
        ident.global_vidmm_tracker().map_or(0, |tracker| {
            (u64::from(tracker.cookie) << 32) | u64::from(tracker.global_share)
        }),
        false,
        false,
        cross_context_optimal,
        dedicated_present_buffer,
    );
    if opened.is_none() {
        // Import of a KMD-validated-live resource failed: a real bug, not a
        // condition to paper over with substitute content (audit C1.3).
        log_error!(
            "DDI open_resource FAILED: ddi-shared import {}x{} d3dfmt={} alloc=0x{:x} hKM={:?} res_id={} -> E_FAIL",
            meta.width, meta.height, meta.format, allocation, a.hKMResource, ident.resource_id
        );
        set_runtime_error(h, E_FAIL);
        return;
    }

    let Some(res) = opened else {
        return;
    };
    let raw = res.as_raw() as usize;
    stamp_dxvk_resource_kmt_handles(h, &res, allocation, a.hKMResource.handle);
    let resident = match unsafe { make_resident(dev, allocation) } {
        Ok(resident) => resident,
        Err(hr) => {
            log_error!(
                "DDI open_resource FAILED: MakeResident alloc=0x{:x} hr=0x{:08x}",
                allocation,
                hr as u32
            );
            set_runtime_error(h, hr);
            return;
        }
    };
    log_error!(
        "DDI open_resource ddi-shared ok: {}x{} d3dfmt={} alloc=0x{:x} hKM={:?} raw=0x{:x}",
        meta.width,
        meta.height,
        meta.format,
        allocation,
        a.hKMResource,
        raw
    );
    let snapshot_source = (ident.resource_id != 0
        && venus_alloc_size != 0
        && meta.width != 0
        && meta.height != 0
        && open_dxgi_format != 0)
        .then_some(SnapshotSourceDesc {
            resource_id: ident.resource_id,
            venus_alloc_size,
            memory_type_index: ident.memory_type_index,
            width: meta.width,
            height: meta.height,
            dxgi_format: open_dxgi_format,
        });
    store_resource(
        h_resource,
        res,
        Some(resident),
        a.hKMResource.handle,
        h_rt.handle,
        AllocationOwnership::OpenedByRuntime,
        empty_present_private(),
        snapshot_source,
    );
}

pub(crate) unsafe extern "C" fn calc_size_opened_resource(
    _h: Hdevice,
    _arg: *const ddi::D3D10DDIARG_OPENRESOURCE,
) -> u64 {
    8
}

pub(crate) unsafe extern "C" fn resolve_shared_resource(
    h: ddi::HANDLE,
    arg: *const ddi::D3DDDIARG_RESOLVESHAREDRESOURCE,
) -> i32 {
    let h_resource: *mut c_void = if arg.is_null() {
        core::ptr::null_mut()
    } else {
        (*arg).hResource as *mut c_void
    };
    let resource = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: h_resource,
    };
    let alloc = resource_allocation(resource);
    let (width, height) = resource_dimensions(resource);
    log_error!(
        "DDI ResolveSharedResource: hDevice={:p} hResource={:p} alloc=0x{:x} {}x{}",
        h,
        h_resource,
        alloc,
        width,
        height
    );
    if let Some(context) = d3d11_context(Hdevice {
        pDrvPrivate: h as *mut c_void,
    }) {
        context.Flush();
    }
    0
}

pub(crate) unsafe extern "C" fn dxgi_resolve_shared_resource(
    arg: *mut ddi::DXGI_DDI_ARG_RESOLVESHAREDRESOURCE,
) -> i32 {
    let (h_device, h_resource): (usize, usize) = if arg.is_null() {
        (0, 0)
    } else {
        ((*arg).hDevice as usize, (*arg).hResource as usize)
    };
    let resource = dxgi_resource_handle(h_resource as ddi::DXGI_DDI_HRESOURCE);
    let alloc = resource_allocation(resource);
    let (width, height) = resource_dimensions(resource);
    trace_line!(
        "DXGI ResolveSharedResource: hDevice=0x{:x} hResource=0x{:x} alloc=0x{:x} {}x{}",
        h_device,
        h_resource,
        alloc,
        width,
        height
    );
    if let Some(context) = d3d11_context(Hdevice {
        pDrvPrivate: h_device as *mut c_void,
    }) {
        context.Flush();
    }
    0
}
