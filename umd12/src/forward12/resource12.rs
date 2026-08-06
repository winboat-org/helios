//! L4 — resources, heaps, residency and introspection.
//!
//! Owns 16 of `DEVICE_FUNCS_CORE_0109` (groups (g) 11, (h) 5), verified against
//! `DDI_REFERENCE.md` §3.2 and against `d3d12umddi.h`'s own member order in
//! `umd12/bindgen/cached/d3d12umddi.rs:54702-54723`.
//!
//! # ⭐ The shape of this lane: one fused create DDI, three public APIs
//!
//! `pfnCreateHeapAndResource` is ONE slot doing THREE jobs, and the arm is
//! selected by which of its two `_In_opt_` argument pointers are non-NULL
//! (`DDI_REFERENCE.md` §7.3(1), from `ResourceHeaps.md:1180`):
//!
//! | `pCreateHeap` | `pCreateResource` | public API | this file |
//! |---|---|---|---|
//! | non-NULL | non-NULL | `CreateCommittedResource` | [`create_committed`] |
//! | non-NULL | NULL | `CreateHeap` | [`create_heap_only`] |
//! | NULL | non-NULL | `CreatePlacedResource` / `CreateReservedResource` | [`create_placed_or_reserved`] |
//! | NULL | NULL | illegal | refused, `HeapResourceCreateBadArg` |
//!
//! ⛔ CLAUDE.md's *"validate every runtime-supplied size & offset per-arm, not
//! max-union"* applies literally here: the RenderGdi ~48 % drop bug was exactly
//! this mistake in D3D11. Each arm below reads only the fields its own arm
//! guarantees.
//!
//! ⭐ **Placed resources do not name a heap.** `ResourceHeaps.md:1212`, mined in
//! `SPECS.md` §9.7: *"The D3D12 DDI carries no heap-handle-plus-heap-offset
//! placement parameter. The only placement parent expressible in
//! `D3D12DDIARG_CREATERESOURCE_0109` is `ReuseBufferGPUVA`"* — a
//! `D3D12DDIARG_HRESOURCE_PLACEMENT { D3D12DDI_HRESOURCE hResource; UINT64
//! Offset }`. And *"a RESERVED resource is the resource-args-only arm with
//! `ReuseBufferGPUVA.BaseAddress.UMD.hResource == NULL`"* (`:1204`). So the
//! parent of a placed resource is another **resource handle**, and the engine
//! heap has to be reachable *through* it. That is why [`ResourceState`] carries
//! a [`HeapSpan`]: every object this file creates inside an engine heap records
//! which heap and at what offset, so a placement chain resolves without the DDI
//! ever naming the heap.
//!
//! # ⭐ The engine is reached through `ID3D12Device10`, not `ID3D12Device`
//!
//! `D3D12DDIARG_CREATERESOURCE_0109` carries `InitialBarrierLayout`,
//! `SamplerFeedbackMipRegion` and `NumCastableFormats`/`pCastableFormats`. Those
//! three fields exist on exactly one API entry point family —
//! `ID3D12Device10::CreateCommittedResource3` / `CreatePlacedResource2`, which
//! take `D3D12_RESOURCE_DESC1` (= `D3D12_RESOURCE_DESC` + the mip region) plus a
//! `D3D12_BARRIER_LAYOUT` and a castable-format list. The older
//! `CreateCommittedResource` would force this driver to invent a legacy
//! `D3D12_RESOURCE_STATES` and to silently drop two fields the runtime handed
//! it. vkd3d implements the whole family (`libs/vkd3d/device.c:9408`, `:9448`)
//! and its `QueryInterface` accepts `IID_ID3D12Device10` (`device.c:4647`).
//!
//! ⚠ Everything here is reachable from Rust through the borrowed `ID3D12Device`
//! the bridge already exposes. **No cxx bridge module was needed or added**, so
//! this whole lane type-checks on the Linux host (`PARALLEL.md` §7).
//!
//! # ⛔ Three ABI traps in the DDI-to-API translation, all of them silent
//!
//! 1. **`D3D12DDI_MEMORY_POOL` is offset by one from `D3D12_MEMORY_POOL`.** DDI
//!    `L0 = 0, L1 = 1` (`d3d12umddi.rs:48255-48256`); API `UNKNOWN = 0, L0 = 1,
//!    L1 = 2`. A blind cast turns "system memory" into "unknown".
//! 2. **`D3D12DDI_CPU_PAGE_PROPERTY` is offset by one too.** DDI
//!    `NOT_AVAILABLE = 0, WRITE_COMBINE = 1, WRITE_BACK = 2`
//!    (`d3d12umddi.rs:48248-48253`); API `UNKNOWN = 0, NOT_AVAILABLE = 1,
//!    WRITE_COMBINE = 2, WRITE_BACK = 3`. Same failure, worse: a
//!    not-CPU-visible heap becomes a driver-chooses heap.
//! 3. **The heap and resource flag enums are not the same bits, and two of them
//!    are INVERTED.** The DDI hands positive ALLOW-style heap bits
//!    (`NON_RT_DS_TEXTURES = 0x2`, `BUFFERS = 0x4`, `RT_DS_TEXTURES = 0x20`)
//!    where the API takes DENY bits (`ResourceHeaps.md:874`), and
//!    `D3D12DDI_RESOURCE_FLAG_0003_SHADER_RESOURCE = 0x10` is the positive form
//!    of the API's `DENY_SHADER_RESOURCE = 0x8`.
//!
//! ⇒ every enum crossing this boundary goes through an explicit `match`, never a
//! cast. That is the same finding the 80th memory records for formats: D3D11
//! harmonised its DDI enums with the API's and D3D12 did **not**.
//!
//! # ⛔ `DECISIONS.md` D13 — and why this lane now DOES take the dependency
//!
//! D13 binds this lane hardest: private data that CROSSES a module boundary is
//! declared once, in `helios_protocol`, and reused verbatim —
//! `HeliosWddmAllocPrivate` (`'HWDM'`), `HeliosWddmAllocMeta`,
//! `HeliosWddmOpenIdentity` (`'HIDN'`), `HeliosPresentPrivateData` and
//! `HeliosPresentRenderCmd`.
//!
//! ⚠ **This block used to say the opposite**, and the reversal is recorded
//! rather than quietly edited. It said this lane *"declares no such record and
//! takes no `helios_protocol` dependency, because it writes none: it mints no
//! WDDM allocation"* — vkd3d's memory being minted by the Mesa venus ICD
//! through its own `D3DKMT` path, so this driver never calls `pfnAllocateCb`.
//! Every clause of that was true of L4 as shipped, and the conclusion is now
//! **wrong**, because `KMD_IMPACT.md` §14a.3 settled what the D3D12 present path
//! actually needs: not the ICD handing over a `D3DKMT_HANDLE` (it has none that
//! means anything — its only `D3DKMTCreateAllocation2` mints a
//! `kind = TRACKING` VidMm charge the KMD forbids from carrying identity,
//! `create_allocation.rs:2333-2344`), and not this driver allocating and the ICD
//! importing (backwards), but the third shape, the one D3D11 ships: **the engine
//! allocates the Vulkan memory and this driver ADOPTS it**, by calling
//! `pfnAllocateCb` with `HeliosWddmAllocPrivate.adopt_resource_id = <venus
//! resid>`. The KMD already accepts exactly that
//! (`create_allocation.rs:2377-2379`: `kind == DEVICE_MEMORY &&
//! adopt_resource_id != 0` → `AllocationBacking::AdoptedUmdResource`, with
//! `write_open_identity` stamping `HeliosWddmOpenIdentity` back so DWM's D3D11
//! opener works unchanged) — so there is no new allocation shape and no new KMD
//! verb, only a writer this lane did not have. The model to mirror is
//! `umd/src/forward/resource.rs:263-324` (build the record) and `:374` (the one
//! `pfnAllocateCb` call site — there is exactly one, for four callers).
//!
//! ⇒ `PARALLEL.md` §5's *"`umd12` does not yet depend on `helios_protocol`; the
//! first lane that needs a crossing record adds it, and says so"* is discharged
//! **here**: `umd12/Cargo.toml` takes it, and this is the saying-so.
//!
//! ⚠ What is NOT yet true, stated so nothing reads more into the dependency than
//! it carries: **nothing in this crate calls `pfnAllocateCb` yet.** That is UP-5.
//! `pfnCheckResourceAllocationHandle` still answers 0 and still counts
//! (`ResourceAllocationHandleUnavailable`), and `pfnOpenHeapAndResource` and its
//! sizing call are still refused — see [`open_heap_and_resource`] for exactly
//! what is missing and why. UP-1 takes the dependency and deletes the claim that
//! it is not needed; it adds no writer and no reader of its own. The size and
//! layout asserts are not restated either — `protocol/src/wddm.rs:483-501`
//! already carries all of them, and a second copy of an assert is a second thing
//! that can drift.
//!
//! The per-object `pDrvPrivate` payloads below ([`HeapState`], [`ResourceState`])
//! are runtime-allocated, per-object, per-process and read by nothing outside
//! this DLL, which is precisely the class D13's refined form keeps local and
//! typed against `ddi12`.
//!
//! # What this lane deliberately does not do
//!
//! * **Cross-DDI / cross-process resource opening** (`pfnOpenHeapAndResource`,
//!   `pfnCalcPrivateOpenedHeapAndResourceSizes`) — refused with named counters.
//! * **Reserved (tiled) resources** — refused, because `caps12.rs:278` reports
//!   `TiledResourcesTier = NOT_SUPPORTED` and creating one would contradict the
//!   caps this device was accepted on.
//! * **Kernel allocation identity** (`pfnCheckResourceAllocationHandle`) —
//!   answered 0 with a counter; there is still no `D3DKMT_HANDLE` to give.
//!   ⚠ UP-4 adds the *bookkeeping* for one ([`identity12`], written by
//!   [`note_presentable_identity`] and retired by [`destroy_heap_and_resource`])
//!   but not the handle: minting it is UP-5's `pfnAllocateCb` call, and UP-6 is
//!   what makes this slot answer with it.
//!
//! Each is a named counter, never a silent stub (CLAUDE.md rule 2).

use core::ffi::c_void;

