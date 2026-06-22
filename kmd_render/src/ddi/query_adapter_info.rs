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
    DXGKQAITYPE_DRIVERCAPS, DXGKQAITYPE_GPUMMUCAPS, DXGKQAITYPE_GPUVERSION,
    DXGKQAITYPE_HARDWARERESERVEDRANGES2, DXGKQAITYPE_HISTORYBUFFERPRECISION,
    DXGKQAITYPE_IOMMU_CAPS, DXGKQAITYPE_PAGETABLELEVELDESC, DXGKQAITYPE_PHYSICAL_MEMORY_CAPS,
    DXGKQAITYPE_QUERYSEGMENT4, DXGKQAITYPE_WDDMDEVICECAPS,
};
use crate::dxgk::_DXGK_WDDMVERSION::DXGKDDI_WDDMv3_2;
use crate::dxgk::*;

use crate::adapter::AdapterContext;
use crate::ddi::gpummu;

pub unsafe extern "C" fn dxgkddi_query_adapter_info(
    miniport_device_context: *mut c_void,
    query_adapter_info: *const DXGKARG_QUERYADAPTERINFO,
) -> NTSTATUS {
    if miniport_device_context.is_null() || query_adapter_info.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: valid per the DDI contract; we only read the args struct.
    let args = unsafe { &*query_adapter_info };
    // SAFETY: Dxgkrnl hands back our AdapterContext as the miniport context.
    let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };

    // DIAG: log every QueryAdapterInfo type dxgkrnl requests during AddAdapter,
    // EXCEPT the steady-state perf-poll types (NODEPERFDATA 0x18 / ADAPTERPERFDATA
    // 0x19) that dxgkrnl polls continuously to feed the Task Manager GPU tab.
    // Those flood the 160-entry diag ring within ~1s and overwrite the one-shot
    // bring-up / failure breadcrumbs we actually need. Gate them out here so the
    // ring stays readable for post-start diagnosis.
    let type_num = args.Type as u32 & 0xFFFF;
    let is_perf_poll = type_num == 0x18 || type_num == 0x19;
    if !is_perf_poll {
        crate::diag::record(0x0100_0000 | type_num);
    }

    match args.Type {
        DXGKQAITYPE_DRIVERCAPS => unsafe { query_driver_caps(args) },
        DXGKQAITYPE_QUERYSEGMENT4 => unsafe { query_segments(adapter, args) },
        DXGKQAITYPE_GPUMMUCAPS => unsafe { gpummu::fill_gpummu_caps(args) },
        DXGKQAITYPE_PAGETABLELEVELDESC => unsafe {
            gpummu::fill_page_table_level_desc(args, gpummu::SYSTEM_MEMORY_SEGMENT_ID)
        },
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
            // Same perf-poll gate as the entry log — 0x18/0x19 flood the ring.
            if !is_perf_poll {
                crate::diag::record(0x0200_0000 | (other as u32 & 0xFFFF));
            }
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
    // Avoid dxgkrnl falling back through the CDD shadow-buffer interop path for a
    // zero-source render adapter. Bit 8 is DriverSupportsCddDwmInterop in the WDK
    // binding dump.
    const PRESENTATIONCAPS_DRIVER_SUPPORTS_CDD_DWM_INTEROP: u32 = 1 << 8;
    const FLIPCAPS_FLIP_ON_VSYNC_MMIO: u32 = 1 << 1;
    const SCHEDULINGCAPS_MULTI_ENGINE_AWARE: u32 = 1 << 0;
    const SCHEDULINGCAPS_PREEMPTION_AWARE: u32 = 1 << 2;
    const MEMORYMANAGEMENTCAPS_SECTION_BACKED_PRIMARY: u32 = 1 << 3;
    // GpuMmu memory-model opt-in (DXGK_VIDMMCAPS = DRIVERCAPS.MemoryManagementCaps,
    // bit positions verified against the bindgen dump, WDDM_FAKE_VIDMM_RESEARCH.md
    // §A1): bit 5 = VirtualAddressingSupported (umbrella "this adapter does WDDM 2.0
    // GPU virtual addressing"), bit 6 = GpuMmuSupported (selects the GpuMmu model).
    // Per the research doc these two together are the minimal opt-in. We deliberately
    // do NOT set IoMmuSupported (bit 7) — no IoMmu path — nor ParavirtualizationSupported
    // (bit 10): that is a host-KMD GPU-PV contract Helios does not implement, and the
    // GpuVirtualizationFlags lever was already proven not to govern our path.
    const MEMORYMANAGEMENTCAPS_VIRTUAL_ADDRESSING_SUPPORTED: u32 = 1 << 5;
    const MEMORYMANAGEMENTCAPS_GPU_MMU_SUPPORTED: u32 = 1 << 6;

    caps.PresentationCaps.__bindgen_anon_1.Value =
        PRESENTATIONCAPS_SUPPORT_KERNEL_MODE_COMMAND_BUFFER
            | PRESENTATIONCAPS_DRIVER_SUPPORTS_CDD_DWM_INTEROP;
    caps.FlipCaps.__bindgen_anon_1.Value = FLIPCAPS_FLIP_ON_VSYNC_MMIO;
    caps.SchedulingCaps.__bindgen_anon_1.Value =
        SCHEDULINGCAPS_MULTI_ENGINE_AWARE | SCHEDULINGCAPS_PREEMPTION_AWARE;
    caps.MemoryManagementCaps.__bindgen_anon_1.Value = MEMORYMANAGEMENTCAPS_SECTION_BACKED_PRIMARY
        | MEMORYMANAGEMENTCAPS_VIRTUAL_ADDRESSING_SUPPORTED
        | MEMORYMANAGEMENTCAPS_GPU_MMU_SUPPORTED;
    // PagingNode = 0 (single engine node). MemoryManagementCaps.PagingNode is the
    // second field of DXGK_VIDMMCAPS, after the flags union; the zero-fill at the
    // top of this fn already leaves it 0, which is what we want.
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

/// GPU-VA base of the aperture segment's address window (matches viogpu3d's
/// `0xC0000000`). An aperture is address-space only; the value just needs to be a
/// plausible, non-overlapping window base.
const APERTURE_BASE_ADDRESS: i64 = 0xC000_0000;
/// Aperture window length. viogpu3d uses 1 GiB (`256 * 1024 * 4096`).
const APERTURE_SEGMENT_SIZE: SIZE_T = 256 * 1024 * 4096;

/// Segment-shape selector — the lever from the live-KD decode of dxgmms2's paging
/// DMA-pool init (INITDMAPOOLS_HANDOVER.md, 2026-06-21).
///
/// `VIDMM_GLOBAL::InitDmaPools` builds ONE paging DMA pool and validates the FIRST
/// reported segment (`segdesc[0]`): it requires that segment's per-attribute
/// object's "can host a paging buffer" flag (`[attr+0x68] & 1`). A CPU-visible
/// *memory* segment (the BAR) NEVER has that flag; a linear **aperture** segment
/// does (classic WDDM paging-buffer backing via MAP_APERTURE_SEGMENT — what the
/// proven viogpu3d driver reports, WDDM_FAKE_VIDMM §A2.2). The decode also showed
/// VidMm DROPS a system-RAM-backed memory segment (only the BAR registered), so the
/// old "RAM paging segment id 2" never existed.
///
/// - `false` = **SAFE** shape: report only the BAR CpuVisible **memory** segment.
///   InitDmaPools then cleanly *rejects* it (`STATUS_INVALID_PARAMETER`, Code 43) —
///   a status return, not a crash, so the adapter just fails to start and the VM
///   boots normally with gpu-gl attached. Use this for a deployable safe baseline.
/// - `true` = **APERTURE** shape: report a viogpu3d-style aperture segment FIRST
///   (= `PagingBufferSegmentId`) so InitDmaPools ACCEPTS it, plus the BAR memory
///   segment (id 2) for page tables (`MEMORY_SEGMENT_ID`) + render. This is the
///   deployable shape after the 2026-06-22 follow-on fix in `CreateContext`:
///   `DmaBufferSegmentSet=1` keeps the CDD/system context DMA pool on the
///   aperture allocation path instead of letting VidMm build a null-allocation
///   contiguous-memory pool.
const REPORT_APERTURE_PAGING_SEGMENT: bool = true;

/// Write the viogpu3d-style linear aperture descriptor (paging-buffer host).
/// SAFETY: `seg` points to a writable `DXGK_SEGMENTDESCRIPTOR4`.
unsafe fn write_aperture_descriptor(seg: *mut DXGK_SEGMENTDESCRIPTOR4) {
    unsafe {
        core::ptr::write_bytes(seg as *mut u8, 0, size_of::<DXGK_SEGMENTDESCRIPTOR4>());
        let s = &mut *seg;
        s.Flags.__bindgen_anon_1.__bindgen_anon_1.set_Aperture(1);
        s.Flags
            .__bindgen_anon_1
            .__bindgen_anon_1
            .set_CacheCoherent(1);
        s.Flags.__bindgen_anon_1.__bindgen_anon_1.set_DirectFlip(1);
        // CpuVisible deliberately 0 — an aperture holds no bits (viogpu3d :558).
        s.BaseAddress.QuadPart = APERTURE_BASE_ADDRESS;
        s.Size = APERTURE_SEGMENT_SIZE;
        s.CommitLimit = APERTURE_SEGMENT_SIZE;
    }
}

/// Write a MEMORY descriptor (`Aperture=0`, holds bits) whose backing lives at
/// `base` (guest-physical) for `len` bytes. For the GpuMmu page-table segment we
/// expose CPU access through WDDM's CPU host aperture, not the legacy CpuVisible
/// direct-BAR path: VidMm's `GetCpuVisibleAddress` accepts the internal segment
/// attribute only when it is a CPU-host-aperture segment (bit 13), and the public
/// docs describe `DXGK_CPUHOSTAPERTURE` as the alternative mapping path for
/// non-CPU-accessible memory segments.
/// SAFETY: `seg` points to a writable `DXGK_SEGMENTDESCRIPTOR4`.
unsafe fn write_cpu_host_memory_descriptor(seg: *mut DXGK_SEGMENTDESCRIPTOR4, base: u64, len: u64) {
    unsafe {
        core::ptr::write_bytes(seg as *mut u8, 0, size_of::<DXGK_SEGMENTDESCRIPTOR4>());
        let s = &mut *seg;
        s.BaseAddress.QuadPart = 0;
        // CpuVisible deliberately stays 0. If this is set, dxgkrnl treats the
        // union as CpuTranslatedAddress and does not create the CPU-host-aperture
        // segment attributes VidMm needs for paging-process page tables.
        s.Flags
            .__bindgen_anon_1
            .__bindgen_anon_1
            .set_SupportsCpuHostAperture(1);
        s.Flags
            .__bindgen_anon_1
            .__bindgen_anon_1
            .set_SupportsCachedCpuHostAperture(1);
        // Aperture stays 0 — this segment holds bits (the BAR/device memory).
        s.Flags
            .__bindgen_anon_1
            .__bindgen_anon_1
            .set_CacheCoherent(1);
        let cpu_host = DXGK_CPUHOSTAPERTURE {
            PhysicalAddress: base,
            SizeInPages: (len / 4096).min(u32::MAX as u64) as u32,
        };
        *s.__bindgen_anon_1.CpuHostAperture.as_mut() = cpu_host;
        s.Size = len as SIZE_T;
        s.CommitLimit = len as SIZE_T;
    }
}

unsafe fn query_segments(adapter: &AdapterContext, args: &DXGKARG_QUERYADAPTERINFO) -> NTSTATUS {
    if (args.OutputDataSize as usize) < size_of::<DXGK_QUERYSEGMENTOUT4>() {
        return STATUS_BUFFER_TOO_SMALL;
    }

    // The host-visible BAR window backs the CPU-visible memory segment.
    let window = adapter.with_virtio(|v| v.host_visible()).ok().flatten();
    crate::diag::record(if window.is_some() {
        0x0900_0001
    } else {
        0x0900_0000
    });

    // SAFETY: pOutputData points to a writable DXGK_QUERYSEGMENTOUT4 of
    // sufficient size, checked above.
    let out = unsafe { &mut *(args.pOutputData as *mut DXGK_QUERYSEGMENTOUT4) };
    let descriptors = out.pSegmentDescriptor;
    unsafe {
        core::ptr::write_bytes(
            out as *mut _ as *mut u8,
            0,
            size_of::<DXGK_QUERYSEGMENTOUT4>(),
        );
    }

    let stride = size_of::<DXGK_SEGMENTDESCRIPTOR4>();
    out.SegmentDescriptorStride = stride as u64;
    out.PagingBufferSize = 64 * 1024;
    out.PagingBufferPrivateDataSize = 0;

    if REPORT_APERTURE_PAGING_SEGMENT {
        // APERTURE shape (Option A): aperture (id 1, idx 0) = paging-buffer host
        // (passes InitDmaPools); AddDevice-time RAM-backed MEMORY segment (id 2,
        // idx 1) = page tables, exposed through WDDM CPU host aperture. This must
        // use `paging_ram`, not `page_table_window`: QuerySegment4 runs before
        // StartDevice's venus allocation, so `page_table_window` is not available
        // when VidMm builds its segment table. If the contiguous RAM allocation was
        // unavailable, fall back to NbSegment=1 (aperture only).
        let page_table = adapter.paging_ram();
        crate::diag::record(if descriptors.is_null() {
            0x0901_0000
        } else {
            0x0901_0001
        });
        crate::diag::record(if page_table.is_some() {
            0x0902_0001
        } else {
            0x0902_0000
        });
        out.NbSegment = if page_table.is_some() { 2 } else { 1 };
        // Paging buffers come from segdesc[0] (the aperture) — InitDmaPools
        // validates the FIRST segment for paging-buffer-host capability.
        out.PagingBufferSegmentId = gpummu::APERTURE_SEGMENT_ID;
        if !descriptors.is_null() {
            // SAFETY: the second QUERYSEGMENT4 call provides NbSegment descriptors.
            unsafe { write_aperture_descriptor(descriptors as *mut DXGK_SEGMENTDESCRIPTOR4) };
            if let Some((gpa, size)) = page_table {
                crate::diag::record(0x0903_0000 | (((gpa >> 12) as u32) & 0xFFFF));
                crate::diag::record(0x0904_0000 | (((size >> 12) as u32) & 0xFFFF));
                // SAFETY: index 1 is in bounds (NbSegment == 2).
                let seg2 = unsafe { (descriptors as *mut u8).add(stride) };
                unsafe {
                    write_cpu_host_memory_descriptor(
                        seg2 as *mut DXGK_SEGMENTDESCRIPTOR4,
                        gpa,
                        size,
                    )
                };
            }
        }
    } else {
        // SAFE shape: a single BAR CpuVisible memory segment (id 1). InitDmaPools
        // rejects it cleanly (Code 43, no crash). If there is no host-visible
        // window, fall back to the aperture descriptor (never CpuVisible-backed).
        out.NbSegment = 1;
        out.PagingBufferSegmentId = gpummu::APERTURE_SEGMENT_ID; // numeric id 1 = sole segment
        if !descriptors.is_null() {
            let seg0 = descriptors as *mut DXGK_SEGMENTDESCRIPTOR4;
            match window {
                // SAFETY: descriptors points to a writable DXGK_SEGMENTDESCRIPTOR4.
                Some(w) => unsafe { write_cpu_host_memory_descriptor(seg0, w.base, w.len) },
                None => unsafe { write_aperture_descriptor(seg0) },
            }
        }
    }

    crate::diag::record(0x0900_0002 | (out.NbSegment << 8));
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
    // Per-node memory-model selection (WDDM_FAKE_VIDMM_RESEARCH.md §A1/§G): the
    // GpuMmu-vs-IoMmu choice is made HERE per engine node, in tandem with the
    // DXGK_VIDMMCAPS GpuMmuSupported bit in query_driver_caps. We declare the
    // decorative GpuMmu model (host GPU owns the real MMU; guest page tables are
    // never read by hardware — §A3.7) and no IoMmu. The GPUMMUCAPS / page-table
    // geometry VidMm then queries lives in `ddi::gpummu`.
    node.GpuMmuSupported = 1;
    node.IoMmuSupported = 0;
    // FriendlyName, Flags stay zeroed.
    STATUS_SUCCESS
}
