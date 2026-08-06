//! **L1 — the caps gauntlet.** `pfnGetCaps` over the 43 `D3D12DDICAPS_TYPE`
//! enumerators, plus the three device-core format/MSAA query slots.
//!
//! # ⛔ Why this lane is one agent's, whole, and why it comes first
//!
//! `PARALLEL.md` §8: `D3D12Core.dll` enforces ~60 cross-tier consistency rules
//! and states them in English on ETW. Splitting caps across agents is how two
//! individually-plausible tiers become a rejected device. `DECISIONS.md` §7.8:
//! **advertising a capability that is not backed is a lie the OS acts on.**
//!
//! It comes first because **caps decide whether a device is created at all**,
//! and on Helios that is measured. S5's knob-ON run
//! (`tmp/dx12/gates/G6/RESULT.md`) shows the whole negotiation:
//!
//! ```text
//! OpenAdapter12 -> GetCaps(1074) -> GetCaps(1007) -> GetSupportedVersions x2 -> CloseAdapter
//! ```
//!
//! ⛔ **`pfnGetCaps` runs BEFORE `pfnGetSupportedVersions`** — the opposite of
//! `ARCHITECTURE.md` §1.2's original step order, corrected there from this
//! measurement. ⇒ **no answer in this file may depend on a negotiated interface
//! version, because at `pfnGetCaps` time there is not one.**
//!
//! # ⭐ The three rules that shape every answer below
//!
//! 1. **A failing HRESULT is tolerated on most caps, and fatal on ~13.**
//!    `D12-G5` measured WARP itself answering `1074` and `1080` with
//!    `E_UNEXPECTED` on *every* run while the device still created. But the
//!    ~13 caps with an explicit *"device creation fails"* runtime string were
//!    answered `S_OK` in every run, so refusing one is **untested**
//!    (`DDI_REFERENCE.md` §11.2). Every one of those 13 is answered here.
//! 2. **The default is "zero `pData` up to `DataSize` and return `S_OK`"**, not
//!    a refusal, and ⛔ **never `S_OK` without writing** — the runtime reads
//!    whatever was in its buffer.
//! 3. ⛔ **But the zero-fill default is ILLEGAL for four of the caps below**,
//!    and that is the least obvious thing in this file:
//!    * `1002 MEMORY_ARCHITECTURE` — `IOCoherent` **must be TRUE** on x86/amd64
//!      (strings:89), so an all-zero answer violates the cap by construction;
//!    * `1004 SHADER` — zeroed `WaveLaneCountMin/Max/TotalLaneCount` fails
//!      device creation outright (strings:29);
//!    * `1003 TEXTURE_LAYOUT_SETS` — zeroed alignments are **four** separate
//!      errors (strings:84-87: zero, non-power-of-two, out of range,
//!      `SubCaps[0].MaxElementSize == 0`);
//!    * `1088 OPTIONS_0110` — `D3D12DDI_EXECUTE_INDIRECT_TIER` has **no zero
//!      enumerator**: the only values are `_1_0 = 10` and `_1_1 = 11`, so a
//!      zero-fill writes an out-of-range tier, which the runtime **clamps
//!      silently**. That is CLAUDE.md rule 8 with the loud failure removed.
//!
//! # ⭐ THE COUPLING: this commit ships FEATURE LEVEL 11_0, and the OPTIONS
//! answer is only legal *because of that*
//!
//! ⛔ **Do not change one without the other.** `D12-G5` measured a reproducible
//! retail `D3D12CreateDevice` failure — `DXGI_ERROR_DRIVER_INTERNAL_ERROR`
//! (`0x887A0020`) with an English reason on ETW `Microsoft-Windows-Direct3D12`:
//!
//! > `FL12+ driver incorrectly did not report support for resource binding tier 2+.`
//!
//! The feature level is **asserted by the driver**, never inferred by the
//! runtime (`DDI_REFERENCE.md` §11.5.0), and asserting 12_0 arms a set of cap
//! floors: typed-UAV-load additional formats, `ResourceBindingTier >= 2`,
//! `TiledResourcesTier >= 2`; 12_1 adds ROVs and conservative rasterisation;
//! 12_2 adds eighteen more. **Every one of those is a lie on a driver whose
//! descriptor, resource and recording lanes are still counting noops.** So this
//! commit asserts **11_0**, which arms no floor, and answers every optional tier
//! at its absent value.
//!
//! ⚠ **The substrate is much more capable than this, and that is deliberate.**
//! Measured on a live vkd3d device on this guest (`docs/dx12/baselines/d3d12-caps.csv`):
//! `ResourceBindingTier 3`, `TiledResourcesTier 4`, SM **6.8**, RT tier 1_1,
//! mesh tier 1. Those are what the *engine* can do; this file reports what the
//! *driver* can do, and until L4/L5/L6 land those are different numbers. Each
//! raise belongs to the lane that earns it, in the commit that earns it, and
//! must move the feature level and the floors it arms **together**.
//!
//! # ⭐ The per-format half, at the bottom of this file
//!
//! The 43-enumerator gauntlet above is only part of what the runtime asks. The
//! three device-core slots this lane also owns — `pfnCheckFormatSupport`,
//! `pfnCheckMultisampleQualityLevels`, `pfnGetMipPacking` — are called
//! **2 823 times inside a single `D3D12CreateDevice`** (measured:
//! `tmp/dx12/gates/G7/RESULT.md`, 93 format queries + 2 730 MSAA queries; the
//! mip-packing slot is never reached).
//!
//! ⚠ An earlier revision of this paragraph said they were *"device-scope and
//! not on the path that gates device creation"*. **Measured, and wrong**: with
//! counting noops they answered 0, i.e. *"no format supports anything"*, which
//! is an inconsistent caps set and is exactly the
//! `DXGI_ERROR_DRIVER_INTERNAL_ERROR` `D12-G7` was failing with.
//!
//! They are implemented at the bottom of this file, forwarding into the vkd3d
//! engine's `ID3D12Device::CheckFeatureSupport` and narrowing the result to
//! what the caps above claim. The section header there carries the shape, the
//! `_NOT_SUPPORTED`-sentinel trap, and the one coherence check that looks right
//! and is not.

use helios_umd_common::hr::{Hresult, E_INVALIDARG, S_OK};

use crate::ddi12;
use crate::forward12::tables12::{stage, DeviceCoreTable, Filling};
use crate::{log_error, note_refusal, trace_line, UMD12_REFUSALS};

/// Short aliases for the bindgen enumerator names.
///
/// bindgen prefixes each constant with its enum's name and the SDK header
/// already does the same, so every enumerator arrives doubled —
/// `D3D12DDI_TILED_RESOURCES_TIER_D3D12DDI_TILED_RESOURCES_TIER_3`. Aliasing
/// them once keeps the answer table readable **without** hiding which generated
/// constant each one is: every line below is a compile-checked reference to the
/// header, not a transcribed number (`ARCHITECTURE.md` §12 rule 1).
mod v {
    use crate::ddi12::*;

    pub(super) const FL_11_0: D3D12DDI_3DPIPELINELEVEL =
        D3D12DDI_3DPIPELINELEVEL_D3D12DDI_3DPIPELINELEVEL_11_0;
    pub(super) const FL_12_1: D3D12DDI_3DPIPELINELEVEL =
        D3D12DDI_3DPIPELINELEVEL_D3D12DDI_3DPIPELINELEVEL_12_1;

    pub(super) const BINDING_TIER_1: D3D12DDI_RESOURCE_BINDING_TIER =
        D3D12DDI_RESOURCE_BINDING_TIER_D3D12DDI_RESOURCE_BINDING_TIER_1;
    pub(super) const CONSERVATIVE_RASTER_NONE: D3D12DDI_CONSERVATIVE_RASTERIZATION_TIER =
        D3D12DDI_CONSERVATIVE_RASTERIZATION_TIER_D3D12DDI_CONSERVATIVE_RASTERIZATION_TIER_NOT_SUPPORTED;
    pub(super) const TILED_NONE: D3D12DDI_TILED_RESOURCES_TIER =
        D3D12DDI_TILED_RESOURCES_TIER_D3D12DDI_TILED_RESOURCES_TIER_NOT_SUPPORTED;
    /// The ceiling this SDK's enum can express. ⛔ The clamp target, not a value
    /// this driver reports today — see `tiled_resources_tier`.
    pub(super) const TILED_MAX: D3D12DDI_TILED_RESOURCES_TIER =
        D3D12DDI_TILED_RESOURCES_TIER_D3D12DDI_TILED_RESOURCES_TIER_3;
    pub(super) const CROSS_NODE_NONE: D3D12DDI_CROSS_NODE_SHARING_TIER =
        D3D12DDI_CROSS_NODE_SHARING_TIER_D3D12DDI_CROSS_NODE_SHARING_TIER_NOT_SUPPORTED;
    pub(super) const HEAP_TIER_1: D3D12DDI_RESOURCE_HEAP_TIER =
        D3D12DDI_RESOURCE_HEAP_TIER_D3D12DDI_RESOURCE_HEAP_TIER_1;
    pub(super) const SAMPLE_POSITIONS_NONE: D3D12DDI_PROGRAMMABLE_SAMPLE_POSITIONS_TIER =
        D3D12DDI_PROGRAMMABLE_SAMPLE_POSITIONS_TIER_D3D12DDI_PROGRAMMABLE_SAMPLE_POSITIONS_TIER_NOT_SUPPORTED;
    pub(super) const QUEUE_FLAG_NONE: D3D12DDI_COMMAND_QUEUE_FLAGS =
        D3D12DDI_COMMAND_QUEUE_FLAGS_D3D12DDI_COMMAND_QUEUE_FLAG_NONE;
    pub(super) const VIEW_INSTANCING_NONE: D3D12DDI_VIEW_INSTANCING_TIER =
        D3D12DDI_VIEW_INSTANCING_TIER_D3D12DDI_VIEW_INSTANCING_TIER_NOT_SUPPORTED;
    pub(super) const RENDER_PASS_NONE: D3D12DDI_RENDER_PASS_TIER =
        D3D12DDI_RENDER_PASS_TIER_D3D12DDI_RENDER_PASS_TIER_NOT_SUPPORTED;
    pub(super) const RAYTRACING_NONE: D3D12DDI_RAYTRACING_TIER =
        D3D12DDI_RAYTRACING_TIER_D3D12DDI_RAYTRACING_TIER_NOT_SUPPORTED;
    pub(super) const VRS_NONE: D3D12DDI_VARIABLE_SHADING_RATE_TIER =
        D3D12DDI_VARIABLE_SHADING_RATE_TIER_D3D12DDI_VARIABLE_SHADING_RATE_TIER_NOT_SUPPORTED;
    pub(super) const MESH_NONE: D3D12DDI_MESH_SHADER_TIER =
        D3D12DDI_MESH_SHADER_TIER_D3D12DDI_MESH_SHADER_TIER_NOT_SUPPORTED;
    pub(super) const SAMPLER_FEEDBACK_NONE: D3D12DDI_SAMPLER_FEEDBACK_TIER =
        D3D12DDI_SAMPLER_FEEDBACK_TIER_D3D12DDI_SAMPLER_FEEDBACK_TIER_NOT_SUPPORTED;
    pub(super) const MIN_PRECISION_NONE: D3D12DDI_SHADER_MIN_PRECISION =
        D3D12DDI_SHADER_MIN_PRECISION_D3D12DDI_SHADER_MIN_PRECISION_NONE;
    pub(super) const WAVE_MMA_NONE: D3D12DDI_WAVE_MMA_TIER =
        D3D12DDI_WAVE_MMA_TIER_D3D12DDI_WAVE_MMA_TIER_NOT_SUPPORTED;
    /// ⛔ `D3D12DDI_EXECUTE_INDIRECT_TIER` has NO zero enumerator: `_1_0 = 10`
    /// and `_1_1 = 11` are the only values, so 0 is out of range and the runtime
    /// clamps it silently.
    pub(super) const EXECUTE_INDIRECT_1_0: D3D12DDI_EXECUTE_INDIRECT_TIER =
        D3D12DDI_EXECUTE_INDIRECT_TIER_D3D12DDI_EXECUTE_INDIRECT_TIER_1_0;
    pub(super) const HEAP_SERIALIZATION_0: D3D12DDI_HEAP_SERIALIZATION_TIER_0041 =
        D3D12DDI_HEAP_SERIALIZATION_TIER_0041_D3D12DDI_HEAP_SERIALIZATION_TIER_0041_0;
    pub(super) const RESOURCE_SERIALIZATION_0: D3D12DDI_RESOURCE_SERIALIZATION_TIER_0041 =
        D3D12DDI_RESOURCE_SERIALIZATION_TIER_0041_D3D12DDI_RESOURCE_SERIALIZATION_TIER_0041_0;
    pub(super) const ROW_MAJOR_FLAG_NONE: D3D12DDI_ROW_MAJOR_LAYOUT_FLAGS =
        D3D12DDI_ROW_MAJOR_LAYOUT_FLAGS_D3D12DDI_ROW_MAJOR_LAYOUT_FLAG_NONE;
    pub(super) const TL_ROW_MAJOR: D3D12DDI_TEXTURE_LAYOUT =
        D3D12DDI_TEXTURE_LAYOUT_D3D12DDI_TL_ROW_MAJOR;
    pub(super) const FUNCUNIT_COMBINED: D3D12DDI_FUNCTIONAL_UNIT =
        D3D12DDI_FUNCTIONAL_UNIT_D3D12DDI_FUNCUNIT_COMBINED;
    pub(super) const SM_5_1: D3D12DDI_SHADER_MODEL =
        D3D12DDI_SHADER_MODEL_D3D12DDI_SHADER_MODEL_5_1_RELEASE_0011;
    pub(super) const SM_6_0: D3D12DDI_SHADER_MODEL =
        D3D12DDI_SHADER_MODEL_D3D12DDI_SHADER_MODEL_6_0_RELEASE_0011;

