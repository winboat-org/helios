//! `DxgkDdiBuildPagingBuffer` and the GpuMmu root-page-table DDIs.
//!
//! Helios declares a **decorative** GpuMmu (WDDM_FAKE_VIDMM_RESEARCH.md §A3.7):
//! the host GPU owns the real MMU and venus addresses resources by opaque id, so
//! the guest page-table *content* is never read by any hardware. What VidMm still
//! requires is that every page-table DDI exist, succeed, and return values
//! consistent with the declared `ddi::gpummu` geometry. So:
//!
//!   - For the aperture / page-table segments `BuildPagingBuffer` stays a
//!     **null engine**: it consumes the operation and returns success **without
//!     writing DMA / advancing `pDmaBuffer`**. The accompanying `SubmitCommand`
//!     retires the fence, so VidMm believes the operation ran.
//!   - `GetRootPageTableSize` returns a byte size consistent with the declared
//!     PTE size, so VidMm carves a correctly-sized root page table.
//!   - `SetRootPageTable` records-and-ignores (the root address is decorative).
//!
//! **BAR SEGMENT (id 3) CONTENT OPS ARE REAL** (two-memory-split fix,
//! HANDOFF_GDI_EXECUTOR_2026_07_05.md ★FINAL). A segment-3 allocation's
//! content IS its venus blob (the CPU host aperture exposes the blob bytes —
//! `cpu_host_aperture.rs`), so:
//!
//!   - Content TRANSFERs (system MDL ↔ segment) execute synchronously here as
//!     CPU copies between the MDL and a transient kernel map of the blob,
//!     BEFORE the paging fence retires — VidMm's content model stays truthful
//!     across eviction/re-commit. FILL / VIRTUAL_FILL pattern-fill the blob.
//!   - A segment→segment move needs NO copy (content is intrinsic to the host
//!     memory object; the decorative SegmentAddress values are ignored).
//!   - A leaf UPDATE_PAGE_TABLE naming segment 3 is harvested (atomic store
//!     only) as a placement diagnostic.
//!
//! IRQL: the docs state `DxgkDdiBuildPagingBuffer` runs at PASSIVE_LEVEL; the
//! BAR content work (host round-trips, Mm mapping, registry counters) is
//! additionally gated on a runtime `KeGetCurrentIrql() == PASSIVE_LEVEL` check
//! and counted loudly (`PgEi`) if that ever fails — never a silent skip, never
//! a DISPATCH-illegal call. `SetRootPageTable` can run at DISPATCH_LEVEL and
//! keeps its atomics-only tracing.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use wdk_sys::ntddk::{
    KeGetCurrentIrql, MmMapIoSpace, MmMapLockedPagesSpecifyCache, MmUnmapIoSpace,
};
use wdk_sys::{_MEMORY_CACHING_TYPE, PHYSICAL_ADDRESS, PMDL};

use crate::adapter::AdapterContext;
use crate::ddi::create_allocation::{paging_alloc_info, set_bar_placement};
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

// ── BAR-segment (id 3) paging engine ─────────────────────────────────────────
//
// Content ops for segment-3 allocations are PLACEMENT-INDEPENDENT: an
// allocation's content is its venus BLOB (the CPU host aperture exposes blob
// bytes wherever dxgkrnl asked them mapped — `cpu_host_aperture.rs`), so a
// paging TRANSFER/FILL reads/writes the blob through a transient kernel map of
// its CURRENT window mapping, and a segment→segment "move" needs no copy at
// all. The decorative SegmentAddress values are ignored.

/// `PASSIVE_LEVEL` (KIRQL 0) — the only IRQL at which the BAR content ops
/// (host round-trips, Mm mapping calls) may run.
const PASSIVE_LEVEL_IRQL: u8 = 0;

