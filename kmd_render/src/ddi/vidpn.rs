//! VidPn topology / mode-enumeration helpers for the active display half.
//!
//! With the production `DisplayHalf` service knob on, `StartDevice` advertises
//! one video-present source and one child output. `CommitVidPn` validates the
//! fixed mode and `SetVidPnSourceAddress` selects the authoritative primary for
//! virtio-gpu scanout. Knob-off render-only behavior remains a recovery mode.
//!
//! The VidPn iteration mirrors the proven virtio-gpu display-only miniport
//! (`virtio-research-only-3d/viogpu/viogpudo`) `EnumVidPnCofuncModality`. A
//! panic in any VidPn DDI is a silent graphics deadlock, so every path returns
//! a legal NTSTATUS and no `unwrap`/`panic!` is used.

use core::ptr::{null, null_mut};

use crate::adapter::AdapterContext;
use crate::dxgk::*;

// ── Fixed topology / mode constants ─────────────────────────────────────────
/// One video-present source, one child (video output) — reported at StartDevice.
pub const NUM_VIDPN_SOURCES: u32 = 1;
pub const NUM_CHILDREN: u32 = 1;
/// Index of our single child in the QueryChildRelations array.
pub const CHILD_INDEX: u32 = 0;
/// Stable child uid == the video-present target id for our single output.
pub const CHILD_UID: u32 = 0;
/// Fallback mode for the virtual monitor when the host's `GET_DISPLAY_INFO`
/// reported no usable scanout-0 size. The live mode comes from
/// [`AdapterContext::display_mode`](crate::adapter::AdapterContext::display_mode).
pub const DEFAULT_MODE_WIDTH: u32 = 1920;
pub const DEFAULT_MODE_HEIGHT: u32 = 1080;
const REFRESH_HZ: u32 = 60;

