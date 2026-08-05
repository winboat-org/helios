//! `OpenAdapter12` and the eight `D3D12DDI_ADAPTERFUNCS_0109` slots — **stage
//! S5, the commit that makes this body reachable.**
//!
//! `DECISIONS.md` §7.1 / R908: *`OpenAdapter12` must stop refusing in the same
//! commit that makes its body reachable, or the body must not be written yet.*
//! This file is the second half of that sentence. Everything in it is reached
//! from the export below, and the export is registered at
//! `UserModeDriverName[3]` by the same commit.
//!
//! Modelled on `umd/src/adapter.rs`, which is the same eight-step shape for
//! D3D11. Two things there must not be reinvented and are not:
//!
//! * the **closed enum + exhaustive match** on the negotiated interface
//!   (`ARCHITECTURE.md` §12 trap 2). The D3D11 `if/else-if/else` treated
//!   "unknown or older" as D3D11.0 and bulk-filled 150 pointer slots into a
//!   table sized for 101 — *a 376..392 byte out-of-bounds write into the
//!   runtime's heap*;
//! * the ZST **adapter token** (`umd/src/adapter.rs:120-121`), address-taken, so
//!   a handle that is not ours is countable rather than dereferenced.
//!
//! # What is real here and what refuses, at S5
//!
//! | slot | S5 |
//! |---|---|
//! | `pfnGetSupportedVersions` | **real** — the one-token set, D12 |
//! | `pfnGetOptionalDDITables` | **real** — `*puEntries = 0`, the measured-correct answer (`DDI_REFERENCE.md` §2.2) |
//! | `pfnCloseAdapter` | **real** — validates the handle and dumps the refusal set |
//! | `pfnGetCaps` | refuses, counted, and **logs every caps type it was asked** |
//! | `pfnFillDDITable` | refuses, counted, and **logs `(type, size, index)`** — S6-0 |
//! | `pfnCalcPrivateDeviceSize` | 0, counted — S6-0 |
//! | `pfnCreateDevice` | refuses, counted — S6-0 |
//! | `pfnDestroyDevice` | counted; no device can exist yet |
//!
//! ⛔ Those are **documented refusals with named counters, not silent stubs**
//! (CLAUDE.md rule 2). Each is reached by the runtime on a knob-ON adapter open,
//! so none of it is the unreachable scaffolding R908 deleted.
//!
//! ⭐ The two bounded log lines are not decoration. `D12-G5` had to be run
//! against WARP through a spy proxy to learn which caps types this runtime asks
//! for and what `TableSize` it passes; from S5 the same contract is recorded on
//! **our own adapter**, for free, every time the knob is on — which is exactly
//! the input L1 (caps) and S6-0 (the tables) need.
//!
//! # The order the runtime calls these in
//!
//! `ARCHITECTURE.md` §1.2, steps 7-12: `pfnGetSupportedVersions` →
//! `pfnGetCaps` ×43 → `pfnGetOptionalDDITables` → `pfnFillDDITable` ×N →
//! `pfnCalcPrivateDeviceSize` → `pfnCreateDevice`. ⚠ **`pfnFillDDITable` runs
//! before `pfnCreateDevice`**, i.e. the tables are adapter-scoped and filled
//! before any device exists — the opposite of the D3D11 shape, where
//! `CreateDevice` fills the device-funcs table itself. At S5 the runtime never
//! gets past `pfnGetCaps`, and it says so in English on ETW: *"Driver did not
//! respond to D3D12DDICAPS_TYPE_D3D12_OPTIONS caps query."*

use core::ffi::c_void;

use helios_umd_common::hr::{Hresult, DXGI_ERROR_UNSUPPORTED, E_INVALIDARG, E_OUTOFMEMORY, S_OK};

use crate::ddi12;
use crate::knobs12;
use crate::{init_once, log_error, log_refusal_summary, note_refusal, UMD12_REFUSALS};

// ---------------------------------------------------------------------------
// Version negotiation — `DECISIONS.md` D12
// ---------------------------------------------------------------------------

