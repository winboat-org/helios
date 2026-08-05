//! `helios_umd12.dll` — the Helios D3D12 user-mode display driver.
//!
//! # Status: SKELETON + the engine bridge. `OpenAdapter12` refuses, and must keep refusing.
//!
//! This crate exists so the two-cdylib layout — build, mirror, sign, install,
//! and `UserModeDriverName[3]` — can be proven end to end **before** any DDI
//! code is written. As of S4 it links the vkd3d engine and can create a real
//! `ID3D12Device` through [`bridge12`], reached only by the `probe12` test
//! exports. It still implements no DDI and fills no table.
//!
//! ⛔ **The standing rule** (`DECISIONS.md` §7.1, `DX12.md` §3.2):
//!
//! > `OpenAdapter12` must stop refusing **in the same commit** that makes its
//! > body reachable — or the body must not be written yet.
//!
//! R908 (`e315d03`) is what that rule cost to learn: five hand-written
//! `D3d12Ddi*` ABI structs, eight `d3d12_*` handlers, a whole hand-transcribed
//! caps policy and `D3D12_SUPPORTED_DDI_VERSIONS` sat behind an unconditional
//! early return with `#[allow(unreachable_code)]` silencing the compiler's
//! proof that it was dead. ~230 lines that read as a live contract and could
//! never run. **Do not write a body here until the commit that reaches it.**
//!
//! ⚠ The S4 bridge is not a violation of that rule and the distinction matters:
//! `bridge12`/`probe12` are not an unreachable DDI body, they are a **reachable
//! instrument**. `tools/d3d12_bridge_probe.cpp`'s third arm calls the three
//! `helios_umd12_probe_*_v1` exports and draws `D12-G1`'s triangle through this
//! DLL. Code nothing can run is what R908 forbids; code only a probe runs is
//! evidence.
//!
//! # What comes next, in order (`ARCHITECTURE.md` §11)
//!
//! | stage | content |
//! |---|---|
//! | **S3** | `build.rs` + bindgen of `d3d12umddi.h` with `layout_tests(true)` + `ddi12.rs`. The layout assertions ARE the deliverable: if it compiles, the ABI is machine-checked. Still refusing, still not deployed. **DONE.** |
//! | **S4** | `vkd3d_bridge.{h,cpp}` + `bridge12.rs` — `helios_vkd3d_create_device` and the root-signature serializer, reached by a `tools/` probe. No runtime, no INF, no registry. **This stage.** |
//! | **S4b** | The ICD anchor (`helios_icd_anchor_v1`) — must land before the first two-engine run. |
//! | **S5** | INF + slot 3; `umd` **drops** its `OpenAdapter12` export and this one becomes reachable and stops refusing — **all in one commit** — with the `UmdD3D12` kill switch, default OFF. |
//! | **S6** | The DDI surface in `forward12/*`: caps first (H4), then device/queue/command-list, then descriptors, then present. |
//!
//! # Two things measured about this DDI that shape the crate (`D12-G5`)
//!
//! - The negotiated version is `D3D12DDI_SUPPORTED_0110`, but **`_0040` is
//!   accepted by this Windows build and a triangle presents on it** — 96 core +
//!   58 CL slots instead of 124 + 75. That choice belongs to P3.
//!   ⚠ `_0110` is not merely a bigger table: it is a behavioural contract with
//!   thirteen `VulkanOn12` obligations that carry no cap and cannot be declined
//!   (`SUBSTRATE.md` §4.5).
//! - **There is no DXGI table.** `D3D12DDI_TABLE_TYPE_DXGI` was never requested
//!   across 20 flip-model presents; present arrives on the command-list table.
//!   `ResourceHeaps.md` says why: *"the entire [DXGI] table is deprecated."*
//!
//! # Logging, knobs and refusals: all three mechanisms are the shared ones
//!
//! Stage **S2** is done — `log`, `knobs` and `refusals` live in
//! `helios_umd_common`, so the raw `OutputDebugStringA` primitive this crate
//! used as a placeholder is gone, and with it the hand-rolled `AtomicUsize`
//! refusal counter. What is shared is only the *mechanism*: this crate's log
//! basename (`umd12-<pid>.log`), its knob set ([`knobs12`]) and its refusal set
//! ([`UMD12_REFUSAL_SET`]) are its own, because both drivers can be loaded into
//! one process and an evidence channel that cannot say which DLL produced a line
//! is not an evidence channel.
//!
//! ⛔ `log::init`'s basename defaults to `"umd"`. Arriving late means this DLL's
//! lines land in **D3D11's** file and all that happens is `LOG_INIT_LATE` ticks
//! — which is why [`init_once`] runs at the *top* of every entry point, above
//! that entry point's first log line.

