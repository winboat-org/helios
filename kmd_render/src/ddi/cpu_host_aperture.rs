//! CPU host aperture DDIs.
//!
//! Two roles:
//!
//! 1. **Segment 2 (paging RAM)**: legacy decorative behavior — VidMm uses the
//!    CPU host aperture during adapter init for paging-process bookkeeping;
//!    the RAM region receives any writes and no hardware consumes them, so the
//!    DDIs only acknowledge (identity aperture over real RAM).
//!
//! 2. **Segment 3 (BAR / venus window head) — REAL.** This is the CPU side of
//!    the two-memory-split fix (HANDOFF_GDI_EXECUTOR_2026_07_05.md ★FINAL):
//!    when dxgkrnl needs CPU access to a segment-3 allocation (Lock, win32k
//!    GDI raster), it calls `DxgkDdiMapCpuHostAperture` with the aperture
//!    pages IT chose inside our declared window-head region. We host-map the
//!    allocation's venus BLOB at exactly that window offset
//!    (`ctrl::map_blob_at`), so the CPU VAs dxgkrnl builds over
//!    `aperture_base + page*4K` read/write THE BLOB BYTES — the same bytes the
//!    RenderGdi executor writes and dwm's venus import samples. One memory.
//!
//!    A blob maps at ONE window offset, whole-blob only, so only
//!    whole-allocation, consecutive-page aperture requests can be served; any
//!    other shape is REFUSED LOUDLY (`ChE*` counters) rather than silently
//!    mis-mapped. dxgkrnl allocates aperture ranges allocation-sized in
//!    practice; the counters prove or refute that assumption post-boot.
//!
//! IRQL: `DxgkDdiMapCpuHostAperture` is documented PASSIVE (it does host
//! round-trips here); a non-PASSIVE arrival is counted (`ChEi`) and refused.
//! `DxgkDdiUnmapCpuHostAperture` at non-PASSIVE leaves the mapping in place
//! (counted): a later `map_blob_at` self-heals via stale-overlap eviction.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use wdk_sys::ntddk::KeGetCurrentIrql;

use crate::adapter::AdapterContext;
use crate::ddi::create_allocation::paging_alloc_info;
use crate::dxgk::*;

pub static CPU_HOST_MAP_COUNT: AtomicU32 = AtomicU32::new(0);
pub static CPU_HOST_UNMAP_COUNT: AtomicU32 = AtomicU32::new(0);
pub static CPU_HOST_LAST_MAP: AtomicU64 = AtomicU64::new(0);
pub static CPU_HOST_LAST_UNMAP: AtomicU64 = AtomicU64::new(0);

// Segment-3 (BAR) aperture engine counters (registry-visible as Ch*).
static BAR_AP_MAPS: AtomicU32 = AtomicU32::new(0); // blob maps performed
static BAR_AP_HITS: AtomicU32 = AtomicU32::new(0); // already mapped at the offset
static BAR_AP_UNMAPS: AtomicU32 = AtomicU32::new(0);
static BAR_AP_LAST_RESID: AtomicU32 = AtomicU32::new(0);
static BAR_AP_LAST_PAGE: AtomicU32 = AtomicU32::new(0);
// Loud failure counters — any nonzero after boot is a design gap to chase.
static BAR_AP_ERR_IRQL: AtomicU32 = AtomicU32::new(0); // arrived > PASSIVE
static BAR_AP_ERR_ALLOC: AtomicU32 = AtomicU32::new(0); // hAllocation unresolvable
static BAR_AP_ERR_PARTIAL: AtomicU32 = AtomicU32::new(0); // not a whole-allocation map
static BAR_AP_ERR_SPARSE: AtomicU32 = AtomicU32::new(0); // aperture pages not consecutive
static BAR_AP_ERR_BOUNDS: AtomicU32 = AtomicU32::new(0); // outside the declared region
static BAR_AP_ERR_MAP: AtomicU32 = AtomicU32::new(0); // map_blob_at failed
static BAR_AP_ERR_UNRESOLVED_UNMAP: AtomicU32 = AtomicU32::new(0); // no blob at unmap offset
                                                                   // Last IRQL a raised-IRQL Map/Unmap arrived at (2 = DISPATCH). Recorded at DIRQL;
                                                                   // flushed to the registry (`ChIq`) from a PASSIVE context (`diag_dump`).
