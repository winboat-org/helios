//! Decorative GpuMmu geometry — the single source of truth for the fake-but-
//! coherent WDDM GPU memory model (Step 2 of the IDD render plan).
//!
//! WHY THIS EXISTS. To make VidMm accept Helios as a render adapter (so DWM can
//! composite the whole desktop on Helios → venus → host GPU), the driver declares
//! a GpuMmu memory model. But Helios has no guest GPU MMU: venus addresses host
//! resources by opaque id and the **host GPU owns the real MMU**, so the guest
//! page tables are *decorative* — their content is never read by any hardware
//! (WDDM_FAKE_VIDMM_RESEARCH.md §A3.7). What is NOT optional is the *structural*
//! bookkeeping VidMm maintains regardless: the declared `DXGK_GPUMMUCAPS` /
//! per-level `DXGK_PAGE_TABLE_LEVEL_DESC` geometry must be **self-consistent**,
//! and every page-table DDI (`GetRootPageTableSize`, `SetRootPageTable`,
//! `BuildPagingBuffer`/`UpdatePageTable`) must return success and values
//! consistent with this geometry, even when their hardware effect is nil.
//!
//! THE GEOMETRY (system-memory page tables, one CPU page per table):
//!   - 40-bit GPU virtual addresses, 4-KiB pages (12-bit page offset).
//!     The live 0x10E:0x49 op applies PTEs at GPU VA 0x8001064300, so 40 bits
//!     covers the current VidMm-assigned range.
//!   - 4 translation levels: 9-bit leaf + 9-bit L1 + 9-bit L2 + 1-bit root.
//!     Every table is one 4-KiB CPU page. This is deliberate: WDDM reserves
//!     `DXGK_PAGE_TABLE_LEVEL_DESC::*SegmentId = 0` for system memory, but the
//!     docs also cap system-memory page tables at 4 KiB. The previous 2-level
//!     19-bit root required a 4-MiB root allocation and forced us into a fake
//!     device segment, which is exactly where `0x10E:0x49` fired.
//!
//! THE PAGE-TABLE UPDATE MODE. The 2026-06-21 ntoseye decode of the 0x10E:0x49
//! crash showed `CPU_VIRTUAL` dies inside
//! `VIDMM_PAGE_TABLE_BASE::GetCpuVisibleAddress`: VidMm tries to resolve a CPU VA
//! for page-table segment 2 and fatals when that segment's internal attribute word
//! lacks the accepted CPU-update capability bits. That held for RAM, unbacked-BAR,
//! and venus-backed variants, so the backing was not the deciding contract. Use
//! `GPU_PHYSICAL` instead: VidMm routes PTE writes through `BuildPagingBuffer`
//! (`UPDATE_PAGE_TABLE`) + `SubmitCommand`, which our null engine consumes and
//! no-ops — nothing ever CPU-touches decorative page-table bytes.

use core::mem::size_of;

use crate::dxgk::_DXGK_PAGETABLEUPDATEMODE::DXGK_PAGETABLEUPDATE_GPU_PHYSICAL;
use crate::dxgk::*;

/// Total GPU virtual-address width (bits). 40 covers VidMm's current
/// 0x8001064300 page-table init range and keeps the root table bounded.
pub const VIRTUAL_ADDRESS_BIT_COUNT: u32 = 40;
/// Bits consumed by the in-page byte offset (4-KiB pages).
pub const PAGE_SHIFT_BITS: u32 = 12;
/// Bits that index a leaf page table (512 entries ⇒ 9 bits).
pub const LEAF_PAGE_TABLE_INDEX_BIT_COUNT: u32 = 9;
/// Number of translation levels. Four levels keep every table at or below 4 KiB,
/// allowing all page-table allocations to use system memory segment id 0.
pub const PAGE_TABLE_LEVEL_COUNT: u32 = 4;
/// Intermediate non-root levels are also one 4-KiB page each.
pub const INTERMEDIATE_PAGE_TABLE_INDEX_BIT_COUNT: u32 = 9;
/// Root index bits are "the rest" above page offset + three 9-bit levels.
pub const ROOT_PAGE_TABLE_INDEX_BIT_COUNT: u32 = VIRTUAL_ADDRESS_BIT_COUNT
    - PAGE_SHIFT_BITS
    - LEAF_PAGE_TABLE_INDEX_BIT_COUNT
    - 2 * INTERMEDIATE_PAGE_TABLE_INDEX_BIT_COUNT;