/// Build a valid EDID 1.4 (checksum `sum % 256 == 0`) whose preferred detailed
/// timing (DTD1) is exactly `w × h @ ~60 Hz`. `DxgkDdiQueryDeviceDescriptor` serves
/// this so the OS builds a REAL monitor (not the EDID-less "default monitor") —
/// both viogpu display miniports ship an EDID, and a monitor-less child is a
/// documented reason the OS refuses to make a target presentable
/// (WINDOWED_BLT_DESIGN §6.3). Generating it for the live host mode keeps the
/// monitor's native resolution equal to the source/target modes we assign
/// (cofunctional). QEMU's virtio-gpu display is not a real panel, so the timing
/// only needs valid structure + active-pixel fields — Windows reads HActive/VActive
/// for the native mode and validates the header/checksum, not the electrical clock.
pub fn build_edid(w: u32, h: u32) -> [u8; 128] {
    let mut e = [0u8; 128];
    // Header + manufacturer "HLS" (5-bit letters, A=1) + product 0x0001.
    e[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    let mfg: u16 = (8 << 10) | (12 << 5) | 19;
    e[8] = (mfg >> 8) as u8;
    e[9] = (mfg & 0xFF) as u8;
    e[10] = 0x01;
    e[16] = 1; // week
    e[17] = 34; // year 1990+34 = 2024
    e[18] = 1; // EDID 1.4
    e[19] = 4;
    e[20] = 0xA5; // digital, 8bpc, DisplayPort
    e[21] = 51; // max h image size (cm)
    e[22] = 29; // max v image size (cm)
    e[23] = 120; // gamma 2.2
    e[24] = 0x02; // features: preferred timing is native
    e[25..35].copy_from_slice(&[0xEE, 0x95, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54]);
    // Standard timings unused (0x0101 × 8).
    for b in e.iter_mut().take(54).skip(38) {
        *b = 0x01;
    }
    // Detailed Timing Descriptor 1 (bytes 54..72): w × h, ~60 Hz.
    let hb: u32 = ((w / 4) & !7).max(160); // horizontal blanking
    let vb: u32 = 45; // vertical blanking
    let ht = w + hb;
    let vt = h + vb;
    let pc = ((ht as u64 * vt as u64 * 60) / 10_000).clamp(1, 0xFFFF) as u32; // pixel clock /10 kHz
    let hfp = (hb / 3).min(88);
    let hsw = (hb / 5).min(44);
    let (vfp, vsw) = (3u32, 5u32);
    let d = &mut e[54..72];
    d[0] = (pc & 0xFF) as u8;
    d[1] = (pc >> 8) as u8;
    d[2] = (w & 0xFF) as u8;
    d[3] = (hb & 0xFF) as u8;
    d[4] = ((((w >> 8) & 0xF) << 4) | ((hb >> 8) & 0xF)) as u8;
    d[5] = (h & 0xFF) as u8;
    d[6] = (vb & 0xFF) as u8;
    d[7] = ((((h >> 8) & 0xF) << 4) | ((vb >> 8) & 0xF)) as u8;
    d[8] = (hfp & 0xFF) as u8;
    d[9] = (hsw & 0xFF) as u8;
    d[10] = (((vfp & 0xF) << 4) | (vsw & 0xF)) as u8;
    d[17] = 0x1E; // digital separate sync, +H +V
                  // Descriptor 2: monitor range limits (0xFD).
    e[72..90].copy_from_slice(&[
        0x00, 0x00, 0x00, 0xFD, 0x00, 0x32, 0x4B, 0x1E, 0x50, 0x14, 0x00, 0x0A, 0x20, 0x20, 0x20,
        0x20, 0x20, 0x20,
    ]);
    // Descriptor 3: monitor name "Helios" (0xFC).
    e[90..95].copy_from_slice(&[0x00, 0x00, 0x00, 0xFC, 0x00]);
    let name = b"Helios\n";
    e[95..95 + name.len()].copy_from_slice(name);
    for b in e.iter_mut().take(108).skip(95 + name.len()) {
        *b = 0x20;
    }
    // Descriptor 4: unused (0x10).
    e[108..113].copy_from_slice(&[0x00, 0x00, 0x00, 0x10, 0x00]);
    // Byte 126 = extension count (0). Byte 127 = checksum: sum of all 128 == 0 mod 256.
    let sum: u32 = e[..127].iter().map(|&b| b as u32).sum();
    e[127] = ((256 - (sum % 256)) % 256) as u8;
    e
}

/// Stable container id for the virtual monitor devnode (DxgkDdiGetChildContainerId).
pub const HELIOS_MONITOR_CONTAINER_ID: GUID = GUID {
    Data1: 0x48454C49,
    Data2: 0x4F53,
    Data3: 0x4D4E,
    Data4: [0x54, 0x52, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01],
};

// ── Graphics NTSTATUS values (not in the generated bindings) ────────────────
// Verified against WDK ntstatus.h (10.0.22621.0). NTSTATUS is i32; the 0xC...
// values are negative (error), 0x4... are informational (NT_SUCCESS true).
pub const STATUS_GRAPHICS_NO_RECOMMENDED_FUNCTIONAL_VIDPN: NTSTATUS = 0xC01E0323u32 as NTSTATUS;
pub const STATUS_GRAPHICS_MODE_ALREADY_IN_MODESET: NTSTATUS = 0xC01E0314u32 as NTSTATUS;
pub const STATUS_GRAPHICS_NO_MORE_ELEMENTS_IN_DATASET: NTSTATUS = 0x401E034Cu32 as NTSTATUS;
pub const STATUS_GRAPHICS_CHILD_DESCRIPTOR_NOT_SUPPORTED: NTSTATUS = 0xC01E0401u32 as NTSTATUS;
pub const STATUS_GRAPHICS_INVALID_VIDPN: NTSTATUS = 0xC01E0303u32 as NTSTATUS;
/// The requested EDID descriptor offset is past the end of our block (the OS reads
/// the EDID in chunks and stops on this). WDK ntstatus.h 0xC01E0501.
pub const STATUS_MONITOR_NO_MORE_DESCRIPTOR_DATA: NTSTATUS = 0xC01E0501u32 as NTSTATUS;

#[inline]
fn ok(s: NTSTATUS) -> bool {
    s >= 0
}

fn rec(name: &[u8], value: u32) {
    crate::diag::record_named_bytes(name, value);
}

/// Count of cofunc-modality/monitor-modes calls that hit a callback failure.
static COFUNC_ERR_COUNT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Clamp a VidPn-DDI result to that DDI's legal return contract. dxgkrnl treats
/// ANY out-of-contract NTSTATUS from `IsSupportedVidPn`/`EnumVidPnCofuncModality`/
/// `RecommendMonitorModes`/`CommitVidPn` as a DRIVER BUG (AzureTriage task 494,
/// "Driver returned an invalid NTSTATUS code") and discards EVERY candidate VidPn
/// → 0 display paths, `SetDisplayConfig` GEN_FAILURE (ETW-proven 36th session). The
/// legal error set is `STATUS_GRAPHICS_*` (0xC01Exxxx) + `STATUS_NO_MEMORY`; any
/// other failure a callback hands back must be reported as
/// `STATUS_GRAPHICS_INVALID_VIDPN` so dxgkrnl merely rejects that candidate and
/// keeps negotiating instead of giving up on the adapter.
#[inline]
pub(crate) fn legalize_vidpn(s: NTSTATUS) -> NTSTATUS {
    if s >= 0 {
        return s; // success / informational
    }
    let u = s as u32;
    if (u >> 16) == 0xC01E || u == 0xC000_0017
    /* STATUS_NO_MEMORY */
    {
        return s;
    }
    STATUS_GRAPHICS_INVALID_VIDPN
}

/// Resolve the OS VidPn interface for `h_vidpn` via `DxgkCbQueryVidPnInterface`.
/// Returns a raw `*const DXGK_VIDPN_INTERFACE` (borrowed from dxgkrnl, valid for
/// the duration of the DDI call) or an error NTSTATUS.
///
/// # Safety
/// `adapter.dxgkrnl` must be the live interface saved at StartDevice.
unsafe fn vidpn_interface(
    adapter: &AdapterContext,
    h_vidpn: D3DKMDT_HVIDPN,
) -> Result<*const DXGK_VIDPN_INTERFACE, NTSTATUS> {
    let dxgkrnl = adapter
        .dxgkrnl_opt()
        .ok_or(STATUS_GRAPHICS_INVALID_VIDPN)?;
    let query = dxgkrnl
        .DxgkCbQueryVidPnInterface
        .ok_or(STATUS_GRAPHICS_INVALID_VIDPN)?;
    let mut iface: *const DXGK_VIDPN_INTERFACE = null();
    // SAFETY: PASSIVE-level callback; `iface` is a valid out-pointer.
    let st = unsafe {
        query(
            h_vidpn,
            _DXGK_VIDPN_INTERFACE_VERSION::DXGK_VIDPN_INTERFACE_VERSION_V1,
            &mut iface,
        )
    };
    if !ok(st) || iface.is_null() {
        return Err(if ok(st) {
            STATUS_GRAPHICS_INVALID_VIDPN
        } else {
            st
        });
    }
    Ok(iface)
}

/// Build a fully-specified 1080p60 progressive video signal (source and target
/// modes share it so cofunctional-mode validation stays consistent).
fn video_signal_info(w: u32, h: u32) -> D3DKMDT_VIDEO_SIGNAL_INFO {
    // SAFETY: an all-zero VIDEO_SIGNAL_INFO is a valid starting point.
    let mut sig: D3DKMDT_VIDEO_SIGNAL_INFO = unsafe { core::mem::zeroed() };
    sig.VideoStandard = _D3DKMDT_VIDEO_SIGNAL_STANDARD::D3DKMDT_VSS_OTHER;
    sig.TotalSize.cx = w;
    sig.TotalSize.cy = h;
    sig.ActiveSize = sig.TotalSize;
    sig.VSyncFreq.Numerator = REFRESH_HZ;
    sig.VSyncFreq.Denominator = 1;
    sig.HSyncFreq.Numerator = h * REFRESH_HZ;
    sig.HSyncFreq.Denominator = 1;
    sig.PixelRate = (w as SIZE_T) * (h as SIZE_T) * (REFRESH_HZ as SIZE_T);
    // ScanLineOrdering shares a union with the (unused) additional-signal-info
    // bitfield; the whole struct was zeroed, so selecting this arm is a plain
    // union write (safe — only union reads are unsafe).
    sig.__bindgen_anon_1.ScanLineOrdering =
        _D3DDDI_VIDEO_SIGNAL_SCANLINE_ORDERING::D3DDDI_VSSLO_PROGRESSIVE;
    sig
}

/// Create + add one source mode into `h_set`.
///
/// # Safety
/// `iface` is a live `DXGK_VIDPNSOURCEMODESET_INTERFACE` for `h_set`.
unsafe fn add_source_mode(
    iface: *const DXGK_VIDPNSOURCEMODESET_INTERFACE,
    h_set: D3DKMDT_HVIDPNSOURCEMODESET,
    w: u32,
    h: u32,
    pixel_format: D3DDDIFORMAT,
) -> NTSTATUS {
    // SAFETY: iface valid per the fn contract.
    let iface = unsafe { &*iface };
    let (Some(create), Some(add), Some(release)) = (
        iface.pfnCreateNewModeInfo,
        iface.pfnAddMode,
        iface.pfnReleaseModeInfo,
    ) else {
        return STATUS_GRAPHICS_INVALID_VIDPN;
    };
    let mut mode: *mut D3DKMDT_VIDPN_SOURCE_MODE = null_mut();
    // SAFETY: valid out-pointer.
    let st = unsafe { create(h_set, &mut mode) };
    if !ok(st) || mode.is_null() {
        return if ok(st) {
            STATUS_GRAPHICS_INVALID_VIDPN
        } else {
            st
        };
    }
    // SAFETY: `mode` is a writable source-mode the OS just allocated. `.Format`
    // is a Copy union; the Graphics arm is the one source modes use.
    unsafe {
        (*mode).Type = _D3DKMDT_VIDPN_SOURCE_MODE_TYPE::D3DKMDT_RMT_GRAPHICS;
        let g = &mut (*mode).Format.Graphics;
        g.PrimSurfSize.cx = w;
        g.PrimSurfSize.cy = h;
        g.VisibleRegionSize = g.PrimSurfSize;
        g.Stride = w * 4;
        g.PixelFormat = pixel_format;
        g.ColorBasis = _D3DKMDT_COLOR_BASIS::D3DKMDT_CB_SCRGB;
        g.PixelValueAccessMode = _D3DKMDT_PIXEL_VALUE_ACCESS_MODE::D3DKMDT_PVAM_DIRECT;
    }
    // SAFETY: add/release take the mode we filled.
    let st = unsafe { add(h_set, mode) };
    if !ok(st) {
        let _ = unsafe { release(h_set, mode) };
        if st == STATUS_GRAPHICS_MODE_ALREADY_IN_MODESET {
            return STATUS_SUCCESS;
        }
        return st;
    }
    STATUS_SUCCESS
}

/// Publish the two 32-bit source formats Helios carries end-to-end.
///
/// DXGI mode enumeration filters the VidPn source-mode set by the caller's
/// requested format. Advertising only A8R8G8B8 therefore makes an RGBA8
/// `GetDisplayModeList` call succeed with zero modes even though RGBA8 is a
/// valid D3D11 display format. Each mode here is backed by the exact
/// SetVidPnSourceAddress allocation format; the display copy path converts
/// A8B8G8R8 into the adapter-owned BGRA scanout image.
unsafe fn add_source_modes(
    iface: *const DXGK_VIDPNSOURCEMODESET_INTERFACE,
    h_set: D3DKMDT_HVIDPNSOURCEMODESET,
    w: u32,
    h: u32,
) -> NTSTATUS {
    for pixel_format in [
        _D3DDDIFORMAT::D3DDDIFMT_A8R8G8B8,
        _D3DDDIFORMAT::D3DDDIFMT_A8B8G8R8,
    ] {
        let status = unsafe { add_source_mode(iface, h_set, w, h, pixel_format) };
        if !ok(status) {
            return status;
        }
    }
    STATUS_SUCCESS
}

/// Create + add our single target mode into `h_set`.
///
/// # Safety
/// `iface` is a live `DXGK_VIDPNTARGETMODESET_INTERFACE` for `h_set`.
unsafe fn add_single_target_mode(
    iface: *const DXGK_VIDPNTARGETMODESET_INTERFACE,
    h_set: D3DKMDT_HVIDPNTARGETMODESET,
    w: u32,
    h: u32,
) -> NTSTATUS {
    // SAFETY: iface valid per the fn contract.
    let iface = unsafe { &*iface };
    let (Some(create), Some(add), Some(release)) = (
        iface.pfnCreateNewModeInfo,
        iface.pfnAddMode,
        iface.pfnReleaseModeInfo,
    ) else {
        return STATUS_GRAPHICS_INVALID_VIDPN;
    };
    let mut mode: *mut D3DKMDT_VIDPN_TARGET_MODE = null_mut();
    // SAFETY: valid out-pointer.
    let st = unsafe { create(h_set, &mut mode) };
    if !ok(st) || mode.is_null() {
        return if ok(st) {
            STATUS_GRAPHICS_INVALID_VIDPN
        } else {
            st
        };
    }
    // SAFETY: `mode` is writable; Preference lives in the Copy union's bitfield arm.
    unsafe {
        (*mode).VideoSignalInfo = video_signal_info(w, h);
        (*mode)
            .__bindgen_anon_1
            .__bindgen_anon_1
            .set_Preference(_D3DKMDT_MODE_PREFERENCE::D3DKMDT_MP_PREFERRED);
    }
    // SAFETY: add/release take the mode we filled.
    let st = unsafe { add(h_set, mode) };
    if !ok(st) {
        let _ = unsafe { release(h_set, mode) };
        if st == STATUS_GRAPHICS_MODE_ALREADY_IN_MODESET {
            return STATUS_SUCCESS;
        }
        return st;
    }
    STATUS_SUCCESS
}

/// `DxgkDdiRecommendMonitorModes` body: publish the single monitor source mode
/// for the default monitor the OS created for our EDID-less child.
///
/// # Safety
/// `arg` points to a valid `DXGKARG_RECOMMENDMONITORMODES` for the call.
pub unsafe fn recommend_monitor_modes(
    adapter: &AdapterContext,
    arg: *const DXGKARG_RECOMMENDMONITORMODES,
) -> NTSTATUS {
    rec(b"VpMM", 1);
    if arg.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let (w, h) = adapter.display_mode();
    // SAFETY: valid per fn contract.
    let a = unsafe { &*arg };
    if a.pMonitorSourceModeSetInterface.is_null() {
        return STATUS_GRAPHICS_INVALID_VIDPN;
    }
    // SAFETY: dxgkrnl-provided interface, valid for the call.
    let iface = unsafe { &*a.pMonitorSourceModeSetInterface };
    let (Some(create), Some(add), Some(release)) = (
        iface.pfnCreateNewModeInfo,
        iface.pfnAddMode,
        iface.pfnReleaseModeInfo,
    ) else {
        return STATUS_GRAPHICS_INVALID_VIDPN;
    };
    let h_set = a.hMonitorSourceModeSet;
    let mut mode: *mut D3DKMDT_MONITOR_SOURCE_MODE = null_mut();
    // SAFETY: valid out-pointer.
    let st = unsafe { create(h_set, &mut mode) };
    if !ok(st) || mode.is_null() {
        return if ok(st) {
            STATUS_GRAPHICS_INVALID_VIDPN
        } else {
            st
        };
    }
    // SAFETY: `mode` is writable.
    unsafe {
        (*mode).VideoSignalInfo = video_signal_info(w, h);
        (*mode).ColorBasis = _D3DKMDT_COLOR_BASIS::D3DKMDT_CB_SRGB;
        (*mode).ColorCoeffDynamicRanges.FirstChannel = 8;
        (*mode).ColorCoeffDynamicRanges.SecondChannel = 8;
        (*mode).ColorCoeffDynamicRanges.ThirdChannel = 8;
        (*mode).ColorCoeffDynamicRanges.FourthChannel = 8;
        (*mode).Origin = _D3DKMDT_MONITOR_CAPABILITIES_ORIGIN::D3DKMDT_MCO_DRIVER;
        (*mode).Preference = _D3DKMDT_MODE_PREFERENCE::D3DKMDT_MP_PREFERRED;
    }
    // SAFETY: add/release take the mode we filled.
    let st = unsafe { add(h_set, mode) };
    if !ok(st) {
        let _ = unsafe { release(h_set, mode) };
        if st == STATUS_GRAPHICS_MODE_ALREADY_IN_MODESET {
            return STATUS_SUCCESS;
        }
        // Record the raw pfnAddMode failure so we can tell if the MONITOR mode is
        // what a callback rejects (the caller legalizes the escaping status).
        rec(b"VpMMe", st as u32);
        return st;
    }
    STATUS_SUCCESS
}

/// Where `enum_cofunc_modality`'s error ladder stopped.
///
/// The `fp` local these replace was an undocumented magic number set at eight
/// `break` sites and used only as the `VpECf` registry breadcrumb. The numbers
/// are owner debugging ABI, so they are mapped in exactly ONE place here and
/// stay byte-identical.
#[derive(Clone, Copy)]
enum CofuncStage {
    VidPnInterface,
    Topology,
    FirstPath,
    AcquireSourceSet,
    AcquireSourcePinned,
    ReleaseSourceSet,
    CreateSourceSet,
    AddSourceModes,
    AssignSourceSet,
    AcquireTargetSet,
    AcquireTargetPinned,
    ReleaseTargetSet,
    CreateTargetSet,
    AddTargetMode,
    AssignTargetSet,
    UpdatePathSupport,
    NextPath,
    /// A required callback slot was NULL. Never had an `fp` value — it broke
    /// with `fp` still 0, and this preserves that exactly.
    MissingCallback,
}

impl CofuncStage {
    /// The `VpECf` value. Do NOT renumber.
    const fn code(self) -> u32 {
        match self {
            Self::MissingCallback => 0,
            Self::VidPnInterface => 1,
            Self::Topology => 2,
            Self::FirstPath => 3,
            Self::AcquireSourceSet => 10,
            Self::AcquireSourcePinned => 12,
            Self::ReleaseSourceSet => 13,
            Self::CreateSourceSet => 14,
            Self::AddSourceModes => 15,
            Self::AssignSourceSet => 16,
            Self::AcquireTargetSet => 20,
            Self::AcquireTargetPinned => 22,
            Self::ReleaseTargetSet => 23,
            Self::CreateTargetSet => 24,
            Self::AddTargetMode => 25,
            Self::AssignTargetSet => 26,
            Self::UpdatePathSupport => 30,
            Self::NextPath => 31,
        }
    }
}

/// An acquired-or-created SOURCE mode set, released on drop.
///
/// The function acquires and releases four kinds of OS-owned object across a
/// long error ladder, with a manual release call at every numbered break site.
/// Two arms leaked a created-but-unassigned set — fixed by hand in T1a
/// (k-display-13); this makes the omission unrepresentable. A set that is
/// created and never assigned stays acquired inside dxgkrnl for the constraining
/// VidPn's lifetime and poisons every later acquire for the same source, turning
/// a transient rejection into a persistent 0-path VidPn.
///
/// ⚠ Ownership transfers to the VidPn on a SUCCESSFUL assign and only then, so
/// [`Self::assign`] consumes the guard and forgets it only on success. A `Drop`
/// that ran after a successful assign would be a DOUBLE-RELEASE into dxgkrnl.
struct SourceModeSet<'a> {
    vidpn: &'a DXGK_VIDPN_INTERFACE,
    h_vidpn: D3DKMDT_HVIDPN,
    source_id: u32,
    h_set: D3DKMDT_HVIDPNSOURCEMODESET,
    iface: *const DXGK_VIDPNSOURCEMODESET_INTERFACE,
}

