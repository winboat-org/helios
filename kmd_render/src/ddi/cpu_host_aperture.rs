//! Minimal CPU host aperture support for the decorative GpuMmu path.
//!
//! VidMm uses the CPU host aperture during adapter initialization to obtain a CPU
//! VA for the system paging-process page directory/page tables. Helios does not
//! have hardware page tables, so the map/unmap DDIs only acknowledge the mapping:
//! the CPU-visible BAR page receives any writes, and no hardware consumes them.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::dxgk::*;

pub static CPU_HOST_MAP_COUNT: AtomicU32 = AtomicU32::new(0);
pub static CPU_HOST_UNMAP_COUNT: AtomicU32 = AtomicU32::new(0);
pub static CPU_HOST_LAST_MAP: AtomicU64 = AtomicU64::new(0);
pub static CPU_HOST_LAST_UNMAP: AtomicU64 = AtomicU64::new(0);

pub unsafe extern "C" fn dxgkddi_map_cpu_host_aperture(
    h_adapter: *mut c_void,
    map: IN_CONST_PDXGKARG_MAPCPUHOSTAPERTURE,
) -> NTSTATUS {
    if h_adapter.is_null() || map.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    // SAFETY: valid per DDI contract; we only inspect scalar fields. The page
    // arrays describe the mapping VidMm wants. The fake GpuMmu does not need to
    // program real aperture PTEs because the guest page-table bytes are decorative.
    let args = unsafe { &*map };
    CPU_HOST_MAP_COUNT.fetch_add(1, Ordering::Relaxed);
    CPU_HOST_LAST_MAP.store(
        ((args.SegmentId as u64) << 48)
            | ((args.PhysicalAdapterIndex as u64) << 32)
            | (args.NumberOfPages & 0xFFFF_FFFF),
        Ordering::Relaxed,
    );

    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_unmap_cpu_host_aperture(
    h_adapter: *mut c_void,
    unmap: IN_CONST_PDXGKARG_UNMAPCPUHOSTAPERTURE,
) -> NTSTATUS {
    if h_adapter.is_null() || unmap.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    // SAFETY: valid per DDI contract; no hardware aperture PTEs exist to tear
    // down, so this is only tracked for ntoseye/diag correlation.
    let args = unsafe { &*unmap };
    CPU_HOST_UNMAP_COUNT.fetch_add(1, Ordering::Relaxed);
    CPU_HOST_LAST_UNMAP.store(
        ((args.SegmentId as u64) << 48)
            | ((args.PhysicalAdapterIndex as u64) << 32)
            | (args.NumberOfPages & 0xFFFF_FFFF),
        Ordering::Relaxed,
    );

    STATUS_SUCCESS
}
