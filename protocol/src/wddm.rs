//! Helios WDDM (D3DKMT) wire-format structs — the venus-over-WDDM transport
//! contract (see `GATE5_VENUS_WDDM_DESIGN.md`).
//!
//! Parallel to [`crate::escape`] (which carries the System-class IOCTL payloads):
//! these are the private-driver-data layouts the Mesa Venus ICD passes to the
//! `kmd_render` WDDM adapter through `D3DKMT*` thunks, and that the KMD reads in
//! its `DxgkDdi*` callbacks. The IOCTL path stays unchanged; this is additive.
//!
//! Two boundaries use these:
//!   - `D3DKMTCreateAllocation` → `pAllocationInfo[i].pPrivateDriverData`
//!     carries [`HeliosWddmAllocPrivate`]; the KMD's `DxgkDdiCreateAllocation`
//!     reads it and creates the backing virtio-gpu blob (`resource_create_blob`).
//!   - `D3DKMTRender` → the command buffer begins with [`HeliosWddmCmdBuf`]
//!     followed by the opaque venus byte stream; `DxgkDdiRender`/`SubmitCommand`
//!     reads it and forwards the stream via `submit_venus`. Allocation references
//!     travel in the `D3DDDI_ALLOCATIONLIST`/patch list, not inline here.
//!
//! All structs are `repr(C)`, padding-free (8-byte fields first), so they derive
//! `Pod`/`Zeroable` and have a stable byte layout the C ICD mirrors.

use bytemuck::{Pod, Zeroable};

/// `'HWDM'` — sanity magic at the start of every WDDM private-data blob.
pub const HELIOS_WDDM_MAGIC: u32 = 0x4857_444D;
/// Current WDDM transport protocol version.
pub const HELIOS_WDDM_VERSION: u32 = 1;

/// [`HeliosWddmAllocPrivate::kind`] — a host-visible command/staging ring blob
/// with no venus device-memory backing (`blob_id == 0`).
pub const HELIOS_WDDM_ALLOC_KIND_SHMEM: u32 = 0;
/// [`HeliosWddmAllocPrivate::kind`] — a blob bound to a venus `VkDeviceMemory`
/// object (`blob_id == venus mem id` from the ICD's `vkAllocateMemory`).
pub const HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY: u32 = 1;
/// [`HeliosWddmAllocPrivate::kind`] — a runtime "standard" allocation (the
/// shared-primary / shadow / staging / GDI surfaces DWM and IddCx create). The KMD
/// self-fills this private data in `DxgkDdiGetStandardAllocationDriverData`.
/// The meta trailer selects the exact backing: CPU-visible variants use a
/// pitched HOST3D blob, while `D3DKMDT_GDISURFACE_TEXTURE` uses a non-CPU-visible
/// shared OPTIMAL image. The geometry the runtime supplied
/// (width/height/format) is appended after this struct.
pub const HELIOS_WDDM_ALLOC_KIND_STANDARD: u32 = 2;

/// [`HeliosWddmAllocMeta::misc_flags`] — the standard allocation is the exact
/// VidPn primary selected for direct scanout.
///
/// These high bits are guest-private metadata, not D3D11 resource flags. Keep
/// their definitions in the shared protocol crate so the KMD writer and UMD
/// reader cannot silently drift.
pub const HELIOS_WDDM_ALLOC_MISC_PRIMARY: u32 = 0x8000_0000;
/// [`HeliosWddmAllocMeta::misc_flags`] — the primary has the direct-scanout
/// external-memory contract.
pub const HELIOS_WDDM_ALLOC_MISC_DIRECT_SCANOUT: u32 = 0x4000_0000;
/// [`HeliosWddmAllocMeta::misc_flags`] — a
/// `D3DKMDT_GDISURFACE_TEXTURE` standard allocation is a non-CPU-visible,
/// cross-context OPTIMAL image rather than a pitched byte buffer.
pub const HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE: u32 = 0x2000_0000;
/// [`HeliosWddmAllocMeta::misc_flags`] — dxgkrnl asked the KMD to create a
/// resource object for this allocation (`DXGK_CREATEALLOCATIONFLAGS::Resource`).
///
/// This is preserved from the Windows callback verbatim so later
/// `OpenAllocation`/`Present` diagnostics can distinguish a resource-associated
/// allocation from a device-associated allocation without inferring anything
/// from its dimensions, process, or creation order.
pub const HELIOS_WDDM_ALLOC_MISC_RESOURCE_ASSOCIATED: u32 = 0x1000_0000;
/// Bit range in [`HeliosWddmAllocMeta::misc_flags`] carrying the exact
/// `D3DKMDT_STANDARDALLOCATION_TYPE` supplied by Windows.
pub const HELIOS_WDDM_ALLOC_MISC_STANDARD_TYPE_MASK: u32 = 0x0F00_0000;
pub const HELIOS_WDDM_ALLOC_MISC_STANDARD_TYPE_SHIFT: u32 = 24;
/// Bit range in [`HeliosWddmAllocMeta::misc_flags`] carrying the exact
/// `D3DKMDT_GDISURFACETYPE` supplied by Windows when the standard allocation
/// type is `D3DKMDT_STANDARDALLOCATION_GDISURFACE`.
pub const HELIOS_WDDM_ALLOC_MISC_GDI_TYPE_MASK: u32 = 0x00F0_0000;
pub const HELIOS_WDDM_ALLOC_MISC_GDI_TYPE_SHIFT: u32 = 20;