use helios_umd_common::hr::{Hresult, E_FAIL, E_INVALIDARG, E_NOTIMPL, S_OK};
use helios_umd_common::refusals::RefusalCounter;
use helios_umd_common::slot::{Boxed, Slot};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::{
    ID3D12Device10, ID3D12Heap, ID3D12ProtectedResourceSession, ID3D12Resource,
    D3D12_BARRIER_LAYOUT, D3D12_BARRIER_LAYOUT_COMMON, D3D12_BARRIER_LAYOUT_COPY_DEST,
    D3D12_BARRIER_LAYOUT_COPY_SOURCE, D3D12_BARRIER_LAYOUT_GENERIC_READ,
    D3D12_BARRIER_LAYOUT_SHADER_RESOURCE, D3D12_BARRIER_LAYOUT_UNDEFINED,
    D3D12_BARRIER_LAYOUT_VIDEO_QUEUE_COMMON, D3D12_CLEAR_VALUE, D3D12_CLEAR_VALUE_0,
    D3D12_CPU_PAGE_PROPERTY, D3D12_CPU_PAGE_PROPERTY_NOT_AVAILABLE,
    D3D12_CPU_PAGE_PROPERTY_WRITE_BACK, D3D12_CPU_PAGE_PROPERTY_WRITE_COMBINE,
    D3D12_DEPTH_STENCIL_VALUE, D3D12_HEAP_DESC, D3D12_HEAP_FLAGS, D3D12_HEAP_FLAG_DENY_BUFFERS,
    D3D12_HEAP_FLAG_DENY_NON_RT_DS_TEXTURES, D3D12_HEAP_FLAG_DENY_RT_DS_TEXTURES,
    D3D12_HEAP_FLAG_NONE, D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_CUSTOM, D3D12_MEMORY_POOL,
    D3D12_MEMORY_POOL_L0, D3D12_MEMORY_POOL_L1, D3D12_MIP_REGION,
    D3D12_PLACED_SUBRESOURCE_FOOTPRINT, D3D12_RESOURCE_DESC, D3D12_RESOURCE_DESC1,
    D3D12_SUBRESOURCE_FOOTPRINT, D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT,
    D3D12_RESOURCE_DIMENSION, D3D12_RESOURCE_DIMENSION_BUFFER, D3D12_RESOURCE_DIMENSION_TEXTURE1D,
    D3D12_RESOURCE_DIMENSION_TEXTURE2D, D3D12_RESOURCE_DIMENSION_TEXTURE3D, D3D12_RESOURCE_FLAGS,
    D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER, D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
    D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET, D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
    D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE,
    D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_FLAG_RAYTRACING_ACCELERATION_STRUCTURE,
    D3D12_RESOURCE_FLAG_VIDEO_DECODE_REFERENCE_ONLY,
    D3D12_RESOURCE_FLAG_VIDEO_ENCODE_REFERENCE_ONLY, D3D12_TEXTURE_LAYOUT,
    D3D12_TEXTURE_LAYOUT_64KB_STANDARD_SWIZZLE, D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE,
    D3D12_TEXTURE_LAYOUT_ROW_MAJOR, D3D12_TEXTURE_LAYOUT_UNKNOWN,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

use super::identity12;
use super::tables12::{stage, DeviceCoreTable, Filling};
use crate::device12::{self, HeliosD3D12Device};
use crate::{ddi12, log_error, note_refusal};

/// Short aliases for the bindgen enumerator names, which arrive doubled because
/// bindgen prefixes each constant with its enum's name and the SDK header
/// already did the same. Same device as `caps12`'s `mod v`: every line is a
/// compile-checked reference to the header rather than a transcribed number
/// (`ARCHITECTURE.md` §12 rule 1).
mod v {
    use crate::ddi12::*;

    // -- D3D12DDI_RESOURCE_TYPE (d3d12umddi.rs:48391-48394) -----------------
    pub(super) const RT_BUFFER: D3D12DDI_RESOURCE_TYPE = D3D12DDI_RESOURCE_TYPE_D3D12DDI_RT_BUFFER;
    pub(super) const RT_TEXTURE1D: D3D12DDI_RESOURCE_TYPE =
        D3D12DDI_RESOURCE_TYPE_D3D12DDI_RT_TEXTURE1D;
    pub(super) const RT_TEXTURE2D: D3D12DDI_RESOURCE_TYPE =
        D3D12DDI_RESOURCE_TYPE_D3D12DDI_RT_TEXTURE2D;
    pub(super) const RT_TEXTURE3D: D3D12DDI_RESOURCE_TYPE =
        D3D12DDI_RESOURCE_TYPE_D3D12DDI_RT_TEXTURE3D;

    // -- D3D12DDI_TEXTURE_LAYOUT (d3d12umddi.rs:48016-48023) ----------------
    pub(super) const TL_UNDEFINED: D3D12DDI_TEXTURE_LAYOUT =
        D3D12DDI_TEXTURE_LAYOUT_D3D12DDI_TL_UNDEFINED;
    pub(super) const TL_ROW_MAJOR: D3D12DDI_TEXTURE_LAYOUT =
        D3D12DDI_TEXTURE_LAYOUT_D3D12DDI_TL_ROW_MAJOR;
    pub(super) const TL_64KB_TILE_UNDEFINED_SWIZZLE: D3D12DDI_TEXTURE_LAYOUT =
        D3D12DDI_TEXTURE_LAYOUT_D3D12DDI_TL_64KB_TILE_UNDEFINED_SWIZZLE;
    pub(super) const TL_64KB_TILE_STANDARD_SWIZZLE: D3D12DDI_TEXTURE_LAYOUT =
        D3D12DDI_TEXTURE_LAYOUT_D3D12DDI_TL_64KB_TILE_STANDARD_SWIZZLE;

    // -- D3D12DDI_MEMORY_POOL (d3d12umddi.rs:48255-48256) -------------------
    pub(super) const POOL_L0: D3D12DDI_MEMORY_POOL = D3D12DDI_MEMORY_POOL_D3D12DDI_MEMORY_POOL_L0;
    pub(super) const POOL_L1: D3D12DDI_MEMORY_POOL = D3D12DDI_MEMORY_POOL_D3D12DDI_MEMORY_POOL_L1;

    // -- D3D12DDI_CPU_PAGE_PROPERTY (d3d12umddi.rs:48248-48253) -------------
    pub(super) const CPU_NOT_AVAILABLE: D3D12DDI_CPU_PAGE_PROPERTY =
        D3D12DDI_CPU_PAGE_PROPERTY_D3D12DDI_CPU_PAGE_PROPERTY_NOT_AVAILABLE;
    pub(super) const CPU_WRITE_COMBINE: D3D12DDI_CPU_PAGE_PROPERTY =
        D3D12DDI_CPU_PAGE_PROPERTY_D3D12DDI_CPU_PAGE_PROPERTY_WRITE_COMBINE;
    pub(super) const CPU_WRITE_BACK: D3D12DDI_CPU_PAGE_PROPERTY =
        D3D12DDI_CPU_PAGE_PROPERTY_D3D12DDI_CPU_PAGE_PROPERTY_WRITE_BACK;

    // -- D3D12DDI_HEAP_FLAGS (d3d12umddi.rs:48258-48264) --------------------
    // ⛔ POSITIVE "allow" bits. The API takes DENY bits (`ResourceHeaps.md:874`).
    pub(super) const HEAP_ALLOW_NON_RT_DS_TEXTURES: D3D12DDI_HEAP_FLAGS =
        D3D12DDI_HEAP_FLAGS_D3D12DDI_HEAP_FLAG_NON_RT_DS_TEXTURES;
    pub(super) const HEAP_ALLOW_BUFFERS: D3D12DDI_HEAP_FLAGS =
        D3D12DDI_HEAP_FLAGS_D3D12DDI_HEAP_FLAG_BUFFERS;
    pub(super) const HEAP_ALLOW_RT_DS_TEXTURES: D3D12DDI_HEAP_FLAGS =
        D3D12DDI_HEAP_FLAGS_D3D12DDI_HEAP_FLAG_RT_DS_TEXTURES;
    /// ⛔ The one dropped heap bit that changes what the object *is*, rather
    /// than how it may be used. `SPECS.md` §9.7 from `ResourceHeaps.md:897`:
    /// *"The PRIMARY heap flag is the D3D12 replacement for
    /// `DXGI_DDI_PRIMARY_DESC`: no primary description is ever passed to the
    /// UMD, the flag alone declares the primary"*. It gets its own counter, not
    /// the shared unrepresentable bucket — see [`super::heap_flags`].
    pub(super) const HEAP_PRIMARY: D3D12DDI_HEAP_FLAGS =
        D3D12DDI_HEAP_FLAGS_D3D12DDI_HEAP_FLAG_PRIMARY;

    // -- D3D12DDI_RESOURCE_OPTIMIZATION_FLAGS (d3d12umddi.rs:48846-48854) ----
    /// ⚠ `KMD_IMPACT.md` §14a.3's *"second signal"* for a primary — and it
    /// arrives on `pfnCheckResourceAllocationInfo`, NOT on the create. This
    /// enum appears in exactly one function-pointer family
    /// (`d3d12umddi.rs:51734`, `:59866`, `:75022`, `:76696`, `:79414`,
    /// `:87548`), and `D3D12DDIARG_CREATERESOURCE_0109` has no field of the
    /// type. See [`super::note_presentable_identity`].
    pub(super) const RESOURCE_OPT_PRIMARY: D3D12DDI_RESOURCE_OPTIMIZATION_FLAGS =
        D3D12DDI_RESOURCE_OPTIMIZATION_FLAGS_D3D12DDI_RESOURCE_OPTIMIZATION_FLAG_PRIMARY;

    // -- D3D12DDI_RESOURCE_FLAGS_0003 (d3d12umddi.rs:48362-48389) -----------
    pub(super) const RES_RENDER_TARGET: D3D12DDI_RESOURCE_FLAGS_0003 =
        D3D12DDI_RESOURCE_FLAGS_0003_D3D12DDI_RESOURCE_FLAG_0003_RENDER_TARGET;
    pub(super) const RES_DEPTH_STENCIL: D3D12DDI_RESOURCE_FLAGS_0003 =
        D3D12DDI_RESOURCE_FLAGS_0003_D3D12DDI_RESOURCE_FLAG_0003_DEPTH_STENCIL;
    pub(super) const RES_CROSS_ADAPTER: D3D12DDI_RESOURCE_FLAGS_0003 =
        D3D12DDI_RESOURCE_FLAGS_0003_D3D12DDI_RESOURCE_FLAG_0003_CROSS_ADAPTER;
    pub(super) const RES_SIMULTANEOUS_ACCESS: D3D12DDI_RESOURCE_FLAGS_0003 =
        D3D12DDI_RESOURCE_FLAGS_0003_D3D12DDI_RESOURCE_FLAG_0003_SIMULTANEOUS_ACCESS;
    /// ⛔ The INVERTED one: positive here, `DENY_SHADER_RESOURCE` in the API.
    pub(super) const RES_SHADER_RESOURCE: D3D12DDI_RESOURCE_FLAGS_0003 =
        D3D12DDI_RESOURCE_FLAGS_0003_D3D12DDI_RESOURCE_FLAG_0003_SHADER_RESOURCE;
    pub(super) const RES_VIDEO_DECODE_REFERENCE_ONLY: D3D12DDI_RESOURCE_FLAGS_0003 =
        D3D12DDI_RESOURCE_FLAGS_0003_D3D12DDI_RESOURCE_FLAG_0020_VIDEO_DECODE_REFERENCE_ONLY;
    pub(super) const RES_UNORDERED_ACCESS: D3D12DDI_RESOURCE_FLAGS_0003 =
        D3D12DDI_RESOURCE_FLAGS_0003_D3D12DDI_RESOURCE_FLAG_0022_UNORDERED_ACCESS;
    pub(super) const RES_VIDEO_ENCODE_REFERENCE_ONLY: D3D12DDI_RESOURCE_FLAGS_0003 =
        D3D12DDI_RESOURCE_FLAGS_0003_D3D12DDI_RESOURCE_FLAG_0080_VIDEO_ENCODE_REFERENCE_ONLY;
    pub(super) const RES_RAYTRACING_ACCELERATION_STRUCTURE: D3D12DDI_RESOURCE_FLAGS_0003 =
        D3D12DDI_RESOURCE_FLAGS_0003_D3D12DDI_RESOURCE_FLAG_0088_RAYTRACING_ACCELERATION_STRUCTURE;

    // -- D3D12DDI_BARRIER_LAYOUT (d3d12umddi.rs:78589-78657) ----------------
    pub(super) const LAYOUT_UNDEFINED: D3D12DDI_BARRIER_LAYOUT =
        D3D12DDI_BARRIER_LAYOUT_D3D12DDI_BARRIER_LAYOUT_UNDEFINED;
    pub(super) const LAYOUT_COMMON: D3D12DDI_BARRIER_LAYOUT =
        D3D12DDI_BARRIER_LAYOUT_D3D12DDI_BARRIER_LAYOUT_COMMON;
    pub(super) const LAYOUT_VIDEO_QUEUE_COMMON: D3D12DDI_BARRIER_LAYOUT =
        D3D12DDI_BARRIER_LAYOUT_D3D12DDI_BARRIER_LAYOUT_VIDEO_QUEUE_COMMON;
    /// The four LEGACY_* enumerators plus 31 exist ONLY in the DDI: they are how
    /// the runtime hands a driver a legacy `D3D12_RESOURCE_STATES`-derived
    /// initial layout. The API `D3D12_BARRIER_LAYOUT` stops at
    /// `VIDEO_QUEUE_COMMON = 30`.
    pub(super) const LAYOUT_LEGACY_GENERIC_READ: D3D12DDI_BARRIER_LAYOUT =
        D3D12DDI_BARRIER_LAYOUT_D3D12DDI_BARRIER_LAYOUT_LEGACY_DIRECT_QUEUE_GENERIC_READ_COMPUTE_QUEUE_ACCESSIBLE;
    pub(super) const LAYOUT_LEGACY_COPY_SOURCE: D3D12DDI_BARRIER_LAYOUT =
        D3D12DDI_BARRIER_LAYOUT_D3D12DDI_BARRIER_LAYOUT_LEGACY_COPY_SOURCE;
    pub(super) const LAYOUT_LEGACY_COPY_DEST: D3D12DDI_BARRIER_LAYOUT =
        D3D12DDI_BARRIER_LAYOUT_D3D12DDI_BARRIER_LAYOUT_LEGACY_COPY_DEST;
    pub(super) const LAYOUT_LEGACY_SHADER_RESOURCE: D3D12DDI_BARRIER_LAYOUT =
        D3D12DDI_BARRIER_LAYOUT_D3D12DDI_BARRIER_LAYOUT_LEGACY_SHADER_RESOURCE;
    pub(super) const LAYOUT_LEGACY_PIXEL_SHADER_RESOURCE: D3D12DDI_BARRIER_LAYOUT =
        D3D12DDI_BARRIER_LAYOUT_D3D12DDI_BARRIER_LAYOUT_LEGACY_PIXEL_SHADER_RESOURCE;
}

/// How many times any one bounded evidence line may repeat, per site.
///
/// Matches `caps12`'s and `misc.rs`'s idiom: the counter is unbounded, the log
/// line is not. T2 measured what an unbounded per-op logger costs.
const LOG_BUDGET: usize = 32;

/// The value [`check_subresource_info`] seeds its 32-bit `GetCopyableFootprints`
/// outputs with, and reads back as *"the engine did not answer"*.
///
/// ⛔ Not an invented convention: vkd3d `memset`s every out-parameter array to
/// `0xff` and sets `total = ~0ull` **before** validating the subresource range,
/// then `goto end`s without touching them if the range is bad
/// (`libs/vkd3d/device.c:9234-9268`). So `UINT_MAX` is the engine's own way of
/// saying it declined, and seeding the same value makes "the engine wrote the
/// sentinel" and "the engine wrote nothing" the same observation. No real
/// footprint has a `RowPitch` or row count of `UINT_MAX`: the pitch is aligned to
/// 256 B and a resource that large is not creatable.
const FOOTPRINT_UNANSWERED_U32: u32 = u32::MAX;

/// The 64-bit form of [`FOOTPRINT_UNANSWERED_U32`], for `pTotalBytes`.
const FOOTPRINT_UNANSWERED_U64: u64 = u64::MAX;

/// The largest castable-format list this driver will forward.
///
/// `D3D12DDIARG_CREATERESOURCE_0109::NumCastableFormats` is a `UINT32` read
/// straight from the runtime, and it becomes a slice length. DXGI defines fewer
/// than 200 formats, so anything above this is not a list.
const CASTABLE_FORMAT_LIMIT: usize = 1_024;

/// The alignment ceiling a driver may report for a non-multisampled resource.
///
/// `ResourceHeaps.md:1350` (mined in `SPECS.md` §9.7): *"The driver-reported
/// resource alignment is capped: it must be a power of two and must never exceed
/// 64KiB unless `SampleDesc.Count > 1` — the >64KiB (4MB-class) tier exists only
/// for MSAA."* vkd3d answers `GetResourceAllocationInfo` from its own Vulkan
/// memory requirements and is under no such obligation, so the clamp is this
/// driver's.
const MAX_NON_MSAA_ALIGNMENT: u64 = 64 * 1024;

/// `GetResourceAllocationInfo`'s documented failure answer.
///
/// The API has no HRESULT — it returns the struct by value — and reports failure
/// by setting `SizeInBytes` to `UINT64_MAX`. vkd3d does exactly that
/// (`libs/vkd3d/device.c`, `d3d12_device_GetResourceAllocationInfo`).
const ALLOCATION_INFO_FAILURE: u64 = u64::MAX;

// ---------------------------------------------------------------------------
// Per-object payloads — LOCAL and typed, per `DECISIONS.md` D13
// ---------------------------------------------------------------------------

/// Where an object lives inside an engine heap.
///
/// ⭐ This is what makes `CreatePlacedResource` reachable from a DDI that never
/// names a heap. `ReuseBufferGPUVA` names a **resource** and an offset within
/// it; every object this driver puts in a heap records the heap and its own base
/// offset, so resolving the parent resource yields `(heap, parent_base)` and the
/// child lands at `parent_base + requested_offset`.
struct HeapSpan {
    /// The engine heap. An owning reference: a placed resource must not outlive
    /// its heap, and holding a reference per child is how that is guaranteed
    /// without this driver tracking a child list.
    heap: ID3D12Heap,
    /// This object's byte offset from the start of `heap`.
    base_offset: u64,
}

/// What this driver stores in `D3D12DDI_HHEAP::pDrvPrivate`.
///
/// ⚠ Immutable after construction, deliberately. D3D12 DDIs are free-threaded
/// and `device12::device`'s note applies verbatim: concurrent **reads** are the
/// expected case and `&` permits them by construction. Nothing here needs
/// interior mutation, so nothing here has a lock.
struct HeapState {
    /// The engine heap, for the `CreateHeap` arm. `None` for the implicit heap
    /// of a committed resource, which has no `ID3D12Heap` at the API level.
    heap: Option<ID3D12Heap>,
    /// The resource [`map_heap`] maps to obtain the heap's CPU base address.
    ///
    /// ⭐ **At the DDI, Map lives on the HEAP and never on the resource**
    /// (`ResourceHeaps.md:454`, `SPECS.md` §9.7): `ID3D12Resource::Map` does not
    /// reach the driver as a resource map; the runtime maps the heap and derives
    /// per-resource pointers from `pfnCheckSubresourceInfo`. The D3D12 **API**
    /// has no heap map at all, so the only Rust-reachable way to obtain a heap's
    /// CPU address is to map a resource that covers it at offset 0:
    ///
    /// * committed arm -- the resource itself, which by definition starts at
    ///   offset 0 of its implicit heap;
    /// * heap-only arm -- a whole-heap buffer created alongside the heap, see
    ///   [`create_heap_span_buffer`].
    ///
    /// `None` means this heap cannot be mapped, and [`map_heap`] refuses with
    /// `MapHeapNoAnchor` rather than inventing an address.
    map_anchor: Option<ID3D12Resource>,
    /// The heap's size in bytes, as the runtime asked for it. Reported on the
    /// creation evidence line.
    byte_size: u64,
}

/// What this driver stores in `D3D12DDI_HRESOURCE::pDrvPrivate`.
struct ResourceState {
    /// The engine resource. `None` only for the whole-heap object of a heap
    /// whose flags deny buffers, where no covering resource can exist.
    resource: Option<ID3D12Resource>,
    /// Where this object sits inside an engine heap, when it does. `None` for a
    /// committed resource: its heap is implicit and has no `ID3D12Heap`, so it
    /// can never be a placement parent.
    span: Option<HeapSpan>,
    /// The API description this driver built for the resource, cached so
    /// [`check_subresource_info`] is deterministic for a given resource and
    /// subresource (`ResourceHeaps.md:1437` requires exactly that) without a COM
    /// round trip per query.
    desc: D3D12_RESOURCE_DESC1,
    /// The allocation info this driver reported at create time, so
    /// [`check_existing_resource_allocation_info`] answers the same numbers
    /// [`check_resource_allocation_info`] did. Two independent computations of
    /// one answer is how they come to disagree.
    alloc_info: ddi12::D3D12DDI_RESOURCE_ALLOCATION_INFO_0022,
    /// ⛔ **Whether the `hHeap` of this object's fused create/destroy pair is a
    /// private block THIS DRIVER OWNS**, i.e. whether the paired sizing call
    /// asked for one. `true` for the heap-only and committed arms, `false` for
    /// the placed arm, where `hHeap` is an **already-live** heap's handle rather
    /// than a fresh block.
    ///
    /// ⭐ It exists because `pfnDestroyHeapAndResource` is handed the same
    /// `(hHeap, hResource)` pair as the create but **not** the `pCreateHeap` /
    /// `pCreateResource` pointers the create classified the arm from, so the
    /// answer has to be carried on the object. Without it the destroy cannot
    /// tell "my heap block" from "someone else's live heap", and the two halves
    /// of one operation held contradictory beliefs about the same parameter:
    /// `create_heap_and_resource` refuses even to *clear* that word on the
    /// placed arm — at length, calling it *"the single most dangerous line in
    /// the lane if it is wrong"* — while the destroy took the box and dropped
    /// it on every arm. Under the create site's own premise, the first
    /// `Release()` of any placed resource tore down its live parent heap:
    /// `ID3D12Heap` released, map anchor gone, `pfnMapHeap` refusing thereafter,
    /// and a later placed create into that heap misdiagnosed as a *reserved*
    /// resource. Found by the `PARALLEL.md` §10 review, before any VM run.
    ///
    /// ⚠ Recorded on the RESOURCE and not on the heap deliberately: on the
    /// placed arm the heap's own `HeapState` says "I am owned" — truthfully, by
    /// its own create — so it cannot distinguish which operation is asking.
    /// The resource is the object whose create knew the answer.
    owns_heap_block: bool,
}

// ⚠ **`D3D12DDI_HRTRESOURCE` is deliberately NOT a field**, and the omission is
// a rule rather than an oversight. `DDI_REFERENCE.md` §7.3(3) says *"the `hRT`
// must be stored -- it is the token every callback about that object takes"*,
// and it names the callbacks that make that true: `pfnCreateContextCb` takes an
// `HRTCOMMANDQUEUE`, `pfnSetCommandListErrorCb` takes an `HRTCOMMANDLIST`.
// **Nothing in the corelayer or kernel callback tables takes an
// `HRTRESOURCE`**, so a field holding it would be written once and read never --
// which is R908's shape exactly, and `PARALLEL.md` §10 forbids the
// `#[allow(dead_code)]` that would otherwise silence it. The lane that finds a
// resource callback needing the runtime handle adds the field then, with a
// reader.

// ---------------------------------------------------------------------------
// Slot access — the D3D12 soundness argument, RE-DERIVED
// ---------------------------------------------------------------------------

/// Borrow the heap state behind a DDI heap handle.
///
/// # ⛔ Why this uses [`Slot::ptr`] and not `Slot::get`
///
/// `umd_common/src/slot.rs:304-322` states plainly that
/// `Slot<Boxed<S>>::get() -> &'static S`'s soundness argument rests on the D3D11
/// runtime's `CUseCountedObject` first-created / last-destroyed ordering, that
/// **no equivalent statement has been located for `d3d12umddi`**, and that a
/// `umd12` caller either owes the derivation or *"must reach for `Self::ptr` and
/// carry the lifetime themselves"*. `PARALLEL.md` §9.4 repeats it: D13 shares
/// declarations, not claims.
///
/// This takes the second option, and it is the stronger one. The returned
/// reference is **not** `'static`: it borrows for the caller's binding, and
/// every caller is one DDI invocation that drops it before returning. What is
/// still needed is that no borrow overlaps the box's teardown, and that argument
/// does not depend on `CUseCountedObject`:
///
/// * the box is written exactly once, inside [`create_heap_and_resource`], into
///   a `pDrvPrivate` block the runtime allocated for *this* object and has not
///   yet handed to any other DDI;
/// * it is taken exactly once, in [`destroy_heap_and_resource`];
/// * `pDrvPrivate` is memory **the runtime owns and frees itself** immediately
///   after that Destroy returns (`DDI_REFERENCE.md` §7.1 step 4). A runtime that
///   dispatched another DDI carrying the same handle concurrently with, or
///   after, its Destroy would be reading an allocation it is about to free or
///   has already freed — a defect in its own memory management, not a race this
///   driver could lose differently.
///
/// ⚠ Concurrent **reads** from FREETHREADED worker threads are permitted by `&`
/// and are the expected case; [`HeapState`] is immutable after construction so
/// there is nothing to serialise.
///
/// # Safety
/// `h_heap.pDrvPrivate`, when non-null, must be the private block this driver
/// wrote for a live heap, and the returned reference must not outlive the DDI
/// call that obtained it.
unsafe fn heap_state<'a>(h_heap: ddi12::D3D12DDI_HHEAP) -> Option<&'a HeapState> {
    // SAFETY: the caller guarantees the slot is either null or this driver's own
    // private block for a heap; `from_priv` only records non-nullness.
    let slot = unsafe { Slot::<Boxed<HeapState>>::from_priv(h_heap.pDrvPrivate) }?;
    // SAFETY: same precondition; `ptr` reads the slot word and casts it, which
    // is the one cast `slot` exists to confine to itself.
    let p = unsafe { slot.ptr() };
    if p.is_null() {
        return None;
    }
    // SAFETY: non-null per the check, points at the `Box<HeapState>` this driver
    // leaked into the slot, and the lifetime is the caller's per the argument
    // above.
    Some(unsafe { &*p })
}

/// Borrow the resource state behind a DDI resource handle.
///
/// The [`heap_state`] argument applies verbatim, with `pfnCreateHeapAndResource`
/// and `pfnDestroyHeapAndResource` as the same single writer and single taker.
///
/// # Safety
/// As [`heap_state`], for a resource handle.
unsafe fn resource_state<'a>(h_resource: ddi12::D3D12DDI_HRESOURCE) -> Option<&'a ResourceState> {
    // SAFETY: as `heap_state`.
    let slot = unsafe { Slot::<Boxed<ResourceState>>::from_priv(h_resource.pDrvPrivate) }?;
    // SAFETY: as `heap_state`.
    let p = unsafe { slot.ptr() };
    if p.is_null() {
        return None;
    }
    // SAFETY: as `heap_state`.
    Some(unsafe { &*p })
}

/// The engine resource behind a DDI resource handle, borrowed.
///
/// ⭐ `pub(crate)` because it is the seam every other lane needs: L3c's copies,
/// L5's view creation and L8's present all take a `D3D12DDI_HRESOURCE` and need
/// the `ID3D12Resource` behind it, and R803's scar is that the payload must be
/// derived from the handle **type** in one place rather than decoded at each
/// call site. This file owns resource handles, so this is that one place.
///
/// ⚠ A **shared reference**, not a `ManuallyDrop<ID3D12Resource>`. The state box
/// keeps the owning reference; borrowing it as `&` makes releasing it
/// unwritable, where a `ManuallyDrop` merely makes it unlikely. That is the
/// stronger half of `bridge12.rs`'s owned-vs-borrowed rule, available here only
/// because this driver keeps the object rather than receiving a raw `usize`
/// across the FFI.
///
/// # Safety
/// As [`resource_state`]. The returned reference must not outlive the DDI call.
pub(crate) unsafe fn engine_resource<'a>(
    h_resource: ddi12::D3D12DDI_HRESOURCE,
) -> Option<&'a ID3D12Resource> {
    // SAFETY: forwarded unchanged.
    let state = unsafe { resource_state::<'a>(h_resource) }?;
    state.resource.as_ref()
}

/// The engine device, at the `ID3D12Device10` revision this lane forwards to.
///
/// Returns an **owned** reference (`Interface::cast` is a `QueryInterface`), so
/// the caller releases it by dropping. The QI is per call rather than cached in
/// `HeliosD3D12Device` because caching would mean appending a field to
/// `device12.rs`, a shared file three other lanes are also editing
/// (`PARALLEL.md` §5), for a GUID compare against an already-live object.
fn engine_device10(dev: &HeliosD3D12Device) -> Option<ID3D12Device10> {
    let Some(engine) = dev.engine.d3d12_device() else {
        // Unreachable by construction: `BridgeDevice12::create` folds a null
        // engine into `None`, so a live device always carries one. Counted
        // because "unreachable by construction" is a claim about a cross-FFI
        // contract, and this is where it would be observed breaking.
        note_refusal(&L4_REFUSALS.resource_no_device);
        return None;
    };
    match engine.cast::<ID3D12Device10>() {
        Ok(device10) => Some(device10),
        Err(err) => {
            L4_REFUSALS.resource_device10_unavailable.bump();
            let n = L4_REFUSALS.resource_device10_unavailable.get();
            if n <= LOG_BUDGET {
                log_error!(
                    "L4: the engine does not expose ID3D12Device10 (hr={:#010x}) -- resource \
                     creation needs CreateCommittedResource3/CreatePlacedResource2 for the \
                     barrier layout and castable formats the DDI carries (x{n})",
                    err.code().0 as u32,
                );
            }
            None
        }
    }
}

// ---------------------------------------------------------------------------
// DDI -> API translation. Every enum by `match`, never by cast.
// ---------------------------------------------------------------------------

/// `D3D12DDI_MEMORY_POOL` -> `D3D12_MEMORY_POOL`.
///
/// ⛔ **Offset by one.** DDI `L0 = 0, L1 = 1`; API `UNKNOWN = 0, L0 = 1,
/// L1 = 2`. A cast would report every system-memory heap as "driver chooses".
/// `ResourceHeaps.md:915`: *"the driver must honour the requested memory pool
/// and CPU page property exactly"*, so there is no defensible default here — an
/// unrecognised value is counted and mapped to L0, the conservative pool
/// (system memory is always addressable; local video memory is not).
fn memory_pool(pool: ddi12::D3D12DDI_MEMORY_POOL) -> D3D12_MEMORY_POOL {
    match pool {
        v::POOL_L0 => D3D12_MEMORY_POOL_L0,
        v::POOL_L1 => D3D12_MEMORY_POOL_L1,
        other => {
            L4_REFUSALS.heap_property_unrepresentable.bump();
            let n = L4_REFUSALS.heap_property_unrepresentable.get();
            if n <= LOG_BUDGET {
                log_error!("L4: unknown D3D12DDI_MEMORY_POOL {other} -> L0 (x{n})");
            }
            D3D12_MEMORY_POOL_L0
        }
    }
}

/// `D3D12DDI_CPU_PAGE_PROPERTY` -> `D3D12_CPU_PAGE_PROPERTY`.
///
/// ⛔ **Offset by one, and the failure is worse than the pool's.** DDI
/// `NOT_AVAILABLE = 0`; API `UNKNOWN = 0`. A cast would turn every
/// GPU-only heap into "driver chooses", which is how a device-local heap
/// silently becomes CPU-visible.
fn cpu_page_property(prop: ddi12::D3D12DDI_CPU_PAGE_PROPERTY) -> D3D12_CPU_PAGE_PROPERTY {
    match prop {
        v::CPU_NOT_AVAILABLE => D3D12_CPU_PAGE_PROPERTY_NOT_AVAILABLE,
        v::CPU_WRITE_COMBINE => D3D12_CPU_PAGE_PROPERTY_WRITE_COMBINE,
        v::CPU_WRITE_BACK => D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
        other => {
            L4_REFUSALS.heap_property_unrepresentable.bump();
            let n = L4_REFUSALS.heap_property_unrepresentable.get();
            if n <= LOG_BUDGET {
                log_error!(
                    "L4: unknown D3D12DDI_CPU_PAGE_PROPERTY {other} -> NOT_AVAILABLE (x{n})"
                );
            }
            D3D12_CPU_PAGE_PROPERTY_NOT_AVAILABLE
        }
    }
}

/// `D3D12DDI_HEAP_FLAGS` (positive ALLOW bits) -> `D3D12_HEAP_FLAGS` (DENY bits).
///
/// ⛔ **The polarity inverts.** `ResourceHeaps.md:874`: *"the app writes
/// DENY_BUFFERS / DENY_RT_DS_TEXTURES / DENY_NON_RT_DS_TEXTURES, but the driver
/// receives positive ALLOW-style bits at exactly the values 0x2 / 0x4 / 0x20"*.
///
/// ⚠ And `ResourceHeaps.md:907`: *"Heap tier 1 does NOT mean the driver may
/// assume mutually exclusive category bits at the DDI — the runtime deliberately
/// sends heaps with both texture-category bits set and the UMD is required to
/// handle them."* This driver reports `ResourceHeapTier = 1` (`caps12.rs:570`)
/// and this function makes no exclusivity assumption: each bit is tested and
/// inverted on its own.
///
/// The DDI bits with no API counterpart — `COHERENT_SYSTEMWIDE`,
/// `_0041_DENY_L0_DEMOTION`, and any future one — are dropped and counted into
/// one shared bucket, which is honest because dropping either is benign.
///
/// ⛔ **`PRIMARY` is NOT in that bucket, and separating it is the point.**
/// `SPECS.md` §9.7 from `ResourceHeaps.md:897`: *"The PRIMARY heap flag is the
/// D3D12 replacement for `DXGI_DDI_PRIMARY_DESC`: no primary description is ever
/// passed to the UMD, the flag alone declares the primary, and receiving it
/// obliges the driver to create a resource simultaneously with the heap."* It is
/// therefore the only channel by which this driver can learn it is building the
/// thing that reaches the scanout, and dropping it into an aggregate this lane's
/// own evidence contract grades *"non-zero OK"* would hide a demoted primary
/// behind two genuinely benign bits. It gets its own counter, expected 0 until
/// L8's present path consumes it, so the aggregate can honestly stay "non-zero
/// OK" and the one bit that changes the present path is visible on its own.
fn heap_flags(flags: ddi12::D3D12DDI_HEAP_FLAGS) -> D3D12_HEAP_FLAGS {
    let mut out = D3D12_HEAP_FLAG_NONE;
    if flags & v::HEAP_ALLOW_BUFFERS == 0 {
        out |= D3D12_HEAP_FLAG_DENY_BUFFERS;
    }
    if flags & v::HEAP_ALLOW_NON_RT_DS_TEXTURES == 0 {
        out |= D3D12_HEAP_FLAG_DENY_NON_RT_DS_TEXTURES;
    }
    if flags & v::HEAP_ALLOW_RT_DS_TEXTURES == 0 {
        out |= D3D12_HEAP_FLAG_DENY_RT_DS_TEXTURES;
    }
    if flags & v::HEAP_PRIMARY != 0 {
        L4_REFUSALS.heap_primary_flag_dropped.bump();
        let n = L4_REFUSALS.heap_primary_flag_dropped.get();
        if n <= LOG_BUDGET {
            log_error!(
                "L4: D3D12DDI_HEAP_FLAG_PRIMARY arrived -- this heap declares the swapchain \
                 primary and is being created as an ordinary CUSTOM heap, because the API has \
                 no counterpart bit. The FACT is not lost: on the committed arm \
                 note_presentable_identity records it in the identity table (UP-4) (x{n})"
            );
        }
    }
    let unmapped = flags
        & !(v::HEAP_ALLOW_BUFFERS
            | v::HEAP_ALLOW_NON_RT_DS_TEXTURES
            | v::HEAP_ALLOW_RT_DS_TEXTURES
            | v::HEAP_PRIMARY);
    if unmapped != 0 {
        L4_REFUSALS.heap_flag_unrepresentable.bump();
        let n = L4_REFUSALS.heap_flag_unrepresentable.get();
        if n <= LOG_BUDGET {
            log_error!(
                "L4: D3D12DDI_HEAP_FLAGS {unmapped:#x} have no D3D12_HEAP_FLAGS counterpart -- \
                 dropped (x{n})"
            );
        }
    }
    out
}

