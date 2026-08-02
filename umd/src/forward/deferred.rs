//! Native deferred contexts + command lists (Phase C of the command-list
//! build; design in `tmp/handoff-perf-structural/PLAN-commandlists.md`).
//!
//! The D3D11 runtime emulates command lists against a driver that reports no
//! COMMANDLISTS caps: worker threads record into runtime SWDC objects and the
//! render thread replays them call-by-call — verified as the #1 render-thread
//! cost under Fire Strike (`reports/p0-commandlist-verification.md`). These
//! DDIs let the runtime hand the recordings to US instead: a deferred context
//! forwards to DXVK's stock `D3D11DeferredContext` (already linked into this
//! DLL, zero bridge changes), `pfnCreateCommandList` is a COM
//! `FinishCommandList`, and `pfnCommandListExecute` is a COM
//! `ExecuteCommandList` on the target context.
//!
//! # The DDI shape (WDK 10.0.26100; researched by the 66th session)
//!
//! - A deferred context IS a `D3D10DDI_HDEVICE`: created by
//!   `pfnCreateDeferredContext` into `hDrvContext` private memory (sized by
//!   `pfnCalcPrivateDeferredContextSize`), destroyed through
//!   `pfnDestroyDevice` — hence the tag discrimination in
//!   `device_funcs`/`state`.
//! - The DRIVER fills the DC's own DEVICEFUNCS table (the
//!   `D3D11DDIARG_CREATEDEFERREDCONTEXT` funcs union member matching the
//!   device's negotiated level), with the WDK exclusion list NULLed and the
//!   create/destroy slots replaced by context-local open/close shims.
//! - **Context-local handles**: on first use of an object per DC the runtime
//!   calls the DC table's `pfnCreate*` with the args NULL/dependency-only and
//!   the hRT\* parameter equal to the IMMEDIATE context's driver handle. Our
//!   DC-local region layout is a COPY of the IC region's identity word (COM
//!   pointer or Box pointer), so every existing read path — `load_com`,
//!   `resource_state`, the bind arrays, the box/COM-keyed caches — works
//!   unchanged on both contexts. DC regions are BORROWS: closes clear the
//!   word, never release/take.
//! - **BUILD_2 recycle flow** (we report COMMANDLISTS_BUILD_2, so the four
//!   Recycle DDIs are required): FinishCommandList drives
//!   DC::RecycleCommandList → IC::CreateCommandList (or
//!   IC::RecycleCreateCommandList into a recycled region) → DC-local closes →
//!   IC::RecycleCreateDeferredContext (the DC is reborn). Recycle-create DDIs
//!   return HRESULT directly, NOT via pfnSetErrorCb; DC create errors go to
//!   the DC's OWN pfnSetErrorCb.
//!
//! The slots here are REAL and installed unconditionally; the `UmdCommandLists`
//! knob only decides whether `threading_caps()` invites the runtime to use
//! them (see `device_funcs::threading_caps` for the R812 pairing note).

use super::*;
use crate::adapter::NegotiatedInterface;
use crate::device_funcs::{
    ddi_destroy_device, deferred_context_private_size, log_backtrace, CtxBindings,
    HeliosDeferredContext, HELIOS_TAG_DEFERRED,
};

// ---------------------------------------------------------------------------
// Counters (log-line surface, like `ddi_refusal_summary`)
// ---------------------------------------------------------------------------

static DC_CREATED: AtomicUsize = AtomicUsize::new(0);
static DC_DESTROYED: AtomicUsize = AtomicUsize::new(0);
static DC_RECYCLED: AtomicUsize = AtomicUsize::new(0);
/// A DC-table slot outside the installed set was called. Expected 0; the
/// first hit logs a backtrace naming the caller.
static DC_UNEXPECTED_SLOT: AtomicUsize = AtomicUsize::new(0);
/// DC-local opens that copied a ZERO identity word from the IC handle (the
/// IC-side create had failed). The open still "succeeds" — the runtime owns
/// the failure of the original create — but it must be countable.
static DC_OPEN_EMPTY: AtomicUsize = AtomicUsize::new(0);
static CL_FINISHED: AtomicUsize = AtomicUsize::new(0);
static CL_EXECUTED: AtomicUsize = AtomicUsize::new(0);
static CL_ABANDONED: AtomicUsize = AtomicUsize::new(0);
/// `pfnCommandListExecute` with an empty command-list slot — refused.
static CL_EXECUTE_EMPTY: AtomicUsize = AtomicUsize::new(0);
static CHECK_HANDLE_SIZES_CALLS: AtomicUsize = AtomicUsize::new(0);
static DC_LOG: LogThrottle = LogThrottle::new();

