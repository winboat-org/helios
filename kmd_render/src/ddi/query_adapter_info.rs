//! `DxgkDdiQueryAdapterInfo` — report adapter capabilities.
//!
//! Capability reporting for the active render+display adapter. The legacy
//! knob-off render-only shape remains available for recovery/A-B testing.
//!
//! Reference: https://learn.microsoft.com/windows-hardware/drivers/ddi/d3dkmddi/nc-d3dkmddi-dxgkddi_queryadapterinfo

use core::ffi::c_void;
use core::mem::{offset_of, size_of};

use crate::dxgk::_D3DKMDT_COMPUTE_PREEMPTION_GRANULARITY::D3DKMDT_COMPUTE_PREEMPTION_DMA_BUFFER_BOUNDARY;
use crate::dxgk::_D3DKMDT_GRAPHICS_PREEMPTION_GRANULARITY::D3DKMDT_GRAPHICS_PREEMPTION_DMA_BUFFER_BOUNDARY;
use crate::dxgk::_DXGK_QUERYADAPTERINFOTYPE::{
    DXGKQAITYPE_64BITONLYCAPS, DXGKQAITYPE_ADAPTERPERFDATA_CAPS, DXGKQAITYPE_DIRTYBITTRACKINGCAPS,
    DXGKQAITYPE_DRIVERCAPS, DXGKQAITYPE_GPUMMUCAPS, DXGKQAITYPE_GPUVERSION,
    DXGKQAITYPE_HARDWARERESERVEDRANGES2, DXGKQAITYPE_HISTORYBUFFERPRECISION,
    DXGKQAITYPE_IOMMU_CAPS, DXGKQAITYPE_PAGETABLELEVELDESC, DXGKQAITYPE_PHYSICAL_MEMORY_CAPS,
    DXGKQAITYPE_QUERYSEGMENT, DXGKQAITYPE_QUERYSEGMENT3, DXGKQAITYPE_QUERYSEGMENT4,
    DXGKQAITYPE_WDDMDEVICECAPS,
};
use crate::dxgk::*;

use crate::adapter::AdapterContext;
use crate::ddi::gpummu;
use crate::ddi::wddm_surface::SURFACE;

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
        DXGKQAITYPE_DRIVERCAPS => unsafe { query_driver_caps(adapter, args) },
        DXGKQAITYPE_QUERYSEGMENT => unsafe { query_segments_legacy(args) },
        DXGKQAITYPE_QUERYSEGMENT3 => unsafe { query_segments3(args) },
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
        // PHYSICALADAPTERCAPS (0x0F) stays rejected: answering it (2026-06-22 test)
        // pulls dxgmms2 into per-physical-adapter / per-execution-node setup that
        // dereferences a null node structure our null engine never provides
        // (bugcheck 0x3B SYSTEM_SERVICE_EXCEPTION, AV on null-base+0x210 in
        // dxgmms2+0x9775d). viogpu3d also leaves it unimplemented. So its contract
        // remains undefined for us until we have real execution nodes; D3D11's
        // device-create rejection is NOT this query (DXGI tolerates the rejection).
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

/// A dxgkrnl-supplied **versioned** output buffer: a leading prefix of `T` whose
/// length the running OS chose, which may legitimately be SHORTER than `T`.
///
/// That is the whole reason this type exists. `query_driver_caps`'s size gate
/// deliberately admits `OutputDataSize` values below `size_of::<DXGK_DRIVERCAPS>()`
/// (592 on the bindgen'd 26100 headers) — a caps buffer is versioned, and 540 is
/// all this driver's surface needs. Forming `&mut *(pOutputData as *mut
/// DXGK_DRIVERCAPS)` over such a buffer is undefined behaviour *independent of
/// which fields are touched*: a reference must be dereferenceable for its whole
/// referent type. The old code did exactly that, and then wrote through
/// `args.pOutputData` — a second, separately-derived raw pointer — while that
/// reference was still live and used afterwards, which is additionally a
/// Stacked-Borrows aliasing violation.
///
/// So: **`VersionedOut` never exposes the referent type by reference.** A wrapper
/// that still handed out `&mut T` internally would relocate the defect, not remove
/// it. The only way out of this type is [`VersionedOut::set`], one field at a time,
/// each bounds-checked against the length the OS actually declared.
///
/// The bound also becomes self-maintaining. `REQUIRED_DRIVER_CAPS_SIZE` is a
/// hand-maintained claim — "the last field we write is `SupportDirectFlip`" —
/// that nothing checked against the writes below it. It is true today only by
/// luck: `SupportMultiPlaneOverlay` sits at offset 540, exactly one byte past the
/// checked bound, so `caps.SupportMultiPlaneOverlay = 1` in a future commit would
/// have written past dxgkrnl's allocation in the DDI that gates adapter start.
/// With a per-field check that write is refused and counted instead.
struct VersionedOut {
    base: *mut u8,
    len: usize,
    /// Fields skipped because they did not fit. Reported through `CapTrunc`.
    skipped: u32,
}

impl VersionedOut {
    /// # Safety
    ///
    /// `base` must be valid for writes of `len` bytes and must stay so for the
    /// lifetime of this value. `len` is the size the runtime declared, NOT
    /// `size_of::<T>()`.
    unsafe fn new(base: *mut u8, len: usize) -> Self {
        Self {
            base,
            len,
            skipped: 0,
        }
    }

