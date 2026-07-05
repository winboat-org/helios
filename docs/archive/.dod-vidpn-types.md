# DOD VidPN FFI types (extracted verbatim from dxgk_bindings.rs)

Source: `kmd/target/debug/build/helios_kmd-*/out/dxgk_bindings.rs` (100874 lines).

Bindgen convention note: enum constants live ONLY inside their generated module
`pub mod _<TAG> { pub type Type = c_int; pub const VARIANT: Type = N; }` and the tag is
re-exported as a type alias (`pub use self::_<TAG>::Type as <TAG>;`). There is generally
NO top-level alias for the *constant* — you must write `_<TAG>::VARIANT` (or `use _<TAG>::*`).
The lone exception found is `DXGK_VIDPN_INTERFACE_VERSION_V1` which ALSO exists as a
top-level `pub const`.

================================================================================
## SECTION 1 — pfn typedefs + modeset interface structs
================================================================================

### DXGK_VIDPN_INTERFACE members

```rust
pub type DXGKDDI_VIDPN_GETTOPOLOGY = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPn: IN_CONST_D3DKMDT_HVIDPN,
        phVidPnTopology: OUT_PD3DKMDT_HVIDPNTOPOLOGY,
        ppVidPnTopologyInterface: DEREF_OUT_CONST_PPDXGK_VIDPNTOPOLOGY_INTERFACE,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPN_ACQUIRESOURCEMODESET = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPn: IN_CONST_D3DKMDT_HVIDPN,
        VidPnSourceId: IN_CONST_D3DDDI_VIDEO_PRESENT_SOURCE_ID,
        phVidPnSourceModeSet: OUT_PD3DKMDT_HVIDPNSOURCEMODESET,
        ppVidPnSourceModeSetInterface: DEREF_OUT_CONST_PPDXGK_VIDPNSOURCEMODESET_INTERFACE,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPN_RELEASESOURCEMODESET = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPn: IN_CONST_D3DKMDT_HVIDPN,
        hVidPnSourceModeSet: IN_CONST_D3DKMDT_HVIDPNSOURCEMODESET,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPN_CREATENEWSOURCEMODESET = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPn: IN_CONST_D3DKMDT_HVIDPN,
        VidPnSourceId: IN_CONST_D3DDDI_VIDEO_PRESENT_SOURCE_ID,
        phNewVidPnSourceModeSet: OUT_PD3DKMDT_HVIDPNSOURCEMODESET,
        ppVidPnSourceModeSetInterface: DEREF_OUT_CONST_PPDXGK_VIDPNSOURCEMODESET_INTERFACE,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPN_ASSIGNSOURCEMODESET = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPn: IN_D3DKMDT_HVIDPN,
        VidPnSourceId: IN_CONST_D3DDDI_VIDEO_PRESENT_SOURCE_ID,
        hVidPnSourceModeSet: IN_CONST_D3DKMDT_HVIDPNSOURCEMODESET,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPN_ACQUIRETARGETMODESET = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPn: IN_CONST_D3DKMDT_HVIDPN,
        VidPnTargetId: IN_CONST_D3DDDI_VIDEO_PRESENT_TARGET_ID,
        phVidPnTargetModeSet: OUT_PD3DKMDT_HVIDPNTARGETMODESET,
        ppVidPnTargetModeSetInterface: DEREF_OUT_CONST_PPDXGK_VIDPNTARGETMODESET_INTERFACE,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPN_RELEASETARGETMODESET = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPn: IN_CONST_D3DKMDT_HVIDPN,
        hVidPnTargetModeSet: IN_CONST_D3DKMDT_HVIDPNTARGETMODESET,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPN_CREATENEWTARGETMODESET = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPn: IN_CONST_D3DKMDT_HVIDPN,
        VidPnTargetId: IN_CONST_D3DDDI_VIDEO_PRESENT_TARGET_ID,
        phNewVidPnTargetModeSet: OUT_PD3DKMDT_HVIDPNTARGETMODESET,
        ppVidPnTargetModeSetInterace: DEREF_OUT_CONST_PPDXGK_VIDPNTARGETMODESET_INTERFACE,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPN_ASSIGNTARGETMODESET = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPn: IN_D3DKMDT_HVIDPN,
        VidPnTargetId: IN_CONST_D3DDDI_VIDEO_PRESENT_TARGET_ID,
        hVidPnTargetModeSet: IN_CONST_D3DKMDT_HVIDPNTARGETMODESET,
    ) -> NTSTATUS,
>;
```

