//! `DxgkDdiQueryAdapterInfo` — report adapter capabilities.
//!
//! Gate-1 render-only adapter capability reporting.
//!
//! This deliberately reports only base driver caps until the segment, GPU-VA,
//! paging, and scheduler paths are real. No fake aperture/GPU-MMU caps.
//!
//! Reference: https://learn.microsoft.com/windows-hardware/drivers/ddi/d3dkmddi/nc-d3dkmddi-dxgkddi_queryadapterinfo

use core::ffi::c_void;
use core::mem::size_of;

use crate::dxgk::_D3DKMDT_COMPUTE_PREEMPTION_GRANULARITY::D3DKMDT_COMPUTE_PREEMPTION_DMA_BUFFER_BOUNDARY;
use crate::dxgk::_D3DKMDT_GRAPHICS_PREEMPTION_GRANULARITY::D3DKMDT_GRAPHICS_PREEMPTION_DMA_BUFFER_BOUNDARY;
use crate::dxgk::_DXGK_QUERYADAPTERINFOTYPE::{
    DXGKQAITYPE_64BITONLYCAPS, DXGKQAITYPE_ADAPTERPERFDATA_CAPS, DXGKQAITYPE_DIRTYBITTRACKINGCAPS,
    DXGKQAITYPE_DRIVERCAPS, DXGKQAITYPE_GPUVERSION, DXGKQAITYPE_HARDWARERESERVEDRANGES2,
    DXGKQAITYPE_HISTORYBUFFERPRECISION, DXGKQAITYPE_IOMMU_CAPS, DXGKQAITYPE_PHYSICAL_MEMORY_CAPS,
    DXGKQAITYPE_QUERYSEGMENT4, DXGKQAITYPE_WDDMDEVICECAPS,
};
use crate::dxgk::_DXGK_WDDMVERSION::DXGKDDI_WDDMv3_2;
use crate::dxgk::*;

pub unsafe extern "C" fn dxgkddi_query_adapter_info(
    miniport_device_context: *mut c_void,
    query_adapter_info: *const DXGKARG_QUERYADAPTERINFO,
) -> NTSTATUS {
    if miniport_device_context.is_null() || query_adapter_info.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: valid per the DDI contract; we only read the args struct.
    let args = unsafe { &*query_adapter_info };

    // DIAG: log every QueryAdapterInfo type dxgkrnl requests during AddAdapter.
    crate::diag::record(0x0100_0000 | (args.Type as u32 & 0xFFFF));

    match args.Type {
        DXGKQAITYPE_DRIVERCAPS => unsafe { query_driver_caps(args) },
        DXGKQAITYPE_QUERYSEGMENT4 => unsafe { query_segments(args) },
        DXGKQAITYPE_WDDMDEVICECAPS => unsafe { query_wddm_device_caps(args) },
        DXGKQAITYPE_PHYSICAL_MEMORY_CAPS => unsafe { query_physical_memory_caps(args) },
        DXGKQAITYPE_IOMMU_CAPS => unsafe { query_zeroed::<DXGK_IOMMU_CAPS>(args) },
        DXGKQAITYPE_HARDWARERESERVEDRANGES2 => unsafe {
            query_zeroed::<DXGK_HARDWARERESERVEDRANGES>(args)
        },
        DXGKQAITYPE_GPUVERSION => unsafe { query_gpu_version(args) },
        DXGKQAITYPE_ADAPTERPERFDATA_CAPS => unsafe {
            query_zeroed::<DXGK_ADAPTER_PERFDATACAPS>(args)
        },
        DXGKQAITYPE_DIRTYBITTRACKINGCAPS => unsafe {
            query_zeroed::<DXGK_DIRTY_BIT_TRACKING_CAPS>(args)
        },
        DXGKQAITYPE_HISTORYBUFFERPRECISION => unsafe { query_history_buffer_precision(args) },
        DXGKQAITYPE_64BITONLYCAPS => unsafe { query_zeroed::<DXGK_64_BIT_ONLY_CAPS>(args) },
        // Everything else stays unsupported until backed by real implementation
        // (checklist rule: "unknown must stay unadvertised"). Notably dxgkrnl
        // steady-state-polls NODEPERFDATA (0x18) and ADAPTERPERFDATA (0x19) to
        // feed the Task Manager GPU tab; virtio-gpu exposes no such telemetry, so
        // NOT_SUPPORTED is the honest answer here — an expected poll, not a gap.
        // PHYSICALADAPTERCAPS (0x0F) is also deferred: its DxgkPhysicalAdapterHandle
        // / Flags contract is undefined for us until we have real execution nodes.
        other => {
            // DIAG: which type we rejected (suspect if AddAdapter dies right after).
            crate::diag::record(0x0200_0000 | (other as u32 & 0xFFFF));
            STATUS_NOT_SUPPORTED
        }
    }
}