static BAR_AP_LAST_IRQL: AtomicU32 = AtomicU32::new(0);
// Times the raised-IRQL Map path acknowledged because the blob was ALREADY mapped
// at the requested offset (idempotent SUCCESS) vs. had to defer to a PASSIVE retry.
static BAR_AP_IRQL_ACK: AtomicU32 = AtomicU32::new(0);
static BAR_AP_IRQL_DEFER: AtomicU32 = AtomicU32::new(0);

/// Mirror the segment-3 aperture counters into the registry. PASSIVE only.
fn dump_bar_ap_counters() {
    crate::diag::record_named_bytes(b"ChMn", BAR_AP_MAPS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"ChMh", BAR_AP_HITS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"ChUn", BAR_AP_UNMAPS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"ChMr", BAR_AP_LAST_RESID.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"ChMo", BAR_AP_LAST_PAGE.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"ChEi", BAR_AP_ERR_IRQL.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"ChEa", BAR_AP_ERR_ALLOC.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"ChEp", BAR_AP_ERR_PARTIAL.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"ChEs", BAR_AP_ERR_SPARSE.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"ChEb", BAR_AP_ERR_BOUNDS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"ChEm", BAR_AP_ERR_MAP.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"ChEu", BAR_AP_ERR_UNRESOLVED_UNMAP.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"ChMc", CPU_HOST_MAP_COUNT.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"ChIq", BAR_AP_LAST_IRQL.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"ChIa", BAR_AP_IRQL_ACK.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"ChId", BAR_AP_IRQL_DEFER.load(Ordering::Relaxed));
}

/// PASSIVE-only flush of the CPU-host-aperture counters into the registry ring.
/// The raised-IRQL Map path (below) can record its atomics but cannot touch the
/// registry, so a PASSIVE DDI on the mode-set path (`DxgkDdiCommitVidPn`) calls
/// this to surface `ChIq`/`ChEi`/`ChMc` — proving whether `MapCpuHostAperture` is
/// driven at DISPATCH during display activation.
pub fn diag_dump_cpu_host_atomics() {
    dump_bar_ap_counters();
}

const PASSIVE_LEVEL_IRQL: u8 = 0;