#![deny(deprecated)]

mod bridge12;
mod ddi12;
mod knobs12;
mod probe12;

// Mirrors `umd/src/lib.rs`: this is a Windows display driver and nothing in it
// is meaningful on another target. Failing at the top is clearer than failing
// deep inside a platform intrinsic.
#[cfg(not(windows))]
compile_error!(
    "helios_umd12 is a Windows-only WDDM user-mode display driver; it cannot be built for a \
     host target"
);

use core::ffi::c_void;

use helios_umd_common::hr::{Hresult, DXGI_ERROR_UNSUPPORTED};
use helios_umd_common::refusals::{self, RefusalCounter};

// `format`, `hr` and `log` live in the shared `umd_common` crate (S1/S2,
// `DECISIONS.md` D3b). Re-exported at this crate's root under their original
// names, exactly as `umd/src/lib.rs:47` does it, so every future
// `crate::log::…` / `crate::log_error!` path in `forward12/*` resolves the same
// way in both drivers and a file can be read without first working out which
// crate it belongs to.
pub(crate) use helios_umd_common::{log, log_error};
pub(crate) use log::log_self_module_path;
// ⚠ `trace_line!` is deliberately NOT re-exported yet. S6's per-op DDI handlers
// are its only consumers, and importing it now would need `#[allow(unused_imports)]`
// on a hand-written line — which `PARALLEL.md` §10's merge checklist forbids
// outright ("generated code may be allowed, hand-written code may not — R908").
// The first per-op site adds the one-line re-export beside its own first use;
// R420's two-name choice (`log_error!` for errors/one-shots/refusals,
// `trace_line!` for per-op repeat traffic) is enforced by `#![deny(deprecated)]`
// on `log_line`, not by which names happen to be in scope.

/// This driver's refusal counters.
///
/// CLAUDE.md rule 2: *every skipped/refused path gets a named counter — loud
/// failure over fake success.*
///
/// ⛔ A separate set from `umd`'s eleven, and that is D3b's instruction: a
/// shared list would make `DDI refusals:` a line about two drivers, while
/// `CONFORMANCE.md`'s charter — *"drive the UMD's `DDI refusals:` counters to
/// zero"* — reads it per driver. The prefix below is `D3D12 DDI refusals:` so
/// the two lines are greppable apart even when both DLLs are in one process.
pub(crate) struct Umd12Refusals {
    /// How many times the runtime asked this driver for a D3D12 adapter and was
    /// refused. Expected to be non-zero for the whole of P2: refusing is the
    /// correct answer until S5, and this counter is what makes "it refused"
    /// visible instead of merely true.
    pub(crate) open_adapter12: RefusalCounter,
    /// A `helios_umd12_probe_*_v1` export called with a null out-param or a
    /// null descriptor. Expected 0 — the only caller is
    /// `tools/d3d12_bridge_probe.cpp`, so a hit means the probe's third arm is
    /// miscompiled or mis-resolved, which is worth an immediate summary line
    /// rather than an `E_INVALIDARG` the probe prints as one of many.
    pub(crate) probe12_bad_arg: RefusalCounter,
    /// `BridgeDevice12::create` returned `None`: no adapter with that LUID, or
    /// the engine refused. The C++ side has already logged the engine's
    /// HRESULT to `umd12-<pid>.log`; this is the countable half.
    pub(crate) probe12_create_failed: RefusalCounter,
    /// The bridge was created but carries no `ID3D12Device`. Expected 0 by
    /// construction — lane A's `helios_vkd3d_bridge_create_device` returns a
    /// null `unique_ptr` rather than an empty one on failure — so a hit means
    /// that cross-FFI contract has been broken.
    pub(crate) probe12_no_device: RefusalCounter,
    /// The root-signature serializer produced an error blob that had nowhere to
    /// go, because the caller passed a null `err_out` (which is legal — it is
    /// the shape of `D3D12SerializeRootSignature` itself). The blob is released
    /// rather than leaked; this counts the fact that the only text explaining a
    /// failed serialize was discarded. Expected 0: the probe passes both outs.
    pub(crate) probe12_err_blob_dropped: RefusalCounter,
}