/// `D3D12DDI_RESOURCE_TYPE` -> `D3D12_RESOURCE_DIMENSION`.
///
/// The two enums agree numerically (BUFFER 1, TEXTURE1D 2, TEXTURE2D 3,
/// TEXTURE3D 4) but they are still matched rather than cast, because agreement
/// today is not a contract and the compiler cannot see the coincidence.
fn resource_dimension(
    resource_type: ddi12::D3D12DDI_RESOURCE_TYPE,
) -> Option<D3D12_RESOURCE_DIMENSION> {
    match resource_type {
        v::RT_BUFFER => Some(D3D12_RESOURCE_DIMENSION_BUFFER),
        v::RT_TEXTURE1D => Some(D3D12_RESOURCE_DIMENSION_TEXTURE1D),
        v::RT_TEXTURE2D => Some(D3D12_RESOURCE_DIMENSION_TEXTURE2D),
        v::RT_TEXTURE3D => Some(D3D12_RESOURCE_DIMENSION_TEXTURE3D),
        _ => None,
    }
}

/// `D3D12DDI_TEXTURE_LAYOUT` -> `D3D12_TEXTURE_LAYOUT`.
///
/// The first four agree numerically; `D3D12DDI_TL_DEVICE_DEPENDENT_SWIZZLE_0`
/// (4) has no API counterpart and is counted, then passed as `UNKNOWN` — which
/// is the API's own name for "the driver picks", i.e. exactly what a
/// device-dependent swizzle is from the runtime's side.
fn texture_layout(layout: ddi12::D3D12DDI_TEXTURE_LAYOUT) -> D3D12_TEXTURE_LAYOUT {
    match layout {
        v::TL_UNDEFINED => D3D12_TEXTURE_LAYOUT_UNKNOWN,
        v::TL_ROW_MAJOR => D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        v::TL_64KB_TILE_UNDEFINED_SWIZZLE => D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE,
        v::TL_64KB_TILE_STANDARD_SWIZZLE => D3D12_TEXTURE_LAYOUT_64KB_STANDARD_SWIZZLE,
        other => {
            L4_REFUSALS.resource_layout_unrepresentable.bump();
            let n = L4_REFUSALS.resource_layout_unrepresentable.get();
            if n <= LOG_BUDGET {
                log_error!(
                    "L4: D3D12DDI_TEXTURE_LAYOUT {other} has no D3D12_TEXTURE_LAYOUT counterpart \
                     -> UNKNOWN (x{n})"
                );
            }
            D3D12_TEXTURE_LAYOUT_UNKNOWN
        }
    }
}

/// `D3D12DDI_RESOURCE_FLAGS_0003` -> `D3D12_RESOURCE_FLAGS`.
///
/// ⛔ A different bit layout AND one inverted bit. The DDI's
/// `SHADER_RESOURCE = 0x10` is a positive "this resource may be sampled"; the
/// API expresses the same fact as the **absence** of
/// `DENY_SHADER_RESOURCE = 0x8`. Every other bit moves position:
/// `UNORDERED_ACCESS` 0x80 -> 0x4, `CROSS_ADAPTER` 0x4 -> 0x10,
/// `SIMULTANEOUS_ACCESS` 0x8 -> 0x20, `VIDEO_DECODE_REFERENCE_ONLY` 0x20 ->
/// 0x40, `VIDEO_ENCODE_REFERENCE_ONLY` 0x1000 -> 0x80,
/// `RAYTRACING_ACCELERATION_STRUCTURE` 0x2000 -> 0x100.
///
/// The DDI-only bits are dropped and counted: `_0020_CONTENT_PROTECTION`
/// (this driver reports no protected-resource support), the two
/// `_0041_ONLY_*_TEXTURE_PLACEMENT` placement hints, `_0041_4MB_ALIGNED` and
/// `_0073_SAMPLER_FEEDBACK`.
fn resource_flags(flags: ddi12::D3D12DDI_RESOURCE_FLAGS_0003) -> D3D12_RESOURCE_FLAGS {
    let mut out = D3D12_RESOURCE_FLAG_NONE;
    if flags & v::RES_RENDER_TARGET != 0 {
        out |= D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
    }
    if flags & v::RES_DEPTH_STENCIL != 0 {
        out |= D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL;
    }
    if flags & v::RES_UNORDERED_ACCESS != 0 {
        out |= D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    }
    if flags & v::RES_CROSS_ADAPTER != 0 {
        out |= D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER;
    }
    if flags & v::RES_SIMULTANEOUS_ACCESS != 0 {
        out |= D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS;
    }
    if flags & v::RES_VIDEO_DECODE_REFERENCE_ONLY != 0 {
        out |= D3D12_RESOURCE_FLAG_VIDEO_DECODE_REFERENCE_ONLY;
    }
    if flags & v::RES_VIDEO_ENCODE_REFERENCE_ONLY != 0 {
        out |= D3D12_RESOURCE_FLAG_VIDEO_ENCODE_REFERENCE_ONLY;
    }
    if flags & v::RES_RAYTRACING_ACCELERATION_STRUCTURE != 0 {
        out |= D3D12_RESOURCE_FLAG_RAYTRACING_ACCELERATION_STRUCTURE;
    }
    // ⛔ The inversion, and it is the whole reason this is not a cast.
    if flags & v::RES_SHADER_RESOURCE == 0 {
        out |= D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE;
    }

    let translated = v::RES_RENDER_TARGET
        | v::RES_DEPTH_STENCIL
        | v::RES_UNORDERED_ACCESS
        | v::RES_CROSS_ADAPTER
        | v::RES_SIMULTANEOUS_ACCESS
        | v::RES_VIDEO_DECODE_REFERENCE_ONLY
        | v::RES_VIDEO_ENCODE_REFERENCE_ONLY
        | v::RES_RAYTRACING_ACCELERATION_STRUCTURE
        | v::RES_SHADER_RESOURCE;
    let unmapped = flags & !translated;
    if unmapped != 0 {
        L4_REFUSALS.resource_flag_unrepresentable.bump();
        let n = L4_REFUSALS.resource_flag_unrepresentable.get();
        if n <= LOG_BUDGET {
            log_error!(
                "L4: D3D12DDI_RESOURCE_FLAGS_0003 {unmapped:#x} have no D3D12_RESOURCE_FLAGS \
                 counterpart -- dropped (x{n})"
            );
        }
    }
    out
}

/// `D3D12DDI_BARRIER_LAYOUT` -> `D3D12_BARRIER_LAYOUT`.
///
/// The two enums agree exactly over `UNDEFINED = -1` through
/// `VIDEO_QUEUE_COMMON = 30`, which is asserted below rather than assumed. What
/// the DDI adds and the API does not have is the legacy family — value 31 and
/// the four negative `LEGACY_*` enumerators — which is how the runtime hands a
/// driver an initial layout derived from a legacy `D3D12_RESOURCE_STATES`. Each
/// maps to the nearest API layout and is counted, because the mapping is this
/// driver's judgement rather than the header's.
///
/// ⚠ **Buffers must be `UNDEFINED`.** vkd3d rejects any other layout for a
/// buffer outright — *"Using non-undefined layout for buffer. This is not
/// allowed."*, `libs/vkd3d/device.c:9427` and `:9463`, `E_INVALIDARG`. That is
/// the API's rule, not vkd3d's invention, so a buffer's layout is forced and
/// counted here rather than discovered as a failed create.
fn barrier_layout(
    layout: ddi12::D3D12DDI_BARRIER_LAYOUT,
    is_buffer: bool,
) -> D3D12_BARRIER_LAYOUT {
    if is_buffer {
        if layout != v::LAYOUT_UNDEFINED {
            L4_REFUSALS.resource_barrier_layout_coerced.bump();
            let n = L4_REFUSALS.resource_barrier_layout_coerced.get();
            if n <= LOG_BUDGET {
                log_error!(
                    "L4: buffer arrived with InitialBarrierLayout {layout} -- forcing UNDEFINED, \
                     which is the only layout a buffer may carry (x{n})"
                );
            }
        }
        return D3D12_BARRIER_LAYOUT_UNDEFINED;
    }

    // The shared range, passed through by value. The four anchors below are
    // asserted at compile time so "the enums agree" is checked, not claimed.
    const _: () = assert!(v::LAYOUT_UNDEFINED == D3D12_BARRIER_LAYOUT_UNDEFINED.0);
    const _: () = assert!(v::LAYOUT_COMMON == D3D12_BARRIER_LAYOUT_COMMON.0);
    const _: () = assert!(v::LAYOUT_VIDEO_QUEUE_COMMON == D3D12_BARRIER_LAYOUT_VIDEO_QUEUE_COMMON.0);
    if (v::LAYOUT_UNDEFINED..=v::LAYOUT_VIDEO_QUEUE_COMMON).contains(&layout) {
        return D3D12_BARRIER_LAYOUT(layout);
    }

    let coerced = match layout {
        v::LAYOUT_LEGACY_GENERIC_READ => D3D12_BARRIER_LAYOUT_GENERIC_READ,
        v::LAYOUT_LEGACY_COPY_SOURCE => D3D12_BARRIER_LAYOUT_COPY_SOURCE,
        v::LAYOUT_LEGACY_COPY_DEST => D3D12_BARRIER_LAYOUT_COPY_DEST,
        v::LAYOUT_LEGACY_SHADER_RESOURCE | v::LAYOUT_LEGACY_PIXEL_SHADER_RESOURCE => {
            D3D12_BARRIER_LAYOUT_SHADER_RESOURCE
        }
        // ⚠ COMMON and not UNDEFINED: UNDEFINED tells the engine the contents
        // are discardable, which is a claim about the resource rather than about
        // this driver's ignorance. vkd3d makes the same choice for an
        // unrecognised layout and says so (`device.c`, the generic fallback in
        // `vkd3d_barrier_layout_to_resource_state`).
        _ => D3D12_BARRIER_LAYOUT_COMMON,
    };
    L4_REFUSALS.resource_barrier_layout_coerced.bump();
    let n = L4_REFUSALS.resource_barrier_layout_coerced.get();
    if n <= LOG_BUDGET {
        log_error!(
            "L4: D3D12DDI_BARRIER_LAYOUT {layout} is outside the API enum -> {} (x{n})",
            coerced.0,
        );
    }
    coerced
}

/// Build the API resource description from the DDI's.
///
/// ⛔ **`Alignment` is always 0, and it is NOT a parameter.** The DDI moved the
/// field out of the struct — `D3D12DDIARG_CREATERESOURCE_0109` has no
/// `Alignment` while `D3D12_RESOURCE_DESC` does — and `pfnCreateHeapAndResource`
/// has no such argument either, so the create path can only ever say 0: the
/// API's *"the driver picks"*, which is exactly what the DDI means by omitting
/// it. `pfnCheckResourceAllocationInfo` does receive the application's requested
/// value as `AlignmentRestriction`, and it does **not** pass it here — see that
/// function's doc for why forwarding it would make the Check slots and the
/// create arms answer different questions about one resource. Making it a
/// constant rather than a parameter is what stops the two from drifting apart
/// again.
///
/// # Safety
/// `a` must be a live `D3D12DDIARG_CREATERESOURCE_0109`.
unsafe fn resource_desc1(
    a: &ddi12::D3D12DDIARG_CREATERESOURCE_0109,
) -> Option<D3D12_RESOURCE_DESC1> {
    let dimension = resource_dimension(a.ResourceType)?;
    Some(D3D12_RESOURCE_DESC1 {
        Dimension: dimension,
        Alignment: 0,
        Width: a.Width,
        Height: a.Height,
        DepthOrArraySize: a.DepthOrArraySize,
        MipLevels: a.MipLevels,
        Format: DXGI_FORMAT(a.Format),
        // Field by field, never a transmute: the two structs happen to be two
        // `UINT`s each, and `ARCHITECTURE.md` §12 rule 1 is that a happening is
        // not an ABI contract.
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: a.SampleDesc.Count,
            Quality: a.SampleDesc.Quality,
        },
        Layout: texture_layout(a.Layout),
        Flags: resource_flags(a.Flags),
        SamplerFeedbackMipRegion: D3D12_MIP_REGION {
            Width: a.SamplerFeedbackMipRegion.Width,
            Height: a.SamplerFeedbackMipRegion.Height,
            Depth: a.SamplerFeedbackMipRegion.Depth,
        },
    })
}

/// `D3D12_RESOURCE_DESC1` -> `D3D12_RESOURCE_DESC`, dropping the mip region.
///
/// The older entry points (`GetResourceAllocationInfo`, `GetCopyableFootprints`)
/// take the shorter struct. The mip region only describes sampler feedback,
/// which has no bearing on either answer.
fn resource_desc(desc1: &D3D12_RESOURCE_DESC1) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: desc1.Dimension,
        Alignment: desc1.Alignment,
        Width: desc1.Width,
        Height: desc1.Height,
        DepthOrArraySize: desc1.DepthOrArraySize,
        MipLevels: desc1.MipLevels,
        Format: desc1.Format,
        SampleDesc: desc1.SampleDesc,
        Layout: desc1.Layout,
        Flags: desc1.Flags,
    }
}

/// The optimized clear value, when the runtime supplied one.
///
/// ⚠ Which arm of the DDI union is live is decided by the **resource's** flags,
/// exactly as D3D12 defines an optimized clear value: a depth-stencil resource
/// carries `{Depth, Stencil}` and everything else carries a colour. There is no
/// discriminator in `D3D12DDI_CLEAR_VALUES` itself.
///
/// # Safety
/// `clear`, when non-null, must be a live `D3D12DDI_CLEAR_VALUES`.
unsafe fn clear_value(
    clear: *const ddi12::D3D12DDI_CLEAR_VALUES,
    ddi_flags: ddi12::D3D12DDI_RESOURCE_FLAGS_0003,
) -> Option<D3D12_CLEAR_VALUE> {
    if clear.is_null() {
        return None;
    }
    // SAFETY: non-null per the check; the DDI declares it `_In_opt_ CONST`, so a
    // non-null pointer is a live struct for the duration of the call.
    let c = unsafe { &*clear };
    let anonymous = if ddi_flags & v::RES_DEPTH_STENCIL != 0 {
        // SAFETY: reading the union's `DepthStencil` arm. Both arms are plain
        // data with no invalid bit patterns (`[f32; 4]` and `{f32, u8}`), so any
        // initialisation the runtime performed makes this read defined; which
        // arm is *meaningful* is the resource-flag test above.
        D3D12_CLEAR_VALUE_0 {
            DepthStencil: D3D12_DEPTH_STENCIL_VALUE {
                Depth: unsafe { c.__bindgen_anon_1.DepthStencil }.Depth,
                Stencil: unsafe { c.__bindgen_anon_1.DepthStencil }.Stencil,
            },
        }
    } else {
        // SAFETY: as above, for the `Color` arm.
        D3D12_CLEAR_VALUE_0 {
            Color: unsafe { c.__bindgen_anon_1.Color },
        }
    };
    Some(D3D12_CLEAR_VALUE {
        Format: DXGI_FORMAT(c.Format),
        Anonymous: anonymous,
    })
}

// ---------------------------------------------------------------------------
// (g) pfnCalcPrivateHeapAndResourceSizes / pfnCreateHeapAndResource /
//     pfnDestroyHeapAndResource
// ---------------------------------------------------------------------------

/// The two private-block sizes, as ONE function of the arguments.
///
/// ⛔ **Called by both the sizing DDI and the create DDI**, for the reason
/// `device12.rs`'s `device_private_size` states for the device block: a size
/// that is a function of the arguments at one site and a constant at the other
/// is a buffer overrun waiting for whichever caller disagrees. Here the stakes
/// are higher than there, because there are **two** sizes and pairing them the
/// wrong way round would put a `Box<HeapState>` pointer in the resource slot.
///
/// * The **heap** block exists only when the runtime asked for a heap. ⭐ This
///   is not economy, it is required: on the placed arm (`pCreateHeap == NULL`)
///   the `hHeap` the runtime passes must remain the *existing* heap's, and
///   asking for a fresh block would invite the runtime to hand a newly allocated
///   empty one instead.
/// * The **resource** block is asked for unconditionally, even on the heap-only
///   arm. `D3D12DDIARG_CREATERESOURCE_0109` has no field naming a heap, and
///   placement is expressed as `ReuseBufferGPUVA.hResource` — a *resource*
///   handle (`ResourceHeaps.md:1212`). Whether the runtime hands this driver an
///   `hResource` for a heap's own address range is not established here and can
///   only be settled on the VM, so one pointer-sized block is reserved for it:
///   if it arrives, [`create_heap_only`] records the heap span in it and the
///   placement chain resolves; if it does not, eight bytes go unused.
///
/// Both sizes are one machine word because that is what the `umd_common::slot`
/// encoding stores — the payload lives in a `Box` this driver owns and the
/// runtime's block holds only the pointer. `umd/src/forward/resource.rs:12-17`
/// is the D3D11 site that answers `8` for exactly this reason.
fn heap_and_resource_private_sizes(
    p_heap: *const ddi12::D3D12DDIARG_CREATEHEAP_0001,
    p_resource: *const ddi12::D3D12DDIARG_CREATERESOURCE_0109,
) -> ddi12::D3D12DDI_HEAP_AND_RESOURCE_SIZES {
    let word = core::mem::size_of::<*mut c_void>() as ddi12::SIZE_T;
    let both_null = p_heap.is_null() && p_resource.is_null();
    ddi12::D3D12DDI_HEAP_AND_RESOURCE_SIZES {
        Heap: if p_heap.is_null() { 0 } else { word },
        Resource: if both_null { 0 } else { word },
    }
}

/// `pfnCalcPrivateHeapAndResourceSizes`.
///
/// ⚠ Returns a **two-word struct by value**. On MSVC x64 that is an sret return
/// through a hidden pointer, not `RAX:RDX` — which is why the signature comes
/// from bindgen and this function is only ever installed by assigning it to the
/// table field (`DDI_REFERENCE.md` §7.3(1): *"do not hand-write it"*).
///
/// # Safety
/// `p_heap` and `p_resource`, when non-null, must be live for the call. Neither
/// is dereferenced here — only their nullness selects the answer.
unsafe extern "C" fn calc_private_heap_and_resource_sizes(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    p_heap: *const ddi12::D3D12DDIARG_CREATEHEAP_0001,
    p_resource: *const ddi12::D3D12DDIARG_CREATERESOURCE_0109,
    h_protected_session: ddi12::D3D12DDI_HPROTECTEDRESOURCESESSION_0030,
) -> ddi12::D3D12DDI_HEAP_AND_RESOURCE_SIZES {
    if !h_protected_session.pDrvPrivate.is_null() {
        note_refusal(&L4_REFUSALS.protected_session_ignored);
    }
    if p_heap.is_null() && p_resource.is_null() {
        // The fourth row of the arm table: illegal. Sizing cannot refuse -- the
        // DDI returns a struct -- so it answers zero and the paired create
        // refuses the same world explicitly.
        note_refusal(&L4_REFUSALS.heap_resource_calc_bad_arg);
    }
    heap_and_resource_private_sizes(p_heap, p_resource)
}

/// Create the whole-heap buffer that gives a standalone heap a CPU address and a
/// GPU virtual address.
///
/// ⭐ **Why this exists at all.** `pfnMapHeap` is heap-scoped and the D3D12 API
/// has no heap map; `pfnCheckResourceVirtualAddress` is resource-scoped and
/// `ID3D12Heap` has no GPU VA accessor. A buffer placed at offset 0 covering the
/// whole heap answers both, and it is the only construction reachable through
/// the public `ID3D12*` surface this driver forwards to.
///
/// ⚠ **This is the riskiest construct in the lane**, so it is arranged to fail
/// softly: it is attempted only when the heap's flags allow buffers, its failure
/// is counted rather than fatal, and a heap without one simply refuses
/// `pfnMapHeap` with its own counter. A heap that denies buffers (an RT/DS-only
/// heap on a tier-1 adapter, which is what `caps12.rs:570` reports) cannot have
/// one by construction, and such heaps are not CPU-mappable anyway.
///
/// The description is the D3D12 buffer canon — `Height`, `DepthOrArraySize` and
/// `MipLevels` all 1, `Format` UNKNOWN, one sample, `ROW_MAJOR` — and the
/// initial layout is `UNDEFINED`, which is the only layout a buffer may carry.
fn create_heap_span_buffer(
    device10: &ID3D12Device10,
    heap: &ID3D12Heap,
    byte_size: u64,
    ddi_heap_flags: ddi12::D3D12DDI_HEAP_FLAGS,
) -> Option<ID3D12Resource> {
    if ddi_heap_flags & v::HEAP_ALLOW_BUFFERS == 0 {
        L4_REFUSALS.heap_span_buffer_unavailable.bump();
        let n = L4_REFUSALS.heap_span_buffer_unavailable.get();
        if n <= LOG_BUDGET {
            log_error!(
                "L4: heap flags {ddi_heap_flags:#x} deny buffers, so this heap gets no span \
                 buffer -- MapHeap and CheckResourceVirtualAddress will refuse for it (x{n})"
            );
        }
        return None;
    }

    let desc = D3D12_RESOURCE_DESC1 {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: 0,
        Width: byte_size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT(0),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_NONE,
        SamplerFeedbackMipRegion: D3D12_MIP_REGION {
            Width: 0,
            Height: 0,
            Depth: 0,
        },
    };
    let mut out: Option<ID3D12Resource> = None;
    // SAFETY: `desc` is a live local of exactly the struct the entry point
    // names; `heap` is the engine heap this driver just created and holds a
    // reference to; `out` is a live local the engine writes an owned reference
    // into. No castable formats and no clear value for a plain buffer.
    let created = unsafe {
        device10.CreatePlacedResource2(
            heap,
            0,
            &desc,
            D3D12_BARRIER_LAYOUT_UNDEFINED,
            None,
            None,
            &mut out,
        )
    };
    match created {
        Ok(()) => out,
        Err(err) => {
            L4_REFUSALS.heap_span_buffer_unavailable.bump();
            let n = L4_REFUSALS.heap_span_buffer_unavailable.get();
            if n <= LOG_BUDGET {
                log_error!(
                    "L4: whole-heap span buffer ({byte_size} B) refused by the engine \
                     hr={:#010x} -- MapHeap and CheckResourceVirtualAddress will refuse for this \
                     heap (x{n})",
                    err.code().0 as u32,
                );
            }
            None
        }
    }
}