### DXGK_VIDPNTOPOLOGY_INTERFACE members

```rust
pub type DXGKDDI_VIDPNTOPOLOGY_GETNUMPATHS = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPnTopology: IN_CONST_D3DKMDT_HVIDPNTOPOLOGY,
        pNumPaths: OUT_PSIZE_T,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPNTOPOLOGY_GETNUMPATHSFROMSOURCE = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPnTopology: IN_CONST_D3DKMDT_HVIDPNTOPOLOGY,
        VidPnSourceId: IN_CONST_D3DDDI_VIDEO_PRESENT_SOURCE_ID,
        pNumPathsFromSource: OUT_PSIZE_T,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPNTOPOLOGY_ENUMPATHTARGETSFROMSOURCE = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPnTopology: IN_CONST_D3DKMDT_HVIDPNTOPOLOGY,
        VidPnSourceId: IN_CONST_D3DDDI_VIDEO_PRESENT_SOURCE_ID,
        VidPnPresentPathIndex: IN_CONST_D3DKMDT_VIDPN_PRESENT_PATH_INDEX,
        pVidPnTargetId: OUT_PD3DDDI_VIDEO_PRESENT_TARGET_ID,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPNTOPOLOGY_ACQUIREPATHINFO = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPnTopology: IN_CONST_D3DKMDT_HVIDPNTOPOLOGY,
        VidPnSourceId: IN_CONST_D3DDDI_VIDEO_PRESENT_SOURCE_ID,
        VidPnTargetId: IN_CONST_D3DDDI_VIDEO_PRESENT_TARGET_ID,
        ppVidPnPresentPathInfo: DEREF_OUT_CONST_PPD3DKMDT_VIDPN_PRESENT_PATH,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPNTOPOLOGY_ACQUIREFIRSTPATHINFO = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPnTopology: IN_CONST_D3DKMDT_HVIDPNTOPOLOGY,
        ppFirstVidPnPresentPathInfo: DEREF_OUT_CONST_PPD3DKMDT_VIDPN_PRESENT_PATH,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPNTOPOLOGY_ACQUIRENEXTPATHINFO = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPnTopology: IN_CONST_D3DKMDT_HVIDPNTOPOLOGY,
        pVidPnPresentPathInfo: IN_CONST_PD3DKMDT_VIDPN_PRESENT_PATH_CONST,
        ppNextVidPnPresentPathInfo: DEREF_OUT_CONST_PPD3DKMDT_VIDPN_PRESENT_PATH,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPNTOPOLOGY_UPDATEPATHSUPPORTINFO = ::core::option::Option<
    unsafe extern "C" fn(
        i_hVidPnTopology: IN_CONST_D3DKMDT_HVIDPNTOPOLOGY,
        i_pVidPnPresentPathInfo: IN_CONST_PD3DKMDT_VIDPN_PRESENT_PATH,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPNTOPOLOGY_RELEASEPATHINFO = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPnTopology: IN_CONST_D3DKMDT_HVIDPNTOPOLOGY,
        pVidPnPresentPathInfo: IN_CONST_PD3DKMDT_VIDPN_PRESENT_PATH_CONST,
    ) -> NTSTATUS,
>;
```

### Modeset interface structs (public alias = name without leading underscore)

