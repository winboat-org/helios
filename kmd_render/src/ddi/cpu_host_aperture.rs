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
//!    when dxgkrnl needs CPU access to a BAR-segment allocation (Lock, win32k
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
use core::sync::atomic::{AtomicU32, Ordering};

use wdk_sys::ntddk::KeGetCurrentIrql;

use crate::adapter::AdapterContext;
use crate::ddi::create_allocation::{paging_alloc_info, PagingAllocInfo};
use crate::dxgk::*;

pub static CPU_HOST_MAP_COUNT: AtomicU32 = AtomicU32::new(0);
/// DDI-level unmap count. Reported as `ChUc` and through QUERY_STATS V3, so the
/// map/unmap pairing can be checked against `ChMc`.
pub static CPU_HOST_UNMAP_COUNT: AtomicU32 = AtomicU32::new(0);
// CPU_HOST_LAST_MAP / CPU_HOST_LAST_UNMAP were write-only: nothing loaded them,
// no report carried them and no ntoseye recipe named them (R315/x-dup-dead-22).
// Their information — segment id, adapter index, page count — is in ChMr/ChMo
// and the S-ring. Deleted rather than reported.

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
// R618: times the page count derived from the ALLOCATION's own size disagreed
// with the page count implied by the TRACKED BLOB's size, and the last such
// delta in pages. The whole-allocation refusal below computes `blob_pages` from
// `alloc.size` while `map_blob_at` maps whatever length the tracked blob size
// implies — two different sources for the same quantity. COUNTER ONLY: do not
// switch the authoritative source until these prove the two agree on every
// boot, because a host-rounded vs page-rounded difference would change the
// BAR_AP_ERR_PARTIAL refusal population and make it a behaviour change.
static BAR_AP_SIZE_MISMATCH: AtomicU32 = AtomicU32::new(0);
static BAR_AP_SIZE_DELTA: AtomicU32 = AtomicU32::new(0);
// R618: a BAR-eligible allocation whose size is NOT host-authoritative. That is
// the state the aperture size check has always been unsafe in, and it was
// prevented only incidentally, by `bar_eligible` requiring `venus_memory_id != 0`.
// Making adopted DEVICE_MEMORY allocations BAR-eligible — explicitly
// contemplated in create_allocation.rs — would open it, and a UMD reporting a
// larger `ap.size` than the real blob would then get an aperture assignment of
// blob_pages pages while the map covers only the real blob length, with the
// CPU view's tail addressing unbacked window offsets. Must stay 0.
static BAR_AP_SIZE_PROVENANCE: AtomicU32 = AtomicU32::new(0);

/// The BAR-segment aperture counter block, mirrored through the shared throttled
/// emitter (R317). Same names and encodings; the cadence changes — this ran on
/// every map, unmap and refusal (17 synchronous registry writes each). Failure
/// counters still surface on the operation that produced them.
static AP_FLUSH_TICKS: AtomicU32 = AtomicU32::new(0);
static AP_FLUSH_FAILURES: AtomicU32 = AtomicU32::new(0);

const fn e(name: &'static [u8], value: &'static AtomicU32) -> crate::diag::CounterEntry {
    crate::diag::CounterEntry {
        name,
        value: crate::diag::CounterRef::U32(value),
        failure: false,
    }
}
const fn f(name: &'static [u8], value: &'static AtomicU32) -> crate::diag::CounterEntry {
    crate::diag::CounterEntry {
        name,
        value: crate::diag::CounterRef::U32(value),
        failure: true,
    }
}

