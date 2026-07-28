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

mod state;
mod resource;
mod views;
mod transfer;
mod shaders;
mod pipeline;
mod state_objects;
mod bindings;
mod tiles;
mod queries;
mod format_caps;
mod layout;

pub(crate) use state::*;
pub(crate) use resource::*;
pub(crate) use views::*;
pub(crate) use transfer::*;
pub(crate) use shaders::*;
pub(crate) use pipeline::*;
pub(crate) use state_objects::*;
pub(crate) use bindings::*;
pub(crate) use tiles::*;
pub(crate) use queries::*;
pub(crate) use format_caps::*;
pub(crate) use layout::*;
pub(super) use alloc::{ScanoutGeometry, VenusBacking};
pub(super) use crate::bridge::{DstRes, SrcRes};
pub(super) use handles::{Boxed, Com, ComHandle, DdiHandle, Slot};
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
    HELIOS_PRESENT_PRIVATE_MAGIC, HELIOS_PRESENT_PRIVATE_VERSION, HELIOS_PRESENT_REFRESH_MAGIC,
    HELIOS_PRESENT_REFRESH_VERSION, HELIOS_PRESENT_RENDER_MAGIC, HELIOS_PRESENT_RENDER_VERSION,
    HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY, HELIOS_WDDM_ALLOC_KIND_STANDARD,
    HELIOS_WDDM_ALLOC_MISC_DIRECT_SCANOUT, HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE,
    HELIOS_WDDM_ALLOC_MISC_PRIMARY, VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE,
    VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE, VIRTIO_GPU_BLOB_MEM_HOST3D, VIRTIO_GPU_MAP_CACHE_CACHED,
};

pub(super) use crate::ddi;
pub(super) use crate::device_funcs::HeliosDevice;
pub(super) use crate::log_error;
pub(super) use crate::present_gate_us;
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
    /// A vehicle present was MINTED on `device`, which
    /// `helios_umd_wait_last_present` then targets. R912(a) removed the
    /// `result: Option<(u32, u64)>` half: it could only ever be `None`, since
    /// its only producer was `present_sync_publish` behind a knob that
    /// defaulted off.
    Minted { device: usize },
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
        VehicleSlot::Idle | VehicleSlot::Minted { .. } => 0,
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

