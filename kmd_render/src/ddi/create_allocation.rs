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
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use bytemuck::{bytes_of, pod_read_unaligned, Zeroable};
use helios_protocol::{
    HeliosWddmAllocMeta, HeliosWddmAllocPrivate, HeliosWddmOpenIdentity,
    HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY, HELIOS_WDDM_ALLOC_KIND_STANDARD,
    HELIOS_WDDM_ALLOC_MISC_DIRECT_SCANOUT, HELIOS_WDDM_ALLOC_MISC_GDI_TYPE_MASK,
    HELIOS_WDDM_ALLOC_MISC_GDI_TYPE_SHIFT, HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE,
    HELIOS_WDDM_ALLOC_MISC_PRIMARY, HELIOS_WDDM_ALLOC_MISC_RESOURCE_ASSOCIATED,
    HELIOS_WDDM_ALLOC_MISC_STANDARD_TYPE_MASK, HELIOS_WDDM_ALLOC_MISC_STANDARD_TYPE_SHIFT,
    VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE, VIRTIO_GPU_BLOB_MEM_HOST3D, VIRTIO_GPU_MAP_CACHE_CACHED,
    VIRTIO_GPU_MAP_CACHE_WC,
};

use crate::adapter::AdapterContext;
use crate::dxgk::_D3DDDIFORMAT::{D3DDDIFMT_A8B8G8R8, D3DDDIFMT_A8R8G8B8, D3DDDIFMT_X8R8G8B8};
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
    /// Nonzero when the standard allocation's memory is bound to a kernel-created
    /// Venus `VkImage` (the shared-primary scanout path).
    venus_image_id: u64,
    /// Lazily-created kernel-Venus alias of an adopted UMD OPTIMAL image. The
    /// alias imports `resource_id` memory and exists solely so the KMD can copy
    /// the exact SetVidPn primary into its durable LINEAR scanout image.
    scanout_copy_image_id: core::sync::atomic::AtomicU64,
    scanout_copy_memory_id: core::sync::atomic::AtomicU64,
    scanout_copy_conversion_image_id: core::sync::atomic::AtomicU64,
    scanout_copy_conversion_memory_id: core::sync::atomic::AtomicU64,
    scanout_copy_conversion_init_pool_id: core::sync::atomic::AtomicU64,
    scanout_copy_pool_id: core::sync::atomic::AtomicU64,
    scanout_copy_command_buffer_id: core::sync::atomic::AtomicU64,
    scanout_copy_target_image_id: core::sync::atomic::AtomicU64,
    scanout_copy_last_fence: core::sync::atomic::AtomicU64,
    scanout_copy_owns_source_alias: AtomicU32,
    /// Exact segment-relative address supplied by Windows in
    /// `DXGKARG_SETVIDPNSOURCEADDRESS` for this allocation. Keeping it on the
    /// allocation makes the raised-IRQL callback's deferred handle and address
    /// one identity; the worker never combines an allocation with a global
    /// "latest address" from another flip.
    vidpn_primary_address: AtomicU64,
    /// Exact WDDM segment containing `vidpn_primary_address`, supplied in the
    /// same `DXGKARG_SETVIDPNSOURCEADDRESS` callback.
    vidpn_primary_segment: AtomicU32,
    /// Exact `DXGK_SETVIDPNSOURCEADDRESS_FLAGS::Value` paired with the callback.
    vidpn_primary_flags: AtomicU32,
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
    /// Exact D3D11 DDI bind flags supplied by the creator. The KMD Venus
    /// source alias must reproduce the OPTIMAL image's usage contract for
    /// external-memory aliasing on NVIDIA.
    bind_flags: u32,
    /// Row pitch in bytes as the UMD laid the surface out (`cross_adapter_pitch`,
    /// 256-aligned — NOT `width*4`). `SetVidPnSourceAddress`'s `SET_SCANOUT_BLOB`
    /// must use THIS stride so the host reads rows at the right offset (a `width*4`
    /// stride shears the scan-out: 1896×4=7584 vs the real 7680). 0 for allocations
    /// with no geometry trailer.
    pitch: u32,
    /// Exact DXGI format the creator used (`meta.dxgi_format`) — the D3DDDIFORMAT
    /// `format` field above is lossy (both B8G8R8A8 and R8G8B8A8 collapse to
    /// A8R8G8B8), so the scan-out format is resolved from this.
    dxgi_format: u32,
    /// The UMD created this exact `pPrimaryDesc` allocation as a plain LINEAR
    /// DMA_BUF and recorded the verified direct-scanout marker in its meta.
    direct_scanout: bool,
    /// Byte offset of the plain-LINEAR COLOR plane within the backing allocation
    /// (from the UMD's `vkGetImageSubresourceLayout` on a direct primary).
    /// `SetVidPnSourceAddress`'s `SET_SCANOUT_BLOB` uses it as the plane offset;
    /// 0 for surfaces whose data starts at offset 0.
    plane_offset: u64,
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

/// `"HERC"` — the marker that says a `hResource` is one this driver minted.
const RESOURCE_CTX_MARKER: u32 = 0x4845_5243;

/// CreateAllocation calls that arrived with a non-null `hResource` (`RcIn`) and
/// how many of those did not carry our marker (`RcBad`). `RcIn` staying 0 is
/// what makes the "mint only over null" rule a provable no-op.
static RESOURCE_INPUT_HANDLES: AtomicU32 = AtomicU32::new(0);
static RESOURCE_FOREIGN_HANDLES: AtomicU32 = AtomicU32::new(0);

/// OpenAllocation calls carrying more than one entry (`OaMulti`). The call-level
/// OUT fields describe exactly one of them, so this is the population that would
/// have to exist before anything is tuned for multi-surface opens. Every writer
/// in this tree sets NumAllocations = 1.
static MULTI_ENTRY_OPENS: AtomicU32 = AtomicU32::new(0);

/// Per-device open state for an allocation. Dxgkrnl's `hAllocation` in
/// `DXGK_OPENALLOCATIONINFO` is its non-device-specific allocation handle; the
/// miniport must return its own device-specific handle here and later receives it
/// in command allocation lists / CloseAllocation.
const OPEN_ALLOCATION_CTX_MAGIC: u32 = 0x484F_504E; // "HOPN"

/// Opaque allocation token owned by dxgkrnl.
///
/// This is deliberately not a pointer-shaped type. `DXGK_OPENALLOCATIONINFO`
/// calls the field `hAllocation`, but its type is `D3DKMT_HANDLE` (a 32-bit
/// runtime token), not the miniport's `AllocationContext*` returned from
/// `DxgkDdiCreateAllocation`. Keeping the token behind a newtype prevents open
/// allocation code from accidentally casting it back to a KMD context.
#[repr(transparent)]
#[derive(Clone, Copy)]
struct RuntimeAllocationHandle(D3DKMT_HANDLE);

struct OpenAllocationContext {
    magic: u32,
    runtime_allocation: RuntimeAllocationHandle,
    private_size: u32,
    /// Validated immutable view captured from open-time private data. Present
    /// receives only this device-specific open handle, so it must use this
    /// snapshot rather than trying to reinterpret dxgkrnl's runtime token as an
    /// `AllocationContext*`.
    present: Option<PresentAllocInfo>,
}

