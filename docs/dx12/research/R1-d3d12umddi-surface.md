# R1 — The D3D12 UMD DDI surface, inventoried from `d3d12umddi.h`

**Lane:** R1 (complete inventory of the D3D12 user-mode driver DDI). This is the implementer's map
of "what must be written" if Helios ever takes strategy (a) from `DX12.md` §2 — a native D3D12 UMD.

**Primary source (all unqualified line numbers refer to it):**
`/home/rupansh/helios-vgpu/tmp/dx12/sdk/d3d12umddi.h`, **19 031 lines**, Windows SDK
**10.0.26100.0**. Verified: `wc -l tmp/dx12/sdk/*.h` → `19031 tmp/dx12/sdk/d3d12umddi.h`.

Secondary sources are named inline. Everything I could not establish from a file, a command, or a
URL carries the literal marker **UNVERIFIED** plus the experiment that would settle it.

**Evidence classes used below**, kept distinct on purpose:
- *"the header says"* — a quotable line from `d3d12umddi.h` (or another staged/VM header).
- *"measured on win11"* — a command I ran on the dev VM, with its output.
- *"I infer"* — a conclusion from structure, explicitly flagged. Never presented as fact.

---

## 0. Executive summary — the three numbers that matter

| Quantity | Value | Where |
|---|---|---|
| Function pointers in the newest core device table `D3D12DDI_DEVICE_FUNCS_CORE_0109` | **124** | L13451–13616 |
| Function pointers in the newest 3D command-list table `D3D12DDI_COMMAND_LIST_FUNCS_3D_0108` | **75** | L13303–13388 |
| Function pointers in the 3D command-queue table `D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001` | **7** | L2729–2738 |
| Function pointers in the adapter table `D3D12DDI_ADAPTERFUNCS_0109` | **8** | L13640–13650 |
| **Baseline (non-video, non-content-protection) driver-implemented total** | **214** | sum of the four |
| Runtime→driver UM callbacks the driver *consumes* (`…DEVICECALLBACKS_0062`) | **28** | L8606–8647 |
| Runtime→driver KT (kernel) callbacks the driver *consumes* (`D3DDDI_DEVICECALLBACKS`) | **65** | `d3dumddi.h` L4499ff (measured on win11) |
| Distinct `D3D12DDI_SUPPORTED_*` version constants in the header | **72** | `grep -c` below |
| Helios D3D11 UMD device-table slots actually filled today | **157** unique (+18 DXGI) | `umd/src/forward/tables.rs` |

So a D3D12 UMD is **~214 driver functions vs the D3D11 UMD's ~175** — the same order of magnitude
in *count*, but a materially different object model (§6) and, decisively, a different *memory and
addressing* contract (§4, §8): every D3D12 fence is a **GPU virtual address**, and Helios' guest GPU
VA is documented as decorative (`kmd_render/src/ddi/gpummu.rs:1-14`, quoted in `DX12.md` §3.3).

---

## 1. Entry point and negotiation

### 1.1 The export

**Measured on win11** — Microsoft's own reference D3D12 UMD (WARP) is the ground truth for the export
name, because the header only declares the *typedef*, never the symbol:

```
> & dumpbin.exe /exports C:\Windows\System32\d3d10warp.dll | Select-String "OpenAdapter"
        203    2 001CF510 OpenAdapter
        204    3 000FFF70 OpenAdapter10_2
        205    4 000FFBB0 OpenAdapter12
```
(`d3d10warp.dll`, version `10.0.26100.8875`, 5 931 008 bytes.)

So the D3D12 entry point is the exported symbol **`OpenAdapter12`**, matching what
`umd/src/adapter.rs:178` already exports and refuses. Note WARP exports plain `OpenAdapter`
(the D3D10 name) where Helios exports `OpenAdapter10` — both are accepted by the runtime.

⚠ **`OpenAdapter12` appears nowhere in Microsoft's public driver documentation.** Verified:
```
$ grep -rn "OpenAdapter12" windows-driver-docs-research-only/   # → no output
```
and `grep -rl "D3D12DDI" .../display/` returns only 12 files, all of them feature pages
(`d3d12-render-passes.md`, `work-graphs.md`, `enhanced-barriers.md`, `video-encoding-d3d12*.md`,
`gpu-paravirtualization.md`, `generic-programs.md`, `signaling-cpu-event-from-kmd.md`,
`what-s-new-*`), never the core contract. **The D3D12 UMD DDI is header-only, undocumented
territory.** This is the single largest risk multiplier for strategy (a): there is no MS prose
saying which functions are mandatory, in what order they are called, or what the runtime does with
a NULL slot.

### 1.2 `D3D12DDIARG_OPENADAPTER`

Verbatim, L2686–2694:

```c
typedef struct D3D12DDIARG_OPENADAPTER
{
    D3D12DDI_HRTADAPTER            hRTAdapter;         // in:  Runtime handle
    D3D12DDI_HADAPTER              hAdapter;           // out: Driver handle
    CONST D3DDDI_ADAPTERCALLBACKS* pAdapterCallbacks;  // in:  Pointer to runtime callbacks
    D3D12DDI_ADAPTERFUNCS*         pAdapterFuncs;      // out: Driver function table
} D3D12DDIARG_OPENADAPTER;

typedef HRESULT (APIENTRY *PFND3D12DDI_OPENADAPTER)(_Inout_ D3D12DDIARG_OPENADAPTER*);
```

**Four fields only.** Contrast the D3D10/11 form, which carries `Interface` and `Version` *in the
open-adapter argument* (`d3dumddi.h`, measured on win11:
`typedef struct _D3DDDIARG_OPENADAPTER { HANDLE hAdapter; UINT Interface; UINT Version; … }`).
**In D3D12, version negotiation happens *after* `OpenAdapter12`, not in it** — the runtime gets the
adapter-funcs table first and then calls `pfnGetSupportedVersions`. This is a genuine shape
difference from the D3D11 path `umd/src/adapter.rs::open_adapter_common` implements today.

The only runtime callback table handed in at adapter scope is `D3DDDI_ADAPTERCALLBACKS`
(three entries — measured on win11, `d3dumddi.h`):
`pfnQueryAdapterInfoCb`, `pfnGetMultisampleMethodListCb`, `pfnQueryAdapterInfoCb2`.
That is the same table `umd/` already receives for D3D11.

### 1.3 The adapter funcs table

Two versions exist in the header, and they differ **only** in the `pfnCreateDevice` signature:

`D3D12DDI_ADAPTERFUNCS` (L2674–2684) and `D3D12DDI_ADAPTERFUNCS_0109` (L13640–13650), both **8
members**:

```c
typedef struct D3D12DDI_ADAPTERFUNCS_0109
{
    PFND3D12DDI_CALCPRIVATEDEVICESIZE         pfnCalcPrivateDeviceSize;
    PFND3D12DDI_CREATEDEVICE_0109             pfnCreateDevice;      // 0003 in the base version
    PFND3D12DDI_CLOSEADAPTER                  pfnCloseAdapter;
    PFND3D12DDI_GETSUPPORTEDVERSIONS          pfnGetSupportedVersions;
    PFND3D12DDI_GETCAPS                       pfnGetCaps;
    PFND3D12DDI_GETOPTIONALDDITTABLES         pfnGetOptionalDDITables;
    PFND3D12DDI_FILLDDITTABLE                 pfnFillDDITable;
    PFND3D12DDI_DESTROYDEVICE                 pfnDestroyDevice;
} D3D12DDI_ADAPTERFUNCS_0109;
```

Signatures (L2604–2620, L2622):

```c
typedef SIZE_T  (APIENTRY *PFND3D12DDI_CALCPRIVATEDEVICESIZE)(D3D12DDI_HADAPTER, _In_ CONST D3D12DDIARG_CALCPRIVATEDEVICESIZE*);
typedef HRESULT (APIENTRY *PFND3D12DDI_CLOSEADAPTER)(D3D12DDI_HADAPTER);
typedef HRESULT (APIENTRY *PFND3D12DDI_GETSUPPORTEDVERSIONS)(D3D12DDI_HADAPTER,
    _Inout_ UINT32* puEntries, _Out_writes_opt_( *puEntries ) UINT64* pSupportedDDIInterfaceVersions);
typedef HRESULT (APIENTRY *PFND3D12DDI_GETCAPS)(D3D12DDI_HADAPTER, _In_ CONST D3D12DDIARG_GETCAPS*);
typedef VOID    (APIENTRY *PFND3D12DDI_DESTROYDEVICE)(D3D12DDI_HDEVICE);
```

`pfnGetSupportedVersions` is the classic **two-call query**: called once with `pSupportedDDIInterfaceVersions == NULL`
to learn the count, then again with a buffer. (I infer the two-call shape from the
`_Inout_ UINT32* puEntries` + `_Out_writes_opt_` annotation pair; the header states no prose. Same
shape as `pfnGetOptionalDDITables`.)

`D3D12DDIARG_GETCAPS` (L2611–2617):
```c
typedef struct D3D12DDIARG_GETCAPS { D3D12DDICAPS_TYPE Type; VOID* pInfo; VOID* pData; UINT DataSize; } D3D12DDIARG_GETCAPS;
```
Note **`pfnGetCaps` is on the ADAPTER, taking `D3D12DDI_HADAPTER`** — unlike D3D11 where caps are
adapter-scoped too but the D3D12 caps enum is far richer (§5). Several caps take an *input* through
`pInfo` (e.g. `NodeIndex` for `MEMORY_ARCHITECTURE` and `GPUVA_CAPS`, L152-155 / L250-253) and some
are in/out through `pData` (`D3D12DDI_3DPIPELINESUPPORT1_DATA_0081`, §5.3).

### 1.4 Device creation

`D3D12DDIARG_CREATEDEVICE_0109` verbatim (L13618–13636):

```c
typedef struct D3D12DDIARG_CREATEDEVICE_0109
{
    D3D12DDI_HRTDEVICE              hRTDevice;              // in:  Runtime handle
    UINT                            Interface;              // in:  Interface version
    UINT                            Version;                // in:  Runtime Version
    CONST D3DDDI_DEVICECALLBACKS*   pKTCallbacks;           // in:  Pointer to runtime callbacks that invoke kernel
    D3D12DDI_HDEVICE                hDrvDevice;             // in:  Driver private handle/ storage.
    union
    {
        CONST D3D12DDI_CORELAYER_DEVICECALLBACKS_0003* p12UMCallbacks;
        CONST struct D3D12DDI_CORELAYER_DEVICECALLBACKS_0022* p12UMCallbacks_0022;
        CONST struct D3D12DDI_CORELAYER_DEVICECALLBACKS_0050* p12UMCallbacks_0050;
        CONST struct D3D12DDI_CORELAYER_DEVICECALLBACKS_0062* p12UMCallbacks_0062;
    };
    D3D12DDI_CREATE_DEVICE_FLAGS    Flags;
    D3D12DDI_GPU_VIRTUAL_ADDRESS_RANGE* pReserveRanges;     // NEW in 0109
    UINT NumReserveRanges;                                  // NEW in 0109
} D3D12DDIARG_CREATEDEVICE_0109;
```

The base form `D3D12DDIARG_CREATEDEVICE_0003` (L2655–2670) is identical minus the two trailing
reserve-range fields.

⚠ **The `p12UMCallbacks` union is a landmine of exactly the class `adapter.rs:36-45` records for
D3D11** (a 376→392-byte OOB write from an `else`-as-default in interface dispatch). Which arm is
live is determined by the negotiated `Interface`/`Version`, and the four structs are **19 / 21 / 27 /
28 members** respectively — reading the wrong arm reads past the end of a shorter one.
`D3D12DDI_GPU_VIRTUAL_ADDRESS_RANGE` (L7964–7968) is `{ D3D12DDI_GPU_VIRTUAL_ADDRESS StartAddress;
UINT64 SizeInBytes; }` where `D3D12DDI_GPU_VIRTUAL_ADDRESS` is `UINT64` (L92).

`D3D12DDIARG_CALCPRIVATEDEVICESIZE` (L2595–2600) is `{ UINT Interface; UINT Version;
D3D12DDI_CREATE_DEVICE_FLAGS Flags; }`. `D3D12DDI_CREATE_DEVICE_FLAGS` (L2587–2593):
`NONE = 0x0`, `DISABLE_IMPLICIT_MGPU = 0x1`, `DEBUGGABLE = 0x2`.

### 1.5 Version-number encoding — the complete constant list

The header says (L38–56):

```c
#define D3D12DDI_MAJOR_VERSION 12
#define D3D12DDI_MINOR_VERSION 2
#define D3D12DDI_INTERFACE_VERSION ((D3D12DDI_MAJOR_VERSION << 16) | D3D12DDI_MINOR_VERSION)
#define D3D12DDI_BUILD_VERSION 8
#define D3D12DDI_SUPPORTED ((((UINT64)D3D12DDI_INTERFACE_VERSION) << 32) | (((UINT64)D3D12DDI_BUILD_VERSION) << 16))
#define D3D12DDI_INTERFACE_VERSION_R0       D3D12DDI_INTERFACE_VERSION
#define D3D12DDI_SUPPORTED_0003             D3D12DDI_SUPPORTED
```

So every `D3D12DDI_SUPPORTED_NNNN` is a **UINT64**:

```
value = ((UINT64)((12 << 16) | MINOR_R_n) << 32) | ((UINT64)BUILD << 16)
```

Release "minor" values, all from the header:

| Release | `D3D12DDI_MINOR_VERSION_Rn` | Line | `INTERFACE_VERSION_Rn` (hex) |
|---|---|---|---|
| R0 | 2 (`D3D12DDI_MINOR_VERSION`) | L39 | `0x000C0002` |
| R1 | 10 | L3182 | `0x000C000A` |
| R2 | 20 | L4055 | `0x000C0014` |
| R3 | 30 | L5914 | `0x000C001E` |
| R4 | 40 | L6532 | `0x000C0028` |
| R5 | 50 | L6998 | `0x000C0032` |
| R6 | 60 | L8438 | `0x000C003C` |
| R7 | 70 | L9006 | `0x000C0046` |
| R8 | 80 | L10148 (redefined identically at L10548) | `0x000C0050` |

