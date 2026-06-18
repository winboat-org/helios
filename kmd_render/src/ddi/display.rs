//! Display/VidPn DDIs required for a complete WDDM table shape.
//!
//! Helios targets render-only WDDM. These callbacks therefore do not implement
//! scanout; they make unsupported display paths explicit instead of leaving a
//! large part of the display-miniport table NULL during bring-up.

use core::ffi::c_void;

use crate::dxgk::*;

pub unsafe extern "C" fn dxgkddi_present(
    _adapter: IN_CONST_HANDLE,
    _present: INOUT_PDXGKARG_PRESENT,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_set_pointer_position(
    _adapter: IN_CONST_HANDLE,
    _position: IN_CONST_PDXGKARG_SETPOINTERPOSITION,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_set_pointer_shape(
    _adapter: IN_CONST_HANDLE,
    _shape: IN_CONST_PDXGKARG_SETPOINTERSHAPE,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_is_supported_vidpn(
    _adapter: IN_CONST_HANDLE,
    _is_supported: INOUT_PDXGKARG_ISSUPPORTEDVIDPN,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_recommend_functional_vidpn(
    _adapter: IN_CONST_HANDLE,
    _recommend: IN_CONST_PDXGKARG_RECOMMENDFUNCTIONALVIDPN_CONST,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_enum_vidpn_cofunc_modality(
    _adapter: IN_CONST_HANDLE,
    _enum_modality: IN_CONST_PDXGKARG_ENUMVIDPNCOFUNCMODALITY_CONST,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_set_vidpn_source_visibility(
    _adapter: IN_CONST_HANDLE,
    _visibility: IN_CONST_PDXGKARG_SETVIDPNSOURCEVISIBILITY,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_commit_vidpn(
    _adapter: IN_CONST_HANDLE,
    _commit: IN_CONST_PDXGKARG_COMMITVIDPN_CONST,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_update_active_vidpn_present_path(
    _adapter: IN_CONST_HANDLE,
    _path: IN_CONST_PDXGKARG_UPDATEACTIVEVIDPNPRESENTPATH_CONST,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_set_vidpn_source_address(
    _adapter: IN_CONST_HANDLE,
    _address: IN_CONST_PDXGKARG_SETVIDPNSOURCEADDRESS,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_recommend_monitor_modes(
    _adapter: IN_CONST_HANDLE,
    _recommend: IN_CONST_PDXGKARG_RECOMMENDMONITORMODES_CONST,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_query_vidpn_hw_capability(
    _adapter: IN_CONST_HANDLE,
    _caps: INOUT_PDXGKARG_QUERYVIDPNHWCAPABILITY,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_get_scan_line(
    _adapter: IN_CONST_HANDLE,
    _scan_line: INOUT_PDXGKARG_GETSCANLINE,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_stop_device_and_release_post_display_ownership(
    _miniport_device_context: *mut c_void,
    _target_id: D3DDDI_VIDEO_PRESENT_TARGET_ID,
    _display_info: PDXGK_DISPLAY_INFORMATION,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_system_display_enable(
    _miniport_device_context: *mut c_void,
    _target_id: D3DDDI_VIDEO_PRESENT_TARGET_ID,
    _flags: PDXGKARG_SYSTEM_DISPLAY_ENABLE_FLAGS,
    _width: *mut UINT,
    _height: *mut UINT,
    _color_format: *mut D3DDDIFORMAT,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

pub unsafe extern "C" fn dxgkddi_system_display_write(
    _miniport_device_context: *mut c_void,
    _source: *mut c_void,
    _source_width: UINT,
    _source_height: UINT,
    _source_stride: UINT,
    _position_x: UINT,
    _position_y: UINT,
) {
}

pub unsafe extern "C" fn dxgkddi_exchange_pre_start_info(
    _adapter: IN_CONST_HANDLE,
    pre_start_info: IN_OUT_PDXGK_PRE_START_INFO,
) -> NTSTATUS {
    crate::diag::record(0x0E00_0001);
    if pre_start_info.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    crate::diag::record(0x0E00_0002);
    STATUS_SUCCESS
}