// Counters (registry-visible after any BAR-segment op; atomics are the source
// of truth and stay readable by symbol even if the registry write is skipped).
static BAR_XFER_IN: AtomicU32 = AtomicU32::new(0); // system MDL → blob copies
static BAR_XFER_OUT: AtomicU32 = AtomicU32::new(0); // blob → system MDL copies
static BAR_XFER_MOVE: AtomicU32 = AtomicU32::new(0); // segment→segment (no-op)
static BAR_FILLS: AtomicU32 = AtomicU32::new(0);
static BAR_DISCARDS: AtomicU32 = AtomicU32::new(0);
static BAR_PT_HARVESTS: AtomicU32 = AtomicU32::new(0); // placements seen in leaf PTEs
static BAR_LAST_RESID: AtomicU32 = AtomicU32::new(0);
static BAR_LAST_XFER_FLAGS: AtomicU32 = AtomicU32::new(0);
static BAR_LAST_XFER_OFF: AtomicU32 = AtomicU32::new(0);
static BAR_LAST_MDL_OFF: AtomicU32 = AtomicU32::new(0);
// Loud failure counters — any nonzero value after boot is a design gap to chase.
static BAR_ERR_IRQL: AtomicU32 = AtomicU32::new(0); // content op arrived > PASSIVE
static BAR_ERR_MAP: AtomicU32 = AtomicU32::new(0); // blob map / kernel map failed
static BAR_ERR_BOUNDS: AtomicU32 = AtomicU32::new(0); // op range outside the blob
static BAR_ERR_DISCONTIG: AtomicU32 = AtomicU32::new(0); // leaf PTEs not contiguous
static BAR_ERR_VIRTUAL: AtomicU32 = AtomicU32::new(0); // unresolvable VIRTUAL_TRANSFER
static BAR_ERR_MDL: AtomicU32 = AtomicU32::new(0); // system-MDL kernel map failed