    pub(super) const CAPS_TEXTURE_LAYOUT: D3D12DDICAPS_TYPE =
        D3D12DDICAPS_TYPE_D3D12DDICAPS_TYPE_TEXTURE_LAYOUT;
    pub(super) const CAPS_MEMORY_ARCHITECTURE: D3D12DDICAPS_TYPE =
        D3D12DDICAPS_TYPE_D3D12DDICAPS_TYPE_MEMORY_ARCHITECTURE;
    pub(super) const CAPS_TEXTURE_LAYOUT_SETS: D3D12DDICAPS_TYPE =
        D3D12DDICAPS_TYPE_D3D12DDICAPS_TYPE_TEXTURE_LAYOUT_SETS;
    pub(super) const CAPS_SHADER: D3D12DDICAPS_TYPE = D3D12DDICAPS_TYPE_D3D12DDICAPS_TYPE_SHADER;
    pub(super) const CAPS_ARCHITECTURE_INFO: D3D12DDICAPS_TYPE =
        D3D12DDICAPS_TYPE_D3D12DDICAPS_TYPE_ARCHITECTURE_INFO;
    pub(super) const CAPS_D3D12_OPTIONS: D3D12DDICAPS_TYPE =
        D3D12DDICAPS_TYPE_D3D12DDICAPS_TYPE_D3D12_OPTIONS;
    pub(super) const CAPS_3DPIPELINESUPPORT: D3D12DDICAPS_TYPE =
        D3D12DDICAPS_TYPE_D3D12DDICAPS_TYPE_3DPIPELINESUPPORT;
    pub(super) const CAPS_GPUVA: D3D12DDICAPS_TYPE =
        D3D12DDICAPS_TYPE_D3D12DDICAPS_TYPE_GPUVA_CAPS;
    pub(super) const CAPS_TEXTURE_LAYOUT1: D3D12DDICAPS_TYPE =
        D3D12DDICAPS_TYPE_D3D12DDICAPS_TYPE_TEXTURE_LAYOUT1;
    pub(super) const CAPS_SHADER_MODELS: D3D12DDICAPS_TYPE =
        D3D12DDICAPS_TYPE_D3D12DDICAPS_TYPE_0011_SHADER_MODELS;
    pub(super) const CAPS_CPU_PAGE_TABLE_FALSE_POSITIVES: D3D12DDICAPS_TYPE =
        D3D12DDICAPS_TYPE_D3D12DDICAPS_TYPE_0022_CPU_PAGE_TABLE_FALSE_POSITIVES;
    pub(super) const CAPS_0022_TEXTURE_LAYOUT: D3D12DDICAPS_TYPE =
        D3D12DDICAPS_TYPE_D3D12DDICAPS_TYPE_0022_TEXTURE_LAYOUT;
    pub(super) const CAPS_ADAPTER_COMPUTE_ONLY: D3D12DDICAPS_TYPE =
        D3D12DDICAPS_TYPE_D3D12DDICAPS_TYPE_0033_ADAPTER_COMPUTE_ONLY;
    pub(super) const CAPS_HARDWARE_SCHEDULING: D3D12DDICAPS_TYPE =
        D3D12DDICAPS_TYPE_D3D12DDICAPS_TYPE_0050_HARDWARE_SCHEDULING_CAPS;
    pub(super) const CAPS_3DPIPELINESUPPORT1: D3D12DDICAPS_TYPE =
        D3D12DDICAPS_TYPE_D3D12DDICAPS_TYPE_0081_3DPIPELINESUPPORT1;
    pub(super) const CAPS_OPTIONS_0102: D3D12DDICAPS_TYPE =
        D3D12DDICAPS_TYPE_D3D12DDICAPS_TYPE_OPTIONS_0102;
    pub(super) const CAPS_OPTIONS_0110: D3D12DDICAPS_TYPE =
        D3D12DDICAPS_TYPE_D3D12DDICAPS_TYPE_OPTIONS_0110;
}

/// ⭐ **The feature level this driver asserts, and the single value the whole
/// OPTIONS answer is coupled to.**
///
/// 11_0 arms **no** cap floor. Raising it arms them all at once — see the
/// module doc — so this constant, `d3d12_options`'s tiers and
/// `shader_caps`'s `ROVs` / `TypedUAVLoadAdditionalFormats` move **together or
/// not at all**.
///
/// # ⭐ 11_0 IS A STAGING VALUE. THE TARGET IS 12_2.
///
/// Owner directive, 2026-08-06: the goal by **P4 (first frame) / P5
/// (conformance)** is the **highest feature level D3D12 defines — FL 12_2,
/// "DirectX Ultimate"**. `DX12.md` §4.4 is the ladder: which floors each step
/// arms, and which lane must land before the step can be taken.
///
/// ⭐ **The ceiling is not the substrate.** Every one of the eighteen FL 12_2
/// floors is already backed on this guest — `baselines/d3d12-caps.csv` has them
/// all, and vkd3d's own log says `DX Ultimate supported!`. What gates the level
/// is that the DDI slots behind each cap become real, because `DECISIONS.md`
/// §7.8 forbids advertising what the *driver* cannot back.
///
/// ⛔ So this constant is not "raise it when someone feels brave". It is
/// **`min(what every lane has landed)`**, and the commit that raises it is the
/// commit that raises its floors — never one without the other, which is the
/// failure `D12-G5` measured verbatim.
///
/// ⚠ The long pole is `TiledResourcesTier >= 3`, and it is the one floor that
/// is **not a UMD-only job**: `kmd_render`'s guest page tables are decorative
/// (`kmd_render/src/ddi/gpummu.rs:1-14`). See §4.4.
///
/// ⛔ Two values this must never be, both by precedent:
/// * a **bitmask**. `D3D12DDICAPS_TYPE_3DPIPELINESUPPORT` is a *maximum level*
///   for D3D12 — the exact opposite of `D3D11DDICAPS_3DPIPELINESUPPORT`, which
///   `umd/src/caps.rs:57-66` builds as `0x8F`. Writing `0x8F` here reads as
///   "level 143".
/// * `1_0_CORE`. That is the **compute-only** level, and it is what the retired
///   R908 body reported. Do not resurrect it by copy-paste.
const DRIVER_MAX_FEATURE_LEVEL: ddi12::D3D12DDI_3DPIPELINELEVEL = v::FL_11_0;

// ---------------------------------------------------------------------------
// The three caps that a per-format answer is coupled to
// ---------------------------------------------------------------------------
//
// ⭐ These exist because the format/MSAA slots at the bottom of this file must
// give the **same** answer as the caps structs at the top, and nothing else
// would make that checkable. `D3D12Core.dll` cross-validates the caps set as a
// whole (`DDI_REFERENCE.md` §11.5); a per-format bit that contradicts the tier
// two hundred lines above it is exactly the inconsistency `D12-G7` was failing
// on. Each is read at **both** sites, so the lane that raises one finds the
// other by following the constant rather than by remembering.

/// The tiled-resources tier this driver reports, and the reason
/// `D3D12DDI_FORMAT_SUPPORT_TILED` is withheld from every format and a
/// `TILED_RESOURCE` multisample query is answered with zero quality levels.
///
/// See [`tiled_resources_tier`] for why it is `NOT_SUPPORTED` while the engine
/// backs tier 4.
const TILED_RESOURCES_TIER_REPORTED: ddi12::D3D12DDI_TILED_RESOURCES_TIER = v::TILED_NONE;

/// `D3D12DDI_SHADER_CAPS_0084::TypedUAVLoadAdditionalFormats`, and the reason
/// `D3D12DDI_FORMAT_SUPPORT_UAV_READS` is narrowed to the three formats FL 11_0
/// mandates.
///
/// ⛔ FALSE is only legal *below* FL 12_0 — strings:169, *"FL 12+ driver
/// incorrectly does not report support for typed UAV load additional formats."* —
/// so this constant and [`DRIVER_MAX_FEATURE_LEVEL`] move together.
const TYPED_UAV_LOAD_ADDITIONAL_FORMATS: ddi12::BOOL = 0;

/// `D3D12DDI_D3D12_OPTIONS_DATA_0089::OutputMergerLogicOp`, and the reason
/// `D3D12DDI_FORMAT_SUPPORT_OUTPUT_MERGER_LOGIC_OP` is withheld from every
/// format.
const OUTPUT_MERGER_LOGIC_OP: ddi12::BOOL = 0;

/// The three `DXGI_FORMAT`s whose typed UAV load FL 11_0 mandates
/// unconditionally: `R32_FLOAT`, `R32_UINT`, `R32_SINT`.
///
/// ⭐ This is what makes [`TYPED_UAV_LOAD_ADDITIONAL_FORMATS`] `= FALSE` mean
/// what it says. The cap's name is *additional* formats: FALSE narrows typed UAV
/// loads to these three rather than removing them, so masking `UAV_READS` off
/// everywhere would under-report a floor FL 11_0 requires, and forwarding it
/// everywhere would over-report the cap. Both directions are wrong; this is the
/// only answer consistent with both.
const FL11_TYPED_UAV_LOAD_FORMATS: [ddi12::DXGI_FORMAT; 3] = [41, 42, 43];

/// `Umd12FormatCaps` = 1: hand the engine's API-level `D3D12_FORMAT_SUPPORT1`
/// back unchanged instead of translating it into `D3D12DDI_FORMAT_SUPPORT`.
/// See [`crate::knobs12::UMD12_FORMAT_CAPS`] and [`driver_format_support`].
const FORMAT_CAPS_API_PASSTHROUGH: u32 = 1;

/// How many times a bounded evidence line may repeat, per site.
const LOG_BUDGET: usize = 64;

/// The per-call evidence budget for `pfnCheckFormatSupport`.
///
/// ⚠ Deliberately larger than [`LOG_BUDGET`] and sized to cover a **whole
/// sweep**: `D12-G7` measured 93 calls inside one `D3D12CreateDevice`, and the
/// question this budget has to answer on a failing run is *"which format got
/// which bits"* — an answer that is useless truncated at 64 of 93. Process
/// global, so the second device's sweep is silent.
const FORMAT_SUPPORT_LOG_BUDGET: usize = 128;

/// The per-call evidence budget for `pfnCheckMultisampleQualityLevels`.
///
/// ⛔ Small on purpose: the same measurement counted **2 730** calls in one
/// device creation (91 formats x 30 sample counts). A budget that covered the
/// sweep would be a log flood on every device, and the counters carry the
/// aggregate. The refusal arms below have their own separate budgets, so a rare
/// event is never crowded out by the common one.
const MSAA_LOG_BUDGET: usize = 16;

// ---------------------------------------------------------------------------
// The pData writer
// ---------------------------------------------------------------------------

/// Zero the runtime's buffer up to `DataSize`, then write `value` over it.
///
/// ⛔ **Both halves matter.** Returning `S_OK` without writing leaves the
/// runtime reading whatever was in its own buffer (`DDI_REFERENCE.md` §11.2);
/// writing `size_of::<T>()` without checking `DataSize` is the R702 class again,
/// in a second place. The zero-fill covers the tail when the runtime asked for a
/// *newer, larger* struct than this header describes — those fields then read as
/// "absent", which is the conservative answer for every cap in this file.
///
/// ⚠ **Only for pure-OUT caps.** Four of the caps below carry runtime *inputs*
/// in `pData` (`1074` is in/out, `1085` is six-sevenths input, `1073` hands over
/// a caller-owned array) and zeroing them destroys the question before it is
/// read. Those are handled by hand and do not come through here.
///
/// # Safety
/// `p_data` must point at `data_size` writable bytes the runtime owns.
unsafe fn write_caps<T>(
    name: &str,
    p_data: *mut core::ffi::c_void,
    data_size: usize,
    value: T,
) -> Hresult {
    if data_size < core::mem::size_of::<T>() {
        note_refusal(&UMD12_REFUSALS.caps_data_size_too_small);
        log_error!(
            "GetCaps {name}: runtime buffer is {data_size} B, the answer needs {} B -> \
             E_INVALIDARG (nothing written)",
            core::mem::size_of::<T>(),
        );
        return E_INVALIDARG;
    }
    // SAFETY: `data_size` writable bytes per the caller's guarantee, and
    // `size_of::<T>() <= data_size` per the check above.
    unsafe {
        core::ptr::write_bytes(p_data.cast::<u8>(), 0, data_size);
        core::ptr::write_unaligned(p_data.cast::<T>(), value);
    }
    S_OK
}

// ---------------------------------------------------------------------------
// pfnGetCaps
// ---------------------------------------------------------------------------

