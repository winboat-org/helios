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

// T8/R1107 commit 0: the child modules below need this file's import surface,
// and a `use` statement is not an item -- `use super::*;` in a child cannot see
// one. Re-exporting them is what lets every child declare exactly
// `use super::*;` instead of carrying its own copy of a 50-line import block
// that would then drift.
mod alloc;
mod handles;

mod bindings;
mod deferred;
mod format_caps;
mod layout;
mod pipeline;
mod present;
mod queries;
mod resource;
mod shaders;
mod snapshot;
mod state;
mod state_objects;
mod tables;
mod tiles;
mod transfer;
mod vehicle;
mod views;

pub(super) use crate::bridge::{DstRes, PresentStreamCorrelation, SrcRes};
pub(super) use alloc::{ScanoutGeometry, VenusBacking};
pub(crate) use bindings::*;
pub(crate) use deferred::*;
pub(crate) use format_caps::*;
pub(super) use handles::{Boxed, Com, ComHandle, DdiHandle, Slot};
pub(crate) use layout::*;
pub(crate) use pipeline::*;
pub(crate) use present::*;
pub(crate) use queries::*;
pub(crate) use resource::*;
pub(crate) use shaders::*;
pub(crate) use snapshot::*;
pub(crate) use state::*;
pub(crate) use state_objects::*;
pub(crate) use tables::*;
pub(crate) use tiles::*;
pub(crate) use transfer::*;
pub(crate) use vehicle::*;
pub(crate) use views::*;
// NOT re-exported: `boxed_slot` is `pub(super)` in `handles` and its
// `BoxedHandle` bound names types (`ResourceState`, `RtvState`, `LayoutData`)
// that are private to this subtree, so a `pub(super)` re-export would leak
// them (E0446). The child modules that need it say
// `use super::handles::boxed_slot;` -- one line, and the bound stays sealed.
use handles::boxed_slot;

pub(super) use core::ffi::c_void;
pub(super) use core::mem::ManuallyDrop;
pub(super) use std::sync::atomic::{AtomicUsize, Ordering};

pub(super) use windows::core::{IUnknown, Interface, PCSTR};
pub(super) use windows::Win32::Foundation::{BOOL, RECT};
pub(super) use windows::Win32::Graphics::Direct3D::{
    D3D11_SRV_DIMENSION_BUFFER, D3D11_SRV_DIMENSION_BUFFEREX, D3D11_SRV_DIMENSION_TEXTURE1D,
    D3D11_SRV_DIMENSION_TEXTURE1DARRAY, D3D11_SRV_DIMENSION_TEXTURE2D,
    D3D11_SRV_DIMENSION_TEXTURE2DARRAY, D3D11_SRV_DIMENSION_TEXTURE2DMS,
    D3D11_SRV_DIMENSION_TEXTURE2DMSARRAY, D3D11_SRV_DIMENSION_TEXTURE3D,
    D3D11_SRV_DIMENSION_TEXTURECUBE, D3D11_SRV_DIMENSION_TEXTURECUBEARRAY,
};
pub(super) use windows::Win32::Graphics::Direct3D11::*;
pub(super) use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

pub(super) use helios_protocol::{
    HeliosPresentPrivateData, HeliosPresentRefreshCmd, HeliosPresentRenderCmd, HeliosWddmAllocMeta,
    HeliosWddmAllocPrivate, HeliosWddmOpenIdentity, HELIOS_PRESENT_PRIVATE_FLAG_DIRECT_SCANOUT,
    HELIOS_PRESENT_PRIVATE_FLAG_SNAPSHOT, HELIOS_PRESENT_PRIVATE_FLAG_WINDOWED_BLT_SNAPSHOT,
    HELIOS_PRESENT_PRIVATE_MAGIC, HELIOS_PRESENT_SNAPSHOT_PURPOSE_NONE,
    HELIOS_PRESENT_SNAPSHOT_PURPOSE_WINDOWED_BLT,
    HELIOS_PRESENT_PRIVATE_VERSION, HELIOS_PRESENT_REFRESH_MAGIC,
    HELIOS_PRESENT_REFRESH_VERSION, HELIOS_PRESENT_RENDER_MAGIC, HELIOS_PRESENT_RENDER_VERSION,
    HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY, HELIOS_WDDM_ALLOC_KIND_STANDARD,
    HELIOS_WDDM_ALLOC_MISC_DIRECT_SCANOUT, HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE,
    HELIOS_WDDM_ALLOC_MISC_PRIMARY, HELIOS_WDDM_BLOB_FLAG_GLOBAL_VIDMM_TRACKER,
    VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE, VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE,
    VIRTIO_GPU_BLOB_MEM_HOST3D, VIRTIO_GPU_MAP_CACHE_CACHED,
};

