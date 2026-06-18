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

use bytemuck::pod_read_unaligned;
use helios_protocol::HeliosWddmAllocPrivate;

use crate::adapter::AdapterContext;
use crate::dxgk::*;

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

    // ── Create the backing virtio-gpu blob (create_blob + ctx_attach) ───────
    crate::diag::record(0x0C01_0010 | (ap.kind & 0xFF));
    let resource_id = match adapter
        .with_virtio(|v| v.resource_create_blob(ap.ctx_id, ap.blob_mem, ap.blob_flags, ap.blob_id, ap.size))
    {
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

    let size = round_up_page(if ap.size == 0 { PAGE } else { ap.size as SIZE_T });
    let ctx = Box::new(AllocationContext {
        ctx_id: ap.ctx_id,
        resource_id,
        blob_id: ap.blob_id,
        size,
        map_offset: 0,
        map_len: 0,
        mapped: false,
    });

    // ── VidMm metadata: a CPU-visible blob in segment 1 ─────────────────────
    info.hAllocation = Box::into_raw(ctx) as HANDLE;
    info.Size = size;
    info.PitchAlignedSize = size;
    info.SupportedWriteSegmentSet = 1; // segment id 1 (bit 0)
    info.EvictionSegmentSet = 0; // host-visible blob is pinned; never evicted
    unsafe {
        info.__bindgen_anon_1.Alignment = PAGE as UINT;
        info.__bindgen_anon_2.SupportedReadSegmentSet = 1;
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
                    let ctx =
                        unsafe { Box::from_raw(prev.hAllocation as *mut AllocationContext) };
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
        info.hDeviceSpecificAllocation = info.hAllocation as usize as HANDLE;
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
/// Filled with real metadata in Stage 3 (once submits reference allocations).
pub unsafe extern "C" fn dxgkddi_describe_allocation(
    _h_adapter: IN_CONST_HANDLE,
    _describe_allocation: INOUT_PDXGKARG_DESCRIBEALLOCATION,
) -> NTSTATUS {
    crate::diag::record(0x0C02_0001);
    STATUS_NOT_IMPLEMENTED
}

/// `DxgkDdiGetStandardAllocationDriverData` — describe a runtime "standard"
/// allocation (shared primary, staging surface, ...).
pub unsafe extern "C" fn dxgkddi_get_standard_allocation_driver_data(
    _h_adapter: IN_CONST_HANDLE,
    _standard_allocation: INOUT_PDXGKARG_GETSTANDARDALLOCATIONDRIVERDATA,
) -> NTSTATUS {
    crate::diag::record(0x0C02_0002);
    STATUS_NOT_IMPLEMENTED
}