/// One bounded line carrying the whole deferred-context surface, emitted at
/// device DestroyDevice beside `ddi_refusal_summary` (an instrument nothing
/// reads is not an instrument — T5's lesson).
pub(crate) fn deferred_summary() -> String {
    format!(
        "DC counters: dc_created={} dc_destroyed={} dc_recycled={} \
         cl_finished={} cl_executed={} cl_abandoned={} cl_exec_empty={} \
         dc_open_empty={} dc_unexpected_slot={} check_sizes_calls={}",
        DC_CREATED.load(Ordering::Relaxed),
        DC_DESTROYED.load(Ordering::Relaxed),
        DC_RECYCLED.load(Ordering::Relaxed),
        CL_FINISHED.load(Ordering::Relaxed),
        CL_EXECUTED.load(Ordering::Relaxed),
        CL_ABANDONED.load(Ordering::Relaxed),
        CL_EXECUTE_EMPTY.load(Ordering::Relaxed),
        DC_OPEN_EMPTY.load(Ordering::Relaxed),
        DC_UNEXPECTED_SLOT.load(Ordering::Relaxed),
        CHECK_HANDLE_SIZES_CALLS.load(Ordering::Relaxed),
    )
}

/// Called from `ddi_destroy_device`'s deferred arm (the teardown itself lives
/// there beside the tag dispatch).
pub(crate) fn note_deferred_context_destroyed() {
    let n = DC_DESTROYED.fetch_add(1, Ordering::Relaxed);
    if n < 16 {
        log_error!("DDI DestroyDevice(DC): deferred context destroyed (x{})", n + 1);
    }
}

// ---------------------------------------------------------------------------
// Sizes
// ---------------------------------------------------------------------------

/// Every DC-local handle region is ONE word: the copied IC identity word.
/// `CheckDeferredContextHandleSizes` and `CalcDeferredContextHandleSize` must
/// stay in lockstep (the WDK requires Calc's answer to be a member of the
/// Check array) — both read this constant.
const DC_HANDLE_WORD: usize = core::mem::size_of::<usize>();

/// The handle types a DC of ours can hold locally: everything the DC-table
/// open shim serves. One `D3D11DDI_HANDLESIZE` entry each, all
/// [`DC_HANDLE_WORD`].
const DC_HANDLE_TYPES: [ddi::D3D11DDI_HANDLETYPE; 13] = [
    ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_RESOURCE,
    ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_SHADERRESOURCEVIEW,
    ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_RENDERTARGETVIEW,
    ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_DEPTHSTENCILVIEW,
    ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_SHADER,
    ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_ELEMENTLAYOUT,
    ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_BLENDSTATE,
    ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_DEPTHSTENCILSTATE,
    ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_RASTERIZERSTATE,
    ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_SAMPLERSTATE,
    ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_QUERY,
    ddi::D3D11DDI_HANDLETYPE_D3D11DDI_HT_COMMANDLIST,
    ddi::D3D11DDI_HANDLETYPE_D3D11DDI_HT_UNORDEREDACCESSVIEW,
];

/// `pfnCheckDeferredContextHandleSizes` — a VOID writer with a count
/// out-param, double-polled at device create: first call with a null array
/// (count only), second with `*pHSizes` entries to fill. This firing at all
/// is the first sign of deferred-context life (the runtime only polls it once
/// it has seen the COMMANDLISTS caps bit).
pub(crate) unsafe extern "C" fn check_deferred_context_handle_sizes(
    _h_device: Hdevice,
    p_h_sizes: *mut ddi::UINT,
    sizes: *mut ddi::D3D11DDI_HANDLESIZE,
) {
    let n = CHECK_HANDLE_SIZES_CALLS.fetch_add(1, Ordering::Relaxed);
    if n < 8 {
        log_error!(
            "DDI CheckDeferredContextHandleSizes (x{}) count_only={} caps={}",
            n + 1,
            sizes.is_null(),
            crate::device_funcs::threading_caps()
        );
    }
    if p_h_sizes.is_null() {
        return;
    }
    if !sizes.is_null() {
        let want = (*p_h_sizes as usize).min(DC_HANDLE_TYPES.len());
        for (i, &handle_type) in DC_HANDLE_TYPES.iter().take(want).enumerate() {
            *sizes.add(i) = ddi::D3D11DDI_HANDLESIZE {
                HandleType: handle_type,
                DriverPrivateSize: DC_HANDLE_WORD as ddi::SIZE_T,
            };
        }
    }
    *p_h_sizes = DC_HANDLE_TYPES.len() as ddi::UINT;
}