```rust
pub struct _DXGK_VIDPNSOURCEMODESET_INTERFACE {
    pub pfnGetNumModes: DXGKDDI_VIDPNSOURCEMODESET_GETNUMMODES,
    pub pfnAcquireFirstModeInfo: DXGKDDI_VIDPNSOURCEMODESET_ACQUIREFIRSTMODEINFO,
    pub pfnAcquireNextModeInfo: DXGKDDI_VIDPNSOURCEMODESET_ACQUIRENEXTMODEINFO,
    pub pfnAcquirePinnedModeInfo: DXGKDDI_VIDPNSOURCEMODESET_ACQUIREPINNEDMODEINFO,
    pub pfnReleaseModeInfo: DXGKDDI_VIDPNSOURCEMODESET_RELEASEMODEINFO,
    pub pfnCreateNewModeInfo: DXGKDDI_VIDPNSOURCEMODESET_CREATENEWMODEINFO,
    pub pfnAddMode: DXGKDDI_VIDPNSOURCEMODESET_ADDMODE,
    pub pfnPinMode: DXGKDDI_VIDPNSOURCEMODESET_PINMODE,
}
pub struct _DXGK_VIDPNTARGETMODESET_INTERFACE {
    pub pfnGetNumModes: DXGKDDI_VIDPNTARGETMODESET_GETNUMMODES,
    pub pfnAcquireFirstModeInfo: DXGKDDI_VIDPNTARGETMODESET_ACQUIREFIRSTMODEINFO,
    pub pfnAcquireNextModeInfo: DXGKDDI_VIDPNTARGETMODESET_ACQUIRENEXTMODEINFO,
    pub pfnAcquirePinnedModeInfo: DXGKDDI_VIDPNTARGETMODESET_ACQUIREPINNEDMODEINFO,
    pub pfnReleaseModeInfo: DXGKDDI_VIDPNTARGETMODESET_RELEASEMODEINFO,
    pub pfnCreateNewModeInfo: DXGKDDI_VIDPNTARGETMODESET_CREATENEWMODEINFO,
    pub pfnAddMode: DXGKDDI_VIDPNTARGETMODESET_ADDMODE,
    pub pfnPinMode: DXGKDDI_VIDPNTARGETMODESET_PINMODE,
}
pub struct _DXGK_MONITORSOURCEMODESET_INTERFACE {
    pub pfnGetNumModes: DXGKDDI_MONITORSOURCEMODESET_GETNUMMODES,
    pub pfnAcquirePreferredModeInfo: DXGKDDI_MONITORSOURCEMODESET_ACQUIREPREFERREDMODEINFO,
    pub pfnAcquireFirstModeInfo: DXGKDDI_MONITORSOURCEMODESET_ACQUIREFIRSTMODEINFO,
    pub pfnAcquireNextModeInfo: DXGKDDI_MONITORSOURCEMODESET_ACQUIRENEXTMODEINFO,
    pub pfnCreateNewModeInfo: DXGKDDI_MONITORSOURCEMODESET_CREATENEWMODEINFO,
    pub pfnAddMode: DXGKDDI_MONITORSOURCEMODESET_ADDMODE,
    pub pfnReleaseModeInfo: DXGKDDI_MONITORSOURCEMODESET_RELEASEMODEINFO,
}
```

### Member pfn typedefs (Acquire/Release/CreateNew/Add)