pub(crate) static UMD12_REFUSALS: Umd12Refusals = Umd12Refusals {
    open_adapter12: RefusalCounter::new("OpenAdapter12"),
    probe12_bad_arg: RefusalCounter::new("Probe12BadArg"),
    probe12_create_failed: RefusalCounter::new("Probe12CreateFailed"),
    probe12_no_device: RefusalCounter::new("Probe12NoDevice"),
    probe12_err_blob_dropped: RefusalCounter::new("Probe12ErrBlobDropped"),
};

/// The set, in the order the summary prints them. ⛔ This order is the evidence
/// contract: `D3D12 DDI refusals:` lines from different builds get diffed.
static UMD12_REFUSAL_SET: [&RefusalCounter; 5] = [
    &UMD12_REFUSALS.open_adapter12,
    &UMD12_REFUSALS.probe12_bad_arg,
    &UMD12_REFUSALS.probe12_create_failed,
    &UMD12_REFUSALS.probe12_no_device,
    &UMD12_REFUSALS.probe12_err_blob_dropped,
];

/// Bump one refusal counter and emit the whole set's summary on its FIRST hit.
///
/// Taking `&RefusalCounter` rather than a field name keeps the call sites one
/// line and makes "increment without a readout" impossible to write by accident
/// — T5's lesson, restated in `umd_common::refusals`: *an instrument nothing
/// can read is not an instrument.* `note()` is `#[must_use]`, so discarding the
/// first-hit signal does not compile.
pub(crate) fn note_refusal(counter: &RefusalCounter) {
    if counter.note() {
        log_error!("{}", refusals::summary("D3D12 DDI refusals:", &UMD12_REFUSAL_SET));
    }
}

/// Name this DLL's log file, resolve its trace gate, and dump the module path
/// and knob inventory — once per process.
///
/// ⛔ **Call this at the TOP of every entry point, above that entry point's
/// first log line.** `log::init`'s basename defaults to `"umd"` and the log
/// PATH is a `OnceLock` latched by the first line of any kind, so arriving late
/// puts this driver's evidence in `umd-<pid>.log` — D3D11's file — permanently,
/// and the only signal is `helios_umd_common::log::LOG_INIT_LATE` ticking. With
/// two Helios UMDs able to load into one process there is no other way to tell
/// the two line streams apart.
///
/// Idempotent: `log::init` is cheap and re-entrant, and both
/// `log_self_module_path` and `log_knob_inventory` self-gate on a `swap`ped
/// `AtomicBool` inside `umd_common`.
///
/// ⚠ `log_self_module_path`'s anchor is its own address inside `umd_common`,
/// but the crate is an rlib linked *into* each cdylib, so this call reports
/// **this** DLL's file — that is the property `tools/capture-knob-inventory.ps1`
/// keys on.
pub(crate) fn init_once() {
    log::init("umd12", knobs12::umd12_trace());
    log_self_module_path();
    knobs12::log_knob_inventory();
}