/// Compose a `D3D12DDI_SUPPORTED_*` token from the header's own two halves.
///
/// ⛔ **The token is composed, never transcribed.** `DECISIONS.md` §7.2 bans
/// hand-written DDI ABI values, and `DDI_REFERENCE.md` §1.5 records a worked
/// example in the research corpus that got `_0080` wrong by assuming the build
/// half is the decimal `NNNN` (it is the rev digit below `_0090` and the full
/// number from `_0090` up). Both inputs below are bindgen'd `#define`s.
///
/// ⚠ bindgen does **not** emit `D3D12DDI_SUPPORTED_0110` itself — that macro
/// casts through `(UINT64)`, which bindgen cannot constant-fold — so composing
/// from the two halves it *does* emit is the only generated-source route to the
/// value. The formula is `d3d12umddi.h:37-56` verbatim:
/// `((UINT64)INTERFACE_VERSION_Rn << 32) | ((UINT64)BUILD_VERSION_NNNN << 16)`.
const fn ddi12_supported(interface_version: u32, build_version: u32) -> u64 {
    ((interface_version as u64) << 32) | ((build_version as u64) << 16)
}

/// The DDI interface versions `pfnGetSupportedVersions` advertises.
///
/// ⛔ **Exactly one entry, and that is `DECISIONS.md` D12's load-bearing half.**
/// With a one-element set the runtime either negotiates `_0110` or fails the
/// handshake with its own string (*"Failed to find matching DDI versions"*), so
/// there is exactly one legal `(Interface, Version)` pair and exactly one legal
/// table shape. A second entry would make a second table shape reachable —
/// which is precisely the `ARCHITECTURE.md` §12 trap 2 / R702 surface that
/// D12 closes by construction rather than by guarding.
const SUPPORTED_DDI_VERSIONS: &[u64] = &[ddi12_supported(
    ddi12::D3D12DDI_INTERFACE_VERSION_R8,
    ddi12::D3D12DDI_BUILD_VERSION_0110,
)];

/// The negotiated DDI interface, as a **closed set**.
///
/// `ARCHITECTURE.md` §12 trap 2: never let an unknown interface fall into an
/// `else` that fills the largest table. The D3D11 driver paid 376..392 bytes of
/// the runtime's heap to learn that. Here the set has one member, so the match
/// below is exhaustive with a single legal arm and every other pair is a
/// counted refusal.
#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) enum Ddi12Interface {
    /// `D3D12DDI_SUPPORTED_0110` — release R8, build 110. Fills the
    /// `_0109`-generation tables (D12: `_0110` adds no table struct of its own).
    R8_0110,
}

impl Ddi12Interface {
    /// The high 32 bits of the token: `(12 << 16) | MINOR_VERSION_R8`.
    const R8_0110_INTERFACE: u32 = ddi12::D3D12DDI_INTERFACE_VERSION_R8;
    /// The low 32 bits: `BUILD_VERSION_0110 << 16`.
    const R8_0110_VERSION: u32 = ddi12::D3D12DDI_BUILD_VERSION_0110 << 16;

    /// ✅ `D12-G5` confirmed the split rather than inferring it:
    /// `D3D12DDIARG_CREATEDEVICE::Interface` carries the token's **high** 32
    /// bits and `::Version` its **low** 32 bits, and
    /// `((u64)Interface << 32) | Version` matched the advertised entry bit for
    /// bit. Matching on the pair keeps this site independent of that split
    /// being re-derived correctly a second time.
    ///
    /// Panic-free: a match over two `u32`s, no indexing.
    fn from_pair(interface: u32, version: u32) -> Option<Self> {
        match (interface, version) {
            (Self::R8_0110_INTERFACE, Self::R8_0110_VERSION) => Some(Self::R8_0110),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::R8_0110 => "_0110",
        }
    }
}

// Keep the enum and the advertised set in lockstep at COMPILE time, the way
// `umd/src/adapter.rs:90-95` does: adding a token without adding a variant
// fails here, and adding a variant without a dispatch arm fails the exhaustive
// match in `from_pair`. That is the property the `else`-as-default did not have.
const _: () = {
    assert!(SUPPORTED_DDI_VERSIONS.len() == 1);
    assert!((SUPPORTED_DDI_VERSIONS[0] >> 32) as u32 == Ddi12Interface::R8_0110_INTERFACE);
    assert!(SUPPORTED_DDI_VERSIONS[0] as u32 == Ddi12Interface::R8_0110_VERSION);
};