/// `pfnCalcDeferredContextHandleSize(hDevice, type, pICHandle)`, called on
/// the IC table right after every IC create. Free-threaded by WDK retro-rule;
/// trivially so — it reads no state. The answer must be a member of the
/// Check array above, which every entry is.
pub(crate) unsafe extern "C" fn calc_deferred_context_handle_size(
    _h_device: Hdevice,
    _handle_type: ddi::D3D11DDI_HANDLETYPE,
    _p_ic_handle: *mut c_void,
) -> ddi::SIZE_T {
    DC_HANDLE_WORD as ddi::SIZE_T
}

pub(crate) unsafe extern "C" fn calc_private_deferred_context_size(
    _h_device: Hdevice,
    _args: *const ddi::D3D11DDIARG_CALCPRIVATEDEFERREDCONTEXTSIZE,
) -> ddi::SIZE_T {
    deferred_context_private_size() as ddi::SIZE_T
}

/// The IC-side command-list region: one owned `ID3D11CommandList` COM word,
/// stored/loaded/released through the `Slot<Com<_>>` machinery like every
/// bare-COM handle.
pub(crate) unsafe extern "C" fn calc_private_command_list_size(
    _h_device: Hdevice,
    _args: *const ddi::D3D11DDIARG_CREATECOMMANDLIST,
) -> ddi::SIZE_T {
    core::mem::size_of::<usize>() as ddi::SIZE_T
}

// ---------------------------------------------------------------------------
// Deferred-context lifecycle
// ---------------------------------------------------------------------------

/// Report an error through a DC's OWN corelayer channel. Used at create time,
/// when the `HeliosDeferredContext` may not exist yet to route through
/// `set_runtime_error`'s tag dispatch.
unsafe fn report_dc_error(core_layer: *mut c_void, um_callbacks: *const c_void, hr: i32) {
    if um_callbacks.is_null() {
        log_error!("CreateDeferredContext: no DC corelayer callbacks to report hr=0x{:08x}", hr as u32);
        return;
    }
    let cb = &*(um_callbacks as *const ddi::D3D11DDI_CORELAYER_DEVICECALLBACKS);
    if let Some(f) = cb.pfnSetErrorCb {
        f(ddi::D3D10DDI_HRTCORELAYER { handle: core_layer }, hr);
    }
}

pub(crate) unsafe extern "C" fn create_deferred_context(
    h: Hdevice,
    args: *const ddi::D3D11DDIARG_CREATEDEFERREDCONTEXT,
) {
    if args.is_null() {
        log_error!("DDI CreateDeferredContext: null args");
        return;
    }
    let a = &*args;
    let dc_priv = a.hDrvContext.pDrvPrivate;
    let dc_core_layer = a.hRTCoreLayer.handle;
    // p11UMCallbacks aliases every union member at offset 0; the corelayer
    // callbacks are read through the 11.0 layout (pfnSetErrorCb is the first
    // member of every revision).
    let dc_um_callbacks = a.__bindgen_anon_2.p11UMCallbacks as *const c_void;
    if dc_priv.is_null() {
        log_error!("DDI CreateDeferredContext: null hDrvContext");
        report_dc_error(dc_core_layer, dc_um_callbacks, E_INVALIDARG);
        return;
    }
    let Some(DrvHandle::Device(dev)) = drv_handle(h) else {
        log_error!("DDI CreateDeferredContext: parent handle is not a device");
        report_dc_error(dc_core_layer, dc_um_callbacks, E_INVALIDARG);
        return;
    };
    // Caps unreported ⇒ the runtime has no business here; refuse loudly
    // rather than constructing a DC the rest of the knob-off path never
    // expected. (E_OUTOFMEMORY is the documented DC-create failure code.)
    if !crate::umd_command_lists() {
        log_error!("DDI CreateDeferredContext: UmdCommandLists off — refused");
        report_dc_error(dc_core_layer, dc_um_callbacks, E_OUTOFMEMORY);
        return;
    }
    let Some(device) = dev.dxvk.d3d11_device() else {
        report_dc_error(dc_core_layer, dc_um_callbacks, E_OUTOFMEMORY);
        return;
    };
    let mut ctx: Option<ID3D11DeviceContext> = None;
    let created = device.CreateDeferredContext(0, Some(&mut ctx));
    let ctx = match (created, ctx) {
        (Ok(()), Some(c)) => c,
        (Ok(()), None) => {
            report_dc_error(dc_core_layer, dc_um_callbacks, E_OUTOFMEMORY);
            return;
        }
        (Err(e), _) => {
            log_error!("DDI CreateDeferredContext: DXVK create failed {e:?}");
            report_dc_error(dc_core_layer, dc_um_callbacks, create_error_hr(&e));
            return;
        }
    };
    core::ptr::write(
        dc_priv as *mut HeliosDeferredContext,
        HeliosDeferredContext {
            tag: HELIOS_TAG_DEFERRED,
            parent: dev as *const HeliosDevice,
            dc: Some(ctx),
            bindings: CtxBindings::default(),
            dc_core_layer,
            dc_um_callbacks,
        },
    );
    fill_dc_funcs(dev.negotiated, &a.__bindgen_anon_1);
    let n = DC_CREATED.fetch_add(1, Ordering::Relaxed);
    if n < 16 {
        log_error!(
            "DDI CreateDeferredContext: dc={dc_priv:p} flags=0x{:x} level={} (x{})",
            a.Flags,
            dev.negotiated.name(),
            n + 1
        );
    }
}

