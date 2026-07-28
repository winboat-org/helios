//! The D3D11 caps surface: everything `GetCaps` answers for this adapter.
//!
//! Moved verbatim out of `lib.rs` by T8/R1106.

use crate::ddi;
use crate::hr::{Hresult, S_OK};
use crate::log_error;
use crate::{device_funcs, feature_level_mode};

pub(crate) unsafe extern "C" fn get_caps(
    _h_adapter: ddi::D3D10DDI_HADAPTER,
    args: *const ddi::D3D10_2DDIARG_GETCAPS,
) -> Hresult {
    // Aliases onto the generated caps-type enumerators rather than eight hand-
    // written literals. The numerics are identical (128, 129, 130, 131, 132,
    // 134, 136, 137) -- what changes is that they now come from the WDK header
    // and carry `D3D10_2DDICAPS_TYPE`, which is what `args.Type` actually is.
    // The short local names are kept so the match arms below read unchanged.
    use ddi::{
        D3D10_2DDICAPS_TYPE_D3D11DDICAPS_3DPIPELINESUPPORT as D3D11DDICAPS_3DPIPELINESUPPORT,
        D3D10_2DDICAPS_TYPE_D3D11DDICAPS_SHADER as D3D11DDICAPS_SHADER,
        D3D10_2DDICAPS_TYPE_D3D11DDICAPS_THREADING as D3D11DDICAPS_THREADING,
        D3D10_2DDICAPS_TYPE_D3D11_1DDICAPS_ARCHITECTURE_INFO as D3D11_1DDICAPS_ARCHITECTURE_INFO,
        D3D10_2DDICAPS_TYPE_D3D11_1DDICAPS_D3D11_OPTIONS as D3D11_1DDICAPS_D3D11_OPTIONS,
        D3D10_2DDICAPS_TYPE_D3D11_1DDICAPS_SHADER_MIN_PRECISION_SUPPORT as D3D11_1DDICAPS_SHADER_MIN_PRECISION_SUPPORT,
        D3D10_2DDICAPS_TYPE_D3DWDDM1_3DDICAPS_D3D11_OPTIONS1 as D3DWDDM1_3DDICAPS_D3D11_OPTIONS1,
        D3D10_2DDICAPS_TYPE_D3DWDDM1_3DDICAPS_MARKER as D3DWDDM1_3DDICAPS_MARKER,
    };
    // The old literals, pinned so the alias swap is provably value-preserving.
    const _: () = assert!(D3D11DDICAPS_THREADING == 128);
    const _: () = assert!(D3D11DDICAPS_SHADER == 129);
    const _: () = assert!(D3D11DDICAPS_3DPIPELINESUPPORT == 130);
    const _: () = assert!(D3D11_1DDICAPS_D3D11_OPTIONS == 131);
    const _: () = assert!(D3D11_1DDICAPS_ARCHITECTURE_INFO == 132);
    const _: () = assert!(D3D11_1DDICAPS_SHADER_MIN_PRECISION_SUPPORT == 134);
    const _: () = assert!(D3DWDDM1_3DDICAPS_D3D11_OPTIONS1 == 136);
    const _: () = assert!(D3DWDDM1_3DDICAPS_MARKER == 137);

    if !args.is_null() {
        let args = unsafe { &*args };
        log_error!(
            "GetCaps type=0x{:08x} dataSize={} pInfo={:p}",
            args.Type, args.DataSize, args.pInfo,
        );
        if !args.pData.is_null() && args.DataSize != 0 {
            // Default: zero the output.
            unsafe { core::ptr::write_bytes(args.pData as *mut u8, 0, args.DataSize as usize) };
            match args.Type {
                // D3D11DDI_THREADING_CAPS::Caps. Zero means no free-threaded
                // mode and no command-list build support; the runtime must
                // serialize/emulate.
                D3D11DDICAPS_THREADING if args.DataSize >= 4 => {
                    // The value and the state model it licenses now live on one
                    // symbol, next to the Cell/RefCell fields that are sound
                    // only because it is 0. R811.
                    let caps = device_funcs::THREADING_CAPS;
                    unsafe { *(args.pData as *mut u32) = caps };
                    log_error!("  GetCaps: THREADING caps = {caps}");
                }
                // D3D11DDI_SHADER_CAPS::Caps. FL11 mandates compute shaders;
                // the runtime rejects the adapter with "Driver doesn't support
                // compute on FL11" (0x887a0020) if this doesn't advertise
                // compute. Bit 0x2 =
                // D3D11DDICAPS_SHADER_COMPUTE_PLUS_RAW_AND_STRUCTURED_BUFFERS_IN_SHADER_4_X
                // is the driver's compute-capability signal; dxvk/venus back
                // full CS 5.0. FL12_0 additionally requires the D3D11.3 typed
                // UAV-load additional-formats bit. FL10 profile stays 0 (no
                // optional shader caps).
                D3D11DDICAPS_SHADER if args.DataSize >= 4 => {
                    const SHADER_COMPUTE: u32 = 0x2;
                    const SHADER_TYPED_UAV_LOAD_ADDITIONAL_FORMATS: u32 = 0x20;
                    let caps = if feature_level_mode() >= 1 {
                        SHADER_COMPUTE | SHADER_TYPED_UAV_LOAD_ADDITIONAL_FORMATS
                    } else {
                        0
                    };
                    unsafe { *(args.pData as *mut u32) = caps };
                    log_error!("  GetCaps: SHADER caps = 0x{caps:x}");
                }
                // D3D11DDI_3DPIPELINESUPPORT_CAPS::Caps is a BITMASK, NOT the
                // bare D3D11DDI_3DPIPELINELEVEL enum: each supported level sets
                // one bit, D3D11DDI_ENCODE_3DPIPELINESUPPORT_CAP(Level)=(1<<Level),
                // OR'd contiguously from 10_0 up (WDK 10.0.26100 d3d10umddi.h).
                // Enum: 10_0=0, 10_1=1, 11_0=2, 11_1=3, 12_0=7, 12_1=8.
                // FL12_0 requires tiled-resource tier 2+; GetCaps(OPTIONS1)
                // below advertises tier 2 and the WDDM1.3 function table
                // forwards the tile DDIs to DXVK's sparse-resource path. Do
                // not advertise FL12_1 until ROV support is plumbed.
                // Writing the bare enum value was THE FL11 bug: value 2 =
                // bit1 only = "10_1 without 10_0" = an invalid mask, which
                // d3d11.dll rejects with "Driver returned invalid pipeline
                // caps" (0x887a0020) → "Failed to find DDI to drive requested
                // feature levels" (0x887a0004) for EVERY level. (The old FL10
                // path wrote 1 == (1<<0) == the 10_0 bit, so it worked by
                // coincidence and produced an FL10_0 device.)
                D3D11DDICAPS_3DPIPELINESUPPORT if args.DataSize >= 4 => {
                    const LVL_10_0: u32 = 1 << 0;
                    const LVL_10_1: u32 = 1 << 1;
                    const LVL_11_0: u32 = 1 << 2;
                    const LVL_11_1: u32 = 1 << 3;
                    const LVL_12_0: u32 = 1 << 7;
                    let caps = if feature_level_mode() >= 1 {
                        LVL_10_0 | LVL_10_1 | LVL_11_0 | LVL_11_1 | LVL_12_0
                    } else {
                        LVL_10_0 // 0x1: max FL10_0 (the proven baseline)
                    };
                    unsafe { *(args.pData as *mut u32) = caps };
                    log_error!("  GetCaps: 3DPIPELINESUPPORT bitmask=0x{caps:x}");
                }
                // D3D11.1 caps. FL11_1 requires output-merger logic ops; the
                // 11.1 blend-state forwarder maps LogicOpEnable/LogicOp to
                // ID3D11Device1::CreateBlendState1. Keep debug binary support
                // and shader min-precision support disabled.
                D3D11_1DDICAPS_D3D11_OPTIONS if args.DataSize >= 8 => {
                    unsafe { *(args.pData as *mut u32) = 1 };
                    log_error!("  GetCaps: D3D11_OPTIONS OutputMergerLogicOp=TRUE");
                }
                D3D11_1DDICAPS_ARCHITECTURE_INFO if args.DataSize >= 4 => {
                    log_error!("  GetCaps: ARCHITECTURE_INFO = zero");
                }
                D3D11_1DDICAPS_SHADER_MIN_PRECISION_SUPPORT if args.DataSize >= 8 => {
                    log_error!("  GetCaps: SHADER_MIN_PRECISION_SUPPORT = zero");
                }
                D3DWDDM1_3DDICAPS_D3D11_OPTIONS1 if args.DataSize >= 4 => {
                    const TILED_RESOURCES_TIER_2_SUPPORTED: u32 = 0x2;
                    let caps = if feature_level_mode() >= 1 {
                        TILED_RESOURCES_TIER_2_SUPPORTED
                    } else {
                        0
                    };
                    unsafe { *(args.pData as *mut u32) = caps };
                    log_error!(
                        "  GetCaps: D3D11_OPTIONS1 TiledResourcesSupportFlags=0x{caps:x}"
                    );
                }
                D3DWDDM1_3DDICAPS_MARKER if args.DataSize >= 4 => {
                    const D3DWDDM1_3DDI_MARKER_TYPE_NONE: u32 = 0;
                    unsafe { *(args.pData as *mut u32) = D3DWDDM1_3DDI_MARKER_TYPE_NONE };
                    log_error!("  GetCaps: MARKER type = NONE");
                }
                other => {
                    log_error!(
                        "  GetCaps: unsupported cap type {} (zeroed {} bytes)",
                        other, args.DataSize
                    );
                }
            }
        }
    } else {
        log_error!("GetCaps: null args");
    }
    S_OK
}
