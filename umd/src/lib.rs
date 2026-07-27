//! Helios WDDM user-mode display driver bring-up DLL.
//!
//! This is not a D3D implementation yet. It gives the display package a real
//! UMD DLL with the WDDM adapter-open exports that the OS/runtime can resolve.
//! Until the DXVK/VKD3D-backed adapter/device path exists, device creation still
//! fails explicitly. The adapter-open handshake itself must succeed because
//! dxgkrnl calls it during render-adapter start validation.
// R420's static guarantee, at DENY level so it is a compile ERROR and not a
// warning: `log_line` is `#[deprecated]` purely as an internal marker, and the
// only things allowed to call it are the `trace_line!` and `log_error!` macros
// (each wraps the call in `#[allow(deprecated)]`). A new per-op site therefore
// cannot reach the unconditional writer by accident — it does not compile.
// Verified by fault injection: a direct `crate::log_line(..)` yields
// "error: use of deprecated function `log_line`: use log_error! ... or
// trace_line! ...". No dependency trips this lint.
#![deny(deprecated)]

// This crate builds for Windows targets only. `src/ddi.rs` unconditionally
// `include!`s `$OUT_DIR/d3d10umddi.rs`, which `build.rs` can only generate on
// Windows (bindgen over the WDK's d3d10umddi.h). Without this, a host
// `cargo check` produced two contradictory diagnostics — build.rs reporting a
// deliberate skip, then rustc reporting a missing generated file — neither of
// which names the actual constraint. The predicate matches `build.rs`'s own
// `TARGET`-based guard: `cfg(windows)` is evaluated against the target.
#[cfg(not(windows))]
compile_error!(
    "helios_umd builds for windows targets only: src/ddi.rs includes WDK-derived \
     bindgen output that build.rs can only generate on Windows"
);

use core::ffi::c_void;
use std::io::Write;

mod bridge;
mod ddi;
mod device_funcs;
mod forward;
mod hr;

use crate::hr::{
    Hresult, DXGI_ERROR_UNSUPPORTED, DXGI_STATUS_NO_REDIRECTION, E_FAIL, E_NOTIMPL, E_OUTOFMEMORY,
    S_OK,
};

const fn ddi_supported(major: u64, minor: u64, build: u64) -> u64 {
    let interface = (major << 16) | minor;
    (interface << 32) | (build << 16)
}

// Advertise D3D11.1 first so the runtime exposes DXGI 1.1 resource-sharing
// DDIs (notably ResolveSharedResource) and the extended resource-sharing path
// DWM/IddCx require. Keep D3D11.0 as a fallback for older/runtime-selected paths.
const SUPPORTED_DDI_VERSIONS: &[u64] = &[
    ddi_supported(11, 16, 1), // D3DWDDM1_3_DDI_SUPPORTED
    ddi_supported(11, 15, 0), // D3D11_1_DDI_SUPPORTED
    ddi_supported(11, 10, 2), // D3D11_0_DDI_SUPPORTED
];

/// The DDI interface versions `GetSupportedVersions` advertises, as a closed
/// set. `CreateDevice` dispatches on this and nothing else: the previous
/// `if/else-if/else` chain treated "unknown or older interface" as D3D11.0 and
/// bulk-filled `size_of::<D3D11DDI_DEVICEFUNCS>()` = 150 pointer slots into
/// whatever table the runtime had allocated — 101 slots for
/// `D3D10DDI_DEVICEFUNCS`, 103 for `D3D10_1DDI_DEVICEFUNCS`, i.e. a 376..392
/// byte out-of-bounds write into the runtime's heap. `OpenAdapter10` installs
/// `create_device` while installing no `pfnGetSupportedVersions`, so on that
/// path the negotiated interface is entirely the runtime's choice and every
/// D3D10 interface (`0x000a_0001..0x000a_000a`) landed in that `else`.
#[derive(Copy, Clone, PartialEq, Eq)]
enum NegotiatedInterface {
    D3D11_0,
    D3D11_1,
    Wddm1_3,
}

impl NegotiatedInterface {
    const D3D11_0_INTERFACE: u32 = 0x000b_000a;
    const D3D11_1_INTERFACE: u32 = 0x000b_000f;
    const WDDM1_3_INTERFACE: u32 = 0x000b_0010;

    /// Panic-free: a linear scan of a three-element array literal, no indexing.
    fn from_interface(interface: u32) -> Option<Self> {
        match interface {
            Self::WDDM1_3_INTERFACE => Some(Self::Wddm1_3),
            Self::D3D11_1_INTERFACE => Some(Self::D3D11_1),
            Self::D3D11_0_INTERFACE => Some(Self::D3D11_0),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Wddm1_3 => "WDDM1_3",
            Self::D3D11_1 => "D3D11_1",
            Self::D3D11_0 => "D3D11_0",
        }
    }
}

// Keep the enum and the advertised table in lockstep at COMPILE time: adding a
// version to SUPPORTED_DDI_VERSIONS without adding an enum variant fails here,
// and adding a variant without a fill arm fails the exhaustive match in
// `create_device`. That is the property the `else`-as-default did not have.
const _: () = {
    assert!(SUPPORTED_DDI_VERSIONS.len() == 3);
    assert!((SUPPORTED_DDI_VERSIONS[0] >> 32) as u32 == NegotiatedInterface::WDDM1_3_INTERFACE);
    assert!((SUPPORTED_DDI_VERSIONS[1] >> 32) as u32 == NegotiatedInterface::D3D11_1_INTERFACE);
    assert!((SUPPORTED_DDI_VERSIONS[2] >> 32) as u32 == NegotiatedInterface::D3D11_0_INTERFACE);
};

const D3D12_SUPPORTED_DDI_VERSIONS: &[u64] = &[
    // D3D12DDI_SUPPORTED_0003 from WDK 10.0.26100 d3d12umddi.h:
    // interface ((12 << 16) | 2), build 8.
    ddi_supported(12, 2, 8),
];

// The seven d3d10umddi ABI structs that used to be hand-transcribed here
// (ddi::D3D10DDI_HADAPTER, D3d10DdiArgOpenAdapter, D3d10DdiAdapterFuncs,
// D3d10_2DdiAdapterFuncs, D3d10_2DdiArgGetCaps, DxgiDdiBaseArgs,
// D3d10DdiArgCreateDevice) are gone: every one of them is generated into
// `ddi::*` from the WDK header, with bindgen's size/alignment/offset
// assertions, and the code below uses the generated types directly. R802.
//
// The `D3d12Ddi*` structs BELOW stay hand-written, and that is deliberate
// rather than an oversight: build.rs bindgens d3d10umddi.h only, so there is
// no generated d3d12umddi counterpart to switch to. They are reachable only
// from OpenAdapter12, which declines with DXGI_ERROR_UNSUPPORTED before
// touching them; T6's u-core-04 deletes the whole D3D12 adapter path.

#[repr(C)]
pub struct D3d12DdiArgOpenAdapter {
    pub h_rt_adapter: *mut c_void,
    pub h_adapter: ddi::D3D10DDI_HADAPTER,
    pub p_adapter_callbacks: *const c_void,
    pub p_adapter_funcs: *mut D3d12DdiAdapterFuncs,
}

#[repr(C)]
pub struct D3d12DdiAdapterFuncs {
    pub pfn_calc_private_device_size: Option<
        unsafe extern "system" fn(
            ddi::D3D10DDI_HADAPTER,
            *const D3d12DdiArgCalcPrivateDeviceSize,
        ) -> usize,
    >,
    pub pfn_create_device: Option<
        unsafe extern "system" fn(ddi::D3D10DDI_HADAPTER, *const D3d12DdiArgCreateDevice) -> Hresult,
    >,
    pub pfn_close_adapter: Option<unsafe extern "system" fn(ddi::D3D10DDI_HADAPTER) -> Hresult>,
    pub pfn_get_supported_versions:
        Option<unsafe extern "system" fn(ddi::D3D10DDI_HADAPTER, *mut u32, *mut u64) -> Hresult>,
    pub pfn_get_caps: Option<
        unsafe extern "system" fn(ddi::D3D10DDI_HADAPTER, *const ddi::D3D10_2DDIARG_GETCAPS) -> Hresult,
    >,
    pub pfn_get_optional_ddi_tables: Option<
        unsafe extern "system" fn(
            ddi::D3D10DDI_HADAPTER,
            *mut u32,
            *mut D3d12DdiTableRequest,
        ) -> Hresult,
    >,
    pub pfn_fill_ddi_table: Option<
        unsafe extern "system" fn(
            ddi::D3D10DDI_HADAPTER,
            u32,
            *mut c_void,
            usize,
            u32,
            *mut c_void,
        ) -> Hresult,
    >,
    pub pfn_destroy_device: Option<unsafe extern "system" fn(*mut c_void)>,
}