Worked example, computed: `D3D12DDI_SUPPORTED_0110` = `0x000C0050_006E0000`;
`D3D12DDI_SUPPORTED_0080` = `0x000C0050_00500000`; `D3D12DDI_SUPPORTED_0003` = `0x000C0002_00080000`.

**Every `D3D12DDI_SUPPORTED_*` constant defined in the header — 72 of them**
(`grep -c "^#define D3D12DDI_SUPPORTED_" d3d12umddi.h` → `72`):

```
0003
0010 0011 0012 0013 0014 0015
0020 0021 0022 0023 0024 0025 0026 0027 0028
0030 0031 0032 0033 0034
0040 0041 0042 0043
0050 0051 0052 0053 0054
0060 0061 0062 0063 0064 0065
0070 0071 0072 0073 0074 0075 0076
0080 0081 0082 0083 0084 0086 0088 0089
0090 0091 0092 0093 0094 0095 0096 0097 0098 0099
0100 0101 0102 0103 0104 0105 0106 0107 0108 0109 0110
```
Gaps are real: there is no `…_0085` and no `…_0087` in this header.

⚠ **The header does NOT record which OS or SDK each version corresponds to.** Every
`// D3D12 Release N, Build rev M.` banner is followed by a *feature* description only, never an OS
build. **UNVERIFIED: the version→Windows-release mapping.** Settling read: the WDK "What's new for
Windows display drivers" pages
(`windows-driver-docs-research-only/windows-driver-docs-pr/display/what-s-new-for-windows-10-display-and-graphics-drivers.md`
and `what-s-new-for-prior-wddm-2-x-versions.md`) attribute *features* to WDDM versions; correlate
those feature names against the banner text below. That is an inference chain, not a lookup — treat
the result as a hypothesis, not a fact.

The banner feature text, extracted for the versions that matter to a bring-up (verbatim from the
header comments):

| Const | Line | Header's own description |
|---|---|---|
| `_0054` | L7678 | (R5 rev 4 — introduces raytracing / state objects; see the `pfnCreateStateObject` family L13590+) |
| `_0064` | L8987 | "This version is introduced to detect the presence of the SubmitHistorySequence callback in the KT callback table." |
| `_0070` | L9003 | "This adds new raytracing features." |
| `_0074` | L9638 | "Added mesh shader DDI" |
| `_0080` | L10145 | "Add driver managed shader cache control DDIs" |
| `_0081` | L10360 | "Add new 3DPipelineSupport1 cap, which allows drivers to report feature levels higher than 12_1…" |
| `_0088` | L10589 | "Add Create*Resource with initial D3D12_BARRIER_LAYOUT" |
| `_0090` | L11115 | "Change cap-adding convention to reduce bloat / Add RelaxedFormatCastingSupported cap" |
| `_0094` | L11262 | "Update D3D12DDI_RANGED_BARRIER" |
| `_0098` | L11897 | "Enable independent D3D12 devices / Add L1MemoryFullyCpuAccessible cap" |
| `_0099` | L11911 | "Add dynamic depth bias state … Change rasterizer desc DepthBias from INT to FLOAT" |
| `_0102` | L12471 | "Add VulkanOn12 compatibility features" |
| `_0106` | L12740 | "Add new caps for D3D_FEATURE_LEVEL_1_0_GENERIC optional features" |
| `_0108` | L12775 | "Work Graphs DDIs" |
| `_0109` | L13391 | "RecreateAt for Heaps and Resources" |
| `_0110` | L13653 | "Execute indirect tier 1.1: Incrementing constant" |

**Practical consequence for a Helios implementer:** you do not have to support 72 versions. You
report *one* `UINT64` from `pfnGetSupportedVersions` (or a short list) and fill the tables for
exactly that version. The lowest version that still has a *complete, coherent* modern table set is
worth choosing deliberately — see §9.

**A load-bearing inference, flagged:** I infer that `D3D12DDIARG_CREATEDEVICE::Interface` receives
the **high 32 bits** of the chosen `D3D12DDI_SUPPORTED_*` value (`D3D12DDI_INTERFACE_VERSION_Rn`)
and `::Version` the **low 32 bits** (`BuildVersion << 16`), because that is the only split of a
UINT64 into two UINTs consistent with the constant's construction and with the D3D10/11 convention
(`D3D10DDIARG_OPENADAPTER.Interface`/`.Version`). The header does not state it. **UNVERIFIED.**
Settling experiment: the proxy-DLL spy in §9.4 — log `Interface` and `Version` as WARP receives
them and compare against `D3D12DDI_SUPPORTED_*`.

---

## 2. The table model

### 2.1 `D3D12DDI_TABLE_TYPE` — every value

Verbatim, L2488–2516:

```c
typedef enum D3D12DDI_TABLE_TYPE
{
    D3D12DDI_TABLE_TYPE_DEVICE_CORE                                 = 0,
    D3D12DDI_TABLE_TYPE_COMMAND_LIST_3D                             = 1,
    D3D12DDI_TABLE_TYPE_COMMAND_QUEUE_3D                            = 2,
    D3D12DDI_TABLE_TYPE_DXGI                                        = 3,
    D3D12DDI_TABLE_TYPE_0020_DEVICE_VIDEO                           = 4,
    D3D12DDI_TABLE_TYPE_0020_DEVICE_CORE_VIDEO                      = 7,
    D3D12DDI_TABLE_TYPE_0020_EXTENDED_FEATURES                      = 8,
    D3D12DDI_TABLE_TYPE_0020_PASS_EXPERIMENT                        = 9,
    D3D12DDI_TABLE_TYPE_0021_SHADERCACHE_CALLBACKS                  = 10,
    D3D12DDI_TABLE_TYPE_0022_COMMAND_QUEUE_VIDEO_DECODE             = 11,
    D3D12DDI_TABLE_TYPE_0022_COMMAND_LIST_VIDEO_DECODE              = 12,
    D3D12DDI_TABLE_TYPE_0022_COMMAND_QUEUE_VIDEO_PROCESS            = 13,
    D3D12DDI_TABLE_TYPE_0022_COMMAND_LIST_VIDEO_PROCESS             = 14,
    D3D12DDI_TABLE_TYPE_0030_DEVICE_CONTENT_PROTECTION_RESOURCES    = 15,
    D3D12DDI_TABLE_TYPE_0030_CONTENT_PROTECTION_CALLBACKS           = 16,
    D3D12DDI_TABLE_TYPE_0030_DEVICE_CONTENT_PROTECTION_STREAMING    = 17,
    D3D12DDI_TABLE_TYPE_0033_METACOMMAND                            = 19,
    D3D12DDI_TABLE_TYPE_0043_RENDER_PASS                            = 20,
    D3D12DDI_TABLE_TYPE_0053_COMMAND_LIST_VIDEO_ENCODE              = 21,
    D3D12DDI_TABLE_TYPE_0053_COMMAND_QUEUE_VIDEO_ENCODE             = 22,
    D3D12DDI_TABLE_TYPE_0054_DOWNLEVEL_SUPPORT_CALLBACKS            = 23,
    D3D12DDI_TABLE_TYPE_0054_DEVICE_DOWNLEVEL_SUPPORT               = 24,
    D3D12DDI_TABLE_TYPE_0076_PIN_RESOURCES_CALLBACKS                = 25,
    D3D12DDI_TABLE_TYPE_0084_STATE_OBJECTS_EXPERIMENT               = 26,
    D3D12DDI_TABLE_TYPE_0096_EXTENDED_FEATURES                      = 27,
} D3D12DDI_TABLE_TYPE;
```

**24 values; 5, 6 and 18 are absent** (retired). Types 0–3 are the baseline; everything ≥4 is
video, content protection, or an *extended feature* (§2.3).

### 2.2 ⚠ Correction: there is no `pfnGetDDITable` / `pfnGetDDITable32`

The assignment brief names `pfnGetDDITable` / `pfnGetDDITable32`. **Those symbols do not exist in
this header.** Verified:

```
$ grep -rn "GETDDITABLE\|GetDDITable\|SETDDITABLE" tmp/dx12/sdk/*.h     # → no output
```

The actual mechanism is a **two-function pair on the adapter table** (L2518–2528):

```c
typedef struct D3D12DDI_TABLE_REQUEST
{
    D3D12DDI_TABLE_TYPE tableType;
    UINT                numTables;
} D3D12DDI_TABLE_REQUEST;

typedef HRESULT ( APIENTRY * PFND3D12DDI_GETOPTIONALDDITTABLES )(
    D3D12DDI_HADAPTER, _Inout_ UINT32* puEntries, _Out_writes_opt_( *puEntries ) D3D12DDI_TABLE_REQUEST* );

typedef HRESULT ( APIENTRY * PFND3D12DDI_FILLDDITTABLE )(
    D3D12DDI_HADAPTER, D3D12DDI_TABLE_TYPE, _Inout_ VOID*, SIZE_T, UINT, _In_opt_ D3D12DDI_HRTTABLE );
```

(The doubled `TT` in `GETOPTIONALDDITTABLES` / `FILLDDITTABLE` is Microsoft's typo and is load-bearing
— it is the actual identifier.)

Reading of `pfnFillDDITable`'s unnamed parameters, in order:
`(hAdapter, TableType, pTable /*inout, the runtime's buffer*/, TableSize /*SIZE_T*/,
 UINT /* version/index — see below */, hRTTable /*optional runtime handle for this table*/)`.

**UNVERIFIED: the meaning of the 5th parameter (`UINT`) and of `D3D12DDI_TABLE_REQUEST::numTables`.**
Two readings are consistent with the header: (i) the `UINT` is the *DDI version* the runtime wants
this table filled for; (ii) it is a *table index* in the range `[0, numTables)` for table types that
come in multiples (the `numTables` field in `TABLE_REQUEST` strongly suggests multiplicity exists).
Settling experiment: §9.4's proxy spy — log `(TableType, TableSize, UINT, hRTTable)` for every
`pfnFillDDITable` call the runtime makes into WARP, for a plain `D3D12CreateDevice` +
`CreateCommandQueue` + `CreateCommandList` sequence.

**Versioning per table is by struct, not by field.** There is no size/flags header inside the
tables; `D3D12DDI_DEVICE_FUNCS_CORE_0109` is simply a *different, longer* struct than
`…_CORE_0108`. The runtime passes `TableSize` and the driver must write exactly that many bytes'
worth of the version it agreed to. **This is the direct analogue of the D3D11 `DRIVERCAPS` UB that
R702 found** (24H2 passing 576 B for a 592 B struct — `DX12.md`-adjacent, memory
`t4b-landed-47th.md`): trusting `sizeof(struct)` instead of the runtime's `TableSize` is a heap
overwrite.

### 2.3 The reverse direction — runtime-provided callback tables

Three `TABLE_TYPE` values are **not filled by the driver**; the runtime hands them *to* the driver
through the extended-features mechanism. Confirmed by the direction annotation on
`PFND3D12DDI_SET_EXTENDED_FEATURE_CALLBACKS_0021` (L4100–4101):

```c
typedef HRESULT ( APIENTRY * PFND3D12DDI_SET_EXTENDED_FEATURE_CALLBACKS_0021 )(
    D3D12DDI_HDEVICE hDevice, D3D12DDI_TABLE_TYPE Table, _In_reads_(TableSize) const void* pTable, SIZE_T TableSize);
```

`_In_reads_` = the driver *reads* the table. The three are:

| Table type | Struct | Members | Line |
|---|---|---|---|
| `…_0021_SHADERCACHE_CALLBACKS` (10) | `D3D12DDI_SHADERCACHE_CALLBACKS_0021` | 2 (`pfnShaderCacheGetValueCb`, `pfnShaderCacheStoreValueCb`) | L4266 |
| `…_0030_CONTENT_PROTECTION_CALLBACKS` (16) | `D3D12DDI_CONTENT_PROTECTION_CALLBACKS_0030` | 1 | L13845 |
| `…_0054_DOWNLEVEL_SUPPORT_CALLBACKS` (23) | `D3D12DDI_DOWNLEVEL_SUPPORT_CALLBACKS_0054` | 3 (`pfnCreateSynchronizationObject2Cb`, `pfnWaitForSynchronizationObject2Cb`, `pfnSignalSynchronizationObject2Cb`) | L18305 |
| `…_0076_PIN_RESOURCES_CALLBACKS` (25) | `D3D12DDI_PIN_RESOURCES_CALLBACKS_0076` | 2 | L18380 |

The extended-feature negotiation itself is a driver-filled table
(`D3D12DDI_TABLE_TYPE_0020_EXTENDED_FEATURES` = 8, `…_0096_EXTENDED_FEATURES` = 27):

```c
typedef enum D3D12DDI_FEATURE_0020            // L4060-4073
{
    D3D12DDI_FEATURE_0020_VIDEO = 2,
    D3D12DDI_FEATURE_0020_PASS_EXPERIMENT = 3,
    D3D12DDI_FEATURE_0021_SHADERCACHING = 4,
    D3D12DDI_FEATURE_0030_CONTENT_PROTECTION_RESOURCES = 5,
    D3D12DDI_FEATURE_0030_CONTENT_PROTECTION_STREAMING = 6,
    D3D12DDI_FEATURE_0033_METACOMMAND = 9, //superseded with public APIs
    D3D12DDI_FEATURE_0043_RENDER_PASS = 10,
    D3D12DDI_FEATURE_0054_DOWNLEVEL_SUPPORT = 11,
    D3D12DDI_FEATURE_0076_PIN_RESOURCES = 12,
    D3D12DDI_FEATURE_0084_STATE_OBJECTS_EXPERIMENT = 13,
} D3D12DDI_FEATURE_0020;

typedef struct D3D12DDI_EXTENDED_FEATURES_FUNCS_0096   // L11879-11886, 4 members
{
    PFND3D12DDI_GET_SUPPORTED_EXTENDED_FEATURES_0096            pfnGetSupportedExtendedFeatures;
    PFND3D12DDI_GET_SUPPORTED_EXTENDED_FEATURE_VERSIONS_0020    pfnGetSupportedExtendedFeatureVersions;
    PFND3D12DDI_ENABLE_EXTENDED_FEATURE_0020                    pfnEnableExtendedFeature;
    PFND3D12DDI_SET_EXTENDED_FEATURE_CALLBACKS_0021             pfnSetExtendedFeatureCallbacks;
} D3D12DDI_EXTENDED_FEATURES_FUNCS_0096;
```

