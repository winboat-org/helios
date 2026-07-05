//! Allocation management DDIs (Gate 5a Stage 2).
//!
//! `DxgkDdiCreateAllocation` reads the ICD's `HeliosWddmAllocPrivate` (passed via
//! `D3DKMTCreateAllocation` per-allocation private driver data) and creates the
//! backing virtio-gpu HOST3D blob (`resource_create_blob` = create_blob +
//! ctx_attach), recording the `resource_id` so later stages can map it into the
//! host-visible window (`DxgkDdiBuildPagingBuffer`, Stage 2b) and tear it down
//! (`DxgkDdiDestroyAllocation`). See `GATE5_STAGE2_ALLOC_DESIGN.md`.
//!
//! TRUST BOUNDARY: `pPrivateDriverData` is ICD-supplied. `PrivateDriverDataSize`
//! is the only authoritative length; we bounds-check it against the struct size
//! before reading, validate the magic/version, and read with `pod_read_unaligned`.

use alloc::boxed::Box;
use core::ffi::c_void;
use core::mem::size_of;
use core::sync::atomic::{AtomicU32, Ordering};

use bytemuck::{bytes_of, pod_read_unaligned, Zeroable};
use helios_protocol::{
    HeliosWddmAllocMeta, HeliosWddmAllocPrivate, HeliosWddmOpenIdentity,
    HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY, HELIOS_WDDM_ALLOC_KIND_STANDARD,
    VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE, VIRTIO_GPU_BLOB_MEM_HOST3D, VIRTIO_GPU_MAP_CACHE_CACHED,
};

use crate::adapter::AdapterContext;
use crate::dxgk::_D3DDDIFORMAT::D3DDDIFMT_A8R8G8B8;
use crate::dxgk::_D3DKMDT_STANDARDALLOCATION_TYPE::{
    D3DKMDT_STANDARDALLOCATION_GDISURFACE, D3DKMDT_STANDARDALLOCATION_SHADOWSURFACE,
    D3DKMDT_STANDARDALLOCATION_SHAREDPRIMARYSURFACE, D3DKMDT_STANDARDALLOCATION_STAGINGSURFACE,
};
use crate::dxgk::*;

/// `AllocationContext::magic` — validates `hAllocation` casts in paging DDIs
/// (a garbage dereference in BuildPagingBuffer is a bugcheck).
const ALLOCATION_CTX_MAGIC: u32 = 0x4841_4C43; // "HALC"

/// Sentinel for [`AllocationContext::bar_placed`]: not placed in the BAR segment.
pub(crate) const BAR_UNPLACED: u64 = u64::MAX;

/// Per-allocation KMD state: the venus context + virtio resource backing it, plus
/// the host-visible window mapping (filled in Stage 2b by BuildPagingBuffer).
struct AllocationContext {
    /// [`ALLOCATION_CTX_MAGIC`] — must be the FIRST field (paging-DDI cast check).
    magic: u32,
    ctx_id: u32,
    resource_id: u32,
    owns_resource: bool,
    blob_id: u64,
    /// Nonzero for KMD-backed standard allocations: the kernel venus client's
    /// `VkDeviceMemory` object id behind the blob, freed (`vkFreeMemory`) at
    /// DestroyAllocation after the resource unref.
    venus_memory_id: u64,
    size: SIZE_T,
    /// Host-visible window byte offset this blob is mapped at (Stage 2b).
    map_offset: u64,
    /// Page-rounded mapped length (Stage 2b).
    map_len: u64,
    /// `true` once `RESOURCE_MAP_BLOB` has succeeded (so destroy unmaps first).
    mapped: bool,
    /// Surface geometry for `DxgkDdiDescribeAllocation` (0 for UMD blob allocations
    /// that carry no dimensions). Populated from the standard-allocation trailer.
    width: u32,
    height: u32,
    format: u32, // D3DDDIFORMAT
    /// C1 identity: the creator's exact `vkAllocateMemory` size + memory type
    /// (what a cross-process opener must import with). Diagnostic copies — the
    /// authoritative record travels in the private-data trailer / open identity.
    venus_alloc_size: u64,
    memory_type_index: u32,
    /// VidMm-assigned SegmentAddress in the CPU-visible BAR segment (id 3), or
    /// [`BAR_UNPLACED`]. Written by `BuildPagingBuffer` when it maps the blob at
    /// the assigned offset; atomic because paging DDIs run concurrently with
    /// allocation DDIs. Only meaningful for `bar_eligible` allocations.
    bar_placed: core::sync::atomic::AtomicU64,
    /// This allocation was reported to VidMm as BAR-segment-only (KMD-backed
    /// standard allocation with a mappable venus blob, BAR segment active).
    bar_eligible: bool,
}

/// Per-resource KMD state. Dxgkrnl requires a non-null KMD resource handle for
/// `Flags.Resource` CreateAllocation calls (not just per-allocation handles);
/// the handle is opaque to us until DestroyAllocation carries it back.
struct ResourceContext {
    _marker: u32,
}

/// Per-device open state for an allocation. Dxgkrnl's `hAllocation` in
/// `DXGK_OPENALLOCATIONINFO` is its non-device-specific allocation handle; the
/// miniport must return its own device-specific handle here and later receives it
/// in command allocation lists / CloseAllocation.
struct OpenAllocationContext {
    allocation: D3DKMT_HANDLE,
    private_size: u32,
    /// Venus resource id (from `HeliosWddmAllocPrivate._pad`) + geometry, captured
    /// from the open-time private data so the Present blit can resolve the
    /// composition source / IddCx destination surfaces by `hDeviceSpecificAllocation`
    /// (dxgkrnl gives Present only this device-specific handle, not `hAllocation`).
    resource_id: u32,
    width: u32,
    height: u32,
    format: u32,
}

/// Surface identity + geometry for a Present allocation-list entry, resolved from
/// its `hDeviceSpecificAllocation` ([`present_alloc_info`]).
#[derive(Clone, Copy)]
pub struct PresentAllocInfo {
    pub resource_id: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
}

/// Resolve a Present allocation-list entry's `hDeviceSpecificAllocation` (an
/// [`OpenAllocationContext`] we returned from `DxgkDdiOpenAllocation`) to the
/// backing venus resource id + geometry. Returns `None` for a null handle.
///
/// SAFETY: `h` must be an `hDeviceSpecificAllocation` value the KMD returned from
/// `DxgkDdiOpenAllocation` (dxgkrnl round-trips it unmodified in command/present
/// allocation lists) and still open (not yet `CloseAllocation`-freed).
pub unsafe fn present_alloc_info(h: HANDLE) -> Option<PresentAllocInfo> {
    if h.is_null() {
        return None;
    }
    let open = unsafe { &*(h as *const OpenAllocationContext) };
    if open.resource_id == 0 {
        return None;
    }
    Some(PresentAllocInfo {
        resource_id: open.resource_id,
        width: open.width,
        height: open.height,
        format: open.format,
    })
}