#[repr(C)]
pub struct D3d12DdiArgCalcPrivateDeviceSize {
    pub interface: u32,
    pub version: u32,
    pub flags: u32,
}

#[repr(C)]
pub struct D3d12DdiArgCreateDevice {
    pub h_rt_device: *mut c_void,
    pub interface: u32,
    pub version: u32,
    pub p_kt_callbacks: *const c_void,
    pub h_drv_device: *mut c_void,
    pub p_um_callbacks: *const c_void,
    pub flags: u32,
}

#[repr(C)]
pub struct D3d12DdiTableRequest {
    pub table_type: u32,
    pub num_tables: u32,
}

static mut ADAPTER_COOKIE: usize = 0x4845_4c49_4f53_554d; // "HELIOSUM"

/// Dcomp present vehicle (road 4 unit 2), in-process export for the ICD's
/// WSI: hand over the frame to present — venus resid + WS1 #4 fence value +
/// geometry + the creator's exact allocation identity — immediately before
/// calling Present() on the vehicle swapchain ON THE SAME THREAD. The next
/// dxgi_present on this thread consumes the slot (see
/// forward::set_present_source / vehicle_present_prepare).
/// Returns 0 = stored, 1 = stored but overwrote a pending source (counted
/// contract violation), -1 = refused (zero resid/geometry).
#[no_mangle]
pub extern "system" fn helios_umd_set_present_source(
    resid: u32,
    fence_value: u64,
    width: u32,
    height: u32,
    dxgi_format: u32,
    alloc_size: u64,
    memory_type_index: u32,
) -> i32 {
    forward::set_present_source(
        resid,
        fence_value,
        width,
        height,
        dxgi_format,
        alloc_size,
        memory_type_index,
    )
}

/// Companion export: bounded wait (µs) until the last vehicle present on
/// THIS thread — the frame copy included — completed on the GPU. The ICD's
/// present worker calls this after Present() returns and only then recycles
/// the frame image, closing the copy-vs-rerender race. Returns 0 =
/// complete, 1 = timeout (counted caller-side), -1 = no vehicle present
/// recorded on this thread.
#[no_mangle]
pub extern "system" fn helios_umd_wait_last_present(timeout_us: u32) -> i32 {
    forward::wait_last_present(timeout_us)
}

/// Companion export: take the last MINTED vehicle present's recycle-gate
/// identity — the vehicle device's named present fence
/// (Global\HeliosPresentFence_<pid>_<fence_id>) and the value its frame-copy
/// signal retires at (host GPU completion). Same-thread contract as the
/// pair above; consuming. The ICD imports the fence by name once per chain
/// and gates frame-image reuse on (fence, value) at ACQUIRE — replacing the
/// worker-serial wait_last_present. Returns 0 = taken, -1 = none pending
/// (failed present / publish unavailable / contract violation, counted) —
/// the caller must fall back to helios_umd_wait_last_present.
#[no_mangle]
pub extern "system" fn helios_umd_get_present_result(fence_id: *mut u32, value: *mut u64) -> i32 {
    if fence_id.is_null() || value.is_null() {
        return -1;
    }
    // SAFETY: null-checked above; the ICD caller owns both out-params for
    // the duration of the call (same-thread, synchronous contract).
    unsafe { forward::take_present_result(&mut *fence_id, &mut *value) }
}

/// TEMPORARY (Gate 5b bring-up): out-of-band smoke test of the DXVK bridge from a
/// normal process — no WDDM/INF/devcon/DWM involvement. Returns 0 if DXVK brought
/// up a logical device on the venus adapter, 1 otherwise. Remove once the DDI path
/// is validated end-to-end.
#[no_mangle]
pub extern "system" fn helios_umd_selftest() -> i32 {
    log_error!("helios_umd_selftest: creating DXVK device on venus...");
    let dev = bridge::ffi::helios_dxvk_create_device(0, 0);
    let bridge_ok = !dev.is_null();
    log_error!("helios_umd_selftest: bridge ok={bridge_ok}");
    if !bridge_ok {
        return 1;
    }

    // Prove the windows-crate COM bindings call straight into DXVK's ID3D11Device
    // (the foundation for the pure-Rust DDI forwarders): create a real buffer.
    {
        use windows::Win32::Graphics::Direct3D11::*;
        let dev_ptr = dev.d3d11_device_ptr();
        log_error!(
            "helios_umd_selftest: ID3D11Device* = 0x{dev_ptr:x}"
        );
        if dev_ptr != 0 {
            let device = core::mem::ManuallyDrop::new(unsafe {
                <ID3D11Device as windows::core::Interface>::from_raw(dev_ptr as *mut _)
            });
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: 256,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
                ..Default::default()
            };
            let mut buffer: Option<ID3D11Buffer> = None;
            let hr = unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) };
            log_error!(
                "helios_umd_selftest: windows-crate CreateBuffer -> {hr:?}, buffer_some={}",
                buffer.is_some()
            );
        }
    }
    drop(dev);

    // Exercise the full CreateDevice path in-process with synthesized runtime
    // buffers (no D3D runtime / DWM): proves CalcPrivateDeviceSize + CreateDevice
    // + the 152-entry table fill + DestroyDevice don't crash before risking a
    // live install. Buffers are u64-backed for 8-byte alignment.
    let hadapter = ddi::D3D10DDI_HADAPTER {
        pDrvPrivate: core::ptr::null_mut(),
    };
    let dev_size = unsafe { calc_private_device_size(hadapter, core::ptr::null()) } as usize;

    let mut device_priv = vec![0u64; dev_size / 8 + 1];
    let mut funcs = vec![0u64; core::mem::size_of::<ddi::D3D11DDI_DEVICEFUNCS>() / 8 + 1];
    let mut dxgi_funcs = vec![0u64; core::mem::size_of::<ddi::DXGI_DDI_BASE_FUNCTIONS>() / 8 + 1];

    // Both unions are initialised explicitly, and each names the member that
    // matches the `Interface` above: an 11.0-negotiated device reads
    // `p11DeviceFuncs` and `pUMCallbacks`. Every member of each union is a
    // pointer at offset 0 (bindgen asserts that in the generated module), so
    // this is representationally identical to the old single-pointer field --
    // what it buys is that the member name and `Interface` now have to agree.
    let arg = ddi::D3D10DDIARG_CREATEDEVICE {
        // The WDK spells runtime-owned handles `handle` and driver-private ones
        // `pDrvPrivate`. The hand-written struct erased that by declaring every
        // one as a bare `*mut c_void`.
        hRTDevice: ddi::D3D10DDI_HRTDEVICE {
            handle: core::ptr::null_mut(),
        },
        Interface: 0x000b_000a, // D3D11_0_DDI_INTERFACE_VERSION
        Version: 0,
        pKTCallbacks: core::ptr::null(),
        __bindgen_anon_1: ddi::D3D10DDIARG_CREATEDEVICE__bindgen_ty_1 {
            p11DeviceFuncs: funcs.as_mut_ptr().cast(),
        },
        hDrvDevice: ddi::D3D10DDI_HDEVICE {
            pDrvPrivate: device_priv.as_mut_ptr().cast(),
        },
        DXGIBaseDDI: ddi::DXGI_DDI_BASE_ARGS {
            pDXGIBaseCallbacks: core::ptr::null_mut(),
            __bindgen_anon_1: ddi::DXGI_DDI_BASE_ARGS__bindgen_ty_1 {
                pDXGIDDIBaseFunctions: dxgi_funcs.as_mut_ptr().cast(),
            },
        },
        hRTCoreLayer: ddi::D3D10DDI_HRTCORELAYER {
            handle: core::ptr::null_mut(),
        },
        __bindgen_anon_2: ddi::D3D10DDIARG_CREATEDEVICE__bindgen_ty_2 {
            pUMCallbacks: core::ptr::null(),
        },
        Flags: 0,
        ppfnRetrieveSubObject: core::ptr::null_mut(),
    };

    // No `as *mut c_void` any more: `create_device` takes the arg struct by its
    // own type, so passing the wrong thing here is a compile error.
    let mut arg = arg;
    let hr = unsafe { create_device(hadapter, &mut arg) };
    log_error!(
        "helios_umd_selftest: synthesized CreateDevice -> 0x{hr:08x}"
    );
    if hr != S_OK {
        return 2;
    }

    // Sanity: the table must be fully non-null (a null entry the runtime calls = crash).
    let table = funcs.as_ptr() as *const Option<unsafe extern "C" fn(usize) -> usize>;
    let n = core::mem::size_of::<ddi::D3D11DDI_DEVICEFUNCS>() / 8;
    let null_slots = (0..n)
        .filter(|&i| unsafe { (*table.add(i)).is_none() })
        .count();
    log_error!(
        "helios_umd_selftest: device-funcs null slots = {null_slots} / {n}"
    );

    // Offscreen clear+readback through the real forwarders.
    let hdev = ddi::D3D10DDI_HDEVICE {
        pDrvPrivate: device_priv.as_mut_ptr().cast(),
    };
    let render_rc = unsafe { forward::selftest_offscreen_clear(hdev) };
    log_error!(
        "helios_umd_selftest: offscreen clear rc={render_rc}"
    );
    let tri_rc = unsafe { forward::selftest_triangle(hdev) };
    log_error!("helios_umd_selftest: triangle rc={tri_rc}");
    let cb_rc = unsafe { forward::selftest_cb_readback(hdev) };
    log_error!("helios_umd_selftest: cb_readback rc={cb_rc}");
    let cbtri_rc = unsafe { forward::selftest_triangle_cb(hdev) };
    log_error!("helios_umd_selftest: triangle_cb rc={cbtri_rc}");

    // Tear the device back down via the real DestroyDevice entry.
    let device_funcs_table = funcs.as_ptr() as *const ddi::D3D11DDI_DEVICEFUNCS;
    if let Some(destroy) = unsafe { (*device_funcs_table).pfnDestroyDevice } {
        let hdev = ddi::D3D10DDI_HDEVICE {
            pDrvPrivate: device_priv.as_mut_ptr().cast(),
        };
        unsafe { destroy(hdev) };
    }

    if null_slots == 0 {
        0
    } else {
        3
    }
}