/// The vehicle present body: cached alias-import of the ICD frame, image copy
/// into the backbuffer. On error the caller must FAIL the present (no
/// pfnPresentCb) so the ICD latches its sw fallback instead of flipping a
/// stale backbuffer.
///
/// It used to also publish the backbuffer slot with this device's fence and
/// return `(sync_value, fence_id)`; R912(a) retired that producer, so there is
/// nothing left to hand back.
unsafe fn vehicle_present_prepare(
    h: Hdevice,
    backbuffer_h: ddi::D3D10DDI_HRESOURCE,
    info: &PresentSource,
) -> Result<(), i32> {
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

    Ok(())
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
    // `dxgi_bytes_per_pixel` is a PITCH-PADDING estimate, not a true bpp: its
    // 4-byte default covers the genuinely 16-bpp B5G6R5 / B5G5R5A1 / B4G4R4A4
    // formats and every block-compressed format. Over-reporting is harmless
    // where it is used to pad `linear_size`, but here it is a byte-addressing
    // stride, and `(Width - 1) * bpp` then runs past the row -- on the LAST
    // row, past the end of the mapping. `maybe_force_present_alpha_opaque`
    // already guards its own indexing with a hard `bpp != 4`; this is the
    // same refusal, expressed against the pitch the runtime actually mapped
    // so every currently-correct width/format still reads.
    let last_sample_end = (desc.Width.saturating_sub(1) as usize)
        .saturating_mul(bpp)
        .saturating_add(bpp.min(4));
    if row_pitch == 0 || last_sample_end > row_pitch {
        note_ddi_refusal(&DDI_REFUSALS.readback_stride_unsafe);
        log_error!(
            "DXGI Present readback: stride would leave the mapping, refusing \
             {}x{} fmt={} bpp={} row_pitch={} last_sample_end={}",
            desc.Width,
            desc.Height,
            desc.Format.0,
            bpp,
            row_pitch,
            last_sample_end
        );
        context.Unmap(&staging_res, 0);
        return;
    }
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
    /// `HeliosPresentRefreshCmd`. It still submits the present's allocation
    /// dependencies -- which is what distinguished it from the allocation-free
    /// `Refresh` variant, retired with the LINEAR copy path in R910: that
    /// marker's only trigger was `publish_dwm_composition` succeeding, and the
    /// KMD issues its own `HeliosPresentRefreshCmd` for the direct primary
    /// (`display.rs`), so the frozen refresh-marker ordering is unaffected.
    MarkerPresent {
        dependencies: RuntimePresentDependencies,
    },
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

/// Which entry point a [`finish_present`] call is serving.
///
/// R1013. `dxgi_present` and `dxgi_present1`'s multi-surface arm duplicated
/// ~70 lines -- the `DXGIDDICB_PRESENT` construction, the private-data attach,
/// `RuntimePresentDependencies::new`, the PresentCb identity trace and
/// `submit_runtime_present_then_call` -- and the copies had DRIFTED. Every
/// fix landed in `dxgi_present` only, silently not applying to any swapchain
/// presenting through Present1's multi-surface form.
///
/// This impl is the divergence table: each surviving difference is one method
/// with both arms visible, instead of code that is present on one path and
/// absent on the other. **Every arm below reproduces today's behaviour
/// exactly**; narrowing any of them is a separate change with its own
/// evidence.
///
/// Four of the six differences the review lists are already gone, deleted by
/// T6 rather than reconciled here: the vehicle TLS slot and `PRESENT_RESULT`
/// (R912), the discarded frame-gate result -- `EXT_FLIP_GATE_TIMEOUTS` is now
/// bumped in one shared place, `run_present_frame_gate` -- the
/// `copy_to_scanout_target` asymmetry (R910), and `syncVal` (R912).
#[derive(Clone, Copy, PartialEq, Eq)]
enum PresentKind {
    Present,
    Present1Multi,
}

impl PresentKind {
    /// Prefix on the PresentCb identity trace. The two spellings are load
    /// bearing: they are how a log reader tells which entry point ran.
    fn identity_prefix(self) -> &'static str {
        match self {
            PresentKind::Present => "DXGI ",
            PresentKind::Present1Multi => "DXGI Present1 ",
        }
    }

    /// Prefix on this tail's error lines.
    fn error_tag(self) -> &'static str {
        match self {
            PresentKind::Present => "DXGI Present",
            PresentKind::Present1Multi => "DXGI Present1 multi",
        }
    }

    /// What the tail returns when it never reaches the present callback --
    /// no device, or a refused prerequisite on the fall-through path.
    ///
    /// DIVERGENT AND PRESERVED: Present1-multi initialises to `E_INVALIDARG`,
    /// so a device-less path there returns a FAILURE where Present returns
    /// success.
    fn initial_hr(self) -> i32 {
        match self {
            PresentKind::Present => 0,
            PresentKind::Present1Multi => E_INVALIDARG,
        }
    }

    /// What a failed `present_prerequisites` check does.
    ///
    /// DIVERGENT AND PRESERVED: Present logs (rate-capped) and FALLS THROUGH
    /// to the rest of its body with `initial_hr`; Present1-multi RETURNS
    /// `DXGI_ERROR_UNSUPPORTED` immediately, skipping its trailing log. The
    /// two also log different field sets, which is why the message is emitted
    /// per kind rather than unified.
    fn missing_prereq_hr(self) -> Option<i32> {
        match self {
            PresentKind::Present => None,
            PresentKind::Present1Multi => Some(DXGI_ERROR_UNSUPPORTED),
        }
    }
}