unsafe fn query_driver_caps(args: &DXGKARG_QUERYADAPTERINFO) -> NTSTATUS {
    if (args.OutputDataSize as usize) < size_of::<DXGK_DRIVERCAPS>() {
        return STATUS_BUFFER_TOO_SMALL;
    }
    // SAFETY: pOutputData points to a DXGK_DRIVERCAPS of sufficient size.
    let caps = unsafe { &mut *(args.pOutputData as *mut DXGK_DRIVERCAPS) };
    unsafe { core::ptr::write_bytes(caps as *mut _ as *mut u8, 0, size_of::<DXGK_DRIVERCAPS>()) };

    // 64-bit addressable; no render/memory features are advertised here yet.
    caps.HighestAcceptableAddress.QuadPart = -1;
    caps.MaxAllocationListSlotId = 0xFFFF;
    caps.ApertureSegmentCommitLimit = 64 * 1024 * 1024;
    // Not a legacy VGA device.
    caps.SupportNonVGA = 1;

    // Keep this in sync with DXGKQAITYPE_WDDMDEVICECAPS and DriverEntry's
    // DRIVER_INITIALIZATION_DATA.Version. Dxgkrnl rejects internally
    // inconsistent version/capability surfaces during AddAdapter.
    caps.WDDMVersion = DXGKDDI_WDDMv3_2;
    caps.PreemptionCaps.GraphicsPreemptionGranularity =
        D3DKMDT_GRAPHICS_PREEMPTION_DMA_BUFFER_BOUNDARY;
    caps.PreemptionCaps.ComputePreemptionGranularity =
        D3DKMDT_COMPUTE_PREEMPTION_DMA_BUFFER_BOUNDARY;
    caps.SupportPerEngineTDR = 1;

    // Required WDDM 1.2+ render-only caps. Bit positions verified field-by-field
    // against WDK 10.0.26100 `shared/d3dkmddi.h` (2026-06-18). `__bindgen_anon_1`
    // is the cap union; `.Value` is its UINT view of the bitfield struct, so a
    // named mask written to `.Value` is the stable, layout-independent way to set
    // a single documented bit.
    //
    //   DXGK_PRESENTATIONCAPS:      bit 2 = SupportKernelModeCommandBuffer
    //   DXGK_FLIPCAPS:              bit 1 = FlipOnVSyncMmIo
    //   DXGK_SCHEDULINGCAPS:        bit 0 = MultiEngineAware, bit 2 = PreemptionAware
    //   DXGK_MEMORYMANAGEMENTCAPS:  bit 3 = SectionBackedPrimary
    //
    // COHERENCE DEBT (tracked in WDDM_RENDER_ONLY_DDI_CHECKLIST.md): these are
    // MANDATORY for a WDDM 3.2 render-only adapter to load, but the paths behind
    // them are still the null bring-up engine — SupportKernelModeCommandBuffer
    // implies a working DxgkDdiRenderKm, FlipOnVSyncMmIo a real flip path,
    // PreemptionAware a preemptible scheduler. Gate 2/3 must make these real
    // (or stop advertising them); do not treat their presence as proof of support.
    // STEP-0 CAP BISECT RESULT (GATE2_3_CAPS_BACKING.md, 2026-06-18): dropping
    // FlipOnVSyncMmIo regresses to Code 43 even with SectionBackedPrimary present
    // (`.59` dropped Flip+SectionBackedPrimary → 43; `.60` re-added
    // SectionBackedPrimary, kept Flip dropped → still 43). So `FlipOnVSyncMmIo` is
    // MANDATORY for dxgkrnl render-adapter load even on a render-only adapter —
    // these caps are NOT over-advertised and cannot be honestly dropped; they
    // must be BACKED with real impl (Gate 2/3). Full mandatory set restored below.
    const PRESENTATIONCAPS_SUPPORT_KERNEL_MODE_COMMAND_BUFFER: u32 = 1 << 2;
    const FLIPCAPS_FLIP_ON_VSYNC_MMIO: u32 = 1 << 1;
    const SCHEDULINGCAPS_MULTI_ENGINE_AWARE: u32 = 1 << 0;
    const SCHEDULINGCAPS_PREEMPTION_AWARE: u32 = 1 << 2;
    const MEMORYMANAGEMENTCAPS_SECTION_BACKED_PRIMARY: u32 = 1 << 3;

    caps.PresentationCaps.__bindgen_anon_1.Value =
        PRESENTATIONCAPS_SUPPORT_KERNEL_MODE_COMMAND_BUFFER;
    caps.FlipCaps.__bindgen_anon_1.Value = FLIPCAPS_FLIP_ON_VSYNC_MMIO;
    caps.SchedulingCaps.__bindgen_anon_1.Value =
        SCHEDULINGCAPS_MULTI_ENGINE_AWARE | SCHEDULINGCAPS_PREEMPTION_AWARE;
    caps.MemoryManagementCaps.__bindgen_anon_1.Value =
        MEMORYMANAGEMENTCAPS_SECTION_BACKED_PRIMARY;
    caps.MaxQueuedFlipOnVSync = 1;
    caps.GpuEngineTopology.NbAsymetricProcessingNodes = 1;

    STATUS_SUCCESS
}

