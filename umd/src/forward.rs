//! d3d10umddi device-funcs → D3D11 COM forwarders (pure Rust via the windows
//! crate). Each func reads its bindgen DDI arg struct, translates to a
//! windows-crate COM call on the DXVK `ID3D11Device`/`ID3D11DeviceContext`, and
//! stores the returned COM interface in the runtime-allocated DDI handle.
//!
//! DDI Usage/BindFlags/MiscFlags mirror the D3D11 API bit values (passthrough).
//! Resource/view handles store the raw COM pointer (8 bytes) in pDrvPrivate;
//! CalcPrivate*Size returns 8. Errors on VOID-returning Create* are dropped for
//! now (TODO: report via the device error callback) — a failed create leaves a
//! null handle.

mod alloc;
mod handles;
use alloc::{ScanoutGeometry, VenusBacking};
use crate::bridge::{DstRes, SrcRes};
use handles::{boxed_slot, Boxed, Com, ComHandle, DdiHandle, Slot};

use core::ffi::c_void;
use core::mem::ManuallyDrop;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::core::{IUnknown, Interface, PCSTR};
use windows::Win32::Foundation::{BOOL, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D::{
    D3D11_SRV_DIMENSION_BUFFER, D3D11_SRV_DIMENSION_BUFFEREX, D3D11_SRV_DIMENSION_TEXTURE1D,
    D3D11_SRV_DIMENSION_TEXTURE1DARRAY, D3D11_SRV_DIMENSION_TEXTURE2D,
    D3D11_SRV_DIMENSION_TEXTURE2DARRAY, D3D11_SRV_DIMENSION_TEXTURE2DMS,
    D3D11_SRV_DIMENSION_TEXTURE2DMSARRAY, D3D11_SRV_DIMENSION_TEXTURE3D,
    D3D11_SRV_DIMENSION_TEXTURECUBE, D3D11_SRV_DIMENSION_TEXTURECUBEARRAY,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

use helios_protocol::{
    HeliosPresentPrivateData, HeliosPresentRefreshCmd, HeliosPresentRenderCmd, HeliosWddmAllocMeta,
    HeliosWddmAllocPrivate, HeliosWddmOpenIdentity, HELIOS_PRESENT_PRIVATE_FLAG_DIRECT_SCANOUT,
    HELIOS_PRESENT_PRIVATE_MAGIC, HELIOS_PRESENT_PRIVATE_VERSION, HELIOS_PRESENT_REFRESH_MAGIC,
    HELIOS_PRESENT_REFRESH_VERSION, HELIOS_PRESENT_RENDER_MAGIC, HELIOS_PRESENT_RENDER_VERSION,
    HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY, HELIOS_WDDM_ALLOC_KIND_STANDARD,
    HELIOS_WDDM_ALLOC_MISC_DIRECT_SCANOUT, HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE,
    HELIOS_WDDM_ALLOC_MISC_PRIMARY, VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE,
    VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE, VIRTIO_GPU_BLOB_MEM_HOST3D, VIRTIO_GPU_MAP_CACHE_CACHED,
};

use crate::ddi;
use crate::device_funcs::HeliosDevice;
use crate::log_error;
use crate::present_gate_us;
use crate::present_sync_publish_enabled;
use crate::trace_line;

type Hdevice = ddi::D3D10DDI_HDEVICE;

/// One rate-limited log site's occurrence counter.
///
/// Replaces the hand-rolled `AtomicUsize` + threshold expression that was
/// re-derived at every reference, in eleven different shapes (`n < 16`, `< 32`,
/// `< 64`, `< 128`, `< 256`, `< 512`, `< 1024`, `< 2048`, `n % 512 == 0`,
/// `n % 1024 == 0`, `(n + 1) % 512 == 0`, `(n + 1) % 2048 == 0`).
///
/// DEVIATION from the review, and the reason: it asks for the budget to live in
/// the static, "instantiated per site with the site's current numbers so no
/// site's cadence changes". That is not implementable — eleven of these statics
/// are SHARED by sites with different budgets (`SHADER_BIND_LOG_COUNT` is used
/// with both `< 128` and `< 256`, `MPO_LOG_COUNT` with `< 16`, `< 64` and
/// `< 128`, `VIEW_LOG_COUNT` with `< 128` and `< 256`, `DRAW_LOG_COUNT` with
/// `< 2048` and a `% 1024` shape). Giving each site its own counter would change
/// the cadence of every one of them, which is precisely what must not happen.
/// So the counter is shared exactly as today and the budget is a call argument.
struct LogThrottle {
    count: AtomicUsize,
}

impl LogThrottle {
    const fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
        }
    }

    /// Bump and return the occurrence ordinal with no rate decision, for sites
    /// whose gate carries an extra escape clause (`|| alloc != 0`) or a shape
    /// of its own.
    fn next(&self) -> usize {
        self.count.fetch_add(1, Ordering::Relaxed)
    }

    /// Read the ordinal WITHOUT bumping it — one site logs a "pre" line under
    /// the same budget as the "post" line that follows it.
    fn peek(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// The first `first` occurrences.
    fn first_n(&self, first: usize) -> Option<usize> {
        let n = self.next();
        (n < first).then_some(n)
    }

    /// The first `first`, then every `every`-th counting from zero.
    fn first_n_then_every(&self, first: usize, every: usize) -> Option<usize> {
        let n = self.next();
        (n < first || n % every == 0).then_some(n)
    }

    /// The first `first`, then every `every`-th counting from one. Distinct
    /// from [`Self::first_n_then_every`]: it fires at n = every-1, 2*every-1,
    /// not at n = 0, every, 2*every.
    fn first_n_then_every_from_one(&self, first: usize, every: usize) -> Option<usize> {
        let n = self.next();
        (n < first || (n + 1) % every == 0).then_some(n)
    }
}

static RESOURCE_LOG_COUNT: LogThrottle = LogThrottle::new();
static CREATE_RESOURCE_IDENTITY_LOG_COUNT: LogThrottle = LogThrottle::new();
static VIEW_LOG_COUNT: LogThrottle = LogThrottle::new();
static WDDM_ALLOC_LOG_COUNT: LogThrottle = LogThrottle::new();
static D3D11_1_LOG_COUNT: LogThrottle = LogThrottle::new();
static COPY_LOG_COUNT: LogThrottle = LogThrottle::new();
static COPY_REGION_LOG_COUNT: LogThrottle = LogThrottle::new();
static MAP_LOG_COUNT: LogThrottle = LogThrottle::new();
static SHADER_BIND_LOG_COUNT: LogThrottle = LogThrottle::new();
static SHADER_SET_LOG_COUNT: LogThrottle = LogThrottle::new();
static SRV_CREATE_LOG_COUNT: LogThrottle = LogThrottle::new();
static SRV_BIND_LOG_COUNT: LogThrottle = LogThrottle::new();
static DRAW_LOG_COUNT: LogThrottle = LogThrottle::new();
static OM_LOG_COUNT: LogThrottle = LogThrottle::new();
static UPDATE_LOG_COUNT: LogThrottle = LogThrottle::new();
/// UpdateSubresource lines the rate cap dropped. Without this the cap would
/// turn "no lines" into "nothing happened".
static UPDATE_SUPPRESSED: AtomicUsize = AtomicUsize::new(0);
static DISPATCH_LOG_COUNT: LogThrottle = LogThrottle::new();
static HANDLE_MISS_LOG_COUNT: LogThrottle = LogThrottle::new();
static UAV_BIND_LOG_COUNT: LogThrottle = LogThrottle::new();
static CLEAR_RTV_LOG_COUNT: LogThrottle = LogThrottle::new();
static VIEWPORT_LOG_COUNT: LogThrottle = LogThrottle::new();
static SCISSOR_LOG_COUNT: LogThrottle = LogThrottle::new();
static RASTER_LOG_COUNT: LogThrottle = LogThrottle::new();
static IA_BIND_LOG_COUNT: LogThrottle = LogThrottle::new();
static PRESENT_READBACK_LOG_COUNT: LogThrottle = LogThrottle::new();
static PRESENT_FORCE_OPAQUE_LOG_COUNT: LogThrottle = LogThrottle::new();
static PRESENT_CB_LOG_COUNT: LogThrottle = LogThrottle::new();

struct ResourceState {
    com_raw: usize,
    /// A WDDM allocation is stored only after pfnMakeResidentCb has added one
    /// device-residency reference. Dropping the guard removes exactly that
    /// reference before the allocation is deallocated.
    allocation: Option<ResidentAllocation>,
    km_resource: ddi::D3DKMT_HANDLE,
    rt_resource: ddi::HANDLE,
    /// True when this UMD allocated `allocation` itself (pfnAllocateCb in
    /// `create_resource`); false for handles the runtime handed us at
    /// `open_resource`. `release_resource` may only pass allocation handles it
    /// created to pfnDeallocateCb's HandleList form — deallocating opened
    /// handles that way is what returned 0x80070057 and leaked the runtime's
    /// side of the open.
    ownership: AllocationOwnership,
    present_private: HeliosPresentPrivateData,
}

/// Who owns the WDDM allocation behind a resource.
///
/// This used to be an unnamed positional `bool` sitting between `km_resource`
/// and `rt_resource` in a seven-argument call, and the deallocate form was
/// reconstructed from `(rt_resource.is_null(), owns_allocation)` at destroy
/// time. R804.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AllocationOwnership {
    /// This UMD called `pfnAllocateCb` for it, in `create_resource`.
    CreatedByUmd,
    /// The runtime handed us the handle at `open_resource`. Passing these to
    /// `pfnDeallocateCb`'s HandleList form is what returned 0x80070057 and
    /// leaked the runtime's side of the open.
    OpenedByRuntime,
}

impl AllocationOwnership {
    /// The value the `owned=` log key has always carried. Kept so the
    /// `DDI deallocate_resource:` line stays byte-identical across R804.
    fn owns(self) -> bool {
        matches!(self, Self::CreatedByUmd)
    }
}

/// The one legal shape of a `D3DDDICB_DEALLOCATE` call.
///
/// The wire contract is three-way: EITHER `hResource` alone, OR
/// `NumAllocations`+`HandleList` with `hResource` NULL. Both together is
/// E_INVALIDARG -- the old 0x80070057, which also leaked opened resources.
/// Constructing this enum is the only way a `D3DDDICB_DEALLOCATE` is built, so
/// the both-set combination is unrepresentable rather than merely avoided.
///
/// `Nothing` must still exist: it is the (currently unreachable) case of no
/// runtime resource and an allocation we did not create. The guarantee is that
/// it is named and logged, not that it is gone.
enum DeallocateForm {
    ByResource(ddi::HANDLE),
    ByHandleList(core::num::NonZeroU32),
    Nothing { reason: &'static str },
}

impl DeallocateForm {
    /// The single exhaustive decision. Order matters and is the pre-R804
    /// behaviour exactly: a runtime resource handle wins over ownership,
    /// because the runtime then releases every allocation it tracks for the
    /// resource -- created AND opened instances.
    fn select(
        rt_resource: ddi::HANDLE,
        ownership: AllocationOwnership,
        allocation: ddi::D3DKMT_HANDLE,
    ) -> Self {
        if !rt_resource.is_null() {
            return Self::ByResource(rt_resource);
        }
        match (ownership, core::num::NonZeroU32::new(allocation)) {
            (AllocationOwnership::CreatedByUmd, Some(a)) => Self::ByHandleList(a),
            (AllocationOwnership::CreatedByUmd, None) => Self::Nothing {
                reason: "owner but no allocation handle",
            },
            (AllocationOwnership::OpenedByRuntime, _) => Self::Nothing {
                reason: "not owner",
            },
        }
    }
}

type EvictCallback = unsafe extern "C" fn(ddi::HANDLE, *mut ddi::D3DDDICB_EVICT) -> ddi::HRESULT;

/// One persistent WDDM 2.x device-residency reference.
///
/// This type is deliberately non-Clone and non-Copy: every successful
/// pfnMakeResidentCb call creates exactly one guard, and moving/dropping that
/// guard is the only way the reference can change ownership or be evicted.
struct ResidentAllocation {
    handle: core::num::NonZeroU32,
    h_rt_device: ddi::HANDLE,
    evict_cb: EvictCallback,
}

impl ResidentAllocation {
    fn handle(&self) -> ddi::D3DKMT_HANDLE {
        self.handle.get()
    }
}

impl Drop for ResidentAllocation {
    fn drop(&mut self) {
        let handle = self.handle.get();
        let mut evict = ddi::D3DDDICB_EVICT::default();
        evict.NumAllocations = 1;
        evict.AllocationList = &handle;
        let hr = unsafe { (self.evict_cb)(self.h_rt_device, &mut evict) };
        trace_line!(
            "WDDM residency: Evict alloc=0x{:x} hr=0x{:08x}",
            handle,
            hr as u32
        );
        if hr != 0 {
            log_error!(
                "WDDM residency: Evict FAILED alloc=0x{:x} hr=0x{:08x}",
                handle, hr as u32
            );
        }
    }
}

struct RtvState {
    com_raw: usize,
    /// Non-owning resource pointer; the RTV itself keeps the resource alive.
    resource_raw: usize,
    allocation: ddi::D3DKMT_HANDLE,
    width: u32,
    height: u32,
    format: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct RuntimeAllocPrivate {
    alloc: HeliosWddmAllocPrivate,
    meta: HeliosWddmAllocMeta,
}

#[inline]
fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some()
}

/// The three present-path debug knobs, read ONCE per process.
///
/// Each was an uncached `GetEnvironmentVariableW` plus an `OsString`
/// allocation on every present: two from the debug hooks and one for
/// `bOptimizeForComposition`. OBSERVABLE CHANGE, stated deliberately: these
/// become read-once-per-process, so setting them on a live process no longer
/// takes effect — they must be set before the process starts, like every
/// registry knob in `lib.rs`, all of which already use `OnceLock`.
///
/// Cost honesty (the review asks for it): three env reads per present against
/// a 10 ms frame gate is not where the present path spends its time. This is
/// removing avoidable per-frame work, not a measured win.
fn present_readback_enabled() -> bool {
    static VALUE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VALUE.get_or_init(|| env_flag("HELIOS_PRESENT_READBACK"))
}

fn present_force_opaque_enabled() -> bool {
    static VALUE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VALUE.get_or_init(|| env_flag("HELIOS_PRESENT_FORCE_OPAQUE"))
}

fn present_optimize_composition_enabled() -> bool {
    static VALUE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VALUE.get_or_init(|| env_flag("HELIOS_PRESENT_OPTIMIZE_COMPOSITION"))
}

fn empty_present_private() -> HeliosPresentPrivateData {
    HeliosPresentPrivateData {
        plane_offset: 0,
        magic: 0,
        version: 0,
        resource_id: 0,
        width: 0,
        height: 0,
        pitch: 0,
        dxgi_format: 0,
        reserved: 0,
    }
}

/// Legacy 24-byte trailer (geometry + bind/misc, no venus identity) written by
/// pre-identity driver builds. Parse-only.
#[repr(C)]
#[derive(Default, Copy, Clone)]
struct StandardAllocMetaV2 {
    width: u32,
    height: u32,
    format: u32,
    pitch: u32,
    bind_flags: u32,
    misc_flags: u32,
}

/// Oldest legacy 16-byte trailer. Parse-only.
#[repr(C)]
#[derive(Default, Copy, Clone)]
struct StandardAllocMetaV1 {
    width: u32,
    height: u32,
    format: u32,
    pitch: u32,
}

fn dxgi_to_d3dddi_format(format: u32) -> u32 {
    // KMD DescribeAllocation consumes legacy D3DDDIFORMAT values, not DXGI_FORMAT.
    const DXGI_FORMAT_R8G8B8A8_UNORM: u32 = 28;
    const DXGI_FORMAT_B8G8R8A8_UNORM: u32 = 87;
    const D3DDDIFMT_A8R8G8B8: u32 = 21;
    const D3DDDIFMT_A8B8G8R8: u32 = 32;

    match format {
        DXGI_FORMAT_R8G8B8A8_UNORM => D3DDDIFMT_A8B8G8R8,
        DXGI_FORMAT_B8G8R8A8_UNORM => D3DDDIFMT_A8R8G8B8,
        _ => 0,
    }
}

fn d3dddi_to_dxgi_format(format: u32) -> DXGI_FORMAT {
    const D3DDDIFMT_A8R8G8B8: u32 = 21;
    const D3DDDIFMT_A8B8G8R8: u32 = 32;

    match format {
        D3DDDIFMT_A8R8G8B8 => windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
        D3DDDIFMT_A8B8G8R8 => windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
        _ => windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
    }
}

/// Bytes per pixel of an (uncompressed) `DXGI_FORMAT`, for computing the WDDM
/// surface pitch. Only the single-byte channel block (R8_*/A8/R1, 60..=66) and
/// the wider HDR/deep formats matter for DWM composition; everything else
/// defaults to 4 (32-bpp BGRA/RGBA — the historical assumption). Over-reporting
/// is safe (the pitch only pads `linear_size`); under-reporting an A8 mask as
/// 4bpp is what made openers size these surfaces 4x too large.
fn dxgi_bytes_per_pixel(format: u32) -> u32 {
    match format {
        1..=4 => 16,  // R32G32B32A32_*
        5..=8 => 12,  // R32G32B32_*
        9..=14 => 8,  // R16G16B16A16_*
        15..=19 => 8, // R32G32_*, R32G8X24 depth
        60..=66 => 1, // R8_TYPELESS/UNORM/UINT/SNORM/SINT, A8_UNORM, R1_UNORM
        _ => 4,
    }
}

/// Bits per sample for the uncompressed DXGI formats that can participate in
/// D3D11 output/MSAA validation. Compressed/video-only formats are intentionally
/// absent because the runtime must not require MSAA for them.
fn dxgi_bits_per_sample(format: u32) -> Option<u32> {
    match format {
        1..=4 => Some(128),        // R32G32B32A32_*
        5..=8 => Some(96),         // R32G32B32_*
        9..=18 => Some(64),        // R16G16B16A16_*, R32G32_*
        19..=22 => Some(64),       // R32G8X24 / D32_FLOAT_S8X24 family
        23..=32 => Some(32),       // R10G10B10A2, R11G11B10, R8G8B8A8
        33..=47 => Some(32),       // R16G16, R32, R24G8
        48..=59 => Some(16),       // R8G8, R16 / D16
        60..=65 => Some(8),        // R8, A8
        85 | 86 | 115 => Some(16), // B5G6R5, B5G5R5A1, B4G4R4A4
        87..=93 => Some(32),       // BGRA/RGBX and XR_BIAS
        _ => None,
    }
}

fn dxgi_output_family_bits(format: u32) -> Option<u32> {
    match format {
        1..=4 => Some(128),            // R32G32B32A32 family
        5..=8 => Some(96),             // R32G32B32 family
        9..=18 => Some(64),            // R16G16B16A16 / R32G32 families
        19..=22 => Some(64),           // R32G8X24 / D32_FLOAT_S8X24 family
        23..=32 => Some(32),           // R10G10B10A2, R8G8B8A8 families
        33..=47 => Some(32),           // R16G16, R32, R24G8 families
        48..=59 => Some(16),           // R8G8, R16 / D16 families
        60..=64 => Some(8),            // R8 family
        87 | 88 | 90..=93 => Some(32), // BGRA/RGBX families
        _ => None,
    }
}

fn dxgi_output_bits_per_sample(format: u32, caps: u32) -> Option<u32> {
    const D3D11_FORMAT_SUPPORT_RENDER_TARGET: u32 = 0x0000_4000;
    const D3D11_FORMAT_SUPPORT_DEPTH_STENCIL: u32 = 0x0001_0000;

    if caps & (D3D11_FORMAT_SUPPORT_RENDER_TARGET | D3D11_FORMAT_SUPPORT_DEPTH_STENCIL) != 0 {
        dxgi_bits_per_sample(format)
    } else {
        dxgi_output_family_bits(format)
    }
}

fn dxgi_msaa_bits_per_sample(format: u32, caps: u32) -> Option<u32> {
    match format {
        // Depth-resource read/view formats are format-support siblings of the
        // MSAA-capable typeless/depth formats, but WARP reports zero quality
        // levels above 1x and the runtime rejects advertising them as MSAA RTs.
        21 | 22 | 46 | 47 => None,
        _ => dxgi_output_bits_per_sample(format, caps),
    }
}

fn dxgi_resolve_required(format: u32) -> bool {
    match format {
        // FLOAT families.
        2 | 6 | 10 | 16 | 26 | 34 | 41 | 54 => true,
        // UNORM / UNORM_SRGB families.
        11 | 24 | 28 | 29 | 35 | 45 | 46 | 49 | 55 | 56 | 61 | 85 | 86 | 87 | 88 | 89 | 91 | 93
        | 115 => true,
        // SNORM families.
        13 | 31 | 37 | 51 | 58 | 63 => true,
        // Typeless parents whose output views include at least one resolvable
        // UNORM/SNORM/FLOAT interpretation.
        1 | 5 | 9 | 15 | 19 | 23 | 27 | 33 | 39 | 44 | 48 | 53 | 60 | 90 | 92 => true,
        _ => false,
    }
}

fn dxgi_color_typeless_parent(format: u32) -> bool {
    matches!(
        format,
        1 | 5 | 9 | 15 | 23 | 27 | 33 | 48 | 53 | 60 | 90 | 92
    )
}

fn dxgi_integer_typed_format(format: u32) -> bool {
    matches!(
        format,
        3 | 4
            | 7
            | 8
            | 12
            | 14
            | 17
            | 18
            | 25
            | 30
            | 32
            | 36
            | 38
            | 42
            | 43
            | 50
            | 52
            | 57
            | 59
            | 62
            | 64
    )
}

/// Parse the meta trailer at `base_off` bytes into the buffer, tolerating the
/// two legacy (shorter) layouts. Returns a zero-extended [`HeliosWddmAllocMeta`].
unsafe fn read_alloc_meta(
    ptr: *const c_void,
    size: u32,
    base_off: usize,
) -> Option<HeliosWddmAllocMeta> {
    let avail = (size as usize).checked_sub(base_off)?;
    let meta_ptr = (ptr as *const u8).add(base_off);
    if avail >= core::mem::size_of::<HeliosWddmAllocMeta>() {
        return Some(core::ptr::read_unaligned(
            meta_ptr as *const HeliosWddmAllocMeta,
        ));
    }
    if avail >= core::mem::size_of::<StandardAllocMetaV2>() {
        let legacy = core::ptr::read_unaligned(meta_ptr as *const StandardAllocMetaV2);
        return Some(HeliosWddmAllocMeta {
            width: legacy.width,
            height: legacy.height,
            format: legacy.format,
            pitch: legacy.pitch,
            bind_flags: legacy.bind_flags,
            misc_flags: legacy.misc_flags,
            venus_alloc_size: 0,
            memory_type_index: 0,
            dxgi_format: 0,
            plane_offset: 0,
        });
    }
    if avail >= core::mem::size_of::<StandardAllocMetaV1>() {
        let legacy = core::ptr::read_unaligned(meta_ptr as *const StandardAllocMetaV1);
        return Some(HeliosWddmAllocMeta {
            width: legacy.width,
            height: legacy.height,
            format: legacy.format,
            pitch: legacy.pitch,
            // KMD standard allocations predate these trailer fields. Use the
            // composition-surface baseline so opened resources are renderable and
            // shader-readable instead of constructing a zero-usage texture.
            bind_flags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            misc_flags: 0,
            venus_alloc_size: 0,
            memory_type_index: 0,
            dxgi_format: 0,
            plane_offset: 0,
        });
    }
    None
}

/// A fully identified open: the KMD's identity record AND the creator's meta
/// trailer. The trailer is non-optional by construction — the import geometry
/// is read out of it, so there is no `Option` left to default at the
/// `open_ddi_texture2d` call and the 1x1 alias cannot be reconstructed.
#[derive(Clone, Copy)]
struct OpenedAllocation {
    ident: HeliosWddmOpenIdentity,
    meta: HeliosWddmAllocMeta,
}

/// Parse the OPEN-time private data: the KMD's versioned [`HeliosWddmOpenIdentity`]
/// record (written in `DxgkDdiOpenAllocation` after validating the venus resource
/// is LIVE) plus the creator's meta trailer. This replaces the adopt-id
/// heuristics: an open-time buffer either carries a valid identity or the open is
/// not backed by an identified venus resource.
///
/// This is the DIAGNOSTIC parse: it tolerates a missing trailer so the C1
/// identity evidence lines can still print what the buffer actually held. An
/// open must go through [`read_opened_allocation`] instead — an identity with
/// no trailer carries no geometry and is not openable.
unsafe fn read_open_identity(
    ptr: *const c_void,
    size: u32,
) -> Option<(HeliosWddmOpenIdentity, Option<HeliosWddmAllocMeta>)> {
    if ptr.is_null() || (size as usize) < core::mem::size_of::<HeliosWddmOpenIdentity>() {
        return None;
    }
    let ident = core::ptr::read_unaligned(ptr as *const HeliosWddmOpenIdentity);
    if !ident.is_valid() || ident.resource_id == 0 {
        return None;
    }
    let meta = read_alloc_meta(ptr, size, core::mem::size_of::<HeliosWddmOpenIdentity>());
    Some((ident, meta))
}

/// The parse an OPEN may use: identity plus a present meta trailer, or nothing.
unsafe fn read_opened_allocation(ptr: *const c_void, size: u32) -> Option<OpenedAllocation> {
    let (ident, meta) = read_open_identity(ptr, size)?;
    Some(OpenedAllocation { ident, meta: meta? })
}

// --- handle <-> COM helpers -------------------------------------------------

unsafe fn d3d11_device(h: Hdevice) -> Option<ManuallyDrop<ID3D11Device>> {
    let hd = h.pDrvPrivate as *const HeliosDevice;
    if hd.is_null() {
        return None;
    }
    // Borrowed, not adopted: the bridge keeps the owning reference. The
    // ManuallyDrop lives inside the wrapper now, so this cannot be written as
    // an adopting `from_raw` by a future edit. R813.
    (*hd).dxvk.d3d11_device()
}

/// Borrow the `HeliosDevice` behind a device handle (for the deferred
/// input-assembler state). Does not take ownership.
unsafe fn helios_device<'a>(h: Hdevice) -> Option<&'a HeliosDevice> {
    let hd = h.pDrvPrivate as *const HeliosDevice;
    if hd.is_null() {
        None
    } else {
        Some(&*hd)
    }
}

use crate::hr::{DXGI_ERROR_UNSUPPORTED, E_FAIL, E_INVALIDARG, E_OUTOFMEMORY};

/// Report an error to the D3D11 runtime for a VOID-returning DDI via the
/// corelayer `pfnSetErrorCb`. The runtime fails the API call that invoked the
/// DDI (e.g. `OpenSharedResource`), which is the contractual way for
/// `open_resource`/`create_*` to fail loudly instead of leaving a null handle
/// the runtime will dereference.
unsafe fn set_runtime_error(h: Hdevice, hr: i32) {
    let Some(dev) = helios_device(h) else {
        return;
    };
    if dev.um_callbacks.is_null() {
        log_error!("set_runtime_error: no corelayer callbacks");
        return;
    }
    // pfnSetErrorCb is the first member of every D3D11DDI_CORELAYER_DEVICECALLBACKS
    // revision, so reading it through the 11.0 layout is version-independent.
    let cb = &*(dev.um_callbacks as *const ddi::D3D11DDI_CORELAYER_DEVICECALLBACKS);
    if let Some(f) = cb.pfnSetErrorCb {
        f(
            ddi::D3D10DDI_HRTCORELAYER {
                handle: dev.h_rt_core_layer,
            },
            hr,
        );
    }
}

unsafe fn d3d11_context(h: Hdevice) -> Option<ManuallyDrop<ID3D11DeviceContext>> {
    let hd = h.pDrvPrivate as *const HeliosDevice;
    if hd.is_null() {
        return None;
    }
    (*hd).dxvk.d3d11_context()
}

unsafe fn d3d11_context1(h: Hdevice) -> Option<ID3D11DeviceContext1> {
    let context = d3d11_context(h)?;
    (*context).cast::<ID3D11DeviceContext1>().ok()
}

unsafe fn d3d11_context2(h: Hdevice) -> Option<ID3D11DeviceContext2> {
    let context = d3d11_context(h)?;
    (*context).cast::<ID3D11DeviceContext2>().ok()
}

unsafe fn d3d11_device2(h: Hdevice) -> Option<ID3D11Device2> {
    let device = d3d11_device(h)?;
    (*device).cast::<ID3D11Device2>().ok()
}

/// Store a COM interface's raw pointer (ownership transferred) in a DDI handle.
///
/// The null check is new: this was the one writer of the three that lacked it,
/// so a null slot wrote through a null pointer where `store_raw_com` and
/// `clear_handle` returned quietly. R803.
unsafe fn store_com<T: Interface>(h: impl ComHandle, obj: T) {
    match Slot::<Com<T>>::from_priv(h.drv_private()) {
        Some(slot) => slot.store(obj),
        // Dropping `obj` here releases the reference we were asked to hand to
        // the runtime. That is correct: with no slot to put it in, the
        // alternative is leaking it.
        None => drop(obj),
    }
}

/// Map a DXVK create failure onto an HRESULT the invoking API is documented to
/// return. Every `Create*` DDI this is used from documents exactly
/// `E_OUTOFMEMORY` and `E_INVALIDARG`; an HRESULT outside that set is itself
/// logged by the runtime as a driver bug, so a DXVK code outside it is
/// substituted rather than passed through.
fn create_error_hr(e: &windows::core::Error) -> i32 {
    match e.code().0 {
        hr @ (E_OUTOFMEMORY | E_INVALIDARG) => hr,
        _ => E_OUTOFMEMORY,
    }
}

/// The only way a VOID-returning `Create*` DDI may leave its handle slot:
/// either `store` runs, or the runtime is told the invoking API call failed.
///
/// The DDI has no return value, so `pfnSetErrorCb` is the sole channel through
/// which a failed create becomes a failed `CreateRasterizerState` /
/// `CreateRenderTargetView` / `CreateTexture2D` instead of an S_OK with a null
/// driver handle that the app then binds to nothing.
///
/// `result` is the DXVK call's `Result<()>` and `obj` its out-param: S_OK with
/// no object is as much a fake success as an error HRESULT, so both report.
/// Panic-free by construction (no `unwrap`, no indexing) — these are
/// `extern "C"` entry points under `panic = "abort"`.
unsafe fn finish_create<T: Interface>(
    h: Hdevice,
    result: windows::core::Result<()>,
    obj: Option<T>,
    store: impl FnOnce(T),
) {
    match result {
        Ok(()) => match obj {
            Some(o) => store(o),
            None => set_runtime_error(h, E_OUTOFMEMORY),
        },
        Err(e) => set_runtime_error(h, create_error_hr(&e)),
    }
}

unsafe fn store_raw_com(h: impl ComHandle, raw: usize) {
    if let Some(slot) = Slot::<Com<IUnknown>>::from_priv(h.drv_private()) {
        slot.store_raw(raw);
    }
}

/// Null a handle's slot. Payload-agnostic on purpose -- this writes the word
/// and never touches what it pointed at -- so it takes any `DdiHandle`,
/// including the boxed-payload ones. Every `Create*`/`Open*` DDI calls it on
/// entry so a failure leaves a null handle rather than stale garbage.
unsafe fn clear_handle(h: impl DdiHandle) {
    if let Some(slot) = Slot::<Com<IUnknown>>::from_priv(h.drv_private()) {
        slot.clear();
    }
}

/// The raw word behind a bare-COM DDI handle (0 when absent).
///
/// Correct for the shader/DSV/state slots it is used on, and silently the
/// `Box` pointer if ever applied to a resource or RTV slot -- which is exactly
/// the confusion `Slot<P>` exists to remove. Retained with its old signature so
/// this commit churns no call sites; the per-family conversions replace each
/// use with `Slot::<Com<T>>::word()` on a typed handle.
unsafe fn handle_com_raw(h: impl ComHandle) -> usize {
    handle_com_raw_at(h.drv_private())
}

/// `handle_com_raw` for the runtime-tag dispatches, which receive a bare
/// `pDrvPrivate` whose payload is selected by a `D3D11DDI_HANDLETYPE` value at
/// run time and so cannot be keyed on a static handle type. Callers must be an
/// arm of such a dispatch that has already matched a bare-COM tag.
unsafe fn handle_com_raw_at(handle_priv: *mut c_void) -> usize {
    match Slot::<Com<IUnknown>>::from_priv(handle_priv) {
        Some(slot) => slot.word(),
        None => 0,
    }
}

unsafe fn store_resource(
    h_res: ddi::D3D10DDI_HRESOURCE,
    obj: ID3D11Resource,
    allocation: Option<ResidentAllocation>,
    km_resource: ddi::D3DKMT_HANDLE,
    rt_resource: ddi::HANDLE,
    ownership: AllocationOwnership,
    present_private: HeliosPresentPrivateData,
) {
    let Some(slot) = boxed_slot(h_res) else {
        drop(obj);
        return;
    };
    slot.store(ResourceState {
        com_raw: obj.into_raw() as usize,
        allocation,
        km_resource,
        rt_resource,
        ownership,
        present_private,
    });
}

unsafe fn stamp_dxvk_resource_kmt_handles(
    h: Hdevice,
    obj: &ID3D11Resource,
    local: ddi::D3DKMT_HANDLE,
    global: ddi::D3DKMT_HANDLE,
) {
    trace_line!(
        "DDI resource KMT stamp enter: raw_local=0x{:x} raw_global=0x{:x}",
        local,
        global
    );
    let local = if local != 0 { local } else { global };
    if local == 0 {
        trace_line!("DDI resource KMT stamp skipped: no usable handle");
        return;
    }
    let Some(dev) = helios_device(h) else {
        log_error!("DDI resource KMT stamp skipped: missing device");
        return;
    };
    if dev
        .dxvk
        // SAFETY: `obj` is a live ID3D11Resource borrowed for this call, so the
        // pointer the bridge reinterpret_casts is valid for its duration.
        // R814 moved `unsafe` onto this declaration because it launders a
        // pointer; the marking now matches where the precondition is.
        .set_resource_kmt_handles(obj.as_raw() as usize, local, global)
    {
        log_error!(
            "DDI resource KMT handles stamped: local=0x{:x} global=0x{:x}",
            local, global
        );
    } else {
        log_error!(
            "DDI resource KMT handle stamp failed: local=0x{:x} global=0x{:x}",
            local, global
        );
    }
}

unsafe fn load_resource(h_res: ddi::D3D10DDI_HRESOURCE) -> Option<ManuallyDrop<ID3D11Resource>> {
    load_resource_at(h_res.pDrvPrivate)
}

/// `load_resource` for the runtime-tag dispatches (`Discard`, the tiled-resource
/// barrier), which receive a bare `pDrvPrivate` whose payload is selected by a
/// `D3D11DDI_HANDLETYPE` value at run time and so cannot be keyed on a static
/// handle type. Callers must be an arm of such a dispatch that has already
/// matched `HT_RESOURCE`. Same contract as `handle_com_raw_at`/`load_com_at`.
unsafe fn load_resource_at(handle_priv: *mut c_void) -> Option<ManuallyDrop<ID3D11Resource>> {
    let state = resource_state_at(handle_priv)?;
    if state.com_raw == 0 {
        return None;
    }
    Some(ManuallyDrop::new(ID3D11Resource::from_raw(
        state.com_raw as *mut c_void,
    )))
}

/// The `ResourceState` behind a DDI resource handle, or `None` for an empty
/// slot. The single place the resource slot is decoded; every reader below
/// goes through it instead of repeating the two-step null dance.
///
/// Taking the handle rather than its `pDrvPrivate` is what makes the payload
/// follow from the handle's type: `resource_state(h_rtv)` does not resolve.
unsafe fn resource_state(h_res: ddi::D3D10DDI_HRESOURCE) -> Option<&'static ResourceState> {
    boxed_slot(h_res)?.get()
}

/// `resource_state` for the runtime-tag dispatches. See `load_resource_at`.
unsafe fn resource_state_at(handle_priv: *mut c_void) -> Option<&'static ResourceState> {
    Slot::<Boxed<ResourceState>>::from_priv(handle_priv)?.get()
}

/// Raw ID3D11Resource COM pointer behind a DDI resource handle (0 when
/// absent) — for bridge calls that inspect the DXVK image without taking a COM
/// reference.
unsafe fn resource_com_raw(h_res: ddi::D3D10DDI_HRESOURCE) -> usize {
    resource_state(h_res).map_or(0, |s| s.com_raw)
}

unsafe fn resource_allocation(h_res: ddi::D3D10DDI_HRESOURCE) -> ddi::D3DKMT_HANDLE {
    resource_state(h_res).map_or(0, |s| {
        s.allocation
            .as_ref()
            .map(ResidentAllocation::handle)
            .unwrap_or(0)
    })
}

unsafe fn resource_parent_handles(
    h_res: ddi::D3D10DDI_HRESOURCE,
) -> (ddi::HANDLE, ddi::D3DKMT_HANDLE) {
    resource_state(h_res)
        .map_or((core::ptr::null_mut(), 0), |s| (s.rt_resource, s.km_resource))
}

unsafe fn resource_present_private(
    h_res: ddi::D3D10DDI_HRESOURCE,
) -> Option<HeliosPresentPrivateData> {
    let p = resource_state(h_res)?.present_private;
    p.is_valid().then_some(p)
}

/// Resolve scanout metadata by the allocation Windows is actually presenting.
/// DXGI may keep one stable resource object while rotating its `hAllocation`
/// among the pPrimaryDesc ring, so resource-local metadata alone is not enough.
unsafe fn presented_primary_private(
    h: Hdevice,
    h_res: ddi::D3D10DDI_HRESOURCE,
) -> Option<HeliosPresentPrivateData> {
    if let Some(private) = unsafe { resource_present_private(h_res) } {
        return Some(private);
    }
    let allocation = unsafe { resource_allocation(h_res) };
    if allocation == 0 {
        return None;
    }
    let dev = unsafe { helios_device(h) }?;
    dev.direct_scanout_allocations
        .borrow()
        .iter()
        .find_map(|(candidate, private)| (*candidate == allocation).then_some(*private))
}

unsafe fn remember_direct_scanout_allocation(
    h: Hdevice,
    allocation: ddi::D3DKMT_HANDLE,
    private: HeliosPresentPrivateData,
) {
    if allocation == 0 || !private.is_valid() {
        return;
    }
    let Some(dev) = (unsafe { helios_device(h) }) else {
        return;
    };
    {
        let mut entries = dev.direct_scanout_allocations.borrow_mut();
        entries.retain(|(candidate, _)| *candidate != allocation);
        entries.push((allocation, private));
    }
    // A new direct-scanout primary is exactly the event a mode change produces,
    // so re-arm a previously-Unavailable LINEAR probe.
    dev.scanout_epoch.set(dev.scanout_epoch.get().wrapping_add(1));
}

/// Drop a destroyed allocation's direct-scanout entry. Returns
/// `(removed, remaining)` when something was removed, so the caller can log the
/// list length — the property that must stay bounded.
unsafe fn forget_direct_scanout_allocation(
    h: Hdevice,
    allocation: ddi::D3DKMT_HANDLE,
) -> Option<(usize, usize)> {
    if allocation == 0 {
        return None;
    }
    let dev = unsafe { helios_device(h) }?;
    let mut entries = dev.direct_scanout_allocations.borrow_mut();
    let before = entries.len();
    entries.retain(|(candidate, _)| *candidate != allocation);
    let removed = before - entries.len();
    let remaining = entries.len();
    drop(entries);
    if removed != 0 {
        dev.scanout_epoch.set(dev.scanout_epoch.get().wrapping_add(1));
    }
    (removed != 0).then_some((removed, remaining))
}

unsafe fn remember_scanout_target(
    h: Hdevice,
    raw: usize,
    allocation: ddi::D3DKMT_HANDLE,
    private: HeliosPresentPrivateData,
) {
    if raw == 0 || allocation == 0 || !private.is_valid() {
        return;
    }
    let Some(dev) = helios_device(h) else {
        return;
    };
    // The "largest area wins" policy. It is UNCHANGED by R809 and is now stated
    // where it happens instead of only in a field comment calling the pointer
    // "the largest scanout primary". A resolution change DOWNWARDS therefore
    // still keeps the older, larger geometry -- see SCANOUT_DOWNRES_KEPT, which
    // exists to measure how often that actually happens before the policy is
    // changed. Deferred, not adopted: making it an explicit counted policy is
    // R809's behaviour half.
    let area = (private.width as u64).saturating_mul(private.height as u64);
    let existing = dev.owned.scanout_target.borrow();
    let current_area = existing.as_ref().map_or(0u64, |t| {
        (t.width.get() as u64).saturating_mul(t.height.get() as u64)
    });
    // Observation only: how often a DirectPrimary record would be displaced by
    // the LINEAR import, which R809's deferred DirectPrimary-wins rule would
    // forbid. Counted here where the competing record is visible.
    if matches!(
        existing.as_ref().map(|t| &t.kind),
        Some(crate::device_funcs::ScanoutKind::KmdLinearImport { .. })
    ) {
        SCANOUT_DIRECT_OVER_LINEAR.fetch_add(1, Ordering::Relaxed);
    }
    if current_area != 0 && area < current_area {
        SCANOUT_DOWNRES_KEPT.fetch_add(1, Ordering::Relaxed);
        return;
    }
    drop(existing);
    let (Some(width), Some(height)) = (
        core::num::NonZeroU32::new(private.width),
        core::num::NonZeroU32::new(private.height),
    ) else {
        SCANOUT_ZERO_EXTENT.fetch_add(1, Ordering::Relaxed);
        log_error!(
            "DDI scanout target refused: zero extent {}x{} raw=0x{raw:x}",
            private.width, private.height
        );
        return;
    };
    *dev.owned.scanout_target.borrow_mut() = Some(crate::device_funcs::ScanoutTarget {
        resource_raw: raw,
        resource_id: private.resource_id,
        allocation,
        width,
        height,
        format: private.dxgi_format,
        kind: crate::device_funcs::ScanoutKind::DirectPrimary,
    });
    // Read back through the stored record rather than the parameters, so the
    // line reports what was actually committed. This is also what keeps
    // `allocation` a live field: pre-R809 `scanout_allocation` was written by
    // two writers and read by nobody, which the grouping made visible as a
    // dead-field warning. It is part of the scan-out identity the KMD matches
    // on, so it is kept and reported rather than deleted (that is T6's call).
    let stored = dev.owned.scanout_target.borrow();
    if let Some(t) = stored.as_ref() {
        log_error!(
            "DDI scanout target: raw=0x{:x} alloc=0x{:x} res_id={} {}x{} fmt={} pitch={}",
            t.resource_raw, t.allocation, t.resource_id, t.width, t.height, t.format,
            private.pitch
        );
    }
}

unsafe fn clear_scanout_target_if_matches(h: Hdevice, raw: usize) {
    if raw == 0 {
        return;
    }
    let Some(dev) = helios_device(h) else {
        return;
    };
    // Pre-R809 this reset six Cells and left `scanout_import` and
    // `scanout_generation` behind, so a cleared target still had a live import
    // describing it. There is now one value to clear, and clearing it releases
    // the imported COM reference with it -- nothing can be left stale because
    // nothing is left.
    let matches = dev
        .owned
        .scanout_target
        .borrow()
        .as_ref()
        .is_some_and(|t| t.resource_raw == raw);
    if matches {
        *dev.owned.scanout_target.borrow_mut() = None;
        log_error!("DDI scanout target cleared");
    }
}

fn is_dwm_process() -> bool {
    use std::sync::OnceLock;
    static IS_DWM: OnceLock<bool> = OnceLock::new();
    *IS_DWM.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .map(|n| n.eq_ignore_ascii_case("dwm.exe"))
            .unwrap_or(false)
    })
}

fn needs_wddm_texture_allocation(a: &ddi::D3D11DDIARG_CREATERESOURCE) -> bool {
    const DDI_BIND_PRESENT: u32 = 0x0000_0080;
    const DDI_MISC_SHARED: u32 = 0x0000_0002;
    const DDI_MISC_SHARED_KEYEDMUTEX: u32 = 0x0000_0100;

    !a.pPrimaryDesc.is_null()
        || (a.BindFlags & DDI_BIND_PRESENT) != 0
        || (a.MiscFlags & (DDI_MISC_SHARED | DDI_MISC_SHARED_KEYEDMUTEX)) != 0
}

/// Lazily query and import the KMD-owned LINEAR primary into DWM's existing
/// Venus device. The bridge uses this VkInstance's D3DKMT handles, so the import
/// and its resource attachment cannot accidentally land in another live Venus
/// instance hosted by the process.
unsafe fn ensure_kmd_scanout_target(h: Hdevice) -> bool {
    if !is_dwm_process() {
        return false;
    }
    let Some(dev) = helios_device(h) else {
        return false;
    };
    let epoch = dev.scanout_epoch.get();
    // The positive half of the cache is now the recorded target itself: a
    // KmdLinearImport target IS the successful probe result.
    if matches!(
        dev.owned.scanout_target.borrow().as_ref().map(|t| &t.kind),
        Some(crate::device_funcs::ScanoutKind::KmdLinearImport { .. })
    ) {
        return true;
    }
    // The negative half: without it, any of (a) primary_scanout_resource == 0,
    // (b) resource_is_live false, (c) the import returning 0 re-armed the full
    // walk + D3DKMTEscape + KMD virtio round-trip for the next bind and the
    // next flush.
    if let crate::device_funcs::ScanoutProbe::Unavailable { at_epoch } = dev.scanout_probe.get() {
        if at_epoch == epoch {
            return false;
        }
    }

    // The wrapper adopts the reference and returns the geometry together with
    // it, so the five out-params cannot be read after a failed open (when they
    // are whatever the bridge zeroed them to). R813.
    let opened = dev.dxvk.open_scanout_target();
    let (resource_id, width, height, pitch, generation) = opened
        .as_ref()
        .map_or((0, 0, 0, 0, 0), |(_, i)| {
            (i.resource_id, i.width, i.height, i.pitch, i.generation)
        });
    if opened.is_none() {
        dev.scanout_probe
            .set(crate::device_funcs::ScanoutProbe::Unavailable { at_epoch: epoch });
        if let Some(n) = SCANOUT_UNAVAILABLE_LOG_COUNT.first_n_then_every(16, 512) {
            log_error!(
                "DWM KMD scanout import unavailable at epoch {epoch} (x{}) — not re-probing until it moves",
                n + 1
            );
        }
        return false;
    }

    let Some((import, _)) = opened else {
        return false;
    };
    let raw = import.as_raw() as usize;
    let (Some(width_nz), Some(height_nz)) = (
        core::num::NonZeroU32::new(width),
        core::num::NonZeroU32::new(height),
    ) else {
        SCANOUT_ZERO_EXTENT.fetch_add(1, Ordering::Relaxed);
        log_error!("DWM KMD scanout import refused: zero extent {width}x{height}");
        dev.scanout_probe
            .set(crate::device_funcs::ScanoutProbe::Unavailable { at_epoch: epoch });
        return false;
    };
    // The geometry, the identity and the owning import are now written as ONE
    // value. Pre-R809 these were six Cells plus scanout_generation plus
    // scanout_import, and this writer set them with allocation = 0 and a
    // hard-coded format = 87 -- indistinguishable, to any reader, from a
    // DirectPrimary record.
    *dev.owned.scanout_target.borrow_mut() = Some(crate::device_funcs::ScanoutTarget {
        resource_raw: raw,
        resource_id,
        allocation: 0,
        width: width_nz,
        height: height_nz,
        // The target VkImage is BGRA; virtio scanout ignores alpha as XR24.
        format: 87,
        kind: crate::device_funcs::ScanoutKind::KmdLinearImport { import, generation },
    });
    // `generation` likewise: pre-R809 `scanout_generation` had one writer and
    // no reader. Reported from the stored variant so it is live state, not a
    // value written into a Cell nothing consults.
    let stored = dev.owned.scanout_target.borrow();
    let stored_gen = match stored.as_ref().map(|t| &t.kind) {
        Some(crate::device_funcs::ScanoutKind::KmdLinearImport { generation, .. }) => *generation,
        _ => 0,
    };
    log_error!(
        "DWM KMD scanout import ready: res_id={} {}x{} pitch={} gen={}",
        resource_id, width, height, pitch, stored_gen
    );
    drop(stored);
    true
}

/// Re-arming an `Unavailable` probe is driven by `scanout_epoch`, which the two
/// `direct_scanout_allocations` writers bump. The KMD's own scanout generation
/// is only readable THROUGH the probe this cache exists to avoid, so the
/// trigger has to be the UMD's own observation: a direct-scanout primary
/// appearing or going away is exactly what a mode change produces.
static SCANOUT_UNAVAILABLE_LOG_COUNT: LogThrottle = LogThrottle::new();


/// Remember a full-mode BGRA render target as DWM's current private optimal
/// composition surface. Holding our own COM reference makes the pointer safe
/// across the later Flush callback even if DWM releases the RTV first.
unsafe fn track_dwm_composition_target(
    h: Hdevice,
    resource_raw: usize,
    allocation: ddi::D3DKMT_HANDLE,
    width: u32,
    height: u32,
    format: u32,
) {
    // An exact Windows-designated primary already is the scanout backing.
    // Never create/select the legacy LINEAR copy target for it.
    let direct_primary = unsafe { helios_device(h) }.is_some_and(|dev| {
        dev.direct_scanout_allocations
            .borrow()
            .iter()
            .any(|(candidate, _)| *candidate == allocation)
    });
    if resource_raw == 0 || direct_primary || !unsafe { ensure_kmd_scanout_target(h) } {
        return;
    }
    let Some(dev) = helios_device(h) else {
        return;
    };
    let geometry_mismatch = dev.owned.scanout_target.borrow().as_ref().map_or(true, |t| {
        width != t.width.get()
            || height != t.height.get()
            || format != t.format
            || resource_raw == t.resource_raw
    });
    if geometry_mismatch {
        return;
    }

    if dev
        .owned
        .composition_source
        .borrow()
        .as_ref()
        .map(|r| r.as_raw() as usize == resource_raw)
        .unwrap_or(false)
    {
        return;
    }
    // SAFETY: `resource_raw` is kept alive by the bound RTV. ManuallyDrop makes
    // this a borrowed COM wrapper; clone takes the owned reference we retain.
    let borrowed =
        ManuallyDrop::new(unsafe { ID3D11Resource::from_raw(resource_raw as *mut c_void) });
    dev.owned.composition_source.replace(Some((*borrowed).clone()));
    log_error!(
        "DWM composition target selected: raw=0x{:x} {}x{} fmt={}",
        resource_raw, width, height, format
    );
}

/// Record the one required per-frame GPU copy on DWM's own command stream.
/// The following `ID3D11DeviceContext::Flush` submits both DWM's rendering and
/// this copy in order; DXVK then releases the shared LINEAR target externally.
unsafe fn publish_dwm_composition(context: &ID3D11DeviceContext, h: Hdevice) -> bool {
    if !unsafe { ensure_kmd_scanout_target(h) } {
        return false;
    }
    let Some(dev) = helios_device(h) else {
        return false;
    };
    let source = dev.owned.composition_source.borrow();
    let recorded = dev.owned.scanout_target.borrow();
    // The copy target must be a LINEAR import specifically -- a DirectPrimary
    // record describes a surface this path must never copy into. Pre-R809 the
    // `kind` did not exist, and the guard was that `scanout_import` happened to
    // hold a `Ready`.
    let (
        Some(source),
        Some(crate::device_funcs::ScanoutTarget {
            resource_id: target_id,
            width: target_w,
            height: target_h,
            kind: crate::device_funcs::ScanoutKind::KmdLinearImport { import: target, .. },
            ..
        }),
    ) = (source.as_ref(), recorded.as_ref())
    else {
        return false;
    };
    let (target_id, target_w, target_h) = (*target_id, target_w.get(), target_h.get());
    if source.as_raw() == target.as_raw() {
        return false;
    }
    context.CopySubresourceRegion(target, 0, 0, 0, 0, source, 0, None);
    let n = dev.scanout_copy_count.get().wrapping_add(1);
    dev.scanout_copy_count.set(n);
    if n == 1 {
        // THIS is entry into the COPY path, and it is a silent regression away
        // from the direct primary if it is not loud: the desktop still appears
        // either way, so nothing else distinguishes them. Merely opening the
        // LINEAR target does not qualify — that happens on every DWM boot while
        // this count stays 0.
        log_error!(
            "WARNING: DWM is compositing through the LEGACY LINEAR copy target — \
             the direct primary is NOT the presented surface for this device"
        );
    }
    if n <= 8 || n % 600 == 0 {
        log_error!(
            "DWM desktop->LINEAR scanout copy #{} res_id={} {}x{}",
            n,
            target_id,
            target_w,
            target_h
        );
    }
    true
}

unsafe fn copy_to_scanout_target(
    context: &ID3D11DeviceContext,
    h: Hdevice,
    src_h: ddi::D3D10DDI_HRESOURCE,
) -> bool {
    let Some(dev) = helios_device(h) else {
        return false;
    };
    let dst_raw = dev
        .owned
        .scanout_target
        .borrow()
        .as_ref()
        .map_or(0, |t| t.resource_raw);
    if dst_raw == 0 || dst_raw == resource_com_raw(src_h) {
        return false;
    }
    let Some(src) = load_resource(src_h) else {
        return false;
    };
    let dst = ManuallyDrop::new(ID3D11Resource::from_raw(dst_raw as *mut c_void));
    let (Ok(src_tex), Ok(dst_tex)) = (
        (*src).cast::<ID3D11Texture2D>(),
        (*dst).cast::<ID3D11Texture2D>(),
    ) else {
        return false;
    };
    let mut src_desc = D3D11_TEXTURE2D_DESC::default();
    let mut dst_desc = D3D11_TEXTURE2D_DESC::default();
    src_tex.GetDesc(&mut src_desc);
    dst_tex.GetDesc(&mut dst_desc);
    if src_desc.Format != dst_desc.Format
        || src_desc.SampleDesc.Count != 1
        || dst_desc.SampleDesc.Count != 1
    {
        log_error!(
            "DXGI Present scanout-copy skipped: src {}x{} fmt={} dst {}x{} fmt={}",
            src_desc.Width,
            src_desc.Height,
            src_desc.Format.0,
            dst_desc.Width,
            dst_desc.Height,
            dst_desc.Format.0
        );
        return false;
    }
    let w = src_desc.Width.min(dst_desc.Width);
    let hgt = src_desc.Height.min(dst_desc.Height);
    if w == 0 || hgt == 0 {
        return false;
    }
    let bx = D3D11_BOX {
        left: 0,
        top: 0,
        front: 0,
        right: w,
        bottom: hgt,
        back: 1,
    };
    context.CopySubresourceRegion(&*dst, 0, 0, 0, 0, &*src, 0, Some(&bx as *const D3D11_BOX));
    true
}

unsafe fn dxvk_resource_memory_info(h: Hdevice, obj: &ID3D11Resource) -> (u64, u64, u64, u32) {
    let Some(dev) = helios_device(h) else {
        return (0, 0, 0, 0);
    };
    let mut memory = 0u64;
    let mut size = 0u64;
    let mut offset = 0u64;
    let mut resource_id = 0u32;
    if dev.dxvk.get_resource_memory_info(
        obj.as_raw() as usize,
        &mut memory,
        &mut size,
        &mut offset,
        &mut resource_id,
    ) {
        (memory, size, offset, resource_id)
    } else {
        (0, 0, 0, 0)
    }
}

unsafe fn resource_dimensions(h_res: ddi::D3D10DDI_HRESOURCE) -> (u32, u32) {
    let Some(res) = load_resource(h_res) else {
        return (0, 0);
    };
    let Ok(tex) = (*res).cast::<ID3D11Texture2D>() else {
        return (0, 0);
    };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    tex.GetDesc(&mut desc);
    (desc.Width, desc.Height)
}

unsafe fn resource_sample_count(h_res: ddi::D3D10DDI_HRESOURCE) -> u32 {
    let Some(res) = load_resource(h_res) else {
        return 1;
    };
    let Ok(tex) = (*res).cast::<ID3D11Texture2D>() else {
        return 1;
    };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    tex.GetDesc(&mut desc);
    desc.SampleDesc.Count.max(1)
}

unsafe fn resource_dxgi_format(h_res: ddi::D3D10DDI_HRESOURCE) -> DXGI_FORMAT {
    let Some(res) = load_resource(h_res) else {
        return DXGI_FORMAT(0);
    };
    let Ok(tex) = (*res).cast::<ID3D11Texture2D>() else {
        return DXGI_FORMAT(0);
    };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    tex.GetDesc(&mut desc);
    desc.Format
}

unsafe fn resource_summary(
    h_res: ddi::D3D10DDI_HRESOURCE,
) -> (ddi::D3DKMT_HANDLE, &'static str, u32, u32, u32, u32) {
    let allocation = resource_allocation(h_res);
    let Some(res) = load_resource(h_res) else {
        return (allocation, "missing", 0, 0, 0, 0);
    };
    if let Ok(buf) = (*res).cast::<ID3D11Buffer>() {
        let mut desc = D3D11_BUFFER_DESC::default();
        buf.GetDesc(&mut desc);
        return (allocation, "buffer", desc.ByteWidth, 1, 1, 0);
    }
    if let Ok(tex) = (*res).cast::<ID3D11Texture2D>() {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        tex.GetDesc(&mut desc);
        return (
            allocation,
            "tex2d",
            desc.Width,
            desc.Height,
            desc.ArraySize,
            desc.Format.0 as u32,
        );
    }
    if let Ok(tex) = (*res).cast::<ID3D11Texture3D>() {
        let mut desc = D3D11_TEXTURE3D_DESC::default();
        tex.GetDesc(&mut desc);
        return (
            allocation,
            "tex3d",
            desc.Width,
            desc.Height,
            desc.Depth,
            desc.Format.0 as u32,
        );
    }
    (allocation, "resource", 0, 0, 0, 0)
}

unsafe fn store_rtv(
    h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW,
    obj: ID3D11RenderTargetView,
    resource_raw: usize,
    allocation: ddi::D3DKMT_HANDLE,
    width: u32,
    height: u32,
    format: u32,
) {
    let Some(slot) = boxed_slot(h_rtv) else {
        drop(obj);
        return;
    };
    slot.store(RtvState {
        com_raw: obj.into_raw() as usize,
        resource_raw,
        allocation,
        width,
        height,
        format,
    });
}

/// The `RtvState` behind a DDI render-target-view handle, or `None` for an
/// empty slot. The single place the RTV slot is decoded.
unsafe fn rtv_state(h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW) -> Option<&'static RtvState> {
    boxed_slot(h_rtv)?.get()
}

/// `rtv_state` for the runtime-tag dispatches. See `load_resource_at`.
unsafe fn rtv_state_at(handle_priv: *mut c_void) -> Option<&'static RtvState> {
    Slot::<Boxed<RtvState>>::from_priv(handle_priv)?.get()
}

unsafe fn load_rtv(
    h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW,
) -> Option<ManuallyDrop<ID3D11RenderTargetView>> {
    load_rtv_at(h_rtv.pDrvPrivate)
}

/// `load_rtv` for the runtime-tag dispatches (`Discard`, `ClearView`, the
/// tiled-resource barrier). See `load_resource_at`.
unsafe fn load_rtv_at(handle_priv: *mut c_void) -> Option<ManuallyDrop<ID3D11RenderTargetView>> {
    let state = rtv_state_at(handle_priv)?;
    if state.com_raw == 0 {
        return None;
    }
    Some(ManuallyDrop::new(ID3D11RenderTargetView::from_raw(
        state.com_raw as *mut c_void,
    )))
}

/// Geometry + identity of the RTV behind a handle, all zeros when absent.
///
/// `clear_rtv` used to re-derive these by casting the slot inline, duplicating
/// this function's body four lines from a `load_rtv` call on the same handle.
/// That copy is gone; the log line it feeds is unchanged. R803.
unsafe fn rtv_info(
    h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW,
) -> (ddi::D3DKMT_HANDLE, u32, u32, u32, usize) {
    rtv_state(h_rtv).map_or((0, 0, 0, 0, 0), |s| {
        (s.allocation, s.width, s.height, s.format, s.resource_raw)
    })
}

unsafe fn release_rtv(h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW) {
    let Some(slot) = boxed_slot(h_rtv) else {
        return;
    };
    // `take` empties the slot as it hands the box over, so a second release on
    // the same handle finds `None` instead of freeing twice.
    if let Some(state) = slot.take() {
        if state.com_raw != 0 {
            drop(IUnknown::from_raw(state.com_raw as *mut c_void));
        }
    }
}

unsafe fn release_resource(h: Hdevice, h_res: ddi::D3D10DDI_HRESOURCE) {
    let Some(slot) = boxed_slot(h_res) else {
        return;
    };
    // Take the box (and empty the slot) BEFORE the teardown below, which
    // re-enters this driver through helios_device/pfnDeallocateCb. A second
    // release of the same handle now finds an empty slot instead of freeing
    // twice. `state` is an owned Box, so the drop at the end of this scope is
    // what frees it -- the explicit `Box::from_raw` is gone.
    if let Some(mut state) = slot.take() {
        let state = &mut *state;
        clear_scanout_target_if_matches(h, (*state).com_raw);
        let allocation = (*state)
            .allocation
            .as_ref()
            .map(ResidentAllocation::handle)
            .unwrap_or(0);
        // The direct-scanout registry was add-only: it grew without bound across
        // mode changes and DWM primary generations inside one process, the
        // per-present `presented_primary_private` lookup became a linear scan
        // over dead entries, and if dxgkrnl reissues a freed D3DKMT_HANDLE the
        // lookup returns the previous primary's resource_id/width/height/pitch
        // for the new allocation — a stale scanout identity handed to the KMD.
        // `clear_scanout_target_if_matches` above only clears the `scanout_*`
        // Cells; it never touched this Vec.
        if let Some((removed, remaining)) = forget_direct_scanout_allocation(h, allocation) {
            log_error!(
                "DDI scanout registry: dropped {removed} entry alloc=0x{allocation:x} remaining={remaining}"
            );
        }
        // Evict while the allocation and runtime device are both still valid.
        // Option::take makes it impossible for ResourceState::drop to evict a
        // second time after pfnDeallocateCb.
        drop((*state).allocation.take());
        if allocation != 0 || !(*state).rt_resource.is_null() {
            if let Some(dev) = helios_device(h) {
                if !dev.kt_callbacks.is_null() {
                    if let Some(deallocate_cb) = (*dev.kt_callbacks).pfnDeallocateCb {
                        // D3DDDICB_DEALLOCATE contract: EITHER hResource (the
                        // runtime releases/closes every allocation it tracks
                        // for the resource — created AND opened instances) OR
                        // NumAllocations+HandleList with hResource NULL. Both
                        // together is E_INVALIDARG (the old 0x80070057, which
                        // also leaked opened resources).
                        let mut allocation = allocation;
                        // One exhaustive decision, then one construction. The
                        // hResource-and-HandleList-together shape the runtime
                        // rejects cannot be built from any DeallocateForm.
                        let form = DeallocateForm::select(
                            (*state).rt_resource,
                            (*state).ownership,
                            allocation,
                        );
                        let mut dealloc = match form {
                            DeallocateForm::ByResource(h_resource) => ddi::D3DDDICB_DEALLOCATE {
                                hResource: h_resource,
                                NumAllocations: 0,
                                HandleList: core::ptr::null_mut(),
                            },
                            DeallocateForm::ByHandleList(handle) => {
                                // Take the handle from the form, not from the
                                // surrounding local: the form is what validated
                                // it as non-zero, and `HandleList` must point at
                                // exactly that value.
                                allocation = handle.get();
                                ddi::D3DDDICB_DEALLOCATE {
                                    hResource: core::ptr::null_mut(),
                                    NumAllocations: 1,
                                    HandleList: &mut allocation,
                                }
                            }
                            DeallocateForm::Nothing { reason } => {
                                log_error!(
                                    "DDI deallocate_resource skip: {} alloc=0x{:x} km=0x{:x}",
                                    reason,
                                    allocation,
                                    (*state).km_resource
                                );
                                ddi::D3DDDICB_DEALLOCATE {
                                    hResource: core::ptr::null_mut(),
                                    NumAllocations: 0,
                                    HandleList: core::ptr::null_mut(),
                                }
                            }
                        };
                        if !matches!(form, DeallocateForm::Nothing { .. }) {
                            let hr = deallocate_cb(dev.h_rt_device, &mut dealloc);
                            trace_line!(
                                "DDI deallocate_resource: hr=0x{:08x} alloc=0x{:x} km=0x{:x} rt={:p} owned={}",
                                hr as u32,
                                allocation,
                                (*state).km_resource,
                                (*state).rt_resource,
                                (*state).ownership.owns()
                            );
                        }
                    }
                }
            }
        }
        if (*state).com_raw != 0 {
            drop(IUnknown::from_raw((*state).com_raw as *mut c_void));
        }
    }
}

/// Borrow the COM interface stored in a DDI handle (does not take ownership).
unsafe fn load_com<T: Interface>(h: impl ComHandle) -> Option<ManuallyDrop<T>> {
    load_com_at::<T>(h.drv_private())
}

/// `load_com` for the three runtime-tag dispatches (`discard_11_1`,
/// `clear_view_11_1`, `tiled_barrier_child`).
///
/// Those receive one `pDrvPrivate` plus a `D3D11DDI_HANDLETYPE` that selects
/// the payload at run time, so the static `ComHandle` bound cannot apply. Each
/// already matches the tag and calls the decoder its arm proved correct --
/// `load_resource` for HT_RESOURCE, `load_rtv` for HT_RENDERTARGETVIEW, this
/// for the bare-COM view tags. Do not call it from anywhere else: it is the
/// unchecked form `load_com` exists to replace.
unsafe fn load_com_at<T: Interface>(handle_priv: *mut c_void) -> Option<ManuallyDrop<T>> {
    Slot::<Com<T>>::from_priv(handle_priv)?.load()
}

/// Release the COM interface stored in a DDI handle.
unsafe fn release_com(h: impl ComHandle) {
    if let Some(slot) = Slot::<Com<IUnknown>>::from_priv(h.drv_private()) {
        slot.release();
    }
}

fn cpu_access(usage: u32) -> u32 {
    // D3D11_USAGE: DEFAULT=0, IMMUTABLE=1, DYNAMIC=2, STAGING=3.
    match usage {
        2 => D3D11_CPU_ACCESS_WRITE.0 as u32,
        3 => (D3D11_CPU_ACCESS_WRITE.0 | D3D11_CPU_ACCESS_READ.0) as u32,
        _ => 0,
    }
}

fn api_bind_flags(ddi_bind: u32) -> u32 {
    const DDI_PIPELINE_MASK: u32 = 0x0000_007f;
    const DDI_BIND_PRESENT: u32 = 0x0000_0080;
    const DDI_BIND_UAV: u32 = 0x0000_0100;
    const DDI_BIND_DECODER: u32 = 0x0000_0200;
    const DDI_BIND_VIDEO_ENCODER: u32 = 0x0000_0400;
    const DDI_BIND_CAPTURE: u32 = 0x0000_0800;

    let mut out = ddi_bind & DDI_PIPELINE_MASK;
    if ddi_bind & DDI_BIND_UAV != 0 {
        out |= D3D11_BIND_UNORDERED_ACCESS.0 as u32;
    }
    if ddi_bind & DDI_BIND_DECODER != 0 {
        out |= D3D11_BIND_DECODER.0 as u32;
    }
    if ddi_bind & DDI_BIND_VIDEO_ENCODER != 0 {
        out |= D3D11_BIND_VIDEO_ENCODER.0 as u32;
    }
    let _ = DDI_BIND_PRESENT | DDI_BIND_CAPTURE;
    out
}

fn api_misc_flags(ddi_misc: u32, ddi_bind: u32, is_buffer: bool) -> u32 {
    const DDI_MISC_AUTO_GEN_MIP_MAP: u32 = 0x0000_0001;
    const DDI_MISC_SHARED: u32 = 0x0000_0002;
    const DDI_MISC_DRAWINDIRECT_ARGS: u32 = 0x0000_0010;
    const DDI_MISC_BUFFER_ALLOW_RAW_VIEWS: u32 = 0x0000_0020;
    const DDI_MISC_BUFFER_STRUCTURED: u32 = 0x0000_0040;
    const DDI_MISC_RESOURCE_CLAMP: u32 = 0x0000_0080;
    const DDI_MISC_SHARED_KEYEDMUTEX: u32 = 0x0000_0100;
    const DDI_MISC_GDI_COMPATIBLE: u32 = 0x0000_0200;
    const DDI_MISC_TILED: u32 = 0x0000_4000;
    const DDI_MISC_TILE_POOL: u32 = 0x0000_8000;
    const DDI_BIND_PRESENT: u32 = 0x0000_0080;
    const API_MISC_SHARED_NTHANDLE: u32 = 0x0000_0800;
    const API_MISC_TILE_POOL: u32 = 0x0002_0000;
    const API_MISC_TILED: u32 = 0x0004_0000;

    if is_buffer {
        let mut out = ddi_misc
            & (DDI_MISC_DRAWINDIRECT_ARGS
                | DDI_MISC_BUFFER_ALLOW_RAW_VIEWS
                | DDI_MISC_BUFFER_STRUCTURED
                | DDI_MISC_RESOURCE_CLAMP);
        if ddi_misc & DDI_MISC_TILE_POOL != 0 {
            out |= API_MISC_TILE_POOL;
        }
        if ddi_misc & DDI_MISC_TILED != 0 {
            out |= API_MISC_TILED;
        }
        out
    } else {
        let mut out = ddi_misc
            & (DDI_MISC_AUTO_GEN_MIP_MAP
                | DDI_MISC_SHARED
                | DDI_MISC_RESOURCE_CLAMP
                | DDI_MISC_GDI_COMPATIBLE);

        // Producer-side D3D11 resources still need DXVK's NT-shareable export
        // path so native DWM can create shared handles normally. The DDI
        // open_resource path below intentionally imports the KMT resource as
        // plain shared to avoid creating a second DXVK keyed mutex.
        if ddi_misc & DDI_MISC_SHARED != 0 {
            out |= API_MISC_SHARED_NTHANDLE;
        }
        if ddi_misc & DDI_MISC_SHARED_KEYEDMUTEX != 0 || ddi_bind & DDI_BIND_PRESENT != 0 {
            out |= DDI_MISC_SHARED;
        }
        if ddi_misc & DDI_MISC_TILED != 0 {
            out |= API_MISC_TILED;
        }
        out
    }
}

// --- CalcPrivate*Size (all store one COM pointer) ---------------------------

unsafe extern "C" fn calc_size_resource(
    _h: Hdevice,
    _a: *const ddi::D3D11DDIARG_CREATERESOURCE,
) -> u64 {
    8
}
unsafe extern "C" fn calc_size_rtv(
    _h: Hdevice,
    _a: *const ddi::D3D10DDIARG_CREATERENDERTARGETVIEW,
) -> u64 {
    8
}

// --- Resources --------------------------------------------------------------

const RES_BUFFER: ddi::D3D10DDIRESOURCE_TYPE = ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_BUFFER;
const RES_BUFFEREX: ddi::D3D10DDIRESOURCE_TYPE =
    ddi::D3D10DDIRESOURCE_TYPE_D3D11DDIRESOURCE_BUFFEREX;
const RES_TEX2D: ddi::D3D10DDIRESOURCE_TYPE = ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_TEXTURE2D;
const RES_TEX1D: ddi::D3D10DDIRESOURCE_TYPE = ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_TEXTURE1D;
const RES_TEX3D: ddi::D3D10DDIRESOURCE_TYPE = ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_TEXTURE3D;
const RES_TEXCUBE: ddi::D3D10DDIRESOURCE_TYPE =
    ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_TEXTURECUBE;

/// The resource dimensions `create_resource` implements, as a closed set.
///
/// `D3D10DDIRESOURCE_TYPE` is a bindgen integer, so the conversion stays
/// fallible and exactly one counted catch-all survives at the conversion. The
/// guarantee is that no KNOWN dimension can be silently dropped by a match that
/// quietly grew a hole, not that the integer domain becomes closed.
#[derive(Copy, Clone, PartialEq, Eq)]
enum ResourceDimension {
    Buffer,
    Texture1D,
    Texture2D,
    Texture3D,
}

impl ResourceDimension {
    fn from_ddi(dimension: ddi::D3D10DDIRESOURCE_TYPE) -> Option<Self> {
        match dimension {
            RES_BUFFER | RES_BUFFEREX => Some(Self::Buffer),
            RES_TEX1D => Some(Self::Texture1D),
            RES_TEX2D | RES_TEXCUBE => Some(Self::Texture2D),
            RES_TEX3D => Some(Self::Texture3D),
            _ => None,
        }
    }
}

/// Add one allocation to the WDDM 2.x device residency list.
///
/// E_PENDING is completed with a blocking monitored-fence wait through the
/// runtime callback. No command referencing the allocation may be submitted
/// before that fence reaches `PagingFenceValue`.
unsafe fn make_resident(
    dev: &crate::device_funcs::HeliosDevice,
    handle: ddi::D3DKMT_HANDLE,
) -> Result<ResidentAllocation, i32> {
    const E_PENDING: i32 = 0x8000_000Au32 as i32;

    let Some(handle) = core::num::NonZeroU32::new(handle) else {
        log_error!("WDDM residency: zero allocation handle");
        return Err(E_INVALIDARG);
    };
    let Some(queue) = dev.paging_queue else {
        log_error!("WDDM residency: device has no paging queue");
        return Err(E_FAIL);
    };
    if dev.kt_callbacks.is_null() {
        log_error!("WDDM residency: no runtime callbacks");
        return Err(E_FAIL);
    }
    let Some(make_resident_cb) = (*dev.kt_callbacks).pfnMakeResidentCb else {
        log_error!("WDDM residency: pfnMakeResidentCb missing");
        return Err(E_FAIL);
    };
    let Some(evict_cb) = (*dev.kt_callbacks).pfnEvictCb else {
        // Do not acquire a residency reference that cannot be balanced.
        log_error!("WDDM residency: pfnEvictCb missing");
        return Err(E_FAIL);
    };

    let allocation = handle.get();
    let mut arg = ddi::D3DDDI_MAKERESIDENT::default();
    arg.hPagingQueue = queue.handle.get();
    arg.NumAllocations = 1;
    arg.AllocationList = &allocation;
    let hr = make_resident_cb(dev.h_rt_device, &mut arg);
    trace_line!(
        "WDDM residency: MakeResident alloc=0x{:x} hr=0x{:08x} fence={} trim={}",
        allocation,
        hr as u32,
        arg.PagingFenceValue,
        arg.NumBytesToTrim
    );
    if hr != 0 && hr != E_PENDING {
        log_error!(
            "WDDM residency: MakeResident FAILED alloc=0x{:x} hr=0x{:08x} trim={}",
            allocation, hr as u32, arg.NumBytesToTrim
        );
        return Err(hr);
    }

    let resident = ResidentAllocation {
        handle,
        h_rt_device: dev.h_rt_device,
        evict_cb,
    };
    if hr == E_PENDING {
        let Some(wait_cb) = (*dev.kt_callbacks).pfnWaitForSynchronizationObjectFromCpuCb else {
            log_error!("WDDM residency: E_PENDING but CPU fence-wait callback is missing");
            drop(resident);
            return Err(E_FAIL);
        };
        let sync_object = queue.sync_object.get();
        let fence_value = arg.PagingFenceValue;
        let mut wait = ddi::D3DDDICB_WAITFORSYNCHRONIZATIONOBJECTFROMCPU::default();
        wait.ObjectCount = 1;
        wait.ObjectHandleArray = &sync_object;
        wait.FenceValueArray = &fence_value;
        // hAsyncEvent == NULL selects the blocking, non-polling form.
        let wait_hr = wait_cb(dev.h_rt_device, &wait);
        trace_line!(
            "WDDM residency: wait alloc=0x{:x} fence={} observed={} hr=0x{:08x}",
            allocation,
            fence_value,
            queue.fence_value_cpu.as_ptr().read_volatile(),
            wait_hr as u32
        );
        if wait_hr != 0 {
            log_error!(
                "WDDM residency: paging-fence wait FAILED alloc=0x{:x} fence={} hr=0x{:08x}",
                allocation, fence_value, wait_hr as u32
            );
            drop(resident);
            return Err(wait_hr);
        }
    }

    Ok(resident)
}

unsafe fn deallocate_standalone(
    dev: &crate::device_funcs::HeliosDevice,
    allocation: ddi::D3DKMT_HANDLE,
) {
    if dev.kt_callbacks.is_null() {
        return;
    }
    let Some(deallocate_cb) = (*dev.kt_callbacks).pfnDeallocateCb else {
        return;
    };
    let mut allocation = allocation;
    let mut arg = ddi::D3DDDICB_DEALLOCATE {
        hResource: core::ptr::null_mut(),
        NumAllocations: 1,
        HandleList: &mut allocation,
    };
    let hr = deallocate_cb(dev.h_rt_device, &mut arg);
    log_error!(
        "DDI allocate rollback: alloc=0x{:x} hr=0x{:08x}",
        allocation, hr as u32
    );
}

unsafe fn allocate_wddm_resource(
    h: Hdevice,
    a: &ddi::D3D11DDIARG_CREATERESOURCE,
    mip0: &ddi::D3D10DDI_MIPINFO,
    h_rt: ddi::D3D10DDI_HRTRESOURCE,
    // `Some` = venus-backed (the KMD adopts our allocation), `None` = plain
    // KMD-backed standard allocation. This one `Option` replaces the three
    // independent `backing_blob_id != 0` conjunctions.
    backing: Option<VenusBacking>,
    // Kept separate from `scanout` on purpose -- see alloc.rs.
    direct_scanout_primary: bool,
    // Scan-out primary metadata. LINEAR paths use the queried COLOR row pitch;
    // direct OPTIMAL uses a logical scanout stride while QEMU validates the
    // opaque allocation with its exact Vulkan allocation size.
    scanout: Option<ScanoutGeometry>,
) -> Result<(Option<ResidentAllocation>, ddi::D3DKMT_HANDLE), i32> {
    const DDI_BIND_PRESENT: u32 = 0x0000_0080;

    let needs_allocation = needs_wddm_texture_allocation(a);
    if !needs_allocation {
        return Ok((None, 0));
    }

    let Some(dev) = helios_device(h) else {
        return Err(E_FAIL);
    };
    if dev.kt_callbacks.is_null() {
        log_error!("DDI allocate_wddm_resource: no KT callbacks");
        return Err(E_FAIL);
    }
    let Some(allocate_cb) = (*dev.kt_callbacks).pfnAllocateCb else {
        log_error!("DDI allocate_wddm_resource: pfnAllocateCb missing");
        return Err(E_FAIL);
    };

    let venus_ctx_id = dev.dxvk.venus_context_id();
    if venus_ctx_id == 0 {
        log_error!("DDI allocate_wddm_resource: no Venus context id");
    }

    const CROSS_ADAPTER_PITCH_ALIGN: u32 = 256;
    let raw_pitch = mip0
        .TexelWidth
        .saturating_mul(dxgi_bytes_per_pixel(a.Format as u32));
    let pitch =
        raw_pitch.saturating_add(CROSS_ADAPTER_PITCH_ALIGN - 1) & !(CROSS_ADAPTER_PITCH_ALIGN - 1);
    // A LINEAR scan-out primary reports its exact COLOR row pitch; use it verbatim so
    // `SET_SCANOUT_BLOB` reads rows at the true host stride instead of the
    // cross-adapter guess (a wrong stride shears the scanned-out image).
    let pitch = match scanout {
        Some(g) => g.pitch.get(),
        None => pitch,
    };
    let linear_size = (pitch as u64)
        .saturating_mul(mip0.TexelHeight.max(1) as u64)
        .max(4096);
    // A live backing with a zero blob_size still falls back to the linear size:
    // blob_id is the only field that gates a mode.
    let size = match backing {
        Some(b) if b.blob_size != 0 => b.blob_size,
        _ => linear_size,
    };

    // pPrimaryDesc is the runtime's authoritative primary classification.
    // A dedicated-copy source is intentionally OPTIMAL and has no scanout
    // pitch, but it still must be a WDDM primary or DXGI rejects every Flip
    // before DxgkDdiPresent with "Source of Flip must be primary".
    let marks_scanout_primary = !a.pPrimaryDesc.is_null();
    let meta_misc_flags = if direct_scanout_primary {
        a.MiscFlags | HELIOS_WDDM_ALLOC_MISC_PRIMARY | HELIOS_WDDM_ALLOC_MISC_DIRECT_SCANOUT
    } else if marks_scanout_primary {
        a.MiscFlags | HELIOS_WDDM_ALLOC_MISC_PRIMARY
    } else {
        a.MiscFlags
    };

    let mut private = RuntimeAllocPrivate {
        alloc: HeliosWddmAllocPrivate::new(
            if backing.is_some() {
                HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY
            } else {
                HELIOS_WDDM_ALLOC_KIND_STANDARD
            },
            venus_ctx_id,
            backing.map_or(0, |b| b.blob_id.get()),
            size,
            VIRTIO_GPU_BLOB_MEM_HOST3D,
            if backing.is_some() {
                // DXVK render targets are normally backed by device-local Venus
                // memory. virglrenderer rejects USE_MAPPABLE for non-host-visible
                // memory ("mem cannot support mappable blob"). They still must
                // be shareable so the host can export/import the backing memory.
                VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE
            } else {
                VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE
            },
            VIRTIO_GPU_MAP_CACHE_CACHED,
            // The venus resource id for the KMD to adopt. Was patched into the
            // struct after construction because the constructor could not
            // express the field; it is a parameter now, so the record reaches
            // the kernel fully initialised by one expression. R805.
            backing.map_or(0, |b| b.adopt_resource_id()),
        ),
        meta: HeliosWddmAllocMeta {
            width: mip0.TexelWidth,
            height: mip0.TexelHeight,
            format: dxgi_to_d3dddi_format(a.Format as u32),
            pitch,
            bind_flags: a.BindFlags,
            misc_flags: meta_misc_flags,
            // C1 identity: the creating vkAllocateMemory's exact parameters for
            // adopted venus-backed resources (a cross-process opener must import
            // with them). Zero for KMD-backed standard allocations — the KMD
            // fills them at CreateAllocation from its kernel venus client.
            venus_alloc_size: backing.map_or(0, |b| b.alloc_size),
            memory_type_index: backing.map_or(0, |b| b.memory_type_index),
            // Carry the creator's EXACT DXGI format so a cross-process opener
            // rebuilds the image with the same bpp/layout instead of a squashed
            // BGRA (the `format` field below is a lossy D3DDDIFORMAT for the
            // KMD's DescribeAllocation). `a.Format` is already a DXGI_FORMAT.
            dxgi_format: a.Format as u32,
            // Scan-out primary's real memory-plane-0 offset (0 for everything
            // else); the KMD adds it to the blob base in SET_SCANOUT_BLOB.
            plane_offset: scanout.map_or(0, |g| g.plane_offset),
        },
    };
    let pre_private_alloc = private.alloc;
    let pre_private_meta = private.meta;

    let pre_n = WDDM_ALLOC_LOG_COUNT.peek();
    if pre_n < 128 {
        log_error!(
            "DDI allocate_wddm_resource pre: blob=0x{:x} res_id={} ctx={} kind={} size={} alloc_size={} mti={} {}x{} fmt={} bind=0x{:x} misc=0x{:x} primary_desc={}",
            private.alloc.blob_id,
            private.alloc.adopt_resource_id,
            private.alloc.ctx_id,
            private.alloc.kind,
            private.alloc.size,
            backing.map_or(0, |b| b.alloc_size),
            backing.map_or(0, |b| b.memory_type_index),
            mip0.TexelWidth,
            mip0.TexelHeight,
            a.Format,
            a.BindFlags,
            a.MiscFlags,
            !a.pPrimaryDesc.is_null()
        );
    }

    let mut allocation_info = ddi::D3DDDI_ALLOCATIONINFO2::default();
    let private_ptr = (&mut private as *mut RuntimeAllocPrivate).cast();
    let private_size = core::mem::size_of::<RuntimeAllocPrivate>() as u32;
    allocation_info.pPrivateDriverData = private_ptr;
    allocation_info.PrivateDriverDataSize = private_size;
    let is_present = (a.BindFlags & DDI_BIND_PRESENT) != 0;
    let is_primary_allocation = !a.pPrimaryDesc.is_null();
    allocation_info.VidPnSourceId = if !a.pPrimaryDesc.is_null() {
        (*a.pPrimaryDesc).VidPnSourceId
    } else {
        0
    };
    // A pPrimaryDesc resource is a real WDDM primary regardless of whether its
    // backing is directly scannable or copied into the KMD-owned LINEAR target.
    allocation_info.Flags.Value = if is_primary_allocation { 1 } else { 0 };
    let mut alloc = ddi::D3DDDICB_ALLOCATE::default();
    // pfnAllocateCb expects the runtime resource handle for the resource whose
    // surfaces are being allocated. Shared resources additionally return an
    // hKMResource, but the association itself is not optional for present-only
    // allocations.
    alloc.pPrivateDriverData = private_ptr;
    alloc.PrivateDriverDataSize = private_size;
    alloc.hResource = h_rt.handle;
    alloc.NumAllocations = 1;
    alloc.__bindgen_anon_1.pAllocationInfo2 = &mut allocation_info;

    let hr = allocate_cb(dev.h_rt_device, &mut alloc);
    let h_allocation = allocation_info.hAllocation;
    let post_private_alloc = private.alloc;
    let post_private_meta = private.meta;
    let post_open_identity =
        unsafe { read_open_identity(private_ptr, private_size) };
    let n = WDDM_ALLOC_LOG_COUNT.next();
    if n < 128 || hr != 0 {
        log_error!(
            "DDI allocate_wddm_resource: hr=0x{:08x} alloc=0x{:x} km=0x{:x} rt={:p} assoc={:p} info={} rpriv={} size={} pitch={} blob=0x{:x} res_id={} ctx={} kind={} primary={} present={} vidpn={} {}x{} fmt={} bind=0x{:x} misc=0x{:x}",
            hr as u32,
            h_allocation,
            alloc.hKMResource,
            h_rt.handle,
            alloc.hResource,
            allocation_info.PrivateDriverDataSize,
            alloc.PrivateDriverDataSize,
            size,
            pitch,
            backing.map_or(0, |b| b.blob_id.get()),
            private.alloc.adopt_resource_id,
            private.alloc.ctx_id,
            private.alloc.kind,
            is_primary_allocation,
            is_present,
            allocation_info.VidPnSourceId,
            mip0.TexelWidth,
            mip0.TexelHeight,
            a.Format,
            a.BindFlags,
            a.MiscFlags
        );
        if pre_private_alloc.adopt_resource_id != post_private_alloc.adopt_resource_id
            || pre_private_alloc.kind != post_private_alloc.kind
            || pre_private_alloc.ctx_id != post_private_alloc.ctx_id
            || pre_private_alloc.blob_id != post_private_alloc.blob_id
            || pre_private_meta.venus_alloc_size != post_private_meta.venus_alloc_size
            || pre_private_meta.memory_type_index != post_private_meta.memory_type_index
        {
            log_error!(
                "DDI allocate_wddm_resource private mutated: pre blob=0x{:x} res_id={} ctx={} kind={} vas={} mti={} -> post blob=0x{:x} res_id={} ctx={} kind={} vas={} mti={}",
                pre_private_alloc.blob_id,
                pre_private_alloc.adopt_resource_id,
                pre_private_alloc.ctx_id,
                pre_private_alloc.kind,
                pre_private_meta.venus_alloc_size,
                pre_private_meta.memory_type_index,
                post_private_alloc.blob_id,
                post_private_alloc.adopt_resource_id,
                post_private_alloc.ctx_id,
                post_private_alloc.kind,
                post_private_meta.venus_alloc_size,
                post_private_meta.memory_type_index
            );
        }
        if let Some((ident, meta)) = post_open_identity {
            let meta = meta.unwrap_or_default();
            log_error!(
                "DDI allocate_wddm_resource callback private: hRT={:p} hKM=0x{:x} \
                 hAlloc=0x{:x} identity=res_id:{} kind:{} ctx:{} blob_size:{} \
                 venus_size:{} mem_type:{} meta={}x{} pitch:{} d3dfmt:{} \
                 dxgifmt:{} bind:0x{:x} misc:0x{:x}",
                h_rt.handle,
                alloc.hKMResource,
                h_allocation,
                ident.resource_id,
                ident.kind,
                ident.ctx_id,
                ident.blob_size,
                ident.venus_alloc_size,
                ident.memory_type_index,
                meta.width,
                meta.height,
                meta.pitch,
                meta.format,
                meta.dxgi_format,
                meta.bind_flags,
                meta.misc_flags,
            );
        }
    }
    if hr != 0 {
        return Err(hr);
    }

    match unsafe { make_resident(dev, h_allocation) } {
        Ok(resident) => Ok((Some(resident), alloc.hKMResource)),
        Err(resident_hr) => {
            // pfnAllocateCb succeeded, so this UMD owns the allocation even
            // though residency failed. Roll it back before surfacing the
            // failure; no partially initialized ResourceState is created.
            unsafe { deallocate_standalone(dev, h_allocation) };
            Err(resident_hr)
        }
    }
}

/// Common tail for a freshly created 2D texture resource (normal or scan-out
/// primary): record the venus backing identity, make the paired WDDM/KMD
/// allocation (carrying the scan-out row pitch + plane offset for a primary),
/// transfer venus-resource ownership to that allocation, KMT-stamp the DXVK
/// resource, and store it in the DDI handle. `scanout_pitch`/`scanout_offset`
/// are either the LINEAR COLOR layout or the direct-OPTIMAL logical scanout
/// metadata; they are 0/0 for non-primary resources.
unsafe fn finish_wddm_tex2d(
    h: Hdevice,
    a: &ddi::D3D11DDIARG_CREATERESOURCE,
    mip0: &ddi::D3D10DDI_MIPINFO,
    h_rt: ddi::D3D10DDI_HRTRESOURCE,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    res: ID3D11Resource,
    direct_scanout_primary: bool,
    scanout: Option<ScanoutGeometry>,
) {
    let (memory, memory_size, memory_offset, resource_id) = dxvk_resource_memory_info(h, &res);
    let (backing_blob_id, backing_blob_size, backing_resource_id) = if memory != 0
        && memory_offset == 0
        && memory <= u32::MAX as u64
    {
        (memory, memory_size, resource_id)
    } else {
        // Only resources that get a WDDM allocation (shared / keyed-mutex /
        // present / primary) NEED an importable backing — for those a
        // suballocated DXVK memory means a cross-process opener sees a
        // disconnected KMD blob (two-memory split), so shout. Private
        // textures are suballocated by design (18th session).
        let needs_importable = needs_wddm_texture_allocation(a);
        if memory != 0 && needs_importable {
            log_error!(
                    "DDI create_resource(tex2d): SHARED RESOURCE WITHOUT IMPORTABLE BACKING memory=0x{:x} res_id={} size={} offset={} bind=0x{:x} misc=0x{:x}",
                    memory, resource_id, memory_size, memory_offset, a.BindFlags, a.MiscFlags
                );
        } else if memory != 0 {
            trace_line!(
                "DDI create_resource(tex2d): private suballocated memory=0x{:x} size={} offset={}",
                memory,
                memory_size,
                memory_offset
            );
        }
        (0, 0, 0)
    };
    // C1: record the creating vkAllocateMemory's exact size + memory type into
    // the allocation trailer so cross-process openers import with them.
    let (mut venus_alloc_size, mut memory_type_index) = (0u64, 0u32);
    if backing_resource_id != 0 {
        if let Some(dev) = helios_device(h) {
            if !dev.dxvk.get_resource_alloc_identity(
                res.as_raw() as usize,
                &mut venus_alloc_size,
                &mut memory_type_index,
            ) {
                log_error!(
                    "DDI create_resource(tex2d): no venus alloc identity for res_id={}",
                    backing_resource_id
                );
            }
        }
    }
    let backing = VenusBacking::new(
        backing_blob_id,
        backing_blob_size,
        backing_resource_id,
        venus_alloc_size,
        memory_type_index,
    );
    let (allocation, km_resource) = match allocate_wddm_resource(
        h,
        a,
        mip0,
        h_rt,
        backing,
        direct_scanout_primary,
        scanout,
    ) {
        Ok(allocation) => allocation,
        Err(hr) => {
            log_error!(
                "DDI create_resource(tex2d): WDDM allocation/residency failed hr=0x{:08x}",
                hr as u32
            );
            set_runtime_error(h, hr);
            return;
        }
    };
    let allocation_handle = allocation
        .as_ref()
        .map(ResidentAllocation::handle)
        .unwrap_or(0);
    if allocation_handle != 0 && backing_resource_id != 0 {
        if let Some(dev) = helios_device(h) {
            // SAFETY: `res` is the live resource this DDI just created.
            if !unsafe { dev.dxvk.transfer_resource_ownership(res.as_raw() as usize) } {
                log_error!(
                    "DDI create_resource(tex2d): ownership transfer failed res_id={}",
                    backing_resource_id
                );
            }
        }
    }
    trace_line!(
        "DDI create_resource(tex2d): before KMT stamp km=0x{:x} alloc=0x{:x} blob=0x{:x} res_id={} blob_size={}",
        km_resource,
        allocation_handle,
        backing_blob_id,
        backing_resource_id,
        backing_blob_size
    );
    stamp_dxvk_resource_kmt_handles(h, &res, allocation_handle, km_resource);
    // Only the exact runtime-designated primary may identify itself through
    // PresentCb private data. Ordinary shared/pitched DWM sources are copied to
    // the KMD-owned LINEAR target and must use the identity-free refresh path.
    let present_private = match (
        direct_scanout_primary,
        core::num::NonZeroU32::new(backing_resource_id),
        scanout,
    ) {
        (true, Some(resource_id), Some(geometry)) => HeliosPresentPrivateData {
            plane_offset: geometry.plane_offset,
            magic: HELIOS_PRESENT_PRIVATE_MAGIC,
            version: HELIOS_PRESENT_PRIVATE_VERSION,
            resource_id: resource_id.get(),
            width: mip0.TexelWidth,
            height: mip0.TexelHeight,
            pitch: geometry.pitch.get(),
            dxgi_format: a.Format as u32,
            reserved: HELIOS_PRESENT_PRIVATE_FLAG_DIRECT_SCANOUT,
        },
        _ => empty_present_private(),
    };
    let scanout_raw = res.as_raw() as usize;
    if RESOURCE_LOG_COUNT.first_n(128).is_some() {
        trace_line!(
            "DDI create_resource(tex2d) ok after-stamp-call: {}x{} fmt={} usage={} bind=0x{:x} misc=0x{:x} sample={}x{}",
            mip0.TexelWidth, mip0.TexelHeight, a.Format, a.Usage, a.BindFlags,
            a.MiscFlags, a.SampleDesc.Count, a.SampleDesc.Quality
        );
    }
    store_resource(
        h_resource,
        res,
        allocation,
        km_resource,
        h_rt.handle,
        AllocationOwnership::CreatedByUmd, // via pfnAllocateCb above
        present_private,
    );
    if present_private.is_valid() {
        unsafe { remember_direct_scanout_allocation(h, allocation_handle, present_private) };
        remember_scanout_target(h, scanout_raw, allocation_handle, present_private);
    }
}

unsafe extern "C" fn create_resource(
    h: Hdevice,
    arg: *const ddi::D3D11DDIARG_CREATERESOURCE,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    h_rt: ddi::D3D10DDI_HRTRESOURCE,
) {
    clear_handle(h_resource);
    if arg.is_null() {
        log_error!("DDI CreateResource identity: null args");
        set_runtime_error(h, E_INVALIDARG);
        return;
    }
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let mip0 = if a.pMipInfoList.is_null() {
        ddi::D3D10DDI_MIPINFO {
            TexelWidth: 0,
            TexelHeight: 0,
            TexelDepth: 0,
            PhysicalWidth: 0,
            PhysicalHeight: 0,
            PhysicalDepth: 0,
        }
    } else {
        *a.pMipInfoList
    };

    // Build initial-data array if provided (one entry per subresource).
    let num_sub = (a.MipLevels.max(1) * a.ArraySize.max(1)) as usize;
    if let Some(identity_n) = CREATE_RESOURCE_IDENTITY_LOG_COUNT.first_n_then_every_from_one(512, 2048) {
        trace_line!(
            "DDI CreateResource identity: #{} hDrv={:p} hRT={:p} hDevice={:p} \
             dim={} fmt={} texel={}x{}x{} physical={}x{}x{} usage={} map=0x{:x} \
             bind=0x{:x} misc=0x{:x} mips={} array={} sample={}x{} byte_stride={} \
             decoder_type={} texture_layout={} initial={:p}/{} primary={:p}",
            identity_n,
            h_resource.pDrvPrivate,
            h_rt.handle,
            h.pDrvPrivate,
            a.ResourceDimension,
            a.Format,
            mip0.TexelWidth,
            mip0.TexelHeight,
            mip0.TexelDepth,
            mip0.PhysicalWidth,
            mip0.PhysicalHeight,
            mip0.PhysicalDepth,
            a.Usage,
            a.MapFlags,
            a.BindFlags,
            a.MiscFlags,
            a.MipLevels,
            a.ArraySize,
            a.SampleDesc.Count,
            a.SampleDesc.Quality,
            a.ByteStride,
            a.DecoderBufferType,
            a.TextureLayout,
            a.pInitialDataUP,
            if a.pInitialDataUP.is_null() {
                0
            } else {
                num_sub
            },
            a.pPrimaryDesc,
        );
        if !a.pPrimaryDesc.is_null() {
            let primary = &*a.pPrimaryDesc;
            let mode = &primary.ModeDesc;
            trace_line!(
                "DDI CreateResource primary: #{} hRT={:p} flags=0x{:x} vidpn={} \
                 mode={}x{} fmt={} refresh={}/{} scanline={} rotation={} scaling={} \
                 driver_flags=0x{:x}",
                identity_n,
                h_rt.handle,
                primary.Flags,
                primary.VidPnSourceId,
                mode.Width,
                mode.Height,
                mode.Format,
                mode.RefreshRate.Numerator,
                mode.RefreshRate.Denominator,
                mode.ScanlineOrdering,
                mode.Rotation,
                mode.Scaling,
                primary.DriverFlags,
            );
        }
        if !a.pInitialDataUP.is_null() {
            for subresource in 0..num_sub.min(16) {
                let up = &*a.pInitialDataUP.add(subresource);
                trace_line!(
                    "DDI CreateResource initial: #{} subresource={} sysmem={:p} \
                     row_pitch={} slice_pitch={}",
                    identity_n,
                    subresource,
                    up.pSysMem,
                    up.SysMemPitch,
                    up.SysMemSlicePitch,
                );
            }
        }
    }
    let mut init: Vec<D3D11_SUBRESOURCE_DATA> = Vec::new();
    if !a.pInitialDataUP.is_null() {
        for i in 0..num_sub {
            let up = &*a.pInitialDataUP.add(i);
            init.push(D3D11_SUBRESOURCE_DATA {
                pSysMem: up.pSysMem,
                SysMemPitch: up.SysMemPitch,
                SysMemSlicePitch: up.SysMemSlicePitch,
            });
        }
    }
    let init_ptr = if init.is_empty() {
        None
    } else {
        Some(init.as_ptr())
    };

    let cpu = cpu_access(a.Usage);

    let Some(dimension) = ResourceDimension::from_ddi(a.ResourceDimension) else {
        // The DDI returns void and `clear_handle` already nulled the slot, so
        // logging and returning told the runtime S_OK with a null driver
        // resource. Every later `load_resource` then returns None and the view
        // create / Map / Copy silently does nothing — "nothing draws", not an
        // error. E_INVALIDARG is in CreateTexture*'s documented return set.
        log_error!(
            "DDI create_resource: unhandled dimension {}",
            a.ResourceDimension
        );
        set_runtime_error(h, E_INVALIDARG);
        return;
    };
    match dimension {
        ResourceDimension::Buffer => {
            let bind = api_bind_flags(a.BindFlags);
            let misc = api_misc_flags(a.MiscFlags, a.BindFlags, true);
            if bind != a.BindFlags || misc != a.MiscFlags || !a.pPrimaryDesc.is_null() {
                log_error!(
                    "DDI create_resource(buffer): normalize bind 0x{:x}->0x{:x} misc 0x{:x}->0x{:x} primary={}",
                    a.BindFlags,
                    bind,
                    a.MiscFlags,
                    misc,
                    !a.pPrimaryDesc.is_null()
                );
            }
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: mip0.TexelWidth,
                Usage: D3D11_USAGE(a.Usage as i32),
                BindFlags: bind,
                CPUAccessFlags: cpu,
                MiscFlags: misc,
                StructureByteStride: a.ByteStride,
            };
            let (allocation, km_resource) =
                match allocate_wddm_resource(h, a, &mip0, h_rt, None, false, None) {
                    Ok(allocation) => allocation,
                    Err(hr) => {
                        log_error!(
                        "DDI create_resource(buffer): WDDM allocation/residency failed hr=0x{:08x}",
                        hr as u32
                    );
                        set_runtime_error(h, hr);
                        return;
                    }
                };
            let allocation_handle = allocation
                .as_ref()
                .map(ResidentAllocation::handle)
                .unwrap_or(0);
            let mut buf: Option<ID3D11Buffer> = None;
            let created = device.CreateBuffer(&desc, init_ptr, Some(&mut buf));
            if let Err(ref e) = created {
                log_error!("DDI create_resource(buffer) failed: {e:?}");
            }
            let res = match buf {
                Some(b) => match b.cast::<ID3D11Resource>() {
                    Ok(r) => Some(r),
                    Err(e) => {
                        log_error!(
                            "DDI create_resource(buffer): cast to ID3D11Resource failed: {e:?}"
                        );
                        None
                    }
                },
                None => {
                    if created.is_ok() {
                        log_error!("DDI create_resource(buffer): DXVK CreateBuffer returned no buffer");
                    }
                    None
                }
            };
            let mut stored = false;
            finish_create(h, created, res, |res| {
                stored = true;
                stamp_dxvk_resource_kmt_handles(h, &res, allocation_handle, km_resource);
                if RESOURCE_LOG_COUNT.first_n(128).is_some() {
                    log_error!(
                        "DDI create_resource(buffer) ok: bytes={} fmt={} usage={} bind=0x{:x} misc=0x{:x}",
                        mip0.TexelWidth, a.Format, a.Usage, bind, misc
                    );
                }
                store_resource(
                    h_resource,
                    res,
                    allocation,
                    km_resource,
                    h_rt.handle,
                    AllocationOwnership::CreatedByUmd, // via pfnAllocateCb above
                    empty_present_private(),
                );
            });
            if !stored && allocation_handle != 0 {
                // The buffer arm allocates first and creates second, so a failed
                // CreateBuffer (or a missing object, or a failed cast) leaves a
                // kernel allocation nobody can reach: the closure's
                // `Option<ResidentAllocation>` has been dropped by now — evicting
                // the residency reference — but pfnDeallocateCb was never called,
                // and `release_resource` cannot recover it later because
                // `clear_handle` already nulled pDrvPrivate at DDI entry, so
                // DestroyResource reads a null state pointer and returns. The
                // handle would stay associated with the runtime resource until
                // device teardown.
                //
                // Evict-then-deallocate is the order `release_resource`
                // documents, and dropping the closure above already did the
                // evict half. Same call `allocate_wddm_resource` makes for its
                // own rollback.
                if let Some(dev) = helios_device(h) {
                    deallocate_standalone(dev, allocation_handle);
                }
            }
        }
        ResourceDimension::Texture2D => {
            let bind = api_bind_flags(a.BindFlags);
            let mut misc = api_misc_flags(a.MiscFlags, a.BindFlags, false);
            if a.ResourceDimension == RES_TEXCUBE {
                misc |= D3D11_RESOURCE_MISC_TEXTURECUBE.0 as u32;
            }
            if a.MiscFlags != 0 {
                log_error!(
                    "DDI misc translation v2: ddi_misc=0x{:x} ddi_bind=0x{:x} api_misc=0x{:x}",
                    a.MiscFlags, a.BindFlags, misc
                );
            }
            if bind != a.BindFlags
                || (misc & !D3D11_RESOURCE_MISC_TEXTURECUBE.0 as u32) != a.MiscFlags
                || !a.pPrimaryDesc.is_null()
            {
                log_error!(
                    "DDI create_resource(tex2d): {}x{} fmt={} usage={} bind 0x{:x}->0x{:x} misc 0x{:x}->0x{:x} primary={} sample={}x{}",
                    mip0.TexelWidth,
                    mip0.TexelHeight,
                    a.Format,
                    a.Usage,
                    a.BindFlags,
                    bind,
                    a.MiscFlags,
                    misc,
                    !a.pPrimaryDesc.is_null(),
                    a.SampleDesc.Count,
                    a.SampleDesc.Quality
                );
            }
            let desc = D3D11_TEXTURE2D_DESC {
                Width: mip0.TexelWidth,
                Height: mip0.TexelHeight,
                MipLevels: a.MipLevels,
                ArraySize: a.ArraySize.max(1),
                Format: DXGI_FORMAT(a.Format as i32),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: a.SampleDesc.Count,
                    Quality: a.SampleDesc.Quality,
                },
                Usage: D3D11_USAGE(a.Usage as i32),
                BindFlags: bind,
                CPUAccessFlags: cpu,
                MiscFlags: misc,
            };
            // Windows' pPrimaryDesc is the authoritative, non-heuristic marker
            // for a scan-out primary. The supported 32-bit Windows primary
            // formats become dedicated OPTIMAL DMA_BUF exports.
            let is_scanout =
                !a.pPrimaryDesc.is_null() && matches!(a.Format as u32, 28 | 87 | 88);
            let mut handled = false;
            if is_scanout {
                // The QEMU fork reconstructs this exact same-driver OPTIMAL
                // DMA_BUF with the original blob allocation size. This avoids a
                // guest copy and any virtio protocol field or global modifier
                // extension. The host display backend currently reads it back.
                // The wrapper adopts the reference and returns the metadata
                // with it, so `rp`/`off` cannot be read on the failure path.
                // R813.
                let created = helios_device(h).and_then(|dev| {
                    dev.dxvk.create_scanout_texture2d(
                        mip0.TexelWidth,
                        mip0.TexelHeight,
                        a.Format as u32,
                        bind,
                        misc,
                    )
                });
                let (rp, off) = created.as_ref().map_or((0, 0), |(_, p, o)| (*p, *o));
                // BEHAVIOUR CHANGE (R806 sub-commit 2): a zero row pitch is a
                // failed scan-out-primary create, not a primary with no
                // geometry. Previously only `raw != 0` was checked, so a
                // non-zero resource with `rp == 0` would stamp
                // HELIOS_WDDM_ALLOC_MISC_PRIMARY | MISC_DIRECT_SCANOUT into the
                // KMD meta while finish_wddm_tex2d's present_private gate
                // failed -- a direct scan-out primary in the kernel that the
                // UMD never registered in direct_scanout_allocations and could
                // never identify through PresentCb private data. Nothing
                // detected that split state.
                //
                // Not reachable through today's bridge: create_ddi_scanout_
                // texture2d returns 0 for a zero width/height and otherwise
                // computes a non-zero pitch, so raw != 0 implies rp != 0. This
                // closes the cross-FFI contract dependency rather than a live
                // bug, which is why the counter is expected to stay 0.
                let geometry = ScanoutGeometry::new(rp as u32, off);
                // One match over the pair, so the created resource is moved
                // into exactly one arm and the refusal arm still owns it (and
                // therefore still releases it).
                match (created, geometry) {
                    (Some((res, _, _)), Some(geometry)) => {
                    log_error!(
                        "DDI create_resource(tex2d): direct scan-out primary {}x{} fmt={} logicalPitch={} offset={} (OPTIMAL DMA_BUF)",
                        mip0.TexelWidth, mip0.TexelHeight, a.Format, rp, off
                    );
                        finish_wddm_tex2d(
                            h, a, &mip0, h_rt, h_resource, res, true, Some(geometry),
                        );
                    }
                    (created, _) => {
                    // Loud failure over fake success: do NOT fall back to a plain
                    // primary — that reintroduces the black scan-out as a "working"
                    // desktop. A failure here is a real direct-scanout regression.
                    if let Some((res, _, _)) = created {
                        // The new arm: the bridge handed back a resource but no
                        // usable stride. Dropping the adopted wrapper releases
                        // it -- nothing else will. R813 removed the manual
                        // IUnknown::from_raw this used to need.
                        SCANOUT_PRIMARY_ZERO_PITCH.fetch_add(1, Ordering::Relaxed);
                        let raw = res.as_raw() as usize;
                        drop(res);
                        log_error!(
                            "DDI create_resource(tex2d): SCAN-OUT PRIMARY ZERO PITCH {}x{} fmt={} raw=0x{:x} offset={} -> refused (zero_pitch={})",
                            mip0.TexelWidth,
                            mip0.TexelHeight,
                            a.Format,
                            raw,
                            off,
                            SCANOUT_PRIMARY_ZERO_PITCH.load(Ordering::Relaxed)
                        );
                    }
                    log_error!(
                        "DDI create_resource(tex2d): SCAN-OUT PRIMARY CREATE FAILED {}x{} fmt={} bind=0x{:x} -> no primary (optimal/dmabuf rejected?)",
                        mip0.TexelWidth, mip0.TexelHeight, a.Format, bind
                    );
                    // The loudness has to reach the runtime, not stop at the log
                    // file: this DDI returns void, so pfnSetErrorCb is the only
                    // way CreateTexture2D fails instead of handing the caller
                    // S_OK with a null driver resource. E_OUTOFMEMORY is in
                    // CreateTexture2D's documented return set; the bridge
                    // returns 0 with no HRESULT to map through.
                    set_runtime_error(h, E_OUTOFMEMORY);
                    }
                }
                handled = true;
            }
            if !handled {
                let mut tex: Option<ID3D11Texture2D> = None;
                // The five existing tex2d trace gates are the same predicate;
                // name it once so the restructured arm cannot drift between them.
                let big = mip0.TexelWidth >= 1024 || mip0.TexelHeight >= 576 || misc != 0;
                if big {
                    log_error!(
                        "DDI create_resource(tex2d): calling DXVK CreateTexture2D {}x{} fmt={} bind=0x{:x} misc=0x{:x} init={} hrt={:p} mips={} array={} usage={} cpu=0x{:x} sample={}x{}",
                        mip0.TexelWidth,
                        mip0.TexelHeight,
                        a.Format,
                        bind,
                        misc,
                        init_ptr.is_some(),
                        h_rt.handle,
                        desc.MipLevels,
                        desc.ArraySize,
                        a.Usage,
                        cpu,
                        a.SampleDesc.Count,
                        a.SampleDesc.Quality
                    );
                }
                let created = device.CreateTexture2D(&desc, init_ptr, Some(&mut tex));
                match created {
                    Ok(()) => {
                        if big {
                            log_error!(
                                "DDI create_resource(tex2d): DXVK CreateTexture2D returned S_OK tex_present={}",
                                tex.is_some()
                            );
                        }
                    }
                    Err(ref e) => log_error!("DDI create_resource(tex2d) failed: {e:?}"),
                }
                let res = match tex {
                    Some(t) => match t.cast::<ID3D11Resource>() {
                        Ok(r) => {
                            if big {
                                log_error!("DDI create_resource(tex2d): cast to ID3D11Resource OK");
                            }
                            Some(r)
                        }
                        Err(_) => {
                            if big {
                                log_error!(
                                    "DDI create_resource(tex2d): cast to ID3D11Resource failed"
                                );
                            }
                            None
                        }
                    },
                    None => {
                        if big && created.is_ok() {
                            log_error!(
                                "DDI create_resource(tex2d): DXVK CreateTexture2D returned no texture"
                            );
                        }
                        None
                    }
                };
                finish_create(h, created, res, |res| {
                    finish_wddm_tex2d(h, a, &mip0, h_rt, h_resource, res, false, None);
                });
            }
        }
        ResourceDimension::Texture1D => {
            // Same shape as the tex3d arm: create first, then allocate, then
            // store — no fallible step between the allocation and the store, so
            // it does not need R407's rollback. All four view translators
            // already handle RES_TEX1D; this arm is what stops
            // `CreateTexture1D` from being an outright failure now that the
            // catch-all reports.
            let bind = api_bind_flags(a.BindFlags);
            let misc = api_misc_flags(a.MiscFlags, a.BindFlags, false);
            log_error!(
                "DDI create_resource(tex1d): {} fmt={} usage={} bind 0x{:x}->0x{:x} misc 0x{:x}->0x{:x} init={} mips={} array={}",
                mip0.TexelWidth,
                a.Format,
                a.Usage,
                a.BindFlags,
                bind,
                a.MiscFlags,
                misc,
                init_ptr.is_some(),
                a.MipLevels,
                a.ArraySize
            );
            let desc = D3D11_TEXTURE1D_DESC {
                Width: mip0.TexelWidth,
                MipLevels: a.MipLevels,
                ArraySize: a.ArraySize.max(1),
                Format: DXGI_FORMAT(a.Format as i32),
                Usage: D3D11_USAGE(a.Usage as i32),
                BindFlags: bind,
                CPUAccessFlags: cpu,
                MiscFlags: misc,
            };
            let mut tex: Option<ID3D11Texture1D> = None;
            let created = device.CreateTexture1D(&desc, init_ptr, Some(&mut tex));
            if let Err(ref e) = created {
                log_error!("DDI create_resource(tex1d) failed: {e:?}");
            }
            let res = match tex {
                Some(t) => match t.cast::<ID3D11Resource>() {
                    Ok(r) => Some(r),
                    Err(_) => {
                        log_error!("DDI create_resource(tex1d): cast to ID3D11Resource failed");
                        None
                    }
                },
                None => {
                    if created.is_ok() {
                        log_error!(
                            "DDI create_resource(tex1d): DXVK CreateTexture1D returned no texture"
                        );
                    }
                    None
                }
            };
            finish_create(h, created, res, |res| {
                let (allocation, km_resource) =
                    match allocate_wddm_resource(h, a, &mip0, h_rt, None, false, None) {
                        Ok(allocation) => allocation,
                        Err(hr) => {
                            log_error!(
                                "DDI create_resource(tex1d): WDDM allocation/residency failed hr=0x{:08x}",
                                hr as u32
                            );
                            set_runtime_error(h, hr);
                            return;
                        }
                    };
                let allocation_handle = allocation
                    .as_ref()
                    .map(ResidentAllocation::handle)
                    .unwrap_or(0);
                stamp_dxvk_resource_kmt_handles(h, &res, allocation_handle, km_resource);
                log_error!(
                    "DDI create_resource(tex1d) ok: {} fmt={} bind=0x{:x} misc=0x{:x}",
                    mip0.TexelWidth, a.Format, bind, misc
                );
                store_resource(
                    h_resource,
                    res,
                    allocation,
                    km_resource,
                    h_rt.handle,
                    AllocationOwnership::CreatedByUmd,
                    empty_present_private(),
                );
            });
        }
        ResourceDimension::Texture3D => {
            let bind = api_bind_flags(a.BindFlags);
            let misc = api_misc_flags(a.MiscFlags, a.BindFlags, false);
            log_error!(
                "DDI create_resource(tex3d): {}x{}x{} fmt={} usage={} bind 0x{:x}->0x{:x} misc 0x{:x}->0x{:x} init={} mips={}",
                mip0.TexelWidth,
                mip0.TexelHeight,
                mip0.TexelDepth,
                a.Format,
                a.Usage,
                a.BindFlags,
                bind,
                a.MiscFlags,
                misc,
                init_ptr.is_some(),
                a.MipLevels
            );
            let desc = D3D11_TEXTURE3D_DESC {
                Width: mip0.TexelWidth,
                Height: mip0.TexelHeight,
                Depth: mip0.TexelDepth.max(1),
                MipLevels: a.MipLevels,
                Format: DXGI_FORMAT(a.Format as i32),
                Usage: D3D11_USAGE(a.Usage as i32),
                BindFlags: bind,
                CPUAccessFlags: cpu,
                MiscFlags: misc,
            };
            let mut tex: Option<ID3D11Texture3D> = None;
            let created = device.CreateTexture3D(&desc, init_ptr, Some(&mut tex));
            if let Err(ref e) = created {
                log_error!("DDI create_resource(tex3d) failed: {e:?}");
            }
            let res = match tex {
                Some(t) => match t.cast::<ID3D11Resource>() {
                    Ok(r) => Some(r),
                    Err(_) => {
                        log_error!("DDI create_resource(tex3d): cast to ID3D11Resource failed");
                        None
                    }
                },
                None => {
                    if created.is_ok() {
                        log_error!(
                            "DDI create_resource(tex3d): DXVK CreateTexture3D returned no texture"
                        );
                    }
                    None
                }
            };
            finish_create(h, created, res, |res| {
                // The allocation-failure arm below already reported through
                // set_runtime_error; `return` leaves the closure, and this is
                // the last statement of create_resource, so that is the same
                // exit it was before.
                let (allocation, km_resource) =
                    match allocate_wddm_resource(h, a, &mip0, h_rt, None, false, None) {
                        Ok(allocation) => allocation,
                        Err(hr) => {
                            log_error!(
                                "DDI create_resource(tex3d): WDDM allocation/residency failed hr=0x{:08x}",
                                hr as u32
                            );
                            set_runtime_error(h, hr);
                            return;
                        }
                    };
                let allocation_handle = allocation
                    .as_ref()
                    .map(ResidentAllocation::handle)
                    .unwrap_or(0);
                stamp_dxvk_resource_kmt_handles(h, &res, allocation_handle, km_resource);
                log_error!(
                    "DDI create_resource(tex3d) ok: {}x{}x{} fmt={} bind=0x{:x} misc=0x{:x}",
                    mip0.TexelWidth, mip0.TexelHeight, mip0.TexelDepth, a.Format, bind, misc
                );
                store_resource(
                    h_resource,
                    res,
                    allocation,
                    km_resource,
                    h_rt.handle,
                    AllocationOwnership::CreatedByUmd,
                    empty_present_private(),
                );
            });
        }
    }
}

unsafe extern "C" fn destroy_resource(h: Hdevice, h_resource: ddi::D3D10DDI_HRESOURCE) {
    release_resource(h, h_resource);
}

unsafe extern "C" fn open_resource(
    h: Hdevice,
    arg: *const ddi::D3D10DDIARG_OPENRESOURCE,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    h_rt: ddi::D3D10DDI_HRTRESOURCE,
) {
    clear_handle(h_resource);

    if arg.is_null() {
        log_error!("DDI open_resource: null args");
        set_runtime_error(h, E_INVALIDARG);
        return;
    }

    let a = &*arg;
    let info2 = unsafe { a.__bindgen_anon_1.pOpenAllocationInfo2 };
    let mut allocation: ddi::D3DKMT_HANDLE = 0;
    // C1 identity ABI: the KMD wrote a versioned HeliosWddmOpenIdentity record
    // into the open-time private data in DxgkDdiOpenAllocation, after
    // validating the backing venus resource is LIVE. Prefer the per-allocation
    // buffer; fall back to the resource-level one. No adopt-id heuristics.
    let mut identity =
        unsafe { read_opened_allocation(a.pPrivateDriverData, a.PrivateDriverDataSize) };
    // Distinguishes "no identity anywhere" from "identity present, trailer
    // absent" in the refusal below; the second is a producer/KMD bug that used
    // to be swallowed by a 1x1 default.
    let mut identity_without_trailer = matches!(
        unsafe { read_open_identity(a.pPrivateDriverData, a.PrivateDriverDataSize) },
        Some((_, None))
    );
    if a.NumAllocations != 0 && info2.is_null() {
        log_error!("DDI open_resource FAILED: allocation array is null");
        set_runtime_error(h, E_INVALIDARG);
        return;
    }
    for index in 0..a.NumAllocations as usize {
        let info = &*info2.add(index);
        if index == 0 {
            allocation = info.hAllocation;
        }
        let allocation_identity =
            unsafe { read_open_identity(info.pPrivateDriverData, info.PrivateDriverDataSize) };
        // Only a candidate that carries the meta TRAILER can be selected — the
        // type says so now. The KMD stamps the identity into both the
        // per-allocation and the resource-level private buffer
        // (create_allocation.rs `write_open_identity`), but the resource-level
        // copy for KMD standard allocations is the pristine
        // GetStandardAllocationDriverData output whose size the KMD does not
        // choose: it can hold a valid 48-byte identity with no 16-byte trailer,
        // and selecting it loses the real geometry.
        match allocation_identity {
            Some((ident, Some(meta))) => {
                if identity.is_none() {
                    identity = Some(OpenedAllocation { ident, meta });
                }
            }
            Some((_, None)) => identity_without_trailer = true,
            None => {}
        }
        if let Some((ident, meta)) = allocation_identity {
            let meta = meta.unwrap_or_default();
            log_error!(
                "DDI OpenResource allocation: index={} hDrv={:p} hRT={:p} hKM=0x{:x} \
                 hAlloc=0x{:x} private={:p}/{} gpuva=0x{:x} res_id={} kind={} ctx={} \
                 blob_size={} venus_size={} mem_type={} {}x{} pitch={} d3dfmt={} \
                 dxgifmt={} bind=0x{:x} misc=0x{:x}",
                index,
                h_resource.pDrvPrivate,
                h_rt.handle,
                a.hKMResource.handle,
                info.hAllocation,
                info.pPrivateDriverData,
                info.PrivateDriverDataSize,
                info.GpuVirtualAddress,
                ident.resource_id,
                ident.kind,
                ident.ctx_id,
                ident.blob_size,
                ident.venus_alloc_size,
                ident.memory_type_index,
                meta.width,
                meta.height,
                meta.pitch,
                meta.format,
                meta.dxgi_format,
                meta.bind_flags,
                meta.misc_flags,
            );
        } else {
            log_error!(
                "DDI OpenResource allocation: index={} hDrv={:p} hRT={:p} hKM=0x{:x} \
                 hAlloc=0x{:x} private={:p}/{} gpuva=0x{:x} identity=missing",
                index,
                h_resource.pDrvPrivate,
                h_rt.handle,
                a.hKMResource.handle,
                info.hAllocation,
                info.pPrivateDriverData,
                info.PrivateDriverDataSize,
                info.GpuVirtualAddress,
            );
        }
    }
    log_error!(
        "DDI OpenResource resource: hDrv={:p} hRT={:p} num_alloc={} hKM=0x{:x} private={:p}/{}",
        h_resource.pDrvPrivate,
        h_rt.handle,
        a.NumAllocations,
        a.hKMResource.handle,
        a.pPrivateDriverData,
        a.PrivateDriverDataSize,
    );

    // A shared open without a KMD identity record cannot alias the real
    // surface. The old metadata-texture fallback fabricated a blank texture
    // here and stamped it with the real KMT handles — draws "succeeded" and
    // the shared content stayed black forever (audit U-B2). Fail loudly
    // instead so the producer-side bug gets found.
    //
    // The trailer is part of that record, not an optional extra: it carries the
    // width/height/pitch/format the import is built from. Defaulting it to
    // 1x1 BGRA produced exactly the audited failure — a real-KMT-stamped,
    // resident, storable alias whose geometry was wrong. Because that alias is
    // UNDERSIZE relative to the true allocation, the oversize guard that caught
    // the 38th-session import regression cannot see it either.
    let Some(opened) = identity else {
        if identity_without_trailer {
            log_error!(
                "DDI open_resource FAILED: venus identity carries no meta trailer (hKM={:?} alloc=0x{:x}) -> E_FAIL",
                a.hKMResource, allocation
            );
        } else {
            log_error!(
                "DDI open_resource FAILED: no venus identity record (hKM={:?} alloc=0x{:x}) -> E_FAIL",
                a.hKMResource, allocation
            );
        }
        set_runtime_error(h, E_FAIL);
        return;
    };
    let ident = opened.ident;
    let meta = opened.meta;

    if a.hKMResource.handle == 0 {
        log_error!(
            "DDI open_resource FAILED: no hKMResource (res_id={}) -> E_FAIL",
            ident.resource_id
        );
        set_runtime_error(h, E_FAIL);
        return;
    }

    let Some(dev) = helios_device(h) else {
        log_error!("DDI open_resource FAILED: no Helios device -> E_FAIL");
        set_runtime_error(h, E_FAIL);
        return;
    };

    let open_bind = api_bind_flags(meta.bind_flags);
    let open_misc = meta.misc_flags
        & (D3D11_RESOURCE_MISC_SHARED.0 as u32
            | D3D11_RESOURCE_MISC_RESOURCE_CLAMP.0 as u32
            | D3D11_RESOURCE_MISC_GDI_COMPATIBLE.0 as u32);
    // Prefer the identity's venus alloc size; the meta trailer carries the
    // creator-recorded copy for KMD standard allocations.
    let venus_alloc_size = if ident.venus_alloc_size != 0 {
        ident.venus_alloc_size
    } else {
        meta.venus_alloc_size
    };
    // Rebuild the image with the creator's EXACT DXGI format. Falls back to the
    // lossy D3DDDIFORMAT translation only when the creator did not record a DXGI
    // format (0 = legacy trailer / KMD standard allocation, which are BGRA). The
    // old unconditional `d3dddi_to_dxgi_format(meta.format)` collapsed every
    // surface to BGRA — an A8/R8 mask then rebuilt as 4bpp needed 4x the memory
    // and its (correctly sized) import was refused as "undersized".
    let open_dxgi_format = if meta.dxgi_format != 0 {
        meta.dxgi_format
    } else {
        d3dddi_to_dxgi_format(meta.format).0 as u32
    };
    let cross_context_optimal = ident.kind == HELIOS_WDDM_ALLOC_KIND_STANDARD
        && meta.misc_flags & HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE != 0;
    log_error!(
        "DDI open_resource identity: res_id={} alloc_size={} mem_type={} kind={} ctx={} meta_bind=0x{:x} meta_misc=0x{:x} open_bind=0x{:x} open_misc=0x{:x} dxgi_fmt={} d3dddi_fmt={}",
        ident.resource_id, venus_alloc_size, ident.memory_type_index, ident.kind, ident.ctx_id,
        meta.bind_flags, meta.misc_flags, open_bind, open_misc, open_dxgi_format, meta.format
    );
    // Ordinary shared images always retain their creator's OPTIMAL contract.
    // Production scanout uses a separate KMD-owned plain-LINEAR target; never
    // rebuild DWM imports as DRM-modifier images (the .38 regression).
    let opened = dev.dxvk.open_texture2d(
        meta.width.max(1),
        meta.height.max(1),
        open_dxgi_format,
        open_bind,
        open_misc,
        a.hKMResource.handle,
        ident.resource_id,
        venus_alloc_size,
        ident.memory_type_index,
        false,
        false,
        cross_context_optimal,
    );
    if opened.is_none() {
        // Import of a KMD-validated-live resource failed: a real bug, not a
        // condition to paper over with substitute content (audit C1.3).
        log_error!(
            "DDI open_resource FAILED: ddi-shared import {}x{} d3dfmt={} alloc=0x{:x} hKM={:?} res_id={} -> E_FAIL",
            meta.width, meta.height, meta.format, allocation, a.hKMResource, ident.resource_id
        );
        set_runtime_error(h, E_FAIL);
        return;
    }

    let Some(res) = opened else {
        return;
    };
    let raw = res.as_raw() as usize;
    stamp_dxvk_resource_kmt_handles(h, &res, allocation, a.hKMResource.handle);
    let resident = match unsafe { make_resident(dev, allocation) } {
        Ok(resident) => resident,
        Err(hr) => {
            log_error!(
                "DDI open_resource FAILED: MakeResident alloc=0x{:x} hr=0x{:08x}",
                allocation, hr as u32
            );
            set_runtime_error(h, hr);
            return;
        }
    };
    log_error!(
        "DDI open_resource ddi-shared ok: {}x{} d3dfmt={} alloc=0x{:x} hKM={:?} raw=0x{:x}",
        meta.width, meta.height, meta.format, allocation, a.hKMResource, raw
    );
    store_resource(
        h_resource,
        res,
        Some(resident),
        a.hKMResource.handle,
        h_rt.handle,
        AllocationOwnership::OpenedByRuntime,
        empty_present_private(),
    );
}

unsafe extern "C" fn calc_size_opened_resource(
    _h: Hdevice,
    _arg: *const ddi::D3D10DDIARG_OPENRESOURCE,
) -> u64 {
    8
}

unsafe extern "C" fn resolve_shared_resource(
    h: ddi::HANDLE,
    arg: *const ddi::D3DDDIARG_RESOLVESHAREDRESOURCE,
) -> i32 {
    let h_resource: *mut c_void = if arg.is_null() {
        core::ptr::null_mut()
    } else {
        (*arg).hResource as *mut c_void
    };
    let resource = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: h_resource,
    };
    let alloc = resource_allocation(resource);
    let (width, height) = resource_dimensions(resource);
    log_error!(
        "DDI ResolveSharedResource: hDevice={:p} hResource={:p} alloc=0x{:x} {}x{}",
        h, h_resource, alloc, width, height
    );
    if let Some(context) = d3d11_context(Hdevice {
        pDrvPrivate: h as *mut c_void,
    }) {
        context.Flush();
    }
    0
}

unsafe extern "C" fn dxgi_resolve_shared_resource(
    arg: *mut ddi::DXGI_DDI_ARG_RESOLVESHAREDRESOURCE,
) -> i32 {
    let (h_device, h_resource): (usize, usize) = if arg.is_null() {
        (0, 0)
    } else {
        ((*arg).hDevice as usize, (*arg).hResource as usize)
    };
    let resource = dxgi_resource_handle(h_resource as ddi::DXGI_DDI_HRESOURCE);
    let alloc = resource_allocation(resource);
    let (width, height) = resource_dimensions(resource);
    trace_line!(
        "DXGI ResolveSharedResource: hDevice=0x{:x} hResource=0x{:x} alloc=0x{:x} {}x{}",
        h_device,
        h_resource,
        alloc,
        width,
        height
    );
    if let Some(context) = d3d11_context(Hdevice {
        pDrvPrivate: h_device as *mut c_void,
    }) {
        context.Flush();
    }
    0
}

// --- Render target views ----------------------------------------------------

unsafe extern "C" fn create_rtv(
    h: Hdevice,
    arg: *const ddi::D3D10DDIARG_CREATERENDERTARGETVIEW,
    h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW,
    _h_rt: ddi::D3D10DDI_HRTRENDERTARGETVIEW,
) {
    clear_handle(h_rtv);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let Some(res) = load_resource(a.hDrvResource) else {
        log_error!("DDI create_rtv: resource handle empty");
        return;
    };
    let Some(desc) = rtv_desc(a, a.hDrvResource) else {
        log_error!(
            "DDI create_rtv: unsupported resource dimension {} fmt={}",
            a.ResourceDimension, a.Format
        );
        return;
    };
    let mut rtv: Option<ID3D11RenderTargetView> = None;
    let created = device.CreateRenderTargetView(&*res, Some(&desc), Some(&mut rtv));
    if let Err(ref e) = created {
        log_error!(
            "DDI create_rtv failed: dim={} fmt={} {e:?}",
            a.ResourceDimension, a.Format
        );
    }
    finish_create(h, created, rtv, |v| {
        let n = VIEW_LOG_COUNT.next();
        let allocation = resource_allocation(a.hDrvResource);
        let (width, height) = resource_dimensions(a.hDrvResource);
        if n < 128 {
            trace_line!(
                "DDI create_rtv ok: dim={} fmt={} alloc=0x{:x} {}x{}",
                a.ResourceDimension, a.Format, allocation, width, height
            );
        }
        store_rtv(
            h_rtv,
            v,
            resource_com_raw(a.hDrvResource),
            allocation,
            width,
            height,
            a.Format as u32,
        );
    });
}

unsafe fn rtv_desc(
    a: &ddi::D3D10DDIARG_CREATERENDERTARGETVIEW,
    h_res: ddi::D3D10DDI_HRESOURCE,
) -> Option<D3D11_RENDER_TARGET_VIEW_DESC> {
    let format = DXGI_FORMAT(a.Format as i32);
    match a.ResourceDimension {
        RES_BUFFER | RES_BUFFEREX => {
            let b = a.__bindgen_anon_1.Buffer;
            Some(D3D11_RENDER_TARGET_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_RTV_DIMENSION_BUFFER,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Buffer: D3D11_BUFFER_RTV {
                        Anonymous1: D3D11_BUFFER_RTV_0 {
                            FirstElement: b.__bindgen_anon_1.FirstElement,
                        },
                        Anonymous2: D3D11_BUFFER_RTV_1 {
                            NumElements: b.__bindgen_anon_2.NumElements,
                        },
                    },
                },
            })
        }
        RES_TEX1D => {
            let t = a.__bindgen_anon_1.Tex1D;
            if t.ArraySize > 1 {
                Some(D3D11_RENDER_TARGET_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_RTV_DIMENSION_TEXTURE1DARRAY,
                    Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                        Texture1DArray: D3D11_TEX1D_ARRAY_RTV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                })
            } else {
                Some(D3D11_RENDER_TARGET_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_RTV_DIMENSION_TEXTURE1D,
                    Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                        Texture1D: D3D11_TEX1D_RTV {
                            MipSlice: t.MipSlice,
                        },
                    },
                })
            }
        }
        RES_TEX2D => {
            let t = a.__bindgen_anon_1.Tex2D;
            let is_msaa = resource_sample_count(h_res) > 1;
            if is_msaa && t.ArraySize > 1 {
                Some(D3D11_RENDER_TARGET_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2DMSARRAY,
                    Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                        Texture2DMSArray: D3D11_TEX2DMS_ARRAY_RTV {
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                })
            } else if is_msaa {
                Some(D3D11_RENDER_TARGET_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2DMS,
                    Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                        Texture2DMS: D3D11_TEX2DMS_RTV {
                            UnusedField_NothingToDefine: 0,
                        },
                    },
                })
            } else if t.ArraySize > 1 {
                Some(D3D11_RENDER_TARGET_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2DARRAY,
                    Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                        Texture2DArray: D3D11_TEX2D_ARRAY_RTV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                })
            } else {
                Some(D3D11_RENDER_TARGET_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_RTV {
                            MipSlice: t.MipSlice,
                        },
                    },
                })
            }
        }
        RES_TEX3D => {
            let t = a.__bindgen_anon_1.Tex3D;
            Some(D3D11_RENDER_TARGET_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE3D,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Texture3D: D3D11_TEX3D_RTV {
                        MipSlice: t.MipSlice,
                        FirstWSlice: t.FirstW,
                        WSize: t.WSize,
                    },
                },
            })
        }
        RES_TEXCUBE => {
            let t = a.__bindgen_anon_1.TexCube;
            Some(D3D11_RENDER_TARGET_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Texture2DArray: D3D11_TEX2D_ARRAY_RTV {
                        MipSlice: t.MipSlice,
                        FirstArraySlice: t.FirstArraySlice,
                        ArraySize: t.ArraySize,
                    },
                },
            })
        }
        _ => None,
    }
}

unsafe extern "C" fn destroy_rtv(_h: Hdevice, h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW) {
    release_rtv(h_rtv);
}

// --- Depth-stencil views ----------------------------------------------------

unsafe extern "C" fn calc_size_dsv(
    _h: Hdevice,
    _a: *const ddi::D3D11DDIARG_CREATEDEPTHSTENCILVIEW,
) -> u64 {
    8
}

unsafe extern "C" fn create_dsv(
    h: Hdevice,
    arg: *const ddi::D3D11DDIARG_CREATEDEPTHSTENCILVIEW,
    h_dsv: ddi::D3D10DDI_HDEPTHSTENCILVIEW,
    _h_rt: ddi::D3D10DDI_HRTDEPTHSTENCILVIEW,
) {
    clear_handle(h_dsv);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let Some(res) = load_resource(a.hDrvResource) else {
        log_error!("DDI create_dsv: resource handle empty");
        return;
    };
    let Some(desc) = dsv_desc(a, a.hDrvResource) else {
        log_error!(
            "DDI create_dsv: unsupported resource dimension {} fmt={}",
            a.ResourceDimension, a.Format
        );
        return;
    };
    let mut dsv: Option<ID3D11DepthStencilView> = None;
    let created = device.CreateDepthStencilView(&*res, Some(&desc), Some(&mut dsv));
    if let Err(ref e) = created {
        log_error!(
            "DDI create_dsv failed: dim={} fmt={} flags=0x{:x} {e:?}",
            a.ResourceDimension, a.Format, a.Flags
        );
    }
    finish_create(h, created, dsv, |v| {
        if VIEW_LOG_COUNT.first_n(128).is_some() {
            trace_line!(
                "DDI create_dsv ok: dim={} fmt={} flags=0x{:x}",
                a.ResourceDimension, a.Format, a.Flags
            );
        }
        store_com(h_dsv, v);
    });
}

unsafe fn dsv_desc(
    a: &ddi::D3D11DDIARG_CREATEDEPTHSTENCILVIEW,
    h_res: ddi::D3D10DDI_HRESOURCE,
) -> Option<D3D11_DEPTH_STENCIL_VIEW_DESC> {
    let format = DXGI_FORMAT(a.Format as i32);
    match a.ResourceDimension {
        RES_TEX1D => {
            let t = a.__bindgen_anon_1.Tex1D;
            if t.ArraySize > 1 {
                Some(D3D11_DEPTH_STENCIL_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_DSV_DIMENSION_TEXTURE1DARRAY,
                    Flags: a.Flags,
                    Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                        Texture1DArray: D3D11_TEX1D_ARRAY_DSV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                })
            } else {
                Some(D3D11_DEPTH_STENCIL_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_DSV_DIMENSION_TEXTURE1D,
                    Flags: a.Flags,
                    Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                        Texture1D: D3D11_TEX1D_DSV {
                            MipSlice: t.MipSlice,
                        },
                    },
                })
            }
        }
        RES_TEX2D => {
            let t = a.__bindgen_anon_1.Tex2D;
            let is_msaa = resource_sample_count(h_res) > 1;
            if is_msaa && t.ArraySize > 1 {
                Some(D3D11_DEPTH_STENCIL_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2DMSARRAY,
                    Flags: a.Flags,
                    Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                        Texture2DMSArray: D3D11_TEX2DMS_ARRAY_DSV {
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                })
            } else if is_msaa {
                Some(D3D11_DEPTH_STENCIL_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2DMS,
                    Flags: a.Flags,
                    Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                        Texture2DMS: D3D11_TEX2DMS_DSV {
                            UnusedField_NothingToDefine: 0,
                        },
                    },
                })
            } else if t.ArraySize > 1 {
                Some(D3D11_DEPTH_STENCIL_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2DARRAY,
                    Flags: a.Flags,
                    Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                        Texture2DArray: D3D11_TEX2D_ARRAY_DSV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                })
            } else {
                Some(D3D11_DEPTH_STENCIL_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2D,
                    Flags: a.Flags,
                    Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_DSV {
                            MipSlice: t.MipSlice,
                        },
                    },
                })
            }
        }
        RES_TEXCUBE => {
            let t = a.__bindgen_anon_1.TexCube;
            Some(D3D11_DEPTH_STENCIL_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2DARRAY,
                Flags: a.Flags,
                Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                    Texture2DArray: D3D11_TEX2D_ARRAY_DSV {
                        MipSlice: t.MipSlice,
                        FirstArraySlice: t.FirstArraySlice,
                        ArraySize: t.ArraySize,
                    },
                },
            })
        }
        _ => None,
    }
}

unsafe extern "C" fn destroy_dsv(_h: Hdevice, h_dsv: ddi::D3D10DDI_HDEPTHSTENCILVIEW) {
    release_com(h_dsv);
}

unsafe extern "C" fn clear_rtv(
    h: Hdevice,
    h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW,
    color: *mut f32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(rtv) = load_rtv(h_rtv) else {
        return;
    };
    let rgba: [f32; 4] = if color.is_null() {
        [0.0; 4]
    } else {
        [*color, *color.add(1), *color.add(2), *color.add(3)]
    };
    {
        let (allocation, width, height, format, _resource_raw) = rtv_info(h_rtv);
        if allocation != 0 || width != 0 || height != 0 || format != 0 {
            if let Some(n) = CLEAR_RTV_LOG_COUNT.first_n_then_every_from_one(64, 512) {
                log_error!(
                    "DDI ClearRenderTargetView #{} alloc=0x{:x} {}x{} fmt={} rgba=({:.3},{:.3},{:.3},{:.3})",
                    n + 1,
                    allocation,
                    width,
                    height,
                    format,
                    rgba[0],
                    rgba[1],
                    rgba[2],
                    rgba[3]
                );
            }
        }
    }
    context.ClearRenderTargetView(&*rtv, &rgba);
}

unsafe extern "C" fn clear_dsv(
    h: Hdevice,
    h_dsv: ddi::D3D10DDI_HDEPTHSTENCILVIEW,
    flags: u32,
    depth: f32,
    stencil: u8,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(dsv) = load_com::<ID3D11DepthStencilView>(h_dsv) else {
        return;
    };
    context.ClearDepthStencilView(&*dsv, flags, depth, stencil);
}

// --- Copy / Map / Flush -----------------------------------------------------

unsafe extern "C" fn resource_copy(
    h: Hdevice,
    h_dst: ddi::D3D10DDI_HRESOURCE,
    h_src: ddi::D3D10DDI_HRESOURCE,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let (Some(dst), Some(src)) = (
        load_resource(h_dst),
        load_resource(h_src),
    ) else {
        if COPY_LOG_COUNT.first_n(256).is_some() {
            log_error!(
                "DDI resource_copy missing resource dst_priv={:p} src_priv={:p}",
                h_dst.pDrvPrivate, h_src.pDrvPrivate
            );
        }
        return;
    };
    let dst_alloc = resource_allocation(h_dst);
    let src_alloc = resource_allocation(h_src);
    let n = COPY_LOG_COUNT.next();
    if n < 256 || dst_alloc != 0 || src_alloc != 0 {
        trace_line!(
            "DDI resource_copy dst_alloc=0x{:x} src_alloc=0x{:x}",
            dst_alloc, src_alloc
        );
    }
    context.CopyResource(&*dst, &*src);
}

unsafe extern "C" fn resource_copy_region(
    h: Hdevice,
    h_dst: ddi::D3D10DDI_HRESOURCE,
    dst_subresource: u32,
    dst_x: u32,
    dst_y: u32,
    dst_z: u32,
    h_src: ddi::D3D10DDI_HRESOURCE,
    src_subresource: u32,
    box_: *const ddi::D3D10_DDI_BOX,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let dst = load_resource(h_dst);
    let src = load_resource(h_src);
    let dst_summary = resource_summary(h_dst);
    let src_summary = resource_summary(h_src);
    let (dst_rt, dst_km) = resource_parent_handles(h_dst);
    let (src_rt, src_km) = resource_parent_handles(h_src);
    let n = COPY_REGION_LOG_COUNT.next();
    if n < 1024 || dst.is_none() || src.is_none() {
        trace_line!(
            "DDI ResourceCopyRegion: #{} dstDrv={:p} dstRT={:p} dstKM=0x{:x} \
             dstAlloc=0x{:x} dst={}x{} fmt={} srcDrv={:p} srcRT={:p} srcKM=0x{:x} \
             srcAlloc=0x{:x} src={}x{} fmt={} dstSub={} xyz={},{},{} srcSub={} box={:p} \
             dstOk={} srcOk={}",
            n,
            h_dst.pDrvPrivate,
            dst_rt,
            dst_km,
            dst_summary.0,
            dst_summary.2,
            dst_summary.3,
            dst_summary.5,
            h_src.pDrvPrivate,
            src_rt,
            src_km,
            src_summary.0,
            src_summary.2,
            src_summary.3,
            src_summary.5,
            dst_subresource,
            dst_x,
            dst_y,
            dst_z,
            src_subresource,
            box_,
            dst.is_some(),
            src.is_some(),
        );
    }
    let (Some(dst), Some(src)) = (dst, src) else {
        return;
    };
    let bx;
    let bx_ptr = if box_.is_null() {
        None
    } else {
        let b = &*box_;
        bx = D3D11_BOX {
            left: b.left as u32,
            top: b.top as u32,
            front: b.front as u32,
            right: b.right as u32,
            bottom: b.bottom as u32,
            back: b.back as u32,
        };
        Some(&bx as *const D3D11_BOX)
    };
    context.CopySubresourceRegion(
        &*dst,
        dst_subresource,
        dst_x,
        dst_y,
        dst_z,
        &*src,
        src_subresource,
        bx_ptr,
    );
}

unsafe extern "C" fn resource_copy_region_11_1(
    h: Hdevice,
    h_dst: ddi::D3D10DDI_HRESOURCE,
    dst_subresource: u32,
    dst_x: u32,
    dst_y: u32,
    dst_z: u32,
    h_src: ddi::D3D10DDI_HRESOURCE,
    src_subresource: u32,
    box_: *const ddi::D3D10_DDI_BOX,
    _copy_flags: u32,
) {
    resource_copy_region(
        h,
        h_dst,
        dst_subresource,
        dst_x,
        dst_y,
        dst_z,
        h_src,
        src_subresource,
        box_,
    );
}

unsafe extern "C" fn resource_resolve_subresource(
    h: Hdevice,
    h_dst: ddi::D3D10DDI_HRESOURCE,
    dst_subresource: u32,
    h_src: ddi::D3D10DDI_HRESOURCE,
    src_subresource: u32,
    format: ddi::DXGI_FORMAT,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let (Some(dst), Some(src)) = (
        load_resource(h_dst),
        load_resource(h_src),
    ) else {
        return;
    };
    context.ResolveSubresource(
        &*dst,
        dst_subresource,
        &*src,
        src_subresource,
        DXGI_FORMAT(format as i32),
    );
}

unsafe extern "C" fn resource_is_staging_busy(
    _h: Hdevice,
    _h_resource: ddi::D3D10DDI_HRESOURCE,
) -> i32 {
    0
}

unsafe extern "C" fn resource_map(
    h: Hdevice,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    subresource: u32,
    map_type: ddi::D3D10_DDI_MAP,
    _map_flags: u32,
    mapped: *mut ddi::D3D10DDI_MAPPED_SUBRESOURCE,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(res) = load_resource(h_resource) else {
        return;
    };
    let mut out = D3D11_MAPPED_SUBRESOURCE::default();
    // DDI D3D10_DDI_MAP values match D3D11_MAP (READ=1, WRITE=2, ...).
    match context.Map(
        &*res,
        subresource,
        D3D11_MAP(map_type as i32),
        0,
        Some(&mut out),
    ) {
        Ok(()) => {
            let allocation = resource_allocation(h_resource);
            let n = MAP_LOG_COUNT.next();
            if n < 256 || allocation != 0 {
                trace_line!(
                    "DDI resource_map ok alloc=0x{:x} sub={} map={} rowPitch={} depthPitch={} pData={:p}",
                    allocation,
                    subresource,
                    map_type,
                    out.RowPitch,
                    out.DepthPitch,
                    out.pData
                );
            }
            if !mapped.is_null() {
                (*mapped).pData = out.pData;
                (*mapped).RowPitch = out.RowPitch;
                (*mapped).DepthPitch = out.DepthPitch;
            }
        }
        Err(e) => {
            log_error!("DDI resource_map failed: {e:?}");
            if !mapped.is_null() {
                (*mapped).pData = core::ptr::null_mut();
            }
        }
    }
}

/// `pfnDynamicConstantBufferMapNoOverwrite`, which exists only in
/// `D3D11_1DDI_DEVICEFUNCS` and later. Its PFN type is `PFND3D10DDI_RESOURCEMAP`
/// — the same shape as `pfnDynamicIABufferMapNoOverwrite`, which `install()`
/// already points at `resource_map` — and `D3D10_DDI_MAP_WRITE_NOOVERWRITE`
/// equals `D3D11_MAP_WRITE_NO_OVERWRITE` (4), so no-overwrite semantics reach
/// DXVK unchanged.
///
/// The wrapper exists only to make the slot's first use measurable: it spent
/// its life on `ddi_noop_device`, a stub that returns without touching the
/// caller's `D3D10DDI_MAPPED_SUBRESOURCE` even though filling it is this
/// slot's entire job.
unsafe extern "C" fn dynamic_cb_map_no_overwrite(
    h: Hdevice,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    subresource: u32,
    map_type: ddi::D3D10_DDI_MAP,
    map_flags: u32,
    mapped: *mut ddi::D3D10DDI_MAPPED_SUBRESOURCE,
) {
    static FIRST_HIT: AtomicUsize = AtomicUsize::new(0);
    if FIRST_HIT.fetch_add(1, Ordering::Relaxed) == 0 {
        trace_line!(
            "DDI DynamicConstantBufferMapNoOverwrite: first hit sub={} map={}",
            subresource,
            map_type
        );
    }
    resource_map(h, h_resource, subresource, map_type, map_flags, mapped);
}

unsafe extern "C" fn resource_unmap(
    h: Hdevice,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    subresource: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(res) = load_resource(h_resource) else {
        return;
    };
    context.Unmap(&*res, subresource);
}

unsafe extern "C" fn srv_read_after_write_hazard(
    _h: Hdevice,
    _srv: ddi::D3D10DDI_HSHADERRESOURCEVIEW,
    _resource: ddi::D3D10DDI_HRESOURCE,
) {
}

unsafe extern "C" fn resource_read_after_write_hazard(
    _h: Hdevice,
    _resource: ddi::D3D10DDI_HRESOURCE,
) {
}

// Unreachable: nothing at or above 0x000b_0022 is advertised, so
// NegotiatedInterface has no WDDM2_1 variant to dispatch here. The code stays
// in place; its deletion is T6's u-core-V02.
#[allow(dead_code)]
unsafe fn sync_token_cb(h: Hdevice, arg: *const ddi::D3DDDIARG_SYNCTOKEN, release: bool) {
    let Some(dev) = helios_device(h) else {
        log_error!("DDI sync_token: missing device");
        return;
    };
    if dev.kt_callbacks.is_null() || arg.is_null() {
        log_error!(
            "DDI sync_token: missing callbacks={} arg={}",
            dev.kt_callbacks.is_null(),
            arg.is_null()
        );
        return;
    }

    let a = &*arg;
    let cb_arg = ddi::D3DDDICB_SYNCTOKEN {
        hSyncToken: a.hSyncToken,
        BroadcastContextCount: 0,
        BroadcastContextArray: core::ptr::null(),
    };
    let cb = if release {
        (*dev.kt_callbacks).pfnReleaseResourceCb
    } else {
        (*dev.kt_callbacks).pfnAcquireResourceCb
    };
    let Some(cb) = cb else {
        log_error!(
            "DDI sync_token: callback missing release={} resource={:p} token={:p}",
            release, a.hResource, a.hSyncToken
        );
        return;
    };
    let hr = cb(dev.h_rt_device, &cb_arg);
    log_error!(
        "DDI sync_token: release={} resource={:p} token={:p} hr=0x{:08x}",
        release, a.hResource, a.hSyncToken, hr as u32
    );
}

unsafe extern "C" fn acquire_resource(h: Hdevice, arg: *const ddi::D3DDDIARG_SYNCTOKEN) {
    sync_token_cb(h, arg, false);
}

unsafe extern "C" fn release_resource_sync(h: Hdevice, arg: *const ddi::D3DDDIARG_SYNCTOKEN) {
    sync_token_cb(h, arg, true);
}

// Unreachable: nothing at or above 0x000b_0022 is advertised, so
// NegotiatedInterface has no WDDM2_1 variant to dispatch here. The code stays
// in place; its deletion is T6's u-core-V02.
#[allow(dead_code)]
unsafe fn sync_token_cb_2_1(
    h: Hdevice,
    resource: ddi::D3D10DDI_HRESOURCE,
    token: ddi::HANDLE,
    release: bool,
) {
    let arg = ddi::D3DDDIARG_SYNCTOKEN {
        hResource: resource.pDrvPrivate,
        hSyncToken: token,
    };
    sync_token_cb(h, &arg, release);
}

// Unreachable: nothing at or above 0x000b_0022 is advertised, so
// NegotiatedInterface has no WDDM2_1 variant to dispatch here. The code stays
// in place; its deletion is T6's u-core-V02.
#[allow(dead_code)]
unsafe extern "C" fn acquire_resource_2_1(
    h: Hdevice,
    resource: ddi::D3D10DDI_HRESOURCE,
    token: ddi::HANDLE,
) {
    sync_token_cb_2_1(h, resource, token, false);
}

// Unreachable: nothing at or above 0x000b_0022 is advertised, so
// NegotiatedInterface has no WDDM2_1 variant to dispatch here. The code stays
// in place; its deletion is T6's u-core-V02.
#[allow(dead_code)]
unsafe extern "C" fn release_resource_2_1(
    h: Hdevice,
    resource: ddi::D3D10DDI_HRESOURCE,
    token: ddi::HANDLE,
) {
    sync_token_cb_2_1(h, resource, token, true);
}

unsafe extern "C" fn flush(h: Hdevice) {
    if let Some(context) = d3d11_context(h) {
        let published = unsafe { publish_dwm_composition(&context, h) };
        context.Flush();
        if published {
            if let Some(dev) = helios_device(h) {
                // This marker is the exact dirty edge for the fallback scanout
                // copy recorded immediately above. The KMD captures its Venus
                // watermark in Render and emits RESOURCE_FLUSH from the
                // used-ring DPC only after all preceding work retires.
                let _ = unsafe { submit_runtime_refresh(dev) };
            }
        }
    }
}

unsafe extern "C" fn flush_11_1(h: Hdevice, _flush_flags: u32) -> ddi::BOOL {
    flush(h);
    1
}

unsafe extern "C" fn discard_11_1(
    h: Hdevice,
    handle_type: ddi::D3D11DDI_HANDLETYPE,
    handle: *mut c_void,
    _rects: *const ddi::D3D10_DDI_RECT,
    num_rects: u32,
) {
    if D3D11_1_LOG_COUNT.first_n(64).is_some() {
        trace_line!(
            "DDI D3D11.1 Discard: type={handle_type} rects={num_rects}"
        );
    }

    // A rect-limited discard invalidates ONLY the given sub-rects. D3D11's
    // Discard is a hint, so discarding LESS than requested is always legal —
    // discarding more is not: forwarding these as full-view discards made
    // DXVK reinitialize the whole image, wiping the undamaged 99% of dwm's
    // flip backbuffer every frame (dwm redraws only the dirty region and
    // rect-discards exactly that region on the incoming buffer). Match
    // upstream DXVK's DiscardView1 behaviour: drop partial discards.
    if num_rects != 0 {
        return;
    }

    let Some(context) = d3d11_context1(h) else {
        return;
    };
    match handle_type {
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_RESOURCE => {
            if let Some(res) = load_resource_at(handle) {
                context.DiscardResource(&*res);
            }
        }
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_SHADERRESOURCEVIEW => {
            if let Some(view) = load_com_at::<ID3D11ShaderResourceView>(handle)
                .and_then(|v| (*v).cast::<ID3D11View>().ok())
            {
                context.DiscardView(&view);
            }
        }
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_RENDERTARGETVIEW => {
            if let Some(view) = load_rtv_at(handle).and_then(|v| (*v).cast::<ID3D11View>().ok()) {
                context.DiscardView(&view);
            }
        }
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_DEPTHSTENCILVIEW => {
            if let Some(view) = load_com_at::<ID3D11DepthStencilView>(handle)
                .and_then(|v| (*v).cast::<ID3D11View>().ok())
            {
                context.DiscardView(&view);
            }
        }
        ddi::D3D11DDI_HANDLETYPE_D3D11DDI_HT_UNORDEREDACCESSVIEW => {
            if let Some(view) = load_com_at::<ID3D11UnorderedAccessView>(handle)
                .and_then(|v| (*v).cast::<ID3D11View>().ok())
            {
                context.DiscardView(&view);
            }
        }
        _ => {}
    }
}

unsafe extern "C" fn check_direct_flip_support_11_1(
    _h: Hdevice,
    _resource1: ddi::D3D10DDI_HRESOURCE,
    _resource2: ddi::D3D10DDI_HRESOURCE,
    flags: u32,
    supported: *mut ddi::BOOL,
) {
    if !supported.is_null() {
        *supported = 0;
    }
    if D3D11_1_LOG_COUNT.first_n(64).is_some() {
        log_error!(
            "DDI D3D11.1 CheckDirectFlipSupport: flags=0x{flags:x} -> no"
        );
    }
}

unsafe extern "C" fn clear_view_11_1(
    h: Hdevice,
    view_type: ddi::D3D11DDI_HANDLETYPE,
    view: *mut c_void,
    color: *const f32,
    rects: *const ddi::D3D10_DDI_RECT,
    num_rects: u32,
) {
    if D3D11_1_LOG_COUNT.first_n(64).is_some() {
        trace_line!(
            "DDI D3D11.1 ClearView: type={view_type} rects={num_rects}"
        );
    }
    if view_type != ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_RENDERTARGETVIEW {
        log_error!(
            "DDI D3D11.1 ClearView UNSUPPORTED view type {view_type} — clear dropped"
        );
        return;
    }
    let Some(context) = d3d11_context1(h) else {
        return;
    };
    let Some(rtv) = load_rtv_at(view) else {
        return;
    };
    let Ok(view) = (*rtv).cast::<ID3D11View>() else {
        return;
    };
    let rgba = if color.is_null() {
        [0.0; 4]
    } else {
        [*color, *color.add(1), *color.add(2), *color.add(3)]
    };
    // A rect-limited ClearView clears ONLY the given sub-rects. The previous
    // ClearRenderTargetView forwarding cleared the WHOLE view: dwm's flip
    // composition issues ClearView(dirty-rect) each frame before redrawing
    // just that region, so every frame the accumulated desktop was wiped to
    // the (transparent-black) clear color and only the delta survived — the
    // all-zero presented-frame class. D3D10_DDI_RECT is layout-identical to
    // RECT; DXVK implements ID3D11DeviceContext1::ClearView incl. rects.
    if num_rects != 0 && !rects.is_null() {
        let rects = core::slice::from_raw_parts(
            rects as *const windows::Win32::Foundation::RECT,
            num_rects as usize,
        );
        context.ClearView(&view, &rgba, Some(rects));
    } else {
        context.ClearView(&view, &rgba, None);
    }
}

// --- Shaders ----------------------------------------------------------------

unsafe fn shader_code_len(code: *const u32) -> usize {
    if code.is_null() {
        return 0;
    }

    // D3D API bytecode is a DXBC container with the total size at byte offset 24.
    // Bound it the same way the SHDR arm below is bounded: the dword at offset
    // 24 is read BEFORE anything is known about the container's real size, and
    // ten call sites build a `from_raw_parts` slice out of the result. Require
    // at least the 32-byte container header and at most 1 << 20 dwords.
    if *code == u32::from_le_bytes(*b"DXBC") {
        let total = *code.add(6) as usize;
        if total < 32 || total > (1 << 20) * core::mem::size_of::<u32>() {
            log_error!(
                "DDI shader_code_len: rejecting DXBC total size {total}"
            );
            return 0;
        }
        return total;
    }

    // D3D UMD callbacks receive raw SHDR/SHEX token streams. The second DWORD
    // is the stream length in DWORDs, including the two-token shader header.
    let dwords = *code.add(1) as usize;
    if dwords < 2 || dwords > (1 << 20) {
        return 0;
    }

    dwords * core::mem::size_of::<u32>()
}

unsafe fn log_shader_code(kind: &str, code: *const u32, len: usize) {
    if code.is_null() {
        log_error!("DDI {kind}: null shader code");
        return;
    }
    let d0 = *code.add(0);
    let d1 = *code.add(1);
    let d2 = *code.add(2);
    let d3 = *code.add(3);
    let is_dxbc = d0 == u32::from_le_bytes(*b"DXBC");
    log_error!(
        "DDI {kind}: shader len={} dxbc={} tokens={:08x} {:08x} {:08x} {:08x}",
        len, is_dxbc, d0, d1, d2, d3
    );
}

unsafe extern "C" fn calc_size_shader(
    _h: Hdevice,
    _code: *const u32,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) -> u64 {
    8
}

unsafe extern "C" fn create_vertex_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_vertex_shader", code, len);
    if len == 0 {
        log_error!("DDI create_vertex_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let raw = dxvk.create_vertex_shader(bytes.as_ptr(), bytes.len());
    if raw != 0 {
        if SHADER_BIND_LOG_COUNT.first_n(128).is_some() {
            log_error!(
                "DDI create_vertex_shader ok: raw=0x{raw:x} len={len}"
            );
        }
        store_raw_com(h_shader, raw);
        // Keep the bytecode so input layouts can be created lazily (the ISGN
        // supplies the semantic names CreateInputLayout requires).
        dev.owned.ia.borrow_mut().vs_bytecode.insert(raw, bytes.to_vec());
    } else {
        log_error!("DDI create_vertex_shader failed");
    }
}

unsafe extern "C" fn create_pixel_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_pixel_shader", code, len);
    if len == 0 {
        log_error!("DDI create_pixel_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let raw = dxvk.create_pixel_shader(bytes.as_ptr(), bytes.len());
    if raw != 0 {
        if SHADER_BIND_LOG_COUNT.first_n(128).is_some() {
            log_error!(
                "DDI create_pixel_shader ok: raw=0x{raw:x} len={len}"
            );
        }
        store_raw_com(h_shader, raw);
    } else {
        log_error!("DDI create_pixel_shader failed");
    }
}

/// Flatten a >=11.1 typed signature block into the bridge wire layout:
/// [n_in, n_out, (sysval, register, mask, comptype, stream) x n_in, same x
/// n_out]. The ENTRY2 arm is the one the >=11.1 runtime fills.
unsafe fn flatten_stage_io_signatures(
    sig: *const ddi::D3D11_1DDIARG_STAGE_IO_SIGNATURES,
) -> Vec<u32> {
    let mut words = vec![0u32, 0u32];
    if sig.is_null() {
        return words;
    }
    let s = &*sig;
    let p_in = s.__bindgen_anon_1.pInputSignature;
    let p_out = s.__bindgen_anon_2.pOutputSignature;
    let n_in = if p_in.is_null() {
        0
    } else {
        s.NumInputSignatureEntries
    };
    let n_out = if p_out.is_null() {
        0
    } else {
        s.NumOutputSignatureEntries
    };
    words[0] = n_in;
    words[1] = n_out;
    for i in 0..n_in as usize {
        let e = &*p_in.add(i);
        words.extend_from_slice(&[
            e.SystemValue as u32,
            e.Register,
            e.Mask as u32,
            e.RegisterComponentType as u32,
            e.Stream as u32,
        ]);
    }
    for i in 0..n_out as usize {
        let e = &*p_out.add(i);
        words.extend_from_slice(&[
            e.SystemValue as u32,
            e.Register,
            e.Mask as u32,
            e.RegisterComponentType as u32,
            e.Stream as u32,
        ]);
    }
    words
}

/// Flatten a D3D11 tessellation signature block into the bridge wire layout:
/// [n_in, n_out, n_patch, entries...]. The D3D11 tessellation DDI uses the
/// older D3D10 signature entry shape, so component type and stream are not
/// available; pass zeros and let the bridge's DXBC signature writer use its
/// existing UNKNOWN-component fallback.
unsafe fn flatten_tess_io_signatures(
    sig: *const ddi::D3D11DDIARG_TESSELLATION_IO_SIGNATURES,
) -> Vec<u32> {
    let mut words = vec![0u32, 0u32, 0u32];
    if sig.is_null() {
        return words;
    }
    let s = &*sig;
    let p_in = s.pInputSignature;
    let p_out = s.pOutputSignature;
    let p_patch = s.pPatchConstantSignature;
    let n_in = if p_in.is_null() {
        0
    } else {
        s.NumInputSignatureEntries
    };
    let n_out = if p_out.is_null() {
        0
    } else {
        s.NumOutputSignatureEntries
    };
    let n_patch = if p_patch.is_null() {
        0
    } else {
        s.NumPatchConstantSignatureEntries
    };
    words[0] = n_in;
    words[1] = n_out;
    words[2] = n_patch;
    for i in 0..n_in as usize {
        let e = &*p_in.add(i);
        words.extend_from_slice(&[e.SystemValue as u32, e.Register, e.Mask as u32, 0, 0]);
    }
    for i in 0..n_out as usize {
        let e = &*p_out.add(i);
        words.extend_from_slice(&[e.SystemValue as u32, e.Register, e.Mask as u32, 0, 0]);
    }
    for i in 0..n_patch as usize {
        let e = &*p_patch.add(i);
        words.extend_from_slice(&[e.SystemValue as u32, e.Register, e.Mask as u32, 0, 0]);
    }
    words
}

/// Flatten a >=11.1 tessellation signature block into the bridge wire layout:
/// [n_in, n_out, n_patch, entries...]. The 11.1 tessellation callbacks use
/// ENTRY2, so register component type and stream are available just like the
/// non-tessellation 11.1 shader creates.
unsafe fn flatten_tess_io_signatures_11_1(
    sig: *const ddi::D3D11_1DDIARG_TESSELLATION_IO_SIGNATURES,
) -> Vec<u32> {
    let mut words = vec![0u32, 0u32, 0u32];
    if sig.is_null() {
        return words;
    }
    let s = &*sig;
    let p_in = s.__bindgen_anon_1.pInputSignature;
    let p_out = s.__bindgen_anon_2.pOutputSignature;
    let p_patch = s.__bindgen_anon_3.pPatchConstantSignature;
    let n_in = if p_in.is_null() {
        0
    } else {
        s.NumInputSignatureEntries
    };
    let n_out = if p_out.is_null() {
        0
    } else {
        s.NumOutputSignatureEntries
    };
    let n_patch = if p_patch.is_null() {
        0
    } else {
        s.NumPatchConstantSignatureEntries
    };
    words[0] = n_in;
    words[1] = n_out;
    words[2] = n_patch;
    for i in 0..n_in as usize {
        let e = &*p_in.add(i);
        words.extend_from_slice(&[
            e.SystemValue as u32,
            e.Register,
            e.Mask as u32,
            e.RegisterComponentType as u32,
            e.Stream as u32,
        ]);
    }
    for i in 0..n_out as usize {
        let e = &*p_out.add(i);
        words.extend_from_slice(&[
            e.SystemValue as u32,
            e.Register,
            e.Mask as u32,
            e.RegisterComponentType as u32,
            e.Stream as u32,
        ]);
    }
    for i in 0..n_patch as usize {
        let e = &*p_patch.add(i);
        words.extend_from_slice(&[
            e.SystemValue as u32,
            e.Register,
            e.Mask as u32,
            e.RegisterComponentType as u32,
            e.Stream as u32,
        ]);
    }
    words
}

unsafe fn log_tess_sig_summary(name: &str, sig_words: &[u32]) {
    if sig_words.len() < 3 {
        return;
    }
    let n_in = sig_words[0] as usize;
    let n_out = sig_words[1] as usize;
    let n_patch = sig_words[2] as usize;
    let mut dump = format!("DDI {name} tess sig counts: in={n_in} out={n_out} patch={n_patch}");
    let groups = [
        ("i", 3usize, n_in),
        ("o", 3usize + n_in * 5, n_out),
        ("p", 3usize + (n_in + n_out) * 5, n_patch),
    ];
    for (tag, start, count) in groups {
        for i in 0..count.min(4) {
            let base = start + i * 5;
            if base + 2 >= sig_words.len() {
                break;
            }
            dump.push_str(&format!(
                " {tag}[r{} m0x{:x} sv{}]",
                sig_words[base + 1],
                sig_words[base + 2],
                sig_words[base]
            ));
        }
    }
    log_error!("{dump}");
}

/// Shared body for the >=11.1 typed shader creates. `kind`: 0 = vertex,
/// 1 = pixel, 2 = geometry (bridge convention). The typed signatures carry
/// the component types the raw token stream cannot express — without them
/// dxbc-spv declared every input float32 while dwm binds R16G16_SINT vertex
/// data (VUID-Input-08733 UB: garbage positions, nothing rasterized).
unsafe fn create_shader_11_1_common(
    h: Hdevice,
    kind: u32,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    sig: *const ddi::D3D11_1DDIARG_STAGE_IO_SIGNATURES,
    name: &str,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code(name, code, len);
    if len == 0 {
        log_error!("DDI {name} failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let sig_words = flatten_stage_io_signatures(sig);
    {
        // Evidence line for the Input-08733 investigation: dump each input
        // entry's (register, mask, component type) — comptype 0 (UNKNOWN)
        // falls back to float32 in the bridge, which is UB against SINT
        // vertex formats.
        let n_in = sig_words[0] as usize;
        let mut dump = format!("DDI {name} sig entries:");
        for i in 0..n_in.min(8) {
            let base = 2 + i * 5;
            dump.push_str(&format!(
                " [r{} m0x{:x} t{}]",
                sig_words[base + 1],
                sig_words[base + 2],
                sig_words[base + 3]
            ));
        }
        log_error!("{dump}");
    }
    let raw = dxvk.create_shader_sig(
        kind,
        bytes.as_ptr(),
        bytes.len(),
        sig_words.as_ptr(),
        sig_words.len(),
    );
    if raw != 0 {
        if SHADER_BIND_LOG_COUNT.first_n(128).is_some() {
            log_error!(
                "DDI {name} ok: raw=0x{raw:x} len={len} sig_in={} sig_out={}",
                sig_words[0], sig_words[1]
            );
        }
        store_raw_com(h_shader, raw);
        if kind == 0 {
            // Keep the bytecode so input layouts can be created lazily, and
            // the signature words so input-class variants can be recompiled
            // against the bound layout (resolve_vs_input_variant).
            let mut ia = dev.owned.ia.borrow_mut();
            ia.vs_bytecode.insert(raw, bytes.to_vec());
            ia.vs_sig_words.insert(raw, sig_words);
        }
    } else {
        log_error!("DDI {name} failed");
    }
}

unsafe extern "C" fn create_vertex_shader_11_1(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    sig: *const ddi::D3D11_1DDIARG_STAGE_IO_SIGNATURES,
) {
    create_shader_11_1_common(h, 0, code, h_shader, sig, "create_vertex_shader_11_1");
}

unsafe extern "C" fn create_pixel_shader_11_1(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    sig: *const ddi::D3D11_1DDIARG_STAGE_IO_SIGNATURES,
) {
    create_shader_11_1_common(h, 1, code, h_shader, sig, "create_pixel_shader_11_1");
}

unsafe extern "C" fn create_geometry_shader_11_1(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    sig: *const ddi::D3D11_1DDIARG_STAGE_IO_SIGNATURES,
) {
    create_shader_11_1_common(h, 2, code, h_shader, sig, "create_geometry_shader_11_1");
}

unsafe extern "C" fn create_geometry_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_geometry_shader", code, len);
    if len == 0 {
        log_error!("DDI create_geometry_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let raw = dxvk.create_geometry_shader(bytes.as_ptr(), bytes.len());
    if raw != 0 {
        store_raw_com(h_shader, raw);
    } else {
        log_error!("DDI create_geometry_shader failed");
    }
}

unsafe extern "C" fn calc_size_geometry_shader_so(
    _h: Hdevice,
    _arg: *const ddi::D3D11DDIARG_CREATEGEOMETRYSHADERWITHSTREAMOUTPUT,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) -> u64 {
    8
}

unsafe extern "C" fn create_geometry_shader_so(
    h: Hdevice,
    arg: *const ddi::D3D11DDIARG_CREATEGEOMETRYSHADERWITHSTREAMOUTPUT,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) {
    clear_handle(h_shader);
    if arg.is_null() {
        return;
    }
    let Some(dev) = helios_device(h) else {
        return;
    };
    let a = &*arg;
    let len = shader_code_len(a.pShaderCode);
    log_shader_code("create_geometry_shader_so", a.pShaderCode, len);
    if len == 0 {
        log_error!("DDI create_geometry_shader_so failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(a.pShaderCode as *const u8, len);
    // Stream-output declarations need semantic names that are not present in the
    // compact DDI declaration. Create a plain GS for now; DWM's composition path
    // should not depend on SO capture.
    let dxvk = &dev.dxvk;
    let raw = dxvk.create_geometry_shader(bytes.as_ptr(), bytes.len());
    if raw != 0 {
        store_raw_com(h_shader, raw);
    } else {
        log_error!("DDI create_geometry_shader_so failed");
    }
}

unsafe extern "C" fn calc_size_tess_shader(
    _h: Hdevice,
    _code: *const u32,
    _sig: *const ddi::D3D11DDIARG_TESSELLATION_IO_SIGNATURES,
) -> u64 {
    8
}

unsafe extern "C" fn calc_size_tess_shader_11_1(
    _h: Hdevice,
    _code: *const u32,
    _sig: *const ddi::D3D11_1DDIARG_TESSELLATION_IO_SIGNATURES,
) -> u64 {
    8
}

unsafe extern "C" fn create_hull_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    sig: *const ddi::D3D11DDIARG_TESSELLATION_IO_SIGNATURES,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_hull_shader", code, len);
    if len == 0 {
        log_error!("DDI create_hull_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let sig_words = flatten_tess_io_signatures(sig);
    log_tess_sig_summary("create_hull_shader", &sig_words);
    let mut raw = dxvk.create_tess_shader_sig(
        0,
        bytes.as_ptr(),
        bytes.len(),
        sig_words.as_ptr(),
        sig_words.len(),
    );
    if raw == 0 {
        log_error!("DDI create_hull_shader signature path failed; falling back to raw bytecode");
        raw = dxvk.create_hull_shader(bytes.as_ptr(), bytes.len());
    }
    if raw != 0 {
        store_raw_com(h_shader, raw);
    } else {
        log_error!("DDI create_hull_shader failed");
    }
}

unsafe extern "C" fn create_hull_shader_11_1(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    sig: *const ddi::D3D11_1DDIARG_TESSELLATION_IO_SIGNATURES,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_hull_shader_11_1", code, len);
    if len == 0 {
        log_error!("DDI create_hull_shader_11_1 failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let sig_words = flatten_tess_io_signatures_11_1(sig);
    log_tess_sig_summary("create_hull_shader_11_1", &sig_words);
    let mut raw = dxvk.create_tess_shader_sig(
        0,
        bytes.as_ptr(),
        bytes.len(),
        sig_words.as_ptr(),
        sig_words.len(),
    );
    if raw == 0 {
        log_error!("DDI create_hull_shader_11_1 signature path failed; falling back to raw bytecode");
        raw = dxvk.create_hull_shader(bytes.as_ptr(), bytes.len());
    }
    if raw != 0 {
        store_raw_com(h_shader, raw);
    } else {
        log_error!("DDI create_hull_shader_11_1 failed");
    }
}

unsafe extern "C" fn create_domain_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    sig: *const ddi::D3D11DDIARG_TESSELLATION_IO_SIGNATURES,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_domain_shader", code, len);
    if len == 0 {
        log_error!("DDI create_domain_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let sig_words = flatten_tess_io_signatures(sig);
    log_tess_sig_summary("create_domain_shader", &sig_words);
    let mut raw = dxvk.create_tess_shader_sig(
        1,
        bytes.as_ptr(),
        bytes.len(),
        sig_words.as_ptr(),
        sig_words.len(),
    );
    if raw == 0 {
        log_error!("DDI create_domain_shader signature path failed; falling back to raw bytecode");
        raw = dxvk.create_domain_shader(bytes.as_ptr(), bytes.len());
    }
    if raw != 0 {
        store_raw_com(h_shader, raw);
    } else {
        log_error!("DDI create_domain_shader failed");
    }
}

unsafe extern "C" fn create_domain_shader_11_1(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    sig: *const ddi::D3D11_1DDIARG_TESSELLATION_IO_SIGNATURES,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_domain_shader_11_1", code, len);
    if len == 0 {
        log_error!("DDI create_domain_shader_11_1 failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let sig_words = flatten_tess_io_signatures_11_1(sig);
    log_tess_sig_summary("create_domain_shader_11_1", &sig_words);
    let mut raw = dxvk.create_tess_shader_sig(
        1,
        bytes.as_ptr(),
        bytes.len(),
        sig_words.as_ptr(),
        sig_words.len(),
    );
    if raw == 0 {
        log_error!(
            "DDI create_domain_shader_11_1 signature path failed; falling back to raw bytecode"
        );
        raw = dxvk.create_domain_shader(bytes.as_ptr(), bytes.len());
    }
    if raw != 0 {
        store_raw_com(h_shader, raw);
    } else {
        log_error!("DDI create_domain_shader_11_1 failed");
    }
}

unsafe extern "C" fn create_compute_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_compute_shader", code, len);
    if len == 0 {
        log_error!("DDI create_compute_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let raw = dxvk.create_compute_shader(bytes.as_ptr(), bytes.len());
    if raw != 0 {
        store_raw_com(h_shader, raw);
    } else {
        log_error!("DDI create_compute_shader failed");
    }
}

unsafe extern "C" fn destroy_shader(h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    let raw = handle_com_raw(h_shader);
    if raw != 0 {
        if let Some(dev) = helios_device(h) {
            let mut owned = std::collections::HashSet::new();
            let was_vertex_shader = {
                let mut ia = dev.owned.ia.borrow_mut();
                let had_bytecode = ia.vs_bytecode.remove(&raw).is_some();
                let had_signature = ia.vs_sig_words.remove(&raw).is_some();
                let was_vertex_shader = had_bytecode || had_signature;
                if was_vertex_shader {
                    ia.vs_variants.retain(|&(vs, _), variant| {
                        if vs == raw {
                            if *variant != 0 {
                                owned.insert(*variant);
                            }
                            false
                        } else {
                            true
                        }
                    });
                    ia.layout_cache.retain(|&(_, vs), layout| {
                        if vs == raw {
                            if *layout != 0 {
                                owned.insert(*layout);
                            }
                            false
                        } else {
                            true
                        }
                    });
                    if ia.current_vs == raw {
                        ia.current_vs = 0;
                    }
                    if ia.bound_vs_com == raw || owned.contains(&ia.bound_vs_com) {
                        ia.bound_vs_com = 0;
                    }
                }
                was_vertex_shader
            };
            for cached in &owned {
                // SAFETY: the cache owns the COM reference returned by its
                // Create* operation. Removing the entry transfers that one
                // reference here for release.
                drop(IUnknown::from_raw(*cached as *mut c_void));
            }
            if was_vertex_shader {
                trace_line!(
                    "DDI DestroyShader: VS raw=0x{:x} released_cached={}",
                    raw,
                    owned.len()
                );
            }
        }
    }
    release_com(h_shader);
}

unsafe extern "C" fn vs_set_shader(h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    let com = handle_com_raw(h_shader);
    if let Some(dev) = helios_device(h) {
        let mut ia = dev.owned.ia.borrow_mut();
        ia.current_vs = com;
        ia.bound_vs_com = com;
    }
    if SHADER_SET_LOG_COUNT.first_n(512).is_some() {
        trace_line!("DDI VSSetShader raw=0x{com:x}");
    }
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11VertexShader>(h_shader) {
        Some(s) => context.VSSetShader(&*s, None),
        None => context.VSSetShader(None, None),
    }
}

unsafe extern "C" fn ps_set_shader(h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    let com = handle_com_raw(h_shader);
    if let Some(dev) = helios_device(h) {
        dev.owned.ia.borrow_mut().current_ps = com;
    }
    if SHADER_SET_LOG_COUNT.first_n(512).is_some() {
        trace_line!("DDI PSSetShader raw=0x{com:x}");
    }
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11PixelShader>(h_shader) {
        Some(s) => context.PSSetShader(&*s, None),
        None => context.PSSetShader(None, None),
    }
}

unsafe extern "C" fn gs_set_shader(h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    let com = handle_com_raw(h_shader);
    if let Some(dev) = helios_device(h) {
        dev.owned.ia.borrow_mut().current_gs = com;
    }
    if SHADER_SET_LOG_COUNT.first_n(512).is_some() {
        trace_line!("DDI GSSetShader raw=0x{com:x}");
    }
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11GeometryShader>(h_shader) {
        Some(s) => context.GSSetShader(&*s, None),
        None => context.GSSetShader(None, None),
    }
}

unsafe extern "C" fn hs_set_shader(h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    let com = handle_com_raw(h_shader);
    if let Some(dev) = helios_device(h) {
        dev.owned.ia.borrow_mut().current_hs = com;
    }
    if SHADER_SET_LOG_COUNT.first_n(512).is_some() {
        trace_line!("DDI HSSetShader raw=0x{com:x}");
    }
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11HullShader>(h_shader) {
        Some(s) => context.HSSetShader(&*s, None),
        None => context.HSSetShader(None, None),
    }
}

unsafe extern "C" fn ds_set_shader(h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    let com = handle_com_raw(h_shader);
    if let Some(dev) = helios_device(h) {
        dev.owned.ia.borrow_mut().current_ds = com;
    }
    if SHADER_SET_LOG_COUNT.first_n(512).is_some() {
        trace_line!("DDI DSSetShader raw=0x{com:x}");
    }
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11DomainShader>(h_shader) {
        Some(s) => context.DSSetShader(&*s, None),
        None => context.DSSetShader(None, None),
    }
}

unsafe extern "C" fn cs_set_shader(h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    let com = handle_com_raw(h_shader);
    if let Some(dev) = helios_device(h) {
        dev.owned.ia.borrow_mut().current_cs = com;
    }
    if SHADER_SET_LOG_COUNT.first_n(512).is_some() {
        trace_line!("DDI CSSetShader raw=0x{com:x}");
    }
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11ComputeShader>(h_shader) {
        Some(s) => context.CSSetShader(&*s, None),
        None => context.CSSetShader(None, None),
    }
}

unsafe extern "C" fn ps_set_shader_with_ifaces(
    h: Hdevice,
    h_shader: ddi::D3D10DDI_HSHADER,
    _num_class_instances: u32,
    _class_instance_ids: *const u32,
    _pointer_data: *const ddi::D3D11DDIARG_POINTERDATA,
) {
    ps_set_shader(h, h_shader);
}

unsafe extern "C" fn vs_set_shader_with_ifaces(
    h: Hdevice,
    h_shader: ddi::D3D10DDI_HSHADER,
    _num_class_instances: u32,
    _class_instance_ids: *const u32,
    _pointer_data: *const ddi::D3D11DDIARG_POINTERDATA,
) {
    vs_set_shader(h, h_shader);
}

unsafe extern "C" fn gs_set_shader_with_ifaces(
    h: Hdevice,
    h_shader: ddi::D3D10DDI_HSHADER,
    _num_class_instances: u32,
    _class_instance_ids: *const u32,
    _pointer_data: *const ddi::D3D11DDIARG_POINTERDATA,
) {
    gs_set_shader(h, h_shader);
}

unsafe extern "C" fn hs_set_shader_with_ifaces(
    h: Hdevice,
    h_shader: ddi::D3D10DDI_HSHADER,
    _num_class_instances: u32,
    _class_instance_ids: *const u32,
    _pointer_data: *const ddi::D3D11DDIARG_POINTERDATA,
) {
    hs_set_shader(h, h_shader);
}

unsafe extern "C" fn ds_set_shader_with_ifaces(
    h: Hdevice,
    h_shader: ddi::D3D10DDI_HSHADER,
    _num_class_instances: u32,
    _class_instance_ids: *const u32,
    _pointer_data: *const ddi::D3D11DDIARG_POINTERDATA,
) {
    ds_set_shader(h, h_shader);
}

unsafe extern "C" fn cs_set_shader_with_ifaces(
    h: Hdevice,
    h_shader: ddi::D3D10DDI_HSHADER,
    _num_class_instances: u32,
    _class_instance_ids: *const u32,
    _pointer_data: *const ddi::D3D11DDIARG_POINTERDATA,
) {
    cs_set_shader(h, h_shader);
}

// --- Output-merger / rasterizer state setters -------------------------------

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn set_render_targets(
    h: Hdevice,
    rtvs: *const ddi::D3D10DDI_HRENDERTARGETVIEW,
    num_views: u32,
    _clear_slots: u32,
    dsv: ddi::D3D10DDI_HDEPTHSTENCILVIEW,
    uavs: *const ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    uav_counts: *const u32,
    uav_start: u32,
    num_uavs: u32,
    uav_range_start: u32,
    uav_range_size: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    // The RTV array was dereferenced unconditionally in the same function that
    // checks `uavs.is_null()` and logs a `uavs_null=` field.
    let rtv_slice = DdiSlice::new(rtvs, num_views);
    let mut views: Vec<Option<ID3D11RenderTargetView>> = Vec::with_capacity(num_views as usize);
    let mut rt0 = (0, 0, 0, 0, 0);
    let mut rt_nonnull = 0u32;
    let mut rt_missing = 0u32;
    for i in 0..num_views as usize {
        // An absent slot is spelled as a null handle rather than a null
        // pointer, so the readers below stay on the typed accessors.
        let h_rtv = rtv_slice.as_ref().and_then(|s| s.get(i)).copied().unwrap_or(
            ddi::D3D10DDI_HRENDERTARGETVIEW {
                pDrvPrivate: core::ptr::null_mut(),
            },
        );
        if i == 0 {
            rt0 = rtv_info(h_rtv);
        }
        let view = load_rtv(h_rtv).map(|m| (*m).clone());
        if view.is_some() {
            rt_nonnull += 1;
        } else if !h_rtv.pDrvPrivate.is_null() {
            rt_missing += 1;
        }
        views.push(view);
    }
    if let Some(dev) = helios_device(h) {
        let mut ia = dev.owned.ia.borrow_mut();
        ia.current_rt0_alloc = rt0.0;
        ia.current_rt0_width = rt0.1;
        ia.current_rt0_height = rt0.2;
        ia.current_rt0_format = rt0.3;
    }
    unsafe { track_dwm_composition_target(h, rt0.4, rt0.0, rt0.1, rt0.2, rt0.3) };
    let n = OM_LOG_COUNT.next();
    if n < 1024 || rt_missing != 0 || rt0.0 != 0 {
        trace_line!(
            "DDI OMSetRenderTargets num={} rt_nonnull={} rt_missing={} rt0_alloc=0x{:x} rt0={}x{} fmt={} dsv_raw=0x{:x} uav_start={} num_uavs={} uav_range={}:{}",
            num_views,
            rt_nonnull,
            rt_missing,
            rt0.0,
            rt0.1,
            rt0.2,
            rt0.3,
            handle_com_raw(dsv),
            uav_start,
            num_uavs,
            uav_range_start,
            uav_range_size
        );
    }
    let depth = load_com::<ID3D11DepthStencilView>(dsv).map(|m| (*m).clone());
    if num_uavs != 0 {
        let uav_slice = DdiSlice::new(uavs, num_uavs);
        let mut uav_views: Vec<Option<ID3D11UnorderedAccessView>> =
            Vec::with_capacity(num_uavs as usize);
        let mut uav_nonnull = 0u32;
        let mut uav_missing = 0u32;
        for i in 0..num_uavs as usize {
            uav_views.push(match uav_slice.as_ref().and_then(|s| s.get(i)) {
                Some(handle) => {
                    let p = handle.pDrvPrivate;
                    let view = load_com::<ID3D11UnorderedAccessView>(*handle).map(|m| (*m).clone());
                    if view.is_some() {
                        uav_nonnull += 1;
                    } else if !p.is_null() {
                        uav_missing += 1;
                    }
                    view
                }
                None => None,
            });
        }
        if n < 1024 || uav_missing != 0 || uav_slice.is_none() {
            trace_line!(
                "DDI OMSetRenderTargets UAV summary start={} num={} nonnull={} missing={} uavs_null={} counts_ptr={}",
                uav_start,
                num_uavs,
                uav_nonnull,
                uav_missing,
                uav_slice.is_none(),
                !uav_counts.is_null()
            );
        }
        context.OMSetRenderTargetsAndUnorderedAccessViews(
            Some(&views),
            depth.as_ref(),
            uav_start,
            num_uavs,
            Some(uav_views.as_ptr()),
            if uav_counts.is_null() {
                None
            } else {
                Some(uav_counts)
            },
        );
    } else {
        context.OMSetRenderTargets(Some(&views), depth.as_ref());
    }
}

unsafe extern "C" fn set_viewports(
    h: Hdevice,
    num: u32,
    _clear: u32,
    vps: *const ddi::D3D10_DDI_VIEWPORT,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let mut out: Vec<D3D11_VIEWPORT> = Vec::with_capacity(num as usize);
    for i in 0..num as usize {
        let v = &*vps.add(i);
        out.push(D3D11_VIEWPORT {
            TopLeftX: v.TopLeftX as f32,
            TopLeftY: v.TopLeftY as f32,
            Width: v.Width as f32,
            Height: v.Height as f32,
            MinDepth: v.MinDepth,
            MaxDepth: v.MaxDepth,
        });
    }
    let n = VIEWPORT_LOG_COUNT.next();
    if n < 64 || num == 0 {
        if let Some(v) = out.first() {
            trace_line!(
                "DDI RSSetViewports num={} clear={} first=({},{} {}x{} depth={:.3}..{:.3})",
                num, _clear, v.TopLeftX, v.TopLeftY, v.Width, v.Height, v.MinDepth, v.MaxDepth
            );
        } else {
            trace_line!(
                "DDI RSSetViewports num={} clear={} empty",
                num, _clear
            );
        }
    }
    context.RSSetViewports(Some(&out));
}

unsafe extern "C" fn set_scissor_rects(
    h: Hdevice,
    num: u32,
    _clear: u32,
    rects: *const ddi::D3D10_DDI_RECT,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let mut out: Vec<RECT> = Vec::with_capacity(num as usize);
    if !rects.is_null() {
        for i in 0..num as usize {
            let r = &*rects.add(i);
            out.push(RECT {
                left: r.left,
                top: r.top,
                right: r.right,
                bottom: r.bottom,
            });
        }
    }
    let n = SCISSOR_LOG_COUNT.next();
    if n < 64 || num == 0 {
        if let Some(r) = out.first() {
            trace_line!(
                "DDI RSSetScissorRects num={} clear={} first=({},{}-{}, {})",
                num, _clear, r.left, r.top, r.right, r.bottom
            );
        } else {
            trace_line!(
                "DDI RSSetScissorRects num={} clear={} empty rects_null={}",
                num,
                _clear,
                rects.is_null()
            );
        }
    }
    context.RSSetScissorRects(Some(&out));
}

unsafe extern "C" fn set_text_filter_size(_h: Hdevice, _w: u32, _hgt: u32) {}

unsafe extern "C" fn ia_set_topology(h: Hdevice, topo: ddi::D3D10_DDI_PRIMITIVE_TOPOLOGY) {
    if let Some(dev) = helios_device(h) {
        dev.owned.ia.borrow_mut().current_topology = topo as u32;
    }
    if IA_BIND_LOG_COUNT.first_n(64).is_some() {
        trace_line!("DDI IASetTopology topo={}", topo as u32);
    }
    if let Some(context) = d3d11_context(h) {
        context.IASetPrimitiveTopology(windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY(
            topo as i32,
        ));
    }
}

unsafe fn log_draw_state(
    h: Hdevice,
    kind: &str,
    count0: u32,
    start0: u32,
    count1: u32,
    start1: u32,
) {
    // Gate the WHOLE function, not just the write: this runs from all seven
    // draw entry points, and with tracing off it used to pay an atomic
    // fetch_add on every draw plus a `dev.owned.ia.borrow()` and a heap-allocating
    // 21-argument `format!` on the first 1024 draws and every 1024th after —
    // through the unconditional mutex-serialised writer. A game issuing 2000
    // draws per frame at 60 fps meant ~120 file writes per second from the draw
    // path after warm-up, and 1024 back-to-back writes during the first frames,
    // which is exactly the startup window idle-to-active latency is measured in.
    if !crate::trace_enabled() {
        return;
    }
    let n = DRAW_LOG_COUNT.next();
    if n >= 1024 && (n % 1024) != 0 {
        return;
    }
    let Some(dev) = helios_device(h) else {
        return;
    };
    let ia = dev.owned.ia.borrow();
    trace_line!(
        "DDI {kind}: a={} b={} c={} d={} topo={} vb0=0x{:x}/{}+{} ib=0x{:x}/fmt{}+{} vs=0x{:x} ps=0x{:x} gs=0x{:x} hs=0x{:x} ds=0x{:x} rt0_alloc=0x{:x} rt0={}x{} fmt={} layout=0x{:x}",
        count0,
        start0,
        count1,
        start1,
        ia.current_topology,
        ia.current_vb0,
        ia.current_vb0_stride,
        ia.current_vb0_offset,
        ia.current_ib,
        ia.current_ib_format,
        ia.current_ib_offset,
        ia.current_vs,
        ia.current_ps,
        ia.current_gs,
        ia.current_hs,
        ia.current_ds,
        ia.current_rt0_alloc,
        ia.current_rt0_width,
        ia.current_rt0_height,
        ia.current_rt0_format,
        ia.current_layout
    );
}

unsafe extern "C" fn draw(h: Hdevice, vertex_count: u32, start_vertex: u32) {
    bind_input_layout(h);
    log_draw_state(h, "Draw", vertex_count, start_vertex, 0, 0);
    if let Some(context) = d3d11_context(h) {
        context.Draw(vertex_count, start_vertex);
    }
}

unsafe extern "C" fn draw_indexed(
    h: Hdevice,
    index_count: u32,
    start_index: u32,
    base_vertex: i32,
) {
    bind_input_layout(h);
    log_draw_state(
        h,
        "DrawIndexed",
        index_count,
        start_index,
        base_vertex as u32,
        0,
    );
    if let Some(context) = d3d11_context(h) {
        context.DrawIndexed(index_count, start_index, base_vertex);
    }
}

unsafe extern "C" fn draw_instanced(
    h: Hdevice,
    vertex_count_per_instance: u32,
    instance_count: u32,
    start_vertex: u32,
    start_instance: u32,
) {
    bind_input_layout(h);
    log_draw_state(
        h,
        "DrawInstanced",
        vertex_count_per_instance,
        start_vertex,
        instance_count,
        start_instance,
    );
    if let Some(context) = d3d11_context(h) {
        context.DrawInstanced(
            vertex_count_per_instance,
            instance_count,
            start_vertex,
            start_instance,
        );
    }
}

unsafe extern "C" fn draw_indexed_instanced(
    h: Hdevice,
    index_count_per_instance: u32,
    instance_count: u32,
    start_index: u32,
    base_vertex: i32,
    start_instance: u32,
) {
    bind_input_layout(h);
    log_draw_state(
        h,
        "DrawIndexedInstanced",
        index_count_per_instance,
        start_index,
        instance_count,
        start_instance,
    );
    if let Some(context) = d3d11_context(h) {
        context.DrawIndexedInstanced(
            index_count_per_instance,
            instance_count,
            start_index,
            base_vertex,
            start_instance,
        );
    }
}

unsafe extern "C" fn draw_auto(h: Hdevice) {
    bind_input_layout(h);
    log_draw_state(h, "DrawAuto", 0, 0, 0, 0);
    if let Some(context) = d3d11_context(h) {
        context.DrawAuto();
    }
}

unsafe extern "C" fn draw_instanced_indirect(
    h: Hdevice,
    h_args: ddi::D3D10DDI_HRESOURCE,
    aligned_byte_offset: u32,
) {
    bind_input_layout(h);
    log_draw_state(h, "DrawInstancedIndirect", aligned_byte_offset, 0, 0, 0);
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let (alloc, kind, width, height, depth, fmt) = resource_summary(h_args);
    let Some(res) = load_resource(h_args) else {
        log_error!(
            "DDI DrawInstancedIndirect skipped: args missing alloc=0x{alloc:x} offset={aligned_byte_offset}"
        );
        return;
    };
    let Ok(buf) = (*res).cast::<ID3D11Buffer>() else {
        log_error!(
            "DDI DrawInstancedIndirect skipped: args not buffer kind={kind} dims={}x{}x{} fmt={} alloc=0x{alloc:x} offset={aligned_byte_offset}",
            width, height, depth, fmt
        );
        return;
    };
    if DRAW_LOG_COUNT.first_n(2048).is_some() {
        trace_line!(
            "DDI DrawInstancedIndirect args: alloc=0x{alloc:x} bytes={width} offset={aligned_byte_offset}"
        );
    }
    context.DrawInstancedIndirect(&buf, aligned_byte_offset);
}

unsafe extern "C" fn draw_indexed_instanced_indirect(
    h: Hdevice,
    h_args: ddi::D3D10DDI_HRESOURCE,
    aligned_byte_offset: u32,
) {
    bind_input_layout(h);
    log_draw_state(
        h,
        "DrawIndexedInstancedIndirect",
        aligned_byte_offset,
        0,
        0,
        0,
    );
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let (alloc, kind, width, height, depth, fmt) = resource_summary(h_args);
    let Some(res) = load_resource(h_args) else {
        log_error!(
            "DDI DrawIndexedInstancedIndirect skipped: args missing alloc=0x{alloc:x} offset={aligned_byte_offset}"
        );
        return;
    };
    let Ok(buf) = (*res).cast::<ID3D11Buffer>() else {
        log_error!(
            "DDI DrawIndexedInstancedIndirect skipped: args not buffer kind={kind} dims={}x{}x{} fmt={} alloc=0x{alloc:x} offset={aligned_byte_offset}",
            width, height, depth, fmt
        );
        return;
    };
    if DRAW_LOG_COUNT.first_n(2048).is_some() {
        trace_line!(
            "DDI DrawIndexedInstancedIndirect args: alloc=0x{alloc:x} bytes={width} offset={aligned_byte_offset}"
        );
    }
    context.DrawIndexedInstancedIndirect(&buf, aligned_byte_offset);
}

unsafe extern "C" fn so_set_targets(
    h: Hdevice,
    num: u32,
    _clear: u32,
    buffers: *const ddi::D3D10DDI_HRESOURCE,
    offsets: *const u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    // Was: skip the fill loop for a null array but still pass `num` and
    // `out.as_ptr()`. `out.len()` was then 0 with `out.capacity() == num`, so
    // DXVK read `num` uninitialised words as ID3D11Buffer* and AddRef'd them.
    // `offsets` was passed as `Some(..)` with no null check while every sibling
    // path checked theirs.
    let out = collect_slots(DdiSlice::new(buffers, num), num, |handle| {
        load_resource(*handle).and_then(|r| (*r).cast::<ID3D11Buffer>().ok())
    });
    context.SOSetTargets(
        num,
        Some(out.as_ptr()),
        if offsets.is_null() { None } else { Some(offsets) },
    );
}

// --- Rasterizer / depth-stencil state ---------------------------------------

unsafe extern "C" fn calc_size_raster(
    _h: Hdevice,
    _d: *const ddi::D3D10_DDI_RASTERIZER_DESC,
) -> u64 {
    8
}
unsafe extern "C" fn calc_size_depth(
    _h: Hdevice,
    _d: *const ddi::D3D10_DDI_DEPTH_STENCIL_DESC,
) -> u64 {
    8
}

unsafe extern "C" fn create_rasterizer_state(
    h: Hdevice,
    desc: *const ddi::D3D10_DDI_RASTERIZER_DESC,
    h_rs: ddi::D3D10DDI_HRASTERIZERSTATE,
    _hrt: ddi::D3D10DDI_HRTRASTERIZERSTATE,
) {
    clear_handle(h_rs);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let d = &*desc;
    let rd = D3D11_RASTERIZER_DESC {
        FillMode: D3D11_FILL_MODE(d.FillMode),
        CullMode: D3D11_CULL_MODE(d.CullMode),
        FrontCounterClockwise: windows::Win32::Foundation::BOOL(d.FrontCounterClockwise),
        DepthBias: d.DepthBias,
        DepthBiasClamp: d.DepthBiasClamp,
        SlopeScaledDepthBias: d.SlopeScaledDepthBias,
        DepthClipEnable: windows::Win32::Foundation::BOOL(d.DepthClipEnable),
        ScissorEnable: windows::Win32::Foundation::BOOL(d.ScissorEnable),
        MultisampleEnable: windows::Win32::Foundation::BOOL(d.MultisampleEnable),
        AntialiasedLineEnable: windows::Win32::Foundation::BOOL(d.AntialiasedLineEnable),
    };
    if RASTER_LOG_COUNT.first_n(64).is_some() {
        log_error!(
            "DDI CreateRasterizerState fill={} cull={} front_ccw={} depth_clip={} scissor={} msaa={} aaline={} bias={} slope_bias={:.3}",
            d.FillMode,
            d.CullMode,
            d.FrontCounterClockwise,
            d.DepthClipEnable,
            d.ScissorEnable,
            d.MultisampleEnable,
            d.AntialiasedLineEnable,
            d.DepthBias,
            d.SlopeScaledDepthBias
        );
    }
    let mut rs: Option<ID3D11RasterizerState> = None;
    let created = device.CreateRasterizerState(&rd, Some(&mut rs));
    if let Err(ref e) = created {
        log_error!("DDI CreateRasterizerState failed: {e:?}");
    }
    finish_create(h, created, rs, |s| store_com(h_rs, s));
}

unsafe extern "C" fn set_rasterizer_state(h: Hdevice, h_rs: ddi::D3D10DDI_HRASTERIZERSTATE) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    if RASTER_LOG_COUNT.first_n(128).is_some() {
        trace_line!(
            "DDI RSSetState raw=0x{:x}",
            handle_com_raw(h_rs)
        );
    }
    match load_com::<ID3D11RasterizerState>(h_rs) {
        Some(s) => context.RSSetState(&*s),
        None => context.RSSetState(None),
    }
}

unsafe fn cvt_stencilop(d: &ddi::D3D10_DDI_DEPTH_STENCILOP_DESC) -> D3D11_DEPTH_STENCILOP_DESC {
    D3D11_DEPTH_STENCILOP_DESC {
        StencilFailOp: D3D11_STENCIL_OP(d.StencilFailOp),
        StencilDepthFailOp: D3D11_STENCIL_OP(d.StencilDepthFailOp),
        StencilPassOp: D3D11_STENCIL_OP(d.StencilPassOp),
        StencilFunc: D3D11_COMPARISON_FUNC(d.StencilFunc),
    }
}

unsafe extern "C" fn create_depth_stencil_state(
    h: Hdevice,
    desc: *const ddi::D3D10_DDI_DEPTH_STENCIL_DESC,
    h_ds: ddi::D3D10DDI_HDEPTHSTENCILSTATE,
    _hrt: ddi::D3D10DDI_HRTDEPTHSTENCILSTATE,
) {
    clear_handle(h_ds);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let d = &*desc;
    let dd = D3D11_DEPTH_STENCIL_DESC {
        DepthEnable: windows::Win32::Foundation::BOOL(d.DepthEnable),
        DepthWriteMask: D3D11_DEPTH_WRITE_MASK(d.DepthWriteMask),
        DepthFunc: D3D11_COMPARISON_FUNC(d.DepthFunc),
        StencilEnable: windows::Win32::Foundation::BOOL(d.StencilEnable),
        StencilReadMask: d.StencilReadMask,
        StencilWriteMask: d.StencilWriteMask,
        FrontFace: cvt_stencilop(&d.FrontFace),
        BackFace: cvt_stencilop(&d.BackFace),
    };
    let mut ds: Option<ID3D11DepthStencilState> = None;
    let created = device.CreateDepthStencilState(&dd, Some(&mut ds));
    if let Err(ref e) = created {
        log_error!("DDI CreateDepthStencilState failed: {e:?}");
    }
    finish_create(h, created, ds, |s| store_com(h_ds, s));
}

unsafe extern "C" fn set_depth_stencil_state(
    h: Hdevice,
    h_ds: ddi::D3D10DDI_HDEPTHSTENCILSTATE,
    stencil_ref: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11DepthStencilState>(h_ds) {
        Some(s) => context.OMSetDepthStencilState(&*s, stencil_ref),
        None => context.OMSetDepthStencilState(None, stencil_ref),
    }
}

unsafe extern "C" fn destroy_raster_state(_h: Hdevice, h_state: ddi::D3D10DDI_HRASTERIZERSTATE) {
    release_com(h_state);
}

// --- Shader resource views, samplers, constant buffers ----------------------

unsafe extern "C" fn calc_size_srv(
    _h: Hdevice,
    _a: *const ddi::D3D11DDIARG_CREATESHADERRESOURCEVIEW,
) -> u64 {
    8
}

unsafe extern "C" fn create_srv(
    h: Hdevice,
    arg: *const ddi::D3D11DDIARG_CREATESHADERRESOURCEVIEW,
    h_srv: ddi::D3D10DDI_HSHADERRESOURCEVIEW,
    _hrt: ddi::D3D10DDI_HRTSHADERRESOURCEVIEW,
) {
    clear_handle(h_srv);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let Some(res) = load_resource(a.hDrvResource) else {
        log_error!(
            "DDI create_srv: resource handle empty dim={} fmt={} hpriv={:p}",
            a.ResourceDimension, a.Format, a.hDrvResource.pDrvPrivate
        );
        return;
    };
    let Some(desc) = srv_desc(a, a.hDrvResource) else {
        log_error!(
            "DDI create_srv: unsupported resource dimension {} fmt={}",
            a.ResourceDimension, a.Format
        );
        return;
    };
    let mut srv: Option<ID3D11ShaderResourceView> = None;
    let created = device.CreateShaderResourceView(&*res, Some(&desc), Some(&mut srv));
    if let Err(ref e) = created {
        log_error!(
            "DDI create_srv failed: dim={} fmt={} {e:?}",
            a.ResourceDimension, a.Format
        );
    }
    finish_create(h, created, srv, |v| {
        let allocation = resource_allocation(a.hDrvResource);
        let n = SRV_CREATE_LOG_COUNT.next();
        if n < 1024 || allocation != 0 {
            let (width, height) = resource_dimensions(a.hDrvResource);
            trace_line!(
                "DDI create_srv ok: hpriv={:p} alloc=0x{:x} dim={} fmt={} {}x{}",
                h_srv.pDrvPrivate, allocation, a.ResourceDimension, a.Format, width, height
            );
        }
        store_com(h_srv, v);
    });
}

unsafe fn srv_desc(
    a: &ddi::D3D11DDIARG_CREATESHADERRESOURCEVIEW,
    h_res: ddi::D3D10DDI_HRESOURCE,
) -> Option<D3D11_SHADER_RESOURCE_VIEW_DESC> {
    let format = DXGI_FORMAT(a.Format as i32);
    match a.ResourceDimension {
        RES_BUFFER => {
            let b = a.__bindgen_anon_1.Buffer;
            Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_SRV_DIMENSION_BUFFER,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D11_BUFFER_SRV {
                        Anonymous1: D3D11_BUFFER_SRV_0 {
                            FirstElement: b.__bindgen_anon_1.FirstElement,
                        },
                        Anonymous2: D3D11_BUFFER_SRV_1 {
                            NumElements: b.__bindgen_anon_2.NumElements,
                        },
                    },
                },
            })
        }
        RES_BUFFEREX => {
            let b = a.__bindgen_anon_1.BufferEx;
            Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_SRV_DIMENSION_BUFFEREX,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    BufferEx: D3D11_BUFFEREX_SRV {
                        FirstElement: b.__bindgen_anon_1.FirstElement,
                        NumElements: b.__bindgen_anon_2.NumElements,
                        Flags: b.Flags,
                    },
                },
            })
        }
        RES_TEX1D => {
            let t = a.__bindgen_anon_1.Tex1D;
            if t.ArraySize > 1 {
                Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURE1DARRAY,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture1DArray: D3D11_TEX1D_ARRAY_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                })
            } else {
                Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURE1D,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture1D: D3D11_TEX1D_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                        },
                    },
                })
            }
        }
        RES_TEX2D => {
            let t = a.__bindgen_anon_1.Tex2D;
            let is_msaa = resource_sample_count(h_res) > 1;
            if is_msaa && t.ArraySize > 1 {
                Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2DMSARRAY,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2DMSArray: D3D11_TEX2DMS_ARRAY_SRV {
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                })
            } else if is_msaa {
                Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2DMS,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2DMS: D3D11_TEX2DMS_SRV {
                            UnusedField_NothingToDefine: 0,
                        },
                    },
                })
            } else if t.ArraySize > 1 {
                Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2DARRAY,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                })
            } else {
                Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                        },
                    },
                })
            }
        }
        RES_TEX3D => {
            let t = a.__bindgen_anon_1.Tex3D;
            Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_SRV_DIMENSION_TEXTURE3D,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture3D: D3D11_TEX3D_SRV {
                        MostDetailedMip: t.MostDetailedMip,
                        MipLevels: t.MipLevels,
                    },
                },
            })
        }
        RES_TEXCUBE => {
            let t = a.__bindgen_anon_1.TexCube;
            if t.NumCubes > 1 {
                Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURECUBEARRAY,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        TextureCubeArray: D3D11_TEXCUBE_ARRAY_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                            First2DArrayFace: t.First2DArrayFace,
                            NumCubes: t.NumCubes,
                        },
                    },
                })
            } else {
                Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURECUBE,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        TextureCube: D3D11_TEXCUBE_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                        },
                    },
                })
            }
        }
        _ => None,
    }
}

unsafe extern "C" fn destroy_srv(_h: Hdevice, h_srv: ddi::D3D10DDI_HSHADERRESOURCEVIEW) {
    release_com(h_srv);
}

unsafe extern "C" fn gen_mips(h: Hdevice, h_srv: ddi::D3D10DDI_HSHADERRESOURCEVIEW) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(srv) = load_com::<ID3D11ShaderResourceView>(h_srv) else {
        return;
    };
    context.GenerateMips(&*srv);
}

unsafe extern "C" fn calc_size_uav(
    _h: Hdevice,
    _a: *const ddi::D3D11DDIARG_CREATEUNORDEREDACCESSVIEW,
) -> u64 {
    8
}

unsafe extern "C" fn create_uav(
    h: Hdevice,
    arg: *const ddi::D3D11DDIARG_CREATEUNORDEREDACCESSVIEW,
    h_uav: ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    _hrt: ddi::D3D11DDI_HRTUNORDEREDACCESSVIEW,
) {
    clear_handle(h_uav);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let Some(res) = load_resource(a.hDrvResource) else {
        return;
    };
    let Some(desc) = uav_desc(a) else {
        log_error!(
            "DDI create_uav: unsupported resource dimension {} fmt={}",
            a.ResourceDimension, a.Format
        );
        return;
    };
    let mut uav: Option<ID3D11UnorderedAccessView> = None;
    let created = device.CreateUnorderedAccessView(&*res, Some(&desc), Some(&mut uav));
    match created {
        Ok(()) => {}
        Err(ref e) => {
            let detail = match a.ResourceDimension {
                RES_BUFFER | RES_BUFFEREX => {
                    let b = a.__bindgen_anon_1.Buffer;
                    format!(
                        "buffer first={} num={} flags=0x{:x}",
                        b.FirstElement, b.NumElements, b.Flags
                    )
                }
                RES_TEX1D => {
                    let t = a.__bindgen_anon_1.Tex1D;
                    format!(
                        "tex1d mip={} first={} array={}",
                        t.MipSlice, t.FirstArraySlice, t.ArraySize
                    )
                }
                RES_TEX2D => {
                    let t = a.__bindgen_anon_1.Tex2D;
                    format!(
                        "tex2d mip={} first={} array={}",
                        t.MipSlice, t.FirstArraySlice, t.ArraySize
                    )
                }
                RES_TEX3D => {
                    let t = a.__bindgen_anon_1.Tex3D;
                    format!(
                        "tex3d mip={} first_w={} wsize={}",
                        t.MipSlice, t.FirstW, t.WSize
                    )
                }
                _ => String::from("unknown"),
            };
            log_error!(
                "DDI create_uav failed: dim={} fmt={} {} {e:?}",
                a.ResourceDimension, a.Format, detail
            );
        }
    }
    finish_create(h, created, uav, |v| {
        if VIEW_LOG_COUNT.first_n(256).is_some() {
            trace_line!(
                "DDI create_uav ok: dim={} fmt={} alloc=0x{:x}",
                a.ResourceDimension,
                a.Format,
                resource_allocation(a.hDrvResource)
            );
        }
        store_com(h_uav, v);
    });
}

unsafe fn uav_desc(
    a: &ddi::D3D11DDIARG_CREATEUNORDEREDACCESSVIEW,
) -> Option<D3D11_UNORDERED_ACCESS_VIEW_DESC> {
    let format = DXGI_FORMAT(a.Format as i32);
    match a.ResourceDimension {
        RES_BUFFER | RES_BUFFEREX => {
            let b = a.__bindgen_anon_1.Buffer;
            Some(D3D11_UNORDERED_ACCESS_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
                Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Buffer: D3D11_BUFFER_UAV {
                        FirstElement: b.FirstElement,
                        NumElements: b.NumElements,
                        Flags: b.Flags,
                    },
                },
            })
        }
        RES_TEX1D => {
            let t = a.__bindgen_anon_1.Tex1D;
            if t.ArraySize > 1 {
                Some(D3D11_UNORDERED_ACCESS_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_UAV_DIMENSION_TEXTURE1DARRAY,
                    Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Texture1DArray: D3D11_TEX1D_ARRAY_UAV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                })
            } else {
                Some(D3D11_UNORDERED_ACCESS_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_UAV_DIMENSION_TEXTURE1D,
                    Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Texture1D: D3D11_TEX1D_UAV {
                            MipSlice: t.MipSlice,
                        },
                    },
                })
            }
        }
        RES_TEX2D => {
            let t = a.__bindgen_anon_1.Tex2D;
            if t.ArraySize > 1 {
                Some(D3D11_UNORDERED_ACCESS_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2DARRAY,
                    Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Texture2DArray: D3D11_TEX2D_ARRAY_UAV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                })
            } else {
                Some(D3D11_UNORDERED_ACCESS_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_UAV {
                            MipSlice: t.MipSlice,
                        },
                    },
                })
            }
        }
        RES_TEX3D => {
            let t = a.__bindgen_anon_1.Tex3D;
            Some(D3D11_UNORDERED_ACCESS_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_UAV_DIMENSION_TEXTURE3D,
                Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Texture3D: D3D11_TEX3D_UAV {
                        MipSlice: t.MipSlice,
                        FirstWSlice: t.FirstW,
                        WSize: t.WSize,
                    },
                },
            })
        }
        _ => None,
    }
}

unsafe extern "C" fn destroy_uav(_h: Hdevice, h_uav: ddi::D3D11DDI_HUNORDEREDACCESSVIEW) {
    release_com(h_uav);
}

unsafe extern "C" fn clear_uav_uint(
    h: Hdevice,
    h_uav: ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    values: *const u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(uav) = load_com::<ID3D11UnorderedAccessView>(h_uav) else {
        return;
    };
    let v = if values.is_null() {
        [0u32; 4]
    } else {
        [*values, *values.add(1), *values.add(2), *values.add(3)]
    };
    context.ClearUnorderedAccessViewUint(&*uav, &v);
}

unsafe extern "C" fn clear_uav_float(
    h: Hdevice,
    h_uav: ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    values: *const f32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(uav) = load_com::<ID3D11UnorderedAccessView>(h_uav) else {
        return;
    };
    let v = if values.is_null() {
        [0.0f32; 4]
    } else {
        [*values, *values.add(1), *values.add(2), *values.add(3)]
    };
    context.ClearUnorderedAccessViewFloat(&*uav, &v);
}

unsafe extern "C" fn cs_set_uavs(
    h: Hdevice,
    start: u32,
    num: u32,
    uavs: *const ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    counts: *const u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let slice = DdiSlice::new(uavs, num);
    let mut out: Vec<Option<ID3D11UnorderedAccessView>> = Vec::with_capacity(num as usize);
    let mut nonnull = 0u32;
    let mut missing = 0u32;
    for i in 0..num as usize {
        out.push(match slice.as_ref().and_then(|s| s.get(i)) {
            Some(handle) => {
                let p = handle.pDrvPrivate;
                let view = load_com::<ID3D11UnorderedAccessView>(*handle).map(|m| (*m).clone());
                if view.is_some() {
                    nonnull += 1;
                } else if !p.is_null() {
                    missing += 1;
                }
                view
            }
            None => None,
        });
    }
    let n = UAV_BIND_LOG_COUNT.next();
    if n < 1024 || missing != 0 || slice.is_none() {
        trace_line!(
            "DDI CSSetUnorderedAccessViews start={} num={} nonnull={} missing={} uavs_null={} counts_ptr={}",
            start,
            num,
            nonnull,
            missing,
            slice.is_none(),
            !counts.is_null()
        );
    }
    context.CSSetUnorderedAccessViews(
        start,
        num,
        Some(out.as_ptr()),
        if counts.is_null() { None } else { Some(counts) },
    );
}

unsafe extern "C" fn copy_structure_count(
    h: Hdevice,
    h_dst: ddi::D3D10DDI_HRESOURCE,
    dst_offset: u32,
    h_src: ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(dst) = load_resource(h_dst).and_then(|r| (*r).cast::<ID3D11Buffer>().ok())
    else {
        return;
    };
    let Some(src) = load_com::<ID3D11UnorderedAccessView>(h_src) else {
        return;
    };
    context.CopyStructureCount(&dst, dst_offset, &*src);
}

unsafe extern "C" fn calc_size_sampler(_h: Hdevice, _d: *const ddi::D3D10_DDI_SAMPLER_DESC) -> u64 {
    8
}

unsafe extern "C" fn create_sampler(
    h: Hdevice,
    desc: *const ddi::D3D10_DDI_SAMPLER_DESC,
    h_sampler: ddi::D3D10DDI_HSAMPLER,
    _hrt: ddi::D3D10DDI_HRTSAMPLER,
) {
    clear_handle(h_sampler);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let d = &*desc;
    let sd = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER(d.Filter),
        AddressU: D3D11_TEXTURE_ADDRESS_MODE(d.AddressU),
        AddressV: D3D11_TEXTURE_ADDRESS_MODE(d.AddressV),
        AddressW: D3D11_TEXTURE_ADDRESS_MODE(d.AddressW),
        MipLODBias: d.MipLODBias,
        MaxAnisotropy: d.MaxAnisotropy,
        ComparisonFunc: D3D11_COMPARISON_FUNC(d.ComparisonFunc),
        BorderColor: d.BorderColor,
        MinLOD: d.MinLOD,
        MaxLOD: d.MaxLOD,
    };
    let mut s: Option<ID3D11SamplerState> = None;
    let created = device.CreateSamplerState(&sd, Some(&mut s));
    if let Err(ref e) = created {
        log_error!("DDI CreateSamplerState failed: {e:?}");
    }
    finish_create(h, created, s, |o| store_com(h_sampler, o));
}

unsafe extern "C" fn destroy_sampler(_h: Hdevice, h_sampler: ddi::D3D10DDI_HSAMPLER) {
    release_com(h_sampler);
}

/// A runtime-supplied DDI handle array, with the null check moved into the type.
///
/// Four incompatible conventions for this exact shape used to coexist in this
/// file: push `None` per slot (correct), skip the fill but still pass `num`
/// (`so_set_targets` — DXVK then read `num` uninitialised words as COM pointers
/// and AddRef'd them), dereference unconditionally (`collect_buffers`,
/// `collect_samplers`, `set_render_targets`'s RTV loop), and early-return a
/// count (`srv_bind_summary`). `new` is the only constructor and it is the only
/// place the null test lives, so a call site cannot forget it: the raw pointer
/// is never in scope again.
struct DdiSlice<H> {
    ptr: *const H,
    len: usize,
}

impl<H> DdiSlice<H> {
    /// `None` = the runtime handed us a null array.
    ///
    /// SAFETY: the caller asserts the DDI contract that a non-null array really
    /// has `num` elements. That part is not encodable from the driver side.
    unsafe fn new(ptr: *const H, num: u32) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(DdiSlice {
                ptr,
                len: num as usize,
            })
        }
    }

    /// Panic-free element access: bounds-checked, no indexing, no `unwrap`.
    ///
    /// SAFETY: as [`DdiSlice::new`].
    unsafe fn get(&self, index: usize) -> Option<&H> {
        if index >= self.len {
            None
        } else {
            Some(&*self.ptr.add(index))
        }
    }
}

/// Decode `num` slots of a runtime handle array. A null array yields `None` for
/// every slot — that is the one policy: "no bindings", never "read `num`
/// uninitialised pointers".
unsafe fn collect_slots<H, T>(
    slice: Option<DdiSlice<H>>,
    num: u32,
    decode: impl Fn(&H) -> Option<T>,
) -> Vec<Option<T>> {
    let mut out = Vec::with_capacity(num as usize);
    for index in 0..num as usize {
        out.push(match slice.as_ref().and_then(|s| s.get(index)) {
            Some(handle) => decode(handle),
            None => None,
        });
    }
    out
}

unsafe fn collect_buffers(
    start: u32,
    num: u32,
    h: *const ddi::D3D10DDI_HRESOURCE,
) -> Vec<Option<ID3D11Buffer>> {
    let _ = start;
    collect_slots(DdiSlice::new(h, num), num, |handle| {
        load_resource(*handle).and_then(|r| (*r).cast::<ID3D11Buffer>().ok())
    })
}
unsafe fn collect_srvs(
    num: u32,
    h: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) -> Vec<Option<ID3D11ShaderResourceView>> {
    collect_slots(DdiSlice::new(h, num), num, |handle| {
        load_com::<ID3D11ShaderResourceView>(*handle).map(|m| (*m).clone())
    })
}

unsafe fn srv_bind_summary(
    num: u32,
    h: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) -> (u32, u32, usize, *mut c_void) {
    if h.is_null() {
        return (0, num, 0, core::ptr::null_mut());
    }
    let mut nonnull = 0u32;
    let mut missing = 0u32;
    let mut first_raw = 0usize;
    let mut first_priv: *mut c_void = core::ptr::null_mut();
    for i in 0..num as usize {
        let p = (*h.add(i)).pDrvPrivate;
        if p.is_null() {
            continue;
        }
        let raw = handle_com_raw_at(p);
        if raw == 0 {
            missing += 1;
            continue;
        }
        nonnull += 1;
        if first_raw == 0 {
            first_raw = raw;
            first_priv = p;
        }
    }
    (nonnull, missing, first_raw, first_priv)
}
unsafe fn collect_samplers(
    num: u32,
    h: *const ddi::D3D10DDI_HSAMPLER,
) -> Vec<Option<ID3D11SamplerState>> {
    collect_slots(DdiSlice::new(h, num), num, |handle| {
        load_com::<ID3D11SamplerState>(*handle).map(|m| (*m).clone())
    })
}

unsafe extern "C" fn ps_set_constant_buffers(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
) {
    if let Some(c) = d3d11_context(h) {
        c.PSSetConstantBuffers(start, Some(&collect_buffers(start, num, bufs)));
    }
}
unsafe extern "C" fn vs_set_constant_buffers(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
) {
    if let Some(c) = d3d11_context(h) {
        c.VSSetConstantBuffers(start, Some(&collect_buffers(start, num, bufs)));
    }
}
unsafe extern "C" fn gs_set_constant_buffers(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
) {
    if let Some(c) = d3d11_context(h) {
        c.GSSetConstantBuffers(start, Some(&collect_buffers(start, num, bufs)));
    }
}
unsafe extern "C" fn hs_set_constant_buffers(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
) {
    if let Some(c) = d3d11_context(h) {
        c.HSSetConstantBuffers(start, Some(&collect_buffers(start, num, bufs)));
    }
}
unsafe extern "C" fn ds_set_constant_buffers(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
) {
    if let Some(c) = d3d11_context(h) {
        c.DSSetConstantBuffers(start, Some(&collect_buffers(start, num, bufs)));
    }
}
unsafe extern "C" fn cs_set_constant_buffers(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
) {
    if let Some(c) = d3d11_context(h) {
        c.CSSetConstantBuffers(start, Some(&collect_buffers(start, num, bufs)));
    }
}

/// The six programmable shader stages the D3D11 DDI binds to.
///
/// Replaces a `&str` discriminator matched with a silent `_ => {}` catch-all on
/// two bind paths. All twelve callers passed correct literals, so this removes a
/// latent silent-skip class rather than fixing a live bind: a typo, or a future
/// `AS`/`MS` stage, would have bound NOTHING and left the previous stage's
/// buffers or SRVs live for the following draw, with no counter and no log.
/// R827.
#[derive(Clone, Copy)]
enum ShaderStage {
    Vs,
    Ps,
    Gs,
    Hs,
    Ds,
    Cs,
}

impl ShaderStage {
    /// Exactly the two-letter strings the log lines carried before R827, so
    /// `DDI {stage}SetShaderResources ...` and
    /// `DDI {stage}SetConstantBuffers1 ...` stay textually identical.
    fn name(self) -> &'static str {
        match self {
            Self::Vs => "VS",
            Self::Ps => "PS",
            Self::Gs => "GS",
            Self::Hs => "HS",
            Self::Ds => "DS",
            Self::Cs => "CS",
        }
    }
}

unsafe fn set_constant_buffers1_common(
    h: Hdevice,
    stage: ShaderStage,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
    first_constants: *const u32,
    num_constants: *const u32,
) {
    let Some(c) = d3d11_context1(h) else {
        return;
    };
    let buffers = collect_buffers(start, num, bufs);
    let buffers_ptr = if num == 0 {
        None
    } else {
        Some(buffers.as_ptr())
    };
    let first_ptr = if first_constants.is_null() {
        None
    } else {
        Some(first_constants)
    };
    let count_ptr = if num_constants.is_null() {
        None
    } else {
        Some(num_constants)
    };
    if SHADER_BIND_LOG_COUNT.first_n(256).is_some() {
        let first0 = if first_constants.is_null() {
            0
        } else {
            *first_constants
        };
        let count0 = if num_constants.is_null() {
            0
        } else {
            *num_constants
        };
        trace_line!(
            "DDI {}SetConstantBuffers1 start={} num={} first_ptr={} count_ptr={} first0={} count0={}",
            stage.name(), start, num, !first_constants.is_null(), !num_constants.is_null(), first0, count0
        );
    }
    // Exhaustive: adding a stage to the enum is a compile error here, not a
    // silent no-op bind.
    match stage {
        ShaderStage::Vs => c.VSSetConstantBuffers1(start, num, buffers_ptr, first_ptr, count_ptr),
        ShaderStage::Ps => c.PSSetConstantBuffers1(start, num, buffers_ptr, first_ptr, count_ptr),
        ShaderStage::Gs => c.GSSetConstantBuffers1(start, num, buffers_ptr, first_ptr, count_ptr),
        ShaderStage::Hs => c.HSSetConstantBuffers1(start, num, buffers_ptr, first_ptr, count_ptr),
        ShaderStage::Ds => c.DSSetConstantBuffers1(start, num, buffers_ptr, first_ptr, count_ptr),
        ShaderStage::Cs => c.CSSetConstantBuffers1(start, num, buffers_ptr, first_ptr, count_ptr),
    }
}

unsafe extern "C" fn vs_set_constant_buffers1(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
    first_constants: *const u32,
    num_constants: *const u32,
) {
    set_constant_buffers1_common(h, ShaderStage::Vs, start, num, bufs, first_constants, num_constants);
}

unsafe extern "C" fn ps_set_constant_buffers1(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
    first_constants: *const u32,
    num_constants: *const u32,
) {
    set_constant_buffers1_common(h, ShaderStage::Ps, start, num, bufs, first_constants, num_constants);
}

unsafe extern "C" fn gs_set_constant_buffers1(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
    first_constants: *const u32,
    num_constants: *const u32,
) {
    set_constant_buffers1_common(h, ShaderStage::Gs, start, num, bufs, first_constants, num_constants);
}

unsafe extern "C" fn hs_set_constant_buffers1(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
    first_constants: *const u32,
    num_constants: *const u32,
) {
    set_constant_buffers1_common(h, ShaderStage::Hs, start, num, bufs, first_constants, num_constants);
}

unsafe extern "C" fn ds_set_constant_buffers1(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
    first_constants: *const u32,
    num_constants: *const u32,
) {
    set_constant_buffers1_common(h, ShaderStage::Ds, start, num, bufs, first_constants, num_constants);
}

unsafe extern "C" fn cs_set_constant_buffers1(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
    first_constants: *const u32,
    num_constants: *const u32,
) {
    set_constant_buffers1_common(h, ShaderStage::Cs, start, num, bufs, first_constants, num_constants);
}

unsafe extern "C" fn ps_set_shader_resources(
    h: Hdevice,
    start: u32,
    num: u32,
    srvs: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) {
    set_shader_resources_common(h, ShaderStage::Ps, start, num, srvs);
}

unsafe fn set_shader_resources_common(
    h: Hdevice,
    stage: ShaderStage,
    start: u32,
    num: u32,
    srvs: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) {
    let Some(c) = d3d11_context(h) else {
        return;
    };
    let views = collect_srvs(num, srvs);
    let (nonnull, missing, first_raw, first_priv) = srv_bind_summary(num, srvs);
    let n = SRV_BIND_LOG_COUNT.next();
    if n < 2048 || missing != 0 {
        trace_line!(
            "DDI {}SetShaderResources start={} num={} nonnull={} missing={} first_raw=0x{:x} first_priv={:p}",
            stage.name(), start, num, nonnull, missing, first_raw, first_priv
        );
    }
    match stage {
        ShaderStage::Vs => c.VSSetShaderResources(start, Some(&views)),
        ShaderStage::Ps => c.PSSetShaderResources(start, Some(&views)),
        ShaderStage::Gs => c.GSSetShaderResources(start, Some(&views)),
        ShaderStage::Hs => c.HSSetShaderResources(start, Some(&views)),
        ShaderStage::Ds => c.DSSetShaderResources(start, Some(&views)),
        ShaderStage::Cs => c.CSSetShaderResources(start, Some(&views)),
    }
}
unsafe extern "C" fn vs_set_shader_resources(
    h: Hdevice,
    start: u32,
    num: u32,
    srvs: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) {
    set_shader_resources_common(h, ShaderStage::Vs, start, num, srvs);
}
unsafe extern "C" fn gs_set_shader_resources(
    h: Hdevice,
    start: u32,
    num: u32,
    srvs: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) {
    set_shader_resources_common(h, ShaderStage::Gs, start, num, srvs);
}
unsafe extern "C" fn hs_set_shader_resources(
    h: Hdevice,
    start: u32,
    num: u32,
    srvs: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) {
    set_shader_resources_common(h, ShaderStage::Hs, start, num, srvs);
}
unsafe extern "C" fn ds_set_shader_resources(
    h: Hdevice,
    start: u32,
    num: u32,
    srvs: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) {
    set_shader_resources_common(h, ShaderStage::Ds, start, num, srvs);
}
unsafe extern "C" fn cs_set_shader_resources(
    h: Hdevice,
    start: u32,
    num: u32,
    srvs: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) {
    set_shader_resources_common(h, ShaderStage::Cs, start, num, srvs);
}
unsafe extern "C" fn ps_set_samplers(
    h: Hdevice,
    start: u32,
    num: u32,
    samplers: *const ddi::D3D10DDI_HSAMPLER,
) {
    if let Some(c) = d3d11_context(h) {
        c.PSSetSamplers(start, Some(&collect_samplers(num, samplers)));
    }
}
unsafe extern "C" fn vs_set_samplers(
    h: Hdevice,
    start: u32,
    num: u32,
    samplers: *const ddi::D3D10DDI_HSAMPLER,
) {
    if let Some(c) = d3d11_context(h) {
        c.VSSetSamplers(start, Some(&collect_samplers(num, samplers)));
    }
}
unsafe extern "C" fn gs_set_samplers(
    h: Hdevice,
    start: u32,
    num: u32,
    samplers: *const ddi::D3D10DDI_HSAMPLER,
) {
    if let Some(c) = d3d11_context(h) {
        c.GSSetSamplers(start, Some(&collect_samplers(num, samplers)));
    }
}
unsafe extern "C" fn hs_set_samplers(
    h: Hdevice,
    start: u32,
    num: u32,
    samplers: *const ddi::D3D10DDI_HSAMPLER,
) {
    if let Some(c) = d3d11_context(h) {
        c.HSSetSamplers(start, Some(&collect_samplers(num, samplers)));
    }
}
unsafe extern "C" fn ds_set_samplers(
    h: Hdevice,
    start: u32,
    num: u32,
    samplers: *const ddi::D3D10DDI_HSAMPLER,
) {
    if let Some(c) = d3d11_context(h) {
        c.DSSetSamplers(start, Some(&collect_samplers(num, samplers)));
    }
}
unsafe extern "C" fn cs_set_samplers(
    h: Hdevice,
    start: u32,
    num: u32,
    samplers: *const ddi::D3D10DDI_HSAMPLER,
) {
    if let Some(c) = d3d11_context(h) {
        c.CSSetSamplers(start, Some(&collect_samplers(num, samplers)));
    }
}

unsafe extern "C" fn resource_update_subresource(
    h: Hdevice,
    h_res: ddi::D3D10DDI_HRESOURCE,
    subresource: u32,
    box_: *const ddi::D3D10_DDI_BOX,
    data: *const c_void,
    row_pitch: u32,
    depth_pitch: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(res) = load_resource(h_res) else {
        if HANDLE_MISS_LOG_COUNT.first_n(256).is_some() {
            log_error!(
                "DDI UpdateSubresource missing resource hpriv={:p} sub={} data={:p}",
                h_res.pDrvPrivate, subresource, data
            );
        }
        return;
    };
    // `alloc` selects the gate below, so the summary read stays out here; every
    // other operand is log-only and now lives inside it. The two
    // `read_unaligned` probes in particular are two dependent cache misses into
    // the CALLER's buffer, and they used to be paid on every BGRA/RGBA tex2d
    // update purely to produce a log field.
    let (alloc, kind, width, height, depth, fmt) = resource_summary(h_res);
    let n = UPDATE_LOG_COUNT.next();
    // DECLARED diagnostic change: the old gate's `|| alloc != 0` disjunct
    // removed the rate cap entirely for exactly the shared/primary/present
    // resources that update most often, so a steady stream of updates to a
    // WDDM-allocated texture wrote one 21-argument formatted line per call.
    // Allocation-backed updates stay observable without being unbounded, and
    // what is no longer emitted is counted rather than silently dropped.
    let rate_ok = n < 1024 || (alloc != 0 && n % 512 == 0);
    if !rate_ok {
        UPDATE_SUPPRESSED.fetch_add(1, Ordering::Relaxed);
    }
    if crate::trace_enabled() && rate_ok {
        let (rt_resource, km_resource) = resource_parent_handles(h_res);
        let (box_left, box_top, box_right, box_bottom) = if box_.is_null() {
            (
                0i32,
                0i32,
                i32::try_from(width).unwrap_or(i32::MAX),
                i32::try_from(height).unwrap_or(i32::MAX),
            )
        } else {
            let b = &*box_;
            (b.left, b.top, b.right, b.bottom)
        };
        let source_width = u32::try_from(box_right.saturating_sub(box_left)).unwrap_or(0);
        let source_height = u32::try_from(box_bottom.saturating_sub(box_top)).unwrap_or(0);
        let source_samples = if !data.is_null()
            && kind == "tex2d"
            && matches!(fmt, 28 | 87 | 88)
            && source_width != 0
            && source_height != 0
            && row_pitch >= source_width.saturating_mul(4)
        {
            let center_offset = (source_height as usize / 2)
                .saturating_mul(row_pitch as usize)
                .saturating_add((source_width as usize / 2).saturating_mul(4));
            Some((
                core::ptr::read_unaligned(data.cast::<u32>()),
                core::ptr::read_unaligned((data as *const u8).add(center_offset).cast::<u32>()),
            ))
        } else {
            None
        };
        trace_line!(
            "DDI UpdateSubresource #{} hDrv={:p} hRT={:p} hKM=0x{:x} alloc=0x{:x} \
             kind={} dims={}x{}x{} fmt={} sub={} box={},{},{},{} data={:p} \
             row_pitch={} depth_pitch={} sample0={} sample_center={}",
            n,
            h_res.pDrvPrivate,
            rt_resource,
            km_resource,
            alloc,
            kind,
            width,
            height,
            depth,
            fmt,
            subresource,
            box_left,
            box_top,
            box_right,
            box_bottom,
            data,
            row_pitch,
            depth_pitch,
            source_samples
                .map(|samples| format!("0x{:08x}", samples.0))
                .unwrap_or_else(|| "n/a".to_string()),
            source_samples
                .map(|samples| format!("0x{:08x}", samples.1))
                .unwrap_or_else(|| "n/a".to_string()),
        );
        if UPDATE_SUPPRESSED.load(Ordering::Relaxed) != 0 {
            trace_line!(
                "DDI UpdateSubresource: {} update lines suppressed by the rate cap so far",
                UPDATE_SUPPRESSED.load(Ordering::Relaxed)
            );
        }
    }
    let bx;
    let bx_ptr = if box_.is_null() {
        None
    } else {
        let b = &*box_;
        bx = D3D11_BOX {
            left: b.left as u32,
            top: b.top as u32,
            front: b.front as u32,
            right: b.right as u32,
            bottom: b.bottom as u32,
            back: b.back as u32,
        };
        Some(&bx as *const D3D11_BOX)
    };
    context.UpdateSubresource(&*res, subresource, bx_ptr, data, row_pitch, depth_pitch);
}

unsafe extern "C" fn resource_update_subresource_11_1(
    h: Hdevice,
    h_res: ddi::D3D10DDI_HRESOURCE,
    subresource: u32,
    box_: *const ddi::D3D10_DDI_BOX,
    data: *const c_void,
    row_pitch: u32,
    depth_pitch: u32,
    _copy_flags: u32,
) {
    resource_update_subresource(h, h_res, subresource, box_, data, row_pitch, depth_pitch);
}

fn tile_coord(
    coord: &ddi::D3DWDDM1_3DDI_TILED_RESOURCE_COORDINATE,
) -> D3D11_TILED_RESOURCE_COORDINATE {
    D3D11_TILED_RESOURCE_COORDINATE {
        X: coord.X,
        Y: coord.Y,
        Z: coord.Z,
        Subresource: coord.Subresource,
    }
}

fn tile_region(size: &ddi::D3DWDDM1_3DDI_TILE_REGION_SIZE) -> D3D11_TILE_REGION_SIZE {
    D3D11_TILE_REGION_SIZE {
        NumTiles: size.NumTiles,
        bUseBox: BOOL(size.bUseBox),
        Width: size.Width,
        Height: size.Height,
        Depth: size.Depth,
    }
}

unsafe fn tile_coords(
    ptr: *const ddi::D3DWDDM1_3DDI_TILED_RESOURCE_COORDINATE,
    count: u32,
) -> Vec<D3D11_TILED_RESOURCE_COORDINATE> {
    if ptr.is_null() || count == 0 {
        return Vec::new();
    }
    (0..count)
        .map(|i| tile_coord(&*ptr.add(i as usize)))
        .collect()
}

unsafe fn tile_regions(
    ptr: *const ddi::D3DWDDM1_3DDI_TILE_REGION_SIZE,
    count: u32,
) -> Vec<D3D11_TILE_REGION_SIZE> {
    if ptr.is_null() || count == 0 {
        return Vec::new();
    }
    (0..count)
        .map(|i| tile_region(&*ptr.add(i as usize)))
        .collect()
}

unsafe fn resource_as_buffer(
    h_resource: ddi::D3D10DDI_HRESOURCE,
) -> Option<ManuallyDrop<ID3D11Buffer>> {
    let res = load_resource(h_resource)?;
    (*res).cast::<ID3D11Buffer>().ok().map(ManuallyDrop::new)
}

unsafe extern "C" fn update_tile_mappings(
    h: Hdevice,
    h_tiled_resource: ddi::D3D10DDI_HRESOURCE,
    region_count: u32,
    region_start_coords: *const ddi::D3DWDDM1_3DDI_TILED_RESOURCE_COORDINATE,
    region_sizes: *const ddi::D3DWDDM1_3DDI_TILE_REGION_SIZE,
    h_tile_pool: ddi::D3D10DDI_HRESOURCE,
    range_count: u32,
    range_flags: *const u32,
    tile_pool_start_offsets: *const u32,
    range_tile_counts: *const u32,
    flags: u32,
) {
    let Some(context) = d3d11_context2(h) else {
        return;
    };
    let Some(tiled_resource) = load_resource(h_tiled_resource) else {
        return;
    };
    let coords = tile_coords(region_start_coords, region_count);
    let sizes = tile_regions(region_sizes, region_count);
    let tile_pool = resource_as_buffer(h_tile_pool);
    let tile_pool_ref: Option<&ID3D11Buffer> = tile_pool.as_ref().map(|p| &**p);
    let sizes_ptr = if sizes.is_empty() {
        None
    } else {
        Some(sizes.as_ptr())
    };
    let coords_ptr = if coords.is_empty() {
        None
    } else {
        Some(coords.as_ptr())
    };
    let _ = context.UpdateTileMappings(
        &*tiled_resource,
        region_count,
        coords_ptr,
        sizes_ptr,
        tile_pool_ref,
        range_count,
        (!range_flags.is_null()).then_some(range_flags),
        (!tile_pool_start_offsets.is_null()).then_some(tile_pool_start_offsets),
        (!range_tile_counts.is_null()).then_some(range_tile_counts),
        flags,
    );
}

unsafe extern "C" fn copy_tile_mappings(
    h: Hdevice,
    h_dst_resource: ddi::D3D10DDI_HRESOURCE,
    dst_start_coord: *const ddi::D3DWDDM1_3DDI_TILED_RESOURCE_COORDINATE,
    h_src_resource: ddi::D3D10DDI_HRESOURCE,
    src_start_coord: *const ddi::D3DWDDM1_3DDI_TILED_RESOURCE_COORDINATE,
    region_size: *const ddi::D3DWDDM1_3DDI_TILE_REGION_SIZE,
    flags: u32,
) {
    let Some(context) = d3d11_context2(h) else {
        return;
    };
    let Some(dst) = load_resource(h_dst_resource) else {
        return;
    };
    let Some(src) = load_resource(h_src_resource) else {
        return;
    };
    if dst_start_coord.is_null() || src_start_coord.is_null() || region_size.is_null() {
        return;
    }
    let dst_coord = tile_coord(&*dst_start_coord);
    let src_coord = tile_coord(&*src_start_coord);
    let size = tile_region(&*region_size);
    let _ = context.CopyTileMappings(&*dst, &dst_coord, &*src, &src_coord, &size, flags);
}

unsafe extern "C" fn copy_tiles(
    h: Hdevice,
    h_tiled_resource: ddi::D3D10DDI_HRESOURCE,
    region_start_coord: *const ddi::D3DWDDM1_3DDI_TILED_RESOURCE_COORDINATE,
    region_size: *const ddi::D3DWDDM1_3DDI_TILE_REGION_SIZE,
    h_buffer: ddi::D3D10DDI_HRESOURCE,
    buffer_start_offset: u64,
    flags: u32,
) {
    let Some(context) = d3d11_context2(h) else {
        return;
    };
    let Some(tiled_resource) = load_resource(h_tiled_resource) else {
        return;
    };
    let Some(buffer) = resource_as_buffer(h_buffer) else {
        return;
    };
    if region_start_coord.is_null() || region_size.is_null() {
        return;
    }
    let coord = tile_coord(&*region_start_coord);
    let size = tile_region(&*region_size);
    context.CopyTiles(
        &*tiled_resource,
        &coord,
        &size,
        &*buffer,
        buffer_start_offset,
        flags,
    );
}

unsafe extern "C" fn update_tiles(
    h: Hdevice,
    h_dst_resource: ddi::D3D10DDI_HRESOURCE,
    dst_start_coord: *const ddi::D3DWDDM1_3DDI_TILED_RESOURCE_COORDINATE,
    dst_region_size: *const ddi::D3DWDDM1_3DDI_TILE_REGION_SIZE,
    src_tile_data: *const c_void,
    flags: u32,
) {
    let Some(context) = d3d11_context2(h) else {
        return;
    };
    let Some(dst) = load_resource(h_dst_resource) else {
        return;
    };
    if dst_start_coord.is_null() || dst_region_size.is_null() || src_tile_data.is_null() {
        return;
    }
    let coord = tile_coord(&*dst_start_coord);
    let size = tile_region(&*dst_region_size);
    context.UpdateTiles(&*dst, &coord, &size, src_tile_data, flags);
}

unsafe fn tiled_barrier_child(
    handle_type: ddi::D3D11DDI_HANDLETYPE,
    handle: *mut c_void,
) -> Option<ID3D11DeviceChild> {
    match handle_type {
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_RESOURCE => {
            let res = load_resource_at(handle)?;
            (*res).cast::<ID3D11DeviceChild>().ok()
        }
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_SHADERRESOURCEVIEW => {
            let view = load_com_at::<ID3D11ShaderResourceView>(handle)?;
            (*view).cast::<ID3D11DeviceChild>().ok()
        }
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_RENDERTARGETVIEW => {
            let view = load_rtv_at(handle)?;
            (*view).cast::<ID3D11DeviceChild>().ok()
        }
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_DEPTHSTENCILVIEW => {
            let view = load_com_at::<ID3D11DepthStencilView>(handle)?;
            (*view).cast::<ID3D11DeviceChild>().ok()
        }
        ddi::D3D11DDI_HANDLETYPE_D3D11DDI_HT_UNORDEREDACCESSVIEW => {
            let view = load_com_at::<ID3D11UnorderedAccessView>(handle)?;
            (*view).cast::<ID3D11DeviceChild>().ok()
        }
        _ => None,
    }
}

unsafe extern "C" fn tiled_resource_barrier(
    h: Hdevice,
    before_type: ddi::D3D11DDI_HANDLETYPE,
    before: *mut c_void,
    after_type: ddi::D3D11DDI_HANDLETYPE,
    after: *mut c_void,
) {
    let Some(context) = d3d11_context2(h) else {
        return;
    };
    let before_child = tiled_barrier_child(before_type, before);
    let after_child = tiled_barrier_child(after_type, after);
    context.TiledResourceBarrier(before_child.as_ref(), after_child.as_ref());
}

unsafe extern "C" fn get_mip_packing(
    h: Hdevice,
    h_tiled_resource: ddi::D3D10DDI_HRESOURCE,
    packed_mips: *mut u32,
    tiles_for_packed_mips: *mut u32,
) {
    if !packed_mips.is_null() {
        *packed_mips = 0;
    }
    if !tiles_for_packed_mips.is_null() {
        *tiles_for_packed_mips = 0;
    }
    let Some(device) = d3d11_device2(h) else {
        return;
    };
    let Some(resource) = load_resource(h_tiled_resource) else {
        return;
    };
    let mut total_tiles = 0u32;
    let mut packed = D3D11_PACKED_MIP_DESC::default();
    let mut shape = D3D11_TILE_SHAPE::default();
    let mut subresource_count = 0u32;
    device.GetResourceTiling(
        &*resource,
        Some(&mut total_tiles),
        Some(&mut packed),
        Some(&mut shape),
        Some(&mut subresource_count),
        0,
        core::ptr::null_mut(),
    );
    if !packed_mips.is_null() {
        *packed_mips = packed.NumPackedMips as u32;
    }
    if !tiles_for_packed_mips.is_null() {
        *tiles_for_packed_mips = packed.NumTilesForPackedMips;
    }
}

unsafe extern "C" fn resize_tile_pool(
    h: Hdevice,
    h_tile_pool: ddi::D3D10DDI_HRESOURCE,
    new_size: u64,
) {
    let Some(context) = d3d11_context2(h) else {
        return;
    };
    let Some(tile_pool) = resource_as_buffer(h_tile_pool) else {
        return;
    };
    let _ = context.ResizeTilePool(&*tile_pool, new_size);
}

static WDDM13_MARKER_LOG_COUNT: LogThrottle = LogThrottle::new();

unsafe extern "C" fn set_marker(h: Hdevice) {
    if let Some(n) = WDDM13_MARKER_LOG_COUNT.first_n_then_every(16, 1024) {
        log_error!(
            "WDDM1.3 SetMarker h={:p} hit={}",
            h.pDrvPrivate,
            n + 1
        );
    }
}

unsafe extern "C" fn set_marker_mode(
    h: Hdevice,
    marker_type: ddi::D3DWDDM1_3DDI_MARKER_TYPE,
    flags: u32,
) {
    if let Some(n) = WDDM13_MARKER_LOG_COUNT.first_n_then_every(16, 1024) {
        log_error!(
            "WDDM1.3 SetMarkerMode h={:p} type={} flags=0x{:x} hit={}",
            h.pDrvPrivate,
            marker_type,
            flags,
            n + 1
        );
    }
}

// --- Queries / counters -----------------------------------------------------

unsafe extern "C" fn calc_size_query(_h: Hdevice, _a: *const ddi::D3D10DDIARG_CREATEQUERY) -> u64 {
    8
}

unsafe extern "C" fn create_query(
    h: Hdevice,
    arg: *const ddi::D3D10DDIARG_CREATEQUERY,
    h_query: ddi::D3D10DDI_HQUERY,
    _hrt: ddi::D3D10DDI_HRTQUERY,
) {
    clear_handle(h_query);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let desc = D3D11_QUERY_DESC {
        Query: D3D11_QUERY(a.Query),
        MiscFlags: a.MiscFlags,
    };
    let mut q: Option<ID3D11Query> = None;
    match device.CreateQuery(&desc, Some(&mut q)) {
        Ok(()) => {
            if let Some(query) = q {
                store_com(h_query, query);
            }
        }
        Err(e) => log_error!("DDI create_query failed: {e:?}"),
    }
}

unsafe extern "C" fn destroy_query(_h: Hdevice, h_query: ddi::D3D10DDI_HQUERY) {
    release_com(h_query);
}

unsafe extern "C" fn query_begin(h: Hdevice, h_query: ddi::D3D10DDI_HQUERY) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(q) = load_com::<ID3D11Query>(h_query) else {
        return;
    };
    if let Ok(async_) = (*q).cast::<ID3D11Asynchronous>() {
        context.Begin(&async_);
    }
}

unsafe extern "C" fn query_end(h: Hdevice, h_query: ddi::D3D10DDI_HQUERY) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(q) = load_com::<ID3D11Query>(h_query) else {
        return;
    };
    if let Ok(async_) = (*q).cast::<ID3D11Asynchronous>() {
        context.End(&async_);
    }
}

unsafe extern "C" fn query_get_data(
    h: Hdevice,
    h_query: ddi::D3D10DDI_HQUERY,
    data: *mut c_void,
    data_size: u32,
    flags: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(q) = load_com::<ID3D11Query>(h_query) else {
        return;
    };
    if let Ok(async_) = (*q).cast::<ID3D11Asynchronous>() {
        let _ = context.GetData(&async_, Some(data), data_size, flags);
    }
}

unsafe extern "C" fn set_predication(
    h: Hdevice,
    h_query: ddi::D3D10DDI_HQUERY,
    predicate_value: i32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let predicate = load_com::<ID3D11Query>(h_query)
        .and_then(|q| (*q).cast::<ID3D11Predicate>().ok());
    context.SetPredication(predicate.as_ref(), predicate_value != 0);
}

/// Shared rate cap for the three MSAA log sites (R829).
static MSAA_LOG_COUNT: LogThrottle = LogThrottle::new();

/// D3D11 multisample-quality caps, keyed off the active feature-level profile.
///
/// The Microsoft runtime validates `CheckFormatSupport` and
/// `CheckMultisampleQualityLevels` as a coherent feature-level contract during
/// `CDevice::LLOCompleteLayerConstruction`. The FL10.0 profile expresses a
/// no-multisample device (1x only) coherently with `check_format_support`
/// stripping the multisample bits. The FL11_0 profile advertises 1x, 4x, 8x and
/// the optional standard patterns (2x/16x) for EVERY output-capable format. The
/// runtime rejects arbitrary non-power-of-two sample counts.
///
/// R829 (OWNER DECISION): this doc previously claimed the D3D11.3 §19.2.5
/// exception -- 8x only for output formats *below* 128 bits/sample -- which the
/// code has never implemented. The decision was to correct the DOC, not the
/// code. §19.2.5 is a FLOOR, not a ceiling: a driver may advertise above it,
/// and the caps/quality pair stays internally coherent either way because
/// `check_format_support` uses the SAME
/// `dxgi_msaa_bits_per_sample(fmt, caps).is_some()` predicate.
///
/// What made this a decision rather than a cleanup, and worth knowing before
/// revisiting it: `dxgi_msaa_bits_per_sample` resolves to a static format table
/// plus the DXVK caps word. It never asks whether that SAMPLE COUNT is
/// supported, so today's "8x on a 128-bit format" is a table assertion, not a
/// capability probe. Implementing the floor would narrow the claim; probing
/// DXVK would make it true. Neither is done here -- both are behaviour changes
/// on the default-live FL11 caps path, which this tranche freezes.
unsafe fn helios_multisample_quality_levels(
    h: Hdevice,
    fmt: ddi::DXGI_FORMAT,
    sample_count: u32,
) -> u32 {
    if crate::feature_level_mode() != 1 {
        return if sample_count == 1 { 1 } else { 0 };
    }
    if sample_count == 0 {
        return 0;
    }
    let Some(device) = d3d11_device(h) else {
        // DXVK unreachable: fall back to the conservative single-sample answer.
        return if sample_count == 1 { 1 } else { 0 };
    };
    let caps = device
        .CheckFormatSupport(DXGI_FORMAT(fmt as i32))
        .unwrap_or(0);
    let output_bits = dxgi_msaa_bits_per_sample(fmt as u32, caps);
    // The two arms were identical -- (1|2|4|16, Some(_)) and (8, Some(_)) both
    // yielding true -- which is what made the doc's 128-bit exception look
    // implemented. Collapsed; `output_bits` is still bound for the log, which
    // is the only thing that ever consumed it.
    let required = matches!(
        (sample_count, output_bits),
        (1 | 2 | 4 | 8 | 16, Some(_))
    );
    let val = if required { 1 } else { 0 };
    // DECLARED diagnostic-volume change (R829): this site fired whenever
    // `required || sample_count <= 8`, i.e. on essentially every query, and the
    // two public wrappers below logged unconditionally with no cap at all. All
    // three now share one throttle.
    if (required || sample_count <= 8) && MSAA_LOG_COUNT.first_n_then_every(256, 4096).is_some() {
        trace_line!(
            "MSAA q fmt={fmt} c={sample_count} output_bits={output_bits:?} required={required} -> {val}"
        );
    }
    val
}

unsafe extern "C" fn check_multisample_quality_levels(
    h: Hdevice,
    fmt: ddi::DXGI_FORMAT,
    sample_count: u32,
    out: *mut u32,
) {
    if !out.is_null() {
        let val = helios_multisample_quality_levels(h, fmt, sample_count);
        *out = val;
        if MSAA_LOG_COUNT.first_n_then_every(256, 4096).is_some() {
            trace_line!(
                "MSAA out fmt={fmt} c={sample_count} flags=legacy out={out:p} val={val}"
            );
        }
    }
}

unsafe extern "C" fn check_multisample_quality_levels_wddm1_3(
    h: Hdevice,
    fmt: ddi::DXGI_FORMAT,
    sample_count: u32,
    _flags: u32,
    out: *mut u32,
) {
    if !out.is_null() {
        let val = helios_multisample_quality_levels(h, fmt, sample_count);
        *out = val;
        if MSAA_LOG_COUNT.first_n_then_every(256, 4096).is_some() {
            trace_line!(
                "MSAA out fmt={fmt} c={sample_count} flags=0x{_flags:x} out={out:p} val={val}"
            );
        }
    }
}

/// `pfnCheckCounterInfo` — report the device's performance-counter capabilities.
/// Previously an unimplemented noop that left the out struct unwritten, so the
/// D3D11 runtime read whatever it had pre-set for `NumSimultaneousCounters` /
/// `NumDetectableParallelUnits` (potentially garbage → over-allocation/validation
/// failure during `LLOCompleteLayerConstruction`). We expose no device-dependent
/// counters: zero the struct (LastDeviceDependentCounter = 0, 0 simultaneous
/// counters) and report a single detectable parallel unit. PATH-A (2026-06-22).
unsafe extern "C" fn check_counter_info(_h: Hdevice, info: *mut ddi::D3D10DDI_COUNTER_INFO) {
    if !info.is_null() {
        core::ptr::write_bytes(
            info as *mut u8,
            0,
            core::mem::size_of::<ddi::D3D10DDI_COUNTER_INFO>(),
        );
        (*info).NumDetectableParallelUnits = 1;
    }
}

unsafe extern "C" fn check_counter(
    _h: Hdevice,
    _query: ddi::D3D10DDI_QUERY,
    counter_type: *mut ddi::D3D10DDI_COUNTER_TYPE,
    active_counters: *mut u32,
    _name: ddi::LPSTR,
    name_len: *mut u32,
    _units: ddi::LPSTR,
    units_len: *mut u32,
    _description: ddi::LPSTR,
    description_len: *mut u32,
) {
    if !counter_type.is_null() {
        *counter_type = ddi::D3D10DDI_COUNTER_TYPE_D3D10DDI_COUNTER_TYPE_UINT64;
    }
    if !active_counters.is_null() {
        *active_counters = 0;
    }
    if !name_len.is_null() {
        *name_len = 0;
    }
    if !units_len.is_null() {
        *units_len = 0;
    }
    if !description_len.is_null() {
        *description_len = 0;
    }
}

// --- Compute ---------------------------------------------------------------

unsafe extern "C" fn dispatch(h: Hdevice, x: u32, y: u32, z: u32) {
    if DISPATCH_LOG_COUNT.first_n_then_every(1024, 1024).is_some() {
        if let Some(dev) = helios_device(h) {
            let ia = dev.owned.ia.borrow();
            trace_line!(
                "DDI Dispatch x={} y={} z={} cs=0x{:x} rt0_alloc=0x{:x} rt0={}x{} fmt={}",
                x,
                y,
                z,
                ia.current_cs,
                ia.current_rt0_alloc,
                ia.current_rt0_width,
                ia.current_rt0_height,
                ia.current_rt0_format
            );
        }
    }
    if let Some(context) = d3d11_context(h) {
        context.Dispatch(x, y, z);
    }
}

unsafe extern "C" fn dispatch_indirect(
    h: Hdevice,
    h_args: ddi::D3D10DDI_HRESOURCE,
    aligned_byte_offset: u32,
) {
    if DISPATCH_LOG_COUNT.first_n_then_every(1024, 1024).is_some() {
        trace_line!(
            "DDI DispatchIndirect args_alloc=0x{:x} offset={}",
            resource_allocation(h_args),
            aligned_byte_offset
        );
    }
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(buf) =
        load_resource(h_args).and_then(|r| (*r).cast::<ID3D11Buffer>().ok())
    else {
        return;
    };
    context.DispatchIndirect(&buf, aligned_byte_offset);
}

unsafe extern "C" fn set_resource_min_lod(
    h: Hdevice,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    min_lod: f32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(res) = load_resource(h_resource) else {
        return;
    };
    context.SetResourceMinLOD(&*res, min_lod);
}

unsafe extern "C" fn check_format_support(h: Hdevice, fmt: ddi::DXGI_FORMAT, out: *mut u32) {
    // The D3D11 DDI `pfnCheckFormatSupport` returns API-style D3D11_FORMAT_SUPPORT
    // flags (D3D11 harmonized the DDI with the API enum; the small
    // D3D10_DDI_FORMAT_SUPPORT enum is only for the legacy D3D10 DDI). So pass
    // DXVK's value through unchanged — translating to the D3D10 DDI layout
    // regresses even a plain D3D11CreateDevice to DXGI_ERROR_UNSUPPORTED.
    let mut caps: u32 = 0;
    if let Some(device) = d3d11_device(h) {
        if let Ok(c) = device.CheckFormatSupport(DXGI_FORMAT(fmt as i32)) {
            caps = c;
        }
    }
    let raw_caps = caps;
    // Keep format support coherent with the active feature-level profile and
    // D3D11.3 §19.2.5. API D3D11_FORMAT_SUPPORT:
    // MULTISAMPLE_RESOLVE=0x40000, MULTISAMPLE_RENDERTARGET=0x200000,
    // MULTISAMPLE_LOAD=0x400000.
    const MSAA_RESOLVE: u32 = 0x0004_0000;
    const MSAA_RENDERTARGET: u32 = 0x0020_0000;
    const MSAA_LOAD: u32 = 0x0040_0000;
    const MSAA_BITS: u32 = MSAA_RESOLVE | MSAA_RENDERTARGET | MSAA_LOAD;
    const DDI_MSAA_RENDERTARGET: u32 = 0x0000_0008;
    const DDI_MSAA_LOAD: u32 = 0x0000_0010;
    const VIDEO_BITS: u32 = 0x0800_0000 | 0x1000_0000 | 0x2000_0000 | 0x4000_0000;
    const TEXTURE1D: u32 = 0x0000_0010;
    const TEXTURE3D: u32 = 0x0000_0040;
    const SHADER_SAMPLE: u32 = 0x0000_0200;
    const SHADER_SAMPLE_COMPARISON: u32 = 0x0000_0400;
    const MIP_AUTOGEN: u32 = 0x0000_2000;
    const RENDER_TARGET: u32 = 0x0000_4000;
    const BLENDABLE: u32 = 0x0000_8000;
    const DEPTH_STENCIL: u32 = 0x0001_0000;
    const SHADER_GATHER: u32 = 0x0080_0000;
    const SHADER_GATHER_COMPARISON: u32 = 0x0400_0000;
    // R820: the six D3D11_FORMAT_SUPPORT bits this function used but did not
    // name. They are what the five whole-value hex constants below decompose
    // into; without them a reader could not tell which capability each hex
    // asserts, or whether it contradicts the MSAA/video scrubs around it.
    const TEXTURE2D: u32 = 0x0000_0020;
    const TEXTURECUBE: u32 = 0x0000_0080;
    const SHADER_LOAD: u32 = 0x0000_0100;
    const MIP: u32 = 0x0000_1000;
    const CPU_LOCKABLE: u32 = 0x0002_0000;
    const CAST_WITHIN_BIT_LAYOUT: u32 = 0x0010_0000;

    // The five values copied from WARP, expressed as compositions and PINNED to
    // the hex they replace. The const-asserts are what make this rewrite
    // provably value-preserving -- and what will make the eventual move to
    // forward/format_caps.rs safe.
    const TYPELESS_PARENT_TEXTURE_CAPS: u32 = TEXTURE1D
        | TEXTURE2D
        | TEXTURE3D
        | TEXTURECUBE
        | MIP
        | CPU_LOCKABLE
        | CAST_WITHIN_BIT_LAYOUT;
    const _: () = assert!(TYPELESS_PARENT_TEXTURE_CAPS == 0x0012_10f0);

    /// The TYPELESS depth-stencil PARENTS: R32G8X24_TYPELESS (19) and
    /// R24G8_TYPELESS (44). Lockable texture families with no depth,
    /// render-target or multisample capability of their own -- the typed
    /// children below carry those.
    const WARP_TYPELESS_PARENT_CAPS: u32 =
        TEXTURE1D | TEXTURE2D | TEXTURECUBE | MIP | CPU_LOCKABLE | CAST_WITHIN_BIT_LAYOUT;
    const _: () = assert!(WARP_TYPELESS_PARENT_CAPS == 0x0012_10b0);

    /// The typed DEPTH formats: D32_FLOAT_S8X24_UINT (20), D32_FLOAT (40),
    /// D24_UNORM_S8_UINT (45) and D16_UNORM (55). These add DEPTH_STENCIL and
    /// the multisample render-target bit.
    const WARP_DEPTH_CAPS: u32 = TEXTURE1D
        | TEXTURE2D
        | TEXTURECUBE
        | MIP
        | DEPTH_STENCIL
        | CPU_LOCKABLE
        | CAST_WITHIN_BIT_LAYOUT
        | MSAA_RENDERTARGET;
    const _: () = assert!(WARP_DEPTH_CAPS == 0x0033_10b0);

    /// The DEPTH read views: R32_FLOAT_X8X24_TYPELESS (21) and
    /// R24_UNORM_X8_TYPELESS (46). Fully sampleable -- sample,
    /// comparison-sample, gather, comparison-gather and multisample load.
    const WARP_DEPTH_READ_CAPS: u32 = TEXTURE1D
        | TEXTURE2D
        | TEXTURECUBE
        | SHADER_LOAD
        | SHADER_SAMPLE
        | SHADER_SAMPLE_COMPARISON
        | MIP
        | CPU_LOCKABLE
        | CAST_WITHIN_BIT_LAYOUT
        | MSAA_LOAD
        | SHADER_GATHER
        | SHADER_GATHER_COMPARISON;
    const _: () = assert!(WARP_DEPTH_READ_CAPS == 0x04d2_17b0);

    /// The STENCIL read views: X32_TYPELESS_G8X24_UINT (22) and
    /// X24_TYPELESS_G8_UINT (47). Integer, so loadable but not sampleable --
    /// no SHADER_SAMPLE and no gather.
    const WARP_STENCIL_READ_CAPS: u32 = TEXTURE1D
        | TEXTURE2D
        | TEXTURECUBE
        | SHADER_LOAD
        | MIP
        | CPU_LOCKABLE
        | CAST_WITHIN_BIT_LAYOUT
        | MSAA_LOAD;
    const _: () = assert!(WARP_STENCIL_READ_CAPS == 0x0052_11b0);
    if crate::feature_level_mode() != 1 {
        // FL10.0 profile (and diagnostic mode 2): strip the multisample bits.
        caps &= !MSAA_BITS;
    } else if dxgi_msaa_bits_per_sample(fmt as u32, caps).is_some() {
        // FL11: every output-capable format supports at least 4x MSAA. Expose
        // the generic multisample bit for those formats; load/resolve are
        // narrower and follow the §19.2 resource-load/resolve rules.
        caps |= MSAA_RENDERTARGET;
        // The D3D11 UMD callback uses DDI-format-support low bits even though
        // our backing query is API-style. Preserve the API-style bits for the
        // proven path, but also set the DDI MSAA bits the runtime validates
        // during FL11 device construction.
        caps |= DDI_MSAA_RENDERTARGET;
        if caps & DEPTH_STENCIL == 0 {
            caps |= MSAA_LOAD;
            caps |= DDI_MSAA_LOAD;
        }
        if dxgi_resolve_required(fmt as u32) {
            caps |= MSAA_RESOLVE;
        }
        // Helios does not implement the D3D11 video DDI. DXVK's API-level
        // CheckFormatSupport marks ordinary sampled/output formats as video
        // processor inputs/outputs, but the Microsoft runtime validates those
        // bits as part of the UMD feature contract.
        caps &= !VIDEO_BITS;
        if dxgi_color_typeless_parent(fmt as u32) {
            caps = TYPELESS_PARENT_TEXTURE_CAPS;
        }
        if dxgi_integer_typed_format(fmt as u32) {
            caps &= !(SHADER_SAMPLE
                | SHADER_SAMPLE_COMPARISON
                | MIP_AUTOGEN
                | MSAA_RESOLVE
                | SHADER_GATHER
                | SHADER_GATHER_COMPARISON);
        }
        // D3D11 requires the 96-bit R32G32B32 typed output formats as ordinary
        // texture/render-target formats. Vulkan/DXVK under-reports several of
        // these bits; WARP exposes them and the runtime validates the family as
        // part of the FL11 construction path before it reaches application code.
        match fmt as u32 {
            6 => caps |= TEXTURE1D | TEXTURE3D | MIP_AUTOGEN | RENDER_TARGET | BLENDABLE,
            7 | 8 => caps |= TEXTURE1D | TEXTURE3D | RENDER_TARGET,
            _ => {}
        }
    }

    // The Microsoft D3D11 runtime validates some typeless/depth format families
    // as a group during CDevice::LLOCompleteLayerConstruction. DXVK reports the
    // host's raw SO_BUFFER support for the color-typed siblings (for example
    // R32_FLOAT), while the matching depth format (D32_FLOAT) reports none; that
    // mismatch is rejected with DXGI_ERROR_UNSUPPORTED. Normalize the family to
    // the stricter depth-compatible answer.
    const D3D11_FORMAT_SUPPORT_SO_BUFFER: u32 = 0x0000_0008;
    if matches!(
        fmt,
        DXGI_FORMAT_R32_TYPELESS
            | DXGI_FORMAT_D32_FLOAT
            | DXGI_FORMAT_R32_FLOAT
            | DXGI_FORMAT_R32_UINT
            | DXGI_FORMAT_R32_SINT
            | DXGI_FORMAT_R24G8_TYPELESS
            | DXGI_FORMAT_D24_UNORM_S8_UINT
            | DXGI_FORMAT_R24_UNORM_X8_TYPELESS
            | DXGI_FORMAT_X24_TYPELESS_G8_UINT
            | DXGI_FORMAT_R32G8X24_TYPELESS
            | DXGI_FORMAT_D32_FLOAT_S8X24_UINT
            | DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS
            | DXGI_FORMAT_X32_TYPELESS_G8X24_UINT
    ) {
        caps &= !D3D11_FORMAT_SUPPORT_SO_BUFFER;
    }
    const DXGI_FORMAT_R32_TYPELESS: ddi::DXGI_FORMAT = 39;
    const DXGI_FORMAT_D32_FLOAT: ddi::DXGI_FORMAT = 40;
    const DXGI_FORMAT_R32_FLOAT: ddi::DXGI_FORMAT = 41;
    const DXGI_FORMAT_R32_UINT: ddi::DXGI_FORMAT = 42;
    const DXGI_FORMAT_R32_SINT: ddi::DXGI_FORMAT = 43;
    const DXGI_FORMAT_R24G8_TYPELESS: ddi::DXGI_FORMAT = 44;
    const DXGI_FORMAT_D24_UNORM_S8_UINT: ddi::DXGI_FORMAT = 45;
    const DXGI_FORMAT_R24_UNORM_X8_TYPELESS: ddi::DXGI_FORMAT = 46;
    const DXGI_FORMAT_X24_TYPELESS_G8_UINT: ddi::DXGI_FORMAT = 47;
    const DXGI_FORMAT_R32G8X24_TYPELESS: ddi::DXGI_FORMAT = 19;
    const DXGI_FORMAT_D32_FLOAT_S8X24_UINT: ddi::DXGI_FORMAT = 20;
    const DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS: ddi::DXGI_FORMAT = 21;
    const DXGI_FORMAT_X32_TYPELESS_G8X24_UINT: ddi::DXGI_FORMAT = 22;
    const DXGI_FORMAT_D16_UNORM: ddi::DXGI_FORMAT = 55;
    if crate::feature_level_mode() == 1 {
        match fmt {
            // Match WARP's API-visible caps for depth-format families; the
            // DDI-only MSAA RT bit is re-applied immediately below where
            // required. DXVK over-reports the read/view siblings here, and the
            // FL11 constructor rejects that before issuing an MSAA query.
            DXGI_FORMAT_R32G8X24_TYPELESS | DXGI_FORMAT_R24G8_TYPELESS => {
                caps = WARP_TYPELESS_PARENT_CAPS
            }
            DXGI_FORMAT_D32_FLOAT_S8X24_UINT
            | DXGI_FORMAT_D32_FLOAT
            | DXGI_FORMAT_D24_UNORM_S8_UINT
            | DXGI_FORMAT_D16_UNORM => caps = WARP_DEPTH_CAPS,
            DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS | DXGI_FORMAT_R24_UNORM_X8_TYPELESS => {
                caps = WARP_DEPTH_READ_CAPS
            }
            DXGI_FORMAT_X32_TYPELESS_G8X24_UINT | DXGI_FORMAT_X24_TYPELESS_G8_UINT => {
                caps = WARP_STENCIL_READ_CAPS
            }
            _ => {}
        }
    }
    if crate::feature_level_mode() == 1 && dxgi_msaa_bits_per_sample(fmt as u32, caps).is_some() {
        // In the D3D10/11 UMD callback, low bit 0x8 is
        // D3D10_DDI_FORMAT_SUPPORT_MULTISAMPLE_RENDERTARGET, not API
        // SO_BUFFER. Re-assert it after the API-style compatibility scrubs
        // above so FL11's MSAA validation sees a coherent format-support /
        // quality-level pair, including depth-stencil families.
        caps |= DDI_MSAA_RENDERTARGET;
        if caps & DEPTH_STENCIL == 0 {
            caps |= DDI_MSAA_LOAD;
        }
    }

    // `DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM` (89) is the one format the WDDM
    // runtime validates specially during device creation: the driver MUST signal
    // lack of support with the explicit `D3D10_DDI_FORMAT_SUPPORT_NOT_SUPPORTED`
    // sentinel (0x80000000, "Set only this bit") rather than a bare 0. DXVK does
    // not implement this legacy XR format and returns 0, which the runtime treats
    // as a malformed response and fails `D3D11CreateDevice` with
    // `DXGI_ERROR_DRIVER_INTERNAL_ERROR` (0x887a0020) — the only caps=0 format,
    // observed live. (The observed *value* is unchanged; before R801 this comment
    // named it `DXGI_ERROR_UNSUPPORTED`, which is 0x887a0004. A malformed driver
    // caps response being reported as a driver-internal fault is consistent.)
    // That is the device-create failure DWM hits, after which
    // dwmcore!CreateD3D11Device raises the DWM error 0x889800b0 and crash-loops.
    // Map the 0 to the sentinel so the runtime accepts the (legitimately
    // unsupported) format. PATH-A (2026-06-22).
    const DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM: ddi::DXGI_FORMAT = 89;
    const DDI_FORMAT_SUPPORT_NOT_SUPPORTED: u32 = 0x8000_0000;
    if fmt == DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM && caps == 0 {
        caps = DDI_FORMAT_SUPPORT_NOT_SUPPORTED;
    }
    if crate::feature_level_mode() == 1 {
        trace_line!(
            "FormatSupport fmt={fmt} raw=0x{raw_caps:08x} final=0x{caps:08x} output_bits={:?}",
            dxgi_output_bits_per_sample(fmt as u32, caps)
        );
    }
    if !out.is_null() {
        *out = caps;
    }
}
unsafe extern "C" fn destroy_depth_state(_h: Hdevice, h_state: ddi::D3D10DDI_HDEPTHSTENCILSTATE) {
    release_com(h_state);
}

/// In-process offscreen clear+readback through the real forwarders (no install,
/// no DXGI, no runtime): create a render-target texture, clear it, copy to a
/// staging texture, map and read back pixel 0. Returns 0 on PASS. `h` is the
/// device handle whose pDrvPrivate is a constructed HeliosDevice.
pub unsafe fn selftest_offscreen_clear(h: Hdevice) -> i32 {
    let fmt_bgra: ddi::DXGI_FORMAT = 87; // DXGI_FORMAT_B8G8R8A8_UNORM
    let mip = ddi::D3D10DDI_MIPINFO {
        TexelWidth: 64,
        TexelHeight: 64,
        TexelDepth: 1,
        PhysicalWidth: 64,
        PhysicalHeight: 64,
        PhysicalDepth: 1,
    };

    let mut rt_desc = ddi::D3D11DDIARG_CREATERESOURCE::default();
    rt_desc.pMipInfoList = &mip;
    rt_desc.ResourceDimension = RES_TEX2D;
    rt_desc.Format = fmt_bgra;
    rt_desc.Usage = 0; // DEFAULT
    rt_desc.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
    rt_desc.MipLevels = 1;
    rt_desc.ArraySize = 1;
    rt_desc.SampleDesc.Count = 1;

    let mut rt_priv = 0u64;
    let h_rt = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut rt_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &rt_desc, h_rt, Default::default());
    if rt_priv == 0 {
        log_error!("selftest_offscreen_clear: RT create failed");
        return 1;
    }

    let mut rtv_desc = ddi::D3D10DDIARG_CREATERENDERTARGETVIEW::default();
    rtv_desc.hDrvResource = h_rt;
    rtv_desc.Format = fmt_bgra;
    rtv_desc.ResourceDimension = RES_TEX2D;
    let mut rtv_priv = 0u64;
    let h_rtv = ddi::D3D10DDI_HRENDERTARGETVIEW {
        pDrvPrivate: &mut rtv_priv as *mut u64 as *mut c_void,
    };
    create_rtv(h, &rtv_desc, h_rtv, Default::default());
    if rtv_priv == 0 {
        log_error!("selftest_offscreen_clear: RTV create failed");
        return 2;
    }

    let mut color = [0.25f32, 0.5, 0.75, 1.0];
    clear_rtv(h, h_rtv, color.as_mut_ptr());

    let mut st_desc = ddi::D3D11DDIARG_CREATERESOURCE::default();
    st_desc.pMipInfoList = &mip;
    st_desc.ResourceDimension = RES_TEX2D;
    st_desc.Format = fmt_bgra;
    st_desc.Usage = 3; // STAGING
    st_desc.BindFlags = 0;
    st_desc.MipLevels = 1;
    st_desc.ArraySize = 1;
    st_desc.SampleDesc.Count = 1;
    let mut st_priv = 0u64;
    let h_st = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut st_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &st_desc, h_st, Default::default());
    if st_priv == 0 {
        log_error!("selftest_offscreen_clear: staging create failed");
        return 3;
    }

    resource_copy(h, h_st, h_rt);
    flush(h);

    let mut mapped = ddi::D3D10DDI_MAPPED_SUBRESOURCE::default();
    resource_map(h, h_st, 0, 1 /* D3D10_DDI_MAP_READ */, 0, &mut mapped);
    let mut result = 4;
    if !mapped.pData.is_null() {
        let px = mapped.pData as *const u8;
        let (b, g, r, a) = (*px, *px.add(1), *px.add(2), *px.add(3));
        log_error!(
            "selftest_offscreen_clear: readback BGRA = {b} {g} {r} {a} (want ~191 128 64 255)"
        );
        let ok = (b as i32 - 191).abs() <= 2
            && (g as i32 - 128).abs() <= 2
            && (r as i32 - 64).abs() <= 2
            && a == 255;
        result = if ok { 0 } else { 5 };
        resource_unmap(h, h_st, 0);
    }

    destroy_resource(h, h_st);
    destroy_rtv(h, h_rtv);
    destroy_resource(h, h_rt);
    log_error!(
        "selftest_offscreen_clear: result={result} (0=PASS)"
    );
    result
}

unsafe fn compile_hlsl(src: &[u8], entry: &[u8], target: &[u8]) -> Option<ID3DBlob> {
    let mut blob: Option<ID3DBlob> = None;
    let mut errs: Option<ID3DBlob> = None;
    let hr = D3DCompile(
        src.as_ptr() as *const c_void,
        src.len(),
        PCSTR::null(),
        None,
        None,
        PCSTR(entry.as_ptr()),
        PCSTR(target.as_ptr()),
        0,
        0,
        &mut blob,
        Some(&mut errs),
    );
    if hr.is_err() {
        if let Some(e) = errs {
            let msg =
                core::slice::from_raw_parts(e.GetBufferPointer() as *const u8, e.GetBufferSize());
            log_error!(
                "D3DCompile error: {}",
                String::from_utf8_lossy(msg)
            );
        }
        return None;
    }
    blob
}

/// Synthesize + create a tex2d via the create_resource forwarder. Returns the
/// resource handle private storage (caller owns it).
unsafe fn make_tex2d(
    h: Hdevice,
    priv_: &mut u64,
    usage: u32,
    bind: u32,
) -> ddi::D3D10DDI_HRESOURCE {
    let mip = ddi::D3D10DDI_MIPINFO {
        TexelWidth: 64,
        TexelHeight: 64,
        TexelDepth: 1,
        PhysicalWidth: 64,
        PhysicalHeight: 64,
        PhysicalDepth: 1,
    };
    let mut desc = ddi::D3D11DDIARG_CREATERESOURCE::default();
    desc.pMipInfoList = &mip;
    desc.ResourceDimension = RES_TEX2D;
    desc.Format = 87; // BGRA
    desc.Usage = usage;
    desc.BindFlags = bind;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.SampleDesc.Count = 1;
    let hr = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: priv_ as *mut u64 as *mut c_void,
    };
    create_resource(h, &desc, hr, Default::default());
    hr
}

/// Draw a `SV_VertexID` triangle (no vertex buffer / input layout) into an
/// offscreen RT and read back the center pixel. Returns 0 on PASS.
pub unsafe fn selftest_triangle(h: Hdevice) -> i32 {
    let vs_src = b"float4 VS(uint id:SV_VertexID):SV_Position{float2 uv=float2((id<<1)&2,id&2);return float4(uv*2-1,0,1);}\0";
    let ps_src = b"float4 PS():SV_Target{return float4(1,0,0,1);}\0"; // red
    let Some(vsb) = compile_hlsl(vs_src, b"VS\0", b"vs_5_0\0") else {
        return 10;
    };
    let Some(psb) = compile_hlsl(ps_src, b"PS\0", b"ps_5_0\0") else {
        return 11;
    };

    let mut rt_priv = 0u64;
    let h_rt = make_tex2d(h, &mut rt_priv, 0, D3D11_BIND_RENDER_TARGET.0 as u32);
    if rt_priv == 0 {
        return 12;
    }
    let mut rtv_desc = ddi::D3D10DDIARG_CREATERENDERTARGETVIEW::default();
    rtv_desc.hDrvResource = h_rt;
    rtv_desc.Format = 87;
    rtv_desc.ResourceDimension = RES_TEX2D;
    let mut rtv_priv = 0u64;
    let h_rtv = ddi::D3D10DDI_HRENDERTARGETVIEW {
        pDrvPrivate: &mut rtv_priv as *mut u64 as *mut c_void,
    };
    create_rtv(h, &rtv_desc, h_rtv, Default::default());
    if rtv_priv == 0 {
        return 13;
    }

    let mut vs_priv = 0u64;
    let h_vs = ddi::D3D10DDI_HSHADER {
        pDrvPrivate: &mut vs_priv as *mut u64 as *mut c_void,
    };
    create_vertex_shader(
        h,
        vsb.GetBufferPointer() as *const u32,
        h_vs,
        Default::default(),
        core::ptr::null(),
    );
    let mut ps_priv = 0u64;
    let h_ps = ddi::D3D10DDI_HSHADER {
        pDrvPrivate: &mut ps_priv as *mut u64 as *mut c_void,
    };
    create_pixel_shader(
        h,
        psb.GetBufferPointer() as *const u32,
        h_ps,
        Default::default(),
        core::ptr::null(),
    );
    if vs_priv == 0 || ps_priv == 0 {
        log_error!("selftest_triangle: shader create failed");
        return 14;
    }
    vs_set_shader(h, h_vs);
    ps_set_shader(h, h_ps);

    // CULL_NONE rasterizer so the (back-facing) triangle isn't culled — also
    // validates create_rasterizer_state + set_rasterizer_state.
    let mut rs_desc = ddi::D3D10_DDI_RASTERIZER_DESC::default();
    rs_desc.FillMode = 3; // SOLID
    rs_desc.CullMode = 1; // NONE
    rs_desc.DepthClipEnable = 1;
    let mut rs_priv = 0u64;
    let h_rs = ddi::D3D10DDI_HRASTERIZERSTATE {
        pDrvPrivate: &mut rs_priv as *mut u64 as *mut c_void,
    };
    create_rasterizer_state(h, &rs_desc, h_rs, Default::default());
    set_rasterizer_state(h, h_rs);

    let mut black = [0.0f32, 0.0, 0.0, 1.0];
    clear_rtv(h, h_rtv, black.as_mut_ptr());
    set_render_targets(
        h,
        &h_rtv,
        1,
        0,
        Default::default(),
        core::ptr::null(),
        core::ptr::null(),
        0,
        0,
        0,
        0,
    );
    let vp = ddi::D3D10_DDI_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: 64.0,
        Height: 64.0,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    };
    set_viewports(h, 1, 0, &vp);
    ia_set_topology(h, 4 /* TRIANGLELIST */);
    draw(h, 3, 0);
    flush(h);

    // Read back the center pixel.
    let mut st_priv = 0u64;
    let h_st = make_tex2d(h, &mut st_priv, 3 /* STAGING */, 0);
    if st_priv == 0 {
        return 15;
    }
    resource_copy(h, h_st, h_rt);
    flush(h);
    let mut mapped = ddi::D3D10DDI_MAPPED_SUBRESOURCE::default();
    resource_map(h, h_st, 0, 1, 0, &mut mapped);
    let mut result = 16;
    if !mapped.pData.is_null() {
        let off = 32 * mapped.RowPitch as usize + 32 * 4;
        let px = (mapped.pData as *const u8).add(off);
        let (b, g, r, a) = (*px, *px.add(1), *px.add(2), *px.add(3));
        log_error!(
            "selftest_triangle: center BGRA = {b} {g} {r} {a} (want red ~0 0 255 255)"
        );
        result = if r > 250 && g < 5 && b < 5 && a == 255 {
            0
        } else {
            17
        };
        resource_unmap(h, h_st, 0);
    }
    destroy_resource(h, h_st);
    destroy_shader(h, h_ps);
    destroy_shader(h, h_vs);
    destroy_rtv(h, h_rtv);
    destroy_resource(h, h_rt);
    log_error!("selftest_triangle: result={result} (0=PASS)");
    result
}

/// Diagnostic: create an immutable constant buffer with known content, copy it
/// to a staging buffer, and read it back. Tells us whether buffer-content upload
/// works (red back) or not (zero back) — independent of shader binding.
pub unsafe fn selftest_cb_readback(h: Hdevice) -> i32 {
    let red: [f32; 4] = [1.0, 0.25, 0.5, 1.0];
    let mip = ddi::D3D10DDI_MIPINFO {
        TexelWidth: 16,
        ..Default::default()
    };
    let init = ddi::D3D10_DDIARG_SUBRESOURCE_UP {
        pSysMem: red.as_ptr() as *mut c_void,
        SysMemPitch: 16,
        SysMemSlicePitch: 16,
    };
    let mut cb = ddi::D3D11DDIARG_CREATERESOURCE::default();
    cb.pMipInfoList = &mip;
    cb.pInitialDataUP = &init;
    cb.ResourceDimension = RES_BUFFER;
    cb.Usage = 1; // IMMUTABLE
    cb.BindFlags = D3D11_BIND_CONSTANT_BUFFER.0 as u32;
    cb.MipLevels = 1;
    cb.ArraySize = 1;
    let mut cb_priv = 0u64;
    let h_cb = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut cb_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &cb, h_cb, Default::default());

    let mut st = ddi::D3D11DDIARG_CREATERESOURCE::default();
    st.pMipInfoList = &mip;
    st.ResourceDimension = RES_BUFFER;
    st.Usage = 3; // STAGING
    st.BindFlags = 0;
    st.MipLevels = 1;
    st.ArraySize = 1;
    let mut st_priv = 0u64;
    let h_st = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut st_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &st, h_st, Default::default());
    if cb_priv == 0 || st_priv == 0 {
        log_error!(
            "cb_readback: create failed cb={cb_priv:#x} st={st_priv:#x}"
        );
        return 1;
    }
    resource_copy(h, h_st, h_cb);
    flush(h);
    let mut mapped = ddi::D3D10DDI_MAPPED_SUBRESOURCE::default();
    resource_map(h, h_st, 0, 1, 0, &mut mapped);
    if mapped.pData.is_null() {
        log_error!("cb_readback: map failed");
        return 2;
    }
    let f = mapped.pData as *const f32;
    log_error!(
        "cb_readback: floats = {} {} {} {} (want 1 0.25 0.5 1)",
        *f,
        *f.add(1),
        *f.add(2),
        *f.add(3)
    );
    resource_unmap(h, h_st, 0);
    destroy_resource(h, h_st);
    destroy_resource(h, h_cb);
    0
}

/// Diagnostic: full draw of a triangle whose PS reads a constant buffer, with
/// probes to localize the "CB content never reaches the shader" bug. Returns 0
/// when the center pixel is the CB-supplied green.
///
/// Probes, in order:
///  1. CB created with immutable green initial-data (proven-good content path).
///  2. After `ps_set_constant_buffers`, call `PSGetConstantBuffers` and log
///     whether DXVK reports our buffer bound — splits "our bind didn't register"
///     from "bound but shader reads garbage".
///  3. After the draw, copy the CB to staging and read it back — confirms the
///     resource still holds green at draw time (rules out it being clobbered).
pub unsafe fn selftest_triangle_cb(h: Hdevice) -> i32 {
    let vs_src = b"float4 VS(uint id:SV_VertexID):SV_Position{float2 uv=float2((id<<1)&2,id&2);return float4(uv*2-1,0,1);}\0";
    // PS reads col from the constant buffer at b0.
    let ps_src = b"cbuffer C:register(b0){float4 col;} float4 PS():SV_Target{return col;}\0";
    let Some(vsb) = compile_hlsl(vs_src, b"VS\0", b"vs_5_0\0") else {
        return 30;
    };
    let Some(psb) = compile_hlsl(ps_src, b"PS\0", b"ps_5_0\0") else {
        return 31;
    };

    let mut rt_priv = 0u64;
    let h_rt = make_tex2d(h, &mut rt_priv, 0, D3D11_BIND_RENDER_TARGET.0 as u32);
    if rt_priv == 0 {
        return 32;
    }
    let mut rtv_desc = ddi::D3D10DDIARG_CREATERENDERTARGETVIEW::default();
    rtv_desc.hDrvResource = h_rt;
    rtv_desc.Format = 87;
    rtv_desc.ResourceDimension = RES_TEX2D;
    let mut rtv_priv = 0u64;
    let h_rtv = ddi::D3D10DDI_HRENDERTARGETVIEW {
        pDrvPrivate: &mut rtv_priv as *mut u64 as *mut c_void,
    };
    create_rtv(h, &rtv_desc, h_rtv, Default::default());
    if rtv_priv == 0 {
        return 33;
    }

    // Constant buffer: 16 bytes, immutable, content = green (0,1,0,1).
    let green: [f32; 4] = [0.0, 1.0, 0.0, 1.0];
    let mip = ddi::D3D10DDI_MIPINFO {
        TexelWidth: 16,
        ..Default::default()
    };
    let init = ddi::D3D10_DDIARG_SUBRESOURCE_UP {
        pSysMem: green.as_ptr() as *mut c_void,
        SysMemPitch: 16,
        SysMemSlicePitch: 16,
    };
    let mut cb = ddi::D3D11DDIARG_CREATERESOURCE::default();
    cb.pMipInfoList = &mip;
    cb.pInitialDataUP = &init;
    cb.ResourceDimension = RES_BUFFER;
    cb.Usage = 1; // IMMUTABLE
    cb.BindFlags = D3D11_BIND_CONSTANT_BUFFER.0 as u32;
    cb.MipLevels = 1;
    cb.ArraySize = 1;
    let mut cb_priv = 0u64;
    let h_cb = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut cb_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &cb, h_cb, Default::default());
    if cb_priv == 0 {
        log_error!("selftest_triangle_cb: CB create failed");
        return 34;
    }
    log_error!("selftest_triangle_cb: CB COM ptr = {cb_priv:#x}");

    // Shaders.
    let mut vs_priv = 0u64;
    let h_vs = ddi::D3D10DDI_HSHADER {
        pDrvPrivate: &mut vs_priv as *mut u64 as *mut c_void,
    };
    create_vertex_shader(
        h,
        vsb.GetBufferPointer() as *const u32,
        h_vs,
        Default::default(),
        core::ptr::null(),
    );
    let mut ps_priv = 0u64;
    let h_ps = ddi::D3D10DDI_HSHADER {
        pDrvPrivate: &mut ps_priv as *mut u64 as *mut c_void,
    };
    create_pixel_shader(
        h,
        psb.GetBufferPointer() as *const u32,
        h_ps,
        Default::default(),
        core::ptr::null(),
    );
    if vs_priv == 0 || ps_priv == 0 {
        return 35;
    }
    vs_set_shader(h, h_vs);
    ps_set_shader(h, h_ps);

    // CULL_NONE so the triangle isn't culled.
    let mut rs_desc = ddi::D3D10_DDI_RASTERIZER_DESC::default();
    rs_desc.FillMode = 3;
    rs_desc.CullMode = 1;
    rs_desc.DepthClipEnable = 1;
    let mut rs_priv = 0u64;
    let h_rs = ddi::D3D10DDI_HRASTERIZERSTATE {
        pDrvPrivate: &mut rs_priv as *mut u64 as *mut c_void,
    };
    create_rasterizer_state(h, &rs_desc, h_rs, Default::default());
    set_rasterizer_state(h, h_rs);

    // Bind the constant buffer to PS slot b0.
    ps_set_constant_buffers(h, 0, 1, &h_cb);

    // PROBE 2: ask DXVK what it now has bound at PS b0.
    if let Some(c) = d3d11_context(h) {
        let mut bound: [Option<ID3D11Buffer>; 1] = [None];
        c.PSGetConstantBuffers(0, Some(&mut bound));
        match &bound[0] {
            Some(b) => {
                let raw = b.as_raw() as usize;
                log_error!(
                    "selftest_triangle_cb: PSGetConstantBuffers[0] = {raw:#x} (CB was {cb_priv:#x}, match={})",
                    raw == cb_priv as usize
                );
            }
            None => log_error!(
                "selftest_triangle_cb: PSGetConstantBuffers[0] = NULL (bind did NOT register!)"
            ),
        }
    }

    let mut black = [0.0f32, 0.0, 0.0, 1.0];
    clear_rtv(h, h_rtv, black.as_mut_ptr());
    set_render_targets(
        h,
        &h_rtv,
        1,
        0,
        Default::default(),
        core::ptr::null(),
        core::ptr::null(),
        0,
        0,
        0,
        0,
    );
    let vp = ddi::D3D10_DDI_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: 64.0,
        Height: 64.0,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    };
    set_viewports(h, 1, 0, &vp);
    ia_set_topology(h, 4 /* TRIANGLELIST */);
    draw(h, 3, 0);
    flush(h);

    // PROBE 3: CB content still green at draw time?
    let mut cbst = ddi::D3D11DDIARG_CREATERESOURCE::default();
    cbst.pMipInfoList = &mip;
    cbst.ResourceDimension = RES_BUFFER;
    cbst.Usage = 3; // STAGING
    cbst.MipLevels = 1;
    cbst.ArraySize = 1;
    let mut cbst_priv = 0u64;
    let h_cbst = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut cbst_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &cbst, h_cbst, Default::default());
    if cbst_priv != 0 {
        resource_copy(h, h_cbst, h_cb);
        flush(h);
        let mut m = ddi::D3D10DDI_MAPPED_SUBRESOURCE::default();
        resource_map(h, h_cbst, 0, 1, 0, &mut m);
        if !m.pData.is_null() {
            let f = m.pData as *const f32;
            log_error!(
                "selftest_triangle_cb: CB content at draw time = {} {} {} {} (want 0 1 0 1)",
                *f,
                *f.add(1),
                *f.add(2),
                *f.add(3)
            );
            resource_unmap(h, h_cbst, 0);
        }
        destroy_resource(h, h_cbst);
    }

    // Read back the center pixel.
    let mut st_priv = 0u64;
    let h_st = make_tex2d(h, &mut st_priv, 3, 0);
    if st_priv == 0 {
        return 36;
    }
    resource_copy(h, h_st, h_rt);
    flush(h);
    let mut mapped = ddi::D3D10DDI_MAPPED_SUBRESOURCE::default();
    resource_map(h, h_st, 0, 1, 0, &mut mapped);
    let mut result = 37;
    if !mapped.pData.is_null() {
        let off = 32 * mapped.RowPitch as usize + 32 * 4;
        let px = (mapped.pData as *const u8).add(off);
        let (b, g, r, a) = (*px, *px.add(1), *px.add(2), *px.add(3));
        log_error!(
            "selftest_triangle_cb: center BGRA = {b} {g} {r} {a} (want green ~0 255 0 255)"
        );
        result = if g > 250 && r < 5 && b < 5 && a == 255 {
            0
        } else {
            38
        };
        resource_unmap(h, h_st, 0);
    }
    destroy_resource(h, h_st);
    destroy_shader(h, h_ps);
    destroy_shader(h, h_vs);
    destroy_resource(h, h_cb);
    destroy_rtv(h, h_rtv);
    destroy_resource(h, h_rt);
    log_error!("selftest_triangle_cb: result={result} (0=PASS)");
    result
}

// --- Input layouts (lazy, via the VS input signature) -----------------------

/// One d3d10umddi input element, kept until the bound VS is known (the DDI gives
/// us the VS input *register*, not a semantic name, so we resolve names lazily).
struct DdiInputElement {
    input_slot: u32,
    aligned_byte_offset: u32,
    format: i32,
    input_slot_class: u32,
    instance_step_rate: u32,
    input_register: u32,
}

/// Element-layout data, Box'd and stashed in the CreateElementLayout handle.
struct LayoutData {
    elements: Vec<DdiInputElement>,
}

/// Parse a vertex shader's DXBC `ISGN` (input signature) chunk and return the
/// (semantic name, semantic index) of the element bound to input `register`.
/// `ID3D11Device::CreateInputLayout` needs semantic names, but the DDI only
/// passes the register index, so we recover the names from the shader bytecode.
unsafe fn isgn_lookup(dxbc: &[u8], register: u32) -> Option<(std::ffi::CString, u32)> {
    if dxbc.len() < 32 || &dxbc[0..4] != b"DXBC" {
        return None;
    }
    let chunk_count = u32::from_le_bytes(dxbc[28..32].try_into().ok()?) as usize;
    for i in 0..chunk_count {
        let off_pos = 32 + i * 4;
        if off_pos + 4 > dxbc.len() {
            return None;
        }
        let coff = u32::from_le_bytes(dxbc[off_pos..off_pos + 4].try_into().ok()?) as usize;
        if coff + 8 > dxbc.len() || &dxbc[coff..coff + 4] != b"ISGN" {
            continue;
        }
        let data = coff + 8; // skip FourCC + chunk size
        if data + 8 > dxbc.len() {
            return None;
        }
        let elem_count = u32::from_le_bytes(dxbc[data..data + 4].try_into().ok()?) as usize;
        for e in 0..elem_count {
            let ep = data + 8 + e * 24;
            if ep + 24 > dxbc.len() {
                return None;
            }
            let name_off = u32::from_le_bytes(dxbc[ep..ep + 4].try_into().ok()?) as usize;
            let sem_index = u32::from_le_bytes(dxbc[ep + 4..ep + 8].try_into().ok()?);
            let reg = u32::from_le_bytes(dxbc[ep + 16..ep + 20].try_into().ok()?);
            if reg == register {
                let nstart = data + name_off;
                // Every other offset in this function is checked; this one was
                // not, and `&v[a..a]` with `a > len` is out of bounds in Rust —
                // a panic in a DDI is a silent graphics deadlock.
                if nstart >= dxbc.len() {
                    return None;
                }
                let mut nend = nstart;
                while nend < dxbc.len() && dxbc[nend] != 0 {
                    nend += 1;
                }
                let name = std::ffi::CString::new(&dxbc[nstart..nend]).ok()?;
                return Some((name, sem_index));
            }
        }
        return None;
    }
    None
}

// R423 note: the review asks for a host-target unit test asserting that
// `isgn_lookup` returns None (rather than panicking) for a `name_off` past the
// chunk. It is not addable in T2: this crate is `crate-type = ["cdylib"]`, so
// `cargo test` has no lib target, and adding `rlib` is not enough — the test
// harness then fails to link because build.rs passes the DXVK static archives
// through `cargo:rustc-link-arg-cdylib`, and this cargo rejects
// `rustc-link-arg-tests` as an invalid instruction. Switching them to a plain
// `rustc-link-arg` would change the SHIPPING cdylib's link line, which is not a
// trade worth making for one test. The analogue of T0's host-testable
// `kmd_logic` crate does not exist for the UMD; creating one is a file-split
// change and belongs with T8. The bounds check itself is in `isgn_lookup`.

/// Build a minimal DXBC container with a synthetic `ISGN` chunk for the given
/// input registers, followed by the raw SM4/SM5 token stream (SHDR/SHEX).
///
/// The DDI hands shaders to the driver as raw token streams with no DXBC
/// container, so there are no semantic names to recover for
/// `ID3D11Device::CreateInputLayout`. Names are only a matching key between
/// the layout descs and the shader signature, and DXVK resolves an element's
/// vertex-input LOCATION from the matched signature entry's register — which
/// is also how dxbc-spv assigns locations (`dcl_input v[r]` → location r) when
/// it compiles the container-less shader. So a fabricated "TEXCOORD<r>" per
/// register keeps both sides consistent.
fn build_layout_signature_blob(registers: &[u32], tokens: &[u8]) -> Vec<u8> {
    const NAME: &[u8] = b"TEXCOORD\0";
    let entry_count = registers.len();
    let entries_size = entry_count * 24;
    let name_off = 8 + entries_size; // offsets are relative to chunk-data start
    let isgn_len_unpadded = name_off + NAME.len();
    let isgn_len = (isgn_len_unpadded + 3) & !3;

    // Code chunk tag from the version token (major >= 5 uses SHEX).
    let version_token = if tokens.len() >= 4 {
        u32::from_le_bytes(tokens[0..4].try_into().unwrap())
    } else {
        0
    };
    let code_tag: &[u8; 4] = if ((version_token >> 4) & 0xF) >= 5 {
        b"SHEX"
    } else {
        b"SHDR"
    };

    // DXBC header (32) + 2 chunk offsets (8).
    let isgn_chunk_off = 32 + 8;
    let code_chunk_off = isgn_chunk_off + 8 + isgn_len;
    let total = code_chunk_off + 8 + tokens.len();

    let mut blob = vec![0u8; total];
    blob[0..4].copy_from_slice(b"DXBC");
    // bytes 4..20: checksum left zero (DXVK does not verify it)
    blob[20..24].copy_from_slice(&1u32.to_le_bytes());
    blob[24..28].copy_from_slice(&(total as u32).to_le_bytes());
    blob[28..32].copy_from_slice(&2u32.to_le_bytes());
    blob[32..36].copy_from_slice(&(isgn_chunk_off as u32).to_le_bytes());
    blob[36..40].copy_from_slice(&(code_chunk_off as u32).to_le_bytes());

    blob[isgn_chunk_off..isgn_chunk_off + 4].copy_from_slice(b"ISGN");
    blob[isgn_chunk_off + 4..isgn_chunk_off + 8].copy_from_slice(&(isgn_len as u32).to_le_bytes());
    let data = isgn_chunk_off + 8;
    blob[data..data + 4].copy_from_slice(&(entry_count as u32).to_le_bytes());
    blob[data + 4..data + 8].copy_from_slice(&8u32.to_le_bytes());
    for (i, reg) in registers.iter().enumerate() {
        let ep = data + 8 + i * 24;
        blob[ep..ep + 4].copy_from_slice(&(name_off as u32).to_le_bytes());
        blob[ep + 4..ep + 8].copy_from_slice(&reg.to_le_bytes()); // semantic index = register
        blob[ep + 8..ep + 12].copy_from_slice(&0u32.to_le_bytes()); // no system value
        blob[ep + 12..ep + 16].copy_from_slice(&3u32.to_le_bytes()); // float32
        blob[ep + 16..ep + 20].copy_from_slice(&reg.to_le_bytes());
        blob[ep + 20] = 0x0F; // mask
        blob[ep + 21] = 0x0F; // read/write mask
    }
    blob[data + name_off..data + name_off + NAME.len()].copy_from_slice(NAME);

    blob[code_chunk_off..code_chunk_off + 4].copy_from_slice(code_tag);
    blob[code_chunk_off + 4..code_chunk_off + 8]
        .copy_from_slice(&(tokens.len() as u32).to_le_bytes());
    blob[code_chunk_off + 8..code_chunk_off + 8 + tokens.len()].copy_from_slice(tokens);
    blob
}

unsafe extern "C" fn calc_size_element_layout(
    _h: Hdevice,
    _a: *const ddi::D3D10DDIARG_CREATEELEMENTLAYOUT,
) -> u64 {
    8
}

unsafe extern "C" fn create_element_layout(
    _h: Hdevice,
    arg: *const ddi::D3D10DDIARG_CREATEELEMENTLAYOUT,
    h_el: ddi::D3D10DDI_HELEMENTLAYOUT,
    _hrt: ddi::D3D10DDI_HRTELEMENTLAYOUT,
) {
    clear_handle(h_el);
    let a = &*arg;
    let mut elems = Vec::with_capacity(a.NumElements as usize);
    for i in 0..a.NumElements as usize {
        let e = &*a.pVertexElements.add(i);
        elems.push(DdiInputElement {
            input_slot: e.InputSlot,
            aligned_byte_offset: e.AlignedByteOffset,
            format: e.Format as i32,
            input_slot_class: e.InputSlotClass as u32,
            instance_step_rate: e.InstanceDataStepRate,
            input_register: e.InputRegister,
        });
    }
    // The element-layout slot is a fourth payload kind: a Box this driver
    // allocated, but stored as a bare `usize` and then used as the identity key
    // of `IaState::layout_cache`. R803's finding does not list it; it is the
    // same latent confusion as the resource and RTV slots.
    let Some(slot) = boxed_slot(h_el) else {
        return;
    };
    slot.store(LayoutData { elements: elems });
}

unsafe extern "C" fn destroy_element_layout(h: Hdevice, h_el: ddi::D3D10DDI_HELEMENTLAYOUT) {
    let Some(slot) = boxed_slot(h_el) else {
        return;
    };
    // The slot word doubles as this layout's identity in `layout_cache`, so it
    // is read before the box is taken.
    let p = slot.word();
    if p != 0 {
        let mut owned = std::collections::HashSet::new();
        if let Some(dev) = helios_device(h) {
            let mut ia = dev.owned.ia.borrow_mut();
            ia.layout_cache.retain(|&(layout, _), cached| {
                if layout == p {
                    if *cached != 0 {
                        owned.insert(*cached);
                    }
                    false
                } else {
                    true
                }
            });
            if ia.current_layout == p {
                ia.current_layout = 0;
            }
        }
        for cached in &owned {
            // SAFETY: each removed cache value owns one CreateInputLayout
            // reference, adopted here and released exactly once.
            drop(IUnknown::from_raw(*cached as *mut c_void));
        }
        trace_line!(
            "DDI DestroyElementLayout: layout=0x{:x} released_cached={}",
            p,
            owned.len()
        );
        drop(slot.take());
    }
}

unsafe extern "C" fn ia_set_input_layout(h: Hdevice, h_el: ddi::D3D10DDI_HELEMENTLAYOUT) {
    if let Some(dev) = helios_device(h) {
        let p = match boxed_slot(h_el) {
            Some(slot) => slot.word(),
            None => 0,
        };
        dev.owned.ia.borrow_mut().current_layout = p;
    }
}

/// Lazily create + bind the `ID3D11InputLayout` for the current (element layout,
/// VS) pair, resolving element semantic names from the VS input signature.
unsafe fn bind_input_layout(h: Hdevice) {
    let Some(dev) = helios_device(h) else {
        return;
    };
    let (lp, vp) = {
        let ia = dev.owned.ia.borrow();
        (ia.current_layout, ia.current_vs)
    };
    if lp == 0 || vp == 0 {
        if SHADER_BIND_LOG_COUNT.first_n(256).is_some() {
            log_error!(
                "DDI bind_input_layout skipped: layout=0x{:x} vs=0x{:x}",
                lp, vp
            );
        }
        return;
    }
    let cached = dev.owned.ia.borrow().layout_cache.get(&(lp, vp)).copied();
    let il_raw = match cached {
        Some(p) => p,
        None => {
            let bytecode = match dev.owned.ia.borrow().vs_bytecode.get(&vp) {
                Some(b) => b.clone(),
                None => {
                    log_error!(
                        "DDI bind_input_layout skipped: missing VS bytecode layout=0x{:x} vs=0x{:x}",
                        lp, vp
                    );
                    return;
                }
            };
            let layout = &*(lp as *const LayoutData);
            let is_dxbc = bytecode.len() >= 4 && &bytecode[0..4] == b"DXBC";
            // Reserve so the CString store never reallocates (the descs below
            // borrow raw pointers into it for the CreateInputLayout call).
            let mut names: Vec<std::ffi::CString> = Vec::with_capacity(layout.elements.len());
            let mut descs: Vec<D3D11_INPUT_ELEMENT_DESC> =
                Vec::with_capacity(layout.elements.len());
            let mut registers: Vec<u32> = Vec::with_capacity(layout.elements.len());
            for el in &layout.elements {
                let (name, sem_index) = if is_dxbc {
                    // Real container: recover the shader's own semantic names.
                    match isgn_lookup(&bytecode, el.input_register) {
                        Some(v) => v,
                        None => {
                            log_error!(
                                "DDI bind_input_layout: no ISGN entry for input_register={} fmt={} slot={} offset={}",
                                el.input_register, el.format, el.input_slot, el.aligned_byte_offset
                            );
                            continue;
                        }
                    }
                } else {
                    // Raw DDI token stream: no ISGN exists. Fabricate
                    // TEXCOORD<register> and pair it with a synthetic ISGN in
                    // the blob below so name-matching resolves to the register.
                    (
                        std::ffi::CString::new("TEXCOORD").unwrap(),
                        el.input_register,
                    )
                };
                names.push(name);
                let name_ptr = names.last().unwrap().as_ptr() as *const u8;
                if !registers.contains(&el.input_register) {
                    registers.push(el.input_register);
                }
                descs.push(D3D11_INPUT_ELEMENT_DESC {
                    SemanticName: PCSTR(name_ptr),
                    SemanticIndex: sem_index,
                    Format: DXGI_FORMAT(el.format),
                    InputSlot: el.input_slot,
                    AlignedByteOffset: el.aligned_byte_offset,
                    InputSlotClass: D3D11_INPUT_CLASSIFICATION(el.input_slot_class as i32),
                    InstanceDataStepRate: el.instance_step_rate,
                });
            }
            if descs.is_empty() {
                log_error!(
                    "DDI bind_input_layout skipped: empty descs elements={} vs_len={}",
                    layout.elements.len(),
                    bytecode.len()
                );
                return;
            }
            let signature_blob;
            let blob_for_layout: &[u8] = if is_dxbc {
                &bytecode
            } else {
                signature_blob = build_layout_signature_blob(&registers, &bytecode);
                &signature_blob
            };
            let Some(device) = d3d11_device(h) else {
                return;
            };
            let mut il: Option<ID3D11InputLayout> = None;
            match device.CreateInputLayout(&descs, blob_for_layout, Some(&mut il)) {
                Ok(()) => match il {
                    Some(l) => {
                        let raw = l.into_raw() as usize;
                        log_error!(
                            "DDI CreateInputLayout ok: layout=0x{:x} vs=0x{:x} elems={} raw=0x{:x}",
                            lp,
                            vp,
                            descs.len(),
                            raw
                        );
                        dev.owned.ia.borrow_mut().layout_cache.insert((lp, vp), raw);
                        raw
                    }
                    None => return,
                },
                Err(e) => {
                    log_error!("CreateInputLayout failed: {e:?}");
                    return;
                }
            }
        }
    };
    if let Some(context) = d3d11_context(h) {
        let il = ManuallyDrop::new(ID3D11InputLayout::from_raw(il_raw as *mut c_void));
        context.IASetInputLayout(&*il);
    }

    // VUID-Input-08733: the DDI never provides shader-input component types
    // (RegisterComponentType arrives 0/UNKNOWN — verified against both dwm's
    // SM4 shaders and the SM5 draw probe), so compiled VS inputs default to
    // float32 while layouts may bind SINT/UINT vertex formats: vertex-fetch
    // UB (dwm binds R16G16_SINT TEXCOORDs; the garbage is the prime Xid-109
    // suspect). The INPUT LAYOUT is the ground truth for the numeric class —
    // any (layout, VS) pair the runtime allows to bind matched the app's
    // original input signature — so bind a variant recompiled with the
    // layout's classes whenever any attribute is non-float.
    resolve_vs_input_variant(h, lp, vp);
}

/// Numeric class of a DXGI vertex format for Vulkan's vertex-input contract,
/// as a DXBC ISGN component type: 1 = UINT, 2 = SINT, 3 = FLOAT (covers
/// FLOAT/UNORM/SNORM — all float-class at the fetch).
fn dxgi_vertex_class(format: i32) -> u32 {
    match format {
        // *_UINT: R32G32B32A32, R32G32B32, R16G16B16A16, R32G32, R10G10B10A2,
        // R8G8B8A8, R16G16, R32, R8G8, R16, R8
        3 | 7 | 12 | 17 | 25 | 30 | 36 | 42 | 50 | 57 | 62 => 1,
        // *_SINT: same families
        4 | 8 | 14 | 18 | 32 | 38 | 43 | 52 | 59 | 64 => 2,
        _ => 3,
    }
}

/// Component mask of a DXGI vertex format (for synthesized ISGN entries).
fn dxgi_vertex_mask(format: i32) -> u32 {
    match format {
        1..=4 | 9..=14 | 19..=32 => 0xf,    // 4-component families
        5..=8 => 0x7,                       // R32G32B32
        15..=18 | 33..=38 | 48..=52 => 0x3, // 2-component families
        _ => 0x1,                           // scalars and the rest
    }
}

/// Pick (and lazily compile) the vertex-shader variant whose declared input
/// component types match the bound layout's format classes, then bind it.
/// All-float layouts (the overwhelmingly common case) bind the original.
unsafe fn resolve_vs_input_variant(h: Hdevice, lp: usize, vp: usize) {
    let Some(dev) = helios_device(h) else {
        return;
    };
    let layout = &*(lp as *const LayoutData);
    // (register, class, mask) per input register, merging multi-element
    // registers (matrix-style attributes span elements, same class).
    let mut classes: Vec<(u32, u32, u32)> = Vec::new();
    let mut any_nonfloat = false;
    for el in &layout.elements {
        let class = dxgi_vertex_class(el.format);
        let mask = dxgi_vertex_mask(el.format);
        if class != 3 {
            any_nonfloat = true;
        }
        if let Some(entry) = classes.iter_mut().find(|c| c.0 == el.input_register) {
            entry.1 = entry.1.max(class);
            entry.2 |= mask;
        } else {
            classes.push((el.input_register, class, mask));
        }
    }

    let desired = if !any_nonfloat {
        vp
    } else {
        // FNV-1a over (register, class) pairs = the variant cache key.
        let mut key: u64 = 0xcbf2_9ce4_8422_2325;
        for &(r, c, _) in &classes {
            key = (key ^ (((r as u64) << 8) | c as u64)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        let cached = dev.owned.ia.borrow().vs_variants.get(&(vp, key)).copied();
        let variant = match cached {
            Some(v) => v,
            None => {
                let v = create_vs_input_variant(dev, vp, &classes);
                dev.owned.ia.borrow_mut().vs_variants.insert((vp, key), v);
                v
            }
        };
        if variant != 0 {
            variant
        } else {
            vp
        }
    };

    if dev.owned.ia.borrow().bound_vs_com == desired {
        return;
    }
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let s = ManuallyDrop::new(ID3D11VertexShader::from_raw(desired as *mut c_void));
    context.VSSetShader(&*s, None);
    dev.owned.ia.borrow_mut().bound_vs_com = desired;
    if SHADER_BIND_LOG_COUNT.first_n(256).is_some() {
        trace_line!("DDI VS input-class variant bound: vs=0x{vp:x} -> 0x{desired:x}");
    }
}

/// Recompile a vertex shader with its synthesized ISGN component types taken
/// from the bound input layout. Returns 0 on failure (caller falls back to
/// the original shader — no worse than the pre-variant behaviour).
unsafe fn create_vs_input_variant(
    dev: &HeliosDevice,
    vp: usize,
    classes: &[(u32, u32, u32)],
) -> usize {
    let (bytecode, mut words) = {
        let ia = dev.owned.ia.borrow();
        let Some(b) = ia.vs_bytecode.get(&vp) else {
            log_error!("VS variant: no bytecode for vs=0x{vp:x}");
            return 0;
        };
        if b.len() >= 4 && &b[0..4] == b"DXBC" {
            // Real container: its own ISGN already carries real types.
            return 0;
        }
        let w = ia
            .vs_sig_words
            .get(&vp)
            .cloned()
            .unwrap_or_else(|| vec![0u32, 0u32]);
        (b.clone(), w)
    };

    let n_in = words[0] as usize;
    if n_in > 0 {
        // Patch the DDI-provided entries' component types from the layout.
        for i in 0..n_in {
            let base = 2 + i * 5;
            let reg = words[base + 1];
            if let Some(&(_, class, _)) = classes.iter().find(|c| c.0 == reg) {
                words[base + 3] = class;
            } else if words[base + 3] == 0 {
                words[base + 3] = 3;
            }
        }
    } else {
        // Shader arrived through the legacy untyped create: synthesize the
        // input entries wholesale from the layout (extra entries for unused
        // registers are declared-then-eliminated by the compiler).
        let n_out = words[1];
        let out_words = words.split_off(2);
        words = vec![classes.len() as u32, n_out];
        for &(reg, class, mask) in classes {
            words.extend_from_slice(&[0, reg, mask, class, 0]);
        }
        words.extend_from_slice(&out_words);
    }

    let dxvk = &dev.dxvk;
    let raw = dxvk.create_shader_sig(
        0,
        bytecode.as_ptr(),
        bytecode.len(),
        words.as_ptr(),
        words.len(),
    );
    log_error!(
        "VS input-class variant: vs=0x{vp:x} classes={:?} -> raw=0x{raw:x}",
        classes.iter().map(|c| (c.0, c.1)).collect::<Vec<_>>()
    );
    raw
}

unsafe extern "C" fn ia_set_vertex_buffers(
    h: Hdevice,
    start: u32,
    num: u32,
    buffers: *const ddi::D3D10DDI_HRESOURCE,
    strides: *const u32,
    offsets: *const u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let mut bufs: Vec<Option<ID3D11Buffer>> = Vec::with_capacity(num as usize);
    for i in 0..num as usize {
        let h_buf = *buffers.add(i);
        bufs.push(load_resource(h_buf).and_then(|r| (*r).cast::<ID3D11Buffer>().ok()));
    }
    if let Some(dev) = helios_device(h) {
        let mut ia = dev.owned.ia.borrow_mut();
        if start == 0 && num != 0 {
            ia.current_vb0 = bufs
                .first()
                .and_then(|b| b.as_ref())
                .map(|b| b.as_raw() as usize)
                .unwrap_or(0);
            ia.current_vb0_stride = if strides.is_null() { 0 } else { *strides };
            ia.current_vb0_offset = if offsets.is_null() { 0 } else { *offsets };
        }
    }
    let n = IA_BIND_LOG_COUNT.next();
    if n < 128 || num == 0 {
        let first_stride = if num != 0 && !strides.is_null() {
            *strides
        } else {
            0
        };
        let first_offset = if num != 0 && !offsets.is_null() {
            *offsets
        } else {
            0
        };
        let first_raw = bufs
            .first()
            .and_then(|b| b.as_ref())
            .map(|b| b.as_raw() as usize)
            .unwrap_or(0);
        trace_line!(
            "DDI IASetVertexBuffers start={} num={} first=0x{:x} stride={} offset={}",
            start, num, first_raw, first_stride, first_offset
        );
    }
    context.IASetVertexBuffers(
        start,
        num,
        Some(bufs.as_ptr()),
        Some(strides),
        Some(offsets),
    );
}

unsafe extern "C" fn ia_set_index_buffer(
    h: Hdevice,
    h_buf: ddi::D3D10DDI_HRESOURCE,
    format: ddi::DXGI_FORMAT,
    offset: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let buf = load_resource(h_buf).and_then(|r| (*r).cast::<ID3D11Buffer>().ok());
    if let Some(dev) = helios_device(h) {
        let mut ia = dev.owned.ia.borrow_mut();
        ia.current_ib = buf.as_ref().map(|b| b.as_raw() as usize).unwrap_or(0);
        ia.current_ib_format = format as u32;
        ia.current_ib_offset = offset;
    }
    if IA_BIND_LOG_COUNT.first_n(128).is_some() {
        trace_line!(
            "DDI IASetIndexBuffer raw=0x{:x} fmt={} offset={}",
            buf.as_ref().map(|b| b.as_raw() as usize).unwrap_or(0),
            format as u32,
            offset
        );
    }
    context.IASetIndexBuffer(buf.as_ref(), DXGI_FORMAT(format as i32), offset);
}

// --- Blend state ------------------------------------------------------------

unsafe extern "C" fn calc_size_blend(_h: Hdevice, _d: *const ddi::D3D10_1_DDI_BLEND_DESC) -> u64 {
    8
}

unsafe extern "C" fn create_blend_state(
    h: Hdevice,
    desc: *const ddi::D3D10_1_DDI_BLEND_DESC,
    h_bs: ddi::D3D10DDI_HBLENDSTATE,
    _hrt: ddi::D3D10DDI_HRTBLENDSTATE,
) {
    clear_handle(h_bs);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let d = &*desc;
    let mut rt: [D3D11_RENDER_TARGET_BLEND_DESC; 8] = Default::default();
    for i in 0..8 {
        let s = &d.RenderTarget[i];
        rt[i] = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: windows::Win32::Foundation::BOOL(s.BlendEnable),
            SrcBlend: D3D11_BLEND(s.SrcBlend),
            DestBlend: D3D11_BLEND(s.DestBlend),
            BlendOp: D3D11_BLEND_OP(s.BlendOp),
            SrcBlendAlpha: D3D11_BLEND(s.SrcBlendAlpha),
            DestBlendAlpha: D3D11_BLEND(s.DestBlendAlpha),
            BlendOpAlpha: D3D11_BLEND_OP(s.BlendOpAlpha),
            RenderTargetWriteMask: s.RenderTargetWriteMask,
        };
    }
    let bd = D3D11_BLEND_DESC {
        AlphaToCoverageEnable: windows::Win32::Foundation::BOOL(d.AlphaToCoverageEnable),
        IndependentBlendEnable: windows::Win32::Foundation::BOOL(d.IndependentBlendEnable),
        RenderTarget: rt,
    };
    let mut bs: Option<ID3D11BlendState> = None;
    let created = device.CreateBlendState(&bd, Some(&mut bs));
    if let Err(ref e) = created {
        log_error!("DDI CreateBlendState failed: {e:?}");
    }
    finish_create(h, created, bs, |s| store_com(h_bs, s));
}

/// D3D11.1-interface `pfnCalcPrivateBlendStateSize` (same 8-byte COM-pointer
/// slot as `calc_size_blend`; only the desc type differs).
unsafe extern "C" fn calc_size_blend_11_1(
    _h: Hdevice,
    _d: *const ddi::D3D11_1_DDI_BLEND_DESC,
) -> u64 {
    8
}

/// D3D11.1-interface `pfnCreateBlendState`. Every device-funcs table from
/// D3D11.1 up passes `D3D11_1_DDI_BLEND_DESC`, whose per-RT struct INSERTS
/// `LogicOpEnable` after `BlendEnable` and `LogicOp` before
/// `RenderTargetWriteMask` — it is NOT prefix-compatible with the 10.1 desc.
/// Installing the 10.1 reader here made the mask field land on the 11.1
/// desc's `BlendOpAlpha` (default D3D11_BLEND_OP_ADD = 1), so every blend
/// state — including the runtime's defaults — carried RenderTargetWriteMask =
/// RED-only and every draw on the stack wrote just the R channel (the
/// black/red-tinted composition class; minimal repro
/// tools/d3d11_shared_draw_probe.cpp, 2026-07-03).
unsafe extern "C" fn create_blend_state_11_1(
    h: Hdevice,
    desc: *const ddi::D3D11_1_DDI_BLEND_DESC,
    h_bs: ddi::D3D10DDI_HBLENDSTATE,
    _hrt: ddi::D3D10DDI_HRTBLENDSTATE,
) {
    clear_handle(h_bs);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let Ok(device1) = device.cast::<ID3D11Device1>() else {
        log_error!("DDI create_blend_state_11_1: ID3D11Device1 cast failed");
        return;
    };
    let d = &*desc;
    let mut rt: [D3D11_RENDER_TARGET_BLEND_DESC1; 8] = Default::default();
    for i in 0..8 {
        let s = &d.RenderTarget[i];
        rt[i] = D3D11_RENDER_TARGET_BLEND_DESC1 {
            BlendEnable: windows::Win32::Foundation::BOOL(s.BlendEnable),
            LogicOpEnable: windows::Win32::Foundation::BOOL(s.LogicOpEnable),
            SrcBlend: D3D11_BLEND(s.SrcBlend),
            DestBlend: D3D11_BLEND(s.DestBlend),
            BlendOp: D3D11_BLEND_OP(s.BlendOp),
            SrcBlendAlpha: D3D11_BLEND(s.SrcBlendAlpha),
            DestBlendAlpha: D3D11_BLEND(s.DestBlendAlpha),
            BlendOpAlpha: D3D11_BLEND_OP(s.BlendOpAlpha),
            // The API logic-op enum mirrors the DDI enum value-for-value.
            LogicOp: D3D11_LOGIC_OP(s.LogicOp),
            RenderTargetWriteMask: s.RenderTargetWriteMask,
        };
    }
    let bd = D3D11_BLEND_DESC1 {
        AlphaToCoverageEnable: windows::Win32::Foundation::BOOL(d.AlphaToCoverageEnable),
        IndependentBlendEnable: windows::Win32::Foundation::BOOL(d.IndependentBlendEnable),
        RenderTarget: rt,
    };
    let mut bs: Option<ID3D11BlendState1> = None;
    let created = device1.CreateBlendState1(&bd, Some(&mut bs));
    if created.is_err() {
        log_error!("DDI create_blend_state_11_1: CreateBlendState1 failed");
    }
    // `set_blend_state` loads an ID3D11BlendState from this slot; store the
    // base interface. A failed QI is a create failure like any other — folding
    // it into `None` routes it through the same report instead of dropping it.
    let base = match bs {
        Some(s) => match s.cast::<ID3D11BlendState>() {
            Ok(b) => Some(b),
            Err(e) => {
                log_error!(
                    "DDI create_blend_state_11_1: ID3D11BlendState cast failed: {e:?}"
                );
                None
            }
        },
        None => None,
    };
    finish_create(h, created, base, |b| store_com(h_bs, b));
}

unsafe extern "C" fn set_blend_state(
    h: Hdevice,
    h_bs: ddi::D3D10DDI_HBLENDSTATE,
    factor: *const f32,
    sample_mask: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let f = if factor.is_null() {
        [1.0f32; 4]
    } else {
        [*factor, *factor.add(1), *factor.add(2), *factor.add(3)]
    };
    match load_com::<ID3D11BlendState>(h_bs) {
        Some(s) => context.OMSetBlendState(&*s, Some(&f), sample_mask),
        None => context.OMSetBlendState(None, Some(&f), sample_mask),
    }
}

unsafe extern "C" fn destroy_blend_state(_h: Hdevice, h_bs: ddi::D3D10DDI_HBLENDSTATE) {
    release_com(h_bs);
}

// --- Dcomp present vehicle (road 4 unit 2) ----------------------------------
//
// The ICD (mesa WSI) presents a Vulkan frame through a D3D11 composition
// swapchain it owns (the "vehicle"): it publishes its frame's
// (resid -> pid, fenceId, value) in the WS1 #4 seqlock table, hands
// (resid, value, geometry, allocation identity) to
// `helios_umd_set_present_source` — stored per-THREAD below — and calls
// Present() on the vehicle ON THE SAME THREAD. The next dxgi_present on
// that thread consumes the slot: instead of the normal src->dst copy it
// alias-imports the ICD frame by resid (cached per resid), image-copies it
// into hSurfaceToPresent's DXVK texture (the copy-time consumer wait orders
// the copy against the ICD's GPU writes via the published slot), publishes
// the BACKBUFFER slot with THIS device's own fence (correct: the vehicle
// wrote the backbuffer; dwm's consumer wait needs zero changes), then mints
// the token via pfnPresentCb exactly as any flip-model present.

/// Pending vehicle present source (one per thread; same-thread contract).
#[derive(Clone, Copy)]
pub struct PresentSource {
    pub resid: u32,
    pub fence_value: u64,
    pub width: u32,
    pub height: u32,
    pub dxgi_format: u32,
    /// Creator's exact vkAllocateMemory size/type — the typed import
    /// identity (vkr's OPAQUE-fd import needs an exact-size match; importing
    /// at the opener's own requirements is the wrong-size failure mode).
    pub alloc_size: u64,
    pub memory_type_index: u32,
}

/// The dcomp-vehicle present protocol as ONE state machine.
///
/// It used to be three independent `Cell`s — a pending source, a raw
/// `HeliosDevice` pointer, and a pending (fenceId, value) — each path had to
/// remember to update in lockstep, with the ordering enforced only by comments
/// and by counters that fire after the fact. Two failure arms cleared two of
/// the three, and `dxgi_present` had an `E_FAIL` exit that cleared neither, so
/// the ICD could consume frame N's `(fenceId, value)` for frame N+1 and recycle
/// an image on a fence that had already retired.
///
/// Cross-DLL sequence, one thread, once per frame:
/// `helios_umd_set_present_source` -> `Present` ->
/// `helios_umd_get_present_result` -> optional `helios_umd_wait_last_present`.
/// Every exit now has to name a next state.
///
/// `Copy` on purpose: a `Cell` cannot panic, where a `RefCell` can double-borrow
/// — and these are reached from `extern "C"` exports under `panic = "abort"`.
#[derive(Clone, Copy)]
enum VehicleSlot {
    Idle,
    /// A source was armed and the vehicle `Present` has not consumed it yet.
    Armed(PresentSource),
    /// A vehicle present was MINTED on `device`. `result` is its
    /// (fenceId, value) — the vehicle device's named present-fence signal that
    /// retires when the frame copy completes on the host GPU — or `None` when
    /// the publish path was unavailable, so the ICD's consume MISSES and it
    /// falls back to the serial wait.
    Minted {
        device: usize,
        result: Option<(u32, u64)>,
    },
}

thread_local! {
    static VEHICLE: core::cell::Cell<VehicleSlot> =
        const { core::cell::Cell::new(VehicleSlot::Idle) };
}

/// Live `HeliosDevice` private blocks.
///
/// `wait_last_present` dereferences a device pointer recorded by an earlier
/// DDI call, and nothing tied that pointer to the device's lifetime: the
/// vehicle D3D11 device is per-swapchain and released on the ICD worker thread,
/// and a SUCCESS-but-not-displayed `Present` status (DXGI_STATUS_OCCLUDED, which
/// the ICD explicitly handles) means dxgkrnl never calls our present DDI, so the
/// slot is neither updated nor cleared. The runtime-owned private block dxgkrnl
/// frees can then be reused by an unrelated device.
///
/// This is a runtime-guarded REFUSAL, not a proof: a compile-time lifetime is
/// not achievable across the `extern "C"` export boundary, and the ICD may
/// still call on a thread whose device dies between the check and the
/// dereference. Deliberately NOT a global epoch bumped on any destroy — a
/// stale-epoch refusal returns -1, which the ICD reads as "no gate" and then
/// performs no wait at all, reintroducing the 21st-session torn-copy class for
/// unrelated devices.
///
/// The lock is taken once per wait and once per device create/destroy, never on
/// a per-draw or per-present path.
fn live_devices() -> &'static std::sync::Mutex<std::collections::HashSet<usize>> {
    static LIVE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<usize>>> =
        std::sync::OnceLock::new();
    LIVE.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

pub(crate) fn register_live_device(device: usize) {
    if device == 0 {
        return;
    }
    if let Ok(mut live) = live_devices().lock() {
        live.insert(device);
    }
}

pub(crate) fn unregister_live_device(device: usize) {
    if device == 0 {
        return;
    }
    if let Ok(mut live) = live_devices().lock() {
        live.remove(&device);
    }
}

fn device_is_live(device: usize) -> bool {
    match live_devices().lock() {
        Ok(live) => live.contains(&device),
        // `panic = "abort"` makes poisoning unreachable; refusing is the safe
        // answer if it ever were not.
        Err(_) => false,
    }
}

/// R809 observation counters. All three measure policies the tranche
/// deliberately did NOT change, so the gate run produces the evidence to decide
/// them rather than the decision being taken blind.
///
/// Times a DirectPrimary record was written over a KmdLinearImport one. R809's
/// deferred "DirectPrimary wins, and a LINEAR import may not displace it" rule
/// would make the reverse direction impossible; this counts how often the two
/// actually compete at all.
static SCANOUT_DIRECT_OVER_LINEAR: AtomicUsize = AtomicUsize::new(0);
/// Times the "largest area wins" heuristic kept older, larger geometry after a
/// resolution change DOWNWARDS -- the undocumented policy R809 names.
static SCANOUT_DOWNRES_KEPT: AtomicUsize = AtomicUsize::new(0);
/// Scan-out targets refused for a zero width or height. Expected 0; a
/// zero-extent scan-out target is meaningless, and `NonZeroU32` is what makes
/// it unrepresentable rather than merely unlikely.
static SCANOUT_ZERO_EXTENT: AtomicUsize = AtomicUsize::new(0);

/// Scan-out primary creates refused because the bridge returned a resource
/// with a zero row pitch (R806 sub-commit 2).
///
/// Expected to stay 0: `create_ddi_scanout_texture2d` returns 0 for a zero
/// width/height and otherwise computes a non-zero pitch, so a non-zero
/// resource implies a non-zero pitch. A non-zero value here means that
/// cross-FFI contract has been broken, and the refusal is what stops a
/// direct-scanout primary being stamped into the KMD meta that the UMD could
/// never identify through PresentCb private data.
static SCANOUT_PRIMARY_ZERO_PITCH: AtomicUsize = AtomicUsize::new(0);

/// `set_present_source` refusals (invalid geometry/resid from the ICD).
static EXT_SOURCE_REFUSED: AtomicUsize = AtomicUsize::new(0);
/// `wait_last_present` calls whose recorded device is no longer live.
static EXT_WAIT_DEAD_DEVICE: AtomicUsize = AtomicUsize::new(0);

static EXT_PRESENTS: AtomicUsize = AtomicUsize::new(0);
static EXT_IMPORT_FAILS: AtomicUsize = AtomicUsize::new(0);
static EXT_COPY_FAILS: AtomicUsize = AtomicUsize::new(0);
static EXT_GEOM_MISMATCH: AtomicUsize = AtomicUsize::new(0);
static EXT_OVERWRITES: AtomicUsize = AtomicUsize::new(0);
static EXT_NO_DEVICE: AtomicUsize = AtomicUsize::new(0);
/// get_present_result called with no pending result (contract violation or
/// a present that failed / published nothing — the ICD falls back to the
/// serial wait).
static EXT_RESULT_MISSES: AtomicUsize = AtomicUsize::new(0);
/// A minted present's result was never consumed before the next one — an
/// ICD that predates the acquire-gate (deploy skew) or a dropped consume.
static EXT_RESULT_OVERWRITES: AtomicUsize = AtomicUsize::new(0);
/// Bounded flip-ordering gate expiries (the flip proceeds; a stale frame on
/// a direct-flip window beats a wedged worker). Steady-state nonzero =
/// the retire→signal chain is slower than the gate bound.
static EXT_FLIP_GATE_TIMEOUTS: AtomicUsize = AtomicUsize::new(0);
/// Why a present returned without minting a swapchain token. All three shapes
/// used to share one log line and return S_OK to DXGI, so the failing stage was
/// lost and the runtime never learned the present had not happened.
#[derive(Copy, Clone, PartialEq, Eq)]
enum PresentSkip {
    NoDxgiCallbacks,
    NoContext,
    NoSourceAllocation,
}

/// The three preconditions of the present-callback block, resolved once. The
/// callback code is unreachable with any of them unmet, so "skipped" is
/// distinguishable from "succeeded" at the type level even though the returned
/// HRESULT is unchanged.
struct PresentReady {
    h_context: core::ptr::NonNull<c_void>,
    src_alloc: core::num::NonZeroU32,
}

/// `dxgi_callbacks` was null: no DXGI base callback table on the device.
static PRESENT_SKIP_NO_CALLBACKS: AtomicUsize = AtomicUsize::new(0);
/// `h_context` was null: pfnCreateContextCb failed at CreateDevice. R404 closes
/// the creation half (such a device is now refused); this counts the presents
/// that reach here on a device that predates it or fails another way.
static PRESENT_SKIP_NO_CONTEXT: AtomicUsize = AtomicUsize::new(0);
/// The presented source resource carries no WDDM allocation.
static PRESENT_SKIP_NO_SRC_ALLOC: AtomicUsize = AtomicUsize::new(0);
/// Rate cap for the skip log line (declared diagnostic-volume change: a device
/// that permanently lacks a context used to write one formatted line per
/// present, at frame rate, through the unconditional writer).
static PRESENT_SKIP_LOG_COUNT: LogThrottle = LogThrottle::new();

/// Resolve the present-callback preconditions, counting exactly which one
/// failed. Deliberately no fourth "NoDevice" variant: `helios_device` returns
/// None only for a null `pDrvPrivate`, which dxgkrnl does not pass.
unsafe fn present_prerequisites(
    dev: &crate::device_funcs::HeliosDevice,
    src_alloc: u32,
) -> Result<PresentReady, PresentSkip> {
    if dev.dxgi_callbacks.is_null() {
        PRESENT_SKIP_NO_CALLBACKS.fetch_add(1, Ordering::Relaxed);
        return Err(PresentSkip::NoDxgiCallbacks);
    }
    let Some(h_context) = dev.context.as_ref().map(|c| c.handle) else {
        PRESENT_SKIP_NO_CONTEXT.fetch_add(1, Ordering::Relaxed);
        return Err(PresentSkip::NoContext);
    };
    let Some(src_alloc) = core::num::NonZeroU32::new(src_alloc) else {
        PRESENT_SKIP_NO_SRC_ALLOC.fetch_add(1, Ordering::Relaxed);
        return Err(PresentSkip::NoSourceAllocation);
    };
    Ok(PresentReady {
        h_context,
        src_alloc,
    })
}

/// Outcome of the bounded frame gate. `#[must_use]` because `dxgi_present1`'s
/// multi arm silently discarded the boolean this replaces.
///
/// "Did not confirm completion", not "timed out": `present_frame_gate` also
/// returns false when the bridge impl/context is missing or an exception was
/// caught, so a nonzero count folds those in.
#[must_use]
#[derive(Copy, Clone, PartialEq, Eq)]
enum GateOutcome {
    Completed,
    NotConfirmed,
}

/// Frame-gate non-confirmations on EVERY path. `EXT_FLIP_GATE_TIMEOUTS` is
/// conditioned on `is_vehicle_present`, so an expiry on the direct-primary
/// path — the one that ships — incremented nothing and logged nothing, and the
/// only trace was the aggregated C++ `present-gate: ... timeouts=` line every
/// 128 presents. A gate expiry means the present is published while DXVK still
/// has queued work: exactly the producer race the gate exists to close, so a
/// steady-state expiry was indistinguishable from a healthy run in the guest
/// counters and the stale-frame symptom got blamed on the KMD marker or the
/// host.
static PRESENT_GATE_TIMEOUTS: AtomicUsize = AtomicUsize::new(0);
static PRESENT_GATE_LOG_COUNT: LogThrottle = LogThrottle::new();

/// Run the bounded gate and count every non-confirmation. The present proceeds
/// either way — a stale frame beats a wedged worker — so the outcome is
/// telemetry, not control flow, but it must not be droppable by accident.
unsafe fn run_present_frame_gate(
    dev: &crate::device_funcs::HeliosDevice,
    gate_us: u32,
    is_vehicle_present: bool,
) -> GateOutcome {
    if dev.dxvk.present_frame_gate(gate_us) {
        return GateOutcome::Completed;
    }
    let total = PRESENT_GATE_TIMEOUTS.fetch_add(1, Ordering::Relaxed) + 1;
    if is_vehicle_present {
        // Unchanged text and cadence: this is the pre-existing vehicle line.
        let n = EXT_FLIP_GATE_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
        if n < 16 || n % 512 == 0 {
            log_error!(
                "vehicle flip gate TIMEOUT (x{}) — flipping anyway",
                n + 1
            );
        }
    } else {
        if PRESENT_GATE_LOG_COUNT.first_n_then_every(16, 512).is_some() {
            log_error!(
                "present frame gate did not confirm completion (x{total}) — presenting anyway"
            );
        }
    }
    GateOutcome::NotConfirmed
}

/// Kernel flip-waits queued ahead of the present packet (the ordering is
/// dxgkrnl-enforced for these presents; the CPU gate is skipped).
static EXT_KWAIT_ARMED: AtomicUsize = AtomicUsize::new(0);
/// Bridge arm refusals (present fence unavailable) — the present falls back
/// to the bounded CPU gate.
static EXT_KWAIT_ARM_FAILS: AtomicUsize = AtomicUsize::new(0);
/// pfnWaitForSynchronizationObjectFromGpuCb failures AFTER a successful arm
/// (the stray future signal is harmless — monotonic, no waiter) — CPU-gate
/// fallback for the present.
static EXT_KWAIT_QUEUE_FAILS: AtomicUsize = AtomicUsize::new(0);

/// Lazy per-device setup for the kernel-enforced flip ordering: create a
/// monitored fence on the RUNTIME's kernel device (the only scope the
/// present context's queued GPU waits accept — raw cross-device handles are
/// rejected 0xC000000D, probe-proven) via pfnCreateSynchronizationObject2Cb,
/// then hand the signal side (runtime CPU-signal callback + fence + CPU VA)
/// to the bridge, which fires it from the present-fence waiter and runs the
/// wedge watchdog. Any missing piece disables the path loudly ONCE for the
/// device; the bounded CPU gate serves instead.
/// The runtime callback the queued GPU wait targets. Spelled out rather than
/// reusing bindgen's `PFND3DDDI_WAITFORSYNCHRONIZATIONOBJECTFROMGPUCB`, which
/// **is** the `Option` -- the whole point of the token is that the `Some` has
/// already been taken.
pub type WaitFromGpuCb = unsafe extern "C" fn(
    ddi::HANDLE,
    *const ddi::D3DDDICB_WAITFORSYNCHRONIZATIONOBJECTFROMGPU,
) -> ddi::HRESULT;

/// Proof that this device's kernel flip-wait path is fully armed.
///
/// Possession is compile-time evidence that `pfnCreateSynchronizationObject2Cb`,
/// `pfnSignalSynchronizationObjectFromCpuCb` and
/// `pfnWaitForSynchronizationObjectFromGpuCb` all resolved AND that the
/// monitored fence exists -- three facts a `flip_wait_state == 1` sentinel
/// merely implied. The present path used to re-read that integer and, on `1`,
/// call `.unwrap()` on an `Option<fn>` it never re-checked, with the guarantee
/// carried by a one-line comment. That unwrap is gone rather than moved: by
/// project invariant a panic in a DDI is a silent graphics deadlock, not a
/// diagnosable failure.
///
/// Same style as `kmd_render`'s `WddmNotifyGuard`. R810.
#[derive(Clone, Copy)]
pub struct FlipWaitReady {
    pub fence: core::num::NonZeroU32,
    pub wait_cb: WaitFromGpuCb,
    pub h_rt_device: ddi::HANDLE,
}

// R818: the bridge re-derives the WDDM CPU-signal callback ABI by hand,
// because that TU compiles without the WDK headers
// (`HeliosCbSignalSyncFromCpu` in dxvk_bridge.cpp). These asserts pin the
// AUTHORITATIVE definition -- bindgen's, generated from the shipping
// d3dumddi.h -- to the same three numbers the C++ copy static_asserts against.
//
// That is the load-bearing half of the fix: a WDK revision that moves a field
// regenerates this type and fails the build HERE, instead of the bridge
// silently passing a mis-laid-out argument to dxgkrnl and signalling the flip
// fence with a garbage value. Re-verify both sides together on a WDK bump.
const _: () = assert!(
    core::mem::size_of::<ddi::D3DDDICB_SIGNALSYNCHRONIZATIONOBJECTFROMCPU>() == 24
);
const _: () = assert!(
    core::mem::offset_of!(ddi::D3DDDICB_SIGNALSYNCHRONIZATIONOBJECTFROMCPU, ObjectCount) == 0
);
const _: () = assert!(
    core::mem::offset_of!(
        ddi::D3DDDICB_SIGNALSYNCHRONIZATIONOBJECTFROMCPU,
        ObjectHandleArray
    ) == 8
);
const _: () = assert!(
    core::mem::offset_of!(
        ddi::D3DDDICB_SIGNALSYNCHRONIZATIONOBJECTFROMCPU,
        FenceValueArray
    ) == 16
);

unsafe fn flip_wait_setup(
    dev: &crate::device_funcs::HeliosDevice,
) -> Option<FlipWaitReady> {
    if let Some(ready) = dev.flip_wait.get() {
        return Some(ready);
    }
    if dev.flip_wait_disabled.get() {
        return None;
    }
    let disable = |reason: &str| {
        dev.flip_wait_disabled.set(true);
        log_error!(
            "flip-kwait DISABLED for this device: {reason} — bounded CPU gate serves"
        );
        None
    };
    if !crate::vehicle_kernel_flip_wait() {
        return disable("VehicleKernelFlipWait=0");
    }
    if dev.kt_callbacks.is_null() || dev.context.is_none() {
        return disable("no runtime callbacks/context");
    }
    let cbs = &*dev.kt_callbacks;
    let Some(create_cb) = cbs.pfnCreateSynchronizationObject2Cb else {
        return disable("pfnCreateSynchronizationObject2Cb missing");
    };
    let Some(signal_cb) = cbs.pfnSignalSynchronizationObjectFromCpuCb else {
        return disable("pfnSignalSynchronizationObjectFromCpuCb missing");
    };
    // Captured, not merely checked: the token carries the resolved callback, so
    // the present path never re-derives it and can never unwrap a None.
    let Some(wait_cb) = cbs.pfnWaitForSynchronizationObjectFromGpuCb else {
        return disable("pfnWaitForSynchronizationObjectFromGpuCb missing");
    };

    let mut arg: ddi::D3DDDICB_CREATESYNCHRONIZATIONOBJECT2 = core::mem::zeroed();
    arg.Info.Type = ddi::_D3DDDI_SYNCHRONIZATIONOBJECT_TYPE_D3DDDI_MONITORED_FENCE;
    arg.Info.__bindgen_anon_1.MonitoredFence.InitialFenceValue = 0;
    let hr = create_cb(dev.h_rt_device, &mut arg);
    if hr < 0 {
        return disable(&format!(
            "CreateSynchronizationObject2Cb hr=0x{:08x}",
            hr as u32
        ));
    }
    let h_fence = arg.hSyncObject;
    let cpu_va = arg
        .Info
        .__bindgen_anon_1
        .MonitoredFence
        .FenceValueCPUVirtualAddress as usize;
    let (Some(fence), true) = (core::num::NonZeroU32::new(h_fence), cpu_va != 0) else {
        return disable("monitored fence returned no handle/CPU VA");
    };

    if !dev.dxvk.present_flip_wait_setup(
        signal_cb as usize,
        dev.h_rt_device as usize,
        h_fence,
        cpu_va,
    ) {
        return disable("bridge setup refused (present fence path disabled?)");
    }
    let ready = FlipWaitReady {
        fence,
        wait_cb,
        h_rt_device: dev.h_rt_device,
    };
    dev.flip_wait.set(Some(ready));
    log_error!(
        "flip-kwait READY: runtime-device fence 0x{h_fence:x} — vehicle flips are \
         kernel-ordered on the copy's completion (CPU gate retired for this device)"
    );
    Some(ready)
}

/// Backing for the `helios_umd_set_present_source` C export.
pub fn set_present_source(
    resid: u32,
    fence_value: u64,
    width: u32,
    height: u32,
    dxgi_format: u32,
    alloc_size: u64,
    memory_type_index: u32,
) -> i32 {
    if resid == 0 || width == 0 || height == 0 || dxgi_format == 0 {
        EXT_SOURCE_REFUSED.fetch_add(1, Ordering::Relaxed);
        log_error!(
            "set_present_source REFUSED: resid={} {}x{} fmt={}",
            resid, width, height, dxgi_format
        );
        return -1;
    }
    let prev = VEHICLE.with(|c| {
        c.replace(VehicleSlot::Armed(PresentSource {
            resid,
            fence_value,
            width,
            height,
            dxgi_format,
            alloc_size,
            memory_type_index,
        }))
    });
    match prev {
        VehicleSlot::Armed(_) => {
            // A pending source nobody consumed: a Present() that never reached
            // our DDI, or a same-thread-contract violation. Count loudly; the
            // new source replaces it.
            let n = EXT_OVERWRITES.fetch_add(1, Ordering::Relaxed);
            if n < 16 || n % 512 == 0 {
                log_error!(
                    "set_present_source: overwrote a pending source (x{})",
                    n + 1
                );
            }
            1
        }
        VehicleSlot::Minted {
            result: Some(_), ..
        } => {
            // Arming over a result the ICD never consumed. Counted on the SAME
            // counter the next present's overwrite used to hit, so the total
            // stays one increment per lost result rather than moving between
            // counters.
            let n = EXT_RESULT_OVERWRITES.fetch_add(1, Ordering::Relaxed);
            if n < 16 || n % 512 == 0 {
                log_error!(
                    "vehicle present: unconsumed present result overwritten (x{})",
                    n + 1
                );
            }
            0
        }
        VehicleSlot::Idle | VehicleSlot::Minted { result: None, .. } => 0,
    }
}

/// Backing for the `helios_umd_wait_last_present` C export: bounded wait for
/// the last vehicle present's submission (frame copy included) to complete
/// on the GPU. 0 = complete, 1 = timeout, -1 = no vehicle present recorded
/// on this thread.
pub fn wait_last_present(timeout_us: u32) -> i32 {
    let dev_ptr = match VEHICLE.with(|c| c.get()) {
        VehicleSlot::Minted { device, .. } => device,
        VehicleSlot::Idle | VehicleSlot::Armed(_) => return -1,
    };
    if dev_ptr == 0 {
        return -1;
    }
    if !device_is_live(dev_ptr) {
        // The recorded device was destroyed without this slot being cleared —
        // dxgkrnl may already have reused its private block. Refuse rather than
        // dereference it, and drop the slot so the refusal is not repeated.
        VEHICLE.with(|c| c.set(VehicleSlot::Idle));
        let n = EXT_WAIT_DEAD_DEVICE.fetch_add(1, Ordering::Relaxed);
        if n < 16 || n % 512 == 0 {
            log_error!(
                "wait_last_present REFUSED: recorded device 0x{dev_ptr:x} is no longer live (x{})",
                n + 1
            );
        }
        return -1;
    }
    // SAFETY: same-thread contract — the ICD calls this immediately after
    // the vehicle Present() returned on this thread, so the device the
    // present ran on is still alive (the ICD holds the vehicle D3D11
    // device reference) — now backed by the liveness check above rather than
    // by that contract alone.
    let dev = unsafe { &*(dev_ptr as *const HeliosDevice) };
    if dev.dxvk.present_frame_gate(timeout_us) {
        0
    } else {
        1
    }
}

/// Backing for the `helios_umd_get_present_result` C export: take the last
/// minted vehicle present's (fenceId, value). 0 = taken, -1 = none pending
/// (failed present, publish unavailable, or contract violation — counted;
/// the ICD falls back to the serial wait).
pub fn take_present_result(fence_id: &mut u32, value: &mut u64) -> i32 {
    let taken = VEHICLE.with(|c| match c.get() {
        VehicleSlot::Minted {
            device,
            result: Some(pending),
        } => {
            // Consuming the result leaves the device recorded: a following
            // wait_last_present still targets the present that minted it.
            c.set(VehicleSlot::Minted {
                device,
                result: None,
            });
            Some(pending)
        }
        _ => None,
    });
    match taken {
        Some((fid, val)) => {
            *fence_id = fid;
            *value = val;
            0
        }
        None => {
            let n = EXT_RESULT_MISSES.fetch_add(1, Ordering::Relaxed);
            if n < 16 || n % 512 == 0 {
                log_error!("get_present_result: none pending (x{})", n + 1);
            }
            -1
        }
    }
}

/// The vehicle present body: cached alias-import of the ICD frame, image
/// copy into the backbuffer, backbuffer publish with this device's fence.
/// Returns (published sync value, fence name id) — both 0 when the publish
/// path is unavailable; on error the caller must FAIL the present (no
/// pfnPresentCb) so the ICD latches its sw fallback instead of flipping a
/// stale backbuffer.
unsafe fn vehicle_present_prepare(
    h: Hdevice,
    backbuffer_h: ddi::D3D10DDI_HRESOURCE,
    info: &PresentSource,
) -> Result<(u64, u32), i32> {
    let Some(dev) = helios_device(h) else {
        EXT_NO_DEVICE.fetch_add(1, Ordering::Relaxed);
        log_error!("vehicle present FAILED: no Helios device");
        return Err(E_FAIL);
    };
    let backbuffer_raw = resource_com_raw(backbuffer_h);
    if backbuffer_raw == 0 {
        EXT_NO_DEVICE.fetch_add(1, Ordering::Relaxed);
        log_error!("vehicle present FAILED: backbuffer has no COM resource");
        return Err(E_FAIL);
    }

    // Cached alias-import by resid; geometry/format change invalidates the
    // entry (swapchain recreates give new resids, so also cap the cache).
    let mut imported_raw = {
        let mut cache = dev.owned.present_src_cache.borrow_mut();
        match cache.iter().position(|e| e.resid == info.resid) {
            Some(pos)
                if cache[pos].width == info.width
                    && cache[pos].height == info.height
                    && cache[pos].dxgi_format == info.dxgi_format =>
            {
                cache[pos].resource_raw
            }
            Some(pos) => {
                cache.remove(pos); // drop releases the stale import
                0
            }
            None => 0,
        }
    };
    if imported_raw == 0 {
        let opened = dev.dxvk.open_texture2d(
            info.width,
            info.height,
            info.dxgi_format,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            0,
            // `global` is log-only in the bridge but must be nonzero; there
            // is no KMT handle on this in-process path — carry the resid.
            info.resid,
            info.resid,
            info.alloc_size,
            info.memory_type_index,
            // Not the DWM scan-out primary import; keep the plain OPTIMAL path.
            false,
            false,
            false,
        );
        let Some(imported) = opened else {
            let n = EXT_IMPORT_FAILS.fetch_add(1, Ordering::Relaxed);
            if n < 16 || n % 512 == 0 {
                log_error!(
                    "vehicle present FAILED: import resid={} {}x{} fmt={} alloc={} type={} (x{})",
                    info.resid,
                    info.width,
                    info.height,
                    info.dxgi_format,
                    info.alloc_size,
                    info.memory_type_index,
                    n + 1
                );
            }
            return Err(E_FAIL);
        };
        // PresentSrcEntry owns the reference from here; into_raw hands it over
        // so the adopted wrapper does not release it on drop.
        let raw = imported.into_raw() as usize;
        let mut cache = dev.owned.present_src_cache.borrow_mut();
        if cache.len() >= 16 {
            cache.remove(0);
        }
        cache.push(crate::device_funcs::PresentSrcEntry {
            resid: info.resid,
            width: info.width,
            height: info.height,
            dxgi_format: info.dxgi_format,
            resource_raw: raw,
        });
        imported_raw = raw;
    }

    match dev
        .dxvk
        .present_vehicle_copy(DstRes(backbuffer_raw), SrcRes(imported_raw))
    {
        0 => {}
        1 => {
            EXT_GEOM_MISMATCH.fetch_add(1, Ordering::Relaxed);
        }
        rc => {
            let n = EXT_COPY_FAILS.fetch_add(1, Ordering::Relaxed);
            if n < 16 || n % 512 == 0 {
                log_error!(
                    "vehicle present FAILED: copy rc={} resid={} (x{})",
                    rc,
                    info.resid,
                    n + 1
                );
            }
            return Err(E_FAIL);
        }
    }

    // Publish the BACKBUFFER slot with this device's own fence, recorded
    // AFTER the copy on the open command list so the value orders the copy.
    // The kwait advertisement is optimistic: setup succeeding means the
    // caller WILL queue the dxgkrnl wait for this present; a per-present
    // queue failure (kwait_queue_fails, 0 observed live) degrades one frame
    // to consumer-side bounded semantics — counted, self-healing.
    let kwait_ordered = flip_wait_setup(dev).is_some();
    let mut sync_value = 0u64;
    let mut fence_id = 0u32;
    if present_sync_publish_enabled() {
        sync_value = dev
            .dxvk
            .present_sync_publish(SrcRes(backbuffer_raw), DstRes(0), kwait_ordered);
        if sync_value != 0 {
            fence_id = dev.dxvk.present_sync_fence_id();
        }
    }
    Ok((sync_value, fence_id))
}

// --- DXGI present -----------------------------------------------------------

unsafe fn dxgi_device_handle(h: ddi::DXGI_DDI_HDEVICE) -> Hdevice {
    Hdevice {
        pDrvPrivate: h as *mut c_void,
    }
}

unsafe fn dxgi_resource_handle(h: ddi::DXGI_DDI_HRESOURCE) -> ddi::D3D10DDI_HRESOURCE {
    ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: h as *mut c_void,
    }
}

unsafe fn maybe_log_present_readback(h: Hdevice, src_h: ddi::D3D10DDI_HRESOURCE) {
    if !present_readback_enabled() {
        return;
    }
    let n = PRESENT_READBACK_LOG_COUNT.next();
    if n >= 8 {
        return;
    }
    let Some(device) = d3d11_device(h) else {
        log_error!("DXGI Present readback: no D3D11 device");
        return;
    };
    let Some(context) = d3d11_context(h) else {
        log_error!("DXGI Present readback: no D3D11 context");
        return;
    };
    let Some(res) = load_resource(src_h) else {
        log_error!("DXGI Present readback: source resource missing");
        return;
    };
    let Ok(tex) = (*res).cast::<ID3D11Texture2D>() else {
        log_error!("DXGI Present readback: source is not Texture2D");
        return;
    };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    tex.GetDesc(&mut desc);
    if desc.Width == 0 || desc.Height == 0 || desc.SampleDesc.Count != 1 {
        log_error!(
            "DXGI Present readback: unsupported {}x{} fmt={} sample={}x{}",
            desc.Width, desc.Height, desc.Format.0, desc.SampleDesc.Count, desc.SampleDesc.Quality
        );
        return;
    }

    let mut staging_desc = desc;
    staging_desc.MipLevels = 1;
    staging_desc.ArraySize = 1;
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.BindFlags = 0;
    staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    staging_desc.MiscFlags = 0;
    let mut staging: Option<ID3D11Texture2D> = None;
    if let Err(e) = device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) {
        log_error!(
            "DXGI Present readback: staging create failed {e:?}"
        );
        return;
    }
    let Some(staging) = staging else {
        log_error!("DXGI Present readback: staging create returned None");
        return;
    };
    let Ok(staging_res) = staging.cast::<ID3D11Resource>() else {
        log_error!("DXGI Present readback: staging cast failed");
        return;
    };
    context.CopyResource(&staging_res, &*res);

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    if let Err(e) = context.Map(&staging_res, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) {
        log_error!("DXGI Present readback: map failed {e:?}");
        return;
    }
    let bpp = dxgi_bytes_per_pixel(desc.Format.0 as u32).max(1) as usize;
    let row_pitch = mapped.RowPitch as usize;
    let data = mapped.pData as *const u8;
    let mut sum: u64 = 0;
    let mut nonzero = 0u32;
    for y in 0..4u32 {
        for x in 0..4u32 {
            let sx = ((desc.Width - 1) as u64 * x as u64 / 3) as usize;
            let sy = ((desc.Height - 1) as u64 * y as u64 / 3) as usize;
            let p = data.add(sy * row_pitch + sx * bpp);
            let mut px = 0u32;
            for c in 0..bpp.min(4) {
                let v = *p.add(c) as u32;
                px |= v << (c * 8);
                sum += v as u64;
            }
            if px != 0 {
                nonzero += 1;
            }
        }
    }
    let cx = (desc.Width / 2) as usize;
    let cy = (desc.Height / 2) as usize;
    let cp = data.add(cy * row_pitch + cx * bpp);
    let mut center = 0u32;
    for c in 0..bpp.min(4) {
        center |= (*cp.add(c) as u32) << (c * 8);
    }
    let mut frame_sum: u64 = 0;
    let mut frame_nonzero = 0u64;
    if std::env::var_os("HELIOS_PRESENT_DUMP_DIR").is_some() {
        for y in 0..desc.Height as usize {
            for x in 0..desc.Width as usize {
                let p = data.add(y * row_pitch + x * bpp);
                let mut px = 0u32;
                for c in 0..bpp.min(4) {
                    let v = *p.add(c) as u32;
                    px |= v << (c * 8);
                    frame_sum = frame_sum.wrapping_add(v as u64);
                }
                if px != 0 {
                    frame_nonzero += 1;
                }
            }
        }
        if bpp >= 4 {
            if let Some(dir) = std::env::var_os("HELIOS_PRESENT_DUMP_DIR") {
                let _ = std::fs::create_dir_all(&dir);
                let pid = std::process::id();
                let path = std::path::PathBuf::from(dir).join(format!(
                    "present-{pid}-{:03}-{}x{}-fmt{}.bmp",
                    n + 1,
                    desc.Width,
                    desc.Height,
                    desc.Format.0
                ));
                if let Err(e) = write_bgra32_bmp(&path, data, row_pitch, desc.Width, desc.Height) {
                    log_error!("DXGI Present readback dump failed: {e}");
                } else {
                    log_error!("DXGI Present readback dump: {}", path.display());
                }
            }
        } else {
            log_error!(
                "DXGI Present readback dump skipped: bpp={} unsupported",
                bpp
            );
        }
    }
    context.Unmap(&staging_res, 0);
    log_error!(
        "DXGI Present readback #{}: {}x{} fmt={} bpp={} grid_sum={} nonzero={} center=0x{:08x} frame_sum={} frame_nonzero={}",
        n + 1,
        desc.Width,
        desc.Height,
        desc.Format.0,
        bpp,
        sum,
        nonzero,
        center,
        frame_sum,
        frame_nonzero
    );
}

unsafe fn write_bgra32_bmp(
    path: &std::path::Path,
    data: *const u8,
    row_pitch: usize,
    width: u32,
    height: u32,
) -> std::io::Result<()> {
    use std::io::Write;

    let row_bytes = width as usize * 4;
    let image_size = row_bytes * height as usize;
    let file_size = 14usize + 40usize + image_size;

    let mut file = std::fs::File::create(path)?;
    file.write_all(b"BM")?;
    file.write_all(&(file_size as u32).to_le_bytes())?;
    file.write_all(&0u16.to_le_bytes())?;
    file.write_all(&0u16.to_le_bytes())?;
    file.write_all(&54u32.to_le_bytes())?;

    file.write_all(&40u32.to_le_bytes())?;
    file.write_all(&(width as i32).to_le_bytes())?;
    // Negative height stores top-down rows, matching D3D's mapped row order.
    file.write_all(&(-(height as i32)).to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&32u16.to_le_bytes())?;
    file.write_all(&0u32.to_le_bytes())?;
    file.write_all(&(image_size as u32).to_le_bytes())?;
    file.write_all(&2835i32.to_le_bytes())?;
    file.write_all(&2835i32.to_le_bytes())?;
    file.write_all(&0u32.to_le_bytes())?;
    file.write_all(&0u32.to_le_bytes())?;

    for y in 0..height as usize {
        let row = std::slice::from_raw_parts(data.add(y * row_pitch), row_bytes);
        file.write_all(row)?;
    }

    Ok(())
}

unsafe fn maybe_force_present_alpha_opaque(h: Hdevice, src_h: ddi::D3D10DDI_HRESOURCE) {
    if !present_force_opaque_enabled() {
        return;
    }

    let n = PRESENT_FORCE_OPAQUE_LOG_COUNT.next();
    let Some(device) = d3d11_device(h) else {
        if n < 8 {
            log_error!("DXGI Present force-opaque: no D3D11 device");
        }
        return;
    };
    let Some(context) = d3d11_context(h) else {
        if n < 8 {
            log_error!("DXGI Present force-opaque: no D3D11 context");
        }
        return;
    };
    let Some(res) = load_resource(src_h) else {
        if n < 8 {
            log_error!("DXGI Present force-opaque: source resource missing");
        }
        return;
    };
    let Ok(tex) = (*res).cast::<ID3D11Texture2D>() else {
        if n < 8 {
            log_error!("DXGI Present force-opaque: source is not Texture2D");
        }
        return;
    };

    let mut desc = D3D11_TEXTURE2D_DESC::default();
    tex.GetDesc(&mut desc);
    let bpp = dxgi_bytes_per_pixel(desc.Format.0 as u32);
    if desc.Width == 0 || desc.Height == 0 || desc.SampleDesc.Count != 1 || bpp != 4 {
        if n < 8 {
            log_error!(
                "DXGI Present force-opaque: unsupported {}x{} fmt={} bpp={} sample={}x{}",
                desc.Width,
                desc.Height,
                desc.Format.0,
                bpp,
                desc.SampleDesc.Count,
                desc.SampleDesc.Quality
            );
        }
        return;
    }

    let mut staging_desc = desc;
    staging_desc.MipLevels = 1;
    staging_desc.ArraySize = 1;
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.BindFlags = 0;
    staging_desc.CPUAccessFlags = (D3D11_CPU_ACCESS_READ.0 | D3D11_CPU_ACCESS_WRITE.0) as u32;
    staging_desc.MiscFlags = 0;

    let mut staging: Option<ID3D11Texture2D> = None;
    if let Err(e) = device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) {
        if n < 8 {
            log_error!(
                "DXGI Present force-opaque: staging create failed {e:?}"
            );
        }
        return;
    }
    let Some(staging) = staging else {
        if n < 8 {
            log_error!("DXGI Present force-opaque: staging create returned None");
        }
        return;
    };
    let Ok(staging_res) = staging.cast::<ID3D11Resource>() else {
        if n < 8 {
            log_error!("DXGI Present force-opaque: staging cast failed");
        }
        return;
    };

    context.CopyResource(&staging_res, &*res);

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    if let Err(e) = context.Map(&staging_res, 0, D3D11_MAP_READ_WRITE, 0, Some(&mut mapped)) {
        if n < 8 {
            log_error!("DXGI Present force-opaque: map failed {e:?}");
        }
        return;
    }

    let row_pitch = mapped.RowPitch as usize;
    let data = mapped.pData as *mut u8;
    let mut alpha_zero = 0u64;
    let mut alpha_non_opaque = 0u64;
    for y in 0..desc.Height as usize {
        for x in 0..desc.Width as usize {
            let alpha = data.add(y * row_pitch + x * 4 + 3);
            let old = *alpha;
            if old == 0 {
                alpha_zero += 1;
            }
            if old != 0xff {
                alpha_non_opaque += 1;
                *alpha = 0xff;
            }
        }
    }
    context.Unmap(&staging_res, 0);
    context.CopyResource(&*res, &staging_res);
    context.Flush();

    if n < 8 || (n + 1) % 512 == 0 {
        log_error!(
            "DXGI Present force-opaque #{}: {}x{} fmt={} alpha_zero={} alpha_non_opaque={}",
            n + 1,
            desc.Width,
            desc.Height,
            desc.Format.0,
            alpha_zero,
            alpha_non_opaque
        );
    }
}

#[derive(Clone, Copy)]
struct RuntimePresentDependencies {
    source: core::num::NonZeroU32,
    destination: Option<core::num::NonZeroU32>,
}

impl RuntimePresentDependencies {
    fn new(source: ddi::D3DKMT_HANDLE, destination: ddi::D3DKMT_HANDLE) -> Option<Self> {
        Some(Self {
            source: core::num::NonZeroU32::new(source)?,
            destination: core::num::NonZeroU32::new(destination),
        })
    }

    fn count(self) -> u32 {
        1 + u32::from(self.destination.is_some())
    }

    /// Populate the runtime-owned legacy allocation list used by pfnRenderCb.
    ///
    /// # Safety
    /// The list pointer and capacity came from pfnCreateContextCb or the
    /// preceding successful pfnRenderCb. This method validates both before
    /// writing exactly `count()` initialized entries.
    unsafe fn write_to(self, ctx: &crate::device_funcs::RuntimeContext) -> Result<u32, i32> {
        let required = self.count();
        // Pointer and capacity arrive together, so the `<` comparison cannot be
        // made against a capacity that describes a different pointer. The
        // comparison itself, and `required`, are unchanged from pre-R808.
        let window = ctx.allocations.get();
        let list = window.map_or(core::ptr::null_mut(), |w| w.ptr.as_ptr());
        let capacity = window.map_or(0, |w| w.capacity);
        if list.is_null() || capacity < required {
            log_error!(
                "DXGI Present: runtime allocation list unavailable ptr={:p} capacity={} required={}",
                list,
                capacity,
                required
            );
            return Err(E_FAIL);
        }

        let mut source = ddi::D3DDDI_ALLOCATIONLIST::default();
        source.hAllocation = self.source.get();
        // Value bit 0 is WriteOperation. The present source is read-only.
        source.__bindgen_anon_1.Value = 0;
        list.write(source);

        if let Some(destination) = self.destination {
            let mut entry = ddi::D3DDDI_ALLOCATIONLIST::default();
            entry.hAllocation = destination.get();
            // The present destination is written by the copy operation.
            entry.__bindgen_anon_1.Value = 1;
            list.add(1).write(entry);
        }

        Ok(required)
    }
}

#[derive(Clone, Copy)]
enum RuntimeSubmission {
    /// A DXGI present carrying the scan-out identity, written as a
    /// `HeliosPresentRenderCmd`. The typed dependency value makes a
    /// source-allocation-free present submission unrepresentable.
    TypedPresent {
        dependencies: RuntimePresentDependencies,
        private: HeliosPresentPrivateData,
    },
    /// A DXGI present with no identity to carry, written as a
    /// `HeliosPresentRefreshCmd`. Distinct from [`Self::Refresh`] because it
    /// still submits the present's allocation dependencies.
    MarkerPresent {
        dependencies: RuntimePresentDependencies,
    },
    /// An allocation-free dirty marker for an already-published scanout.
    Refresh,
}

impl RuntimeSubmission {
    /// The wire command's length and the label its log line carries.
    ///
    /// Pre-R828 the enum had two variants for three commands: `Present` with
    /// `private: Option<_>` selected the command type by an inner match, so the
    /// length and the bytes written were decided in two separate places, and
    /// BOTH present arms produced the label "Present". The labels are kept
    /// EXACTLY as they were -- TypedPresent and MarkerPresent both log
    /// "Present" -- so validation stays byte-identical.
    fn command_length_and_label(&self) -> (u32, &'static str) {
        match self {
            Self::TypedPresent { .. } => (
                core::mem::size_of::<HeliosPresentRenderCmd>() as u32,
                "Present",
            ),
            Self::MarkerPresent { .. } => (
                core::mem::size_of::<HeliosPresentRefreshCmd>() as u32,
                "Present",
            ),
            Self::Refresh => (
                core::mem::size_of::<HeliosPresentRefreshCmd>() as u32,
                "refresh",
            ),
        }
    }
}

/// The allocation-free dirty marker, built in ONE place.
///
/// Pre-R828 this literal appeared twice, byte for byte, in two arms of the same
/// match -- the `Present { private: None }` arm and the `Refresh` arm.
///
/// `source_index`/`destination_index` are RESERVED-ZERO on this path.
/// `submit_command.rs` validates only the magic and version and reads neither;
/// the KMD writes its own copy with real `DXGK_PRESENT_*_INDEX` values in
/// `display.rs`. Populating them here would be a wire-semantics change with no
/// reader.
fn present_refresh_cmd() -> HeliosPresentRefreshCmd {
    HeliosPresentRefreshCmd {
        magic: HELIOS_PRESENT_REFRESH_MAGIC,
        version: HELIOS_PRESENT_REFRESH_VERSION,
        source_index: 0,
        destination_index: 0,
    }
}

/// Submit a runtime-owned WDDM command buffer.
///
/// The legacy pfnRenderCb allocation list is mandatory for a DXGI present even
/// though Helios's marker contains no guest GPU address. VidMm uses that list
/// to make the present source/destination resident and keep them live through
/// the pending operation. A standalone refresh has no pending allocation and
/// deliberately submits an empty list.
unsafe fn submit_runtime_submission(
    dev: &crate::device_funcs::HeliosDevice,
    submission: RuntimeSubmission,
) -> i32 {
    static LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

    let (Some(ctx), false) = (dev.context.as_ref(), dev.kt_callbacks.is_null()) else {
        return E_FAIL;
    };
    let Some(render_cb) = (*dev.kt_callbacks).pfnRenderCb else {
        log_error!("DXGI submission: pfnRenderCb missing");
        return E_FAIL;
    };
    let command_window = ctx.command.get();
    let command = command_window.map_or(core::ptr::null_mut(), |w| w.ptr.as_ptr());
    let (command_length, label) = submission.command_length_and_label();
    if command.is_null() || command_window.map_or(0, |w| w.capacity) < command_length {
        log_error!("DXGI {label}: no runtime command buffer");
        return E_FAIL;
    }

    // Exactly one write per command type. A variant that writes the wrong
    // command is no longer representable: the length above and the bytes below
    // are both derived from the same variant.
    let allocation_count = match submission {
        RuntimeSubmission::TypedPresent {
            dependencies,
            private,
        } => {
            let count = match dependencies.write_to(ctx) {
                Ok(count) => count,
                Err(hr) => return hr,
            };
            (command as *mut HeliosPresentRenderCmd).write_unaligned(HeliosPresentRenderCmd {
                magic: HELIOS_PRESENT_RENDER_MAGIC,
                version: HELIOS_PRESENT_RENDER_VERSION,
                present: private,
            });
            count
        }
        RuntimeSubmission::MarkerPresent { dependencies } => {
            let count = match dependencies.write_to(ctx) {
                Ok(count) => count,
                Err(hr) => return hr,
            };
            (command as *mut HeliosPresentRefreshCmd).write_unaligned(present_refresh_cmd());
            count
        }
        RuntimeSubmission::Refresh => {
            (command as *mut HeliosPresentRefreshCmd).write_unaligned(present_refresh_cmd());
            0
        }
    };

    let mut render = ddi::D3DDDICB_RENDER::default();
    render.CommandLength = command_length;
    render.CommandOffset = 0;
    render.NumAllocations = allocation_count;
    render.NumPatchLocations = 0;
    render.hContext = ctx.handle.as_ptr();
    let hr = render_cb(dev.h_rt_device, &mut render);

    if hr >= 0 {
        // Each window is replaced as a unit, so a new pointer can never be
        // stored against the old capacity. The `!= 0` size guards are retained:
        // the runtime returning a pointer with a zero size means "keep what you
        // have", not "here is an empty buffer".
        if render.NewCommandBufferSize != 0 {
            if let Some(w) = crate::device_funcs::Window::new(
                render.pNewCommandBuffer,
                render.NewCommandBufferSize,
            ) {
                ctx.command.set(Some(w));
            }
        }
        if render.NewAllocationListSize != 0 {
            if let Some(w) = crate::device_funcs::Window::new(
                render.pNewAllocationList,
                render.NewAllocationListSize,
            ) {
                ctx.allocations.set(Some(w));
            }
        }
        if render.NewPatchLocationListSize != 0 {
            if let Some(w) = crate::device_funcs::Window::new(
                render.pNewPatchLocationList,
                render.NewPatchLocationListSize,
            ) {
                ctx.patches.set(Some(w));
            }
        }
    }

    let n = LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 64 || hr < 0 {
        log_error!(
            "DXGI {label}: pfnRenderCb hr=0x{:08x} allocations={} queued={} next_cmd={:p}/{}",
            hr as u32,
            allocation_count,
            render.QueuedBufferCount,
            render.pNewCommandBuffer,
            render.NewCommandBufferSize,
        );
    }
    hr
}

unsafe fn submit_runtime_present(
    dev: &crate::device_funcs::HeliosDevice,
    dependencies: RuntimePresentDependencies,
    private: Option<HeliosPresentPrivateData>,
) -> i32 {
    submit_runtime_submission(
        dev,
        match private {
            Some(private) => RuntimeSubmission::TypedPresent {
                dependencies,
                private,
            },
            None => RuntimeSubmission::MarkerPresent { dependencies },
        },
    )
}

/// Submit all pending WDDM render dependencies before asking dxgkrnl to
/// present them.
///
/// The DXGI DDI requires `pfnRenderCb` to precede `pfnPresentCb`.  Keeping the
/// two callbacks in one helper makes it impossible for the ordinary Present
/// and Present1 paths to accidentally reverse that ordering again.  The typed
/// dependency value also makes a source-allocation-free present
/// unrepresentable.
unsafe fn submit_runtime_present_then_call(
    dev: &crate::device_funcs::HeliosDevice,
    dependencies: RuntimePresentDependencies,
    private: Option<HeliosPresentPrivateData>,
    callback_args: &mut ddi::DXGIDDICB_PRESENT,
) -> i32 {
    if dev.dxgi_callbacks.is_null() {
        log_error!("DXGI Present: callback table missing");
        return E_FAIL;
    }
    let Some(present_cb) = (*dev.dxgi_callbacks).pfnPresentCb else {
        log_error!("DXGI Present: pfnPresentCb missing");
        return E_FAIL;
    };

    let render_hr = submit_runtime_present(dev, dependencies, private);
    if render_hr < 0 {
        return render_hr;
    }

    present_cb(dev.h_rt_device, callback_args)
}

unsafe fn submit_runtime_refresh(dev: &crate::device_funcs::HeliosDevice) -> i32 {
    submit_runtime_submission(dev, RuntimeSubmission::Refresh)
}

/// DXGI `pfnPresent`: copy the source resource to the destination resource when
/// DXGI provides both handles, then flush submitted GPU work.
unsafe extern "C" fn dxgi_present(arg: *mut ddi::DXGI_DDI_ARG_PRESENT) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    // DXGI_DDI_HDEVICE is a UINT_PTR carrying the driver device handle, the same
    // private pointer stored in D3D10DDI_HDEVICE.pDrvPrivate.
    let h = dxgi_device_handle(a.hDevice);
    let context = d3d11_context(h);
    let src_h = dxgi_resource_handle(a.hSurfaceToPresent);
    let dst_h = dxgi_resource_handle(a.hDstResource);
    let src_alloc = resource_allocation(src_h);
    let dst_alloc = resource_allocation(dst_h);
    let mut copied = false;
    let mut present_hr = 0;
    let mut sync_value: u64 = 0;

    // Dcomp present vehicle (road 4): a pending TLS source means THIS
    // present is the vehicle carrying an ICD frame — replace the normal
    // src->dst copy, publish and gate with the vehicle body; a vehicle
    // failure FAILS the present (no token minted) so the ICD latches its sw
    // fallback instead of flipping a stale backbuffer.
    let ext_source = VEHICLE.with(|c| match c.get() {
        VehicleSlot::Armed(source) => {
            // Consuming the arm returns the slot to Idle until this present
            // either mints or fails. A non-vehicle present must NOT touch a
            // pending Minted result, so only this arm writes.
            c.set(VehicleSlot::Idle);
            Some(source)
        }
        _ => None,
    });
    let is_vehicle_present = ext_source.is_some();
    let mut vehicle_fence_id: u32 = 0;

    if let Some(context) = &context {
        if let Some(ref src_info) = ext_source {
            match vehicle_present_prepare(h, src_h, src_info) {
                Ok((value, fence_id)) => {
                    sync_value = value;
                    vehicle_fence_id = fence_id;
                    copied = true;
                }
                Err(hr) => {
                    VEHICLE.with(|c| c.set(VehicleSlot::Idle));
                    return hr;
                }
            }
            let _ = unsafe { publish_dwm_composition(context, h) };
            context.Flush();
        } else {
            // A direct primary already is the scanout backing. Do not copy it
            // through the adapter-owned LINEAR target; Present will publish its
            // rotated resource id after flushing DWM's rendering.
            let mut published_to_scanout =
                presented_primary_private(h, src_h).is_some();
            let copy_pair = if published_to_scanout {
                None
            } else {
                match (
                    load_resource(dst_h),
                    load_resource(src_h),
                ) {
                    (Some(dst), Some(src)) => Some((dst, src)),
                    _ => None,
                }
            };
            if let Some((dst, src)) = copy_pair {
                context.CopySubresourceRegion(&*dst, 0, 0, 0, 0, &*src, 0, None);
                copied = true;
            } else if !published_to_scanout
                && dst_alloc == 0
                && copy_to_scanout_target(context, h, src_h)
            {
                copied = true;
                published_to_scanout = true;
            }
            // WS1 #4 producer: record the named-present-fence signal BEFORE the
            // flush so it submits WITH the frame's last work, and publish
            // (resid -> pid, value) for the IddCx consumer's bounded wait.
            // `HKLM\SOFTWARE\Helios!PresentSyncPublish = 0` kills the path.
            if present_sync_publish_enabled() {
                if let Some(dev) = helios_device(h) {
                    // Non-vehicle flips carry NO kernel wait — the IddCx
                    // consumer's bounded wait stays the orderer; never
                    // advertise kwait here (a skipping consumer on an idle
                    // desktop would freeze the host display one frame back).
                    sync_value = dev.dxvk.present_sync_publish(
                        SrcRes(resource_com_raw(src_h)),
                        DstRes(resource_com_raw(dst_h)),
                        false,
                    );
                }
            }
            // The DXGI Present source is the authoritative completed desktop.
            // Use the RTV-tracking fallback only when no present source could
            // be copied, otherwise this records the same full-frame copy twice.
            if !published_to_scanout {
                let _ = unsafe { publish_dwm_composition(context, h) };
            }
            context.Flush();
        }
    } else if is_vehicle_present {
        // No immediate context = nothing was copied or published.
        VEHICLE.with(|c| c.set(VehicleSlot::Idle));
        EXT_NO_DEVICE.fetch_add(1, Ordering::Relaxed);
        return E_FAIL;
    }

    maybe_force_present_alpha_opaque(h, src_h);
    maybe_log_present_readback(h, src_h);

    // Frame-completion gate BEFORE the kernel present becomes visible. The
    // direct-primary KMD marker can order Venus commands which have reached the
    // transport, but `context.Flush()` may return while matching work is still
    // queued on DXVK's submission thread. Waiting for DXVK's submission fence
    // closes that future-work gap before dxgkrnl publishes the primary.
    // Bounded: on timeout the present proceeds loudly and the next full-frame
    // refresh self-heals. `HKLM\SOFTWARE\Helios!PresentGateUs` (DWORD)
    // overrides the 10 ms default; 0 disables. Cost telemetry:
    // `present-gate:` lines.
    //
    // Vehicle flip ordering, kernel-enforced (25th session, replaces the
    // bounded CPU gate's leak): queue a dxgkrnl GPU-side WAIT on the flip
    // fence AHEAD of the present packet, so the flip physically cannot
    // execute before the venus copy lands — the CPU gate's 32 ms timeout
    // leaked a stale flip per expiry (A/B-proven stutter source). The flip
    // fence is CPU-signaled by the bridge when the present fence reaches
    // this present's publish value (the ICD retire thread signals that at
    // host-GPU copy completion), and a watchdog unwedges a poisoned chain.
    // ARM BEFORE QUEUE: an armed-but-unqueued signal is harmless (monotonic,
    // no waiter); a queued-but-unarmed wait would park the context forever.
    let mut kernel_wait_armed = false;
    if is_vehicle_present && sync_value != 0 {
        if let Some(dev) = helios_device(h) {
            if let Some(ready) = flip_wait_setup(dev) {
                let v = dev.flip_wait_next_value.get() + 1;
                dev.flip_wait_next_value.set(v);
                if dev.dxvk.present_flip_wait_arm(sync_value, v) {
                    let handles = [ready.fence.get()];
                    let values = [v];
                    let mut warg: ddi::D3DDDICB_WAITFORSYNCHRONIZATIONOBJECTFROMGPU =
                        core::mem::zeroed();
                    warg.hContext = dev
                        .context
                        .as_ref()
                        .map_or(core::ptr::null_mut(), |c| c.handle.as_ptr());
                    warg.ObjectCount = 1;
                    warg.ObjectHandleArray = handles.as_ptr();
                    warg.__bindgen_anon_1.MonitoredFenceValueArray = values.as_ptr();
                    // The callback comes from the token, which is why there is
                    // no `.unwrap()` here any more and no comment vouching for
                    // one. R810.
                    let hr = (ready.wait_cb)(ready.h_rt_device, &warg);
                    if hr >= 0 {
                        EXT_KWAIT_ARMED.fetch_add(1, Ordering::Relaxed);
                        kernel_wait_armed = true;
                    } else {
                        let n = EXT_KWAIT_QUEUE_FAILS.fetch_add(1, Ordering::Relaxed);
                        if n < 16 || n % 512 == 0 {
                            log_error!(
                                "flip-kwait: GPU-wait queue FAILED hr=0x{:08x} (x{}) — \
                                 CPU gate serves this present",
                                hr as u32,
                                n + 1
                            );
                        }
                    }
                } else {
                    EXT_KWAIT_ARM_FAILS.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    // Bounded CPU gate (`VehicleFlipGateUs` / `PresentGateUs`): the fallback
    // ordering when the vehicle kernel wait is off/unavailable/refused, and
    // the producer-ordering gate for direct-primary/non-vehicle presents.
    // Timeout = proceed loudly (a stale frame beats a wedged worker); the
    // vehicle kernel-wait path has no such leak.
    let gate_us = if is_vehicle_present {
        crate::vehicle_flip_gate_us()
    } else {
        present_gate_us()
    };
    if !kernel_wait_armed && gate_us != 0 {
        if let Some(dev) = helios_device(h) {
            let _outcome = run_present_frame_gate(dev, gate_us, is_vehicle_present);
        }
    }

    if let Some(dev) = helios_device(h) {
        if let Ok(ready) = present_prerequisites(dev, src_alloc) {
            let mut cb = ddi::DXGIDDICB_PRESENT::default();
            let present_private = presented_primary_private(h, src_h);
            cb.hSrcAllocation = ready.src_alloc.get();
            cb.hDstAllocation = dst_alloc;
            cb.pDXGIContext = a.pDXGIContext;
            cb.hContext = ready.h_context.as_ptr();
            cb.BroadcastContextCount = 0;
            if let Some(ref private) = present_private {
                cb.PrivateDriverDataSize = core::mem::size_of::<HeliosPresentPrivateData>() as u32;
                cb.pPrivateDriverData = (private as *const HeliosPresentPrivateData)
                    .cast_mut()
                    .cast();
            } else {
                cb.PrivateDriverDataSize = 0;
                cb.pPrivateDriverData = core::ptr::null_mut();
            }
            cb.bOptimizeForComposition = if present_optimize_composition_enabled() {
                1
            } else {
                0
            };
            let Some(dependencies) = RuntimePresentDependencies::new(src_alloc, dst_alloc) else {
                log_error!("DXGI Present: nonzero source allocation invariant lost");
                return E_FAIL;
            };
            if let Some(cb_n) = PRESENT_CB_LOG_COUNT.first_n_then_every_from_one(128, 512) {
                let (src_rt, src_km) = resource_parent_handles(src_h);
                let (dst_rt, dst_km) = resource_parent_handles(dst_h);
                trace_line!(
                    "DXGI PresentCb identity: #{} src_alloc=0x{:x} dst_alloc=0x{:x} \
                     src_hDrv={:p} src_hRT={:p} src_hKM=0x{:x} dst_hDrv={:p} \
                     dst_hRT={:p} dst_hKM=0x{:x} hContext={:p} dxgi_context={:p} \
                     flags=0x{:x} broadcast={} private={:p}/{} optimize={}",
                    cb_n,
                    cb.hSrcAllocation,
                    cb.hDstAllocation,
                    src_h.pDrvPrivate,
                    src_rt,
                    src_km,
                    dst_h.pDrvPrivate,
                    dst_rt,
                    dst_km,
                    cb.hContext,
                    cb.pDXGIContext,
                    *(&a.Flags as *const ddi::DXGI_DDI_PRESENT_FLAGS as *const u32),
                    cb.BroadcastContextCount,
                    cb.pPrivateDriverData,
                    cb.PrivateDriverDataSize,
                    cb.bOptimizeForComposition,
                );
            }
            present_hr =
                submit_runtime_present_then_call(dev, dependencies, present_private, &mut cb);
        } else {
            // Rate cap: same message text and field set, fewer lines. Which of
            // the three preconditions failed now lives in the counters below.
            if PRESENT_SKIP_LOG_COUNT.first_n_then_every_from_one(64, 512).is_some() {
                log_error!(
                    "DXGI Present: skip PresentCb callbacks={} src=0x{:x} hContext={:p}",
                    dev.dxgi_callbacks.is_null(),
                    src_alloc,
                    dev.context
                        .as_ref()
                        .map_or(core::ptr::null_mut(), |c| c.handle.as_ptr())
                );
            }
        }
    }

    if is_vehicle_present {
        // The recycle-gate result: only a minted present with a live publish
        // carries one — otherwise leave None so the ICD's consume MISSES and
        // it falls back to the serial wait. wait_last_present targets the
        // device recorded here, and the two now move together by construction.
        let result =
            (vehicle_fence_id != 0 && sync_value != 0).then_some((vehicle_fence_id, sync_value));
        let prev = VEHICLE.with(|c| {
            c.replace(VehicleSlot::Minted {
                device: h.pDrvPrivate as usize,
                result,
            })
        });
        if matches!(
            prev,
            VehicleSlot::Minted {
                result: Some(_),
                ..
            }
        ) {
            let n = EXT_RESULT_OVERWRITES.fetch_add(1, Ordering::Relaxed);
            if n < 16 || n % 512 == 0 {
                log_error!(
                    "vehicle present: unconsumed present result overwritten (x{})",
                    n + 1
                );
            }
        }
        let n = EXT_PRESENTS.fetch_add(1, Ordering::Relaxed);
        if n < 4 || (n + 1) % 512 == 0 {
            log_error!(
                "vehicle present #{}: imports_failed={} copies_failed={} geom_mismatch={} \
                 overwrites={} kwait_armed={} kwait_arm_fails={} kwait_queue_fails={}",
                n + 1,
                EXT_IMPORT_FAILS.load(Ordering::Relaxed),
                EXT_COPY_FAILS.load(Ordering::Relaxed),
                EXT_GEOM_MISMATCH.load(Ordering::Relaxed),
                EXT_OVERWRITES.load(Ordering::Relaxed),
                EXT_KWAIT_ARMED.load(Ordering::Relaxed),
                EXT_KWAIT_ARM_FAILS.load(Ordering::Relaxed),
                EXT_KWAIT_QUEUE_FAILS.load(Ordering::Relaxed),
            );
        }
    }

    // Forensics for the DWM indirect-swapchain flip-present failure (3 OK then
    // 0x80070057): log the rotating runtime resource handle vs our collapsed
    // allocation handle, subresource indices, raw flags and flip interval, and
    // a per-process present ordinal so cycles can be told apart.
    static PRESENT_ORDINAL: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let ordinal = PRESENT_ORDINAL.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if ordinal < 64 || (ordinal + 1) % 512 == 0 {
        log_error!(
            "DXGI Present: #{} src=0x{:x} dst=0x{:x} copied={} flags=0x{:x} opt_comp={} presentCb=0x{:08x} \
             hSurf={:p} srcSub={} hDstRes={:p} dstSub={} flipInterval={} dxgiCtx={:p} hContext={:p} \
             syncVal={} skips={}/{}/{} gate_nc={}",
            ordinal,
            src_alloc,
            dst_alloc,
            copied,
            *(&a.Flags as *const ddi::DXGI_DDI_PRESENT_FLAGS as *const u32),
            present_optimize_composition_enabled() as u32,
            present_hr as u32,
            src_h.pDrvPrivate,
            a.SrcSubResourceIndex,
            dst_h.pDrvPrivate,
            a.DstSubResourceIndex,
            a.FlipInterval,
            a.pDXGIContext,
            dev_context_for_log(h),
            sync_value,
            PRESENT_SKIP_NO_CALLBACKS.load(Ordering::Relaxed),
            PRESENT_SKIP_NO_CONTEXT.load(Ordering::Relaxed),
            PRESENT_SKIP_NO_SRC_ALLOC.load(Ordering::Relaxed),
            PRESENT_GATE_TIMEOUTS.load(Ordering::Relaxed),
        );
    }
    present_hr
}

// Best-effort context handle for present logging (null when unavailable).
fn dev_context_for_log(h: ddi::D3D10DDI_HDEVICE) -> *mut core::ffi::c_void {
    unsafe {
        helios_device(h).map_or(core::ptr::null_mut(), |d| {
            d.context
                .as_ref()
                .map_or(core::ptr::null_mut(), |c| c.handle.as_ptr())
        })
    }
}

unsafe extern "C" fn dxgi_get_gamma_caps(
    arg: *mut ddi::DXGI_DDI_ARG_GET_GAMMA_CONTROL_CAPS,
) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let caps = (*arg).pGammaCapabilities;
    if !caps.is_null() {
        core::ptr::write_bytes(
            caps as *mut u8,
            0,
            core::mem::size_of::<ddi::DXGI_GAMMA_CONTROL_CAPABILITIES>(),
        );
        (*caps).MaxConvertedValue = 1.0;
        (*caps).MinConvertedValue = 0.0;
    }
    0
}

unsafe extern "C" fn dxgi_set_display_mode(arg: *mut ddi::DXGI_DDI_ARG_SETDISPLAYMODE) -> i32 {
    if arg.is_null() {
        return E_INVALIDARG;
    }
    let a = &*arg;
    let h = dxgi_device_handle(a.hDevice);
    let Some(dev) = helios_device(h) else {
        log_error!("DXGI SetDisplayMode: missing device");
        return E_INVALIDARG;
    };
    if dev.kt_callbacks.is_null() {
        log_error!("DXGI SetDisplayMode: missing runtime callbacks");
        return E_FAIL;
    }
    let Some(set_display_mode_cb) = (*dev.kt_callbacks).pfnSetDisplayModeCb else {
        log_error!("DXGI SetDisplayMode: pfnSetDisplayModeCb missing");
        return E_FAIL;
    };

    // Windows supplies the authoritative primary resource and subresource for
    // the fullscreen transition. Translate that exact runtime resource to the
    // allocation created for it; pfnSetDisplayModeCb then asks dxgkrnl to make
    // that allocation the scan-out primary and initiates the VidPn commit.
    let resource = dxgi_resource_handle(a.hResource);
    let allocation = resource_allocation(resource);
    if allocation == 0 {
        log_error!(
            "DXGI SetDisplayMode: resource=0x{:x} sub={} has no WDDM allocation",
            a.hResource, a.SubResourceIndex
        );
        return E_INVALIDARG;
    }

    if let Some(context) = d3d11_context(h) {
        let _ = unsafe { publish_dwm_composition(&context, h) };
        context.Flush();
    }
    let mut callback = ddi::D3DDDICB_SETDISPLAYMODE {
        hPrimaryAllocation: allocation,
        PrivateDriverFormatAttribute: 0,
    };
    let hr = set_display_mode_cb(dev.h_rt_device, &mut callback);
    log_error!(
        "DXGI SetDisplayMode: resource=0x{:x} sub={} allocation=0x{:x} hr=0x{:08x} private_format=0x{:x}",
        a.hResource,
        a.SubResourceIndex,
        allocation,
        hr as u32,
        callback.PrivateDriverFormatAttribute
    );
    hr
}

unsafe extern "C" fn dxgi_set_resource_priority(
    _arg: *mut ddi::DXGI_DDI_ARG_SETRESOURCEPRIORITY,
) -> i32 {
    0
}

unsafe extern "C" fn dxgi_query_resource_residency(
    arg: *mut ddi::DXGI_DDI_ARG_QUERYRESOURCERESIDENCY,
) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    if !a.pStatus.is_null() {
        for i in 0..a.Resources as usize {
            *a.pStatus.add(i) = ddi::DXGI_DDI_RESIDENCY_DXGI_DDI_RESIDENCY_FULLY_RESIDENT;
        }
    }
    0
}

/// DXGI flip-model identity rotation. The runtime calls this after each flip
/// present so the app's fixed buffer objects walk the swapchain's allocation
/// ring: resource[i] takes resource[i+1]'s identity, the last takes the
/// first's. Two coordinated moves keep the world consistent:
///   1. the DXVK storages (venus memory + VkImage + KMT handles) rotate in
///      the bridge, so draws through existing views land in the allocation
///      the runtime now associates with the buffer;
///   2. our per-resource WDDM {allocation, km} records rotate here, so the
///      next present reports the rotated hSrcAllocation to dxgkrnl.
/// The old Flush-only stub pinned dwm's composition to ONE allocation while
/// dxgkrnl/IddCx walked all three swapchain buffers — two of every three
/// acquired frames were buffers dwm never rendered (black IDD output).
/// Outcome of one swapchain identity rotation. Five exits used to `return 0`,
/// which the DXGI DDI reads as success; this names them instead.
#[derive(Copy, Clone, PartialEq, Eq)]
enum RotationOutcome {
    Rotated,
    /// `rotate_resource_backings` returned false — an entry with no DXVK image
    /// storage, or a `DxvkError`/unknown exception swallowed into false.
    BridgeRefused,
    /// No Helios device behind the DXGI device handle.
    NoDevice,
}

/// The DXVK backing rotation refused it.
static ROTATE_REFUSED: AtomicUsize = AtomicUsize::new(0);
/// A null resource handle or an untracked resource in the ring.
static ROTATE_UNTRACKED: AtomicUsize = AtomicUsize::new(0);
/// No Helios device behind the DXGI device handle.
static ROTATE_NO_DEVICE: AtomicUsize = AtomicUsize::new(0);
/// `Resources < 2` or a null array — the exit that had no log at all.
static ROTATE_SKIPPED: AtomicUsize = AtomicUsize::new(0);

fn rotate_counter_summary() -> String {
    format!(
        "refused={} untracked={} no_device={} skipped={}",
        ROTATE_REFUSED.load(Ordering::Relaxed),
        ROTATE_UNTRACKED.load(Ordering::Relaxed),
        ROTATE_NO_DEVICE.load(Ordering::Relaxed),
        ROTATE_SKIPPED.load(Ordering::Relaxed),
    )
}

/// Both rotation phases, with NO return path between them.
///
/// The bridge rotation and the WDDM record rotation used to be two statements
/// held in the right order purely by statement order. If the bridge refuses
/// after the records moved — or vice versa — dwm composites into an allocation
/// dxgkrnl no longer scans out, which is the historical black-IDD bug this
/// DDI's own doc comment describes.
///
/// `states` is a slice of INDEPENDENT raw pointers, so a `&mut [ResourceState]`
/// cannot be formed from it and this stays `unsafe`. Panic-free: no indexing,
/// no `unwrap` — `first`/`last`/`windows` only.
unsafe fn rotate_ring(
    dev: &crate::device_funcs::HeliosDevice,
    states: &[*mut ResourceState],
) -> RotationOutcome {
    let (Some(&first), Some(&last)) = (states.first(), states.last()) else {
        // Unreachable: the caller validated len >= 2.
        ROTATE_SKIPPED.fetch_add(1, Ordering::Relaxed);
        return RotationOutcome::BridgeRefused;
    };

    let ptrs: Vec<usize> = states.iter().map(|s| (**s).com_raw).collect();
    if !dev.dxvk.rotate_resource_backings(ptrs.as_ptr(), ptrs.len()) {
        ROTATE_REFUSED.fetch_add(1, Ordering::Relaxed);
        return RotationOutcome::BridgeRefused;
    }

    // Rotate the WDDM identity records in lockstep with the storages.
    let first_allocation = (*first).allocation.take();
    let first_km_resource = (*first).km_resource;
    // `ownership` rotates with the allocation it describes; `rt_resource`
    // deliberately does NOT rotate and is not touched here. That asymmetry is
    // why R804 keeps the discriminant a separate field rather than bundling the
    // runtime handle into the ownership enum -- a variant carrying the handle
    // would change what RotateResourceIdentities moves.
    let first_ownership = (*first).ownership;
    let first_present_private = (*first).present_private;
    for pair in states.windows(2) {
        let (Some(&cur), Some(&next)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        (*cur).allocation = (*next).allocation.take();
        (*cur).km_resource = (*next).km_resource;
        (*cur).ownership = (*next).ownership;
        // Present private data identifies the backing allocation (Venus
        // resource id, layout and extent), not the stable D3D resource object.
        // DXGI rotates that backing identity together with the allocation and
        // DXVK storage. Leaving this behind makes a flip scan out the previous
        // resource's memory after the first RotateResourceIdentities call.
        (*cur).present_private = (*next).present_private;
    }
    (*last).allocation = first_allocation;
    (*last).km_resource = first_km_resource;
    (*last).ownership = first_ownership;
    (*last).present_private = first_present_private;

    RotationOutcome::Rotated
}

unsafe extern "C" fn dxgi_rotate_resource_identities(
    arg: *mut ddi::DXGI_DDI_ARG_ROTATE_RESOURCE_IDENTITIES,
) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    let h = dxgi_device_handle(a.hDevice);
    let n = a.Resources as usize;
    if n < 2 || a.pResources.is_null() {
        let c = ROTATE_SKIPPED.fetch_add(1, Ordering::Relaxed);
        if c < 16 || c % 512 == 0 {
            log_error!(
                "DXGI RotateResourceIdentities: skipped resources={} null_array={} ({})",
                n,
                a.pResources.is_null(),
                rotate_counter_summary()
            );
        }
        return 0;
    }

    // Collect the per-resource state pointers; all entries must be tracked
    // resources or the rotation is refused whole (a partial rotation would
    // permanently corrupt the swapchain mapping).
    let mut states: Vec<*mut ResourceState> = Vec::with_capacity(n);
    for i in 0..n {
        let hr = dxgi_resource_handle(*a.pResources.add(i));
        if hr.pDrvPrivate.is_null() {
            ROTATE_UNTRACKED.fetch_add(1, Ordering::Relaxed);
            log_error!(
                "DXGI RotateResourceIdentities: null resource handle ({})",
                rotate_counter_summary()
            );
            return 0;
        }
        let state = match boxed_slot(hr) {
            Some(slot) => slot.ptr(),
            None => core::ptr::null_mut(),
        };
        if state.is_null() {
            ROTATE_UNTRACKED.fetch_add(1, Ordering::Relaxed);
            log_error!(
                "DXGI RotateResourceIdentities: untracked resource ({})",
                rotate_counter_summary()
            );
            return 0;
        }
        states.push(state);
    }

    let outcome = match helios_device(h) {
        Some(dev) => rotate_ring(dev, &states),
        None => {
            ROTATE_NO_DEVICE.fetch_add(1, Ordering::Relaxed);
            RotationOutcome::NoDevice
        }
    };
    if outcome != RotationOutcome::Rotated {
        log_error!(
            "DXGI RotateResourceIdentities: backing rotation FAILED ({})",
            rotate_counter_summary()
        );
        return 0;
    }

    if ROTATE_LOG_COUNT.first_n(64).is_some() {
        let (first_handle, first_resource_id) = match states.first() {
            Some(&first) => (
                (*first)
                    .allocation
                    .as_ref()
                    .map(ResidentAllocation::handle)
                    .unwrap_or(0),
                (*first).present_private.resource_id,
            ),
            None => (0, 0),
        };
        trace_line!(
            "DXGI RotateResourceIdentities: rotated {} resources, alloc[0]=0x{:x} scanout_res[0]={}",
            n,
            first_handle,
            first_resource_id
        );
    }
    // HRESULT unchanged: every path returned 0 before and every path returns 0
    // now. Making a refused rotation FAIL the DDI is a separate decision with
    // its own blast radius.
    0
}

static ROTATE_LOG_COUNT: LogThrottle = LogThrottle::new();
static BLT_LOG_COUNT: LogThrottle = LogThrottle::new();
static BLT1_LOG_COUNT: LogThrottle = LogThrottle::new();
static RESIDENCY_LOG_COUNT: LogThrottle = LogThrottle::new();
static MPO_LOG_COUNT: LogThrottle = LogThrottle::new();
static PRESENT1_LOG_COUNT: LogThrottle = LogThrottle::new();
static DXGI13_RESERVED_LOG_COUNT: LogThrottle = LogThrottle::new();
const DXGI_MPO_MAX_PLANES: u32 = 16;

unsafe extern "C" fn dxgi_blt(arg: *mut ddi::DXGI_DDI_ARG_BLT) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    let Some(context) = d3d11_context(dxgi_device_handle(a.hDevice)) else {
        return 0;
    };
    let dst_h = dxgi_resource_handle(a.hDstResource);
    let src_h = dxgi_resource_handle(a.hSrcResource);
    let (Some(dst), Some(src)) = (
        load_resource(dst_h),
        load_resource(src_h),
    ) else {
        log_error!(
            "DXGI Blt: missing resource dst=0x{:x} src=0x{:x}",
            a.hDstResource, a.hSrcResource
        );
        return 0;
    };

    if let Some(n) = BLT_LOG_COUNT.first_n_then_every_from_one(128, 512) {
        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        let mut dst_desc = D3D11_TEXTURE2D_DESC::default();
        let src_tex = (*src).cast::<ID3D11Texture2D>().ok();
        let dst_tex = (*dst).cast::<ID3D11Texture2D>().ok();
        if let Some(tex) = &src_tex {
            tex.GetDesc(&mut src_desc);
        }
        if let Some(tex) = &dst_tex {
            tex.GetDesc(&mut dst_desc);
        }
        trace_line!(
            "DXGI Blt: #{} src={:p}/{} alloc=0x{:x} {}x{} fmt={} -> \
             dst={:p}/{} alloc=0x{:x} {}x{} fmt={} flags=0x{:x} rotate={}",
            n,
            src_h.pDrvPrivate,
            a.SrcSubresource,
            resource_allocation(src_h),
            src_desc.Width,
            src_desc.Height,
            src_desc.Format.0,
            dst_h.pDrvPrivate,
            a.DstSubresource,
            resource_allocation(dst_h),
            dst_desc.Width,
            dst_desc.Height,
            dst_desc.Format.0,
            a.Flags.__bindgen_anon_1.Value,
            a.Rotate,
        );
    }

    // The DXGI 1.0 blit DDI has no source rectangle. For DWM/windowed present
    // setup the runtime uses it to move between compatible proxy/front-buffer
    // surfaces, so a full subresource copy is the safest baseline.
    context.CopySubresourceRegion(
        &*dst,
        a.DstSubresource,
        a.DstLeft,
        a.DstTop,
        0,
        &*src,
        a.SrcSubresource,
        None,
    );
    context.Flush();
    0
}

unsafe extern "C" fn dxgi_blt1(arg: *mut ddi::DXGI_DDI_ARG_BLT1) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    let Some(context) = d3d11_context(dxgi_device_handle(a.hDevice)) else {
        return 0;
    };
    let dst_h = dxgi_resource_handle(a.hDstResource);
    let src_h = dxgi_resource_handle(a.hSrcResource);
    let (Some(dst), Some(src)) = (
        load_resource(dst_h),
        load_resource(src_h),
    ) else {
        log_error!(
            "DXGI Blt1: missing resource dst=0x{:x} src=0x{:x}",
            a.hDstResource, a.hSrcResource
        );
        return E_INVALIDARG;
    };

    const BLT_RESOLVE: u32 = 0x1;
    const BLT_CONVERT: u32 = 0x2;
    const BLT_STRETCH: u32 = 0x4;
    let flags = a.Flags.__bindgen_anon_1.Value;
    if flags & BLT_CONVERT != 0 {
        log_error!("DXGI Blt1: convert unsupported flags=0x{flags:x}");
        return DXGI_ERROR_UNSUPPORTED;
    }

    let src_w = a.SrcRight.saturating_sub(a.SrcLeft);
    let src_h_px = a.SrcBottom.saturating_sub(a.SrcTop);
    let dst_w = a.DstRight.saturating_sub(a.DstLeft);
    let dst_h_px = a.DstBottom.saturating_sub(a.DstTop);

    if flags & BLT_RESOLVE != 0 {
        let format = resource_dxgi_format(dst_h);
        if format.0 == 0 {
            log_error!("DXGI Blt1: resolve has unknown destination format");
            return E_INVALIDARG;
        }
        context.ResolveSubresource(&*dst, a.DstSubresource, &*src, a.SrcSubresource, format);
        context.Flush();
        return 0;
    }

    if flags & BLT_STRETCH != 0
        || (src_w != 0 && dst_w != 0 && (src_w != dst_w || src_h_px != dst_h_px))
    {
        log_error!(
            "DXGI Blt1: stretch unsupported src={}x{} dst={}x{} flags=0x{flags:x}",
            src_w, src_h_px, dst_w, dst_h_px
        );
        return DXGI_ERROR_UNSUPPORTED;
    }

    let bx;
    let bx_ptr = if a.SrcRight > a.SrcLeft && a.SrcBottom > a.SrcTop {
        bx = D3D11_BOX {
            left: a.SrcLeft,
            top: a.SrcTop,
            front: 0,
            right: a.SrcRight,
            bottom: a.SrcBottom,
            back: 1,
        };
        Some(&bx as *const D3D11_BOX)
    } else {
        None
    };

    if BLT1_LOG_COUNT.first_n(32).is_some() {
        trace_line!(
            "DXGI Blt1: copy src={}x{} dst={}x{} flags=0x{flags:x}",
            src_w,
            src_h_px,
            dst_w,
            dst_h_px
        );
    }

    context.CopySubresourceRegion(
        &*dst,
        a.DstSubresource,
        a.DstLeft,
        a.DstTop,
        0,
        &*src,
        a.SrcSubresource,
        bx_ptr,
    );
    context.Flush();
    0
}

unsafe extern "C" fn dxgi_offer_resources(arg: *mut ddi::DXGI_DDI_ARG_OFFERRESOURCES) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    if RESIDENCY_LOG_COUNT.first_n(32).is_some() {
        log_error!(
            "DXGI OfferResources: resources={} priority={} (kept resident)",
            a.Resources, a.Priority
        );
    }
    0
}

unsafe extern "C" fn dxgi_reclaim_resources(arg: *mut ddi::DXGI_DDI_ARG_RECLAIMRESOURCES) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    if !a.pDiscarded.is_null() {
        for i in 0..a.Resources as usize {
            *a.pDiscarded.add(i) = 0;
        }
    }
    if RESIDENCY_LOG_COUNT.first_n(32).is_some() {
        log_error!(
            "DXGI ReclaimResources: resources={} discarded=FALSE",
            a.Resources
        );
    }
    0
}

// ---------------------------------------------------------------------------
// R830 (OWNER DECISION): name the literals, DO NOT change the values.
// ---------------------------------------------------------------------------
//
// Helios advertises MaxPlanes = 16, 16x stretch and shrink, and BILINEAR
// filtering, while the KMD deliberately does not register the MPO3 interface
// (query_adapter_info.rs pins the display surface to WDDM 2.1) and dxgi_blt1
// rejects any stretch with DXGI_ERROR_UNSUPPORTED. So these are caps with no
// kernel overlay path behind them.
//
// The review's own correction stands and is worth keeping visible: the plane
// count IS already a named constant (DXGI_MPO_MAX_PLANES), and
// `dxgi_present_mpo` forwarding only (allocation, subresource) is CORRECT --
// DXGIDDICB_PRESENT_MULTIPLANE_OVERLAY has no geometry fields at all. Plane
// attributes reach the kernel through dxgkrnl's MPO VidPn DDIs, which is
// exactly where Helios has nothing. The unjustified literals were the two 16.0
// factors, BILINEAR and NumCapabilityGroups: 1 -- named below.
//
// Reducing the advertised caps is behaviour-affecting: DWM picks its
// composition strategy from them, and the direct-primary scanout path is this
// tranche's frozen baseline. DEFERRED pending same-boot evidence on whether DWM
// queries MPO at all (zero GetMultiplaneOverlayCaps / MPO-plane lines appear in
// any UMD log on this box, but those logs predate the tranche by three weeks --
// re-sample at the gate). See the ROADMAP T5 entry.
/// The four MPO feature-cap bits Helios advertises. Hoisted to module scope by
/// R830 so `HELIOS_MPO_OVERLAY_CAPS` below can be the single composition.
const RGB: u32 =
    ddi::DXGI_DDI_MULTIPLANE_OVERLAY_FEATURE_CAPS_DXGI_DDI_MULTIPLANE_OVERLAY_FEATURE_CAPS_RGB
        as u32;
const BILINEAR: u32 = ddi::DXGI_DDI_MULTIPLANE_OVERLAY_FEATURE_CAPS_DXGI_DDI_MULTIPLANE_OVERLAY_FEATURE_CAPS_BILINEAR_FILTER
    as u32;
const SHARED: u32 =
    ddi::DXGI_DDI_MULTIPLANE_OVERLAY_FEATURE_CAPS_DXGI_DDI_MULTIPLANE_OVERLAY_FEATURE_CAPS_SHARED
        as u32;
const IMMEDIATE: u32 = ddi::DXGI_DDI_MULTIPLANE_OVERLAY_FEATURE_CAPS_DXGI_DDI_MULTIPLANE_OVERLAY_FEATURE_CAPS_IMMEDIATE
    as u32;

/// Maximum stretch the caps advertise. NOT implemented: `dxgi_blt1` refuses any
/// stretch with DXGI_ERROR_UNSUPPORTED.
const HELIOS_MPO_MAX_STRETCH: f32 = 16.0;
/// Maximum shrink the caps advertise. Same caveat as the stretch factor.
const HELIOS_MPO_MAX_SHRINK: f32 = 16.0;
/// One capability group, covering all planes.
const HELIOS_MPO_GROUPS: u32 = 1;
/// The advertised overlay feature caps. BILINEAR is the questionable member --
/// there is no filter path behind it.
const HELIOS_MPO_OVERLAY_CAPS: u32 = RGB | BILINEAR | SHARED | IMMEDIATE;

unsafe extern "C" fn dxgi_get_mpo_caps(
    arg: *mut ddi::DXGI_DDI_ARG_GETMULTIPLANEOVERLAYCAPS,
) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &mut *arg;
    a.MultiplaneOverlayCaps = ddi::DXGI_DDI_MULTIPLANE_OVERLAY_CAPS {
        MaxPlanes: DXGI_MPO_MAX_PLANES,
        NumCapabilityGroups: HELIOS_MPO_GROUPS,
    };
    if MPO_LOG_COUNT.first_n(16).is_some() {
        log_error!(
            "DXGI GetMultiplaneOverlayCaps: MaxPlanes={} groups=1",
            DXGI_MPO_MAX_PLANES
        );
    }
    0
}

unsafe extern "C" fn dxgi_get_mpo_group_caps(
    arg: *mut ddi::DXGI_DDI_ARG_GETMULTIPLANEOVERLAYGROUPCAPS,
) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &mut *arg;
    a.MultiplaneOverlayGroupCaps = if a.GroupIndex == 0 {
        ddi::DXGI_DDI_MULTIPLANE_OVERLAY_GROUP_CAPS {
            NumPlanes: DXGI_MPO_MAX_PLANES,
            MaxStretchFactor: HELIOS_MPO_MAX_STRETCH,
            MaxShrinkFactor: HELIOS_MPO_MAX_SHRINK,
            OverlayCaps: HELIOS_MPO_OVERLAY_CAPS,
            StereoCaps: 0,
        }
    } else {
        ddi::DXGI_DDI_MULTIPLANE_OVERLAY_GROUP_CAPS::default()
    };
    if MPO_LOG_COUNT.first_n(16).is_some() {
        log_error!(
            "DXGI GetMultiplaneOverlayGroupCaps: group={} planes={} caps=0x{:x}",
            a.GroupIndex,
            a.MultiplaneOverlayGroupCaps.NumPlanes,
            a.MultiplaneOverlayGroupCaps.OverlayCaps
        );
    }
    0
}

unsafe extern "C" fn dxgi_present_mpo(arg: *mut ddi::DXGI_DDI_ARG_PRESENTMULTIPLANEOVERLAY) -> i32 {
    if arg.is_null() {
        return E_INVALIDARG;
    }
    let a = &*arg;
    if a.PresentPlaneCount == 0 || a.pPresentPlanes.is_null() {
        log_error!("DXGI PresentMultiplaneOverlay: no present planes");
        return E_INVALIDARG;
    }
    if a.PresentPlaneCount > DXGI_MPO_MAX_PLANES {
        log_error!(
            "DXGI PresentMultiplaneOverlay: too many planes {}",
            a.PresentPlaneCount
        );
        return E_INVALIDARG;
    }

    let h = dxgi_device_handle(a.hDevice);
    let Some(dev) = helios_device(h) else {
        return E_INVALIDARG;
    };
    let (false, Some(ctx)) = (dev.dxgi_callbacks.is_null(), dev.context.as_ref()) else {
        log_error!("DXGI PresentMultiplaneOverlay: no DXGI callbacks/context");
        return DXGI_ERROR_UNSUPPORTED;
    };
    let Some(present_cb) = (*dev.dxgi_callbacks).pfnPresentMultiplaneOverlayCb else {
        log_error!("DXGI PresentMultiplaneOverlay: pfnPresentMultiplaneOverlayCb missing");
        return DXGI_ERROR_UNSUPPORTED;
    };

    let mut cb = ddi::DXGIDDICB_PRESENT_MULTIPLANE_OVERLAY::default();
    cb.pDXGIContext = a.pDXGIContext;
    cb.hContext = ctx.handle.as_ptr();
    cb.BroadcastContextCount = 0;

    for i in 0..a.PresentPlaneCount as usize {
        let plane = &*a.pPresentPlanes.add(i);
        let attrs = &plane.PlaneAttributes;
        if MPO_LOG_COUNT.first_n(128).is_some() {
            trace_line!(
                "DXGI MPO plane {}: enabled={} hRes=0x{:x} sub={} flags=0x{:x} \
                 src=({},{}-{}, {}) dst=({},{}-{}, {}) clip=({},{}-{}, {}) rot={} blend={} \
                 dirty={} ycbcr=0x{:x} stretch={}",
                i,
                plane.Enabled,
                plane.hResource,
                plane.SubResourceIndex,
                attrs.Flags,
                attrs.SrcRect.left,
                attrs.SrcRect.top,
                attrs.SrcRect.right,
                attrs.SrcRect.bottom,
                attrs.DstRect.left,
                attrs.DstRect.top,
                attrs.DstRect.right,
                attrs.DstRect.bottom,
                attrs.ClipRect.left,
                attrs.ClipRect.top,
                attrs.ClipRect.right,
                attrs.ClipRect.bottom,
                attrs.Rotation,
                attrs.Blend,
                attrs.DirtyRectCount,
                attrs.YCbCrFlags,
                attrs.StretchQuality
            );
        }
        if plane.Enabled == 0 {
            continue;
        }
        if cb.AllocationInfoCount as usize >= cb.AllocationInfo.len() {
            return E_INVALIDARG;
        }
        let resource = dxgi_resource_handle(plane.hResource);
        let alloc = resource_allocation(resource);
        if alloc == 0 {
            log_error!(
                "DXGI PresentMultiplaneOverlay: plane {} has no allocation hResource=0x{:x}",
                i, plane.hResource
            );
            return E_INVALIDARG;
        }
        let slot = cb.AllocationInfoCount as usize;
        cb.AllocationInfo[slot].PresentAllocation = alloc;
        cb.AllocationInfo[slot].SubResourceIndex = plane.SubResourceIndex;
        if MPO_LOG_COUNT.first_n(128).is_some() {
            trace_line!(
                "DXGI MPO plane {} -> allocation=0x{:x} slot={}",
                i,
                alloc,
                slot
            );
        }
        cb.AllocationInfoCount += 1;
    }

    if cb.AllocationInfoCount == 0 {
        log_error!("DXGI PresentMultiplaneOverlay: no enabled planes");
        return E_INVALIDARG;
    }

    if let Some(context) = d3d11_context(h) {
        let _ = unsafe { publish_dwm_composition(&context, h) };
        context.Flush();
    }

    let hr = present_cb(dev.h_rt_device, &cb);
    if MPO_LOG_COUNT.first_n(64).is_some() {
        trace_line!(
            "DXGI PresentMultiplaneOverlay: planes={} enabled={} presentCb=0x{:08x} ctx={:p}",
            a.PresentPlaneCount,
            cb.AllocationInfoCount,
            hr as u32,
            ctx.handle.as_ptr()
        );
    }
    hr
}

unsafe extern "C" fn dxgi_reserved_unsupported(_arg: *mut c_void) -> i32 {
    if DXGI13_RESERVED_LOG_COUNT.first_n(16).is_some() {
        log_error!("DXGI reserved callback -> DXGI_ERROR_UNSUPPORTED");
    }
    DXGI_ERROR_UNSUPPORTED
}

unsafe extern "C" fn dxgi_present1(arg: *mut ddi::DXGI_DDI_ARG_PRESENT1) -> i32 {
    if arg.is_null() {
        return E_INVALIDARG;
    }
    let a = &*arg;
    if a.SurfacesToPresent == 0 || a.phSurfacesToPresent.is_null() {
        log_error!("DXGI Present1: no source surfaces");
        return E_INVALIDARG;
    }

    if a.SurfacesToPresent == 1 {
        let source = *a.phSurfacesToPresent;
        let mut present = ddi::DXGI_DDI_ARG_PRESENT {
            hDevice: a.hDevice,
            hSurfaceToPresent: source.hSurface,
            SrcSubResourceIndex: source.SubResourceIndex,
            hDstResource: a.hDstResource,
            DstSubResourceIndex: a.DstSubResourceIndex,
            pDXGIContext: a.pDXGIContext,
            Flags: a.Flags,
            FlipInterval: a.FlipInterval,
        };
        return dxgi_present(&mut present);
    }

    // WDDM 1.3 Present1's surface array is not an old single-source Present.
    // Earlier entries are part of the DXGI display/release list; the documented
    // callback contract for a many-resource present is specifically to translate
    // only the last source handle into DXGIDDICB_PRESENT. Dirty rects are hints
    // and must never be a failure reason.
    let source_index = a.SurfacesToPresent as usize - 1;
    let source = *a.phSurfacesToPresent.add(source_index);
    let h = dxgi_device_handle(a.hDevice);
    let src_h = dxgi_resource_handle(source.hSurface);
    let dst_h = dxgi_resource_handle(a.hDstResource);
    let src_alloc = resource_allocation(src_h);
    let dst_alloc = resource_allocation(dst_h);
    if PRESENT1_LOG_COUNT.first_n(64).is_some() {
        trace_line!(
            "DXGI Present1 multi: surfaces={} callback_src={} src={:p}/{} alloc=0x{:x} \
             dst={:p}/{} dstAlloc=0x{:x} dirty={} multiplicity={} flags=0x{:x}",
            a.SurfacesToPresent,
            source_index,
            source.hSurface as *mut c_void,
            source.SubResourceIndex,
            src_alloc,
            a.hDstResource as *mut c_void,
            a.DstSubResourceIndex,
            dst_alloc,
            a.DirtyRects,
            a.BackBufferMultiplicity,
            *(&a.Flags as *const ddi::DXGI_DDI_PRESENT_FLAGS as *const u32),
        );
    }

    if src_alloc == 0 {
        log_error!(
            "DXGI Present1 multi: callback source has no allocation hResource=0x{:x}",
            source.hSurface
        );
        return E_INVALIDARG;
    }

    if let Some(context) = d3d11_context(h) {
        let published_to_scanout = presented_primary_private(h, src_h).is_some()
            || (dst_alloc == 0 && copy_to_scanout_target(&context, h, src_h));
        if !published_to_scanout {
            let _ = unsafe { publish_dwm_composition(&context, h) };
        }
        context.Flush();
    }

    let mut sync_value = 0;
    if present_sync_publish_enabled() {
        if let Some(dev) = helios_device(h) {
            sync_value = dev.dxvk.present_sync_publish(
                SrcRes(resource_com_raw(src_h)),
                DstRes(resource_com_raw(dst_h)),
                false,
            );
        }
    }

    let gate_us = present_gate_us();
    if gate_us != 0 {
        if let Some(dev) = helios_device(h) {
            // Present1-multi discarded this boolean entirely; #[must_use] on
            // GateOutcome makes that a compiler warning rather than a silence.
            let _outcome = run_present_frame_gate(dev, gate_us, false);
        }
    }

    let mut present_hr = E_INVALIDARG;
    if let Some(dev) = helios_device(h) {
        // Same three preconditions, counted the same way. The HRESULTs this
        // path returns are NOT changed here: Present1-multi rejects a missing
        // callback table/context with DXGI_ERROR_UNSUPPORTED where
        // dxgi_present logs and returns present_hr, and it already rejected
        // src_alloc == 0 with E_INVALIDARG above. Unifying the two tails is
        // T7's u-forward-b-04.
        if let Err(_skip) = present_prerequisites(dev, src_alloc) {
            log_error!(
                "DXGI Present1 multi: missing callback table/context callbacks={} hContext={:p}",
                dev.dxgi_callbacks.is_null(),
                dev.context
                    .as_ref()
                    .map_or(core::ptr::null_mut(), |c| c.handle.as_ptr())
            );
            return DXGI_ERROR_UNSUPPORTED;
        }
        let mut cb = ddi::DXGIDDICB_PRESENT::default();
        let present_private = presented_primary_private(h, src_h);
        cb.hSrcAllocation = src_alloc;
        cb.hDstAllocation = dst_alloc;
        cb.pDXGIContext = a.pDXGIContext;
        cb.hContext = dev
            .context
            .as_ref()
            .map_or(core::ptr::null_mut(), |c| c.handle.as_ptr());
        cb.BroadcastContextCount = 0;
        if let Some(ref private) = present_private {
            cb.PrivateDriverDataSize = core::mem::size_of::<HeliosPresentPrivateData>() as u32;
            cb.pPrivateDriverData = (private as *const HeliosPresentPrivateData)
                .cast_mut()
                .cast();
        } else {
            cb.PrivateDriverDataSize = 0;
            cb.pPrivateDriverData = core::ptr::null_mut();
        }
        cb.bOptimizeForComposition = if present_optimize_composition_enabled() {
            1
        } else {
            0
        };
        let Some(dependencies) = RuntimePresentDependencies::new(src_alloc, dst_alloc) else {
            log_error!("DXGI Present1 multi: nonzero source allocation invariant lost");
            return E_FAIL;
        };
        if let Some(cb_n) = PRESENT_CB_LOG_COUNT.first_n_then_every_from_one(128, 512) {
            let (src_rt, src_km) = resource_parent_handles(src_h);
            let (dst_rt, dst_km) = resource_parent_handles(dst_h);
            trace_line!(
                "DXGI Present1 PresentCb identity: #{} src_alloc=0x{:x} dst_alloc=0x{:x} \
                 src_hDrv={:p} src_hRT={:p} src_hKM=0x{:x} dst_hDrv={:p} \
                 dst_hRT={:p} dst_hKM=0x{:x} hContext={:p} dxgi_context={:p} \
                 flags=0x{:x} broadcast={} private={:p}/{} optimize={}",
                cb_n,
                cb.hSrcAllocation,
                cb.hDstAllocation,
                src_h.pDrvPrivate,
                src_rt,
                src_km,
                dst_h.pDrvPrivate,
                dst_rt,
                dst_km,
                cb.hContext,
                cb.pDXGIContext,
                *(&a.Flags as *const ddi::DXGI_DDI_PRESENT_FLAGS as *const u32),
                cb.BroadcastContextCount,
                cb.pPrivateDriverData,
                cb.PrivateDriverDataSize,
                cb.bOptimizeForComposition,
            );
        }
        present_hr = submit_runtime_present_then_call(dev, dependencies, present_private, &mut cb);
    }

    if PRESENT1_LOG_COUNT.first_n(64).is_some() {
        trace_line!(
            "DXGI Present1 multi: presentCb=0x{:08x} srcAlloc=0x{:x} dstAlloc=0x{:x} opt_comp={} \
             dxgiCtx={:p} hContext={:p} syncVal={}",
            present_hr as u32,
            src_alloc,
            dst_alloc,
            present_optimize_composition_enabled() as u32,
            a.pDXGIContext,
            dev_context_for_log(h),
            sync_value
        );
    }
    present_hr
}

unsafe extern "C" fn dxgi_check_present_duration_support(
    arg: *mut ddi::DXGI_DDI_ARG_CHECKPRESENTDURATIONSUPPORT,
) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &mut *arg;
    a.ClosestSmallerDuration = 0;
    a.ClosestLargerDuration = 0;
    if PRESENT1_LOG_COUNT.first_n(16).is_some() {
        log_error!(
            "DXGI CheckPresentDurationSupport: desired={} smaller=0 larger=0",
            a.DesiredPresentDuration
        );
    }
    0
}

/// Install typed DXGI base-DDI handlers over the stub fill.
pub unsafe fn install_dxgi(funcs: *mut ddi::DXGI_DDI_BASE_FUNCTIONS) {
    let f = &mut *funcs;
    f.pfnPresent = Some(dxgi_present);
    f.pfnGetGammaCaps = Some(dxgi_get_gamma_caps);
    f.pfnSetDisplayMode = Some(dxgi_set_display_mode);
    f.pfnSetResourcePriority = Some(dxgi_set_resource_priority);
    f.pfnQueryResourceResidency = Some(dxgi_query_resource_residency);
    f.pfnRotateResourceIdentities = Some(dxgi_rotate_resource_identities);
    f.pfnBlt = Some(dxgi_blt);
}

pub unsafe fn install_dxgi_1_1(funcs: *mut ddi::DXGI1_1_DDI_BASE_FUNCTIONS) {
    let f = &mut *funcs;
    f.pfnResolveSharedResource = Some(dxgi_resolve_shared_resource);
}

pub unsafe fn install_dxgi_1_3(funcs: *mut ddi::DXGI1_3_DDI_BASE_FUNCTIONS) {
    let f = &mut *funcs;
    f.pfnBlt1 = Some(dxgi_blt1);
    f.pfnOfferResources = Some(dxgi_offer_resources);
    f.pfnReclaimResources = Some(dxgi_reclaim_resources);
    f.pfnGetMultiplaneOverlayCaps = Some(dxgi_get_mpo_caps);
    f.pfnGetMultiplaneOverlayGroupCaps = Some(dxgi_get_mpo_group_caps);
    f.pfnReserved1 = Some(dxgi_reserved_unsupported);
    f.pfnPresentMultiplaneOverlay = Some(dxgi_present_mpo);
    f.pfnReserved2 = Some(dxgi_reserved_unsupported);
    f.pfnPresent1 = Some(dxgi_present1);
    f.pfnCheckPresentDurationSupport = Some(dxgi_check_present_duration_support);
}

/// Install the implemented forwarders into the device-funcs table (over the
/// stub fill). Uses the real bindgen PFN field types — no transmute.
pub unsafe fn install(funcs: *mut ddi::D3D11DDI_DEVICEFUNCS) {
    let f = &mut *funcs;
    f.pfnCalcPrivateResourceSize = Some(calc_size_resource);
    f.pfnCalcPrivateOpenedResourceSize = Some(calc_size_opened_resource);
    f.pfnCreateResource = Some(create_resource);
    f.pfnOpenResource = Some(open_resource);
    f.pfnDestroyResource = Some(destroy_resource);
    f.pfnCalcPrivateRenderTargetViewSize = Some(calc_size_rtv);
    f.pfnCreateRenderTargetView = Some(create_rtv);
    f.pfnDestroyRenderTargetView = Some(destroy_rtv);
    f.pfnClearRenderTargetView = Some(clear_rtv);
    f.pfnCalcPrivateDepthStencilViewSize = Some(calc_size_dsv);
    f.pfnCreateDepthStencilView = Some(create_dsv);
    f.pfnDestroyDepthStencilView = Some(destroy_dsv);
    f.pfnClearDepthStencilView = Some(clear_dsv);
    f.pfnResourceCopy = Some(resource_copy);
    f.pfnResourceCopyRegion = Some(resource_copy_region);
    f.pfnResourceConvert = Some(resource_copy);
    f.pfnResourceConvertRegion = Some(resource_copy_region);
    f.pfnResourceResolveSubresource = Some(resource_resolve_subresource);
    f.pfnResourceIsStagingBusy = Some(resource_is_staging_busy);
    f.pfnResourceMap = Some(resource_map);
    f.pfnResourceUnmap = Some(resource_unmap);
    f.pfnDynamicIABufferMapNoOverwrite = Some(resource_map);
    f.pfnDynamicIABufferUnmap = Some(resource_unmap);
    f.pfnDynamicConstantBufferMapDiscard = Some(resource_map);
    f.pfnDynamicIABufferMapDiscard = Some(resource_map);
    f.pfnDynamicConstantBufferUnmap = Some(resource_unmap);
    f.pfnDynamicResourceMapDiscard = Some(resource_map);
    f.pfnDynamicResourceUnmap = Some(resource_unmap);
    f.pfnStagingResourceMap = Some(resource_map);
    f.pfnStagingResourceUnmap = Some(resource_unmap);
    f.pfnShaderResourceViewReadAfterWriteHazard = Some(srv_read_after_write_hazard);
    f.pfnResourceReadAfterWriteHazard = Some(resource_read_after_write_hazard);
    f.pfnFlush = Some(flush);

    // Shaders + pipeline.
    f.pfnCalcPrivateShaderSize = Some(calc_size_shader);
    f.pfnCreateVertexShader = Some(create_vertex_shader);
    f.pfnCreateGeometryShader = Some(create_geometry_shader);
    f.pfnCreatePixelShader = Some(create_pixel_shader);
    f.pfnCalcPrivateGeometryShaderWithStreamOutput = Some(calc_size_geometry_shader_so);
    f.pfnCreateGeometryShaderWithStreamOutput = Some(create_geometry_shader_so);
    f.pfnCalcPrivateTessellationShaderSize = Some(calc_size_tess_shader);
    f.pfnCreateHullShader = Some(create_hull_shader);
    f.pfnCreateDomainShader = Some(create_domain_shader);
    f.pfnCreateComputeShader = Some(create_compute_shader);
    f.pfnDestroyShader = Some(destroy_shader);
    f.pfnVsSetShader = Some(vs_set_shader);
    f.pfnPsSetShader = Some(ps_set_shader);
    f.pfnGsSetShader = Some(gs_set_shader);
    f.pfnHsSetShader = Some(hs_set_shader);
    f.pfnDsSetShader = Some(ds_set_shader);
    f.pfnCsSetShader = Some(cs_set_shader);
    f.pfnPsSetShaderWithIfaces = Some(ps_set_shader_with_ifaces);
    f.pfnVsSetShaderWithIfaces = Some(vs_set_shader_with_ifaces);
    f.pfnGsSetShaderWithIfaces = Some(gs_set_shader_with_ifaces);
    f.pfnHsSetShaderWithIfaces = Some(hs_set_shader_with_ifaces);
    f.pfnDsSetShaderWithIfaces = Some(ds_set_shader_with_ifaces);
    f.pfnCsSetShaderWithIfaces = Some(cs_set_shader_with_ifaces);
    f.pfnSetRenderTargets = Some(set_render_targets);
    f.pfnSetViewports = Some(set_viewports);
    f.pfnSetScissorRects = Some(set_scissor_rects);
    f.pfnIaSetTopology = Some(ia_set_topology);
    f.pfnDraw = Some(draw);
    f.pfnDrawIndexed = Some(draw_indexed);
    f.pfnDrawInstanced = Some(draw_instanced);
    f.pfnDrawIndexedInstanced = Some(draw_indexed_instanced);
    f.pfnDrawAuto = Some(draw_auto);
    f.pfnDrawInstancedIndirect = Some(draw_instanced_indirect);
    f.pfnDrawIndexedInstancedIndirect = Some(draw_indexed_instanced_indirect);
    f.pfnSoSetTargets = Some(so_set_targets);
    f.pfnSetTextFilterSize = Some(set_text_filter_size);

    // Rasterizer + depth-stencil state.
    f.pfnCalcPrivateRasterizerStateSize = Some(calc_size_raster);
    f.pfnCreateRasterizerState = Some(create_rasterizer_state);
    f.pfnSetRasterizerState = Some(set_rasterizer_state);
    f.pfnDestroyRasterizerState = Some(destroy_raster_state);
    f.pfnCalcPrivateDepthStencilStateSize = Some(calc_size_depth);
    f.pfnCreateDepthStencilState = Some(create_depth_stencil_state);
    f.pfnSetDepthStencilState = Some(set_depth_stencil_state);
    f.pfnDestroyDepthStencilState = Some(destroy_depth_state);

    // SRVs, samplers, constant buffers, updates, format support.
    f.pfnCalcPrivateShaderResourceViewSize = Some(calc_size_srv);
    f.pfnCreateShaderResourceView = Some(create_srv);
    f.pfnDestroyShaderResourceView = Some(destroy_srv);
    f.pfnCalcPrivateSamplerSize = Some(calc_size_sampler);
    f.pfnCreateSampler = Some(create_sampler);
    f.pfnDestroySampler = Some(destroy_sampler);
    f.pfnPsSetConstantBuffers = Some(ps_set_constant_buffers);
    f.pfnVsSetConstantBuffers = Some(vs_set_constant_buffers);
    f.pfnGsSetConstantBuffers = Some(gs_set_constant_buffers);
    f.pfnHsSetConstantBuffers = Some(hs_set_constant_buffers);
    f.pfnDsSetConstantBuffers = Some(ds_set_constant_buffers);
    f.pfnCsSetConstantBuffers = Some(cs_set_constant_buffers);
    f.pfnPsSetShaderResources = Some(ps_set_shader_resources);
    f.pfnVsSetShaderResources = Some(vs_set_shader_resources);
    f.pfnGsSetShaderResources = Some(gs_set_shader_resources);
    f.pfnHsSetShaderResources = Some(hs_set_shader_resources);
    f.pfnDsSetShaderResources = Some(ds_set_shader_resources);
    f.pfnCsSetShaderResources = Some(cs_set_shader_resources);
    f.pfnPsSetSamplers = Some(ps_set_samplers);
    f.pfnVsSetSamplers = Some(vs_set_samplers);
    f.pfnGsSetSamplers = Some(gs_set_samplers);
    f.pfnHsSetSamplers = Some(hs_set_samplers);
    f.pfnDsSetSamplers = Some(ds_set_samplers);
    f.pfnCsSetSamplers = Some(cs_set_samplers);
    f.pfnResourceUpdateSubresourceUP = Some(resource_update_subresource);
    f.pfnDefaultConstantBufferUpdateSubresourceUP = Some(resource_update_subresource);
    f.pfnGenMips = Some(gen_mips);
    f.pfnCheckFormatSupport = Some(check_format_support);
    f.pfnCheckMultisampleQualityLevels = Some(check_multisample_quality_levels);
    f.pfnCheckCounterInfo = Some(check_counter_info);
    f.pfnCheckCounter = Some(check_counter);

    // Queries and predication.
    f.pfnCalcPrivateQuerySize = Some(calc_size_query);
    f.pfnCreateQuery = Some(create_query);
    f.pfnDestroyQuery = Some(destroy_query);
    f.pfnQueryBegin = Some(query_begin);
    f.pfnQueryEnd = Some(query_end);
    f.pfnQueryGetData = Some(query_get_data);
    f.pfnSetPredication = Some(set_predication);

    // D3D11 UAV/compute paths.
    f.pfnCalcPrivateUnorderedAccessViewSize = Some(calc_size_uav);
    f.pfnCreateUnorderedAccessView = Some(create_uav);
    f.pfnDestroyUnorderedAccessView = Some(destroy_uav);
    f.pfnClearUnorderedAccessViewUint = Some(clear_uav_uint);
    f.pfnClearUnorderedAccessViewFloat = Some(clear_uav_float);
    f.pfnCsSetUnorderedAccessViews = Some(cs_set_uavs);
    f.pfnCopyStructureCount = Some(copy_structure_count);
    f.pfnDispatch = Some(dispatch);
    f.pfnDispatchIndirect = Some(dispatch_indirect);
    f.pfnSetResourceMinLOD = Some(set_resource_min_lod);

    // Input layouts (lazy), vertex/index buffers, blend state.
    f.pfnCalcPrivateElementLayoutSize = Some(calc_size_element_layout);
    f.pfnCreateElementLayout = Some(create_element_layout);
    f.pfnDestroyElementLayout = Some(destroy_element_layout);
    f.pfnIaSetInputLayout = Some(ia_set_input_layout);
    f.pfnIaSetVertexBuffers = Some(ia_set_vertex_buffers);
    f.pfnIaSetIndexBuffer = Some(ia_set_index_buffer);
    f.pfnCalcPrivateBlendStateSize = Some(calc_size_blend);
    f.pfnCreateBlendState = Some(create_blend_state);
    f.pfnSetBlendState = Some(set_blend_state);
    f.pfnDestroyBlendState = Some(destroy_blend_state);
}

/// Install D3D11.1-specific handlers whose signatures differ from the D3D11.0
/// prefix or only exist in the D3D11.1 table.
pub unsafe fn install_11_1(funcs: *mut ddi::D3D11_1DDI_DEVICEFUNCS) {
    let f = &mut *funcs;
    f.pfnVsSetConstantBuffers = Some(vs_set_constant_buffers1);
    f.pfnPsSetConstantBuffers = Some(ps_set_constant_buffers1);
    f.pfnGsSetConstantBuffers = Some(gs_set_constant_buffers1);
    f.pfnHsSetConstantBuffers = Some(hs_set_constant_buffers1);
    f.pfnDsSetConstantBuffers = Some(ds_set_constant_buffers1);
    f.pfnCsSetConstantBuffers = Some(cs_set_constant_buffers1);
    f.pfnFlush = Some(flush_11_1);
    f.pfnResourceCopyRegion = Some(resource_copy_region_11_1);
    f.pfnResourceConvertRegion = Some(resource_copy_region_11_1);
    f.pfnResourceUpdateSubresourceUP = Some(resource_update_subresource_11_1);
    f.pfnDefaultConstantBufferUpdateSubresourceUP = Some(resource_update_subresource_11_1);
    f.pfnDiscard = Some(discard_11_1);
    // Only >=11.1 tables have this slot, so `install()` (written against the
    // 11.0 shape) cannot set it and it was left on the uniform no-op stub.
    // Nothing gated it either: D3D11_1DDI_D3D11_OPTIONS_DATA carries only
    // OutputMergerLogicOp and AssignDebugBinarySupport, so every >=11.1 device
    // — dwm negotiates WDDM1.3 — exposed the feature with a handler that never
    // wrote the caller's MAPPED_SUBRESOURCE.
    f.pfnDynamicConstantBufferMapNoOverwrite = Some(dynamic_cb_map_no_overwrite);
    f.pfnCheckDirectFlipSupport = Some(check_direct_flip_support_11_1);
    f.pfnClearView = Some(clear_view_11_1);
    // The >=11.1 tables pass D3D11_1_DDI_BLEND_DESC (LogicOpEnable/LogicOp
    // inserted mid-struct) — `install()`'s 10.1-desc handlers misread the
    // write mask (see create_blend_state_11_1). NOTE the 11.1 rasterizer desc
    // only APPENDS ForcedSampleCount, so the shared 10.x reader stays valid
    // for pfnCreateRasterizerState.
    f.pfnCalcPrivateBlendStateSize = Some(calc_size_blend_11_1);
    f.pfnCreateBlendState = Some(create_blend_state_11_1);
    // The >=11.1 shader creates carry TYPED signature entries
    // (D3D11_1DDIARG_SIGNATURE_ENTRY2.RegisterComponentType); forward them so
    // dxbc-spv declares correctly-typed shader I/O instead of assuming
    // float32 for everything. Hull/domain use a different 11.1 tessellation
    // signatures struct, so override those ABI-specific slots as well.
    f.pfnCreateVertexShader = Some(create_vertex_shader_11_1);
    f.pfnCreatePixelShader = Some(create_pixel_shader_11_1);
    f.pfnCreateGeometryShader = Some(create_geometry_shader_11_1);
    f.pfnCalcPrivateTessellationShaderSize = Some(calc_size_tess_shader_11_1);
    f.pfnCreateHullShader = Some(create_hull_shader_11_1);
    f.pfnCreateDomainShader = Some(create_domain_shader_11_1);
}

pub unsafe fn install_wddm1_3(funcs: *mut ddi::D3DWDDM1_3DDI_DEVICEFUNCS) {
    let f = &mut *funcs;
    f.pfnCheckMultisampleQualityLevels = Some(check_multisample_quality_levels_wddm1_3);
    f.pfnUpdateTileMappings = Some(update_tile_mappings);
    f.pfnCopyTileMappings = Some(copy_tile_mappings);
    f.pfnCopyTiles = Some(copy_tiles);
    f.pfnUpdateTiles = Some(update_tiles);
    f.pfnTiledResourceBarrier = Some(tiled_resource_barrier);
    f.pfnGetMipPacking = Some(get_mip_packing);
    f.pfnResizeTilePool = Some(resize_tile_pool);
    f.pfnSetMarker = Some(set_marker);
    f.pfnSetMarkerMode = Some(set_marker_mode);
}

// Unreachable: nothing at or above 0x000b_0022 is advertised, so
// NegotiatedInterface has no WDDM2_1 variant to dispatch here. The code stays
// in place; its deletion is T6's u-core-V02.
#[allow(dead_code)]
pub unsafe fn install_wddm2_1(funcs: *mut ddi::D3DWDDM2_1DDI_DEVICEFUNCS) {
    let f = &mut *funcs;
    f.pfnCheckMultisampleQualityLevels = Some(check_multisample_quality_levels_wddm1_3);
    f.pfnAcquireResource = Some(acquire_resource_2_1);
    f.pfnReleaseResource = Some(release_resource_2_1);
}
