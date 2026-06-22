//! Per-D3D-device, per-context, and per-process state, plus their DDIs.
//!
//! Phase 1 implements device alloc/free (so the runtime can open a device
//! without crashing). Context and GPU-VA process DDIs are stubbed until the
//! Venus path (Phase 4) and the memory model (Phase 3) land.
//!
//! NOTE: the exact argument struct/handle types below come from the generated
//! `dxgk` bindings and may need a binding-alignment pass at first compile.

use alloc::boxed::Box;
use core::ffi::c_void;

use crate::adapter::AdapterContext;
use crate::dxgk::*;

/// State for one D3D device opened on the adapter.
pub struct DeviceContext {
    /// Back-pointer to the owning adapter (valid for the device's lifetime).
    pub adapter: *mut AdapterContext,
}

/// State for one scheduler context opened on a D3D device.
pub struct ContextContext {
    /// Back-pointer to the owning device (valid for the context's lifetime).
    pub device: *mut DeviceContext,
}

/// State for one GPU process object (WDDM 2.0 GPU-VA requirement). We keep no
/// per-process GPU virtual address space (host-owned VA), but dxgkrnl requires a
/// non-NULL driver handle it can round-trip through every per-process DDI and
/// hand back at DestroyProcess, so we allocate a real object to back the handle.
pub struct ProcessContext {
    /// Back-pointer to the owning adapter (valid for the process's lifetime).
    pub adapter: *mut AdapterContext,
}

/// `DxgkDdiCreateDevice` — allocate per-device state.
pub unsafe extern "C" fn dxgkddi_create_device(
    miniport_device_context: *mut c_void,
    create_device: *mut DXGKARG_CREATEDEVICE,
) -> NTSTATUS {
    if miniport_device_context.is_null() || create_device.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: Dxgkrnl passes our adapter context and a valid args struct.
    let args = unsafe { &mut *create_device };
    let ctx = Box::new(DeviceContext {
        adapter: miniport_device_context as *mut AdapterContext,
    });
    // Hand the device handle back to Dxgkrnl; reclaimed in destroy_device.
    args.hDevice = Box::into_raw(ctx) as *mut c_void;
    STATUS_SUCCESS
}

/// `DxgkDdiDestroyDevice` — free per-device state, after unmapping any host-visible
/// blob views this device opened (Gate 5a Stage 2b). The user VAs were mapped by
/// `HELIOS_ESCAPE_MAP_BLOB` (tagged with this `h_device` as owner) and MUST be
/// unmapped here — in the creating process, at PASSIVE_LEVEL — or the kernel
/// bugchecks `0x76 PROCESS_HAS_LOCKED_PAGES` at process exit. This DDI runs in the
/// context of the thread destroying the device (the ICD's process), so the unmap is
/// in-process. The mapping table is on the AdapterContext (independent spinlock), so
/// teardown is correct even if the virtio transport is already gone.
pub unsafe extern "C" fn dxgkddi_destroy_device(h_device: *mut c_void) -> NTSTATUS {
    if !h_device.is_null() {
        // SAFETY: h_device came from Box::into_raw in create_device; its `adapter`
        // back-pointer is valid for the device's lifetime.
        let dev = unsafe { &*(h_device as *mut DeviceContext) };
        let adapter = unsafe { &*dev.adapter };
        let owner = h_device as usize;
        // Drain THIS device's mappings one at a time, unmapping outside the table
        // lock (MmUnmapLockedPages needs PASSIVE; the table lock raises to DISPATCH).
        while let Some((user_va, mdl)) = adapter.mappings.take_one_for(owner) {
            // SAFETY: PASSIVE_LEVEL in the creating process; pair from a prior
            // MAP_BLOB on this device handle.
            unsafe { crate::ddi::unmap_io_pages_from_user(user_va, mdl as *mut wdk_sys::MDL) };
        }
        // Reclaim any virtio blobs / contexts this device allocated but did not
        // release (ICD crash, or a process that skipped RELEASE_BLOB/CTX_DESTROY —
        // e.g. a crash-looping client). Without this the bounded blob table fills
        // across device creations and later ALLOC_BLOBs fail STATUS_INSUFFICIENT_
        // RESOURCES, surfacing as spurious "venus wedge" / render corruption. If
        // the transport is already gone (StopDevice), there is nothing to reclaim.
        // DIAG: 0x0E00_0001 = DestroyDevice entry (unconditional). Low 16 bits of
        // the owning handle so we can correlate with ALLOC_BLOB's owner.
        crate::diag::record(0x0E00_0001);
        crate::diag::record(0x0E01_0000 | ((owner as u32) & 0xFFFF));
        // Mirror the GpuMmu page-table-DDI tracers into the PASSIVE ring so the
        // post-CreateContext Code-43 failure stage is visible over SSH without
        // ntoseye (Step-2 decorative-GpuMmu bring-up).
        crate::ddi::diag_dump_gpummu_atomics();
        // Engine-path tracers (SubmitCommand/Render/Patch/ISR/DPC counts): show
        // whether VidSch exercised the submission engine at all before the
        // post-CreateContext VidSchTerminateAdapter Code-43 (Step-2 coherent fence).
        crate::ddi::diag_dump_engine_atomics();
        let _ = adapter.with_virtio(|v| {
            let before = v.blob_count() as u32;
            let blobs = v.release_blobs_for_owner(owner);
            let contexts = v.destroy_contexts_for_owner(owner);
            // 0x0E02_BBBB = blob-table size BEFORE reclaim (saturated to 16 bits).
            crate::diag::record(0x0E02_0000 | before.min(0xFFFF));
            // 0x0E03_RRCC = reclaimed blobs (RR) + contexts (CC).
            crate::diag::record(0x0E03_0000 | ((blobs.min(0xFF) << 8) | contexts.min(0xFF)));
        });
        // SAFETY: produced by Box::into_raw in create_device; destroyed exactly once.
        drop(unsafe { Box::from_raw(h_device as *mut DeviceContext) });
    }
    STATUS_SUCCESS
}

