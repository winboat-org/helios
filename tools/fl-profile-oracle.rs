// Host-side oracle for `umd/src/caps.rs`'s `FeatureProfile` (T8/R1106).
//
//   rustc -O -o /tmp/fl-oracle tools/fl-profile-oracle.rs && /tmp/fl-oracle
//
// Runs on Linux in a second. `umd` is `crate-type = ["cdylib"]` with a
// Windows-only build.rs, so `cargo test` cannot reach the profile table without
// a build-artifact change (which belongs to T0, not here) -- this file is the
// substitute the item asks for, and it is a REGRESSION GATE, not a one-off:
// re-run it whenever the profile table or the knob mapping changes.
//
// It is fault-injected: pointing the `2 =>` arm at `FL11_0` makes it report
// `*** NO ***` for mode 2 and panic on the distinctness assertion (exit 101).
// T8/R1106 oracle: FeatureProfile must answer EXACTLY what the eight
// `feature_level_mode()` comparisons answered, for every knob value.
// The constants are copied from umd/src/caps.rs; the OLD predicates are copied
// from the pre-change caps.rs (`>= 1`) and forward.rs (`== 1` / `!= 1`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MsaaPolicy { Full, SingleSampleOnly }
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FormatPolicy { Unmasked, StripMultisampleBits }
#[derive(Clone, Copy, Debug)]
struct FeatureProfile { pipeline_mask: u32, shader_caps: u32, options1: u32,
                        msaa: MsaaPolicy, format: FormatPolicy }

const LVL_10_0: u32 = 1 << 0;
const LVL_10_1: u32 = 1 << 1;
const LVL_11_0: u32 = 1 << 2;
const LVL_11_1: u32 = 1 << 3;
const LVL_12_0: u32 = 1 << 7;
const SHADER_COMPUTE: u32 = 0x2;
const SHADER_TYPED_UAV_LOAD_ADDITIONAL_FORMATS: u32 = 0x20;
const TILED_RESOURCES_TIER_2_SUPPORTED: u32 = 0x2;
const FL11_PIPELINE_MASK: u32 = LVL_10_0 | LVL_10_1 | LVL_11_0 | LVL_11_1 | LVL_12_0;
const FL11_SHADER_CAPS: u32 = SHADER_COMPUTE | SHADER_TYPED_UAV_LOAD_ADDITIONAL_FORMATS;

const FL10_0: FeatureProfile = FeatureProfile { pipeline_mask: LVL_10_0, shader_caps: 0,
    options1: 0, msaa: MsaaPolicy::SingleSampleOnly, format: FormatPolicy::StripMultisampleBits };
const FL11_0: FeatureProfile = FeatureProfile { pipeline_mask: FL11_PIPELINE_MASK,
    shader_caps: FL11_SHADER_CAPS, options1: TILED_RESOURCES_TIER_2_SUPPORTED,
    msaa: MsaaPolicy::Full, format: FormatPolicy::Unmasked };
const FL11_PIPELINE_ONLY: FeatureProfile = FeatureProfile { pipeline_mask: FL11_PIPELINE_MASK,
    shader_caps: FL11_SHADER_CAPS, options1: TILED_RESOURCES_TIER_2_SUPPORTED,
    msaa: MsaaPolicy::SingleSampleOnly, format: FormatPolicy::StripMultisampleBits };

fn feature_profile(mode: u32) -> &'static FeatureProfile {
    match mode { 0 => &FL10_0, 1 => &FL11_0, 2 => &FL11_PIPELINE_ONLY, _ => &FL11_PIPELINE_ONLY }
}

fn main() {
    // `feature_level_mode()` maps ABSENT -> 1, so mode 1 covers the absent case.
    let mut bad = 0;
    println!("mode | pipeline    shader  opt1 | msaa/fmt | matches old?");
    for mode in 0u32..=6 {
        let p = feature_profile(mode);
        let old_pipeline = if mode >= 1 { FL11_PIPELINE_MASK } else { LVL_10_0 };
        let old_shader   = if mode >= 1 { FL11_SHADER_CAPS } else { 0 };
        let old_options1 = if mode >= 1 { TILED_RESOURCES_TIER_2_SUPPORTED } else { 0 };
        // forward.rs: `!= 1` took the no-MSAA / strip-bits arm.
        let old_msaa_single = mode != 1;
        let old_strip       = mode != 1;
        let ok = p.pipeline_mask == old_pipeline
            && p.shader_caps == old_shader
            && p.options1 == old_options1
            && (p.msaa == MsaaPolicy::SingleSampleOnly) == old_msaa_single
            && (p.format == FormatPolicy::StripMultisampleBits) == old_strip;
        if !ok { bad += 1; }
        println!("{:>4} | 0x{:08x}  0x{:04x}  0x{:02x} | {:?}/{:?} | {}",
                 mode, p.pipeline_mask, p.shader_caps, p.options1, p.msaa, p.format,
                 if ok { "yes" } else { "*** NO ***" });
    }
    // Mode 2 must stay distinct: FL11 caps, FL10 policies.
    assert_eq!(feature_profile(2).pipeline_mask, FL11_0.pipeline_mask);
    assert_ne!(feature_profile(2).format, FL11_0.format);
    assert_ne!(feature_profile(2).pipeline_mask, FL10_0.pipeline_mask);
    println!("\nmode 2 is distinct from BOTH neighbours: FL11 caps + FL10 policies");
    println!("{}", if bad == 0 { "ALL SEVEN KNOB VALUES BEHAVIOUR-IDENTICAL" } else { "MISMATCH" });
    std::process::exit(if bad == 0 { 0 } else { 1 });
}