pub unsafe extern "C" fn dxgkddi_map_cpu_host_aperture(
    h_adapter: *mut c_void,
    map: IN_CONST_PDXGKARG_MAPCPUHOSTAPERTURE,
) -> NTSTATUS {
    if h_adapter.is_null() || map.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    // SAFETY: valid per DDI contract.
    let args = unsafe { &*map };
    CPU_HOST_MAP_COUNT.fetch_add(1, Ordering::Relaxed);
    CPU_HOST_LAST_MAP.store(
        ((args.SegmentId as u64) << 48)
            | ((args.PhysicalAdapterIndex as u64) << 32)
            | (args.NumberOfPages & 0xFFFF_FFFF),
        Ordering::Relaxed,
    );

    // SAFETY: dxgkrnl hands back our AdapterContext as the miniport context.
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let bar_active = adapter
        .bar_segment
        .as_ref()
        .filter(|b| !b.probe_only && b.seg_id == args.SegmentId as u32);
    let Some(bar) = bar_active else {
        // Paging-RAM segment (or a probe arm): decorative identity aperture
        // over real RAM; acknowledging is correct (see module doc).
        return STATUS_SUCCESS;
    };
    // Refusing (loudly) beats null-succeeding: a null success lets the CPU
    // read/write UNBACKED window offsets (dropped writes / 0xFF reads) — the
    // exact silent-content-loss class this fix exists to kill.
    // SAFETY: KeGetCurrentIrql is callable at any IRQL.
    let irql = unsafe { KeGetCurrentIrql() };
    if irql != PASSIVE_LEVEL_IRQL {
        // Documented PASSIVE, but display activation drives it at DISPATCH on the
        // scan-out primary (ETW-confirmed v71: a registered DDI returned 0xC0000001
        // ×40 during CommitVidPn). `map_blob_at` needs a host round-trip (PASSIVE),
        // so a NEW mapping can't be established here. Contract rules for the return:
        //  1. NEVER return `STATUS_UNSUCCESSFUL` — it is OUT of this DDI's legal set,
        //     so dxgkrnl flags a driver bug and DISCARDS THE ENTIRE VidPn → 0-path
        //     commit → the display never activates (the exact v71 blocker).
        //  2. If the blob is ALREADY host-mapped at the aperture offset dxgkrnl
        //     chose (the PASSIVE paging path established it, `bar_placed`), the CPU
        //     window is live → acknowledge (idempotent SUCCESS). Reads only:
        //     `paging_alloc_info` derefs the handle; `blob_resid_at_offset` runs
        //     under the DISPATCH spinlock — both DISPATCH-safe.
        //  3. Otherwise defer with a LEGAL, retryable status (`STATUS_NO_MEMORY`) so
        //     dxgkrnl re-issues the map at PASSIVE, where the round-trip can run.
        BAR_AP_ERR_IRQL.fetch_add(1, Ordering::Relaxed);
        BAR_AP_LAST_IRQL.store(irql as u32, Ordering::Relaxed);
        let already_mapped = unsafe { paging_alloc_info(args.hAllocation) }
            .filter(|a| {
                a.bar_eligible
                    && args.NumberOfPages != 0
                    && !args.pCpuHostAperturePages.is_null()
                    // whole-allocation, same shape as the PASSIVE map contract below
                    && args.NumberOfPages as u64 == (a.size.saturating_add(4095) >> 12).max(1)
            })
            .map(|a| {
                // SAFETY: pCpuHostAperturePages holds >=1 entry (checked above).
                let page0 = unsafe { core::ptr::read_unaligned(args.pCpuHostAperturePages) } as u64;
                let offset = page0 << 12;
                adapter
                    .with_virtio(|v| v.blob_resid_at_offset(offset))
                    .ok()
                    .flatten()
                    == Some(a.resource_id)
            })
            .unwrap_or(false);
        if already_mapped {
            BAR_AP_IRQL_ACK.fetch_add(1, Ordering::Relaxed);
            return STATUS_SUCCESS;
        }
        BAR_AP_IRQL_DEFER.fetch_add(1, Ordering::Relaxed);
        return STATUS_NO_MEMORY;
    }
    let Some(alloc) = (unsafe { paging_alloc_info(args.hAllocation) }) else {
        BAR_AP_ERR_ALLOC.fetch_add(1, Ordering::Relaxed);
        dump_bar_ap_counters();
        return STATUS_NO_MEMORY;
    };
    if !alloc.bar_eligible {
        // The allocation contract says this object is device-local/opaque.
        // Never reinterpret it as CPU-addressable blob bytes even if VidMm
        // supplies a BAR-segment aperture request.
        BAR_AP_ERR_ALLOC.fetch_add(1, Ordering::Relaxed);
        BAR_AP_LAST_RESID.store(alloc.resource_id, Ordering::Relaxed);
        dump_bar_ap_counters();
        return STATUS_NO_MEMORY;
    }
    let n = args.NumberOfPages;
    let blob_pages = (alloc.size.saturating_add(4095) >> 12).max(1);
    if n == 0 || args.pCpuHostAperturePages.is_null() {
        BAR_AP_ERR_PARTIAL.fetch_add(1, Ordering::Relaxed);
        dump_bar_ap_counters();
        return STATUS_NO_MEMORY;
    }
    // Whole-allocation only: a blob maps at one offset, whole-blob — a partial
    // map would spill blob pages over neighboring aperture assignments.
    if n != blob_pages {
        BAR_AP_ERR_PARTIAL.fetch_add(1, Ordering::Relaxed);
        BAR_AP_LAST_RESID.store(alloc.resource_id, Ordering::Relaxed);
        dump_bar_ap_counters();
        return STATUS_NO_MEMORY;
    }
    // SAFETY: pCpuHostAperturePages holds NumberOfPages entries for the call.
    let page0 = unsafe { core::ptr::read_unaligned(args.pCpuHostAperturePages) } as u64;
    for i in 1..n {
        let p =
            unsafe { core::ptr::read_unaligned(args.pCpuHostAperturePages.add(i as usize)) } as u64;
        if p != page0 + i {
            BAR_AP_ERR_SPARSE.fetch_add(1, Ordering::Relaxed);
            dump_bar_ap_counters();
            return STATUS_NO_MEMORY;
        }
    }
    let offset = page0 << 12;
    if offset.saturating_add(n << 12) > bar.size {
        BAR_AP_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
        dump_bar_ap_counters();
        return STATUS_NO_MEMORY;
    }

    match crate::virtio::ctrl::map_blob_at(adapter, alloc.resource_id, offset) {
        Ok(_prep) => {
            BAR_AP_MAPS.fetch_add(1, Ordering::Relaxed);
            BAR_AP_LAST_RESID.store(alloc.resource_id, Ordering::Relaxed);
            BAR_AP_LAST_PAGE.store(page0 as u32, Ordering::Relaxed);
            dump_bar_ap_counters();
            STATUS_SUCCESS
        }
        Err(_e) => {
            BAR_AP_ERR_MAP.fetch_add(1, Ordering::Relaxed);
            BAR_AP_LAST_RESID.store(alloc.resource_id, Ordering::Relaxed);
            dump_bar_ap_counters();
            // Legal (retryable) status, NOT the out-of-contract STATUS_UNSUCCESSFUL
            // that dxgkrnl treats as a driver bug and discards the whole VidPn for.
            STATUS_NO_MEMORY
        }
    }
}

