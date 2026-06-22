//! Allocation management DDIs (Gate 5a Stage 2).
//!
//! `DxgkDdiCreateAllocation` reads the ICD's `HeliosWddmAllocPrivate` (passed via
//! `D3DKMTCreateAllocation` per-allocation private driver data) and creates the
//! backing virtio-gpu HOST3D blob (`resource_create_blob` = create_blob +
//! ctx_attach), recording the `resource_id` so later stages can map it into the
//! host-visible window (`DxgkDdiBuildPagingBuffer`, Stage 2b) and tear it down
//! (`DxgkDdiDestroyAllocation`). See `GATE5_STAGE2_ALLOC_DESIGN.md`.
//!
//! TRUST BOUNDARY: `pPrivateDriverData` is ICD-supplied. `PrivateDriverDataSize`
//! is the only authoritative length; we bounds-check it against the struct size
//! before reading, validate the magic/version, and read with `pod_read_unaligned`.

use alloc::boxed::Box;
use core::ffi::c_void;
use core::mem::size_of;

use bytemuck::{pod_read_unaligned, Pod, Zeroable};
use helios_protocol::{
    HeliosWddmAllocPrivate, HELIOS_WDDM_ALLOC_KIND_STANDARD, VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE,
    VIRTIO_GPU_BLOB_MEM_HOST3D, VIRTIO_GPU_MAP_CACHE_CACHED,
};

use crate::adapter::AdapterContext;
use crate::dxgk::_D3DDDIFORMAT::D3DDDIFMT_A8R8G8B8;
use crate::dxgk::_D3DKMDT_STANDARDALLOCATION_TYPE::{
    D3DKMDT_STANDARDALLOCATION_GDISURFACE, D3DKMDT_STANDARDALLOCATION_SHADOWSURFACE,
    D3DKMDT_STANDARDALLOCATION_SHAREDPRIMARYSURFACE, D3DKMDT_STANDARDALLOCATION_STAGINGSURFACE,
};
use crate::dxgk::*;

/// KMD-private trailer appended after [`HeliosWddmAllocPrivate`] for a
/// `HELIOS_WDDM_ALLOC_KIND_STANDARD` allocation. The KMD writes it in
/// `DxgkDdiGetStandardAllocationDriverData` (from the runtime's surface data) and
/// reads it back in `DxgkDdiCreateAllocation` to populate the allocation's
/// `DxgkDdiDescribeAllocation` metadata. Both ends are the KMD, so this layout is
/// private (it never crosses to the ICD/UMD, which only sends the 48-byte prefix).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct StandardAllocMeta {
    width: u32,
    height: u32,
    format: u32, // D3DDDIFORMAT
    pitch: u32,
}

/// Per-allocation KMD state: the venus context + virtio resource backing it, plus
/// the host-visible window mapping (filled in Stage 2b by BuildPagingBuffer).
struct AllocationContext {
    ctx_id: u32,
    resource_id: u32,
    blob_id: u64,
    size: SIZE_T,
    /// Host-visible window byte offset this blob is mapped at (Stage 2b).
    map_offset: u64,
    /// Page-rounded mapped length (Stage 2b).
    map_len: u64,
    /// `true` once `RESOURCE_MAP_BLOB` has succeeded (so destroy unmaps first).
    mapped: bool,
    /// Surface geometry for `DxgkDdiDescribeAllocation` (0 for UMD blob allocations
    /// that carry no dimensions). Populated from the standard-allocation trailer.
    width: u32,
    height: u32,
    format: u32, // D3DDDIFORMAT
}

const PAGE: SIZE_T = 4096;

fn round_up_page(n: SIZE_T) -> SIZE_T {
    n.saturating_add(PAGE - 1) & !(PAGE - 1)
}

/// Tear down one blob allocation: unmap (if mapped) → detach → unref → free the
/// KMD context. Best-effort on the virtio ops (teardown must not get stuck).
unsafe fn destroy_allocation_ctx(adapter: &AdapterContext, ctx: Box<AllocationContext>) {
    if ctx.resource_id != 0 {
        let _ = adapter.with_virtio(|v| {
            if ctx.mapped {
                let _ = v.resource_unmap_blob(ctx.resource_id);
            }
            let _ = v.ctx_detach_resource(ctx.ctx_id, ctx.resource_id);
            v.resource_unref(ctx.resource_id)
        });
    }
    drop(ctx);
}

