//! `CheckFormatSupport`: the per-format capability answer.
//!
//! Reads the feature profile from [`crate::caps`] (T8/R1106) rather than
//! re-deriving a comparison on the `FeatureLevel11` knob.
//!
//! Moved verbatim out of `forward.rs` by T8/R1107.

use super::*;

pub(crate) unsafe extern "C" fn check_format_support(
    h: Hdevice,
    fmt: ddi::DXGI_FORMAT,
    out: *mut u32,
) {
    // The D3D11 DDI `pfnCheckFormatSupport` returns API-style D3D11_FORMAT_SUPPORT
    // flags (D3D11 harmonized the DDI with the API enum; the small
    // D3D10_DDI_FORMAT_SUPPORT enum is only for the legacy D3D10 DDI). So pass
    // DXVK's value through unchanged — translating to the D3D10 DDI layout
    // regresses even a plain D3D11CreateDevice to DXGI_ERROR_UNSUPPORTED.
    let mut caps: u32 = 0;
    if let Some(device) = d3d11_device(h) {
        if let Ok(c) = device.CheckFormatSupport(DXGI_FORMAT(fmt as i32)) {
            caps = c;
        }
    }
    let raw_caps = caps;
    // Keep format support coherent with the active feature-level profile and
    // D3D11.3 §19.2.5. API D3D11_FORMAT_SUPPORT:
    // MULTISAMPLE_RESOLVE=0x40000, MULTISAMPLE_RENDERTARGET=0x200000,
    // MULTISAMPLE_LOAD=0x400000.
    const MSAA_RESOLVE: u32 = 0x0004_0000;
    const MSAA_RENDERTARGET: u32 = 0x0020_0000;
    const MSAA_LOAD: u32 = 0x0040_0000;
    const MSAA_BITS: u32 = MSAA_RESOLVE | MSAA_RENDERTARGET | MSAA_LOAD;
    const DDI_MSAA_RENDERTARGET: u32 = 0x0000_0008;
    const DDI_MSAA_LOAD: u32 = 0x0000_0010;
    const VIDEO_BITS: u32 = 0x0800_0000 | 0x1000_0000 | 0x2000_0000 | 0x4000_0000;
    const TEXTURE1D: u32 = 0x0000_0010;
    const TEXTURE3D: u32 = 0x0000_0040;
    const SHADER_SAMPLE: u32 = 0x0000_0200;
    const SHADER_SAMPLE_COMPARISON: u32 = 0x0000_0400;
    const MIP_AUTOGEN: u32 = 0x0000_2000;
    const RENDER_TARGET: u32 = 0x0000_4000;
    const BLENDABLE: u32 = 0x0000_8000;
    const DEPTH_STENCIL: u32 = 0x0001_0000;
    const SHADER_GATHER: u32 = 0x0080_0000;
    const SHADER_GATHER_COMPARISON: u32 = 0x0400_0000;
    // R820: the six D3D11_FORMAT_SUPPORT bits this function used but did not
    // name. They are what the five whole-value hex constants below decompose
    // into; without them a reader could not tell which capability each hex
    // asserts, or whether it contradicts the MSAA/video scrubs around it.
    const TEXTURE2D: u32 = 0x0000_0020;
    const TEXTURECUBE: u32 = 0x0000_0080;
    const SHADER_LOAD: u32 = 0x0000_0100;
    const MIP: u32 = 0x0000_1000;
    const CPU_LOCKABLE: u32 = 0x0002_0000;
    const CAST_WITHIN_BIT_LAYOUT: u32 = 0x0010_0000;

    // The five values copied from WARP, expressed as compositions and PINNED to
    // the hex they replace. The const-asserts are what make this rewrite
    // provably value-preserving -- and what will make the eventual move to
    // forward/format_caps.rs safe.
    const TYPELESS_PARENT_TEXTURE_CAPS: u32 = TEXTURE1D
        | TEXTURE2D
        | TEXTURE3D
        | TEXTURECUBE
        | MIP
        | CPU_LOCKABLE
        | CAST_WITHIN_BIT_LAYOUT;
    const _: () = assert!(TYPELESS_PARENT_TEXTURE_CAPS == 0x0012_10f0);

    /// The TYPELESS depth-stencil PARENTS: R32G8X24_TYPELESS (19) and
    /// R24G8_TYPELESS (44). Lockable texture families with no depth,
    /// render-target or multisample capability of their own -- the typed
    /// children below carry those.
    const WARP_TYPELESS_PARENT_CAPS: u32 =
        TEXTURE1D | TEXTURE2D | TEXTURECUBE | MIP | CPU_LOCKABLE | CAST_WITHIN_BIT_LAYOUT;
    const _: () = assert!(WARP_TYPELESS_PARENT_CAPS == 0x0012_10b0);

    /// The typed DEPTH formats: D32_FLOAT_S8X24_UINT (20), D32_FLOAT (40),
    /// D24_UNORM_S8_UINT (45) and D16_UNORM (55). These add DEPTH_STENCIL and
    /// the multisample render-target bit.
    const WARP_DEPTH_CAPS: u32 = TEXTURE1D
        | TEXTURE2D
        | TEXTURECUBE
        | MIP
        | DEPTH_STENCIL
        | CPU_LOCKABLE
        | CAST_WITHIN_BIT_LAYOUT
        | MSAA_RENDERTARGET;
    const _: () = assert!(WARP_DEPTH_CAPS == 0x0033_10b0);

    /// The DEPTH read views: R32_FLOAT_X8X24_TYPELESS (21) and
    /// R24_UNORM_X8_TYPELESS (46). Fully sampleable -- sample,
    /// comparison-sample, gather, comparison-gather and multisample load.
    const WARP_DEPTH_READ_CAPS: u32 = TEXTURE1D
        | TEXTURE2D
        | TEXTURECUBE
        | SHADER_LOAD
        | SHADER_SAMPLE
        | SHADER_SAMPLE_COMPARISON
        | MIP
        | CPU_LOCKABLE
        | CAST_WITHIN_BIT_LAYOUT
        | MSAA_LOAD
        | SHADER_GATHER
        | SHADER_GATHER_COMPARISON;
    const _: () = assert!(WARP_DEPTH_READ_CAPS == 0x04d2_17b0);

    /// The STENCIL read views: X32_TYPELESS_G8X24_UINT (22) and
    /// X24_TYPELESS_G8_UINT (47). Integer, so loadable but not sampleable --
    /// no SHADER_SAMPLE and no gather.
    const WARP_STENCIL_READ_CAPS: u32 = TEXTURE1D
        | TEXTURE2D
        | TEXTURECUBE
        | SHADER_LOAD
        | MIP
        | CPU_LOCKABLE
        | CAST_WITHIN_BIT_LAYOUT
        | MSAA_LOAD;
    const _: () = assert!(WARP_STENCIL_READ_CAPS == 0x0052_11b0);
    if crate::caps::feature_profile().format == crate::caps::FormatPolicy::StripMultisampleBits {
        // FL10.0 profile (and diagnostic mode 2): strip the multisample bits.
        caps &= !MSAA_BITS;
    } else if dxgi_msaa_bits_per_sample(fmt as u32, caps).is_some() {
        // FL11: every output-capable format supports at least 4x MSAA. Expose
        // the generic multisample bit for those formats; load/resolve are
        // narrower and follow the §19.2 resource-load/resolve rules.
        caps |= MSAA_RENDERTARGET;
        // The D3D11 UMD callback uses DDI-format-support low bits even though
        // our backing query is API-style. Preserve the API-style bits for the
        // proven path, but also set the DDI MSAA bits the runtime validates
        // during FL11 device construction.
        caps |= DDI_MSAA_RENDERTARGET;
        if caps & DEPTH_STENCIL == 0 {
            caps |= MSAA_LOAD;
            caps |= DDI_MSAA_LOAD;
        }
        if dxgi_resolve_required(fmt as u32) {
            caps |= MSAA_RESOLVE;
        }
        // Helios does not implement the D3D11 video DDI. DXVK's API-level
        // CheckFormatSupport marks ordinary sampled/output formats as video
        // processor inputs/outputs, but the Microsoft runtime validates those
        // bits as part of the UMD feature contract.
        caps &= !VIDEO_BITS;
        if dxgi_color_typeless_parent(fmt as u32) {
            caps = TYPELESS_PARENT_TEXTURE_CAPS;
        }
        if dxgi_integer_typed_format(fmt as u32) {
            caps &= !(SHADER_SAMPLE
                | SHADER_SAMPLE_COMPARISON
                | MIP_AUTOGEN
                | MSAA_RESOLVE
                | SHADER_GATHER
                | SHADER_GATHER_COMPARISON);
        }
        // D3D11 requires the 96-bit R32G32B32 typed output formats as ordinary
        // texture/render-target formats. Vulkan/DXVK under-reports several of
        // these bits; WARP exposes them and the runtime validates the family as
        // part of the FL11 construction path before it reaches application code.
        match fmt as u32 {
            6 => caps |= TEXTURE1D | TEXTURE3D | MIP_AUTOGEN | RENDER_TARGET | BLENDABLE,
            7 | 8 => caps |= TEXTURE1D | TEXTURE3D | RENDER_TARGET,
            _ => {}
        }
    }

    // The Microsoft D3D11 runtime validates some typeless/depth format families
    // as a group during CDevice::LLOCompleteLayerConstruction. DXVK reports the
    // host's raw SO_BUFFER support for the color-typed siblings (for example
    // R32_FLOAT), while the matching depth format (D32_FLOAT) reports none; that
    // mismatch is rejected with DXGI_ERROR_UNSUPPORTED. Normalize the family to
    // the stricter depth-compatible answer.
    const D3D11_FORMAT_SUPPORT_SO_BUFFER: u32 = 0x0000_0008;
    if matches!(
        fmt,
        DXGI_FORMAT_R32_TYPELESS
            | DXGI_FORMAT_D32_FLOAT
            | DXGI_FORMAT_R32_FLOAT
            | DXGI_FORMAT_R32_UINT
            | DXGI_FORMAT_R32_SINT
            | DXGI_FORMAT_R24G8_TYPELESS
            | DXGI_FORMAT_D24_UNORM_S8_UINT
            | DXGI_FORMAT_R24_UNORM_X8_TYPELESS
            | DXGI_FORMAT_X24_TYPELESS_G8_UINT
            | DXGI_FORMAT_R32G8X24_TYPELESS
            | DXGI_FORMAT_D32_FLOAT_S8X24_UINT
            | DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS
            | DXGI_FORMAT_X32_TYPELESS_G8X24_UINT
    ) {
        caps &= !D3D11_FORMAT_SUPPORT_SO_BUFFER;
    }
    const DXGI_FORMAT_R32_TYPELESS: ddi::DXGI_FORMAT = 39;
    const DXGI_FORMAT_D32_FLOAT: ddi::DXGI_FORMAT = 40;
    const DXGI_FORMAT_R32_FLOAT: ddi::DXGI_FORMAT = 41;
    const DXGI_FORMAT_R32_UINT: ddi::DXGI_FORMAT = 42;
    const DXGI_FORMAT_R32_SINT: ddi::DXGI_FORMAT = 43;
    const DXGI_FORMAT_R24G8_TYPELESS: ddi::DXGI_FORMAT = 44;
    const DXGI_FORMAT_D24_UNORM_S8_UINT: ddi::DXGI_FORMAT = 45;
    const DXGI_FORMAT_R24_UNORM_X8_TYPELESS: ddi::DXGI_FORMAT = 46;
    const DXGI_FORMAT_X24_TYPELESS_G8_UINT: ddi::DXGI_FORMAT = 47;
    const DXGI_FORMAT_R32G8X24_TYPELESS: ddi::DXGI_FORMAT = 19;
    const DXGI_FORMAT_D32_FLOAT_S8X24_UINT: ddi::DXGI_FORMAT = 20;
    const DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS: ddi::DXGI_FORMAT = 21;
    const DXGI_FORMAT_X32_TYPELESS_G8X24_UINT: ddi::DXGI_FORMAT = 22;
    const DXGI_FORMAT_D16_UNORM: ddi::DXGI_FORMAT = 55;
    if crate::caps::feature_profile().format == crate::caps::FormatPolicy::Unmasked {
        match fmt {
            // Match WARP's API-visible caps for depth-format families; the
            // DDI-only MSAA RT bit is re-applied immediately below where
            // required. DXVK over-reports the read/view siblings here, and the
            // FL11 constructor rejects that before issuing an MSAA query.
            DXGI_FORMAT_R32G8X24_TYPELESS | DXGI_FORMAT_R24G8_TYPELESS => {
                caps = WARP_TYPELESS_PARENT_CAPS
            }
            DXGI_FORMAT_D32_FLOAT_S8X24_UINT
            | DXGI_FORMAT_D32_FLOAT
            | DXGI_FORMAT_D24_UNORM_S8_UINT
            | DXGI_FORMAT_D16_UNORM => caps = WARP_DEPTH_CAPS,
            DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS | DXGI_FORMAT_R24_UNORM_X8_TYPELESS => {
                caps = WARP_DEPTH_READ_CAPS
            }
            DXGI_FORMAT_X32_TYPELESS_G8X24_UINT | DXGI_FORMAT_X24_TYPELESS_G8_UINT => {
                caps = WARP_STENCIL_READ_CAPS
            }
            _ => {}
        }
    }
    if crate::caps::feature_profile().format == crate::caps::FormatPolicy::Unmasked
        && dxgi_msaa_bits_per_sample(fmt as u32, caps).is_some()
    {
        // In the D3D10/11 UMD callback, low bit 0x8 is
        // D3D10_DDI_FORMAT_SUPPORT_MULTISAMPLE_RENDERTARGET, not API
        // SO_BUFFER. Re-assert it after the API-style compatibility scrubs
        // above so FL11's MSAA validation sees a coherent format-support /
        // quality-level pair, including depth-stencil families.
        caps |= DDI_MSAA_RENDERTARGET;
        if caps & DEPTH_STENCIL == 0 {
            caps |= DDI_MSAA_LOAD;
        }
    }

    // `DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM` (89) is the one format the WDDM
    // runtime validates specially during device creation: the driver MUST signal
    // lack of support with the explicit `D3D10_DDI_FORMAT_SUPPORT_NOT_SUPPORTED`
    // sentinel (0x80000000, "Set only this bit") rather than a bare 0. DXVK does
    // not implement this legacy XR format and returns 0, which the runtime treats
    // as a malformed response and fails `D3D11CreateDevice` with
    // `DXGI_ERROR_DRIVER_INTERNAL_ERROR` (0x887a0020) — the only caps=0 format,
    // observed live. (The observed *value* is unchanged; before R801 this comment
    // named it `DXGI_ERROR_UNSUPPORTED`, which is 0x887a0004. A malformed driver
    // caps response being reported as a driver-internal fault is consistent.)
    // That is the device-create failure DWM hits, after which
    // dwmcore!CreateD3D11Device raises the DWM error 0x889800b0 and crash-loops.
    // Map the 0 to the sentinel so the runtime accepts the (legitimately
    // unsupported) format. PATH-A (2026-06-22).
    const DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM: ddi::DXGI_FORMAT = 89;
    const DDI_FORMAT_SUPPORT_NOT_SUPPORTED: u32 = 0x8000_0000;
    if fmt == DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM && caps == 0 {
        caps = DDI_FORMAT_SUPPORT_NOT_SUPPORTED;
    }
    if crate::caps::feature_profile().format == crate::caps::FormatPolicy::Unmasked {
        trace_line!(
            "FormatSupport fmt={fmt} raw=0x{raw_caps:08x} final=0x{caps:08x} output_bits={:?}",
            dxgi_output_bits_per_sample(fmt as u32, caps)
        );
    }
    if !out.is_null() {
        *out = caps;
    }
}