/// Report a heap node mask this single-node adapter did not expect.
///
/// ⛔ Reported, never rewritten. `ResourceHeaps.md:915`: *"the driver must
/// honour the requested memory pool and CPU page property exactly"* -- the same
/// posture applies to the node masks, and normalising one to 1 would be this
/// driver deciding which adapter the runtime meant. Both heap-creating arms call
/// it, because a committed resource creates an implicit heap from exactly the
/// same `D3D12DDIARG_CREATEHEAP_0001`.
fn note_node_masks(a: &ddi12::D3D12DDIARG_CREATEHEAP_0001) {
    if a.CreationNodeMask == 1 && a.VisibleNodeMask == 1 {
        return;
    }
    L4_REFUSALS.heap_node_mask_unexpected.bump();
    let n = L4_REFUSALS.heap_node_mask_unexpected.get();
    if n <= LOG_BUDGET {
        log_error!(
            "L4: heap CreationNodeMask={} VisibleNodeMask={} on a single-node adapter -- \
             forwarded unchanged (x{n})",
            a.CreationNodeMask,
            a.VisibleNodeMask,
        );
    }
}

/// Arm 2 of the fused create: `pCreateHeap` alone -> `ID3D12Device::CreateHeap`.
///
/// # Safety
/// `a` must be a live `D3D12DDIARG_CREATEHEAP_0001`, and the two slots must be
/// this driver's own private blocks, already cleared by the caller.
unsafe fn create_heap_only(
    device10: &ID3D12Device10,
    a: &ddi12::D3D12DDIARG_CREATEHEAP_0001,
    heap_slot: Slot<Boxed<HeapState>>,
    resource_slot: Option<Slot<Boxed<ResourceState>>>,
) -> Hresult {
    note_node_masks(a);

    // ⛔ A `PRIMARY` heap with no resource description contradicts the DDI's own
    // contract: `SPECS.md` §9.7 from `ResourceHeaps.md:897` says receiving the
    // flag *"obliges the driver to create a resource simultaneously with the
    // heap"*, i.e. a primary is always the committed arm. If it ever arrives here
    // the UP-4 identity table cannot record it -- there is no `ID3D12Resource` to
    // key on -- so the primary would be silently unrecorded. Counted, not
    // assumed away, because `note_presentable_identity`'s admission predicate
    // depends on the obligation holding.
    if a.Flags & v::HEAP_PRIMARY != 0 {
        note_refusal(&L4_REFUSALS.heap_primary_without_resource);
        log_error!(
            "L4: D3D12DDI_HEAP_FLAG_PRIMARY on the heap-ONLY arm -- ResourceHeaps.md:897 says a \
             primary heap must come with a resource description, so this primary has no resource \
             to record an identity against (size={} flags={:#x})",
            a.ByteSize,
            a.Flags,
        );
    }

    let desc = D3D12_HEAP_DESC {
        SizeInBytes: a.ByteSize,
        Properties: D3D12_HEAP_PROPERTIES {
            // ⭐ CUSTOM is forced, not chosen. `SPECS.md` §9.7 from
            // `D3D12GPUUploadHeaps.md:41`: *"there is exactly one heap-creation
            // argument struct in SDK 26100, `D3D12DDIARG_CREATEHEAP_0001`, and
            // it carries no heap type"*. The DDI hands a memory pool and a CPU
            // page property, which is precisely what a CUSTOM heap is; there is
            // no way to recover UPLOAD/READBACK/DEFAULT and no need to.
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: cpu_page_property(a.CPUPageProperty),
            MemoryPoolPreference: memory_pool(a.MemoryPool),
            CreationNodeMask: a.CreationNodeMask,
            VisibleNodeMask: a.VisibleNodeMask,
        },
        Alignment: a.Alignment,
        Flags: heap_flags(a.Flags),
    };

    let mut heap: Option<ID3D12Heap> = None;
    // SAFETY: `desc` is a live local of exactly the struct `CreateHeap` names,
    // and `heap` is a live local the engine writes an owned reference into.
    let created = unsafe { device10.CreateHeap(&desc, &mut heap) };
    // ⚠ The engine's own HRESULT is returned rather than a substituted one: it
    // reaches the application through `ID3D12Device::CreateHeap`, and vkd3d's
    // answers (`E_OUTOFMEMORY`, `E_INVALIDARG`) are more informative than
    // anything this driver could invent. ⛔ It is never rewritten to
    // `DXGI_ERROR_DRIVER_INTERNAL_ERROR`, which `DECISIONS.md` §7.5 reserves for
    // a genuine driver fault.
    let hr = match created {
        Ok(()) => S_OK,
        Err(err) => err.code().0,
    };
    let Some(heap) = heap.filter(|_| hr >= 0) else {
        note_refusal(&L4_REFUSALS.heap_create_engine_failed);
        log_error!(
            "L4: CreateHeap FAILED hr={:#010x} size={} align={} pool={} cpu={} flags={:#x}",
            hr as u32,
            a.ByteSize,
            a.Alignment,
            a.MemoryPool,
            a.CPUPageProperty,
            a.Flags,
        );
        return if hr < 0 { hr } else { E_FAIL };
    };

    let span_buffer = create_heap_span_buffer(device10, &heap, a.ByteSize, a.Flags);

    // The whole-heap object, when the runtime gave a block for it. It is the
    // placement parent every later `ReuseBufferGPUVA` will name.
    if let Some(resource_slot) = resource_slot {
        let desc1 = D3D12_RESOURCE_DESC1 {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: a.Alignment,
            Width: a.ByteSize,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT(0),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_NONE,
            SamplerFeedbackMipRegion: D3D12_MIP_REGION {
                Width: 0,
                Height: 0,
                Depth: 0,
            },
        };
        // SAFETY: `resource_slot` is this driver's own private block for the
        // object being created, cleared by the caller and written exactly once.
        unsafe {
            resource_slot.store(ResourceState {
                resource: span_buffer.clone(),
                span: Some(HeapSpan {
                    heap: heap.clone(),
                    base_offset: 0,
                }),
                desc: desc1,
                alloc_info: whole_heap_allocation_info(a.ByteSize, a.Alignment),
                // The heap-only arm: `pCreateHeap` was non-null, so the sizing
                // asked for a heap block and it is this driver's to reclaim.
                owns_heap_block: true,
            });
        }
    }

    // SAFETY: as above, for the heap's own block.
    unsafe {
        heap_slot.store(HeapState {
            heap: Some(heap),
            map_anchor: span_buffer,
            byte_size: a.ByteSize,
        });
    }
    S_OK
}

/// The allocation info this driver reports for a heap's own whole-heap object.
///
/// It is the heap as the runtime described it, with no additional driver data —
/// which is the honest answer, and the one the runtime's four
/// `AdditionalData*` checks (`DDI_REFERENCE.md` §9.7, strings:79-82) require.
fn whole_heap_allocation_info(
    byte_size: u64,
    alignment: u64,
) -> ddi12::D3D12DDI_RESOURCE_ALLOCATION_INFO_0022 {
    ddi12::D3D12DDI_RESOURCE_ALLOCATION_INFO_0022 {
        ResourceDataSize: byte_size,
        AdditionalDataHeaderSize: 0,
        AdditionalDataSize: 0,
        ResourceDataAlignment: u32::try_from(alignment).unwrap_or(0),
        AdditionalDataHeaderAlignment: 0,
        AdditionalDataAlignment: 0,
        Layout: v::TL_ROW_MAJOR,
        MipLevelSwizzleTransition: [0; 5],
        PlaneSliceSwizzleTransition: [0; 2],
    }
}

/// Record a `PRIMARY` committed resource in the UP-4 identity table.
///
/// `KMD_IMPACT.md` §14a.3 UP-4. Called from [`create_committed`] only, because
/// the primary declaration only reaches this driver there — see the three notes
/// below.
///
/// # ⛔ The admission predicate is `D3D12DDI_HEAP_FLAG_PRIMARY`, and it is the
/// only per-create primary signal that exists
///
/// `SPECS.md` §9.7 from `ResourceHeaps.md:897`: the flag *is* the D3D12
/// replacement for `DXGI_DDI_PRIMARY_DESC`, no primary description is ever passed
/// to the UMD, and receiving it obliges the driver to create a resource
/// simultaneously with the heap. So a primary is a **committed** create by
/// construction, and this is the one place the fact arrives.
///
/// ⚠ **§14a.3's "second signal" is NOT available here, and that is a real
/// mismatch rather than a naming difference.**
/// `D3D12DDI_RESOURCE_OPTIMIZATION_FLAG_PRIMARY = 4` is a
/// `D3D12DDI_RESOURCE_OPTIMIZATION_FLAGS` value, and that type appears on exactly
/// one DDI slot family — `pfnCheckResourceAllocationInfo`, in every generation
/// `_0003` through `_0109` (`umd12/bindgen/cached/d3d12umddi.rs:51734`, `:59866`,
/// `:75022`, `:76696`, `:79414`, `:87548`). It is **not** a field of
/// `D3D12DDIARG_CREATERESOURCE_0109` (`:87456-87473`, 16 fields, checked) and not
/// a parameter of `pfnCreateHeapAndResource` or
/// `pfnCalcPrivateHeapAndResourceSizes`. It arrives on a *sizing query* that
/// carries a description and no handle, for a resource that does not exist yet,
/// so it cannot be attributed to one. It is therefore **measured** rather than
/// used: [`check_resource_allocation_info`] splits it out of the
/// `ResourceOptimizationFlagsIgnored` aggregate into
/// `ResourceOptimizationPrimary`, which is the instrument that says whether the
/// signal arrives at all.
///
/// ⛔ **And the first signal's arrival is UNMEASURED**, which is why the split
/// above matters. §14a.3 states the trigger is *"already detected and counted:
/// `D3D12DDI_HEAP_FLAG_PRIMARY = 16` arrives and is dropped"*. The detection is
/// real; the arrival is not established. `HeapPrimaryFlagDropped` reads **0 in
/// all 150** logged `umd12` runs in `tmp/`, and none of those was a swapchain
/// workload, so the counter is untested rather than negative. If a real D3D12
/// present run leaves `HeapPrimaryFlagDropped` at 0 while
/// `ResourceOptimizationPrimary` is non-zero, then the heap flag is not this
/// runtime's channel and UP-5's predicate has to change — most plausibly to
/// latching the PRIMARY-flagged sizing query's description and matching it at the
/// next create. That correlation machinery is deliberately NOT written on
/// speculation; the two counters decide whether it is needed.
fn note_presentable_identity(
    resource: &ID3D12Resource,
    heap_arg: &ddi12::D3D12DDIARG_CREATEHEAP_0001,
    res_arg: &ddi12::D3D12DDIARG_CREATERESOURCE_0109,
) {
    if heap_arg.Flags & v::HEAP_PRIMARY == 0 {
        return;
    }

    let identity = identity12::PresentableIdentity {
        // ⚠ An identity token, never dereferenced by the table — see
        // `identity12`'s module doc for the whole argument, including how the
        // address-recycling hazard is closed.
        engine_resource: resource.as_raw() as usize,
        // ⛔ The two unresolved halves. They are zero because there is no path to
        // them from this crate yet, and each is counted below rather than
        // silently left at a plausible-looking 0:
        //   * the Vulkan half needs `ID3D12DXVKInteropDevice4::
        //     GetVulkanResourceMemoryInfo` (UP-2, which exists in the engine)
        //     reached through a `bridge12` entry point that does not exist;
        //   * the venus half needs the ICD's `helios_venus_memory_res_id` /
        //     `helios_venus_memory_alloc_info`, and this crate's bridge resolves
        //     the process ICD anchor for `venus_context_id()` alone.
        vk_memory: 0,
        memory_offset: 0,
        memory_size: 0,
        venus_res_id: 0,
        venus_alloc_size: 0,
        memory_type_index: 0,
        geometry: identity12::IdentityGeometry {
            width: res_arg.Width,
            height: res_arg.Height,
            depth_or_array_size: res_arg.DepthOrArraySize,
            mip_levels: res_arg.MipLevels,
            sample_count: res_arg.SampleDesc.Count,
            dxgi_format: res_arg.Format as u32,
        },
        heap_flags: heap_arg.Flags as u32,
    };

    match identity12::record(identity) {
        identity12::RecordOutcome::Inserted => {
            L4_REFUSALS.identity_recorded.bump();
        }
        identity12::RecordOutcome::Replaced => {
            L4_REFUSALS.identity_recorded.bump();
            note_refusal(&L4_REFUSALS.identity_replaced);
        }
        identity12::RecordOutcome::TableFull => {
            note_refusal(&L4_REFUSALS.identity_table_full);
            log_error!(
                "L4: identity table FULL -- the primary at {:#x} is NOT recorded and cannot be \
                 presented; {}x{} fmt={} heapFlags={:#x}",
                identity.engine_resource,
                identity.geometry.width,
                identity.geometry.height,
                identity.geometry.dxgi_format,
                identity.heap_flags,
            );
            return;
        }
    }

    if identity.vk_memory == 0 {
        L4_REFUSALS.identity_vk_memory_unresolved.bump();
    }
    if identity.venus_res_id == 0 {
        L4_REFUSALS.identity_venus_unresolved.bump();
    }

    let n = L4_REFUSALS.identity_recorded.get();
    if n <= LOG_BUDGET {
        // ⚠ Every field is printed, and that is not only for the log: an entry
        // whose fields no code reads is `dead_code`, and a table whose contents
        // cannot be seen from a run is the T5 lesson (*an instrument nothing can
        // read is not an instrument*) rebuilt. Until UP-5 consumes the table this
        // line is the whole readout.
        log_error!(
            "L4: presentable identity recorded res={:#x} {}x{}x{} mips={} samples={} fmt={} \
             heapFlags={:#x} vk_memory={:#x} off={} mem_size={} mti={} venus_res_id={} \
             venus_alloc_size={} (x{n})",
            identity.engine_resource,
            identity.geometry.width,
            identity.geometry.height,
            identity.geometry.depth_or_array_size,
            identity.geometry.mip_levels,
            identity.geometry.sample_count,
            identity.geometry.dxgi_format,
            identity.heap_flags,
            identity.vk_memory,
            identity.memory_offset,
            identity.memory_size,
            identity.memory_type_index,
            identity.venus_res_id,
            identity.venus_alloc_size,
        );
    }
}

/// Arm 1 of the fused create: both argument pointers ->
/// `ID3D12Device10::CreateCommittedResource3`.
///
/// ⭐ `ResourceHeaps.md:1186` records why the call is fused at all: *"the spec's
/// stated reason for fusing the call is to let the driver perform a single VA
/// commit instead of a reserve-then-commit pair"* — which is exactly what
/// `CreateCommittedResource3` does, so the fusion survives the forward.
///
/// The heap block is written too, with no `ID3D12Heap` (a committed resource's
/// heap is implicit and has no API object) and with the resource as its map
/// anchor: a committed resource starts at offset 0 of its own implicit heap, so
/// its mapped base **is** the heap's base.
///
/// # Safety
/// `heap_arg` and `res_arg` must be live for the call; both slots must be this
/// driver's own private blocks, already cleared by the caller.
unsafe fn create_committed(
    device10: &ID3D12Device10,
    heap_arg: &ddi12::D3D12DDIARG_CREATEHEAP_0001,
    res_arg: &ddi12::D3D12DDIARG_CREATERESOURCE_0109,
    p_clear: *const ddi12::D3D12DDI_CLEAR_VALUES,
    heap_slot: Slot<Boxed<HeapState>>,
    resource_slot: Slot<Boxed<ResourceState>>,
) -> Hresult {
    note_node_masks(heap_arg);
    // SAFETY: `res_arg` is live per the caller's guarantee.
    let Some(desc) = (unsafe { resource_desc1(res_arg) }) else {
        note_refusal(&L4_REFUSALS.heap_resource_create_bad_arg);
        log_error!(
            "L4: CreateCommittedResource with unknown D3D12DDI_RESOURCE_TYPE {}",
            res_arg.ResourceType,
        );
        return E_INVALIDARG;
    };
    let properties = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_CUSTOM,
        CPUPageProperty: cpu_page_property(heap_arg.CPUPageProperty),
        MemoryPoolPreference: memory_pool(heap_arg.MemoryPool),
        CreationNodeMask: heap_arg.CreationNodeMask,
        VisibleNodeMask: heap_arg.VisibleNodeMask,
    };
    // SAFETY: `p_clear` is `_In_opt_` and live for the call per the caller.
    let clear = unsafe { clear_value(p_clear, res_arg.Flags) };
    let is_buffer = desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER;
    // SAFETY: `res_arg` is live; the slice is bounded and used only for this
    // call.
    let castable = unsafe { castable_formats(res_arg) };

    let mut out: Option<ID3D12Resource> = None;
    // SAFETY: every pointer argument is a live local or a bounded slice built
    // above; `out` is a live local the engine writes an owned reference into.
    // The protected-session parameter is explicitly `None`: this driver reports
    // no protected-resource support, and an arriving session is counted in
    // `create_heap_and_resource` rather than forwarded.
    let created = unsafe {
        device10.CreateCommittedResource3(
            &properties,
            heap_flags(heap_arg.Flags),
            &desc,
            barrier_layout(res_arg.InitialBarrierLayout, is_buffer),
            clear.as_ref().map(core::ptr::from_ref),
            None::<&ID3D12ProtectedResourceSession>,
            castable,
            &mut out,
        )
    };
    // ⚠ As `create_heap_only`: the engine's HRESULT is forwarded, not replaced.
    let hr = match created {
        Ok(()) => S_OK,
        Err(err) => err.code().0,
    };
    let Some(resource) = out.filter(|_| hr >= 0) else {
        note_refusal(&L4_REFUSALS.resource_create_engine_failed);
        log_error!(
            "L4: CreateCommittedResource3 FAILED hr={:#010x} type={} fmt={} {}x{}x{} mips={} \
             samples={} flags={:#x} layout={} heapFlags={:#x}",
            hr as u32,
            res_arg.ResourceType,
            res_arg.Format,
            res_arg.Width,
            res_arg.Height,
            res_arg.DepthOrArraySize,
            res_arg.MipLevels,
            res_arg.SampleDesc.Count,
            res_arg.Flags,
            res_arg.InitialBarrierLayout,
            heap_arg.Flags,
        );
        return if hr < 0 { hr } else { E_FAIL };
    };

    // UP-4: if this is the swapchain primary, remember how to find its memory.
    // Before the state is stored, so a `TableFull` refusal is not attributed to
    // the create -- it never fails the create, it only makes the resource
    // un-presentable, and UP-5 is the commit that turns that into an error.
    note_presentable_identity(&resource, heap_arg, res_arg);

    // SAFETY: `resource_slot` is this driver's private block for the resource
    // being created, cleared by the caller and written exactly once.
    unsafe {
        resource_slot.store(ResourceState {
            resource: Some(resource.clone()),
            // ⚠ No span: a committed resource's heap has no `ID3D12Heap`, so it
            // can never be a placement parent. A later placed create naming it
            // is counted by `ResourcePlacementUnresolved` rather than silently
            // mis-placed.
            span: None,
            desc,
            // ⚠ The visible-node mask comes from the heap the runtime asked
            // for, not from a constant: it is the one arm where the DDI states
            // it. The placed arm has no heap argument and passes 1, which is
            // this single-node adapter's only legal value (`misc.rs`'s
            // `HELIOS_PHYSICAL_ADAPTER_COUNT`).
            alloc_info: allocation_info_from_engine(
                device10,
                &desc,
                res_arg,
                heap_arg.VisibleNodeMask,
            ),
            // The committed arm: both arg pointers were non-null, so the sizing
            // asked for a heap block and the implicit heap stored below is this
            // driver's to reclaim.
            owns_heap_block: true,
        });
    }
    // SAFETY: as above, for the implicit heap's block.
    unsafe {
        heap_slot.store(HeapState {
            heap: None,
            map_anchor: Some(resource),
            byte_size: heap_arg.ByteSize,
        });
    }
    S_OK
}