/// Create the virtio blob for one allocation and fill its VidMm metadata. On
/// failure nothing is stored (the caller unwinds prior allocations).
unsafe fn create_one(
    adapter: &AdapterContext,
    info: &mut DXGK_ALLOCATIONINFO,
) -> Result<(), NTSTATUS> {
    // ── Read + validate the ICD's private driver data ───────────────────────
    let priv_ptr = info.pPrivateDriverData as *const u8;
    let priv_len = info.PrivateDriverDataSize as usize;
    if priv_ptr.is_null() || priv_len < size_of::<HeliosWddmAllocPrivate>() {
        crate::diag::record(0x0C01_0002);
        return Err(STATUS_INVALID_PARAMETER);
    }
    // SAFETY: bounds-checked above; the runtime guarantees `priv_len` bytes at
    // `priv_ptr`. Read unaligned — the buffer carries no alignment guarantee.
    let priv_bytes =
        unsafe { core::slice::from_raw_parts(priv_ptr, size_of::<HeliosWddmAllocPrivate>()) };
    let ap: HeliosWddmAllocPrivate = pod_read_unaligned(priv_bytes);
    if !ap.is_valid() {
        crate::diag::record(0x0C01_0003);
        return Err(STATUS_INVALID_PARAMETER);
    }

    // For a KMD-originated standard allocation (DWM/IddCx composition surface), the
    // private data carries a geometry trailer the KMD itself wrote in
    // GetStandardAllocationDriverData, and `ctx_id` is the KMD's internal venus
    // context — which must be live (it is at Code 0; defensive otherwise).
    let mut meta = StandardAllocMeta::zeroed();
    if ap.kind == HELIOS_WDDM_ALLOC_KIND_STANDARD {
        if ap.ctx_id == 0 {
            crate::diag::record(0x0C01_00E2);
            return Err(STATUS_DEVICE_NOT_READY);
        }
        let want = size_of::<HeliosWddmAllocPrivate>() + size_of::<StandardAllocMeta>();
        if priv_len >= want {
            // SAFETY: bounds-checked above; the trailer follows the 48-byte prefix.
            let meta_bytes = unsafe {
                core::slice::from_raw_parts(
                    priv_ptr.add(size_of::<HeliosWddmAllocPrivate>()),
                    size_of::<StandardAllocMeta>(),
                )
            };
            meta = pod_read_unaligned(meta_bytes);
        }
    }

    // ── Create the backing virtio-gpu blob (create_blob + ctx_attach) ───────
    crate::diag::record(0x0C01_0010 | (ap.kind & 0xFF));
    let resource_id = match adapter.with_virtio(|v| {
        v.resource_create_blob(ap.ctx_id, ap.blob_mem, ap.blob_flags, ap.blob_id, ap.size)
    }) {
        Ok(Ok(rid)) => rid,
        Ok(Err(_ve)) => {
            // Host rejected the blob (e.g. the .56 blob_id=0 RESP_ERR_UNSPEC case).
            crate::diag::record(0x0C01_00E0);
            return Err(STATUS_UNSUCCESSFUL);
        }
        Err(_de) => {
            crate::diag::record(0x0C01_00E1);
            return Err(STATUS_DEVICE_NOT_READY);
        }
    };
    crate::diag::record(0x0C01_0020);
    crate::diag::record(resource_id);

    let size = round_up_page(if ap.size == 0 {
        PAGE
    } else {
        ap.size as SIZE_T
    });
    let ctx = Box::new(AllocationContext {
        ctx_id: ap.ctx_id,
        resource_id,
        blob_id: ap.blob_id,
        size,
        map_offset: 0,
        map_len: 0,
        mapped: false,
        width: meta.width,
        height: meta.height,
        format: meta.format,
    });

    // ── VidMm metadata: a CPU-visible blob in the memory segment ────────────
    info.hAllocation = Box::into_raw(ctx) as HANDLE;
    info.Size = size;
    info.PitchAlignedSize = size;
    let segment_bit = 1u32 << (crate::ddi::gpummu::MEMORY_SEGMENT_ID - 1);
    info.SupportedWriteSegmentSet = segment_bit;
    info.EvictionSegmentSet = 0; // host-visible blob is pinned; never evicted
    unsafe {
        info.__bindgen_anon_1.Alignment = PAGE as UINT;
        info.PreferredSegment
            .__bindgen_anon_1
            .__bindgen_anon_1
            .set_SegmentId0(crate::ddi::gpummu::MEMORY_SEGMENT_ID);
        info.__bindgen_anon_2.SupportedReadSegmentSet = segment_bit;
        info.__bindgen_anon_3.MaximumRenamingListLength = 0;
        info.__bindgen_anon_4
            .FlagsWddm2
            .__bindgen_anon_1
            .__bindgen_anon_1
            .set_CpuVisible(1);
    }
    Ok(())
}