static AP_COUNTERS: crate::diag::CounterBlock = crate::diag::CounterBlock {
    entries: &[
        e(b"ChMn", &BAR_AP_MAPS),
        e(b"ChMh", &BAR_AP_HITS),
        e(b"ChUn", &BAR_AP_UNMAPS),
        e(b"ChMr", &BAR_AP_LAST_RESID),
        e(b"ChMo", &BAR_AP_LAST_PAGE),
        f(b"ChEi", &BAR_AP_ERR_IRQL),
        f(b"ChEa", &BAR_AP_ERR_ALLOC),
        f(b"ChEp", &BAR_AP_ERR_PARTIAL),
        f(b"ChEs", &BAR_AP_ERR_SPARSE),
        f(b"ChEb", &BAR_AP_ERR_BOUNDS),
        f(b"ChEm", &BAR_AP_ERR_MAP),
        f(b"ChEu", &BAR_AP_ERR_UNRESOLVED_UNMAP),
        e(b"ChMc", &CPU_HOST_MAP_COUNT),
        e(b"ChUc", &CPU_HOST_UNMAP_COUNT),
        e(b"ChIq", &BAR_AP_LAST_IRQL),
        e(b"ChIa", &BAR_AP_IRQL_ACK),
        e(b"ChId", &BAR_AP_IRQL_DEFER),
        f(b"ChSzMm", &BAR_AP_SIZE_MISMATCH),
        e(b"ChSzDl", &BAR_AP_SIZE_DELTA),
        f(b"ChSzPv", &BAR_AP_SIZE_PROVENANCE),
        // R718's measurement. It lives in this block rather than one of its own
        // because it is an APERTURE fact — "how many CpuVisible BAR allocations
        // shipped with no aperture segment to fall back to" — and because this
        // block already has a PASSIVE flush site on the mode-set path. Marked a
        // FAILURE entry so a change forces an immediate flush: on the production
        // DisplayHalf=1 configuration it must read 0, and any movement means the
        // v71/v72 VidPn-commit rejection shape is being emitted again.
        f(
            b"ApMiss",
            &crate::ddi::create_allocation::APERTURE_MISSING_CPU_VISIBLE,
        ),
        // R719's measurement: how often the empirical linear-blob size guess
        // disagreed with the exact Vulkan requirement. A VALUE entry, not a
        // failure — the guess being off is information, not a fault.
        e(
            b"BlbSzD",
            &crate::ddi::create_allocation::LINEAR_BLOB_SIZE_DIVERGENCE,
        ),
    ],
    ticks: &AP_FLUSH_TICKS,
    failures: &AP_FLUSH_FAILURES,
    policy: crate::diag::FlushPolicy::EveryNth(64),
};

fn dump_bar_ap_counters() {
    AP_COUNTERS.flush();
}

/// PASSIVE-only flush of the CPU-host-aperture counters into the registry ring.
/// The raised-IRQL Map path (below) can record its atomics but cannot touch the
/// registry, so a PASSIVE DDI on the mode-set path (`DxgkDdiCommitVidPn`) calls
/// this to surface `ChIq`/`ChEi`/`ChMc` — proving whether `MapCpuHostAperture` is
/// driven at DISPATCH during display activation.
pub fn diag_dump_cpu_host_atomics() {
    dump_bar_ap_counters();
}

use crate::ddi::PASSIVE_LEVEL_IRQL;

/// An aperture request proven whole-allocation, consecutive and in-bounds.
///
/// Constructible only by [`validate_aperture_request`], and both things that can
/// act on a request — establishing the mapping and acknowledging an existing one
/// — take it. A future path therefore cannot answer an unvalidated request,
/// because it cannot produce the token (k-alloc-13).
#[derive(Clone, Copy)]
struct ValidatedApertureRange {
    offset: u64,
    #[allow(dead_code)]
    pages: u64,
}

/// Why an aperture request was refused. Carries no status: the two callers run
/// at different IRQLs and answer differently.
#[derive(Clone, Copy)]
enum ApertureRefusal {
    /// Zero pages, or a null page array.
    Partial,
    /// Not a whole-allocation map.
    NotWholeAllocation,
    /// The aperture pages are not one consecutive run.
    Sparse,
    /// The range leaves the declared window region.
    Bounds,
}