pub(super) use crate::ddi;
pub(super) use crate::device_funcs::HeliosDevice;
pub(super) use crate::log_error;
pub(super) use crate::trace_line;

pub(super) type Hdevice = ddi::D3D10DDI_HDEVICE;

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
pub(super) struct LogThrottle {
    count: AtomicUsize,
}

impl LogThrottle {
    pub(super) const fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
        }
    }

    /// Bump and return the occurrence ordinal with no rate decision, for sites
    /// whose gate carries an extra escape clause (`|| alloc != 0`) or a shape
    /// of its own.
    pub(super) fn next(&self) -> usize {
        self.count.fetch_add(1, Ordering::Relaxed)
    }

    /// Read the ordinal WITHOUT bumping it — one site logs a "pre" line under
    /// the same budget as the "post" line that follows it.
    pub(super) fn peek(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// The first `first` occurrences.
    pub(super) fn first_n(&self, first: usize) -> Option<usize> {
        let n = self.next();
        (n < first).then_some(n)
    }

    /// The first `first`, then every `every`-th counting from zero.
    pub(super) fn first_n_then_every(&self, first: usize, every: usize) -> Option<usize> {
        let n = self.next();
        (n < first || n % every == 0).then_some(n)
    }

    /// The first `first`, then every `every`-th counting from one. Distinct
    /// from [`Self::first_n_then_every`]: it fires at n = every-1, 2*every-1,
    /// not at n = 0, every, 2*every.
    pub(super) fn first_n_then_every_from_one(&self, first: usize, every: usize) -> Option<usize> {
        let n = self.next();
        (n < first || (n + 1) % every == 0).then_some(n)
    }
}

pub(super) static RESOURCE_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static CREATE_RESOURCE_IDENTITY_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static VIEW_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static WDDM_ALLOC_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static D3D11_1_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static COPY_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static COPY_REGION_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static MAP_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static SHADER_BIND_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static SHADER_SET_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static SRV_CREATE_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static SRV_BIND_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static DRAW_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static OM_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static UPDATE_LOG_COUNT: LogThrottle = LogThrottle::new();
/// UpdateSubresource lines the rate cap dropped. Without this the cap would
/// turn "no lines" into "nothing happened".
pub(super) static UPDATE_SUPPRESSED: AtomicUsize = AtomicUsize::new(0);
pub(super) static DISPATCH_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static HANDLE_MISS_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static UAV_BIND_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static CLEAR_RTV_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static VIEWPORT_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static SCISSOR_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static RASTER_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static IA_BIND_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static PRESENT_READBACK_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static PRESENT_FORCE_OPAQUE_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(super) static PRESENT_CB_LOG_COUNT: LogThrottle = LogThrottle::new();

pub(super) use crate::format;

/// The lossy DXGI -> legacy D3DDDIFORMAT downgrade the KMD's
/// `DxgkDdiDescribeAllocation` consumes. Counted, not refused: only two DXGI
/// formats have a spelling here, the EXACT format travels beside it in
/// `HeliosWddmAllocMeta::dxgi_format`, and every consumer that needs
/// bpp/layout reads that one. It was the last silent answer in the format
/// readers.
fn dxgi_to_d3dddi_format(fmt: u32) -> u32 {
    let d3dddi = format::to_d3dddi(fmt);
    if d3dddi == format::D3DDDIFMT_UNKNOWN {
        note_ddi_refusal(&DDI_REFUSALS.alloc_meta_format_unknown);
    }
    d3dddi
}