/// Surface identity + geometry for a Present allocation-list entry, resolved from
/// its `hDeviceSpecificAllocation` ([`present_alloc_info`]).
#[derive(Clone, Copy)]
pub struct PresentAllocInfo {
    /// Per-device runtime allocation token supplied by dxgkrnl in
    /// `DXGK_OPENALLOCATIONINFO::hAllocation`.
    pub runtime_allocation: u32,
    pub resource_id: u32,
    /// Versioned allocation kind from the creator/open identity. Present uses
    /// this explicit contract to choose image-vs-buffer interpretation; a
    /// resource id is never guessed from geometry or memory visibility.
    pub kind: u32,
    pub width: u32,
    pub height: u32,
    /// Authoritative byte stride of KMD-created standard allocations. Ordinary
    /// UMD OPTIMAL images leave this at zero because they have no linear row
    /// layout; Present destinations backed by the GDI staging contract carry
    /// the exact 256-byte-aligned pitch.
    pub pitch: u32,
    /// Exact memory-plane-0 offset carried in the allocation private data.
    pub plane_offset: u64,
    /// Authoritative legacy D3DDDIFORMAT supplied for KMD-created standard
    /// allocations. Some such allocations predate an exact DXGI trailer.
    pub format: u32,
    /// Exact creator-side DXGI format; unlike D3DDDIFORMAT, this preserves
    /// BGRA alpha-vs-X identity.
    pub dxgi_format: u32,
    /// Exact D3D11 DDI bind flags used to create the ordinary OPTIMAL image.
    pub bind_flags: u32,
    /// The creator's image/buffer storage contract, captured from authoritative
    /// private data. Present must match this exhaustively; a STANDARD allocation
    /// is not inherently a linear byte buffer.
    pub storage: PresentAllocationStorage,
    /// Exact external allocation contract required by Venus import.
    pub venus_alloc_size: u64,
    pub memory_type_index: u32,
    /// Exact `D3DKMDT_STANDARDALLOCATION_TYPE` supplied by Windows, or zero for
    /// a UMD-created allocation.
    pub standard_allocation_type: u32,
    /// Exact `D3DKMDT_GDISURFACETYPE` supplied by Windows, or zero when the
    /// standard allocation is not a GDI surface.
    pub standard_gdi_surface_type: u32,
    /// Exact `DXGK_OPENALLOCATIONFLAGS::Value` supplied by dxgkrnl.
    pub open_flags: u32,
    /// Whether `DXGK_CREATEALLOCATIONFLAGS::Resource` was set for the
    /// allocation's create call.
    pub resource_associated: bool,
    pub allocation_private_size: u32,
    pub resource_private_size: u32,
    /// The allocation was created from the runtime's documented
    /// `pPrimaryDesc` contract and explicitly exported for direct scanout.
    pub direct_scanout: bool,
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PresentAllocationStorage {
    /// Ordinary UMD shared OPTIMAL image, imported through OPAQUE_FD.
    OptimalOpaqueFdImage = 0,
    /// Cross-context DMA_BUF image (direct primary or a KMD-created GDI
    /// redirection texture).
    OptimalCrossContextImage = 1,
    /// KMD-created standard CPU-visible surface with an authoritative pitch.
    PitchedStandardBuffer = 2,
}

impl PresentAllocInfo {
    /// Resolve the exact Vulkan/DXGI Present format without
    /// geometry/content heuristics.
    ///
    /// UMD-created allocations carry an exact DXGI value. KMD-created standard
    /// allocations may carry only the authoritative D3DDDIFORMAT; use the same
    /// fixed mapping as UMD `d3d_format_to_dxgi`.
    pub fn resolved_dxgi_format(self) -> Option<u32> {
        match self.dxgi_format {
            exact if exact != 0 => Some(exact),
            0 if self.format == D3DDDIFMT_A8B8G8R8 as u32 => Some(28),
            0 if self.format == D3DDDIFMT_A8R8G8B8 as u32 => Some(87),
            0 if self.format == D3DDDIFMT_X8R8G8B8 as u32 => Some(88),
            _ => None,
        }
    }
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
    if open.magic != OPEN_ALLOCATION_CTX_MAGIC {
        return None;
    }
    open.present
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

/// Allocation handles refused because they were null or failed the magic check
/// (`DsBad` — DescribeAllocation) and reclaim sites that refused to reconstruct
/// a `Box` from such a handle (`FreeBad`). Both must stay 0.
static DESCRIBE_BAD_HANDLE: AtomicU32 = AtomicU32::new(0);
static RECLAIM_BAD_HANDLE: AtomicU32 = AtomicU32::new(0);

/// The ONE place a dxgkrnl allocation handle becomes an `&AllocationContext`.
///
/// Every accessor in this file open-codes the same null + magic pair, and
/// `DxgkDdiDescribeAllocation` open-coded neither — it dereferenced the handle
/// raw. Honest note on the guarantee: a magic check does NOT reliably detect a
/// freed Box (freed non-paged pool often still reads back as "HALC"). What it
/// buys is one owner for the cast plus a counter if a foreign handle ever shows
/// up (k-alloc-02).
///
/// # Safety
/// `h` must be a handle this driver returned from `DxgkDdiCreateAllocation` and
/// that dxgkrnl still considers live.
unsafe fn resolve_alloc(h: HANDLE) -> Option<&'static AllocationContext> {
    if h.is_null() {
        return None;
    }
    // SAFETY: non-null; the magic word is checked before any other field is
    // trusted, and the caller guarantees the handle's provenance.
    let ctx = unsafe { &*(h as *const AllocationContext) };
    (ctx.magic == ALLOCATION_CTX_MAGIC).then_some(ctx)
}

/// Geometry `DxgkDdiDescribeAllocation` reports, from a magic-checked handle.
pub(crate) struct DescribeInfo {
    pub width: u32,
    pub height: u32,
    pub format: u32,
}

/// Reclaim ownership of an allocation handle's `Box`, but only if it still
/// passes the magic check.
///
/// A handle that does not is LEAKED on purpose: reconstructing a `Box` from a
/// pointer this driver did not mint would free foreign pool. Counted as
/// `FreeBad`, which must stay 0.
///
/// # Safety
/// `h` must be a handle from `DxgkDdiCreateAllocation` that dxgkrnl is handing
/// back exactly once for reclamation.
unsafe fn take_alloc_ctx(h: HANDLE) -> Option<Box<AllocationContext>> {
    if unsafe { resolve_alloc(h) }.is_none() {
        RECLAIM_BAD_HANDLE.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // SAFETY: the magic check just proved this is one of our boxes, and the
    // caller guarantees dxgkrnl hands each handle back once.
    Some(unsafe { Box::from_raw(h as *mut AllocationContext) })
}

/// Reclaim an open-allocation handle's `Box`, magic-checked like
/// [`take_alloc_ctx`]. A handle that fails is leaked and counted (`FreeBad`).
///
/// # Safety
/// `h` must be a `hDeviceSpecificAllocation` this driver published, handed back
/// exactly once.
unsafe fn take_open_ctx(h: HANDLE) -> Option<Box<OpenAllocationContext>> {
    if h.is_null() {
        return None;
    }
    // SAFETY: non-null; magic is read before any other field is trusted.
    let magic_ok = unsafe { (*(h as *const OpenAllocationContext)).magic }
        == OPEN_ALLOCATION_CTX_MAGIC;
    if !magic_ok {
        RECLAIM_BAD_HANDLE.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // SAFETY: as above, plus the caller's once-only guarantee.
    Some(unsafe { Box::from_raw(h as *mut OpenAllocationContext) })
}

/// # Safety
/// As [`resolve_alloc`].
unsafe fn describe_alloc_info(h: HANDLE) -> Option<DescribeInfo> {
    let ctx = unsafe { resolve_alloc(h) }?;
    Some(DescribeInfo {
        width: ctx.width,
        height: ctx.height,
        format: ctx.format,
    })
}

/// Resolve a paging-op `hAllocation` (the handle this driver returned from
/// `DxgkDdiCreateAllocation`) to its paging view. Returns `None` for null or
/// magic-mismatched handles — a garbage dereference here would bugcheck.
///
/// SAFETY: `h` must be an in-flight paging op's `hAllocation` (dxgkrnl keeps
/// the allocation alive across its paging operations).
pub(crate) unsafe fn paging_alloc_info(h: HANDLE) -> Option<PagingAllocInfo> {
    let ctx = unsafe { resolve_alloc(h) }?;
    Some(PagingAllocInfo {
        resource_id: ctx.resource_id,
        size: ctx.size as u64,
        bar_eligible: ctx.bar_eligible,
        bar_placed: ctx.bar_placed.load(Ordering::Acquire),
    })
}

/// Geometry + layout of a scan-out primary, resolved from its `hAllocation`.
#[derive(Clone, Copy)]
pub(crate) struct ScanoutInfo {
    pub resource_id: u32,
    pub width: u32,
    pub height: u32,
    /// Row pitch the UMD laid the surface out with (bytes) — the stride
    /// `SET_SCANOUT_BLOB` must use, NOT `width*4`. 0 if unknown.
    pub pitch: u32,
    /// Exact DXGI format (lossless) for resolving the virtio scan-out format.
    pub dxgi_format: u32,
    /// Memory-plane-0 byte offset for `SET_SCANOUT_BLOB` (0 if data starts at 0).
    pub plane_offset: u64,
    /// Exact Venus allocation identity used by cross-context imports.
    pub venus_alloc_size: u64,
    pub memory_type_index: u32,
    pub direct_scanout: bool,
    /// Exact `PrimarySegment` paired with this hAllocation by Windows.
    pub primary_segment: u32,
    /// Exact `PrimaryAddress` paired with this hAllocation by Windows.
    pub primary_address: u64,
    /// Exact `DXGK_SETVIDPNSOURCEADDRESS_FLAGS::Value` supplied by Windows.
    pub primary_flags: u32,
}

/// Preserve the exact segment, address, and flags Windows paired with a
/// SetVidPn allocation.
///
/// SAFETY: `h` is the live KMD allocation handle supplied by dxgkrnl to
/// `DxgkDdiSetVidPnSourceAddress`.
pub(crate) unsafe fn set_vidpn_primary_address(
    h: HANDLE,
    primary_segment: u32,
    primary_address: u64,
    primary_flags: u32,
) -> bool {
    if h.is_null() {
        return false;
    }
    let ctx = unsafe { &*(h as *const AllocationContext) };
    if ctx.magic != ALLOCATION_CTX_MAGIC {
        return false;
    }
    ctx.vidpn_primary_segment
        .store(primary_segment, Ordering::Relaxed);
    ctx.vidpn_primary_flags
        .store(primary_flags, Ordering::Relaxed);
    ctx.vidpn_primary_address
        .store(primary_address, Ordering::Release);
    true
}

/// Resolve a primary allocation's `hAllocation` (the CreateAllocation handle
/// dxgkrnl passes in `SetVidPnSourceAddress`) to its scan-out geometry + layout
/// for `SET_SCANOUT_BLOB`. Returns `None` for a null/foreign handle or an
/// unbacked allocation. SAFETY: same contract as [`paging_alloc_info`].
pub(crate) unsafe fn scanout_alloc_info(h: HANDLE) -> Option<ScanoutInfo> {
    if h.is_null() {
        return None;
    }
    let ctx = unsafe { &*(h as *const AllocationContext) };
    if ctx.magic != ALLOCATION_CTX_MAGIC || ctx.resource_id == 0 {
        return None;
    }
    Some(ScanoutInfo {
        resource_id: ctx.resource_id,
        width: ctx.width,
        height: ctx.height,
        pitch: ctx.pitch,
        dxgi_format: ctx.dxgi_format,
        plane_offset: ctx.plane_offset,
        venus_alloc_size: ctx.venus_alloc_size,
        memory_type_index: ctx.memory_type_index,
        direct_scanout: ctx.direct_scanout,
        primary_segment: ctx.vidpn_primary_segment.load(Ordering::Relaxed),
        primary_address: ctx.vidpn_primary_address.load(Ordering::Acquire),
        primary_flags: ctx.vidpn_primary_flags.load(Ordering::Relaxed),
    })
}

fn cached_prepared_copy(
    ctx: &AllocationContext,
) -> Option<crate::virtio::venus::PreparedImageCopy> {
    let command_buffer_id = ctx.scanout_copy_command_buffer_id.load(Ordering::Acquire);
    if command_buffer_id == 0 {
        return None;
    }
    let owns_source_alias = ctx.scanout_copy_owns_source_alias.load(Ordering::Relaxed) != 0;
    Some(crate::virtio::venus::PreparedImageCopy {
        owns_source_alias,
        source_resource_id: if owns_source_alias {
            ctx.resource_id
        } else {
            0
        },
        source_image_id: ctx.scanout_copy_image_id.load(Ordering::Relaxed),
        source_memory_id: ctx.scanout_copy_memory_id.load(Ordering::Relaxed),
        conversion_image_id: ctx.scanout_copy_conversion_image_id.load(Ordering::Relaxed),
        conversion_memory_id: ctx
            .scanout_copy_conversion_memory_id
            .load(Ordering::Relaxed),
        conversion_init_pool_id: ctx
            .scanout_copy_conversion_init_pool_id
            .load(Ordering::Relaxed),
        command_pool_id: ctx.scanout_copy_pool_id.load(Ordering::Relaxed),
        command_buffer_id,
        target_image_id: ctx.scanout_copy_target_image_id.load(Ordering::Relaxed),
        width: ctx.width,
        height: ctx.height,
    })
}

fn publish_prepared_copy(ctx: &AllocationContext, copy: &crate::virtio::venus::PreparedImageCopy) {
    // command_buffer_id is the publish word. A reader that acquires a nonzero
    // command id sees one coherent immutable PreparedImageCopy snapshot.
    ctx.scanout_copy_owns_source_alias
        .store(copy.owns_source_alias as u32, Ordering::Relaxed);
    ctx.scanout_copy_image_id
        .store(copy.source_image_id, Ordering::Relaxed);
    ctx.scanout_copy_memory_id
        .store(copy.source_memory_id, Ordering::Relaxed);
    ctx.scanout_copy_conversion_image_id
        .store(copy.conversion_image_id, Ordering::Relaxed);
    ctx.scanout_copy_conversion_memory_id
        .store(copy.conversion_memory_id, Ordering::Relaxed);
    ctx.scanout_copy_conversion_init_pool_id
        .store(copy.conversion_init_pool_id, Ordering::Relaxed);
    ctx.scanout_copy_pool_id
        .store(copy.command_pool_id, Ordering::Relaxed);
    ctx.scanout_copy_target_image_id
        .store(copy.target_image_id, Ordering::Relaxed);
    ctx.scanout_copy_command_buffer_id
        .store(copy.command_buffer_id, Ordering::Release);
}

fn clear_prepared_copy(ctx: &AllocationContext) {
    ctx.scanout_copy_command_buffer_id
        .store(0, Ordering::Release);
    ctx.scanout_copy_last_fence.store(0, Ordering::Relaxed);
    ctx.scanout_copy_target_image_id.store(0, Ordering::Relaxed);
    ctx.scanout_copy_pool_id.store(0, Ordering::Relaxed);
    ctx.scanout_copy_conversion_init_pool_id
        .store(0, Ordering::Relaxed);
    ctx.scanout_copy_conversion_memory_id
        .store(0, Ordering::Relaxed);
    ctx.scanout_copy_conversion_image_id
        .store(0, Ordering::Relaxed);
    ctx.scanout_copy_memory_id.store(0, Ordering::Relaxed);
    ctx.scanout_copy_image_id.store(0, Ordering::Relaxed);
    ctx.scanout_copy_owns_source_alias
        .store(0, Ordering::Relaxed);
}

/// Submit a GPU copy from the exact allocation selected by
/// `SetVidPnSourceAddress` into the durable adapter-owned LINEAR scanout image.
/// Setup (external-memory import + command recording) happens once per WDDM
/// allocation; the frame path only queues the reusable command buffer and
/// returns its ring-1 GPU-completion fence.
///
/// SAFETY: `h` is the live `hAllocation` passed by dxgkrnl to
/// SetVidPnSourceAddress. PASSIVE_LEVEL only (the Venus client mutex may wait).
pub(crate) unsafe fn submit_primary_scanout_copy(
    adapter: &AdapterContext,
    h: HANDLE,
    target_image_id: u64,
    width: u32,
    height: u32,
    primary_address: u64,
) -> Result<u64, NTSTATUS> {
    if h.is_null() || target_image_id == 0 || width == 0 || height == 0 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let ctx = unsafe { &*(h as *const AllocationContext) };
    if ctx.magic != ALLOCATION_CTX_MAGIC
        || ctx.resource_id == 0
        || ctx.width != width
        || ctx.height != height
    {
        crate::diag::record_named_bytes(b"CpCpy", 0xE1);
        return Err(STATUS_INVALID_PARAMETER);
    }
    const DXGI_FORMAT_B8G8R8A8_UNORM: u32 = 87;
    const DXGI_FORMAT_B8G8R8X8_UNORM: u32 = 88;
    const DXGI_FORMAT_R8G8B8A8_UNORM: u32 = 28;
    if ctx.dxgi_format != DXGI_FORMAT_R8G8B8A8_UNORM
        && ctx.dxgi_format != DXGI_FORMAT_B8G8R8A8_UNORM
        && ctx.dxgi_format != DXGI_FORMAT_B8G8R8X8_UNORM
    {
        crate::diag::record_named_bytes(b"CpFmt", ctx.dxgi_format);
        crate::diag::record_named_bytes(b"CpCpy", 0xE2);
        return Err(STATUS_NOT_SUPPORTED);
    }

    let result = adapter.with_venus_client(|client| {
        let mut prepared = cached_prepared_copy(ctx);
        if prepared
            .as_ref()
            .map(|p| p.target_image_id != target_image_id)
            .unwrap_or(false)
        {
            let old = prepared.take().unwrap();
            let last = ctx.scanout_copy_last_fence.load(Ordering::Acquire);
            client.destroy_prepared_image_copy(adapter, old, last)?;
            clear_prepared_copy(ctx);
        }
        let copy = match prepared {
            Some(copy) => copy,
            None => {
                let copy = if ctx.venus_image_id != 0 {
                    client.prepare_existing_linear_source_copy(
                        adapter,
                        ctx.venus_image_id,
                        width,
                        height,
                        ctx.dxgi_format,
                        target_image_id,
                    )?
                } else {
                    client.prepare_optimal_scanout_copy(
                        adapter,
                        ctx.resource_id,
                        ctx.venus_alloc_size,
                        ctx.memory_type_index,
                        width,
                        height,
                        ctx.dxgi_format,
                        ctx.bind_flags,
                        target_image_id,
                    )?
                };
                publish_prepared_copy(ctx, &copy);
                copy
            }
        };
        let fence = client.submit_prepared_image_copy(adapter, &copy, primary_address)?;
        ctx.scanout_copy_last_fence.store(fence, Ordering::Release);
        Ok::<u64, crate::virtio::VirtioError>(fence)
    });

    match result {
        Ok(Ok(fence)) => {
            let n = PRIMARY_COPY_SUBMIT_COUNT
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1);
            if n == 1 || n % 600 == 0 {
                crate::diag::record_named_bytes(b"CpCpy", 1);
                crate::diag::record_named_bytes(b"CpFnc", fence as u32);
                crate::diag::record_named_bytes(b"CpCnt", n);
            }
            Ok(fence)
        }
        Ok(Err(_)) => {
            crate::diag::record_named_bytes(b"CpCpy", 0xE3);
            Err(STATUS_DEVICE_NOT_READY)
        }
        Err(_) => {
            crate::diag::record_named_bytes(b"CpCpy", 0xE4);
            Err(STATUS_DEVICE_NOT_READY)
        }
    }
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

/// 32-bpp linear row pitch aligned to the cross-adapter requirement
/// (`D3D12_TEXTURE_DATA_PITCH_ALIGNMENT`, 256 bytes). Re-exported so the existing
/// `crate::ddi::create_allocation::cross_adapter_pitch` call paths in
/// `ddi/display.rs`, `ddi/gdi_blit.rs` and `ddi/scanout_diag.rs` are unchanged.
///
/// The body now lives in `helios_kmd_logic`, which has no dependency edge to
/// `wdk-sys` or the generated `dxgk` bindings and is covered by host unit tests —
/// including the `1896 -> 7680` case `ddi/display.rs` names, i.e. the `.117`-era
/// short-row scanout defect.
pub(crate) use helios_kmd_logic::cross_adapter_pitch;

/// `helios_kmd_logic::round_up_page` operates on `u64`; the callers here pass
/// `SIZE_T`. The call below type-checks only while the two are the same type, and
/// this assertion makes a future divergence a compile error rather than a silent
/// widening of an allocation size.
const _: () = assert!(core::mem::size_of::<SIZE_T>() == core::mem::size_of::<u64>());

fn round_up_page(n: SIZE_T) -> SIZE_T {
    helios_kmd_logic::round_up_page(n)
}

/// Cycling 8-slot fixed-name registry ring of allocation create/open events, so a
/// single boot's surface map (venus resid + geometry + ctx, create vs open) is
/// readable live — used to correlate DWM's composition surfaces (1952x1088,
/// res_id 52/54) against the IDD's IddCx swapchain surface (1920x1080): same
/// venus resid ⇒ shared (sync problem), different ⇒ the surfaces never alias and
/// the composed pixels are never copied into what the IDD reads.
static ALLOC_EVENT_SEQ: AtomicU32 = AtomicU32::new(0);
/// Successful exact-primary copy submissions. Fixed registry breadcrumbs are
/// throttled from this counter; writing the registry per frame would itself
/// throttle the display path.
static PRIMARY_COPY_SUBMIT_COUNT: AtomicU32 = AtomicU32::new(0);

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

/// Trailer lengths that named neither real layout and were therefore refused
/// (registry-visible as `MetaLen`). Zero is the expected value forever: the KMD
/// reports PRIV_SIZE 48 + 48 = 96 and the UMD's RuntimeAllocPrivate is exactly
/// 96 with NumAllocations = 1, so no live writer can produce one.
static META_LEN_REJECTS: AtomicU32 = AtomicU32::new(0);

unsafe fn read_standard_meta(
    private: *const c_void,
    private_size: UINT,
) -> Option<HeliosWddmAllocMeta> {
    use helios_kmd_logic::MetaLayout;

    // The layout enum's byte counts are a copy of the protocol's (kmd_logic has
    // no dependency edge to helios_protocol); this pins them together.
    const _: () = assert!(size_of::<HeliosWddmAllocMeta>() == MetaLayout::FULL_BYTES);

    let base = size_of::<HeliosWddmAllocPrivate>();
    if private.is_null() {
        return None;
    }
    let Some(trailer_len) = (private_size as usize).checked_sub(base) else {
        return None;
    };
    // PER-ARM, not max-union: the copy length comes from the layout, never from
    // arithmetic on an untrusted size, so a trailer that stops mid-field is
    // refused instead of being zero-extended into a plausible wrong value.
    let Ok(layout) = MetaLayout::try_from(trailer_len) else {
        if trailer_len >= MetaLayout::LEGACY_BYTES {
            // A trailer long enough to have been accepted by the old bound.
            // Shorter ones are "no trailer at all" and keep their existing
            // silent refusal.
            let n = META_LEN_REJECTS.fetch_add(1, Ordering::Relaxed) + 1;
            // PASSIVE (both call sites are PASSIVE DDIs). First occurrence plus
            // every 64th: this path is guest-reachable, so it must not be able
            // to turn into a registry-write storm.
            if n == 1 || n % 64 == 0 {
                crate::diag::record_named_bytes(b"MetaLen", n);
            }
        }
        return None;
    };
    let have = layout.copy_bytes();
    let mut raw = [0u8; size_of::<HeliosWddmAllocMeta>()];
    // SAFETY: `have` is 24 or 48 and `trailer_len >= have` was proven by the
    // layout classification, so that many trailer bytes exist past the prefix.
    // A Legacy24 read leaves the remaining 24 bytes zeroed, which is the
    // documented zero-extension.
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
    let allocation_handle = (&*ctx as *const AllocationContext) as usize;
    // Retire the exact Windows/KMD allocation identity before any backing
    // resource, Venus image, or cached copy can be torn down. If QEMU cannot
    // confirm resource_id=0 scanout disable, retain every host object until
    // device teardown rather than leave scanout 0 pointing at an unref'd blob.
    if !adapter.retire_scanout_allocation(allocation_handle, ctx.resource_id) {
        drop(ctx);
        return;
    }
    adapter.system_backings.remove(ctx.resource_id);
    // A prepared scanout copy owns a command buffer which may still be queued
    // through the outer async SUBMIT_3D. Drain that GPU-completion fence and
    // tear the prepared objects down BEFORE touching the allocation's resource,
    // image, or memory. On an ambiguous drain failure, leak the allocation's
    // host objects to Venus-context teardown rather than use-after-free them.
    if let Some(copy) = cached_prepared_copy(&ctx) {
        let last_fence = ctx.scanout_copy_last_fence.load(Ordering::Acquire);
        let drained = adapter
            .with_venus_client(|client| {
                client.destroy_prepared_image_copy(adapter, copy, last_fence)
            })
            .map(|r| r.is_ok())
            .unwrap_or(false);
        if !drained {
            crate::diag::record_named_bytes(b"CpDrn", 0xE);
            drop(ctx);
            return;
        }
        clear_prepared_copy(&ctx);
        crate::diag::record_named_bytes(b"CpDrn", 1);
    }

    // Present BLT command buffers bake imported aliases of ordinary WDDM
    // resources. Drain the cache before any one backing resource can be
    // detached/unref'd. The cache is intentionally one ownership unit because
    // several swapchain sources may share the same DWM destination.
    let present_drained = adapter
        .with_venus_client(|client| {
            client.release_present_blits_for_resource(adapter, ctx.resource_id)
        })
        .map(|result| result.is_ok())
        .unwrap_or(false);
    if !present_drained {
        crate::diag::record_named_bytes(b"PBDrn", 0xE);
        // Ambiguous GPU completion: retain the allocation and all host objects
        // until Venus-context teardown rather than risk a GPU use-after-free.
        drop(ctx);
        return;
    }

    // A DWM import of the adapter-owned LINEAR target can acquire a transient
    // WDDM AllocationContext carrying the same resource id.  That allocation
    // is only an importer: destroying it must not clear, detach, unref, or
    // destroy the adapter-owned scanout image/memory.
    let adapter_owned_scanout = ctx.resource_id != 0
        && adapter.dedicated_scanout_resource.load(Ordering::Acquire) == ctx.resource_id;
    if ctx.resource_id != 0 && !adapter_owned_scanout {
        adapter.forget_primary_scanout(ctx.resource_id);
    }
    if ctx.resource_id != 0 && ctx.owns_resource && !adapter_owned_scanout {
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
        if ctx.venus_image_id != 0 {
            let _ = adapter.with_venus_client(|c| c.destroy_image(adapter, ctx.venus_image_id));
        }
        if ctx.venus_memory_id != 0 {
            // KMD-backed standard allocation: after the RESOURCE teardown above
            // (the host blob holds a reference into the memory object),
            // vkFreeMemory the venus memory. Best-effort: if the venus client
            // is already gone (device teardown), the host context destruction
            // reclaims everything anyway.
            let _ = adapter.with_venus_client(|c| c.free_memory_blob(adapter, ctx.venus_memory_id));
        }
    } else if adapter_owned_scanout {
        crate::diag::record_named_bytes(b"CpKeep", ctx.resource_id);
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
    resource_associated: bool,
) -> Result<(), NTSTATUS> {
    // ── Read + validate the ICD's private driver data ───────────────────────
    //
    // TWO SEPARATE DECISIONS (M3/k-alloc-11). The READ may fall back to the
    // resource-level buffer when it is the larger of the two; the WRITE-BACK
    // below may not. Both HeliosWddmAllocPrivate and HeliosWddmOpenIdentity put
    // `magic` at byte offset 16, so stamping the identity into the shared
    // resource buffer makes the NEXT allocation in the same call fail
    // HeliosWddmAllocPrivate::is_valid. `write_target` is therefore always the
    // per-allocation buffer.
    //
    // The fallback is believed unreachable today (CARAPSz and CARRSz both read
    // 96 on the live box, so `resource_private_size > priv_len` is false), but
    // proving that needs DiagLevel >= 1 for the 0x0C01_0040 breadcrumb, so it is
    // kept rather than deleted.
    let write_target = info.pPrivateDriverData as *const u8;
    let write_target_len = info.PrivateDriverDataSize as usize;
    let mut priv_ptr = write_target;
    let mut priv_len = write_target_len;
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
    if resource_associated {
        meta.misc_flags |= HELIOS_WDDM_ALLOC_MISC_RESOURCE_ASSOCIATED;
    }
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
    let mut venus_image_id = 0u64;
    let is_primary = (meta.misc_flags & HELIOS_WDDM_ALLOC_MISC_PRIMARY) != 0;
    let is_optimal_gdi_texture = ap.kind == HELIOS_WDDM_ALLOC_KIND_STANDARD
        && (meta.misc_flags & HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE) != 0;
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
    } else if ap.kind == HELIOS_WDDM_ALLOC_KIND_STANDARD && is_primary {
        match adapter.with_venus_client(|c| {
            c.allocate_linear_scanout_image_blob(adapter, meta.width, meta.height)
        }) {
            Ok(Ok(scanout)) => {
                venus_memory_id = scanout.blob.blob_id;
                venus_image_id = scanout.image_id;
                meta.pitch = scanout.row_pitch;
                meta.plane_offset = scanout.plane_offset as u64;
                meta.venus_alloc_size = scanout.blob.size;
                meta.memory_type_index = scanout.memory_type_index;
                ap.size = scanout.blob.size;
                scanout.blob.res_id
            }
            Ok(Err(_ve)) => {
                crate::diag::record(0x0C01_00E5);
                return Err(STATUS_NO_MEMORY);
            }
            Err(_de) => {
                crate::diag::record(0x0C01_00E1);
                return Err(STATUS_DEVICE_NOT_READY);
            }
        }
    } else if is_optimal_gdi_texture {
        // D3DKMDT_GDISURFACE_TEXTURE is a shared, non-CPU-visible texture used
        // as both a DWM sample source and a DirectX render target. Preserve that
        // contract with a real cross-context OPTIMAL image. Reinterpreting this
        // allocation as pitched host bytes makes DWM sample tiled memory as a
        // different resource shape and produces a black redirected window.
        let dxgi_format = match meta.format {
            value if value == D3DDDIFMT_A8R8G8B8 as u32 => 87,
            value if value == D3DDDIFMT_X8R8G8B8 as u32 => 88,
            _ => {
                crate::diag::record_named_bytes(b"GdiOFmt", meta.format);
                return Err(STATUS_NOT_SUPPORTED);
            }
        };
        match adapter.with_venus_client(|client| {
            client.allocate_optimal_gdi_image_blob(
                adapter,
                meta.width,
                meta.height,
                meta.bind_flags,
                dxgi_format,
            )
        }) {
            Ok(Ok(image)) => {
                venus_memory_id = image.blob.blob_id;
                venus_image_id = image.image_id;
                meta.pitch = 0;
                meta.plane_offset = 0;
                meta.dxgi_format = dxgi_format;
                meta.venus_alloc_size = image.blob.size;
                meta.memory_type_index = image.memory_type_index;
                ap.size = image.blob.size;
                image.blob.res_id
            }
            Ok(Err(_ve)) => {
                crate::diag::record_named_bytes(b"GdiOImg", 0xE1);
                return Err(STATUS_NO_MEMORY);
            }
            Err(_de) => {
                crate::diag::record_named_bytes(b"GdiOImg", 0xE2);
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
            c.allocate_memory_blob(adapter, ap.size, true, is_primary)
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
    if write_target_len >= size_of::<HeliosWddmOpenIdentity>() && !write_target.is_null() {
        // Create-time write-back into dxgkrnl's per-allocation buffer (the copy
        // OpenAllocation later reads). This must happen for BOTH KMD-created
        // standard allocations and UMD/Venus-backed adopted allocations:
        // windowed DXGI presents hand DWM an allocation token, and the opener
        // can only import the rendered Venus image if this buffer carries the
        // live resource id plus exact vkAllocateMemory identity.
        let ident = ParsedAllocIdentity {
            resource_id,
            blob_size: ap.size,
            venus_alloc_size: meta.venus_alloc_size,
            memory_type_index: meta.memory_type_index,
            ctx_id: ap.ctx_id,
            kind: ap.kind,
        };
        unsafe {
            write_open_identity(
                write_target as *mut c_void,
                write_target_len as UINT,
                &ident,
            );
        }
        if write_target_len >= size_of::<HeliosWddmAllocPrivate>() + size_of::<HeliosWddmAllocMeta>()
        {
            // SAFETY: bounds-checked; trailer follows the 48-byte prefix in the
            // per-allocation runtime-owned buffer.
            let meta_dst = unsafe {
                core::slice::from_raw_parts_mut(
                    (write_target as *mut u8).add(size_of::<HeliosWddmAllocPrivate>()),
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
    // segment: CPU raster then lands in the SAME bytes the allocation's venus
    // blob exposes (two-memory-split fix). UMD/venus-backed adopted allocations
    // stay off this path for now: making every adopted present/shared resource
    // BAR-eligible destabilized the LogonUI/DWM boot path, so the 3D-present
    // equivalent needs a narrower resource-class gate. Bisect arms (probe_only
    // RAM region / classic descriptor) never receive allocations.
    let bar_seg_id = adapter
        .bar_segment
        .as_ref()
        .filter(|b| !b.probe_only)
        .map(|b| b.seg_id);
    let bar_eligible = venus_memory_id != 0 && !is_optimal_gdi_texture && bar_seg_id.is_some();

    let ctx = Box::new(AllocationContext {
        magic: ALLOCATION_CTX_MAGIC,
        ctx_id: ap.ctx_id,
        resource_id,
        owns_resource,
        blob_id: ap.blob_id,
        venus_memory_id,
        venus_image_id,
        scanout_copy_image_id: core::sync::atomic::AtomicU64::new(0),
        scanout_copy_memory_id: core::sync::atomic::AtomicU64::new(0),
        scanout_copy_conversion_image_id: core::sync::atomic::AtomicU64::new(0),
        scanout_copy_conversion_memory_id: core::sync::atomic::AtomicU64::new(0),
        scanout_copy_conversion_init_pool_id: core::sync::atomic::AtomicU64::new(0),
        scanout_copy_pool_id: core::sync::atomic::AtomicU64::new(0),
        scanout_copy_command_buffer_id: core::sync::atomic::AtomicU64::new(0),
        scanout_copy_target_image_id: core::sync::atomic::AtomicU64::new(0),
        scanout_copy_last_fence: core::sync::atomic::AtomicU64::new(0),
        scanout_copy_owns_source_alias: AtomicU32::new(0),
        vidpn_primary_address: AtomicU64::new(0),
        vidpn_primary_segment: AtomicU32::new(0),
        vidpn_primary_flags: AtomicU32::new(0),
        size,
        map_offset: 0,
        map_len: 0,
        mapped: false,
        width: meta.width,
        height: meta.height,
        format: meta.format,
        bind_flags: meta.bind_flags,
        pitch: meta.pitch,
        dxgi_format: meta.dxgi_format,
        direct_scanout: (meta.misc_flags & HELIOS_WDDM_ALLOC_MISC_DIRECT_SCANOUT) != 0,
        plane_offset: meta.plane_offset,
        venus_alloc_size: meta.venus_alloc_size,
        memory_type_index: meta.memory_type_index,
        bar_placed: core::sync::atomic::AtomicU64::new(BAR_UNPLACED),
        bar_eligible,
    });

    // ── VidMm metadata: segment placement + CPU visibility ──────────────────
    info.hAllocation = Box::into_raw(ctx) as HANDLE;
    info.Size = size;
    info.PitchAlignedSize = size;
    // The scan-out display primary (CpuVisible SHAREDPRIMARYSURFACE) needs special
    // handling: dxgkrnl rejects it unless the supported segment set includes an
    // APERTURE segment ("CPUVisible allocations must include an aperture segment in
    // the supported segment set" — ETW-confirmed 36th session), which without it
    // fails the whole VidPn commit → 0-path VidPn → display never activates.
    let aperture_bit = 1u32 << (crate::ddi::gpummu::APERTURE_SEGMENT_ID - 1);
    let (preferred_segment, supported_segments) =
        if let (true, Some(seg_id)) = (bar_eligible, bar_seg_id) {
            // Prefer the BAR (the two-memory-split fix keeps CPU raster in the venus
            // blob's bytes). These allocations are CpuVisible (set below) in a
            // NON-CPU-accessible memory segment — the BAR exposes CPU access only via
            // the CpuHostAperture (segment CpuVisible=0) — and WDDM REQUIRES every such
            // allocation to list an aperture segment in its supported set so VidMm can
            // always obtain a CPU virtual address, falling back to system memory if the
            // CpuHostAperture is full (allocation-usage-tracking.md: "all CPU-accessible
            // allocations in non-CPU-accessible memory segments must contain an aperture
            // segment in their supported segment set"). v71 added it to the PRIMARY only;
            // the other CpuVisible surfaces (SHADOW/STAGING/GDI) shipped BAR-only and
            // dxgkrnl rejected them ("CPUVisible allocations must include an aperture
            // segment", ETW-confirmed v71/v72 — 10 `0x0202` violators in the S-ring) →
            // the whole VidPn commit failed. Gated on the display half so the proven
            // render-only surface (DisplayHalf off) stays byte-identical: it never hit
            // the rejection because its CpuHostAperture always had space, so the fallback
            // was never demanded. PreferredSegment stays the BAR — with a 1 GiB BAR vs a
            // ~200 MB CpuVisible working set, content lives in the venus blob in steady
            // state and the aperture (which VidMm upgrades to the implicit system-memory
            // segment without AccessedPhysically, iommu-dma-remapping.md) is an
            // eviction-only fallback not exercised at this scale — negligible runtime cost.
            let needs_aperture = is_primary || adapter.display_half;
            let supp = 1u32 << (seg_id - 1);
            (
                seg_id,
                if needs_aperture {
                    supp | aperture_bit
                } else {
                    supp
                },
            )
        } else {
            (crate::ddi::gpummu::APERTURE_SEGMENT_ID, aperture_bit)
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
            .set_CpuVisible(u32::from(!is_optimal_gdi_texture));
        // A D3DDDI primary is selected by the display engine using the physical
        // address delivered in SetVidPnSourceAddress. Tell VidMm that exact
        // access model so it allocates the primary contiguously in a
        // GPU-addressable segment rather than at a non-identifiable implicit
        // system-memory address. `is_primary` comes only from Windows'
        // pPrimaryDesc/standard-allocation contract preserved in private data.
        if is_primary {
            info.__bindgen_anon_4
                .FlagsWddm2
                .__bindgen_anon_1
                .__bindgen_anon_1
                .set_AccessedPhysically(1);
        }
        // WB-cacheable CPU views: without `Cached`, dxgkrnl maps user views of
        // these allocations write-combined; WC READS of the BAR window measured
        // ~200 MB/s (36 ms per 7.8 MiB IDD readback frame, 2026-07-06). The BAR
        // is RAM-backed host shmem — cache-coherent on x86 for every agent on
        // the same physical pages, and the host reports the venus blobs
        // CACHED (blob_map honors the same hint for kernel maps). Service-key
        // `AllocCached=0` is the kill switch (read at StartDevice).
        // Omit `Cached` on the scan-out primary: dxgkrnl rejects Cached-with-Primary
        // (AzureTriage; 36th-session primary-creation failure → no VidPn path). The
        // primary is host-scanned-out, not CPU-read-hot, so write-combined is fine.
        if is_primary {
            crate::diag::record(0x0C3E_0000 | (resource_id & 0xFFFF));
        }
        if adapter.alloc_cached && !is_primary && !is_optimal_gdi_texture {
            info.__bindgen_anon_4
                .FlagsWddm2
                .__bindgen_anon_1
                .__bindgen_anon_1
                .set_Cached(1);
        }
    }
    // Allocation creation is not a scanout-selection event. Modern DWM creates
    // and rotates multiple ManagedPrimary allocations; only
    // SetVidPnSourceAddress identifies the one Windows selected for this flip.
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
    let create_flags = unsafe { args.Flags.__bindgen_anon_1.Value };
    let input_resource = args.hResource as usize as u64;
    crate::diag::record_named_bytes(b"CARFlg", create_flags);
    crate::diag::record_named_bytes(b"CARNum", args.NumAllocations);
    crate::diag::record_named_bytes(b"CARRSz", args.PrivateDriverDataSize);
    crate::diag::record_named_bytes(b"CARInLo", input_resource as u32);
    crate::diag::record_named_bytes(b"CARInHi", (input_resource >> 32) as u32);
    crate::diag::record(0x0C10_0000 | ((args.NumAllocations as u32).min(0xFFFF)));
    crate::diag::record(0x0C33_0000 | ((args.PrivateDriverDataSize as u32).min(0xFFFF)));
    crate::diag::record(0x0C34_0000 | (unsafe { args.Flags.__bindgen_anon_1.Value } & 0xFFFF));
    if args.NumAllocations == 0 || args.pAllocationInfo.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    let wants_resource = unsafe { args.Flags.__bindgen_anon_1.__bindgen_anon_1.Resource() } != 0;
    // Mint a ResourceContext ONLY over a null input handle. A non-null
    // args.hResource is an add-allocation-to-existing-resource call: overwriting
    // it minted a second box for one resource and orphaned the first, since the
    // only free path is keyed on Flags.DestroyResource and sees just the last
    // handle. Evidence for the population: CARInLo/CARInHi read 0/0 on the live
    // box, so no in-tree caller takes the new arm today — RcIn is the counter
    // that proves it stays that way (k-alloc-V01).
    let minted_resource = wants_resource && args.hResource.is_null();
    if wants_resource {
        crate::diag::record(0x0C3C_0000 | ((args.hResource as usize as u32) & 0xFFFF));
        if minted_resource {
            let resource = Box::new(ResourceContext {
                _marker: RESOURCE_CTX_MARKER,
            });
            args.hResource = Box::into_raw(resource) as HANDLE;
            crate::diag::record(0x0C01_0030);
        } else {
            // Keep the runtime's handle. Validate it is ours; a foreign value is
            // counted and left untouched — never freed, never overwritten.
            let ours = unsafe { (*(args.hResource as *const ResourceContext))._marker }
                == RESOURCE_CTX_MARKER;
            let n = RESOURCE_INPUT_HANDLES.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n % 64 == 0 {
                crate::diag::record_named_bytes(b"RcIn", n);
            }
            if !ours {
                let m = RESOURCE_FOREIGN_HANDLES.fetch_add(1, Ordering::Relaxed) + 1;
                if m == 1 || m % 64 == 0 {
                    crate::diag::record_named_bytes(b"RcBad", m);
                }
            }
        }
        crate::diag::record(0x0C3D_0000 | ((args.hResource as usize as u32) & 0xFFFF));
    }
    let output_resource = args.hResource as usize as u64;
    crate::diag::record_named_bytes(b"CAROutLo", output_resource as u32);
    crate::diag::record_named_bytes(b"CAROutHi", (output_resource >> 32) as u32);

    for i in 0..args.NumAllocations as usize {
        // SAFETY: pAllocationInfo points to NumAllocations elements.
        let info = unsafe { &mut *args.pAllocationInfo.add(i) };
        crate::diag::record_named_bytes(b"CARAPSz", info.PrivateDriverDataSize);
        if let Err(status) = unsafe {
            create_one(
                adapter,
                args.pPrivateDriverData,
                args.PrivateDriverDataSize,
                info,
                wants_resource,
            )
        } {
            // Unwind the allocations already created in this call, then the
            // ResourceContext this call minted — leaving it published over a
            // failed create handed dxgkrnl a handle to pool nothing would ever
            // free (the only free path runs on DestroyAllocation with
            // Flags.DestroyResource, which never arrives for a failed create).
            for j in 0..i {
                let prev = unsafe { &mut *args.pAllocationInfo.add(j) };
                if let Some(ctx) = unsafe { take_alloc_ctx(prev.hAllocation) } {
                    unsafe { destroy_allocation_ctx(adapter, ctx) };
                }
                prev.hAllocation = core::ptr::null_mut();
            }
            if minted_resource {
                drop(unsafe { Box::from_raw(args.hResource as *mut ResourceContext) });
                args.hResource = input_resource as usize as HANDLE;
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
        if let Some(ctx) = unsafe { take_alloc_ctx(handle) } {
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
        // Magic-checked like the allocation handles: a foreign value is counted
        // and leaked rather than freed as if it were our pool.
        let ours = unsafe { (*(args.hResource as *const ResourceContext))._marker }
            == RESOURCE_CTX_MARKER;
        if ours {
            let _resource = unsafe { Box::from_raw(args.hResource as *mut ResourceContext) };
        } else {
            RECLAIM_BAD_HANDLE.fetch_add(1, Ordering::Relaxed);
        }
    }

    STATUS_SUCCESS
}

// ── Allocation lifetime DDIs. ───────────────────────────────────────────────

/// Free AND NULL every `hDeviceSpecificAllocation` published by entries
/// `0..upto` of this OpenAllocation call.
///
/// Freeing and nulling together is what makes this safe under either dxgkrnl
/// convention (whether or not it calls CloseAllocation for a failed open): a
/// nulled slot cannot be double-freed, and a freed-but-published pointer cannot
/// be dereferenced. It is the exact behaviour the liveness-refusal arm already
/// had by hand; the transport-error arm had none at all (k-alloc-09).
///
/// # Safety
/// `args.pOpenAllocation` must hold at least `upto` entries this call published.
unsafe fn unwind_opens(args: &mut DXGKARG_OPENALLOCATION, upto: usize) {
    for j in 0..upto {
        // SAFETY: j < upto <= NumAllocations, checked by the caller.
        let prev = unsafe { &mut *args.pOpenAllocation.add(j) };
        drop(unsafe { take_open_ctx(prev.hDeviceSpecificAllocation) });
        prev.hDeviceSpecificAllocation = core::ptr::null_mut();
    }
}

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
    let adapter = unsafe { &*(*(h_device as *const crate::device::DeviceContext)).adapter };
    // SAFETY: valid per the DDI contract; `pOpenAllocation` is a `*mut` array of
    // `NumAllocations` entries whose `hDeviceSpecificAllocation` we fill.
    // The struct has output fields (`Pitch`, `SubresourceOffset`) despite the WDK
    // typedef being exposed through a const pointer in our bindings.
    let args = unsafe { &mut *(open_allocation as *mut DXGKARG_OPENALLOCATION) };
    if args.NumAllocations != 0 && args.pOpenAllocation.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // Which entry the call-level OUT fields describe. SubresourceIndex exists in
    // the binding and was never read; entry 0 is the fallback, which reproduces
    // today's value exactly for the single-entry opens this tree produces
    // (the UMD always sets NumAllocations = 1).
    let subresource_entry = (args.SubresourceIndex as usize).min(
        (args.NumAllocations as usize).saturating_sub(1),
    );
    if args.NumAllocations > 1 {
        let n = MULTI_ENTRY_OPENS.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 || n % 64 == 0 {
            crate::diag::record_named_bytes(b"OaMulti", n);
        }
    }
    let mut subresource_meta = None;
    let mut subresource_ident = None;
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
                    unsafe { unwind_opens(args, i) };
                    return STATUS_INVALID_PARAMETER;
                }
                Err(_de) => {
                    crate::diag::record(0x0C02_00E5);
                    // Was a bare return: a transport error mid-OpenAllocation
                    // leaked every OpenAllocationContext already published in
                    // this call. Both failure arms unwind identically now.
                    unsafe { unwind_opens(args, i) };
                    return STATUS_DEVICE_NOT_READY;
                }
            }
        }

        let open_flags = unsafe { args.Flags.__bindgen_anon_1.Value };
        let present = ident.map(|identity| {
            let misc_flags = meta.map(|m| m.misc_flags).unwrap_or(0);
            let storage = if identity.kind == HELIOS_WDDM_ALLOC_KIND_STANDARD {
                if misc_flags & HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE != 0 {
                    PresentAllocationStorage::OptimalCrossContextImage
                } else {
                    PresentAllocationStorage::PitchedStandardBuffer
                }
            } else if misc_flags & HELIOS_WDDM_ALLOC_MISC_DIRECT_SCANOUT != 0 {
                PresentAllocationStorage::OptimalCrossContextImage
            } else {
                PresentAllocationStorage::OptimalOpaqueFdImage
            };
            PresentAllocInfo {
                runtime_allocation: info.hAllocation,
                resource_id,
                kind: identity.kind,
                width: meta.map(|m| m.width).unwrap_or(0),
                height: meta.map(|m| m.height).unwrap_or(0),
                pitch: meta.map(|m| m.pitch).unwrap_or(0),
                plane_offset: meta.map(|m| m.plane_offset).unwrap_or(0),
                format: meta.map(|m| m.format).unwrap_or(0),
                dxgi_format: meta.map(|m| m.dxgi_format).unwrap_or(0),
                bind_flags: meta.map(|m| m.bind_flags).unwrap_or(0),
                storage,
                venus_alloc_size: identity.venus_alloc_size,
                memory_type_index: identity.memory_type_index,
                standard_allocation_type: (misc_flags & HELIOS_WDDM_ALLOC_MISC_STANDARD_TYPE_MASK)
                    >> HELIOS_WDDM_ALLOC_MISC_STANDARD_TYPE_SHIFT,
                standard_gdi_surface_type: (misc_flags & HELIOS_WDDM_ALLOC_MISC_GDI_TYPE_MASK)
                    >> HELIOS_WDDM_ALLOC_MISC_GDI_TYPE_SHIFT,
                open_flags,
                resource_associated: misc_flags & HELIOS_WDDM_ALLOC_MISC_RESOURCE_ASSOCIATED != 0,
                allocation_private_size: info.PrivateDriverDataSize,
                resource_private_size: args.PrivateDriverSize,
                direct_scanout: misc_flags & HELIOS_WDDM_ALLOC_MISC_DIRECT_SCANOUT != 0,
            }
        });
        let open = Box::new(OpenAllocationContext {
            magic: OPEN_ALLOCATION_CTX_MAGIC,
            runtime_allocation: RuntimeAllocationHandle(info.hAllocation),
            private_size: info.PrivateDriverDataSize,
            present,
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
            // Per-allocation buffer here; the RESOURCE-level copy is written
            // once after the loop, from the designated entry only (M4) — it is
            // one buffer shared by every entry in the call, so writing it per
            // iteration meant the last entry silently won.
            unsafe {
                write_open_identity(
                    info.pPrivateDriverData as *mut c_void,
                    info.PrivateDriverDataSize,
                    &ident,
                );
            }
        }

        // `Pitch` and `SubresourceOffset` are CALL-level OUT fields, not
        // per-entry ones; writing them inside the loop meant the last entry won
        // silently. They are computed once after the loop, from the entry
        // args.SubresourceIndex designates (M4/k-alloc-12).
        if i == subresource_entry {
            subresource_meta = meta;
            subresource_ident = ident;
        }
    }

    // The resource-level identity buffer is call-level, like Pitch: written once,
    // from the designated entry. Load-bearing for KMD-created standard
    // allocations, whose UMD-visible resource-level copy is otherwise the
    // pristine GetStandardAllocationDriverData output — without this the UMD's
    // pfnOpenResource cannot alias the venus resource.
    if let Some(ident) = subresource_ident {
        unsafe {
            write_open_identity(
                args.pPrivateDriverData as *mut c_void,
                args.PrivateDriverSize,
                &ident,
            );
        }
    }

    if let Some(meta) = subresource_meta {
        args.SubresourceOffset = 0;
        args.Pitch = if meta.misc_flags & HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE != 0 {
            0
        } else if meta.pitch != 0 {
            meta.pitch
        } else {
            cross_adapter_pitch(meta.width)
        };
        crate::diag::record(0x0C38_0000 | (args.Pitch.min(0xFFFF) as u32));
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
            if let Some(open) = unsafe { take_open_ctx(handle) } {
                let _ = (open.runtime_allocation, open.private_size);
            }
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
    // This DDI used to dereference hAllocation with no magic check at all —
    // the only handle accessor in the file that trusted the pointer outright.
    // SAFETY: hAllocation is the AllocationContext pointer we returned from
    // CreateAllocation; dxgkrnl round-trips it back unmodified.
    let Some(ctx) = (unsafe { describe_alloc_info(args.hAllocation) }) else {
        let n = DESCRIBE_BAD_HANDLE.fetch_add(1, Ordering::Relaxed) + 1;
        // First occurrence plus every 64th — never a per-call registry write on
        // a path a caller could repeat.
        if n == 1 || n % 64 == 0 {
            crate::diag::record_named_bytes(b"DsBad", n);
        }
        return STATUS_INVALID_PARAMETER;
    };
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
    let standard_allocation_type = args.StandardAllocationType as u32;
    crate::diag::record_named_bytes(b"StdType", standard_allocation_type);
    crate::diag::record(0x0C02_0002 | ((args.StandardAllocationType as u32 & 0xFF) << 4));

    const PRIV_SIZE: u32 =
        (size_of::<HeliosWddmAllocPrivate>() + size_of::<HeliosWddmAllocMeta>()) as u32;

    // ── Phase 1: size query (runtime passes a null allocation buffer) ────────
    if args.pAllocationPrivateDriverData.is_null() {
        args.AllocationPrivateDriverDataSize = PRIV_SIZE;
        args.ResourcePrivateDriverDataSize = PRIV_SIZE;
        crate::diag::record_named_bytes(b"StdPhase", 1);
        crate::diag::record_named_bytes(b"StdAPSz", PRIV_SIZE);
        crate::diag::record_named_bytes(b"StdRPSz", PRIV_SIZE);
        return STATUS_SUCCESS;
    }
    crate::diag::record_named_bytes(b"StdPhase", 2);
    crate::diag::record_named_bytes(b"StdAPSz", args.AllocationPrivateDriverDataSize);
    crate::diag::record_named_bytes(b"StdRPSz", args.ResourcePrivateDriverDataSize);
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
    let is_primary = args.StandardAllocationType == D3DKMDT_STANDARDALLOCATION_SHAREDPRIMARYSURFACE;
    let mut is_optimal_gdi_texture = false;
    let mut gdi_surface_type = 0u32;
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
            gdi_surface_type = sd.Type as u32;
            crate::diag::record_named_bytes(b"GdiType", gdi_surface_type);
            // D3DKMDT_GDISURFACE_TEXTURE (enum value 1) is explicitly not
            // CPU-visible and has no linear-pitch contract. The CPU-visible
            // staging variants are the only GDI types for which Windows
            // requires the miniport to return a pitch.
            is_optimal_gdi_texture = gdi_surface_type == 1;
            sd.Pitch = if is_optimal_gdi_texture {
                0
            } else {
                cross_adapter_pitch(sd.Width)
            };
            (sd.Width, sd.Height, sd.Format as u32)
        }
        _ => {
            crate::diag::record(0x0C02_00E2);
            return STATUS_NOT_SUPPORTED;
        }
    };

    let pitch = if is_optimal_gdi_texture {
        0
    } else {
        cross_adapter_pitch(width)
    };
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
    let size = if is_optimal_gdi_texture {
        // CreateAllocation replaces this estimate with Vulkan's exact memory
        // requirement before reporting Size to VidMm.
        (width as u64)
            .saturating_mul(height as u64)
            .saturating_mul(4)
            .max(PAGE as u64)
    } else {
        (pitch as u64)
            .saturating_mul(padded_rows)
            .saturating_add(64 * 1024)
            .max(PAGE as u64)
    };

    let map_cache = if is_primary {
        VIRTIO_GPU_MAP_CACHE_WC
    } else {
        VIRTIO_GPU_MAP_CACHE_CACHED
    };
    let ap = HeliosWddmAllocPrivate::new(
        HELIOS_WDDM_ALLOC_KIND_STANDARD,
        adapter.venus_ctx_id,
        0, // blob_id 0 → host-allocated HOST3D mappable blob (no UMD venus memory)
        size,
        VIRTIO_GPU_BLOB_MEM_HOST3D,
        VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE,
        map_cache,
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
        // Flag a shared-primary surface so create_one omits the illegal
        // `Cached`-with-Primary flag on the scanout primary.
        misc_flags: ((standard_allocation_type << HELIOS_WDDM_ALLOC_MISC_STANDARD_TYPE_SHIFT)
            & HELIOS_WDDM_ALLOC_MISC_STANDARD_TYPE_MASK)
            | ((gdi_surface_type << HELIOS_WDDM_ALLOC_MISC_GDI_TYPE_SHIFT)
                & HELIOS_WDDM_ALLOC_MISC_GDI_TYPE_MASK)
            | if is_primary {
                HELIOS_WDDM_ALLOC_MISC_PRIMARY
            } else if is_optimal_gdi_texture {
                HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE
            } else {
                0
            },
        // Filled by DxgkDdiCreateAllocation's write-back once the kernel venus
        // client has actually allocated the backing memory.
        venus_alloc_size: 0,
        memory_type_index: 0,
        // The display primary must scan out as XR24/XRGB on the virtio-gpu
        // contract. The Linux virtio primary plane advertises XRGB only, and the
        // matching CachyOS dma-buf probe reached egl-headless only with XR24.
        // Non-primary standard allocations keep the legacy zero hint so UMD
        // openers use the existing BGRA fallback.
        dxgi_format: if is_primary {
            88
        } else if format == D3DDDIFMT_A8R8G8B8 as u32 {
            87
        } else if format == D3DDDIFMT_X8R8G8B8 as u32 {
            88
        } else {
            0
        },
        // KMD standard allocations carry no scan-out plane offset.
        plane_offset: 0,
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
