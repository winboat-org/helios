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
mod ddi;
mod device_funcs;
mod forward;

type Hresult = i32;

const S_OK: Hresult = 0;
const E_FAIL: Hresult = 0x8000_4005u32 as i32;
const E_NOTIMPL: Hresult = 0x8000_4001u32 as i32;
const E_OUTOFMEMORY: Hresult = 0x8007_000eu32 as i32;
const DXGI_ERROR_UNSUPPORTED: Hresult = 0x887a_0020u32 as i32;

const fn ddi_supported(major: u64, minor: u64, build: u64) -> u64 {
    let interface = (major << 16) | minor;
    (interface << 32) | (build << 16)
}

// Advertise D3D11.1 first so the runtime exposes DXGI 1.1 resource-sharing
// DDIs (notably ResolveSharedResource) and the extended resource-sharing path
// DWM/IddCx require. Keep D3D11.0 as a fallback for older/runtime-selected paths.
const SUPPORTED_DDI_VERSIONS: &[u64] = &[
    ddi_supported(11, 15, 0), // D3D11_1_DDI_SUPPORTED
    ddi_supported(11, 10, 2), // D3D11_0_DDI_SUPPORTED
];

const D3D12_SUPPORTED_DDI_VERSIONS: &[u64] = &[
    // D3D12DDI_SUPPORTED_0003 from WDK 10.0.26100 d3d12umddi.h:
    // interface ((12 << 16) | 2), build 8.
    ddi_supported(12, 2, 8),
];

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
    pub pfn_calc_private_device_size: Option<
        unsafe extern "system" fn(
            D3d10DdiAdapterHandle,
            *const D3d12DdiArgCalcPrivateDeviceSize,
        ) -> usize,
    >,
    pub pfn_create_device: Option<
        unsafe extern "system" fn(D3d10DdiAdapterHandle, *const D3d12DdiArgCreateDevice) -> Hresult,
    >,
    pub pfn_close_adapter: Option<unsafe extern "system" fn(D3d10DdiAdapterHandle) -> Hresult>,
    pub pfn_get_supported_versions:
        Option<unsafe extern "system" fn(D3d10DdiAdapterHandle, *mut u32, *mut u64) -> Hresult>,
    pub pfn_get_caps: Option<
        unsafe extern "system" fn(D3d10DdiAdapterHandle, *const D3d10_2DdiArgGetCaps) -> Hresult,
    >,
    pub pfn_get_optional_ddi_tables: Option<
        unsafe extern "system" fn(
            D3d10DdiAdapterHandle,
            *mut u32,
            *mut D3d12DdiTableRequest,
        ) -> Hresult,
    >,
    pub pfn_fill_ddi_table: Option<
        unsafe extern "system" fn(
            D3d10DdiAdapterHandle,
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

/// TEMPORARY (Gate 5b bring-up): out-of-band smoke test of the DXVK bridge from a
/// normal process — no WDDM/INF/devcon/DWM involvement. Returns 0 if DXVK brought
/// up a logical device on the venus adapter, 1 otherwise. Remove once the DDI path
/// is validated end-to-end.
#[no_mangle]
pub extern "system" fn helios_umd_selftest() -> i32 {
    log_line("helios_umd_selftest: creating DXVK device on venus...");
    let dev = bridge::ffi::helios_dxvk_create_device(0, 0);
    let bridge_ok = !dev.is_null();
    log_line(&format!("helios_umd_selftest: bridge ok={bridge_ok}"));
    if !bridge_ok {
        return 1;
    }

    // Prove the windows-crate COM bindings call straight into DXVK's ID3D11Device
    // (the foundation for the pure-Rust DDI forwarders): create a real buffer.
    {
        use windows::Win32::Graphics::Direct3D11::*;
        let dev_ptr = dev.d3d11_device_ptr();
        log_line(&format!(
            "helios_umd_selftest: ID3D11Device* = 0x{dev_ptr:x}"
        ));
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
            log_line(&format!(
                "helios_umd_selftest: windows-crate CreateBuffer -> {hr:?}, buffer_some={}",
                buffer.is_some()
            ));
        }
    }
    drop(dev);

    // Exercise the full CreateDevice path in-process with synthesized runtime
    // buffers (no D3D runtime / DWM): proves CalcPrivateDeviceSize + CreateDevice
    // + the 152-entry table fill + DestroyDevice don't crash before risking a
    // live install. Buffers are u64-backed for 8-byte alignment.
    let hadapter = D3d10DdiAdapterHandle {
        p_drv_private: core::ptr::null_mut(),
    };
    let dev_size = unsafe { calc_private_device_size(hadapter, core::ptr::null()) };

    let mut device_priv = vec![0u64; dev_size / 8 + 1];
    let mut funcs = vec![0u64; core::mem::size_of::<ddi::D3D11DDI_DEVICEFUNCS>() / 8 + 1];
    let mut dxgi_funcs = vec![0u64; core::mem::size_of::<ddi::DXGI_DDI_BASE_FUNCTIONS>() / 8 + 1];

    let arg = D3d10DdiArgCreateDevice {
        h_rt_device: core::ptr::null_mut(),
        interface: 0x000b_000a, // D3D11_0_DDI_INTERFACE_VERSION
        version: 0,
        p_kt_callbacks: core::ptr::null(),
        p_device_funcs: funcs.as_mut_ptr().cast(),
        h_drv_device: device_priv.as_mut_ptr().cast(),
        dxgi_base_ddi: DxgiDdiBaseArgs {
            p_dxgi_base_callbacks: core::ptr::null(),
            p_dxgi_ddi_base_functions: dxgi_funcs.as_mut_ptr().cast(),
        },
        h_rt_core_layer: core::ptr::null_mut(),
        p_um_callbacks: core::ptr::null(),
        flags: 0,
        ppfn_retrieve_sub_object: core::ptr::null_mut(),
    };

    let hr = unsafe { create_device(hadapter, &arg as *const _ as *mut c_void) };
    log_line(&format!(
        "helios_umd_selftest: synthesized CreateDevice -> 0x{hr:08x}"
    ));
    if hr != S_OK {
        return 2;
    }

    // Sanity: the table must be fully non-null (a null entry the runtime calls = crash).
    let table = funcs.as_ptr() as *const Option<unsafe extern "C" fn(usize) -> usize>;
    let n = core::mem::size_of::<ddi::D3D11DDI_DEVICEFUNCS>() / 8;
    let null_slots = (0..n)
        .filter(|&i| unsafe { (*table.add(i)).is_none() })
        .count();
    log_line(&format!(
        "helios_umd_selftest: device-funcs null slots = {null_slots} / {n}"
    ));

    // Offscreen clear+readback through the real forwarders.
    let hdev = ddi::D3D10DDI_HDEVICE {
        pDrvPrivate: device_priv.as_mut_ptr().cast(),
    };
    let render_rc = unsafe { forward::selftest_offscreen_clear(hdev) };
    log_line(&format!(
        "helios_umd_selftest: offscreen clear rc={render_rc}"
    ));
    let tri_rc = unsafe { forward::selftest_triangle(hdev) };
    log_line(&format!("helios_umd_selftest: triangle rc={tri_rc}"));
    let cb_rc = unsafe { forward::selftest_cb_readback(hdev) };
    log_line(&format!("helios_umd_selftest: cb_readback rc={cb_rc}"));
    let cbtri_rc = unsafe { forward::selftest_triangle_cb(hdev) };
    log_line(&format!("helios_umd_selftest: triangle_cb rc={cbtri_rc}"));

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
    log_line("OpenAdapter12 -> DXGI_ERROR_UNSUPPORTED (D3D12 DDI not implemented yet)");
    let _ = open_data;
    return DXGI_ERROR_UNSUPPORTED;

    #[allow(unreachable_code)]
    {
        if open_data.is_null() {
            log_line("OpenAdapter12 null open_data -> E_NOTIMPL");
            return E_NOTIMPL;
        }

        let open = unsafe { &mut *open_data };
        if open.p_adapter_funcs.is_null() {
            log_line("OpenAdapter12 null pAdapterFuncs -> E_NOTIMPL");
            return E_NOTIMPL;
        }

        open.h_adapter = D3d10DdiAdapterHandle {
            p_drv_private: core::ptr::addr_of_mut!(ADAPTER_COOKIE).cast::<c_void>(),
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

        log_line("OpenAdapter12 -> S_OK (adapter funcs installed)");
        S_OK
    }
}

unsafe extern "system" fn d3d12_calc_private_device_size(
    _h_adapter: D3d10DdiAdapterHandle,
    args: *const D3d12DdiArgCalcPrivateDeviceSize,
) -> usize {
    if args.is_null() {
        log_line("D3D12 CalcPrivateDeviceSize null args -> 8");
    } else {
        let args = unsafe { &*args };
        log_line(&format!(
            "D3D12 CalcPrivateDeviceSize interface=0x{:08x} version=0x{:08x} flags=0x{:08x} -> 8",
            args.interface, args.version, args.flags
        ));
    }
    core::mem::size_of::<usize>()
}

unsafe extern "system" fn d3d12_create_device(
    _h_adapter: D3d10DdiAdapterHandle,
    args: *const D3d12DdiArgCreateDevice,
) -> Hresult {
    if args.is_null() {
        log_line("D3D12 CreateDevice null args -> E_NOTIMPL");
    } else {
        let args = unsafe { &*args };
        log_line(&format!(
            "D3D12 CreateDevice interface=0x{:08x} version=0x{:08x} flags=0x{:08x} \
             hRTDevice={:p} hDrvDevice={:p} pKTCallbacks={:p} pUMCallbacks={:p} -> E_NOTIMPL",
            args.interface,
            args.version,
            args.flags,
            args.h_rt_device,
            args.h_drv_device,
            args.p_kt_callbacks,
            args.p_um_callbacks,
        ));
    }
    E_NOTIMPL
}

unsafe extern "system" fn d3d12_close_adapter(_h_adapter: D3d10DdiAdapterHandle) -> Hresult {
    log_line("D3D12 CloseAdapter");
    S_OK
}

unsafe extern "system" fn d3d12_get_supported_versions(
    _h_adapter: D3d10DdiAdapterHandle,
    entries: *mut u32,
    supported_versions: *mut u64,
) -> Hresult {
    if entries.is_null() {
        log_line("D3D12 GetSupportedVersions null entries -> E_NOTIMPL");
        return E_NOTIMPL;
    }

    let requested_entries = unsafe { *entries };
    log_line(&format!(
        "D3D12 GetSupportedVersions requested={requested_entries} bufNull={} (advertising {:#018x?})",
        supported_versions.is_null(),
        D3D12_SUPPORTED_DDI_VERSIONS,
    ));
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
    _h_adapter: D3d10DdiAdapterHandle,
    args: *const D3d10_2DdiArgGetCaps,
) -> Hresult {
    const D3D12DDICAPS_TYPE_MEMORY_ARCHITECTURE: u32 = 1002;
    const D3D12DDICAPS_TYPE_SHADER: u32 = 1004;
    const D3D12DDICAPS_TYPE_ARCHITECTURE_INFO: u32 = 1005;
    const D3D12DDICAPS_TYPE_D3D12_OPTIONS: u32 = 1006;
    const D3D12DDICAPS_TYPE_3DPIPELINESUPPORT: u32 = 1007;
    const D3D12DDICAPS_TYPE_0081_3DPIPELINESUPPORT1: u32 = 1074;
    const D3D12DDI_3DPIPELINELEVEL_1_0_CORE: u32 = 2;

    if args.is_null() {
        log_line("D3D12 GetCaps null args -> S_OK");
        return S_OK;
    }

    let args = unsafe { &*args };
    log_line(&format!(
        "D3D12 GetCaps type=0x{:08x} dataSize={} pInfo={:p}",
        args.caps_type, args.data_size, args.p_info,
    ));

    if !args.p_data.is_null() && args.data_size != 0 {
        if args.caps_type == D3D12DDICAPS_TYPE_0081_3DPIPELINESUPPORT1 && args.data_size >= 8 {
            let data = args.p_data as *mut u32;
            let runtime_max = unsafe { *data.add(0) };
            let driver_max = runtime_max.min(D3D12DDI_3DPIPELINELEVEL_1_0_CORE);
            unsafe {
                *data.add(1) = driver_max;
            }
            log_line(&format!(
                "  D3D12 GetCaps: 3DPIPELINESUPPORT1 runtimeMax={} driverMax={}",
                runtime_max, driver_max
            ));
            return S_OK;
        }

        unsafe { core::ptr::write_bytes(args.p_data as *mut u8, 0, args.data_size as usize) };
        match args.caps_type {
            D3D12DDICAPS_TYPE_3DPIPELINESUPPORT if args.data_size >= 4 => {
                unsafe { *(args.p_data as *mut u32) = D3D12DDI_3DPIPELINELEVEL_1_0_CORE };
                log_line("  D3D12 GetCaps: 3DPIPELINESUPPORT = 1_0_CORE");
            }
            D3D12DDICAPS_TYPE_MEMORY_ARCHITECTURE if args.data_size >= 12 => {
                let data = args.p_data as *mut u32;
                unsafe {
                    *data.add(0) = 1; // UMA
                    *data.add(1) = 1; // IO coherent
                    *data.add(2) = 1; // Cache coherent
                }
                log_line("  D3D12 GetCaps: MEMORY_ARCHITECTURE = UMA/IO/cache coherent");
            }
            D3D12DDICAPS_TYPE_SHADER => {
                log_line("  D3D12 GetCaps: SHADER = zero");
            }
            D3D12DDICAPS_TYPE_D3D12_OPTIONS => {
                log_line("  D3D12 GetCaps: D3D12_OPTIONS = zero");
            }
            D3D12DDICAPS_TYPE_ARCHITECTURE_INFO => {
                log_line("  D3D12 GetCaps: ARCHITECTURE_INFO = zero");
            }
            _ => {}
        }
    }

    S_OK
}

unsafe extern "system" fn d3d12_get_optional_ddi_tables(
    _h_adapter: D3d10DdiAdapterHandle,
    entries: *mut u32,
    requests: *mut D3d12DdiTableRequest,
) -> Hresult {
    if entries.is_null() {
        log_line("D3D12 GetOptionalDDITables null entries -> E_NOTIMPL");
        return E_NOTIMPL;
    }

    let requested_entries = unsafe { *entries };
    log_line(&format!(
        "D3D12 GetOptionalDDITables requested={requested_entries} requestsNull={} -> 0 tables",
        requests.is_null()
    ));
    unsafe { *entries = 0 };
    S_OK
}

unsafe extern "system" fn d3d12_fill_ddi_table(
    _h_adapter: D3d10DdiAdapterHandle,
    table_type: u32,
    table: *mut c_void,
    table_size: usize,
    interface: u32,
    rt_table: *mut c_void,
) -> Hresult {
    log_line(&format!(
        "D3D12 FillDDITable type={} table={:p} size={} interface=0x{:08x} rtTable={:p} -> E_NOTIMPL",
        table_type, table, table_size, interface, rt_table,
    ));
    E_NOTIMPL
}

unsafe extern "system" fn d3d12_destroy_device(h_device: *mut c_void) {
    log_line(&format!("D3D12 DestroyDevice hDevice={h_device:p}"));
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
    let size = device_funcs::device_private_size();
    log_line(&format!("CalcPrivateDeviceSize -> {size}"));
    size
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
         pDeviceFuncs={:p} hDrvDevice={:p} pKTCallbacks={:p} pUMCallbacks={:p} pDXGIBaseFuncs={:p}",
        create.interface,
        create.version,
        create.flags,
        create.p_device_funcs,
        create.h_drv_device,
        create.p_kt_callbacks,
        create.p_um_callbacks,
        create.dxgi_base_ddi.p_dxgi_ddi_base_functions,
    ));

    // 1) Bring up the DXVK device on the Helios venus adapter.
    let dxvk = bridge::ffi::helios_dxvk_create_device(0, 0);
    if dxvk.is_null() {
        log_line("  CreateDevice: DXVK device creation FAILED -> E_FAIL");
        return E_FAIL;
    }

    // 2) Construct our device object in the runtime-allocated private memory
    //    (size came from CalcPrivateDeviceSize). hDrvDevice IS that pointer.
    if create.h_drv_device.is_null() {
        log_line("  CreateDevice: null hDrvDevice -> E_FAIL");
        return E_FAIL;
    }
    unsafe {
        core::ptr::write(
            create.h_drv_device as *mut device_funcs::HeliosDevice,
            device_funcs::HeliosDevice {
                dxvk,
                h_rt_device: create.h_rt_device,
                h_context: core::ptr::null_mut(),
                kt_callbacks: create.p_kt_callbacks as *const ddi::D3DDDI_DEVICECALLBACKS,
                dxgi_callbacks: create.dxgi_base_ddi.p_dxgi_base_callbacks
                    as *mut ddi::DXGI_DDI_BASE_CALLBACKS,
                h_rt_core_layer: create.h_rt_core_layer,
                um_callbacks: create.p_um_callbacks,
                ia: core::cell::RefCell::new(device_funcs::IaState::default()),
            },
        );
        device_funcs::create_runtime_context(
            &mut *(create.h_drv_device as *mut device_funcs::HeliosDevice),
        );
    }

    // 3) Fill the device-funcs table (Interface == D3D11_0 -> p11DeviceFuncs) and
    //    the DXGI base DDI table the runtime handed us.
    if create.p_device_funcs.is_null() {
        log_line("  CreateDevice: null pDeviceFuncs -> E_FAIL");
        return E_FAIL;
    }
    unsafe {
        if create.interface >= 0x000b_0022 {
            device_funcs::fill_wddm2_1_device_funcs(
                create.p_device_funcs as *mut ddi::D3DWDDM2_1DDI_DEVICEFUNCS,
            );
            device_funcs::fill_dxgi_1_1_base_funcs(
                create.dxgi_base_ddi.p_dxgi_ddi_base_functions
                    as *mut ddi::DXGI1_1_DDI_BASE_FUNCTIONS,
            );
        } else if create.interface >= 0x000b_000f {
            device_funcs::fill_d3d11_1_device_funcs(
                create.p_device_funcs as *mut ddi::D3D11_1DDI_DEVICEFUNCS,
            );
            device_funcs::fill_dxgi_1_1_base_funcs(
                create.dxgi_base_ddi.p_dxgi_ddi_base_functions
                    as *mut ddi::DXGI1_1_DDI_BASE_FUNCTIONS,
            );
        } else {
            device_funcs::fill_d3d11_device_funcs(
                create.p_device_funcs as *mut ddi::D3D11DDI_DEVICEFUNCS,
            );
            device_funcs::fill_dxgi_base_funcs(
                create.dxgi_base_ddi.p_dxgi_ddi_base_functions as *mut ddi::DXGI_DDI_BASE_FUNCTIONS,
            );
        }
    }

    log_line("  CreateDevice -> S_OK (DXVK device + D3D11 funcs table installed)");
    S_OK
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
    if entries.is_null() {
        log_line("GetSupportedVersions: null entries -> E_NOTIMPL");
        return E_NOTIMPL;
    }

    let requested_entries = unsafe { *entries };
    log_line(&format!(
        "GetSupportedVersions requested={requested_entries} bufNull={} (advertising {:#018x?})",
        supported_versions.is_null(),
        SUPPORTED_DDI_VERSIONS,
    ));
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
    const D3D11DDICAPS_THREADING: u32 = 128;
    const D3D11DDICAPS_SHADER: u32 = 129;
    const D3D11DDICAPS_3DPIPELINESUPPORT: u32 = 130;
    const D3D11_1DDICAPS_D3D11_OPTIONS: u32 = 131;
    const D3D11_1DDICAPS_ARCHITECTURE_INFO: u32 = 132;
    const D3D11_1DDICAPS_SHADER_MIN_PRECISION_SUPPORT: u32 = 134;

    if !args.is_null() {
        let args = unsafe { &*args };
        log_line(&format!(
            "GetCaps type=0x{:08x} dataSize={} pInfo={:p}",
            args.caps_type, args.data_size, args.p_info,
        ));
        if !args.p_data.is_null() && args.data_size != 0 {
            // Default: zero the output.
            unsafe { core::ptr::write_bytes(args.p_data as *mut u8, 0, args.data_size as usize) };
            match args.caps_type {
                // D3D11DDI_THREADING_CAPS::Caps. Zero means no free-threaded
                // mode and no command-list build support; the runtime must
                // serialize/emulate.
                D3D11DDICAPS_THREADING if args.data_size >= 4 => {
                    unsafe { *(args.p_data as *mut u32) = 0 };
                    log_line("  GetCaps: THREADING caps = 0");
                }
                // D3D11DDI_SHADER_CAPS::Caps. Zero means no optional shader
                // caps such as double precision.
                D3D11DDICAPS_SHADER if args.data_size >= 4 => {
                    unsafe { *(args.p_data as *mut u32) = 0 };
                    log_line("  GetCaps: SHADER caps = 0");
                }
                // D3D11DDI_3DPIPELINESUPPORT_CAPS::Caps is a
                // D3D11DDI_3DPIPELINELEVEL enum. Advertising 10_1 lets the
                // runtime enter CreateDevice; the stricter MSAA/format caps then
                // make it settle on FL10_0 for this adapter. Advertising exactly
                // 10_0 makes the runtime remove the device before CreateDevice
                // completes on this stack.
                D3D11DDICAPS_3DPIPELINESUPPORT if args.data_size >= 4 => {
                    const D3D11DDI_3DPIPELINELEVEL_10_1: u32 = 1;
                    unsafe { *(args.p_data as *mut u32) = D3D11DDI_3DPIPELINELEVEL_10_1 };
                    log_line("  GetCaps: 3DPIPELINESUPPORT = 10_1");
                }
                // D3D11.1 optional caps. The zeroed structs are valid
                // conservative answers: no logic-op, no debug binary support,
                // immediate-mode renderer, no shader min-precision support.
                D3D11_1DDICAPS_D3D11_OPTIONS if args.data_size >= 8 => {
                    log_line("  GetCaps: D3D11_OPTIONS = zero");
                }
                D3D11_1DDICAPS_ARCHITECTURE_INFO if args.data_size >= 4 => {
                    log_line("  GetCaps: ARCHITECTURE_INFO = zero");
                }
                D3D11_1DDICAPS_SHADER_MIN_PRECISION_SUPPORT if args.data_size >= 8 => {
                    log_line("  GetCaps: SHADER_MIN_PRECISION_SUPPORT = zero");
                }
                _ => {}
            }
        }
    } else {
        log_line("GetCaps: null args");
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

pub(crate) fn log_line(message: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(umd_log_path())
    {
        let _ = writeln!(file, "[pid={}] {}", std::process::id(), message);
    }
}