```rust
pub type DXGKDDI_VIDPNSOURCEMODESET_ACQUIREPINNEDMODEINFO = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPnSourceModeSet: IN_CONST_D3DKMDT_HVIDPNSOURCEMODESET,
        ppPinnedVidPnSourceModeInfo: DEREF_OUT_CONST_PPD3DKMDT_VIDPN_SOURCE_MODE,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPNSOURCEMODESET_RELEASEMODEINFO = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPnSourceModeSet: IN_CONST_D3DKMDT_HVIDPNSOURCEMODESET,
        pVidPnSourceModeInfo: IN_CONST_PD3DKMDT_VIDPN_SOURCE_MODE_CONST,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPNSOURCEMODESET_CREATENEWMODEINFO = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPnSourceModeSet: IN_CONST_D3DKMDT_HVIDPNSOURCEMODESET,
        ppNewVidPnSourceModeInfo: DEREF_OUT_PPD3DKMDT_VIDPN_SOURCE_MODE,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPNSOURCEMODESET_ADDMODE = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPnSourceModeSet: IN_D3DKMDT_HVIDPNSOURCEMODESET,
        pVidPnSourceModeInfo: IN_PD3DKMDT_VIDPN_SOURCE_MODE_CONST,
    ) -> NTSTATUS,
>;

pub type DXGKDDI_VIDPNTARGETMODESET_ACQUIREPINNEDMODEINFO = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPnTargetModeSet: IN_CONST_D3DKMDT_HVIDPNTARGETMODESET,
        ppPinnedVidPnTargetModeInfo: DEREF_OUT_CONST_PPD3DKMDT_VIDPN_TARGET_MODE,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPNTARGETMODESET_RELEASEMODEINFO = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPnTargetModeSet: IN_CONST_D3DKMDT_HVIDPNTARGETMODESET,
        pVidPnTargetModeInfo: IN_CONST_PD3DKMDT_VIDPN_TARGET_MODE_CONST,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPNTARGETMODESET_CREATENEWMODEINFO = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPnTargetModeSet: IN_CONST_D3DKMDT_HVIDPNTARGETMODESET,
        ppNewVidPnTargetModeInfo: DEREF_OUT_PPD3DKMDT_VIDPN_TARGET_MODE,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_VIDPNTARGETMODESET_ADDMODE = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPnTargetModeSet: IN_D3DKMDT_HVIDPNTARGETMODESET,
        pVidPnTargetModeInfo: IN_PD3DKMDT_VIDPN_TARGET_MODE_CONST,
    ) -> NTSTATUS,
>;

pub type DXGKDDI_MONITORSOURCEMODESET_ACQUIREPREFERREDMODEINFO = ::core::option::Option<
    unsafe extern "C" fn(
        hMonitorSourceModeSet: IN_CONST_D3DKMDT_HMONITORSOURCEMODESET,
        ppFirstMonitorSourceModeInfo: DEREF_OUT_CONST_PPD3DKMDT_MONITOR_SOURCE_MODE,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_MONITORSOURCEMODESET_RELEASEMODEINFO = ::core::option::Option<
    unsafe extern "C" fn(
        hMonitorSourceModeSet: IN_CONST_D3DKMDT_HMONITORSOURCEMODESET,
        pMonitorSourceModeInfo: IN_CONST_PD3DKMDT_MONITOR_SOURCE_MODE_CONST,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_MONITORSOURCEMODESET_CREATENEWMODEINFO = ::core::option::Option<
    unsafe extern "C" fn(
        hMonitorSourceModeSet: IN_CONST_D3DKMDT_HMONITORSOURCEMODESET,
        ppNewMonitorSourceModeInfo: DEREF_OUT_PPD3DKMDT_MONITOR_SOURCE_MODE,
    ) -> NTSTATUS,
>;
pub type DXGKDDI_MONITORSOURCEMODESET_ADDMODE = ::core::option::Option<
    unsafe extern "C" fn(
        hMonitorSourceModeSet: IN_CONST_D3DKMDT_HMONITORSOURCEMODESET,
        pMonitorSourceModeInfo: IN_PD3DKMDT_MONITOR_SOURCE_MODE_CONST,
    ) -> NTSTATUS,
>;
```

================================================================================
## SECTION 2 — argument structs
================================================================================

