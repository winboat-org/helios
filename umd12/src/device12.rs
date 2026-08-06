//! `HeliosD3D12Device` — the driver's per-device private block, and the
//! `pfnCalcPrivateDeviceSize` / `pfnCreateDevice` / `pfnDestroyDevice` bodies
//! behind it.
//!
//! # Where this block lives, and why the size site and the write site are one
//! function
//!
//! WDDM's object model: the driver states a size, the **runtime** allocates that
//! many bytes, and the driver constructs its object *in place* inside them.
//! `hDrvDevice.pDrvPrivate` is that pointer. Identical to the D3D11 shape
//! (`umd/src/device_funcs.rs`, R2 §1.1).
//!
//! ⚠ `D3D12DDI_CREATE_DEVICE_FLAG_DEBUGGABLE` arrives on **both**
//! `D3D12DDIARG_CALCPRIVATEDEVICESIZE` and `D3D12DDIARG_CREATEDEVICE_0109`
//! (`DDI_REFERENCE.md` §1.4), so the private size may legitimately differ
//! between debug and retail — and the two sites must compute it from the *same*
//! function of `Flags`, never `size_of::<Device>()` at one and a constant at the
//! other. [`device_private_size`] is that one function, and both call it.
//!
//! # ⛔ `DECISIONS.md` D13 and what does NOT belong here
//!
//! This block is **per-object, per-process, and crosses no module boundary** —
//! nothing outside this DLL ever reads it — so it lives here, typed against
//! `ddi12`, which is what D13's refined form says. What must never be
//! re-declared here is the private data that *does* cross: the allocation
//! private driver data (`HeliosWddmAllocPrivate`), the KMD-stamped open identity
//! (`HeliosWddmOpenIdentity`) and the present identity channel
//! (`HeliosPresentRenderCmd`). Those are `helios_protocol`'s, already read by
//! `umd`, `kmd_render` and the Mesa ICD, and L4/L8 reuse them verbatim.
//!
//! # The unwind guard
//!
//! `create_device` builds three things that must be released if a later step
//! fails: the engine device, the in-place `HeliosD3D12Device`, and (from L2) the
//! per-queue WDDM contexts. `umd/src/adapter.rs`'s `DeviceUnderConstruction`
//! exists because two checks used to run *after* construction and each leaked —
//! *"the `hDrvDevice` one leaked the whole DXVK/Vulkan device, and the
//! `pDeviceFuncs` one … leaked a kernel context and a paging queue per attempt.
//! A crash-looping client exhausted them."* [`DeviceUnderConstruction`] is the
//! same guard for D3D12, and the ordering rule it enforces is the cheaper half:
//! **validate every runtime-supplied pointer BEFORE constructing anything.**

use helios_umd_common::hr::{Hresult, DXGI_ERROR_UNSUPPORTED, E_FAIL, E_INVALIDARG, S_OK};

use crate::adapter12::Ddi12Interface;
use crate::bridge12::BridgeDevice12;
use crate::{ddi12, log_error, note_refusal, UMD12_REFUSALS};

/// The Helios D3D12 device: what this driver keeps for the lifetime of one
/// `ID3D12Device`.
///
/// ⚠ **New fields are appended at the END** (`PARALLEL.md` §5). Eleven lanes add
/// to this struct; reordering or repurposing an existing field is how two lanes'
/// state silently becomes one.
pub(crate) struct HeliosD3D12Device {
    /// The runtime's handle for this device. Every corelayer callback that
    /// reports an error takes it — `pfnSetErrorCb(hRTDevice, hr)` is how a
    /// `VOID`-returning DDI says it failed, and most D3D12 DDIs return `VOID`
    /// (`DECISIONS.md` §7.6).
    pub(crate) h_rt_device: ddi12::D3D12DDI_HRTDEVICE,

    /// The negotiated interface. One value today (D12 advertises one token), and
    /// stored anyway: a lane that needs to know which revision it is serving
    /// must read it from here rather than assume, and the day a second token is
    /// ever advertised this is the field that stops being a constant.
    pub(crate) negotiated: Ddi12Interface,

    /// `D3D12DDI_CREATE_DEVICE_FLAGS` as the runtime gave them, including
    /// `DEBUGGABLE`. Kept because [`device_private_size`] is a function of them
    /// and a later reader must be able to check that this block was sized by the
    /// same input it was built with.
    pub(crate) flags: ddi12::D3D12DDI_CREATE_DEVICE_FLAGS,

