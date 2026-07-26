//! `DxgkDdiBuildPagingBuffer` and the GpuMmu root-page-table DDIs.
//!
//! Helios declares a **decorative** GpuMmu (WDDM_FAKE_VIDMM_RESEARCH.md §A3.7):
//! the host GPU owns the real MMU and venus addresses resources by opaque id, so
//! the guest page-table *content* is never read by hardware. What VidMm still
//! requires is that every page-table DDI exist, succeed, and return values
//! consistent with the declared `ddi::gpummu` geometry. One part of that content
//! is nevertheless authoritative to the CPU paging executor: leaf PTEs for the
//! paging-process virtual addresses used by `VIRTUAL_TRANSFER`. So:
//!
//!   - For the aperture / page-table segments `BuildPagingBuffer` stays a
//!     **null engine**: it consumes the operation and returns success **without
//!     writing DMA / advancing `pDmaBuffer`**. The accompanying `SubmitCommand`
//!     retires the fence, so VidMm believes the operation ran.
//!   - Leaf `UPDATE_PAGE_TABLE` calls retain the exact system-memory
//!     `FirstPteVirtualAddress` → `DXGK_PTE::PageAddress` mappings supplied by
//!     VidMm for Helios blob allocations. `VIRTUAL_TRANSFER` uses those mappings
//!     to copy allocation content between the blob and the locked system pages.
//!     This is the software implementation of the paging GPU's VA walk; it does
//!     not classify resources or infer an identity.
//!   - `GetRootPageTableSize` returns a byte size consistent with the declared
//!     PTE size, so VidMm carves a correctly-sized root page table.
//!   - `SetRootPageTable` records-and-ignores (the root address is decorative).
//!
//! **BAR SEGMENT (id 3) CONTENT OPS ARE REAL** (two-memory-split fix,
//! HANDOFF_GDI_EXECUTOR_2026_07_05.md ★FINAL). A segment-3 allocation's
//! content IS its venus blob (the CPU host aperture exposes the blob bytes —
//! `cpu_host_aperture.rs`), so:
//!
//!   - Content TRANSFERs (system MDL ↔ segment) and VIRTUAL_TRANSFERs
//!     (paging-process GPU VA ↔ segment) execute synchronously here as
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

use alloc::sync::Arc;
use alloc::vec::Vec;

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use wdk_sys::ntddk::{
    KeAcquireSpinLockRaiseToDpc, KeGetCurrentIrql, KeReleaseSpinLock, MmGetPhysicalAddress,
    MmMapIoSpace, MmMapLockedPagesSpecifyCache, MmUnmapIoSpace,
};
use wdk_sys::{_MEMORY_CACHING_TYPE, KSPIN_LOCK, PHYSICAL_ADDRESS, PMDL};

use crate::adapter::{AdapterContext, SystemBackingSnapshot};
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

/// What one content-op executor did, as a value the dispatch must consume.
///
/// The executors used to return `()`: every failure inside them — an
/// unresolvable handle, an MDL map failure, a blob map failure, an out-of-blob
/// range — was discarded and `DxgkDdiBuildPagingBuffer` answered
/// STATUS_SUCCESS. VidMm then retired the paging fence believing the content
/// had moved, so a page-in left stale bytes in the BAR blob and an eviction
/// lost the only copy of the allocation. Making the contribution part of the
/// return type means a new executor cannot silently skip it.
enum PagingOpOutcome {
    /// The content operation ran to completion.
    Executed,
    /// The operation does not belong to this driver's content engine (another
    /// segment, or a device-local allocation whose bytes are host-owned).
    /// Reported as success, exactly as before — nothing was supposed to happen.
    NotOurs,
    /// The operation was ours and did not happen. Must reach the DDI status.
    Failed(NTSTATUS),
}

/// The single failure status this DDI returns, for every arm.
///
/// STATUS_INSUFFICIENT_RESOURCES is the value the shadow-full arm of this same
/// function already returns, and it is what two sibling DDIs
/// (`create_allocation.rs`, `cpu_host_aperture.rs`) were changed to when
/// STATUS_UNSUCCESSFUL was proven out of contract — dxgkrnl logged it as
/// "Driver returned an invalid NTSTATUS" 197x with adapter resets. Routing every
/// arm through one function keeps the legal-return set a one-line audit.
const fn paging_failure() -> NTSTATUS {
    STATUS_INSUFFICIENT_RESOURCES
}

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
static BAR_ERR_VIRTUAL: AtomicU32 = AtomicU32::new(0); // unresolved paging-process VA
static BAR_ERR_MDL: AtomicU32 = AtomicU32::new(0); // system-MDL kernel map failed
static BAR_ERR_SHADOW_FULL: AtomicU32 = AtomicU32::new(0); // PTE shadow capacity exhausted
/// A classic TRANSFER (`PgEh`) / FILL (`PgFh`) named an `hAllocation` that does
/// not resolve to a live Helios allocation. Both were bare `return`s: the op did
/// not run, nothing was counted, and the DDI still answered STATUS_SUCCESS, so
/// VidMm retired the paging fence believing content had moved.
static BAR_ERR_XFER_HANDLE: AtomicU32 = AtomicU32::new(0);
static BAR_ERR_FILL_HANDLE: AtomicU32 = AtomicU32::new(0);
/// `VIRTUAL_FILL`s (`PgFv`) that arrived while the allocation was system-
/// resident — evidence only, no behaviour change.
///
/// The VIRTUAL_FILL arm fills the blob at `AllocationOffsetInBytes` and never
/// resolves `DestinationVirtualAddress` through the PTE shadow, the way
/// `bar_virtual_transfer` does for the same class of address. While the
/// allocation is paged out to system memory, the bytes VidMm means are the
/// system pages, not the blob. Whether that is reachable at all is an open
/// question this counter answers before anything is built for it: a nonzero
/// value is the trigger for a VA-resolving implementation (k-paging-14).
static BAR_VIRTUAL_FILL_SYSTEM: AtomicU32 = AtomicU32::new(0);
static BAR_VIRTUAL_PTES: AtomicU32 = AtomicU32::new(0); // system PTEs retained
static BAR_LAST_VIRTUAL_SRC: AtomicU64 = AtomicU64::new(0);
static BAR_LAST_VIRTUAL_DST: AtomicU64 = AtomicU64::new(0);
/// Paging content op named a device-local/opaque allocation. Such resources
/// have no CPU byte mapping; attempting RESOURCE_MAP_BLOB is a contract error.
static BAR_DEVICE_OP_SKIPS: AtomicU32 = AtomicU32::new(0);
static BAR_SYSTEM_BACKING_CAPTURES: AtomicU32 = AtomicU32::new(0);
static BAR_SYSTEM_BACKING_MIRRORS: AtomicU32 = AtomicU32::new(0);
static BAR_SYSTEM_BACKING_ERRORS: AtomicU32 = AtomicU32::new(0);