impl ApertureRefusal {
    /// Bump the matching counter. Atomics only — DISPATCH-safe by construction,
    /// which is what lets the raised-IRQL caller report a refusal at all.
    fn count(self, resource_id: Option<u32>) {
        match self {
            ApertureRefusal::Partial | ApertureRefusal::NotWholeAllocation => {
                BAR_AP_ERR_PARTIAL.fetch_add(1, Ordering::Relaxed);
            }
            ApertureRefusal::Sparse => {
                BAR_AP_ERR_SPARSE.fetch_add(1, Ordering::Relaxed);
            }
            ApertureRefusal::Bounds => {
                BAR_AP_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Some(res) = resource_id {
            BAR_AP_LAST_RESID.store(res, Ordering::Relaxed);
        }
    }
}

/// Cross-check the allocation's own size against the tracked blob's, counting
/// only.
///
/// Atomics only, so it is safe on the DISPATCH arm too — the registry mirror
/// happens from the PASSIVE `dump` through [`AP_COUNTERS`], never here.
/// `blob_lookup` runs under the device spinlock, which this path already takes
/// for `blob_resid_at_offset`.
fn note_size_provenance(adapter: &AdapterContext, alloc: &PagingAllocInfo) {
    // The invariant this item makes explicit rather than incidental: only a
    // host-authoritative size may become BAR-eligible. Counted, not asserted —
    // a bugcheck here would be worse than the refusal that follows anyway.
    if alloc.bar_eligible && !alloc.size_provenance.is_host_authoritative() {
        BAR_AP_SIZE_PROVENANCE.fetch_add(1, Ordering::Relaxed);
    }
    let Ok(Some((_, tracked, _))) = adapter.with_virtio(|v| v.blob_lookup(alloc.resource_id))
    else {
        return;
    };
    let from_alloc = (alloc.size.saturating_add(4095) >> 12).max(1);
    let from_blob = (tracked.saturating_add(4095) >> 12).max(1);
    if from_alloc != from_blob {
        BAR_AP_SIZE_MISMATCH.fetch_add(1, Ordering::Relaxed);
        BAR_AP_SIZE_DELTA.store(from_alloc.abs_diff(from_blob) as u32, Ordering::Relaxed);
    }
}

/// The ONE aperture-request validation rule, for both IRQL paths.
///
/// This used to exist twice, and the raised-IRQL copy was incomplete: it checked
/// the page count and then compared `blob_resid_at_offset(page0 << 12)` against
/// the allocation, but never asked whether the remaining pages were consecutive.
/// A request of `[P, P+7, P+8, ...]` whose first page matched the offset the blob
/// was already mapped at was ACKNOWLEDGED with STATUS_SUCCESS, telling dxgkrnl
/// the whole range was backed while the CPU view for pages 1..n-1 addressed
/// foreign window offsets. The identical request on the PASSIVE path was refused.
///
/// # Safety
/// `pCpuHostAperturePages` must hold `NumberOfPages` entries for the call.
/// Reading all of them at DISPATCH is sound for the same reason reading element
/// 0 there already was: the array is dxgkrnl-owned and must be non-paged either
/// way, since this DDI is documented to be callable above PASSIVE.
unsafe fn validate_aperture_request(
    args: &DXGKARG_MAPCPUHOSTAPERTURE,
    alloc: &crate::ddi::create_allocation::PagingAllocInfo,
    bar: &crate::adapter::BarSegment,
) -> Result<ValidatedApertureRange, ApertureRefusal> {
    let n = args.NumberOfPages;
    if n == 0 || args.pCpuHostAperturePages.is_null() {
        return Err(ApertureRefusal::Partial);
    }
    // Whole-allocation only: a blob maps at one offset, whole-blob — a partial
    // map would spill blob pages over neighboring aperture assignments.
    let blob_pages = (alloc.size.saturating_add(4095) >> 12).max(1);
    if n != blob_pages {
        return Err(ApertureRefusal::NotWholeAllocation);
    }
    // SAFETY: n entries exist per the fn contract.
    let page0 = unsafe { core::ptr::read_unaligned(args.pCpuHostAperturePages) } as u64;
    for i in 1..n {
        // SAFETY: as above, i < n.
        let p =
            unsafe { core::ptr::read_unaligned(args.pCpuHostAperturePages.add(i as usize)) } as u64;
        if p != page0 + i {
            return Err(ApertureRefusal::Sparse);
        }
    }
    let offset = page0 << 12;
    if offset.saturating_add(n << 12) > bar.size {
        return Err(ApertureRefusal::Bounds);
    }
    Ok(ValidatedApertureRange { offset, pages: n })
}

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

    // SAFETY: dxgkrnl hands back our AdapterContext as the miniport context.
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let bar_active = adapter
        .bar_segment()
        .filter(|b| b.seg_id == args.SegmentId as u32);
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
        // Same validator as the PASSIVE arm — all four checks, not two — and
        // only then the "already mapped at this offset?" question on top of a
        // fully validated range. REFUSAL HANDLING IS SPLIT BY IRQL: this arm may
        // only bump atomics and defer; it must NEVER call dump_bar_ap_counters,
        // because RtlWriteRegistryValue above PASSIVE is the same invariant
        // violation R307 removed from the paging path.
        let already_mapped = unsafe { paging_alloc_info(args.hAllocation) }
            .filter(|a| a.bar_eligible)
            .inspect(|a| note_size_provenance(adapter, a))
            .and_then(|a| {
                // SAFETY: dxgkrnl owns the page array for the call; see the
                // validator's safety note on reading it at DISPATCH.
                match unsafe { validate_aperture_request(args, &a, bar) } {
                    Ok(range) => Some((a, range)),
                    Err(refusal) => {
                        refusal.count(Some(a.resource_id));
                        None
                    }
                }
            })
            .map(|(a, range)| {
                adapter
                    .with_virtio(|v| v.blob_resid_at_offset(range.offset))
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
    // SAFETY: this DDI is documented PASSIVE but ARRIVES AT DISPATCH in practice
    // (ETW-proven, v71), which is exactly why the gate above exists. The mint
    // belongs HERE, on the PASSIVE side of it, and must never move above the
    // gate: the DISPATCH arm may only bump atomics and defer with
    // STATUS_NO_MEMORY, and it holds no token so it cannot call `map_blob_at`
    // even by accident. That refusal is the reason `assume()` counts rather than
    // returning a Result — STATUS_UNSUCCESSFUL here costs the whole VidPn.
    let passive = unsafe { crate::irql::PassiveLevel::assume() };
    let Some(alloc) = (unsafe { paging_alloc_info(args.hAllocation) }) else {
        BAR_AP_ERR_ALLOC.fetch_add(1, Ordering::Relaxed);
        dump_bar_ap_counters();
        return STATUS_NO_MEMORY;
    };
    note_size_provenance(adapter, &alloc);
    if !alloc.bar_eligible {
        // The allocation contract says this object is device-local/opaque.
        // Never reinterpret it as CPU-addressable blob bytes even if VidMm
        // supplies a BAR-segment aperture request.
        BAR_AP_ERR_ALLOC.fetch_add(1, Ordering::Relaxed);
        BAR_AP_LAST_RESID.store(alloc.resource_id, Ordering::Relaxed);
        dump_bar_ap_counters();
        return STATUS_NO_MEMORY;
    }
    // SAFETY: dxgkrnl owns `pCpuHostAperturePages` for the duration of the call.
    let range = match unsafe { validate_aperture_request(args, &alloc, bar) } {
        Ok(range) => range,
        Err(refusal) => {
            refusal.count(Some(alloc.resource_id));
            // Only the PASSIVE caller flushes.
            dump_bar_ap_counters();
            return STATUS_NO_MEMORY;
        }
    };

    match crate::virtio::ctrl::map_blob_at(passive, adapter, alloc.resource_id, range.offset) {
        Ok(_prep) => {
            BAR_AP_MAPS.fetch_add(1, Ordering::Relaxed);
            BAR_AP_LAST_RESID.store(alloc.resource_id, Ordering::Relaxed);
            BAR_AP_LAST_PAGE.store((range.offset >> 12) as u32, Ordering::Relaxed);
            // No flush on the SUCCESS path: the atomics carry the values, the
            // refusal paths and the PASSIVE mode-set hook flush them, and this
            // is the site that made a successful aperture map cost 17 registry
            // writes (R317 / k-alloc-05).
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

    // Tear down the blob mapping backing this aperture range so dxgkrnl can
    // re-book the pages without overlapping host window subregions. No
    // hAllocation in this DDI — resolve by aperture offset.
    // SAFETY: our AdapterContext (DDI contract).
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let is_bar = adapter
        .bar_segment()
        .as_ref()
        .map_or(false, |b| b.seg_id == args.SegmentId as u32);
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
    // SAFETY: as in the map DDI above — downstream of a runtime check, not of
    // the documented annotation. The non-PASSIVE arm leaves the mapping in place
    // (self-healed by the next `map_blob_at`) and holds no token.
    let passive = unsafe { crate::irql::PassiveLevel::assume() };
    let resid = adapter
        .with_virtio(|v| v.blob_resid_at_offset(offset))
        .unwrap_or(None);
    match resid {
        Some(res) => {
            let _ = crate::virtio::ctrl::resource_unmap_blob(passive, adapter, res);
            let _ = adapter.with_virtio(|v| v.blob_note_unmapped(res));
            BAR_AP_UNMAPS.fetch_add(1, Ordering::Relaxed);
        }
        None => {
            // Nothing mapped there (already torn down at DestroyAllocation, or
            // a map this driver refused). Counted for visibility.
            BAR_AP_ERR_UNRESOLVED_UNMAP.fetch_add(1, Ordering::Relaxed);
            // A refusal: flush so it surfaces on the operation that produced it.
            dump_bar_ap_counters();
        }
    }
    STATUS_SUCCESS
}