    /// The **usermode** D3D12 corelayer callbacks, at the `_0062` revision.
    ///
    /// ⛔ This pointer is the `ARCHITECTURE.md` §12 trap 2 landmine in its
    /// D3D12 form. `p12UMCallbacks` is a **union of four arms** — `_0003`,
    /// `_0022`, `_0050`, `_0062` — of 12 / 14 / 17 / 18 pointer-wide members
    /// (`DDI_REFERENCE.md` §1.4, §6.2), and reading the wrong arm reads past the
    /// end of a shorter one. Which arm is live is decided by the negotiated
    /// version, and D12's one-token set fixes it at `_0062`: there is no other
    /// revision this driver can be asked for. That is why the field has a
    /// concrete type instead of a `*const c_void` a call site reinterprets.
    pub(crate) um_callbacks: *const ddi12::D3D12DDI_CORELAYER_DEVICECALLBACKS_0062,

    /// The **kernel** callbacks — the same 65-entry `D3DDDI_DEVICECALLBACKS`
    /// table the D3D11 UMD drives (`DECISIONS.md` §4.1, R2 §1.2), carrying
    /// `pfnRenderCb`, `pfnPresentCb`, `pfnAllocateCb`, `pfnEscapeCb`.
    ///
    /// ⭐ It is the reason a D3D12 present can share the D3D11 identity channel
    /// at all: `DDI_REFERENCE.md` §8.2 records that there is **no DMA buffer in
    /// the D3D12 DDI and no `pfnRenderCb` in `d3d12umddi.h`** — it arrives here,
    /// through `pKTCallbacks`, which is `d3dumddi.h`'s table.
    pub(crate) kt_callbacks: *const ddi12::D3DDDI_DEVICECALLBACKS,

    /// The vkd3d engine device. Owns the `ID3D12Device` and, through it, the
    /// whole Vulkan device, its memory and its queues.
    ///
    /// ⚠ Dropping this drops all of that, which is why `destroy_device` is
    /// `drop_in_place` and not a bare "null the handle".
    pub(crate) engine: BridgeDevice12,
}

/// The size of the private block the runtime must allocate for one device.
///
/// ⛔ **One function, called by both `pfnCalcPrivateDeviceSize` and
/// `pfnCreateDevice`**, because `DEBUGGABLE` reaches both and a size that is a
/// function of `Flags` at one site and a constant at the other is a buffer
/// overrun waiting for a debug-layer client (`DDI_REFERENCE.md` §1.4).
///
/// Today the answer does not depend on `flags` — nothing in
/// [`HeliosD3D12Device`] is debug-only. The parameter is present so that when a
/// lane adds a debug-only field it is *forced* to change one function rather
/// than remember two.
pub(crate) fn device_private_size(_flags: ddi12::D3D12DDI_CREATE_DEVICE_FLAGS) -> usize {
    core::mem::size_of::<HeliosD3D12Device>()
}

/// A device that has been written into the runtime's private block but whose
/// handle the runtime has not accepted yet.
///
/// Dropping it tears the device down; [`DeviceUnderConstruction::defuse`] hands
/// ownership to the runtime. `umd/src/adapter.rs`'s guard of the same name is
/// the precedent, and its docstring records what its absence cost: a leaked
/// Vulkan device per failed create, and a leaked kernel context and paging queue
/// per attempt, which *"a crash-looping client exhausted"*.
struct DeviceUnderConstruction {
    device: *mut HeliosD3D12Device,
}

impl DeviceUnderConstruction {
    /// The runtime now owns the device; do not tear it down.
    fn defuse(mut self) {
        self.device = core::ptr::null_mut();
    }
}

impl Drop for DeviceUnderConstruction {
    fn drop(&mut self) {
        if self.device.is_null() {
            return;
        }
        log_error!("CreateDevice rollback: dropping the partially-built D3D12 device");
        // SAFETY: `device` points at the runtime-allocated private block this
        // guard was constructed around, `core::ptr::write` has run over it, and
        // the runtime has never seen the handle — so this guard is the only
        // owner and no other reference can exist during teardown.
        unsafe { core::ptr::drop_in_place(self.device) };
    }
}