/// Arm 3 of the fused create: `pCreateResource` alone.
///
/// `ReuseBufferGPUVA.BaseAddress.UMD.hResource` is the spec's discriminator:
/// non-NULL is a **placed** resource whose parent's span gives the engine heap
/// and base offset; NULL is a **reserved** (tiled) resource
/// (`ResourceHeaps.md:1204`), which this driver refuses because `caps12.rs:278`
/// reports `TiledResourcesTier = NOT_SUPPORTED` and creating one would
/// contradict the caps this device was accepted on.
///
/// # ⭐ Two ways to find the target heap, tried in the spec's order
///
/// 1. **`ReuseBufferGPUVA`**, which is what `ResourceHeaps.md:1212` documents as
///    the only placement parent the DDI can express. Resolving the parent
///    resource yields its [`HeapSpan`], i.e. an engine heap and the parent's own
///    base offset within it; the child lands at `base + requested`.
/// 2. **`hHeap`**, the argument `pfnCreateHeapAndResource` carries on *every*
///    arm. ⚠ Whether the runtime populates it on the placed arm is not
///    established by any document in this set, so it is a **fallback and not the
///    primary**: taking it first would make the driver depend on behaviour
///    nobody has measured, while taking it second turns an unresolved
///    `ReuseBufferGPUVA` from a refusal into a working create wherever the
///    runtime does supply it. Which path was taken is visible, because path 2
///    only runs after path 1 has already bumped `ResourcePlacementUnresolved`.
///
/// ⛔ **The reserved test comes AFTER both paths, not before, and the ordering
/// is load-bearing.** Testing `hResource == NULL` first made path 2 unreachable
/// on every possible input — a NULL parent returned `E_NOTIMPL` before `hHeap`
/// was ever consulted, and a non-NULL one that failed to resolve then landed on
/// a slot the caller had just nulled. An unreachable fallback is not an
/// instrument, and `ResourcePlacementUnresolved` could never have read non-zero
/// with placed creates still succeeding, which is exactly the gate check the
/// lane's own §6 U3 rests on. So a resource is declared *reserved* only when
/// **neither** channel names a heap: no `ReuseBufferGPUVA` parent and no usable
/// `hHeap`. ⚠ The residual risk is the converse — a genuine reserved create that
/// arrives carrying a live `hHeap` would be placed rather than refused. That
/// cannot happen through the sizing contract this driver states (a reserved
/// resource has no heap for the runtime to name), and if it ever does it shows
/// up as a `CreatePlacedResource2` that succeeded for a resource `caps12` says
/// cannot exist, not as a silent wrong answer.
///
/// # Safety
/// `res_arg` must be live for the call, and `resource_slot` must be this
/// driver's own private block, already cleared by the caller.
unsafe fn create_placed_or_reserved(
    device10: &ID3D12Device10,
    res_arg: &ddi12::D3D12DDIARG_CREATERESOURCE_0109,
    h_heap: ddi12::D3D12DDI_HHEAP,
    p_clear: *const ddi12::D3D12DDI_CLEAR_VALUES,
    resource_slot: Slot<Boxed<ResourceState>>,
) -> Hresult {
    // SAFETY: the union has exactly one member, `UMD`, so any initialisation the
    // runtime performed makes this read defined. `D3D12DDIARG_BUFFER_PLACEMENT`
    // is a by-value field of the live `res_arg`.
    let placement = unsafe { res_arg.ReuseBufferGPUVA.BaseAddress.UMD };
    let names_parent = !placement.hResource.pDrvPrivate.is_null();

    // Path 1 -- the documented placement parent.
    let via_parent = if names_parent {
        // SAFETY: the runtime named a resource handle it obtained from this
        // driver's own create, so the block is ours; the borrow ends inside this
        // function.
        let parent = unsafe { resource_state(placement.hResource) };
        let resolved = parent
            .and_then(|p| p.span.as_ref())
            .and_then(|span| Some((&span.heap, span.base_offset.checked_add(placement.Offset)?)));
        if resolved.is_none() {
            L4_REFUSALS.resource_placement_unresolved.bump();
            let n = L4_REFUSALS.resource_placement_unresolved.get();
            if n <= LOG_BUDGET {
                log_error!(
                    "L4: ReuseBufferGPUVA hResource={:p} offset={} did not resolve to an engine \
                     heap -- falling back to hHeap (x{n})",
                    placement.hResource.pDrvPrivate,
                    placement.Offset,
                );
            }
        }
        resolved
    } else {
        None
    };

    // Path 2 -- the fallback, now reachable from both directions: a named parent
    // that did not resolve, and no named parent at all.
    let target = match via_parent {
        Some(target) => Some(target),
        // SAFETY: `h_heap`, when non-null, is a heap block this driver wrote --
        // and on this arm the caller deliberately did NOT clear it, because the
        // paired sizing call asked for no heap block. The borrow ends inside
        // this function.
        None => unsafe { heap_state(h_heap) }
            .and_then(|state| state.heap.as_ref())
            .map(|heap| (heap, placement.Offset)),
    };

    let Some((heap, heap_offset)) = target else {
        if !names_parent {
            // ⛔ Both channels are empty, which is `ResourceHeaps.md:1204`'s
            // definition of a reserved (tiled) resource.
            note_refusal(&L4_REFUSALS.resource_reserved_refused);
            log_error!(
                "L4: reserved (tiled) resource refused -- no ReuseBufferGPUVA parent and no \
                 hHeap, and this driver reports TiledResourcesTier = NOT_SUPPORTED (caps12), so \
                 no tiled resource may exist. type={} fmt={} {}x{}",
                res_arg.ResourceType,
                res_arg.Format,
                res_arg.Width,
                res_arg.Height,
            );
            return E_NOTIMPL;
        }
        log_error!(
            "L4: placed resource refused -- neither ReuseBufferGPUVA nor hHeap names an engine \
             heap this driver owns"
        );
        return E_INVALIDARG;
    };

    // SAFETY: `res_arg` is live per the caller's guarantee.
    let Some(desc) = (unsafe { resource_desc1(res_arg) }) else {
        note_refusal(&L4_REFUSALS.heap_resource_create_bad_arg);
        log_error!(
            "L4: CreatePlacedResource with unknown D3D12DDI_RESOURCE_TYPE {}",
            res_arg.ResourceType,
        );
        return E_INVALIDARG;
    };
    // SAFETY: `_In_opt_` and live for the call per the caller.
    let clear = unsafe { clear_value(p_clear, res_arg.Flags) };
    let is_buffer = desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER;
    // SAFETY: `res_arg` is live; the slice is bounded and used only for this
    // call.
    let castable = unsafe { castable_formats(res_arg) };

    let mut out: Option<ID3D12Resource> = None;
    // SAFETY: as `create_committed`.
    let created = unsafe {
        device10.CreatePlacedResource2(
            heap,
            heap_offset,
            &desc,
            barrier_layout(res_arg.InitialBarrierLayout, is_buffer),
            clear.as_ref().map(core::ptr::from_ref),
            castable,
            &mut out,
        )
    };
    // ⚠ As `create_heap_only`: the engine's HRESULT is forwarded, not replaced.
    let hr = match created {
        Ok(()) => S_OK,
        Err(err) => err.code().0,
    };
    let Some(resource) = out.filter(|_| hr >= 0) else {
        note_refusal(&L4_REFUSALS.resource_create_engine_failed);
        log_error!(
            "L4: CreatePlacedResource2 FAILED hr={:#010x} at heap offset {heap_offset} type={} \
             fmt={} {}x{}x{} mips={} flags={:#x}",
            hr as u32,
            res_arg.ResourceType,
            res_arg.Format,
            res_arg.Width,
            res_arg.Height,
            res_arg.DepthOrArraySize,
            res_arg.MipLevels,
            res_arg.Flags,
        );
        return if hr < 0 { hr } else { E_FAIL };
    };

    // SAFETY: this driver's own private block for the resource, cleared by the
    // caller and written exactly once.
    unsafe {
        resource_slot.store(ResourceState {
            resource: Some(resource),
            span: Some(HeapSpan {
                heap: heap.clone(),
                base_offset: heap_offset,
            }),
            desc,
            alloc_info: allocation_info_from_engine(device10, &desc, res_arg, 1),
            // ⛔ The placed arm: `pCreateHeap` was NULL, the sizing answered
            // `Heap = 0`, and the `hHeap` this create was handed is an
            // **already-live** heap owned by its own create. Destroying this
            // resource must not touch it.
            owns_heap_block: false,
        });
    }
    S_OK
}

/// The castable-format list, borrowed from the runtime's array.
///
/// `DXGI_FORMAT` is `#[repr(transparent)]` over `i32` in the `windows` crate and
/// the DDI's `DXGI_FORMAT` is a `c_int`, so the array can be viewed in place
/// with no copy and no transmute of a compound type. The count is runtime data,
/// so it is bounded: DXGI defines fewer than 200 formats and anything above
/// [`CASTABLE_FORMAT_LIMIT`] is not a list.
///
/// # Safety
/// `a` must be live, and `a.pCastableFormats` must address
/// `a.NumCastableFormats` `DXGI_FORMAT`s for the duration of the call.
unsafe fn castable_formats(
    a: &ddi12::D3D12DDIARG_CREATERESOURCE_0109,
) -> Option<&[DXGI_FORMAT]> {
    let count = a.NumCastableFormats as usize;
    if count == 0 || a.pCastableFormats.is_null() {
        return None;
    }
    if count > CASTABLE_FORMAT_LIMIT {
        L4_REFUSALS.castable_formats_dropped.bump();
        let n = L4_REFUSALS.castable_formats_dropped.get();
        if n <= LOG_BUDGET {
            log_error!(
                "L4: NumCastableFormats={count} exceeds the {CASTABLE_FORMAT_LIMIT} bound -- the \
                 list is dropped rather than read (x{n})"
            );
        }
        return None;
    }
    // SAFETY: non-null and `count` elements per the caller's guarantee, and
    // `DXGI_FORMAT` is `#[repr(transparent)]` over the `i32` the DDI array
    // holds, so the element type is layout-identical.
    Some(unsafe { core::slice::from_raw_parts(a.pCastableFormats.cast::<DXGI_FORMAT>(), count) })
}

/// `pfnCreateHeapAndResource` — the fused create.
///
/// # Safety
/// `p_heap` / `p_resource` / `p_clear`, when non-null, must be live for the
/// call. `h_heap.pDrvPrivate` and `h_resource.pDrvPrivate` must address the
/// blocks the paired [`calc_private_heap_and_resource_sizes`] sized.
///
/// ⚠ `_h_rt_resource` is unused on purpose; see the note under [`ResourceState`]
/// for why the runtime resource handle is not stored.
unsafe extern "C" fn create_heap_and_resource(
    h_device: ddi12::D3D12DDI_HDEVICE,
    p_heap: *const ddi12::D3D12DDIARG_CREATEHEAP_0001,
    h_heap: ddi12::D3D12DDI_HHEAP,
    _h_rt_resource: ddi12::D3D12DDI_HRTRESOURCE,
    p_resource: *const ddi12::D3D12DDIARG_CREATERESOURCE_0109,
    p_clear: *const ddi12::D3D12DDI_CLEAR_VALUES,
    h_protected_session: ddi12::D3D12DDI_HPROTECTEDRESOURCESESSION_0030,
    h_resource: ddi12::D3D12DDI_HRESOURCE,
) -> Hresult {
    // ⛔ **Only a slot the paired sizing call ASKED FOR may be cleared**, and
    // this is the single most dangerous line in the lane if it is wrong. Every
    // Create nulls the private blocks of the objects it is creating, so a failed
    // create leaves a null handle rather than stale garbage
    // (`umd_common::slot`'s `DdiHandle` doc) — but on the placed arm
    // `pCreateHeap` is NULL, [`heap_and_resource_private_sizes`] answers
    // `Heap = 0` on purpose, and the `hHeap` the runtime then passes is the
    // **already-live** heap's handle rather than a fresh block. Clearing that
    // one would null a slot whose `Box<HeapState>` this driver still owns:
    // `Slot::clear` is documented as nulling the word *without* touching what it
    // pointed at, so the `ID3D12Heap` and its map anchor would leak, and every
    // later `pfnMapHeap` / `pfnUnmapHeap` / `pfnDestroyHeapAndResource` for that
    // heap would find nothing and refuse with `ResourceHandleUnresolved`. It
    // would also falsify [`heap_state`]'s own soundness argument, which rests on
    // the box being written once and taken once.
    //
    // ⇒ the expected sizes are computed FIRST, from the same function that told
    // the runtime how many bytes to allocate, and each slot is cleared only when
    // that function asked for it.
    let expected = heap_and_resource_private_sizes(p_heap, p_resource);
    // SAFETY: each slot, when non-null, is this driver's own private block;
    // clearing writes one machine word and touches nothing it pointed at.
    let heap_slot = unsafe { Slot::<Boxed<HeapState>>::from_priv(h_heap.pDrvPrivate) };
    if expected.Heap != 0 {
        if let Some(slot) = heap_slot {
            // SAFETY: as above, and the block is one this create is building.
            unsafe { slot.clear() };
        }
    }
    // SAFETY: as above, for the resource's block; `from_priv` only records
    // non-nullness and dereferences nothing.
    let resource_slot = unsafe { Slot::<Boxed<ResourceState>>::from_priv(h_resource.pDrvPrivate) };
    if expected.Resource != 0 {
        if let Some(slot) = resource_slot {
            // SAFETY: as above, and the block is one this create is building.
            unsafe { slot.clear() };
        }
    }

    if !h_protected_session.pDrvPrivate.is_null() {
        note_refusal(&L4_REFUSALS.protected_session_ignored);
    }

    // SAFETY: device-scope DDI, so the runtime passes a handle `create_device`
    // returned `S_OK` for; the borrow ends with this call.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&L4_REFUSALS.resource_no_device);
        return E_INVALIDARG;
    };
    let Some(device10) = engine_device10(dev) else {
        return E_NOTIMPL;
    };

    // ⛔ The size site and the write site agree by construction: the same
    // function that told the runtime how many bytes to allocate now says which
    // slots must exist. A block the sizing call asked for that did not arrive is
    // a refusal, not something to work around.
    //
    // ⚠ **One-directional, deliberately.** The converse -- a slot arriving that
    // the sizing call did not ask for -- is NOT an error, and asserting it was
    // would break the arm this driver most needs to work: on the placed arm the
    // sizing call answers `Heap = 0` (see [`heap_and_resource_private_sizes`])
    // precisely so the runtime keeps passing the *existing* heap's `hHeap`, and
    // that handle is non-null by design. Only "asked for and absent" is a fault.
    // (`expected` is the same value computed at the top of this function, where
    // it also decides which slots may be cleared.)
    if expected.Heap != 0 && heap_slot.is_none() {
        note_refusal(&L4_REFUSALS.heap_resource_create_bad_arg);
        log_error!(
            "L4: CreateHeapAndResource private-block mismatch -- sized Heap={} but hHeap is null",
            expected.Heap,
        );
        return E_INVALIDARG;
    }
    // ⚠ Only the description-present case is a fault: the heap-only arm asks for
    // a resource block speculatively (see the sizing function's second bullet),
    // so a runtime that declines to supply one there is not misbehaving.
    if !p_resource.is_null() && resource_slot.is_none() {
        note_refusal(&L4_REFUSALS.heap_resource_create_bad_arg);
        log_error!(
            "L4: CreateHeapAndResource private-block mismatch -- sized Resource={} but hResource \
             is null",
            expected.Resource,
        );
        return E_INVALIDARG;
    }

    // SAFETY: both pointers are `_In_opt_ CONST` and live for the call when
    // non-null, per the caller's guarantee; each arm reads only its own arg.
    let heap_arg = unsafe { p_heap.as_ref() };
    let res_arg = unsafe { p_resource.as_ref() };

    // ⛔ `CreateAtVirtualAddress` and `pRowMajorLayout` cannot be honoured, and
    // that is counted rather than passed over. `DDI_REFERENCE.md` §9.7 records
    // that `RecreateAt` is gated behind DDI 0111, which is absent from SDK
    // 10.0.26100.0 — so a non-zero address arriving means the build assumption
    // behind this driver's `RecreateAtTier = NOT_SUPPORTED` answer is wrong.
    if let Some(a) = res_arg {
        if a.CreateAtVirtualAddress != 0 {
            note_refusal(&L4_REFUSALS.resource_create_at_virtual_address);
            log_error!(
                "L4: CreateAtVirtualAddress={:#x} arrived, but RecreateAt is gated behind DDI \
                 0111 which this SDK does not define -- ignored",
                a.CreateAtVirtualAddress,
            );
        }
        if !a.pRowMajorLayout.is_null() {
            note_refusal(&L4_REFUSALS.resource_row_major_layout_ignored);
        }
    }

    match (heap_arg, res_arg) {
        (Some(heap_arg), Some(res_arg)) => {
            let Some(resource_slot) = resource_slot else {
                note_refusal(&L4_REFUSALS.heap_resource_create_bad_arg);
                return E_INVALIDARG;
            };
            let Some(heap_slot) = heap_slot else {
                note_refusal(&L4_REFUSALS.heap_resource_create_bad_arg);
                return E_INVALIDARG;
            };
            // SAFETY: both args are live references; both slots are this
            // driver's own blocks, cleared above.
            unsafe {
                create_committed(
                    &device10,
                    heap_arg,
                    res_arg,
                    p_clear,
                    heap_slot,
                    resource_slot,
                )
            }
        }
        (Some(heap_arg), None) => {
            let Some(heap_slot) = heap_slot else {
                note_refusal(&L4_REFUSALS.heap_resource_create_bad_arg);
                return E_INVALIDARG;
            };
            // SAFETY: as above.
            unsafe {
                create_heap_only(&device10, heap_arg, heap_slot, resource_slot)
            }
        }
        (None, Some(res_arg)) => {
            let Some(resource_slot) = resource_slot else {
                note_refusal(&L4_REFUSALS.heap_resource_create_bad_arg);
                return E_INVALIDARG;
            };
            // SAFETY: as above.
            unsafe {
                create_placed_or_reserved(
                    &device10,
                    res_arg,
                    h_heap,
                    p_clear,
                    resource_slot,
                )
            }
        }
        (None, None) => {
            // Row four of the arm table.
            note_refusal(&L4_REFUSALS.heap_resource_create_bad_arg);
            log_error!("L4: CreateHeapAndResource with neither a heap nor a resource description");
            E_INVALIDARG
        }
    }
}

/// `pfnDestroyHeapAndResource`.
///
/// Returns `VOID`, so the only channel is the counters (`DECISIONS.md` §7.6).
/// The resource is dropped before the heap: a placed resource holds a reference
/// to its heap, so releasing in that order keeps the engine's own teardown in
/// the order it would see from an application.
///
/// # Safety
/// `h_heap` and `h_resource`, when their `pDrvPrivate` is non-null, must be
/// handles [`create_heap_and_resource`] returned `S_OK` for and which have not
/// already been destroyed. Passing one twice is a double free.
unsafe extern "C" fn destroy_heap_and_resource(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_heap: ddi12::D3D12DDI_HHEAP,
    h_resource: ddi12::D3D12DDI_HRESOURCE,
) {
    let mut seen = false;

    // ⛔ **Whether this driver may reclaim `hHeap` is decided by the RESOURCE**,
    // not by `hHeap`'s own contents, and the default is "no". `ResourceState`'s
    // `owns_heap_block` doc carries the whole argument; the short version is that
    // on the placed arm `hHeap` is an already-live heap belonging to a different
    // create, so taking its box would tear down a live `ID3D12Heap` under its
    // owner.
    //
    // ⚠ Defaulting to `false` when the resource block does not resolve is the
    // deliberate direction: not reclaiming is a leak, which is counted below and
    // is diagnosable; reclaiming something that is not ours is a use-after-free
    // in the runtime's own object graph, which is not.
    let mut owns_heap_block = false;

    // SAFETY: the caller guarantees a live, not-yet-destroyed handle; `take`
    // nulls the slot first, so a second call finds nothing and does nothing.
    if let Some(slot) = unsafe { Slot::<Boxed<ResourceState>>::from_priv(h_resource.pDrvPrivate) } {
        // SAFETY: as above.
        if let Some(state) = unsafe { slot.take() } {
            owns_heap_block = state.owns_heap_block;
            // ⛔ UP-4: the identity dies with the resource, and it dies HERE
            // because this is the only site that ever retires an
            // `ID3D12Resource` this driver created. The engine address is read
            // out of the state box while the box is still alive and the COM
            // reference it holds is still valid, so the key that is removed is
            // provably the key `note_presentable_identity` inserted -- both are
            // `ID3D12Resource::as_raw()` on the same object.
            //
            // ⚠ Ordering: before `drop(state)`. After the drop the resource may
            // be released, its address may be recycled, and removing it then
            // could delete an entry a *different* create had just inserted at
            // the same address. Reading the address first makes the removal
            // unconditionally about this object.
            //
            // ⚠ Unconditional: an ordinary non-presentable resource has no
            // entry, `remove` finds nothing, returns false and nothing is
            // counted. `IdentityRecorded - IdentityRemoved` is therefore the
            // live-entry count and a leak shows up as a growing difference
            // rather than needing its own instrument.
            if let Some(engine) = state.resource.as_ref() {
                if identity12::remove(engine.as_raw() as usize) {
                    L4_REFUSALS.identity_removed.bump();
                }
            }
            drop(state);
            seen = true;
        }
    }

    if owns_heap_block {
        // SAFETY: as above, for the heap.
        if let Some(slot) = unsafe { Slot::<Boxed<HeapState>>::from_priv(h_heap.pDrvPrivate) } {
            // SAFETY: as above.
            if let Some(state) = unsafe { slot.take() } {
                drop(state);
                seen = true;
            } else {
                // The paired create asked for a heap block and the destroy found
                // none: the `ID3D12Heap` and its map anchor are now unreachable.
                // ⚠ A separate counter from `ResourceHandleUnresolved` because
                // `seen` is already true here — the resource resolved — so the
                // aggregate could never have shown this.
                note_refusal(&L4_REFUSALS.heap_block_unreclaimed);
            }
        } else if !h_heap.pDrvPrivate.is_null() {
            note_refusal(&L4_REFUSALS.heap_block_unreclaimed);
        }
    }

    if !seen {
        note_refusal(&L4_REFUSALS.resource_handle_unresolved);
    }
}

// ---------------------------------------------------------------------------
// (g) pfnMapHeap / pfnUnmapHeap
// ---------------------------------------------------------------------------

/// `pfnMapHeap`.
///
/// ⭐ **Map is heap-scoped at the DDI and resource-scoped in the API.** The
/// runtime maps a heap once, ref-counts and serialises `pfnMapHeap` /
/// `pfnUnmapHeap` across application threads so the driver sees at most one live
/// mapping per heap (`ResourceHeaps.md:454`), and derives every per-resource
/// pointer from `pfnCheckSubresourceInfo` (`:1437`). This driver has no heap map
/// to forward to, so it maps the heap's anchor resource — see
/// [`HeapState::map_anchor`].
///
/// ⚠ Both ranges are passed as `NULL`, which is the API's *"the CPU may read /
/// may have written the whole resource"*. The DDI carries no range at all, so
/// the conservative direction is the only defensible one: a narrower claim would
/// be this driver inventing a promise on the application's behalf.
///
/// The D3D11 precedent is `umd/src/forward/transfer.rs:182-229`, which forwards
/// `pfnResourceMap` to the engine's `Map` and copies out the pointer; the shape
/// here is the same one, moved up to the heap.
///
/// # Safety
/// `h_heap` must be a live heap handle from [`create_heap_and_resource`], and
/// `out` must address one writable pointer the runtime owns.
unsafe extern "C" fn map_heap(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_heap: ddi12::D3D12DDI_HHEAP,
    out: *mut *mut c_void,
) -> Hresult {
    if out.is_null() {
        note_refusal(&L4_REFUSALS.heap_resource_create_bad_arg);
        return E_INVALIDARG;
    }
    // ⛔ Written before anything can fail, so no path leaves the runtime reading
    // whatever was on its stack as a heap base address.
    // SAFETY: non-null per the check; the DDI declares it an out-parameter.
    unsafe { core::ptr::write_unaligned(out, core::ptr::null_mut()) };

    // SAFETY: the runtime passes a heap handle this driver wrote; the borrow
    // ends with this call.
    let Some(state) = (unsafe { heap_state(h_heap) }) else {
        note_refusal(&L4_REFUSALS.resource_handle_unresolved);
        return E_INVALIDARG;
    };
    let Some(anchor) = state.map_anchor.as_ref() else {
        L4_REFUSALS.map_heap_no_anchor.bump();
        let n = L4_REFUSALS.map_heap_no_anchor.get();
        if n <= LOG_BUDGET {
            log_error!(
                "L4: MapHeap on a {} B heap with no mappable anchor -- see \
                 HeapSpanBufferUnavailable for why this heap has none (x{n})",
                state.byte_size,
            );
        }
        return E_NOTIMPL;
    };

    let mut data: *mut c_void = core::ptr::null_mut();
    // SAFETY: `anchor` is the engine resource this driver holds a reference to
    // for the heap's whole life; `data` is a live local. A null read range means
    // "the CPU may read all of it", which is the conservative reading.
    let mapped = unsafe { anchor.Map(0, None, Some(&mut data)) };
    if let Err(err) = mapped {
        L4_REFUSALS.map_heap_engine_failed.bump();
        let n = L4_REFUSALS.map_heap_engine_failed.get();
        if n <= LOG_BUDGET {
            log_error!(
                "L4: MapHeap: the engine refused Map on the anchor hr={:#010x} (x{n})",
                err.code().0 as u32,
            );
        }
        return err.code().0;
    }
    // SAFETY: as the first write above.
    unsafe { core::ptr::write_unaligned(out, data) };
    L4_REFUSALS.map_heap_calls.bump();
    S_OK
}

