//! `helios_umd12.dll` — the Helios D3D12 user-mode display driver.
//!
//! # Status: S5. `OpenAdapter12` is REACHABLE, behind the `UmdD3D12` kill switch.
//!
//! This crate exists so the two-cdylib layout — build, mirror, sign, install,
//! and `UserModeDriverName[3]` — can be proven end to end **before** the DDI
//! surface is written. As of S4 it links the vkd3d engine and can create a real
//! `ID3D12Device` through [`bridge12`]. As of **S5** it is registered at
//! `UserModeDriverName[3]`, `umd`'s duplicate `OpenAdapter12` export is gone,
//! and [`adapter12::OpenAdapter12`] fills all eight adapter slots when
//! `HKLM\SOFTWARE\Helios!UmdD3D12` is non-zero. It still fills **no DDI table**:
//! `pfnGetCaps` and `pfnFillDDITable` refuse with named counters until L1 and
//! S6-0.
//!
//! ⛔ **The standing rule this crate was shaped by** (`DECISIONS.md` §7.1,
//! `DX12.md` §3.2):
//!
//! > `OpenAdapter12` must stop refusing **in the same commit** that makes its
//! > body reachable — or the body must not be written yet.
//!
//! R908 (`e315d03`) is what that rule cost to learn: five hand-written
//! `D3d12Ddi*` ABI structs, eight `d3d12_*` handlers, a whole hand-transcribed
//! caps policy and `D3D12_SUPPORTED_DDI_VERSIONS` sat behind an unconditional
//! early return with `#[allow(unreachable_code)]` silencing the compiler's
//! proof that it was dead. ~230 lines that read as a live contract and could
//! never run.
//!
//! ⚠ S5 satisfies it in the only way the rule allows: the INF/registry change
//! that puts this DLL in slot 3, the `UmdD3D12` knob, the deletion of `umd`'s
//! export and [`adapter12`]'s eight slots are **one commit**. Everything in
//! `adapter12` is reached by the runtime on a knob-ON adapter open; the parts
//! that are not implemented **refuse with a named counter** rather than sitting
//! behind a dead branch.
//!
//! ⚠ The S4 bridge was not a violation either, and the distinction matters:
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
//! | **S3** | `build.rs` + bindgen of `d3d12umddi.h` with `layout_tests(true)` + `ddi12.rs`. The layout assertions ARE the deliverable: if it compiles, the ABI is machine-checked. **DONE.** |
//! | **S4** | `vkd3d_bridge.{h,cpp}` + `bridge12.rs` — `helios_vkd3d_create_device` and the root-signature serializer, reached by a `tools/` probe. **DONE.** |
//! | **S4b** | The ICD anchor (`helios_icd_anchor_v1`) — one venus ICD module per process. **DONE.** |
//! | **S5** | INF + slot 3; `umd` **drops** its `OpenAdapter12` export and this one becomes reachable — **all in one commit** — with the `UmdD3D12` kill switch, default OFF. **This stage.** |
//! | **S6-0** | All 214 device/command-list/queue slots stubbed with counting noops, plus one `install_<lane>()` per lane, so every lane is *substitutive* rather than *additive* (`PARALLEL.md` §3). |
//! | **S6** | The DDI surface in `forward12/*` across 11 lanes: caps first (H4), then queue, PSO, descriptors, resources, recording, present. |
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