/// The DLL entry point, present for exactly one reason: to release this
/// module's process-lifetime handles when it is unloaded.
///
/// The shape is copied from `umd/src/lib.rs:69-80` — deliberately the *shape*
/// and not shared code, because it is the loader-lock contract that is common,
/// not a body worth abstracting (a `DllMain` that lives in another crate is a
/// `DllMain` you cannot read at the site the linker resolves it).
///
/// `helios_umd.dll` is loaded and unloaded ONCE PER D3D11 DEVICE — measured,
/// not assumed (`tools/helios_handle_types.cpp`). Rust `static`s are never
/// dropped and the loader closes nothing a module opened, so every such unload
/// stranded the log-file handle: one leaked kernel handle per device, linear,
/// no plateau. See [`log::close_at_detach`].
/// ⚠ Whether *this* DLL is also unloaded once per device is UNVERIFIED and is
/// scheduled for S5 (it is `umd_common::log::close_at_detach`'s own
/// UNVERIFIED-5). Wiring the release now costs nothing and means a second UMD
/// cannot double the leak while that question is open.
///
/// Rules this body obeys, all of them loader-lock rules:
/// * no allocation, no I/O, no `LoadLibrary`, no thread waits;
/// * no panic — a panic through `extern "system"` in DllMain aborts the
///   process, and this crate is `panic = "abort"` anyway;
/// * nothing at all on the `lpv_reserved != NULL` (process-exit) path, where
///   the kernel reclaims every handle and other threads are already dead.
///
/// The MSVC CRT's `_DllMainCRTStartup` calls this by name; returning non-zero
/// is "succeeded" and is the only legal answer for a DETACH notification.
#[unsafe(no_mangle)]
pub extern "system" fn DllMain(
    _instance: *mut c_void,
    reason: u32,
    lpv_reserved: *mut c_void,
) -> i32 {
    const DLL_PROCESS_DETACH: u32 = 0;
    if reason == DLL_PROCESS_DETACH && lpv_reserved.is_null() {
        log::close_at_detach();
    }
    1
}

/// The D3D12 adapter entry point — **exported and refusing, on purpose.**
///
/// The loader resolves this by name, so a missing export is a different and
/// worse failure than a clean refusal.
///
/// `open_data` is `*mut c_void` because nothing here reads it. Giving it a real
/// type would mean either hand-transcribing `D3D12DDIARG_OPENADAPTER` — banned
/// by `DECISIONS.md` §7.2, which records a 376..392-byte out-of-bounds write
/// into the runtime's heap from exactly that mistake — or reaching into the
/// bindgen'd `ddi12` types, which would be the first line of the body the
/// standing rule forbids until S5.
///
/// ⚠ Until S5, `helios_umd.dll` also exports a refusing `OpenAdapter12` and is
/// the one the INF registers. Both refusing simultaneously is the intended
/// state; S5 deletes `umd`'s export and registers this DLL at slot 3 in the
/// same commit that makes this body reachable.
///
/// # Safety
/// `open_data` is the runtime's `D3D12DDIARG_OPENADAPTER*`. **This body
/// dereferences nothing**, so today there is no precondition it can violate —
/// the `unsafe` is on the signature because the D3D runtime loader resolves and
/// calls it by name across an FFI boundary, not because the body reads memory.
///
/// ⛔ That changes at S5. The commit that makes this body reachable must
/// restate this section as the real contract: `open_data` points at a
/// `D3D12DDIARG_OPENADAPTER` whose `Interface`/`Version` fields select the
/// table size, and writing a table sized for a version the runtime did not ask
/// for is `ARCHITECTURE.md` §12 trap 2 — a 376..392-byte out-of-bounds write
/// into the runtime's heap.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn OpenAdapter12(open_data: *mut c_void) -> Hresult {
    init_once();
    let _ = open_data;
    // ⚠ ONE log line for one event, and it is the set summary, not a bespoke
    // "OpenAdapter12 refused" line beside it. R911 is explicit that an already-
    // loud arm must not also emit the summary; the inverse holds too — a second
    // line saying what `OpenAdapter12=1` already says makes the count and the
    // prose two things that can disagree. The refusal's HRESULT is not in the
    // line because it is a constant of this function, stated below.
    note_refusal(&UMD12_REFUSALS.open_adapter12);
    // ⛔ Declining an unimplemented DDI is DXGI_ERROR_UNSUPPORTED (0x887A_0004),
    // NEVER DXGI_ERROR_DRIVER_INTERNAL_ERROR (0x887A_0020) — the latter is
    // recorded by the runtime and by ETW as a *driver fault*, so a client's
    // ordinary "this driver has no D3D12 DDI" negotiation would be logged as a
    // Helios bug. `umd`'s copy of this export returned the wrong one until R801
    // because the two shared a constant name and both printed identically in
    // our own logs. `helios_umd_common::hr` is why that can no longer recur:
    // one numeric literal per HRESULT, in one file, with build-time asserts.
    DXGI_ERROR_UNSUPPORTED
}