A baseline Helios D3D12 UMD can answer `pfnGetSupportedExtendedFeatures` with **zero features** and
never see any of the video / protection / pin-resources tables. That is the honest posture and it
matches the KMD's stance (`DX12.md` §3.5: protected-content DDIs unset).

### 2.4 Full table inventory in this header

Every `D3D12DDI_*FUNCS*` / `*CALLBACKS*` struct in the file, with member counts (generated by
script over the struct bodies; counts include `void*`-reserved slots):

**Baseline (a non-video, non-protected D3D12 device):**
```
D3D12DDI_ADAPTERFUNCS                        L 2674   8
D3D12DDI_ADAPTERFUNCS_0109                   L13640   8
D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001       L 2729   7
D3D12DDI_COMMAND_LIST_FUNCS_3D_0003…0108     L 2999…13303   51 → 75  (16 versions)
D3D12DDI_DEVICE_FUNCS_CORE_0003…0109         L 3060…13451   89 → 124 (31 versions)
D3D12DDI_CORELAYER_DEVICECALLBACKS_0003/0022/0050/0062   19 / 21 / 27 / 28
D3D12DDI_EXTENDED_FEATURES_FUNCS_0020/0021/0096          3 / 4 / 4
```

**Optional / feature tables (all skippable for a baseline device):**
```
D3D12DDI_SHADERCACHE_CALLBACKS_0021                       L 4266   2
D3D12DDI_DEVICE_FUNCS_CONTENT_PROTECTION_RESOURCES_0030   L13823   5
D3D12DDI_DEVICE_FUNCS_CONTENT_PROTECTION_RESOURCES_0074   L13901   5
D3D12DDI_CONTENT_PROTECTION_CALLBACKS_0030                L13845   1
D3D12DDI_DEVICE_FUNCS_CONTENT_PROTECTION_STREAMING_0030   L14075  12
D3D12DDI_DEVICE_FUNCS_VIDEO_0033/0043/0053/0060/0063/0072/0076/0080_2/0082_0   10…26
D3D12DDI_COMMAND_QUEUE_FUNCS_VIDEO_0020                   L 4713   5
D3D12DDI_COMMAND_LIST_FUNCS_VIDEO_{DECODE,PROCESS,ENCODE}_*                   12…18
D3D12DDI_PASS_EXPERIMENT_FUNCS_0020                       L17959   7
D3D12DDI_RENDER_PASS_FUNCS_0043 / _0053                   L18105/18254   2 / 2
D3D12DDI_DOWNLEVEL_SUPPORT_CALLBACKS_0054                 L18305   3
D3D12DDI_DEVICE_DOWNLEVEL_SUPPORT_FUNCS_0054              L18345   2
D3D12DDI_PIN_RESOURCES_CALLBACKS_0076                     L18380   2
D3D12DDI_STATE_OBJECTS_EXPERIMENT_FUNCS_0084              L18579   4
```

**`D3D12DDI_TABLE_TYPE_DXGI` (=3) has no struct in `d3d12umddi.h`.** Verified: the only `DXGI`
matches outside format/flag enums are L1620/1621 (`DXGI_DDI_ARG_BLT_FLAGS`, `DXGI_DDI_MODE_ROTATION`)
and L2493 (the enum value itself). The table therefore comes from `dxgiddi.h`. Measured on win11,
`C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0\um\dxgiddi.h` defines seven candidates:

```
658: DXGI_DDI_BASE_FUNCTIONS
670: DXGI1_1_DDI_BASE_FUNCTIONS
685: DXGI1_2_DDI_BASE_FUNCTIONS
710: DXGI1_3_DDI_BASE_FUNCTIONS
737: DXGI1_4_DDI_BASE_FUNCTIONS      (21 members)
767: DXGI1_5_DDI_BASE_FUNCTIONS      (22 members)
798: DXGI1_6_1_DDI_BASE_FUNCTIONS    (22 members; pfnPresent1 takes DXGI1_6_1_DDI_ARG_PRESENT)
```

**UNVERIFIED: which `DXGI*_DDI_BASE_FUNCTIONS` shape D3D12 requests for `TABLE_TYPE_DXGI`, and
whether it varies with the negotiated DDI version.** Settling experiment: the §9.4 proxy logs
`TableSize` for `TableType == 3`; `sizeof(DXGI1_6_1_DDI_BASE_FUNCTIONS)` = 22 × 8 = 176 bytes on
x64, `DXGI1_4` = 168, so the size alone identifies the struct. This is the hand-off point to lane R7
(presentation) — Helios' D3D11 UMD already implements 18 of these slots
(`umd/src/forward/tables.rs::install_dxgi`, `install_dxgi_1_1`, `install_dxgi_1_3`).

---

## 3. The device tables — full inventory

### 3.1 `D3D12DDI_DEVICE_FUNCS_CORE` — 31 versions, 89 → 124 members

Every version, with line span and exact member count (script over the struct bodies):

| Struct | Lines | Members | | Struct | Lines | Members |
|---|---|---|---|---|---|---|
| `_0003` | 3060–3176 | 89 | | `_0063` | 8835–8984 | 115 |
| `_0010` | 3351–3470 | 91 | | `_0070` | 9016–9166 | 116 |
| `_0012` | 3550–3670 | 92 | | `_0072` | 9193–9344 | 117 |
| `_0013` | 3762–3882 | 92 | | `_0073` | 9479–9635 | 118 |
| `_0014` | 3907–4027 | 92 | | `_0074` | 9725–9886 | 121 |
| `_0021` | 4113–4233 | 92 | | `_0075` | 9974–10135 | 121 |
| `_0022` | 4907–5027 | 92 | | `_0080` | 10195–10357 | 122 |
| `_0023` | 5162–5282 | 92 | | `_0088` | 10907–11069 | 122 |
| `_0025` | 5332–5452 | 92 | | `_0095` | 11418–11580 | 122 |
| `_0026` | 5572–5692 | 92 | | `_0096` | 11711–11873 | 122 |
| `_0030` | 5932–6052 | 92 | | `_0099` | 11984–12146 | 122 |
| `_0033` | 6401–6521 | 92 | | `_0100` | 12292–12454 | 122 |
| `_0040` | 6660–6785 | 96 | | `_0102` | 12516–12678 | 122 |
| `_0043` | 6868–6993 | 96 | | `_0108` | 13133–13298 | 124 |
| `_0050` | 7030–7159 | 99 | | `_0109` | 13451–13616 | **124** |
| `_0052` | 7404–7541 | 105 | | | | |
| `_0054` | 8211–8358 | 114 | | | | |
| `_0062` | 8671–8820 | 115 | | | | |

**The newest, `D3D12DDI_DEVICE_FUNCS_CORE_0109` (124 members), functionally grouped.** All names
verbatim; the group headings are mine.

*(a) Format / capability queries at device scope — 3*
```
pfnCheckFormatSupport                    (D3D12DDI_HDEVICE, DXGI_FORMAT, _Out_ UINT*)          L2936
pfnCheckMultisampleQualityLevels         (hDevice, DXGI_FORMAT, SampleCount, Flags, _Out_ UINT*) L2939
pfnGetMipPacking                         (hDevice, hTiledResource, _Out_ UINT* pNumPackedMips,
                                          _Out_ UINT* pNumTilesForPackedMips)                   L2947
```

*(b) Immutable pipeline sub-state objects — 12 (4 × Calc/Create/Destroy)*
```
pfnCalcPrivateElementLayoutSize / pfnCreateElementLayout / pfnDestroyElementLayout
pfnCalcPrivateBlendStateSize / pfnCreateBlendState / pfnDestroyBlendState
pfnCalcPrivateDepthStencilStateSize / pfnCreateDepthStencilState / pfnDestroyDepthStencilState
pfnCalcPrivateRasterizerStateSize / pfnCreateRasterizerState / pfnDestroyRasterizerState
```

*(c) Shaders — 13*
```
pfnCalcPrivateShaderSize, pfnCreateVertexShader, pfnCreatePixelShader, pfnCreateGeometryShader,
pfnCreateComputeShader, pfnCalcPrivateGeometryShaderWithStreamOutput,
pfnCreateGeometryShaderWithStreamOutput, pfnCalcPrivateTessellationShaderSize,
pfnCreateHullShader, pfnCreateDomainShader, pfnDestroyShader,
pfnCreateAmplificationShader, pfnCreateMeshShader   (+ pfnCalcPrivateMeshShaderSize in group (l))
```
All six of `CreateVertexShader … CreateDomainShader` in 0109 share **one** typedef
`PFND3D12DDI_CREATE_SHADER_0026`, and `pfnCalcPrivateTessellationShaderSize` is the *same* typedef
as `pfnCalcPrivateShaderSize` (`PFND3D12DDI_CALC_PRIVATE_SHADER_SIZE_0026`, L13473/13482) — a
simplification versus 0003, where hull/domain had their own `…_CREATE_TESS_SHADER_0003`.

*(d) Command queues, pools, lists, recorders — 15*
```
pfnCalcPrivateCommandQueueSize / pfnCreateCommandQueue / pfnDestroyCommandQueue
pfnCalcPrivateCommandPoolSize / pfnCreateCommandPool / pfnDestroyCommandPool / pfnResetCommandPool
pfnCalcPrivateCommandListSize / pfnCreateCommandList / pfnDestroyCommandList
pfnCalcPrivateCommandRecorderSize / pfnCreateCommandRecorder / pfnDestroyCommandRecorder
pfnCommandRecorderSetCommandPoolAsTarget
pfnCalcPrivateCommandSignatureSize / pfnCreateCommandSignature / pfnDestroyCommandSignature   (3, for ExecuteIndirect)
```

*(e) Pipeline state + libraries + root signatures — 10*
```
pfnCalcPrivatePipelineStateSize / pfnCreatePipelineState / pfnDestroyPipelineState
pfnCalcPrivateRootSignatureSize / pfnCreateRootSignature / pfnDestroyRootSignature
pfnCalcPrivatePipelineLibrarySize / pfnCreatePipelineLibrary / pfnDestroyPipelineLibrary
pfnAddPipelineStateToLibrary, pfnCalcSerializedLibrarySize, pfnSerializeLibrary
```

*(f) Descriptor heaps and views — 12*
```
pfnCalcPrivateDescriptorHeapSize / pfnCreateDescriptorHeap / pfnDestroyDescriptorHeap
pfnGetDescriptorSizeInBytes, pfnGetCPUDescriptorHandleForHeapStart, pfnGetGPUDescriptorHandleForHeapStart
pfnCreateShaderResourceView, pfnCreateConstantBufferView, pfnCreateSampler,
pfnCreateUnorderedAccessView, pfnCreateRenderTargetView, pfnCreateDepthStencilView
pfnCopyDescriptors, pfnCopyDescriptorsSimple
pfnCreateSamplerFeedbackUnorderedAccessView
```

*(g) Heaps, resources, residency — 11*
```
pfnMapHeap, pfnUnmapHeap
pfnCalcPrivateHeapAndResourceSizes / pfnCreateHeapAndResource / pfnDestroyHeapAndResource
pfnCalcPrivateOpenedHeapAndResourceSizes / pfnOpenHeapAndResource
pfnMakeResident, pfnEvict
pfnOfferResources, pfnReclaimResources
```

*(h) Resource introspection — 5*
```
pfnCheckResourceVirtualAddress    -> D3D12DDI_GPU_VIRTUAL_ADDRESS   (L2476)
pfnCheckResourceAllocationInfo
pfnCheckSubresourceInfo
pfnCheckExistingResourceAllocationInfo
pfnCheckResourceAllocationHandle  -> D3DKMT_HANDLE                  (L2992)
```

*(i) Fences — 3*  `pfnCalcPrivateFenceSize / pfnCreateFence / pfnDestroyFence`

*(j) Queries — 3*  `pfnCalcPrivateQueryHeapSize / pfnCreateQueryHeap / pfnDestroyQueryHeap`

*(k) Multi-adapter / misc — 5*
```
pfnGetImplicitPhysicalAdapterMask, pfnQueryNodeMap,
pfnGetPresentPrivateDriverDataSize, pfnRetrieveShaderComment, pfnGetDebugAllocationInfo
```

*(l) Scheduling groups (hardware scheduling) — 3*
`pfnCalcPrivateSchedulingGroupSize / pfnCreateSchedulingGroup / pfnDestroySchedulingGroup`

*(m) Meta-commands — 6*
`pfnEnumerateMetaCommands, pfnEnumerateMetaCommandParameters, pfnCalcPrivateMetaCommandSize,
pfnCreateMetaCommand, pfnDestroyMetaCommand, pfnGetMetaCommandRequiredParameterInfo`

*(n) State objects / raytracing / work graphs — 13*
```
pfnCalcPrivateStateObjectSize, pfnCreateStateObject, pfnDestroyStateObject,
pfnGetRaytracingAccelerationStructurePrebuildInfo, pfnCheckDriverMatchingIdentifier,
pfnGetShaderIdentifier, pfnGetShaderStackSize, pfnGetPipelineStackSize, pfnSetPipelineStackSize,
pfnCalcPrivateAddToStateObjectSize, pfnAddToStateObject,
pfnGetProgramIdentifier, pfnGetWorkGraphMemoryRequirements
```

*(o) Misc device policy — 2*  `pfnSetBackgroundProcessingMode, pfnImplicitShaderCacheControl`

Aggregate shape of `_0109`: **26 `pfnCalcPrivate*`/`pfnCalc*`, 35 `pfnCreate*`, 20 `pfnDestroy*`,
43 other.**

### 3.2 `D3D12DDI_COMMAND_LIST_FUNCS_3D` — 16 versions, 51 → 75 members