/// `pfnRecycleCreateDeferredContext` (IC table): the DC is reborn for its
/// next recording. DXVK's `FinishCommandList` already self-reset the deferred
/// context, so the object-level work is only our shadow state + the funcs
/// table + the per-DC error channel. Returns HRESULT directly (BUILD_2
/// recycle contract), never pfnSetErrorCb.
pub(crate) unsafe extern "C" fn recycle_create_deferred_context(
    _h: Hdevice,
    args: *const ddi::D3D11DDIARG_CREATEDEFERREDCONTEXT,
) -> ddi::HRESULT {
    if args.is_null() {
        return E_OUTOFMEMORY;
    }
    let a = &*args;
    // Raw access throughout: this DDI MUTATES the DC (error-channel refresh),
    // so no shared borrow from `drv_handle` may be live across the writes.
    let p = a.hDrvContext.pDrvPrivate as *mut HeliosDeferredContext;
    if p.is_null() || *(p as *const usize) != HELIOS_TAG_DEFERRED {
        log_error!("DDI RecycleCreateDeferredContext: handle is not a DC");
        return E_OUTOFMEMORY;
    }
    (*p).bindings.reset();
    // The runtime may re-hand the corelayer/callback pointers on rebirth.
    (*p).dc_core_layer = a.hRTCoreLayer.handle;
    (*p).dc_um_callbacks = a.__bindgen_anon_2.p11UMCallbacks as *const c_void;
    let negotiated = (*(*p).parent).negotiated;
    fill_dc_funcs(negotiated, &a.__bindgen_anon_1);
    let n = DC_RECYCLED.fetch_add(1, Ordering::Relaxed);
    if DC_LOG.first_n_then_every(8, 4096).is_some() {
        log_error!("DDI RecycleCreateDeferredContext (x{})", n + 1);
    }
    0
}

// ---------------------------------------------------------------------------
// Command lists
// ---------------------------------------------------------------------------

/// The shared FinishCommandList core: close the DC's recording into an owned
/// `ID3D11CommandList` stored in `h_cl`'s region. Returns 0 or an HRESULT
/// from the create-legal set.
unsafe fn finish_command_list_into(
    args: *const ddi::D3D11DDIARG_CREATECOMMANDLIST,
    h_cl: ddi::D3D11DDI_HCOMMANDLIST,
) -> i32 {
    clear_handle(h_cl);
    if args.is_null() {
        return E_INVALIDARG;
    }
    let Some(DrvHandle::Deferred(dc)) = drv_handle((*args).hDeferredContext) else {
        log_error!("DDI CreateCommandList: hDeferredContext is not a DC");
        return E_INVALIDARG;
    };
    let Some(dc_ctx) = dc.dc.as_ref() else {
        return E_OUTOFMEMORY;
    };
    let mut cl: Option<ID3D11CommandList> = None;
    // RestoreDeferredContextState = FALSE: the runtime's clear-state
    // convention, and what maps DXVK's self-reset onto the recycle flow with
    // zero object churn.
    match dc_ctx.FinishCommandList(false, Some(&mut cl)) {
        Ok(()) => match cl {
            Some(c) => {
                store_com(h_cl, c);
                let n = CL_FINISHED.fetch_add(1, Ordering::Relaxed);
                if DC_LOG.first_n_then_every(8, 4096).is_some() {
                    log_error!("DDI CreateCommandList: finished (x{})", n + 1);
                }
                0
            }
            None => E_OUTOFMEMORY,
        },
        Err(e) => {
            log_error!("DDI CreateCommandList: FinishCommandList failed {e:?}");
            create_error_hr(&e)
        }
    }
}

