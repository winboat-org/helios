//! Display child enumeration: relations, HPD status, the EDID descriptor and
//! the container id for the single video-output child.
//!
//! Moved verbatim out of `ddi/start_device.rs` by T8/R1102.

use core::ffi::c_void;

use crate::adapter::AdapterContext;
use crate::dxgk::*;

/// `DxgkDdiQueryChildRelations` — enumerate the adapter's child devices.
///
/// Render-only (DisplayHalf off): expose no child devices. DisplayHalf on
/// (Option A): report ONE `TypeVideoOutput` child so the OS can build a VidPn
/// target + attach the default monitor — the presentable output legacy BLT
/// windowed present needs. The array dxgkrnl passes is NUL-terminated (its last
/// entry stays zeroed), so the usable count is `size/stride - 1` (viogpu shape).
pub unsafe extern "C" fn dxgkddi_query_child_relations(
    miniport_device_context: *mut c_void,
    child_relations: *mut DXGK_CHILD_DESCRIPTOR,
    child_relations_size: u32,
) -> NTSTATUS {
    crate::diag::record(0x1200_0001);
    crate::diag::record(0x1201_0000 | (child_relations_size & 0xFFFF));

    if miniport_device_context.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl hands back our AdapterContext.
    let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };
    if !adapter.display_half() || child_relations.is_null() {
        // No connectors/monitors → leave the (already-zeroed) array untouched.
        return STATUS_SUCCESS;
    }

    let stride = core::mem::size_of::<DXGK_CHILD_DESCRIPTOR>() as u32;
    // Two-call contract: the array is NUL-terminated, so a size of exactly
    // (count+1)*stride is expected; require room for our one child + terminator.
    if stride == 0 || child_relations_size < stride.saturating_mul(2) {
        crate::diag::record(0x1205_00E0);
        return STATUS_INVALID_PARAMETER;
    }

    // SAFETY: index 0 is within the caller-provided array (checked above). We
    // fully initialize the single video-output child; the terminator entry the
    // OS provided stays zeroed.
    unsafe {
        let d = &mut *child_relations.add(crate::ddi::vidpn::CHILD_INDEX as usize);
        core::ptr::write_bytes(d as *mut _ as *mut u8, 0, stride as usize);
        d.ChildDeviceType = _DXGK_CHILD_DEVICE_TYPE::TypeVideoOutput;
        // AlwaysConnected (NOT Interruptible) is deliberate + load-bearing: the OS
        // synthesizes its initial 1-path VidPn as StartAdapter completes, and for an
        // Interruptible target the target PDO only exists once the driver has
        // asserted DxgkCbIndicateChildStatus(connected) — which our HPD worker does
        // ~500 ms LATER, after the OS has already committed the empty "display
        // nothing" topology. AlwaysConnected creates the target PDO unconditionally
        // at StartDevice (no race), so the OS can pair source0→target0 immediately.
        // Correct for a virtual monitor that never unplugs
        // (enumerating-child-devices-of-a-display-adapter.md:25-27,41-48).
        d.ChildCapabilities.HpdAwareness =
            _DXGK_CHILD_DEVICE_HPD_AWARENESS::HpdAwarenessAlwaysConnected;
        // `Type` is a real (Copy) bindgen union; write the VideoOutput arm's
        // fields directly (each write is a union place-expression, unsafe).
        // HD15 (analog VGA) — NOT VOT_OTHER — is deliberate: per
        // `forced-versus-connected-targets.md`, ONLY analog target types are
        // "forceable", and a target can be enabled (→ a present path is created)
        // only if a monitor is *connected* OR the target is *forceable*. A
        // non-forceable VOT_OTHER target whose virtual-monitor connection the OS
        // doesn't fully recognize is never given a path → 0-path VidPn commits
        // (36th-session root cause). viogpu3d's non-VGA output is likewise VOT_HD15.
        d.ChildCapabilities.Type.VideoOutput.InterfaceTechnology =
            _D3DKMDT_VIDEO_OUTPUT_TECHNOLOGY::D3DKMDT_VOT_HD15;
        d.ChildCapabilities
            .Type
            .VideoOutput
            .MonitorOrientationAwareness = _D3DKMDT_MONITOR_ORIENTATION_AWARENESS::D3DKMDT_MOA_NONE;
        d.ChildCapabilities.Type.VideoOutput.SupportsSdtvModes = 0;
        d.AcpiUid = 0;
        d.ChildUid = crate::ddi::vidpn::CHILD_UID;
    }
    crate::diag::record(0x1205_0001);
    STATUS_SUCCESS
}