/// Answer one caps query.
///
/// Called from `adapter12::get_caps`, which owns the DDI slot.
///
/// # Safety
/// `arg`, when non-null, must point at a live `D3D12DDIARG_GETCAPS` whose
/// `pData` addresses `DataSize` writable bytes the runtime owns, and whose
/// `pInfo` — where the cap defines one — addresses a readable input of the
/// shape that cap documents.
pub(crate) unsafe fn get_caps(arg: *const ddi12::D3D12DDIARG_GETCAPS) -> Hresult {
    if arg.is_null() {
        note_refusal(&UMD12_REFUSALS.caps_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: non-null per the check above; the DDI declares it `_In_ CONST`.
    let a = unsafe { &*arg };
    if a.pData.is_null() {
        note_refusal(&UMD12_REFUSALS.caps_bad_arg);
        return E_INVALIDARG;
    }
    let data_size = a.DataSize as usize;

    // ⭐ Bounded contract capture on our own adapter. `D12-G5` needed a spy
    // proxy in front of WARP to learn which caps this runtime asks for, in what
    // order and at what `DataSize`; from here it is recorded for free.
    let n = UMD12_REFUSALS.caps_calls.get() + 1;
    UMD12_REFUSALS.caps_calls.bump();
    if n <= LOG_BUDGET {
        log_error!(
            "GetCaps: type={} DataSize={data_size} pInfo={:p} (x{n})",
            a.Type,
            a.pInfo,
        );
    }

    // ⛔ An exhaustive match is impossible here and pretending otherwise would be
    // the lie: `D3D12DDICAPS_TYPE` is a bindgen `c_int`, not a Rust enum, so
    // `_` is mandatory. What `DECISIONS.md` §7.4 actually demands is that the
    // default arm cannot select a *shape* — and it cannot: it writes zeroes and
    // nothing else.
    match a.Type {
        // ── The feature level. Both selectors, and the coupling above. ──────
        v::CAPS_3DPIPELINESUPPORT1 => unsafe { pipeline_support1(a) },
        v::CAPS_3DPIPELINESUPPORT => unsafe { pipeline_support(a, data_size) },

        // ── The must-answer set (`DDI_REFERENCE.md` §11.2) ──────────────────
        v::CAPS_D3D12_OPTIONS => unsafe { d3d12_options(a, data_size) },
        v::CAPS_ARCHITECTURE_INFO => unsafe { architecture_info(a, data_size) },
        v::CAPS_SHADER => unsafe { shader_caps(a, data_size) },
        v::CAPS_SHADER_MODELS => unsafe { shader_models(a, data_size) },
        v::CAPS_MEMORY_ARCHITECTURE => unsafe { memory_architecture(a, data_size) },
        v::CAPS_GPUVA => unsafe { gpuva_caps(a, data_size) },
        v::CAPS_HARDWARE_SCHEDULING => unsafe { hardware_scheduling(a, data_size) },
        v::CAPS_CPU_PAGE_TABLE_FALSE_POSITIVES => unsafe {
            cpu_page_table_false_positives(a, data_size)
        },
        v::CAPS_TEXTURE_LAYOUT | v::CAPS_TEXTURE_LAYOUT1 => unsafe {
            texture_layout_deprecated(a, data_size)
        },
        v::CAPS_0022_TEXTURE_LAYOUT => unsafe { texture_layout_0022(a, data_size) },
        v::CAPS_TEXTURE_LAYOUT_SETS => unsafe { texture_layout_sets(a, data_size) },

        // ── Answered because their zero is illegal or their meaning is known ─
        v::CAPS_OPTIONS_0110 => unsafe { options_0110(a, data_size) },
        v::CAPS_OPTIONS_0102 => unsafe { options_0102(a, data_size) },
        v::CAPS_ADAPTER_COMPUTE_ONLY => unsafe { adapter_compute_only(a, data_size) },

        // ── The §11.2 safe default ──────────────────────────────────────────
        other => {
            UMD12_REFUSALS.caps_defaulted.bump();
            let d = UMD12_REFUSALS.caps_defaulted.get();
            if d <= LOG_BUDGET {
                log_error!(
                    "GetCaps: type={other} not individually answered; zeroing {data_size} B \
                     and returning S_OK (x{d})"
                );
            }
            // SAFETY: `pData` is non-null (checked above) and the runtime
            // guarantees `DataSize` writable bytes behind it.
            unsafe { core::ptr::write_bytes(a.pData.cast::<u8>(), 0, data_size) };
            S_OK
        }
    }
}

/// `1074 _0081_3DPIPELINESUPPORT1` — **in/out**, and the one the modern runtime
/// asks first.
///
/// ⛔ **Never zero-filled**: `HighestRuntimeSupportedFeatureLevel` is the
/// runtime's *input*, and destroying it destroys the question.
/// `out = min(driver_max, in)` (`DDI_REFERENCE.md` §11.3).
///
/// ⭐ **Not implementing this is a SILENT demotion**, which is why it is
/// answered even though the driver's own maximum is only 11_0: the runtime falls
/// back to `1007` on failure, and `1007` may never answer above 12_1
/// (`DX12.md` §4.3 row 2). WARP answers `E_UNEXPECTED` here on every run and its
/// device still creates, so a refusal is *tolerated* — but it is tolerated at
/// the cost of a cap nobody chose, which is the thing CLAUDE.md rule 8 is about.
///
/// # Safety
/// As [`get_caps`].
unsafe fn pipeline_support1(a: &ddi12::D3D12DDIARG_GETCAPS) -> Hresult {
    let needed = core::mem::size_of::<ddi12::D3D12DDI_3DPIPELINESUPPORT1_DATA_0081>();
    if (a.DataSize as usize) < needed {
        note_refusal(&UMD12_REFUSALS.caps_data_size_too_small);
        return E_INVALIDARG;
    }
    let slot = a.pData.cast::<ddi12::D3D12DDI_3DPIPELINESUPPORT1_DATA_0081>();
    // SAFETY: non-null and at least `needed` writable bytes per the check above.
    // Read first, write second, and never `write_bytes` over it.
    let runtime_max = unsafe { core::ptr::read_unaligned(slot) }.HighestRuntimeSupportedFeatureLevel;
    let answer = if DRIVER_MAX_FEATURE_LEVEL <= runtime_max {
        DRIVER_MAX_FEATURE_LEVEL
    } else {
        runtime_max
    };
    log_error!(
        "GetCaps 3DPIPELINESUPPORT1: runtime understands {runtime_max}, driver max \
         {DRIVER_MAX_FEATURE_LEVEL} -> {answer}"
    );
    // SAFETY: as above. Only the OUT field is written.
    unsafe {
        core::ptr::write_unaligned(
            slot,
            ddi12::D3D12DDI_3DPIPELINESUPPORT1_DATA_0081 {
                HighestRuntimeSupportedFeatureLevel: runtime_max,
                MaximumDriverSupportedFeatureLevel: answer,
            },
        );
    }
    S_OK
}

/// `1007 3DPIPELINESUPPORT` — the legacy selector, **clamped at 12_1**.
///
/// ⛔ The header mandates it: *"the driver must not return anything higher than
/// 12_1"*, because a pre-Vibranium runtime sanitises anything it does not
/// understand down to `1_0 core`. The clamp is explicit here even though this
/// driver's maximum is 11_0, so raising [`DRIVER_MAX_FEATURE_LEVEL`] cannot
/// silently break it.
///
/// # Safety
/// As [`get_caps`].
unsafe fn pipeline_support(a: &ddi12::D3D12DDIARG_GETCAPS, data_size: usize) -> Hresult {
    let level = if DRIVER_MAX_FEATURE_LEVEL <= v::FL_12_1 {
        DRIVER_MAX_FEATURE_LEVEL
    } else {
        v::FL_12_1
    };
    log_error!("GetCaps 3DPIPELINESUPPORT -> {level}");
    // SAFETY: as [`get_caps`].
    unsafe { write_caps("3DPIPELINESUPPORT", a.pData, data_size, level) }
}

/// `1006 D3D12_OPTIONS` — all 31 fields, written explicitly.
///
/// ⛔ **Every value here is coupled to [`DRIVER_MAX_FEATURE_LEVEL`] = 11_0.**
/// See the module doc: asserting 12_0 would *force* `ResourceBindingTier >= 2`,
/// `TiledResourcesTier >= 2` and typed-UAV-load additional formats, and the
/// runtime fails device creation with an English reason when the set is
/// inconsistent.
///
/// Two values that are pinned for Helios-specific reasons rather than tier
/// policy, both from `DDI_REFERENCE.md` §11.6:
/// * `Deterministic64KBUndefinedSwizzle = FALSE` — TRUE makes applications write
///   texture tiles CPU-side in the standard swizzle and expect the GPU to read
///   them back identically. On this stack the real layout is chosen **host-side**
///   by venus/NVIDIA and is not knowable to the guest ⇒ garbage texels **with no
///   error path at all**. vkd3d hardcodes it FALSE; so does this.
/// * `RenderPassTier = NOT_SUPPORTED` — the tier and the render-pass DDI table
///   must agree in **both** directions: a tier without the table is an error.
///   Helios answers `pfnGetOptionalDDITables` with zero tables and never fills
///   `D3D12DDI_TABLE_TYPE_0043_RENDER_PASS`, so the tier must be absent.
///
/// # Safety
/// As [`get_caps`].
unsafe fn d3d12_options(a: &ddi12::D3D12DDIARG_GETCAPS, data_size: usize) -> Hresult {
    let options = ddi12::D3D12DDI_D3D12_OPTIONS_DATA_0089 {
        // FL 11_0's floor. ⚠ The engine backs TIER_3 unconditionally
        // (baselines/d3d12-caps.csv), but L5 has not written a descriptor
        // handler yet, so this reports what the DRIVER does.
        ResourceBindingTier: v::BINDING_TIER_1,
        ConservativeRasterizationTier: v::CONSERVATIVE_RASTER_NONE,
        TiledResourcesTier: tiled_resources_tier(),
        CrossNodeSharingTier: v::CROSS_NODE_NONE,
        VPAndRTArrayIndexFromAnyShaderFeedingRasterizerSupportedWithoutGSEmulation: 0,
        // ⚠ Read from the shared constant, not written as a literal: the
        // per-format `OUTPUT_MERGER_LOGIC_OP` bit at the bottom of this file is
        // gated on the same value, and a driver that reports the cap FALSE
        // while setting the bit on a format contradicts itself.
        OutputMergerLogicOp: OUTPUT_MERGER_LOGIC_OP,
        ResourceHeapTier: v::HEAP_TIER_1,
        DepthBoundsTestSupported: 0,
        ProgrammableSamplePositionsTier: v::SAMPLE_POSITIONS_NONE,
        CopyQueueTimestampQueriesSupported: 0,
        // A bitmask, not a tier. NONE = "no queue supports WriteBufferImmediate",
        // which is the honest answer while `pfnWriteBufferImmediate` is a noop.
        WriteBufferImmediateQueueFlags: v::QUEUE_FLAG_NONE,
        ViewInstancingTier: v::VIEW_INSTANCING_NONE,
        BarycentricsSupported: 0,
        ReservedBufferPlacementSupported: 0,
        // ⛔ See the doc above: garbage texels with no error path.
        Deterministic64KBUndefinedSwizzle: 0,
        SRVOnlyTiledResourceTier3: 0,
        // ⛔ A tier without the render-pass DDI table is an error.
        RenderPassTier: v::RENDER_PASS_NONE,
        RaytracingTier: v::RAYTRACING_NONE,
        VariableShadingRateTier: v::VRS_NONE,
        PerPrimitiveShadingRateSupportedWithViewportIndexing: 0,
        AdditionalShadingRatesSupported: 0,
        // Must be valid at VRS tier >= 1 and non-zero at tier 2. Zero is correct
        // at NOT_SUPPORTED and is the only value that cannot contradict it.
        ShadingRateImageTileSize: 0,
        BackgroundProcessingSupported: 0,
        MeshShaderTier: v::MESH_NONE,
        SamplerFeedbackTier: v::SAMPLER_FEEDBACK_NONE,
        DriverManagedShaderCachePresent: 0,
        MeshShaderSupportsFullRangeRenderTargetArrayIndex: 0,
        VariableRateShadingSumCombinerSupported: 0,
        MeshShaderPerPrimitiveShadingRateSupported: 0,
        MSPrimitivesPipelineStatisticIncludesCulledPrimitives: 0,
        // ⛔ FALSE until L3c's `pfnBarrier` is real. At TRUE the runtime lowers
        // EVERY `ResourceBarrier` to `pfnBarrier` and *"legacy barrier DDI's are
        // never invoked"* — there is no fallback left (`DX12.md` §4.3 row 1).
        EnhancedBarriersSupported: 0,
    };
    // SAFETY: as [`get_caps`].
    unsafe { write_caps("D3D12_OPTIONS", a.pData, data_size, options) }
}

/// The tiled-resources tier, **clamped explicitly**.
///
/// ⭐ `DX12.md` §4.3 row 3 / `DDI_REFERENCE.md` §11.4.1: a live vkd3d device on
/// this guest reports **tier 4**, `D3D12DDI_TILED_RESOURCES_TIER` stops at 3 in
/// SDK 26100, and an out-of-range tier is **clamped silently** — so without an
/// explicit clamp Helios ships a number nobody chose, which is CLAUDE.md rule 8
/// in its purest form.
///
/// This driver reports `NOT_SUPPORTED` today because `pfnUpdateTileMappings` is
/// a counting noop and the KMD's guest page tables are decorative
/// (`kmd_render/src/ddi/gpummu.rs:1-14`), so reads from unmapped tiles would
/// return whatever was there instead of zero — with no failure the app can see.
/// The clamp is written now, at the site, so that the lane which raises this
/// cannot forget it.
fn tiled_resources_tier() -> ddi12::D3D12DDI_TILED_RESOURCES_TIER {
    let engine_reports = TILED_RESOURCES_TIER_REPORTED;
    if engine_reports > v::TILED_MAX {
        UMD12_REFUSALS.caps_tiled_tier_clamped.bump();
        return v::TILED_MAX;
    }
    engine_reports
}

/// `1005 ARCHITECTURE_INFO`. An NVIDIA discrete GPU behind venus is not a
/// tile-based deferred renderer, and the measured baseline agrees.
///
/// # Safety
/// As [`get_caps`].
unsafe fn architecture_info(a: &ddi12::D3D12DDIARG_GETCAPS, data_size: usize) -> Hresult {
    let info = ddi12::D3D12DDI_ARCHITECTURE_INFO_DATA {
        TileBasedDeferredRenderer: 0,
    };
    // SAFETY: as [`get_caps`].
    unsafe { write_caps("ARCHITECTURE_INFO", a.pData, data_size, info) }
}

/// `1004 SHADER` — 16 fields, and ⛔ **the zero-fill default is fatal here**.
///
/// > `Driver did not set valid WaveLaneCountMin/Max or TotalLaneCount via
/// > D3D12DDICAPS_TYPE_SHADER caps query` — strings:29
///
/// so an all-zero answer fails device creation. The lane counts are filled
/// even though `WaveOps` is FALSE, because it is **not established** that the
/// check is gated on `WaveOps`.
///
/// ⚠ **`TotalLaneCount` is a known-wrong placeholder and is counted as one.**
/// 32 x 32 is vkd3d's fallback; `DDI_REFERENCE.md` §11.7 records that it
/// under-reports by roughly 24x on this GPU, because venus exposes neither
/// `VK_AMD_shader_core_properties` nor `VK_NV_shader_sm_builtins` and there is
/// no other source for the real figure. `CapsTotalLaneCountGuess` exists so the
/// number is never mistaken for a measurement.
///
/// The five SM 6.6 fields (three `AtomicInt64*`, `DerivativesInMeshAnd...`,
/// `WaveMMATier`) are FALSE because the shader-model list below stops at 6.0 —
/// strings:116, which `D12-G5` proved is a live retail gate. `ROVs` is FALSE
/// both because FL 11_0 arms no floor and because there is no real
/// fragment-shader interlock: `DDI_REFERENCE.md` §11.6 hazard 2 calls that
/// *"non-deterministically wrong and frame-rate dependent"*, the hardest class
/// this project has already burned four sessions on.
///
/// # Safety
/// As [`get_caps`].
unsafe fn shader_caps(a: &ddi12::D3D12DDIARG_GETCAPS, data_size: usize) -> Hresult {
    /// The guest's Vulkan subgroup size, min and max
    /// (`VkPhysicalDeviceVulkan13Properties`, measured 32/32 on this host).
    const SUBGROUP_SIZE: ddi12::UINT = 32;
    /// vkd3d's fallback: `32 * subgroupSize`. See the doc above.
    const TOTAL_LANE_COUNT_GUESS: ddi12::UINT = 32 * SUBGROUP_SIZE;

    UMD12_REFUSALS.caps_total_lane_count_guess.bump();
    let caps = ddi12::D3D12DDI_SHADER_CAPS_0084 {
        MinPrecision: v::MIN_PRECISION_NONE,
        DoubleOps: 0,
        ShaderSpecifiedStencilRef: 0,
        // ⚠ The shared constant, for the same reason as `OutputMergerLogicOp`:
        // it is what narrows the per-format `UAV_READS` bit at the bottom of
        // this file to the three formats FL 11_0 mandates.
        TypedUAVLoadAdditionalFormats: TYPED_UAV_LOAD_ADDITIONAL_FORMATS,
        ROVs: 0,
        WaveOps: 0,
        WaveLaneCountMin: SUBGROUP_SIZE,
        WaveLaneCountMax: SUBGROUP_SIZE,
        TotalLaneCount: TOTAL_LANE_COUNT_GUESS,
        Int64Ops: 0,
        Native16BitOps: 0,
        AtomicInt64OnTypedResource: 0,
        AtomicInt64OnGroupShared: 0,
        DerivativesInMeshAndAmplificationShaders: 0,
        WaveMMATier: v::WAVE_MMA_NONE,
        AtomicInt64OnDescriptorHeapResource: 0,
    };
    // SAFETY: as [`get_caps`].
    unsafe { write_caps("SHADER", a.pData, data_size, caps) }
}

/// `1012 _0011_SHADER_MODELS` — **the caller owns both pointers**, so this is
/// not a `write_caps` shape.
///
/// The struct is two out-pointers, not a value:
/// `{ UINT* pNumShaderModelsSupported; D3D12DDI_SHADER_MODEL* pShaderModelsSupported; }`.
/// The count is IN (the caller's capacity) and OUT (what we wrote), and the
/// array is the caller's storage. ⛔ Zeroing `pData` would null both pointers.
///
/// The list must be **non-empty**, **gapless** across release shader models, and
/// **must include 5.1** (strings:24-25 and the §11.5(a) rules).
///
/// ⚠ **The substrate measures SM 6.8** on a live vkd3d device
/// (`baselines/d3d12-caps.csv:7`). Reporting `{5.1, 6.0}` is a deliberate
/// under-report while `pfnCreate*Shader` is a counting noop: the coupling rules
/// run **tier ⇒ shader model**, never the reverse, so a short list constrains
/// nothing except what an application may compile. L6 raises it, in the commit
/// that makes the shader creates real.
///
/// # Safety
/// As [`get_caps`].
unsafe fn shader_models(a: &ddi12::D3D12DDIARG_GETCAPS, data_size: usize) -> Hresult {
    const MODELS: [ddi12::D3D12DDI_SHADER_MODEL; 2] = [v::SM_5_1, v::SM_6_0];

    let needed = core::mem::size_of::<ddi12::D3D12DDI_D3D12_SHADER_MODELS_DATA_0011>();
    if data_size < needed {
        note_refusal(&UMD12_REFUSALS.caps_data_size_too_small);
        return E_INVALIDARG;
    }
    // SAFETY: `pData` is non-null with at least `needed` readable bytes; the two
    // members are the caller's own pointers and are read, never overwritten.
    let slots = unsafe {
        core::ptr::read_unaligned(a.pData.cast::<ddi12::D3D12DDI_D3D12_SHADER_MODELS_DATA_0011>())
    };
    if slots.pNumShaderModelsSupported.is_null() {
        note_refusal(&UMD12_REFUSALS.caps_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: non-null per the check; the DDI declares it as the count slot and
    // the runtime owns one `UINT` there.
    let capacity = unsafe { core::ptr::read_unaligned(slots.pNumShaderModelsSupported) } as usize;

    if slots.pShaderModelsSupported.is_null() {
        // The count query. Report how many we have and write no models.
        log_error!("GetCaps SHADER_MODELS: count query -> {}", MODELS.len());
        // SAFETY: the count slot is non-null per the check above.
        unsafe {
            core::ptr::write_unaligned(
                slots.pNumShaderModelsSupported,
                MODELS.len() as ddi12::UINT,
            )
        };
        return S_OK;
    }

    let written = capacity.min(MODELS.len());
    if written < MODELS.len() {
        // ⚠ Counted rather than refused: a short buffer is the caller's choice,
        // and the count written back tells it the truth. A truncated list that
        // still contains 5.1 is legal; one that does not is the failure this
        // counter exists to make visible.
        note_refusal(&UMD12_REFUSALS.caps_shader_models_truncated);
    }
    for (index, model) in MODELS.iter().take(written).enumerate() {
        // SAFETY: the caller advertised `capacity` entries and `written <=
        // capacity`, so every index is inside the caller's array.
        unsafe { core::ptr::write_unaligned(slots.pShaderModelsSupported.add(index), *model) };
    }
    // SAFETY: the count slot is non-null per the check above.
    unsafe {
        core::ptr::write_unaligned(slots.pNumShaderModelsSupported, written as ddi12::UINT)
    };
    log_error!("GetCaps SHADER_MODELS: capacity={capacity} -> wrote {written}");
    S_OK
}

/// `1002 MEMORY_ARCHITECTURE`. ⛔ **The zero-fill default violates this cap by
/// construction**: `IOCoherent` **must be TRUE** on any x86/amd64 system
/// (strings:89), unconditionally.
///
/// `UMA = FALSE` matches the KMD's discrete-style topology — a device-local
/// segment plus a `SupportsCpuHostAperture` segment — and the measured baseline.
/// With `UMA = FALSE`, strings:88 then **forces** `CacheCoherent = FALSE`.
///
/// ⚠ `pInfo` is a `NodeIndex` and may be NULL. Helios has one node, so the
/// answer does not depend on it and it is never dereferenced.
///
/// # Safety
/// As [`get_caps`].
unsafe fn memory_architecture(a: &ddi12::D3D12DDIARG_GETCAPS, data_size: usize) -> Hresult {
    let caps = ddi12::D3D12DDI_MEMORY_ARCHITECTURE_CAPS_0041 {
        UMA: 0,
        IOCoherent: 1,
        CacheCoherent: 0,
        HeapSerializationTier: v::HEAP_SERIALIZATION_0,
        ResourceSerializationTier: v::RESOURCE_SERIALIZATION_0,
    };
    // SAFETY: as [`get_caps`].
    unsafe { write_caps("MEMORY_ARCHITECTURE", a.pData, data_size, caps) }
}

/// `1009 GPUVA_CAPS`. ⛔ Must be **non-zero**: *"Driver set
/// MaxGPUVirtualAddressBitsPerResource to 0."* — strings:94.
///
/// ⭐ 40 bits, and the number is not a guess from either side: vkd3d hardcodes
/// 40 with an `/* XXX */`, and `kmd_render` independently declares a 40-bit GPU
/// virtual address width. Two unrelated sources agreeing is why this one is
/// pinned rather than forwarded.
///
/// # Safety
/// As [`get_caps`].
unsafe fn gpuva_caps(a: &ddi12::D3D12DDIARG_GETCAPS, data_size: usize) -> Hresult {
    const HELIOS_GPU_VA_BITS: ddi12::UINT = 40;
    let caps = ddi12::D3D12DDI_GPUVA_CAPS_0004 {
        MaxGPUVirtualAddressBitsPerResource: HELIOS_GPU_VA_BITS,
    };
    // SAFETY: as [`get_caps`].
    unsafe { write_caps("GPUVA_CAPS", a.pData, data_size, caps) }
}

/// `1067 _0050_HARDWARE_SCHEDULING_CAPS` — ⛔ **0, and it is a `D12-G7` pass
/// criterion, not a preference.**
///
/// *"0 means don't use scheduling groups"*. A D3D12 device must never reach
/// `DxgkDdiCreateHwQueue`, which the Helios KMD refuses with
/// `STATUS_NOT_SUPPORTED` while recording `HwQRef`
/// (`kmd_render/src/ddi/scheduler.rs:180-187`); a non-zero answer lands on the
/// VidSch `0x119` / Arg1=2 **bugcheck**. `GATES.md` §4.8 reads `HwQRef` not
/// moving as the evidence that it did not.
///
/// # Safety
/// As [`get_caps`].
unsafe fn hardware_scheduling(a: &ddi12::D3D12DDIARG_GETCAPS, data_size: usize) -> Hresult {
    let caps = ddi12::D3D12DDICAPS_HARDWARE_SCHEDULING_CAPS_0050 {
        ComputeQueuesPer3DQueue: 0,
    };
    // SAFETY: as [`get_caps`].
    unsafe { write_caps("HARDWARE_SCHEDULING_CAPS", a.pData, data_size, caps) }
}

/// `1059 _0022_CPU_PAGE_TABLE_FALSE_POSITIVES` — a `D3D12DDI_COMMAND_QUEUE_FLAGS`
/// bitmask, and **only legal bits may be set** (*"Driver responded with invalid
/// bits …"* — strings:77). `NONE` is legal and says no queue type produces CPU
/// page-table false positives.
///
/// ⚠ `pInfo` is a `NodeIndex`; one node, so it is not read.
///
/// # Safety
/// As [`get_caps`].
unsafe fn cpu_page_table_false_positives(
    a: &ddi12::D3D12DDIARG_GETCAPS,
    data_size: usize,
) -> Hresult {
    // SAFETY: as [`get_caps`].
    unsafe {
        write_caps(
            "CPU_PAGE_TABLE_FALSE_POSITIVES",
            a.pData,
            data_size,
            v::QUEUE_FLAG_NONE,
        )
    }
}

/// `1000 TEXTURE_LAYOUT` / `1010 TEXTURE_LAYOUT1` — deprecated, and both are in
/// the must-answer table with a *"device creation fails"* string, so the arms
/// exist even though `D12-G5` measured that this runtime never asks them.
///
/// ⚠ `pInfo` is documented NULL for both; it is not dereferenced.
/// `Supports64KStandardSwizzle = FALSE` for the same reason as
/// `Deterministic64KBUndefinedSwizzle` in [`d3d12_options`].
///
/// # Safety
/// As [`get_caps`].
unsafe fn texture_layout_deprecated(
    a: &ddi12::D3D12DDIARG_GETCAPS,
    data_size: usize,
) -> Hresult {
    let caps = ddi12::D3D12DDI_TEXTURE_LAYOUT_CAPS {
        DeviceDependentLayoutCount: 0,
        DeviceDependentSwizzleCount: 0,
        Supports64KStandardSwizzle: 0,
    };
    // SAFETY: as [`get_caps`].
    unsafe { write_caps("TEXTURE_LAYOUT(deprecated)", a.pData, data_size, caps) }
}

/// `1060 _0022_TEXTURE_LAYOUT` — the live form. ⚠ **NULL `pInfo` is legal** and
/// is one of the two shapes with its own failure string, so it is never
/// dereferenced here.
///
/// `SupportsRowMajorTexture = FALSE`: TRUE obliges the **KMD** to set
/// `DXGK_VIDMMCAPS::CrossAdapterResourceTexture`, and `kmd_render` does not.
/// Reporting a capability that another component must back is the same class of
/// lie as reporting one the hardware must back.
///
/// # Safety
/// As [`get_caps`].
unsafe fn texture_layout_0022(a: &ddi12::D3D12DDIARG_GETCAPS, data_size: usize) -> Hresult {
    let caps = ddi12::D3D12DDI_TEXTURE_LAYOUT_CAPS_0026 {
        DeviceDependentLayoutCount: 0,
        DeviceDependentSwizzleCount: 0,
        Supports64KStandardSwizzle: 0,
        SupportsRowMajorTexture: 0,
        IndexableSwizzlePatterns: 0,
    };
    // SAFETY: as [`get_caps`].
    unsafe { write_caps("0022_TEXTURE_LAYOUT", a.pData, data_size, caps) }
}

/// `1003 TEXTURE_LAYOUT_SETS` — an **enumeration**, and the one answer in this
/// file with a documented A/B.
///
/// ⛔ **The zero-fill default is illegal four times over**: strings:84-86 reject
/// a zero, non-power-of-two or out-of-range value for each alignment, and
/// strings:87 rejects `SubCaps[0].MaxElementSize == 0`.
///
/// `pInfo` is `UINT[2] = { D3D12DDI_TEXTURE_LAYOUT, D3D12DDI_FUNCTIONAL_UNIT }`
/// and the runtime drives it until the driver fails. ⭐ **This mirrors WARP's
/// measured contract** (`baselines/d12-g5-contract.md`): `S_OK` once, then
/// `E_UNEXPECTED` twice — so the set advertised is exactly one entry,
/// `{ROW_MAJOR, COMBINED}`, and the enumeration terminates on the second call
/// rather than on the driver running out of enum values.
///
/// The numbers are D3D12's own row-major invariants:
/// `D3D12_TEXTURE_DATA_PITCH_ALIGNMENT` = 256,
/// `D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT` = 512, and 16 bytes is the largest
/// format element (`R32G32B32A32`). All four fields are `UINT16`.
///
/// # Safety
/// As [`get_caps`].
unsafe fn texture_layout_sets(a: &ddi12::D3D12DDIARG_GETCAPS, data_size: usize) -> Hresult {
    if a.pInfo.is_null() {
        note_refusal(&UMD12_REFUSALS.caps_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: non-null per the check; the header documents `pInfo` here as
    // `UINT[2]`, and only those two words are read.
    let layout = unsafe { core::ptr::read_unaligned(a.pInfo.cast::<ddi12::UINT>()) };
    // SAFETY: as above — the second word of the same two-word input.
    let unit = unsafe { core::ptr::read_unaligned(a.pInfo.cast::<ddi12::UINT>().add(1)) };

    if layout != v::TL_ROW_MAJOR as ddi12::UINT || unit != v::FUNCUNIT_COMBINED as ddi12::UINT {
        // Not a refusal of something we should support: it is how the
        // enumeration ENDS. Counted so "how many sets did we advertise" is a
        // number, and `E_UNEXPECTED`-shaped to match the measured WARP contract.
        UMD12_REFUSALS.caps_texture_layout_set_end.bump();
        log_error!("GetCaps TEXTURE_LAYOUT_SETS: layout={layout} unit={unit} -> end of set");
        return E_INVALIDARG;
    }

    const SUB: ddi12::D3D12DDI_ROW_MAJOR_LAYOUT_SUB_CAPS =
        ddi12::D3D12DDI_ROW_MAJOR_LAYOUT_SUB_CAPS {
            MaxElementSize: 16,
            BaseOffsetAlignment: 512,
            PitchAlignment: 256,
            // ⛔ **256, and it was 512 until the runtime said so in English.**
            // `D12-G7`, 2026-08-06, ETW `Microsoft-Windows-Direct3D12`:
            //
            // > `Driver set D3D12DDI_ROW_MAJOR_LAYOUT_SUB_CAPS::SubCaps::DepthPitchAlignment
            // > either too large, 0, or to a non-pow2 value.`  (strings:85)
            //
            // 512 is non-zero and a power of two, so the complaint was **too
            // large** — and the bound is relative, not absolute: the identical
            // 512 in `BaseOffsetAlignment` above passes (strings:84 never
            // fired). The reason it cannot exceed `PitchAlignment`: a row-major
            // depth pitch is `RowPitch * Height`, and `RowPitch` is only
            // guaranteed aligned to `PitchAlignment` — so demanding 512 is
            // unsatisfiable for any odd height, which the runtime can prove
            // without asking.
            DepthPitchAlignment: 256,
        };
    // The property the string is about, stated to the compiler: neither
    // alignment may be zero or non-power-of-two, and the depth pitch cannot be
    // aligned more strictly than the row pitch it is a multiple of.
    const _: () = assert!(SUB.PitchAlignment != 0 && SUB.PitchAlignment.is_power_of_two());
    const _: () = assert!(SUB.BaseOffsetAlignment != 0 && SUB.BaseOffsetAlignment.is_power_of_two());
    const _: () = assert!(
        SUB.DepthPitchAlignment != 0
            && SUB.DepthPitchAlignment.is_power_of_two()
            && SUB.DepthPitchAlignment <= SUB.PitchAlignment
    );
    const _: () = assert!(SUB.MaxElementSize != 0);
    let caps = ddi12::D3D12DDI_ROW_MAJOR_LAYOUT_CAPS {
        SubCaps: [SUB, SUB],
        Flags: v::ROW_MAJOR_FLAG_NONE,
    };
    // SAFETY: as [`get_caps`].
    unsafe { write_caps("TEXTURE_LAYOUT_SETS", a.pData, data_size, caps) }
}

/// `1066 _0033_ADAPTER_COMPUTE_ONLY` — FALSE. Helios is a render+display
/// adapter; the compute-only profiles are the `1_0_GENERIC` / `1_0_CORE` levels
/// below FL 11_0.
///
/// # Safety
/// As [`get_caps`].
unsafe fn adapter_compute_only(a: &ddi12::D3D12DDIARG_GETCAPS, data_size: usize) -> Hresult {
    // SAFETY: as [`get_caps`].
    unsafe { write_caps("ADAPTER_COMPUTE_ONLY", a.pData, data_size, 0i32) }
}

/// `1088 OPTIONS_0110` — ⛔ **the zero-fill default writes an out-of-range
/// value here.** `D3D12DDI_EXECUTE_INDIRECT_TIER` has no zero enumerator: the
/// only values are `_1_0 = 10` and `_1_1 = 11`. An out-of-range tier is
/// **clamped silently** by the runtime, so a zero-fill would ship a tier nobody
/// chose — CLAUDE.md rule 8 with the loud failure removed.
///
/// `_1_0` is the floor and is what the substrate backs: `VK_EXT_device_generated_commands`
/// is absent from this guest, which is exactly what separates 1_1 from 1_0
/// (`SUBSTRATE.md` S10).
///
/// # Safety
/// As [`get_caps`].
unsafe fn options_0110(a: &ddi12::D3D12DDIARG_GETCAPS, data_size: usize) -> Hresult {
    let options = ddi12::D3D12DDI_OPTIONS_DATA_0110 {
        ExecuteIndirectTier: v::EXECUTE_INDIRECT_1_0,
    };
    // SAFETY: as [`get_caps`].
    unsafe { write_caps("OPTIONS_0110", a.pData, data_size, options) }
}

/// `1082 OPTIONS_0102` — descriptor-heap ceilings, and ⭐ **the answer in this
/// file that was argued down from the right value and then corrected by the
/// runtime saying so out loud.**
///
/// `SUBSTRATE.md` §4.5 states that `MaxSamplerDescriptorHeapSize` must be
/// **>= 4000** at DDI 0102+. L1's first cut overrode that to 2048 on two
/// arguments that both look strong and are both about **the wrong layer**:
///
/// * *"the D3D12 API constant `D3D12_MAX_SHADER_VISIBLE_SAMPLER_HEAP_SIZE` is
///   2048, so 4000 advertises a heap larger than the API permits"* — but this
///   is the **DDI** cap, and the runtime is what clamps it down to the API
///   limit. The two numbers describe different layers and do not compete;
/// * *"the measured baseline reports 2048"* (`baselines/d3d12-caps.csv:85-86`)
///   — that CSV is `tools/d3d12_caps_dump.cpp` reading
///   `D3D12_FEATURE_DATA_D3D12_OPTIONS19` through the **API**. It is the
///   post-clamp value, so it could not have disagreed with 4000 whatever the
///   driver underneath reported.
///
/// ⛔ **The runtime settled it.** `D12-G7`, 2026-08-06, ETW
/// `Microsoft-Windows-Direct3D12`: `Driver's MaxSamplerDescriptorHeapSize is
/// too small` (strings:113) with 2048. §4.5 was right; the "correction" was a
/// layer confusion, and the lesson is that an API-level capture cannot falsify
/// a DDI-level requirement.
///
/// ⚠ 4000 is **exactly** the guest's `maxSamplerAllocationCount` (§4.5), i.e.
/// zero headroom — §4.5 flags that itself, and it is a real substrate ceiling
/// rather than a number chosen for comfort.
/// `MaxSamplerDescriptorHeapSizeWithStaticSamplers` is equal rather than
/// smaller because strings:114 rejects it only for being **larger** than the
/// heap size or too small; reserving a slice for static samplers is a
/// refinement no measurement calls for yet.
///
/// `SupportedSampleCountsWithNoOutputs = 1` (count 1 only) is likewise the
/// measured baseline (`:80`) rather than the spec-prose `0x1D`; raising it needs
/// a probe that sample-frequency PS with zero bound outputs actually works.
///
/// # Safety
/// As [`get_caps`].
unsafe fn options_0102(a: &ddi12::D3D12DDIARG_GETCAPS, data_size: usize) -> Hresult {
    /// `SUBSTRATE.md` §4.5's DDI floor, and the guest's exact
    /// `maxSamplerAllocationCount`. ⛔ NOT `D3D12_MAX_SHADER_VISIBLE_SAMPLER_HEAP_SIZE`
    /// (2048) — that is the API limit the runtime clamps to, one layer up, and
    /// reporting it here is what the runtime rejected with strings:113.
    const MAX_SAMPLER_HEAP: ddi12::UINT = 4000;
    /// The measured baseline's view-heap ceiling.
    const MAX_VIEW_HEAP: ddi12::UINT = 1_000_000;

    let options = ddi12::D3D12DDI_OPTIONS_0102 {
        SupportedSampleCountsWithNoOutputs: 1,
        MaxSamplerDescriptorHeapSize: MAX_SAMPLER_HEAP,
        MaxSamplerDescriptorHeapSizeWithStaticSamplers: MAX_SAMPLER_HEAP,
        MaxViewDescriptorHeapSize: MAX_VIEW_HEAP,
    };
    // SAFETY: as [`get_caps`].
    unsafe { write_caps("OPTIONS_0102", a.pData, data_size, options) }
}

// ---------------------------------------------------------------------------
// The device-core slots — the per-format half of the caps contract
// ---------------------------------------------------------------------------
//
// ⭐ **These three are not a footnote to the caps gauntlet, they are half of
// it.** `tmp/dx12/gates/G7/RESULT.md` counted three noop slots hit **2 824
// times inside `D3D12CreateDevice`**; two of them are here —
// `pfnCheckFormatSupport` **93** (the 91-format sweep `DDI_REFERENCE.md` §11.1
// predicted; the 91 x 30 decomposition is `DDI_REFERENCE.md` §14.0, not the
// gate file) and `pfnCheckMultisampleQualityLevels` **2 730** — for **2 823**.
// ⚠ The remaining 1 is `pfnQueryNodeMap`, which is L9's and lands in
// `forward12/misc.rs`; `pfnGetMipPacking` was never called at all. With
// counting noops these answered *"no format supports anything and no sample
// count has any quality level"*, which is an inconsistent caps set by
// construction and is what `DXGI_ERROR_DRIVER_INTERNAL_ERROR` was reporting.
//
// ⚠ An earlier revision of this file's module doc called them "device-scope and
// not on the path that gates device creation". Measured, and wrong.
//
// # ⭐ Why there is no new C++ here, and why that is not a shortcut
//
// The handoff for this work specified a new `bridge12` cxx module forwarding
// into `ID3D12Device::CheckFeatureSupport` — C++, compilable only on the VM.
// It is not needed: `ID3D12Device` is a **COM interface**, `bridge12` already
// hands Rust a borrowed one through [`crate::bridge12::BridgeDevice12`], and
// the `windows` crate's generated vtable call is the same indirect call the
// C++ would have made. A cxx module would have added a translation unit, a
// `build.rs` edit and a VM-only compile step to reach the identical vtable
// slot.
//
// `PARALLEL.md` §5's *"a lane that needs new engine calls gets its own cxx
// bridge module"* still stands for the calls that genuinely need C++ — the ones
// taking engine types the SDK headers do not describe. `CheckFeatureSupport`
// takes `D3D12_FEATURE` plus a `void*`, so it needs none. ⭐ The consequence is
// that this whole lane type-checks on the Linux host (`PARALLEL.md` §7), which
// the C++ route would have taken away from it.
//
// # ⛔ The trap, and it is the reason this gate failed the same way twice
//
// `DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM` (89) must be refused with the
// explicit `_NOT_SUPPORTED` sentinel `0x8000_0000`, **never a bare 0**. The
// D3D11 runtime rejected a bare 0 there as a malformed caps response and failed
// `D3D11CreateDevice` with `DXGI_ERROR_DRIVER_INTERNAL_ERROR` (`0x887A0020`) —
// the *same* HRESULT, on the same box, from the same class of mistake
// (`umd/src/forward/format_caps.rs:244-262`). The D3D10 DDI's format-support
// bit values are byte-for-byte `D3D12DDI_FORMAT_SUPPORT`'s, sentinel included,
// so the precedent transfers whole.

use windows::Win32::Graphics::Direct3D12::{
    D3D12_FEATURE_DATA_FORMAT_SUPPORT, D3D12_FEATURE_DATA_MULTISAMPLE_QUALITY_LEVELS,
    D3D12_FEATURE_FORMAT_SUPPORT, D3D12_FEATURE_MULTISAMPLE_QUALITY_LEVELS, D3D12_FORMAT_SUPPORT1,
    D3D12_FORMAT_SUPPORT1_BLENDABLE, D3D12_FORMAT_SUPPORT1_BUFFER, D3D12_FORMAT_SUPPORT1_DISPLAY,
    D3D12_FORMAT_SUPPORT1_IA_VERTEX_BUFFER, D3D12_FORMAT_SUPPORT1_MULTISAMPLE_LOAD,
    D3D12_FORMAT_SUPPORT1_DEPTH_STENCIL, D3D12_FORMAT_SUPPORT1_MULTISAMPLE_RENDERTARGET,
    D3D12_FORMAT_SUPPORT1_RENDER_TARGET,
    D3D12_FORMAT_SUPPORT1_SHADER_GATHER, D3D12_FORMAT_SUPPORT1_SHADER_SAMPLE,
    D3D12_FORMAT_SUPPORT2, D3D12_FORMAT_SUPPORT2_UAV_TYPED_LOAD,
    D3D12_FORMAT_SUPPORT2_UAV_TYPED_STORE, D3D12_MULTISAMPLE_QUALITY_LEVELS_FLAG_TILED_RESOURCE,
    D3D12_MULTISAMPLE_QUALITY_LEVEL_FLAGS,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;

use crate::device12::{self, HeliosD3D12Device};

/// Short aliases for the DDI's own format-support bits.
///
/// Same discipline as [`v`]: every line below is a compile-checked reference to
/// the generated header constant, never a transcribed number.
mod fs {
    use crate::ddi12;

    pub(super) const SHADER_SAMPLE: u32 = ddi12::D3D12DDI_FORMAT_SUPPORT_SHADER_SAMPLE;
    pub(super) const RENDERTARGET: u32 = ddi12::D3D12DDI_FORMAT_SUPPORT_RENDERTARGET;
    pub(super) const BLENDABLE: u32 = ddi12::D3D12DDI_FORMAT_SUPPORT_BLENDABLE;
    pub(super) const MULTISAMPLE_RENDERTARGET: u32 =
        ddi12::D3D12DDI_FORMAT_SUPPORT_MULTISAMPLE_RENDERTARGET;
    pub(super) const MULTISAMPLE_LOAD: u32 = ddi12::D3D12DDI_FORMAT_SUPPORT_MULTISAMPLE_LOAD;
    pub(super) const DECODER_OUTPUT: u32 = ddi12::D3D12DDI_FORMAT_SUPPORT_DECODER_OUTPUT;
    pub(super) const VIDEO_PROCESSOR_OUTPUT: u32 =
        ddi12::D3D12DDI_FORMAT_SUPPORT_VIDEO_PROCESSOR_OUTPUT;
    pub(super) const VIDEO_PROCESSOR_INPUT: u32 =
        ddi12::D3D12DDI_FORMAT_SUPPORT_VIDEO_PROCESSOR_INPUT;
    pub(super) const VERTEX_BUFFER: u32 = ddi12::D3D12DDI_FORMAT_SUPPORT_VERTEX_BUFFER;
    pub(super) const UAV_WRITES: u32 = ddi12::D3D12DDI_FORMAT_SUPPORT_UAV_WRITES;
    pub(super) const BUFFER: u32 = ddi12::D3D12DDI_FORMAT_SUPPORT_BUFFER;
    pub(super) const CAPTURE: u32 = ddi12::D3D12DDI_FORMAT_SUPPORT_CAPTURE;
    pub(super) const VIDEO_ENCODER: u32 = ddi12::D3D12DDI_FORMAT_SUPPORT_VIDEO_ENCODER;
    pub(super) const OUTPUT_MERGER_LOGIC_OP: u32 =
        ddi12::D3D12DDI_FORMAT_SUPPORT_OUTPUT_MERGER_LOGIC_OP;
    pub(super) const SHADER_GATHER: u32 = ddi12::D3D12DDI_FORMAT_SUPPORT_SHADER_GATHER;
    pub(super) const MULTIPLANE_OVERLAY: u32 = ddi12::D3D12DDI_FORMAT_SUPPORT_MULTIPLANE_OVERLAY;
    pub(super) const TILED: u32 = ddi12::D3D12DDI_FORMAT_SUPPORT_TILED;
    pub(super) const UAV_READS: u32 = ddi12::D3D12DDI_FORMAT_SUPPORT_UAV_READS;
    pub(super) const DISPLAY: u32 = ddi12::D3D12DDI_FORMAT_SUPPORT_DISPLAY;
    /// ⛔ `0x8000_0000`, *"Set only this bit"*. See the section header.
    pub(super) const NOT_SUPPORTED: u32 = ddi12::D3D12DDI_FORMAT_SUPPORT_NOT_SUPPORTED;
}

/// The engine's `D3D12_FORMAT_SUPPORT1` bit -> this DDI's bit, for every DDI bit
/// derived from the engine's answer.
///
/// ⚠ The two enums are **not** the same numbering — the API's `SHADER_SAMPLE` is
/// `0x200` and the DDI's is `0x1` — so this is a translation and not a
/// pass-through. `umd/src/forward/format_caps.rs` passes its value through
/// unchanged, and that is correct *there* for a reason that does not transfer:
/// the D3D11 DDI harmonised with the D3D11 API enum. The D3D12 DDI did not; it
/// defines its own 20-bit `D3D12DDI_FORMAT_SUPPORT` right beside
/// `pfnCheckFormatSupport`, and those are the bits the runtime reads.
///
/// ⚠ **The API bits with NO DDI counterpart, listed so their absence is a
/// decision rather than an omission.** The DDI enum carries only *usage*
/// capabilities; it has no way to say "this is a 2D texture format" or "this is
/// a depth format", so these are dropped: `TEXTURE1D/2D/3D`, `TEXTURECUBE`,
/// `MIP`, `SHADER_LOAD`, `SHADER_SAMPLE_COMPARISON`, `SHADER_SAMPLE_MONO_TEXT`,
/// `SHADER_GATHER_COMPARISON`, `CAST_WITHIN_BIT_LAYOUT`, `BACK_BUFFER_CAST`,
/// `IA_INDEX_BUFFER`, `SO_BUFFER`, `MULTISAMPLE_RESOLVE`,
/// `TYPED_UNORDERED_ACCESS_VIEW` and — the one worth naming twice —
/// **`DEPTH_STENCIL`**. ⛔ It is deliberately NOT folded into
/// `D3D12DDI_FORMAT_SUPPORT_RENDERTARGET`: in this DDI that bit means "usable
/// as a render target", and a depth format is not, so folding it would be an
/// over-claim to fix a cosmetic gap. The consequence is visible and expected —
/// `D32_FLOAT_S8X24_UINT` answers `MULTISAMPLE_RENDERTARGET` alone — and the
/// runtime already knows which formats are depth formats without asking.
const SUPPORT1_TO_DDI: &[(u32, u32)] = &[
    (D3D12_FORMAT_SUPPORT1_SHADER_SAMPLE.0 as u32, fs::SHADER_SAMPLE),
    (D3D12_FORMAT_SUPPORT1_RENDER_TARGET.0 as u32, fs::RENDERTARGET),
    (D3D12_FORMAT_SUPPORT1_BLENDABLE.0 as u32, fs::BLENDABLE),
    (
        D3D12_FORMAT_SUPPORT1_MULTISAMPLE_RENDERTARGET.0 as u32,
        fs::MULTISAMPLE_RENDERTARGET,
    ),
    (
        D3D12_FORMAT_SUPPORT1_MULTISAMPLE_LOAD.0 as u32,
        fs::MULTISAMPLE_LOAD,
    ),
    (
        D3D12_FORMAT_SUPPORT1_IA_VERTEX_BUFFER.0 as u32,
        fs::VERTEX_BUFFER,
    ),
    (D3D12_FORMAT_SUPPORT1_BUFFER.0 as u32, fs::BUFFER),
    (D3D12_FORMAT_SUPPORT1_SHADER_GATHER.0 as u32, fs::SHADER_GATHER),
    // ⚠ Scan-out capability, and the one bit here that another Helios component
    // has to back. The KMD owns a real VidPn source and sends DWM's shared
    // primary through `SET_SCANOUT_BLOB`, so the capability exists; the engine's
    // list of displayable formats is the narrower of the two and is what is
    // reported. L8 (present) is the lane that would have to widen it.
    (D3D12_FORMAT_SUPPORT1_DISPLAY.0 as u32, fs::DISPLAY),
];

/// The engine's `D3D12_FORMAT_SUPPORT2` bit -> this DDI's bit.
const SUPPORT2_TO_DDI: &[(u32, u32)] = &[
    (D3D12_FORMAT_SUPPORT2_UAV_TYPED_STORE.0 as u32, fs::UAV_WRITES),
    // ⚠ Additionally narrowed by [`FL11_TYPED_UAV_LOAD_FORMATS`]; see
    // [`TYPED_UAV_LOAD_ADDITIONAL_FORMATS`].
    (D3D12_FORMAT_SUPPORT2_UAV_TYPED_LOAD.0 as u32, fs::UAV_READS),
];

/// Every DDI bit that is decided by the engine's answer.
const ENGINE_DERIVED_BITS: u32 = or_ddi_bits(SUPPORT1_TO_DDI) | or_ddi_bits(SUPPORT2_TO_DDI);

/// Every DDI bit this driver withholds from **every** format, with the cap or
/// the missing subsystem each one is withheld for.
///
/// * the four video bits (`DECODER_OUTPUT`, `VIDEO_PROCESSOR_OUTPUT`,
///   `VIDEO_PROCESSOR_INPUT`, `VIDEO_ENCODER`) and `CAPTURE` — Helios implements
///   no video DDI in either driver, so a format cannot be a decoder output or a
///   capture target however capable the underlying Vulkan format is. The D3D11
///   driver scrubs the same family for the same reason
///   (`umd/src/forward/format_caps.rs`'s `VIDEO_BITS`).
/// * `OUTPUT_MERGER_LOGIC_OP` — [`OUTPUT_MERGER_LOGIC_OP`] is FALSE.
/// * `MULTIPLANE_OVERLAY` — there is no overlay path: `pfnGetOptionalDDITables`
///   answers zero tables and `D12-G5` measured that this runtime never requests
///   `D3D12DDI_TABLE_TYPE_DXGI` at all.
/// * `TILED` — [`TILED_RESOURCES_TIER_REPORTED`] is `NOT_SUPPORTED`, so no
///   tiled resource can exist and no format can be usable in one.
///   ⚠ **No runtime string is known to enforce this**, and an earlier revision
///   of this comment wrongly cited strings:48 for it: that string is a *range*
///   check on the value of `D3D12DDI_D3D12_OPTIONS_DATA::TiledResourcesTier`,
///   which `NOT_SUPPORTED` passes, in a different DDI call. The bit is withheld
///   because `DECISIONS.md` §7.8 says an unbacked capability is a lie the OS
///   acts on — not because a check was found that would catch it.
const WITHHELD_BITS: u32 = fs::DECODER_OUTPUT
    | fs::VIDEO_PROCESSOR_OUTPUT
    | fs::VIDEO_PROCESSOR_INPUT
    | fs::VIDEO_ENCODER
    | fs::CAPTURE
    | fs::OUTPUT_MERGER_LOGIC_OP
    | fs::MULTIPLANE_OVERLAY
    | fs::TILED;

/// Every `D3D12DDI_FORMAT_SUPPORT_*` bit this build's header defines, except the
/// `NOT_SUPPORTED` sentinel — which is not a capability and is never combined
/// with one.
const ALL_CAPABILITY_BITS: u32 = fs::SHADER_SAMPLE
    | fs::RENDERTARGET
    | fs::BLENDABLE
    | fs::MULTISAMPLE_RENDERTARGET
    | fs::MULTISAMPLE_LOAD
    | fs::DECODER_OUTPUT
    | fs::VIDEO_PROCESSOR_OUTPUT
    | fs::VIDEO_PROCESSOR_INPUT
    | fs::VERTEX_BUFFER
    | fs::UAV_WRITES
    | fs::BUFFER
    | fs::CAPTURE
    | fs::VIDEO_ENCODER
    | fs::OUTPUT_MERGER_LOGIC_OP
    | fs::SHADER_GATHER
    | fs::MULTIPLANE_OVERLAY
    | fs::TILED
    | fs::UAV_READS
    | fs::DISPLAY;

// ⭐ **THE PARTITION PROOF, and it is the deliverable of this section.** Every
// capability bit the header defines is either derived from the engine or
// explicitly withheld with a reason — never neither, and never both. A bit
// nobody decided about is exactly how a driver ships a capability it cannot
// back (`DECISIONS.md` §7.8), and a bit decided twice is how the two decisions
// diverge. Same idea as `forward12::noop12`'s ABI-order proof: state the
// invariant to the compiler rather than to the reader.
//
// ⚠ **What it does NOT catch, stated because the obvious reading is wrong.** All
// three operands are hand-written lists in this file, so a WDK bump that adds a
// `D3D12DDI_FORMAT_SUPPORT_*` constant is referenced by none of them and the
// build stays green with the new bit silently reported as unsupported forever.
// bindgen emits the constant, `ddi12`'s crate-level `#![allow(dead_code)]`
// suppresses any lint on it, and nothing here notices. The assertions prove a
// property of the DECISIONS this file makes, not of the header it makes them
// about — which is still worth having, and is not the same claim.
const _: () = assert!(
    ENGINE_DERIVED_BITS | WITHHELD_BITS == ALL_CAPABILITY_BITS,
    "every D3D12DDI_FORMAT_SUPPORT bit must be either engine-derived or explicitly withheld"
);
const _: () = assert!(
    ENGINE_DERIVED_BITS & WITHHELD_BITS == 0,
    "a D3D12DDI_FORMAT_SUPPORT bit cannot be both engine-derived and withheld"
);
const _: () = assert!(
    ALL_CAPABILITY_BITS & fs::NOT_SUPPORTED == 0,
    "the NOT_SUPPORTED sentinel is not a capability bit"
);

// ⛔ The DDI's multisample flag and the API's must be the same value, because
// the flag is forwarded to the engine unchanged. They are separate enums in
// separate headers and nothing but this line couples them.
const _: () = assert!(
    ddi12::D3D12DDI_MULTISAMPLE_QUALITY_LEVEL_FLAGS_D3D12DDI_MULTISAMPLE_QUALITY_LEVEL_FLAG_TILED_RESOURCE
        == D3D12_MULTISAMPLE_QUALITY_LEVELS_FLAG_TILED_RESOURCE.0,
    "D3D12DDI_MULTISAMPLE_QUALITY_LEVEL_FLAG_TILED_RESOURCE must equal the API flag it forwards to"
);

/// OR together the DDI-side bit of every pair in a translation table.
const fn or_ddi_bits(pairs: &[(u32, u32)]) -> u32 {
    let mut acc = 0u32;
    let mut i = 0;
    while i < pairs.len() {
        acc |= pairs[i].1;
        i += 1;
    }
    acc
}

/// Map a source bitmask through a translation table.
fn translate(pairs: &[(u32, u32)], src: u32) -> u32 {
    let mut out = 0u32;
    for &(from, to) in pairs {
        if src & from != 0 {
            out |= to;
        }
    }
    out
}

/// Ask the engine what it supports for one format. `None` when there is no
/// engine to ask or it refused; both are counted.
fn engine_format_support(dev: &HeliosD3D12Device, format: ddi12::DXGI_FORMAT) -> Option<(u32, u32)> {
    let Some(engine) = dev.engine.d3d12_device() else {
        // Unreachable by construction — `helios_vkd3d_bridge_create_device`
        // returns a null `unique_ptr` rather than an empty one on every failure
        // path, and `BridgeDevice12::create` folds null into `None` — so a live
        // `HeliosD3D12Device` always carries a device. Counted anyway, because
        // "unreachable by construction" is a claim about a cross-FFI contract
        // and this is the only place it could be observed breaking.
        note_refusal(&UMD12_REFUSALS.caps_slot_no_device);
        return None;
    };
    let mut data = D3D12_FEATURE_DATA_FORMAT_SUPPORT {
        Format: DXGI_FORMAT(format),
        Support1: D3D12_FORMAT_SUPPORT1(0),
        Support2: D3D12_FORMAT_SUPPORT2(0),
    };
    // SAFETY: `feature` and the buffer agree by construction — `data` is a live
    // local of exactly the struct `D3D12_FEATURE_FORMAT_SUPPORT` names, and the
    // size passed is its own `size_of`, so the engine cannot write outside it.
    // `engine` is the bridge's BORROWED `ID3D12Device` in a `ManuallyDrop`; the
    // call issues no reference-count change and the wrapper is never released.
    let asked = unsafe {
        engine.CheckFeatureSupport(
            D3D12_FEATURE_FORMAT_SUPPORT,
            core::ptr::from_mut(&mut data).cast::<core::ffi::c_void>(),
            core::mem::size_of::<D3D12_FEATURE_DATA_FORMAT_SUPPORT>() as u32,
        )
    };
    if let Err(err) = asked {
        UMD12_REFUSALS.caps_format_support_engine_failed.bump();
        let n = UMD12_REFUSALS.caps_format_support_engine_failed.get();
        if n <= LOG_BUDGET {
            log_error!(
                "CheckFormatSupport fmt={format}: engine refused hr={:#010x} -> answering \
                 unsupported (x{n})",
                err.code().0 as u32,
            );
        }
        return None;
    }
    Some((data.Support1.0 as u32, data.Support2.0 as u32))
}

/// What **this driver** reports for one format: the engine's answer, translated
/// into DDI bits, narrowed to what the caps at the top of this file claim.
///
/// ⚠ **Every typeless format answers 0 here**, and that is the engine's own
/// position rather than an artefact of the masking: `d3d12_device_get_format_support`
/// skips its whole rendering-and-shader block for non-planar typeless formats
/// (`device.c:5300`), and the bits it does still set for them — `TEXTURE1D/2D/3D`,
/// `TEXTURECUBE`, `MIP`, `CAST_WITHIN_BIT_LAYOUT` — have **no counterpart in
/// `D3D12DDI_FORMAT_SUPPORT` at all**. The DDI enum carries only usage
/// capabilities, and a typeless format is not directly usable. See
/// `check_multisample_quality_levels` for why that must not be turned into a
/// cross-check against the multisample answer.
fn driver_format_support(dev: &HeliosD3D12Device, format: ddi12::DXGI_FORMAT) -> u32 {
    let (support1, support2) = engine_format_support(dev, format).unwrap_or((0, 0));

    // ⭐ **THE ENCODING, AND IT IS AN A/B BECAUSE IT IS NOT SETTLED.** See
    // [`crate::knobs12::UMD12_FORMAT_CAPS`] for the full argument: the header
    // defines a small `D3D12DDI_FORMAT_SUPPORT` enum beside this DDI, but the
    // D3D11 side of this project measured that its own DDI is *harmonized with
    // the API enum* and that translating regresses device creation
    // (`umd/src/forward/format_caps.rs:15-19`). If D3D12 inherited that, the
    // translation below is the same mistake one generation later.
    //
    // ⚠ Arm 1 passes the engine's `D3D12_FORMAT_SUPPORT1` through unchanged,
    // which is exactly what the D3D11 driver does with DXVK's value. It applies
    // none of the masking below, deliberately: the point of the arm is to test
    // the *encoding*, and mixing in a policy difference would make a green run
    // unattributable.
    if crate::knobs12::umd12_format_caps() == FORMAT_CAPS_API_PASSTHROUGH {
        let n = UMD12_REFUSALS.caps_format_support_calls.get();
        if n <= FORMAT_SUPPORT_LOG_BUDGET {
            log_error!(
                "CheckFormatSupport fmt={format} s1={support1:#010x} s2={support2:#010x} -> \
                 {support1:#010x} (API PASSTHROUGH, Umd12FormatCaps=1) (x{n})"
            );
        }
        return support1;
    }

    let mut caps = translate(SUPPORT1_TO_DDI, support1) | translate(SUPPORT2_TO_DDI, support2);
    caps &= !WITHHELD_BITS;

    // ⛔ Typed UAV *loads*, narrowed rather than dropped. See
    // [`FL11_TYPED_UAV_LOAD_FORMATS`]: the cap is named *additional* formats, so
    // FALSE means "the three FL 11_0 mandates and no others", and both the
    // all-off and the all-on answer contradict it.
    if TYPED_UAV_LOAD_ADDITIONAL_FORMATS == 0 && !FL11_TYPED_UAV_LOAD_FORMATS.contains(&format) {
        caps &= !fs::UAV_READS;
    }

    // ── The multisample answer: ONE predicate, both slots ──────────────────
    //
    // ⭐ **This is transliterated from the D3D11 driver, and the structure is
    // the hard-won part.** `umd/src/forward/queries.rs:104-164` does not forward
    // the engine's quality-level answer at all -- it *derives* it from the same
    // `dxgi_msaa_bits_per_sample(fmt, caps).is_some()` predicate that decides
    // the format-support multisample bits, and says why in as many words:
    //
    // > "The Microsoft runtime validates `CheckFormatSupport` and
    // > `CheckMultisampleQualityLevels` as a coherent feature-level contract
    // > during `CDevice::LLOCompleteLayerConstruction`. ... the caps/quality
    // > pair stays internally coherent either way because `check_format_support`
    // > uses the SAME predicate."
    //
    // ⛔ Forwarding two independent engine queries -- which is what this file did
    // first -- makes the pair a *coincidence*, and `D12-G7` measured the runtime
    // rejecting it four different ways on one format. Deriving both from
    // [`msaa_capable`] makes disagreement unrepresentable, which is the same
    // move `forward12::noop12` makes for slot ordinals.
    let msaa = msaa_capable(dev, format, support1);
    if msaa {
        caps |= fs::MULTISAMPLE_RENDERTARGET;
        // ⚠ `LOAD` only for non-depth formats, exactly as
        // `format_caps.rs:135-138` gates it (`if caps & DEPTH_STENCIL == 0`).
        // It is *"can be used as source for 'ld2dms'"*, and it is also the bit
        // whose absence is why `D32_FLOAT_S8X24_UINT` (20) is accepted carrying
        // `MULTISAMPLE_RENDERTARGET` **alone** -- measured.
        if support1 & API_DEPTH_STENCIL == 0 {
            caps |= fs::MULTISAMPLE_LOAD;
        }
    } else if caps & (fs::MULTISAMPLE_RENDERTARGET | fs::MULTISAMPLE_LOAD) != 0 {
        UMD12_REFUSALS.caps_msaa_bits_dropped.bump();
        let n = UMD12_REFUSALS.caps_msaa_bits_dropped.get();
        if n <= LOG_BUDGET {
            log_error!(
                "CheckFormatSupport fmt={format}: engine claims multisample bits ({:#010x}) but \
                 this format is not MSAA-capable -> dropping them (x{n})",
                caps & (fs::MULTISAMPLE_RENDERTARGET | fs::MULTISAMPLE_LOAD),
            );
        }
        caps &= !(fs::MULTISAMPLE_RENDERTARGET | fs::MULTISAMPLE_LOAD);
    }

    // ⭐ The evidence line, and it carries the ENGINE'S RAW ANSWER as well as
    // this driver's. The first `D12-G7` run with these slots live could not be
    // diagnosed from the log — it took an ETW `Microsoft-Windows-Direct3D12`
    // capture to learn which format and which rule — because the log recorded
    // only the final DDI value, and the whole question was how it was derived.
    // `s1`/`s2` are the API-level `D3D12_FORMAT_SUPPORT1`/`2` the engine gave.
    let n = UMD12_REFUSALS.caps_format_support_calls.get();
    if n <= FORMAT_SUPPORT_LOG_BUDGET {
        log_error!(
            "CheckFormatSupport fmt={format} s1={support1:#010x} s2={support2:#010x} -> \
             {caps:#010x} (x{n})"
        );
    }

    if caps != 0 {
        return caps;
    }

    // ⛔ THE SENTINEL. A format with no capability at all is reported with a
    // bare 0 — except for the one the runtime validates specially, where a bare
    // 0 is a malformed response and `0x8000_0000` set alone is the required
    // answer. The header's own words are *"Currently only valid for
    // DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM. (Set only this bit)"*, which is
    // why this is not applied to every unsupported format: the sentinel is not a
    // generic "no" and setting it where it is not valid is the same class of
    // malformed answer in the other direction.
    if format == DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM {
        UMD12_REFUSALS.caps_format_not_supported_sentinel.bump();
        let n = UMD12_REFUSALS.caps_format_not_supported_sentinel.get();
        if n <= LOG_BUDGET {
            log_error!(
                "CheckFormatSupport fmt={format} (R10G10B10_XR_BIAS_A2_UNORM): unsupported -> \
                 NOT_SUPPORTED sentinel {:#010x}, never a bare 0 (x{n})",
                fs::NOT_SUPPORTED,
            );
        }
        return fs::NOT_SUPPORTED;
    }
    0
}

/// `D3D12_FORMAT_SUPPORT1_RENDER_TARGET`, needed as a raw value because the
/// output test below is asked of the **engine's API bits**, not of the DDI
/// answer. The DDI enum has no depth bit at all, so the DDI value cannot
/// distinguish "render target" from "depth target" — and that distinction is
/// exactly what decides `MULTISAMPLE_LOAD`.
const API_RENDER_TARGET: u32 = 0x0000_4000;
/// `D3D12_FORMAT_SUPPORT1_DEPTH_STENCIL`. See [`API_RENDER_TARGET`].
const API_DEPTH_STENCIL: u32 = 0x0001_0000;

const _: () = assert!(API_RENDER_TARGET == D3D12_FORMAT_SUPPORT1_RENDER_TARGET.0 as u32);
const _: () = assert!(API_DEPTH_STENCIL == D3D12_FORMAT_SUPPORT1_DEPTH_STENCIL.0 as u32);

/// ⭐ **THE multisample predicate — one function, both slots.**
///
/// A transliteration of the D3D11 driver's
/// `dxgi_msaa_bits_per_sample(fmt, caps).is_some()`
/// (`umd/src/forward.rs:197-214`), which is what
/// `umd/src/forward/format_caps.rs` and
/// `umd/src/forward/queries.rs::helios_multisample_quality_levels` **both**
/// consult so their answers cannot drift apart. `D12-G7` measured what happens
/// when they can.
///
/// Three terms, and each one is load-bearing:
///
/// 1. ⛔ **`format::msaa_ineligible`** — true for exactly `R32_FLOAT_X8X24_TYPELESS`
///    (21), `X32_TYPELESS_G8X24_UINT` (22), `R24_UNORM_X8_TYPELESS` (46) and
///    `X24_TYPELESS_G8_UINT` (47). Its own field doc in
///    `umd_common/src/format.rs` is the whole answer to this gate's blocker:
///    *"Depth-resource read/view formats are format-support siblings of the
///    MSAA-capable typeless/depth formats, but WARP reports zero quality levels
///    above 1x and the runtime rejects advertising them as MSAA render
///    targets."* Format **21 is where `D12-G7`'s sweep stops**, and every arm
///    tried before this one changed the format bits while still forwarding the
///    engine's non-zero quality levels — so the one answer that works, *neither
///    MSAA bits nor levels*, was never on the table.
/// 2. **the output test**, asked of the engine's API bits: a format carrying
///    `RENDER_TARGET` or `DEPTH_STENCIL` is sized by `bits_per_sample`, and one
///    carrying neither by its `output_family_bits`. Absent ⇒ not MSAA-capable,
///    which is how compressed and video-only formats stay out (`format.rs:38-41`:
///    *"deliberately absent so the runtime does not require MSAA for them"*).
/// 3. ⚠ **the engine term, which D3D11 does NOT have.** R829 records that the
///    D3D11 predicate *"never asks whether that SAMPLE COUNT is supported, so
///    today's '8x on a 128-bit format' is a table assertion, not a capability
///    probe"*. Here the probe is free — `supported_sample_counts` is computed
///    once per format at device init and this is a mask test — so the claim is
///    narrowed to what vkd3d can actually back. It is what keeps
///    `R32G32B32_FLOAT` (6) answering no MSAA bits, which is the value the
///    runtime was measured accepting.
fn msaa_capable(dev: &HeliosD3D12Device, format: ddi12::DXGI_FORMAT, support1: u32) -> bool {
    if helios_umd_common::format::msaa_ineligible(format as u32) {
        UMD12_REFUSALS.caps_msaa_ineligible_format.bump();
        return false;
    }
    let output_bits = if support1 & (API_RENDER_TARGET | API_DEPTH_STENCIL) != 0 {
        helios_umd_common::format::bits_per_sample(format as u32)
    } else {
        helios_umd_common::format::output_family_bits(format as u32)
    };
    output_bits.is_some() && engine_offers_any_msaa(dev, format)
}

/// Does the engine offer a quality level at ANY sample count above 1?
///
/// The five counts are every value `vk_samples_from_sample_count` can map
/// (`VK_SAMPLE_COUNT_2/4/8/16/32_BIT`), so a `false` here means no
/// `pfnCheckMultisampleQualityLevels` answer this driver can give is non-zero.
///
/// ⚠ Not a host round trip per call: `supported_sample_counts` is computed once
/// per format at device init (`vkd3d_init_format_sample_counts`) and this is a
/// mask test against it. Short-circuits on the first hit, so the common case is
/// one call.
fn engine_offers_any_msaa(dev: &HeliosD3D12Device, format: ddi12::DXGI_FORMAT) -> bool {
    const PROBE_COUNTS: [ddi12::UINT; 5] = [2, 4, 8, 16, 32];
    let none = ddi12::D3D12DDI_MULTISAMPLE_QUALITY_LEVEL_FLAGS_D3D12DDI_MULTISAMPLE_QUALITY_LEVEL_FLAG_NONE;
    PROBE_COUNTS
        .iter()
        .any(|&count| engine_msaa_quality_levels(dev, format, count, none).unwrap_or(0) != 0)
}

/// The one format the WDDM runtime validates specially during device creation.
/// ⛔ See [`driver_format_support`]; `umd/src/forward/format_caps.rs:258` is the
/// D3D11 site that learned it the hard way.
const DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM: ddi12::DXGI_FORMAT = 89;

/// `pfnCheckFormatSupport` — 93 calls inside one `D3D12CreateDevice`.
///
/// Returns `VOID`. ⚠ `DECISIONS.md` §7 item 6 notes that a `VOID` D3D12 DDI
/// reports errors through `pfnSetErrorCb`, and this slot deliberately does not
/// use it: *"this format supports nothing"* is a legitimate answer to a
/// capability query, not a device error, and raising one would remove the device
/// over a format the application never asked for. The channel here is the zeroed
/// out-parameter plus the counters.
///
/// # Safety
/// `h_device` must be a live handle from `device12::create_device`, and `out`
/// must address one writable `UINT` the runtime owns.
unsafe extern "C" fn check_format_support(
    h_device: ddi12::D3D12DDI_HDEVICE,
    format: ddi12::DXGI_FORMAT,
    out: *mut ddi12::UINT,
) {
    UMD12_REFUSALS.caps_format_support_calls.bump();
    if out.is_null() {
        note_refusal(&UMD12_REFUSALS.caps_slot_bad_arg);
        return;
    }
    // ⛔ Written before anything can fail, so every path below leaves a defined
    // answer. The runtime reads `*out` unconditionally, and leaving its own
    // buffer untouched is how a "we could not answer" becomes whatever was on
    // its stack.
    // SAFETY: non-null per the check above; the DDI declares it `_Out_`.
    unsafe { core::ptr::write_unaligned(out, 0) };

    // SAFETY: this is a device-scope DDI, so the runtime passes a handle
    // `create_device` returned `S_OK` for; the borrow lives only until the end
    // of this call, which is `device12::device`'s stated precondition.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&UMD12_REFUSALS.caps_slot_no_device);
        return;
    };

    let caps = driver_format_support(dev, format);
    // SAFETY: as above.
    unsafe { core::ptr::write_unaligned(out, caps) };
}

/// `pfnCheckMultisampleQualityLevels` — **2 730** calls inside one
/// `D3D12CreateDevice`, and the single hottest DDI on the device-creation path.
///
/// Exactly **one** gate sits between the engine's answer and the runtime's: a
/// `TILED_RESOURCE` query is answered with zero quality levels, because
/// [`TILED_RESOURCES_TIER_REPORTED`] is `NOT_SUPPORTED`. The engine backs tier 4
/// and answers from `supported_sparse_sample_counts` otherwise
/// (`vkd3d-proton-helios/libs/vkd3d/device.c:5113-5115`), so without the gate
/// this driver would offer multisampled tiled resources on the same device that
/// reports no tiled tier at all.
///
/// # ⛔ The coherence check that looks obviously right, and is not
///
/// The first draft of this function also refused a `SampleCount > 1` answer
/// whose format did not carry `MULTISAMPLE_RENDERTARGET` in
/// [`driver_format_support`], reasoning that *"Driver claimed MSAA support when
/// it shouldn't"* (strings:20) is a device-creation failure and that the two
/// answers, coming from one engine, could not disagree.
///
/// **They can, they do, and the disagreement is deliberate.** vkd3d computes
/// them in two unrelated functions:
///
/// * `d3d12_device_get_format_support` wraps its ENTIRE rendering-and-shader
///   block — including both sites that set `MULTISAMPLE_RENDERTARGET` — in
///   `if (format->type != VKD3D_FORMAT_TYPE_TYPELESS || (aspect & PLANE_0))`
///   (`device.c:5300`), with the comment *"Rendering and shader usage features
///   are not set for typeless formats"*;
/// * `d3d12_device_check_multisample_quality_levels` never looks at
///   `format->type` at all — it tests `format->supported_sample_counts`
///   (`device.c:5113-5119`), which `vkd3d_init_format_sample_counts` fills for
///   every table entry, typeless included.
///
/// So `R24G8_TYPELESS` and `R32_TYPELESS` — **the formats an application
/// actually creates an MSAA depth buffer with** — report no
/// `MULTISAMPLE_RENDERTARGET` and 1 quality level at 4x, simultaneously and
/// correctly. vkd3d's typeless suppression exists precisely to match what
/// native drivers report. The check would therefore have zeroed the quality
/// levels for the whole typeless family, `CreateCommittedResource` with
/// `SampleDesc.Count > 1` would have failed, and its counter — documented
/// "expected 0" — would have read tens per device.
///
/// ⇒ The engine's answer is forwarded. The invariant the check enforced does
/// not exist, and a gate defending an invariant that does not exist is worse
/// than no gate: it breaks the working case and reports that as health.
///
/// # Safety
/// `h_device` must be a live handle from `device12::create_device`, and
/// `num_quality_levels` must address one writable `UINT` the runtime owns.
unsafe extern "C" fn check_multisample_quality_levels(
    h_device: ddi12::D3D12DDI_HDEVICE,
    format: ddi12::DXGI_FORMAT,
    sample_count: ddi12::UINT,
    flags: ddi12::D3D12DDI_MULTISAMPLE_QUALITY_LEVEL_FLAGS,
    num_quality_levels: *mut ddi12::UINT,
) {
    UMD12_REFUSALS.caps_msaa_calls.bump();
    if num_quality_levels.is_null() {
        note_refusal(&UMD12_REFUSALS.caps_slot_bad_arg);
        return;
    }
    // SAFETY: non-null per the check above; the DDI declares it `_Out_`. Zero
    // first, for the same reason as `check_format_support`.
    unsafe { core::ptr::write_unaligned(num_quality_levels, 0) };

    // SAFETY: as `check_format_support`.
    let Some(dev) = (unsafe { device12::device(h_device) }) else {
        note_refusal(&UMD12_REFUSALS.caps_slot_no_device);
        return;
    };

    let tiled_flag =
        ddi12::D3D12DDI_MULTISAMPLE_QUALITY_LEVEL_FLAGS_D3D12DDI_MULTISAMPLE_QUALITY_LEVEL_FLAG_TILED_RESOURCE;
    if flags & tiled_flag != 0 && TILED_RESOURCES_TIER_REPORTED == v::TILED_NONE {
        UMD12_REFUSALS.caps_msaa_tiled_refused.bump();
        let n = UMD12_REFUSALS.caps_msaa_tiled_refused.get();
        if n <= LOG_BUDGET {
            log_error!(
                "CheckMultisampleQualityLevels fmt={format} count={sample_count}: TILED_RESOURCE \
                 query with TiledResourcesTier NOT_SUPPORTED -> 0 levels (x{n})"
            );
        }
        return;
    }

    // ⭐ **DERIVED, NOT FORWARDED.** The engine's own quality-level answer is
    // deliberately not used here: it comes from a different vkd3d function than
    // its format-support answer (`device.c:5104-5121` vs `:5290-5345`) and the
    // two disagree, which the runtime rejects as one contract. Both slots now
    // read [`msaa_capable`], so the pair is coherent by construction — the
    // structure `umd/src/forward/queries.rs:129-164` arrived at for D3D11.
    let (support1, _support2) = engine_format_support(dev, format).unwrap_or((0, 0));
    let capable = msaa_capable(dev, format, support1);

    // ⛔ Only the power-of-two counts D3D11's predicate admits: *"The runtime
    // rejects arbitrary non-power-of-two sample counts"*
    // (`queries.rs:110-112`). The runtime sweeps 2..31 per format, so this is
    // what makes 27 of every 30 answers zero.
    let levels = if capable && matches!(sample_count, 1 | 2 | 4 | 8 | 16) {
        1
    } else {
        0
    };

    // ⭐ **EVERY call, zeros included, and that is the point.** This is the one
    // instrument that can answer *"which (format, sample count) did the runtime
    // reject"*, and it exists because two `D12-G7` runs tried to infer it from
    // call COUNTS instead. `log_error!` cannot do it: a bounded budget truncates
    // long before the interesting call and an unbounded one writes 2 730 lines
    // per device creation, on every boot, forever. `trace_line!` is gated on
    // `HKLM\SOFTWARE\Helios!Umd12Trace`, so the shipping cost is one relaxed
    // `bool` load per call (R420 / `umd_common::log`).
    trace_line!(
        "CheckMultisampleQualityLevels fmt={format} count={sample_count} flags={flags} \
         capable={capable} -> {levels}"
    );

    if levels == 0 {
        return;
    }

    let n = UMD12_REFUSALS.caps_msaa_calls.get();
    if n <= MSAA_LOG_BUDGET {
        log_error!(
            "CheckMultisampleQualityLevels fmt={format} count={sample_count} flags={flags} -> \
             {levels} (x{n})"
        );
    }
    // SAFETY: as above.
    unsafe { core::ptr::write_unaligned(num_quality_levels, levels) };}

/// Ask the engine how many quality levels one (format, sample count, flags)
/// triple has. `None` when there is no engine or it refused; both are counted.
fn engine_msaa_quality_levels(
    dev: &HeliosD3D12Device,
    format: ddi12::DXGI_FORMAT,
    sample_count: ddi12::UINT,
    flags: ddi12::D3D12DDI_MULTISAMPLE_QUALITY_LEVEL_FLAGS,
) -> Option<ddi12::UINT> {
    let Some(engine) = dev.engine.d3d12_device() else {
        // As `engine_format_support`: unreachable by construction, counted so
        // that the construction claim is observable rather than merely stated.
        note_refusal(&UMD12_REFUSALS.caps_slot_no_device);
        return None;
    };
    let mut data = D3D12_FEATURE_DATA_MULTISAMPLE_QUALITY_LEVELS {
        Format: DXGI_FORMAT(format),
        SampleCount: sample_count,
        // The two enums are the same value; see the `const _` assertion above.
        Flags: D3D12_MULTISAMPLE_QUALITY_LEVEL_FLAGS(flags),
        NumQualityLevels: 0,
    };
    // SAFETY: as `engine_format_support` — a live local of exactly the struct
    // the feature names, its own `size_of`, and a borrowed `ID3D12Device` that
    // is never released here.
    let asked = unsafe {
        engine.CheckFeatureSupport(
            D3D12_FEATURE_MULTISAMPLE_QUALITY_LEVELS,
            core::ptr::from_mut(&mut data).cast::<core::ffi::c_void>(),
            core::mem::size_of::<D3D12_FEATURE_DATA_MULTISAMPLE_QUALITY_LEVELS>() as u32,
        )
    };
    if let Err(err) = asked {
        UMD12_REFUSALS.caps_msaa_engine_failed.bump();
        let n = UMD12_REFUSALS.caps_msaa_engine_failed.get();
        if n <= LOG_BUDGET {
            log_error!(
                "CheckMultisampleQualityLevels fmt={format} count={sample_count}: engine refused \
                 hr={:#010x} -> 0 levels (x{n})",
                err.code().0 as u32,
            );
        }
        return None;
    }
    Some(data.NumQualityLevels)
}

/// `pfnGetMipPacking` — never called on this driver, and answered as such.
///
/// ⛔ It describes the packed-mip tail of a **tiled** resource, and
/// [`TILED_RESOURCES_TIER_REPORTED`] is `NOT_SUPPORTED`, so no tiled resource
/// can exist for it to be asked about. It answers "no packed mips, no tiles"
/// and counts, which is the honest pair: a slot that cannot be reached legally
/// still must not leave the runtime's two out-parameters holding stack garbage.
///
/// ⚠ The lane that raises the tiled tier owns this body — it is
/// `pfnUpdateTileMappings`' partner and cannot be written before it.
///
/// # Safety
/// `num_packed_mips` and `num_tiles_for_packed_mips` must each address one
/// writable `UINT` the runtime owns.
unsafe extern "C" fn get_mip_packing(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _h_tiled_resource: ddi12::D3D12DDI_HRESOURCE,
    num_packed_mips: *mut ddi12::UINT,
    num_tiles_for_packed_mips: *mut ddi12::UINT,
) {
    if num_packed_mips.is_null() || num_tiles_for_packed_mips.is_null() {
        note_refusal(&UMD12_REFUSALS.caps_slot_bad_arg);
        return;
    }
    // SAFETY: both non-null per the check above; the DDI declares both `_Out_`.
    unsafe {
        core::ptr::write_unaligned(num_packed_mips, 0);
        core::ptr::write_unaligned(num_tiles_for_packed_mips, 0);
    }
    note_refusal(&UMD12_REFUSALS.caps_mip_packing_refused);
}

/// Install L1's 3 device-core slots: `pfnCheckFormatSupport`,
/// `pfnCheckMultisampleQualityLevels`, `pfnGetMipPacking`.
///
/// Chain position: `Stubbed` -> `CapsSlots` on the device-core table — first,
/// because caps decide everything downstream.
pub(crate) fn install(
    mut filling: Filling<'_, DeviceCoreTable, stage::Stubbed>,
) -> Filling<'_, DeviceCoreTable, stage::CapsSlots> {
    let table = filling.table();
    table.pfnCheckFormatSupport = Some(check_format_support);
    table.pfnCheckMultisampleQualityLevels = Some(check_multisample_quality_levels);
    table.pfnGetMipPacking = Some(get_mip_packing);
    filling.advance()
}