    /// Write `value` at `offset` if the whole field lies inside the declared
    /// buffer; otherwise count it and skip.
    ///
    /// Skipping rather than returning early is deliberate: a short buffer still
    /// gets the maximal valid prefix of the capability surface, which is what a
    /// versioned buffer means. A caller that needs a field to be mandatory must
    /// enforce that through the minimum-size gate, not through this.
    fn set<F>(&mut self, offset: usize, value: F) {
        let fits = offset
            .checked_add(size_of::<F>())
            .is_some_and(|end| end <= self.len);
        if !fits {
            self.skipped = self.skipped.saturating_add(1);
            return;
        }
        // SAFETY: `base` is valid for writes of `len` bytes (constructor
        // precondition) and the check above proves `offset .. offset +
        // size_of::<F>()` lies inside that range. `write_unaligned` because the
        // buffer's alignment is dxgkrnl's to promise, not ours.
        unsafe { self.base.add(offset).cast::<F>().write_unaligned(value) };
    }
}

/// Byte offset of a `DXGK_DRIVERCAPS` field, including one level of nesting.
///
/// Written as an addition of two `offset_of!`s rather than a nested field path so
/// it composes through the bindgen anonymous-union wrappers as well: for
/// `PresentationCaps`/`FlipCaps`/`SchedulingCaps`/`MemoryManagementCaps` the flags
/// union is the FIRST member of its struct and `Value` is the first member of that
/// union, so the struct's own offset already names the `UINT` view — the same
/// `.Value`-plus-mask convention the bit definitions below use.
macro_rules! caps_offset {
    ($field:ident) => {
        offset_of!(DXGK_DRIVERCAPS, $field)
    };
    ($field:ident, $inner:ty, $sub:ident) => {
        offset_of!(DXGK_DRIVERCAPS, $field) + offset_of!($inner, $sub)
    };
}