```rust
pub struct _DXGKARG_ENUMVIDPNCOFUNCMODALITY {
    pub hConstrainingVidPn: D3DKMDT_HVIDPN,
    pub EnumPivotType: D3DKMDT_ENUMCOFUNCMODALITY_PIVOT_TYPE,   // = _D3DKMDT_ENUMCOFUNCMODALITY_PIVOT_TYPE::Type
    pub EnumPivot: DXGK_ENUM_PIVOT,
}
pub struct _DXGKARG_COMMITVIDPN {
    pub hFunctionalVidPn: D3DKMDT_HVIDPN,
    pub AffectedVidPnSourceId: D3DDDI_VIDEO_PRESENT_SOURCE_ID,
    pub MonitorConnectivityChecks: D3DKMDT_MONITOR_CONNECTIVITY_CHECKS,
    pub hPrimaryAllocation: HANDLE,
    pub Flags: DXGKARG_COMMITVIDPN_FLAGS,
}
pub struct _DXGKARG_ISSUPPORTEDVIDPN {
    pub hDesiredVidPn: D3DKMDT_HVIDPN,
    pub IsVidPnSupported: BOOLEAN,
}
pub struct _DXGKARG_RECOMMENDMONITORMODES {
    pub VideoPresentTargetId: D3DDDI_VIDEO_PRESENT_TARGET_ID,
    pub hMonitorSourceModeSet: D3DKMDT_HMONITORSOURCEMODESET,
    pub pMonitorSourceModeSetInterface: *const DXGK_MONITORSOURCEMODESET_INTERFACE,
}
// NOTE: _DXGK_ENUM_PIVOT is rendered as a plain struct here (NOT a union) —
// both fields are present at once:
pub struct _DXGK_ENUM_PIVOT {
    pub VidPnSourceId: D3DDDI_VIDEO_PRESENT_SOURCE_ID,
    pub VidPnTargetId: D3DDDI_VIDEO_PRESENT_TARGET_ID,
}
```

================================================================================
## SECTION 3 — mode / path structs (+ nested anon)
================================================================================