/// `pfnUnmapHeap`.
///
/// A null written-range is the conservative direction for the same reason
/// [`map_heap`]'s null read-range is: the DDI carries no range, so claiming the
/// CPU wrote nothing would be this driver making a promise it cannot check.
///
/// # Safety
/// `h_heap` must be a live heap handle that [`map_heap`] returned `S_OK` for.
unsafe extern "C" fn unmap_heap(_h_device: ddi12::D3D12DDI_HDEVICE, h_heap: ddi12::D3D12DDI_HHEAP) {
    // SAFETY: as `map_heap`.
    let Some(state) = (unsafe { heap_state(h_heap) }) else {
        note_refusal(&L4_REFUSALS.resource_handle_unresolved);
        return;
    };
    let Some(anchor) = state.map_anchor.as_ref() else {
        // A heap that could not be mapped cannot be unmapped. `map_heap` already
        // refused and counted; counting again here would double every event.
        return;
    };
    // SAFETY: `anchor` is the engine resource this driver holds for the heap's
    // whole life; `Unmap` on a resource that is not mapped is a documented
    // no-op, so an unbalanced call cannot corrupt anything.
    unsafe { anchor.Unmap(0, None) };
}

// ---------------------------------------------------------------------------
// (g) pfnMakeResident / pfnEvict / pfnOfferResources / pfnReclaimResources
// ---------------------------------------------------------------------------

/// Zero a paging-fence array the runtime handed as an out-parameter.
///
/// ⛔ **This is the `pfnQueryNodeMap` class, and it already cost this project a
/// device once** (`forward12/misc.rs`): an `_Out_` the driver leaves untouched
/// is the runtime reading its own uninitialised memory as a value it acts on.
/// `D3D12DDIARG_MAKERESIDENT_0001::pPagingFenceValue` is
/// `_Field_size_(NumAdapters) UINT64*`, so every entry the runtime sized is
/// written, and the count comes from the argument.
///
/// # Safety
/// `values`, when non-null, must address `count` writable `UINT64`s the runtime
/// owns.
unsafe fn zero_paging_fences(values: *mut ddi12::UINT64, count: ddi12::UINT) {
    if values.is_null() {
        return;
    }
    for index in 0..count as usize {
        // SAFETY: the caller guarantees `count` writable `UINT64`s, and `index`
        // is strictly below that count.
        unsafe { core::ptr::write_unaligned(values.add(index), 0) };
    }
}

/// `pfnMakeResident`.
///
/// ⭐ **Hit twice inside one `D3D12CreateDevice` on the passing `D12-G7` run**,
/// which is why it needed a real body before anything else could work.
///
/// The honest answer is `S_OK` with no paging fence, and the reasoning is
/// `DDI_REFERENCE.md` §9.8's: **VidMm owns residency**, and it owns it for
/// *allocations*, of which this driver has none. vkd3d's memory is minted by the
/// Mesa venus ICD through its own `D3DKMT` path, so no object reachable from
/// this DDI carries a `D3DKMT_HANDLE` this driver could hand to
/// `pfnMakeResidentCb` — which is the same fact
/// [`check_resource_allocation_handle`] reports. There is nothing to page in,
/// so the operation is already complete, and "already complete" is `S_OK`.
///
/// ⛔ **`E_PENDING` is the one thing that must never be faked here.** §9.8: an
/// `E_PENDING` without a valid `pPagingFenceValue` / `WaitMask` *"hangs the
/// caller with no error anywhere"*. Returning `S_OK` makes the fence protocol
/// unreachable by construction rather than merely unused.
///
/// ⚠ The paging fences and `WaitMask` are still written. The header marks them
/// meaningful *"only if MakeResident returns E_PENDING"*, so a runtime that
/// obeys its own comment never reads them — but the cost of writing zeros is
/// nothing and the cost of being wrong about that is a device.
///
/// # Safety
/// `arg`, when non-null, must be a live `D3D12DDIARG_MAKERESIDENT_0001` whose
/// `pPagingFenceValue` addresses `NumAdapters` writable `UINT64`s.
unsafe extern "C" fn make_resident(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *mut ddi12::D3D12DDIARG_MAKERESIDENT_0001,
) -> Hresult {
    if arg.is_null() {
        note_refusal(&L4_REFUSALS.residency_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: non-null per the check; the DDI declares it a live in/out struct.
    let a = unsafe { &mut *arg };
    // SAFETY: the struct's own `_Field_size_` names `NumAdapters` as the count.
    unsafe { zero_paging_fences(a.pPagingFenceValue, a.NumAdapters) };
    a.WaitMask = 0;

    L4_REFUSALS.make_resident_calls.bump();
    let n = L4_REFUSALS.make_resident_calls.get();
    if n <= LOG_BUDGET {
        // ⚠ `D3DDDI_MAKERESIDENT_FLAGS` is a bitfield struct, not an integer;
        // its union carries a `Value: UINT` arm that covers every bit.
        // SAFETY: reading the `Value` arm of a union whose other arm is a
        // bitfield over the same `UINT` is defined for any initialisation the
        // runtime can have performed -- there is no invalid `u32`.
        let flags = unsafe { a.Flags.__bindgen_anon_1.Value };
        log_error!(
            "L4: MakeResident objects={} adapters={} flags={flags:#x} -> S_OK, no paging fence \
             (VidMm owns residency; this driver mints no allocations) (x{n})",
            a.NumObjects,
            a.NumAdapters,
        );
    }
    S_OK
}

/// `pfnEvict`.
///
/// The counterpart of [`make_resident`] and honest for the same reason: there is
/// no allocation to evict, so there is nothing to fail.
///
/// # Safety
/// `arg`, when non-null, must be a live `D3D12DDIARG_EVICT`.
unsafe extern "C" fn evict(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_EVICT,
) -> Hresult {
    if arg.is_null() {
        note_refusal(&L4_REFUSALS.residency_bad_arg);
        return E_INVALIDARG;
    }
    L4_REFUSALS.evict_calls.bump();
    S_OK
}

/// `pfnOfferResources`.
///
/// An offer is advisory: it tells the driver the application no longer needs the
/// contents and the driver *may* discard them. This driver keeps them, which is
/// a legal response to every offer priority, and [`reclaim_resources`] then
/// truthfully reports that nothing was discarded. The counter is what stops
/// "we accepted and did nothing" reading as "we implemented offer/reclaim".
///
/// # Safety
/// `arg`, when non-null, must be a live `D3D12DDIARG_OFFERRESOURCES`.
unsafe extern "C" fn offer_resources(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_OFFERRESOURCES,
) -> Hresult {
    if arg.is_null() {
        note_refusal(&L4_REFUSALS.residency_bad_arg);
        return E_INVALIDARG;
    }
    L4_REFUSALS.offer_resources_accepted.bump();
    S_OK
}

/// `pfnReclaimResources`.
///
/// ⛔ **`pDiscarded` is an `_Out_ BOOL` per object and it is not optional.**
/// [`offer_resources`] never discards anything, so every entry is `FALSE` — and
/// leaving the array untouched would have the runtime tell the application its
/// contents are gone, on the strength of whatever was in its own buffer. Same
/// class as `pfnQueryNodeMap`, same fix: write every entry the runtime asked
/// for, with the count from the argument.
///
/// # Safety
/// `arg`, when non-null, must be a live `D3D12DDIARG_RECLAIMRESOURCES_0001`
/// whose `pDiscarded` addresses `NumObjects` writable `BOOL`s and whose
/// `pPagingFenceValue` addresses `NumAdapters` writable `UINT64`s.
unsafe extern "C" fn reclaim_resources(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *mut ddi12::D3D12DDIARG_RECLAIMRESOURCES_0001,
) -> Hresult {
    if arg.is_null() {
        note_refusal(&L4_REFUSALS.residency_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: non-null per the check; the DDI declares it a live in/out struct.
    let a = unsafe { &mut *arg };
    if !a.pDiscarded.is_null() {
        for index in 0..a.NumObjects as usize {
            // SAFETY: the DDI sizes `pDiscarded` by `NumObjects` and `index` is
            // strictly below that count. `FALSE` is 0 for `BOOL`.
            unsafe { core::ptr::write_unaligned(a.pDiscarded.add(index), 0) };
        }
    }
    // SAFETY: sized by `NumAdapters`, exactly as in `make_resident`.
    unsafe { zero_paging_fences(a.pPagingFenceValue, a.NumAdapters) };
    a.WaitMask = 0;

    L4_REFUSALS.reclaim_resources_calls.bump();
    S_OK
}

// ---------------------------------------------------------------------------
// (g) pfnCalcPrivateOpenedHeapAndResourceSizes / pfnOpenHeapAndResource
// ---------------------------------------------------------------------------

/// `pfnCalcPrivateOpenedHeapAndResourceSizes` — refused, sized zero.
///
/// Paired with [`open_heap_and_resource`]'s refusal: no block is asked for,
/// because no object will be built.
///
/// # Safety
/// `_arg`, when non-null, must be live for the call. It is not dereferenced.
unsafe extern "C" fn calc_private_opened_heap_and_resource_sizes(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _arg: *const ddi12::D3D12DDIARG_OPENHEAP_0003,
    _h_protected_session: ddi12::D3D12DDI_HPROTECTEDRESOURCESESSION_0030,
) -> ddi12::D3D12DDI_HEAP_AND_RESOURCE_SIZES {
    note_refusal(&L4_REFUSALS.open_heap_calc_refused);
    ddi12::D3D12DDI_HEAP_AND_RESOURCE_SIZES {
        Heap: 0,
        Resource: 0,
    }
}

/// `pfnOpenHeapAndResource` — REFUSED, and this is the lane's one deliberate
/// scope cut.
///
/// ⚠ It is one of the runtime's nine hard NULL-checks (`DDI_REFERENCE.md` §9.7,
/// strings:103), so it is a **real function that refuses**, never a NULL slot.
///
/// # What is missing, precisely
///
/// This slot is what discharges `DECISIONS.md` D3c — *"D3D12-created resources
/// must be able to be opened by DWM, using D3D11 and the 11 DDI"* — and it is
/// the one place in this lane that needs `helios_protocol`'s
/// `HeliosWddmAllocPrivate` / `HeliosWddmOpenIdentity`. Three things stand
/// between here and an implementation, and none of them is a line of Rust:
///
/// 1. **There is no engine entry point to adopt a foreign allocation.**
///    `D3D12DDIARG_OPENHEAP_0003` carries `D3DDDI_OPENALLOCATIONINFO` plus a
///    `D3D12DDI_HKMRESOURCE` — kernel identity from `Dxgkrnl`. vkd3d's only
///    adoption path is `ID3D12Device::OpenSharedHandle`, which takes an **NT
///    handle** from `CreateSharedHandle`, not a `D3DKMT` allocation. Bridging
///    the two means a new C++ entry point into vkd3d's internals to build a
///    `d3d12_resource` over Vulkan external memory the Mesa ICD already owns.
/// 2. **The other half of the channel does not exist yet either.** For DWM to
///    open a D3D12-created resource through the 11 DDI, this driver must first
///    *write* `HeliosWddmAllocPrivate` at create time — which it cannot, because
///    it mints no WDDM allocation at all (see the module doc).
/// 3. **Nothing on the triangle path needs it.** `DDI_REFERENCE.md` §14.0's
///    measured `D12-G5` trace — device, queue, swapchain, two PSOs, two draws,
///    three presents — never reaches this slot.
///
/// ⇒ `PARALLEL.md` §9.1's *"implemented **or** explicitly refused with a named
/// counter"*, taken deliberately, with the counter and this comment as the
/// record. `GATES.md` §7.23 is where the settling experiment already lives.
///
/// `E_NOTIMPL` and not `DXGI_ERROR_UNSUPPORTED`: the latter is this project's
/// code for declining a DDI **negotiation** (`umd_common/src/hr.rs:51-56`), and
/// this is a create that a future build will implement. Neither is
/// `DXGI_ERROR_DRIVER_INTERNAL_ERROR`, which `DECISIONS.md` §7.5 reserves for a
/// genuine driver fault.
///
/// ⛔ **Neither slot is touched, and that is the same rule
/// [`create_heap_and_resource`] follows**: a Create may null only a private
/// block its own paired sizing call asked for.
/// [`calc_private_opened_heap_and_resource_sizes`] answers `{0, 0}`, so this
/// driver owns no block here — any non-null `pDrvPrivate` arriving belongs to
/// something else (an existing heap being opened into), and clearing it would
/// leak that object's `Box` and make it unresolvable for the rest of the
/// process. Refusing without writing is the only honest action for a slot that
/// asked for nothing.
///
/// # Safety
/// `_arg`, when non-null, must be live for the call. Nothing is dereferenced and
/// nothing is written.
unsafe extern "C" fn open_heap_and_resource(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _arg: *const ddi12::D3D12DDIARG_OPENHEAP_0003,
    _h_heap: ddi12::D3D12DDI_HHEAP,
    _h_rt_resource: ddi12::D3D12DDI_HRTRESOURCE,
    _h_protected_session: ddi12::D3D12DDI_HPROTECTEDRESOURCESESSION_0030,
    _h_resource: ddi12::D3D12DDI_HRESOURCE,
) -> Hresult {
    note_refusal(&L4_REFUSALS.open_heap_and_resource_refused);
    E_NOTIMPL
}

// ---------------------------------------------------------------------------
// (h) The five introspection slots
// ---------------------------------------------------------------------------

/// Ask the engine for a resource's allocation size and alignment, and shape the
/// answer into the DDI's struct.
///
/// ⭐ **The runtime acts on all four numbers**, so a zero-fill here is a lie and
/// not a default. `DDI_REFERENCE.md` §9.7 quotes four runtime strings that
/// reject a driver's answer for a resource whose layout the runtime already
/// knows — a wrong `Layout`, a wrong `ResourceDataSize`, and any non-zero
/// `AdditionalDataSize` or `AdditionalDataHeaderSize`.
///
/// * **`Layout` is echoed verbatim.** `ResourceHeaps.md:2390`: the runtime may
///   ask with `Layout` = `_UNDEFINED`, *"in which case the driver may choose any
///   texture layout, but must echo `STANDARD_SWIZZLE` or `ROW_MAJOR` back
///   verbatim when either was requested"*. Echoing satisfies the second half
///   trivially and answers the first half honestly: a Vulkan optimal-tiled image
///   *is* a driver-chosen unspecified layout, which is what `_UNDEFINED` names.
/// * **The additional-data fields are zero** because there is none: this driver
///   attaches no metadata of its own to a resource's memory.
/// * **The alignment is clamped.** `ResourceHeaps.md:1350`: it must be a power of
///   two and must never exceed 64 KiB unless `SampleDesc.Count > 1`. vkd3d
///   answers from Vulkan memory requirements and is under no such obligation.
fn allocation_info_from_engine(
    device10: &ID3D12Device10,
    desc1: &D3D12_RESOURCE_DESC1,
    a: &ddi12::D3D12DDIARG_CREATERESOURCE_0109,
    visible_node_mask: u32,
) -> ddi12::D3D12DDI_RESOURCE_ALLOCATION_INFO_0022 {
    let desc = resource_desc(desc1);
    // SAFETY: `desc` is a live local of exactly the struct the entry point
    // names, and the slice it is passed in is one element long.
    let info = unsafe { device10.GetResourceAllocationInfo(visible_node_mask, &[desc]) };

    let (size, alignment) = if info.SizeInBytes == ALLOCATION_INFO_FAILURE {
        L4_REFUSALS.resource_alloc_info_engine_failed.bump();
        let n = L4_REFUSALS.resource_alloc_info_engine_failed.get();
        if n <= LOG_BUDGET {
            log_error!(
                "L4: GetResourceAllocationInfo refused type={} fmt={} {}x{}x{} mips={} \
                 samples={} -- reporting a zero-sized allocation, which the runtime will \
                 reject (x{n})",
                a.ResourceType,
                a.Format,
                a.Width,
                a.Height,
                a.DepthOrArraySize,
                a.MipLevels,
                a.SampleDesc.Count,
            );
        }
        (0, 0)
    } else {
        (info.SizeInBytes, info.Alignment)
    };

    let clamped = if a.SampleDesc.Count <= 1 && alignment > MAX_NON_MSAA_ALIGNMENT {
        L4_REFUSALS.resource_alignment_clamped.bump();
        let n = L4_REFUSALS.resource_alignment_clamped.get();
        if n <= LOG_BUDGET {
            log_error!(
                "L4: engine alignment {alignment} exceeds the 64 KiB ceiling a non-multisampled \
                 resource may report -- clamped (x{n})"
            );
        }
        MAX_NON_MSAA_ALIGNMENT
    } else {
        alignment
    };

    if a.Layout == v::TL_UNDEFINED {
        L4_REFUSALS.resource_alloc_info_layout_chosen.bump();
    }

    ddi12::D3D12DDI_RESOURCE_ALLOCATION_INFO_0022 {
        ResourceDataSize: size,
        AdditionalDataHeaderSize: 0,
        AdditionalDataSize: 0,
        // The clamp above bounds this at 64 KiB for every non-MSAA resource and
        // at the 4 MB MSAA tier otherwise, so the narrowing cannot lose bits;
        // `unwrap_or(0)` is the compile-time-total form of that, not a guess.
        ResourceDataAlignment: u32::try_from(clamped).unwrap_or(0),
        AdditionalDataHeaderAlignment: 0,
        AdditionalDataAlignment: 0,
        Layout: a.Layout,
        MipLevelSwizzleTransition: [0; 5],
        PlaneSliceSwizzleTransition: [0; 2],
    }
}

/// `pfnCheckResourceAllocationInfo`.
///
/// ⭐ **`AlignmentRestriction` is the API's `D3D12_RESOURCE_DESC::Alignment`.**
/// The DDI moved the field out of the struct — `D3D12DDIARG_CREATERESOURCE_0109`
/// has no `Alignment` while `D3D12_RESOURCE_DESC` does — and put it here as a
/// parameter, which is also why `pfnCreateHeapAndResource` has no such argument.
///
/// # ⛔ It is COUNTED AND IGNORED, so that there is exactly ONE derivation
///
/// Forwarding it would make this slot answer a different question from the one
/// the create arms ask. vkd3d honours `desc->Alignment` —
/// `d3d12_device_GetResourceAllocationInfo3` does
/// `requested = desc->Alignment; if (!desc->Alignment) requested =
/// d3d12_resource_desc_default_alignment(desc); Alignment = max(vk, requested)`
/// (`libs/vkd3d/device.c:9036-9041`) — while [`create_committed`] and
/// [`create_placed_or_reserved`] must pass 0, because the create DDI carries no
/// alignment for them to pass. Honouring it here therefore produces the exact
/// failure `check_existing_resource_allocation_info`'s doc says caching prevents:
/// an app asks with `D3D12_SMALL_RESOURCE_PLACEMENT_ALIGNMENT` (4 KiB), is told
/// 4 KiB, places the resource at a 4 KiB-aligned heap offset, and the create then
/// substitutes vkd3d's 64 KiB default and refuses the placement.
///
/// Reporting the driver's natural alignment instead is not merely
/// self-consistent, it is the API's own documented behaviour for a small-resource
/// request: a driver that cannot place at the smaller alignment answers with the
/// default, and the application is required to use the number it was given. The
/// ignoring is a number — `ResourceAlignmentRestrictionIgnored` — rather than
/// silence, so "we did not honour the app's alignment" is visible.
///
/// ⚠ `D3D12DDI_RESOURCE_OPTIMIZATION_FLAGS` has no counterpart on
/// `GetResourceAllocationInfo` and is a hint, not a requirement: an ignored hint
/// yields a correct answer that may be larger than an optimal one. Non-zero
/// arrivals are counted so "we ignored it" is a number.
///
/// # Safety
/// `p_resource` must be a live `D3D12DDIARG_CREATERESOURCE_0109`, and `out` must
/// address one writable `D3D12DDI_RESOURCE_ALLOCATION_INFO_0022`.
unsafe extern "C" fn check_resource_allocation_info(
    h_device: ddi12::D3D12DDI_HDEVICE,
    p_resource: *const ddi12::D3D12DDIARG_CREATERESOURCE_0109,
    optimization_flags: ddi12::D3D12DDI_RESOURCE_OPTIMIZATION_FLAGS,
    alignment_restriction: ddi12::UINT32,
    visible_node_mask: ddi12::UINT,
    out: *mut ddi12::D3D12DDI_RESOURCE_ALLOCATION_INFO_0022,
) {
    if out.is_null() {
        note_refusal(&L4_REFUSALS.heap_resource_create_bad_arg);
        return;
    }
    // ⛔ A defined answer on every path, before anything can fail.
    // SAFETY: non-null per the check; the DDI declares it an out-parameter.
    unsafe {
        core::ptr::write_unaligned(out, ddi12::D3D12DDI_RESOURCE_ALLOCATION_INFO_0022::default())
    };

    if p_resource.is_null() {
        note_refusal(&L4_REFUSALS.heap_resource_create_bad_arg);
        return;
    }
    if optimization_flags != 0 {
        L4_REFUSALS.resource_optimization_flags_ignored.bump();
    }
    // ⭐ UP-4 splits `PRIMARY` out of that aggregate, the same move the file
    // already makes for `HeapPrimaryFlagDropped` against
    // `HeapFlagUnrepresentable` and for the same reason: the aggregate is graded
    // "non-zero OK" because ignoring an optimisation *hint* is benign, and the
    // PRIMARY bit is not a hint about layout, it is a declaration that the
    // swapchain primary is being sized. `KMD_IMPACT.md` §14a.3 names it as the
    // present path's second trigger, and this counter is the only instrument that
    // can say whether it arrives -- the measured logs show the aggregate at 1..3
    // in 101 of 150 runs, so *something* sets these bits and the aggregate cannot
    // say which.
    //
    // ⛔ Counted and NOT acted on. This slot has no `D3D12DDI_HRESOURCE`: it is a
    // sizing query about a resource that does not exist yet, so there is nothing
    // to record an identity against. See `note_presentable_identity` for why that
    // makes §14a.3's "use both signals" unimplementable as written.
    if optimization_flags & v::RESOURCE_OPT_PRIMARY != 0 {
        L4_REFUSALS.resource_optimization_primary.bump();
        let n = L4_REFUSALS.resource_optimization_primary.get();
        if n <= LOG_BUDGET {
            log_error!(
                "L4: D3D12DDI_RESOURCE_OPTIMIZATION_FLAG_PRIMARY on CheckResourceAllocationInfo -- \
                 the swapchain primary is being SIZED here, and this slot has no resource handle to \
                 attach an identity to; {}x{} fmt={} (x{n})",
                // SAFETY: `p_resource` was null-checked above and is `_In_ CONST`.
                unsafe { (*p_resource).Width },
                unsafe { (*p_resource).Height },
                unsafe { (*p_resource).Format },
            );
        }
    }
    if alignment_restriction != 0 {
        L4_REFUSALS.resource_alignment_restriction_ignored.bump();
        let n = L4_REFUSALS.resource_alignment_restriction_ignored.get();
        if n <= LOG_BUDGET {
            log_error!(
                "L4: AlignmentRestriction={alignment_restriction} ignored -- the create DDI \
                 carries no alignment, so honouring it here would answer a different question \
                 from the one the create asks (x{n})"
            );
        }
    }
    // SAFETY: non-null per the check; the DDI declares it `_In_ CONST`.
    let a = unsafe { &*p_resource };

    // SAFETY: device-scope DDI; the borrow ends with this call.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&L4_REFUSALS.resource_no_device);
        return;
    };
    let Some(device10) = engine_device10(dev) else {
        return;
    };
    // ⛔ `0`, exactly as both create arms pass -- see this function's doc. The
    // two Check slots and the create must call `resource_desc1` with the same
    // alignment argument or they are two derivations of one answer.
    // SAFETY: `a` is live per the check above.
    let Some(desc1) = (unsafe { resource_desc1(a) }) else {
        note_refusal(&L4_REFUSALS.heap_resource_create_bad_arg);
        return;
    };

    let info = allocation_info_from_engine(&device10, &desc1, a, visible_node_mask);
    // SAFETY: as the first write above.
    unsafe { core::ptr::write_unaligned(out, info) };
}

/// `pfnCheckExistingResourceAllocationInfo`.
///
/// Answers from the resource's own state, which is the allocation info this
/// driver reported when it created the resource. Recomputing would be a second
/// derivation of one answer and the only way the two could disagree.
///
/// ⚠ The typedef's name carries an SDK typo that SDK 26100 still ships —
/// `PFND3D12DDI_CHECKEXISITINGRESOURCEALLOCATIONINFO_0022`, with `EXISITING`
/// transposed (`SPECS.md` §9.7, from `ResourceHeaps.md:2968`). The corrected
/// spelling does not compile.
///
/// # Safety
/// `h_resource` must be a live resource handle from
/// [`create_heap_and_resource`], and `out` must address one writable
/// `D3D12DDI_RESOURCE_ALLOCATION_INFO_0022`.
unsafe extern "C" fn check_existing_resource_allocation_info(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_resource: ddi12::D3D12DDI_HRESOURCE,
    out: *mut ddi12::D3D12DDI_RESOURCE_ALLOCATION_INFO_0022,
) {
    if out.is_null() {
        note_refusal(&L4_REFUSALS.heap_resource_create_bad_arg);
        return;
    }
    // SAFETY: non-null per the check; the DDI declares it an out-parameter.
    unsafe {
        core::ptr::write_unaligned(out, ddi12::D3D12DDI_RESOURCE_ALLOCATION_INFO_0022::default())
    };

    // SAFETY: the runtime passes a resource handle this driver wrote; the borrow
    // ends with this call.
    let Some(state) = (unsafe { resource_state(h_resource) }) else {
        note_refusal(&L4_REFUSALS.resource_handle_unresolved);
        return;
    };
    // SAFETY: as the first write above.
    unsafe { core::ptr::write_unaligned(out, state.alloc_info) };
}

/// `pfnCheckSubresourceInfo`.
///
/// ⭐ **This is where the runtime learns a subresource's offset and strides**,
/// and it exists precisely because Map is heap-scoped: *"Because Map is on the
/// heap, per-subresource offset and strides come from a separate DDI,
/// `pfnCheckSubresourceInfo`, which must be thread-safe and deterministic for a
/// given resource and subresource"* (`ResourceHeaps.md:1437`). Determinism is
/// why the description comes from the cached [`ResourceState::desc`] rather than
/// from a fresh `GetDesc`, and thread-safety is free: the state is immutable.
///
/// # ⛔ Two SCALAR queries, and no array sized from runtime data
///
/// `GetCopyableFootprints(desc, Subresource, 1, 0, ...)` answers `Offset = 0`,
/// because the footprint it computes is packed from the *first requested*
/// subresource — so the naive one-subresource query cannot give an offset. The
/// first draft therefore asked for `[0, Subresource]` and read the last entry,
/// which is correct but sizes an array from a runtime `UINT`. Bounding that
/// array turned a **legal** resource into a wrong answer: a `Texture2DArray`
/// with `DepthOrArraySize = 2048, MipLevels = 15` has 30 720 subresources,
/// every one of them legal and every query above the bound answered
/// `Offset = 0, RowStride = 0`, which the runtime would then act on.
///
/// The array is not needed. `pTotalBytes` is a **scalar**, and vkd3d's loop
/// (`libs/vkd3d/device.c:9270-9314`) ends each iteration with
/// `total = offset + size; offset = align(total,
/// D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT)`, writing `layouts[i].Offset =
/// base_offset + offset` at the top. So the offset of subresource *n* is exactly
/// `align(pTotalBytes over [0, n), D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT)` —
/// one scalar query and one round-up, with no allocation, no bound and no cliff.
/// The pitch and row count come from a second query of exactly ONE subresource.
///
/// ⛔ Both calls still obey the rule that cost this file a stack overflow in
/// draft: **every out-parameter of `GetCopyableFootprints` is an ARRAY of
/// `NumSubresources` entries** — `pLayouts`, `pNumRows`, `pRowSizeInBytes` all
/// are — so a scalar may only be passed where `NumSubresources` is 1. The range
/// query asks for none of them; the single query asks with a count of 1.
///
/// ⚠ **An out-of-range subresource is detected, not answered.** vkd3d pre-fills
/// every out-parameter with `0xff` and sets `total = ~0ull` before validating,
/// then `goto end`s on a bad range (`device.c:9234-9268`), so an untouched or
/// refused answer is `UINT_MAX` / `UINT64_MAX`. Both locals are seeded with the
/// same sentinel, so "the engine declined" reads the same whether the engine
/// wrote the sentinel or wrote nothing at all. That is the only case
/// `SubresourceInfoOutOfRange` now counts, and it is a case with no right answer
/// rather than a legal query this driver refused to compute.
///
/// ⚠ `DepthStride` is `RowPitch * NumRows` — the byte distance between depth
/// slices of one subresource, which is what the packed footprint defines and
/// what the DDI's field name asks for.
///
/// The swizzle offsets are zero: this driver reports no standard-swizzle layout,
/// so there is no swizzle pattern for a transition to be described against.
///
/// # Safety
/// `h_resource` must be a live resource handle from
/// [`create_heap_and_resource`], and `out` must address one writable
/// `D3D12DDI_SUBRESOURCE_INFO`.
unsafe extern "C" fn check_subresource_info(
    h_device: ddi12::D3D12DDI_HDEVICE,
    h_resource: ddi12::D3D12DDI_HRESOURCE,
    subresource: ddi12::UINT,
    out: *mut ddi12::D3D12DDI_SUBRESOURCE_INFO,
) {
    if out.is_null() {
        note_refusal(&L4_REFUSALS.heap_resource_create_bad_arg);
        return;
    }
    // SAFETY: non-null per the check; the DDI declares it an out-parameter.
    unsafe { core::ptr::write_unaligned(out, ddi12::D3D12DDI_SUBRESOURCE_INFO::default()) };

    // SAFETY: the runtime passes a resource handle this driver wrote; the borrow
    // ends with this call.
    let Some(state) = (unsafe { resource_state(h_resource) }) else {
        note_refusal(&L4_REFUSALS.resource_handle_unresolved);
        return;
    };
    // SAFETY: device-scope DDI; the borrow ends with this call.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&L4_REFUSALS.resource_no_device);
        return;
    };
    let Some(device10) = engine_device10(dev) else {
        return;
    };

    let desc = resource_desc(&state.desc);

    // Query 1 -- this subresource's own pitch and row count, and the range
    // check. Exactly ONE subresource, so each out-parameter is an array of one
    // and a single live local is the correct storage.
    let mut layout = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
        Offset: 0,
        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
            Format: DXGI_FORMAT(0),
            Width: 0,
            Height: 0,
            Depth: 0,
            RowPitch: FOOTPRINT_UNANSWERED_U32,
        },
    };
    let mut num_rows: u32 = FOOTPRINT_UNANSWERED_U32;
    // SAFETY: `desc`, `layout` and `num_rows` are live locals; `NumSubresources`
    // is 1, so each array the call writes has exactly one element. The two
    // outputs this call does not want are declined with `None`.
    unsafe {
        device10.GetCopyableFootprints(
            &desc,
            subresource,
            1,
            0,
            Some(core::ptr::from_mut(&mut layout)),
            Some(core::ptr::from_mut(&mut num_rows)),
            None,
            None,
        );
    }
    if num_rows == FOOTPRINT_UNANSWERED_U32 || layout.Footprint.RowPitch == FOOTPRINT_UNANSWERED_U32
    {
        L4_REFUSALS.subresource_info_out_of_range.bump();
        let n = L4_REFUSALS.subresource_info_out_of_range.get();
        if n <= LOG_BUDGET {
            log_error!(
                "L4: CheckSubresourceInfo -- the engine did not describe subresource \
                 {subresource} of a {}x{} resource with {} mips and DepthOrArraySize {}, so the \
                 index is outside it; the zeroed answer stands because there is none (x{n})",
                state.desc.Width,
                state.desc.Height,
                state.desc.MipLevels,
                state.desc.DepthOrArraySize,
            );
        }
        return;
    }

    // Query 2 -- the byte extent of everything before it, which is where it
    // starts once rounded up to the texture-data placement alignment. Subresource
    // 0 starts at 0 and needs no query at all.
    let offset = if subresource == 0 {
        0
    } else {
        let mut total: u64 = FOOTPRINT_UNANSWERED_U64;
        // SAFETY: `desc` and `total` are live locals; `pTotalBytes` is a scalar
        // for any `NumSubresources`, and every array output is declined with
        // `None`, so nothing is written through a pointer this call does not
        // size.
        unsafe {
            device10.GetCopyableFootprints(
                &desc,
                0,
                subresource,
                0,
                None,
                None,
                None,
                Some(core::ptr::from_mut(&mut total)),
            );
        }
        // `checked_next_multiple_of` and not `next_multiple_of`: the latter
        // panics on overflow, and a panic in a DDI is a silent graphics
        // deadlock.
        let aligned = if total == FOOTPRINT_UNANSWERED_U64 {
            None
        } else {
            total.checked_next_multiple_of(u64::from(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT))
        };
        let Some(aligned) = aligned else {
            L4_REFUSALS.subresource_info_out_of_range.bump();
            let n = L4_REFUSALS.subresource_info_out_of_range.get();
            if n <= LOG_BUDGET {
                log_error!(
                    "L4: CheckSubresourceInfo -- the engine did not give a byte extent for \
                     subresources 0..{subresource}, so this one has no offset (x{n})"
                );
            }
            return;
        };
        aligned
    };

    let row_stride = u64::from(layout.Footprint.RowPitch);
    let info = ddi12::D3D12DDI_SUBRESOURCE_INFO {
        Offset: offset,
        RowStride: row_stride,
        DepthStride: row_stride.saturating_mul(u64::from(num_rows)),
        RowBytePreSwizzleOffset: 0,
        ColumnPreSwizzleOffset: 0,
        DepthPreSwizzleOffset: 0,
    };
    // SAFETY: as the first write above.
    unsafe { core::ptr::write_unaligned(out, info) };
}