/// Private driver data for one allocation created via `D3DKMTCreateAllocation`.
///
/// `blob_id` is the venus device-memory id that backs a HOST3D mappable blob (0
/// for a scratch shmem ring); `blob_mem`/`blob_flags` mirror the
/// `VIRTIO_GPU_BLOB_MEM_*` / `VIRTIO_GPU_BLOB_FLAG_*` the KMD forwards to
/// `VirtioGpu::resource_create_blob`. 48 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosWddmAllocPrivate {
    pub blob_id: u64, // in:  venus device-memory id backing the blob (0 = scratch shmem)
    pub size: u64,    // in:  blob size in bytes
    pub magic: u32,   // == HELIOS_WDDM_MAGIC
    pub version: u32, // == HELIOS_WDDM_VERSION
    pub blob_mem: u32, // in:  VIRTIO_GPU_BLOB_MEM_* (HOST3D)
    pub blob_flags: u32, // in:  VIRTIO_GPU_BLOB_FLAG_* (USE_MAPPABLE)
    pub ctx_id: u32,  // in:  owning venus context id
    pub map_cache: u32, // in/out: requested/effective VIRTIO_GPU_MAP_CACHE_*
    pub kind: u32,    // in:  HELIOS_WDDM_ALLOC_KIND_*
    /// Optional existing virtio resource id for the KMD to adopt instead of
    /// creating a new one (0 = create). Named `_pad` until R805, which is why
    /// several comments in this tree still call the scheme "smuggling": the
    /// name advertised the field as unused while it was load-bearing, so an
    /// edit that zeroed it, reordered it, or inserted a field before it would
    /// have silently detached every device-memory allocation from its venus
    /// resource with the `size_of == 48` assert still passing.
    pub adopt_resource_id: u32,
}

impl HeliosWddmAllocPrivate {
    pub const fn new(
        kind: u32,
        ctx_id: u32,
        blob_id: u64,
        size: u64,
        blob_mem: u32,
        blob_flags: u32,
        map_cache: u32,
        adopt_resource_id: u32,
    ) -> Self {
        Self {
            blob_id,
            size,
            magic: HELIOS_WDDM_MAGIC,
            version: HELIOS_WDDM_VERSION,
            blob_mem,
            blob_flags,
            ctx_id,
            map_cache,
            kind,
            adopt_resource_id,
        }
    }

    /// Validate magic + version. The KMD calls this before trusting any field.
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.magic == HELIOS_WDDM_MAGIC && self.version == HELIOS_WDDM_VERSION
    }
}

/// Header at the start of a `D3DKMTRender` command buffer. The opaque venus byte
/// stream begins at `venus_offset` and runs for `venus_size` bytes; the KMD
/// forwards exactly those bytes to `submit_venus(ctx_id, fence, stream)`.
///
/// `ring_idx` is the venus per-queue host timeline (0 = CPU/primary ring), same
/// meaning as [`crate::escape::HeliosEscapeSubmitVenus::ring_idx`]. `seq` is an
/// ICD-side monotonically increasing submission sequence for debugging/ordering;
/// the authoritative GPU fence is WDDM's `SubmissionFenceId`. 32 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosWddmCmdBuf {
    pub seq: u64,          // in:  ICD submission sequence (debug/ordering)
    pub magic: u32,        // == HELIOS_WDDM_MAGIC
    pub version: u32,      // == HELIOS_WDDM_VERSION
    pub ctx_id: u32,       // in:  owning venus context id
    pub ring_idx: u32,     // in:  venus per-queue host timeline (0 = CPU ring)
    pub venus_offset: u32, // in:  byte offset of the venus stream within the command buffer
    pub venus_size: u32,   // in:  venus stream length in bytes
}