#[no_mangle]
pub unsafe extern "system" fn OpenAdapter10(open_data: *mut ddi::D3D10DDIARG_OPENADAPTER) -> Hresult {
    log_error!("OpenAdapter10");
    unsafe { open_adapter_common(open_data, false) }
}

#[no_mangle]
pub unsafe extern "system" fn OpenAdapter10_2(open_data: *mut ddi::D3D10DDIARG_OPENADAPTER) -> Hresult {
    log_error!("OpenAdapter10_2");
    unsafe { open_adapter_common(open_data, true) }
}

#[no_mangle]
pub unsafe extern "system" fn OpenAdapter12(open_data: *mut D3d12DdiArgOpenAdapter) -> Hresult {
    log_error!("OpenAdapter12");
    log_error!("OpenAdapter12 -> DXGI_ERROR_UNSUPPORTED (D3D12 DDI not implemented yet)");
    let _ = open_data;
    // Declining an unimplemented DDI is DXGI_ERROR_UNSUPPORTED (0x887A_0004),
    // not DXGI_ERROR_DRIVER_INTERNAL_ERROR (0x887A_0020). This site returned
    // the latter until R801 because the two shared a constant name: a D3D12
    // client's ordinary "this driver has no D3D12 DDI" negotiation was recorded
    // by the runtime and by ETW as a *driver fault*. Nothing distinguished the
    // two in our log, since both printed as "DXGI_ERROR_UNSUPPORTED".
    return DXGI_ERROR_UNSUPPORTED;

    #[allow(unreachable_code)]
    {
        if open_data.is_null() {
            log_error!("OpenAdapter12 null open_data -> E_NOTIMPL");
            return E_NOTIMPL;
        }

        let open = unsafe { &mut *open_data };
        if open.p_adapter_funcs.is_null() {
            log_error!("OpenAdapter12 null pAdapterFuncs -> E_NOTIMPL");
            return E_NOTIMPL;
        }

        open.h_adapter = ddi::D3D10DDI_HADAPTER {
            pDrvPrivate: core::ptr::addr_of_mut!(ADAPTER_COOKIE).cast::<c_void>(),
        };

        let funcs = unsafe { &mut *open.p_adapter_funcs };
        funcs.pfn_calc_private_device_size = Some(d3d12_calc_private_device_size);
        funcs.pfn_create_device = Some(d3d12_create_device);
        funcs.pfn_close_adapter = Some(d3d12_close_adapter);
        funcs.pfn_get_supported_versions = Some(d3d12_get_supported_versions);
        funcs.pfn_get_caps = Some(d3d12_get_caps);
        funcs.pfn_get_optional_ddi_tables = Some(d3d12_get_optional_ddi_tables);
        funcs.pfn_fill_ddi_table = Some(d3d12_fill_ddi_table);
        funcs.pfn_destroy_device = Some(d3d12_destroy_device);

        log_error!("OpenAdapter12 -> S_OK (adapter funcs installed)");
        S_OK
    }
}

unsafe extern "system" fn d3d12_calc_private_device_size(
    _h_adapter: ddi::D3D10DDI_HADAPTER,
    args: *const D3d12DdiArgCalcPrivateDeviceSize,
) -> usize {
    if args.is_null() {
        log_error!("D3D12 CalcPrivateDeviceSize null args -> 8");
    } else {
        let args = unsafe { &*args };
        log_error!(
            "D3D12 CalcPrivateDeviceSize interface=0x{:08x} version=0x{:08x} flags=0x{:08x} -> 8",
            args.interface, args.version, args.flags
        );
    }
    core::mem::size_of::<usize>()
}

unsafe extern "system" fn d3d12_create_device(
    _h_adapter: ddi::D3D10DDI_HADAPTER,
    args: *const D3d12DdiArgCreateDevice,
) -> Hresult {
    if args.is_null() {
        log_error!("D3D12 CreateDevice null args -> E_NOTIMPL");
    } else {
        let args = unsafe { &*args };
        log_error!(
            "D3D12 CreateDevice interface=0x{:08x} version=0x{:08x} flags=0x{:08x} \
             hRTDevice={:p} hDrvDevice={:p} pKTCallbacks={:p} pUMCallbacks={:p} -> E_NOTIMPL",
            args.interface,
            args.version,
            args.flags,
            args.h_rt_device,
            args.h_drv_device,
            args.p_kt_callbacks,
            args.p_um_callbacks,
        );
    }
    E_NOTIMPL
}

unsafe extern "system" fn d3d12_close_adapter(_h_adapter: ddi::D3D10DDI_HADAPTER) -> Hresult {
    log_error!("D3D12 CloseAdapter");
    S_OK
}

unsafe extern "system" fn d3d12_get_supported_versions(
    _h_adapter: ddi::D3D10DDI_HADAPTER,
    entries: *mut u32,
    supported_versions: *mut u64,
) -> Hresult {
    if entries.is_null() {
        log_error!("D3D12 GetSupportedVersions null entries -> E_NOTIMPL");
        return E_NOTIMPL;
    }

    let requested_entries = unsafe { *entries };
    log_error!(
        "D3D12 GetSupportedVersions requested={requested_entries} bufNull={} (advertising {:#018x?})",
        supported_versions.is_null(),
        D3D12_SUPPORTED_DDI_VERSIONS,
    );
    unsafe { *entries = D3D12_SUPPORTED_DDI_VERSIONS.len() as u32 };

    if supported_versions.is_null() {
        return S_OK;
    }

    if requested_entries < D3D12_SUPPORTED_DDI_VERSIONS.len() as u32 {
        return E_OUTOFMEMORY;
    }

    for (index, version) in D3D12_SUPPORTED_DDI_VERSIONS.iter().enumerate() {
        unsafe { *supported_versions.add(index) = *version };
    }
    S_OK
}