/// Snapshot of the [`AllocationContext`] fields `BuildPagingBuffer` needs to
/// service content/placement ops against the CPU-visible BAR segment.
#[derive(Clone, Copy)]
pub(crate) struct PagingAllocInfo {
    pub resource_id: u32,
    pub size: u64,
    pub bar_eligible: bool,
    /// Current placement ([`BAR_UNPLACED`] if none).
    pub bar_placed: u64,
}

/// Resolve a paging-op `hAllocation` (the handle this driver returned from
/// `DxgkDdiCreateAllocation`) to its paging view. Returns `None` for null or
/// magic-mismatched handles — a garbage dereference here would bugcheck.
///
/// SAFETY: `h` must be an in-flight paging op's `hAllocation` (dxgkrnl keeps
/// the allocation alive across its paging operations).
pub(crate) unsafe fn paging_alloc_info(h: HANDLE) -> Option<PagingAllocInfo> {
    if h.is_null() {
        return None;
    }
    let ctx = unsafe { &*(h as *const AllocationContext) };
    if ctx.magic != ALLOCATION_CTX_MAGIC {
        return None;
    }
    Some(PagingAllocInfo {
        resource_id: ctx.resource_id,
        size: ctx.size as u64,
        bar_eligible: ctx.bar_eligible,
        bar_placed: ctx.bar_placed.load(Ordering::Acquire),
    })
}

/// Record (or clear, with [`BAR_UNPLACED`]) an allocation's VidMm-assigned BAR
/// SegmentAddress. SAFETY: same contract as [`paging_alloc_info`].
pub(crate) unsafe fn set_bar_placement(h: HANDLE, offset: u64) {
    if h.is_null() {
        return;
    }
    let ctx = unsafe { &*(h as *const AllocationContext) };
    if ctx.magic == ALLOCATION_CTX_MAGIC {
        ctx.bar_placed.store(offset, Ordering::Release);
    }
}

const PAGE: SIZE_T = 4096;
const D3DDDI_ALLOCATIONPRIORITY_NORMAL: UINT = 0x7800_0000;

/// Cross-adapter row-major textures require a 256-byte row-pitch alignment
/// (`D3D12_TEXTURE_DATA_PITCH_ALIGNMENT`). The IddCx composition surface is a
/// cross-adapter resource (created as a standard allocation, opened on the Helios
/// render side — `rendering-on-a-discrete-gpu-using-cross-adapter-resources.md`),
/// so its backing must be linear with this pitch for the IndirectKMD adapter to
/// open the same surface. PATH-A (2026-06-22).
const CROSS_ADAPTER_PITCH_ALIGN: u32 = 256;

fn round_up_page(n: SIZE_T) -> SIZE_T {
    n.saturating_add(PAGE - 1) & !(PAGE - 1)
}

/// 32-bpp linear row pitch aligned to the cross-adapter requirement.
pub(crate) fn cross_adapter_pitch(width: u32) -> u32 {
    let raw = width.saturating_mul(4);
    raw.saturating_add(CROSS_ADAPTER_PITCH_ALIGN - 1) & !(CROSS_ADAPTER_PITCH_ALIGN - 1)
}

/// Cycling 8-slot fixed-name registry ring of allocation create/open events, so a
/// single boot's surface map (venus resid + geometry + ctx, create vs open) is
/// readable live — used to correlate DWM's composition surfaces (1952x1088,
/// res_id 52/54) against the IDD's IddCx swapchain surface (1920x1080): same
/// venus resid ⇒ shared (sync problem), different ⇒ the surfaces never alias and
/// the composed pixels are never copied into what the IDD reads.
static ALLOC_EVENT_SEQ: AtomicU32 = AtomicU32::new(0);

fn record_alloc_event(resid: u32, width: u32, height: u32, ctx_id: u32, is_open: bool) {
    let i = (ALLOC_EVENT_SEQ.fetch_add(1, Ordering::Relaxed) % 8) as u8;
    let d = b'0' + i;
    crate::diag::record_named_bytes(&[b'A', b'E', d, b'r'], resid);
    crate::diag::record_named_bytes(&[b'A', b'E', d, b'd'], (width << 16) | (height & 0xFFFF));
    crate::diag::record_named_bytes(
        &[b'A', b'E', d, b'c'],
        (ctx_id & 0x7FFF_FFFF) | ((is_open as u32) << 31),
    );
}

unsafe fn read_standard_meta(
    private: *const c_void,
    private_size: UINT,
) -> Option<HeliosWddmAllocMeta> {
    // Legacy 24-byte trailer (geometry + bind/misc, no venus identity fields):
    // its layout is exactly the first 24 bytes of HeliosWddmAllocMeta, so a
    // short trailer parses into a zero-extended meta. Allocations created by a
    // pre-identity driver instance can still be opened after a component
    // update without a reboot.
    const LEGACY_META_SIZE: usize = 24;
    let base = size_of::<HeliosWddmAllocPrivate>();
    if private.is_null() || (private_size as usize) < base + LEGACY_META_SIZE {
        return None;
    }
    let have = ((private_size as usize) - base).min(size_of::<HeliosWddmAllocMeta>());
    let mut raw = [0u8; size_of::<HeliosWddmAllocMeta>()];
    // SAFETY: bounds-checked; `have` bytes of trailer exist past the 48-byte prefix.
    unsafe {
        core::ptr::copy_nonoverlapping((private as *const u8).add(base), raw.as_mut_ptr(), have);
    }
    Some(pod_read_unaligned(&raw))
}

/// Identity summary parsed from an allocation's private driver data at
/// OpenAllocation time. Sourced from either layout the buffer may hold:
/// the creator's [`HeliosWddmAllocPrivate`] (with the create-time `_pad`
/// resid write-back), or a [`HeliosWddmOpenIdentity`] a previous open of the
/// same allocation already wrote (dxgkrnl keeps ONE per-allocation buffer, so
/// KMD mutations persist across opens — proven by the old `_pad` patching).
#[derive(Clone, Copy)]
struct ParsedAllocIdentity {
    resource_id: u32,
    blob_size: u64,
    venus_alloc_size: u64,
    memory_type_index: u32,
    ctx_id: u32,
    kind: u32,
}

