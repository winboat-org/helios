//! Helios WDDM user-mode display driver bring-up DLL.
//!
//! This is not a D3D implementation yet. It gives the display package a real
//! UMD DLL with the WDDM adapter-open exports that the OS/runtime can resolve.
//! Until the DXVK/VKD3D-backed adapter/device path exists, device creation still
//! fails explicitly. The adapter-open handshake itself must succeed because
//! dxgkrnl calls it during render-adapter start validation.

use core::ffi::c_void;
use std::io::Write;

mod bridge;

type Hresult = i32;

const S_OK: Hresult = 0;
const E_NOTIMPL: Hresult = 0x8000_4001u32 as i32;
const E_OUTOFMEMORY: Hresult = 0x8007_000eu32 as i32;

const fn ddi_supported(major: u64, minor: u64, build: u64) -> u64 {
    let interface = (major << 16) | minor;
    (interface << 32) | (build << 16)
}

// DWM-CRASH EXPERIMENT (2026-06-18): empty = "this adapter supports no D3D
// device DDI version yet". The runtime queries this after OpenAdapter; with a
// non-empty (and partly fabricated) list it then picks a version and calls
// CreateDevice, which honestly returns E_NOTIMPL — and DWM (observed calling
// CreateDevice on Helios, pids 1896/10604) crash-loops in dwmcore on that
// advertise-render-caps-but-fail-CreateDevice inconsistency. Returning zero
// supported versions is the honest state until the device funcs table is real
// (Gate 5b): the runtime should then NOT attempt CreateDevice and skip Helios
// for D3D, which DWM handles gracefully. Restore real entries when CreateDevice
// is backed. The Gate-5a venus path uses D3DKMT thunks directly and does NOT
// depend on this list, so emptying it is safe for Gate 5.
const SUPPORTED_DDI_VERSIONS: &[u64] = &[];

#[repr(C)]
#[derive(Copy, Clone)]
pub struct D3d10DdiAdapterHandle {
    pub p_drv_private: *mut c_void,
}

#[repr(C)]
pub struct D3d10DdiArgOpenAdapter {
    pub h_rt_adapter: *mut c_void,
    pub h_adapter: D3d10DdiAdapterHandle,
    pub interface: u32,
    pub version: u32,
    pub p_adapter_callbacks: *const c_void,
    pub p_adapter_funcs: *mut c_void,
}

#[repr(C)]
pub struct D3d10DdiAdapterFuncs {
    pub pfn_calc_private_device_size:
        Option<unsafe extern "system" fn(D3d10DdiAdapterHandle, *const c_void) -> usize>,
    pub pfn_create_device:
        Option<unsafe extern "system" fn(D3d10DdiAdapterHandle, *mut c_void) -> Hresult>,
    pub pfn_close_adapter: Option<unsafe extern "system" fn(D3d10DdiAdapterHandle) -> Hresult>,
}

#[repr(C)]
pub struct D3d10_2DdiAdapterFuncs {
    pub base: D3d10DdiAdapterFuncs,
    pub pfn_get_supported_versions:
        Option<unsafe extern "system" fn(D3d10DdiAdapterHandle, *mut u32, *mut u64) -> Hresult>,
    pub pfn_get_caps: Option<
        unsafe extern "system" fn(D3d10DdiAdapterHandle, *const D3d10_2DdiArgGetCaps) -> Hresult,
    >,
}

#[repr(C)]
pub struct D3d10_2DdiArgGetCaps {
    pub caps_type: u32,
    pub p_info: *mut c_void,
    pub p_data: *mut c_void,
    pub data_size: u32,
}

/// `DXGI_DDI_BASE_ARGS` (dxgiddi.h): the runtime hands `pDXGIBaseCallbacks` (in)
/// and expects the driver to fill the `pDXGIDDIBaseFunctions*` union member
/// (in/out) with the DXGI base entry points. A real `CreateDevice` MUST fill it.
#[repr(C)]
pub struct DxgiDdiBaseArgs {
    pub p_dxgi_base_callbacks: *const c_void,
    pub p_dxgi_ddi_base_functions: *mut c_void,
}

/// `D3D10DDIARG_CREATEDEVICE` (d3d10umddi.h, WDK 10.0.26100, x64), laid out
/// field-for-field so we can read the negotiated `Interface`/`Version`/`Flags`
/// the runtime asks Helios for. The function-pointer union (`pDeviceFuncs` and
/// friends) is one pointer; which device-funcs table it points at is selected by
/// `interface` (e.g. `D3D11_0_DDI_INTERFACE_VERSION` -> `p11DeviceFuncs`).
///
/// Offsets (x64): hRTDevice@0, interface@8, version@12, pKTCallbacks@16,
/// pDeviceFuncs@24, hDrvDevice@32, DXGIBaseDDI@40 (16B), hRTCoreLayer@56,
/// pUMCallbacks@64, flags@72, ppfnRetrieveSubObject@80 (minor>=3).
#[repr(C)]
pub struct D3d10DdiArgCreateDevice {
    pub h_rt_device: *mut c_void,
    pub interface: u32,
    pub version: u32,
    pub p_kt_callbacks: *const c_void,
    pub p_device_funcs: *mut c_void,
    pub h_drv_device: *mut c_void,
    pub dxgi_base_ddi: DxgiDdiBaseArgs,
    pub h_rt_core_layer: *mut c_void,
    pub p_um_callbacks: *const c_void,
    pub flags: u32,
    pub ppfn_retrieve_sub_object: *mut c_void,
}