fn d3dddi_to_dxgi_format(fmt: u32) -> DXGI_FORMAT {
    DXGI_FORMAT(format::from_d3dddi(fmt) as i32)
}

/// Bytes per pixel of an (uncompressed) `DXGI_FORMAT`, for computing the WDDM
/// surface pitch.
fn dxgi_bytes_per_pixel(fmt: u32) -> u32 {
    format::bytes_per_pixel(fmt)
}

/// Bits per sample for the uncompressed DXGI formats that can participate in
/// D3D11 output/MSAA validation.
fn dxgi_bits_per_sample(fmt: u32) -> Option<u32> {
    format::bits_per_sample(fmt)
}

fn dxgi_output_family_bits(fmt: u32) -> Option<u32> {
    format::output_family_bits(fmt)
}

fn dxgi_output_bits_per_sample(fmt: u32, caps: u32) -> Option<u32> {
    const D3D11_FORMAT_SUPPORT_RENDER_TARGET: u32 = 0x0000_4000;
    const D3D11_FORMAT_SUPPORT_DEPTH_STENCIL: u32 = 0x0001_0000;

    if caps & (D3D11_FORMAT_SUPPORT_RENDER_TARGET | D3D11_FORMAT_SUPPORT_DEPTH_STENCIL) != 0 {
        dxgi_bits_per_sample(fmt)
    } else {
        dxgi_output_family_bits(fmt)
    }
}

fn dxgi_msaa_bits_per_sample(fmt: u32, caps: u32) -> Option<u32> {
    if format::msaa_ineligible(fmt) {
        None
    } else {
        dxgi_output_bits_per_sample(fmt, caps)
    }
}

fn dxgi_resolve_required(fmt: u32) -> bool {
    format::resolve_required(fmt)
}

fn dxgi_color_typeless_parent(fmt: u32) -> bool {
    format::color_typeless_parent(fmt)
}

fn dxgi_integer_typed_format(fmt: u32) -> bool {
    format::integer_typed(fmt)
}

pub(super) use crate::hr::{DXGI_ERROR_UNSUPPORTED, E_FAIL, E_INVALIDARG, E_OUTOFMEMORY};
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

/// Lock a mutex, ignoring poison. With `panic = "abort"` in both profiles no
/// unwind can ever poison a mutex, and a DDI must never panic on lock — so the
/// poisoned arm hands back the inner guard instead of propagating.
pub(crate) fn lock_ignore_poison<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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