impl SourceModeSet<'_> {
    /// Hand the set to the VidPn. Consumes the guard; forgets it ONLY on
    /// success, so a failed assign still releases.
    fn assign(self) -> NTSTATUS {
        let Some(assign) = self.vidpn.pfnAssignSourceModeSet else {
            return STATUS_GRAPHICS_INVALID_VIDPN;
        };
        // SAFETY: live VidPn interface and handles for the duration of the DDI.
        let st = unsafe { assign(self.h_vidpn, self.source_id, self.h_set) };
        if ok(st) {
            // Ownership moved to the VidPn — releasing now would double-release.
            core::mem::forget(self);
        }
        st
    }

    /// Release explicitly and report the status, for the one arm that must check
    /// it (the pinned-null path releases the acquired set before creating a
    /// replacement).
    fn release_now(self) -> NTSTATUS {
        let Some(release) = self.vidpn.pfnReleaseSourceModeSet else {
            return STATUS_GRAPHICS_INVALID_VIDPN;
        };
        // SAFETY: live VidPn interface and handles for the duration of the DDI.
        let st = unsafe { release(self.h_vidpn, self.h_set) };
        core::mem::forget(self);
        st
    }
}

impl Drop for SourceModeSet<'_> {
    fn drop(&mut self) {
        let Some(release) = self.vidpn.pfnReleaseSourceModeSet else {
            return;
        };
        // SAFETY: the handle came from a successful acquire/create on this same
        // VidPn and has not been assigned (assign forgets the guard).
        let _ = unsafe { release(self.h_vidpn, self.h_set) };
    }
}

