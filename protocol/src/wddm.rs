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
/// self-fills this private data in `DxgkDdiGetStandardAllocationDriverData` and
/// self-backs it with a host-allocated HOST3D mappable blob (`blob_id == 0`,
/// `ctx_id` = the KMD's internal venus context). The geometry the runtime supplied
/// (width/height/format) is appended after this struct as a KMD-private trailer.
pub const HELIOS_WDDM_ALLOC_KIND_STANDARD: u32 = 2;

/// Private driver data for one allocation created via `D3DKMTCreateAllocation`.
///
/// `blob_id` is the venus device-memory id that backs a HOST3D mappable blob (0
/// for a scratch shmem ring); `blob_mem`/`blob_flags` mirror the
/// `VIRTIO_GPU_BLOB_MEM_*` / `VIRTIO_GPU_BLOB_FLAG_*` the KMD forwards to
/// `VirtioGpu::resource_create_blob`. 48 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosWddmAllocPrivate {
    pub blob_id: u64,    // in:  venus device-memory id backing the blob (0 = scratch shmem)
    pub size: u64,       // in:  blob size in bytes
    pub magic: u32,      // == HELIOS_WDDM_MAGIC
    pub version: u32,    // == HELIOS_WDDM_VERSION
    pub blob_mem: u32,   // in:  VIRTIO_GPU_BLOB_MEM_* (HOST3D)
    pub blob_flags: u32, // in:  VIRTIO_GPU_BLOB_FLAG_* (USE_MAPPABLE)
    pub ctx_id: u32,     // in:  owning venus context id
    pub map_cache: u32,  // in/out: requested/effective VIRTIO_GPU_MAP_CACHE_*
    pub kind: u32,       // in:  HELIOS_WDDM_ALLOC_KIND_*
    pub _pad: u32,       // in:  optional existing virtio resource id to adopt
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
            _pad: 0,
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
/// 40 bytes, padding-free (six u32 = 24 bytes, so the u64 lands 8-aligned).
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
}

/// `'HIDN'` — magic of [`HeliosWddmOpenIdentity`].
pub const HELIOS_WDDM_IDENTITY_MAGIC: u32 = 0x4849_444E;
/// Current open-identity ABI version.
pub const HELIOS_WDDM_IDENTITY_VERSION: u32 = 1;

/// Allocation identity record the KMD writes into the OPEN-time private driver
/// data in `DxgkDdiOpenAllocation`, overwriting the first 48 bytes (the
/// [`HeliosWddmAllocPrivate`] region — the [`HeliosWddmAllocMeta`] trailer at
/// bytes 48.. is left as the creator wrote it). This replaces the old scheme of
/// smuggling the venus resource id through `HeliosWddmAllocPrivate::_pad`.
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
        self.magic == HELIOS_WDDM_IDENTITY_MAGIC
            && self.version == HELIOS_WDDM_IDENTITY_VERSION
    }
}

const _: () = {
    assert!(core::mem::size_of::<HeliosWddmAllocPrivate>() == 48);
    assert!(core::mem::size_of::<HeliosWddmCmdBuf>() == 32);
    assert!(core::mem::size_of::<HeliosWddmAllocMeta>() == 40);
    // The identity record must fit exactly over the HeliosWddmAllocPrivate
    // region so the meta trailer's offset is unchanged for openers.
    assert!(
        core::mem::size_of::<HeliosWddmOpenIdentity>()
            == core::mem::size_of::<HeliosWddmAllocPrivate>()
    );
};