unsafe extern "system" fn d3d12_get_caps(
    _h_adapter: ddi::D3D10DDI_HADAPTER,
    args: *const ddi::D3D10_2DDIARG_GETCAPS,
) -> Hresult {
    // Typed as D3D10_2DDICAPS_TYPE to match `args.Type`. These stay hand-written
    // (unlike the D3D11 set above, which now aliases the generated enumerators)
    // because build.rs bindgens d3d10umddi.h only -- there is no generated
    // d3d12umddi, so no D3D12DDICAPS_TYPE_* to alias. Values unchanged.
    const D3D12DDICAPS_TYPE_MEMORY_ARCHITECTURE: ddi::D3D10_2DDICAPS_TYPE = 1002;
    const D3D12DDICAPS_TYPE_SHADER: ddi::D3D10_2DDICAPS_TYPE = 1004;
    const D3D12DDICAPS_TYPE_ARCHITECTURE_INFO: ddi::D3D10_2DDICAPS_TYPE = 1005;
    const D3D12DDICAPS_TYPE_D3D12_OPTIONS: ddi::D3D10_2DDICAPS_TYPE = 1006;
    const D3D12DDICAPS_TYPE_3DPIPELINESUPPORT: ddi::D3D10_2DDICAPS_TYPE = 1007;
    const D3D12DDICAPS_TYPE_0081_3DPIPELINESUPPORT1: ddi::D3D10_2DDICAPS_TYPE = 1074;
    const D3D12DDI_3DPIPELINELEVEL_1_0_CORE: u32 = 2;

    if args.is_null() {
        log_error!("D3D12 GetCaps null args -> S_OK");
        return S_OK;
    }

    let args = unsafe { &*args };
    log_error!(
        "D3D12 GetCaps type=0x{:08x} dataSize={} pInfo={:p}",
        args.Type, args.DataSize, args.pInfo,
    );

    if !args.pData.is_null() && args.DataSize != 0 {
        if args.Type == D3D12DDICAPS_TYPE_0081_3DPIPELINESUPPORT1 && args.DataSize >= 8 {
            let data = args.pData as *mut u32;
            let runtime_max = unsafe { *data.add(0) };
            let driver_max = runtime_max.min(D3D12DDI_3DPIPELINELEVEL_1_0_CORE);
            unsafe {
                *data.add(1) = driver_max;
            }
            log_error!(
                "  D3D12 GetCaps: 3DPIPELINESUPPORT1 runtimeMax={} driverMax={}",
                runtime_max, driver_max
            );
            return S_OK;
        }

        unsafe { core::ptr::write_bytes(args.pData as *mut u8, 0, args.DataSize as usize) };
        match args.Type {
            D3D12DDICAPS_TYPE_3DPIPELINESUPPORT if args.DataSize >= 4 => {
                unsafe { *(args.pData as *mut u32) = D3D12DDI_3DPIPELINELEVEL_1_0_CORE };
                log_error!("  D3D12 GetCaps: 3DPIPELINESUPPORT = 1_0_CORE");
            }
            D3D12DDICAPS_TYPE_MEMORY_ARCHITECTURE if args.DataSize >= 12 => {
                let data = args.pData as *mut u32;
                unsafe {
                    *data.add(0) = 1; // UMA
                    *data.add(1) = 1; // IO coherent
                    *data.add(2) = 1; // Cache coherent
                }
                log_error!("  D3D12 GetCaps: MEMORY_ARCHITECTURE = UMA/IO/cache coherent");
            }
            D3D12DDICAPS_TYPE_SHADER => {
                log_error!("  D3D12 GetCaps: SHADER = zero");
            }
            D3D12DDICAPS_TYPE_D3D12_OPTIONS => {
                log_error!("  D3D12 GetCaps: D3D12_OPTIONS = zero");
            }
            D3D12DDICAPS_TYPE_ARCHITECTURE_INFO => {
                log_error!("  D3D12 GetCaps: ARCHITECTURE_INFO = zero");
            }
            _ => {}
        }
    }

    S_OK
}

unsafe extern "system" fn d3d12_get_optional_ddi_tables(
    _h_adapter: ddi::D3D10DDI_HADAPTER,
    entries: *mut u32,
    requests: *mut D3d12DdiTableRequest,
) -> Hresult {
    if entries.is_null() {
        log_error!("D3D12 GetOptionalDDITables null entries -> E_NOTIMPL");
        return E_NOTIMPL;
    }

    let requested_entries = unsafe { *entries };
    log_error!(
        "D3D12 GetOptionalDDITables requested={requested_entries} requestsNull={} -> 0 tables",
        requests.is_null()
    );
    unsafe { *entries = 0 };
    S_OK
}

unsafe extern "system" fn d3d12_fill_ddi_table(
    _h_adapter: ddi::D3D10DDI_HADAPTER,
    table_type: u32,
    table: *mut c_void,
    table_size: usize,
    interface: u32,
    rt_table: *mut c_void,
) -> Hresult {
    log_error!(
        "D3D12 FillDDITable type={} table={:p} size={} interface=0x{:08x} rtTable={:p} -> E_NOTIMPL",
        table_type, table, table_size, interface, rt_table,
    );
    E_NOTIMPL
}

unsafe extern "system" fn d3d12_destroy_device(h_device: *mut c_void) {
    log_error!("D3D12 DestroyDevice hDevice={h_device:p}");
}

unsafe fn open_adapter_common(open_data: *mut ddi::D3D10DDIARG_OPENADAPTER, with_10_2: bool) -> Hresult {
    if open_data.is_null() {
        log_error!("open_adapter_common null open_data");
        return E_NOTIMPL;
    }

    let open = unsafe { &mut *open_data };
    // `pAdapterFuncs` and `pAdapterFuncs_2` are the two members of one union;
    // which one the runtime means is decided by which OpenAdapter export it
    // called, i.e. by `with_10_2`. Read it generically for the null check --
    // both members alias at offset 0 -- and name the right member per arm
    // below, where the table shape actually matters.
    // SAFETY: reading either member of a union of same-offset pointers is
    // well-defined for any initialisation the runtime can have performed.
    if unsafe { open.__bindgen_anon_1.pAdapterFuncs }.is_null() {
        log_error!("open_adapter_common null p_adapter_funcs");
        return E_NOTIMPL;
    }
    log_error!(
        "open_adapter_common interface=0x{:08x} version=0x{:08x} with_10_2={}",
        open.Interface, open.Version, with_10_2
    );
    log_self_module_path();

    open.hAdapter = ddi::D3D10DDI_HADAPTER {
        pDrvPrivate: core::ptr::addr_of_mut!(ADAPTER_COOKIE).cast::<c_void>(),
    };

    // The generated D3D10_2DDI_ADAPTERFUNCS is FLAT -- the WDK repeats the
    // three 10.0 entries rather than nesting them, where the hand copy modelled
    // them as a `base` sub-struct. Same layout (the hand copy's `base` sat at
    // offset 0), different spelling.
    unsafe {
        if with_10_2 {
            let funcs = &mut *open.__bindgen_anon_1.pAdapterFuncs_2;
            funcs.pfnCalcPrivateDeviceSize = Some(calc_private_device_size);
            funcs.pfnCreateDevice = Some(create_device);
            funcs.pfnCloseAdapter = Some(close_adapter);
            funcs.pfnGetSupportedVersions = Some(get_supported_versions);
            funcs.pfnGetCaps = Some(get_caps);
        } else {
            let funcs = &mut *open.__bindgen_anon_1.pAdapterFuncs;
            funcs.pfnCalcPrivateDeviceSize = Some(calc_private_device_size);
            funcs.pfnCreateDevice = Some(create_device);
            funcs.pfnCloseAdapter = Some(close_adapter);
        }
    }

    S_OK
}

