//! Host-visible blob → user-VA mapping primitives (Gate 5a Stage 2b).
//!
//! The venus ICD maps a HOST3D blob into its address space over `DxgkDdiEscape`
//! (`HELIOS_ESCAPE_MAP_BLOB`). The KMD picks a window offset and issues
//! `RESOURCE_MAP_BLOB` (under the virtio lock, DISPATCH_LEVEL), then maps the
//! resulting host-visible BAR pages into the CALLING process here at
//! PASSIVE_LEVEL — the zero-copy BAR model, no WDDM memory segment / GpuMmu.
//!
//! Ported 1:1 from the proven System-class `kmd/src/ioctl.rs` (the `Z:\` driver
//! that ran the full venus stack). The only change is the entry point: an Escape
//! verb in the caller's process context instead of an IOCTL on a device handle.

use core::mem::size_of;

use helios_protocol::{
    VIRTIO_GPU_MAP_CACHE_CACHED, VIRTIO_GPU_MAP_CACHE_UNCACHED, VIRTIO_GPU_MAP_CACHE_WC,
};
use wdk_sys::ntddk::{
    IoAllocateMdl, IoFreeMdl, MmMapLockedPagesSpecifyCache, MmUnmapLockedPages,
};
use wdk_sys::{MDL, PMDL, PVOID, ULONG, _MEMORY_CACHING_TYPE};

/// log2(page size). The host-visible window is mapped page-granular.
const PAGE_SHIFT: u32 = 12;
/// `MDL_PAGES_LOCKED` — set on a manually-built MDL describing device (BAR) pages,
/// which are not pageable, so `MmMapLockedPagesSpecifyCache` treats them as locked.
const MDL_PAGES_LOCKED: i16 = 0x0002;
/// `MDL_IO_SPACE` — the described frames are PCI-BAR (host-visible window) pages
/// that live in high MMIO ABOVE guest RAM and have NO entry in the guest PFN
/// database. Without this flag `MmMapLockedPagesSpecifyCache` would index
/// `MmPfnDatabase[pfn]` → a wild out-of-range access. The flag tells MM to bypass
/// the PFN database and build the user PTEs straight from our PFN array.
const MDL_IO_SPACE: i16 = 0x0800;
/// `NormalPagePriority` (`MM_PAGE_PRIORITY`) for `MmMapLockedPagesSpecifyCache`.
const NORMAL_PAGE_PRIORITY: u32 = 16;
/// `MdlMappingNoExecute` — map the user view non-executable (data-only blob).
const MDL_MAPPING_NO_EXECUTE: u32 = 0x4000_0000;
/// `UserMode` (`KPROCESSOR_MODE`) — map into the calling user process.
const USER_MODE: i8 = 1;

/// Translate a virtio-gpu `map_info` caching nibble to a Windows cache type.
pub fn map_cache_to_mm(map_cache: u32) -> _MEMORY_CACHING_TYPE::Type {
    match map_cache {
        VIRTIO_GPU_MAP_CACHE_CACHED => _MEMORY_CACHING_TYPE::MmCached,
        VIRTIO_GPU_MAP_CACHE_WC => _MEMORY_CACHING_TYPE::MmWriteCombined,
        VIRTIO_GPU_MAP_CACHE_UNCACHED => _MEMORY_CACHING_TYPE::MmNonCached,
        _ => _MEMORY_CACHING_TYPE::MmNonCached,
    }
}

/// Pick the effective cache type: honor the ICD's request if valid, else the host's.
pub fn effective_map_cache(requested: u32, host: u32) -> u32 {
    match requested {
        VIRTIO_GPU_MAP_CACHE_CACHED | VIRTIO_GPU_MAP_CACHE_WC | VIRTIO_GPU_MAP_CACHE_UNCACHED => {
            requested
        }
        _ => host,
    }
}

/// Build an MDL over the page-aligned guest-physical range `[gpa, gpa + size)` (a
/// span of the host-visible BAR — device I/O pages, NOT pageable RAM) and map it
/// into the CURRENT process's user address space. Returns `(user_va, mdl)`.
///
/// # Safety
/// Must be called at PASSIVE_LEVEL in the target process's context, holding no
/// spinlock. `gpa`/`size` must be page-aligned and name a valid host-injected
/// window range (from `RESOURCE_MAP_BLOB`). The PFNs are device pages not subject
/// to paging, so `MDL_PAGES_LOCKED` is the correct flag (no `MmProbeAndLockPages`).
///
/// ⚠️ HARDENING TODO (carried from the System-class driver): for UserMode,
/// `MmMapLockedPagesSpecifyCache` RAISES on failure rather than returning NULL,
/// and this `no_std` crate has no SEH, so a failure unwinds → bugcheck. Exposure is
/// bounded by the per-map size cap and the sole caller being the trusted ICD. Add a
/// C `__try/__except` shim before exposing MAP_BLOB to any untrusted caller.
pub unsafe fn map_io_pages_to_user(
    gpa: u64,
    size: u64,
    cache: _MEMORY_CACHING_TYPE::Type,
) -> Option<(u64, PMDL)> {
    // SAFETY: VirtualAddress = NULL is valid for a manually-populated MDL; Length is
    // page-aligned so the PFN-array span is exactly `size >> PAGE_SHIFT`.
    let mdl = IoAllocateMdl(
        core::ptr::null_mut(),
        size as ULONG,
        0, // SecondaryBuffer = FALSE
        0, // ChargeQuota = FALSE
        core::ptr::null_mut(),
    );
    if mdl.is_null() {
        return None;
    }
    // The (device BAR) pages are inherently locked/non-pageable and live in I/O
    // space (no PFN-database entry) — both flags are required, see their docs.
    (*mdl).MdlFlags |= MDL_PAGES_LOCKED | MDL_IO_SPACE;
    // The PFN array immediately follows the MDL header.
    let pfns = (mdl as *mut u8).add(size_of::<MDL>()) as *mut u64;
    let pages = (size >> PAGE_SHIFT) as usize;
    let pfn0 = gpa >> PAGE_SHIFT;
    for i in 0..pages {
        // SAFETY: `pfns[0..pages]` is the freshly-allocated PFN array.
        *pfns.add(i) = pfn0 + i as u64;
    }
    let priority = NORMAL_PAGE_PRIORITY | MDL_MAPPING_NO_EXECUTE;
    // SAFETY: `mdl` is a valid, populated, locked MDL; maps into the current (user)
    // process. BugCheckOnFailure = FALSE (ignored for UserMode — see the note above).
    let va =
        MmMapLockedPagesSpecifyCache(mdl, USER_MODE, cache, core::ptr::null_mut(), 0, priority);
    if va.is_null() {
        // Unreached for UserMode (it raises rather than returning NULL), kept as
        // defense-in-depth in case a future kernel build returns NULL.
        IoFreeMdl(mdl);
        return None;
    }
    Some((va as u64, mdl))
}

/// Unmap a user-space blob mapping made by [`map_io_pages_to_user`] and free its MDL.
///
/// # Safety
/// Must run at PASSIVE_LEVEL in the SAME process the mapping was created in;
/// `user_va`/`mdl` must be a pair returned by `map_io_pages_to_user`, not yet unmapped.
pub unsafe fn unmap_io_pages_from_user(user_va: u64, mdl: PMDL) {
    MmUnmapLockedPages(user_va as PVOID, mdl);
    IoFreeMdl(mdl);
}
