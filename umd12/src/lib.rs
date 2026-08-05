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
mod device12;
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
pub(crate) use helios_umd_common::{log, log_error, trace_line};
pub(crate) use log::log_self_module_path;
// ⭐ **`trace_line!` arrives here with its first per-op consumer**, exactly as
// this comment previously said it would: `caps12::check_multisample_quality_levels`
// runs **2 730 times inside one `D3D12CreateDevice`** and is the first DDI in
// this crate with per-op repeat traffic. R420's two-name split is what makes
// that safe — `log_error!` for errors, one-shots and refusals; `trace_line!` for
// traffic that is gated off unless `HKLM\SOFTWARE\Helios!Umd12Trace` is set, so
// the shipping default pays a `bool` load per call and nothing else.
//
// ⚠ It earned its way in rather than being added on spec: two `D12-G7` runs were
// spent inferring which (format, sample count) the runtime rejected from call
// COUNTS, because the bounded `log_error!` budget could not cover a 2 730-call
// sweep and an unbounded one would flood every device creation. A trace-gated
// per-call line is the only shape that answers the question without paying for
// it on every boot.

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
    /// `pfnFillDDITable` with a null table pointer, or a byte count too small to
    /// hold even one slot. Expected 0.
    pub(crate) fill_ddi_table_bad_arg: RefusalCounter,
    /// `pfnFillDDITable` for a `D3D12DDI_TABLE_TYPE` this driver has no typed
    /// handler for. The table is filled with counting stubs at the **runtime's**
    /// byte count, so no slot is NULL and no shape is selected; nothing in it is
    /// implemented.
    ///
    /// ⚠ **Expected non-zero, and 1 per device is the measured normal**:
    /// `D12-G7` showed the runtime asking for
    /// `D3D12DDI_TABLE_TYPE_0096_EXTENDED_FEATURES` (27, 32 B) on a baseline
    /// device, and refusing it **loses the device**. A count above 1 per device
    /// means a table nobody has looked at yet.
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
    /// `pfnCalcPrivateDeviceSize` with a null arg. Expected 0. ⚠ There is no
    /// HRESULT to refuse with — the DDI returns `SIZE_T` — so this counter is
    /// the slot's only channel.
    pub(crate) calc_private_device_size_bad_arg: RefusalCounter,
    /// `pfnCreateDevice` with a null arg, a null `hDrvDevice`, a null
    /// `pKTCallbacks`, or a null `p12UMCallbacks`. Expected 0 — all four are
    /// validated **before** anything is constructed, which is the ordering
    /// `umd/src/adapter.rs`'s `DeviceUnderConstruction` docstring exists to
    /// record: the two checks that used to run after construction leaked a
    /// Vulkan device, a kernel context and a paging queue *per attempt*.
    pub(crate) create_device_bad_arg: RefusalCounter,
    /// The vkd3d engine refused to create a device. The C++ side has already
    /// logged the engine's HRESULT to `umd12-<pid>.log`; this is the countable
    /// half.
    pub(crate) create_device_engine_failed: RefusalCounter,
    /// `pfnCreateDevice` was handed a non-empty `pReserveRanges` and **ignored
    /// it**. The GPU-virtual-address ranges the runtime asks to be reserved at
    /// device creation are L4's (resources, heaps, GPU VA); counting the request
    /// makes "we ignored it" a number rather than a silence.
    pub(crate) reserve_ranges_ignored: RefusalCounter,
    /// `pfnDestroyDevice` on a null `hDrvDevice`. Expected 0.
    pub(crate) destroy_device_bad_arg: RefusalCounter,
    // ── L1: the caps gauntlet (`caps12`). ──────────────────────────────────
    /// `pfnGetCaps` with a null arg, a null `pData`, or a null `pInfo` on a cap
    /// that requires one. Expected 0.
    pub(crate) caps_bad_arg: RefusalCounter,
    /// The runtime's `pData` buffer was smaller than the struct this build's
    /// header describes for that cap, so **nothing was written**. Expected 0 —
    /// and a hit is the R702 class arriving through `pfnGetCaps` rather than
    /// `pfnFillDDITable`.
    pub(crate) caps_data_size_too_small: RefusalCounter,
    /// A caps type this driver does not individually answer, served by the
    /// `DDI_REFERENCE.md` §11.2 safe default (zero `pData` up to `DataSize`,
    /// return `S_OK`). ⚠ **Expected non-zero and that is fine** — it is the
    /// documented answer for the ~30 caps outside the must-answer set. It reads
    /// as a work list for later lanes, not as a fault.
    pub(crate) caps_defaulted: RefusalCounter,
    /// How many `pfnGetCaps` calls this adapter has seen. ⚠ Not a refusal — it
    /// bounds the per-call evidence line, and "how many caps did this runtime
    /// ask, of which types" is what `D12-G5` needed a WARP spy proxy to learn.
    pub(crate) caps_calls: RefusalCounter,
    /// `TiledResourcesTier` was clamped to what `D3D12DDI_TILED_RESOURCES_TIER`
    /// can express. Expected 0 while the tier is reported `NOT_SUPPORTED`; the
    /// clamp exists now so the lane that raises it cannot forget it (vkd3d
    /// reports **4**, the enum stops at 3, and the runtime clamps silently).
    pub(crate) caps_tiled_tier_clamped: RefusalCounter,
    /// `D3D12DDICAPS_TYPE_SHADER` was answered with a `TotalLaneCount` that is
    /// **vkd3d's fallback, not a measurement**. ⚠ Expected non-zero, and that is
    /// the point: `DDI_REFERENCE.md` §11.7 records that `32 * subgroupSize`
    /// under-reports this GPU by roughly 24x, and venus exposes neither
    /// `VK_AMD_shader_core_properties` nor `VK_NV_shader_sm_builtins` to do
    /// better. The counter is what stops the number being mistaken for a fact.
    pub(crate) caps_total_lane_count_guess: RefusalCounter,
    /// The runtime's shader-model array was shorter than this driver's list, so
    /// the list was truncated. Expected 0.
    pub(crate) caps_shader_models_truncated: RefusalCounter,
    /// `D3D12DDICAPS_TYPE_TEXTURE_LAYOUT_SETS` was asked for a layout/functional
    /// unit outside the one set this driver advertises. ⚠ **Expected non-zero:
    /// that is how the enumeration ENDS** — the runtime drives it until the
    /// driver fails, and WARP's measured contract is `S_OK` once then two
    /// failures. It counts how many sets were advertised, not a fault.
    pub(crate) caps_texture_layout_set_end: RefusalCounter,

    /// The runtime handed back an `(Interface, Version)` pair that is **not** the
    /// single token `pfnGetSupportedVersions` advertised (D12). Expected 0, and
    /// a non-zero reading is a real finding: it would mean the one-token set is
    /// not doing what `DECISIONS.md` D12 says it does, and that a second table
    /// shape is reachable.
    pub(crate) ddi12_version_mismatch: RefusalCounter,
    /// `pfnDestroyDevice` on a device this driver never created. Expected 0 —
    /// `pfnCreateDevice` refuses unconditionally at S5.
    pub(crate) destroy_device_unexpected: RefusalCounter,

    // ── L1, second half: the three device-core format/MSAA slots. Appended at
    // the END, for the same reason as the S5 block above. ──────────────────
    /// How many `pfnCheckFormatSupport` calls this process has served. ⚠ Not a
    /// refusal — it is the number the noop hit counters used to carry and which
    /// implementing the slot would otherwise delete. `D12-G7` measured **93**
    /// inside one `D3D12CreateDevice` (`DDI_REFERENCE.md` §11.1 predicted a
    /// 91-format sweep), so a reading near 93 per device is the expected shape.
    pub(crate) caps_format_support_calls: RefusalCounter,
    /// How many `pfnCheckMultisampleQualityLevels` calls this process has
    /// served. ⚠ Not a refusal, same reason: `D12-G7` measured **2 730** inside
    /// one `D3D12CreateDevice` — 91 formats x 30 sample counts.
    pub(crate) caps_msaa_calls: RefusalCounter,
    /// One of the three device-core slots was called with a null out-pointer.
    /// Expected 0 — all three declare their outputs `_Out_`, never optional.
    pub(crate) caps_slot_bad_arg: RefusalCounter,
    /// One of the three device-core slots could not reach the engine: the
    /// `hDevice` did not resolve, or the bridge carries no `ID3D12Device`. The
    /// slot answers "nothing supported" rather than reading uninitialised
    /// runtime memory. Expected 0 — these are device-scope DDIs and a device
    /// exists by construction.
    pub(crate) caps_slot_no_device: RefusalCounter,
    /// `ID3D12Device::CheckFeatureSupport(D3D12_FEATURE_FORMAT_SUPPORT)`
    /// returned a failure HRESULT, so the format is answered as unsupported.
    ///
    /// ⚠ **Expected non-zero, and that is not a fault.** vkd3d refuses a
    /// `DXGI_FORMAT` it has no `vkd3d_get_format` table entry for with
    /// **`E_FAIL`** (`libs/vkd3d/device.c:5241-5245`), and reserves
    /// `E_INVALIDARG` for a value outside the format enum ranges
    /// (`device.c:5225-5229`). The runtime's own device-creation sweep walks
    /// formats this engine does not implement — legacy XR, video and
    /// planar/subsampled families among them. The count is what stops "the
    /// engine said no" being mistaken for "the driver forgot to answer".
    pub(crate) caps_format_support_engine_failed: RefusalCounter,
    /// A format was answered with the explicit `D3D12DDI_FORMAT_SUPPORT_NOT_SUPPORTED`
    /// sentinel rather than a bare 0.
    ///
    /// ⛔ **Expected exactly 1 per format sweep, and it is the instrument for
    /// the one trap that has already cost this project a device.**
    /// `DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM` (89) must be refused with
    /// `0x80000000` set alone; the D3D11 runtime rejected a bare 0 there as a
    /// malformed caps response and failed `D3D11CreateDevice` with
    /// `DXGI_ERROR_DRIVER_INTERNAL_ERROR` (`0x887A0020`) — the same HRESULT
    /// `D12-G7` was failing with. A **zero** reading on a run that answered
    /// caps is the finding, not a non-zero one.
    pub(crate) caps_format_not_supported_sentinel: RefusalCounter,
    /// `ID3D12Device::CheckFeatureSupport(D3D12_FEATURE_MULTISAMPLE_QUALITY_LEVELS)`
    /// returned a failure HRESULT, so the query is answered with zero quality
    /// levels. ⚠ Expected non-zero for the same reason as
    /// `CapsFormatSupportEngineFailed`.
    pub(crate) caps_msaa_engine_failed: RefusalCounter,
    /// A multisample query carrying `D3D12DDI_MULTISAMPLE_QUALITY_LEVEL_FLAG_TILED_RESOURCE`
    /// was answered with zero quality levels because this driver reports
    /// `TiledResourcesTier = NOT_SUPPORTED`.
    ///
    /// ⚠ Expected non-zero if the runtime sweeps the flag. It is a **coupling**,
    /// not a fault: the lane that raises the tiled tier removes this gate in the
    /// same commit, and until then answering the engine's tier-4 truth here
    /// would contradict the tier this driver reports two functions above.
    pub(crate) caps_msaa_tiled_refused: RefusalCounter,
    /// `pfnGetMipPacking` was called and answered "no packed mips, no tiles",
    /// because this driver reports `TiledResourcesTier = NOT_SUPPORTED` and so
    /// no tiled resource can exist for it to describe.
    ///
    /// ⛔ Expected 0. A hit means the runtime reached a tiled-resource path on a
    /// driver that reports no tier, which is a caps inconsistency somewhere
    /// else — not something this slot can fix.
    pub(crate) caps_mip_packing_refused: RefusalCounter,
    /// A format whose engine answer carried `MULTISAMPLE_RENDERTARGET` or
    /// `MULTISAMPLE_LOAD` had both bits **dropped**, because the same engine
    /// offers no quality level at any sample count above 1.
    ///
    /// ⛔ **This is the counter for the rule that failed `D12-G7` on 2026-08-06**,
    /// and it is the one number that says whether the fix is doing anything. The
    /// runtime rejects the pair with `0x887A0020` and the ETW
    /// `Microsoft-Windows-Direct3D12` reason *"MSAA quality reported to be 0"*.
    ///
    /// ⚠ **Expected non-zero**, and small. It is an incoherence inside vkd3d
    /// rather than a driver fault: `d3d12_device_get_format_support` sets
    /// `MULTISAMPLE_LOAD` when `supported_sample_counts != VK_SAMPLE_COUNT_1_BIT`,
    /// which is also true when that mask is **0**, while the quality-level query
    /// reads the same 0 and answers nothing. The depth-read views
    /// (`R32_FLOAT_X8X24_TYPELESS`, `X32_TYPELESS_G8X24_UINT` and their
    /// `D24_UNORM_S8_UINT` siblings) are where it shows up.
    pub(crate) caps_msaa_bits_dropped: RefusalCounter,
    /// A format that the engine answered `MULTISAMPLE_LOAD` **without**
    /// `MULTISAMPLE_RENDERTARGET`, while offering a quality level above 1
    /// sample, had `MULTISAMPLE_RENDERTARGET` added.
    ///
    /// ⛔ **This is the counter for the pair rule that failed `D12-G7` twice.**
    /// The runtime rejects `LOAD` alone with `0x887A0020` and the ETW
    /// `Microsoft-Windows-Direct3D12` reason *"MSAA quality reported to be 0"*;
    /// the D3D11 driver on this box cannot emit that pair by construction
    /// (`umd/src/forward/format_caps.rs:234-241`), and this is the D3D12
    /// equivalent made explicit and countable instead of structural.
    ///
    /// ⚠ **Expected non-zero**, and it is the depth-read views that produce it:
    /// `R32_FLOAT_X8X24_TYPELESS` (21), `X32_TYPELESS_G8X24_UINT` (22) and their
    /// `D24_UNORM_S8_UINT` siblings (46, 47), where vkd3d's `is_dsv_format` test
    /// fails on `plane_count == 2` and skips the arm that would have set the
    /// render-target bit.
    pub(crate) caps_msaa_rendertarget_implied: RefusalCounter,
    /// A format carrying `MULTISAMPLE_LOAD` without `RENDERTARGET` had `LOAD`
    /// dropped.
    ///
    /// ⛔ Measured 2026-08-06: the runtime aborts its format sweep at the first
    /// format answered that way. `D3D10_DDI_FORMAT_SUPPORT` defines `LOAD` as
    /// *"can be used as source for 'ld2dms'"*, which presupposes a colour target
    /// to have rendered into.
    ///
    /// ⚠ **Expected non-zero**: the depth-read views (21, 22, 46, 47) are what
    /// vkd3d answers this way, because its sampled-image arm sets `LOAD` while
    /// its `is_dsv_format` test refuses them the render-target arm.
    pub(crate) caps_msaa_load_without_rendertarget: RefusalCounter,
    /// `pfnQueryNodeMap` with a null `pMap`. Expected 0 — the DDI declares it
    /// `_Out_writes_(NumPhysicalAdapters)`.
    pub(crate) node_map_bad_arg: RefusalCounter,
    /// `pfnQueryNodeMap` was asked for a node count other than 1, and the
    /// identity map was written for every entry the runtime asked for.
    ///
    /// ⛔ Expected 0 on this guest: Helios is a single-node adapter, and
    /// `DDI_REFERENCE.md` §11.5h records four separate runtime strings that
    /// reject a bad remapping. A hit means the multi-adapter assumption behind
    /// `ARCHITECTURE.md` §13 UNVERIFIED-11 has been reached for real.
    pub(crate) node_map_unexpected_adapter_count: RefusalCounter,
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
    fill_ddi_table_bad_arg: RefusalCounter::new("FillDDITableBadArg"),
    fill_ddi_table_unknown_type: RefusalCounter::new("FillDDITableUnknownType"),
    fill_ddi_table_truncated: RefusalCounter::new("FillDDITableTruncated"),
    fill_ddi_table_oversized: RefusalCounter::new("FillDDITableOversized"),
    command_list_table_index_unbounded: RefusalCounter::new("CommandListTableIndexUnbounded"),
    calc_private_device_size_bad_arg: RefusalCounter::new("CalcPrivateDeviceSizeBadArg"),
    create_device_bad_arg: RefusalCounter::new("CreateDeviceBadArg"),
    create_device_engine_failed: RefusalCounter::new("CreateDeviceEngineFailed"),
    reserve_ranges_ignored: RefusalCounter::new("ReserveRangesIgnored"),
    destroy_device_bad_arg: RefusalCounter::new("DestroyDeviceBadArg"),
    caps_bad_arg: RefusalCounter::new("CapsBadArg"),
    caps_data_size_too_small: RefusalCounter::new("CapsDataSizeTooSmall"),
    caps_defaulted: RefusalCounter::new("CapsDefaulted"),
    caps_calls: RefusalCounter::new("CapsCalls"),
    caps_tiled_tier_clamped: RefusalCounter::new("CapsTiledTierClamped"),
    caps_total_lane_count_guess: RefusalCounter::new("CapsTotalLaneCountGuess"),
    caps_shader_models_truncated: RefusalCounter::new("CapsShaderModelsTruncated"),
    caps_texture_layout_set_end: RefusalCounter::new("CapsTextureLayoutSetEnd"),
    ddi12_version_mismatch: RefusalCounter::new("Ddi12VersionMismatch"),
    destroy_device_unexpected: RefusalCounter::new("DestroyDeviceUnexpected"),
    caps_format_support_calls: RefusalCounter::new("CapsFormatSupportCalls"),
    caps_msaa_calls: RefusalCounter::new("CapsMsaaCalls"),
    caps_slot_bad_arg: RefusalCounter::new("CapsSlotBadArg"),
    caps_slot_no_device: RefusalCounter::new("CapsSlotNoDevice"),
    caps_format_support_engine_failed: RefusalCounter::new("CapsFormatSupportEngineFailed"),
    caps_format_not_supported_sentinel: RefusalCounter::new("CapsFormatNotSupportedSentinel"),
    caps_msaa_engine_failed: RefusalCounter::new("CapsMsaaEngineFailed"),
    caps_msaa_tiled_refused: RefusalCounter::new("CapsMsaaTiledRefused"),
    caps_mip_packing_refused: RefusalCounter::new("CapsMipPackingRefused"),
    caps_msaa_bits_dropped: RefusalCounter::new("CapsMsaaBitsDropped"),
    caps_msaa_rendertarget_implied: RefusalCounter::new("CapsMsaaRenderTargetImplied"),
    caps_msaa_load_without_rendertarget: RefusalCounter::new("CapsMsaaLoadWithoutRenderTarget"),
    node_map_bad_arg: RefusalCounter::new("NodeMapBadArg"),
    node_map_unexpected_adapter_count: RefusalCounter::new("NodeMapUnexpectedAdapterCount"),
};