| Struct | Lines | Members | | Struct | Lines | Members |
|---|---|---|---|---|---|---|
| `_0003` | 2999–3057 | 51 | | `_0054` | 8360–8433 | 65 |
| `_0022` | 5029–5088 | 52 | | `_0062` | 8520–8595 | 67 |
| `_0025` | 5457–5517 | 53 | | `_0074` | 9647–9723 | 68 |
| `_0027` | 5758–5820 | 55 | | `_0088` | 10790–10868 | 69 |
| `_0028` | 5846–5908 | 55 | | `_0092` | 11164–11243 | 70 |
| `_0030` | 6056–6119 | 56 | | `_0094` | 11303–11382 | 70 |
| `_0032` | 6184–6248 | 57 | | `_0095` | 11586–11666 | 71 |
| `_0033` | 6303–6368 | 58 | | `_0099` | 12158–12240 | 73 |
| `_0040` | 6547–6612 | 58 | | `_0108` | 13303–13388 | **75** |
| `_0051` | 7253–7318 | 58 | | | | |
| `_0052` | 7548–7616 | 60 | | | | |

`D3D12DDI_COMMAND_LIST_FUNCS_3D_0108` verbatim member list, grouped:

*List lifetime — 2:* `pfnCloseCommandList`, `pfnResetCommandList`
*Draw / dispatch — 3:* `pfnDrawInstanced`, `pfnDrawIndexedInstanced`, `pfnDispatch`
*Clears / discard — 5:* `pfnClearUnorderedAccessViewUint`, `pfnClearUnorderedAccessViewFloat`,
`pfnClearRenderTargetView`, `pfnClearDepthStencilView`, `pfnDiscardResource`
*Copy / resolve — 6:* `pfnCopyTextureRegion`, `pfnResourceCopy`, `pfnCopyTiles`,
`pfnCopyBufferRegion`, `pfnResourceResolveSubresource`, `pfnAtomicCopyBufferRegion`
(+`pfnResourceResolveSubresourceRegion`)
*Indirect / bundles — 2:* `pfnExecuteBundle`, `pfnExecuteIndirect`
*Barriers — 2:* `pfnResourceBarrier` (legacy, `…_0022`), `pfnBarrier` (**enhanced barriers**, `…_0094`)
*Present / blt — 2:* `pfnBlt`, `pfnPresent` (`PFND3D12DDI_PRESENT_0051`)
*Queries / predication — 4:* `pfnBeginQuery`, `pfnEndQuery`, `pfnResolveQueryData`, `pfnSetPredication`
*Fixed-function state — 11:* `pfnIaSetTopology`, `pfnRsSetViewports`, `pfnRsSetScissorRects`,
`pfnOmSetBlendFactor`, `pfnOmSetStencilRef`, `pfnSetPipelineState`, `pfnOMSetDepthBounds`,
`pfnSetSamplePositions`, `pfnOmSetAlphaBlendFactor`, `pfnOmSetFrontAndBackStencilRef`,
`pfnRSSetDepthBias`
*Root arguments / descriptors — 16:* `pfnSetDescriptorHeaps`, `pfnSetComputeRootSignature`,
`pfnSetGraphicsRootSignature`, `pfnSetComputeRootDescriptorTable`, `pfnSetGraphicsRootDescriptorTable`,
`pfnSetComputeRoot32BitConstant`, `pfnSetGraphicsRoot32BitConstant`,
`pfnSetComputeRoot32BitConstants`, `pfnSetGraphicsRoot32BitConstants`,
`pfnSetComputeRootConstantBufferView`, `pfnSetGraphicsRootConstantBufferView`,
`pfnSetComputeRootShaderResourceView`, `pfnSetGraphicsRootShaderResourceView`,
`pfnSetComputeRootUnorderedAccessView`, `pfnSetGraphicsRootUnorderedAccessView`,
`pfnClearRootArguments`
*IA / SO / OM binding — 5:* `pfnIASetIndexBuffer`, `pfnIASetVertexBuffers`, `pfnSOSetTargets`,
`pfnOMSetRenderTargets`, `pfnIASetIndexBufferStripCutValue`
*Markers / protection / immediates / view instancing — 4:* `pfnSetMarker`,
`pfnSetProtectedResourceSession`, `pfnWriteBufferImmediate`, `pfnSetViewInstanceMask`
*Meta-commands — 2:* `pfnInitializeMetaCommand`, `pfnExecuteMetaCommand`
*Raytracing — 5:* `pfnBuildRaytracingAccelerationStructure`,
`pfnEmitRaytracingAccelerationStructurePostbuildInfo`, `pfnCopyRaytracingAccelerationStructure`,
`pfnSetPipelineState1`, `pfnDispatchRays`
*VRS — 2:* `pfnRSSetShadingRate`, `pfnRSSetShadingRateImage`
*Mesh shaders — 1:* `pfnDispatchMesh`
*Work graphs — 2:* `pfnSetProgram`, `pfnDispatchGraph`

⚠ **`pfnPresent` and `pfnBlt` live on the COMMAND LIST, not on a DXGI table.** That is a structural
difference from D3D11 and directly relevant to lane R7. Signature (L7250–7251):
```c
typedef VOID ( APIENTRY* PFND3D12DDI_PRESENT_0051 ) ( D3D12DDI_HCOMMANDLIST, D3D12DDI_HCOMMANDQUEUE,
    _In_ CONST D3D12DDIARG_PRESENT_0001*,
    _Out_ D3D12DDI_PRESENT_0051*, _Out_opt_ D3D12DDI_PRESENT_CONTEXTS_0051*, _Out_opt_ D3D12DDI_PRESENT_HWQUEUES_0051* );
```

### 3.3 `D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001` — one version, 7 members

Verbatim, L2729–2738 — **this table never got a second version in 30 DDI revisions**:

```c
typedef struct D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001
{
    PFND3D12DDI_EXECUTECOMMANDLISTS         pfnExecuteCommandLists;
    void*                                   pfnUnused;
    void*                                   pfnUnused2;
    PFND3D12DDI_UPDATETILEMAPPINGS          pfnUpdateTileMappings;
    PFND3D12DDI_COPYTILEMAPPINGS            pfnCopyTileMappings;
    PFND3D12DDI_SIGNAL_FENCE                pfnSignalFence;
    PFND3D12DDI_WAIT_FOR_FENCE              pfnWaitForFence;
} D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001;
```

Two of the seven slots are named `pfnUnused` / `pfnUnused2` and must be left alone (writing them is
harmless; the runtime never calls them — **I infer** that from the name, the header says nothing).

---

## 4. Runtime→driver callbacks

There are **three** callback surfaces a D3D12 UMD consumes, at three different scopes. Getting this
split right is the whole of lane R6's input.

### 4.1 Adapter scope — `D3DDDI_ADAPTERCALLBACKS` (3 entries)

From `d3dumddi.h` (measured on win11): `pfnQueryAdapterInfoCb`, `pfnGetMultisampleMethodListCb`,
`pfnQueryAdapterInfoCb2`. Identical to what the Helios D3D11 UMD already receives.

### 4.2 Device scope, usermode — `D3D12DDI_CORELAYER_DEVICECALLBACKS_*`

Four versions. The `_0062` form (L8606–8647, **28 members**) is the superset; verbatim:

```c
typedef struct D3D12DDI_CORELAYER_DEVICECALLBACKS_0062
{
    PFND3D12DDI_SETERROR_CB pfnSetErrorCb;                                  // VOID (HRTDEVICE, HRESULT)          L2602
    PFND3D12DDI_SETCOMMANDLISTERROR_CB pfnSetCommandListErrorCb;            // VOID (HRTCOMMANDLIST, HRESULT)     L2585
    PFND3D12DDI_SETCOMMANDLISTDDITABLE_CB pfnSetCommandListDDITableCb;      // VOID (HRTCOMMANDLIST, HRTTABLE)    L2554

    // KM callbacks for 12
    PFND3D12DDI_CREATECONTEXT_CB        pfnCreateContextCb;                 // (HRTCOMMANDQUEUE, D3DDDICB_CREATECONTEXT*)         L2556
    PFND3D12DDI_CREATECONTEXTVIRTUAL_CB pfnCreateContextVirtualCb;          // (HRTCOMMANDQUEUE, D3DDDICB_CREATECONTEXTVIRTUAL*)  L2562
    PFND3D12DDI_DESTROYCONTEXT_CB       pfnDestroyContextCb;                // (HRTCOMMANDQUEUE, D3DDDICB_DESTROYCONTEXT*)        L2568
    PFND3D12DDI_CREATEPAGINGQUEUE_CB    pfnCreatePagingQueueCb;             // (HRTCOMMANDQUEUE, D3DDDICB_CREATEPAGINGQUEUE*)     L2574
    PFND3D12DDI_DESTROYPAGINGQUEUE_CB   pfnDestroyPagingQueueCb;            // (HRTCOMMANDQUEUE, D3DDDI_DESTROYPAGINGQUEUE*)      L2579
    PFND3D12DDI_MAKERESIDENT_CB         pfnMakeResidentCb;                  // (HRTDEVICE, HRTPAGINGQUEUE, D3DDDI_MAKERESIDENT*)  L2531
    PFND3D12DDI_EVICT_CB                pfnEvictCb;                         // (HRTDEVICE, D3DDDICB_EVICT*)                       L2537
    PFND3D12DDI_RECLAIMALLOCATIONS2_CB  pfnReclaimAllocations2Cb;           // (HRTDEVICE, HRTPAGINGQUEUE, D3D12DDICB_RECLAIMALLOCATIONS2*) L2542
    PFND3D12DDI_OFFERALLOCATIONS_CB     pfnOfferAllocationsCb;              // (HRTDEVICE, D3D12DDICB_OFFERALLOCATIONS*)          L2548
    PFND3D12DDI_ALLOCATE_CB_0022        pfnAllocateCb;                      // (HRTDEVICE, D3D12DDICB_ALLOCATE_0022*)             L4868
    PFND3D12DDI_DEALLOCATE_CB_0022      pfnDeallocateCb;                    // (HRTDEVICE, D3D12DDICB_DEALLOCATE_0022*)           L4871
    PFND3D12DDI_CREATESCHEDULINGGROUPCONTEXT_CB_0050        pfnCreateSchedulingGroupContextCb;         L7162
    PFND3D12DDI_CREATESCHEDULINGGROUPCONTEXTVIRTUAL_CB_0050 pfnCreateSchedulingGroupContextVirtualCb;  L7167
    PFND3D12DDI_CREATEHWQUEUE_CB_0050                       pfnCreateHwQueueCb;                        L7172
    PFND3D12DDI_QUEUEPROCESSINGWORK_CB_0062     pfnQueueBackgroundProcessingWorkCb;                    L8598
} D3D12DDI_CORELAYER_DEVICECALLBACKS_0062;
```

(The 28-member count comes from the struct's *widest* preprocessor configuration; the six
`#if D3D_UMD_INTERFACE_VERSION >= WDDM2_0` and three `>= WDDM2_5` gates each have a `void*
pfnReserved…` `#else` arm at the SAME offsets, L8616-8645, so **the struct layout is
version-independent — only the pointer types change**. That is a deliberate ABI design and means a
bindgen'd Rust binding does not need per-WDDM variants, only the right `D3D_UMD_INTERFACE_VERSION`
define to get useful types.)

Versions and members: `_0003` = 19 (L2624), `_0022` = 21 (adds `pfnAllocateCb`/`pfnDeallocateCb`,
L4874), `_0050` = 27 (adds the three scheduling-group/HW-queue creators, L7178), `_0062` = 28
(adds `pfnQueueBackgroundProcessingWorkCb`, L8606).

Two allocate paths, both present:
```c
typedef struct D3D12DDICB_ALLOCATE_0022                      // L4841-4849
{
    CONST VOID* pPrivateDriverData;  UINT PrivateDriverDataSize;
    HANDLE hResource;                D3DKMT_HANDLE hKMResource;
    UINT NumAllocations;             D3D12DDI_ALLOCATION_INFO_0022* pAllocationInfo;
} D3D12DDICB_ALLOCATE_0022;

typedef struct D3D12DDI_ALLOCATION_INFO_0022                 // L4828-4839
{
    D3DKMT_HANDLE hAllocation;   CONST VOID* pSystemMem;
    VOID* pPrivateDriverData;    UINT PrivateDriverDataSize;
    D3DDDI_VIDEO_PRESENT_SOURCE_ID VidPnSourceId;
    D3D12DDI_ALLOCATION_INFO_FLAGS_0022 Flags;
    D3DGPU_VIRTUAL_ADDRESS GpuVirtualAddress;      // <-- the KMD-assigned GPU VA comes back here
    UINT Priority;               ULONG_PTR Reserved[5];
} D3D12DDI_ALLOCATION_INFO_0022;
```

### 4.3 Device scope, kernel — `D3DDDI_DEVICECALLBACKS` (`pKTCallbacks`)

This is the **full D3DKMT callback table**, the same one a D3D11 UMD gets, handed to the D3D12
driver through `D3D12DDIARG_CREATEDEVICE_*::pKTCallbacks`. Measured on win11 from
`C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0\um\d3dumddi.h` L4499ff — **65 entries**
with all version gates on:

```
pfnAllocateCb, pfnDeallocateCb, pfnSetPriorityCb, pfnQueryResidencyCb, pfnSetDisplayModeCb,
pfnPresentCb, pfnRenderCb, pfnLockCb, pfnUnlockCb, pfnEscapeCb,
pfnCreateOverlayCb, pfnUpdateOverlayCb, pfnFlipOverlayCb, pfnDestroyOverlayCb,
pfnCreateContextCb, pfnDestroyContextCb,
pfnCreateSynchronizationObjectCb, pfnDestroySynchronizationObjectCb,
pfnWaitForSynchronizationObjectCb, pfnSignalSynchronizationObjectCb,
pfnSetAsyncCallbacksCb, pfnSetDisplayPrivateDriverFormatCb,
[WIN8]      pfnOfferAllocationsCb, pfnReclaimAllocationsCb, pfnCreateSynchronizationObject2Cb,
            pfnWaitForSynchronizationObject2Cb, pfnSignalSynchronizationObject2Cb,
            pfnPresentMultiPlaneOverlayCb
[WDDM1_3]   pfnLogUMDMarkerCb
[WDDM2_0]   pfnMakeResidentCb, pfnEvictCb,
            pfnWaitForSynchronizationObjectFromCpuCb, pfnSignalSynchronizationObjectFromCpuCb,
            pfnWaitForSynchronizationObjectFromGpuCb, pfnSignalSynchronizationObjectFromGpuCb,
            pfnCreatePagingQueueCb, pfnDestroyPagingQueueCb, pfnLock2Cb, pfnUnlock2Cb,
            pfnInvalidateCacheCb,
            pfnReserveGpuVirtualAddressCb, pfnMapGpuVirtualAddressCb,
            pfnFreeGpuVirtualAddressCb, pfnUpdateGpuVirtualAddressCb,
            pfnCreateContextVirtualCb, pfnSubmitCommandCb, pfnDeallocate2Cb,
            pfnSignalSynchronizationObjectFromGpu2Cb, pfnReclaimAllocations2Cb,
            pfnGetResourcePresentPrivateDriverDataCb
[WDDM2_1_1] pfnUpdateAllocationPropertyCb, pfnOfferAllocations2Cb
[WDDM2_1_2] pfnReclaimAllocations3Cb, pfnAcquireResourceCb
[WDDM2_1_3] pfnReleaseResourceCb
[WDDM2_2_1] pfnCreateHwContextCb, pfnDestroyHwContextCb, pfnCreateHwQueueCb, pfnDestroyHwQueueCb,
            pfnSubmitCommandToHwQueueCb, pfnSubmitWaitForSyncObjectsToHwQueueCb,
            pfnSubmitSignalSyncObjectsToHwQueueCb
[WDDM2_4_2] pfnSubmitPresentBltToHwQueueCb
[WDDM2_5_2] pfnSubmitPresentToHwQueueCb
[WDDM2_6_4] pfnSubmitHistorySequenceCb
```

### 4.4 The split that matters for lane R6

**What the D3D12 corelayer callbacks give you that the raw KT table does not:** the corelayer
`pfnCreateContextCb`/`pfnCreateContextVirtualCb`/`pfnCreatePagingQueueCb`/`pfnCreateHwQueueCb` take
a **`D3D12DDI_HRTCOMMANDQUEUE`**, not a device handle — i.e. the runtime scopes context creation to
the *command queue object* it owns. The KT table's `pfnCreateContextCb` takes the device. A D3D12
UMD is therefore expected to create its KMD contexts through the **corelayer**, so the runtime can
associate the context with the queue for scheduling and for `pfnPresent`'s `hContext` output
(`D3D12DDI_PRESENT_CONTEXTS_0051 { HANDLE hContext; UINT BroadcastContextCount;
HANDLE BroadcastContext[…]; }`, L7237–7242).

**Everything else goes through `pKTCallbacks` directly** — notably:
- **GPU VA management**: `pfnReserveGpuVirtualAddressCb`, `pfnMapGpuVirtualAddressCb`,
  `pfnFreeGpuVirtualAddressCb`, `pfnUpdateGpuVirtualAddressCb`. There is **no** corelayer wrapper
  for these. A D3D12 UMD manages a real GPU VA space via D3DKMT.
- **Submission**: `pfnSubmitCommandCb` (software-scheduled) or the HwQueue family.
- **Monitored fences**: `pfnCreateSynchronizationObject2Cb` +
  `pfnSignalSynchronizationObjectFromGpuCb` / `…FromCpuCb` / `…FromGpu2Cb`.
- **Escape**: `pfnEscapeCb` — the same door Helios' D3D11 UMD already uses
  (memory `d4a-v1-inert-blindspot-63rd.md`: "pfnEscapeCb works (RT adapter handle)").

**This is the load-bearing finding for Helios.** D3D12's fence object is *defined* in terms of a GPU
virtual address (§8), and the reserve/map GPU-VA callbacks are the only way to get one. Helios'
KMD declares `Wddm2_1GpuMmu` with **decorative** page tables (`kmd_render/src/ddi/gpummu.rs:1-14`,
quoted in `DX12.md` §3.4). Whether VidMm's `ReserveGpuVirtualAddress`/`MapGpuVirtualAddress` path
even functions against this adapter is **UNVERIFIED** — and it gates strategy (a) entirely.
Settling experiment (cheap, D3D11-only, no D3D12 code): write `tools/gpuva_probe.cpp` that opens a
D3DKMT device on the Helios adapter and calls `D3DKMTReserveGpuVirtualAddress` +
`D3DKMTMapGpuVirtualAddress` on a real allocation, then reads back the address and checks that
`D3DKMTUpdateGpuVirtualAddress` succeeds. If that fails or returns a nonsense address, strategy (a)
is dead before any DDI code is written.

---

## 5. Caps

### 5.1 `D3D12DDICAPS_TYPE` — every value