/// Mirror the BAR paging counters into the registry (PASSIVE only — callers
/// gate on IRQL). Named-value writes, same style as the GDI executor counters.
fn dump_bar_counters() {
    crate::diag::record_named_bytes(b"PgTi", BAR_XFER_IN.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PgTo", BAR_XFER_OUT.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PgTm", BAR_XFER_MOVE.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PgFn", BAR_FILLS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PgDn", BAR_DISCARDS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PgUn", BAR_PT_HARVESTS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PgMr", BAR_LAST_RESID.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PgSf", BAR_LAST_XFER_FLAGS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PgTs", BAR_LAST_XFER_OFF.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PgTd", BAR_LAST_MDL_OFF.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PgEi", BAR_ERR_IRQL.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PgEm", BAR_ERR_MAP.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PgEb", BAR_ERR_BOUNDS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PgEc", BAR_ERR_DISCONTIG.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PgEv", BAR_ERR_VIRTUAL.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PgEx", BAR_ERR_MDL.load(Ordering::Relaxed));
}

/// `NormalPagePriority | MdlMappingNoExecute` for the system-MDL kernel map.
const MDL_MAP_PRIORITY: u32 = 16 | 0x4000_0000;
/// `MDL_MAPPED_TO_SYSTEM_VA | MDL_SOURCE_IS_NONPAGED_POOL` — MappedSystemVa valid.
const MDL_HAS_SYSTEM_VA: i16 = 0x0001 | 0x0004;
/// `KernelMode` (`KPROCESSOR_MODE`).
const KERNEL_MODE: i8 = 0;

/// Kernel VA of a paging op's system-memory MDL (VidMm passes locked MDLs).
/// Reuses an existing system mapping if the MDL has one; otherwise maps
/// KernelMode/cached (released when VidMm frees the MDL, the
/// `MmGetSystemAddressForMdlSafe` pattern). Returns `None` (counted) on failure.
///
/// # Safety
/// `mdl` must be a valid, locked MDL for the duration of the paging op.
unsafe fn mdl_system_va(mdl: PMDL) -> Option<*mut u8> {
    if mdl.is_null() {
        return None;
    }
    // SAFETY: valid MDL per the fn contract.
    unsafe {
        if (*mdl).MdlFlags & MDL_HAS_SYSTEM_VA != 0 {
            return Some((*mdl).MappedSystemVa as *mut u8);
        }
        let va = MmMapLockedPagesSpecifyCache(
            mdl,
            KERNEL_MODE,
            _MEMORY_CACHING_TYPE::MmCached,
            core::ptr::null_mut(),
            0, // BugCheckOnFailure = FALSE → NULL return for KernelMode failure
            MDL_MAP_PRIORITY,
        );
        if va.is_null() {
            BAR_ERR_MDL.fetch_add(1, Ordering::Relaxed);
            None
        } else {
            Some(va as *mut u8)
        }
    }
}

/// Run `f` over a transient kernel mapping of the allocation's blob bytes
/// (mapped into the window first if it is not currently mapped — content is
/// identical at any window offset). PASSIVE_LEVEL. Returns `false` (counted)
/// if the blob could not be resolved/mapped.
unsafe fn with_blob_bytes(
    adapter: &AdapterContext,
    resource_id: u32,
    f: impl FnOnce(*mut u8, u64),
) -> bool {
    BAR_LAST_RESID.store(resource_id, Ordering::Relaxed);
    let prep = match crate::virtio::ctrl::map_blob_prepare(adapter, None, resource_id) {
        Ok(p) => p,
        Err(_) => {
            BAR_ERR_MAP.fetch_add(1, Ordering::Relaxed);
            return false;
        }
    };
    let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
    pa.QuadPart = prep.gpa as i64;
    // SAFETY: PASSIVE_LEVEL; the range was RESOURCE_MAP_BLOB'd into the
    // host-visible window, so the pages are backed. Unmapped below.
    let va = unsafe { MmMapIoSpace(pa, prep.size, _MEMORY_CACHING_TYPE::MmCached) } as *mut u8;
    if va.is_null() {
        BAR_ERR_MAP.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    f(va, prep.size);
    // SAFETY: `va` maps `prep.size` bytes, mapped just above.
    unsafe { MmUnmapIoSpace(va as *mut c_void, prep.size) };
    true
}

/// Classic TRANSFER touching the BAR segment: content copy between the
/// allocation's blob and its system-memory backing (synchronous — done before
/// the paging fence retires, so VidMm's content model stays truthful across
/// eviction/re-commit). PASSIVE_LEVEL.
unsafe fn bar_transfer(
    adapter: &AdapterContext,
    bar_id: u32,
    t: &_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1,
) {
    let src_seg = t.Source.SegmentId;
    let dst_seg = t.Destination.SegmentId;
    if src_seg != bar_id && dst_seg != bar_id {
        return; // aperture/system transfer — null engine, as before
    }
    let Some(alloc) = (unsafe { paging_alloc_info(t.hAllocation) }) else {
        return;
    };
    let flags = unsafe { t.Flags.__bindgen_anon_1.Value };
    BAR_LAST_XFER_FLAGS.store(flags, Ordering::Relaxed);
    BAR_LAST_XFER_OFF.store(t.TransferOffset, Ordering::Relaxed);
    BAR_LAST_MDL_OFF.store(t.MdlOffset, Ordering::Relaxed);
    let bytes = t.TransferSize as u64;
    // MdlOffset is in pages from the MDL start; sub-page advance (if any)
    // rides TransferOffset's low bits. Validated post-boot via PgTs/PgTd —
    // transfers are whole-allocation in the common case.
    let mdl_off = ((t.MdlOffset as u64) << 12) + (t.TransferOffset as u64 & 0xFFF);
    let blob_off = t.TransferOffset as u64;

    match (src_seg, dst_seg) {
        // Page-in: system backing → blob (evicted or initial content).
        (0, s) if s == bar_id => {
            // SAFETY: Source.pMdl arm is valid for SegmentId 0. The cast maps
            // the dxgk-bindings MDL to the layout-identical wdk_sys MDL.
            let mdl = unsafe { *t.Source.__bindgen_anon_1.pMdl.as_ref() };
            let Some(src) = (unsafe { mdl_system_va(mdl.cast()) }) else {
                return;
            };
            let ok = unsafe {
                with_blob_bytes(adapter, alloc.resource_id, |dst, len| {
                    if blob_off.saturating_add(bytes) > len {
                        BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    // SAFETY: dst covers `len` blob bytes (bounds-checked);
                    // src is the MDL's kernel mapping, valid for the op.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            src.add(mdl_off as usize),
                            dst.add(blob_off as usize),
                            bytes as usize,
                        );
                    }
                })
            };
            if ok {
                BAR_XFER_IN.fetch_add(1, Ordering::Relaxed);
            }
        }
        // Eviction: blob → system backing.
        (s, 0) if s == bar_id => {
            // SAFETY: Destination.pMdl arm is valid for SegmentId 0. Cast as above.
            let mdl = unsafe { *t.Destination.__bindgen_anon_1.pMdl.as_ref() };
            let Some(dst) = (unsafe { mdl_system_va(mdl.cast()) }) else {
                return;
            };
            let ok = unsafe {
                with_blob_bytes(adapter, alloc.resource_id, |src, len| {
                    if blob_off.saturating_add(bytes) > len {
                        BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    // SAFETY: src covers `len` blob bytes (bounds-checked);
                    // dst is the MDL's kernel mapping, valid for the op.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            src.add(blob_off as usize),
                            dst.add(mdl_off as usize),
                            bytes as usize,
                        );
                    }
                })
            };
            if ok {
                BAR_XFER_OUT.fetch_add(1, Ordering::Relaxed);
            }
        }
        // Move within the segment: content is intrinsic to the blob; the CPU
        // view follows the aperture maps. Nothing to copy.
        (s, d) if s == bar_id && d == bar_id => {
            BAR_XFER_MOVE.fetch_add(1, Ordering::Relaxed);
        }
        // BAR ↔ aperture/paging-RAM combinations are not part of the declared
        // allocation segment sets; loud counter, no silent data motion.
        _ => {
            BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Classic FILL of a BAR-segment allocation: CPU-fill the blob. PASSIVE_LEVEL.
unsafe fn bar_fill(
    adapter: &AdapterContext,
    bar_id: u32,
    f: &_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_2,
) {
    if f.Destination.SegmentId != bar_id {
        return;
    }
    let Some(alloc) = (unsafe { paging_alloc_info(f.hAllocation) }) else {
        return;
    };
    let fill_len = f.FillSize as u64;
    let pattern = f.FillPattern;
    let ok = unsafe {
        with_blob_bytes(adapter, alloc.resource_id, |dst, len| {
            let n = fill_len.min(len);
            fill_pattern(dst, n as usize, pattern);
        })
    };
    if ok {
        BAR_FILLS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Write `pattern` (u32, repeated) over `len` bytes at `dst`.
fn fill_pattern(dst: *mut u8, len: usize, pattern: u32) {
    let words = len / 4;
    for i in 0..words {
        // SAFETY: caller bounds-checked `len` bytes at `dst`.
        unsafe { core::ptr::write_unaligned((dst as *mut u32).add(i), pattern) };
    }
    let bytes = pattern.to_le_bytes();
    for i in (words * 4)..len {
        // SAFETY: as above; tail bytes of a non-multiple-of-4 fill.
        unsafe { *dst.add(i) = bytes[i % 4] };
    }
}

/// Diagnostic harvest of a LEAF `UPDATE_PAGE_TABLE`: record the segment-3
/// physical placement VidMm assigned (pure atomic store — DISPATCH-safe, no
/// side effects; content ops do not depend on it in the aperture model).
unsafe fn bar_harvest_page_table(
    bar_id: u32,
    bar_size: u64,
    u: &DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE,
) {
    if u.PageTableLevel != 0 || u.pPageTableEntries.is_null() || u.NumPageTableEntries == 0 {
        return;
    }
    let Some(alloc) = (unsafe { paging_alloc_info(u.hAllocation) }) else {
        return;
    };
    if !alloc.bar_eligible {
        return;
    }
    // SAFETY: pPageTableEntries holds NumPageTableEntries DXGK_PTEs for the call.
    let pte0 = unsafe { core::ptr::read_unaligned(u.pPageTableEntries) };
    let valid = unsafe { pte0.__bindgen_anon_1.__bindgen_anon_1 }.Valid() != 0;
    let seg = unsafe { pte0.__bindgen_anon_1.__bindgen_anon_1 }.Segment() as u32;
    if !valid || seg != bar_id {
        return;
    }
    let page0 = unsafe { pte0.__bindgen_anon_2.PageAddress };
    // Contiguity check: a memory-segment allocation should be one contiguous
    // range; discontiguous PTEs are counted (they would matter if partial
    // aperture maps ever need placement-relative offsets).
    let n = u.NumPageTableEntries as u64;
    if n >= 2 {
        let last = unsafe { core::ptr::read_unaligned(u.pPageTableEntries.add((n - 1) as usize)) };
        let last_valid = unsafe { last.__bindgen_anon_1.__bindgen_anon_1 }.Valid() != 0;
        if last_valid && unsafe { last.__bindgen_anon_2.PageAddress } != page0 + (n - 1) {
            BAR_ERR_DISCONTIG.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    if let Some(base) = (page0 << 12)
        .checked_sub(u.AllocationOffsetInBytes)
        .filter(|b| *b < bar_size)
    {
        if alloc.bar_placed != base {
            // SAFETY: h is the live paging-op allocation handle.
            unsafe { set_bar_placement(u.hAllocation, base) };
            BAR_PT_HARVESTS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// `DxgkDdiBuildPagingBuffer` — translate a memory-management operation into GPU
/// DMA. Null engine for the aperture / page-table segments; REAL content engine
/// for BAR-segment (id 3) allocations. See the module doc.
pub unsafe extern "C" fn dxgkddi_build_paging_buffer(
    h_adapter: *mut c_void,
    build_paging_buffer: *mut DXGKARG_BUILDPAGINGBUFFER,
) -> NTSTATUS {
    if h_adapter.is_null() || build_paging_buffer.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    // SAFETY: valid per the DDI contract. We do NOT advance pDmaBuffer — no
    // hardware command is ever emitted (BAR-segment work runs synchronously
    // on the CPU right here, before the paging fence retires).
    let args = unsafe { &*build_paging_buffer };
    let op = args.Operation as u32;
    PAGING_LAST_OP.store(op, Ordering::Relaxed);
    PAGING_CALL_COUNT.fetch_add(1, Ordering::Relaxed);
    if op < 32 {
        PAGING_OP_SEEN_MASK.fetch_or(1u32 << op, Ordering::Relaxed);
    }

    // SAFETY: dxgkrnl hands back our AdapterContext as the miniport context.
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let Some(bar) = adapter.bar_segment.as_ref() else {
        return STATUS_SUCCESS; // BAR segment inactive → pure null engine
    };

    use crate::dxgk::_DXGK_BUILDPAGINGBUFFER_OPERATION as PagingOp;

    // Placement harvest is DISPATCH-safe (atomic store only) — no IRQL gate.
    if args.Operation == PagingOp::DXGK_OPERATION_UPDATE_PAGE_TABLE {
        // SAFETY: union arm selected by Operation.
        unsafe {
            bar_harvest_page_table(
                bar.seg_id,
                bar.size,
                args.__bindgen_anon_1.UpdatePageTable.as_ref(),
            )
        };
        return STATUS_SUCCESS;
    }

    let is_content_op = matches!(
        args.Operation,
        PagingOp::DXGK_OPERATION_TRANSFER
            | PagingOp::DXGK_OPERATION_FILL
            | PagingOp::DXGK_OPERATION_DISCARD_CONTENT
            | PagingOp::DXGK_OPERATION_VIRTUAL_FILL
            | PagingOp::DXGK_OPERATION_VIRTUAL_TRANSFER
    );
    if !is_content_op {
        return STATUS_SUCCESS;
    }
    // Content ops need PASSIVE (host round-trips, Mm mapping calls). The DDI
    // is documented PASSIVE; if that ever fails in practice this counter
    // fires and the op degrades to the old null engine — loud, not silent.
    // SAFETY: KeGetCurrentIrql is callable at any IRQL.
    if unsafe { KeGetCurrentIrql() } != PASSIVE_LEVEL_IRQL {
        BAR_ERR_IRQL.fetch_add(1, Ordering::Relaxed);
        return STATUS_SUCCESS;
    }

    match args.Operation {
        PagingOp::DXGK_OPERATION_TRANSFER => {
            // SAFETY: union arm selected by Operation.
            unsafe { bar_transfer(adapter, bar.seg_id, args.__bindgen_anon_1.Transfer.as_ref()) };
        }
        PagingOp::DXGK_OPERATION_FILL => {
            // SAFETY: union arm selected by Operation.
            unsafe { bar_fill(adapter, bar.seg_id, args.__bindgen_anon_1.Fill.as_ref()) };
        }
        PagingOp::DXGK_OPERATION_DISCARD_CONTENT => {
            // SAFETY: union arm selected by Operation.
            let d = unsafe { args.__bindgen_anon_1.DiscardContent.as_ref() };
            if d.SegmentId == bar.seg_id && unsafe { paging_alloc_info(d.hAllocation) }.is_some() {
                // Content lives in the blob; nothing to release here (aperture
                // unmaps handle CPU visibility). Counted for the op census.
                BAR_DISCARDS.fetch_add(1, Ordering::Relaxed);
            }
        }
        PagingOp::DXGK_OPERATION_VIRTUAL_FILL => {
            // SAFETY: union arm selected by Operation.
            let fv = unsafe { args.__bindgen_anon_1.FillVirtual.as_ref() };
            if let Some(alloc) = unsafe { paging_alloc_info(fv.hAllocation) } {
                if alloc.bar_eligible {
                    let off = fv.AllocationOffsetInBytes;
                    let fill_len = fv.FillSizeInBytes;
                    let pattern = fv.FillPattern;
                    let ok = unsafe {
                        with_blob_bytes(adapter, alloc.resource_id, |dst, len| {
                            if off.saturating_add(fill_len) > len {
                                BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                                return;
                            }
                            // SAFETY: bounds-checked against the blob mapping.
                            fill_pattern(
                                unsafe { dst.add(off as usize) },
                                fill_len as usize,
                                pattern,
                            );
                        })
                    };
                    if ok {
                        BAR_FILLS.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        PagingOp::DXGK_OPERATION_VIRTUAL_TRANSFER => {
            // SAFETY: union arm selected by Operation.
            let tv = unsafe { args.__bindgen_anon_1.TransferVirtual.as_ref() };
            if let Some(alloc) = unsafe { paging_alloc_info(tv.hAllocation) } {
                if alloc.bar_eligible {
                    // The system side is only expressed as paging-process GPU
                    // VAs — unresolvable without a full PTE shadow. Loud.
                    BAR_ERR_VIRTUAL.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        _ => {}
    }
    dump_bar_counters();
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