```rust
pub struct _D3DDDI_RATIONAL {
    pub Numerator: UINT,
    pub Denominator: UINT,
}
pub struct _D3DKMDT_2DREGION {
    pub cx: UINT,
    pub cy: UINT,
}

pub struct _D3DKMDT_VIDPN_SOURCE_MODE {
    pub Id: D3DKMDT_VIDEO_PRESENT_SOURCE_MODE_ID,
    pub Type: D3DKMDT_VIDPN_SOURCE_MODE_TYPE,          // set via _D3DKMDT_VIDPN_SOURCE_MODE_TYPE::D3DKMDT_RMT_GRAPHICS
    pub Format: _D3DKMDT_VIDPN_SOURCE_MODE__bindgen_ty_1,   // union { Graphics, Text }
}
pub union _D3DKMDT_VIDPN_SOURCE_MODE__bindgen_ty_1 {
    pub Graphics: D3DKMDT_GRAPHICS_RENDERING_FORMAT,
    pub Text: D3DKMDT_TEXT_RENDERING_FORMAT,
}
// => write: src_mode.Format.Graphics.<field>
pub struct _D3DKMDT_GRAPHICS_RENDERING_FORMAT {
    pub PrimSurfSize: D3DKMDT_2DREGION,
    pub VisibleRegionSize: D3DKMDT_2DREGION,
    pub Stride: DWORD,
    pub PixelFormat: D3DDDIFORMAT,                     // = _D3DDDIFORMAT::Type; use _D3DDDIFORMAT::D3DDDIFMT_A8R8G8B8
    pub ColorBasis: D3DKMDT_COLOR_BASIS,               // = _D3DKMDT_COLOR_BASIS::Type
    pub PixelValueAccessMode: D3DKMDT_PIXEL_VALUE_ACCESS_MODE,
}

pub struct _D3DKMDT_VIDEO_SIGNAL_INFO {
    pub VideoStandard: D3DKMDT_VIDEO_SIGNAL_STANDARD,  // = _D3DKMDT_VIDEO_SIGNAL_STANDARD::Type
    pub TotalSize: D3DKMDT_2DREGION,
    pub ActiveSize: D3DKMDT_2DREGION,
    pub VSyncFreq: D3DDDI_RATIONAL,
    pub HSyncFreq: D3DDDI_RATIONAL,
    pub PixelRate: SIZE_T,
    pub __bindgen_anon_1: _D3DKMDT_VIDEO_SIGNAL_INFO__bindgen_ty_1,   // union
}
pub union _D3DKMDT_VIDEO_SIGNAL_INFO__bindgen_ty_1 {
    pub AdditionalSignalInfo: _D3DKMDT_VIDEO_SIGNAL_INFO__bindgen_ty_1__bindgen_ty_1, // bitfield struct
    pub ScanLineOrdering: D3DDDI_VIDEO_SIGNAL_SCANLINE_ORDERING,                       // <-- set this one
}
pub struct _D3DKMDT_VIDEO_SIGNAL_INFO__bindgen_ty_1__bindgen_ty_1 {
    pub _bitfield_align_1: [u32; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}
// => write: sig.__bindgen_anon_1.ScanLineOrdering = _D3DDDI_VIDEO_SIGNAL_SCANLINE_ORDERING::D3DDDI_VSSLO_PROGRESSIVE;

pub struct _D3DKMDT_VIDPN_TARGET_MODE {
    pub Id: D3DKMDT_VIDEO_PRESENT_TARGET_MODE_ID,
    pub VideoSignalInfo: D3DKMDT_VIDEO_SIGNAL_INFO,
    pub __bindgen_anon_1: _D3DKMDT_VIDPN_TARGET_MODE__bindgen_ty_1,   // union (preference / wireformat bitfields)
    pub MinimumVSyncFreq: D3DDDI_RATIONAL,
}
pub union _D3DKMDT_VIDPN_TARGET_MODE__bindgen_ty_1 {
    pub WireFormatAndPreference: D3DKMDT_WIRE_FORMAT_AND_PREFERENCE,
    pub __bindgen_anon_1: _D3DKMDT_VIDPN_TARGET_MODE__bindgen_ty_1__bindgen_ty_1,
}
pub struct _D3DKMDT_VIDPN_TARGET_MODE__bindgen_ty_1__bindgen_ty_1 {
    pub _bitfield_align_1: [u8; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}

pub struct _D3DKMDT_MONITOR_SOURCE_MODE {
    pub Id: D3DKMDT_MONITOR_SOURCE_MODE_ID,
    pub VideoSignalInfo: D3DKMDT_VIDEO_SIGNAL_INFO,
    pub ColorBasis: D3DKMDT_COLOR_BASIS,                          // _D3DKMDT_COLOR_BASIS::Type
    pub ColorCoeffDynamicRanges: D3DKMDT_COLOR_COEFF_DYNAMIC_RANGES,
    pub Origin: D3DKMDT_MONITOR_CAPABILITIES_ORIGIN,             // _D3DKMDT_MONITOR_CAPABILITIES_ORIGIN::D3DKMDT_MCO_DRIVER
    pub Preference: D3DKMDT_MODE_PREFERENCE,                     // _D3DKMDT_MODE_PREFERENCE::D3DKMDT_MP_PREFERRED
}

pub struct _D3DKMDT_VIDPN_PRESENT_PATH {
    pub VidPnSourceId: D3DDDI_VIDEO_PRESENT_SOURCE_ID,
    pub VidPnTargetId: D3DDDI_VIDEO_PRESENT_TARGET_ID,
    pub ImportanceOrdinal: D3DKMDT_VIDPN_PRESENT_PATH_IMPORTANCE,
    pub ContentTransformation: D3DKMDT_VIDPN_PRESENT_PATH_TRANSFORMATION,
    pub VisibleFromActiveTLOffset: D3DKMDT_2DOFFSET,
    pub VisibleFromActiveBROffset: D3DKMDT_2DOFFSET,
    pub VidPnTargetColorBasis: D3DKMDT_COLOR_BASIS,
    pub VidPnTargetColorCoeffDynamicRanges: D3DKMDT_COLOR_COEFF_DYNAMIC_RANGES,
    pub Content: D3DKMDT_VIDPN_PRESENT_PATH_CONTENT,
    pub CopyProtection: D3DKMDT_VIDPN_PRESENT_PATH_COPYPROTECTION,
    pub GammaRamp: D3DKMDT_GAMMA_RAMP,
}
pub struct _D3DKMDT_VIDPN_PRESENT_PATH_TRANSFORMATION {
    pub Scaling: D3DKMDT_VIDPN_PRESENT_PATH_SCALING,             // _D3DKMDT_VIDPN_PRESENT_PATH_SCALING::D3DKMDT_VPPS_UNPINNED
    pub ScalingSupport: D3DKMDT_VIDPN_PRESENT_PATH_SCALING_SUPPORT,  // bitfield struct, setters below
    pub Rotation: D3DKMDT_VIDPN_PRESENT_PATH_ROTATION,          // _D3DKMDT_VIDPN_PRESENT_PATH_ROTATION::D3DKMDT_VPPR_UNPINNED
    pub RotationSupport: D3DKMDT_VIDPN_PRESENT_PATH_ROTATION_SUPPORT, // bitfield struct, setters below
}
pub struct _D3DKMDT_VIDPN_PRESENT_PATH_SCALING_SUPPORT {
    pub _bitfield_align_1: [u8; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 1usize]>,
    pub __bindgen_padding_0: [u8; 3usize],
}
// setters (UINT 0/1): set_Identity, set_Centered, set_Stretched,
//                     set_AspectRatioCenteredMax, set_Custom  (+ new_bitfield_1)
pub struct _D3DKMDT_VIDPN_PRESENT_PATH_ROTATION_SUPPORT {
    pub _bitfield_align_1: [u8; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 1usize]>,
    pub __bindgen_padding_0: [u8; 3usize],
}
// setters (UINT 0/1): set_Identity, set_Rotate90, set_Rotate180, set_Rotate270,
//                     set_Offset0, set_Offset90 (+ more)
```