Verbatim from L94–150 (the header's own inline comments preserved):

```c
typedef enum D3D12DDICAPS_TYPE
{
    D3D12DDICAPS_TYPE_TEXTURE_LAYOUT                             = 1000, // Deprecated by …_0022_TEXTURE_LAYOUT
    D3D12DDICAPS_TYPE_SWIZZLE_PATTERN                            = 1001, // Deprecated by …_0022_SWIZZLE_PATTERN
    D3D12DDICAPS_TYPE_MEMORY_ARCHITECTURE                        = 1002,
    D3D12DDICAPS_TYPE_TEXTURE_LAYOUT_SETS                        = 1003,
    D3D12DDICAPS_TYPE_SHADER                                     = 1004,
    D3D12DDICAPS_TYPE_ARCHITECTURE_INFO                          = 1005,
    D3D12DDICAPS_TYPE_D3D12_OPTIONS                              = 1006,
    D3D12DDICAPS_TYPE_3DPIPELINESUPPORT                          = 1007,
    D3D12DDICAPS_TYPE_GPUVA_CAPS                                 = 1009,
    D3D12DDICAPS_TYPE_TEXTURE_LAYOUT1                            = 1010, // Deprecated
    D3D12DDICAPS_TYPE_0011_SHADER_MODELS                         = 1012,
    D3D12DDICAPS_TYPE_OPTIONS1_0103                              = 1013, // D3D12DDI_OPTIONS1_DATA_0103
    D3D12DDICAPS_TYPE_0030_PROTECTED_RESOURCE_SESSION_SUPPORT    = 1057,
    D3D12DDICAPS_TYPE_0030_CRYPTO_SESSION_SUPPORT                = 1058, // Deprecated, moved to VIDEO
    D3D12DDICAPS_TYPE_0022_CPU_PAGE_TABLE_FALSE_POSITIVES        = 1059,
    D3D12DDICAPS_TYPE_0022_TEXTURE_LAYOUT                        = 1060,
    D3D12DDICAPS_TYPE_0022_SWIZZLE_PATTERN                       = 1061,
    D3D12DDICAPS_TYPE_0023_UMD_BASED_COMMAND_QUEUE_PRIORITY      = 1062,
    D3D12DDICAPS_TYPE_0030_CONTENT_PROTECTION_SYSTEM_COUNT       = 1063, // Deprecated, moved to VIDEO
    D3D12DDICAPS_TYPE_0030_CONTENT_PROTECTION_SYSTEM_SUPPORT     = 1064, // Deprecated, moved to VIDEO
    D3D12DDICAPS_TYPE_0030_CRYPTO_SESSION_TRANSFORM_SUPPORT      = 1065, // Deprecated, moved to VIDEO
    D3D12DDICAPS_TYPE_0033_ADAPTER_COMPUTE_ONLY                  = 1066,
    D3D12DDICAPS_TYPE_0050_HARDWARE_SCHEDULING_CAPS              = 1067,
    D3D12DDICAPS_TYPE_QUERY_META_COMMAND_CAPS_0061               = 1068,
    D3D12DDICAPS_TYPE_EXECUTECOMMANDLISTS_PARALLELISM            = 1069, // pData = BOOL
    D3D12DDICAPS_TYPE_SAMPLER_FEEDBACK_0073                      = 1070,
    D3D12DDICAPS_TYPE_0073_SUPPORT_BATCHED_MARKERS               = 1071, // pData = BOOL
    D3D12DDICAPS_TYPE_0074_PROTECTED_RESOURCE_SESSION_TYPE_COUNT = 1072,
    D3D12DDICAPS_TYPE_0074_PROTECTED_RESOURCE_SESSION_TYPES      = 1073,
    D3D12DDICAPS_TYPE_0081_3DPIPELINESUPPORT1                    = 1074, // pData = D3D12DDI_3DPIPELINESUPPORT1_DATA_0081
    D3D12DDICAPS_TYPE_0103_WAVE_MMA                              = 1075, // pData = D3D12DDI_WAVE_MMA_DATA_0103
    D3D12DDICAPS_TYPE_OPTIONS_0090                               = 1077, // D3D12DDI_OPTIONS_DATA_0090
    D3D12DDICAPS_TYPE_OPTIONS_0091                               = 1078,
    D3D12DDICAPS_TYPE_OPTIONS_0093                               = 1079,
    D3D12DDICAPS_TYPE_OPTIONS_0098                               = 1080,
    D3D12DDICAPS_TYPE_OPTIONS_0101                               = 1081,
    D3D12DDICAPS_TYPE_OPTIONS_0102                               = 1082,
    // D3D12DDICAPS_TYPE_OPTIONS_0092                            = 1083, // …cannot be used.
    D3D12DDI_FEATURE_D3D12_PREDICATION_106                       = 1084,
    D3D12DDI_FEATURE_PLACED_RESOURCE_SUPPORT_INFO_106            = 1085,
    D3D12DDI_FEATURE_HARDWARE_COPY_106                           = 1086,
    D3D12DDICAPS_TYPE_OPTIONS_0109                               = 1087, // D3D12DDI_OPTIONS_DATA_0109
    D3D12DDICAPS_TYPE_OPTIONS_0110                               = 1088, // D3D12DDI_OPTIONS_DATA_0110
    D3D12DDICAPS_TYPE_SHADER_MODEL_6_8_OPTIONS_0110              = 1091, // D3D12DDI_SHADER_MODEL_6_8_OPTIONS_0110
} D3D12DDICAPS_TYPE;
```

**42 live values** (plus one commented out). Values 1008, 1011, 1014–1056 and 1076, 1083, 1089–1090
are absent. Note there is a **separate** `D3D12DDICAPS_TYPE_VIDEO_0020` enum at L4327 for the video
extended feature — a baseline device never sees it.

Since rev 0090 the convention changed; the header states it verbatim (L11121–11124):
> "New options DDIs use a new NNNN version number and add new caps without inheriting the caps from
> the previous version. This is done to avoid bloating one caps struct indefinitely, like what
> happened with `D3D12DDICAPS_TYPE_D3D12_OPTIONS`. … The runtime will keep requesting from the
> driver all `D3D12DDI_OPTION` versions whose caps it cares about."

So `pfnGetCaps` will be called **many** times with many `Type` values, and the driver answers only
those it knows. **UNVERIFIED: what the runtime does when the driver returns a failing HRESULT (or
`E_INVALIDARG`) for a caps type it does not recognise.** The pattern strongly suggests "treat as
zeroed/unsupported" but the header does not say. Settling experiment: §9.4 proxy — return failure
for one benign cap (e.g. `…_OPTIONS_0110`) and observe whether the device still creates.

### 5.2 The tiered caps

`D3D12DDICAPS_TYPE_D3D12_OPTIONS` is the fat legacy struct; the newest is
`D3D12DDI_D3D12_OPTIONS_DATA_0089` (L11079–11112, **31 fields**), verbatim in §0's source. Its
tiered members and the exact enums (all verbatim):

| Field | Enum | Values | Line |
|---|---|---|---|
| `ResourceBindingTier` | `D3D12DDI_RESOURCE_BINDING_TIER` | `_1=1,_2=2,_3=3` | L694 |
| `ConservativeRasterizationTier` | `D3D12DDI_CONSERVATIVE_RASTERIZATION_TIER` | `NOT_SUPPORTED=0,_1..._3` | L701 |
| `TiledResourcesTier` | `D3D12DDI_TILED_RESOURCES_TIER` | `NOT_SUPPORTED=0,_1..._3` | L709 |
| `CrossNodeSharingTier` | `D3D12DDI_CROSS_NODE_SHARING_TIER` | `NOT_SUPPORTED=0, _1_EMULATED=1, _1=2, _2=3, _0041_3=4` | L725 |
| `ResourceHeapTier` | `D3D12DDI_RESOURCE_HEAP_TIER` | `_1=1,_2=2` | L734 |
| `ProgrammableSamplePositionsTier` | `D3D12DDI_PROGRAMMABLE_SAMPLE_POSITIONS_TIER` | `NOT_SUPPORTED=0,_1,_2` | L5700 |
| `ViewInstancingTier` | `D3D12DDI_VIEW_INSTANCING_TIER` | `NOT_SUPPORTED,_1,_2,_3` (0..3) | L6370 |
| `RenderPassTier` | `D3D12DDI_RENDER_PASS_TIER` | `NOT_SUPPORTED=0,_1=1,_2=2` | L7645 |
| `RaytracingTier` | `D3D12DDI_RAYTRACING_TIER` | `NOT_SUPPORTED=0, _1_0=10, _1_1=11` | L7683 |
| `VariableShadingRateTier` | `D3D12DDI_VARIABLE_SHADING_RATE_TIER` | `NOT_SUPPORTED=0,_1=1,_2=2` | L8456 |
| `MeshShaderTier` | `D3D12DDI_MESH_SHADER_TIER` | `NOT_SUPPORTED=0, _1=10` | L9353 |
| `SamplerFeedbackTier` | `D3D12DDI_SAMPLER_FEEDBACK_TIER` | `NOT_SUPPORTED=0, _0_9=90, _1_0=100` | L9359 |
| `WriteBufferImmediateQueueFlags` | `D3D12DDI_COMMAND_QUEUE_FLAGS` | bitmask, see §7 | L1435 |
| `ExecuteIndirectTier` (in `…OPTIONS_DATA_0110`) | `D3D12DDI_EXECUTE_INDIRECT_TIER` | `_1_0=10, _1_1=11` | L13659 |

Plus 17 plain `BOOL`s in `…OPTIONS_DATA_0089`, of which the one a Helios implementer must think
hardest about is **`EnhancedBarriersSupported`** (L11111) — it selects between `pfnResourceBarrier`
(legacy) and `pfnBarrier` (`PFND3D12DDI_BARRIER_0094`), both of which are present in
`COMMAND_LIST_FUNCS_3D_0108`.

Root-signature version (L3743–3747) — note **1_0 is gone from this header**:
```c
typedef enum D3D12DDI_ROOT_SIGNATURE_VERSION { D3D12DDI_ROOT_SIGNATURE_VERSION_1_1 = 0x2,
                                               D3D12DDI_ROOT_SIGNATURE_VERSION_1_2 = 0x3, } …;
```
`D3D12DDIARG_CREATE_ROOT_SIGNATURE_0100` (L12279–12287) carries `Version` plus a union whose only
arm in this revision is `CONST D3D12DDI_ROOT_SIGNATURE_0100* pRootSignature_1_2` — i.e. **at DDI
0100 the driver is handed 1.2-shaped root signatures only**; the runtime up-converts 1.0/1.1.

### 5.3 Caps a baseline device plausibly must answer

⚠ **The header nowhere states which caps are mandatory.** What follows is my ordered
*hypothesis*, derived from what the caps structurally *are*, and is explicitly **UNVERIFIED**.

| Cap | Output struct (lines) | Why baseline |
|---|---|---|
| `_3DPIPELINESUPPORT` (1007) | `D3D12DDI_3DPIPELINELEVEL` (L2924–2933): `1_0_GENERIC=1, 1_0_CORE=2, 11_0=10, 11_1=11, 12_0=12, 12_1=13, 12_2=14` | Selects the feature level `D3D12CreateDevice` will grant. Header says "For D3D12, drivers only report the maximum level they support" (L2923) |
| `_0081_3DPIPELINESUPPORT1` (1074) | `D3D12DDI_3DPIPELINESUPPORT1_DATA_0081` (L10416–10420): `{ IN HighestRuntimeSupportedFeatureLevel; OUT MaximumDriverSupportedFeatureLevel; }` | The header spells out the trap (L10361–10372): drivers **must not** return >12_1 from the old cap, because an old OS sanitises anything it does not understand down to `1_0_CORE` |
| `_D3D12_OPTIONS` (1006) | `D3D12DDI_D3D12_OPTIONS_DATA_0089` (31 fields) or the older/newer sibling matching the negotiated version | The tier contract |
| `_SHADER` (1004) | `D3D12DDI_SHADER_CAPS_0042` (L6843–6856, 11 fields) or `_0003` (L2907, 5) / `_0012` / `_0015` | Wave ops, min precision, lane counts |
| `_0011_SHADER_MODELS` (1012) | `D3D12DDI_D3D12_SHADER_MODELS_DATA_0011` (L3503–3507) — a count + `D3D12DDI_SHADER_MODEL*` array | See §5.4; lane R8's input |
| `_MEMORY_ARCHITECTURE` (1002) | `D3D12DDI_MEMORY_ARCHITECTURE_CAPS_0041` (L6807–6814): `{ BOOL UMA; BOOL IOCoherent; BOOL CacheCoherent; HeapSerializationTier; ResourceSerializationTier; }`; `pInfo = NodeIndex` (L152-155) | Heap-type behaviour |
| `_ARCHITECTURE_INFO` (1005) | `D3D12DDI_ARCHITECTURE_INFO_DATA` (L2917–2920): `{ BOOL TileBasedDeferredRenderer; }` | |
| `_GPUVA_CAPS` (1009) | `D3D12DDI_GPUVA_CAPS_0004` (L254–257): `{ UINT MaxGPUVirtualAddressBitsPerResource; }`; `pInfo = NodeIndex` | **Directly contradicted by Helios' decorative-VA KMD** — see §4.4 |
| `_0022_TEXTURE_LAYOUT` (1060) | `D3D12DDI_TEXTURE_LAYOUT_CAPS_0026` (L5529–5536, 5 fields incl. `SupportsRowMajorTexture`, `IndexableSwizzlePatterns`) | Placed-resource layout |
| `_0050_HARDWARE_SCHEDULING_CAPS` (1067) | `D3D12DDICAPS_HARDWARE_SCHEDULING_CAPS_0050` (L7005–7008): `{ UINT ComputeQueuesPer3DQueue; // 0 means don't use scheduling groups. }` | **Helios answers 0** — matches `DxgkDdiCreateHwQueue → STATUS_NOT_SUPPORTED` (`kmd_render/src/ddi/scheduler.rs:180-187`) |
| `_0033_ADAPTER_COMPUTE_ONLY` (1066) | BOOL-shaped | Must be FALSE for a render+display adapter |
| `_EXECUTECOMMANDLISTS_PARALLELISM` (1069) | `pData = BOOL` (header comment) | |

Settling experiment for the whole of §5.3: the §9.4 proxy — log the exact `(Type, DataSize)` sequence
the runtime asks WARP for during `D3D12CreateDevice(FEATURE_LEVEL_11_0)`, in call order. That single
log turns this table from a hypothesis into a contract.

### 5.4 Shader models (lane R8's hand-off)

`D3D12DDI_SHADER_MODEL` verbatim, L3478–3500 — note the **EXPERIMENTAL/RELEASE pairing**, where
release values are `+5`:

```
5_1_RELEASE_0011      = 0x00050015
6_0_EXPERIMENTAL_0011 = 0x00060000   6_0_RELEASE_0011 = 0x00060005
6_1_EXPERIMENTAL_0033 = 0x00060010   6_1_RELEASE_0033 = 0x00060015
6_2_EXPERIMENTAL_0042 = 0x00060020   6_2_RELEASE_0042 = 0x00060025
6_3_EXPERIMENTAL_0054 = 0x00060030   6_3_RELEASE_0054 = 0x00060035
6_4_EXPERIMENTAL_0054 = 0x00060040   6_4_RELEASE_0062 = 0x00060045
6_5_EXPERIMENTAL_0062 = 0x00060050   6_5_RELEASE_0071 = 0x00060055
6_6_EXPERIMENTAL_0071 = 0x00060060   6_6_RELEASE_0082 = 0x00060065
6_7_EXPERIMENTAL_0082 = 0x00060070   6_7_RELEASE_0093 = 0x00060075
6_8_EXPERIMENTAL_0093 = 0x00060080   6_8_RELEASE_0108 = 0x00060085
6_9_EXPERIMENTAL_0108 = 0x00060090
```

---

## 6. Object model and sizing

### 6.1 The pattern

D3D12 uses the **same runtime-allocates-driver-memory pattern as D3D11**, and the Helios D3D11 UMD
already implements it:

1. Runtime calls `pfnCalcPrivate<X>Size(hDevice, pArgs) -> SIZE_T`.
2. Runtime allocates that many bytes and hands the driver an opaque handle
   (`D3D10DDI_H(...)`-style: a struct with one `void* pDrvPrivate`) pointing at it.
3. Runtime calls `pfnCreate<X>(hDevice, pArgs, hDrvX [, hRTX])`.
4. Runtime calls `pfnDestroy<X>(hDevice, hDrvX)` and frees the memory itself.

Driver handle types (L74–89, 17 of them):
```
D3D12DDI_HCOMMANDQUEUE, HCOMMANDALLOCATOR, HPIPELINESTATE, HCOMMANDLIST, HFENCE, HDESCRIPTORHEAP,
HQUERYHEAP, HCOMMANDSIGNATURE, HHEAP, HUNORDEREDACCESSVIEWCOUNTER, HROOTSIGNATURE,
HCOMMANDRECORDER_0040, HCOMMANDPOOL_0040, HSCHEDULINGGROUP_0050, HMETACOMMAND_0052, HSTATEOBJECT_0054
(+ HPROTECTEDRESOURCESESSION_0030 at L5922)
```
Runtime handle types (L65–72): `D3D12DDI_HRTCOMMANDLIST, HRTTABLE, HRTCOMMANDQUEUE, HRTPAGINGQUEUE,
HRTPIPELINESTATE, HRTSCHEDULINGGROUP_0050, HRTMETACOMMAND_0052, HRTSTATEOBJECT_0054`
(+ `HRTPROTECTEDSESSION_0030` at L13688). `D3D12DDI_HDEVICE`, `HRESOURCE`, `HADAPTER`,
`HRTDEVICE`, `HRTADAPTER`, `HRTRESOURCE`, `HKMRESOURCE`, `HSHADER`, `HBLENDSTATE`,
`HRASTERIZERSTATE`, `HDEPTHSTENCILSTATE`, `HELEMENTLAYOUT` are **typedefs of the D3D10 handles**
(L23–34) — so a Helios D3D12 UMD reuses the exact handle plumbing `umd/src/forward/handles.rs`
already has.

### 6.2 Every `CalcPrivate*`/`Create*` pair in `CORE_0109`

26 sizing functions:
```
pfnCalcPrivateDeviceSize (adapter table)      pfnCalcPrivateElementLayoutSize
pfnCalcPrivateBlendStateSize                  pfnCalcPrivateDepthStencilStateSize
pfnCalcPrivateRasterizerStateSize             pfnCalcPrivateShaderSize
pfnCalcPrivateGeometryShaderWithStreamOutput  pfnCalcPrivateTessellationShaderSize
pfnCalcPrivateMeshShaderSize                  pfnCalcPrivateCommandQueueSize
pfnCalcPrivateCommandPoolSize                 pfnCalcPrivatePipelineStateSize
pfnCalcPrivateCommandListSize                 pfnCalcPrivateFenceSize
pfnCalcPrivateDescriptorHeapSize              pfnCalcPrivateRootSignatureSize
pfnCalcPrivateHeapAndResourceSizes            pfnCalcPrivateOpenedHeapAndResourceSizes
pfnCalcPrivateQueryHeapSize                   pfnCalcPrivateCommandSignatureSize
pfnCalcPrivatePipelineLibrarySize             pfnCalcSerializedLibrarySize
pfnCalcPrivateCommandRecorderSize             pfnCalcPrivateSchedulingGroupSize
pfnCalcPrivateMetaCommandSize                 pfnCalcPrivateStateObjectSize
pfnCalcPrivateAddToStateObjectSize
```

### 6.3 Four differences from the D3D11 pattern the Helios UMD implements

**(1) Two objects, one sizing call.** `pfnCalcPrivateHeapAndResourceSizes` returns a *struct of two
sizes*, not a `SIZE_T` (L556–560, L13443–13445):
```c
typedef struct D3D12DDI_HEAP_AND_RESOURCE_SIZES { SIZE_T Heap; SIZE_T Resource; } D3D12DDI_HEAP_AND_RESOURCE_SIZES;
typedef D3D12DDI_HEAP_AND_RESOURCE_SIZES ( APIENTRY* PFND3D12DDI_CALCPRIVATEHEAPANDRESOURCESIZES_0109)(
     D3D12DDI_HDEVICE, _In_opt_ CONST D3D12DDIARG_CREATEHEAP_0001*, _In_opt_ CONST D3D12DDIARG_CREATERESOURCE_0109*,
     D3D12DDI_HPROTECTEDRESOURCESESSION_0030 );
typedef HRESULT ( APIENTRY* PFND3D12DDI_CREATEHEAPANDRESOURCE_0109)(
    D3D12DDI_HDEVICE, _In_opt_ CONST D3D12DDIARG_CREATEHEAP_0001*, D3D12DDI_HHEAP, D3D12DDI_HRTRESOURCE,
    _In_opt_ CONST D3D12DDIARG_CREATERESOURCE_0109*, _In_opt_ CONST D3D12DDI_CLEAR_VALUES*,
    D3D12DDI_HPROTECTEDRESOURCESESSION_0030, D3D12DDI_HRESOURCE );
```
Both `_In_opt_` args are how D3D12's three resource shapes (committed = both, placed = resource
only, reserved = resource only with no heap) collapse into one entry point. **The NULL combinations
are the arm structure**, and CLAUDE.md's "validate every runtime-supplied length per-arm, not
max-union" applies directly.

**(2) Mixed return conventions.** Some creates return `VOID` and report via `pfnSetErrorCb`
(`pfnCreateElementLayout`, `pfnCreateBlendState`, …); others return `HRESULT` directly
(`pfnCreateFence`, `pfnCreateCommandList`, `pfnCreateCommandQueue`, `pfnCreateRootSignature`,
`pfnCreateHeapAndResource`, `pfnCreatePipelineState`). The D3D11 DDI is uniformly VOID + SetErrorCb.
Getting this wrong on a `VOID`-returning slot means the caller reads a garbage register as an
HRESULT — the exact class of bug memory `t7-umd-crash-fixed-52nd.md` records
(`bridge_guard` deducing `R=int` from a bare `0` and truncating `size_t` returns).

**(3) Some creates take BOTH handles.** `pfnCreateCommandList` takes
`(hDevice, pArgs, D3D12DDI_HCOMMANDLIST hDrv, D3D12DDI_HRTCOMMANDLIST hRT)`;
`pfnCreateCommandQueue` likewise takes `hDrv` + `D3D12DDI_HRTCOMMANDQUEUE`. D3D11's equivalent
passes the RT handle only for a few object classes.

**(4) `pfnDestroyDevice` lives on the ADAPTER table, not the device table.** L2622 / L13649.

`D3D12DDIARG_CREATEHEAP_0001` (L319–328) for reference:
```c
typedef struct D3D12DDIARG_CREATEHEAP_0001 { UINT64 ByteSize; UINT64 Alignment;
    D3D12DDI_MEMORY_POOL MemoryPool; D3D12DDI_CPU_PAGE_PROPERTY CPUPageProperty;
    D3D12DDI_HEAP_FLAGS Flags; UINT CreationNodeMask; UINT VisibleNodeMask; } …;
```
with `D3D12DDI_MEMORY_POOL { L0 = 0 /*Always system memory*/, L1 = 1 /*Typically local video memory*/ }`
(L301–305) and `D3D12DDI_HEAP_FLAGS` (L307–316) `NONE=0x0, NON_RT_DS_TEXTURES=0x2, BUFFERS=0x4,
COHERENT_SYSTEMWIDE=0x8, PRIMARY=0x10, RT_DS_TEXTURES=0x20, _0041_DENY_L0_DEMOTION=0x40`.