unsafe fn query_zeroed<T>(args: &DXGKARG_QUERYADAPTERINFO) -> NTSTATUS {
    if (args.OutputDataSize as usize) < size_of::<T>() {
        return STATUS_BUFFER_TOO_SMALL;
    }

    // SAFETY: pOutputData points to a writable object of the requested type.
    unsafe { core::ptr::write_bytes(args.pOutputData as *mut u8, 0, size_of::<T>()) };
    STATUS_SUCCESS
}

unsafe fn query_wddm_device_caps(args: &DXGKARG_QUERYADAPTERINFO) -> NTSTATUS {
    if (args.OutputDataSize as usize) < size_of::<DXGK_WDDMDEVICECAPS>() {
        return STATUS_BUFFER_TOO_SMALL;
    }

    // SAFETY: pOutputData points to a DXGK_WDDMDEVICECAPS of sufficient size.
    let caps = unsafe { &mut *(args.pOutputData as *mut DXGK_WDDMDEVICECAPS) };
    unsafe {
        core::ptr::write_bytes(
            caps as *mut _ as *mut u8,
            0,
            size_of::<DXGK_WDDMDEVICECAPS>(),
        );
    }
    caps.WDDMVersion = DXGKDDI_WDDMv3_2;
    STATUS_SUCCESS
}

unsafe fn query_physical_memory_caps(args: &DXGKARG_QUERYADAPTERINFO) -> NTSTATUS {
    if (args.OutputDataSize as usize) < size_of::<DXGK_PHYSICAL_MEMORY_CAPS>() {
        return STATUS_BUFFER_TOO_SMALL;
    }

    // SAFETY: pOutputData points to a DXGK_PHYSICAL_MEMORY_CAPS of sufficient size.
    let caps = unsafe { &mut *(args.pOutputData as *mut DXGK_PHYSICAL_MEMORY_CAPS) };
    unsafe {
        core::ptr::write_bytes(
            caps as *mut _ as *mut u8,
            0,
            size_of::<DXGK_PHYSICAL_MEMORY_CAPS>(),
        );
    }
    caps.HighestVisibleAddress.QuadPart = -1;
    STATUS_SUCCESS
}

unsafe fn query_gpu_version(args: &DXGKARG_QUERYADAPTERINFO) -> NTSTATUS {
    if (args.OutputDataSize as usize) < size_of::<DXGK_GPUVERSION>() {
        return STATUS_BUFFER_TOO_SMALL;
    }

    // SAFETY: pOutputData points to a DXGK_GPUVERSION of sufficient size.
    let version = args.pOutputData as *mut DXGK_GPUVERSION;
    unsafe {
        core::ptr::write_bytes(version as *mut u8, 0, size_of::<DXGK_GPUVERSION>());
        write_wchar_z_unaligned(
            core::ptr::addr_of_mut!((*version).BiosVersion) as *mut WCHAR,
            32,
            b"helios-virtio-gpu",
        );
        write_wchar_z_unaligned(
            core::ptr::addr_of_mut!((*version).GpuArchitecture) as *mut WCHAR,
            32,
            b"virtio-gpu",
        );
    }
    STATUS_SUCCESS
}

unsafe fn query_history_buffer_precision(args: &DXGKARG_QUERYADAPTERINFO) -> NTSTATUS {
    if (args.OutputDataSize as usize) < size_of::<DXGKARG_HISTORYBUFFERPRECISION>() {
        return STATUS_BUFFER_TOO_SMALL;
    }

    // SAFETY: pOutputData points to a DXGKARG_HISTORYBUFFERPRECISION of sufficient size.
    let precision = unsafe { &mut *(args.pOutputData as *mut DXGKARG_HISTORYBUFFERPRECISION) };
    unsafe {
        core::ptr::write_bytes(
            precision as *mut _ as *mut u8,
            0,
            size_of::<DXGKARG_HISTORYBUFFERPRECISION>(),
        );
    }
    // Dxgkrnl validates history-buffer timestamp precision during
    // ADAPTER_RENDER bring-up and accepts 32..64 bits.
    precision.PrecisionBits = 32;
    STATUS_SUCCESS
}