/// An acquired-or-created TARGET mode set. Same contract as [`SourceModeSet`].
struct TargetModeSet<'a> {
    vidpn: &'a DXGK_VIDPN_INTERFACE,
    h_vidpn: D3DKMDT_HVIDPN,
    target_id: u32,
    h_set: D3DKMDT_HVIDPNTARGETMODESET,
    iface: *const DXGK_VIDPNTARGETMODESET_INTERFACE,
}

impl TargetModeSet<'_> {
    fn assign(self) -> NTSTATUS {
        let Some(assign) = self.vidpn.pfnAssignTargetModeSet else {
            return STATUS_GRAPHICS_INVALID_VIDPN;
        };
        // SAFETY: live VidPn interface and handles for the duration of the DDI.
        let st = unsafe { assign(self.h_vidpn, self.target_id, self.h_set) };
        if ok(st) {
            core::mem::forget(self);
        }
        st
    }

    fn release_now(self) -> NTSTATUS {
        let Some(release) = self.vidpn.pfnReleaseTargetModeSet else {
            return STATUS_GRAPHICS_INVALID_VIDPN;
        };
        // SAFETY: live VidPn interface and handles for the duration of the DDI.
        let st = unsafe { release(self.h_vidpn, self.h_set) };
        core::mem::forget(self);
        st
    }
}