pub(crate) unsafe extern "C" fn create_command_list(
    h: Hdevice,
    args: *const ddi::D3D11DDIARG_CREATECOMMANDLIST,
    h_cl: ddi::D3D11DDI_HCOMMANDLIST,
    _h_rt_cl: ddi::D3D11DDI_HRTCOMMANDLIST,
) {
    let hr = finish_command_list_into(args, h_cl);
    if hr != 0 {
        set_runtime_error(h, hr);
    }
}

/// BUILD_2: Finish into an already-allocated (recycled) region. Errors return
/// as HRESULT directly — the WDK documents E_OUTOFMEMORY, so the create-set
/// codes are clamped to it.
pub(crate) unsafe extern "C" fn recycle_create_command_list(
    _h: Hdevice,
    args: *const ddi::D3D11DDIARG_CREATECOMMANDLIST,
    h_cl: ddi::D3D11DDI_HCOMMANDLIST,
    _h_rt_cl: ddi::D3D11DDI_HRTCOMMANDLIST,
) -> ddi::HRESULT {
    match finish_command_list_into(args, h_cl) {
        0 => 0,
        _ => E_OUTOFMEMORY,
    }
}

/// IC-side destroy (also installed in pfnRecycleDestroyCommandList, whose
/// only difference is that the runtime keeps the region for a later
/// RecycleCreate): release the owned COM word, leave the slot empty.
pub(crate) unsafe extern "C" fn destroy_command_list(
    _h: Hdevice,
    h_cl: ddi::D3D11DDI_HCOMMANDLIST,
) {
    release_com(h_cl);
}

/// `pfnCommandListExecute`, with clear-state semantics: post-execute the
/// runtime treats everything as unbound and rebinds lazily, so the target
/// context's shadow resets. The target may be the immediate context or
/// (nested execution) another DC — `d3d11_context`/`ctx_bindings` dispatch by
/// tag.
pub(crate) unsafe extern "C" fn command_list_execute(
    h: Hdevice,
    h_cl: ddi::D3D11DDI_HCOMMANDLIST,
) {
    let Some(cl) = load_com::<ID3D11CommandList>(h_cl) else {
        let n = CL_EXECUTE_EMPTY.fetch_add(1, Ordering::Relaxed);
        if n < 16 {
            log_error!("DDI CommandListExecute: empty command-list slot (x{}) — refused", n + 1);
        }
        return;
    };
    let Some(context) = d3d11_context(h) else {
        return;
    };
    // RestoreContextState = FALSE = the DDI's clear-state contract. The
    // chunks dispatch through the immediate context's EmitCsChunk, so the
    // present-time frame gate covers them with NO contract change.
    context.ExecuteCommandList(&*cl, false);
    if let Some(bindings) = ctx_bindings(h) {
        bindings.reset();
    }
    let n = CL_EXECUTED.fetch_add(1, Ordering::Relaxed);
    if DC_LOG.first_n_then_every(8, 4096).is_some() {
        log_error!("DDI CommandListExecute (x{})", n + 1);
    }
}

/// `pfnAbandonCommandList(hDC)`: the app/runtime abandoned a recording.
/// DXVK has no discard primitive; Finish-and-drop both discards the recorded
/// chunks and self-resets the DC for its next recording.
pub(crate) unsafe extern "C" fn abandon_command_list(h: Hdevice) {
    let n = CL_ABANDONED.fetch_add(1, Ordering::Relaxed);
    if n < 16 {
        log_error!("DDI AbandonCommandList (x{})", n + 1);
    }
    let Some(DrvHandle::Deferred(dc)) = drv_handle(h) else {
        log_error!("DDI AbandonCommandList: handle is not a DC");
        return;
    };
    if let Some(dc_ctx) = dc.dc.as_ref() {
        let mut cl: Option<ID3D11CommandList> = None;
        let _ = dc_ctx.FinishCommandList(false, Some(&mut cl));
        drop(cl);
    }
    dc.bindings.reset();
}