mod adapter12;
mod bridge12;
mod caps12;
mod ddi12;
mod forward12;
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
    /// refused **because the `UmdD3D12` kill switch is absent or zero** (D11).
    ///
    /// ⚠ Its meaning changed at S5 and its name deliberately did not. Before S5
    /// it counted "there is no D3D12 DDI"; now it counts "the D3D12 DDI is
    /// switched off". Both are the same observable fact for the client —
    /// `OpenAdapter12` returned `DXGI_ERROR_UNSUPPORTED` — and keeping the name
    /// keeps `D3D12 DDI refusals:` lines diffable across the S5 boundary.
    /// **Expected non-zero on every ordinary boot**: dwm calls `OpenAdapter12`
    /// in production and the knob defaults OFF, so this is the counter that
    /// proves the kill switch is doing its job.
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

    // ── S5: the adapter surface (`adapter12`). Appended at the END, because the
    // set's order is the evidence contract and `D3D12 DDI refusals:` lines get
    // diffed across builds. ─────────────────────────────────────────────────
    /// `OpenAdapter12` was called with a null `D3D12DDIARG_OPENADAPTER*`, or one
    /// whose `pAdapterFuncs` is null. Expected 0 — the runtime always supplies
    /// both. ⚠ Only reachable with the kill switch ON: the knob-off arm returns
    /// before examining anything, so "D3D12 is off" and "the runtime handed us a
    /// bad pointer" never share a counter.
    pub(crate) open_adapter12_bad_arg: RefusalCounter,
    /// A DDI arrived with an adapter handle whose `pDrvPrivate` is not our
    /// token. **Counted only, never refused on** — the D3D11 side
    /// (`umd/src/adapter.rs:132-149`) takes the same position, and for the same
    /// reason: this has to be observed at zero on a real boot before any DDI
    /// starts rejecting on it.
    pub(crate) adapter_unrecognised: RefusalCounter,
    /// `pfnGetSupportedVersions` with a null `puEntries`. Expected 0; the DDI
    /// declares it `_Inout_` and it is the one parameter that is never optional.
    pub(crate) get_supported_versions_bad_arg: RefusalCounter,
    /// `pfnGetOptionalDDITables` with a null `puEntries`. Expected 0, same
    /// reason.
    pub(crate) get_optional_ddi_tables_bad_arg: RefusalCounter,
    /// `pfnGetCaps` refused because L1 (`caps12.rs`) has not landed. **Expected
    /// non-zero and roughly 43 per knob-ON adapter open** until then — this is
    /// the counter `CONFORMANCE.md`'s charter reads, and it is also what stops
    /// S5 from creating a device: the runtime abandons device creation at the
    /// caps gauntlet.
    pub(crate) get_caps_unimplemented: RefusalCounter,
    /// `pfnFillDDITable` with a null table pointer, or a byte count too small to
    /// hold even one slot. Expected 0.
    pub(crate) fill_ddi_table_bad_arg: RefusalCounter,
    /// `pfnFillDDITable` for one of the 22 `D3D12DDI_TABLE_TYPE` values this
    /// driver does not serve. Expected 0 — the runtime asks only for the three
    /// it negotiated — and **nothing is written** on that path, which is the
    /// property `DECISIONS.md` §7.4 actually demands.
    pub(crate) fill_ddi_table_unknown_type: RefusalCounter,
    /// The runtime's table was **smaller** than the struct this build's
    /// `d3d12umddi.h` describes, so the fill was bounded to the runtime's count.
    ///
    /// ⚠ This is the R702 direction and the reason the byte count comes from the
    /// argument: 24H2 passed 576 bytes for a 592-byte `DRIVERCAPS` and the D3D11
    /// driver wrote past it. Expected 0 at `_0110` — 992/600/56 is what `D12-G5`
    /// measured — so non-zero means the negotiated revision is not the one
    /// `ddi12` was generated from.
    pub(crate) fill_ddi_table_truncated: RefusalCounter,
    /// The runtime's table was **larger** than this build's struct. The tail is
    /// served by counted stubs rather than left NULL, but those slots can never
    /// do anything: refresh the bindings.
    pub(crate) fill_ddi_table_oversized: RefusalCounter,
    /// `pfnFillDDITable` asked for a command-list table index beyond the two
    /// `D12-G5` measured, so its `D3D12DDI_HRTTABLE` was not stashed. Expected 0.
    /// ⚠ A hit here is not cosmetic: that handle is the only way to obtain what
    /// `pfnSetCommandListDDITableCb` later needs, and it cannot be recovered.
    pub(crate) command_list_table_index_unbounded: RefusalCounter,
    /// `pfnCalcPrivateDeviceSize` answered 0 because S6-0 owns `device12.rs`.
    /// Expected 0 at S5, for the same reason as the row above.
    pub(crate) calc_private_device_size_unimplemented: RefusalCounter,
    /// `pfnCreateDevice` with a null `D3D12DDIARG_CREATEDEVICE_0109*`.
    /// Expected 0.
    pub(crate) create_device_bad_arg: RefusalCounter,
    /// `pfnCreateDevice` refused because S6-0 owns the device. Expected 0 at S5
    /// (caps refuses first).
    pub(crate) create_device_unimplemented: RefusalCounter,
    /// The runtime handed back an `(Interface, Version)` pair that is **not** the
    /// single token `pfnGetSupportedVersions` advertised (D12). Expected 0, and
    /// a non-zero reading is a real finding: it would mean the one-token set is
    /// not doing what `DECISIONS.md` D12 says it does, and that a second table
    /// shape is reachable.
    pub(crate) ddi12_version_mismatch: RefusalCounter,
    /// `pfnDestroyDevice` on a device this driver never created. Expected 0 —
    /// `pfnCreateDevice` refuses unconditionally at S5.
    pub(crate) destroy_device_unexpected: RefusalCounter,
}