unsafe fn read_alloc_identity(
    private: *const c_void,
    private_size: UINT,
) -> Option<ParsedAllocIdentity> {
    if private.is_null() || (private_size as usize) < size_of::<HeliosWddmAllocPrivate>() {
        return None;
    }
    // SAFETY: bounds-checked above; both candidate layouts are exactly 48 bytes.
    let bytes = unsafe {
        core::slice::from_raw_parts(private as *const u8, size_of::<HeliosWddmAllocPrivate>())
    };
    let ident: HeliosWddmOpenIdentity = pod_read_unaligned(bytes);
    if ident.is_valid() && ident.resource_id != 0 {
        return Some(ParsedAllocIdentity {
            resource_id: ident.resource_id,
            blob_size: ident.blob_size,
            venus_alloc_size: ident.venus_alloc_size,
            memory_type_index: ident.memory_type_index,
            ctx_id: ident.ctx_id,
            kind: ident.kind,
        });
    }
    let ap: HeliosWddmAllocPrivate = pod_read_unaligned(bytes);
    if ap.is_valid() && ap._pad != 0 {
        let meta = unsafe { read_standard_meta(private, private_size) };
        return Some(ParsedAllocIdentity {
            resource_id: ap._pad,
            blob_size: ap.size,
            venus_alloc_size: meta
                .map(|m| m.venus_alloc_size)
                .filter(|&s| s != 0)
                .unwrap_or(ap.size),
            memory_type_index: meta.map(|m| m.memory_type_index).unwrap_or(0),
            ctx_id: ap.ctx_id,
            kind: ap.kind,
        });
    }
    None
}

/// Write the C1 [`HeliosWddmOpenIdentity`] record over the first 48 bytes of an
/// open-time private-data buffer (the meta trailer at bytes 48.. is preserved).
/// Replaces the old `_pad` smuggling: the UMD's pfnOpenResource parses this
/// versioned struct instead of heuristically preferring "whichever buffer has a
/// nonzero `_pad`". Idempotent — rewriting the same record is harmless.
unsafe fn write_open_identity(
    private: *mut c_void,
    private_size: UINT,
    ident: &ParsedAllocIdentity,
) {
    if private.is_null() || (private_size as usize) < size_of::<HeliosWddmOpenIdentity>() {
        return;
    }
    let record = HeliosWddmOpenIdentity {
        venus_alloc_size: ident.venus_alloc_size,
        blob_size: ident.blob_size,
        magic: helios_protocol::HELIOS_WDDM_IDENTITY_MAGIC,
        version: helios_protocol::HELIOS_WDDM_IDENTITY_VERSION,
        resource_id: ident.resource_id,
        memory_type_index: ident.memory_type_index,
        ctx_id: ident.ctx_id,
        kind: ident.kind,
        reserved: [0; 2],
    };
    // SAFETY: bounds-checked; the buffer is writable for the DDI call's duration
    // (same contract create_one relies on for its create-time write-back).
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(private as *mut u8, size_of::<HeliosWddmOpenIdentity>())
    };
    bytes.copy_from_slice(bytes_of(&record));
}

/// Tear down one blob allocation: unmap (if mapped) → detach → unref → free the
/// KMD context. Best-effort on the virtio ops (teardown must not get stuck).
/// PASSIVE_LEVEL (DxgkDdiDestroyAllocation) — the round-trips ride
/// `virtio::ctrl`'s PASSIVE waits.
unsafe fn destroy_allocation_ctx(adapter: &AdapterContext, ctx: Box<AllocationContext>) {
    if ctx.resource_id != 0 && ctx.owns_resource {
        // Drop the owner-0 tracking slot (registered at CreateAllocation, or
        // re-owned to the allocation at adopt), unmapping the GDI executor's
        // host-visible mapping if one is live.
        let unmapped_here = crate::virtio::ctrl::forget_allocation_blob(adapter, ctx.resource_id);
        if ctx.mapped && !unmapped_here {
            let _ = crate::virtio::ctrl::resource_unmap_blob(adapter, ctx.resource_id);
        }
        // One guarded teardown path for created AND adopted resources. The old
        // adopted arm unref'd unconditionally, which double-freed resources
        // another path had already reclaimed — QEMU's "virgl_cmd_resource_unref:
        // resource does not exist ×9" at the 2026-07-03 boot-#3 dwm teardown.
        let first_teardown = adapter
            .with_virtio(|v| v.take_live_resource(ctx.resource_id))
            .unwrap_or(false);
        if first_teardown {
            let _ = crate::virtio::ctrl::ctx_detach_resource(adapter, ctx.ctx_id, ctx.resource_id);
            let _ = crate::virtio::ctrl::resource_unref(adapter, ctx.resource_id);
        }
        if ctx.venus_memory_id != 0 {
            // KMD-backed standard allocation: after the RESOURCE teardown above
            // (the host blob holds a reference into the memory object),
            // vkFreeMemory the venus memory. Best-effort: if the venus client
            // is already gone (device teardown), the host context destruction
            // reclaims everything anyway.
            let _ =
                adapter.with_venus_client(|c| c.free_memory_blob(adapter, ctx.venus_memory_id));
        }
    }
    drop(ctx);
}