pub unsafe extern "C" fn dxgkddi_create_allocation(
    h_adapter: *mut c_void,
    create_allocation: *mut DXGKARG_CREATEALLOCATION,
) -> NTSTATUS {
    if h_adapter.is_null() || create_allocation.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    crate::diag::record(0x0C01_0001);
    // SAFETY: Dxgkrnl passes our adapter context and a valid args struct.
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let args = unsafe { &mut *create_allocation };
    if args.NumAllocations == 0 || args.pAllocationInfo.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    for i in 0..args.NumAllocations as usize {
        // SAFETY: pAllocationInfo points to NumAllocations elements.
        let info = unsafe { &mut *args.pAllocationInfo.add(i) };
        if let Err(status) = unsafe { create_one(adapter, info) } {
            // Unwind the allocations already created in this call.
            for j in 0..i {
                let prev = unsafe { &mut *args.pAllocationInfo.add(j) };
                if !prev.hAllocation.is_null() {
                    let ctx = unsafe { Box::from_raw(prev.hAllocation as *mut AllocationContext) };
                    unsafe { destroy_allocation_ctx(adapter, ctx) };
                    prev.hAllocation = core::ptr::null_mut();
                }
            }
            return status;
        }
    }

    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_destroy_allocation(
    h_adapter: *mut c_void,
    destroy_allocation: *const DXGKARG_DESTROYALLOCATION,
) -> NTSTATUS {
    if h_adapter.is_null() || destroy_allocation.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let args = unsafe { &*destroy_allocation };
    if args.NumAllocations != 0 && args.pAllocationList.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    for i in 0..args.NumAllocations as usize {
        let handle = unsafe { *args.pAllocationList.add(i) };
        if !handle.is_null() {
            let ctx = unsafe { Box::from_raw(handle as *mut AllocationContext) };
            unsafe { destroy_allocation_ctx(adapter, ctx) };
        }
    }

    STATUS_SUCCESS
}

// ── Allocation lifetime DDIs. ───────────────────────────────────────────────

/// `DxgkDdiOpenAllocation` — bind a device to allocations. dxgkrnl calls this for
/// EVERY allocation (including ones the same device just created via
/// `CreateAllocation`, not only cross-process opens), so it must succeed or
/// `D3DKMTCreateAllocation` fails with the open status. For each open-info entry
/// we set a non-null device-specific handle. We don't reference allocations from
/// the submission path yet (Stage 3), so echo the dxgkrnl global handle as the
/// device-local handle; Stage 3 maps it to the `AllocationContext`.
pub unsafe extern "C" fn dxgkddi_open_allocation(
    _h_device: IN_CONST_HANDLE,
    open_allocation: IN_CONST_PDXGKARG_OPENALLOCATION,
) -> NTSTATUS {
    crate::diag::record(0x0C02_0003);
    if open_allocation.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: valid per the DDI contract; `pOpenAllocation` is a `*mut` array of
    // `NumAllocations` entries whose `hDeviceSpecificAllocation` we fill.
    let args = unsafe { &*open_allocation };
    if args.NumAllocations != 0 && args.pOpenAllocation.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    for i in 0..args.NumAllocations as usize {
        let info = unsafe { &mut *args.pOpenAllocation.add(i) };
        info.hDeviceSpecificAllocation = if info.hAllocation == 0 {
            // VidMm also opens implicit/internal allocations that are not backed
            // by a KMD AllocationContext. Returning a null device-specific handle
            // leaves dxgmms2's internal VIDMM_ALLOC output null while still
            // treating the open as successful, which later AVs in DMA-pool init.
            // Use the open-info slot address as a stable non-null token for this
            // open; the null paging/render engine never dereferences it.
            (info as *mut DXGK_OPENALLOCATIONINFO).cast()
        } else {
            info.hAllocation as usize as HANDLE
        };
    }
    STATUS_SUCCESS
}

/// `DxgkDdiCloseAllocation` — release device-local allocation references.
pub unsafe extern "C" fn dxgkddi_close_allocation(
    _h_device: IN_CONST_HANDLE,
    _close_allocation: IN_CONST_PDXGKARG_CLOSEALLOCATION,
) -> NTSTATUS {
    STATUS_SUCCESS
}

/// `DxgkDdiDescribeAllocation` — report an allocation's dimensions/format.
///
/// dxgkrnl calls this for shared / cross-process surfaces (and DWM's composition
/// surfaces) to learn their geometry. We echo the geometry recorded at
/// CreateAllocation time (from the standard-allocation trailer). UMD blob
/// allocations carry no dimensions (0×0); report them as-is.
pub unsafe extern "C" fn dxgkddi_describe_allocation(
    h_adapter: IN_CONST_HANDLE,
    describe_allocation: INOUT_PDXGKARG_DESCRIBEALLOCATION,
) -> NTSTATUS {
    crate::diag::record(0x0C02_0001);
    if h_adapter.is_null() || describe_allocation.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl passes a writable DXGKARG_DESCRIBEALLOCATION.
    let args = unsafe { &mut *describe_allocation };
    if args.hAllocation.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: hAllocation is the AllocationContext pointer we returned from
    // CreateAllocation; dxgkrnl round-trips it back unmodified.
    let ctx = unsafe { &*(args.hAllocation as *const AllocationContext) };

    args.Width = ctx.width;
    args.Height = ctx.height;
    // Default a plausible BGRA format for dimensionless UMD blobs so dxgkrnl never
    // sees D3DDDIFMT_UNKNOWN(0) for a describable allocation.
    args.Format = if ctx.format != 0 {
        ctx.format as D3DDDIFORMAT
    } else {
        D3DDDIFMT_A8R8G8B8
    };
    args.MultisampleMethod.NumSamples = 1;
    args.MultisampleMethod.NumQualityLevels = 1;
    args.RefreshRate.Numerator = 60;
    args.RefreshRate.Denominator = 1;
    args.PrivateDriverFormatAttribute = 0;
    STATUS_SUCCESS
}

/// `DxgkDdiGetStandardAllocationDriverData` — describe a runtime "standard"
/// allocation (shared primary, shadow, staging, GDI surface). DWM and IddCx use
/// these for the desktop composition surfaces.
///
/// Two-call contract (viogpu3d `viogpu_allocation.cpp:135` is the template):
///   1. **Size query** — `pAllocationPrivateDriverData == NULL`: report the byte
///      sizes the runtime must allocate for the per-allocation / per-resource
///      private data.
///   2. **Fill** — buffers provided: write the private data the runtime then hands
///      to `DxgkDdiCreateAllocation`, and fill the surface `Pitch` out-fields.
///
/// We fill a [`HeliosWddmAllocPrivate`] (`kind = STANDARD`, `blob_id = 0` so the
/// host allocates a HOST3D mappable blob, `ctx_id` = the KMD's internal venus
/// context) plus a [`StandardAllocMeta`] geometry trailer.
pub unsafe extern "C" fn dxgkddi_get_standard_allocation_driver_data(
    h_adapter: IN_CONST_HANDLE,
    standard_allocation: INOUT_PDXGKARG_GETSTANDARDALLOCATIONDRIVERDATA,
) -> NTSTATUS {
    if h_adapter.is_null() || standard_allocation.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl hands back our AdapterContext and a writable args struct.
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let args = unsafe { &mut *standard_allocation };
    crate::diag::record(0x0C02_0002 | ((args.StandardAllocationType as u32 & 0xFF) << 4));

    const PRIV_SIZE: u32 =
        (size_of::<HeliosWddmAllocPrivate>() + size_of::<StandardAllocMeta>()) as u32;

    // ── Phase 1: size query (runtime passes a null allocation buffer) ────────
    if args.pAllocationPrivateDriverData.is_null() {
        args.AllocationPrivateDriverDataSize = PRIV_SIZE;
        // We read no per-resource private data in CreateAllocation.
        args.ResourcePrivateDriverDataSize = 0;
        return STATUS_SUCCESS;
    }
    if (args.AllocationPrivateDriverDataSize as usize) < PRIV_SIZE as usize {
        return STATUS_INVALID_PARAMETER;
    }

    // ── Phase 2: extract geometry from the per-type union; set out Pitch ─────
    // SAFETY: the union arm is selected by StandardAllocationType; dxgkrnl
    // guarantees the matching surface-data pointer is valid for the fill call.
    let (width, height, format): (u32, u32, u32) = match args.StandardAllocationType {
        D3DKMDT_STANDARDALLOCATION_SHAREDPRIMARYSURFACE => {
            let sd = unsafe { &*args.__bindgen_anon_1.pCreateSharedPrimarySurfaceData };
            (sd.Width, sd.Height, sd.Format as u32)
        }
        D3DKMDT_STANDARDALLOCATION_SHADOWSURFACE => {
            let sd = unsafe { &mut *args.__bindgen_anon_1.pCreateShadowSurfaceData };
            sd.Pitch = sd.Width.saturating_mul(4);
            (sd.Width, sd.Height, sd.Format as u32)
        }
        D3DKMDT_STANDARDALLOCATION_STAGINGSURFACE => {
            let sd = unsafe { &mut *args.__bindgen_anon_1.pCreateStagingSurfaceData };
            sd.Pitch = sd.Width.saturating_mul(4);
            (sd.Width, sd.Height, D3DDDIFMT_A8R8G8B8 as u32)
        }
        D3DKMDT_STANDARDALLOCATION_GDISURFACE => {
            let sd = unsafe { &mut *args.__bindgen_anon_1.pCreateGdiSurfaceData };
            sd.Pitch = sd.Width.saturating_mul(4);
            (sd.Width, sd.Height, sd.Format as u32)
        }
        _ => {
            crate::diag::record(0x0C02_00E2);
            return STATUS_NOT_SUPPORTED;
        }
    };

    let pitch = width.saturating_mul(4);
    let size = (pitch as u64)
        .saturating_mul(height as u64)
        .max(PAGE as u64);

    let ap = HeliosWddmAllocPrivate::new(
        HELIOS_WDDM_ALLOC_KIND_STANDARD,
        adapter.venus_ctx_id,
        0, // blob_id 0 → host-allocated HOST3D mappable blob (no UMD venus memory)
        size,
        VIRTIO_GPU_BLOB_MEM_HOST3D,
        VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE,
        VIRTIO_GPU_MAP_CACHE_CACHED,
    );
    let meta = StandardAllocMeta {
        width,
        height,
        format,
        pitch,
    };

    // SAFETY: AllocationPrivateDriverDataSize bytes (>= PRIV_SIZE) are writable.
    let dst = args.pAllocationPrivateDriverData as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(
            &ap as *const _ as *const u8,
            dst,
            size_of::<HeliosWddmAllocPrivate>(),
        );
        core::ptr::copy_nonoverlapping(
            &meta as *const _ as *const u8,
            dst.add(size_of::<HeliosWddmAllocPrivate>()),
            size_of::<StandardAllocMeta>(),
        );
    }
    args.AllocationPrivateDriverDataSize = PRIV_SIZE;
    crate::diag::record(0x0C02_0005);
    STATUS_SUCCESS
}