/// `pfnRecycleCommandList` (DC table): a hint that the runtime is recycling
/// this list's memory. Our memory recycling is DXVK-internal, so this is a
/// counted no-op by design, not an unimplemented path.
pub(crate) unsafe extern "C" fn recycle_command_list(
    _h: Hdevice,
    _h_cl: ddi::D3D11DDI_HCOMMANDLIST,
) {
    if DC_LOG.first_n(4).is_some() {
        log_error!("DDI RecycleCommandList (counted no-op)");
    }
}

// ---------------------------------------------------------------------------
// The DC-table shims
// ---------------------------------------------------------------------------

/// Uniform DC-local OPEN shim, transmuted into every DC-table `pfnCreate*`
/// slot. Every create DDI has the shape `(hDevice, args, hHandle, hRT…, …)`
/// — driver handle third, RT handle fourth — and x64's caller-clean
/// convention makes extra trailing args ignorable and the usize return
/// harmless for void DDIs (the same ABI argument `ddi_noop_device` documents).
/// Under the DC-create contract hRT\* IS the immediate context's driver
/// handle, so the shim copies the IC region's identity word into the DC-local
/// region: COM pointer for bare-COM slots, Box pointer for boxed slots,
/// command-list COM word for HT_COMMANDLIST. The copy is a borrow — the DC
/// close shim clears, never releases.
unsafe extern "C" fn dc_open_handle(
    _h_device: usize,
    _args: usize,
    h_priv: usize,
    ic_priv: usize,
) -> usize {
    if h_priv == 0 {
        return 0;
    }
    let word = if ic_priv == 0 {
        0
    } else {
        *(ic_priv as *const usize)
    };
    if word == 0 {
        let n = DC_OPEN_EMPTY.fetch_add(1, Ordering::Relaxed);
        if n < 16 {
            log_error!("DC open: empty IC identity word ic_priv=0x{ic_priv:x} (x{})", n + 1);
        }
    }
    *(h_priv as *mut usize) = word;
    0
}

/// Uniform DC-local CLOSE shim for every DC-table `pfnDestroy*` slot
/// (`(hDevice, hHandle)` uniformly): clear the borrowed word, touch nothing
/// it pointed at. The IC handle is guaranteed to outlive every DC-local open
/// (first-created/last-destroyed + use counting), so no read can race the
/// underlying object's real teardown.
unsafe extern "C" fn dc_close_handle(_h_device: usize, h_priv: usize) -> usize {
    if h_priv != 0 {
        *(h_priv as *mut usize) = 0;
    }
    0
}

/// Loud-failure stub pre-filling every DC-table slot before the real install:
/// anything still pointing here after the fill is a DDI the runtime was never
/// expected to call on a deferred context. Counted; first hit backtraced.
unsafe extern "C" fn dc_unexpected(_h: usize) -> usize {
    let n = DC_UNEXPECTED_SLOT.fetch_add(1, Ordering::Relaxed);
    if n == 0 {
        log_backtrace("DC DDI unexpected-slot");
    } else if n < 64 {
        log_error!("DC DDI unexpected-slot hit (x{})", n + 1);
    }
    0
}

type Uniform1 = unsafe extern "C" fn(usize) -> usize;
type Uniform2 = unsafe extern "C" fn(usize, usize) -> usize;
type Uniform4 = unsafe extern "C" fn(usize, usize, usize, usize) -> usize;

/// Pre-fill a DC funcs table with the loud-failure stub (the DC counterpart
/// of `stub_fill_device_table`; same size-derived slot count, same safety
/// argument) and return the D3D11.0-typed view.
///
/// # Safety
/// As `stub_fill_device_table`: `funcs` must be a writable table of
/// pointer-sized `Option<fn>` slots that layout-extends `D3D11DDI_DEVICEFUNCS`.
unsafe fn stub_fill_dc_table<T>(funcs: *mut T) -> *mut ddi::D3D11DDI_DEVICEFUNCS {
    let n = core::mem::size_of::<T>() / core::mem::size_of::<usize>();
    let slots = funcs as *mut Option<Uniform1>;
    for i in 0..n {
        *slots.add(i) = Some(dc_unexpected);
    }
    funcs as *mut ddi::D3D11DDI_DEVICEFUNCS
}