impl HeliosWddmCmdBuf {
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.magic == HELIOS_WDDM_MAGIC && self.version == HELIOS_WDDM_VERSION
    }
}

/// Geometry + venus-identity trailer appended after [`HeliosWddmAllocPrivate`]
/// in an allocation's create-time private driver data.
///
/// Two writers, one definition (this struct replaces the former per-crate
/// `StandardAllocMeta` copies in the KMD and UMD):
///   - `DxgkDdiGetStandardAllocationDriverData` writes it for OS-requested
///     standard allocations (shared primary / shadow / staging / GDI surfaces);
///     the KMD then fills `venus_alloc_size`/`memory_type_index` at
///     `DxgkDdiCreateAllocation` once the kernel venus client has allocated the
///     backing memory (create-time write-back into the per-allocation buffer).
///   - The UMD writes it for shared DXVK/Venus textures it allocates via
///     `pfnAllocateCb`, filling `venus_alloc_size`/`memory_type_index` from the
///     creating process's `vkAllocateMemory` parameters.
///
/// `venus_alloc_size`/`memory_type_index` exist so a cross-process opener can
/// import the backing venus memory with the creator's EXACT allocation size and
/// memory type — vkr's OPAQUE-fd import requires an exact-size match, and the
/// memory type must be one the host driver accepts for the exported handle.
/// 48 bytes, padding-free (six u32 = 24 bytes, then `venus_alloc_size` u64 lands
/// 8-aligned; two u32 close the 40-byte core; `plane_offset` u64 lands 8-aligned
/// at offset 40 for a 48-byte total).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, Pod, Zeroable)]
pub struct HeliosWddmAllocMeta {
    pub width: u32,
    pub height: u32,
    pub format: u32, // D3DDDIFORMAT
    pub pitch: u32,
    pub bind_flags: u32,
    pub misc_flags: u32,
    /// Exact `VkMemoryAllocateInfo::allocationSize` of the backing venus memory
    /// as encoded by the creator (0 = unknown; opener falls back to blob size).
    pub venus_alloc_size: u64,
    /// Creator's venus `memoryTypeIndex` for the backing memory.
    pub memory_type_index: u32,
    /// Exact `DXGI_FORMAT` the creator built the resource with (0 = unknown, e.g.
    /// KMD standard allocations and legacy trailers — the opener then falls back
    /// to translating the D3DDDIFORMAT `format` field). Carrying the DXGI format
    /// verbatim is required because the `format` field is a *lossy* D3DDDIFORMAT
    /// (the KMD reports it to dxgkrnl from `DxgkDdiDescribeAllocation`), and the
    /// D3DDDIFORMAT<->DXGI stubs collapse every non-BGRA surface to BGRA — which
    /// made openers rebuild e.g. A8 masks as 4bpp BGRA and refuse the (correctly
    /// sized, smaller) import. Occupies the former `reserved` u32, so the on-wire
    /// layout is unchanged and older KMD binaries (which zero it) stay compatible.
    pub dxgi_format: u32,
    /// Byte offset of the scan-out image within the backing allocation, from
    /// `vkGetImageSubresourceLayout` for a queryable LINEAR/modifier image
    /// (0 for every non-scan-out surface and for layouts whose data starts at
    /// offset 0). `SET_SCANOUT_BLOB` adds this to the blob base so the host reads
    /// the first row at the right place. Grows the trailer by 8 bytes; the UMD
    /// and KMD are rebuilt together, so the on-wire layout stays in sync.
    pub plane_offset: u64,
}

/// `'HIDN'` — magic of [`HeliosWddmOpenIdentity`].
pub const HELIOS_WDDM_IDENTITY_MAGIC: u32 = 0x4849_444E;
/// Current open-identity ABI version.
pub const HELIOS_WDDM_IDENTITY_VERSION: u32 = 1;