impl Drop for TargetModeSet<'_> {
    fn drop(&mut self) {
        let Some(release) = self.vidpn.pfnReleaseTargetModeSet else {
            return;
        };
        // SAFETY: as SourceModeSet::drop.
        let _ = unsafe { release(self.h_vidpn, self.h_set) };
    }
}

/// A pinned mode-info reference held on a source mode set.
///
/// Borrows the set, so it can never outlive it, and Rust drops locals in reverse
/// declaration order — which is exactly the required order: release the mode
/// info, then the set.
struct PinnedSourceMode<'a> {
    set: &'a SourceModeSet<'a>,
    mode: *const D3DKMDT_VIDPN_SOURCE_MODE,
}

impl Drop for PinnedSourceMode<'_> {
    fn drop(&mut self) {
        // SAFETY: `iface` came from the same acquire that produced `h_set`.
        if let Some(release_mode) = unsafe { (*self.set.iface).pfnReleaseModeInfo } {
            // SAFETY: `mode` is the pinned info this set handed us.
            let _ = unsafe { release_mode(self.set.h_set, self.mode) };
        }
    }
}

/// As [`PinnedSourceMode`], for a target mode set.
struct PinnedTargetMode<'a> {
    set: &'a TargetModeSet<'a>,
    mode: *const D3DKMDT_VIDPN_TARGET_MODE,
}

impl Drop for PinnedTargetMode<'_> {
    fn drop(&mut self) {
        // SAFETY: `iface` came from the same acquire that produced `h_set`.
        if let Some(release_mode) = unsafe { (*self.set.iface).pfnReleaseModeInfo } {
            // SAFETY: `mode` is the pinned info this set handed us.
            let _ = unsafe { release_mode(self.set.h_set, self.mode) };
        }
    }
}

/// One present-path reference from a topology, released on drop.
struct PathInfo<'a> {
    topo: &'a DXGK_VIDPNTOPOLOGY_INTERFACE,
    h_topo: D3DKMDT_HVIDPNTOPOLOGY,
    path: *const D3DKMDT_VIDPN_PRESENT_PATH,
}

impl<'a> PathInfo<'a> {
    /// The first path, or `None` if the topology is empty.
    ///
    /// # Safety
    /// `h_topo` and `topo` are live for the duration of the DDI call.
    unsafe fn first(
        topo: &'a DXGK_VIDPNTOPOLOGY_INTERFACE,
        h_topo: D3DKMDT_HVIDPNTOPOLOGY,
    ) -> Result<Option<Self>, NTSTATUS> {
        let Some(acquire_first) = topo.pfnAcquireFirstPathInfo else {
            return Err(STATUS_GRAPHICS_INVALID_VIDPN);
        };
        let mut path = null();
        // SAFETY: valid out-pointer; per the fn contract.
        let st = unsafe { acquire_first(h_topo, &mut path) };
        if st == STATUS_GRAPHICS_NO_MORE_ELEMENTS_IN_DATASET {
            return Ok(None);
        }
        if !ok(st) {
            return Err(st);
        }
        Ok(Some(Self { topo, h_topo, path }))
    }

    fn get(&self) -> &D3DKMDT_VIDPN_PRESENT_PATH {
        // SAFETY: a live path-info reference from this topology.
        unsafe { &*self.path }
    }

    /// Advance. CONSUMES this path — which releases it, on every outcome.
    ///
    /// That matches the hand-written dance exactly:
    ///   * success            -> the old code called `release_path(prev)` and
    ///                           continued; the guard's drop does the same.
    ///   * NO_MORE_ELEMENTS   -> that status is warning-severity, so `ok()` was
    ///                           true and `release_path(prev)` ALSO ran before
    ///                           the loop exited. Same.
    ///   * a real failure     -> the old code set `path = prev` and let the final
    ///                           cleanup release it. Same.
    /// The new path is never released here, and on NO_MORE_ELEMENTS it is never
    /// wrapped at all — the old code was explicit that it is invalid.
    fn next(self) -> Result<Option<Self>, NTSTATUS> {
        let Some(acquire_next) = self.topo.pfnAcquireNextPathInfo else {
            return Err(STATUS_GRAPHICS_INVALID_VIDPN);
        };
        let mut next = null();
        // SAFETY: live handles for the duration of the DDI call.
        let st = unsafe { acquire_next(self.h_topo, self.path, &mut next) };
        if st == STATUS_GRAPHICS_NO_MORE_ELEMENTS_IN_DATASET {
            return Ok(None);
        }
        if !ok(st) {
            return Err(st);
        }
        Ok(Some(Self {
            topo: self.topo,
            h_topo: self.h_topo,
            path: next,
        }))
    }
}

impl Drop for PathInfo<'_> {
    fn drop(&mut self) {
        let Some(release) = self.topo.pfnReleasePathInfo else {
            return;
        };
        // SAFETY: a live path-info reference from this topology, released once.
        let _ = unsafe { release(self.h_topo, self.path) };
    }
}