pub(crate) static UMD12_REFUSALS: Umd12Refusals = Umd12Refusals {
    open_adapter12: RefusalCounter::new("OpenAdapter12"),
    probe12_bad_arg: RefusalCounter::new("Probe12BadArg"),
    probe12_create_failed: RefusalCounter::new("Probe12CreateFailed"),
    probe12_no_device: RefusalCounter::new("Probe12NoDevice"),
    probe12_err_blob_dropped: RefusalCounter::new("Probe12ErrBlobDropped"),
    open_adapter12_bad_arg: RefusalCounter::new("OpenAdapter12BadArg"),
    adapter_unrecognised: RefusalCounter::new("AdapterUnrecognised"),
    get_supported_versions_bad_arg: RefusalCounter::new("GetSupportedVersionsBadArg"),
    get_optional_ddi_tables_bad_arg: RefusalCounter::new("GetOptionalDDITablesBadArg"),
    get_caps_unimplemented: RefusalCounter::new("GetCapsUnimplemented"),
    fill_ddi_table_bad_arg: RefusalCounter::new("FillDDITableBadArg"),
    fill_ddi_table_unknown_type: RefusalCounter::new("FillDDITableUnknownType"),
    fill_ddi_table_truncated: RefusalCounter::new("FillDDITableTruncated"),
    fill_ddi_table_oversized: RefusalCounter::new("FillDDITableOversized"),
    command_list_table_index_unbounded: RefusalCounter::new("CommandListTableIndexUnbounded"),
    calc_private_device_size_unimplemented: RefusalCounter::new("CalcPrivateDeviceSizeUnimplemented"),
    create_device_bad_arg: RefusalCounter::new("CreateDeviceBadArg"),
    create_device_unimplemented: RefusalCounter::new("CreateDeviceUnimplemented"),
    ddi12_version_mismatch: RefusalCounter::new("Ddi12VersionMismatch"),
    destroy_device_unexpected: RefusalCounter::new("DestroyDeviceUnexpected"),
};

/// The set, in the order the summary prints them. ⛔ This order is the evidence
/// contract: `D3D12 DDI refusals:` lines from different builds get diffed, so
/// new counters are **appended**, never inserted.
static UMD12_REFUSAL_SET: [&RefusalCounter; 20] = [
    &UMD12_REFUSALS.open_adapter12,
    &UMD12_REFUSALS.probe12_bad_arg,
    &UMD12_REFUSALS.probe12_create_failed,
    &UMD12_REFUSALS.probe12_no_device,
    &UMD12_REFUSALS.probe12_err_blob_dropped,
    &UMD12_REFUSALS.open_adapter12_bad_arg,
    &UMD12_REFUSALS.adapter_unrecognised,
    &UMD12_REFUSALS.get_supported_versions_bad_arg,
    &UMD12_REFUSALS.get_optional_ddi_tables_bad_arg,
    &UMD12_REFUSALS.get_caps_unimplemented,
    &UMD12_REFUSALS.fill_ddi_table_bad_arg,
    &UMD12_REFUSALS.fill_ddi_table_unknown_type,
    &UMD12_REFUSALS.fill_ddi_table_truncated,
    &UMD12_REFUSALS.fill_ddi_table_oversized,
    &UMD12_REFUSALS.command_list_table_index_unbounded,
    &UMD12_REFUSALS.calc_private_device_size_unimplemented,
    &UMD12_REFUSALS.create_device_bad_arg,
    &UMD12_REFUSALS.create_device_unimplemented,
    &UMD12_REFUSALS.ddi12_version_mismatch,
    &UMD12_REFUSALS.destroy_device_unexpected,
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

/// Emit the whole refusal set unconditionally.
///
/// ⭐ [`note_refusal`] emits the summary on a counter's **first** hit, which is
/// enough for the one-shot refusals. It is **not** enough for the arms that use
/// `RefusalCounter::bump` because they already log their own line (R911: an
/// already-loud arm must not also emit the summary) — at S5 that is
/// `GetCapsUnimplemented` and `FillDDITableUnimplemented`, i.e. the two that
/// fire on every adapter open. A run in which only those fired would leave the
/// set unprinted, and T5's lesson is exactly that: *an instrument nothing can
/// read is not an instrument.*
///
/// Called from `adapter12::close_adapter`, which is this driver's only natural
/// teardown point — D3D12 answers everything at adapter scope, so there is no
/// per-device destroy to hang it on until S6-0.
pub(crate) fn log_refusal_summary() {
    log_error!("{}", refusals::summary("D3D12 DDI refusals:", &UMD12_REFUSAL_SET));
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