/// Allocation identity record the KMD writes into the OPEN-time private driver
/// data in `DxgkDdiOpenAllocation`, overwriting the first 48 bytes (the
/// [`HeliosWddmAllocPrivate`] region — the [`HeliosWddmAllocMeta`] trailer at
/// bytes 48.. is left as the creator wrote it). This replaces the old scheme of
/// smuggling the venus resource id through the field then named
/// `HeliosWddmAllocPrivate::_pad` (now `adopt_resource_id`; R805).
///
/// The KMD only writes this record after validating `resource_id` against its
/// live-resource table (the KMD owns the resid namespace: every create and every
/// unref goes through it), so a parsing opener may trust that the resource was
/// alive at open time. An open of an allocation whose venus resource is dead
/// FAILS in the KMD instead of producing this record. 48 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosWddmOpenIdentity {
    /// Exact creator-side `vkAllocateMemory` size for the import (never 0;
    /// falls back to the page-rounded blob size when the creator did not
    /// record one).
    pub venus_alloc_size: u64,
    /// Page-rounded virtio-gpu blob size (the WDDM allocation size).
    pub blob_size: u64,
    pub magic: u32,   // == HELIOS_WDDM_IDENTITY_MAGIC
    pub version: u32, // == HELIOS_WDDM_IDENTITY_VERSION
    /// Venus/virtio resource id backing the allocation; live at open time.
    pub resource_id: u32,
    /// Creator's venus `memoryTypeIndex` for the backing memory.
    pub memory_type_index: u32,
    /// Creating venus context id (diagnostic only).
    pub ctx_id: u32,
    /// Creator's `HELIOS_WDDM_ALLOC_KIND_*`.
    pub kind: u32,
    pub reserved: [u32; 2],
}

impl HeliosWddmOpenIdentity {
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.magic == HELIOS_WDDM_IDENTITY_MAGIC && self.version == HELIOS_WDDM_IDENTITY_VERSION
    }
}

/// `'HPRS'` — magic of [`HeliosPresentPrivateData`].
pub const HELIOS_PRESENT_PRIVATE_MAGIC: u32 = 0x4850_5253;
/// Current present-private-data ABI version. Appended tails are guarded by
/// their exact flags and command coverage, so this remains prefix-compatible
/// with mixed UMD/KMD deployment.
pub const HELIOS_PRESENT_PRIVATE_VERSION: u32 = 1;
/// `HeliosPresentPrivateData::reserved` bit: the payload came from the exact
/// exportable `pPrimaryDesc` allocation and may be scanned out directly. The
/// bit does not promise a LINEAR layout; the host validates native metadata.
pub const HELIOS_PRESENT_PRIVATE_FLAG_DIRECT_SCANOUT: u32 = 1 << 0;
/// `HeliosPresentPrivateData::reserved` bit (D4b snapshot,
/// FIX-DESIGN-d4b-snapshot.md): the descriptor fields (`resource_id`, the
/// geometry, and `venus_alloc_size`) describe a SNAPSHOT image the UMD's
/// venus-queue-ordered blit filled from the presented primary — the KMD
/// should bind/flush THAT resource on the DMA-flip path instead of the
/// flipped allocation's own. The flip bookkeeping (epoch stamp, VidMm
/// physical address, CRTC_VSYNC retirement) stays entirely on the flipped
/// allocation. A reader may only trust `venus_alloc_size` when this bit is
/// set AND the buffer covers the 48-byte extended struct; readers that
/// predate the bit ignore it and behave exactly as before (and the UMD only
/// sets it after the KMD advertised `HELIOS_SCANOUT_CAP_SNAPSHOT_BIND`).
pub const HELIOS_PRESENT_PRIVATE_FLAG_SNAPSHOT: u32 = 1 << 1;
/// The snapshot is the exact source for a windowed `DXGK_PRESENTFLAGS.Blt`.
/// It MUST NOT be consumed by the direct-flip binding arm.  The inverse is
/// also enforced: a direct-flip snapshot never enters the windowed BLT FIFO.
pub const HELIOS_PRESENT_PRIVATE_FLAG_WINDOWED_BLT_SNAPSHOT: u32 = 1 << 2;
/// `'HEPR'` — magic of [`HeliosPresentRenderCmd`].
pub const HELIOS_PRESENT_RENDER_MAGIC: u32 = 0x5250_4548;
/// Current inline present-render command ABI version.
pub const HELIOS_PRESENT_RENDER_VERSION: u32 = 1;
/// `'HERF'` — magic of [`HeliosPresentRefreshCmd`].  This is deliberately
/// distinct from [`HELIOS_PRESENT_RENDER_MAGIC`]: the legacy 16-byte present
/// marker used to start with `HEPR`, so `DxgkDdiRender` could misread the
/// runtime's larger command-buffer capacity (including stale tail bytes) as a
/// valid 48-byte typed scanout command.
pub const HELIOS_PRESENT_REFRESH_MAGIC: u32 = 0x4652_4548;
/// Current stable-scanout refresh command ABI version.
pub const HELIOS_PRESENT_REFRESH_VERSION: u32 = 1;