/// The eleven DDI paths that refuse or silently downgrade runtime-requested work.
///
/// Each field is a legitimate runtime decision about runtime-supplied data, so
/// a *type* encoding would be cosmetic — what they lacked was any record at
/// all. Every one of them returned, dropped or downgraded without incrementing
/// anything, which violates the loud-failure invariant. R911.
///
/// The refusals themselves are unchanged: this makes them countable, not
/// different.
struct DdiRefusals {
    /// `pfnResourceReadAfterWriteHazard` for an SRV — empty body.
    srv_raw_hazard: AtomicUsize,
    /// `pfnResourceReadAfterWriteHazard` for a resource — empty body.
    resource_raw_hazard: AtomicUsize,
    /// `pfnSetTextFilterSize` — empty body.
    text_filter_size_ignored: AtomicUsize,
    /// `pfnResourceIsStagingBusy` returning 0. NOT a no-op: that is the
    /// semantic claim "never busy", which the runtime acts on.
    staging_busy_assumed_free: AtomicUsize,
    /// `pfnDiscard` with `num_rects != 0` — the partial discard is dropped.
    /// Well reasoned (upstream DXVK does the same, and forwarding partial
    /// discards as full-view discards wiped the undamaged 99% of DWM's flip
    /// backbuffer), but it was uncounted AND unlogged once the 64-line budget
    /// was spent.
    discard_partial: AtomicUsize,
    /// `pfnClearView` for a non-RTV view type — the clear is dropped. This one
    /// already logs its refusal, so it was uncounted but never silent; no
    /// second log line is added.
    clear_view_unsupported: AtomicUsize,
    /// `pfnCreateGeometryShaderWithStreamOutput` — the SO declaration is
    /// discarded and a plain GS is created. The most consequential of the nine:
    /// `SOSetTargets` then binds buffers that are never written and `DrawAuto`
    /// reads zero vertices, so the app renders nothing with nothing recording
    /// that SO capture was dropped.
    gs_so_declaration_dropped: AtomicUsize,
    /// Hull/domain shader creates taking the signature-less fallback.
    /// Expected to MOVE under 3DMark; the UB against SINT inputs noted at the
    /// fallback is NOT fixed here — it is made countable, which is the
    /// precondition for fixing it against a real workload.
    tess_sig_fallback: AtomicUsize,
    /// `create_resource` with a resource dimension outside the four we handle.
    unhandled_resource_dimension: AtomicUsize,
    /// A DXGI format with no legacy D3DDDIFORMAT spelling, stamped into the
    /// KMD allocation meta as `D3DDDIFMT_UNKNOWN` (0).
    ///
    /// The tenth, added with R1010. `format::to_d3dddi` knows exactly two
    /// formats -- R8G8B8A8_UNORM and B8G8R8A8_UNORM -- and answered 0 for
    /// everything else with no log and no counter; that 0 goes straight into
    /// `HeliosWddmAllocMeta::format`, which `DxgkDdiDescribeAllocation`
    /// consumes. It is a legitimate downgrade (the EXACT format travels
    /// separately in `dxgi_format`, which is what every consumer that needs
    /// bpp/layout reads), so this counts rather than refuses -- but it was the
    /// one silent path the format table's readers still had.
    alloc_meta_format_unknown: AtomicUsize,
    /// `maybe_log_present_readback` refusing to sample a mapped surface whose
    /// `dxgi_bytes_per_pixel` stride would leave the mapped row.
    ///
    /// The eleventh, added with R1010. Env-gated
    /// (`HELIOS_PRESENT_READBACK`) and capped at 8 invocations, so it is a
    /// debugging path rather than a live one -- but it was reading out of
    /// bounds for a genuinely 16-bpp or block-compressed surface, and a
    /// refusal has to be countable like every other.
    readback_stride_unsafe: AtomicUsize,
}

static DDI_REFUSALS: DdiRefusals = DdiRefusals {
    srv_raw_hazard: AtomicUsize::new(0),
    resource_raw_hazard: AtomicUsize::new(0),
    text_filter_size_ignored: AtomicUsize::new(0),
    staging_busy_assumed_free: AtomicUsize::new(0),
    discard_partial: AtomicUsize::new(0),
    clear_view_unsupported: AtomicUsize::new(0),
    gs_so_declaration_dropped: AtomicUsize::new(0),
    tess_sig_fallback: AtomicUsize::new(0),
    unhandled_resource_dimension: AtomicUsize::new(0),
    alloc_meta_format_unknown: AtomicUsize::new(0),
    readback_stride_unsafe: AtomicUsize::new(0),
};