/// `DxgkDdiEnumVidPnCofuncModality` body — populate cofunctional source/target
/// mode sets for every unpinned end of every path in the constraining VidPn,
/// and advertise Identity scaling/rotation. Mirrors viogpudo's implementation
/// (single mode).
///
/// Cleanup contract, corrected: the final cleanup releases the path-info
/// reference ONLY. Every mode set created by `pfnCreateNewSourceModeSet` /
/// `pfnCreateNewTargetModeSet` must be released on its own failure path, because
/// a set that was never assigned stays acquired inside dxgkrnl for the lifetime
/// of the constraining VidPn and poisons later acquires for the same source.
///
/// # Safety
/// `arg` points to a valid `DXGKARG_ENUMVIDPNCOFUNCMODALITY` for the call.
pub unsafe fn enum_cofunc_modality(
    adapter: &AdapterContext,
    arg: *const DXGKARG_ENUMVIDPNCOFUNCMODALITY,
) -> NTSTATUS {
    rec(b"VpEC", 1);
    // Flush the live CRTC_VSYNC heartbeat count (incremented at DISPATCH in the
    // VSync DPC, which cannot touch the registry). This PASSIVE DDI runs ×dozens
    // during the mode-set retry loop, so `ScVs` shows whether VSync is flowing.
    rec(
        b"ScVs",
        adapter
            .vsync_count
            .load(core::sync::atomic::Ordering::Relaxed),
    );
    // Which numbered stage failed, if any. Replaces the bare `fp: u32`; the
    // registry value is unchanged (see `CofuncStage::code`).
    let mut stage = CofuncStage::MissingCallback;
    if arg.is_null() {
        return STATUS_GRAPHICS_INVALID_VIDPN;
    }
    // SAFETY: valid per fn contract.
    let a = unsafe { &*arg };
    let h_vidpn = a.hConstrainingVidPn;
    let (mode_w, mode_h) = adapter.display_mode();

    // Resolve the VidPn + topology interfaces (nothing held yet → early return).
    let vidpn = match unsafe { vidpn_interface(adapter, h_vidpn) } {
        Ok(p) => p,
        Err(e) => {
            rec(b"VpECe", e as u32);
            rec(b"VpECf", CofuncStage::VidPnInterface.code());
            return legalize_vidpn(e);
        }
    };
    // SAFETY: valid for the call.
    let vidpn = unsafe { &*vidpn };
    let Some(get_topology) = vidpn.pfnGetTopology else {
        return STATUS_GRAPHICS_INVALID_VIDPN;
    };
    let mut h_topo: D3DKMDT_HVIDPNTOPOLOGY = null_mut();
    let mut topo_iface: *const DXGK_VIDPNTOPOLOGY_INTERFACE = null();
    // SAFETY: valid out-pointers.
    let st = unsafe { get_topology(h_vidpn, &mut h_topo, &mut topo_iface) };
    if !ok(st) || topo_iface.is_null() {
        if !ok(st) {
            rec(b"VpECe", st as u32);
            rec(b"VpECf", CofuncStage::Topology.code());
        }
        return if ok(st) {
            STATUS_GRAPHICS_INVALID_VIDPN
        } else {
            legalize_vidpn(st)
        };
    }
    // SAFETY: valid for the call.
    let topo = unsafe { &*topo_iface };
    // Record how many present paths the OS put in the constraining VidPn: `VpECp`=0
    // ⇒ the OS synthesized NO source→target path (target-availability problem —
    // forceable/AlwaysConnected target fix); `VpECp`≥1 ⇒ a path exists and any
    // remaining failure is downstream (mode/flip). 36th-session disambiguator.
    if let Some(get_num) = topo.pfnGetNumPaths {
        let mut n: SIZE_T = 0;
        // SAFETY: valid out-pointer.
        let _ = unsafe { get_num(h_topo, &mut n) };
        rec(b"VpECp", n as u32);
    }
    // Kept as an up-front check, exactly as before, so this early return stays
    // byte-identical (no VpECe/VpECf record, raw STATUS_GRAPHICS_INVALID_VIDPN).
    // It also makes the `else` arms inside `PathInfo`'s methods unreachable.
    let (Some(_), Some(_), Some(_), Some(update_support)) = (
        topo.pfnAcquireFirstPathInfo,
        topo.pfnAcquireNextPathInfo,
        topo.pfnReleasePathInfo,
        topo.pfnUpdatePathSupportInfo,
    ) else {
        return STATUS_GRAPHICS_INVALID_VIDPN;
    };

    let mut status = STATUS_SUCCESS;
    let mut current = match unsafe { PathInfo::first(topo, h_topo) } {
        Ok(first) => first,
        Err(e) => {
            rec(b"VpECe", e as u32);
            rec(b"VpECf", CofuncStage::FirstPath.code());
            return legalize_vidpn(e);
        }
    };

    // Iterate paths. Every OS-owned object acquired below is held by a guard, so
    // a `break` out of this loop releases exactly what is still owned — the
    // hand-matched release ladder is gone.
    while let Some(path) = current {
        let p = path.get();
        let source_id = p.VidPnSourceId;
        let target_id = p.VidPnTargetId;

        // ── source mode set (unless this source is the caller's pivot) ──────
        if !(a.EnumPivotType == _D3DKMDT_ENUMCOFUNCMODALITY_PIVOT_TYPE::D3DKMDT_EPT_VIDPNSOURCE
            && a.EnumPivot.VidPnSourceId == source_id)
        {
            let (Some(acquire_src), Some(create_src)) = (
                vidpn.pfnAcquireSourceModeSet,
                vidpn.pfnCreateNewSourceModeSet,
            ) else {
                status = STATUS_GRAPHICS_INVALID_VIDPN;
                break;
            };
            let mut h_set: D3DKMDT_HVIDPNSOURCEMODESET = null_mut();
            let mut set_iface: *const DXGK_VIDPNSOURCEMODESET_INTERFACE = null();
            // SAFETY: valid out-pointers.
            status = unsafe { acquire_src(h_vidpn, source_id, &mut h_set, &mut set_iface) };
            if !ok(status) {
                stage = CofuncStage::AcquireSourceSet;
                break;
            }
            let set = SourceModeSet {
                vidpn,
                h_vidpn,
                source_id,
                h_set,
                iface: set_iface,
            };
            let Some(acquire_pinned) = (unsafe { (*set.iface).pfnAcquirePinnedModeInfo }) else {
                status = STATUS_GRAPHICS_INVALID_VIDPN;
                break; // `set` drops → released
            };
            let mut pinned: *const D3DKMDT_VIDPN_SOURCE_MODE = null();
            // SAFETY: valid out-pointer.
            status = unsafe { acquire_pinned(set.h_set, &mut pinned) };
            if !ok(status) {
                stage = CofuncStage::AcquireSourcePinned;
                break; // `set` drops → released
            }
            if pinned.is_null() {
                // No pinned source mode → replace the set with every format the
                // primary/scanout path carries end-to-end. This release's status
                // is checked, so it is explicit rather than a drop.
                status = set.release_now();
                if !ok(status) {
                    stage = CofuncStage::ReleaseSourceSet;
                    break;
                }
                let mut h_new: D3DKMDT_HVIDPNSOURCEMODESET = null_mut();
                let mut new_iface: *const DXGK_VIDPNSOURCEMODESET_INTERFACE = null();
                // SAFETY: valid out-pointers.
                status = unsafe { create_src(h_vidpn, source_id, &mut h_new, &mut new_iface) };
                if !ok(status) {
                    stage = CofuncStage::CreateSourceSet;
                    break;
                }
                let created = SourceModeSet {
                    vidpn,
                    h_vidpn,
                    source_id,
                    h_set: h_new,
                    iface: new_iface,
                };
                // SAFETY: live set interface + handle.
                status = unsafe { add_source_modes(created.iface, created.h_set, mode_w, mode_h) };
                if !ok(status) {
                    stage = CofuncStage::AddSourceModes;
                    break; // `created` drops → released
                }
                // Consumes the guard; forgets it ONLY on success, so a failed
                // assign still releases and cannot orphan the set inside dxgkrnl.
                status = created.assign();
                if !ok(status) {
                    stage = CofuncStage::AssignSourceSet;
                    break;
                }
            } else {
                // A pinned mode exists — the guard releases the mode info, and
                // `set` (declared first) releases after it.
                let _pinned = PinnedSourceMode { set: &set, mode: pinned };
            }
        }

        // ── target mode set (unless this target is the caller's pivot) ──────
        if !(a.EnumPivotType == _D3DKMDT_ENUMCOFUNCMODALITY_PIVOT_TYPE::D3DKMDT_EPT_VIDPNTARGET
            && a.EnumPivot.VidPnTargetId == target_id)
        {
            let (Some(acquire_tgt), Some(create_tgt)) = (
                vidpn.pfnAcquireTargetModeSet,
                vidpn.pfnCreateNewTargetModeSet,
            ) else {
                status = STATUS_GRAPHICS_INVALID_VIDPN;
                break;
            };
            let mut h_set: D3DKMDT_HVIDPNTARGETMODESET = null_mut();
            let mut set_iface: *const DXGK_VIDPNTARGETMODESET_INTERFACE = null();
            // SAFETY: valid out-pointers.
            status = unsafe { acquire_tgt(h_vidpn, target_id, &mut h_set, &mut set_iface) };
            if !ok(status) {
                stage = CofuncStage::AcquireTargetSet;
                break;
            }
            let set = TargetModeSet {
                vidpn,
                h_vidpn,
                target_id,
                h_set,
                iface: set_iface,
            };
            let Some(acquire_pinned) = (unsafe { (*set.iface).pfnAcquirePinnedModeInfo }) else {
                status = STATUS_GRAPHICS_INVALID_VIDPN;
                break; // `set` drops → released
            };
            let mut pinned: *const D3DKMDT_VIDPN_TARGET_MODE = null();
            // SAFETY: valid out-pointer.
            status = unsafe { acquire_pinned(set.h_set, &mut pinned) };
            if !ok(status) {
                stage = CofuncStage::AcquireTargetPinned;
                break; // `set` drops → released
            }
            if pinned.is_null() {
                status = set.release_now();
                if !ok(status) {
                    stage = CofuncStage::ReleaseTargetSet;
                    break;
                }
                let mut h_new: D3DKMDT_HVIDPNTARGETMODESET = null_mut();
                let mut new_iface: *const DXGK_VIDPNTARGETMODESET_INTERFACE = null();
                // SAFETY: valid out-pointers.
                status = unsafe { create_tgt(h_vidpn, target_id, &mut h_new, &mut new_iface) };
                if !ok(status) {
                    stage = CofuncStage::CreateTargetSet;
                    break;
                }
                let created = TargetModeSet {
                    vidpn,
                    h_vidpn,
                    target_id,
                    h_set: h_new,
                    iface: new_iface,
                };
                // SAFETY: live set interface + handle.
                status =
                    unsafe { add_single_target_mode(created.iface, created.h_set, mode_w, mode_h) };
                if !ok(status) {
                    stage = CofuncStage::AddTargetMode;
                    break; // `created` drops → released
                }
                status = created.assign();
                if !ok(status) {
                    stage = CofuncStage::AssignTargetSet;
                    break;
                }
            } else {
                let _pinned = PinnedTargetMode { set: &set, mode: pinned };
            }
        }

        // ── scaling / rotation support (unless pivoted) ─────────────────────
        // SAFETY: copy the 360-byte path so we can amend support flags.
        let mut local = *p;
        let mut modified = false;
        if !(a.EnumPivotType == _D3DKMDT_ENUMCOFUNCMODALITY_PIVOT_TYPE::D3DKMDT_EPT_SCALING
            && a.EnumPivot.VidPnSourceId == source_id
            && a.EnumPivot.VidPnTargetId == target_id)
            && p.ContentTransformation.Scaling
                == _D3DKMDT_VIDPN_PRESENT_PATH_SCALING::D3DKMDT_VPPS_UNPINNED
        {
            // SAFETY: zero the bitfield then set the supported flags.
            unsafe {
                core::ptr::write_bytes(
                    &mut local.ContentTransformation.ScalingSupport as *mut _ as *mut u8,
                    0,
                    core::mem::size_of::<D3DKMDT_VIDPN_PRESENT_PATH_SCALING_SUPPORT>(),
                );
            }
            local.ContentTransformation.ScalingSupport.set_Identity(1);
            local.ContentTransformation.ScalingSupport.set_Centered(1);
            modified = true;
        }
        if !(a.EnumPivotType == _D3DKMDT_ENUMCOFUNCMODALITY_PIVOT_TYPE::D3DKMDT_EPT_ROTATION
            && a.EnumPivot.VidPnSourceId == source_id
            && a.EnumPivot.VidPnTargetId == target_id)
            && p.ContentTransformation.Rotation
                == _D3DKMDT_VIDPN_PRESENT_PATH_ROTATION::D3DKMDT_VPPR_UNPINNED
        {
            // SAFETY: zero the bitfield then set Identity (MOA_NONE → no rotate).
            unsafe {
                core::ptr::write_bytes(
                    &mut local.ContentTransformation.RotationSupport as *mut _ as *mut u8,
                    0,
                    core::mem::size_of::<D3DKMDT_VIDPN_PRESENT_PATH_ROTATION_SUPPORT>(),
                );
            }
            local.ContentTransformation.RotationSupport.set_Identity(1);
            // WDDM 1.3 path-independent rotation and later require every
            // primary clone path to advertise exactly one offset.  A
            // test-signed driver that reports none is deliberately bugchecked
            // by dxgkrnl while UpdatePathSupportInfo validates the path.
            local.ContentTransformation.RotationSupport.set_Offset0(1);
            modified = true;
        }
        if modified {
            // SAFETY: `local` is a filled path-info copy.
            status = unsafe { update_support(h_topo, &local) };
            if !ok(status) {
                stage = CofuncStage::UpdatePathSupport;
                break; // `path` drops → released
            }
        }

        // ── advance, releasing the previous ─────────────────────────────────
        // `next` CONSUMES `path`, so the previous reference is released on every
        // outcome — success, end-of-set, and failure alike.
        match path.next() {
            Ok(next) => current = next,
            Err(e) => {
                // `next` already consumed (and released) the path it was called
                // on, so there is nothing left to clean up here.
                status = e;
                stage = CofuncStage::NextPath;
                break;
            }
        }
    }
    // A raw callback failure here MUST NOT escape as an out-of-contract NTSTATUS
    // (dxgkrnl would flag the driver buggy and discard every VidPn — the 36th
    // session's 0-paths root cause). Record the raw status + which stage broke,
    // then clamp to a legal graphics status.
    if status != STATUS_SUCCESS {
        rec(b"VpECe", status as u32); // raw failing NTSTATUS (full 32-bit)
        rec(b"VpECf", stage.code()); // fail-point id (see `CofuncStage::code`)
        COFUNC_ERR_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        rec(
            b"VpECn",
            COFUNC_ERR_COUNT.load(core::sync::atomic::Ordering::Relaxed),
        );
        status = legalize_vidpn(status);
    }
    rec(b"VpECr", status as u32 & 0xFFFF);
    status
}