NOTE: `_D3DKMDT_2DOFFSET` is NOT emitted as its own `pub struct` (used only via the
`D3DKMDT_2DOFFSET` type alias / inline). The fields are the usual `cx`/`cy` pair.

================================================================================
## SECTION 4 — DxgkCbQueryVidPnInterface
================================================================================

```rust
// In struct _DXGKRNL_INTERFACE (offset 144):
pub DxgkCbQueryVidPnInterface: DXGKCB_QUERYVIDPNINTERFACE,

pub type DXGKCB_QUERYVIDPNINTERFACE = ::core::option::Option<
    unsafe extern "C" fn(
        hVidPn: IN_CONST_D3DKMDT_HVIDPN,
        VidPnInterfaceVersion: IN_CONST_DXGK_VIDPN_INTERFACE_VERSION,
        ppVidPnInterface: DEREF_OUT_CONST_PPDXGK_VIDPN_INTERFACE,
    ) -> NTSTATUS,
>;

// Version const — available BOTH top-level and in the mod:
pub const DXGK_VIDPN_INTERFACE_VERSION_V1: Type = 1;          // top-level usable directly
pub mod _DXGK_VIDPN_INTERFACE_VERSION {
    pub const DXGK_VIDPN_INTERFACE_VERSION_UNINITIALIZED: Type = 0;
    pub const DXGK_VIDPN_INTERFACE_VERSION_V1: Type = 1;
    pub const DXGK_VIDPN_INTERFACE_VERSION_V2: Type = 2;
}
// IN_CONST_DXGK_VIDPN_INTERFACE_VERSION == DXGK_VIDPN_INTERFACE_VERSION == _DXGK_VIDPN_INTERFACE_VERSION::Type
```

================================================================================
## SECTION 5 — enum constant Rust paths + values
================================================================================