unsafe fn write_wchar_z_unaligned(dst: *mut WCHAR, dst_len: usize, ascii: &[u8]) {
    let len = core::cmp::min(dst_len.saturating_sub(1), ascii.len());
    for i in 0..len {
        unsafe { core::ptr::write_unaligned(dst.add(i), ascii[i] as WCHAR) };
    }
    if dst_len != 0 {
        unsafe { core::ptr::write_unaligned(dst.add(len), 0) };
    }
}

unsafe fn query_segments(args: &DXGKARG_QUERYADAPTERINFO) -> NTSTATUS {
    if (args.OutputDataSize as usize) < size_of::<DXGK_QUERYSEGMENTOUT4>() {
        return STATUS_BUFFER_TOO_SMALL;
    }

    // SAFETY: pOutputData points to a writable DXGK_QUERYSEGMENTOUT4 of
    // sufficient size, checked above.
    let out = unsafe { &mut *(args.pOutputData as *mut DXGK_QUERYSEGMENTOUT4) };
    let segment_descriptor = out.pSegmentDescriptor;
    unsafe {
        core::ptr::write_bytes(
            out as *mut _ as *mut u8,
            0,
            size_of::<DXGK_QUERYSEGMENTOUT4>(),
        );
    }

    // One small CPU-visible aperture segment is enough for dxgkrnl bring-up and
    // paging-buffer staging. It is not advertised as render-capable GPU memory.
    out.NbSegment = 1;
    out.SegmentDescriptorStride = size_of::<DXGK_SEGMENTDESCRIPTOR4>() as u64;
    out.PagingBufferSegmentId = 1;
    out.PagingBufferSize = 64 * 1024;
    out.PagingBufferPrivateDataSize = 0;

    if !segment_descriptor.is_null() {
        // SAFETY: the second QUERYSEGMENT4 call provides an array for NbSegment
        // descriptors; we report exactly one descriptor.
        let seg = unsafe { &mut *(segment_descriptor as *mut DXGK_SEGMENTDESCRIPTOR4) };
        unsafe {
            core::ptr::write_bytes(
                seg as *mut _ as *mut u8,
                0,
                size_of::<DXGK_SEGMENTDESCRIPTOR4>(),
            );
            seg.Flags
                .__bindgen_anon_1
                .__bindgen_anon_1
                .set_CpuVisible(1);
            seg.Flags.__bindgen_anon_1.__bindgen_anon_1.set_Aperture(1);
        }
        seg.BaseAddress.QuadPart = 0;
        seg.Size = 64 * 1024 * 1024;
        seg.CommitLimit = 64 * 1024 * 1024;
    }

    STATUS_SUCCESS
}

/// `DxgkDdiGetNodeMetadata` — describe GPU engine node `node_ordinal`.
///
/// Unlike the other Phase-1.5 stubs this has a real body: Dxgkrnl enumerates
/// engine nodes during adapter bring-up starting at ordinal 0, and MSDN requires
/// every call for a *valid* ordinal to succeed — `STATUS_NOT_IMPLEMENTED` is not
/// an allowed return and would leave the device in an error state. We expose a
/// single symmetric 3D engine node (ordinal 0); the node count is implicit (1).
pub unsafe extern "C" fn dxgkddi_get_node_metadata(
    _h_adapter: IN_CONST_HANDLE,
    node_ordinal: UINT,
    get_node_metadata: OUT_PDXGKARG_GETNODEMETADATA,
) -> NTSTATUS {
    // DIAG: log node-metadata enumeration during AddAdapter.
    crate::diag::record(0x0300_0000 | (node_ordinal & 0xFFFF));
    // Only node 0 exists; any other ordinal is out of range.
    if get_node_metadata.is_null() || node_ordinal != 0 {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: non-null per the check above; Dxgkrnl provides a writable
    // DXGK_NODEMETADATA (DXGKARG_GETNODEMETADATA is an alias for it).
    let node = unsafe { &mut *get_node_metadata };
    unsafe {
        core::ptr::write_bytes(node as *mut _ as *mut u8, 0, size_of::<DXGK_NODEMETADATA>());
    }
    node.EngineType = DXGK_ENGINE_TYPE::DXGK_ENGINE_TYPE_3D;
    // Do not set GpuMmuSupported until GPU-VA/page-table DDIs are real.
    // FriendlyName, Flags, IoMmuSupported stay zeroed.
    STATUS_SUCCESS
}