/// The caps surface is a pure function of the build's compile-time levers and
/// the two service knobs read below, so it no longer reads adapter state; the
/// parameter stays so the dispatcher's arms remain uniform.
unsafe fn query_driver_caps(
    _adapter: &AdapterContext,
    args: &DXGKARG_QUERYADAPTERINFO,
) -> NTSTATUS {
    // The MINIMUM this DDI insists on, unchanged at 540 so the
    // STATUS_BUFFER_TOO_SMALL threshold and the 0x02CF record keep their values.
    // It is no longer load-bearing for memory safety — every write below is
    // individually bounded — but it still states the smallest surface worth
    // reporting.
    const REQUIRED_DRIVER_CAPS_SIZE: usize =
        offset_of!(DXGK_DRIVERCAPS, SupportDirectFlip) + size_of::<BOOLEAN>();

    crate::diag::record(0x01CF_0000 | (args.OutputDataSize & 0xFFFF));
    if (args.OutputDataSize as usize) < REQUIRED_DRIVER_CAPS_SIZE {
        crate::diag::record(0x02CF_0000 | ((REQUIRED_DRIVER_CAPS_SIZE as u32) & 0xFFFF));
        return STATUS_BUFFER_TOO_SMALL;
    }

    // Zero-fill FIRST, through the raw pointer, before any other pointer into the
    // buffer exists. Doing it after deriving a writer is what made the old code
    // an aliasing violation on top of the referent-size one.
    // SAFETY: dxgkrnl gives us `OutputDataSize` writable bytes at `pOutputData`.
    unsafe { core::ptr::write_bytes(args.pOutputData as *mut u8, 0, args.OutputDataSize as usize) };

    // SAFETY: as above — valid for writes of `OutputDataSize` bytes for the
    // duration of this call, and no other pointer into it is live from here on.
    let mut out =
        unsafe { VersionedOut::new(args.pOutputData as *mut u8, args.OutputDataSize as usize) };

    // 64-bit addressable; no render/memory features are advertised here yet.
    // `HighestAcceptableAddress` is a LARGE_INTEGER union whose `QuadPart` is the
    // full 8-byte view at offset 0.
    out.set::<i64>(caps_offset!(HighestAcceptableAddress), -1);
    let max_allocation_list_slot_id: UINT = 0xFFFF;
    out.set(
        caps_offset!(MaxAllocationListSlotId),
        max_allocation_list_slot_id,
    );
    let aperture_segment_commit_limit: SIZE_T = 64 * 1024 * 1024;
    out.set(
        caps_offset!(ApertureSegmentCommitLimit),
        aperture_segment_commit_limit,
    );
    // Not a legacy VGA device.
    let support_non_vga: BOOLEAN = 1;
    out.set(caps_offset!(SupportNonVGA), support_non_vga);

    // One value drives this, DXGKQAITYPE_WDDMDEVICECAPS and DriverEntry's
    // DRIVER_INITIALIZATION_DATA.Version. Dxgkrnl rejects internally
    // inconsistent version/capability surfaces during AddAdapter.
    let wddm_version = SURFACE.driver_caps_version();
    out.set(caps_offset!(WDDMVersion), wddm_version);
    out.set(
        caps_offset!(
            PreemptionCaps,
            D3DKMDT_PREEMPTION_CAPS,
            GraphicsPreemptionGranularity
        ),
        D3DKMDT_GRAPHICS_PREEMPTION_DMA_BUFFER_BOUNDARY,
    );
    out.set(
        caps_offset!(
            PreemptionCaps,
            D3DKMDT_PREEMPTION_CAPS,
            ComputePreemptionGranularity
        ),
        D3DKMDT_COMPUTE_PREEMPTION_DMA_BUFFER_BOUNDARY,
    );
    let support_per_engine_tdr: BOOLEAN = 1;
    out.set(caps_offset!(SupportPerEngineTDR), support_per_engine_tdr);

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
    // GDI HW ACCELERATION IS NOT ADVERTISED, and there is no configuration in
    // which it can be. `DXGK_PRESENTATIONCAPS::SupportKernelModeCommandBuffer`
    // (bit 2) is the opt-in that makes dxgkrnl route all GDI-drawn window content
    // through `DxgkDdiRenderGdi`; Helios does not implement a GDI raster engine, so
    // the honest answer is 0 and win32k rasterizes GDI on the CPU into CpuVisible
    // allocations instead (Windows' canonical redirection path).
    //
    // History, because the bit was once believed load-mandatory: bisected
    // 2026-07-02 (v22.22.34.0) as dropping it => CM_PROB_FAILED_ADD; that result
    // was overturned by the 2026-07-06 `GdiAccelMode=0` A/B, which bound Code 0 and
    // rendered the desktop correctly with zero RenderGdi traffic. GDI HW
    // acceleration is documented as OPTIONAL (gdi-hardware-acceleration.md) and
    // viogpu3d — the closest WDDM driver to Helios — never sets it. The mode-0
    // default shipped from then on, and the KMD CPU blitter it kept alive was
    // deleted in 22.22.180.0 together with the `GdiAccelMode` rollback knob
    // (reachability re-proven the same boot: with every `Gd*` service value
    // deleted, a full GDI-heavy session — explorer restart, maximized notepad, GDI
    // canary, repaint, paintcap — reproduced none of them).
    const FLIPCAPS_FLIP_ON_VSYNC_MMIO: u32 = 1 << 1;
    const SCHEDULINGCAPS_MULTI_ENGINE_AWARE: u32 = 1 << 0;
    const SCHEDULINGCAPS_PREEMPTION_AWARE: u32 = 1 << 2;
    const MEMORYMANAGEMENTCAPS_SECTION_BACKED_PRIMARY: u32 = 1 << 3;
    // Do not set WDDM 2.0+ GPU virtual-addressing/GpuMmu bits while the driver
    // advertises a WDDM 1.3 interface. The GpuMmu work remains in-tree, but the
    // public capability surface must stay self-consistent or dxgkrnl can reject
    // the adapter before StartDevice.
    // Cross-adapter resource support (DXGK_VIDMMCAPS bit 4, WDDM_FAKE_VIDMM_RESEARCH
    // §A1). PATH-A LEVER (2026-06-22): the Looking Glass IDD selects Helios as its
    // render adapter, but the OS builds 0 display paths and never assigns the
    // indirect monitor a swapchain. WARP — itself a render-ONLY adapter with no
    // VidPN sources — works in that slot, so the gap is NOT a missing VidPN source;
    // it is that the IddCx composition swapchain surface is a CROSS-ADAPTER resource
    // (created as a standard allocation, opened on the render GPU via
    // GetStandardAllocationDriverData — see
    // display/rendering-on-a-discrete-gpu-using-cross-adapter-resources.md). An
    // adapter that does not declare CrossAdapterResource cannot be used as the
    // cross-adapter render side, so the OS cannot stand up the swapchain → no path.
    // Declaring it (the user-mode SupportCrossAdapterResource bit derives from this)
    // lets the OS attempt the cross-adapter composition surface on Helios. Gated so
    // it is reversible if it destabilizes boot (flip false + redeploy / gpu-gl-out).
    const MEMORYMANAGEMENTCAPS_CROSS_ADAPTER_RESOURCE: u32 = 1 << 4;
    // DXGK_VIDMMCAPS bit positions (verified against the generated bitfield order
    // in dxgk_bindings: bit5 = VirtualAddressingSupported, bit6 = GpuMmuSupported).
    // The GpuMmu opt-in is the WDDM 2.0+ GPU-virtual-addressing memory model that
    // gates monitored fences; declared together with the WDDM 3.2 version above and
    // GetNodeMetadata.GpuMmuSupported below so the cap surface is self-consistent.
    const MEMORYMANAGEMENTCAPS_VIRTUAL_ADDRESSING_SUPPORTED: u32 = 1 << 5;
    const MEMORYMANAGEMENTCAPS_GPU_MMU_SUPPORTED: u32 = 1 << 6;

    let mut mem_caps = MEMORYMANAGEMENTCAPS_SECTION_BACKED_PRIMARY;
    if DECLARE_CROSS_ADAPTER_RESOURCE || cross_adapter_advertised() {
        mem_caps |= MEMORYMANAGEMENTCAPS_CROSS_ADAPTER_RESOURCE;
    }
    if SURFACE.gpu_mmu() {
        mem_caps |= MEMORYMANAGEMENTCAPS_VIRTUAL_ADDRESSING_SUPPORTED
            | MEMORYMANAGEMENTCAPS_GPU_MMU_SUPPORTED;
    }

    // No GDI hardware acceleration (see the note above): every documented
    // DXGK_PRESENTATIONCAPS bit stays clear.
    let presentation_caps: UINT = 0;
    let flip_caps: UINT = FLIPCAPS_FLIP_ON_VSYNC_MMIO;
    let scheduling_caps: UINT =
        SCHEDULINGCAPS_MULTI_ENGINE_AWARE | SCHEDULINGCAPS_PREEMPTION_AWARE;
    out.set(caps_offset!(PresentationCaps), presentation_caps);
    out.set(caps_offset!(FlipCaps), flip_caps);
    out.set(caps_offset!(SchedulingCaps), scheduling_caps);
    out.set(caps_offset!(MemoryManagementCaps), mem_caps);
    // PagingNode = 0 (single engine node). MemoryManagementCaps.PagingNode is the
    // second field of DXGK_VIDMMCAPS, after the flags union; the zero-fill at the
    // top of this fn already leaves it 0, which is what we want.
    let max_queued_flip_on_vsync: UINT = 1;
    out.set(caps_offset!(MaxQueuedFlipOnVSync), max_queued_flip_on_vsync);
    // DIRECT-FLIP DENIAL (27th session, 2026-07-07): SupportDirectFlip=1 was an
    // unbacked bring-up advertisement (no bisect ever showed it load-mandatory —
    // the STEP-0 bisect above proved only FlipOnVSyncMmIo). On this adapter it is
    // a LIE: Helios has zero scanout (all VidPn DDIs NOT_SUPPORTED; the display
    // is an IddCx driver capturing dwm's COMPOSED output), so a dwm direct/
    // independent-flip promotion of an eligible visual (flip-model + IGNORE-alpha
    // + unoccluded — exactly the dcomp vehicle chain) makes dwm STOP COMPOSING it
    // while every fence stays green: the owner-reproduced two-stale-frame
    // alternation + old-frames-flashing stutter, cured by any dirty-region
    // recompose (the taskbar clock's minute repaint = the hands-off ~60 s
    // recovery). The UMD already denies CheckDirectFlipSupport unconditionally;
    // this makes the KMD agree. Display-less-adapter guidance
    // (mcdm-implementation-guidelines.md) requires 0 here. `DirectFlipCaps`
    // service knob (default 0) restores the legacy advertisement for A/B via
    // reg add + devcon restart; value lands in the 0x01D7 diag record bit 2.
    let support_direct_flip: BOOLEAN = if direct_flip_advertised() { 1 } else { 0 };
    out.set(caps_offset!(SupportDirectFlip), support_direct_flip);
    let nb_asymetric_processing_nodes: UINT = 1;
    out.set(
        caps_offset!(
            GpuEngineTopology,
            DXGK_GPUENGINETOPOLOGY,
            NbAsymetricProcessingNodes
        ),
        nb_asymetric_processing_nodes,
    );

    // Recorded from the LOCALS, not read back out of dxgkrnl's buffer. Reading
    // back would need a second pointer into it for no benefit, and a field that
    // `set` skipped would report as 0 rather than as the value this driver
    // believes it advertised — `CapTrunc` is what says a field was dropped.
    crate::diag::record(0x01D0_0000 | ((wddm_version as u32) & 0xFFFF));
    crate::diag::record(0x01D1_0000 | (presentation_caps & 0xFFFF));
    crate::diag::record(0x01D2_0000 | (flip_caps & 0xFFFF));
    crate::diag::record(0x01D3_0000 | (scheduling_caps & 0xFFFF));
    crate::diag::record(0x01D4_0000 | (mem_caps & 0xFFFF));
    crate::diag::record(0x01D5_0000 | (max_allocation_list_slot_id & 0xFFFF));
    crate::diag::record(0x01D6_0000 | ((aperture_segment_commit_limit as u32) & 0xFFFF));
    crate::diag::record(
        0x01D7_0000
            | (((support_non_vga as u32) & 1) << 0)
            | (((support_per_engine_tdr as u32) & 1) << 1)
            | (((support_direct_flip as u32) & 1) << 2),
    );
    crate::diag::record(0x01D8_0000 | (nb_asymetric_processing_nodes & 0xFFFF));

    // Ungated: a truncated capability surface must be visible on a default boot.
    // Written unconditionally (not only when nonzero) so the value describes THIS
    // query rather than the last one that happened to truncate.
    crate::diag::fault(crate::diag::FaultCounter::CapTrunc, out.skipped);

    STATUS_SUCCESS
}