/// Return the number of present paths in `h_vidpn`'s topology, or `u32::MAX` if it
/// can't be resolved. Diagnostic-only (IsSupportedVidPn path-count instrumentation).
///
/// # Safety
/// `adapter.dxgkrnl` is the live interface; `h_vidpn` is valid for the call.
pub unsafe fn topology_path_count(adapter: &AdapterContext, h_vidpn: D3DKMDT_HVIDPN) -> u32 {
    if h_vidpn.is_null() {
        return 0;
    }
    let Ok(vidpn) = (unsafe { vidpn_interface(adapter, h_vidpn) }) else {
        return u32::MAX;
    };
    // SAFETY: valid for the call.
    let vidpn = unsafe { &*vidpn };
    let Some(get_topo) = vidpn.pfnGetTopology else {
        return u32::MAX;
    };
    let mut h_topo: D3DKMDT_HVIDPNTOPOLOGY = null_mut();
    let mut topo_iface: *const DXGK_VIDPNTOPOLOGY_INTERFACE = null();
    // SAFETY: valid out-pointers.
    if !ok(unsafe { get_topo(h_vidpn, &mut h_topo, &mut topo_iface) }) || topo_iface.is_null() {
        return u32::MAX;
    }
    // SAFETY: valid for the call.
    let Some(get_num) = (unsafe { (*topo_iface).pfnGetNumPaths }) else {
        return u32::MAX;
    };
    let mut n: SIZE_T = 0;
    // SAFETY: valid out-pointer.
    let _ = unsafe { get_num(h_topo, &mut n) };
    n as u32
}

