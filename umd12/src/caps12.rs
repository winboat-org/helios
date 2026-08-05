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
//! # ⛔ What is NOT here yet, and it is what `D12-G7` is blocked on
//!
//! The three device-core slots ([`install`]) — `pfnCheckFormatSupport`,
//! `pfnCheckMultisampleQualityLevels`, `pfnGetMipPacking` — are still
//! `forward12::noop12` stubs.
//!
//! ⚠ An earlier revision of this paragraph said they were *"device-scope and
//! not on the path that gates device creation"*. **Measured, and wrong**
//! (`tmp/dx12/gates/G7/RESULT.md`): the runtime calls them **2 824 times inside
//! `D3D12CreateDevice`** — 93 format queries, 2 730 MSAA queries, one node map.
//! A counting noop returns 0, so the driver answers *"no format supports
//! anything"*, which is an inconsistent caps set and exactly the
//! `DXGI_ERROR_DRIVER_INTERNAL_ERROR` the gate sees.
//!
//! ⇒ **This lane is not done, and `D12-G7` cannot be green, until those three
//! are real** — see [`install`] for the shape and for the D3D11 precedent whose
//! `_NOT_SUPPORTED`-sentinel trap applies verbatim. `PARALLEL.md` §9.2 wants
//! their noop counters at zero under a real workload; device creation wants them
//! at zero immediately.

use helios_umd_common::hr::{Hresult, E_INVALIDARG, S_OK};

use crate::ddi12;
use crate::forward12::tables12::{stage, DeviceCoreTable, Filling};
use crate::{log_error, note_refusal, UMD12_REFUSALS};

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
/// ⛔ Two values this must never be, both by precedent:
/// * a **bitmask**. `D3D12DDICAPS_TYPE_3DPIPELINESUPPORT` is a *maximum level*
///   for D3D12 — the exact opposite of `D3D11DDICAPS_3DPIPELINESUPPORT`, which
///   `umd/src/caps.rs:57-66` builds as `0x8F`. Writing `0x8F` here reads as
///   "level 143".
/// * `1_0_CORE`. That is the **compute-only** level, and it is what the retired
///   R908 body reported. Do not resurrect it by copy-paste.
const DRIVER_MAX_FEATURE_LEVEL: ddi12::D3D12DDI_3DPIPELINELEVEL = v::FL_11_0;

/// How many times a bounded evidence line may repeat, per site.
const LOG_BUDGET: usize = 64;

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
        OutputMergerLogicOp: 0,
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
    let engine_reports = v::TILED_NONE;
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
        TypedUAVLoadAdditionalFormats: 0,
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
            DepthPitchAlignment: 512,
        };
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

/// `1082 OPTIONS_0102` — descriptor-heap ceilings, and ⭐ **the one answer in
/// this file that a first draft got wrong in two directions at once.**
///
/// `SUBSTRATE.md` §4.5 states that `MaxSamplerDescriptorHeapSize` must be
/// **>= 4000** at DDI 0102+, and pairs it with the guest's
/// `maxSamplerAllocationCount` of exactly 4000 to conclude "zero headroom".
/// Both halves are wrong for this cap:
///
/// * the D3D12 API constant `D3D12_MAX_SHADER_VISIBLE_SAMPLER_HEAP_SIZE` is
///   **2048**, so 4000 would advertise a heap larger than the API permits an
///   application to create;
/// * the **measured** shipping-driver baseline on this very box reports
///   **2048 / 2048** (`docs/dx12/baselines/d3d12-caps.csv:85-86`), not 4000.
///
/// ⇒ 2048, which is simultaneously the API ceiling, the measured baseline, and
/// comfortably inside the Vulkan sampler budget. `SUBSTRATE.md` §4.5's ">= 4000"
/// claim needs correcting there; this site records the disagreement rather than
/// silently diverging.
///
/// `SupportedSampleCountsWithNoOutputs = 1` (count 1 only) is likewise the
/// measured baseline (`:80`) rather than the spec-prose `0x1D`; raising it needs
/// a probe that sample-frequency PS with zero bound outputs actually works.
///
/// # Safety
/// As [`get_caps`].
unsafe fn options_0102(a: &ddi12::D3D12DDIARG_GETCAPS, data_size: usize) -> Hresult {
    /// `D3D12_MAX_SHADER_VISIBLE_SAMPLER_HEAP_SIZE`, and the measured baseline.
    const MAX_SAMPLER_HEAP: ddi12::UINT = 2048;
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
// The device-core slots
// ---------------------------------------------------------------------------

/// Install L1's 3 device-core slots: `pfnCheckFormatSupport`,
/// `pfnCheckMultisampleQualityLevels`, `pfnGetMipPacking`.
///
/// Chain position: `Stubbed` -> `CapsSlots` on the device-core table — first,
/// because caps decide everything downstream.
///
/// ⛔ **Still counting noops, and that is what `D12-G7` is currently blocked
/// on.** An earlier revision of this comment said these three were "not on the
/// path that gates device creation" because they are device-scope rather than
/// the adapter-scope gauntlet. **Measured, and wrong**
/// (`tmp/dx12/gates/G7/RESULT.md`): the runtime calls them **2 824 times inside
/// `D3D12CreateDevice`** — `pfnCheckFormatSupport` 93 times (the 91-format sweep
/// `DDI_REFERENCE.md` §11.1 predicted), `pfnCheckMultisampleQualityLevels`
/// **2 730** times, `pfnQueryNodeMap` once. A counting noop returns 0, so the
/// driver is answering *"no format supports anything and no sample count has any
/// quality level"* — an inconsistent caps set by construction, and the runtime
/// says so with `DXGI_ERROR_DRIVER_INTERNAL_ERROR`.
///
/// ⇒ These need a new `bridge12` entry point forwarding into
/// `ID3D12Device::CheckFeatureSupport` (`D3D12_FEATURE_FORMAT_SUPPORT` and
/// `_MULTISAMPLE_QUALITY_LEVELS`). ⭐ `umd/src/forward/format_caps.rs` is the
/// D3D11 precedent and its `D3D10_DDI_FORMAT_SUPPORT` bit values are
/// **identical** to `D3D12DDI_FORMAT_SUPPORT`'s — including the one that bites:
/// `DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM` (89) must be refused with the
/// explicit `_NOT_SUPPORTED` sentinel `0x8000_0000` and **not a bare 0**, which
/// the D3D11 runtime rejected as a malformed caps response with the same
/// `0x887A0020` this gate is seeing.
pub(crate) fn install(
    mut filling: Filling<'_, DeviceCoreTable, stage::Stubbed>,
) -> Filling<'_, DeviceCoreTable, stage::CapsSlots> {
    let _table = filling.table();
    filling.advance()
}