/// One bounded log line carrying all eleven counters.
///
/// The UMD's evidence channel is the log — it has no registry counter surface —
/// and T5 proved the failure mode this avoids: three of the four R806/R809
/// scan-out counters were process-global atomics that NOTHING ever loaded, so
/// ROADMAP's own instruction to read them after a gate run was not executable.
/// **An instrument nothing can read is not an instrument.** This extends the
/// `scanout_counter_summary()` pattern that fixed it, rather than inventing a
/// second mechanism, and it is a `log_line` summary rather than an escape,
/// which the recommendation is explicit about.
///
/// ⚠ NOT on a per-present path: the UMD hot-path logger cost is exactly what T2
/// measured and reduced. Emitted at `DestroyDevice`, and on the FIRST hit of
/// each counter (so a refusal that fires once in a session that never tears a
/// device down is still visible).
pub(crate) fn ddi_refusal_summary() -> String {
    let r = &DDI_REFUSALS;
    format!(
        "DDI refusals: srv_raw_hazard={} resource_raw_hazard={} \
         text_filter_size_ignored={} staging_busy_assumed_free={} \
         discard_partial={} clear_view_unsupported={} \
         gs_so_declaration_dropped={} tess_sig_fallback={} \
         unhandled_resource_dimension={} alloc_meta_format_unknown={} \
         readback_stride_unsafe={}",
        r.srv_raw_hazard.load(Ordering::Relaxed),
        r.resource_raw_hazard.load(Ordering::Relaxed),
        r.text_filter_size_ignored.load(Ordering::Relaxed),
        r.staging_busy_assumed_free.load(Ordering::Relaxed),
        r.discard_partial.load(Ordering::Relaxed),
        r.clear_view_unsupported.load(Ordering::Relaxed),
        r.gs_so_declaration_dropped.load(Ordering::Relaxed),
        r.tess_sig_fallback.load(Ordering::Relaxed),
        r.unhandled_resource_dimension.load(Ordering::Relaxed),
        r.alloc_meta_format_unknown.load(Ordering::Relaxed),
        r.readback_stride_unsafe.load(Ordering::Relaxed),
    )
}

/// Bump one refusal counter and emit the summary on its FIRST hit.
///
/// Taking `&AtomicUsize` rather than a field name keeps the call sites one line
/// and makes "increment without a readout" — the defect this whole item exists
/// to close — impossible to write by accident.
fn note_ddi_refusal(counter: &AtomicUsize) {
    if counter.fetch_add(1, Ordering::Relaxed) == 0 {
        log_error!("{}", ddi_refusal_summary());
    }
}

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

/// `order_mode` values of `HeliosDxvkDevice::present_frame_gate`, mirroring
/// `kPresentOrderComplete` / `kPresentOrderSubmitted` in `bridge/dxvk_bridge.h`.
pub(super) const PRESENT_ORDER_COMPLETE: u32 = 0;
pub(super) const PRESENT_ORDER_SUBMITTED: u32 = 1;

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
    // The vehicle keeps the COMPLETE wait, and it is the ONLY path that may.
    // Its ordering requirement is genuinely different and genuinely stronger:
    // the vehicle backbuffer is scanned out on a direct/independent flip
    // ordered only on the KMD's DMA fence, which completes at DECODE, so the
    // copy's host-GPU completion is what has to be waited for (24th session).
    // It has its own bound, `VehicleFlipGateUs`.
    //
    // The DIRECT-PRIMARY / ordinary-app path is SUBMITTED, unconditionally and
    // with no knob. It is ordered by the `DxgkDdiRender` watermark, which needs
    // only that the frame's Venus work has reached `vkQueueSubmit`; waiting for
    // GPU completion here is a producer-side CPU stall that removes all
    // CPU/GPU overlap and does not fix the ordering anyway (owner-verified
    // 2026-07-29: an unexpirable 200 ms completion gate still flashed black).
    // The `PresentOrder`/`PresentGateUs` knobs that used to select and bound it
    // were deleted with it — see the ⛔ note in `crate::knobs`.
    let order_mode = if is_vehicle_present {
        PRESENT_ORDER_COMPLETE
    } else {
        PRESENT_ORDER_SUBMITTED
    };
    if dev.dxvk.present_frame_gate(gate_us, order_mode) {
        return GateOutcome::Completed;
    }
    let total = PRESENT_GATE_TIMEOUTS.fetch_add(1, Ordering::Relaxed) + 1;
    if is_vehicle_present {
        // Unchanged text and cadence: this is the pre-existing vehicle line.
        let n = EXT_FLIP_GATE_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
        if n < 16 || n % 512 == 0 {
            log_error!("vehicle flip gate TIMEOUT (x{}) — flipping anyway", n + 1);
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