/// Create the virtio blob for one allocation and fill its VidMm metadata. On
/// failure nothing is stored (the caller unwinds prior allocations).
unsafe fn create_one(
    adapter: &AdapterContext,
    resource_private: *const c_void,
    resource_private_size: UINT,
    info: &mut DXGK_ALLOCATIONINFO,
) -> Result<(), NTSTATUS> {
    // ── Read + validate the ICD's private driver data ───────────────────────
    let mut priv_ptr = info.pPrivateDriverData as *const u8;
    let mut priv_len = info.PrivateDriverDataSize as usize;
    if !resource_private.is_null() && (resource_private_size as usize) > priv_len {
        priv_ptr = resource_private as *const u8;
        priv_len = resource_private_size as usize;
        crate::diag::record(0x0C01_0040);
    }
    if priv_ptr.is_null() || priv_len < size_of::<HeliosWddmAllocPrivate>() {
        crate::diag::record(0x0C01_0002);
        return Err(STATUS_INVALID_PARAMETER);
    }
    // SAFETY: bounds-checked above; the runtime guarantees `priv_len` bytes at
    // `priv_ptr`. Read unaligned — the buffer carries no alignment guarantee.
    let priv_bytes =
        unsafe { core::slice::from_raw_parts(priv_ptr, size_of::<HeliosWddmAllocPrivate>()) };
    let ap: HeliosWddmAllocPrivate = pod_read_unaligned(priv_bytes);
    crate::diag::record(0x0C11_0000 | (ap.kind & 0xFFFF));
    crate::diag::record(0x0C30_0000 | ((info.PrivateDriverDataSize as u32).min(0xFFFF)));
    crate::diag::record(0x0C31_0000 | ((resource_private_size as u32).min(0xFFFF)));
    crate::diag::record(0x0C32_0000 | (ap.ctx_id & 0xFFFF));
    if !ap.is_valid() {
        crate::diag::record(0x0C01_0003);
        return Err(STATUS_INVALID_PARAMETER);
    }

    // For a KMD-originated standard allocation (DWM/IddCx composition surface), the
    // private data carries a geometry trailer the KMD itself wrote in
    // GetStandardAllocationDriverData, and `ctx_id` is the KMD's internal venus
    // context — which must be live (it is at Code 0; defensive otherwise).
    let mut meta = unsafe { read_standard_meta(priv_ptr as *const c_void, priv_len as UINT) }
        .unwrap_or_else(HeliosWddmAllocMeta::zeroed);
    let mut ap = ap;
    let mut supplied_resource_id = 0u32;
    if ap.kind == HELIOS_WDDM_ALLOC_KIND_STANDARD {
        if ap.ctx_id == 0 {
            ap.ctx_id = adapter.venus_ctx_id;
        }
        if ap.ctx_id == 0 {
            crate::diag::record(0x0C01_00E2);
            return Err(STATUS_DEVICE_NOT_READY);
        }
        // Runtime-created standard allocations may either be backed by a Venus
        // VkDeviceMemory object supplied by the UMD (composition/render targets)
        // or by a KMD-created scratch blob (legacy shadow/staging surfaces). Keep
        // a nonzero blob id intact so dxgkrnl/IddCx and DXVK observe the same
        // backing store.
        ap.size = if ap.size == 0 { PAGE as u64 } else { ap.size };
        if ap._pad != 0 {
            supplied_resource_id = ap._pad;
            crate::diag::record(0x0C39_0000 | (supplied_resource_id & 0xFFFF));
        }
    } else if ap.kind == HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY && ap._pad != 0 {
        supplied_resource_id = ap._pad;
        crate::diag::record(0x0C39_0000 | (supplied_resource_id & 0xFFFF));
    }

    // ── Create the backing virtio-gpu blob (create_blob + ctx_attach) ───────
    crate::diag::record(0x0C01_0010 | (ap.kind & 0xFF));
    let adopt_supplied_resource = supplied_resource_id != 0;
    let mut venus_memory_id = 0u64;
    let resource_id = if adopt_supplied_resource {
        // C1 lifetime fix: adopting transfers the blob's ownership from the
        // ICD's escape owner (D3DKMT device handle) to THIS allocation, so a
        // later DestroyDevice sweep of the creating process cannot unref a
        // host resource that live shared WDDM allocations still denote (the
        // res-45 invalid-import class). Adopting a DEAD resid is a hard error:
        // succeeding here would create a permanently-black shared surface that
        // poisons every opener's venus ring at import time. The re-own only
        // happens for DEVICE_MEMORY adopts — the kinds whose lifetime this
        // allocation actually takes (`owns_resource` below); anything else is
        // liveness-validated only.
        let take_ownership = ap.kind == HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY;
        match adapter.with_virtio(|v| {
            if take_ownership {
                v.adopt_blob_for_allocation(supplied_resource_id)
            } else {
                v.resource_is_live(supplied_resource_id)
            }
        }) {
            Ok(true) => supplied_resource_id,
            Ok(false) => {
                crate::diag::record(0x0C01_00E4);
                return Err(STATUS_INVALID_PARAMETER);
            }
            Err(_de) => {
                crate::diag::record(0x0C01_00E1);
                return Err(STATUS_DEVICE_NOT_READY);
            }
        }
    } else if ap.kind == HELIOS_WDDM_ALLOC_KIND_STANDARD {
        // KMD-originated standard allocation (indirect-swapchain backbuffer, GDI
        // redirection/staging surface). Back it with a REAL venus `VkDeviceMemory`
        // blob through the kernel venus client: user-mode venus contexts (DWM
        // opening the surface) import it by resource id and vkBindImageMemory2
        // against it — a raw `blob_id = 0` shmem blob has no memory object behind
        // it, and that bind poisons the importer's venus ring (host: "failed to
        // look up object of type 8" → fatal decoder state → context destroyed).
        // `allocate_memory_blob` also registers the blob in the tracking table
        // (owner 0), which the GDI executor's `blob_kernel_range` resolves.
        // PASSIVE flow under the venus mutex (never the DISPATCH spinlock).
        match adapter.with_venus_client(|c| {
            c.allocate_memory_blob(adapter, ap.size, true)
                .map(|b| (b, c.memory_type_index()))
        }) {
            Ok(Ok((blob, kernel_mti))) => {
                venus_memory_id = blob.blob_id;
                // Record the EXACT venus allocation parameters into the trailer
                // (written back to the runtime's buffer below) so cross-process
                // openers import with the creator's size + memory type.
                meta.venus_alloc_size = blob.size;
                meta.memory_type_index = kernel_mti;
                blob.res_id
            }
            Ok(Err(_ve)) => {
                // Host rejected the backing blob. STATUS_NO_MEMORY is the
                // documented CreateAllocation failure status; STATUS_UNSUCCESSFUL
                // (0xC0000001) is NOT in the DDI's legal return set — dxgkrnl
                // logged it as "Driver returned an invalid NTSTATUS" (197×) and
                // responded with adapter resets during boot.
                crate::diag::record(0x0C01_00E3);
                return Err(STATUS_NO_MEMORY);
            }
            Err(_de) => {
                crate::diag::record(0x0C01_00E1);
                return Err(STATUS_DEVICE_NOT_READY);
            }
        }
    } else {
        match crate::virtio::ctrl::resource_create_blob(
            adapter,
            ap.ctx_id,
            ap.blob_mem,
            ap.blob_flags,
            ap.blob_id,
            ap.size,
        ) {
            Ok(rid) => {
                // Register the blob in the tracking table (owner 0 = KMD-internal)
                // so the GDI executor's `blob_kernel_range` can resolve and
                // host-map this allocation by resource id. Removed again in
                // `destroy_allocation_ctx` via `forget_allocation_blob`.
                let _ = adapter.with_virtio(|v| v.note_blob_size(rid, ap.size));
                rid
            }
            Err(_ve) => {
                // Host rejected the blob (e.g. the .56 blob_id=0 RESP_ERR_UNSPEC case).
                // STATUS_NO_MEMORY, not STATUS_UNSUCCESSFUL — see the standard-alloc
                // arm above (invalid-NTSTATUS → dxgkrnl adapter resets).
                crate::diag::record(0x0C01_00E0);
                return Err(STATUS_NO_MEMORY);
            }
        }
    };
    crate::diag::record(0x0C01_0020);
    crate::diag::record(resource_id);
    let owns_resource = !adopt_supplied_resource || ap.kind == HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY;
    if adopt_supplied_resource {
        // UMD/Venus-backed allocations arrive with a Mesa BO resource id in
        // `_pad`. The UMD transfers lifetime ownership from the ICD to this WDDM
        // allocation after pfnAllocateCb succeeds (blob-slot re-owned above), so
        // DestroyAllocation releases DEVICE_MEMORY resources even though they
        // were not created through this CreateAllocation call. Cross-context
        // imports attach explicitly through HELIOS_ESCAPE_ATTACH_RESOURCE.
        crate::diag::record(0x0C3A_1000 | (resource_id & 0x0FFF));
    }
    // The venus identity a cross-process opener needs: exact vkAllocateMemory
    // size + memory type. For adopted resources the UMD recorded them in the
    // trailer at create; for KMD standard allocations they were filled from the
    // kernel venus client above. Fallback: the (page-rounded) blob size.
    if meta.venus_alloc_size == 0 {
        meta.venus_alloc_size = ap.size;
    }
    if !adopt_supplied_resource && priv_len >= size_of::<HeliosWddmAllocPrivate>() {
        // Create-time write-back into dxgkrnl's per-allocation buffer (the copy
        // OpenAllocation later reads): the created resid in `_pad`, and — when
        // the trailer fits — the venus identity fields recorded above.
        ap._pad = resource_id;
        let dst = unsafe {
            core::slice::from_raw_parts_mut(
                priv_ptr as *mut u8,
                size_of::<HeliosWddmAllocPrivate>(),
            )
        };
        dst.copy_from_slice(bytes_of(&ap));
        if priv_len >= size_of::<HeliosWddmAllocPrivate>() + size_of::<HeliosWddmAllocMeta>() {
            // SAFETY: bounds-checked; trailer follows the 48-byte prefix in the
            // same runtime-owned buffer.
            let meta_dst = unsafe {
                core::slice::from_raw_parts_mut(
                    (priv_ptr as *mut u8).add(size_of::<HeliosWddmAllocPrivate>()),
                    size_of::<HeliosWddmAllocMeta>(),
                )
            };
            meta_dst.copy_from_slice(bytes_of(&meta));
        }
        crate::diag::record(0x0C3B_0000 | (resource_id & 0xFFFF));
    }
    record_alloc_event(resource_id, meta.width, meta.height, ap.ctx_id, false);

    let size = round_up_page(if ap.size == 0 {
        PAGE
    } else {
        ap.size as SIZE_T
    });
    // CPU-rasterized surfaces (GDI/shadow/staging/shared-primary standard
    // allocations, KMD-backed by a mappable venus blob) go to the BAR memory
    // segment (id 3): CPU raster then lands in the SAME bytes the allocation's
    // venus blob exposes (two-memory-split fix). UMD/venus-backed (adopted)
    // allocations keep the aperture — their CPU access rides the ICD's escape
    // blob mapping, and device-local blobs are not mappable. Bisect arms
    // (probe_only RAM region / classic descriptor) never receive allocations.
    let bar_seg_id = adapter
        .bar_segment
        .as_ref()
        .filter(|b| !b.probe_only)
        .map(|b| b.seg_id);
    let bar_eligible = venus_memory_id != 0 && bar_seg_id.is_some();

    let ctx = Box::new(AllocationContext {
        magic: ALLOCATION_CTX_MAGIC,
        ctx_id: ap.ctx_id,
        resource_id,
        owns_resource,
        blob_id: ap.blob_id,
        venus_memory_id,
        size,
        map_offset: 0,
        map_len: 0,
        mapped: false,
        width: meta.width,
        height: meta.height,
        format: meta.format,
        venus_alloc_size: meta.venus_alloc_size,
        memory_type_index: meta.memory_type_index,
        bar_placed: core::sync::atomic::AtomicU64::new(BAR_UNPLACED),
        bar_eligible,
    });

    // ── VidMm metadata: segment placement + CPU visibility ──────────────────
    info.hAllocation = Box::into_raw(ctx) as HANDLE;
    info.Size = size;
    info.PitchAlignedSize = size;
    let (preferred_segment, supported_segments) = if let (true, Some(seg_id)) =
        (bar_eligible, bar_seg_id)
    {
        (seg_id, 1u32 << (seg_id - 1))
    } else {
        (
            crate::ddi::gpummu::APERTURE_SEGMENT_ID,
            1u32 << (crate::ddi::gpummu::APERTURE_SEGMENT_ID - 1),
        )
    };
    info.SupportedWriteSegmentSet = supported_segments;
    info.EvictionSegmentSet = 0;
    info.HintedBank.__bindgen_anon_1.Value = 0;
    info.AllocationPriority = D3DDDI_ALLOCATIONPRIORITY_NORMAL;
    info.pAllocationUsageHint = core::ptr::null_mut();
    unsafe {
        info.__bindgen_anon_1.Alignment = PAGE as UINT;
        info.PreferredSegment
            .__bindgen_anon_1
            .__bindgen_anon_1
            .set_SegmentId0(preferred_segment);
        info.__bindgen_anon_2.SupportedReadSegmentSet = supported_segments;
        info.__bindgen_anon_3.MaximumRenamingListLength = 0;
        info.__bindgen_anon_3.PhysicalAdapterIndex = 0;
        info.__bindgen_anon_4
            .FlagsWddm2
            .__bindgen_anon_1
            .__bindgen_anon_1
            .set_CpuVisible(1);
    }
    crate::diag::record(0x0C12_0000 | ((size >> 12).min(0xFFFF) as u32));
    crate::diag::record(
        0x0C13_0000
            | (((info.__bindgen_anon_1.Alignment as u32 >> 12) & 0xFF) << 8)
            | (info.PitchAlignedSize.min(0xFF) as u32),
    );
    crate::diag::record(
        0x0C14_0000
            | (((unsafe { info.__bindgen_anon_2.SupportedReadSegmentSet } as u32) & 0xFF) << 8)
            | (info.SupportedWriteSegmentSet & 0xFF),
    );
    crate::diag::record(
        0x0C15_0000
            | ((info.EvictionSegmentSet & 0xFF) << 8)
            | (unsafe { info.__bindgen_anon_4.FlagsWddm2.__bindgen_anon_1.Value } & 0xFF),
    );
    crate::diag::record(0x0C19_0000 | ((info.AllocationPriority >> 16) & 0xFFFF));
    crate::diag::record(0x0C16_0000 | (meta.width.min(0xFFFF) as u32));
    crate::diag::record(0x0C17_0000 | (meta.height.min(0xFFFF) as u32));
    crate::diag::record(0x0C18_0000 | (meta.format & 0xFFFF));
    Ok(())
}