/// Decorative hardware PTE size, in bytes. 512 entries × 8 = one 4-KiB page.
pub const PTE_SIZE_BYTES: u32 = 8;
/// Size VidMm allocates per leaf page table (one 4-KiB page).
pub const LEAF_PAGE_TABLE_SIZE_BYTES: u32 =
    (1u32 << LEAF_PAGE_TABLE_INDEX_BIT_COUNT) * PTE_SIZE_BYTES;
/// Every table level is one CPU page in the system-memory contract.
pub const PAGE_TABLE_SIZE_BYTES: u32 = 4096;
/// Page-table alignment (one page).
pub const PAGE_TABLE_ALIGNMENT_BYTES: u32 = 4096;
/// Segment id of the **aperture** segment VidMm carves *paging buffers* (the DMA
/// buffers that carry paging commands) from. This MUST be an aperture segment:
/// the live KD decode of `dxgmms2!VidMm...InitDmaPools` (INITDMAPOOLS_HANDOVER.md)
/// proved the paging DMA-pool validates the FIRST reported segment (`segdesc[0]`)
/// and requires its per-attribute object's "can host a paging buffer" flag
/// (`[attr+0x68] & 1`) to be set — which a CPU-visible *memory* segment (the BAR)
/// never has, but a linear **aperture** segment does (it is the classic WDDM
/// paging-buffer backing: system-memory MDLs mapped via MAP_APERTURE_SEGMENT). So
/// the aperture is reported FIRST (segment id 1) and is `PagingBufferSegmentId`.
/// Matches the proven viogpu3d virtio-gpu WDDM segment (WDDM_FAKE_VIDMM §A2.2).
pub const APERTURE_SEGMENT_ID: u32 = 1;
/// Segment id of the CPU-visible **memory** segment (host-visible venus BAR) used
/// by render allocations. Page-table allocations deliberately use system memory
/// segment id 0 instead; see [`SYSTEM_MEMORY_SEGMENT_ID`].
pub const MEMORY_SEGMENT_ID: u32 = 2;
/// Segment id of the BAR MEMORY SEGMENT (segment index 2, id 3): the head
/// partition of the host-visible venus window, reported with the CPU-HOST-
/// APERTURE shape (the only memory-segment CPU-access shape this dxgkrnl
/// accepts — the classic CpuVisible/CpuTranslatedAddress form fails AddAdapter
/// with Code 43, proven 2026-07-05). CPU access (Lock / win32k GDI raster) is
/// REAL: `DxgkDdiMapCpuHostAperture` maps each allocation's venus blob at the
/// dxgkrnl-chosen aperture offset inside the window, so CPU raster, the
/// RenderGdi executor, and the host GPU all address ONE memory — the
/// two-memory-split fix.
pub const BAR_SEGMENT_ID: u32 = 3;
/// WDDM reserves page-table segment id 0 for system memory. This avoids the
/// `VIDMM_PAGE_TABLE_BASE::GetCpuVisibleAddress` segment-attribute path that
/// bugchecks when a fake page-table device segment is dropped or lacks bit 13.
pub const SYSTEM_MEMORY_SEGMENT_ID: u32 = 0;

// Compile-time guards: the declared geometry must be internally consistent, or
// VidMm will exercise the page-table DDIs against a contradiction.
const _: () = assert!(
    PAGE_SHIFT_BITS
        + LEAF_PAGE_TABLE_INDEX_BIT_COUNT
        + 2 * INTERMEDIATE_PAGE_TABLE_INDEX_BIT_COUNT
        + ROOT_PAGE_TABLE_INDEX_BIT_COUNT
        == VIRTUAL_ADDRESS_BIT_COUNT
);
const _: () = assert!(PAGE_TABLE_LEVEL_COUNT == 4);
const _: () = assert!(LEAF_PAGE_TABLE_SIZE_BYTES == 4096);
const _: () = assert!(ROOT_PAGE_TABLE_INDEX_BIT_COUNT <= 9);
const _: () = assert!(PAGE_TABLE_SIZE_BYTES == 4096);