/// The per-call values the shared present tail needs that are not derivable
/// from [`PresentKind`]. Both entry points read them from their own (different)
/// DDI argument struct.
struct PresentRequest {
    kind: PresentKind,
    /// `DXGI_DDI_ARG_PRESENT{,1}::pDXGIContext`, passed straight through.
    dxgi_context: *mut c_void,
    /// The raw `Flags` word. TRACE ONLY -- nothing branches on it here.
    flags: u32,
}

/// The shared present tail: build `DXGIDDICB_PRESENT`, attach the direct-primary
/// private data, resolve the runtime dependencies, trace the callback identity,
/// and submit.
///
/// Everything from the prerequisite check through
/// `submit_runtime_present_then_call`, written once. The scanout-publish
/// decision, the src->dst copy and the frame gate stay at the call sites: those
/// genuinely differ, and Present1-multi performs none of them.
///
/// `Ok(hr)` means "carry on with the rest of your body"; `Err(hr)` means
/// "return this from the DDI entry point NOW, running nothing else". That
/// distinction is load-bearing rather than stylistic: both entry points used
/// a bare `return` for the lost-invariant and (for Present1) missing-callback
/// cases, which skipped the vehicle mint, `EXT_PRESENTS` and the trailing
/// per-present log. Collapsing those into a plain HRESULT would have started
/// minting vehicle slots on a path that never did.
unsafe fn finish_present(
    h: Hdevice,
    src_h: ddi::D3D10DDI_HRESOURCE,
    dst_h: ddi::D3D10DDI_HRESOURCE,
    src_alloc: u32,
    dst_alloc: u32,
    req: PresentRequest,
) -> Result<i32, i32> {
    let no_callback_hr = req.kind.initial_hr();
    let Some(dev) = helios_device(h) else {
        return Ok(no_callback_hr);
    };

    let ready = match present_prerequisites(dev, src_alloc) {
        Ok(ready) => ready,
        Err(_skip) => {
            // Which of the three preconditions failed lives in
            // PRESENT_SKIP_NO_CALLBACKS / _NO_CONTEXT / _NO_SRC_ALLOC.
            match req.kind {
                PresentKind::Present => {
                    // Rate cap: same message text and field set, fewer lines.
                    if PRESENT_SKIP_LOG_COUNT
                        .first_n_then_every_from_one(64, 512)
                        .is_some()
                    {
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
                PresentKind::Present1Multi => {
                    log_error!(
                        "DXGI Present1 multi: missing callback table/context callbacks={} hContext={:p}",
                        dev.dxgi_callbacks.is_null(),
                        dev.context
                            .as_ref()
                            .map_or(core::ptr::null_mut(), |c| c.handle.as_ptr())
                    );
                }
            }
            return match req.kind.missing_prereq_hr() {
                Some(hr) => Err(hr),
                None => Ok(no_callback_hr),
            };
        }
    };

    let mut cb = ddi::DXGIDDICB_PRESENT::default();
    let present_private = presented_primary_private(h, src_h);
    // `ready` carries the same two values both entry points used to spell out
    // by hand -- `src_alloc` proved non-zero, and the context handle proved
    // present -- so this is the checked form of what Present1-multi was
    // re-deriving from `dev.context` after the check had already run.
    cb.hSrcAllocation = ready.src_alloc.get();
    cb.hDstAllocation = dst_alloc;
    cb.pDXGIContext = req.dxgi_context;
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
        log_error!(
            "{}: nonzero source allocation invariant lost",
            req.kind.error_tag()
        );
        return Err(E_FAIL);
    };
    if let Some(cb_n) = PRESENT_CB_LOG_COUNT.first_n_then_every_from_one(128, 512) {
        let (src_rt, src_km) = resource_parent_handles(src_h);
        let (dst_rt, dst_km) = resource_parent_handles(dst_h);
        trace_line!(
            "{}PresentCb identity: #{} src_alloc=0x{:x} dst_alloc=0x{:x} \
             src_hDrv={:p} src_hRT={:p} src_hKM=0x{:x} dst_hDrv={:p} \
             dst_hRT={:p} dst_hKM=0x{:x} hContext={:p} dxgi_context={:p} \
             flags=0x{:x} broadcast={} private={:p}/{} optimize={}",
            req.kind.identity_prefix(),
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
            req.flags,
            cb.BroadcastContextCount,
            cb.pPrivateDriverData,
            cb.PrivateDriverDataSize,
            cb.bOptimizeForComposition,
        );
    }
    Ok(submit_runtime_present_then_call(
        dev,
        dependencies,
        present_private,
        &mut cb,
    ))
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

    if let Some(context) = &context {
        if let Some(ref src_info) = ext_source {
            match vehicle_present_prepare(h, src_h, src_info) {
                Ok(()) => {
                    copied = true;
                }
                Err(hr) => {
                    VEHICLE.with(|c| c.set(VehicleSlot::Idle));
                    return hr;
                }
            }
            context.Flush();
        } else {
            // A direct primary already is the scanout backing. Do not copy it
            // through the adapter-owned LINEAR target; Present will publish its
            // rotated resource id after flushing DWM's rendering.
            let published_to_scanout =
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
    // Bounded CPU gate (`VehicleFlipGateUs` / `PresentGateUs`): the producer
    // ordering for vehicle and direct-primary/non-vehicle presents alike.
    // Timeout = proceed loudly (a stale frame beats a wedged worker).
    //
    // Until R912(a) a kernel-enforced alternative sat above this: a dxgkrnl
    // GPU-side WAIT queued on a flip fence ahead of the present packet, which
    // set `kernel_wait_armed` and skipped the gate. It required a non-zero
    // `sync_value`, whose only producer was `present_sync_publish` behind a
    // knob that defaulted OFF -- so it never armed. Measured before deleting:
    // `kwait_armed = 0` over 1536 vkcube vehicle presents (ROADMAP 7g(d)).
    let gate_us = if is_vehicle_present {
        crate::vehicle_flip_gate_us()
    } else {
        present_gate_us()
    };
    if gate_us != 0 {
        if let Some(dev) = helios_device(h) {
            let _outcome = run_present_frame_gate(dev, gate_us, is_vehicle_present);
        }
    }

    let present_hr = match finish_present(
        h,
        src_h,
        dst_h,
        src_alloc,
        dst_alloc,
        PresentRequest {
            kind: PresentKind::Present,
            dxgi_context: a.pDXGIContext,
            flags: *(&a.Flags as *const ddi::DXGI_DDI_PRESENT_FLAGS as *const u32),
        },
    ) {
        Ok(hr) => hr,
        // Abandons the vehicle mint, EXT_PRESENTS and the ordinal log below,
        // exactly as the bare `return E_FAIL` did.
        Err(hr) => return hr,
    };

    if is_vehicle_present {
        // `wait_last_present` targets the device recorded here. The
        // `result: Option<(fenceId, value)>` this slot used to carry went with
        // R912(a) -- it could only ever be None.
        VEHICLE.with(|c| {
            c.set(VehicleSlot::Minted {
                device: h.pDrvPrivate as usize,
            })
        });
        let n = EXT_PRESENTS.fetch_add(1, Ordering::Relaxed);
        if n < 4 || (n + 1) % 512 == 0 {
            log_error!(
                "vehicle present #{}: imports_failed={} copies_failed={} geom_mismatch={} \
                 overwrites={}",
                n + 1,
                EXT_IMPORT_FAILS.load(Ordering::Relaxed),
                EXT_COPY_FAILS.load(Ordering::Relaxed),
                EXT_GEOM_MISMATCH.load(Ordering::Relaxed),
                EXT_OVERWRITES.load(Ordering::Relaxed),
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
             skips={}/{}/{} gate_nc={}",
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
        context.Flush();
    }

    let gate_us = present_gate_us();
    if gate_us != 0 {
        if let Some(dev) = helios_device(h) {
            // Present1-multi discarded this boolean entirely; #[must_use] on
            // GateOutcome makes that a compiler warning rather than a silence.
            let _outcome = run_present_frame_gate(dev, gate_us, false);
        }
    }

    let present_hr = match finish_present(
        h,
        src_h,
        dst_h,
        src_alloc,
        dst_alloc,
        PresentRequest {
            kind: PresentKind::Present1Multi,
            dxgi_context: a.pDXGIContext,
            flags: *(&a.Flags as *const ddi::DXGI_DDI_PRESENT_FLAGS as *const u32),
        },
    ) {
        Ok(hr) => hr,
        // Skips the trailing PRESENT1_LOG_COUNT line, exactly as the bare
        // `return DXGI_ERROR_UNSUPPORTED` / `return E_FAIL` did.
        Err(hr) => return hr,
    };

    if PRESENT1_LOG_COUNT.first_n(64).is_some() {
        trace_line!(
            "DXGI Present1 multi: presentCb=0x{:08x} srcAlloc=0x{:x} dstAlloc=0x{:x} opt_comp={} \
             dxgiCtx={:p} hContext={:p}",
            present_hr as u32,
            src_alloc,
            dst_alloc,
            present_optimize_composition_enabled() as u32,
            a.pDXGIContext,
            dev_context_for_log(h)
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
/// Proof that [`install`] has run over a table: its 10.x-typed forwarders are
/// in place, including the eighteen slots [`install_11_1`] must overwrite.
///
/// R1009. Correctness of every >=11.1 device rested on TEXTUAL CALL ORDER
/// inside `device_funcs.rs`: `install()` writes 10.x-typed handlers into slots
/// that `install_11_1()` must run AFTERWARDS to replace. The 11.1 blend
/// descriptor inserts `LogicOpEnable` mid-struct, so a 10.x reader returns the
/// wrong write mask -- wrong blending for DWM, no counter, no log, only pixels
/// -- and the untyped-shader-create form of the same class has already shipped
/// once (VUID-Input-08733).
///
/// These tokens make the ordering structural. `install_11_1` cannot be called
/// without the value `install` returns, so
/// `install_11_1(f); install(f);` no longer compiles.
#[must_use]
pub struct Filled11_0(());

/// Proof that [`install_11_1`] has replaced the eighteen 10.x-typed slots.
#[must_use]
pub struct Filled11_1(());

/// Proof that [`install_wddm1_3`] has run. Terminal: nothing consumes it, and
/// it exists so the chain reads as one pipeline rather than two links and a
/// loose call. T6/R918 deleted the WDDM2.1 level above it -- the runtime could
/// never negotiate that interface, so there is no `upgrade_wddm2_1`.
#[must_use]
pub struct FilledWddm1_3(());

pub unsafe fn install(funcs: *mut ddi::D3D11DDI_DEVICEFUNCS) -> Filled11_0 {
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
    Filled11_0(())
}

/// Install D3D11.1-specific handlers whose signatures differ from the D3D11.0
/// prefix or only exist in the D3D11.1 table.
pub unsafe fn install_11_1(
    base: Filled11_0,
    funcs: *mut ddi::D3D11_1DDI_DEVICEFUNCS,
) -> Filled11_1 {
    // Consumed by value: this is the whole point. The 10.x handlers must
    // already be in the table for these overrides to be overrides.
    let Filled11_0(()) = base;
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
    Filled11_1(())
}

pub unsafe fn install_wddm1_3(
    level_11_1: Filled11_1,
    funcs: *mut ddi::D3DWDDM1_3DDI_DEVICEFUNCS,
) -> FilledWddm1_3 {
    let Filled11_1(()) = level_11_1;
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
    FilledWddm1_3(())
}