pub unsafe extern "C" fn dxgkddi_create_allocation(
    h_adapter: *mut c_void,
    create_allocation: *mut DXGKARG_CREATEALLOCATION,
) -> NTSTATUS {
    if h_adapter.is_null() || create_allocation.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    crate::diag::record(0x0C01_0001);
    // SAFETY: Dxgkrnl passes our adapter context and a valid args struct.
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let args = unsafe { &mut *create_allocation };
    crate::diag::record(0x0C10_0000 | ((args.NumAllocations as u32).min(0xFFFF)));
    crate::diag::record(0x0C33_0000 | ((args.PrivateDriverDataSize as u32).min(0xFFFF)));
    crate::diag::record(0x0C34_0000 | (unsafe { args.Flags.__bindgen_anon_1.Value } & 0xFFFF));
    if args.NumAllocations == 0 || args.pAllocationInfo.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    let wants_resource = unsafe { args.Flags.__bindgen_anon_1.__bindgen_anon_1.Resource() } != 0;
    if wants_resource {
        crate::diag::record(0x0C3C_0000 | ((args.hResource as usize as u32) & 0xFFFF));
        let resource = Box::new(ResourceContext {
            _marker: 0x4845_5243, // "HERC"
        });
        args.hResource = Box::into_raw(resource) as HANDLE;
        crate::diag::record(0x0C01_0030);
        crate::diag::record(0x0C3D_0000 | ((args.hResource as usize as u32) & 0xFFFF));
    }

    for i in 0..args.NumAllocations as usize {
        // SAFETY: pAllocationInfo points to NumAllocations elements.
        let info = unsafe { &mut *args.pAllocationInfo.add(i) };
        if let Err(status) = unsafe {
            create_one(
                adapter,
                args.pPrivateDriverData,
                args.PrivateDriverDataSize,
                info,
            )
        } {
            // Unwind the allocations already created in this call.
            for j in 0..i {
                let prev = unsafe { &mut *args.pAllocationInfo.add(j) };
                if !prev.hAllocation.is_null() {
                    let ctx = unsafe { Box::from_raw(prev.hAllocation as *mut AllocationContext) };
                    unsafe { destroy_allocation_ctx(adapter, ctx) };
                    prev.hAllocation = core::ptr::null_mut();
                }
            }
            return status;
        }
    }

    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_destroy_allocation(
    h_adapter: *mut c_void,
    destroy_allocation: *const DXGKARG_DESTROYALLOCATION,
) -> NTSTATUS {
    if h_adapter.is_null() || destroy_allocation.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let args = unsafe { &*destroy_allocation };
    if args.NumAllocations != 0 && args.pAllocationList.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    for i in 0..args.NumAllocations as usize {
        let handle = unsafe { *args.pAllocationList.add(i) };
        if !handle.is_null() {
            let ctx = unsafe { Box::from_raw(handle as *mut AllocationContext) };
            unsafe { destroy_allocation_ctx(adapter, ctx) };
        }
    }

    let destroy_resource = unsafe {
        args.Flags
            .__bindgen_anon_1
            .__bindgen_anon_1
            .DestroyResource()
    } != 0;
    if destroy_resource && !args.hResource.is_null() {
        crate::diag::record(0x0C01_0031);
        let _resource = unsafe { Box::from_raw(args.hResource as *mut ResourceContext) };
    }

    STATUS_SUCCESS
}

// ── Allocation lifetime DDIs. ───────────────────────────────────────────────

/// `DxgkDdiOpenAllocation` — bind a device to allocations. dxgkrnl calls this for
/// EVERY allocation (including ones the same device just created via
/// `CreateAllocation`, not only cross-process opens), so it must succeed or
/// `D3DKMTCreateAllocation` fails with the open status. For each open-info entry
/// return a miniport-owned, device-specific tracking handle as required by the
/// DDI contract.
pub unsafe extern "C" fn dxgkddi_open_allocation(
    h_device: IN_CONST_HANDLE,
    open_allocation: IN_CONST_PDXGKARG_OPENALLOCATION,
) -> NTSTATUS {
    crate::diag::record(0x0C02_0003);
    if h_device.is_null() || open_allocation.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: hDevice is the DeviceContext we returned from DxgkDdiCreateDevice;
    // its adapter back-pointer is valid for the device's lifetime.
    let adapter = unsafe {
        &*(*(h_device as *const crate::device::DeviceContext)).adapter
    };
    // SAFETY: valid per the DDI contract; `pOpenAllocation` is a `*mut` array of
    // `NumAllocations` entries whose `hDeviceSpecificAllocation` we fill.
    // The struct has output fields (`Pitch`, `SubresourceOffset`) despite the WDK
    // typedef being exposed through a const pointer in our bindings.
    let args = unsafe { &mut *(open_allocation as *mut DXGKARG_OPENALLOCATION) };
    if args.NumAllocations != 0 && args.pOpenAllocation.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    for i in 0..args.NumAllocations as usize {
        let info = unsafe { &mut *args.pOpenAllocation.add(i) };
        crate::diag::record(0x0C21_0000 | ((info.PrivateDriverDataSize as u32).min(0xFFFF)));
        crate::diag::record(0x0C35_0000 | ((info.hAllocation as usize as u32) & 0xFFFF));

        // Capture the backing venus identity + geometry from the open-time
        // private data (the create-time `HeliosWddmAllocPrivate` write-back or a
        // previous open's `HeliosWddmOpenIdentity`, plus the meta trailer).
        // Present only hands us `hDeviceSpecificAllocation`, so this is where
        // the source/destination surfaces become resolvable for the composition
        // blit.
        let meta = unsafe {
            read_standard_meta(info.pPrivateDriverData, info.PrivateDriverDataSize)
                .or_else(|| read_standard_meta(args.pPrivateDriverData, args.PrivateDriverSize))
        };
        let ident = unsafe {
            read_alloc_identity(info.pPrivateDriverData, info.PrivateDriverDataSize)
                .or_else(|| read_alloc_identity(args.pPrivateDriverData, args.PrivateDriverSize))
        };
        let resource_id = ident.map(|d| d.resource_id).unwrap_or(0);

        // C1 liveness gate: an identified allocation whose venus resource is no
        // longer alive must FAIL the open loudly. Succeeding here is what used
        // to hand consumers a dead resid — the opener's venus import then
        // poisoned its whole ring (host `invalid res_id` → CS error → fatal
        // decoder state; dwm abort, boot #3 2026-07-03) or, with the UMD's old
        // fallback, silently rendered a black substitute texture forever.
        if resource_id != 0 {
            match adapter.with_virtio(|v| v.resource_is_live(resource_id)) {
                Ok(true) => {}
                Ok(false) => {
                    crate::diag::record(0x0C02_00E4);
                    crate::diag::record(0x0C3E_0000 | (resource_id & 0xFFFF));
                    record_alloc_event(resource_id, 0xDEAD, 0xDEAD, 0, true);
                    // Unwind opens already handed out in this call.
                    for j in 0..i {
                        let prev = unsafe { &mut *args.pOpenAllocation.add(j) };
                        if !prev.hDeviceSpecificAllocation.is_null() {
                            drop(unsafe {
                                Box::from_raw(
                                    prev.hDeviceSpecificAllocation as *mut OpenAllocationContext,
                                )
                            });
                            prev.hDeviceSpecificAllocation = core::ptr::null_mut();
                        }
                    }
                    return STATUS_INVALID_PARAMETER;
                }
                Err(_de) => {
                    crate::diag::record(0x0C02_00E5);
                    return STATUS_DEVICE_NOT_READY;
                }
            }
        }

        let open = Box::new(OpenAllocationContext {
            allocation: info.hAllocation,
            private_size: info.PrivateDriverDataSize,
            resource_id,
            width: meta.map(|m| m.width).unwrap_or(0),
            height: meta.map(|m| m.height).unwrap_or(0),
            format: meta.map(|m| m.format).unwrap_or(0),
        });
        record_alloc_event(
            resource_id,
            meta.map(|m| m.width).unwrap_or(0),
            meta.map(|m| m.height).unwrap_or(0),
            0,
            true,
        );
        info.hDeviceSpecificAllocation = Box::into_raw(open) as HANDLE;
        crate::diag::record(
            0x0C36_0000 | ((info.hDeviceSpecificAllocation as usize as u32) & 0xFFFF),
        );
        crate::diag::record(0x0C3C_0000 | (resource_id & 0xFFFF));

        // Write the versioned C1 identity record into the open-time private-data
        // buffers (replaces the `_pad` smuggling). For KMD-created standard
        // allocations (indirect-swapchain backbuffers, GDI redirection textures)
        // the runtime's UMD-visible RESOURCE-level copy is the pristine
        // GetStandardAllocationDriverData output, so the UMD's pfnOpenResource
        // could not alias the venus resource without this. This is the only
        // KMD-side point where the open-path buffers are reachable, and the
        // record is only written for a validated-live resource (gate above).
        if let Some(ident) = ident {
            unsafe {
                write_open_identity(
                    info.pPrivateDriverData as *mut c_void,
                    info.PrivateDriverDataSize,
                    &ident,
                );
                write_open_identity(
                    args.pPrivateDriverData as *mut c_void,
                    args.PrivateDriverSize,
                    &ident,
                );
            }
        }

        if let Some(meta) = meta {
            args.SubresourceOffset = 0;
            args.Pitch = if meta.pitch != 0 {
                meta.pitch
            } else {
                cross_adapter_pitch(meta.width)
            };
            crate::diag::record(0x0C38_0000 | (args.Pitch.min(0xFFFF) as u32));
        }
    }
    STATUS_SUCCESS
}

/// `DxgkDdiCloseAllocation` — release device-local allocation references.
pub unsafe extern "C" fn dxgkddi_close_allocation(
    _h_device: IN_CONST_HANDLE,
    close_allocation: IN_CONST_PDXGKARG_CLOSEALLOCATION,
) -> NTSTATUS {
    if close_allocation.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let args = unsafe { &*close_allocation };
    if args.NumAllocations != 0 && args.pOpenHandleList.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    for i in 0..args.NumAllocations as usize {
        let handle = unsafe { *args.pOpenHandleList.add(i) };
        if !handle.is_null() {
            crate::diag::record(0x0C37_0000 | ((handle as usize as u32) & 0xFFFF));
            let open = unsafe { Box::from_raw(handle as *mut OpenAllocationContext) };
            let _ = (open.allocation, open.private_size);
        }
    }
    STATUS_SUCCESS
}

/// `DxgkDdiDescribeAllocation` — report an allocation's dimensions/format.
///
/// dxgkrnl calls this for shared / cross-process surfaces (and DWM's composition
/// surfaces) to learn their geometry. We echo the geometry recorded at
/// CreateAllocation time (from the standard-allocation trailer). UMD blob
/// allocations carry no dimensions (0×0); report them as-is.
pub unsafe extern "C" fn dxgkddi_describe_allocation(
    h_adapter: IN_CONST_HANDLE,
    describe_allocation: INOUT_PDXGKARG_DESCRIBEALLOCATION,
) -> NTSTATUS {
    crate::diag::record(0x0C02_0001);
    if h_adapter.is_null() || describe_allocation.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl passes a writable DXGKARG_DESCRIBEALLOCATION.
    let args = unsafe { &mut *describe_allocation };
    if args.hAllocation.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: hAllocation is the AllocationContext pointer we returned from
    // CreateAllocation; dxgkrnl round-trips it back unmodified.
    let ctx = unsafe { &*(args.hAllocation as *const AllocationContext) };
    crate::diag::record(0x0C20_0000 | (ctx.width.min(0xFFFF) as u32));
    crate::diag::record(0x0C22_0000 | (ctx.height.min(0xFFFF) as u32));
    crate::diag::record(0x0C23_0000 | (ctx.format & 0xFFFF));

    args.Width = ctx.width;
    args.Height = ctx.height;
    // Default a plausible BGRA format for dimensionless UMD blobs so dxgkrnl never
    // sees D3DDDIFMT_UNKNOWN(0) for a describable allocation.
    args.Format = if ctx.format != 0 {
        ctx.format as D3DDDIFORMAT
    } else {
        D3DDDIFMT_A8R8G8B8
    };
    args.MultisampleMethod.NumSamples = 1;
    args.MultisampleMethod.NumQualityLevels = 1;
    args.RefreshRate.Numerator = 60;
    args.RefreshRate.Denominator = 1;
    args.PrivateDriverFormatAttribute = 0;
    STATUS_SUCCESS
}

/// `DxgkDdiGetStandardAllocationDriverData` — describe a runtime "standard"
/// allocation (shared primary, shadow, staging, GDI surface). DWM and IddCx use
/// these for the desktop composition surfaces.
///
/// Two-call contract (viogpu3d `viogpu_allocation.cpp:135` is the template):
///   1. **Size query** — `pAllocationPrivateDriverData == NULL`: report the byte
///      sizes the runtime must allocate for the per-allocation / per-resource
///      private data.
///   2. **Fill** — buffers provided: write the private data the runtime then hands
///      to `DxgkDdiCreateAllocation`, and fill the surface `Pitch` out-fields.
///
/// We fill a [`HeliosWddmAllocPrivate`] (`kind = STANDARD`, `blob_id = 0` so the
/// host allocates a HOST3D mappable blob, `ctx_id` = the KMD's internal venus
/// context) plus a [`HeliosWddmAllocMeta`] geometry trailer.
pub unsafe extern "C" fn dxgkddi_get_standard_allocation_driver_data(
    h_adapter: IN_CONST_HANDLE,
    standard_allocation: INOUT_PDXGKARG_GETSTANDARDALLOCATIONDRIVERDATA,
) -> NTSTATUS {
    if h_adapter.is_null() || standard_allocation.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl hands back our AdapterContext and a writable args struct.
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let args = unsafe { &mut *standard_allocation };
    crate::diag::record(0x0C02_0002 | ((args.StandardAllocationType as u32 & 0xFF) << 4));

    const PRIV_SIZE: u32 =
        (size_of::<HeliosWddmAllocPrivate>() + size_of::<HeliosWddmAllocMeta>()) as u32;

    // ── Phase 1: size query (runtime passes a null allocation buffer) ────────
    if args.pAllocationPrivateDriverData.is_null() {
        args.AllocationPrivateDriverDataSize = PRIV_SIZE;
        args.ResourcePrivateDriverDataSize = PRIV_SIZE;
        return STATUS_SUCCESS;
    }
    if (args.AllocationPrivateDriverDataSize as usize) < PRIV_SIZE as usize {
        return STATUS_INVALID_PARAMETER;
    }
    if !args.pResourcePrivateDriverData.is_null()
        && (args.ResourcePrivateDriverDataSize as usize) < PRIV_SIZE as usize
    {
        return STATUS_INVALID_PARAMETER;
    }

    // ── Phase 2: extract geometry from the per-type union; set out Pitch ─────
    // SAFETY: the union arm is selected by StandardAllocationType; dxgkrnl
    // guarantees the matching surface-data pointer is valid for the fill call.
    let (width, height, format): (u32, u32, u32) = match args.StandardAllocationType {
        D3DKMDT_STANDARDALLOCATION_SHAREDPRIMARYSURFACE => {
            let sd = unsafe { &*args.__bindgen_anon_1.pCreateSharedPrimarySurfaceData };
            (sd.Width, sd.Height, sd.Format as u32)
        }
        D3DKMDT_STANDARDALLOCATION_SHADOWSURFACE => {
            let sd = unsafe { &mut *args.__bindgen_anon_1.pCreateShadowSurfaceData };
            sd.Pitch = cross_adapter_pitch(sd.Width);
            (sd.Width, sd.Height, sd.Format as u32)
        }
        D3DKMDT_STANDARDALLOCATION_STAGINGSURFACE => {
            let sd = unsafe { &mut *args.__bindgen_anon_1.pCreateStagingSurfaceData };
            sd.Pitch = cross_adapter_pitch(sd.Width);
            (sd.Width, sd.Height, D3DDDIFMT_A8R8G8B8 as u32)
        }
        D3DKMDT_STANDARDALLOCATION_GDISURFACE => {
            let sd = unsafe { &mut *args.__bindgen_anon_1.pCreateGdiSurfaceData };
            sd.Pitch = cross_adapter_pitch(sd.Width);
            (sd.Width, sd.Height, sd.Format as u32)
        }
        _ => {
            crate::diag::record(0x0C02_00E2);
            return STATUS_NOT_SUPPORTED;
        }
    };

    let pitch = cross_adapter_pitch(width);
    // Size the blob past pitch×height: these blobs get imported as LINEAR
    // VkImages on the host, and NVIDIA's external-linear image requirements
    // round the row count up to GOB granularity plus opaque tail slack
    // (observed: 1896x48 → 487424 vs 368640 tight; 1896x1030 → 8773632 vs
    // 7913472; 1024x1872 → 7864320 = pitch × align(1872, 128)). A blob
    // smaller than the image requirement binds "successfully" and then MMU-
    // faults when the sampler reads the slack region (host Xid 31,
    // FAULT_PTE VIRT_READ — killed the IDD feed live 2026-07-04). The
    // importer refuses undersized imports loudly, so an insufficient bound
    // here surfaces as a failed open, never a GPU fault.
    let padded_rows = ((height as u64) + 127) & !127;
    let size = (pitch as u64)
        .saturating_mul(padded_rows)
        .saturating_add(64 * 1024)
        .max(PAGE as u64);

    let ap = HeliosWddmAllocPrivate::new(
        HELIOS_WDDM_ALLOC_KIND_STANDARD,
        adapter.venus_ctx_id,
        0, // blob_id 0 → host-allocated HOST3D mappable blob (no UMD venus memory)
        size,
        VIRTIO_GPU_BLOB_MEM_HOST3D,
        VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE,
        VIRTIO_GPU_MAP_CACHE_CACHED,
    );
    let meta = HeliosWddmAllocMeta {
        width,
        height,
        format,
        pitch,
        // D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET. Keep standard
        // cross-adapter surfaces usable by the UMD when another process opens
        // them through the shared-resource path.
        bind_flags: 0x0000_0008 | 0x0000_0020,
        misc_flags: 0,
        // Filled by DxgkDdiCreateAllocation's write-back once the kernel venus
        // client has actually allocated the backing memory.
        venus_alloc_size: 0,
        memory_type_index: 0,
        // KMD standard allocations are BGRA composition surfaces; leave the DXGI
        // format hint 0 so the UMD opener uses its BGRA fallback. (Field formerly
        // named `reserved`; same on-wire slot.)
        dxgi_format: 0,
    };

    // SAFETY: AllocationPrivateDriverDataSize bytes (>= PRIV_SIZE) are writable.
    let dst = args.pAllocationPrivateDriverData as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(
            &ap as *const _ as *const u8,
            dst,
            size_of::<HeliosWddmAllocPrivate>(),
        );
        core::ptr::copy_nonoverlapping(
            &meta as *const _ as *const u8,
            dst.add(size_of::<HeliosWddmAllocPrivate>()),
            size_of::<HeliosWddmAllocMeta>(),
        );
    }
    if !args.pResourcePrivateDriverData.is_null() {
        let dst = args.pResourcePrivateDriverData as *mut u8;
        unsafe {
            core::ptr::copy_nonoverlapping(
                &ap as *const _ as *const u8,
                dst,
                size_of::<HeliosWddmAllocPrivate>(),
            );
            core::ptr::copy_nonoverlapping(
                &meta as *const _ as *const u8,
                dst.add(size_of::<HeliosWddmAllocPrivate>()),
                size_of::<HeliosWddmAllocMeta>(),
            );
        }
        args.ResourcePrivateDriverDataSize = PRIV_SIZE;
    }
    args.AllocationPrivateDriverDataSize = PRIV_SIZE;
    crate::diag::record(0x0C02_0005);
    STATUS_SUCCESS
}