/// `pfnCalcPrivateDeviceSize`.
///
/// # Safety
/// `arg`, when non-null, must point at a live `D3D12DDIARG_CALCPRIVATEDEVICESIZE`
/// for the duration of the call.
pub(crate) unsafe fn calc_private_device_size(
    arg: *const ddi12::D3D12DDIARG_CALCPRIVATEDEVICESIZE,
) -> usize {
    if arg.is_null() {
        // ⚠ There is no HRESULT to refuse with — the DDI returns `SIZE_T`. A 0
        // here is paired with a `create_device` that will refuse the same null
        // world, so the runtime allocates nothing and the create fails cleanly.
        note_refusal(&UMD12_REFUSALS.calc_private_device_size_bad_arg);
        return 0;
    }
    // SAFETY: non-null per the check above; the DDI declares it `_In_ CONST`, so
    // the runtime guarantees a live, aligned struct for the call.
    let a = unsafe { &*arg };

    // Refuse-shaped, but a size cannot refuse: report 0 for a version this
    // driver never advertised, so that if the runtime somehow proceeds it
    // proceeds into `create_device`'s explicit refusal rather than into a block
    // sized for a shape nobody agreed on.
    if Ddi12Interface::from_pair(a.Interface, a.Version).is_none() {
        note_refusal(&UMD12_REFUSALS.ddi12_version_mismatch);
        log_error!(
            "CalcPrivateDeviceSize: UNADVERTISED Interface={:#010x} Version={:#010x} -> 0",
            a.Interface,
            a.Version,
        );
        return 0;
    }

    let size = device_private_size(a.Flags);
    log_error!(
        "CalcPrivateDeviceSize: Flags={:#x} -> {size}",
        a.Flags,
    );
    size
}

