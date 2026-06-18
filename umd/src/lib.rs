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

type Hresult = i32;

const S_OK: Hresult = 0;
const E_FAIL: Hresult = 0x8000_4005u32 as i32;
const E_NOTIMPL: Hresult = 0x8000_4001u32 as i32;
const E_OUTOFMEMORY: Hresult = 0x8007_000eu32 as i32;

const fn ddi_supported(major: u64, minor: u64, build: u64) -> u64 {
    let interface = (major << 16) | minor;
    (interface << 32) | (build << 16)
}

// Gate 5b: advertise D3D11_0 (D3D11_0_DDI_SUPPORTED = ddi_supported(11, 10, 2);
// Interface == D3D11_0_DDI_INTERFACE_VERSION 0x000b000a, build 2). The runtime
// echoes this as create.Interface and so passes us the `p11DeviceFuncs`
// (D3D11DDI_DEVICEFUNCS) table to fill. This list was previously EMPTY to dodge
// the DWM crash-loop (DWM fail-fasts when CreateDevice on its chosen composition
// adapter fails) — now safe because CreateDevice returns S_OK with a real device
// funcs table backed by the DXVK device (see device_funcs.rs).
const SUPPORTED_DDI_VERSIONS: &[u64] = &[ddi_supported(11, 10, 2)];

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
        log_line(&format!("helios_umd_selftest: ID3D11Device* = 0x{dev_ptr:x}"));
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
    log_line(&format!("helios_umd_selftest: synthesized CreateDevice -> 0x{hr:08x}"));
    if hr != S_OK {
        return 2;
    }

    // Sanity: the table must be fully non-null (a null entry the runtime calls = crash).
    let table = funcs.as_ptr() as *const Option<unsafe extern "C" fn(usize) -> usize>;
    let n = core::mem::size_of::<ddi::D3D11DDI_DEVICEFUNCS>() / 8;
    let null_slots = (0..n)
        .filter(|&i| unsafe { (*table.add(i)).is_none() })
        .count();
    log_line(&format!("helios_umd_selftest: device-funcs null slots = {null_slots} / {n}"));

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
            device_funcs::HeliosDevice { dxvk },
        );
    }

    // 3) Fill the device-funcs table (Interface == D3D11_0 -> p11DeviceFuncs) and
    //    the DXGI base DDI table the runtime handed us.
    if create.p_device_funcs.is_null() {
        log_line("  CreateDevice: null pDeviceFuncs -> E_FAIL");
        return E_FAIL;
    }
    unsafe {
        device_funcs::fill_d3d11_device_funcs(
            create.p_device_funcs as *mut ddi::D3D11DDI_DEVICEFUNCS,
        );
        device_funcs::fill_dxgi_base_funcs(
            create.dxgi_base_ddi.p_dxgi_ddi_base_functions as *mut ddi::DXGI_DDI_BASE_FUNCTIONS,
        );
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
    // D3D11DDICAPS_3DPIPELINESUPPORT (130): the runtime gates D3D11 device
    // creation on this — return 0 and it concludes the adapter can't do D3D11 and
    // returns DXGI_ERROR_UNSUPPORTED before ever calling CreateDevice.
    const D3D11DDICAPS_3DPIPELINESUPPORT: u32 = 130;

    if !args.is_null() {
        let args = unsafe { &*args };
        log_line(&format!(
            "GetCaps type=0x{:08x} dataSize={} pInfo={:p}",
            args.caps_type, args.data_size, args.p_info,
        ));
        if !args.p_data.is_null() && args.data_size != 0 {
            // Default: zero the output.
            unsafe { core::ptr::write_bytes(args.p_data as *mut u8, 0, args.data_size as usize) };
            // Report 3D pipeline support so the runtime proceeds to CreateDevice.
            if args.caps_type == D3D11DDICAPS_3DPIPELINESUPPORT && args.data_size >= 4 {
                unsafe { *(args.p_data as *mut u32) = 1 };
                log_line("  GetCaps: reporting 3DPIPELINESUPPORT = TRUE");
            }
        }
    } else {
        log_line("GetCaps: null args");
    }
    S_OK
}

pub(crate) fn log_line(message: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(r"C:\Windows\Temp\helios_umd.log")
    {
        let _ = writeln!(file, "[pid={}] {}", std::process::id(), message);
    }
}
