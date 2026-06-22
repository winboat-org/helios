//! `DxgkDdiBuildPagingBuffer` and the GpuMmu root-page-table DDIs.
//!
//! Helios declares a **decorative** GpuMmu (WDDM_FAKE_VIDMM_RESEARCH.md §A3.7):
//! the host GPU owns the real MMU and venus addresses resources by opaque id, so
//! the guest page-table *content* is never read by any hardware. What VidMm still
//! requires is that every page-table DDI exist, succeed, and return values
//! consistent with the declared `ddi::gpummu` geometry. So:
//!
//!   - `BuildPagingBuffer` is a **null engine**: it consumes every paging
//!     operation (incl. the GpuMmu `UPDATE_PAGE_TABLE` / `MAP_MMU` ops that arrive
//!     because `PAGE_TABLE_UPDATE_MODE = GPU_PHYSICAL` routes PTE writes through
//!     here) and returns success **without writing DMA / advancing `pDmaBuffer`**.
//!     The accompanying `SubmitCommand` retires the fence, so VidMm believes the
//!     page table was programmed; nothing ever CPU- or GPU-touches the PTE bytes.
//!   - `GetRootPageTableSize` returns a byte size consistent with the declared
//!     PTE size, so VidMm carves a correctly-sized root page table.
//!   - `SetRootPageTable` records-and-ignores (the root address is decorative).
//!
//! IRQL: `BuildPagingBuffer` and `SetRootPageTable` can run at DISPATCH_LEVEL,
//! where `diag::record` (`RtlWriteRegistryValue`, PASSIVE-only) is illegal. We
//! trace through the DISPATCH-safe atomics below; ntoseye reads them by symbol.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::dxgk::*;

/// DISPATCH-safe paging tracers (ntoseye reads these by symbol — no IRQL
/// violation, unlike the `diag::record` ring).
pub static PAGING_LAST_OP: AtomicU32 = AtomicU32::new(0xFFFF_FFFF);
pub static PAGING_CALL_COUNT: AtomicU32 = AtomicU32::new(0);
/// Bitmask of every `DXGK_BUILDPAGINGBUFFER_OPERATION` value seen (bit = 1<<op).
/// Lets the bring-up session see *which* paging ops VidMm drives once GpuMmu is
/// declared (e.g. UPDATE_PAGE_TABLE=11 → bit 11) without flooding the ring.
pub static PAGING_OP_SEEN_MASK: AtomicU32 = AtomicU32::new(0);
/// `DxgkDdiSetRootPageTable` call count + the last context/NumEntries seen.
pub static SET_ROOT_PT_COUNT: AtomicU32 = AtomicU32::new(0);
pub static SET_ROOT_PT_LAST: AtomicU64 = AtomicU64::new(0);
/// `DxgkDdiGetRootPageTableSize` call count + last (NumberOfPte<<32 | bytes).
pub static GET_ROOT_PT_SIZE_COUNT: AtomicU32 = AtomicU32::new(0);
pub static GET_ROOT_PT_SIZE_LAST: AtomicU64 = AtomicU64::new(0);