/// `pfnCreateDevice`.
///
/// The eight steps, in the order `umd/src/adapter.rs` learned they must run:
///
/// 0. validate **every** runtime-supplied pointer, before constructing anything;
/// 1. dispatch the interface through the closed set — never an `else`;
/// 2. bring up the engine device;
/// 3. write `HeliosD3D12Device` into the runtime's private block, under the
///    unwind guard;
/// 4. defuse the guard and return.
///
/// ⚠ There is no table fill here. D3D12 fills its tables at **adapter** scope
/// through `pfnFillDDITable`, before any device exists (`ARCHITECTURE.md` §1.2,
/// and measured at S5) — the opposite of D3D11, where `CreateDevice` writes the
/// device-funcs table itself.
///
/// # Safety
/// `arg` must point at a live `D3D12DDIARG_CREATEDEVICE_0109` for the duration
/// of the call, and its `hDrvDevice.pDrvPrivate` at
/// [`device_private_size`]-many writable bytes the runtime allocated for this
/// device.
pub(crate) unsafe fn create_device(
    arg: *const ddi12::D3D12DDIARG_CREATEDEVICE_0109,
) -> Hresult {
    if arg.is_null() {
        note_refusal(&UMD12_REFUSALS.create_device_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: non-null per the check above. ⛔ It is the `_0109` shape and not
    // `_0003` only because D12 advertises a single token — a `_0003`-generation
    // negotiation would make the two trailing fields (`pReserveRanges`,
    // `NumReserveRanges`) a read past the end of the runtime's struct.
    let a = unsafe { &*arg };

    // ── 0. Validate BEFORE constructing ─────────────────────────────────────
    if a.hDrvDevice.pDrvPrivate.is_null() {
        log_error!("CreateDevice: null hDrvDevice -> E_INVALIDARG");
        note_refusal(&UMD12_REFUSALS.create_device_bad_arg);
        return E_INVALIDARG;
    }
    if a.pKTCallbacks.is_null() {
        log_error!("CreateDevice: null pKTCallbacks -> E_INVALIDARG");
        note_refusal(&UMD12_REFUSALS.create_device_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: every arm of this union is a pointer at offset 0 — machine-checked
    // by the bindgen layout assertions — so reading one member to null-check it
    // is well-defined for any initialisation the runtime can have performed.
    // ⛔ Which member is *named* is load-bearing where the pointee is read; that
    // choice is made once, below, against the negotiated version.
    let um_callbacks_raw = unsafe { a.__bindgen_anon_1.p12UMCallbacks_0062 };
    if um_callbacks_raw.is_null() {
        log_error!("CreateDevice: null p12UMCallbacks -> E_INVALIDARG");
        note_refusal(&UMD12_REFUSALS.create_device_bad_arg);
        return E_INVALIDARG;
    }

    // ── 1. The closed interface dispatch ────────────────────────────────────
    let Some(negotiated) = Ddi12Interface::from_pair(a.Interface, a.Version) else {
        // ⛔ `ARCHITECTURE.md` §12 trap 2, refused instead of guessed: treating
        // an unknown interface as the newest known one is what bulk-filled 150
        // slots into a 101-slot table. Here there is no arm that builds anything
        // for an unrecognised pair, so the mistake is unrepresentable rather
        // than merely avoided.
        log_error!(
            "CreateDevice: UNADVERTISED Interface={:#010x} Version={:#010x} -> \
             DXGI_ERROR_UNSUPPORTED",
            a.Interface,
            a.Version,
        );
        note_refusal(&UMD12_REFUSALS.ddi12_version_mismatch);
        return DXGI_ERROR_UNSUPPORTED;
    };

    log_error!(
        "CreateDevice: {} hRTDevice={:p} hDrvDevice={:p} pKTCallbacks={:p} \
         p12UMCallbacks={:p} Flags={:#x} NumReserveRanges={}",
        negotiated.name(),
        a.hRTDevice.handle,
        a.hDrvDevice.pDrvPrivate,
        a.pKTCallbacks,
        um_callbacks_raw,
        a.Flags,
        a.NumReserveRanges,
    );

    // ⚠ `pReserveRanges` / `NumReserveRanges` are read and reported, not acted
    // on. They are the GPU-virtual-address ranges the runtime asks the driver to
    // reserve at device creation, and honouring them is L4's (resources, heaps,
    // GPU VA). Counting a non-empty request makes "we ignored it" a number
    // rather than a silence.
    if a.NumReserveRanges != 0 {
        note_refusal(&UMD12_REFUSALS.reserve_ranges_ignored);
    }

    // ── 2. Bring the engine up ──────────────────────────────────────────────
    //
    // ⚠ `(0, 0)` means "do not match on LUID", and it is what the D3D11 driver
    // passes too (`umd/src/adapter.rs:390`). Three reasons it is the honest
    // value here rather than a shortcut:
    //   * a D3D12 UMD has no supported way to obtain its adapter's LUID —
    //     `D3D12DDIARG_OPENADAPTER` does not carry one, and
    //     `pfnQueryAdapterInfoCb` returns the KMD's *private* adapter data, not
    //     an identity;
    //   * vkd3d does not LUID-match anyway. `helios_entry.c:172-179` passes
    //     `vk_physical_device = VK_NULL_HANDLE` and delegates to
    //     `vkd3d_select_physical_device`, whose own comment says it "is NOT LUID
    //     matching" — so a *correct* LUID would change nothing today;
    //   * the guest is single-GPU, so there is one physical device to pick.
    // ⛔ None of that makes it right on a multi-adapter guest, and that is
    // `ARCHITECTURE.md` §13 UNVERIFIED-11.
    let Some(engine) = BridgeDevice12::create(0, 0) else {
        // The C++ side has already logged the engine's HRESULT.
        log_error!("CreateDevice: vkd3d device creation FAILED -> E_FAIL");
        note_refusal(&UMD12_REFUSALS.create_device_engine_failed);
        return E_FAIL;
    };

    // ── 3. Construct in place, under the guard ──────────────────────────────
    let device = a.hDrvDevice.pDrvPrivate.cast::<HeliosD3D12Device>();
    // SAFETY: `pDrvPrivate` is non-null (checked above) and points at the block
    // the runtime allocated using the size THIS driver returned from
    // `pfnCalcPrivateDeviceSize`, which is `device_private_size(Flags)` — the
    // same function, so the block is exactly large enough and correctly aligned
    // for `HeliosD3D12Device`. `write` initialises it without reading whatever
    // the runtime left there.
    unsafe {
        core::ptr::write(
            device,
            HeliosD3D12Device {
                h_rt_device: a.hRTDevice,
                negotiated,
                flags: a.Flags,
                um_callbacks: um_callbacks_raw,
                kt_callbacks: a.pKTCallbacks,
                engine,
            },
        );
    }
    let guard = DeviceUnderConstruction { device };

    // ⚠ Every fallible step from here on must return WITHOUT defusing, so the
    // guard tears the engine device down. There are none today; there will be
    // once L2 mints WDDM contexts and L4 honours `pReserveRanges`, and this is
    // where they go.

    // ── 4. Hand it to the runtime ───────────────────────────────────────────
    guard.defuse();
    S_OK
}

/// `pfnDestroyDevice`.
///
/// ⚠ Lives on the **adapter** table (`d3d12umddi.h:13649`), not the device
/// table — a shape difference from D3D11 and, per `DDI_REFERENCE.md` §1.3, a
/// classic place to leave a NULL.
///
/// Returns `VOID`: there is no way to report a failure, which is the general
/// D3D12 shape (`DECISIONS.md` §7.6) and the reason the counters matter.
///
/// # Safety
/// `h_device` must be a handle this driver's [`create_device`] returned `S_OK`
/// for and which has not already been destroyed. Passing it twice is a double
/// free.
pub(crate) unsafe fn destroy_device(h_device: ddi12::D3D12DDI_HDEVICE) {
    // SAFETY: the caller guarantees a live, not-yet-destroyed handle from
    // `create_device`. Borrowed only to produce the line below, and dropped
    // before the `drop_in_place` that follows.
    let Some(dev) = (unsafe { device(h_device) }) else {
        note_refusal(&UMD12_REFUSALS.destroy_device_bad_arg);
        return;
    };
    log_device_teardown(dev);

    let device = h_device.pDrvPrivate.cast::<HeliosD3D12Device>();
    // SAFETY: the same live handle, which `core::ptr::write` initialised at
    // exactly this address. Dropping in place drops `BridgeDevice12`, which runs
    // the out-of-line C++ destructor and releases the engine's `ID3D12Device` —
    // and with it the Vulkan device, its memory and its queues. ⛔ The runtime
    // owns the memory itself and frees it; this must not.
    unsafe { core::ptr::drop_in_place(device) };
}

/// One line per device teardown: what the runtime gave this device, and what it
/// did with it.
///
/// ⭐ Not decoration — it is the readout for three things that have no other
/// channel, and dwm owns **several** devices in one process (20th session), so
/// per-device is the granularity that matters:
///
/// * **the two callback-table pointers.** The corelayer arm is the
///   `ARCHITECTURE.md` §12 trap 2 landmine in its D3D12 form (four union arms of
///   12/14/17/18 members). *"Did every device get the same `_0062` table?"* is a
///   real question with a real failure mode, and this is what answers it.
/// * **the engine's venus context id**, per device rather than per process —
///   S4b's evidence channel at the granularity S4b could not reach. ⚠ It is a
///   *degraded read* at 0 (no ICD, or one too old), never an anchor failure: a
///   genuine anchor mismatch refuses device creation, so there would be no
///   device here to ask.
/// * **the counters and the noop hits at device scope**, which says what *this*
///   device touched rather than what the whole adapter did.
fn log_device_teardown(dev: &HeliosD3D12Device) {
    log_error!(
        "DestroyDevice: {} hRTDevice={:p} Flags={:#x} p12UMCallbacks={:p} pKTCallbacks={:p} \
         venusCtx={}",
        dev.negotiated.name(),
        dev.h_rt_device.handle,
        dev.flags,
        dev.um_callbacks,
        dev.kt_callbacks,
        dev.engine.venus_context_id(),
    );
    crate::log_refusal_summary();
    crate::forward12::noop12::log_noop_hits();
}

/// Borrow the device behind a DDI handle.
///
/// ⛔ **The D3D12 soundness argument is re-derived here and NOT inherited from
/// D3D11**, which is what `umd_common::slot` demands in as many words
/// (`slot.rs:304-322`) and `PARALLEL.md` §9.4 restates: the D3D11 `&'static S`
/// argument rests on the runtime's `CUseCountedObject` first-created /
/// last-destroyed ordering, and *"`d3d12umddi` has no THREADING cap and no
/// `CUseCountedObject` statement has been located for it."*
///
/// The argument this function stands on is narrower and does not depend on that:
///
/// * the returned reference is **not** `'static` — it borrows for as long as the
///   caller's own binding lives, and every caller is one DDI invocation;
/// * `pfnDestroyDevice` is the only path that drops the block, and it is the
///   **device** object — the coarsest one there is. A runtime that could destroy
///   a device while another DDI held the same `hDrvDevice` could not implement
///   D3D12 at all;
/// * ⚠ concurrent **reads** across FREETHREADED worker threads are permitted by
///   `&` and are the expected case. A lane that needs interior mutation puts an
///   atomic or a lock in [`HeliosD3D12Device`] — it does not get a `&mut` from
///   here, and there is deliberately no `device_mut`.
///
/// ⚠ Even inside `destroy_device` the borrow is dropped before the
/// `drop_in_place`, so no reference is live across the teardown.
///
/// # Safety
/// `h_device` must be a live handle from [`create_device`] that
/// [`destroy_device`] has not been called on, and the returned reference must
/// not outlive the DDI call that obtained it.
pub(crate) unsafe fn device<'a>(
    h_device: ddi12::D3D12DDI_HDEVICE,
) -> Option<&'a HeliosD3D12Device> {
    let device = h_device.pDrvPrivate.cast::<HeliosD3D12Device>();
    if device.is_null() {
        return None;
    }
    // SAFETY: non-null per the check, and the caller guarantees it is a live
    // block `create_device` wrote and `destroy_device` has not dropped.
    Some(unsafe { &*device })
}

/// Report a device-scope failure to the runtime. Returns whether it was
/// delivered.
///
/// ⭐ **This is how a `VOID`-returning D3D12 DDI says it failed**, and most of
/// them are `VOID` (`DECISIONS.md` §7.6). It did not exist at S6-0b, and its
/// absence was the rule rather than an oversight — `PARALLEL.md` §10 forbids
/// `#[allow(dead_code)]` on a hand-written line (R908) and no handler could yet
/// fail. The S6 Round-1 lanes are what made it reachable: L2's queue-table fence
/// ops, L5's ten `VOID` view/heap slots and L6's thirteen `VOID` shader and
/// sub-state creates all fail with no other channel.
///
/// ⚠ **Three of the four lanes wrote this function independently, in isolated
/// worktrees, and converged on the same contract** — including the non-obvious
/// half, that it returns a `bool` instead of counting. That agreement is the
/// reason the shape below is stated as a rule rather than as one lane's taste.
///
/// Two things the S6-0b comment this replaced flagged as *looking* like
/// mistakes. Both are still true, both are load-bearing, and all three lanes
/// re-derived them:
///
/// * `PFND3D12DDI_SETERROR_CB` takes a `D3D10DDI_HRTDEVICE`, **not** the
///   `D3D12DDI_HRTDEVICE` the create arg carries. They are the same one-pointer
///   struct — `D3D12DDI_HRTDEVICE` is a bindgen *alias* — so `h_rt_device`
///   passes with no cast;
/// * a null `pfnSetErrorCb` must be **counted**. It is the only channel a `VOID`
///   slot has, so losing it turns an error into corrupt output instead of a
///   removed device.
///
/// ⛔ **Which is why the counter belongs to the CALLER, and why this is
/// `#[must_use]`.** Eleven lanes reach this function and each keeps its refusal
/// set in its own file (`PARALLEL.md` §9.1, and `lib.rs`'s `UMD12_REFUSAL_SETS`).
/// A counter named *here* would either have to live in `lib.rs` — the merge point
/// that whole scheme exists to remove — or bind this shared function to one
/// lane's set. `RefusalCounter::note` is `#[must_use]` for the same reason.
///
/// ⚠ Safe, not `unsafe`, and that is a deliberate choice between the two shapes
/// the lanes submitted. The only raw operation is reading `um_callbacks`, whose
/// validity is a **type invariant of [`HeliosD3D12Device`]** rather than a caller
/// obligation: [`create_device`] is the sole constructor, it null-checks the
/// pointer before writing the struct, the field is never reassigned, and there is
/// deliberately no `device_mut` through which it could be. An `unsafe fn` whose
/// precondition no caller can violate teaches callers to stop reading
/// `# Safety` sections.
#[must_use = "a device-scope error that could not be reported must be counted by the caller"]
pub(crate) fn set_error(dev: &HeliosD3D12Device, hr: Hresult) -> bool {
    if dev.um_callbacks.is_null() {
        return false;
    }
    // SAFETY: non-null per the check above, and `create_device` stored the arm
    // the negotiated version selects — D12 advertises exactly one token, so
    // `_0062` is the only reachable shape — for a table the runtime keeps alive
    // at least as long as the device. The borrow does not outlive this call.
    let Some(set_error_cb) = (unsafe { (*dev.um_callbacks).pfnSetErrorCb }) else {
        return false;
    };
    // SAFETY: the runtime supplied this callback for this device, and
    // `h_rt_device` is the handle it supplied alongside it and owns until
    // `pfnDestroyDevice`. The call transfers no ownership.
    unsafe { set_error_cb(dev.h_rt_device, hr) };
    true
}