unsafe fn query_zeroed<T>(args: &DXGKARG_QUERYADAPTERINFO) -> NTSTATUS {
    if (args.OutputDataSize as usize) < size_of::<T>() {
        return STATUS_BUFFER_TOO_SMALL;
    }

    // SAFETY: pOutputData points to a writable object of the requested type.
    unsafe { core::ptr::write_bytes(args.pOutputData as *mut u8, 0, args.OutputDataSize as usize) };
    STATUS_SUCCESS
}

unsafe fn query_wddm_device_caps(args: &DXGKARG_QUERYADAPTERINFO) -> NTSTATUS {
    if (args.OutputDataSize as usize) < size_of::<DXGK_WDDMDEVICECAPS>() {
        return STATUS_BUFFER_TOO_SMALL;
    }

    // SAFETY: pOutputData points to a DXGK_WDDMDEVICECAPS of sufficient size.
    let caps = unsafe { &mut *(args.pOutputData as *mut DXGK_WDDMDEVICECAPS) };
    unsafe { core::ptr::write_bytes(args.pOutputData as *mut u8, 0, args.OutputDataSize as usize) };
    caps.WDDMVersion = SURFACE.driver_caps_version();
    STATUS_SUCCESS
}

unsafe fn query_physical_memory_caps(args: &DXGKARG_QUERYADAPTERINFO) -> NTSTATUS {
    if (args.OutputDataSize as usize) < size_of::<DXGK_PHYSICAL_MEMORY_CAPS>() {
        return STATUS_BUFFER_TOO_SMALL;
    }

    // SAFETY: pOutputData points to a DXGK_PHYSICAL_MEMORY_CAPS of sufficient size.
    let caps = unsafe { &mut *(args.pOutputData as *mut DXGK_PHYSICAL_MEMORY_CAPS) };
    unsafe { core::ptr::write_bytes(args.pOutputData as *mut u8, 0, args.OutputDataSize as usize) };
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
        core::ptr::write_bytes(args.pOutputData as *mut u8, 0, args.OutputDataSize as usize);
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
        core::ptr::write_bytes(args.pOutputData as *mut u8, 0, args.OutputDataSize as usize);
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

// WHY THE APERTURE IS ALWAYS SEGMENT 0 — from the live-KD decode of dxgmms2's
// paging DMA-pool init (INITDMAPOOLS_HANDOVER.md, 2026-06-21).
//
// `VIDMM_GLOBAL::InitDmaPools` builds ONE paging DMA pool and validates the FIRST
// reported segment (`segdesc[0]`): it requires that segment's per-attribute
// object's "can host a paging buffer" flag (`[attr+0x68] & 1`). A CPU-visible
// *memory* segment (the BAR) NEVER has that flag; a linear **aperture** segment
// does (classic WDDM paging-buffer backing via MAP_APERTURE_SEGMENT — what the
// proven viogpu3d driver reports, WDDM_FAKE_VIDMM §A2.2). The decode also showed
// VidMm DROPS a system-RAM-backed memory segment (only the BAR registered), so
// the old "RAM paging segment id 2" never existed.
//
// The `REPORT_APERTURE_PAGING_SEGMENT` lever that used to sit here selected
// between this and a BAR-only shape whose own doc comment described it as
// rejected by InitDmaPools with Code 43 *by design*. It was a `const bool = true`,
// so the false branch was const-folded out of the binary while still widening the
// state space every reader of `query_segments` had to consider. Deleted with the
// rest of the known-rejected shapes; the aperture-first rule is now unconditional
// and stated once, here.

/// PATH-A lever (2026-06-22): declare `DXGK_VIDMMCAPS::CrossAdapterResource` so the
/// OS can use Helios as the cross-adapter render side for the Looking Glass IDD's
/// composition swapchain. See the comment in `query_driver_caps`. Gated for clean
/// reversibility — flip to `false` and redeploy if it destabilizes boot.
/// `pub(crate)` is now vestigial: a grep finds exactly ONE reader, the `mem_caps`
/// OR below. The claim this comment used to make — that `create_allocation.rs`
/// gates a matching cross-adapter surface layout (aperture-eligible + 256-aligned
/// linear) on the same lever — is stale; that file does not read this const.
///
/// 2026-06-22 EXPERIMENT RESULT (1st try, `true`): declaring the cap made dxgkrnl
/// drive the GDI hardware-acceleration render path (`DxgkDdiRenderKm`) against the
/// cross-adapter resource — gated by our (mandatory-for-load)
/// `SupportKernelModeCommandBuffer` cap — and our `RenderKm` returned
/// `STATUS_NOT_IMPLEMENTED`, so dxgkrnl `call`ed a null DMA/submission pointer
/// (`dxgkrnl!ADAPTER_RENDER::DdiRenderGdi+0x140`, `call rax` rax=0 → kernel
/// 0xC0000005). FIX: `dxgkddi_render_km`/`dxgkddi_render_gdi` now record the command
/// into the DMA buffer + advance the output pointer + return SUCCESS
/// (`submit_command.rs`), satisfying the GDI-HW-accel DDI contract
/// (`gdi-hardware-acceleration.md`: CreateAllocation + GetStandardAllocationDriverData
/// + RenderKm). Re-tested with the cap enabled in build 22.22.57.0: DXGI still
/// presented with hDstRes=0 and no Blt/Blt1 destination, while restart churn hit
/// venus/DWM/explorer crashes. Keep disabled until we have a resource-allocation
/// contract that specifically requires it.
pub(crate) const DECLARE_CROSS_ADAPTER_RESOURCE: bool = false;

/// WDDM-sync-redesign M1 (2026-06-25) raised the adapter off the bring-up WDDM 1.3
/// surface so the OS enables the monitored-fence path that `D3DDDI_MONITORED_FENCE`
/// requires. That lever, and the display-surface lever that used to sit beside it,
/// are now the single [`crate::ddi::wddm_surface::SURFACE`] value — a partial raise
/// is internally inconsistent, dxgkrnl rejects it at AddAdapter, and two
/// independently-readable bools could express exactly that. See
/// `ddi/wddm_surface.rs` for the level history and `WDDM_SYNC_REDESIGN.md` M1.
///
/// The GpuMmu DDI surface (GPUMMUCAPS, page-table-level desc, SetRootPageTable,
/// GetRootPageTableSize, BuildPagingBuffer, SubmitCommand/SubmitCommandVirtual,
/// CreateProcess) is implemented (decorative/null engine), so the level only flips
/// the capability *declaration*.
/// `DirectFlipCaps` service knob (REG_DWORD, default 0): 0 = deny direct flip
/// everywhere (adapter cap + aperture segment flags — the truthful surface for
/// a zero-scanout render adapter; see the SupportDirectFlip comment in
/// `query_driver_caps`), nonzero = restore the legacy bring-up advertisement.
/// Read at AddAdapter/QUERYSEGMENT time (PASSIVE), same pattern as BarSegFlags.
/// The BAR knob descriptor is independently governed by BarSegFlags bit 5
/// (default off) and is not gated here.
fn direct_flip_advertised() -> bool {
    crate::diag::read_config_dword(b"DirectFlipCaps", 0) != 0
}

/// `CrossAdaptCaps` service knob (REG_DWORD, default 0): nonzero advertises the
/// `DXGK_VIDMMCAPS.CrossAdapterResource` bit (tier-1 cross-adapter copy support).
/// Without that bit the OS never attempts the cross-adapter *redirected-bitblt*
/// windowed present, so every legacy BLT-model swapchain (DXUT/FaceWorks, older
/// 3DMark) is declared `DXGI_STATUS_OCCLUDED` and renders transparent (priority #1,
/// root-caused 2026-07-08 with `tools/d3d11_triangle.cpp`: FLIP composites, BLT
/// occludes). The redirection-surface machinery already exists — GDISURFACE is
/// handled in `GetStandardAllocationDriverData` and backed by a real venus blob in
/// `create_one`; this cap is the only gate. Kept OFF by default (the compiled
/// `DECLARE_CROSS_ADAPTER_RESOURCE` const stays false, boot-proven surface); the
/// knob is read at AddAdapter (PASSIVE), same pattern as `DirectFlipCaps`, so it
/// A/Bs via `reg add ... /v CrossAdaptCaps /t REG_DWORD /d 1` + `pnputil
/// /restart-device` with NO reboot once this KMD is deployed. Its value is visible
/// in the `0x01D4` MemoryManagementCaps diag record (CrossAdapterResource = bit 4).
/// NB: `read_config_dword` truncates names to 14 chars — this name is exactly 14.
fn cross_adapter_advertised() -> bool {
    crate::diag::read_config_dword(b"CrossAdaptCaps", 0) != 0
}

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
        if direct_flip_advertised() {
            s.Flags.__bindgen_anon_1.__bindgen_anon_1.set_DirectFlip(1);
        }
        // CpuVisible deliberately 0 — an aperture holds no bits (viogpu3d :558).
        s.BaseAddress.QuadPart = APERTURE_BASE_ADDRESS;
        s.Size = APERTURE_SEGMENT_SIZE;
        s.CommitLimit = APERTURE_SEGMENT_SIZE;
    }
}

