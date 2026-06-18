//! `DxgkDdiBuildPagingBuffer` and the GPU-VA root-page-table DDIs.
//!
//! The real Venus-backed WDDM memory model is not implemented yet. For early
//! install smoke testing we provide a null paging engine that consumes dxgkrnl's
//! paging requests without emitting DMA, while render/UMD paths remain disabled.

use core::ffi::c_void;

use crate::dxgk::*;

/// `DxgkDdiBuildPagingBuffer` — translate a memory-management operation into GPU
/// DMA commands written into `pDmaBuffer`.
///
/// dxgkrnl drives GPU-VA memory operations through here. This must become a real
/// implementation before the driver advertises GPU MMU/paging capability.
pub unsafe extern "C" fn dxgkddi_build_paging_buffer(
    h_adapter: *mut c_void,
    build_paging_buffer: *mut DXGKARG_BUILDPAGINGBUFFER,
) -> NTSTATUS {
    if h_adapter.is_null() || build_paging_buffer.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    // Do not advance pDmaBuffer. Dxgkrnl supplied a buffer, but this bring-up
    // engine has no real page-table or aperture commands to write yet.
    let args = unsafe { &*build_paging_buffer };
    crate::diag::record(0x0500_0000 | (args.Operation as u32 & 0xFFFF));
    STATUS_SUCCESS
}

// ── GPU-VA root page-table DDIs. ────────────────────────────────────────────

/// `DxgkDdiSetRootPageTable` — point a context at its GPU-VA root page table.
/// No-op for now; callers must not reach this until GPU-VA support is real.
pub unsafe extern "C" fn dxgkddi_set_root_page_table(
    _h_adapter: IN_CONST_HANDLE,
    _set_root_page_table: IN_CONST_PDXGKARG_SETROOTPAGETABLE,
) {
}

/// `DxgkDdiGetRootPageTableSize` — report the root page-table size in bytes.
///
/// Returns zero until a real page-table design lands.
pub unsafe extern "C" fn dxgkddi_get_root_page_table_size(
    _h_adapter: IN_CONST_HANDLE,
    _get_root_page_table_size: INOUT_PDXGKARG_GETROOTPAGETABLESIZE,
) -> SIZE_T {
    0
}