/// The DC-specific slot surgery applied AFTER the standard forwarder install
/// chain: context-local open/close shims over every create/destroy, the WDK
/// exclusion list NULLed, and the command-list slots.
///
/// # Safety
/// `f` must be the D3D11.0-typed view of a DC table the install chain has
/// already run over.
unsafe fn apply_dc_overrides(f: &mut ddi::D3D11DDI_DEVICEFUNCS) {
    // Context-local opens: one word-copy shim for every create. The 11.1
    // typed-create overrides installed by `install_11_1` sit in these same
    // prefix slots, so overriding through the 11.0 view covers every level.
    macro_rules! open {
        ($($field:ident),* $(,)?) => {$(
            f.$field = core::mem::transmute::<Uniform4, _>(dc_open_handle as Uniform4);
        )*};
    }
    open!(
        pfnCreateResource,
        pfnCreateRenderTargetView,
        pfnCreateDepthStencilView,
        pfnCreateVertexShader,
        pfnCreateGeometryShader,
        pfnCreatePixelShader,
        pfnCreateGeometryShaderWithStreamOutput,
        pfnCreateHullShader,
        pfnCreateDomainShader,
        pfnCreateComputeShader,
        pfnCreateRasterizerState,
        pfnCreateDepthStencilState,
        pfnCreateShaderResourceView,
        pfnCreateSampler,
        pfnCreateQuery,
        pfnCreateUnorderedAccessView,
        pfnCreateElementLayout,
        pfnCreateBlendState,
        pfnCreateCommandList,
    );
    // Context-local closes: clear-only. Includes DestroyCommandList — the
    // DC-local command-list word is a borrow of the IC region's.
    macro_rules! close {
        ($($field:ident),* $(,)?) => {$(
            f.$field = core::mem::transmute::<Uniform2, _>(dc_close_handle as Uniform2);
        )*};
    }
    close!(
        pfnDestroyResource,
        pfnDestroyRenderTargetView,
        pfnDestroyDepthStencilView,
        pfnDestroyShader,
        pfnDestroyRasterizerState,
        pfnDestroyDepthStencilState,
        pfnDestroyShaderResourceView,
        pfnDestroySampler,
        pfnDestroyQuery,
        pfnDestroyUnorderedAccessView,
        pfnDestroyElementLayout,
        pfnDestroyBlendState,
        pfnDestroyCommandList,
    );
    // The WDK exclusion list: the runtime documents it never calls these on a
    // deferred context and expects NULL. The dynamic-map family deliberately
    // STAYS — it is the DC map path (DXVK's deferred Map supports exactly
    // DISCARD/NO_OVERWRITE; deferred Unmap is a no-op there).
    f.pfnStagingResourceMap = None;
    f.pfnStagingResourceUnmap = None;
    f.pfnQueryGetData = None;
    f.pfnFlush = None;
    f.pfnResourceMap = None;
    f.pfnResourceUnmap = None;
    f.pfnResourceIsStagingBusy = None;
    f.pfnOpenResource = None;
    f.pfnSetResourceMinLOD = None;
    f.pfnCalcPrivateResourceSize = None;
    f.pfnCalcPrivateOpenedResourceSize = None;
    f.pfnCalcPrivateShaderResourceViewSize = None;
    f.pfnCalcPrivateRenderTargetViewSize = None;
    f.pfnCalcPrivateDepthStencilViewSize = None;
    f.pfnCalcPrivateElementLayoutSize = None;
    f.pfnCalcPrivateBlendStateSize = None;
    f.pfnCalcPrivateDepthStencilStateSize = None;
    f.pfnCalcPrivateRasterizerStateSize = None;
    f.pfnCalcPrivateShaderSize = None;
    f.pfnCalcPrivateGeometryShaderWithStreamOutput = None;
    f.pfnCalcPrivateSamplerSize = None;
    f.pfnCalcPrivateQuerySize = None;
    f.pfnCalcPrivateTessellationShaderSize = None;
    f.pfnCalcPrivateUnorderedAccessViewSize = None;
    f.pfnCalcPrivateDeferredContextSize = None;
    f.pfnCalcPrivateCommandListSize = None;
    f.pfnCalcDeferredContextHandleSize = None;
    f.pfnCheckDeferredContextHandleSizes = None;
    f.pfnCreateDeferredContext = None;
    // Command-list execution is legal ON a DC (nested ExecuteCommandList —
    // DXVK's deferred context splices), as are abandon/recycle notifications;
    // the destroy entry is the tag-discriminating one shared with devices.
    f.pfnCommandListExecute = Some(command_list_execute);
    f.pfnAbandonCommandList = Some(abandon_command_list);
    f.pfnRecycleCommandList = Some(recycle_command_list);
    f.pfnRecycleCreateCommandList = Some(recycle_create_command_list);
    f.pfnRecycleCreateDeferredContext = Some(recycle_create_deferred_context);
    f.pfnRecycleDestroyCommandList = Some(destroy_command_list);
    f.pfnDestroyDevice = Some(ddi_destroy_device);
}