/// `DxgkDdiQueryChildStatus` — report HPD state of a child device.
///
/// DisplayHalf on: the single video-output child is always connected (the
/// virtual monitor never unplugs). Off: nothing to report.
pub unsafe extern "C" fn dxgkddi_query_child_status(
    miniport_device_context: *mut c_void,
    child_status: *mut DXGK_CHILD_STATUS,
    non_destructive_only: BOOLEAN,
) -> NTSTATUS {
    crate::diag::record(0x1200_0002);
    if !child_status.is_null() {
        crate::diag::record(0x1202_0000 | unsafe { (*child_status).ChildUid & 0xFFFF });
    }
    crate::diag::record(0x1203_0000 | ((non_destructive_only as u32) & 0xFFFF));

    if miniport_device_context.is_null() || child_status.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: our AdapterContext.
    let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };
    if !adapter.display_half() {
        // We reported NumberOfChildren = 0, so there is no child whose status we
        // could answer. Returning SUCCESS with the caller's DXGK_CHILD_STATUS
        // untouched is a fake success; NOT_SUPPORTED is in the DDI's legal
        // return set and is behaviour-neutral in the field, because dxgkrnl does
        // not query children it was never told about. StQcs moving means the
        // child count and this path have gone out of step.
        // SAFETY: non-null per the check above.
        crate::diag::fault(crate::diag::FaultCounter::StQcs, unsafe {
            (*child_status).Type as u32
        });
        return STATUS_NOT_SUPPORTED;
    }

    // SAFETY: non-null per the check; dxgkrnl provides a writable DXGK_CHILD_STATUS.
    let status = unsafe { &mut *child_status };
    match status.Type {
        _DXGK_CHILD_STATUS_TYPE::StatusConnection => {
            // Plain union write (safe): report the output as connected.
            status.__bindgen_anon_1.HotPlug.Connected = 1;
            crate::diag::record(0x1206_0001);
            STATUS_SUCCESS
        }
        // We reported MonitorOrientationAwareness = NONE, so the OS must not query
        // rotation status; anything else is not serviced.
        _ => STATUS_NOT_SUPPORTED,
    }
}

/// `DxgkDdiQueryDeviceDescriptor` — return the child monitor's descriptor (EDID).
///
/// DisplayHalf on: serve the REAL EDID generated at StartDevice for the host's
/// scanout-0 mode, in the OS-requested chunk. That is what makes the OS build a
/// presentable target; the EDID-less `CHILD_DESCRIPTOR_NOT_SUPPORTED`
/// default-monitor path this doc used to describe was replaced in the 36th
/// session and is a suspect for the mode-set retry loop. Off: no child
/// descriptors at all (`STATUS_NOT_SUPPORTED`).
pub unsafe extern "C" fn dxgkddi_query_device_descriptor(
    miniport_device_context: *mut c_void,
    child_uid: u32,
    device_descriptor: *mut DXGK_DEVICE_DESCRIPTOR,
) -> NTSTATUS {
    crate::diag::record(0x1200_0003);
    crate::diag::record(0x1204_0000 | (child_uid & 0xFFFF));

    if miniport_device_context.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: our AdapterContext.
    let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };
    if !adapter.display_half() {
        return STATUS_NOT_SUPPORTED;
    }
    if device_descriptor.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // Serve the EDID generated at StartDevice for the host's scanout-0 mode in the
    // OS-requested chunk (viogpu3d's `QueryDeviceDescriptor`). A REAL monitor (vs
    // the EDID-less "default monitor") is what makes the OS build a presentable
    // target — the 35th session's CHILD_DESCRIPTOR_NOT_SUPPORTED default-monitor
    // path is a suspect for the mode-set retry loop (WINDOWED_BLT_DESIGN §6.3).
    let Some(edid) = adapter.edid() else {
        return STATUS_NOT_SUPPORTED;
    };
    // SAFETY: non-null per the check; dxgkrnl provides a writable descriptor.
    let dd = unsafe { &mut *device_descriptor };
    let offset = dd.DescriptorOffset as usize;
    if offset >= edid.len() {
        return crate::ddi::vidpn::STATUS_MONITOR_NO_MORE_DESCRIPTOR_DATA;
    }
    let len = (dd.DescriptorLength as usize).min(edid.len() - offset);
    if dd.DescriptorBuffer.is_null() || len == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: `DescriptorBuffer` is a writable buffer of at least DescriptorLength
    // bytes; `len` is clamped to both it and the remaining EDID.
    unsafe {
        core::ptr::copy_nonoverlapping(
            edid.as_ptr().add(offset),
            dd.DescriptorBuffer as *mut u8,
            len,
        );
    }
    dd.DescriptorLength = len as u32;
    crate::diag::record(0x120E_0000 | (len as u32 & 0xFFFF));
    STATUS_SUCCESS
}

/// `DxgkDdiGetChildContainerId` — return a stable container id for a child.
///
/// The OS groups a display's devnodes by container id. We hand back a fixed,
/// driver-defined GUID for our single video-output child so the monitor devnode
/// binds cleanly. Only meaningful when the display half is active.
pub unsafe extern "C" fn dxgkddi_get_child_container_id(
    miniport_device_context: *mut c_void,
    child_uid: u32,
    container_id: *mut DXGK_CHILD_CONTAINER_ID,
) -> NTSTATUS {
    crate::diag::record(0x1200_0004);
    crate::diag::record(0x1207_0000 | (child_uid & 0xFFFF));

    if miniport_device_context.is_null() || container_id.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: our AdapterContext.
    let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };
    if !adapter.display_half() {
        return STATUS_NOT_SUPPORTED;
    }
    // SAFETY: dxgkrnl provides a writable DXGK_CHILD_CONTAINER_ID.
    unsafe {
        let cid = &mut *container_id;
        core::ptr::write_bytes(
            cid as *mut _ as *mut u8,
            0,
            core::mem::size_of::<DXGK_CHILD_CONTAINER_ID>(),
        );
        cid.ContainerId = crate::ddi::vidpn::HELIOS_MONITOR_CONTAINER_ID;
    }
    STATUS_SUCCESS
}