// NOTE on the calling convention: the five functions below are `extern "C"`,
// not `extern "system"`, because they are stored into the generated
// `D3D10DDI_ADAPTERFUNCS` / `D3D10_2DDI_ADAPTERFUNCS` tables and bindgen types
// every `PFND3D10DDI_*` as `extern "C"`. On x86_64-pc-windows-msvc the two are
// the same calling convention, so this is a no-op in the emitted code -- but
// rustc treats them as distinct TYPES, so the tables will not accept a
// "system" fn. The `OpenAdapter*` exports above stay `extern "system"`: they
// are resolved by the loader against an exported name, not through a PFN type.
unsafe extern "C" fn calc_private_device_size(
    _h_adapter: ddi::D3D10DDI_HADAPTER,
    _args: *const ddi::D3D10DDIARG_CALCPRIVATEDEVICESIZE,
) -> ddi::SIZE_T {
    let size = device_funcs::device_private_size();
    log_error!("CalcPrivateDeviceSize -> {size}");
    // `SIZE_T` is the WDK's spelling and is a distinct type from `usize` even
    // though both are 64-bit here, so the PFN type needs the conversion.
    size as ddi::SIZE_T
}

unsafe extern "C" fn create_device(
    _h_adapter: ddi::D3D10DDI_HADAPTER,
    args: *mut ddi::D3D10DDIARG_CREATEDEVICE,
) -> Hresult {
    // SAFETY: the runtime passes a valid `D3D10DDIARG_CREATEDEVICE*` per the
    // `PFND3D10DDI_CREATEDEVICE` contract; we only read scalar/pointer fields and
    // never write through it, so an E_NOTIMPL return leaves the runtime's state
    // untouched. We null-check defensively.
    if args.is_null() {
        log_error!("CreateDevice null args -> E_NOTIMPL");
        return E_NOTIMPL;
    }
    // The parameter is now typed by `PFND3D10DDI_CREATEDEVICE` itself rather
    // than being a `*mut c_void` this function reinterprets, so the cast that
    // used to sit here -- the one place a wrong type would have gone unnoticed
    // -- no longer exists.
    let create = unsafe { &*args };
    // The three union members this function reads generically: for logging, for
    // null-checking, and for handing to the runtime. Every member of each union
    // is a pointer at offset 0 -- machine-checked by the bindgen layout
    // assertions in the generated module -- so which member is named here does
    // not change the value. Where the CHOICE is load-bearing is the fill match
    // at step 3, which names the member matching the negotiated interface.
    // SAFETY: reading any member of a union of same-offset pointers is
    // well-defined for every initialisation the runtime can have performed;
    // `create` itself is validated by the caller contract above.
    let p_device_funcs = unsafe { create.__bindgen_anon_1.pDeviceFuncs };
    let p_um_callbacks = unsafe { create.__bindgen_anon_2.pUMCallbacks };
    let p_dxgi_base_functions =
        unsafe { create.DXGIBaseDDI.__bindgen_anon_1.pDXGIDDIBaseFunctions };
    // Ground-truth dump: the negotiated Interface decides which funcs-table
    // LAYOUT the runtime reads back, and a misread here silently wires typed
    // 11.1 handlers into slots an 11.0-negotiated device never calls (dwm's
    // singlethreaded devices were observed hitting the UNTYPED shader creates
    // → float32-typed SPIR-V inputs vs SINT vertex data, VUID-Input-08733).
    if trace_enabled() {
        // Bound the dump by the struct being interpreted, not by a literal. The
        // hand copy is 88 bytes (ppfnRetrieveSubObject@80) and that member only
        // exists from minor >= 3, so the runtime's object can be 80 bytes; the
        // old `0..12` read bytes 0..96, which is 8 past the largest possible
        // layout and 16 past the smallest — an access violation inside the
        // caller's D3D11CreateDevice if the arg sits at the end of a page, or a
        // garbage dump that reads as real ABI evidence.
        //
        // Words 0..9 cover every field this code actually interprets: hRTDevice,
        // interface/version, pKTCallbacks, pDeviceFuncs, hDrvDevice, the 16-byte
        // DXGIBaseDDI, hRTCoreLayer, pUMCallbacks, flags. Word 10 is read only
        // when the negotiated interface says it is there, keyed on the same
        // closed set R405 introduced; an unknown interface reads the short shape.
        let words = match NegotiatedInterface::from_interface(create.Interface) {
            Some(NegotiatedInterface::D3D11_1) | Some(NegotiatedInterface::Wddm1_3) => 11,
            Some(NegotiatedInterface::D3D11_0) | None => 10,
        };
        let q = args as *const u64;
        let mut raw = String::from("CreateDevice raw args:");
        for i in 0..words {
            raw.push_str(&format!(" [{}]=0x{:016x}", i, unsafe {
                q.add(i).read_unaligned()
            }));
        }
        trace_line!("{raw}");
    }
    log_error!(
        "CreateDevice interface=0x{:08x} version=0x{:08x} flags=0x{:08x} \
         pDeviceFuncs={:p} hDrvDevice={:p} pKTCallbacks={:p} pUMCallbacks={:p} pDXGIBaseFuncs={:p}",
        create.Interface,
        create.Version,
        create.Flags,
        p_device_funcs,
        create.hDrvDevice.pDrvPrivate,
        create.pKTCallbacks,
        p_um_callbacks,
        p_dxgi_base_functions,
    );

    // 0) Validate every runtime-supplied pointer BEFORE constructing anything.
    //    Both of these checks used to run after construction: the hDrvDevice one
    //    leaked the whole DXVK/Vulkan device, and the pDeviceFuncs one (which
    //    ran after the device, the in-place HeliosDevice, the runtime context
    //    and the paging queue all existed) leaked a kernel context and a paging
    //    queue per attempt, skipping both destroy_runtime_objects and
    //    drop_in_place. A crash-looping client exhausted them.
    if create.hDrvDevice.pDrvPrivate.is_null() {
        log_error!("  CreateDevice: null hDrvDevice -> E_FAIL");
        return E_FAIL;
    }
    if p_device_funcs.is_null() {
        log_error!("  CreateDevice: null pDeviceFuncs -> E_FAIL");
        return E_FAIL;
    }
    //    The negotiated interface is runtime-supplied too, and it selects the
    //    SHAPE of the table we write. Refuse anything outside the advertised
    //    set rather than defaulting to D3D11.0's 150-slot fill.
    let Some(negotiated) = NegotiatedInterface::from_interface(create.Interface) else {
        log_error!(
            "  CreateDevice: unsupported interface 0x{:08x} (advertised 0x{:08x}/0x{:08x}/0x{:08x}) -> E_NOTIMPL",
            create.Interface,
            NegotiatedInterface::WDDM1_3_INTERFACE,
            NegotiatedInterface::D3D11_1_INTERFACE,
            NegotiatedInterface::D3D11_0_INTERFACE,
        );
        return E_NOTIMPL;
    };

    // 1) Bring up the DXVK device on the Helios venus adapter.
    let dxvk = bridge::ffi::helios_dxvk_create_device(0, 0);
    if dxvk.is_null() {
        log_error!("  CreateDevice: DXVK device creation FAILED -> E_FAIL");
        return E_FAIL;
    }

    // 2) Construct our device object in the runtime-allocated private memory
    //    (size came from CalcPrivateDeviceSize). hDrvDevice IS that pointer.
    unsafe {
        core::ptr::write(
            create.hDrvDevice.pDrvPrivate as *mut device_funcs::HeliosDevice,
            device_funcs::HeliosDevice {
                // One grouped field, constructed (and declared) before `dxvk`
                // so every bridge-derived COM object it holds outlives the
                // bridge device rather than three of the four dropping after
                // it. R807.
                owned: device_funcs::BridgeOwned::new(),
                dxvk,
                h_rt_device: create.hRTDevice.handle,
                // Populated by create_runtime_context below; CreateDevice
                // fails if that does not succeed, so a device the runtime ever
                // sees always has one. R808.
                context: None,
                // Both of these arrive already typed from the bindgen struct;
                // the hand copy declared them as bare `c_void` pointers and had
                // to cast at this site.
                kt_callbacks: create.pKTCallbacks,
                paging_queue: None,
                dxgi_callbacks: create.DXGIBaseDDI.pDXGIBaseCallbacks,
                scanout_resource_raw: core::cell::Cell::new(0),
                scanout_resource_id: core::cell::Cell::new(0),
                scanout_allocation: core::cell::Cell::new(0),
                scanout_width: core::cell::Cell::new(0),
                scanout_height: core::cell::Cell::new(0),
                scanout_format: core::cell::Cell::new(0),
                scanout_generation: core::cell::Cell::new(0),
                scanout_epoch: core::cell::Cell::new(0),
                direct_scanout_allocations: core::cell::RefCell::new(Vec::new()),
                scanout_copy_count: core::cell::Cell::new(0),
                h_rt_core_layer: create.hRTCoreLayer.handle,
                um_callbacks: p_um_callbacks.cast(),
                flip_wait_state: core::cell::Cell::new(0),
                flip_wait_fence: core::cell::Cell::new(0),
                flip_wait_next_value: core::cell::Cell::new(0),
            },
        );
    }

    // From here on every early return must tear down. The guard does it, so it
    // is not something each new failure arm has to remember.
    let guard = DeviceUnderConstruction {
        dev: create.hDrvDevice.pDrvPrivate as *mut device_funcs::HeliosDevice,
    };

    unsafe {
        let dev = &mut *(create.hDrvDevice.pDrvPrivate as *mut device_funcs::HeliosDevice);
        let context_hr = device_funcs::create_runtime_context(dev);
        if context_hr != S_OK {
            log_error!(
                "  CreateDevice: kernel context creation failed hr=0x{:08x}",
                context_hr as u32
            );
            return context_hr;
        }
        let paging_hr = device_funcs::create_runtime_paging_queue(dev);
        if paging_hr != S_OK {
            log_error!(
                "  CreateDevice: paging queue creation failed hr=0x{:08x}",
                paging_hr as u32
            );
            return paging_hr;
        }
    }

    // 3) Fill the device-funcs table (Interface == D3D11_0 -> p11DeviceFuncs) and
    //    the DXGI base DDI table the runtime handed us.
    log_error!(
        "  CreateDevice: filling {} device-funcs table",
        negotiated.name()
    );
    // Each arm now names the union member the negotiated interface selects,
    // instead of casting one `*mut c_void` to a different table type per arm.
    // The pointer value is the same either way (all members alias at offset 0);
    // what changes is that the member name, the fill function and the interface
    // are readable as one triple, which is the R802 defect -- an editor could
    // previously pair `D3D11_1` with `fill_d3d11_device_funcs` and the cast
    // would still compile.
    unsafe {
        match negotiated {
            NegotiatedInterface::Wddm1_3 => {
                device_funcs::fill_wddm1_3_device_funcs(
                    create.__bindgen_anon_1.pWDDM1_3DeviceFuncs,
                );
                device_funcs::fill_dxgi_1_3_base_funcs(
                    create.DXGIBaseDDI.__bindgen_anon_1.pDXGIDDIBaseFunctions4,
                );
            }
            NegotiatedInterface::D3D11_1 => {
                device_funcs::fill_d3d11_1_device_funcs(create.__bindgen_anon_1.p11_1DeviceFuncs);
                device_funcs::fill_dxgi_1_1_base_funcs(
                    create.DXGIBaseDDI.__bindgen_anon_1.pDXGIDDIBaseFunctions2,
                );
            }
            NegotiatedInterface::D3D11_0 => {
                device_funcs::fill_d3d11_device_funcs(create.__bindgen_anon_1.p11DeviceFuncs);
                device_funcs::fill_dxgi_base_funcs(
                    create.DXGIBaseDDI.__bindgen_anon_1.pDXGIDDIBaseFunctions,
                );
            }
        }
    }

    // The device is handed to the runtime from here; it owns teardown through
    // DestroyDevice.
    guard.defuse();
    // Record it live for `helios_umd_wait_last_present`, which dereferences a
    // device pointer the ICD recorded on an earlier call (R415).
    forward::register_live_device(create.hDrvDevice.pDrvPrivate as usize);

    if std::env::var_os("HELIOS_DXGI_NO_REDIRECTION").is_some() {
        log_error!("  CreateDevice -> DXGI_STATUS_NO_REDIRECTION (env-gated; DXGI desktop fallback)");
        DXGI_STATUS_NO_REDIRECTION
    } else {
        log_error!("  CreateDevice -> S_OK (DXVK device + D3D11 funcs table installed)");
        S_OK
    }
}