/// The BAR paging counter block, mirrored into the registry through the shared
/// throttled emitter (R317). Named values and encodings are unchanged; only the
/// cadence is — this ran at the tail of EVERY content op, i.e. 26 synchronous
/// registry writes per paging operation, per allocation, under eviction
/// pressure. Failure counters still surface on the op that produced them, via
/// `CounterBlock`'s flush-on-failure-change rule.
static PAGING_FLUSH_TICKS: AtomicU32 = AtomicU32::new(0);
static PAGING_FLUSH_FAILURES: AtomicU32 = AtomicU32::new(0);

static PAGING_COUNTERS: crate::diag::CounterBlock = crate::diag::CounterBlock {
    entries: &[
        e(b"PgTi", &BAR_XFER_IN),
        e(b"PgTo", &BAR_XFER_OUT),
        e(b"PgTm", &BAR_XFER_MOVE),
        e(b"PgFn", &BAR_FILLS),
        e(b"PgDn", &BAR_DISCARDS),
        e(b"PgUn", &BAR_PT_HARVESTS),
        e(b"PgMr", &BAR_LAST_RESID),
        e(b"PgSf", &BAR_LAST_XFER_FLAGS),
        e(b"PgTs", &BAR_LAST_XFER_OFF),
        e(b"PgTd", &BAR_LAST_MDL_OFF),
        f(b"PgEi", &BAR_ERR_IRQL),
        f(b"PgEm", &BAR_ERR_MAP),
        f(b"PgEb", &BAR_ERR_BOUNDS),
        f(b"PgEc", &BAR_ERR_DISCONTIG),
        f(b"PgEv", &BAR_ERR_VIRTUAL),
        f(b"PgEx", &BAR_ERR_MDL),
        f(b"PgEf", &BAR_ERR_SHADOW_FULL),
        e(b"PgVp", &BAR_VIRTUAL_PTES),
        e64(b"PgVs", &BAR_LAST_VIRTUAL_SRC),
        e64(b"PgVd", &BAR_LAST_VIRTUAL_DST),
        e(b"PgDi", &BAR_DEVICE_OP_SKIPS),
        e(b"PgSc", &BAR_SYSTEM_BACKING_CAPTURES),
        e(b"PgSm", &BAR_SYSTEM_BACKING_MIRRORS),
        f(b"PgSe", &BAR_SYSTEM_BACKING_ERRORS),
        f(b"PgEh", &BAR_ERR_XFER_HANDLE),
        f(b"PgFh", &BAR_ERR_FILL_HANDLE),
        e(b"PgFv", &BAR_VIRTUAL_FILL_SYSTEM),
    ],
    ticks: &PAGING_FLUSH_TICKS,
    failures: &PAGING_FLUSH_FAILURES,
    policy: crate::diag::FlushPolicy::EveryNth(64),
};

/// Value entry.
const fn e(name: &'static [u8], value: &'static AtomicU32) -> crate::diag::CounterEntry {
    crate::diag::CounterEntry {
        name,
        value: crate::diag::CounterRef::U32(value),
        failure: false,
    }
}
/// Failure entry — its change forces an immediate flush.
const fn f(name: &'static [u8], value: &'static AtomicU32) -> crate::diag::CounterEntry {
    crate::diag::CounterEntry {
        name,
        value: crate::diag::CounterRef::U32(value),
        failure: true,
    }
}
/// Value entry reported as the low 32 bits of a u64, as before.
const fn e64(name: &'static [u8], value: &'static AtomicU64) -> crate::diag::CounterEntry {
    crate::diag::CounterEntry {
        name,
        value: crate::diag::CounterRef::U64Low(value),
        failure: false,
    }
}

fn dump_bar_counters() {
    PAGING_COUNTERS.flush();
}

// ── Paging-process leaf-PTE shadow ──────────────────────────────────────────

/// Maximum concurrently-live system-memory PTEs retained per adapter.
///
/// VidMm maps a bounded paging-process scratch range around each virtual content
/// operation and unmaps it immediately afterward. 65,536 4-KiB pages covers
/// 256 MiB of simultaneous transfers. Exhaustion is returned to VidMm as
/// `STATUS_INSUFFICIENT_RESOURCES`; it is never silently treated as success.
const MAX_PAGING_SYSTEM_PTES: usize = 65_536;