/// Private payload carried through `DXGIDDICB_PRESENT::pPrivateDriverData` to
/// `DxgkDdiPresent`.
///
/// This is a normal DXGI present callback contract field, not an escape path. It
/// is used when dxgkrnl calls the KMD present hook without populating source
/// allocation-list entries for the composed DWM scanout allocation.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosPresentPrivateData {
    pub plane_offset: u64,
    pub magic: u32,
    pub version: u32,
    pub resource_id: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub dxgi_format: u32,
    pub reserved: u32,
    /// Total venus blob size backing `resource_id`, for the bind-time
    /// undersize guard (`venus_alloc_size >= plane_offset + pitch*height`).
    /// APPENDED for [`HELIOS_PRESENT_PRIVATE_FLAG_SNAPSHOT`] (40 -> 48 bytes,
    /// offset 40 is 8-aligned, nothing before it moves — prefix-compatible
    /// with pre-snapshot readers). Meaningful ONLY when that flag is set;
    /// pre-snapshot writers leave it absent and readers must not consult it
    /// without the flag + a 48-byte length check.
    pub venus_alloc_size: u64,
    /// Optional registered present-stream marker.  Readers must only inspect
    /// this appended tail when their input covers the complete 64-byte form;
    /// the v1 prefix through `venus_alloc_size` remains the compatibility and
    /// snapshot-coverage boundary.
    pub present_ctx_id: u32,
    pub present_value: u32,
    pub present_cookie: u64,
    /// Exact `VkMemoryAllocateInfo::memoryTypeIndex` recorded for a snapshot
    /// source. Meaningful only with `FLAG_WINDOWED_BLT_SNAPSHOT` and only when
    /// the command covers this complete v2 tail.
    pub snapshot_memory_type_index: u32,
    /// Reserved-zero except for `HELIOS_PRESENT_SNAPSHOT_PURPOSE_WINDOWED_BLT`.
    /// Keeping the purpose scalar makes a flagged malformed/private-data tail a
    /// refusal rather than an implicit direct-vs-windowed inference.
    pub snapshot_purpose: u32,
}

pub const HELIOS_PRESENT_SNAPSHOT_PURPOSE_NONE: u32 = 0;
pub const HELIOS_PRESENT_SNAPSHOT_PURPOSE_WINDOWED_BLT: u32 = 1;

impl HeliosPresentPrivateData {
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.magic == HELIOS_PRESENT_PRIVATE_MAGIC
            && self.version == HELIOS_PRESENT_PRIVATE_VERSION
            && self.resource_id != 0
    }
}

/// Tiny command submitted through `pfnRenderCb` before `pfnPresentCb`.
/// The payload is created only for the exact `pPrimaryDesc` allocation and is
/// consumed by `DxgkDdiRender`; ordinary application presents use the legacy
/// four-byte marker and cannot enter the scanout path.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosPresentRenderCmd {
    pub magic: u32,
    pub version: u32,
    pub present: HeliosPresentPrivateData,
}

impl HeliosPresentRenderCmd {
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.magic == HELIOS_PRESENT_RENDER_MAGIC
            && self.version == HELIOS_PRESENT_RENDER_VERSION
            && self.present.is_valid()
    }
}