---

## 7. Command recording and submission

### 7.1 Four objects, not two

D3D12's public API has *command allocator* + *command list*. The DDI has **four**:

| DDI object | Handle | Created by | Public-API analogue |
|---|---|---|---|
| Command **pool** | `D3D12DDI_HCOMMANDPOOL_0040` | `pfnCreateCommandPool` | the backing store of an allocator |
| Command **recorder** | `D3D12DDI_HCOMMANDRECORDER_0040` | `pfnCreateCommandRecorder` | the recording engine |
| Command **list** | `D3D12DDI_HCOMMANDLIST` | `pfnCreateCommandList` | `ID3D12GraphicsCommandList` |
| Command **allocator** | `D3D12DDI_HCOMMANDALLOCATOR` (L75) | *nothing in `CORE_0109`* — `pfnCalcPrivateCommandAllocatorSize`/`pfnCreateCommandAllocator`/`pfnDestroyCommandAllocator`/`pfnResetCommandAllocator` exist only up to `CORE_0033` (L3101–3104) | superseded by pool+recorder at rev 0040 |

So **at DDI ≥ 0040 the allocator is gone from the DDI and replaced by pool + recorder.** A revival
must not port the 0003-era allocator functions.

Verbatim signatures and args (L6538–6658):
```c
typedef struct D3D12DDIARG_CREATE_COMMAND_POOL_0040 { D3D12DDI_COMMAND_POOL_FLAGS PoolFlags; } …;   // FLAG_NONE only
typedef SIZE_T  (…* PFND3D12DDI_CALC_PRIVATE_COMMAND_POOL_SIZE_0040)(HDEVICE, CONST D3D12DDIARG_CREATE_COMMAND_POOL_0040*);
typedef HRESULT (…* PFND3D12DDI_CREATE_COMMAND_POOL_0040)(HDEVICE, CONST …*, D3D12DDI_HCOMMANDPOOL_0040);
typedef VOID    (…* PFND3D12DDI_RESET_COMMAND_POOL_0040)(HDEVICE, D3D12DDI_HCOMMANDPOOL_0040);

typedef struct D3D12DDIARG_CREATE_COMMAND_RECORDER_0040 {
    D3D12DDI_COMMAND_QUEUE_FLAGS QueueFlags; D3D12DDI_COMMAND_RECORDER_FLAGS RecorderFlags; } …;
typedef VOID (…* PFND3D12DDI_COMMAND_RECORDER_SET_COMMAND_POOL_AS_TARGET_0040)(
    HDEVICE, D3D12DDI_HCOMMANDRECORDER_0040, D3D12DDI_HCOMMANDPOOL_0040);

typedef struct D3D12DDIARG_CREATE_COMMAND_LIST_0040 {
    D3D12DDI_COMMAND_LIST_TYPE   Type;          // DIRECT = 0, BUNDLE = 1        (L1425-1429)
    D3D12DDI_COMMAND_QUEUE_FLAGS QueueFlags;    // 3D / COMPUTE / COPY / …       (L1435-1447)
    UINT64                       ID;
    D3D12DDI_COMMAND_LIST_FLAGS  CommandListFlags;
    UINT                         NodeMask;
} D3D12DDIARG_CREATE_COMMAND_LIST_0040;
typedef HRESULT (…* PFND3D12DDI_CREATE_COMMAND_LIST_0040)(HDEVICE, CONST …*, D3D12DDI_HCOMMANDLIST, D3D12DDI_HRTCOMMANDLIST);

typedef struct D3D12DDIARG_RESETCOMMANDLIST_0040 {
    D3D12DDI_HCOMMANDRECORDER_0040 hDrvCommandRecorder; UINT64 ID; D3D12DDI_COMMAND_LIST_FLAGS CommandListFlags; } …;
typedef VOID (…* PFND3D12DDI_RESETCOMMANDLIST_0040)(D3D12DDI_HCOMMANDLIST, CONST D3D12DDIARG_RESETCOMMANDLIST_0040*);
typedef VOID (…* PFND3D12DDI_CLOSECOMMANDLIST)(D3D12DDI_HCOMMANDLIST);                              // L1750
```

**The list type is only DIRECT or BUNDLE.** COMPUTE and COPY are expressed through
`D3D12DDI_COMMAND_QUEUE_FLAGS` (L1435–1447): `NONE=0, 3D=0x1, COMPUTE=0x2, COPY=0x4, PAGING=0x8,
_0022_VIDEO_DECODE=0x10, _0022_VIDEO_PROCESS=0x20, _0053_VIDEO_ENCODE=0x40`.

### 7.2 Submission

```c
typedef VOID ( APIENTRY* PFND3D12DDI_EXECUTECOMMANDLISTS ) (       // L1735-1739
    D3D12DDI_HCOMMANDQUEUE, UINT Count, _In_reads_(Count) CONST D3D12DDI_HCOMMANDLIST* pCommandLists );
```
This is the **only** submission entry point in the baseline set, and it lives on the *queue* table.

`D3D12DDIARG_CREATECOMMANDQUEUE_0050` (L7019–7025):
```c
{ D3D12DDI_COMMAND_QUEUE_FLAGS QueueFlags; UINT NodeMask;
  D3D12DDI_COMMAND_QUEUE_CREATION_FLAGS QueueCreationFlags;
  D3D12DDI_HSCHEDULINGGROUP_0050 SchedulingGroup; /* May be NULL */ }
```

### 7.3 ⚠ There is NO DMA buffer and NO `pfnRenderCb` equivalent in the D3D12 DDI

Verified by absence: `d3d12umddi.h` contains no `pCommandBuffer`, no `AllocationList`, no
`PatchLocationList` field on any create-device or create-context argument. Compare
`D3DDDIARG_CREATEDEVICE` (measured on win11, `d3dumddi.h`) which carries
`pCommandBuffer / CommandBufferSize / pAllocationList / AllocationListSize / pPatchLocationList /
PatchLocationListSize / CommandBuffer (GPU VA)`.

**Where does the D3D12 UMD get memory to record into?** It allocates it itself, via
`pfnAllocateCb` (corelayer, `D3D12DDICB_ALLOCATE_0022`) or the KT `pfnAllocateCb`, and submits via
`pKTCallbacks->pfnSubmitCommandCb` — the WDDM2.0 GPU-VA submission path, where the DMA buffer is
just a GPU-VA range the UMD owns. **I infer this** from (i) the absence of any command-buffer field
in the D3D12 DDI, (ii) the presence of `pfnReserveGpuVirtualAddressCb`/`pfnMapGpuVirtualAddressCb`/
`pfnSubmitCommandCb` in `pKTCallbacks`, and (iii) `pfnCreateContextVirtualCb` being the corelayer's
context creator. **UNVERIFIED** as a direct statement. Settling experiment: the §9.4 proxy, plus an
ETW `Microsoft-Windows-DxgKrnl` all-keywords trace of a WARP D3D12 run — `DmaPacket` /
`QueuePacket` events name the submission path (the same recipe ROADMAP uses for the present-queue
stall).

**Why this matters enormously for Helios:** the KMD today advertises
`DmaBufferSize = 256 KiB`, `AllocationListSize = PatchLocationListSize =
DXGK_ALLOCATION_LIST_SIZE_GDICONTEXT` from `DxgkDdiCreateContext` (`kmd_render/src/device.rs:389-431`,
per `DX12.md` §3.1) — i.e. the *legacy* command-buffer model. A D3D12 UMD would instead need
`DxgkDdiCreateContext`'s **virtual** flavour and a working GPU VA. That is the same gate as §4.4.

---

## 8. Fences and synchronisation at the DDI

### 8.1 The fence object IS a GPU virtual address

Verbatim, L1575–1598:

```c
typedef struct D3D12DDI_FENCE_PLACEMENT
{
    D3DGPU_VIRTUAL_ADDRESS BaseAddress;
} D3D12DDI_FENCE_PLACEMENT;

typedef enum D3D12DDI_FENCE_FLAGS
{
    D3D12DDI_FENCE_FLAG_NONE           = 0x0,
    D3D12DDI_FENCE_FLAG_BOTTOM_OF_PIPE = 0x1,
} D3D12DDI_FENCE_FLAGS;

typedef struct D3D12DDI_FENCE
{
    D3D12DDI_FENCE_PLACEMENT FenceValue;
    D3D12DDI_FENCE_PLACEMENT FenceMonitoredValue;
    D3D12DDI_FENCE_FLAGS Flags;
} D3D12DDI_FENCE;

typedef struct D3D12DDIARG_CREATE_FENCE
{
    UINT FenceCount;
    _Field_size_(FenceCount) D3D12DDI_FENCE* Fences;
} D3D12DDIARG_CREATE_FENCE;
```

`pfnCreateFence(hDevice, hFence, pArgs) -> HRESULT` (L1787). **The runtime hands the driver an array
of GPU virtual addresses**; the driver does not choose them. Those addresses come from the runtime's
own monitored-fence creation — `D3DKMT_CREATESYNCHRONIZATIONOBJECT2` /
`D3DDDICB_CREATESYNCHRONIZATIONOBJECT2` with `MonitoredFence`, which returns
(staged headers, `tmp/dx12/sdk/d3dukmdt.h:1869-1873`):

```c
    D3DKMT_PTR(VOID*,       FenceValueCPUVirtualAddress);           // out: Read-only mapping of the fence value for the CPU
    D3DKMT_ALIGN64 D3DGPU_VIRTUAL_ADDRESS FenceValueGPUVirtualAddress; // out: Read/write mapping of the fence value for the GPU
} MonitoredFence;
```

`FenceCount > 1` is the multi-adapter (LDA) case — one placement per physical adapter.

### 8.2 Queue-level signal and wait

L2712–2720:
```c
typedef struct D3D12DDIARG_FENCE_OPERATION
{
    D3D12DDI_HFENCE Fence;
    UINT64 Value;
    UINT PhysicalAdapterMask; // Out: The set of adapters to broadcast the operation to
} D3D12DDIARG_FENCE_OPERATION;

typedef void( APIENTRY* PFND3D12DDI_SIGNAL_FENCE )  ( D3D12DDI_HCOMMANDQUEUE, D3D12DDIARG_FENCE_OPERATION*);
typedef void( APIENTRY* PFND3D12DDI_WAIT_FOR_FENCE )( D3D12DDI_HCOMMANDQUEUE, D3D12DDIARG_FENCE_OPERATION*);
```
Note `PhysicalAdapterMask` is annotated **`// Out:`** — the *driver* writes it, telling the runtime
which adapters the operation must be broadcast to. On a single-adapter Helios this is `1`.

There is **no CPU-side wait or signal in the DDI**. `ID3D12Fence::SetEventOnCompletion` and
`ID3D12Fence::Signal` are handled entirely by the runtime against the monitored-fence CPU mapping —
the driver never sees them. **I infer this** from the absence of any such entry point in
`DEVICE_FUNCS_CORE_0109` or `COMMAND_QUEUE_FUNCS_CORE_0001`; it is a strong structural argument, not
a header statement.

### 8.3 Consequence for Helios

`DX12.md` §3.5 records that Helios' KMD leaves `DxgkDdiSignalMonitoredFence`,
`DxgkDdiCreateNativeFence` and the whole native-fence family unset, but that monitored fences
nonetheless *work* on the software-scheduled path (`tools/vehicle_flipwait_probe.c`). §8.1 sharpens
that from "verify, don't assume" to a **specific, testable precondition**: `ID3D12Fence` maps to a
monitored fence whose value lives at a GPU VA the GPU must be able to write. On Helios the GPU
(host-side, via venus) has no access to any guest GPU VA. **How a bottom-of-pipe fence write would
ever land is the single hardest unanswered question for strategy (a).**

Settling experiment: extend `tools/vehicle_flipwait_probe.c` to (1) create a monitored fence via
`D3DKMTCreateSynchronizationObject2`, (2) read back `FenceValueGPUVirtualAddress`, (3) submit a
command that the KMD's null engine is asked to interpret as a fence write, and (4) poll
`FenceValueCPUVirtualAddress`. If the CPU-visible value never moves without dxgkrnl's own
software-scheduler assistance, then D3D12's fence model on Helios must be entirely
software-scheduler-mediated, and the UMD must never claim `BOTTOM_OF_PIPE`.

---

## 9. The minimum viable table

### 9.1 What is structurally mandatory

**Adapter table — all 8.** There is no versioning or optional marker on any of them, and the runtime
has no other way to reach the driver. A NULL in any of the 8 is a call through a null pointer the
first time the runtime uses it.
*(Possible exception: `pfnGetOptionalDDITables`. **UNVERIFIED** whether the runtime null-checks it.
Safest: implement it and return `*puEntries = 0`.)*

**`DEVICE_CORE` (table type 0) — all members of whichever version you negotiate.** There is no
per-slot opt-out mechanism anywhere in the header: `pfnFillDDITable` fills a struct, and a NULL slot
is a crash the first time the runtime dispatches through it. Practically this means:
- **Fill every slot with a named, counting stub first**, then overwrite the implemented ones — the
  exact pattern `umd/src/forward/tables.rs` already uses for D3D11 ("Install … over the stub fill",
  L12/L41).
- The tier caps in §5.2 are what *legitimately* removes work: reporting
  `RaytracingTier = NOT_SUPPORTED` means the runtime will not call `pfnDispatchRays`, but the slot
  must still be non-NULL. **UNVERIFIED that the runtime honours this for every tier.** The
  corresponding D3D11 lesson is `DX12.md` risk #5: advertising a capability that is not backed is a
  lie the OS acts on — the converse (not advertising and hoping the slot is never called) has never
  been tested here.

**`COMMAND_LIST_3D` (1) — all members.** Same argument.

**`COMMAND_QUEUE_3D` (2) — 5 of 7.** `pfnUnused`/`pfnUnused2` are named unused. `pfnUpdateTileMappings`
and `pfnCopyTileMappings` are needed only if `TiledResourcesTier != NOT_SUPPORTED`; a
tier-0 driver still must not leave them NULL.

**`DXGI` (3) — the whole `DXGI*_DDI_BASE_FUNCTIONS` struct.** Helios' D3D11 UMD already implements 18
of these; strong reuse candidate (lane R7).

