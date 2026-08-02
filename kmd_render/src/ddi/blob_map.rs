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
use wdk_sys::ntddk::{IoAllocateMdl, IoFreeMdl, MmBuildMdlForNonPagedPool, MmUnmapLockedPages};
use wdk_sys::{_MEMORY_CACHING_TYPE, MDL, PMDL, PVOID, ULONG};

extern "C" {
    /// C SEH shim (`src/seh_shim.c`, compiled by build.rs): wraps
    /// `MmMapLockedPagesSpecifyCache(mdl, UserMode, cache, NULL, FALSE, priority)`
    /// in `__try/__except`. For UserMode the kernel RAISES on failure instead of
    /// returning NULL; without SEH that raise is a bugcheck reachable from any
    /// process via the Escape MAP_BLOB verb. The shim converts it to NULL.
    fn helios_mm_map_locked_pages_user_seh(
        mdl: PMDL,
        access_mode: i8,
        cache_type: i32,
        priority: u32,
    ) -> PVOID;
}

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
/// `MdlMappingNoWrite` — map the user view READ-ONLY. The D4a read-ledger page
/// is KMD-written truth a process only observes; a writable view would let any
/// mapper forge `retired` bumps and unwedge/rewedge other processes' waits.
const MDL_MAPPING_NO_WRITE: u32 = 0x8000_0000;
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
/// The UserMode map goes through the C SEH shim (`helios_mm_map_locked_pages_user_seh`):
/// `MmMapLockedPagesSpecifyCache` RAISES on failure for UserMode rather than
/// returning NULL, and a raise in this no_std crate is a bugcheck reachable from
/// any process via D3DKMTEscape. The shim's `__except` turns it into NULL.
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
    // process. The shim catches the UserMode failure raise and returns NULL.
    let va = helios_mm_map_locked_pages_user_seh(mdl, USER_MODE, cache as i32, priority);
    if va.is_null() {
        // Map failed (raise caught by the SEH shim, or NULL from the kernel).
        IoFreeMdl(mdl);
        return None;
    }
    Some((va as u64, mdl))
}

/// Map a KMD-owned NONPAGED page (the D4a read ledger,
/// `MmAllocateContiguousMemory`-backed) into the CURRENT process, READ-ONLY.
/// Returns `(user_va, mdl)`.
///
/// The sibling of [`map_io_pages_to_user`] for real RAM: these pages HAVE PFN
/// database entries, so the MDL comes from `MmBuildMdlForNonPagedPool` (which
/// fills the PFN array and marks the MDL nonpaged-source) rather than from the
/// hand-populated `MDL_IO_SPACE` path — `MDL_IO_SPACE` exists precisely to
/// bypass the PFN database, which for RAM would skip the mapping bookkeeping
/// that IS present. Cached, because the KMD writes the page with ordinary
/// cached stores; `MDL_MAPPING_NO_WRITE` because the page is KMD-written truth
/// (see the constant's doc).
///
/// # Safety
/// PASSIVE_LEVEL, in the target process's context, holding no spinlock. `va`
/// must be a page-aligned nonpaged kernel allocation of at least `size` bytes
/// (`size` page-aligned, <= `u32::MAX`) that outlives the mapping — the D4a
/// page is freed only in `AdapterContext::drop`, after every device (and with
/// it every mapping) is gone. Teardown pairs with [`unmap_io_pages_from_user`].
pub unsafe fn map_nonpaged_page_to_user_readonly(va: *mut u8, size: u64) -> Option<(u64, PMDL)> {
    // SAFETY: a real VirtualAddress + Length; the MDL header describes `va`.
    let mdl = IoAllocateMdl(
        va as PVOID,
        size as ULONG,
        0, // SecondaryBuffer = FALSE
        0, // ChargeQuota = FALSE
        core::ptr::null_mut(),
    );
    if mdl.is_null() {
        return None;
    }
    // SAFETY: `va` is nonpaged system memory per this function's contract, so
    // the routine can resolve every PFN without faulting.
    MmBuildMdlForNonPagedPool(mdl);
    let priority = NORMAL_PAGE_PRIORITY | MDL_MAPPING_NO_EXECUTE | MDL_MAPPING_NO_WRITE;
    // SAFETY: valid populated MDL; UserMode map into the current process. The
    // SEH shim converts the UserMode failure raise into NULL (same trap as the
    // blob path: an unshimmed raise here is a bugcheck reachable from any
    // process via D3DKMTEscape).
    let user_va = helios_mm_map_locked_pages_user_seh(
        mdl,
        USER_MODE,
        _MEMORY_CACHING_TYPE::MmCached as i32,
        priority,
    );
    if user_va.is_null() {
        IoFreeMdl(mdl);
        return None;
    }
    Some((user_va as u64, mdl))
}

/// Unmap a user-space mapping made by [`map_io_pages_to_user`] or
/// [`map_nonpaged_page_to_user_readonly`] and free its MDL.
///
/// # Safety
/// Must run at PASSIVE_LEVEL in the SAME process the mapping was created in;
/// `user_va`/`mdl` must be a pair returned by one of the two mappers, not yet
/// unmapped.
pub unsafe fn unmap_io_pages_from_user(user_va: u64, mdl: PMDL) {
    MmUnmapLockedPages(user_va as PVOID, mdl);
    IoFreeMdl(mdl);
}