#[derive(Clone, Copy)]
struct PagingSystemPte {
    /// Paging-process GPU virtual page number.
    gpu_page: u64,
    /// System-memory physical page number from `DXGK_PTE::PageAddress`.
    physical_page: u64,
}

/// Exact system-memory leaf mappings supplied by VidMm in
/// `DXGK_OPERATION_UPDATE_PAGE_TABLE`.
///
/// The table retains no resource classification. `update_leaf` first removes
/// every old entry in the Windows-supplied VA range, then retains an entry only
/// when the update names a live Helios blob allocation and its exact PTE is
/// valid, non-zero, and in segment 0 (system memory).
pub(crate) struct PagingPteShadow {
    lock: UnsafeCell<KSPIN_LOCK>,
    entries: UnsafeCell<Vec<PagingSystemPte>>,
}

// SAFETY: every access to `entries` is serialized by `lock`; entries are POD.
unsafe impl Send for PagingPteShadow {}
unsafe impl Sync for PagingPteShadow {}

impl PagingPteShadow {
    /// Reserve once at adapter construction (PASSIVE_LEVEL); no update allocates
    /// while the spinlock is held.
    pub(crate) fn new() -> Self {
        Self {
            lock: UnsafeCell::new(0),
            entries: UnsafeCell::new(Vec::with_capacity(MAX_PAGING_SYSTEM_PTES)),
        }
    }

    /// Apply one authoritative leaf-page-table update.
    ///
    /// Returns `false` if retaining all supplied system PTEs would exceed the
    /// fixed non-paged table. The caller must fail the paging operation instead
    /// of retiring an incomplete mapping.
    unsafe fn update_leaf(
        &self,
        update: &DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE,
        track_system_pages: bool,
    ) -> bool {
        if update.PageTableLevel != 0
            || update.pPageTableEntries.is_null()
            || update.NumPageTableEntries == 0
        {
            return true;
        }

        let first_page = update.FirstPteVirtualAddress >> 12;
        let page_count = update.NumPageTableEntries as u64;
        let end_page = first_page.saturating_add(page_count);
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(self.lock.get()) };
        // SAFETY: exclusive spinlock ownership.
        let entries = unsafe { &mut *self.entries.get() };

        // The new update replaces this exact Windows-supplied VA range even if
        // it maps a non-Helios allocation, a device segment, or invalid PTEs.
        entries.retain(|entry| entry.gpu_page < first_page || entry.gpu_page >= end_page);

        let mut ok = true;
        if track_system_pages {
            let repeat = update.Flags.Repeat() != 0;
            for i in 0..update.NumPageTableEntries as usize {
                // Repeat means pPageTableEntries names one value replicated
                // across the whole update; otherwise it names Num entries.
                let pte_index = if repeat { 0 } else { i };
                // SAFETY: index follows the DXGK_UPDATEPAGETABLEFLAGS contract.
                let pte =
                    unsafe { core::ptr::read_unaligned(update.pPageTableEntries.add(pte_index)) };
                let bits = unsafe { pte.__bindgen_anon_1.__bindgen_anon_1 };
                let valid = bits.Valid() != 0;
                let zero = bits.Zero() != 0;
                let segment = bits.Segment() as u32;
                if !valid || zero || segment != 0 {
                    continue;
                }
                if entries.len() == MAX_PAGING_SYSTEM_PTES {
                    ok = false;
                    break;
                }
                entries.push(PagingSystemPte {
                    gpu_page: first_page + i as u64,
                    physical_page: unsafe { pte.__bindgen_anon_2.PageAddress },
                });
            }
            entries.sort_unstable_by_key(|entry| entry.gpu_page);
        }
        BAR_VIRTUAL_PTES.store(entries.len() as u32, Ordering::Relaxed);
        unsafe { KeReleaseSpinLock(self.lock.get(), irql) };

        if !ok {
            BAR_ERR_SHADOW_FULL.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }

    /// Resolve a paging-process GPU-VA byte range to the exact ordered physical
    /// pages currently supplied by VidMm.
    fn resolve(&self, virtual_address: u64, size: u64) -> Option<Vec<u64>> {
        if size == 0 {
            return Some(Vec::new());
        }
        let first_page = virtual_address >> 12;
        let last_byte = (virtual_address & 0xFFF).checked_add(size - 1)?;
        let page_count = (last_byte >> 12).checked_add(1)?;
        let page_count_usize = usize::try_from(page_count).ok()?;

        let mut pages = Vec::new();
        if pages.try_reserve_exact(page_count_usize).is_err() {
            return None;
        }

        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(self.lock.get()) };
        // SAFETY: shared read while holding the table spinlock.
        let entries = unsafe { &*self.entries.get() };
        let mut resolved = true;
        for i in 0..page_count {
            let gpu_page = first_page + i;
            match entries.binary_search_by_key(&gpu_page, |entry| entry.gpu_page) {
                Ok(index) => pages.push(entries[index].physical_page),
                Err(_) => {
                    resolved = false;
                    break;
                }
            }
        }
        unsafe { KeReleaseSpinLock(self.lock.get(), irql) };
        resolved.then_some(pages)
    }
}

/// `NormalPagePriority | MdlMappingNoExecute` for the system-MDL kernel map.
const MDL_MAP_PRIORITY: u32 = 16 | 0x4000_0000;
/// `MDL_MAPPED_TO_SYSTEM_VA | MDL_SOURCE_IS_NONPAGED_POOL` — MappedSystemVa valid.
const MDL_HAS_SYSTEM_VA: i16 = 0x0001 | 0x0004;
/// `KernelMode` (`KPROCESSOR_MODE`).
const KERNEL_MODE: i8 = 0;