/// Write the viogpu3d-style linear aperture descriptor for WDDM 1.2/1.3
/// `DXGK_QUERYSEGMENTOUT3`.
/// SAFETY: `seg` points to a writable `DXGK_SEGMENTDESCRIPTOR3`.
unsafe fn write_aperture_descriptor3(seg: *mut DXGK_SEGMENTDESCRIPTOR3) {
    unsafe {
        core::ptr::write_bytes(seg as *mut u8, 0, size_of::<DXGK_SEGMENTDESCRIPTOR3>());
        let s = &mut *seg;
        s.Flags.__bindgen_anon_1.__bindgen_anon_1.set_Aperture(1);
        s.Flags
            .__bindgen_anon_1
            .__bindgen_anon_1
            .set_CacheCoherent(1);
        if direct_flip_advertised() {
            s.Flags.__bindgen_anon_1.__bindgen_anon_1.set_DirectFlip(1);
        }
        // CpuVisible deliberately 0 — a linear aperture redirects system-memory
        // MDLs through MAP_APERTURE_SEGMENT instead of exposing device memory.
        s.BaseAddress.QuadPart = APERTURE_BASE_ADDRESS;
        s.Size = APERTURE_SEGMENT_SIZE;
        s.CommitLimit = APERTURE_SEGMENT_SIZE;
    }
}