/// `pfnCheckResourceVirtualAddress`.
///
/// ⭐ **This is where `ARCHITECTURE.md` §13's open question about GPU virtual
/// addresses becomes code.** `DDI_REFERENCE.md` §9.7 states the position: a
/// forwarding UMD returns vkd3d's `ID3D12Resource::GetGPUVirtualAddress`, which
/// is a Vulkan **buffer device address** in the *host* GPU's space obtained
/// through venus (`libs/vkd3d/resource.c:2656-2663`), and never calls
/// `pfnReserveGpuVirtualAddressCb` / `pfnMapGpuVirtualAddressCb`. Whether the
/// D3D12 runtime and its debug layer accept a VA space the driver never obtained
/// from the kernel is recorded there as **UNVERIFIED**; this is the site the
/// experiment settles.
///
/// ⚠ Zero is a correct answer for a texture: only buffers have a GPU VA in
/// D3D12, and vkd3d returns 0 for images. It is therefore *not* counted, and
/// only an unresolvable handle is.
///
/// # Safety
/// `h_resource` must be a live resource handle from
/// [`create_heap_and_resource`].
unsafe extern "C" fn check_resource_virtual_address(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_resource: ddi12::D3D12DDI_HRESOURCE,
) -> ddi12::D3D12DDI_GPU_VIRTUAL_ADDRESS {
    // SAFETY: the runtime passes a resource handle this driver wrote; the borrow
    // ends with this call.
    let Some(resource) = (unsafe { engine_resource(h_resource) }) else {
        note_refusal(&L4_REFUSALS.resource_handle_unresolved);
        return 0;
    };
    // SAFETY: `resource` is the engine resource this driver holds a reference to
    // for the object's whole life.
    unsafe { resource.GetGPUVirtualAddress() }
}

/// `pfnCheckResourceAllocationHandle` — answers 0, and the counter is the point.
///
/// ⛔ **This is the one place this lane cannot be honest and complete at the same
/// time, so it is honest.** The DDI asks for the `D3DKMT_HANDLE` of the kernel
/// allocation behind a resource. This driver has none: vkd3d's memory is minted
/// by the Mesa venus ICD through its own `D3DKMT` path, so `helios_umd12.dll`
/// never calls `pfnAllocateCb` and no object reachable from this DDI carries a
/// kernel allocation it owns. `DDI_REFERENCE.md` §9.7 names this exactly —
/// *"kernel identity is mandatory in at least three places, so 'pure passthrough
/// with no `pfnAllocateCb`' is not viable"* — and the honest answer while that
/// remains true is 0 with a counter, never a fabricated handle.
///
/// ⚠ Expected **non-zero if this slot is ever called**, and a reading above zero
/// is the measurement that says the passthrough model has been reached for real.
///
/// # Safety
/// `_h_resource` is not dereferenced. Declared `unsafe` because the DDI's PFN
/// typedef is.
unsafe extern "C" fn check_resource_allocation_handle(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _h_resource: ddi12::D3D10DDI_HRESOURCE,
) -> ddi12::D3DKMT_HANDLE {
    note_refusal(&L4_REFUSALS.resource_allocation_handle_unavailable);
    0
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// Install L4's 16 device-core slots.
///
/// Chain position: `QueueSlots` -> `ResourceSlots` on the device-core table.
///
/// Every assignment below is checked by the compiler against the bindgen
/// `Option<unsafe extern "C" fn(...)>` signature for that field, which is the
/// whole premise of the fan-out (`PARALLEL.md` §7).
pub(crate) fn install(
    mut filling: Filling<'_, DeviceCoreTable, stage::QueueSlots>,
) -> Filling<'_, DeviceCoreTable, stage::ResourceSlots> {
    let table = filling.table();

    // (g) heaps, resources, residency -- 11
    table.pfnMapHeap = Some(map_heap);
    table.pfnUnmapHeap = Some(unmap_heap);
    table.pfnCalcPrivateHeapAndResourceSizes = Some(calc_private_heap_and_resource_sizes);
    table.pfnCreateHeapAndResource = Some(create_heap_and_resource);
    table.pfnDestroyHeapAndResource = Some(destroy_heap_and_resource);
    table.pfnCalcPrivateOpenedHeapAndResourceSizes =
        Some(calc_private_opened_heap_and_resource_sizes);
    table.pfnOpenHeapAndResource = Some(open_heap_and_resource);
    table.pfnMakeResident = Some(make_resident);
    table.pfnEvict = Some(evict);
    table.pfnOfferResources = Some(offer_resources);
    table.pfnReclaimResources = Some(reclaim_resources);

    // (h) resource introspection -- 5
    table.pfnCheckResourceVirtualAddress = Some(check_resource_virtual_address);
    table.pfnCheckResourceAllocationInfo = Some(check_resource_allocation_info);
    table.pfnCheckSubresourceInfo = Some(check_subresource_info);
    table.pfnCheckExistingResourceAllocationInfo = Some(check_existing_resource_allocation_info);
    table.pfnCheckResourceAllocationHandle = Some(check_resource_allocation_handle);

    filling.advance()
}

// ---------------------------------------------------------------------------
// Refusal counters
// ---------------------------------------------------------------------------