/// Owns the in-place-constructed `HeliosDevice` for the rest of `CreateDevice`.
/// Any early return after construction tears down through `Drop`; the success
/// path calls [`Self::defuse`] immediately before returning to the runtime.
/// The compiler enforces it, rather than each failure arm remembering to —
/// which is exactly what the two hoisted null checks did not do.
///
/// Teardown order matches the paging-queue rollback it replaces:
/// `destroy_runtime_objects` (kernel context + paging queue, through the
/// runtime callbacks) first, then `drop_in_place` (the DXVK device and the
/// Rust-owned fields).
struct DeviceUnderConstruction {
    dev: *mut device_funcs::HeliosDevice,
}

impl DeviceUnderConstruction {
    fn defuse(mut self) {
        self.dev = core::ptr::null_mut();
    }
}

impl Drop for DeviceUnderConstruction {
    fn drop(&mut self) {
        if self.dev.is_null() {
            return;
        }
        // SAFETY: `dev` points at the runtime-owned private block this function
        // wrote a `HeliosDevice` into with `core::ptr::write`, and it has not
        // been dropped. The guard is the only owner while it is alive — the
        // runtime does not see the handle until `defuse()` runs — so no other
        // reference exists during teardown.
        unsafe {
            // The CreateDevice rollback path is the second place BridgeOwned
            // must be released explicitly (R807): reaching it means the bridge
            // device exists but the runtime never saw the handle, and letting
            // the refs go out with `drop_in_place` would put them back on the
            // drop order this type exists to stop depending on.
            let (variants, layouts) = (*self.dev).owned.release();
            log_error!(
                "CreateDevice rollback: released IA cache variants={} layouts={}",
                variants, layouts
            );
            device_funcs::destroy_runtime_objects(&mut *self.dev);
            core::ptr::drop_in_place(self.dev);
        }
    }
}

unsafe extern "C" fn close_adapter(_h_adapter: ddi::D3D10DDI_HADAPTER) -> Hresult {
    log_error!("CloseAdapter");
    S_OK
}

unsafe extern "C" fn get_supported_versions(
    _h_adapter: ddi::D3D10DDI_HADAPTER,
    entries: *mut u32,
    supported_versions: *mut u64,
) -> Hresult {
    if entries.is_null() {
        log_error!("GetSupportedVersions: null entries -> E_NOTIMPL");
        return E_NOTIMPL;
    }

    let requested_entries = unsafe { *entries };
    log_error!(
        "GetSupportedVersions requested={requested_entries} bufNull={} (advertising {:#018x?})",
        supported_versions.is_null(),
        SUPPORTED_DDI_VERSIONS,
    );
    unsafe { *entries = SUPPORTED_DDI_VERSIONS.len() as u32 };

    if supported_versions.is_null() {
        return S_OK;
    }

    if requested_entries < SUPPORTED_DDI_VERSIONS.len() as u32 {
        return E_OUTOFMEMORY;
    }

    for (index, version) in SUPPORTED_DDI_VERSIONS.iter().enumerate() {
        unsafe { *supported_versions.add(index) = *version };
    }
    S_OK
}

unsafe extern "C" fn get_caps(
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
                    unsafe { *(args.pData as *mut u32) = 0 };
                    log_error!("  GetCaps: THREADING caps = 0");
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

/// Resolve the per-process UMD log path, computed once.
///
/// The restricted IddCx host process (which opens the IDD swapchain surface)
/// cannot write `C:\Windows\Temp\helios_umd.log` — that directory's ACL only
/// grants SYSTEM/Administrators, so the IDD process's log lines vanished. We log
/// to a per-pid file under `C:\ProgramData\Helios\` instead: standard users may
/// create files there (inherited ProgramData ACL), and a per-pid name means each
/// process owns its own file with full control regardless of who created the dir.
pub(crate) fn umd_log_path() -> &'static std::path::Path {
    use std::sync::OnceLock;
    static PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = std::path::Path::new(r"C:\ProgramData\Helios");
        // Best effort: ignore AlreadyExists / permission errors.
        let _ = std::fs::create_dir_all(dir);
        dir.join(format!("umd-{}.log", std::process::id()))
    })
}

