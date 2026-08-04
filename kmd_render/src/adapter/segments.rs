//! Memory topology: the real-RAM paging segment and the host-visible BAR
//! segment, plus the contiguous-allocation helper that backs the former.
//!
//! Moved verbatim out of `adapter.rs` by T8/R1101. The fields these describe
//! stay declared in [`super`]; this module owns only the descriptors and the
//! two allocation entry points.

use core::ptr::NonNull;

use wdk_sys::ntddk::{MmAllocateContiguousMemory, MmGetPhysicalAddress};
use wdk_sys::PHYSICAL_ADDRESS;

use super::AdapterContext;

/// Size of the real-RAM-backed segment Helios reports for VidMm's page tables and
/// paging buffers. The host-visible venus BAR (segment 1) is a CpuVisible MEMORY
/// segment only where a blob is RESOURCE_MAP_BLOB'd, so VidMm cannot allocate the
/// system context's paging buffer / page tables there (an access to an unbacked
/// BAR offset faults). VidMm needs a genuinely-backed, physically-contiguous,
/// CpuVisible segment for that bookkeeping — this block. Modest: a few page tables
/// + the 64 KiB paging buffer during bring-up; bump if VidMm's allocation within
/// the segment ever fails.
const PAGING_RAM_SIZE: usize = 8 * 1024 * 1024;

/// A real-RAM-backed region reported to VidMm as a CpuVisible memory segment, used
/// for page-table / paging-buffer storage (see [`PAGING_RAM_SIZE`]).
pub struct PagingRam {
    /// Kernel VA (for free); the region is mapped non-paged by the allocator.
    pub(super) va: NonNull<u8>,
    /// Guest-physical base — the segment's `CpuTranslatedAddress`.
    pub phys: u64,
    /// Region length in bytes (== reported segment Size/CommitLimit).
    pub size: u64,
}

/// The BAR memory segment: the head of the host-visible venus
/// window, reserved as dxgkrnl's CPU-host-aperture region. Blobs are mapped
/// into it at dxgkrnl-chosen aperture offsets by `DxgkDdiMapCpuHostAperture`
/// (`cpu_host_aperture.rs`). See the two-memory-split root cause
/// (HANDOFF_GDI_EXECUTOR_2026_07_05.md ★FINAL).
pub struct BarSegment {
    /// Guest-physical base = the host-visible window base (partition offset 0).
    pub gpa: u64,
    /// CPU-aperture partition length in bytes (== the declared
    /// `DXGK_CPUHOSTAPERTURE` span == the `reserve_window_prefix` given to the
    /// blob-window allocator). `VidMmVramMB` may make the surrounding device
    /// memory segment larger without enlarging this mapping window.
    pub size: u64,
    /// The WDDM segment id this region is reported as. All BAR-segment consumers
    /// key off this field.
    ///
    /// Always [`crate::ddi::gpummu::MEMORY_SEGMENT_ID`] now that the reported
    /// topology is either "aperture only" or "aperture + BAR". The field stays
    /// (rather than becoming a bare const at each consumer) because it is the
    /// one place the positional id and the segment it describes are tied
    /// together.
    pub seg_id: u32,
}

/// The existence of a [`BarSegment`] IS the topology: `Some` is
/// [`crate::ddi::bar_segment::BarSegTopology::ApertureAndBar`], `None` is
/// `Disabled`. The old `topo: u32` field carried a `BarSegMode` value that a
/// second file then re-matched positionally against the literals 10 and 11;
/// with the rejected shapes deleted there is nothing left for it to say.

impl AdapterContext {
    /// Allocate a zeroed, physically-contiguous non-paged RAM block. PASSIVE.
    pub(crate) fn alloc_contiguous_ram(size: usize) -> Option<PagingRam> {
        // Permit the contiguous block anywhere in the 64-bit physical space.
        let mut highest: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
        highest.QuadPart = i64::MAX;
        // SAFETY: PASSIVE_LEVEL; allocates `size` bytes of physically-
        // contiguous non-paged memory, or null on failure.
        let va = unsafe { MmAllocateContiguousMemory(size as u64, highest) };
        let Some(va) = NonNull::new(va as *mut u8) else {
            crate::diag::record(0x0A00_00E3);
            crate::diag::fault(crate::diag::FaultCounter::StRam, size as u32);
            return None;
        };
        // SAFETY: zero the region so VidMm never reads stale bytes.
        unsafe { core::ptr::write_bytes(va.as_ptr(), 0, size) };
        // SAFETY: `va` is a valid non-paged kernel address.
        let phys = unsafe { MmGetPhysicalAddress(va.as_ptr() as *mut _).QuadPart } as u64;
        crate::diag::record(0x0A00_0003);
        crate::diag::record((phys >> 12).min(0xFFFF_FFFF) as u32);
        Some(PagingRam {
            va,
            phys,
            size: size as u64,
        })
    }

    /// Allocate the real-RAM-backed paging/page-table segment. Called from
    /// StartDevice, after PnP has accepted the display miniport context, so AddDevice
    /// stays a cheap context-allocation step.
    pub(crate) fn alloc_paging_ram() -> Option<PagingRam> {
        Self::alloc_contiguous_ram(PAGING_RAM_SIZE)
    }
}