/// A paging op's system-memory MDL mapping, carrying the length the raw pointer
/// does not.
///
/// The blob side of every copy has always been bounds-checked; the MDL side was
/// not. `mdl_off` and `TransferSize` are VidMm-supplied and were applied raw,
/// with a comment ("validated post-boot via PgTs/PgTd") standing in for the
/// check — on the eviction arm that is a kernel-memory WRITE past the mapped
/// buffer. `_MDL.ByteCount` is exactly the length of the described buffer and is
/// one field read away, so the bound is now carried by construction
/// (k-paging-03).
#[derive(Clone, Copy)]
struct MdlWindow {
    va: core::ptr::NonNull<u8>,
    len: u64,
}

impl MdlWindow {
    /// Pointer to `bytes` mapped bytes at `offset`, or `None` (counted by the
    /// caller as PgEb) if any part of that range leaves the mapping.
    fn slice_at(&self, offset: u64, bytes: u64) -> Option<*mut u8> {
        let offset = helios_kmd_logic::window_range(self.len, offset, bytes)?;
        // SAFETY: offset + bytes <= len was just proven, so the result stays
        // inside the buffer `va` describes.
        Some(unsafe { self.va.as_ptr().add(offset as usize) })
    }
}

/// Kernel VA + length of a paging op's system-memory MDL (VidMm passes locked
/// MDLs). Reuses an existing system mapping if the MDL has one; otherwise maps
/// KernelMode/cached (released when VidMm frees the MDL, the
/// `MmGetSystemAddressForMdlSafe` pattern). Returns `None` (counted) on failure.
///
/// `ByteOffset` is deliberately NOT added to the returned pointer: both branches
/// already point at the start of the described buffer, whose length is exactly
/// `ByteCount`, so adding it would itself introduce an off-by-`ByteOffset`
/// overrun.
///
/// # Safety
/// `mdl` must be a valid, locked MDL for the duration of the paging op.
unsafe fn mdl_system_va(mdl: PMDL) -> Option<MdlWindow> {
    if mdl.is_null() {
        return None;
    }
    // SAFETY: valid MDL per the fn contract.
    unsafe {
        let len = u64::from((*mdl).ByteCount);
        if (*mdl).MdlFlags & MDL_HAS_SYSTEM_VA != 0 {
            return core::ptr::NonNull::new((*mdl).MappedSystemVa as *mut u8)
                .map(|va| MdlWindow { va, len });
        }
        let va = MmMapLockedPagesSpecifyCache(
            mdl,
            KERNEL_MODE,
            _MEMORY_CACHING_TYPE::MmCached,
            core::ptr::null_mut(),
            0, // BugCheckOnFailure = FALSE → NULL return for KernelMode failure
            MDL_MAP_PRIORITY,
        );
        match core::ptr::NonNull::new(va as *mut u8) {
            Some(va) => Some(MdlWindow { va, len }),
            None => {
                BAR_ERR_MDL.fetch_add(1, Ordering::Relaxed);
                None
            }
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
    let prep = match crate::virtio::ctrl::map_blob_prepare(
        adapter,
        crate::virtio::gpu::OwnerFilter::Any,
        resource_id,
    ) {
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

/// Copy between a mapped blob range and the exact locked system pages named by
/// a paging-process PTE range. Adjacent physical pages are mapped as one run to
/// avoid one `MmMapIoSpace` round-trip per 4-KiB page.
///
/// `system_pages` comes either from [`PagingPteShadow::resolve`] for the current
/// virtual transfer or from the exact locked MDL captured when Windows paged
/// this allocation to system memory. It covers
/// `system_virtual_address..+size` in order. `blob_offset..+size` must already
/// have been bounds-checked against the blob mapping.
unsafe fn copy_blob_system_pages(
    blob: *mut u8,
    blob_offset: u64,
    system_pages: &[u64],
    system_virtual_address: u64,
    size: u64,
    blob_to_system: bool,
) -> bool {
    if size == 0 {
        return true;
    }
    let mut page_index = 0usize;
    let mut system_page_offset = (system_virtual_address & 0xFFF) as usize;
    let mut copied = 0u64;

    while copied < size {
        if page_index >= system_pages.len() {
            BAR_ERR_VIRTUAL.fetch_add(1, Ordering::Relaxed);
            return false;
        }

        // Coalesce the physical run that Windows supplied. The pages remain
        // locked for the paging operation's lifetime.
        let mut run_pages = 1usize;
        while page_index + run_pages < system_pages.len()
            && system_pages[page_index + run_pages]
                == system_pages[page_index + run_pages - 1].saturating_add(1)
        {
            run_pages += 1;
        }
        // `checked_shl(12)` was not an overflow guard — it fails only for a
        // shift count >= 64, so with a constant 12 this arm was dead and PgEv
        // could never fire for it. `Pfn::physical_address` checks the multiply
        // AND rejects an address whose i64 QuadPart cast would go negative
        // (k-paging-10).
        let Some(physical_address) =
            helios_kmd_logic::Pfn(system_pages[page_index]).physical_address()
        else {
            BAR_ERR_VIRTUAL.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let map_size = (run_pages as u64) << 12;
        let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
        pa.QuadPart = physical_address as i64;
        // SAFETY: the pages are either locked for the current paging operation
        // or remain owned by the allocation's system backing until the inverse
        // transfer/destroy removes their recorded association.
        let system =
            unsafe { MmMapIoSpace(pa, map_size, _MEMORY_CACHING_TYPE::MmCached) } as *mut u8;
        if system.is_null() {
            BAR_ERR_MAP.fetch_add(1, Ordering::Relaxed);
            return false;
        }

        let available = map_size.saturating_sub(system_page_offset as u64);
        let chunk = (size - copied).min(available);
        // SAFETY: blob bounds were checked by the caller; system is a mapping
        // of map_size bytes and system_page_offset + chunk <= map_size.
        unsafe {
            let blob_ptr = blob.add((blob_offset + copied) as usize);
            let system_ptr = system.add(system_page_offset);
            if blob_to_system {
                core::ptr::copy_nonoverlapping(blob_ptr, system_ptr, chunk as usize);
            } else {
                core::ptr::copy_nonoverlapping(system_ptr, blob_ptr, chunk as usize);
            }
        }
        // SAFETY: exact mapping returned above.
        unsafe { MmUnmapIoSpace(system as *mut c_void, map_size) };

        copied += chunk;
        page_index += run_pages;
        system_page_offset = 0;
    }
    true
}

/// Capture the exact physical pages in a Windows paging-transfer MDL.
///
/// `system_start` is the mapped byte at which this allocation range begins.
/// Every page number comes from `MmGetPhysicalAddress` while the MDL is locked;
/// no allocation dimensions, process identity, or placement ordering is used.
unsafe fn remember_system_backing(
    adapter: &AdapterContext,
    resource_id: u32,
    blob_offset: u64,
    size: u64,
    system_start: *mut u8,
) -> bool {
    if size == 0 || system_start.is_null() {
        return false;
    }
    let first_pa = unsafe { MmGetPhysicalAddress(system_start.cast()) }.QuadPart as u64;
    let first_page_offset = (first_pa & 0xFFF) as u32;
    let page_count = (u64::from(first_page_offset)
        .saturating_add(size)
        .saturating_add(4095))
        >> 12;
    let Ok(page_count) = usize::try_from(page_count) else {
        BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    let mut pages = Vec::new();
    if pages.try_reserve_exact(page_count).is_err() {
        BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    pages.push(first_pa >> 12);
    for index in 1..page_count {
        let delta = (4096usize - first_page_offset as usize)
            .saturating_add((index - 1).saturating_mul(4096));
        let pa = unsafe { MmGetPhysicalAddress(system_start.add(delta).cast()) }.QuadPart as u64;
        if pa & 0xFFF != 0 {
            BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        pages.push(pa >> 12);
    }
    let backing = SystemBackingSnapshot {
        resource_id,
        blob_offset,
        size,
        first_page_offset,
        pages: Arc::from(pages.into_boxed_slice()),
    };
    if adapter.system_backings.replace(backing) {
        BAR_SYSTEM_BACKING_CAPTURES.fetch_add(1, Ordering::Relaxed);
        true
    } else {
        BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
        false
    }
}

/// Mirror a completed Present destination blob into the exact system-memory
/// backing Windows previously supplied for that allocation.
///
/// `None` means the allocation is no longer system-backed (for example Windows
/// paged it back into the BAR before this Present). `Some(false)` is a real
/// mapping/copy failure and must not be silently treated as a successful frame.
pub(crate) unsafe fn mirror_present_system_backing(
    adapter: &AdapterContext,
    resource_id: u32,
) -> Option<bool> {
    let backing = adapter.system_backings.snapshot(resource_id)?;
    let mut copied = false;
    let mapped = unsafe {
        with_blob_bytes(adapter, resource_id, |blob, len| {
            if backing.blob_offset.saturating_add(backing.size) > len {
                return;
            }
            copied = copy_blob_system_pages(
                blob,
                backing.blob_offset,
                backing.pages.as_ref(),
                u64::from(backing.first_page_offset),
                backing.size,
                true,
            );
        })
    };
    let ok = mapped && copied;
    if ok {
        BAR_SYSTEM_BACKING_MIRRORS.fetch_add(1, Ordering::Relaxed);
    } else {
        BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
    }
    Some(ok)
}

/// WDDM 2.x `VIRTUAL_TRANSFER` for a Helios blob allocation.
///
/// The allocation handle, direction, VAs, size, and the leaf PTEs resolving the
/// system-memory side are all supplied by VidMm. The allocation offset applies
/// only to the blob and is deliberately not added to either virtual address, as
/// required by `DXGK_BUILDPAGINGBUFFER_TRANSFERVIRTUAL`.
unsafe fn bar_virtual_transfer(
    adapter: &AdapterContext,
    transfer: &DXGK_BUILDPAGINGBUFFER_TRANSFERVIRTUAL,
) -> bool {
    let Some(alloc) = (unsafe { paging_alloc_info(transfer.hAllocation) }) else {
        BAR_ERR_VIRTUAL.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    if !alloc.bar_eligible {
        // Device-local optimal resources cannot be interpreted as linear bytes.
        // This preserves the existing host-owned-content behavior; the software
        // content engine applies only to allocations KMD made blob-linear.
        BAR_DEVICE_OP_SKIPS.fetch_add(1, Ordering::Relaxed);
        return true;
    }

    let offset = transfer.AllocationOffsetInBytes;
    let size = transfer.TransferSizeInBytes;
    if offset.checked_add(size).is_none_or(|end| end > alloc.size) {
        BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    BAR_LAST_XFER_OFF.store(offset as u32, Ordering::Relaxed);
    BAR_LAST_XFER_FLAGS.store(
        unsafe { transfer.Flags.__bindgen_anon_1.Flags },
        Ordering::Relaxed,
    );
    BAR_LAST_VIRTUAL_SRC.store(transfer.SourceVirtualAddress, Ordering::Relaxed);
    BAR_LAST_VIRTUAL_DST.store(transfer.DestinationVirtualAddress, Ordering::Relaxed);

    use crate::dxgk::_DXGK_MEMORY_TRANSFER_DIRECTION as Direction;
    let (system_va, blob_to_system) = match transfer.TransferDirection {
        Direction::DXGK_MEMORY_TRANSFER_LOCAL_TO_SYSTEM => {
            (transfer.DestinationVirtualAddress, true)
        }
        Direction::DXGK_MEMORY_TRANSFER_SYSTEM_TO_LOCAL => (transfer.SourceVirtualAddress, false),
        Direction::DXGK_MEMORY_TRANSFER_LOCAL_TO_LOCAL => {
            // The allocation's bytes are intrinsic to its venus blob; a VidMm
            // placement move does not move or alias that host memory object.
            BAR_XFER_MOVE.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        _ => {
            BAR_ERR_VIRTUAL.fetch_add(1, Ordering::Relaxed);
            return false;
        }
    };

    let Some(system_pages) = adapter.paging_pte_shadow.resolve(system_va, size) else {
        BAR_ERR_VIRTUAL.fetch_add(1, Ordering::Relaxed);
        return false;
    };

    let mut copied = false;
    let mapped = unsafe {
        with_blob_bytes(adapter, alloc.resource_id, |blob, blob_size| {
            if offset.checked_add(size).is_none_or(|end| end > blob_size) {
                BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                return;
            }
            copied = copy_blob_system_pages(
                blob,
                offset,
                &system_pages,
                system_va,
                size,
                blob_to_system,
            );
        })
    };
    if mapped && copied {
        if blob_to_system {
            let backing = SystemBackingSnapshot {
                resource_id: alloc.resource_id,
                blob_offset: offset,
                size,
                first_page_offset: (system_va & 0xFFF) as u32,
                pages: Arc::from(system_pages.into_boxed_slice()),
            };
            if adapter.system_backings.replace(backing) {
                BAR_SYSTEM_BACKING_CAPTURES.fetch_add(1, Ordering::Relaxed);
            } else {
                BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            BAR_XFER_OUT.fetch_add(1, Ordering::Relaxed);
        } else {
            adapter.system_backings.remove(alloc.resource_id);
            BAR_XFER_IN.fetch_add(1, Ordering::Relaxed);
        }
        true
    } else {
        false
    }
}

/// Classic TRANSFER touching the BAR segment: content copy between the
/// allocation's blob and its system-memory backing (synchronous — done before
/// the paging fence retires, so VidMm's content model stays truthful across
/// eviction/re-commit). PASSIVE_LEVEL.
unsafe fn bar_transfer(
    adapter: &AdapterContext,
    bar_id: u32,
    t: &_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1,
) -> PagingOpOutcome {
    let src_seg = t.Source.SegmentId;
    let dst_seg = t.Destination.SegmentId;
    if src_seg != bar_id && dst_seg != bar_id {
        return PagingOpOutcome::NotOurs; // aperture/system transfer — null engine
    }
    let Some(alloc) = (unsafe { paging_alloc_info(t.hAllocation) }) else {
        // The transfer names the BAR segment but no live Helios allocation:
        // there is nothing this driver can copy, and the caller must not read
        // that as "content moved".
        BAR_ERR_XFER_HANDLE.fetch_add(1, Ordering::Relaxed);
        return PagingOpOutcome::Failed(paging_failure());
    };
    if !alloc.bar_eligible {
        // A device-local OPTIMAL image cannot be interpreted as linear bytes.
        // VidMm placement bookkeeping is decorative for this host-owned memory;
        // never issue RESOURCE_MAP_BLOB for it.
        BAR_DEVICE_OP_SKIPS.fetch_add(1, Ordering::Relaxed);
        return PagingOpOutcome::NotOurs;
    }
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
            let Some(window) = (unsafe { mdl_system_va(mdl.cast()) }) else {
                // PgEx counted inside mdl_system_va. The page-in did not happen,
                // so the blob still holds stale bytes — never report success.
                return PagingOpOutcome::Failed(paging_failure());
            };
            // The MDL side is range-checked exactly like the blob side.
            let Some(src) = window.slice_at(mdl_off, bytes) else {
                BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                return PagingOpOutcome::Failed(paging_failure());
            };
            let mut copied = false;
            let ok = unsafe {
                with_blob_bytes(adapter, alloc.resource_id, |dst, len| {
                    if blob_off.saturating_add(bytes) > len {
                        BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    // SAFETY: dst covers `len` blob bytes and src covers `bytes`
                    // mapped MDL bytes, both checked above.
                    core::ptr::copy_nonoverlapping(src, dst.add(blob_off as usize), bytes as usize);
                    copied = true;
                })
            };
            if !(ok && copied) {
                // PgEm (blob map) or PgEb (out-of-blob range) already counted.
                return PagingOpOutcome::Failed(paging_failure());
            }
            // The inverse transfer makes the BAR blob authoritative again.
            adapter.system_backings.remove(alloc.resource_id);
            BAR_XFER_IN.fetch_add(1, Ordering::Relaxed);
            PagingOpOutcome::Executed
        }
        // Eviction: blob → system backing.
        (s, 0) if s == bar_id => {
            // SAFETY: Destination.pMdl arm is valid for SegmentId 0. Cast as above.
            let mdl = unsafe { *t.Destination.__bindgen_anon_1.pMdl.as_ref() };
            let Some(window) = (unsafe { mdl_system_va(mdl.cast()) }) else {
                // PgEx counted inside mdl_system_va. The eviction did not happen;
                // reporting success here would lose the allocation's only copy.
                return PagingOpOutcome::Failed(paging_failure());
            };
            // THE WRITE SIDE: an unchecked `mdl_off + bytes` here is a kernel
            // memory write past the mapped buffer.
            let Some(dst_start) = window.slice_at(mdl_off, bytes) else {
                BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                return PagingOpOutcome::Failed(paging_failure());
            };
            let mut copied = false;
            let ok = unsafe {
                with_blob_bytes(adapter, alloc.resource_id, |src, len| {
                    if blob_off.saturating_add(bytes) > len {
                        BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    // SAFETY: src covers `len` blob bytes and dst_start covers
                    // `bytes` mapped MDL bytes, both checked above.
                    core::ptr::copy_nonoverlapping(src.add(blob_off as usize), dst_start, bytes as usize);
                    copied = true;
                })
            };
            if !(ok && copied) {
                // PgEm / PgEb already counted.
                return PagingOpOutcome::Failed(paging_failure());
            }
            let _ = unsafe {
                remember_system_backing(adapter, alloc.resource_id, blob_off, bytes, dst_start)
            };
            BAR_XFER_OUT.fetch_add(1, Ordering::Relaxed);
            PagingOpOutcome::Executed
        }
        // Move within the segment: content is intrinsic to the blob; the CPU
        // view follows the aperture maps. Nothing to copy.
        (s, d) if s == bar_id && d == bar_id => {
            BAR_XFER_MOVE.fetch_add(1, Ordering::Relaxed);
            PagingOpOutcome::Executed
        }
        // BAR ↔ aperture/paging-RAM combinations are not part of the declared
        // allocation segment sets; loud counter, no silent data motion.
        _ => {
            BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
            PagingOpOutcome::Failed(paging_failure())
        }
    }
}

/// Classic FILL of a BAR-segment allocation: CPU-fill the blob. PASSIVE_LEVEL.
unsafe fn bar_fill(
    adapter: &AdapterContext,
    bar_id: u32,
    f: &_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_2,
) -> PagingOpOutcome {
    if f.Destination.SegmentId != bar_id {
        return PagingOpOutcome::NotOurs;
    }
    let Some(alloc) = (unsafe { paging_alloc_info(f.hAllocation) }) else {
        // Same class as PgEh on the transfer side: a BAR-segment fill naming no
        // live allocation is a refusal, not a no-op.
        BAR_ERR_FILL_HANDLE.fetch_add(1, Ordering::Relaxed);
        return PagingOpOutcome::Failed(paging_failure());
    };
    if !alloc.bar_eligible {
        BAR_DEVICE_OP_SKIPS.fetch_add(1, Ordering::Relaxed);
        return PagingOpOutcome::NotOurs;
    }
    let fill_len = f.FillSize as u64;
    let pattern = f.FillPattern;
    let mut filled = false;
    let ok = unsafe {
        with_blob_bytes(adapter, alloc.resource_id, |dst, len| {
            // REFUSE, do not clamp (M8/k-paging-19). The VIRTUAL_FILL arm twelve
            // lines below has always refused an over-long fill; clamping here
            // meant one condition had two policies, and the clamped tail was a
            // silent partial fill. The classic FILL arm carries no allocation
            // offset, so "fill from blob start" stays correct — only
            // clamp-versus-refuse changes.
            if fill_len > len {
                BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                return;
            }
            fill_pattern(dst, fill_len as usize, pattern);
            filled = true;
        })
    };
    if !(ok && filled) {
        // PgEm (blob map) or PgEb (oversized fill) already counted.
        return PagingOpOutcome::Failed(paging_failure());
    }
    BAR_FILLS.fetch_add(1, Ordering::Relaxed);
    PagingOpOutcome::Executed
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
    if n >= 2 && u.Flags.Repeat() == 0 {
        let last = unsafe { core::ptr::read_unaligned(u.pPageTableEntries.add((n - 1) as usize)) };
        let last_valid = unsafe { last.__bindgen_anon_1.__bindgen_anon_1 }.Valid() != 0;
        if last_valid && unsafe { last.__bindgen_anon_2.PageAddress } != page0 + (n - 1) {
            BAR_ERR_DISCONTIG.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    // Same guard as the transfer path: an unchecked `page0 << 12` wraps.
    let Some(page0_address) = helios_kmd_logic::Pfn(page0).physical_address() else {
        BAR_ERR_VIRTUAL.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if let Some(base) = page0_address
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
        let update = unsafe { args.__bindgen_anon_1.UpdatePageTable.as_ref() };
        let track_system_pages = unsafe { paging_alloc_info(update.hAllocation) }
            .is_some_and(|alloc| alloc.bar_eligible);
        // Preserve the exact leaf mapping before retiring the page-table update.
        // Every update clears its Windows-supplied VA range first, including
        // updates for unrelated allocations and explicit unmaps.
        if !unsafe {
            adapter
                .paging_pte_shadow
                .update_leaf(update, track_system_pages)
        } {
            // NO REGISTRY FLUSH HERE. This branch is the one the comment above
            // declares DISPATCH-safe, and `dump_bar_counters` is 24 synchronous
            // `RtlWriteRegistryValue` calls — PASSIVE_LEVEL only. It stood
            // directly against the driver's hardest invariant, twelve lines above
            // a content path that installs a runtime IRQL gate precisely because
            // the documented PASSIVE contract is not trusted (k-paging-05).
            // Nothing is lost: `update_leaf` already stored `BAR_ERR_SHADOW_FULL`
            // (PgEf) into its atomic, and the next PASSIVE content op mirrors the
            // whole block, so only the latency of that one value changes.
            return STATUS_INSUFFICIENT_RESOURCES;
        }
        unsafe { bar_harvest_page_table(bar.seg_id, bar.size, update) };
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
    //
    // IRQL-DEGRADE POLICY (decided here, where the status is chosen): this arm
    // keeps STATUS_SUCCESS. It is the one place that cannot honestly fail,
    // because the gate runs before the union is parsed — it covers content ops
    // for allocations that are NOT ours (another segment, a device-local image)
    // just as much as ours, and failing those would refuse work this driver was
    // never asked to do. PgEi is the loud signal instead: it has never moved,
    // and a same-boot nonzero value is a design-gap escalation, not something
    // to absorb.
    // SAFETY: KeGetCurrentIrql is callable at any IRQL.
    if unsafe { KeGetCurrentIrql() } != PASSIVE_LEVEL_IRQL {
        BAR_ERR_IRQL.fetch_add(1, Ordering::Relaxed);
        return STATUS_SUCCESS;
    }

    // Every content arm yields a `PagingOpOutcome`, so the match itself is the
    // driver's answer: `Failed` is the only variant that reaches VidMm as a
    // status, and every arm that produces one routes through `paging_failure()`.
    let outcome = match args.Operation {
        PagingOp::DXGK_OPERATION_TRANSFER => {
            // SAFETY: union arm selected by Operation.
            unsafe { bar_transfer(adapter, bar.seg_id, args.__bindgen_anon_1.Transfer.as_ref()) }
        }
        PagingOp::DXGK_OPERATION_FILL => {
            // SAFETY: union arm selected by Operation.
            unsafe { bar_fill(adapter, bar.seg_id, args.__bindgen_anon_1.Fill.as_ref()) }
        }
        PagingOp::DXGK_OPERATION_DISCARD_CONTENT => {
            // SAFETY: union arm selected by Operation.
            let d = unsafe { args.__bindgen_anon_1.DiscardContent.as_ref() };
            if let Some(alloc) = unsafe { paging_alloc_info(d.hAllocation) } {
                adapter.system_backings.remove(alloc.resource_id);
            }
            if d.SegmentId == bar.seg_id && unsafe { paging_alloc_info(d.hAllocation) }.is_some() {
                // Content lives in the blob; nothing to release here (aperture
                // unmaps handle CPU visibility). Counted for the op census.
                BAR_DISCARDS.fetch_add(1, Ordering::Relaxed);
            }
            // Discard cannot fail: there is nothing to move.
            PagingOpOutcome::Executed
        }
        PagingOp::DXGK_OPERATION_VIRTUAL_FILL => {
            // SAFETY: union arm selected by Operation.
            let fv = unsafe { args.__bindgen_anon_1.FillVirtual.as_ref() };
            match unsafe { paging_alloc_info(fv.hAllocation) } {
                None => {
                    BAR_ERR_FILL_HANDLE.fetch_add(1, Ordering::Relaxed);
                    PagingOpOutcome::Failed(paging_failure())
                }
                Some(alloc) if !alloc.bar_eligible => {
                    BAR_DEVICE_OP_SKIPS.fetch_add(1, Ordering::Relaxed);
                    PagingOpOutcome::NotOurs
                }
                Some(alloc) => {
                    let off = fv.AllocationOffsetInBytes;
                    let fill_len = fv.FillSizeInBytes;
                    let pattern = fv.FillPattern;
                    // Evidence for the blob-versus-system-pages asymmetry
                    // documented on PgFv; the fill below is unchanged.
                    if adapter.system_backings.contains(alloc.resource_id) {
                        BAR_VIRTUAL_FILL_SYSTEM.fetch_add(1, Ordering::Relaxed);
                    }
                    let mut filled = false;
                    let ok = unsafe {
                        with_blob_bytes(adapter, alloc.resource_id, |dst, len| {
                            if off.saturating_add(fill_len) > len {
                                BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                                return;
                            }
                            // SAFETY: bounds-checked against the blob mapping.
                            fill_pattern(dst.add(off as usize), fill_len as usize, pattern);
                            filled = true;
                        })
                    };
                    if ok && filled {
                        BAR_FILLS.fetch_add(1, Ordering::Relaxed);
                        PagingOpOutcome::Executed
                    } else {
                        // PgEm / PgEb already counted.
                        PagingOpOutcome::Failed(paging_failure())
                    }
                }
            }
        }
        PagingOp::DXGK_OPERATION_VIRTUAL_TRANSFER => {
            // SAFETY: union arm selected by Operation.
            let tv = unsafe { args.__bindgen_anon_1.TransferVirtual.as_ref() };
            if unsafe { bar_virtual_transfer(adapter, tv) } {
                PagingOpOutcome::Executed
            } else {
                // Was STATUS_UNSUCCESSFUL — the crate's last use of a status two
                // sibling DDIs carry comments about dxgkrnl logging as "Driver
                // returned an invalid NTSTATUS" 197x with adapter resets.
                PagingOpOutcome::Failed(paging_failure())
            }
        }
        _ => PagingOpOutcome::NotOurs,
    };
    dump_bar_counters();
    match outcome {
        PagingOpOutcome::Failed(reason) => reason,
        PagingOpOutcome::Executed | PagingOpOutcome::NotOurs => STATUS_SUCCESS,
    }
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