/// `DxgkDdiCommitVidPn` body — validate the committed VidPn end-to-end and, most
/// importantly, RECORD whether the OS pinned a source mode on `AffectedVidPnSourceId`.
///
/// This is the fork-in-the-road diagnostic (WINDOWED_BLT_DESIGN §6): a bare
/// `return SUCCESS` commit that never inspects the pin is exactly the viogpu3d
/// "commits but lights nothing → mode-set retry loop, no SetVidPnSourceAddress"
/// failure (`viogpu_vidpn.cpp:270`). We accept the commit (there is no legacy mode
/// engine to program — the scanout is issued from `SetVidPnSourceAddress` via
/// `SET_SCANOUT_BLOB`), but the records tell us THIS boot whether negotiation
/// actually produced a pinned source mode:
///   `VpCN` = number of paths in the committed VidPn
///   `VpCP` = 1 if a source mode is pinned on our source, else 0
///   `VpCW` = pinned source mode `(width << 16) | height` (0 if none)
///
/// # Safety
/// `arg` points to a valid `DXGKARG_COMMITVIDPN` for the call.
pub unsafe fn commit_vidpn(adapter: &AdapterContext, arg: *const DXGKARG_COMMITVIDPN) -> NTSTATUS {
    if arg.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: valid per fn contract.
    let a = unsafe { &*arg };
    // A powered-off path commit carries no active source — accept without probing.
    if a.Flags.PathPoweredOff() != 0 {
        rec(b"VpCP", 0);
        return STATUS_SUCCESS;
    }
    let h_vidpn = a.hFunctionalVidPn;
    let source_id = a.AffectedVidPnSourceId;

    let vidpn = match unsafe { vidpn_interface(adapter, h_vidpn) } {
        Ok(p) => p,
        // Malformed handle: accept the commit but record we couldn't inspect it.
        Err(_) => {
            rec(b"VpCP", 0xE);
            return STATUS_SUCCESS;
        }
    };
    // SAFETY: valid for the call.
    let vidpn = unsafe { &*vidpn };

    // Path count (diagnostic only).
    if let (Some(get_topo), Some(_)) = (vidpn.pfnGetTopology, vidpn.pfnAcquireSourceModeSet) {
        let mut h_topo: D3DKMDT_HVIDPNTOPOLOGY = null_mut();
        let mut topo_iface: *const DXGK_VIDPNTOPOLOGY_INTERFACE = null();
        // SAFETY: valid out-pointers.
        if ok(unsafe { get_topo(h_vidpn, &mut h_topo, &mut topo_iface) }) && !topo_iface.is_null() {
            // SAFETY: valid for the call.
            if let Some(get_num) = unsafe { (*topo_iface).pfnGetNumPaths } {
                let mut n: SIZE_T = 0;
                // SAFETY: valid out-pointer.
                let _ = unsafe { get_num(h_topo, &mut n) };
                rec(b"VpCN", n as u32);
            }
        }
    }

    // The decisive probe: is a source mode pinned on our source?
    //
    // Read-only — this arm never assigns, so its `SourceModeSet` is always
    // released by drop and `assign`/`release_now` are never reached from here.
    let (Some(acquire_src), Some(_)) =
        (vidpn.pfnAcquireSourceModeSet, vidpn.pfnReleaseSourceModeSet)
    else {
        rec(b"VpCP", 0xF);
        return STATUS_SUCCESS;
    };
    let mut h_set: D3DKMDT_HVIDPNSOURCEMODESET = null_mut();
    let mut set_iface: *const DXGK_VIDPNSOURCEMODESET_INTERFACE = null();
    // SAFETY: valid out-pointers.
    let st = unsafe { acquire_src(h_vidpn, source_id, &mut h_set, &mut set_iface) };
    if !ok(st) || set_iface.is_null() {
        // Source not in this VidPn's topology → not the active source this commit.
        rec(b"VpCP", 0);
        return STATUS_SUCCESS;
    }
    let set = SourceModeSet {
        vidpn,
        h_vidpn,
        source_id,
        h_set,
        iface: set_iface,
    };
    let mut pinned = 0u32;
    let mut wh = 0u32;
    // SAFETY: `iface` came from the acquire above.
    if let Some(acquire_pinned) = unsafe { (*set.iface).pfnAcquirePinnedModeInfo } {
        let mut mode: *const D3DKMDT_VIDPN_SOURCE_MODE = null();
        // SAFETY: valid out-pointer.
        if ok(unsafe { acquire_pinned(set.h_set, &mut mode) }) && !mode.is_null() {
            // Guarded so the mode info is released before the set, on every exit
            // from this block.
            let held = PinnedSourceMode { set: &set, mode };
            pinned = 1;
            // SAFETY: `mode` is a live pinned source mode; Graphics is the arm
            // source modes use (Format is a Copy union — read is unsafe).
            let g = unsafe { &(*held.mode).Format.Graphics };
            wh = ((g.PrimSurfSize.cx as u32) << 16) | (g.PrimSurfSize.cy as u32 & 0xFFFF);
        }
    }
    // `set` drops here, releasing the source mode set we acquired.
    drop(set);

    rec(b"VpCP", pinned);
    rec(b"VpCW", wh);
    STATUS_SUCCESS
}