/// Mirror the DISPATCH-safe GpuMmu page-table tracers into the PASSIVE diag ring.
/// Call ONLY from a PASSIVE DDI (e.g. `DxgkDdiDestroyDevice`) — `diag::record`
/// is `RtlWriteRegistryValue` (PASSIVE-only). This lets the registry ring (read
/// over SSH, no ntoseye) show how far into the GpuMmu page-table setup VidMm got
/// before a post-CreateContext Code-43 teardown:
///   0x0F01_MMMM = PAGING_OP_SEEN_MASK (bit11=UPDATE_PAGE_TABLE, bit5=MAP_APERTURE…)
///   0x0F02_NNNN = BuildPagingBuffer call count
///   0x0F03_NNNN = SetRootPageTable call count
///   0x0F04_NNNN = GetRootPageTableSize call count
///   0x0F05_OOOO = last paging Operation
pub fn diag_dump_gpummu_atomics() {
    let mask = PAGING_OP_SEEN_MASK.load(Ordering::Relaxed) & 0xFFFF;
    crate::diag::record(0x0F01_0000 | mask);
    crate::diag::record(0x0F02_0000 | (PAGING_CALL_COUNT.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x0F03_0000 | (SET_ROOT_PT_COUNT.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x0F04_0000 | (GET_ROOT_PT_SIZE_COUNT.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x0F05_0000 | (PAGING_LAST_OP.load(Ordering::Relaxed) & 0xFFFF));
}

/// `DxgkDdiBuildPagingBuffer` — translate a memory-management operation into GPU
/// DMA. Null engine: every operation is consumed and acknowledged with success,
/// emitting no DMA (the decorative GpuMmu has no hardware page table to program,
/// and venus needs no aperture/transfer copies). See the module doc.
pub unsafe extern "C" fn dxgkddi_build_paging_buffer(
    h_adapter: *mut c_void,
    build_paging_buffer: *mut DXGKARG_BUILDPAGINGBUFFER,
) -> NTSTATUS {
    if h_adapter.is_null() || build_paging_buffer.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    // SAFETY: valid per the DDI contract; we only read the discriminant. We do
    // NOT advance pDmaBuffer — this null engine writes no hardware commands.
    let args = unsafe { &*build_paging_buffer };
    let op = args.Operation as u32;
    PAGING_LAST_OP.store(op, Ordering::Relaxed);
    PAGING_CALL_COUNT.fetch_add(1, Ordering::Relaxed);
    if op < 32 {
        PAGING_OP_SEEN_MASK.fetch_or(1u32 << op, Ordering::Relaxed);
    }
    STATUS_SUCCESS
}

// ── GpuMmu root page-table DDIs. ─────────────────────────────────────────────

/// `DxgkDdiSetRootPageTable` — bind a context's root page table. Decorative:
/// the root address names a (never-walked) guest page table, so we record the
/// call for the debugger and ignore it.
pub unsafe extern "C" fn dxgkddi_set_root_page_table(
    _h_adapter: IN_CONST_HANDLE,
    set_root_page_table: IN_CONST_PDXGKARG_SETROOTPAGETABLE,
) {
    SET_ROOT_PT_COUNT.fetch_add(1, Ordering::Relaxed);
    if !set_root_page_table.is_null() {
        // SAFETY: non-null checked; read-only access to the args struct.
        let args = unsafe { &*set_root_page_table };
        // Pack NumEntries (low 32) so the debugger can confirm VidMm drives the
        // declared geometry. (Address is decorative; the host never walks it.)
        SET_ROOT_PT_LAST.store(args.NumEntries as u64, Ordering::Relaxed);
    }
}

/// `DxgkDdiGetRootPageTableSize` — report the byte size of a root page table that
/// must address `NumberOfPte` entries. Must be consistent with the declared PTE
/// size in `ddi::gpummu`, or VidMm carves a mis-sized root page table.
pub unsafe extern "C" fn dxgkddi_get_root_page_table_size(
    _h_adapter: IN_CONST_HANDLE,
    get_root_page_table_size: INOUT_PDXGKARG_GETROOTPAGETABLESIZE,
) -> SIZE_T {
    GET_ROOT_PT_SIZE_COUNT.fetch_add(1, Ordering::Relaxed);
    if get_root_page_table_size.is_null() {
        return 0;
    }
    // SAFETY: non-null checked; NumberOfPte is the input count.
    let args = unsafe { &*get_root_page_table_size };
    let num_pte = args.NumberOfPte;
    let bytes = super::gpummu::root_page_table_size_bytes(num_pte);
    GET_ROOT_PT_SIZE_LAST.store(
        ((num_pte as u64) << 32) | (bytes as u64 & 0xFFFF_FFFF),
        Ordering::Relaxed,
    );
    bytes
}