/// The set, in the order the summary prints them. ⛔ This order is the evidence
/// contract: `D3D12 DDI refusals:` lines from different builds get diffed, so
/// new counters are **appended**, never inserted.
static UMD12_REFUSAL_SET: [&RefusalCounter; 43] = [
    &UMD12_REFUSALS.open_adapter12,
    &UMD12_REFUSALS.probe12_bad_arg,
    &UMD12_REFUSALS.probe12_create_failed,
    &UMD12_REFUSALS.probe12_no_device,
    &UMD12_REFUSALS.probe12_err_blob_dropped,
    &UMD12_REFUSALS.open_adapter12_bad_arg,
    &UMD12_REFUSALS.adapter_unrecognised,
    &UMD12_REFUSALS.get_supported_versions_bad_arg,
    &UMD12_REFUSALS.get_optional_ddi_tables_bad_arg,
    &UMD12_REFUSALS.fill_ddi_table_bad_arg,
    &UMD12_REFUSALS.fill_ddi_table_unknown_type,
    &UMD12_REFUSALS.fill_ddi_table_truncated,
    &UMD12_REFUSALS.fill_ddi_table_oversized,
    &UMD12_REFUSALS.command_list_table_index_unbounded,
    &UMD12_REFUSALS.calc_private_device_size_bad_arg,
    &UMD12_REFUSALS.create_device_bad_arg,
    &UMD12_REFUSALS.create_device_engine_failed,
    &UMD12_REFUSALS.reserve_ranges_ignored,
    &UMD12_REFUSALS.destroy_device_bad_arg,
    &UMD12_REFUSALS.caps_bad_arg,
    &UMD12_REFUSALS.caps_data_size_too_small,
    &UMD12_REFUSALS.caps_defaulted,
    &UMD12_REFUSALS.caps_calls,
    &UMD12_REFUSALS.caps_tiled_tier_clamped,
    &UMD12_REFUSALS.caps_total_lane_count_guess,
    &UMD12_REFUSALS.caps_shader_models_truncated,
    &UMD12_REFUSALS.caps_texture_layout_set_end,
    &UMD12_REFUSALS.ddi12_version_mismatch,
    &UMD12_REFUSALS.destroy_device_unexpected,
    &UMD12_REFUSALS.caps_format_support_calls,
    &UMD12_REFUSALS.caps_msaa_calls,
    &UMD12_REFUSALS.caps_slot_bad_arg,
    &UMD12_REFUSALS.caps_slot_no_device,
    &UMD12_REFUSALS.caps_format_support_engine_failed,
    &UMD12_REFUSALS.caps_format_not_supported_sentinel,
    &UMD12_REFUSALS.caps_msaa_engine_failed,
    &UMD12_REFUSALS.caps_msaa_tiled_refused,
    &UMD12_REFUSALS.caps_mip_packing_refused,
    &UMD12_REFUSALS.caps_msaa_bits_dropped,
    &UMD12_REFUSALS.caps_msaa_rendertarget_implied,
    &UMD12_REFUSALS.caps_msaa_load_without_rendertarget,
    &UMD12_REFUSALS.node_map_bad_arg,
    &UMD12_REFUSALS.node_map_unexpected_adapter_count,
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
/// already-loud arm must not also emit the summary) — `CapsCalls` and
/// `CapsDefaulted` fire on every adapter open, and `CapsTotalLaneCountGuess` on
/// every device. A run in which only those fired would leave the set unprinted,
/// and T5's lesson is exactly that: *an instrument nothing can read is not an
/// instrument.*
///
/// Called from `adapter12::close_adapter` and from `device12`'s teardown
/// readout — adapter scope because D3D12 answers caps and fills tables there,
/// device scope because that is where "what did THIS device touch" is a
/// different question.
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