/// Fill a `DXGK_GPUMMUCAPS` (`DXGKQAITYPE_GPUMMUCAPS = 13`) with the decorative
/// geometry above. All optional behaviour bits stay off (we back none of them):
/// `CacheCoherentMemorySupported`, `LargePageSupported`, `DualPteSupported`, etc.
/// are 0 until their DDI path is real (the checklist rule: unknown stays
/// unadvertised). Returns `STATUS_SUCCESS` / `STATUS_BUFFER_TOO_SMALL`.
pub unsafe fn fill_gpummu_caps(args: &DXGKARG_QUERYADAPTERINFO) -> NTSTATUS {
    if (args.OutputDataSize as usize) < size_of::<DXGK_GPUMMUCAPS>() {
        return STATUS_BUFFER_TOO_SMALL;
    }
    // SAFETY: pOutputData points to a writable DXGK_GPUMMUCAPS of sufficient size.
    let caps = unsafe { &mut *(args.pOutputData as *mut DXGK_GPUMMUCAPS) };
    unsafe { core::ptr::write_bytes(caps as *mut _ as *mut u8, 0, size_of::<DXGK_GPUMMUCAPS>()) };

    // No optional capability bits — flags word stays 0.
    caps.__bindgen_anon_1.Value = 0;
    // GPU_PHYSICAL: page-table updates come through BuildPagingBuffer
    // UPDATE_PAGE_TABLE and are retired by the null engine. CPU_VIRTUAL was proven
    // wrong by KD: it crashes in VidMm's CPU-visible-address resolver before our
    // paging DDI can participate.
    caps.PageTableUpdateMode = DXGK_PAGETABLEUPDATE_GPU_PHYSICAL;
    caps.VirtualAddressBitCount = VIRTUAL_ADDRESS_BIT_COUNT;
    // We don't advertise 64-KiB pages; report the 4-KiB leaf size for consistency.
    caps.LeafPageTableSizeFor64KPagesInBytes = LEAF_PAGE_TABLE_SIZE_BYTES;
    caps.PageTableLevelCount = PAGE_TABLE_LEVEL_COUNT;
    // LegacyBehaviors stays zeroed (no SourcePageTableVaInTransfer).
    STATUS_SUCCESS
}

/// Fill one `DXGK_PAGE_TABLE_LEVEL_DESC` (`DXGKQAITYPE_PAGETABLELEVELDESC = 14`).
/// VidMm queries this once per level (`DXGK_QUERYPAGETABLELEVELDESCIN::LevelIndex`,
/// 0 = leaf, 1 = root for the two-level scheme.
pub unsafe fn fill_page_table_level_desc(
    args: &DXGKARG_QUERYADAPTERINFO,
    page_table_segment_id: u32,
) -> NTSTATUS {
    if (args.OutputDataSize as usize) < size_of::<DXGK_PAGE_TABLE_LEVEL_DESC>() {
        return STATUS_BUFFER_TOO_SMALL;
    }
    // Validate the requested level against the declared level count.
    if (args.InputDataSize as usize) < size_of::<DXGK_QUERYPAGETABLELEVELDESCIN>()
        || args.pInputData.is_null()
    {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: bounds-checked; the input is a DXGK_QUERYPAGETABLELEVELDESCIN.
    let level_in = unsafe { &*(args.pInputData as *const DXGK_QUERYPAGETABLELEVELDESCIN) };
    if level_in.LevelIndex as u32 >= PAGE_TABLE_LEVEL_COUNT {
        return STATUS_INVALID_PARAMETER;
    }

    // SAFETY: pOutputData points to a writable DXGK_PAGE_TABLE_LEVEL_DESC.
    let desc = unsafe { &mut *(args.pOutputData as *mut DXGK_PAGE_TABLE_LEVEL_DESC) };
    unsafe {
        core::ptr::write_bytes(
            desc as *mut _ as *mut u8,
            0,
            size_of::<DXGK_PAGE_TABLE_LEVEL_DESC>(),
        );
    }
    desc.PageTableIndexBitCount = match level_in.LevelIndex as u32 {
        0 => LEAF_PAGE_TABLE_INDEX_BIT_COUNT,
        1 | 2 => INTERMEDIATE_PAGE_TABLE_INDEX_BIT_COUNT,
        _ => ROOT_PAGE_TABLE_INDEX_BIT_COUNT,
    };
    desc.PageTableSegmentId = page_table_segment_id;
    desc.PagingProcessPageTableSegmentId = page_table_segment_id;
    desc.PageTableSizeInBytes = PAGE_TABLE_SIZE_BYTES;
    desc.PageTableAlignmentInBytes = PAGE_TABLE_ALIGNMENT_BYTES;
    STATUS_SUCCESS
}

/// Byte size of a root page table that must address `num_pte` entries, consistent
/// with the declared `PTE_SIZE_BYTES`. Used by `DxgkDdiGetRootPageTableSize`.
/// Rounded up to a page; never zero for a non-zero request.
pub fn root_page_table_size_bytes(num_pte: u32) -> SIZE_T {
    if num_pte == 0 {
        0
    } else {
        PAGE_TABLE_SIZE_BYTES as SIZE_T
    }
}