/// `DxgkDdiCreateContext` — GPU execution context.
///
// STUB: Phase 4 — create the Venus virtio-gpu context here.
// NOTE: a TEMP int3 debug breakpoint lived here during the Step-2 GpuMmu bring-up
// and was removed (ntoseye is a gdbstub, not a Windows KD — a bare int3 BSODs the
// guest instead of trapping). Use a spin-loop released via ntoseye write_memory if
// a live pause is needed. This comment also forces a clean recompile.
pub unsafe extern "C" fn dxgkddi_create_context(
    h_device: *mut c_void,
    create_context: *mut DXGKARG_CREATECONTEXT,
) -> NTSTATUS {
    crate::diag::record(0x0800_0001);
    if h_device.is_null() || create_context.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    let args = unsafe { &mut *create_context };
    let ctx = Box::new(ContextContext {
        device: h_device as *mut DeviceContext,
    });
    args.hContext = Box::into_raw(ctx) as HANDLE;

    // Use the paging aperture for DMA buffers. With the decorative GpuMmu model,
    // dxgkrnl's CDD context creates a privileged DMA pool with GPU-VA mapping
    // enabled; if this is 0, dxgmms2 uses contiguous system memory, skips creating
    // a VIDMM allocation object, then later dereferences that null allocation in
    // VidMmInitDmaPool. A nonzero aperture segment set makes VidMm back the pool
    // through the normal aperture allocation path.
    args.ContextInfo.DmaBufferSegmentSet = 1; // segment id 1 (aperture)
    args.ContextInfo.DmaBufferSize = 256 * 1024;
    args.ContextInfo.DmaBufferPrivateDataSize = 40;
    args.ContextInfo.AllocationListSize = DXGK_ALLOCATION_LIST_SIZE_GDICONTEXT;
    args.ContextInfo.PatchLocationListSize = DXGK_ALLOCATION_LIST_SIZE_GDICONTEXT;

    STATUS_SUCCESS
}

/// `DxgkDdiDestroyContext`.
// STUB: Phase 4 — tear down the Venus context.
pub unsafe extern "C" fn dxgkddi_destroy_context(h_context: *mut c_void) -> NTSTATUS {
    crate::diag::record(0x0800_0002);
    if !h_context.is_null() {
        drop(unsafe { Box::from_raw(h_context as *mut ContextContext) });
    }
    STATUS_SUCCESS
}

/// `DxgkDdiCreateProcess` — GPU-VA process object (WDDM 2.0 requirement).
///
/// dxgkrnl creates a process object during GPU-VA adapter bring-up and expects a
/// non-NULL driver handle back in `hKmdProcess`; leaving this a
/// `STATUS_NOT_IMPLEMENTED` stub fails post-StartDevice (one of the Code-43
/// triggers). We allocate a `ProcessContext`, hand its pointer back as the
/// handle, and reclaim it in DestroyProcess. No GPU virtual address space is
/// tracked (host-owned VA — see build_paging_buffer.rs).
pub unsafe extern "C" fn dxgkddi_create_process(
    miniport_device_context: *mut c_void,
    args: *mut DXGKARG_CREATEPROCESS,
) -> NTSTATUS {
    // DIAG: confirm dxgkrnl reaches CreateProcess during AddAdapter.
    crate::diag::record(0x0600_0000);
    if miniport_device_context.is_null() || args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: Dxgkrnl passes our adapter context and a valid args struct.
    let args = unsafe { &mut *args };
    let ctx = Box::new(ProcessContext {
        adapter: miniport_device_context as *mut AdapterContext,
    });
    // Hand the process handle back to Dxgkrnl; reclaimed in destroy_process.
    args.hKmdProcess = Box::into_raw(ctx) as HANDLE;
    STATUS_SUCCESS
}

/// `DxgkDdiDestroyProcess` — free the per-process state from CreateProcess.
pub unsafe extern "C" fn dxgkddi_destroy_process(
    _miniport_device_context: *mut c_void,
    h_process: *mut c_void,
) -> NTSTATUS {
    if !h_process.is_null() {
        // SAFETY: h_process was produced by Box::into_raw in create_process and
        // is destroyed exactly once.
        drop(unsafe { Box::from_raw(h_process as *mut ProcessContext) });
    }
    STATUS_SUCCESS
}