/// Per-present marker emitted by `DxgkDdiPresent` for ordinary desktop
/// presents.  It never identifies or rebinds an allocation; `DxgkDdiRender`
/// only uses it to dirty the adapter-owned, already-bound LINEAR scanout after
/// the UMD has submitted the desktop copy.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosPresentRefreshCmd {
    pub magic: u32,
    pub version: u32,
    /// RESERVED-ZERO on the UMD path, and documented as such by R828 because
    /// the name suggests otherwise. The UMD writes 0 here;
    /// `kmd_render/src/ddi/submit_command.rs` validates only the magic and
    /// version and reads NEITHER index. The KMD builds its own copy with real
    /// `DXGK_PRESENT_*_INDEX` values in `ddi/display.rs`. Populating these from
    /// the UMD would be a wire-semantics change with no reader.
    pub source_index: u32,
    /// RESERVED-ZERO on the UMD path. See [`Self::source_index`].
    pub destination_index: u32,
    /// Optional registered present-stream marker.  A complete nonzero tail
    /// selects a stream boundary; an absent, partial, or invalid tail follows
    /// the legacy current-wire watermark path.
    pub present_ctx_id: u32,
    pub present_value: u32,
    pub present_cookie: u64,
}

impl HeliosPresentRefreshCmd {
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.magic == HELIOS_PRESENT_REFRESH_MAGIC && self.version == HELIOS_PRESENT_REFRESH_VERSION
    }
}

const _: () = {
    assert!(core::mem::size_of::<HeliosWddmAllocPrivate>() == 48);
    assert!(core::mem::size_of::<HeliosWddmCmdBuf>() == 32);
    assert!(core::mem::size_of::<HeliosWddmAllocMeta>() == 48);
    // 40 -> 48 with the appended `venus_alloc_size` (D4b snapshot); the
    // 40-byte prefix layout is unchanged — pre-snapshot readers keep working.
    assert!(core::mem::size_of::<HeliosPresentPrivateData>() == 72);
    assert!(core::mem::offset_of!(HeliosPresentPrivateData, venus_alloc_size) == 40);
    assert!(core::mem::offset_of!(HeliosPresentPrivateData, present_ctx_id) == 48);
    assert!(core::mem::offset_of!(HeliosPresentPrivateData, snapshot_memory_type_index) == 64);
    assert!(core::mem::size_of::<HeliosPresentRenderCmd>() == 80);
    assert!(core::mem::size_of::<HeliosPresentRefreshCmd>() == 32);
    assert!(core::mem::offset_of!(HeliosPresentRefreshCmd, present_ctx_id) == 16);
    // The identity record must fit exactly over the HeliosWddmAllocPrivate
    // region so the meta trailer's offset is unchanged for openers.
    assert!(
        core::mem::size_of::<HeliosWddmOpenIdentity>()
            == core::mem::size_of::<HeliosWddmAllocPrivate>()
    );
};

// ── Allocation backing classification ────────────────────────────────────────

/// `D3DDDIFMT_A8R8G8B8`, the BGRA D3DDDIFORMAT the GDI surface path accepts.
///
/// Duplicated from the WDK enum on purpose: this crate has no bindgen edge, and
/// the value is a frozen Windows ABI constant.
pub const D3DDDIFMT_A8R8G8B8: u32 = 21;
/// `D3DDDIFMT_X8R8G8B8`.
pub const D3DDDIFMT_X8R8G8B8: u32 = 22;

/// How one allocation's backing store is produced.
///
/// The backing class used to be four booleans (`adopt_supplied_resource`,
/// `is_primary`, `is_optimal_gdi_texture`, `ap.kind == STANDARD`) evaluated as
/// an ordered if/else chain, with each arm mutating the shared `meta`/`ap`
/// locals in place and the write-back happening 100-470 lines later. Nothing
/// stated which fields each arm had to set — the per-arm field obligation
/// behind the historical pitch shear (`width*4` = 7584 vs the real 7680) and
/// the import-size mismatches, enforced only by prose.
///
/// As an exhaustive value, adding a class fails to compile until it is handled,
/// and the ordering between classes is stated once here instead of being an
/// emergent property of an if/else chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocationBacking {
    /// A live virtio resource created by the UMD/ICD, adopted by this
    /// allocation. `take_ownership` re-owns the blob slot from the ICD's escape
    /// owner; otherwise the resource is only liveness-validated.
    AdoptedUmdResource {
        resource_id: u32,
        take_ownership: bool,
    },
    /// The VidPn primary: a KMD-created LINEAR scanout image blob.
    KmdLinearPrimary { width: u32, height: u32 },
    /// `D3DKMDT_GDISURFACE_TEXTURE`: a KMD-created cross-context OPTIMAL image.
    KmdOptimalGdiTexture {
        width: u32,
        height: u32,
        dxgi_format: u32,
        bind_flags: u32,
    },
    /// Any other runtime standard allocation: a KMD-created mappable
    /// `VkDeviceMemory` blob.
    KmdStandardBuffer { size: u64, primary: bool },
    /// A raw HOST3D blob created straight from the ICD's parameters.
    RawHost3dBlob {
        ctx_id: u32,
        blob_mem: u32,
        blob_flags: u32,
        blob_id: u64,
        size: u64,
    },
}