/// The unconditional log writer.
///
/// DO NOT CALL DIRECTLY — it is `#[deprecated]` purely as an internal marker so
/// the compiler enforces that. Use one of the two macros, which is the whole
/// point of R420's static guarantee: the choice between "this is an error, a
/// one-shot or a refusal" and "this is per-op repeat traffic" has to be made
/// explicitly at every site, and a new per-op site cannot reach the
/// unconditional writer by accident.
///
/// - [`log_error!`] — errors, one-shots, refusals. Always written.
/// - [`trace_line!`] — per-op repeat traffic. `UmdTrace`-gated, and it does not
///   even evaluate its arguments when the knob is off.
#[deprecated(note = "use log_error! (errors/one-shots/refusals) or trace_line! (per-op traffic)")]
pub(crate) fn log_line(message: &str) {
    use std::sync::{Mutex, OnceLock};
    // One handle per process: the old open/append/close-per-line pattern cost
    // a full CreateFile round trip on every logged DDI call — measurable on
    // per-frame paths (PSC WS2). Unbuffered File writes keep crash durability.
    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    let file = FILE.get_or_init(|| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(umd_log_path())
            .ok()
            .map(Mutex::new)
    });
    if let Some(lock) = file {
        if let Ok(mut f) = lock.lock() {
            let _ = writeln!(f, "[pid={}] {}", std::process::id(), message);
        }
    }
}

/// Whether per-frame/per-op DDI chatter (`trace_line!`) is enabled:
/// `HKLM\SOFTWARE\Helios!UmdTrace` (REG_DWORD) != 0. Read once per process.
/// Errors, one-shots and refusals keep using [`log_line`] unconditionally —
/// only known-hot repeat traffic (Present, OMSetRenderTargets,
/// ResolveSharedResource, per-op stamps) sits behind this gate.
pub(crate) fn trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        #[link(name = "advapi32")]
        unsafe extern "system" {
            fn RegGetValueA(
                hkey: usize,
                sub_key: *const u8,
                value: *const u8,
                flags: u32,
                type_out: *mut u32,
                data: *mut c_void,
                data_len: *mut u32,
            ) -> i32;
        }
        const HKEY_LOCAL_MACHINE: usize = 0x8000_0002;
        const RRF_RT_REG_DWORD: u32 = 0x10;
        let mut value: u32 = 0;
        let mut len: u32 = 4;
        // SAFETY: NUL-terminated key/value names; `value`/`len` outlive the call.
        let rc = unsafe {
            RegGetValueA(
                HKEY_LOCAL_MACHINE,
                c"SOFTWARE\\Helios".as_ptr().cast(),
                c"UmdTrace".as_ptr().cast(),
                RRF_RT_REG_DWORD,
                core::ptr::null_mut(),
                (&mut value as *mut u32).cast(),
                &mut len,
            )
        };
        rc == 0 && value != 0
    })
}

/// Selects whether the adapter advertises the full D3D11 feature-level profile
/// or the conservative FL10_0 fallback:
/// `HKLM\SOFTWARE\Helios!FeatureLevel11` (REG_DWORD). Absent = full FL11
/// profile; explicit 0 = FL10_0 opt-out. Read once
/// per process, so an already-running dwm keeps the level it created its
/// device at while freshly-launched apps pick up the new value.
///
/// This gate MUST cover the three caps together — the 3DPIPELINESUPPORT
/// pipeline level, `check_format_support`'s multisample bits, and
/// `CheckMultisampleQualityLevels` — because the Microsoft runtime validates
/// them as one coherent feature-level contract during
/// `CDevice::LLOCompleteLayerConstruction`; a partial change is rejected with
/// DXGI_ERROR_UNSUPPORTED. FL11_0 additionally requires real multisample
/// support, which the FL10_0 profile deliberately suppresses.
///
/// 30th/31st-session ETW evidence (Microsoft-Windows-DXGI) showed this is a UMD
/// caps sequence, not a KMD/adapter ceiling: the runtime reaches
/// CreateDevice/venus CTX_CREATE and rejects each bad caps contract with a
/// concrete string. Gates fixed so far: 3DPIPELINESUPPORT is a bitmask,
/// SHADER compute cap is 0x2, and MSAA/format support must match D3D11.3
/// §19.2.5. knob=0 remains the exact FL10_0 baseline opt-out for A/B.
///   absent = full FL11 profile
///   0 = FL10_0 profile
///   1 = full FL11_0 (pipeline 11_0 + real MSAA + unmasked format bits)
///   2 = DIAGNOSTIC: pipeline claims 11_0 but keeps the FL10 MSAA/format caps —
///       isolates pipeline-level validation from the later FL11 caps gates.
pub(crate) fn feature_level_mode() -> u32 {
    use std::sync::OnceLock;
    static MODE: OnceLock<u32> = OnceLock::new();
    *MODE.get_or_init(|| {
        #[link(name = "advapi32")]
        unsafe extern "system" {
            fn RegGetValueA(
                hkey: usize,
                sub_key: *const u8,
                value: *const u8,
                flags: u32,
                type_out: *mut u32,
                data: *mut c_void,
                data_len: *mut u32,
            ) -> i32;
        }
        const HKEY_LOCAL_MACHINE: usize = 0x8000_0002;
        const RRF_RT_REG_DWORD: u32 = 0x10;
        let mut value: u32 = 0;
        let mut len: u32 = 4;
        // SAFETY: NUL-terminated key/value names; `value`/`len` outlive the call.
        let rc = unsafe {
            RegGetValueA(
                HKEY_LOCAL_MACHINE,
                c"SOFTWARE\\Helios".as_ptr().cast(),
                c"FeatureLevel11".as_ptr().cast(),
                RRF_RT_REG_DWORD,
                core::ptr::null_mut(),
                (&mut value as *mut u32).cast(),
                &mut len,
            )
        };
        if rc == 0 {
            value
        } else {
            1
        }
    })
}

/// Present-path frame-completion gate cap in microseconds:
/// `HKLM\SOFTWARE\Helios!PresentGateUs` (REG_DWORD). Read once per process.
/// Absent = 10000 for the direct-primary display path. `context.Flush()` can
/// return before DXVK's submission thread has entered the matching Venus work,
/// so the KMD cannot capture that future command in its completion watermark.
/// This bounded, condition-variable-backed gate closes that producer race
/// without restoring the old 32 ms polling throttle. 0 remains the A/B disable.
pub(crate) fn present_gate_us() -> u32 {
    use std::sync::OnceLock;
    static VALUE: OnceLock<u32> = OnceLock::new();
    *VALUE.get_or_init(|| {
        #[link(name = "advapi32")]
        unsafe extern "system" {
            fn RegGetValueA(
                hkey: usize,
                sub_key: *const u8,
                value: *const u8,
                flags: u32,
                type_out: *mut u32,
                data: *mut c_void,
                data_len: *mut u32,
            ) -> i32;
        }
        const HKEY_LOCAL_MACHINE: usize = 0x8000_0002;
        const RRF_RT_REG_DWORD: u32 = 0x10;
        const DEFAULT_US: u32 = 10000;
        let mut value: u32 = 0;
        let mut len: u32 = 4;
        // SAFETY: NUL-terminated key/value names; `value`/`len` outlive the call.
        let rc = unsafe {
            RegGetValueA(
                HKEY_LOCAL_MACHINE,
                c"SOFTWARE\\Helios".as_ptr().cast(),
                c"PresentGateUs".as_ptr().cast(),
                RRF_RT_REG_DWORD,
                core::ptr::null_mut(),
                (&mut value as *mut u32).cast(),
                &mut len,
            )
        };
        if rc == 0 {
            value
        } else {
            DEFAULT_US
        }
    })
}

