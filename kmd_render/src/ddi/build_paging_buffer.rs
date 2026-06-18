//! `DxgkDdiBuildPagingBuffer` and the GPU-VA root-page-table DDIs.
//!
//! The real Venus-backed WDDM memory model is not implemented yet. For early
//! install smoke testing we provide a null paging engine that consumes dxgkrnl's
//! paging requests without emitting DMA, while render/UMD paths remain disabled.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::dxgk::*;

/// DISPATCH-safe paging-buffer tracer. `DxgkDdiBuildPagingBuffer` can run at
/// DISPATCH_LEVEL, where `diag::record`'s `RtlWriteRegistryValue` is illegal
/// (PASSIVE-only). Instead we bump these lock-free atomics, which ntoseye reads
/// by symbol to see the op flow without an IRQL violation. Stage 2b.
pub static PAGING_LAST_OP: AtomicU32 = AtomicU32::new(0xFFFF_FFFF);
pub static PAGING_CALL_COUNT: AtomicU32 = AtomicU32::new(0);

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
    //
    // IRQL: this DDI can run at DISPATCH_LEVEL — record via DISPATCH-safe atomics,
    // NOT diag::record (RtlWriteRegistryValue is PASSIVE-only). Stage 2b will read
    // the op here (ntoseye breakpoint) to learn which op carries the VidMm segment
    // offset, then issue resource_map_blob(resource_id, offset).
    let args = unsafe { &*build_paging_buffer };
    PAGING_LAST_OP.store(args.Operation as u32, Ordering::Relaxed);
    PAGING_CALL_COUNT.fetch_add(1, Ordering::Relaxed);
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