All of these (except where noted) are ONLY inside their `pub mod _<TAG>` — write `_<TAG>::VARIANT`:

| Constant                            | Value | Module (write `_TAG::VARIANT`)                       |
|-------------------------------------|-------|------------------------------------------------------|
| D3DKMDT_RMT_GRAPHICS                | 1     | _D3DKMDT_VIDPN_SOURCE_MODE_TYPE                       |
| D3DKMDT_CB_SCRGB                    | 3     | _D3DKMDT_COLOR_BASIS                                  |
| D3DKMDT_CB_SRGB                     | 2     | _D3DKMDT_COLOR_BASIS                                  |
| D3DKMDT_CB_UNINITIALIZED           | 0     | _D3DKMDT_COLOR_BASIS                                  |
| D3DKMDT_PVAM_DIRECT                 | 1     | _D3DKMDT_PIXEL_VALUE_ACCESS_MODE                     |
| D3DDDIFMT_A8R8G8B8                  | 21    | _D3DDDIFORMAT                                         |
| D3DKMDT_VSS_OTHER                   | 255   | _D3DKMDT_VIDEO_SIGNAL_STANDARD                       |
| D3DDDI_VSSLO_PROGRESSIVE           | 1     | _D3DDDI_VIDEO_SIGNAL_SCANLINE_ORDERING              |
| D3DKMDT_MCO_DRIVER                  | 5     | _D3DKMDT_MONITOR_CAPABILITIES_ORIGIN                |
| D3DKMDT_MP_PREFERRED               | 1     | _D3DKMDT_MODE_PREFERENCE                             |
| D3DKMDT_EPT_VIDPNSOURCE            | 1     | _D3DKMDT_ENUMCOFUNCMODALITY_PIVOT_TYPE              |
| D3DKMDT_EPT_VIDPNTARGET            | 2     | _D3DKMDT_ENUMCOFUNCMODALITY_PIVOT_TYPE              |
| D3DKMDT_VPPS_UNPINNED              | 254   | _D3DKMDT_VIDPN_PRESENT_PATH_SCALING                 |
| D3DKMDT_VPPR_UNPINNED              | 254   | _D3DKMDT_VIDPN_PRESENT_PATH_ROTATION                |
| DXGK_VIDPN_INTERFACE_VERSION_V1    | 1     | (top-level pub const) OR _DXGK_VIDPN_INTERFACE_VERSION |

### NOT PRESENT in the bindings (bindgen dropped these C `#define` macros)

These do NOT appear as `pub const` anywhere — define them yourself as literals.
Values verified against `ntstatus.h` (10.0.22621.0). NTSTATUS is `i32` in wdk-sys;
write e.g. `const STATUS_... : NTSTATUS = 0xC01E0339u32 as NTSTATUS;`

| Constant                                          | NTSTATUS value      |
|---------------------------------------------------|---------------------|
| STATUS_GRAPHICS_NO_MORE_ELEMENTS_IN_DATASET       | 0x401E034C  (SUCCESS-class, NOT 0xC...!) |
| STATUS_GRAPHICS_SOURCE_NOT_IN_TOPOLOGY            | 0xC01E0339          |
| STATUS_GRAPHICS_MODE_ALREADY_IN_MODESET           | 0xC01E0314          |
| STATUS_GRAPHICS_INVALID_VIDEO_PRESENT_SOURCE_MODE | 0xC01E0310          |
| (bonus) STATUS_GRAPHICS_TARGET_NOT_IN_TOPOLOGY    | 0xC01E0340          |
| (bonus) STATUS_GRAPHICS_PINNED_MODE_MUST_REMAIN_IN_SET | 0xC01E0312     |

D3DKMDT_FREQUENCY_NOTSPECIFIED: a macro dropped by bindgen; it is the D3DDDI_RATIONAL
`{ Numerator: 0, Denominator: 0 }` (zero rational), not a scalar enum.