/// Dcomp-vehicle flip-ordering gate cap in microseconds:
/// `HKLM\SOFTWARE\Helios!VehicleFlipGateUs` (REG_DWORD). Read once per
/// process. Absent = 32000; 0 disables (A/B lever). Bounds the worker-side
/// wait for the vehicle frame COPY's host-GPU completion before the flip is
/// minted: a direct/independent-flip present is ordered only on the KMD's
/// DMA fence, which completes at DECODE — without this gate the backbuffer
/// scans out before the venus copy lands and the previous occupant of the
/// buffer pops out (the 24th-session gameplay stutter). Composed presents
/// are protected by dwm's consumer wait either way; direct flip is not.
pub(crate) fn vehicle_flip_gate_us() -> u32 {
    use std::sync::OnceLock;
    static VALUE: OnceLock<u32> = OnceLock::new();
    *VALUE.get_or_init(|| {
        #[link(name = "advapi32")]
        unsafe extern "system" {
            fn RegGetValueA(
                hkey: usize,
                sub_key: *const u8,
                value: *const u8,
                flags: u32,
                type_out: *mut u32,
                data: *mut c_void,
                data_len: *mut u32,
            ) -> i32;
        }
        const HKEY_LOCAL_MACHINE: usize = 0x8000_0002;
        const RRF_RT_REG_DWORD: u32 = 0x10;
        const DEFAULT_US: u32 = 32_000;
        let mut value: u32 = 0;
        let mut len: u32 = 4;
        // SAFETY: NUL-terminated key/value names; `value`/`len` outlive the call.
        let rc = unsafe {
            RegGetValueA(
                HKEY_LOCAL_MACHINE,
                c"SOFTWARE\\Helios".as_ptr().cast(),
                c"VehicleFlipGateUs".as_ptr().cast(),
                RRF_RT_REG_DWORD,
                core::ptr::null_mut(),
                (&mut value as *mut u32).cast(),
                &mut len,
            )
        };
        if rc == 0 {
            value
        } else {
            DEFAULT_US
        }
    })
}

/// Kernel-enforced vehicle flip ordering:
/// `HKLM\SOFTWARE\Helios!VehicleKernelFlipWait` (REG_DWORD). Read once per
/// process. **Absent = 1 (ON — default flipped 28th session)**: vehicle
/// presents queue a dxgkrnl GPU-side wait on the copy's completion (a
/// runtime-device monitored fence CPU-signaled when the present fence
/// reaches the copy's value) AHEAD of the present packet — the flip
/// physically cannot execute before the venus copy lands, closing the
/// bounded CPU gate's timeout leak AND retiring the worker-serial flip
/// gate (5.6 ms/present measured under Doom). =0 = kill switch (bounded
/// CPU gate serves instead). It also underwrites the consumers'
/// skip-if-unretired refresh (the kwait bit in the present-sync slot).
/// History: the first integration (2026-07-07) wedged the guest — root
/// cause was HardwareAccess=1 escapes convoying dxgkrnl's core resource
/// against the kwait-parked queue, killed in the 26th session (escapes
/// HardwareAccess=0 + exported-sem fold + signalTo monotonicity guard);
/// since then 24k probe + 6k Doom + 4.6k vkcube presents, zero
/// wedges/arm fails.
pub(crate) fn vehicle_kernel_flip_wait() -> bool {
    use std::sync::OnceLock;
    static VALUE: OnceLock<bool> = OnceLock::new();
    *VALUE.get_or_init(|| {
        #[link(name = "advapi32")]
        unsafe extern "system" {
            fn RegGetValueA(
                hkey: usize,
                sub_key: *const u8,
                value: *const u8,
                flags: u32,
                type_out: *mut u32,
                data: *mut c_void,
                data_len: *mut u32,
            ) -> i32;
        }
        const HKEY_LOCAL_MACHINE: usize = 0x8000_0002;
        const RRF_RT_REG_DWORD: u32 = 0x10;
        let mut value: u32 = 0;
        let mut len: u32 = 4;
        // SAFETY: NUL-terminated key/value names; `value`/`len` outlive the call.
        let rc = unsafe {
            RegGetValueA(
                HKEY_LOCAL_MACHINE,
                c"SOFTWARE\\Helios".as_ptr().cast(),
                c"VehicleKernelFlipWait".as_ptr().cast(),
                RRF_RT_REG_DWORD,
                core::ptr::null_mut(),
                (&mut value as *mut u32).cast(),
                &mut len,
            )
        };
        // Absent/unreadable = ON; an explicit 0 is the kill switch.
        rc != 0 || value != 0
    })
}

/// Legacy IddCx producer switch: `HKLM\SOFTWARE\Helios!PresentSyncPublish`
/// (REG_DWORD). Read once per process. Absent = 0 because the real Helios
/// display adapter has no cross-process IDD consumer. Enabling it creates a
/// Win32 external semaphore and an extra D3DKMTEscape on every frame.
pub(crate) fn present_sync_publish_enabled() -> bool {
    use std::sync::OnceLock;
    static VALUE: OnceLock<bool> = OnceLock::new();
    *VALUE.get_or_init(|| {
        #[link(name = "advapi32")]
        unsafe extern "system" {
            fn RegGetValueA(
                hkey: usize,
                sub_key: *const u8,
                value: *const u8,
                flags: u32,
                type_out: *mut u32,
                data: *mut c_void,
                data_len: *mut u32,
            ) -> i32;
        }
        const HKEY_LOCAL_MACHINE: usize = 0x8000_0002;
        const RRF_RT_REG_DWORD: u32 = 0x10;
        let mut value: u32 = 0;
        let mut len: u32 = 4;
        // SAFETY: NUL-terminated key/value names; `value`/`len` outlive the call.
        let rc = unsafe {
            RegGetValueA(
                HKEY_LOCAL_MACHINE,
                c"SOFTWARE\\Helios".as_ptr().cast(),
                c"PresentSyncPublish".as_ptr().cast(),
                RRF_RT_REG_DWORD,
                core::ptr::null_mut(),
                (&mut value as *mut u32).cast(),
                &mut len,
            )
        };
        rc == 0 && value != 0
    })
}

/// Per-frame/per-op trace logging, gated by [`trace_enabled`]. The format
/// arguments are not evaluated when tracing is off.
macro_rules! trace_line {
    ($($arg:tt)*) => {
        if crate::trace_enabled() {
            #[allow(deprecated)]
            crate::log_line(&format!($($arg)*));
        }
    };
}
pub(crate) use trace_line;

/// Unconditional log line: errors, one-shots and refusals ONLY.
///
/// The counterpart to [`trace_line!`]. Per-op repeat traffic must not use this
/// — that is what put a 21-argument `format!` plus a mutex-guarded unbuffered
/// write on all seven draw entry points and on the caps-query path (R420).
macro_rules! log_error {
    ($($arg:tt)*) => {{
        #[allow(deprecated)]
        crate::log_line(&format!($($arg)*));
    }};
}
pub(crate) use log_error;

/// Log which DLL file THIS code is running from, once per process. Multiple
/// UMD copies exist on disk (DriverStore package, ProgramData versioned
/// copies) and boot-time resolution has served stale builds before (a stray
/// pre-typed-signature FileRepository\helios_umd.dll caused cold-boot dwm
/// devices to run old shader handlers, 2026-07-04) — the per-pid log alone
/// cannot distinguish which copy handled which device.
fn log_self_module_path() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if LOGGED.swap(true, Ordering::Relaxed) {
        return;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleExW(flags: u32, module_name: *const u16, module: *mut *mut c_void)
            -> i32;
        fn GetModuleFileNameW(module: *mut c_void, filename: *mut u16, size: u32) -> u32;
    }
    const FROM_ADDRESS: u32 = 0x4; // GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
    const UNCHANGED_REFCOUNT: u32 = 0x2; // ..._UNCHANGED_REFCOUNT
    unsafe {
        let mut hmod: *mut c_void = core::ptr::null_mut();
        let anchor = log_self_module_path as *const ();
        if GetModuleHandleExW(
            FROM_ADDRESS | UNCHANGED_REFCOUNT,
            anchor as *const u16,
            &mut hmod,
        ) != 0
        {
            let mut buf = [0u16; 512];
            let n = GetModuleFileNameW(hmod, buf.as_mut_ptr(), buf.len() as u32) as usize;
            if n > 0 && n < buf.len() {
                log_error!(
                    "UMD module: {}",
                    String::from_utf16_lossy(&buf[..n])
                );
                return;
            }
        }
        log_error!("UMD module: <unresolvable>");
    }
}