**Extended features — genuinely optional.** Answer `pfnGetSupportedExtendedFeatures` with zero
features and none of table types 4–27 is ever requested.

### 9.2 The functions that must actually *do something* for a cleared RTV to reach the screen

I infer this minimum from the object graph, not from the header:

*Adapter:* `pfnGetSupportedVersions`, `pfnGetCaps` (the §5.3 set), `pfnGetOptionalDDITables`,
`pfnFillDDITable`, `pfnCalcPrivateDeviceSize`, `pfnCreateDevice`, `pfnDestroyDevice`, `pfnCloseAdapter`. — **8**

*Device core:* `CheckFormatSupport`, `CheckMultisampleQualityLevels`; the
`CalcPrivate/Create/Destroy` triples for `CommandQueue`, `CommandPool`, `CommandList`,
`CommandRecorder`, `Fence`, `DescriptorHeap`, `RootSignature`, `PipelineState`;
`CommandRecorderSetCommandPoolAsTarget`, `ResetCommandPool`;
`GetDescriptorSizeInBytes`, `GetCPUDescriptorHandleForHeapStart`,
`GetGPUDescriptorHandleForHeapStart`, `CreateRenderTargetView`, `CreateShaderResourceView`,
`CreateConstantBufferView`, `CreateUnorderedAccessView`, `CreateDepthStencilView`, `CreateSampler`,
`CopyDescriptors`, `CopyDescriptorsSimple`;
`CalcPrivateHeapAndResourceSizes`, `CreateHeapAndResource`, `DestroyHeapAndResource`,
`CalcPrivateOpenedHeapAndResourceSizes`, `OpenHeapAndResource`, `MapHeap`, `UnmapHeap`,
`MakeResident`, `Evict`;
`CheckResourceAllocationInfo`, `CheckSubresourceInfo`, `CheckResourceVirtualAddress`,
`CheckResourceAllocationHandle`, `CheckExistingResourceAllocationInfo`;
`CalcPrivateShaderSize` + `CreateVertexShader`/`CreatePixelShader`/`CreateComputeShader` +
`DestroyShader`; the four immutable-state triples;
`GetImplicitPhysicalAdapterMask`, `QueryNodeMap`, `GetPresentPrivateDriverDataSize`. — **≈60 of 124**

*Command list:* `CloseCommandList`, `ResetCommandList`, `ClearRenderTargetView`,
`OMSetRenderTargets`, `RsSetViewports`, `RsSetScissorRects`, `SetPipelineState`,
`SetGraphicsRootSignature`, `SetDescriptorHeaps`, `DrawInstanced`, `ResourceBarrier` **or**
`Barrier`, `Present`, `ResourceCopy`, `CopyTextureRegion`, `CopyBufferRegion`. — **≈15 of 75**

*Command queue:* `ExecuteCommandLists`, `SignalFence`, `WaitForFence`. — **3 of 7**

**≈86 functions with real bodies, ~214 slots that must be non-NULL.**

### 9.3 What the header does NOT tell you, restated as a list

1. **UNVERIFIED:** which caps types the runtime demands, in what order, and what a refusal does.
2. **UNVERIFIED:** whether any DDI slot may legally be NULL.
3. **UNVERIFIED:** the meaning of `pfnFillDDITable`'s 5th `UINT` parameter and of
   `D3D12DDI_TABLE_REQUEST::numTables`.
4. **UNVERIFIED:** which `DXGI*_DDI_BASE_FUNCTIONS` shape table type 3 wants.
5. **UNVERIFIED:** the `Interface`/`Version` split of a `D3D12DDI_SUPPORTED_*` constant.
6. **UNVERIFIED:** the DDI-version→Windows-release mapping.
7. **UNVERIFIED:** where the recording memory comes from (no DMA buffer in the DDI — §7.3).

### 9.4 The one experiment that settles 1–5 and 7 at once

**A logging proxy UMD.** Build a small DLL that:
- exports `OpenAdapter12`;
- `LoadLibrary("d3d10warp.dll")`, `GetProcAddress("OpenAdapter12")`, forwards the call;
- wraps the returned `D3D12DDI_ADAPTERFUNCS*` with thunks that log
  `(pfnGetCaps: Type, DataSize)`, `(pfnFillDDITable: TableType, TableSize, UINT, hRTTable)`,
  `(pfnCreateDevice: Interface, Version, Flags, NumReserveRanges)`,
  `(pfnGetSupportedVersions: the returned UINT64 list)`;
- and then, for the CORE and CL tables WARP fills, replaces every slot with a counting thunk so the
  **actual call sequence** of a real `D3D12CreateDevice` + clear + present is recorded.

Registration: point a test adapter's `UserModeDriverName` at the proxy — the same registry surface
`kmd_render/helios_kmd_render.inx:81` uses for Helios' own four entries. Or, cheaper and with no
driver change: WARP is loaded by `IDXGIFactory4::EnumWarpAdapter`, so a DLL-search-order proxy named
`d3d10warp.dll` next to the test executable works for a self-contained probe.

**Output: one log that turns §5.3, §9.1 and §9.2 from hypotheses into a contract.** This costs a day
and is strictly cheaper than any of `DX12.md` §2's four settling questions except the `vulkaninfo`
read. It should be run *before* any D3D12 DDI code is written, and it is the R1-lane equivalent of
what R908 exists to prevent.

⚠ Write it under `tools/` (per CLAUDE.md's note that `probe/` and `host/` were retired), and run it
in **session 1 via a cloned scheduled task** — a windowed D3D12 sample launched from `win_exec`
lands in session 0 and will fake a driver regression (memory `lease-gate-falsified-60th.md`).

---

## 10. Sizing the work against the live D3D11 UMD

### 10.1 What Helios' D3D11 UMD actually fills today

Measured, `umd/src/forward/tables.rs`:

| Installer | Assignments | Target struct | Struct's total slots |
|---|---|---|---|
| `install` (L72) | 144 | `ddi::D3D11DDI_DEVICEFUNCS` | 150 |
| `install_11_1` (L240) | 23 (19 of them **overwrites**) | `D3D11_1DDI_DEVICEFUNCS` | 155 |
| `install_wddm1_3` (L290) | 10 | `D3DWDDM1_3DDI_DEVICEFUNCS` | 164 |
| `install_dxgi` (L12) | 7 | `DXGI_DDI_BASE_FUNCTIONS` | — |
| `install_dxgi_1_1` (L23) | 1 | `DXGI1_1_DDI_BASE_FUNCTIONS` | — |
| `install_dxgi_1_3` (L28) | 10 | `DXGI1_3_DDI_BASE_FUNCTIONS` | — |
| **totals** | **195 assignments** | | |

Unique slots: **157 device-table + 18 DXGI = 175**. Struct sizes measured from the generated
bindings at `umd/target/release/build/helios_umd-7f8cea4aa7a6bcbd/out/d3d10umddi.rs`
(`D3D11DDI_DEVICEFUNCS` 150, `D3D11_1DDI_DEVICEFUNCS` 155, `D3DWDDM1_3DDI_DEVICEFUNCS` 164 fields).

`ROADMAP.md:3289` says "`forward.rs` implements ~220 DDI functions"; the *measured* number of
distinct table slots written is **175**, and the `forward/` module tree is **13 283 lines** across
19 files. The 220 figure presumably counts handler functions including non-slot helpers; the
comparable number for a D3D12 estimate is **175**.

### 10.2 The comparison

| | D3D11 (live) | D3D12 (`_0109`/`_0108`) |
|---|---|---|
| Device-table slots | 164 (WDDM1_3), 157 filled | **124** |
| Command-list slots | — (immediate/deferred contexts share the device table) | **75** |
| Queue slots | — | **7** |
| Adapter slots | 3 (`D3D10DDI_ADAPTERFUNCS`) + 2 in the 10_2 form | **8** |
| DXGI slots | 18 filled | 21–22 (whole struct) |
| **Total non-NULL slots required** | ~175 | **~214** |
| Rust source today | 13 283 lines in `umd/src/forward/` | — |

**A D3D12 UMD is ~1.2× the D3D11 UMD in slot count.** But slot count is the *least* of it. The
D3D11 UMD forwards into a mature D3D11 engine (DXVK) whose object model matches the DDI
one-for-one. The three things with no D3D11 analogue at all are:

1. **Descriptor heaps** — `pfnCreateDescriptorHeap` + `GetCPU/GPUDescriptorHandleForHeapStart` +
   `CopyDescriptors` mean the driver must define a *binary descriptor format* and hand the runtime
   raw CPU pointers into it (`D3D12DDI_CPU_DESCRIPTOR_HANDLE { SIZE_T ptr; }`, L1415;
   `D3D12DDI_GPU_DESCRIPTOR_HANDLE { UINT64 ptr; }`, L1420). Applications memcpy descriptors around.
   There is nothing like this in D3D11.
2. **Root signatures** — `D3D12DDI_ROOT_SIGNATURE_0100` (L12269) and the whole root-argument half of
   the command-list table (16 of its 75 slots).
3. **GPU virtual addresses as an app-visible currency** — `pfnCheckResourceVirtualAddress` returns
   one (L2476), fences *are* ones (§8), root descriptors take them. Which is where §4.4's gate lives.

### 10.3 The one-line conclusion for the Helios plan

The D3D12 UMD DDI is **inventoriable** — this dossier is the complete map, 214 slots across four
tables, all names and signatures pinned to line numbers. It is **not documented** (§1.1), so at
least seven contract questions (§9.3) can only be answered by experiment, and the cheapest one that
answers six of them is the proxy spy in §9.4. But the *inventory* is not the hard part: **§4.4 and
§8.3 are.** D3D12's DDI is written in the currency of GPU virtual addresses, and Helios' KMD says in
its own comments that its guest GPU VA is decorative. Nothing in the D3D12 DDI provides an escape
hatch from that — there is no "opaque handle" alternative to `D3D12DDI_GPU_VIRTUAL_ADDRESS`
anywhere in the header.

---

## Appendix A — reproducing every count in this dossier

```bash
cd /home/rupansh/helios-vgpu/tmp/dx12/sdk

# 19031 lines, SDK 26100
wc -l d3d12umddi.h

# 72 version constants
grep -c "^#define D3D12DDI_SUPPORTED_" d3d12umddi.h
grep -oP "^#define D3D12DDI_SUPPORTED_\K\d+" d3d12umddi.h

# every table struct + member count
python3 - <<'EOF'
import re
lines=open('d3d12umddi.h',encoding='utf-8',errors='replace').read().split('\n')
pat=re.compile(r'^typedef struct (D3D12DDI_\w*(FUNCS|CALLBACKS)\w*)\s*$')
for i,l in enumerate(lines):
    m=pat.match(l)
    if m:
        j=i; cnt=0
        while not lines[j].startswith('}'):
            if re.match(r'\s*(PFN\w+|void\*|VOID\*)\s+\w+;',lines[j]): cnt+=1
            j+=1
        print(f"{m.group(1):62s} L{i+1:6d} members={cnt}")
EOF

# MS docs never mention the entry point
grep -rn "OpenAdapter12" ../../../windows-driver-docs-research-only/   # → nothing

# no pfnGetDDITable in any staged header
grep -rn "GETDDITABLE\|GetDDITable\|SETDDITABLE" *.h                   # → nothing
```

```bash
cd /home/rupansh/helios-vgpu
# D3D11 UMD slot counts
python3 - <<'EOF'
import re
lines=open('umd/src/forward/tables.rs',encoding='utf-8').read().split('\n')
cur=None;sets={}
for l in lines:
    m=re.match(r'pub unsafe fn (\w+)',l)
    if m: cur=m.group(1); sets.setdefault(cur,set())
    mm=re.match(r'\s*f\.(pfn\w+)\s*=',l)
    if cur and mm: sets[cur].add(mm.group(1))
dev=sets['install']|sets['install_11_1']|sets['install_wddm1_3']
dxgi=sets['install_dxgi']|sets['install_dxgi_1_1']|sets['install_dxgi_1_3']
print(len(dev), len(dxgi))   # -> 157 18
EOF
```

On win11 (read-only, no build, no install):
```powershell
$db = "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64\dumpbin.exe"
& $db /exports C:\Windows\System32\d3d10warp.dll | Select-String "OpenAdapter"
```

## Appendix B — where the other lanes pick this up

- **R2** (runtime↔UMD semantics): §6 object model, §7 recording/submission, §8 fences. The seven
  UNVERIFIED items in §9.3 are the questions R2 must answer or inherit.
- **R3** (vkd3d separability): compare `D3D12DDI_DEVICE_FUNCS_CORE_0109`'s 124 entries against
  vkd3d-proton's `d3d12_device` vtable — the two are *different shapes* (COM `ID3D12Device` vs a
  flat driver table), which is the concrete form of `DX12.md` §2(a)'s open question.
- **R4** (D3D11 UMD as template): §10.1's measured numbers, and the three reuse candidates —
  `umd/src/forward/handles.rs` (handles are literally D3D10 typedefs, §6.1),
  `umd/src/format.rs`, and `install_dxgi*` (§2.4).
- **R5 / R6** (KMD gap, D3DKMT surface): **§4.3, §4.4, §7.3 and §8 are written for you.** The two
  named settling experiments (`tools/gpuva_probe.cpp`, the extended `vehicle_flipwait_probe`) are
  the cheapest gates in this whole workstream.
- **R7** (presentation): §2.4's DXGI-table question, §3.2's "`pfnPresent` is on the command list",
  and `D3D12DDIARG_PRESENT_0001` / `D3D12DDI_PRESENT_0051` / `D3D12DDI_PRESENT_CONTEXTS_0051`
  (L1630, L7226, L7237).
- **R8** (DXIL, caps, feature levels): §5.2 tiers, §5.4 shader models, and the
  `3DPIPELINESUPPORT`/`3DPIPELINESUPPORT1` trap at §5.3.
- **R11** (registration): §1.1's measured WARP export set — note WARP exports `OpenAdapter`, not
  `OpenAdapter10`, while Helios exports `OpenAdapter10`; both work, and `OpenAdapter12` is the
  D3D12 name in both.
