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
mod present;

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
pub(crate) use present::*;
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