/// Why [`classify`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassifyRefusal {
    /// A `D3DKMDT_GDISURFACE_TEXTURE` in a D3DDDIFORMAT with no BGRA DXGI
    /// equivalent. Reinterpreting it would make DWM sample the wrong shape.
    UnsupportedGdiFormat { format: u32 },
}

/// Decide one allocation's backing class. Pure: no adapter, no host round-trip,
/// no side effect — which is what makes the five arms unit-testable.
///
/// `supplied_resource_id` is the id the UMD passed in `adopt_resource_id`, already filtered
/// by kind (STANDARD and DEVICE_MEMORY carry one; SHMEM does not).
///
/// ORDER IS LOAD-BEARING and reproduces the old if/else chain exactly:
/// adopt wins over everything, then primary-standard, then GDI texture, then
/// any other standard, then the raw blob. In particular a STANDARD allocation
/// flagged BOTH `PRIMARY` and `OPTIMAL_GDI_TEXTURE` classifies as the primary —
/// `GetStandardAllocationDriverData`'s own if/else makes that combination
/// unreachable, and the test below pins it so a future writer cannot change the
/// meaning by accident.
pub fn classify(
    ap: &HeliosWddmAllocPrivate,
    meta: &HeliosWddmAllocMeta,
    supplied_resource_id: u32,
) -> Result<AllocationBacking, ClassifyRefusal> {
    if supplied_resource_id != 0 {
        return Ok(AllocationBacking::AdoptedUmdResource {
            resource_id: supplied_resource_id,
            // Only DEVICE_MEMORY adopts take the blob's lifetime; anything else
            // is liveness-validated only.
            take_ownership: ap.kind == HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY,
        });
    }
    let is_standard = ap.kind == HELIOS_WDDM_ALLOC_KIND_STANDARD;
    let is_primary = (meta.misc_flags & HELIOS_WDDM_ALLOC_MISC_PRIMARY) != 0;
    if is_standard && is_primary {
        return Ok(AllocationBacking::KmdLinearPrimary {
            width: meta.width,
            height: meta.height,
        });
    }
    if is_standard && (meta.misc_flags & HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE) != 0 {
        let dxgi_format = match meta.format {
            f if f == D3DDDIFMT_A8R8G8B8 => 87,
            f if f == D3DDDIFMT_X8R8G8B8 => 88,
            f => return Err(ClassifyRefusal::UnsupportedGdiFormat { format: f }),
        };
        return Ok(AllocationBacking::KmdOptimalGdiTexture {
            width: meta.width,
            height: meta.height,
            dxgi_format,
            bind_flags: meta.bind_flags,
        });
    }
    if is_standard {
        return Ok(AllocationBacking::KmdStandardBuffer {
            size: ap.size,
            primary: is_primary,
        });
    }
    Ok(AllocationBacking::RawHost3dBlob {
        ctx_id: ap.ctx_id,
        blob_mem: ap.blob_mem,
        blob_flags: ap.blob_flags,
        blob_id: ap.blob_id,
        size: ap.size,
    })
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    fn private(kind: u32) -> HeliosWddmAllocPrivate {
        // Trailing 0 = adopt_resource_id, which the constructor gained in R805.
        // The tests that exercise adoption set the field explicitly below, as
        // they did when it was `_pad`.
        HeliosWddmAllocPrivate::new(kind, 7, 0xBEEF, 4096, 3, 1, 0, 0)
    }

    fn meta(misc_flags: u32) -> HeliosWddmAllocMeta {
        HeliosWddmAllocMeta {
            width: 1896,
            height: 1030,
            format: D3DDDIFMT_A8R8G8B8,
            pitch: 7680,
            bind_flags: 0x28,
            misc_flags,
            venus_alloc_size: 0,
            memory_type_index: 0,
            dxgi_format: 0,
            plane_offset: 0,
        }
    }

    /// An adopted resource wins over every flag, and only a DEVICE_MEMORY adopt
    /// takes ownership.
    #[test]
    fn adopt_wins_and_only_device_memory_takes_ownership() {
        assert_eq!(
            classify(
                &private(HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY),
                &meta(HELIOS_WDDM_ALLOC_MISC_PRIMARY),
                42
            ),
            Ok(AllocationBacking::AdoptedUmdResource {
                resource_id: 42,
                take_ownership: true
            })
        );
        assert_eq!(
            classify(
                &private(HELIOS_WDDM_ALLOC_KIND_STANDARD),
                &meta(HELIOS_WDDM_ALLOC_MISC_PRIMARY),
                42
            ),
            Ok(AllocationBacking::AdoptedUmdResource {
                resource_id: 42,
                take_ownership: false
            })
        );
    }

    #[test]
    fn standard_primary_is_the_linear_scanout_image() {
        assert_eq!(
            classify(
                &private(HELIOS_WDDM_ALLOC_KIND_STANDARD),
                &meta(HELIOS_WDDM_ALLOC_MISC_PRIMARY),
                0
            ),
            Ok(AllocationBacking::KmdLinearPrimary {
                width: 1896,
                height: 1030
            })
        );
    }

    #[test]
    fn gdi_texture_maps_both_bgra_formats_and_refuses_the_rest() {
        let m = meta(HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE);
        assert_eq!(
            classify(&private(HELIOS_WDDM_ALLOC_KIND_STANDARD), &m, 0),
            Ok(AllocationBacking::KmdOptimalGdiTexture {
                width: 1896,
                height: 1030,
                dxgi_format: 87,
                bind_flags: 0x28,
            })
        );
        let mut m2 = m;
        m2.format = D3DDDIFMT_X8R8G8B8;
        assert!(matches!(
            classify(&private(HELIOS_WDDM_ALLOC_KIND_STANDARD), &m2, 0),
            Ok(AllocationBacking::KmdOptimalGdiTexture {
                dxgi_format: 88,
                ..
            })
        ));
        let mut m3 = m;
        m3.format = 999;
        assert_eq!(
            classify(&private(HELIOS_WDDM_ALLOC_KIND_STANDARD), &m3, 0),
            Err(ClassifyRefusal::UnsupportedGdiFormat { format: 999 })
        );
    }

    /// PRIMARY | OPTIMAL_GDI_TEXTURE must stay unreachable in practice, and it
    /// must resolve to the PRIMARY arm if it ever does occur — which is what the
    /// old ordered if/else did.
    #[test]
    fn primary_beats_gdi_texture_when_both_bits_are_set() {
        assert_eq!(
            classify(
                &private(HELIOS_WDDM_ALLOC_KIND_STANDARD),
                &meta(HELIOS_WDDM_ALLOC_MISC_PRIMARY | HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE),
                0
            ),
            Ok(AllocationBacking::KmdLinearPrimary {
                width: 1896,
                height: 1030
            })
        );
    }

    #[test]
    fn plain_standard_is_a_kmd_buffer_and_everything_else_is_a_raw_blob() {
        assert_eq!(
            classify(&private(HELIOS_WDDM_ALLOC_KIND_STANDARD), &meta(0), 0),
            Ok(AllocationBacking::KmdStandardBuffer {
                size: 4096,
                primary: false
            })
        );
        assert_eq!(
            classify(&private(HELIOS_WDDM_ALLOC_KIND_SHMEM), &meta(0), 0),
            Ok(AllocationBacking::RawHost3dBlob {
                ctx_id: 7,
                blob_mem: 3,
                blob_flags: 1,
                blob_id: 0xBEEF,
                size: 4096,
            })
        );
        // A DEVICE_MEMORY allocation with no supplied id is also a raw blob.
        assert_eq!(
            classify(&private(HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY), &meta(0), 0),
            Ok(AllocationBacking::RawHost3dBlob {
                ctx_id: 7,
                blob_mem: 3,
                blob_flags: 1,
                blob_id: 0xBEEF,
                size: 4096,
            })
        );
    }
}