#[repr(C)]
pub struct D3d12DdiArgOpenAdapter {
    pub h_rt_adapter: *mut c_void,
    pub h_adapter: D3d10DdiAdapterHandle,
    pub p_adapter_callbacks: *const c_void,
    pub p_adapter_funcs: *mut D3d12DdiAdapterFuncs,
}

#[repr(C)]
pub struct D3d12DdiAdapterFuncs {
    pub pfn_calc_private_device_size:
        Option<unsafe extern "system" fn(D3d10DdiAdapterHandle, *const c_void) -> usize>,
    pub pfn_create_device:
        Option<unsafe extern "system" fn(D3d10DdiAdapterHandle, *mut c_void) -> Hresult>,
    pub pfn_close_adapter: Option<unsafe extern "system" fn(D3d10DdiAdapterHandle) -> Hresult>,
    pub pfn_get_supported_versions:
        Option<unsafe extern "system" fn(D3d10DdiAdapterHandle, *mut u32, *mut u64) -> Hresult>,
    pub pfn_get_caps: Option<
        unsafe extern "system" fn(D3d10DdiAdapterHandle, *const D3d10_2DdiArgGetCaps) -> Hresult,
    >,
    pub pfn_get_optional_ddi_tables: *const c_void,
    pub pfn_fill_ddi_table: *const c_void,
    pub pfn_destroy_device: *const c_void,
}

static mut ADAPTER_COOKIE: usize = 0x4845_4c49_4f53_554d; // "HELIOSUM"

/// TEMPORARY (Gate 5b bring-up): out-of-band smoke test of the DXVK bridge from a
/// normal process — no WDDM/INF/devcon/DWM involvement. Returns 0 if DXVK brought
/// up a logical device on the venus adapter, 1 otherwise. Remove once the DDI path
/// is validated end-to-end.
#[no_mangle]
pub extern "system" fn helios_umd_selftest() -> i32 {
    log_line("helios_umd_selftest: creating DXVK device on venus...");
    let dev = bridge::ffi::helios_dxvk_create_device(0, 0);
    let ok = !dev.is_null();
    log_line(&format!("helios_umd_selftest: result ok={ok}"));
    drop(dev);
    if ok {
        0
    } else {
        1
    }
}

#[no_mangle]
pub unsafe extern "system" fn OpenAdapter10(open_data: *mut D3d10DdiArgOpenAdapter) -> Hresult {
    log_line("OpenAdapter10");
    unsafe { open_adapter_common(open_data, false) }
}

#[no_mangle]
pub unsafe extern "system" fn OpenAdapter10_2(open_data: *mut D3d10DdiArgOpenAdapter) -> Hresult {
    log_line("OpenAdapter10_2");
    unsafe { open_adapter_common(open_data, true) }
}

#[no_mangle]
pub unsafe extern "system" fn OpenAdapter12(open_data: *mut D3d12DdiArgOpenAdapter) -> Hresult {
    log_line("OpenAdapter12");
    if open_data.is_null() {
        log_line("OpenAdapter12 null open_data");
        return E_NOTIMPL;
    }

    let open = unsafe { &mut *open_data };
    if open.p_adapter_funcs.is_null() {
        log_line("OpenAdapter12 null p_adapter_funcs");
        return E_NOTIMPL;
    }

    open.h_adapter = D3d10DdiAdapterHandle {
        p_drv_private: core::ptr::addr_of_mut!(ADAPTER_COOKIE).cast::<c_void>(),
    };

    let funcs = unsafe { &mut *open.p_adapter_funcs };
    funcs.pfn_calc_private_device_size = Some(calc_private_device_size);
    funcs.pfn_create_device = Some(create_device);
    funcs.pfn_close_adapter = Some(close_adapter);
    funcs.pfn_get_supported_versions = Some(get_supported_versions);
    funcs.pfn_get_caps = Some(get_caps);
    funcs.pfn_get_optional_ddi_tables = core::ptr::null();
    funcs.pfn_fill_ddi_table = core::ptr::null();
    funcs.pfn_destroy_device = core::ptr::null();

    S_OK
}