/// L4's refusal counters.
///
/// ⛔ **Append only.** Counter order inside a set, and set order in
/// `lib.rs`'s `UMD12_REFUSAL_SETS`, are both the evidence contract:
/// `D3D12 DDI refusals:` lines get diffed across builds.
struct L4Refusals {
    /// `pfnCalcPrivateHeapAndResourceSizes` with neither a heap nor a resource
    /// description — the illegal fourth row of the arm table. Expected 0. A size
    /// call cannot refuse (it returns a struct), so this counter is that site's
    /// only channel.
    heap_resource_calc_bad_arg: RefusalCounter,
    /// A create or introspection slot was handed a null out-pointer, a null
    /// argument struct, an unknown `D3D12DDI_RESOURCE_TYPE`, or a private block
    /// the paired sizing call said would exist and did not. Expected 0.
    heap_resource_create_bad_arg: RefusalCounter,
    /// `ID3D12Device::CreateHeap` refused. Expected 0; a hit is an
    /// out-of-memory or an unsupported heap shape and the line above it says
    /// which.
    heap_create_engine_failed: RefusalCounter,
    /// A `D3D12DDI_MEMORY_POOL` or `D3D12DDI_CPU_PAGE_PROPERTY` this driver does
    /// not recognise. ⛔ Expected 0 — both enums are closed and two-or-three
    /// valued — and a hit means the header moved under the cached bindings.
    heap_property_unrepresentable: RefusalCounter,
    /// `D3D12DDI_HEAP_FLAGS` bits with no `D3D12_HEAP_FLAGS` counterpart, so
    /// dropped: `COHERENT_SYSTEMWIDE`, `_0041_DENY_L0_DEMOTION`.
    /// ⚠ May legitimately be non-zero; it reads as a work list, not a fault —
    /// which is only true because `PRIMARY` is counted separately by
    /// `HeapPrimaryFlagDropped` and is not in this bucket.
    heap_flag_unrepresentable: RefusalCounter,
    /// A heap was created with `CreationNodeMask` or `VisibleNodeMask` other
    /// than 1 and the value was forwarded unchanged. ⛔ Expected 0 on this
    /// single-node guest; a hit is the same multi-adapter assumption
    /// `ARCHITECTURE.md` §13 UNVERIFIED-11 records, reached for real.
    heap_node_mask_unexpected: RefusalCounter,
    /// A standalone heap got no whole-heap span buffer, so it can be neither
    /// mapped nor asked for a GPU virtual address. ⚠ **Expected non-zero**: a
    /// heap whose flags deny buffers cannot have one by construction, and those
    /// heaps are not CPU-mappable anyway.
    heap_span_buffer_unavailable: RefusalCounter,
    /// `CreateCommittedResource3` or `CreatePlacedResource2` refused. Expected
    /// 0 outside genuine out-of-memory; the line above each hit carries the full
    /// description so the refusal can be attributed to a field.
    resource_create_engine_failed: RefusalCounter,
    /// A reserved (tiled) resource was refused — the resource-args-only arm with
    /// **neither** a `ReuseBufferGPUVA` parent nor a usable `hHeap`, which is
    /// `ResourceHeaps.md:1204`'s definition of reserved. ⛔ Expected 0: `caps12`
    /// reports `TiledResourcesTier = NOT_SUPPORTED`, so the runtime should never
    /// ask. A hit is a caps inconsistency, not something this slot can fix.
    resource_reserved_refused: RefusalCounter,
    /// A placed resource named a `ReuseBufferGPUVA` parent this driver could not
    /// resolve to an engine heap — most likely a committed resource, whose heap
    /// is implicit and has no `ID3D12Heap`. Expected 0. ⚠ It is a *census of the
    /// fallback*, not a failure: the `hHeap` path runs next, so non-zero **with
    /// placed creates still succeeding** says the runtime does supply `hHeap` on
    /// the placed arm and the `ReuseBufferGPUVA` model is not how it places;
    /// non-zero **with creates failing** says neither channel works.
    resource_placement_unresolved: RefusalCounter,
    /// A non-zero `CreateAtVirtualAddress` arrived and was ignored. ⛔ Expected
    /// 0, and a hit is a real finding: `DDI_REFERENCE.md` §9.7 records that
    /// `RecreateAt` is gated behind DDI 0111, which SDK 10.0.26100.0 does not
    /// define, so arrival means that build assumption is wrong.
    resource_create_at_virtual_address: RefusalCounter,
    /// A non-null `pRowMajorLayout` arrived. The API has no way to dictate a
    /// row pitch to `CreateCommittedResource3`, so the engine's own layout is
    /// used and the requested one is dropped. Expected 0 while nothing asks for
    /// a dictated row-major layout.
    resource_row_major_layout_ignored: RefusalCounter,
    /// A `D3D12DDI_TEXTURE_LAYOUT` with no API counterpart —
    /// `DEVICE_DEPENDENT_SWIZZLE_0`. Expected 0.
    resource_layout_unrepresentable: RefusalCounter,
    /// `D3D12DDI_RESOURCE_FLAGS_0003` bits with no `D3D12_RESOURCE_FLAGS`
    /// counterpart, so dropped: `CONTENT_PROTECTION`, the two
    /// `ONLY_*_TEXTURE_PLACEMENT` hints, `4MB_ALIGNED`, `SAMPLER_FEEDBACK`.
    /// ⚠ May legitimately be non-zero.
    resource_flag_unrepresentable: RefusalCounter,
    /// An initial barrier layout was changed: a buffer's forced to `UNDEFINED`
    /// (the only layout a buffer may carry), or one of the DDI-only `LEGACY_*`
    /// values mapped onto the nearest API layout. ⚠ Expected non-zero as soon as
    /// any application creates a buffer through the legacy create path.
    resource_barrier_layout_coerced: RefusalCounter,
    /// A castable-format list longer than this driver's bound was dropped rather
    /// than read. Expected 0 — DXGI defines fewer than 200 formats.
    castable_formats_dropped: RefusalCounter,
    /// A protected-resource session handle arrived on a create or sizing call
    /// and was not forwarded. ⛔ Expected 0: this driver reports no
    /// protected-resource support.
    protected_session_ignored: RefusalCounter,
    /// The engine device does not expose `ID3D12Device10`, so no resource can be
    /// created: the DDI carries a barrier layout, a sampler-feedback mip region
    /// and a castable-format list that only that revision's entry points accept.
    /// ⛔ Expected 0 — vkd3d's `QueryInterface` accepts `IID_ID3D12Device10`
    /// (`libs/vkd3d/device.c:4647`) — and a hit means the engine was swapped.
    resource_device10_unavailable: RefusalCounter,
    /// A device-scope slot could not reach the engine: the `hDevice` did not
    /// resolve, or the bridge carries no `ID3D12Device`. Expected 0 — these are
    /// device-scope DDIs and a device exists by construction.
    resource_no_device: RefusalCounter,
    /// A DDI arrived with a heap or resource handle this driver never wrote, or
    /// whose object had already been destroyed. Expected 0.
    resource_handle_unresolved: RefusalCounter,
    /// `pfnMapHeap` on a heap with no mappable anchor. Paired with
    /// `HeapSpanBufferUnavailable`: that counter says a heap has none, this one
    /// says something tried to map it anyway.
    map_heap_no_anchor: RefusalCounter,
    /// `ID3D12Resource::Map` on a heap's anchor refused. ⚠ Expected non-zero for
    /// committed textures whose layout is not `ROW_MAJOR`: D3D12 does not allow
    /// mapping those, and the heap's own base address is not obtainable through
    /// them.
    map_heap_engine_failed: RefusalCounter,
    /// How many heaps were mapped. ⚠ Not a refusal — it is the evidence that the
    /// heap-scoped map path is being exercised at all, which the per-slot noop
    /// counter used to carry and which implementing the slot would otherwise
    /// delete.
    map_heap_calls: RefusalCounter,
    /// `ID3D12Device::GetResourceAllocationInfo` answered its `UINT64_MAX`
    /// failure sentinel, so a zero-sized allocation was reported. ⛔ Expected 0;
    /// the runtime rejects a zero `ResourceDataSize` for a resource whose layout
    /// it knows (strings:82), so a hit turns into a failed create with a
    /// diagnosable cause.
    resource_alloc_info_engine_failed: RefusalCounter,
    /// The engine's alignment exceeded the 64 KiB ceiling a non-multisampled
    /// resource may report (`ResourceHeaps.md:1350`) and was clamped. Expected 0
    /// while vkd3d answers from ordinary Vulkan memory requirements.
    resource_alignment_clamped: RefusalCounter,
    /// The runtime asked for allocation info with `Layout = _UNDEFINED`, leaving
    /// the choice to the driver, and this driver echoed `_UNDEFINED` back — the
    /// honest name for the Vulkan optimal-tiled layout the engine will pick.
    /// ⚠ **Expected non-zero**, and it is a census rather than a fault.
    resource_alloc_info_layout_chosen: RefusalCounter,
    /// A non-zero `D3D12DDI_RESOURCE_OPTIMIZATION_FLAGS` was ignored.
    /// `GetResourceAllocationInfo` has no counterpart parameter, and the flags
    /// are a hint: ignoring one yields a correct answer that may be larger than
    /// an optimal one. ⚠ May legitimately be non-zero — measured at 1..3 in 101
    /// of the 150 logged runs under `tmp/`.
    ///
    /// ⚠ **RE-GRADED at UP-4.** "Non-zero OK" is only defensible now that
    /// `PRIMARY` is counted separately by `ResourceOptimizationPrimary`: this
    /// aggregate absorbs `SHADER_RESOURCE`, `UNORDERED_ACCESS` and
    /// `DETERMINISTIC`, which are layout hints, and the PRIMARY bit is a
    /// declaration about what the resource IS. Both counters still move for a
    /// PRIMARY arrival — this one is deliberately the total, so its measured
    /// history stays comparable across builds.
    resource_optimization_flags_ignored: RefusalCounter,
    /// `pfnCheckSubresourceInfo` was asked about a subresource the engine would
    /// not describe — i.e. an index outside the resource, which
    /// `GetCopyableFootprints` reports by leaving its `0xff` pre-fill in place.
    /// Expected 0. ⚠ It is **not** a driver-imposed bound: every legal
    /// subresource of a legal resource is answered, including the 30 720 of a
    /// 2 048-slice 15-mip Texture2DArray. A hit is a question with no right
    /// answer, and the zeroed out-struct stands because there is none.
    subresource_info_out_of_range: RefusalCounter,
    /// `pfnCheckResourceAllocationHandle` answered 0 because this driver mints
    /// no kernel allocations. ⚠ **Expected non-zero if the slot is called at
    /// all**, and driving it to zero is what closing `DDI_REFERENCE.md` §9.7's
    /// kernel-identity gap would mean.
    resource_allocation_handle_unavailable: RefusalCounter,
    /// `pfnCalcPrivateOpenedHeapAndResourceSizes` answered {0, 0} because the
    /// paired open refuses. Expected 0 until something opens a shared resource.
    open_heap_calc_refused: RefusalCounter,
    /// `pfnOpenHeapAndResource` refused. ⛔ This is `DECISIONS.md` D3c's counter:
    /// non-zero means DWM, or an application, tried to open a resource across
    /// the D3D11/D3D12 DDI boundary and this driver could not. See the handler's
    /// doc comment for the three things that stand in the way.
    open_heap_and_resource_refused: RefusalCounter,
    /// `pfnMakeResident` calls served. ⚠ Not a refusal. `D12-G7` measured
    /// **two** inside one `D3D12CreateDevice`, so a reading near 2 per device is
    /// the expected shape and a reading of 0 means this slot is not being
    /// reached at all.
    make_resident_calls: RefusalCounter,
    /// `pfnEvict` calls served. ⚠ Not a refusal, same reason.
    evict_calls: RefusalCounter,
    /// Offers accepted with the contents retained. ⚠ Not a refusal: keeping the
    /// contents is a legal answer to every offer priority, and
    /// `pfnReclaimResources` then truthfully reports nothing was discarded.
    offer_resources_accepted: RefusalCounter,
    /// `pfnReclaimResources` calls served, each of which reported every object
    /// as not discarded. ⚠ Not a refusal.
    reclaim_resources_calls: RefusalCounter,
    /// A residency DDI arrived with a null argument struct. Expected 0 — all
    /// four declare their argument non-optional.
    residency_bad_arg: RefusalCounter,
    /// ⛔ **`D3D12DDI_HEAP_FLAG_PRIMARY` arrived and was dropped from the
    /// `D3D12_HEAP_FLAGS` word this driver hands the engine**, because the API
    /// has no counterpart bit. Split out of `HeapFlagUnrepresentable`
    /// deliberately: that counter is graded "non-zero OK" because the bits it
    /// absorbs (`COHERENT_SYSTEMWIDE`, `_0041_DENY_L0_DEMOTION`) are benign, and
    /// this one is not. It is the only channel the DDI has for declaring a
    /// primary (`SPECS.md` §9.7, `ResourceHeaps.md:897`).
    ///
    /// ⚠ **RE-GRADED at UP-4, because what it means changed.** It used to mean
    /// *"a primary reached this driver and was demoted — nothing downstream can
    /// tell it apart"*. The flag is still dropped on the way to the engine, but
    /// the **fact** is no longer lost: `note_presentable_identity` now records the
    /// primary in the `forward12::identity12` table, keyed on the engine resource,
    /// with the raw heap-flags word intact. So:
    ///
    /// * still expected **0** until something drives a real D3D12 swapchain;
    /// * non-zero now means a primary arrived **and an identity entry should
    ///   exist for it** — read it against `IdentityRecorded`, which is the same
    ///   event counted at the other site. Equal is correct. `HeapPrimaryFlagDropped`
    ///   ahead of `IdentityRecorded` means primaries arrived on an arm that cannot
    ///   record one (check `HeapPrimaryWithoutResource` and `IdentityTableFull`).
    ///
    /// ⛔ And the arrival itself is still **UNMEASURED**: this counter reads 0 in
    /// all 150 logged `umd12` runs under `tmp/`, none of which was a swapchain
    /// workload. `KMD_IMPACT.md` §14a.3 asserts the flag *"arrives"*; the only
    /// instrument for that claim has never been non-zero, so it is untested rather
    /// than confirmed. `ResourceOptimizationPrimary` is the counter that says
    /// whether the other signal arrives instead.
    heap_primary_flag_dropped: RefusalCounter,
    /// A non-zero `AlignmentRestriction` arrived on
    /// `pfnCheckResourceAllocationInfo` and was NOT forwarded to the engine, so
    /// the alignment reported is the driver's natural one. ⚠ **Expected non-zero
    /// as soon as anything asks for `D3D12_SMALL_RESOURCE_PLACEMENT_ALIGNMENT`**;
    /// it is a census, not a fault. See the handler for why honouring it would
    /// make the two derivations of one answer disagree.
    resource_alignment_restriction_ignored: RefusalCounter,
    /// `pfnDestroyHeapAndResource` was told by the resource it just destroyed
    /// that the paired create HAD asked for a heap block, and then found no
    /// `Box<HeapState>` behind `hHeap` to reclaim.
    ///
    /// ⛔ **Expected 0.** A hit means an `ID3D12Heap` and its map anchor are now
    /// unreachable — a leak of engine memory, not a crash, which is why the
    /// destroy is allowed to reach it at all.
    ///
    /// ⚠ It is a separate counter from `ResourceHandleUnresolved` and not an
    /// extra bump of it, because on this path `seen` is already `true`: the
    /// resource resolved. The aggregate could never have shown a heap that went
    /// unreclaimed, which is exactly the shape the review pass found in the arm
    /// this counter was added with.
    heap_block_unreclaimed: RefusalCounter,
    // -- UP-4, the resource -> kernel-allocation-identity table -------------
    /// Entries made in the `forward12::identity12` table, i.e. `PRIMARY`
    /// committed resources this driver saw. ⚠ **Not a refusal — the census.**
    /// Expected **0** until something drives a real D3D12 swapchain through this
    /// driver, and it must move in lock-step with `HeapPrimaryFlagDropped`: they
    /// are two readings of the same event (the flag arriving) taken at different
    /// sites, so a disagreement means the admission predicate and the flag test
    /// have drifted apart.
    identity_recorded: RefusalCounter,
    /// A primary arrived and the identity table was **full**, so its identity was
    /// dropped and the resource cannot be presented. ⛔ Expected 0. The bound is
    /// four fully-buffered swapchains (`identity12::MAX_PRESENTABLE_IDENTITIES`),
    /// so a hit says the admission predicate is admitting things that are not
    /// primaries — not that the bound is too small.
    identity_table_full: RefusalCounter,
    /// A recorded identity **overwrote** one already held for the same engine
    /// resource address. ⛔ Expected 0, and non-zero is a lifetime defect, not a
    /// benign duplicate: it means a `pfnDestroyHeapAndResource` did not retire
    /// its entry and a recycled COM address then collided. The overwrite is the
    /// safe direction (the live resource wins); this counter is how the missed
    /// removal stops being silent.
    identity_replaced: RefusalCounter,
    /// Identities retired by `pfnDestroyHeapAndResource`. ⚠ Not a refusal.
    /// `IdentityRecorded - IdentityRemoved` is the live-entry count, so a
    /// difference that grows across a run is a leak with no extra instrument
    /// needed. It counts only removals that found an entry, so ordinary
    /// non-presentable destroys do not move it.
    identity_removed: RefusalCounter,
    /// An identity was recorded with `vk_memory == 0`. ⛔ **Expected to equal
    /// `IdentityRecorded` today, and that is the honest reading of UP-4**: the
    /// engine-side accessor exists (`ID3D12DXVKInteropDevice4::
    /// GetVulkanResourceMemoryInfo`, UP-2) but no `bridge12` entry point calls
    /// it, so this driver has no path to a `VkDeviceMemory`. It goes to 0 in the
    /// commit that adds that bridge accessor, and until it does UP-5 cannot run.
    identity_vk_memory_unresolved: RefusalCounter,
    /// An identity was recorded with `venus_res_id == 0`. ⛔ Same shape as
    /// `IdentityVkMemoryUnresolved` and a **separate** link: the venus half needs
    /// the ICD's `helios_venus_memory_res_id` / `helios_venus_memory_alloc_info`
    /// (`icd/mesa/src/virtio/vulkan/vn_renderer_helios.c:619-632`), and this
    /// crate's bridge resolves the process ICD anchor for `venus_context_id()`
    /// alone. Two counters because the two links close in different commits, and
    /// one counter could not say which was still missing. ⛔ A zero
    /// `adopt_resource_id` is what makes the KMD *create* rather than *adopt*
    /// (`protocol/src/wddm.rs:131-138`), so UP-5 must refuse on this rather than
    /// pass the zero through.
    identity_venus_unresolved: RefusalCounter,
    /// `D3D12DDI_HEAP_FLAG_PRIMARY` arrived on the **heap-only** arm, with no
    /// resource description. ⛔ Expected 0: `ResourceHeaps.md:897` says the flag
    /// obliges the driver to create a resource simultaneously with the heap, and
    /// `note_presentable_identity`'s admission predicate depends on that
    /// obligation holding. A hit means a primary reached this driver with no
    /// `ID3D12Resource` to key an identity on, so it went unrecorded.
    heap_primary_without_resource: RefusalCounter,
    /// `D3D12DDI_RESOURCE_OPTIMIZATION_FLAG_PRIMARY` arrived on
    /// `pfnCheckResourceAllocationInfo`. ⚠ Not a refusal — it is
    /// `KMD_IMPACT.md` §14a.3's *"second signal"*, measured at the only slot that
    /// carries it, and split out of `ResourceOptimizationFlagsIgnored` for the
    /// same reason `HeapPrimaryFlagDropped` is split out of
    /// `HeapFlagUnrepresentable`: the aggregate is graded "non-zero OK" because
    /// ignoring a layout hint is benign, and this bit is not a layout hint.
    ///
    /// ⭐ **It is the tie-breaker for UP-5's predicate.** `HeapPrimaryFlagDropped`
    /// reads 0 in all 150 logged runs while the optimisation-flags aggregate
    /// reads 1..3 in 101 of them, so it is entirely possible that this is the
    /// only primary signal this runtime sends. Non-zero here with
    /// `HeapPrimaryFlagDropped` still 0 after a real swapchain run means the
    /// admission predicate must move to description-matching against this query.
    resource_optimization_primary: RefusalCounter,
}

static L4_REFUSALS: L4Refusals = L4Refusals {
    heap_resource_calc_bad_arg: RefusalCounter::new("HeapResourceCalcBadArg"),
    heap_resource_create_bad_arg: RefusalCounter::new("HeapResourceCreateBadArg"),
    heap_create_engine_failed: RefusalCounter::new("HeapCreateEngineFailed"),
    heap_property_unrepresentable: RefusalCounter::new("HeapPropertyUnrepresentable"),
    heap_flag_unrepresentable: RefusalCounter::new("HeapFlagUnrepresentable"),
    heap_node_mask_unexpected: RefusalCounter::new("HeapNodeMaskUnexpected"),
    heap_span_buffer_unavailable: RefusalCounter::new("HeapSpanBufferUnavailable"),
    resource_create_engine_failed: RefusalCounter::new("ResourceCreateEngineFailed"),
    resource_reserved_refused: RefusalCounter::new("ResourceReservedRefused"),
    resource_placement_unresolved: RefusalCounter::new("ResourcePlacementUnresolved"),
    resource_create_at_virtual_address: RefusalCounter::new("ResourceCreateAtVirtualAddress"),
    resource_row_major_layout_ignored: RefusalCounter::new("ResourceRowMajorLayoutIgnored"),
    resource_layout_unrepresentable: RefusalCounter::new("ResourceLayoutUnrepresentable"),
    resource_flag_unrepresentable: RefusalCounter::new("ResourceFlagUnrepresentable"),
    resource_barrier_layout_coerced: RefusalCounter::new("ResourceBarrierLayoutCoerced"),
    castable_formats_dropped: RefusalCounter::new("CastableFormatsDropped"),
    protected_session_ignored: RefusalCounter::new("ProtectedSessionIgnored"),
    resource_device10_unavailable: RefusalCounter::new("ResourceDevice10Unavailable"),
    resource_no_device: RefusalCounter::new("ResourceNoDevice"),
    resource_handle_unresolved: RefusalCounter::new("ResourceHandleUnresolved"),
    map_heap_no_anchor: RefusalCounter::new("MapHeapNoAnchor"),
    map_heap_engine_failed: RefusalCounter::new("MapHeapEngineFailed"),
    map_heap_calls: RefusalCounter::new("MapHeapCalls"),
    resource_alloc_info_engine_failed: RefusalCounter::new("ResourceAllocInfoEngineFailed"),
    resource_alignment_clamped: RefusalCounter::new("ResourceAlignmentClamped"),
    resource_alloc_info_layout_chosen: RefusalCounter::new("ResourceAllocInfoLayoutChosen"),
    resource_optimization_flags_ignored: RefusalCounter::new("ResourceOptimizationFlagsIgnored"),
    subresource_info_out_of_range: RefusalCounter::new("SubresourceInfoOutOfRange"),
    resource_allocation_handle_unavailable: RefusalCounter::new(
        "ResourceAllocationHandleUnavailable",
    ),
    open_heap_calc_refused: RefusalCounter::new("OpenHeapCalcRefused"),
    open_heap_and_resource_refused: RefusalCounter::new("OpenHeapAndResourceRefused"),
    make_resident_calls: RefusalCounter::new("MakeResidentCalls"),
    evict_calls: RefusalCounter::new("EvictCalls"),
    offer_resources_accepted: RefusalCounter::new("OfferResourcesAccepted"),
    reclaim_resources_calls: RefusalCounter::new("ReclaimResourcesCalls"),
    residency_bad_arg: RefusalCounter::new("ResidencyBadArg"),
    heap_primary_flag_dropped: RefusalCounter::new("HeapPrimaryFlagDropped"),
    resource_alignment_restriction_ignored: RefusalCounter::new(
        "ResourceAlignmentRestrictionIgnored",
    ),
    heap_block_unreclaimed: RefusalCounter::new("HeapBlockUnreclaimed"),
    identity_recorded: RefusalCounter::new("IdentityRecorded"),
    identity_table_full: RefusalCounter::new("IdentityTableFull"),
    identity_replaced: RefusalCounter::new("IdentityReplaced"),
    identity_removed: RefusalCounter::new("IdentityRemoved"),
    identity_vk_memory_unresolved: RefusalCounter::new("IdentityVkMemoryUnresolved"),
    identity_venus_unresolved: RefusalCounter::new("IdentityVenusUnresolved"),
    heap_primary_without_resource: RefusalCounter::new("HeapPrimaryWithoutResource"),
    resource_optimization_primary: RefusalCounter::new("ResourceOptimizationPrimary"),
};

/// L4's refusal set, printed by `crate::log_refusal_summary` at this lane's
/// position in `lib.rs`'s `UMD12_REFUSAL_SETS`.
///
/// ⭐ **Declared here rather than in `lib.rs` so this lane's diff against the
/// crate root is empty.** Every one of the eleven S6 lanes needs counters
/// (`PARALLEL.md` §9.1: *every skipped or refused path gets a named counter*),
/// and one flat array in `lib.rs` would have been the split's hottest merge
/// point — §5's shared-file table does not even list `lib.rs`. Same move
/// `forward12::tables12` makes for the 206 slots: name all eleven up front and
/// the lanes become substitutive instead of additive.
///
/// ⛔ **Append only**, in declaration order.
pub(crate) static REFUSALS: &[&RefusalCounter] = &[
    &L4_REFUSALS.heap_resource_calc_bad_arg,
    &L4_REFUSALS.heap_resource_create_bad_arg,
    &L4_REFUSALS.heap_create_engine_failed,
    &L4_REFUSALS.heap_property_unrepresentable,
    &L4_REFUSALS.heap_flag_unrepresentable,
    &L4_REFUSALS.heap_node_mask_unexpected,
    &L4_REFUSALS.heap_span_buffer_unavailable,
    &L4_REFUSALS.resource_create_engine_failed,
    &L4_REFUSALS.resource_reserved_refused,
    &L4_REFUSALS.resource_placement_unresolved,
    &L4_REFUSALS.resource_create_at_virtual_address,
    &L4_REFUSALS.resource_row_major_layout_ignored,
    &L4_REFUSALS.resource_layout_unrepresentable,
    &L4_REFUSALS.resource_flag_unrepresentable,
    &L4_REFUSALS.resource_barrier_layout_coerced,
    &L4_REFUSALS.castable_formats_dropped,
    &L4_REFUSALS.protected_session_ignored,
    &L4_REFUSALS.resource_device10_unavailable,
    &L4_REFUSALS.resource_no_device,
    &L4_REFUSALS.resource_handle_unresolved,
    &L4_REFUSALS.map_heap_no_anchor,
    &L4_REFUSALS.map_heap_engine_failed,
    &L4_REFUSALS.map_heap_calls,
    &L4_REFUSALS.resource_alloc_info_engine_failed,
    &L4_REFUSALS.resource_alignment_clamped,
    &L4_REFUSALS.resource_alloc_info_layout_chosen,
    &L4_REFUSALS.resource_optimization_flags_ignored,
    &L4_REFUSALS.subresource_info_out_of_range,
    &L4_REFUSALS.resource_allocation_handle_unavailable,
    &L4_REFUSALS.open_heap_calc_refused,
    &L4_REFUSALS.open_heap_and_resource_refused,
    &L4_REFUSALS.make_resident_calls,
    &L4_REFUSALS.evict_calls,
    &L4_REFUSALS.offer_resources_accepted,
    &L4_REFUSALS.reclaim_resources_calls,
    &L4_REFUSALS.residency_bad_arg,
    &L4_REFUSALS.heap_primary_flag_dropped,
    &L4_REFUSALS.resource_alignment_restriction_ignored,
    &L4_REFUSALS.heap_block_unreclaimed,
    &L4_REFUSALS.identity_recorded,
    &L4_REFUSALS.identity_table_full,
    &L4_REFUSALS.identity_replaced,
    &L4_REFUSALS.identity_removed,
    &L4_REFUSALS.identity_vk_memory_unresolved,
    &L4_REFUSALS.identity_venus_unresolved,
    &L4_REFUSALS.heap_primary_without_resource,
    &L4_REFUSALS.resource_optimization_primary,
];