// `D3D12DDIARG_OPENADAPTER::pAdapterFuncs` is typed `D3D12DDI_ADAPTERFUNCS*`
// (the base shape), but the version is not negotiated until `pfnGetSupportedVersions`
// runs — which is AFTER this table is written (`ARCHITECTURE.md` §1.2, steps 6-7).
// So the driver must commit to one shape before it knows the answer.
//
// That is sound here only because the two shapes are byte-identical: 8 slots,
// 64 bytes, same offsets, differing solely in `pfnCreateDevice`'s *signature*
// (`_0003` vs `_0109`), which is one pointer either way. Asserted rather than
// asserted-in-prose, because it is the premise of the cast in `OpenAdapter12`.
//
// ⚠ The signature difference is still real and is handled by D12, not by this
// assert: `D3D12DDIARG_CREATEDEVICE_0109` is `_0003` plus two trailing fields,
// so reading it against a `_0003` arg would read past the end. Advertising one
// token means a `_0003`-generation create can never be negotiated.
const _: () = {
    assert!(
        core::mem::size_of::<ddi12::D3D12DDI_ADAPTERFUNCS>()
            == core::mem::size_of::<ddi12::D3D12DDI_ADAPTERFUNCS_0109>()
    );
    assert!(
        core::mem::offset_of!(ddi12::D3D12DDI_ADAPTERFUNCS, pfnCreateDevice)
            == core::mem::offset_of!(ddi12::D3D12DDI_ADAPTERFUNCS_0109, pfnCreateDevice)
    );
    assert!(
        core::mem::offset_of!(ddi12::D3D12DDI_ADAPTERFUNCS, pfnDestroyDevice)
            == core::mem::offset_of!(ddi12::D3D12DDI_ADAPTERFUNCS_0109, pfnDestroyDevice)
    );
};

// ---------------------------------------------------------------------------
// The adapter identity token
// ---------------------------------------------------------------------------

/// The value handed back as this adapter's `pDrvPrivate`.
///
/// A zero-sized type, address-taken — `umd/src/adapter.rs:120-121` (R821) and
/// the same reasoning: a ZST says *"this pointer is not dereferenceable state"*
/// in a way a `usize` carrying a magic number does not.
///
/// ⚠ D3D12 has no per-adapter driver state to keep here. Every adapter-scoped
/// answer is a constant of the build (the version set, the caps policy), and
/// `hRTAdapter` is not stashed because nothing in this driver needs it: the
/// D3D11 side stashes it for `pfnEscapeCb` through the scan-out acquire path
/// (`umd/src/adapter.rs:225`), which is a D3D11 present-vehicle mechanism this
/// DLL deliberately does not have (`probe12`'s module doc: a D3D12 export in the
/// `helios_umd_*` family would steal the D3D11 vehicle). The first D3D12 caller
/// that needs it adds a field here and says why.
struct AdapterToken;
static ADAPTER_TOKEN: AdapterToken = AdapterToken;

/// Validate an adapter handle against the token we handed out. **Reports only.**
///
/// Deliberately not a refusal, exactly as `umd/src/adapter.rs:132-149` is: the
/// counter has to be observed at zero on a real boot before any DDI starts
/// rejecting on it. Returning `bool` rather than nothing keeps that decision at
/// the call site if it ever changes.
fn adapter_ok(h: ddi12::D3D12DDI_HADAPTER) -> bool {
    let expected = core::ptr::addr_of!(ADAPTER_TOKEN) as *const c_void;
    if core::ptr::eq(h.pDrvPrivate as *const c_void, expected) {
        return true;
    }
    UMD12_REFUSALS.adapter_unrecognised.bump();
    let n = UMD12_REFUSALS.adapter_unrecognised.get();
    if n <= LOG_BUDGET {
        log_error!(
            "adapter handle not ours: pDrvPrivate={:p} expected={:p} (x{n}) — counted only",
            h.pDrvPrivate,
            expected,
        );
    }
    false
}

/// How many times a bounded, per-site evidence line may repeat.
///
/// ⚠ These lines are one-shot contract capture, not per-op tracing, so they are
/// not behind `Umd12Trace`: a caps gauntlet is ~43 lines *per adapter open* and
/// the whole point is that it is readable without having set a knob first. The
/// budget is what keeps a pathological caller from turning that into a log
/// flood. `helios_umd_common::throttle` is the per-op mechanism and is
/// deliberately not what these use — it exists for repeat traffic on a hot
/// path, and none of these sites is on one.
const LOG_BUDGET: usize = 64;

// ---------------------------------------------------------------------------
// The export
// ---------------------------------------------------------------------------