unsafe fn open_adapter_common(open_data: *mut D3d10DdiArgOpenAdapter, with_10_2: bool) -> Hresult {
    if open_data.is_null() {
        log_line("open_adapter_common null open_data");
        return E_NOTIMPL;
    }

    let open = unsafe { &mut *open_data };
    if open.p_adapter_funcs.is_null() {
        log_line("open_adapter_common null p_adapter_funcs");
        return E_NOTIMPL;
    }
    log_line(&format!(
        "open_adapter_common interface=0x{:08x} version=0x{:08x} with_10_2={}",
        open.interface, open.version, with_10_2
    ));

    open.h_adapter = D3d10DdiAdapterHandle {
        p_drv_private: core::ptr::addr_of_mut!(ADAPTER_COOKIE).cast::<c_void>(),
    };

    if with_10_2 {
        let funcs = unsafe { &mut *(open.p_adapter_funcs.cast::<D3d10_2DdiAdapterFuncs>()) };
        funcs.base.pfn_calc_private_device_size = Some(calc_private_device_size);
        funcs.base.pfn_create_device = Some(create_device);
        funcs.base.pfn_close_adapter = Some(close_adapter);
        funcs.pfn_get_supported_versions = Some(get_supported_versions);
        funcs.pfn_get_caps = Some(get_caps);
    } else {
        let funcs = unsafe { &mut *(open.p_adapter_funcs.cast::<D3d10DdiAdapterFuncs>()) };
        funcs.pfn_calc_private_device_size = Some(calc_private_device_size);
        funcs.pfn_create_device = Some(create_device);
        funcs.pfn_close_adapter = Some(close_adapter);
    }

    S_OK
}

unsafe extern "system" fn calc_private_device_size(
    _h_adapter: D3d10DdiAdapterHandle,
    _args: *const c_void,
) -> usize {
    log_line("CalcPrivateDeviceSize -> 0");
    0
}

unsafe extern "system" fn create_device(
    _h_adapter: D3d10DdiAdapterHandle,
    args: *mut c_void,
) -> Hresult {
    // SAFETY: the runtime passes a valid `D3D10DDIARG_CREATEDEVICE*` per the
    // `PFND3D10DDI_CREATEDEVICE` contract; we only read scalar/pointer fields and
    // never write through it, so an E_NOTIMPL return leaves the runtime's state
    // untouched. We null-check defensively.
    if args.is_null() {
        log_line("CreateDevice null args -> E_NOTIMPL");
        return E_NOTIMPL;
    }
    let create = unsafe { &*(args as *const D3d10DdiArgCreateDevice) };
    log_line(&format!(
        "CreateDevice interface=0x{:08x} version=0x{:08x} flags=0x{:08x} \
         pDeviceFuncs={:p} pKTCallbacks={:p} pUMCallbacks={:p} pDXGIBaseFuncs={:p}",
        create.interface,
        create.version,
        create.flags,
        create.p_device_funcs,
        create.p_kt_callbacks,
        create.p_um_callbacks,
        create.dxgi_base_ddi.p_dxgi_ddi_base_functions,
    ));

    // Gate 5b smoke test: bring up a DXVK device on the Helios venus adapter from
    // inside the D3D runtime's CreateDevice path. We do NOT yet fill the device
    // funcs table (that's the Milestone-1 work) — returning S_OK with a half-baked
    // table corrupts DWM/Explorer (the .52/.53 lesson). So we still return
    // E_NOTIMPL, but prove the engine round-trips venus here first.
    let dxvk_device = bridge::ffi::helios_dxvk_create_device(0, 0);
    if dxvk_device.is_null() {
        log_line("  bridge: DXVK device creation FAILED (see helios_umd.log dxvk-bridge lines)");
    } else {
        log_line("  bridge: DXVK device created on venus OK (engine reachable from UMD)");
    }
    drop(dxvk_device);

    log_line("  -> E_NOTIMPL (Gate 5b: device funcs table not yet backed; honest failure)");
    E_NOTIMPL
}

unsafe extern "system" fn close_adapter(_h_adapter: D3d10DdiAdapterHandle) -> Hresult {
    log_line("CloseAdapter");
    S_OK
}

unsafe extern "system" fn get_supported_versions(
    _h_adapter: D3d10DdiAdapterHandle,
    entries: *mut u32,
    supported_versions: *mut u64,
) -> Hresult {
    log_line("GetSupportedVersions");
    if entries.is_null() {
        return E_NOTIMPL;
    }

    let requested_entries = unsafe { *entries };
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

unsafe extern "system" fn get_caps(
    _h_adapter: D3d10DdiAdapterHandle,
    args: *const D3d10_2DdiArgGetCaps,
) -> Hresult {
    log_line("GetCaps");
    if !args.is_null() {
        let args = unsafe { &*args };
        if !args.p_data.is_null() && args.data_size != 0 {
            unsafe { core::ptr::write_bytes(args.p_data as *mut u8, 0, args.data_size as usize) };
        }
    }
    S_OK
}

fn log_line(message: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(r"C:\Windows\Temp\helios_umd.log")
    {
        let _ = writeln!(file, "[pid={}] {}", std::process::id(), message);
    }
}