/// Write the legacy `DXGK_QUERYSEGMENTOUT` descriptor used if dxgkrnl falls back
/// from QUERYSEGMENT3.
/// SAFETY: `seg` points to a writable `DXGK_SEGMENTDESCRIPTOR`.
unsafe fn write_aperture_descriptor_legacy(seg: *mut DXGK_SEGMENTDESCRIPTOR) {
    unsafe {
        core::ptr::write_bytes(seg as *mut u8, 0, size_of::<DXGK_SEGMENTDESCRIPTOR>());
        let s = &mut *seg;
        s.Flags.__bindgen_anon_1.__bindgen_anon_1.set_Aperture(1);
        s.Flags
            .__bindgen_anon_1
            .__bindgen_anon_1
            .set_CacheCoherent(1);
        if direct_flip_advertised() {
            s.Flags.__bindgen_anon_1.__bindgen_anon_1.set_DirectFlip(1);
        }
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

/// Segment-3 descriptor, FULLY KNOB-DRIVEN (AddAdapter shape bisect: both the
/// classic-CpuVisible and CpuHostAperture shapes were rejected identically, as
/// were 64 MiB and RAM-backed variants — the remaining hypotheses are flag
/// combinations and GPU-physical BaseAddress overlap with segment 2, and each
/// compiled-in variant costs an owner reboot; registry knobs + devcon restart
/// cost nothing).
///
///   `BarSegFlags` (REG_DWORD, default 0x1C = CacheCoherent |
///                  SupportsCpuHostAperture | SupportsCachedCpuHostAperture):
///     bit0 Aperture   bit1 CpuVisible(CpuTranslatedAddress=gpa)
///     bit2 CacheCoherent    bit3 SupportsCpuHostAperture
///     bit4 SupportsCachedCpuHostAperture   bit5 DirectFlip
///     bit6 PopulatedFromSystemMemory
///   `BarSegBaseMB` (REG_DWORD, default 0): GPU-physical BaseAddress in MiB —
///     segment 2 sits at 0; a nonzero value (e.g. 8192 = 0x2_0000_0000) tests
///     the BaseAddress-overlap hypothesis.
///
/// SAFETY: `seg` points to a writable `DXGK_SEGMENTDESCRIPTOR4`.
unsafe fn write_bar_knob_descriptor(seg: *mut DXGK_SEGMENTDESCRIPTOR4, gpa: u64, len: u64) {
    let flags = crate::diag::read_config_dword(b"BarSegFlags", 0x1C);
    let base_mb = crate::diag::read_config_dword(b"BarSegBaseMB", 0);
    crate::diag::record_named_bytes(b"BarF", flags);
    crate::diag::record_named_bytes(b"BarB", base_mb);
    unsafe {
        core::ptr::write_bytes(seg as *mut u8, 0, size_of::<DXGK_SEGMENTDESCRIPTOR4>());
        let s = &mut *seg;
        s.BaseAddress.QuadPart = (base_mb as i64) << 20;
        let f = &mut s.Flags.__bindgen_anon_1.__bindgen_anon_1;
        if flags & 0x01 != 0 {
            f.set_Aperture(1);
        }
        if flags & 0x02 != 0 {
            f.set_CpuVisible(1);
            (*s.__bindgen_anon_1.CpuTranslatedAddress.as_mut()).QuadPart = gpa as i64;
        }
        if flags & 0x04 != 0 {
            f.set_CacheCoherent(1);
        }
        if flags & 0x08 != 0 {
            f.set_SupportsCpuHostAperture(1);
        }
        if flags & 0x10 != 0 {
            f.set_SupportsCachedCpuHostAperture(1);
        }
        if flags & 0x20 != 0 {
            f.set_DirectFlip(1);
        }
        if flags & 0x40 != 0 {
            f.set_PopulatedFromSystemMemory(1);
        }
        if flags & 0x08 != 0 && flags & 0x02 == 0 {
            // CpuHostAperture and CpuTranslatedAddress share a union — only
            // write the aperture region when CpuVisible didn't claim it.
            let cpu_host = DXGK_CPUHOSTAPERTURE {
                PhysicalAddress: gpa,
                SizeInPages: (len / 4096).min(u32::MAX as u64) as u32,
            };
            *s.__bindgen_anon_1.CpuHostAperture.as_mut() = cpu_host;
        }
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

    out.pSegmentDescriptor = descriptors;
    let stride = size_of::<DXGK_SEGMENTDESCRIPTOR4>();
    out.SegmentDescriptorStride = stride as u64;
    out.PagingBufferSize = 64 * 1024;
    out.PagingBufferPrivateDataSize = 0;

    {
        // Aperture (id 1, idx 0) = paging-buffer host, which is what passes
        // InitDmaPools (see the note above the aperture constants). The second
        // slot is the BAR under `BarSegTopology::ApertureAndBar`, or the
        // AddDevice-time RAM-backed cpu-host MEMORY segment under `Disabled`.
        //
        // The RAM slot must use `paging_ram`, not `page_table_window`:
        // QuerySegment4 runs before StartDevice's venus allocation, so
        // `page_table_window` is not available when VidMm builds its segment
        // table. If the contiguous RAM allocation was unavailable too, this
        // falls back to NbSegment = 1 (aperture only).
        let page_table = adapter.paging_ram();
        let bar = adapter.bar_segment().map(|b| (b.gpa, b.size));
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
        crate::diag::record(if bar.is_some() {
            0x0905_0001
        } else {
            0x0905_0000
        });

        // Segment TABLE per topology (`BarSegTopology` — see `setup_bar_segment`).
        // The list is (writer, args) per positional slot; ids are positional
        // (index 0 = id 1). The aperture is ALWAYS first (InitDmaPools
        // validates segdesc[0] for paging-buffer-host capability).
        enum Seg {
            Aperture,
            RamCpuHost(u64, u64),
            Bar(u64, u64),
        }
        let mut table: [Option<Seg>; 2] = [Some(Seg::Aperture), None];
        // Exhaustive over the two shapes, with no wildcard arm: a segment
        // topology can no longer be selected by falling off the end of a match
        // over an integer knob.
        match (bar, page_table) {
            // ApertureAndBar (production): aperture + the BAR as id 2. The
            // paging-RAM segment is dropped — it was vestigial, since page
            // tables live in system segment 0 and paging buffers in the
            // aperture.
            //
            // Note this arm no longer depends on `page_table`. Under the deleted
            // topologies a `None` paging-RAM allocation made the whole match fall
            // through to the wildcard, which silently dropped the BAR from the
            // REPORTED table while `adapter.bar_segment` stayed `Some` — every
            // BAR consumer then placed allocations against a segment id dxgkrnl
            // was never told about (k-capsescape-04).
            (Some((gpa, size)), _) => {
                table[1] = Some(Seg::Bar(gpa, size));
            }
            // Disabled: aperture + the paging-RAM cpu-host segment as id 2. The
            // recovery baseline that always binds.
            (None, Some((rgpa, rsize))) => {
                table[1] = Some(Seg::RamCpuHost(rgpa, rsize));
            }
            // Disabled with no contiguous RAM either: aperture only.
            (None, None) => {}
        }
        out.NbSegment = table.iter().flatten().count() as u32;
        out.PagingBufferSegmentId = gpummu::APERTURE_SEGMENT_ID;
        if !descriptors.is_null() {
            for (idx, seg) in table.iter().flatten().enumerate() {
                // SAFETY: the second QUERYSEGMENT4 call provides NbSegment
                // descriptors; idx < NbSegment by construction.
                let d = unsafe {
                    (descriptors as *mut u8).add(idx * stride) as *mut DXGK_SEGMENTDESCRIPTOR4
                };
                match seg {
                    Seg::Aperture => unsafe { write_aperture_descriptor(d) },
                    Seg::RamCpuHost(gpa, size) => {
                        crate::diag::record(0x0903_0000 | (((gpa >> 12) as u32) & 0xFFFF));
                        crate::diag::record(0x0904_0000 | (((size >> 12) as u32) & 0xFFFF));
                        // SAFETY: d is a writable descriptor slot (above).
                        unsafe { write_cpu_host_memory_descriptor(d, *gpa, *size) };
                    }
                    Seg::Bar(gpa, size) => {
                        crate::diag::record(0x0906_0000 | (((size >> 20) as u32) & 0xFFFF));
                        // Knob-driven descriptor (BarSegFlags/BarSegBaseMB) —
                        // the AddAdapter shape bisect. The production shape is
                        // CpuHostAperture-like: DxgkDdiMapCpuHostAperture maps
                        // each allocation's venus blob at the dxgkrnl-chosen
                        // aperture offset within this window.
                        unsafe { write_bar_knob_descriptor(d, *gpa, *size) };
                    }
                }
            }
        }
    }

    crate::diag::record(0x0900_0002 | (out.NbSegment << 8));
    STATUS_SUCCESS
}

unsafe fn query_segments3(args: &DXGKARG_QUERYADAPTERINFO) -> NTSTATUS {
    if (args.OutputDataSize as usize) < size_of::<DXGK_QUERYSEGMENTOUT3>() {
        return STATUS_BUFFER_TOO_SMALL;
    }

    let out = unsafe { &mut *(args.pOutputData as *mut DXGK_QUERYSEGMENTOUT3) };
    let descriptors = out.pSegmentDescriptor;
    unsafe {
        core::ptr::write_bytes(
            out as *mut _ as *mut u8,
            0,
            size_of::<DXGK_QUERYSEGMENTOUT3>(),
        );
    }
    out.pSegmentDescriptor = descriptors;

    crate::diag::record(if descriptors.is_null() {
        0x0913_0000
    } else {
        0x0913_0001
    });

    out.NbSegment = 1;
    if descriptors.is_null() {
        // Segment queries are a two-call protocol. On the first call dxgkrnl
        // passes pSegmentDescriptor = NULL and the driver must report only the
        // segment count; paging-buffer fields are valid only with descriptors.
        return STATUS_SUCCESS;
    }

    out.PagingBufferSegmentId = gpummu::APERTURE_SEGMENT_ID;
    out.PagingBufferSize = 10 * 4096;
    out.PagingBufferPrivateDataSize = 0;

    unsafe { write_aperture_descriptor3(descriptors) };

    STATUS_SUCCESS
}

unsafe fn query_segments_legacy(args: &DXGKARG_QUERYADAPTERINFO) -> NTSTATUS {
    if (args.OutputDataSize as usize) < size_of::<DXGK_QUERYSEGMENTOUT>() {
        return STATUS_BUFFER_TOO_SMALL;
    }

    let out = unsafe { &mut *(args.pOutputData as *mut DXGK_QUERYSEGMENTOUT) };
    let descriptors = out.pSegmentDescriptor;
    unsafe {
        core::ptr::write_bytes(
            out as *mut _ as *mut u8,
            0,
            size_of::<DXGK_QUERYSEGMENTOUT>(),
        );
    }
    out.pSegmentDescriptor = descriptors;

    crate::diag::record(if descriptors.is_null() {
        0x0912_0000
    } else {
        0x0912_0001
    });

    out.NbSegment = 1;
    if descriptors.is_null() {
        // Same two-call contract as QUERYSEGMENT3: first call reports only
        // NbSegment, second call supplies pSegmentDescriptor and gets details.
        return STATUS_SUCCESS;
    }

    out.PagingBufferSegmentId = gpummu::APERTURE_SEGMENT_ID;
    out.PagingBufferSize = 10 * 4096;
    out.PagingBufferPrivateDataSize = 0;

    unsafe { write_aperture_descriptor_legacy(descriptors) };

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
    // The node memory model must match the DRIVERCAPS surface, so it reads the same
    // value: a GpuMmu level advertises the GPU-virtual-addressing model, WDDM 1.3
    // stays at the viogpu3d-style surface with no explicit memory model. GpuMmu and
    // IoMmu are mutually exclusive.
    node.GpuMmuSupported = BOOLEAN::from(SURFACE.gpu_mmu());
    node.IoMmuSupported = 0;
    // FriendlyName, Flags stay zeroed.
    STATUS_SUCCESS
}