/// Same throttle discipline as the device relocates: the device-side line
/// measured 2 relocates per CommandListExecute, and a DC-side storm would be
/// just as hostile to the recording threads.
static DC_RELOCATE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

fn dc_relocate_log(tag: &str) {
    let n = DC_RELOCATE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 8 || n % 65536 == 0 {
        log_error!("DDI RelocateDeviceFuncs(DC {tag}) (x{})", n + 1);
    }
}

unsafe extern "C" fn dc_relocate_11_0(
    _h_device: Hdevice,
    funcs: *mut ddi::D3D11DDI_DEVICEFUNCS,
) {
    dc_relocate_log("11.0");
    if !funcs.is_null() {
        fill_dc_11_0(funcs);
    }
}

unsafe extern "C" fn dc_relocate_11_1(
    _h_device: Hdevice,
    funcs: *mut ddi::D3D11_1DDI_DEVICEFUNCS,
) {
    dc_relocate_log("11.1");
    if !funcs.is_null() {
        fill_dc_11_1(funcs);
    }
}

unsafe extern "C" fn dc_relocate_wddm1_3(
    _h_device: Hdevice,
    funcs: *mut ddi::D3DWDDM1_3DDI_DEVICEFUNCS,
) {
    dc_relocate_log("WDDM1.3");
    if !funcs.is_null() {
        fill_dc_wddm1_3(funcs);
    }
}

pub(crate) unsafe fn fill_dc_11_0(funcs: *mut ddi::D3D11DDI_DEVICEFUNCS) {
    if funcs.is_null() {
        log_error!("DC fill: null 11.0 funcs table");
        return;
    }
    let f = &mut *stub_fill_dc_table(funcs);
    let _base = install(f);
    apply_dc_overrides(f);
    f.pfnRelocateDeviceFuncs = Some(dc_relocate_11_0);
}

pub(crate) unsafe fn fill_dc_11_1(funcs: *mut ddi::D3D11_1DDI_DEVICEFUNCS) {
    if funcs.is_null() {
        log_error!("DC fill: null 11.1 funcs table");
        return;
    }
    let f = &mut *stub_fill_dc_table(funcs);
    let base = install(f);
    let _l1 = install_11_1(base, funcs);
    apply_dc_overrides(f);
    (*funcs).pfnRelocateDeviceFuncs = Some(dc_relocate_11_1);
}

pub(crate) unsafe fn fill_dc_wddm1_3(funcs: *mut ddi::D3DWDDM1_3DDI_DEVICEFUNCS) {
    if funcs.is_null() {
        log_error!("DC fill: null WDDM1.3 funcs table");
        return;
    }
    let f = &mut *stub_fill_dc_table(funcs);
    let base = install(f);
    let l1 = install_11_1(base, funcs as *mut ddi::D3D11_1DDI_DEVICEFUNCS);
    let _l13 = install_wddm1_3(l1, funcs);
    apply_dc_overrides(f);
    (*funcs).pfnRelocateDeviceFuncs = Some(dc_relocate_wddm1_3);
}

/// Fill the DC's context-funcs table through the union member matching the
/// parent device's negotiated level — the same member/fill/level triple
/// discipline as `create_device`'s step 3 (R802).
unsafe fn fill_dc_funcs(
    negotiated: NegotiatedInterface,
    funcs: &ddi::D3D11DDIARG_CREATEDEFERREDCONTEXT__bindgen_ty_1,
) {
    match negotiated {
        NegotiatedInterface::Wddm1_3 => fill_dc_wddm1_3(funcs.pWDDM1_3ContextFuncs),
        NegotiatedInterface::D3D11_1 => fill_dc_11_1(funcs.p11_1ContextFuncs),
        NegotiatedInterface::D3D11_0 => fill_dc_11_0(funcs.p11ContextFuncs),
    }
}