pub unsafe extern "C" fn dxgkddi_unmap_cpu_host_aperture(
    h_adapter: *mut c_void,
    unmap: IN_CONST_PDXGKARG_UNMAPCPUHOSTAPERTURE,
) -> NTSTATUS {
    if h_adapter.is_null() || unmap.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    // SAFETY: valid per DDI contract.
    let args = unsafe { &*unmap };
    CPU_HOST_UNMAP_COUNT.fetch_add(1, Ordering::Relaxed);
    CPU_HOST_LAST_UNMAP.store(
        ((args.SegmentId as u64) << 48)
            | ((args.PhysicalAdapterIndex as u64) << 32)
            | (args.NumberOfPages & 0xFFFF_FFFF),
        Ordering::Relaxed,
    );

    // Tear down the blob mapping backing this aperture range so dxgkrnl can
    // re-book the pages without overlapping host window subregions. No
    // hAllocation in this DDI — resolve by aperture offset.
    // SAFETY: our AdapterContext (DDI contract).
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let is_bar = adapter.bar_segment.as_ref().map_or(false, |b| {
        !b.probe_only && b.seg_id == args.SegmentId as u32
    });
    if !is_bar {
        return STATUS_SUCCESS;
    }
    if args.NumberOfPages == 0 || args.pCpuHostAperturePages.is_null() {
        return STATUS_SUCCESS;
    }
    // SAFETY: pCpuHostAperturePages holds NumberOfPages entries for the call.
    let page0 = unsafe { core::ptr::read_unaligned(args.pCpuHostAperturePages) } as u64;
    let offset = page0 << 12;
    // A non-PASSIVE arrival cannot run the unmap round-trip; leave the mapping
    // (counted) — the next map_blob_at over the range evicts it (self-heal).
    // SAFETY: KeGetCurrentIrql is callable at any IRQL.
    if unsafe { KeGetCurrentIrql() } != PASSIVE_LEVEL_IRQL {
        BAR_AP_ERR_IRQL.fetch_add(1, Ordering::Relaxed);
        return STATUS_SUCCESS;
    }
    let resid = adapter
        .with_virtio(|v| v.blob_resid_at_offset(offset))
        .unwrap_or(None);
    match resid {
        Some(res) => {
            let _ = crate::virtio::ctrl::resource_unmap_blob(adapter, res);
            let _ = adapter.with_virtio(|v| v.blob_note_unmapped(res));
            BAR_AP_UNMAPS.fetch_add(1, Ordering::Relaxed);
        }
        None => {
            // Nothing mapped there (already torn down at DestroyAllocation, or
            // a map this driver refused). Counted for visibility.
            BAR_AP_ERR_UNRESOLVED_UNMAP.fetch_add(1, Ordering::Relaxed);
        }
    }
    dump_bar_ap_counters();
    STATUS_SUCCESS
}