/// The D3D12 adapter entry point — **reachable as of S5.**
///
/// The loader resolves this by name out of `UserModeDriverName[3]`, so a missing
/// export is a different and worse failure than a clean refusal.
///
/// Steps, matching `ARCHITECTURE.md` §1.2 rows 1-6:
///
/// 1. `init_once()` — name this DLL's log file **above the first log line**;
/// 2. the `UmdD3D12` kill switch (D11). Absent ⇒ `DXGI_ERROR_UNSUPPORTED`,
///    i.e. bit-identical to a build with no D3D12 path;
/// 3. validate `open_data` and the two out-pointers inside it;
/// 4. hand out the adapter token;
/// 5. fill all **8** slots of `D3D12DDI_ADAPTERFUNCS_0109`.
///
/// ⚠ There is no `Interface`/`Version` in `D3D12DDIARG_OPENADAPTER` — unlike
/// `D3D10DDIARG_OPENADAPTER`, which is why `umd/src/adapter.rs` can dispatch
/// inside `OpenAdapter10` and this cannot (`DDI_REFERENCE.md` §1.2). All
/// negotiation is `pfnGetSupportedVersions` + `pfnGetOptionalDDITables` +
/// `pfnFillDDITable`, all of them **after** this returns.
///
/// # Safety
/// `open_data` is the runtime's `D3D12DDIARG_OPENADAPTER*`. It must point at a
/// live, writable, correctly aligned `D3D12DDIARG_OPENADAPTER` for the duration
/// of the call, and its `pAdapterFuncs` must point at a writable
/// `D3D12DDI_ADAPTERFUNCS`-sized (64-byte) table the runtime owns.
///
/// ⛔ The 64 bytes are the contract, and this body writes exactly that many:
/// it stores an `D3D12DDI_ADAPTERFUNCS_0109` through a cast pointer, whose size
/// and field offsets are asserted equal to the declared type's at the top of
/// this file. Writing a table sized for a version the runtime did not ask for
/// is `ARCHITECTURE.md` §12 trap 2 — a 376..392-byte out-of-bounds write into
/// the runtime's heap.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn OpenAdapter12(open_data: *mut c_void) -> Hresult {
    // ⛔ FIRST, above this entry point's first log line. `log::init`'s basename
    // defaults to `"umd"` and the log PATH is a `OnceLock` latched by the first
    // line of any kind, so arriving late puts this driver's evidence in D3D11's
    // file permanently. See `crate::init_once`.
    init_once();

    // ── 2. The kill switch (D11) ────────────────────────────────────────────
    // ⛔ Above every other check, including the null test, and that ordering is
    // deliberate: with the knob absent this function must be indistinguishable
    // from the pre-S5 refusal, which examined nothing. A null-argument counter
    // that could tick on a knob-OFF machine would make "D3D12 is off" and "the
    // runtime handed us a bad pointer" share an evidence channel.
    //
    // ⚠ dwm.exe already calls this in production (`DECISIONS.md` §7.13). The
    // first boot with `UmdD3D12=1` is a change to the compositor.
    if !knobs12::umd_d3d12() {
        // ⚠ ONE log line for one event, and it is the set summary rather than a
        // bespoke "OpenAdapter12 refused" line beside it. R911: an already-loud
        // arm must not also emit the summary, and the inverse holds too — a
        // second line saying what `OpenAdapter12=1` already says makes the count
        // and the prose two things that can disagree.
        note_refusal(&UMD12_REFUSALS.open_adapter12);
        // ⛔ Declining an unimplemented DDI is DXGI_ERROR_UNSUPPORTED
        // (0x887A_0004), NEVER DXGI_ERROR_DRIVER_INTERNAL_ERROR (0x887A_0020) —
        // the latter is recorded by the runtime and by ETW as a *driver fault*,
        // so a client's ordinary "this driver has no D3D12 DDI" negotiation
        // would be logged as a Helios bug. `umd`'s copy of this export returned
        // the wrong one until R801 because the two shared a constant name and
        // both printed identically in our own logs.
        return DXGI_ERROR_UNSUPPORTED;
    }

    // ── 3. Validate ─────────────────────────────────────────────────────────
    if open_data.is_null() {
        note_refusal(&UMD12_REFUSALS.open_adapter12_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: non-null per the check above, and the caller guarantees a live,
    // aligned, writable `D3D12DDIARG_OPENADAPTER` for the duration of the call.
    let open = unsafe { &mut *open_data.cast::<ddi12::D3D12DDIARG_OPENADAPTER>() };

    let funcs = open.pAdapterFuncs;
    if funcs.is_null() {
        note_refusal(&UMD12_REFUSALS.open_adapter12_bad_arg);
        return E_INVALIDARG;
    }

    log_error!(
        "OpenAdapter12: knob ON, hRTAdapter={:p} pAdapterCallbacks={:p} advertising {:#018x?}",
        open.hRTAdapter.handle,
        open.pAdapterCallbacks,
        SUPPORTED_DDI_VERSIONS,
    );

    // ── 4. The driver's adapter handle ──────────────────────────────────────
    open.hAdapter.pDrvPrivate = core::ptr::addr_of!(ADAPTER_TOKEN) as *mut c_void;

    // ── 5. All eight slots ──────────────────────────────────────────────────
    // Built as a value and written once, rather than eight field stores through
    // a raw pointer: a `D3D12DDI_ADAPTERFUNCS_0109` literal cannot leave a slot
    // NULL, because the struct has no `..Default::default()` here and the
    // compiler requires every field. "The runtime calls through an
    // uninitialised slot" is the failure this shape makes unrepresentable.
    let table = ddi12::D3D12DDI_ADAPTERFUNCS_0109 {
        pfnCalcPrivateDeviceSize: Some(calc_private_device_size),
        pfnCreateDevice: Some(create_device),
        pfnCloseAdapter: Some(close_adapter),
        pfnGetSupportedVersions: Some(get_supported_versions),
        pfnGetCaps: Some(get_caps),
        pfnGetOptionalDDITables: Some(get_optional_ddi_tables),
        pfnFillDDITable: Some(fill_ddi_table),
        pfnDestroyDevice: Some(destroy_device),
    };
    // SAFETY: `funcs` is non-null per the check above and the caller guarantees
    // it points at a writable `D3D12DDI_ADAPTERFUNCS` the runtime owns. The cast
    // to the `_0109` shape writes exactly the same 64 bytes at the same offsets
    // — asserted at compile time at the top of this file — and D12's one-token
    // set makes the `_0003`-generation `pfnCreateDevice` signature unreachable.
    unsafe {
        core::ptr::write(funcs.cast::<ddi12::D3D12DDI_ADAPTERFUNCS_0109>(), table);
    }

    S_OK
}

// ---------------------------------------------------------------------------
// The eight slots
//
// ⛔ Every one is `unsafe extern "C"`, not `extern "system"`. Measured by
// fault-injection against the host cross-check (`PARALLEL.md` §5): the
// `d3d12umddi` PFN typedefs are `extern "C"`. On x86_64 Windows the two are the
// same ABI, so this is a *type* error and not a calling-convention bug — which
// is exactly why it would otherwise have been written wrong 214 times and
// caught by nothing until the first compile. ⚠ Note this differs from
// `OpenAdapter12` above, which the loader resolves by name and which keeps the
// D3D11 side's `extern "system"`.
// ---------------------------------------------------------------------------

/// `pfnGetSupportedVersions` — the count-then-fill idiom, D12's one-token set.
///
/// `_Inout_ UINT32* puEntries` + `_Out_writes_opt_(*puEntries)`: the runtime may
/// call with a null buffer to learn the count, then again with storage.
/// ⚠ **UNVERIFIED** that this runtime actually makes the first call with a null
/// buffer (`DDI_REFERENCE.md` §1.3); this handles both shapes, and the log line
/// below records which one arrived — settling it as a side effect of S5 rather
/// than needing the §15 spy again.
unsafe extern "C" fn get_supported_versions(
    h_adapter: ddi12::D3D12DDI_HADAPTER,
    entries: *mut ddi12::UINT32,
    supported_versions: *mut ddi12::UINT64,
) -> ddi12::HRESULT {
    let _ = adapter_ok(h_adapter);

    if entries.is_null() {
        note_refusal(&UMD12_REFUSALS.get_supported_versions_bad_arg);
        return E_INVALIDARG;
    }

    // SAFETY: non-null per the check above; the DDI declares it `_Inout_`, so
    // the runtime guarantees a live, writable `UINT32` for the call.
    let requested = unsafe { *entries };
    log_error!(
        "GetSupportedVersions: requested={requested} bufNull={} advertising {:#018x?}",
        supported_versions.is_null(),
        SUPPORTED_DDI_VERSIONS,
    );
    // SAFETY: as above. Written before the early return below, because the
    // count-query form's whole purpose is this store.
    unsafe { *entries = SUPPORTED_DDI_VERSIONS.len() as ddi12::UINT32 };

    if supported_versions.is_null() {
        return S_OK;
    }
    if (requested as usize) < SUPPORTED_DDI_VERSIONS.len() {
        // ⚠ Not a refusal counter: the runtime asking with a short buffer is a
        // legal first half of the count-then-fill idiom, and `*entries` above
        // has already told it the real count. Counting it would put a normal
        // negotiation in a line whose whole purpose is to read zero.
        return E_OUTOFMEMORY;
    }

    for (index, version) in SUPPORTED_DDI_VERSIONS.iter().enumerate() {
        // SAFETY: the runtime declared storage for `requested` entries and
        // `requested >= SUPPORTED_DDI_VERSIONS.len()` was just checked, so every
        // index in this loop is inside the buffer.
        unsafe { *supported_versions.add(index) = *version };
    }
    S_OK
}

/// `pfnGetCaps` — **refuses at S5**, and records which caps type it was asked.
///
/// L1 owns `caps12.rs` and the 43-enumerator gauntlet (`PARALLEL.md` §4, §8:
/// one agent, whole — `D3D12Core.dll` enforces ~60 cross-tier consistency rules
/// and advertising an unbacked tier is a lie the OS acts on). Refusing every
/// type until then is the honest answer, and it is also what stops S5's
/// knob-ON adapter from creating a device: the runtime fails device creation
/// here, with its own English string on ETW.
///
/// ⭐ The bounded line is the deliverable. `D12-G5` needed a spy proxy in front
/// of WARP to learn this runtime's caps call order; from here it is recorded on
/// the Helios adapter itself, which is L1's input.
unsafe extern "C" fn get_caps(
    h_adapter: ddi12::D3D12DDI_HADAPTER,
    arg: *const ddi12::D3D12DDIARG_GETCAPS,
) -> ddi12::HRESULT {
    let _ = adapter_ok(h_adapter);

    UMD12_REFUSALS.get_caps_unimplemented.bump();
    let n = UMD12_REFUSALS.get_caps_unimplemented.get();
    if n <= LOG_BUDGET {
        if arg.is_null() {
            log_error!("GetCaps: null arg (x{n}) -> DXGI_ERROR_UNSUPPORTED");
        } else {
            // SAFETY: non-null per the branch. The DDI declares it `_In_ CONST`,
            // so the runtime guarantees a live, aligned `D3D12DDIARG_GETCAPS`
            // for the duration of the call. Read only.
            let caps = unsafe { &*arg };
            log_error!(
                "GetCaps: type={} pInfo={:p} pData={:p} DataSize={} (x{n}) -> \
                 DXGI_ERROR_UNSUPPORTED (L1 owns caps12.rs)",
                caps.Type,
                caps.pInfo,
                caps.pData,
                caps.DataSize,
            );
        }
    }
    // ⛔ Nothing is written to `pData`. A caps answer this driver has not
    // decided is a lie the OS acts on (`DECISIONS.md` §7.8); an unwritten
    // buffer plus a failure HRESULT is the one shape that cannot become one.
    DXGI_ERROR_UNSUPPORTED
}

/// `pfnGetOptionalDDITables` — **real**: this driver wants no extra tables.
///
/// ✅ The measured-correct answer (`DDI_REFERENCE.md` §2.2): WARP answers
/// `*puEntries = 0` and the runtime still fills two command-list tables, so the
/// second table is not something a driver asks for. The runtime also states the
/// only legal use of this entry point in its own strings — *"…only supports
/// `D3D12DDI_TABLE_TYPE_COMMAND_LIST_3D`. An unsupported table type was
/// requested."* — so 0 is the answer that cannot be misread.
unsafe extern "C" fn get_optional_ddi_tables(
    h_adapter: ddi12::D3D12DDI_HADAPTER,
    entries: *mut ddi12::UINT32,
    requests: *mut ddi12::D3D12DDI_TABLE_REQUEST,
) -> ddi12::HRESULT {
    let _ = adapter_ok(h_adapter);

    if entries.is_null() {
        note_refusal(&UMD12_REFUSALS.get_optional_ddi_tables_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: non-null per the check above; the DDI declares it `_Inout_`.
    let requested = unsafe { *entries };
    log_error!(
        "GetOptionalDDITables: requested={requested} bufNull={} -> 0 tables",
        requests.is_null(),
    );
    // SAFETY: as above. ⛔ `requests` is deliberately NOT written: with
    // `*entries = 0` there is no element to write, and touching a
    // `_Out_writes_opt_(*puEntries)` buffer past the count it was just given is
    // the same class of overrun as writing a table sized for the wrong version.
    unsafe { *entries = 0 };
    S_OK
}

/// `pfnFillDDITable` — **refuses at S5**, and records the runtime's own sizes.
///
/// S6-0 owns this: it is where all 214 slots get their counting noops, and it is
/// where `hRTTable` must be stashed per index, because there is no other way to
/// obtain the handle `pfnSetCommandListDDITableCb` later needs
/// (`DDI_REFERENCE.md` §2.2).
///
/// ⛔ **The `SIZE_T` is the contract.** `ARCHITECTURE.md` §12 rule 16 / R702:
/// 24H2 passed 576 bytes for a 592-byte `DRIVERCAPS` and the D3D11 driver wrote
/// past it. D3D12 parameterises the size explicitly and it *moves with the
/// version* — 992/600/56 at `_0110`, 768/464/56 at `_0040` — so
/// `size_of::<T>()` is never the count. The line below is what makes that
/// checkable on this adapter instead of inherited from WARP's capture.
unsafe extern "C" fn fill_ddi_table(
    h_adapter: ddi12::D3D12DDI_HADAPTER,
    table_type: ddi12::D3D12DDI_TABLE_TYPE,
    table: *mut c_void,
    table_size: ddi12::SIZE_T,
    index: ddi12::UINT,
    h_rt_table: ddi12::D3D12DDI_HRTTABLE,
) -> ddi12::HRESULT {
    let _ = adapter_ok(h_adapter);

    UMD12_REFUSALS.fill_ddi_table_unimplemented.bump();
    let n = UMD12_REFUSALS.fill_ddi_table_unimplemented.get();
    if n <= LOG_BUDGET {
        log_error!(
            "FillDDITable: type={table_type} size={table_size} index={index} pTable={table:p} \
             hRTTable={:p} (x{n}) -> DXGI_ERROR_UNSUPPORTED (S6-0 owns the tables)",
            h_rt_table.handle,
        );
    }
    // ⛔ Not one byte is written to `table`. A partial fill would leave the rest
    // of the runtime's table uninitialised, which is strictly worse than
    // refusing: the runtime calls through an uninitialised slot instead of
    // failing device creation.
    DXGI_ERROR_UNSUPPORTED
}

/// `pfnCalcPrivateDeviceSize` — **0 at S5**, paired with a refusing
/// `pfnCreateDevice` in the same commit.
///
/// ⛔ The pairing is the point, not an omission. `DDI_REFERENCE.md` §1.4 warns
/// that the size and the construction must be *one function of `Flags`* —
/// `D3D12DDI_CREATE_DEVICE_FLAG_DEBUGGABLE` arrives at both sites and the
/// private size may legitimately differ between debug and retail. A non-zero
/// size here with a refusing create would be a block nothing writes, and it
/// would put the two sites out of step at exactly the moment S6-0 has to bring
/// them back in step. 0 with a refusing create is coherent: the runtime
/// allocates nothing and the create fails.
///
/// ⚠ In practice this is unreachable at S5 — `pfnGetCaps` refuses first and the
/// runtime abandons device creation there — so the counter reading non-zero is
/// itself information: it would mean the caps gauntlet was satisfied by
/// something, which at S5 nothing should be able to do.
///
/// S6-0 replaces this with `device12::device_private_size(flags)` and the create
/// body in the same commit.
unsafe extern "C" fn calc_private_device_size(
    h_adapter: ddi12::D3D12DDI_HADAPTER,
    arg: *const ddi12::D3D12DDIARG_CALCPRIVATEDEVICESIZE,
) -> ddi12::SIZE_T {
    let _ = adapter_ok(h_adapter);

    // ⚠ There is no HRESULT to refuse with — the DDI returns `SIZE_T`. The
    // counter is the only channel this slot has, which is why it exists.
    UMD12_REFUSALS.calc_private_device_size_unimplemented.bump();
    let n = UMD12_REFUSALS.calc_private_device_size_unimplemented.get();
    if n <= LOG_BUDGET {
        if arg.is_null() {
            log_error!("CalcPrivateDeviceSize: null arg (x{n}) -> 0");
        } else {
            // SAFETY: non-null per the branch; the DDI declares it `_In_ CONST`,
            // so the runtime guarantees a live, aligned struct for the call.
            let a = unsafe { &*arg };
            let negotiated = Ddi12Interface::from_pair(a.Interface, a.Version);
            if negotiated.is_none() {
                UMD12_REFUSALS.ddi12_version_mismatch.bump();
            }
            log_error!(
                "CalcPrivateDeviceSize: Interface={:#010x} Version={:#010x} ({}) Flags={:#x} \
                 (x{n}) -> 0 (S6-0 owns device12.rs)",
                a.Interface,
                a.Version,
                negotiated.map_or("UNADVERTISED", Ddi12Interface::name),
                a.Flags,
            );
        }
    }
    0
}

/// `pfnCreateDevice` — **refuses at S5**, with the version validated first.
///
/// S6-0 + L1 own the body: a device cannot be created until the caps gauntlet
/// is answered (`pfnGetCaps` above) and the three DDI tables are filled
/// (`pfnFillDDITable` above), and both of those are other stages' work.
///
/// The version check is not decoration and is not scaffolding: it decides
/// **which counter** ticks, and "did the runtime hand back the exact token we
/// advertised" is the single question D12's one-token set exists to make
/// answerable. `D12-G5` confirmed the `Interface`/`Version` split against WARP;
/// this is the same check against Helios.
unsafe extern "C" fn create_device(
    h_adapter: ddi12::D3D12DDI_HADAPTER,
    arg: *const ddi12::D3D12DDIARG_CREATEDEVICE_0109,
) -> ddi12::HRESULT {
    let _ = adapter_ok(h_adapter);

    if arg.is_null() {
        note_refusal(&UMD12_REFUSALS.create_device_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: non-null per the check above. The DDI declares it `_In_ CONST`, so
    // the runtime guarantees a live, aligned `D3D12DDIARG_CREATEDEVICE_0109` for
    // the duration of the call. ⛔ It is the `_0109` shape and not `_0003` only
    // because D12 advertises a single token — a `_0003`-generation negotiation
    // would make the two trailing fields (`pReserveRanges`, `NumReserveRanges`)
    // a read past the end of the runtime's struct.
    let a = unsafe { &*arg };

    match Ddi12Interface::from_pair(a.Interface, a.Version) {
        Some(negotiated) => {
            log_error!(
                "CreateDevice: {} hRTDevice={:p} pKTCallbacks={:p} Flags={:#x} \
                 NumReserveRanges={} -> DXGI_ERROR_UNSUPPORTED (S6-0 owns the device)",
                negotiated.name(),
                a.hRTDevice.handle,
                a.pKTCallbacks,
                a.Flags,
                a.NumReserveRanges,
            );
            note_refusal(&UMD12_REFUSALS.create_device_unimplemented);
        }
        None => {
            // ⛔ The `else`-as-default landmine, refused instead of guessed.
            // `ARCHITECTURE.md` §12 trap 2: treating an unknown interface as the
            // largest known one is what bulk-filled 150 slots into a 101-slot
            // table. Here it cannot happen, because there is no arm that fills
            // anything for an unrecognised pair.
            log_error!(
                "CreateDevice: UNADVERTISED Interface={:#010x} Version={:#010x} — refusing \
                 rather than assuming a table shape",
                a.Interface,
                a.Version,
            );
            UMD12_REFUSALS.ddi12_version_mismatch.bump();
            note_refusal(&UMD12_REFUSALS.create_device_unimplemented);
        }
    }
    DXGI_ERROR_UNSUPPORTED
}

/// `pfnDestroyDevice` — **counted**; no device can exist at S5.
///
/// ⚠ It lives on the **adapter** table (`d3d12umddi.h:13649`), not the device
/// table. That is a shape difference from D3D11 and a classic place to leave a
/// NULL (`DDI_REFERENCE.md` §1.3), which is why it is filled and counted rather
/// than omitted: a NULL here is a crash inside the runtime, and a silent noop
/// would hide the fact that a device the driver never made is being destroyed.
unsafe extern "C" fn destroy_device(h_device: ddi12::D3D12DDI_HDEVICE) {
    // `create_device` refuses unconditionally, so the runtime should never reach
    // here. Reaching it means either a device exists that this driver did not
    // build, or the runtime tears down after a failed create — both worth a line.
    UMD12_REFUSALS.destroy_device_unexpected.bump();
    let n = UMD12_REFUSALS.destroy_device_unexpected.get();
    if n <= LOG_BUDGET {
        log_error!(
            "DestroyDevice: hDrvDevice={:p} (x{n}) — no device was ever created; counted only",
            h_device.pDrvPrivate,
        );
    }
}

/// `pfnCloseAdapter` — **real**, and the set's readout point.
///
/// ⭐ This is where the refusal set becomes readable. `note_refusal` emits the
/// summary on a counter's *first* hit, but the two highest-volume S5 refusals
/// (`GetCaps12Unimplemented`, `FillDdiTable12Unimplemented`) use `bump()`
/// because they already log their own line (R911) — so without a readout here a
/// run in which only those fired would leave the set unprinted. T5's lesson,
/// restated: *an instrument nothing can read is not an instrument.*
unsafe extern "C" fn close_adapter(h_adapter: ddi12::D3D12DDI_HADAPTER) -> ddi12::HRESULT {
    let _ = adapter_ok(h_adapter);
    log_error!("CloseAdapter");
    log_refusal_summary();
    S_OK
}
