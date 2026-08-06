# DDI_REFERENCE.md — the `d3d12umddi` contract, reconstructed

**What this is.** The reference manual for the D3D12 user-mode display driver DDI as Helios must
implement it (`DECISIONS.md` D1: `helios_umd12.dll` implements `d3d12umddi.h` and forwards into
vkd3d-proton's `ID3D12*` COM objects). It is a *reconstruction*: every table, every slot, every
struct, every caps rule, plus the semantics that are not in any header, assembled so an
implementation session can open this file and start writing code.

**What this is not.** It is not Microsoft documentation, because no Microsoft document describes this
contract as a whole (`DECISIONS.md` H1). ⚠ **Corrected 2026-08-05 — the older phrasing, "zero
conceptual articles", was true of one corpus and false of another:**

- **driver-docs** (`learn.microsoft.com/windows-hardware/drivers/ddi/`) is ~600 auto-generated
  reference stubs with no Remarks and no conceptual articles. Unchanged, and `OpenAdapter12` appears
  nowhere in it.
- **`microsoft.github.io/DirectX-Specs`** is a different matter: **44 of its 90 documents carry an
  explicit `DDI` section heading, and 123 of this header's 399 `PFND3D12DDI_*` typedefs (31 %) are
  named in its prose** — including the three hardest areas in this file: resource binding (§9.6/§9.9),
  resources and heaps (§9.7), and barriers (§9.10). `docs/dx12/SPECS.md` triages all 90 and registers
  235 verified findings against them.
- **Both corpora are silent on exactly what `D12-G5` had to measure.** `OpenAdapter12`,
  `pfnSetCommandListDDITableCb` and `pfnGetPresentPrivateDriverDataSize` appear **nowhere in either**
  (verified by grep at the pin). The spy covered the genuinely undocumented part.

⛔ **The specs are drafting-era documents and are NOT the arbiter of what exists here.** 173 of the 296
`PFND3D12DDI_*` they name are absent from SDK 10.0.26100.0, and several publish struct shapes that no
longer match the header. Arbiters, in order: the `D12-G5` log → the staged `d3d12umddi.h` →
`D3D12Core.dll`'s strings → the spec. `SPECS.md` §6 lists the six ways the corpus misleads.

It is also not `PRESENT.md` (the frame path), not `ARCHITECTURE.md` (the DLL/crate split), and not
`SUBSTRATE.md` (vkd3d + venus). Those own their material; this file points at them.

**The two reconstruction sources, and how to tell them apart.**

| Source | What it gives | How it is cited here |
|---|---|---|
| `tmp/dx12/sdk/d3d12umddi.h`, 19 031 lines, Windows SDK **10.0.26100.0** | shapes: every struct, enum, typedef, table | `umddi:NNNN` — a line number in that file |
| `docs/dx12/research/d3d12core-driverstrings.txt`, 270 lines, extracted from the live `C:\Windows\System32\D3D12Core.dll` (10.0.26100.8737) | **semantics** — the runtime saying in English what the driver must do | `strings:NN` — the 1-based line in that file |

The 270-line file is the `Driver|driver|DDI` subset of a 25 782-line extraction at
`tmp/dx12/research/d3d12core-strings.txt` (uncommitted); citations into the full file are written
`fullstrings:NNNNN`. **Those strings are the only prose contract for this DDI that exists.** They
are quoted liberally below and they are quoted *verbatim*, including Microsoft's own typos and
missing full stops.

⚠ **All `umddi:` line numbers are pinned to SDK 10.0.26100.0.** The header is Microsoft's and is
not committed. Re-stage before reading any citation (`DECISIONS.md` §preamble):

```powershell
# win_exec, once per machine
New-Item -ItemType Directory -Force -Path Z:\tmp\dx12\sdk | Out-Null
$src = "C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0"
foreach($f in @("um\d3d12umddi.h","um\d3d12.h","um\d3dumddi.h","um\dxgiddi.h","shared\d3dkmthk.h","shared\d3dkmdt.h","shared\d3dukmdt.h","shared\d3dkmddi.h","km\dispmprt.h")) {
  Copy-Item "$src\$f" "Z:\tmp\dx12\sdk\$(Split-Path $f -Leaf)" -Force }
```

⚠ **This list is one file longer than `DECISIONS.md` §preamble's**: it adds `um\dxgiddi.h`, which
`DECISIONS.md` does not stage. §2.3 needs it — table type 3 (`DXGI`) is one of the four tables §14.1
declares structurally mandatory and its struct is defined *only* in `dxgiddi.h`. The file is present
on win11 under `10.0.26100.0` (verified). Cite it as `dxgiddi:NNNN`.

Verify the pin before trusting a line number:

```bash
wc -l /home/rupansh/helios-vgpu/tmp/dx12/sdk/d3d12umddi.h   # must print 19031
```

**Every count in this document was recomputed from the header for this file, not copied from the
research dossiers.** Where a dossier or `DECISIONS.md` carries a different number, §17 lists the
correction and how it was measured. Anything not established from a file, a command or a quoted
string is marked literally **UNVERIFIED** with the experiment that settles it.

---

## 1. Entry point and negotiation

### 1.1 The export is `OpenAdapter12`

The header declares only the typedef, never the symbol (umddi:2694):

```c
typedef HRESULT (APIENTRY *PFND3D12DDI_OPENADAPTER)(_Inout_ D3D12DDIARG_OPENADAPTER*);
```

The name is established empirically from Microsoft's own reference D3D12 UMD, WARP —
measured on win11 (`research/R1` §1.1, `d3d10warp.dll` 10.0.26100.8875, 5 931 008 bytes):

```
> & dumpbin.exe /exports C:\Windows\System32\d3d10warp.dll | Select-String "OpenAdapter"
        203    2 001CF510 OpenAdapter
        204    3 000FFF70 OpenAdapter10_2
        205    4 000FFBB0 OpenAdapter12
```

This matches what Helios already exports and refuses at `umd/src/adapter.rs:178`. The runtime also
has a string for the failure mode: `Driver does not have OpenAdapter entrypoint` (strings:32).

⛔ **Do not change `umd/src/adapter.rs:178` into a working body in a commit that does not also make
the body reachable.** That is `DECISIONS.md` §7.1 (the R908 lesson: ~230 lines of D3D12 scaffolding
behind `#[allow(unreachable_code)]`), and the comment at `umd/src/adapter.rs:170-177` records it in
the source. `helios_umd12.dll` is a *new* DLL (D3, D4); `helios_umd.dll`'s `OpenAdapter12` keeps
refusing.

### 1.2 `D3D12DDIARG_OPENADAPTER` — four fields, and how it differs from D3D10/11

Verbatim, umddi:2686-2694:

```c
typedef struct D3D12DDIARG_OPENADAPTER
{
    D3D12DDI_HRTADAPTER            hRTAdapter;         // in:  Runtime handle
    D3D12DDI_HADAPTER              hAdapter;           // out: Driver handle
    CONST D3DDDI_ADAPTERCALLBACKS* pAdapterCallbacks;  // in:  Pointer to runtime callbacks
    D3D12DDI_ADAPTERFUNCS*         pAdapterFuncs;      // out: Driver function table
} D3D12DDIARG_OPENADAPTER;
```

| | D3D10/11 `D3DDDIARG_OPENADAPTER` | D3D12 `D3D12DDIARG_OPENADAPTER` |
|---|---|---|
| Carries `Interface` / `Version` | **yes**, in the open-adapter arg | **no** |
| Version negotiation happens | inside `OpenAdapter10` | **after**, via `pfnGetSupportedVersions` |
| Adapter callbacks | `D3DDDI_ADAPTERCALLBACKS` | **same struct** |
| Output table | `D3D10DDI_ADAPTERFUNCS` (3 slots) | `D3D12DDI_ADAPTERFUNCS` (**8** slots) |

**Consequence for the port.** `umd/src/adapter.rs::open_adapter_common` (`umd/src/adapter.rs:200+`)
dispatches on `open.Interface` *inside* `OpenAdapter10`. The D3D12 equivalent cannot: at
`OpenAdapter12` time nothing is negotiated. Fill the adapter table, stash `hRTAdapter`, return
`S_OK`; the interface/version arrives later on `D3D12DDIARG_CALCPRIVATEDEVICESIZE` and
`D3D12DDIARG_CREATEDEVICE_*`.

The adapter-scope callback table is the one Helios already receives for D3D11 — `d3dumddi.h`
(staged) 4633-4640, **3 members**, the third version-gated:

```c
typedef struct _D3DDDI_ADAPTERCALLBACKS
{
    PFND3DDDI_QUERYADAPTERINFOCB            pfnQueryAdapterInfoCb;
    PFND3DDDI_GETMULTISAMPLEMETHODLISTCB    pfnGetMultisampleMethodListCb;
#if (D3D_UMD_INTERFACE_VERSION >= D3D_UMD_INTERFACE_VERSION_WDDM2_4)
    PFND3DDDI_QUERYADAPTERINFOCB2           pfnQueryAdapterInfoCb2;
#endif
} D3DDDI_ADAPTERCALLBACKS;
```

### 1.3 `D3D12DDI_ADAPTERFUNCS` — all 8 members

Two versions exist and differ **only** in the `pfnCreateDevice` signature: `D3D12DDI_ADAPTERFUNCS`
(umddi:2674-2684) and `D3D12DDI_ADAPTERFUNCS_0109` (umddi:13640-13650). Verbatim:

```c
typedef struct D3D12DDI_ADAPTERFUNCS_0109
{
    PFND3D12DDI_CALCPRIVATEDEVICESIZE         pfnCalcPrivateDeviceSize;
    PFND3D12DDI_CREATEDEVICE_0109             pfnCreateDevice;      // PFND3D12DDI_CREATEDEVICE_0003 in the base form
    PFND3D12DDI_CLOSEADAPTER                  pfnCloseAdapter;
    PFND3D12DDI_GETSUPPORTEDVERSIONS          pfnGetSupportedVersions;
    PFND3D12DDI_GETCAPS                       pfnGetCaps;
    PFND3D12DDI_GETOPTIONALDDITTABLES         pfnGetOptionalDDITables;
    PFND3D12DDI_FILLDDITTABLE                 pfnFillDDITable;
    PFND3D12DDI_DESTROYDEVICE                 pfnDestroyDevice;
} D3D12DDI_ADAPTERFUNCS_0109;
```

Signatures, verbatim (umddi:2587-2622):

```c
typedef enum D3D12DDI_CREATE_DEVICE_FLAGS
{
    D3D12DDI_CREATE_DEVICE_FLAG_NONE                  = 0x0,
    D3D12DDI_CREATE_DEVICE_FLAG_DISABLE_IMPLICIT_MGPU = 0x1,
    D3D12DDI_CREATE_DEVICE_FLAG_DEBUGGABLE            = 0x2,
} D3D12DDI_CREATE_DEVICE_FLAGS;

typedef struct D3D12DDIARG_CALCPRIVATEDEVICESIZE
{
    UINT                          Interface;          // in:  Interface version
    UINT                          Version;            // in:  Runtime Version
    D3D12DDI_CREATE_DEVICE_FLAGS  Flags;              // in:  Flags
} D3D12DDIARG_CALCPRIVATEDEVICESIZE;

typedef SIZE_T  (APIENTRY *PFND3D12DDI_CALCPRIVATEDEVICESIZE)(D3D12DDI_HADAPTER, _In_ CONST D3D12DDIARG_CALCPRIVATEDEVICESIZE*);
typedef HRESULT (APIENTRY *PFND3D12DDI_CLOSEADAPTER)(D3D12DDI_HADAPTER);
typedef HRESULT (APIENTRY *PFND3D12DDI_GETSUPPORTEDVERSIONS)(D3D12DDI_HADAPTER,
    _Inout_ UINT32* puEntries, _Out_writes_opt_( *puEntries ) UINT64* pSupportedDDIInterfaceVersions);
typedef struct D3D12DDIARG_GETCAPS { D3D12DDICAPS_TYPE Type; VOID* pInfo; VOID* pData; UINT DataSize; } D3D12DDIARG_GETCAPS;
typedef HRESULT (APIENTRY *PFND3D12DDI_GETCAPS)(D3D12DDI_HADAPTER, _In_ CONST D3D12DDIARG_GETCAPS*);
typedef VOID    (APIENTRY *PFND3D12DDI_DESTROYDEVICE)(D3D12DDI_HDEVICE);
```

Three things worth pinning:

- **`pfnGetCaps` is adapter-scoped** (`D3D12DDI_HADAPTER`), not device-scoped. All 43 caps types
  (§11) are answered before any device exists.
- **`pfnDestroyDevice` lives on the ADAPTER table** (umddi:13649), not the device table. This is a
  shape difference from D3D11 and a classic place to leave a NULL.
- `pfnGetSupportedVersions` is a two-call query: the `_Inout_ UINT32* puEntries` +
  `_Out_writes_opt_` annotation pair is the standard count-then-fill idiom. The header states no
  prose. ✅ **CONFIRMED 2026-08-06 on Helios itself, not by the §15 spy** (S5,
  `tmp/dx12/gates/G6/RESULT.md`): the runtime calls it **twice**, first with
  `*puEntries = 0` and `pSupportedDDIInterfaceVersions == NULL`, then with `*puEntries = 1` and a
  real buffer — i.e. it takes the count the first call writes and sizes the second to it. A driver
  that ignores the null-buffer form and writes through it faults the runtime on the first call.
- ⛔ **`pfnGetCaps` runs BEFORE `pfnGetSupportedVersions`**, which is the opposite of
  `ARCHITECTURE.md` §1.2's step order and is now corrected there. Measured sequence:
  `GetCaps(1074)`, `GetCaps(1007)`, then the two version calls. ⇒ **the caps answer cannot depend on
  a negotiated version** (§11 is written on that assumption and is unaffected; anything that starts
  branching on the revision would not be).

### 1.4 `D3D12DDIARG_CREATEDEVICE_0109`

Verbatim, umddi:13618-13636:

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
    D3D12DDI_CREATE_DEVICE_FLAGS    Flags; // in:

    D3D12DDI_GPU_VIRTUAL_ADDRESS_RANGE* pReserveRanges;
    UINT NumReserveRanges;
} D3D12DDIARG_CREATEDEVICE_0109;

typedef HRESULT (APIENTRY *PFND3D12DDI_CREATEDEVICE_0109)(D3D12DDI_HADAPTER, _In_ CONST D3D12DDIARG_CREATEDEVICE_0109*);
```

`D3D12DDIARG_CREATEDEVICE_0003` (umddi:2655-2670) is identical minus `pReserveRanges` /
`NumReserveRanges`. `D3D12DDI_GPU_VIRTUAL_ADDRESS_RANGE` (umddi:7964) is
`{ D3D12DDI_GPU_VIRTUAL_ADDRESS StartAddress; UINT64 SizeInBytes; }` and
`D3D12DDI_GPU_VIRTUAL_ADDRESS` is `UINT64` (umddi:92).

⚠ **The `p12UMCallbacks` union is exactly the `adapter.rs:36-45` landmine class** — a 376..392-byte
out-of-bounds write into the runtime's heap, caused by an `else`-as-default in interface dispatch.
The four arms are **12 / 14 / 17 / 18** pointer-wide slots (§6.2, recounted); reading the wrong arm
reads past the end of a shorter one. `DECISIONS.md` §7.4 is the rule: **a closed enum with an
exhaustive match**, never an `else` that picks the largest.

⚠ `D3D12DDI_CREATE_DEVICE_FLAG_DEBUGGABLE` arrives on **both** `pfnCalcPrivateDeviceSize` and
`pfnCreateDevice`. The private size may legitimately differ between debug and retail, so the two
sites must compute it from the same function of `Flags` — never `size_of::<Device>()` at one site
and a constant at the other.

### 1.5 Version-token encoding — the complete rule, with the trap

The base definitions, verbatim (umddi:37-56):

```c
#define D3D12DDI_MAJOR_VERSION 12
#define D3D12DDI_MINOR_VERSION 2
#define D3D12DDI_INTERFACE_VERSION ((D3D12DDI_MAJOR_VERSION << 16) | D3D12DDI_MINOR_VERSION)
#define D3D12DDI_BUILD_VERSION 8
#define D3D12DDI_SUPPORTED ((((UINT64)D3D12DDI_INTERFACE_VERSION) << 32) | (((UINT64)D3D12DDI_BUILD_VERSION) << 16))
#define D3D12DDI_INTERFACE_VERSION_R0       D3D12DDI_INTERFACE_VERSION
#define D3D12DDI_SUPPORTED_0003             D3D12DDI_SUPPORTED
```

So every token is a `UINT64`:

```
D3D12DDI_SUPPORTED_NNNN = ((UINT64)((12 << 16) | MINOR_Rn) << 32) | ((UINT64)BUILD_VERSION_NNNN << 16)
```

The nine release "minors" (grepped, all present in this header):

| Release | `D3D12DDI_MINOR_VERSION_Rn` | umddi line | `INTERFACE_VERSION_Rn` |
|---|---|---|---|
| R0 | 2 | 39 | `0x000C0002` |
| R1 | 10 | 3182 | `0x000C000A` |
| R2 | 20 | 4055 | `0x000C0014` |
| R3 | 30 | 5914 | `0x000C001E` |
| R4 | 40 | 6532 | `0x000C0028` |
| R5 | 50 | 6998 | `0x000C0032` |
| R6 | 60 | 8438 | `0x000C003C` |
| R7 | 70 | 9006 | `0x000C0046` |
| R8 | 80 | 10148 (redefined identically at 10548) | `0x000C0050` |

⚠ **The trap: `D3D12DDI_BUILD_VERSION_NNNN` is NOT `NNNN`.** For every constant up to `_0089` the
build value is only the **rev digit** — the release lives in the interface half. From `_0090`
onward the convention flips and the build value is the full decimal `NNNN`. Measured
(`grep -oP "^#define D3D12DDI_BUILD_VERSION_\K\d+\s+\d+"`):

```
0010=0 0011=1 … 0015=5      0020=0 … 0028=8      0030=0 … 0034=4      0040=0 … 0043=3
0050=0 … 0054=4             0060=0 … 0065=5      0070=0 … 0076=6      0080=0 … 0084=4 0086=6 0088=8 0089=9
0090=90 0091=91 0092=92 … 0108=108 0109=109 0110=110
```

Worked examples, computed from the rule above and cross-checked against the `#define`s:

| Constant | umddi | Release | Build | Value |
|---|---|---|---|---|
| `D3D12DDI_SUPPORTED_0003` | 56 | R0 `0x000C0002` | 8 | `0x000C0002_00080000` |
| `D3D12DDI_SUPPORTED_0040` | 6536 | R4 `0x000C0028` | **0** | `0x000C0028_00000000` |
| `D3D12DDI_SUPPORTED_0054` | — | R5 `0x000C0032` | 4 | `0x000C0032_00040000` |
| `D3D12DDI_SUPPORTED_0080` | — | R8 `0x000C0050` | **0** | `0x000C0050_00000000` |
| `D3D12DDI_SUPPORTED_0089` | 11076 | R8 `0x000C0050` | 9 | `0x000C0050_00090000` |
| `D3D12DDI_SUPPORTED_0090` | 11121 | R8 `0x000C0050` | 90 | `0x000C0050_005A0000` |
| `D3D12DDI_SUPPORTED_0108` | 12779 | R8 `0x000C0050` | 108 | `0x000C0050_006C0000` |
| `D3D12DDI_SUPPORTED_0109` | 13395 | R8 `0x000C0050` | 109 | `0x000C0050_006D0000` |
| `D3D12DDI_SUPPORTED_0110` | 13657 | R8 `0x000C0050` | 110 | `0x000C0050_006E0000` |

⛔ **Never hand-write these values.** Take them from a bindgen'd header
(`DECISIONS.md` §7.2). The table above exists so a reader can *check* a value, not so anyone
transcribes one. `research/R1` §1.5's worked example "`_0080` = `0x000C0050_00500000`" is wrong for
exactly this reason (it assumed build = 80); the correct value is `0x000C0050_00000000`.

**All 72 `D3D12DDI_SUPPORTED_*` constants** (`grep -c "^#define D3D12DDI_SUPPORTED_"` → 72):

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

Gaps are real: there is no `_0085` and no `_0087`.

✅ **CONFIRMED (`D12-G5`, was "a load-bearing inference"):**
`D3D12DDIARG_CREATEDEVICE::Interface` receives the **high 32 bits** of the chosen token
(`INTERFACE_VERSION_Rn`) and `::Version` the **low 32 bits** (`BUILD_VERSION_NNNN << 16`). The spy
logged both the driver's returned token list and the `Interface`/`Version` pair the runtime came back
with: `((UINT64)Interface << 32) | Version` equals the list entry **bit for bit**
(`0x000c0050_006e0000`, WARP's `version[76]`). Dispatching on the *pair* as an opaque key is still
the right implementation, but the split itself is no longer a guess.

⚠ The first cut of the spy capped its version capture at 64 entries while WARP returns **77**, and
therefore printed "NO MATCH in pfnGetSupportedVersions' list" — a truncated instrument reading
exactly like a finding. Raise the cap before believing a mismatch.

The runtime's failure string for a bad handshake is `Failed to find matching DDI versions`
(strings:167).

✅ **The DDI-version → Windows-release mapping, for one build.** The header records only a feature
banner per version, never an OS build, and one measurement does not give the table — but for
**26100.8875**: WARP offers **77** tokens (13 D3D11-era `0x000b00xx` ones plus `_0003` … `_0110`) and
the runtime picks the **newest**, `D3D12DDI_SUPPORTED_0110`. Forced single-token runs show it also
accepts `_0109`, `_0089` and `_0040` (§15.4).

### 1.6 Which version to implement

✅ **DECIDED 2026-08-06 — `DECISIONS.md` D12: `_0110`, advertised as a set of exactly ONE token,
filling the `_0109`-generation tables.** Everything below is the argument that produced it and is
kept because the rejected arm was measured, not assumed. ⛔ D12 is authoritative; nothing here
reopens it. The one thing D12 adds that this section did not say: **advertise a one-element set**, so
the runtime either negotiates `_0110` or fails the handshake, which makes the closed-enum dispatch
of §12 trap 2 exhaustive with a single legal arm.

⚠ **Two measurements from `D12-G5` change the terms of this section; read them before the argument
below.**

1. **This Windows negotiates `_0110`, not `_0109`.** `_0110` adds no table struct of its own — it
   reuses `ADAPTERFUNCS_0109`, `DEVICE_FUNCS_CORE_0109` (992 B, observed), `COMMAND_LIST_FUNCS_3D_0108`
   (600 B) and `COMMAND_QUEUE_FUNCS_CORE_0001` (56 B). So the *table choice* below is right and only
   the **token** changes: report `_0110` and fill the `_0109`/`_0108` shapes.
2. **`_0040` is accepted by this build**, and a triangle presents on it, at **96 core + 58 CL** slots
   (§15.4). The old text called reporting a single old token a sizing experiment; it is now a real
   option. What it costs is the *old object model* — `_0040` predates the pool + recorder split and
   carries the retired command-**allocator** family, which reason 3 below is about.

Recommendation, unchanged in substance: **negotiate `_0110` and fill the `_0109`-generation tables**
— `D3D12DDI_DEVICE_FUNCS_CORE_0109` (124 slots) +
`D3D12DDI_COMMAND_LIST_FUNCS_3D_0108` (75 slots) + `D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001` (7) +
`D3D12DDI_ADAPTERFUNCS_0109` (8). Reasons, in order:

1. It is the newest coherent set in this header (`_0110` adds only `ExecuteIndirectTier` caps and
   reuses `ADAPTERFUNCS_0109`), and ✅ **measured: it is the shape this 26100-era runtime asks for
   first** — `_0110` is what it picked out of WARP's 77-entry list.
2. Every tier that would otherwise pull in work is *declinable through caps* (§11), not through
   table shape. Choosing an older version does not remove the caps gauntlet; it removes slots you
   would have stubbed anyway.
3. The older revisions carry retired shapes you must not port — notably the command **allocator**
   family (`pfnCalcPrivateCommandAllocatorSize` / `pfnCreateCommandAllocator` /
   `pfnDestroyCommandAllocator` / `pfnResetCommandAllocator`, umddi:1740-1743), which exists only up
   to `CORE_0033` and was replaced at `_0040` by pool + recorder (§8.1).

**The counter-argument was real, and it has now been measured rather than assumed:** `research/R2`
§5.4 proposed reporting a single old token (`D3D12DDI_SUPPORTED_0040`, a 96-slot core table with no
state objects, no mesh shaders, no enhanced barriers, no work graphs) purely to size the project.
✅ **§15.4 ran it: `_0040` is accepted and a triangle presents on it.** So the trade is live and it
is a real decision to make at P3, not a hypothesis:

| | `_0110` (recommended) | `_0040` |
|---|---:|---:|
| baseline slots | 214 (8 + 124 + 75 + 7) | **169** (8 + 96 + 58 + 7) |
| object model | pool + recorder (§8.1) | the retired command **allocator** family |
| caps gauntlet | unchanged | unchanged — an older token does not soften §11.5 |

⛔ Whichever is chosen, choose it **once and explicitly**, and derive every table struct from the
matching header revision through bindgen. A `_0040` token with `_0109`-shaped tables is the R702
class with the size handed to you in the argument.

---

## 2. The table model

### 2.1 `D3D12DDI_TABLE_TYPE` — all 25 values

Verbatim, umddi:2488-2516 (**25 enumerators**; 5, 6 and 18 are absent — retired):

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

**Direction.** Most types are *driver-filled* (the runtime hands a buffer, the driver writes
function pointers). Four are the reverse — the runtime hands the driver a table to **read**,
through `pfnSetExtendedFeatureCallbacks` whose SAL says `_In_reads_(TableSize)` (umddi:4100-4101):

| Type | Direction | Struct | Members | umddi |
|---|---|---|---|---|
| 0 `DEVICE_CORE` | driver → runtime | `D3D12DDI_DEVICE_FUNCS_CORE_*` | 89…124 | 3060…13451 |
| 1 `COMMAND_LIST_3D` | driver → runtime | `D3D12DDI_COMMAND_LIST_FUNCS_3D_*` | 51…75 | 2999…13303 |
| 2 `COMMAND_QUEUE_3D` | driver → runtime | `D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001` | 7 | 2729 |
| 3 `DXGI` | driver → runtime | *not in this header* — `dxgiddi.h` | 21–22 | — |
| 8 / 27 `EXTENDED_FEATURES` | driver → runtime | `D3D12DDI_EXTENDED_FEATURES_FUNCS_0020/0021/0096` | 3 / 4 / 4 | 4086 / 4103 / 11879 |
| 10 `SHADERCACHE_CALLBACKS` | **runtime → driver** | `D3D12DDI_SHADERCACHE_CALLBACKS_0021` | 2 | 4266 |
| 16 `CONTENT_PROTECTION_CALLBACKS` | **runtime → driver** | `D3D12DDI_CONTENT_PROTECTION_CALLBACKS_0030` | 1 | 13845 |
| 23 `DOWNLEVEL_SUPPORT_CALLBACKS` | **runtime → driver** | `D3D12DDI_DOWNLEVEL_SUPPORT_CALLBACKS_0054` | 3 | 18305 |
| 25 `PIN_RESOURCES_CALLBACKS` | **runtime → driver** | `D3D12DDI_PIN_RESOURCES_CALLBACKS_0076` | 2 | 18380 |
| 4,7,9,11–15,17,19–22,24,26 | driver → runtime | video / protection / experimental | — | — |

⚠ **"A baseline Helios device needs exactly four: 0, 1, 2 and 3" was wrong in both directions.**
`D12-G5` measured what the runtime actually fills for a plain device + swapchain + present:

| type | filled? | note |
|---|---|---|
| 0 `DEVICE_CORE` | ✅ once | 992 B |
| 1 `COMMAND_LIST_3D` | ✅ **twice** | indices 0 and 1, distinct `hRTTable` (§2.2) |
| 2 `COMMAND_QUEUE_3D` | ✅ once | 56 B |
| 3 `DXGI` | ⛔ **never** | not at device creation, not by a flip-model swapchain, not across 20 presents (§2.3) |
| 27 `0096_EXTENDED_FEATURES` | ✅ once, **32 B** | filled with **no** extended-features handshake, for a baseline device. ⚠ **Version-dependent**: at `_0089` and `_0040` the runtime fills type **8** `0020_EXTENDED_FEATURES` instead, same 32 bytes |

So the real baseline is **0, 1 (×2), 2 and 27** (or 8 on an older token). Everything else ≥4 is still reached only through the
extended-features handshake, and a driver that answers `pfnGetSupportedExtendedFeatures` with
**zero features** never sees those. That remains the honest posture and it matches the KMD's
(protected-content and video DDIs unset) — but type 27 arrives regardless, so it needs an answer.

```c
typedef enum D3D12DDI_FEATURE_0020            // umddi:4062-4074
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
```

⚠ Returning a value outside that enum is caught:
`PFND3D12DDI_GET_SUPPORTED_EXTENDED_FEATURES_0020 returned an invalid D3D12DDI_FEATURE_0020.`
(strings:237).

### 2.2 ⚠ There is no `pfnGetDDITable` / `pfnGetDDITable32`

Verified by absence — `grep -c "GETDDITABLE\|GetDDITable\|SETDDITABLE" d3d12umddi.h` → **0**. Any
document or memory that names those symbols is describing a different DDI. The real mechanism is a
**pair** on the adapter table (umddi:2518-2528), verbatim:

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

The doubled `TT` in `GETOPTIONALDDITTABLES` / `FILLDDITTABLE` is Microsoft's typo and is
load-bearing — those are the actual identifiers bindgen will emit.

`pfnFillDDITable`'s unnamed parameters, in order:

```
(hAdapter, TableType, pTable /* inout: the RUNTIME's buffer */, TableSize /* SIZE_T */,
 UINT /* see below */, hRTTable /* optional runtime handle for this table instance */)
```

⚠⚠ **`DECISIONS.md` §7.3 in its exact D3D12 form: honour `TableSize`.** Never write
`size_of::<D3D12DDI_DEVICE_FUNCS_CORE_0109>()` bytes. This is the R702 class — 24H2 passed 576 B
for a 592-byte `DRIVERCAPS` and the D3D11 driver wrote past it. D3D12 *parameterises* the size
explicitly, so there is no excuse. The shape to write:

```rust
// helios_umd12: the only legal fill.
let n = core::cmp::min(table_size, core::mem::size_of::<ddi12::D3D12DDI_DEVICE_FUNCS_CORE_0109>());
if n < core::mem::size_of::<ddi12::D3D12DDI_DEVICE_FUNCS_CORE_0109>() {
    FILL_TRUNCATED.fetch_add(1, Ordering::Relaxed);   // named counter, CLAUDE.md rule 2
}
core::ptr::copy_nonoverlapping(&filled as *const _ as *const u8, p_table as *mut u8, n);
```

✅ **MEASURED (`D12-G5`): the 5th `UINT` is the command-list table INDEX.** The runtime fills
`D3D12DDI_TABLE_TYPE_COMMAND_LIST_3D` **twice** during device creation, in immediate succession, with
the 5th `UINT` = **0** then **1** and two distinct `hRTTable` handles (`0x3E0`, `0x638`). Both fills
are the same 600-byte `_0108` shape. Those handles are exactly what the driver later passes to
`pfnSetCommandListDDITableCb` — WARP was observed calling
`pfnSetCommandListDDITableCb(hRTCommandList, 0x3E0)` on every command-list create (§9.3, §15.1 #9).
⇒ **`hRTTable` must be stashed per index at fill time**; there is no other way to obtain it.

⚠ `D3D12DDI_TABLE_REQUEST::numTables` is still unexercised: `pfnGetOptionalDDITables` was called
once and WARP answered `*puEntries = 0`, and the runtime still filled two command-list tables. So the
second table is **not** something the driver asks for — the runtime provides it unconditionally.

✅ **MEASURED: `TableSize` is exactly `size_of` the negotiated revision's struct** — 992 / 600 / 56 at
`_0110` and `_0109`, matching `DEVICE_FUNCS_CORE_0109` / `COMMAND_LIST_FUNCS_3D_0108` /
`COMMAND_QUEUE_FUNCS_CORE_0001` byte for byte. ⛔ **And it moves with the version**: at `_0089` the
runtime passes **976 / 552**, at `_0040` **768 / 464** (§15.4). That is the R702 class with the
correct size handed to you in the argument — there is no excuse for `size_of::<T>()`.

⚠ **`pfnGetOptionalDDITables` may only request table type 1.** The runtime says so:

> `PFND3D12DDI_GETOPTIONALDDITTABLES only supports D3D12DDI_TABLE_TYPE_COMMAND_LIST_3D.  An unsupported table type was requested.` — strings:238

So the *only* legal use of that entry point is to ask for **extra command-list tables**. That
strongly implies the `numTables` field and the 5th `UINT` of `pfnFillDDITable` are a multiplicity
index for command-list tables — the same multiplicity the driver exploits via
`pfnSetCommandListDDITableCb` (§9.3). **UNVERIFIED** as a direct statement; the safest baseline is
to implement `pfnGetOptionalDDITables` and return `*puEntries = 0`, which is the "I want no extra
tables" answer and cannot be misread. Settling experiment: §15.

**Versioning per table is by struct, not by field.** There is no size or flags header inside any
table. `D3D12DDI_DEVICE_FUNCS_CORE_0109` is simply a different, longer struct than `…_0108`. The
runtime tells you which one it wants via the negotiated version plus `TableSize`.

### 2.3 Table type 3 — the DXGI table

**`D3D12DDI_TABLE_TYPE_DXGI` has no struct in `d3d12umddi.h`.** The verifiable form of that claim is
the one to quote: **no `DXGI*_DDI_BASE_FUNCTIONS` struct appears anywhere in the header** —
`grep -c DDI_BASE_FUNCTIONS d3d12umddi.h` → **0** — and the only DXGI *DDI* tokens in it are
umddi:1620-1621 (`DXGI_DDI_ARG_BLT_FLAGS`, `DXGI_DDI_MODE_ROTATION`, both inside `D3D12DDIARG_BLT`)
and umddi:2493 (the enum value itself).

⛔ Do **not** state this as "the only `DXGI` tokens in the header": `DXGI_RATIONAL` (a struct) and
`DXGI_COLOR_SPACE_TYPE` occur at 26 further sites in the video sections — umddi:4473, 4483,
4611-4612, 4730-4731, 5096, 14166, 14178-14181, 14194-14197 among them. Those are DXGI *types*, not
a DXGI *DDI table*, and the absence claim survives them.

The struct comes from `dxgiddi.h`, which defines seven candidates (measured on win11,
`research/R1` §2.4, re-confirmed against `10.0.26100.0` this pass):

```
658: DXGI_DDI_BASE_FUNCTIONS        710: DXGI1_3_DDI_BASE_FUNCTIONS
670: DXGI1_1_DDI_BASE_FUNCTIONS     737: DXGI1_4_DDI_BASE_FUNCTIONS   (21 members = 168 B on x64)
685: DXGI1_2_DDI_BASE_FUNCTIONS     767: DXGI1_5_DDI_BASE_FUNCTIONS   (22 members = 176 B)
                                    798: DXGI1_6_1_DDI_BASE_FUNCTIONS (22 members = 176 B)
```

⭐ ✅ **ANSWERED (`D12-G5`), and the answer is that the question is moot: `D3D12DDI_TABLE_TYPE_DXGI`
is NEVER REQUESTED.** The spy armed 32 generic DXGI thunks and logged every `pfnFillDDITable` call
across four workloads — device-only, +queue, +swapchain with 20 flip-model presents, and +draw.
`TableType == 3` never appeared, and **0 of 32** DXGI thunks were ever called. Present reaches the
driver on the *command-list* table (`cl[19] pfnPresent`) and nowhere else.

⇒ **A D3D12 UMD on this Windows build needs no DXGI table at all**, and `helios_umd12` should not
plan one. (`DECISIONS.md` D2's remark that present arrives "on the command-list table plus
`D3D12DDI_TABLE_TYPE_DXGI` (=3)" is half right: the command-list half is what happens.)

⚠ Scope of the claim: four workloads on WARP, no fullscreen/exclusive transition, no stereo, no
HDR/colour-space call. If a later gate sees type 3 requested, the 168-vs-176 discriminator below is
still the way to identify the shape. ✅ `um\dxgiddi.h` **is** in this document's staging block (§preamble;
it is *not* in `DECISIONS.md`'s — that is the one deliberate difference between the two lists).
Member counts re-confirmed on win11 at `10.0.26100.0`: `dxgiddi:737` `DXGI1_4` = **21** members
(168 B), `dxgiddi:767` `DXGI1_5` = **22** (176 B), `dxgiddi:798` `DXGI1_6_1` = **22** (176 B) — so
the 168-vs-176 discriminator separates `DXGI1_4` from the two later shapes, and only those two need
the `pfnPresent1` tie-break.

Helios' D3D11 UMD already fills 18 DXGI slots across three installers
(`umd/src/forward/tables.rs:12`, `:23`, `:28`); those handlers are the strongest reuse candidate in
the whole port, and `docs/dx12/PRESENT.md` owns the design.

---

## 3. The device core table — `D3D12DDI_DEVICE_FUNCS_CORE_0109`

### 3.1 Version history

**33 versions**, 89 → 124 members, all recomputed:

| Struct | umddi | Members | | Struct | umddi | Members |
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
| `_0040` | 6660–6785 | **96** | | `_0102` | 12516–12678 | 122 |
| `_0043` | 6868–6993 | 96 | | `_0108` | 13133–13298 | **124** |
| `_0050` | 7030–7159 | 99 | | `_0109` | 13451–13616 | **124** |
| `_0052` | 7404–7541 | 105 | | | | |
| `_0054` | 8211–8358 | 114 | | | | |
| `_0062` | 8671–8820 | 115 | | | | |

⚠ **The two shape changes that matter.**

1. **`_0040`: command *allocator* → command *pool* + *recorder*.** `_0033` has
   `pfnCalcPrivateCommandAllocatorSize` / `pfnCreateCommandAllocator` /
   `pfnDestroyCommandAllocator` / `pfnResetCommandAllocator` (typedefs at umddi:1740-1743); `_0040`
   has `pfnCalcPrivateCommandPoolSize` / `pfnCreateCommandPool` / `pfnDestroyCommandPool` /
   `pfnResetCommandPool` **plus** `pfnCalcPrivateCommandRecorderSize` / `pfnCreateCommandRecorder` /
   `pfnDestroyCommandRecorder` / `pfnCommandRecorderSetCommandPoolAsTarget`. The allocator handle
   type `D3D12DDI_HCOMMANDALLOCATOR` (umddi:75) still exists but nothing in `_0109` creates one.
   ⛔ Do not port the 0003-era allocator functions.
2. **`_0090`: the caps convention changed.** The header states it verbatim (umddi:11122-11125):
   > "New options DDIs use a new NNNN version number and add new caps without inheriting the caps
   > from the previous version. This is done to avoid bloating one caps struct indefinitely, like
   > what happened with D3D12DDICAPS_TYPE_D3D12_OPTIONS. … The runtime will keep requesting from the
   > driver all D3D12DDI_OPTION versions whose caps it cares about."

   Consequence: `pfnGetCaps` is called **many** times with many `Type` values and the driver must
   answer each independently. There is no single "options" struct after `_0089`. See §11.

### 3.2 The 124 slots of `_0109`, functionally grouped

Names are verbatim from umddi:13451-13616; the group headings and counts are mine. Every slot must
be non-NULL (§14).

**(a) Format / capability queries at device scope — 3**
```
pfnCheckFormatSupport               VOID (HDEVICE, DXGI_FORMAT, _Out_ UINT*)                      umddi:2937
pfnCheckMultisampleQualityLevels    VOID (HDEVICE, DXGI_FORMAT, SampleCount, Flags, _Out_ UINT*)  umddi:2940
pfnGetMipPacking                    VOID (HDEVICE, hTiledResource, _Out_ UINT* pNumPackedMips,
                                                                   _Out_ UINT* pNumTilesForPackedMips)
```

**(b) Immutable pipeline sub-state objects — 12 (4 × Calc/Create/Destroy)**
```
pfnCalcPrivateElementLayoutSize      pfnCreateElementLayout      pfnDestroyElementLayout
pfnCalcPrivateBlendStateSize         pfnCreateBlendState         pfnDestroyBlendState
pfnCalcPrivateDepthStencilStateSize  pfnCreateDepthStencilState  pfnDestroyDepthStencilState
pfnCalcPrivateRasterizerStateSize    pfnCreateRasterizerState    pfnDestroyRasterizerState
```
Typedef revisions in `_0109`: element layout `_0010`, blend `_0010`, depth-stencil `_0095`,
rasterizer `_0102`. These four are the PSO's by-handle inputs (§9.9).

**(c) Shaders — 14**
```
pfnCalcPrivateShaderSize             pfnCreateVertexShader     pfnCreatePixelShader
pfnCreateGeometryShader              pfnCreateComputeShader
pfnCalcPrivateGeometryShaderWithStreamOutput   pfnCreateGeometryShaderWithStreamOutput
pfnCalcPrivateTessellationShaderSize pfnCreateHullShader       pfnCreateDomainShader
pfnDestroyShader
pfnCreateAmplificationShader         pfnCreateMeshShader       pfnCalcPrivateMeshShaderSize
```
All 14 names are **14 distinct slots**: eleven contiguous at umddi:13473-13486
(`pfnCalcPrivateShaderSize` … `pfnDestroyShader`) plus three at umddi:13608-13610
(`pfnCreateAmplificationShader`, `pfnCreateMeshShader`, `pfnCalcPrivateMeshShaderSize`), which sit
near the bottom of the struct because they were appended later. ⚠ In `_0109` **six** create-shader
slots share one
typedef `PFND3D12DDI_CREATE_SHADER_0026`, and `pfnCalcPrivateTessellationShaderSize` is the *same*
typedef as `pfnCalcPrivateShaderSize` (`PFND3D12DDI_CALC_PRIVATE_SHADER_SIZE_0026`). One Rust
`extern "system" fn` per *stage* is still required — the stage is not a parameter.

**(d) Command queues, pools, recorders, lists, signatures — 17**
```
pfnCalcPrivateCommandQueueSize / pfnCreateCommandQueue / pfnDestroyCommandQueue          (_0050)
pfnCalcPrivateCommandPoolSize / pfnCreateCommandPool / pfnDestroyCommandPool / pfnResetCommandPool  (_0040)
pfnCalcPrivateCommandListSize / pfnCreateCommandList / pfnDestroyCommandList             (_0040)
pfnCalcPrivateCommandRecorderSize / pfnCreateCommandRecorder / pfnDestroyCommandRecorder
pfnCommandRecorderSetCommandPoolAsTarget                                                 (_0040)
pfnCalcPrivateCommandSignatureSize / pfnCreateCommandSignature / pfnDestroyCommandSignature (_0001)
```

**(e) Pipeline state, libraries, root signatures — 12**
```
pfnCalcPrivatePipelineStateSize / pfnCreatePipelineState / pfnDestroyPipelineState        (_0099)
pfnCalcPrivateRootSignatureSize / pfnCreateRootSignature / pfnDestroyRootSignature        (_0100)
pfnCalcPrivatePipelineLibrarySize / pfnCreatePipelineLibrary / pfnDestroyPipelineLibrary  (_0010)
pfnAddPipelineStateToLibrary / pfnCalcSerializedLibrarySize / pfnSerializeLibrary         (_0010)
```

**(f) Descriptor heaps and views — 15**
```
pfnCalcPrivateDescriptorHeapSize / pfnCreateDescriptorHeap / pfnDestroyDescriptorHeap
pfnGetDescriptorSizeInBytes
pfnGetCPUDescriptorHandleForHeapStart      pfnGetGPUDescriptorHandleForHeapStart
pfnCreateShaderResourceView (_0002)        pfnCreateConstantBufferView
pfnCreateSampler (_0096)                   pfnCreateUnorderedAccessView (_0002)
pfnCreateRenderTargetView (_0002)          pfnCreateDepthStencilView
pfnCopyDescriptors (_0003)                 pfnCopyDescriptorsSimple (_0003)
pfnCreateSamplerFeedbackUnorderedAccessView (_0075)
```

**(g) Heaps, resources, residency — 11**
```
pfnMapHeap                          pfnUnmapHeap
pfnCalcPrivateHeapAndResourceSizes (_0109)   pfnCreateHeapAndResource (_0109)
pfnDestroyHeapAndResource
pfnCalcPrivateOpenedHeapAndResourceSizes (_0043)   pfnOpenHeapAndResource (_0043)
pfnMakeResident (_0001)             pfnEvict (PFND3D12DDI_EVICT2)
pfnOfferResources                   pfnReclaimResources (_0001)
```

**(h) Resource introspection — 5**
```
pfnCheckResourceVirtualAddress          -> D3D12DDI_GPU_VIRTUAL_ADDRESS   umddi:2476
pfnCheckResourceAllocationInfo (_0109)
pfnCheckSubresourceInfo
pfnCheckExistingResourceAllocationInfo (_0022)
pfnCheckResourceAllocationHandle        -> D3DKMT_HANDLE                  umddi:2992
```

**(i) Fences — 3** `pfnCalcPrivateFenceSize / pfnCreateFence / pfnDestroyFence`

**(j) Query heaps — 3** `pfnCalcPrivateQueryHeapSize / pfnCreateQueryHeap / pfnDestroyQueryHeap`

**(k) Multi-adapter and misc — 5**
```
pfnGetImplicitPhysicalAdapterMask   -> UINT (HDEVICE)                       umddi:2710
pfnQueryNodeMap                     VOID (HDEVICE, UINT NumPhysicalAdapters, _Out_writes_ UINT* pMap)
pfnGetPresentPrivateDriverDataSize  -> UINT (HDEVICE, CONST D3D12DDIARG_PRESENT_0001*)  umddi:1792
pfnRetrieveShaderComment (_0003)
pfnGetDebugAllocationInfo (_0014)
```

**(l) Scheduling groups (hardware scheduling) — 3**
`pfnCalcPrivateSchedulingGroupSize / pfnCreateSchedulingGroup / pfnDestroySchedulingGroup` (`_0050`)

**(m) Meta-commands — 6** (`_0052`)
`pfnEnumerateMetaCommands, pfnEnumerateMetaCommandParameters, pfnCalcPrivateMetaCommandSize,
pfnCreateMetaCommand, pfnDestroyMetaCommand, pfnGetMetaCommandRequiredParameterInfo`

**(n) State objects / raytracing / work graphs — 13**
```
pfnCalcPrivateStateObjectSize (_0054)   pfnCreateStateObject (_0054)   pfnDestroyStateObject
pfnGetRaytracingAccelerationStructurePrebuildInfo (_0054)
pfnCheckDriverMatchingIdentifier (_0054)
pfnGetShaderIdentifier / pfnGetShaderStackSize / pfnGetPipelineStackSize / pfnSetPipelineStackSize
pfnCalcPrivateAddToStateObjectSize (_0072)   pfnAddToStateObject (_0072)
pfnGetProgramIdentifier (_0108)         pfnGetWorkGraphMemoryRequirements (_0108)
```

**(o) Device policy — 2**
`pfnSetBackgroundProcessingMode (_0063)`, `pfnImplicitShaderCacheControl (_0080)`

Group counts, summed: 3 + 12 + **14** + **17** + 12 + 15 + 11 + 5 + 3 + 3 + 5 + 3 + 6 + 13 + 2 =
**124**. ⚠ Earlier revisions of this section printed (c) as 13 and (d) as 18; the two errors
cancelled, so the total was right while both rows were wrong. If you edit a group count, re-sum.

Aggregate shape of `_0109`: **26 `pfnCalcPrivate*`/`pfnCalc*`, 35 `pfnCreate*`, 20 `pfnDestroy*`, 43
other.**

---

## 4. The command-list table — `D3D12DDI_COMMAND_LIST_FUNCS_3D_0108`

### 4.1 Version history

**20 versions**, 51 → 75 members, recomputed:

| Struct | umddi | Members | | Struct | umddi | Members |
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

### 4.2 The 75 slots of `_0108`, grouped

Slot order is the header's; groups are mine.

| Group | Count | Slots |
|---|---|---|
| List lifetime | 2 | `pfnCloseCommandList`, `pfnResetCommandList` |
| Draw / dispatch | 3 | `pfnDrawInstanced`, `pfnDrawIndexedInstanced`, `pfnDispatch` |
| Clears / discard | 5 | `pfnClearUnorderedAccessViewUint`, `pfnClearUnorderedAccessViewFloat`, `pfnClearRenderTargetView`, `pfnClearDepthStencilView`, `pfnDiscardResource` |
| Copy / resolve | 7 | `pfnCopyTextureRegion`, `pfnResourceCopy`, `pfnCopyTiles`, `pfnCopyBufferRegion`, `pfnResourceResolveSubresource`, `pfnAtomicCopyBufferRegion`, `pfnResourceResolveSubresourceRegion` |
| Indirect / bundles | 2 | `pfnExecuteBundle`, `pfnExecuteIndirect` |
| Barriers | 2 | `pfnResourceBarrier` (`_0022`, legacy), `pfnBarrier` (`_0094`, enhanced) |
| Present / blt | 2 | `pfnBlt`, `pfnPresent` (`PFND3D12DDI_PRESENT_0051`) |
| Queries / predication | 4 | `pfnBeginQuery`, `pfnEndQuery`, `pfnResolveQueryData`, `pfnSetPredication` |
| Fixed-function state | 11 | `pfnIaSetTopology`, `pfnRsSetViewports`, `pfnRsSetScissorRects`, `pfnOmSetBlendFactor`, `pfnOmSetStencilRef`, `pfnSetPipelineState`, `pfnOMSetDepthBounds`, `pfnSetSamplePositions`, `pfnOmSetAlphaBlendFactor`, `pfnOmSetFrontAndBackStencilRef`, `pfnRSSetDepthBias` |
| Root arguments / descriptors | 16 | `pfnSetDescriptorHeaps`, `pfnSet{Compute,Graphics}RootSignature`, `pfnSet{Compute,Graphics}RootDescriptorTable`, `pfnSet{Compute,Graphics}Root32BitConstant`, `pfnSet{Compute,Graphics}Root32BitConstants`, `pfnSet{Compute,Graphics}RootConstantBufferView`, `pfnSet{Compute,Graphics}RootShaderResourceView`, `pfnSet{Compute,Graphics}RootUnorderedAccessView`, `pfnClearRootArguments` |
| IA / SO / OM binding | 5 | `pfnIASetIndexBuffer`, `pfnIASetVertexBuffers`, `pfnSOSetTargets`, `pfnOMSetRenderTargets`, `pfnIASetIndexBufferStripCutValue` |
| Markers / protection / immediates / view instancing | 4 | `pfnSetMarker`, `pfnSetProtectedResourceSession`, `pfnWriteBufferImmediate`, `pfnSetViewInstanceMask` |
| Meta-commands | 2 | `pfnInitializeMetaCommand`, `pfnExecuteMetaCommand` |
| Raytracing | 5 | `pfnBuildRaytracingAccelerationStructure`, `pfnEmitRaytracingAccelerationStructurePostbuildInfo`, `pfnCopyRaytracingAccelerationStructure`, `pfnSetPipelineState1`, `pfnDispatchRays` |
| VRS | 2 | `pfnRSSetShadingRate`, `pfnRSSetShadingRateImage` |
| Mesh shaders | 1 | `pfnDispatchMesh` |
| Work graphs | 2 | `pfnSetProgram`, `pfnDispatchGraph` |
| **total** | **75** | |

Note **both** barrier generations are present in one table. Which one the runtime uses is selected
by the `EnhancedBarriersSupported` cap (§11.5), not by the table shape — so a driver that reports
`FALSE` must still leave `pfnBarrier` non-NULL.

⚠ **`pfnPresent` and `pfnBlt` live on the COMMAND LIST**, not on the DXGI table. That is
`DECISIONS.md` P-C's structural fact; §13 gives the signature.

Sample signatures, verbatim, for the shapes that are least obvious (umddi:1750, 1751-1756, 1767,
1769 — these are *typedef* lines, not struct-member lines; §5 explains why this document keeps the
two conventions labelled):

```c
typedef VOID ( APIENTRY* PFND3D12DDI_CLOSECOMMANDLIST )( D3D12DDI_HCOMMANDLIST );
typedef VOID ( APIENTRY* PFND3D12DDI_DRAWINSTANCED )( D3D12DDI_HCOMMANDLIST, UINT, UINT, UINT, UINT );
typedef VOID ( APIENTRY* PFND3D12DDI_DRAWINDEXEDINSTANCED )( D3D12DDI_HCOMMANDLIST, UINT, UINT, UINT, INT, UINT );
typedef VOID ( APIENTRY* PFND3D12DDI_RESOURCECOPY )( D3D12DDI_HCOMMANDLIST, D3D12DDI_HRESOURCE, D3D12DDI_HRESOURCE );
typedef VOID ( APIENTRY* PFND3D12DDI_SET_PIPELINE_STATE )( D3D12DDI_HCOMMANDLIST, D3D12DDI_HPIPELINESTATE );
typedef VOID ( APIENTRY* PFND3D12DDI_EXECUTE_BUNDLE )( D3D12DDI_HCOMMANDLIST, D3D12DDI_HCOMMANDLIST );
```

**Every command-list DDI returns `VOID`.** There is no error return anywhere on this table. Errors
go out through `pfnSetCommandListErrorCb(D3D12DDI_HRTCOMMANDLIST, HRESULT)` (umddi:2585) — see §7.2
and `DECISIONS.md` §7.6.

---

## 5. The command-queue table — `D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001`

**One version, 7 slots, unchanged across 30 DDI revisions.** Verbatim, umddi:2729-2738:

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

⚠ **The `umddi` column below cites the STRUCT-MEMBER line, consistently** — the line inside
`D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001` at umddi:2729-2738 where the slot is declared. The
function-pointer *typedef* lives elsewhere and is given separately in the Notes column, because
mixing the two conventions in one column is how the previous revision of this table came to cite
`pfnSignalFence` at 2718 (a blank line) and `pfnWaitForFence` at 2719 (which is the
`PFND3D12DDI_SIGNAL_FENCE` typedef, i.e. the *other* function).

| Slot | Signature | umddi (member) | Notes |
|---|---|---|---|
| `pfnExecuteCommandLists` | `VOID (HCOMMANDQUEUE, UINT Count, _In_reads_(Count) CONST D3D12DDI_HCOMMANDLIST*)` | 2731 | typedef `PFND3D12DDI_EXECUTECOMMANDLISTS` at umddi:1735-1739. The **only** submission entry point in the baseline set |
| `pfnUnused` | `void*` | 2732 | named unused — leave `null`, or write a counting stub; either is safe |
| `pfnUnused2` | `void*` | 2733 | same |
| `pfnUpdateTileMappings` | `PFND3D12DDI_UPDATETILEMAPPINGS` | 2734 | typedef at umddi:1852. Reserved-resource tile remap, *immediate* (not recorded) |
| `pfnCopyTileMappings` | `PFND3D12DDI_COPYTILEMAPPINGS` | 2735 | typedef at umddi:1861. Ditto |
| `pfnSignalFence` | `void (HCOMMANDQUEUE, D3D12DDIARG_FENCE_OPERATION*)` | 2736 | typedef `PFND3D12DDI_SIGNAL_FENCE` at umddi:2719; arg struct umddi:2712-2717. §10 |
| `pfnWaitForFence` | `void (HCOMMANDQUEUE, D3D12DDIARG_FENCE_OPERATION*)` | 2737 | typedef `PFND3D12DDI_WAIT_FOR_FENCE` at umddi:2720. §10 |

⚠ **`pfnUnused`/`pfnUnused2` are named unused, but that is the *header's* claim, not a measured
fact.** The runtime never calls them as far as any evidence here shows. Fill them with a counting
stub anyway: a stub costs nothing and turns "the header lied" into a counter instead of a jump
through a null pointer.

⚠ **`pfnCopyTiles` (the command-list slot) is explicitly NULL-checked by the runtime**:
`Driver set pfnCopyTiles to NULL.` (strings:3). No such string exists for the queue-side tile
functions, but assume the same and fill them.

---

## 6. Runtime→driver callbacks — three tables, three scopes

A D3D12 UMD consumes **three** callback surfaces. Getting the split right decides which operations
need a corelayer wrapper and which go straight to D3DKMT.

### 6.1 Adapter scope — `D3DDDI_ADAPTERCALLBACKS`, 3 members

Handed in on `D3D12DDIARG_OPENADAPTER::pAdapterCallbacks`. Identical to what
`helios_umd.dll` receives today. Quoted in §1.2.

### 6.2 Device scope, usermode — `D3D12DDI_CORELAYER_DEVICECALLBACKS_*`

Four versions. **Slot counts, recounted with all version gates ON: 12 / 14 / 17 / 18.**

| Struct | umddi | Slots | Adds |
|---|---|---|---|
| `_0003` | 2624–2653 | **12** | baseline |
| `_0022` | 4874–4905 | **14** | `pfnAllocateCb`, `pfnDeallocateCb` |
| `_0050` | 7178–7218 | **17** | `pfnCreateSchedulingGroupContextCb`, `pfnCreateSchedulingGroupContextVirtualCb`, `pfnCreateHwQueueCb` |
| `_0062` | 8606–8647 | **18** | `pfnQueueBackgroundProcessingWorkCb` |

`_0062` verbatim, umddi:8606-8647 — note the `#if`/`#else` structure, which is the whole point:

```c
typedef struct D3D12DDI_CORELAYER_DEVICECALLBACKS_0062
{
    PFND3D12DDI_SETERROR_CB pfnSetErrorCb;
    PFND3D12DDI_SETCOMMANDLISTERROR_CB pfnSetCommandListErrorCb;
    PFND3D12DDI_SETCOMMANDLISTDDITABLE_CB pfnSetCommandListDDITableCb;

    // KM callbacks for 12
    PFND3D12DDI_CREATECONTEXT_CB        pfnCreateContextCb;
#if D3D_UMD_INTERFACE_VERSION >= D3D_UMD_INTERFACE_VERSION_WDDM2_0
    PFND3D12DDI_CREATECONTEXTVIRTUAL_CB pfnCreateContextVirtualCb;
#else
    void*                               pfnReservedCreateContextVirtualCb;
#endif
    PFND3D12DDI_DESTROYCONTEXT_CB       pfnDestroyContextCb;
#if D3D_UMD_INTERFACE_VERSION >= D3D_UMD_INTERFACE_VERSION_WDDM2_0
    PFND3D12DDI_CREATEPAGINGQUEUE_CB    pfnCreatePagingQueueCb;
    PFND3D12DDI_DESTROYPAGINGQUEUE_CB   pfnDestroyPagingQueueCb;
    PFND3D12DDI_MAKERESIDENT_CB         pfnMakeResidentCb;
    PFND3D12DDI_EVICT_CB                pfnEvictCb;
    PFND3D12DDI_RECLAIMALLOCATIONS2_CB  pfnReclaimAllocations2Cb;
    PFND3D12DDI_OFFERALLOCATIONS_CB     pfnOfferAllocationsCb;
#else
    void*                               pfnReservedCreatePagingQueueCb;
    void*                               pfnReservedDestroyPagingQueueCb;
    void*                               pfnReservedMakeResidentCb;
    void*                               pfnReservedEvictCb;
    void*                               pfnReservedReclaimAllocations2Cb;
    void*                               pfnReservedOfferAllocationsCb;
#endif
    PFND3D12DDI_ALLOCATE_CB_0022        pfnAllocateCb;
    PFND3D12DDI_DEALLOCATE_CB_0022      pfnDeallocateCb;
#if D3D_UMD_INTERFACE_VERSION >= D3D_UMD_INTERFACE_VERSION_WDDM2_5
    PFND3D12DDI_CREATESCHEDULINGGROUPCONTEXT_CB_0050        pfnCreateSchedulingGroupContextCb;
    PFND3D12DDI_CREATESCHEDULINGGROUPCONTEXTVIRTUAL_CB_0050 pfnCreateSchedulingGroupContextVirtualCb;
    PFND3D12DDI_CREATEHWQUEUE_CB_0050                       pfnCreateHwQueueCb;
#else
    void*                               pfnReservedCreateSchedulingGroupContextCb;
    void*                               pfnReservedCreateSchedulingGroupContextVirtualCb;
    void*                               pfnReservedCreateHwQueueCb;
#endif
    PFND3D12DDI_QUEUEPROCESSINGWORK_CB_0062     pfnQueueBackgroundProcessingWorkCb;
} D3D12DDI_CORELAYER_DEVICECALLBACKS_0062;
```

**The ABI-stability property, and why it matters for bindgen.** Every version-gated member has a
same-offset `void* pfnReserved…` alternate in the `#else` arm. **The struct layout is therefore
independent of `D3D_UMD_INTERFACE_VERSION`; only the pointer *types* change.** So:

- One bindgen layout suffices — no per-WDDM variants of the Rust struct.
- Set `D3D_UMD_INTERFACE_VERSION` high enough in `build.rs` to get useful function-pointer types
  rather than `void*` (the existing D3D11 UMD bindgen already does this for `d3d10umddi.h`).
- ⚠ It does **not** mean a version-gated slot is populated. `pfnCreateHwQueueCb` being present in
  the layout says nothing about whether the runtime filled it. Null-check before every use.

Callback signatures worth pinning, verbatim:

```c
typedef VOID (APIENTRY CALLBACK *PFND3D12DDI_SETERROR_CB)( D3D10DDI_HRTDEVICE, HRESULT );          // umddi:2602
typedef VOID (APIENTRY CALLBACK *PFND3D12DDI_SETCOMMANDLISTERROR_CB)( D3D12DDI_HRTCOMMANDLIST, HRESULT ); // umddi:2585
typedef _Check_return_ HRESULT(APIENTRY CALLBACK *PFND3D12DDI_CREATECONTEXT_CB)(
    _In_    D3D12DDI_HRTCOMMANDQUEUE hRTCommandQueue,      // <-- a QUEUE handle, not a device handle
    _Inout_ D3DDDICB_CREATECONTEXT* );                                                             // umddi:2556
```

The two allocation shapes (umddi:4828-4849):

```c
typedef struct D3D12DDI_ALLOCATION_INFO_0022
{
    D3DKMT_HANDLE hAllocation;   CONST VOID* pSystemMem;
    VOID* pPrivateDriverData;    UINT PrivateDriverDataSize;
    D3DDDI_VIDEO_PRESENT_SOURCE_ID VidPnSourceId;
    D3D12DDI_ALLOCATION_INFO_FLAGS_0022 Flags;
    D3DGPU_VIRTUAL_ADDRESS GpuVirtualAddress;      // <-- the KMD-assigned GPU VA comes back here
    UINT Priority;               ULONG_PTR Reserved[5];
} D3D12DDI_ALLOCATION_INFO_0022;

typedef struct D3D12DDICB_ALLOCATE_0022
{
    CONST VOID* pPrivateDriverData;  UINT PrivateDriverDataSize;
    HANDLE hResource;                D3DKMT_HANDLE hKMResource;
    UINT NumAllocations;             D3D12DDI_ALLOCATION_INFO_0022* pAllocationInfo;
} D3D12DDICB_ALLOCATE_0022;
```

⚠ `Reserved fields in D3D12DDI_ALLOCATION_INFO_0022 were not zero.` (strings:251) — zero the whole
struct before filling it.

### 6.3 Device scope, kernel — the full `D3DDDI_DEVICECALLBACKS` via `pKTCallbacks`

`D3D12DDIARG_CREATEDEVICE_*::pKTCallbacks` is **the same `D3DDDI_DEVICECALLBACKS` a D3D11 UMD
receives** — `d3dumddi.h` (staged) 4499-4586, **65 members** with all version gates on (measured;
there are zero `#else` arms in that range, so 65 is the count at every WDDM level ≥ 2.6.4):

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

**So a D3D12 UMD reaches the kernel through the identical thunk set `helios_umd.dll` already
drives.** The bindgen'd struct is already in the tree
(`umd/target/release/build/helios_umd-*/out/d3d10umddi.rs`).

### 6.4 The split that matters: corelayer vs raw D3DKMT

| Operation | Corelayer wrapper? | Handle it takes | Why |
|---|---|---|---|
| Create WDDM context | **yes** — `pfnCreateContextCb` / `pfnCreateContextVirtualCb` | `D3D12DDI_HRTCOMMANDQUEUE` | the runtime must associate the context with the *queue* (for scheduling, and for `pfnPresent`'s `hContext` output) |
| Create scheduling-group context / HW queue | **yes** — `pfnCreateSchedulingGroupContextCb`, `…VirtualCb`, `pfnCreateHwQueueCb` | `HRTCOMMANDQUEUE` / `HRTSCHEDULINGGROUP` | same |
| Create paging queue | **yes** — `pfnCreatePagingQueueCb` | `HRTCOMMANDQUEUE` | |
| MakeResident / Evict / Offer / Reclaim | **yes** — `pfnMakeResidentCb`, `pfnEvictCb`, `pfnOfferAllocationsCb`, `pfnReclaimAllocations2Cb` | `HRTDEVICE` + `HRTPAGINGQUEUE` | |
| Allocate / Deallocate | **yes** — `pfnAllocateCb`, `pfnDeallocateCb` (`_0022`) | `HRTDEVICE` | mints `D3DKMT_HANDLE`s |
| **GPU VA reserve / map / free / update** | **NO wrapper** | device | `pKTCallbacks->pfnReserveGpuVirtualAddressCb` etc. — straight to D3DKMT |
| **Submission** | **NO wrapper** | device | `pKTCallbacks->pfnSubmitCommandCb` (GPU-VA contexts) or `pfnRenderCb` (legacy) or the HwQueue family |
| **Sync objects** | **NO wrapper** | device | `pKTCallbacks->pfnCreateSynchronizationObject2Cb`, `pfnSignalSynchronizationObjectFromGpuCb`, `…FromCpuCb`, `…FromGpu2Cb` |
| **Escape** | **NO wrapper** | device | `pKTCallbacks->pfnEscapeCb` — the same door the ICD's venus submit already uses |
| **Present** | **NO wrapper** | device | `pKTCallbacks->pfnPresentCb` / `pfnRenderCb`; see §13 and `docs/dx12/PRESENT.md` |

⚠ **The runtime enforces the queue scoping**:

> `CreateContextCb or CreateContextVirtualCb called outside of queue creation.` — fullstrings:10597
> `Driver is not allowed to create a global Hw queue for a context which is owned by a command queue or scheduling group.` — strings:53
> `Driver targeted HwQueue against context belonging to different queue.` — strings:109
> `Driver targeted HwQueue against scheduling group that this command queue does not belong to.` — strings:110
> `Reserved flags given to CreateContextCb or CreateContextVirtualCb` — fullstrings:23010

**Contract, verified:** create the WDDM context **inside `pfnCreateCommandQueue`, one per
`ID3D12CommandQueue`**, via the corelayer. Helios' D3D11 UMD already does the device-scoped
equivalent — `umd/src/device_funcs.rs:1046` `create_runtime_context()` calls `pfnCreateContextCb`
with `NodeOrdinal = 0, EngineAffinity = 0` and stores the returned command-buffer / allocation-list
/ patch-list windows, and `umd/src/device_funcs.rs:1101` `create_runtime_paging_queue()` does the
paging queue. **The D3D12 change is cardinality (per queue, not per device), not kind — port both
functions and move the call site.**

Broadcast/submit validation the runtime performs on what the *driver* wrote:

> `D3DDDICB_SUBMITCOMMAND::NumPrimaries is too large. Only half the available array may be used by driver.` — strings:18
> `D3DDDICB_SUBMITCOMMAND::BroadcastContextCount is too large.` — strings:17
> `D3DDDICB_SUBMITCOMMAND::BroadcastContext array must contain contexts that are all associated with the same command queue.` — strings:16
> `D3DDDICB_SUBMITCOMMAND::BroadcastContext array may not contain contexts that belong to a scheduling group.` — strings:15
> `D3DDDICB_RENDER::BroadcastContext array must contain contexts that are all associated with the same command queue.` — strings:14
> `D3DDDICB_RENDER::BroadcastContext array may not contain contexts that belong to a scheduling group.` — strings:13

Single-adapter Helios writes `BroadcastContextCount = 0` and one context; those six strings are the
proof that the *driver* fills the submit structure, i.e. the UMD is the submitting party.

---

## 7. The object model

### 7.1 The `CalcPrivateXxxSize` + `CreateXxx` pattern — identical to D3D11

1. Runtime calls `pfnCalcPrivate<X>Size(hDevice, pArgs) -> SIZE_T`.
2. Runtime allocates that many bytes and hands the driver an opaque handle — a struct with one
   `void* pDrvPrivate` pointing at that buffer.
3. Runtime calls `pfnCreate<X>(hDevice, pArgs, hDrv<X> [, hRT<X>])`.
4. Runtime calls `pfnDestroy<X>(hDevice, hDrv<X>)` and frees the memory itself.

**Every D3D12 handle type is either a D3D10 handle typedef or made by the same macros** (umddi:23-34
and 65-90 — the block runs from `D3D10DDI_HRT( D3D12DDI_HRTCOMMANDLIST )` at :65 through
`D3D10DDI_H( D3D12DDI_HSTATEOBJECT_0054 )` at :**90**, the sixteenth UMD handle type):

```c
typedef D3D10DDI_HSHADER            D3D12DDI_HSHADER;
typedef D3D10DDI_HDEVICE            D3D12DDI_HDEVICE;
typedef D3D10DDI_HRESOURCE          D3D12DDI_HRESOURCE;
typedef D3D10DDI_HBLENDSTATE        D3D12DDI_HBLENDSTATE;
typedef D3D10DDI_HRASTERIZERSTATE   D3D12DDI_HRASTERIZERSTATE;
typedef D3D10DDI_HDEPTHSTENCILSTATE D3D12DDI_HDEPTHSTENCILSTATE;
typedef D3D10DDI_HELEMENTLAYOUT     D3D12DDI_HELEMENTLAYOUT;
typedef D3D10DDI_HADAPTER           D3D12DDI_HADAPTER;
typedef D3D10DDI_HKMRESOURCE        D3D12DDI_HKMRESOURCE;
typedef D3D10DDI_HRTRESOURCE        D3D12DDI_HRTRESOURCE;
typedef D3D10DDI_HRTDEVICE          D3D12DDI_HRTDEVICE;
typedef D3D10DDI_HRTADAPTER         D3D12DDI_HRTADAPTER;

// Runtime handle types (8)
D3D10DDI_HRT( D3D12DDI_HRTCOMMANDLIST )   D3D10DDI_HRT( D3D12DDI_HRTTABLE )
D3D10DDI_HRT( D3D12DDI_HRTCOMMANDQUEUE )  D3D10DDI_HRT( D3D12DDI_HRTPAGINGQUEUE )
D3D10DDI_HRT( D3D12DDI_HRTPIPELINESTATE ) D3D10DDI_HRT( D3D12DDI_HRTSCHEDULINGGROUP_0050 )
D3D10DDI_HRT( D3D12DDI_HRTMETACOMMAND_0052 ) D3D10DDI_HRT( D3D12DDI_HRTSTATEOBJECT_0054 )

// UMD handle types (16)
D3D10DDI_H( D3D12DDI_HCOMMANDQUEUE )       D3D10DDI_H( D3D12DDI_HCOMMANDALLOCATOR )
D3D10DDI_H( D3D12DDI_HPIPELINESTATE )      D3D10DDI_H( D3D12DDI_HCOMMANDLIST )
D3D10DDI_H( D3D12DDI_HFENCE )              D3D10DDI_H( D3D12DDI_HDESCRIPTORHEAP )
D3D10DDI_H( D3D12DDI_HQUERYHEAP )          D3D10DDI_H( D3D12DDI_HCOMMANDSIGNATURE )
D3D10DDI_H( D3D12DDI_HHEAP )               D3D10DDI_H( D3D12DDI_HUNORDEREDACCESSVIEWCOUNTER )
D3D10DDI_H( D3D12DDI_HROOTSIGNATURE )      D3D10DDI_H( D3D12DDI_HCOMMANDRECORDER_0040 )
D3D10DDI_H( D3D12DDI_HCOMMANDPOOL_0040 )   D3D10DDI_H( D3D12DDI_HSCHEDULINGGROUP_0050 )
D3D10DDI_H( D3D12DDI_HMETACOMMAND_0052 )   D3D10DDI_H( D3D12DDI_HSTATEOBJECT_0054 )
```

(+ `D3D12DDI_HPROTECTEDRESOURCESESSION_0030` at umddi:5922 and
`D3D12DDI_HRTPROTECTEDSESSION_0030` at umddi:13688 for the content-protection feature.)

⭐ **`umd/src/forward/handles.rs` ports to D3D12 nearly unchanged, and this is the single
highest-value reuse in the port.** That module's whole design — `Slot<P>` as a non-null tagged
pointer into the runtime's `pDrvPrivate` word, with `Com<T>` for "the word is an owning COM
pointer" and `Boxed<S>` for "the word is a `Box<S>` this driver allocated", and the `*mut c_void` →
payload cast confined to that one module — is exactly the D3D12 model too. What to copy and what to
change:

- **Copy verbatim:** `Slot<P>`, `Com<T>`, `Boxed<S>`, the `ComHandle` / `BoxedHandle` marker traits,
  the `*_at` runtime-tagged accessors, and the doc comment explaining why the discriminator must be
  the handle *type* rather than the call site (`umd/src/forward/handles.rs:1-66`).
- **Change:** the `impl ComHandle for …` / `impl BoxedHandle for …` list. In D3D12 the *majority*
  of handles are `Boxed` — a queue, a list, a PSO, a root signature and a resource each need a
  shadow struct (§9), not a bare COM pointer. Only a few (`HFENCE`?, `HDESCRIPTORHEAP`) can be bare
  COM, and even those want a shadow for the fence GPU VAs.
- **Do not change:** the invariant text at `umd/src/forward/handles.rs:62-66` ("that the runtime
  passed a slot which the matching `CalcPrivate*Size` sized … remain preconditions of construction
  that no type can witness"). That is true of D3D12 verbatim.

`D3D12DDI_HANDLETYPE` (umddi:330-363) enumerates **28** live object classes, from
`D3D12DDI_HT_COMMAND_QUEUE = 19` to `D3D12DDI_HT_0080_VIDEO_ENCODER_HEAP = 49` (values 26, 31, 33
are absent). It is used by `D3D12DDI_HANDLE_AND_TYPE` (umddi:365) for `pfnGetDebugAllocationInfo`
and `pfnSerializeObject`, and it is the runtime-tag dispatch the `*_at` accessors exist for.

### 7.2 Every `CalcPrivate*` in `CORE_0109` — 26

```
pfnCalcPrivateElementLayoutSize          pfnCalcPrivateBlendStateSize
pfnCalcPrivateDepthStencilStateSize      pfnCalcPrivateRasterizerStateSize
pfnCalcPrivateShaderSize                 pfnCalcPrivateGeometryShaderWithStreamOutput
pfnCalcPrivateTessellationShaderSize     pfnCalcPrivateMeshShaderSize
pfnCalcPrivateCommandQueueSize           pfnCalcPrivateCommandPoolSize
pfnCalcPrivatePipelineStateSize          pfnCalcPrivateCommandListSize
pfnCalcPrivateFenceSize                  pfnCalcPrivateDescriptorHeapSize
pfnCalcPrivateRootSignatureSize          pfnCalcPrivateHeapAndResourceSizes
pfnCalcPrivateOpenedHeapAndResourceSizes pfnCalcPrivateQueryHeapSize
pfnCalcPrivateCommandSignatureSize       pfnCalcPrivatePipelineLibrarySize
pfnCalcSerializedLibrarySize             pfnCalcPrivateCommandRecorderSize
pfnCalcPrivateSchedulingGroupSize        pfnCalcPrivateMetaCommandSize
pfnCalcPrivateStateObjectSize            pfnCalcPrivateAddToStateObjectSize
```
(+ `pfnCalcPrivateDeviceSize` on the adapter table = 27 sizing entry points in total.)

### 7.3 The four ways D3D12 differs from the D3D11 pattern

**(1) Two objects, one sizing call.** `pfnCalcPrivateHeapAndResourceSizes` returns a *struct of two
sizes*, not a `SIZE_T` (umddi:556-560, 13443-13445):

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

⚠ Both `_In_opt_` arguments are how D3D12's three resource shapes collapse into one entry point:

| heap arg | resource arg | Public API |
|---|---|---|
| non-NULL | non-NULL | `CreateCommittedResource` |
| non-NULL | NULL | `CreateHeap` |
| NULL | non-NULL | `CreatePlacedResource` / `CreateReservedResource` |
| NULL | NULL | — illegal; refuse and count |

**The NULL combinations ARE the arm structure**, and CLAUDE.md's "validate every runtime-supplied
length per-arm, not max-union" applies literally: the RenderGdi ~48 % drop bug was exactly this
mistake in D3D11.

⚠ **This function returns a two-word struct by value.** On MSVC x64 a 16-byte POD is returned via a
hidden pointer, not in `RAX:RDX`. Get the Rust `extern "system"` signature from bindgen; do not
hand-write it. Same family as (4) below.

**(2) Mixed return conventions.** Some creates return `VOID` and report via `pfnSetErrorCb`
(`pfnCreateElementLayout`, `pfnCreateBlendState`, `pfnCreateDepthStencilState`,
`pfnCreateRasterizerState`, the whole `pfnCreate*Shader` family — `PFND3D12DDI_CREATE_SHADER_0026`
is `VOID`, umddi:5565); others return `HRESULT` directly (`pfnCreateFence`, `pfnCreateCommandList`,
`pfnCreateCommandQueue`, `pfnCreateCommandPool`, `pfnCreateCommandRecorder`,
`pfnCreateRootSignature`, `pfnCreateHeapAndResource`, `pfnCreatePipelineState`,
`pfnCreateDescriptorHeap`). **The D3D11 DDI is uniformly `VOID` + `SetErrorCb`.**

⚠ Getting this wrong on a `VOID`-returning slot means the caller reads a garbage register as an
`HRESULT`. That is memory `t7-umd-crash-fixed-52nd.md` exactly (`bridge_guard` deduced `R=int` from
a bare `0` and truncated `size_t` returns). ⛔ Never write a D3D12 slot signature by hand.

**(3) Some creates take BOTH handles.** `pfnCreateCommandList(hDevice, pArgs, D3D12DDI_HCOMMANDLIST
hDrv, D3D12DDI_HRTCOMMANDLIST hRT)` (umddi:6625); `pfnCreateCommandQueue(hDevice, pArgs,
D3D12DDI_HCOMMANDQUEUE hDrv, D3D12DDI_HRTCOMMANDQUEUE hRT)` (umddi:7028);
`pfnCreatePipelineState(…, HPIPELINESTATE, HRTPIPELINESTATE)` (umddi:11981);
`pfnCreateSchedulingGroup(…, HSCHEDULINGGROUP_0050, HRTSCHEDULINGGROUP_0050)` (umddi:7014). **The
`hRT` must be stored** — it is the token every callback about that object takes
(`pfnCreateContextCb` takes `HRTCOMMANDQUEUE`, `pfnSetCommandListErrorCb` takes `HRTCOMMANDLIST`).

**(4) `pfnDestroyDevice` is on the ADAPTER table** (umddi:2622, 13649), not the device table.

For reference, `D3D12DDIARG_CREATEHEAP_0001` (umddi:319-328):

```c
typedef struct D3D12DDIARG_CREATEHEAP_0001 { UINT64 ByteSize; UINT64 Alignment;
    D3D12DDI_MEMORY_POOL MemoryPool; D3D12DDI_CPU_PAGE_PROPERTY CPUPageProperty;
    D3D12DDI_HEAP_FLAGS Flags; UINT CreationNodeMask; UINT VisibleNodeMask; } D3D12DDIARG_CREATEHEAP_0001;
```
with `D3D12DDI_MEMORY_POOL { L0 = 0 /*Always system memory*/, L1 = 1 /*Typically local video memory*/ }`
(umddi:301) and `D3D12DDI_HEAP_FLAGS` (umddi:307) `NONE=0x0, NON_RT_DS_TEXTURES=0x2, BUFFERS=0x4,
COHERENT_SYSTEMWIDE=0x8, PRIMARY=0x10, RT_DS_TEXTURES=0x20, _0041_DENY_L0_DEMOTION=0x40`.

---

## 8. Command recording and submission — the model

### 8.1 Four objects, not two

D3D12's public API has *command allocator* + *command list*. The DDI at ≥ `_0040` has **four**:

| DDI object | Handle | Created by | Public-API analogue |
|---|---|---|---|
| Command **pool** | `D3D12DDI_HCOMMANDPOOL_0040` | `pfnCreateCommandPool` | the backing store of an allocator |
| Command **recorder** | `D3D12DDI_HCOMMANDRECORDER_0040` | `pfnCreateCommandRecorder` | the recording engine |
| Command **list** | `D3D12DDI_HCOMMANDLIST` | `pfnCreateCommandList` | `ID3D12GraphicsCommandList` |
| Command **allocator** | `D3D12DDI_HCOMMANDALLOCATOR` | *nothing in `_0109`* — retired at `_0040` | superseded |

Verbatim (umddi:6538-6545, 6615-6658):

```c
typedef enum D3D12DDI_COMMAND_POOL_FLAGS { D3D12DDI_COMMAND_POOL_FLAG_NONE = 0x00000000 } D3D12DDI_COMMAND_POOL_FLAGS;
typedef struct D3D12DDIARG_CREATE_COMMAND_POOL_0040 { D3D12DDI_COMMAND_POOL_FLAGS PoolFlags; } D3D12DDIARG_CREATE_COMMAND_POOL_0040;
typedef SIZE_T  (…* PFND3D12DDI_CALC_PRIVATE_COMMAND_POOL_SIZE_0040)(HDEVICE, CONST D3D12DDIARG_CREATE_COMMAND_POOL_0040*);
typedef HRESULT (…* PFND3D12DDI_CREATE_COMMAND_POOL_0040)(HDEVICE, CONST …*, D3D12DDI_HCOMMANDPOOL_0040);
typedef VOID    (…* PFND3D12DDI_DESTROY_COMMAND_POOL_0040)(HDEVICE, D3D12DDI_HCOMMANDPOOL_0040);
typedef VOID    (…* PFND3D12DDI_RESET_COMMAND_POOL_0040)(HDEVICE, D3D12DDI_HCOMMANDPOOL_0040);

typedef enum D3D12DDI_COMMAND_RECORDER_FLAGS { D3D12DDI_COMMAND_RECORDER_FLAG_NONE = 0x00000000 } D3D12DDI_COMMAND_RECORDER_FLAGS;
typedef struct D3D12DDIARG_CREATE_COMMAND_RECORDER_0040 {
    D3D12DDI_COMMAND_QUEUE_FLAGS QueueFlags; D3D12DDI_COMMAND_RECORDER_FLAGS RecorderFlags; } …;
typedef VOID (…* PFND3D12DDI_COMMAND_RECORDER_SET_COMMAND_POOL_AS_TARGET_0040)(
    HDEVICE, D3D12DDI_HCOMMANDRECORDER_0040, D3D12DDI_HCOMMANDPOOL_0040);

typedef struct D3D12DDIARG_CREATE_COMMAND_LIST_0040
{
    D3D12DDI_COMMAND_LIST_TYPE   Type;          // DIRECT = 0, BUNDLE = 1        (umddi:1425-1429)
    D3D12DDI_COMMAND_QUEUE_FLAGS QueueFlags;    // 3D / COMPUTE / COPY / …       (umddi:1435-1447)
    UINT64                       ID;
    D3D12DDI_COMMAND_LIST_FLAGS  CommandListFlags;
    UINT                         NodeMask;
} D3D12DDIARG_CREATE_COMMAND_LIST_0040;
typedef HRESULT (…* PFND3D12DDI_CREATE_COMMAND_LIST_0040)(HDEVICE, CONST …*, D3D12DDI_HCOMMANDLIST, D3D12DDI_HRTCOMMANDLIST);

typedef struct D3D12DDIARG_RESETCOMMANDLIST_0040
{
    D3D12DDI_HCOMMANDRECORDER_0040   hDrvCommandRecorder;
    UINT64                           ID;
    D3D12DDI_COMMAND_LIST_FLAGS      CommandListFlags;
} D3D12DDIARG_RESETCOMMANDLIST_0040;
typedef VOID (…* PFND3D12DDI_RESETCOMMANDLIST_0040)(D3D12DDI_HCOMMANDLIST, CONST D3D12DDIARG_RESETCOMMANDLIST_0040*);
```

⚠ **The list *type* is only `DIRECT` or `BUNDLE`.** COMPUTE and COPY are expressed through
`D3D12DDI_COMMAND_QUEUE_FLAGS` on the list's create args, not by a list type (umddi:1435-1448):

```c
D3D12DDI_COMMAND_QUEUE_FLAG_NONE = 0x0, _3D = 0x1, _COMPUTE = 0x2, _COPY = 0x4, _PAGING = 0x8,
_0020_VIDEO_LEGACY = 0x10 /*Deprecated*/, _0022_VIDEO_DECODE = 0x10,
_0022_VIDEO_PROCESS = 0x20, _0053_VIDEO_ENCODE = 0x40
```

At DDI `0003` the older shape was `pfnCreateCommandAllocator` / `pfnResetCommandAllocator`
(umddi:1740-1743) with `D3D12DDIARG_RESETCOMMANDLIST { hDrvCommandAllocator; UINT Slot; UINT64 ID; }`
(umddi:798-804). The `_0040` refactor replaced *allocator + Slot* with *recorder + pool*.

### 8.2 ⚠ There is NO DMA buffer in the D3D12 DDI — and no `pfnRenderCb` *in `d3d12umddi.h`*

⚠ **Read the scope of that second half exactly.** `pfnRenderCb` and `pfnPresentCb` are absent from
**this header**. They are *not* absent from the D3D12 driver's reach: they arrive on
`D3D12DDIARG_CREATEDEVICE_0109::pKTCallbacks` (umddi:13623), which is a
`CONST D3DDDI_DEVICECALLBACKS*` — the same 65-entry kernel thunk table the D3D11 UMD drives today,
containing both (`d3dumddi.h:4499`, §6.3, `DECISIONS.md` P-C). The grep below is a statement about
*where the symbols are declared*, not about what a D3D12 UMD can call.

Verified by absence — all zero hits in `d3d12umddi.h`:

```
pCommandBuffer  AllocationList  PatchLocationList  pfnRenderCb  pfnPresentCb
```

Compare `D3DDDIARG_CREATEDEVICE` in `d3dumddi.h`, which carries `pCommandBuffer /
CommandBufferSize / pAllocationList / AllocationListSize / pPatchLocationList /
PatchLocationListSize / CommandBuffer (GPU VA)`. **Nothing in the D3D12 UMD DDI hands the driver a
buffer to record into.** `D3D12DDIARG_CREATE_COMMAND_POOL_0040` is *one flags word*.

**This is the single most important structural fact in this document, and it is what makes D1
possible.** The thing that would kill a forwarding UMD — the runtime handing the driver a buffer and
demanding hardware command packets in it — does not happen. The driver owns 100 % of the recording
memory, is free to record into an `ID3D12GraphicsCommandList` it obtained from vkd3d, and submits
whatever it likes on the WDDM context.

**Where the recording memory comes from, then:** the UMD allocates it itself (corelayer
`pfnAllocateCb`, or the KT `pfnAllocateCb`) and submits via `pKTCallbacks->pfnSubmitCommandCb` — the
WDDM 2.0 GPU-VA submission path, where the "DMA buffer" is just a GPU-VA range the UMD owns.
Corroborated by MS Learn for the callback itself:

> "**pfnSubmitCommandCb** is used to submit command buffers on contexts that support graphics
> processing unit (GPU) virtual addressing. These contexts generate commands directly from user
> mode, manage their own command buffer pool and don't make use of allocation or patch location
> list. … Since DMA buffer are built directly by the user mode driver and submitted to the GPU
> without modification …"
> — <https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dumddi/nc-d3dumddi-pfnd3dddi_submitcommandcb>

and by the runtime validating what the *driver* wrote into `D3DDDICB_SUBMITCOMMAND` (§6.4).

✅ **PARTLY SETTLED 2026-08-05 — there IS a direct statement in a Microsoft doc, and it names
`SubmitCommandCB`.** `ResourceHeaps.md:1678` (DirectX-Specs @ `2bd58ca5`, § *"SubmitCommandCB cannot
pass more than 8 handles in WrittenPrimaries"*), verbatim:

> The driver must call `SubmitCommandCB` during the call to `pfnExecuteCommandLists` from the same
> thread that entered the DDI. The driver must only pass DXGK context handles that were created during
> the command queue creation.

`CPUEfficiency.md` corroborates the timing independently — the driver must call the submission callback
*inside* the `ExecuteCommandLists` DDI and may not defer it — though that document predates WDDM 2.0
and never names which callback it means (it says "a command buffer submission callback", once).

**Three obligations this adds, none of which were written down here before:**

1. **Same thread.** `SubmitCommandCB` must be called on the thread that entered
   `pfnExecuteCommandLists`. The D3D11.3 functional spec states the underlying reason —
   *"only a single thread can be working against a HCONTEXT at a time"*
   (`archive/D3D11_3_FunctionalSpec.htm:7141`) — so a forwarder must **not** hand submission to
   vkd3d's internal submission thread and call the callback from there.
2. **Context provenance.** Only DXGK context handles minted during **command-queue creation** may be
   passed. This pins §9.1's per-queue `{ID3D12CommandQueue, WDDM hContext, …}` shadow record as
   mandatory, not merely convenient.
3. **`WrittenPrimaries` ≤ 8**, and *"The driver also cannot merge command lists, such that more than 8
   `WrittenPrimaries` handles would be passed"* (`:1680`). The runtime normally fills this field on the
   driver's behalf; a driver that creates its own primaries must pass them and the callback **fails**
   above eight.

⛔ **What it does NOT settle, and do not overread it:** the sentence mandates that `SubmitCommandCB` be
called during ECL. It does **not** forbid `pfnRenderCb`, and both still appear in `D3D12Core.dll`'s
validation strings (strings:13-18 name `D3DDDICB_RENDER` *and* `D3DDDICB_SUBMITCOMMAND`). So
`DECISIONS.md` P-C's plan — carrying the per-present identity on a `pfnRenderCb` Render command around
`pfnPresent`, as `umd/src/forward/present.rs:795` already does for D3D11 — **stands unchanged**, and
its `D12-G8` settling experiment is still required.

Residual settling experiment, unchanged: an ETW `Microsoft-Windows-DxgKrnl` all-keywords slice of a
D3D12 run — `DmaPacket` / `QueuePacket` events name the submission path (the ROADMAP.md recipe).
⚠ The WARP spy **cannot** contribute: WARP is a software rasterizer and called none of the
`pKTCallbacks` kernel thunks in any `D12-G5` run.

### 8.3 What Helios already has that makes an empty submit honest

The KMD's contract is written down in the source and it is the reason a "the real work went
out-of-band over venus" submit is not a lie
(`kmd_render/src/ddi/submit_command.rs:720-724`, verbatim):

> "There is no guest GPU to program (the host owns the real MMU; venus addresses by resource id —
> the actual work rides the venus Escape channel), but since C3/M3.4 the fence is NOT lied about: it
> queues behind the venus work outstanding at submit time and completes from the interrupt DPC.
> Runs at DISPATCH_LEVEL."

and the exact-boundary refinement (`kmd_render/src/ddi/submit_command.rs:504`, `:628-646`) decodes a
watermark from the DMA buffer's private data, registered on the ICD side by
`venus_register_present_stream(VkDevice, VkSemaphore, uint64_t* out_cookie)`
(`umd/bridge/bridge_icd_exports.h:37-42`).

**So the `pfnExecuteCommandLists` shape for Helios is:**

1. forward to `vkd3d`'s `ID3D12CommandQueue::ExecuteCommandLists`;
2. obtain a monotonic completion watermark for *that* submission;
3. submit an otherwise-empty DMA buffer on the queue's WDDM context, whose private data carries the
   watermark;
4. the KMD completes the DMA fence only when the host has reached it.

Step 2 is the one piece with no existing answer (`research/R2` §4.3): vkd3d's queue is asynchronous
(`vkd3d-proton-helios/libs/vkd3d/command.c`, internal submission thread), so the forwarder must
either signal an internal `ID3D12Fence` after each forwarded ECL and translate its Vulkan timeline
into the ICD present-stream cookie, or reach the `VkQueue` via `vkd3d_acquire_vk_queue`
(`vkd3d-proton-helios/include/vkd3d.h:104-142`) and signal an extra timeline semaphore itself. The
first is smaller and keeps vkd3d's ordering guarantees. `docs/dx12/SUBSTRATE.md` owns that choice.

⛔ **The invariant from CLAUDE.md applies unchanged: never signal a wire fence before host
completion.** An ECL that completes its WDDM fence immediately would reproduce the
DEVICE_LOST/freeze class the C3/M3.4 work fixed.

---

## 9. Semantics, area by area

The verdict column answers one question: *can this area be implemented as a forward into vkd3d's
`ID3D12*` COM objects, the way `umd/` forwards into DXVK's `ID3D11Device`?*

### 9.1 The verdict table

| # | Area | Forwardable? | Shadow state the UMD must keep | Risk |
|---|---|---|---|---|
| 1 | Device / adapter / queue creation & lifetime | **FORWARDABLE** | vkd3d `ID3D12Device`; per-queue `{ID3D12CommandQueue, WDDM hContext, cmd/alloc/patch windows, queue flags, hRTCommandQueue}`; paging queue | LOW |
| 2 | Command pools / recorders / lists / bundles | **FORWARDABLE WITH SHADOW STATE** | pool→`ID3D12CommandAllocator`; recorder→current pool; list→`ID3D12GraphicsCommandList` + last-Reset recorder; per-list `HRTCOMMANDLIST` + DDI-table identity | MEDIUM (volume: 75 CL entry points) |
| 3 | `ExecuteCommandLists` + kernel submission | **FORWARDABLE WITH SHADOW STATE** | per-queue monotonic watermark; DMA private-data marker layout shared with `kmd_render`; empty-DMA-buffer bookkeeping | HIGH (concentrated in §8.3 step 2) |
| 4 | Fences | **FORWARDABLE WITH SHADOW STATE** | fence handle → `{GPU VA pair per adapter, PhysicalAdapterMask, internal vkd3d `ID3D12Fence`, last requested value}` | **MEDIUM** (downgraded from HIGH — `DECISIONS.md` §6; residual in §10.4) |
| 5 | Descriptor heaps | **FORWARDABLE** — handle values pass through unchanged | heap handle → `ID3D12DescriptorHeap` only | LOW-MEDIUM (struct-return ABI) |
| 6 | Resources / heaps / placed & reserved / GPU VA | **FORWARDABLE WITH SHADOW STATE** | resource handle → `{ID3D12Resource, D3DKMT_HANDLE from pfnAllocateCb, VA, the DDI create-args}`; heap handle → `ID3D12Heap` | HIGH (KM identity + VA acceptance) |
| 7 | Residency / MakeResident / Evict / budgets | **FORWARDABLE** | paging-queue handle; per-allocation bookkeeping only if `E_PENDING` is ever returned | LOW |
| 8 | Root signatures, PSOs, PSO libraries, state objects | **FORWARDABLE WITH SHADOW STATE** | shader blobs per handle; blend/rasterizer/DS/element-layout descs per handle; **re-serialized root-signature blob** per handle | MEDIUM-HIGH |
| 9 | Barriers and resource state | **FORWARDABLE** | none beyond the resource handle map | LOW |
| 10 | Multi-queue COPY / COMPUTE, engine nodes | **FORWARDABLE, degraded to one node** | queue-flags→NodeOrdinal policy (all → node 0) | MEDIUM (caps honesty) |
| 11 | Debug layer / SDK layers | **must be designed in from day one** | debug-mode private sizes; `HANDLE_AND_TYPE` → `{D3DKMT_HANDLE, offset, size}` map | MEDIUM |
| 12 | Present | **FORWARDABLE**, and better than the bare-Vulkan path | present-context + KM allocation handles per swapchain buffer | see `docs/dx12/PRESENT.md` |

### 9.2 Device / adapter / queue creation and lifetime — FORWARDABLE

`pfnCreateDevice` → one `vkd3d_create_device()` (`vkd3d-proton-helios/include/vkd3d.h:110`), reached
through the added export `helios_vkd3d_create_device` (`DECISIONS.md` D4). `pfnCreateCommandQueue` →
one `ID3D12Device::CreateCommandQueue` **plus** one `pfnCreateContextCb` (§6.4).

Queue create args, newest form (umddi:7019-7025):

```c
typedef struct D3D12DDIARG_CREATECOMMANDQUEUE_0050
{
    D3D12DDI_COMMAND_QUEUE_FLAGS          QueueFlags;
    UINT                                  NodeMask;
    D3D12DDI_COMMAND_QUEUE_CREATION_FLAGS QueueCreationFlags;
    D3D12DDI_HSCHEDULINGGROUP_0050        SchedulingGroup; // May be NULL
} D3D12DDIARG_CREATECOMMANDQUEUE_0050;
```

⚠ **`D3D12DDIARG_CREATECOMMANDQUEUE_0050` carries no engine/node ordinal beyond `NodeMask`.** The
mapping from `QueueFlags` to a WDDM node is **the driver's choice**, expressed in the
`D3DDDICB_CREATECONTEXTVIRTUAL::NodeOrdinal` the UMD passes. Helios advertises exactly one node
(`DXGK_ENGINE_TYPE_3D`, `NbAsymetricProcessingNodes = 1`), so **all queue classes map to
NodeOrdinal 0**. That is legal WDDM; it costs parallelism, not correctness — dxgkrnl time-slices
the contexts, and the real work is out-of-band anyway. This is `DECISIONS.md` D5's "no extra engine
nodes" in its DDI form.

**Shadow state:** the `hRTCommandQueue`, the `D3DDDICB_CREATECONTEXT{VIRTUAL}` windows, the queue's
`D3D12DDI_COMMAND_QUEUE_FLAGS`, and the vkd3d `ID3D12CommandQueue`.

**Risk: LOW.** The lifetime rules are D3D11's; the only new thing is cardinality.

### 9.3 Command pools, recorders, lists, bundles — FORWARDABLE WITH SHADOW STATE

Forward mapping:

| DDI | vkd3d call |
|---|---|
| `pfnCreateCommandPool` | `ID3D12Device::CreateCommandAllocator(type)` |
| `pfnResetCommandPool` | `ID3D12CommandAllocator::Reset()` |
| `pfnCreateCommandRecorder` | *no vkd3d object* — a Helios-side shadow naming its current pool |
| `pfnCommandRecorderSetCommandPoolAsTarget` | store `recorder.pool = pool` |
| `pfnCreateCommandList` | `ID3D12Device::CreateCommandList(...)`, then immediately `Close()` (D3D12 lists are created open; the DDI's create is followed by a Reset before recording) |
| `pfnResetCommandList(hCL, {hRecorder,…})` | `ID3D12GraphicsCommandList::Reset(recorder->pool->allocator, nullptr)` |
| `pfnCloseCommandList` | `ID3D12GraphicsCommandList::Close()` — HRESULT discarded into `pfnSetCommandListErrorCb` |
| `pfnExecuteBundle(hCL, hBundleCL)` | `ID3D12GraphicsCommandList::ExecuteBundle` |

⚠ **`pfnSetCommandListDDITableCb` is mandatory at command-list creation.** The runtime says so:

> `Driver didn't call pfnSetCommandListDDITableCb or called it with invalid D3D12DDI_HRTTABLE at command list creation, defaulting to stubbed DDIs.` — strings:30

```c
typedef VOID (APIENTRY CALLBACK *PFND3D12DDI_SETCOMMANDLISTDDITABLE_CB)( D3D12DDI_HRTCOMMANDLIST, D3D12DDI_HRTTABLE );  // umddi:2554
```

**This is a genuinely useful mechanism for a forwarder, not just an obligation.** The driver may
swap a command list's DDI table *at any time*, so instead of a `if (!recording) return;` check at
the top of all 75 recording entry points, install:

- a **recording table** — the real forwarders — after a successful `pfnResetCommandList`;
- a **closed/erroring table** — every slot a counting no-op that calls
  `pfnSetCommandListErrorCb(hRT, E_INVALIDARG)` once — after `pfnCloseCommandList` or after the
  list enters an error state.

The `D3D12DDI_HRTTABLE` values come from `pfnGetOptionalDDITables` / `pfnFillDDITable` for
`D3D12DDI_TABLE_TYPE_COMMAND_LIST_3D` — the only type that entry point accepts (strings:238), which
is what `D3D12DDI_TABLE_REQUEST::numTables` is for. ⚠ **UNVERIFIED**: whether the driver may pass a
`HRTTABLE` it did not receive from `pfnFillDDITable`, and how a driver obtains *two* such handles.
Settling experiment: §15's spy logs WARP's `pfnGetOptionalDDITables` answer and every
`pfnSetCommandListDDITableCb` call.

**Shadow state:** recorder→pool binding; list→(recorder, pool) binding at last Reset; the list's
`HRTCOMMANDLIST`; which table is currently installed.

**Risk: MEDIUM.** Not a model mismatch — a volume-of-translation problem (75 entry points), plus one
subtlety: every set-state DDI takes *driver handles*, so each needs a handle→COM lookup through
`handles.rs`.

### 9.4 `ExecuteCommandLists` and kernel submission

Covered in §8.2/§8.3. The two caps the runtime queries and validates here:

- `D3D12DDICAPS_TYPE_EXECUTECOMMANDLISTS_PARALLELISM = 1069, // pData = BOOL` (umddi:128).
- `D3D12DDICAPS_TYPE_0023_UMD_BASED_COMMAND_QUEUE_PRIORITY = 1062` (umddi:118), with
  > `Driver did not correctly respond to D3D12DDICAPS_TYPE_0023_UMD_BASED_COMMAND_QUEUE_PRIORITY caps query.` — strings:22

**Risk: HIGH**, concentrated in the watermark.

### 9.5 Fences

Full treatment in §10.

### 9.6 Descriptor heaps — FORWARDABLE, the cleanest surprise

```c
// umddi:808-832
typedef enum D3D12DDI_DESCRIPTOR_HEAP_TYPE
{
    D3D12DDI_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
    D3D12DDI_DESCRIPTOR_HEAP_TYPE_SAMPLER,
    D3D12DDI_DESCRIPTOR_HEAP_TYPE_RTV,
    D3D12DDI_DESCRIPTOR_HEAP_TYPE_DSV,
    D3D12DDI_DESCRIPTOR_HEAP_TYPE_NUM_TYPES
} D3D12DDI_DESCRIPTOR_HEAP_TYPE;

typedef enum D3D12DDI_DESCRIPTOR_HEAP_FLAGS
{
    D3D12DDI_DESCRIPTOR_HEAP_FLAG_NONE           = 0x0,
    D3D12DDI_DESCRIPTOR_HEAP_FLAG_CPU_VISIBLE    = 0x1,
    D3D12DDI_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE = 0x2,
} D3D12DDI_DESCRIPTOR_HEAP_FLAGS;

typedef struct D3D12DDIARG_CREATE_DESCRIPTOR_HEAP_0001
{ D3D12DDI_DESCRIPTOR_HEAP_TYPE Type; UINT NumDescriptors; D3D12DDI_DESCRIPTOR_HEAP_FLAGS Flags; UINT NodeMask; } …;

// umddi:1415-1423 — both handles are single opaque scalars
typedef struct D3D12DDI_CPU_DESCRIPTOR_HANDLE { SIZE_T ptr; } D3D12DDI_CPU_DESCRIPTOR_HANDLE;
typedef struct D3D12DDI_GPU_DESCRIPTOR_HANDLE { UINT64  ptr; } D3D12DDI_GPU_DESCRIPTOR_HANDLE;

// umddi:1925-1927 — the DRIVER chooses the stride and both heap-start values
typedef UINT ( APIENTRY* PFND3D12DDI_GET_DESCRIPTOR_SIZE_IN_BYTES ) ( D3D12DDI_HDEVICE, D3D12DDI_DESCRIPTOR_HEAP_TYPE );
typedef D3D12DDI_CPU_DESCRIPTOR_HANDLE ( APIENTRY* PFND3D12DDI_GET_CPU_DESCRIPTOR_HANDLE_FOR_HEAP_START )( D3D12DDI_HDEVICE, D3D12DDI_HDESCRIPTORHEAP);
typedef D3D12DDI_GPU_DESCRIPTOR_HANDLE ( APIENTRY* PFND3D12DDI_GET_GPU_DESCRIPTOR_HANDLE_FOR_HEAP_START )( D3D12DDI_HDEVICE, D3D12DDI_HDESCRIPTORHEAP);
```

Answers to the questions a first implementer will ask:

- *Who allocates the descriptor storage?* **The driver, entirely.**
  `D3D12DDIARG_CREATE_DESCRIPTOR_HEAP_0001` has no pointer and no size, and no callback hands the
  driver descriptor memory. `pfnCalcPrivateDescriptorHeapSize` sizes only the *object*.
- *What goes in a CPU descriptor handle?* Whatever the driver likes. The view-creation DDIs take a
  destination CPU handle and are `VOID` — e.g. `pfnCreateShaderResourceView(hDevice, CONST
  D3D12DDIARG_CREATE_SHADER_RESOURCE_VIEW_0002*, D3D12DDI_CPU_DESCRIPTOR_HANDLE DestDescriptor)`
  (umddi:1885), and likewise CBV/UAV/RTV/DSV/Sampler (umddi:1894-1898).
- *What is a GPU descriptor handle at the DDI?* An opaque `UINT64` the driver minted. It comes back
  at `pfnSetGraphicsRootDescriptorTable(hCL, UINT RootParameterIndex, D3D12DDI_GPU_DESCRIPTOR_HANDLE
  BaseDescriptor)` (umddi:1941) and, for clear-UAV, as a (GPU handle in current heap, CPU handle)
  pair (umddi:2007-2032).

⭐ **A forwarder needs no shadow table at all.** Both handle values are driver-chosen opaque scalars
and the driver also chooses the stride, so Helios can create a matching `ID3D12DescriptorHeap` on
the vkd3d device and return **vkd3d's own handle values and increment size verbatim**
(`vkd3d-proton-helios/libs/vkd3d/resource.c:9146-9167` returns `heap->cpu_va` / `heap->gpu_va`;
`libs/vkd3d/device.c:6505-6512` returns vkd3d's increment). Runtime/app descriptor arithmetic
(`base + i*stride`) then lands on vkd3d's own arithmetic because it is the same stride. This is
`DECISIONS.md` H3's "good surprise".

`pfnCopyDescriptors` / `pfnCopyDescriptorsSimple` (umddi:1900-1918) map straight onto
`ID3D12Device::CopyDescriptors` / `CopyDescriptorsSimple`.

⚠⚠ **The one real hazard is ABI, not semantics.** The DDI returns
`D3D12DDI_CPU_DESCRIPTOR_HANDLE` / `D3D12DDI_GPU_DESCRIPTOR_HANDLE` **by value**; vkd3d's C
implementation returns via hidden pointer:

```c
static D3D12_CPU_DESCRIPTOR_HANDLE * STDMETHODCALLTYPE d3d12_descriptor_heap_GetCPUDescriptorHandleForHeapStart(
        ID3D12DescriptorHeap *iface, D3D12_CPU_DESCRIPTOR_HANDLE *descriptor);   // resource.c:9146-9147
```

That is the `bridge_guard` truncation class (commit `ead692e`, memory `t7-umd-crash-fixed-52nd.md`)
and it **must be handled explicitly in the cxx bridge**, with a test that round-trips a known
non-zero handle. Do not assume the C++ compiler picks the same convention on both sides.

**Caps the runtime cross-checks here** (all three abort device creation):

> `Driver's MaxViewDescriptorHeapSize is too small` — strings:115
> `Driver's MaxSamplerDescriptorHeapSize is too small` — strings:113
> `Driver's MaxSamplerDescriptorHeapSizeWithStaticSamplers is too small or larger than MaxSamplerDescriptorHeapSize` — strings:114

**Risk: LOW-MEDIUM.**

#### 9.6.1 ⛔ A second ABI hazard, and this one is silent: the heap flags do not mean the same thing

The DDI and API flag enums **collide on value `0x1` with different meanings**:

```c
// d3d12umddi.h:819-823                        // d3d12.h:3979-3980
D3D12DDI_DESCRIPTOR_HEAP_FLAG_NONE           = 0x0;   D3D12_DESCRIPTOR_HEAP_FLAG_NONE           = 0;
D3D12DDI_DESCRIPTOR_HEAP_FLAG_CPU_VISIBLE    = 0x1;   D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE = 0x1;
D3D12DDI_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE = 0x2;   // (no third value)
```

⛔ **Forwarding `pArgs->Flags` straight into `ID3D12Device::CreateDescriptorHeap` is a bug that
produces the wrong heap with no error**: a DDI `CPU_VISIBLE` heap (`0x1`) becomes an API
**shader-visible** heap, and a DDI `SHADER_VISIBLE` heap (`0x2`) becomes a flag value the API does not
define at all. The bridge must **translate**, not pass through. `D3D12DDIARG_CREATE_DESCRIPTOR_HEAP_0001`
(umddi:826-832) types the member as `D3D12DDI_DESCRIPTOR_HEAP_FLAGS`, so the compiler will not catch it.

*(`CPU_VISIBLE` is a DDI-only concept: the API deleted the flag but the DDI kept it. `ResourceBinding.md`
still documents shader-visible as `0x1`, which is the API value — one more instance of `SPECS.md` §6
trap 1, a spec's DDI block not matching the shipping header.)*

**Three more descriptor-heap facts the corpus supplies, each checkable in one line:**

- **Every descriptor heap must have a non-NULL GPU handle at the DDI, including non-shader-visible
  ones.** The API rule that `GetGPUDescriptorHandleForHeapStart` returns NULL for a non-shader-visible
  heap is an API-layer rule and does **not** carry down to the driver.
- **The runtime is permitted to apply a "cheap scale/shift" to descriptor handles** crossing between
  the application and the driver (`ResourceBinding.md:4397-4401`, restated for the copy path at
  `:4827-4830`). So a handle observed at the DDI is in the **driver's** space, and the driver must never
  assume the app sees the same numeric value. This does **not** disturb the forwarding plan — returning
  vkd3d's own handles and stride verbatim stays correct, because the driver's space *is* vkd3d's space.
- **Two hardware-invariant budgets** bind the driver: **2048** samplers in any shader-visible sampler
  heap, and **2032** unique static samplers across all root signatures alive on the device at once.

⚠ **And one floor that is uncomfortably tight on this substrate.** `VulkanOn12.md:1122` requires new
drivers to report `MaxSamplerDescriptorHeapSize >= 4000`, mandatory at DDI 0102+ and therefore at
Helios' negotiated 0110. The host GPU's `VkPhysicalDeviceLimits::maxSamplerAllocationCount` is
**exactly 4000** (`docs/reference/host-vulkan-profile-rtx-pro-6000-blackwell.json:882`) — the mandated
floor consumes the entire substrate budget with zero headroom, *if* vkd3d allocates one `VkSampler` per
descriptor. Whether it dedupes is **UNVERIFIED**; see `GATES.md` §7.

### 9.7 Resources, heaps, placed/reserved, GPU virtual addresses

Creation is the single fused DDI of §7.3(1). The resource args, verbatim (umddi:13413-13436):

```c
typedef struct D3D12DDIARG_CREATERESOURCE_0109
{
    D3D12DDIARG_BUFFER_PLACEMENT    ReuseBufferGPUVA;
    D3D12DDI_RESOURCE_TYPE          ResourceType;
    UINT64                          Width;   // Virtual coords
    UINT                            Height;  // Virtual coords
    UINT16                          DepthOrArraySize;
    UINT16                          MipLevels;
    DXGI_FORMAT                     Format;
    DXGI_SAMPLE_DESC                SampleDesc;
    D3D12DDI_TEXTURE_LAYOUT         Layout;  // See standard swizzle spec
    D3D12DDI_RESOURCE_FLAGS_0003    Flags;
    D3D12DDI_BARRIER_LAYOUT         InitialBarrierLayout;

    // When Layout = D3D12DDI_TL_ROW_MAJOR and pRowMajorLayout is non-null
    // then *pRowMajorLayout specifies the layout of the resource
    CONST D3D12DDIARG_ROW_MAJOR_RESOURCE_LAYOUT* pRowMajorLayout;

    D3D12DDI_MIP_REGION_0075        SamplerFeedbackMipRegion;
    UINT32                          NumCastableFormats;
    const DXGI_FORMAT *             pCastableFormats;

    D3D12DDI_GPU_VIRTUAL_ADDRESS    CreateAtVirtualAddress;
} D3D12DDIARG_CREATERESOURCE_0109;
```

**The runtime hard-requires the whole family — nine explicit NULL checks:**

> `Driver set pfnCreateHeapAndResource to NULL.` — strings:101
> `Driver set pfnDestroyHeapAndResource to NULL.` — strings:102
> `Driver set pfnOpenHeapAndResource to NULL.` — strings:103
> `Driver set pfnCalcPrivateHeapAndResourceSizes to NULL.` — strings:95
> `Driver set pfnCalcPrivateOpenedHeapAndResourceSizes to NULL.` — strings:96
> `Driver set pfnCheckResourceAllocationInfo to NULL.` — strings:98
> `Driver set pfnCheckExistingResourceAllocationInfo to NULL.` — strings:97
> `Driver set pfnCheckSubresourceInfo to NULL.` — strings:99
> `Driver set pfnCopyBufferRegion to NULL.` — strings:100
> `Driver must set pfnMapHeap and pfnUnmapHeap to non-NULL.` — strings:54

**Kernel identity is mandatory in at least three places, so "pure passthrough with no
`pfnAllocateCb`" is not viable:**

```c
typedef D3DKMT_HANDLE ( APIENTRY* PFND3D12DDI_CHECKRESOURCEALLOCATIONHANDLE )( D3D12DDI_HDEVICE, D3D10DDI_HRESOURCE );  // umddi:2992
// pfnGetDebugAllocationInfo must return, per handle:
typedef struct D3D12DDI_DEBUG_KMT_ALLOCATION_INFO_0014
{ UINT32 PhysicalAdapterIndex; D3DKMT_HANDLE hAllocation; UINT64 Offset; UINT64 Size; };  // umddi:3890-3905
// pfnAllocateCb mints the kernel allocations — §6.2
```

The runtime also constrains what `pfnCheckResourceAllocationInfo` may return for a resource whose
layout it knows:

> `Driver returned unexpected D3D12DDI_RESOURCE_ALLOCATION_INFO_0022::Layout for a resource with a known layout.` — strings:81
> `Driver returned unexpected D3D12DDI_RESOURCE_ALLOCATION_INFO_0022::ResourceDataSize for a resource with a known layout.` — strings:82
> `Driver returned non-zero D3D12DDI_RESOURCE_ALLOCATION_INFO_0022::AdditionalDataSize for a resource with a known layout.` — strings:80
> `Driver returned non-zero D3D12DDI_RESOURCE_ALLOCATION_INFO_0022::AdditionalDataHeaderSize for a resource with a known layout.` — strings:79

**GPU virtual addresses.** `typedef UINT64 D3D12DDI_GPU_VIRTUAL_ADDRESS;` (umddi:92). The runtime
asks the driver for a resource's VA
(`PFND3D12DDI_CHECKRESOURCEVIRTUALADDRESS(HDEVICE, HRESOURCE) -> D3D12DDI_GPU_VIRTUAL_ADDRESS`,
umddi:2476), and the VA then travels through root descriptors
(`PFND3D12DDI_SET_ROOT_BUFFER_VIEW(..., D3D12DDI_GPU_VIRTUAL_ADDRESS BufferLocation)`, umddi:1959),
IB/VB/SO views (umddi:1963-1989), and indirect-argument buffers. Caps and validation:

> `Driver set MaxGPUVirtualAddressBitsPerResource to 0.` — strings:94
> `FL12.2+ driver incorrectly did not report at least 40 bits of GPU virtual address bits` — strings:171

Helios reports a 40-bit GPU VA (`kmd_render/src/ddi/gpummu.rs:44-65`), which clears that bar
exactly.

⚠ **The open question, restated precisely.** `kmd_render/src/ddi/gpummu.rs:1-14` records that the
guest page tables are **decorative** — the host GPU owns the real MMU. For a *forwarding* UMD the
guest VA space is not what would be used at all: vkd3d's `ID3D12Resource::GetGPUVirtualAddress`
returns `resource->res.va` (`vkd3d-proton-helios/libs/vkd3d/resource.c:2656-2663`), a Vulkan
**buffer device address** in the *host* GPU's address space obtained through venus. A forwarder
returns those from `pfnCheckResourceVirtualAddress` and never calls `pfnReserveGpuVirtualAddressCb`
/ `pfnMapGpuVirtualAddressCb`.

**UNVERIFIED: whether the D3D12 runtime and its debug layer accept a VA space the driver never
obtained from the kernel.** Settling experiment: report
`MaxGPUVirtualAddressBitsPerResource = 40`, return BDAs, and run a D3D12 sample **with the debug
layer enabled** (`d3d12SDKLayers.dll`), watching for the string `MaxGPUVirtualAddressBitsPerResource
error` (fullstrings:22509) and any GPU-VA validation break. If the debug layer only tracks
per-resource VA ranges for self-consistency and range, BDAs pass — they are self-consistent and
within 40 bits on this host. The header offers one hook suggesting the runtime *can* care about VA
placement: `D3D12DDIARG_CREATEDEVICE_0109.pReserveRanges / NumReserveRanges` (umddi:13634-13635) and
`D3D12DDI_RECREATE_AT_TIER` + `CreateAtVirtualAddress` (umddi:13397-13435) — but
`D3D12DDI_RECREATE_AT_TIER_NOT_SUPPORTED = 0` is a legal answer and is what Helios should report:

⭐ **And on this build that hook cannot fire at all — for a stronger reason than the tier answer.**
`RecreateAtGpuva-public.md:42` (DirectX-Specs @ `2bd58ca5`) states:

> **Note: While the DDI rev is on 109, RecreateAt functionality is gated behind DDI 0111. **

`D3D12DDI_BUILD_VERSION_0111` and `D3D12DDI_SUPPORTED_0111` are **absent from SDK 10.0.26100.0** (0
hits each; the header stops at `_0110`, which is what `D12-G5` negotiated). So the runtime will not use
`CreateAtVirtualAddress` or `pReserveRanges` on this Windows build **whatever tier the driver reports**
— the binding reason is the DDI version, not Helios' `NOT_SUPPORTED` answer. Helios should still report
`NOT_SUPPORTED`, and should additionally treat a non-zero `CreateAtVirtualAddress` or `NumReserveRanges`
as a **named refusal counter** rather than silently ignoring the fields, because arrival would mean the
build assumption is wrong (CLAUDE.md rule 2).

⚠ **This does not settle §15.1 #10** — see the verdict there. `RecreateAtGpuva-public.md` is the closest
the corpus comes and it describes **no provenance check anywhere**: the runtime *reads* VAs back out of
already-created driver objects (`ID3D12PageableTools::GetAllocation`) and passes recorded ranges through
*"not processed in any way … depending on driver behavior during record"* (`:185`). That is the
direction the BDA plan bets on, but it is prose about a gated path, not a measurement.

```c
typedef enum D3D12DDI_RECREATE_AT_TIER
{
    D3D12DDI_RECREATE_AT_TIER_NOT_SUPPORTED = 0,
    // * Supports setting resource and heap virtual addresses with
    //   CreateAtVirtualAddress in D3D12DDIARG_CREATERESOURCE_0109
    D3D12DDI_RECREATE_AT_TIER_1 = 1,
} D3D12DDI_RECREATE_AT_TIER;

typedef struct D3D12DDI_OPTIONS_0109 { D3D12DDI_RECREATE_AT_TIER RecreateAtTier; } D3D12DDI_OPTIONS_DATA_0109;
```

⚠ Note the struct **tag** is `D3D12DDI_OPTIONS_0109` while the **typedef name** is
`D3D12DDI_OPTIONS_DATA_0109`. Same trick at umddi:12686 (`typedef struct D3D12DDI_OPTIONS1_DATA_0103
{ … } D3D12DDI_OPTIONS_DATA_0103;`). bindgen emits the typedef name; a grep for the tag will miss
it.

**Reserved (tiled) resources** additionally need `pfnUpdateTileMappings` / `pfnCopyTileMappings`,
which live on the **command queue** table — i.e. they are *immediate* operations, not recorded ones.
vkd3d implements them on `ID3D12CommandQueue`, so the mapping is 1:1.

**Risk: HIGH** (kernel-allocation identity + VA acceptance), **but not obviously fatal.**

### 9.8 Residency — FORWARDABLE (mostly trivially)

Driver-side DDIs (umddi:1842-1850):

```c
typedef HRESULT ( APIENTRY* PFND3D12DDI_MAKERESIDENT_0001 )( D3D12DDI_HDEVICE, D3D12DDIARG_MAKERESIDENT_0001* );
typedef HRESULT ( APIENTRY* PFND3D12DDI_EVICT2 )( D3D12DDI_HDEVICE, CONST D3D12DDIARG_EVICT* );
typedef HRESULT ( APIENTRY* PFND3D12DDI_OFFERRESOURCES )( D3D12DDI_HDEVICE, CONST D3D12DDIARG_OFFERRESOURCES* );
typedef HRESULT ( APIENTRY* PFND3D12DDI_RECLAIMRESOURCES_0001 )( D3D12DDI_HDEVICE, D3D12DDIARG_RECLAIMRESOURCES_0001* );
```

with the paging-fence protocol spelled out in the args (umddi:494-514):

```c
    _Field_size_(NumAdapters) UINT64* pPagingFenceValue;    // out: Fence to wait on
    UINT WaitMask;   // out: Bit "i" is set if PagingFenceValue[i] is valid.  Only if MakeResident returns E_PENDING.
```

Callback side: `pfnMakeResidentCb`, `pfnEvictCb`, `pfnReclaimAllocations2Cb`,
`pfnOfferAllocationsCb`, all taking a `D3D12DDI_HRTPAGINGQUEUE` from `pfnCreatePagingQueueCb`.
**Helios' D3D11 UMD already creates the WDDM 2.x paging queue** —
`umd/src/device_funcs.rs:1101` `create_runtime_paging_queue()`; port it.

Budgets are answered through the optional downlevel table (umddi:18320-18349) —
`D3D12DDI_QUERY_VIDEO_MEMORY_INFO_0054 { UINT64 Budget; UINT64 CurrentUsage; }`,
`D3D12DDI_MEMORY_SEGMENT_GROUP_0054 { LOCAL, NON_LOCAL }` — which a baseline device declines by
reporting zero extended features. `QueryVideoMemoryInfo` then falls back to VidMm's own numbers.

`DECISIONS.md` §6 corrects a widely-repeated misattribution: `DriverManagesResidency` is a
**`DXGK_CONTEXTINFO_CAPS`** bit (`d3dkmddi.h:1550-1563`), not a `DXGK_VIDMMCAPS` bit, and Helios
never writes `ContextInfo.Caps` at all. The conclusion is unchanged — **VidMm owns residency** — so
the driver's MakeResident/Evict can be honest thin forwards.

⛔ **The one trap: never fake the `E_PENDING` + paging-fence protocol.** Returning `E_PENDING`
without a valid `pPagingFenceValue`/`WaitMask` hangs the caller with no error anywhere.

**Risk: LOW.**

### 9.9 Root signatures, PSOs, PSO libraries, state objects

**Root signatures arrive PARSED, not as a blob.** Verbatim (umddi:12269-12290):

```c
typedef struct D3D12DDI_ROOT_SIGNATURE_0100
{
    UINT NumParameters;
    CONST D3D12DDI_ROOT_PARAMETER_0013* pRootParameters;
    UINT NumStaticSamplers;
    CONST D3D12DDI_STATIC_SAMPLER_0100* pStaticSamplers;
    D3D12DDI_ROOT_SIGNATURE_FLAGS Flags;
} D3D12DDI_ROOT_SIGNATURE_0100;

typedef struct D3D12DDIARG_CREATE_ROOT_SIGNATURE_0100
{
    D3D12DDI_ROOT_SIGNATURE_VERSION Version;
    union
    {
        CONST D3D12DDI_ROOT_SIGNATURE_0100* pRootSignature_1_2;
    };
    UINT NodeMask;
} D3D12DDIARG_CREATE_ROOT_SIGNATURE_0100;
```

and **there is no root-signature version 1.0 at the DDI** (umddi:3743-3747):

```c
typedef enum D3D12DDI_ROOT_SIGNATURE_VERSION { D3D12DDI_ROOT_SIGNATURE_VERSION_1_1 = 0x2,
                                               D3D12DDI_ROOT_SIGNATURE_VERSION_1_2 = 0x3, } …;
```

✅ **Confirmed independently, and the *reason* there is no 1.0 is now documented.** `ResourceBinding.md`
(DirectX-Specs @ `2bd58ca5`) states that **the runtime up-converts version 1.0 root signatures to 1.1
before the driver sees them**. So a 1.0 root signature from an application is never delivered as 1.0,
and `helios_umd12` needs exactly **one** parse path over the `D3D12DDI_ROOT_SIGNATURE_0100` tree — never
a serialized blob, never a 1.0 arm. ⚠ The same spec contains a sentence claiming a *"serialized
version"* arrives and that the DDK ships deserializer source: that is **dead text**, contradicted by the
spec's own DDI struct and by the `D12-G5` measurement. The measurement wins.

**Three constraints on the H3 re-serializer that nothing here recorded before:**

1. **Defaults are already applied.** For descriptor-range flags and root-descriptor flags the runtime
   fills in the documented API defaults before the driver sees them, so the driver **cannot distinguish
   "app asked for `DATA_VOLATILE`" from "app said nothing and got the default"**. A re-serializer must
   therefore emit the flags it is given verbatim and must not try to reconstruct app intent.
2. **`NumDescriptors == -1` means an unbounded range**, legal only as the **last** entry in a table. As
   an unsigned count that is `0xFFFFFFFF`, so any range-size arithmetic overflows — treat it as a
   sentinel *before* doing arithmetic, not after.
3. ⚠ **A driver must accept root signatures larger than the 64-DWORD API limit — up to 128 DWORDs** —
   because the OS injects its own root parameters for shader instrumentation (in reserved register
   spaces `0xfffffff0`-`0xffffffff`) and deliberately does **not** tell the driver which are which. A
   re-serializer that assumes the API limit will reject an OS-instrumented signature.

~~⚠ **`D3D12DDI_ROOT_CONSTANTS` is not field-order-compatible with the API's
`D3D12_ROOT_CONSTANTS`**: the DDI puts `ShaderRegister` and `RegisterSpace` **before**
`Num32BitValues`; the API puts `Num32BitValues` first. Same three `UINT`s, different order — a
`memcpy` or a struct-cast silently transposes them.~~

⛔ **FALSE, and struck 2026-08-06 (S6 Round 2). The two structs are field-order IDENTICAL**, in
both SDK headers in this repository:

```c
/* tmp/dx12/sdk/d3d12umddi.h:1310-1315 */    /* tmp/dx12/sdk/d3d12.h:4016-4021 */
typedef struct D3D12DDI_ROOT_CONSTANTS       typedef struct D3D12_ROOT_CONSTANTS
{                                                {
    UINT ShaderRegister;                         UINT ShaderRegister;
    UINT RegisterSpace;                          UINT RegisterSpace;
    UINT Num32BitValues;                         UINT Num32BitValues;
} D3D12DDI_ROOT_CONSTANTS;                       }   D3D12_ROOT_CONSTANTS;
```

The Win32 metadata agrees independently — windows-rs 0.58's `D3D12_ROOT_CONSTANTS`
(`Direct3D12/mod.rs:13817-13821`) is `ShaderRegister, RegisterSpace, Num32BitValues`. Three
generators, one order.

⚠ **The correction matters because the claim had propagated and was being acted on**: it reached
`DX12.md` §4.3 row 4 as one of *"two silent ABI hazards in the bridge, neither catchable by the
compiler"*, and from there into `umd12/src/forward12/rootargs.rs`'s module doc, i.e. into the brief
of the lane that would have written a hand-field-by-field copy to defend against a transposition
that does not exist. ⭐ The **other** hazard in that row is real and was verified independently:
the descriptor-heap flags genuinely do collide on `0x1` with different meanings (§9.6.1), and
`descriptors.rs::api_heap_flags` translates them with a `const _` pinning the collision. A half-true
row is worse than a false one, because the true half lends it credibility.

⇒ The generalisable form, which is `PARALLEL.md` §10's **claim-integrity** lens: *an ABI claim in a
document is a claim, and both sides of it are machine-generated — so it can always be checked, and
it must be, before a lane writes code against it.*

At `_0100` the union has exactly one arm, `pRootSignature_1_2` — **the driver is handed
1.2-shaped root signatures only**; the runtime up-converts 1.0 and 1.1. (⚠ Still switch on
`Version` with an exhaustive match: a future revision adds arms, and `DECISIONS.md` §7.4 forbids an
`else`.)

⚠ **vkd3d's `ID3D12Device::CreateRootSignature` wants a serialized DXBC `RTS0` blob**
(`vkd3d-proton-helios/libs/vkd3d/device.c:6514-6531`), so **the UMD must re-serialize**. The
function exists — `vkd3d_serialize_root_signature(const D3D12_ROOT_SIGNATURE_DESC*, version, blob,
error_blob)` at `vkd3d-proton-helios/include/vkd3d.h:129` and `libs/vkd3d/vkd3d_main.c:453`, layered
on `vkd3d_shader_serialize_root_signature` (`libs/vkd3d-shader/dxbc.c:1384`, writer at
`dxbc.c:1019-1045`, checksum at `dxbc.c:1425`) — but it is **not exported** from vkd3d's Windows
DLL (`libs/d3d12core/d3d12core.def` is exactly two lines of exports: `D3D12GetInterface` and
`D3D12SDKVersion DATA PRIVATE`).

✅ **This is settled, not a suggestion: `DECISIONS.md` D4 now specifies TWO added exports on
`helios_vkd3d.dll`, not one.** Both, verbatim, so nobody has to re-derive the second signature:

```c
/* export 1 — the device entry point, bypassing d3d12core's CreateDXGIFactory1 path */
HRESULT helios_vkd3d_create_device(LUID adapter_luid, REFIID iid, void **device);

/* export 2 — the root-signature serializer, because the DDI delivers root signatures
 * ALREADY PARSED (D3D12DDI_ROOT_SIGNATURE_0100) while ID3D12Device::CreateRootSignature
 * wants a serialized DXBC RTS0 blob. Thin wrapper over vkd3d_serialize_root_signature
 * (include/vkd3d.h:129, defined at libs/vkd3d/vkd3d_main.c:453). */
HRESULT helios_vkd3d_serialize_root_signature(const D3D12_ROOT_SIGNATURE_DESC *desc,
                                              D3D_ROOT_SIGNATURE_VERSION version,
                                              ID3DBlob **blob, ID3DBlob **error_blob);
```

✅ Reconciled 2026-08-05: `DECISIONS.md` D4 and `docs/dx12/ARCHITECTURE.md` both specify **two**
added exports. ⚠ The failure mode is worth remembering, because it is silent until late: a
one-export `helios_vkd3d.dll` builds and links fine, passes `D12-G7`, and only fails in the PSO
tranche when `forward12/pso.rs` needs to re-serialize a root signature — by which point the DLL's
export surface is baked into the deploy scripts.

The runtime's own note about a root-signature flag combination the driver will see:

> `…ING_BUFFER_BOUNDS_CHECKS). This combination is ignored and treated as DESCRIPTORS_VOLATILE. To enable static descriptor driver optimizations or debug validation, specify a bounded descriptor table size.` — strings:224

**PSOs arrive as handle bundles**, verbatim (umddi:11952-11978):

```c
typedef struct D3D12DDIARG_CREATE_PIPELINE_STATE_0099
{
    D3D12DDI_HSHADER hComputeShader;  D3D12DDI_HSHADER hVertexShader;  D3D12DDI_HSHADER hPixelShader;
    D3D12DDI_HSHADER hDomainShader;   D3D12DDI_HSHADER hHullShader;    D3D12DDI_HSHADER hGeometryShader;
    D3D12DDI_HROOTSIGNATURE hRootSignature;
    D3D12DDI_HBLENDSTATE hBlendState;
    UINT SampleMask;
    D3D12DDI_HRASTERIZERSTATE hRasterizerState;
    D3D12DDI_HDEPTHSTENCILSTATE hDepthStencilState;
    D3D12DDI_HELEMENTLAYOUT hElementLayout;
    D3D12DDI_INDEX_BUFFER_STRIP_CUT_VALUE IBStripCutValue;
    D3D12DDI_PRIMITIVE_TOPOLOGY_TYPE PrimitiveTopologyType;
    UINT NumRenderTargets;   DXGI_FORMAT RTVFormats[8];   DXGI_FORMAT DSVFormat;
    DXGI_SAMPLE_DESC SampleDesc;   UINT NodeMask;
    D3D12DDI_LIBRARY_REFERENCE_0010 LibraryReference;
    D3D12DDI_VIEW_INSTANCING_DESC ViewInstancingDesc;
    D3D12DDI_HSHADER hMeshShader;  D3D12DDI_HSHADER hAmplificationShader;
    D3D12DDI_PIPELINE_STATE_FLAGS Flags;
} D3D12DDIARG_CREATE_PIPELINE_STATE_0099;
```

So blend / rasterizer / depth-stencil / element-layout are **separate driver objects created
earlier** and referenced by handle; vkd3d wants them **inline** in
`D3D12_GRAPHICS_PIPELINE_STATE_DESC`.

**Shadow state, and this is the concrete work item:** each of `pfnCreateBlendState`,
`pfnCreateRasterizerState`, `pfnCreateDepthStencilState`, `pfnCreateElementLayout` stores its full
DDI desc in the runtime-allocated private block (translating `D3D12DDI_BLEND` (umddi:2740-2763) →
`D3D12_BLEND`, etc.); each `pfnCreate*Shader` stores its bytecode blob (§12); then
`pfnCreatePipelineState` reassembles a `D3D12_GRAPHICS_PIPELINE_STATE_DESC` from the handles and
calls `ID3D12Device::CreateGraphicsPipelineState`. `hComputeShader != NULL` selects
`CreateComputePipelineState` instead.

Pipeline libraries (`pfnCalcPrivatePipelineLibrarySize`, `pfnCreatePipelineLibrary`,
`pfnAddPipelineStateToLibrary`, `pfnCalcSerializedLibrarySize`, `pfnSerializeLibrary`) map to
`ID3D12PipelineLibrary`. State objects / DXR map to `ID3D12StateObject` and are a large second
tranche — declinable at first via `RaytracingTier = NOT_SUPPORTED` (§11).

**Risk: MEDIUM-HIGH.** No model mismatch; a lot of struct translation plus one genuine
reconstruction. Exactly the class of work the bindgen discipline (`DECISIONS.md` §7.2) exists to
make safe.

### 9.10 Barriers and resource state — FORWARDABLE

**The driver sees barriers; the runtime does not resolve them.** Both generations sit in one table
(umddi:4802-4816 legacy, umddi:13380 enhanced):

```c
typedef struct D3D12DDIARG_RESOURCE_BARRIER_0022
{
    D3D12DDI_RESOURCE_BARRIER_TYPE    Type;      // TRANSITION | ALIASING | UAV | 0022_RANGED  (umddi:1477-1483)
    D3D12DDI_RESOURCE_BARRIER_FLAGS   Flags;     // NONE | BEGIN_ONLY | END_ONLY | ATOMIC_COPY | ALIASING (umddi:1505-1512)
    union { D3D12DDI_RESOURCE_TRANSITION_BARRIER_0003 Transition;
            D3D12DDI_RESOURCE_RANGED_BARRIER_0022     Ranged;
            D3D12DDI_RESOURCE_UAV_BARRIER             UAV; };
} D3D12DDIARG_RESOURCE_BARRIER_0022;
typedef VOID ( APIENTRY* PFND3D12DDI_RESOURCEBARRIER_0022 )( D3D12DDI_HCOMMANDLIST, UINT Count, _In_reads_(Count) CONST D3D12DDIARG_RESOURCE_BARRIER_0022* );
```

Support for the enhanced form is opt-in through `EnhancedBarriersSupported` in
`D3D12DDI_D3D12_OPTIONS_DATA_0089` (umddi:11111); Microsoft's own feature page says so
(`windows-driver-docs-pr/display/enhanced-barriers.md:34`). Resources also carry an
`InitialBarrierLayout` at creation (umddi:13425).

Both map 1:1 onto `ID3D12GraphicsCommandList::ResourceBarrier` and
`ID3D12GraphicsCommandList7::Barrier`.

⛔ **Report `EnhancedBarriersSupported = FALSE` until the enhanced path is really implemented.**
Silently treating enhanced barriers as legacy loses synchronisation, which on this stack is a
venus-side write/read race with no guest-visible error. **Risk: LOW** if that rule is kept.

#### 9.10.1 ⭐ The cap is a table-shape decision, not just a feature flag (2026-08-05)

`D3D12EnhancedBarriers.md:539` (DirectX-Specs @ `2bd58ca5`, § *"Compatibility with legacy
D3D12_RESOURCE_STATES"*), verbatim:

> The D3D12 runtime internally translates all `ResourceBarrier` calls to equivalent Enhanced Barriers at the driver interface.  Legacy barrier DDI's are never invoked on a driver supporting enhanced barriers.

⇒ **The two barrier generations are mutually exclusive at the driver, not additive.** This explains the
`D12-G5` measurement (the runtime calls `pfnBarrier`, `cl[68]`, once the driver reports
`EnhancedBarriersSupported = 1`) as a *rule* rather than an observation of one workload:

| answer | what Helios implements | what the other slot becomes |
|---|---|---|
| `EnhancedBarriersSupported = 1` | `pfnBarrier` (`cl[68]`) only | `pfnResourceBarrier` is **dead code** — point it at a counting refusal, not a second lowering path |
| `EnhancedBarriersSupported = 0` | `pfnResourceBarrier` only | `pfnBarrier` never called |

**Helios implements ONE barrier path either way.** That removes the main cost objection to the enhanced
arm, which is the better target for a vkd3d forwarder because `D3D12_BARRIER_LAYOUT` maps far closer to
`VkImageLayout` than `D3D12_RESOURCE_STATES` does. ⛔ The rule above still stands until it is real: the
cap must not be flipped to 1 before `pfnBarrier` is implemented, because at 1 there is no legacy
fallback left to catch the gap.

⚠ **Enhanced barriers do NOT remove promotion/decay from the driver.** The runtime re-encodes the
legacy model's ambiguity as **DDI-only layout values** that never appear in the public API:

```c
D3D12DDI_BARRIER_LAYOUT_LEGACY_COPY_SOURCE = 0x80000000,   // umddi:10632-10635, "Special layouts start here"
D3D12DDI_BARRIER_LAYOUT_LEGACY_COPY_DEST,
D3D12DDI_BARRIER_LAYOUT_LEGACY_SHADER_RESOURCE,
D3D12DDI_BARRIER_LAYOUT_LEGACY_PIXEL_SHADER_RESOURCE,
D3D12DDI_BARRIER_LAYOUT_LEGACY_DIRECT_QUEUE_GENERIC_READ_COMPUTE_QUEUE_ACCESSIBLE,  // umddi:10630
```

The spec calls these *"internal-only and not exposed in public headers"* (`:570`) — and they are indeed
absent from `d3d12.h` while present in `d3d12umddi.h`. **A forwarder must translate them before calling
vkd3d**, which cannot accept a value its own API header does not define. Cross-checking the two shipped
headers (`d3d12umddi.h:10595-10636` vs `d3d12.h:22121-22156`), DDI↔API layout translation is the
**identity for 0..30 and for `UNDEFINED = 0xffffffff`**, with exactly these five DDI-only values needing
a mapping.

⚠ **The DDI buffer-barrier struct has no `Offset` and no `Size`** — the API's two `UINT64` members are
stripped by the runtime before the driver sees them, so a buffer barrier is always whole-resource at
the DDI.

⛔ **The spec's published DDI structs do not match SDK 26100 and must not be transcribed.** Its
`D3D12DDI_RANGED_BARRIER_0094` carries a 24-byte `D3D12DDI_BARRIER_SUBRESOURCE_RANGE_0088 Subresources`
where the shipping header (umddi:11277) has a 4-byte `UINT Subresource`. Take shapes from the header.

⚠ **The entire *Fence Barriers* half of that spec is preview-only** — *"The Fence Barriers preview
requires developer mode."*, it needs `D3D12EnableExperimentalFeatures`, and every symbol it names is
absent from `d3d12umddi.h`. Nothing shipping does it; do not plan for it (§10).

⚠ **One obligation the cap carries that is easy to miss:** a texture barrier with `FLAG_DISCARD` on an
`UNDEFINED`-to-compressible layout transition makes compression-metadata initialisation a **driver**
obligation. `D3D12DDI_TEXTURE_BARRIER_FLAG_DISCARD` and the API flag are both `0x1`, so the bit
forwards raw — but a forwarder must satisfy the obligation, not merely pass the flag.

### 9.11 Multi-queue COPY / COMPUTE — FORWARDABLE, degraded

At the DDI, queue class is a flag, not a node (§8.1). Helios advertises one node, so all contexts
land on NodeOrdinal 0. Multiple contexts on one node is legal WDDM; it costs parallelism, not
correctness, and vkd3d may still get real host-side parallelism because the actual work is
out-of-band.

Two caps must be answered honestly, and the runtime enforces the consequences:

> `Driver did not correctly respond to D3D12DDICAPS_TYPE_0050_HARDWARE_SCHEDULING_CAPS caps query.` — strings:23
> `Driver didn't provide any HwQueues for a hardware scheduling command queue present.` — strings:31

⛔ Helios must report **`ComputeQueuesPer3DQueue = 0`** ("0 means don't use scheduling groups",
umddi:7007) because `DxgkDdiCreateHwQueue` returns `STATUS_NOT_SUPPORTED` and records `HwQRef`
(`kmd_render/src/ddi/scheduler.rs:180-187`). The KMD refuses *at queue creation* specifically to
avoid the "succeed at create, fail at submit" VidSch `0x119`/Arg1=2 bugcheck. Non-zero here opts
into `D3D12DDIARG_CREATESCHEDULINGGROUP_0050` (umddi:7010-7013) and lands on exactly that bugcheck.
`pfnCalcPrivateSchedulingGroupSize` / `pfnCreateSchedulingGroup` / `pfnDestroySchedulingGroup` must
then refuse consistently — at the *first* step, never succeed-then-fail (`DECISIONS.md` §7.7).

**Risk: MEDIUM** — a caps-honesty risk of exactly the `SupportDirectFlip` / `FlipImmediateMmIo`
class.

### 9.12 Debug layer and SDK layers — design in from day one

- `D3D12DDI_CREATE_DEVICE_FLAG_DEBUGGABLE = 0x2` arrives on **both** `pfnCalcPrivateDeviceSize` and
  `pfnCreateDevice` (§1.4).
- `pfnGetDebugAllocationInfo` (umddi:3898-3905) must map any `D3D12DDI_HANDLE_AND_TYPE` to
  `{ VA infos, KMT allocation infos }`. It is one of the 124 slots and it needs a real body as soon
  as anyone runs with the debug layer.
- **Device removal is the runtime's response to `pfnSetErrorCb`:**
  > `Removing device due to bad UMD error.` — fullstrings:22986
  > `Removing device due to driver error.` — strings:247
  > `Removing device due to driver-reported app error.` — strings:248

  So `pfnSetErrorCb` is not a log function. Distinguish *app* errors (bad arguments the app gave)
  from *driver* errors, and count both (`DECISIONS.md` §7.7).
- Independent devices are a caps-adjacent contract (`_0098`: "Enable independent D3D12 devices"):
  > `ID3D12DeviceFactory::CreateDevice: Driver does not support independent devices, a singleton device exists, and the factory was configured to disallow returning an existing device.` — strings:219
  > (and three siblings at strings:220-222)

  A first implementation reports no independent-device support and lives with the singleton
  behaviour; that is a legal, named answer rather than a silent one.

**Risk: MEDIUM.**

---

## 10. Fences

### 10.1 The fence object IS a pair of GPU virtual addresses

Verbatim, umddi:1575-1598:

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
DEFINE_ENUM_FLAG_OPERATORS( D3D12DDI_FENCE_FLAGS );

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

```c
// umddi:1786-1788
typedef SIZE_T ( APIENTRY* PFND3D12DDI_CALCPRIVATEFENCESIZE )( D3D12DDI_HDEVICE, _In_ CONST D3D12DDIARG_CREATE_FENCE* );
typedef HRESULT ( APIENTRY* PFND3D12DDI_CREATEFENCE )( D3D12DDI_HDEVICE, D3D12DDI_HFENCE, _In_ CONST D3D12DDIARG_CREATE_FENCE* );
typedef VOID ( APIENTRY* PFND3D12DDI_DESTROYFENCE )( D3D12DDI_HDEVICE, D3D12DDI_HFENCE );
```

**Read this carefully — it is the least intuitive object in the DDI.**

- **The runtime hands the driver an array of GPU virtual addresses. The driver does not choose
  them, and the driver never gets a `D3DKMT_HANDLE` for the fence.** `D3D12DDIARG_CREATE_FENCE`
  contains only VAs.
- The `FenceValue` / `FenceMonitoredValue` VA pair is exactly the WDDM **monitored fence** shape.
  The runtime created it with `D3DKMTCreateSynchronizationObject2` /
  `D3DDDICB_CREATESYNCHRONIZATIONOBJECT2` and kept the CPU half for itself
  (`tmp/dx12/sdk/d3dukmdt.h:1869-1873`, and the D3DKMT twin at `d3dkmthk.h:1707-1708`):

  ```c
      D3DKMT_PTR(VOID*,       FenceValueCPUVirtualAddress);           // out: Read-only mapping of the fence value for the CPU
      D3DKMT_ALIGN64 D3DGPU_VIRTUAL_ADDRESS FenceValueGPUVirtualAddress; // out: Read/write mapping of the fence value for the GPU
  } MonitoredFence;
  ```

  **The runtime keeps the CPU mapping and the kernel handle; the driver gets the GPU mapping only.**
- `FenceCount > 1` is the multi-adapter (LDA) case — one placement per physical adapter. It pairs
  with `pfnGetImplicitPhysicalAdapterMask` (umddi:2710) and `pfnQueryNodeMap` (umddi:2724). On
  single-adapter Helios `FenceCount` is 1.
- `D3D12DDI_FENCE_FLAG_BOTTOM_OF_PIPE` is the driver being told this fence must be signalled after
  *all* preceding GPU work retires, not at command-processor front-end time.

### 10.2 Queue-level signal and wait — the only two fence operations

Verbatim, umddi:2712-2720:

```c
typedef struct D3D12DDIARG_FENCE_OPERATION
{
    D3D12DDI_HFENCE Fence;
    UINT64 Value;
    UINT PhysicalAdapterMask; // Out: The set of adapters to broadcast the operation to
} D3D12DDIARG_FENCE_OPERATION;

typedef void( APIENTRY* PFND3D12DDI_SIGNAL_FENCE )( D3D12DDI_HCOMMANDQUEUE, D3D12DDIARG_FENCE_OPERATION*);
typedef void( APIENTRY* PFND3D12DDI_WAIT_FOR_FENCE )( D3D12DDI_HCOMMANDQUEUE, D3D12DDIARG_FENCE_OPERATION*);
```

Note `PhysicalAdapterMask` is annotated **`// Out:`** — the *driver* writes it, telling the runtime
which adapters the operation must be broadcast to. On single-adapter Helios that is `1`.

### 10.3 ⚠ There is no CPU-signal DDI, and no CPU-wait DDI

Verified by absence: `grep -c "FROM_CPU\|FromCpu\|FROMCPU" d3d12umddi.h` → **0**. Every fence entry
point in the header is queue-scoped, and there are exactly two of them.

**Therefore `ID3D12Fence::Signal` (the CPU signal), `ID3D12Fence::SetEventOnCompletion` and
`ID3D12Fence::GetCompletedValue` never reach the driver.** The runtime executes them itself against
the monitored fence's CPU mapping and `D3DKMTSignalSynchronizationObjectFromCpu` /
`D3DKMTWaitForSynchronizationObjectFromCpu`. (`SignalSynchronizationObjectFromCpu` is present in
`D3D12Core.dll`'s own string table.) This is a structural argument from the absence of any such
entry point in `DEVICE_FUNCS_CORE_0109` or `COMMAND_QUEUE_FUNCS_CORE_0001`, not a header statement.

**What that means for the forward:** `pfnSignalFence` / `pfnWaitForFence` are *ordering
instructions to the driver's own pipeline* plus an adapter-mask report. The kernel-side
`D3DKMTSignalSynchronizationObjectFromGpu` / `…WaitForSynchronizationObjectFromGpu` are then
performed by the runtime against the driver's context. Confidence high; **UNVERIFIED as a
statement** — see §10.5.

### 10.4 The Helios resolution (`DECISIONS.md` §6) — risk MEDIUM, not HIGH

The question that used to be treated as strategy-deciding was: *with no guest GPU writing
`FenceValueGPUVirtualAddress`, can a monitored fence advance at all on this adapter?*

**It is answered, and the answer is yes.** Microsoft documents the exact fallback for a device that
cannot write a fence VA from the engine
(`windows-driver-docs-pr/display/context-monitoring.md`, `native-gpu-fence-objects.md`):

> *"If a GPU engine isn't capable of writing to a monitored fence using its virtual address, the UMD
> uses the `SignalSynchronizationObjectFromGpuCb` callback to queue a software signal packet"*

and, for the CPU-signal direction, *"Dxgkrnl updates the fence memory location"*. Independently,
`tools/vehicle_flipwait_probe.c` proves the queued-`WAIT(F>=1)`-before-queued-`SIGNAL(G=5)`
primitive **live on this software-scheduled adapter with zero KMD changes**
(`ROADMAP.md:2616` — *"`tools/vehicle_flipwait_probe.c` PROVES the primitive live on our
software-scheduled adapter (queued signal held behind an unsatisfied wait, drained ~10 ms after the
CPU signal; ZERO KMD changes)"*. ⛔ Not `ROADMAP.md:2605-2610`, which is the 25th-session fence-event
A/B — sw 200 fps vs vehicle 120-130 fps and the stale-frame accounting — and does not mention the
probe at all. This citation is load-bearing: it is the evidence for the fence risk being HIGH→MEDIUM
here and in `DECISIONS.md` §6.)

So the D3D12 fence model on Helios is:

| D3D12 concept | Helios mechanism |
|---|---|
| `ID3D12Fence` object | runtime-created monitored fence; driver sees only `D3D12DDI_FENCE`'s VAs |
| `ID3D12CommandQueue::Signal` | `pfnSignalFence` → `pKTCallbacks->pfnSignalSynchronizationObjectFromGpuCb` queued software signal packet on the queue's WDDM context |
| `ID3D12CommandQueue::Wait` | `pfnWaitForFence` → `pfnWaitForSynchronizationObjectFromGpuCb` queued wait on the same context |
| `ID3D12Fence::Signal` / `SetEventOnCompletion` / `GetCompletedValue` | entirely runtime + dxgkrnl; the driver is not involved |
| "signalled after the GPU work in preceding `ExecuteCommandLists`" | reduces to Helios' existing wire-fence contract — `DxgkDdiSubmitCommandVirtual` completes a fence only after the venus work outstanding at submit time (`kmd_render/src/ddi/submit_command.rs:720-724`) |

⛔ **Do not claim `D3D12DDI_FENCE_FLAG_BOTTOM_OF_PIPE` semantics the stack cannot deliver.** The
flag is an input the driver *receives*, so the obligation runs the other way: if a fence carries it,
the queued software signal packet must be ordered behind the frame's own producer completion, not
merely behind submission. That is exactly the `PresentWmk` lesson in fence form (CLAUDE.md's
invariant: *a WDDM fence may wait on the frame's OWN boundary, never on the whole `next_wire_fence`
backlog*).

**Shadow state per `D3D12DDI_HFENCE`:** the `FenceCount` VA pairs, the flags, the
`PhysicalAdapterMask` the driver reports, the last value requested per queue, and — if the forward
needs it — an internal vkd3d `ID3D12Fence` used to obtain the §8.3 watermark.

### 10.5 The residual unknown, stated precisely

**UNVERIFIED — the *D3D12-shaped* fence has not been observed advancing on this adapter.** What is
proven is the *primitive* (`vehicle_flipwait_probe.c`: a queued wait retires behind a queued signal
on a software-scheduled context). What is not proven is that the value the D3D12 runtime reads at
`FenceValueCPUVirtualAddress` moves when dxgkrnl retires a **monitored-fence** signal packet queued
by `pfnSignalSynchronizationObjectFromGpuCb` on a Helios context — i.e. that dxgkrnl performs the
memory write itself rather than relying on a KMD interrupt notification
(`DXGKCB_NOTIFY_INTERRUPT` with `DXGK_INTERRUPT_MONITORED_FENCE_SIGNALED`, which
`kmd_render` does not implement).

**Settling experiment — the G-fence probe. No D3D12 code, no KMD change, ~half a day.**
Write `tools/monitored_fence_probe.c` (a sibling of `tools/vehicle_flipwait_probe.c`) that:

1. `D3DKMTOpenAdapterFromLuid` on the Helios adapter, `D3DKMTCreateDevice`,
   `D3DKMTCreateContextVirtual` (NodeOrdinal 0);
2. `D3DKMTCreateSynchronizationObject2` with `D3DDDI_SYNCHRONIZATION_OBJECT_TYPE::MonitoredFence`,
   capturing `FenceValueCPUVirtualAddress` and `FenceValueGPUVirtualAddress`;
3. `D3DKMTSubmitCommand` an empty DMA buffer on that context;
4. `D3DKMTSignalSynchronizationObjectFromGpu` for value 1 on that context;
5. poll `*(volatile UINT64*)FenceValueCPUVirtualAddress` for ≤ 2 s, and separately
   `D3DKMTWaitForSynchronizationObjectFromCpu`.

- **PASS:** the CPU-visible value reaches 1 with nothing having written the GPU VA. The architecture
  works untouched and §10.4's table is the implementation.
- **FAIL:** it never advances ⇒ `kmd_render` needs a monitored-fence notification path before D3D12
  is possible at all. That is a KMD work item to file in `ROADMAP.md`, not a reason to abandon D1 —
  but it moves onto the critical path, contradicting `DECISIONS.md` D5's "the KMD is not on the
  critical path", so **run this probe before writing fence code**.

⚠ Run it in **session 1** via a cloned scheduled task (`schtasks /run /tn …`) if it opens any
window; a console-only probe is fine from `win_exec`, but check counters *moved this boot*.

**Second UNVERIFIED, cheaper:** whether the runtime — not the driver — performs the kernel
signal/wait for `pfnSignalFence`/`pfnWaitForFence` (§10.3). Settling experiment: run any D3D12 app
on **WARP** (which exports `OpenAdapter12`) and take a `Microsoft-Windows-DxgKrnl` all-keywords ETW
slice around one `ID3D12CommandQueue::Signal`; if `SignalSynchronizationObjectFromGpu` packets
appear on the queue's context with no driver call between, the runtime does it. Recipe in
`ROADMAP.md`. This costs nothing and needs no Helios code.

⚠ **A fact-check note on a string other documents quote.** `research/R2` §2.4 cites
`… must be either monitored fences with GPU access or native fences.` as evidence about D3D12
fences in general. The full line is
`ID3D12VideoEncodeCommandList::EncodeFrame arguments are not supported - SubregionOutputBuffers.ppSubregionFences[%d] must be either monitored fences with GPU access or native fences.`
(fullstrings:20702) — it is a **video-encode** validation string. The conclusion it was used to
support (D3D12 fences are monitored fences or native fences) is independently established by the
`D3D12DDI_FENCE` shape itself; the string is not the evidence.

---

## 11. Caps

`DECISIONS.md` H4: *"the caps gauntlet is a hard gate with ~60 runtime-enforced consistency
rules"*. This section is that gauntlet, enumerated.

### 11.1 `D3D12DDICAPS_TYPE` — all 43 live values

Verbatim, umddi:94-150 (the header's own comments preserved). **43 enumerators** (measured:
`sed -n '95,149p' | grep -cP '^\s+D3D12DDI\w+\s+=\s+\d+,'` → 43), plus one commented-out. Values
1008, 1011, 1014–1056, 1076, 1083 and 1089–1090 are absent.

```c
typedef enum D3D12DDICAPS_TYPE
{
    D3D12DDICAPS_TYPE_TEXTURE_LAYOUT                             = 1000, // Deprecated by D3D12DDICAPS_TYPE_0022_TEXTURE_LAYOUT
    D3D12DDICAPS_TYPE_SWIZZLE_PATTERN                            = 1001, // Deprecated by D3D12DDICAPS_TYPE_0022_SWIZZLE_PATTERN
    D3D12DDICAPS_TYPE_MEMORY_ARCHITECTURE                        = 1002,
    D3D12DDICAPS_TYPE_TEXTURE_LAYOUT_SETS                        = 1003,
    D3D12DDICAPS_TYPE_SHADER                                     = 1004,
    D3D12DDICAPS_TYPE_ARCHITECTURE_INFO                          = 1005,
    D3D12DDICAPS_TYPE_D3D12_OPTIONS                              = 1006,
    D3D12DDICAPS_TYPE_3DPIPELINESUPPORT                          = 1007,

    D3D12DDICAPS_TYPE_GPUVA_CAPS                                 = 1009,
    D3D12DDICAPS_TYPE_TEXTURE_LAYOUT1                            = 1010, // Deprecated by D3D12DDICAPS_TYPE_0022_TEXTURE_LAYOUT

    D3D12DDICAPS_TYPE_0011_SHADER_MODELS                         = 1012,
    D3D12DDICAPS_TYPE_OPTIONS1_0103                              = 1013, // D3D12DDI_OPTIONS1_DATA_0103

    D3D12DDICAPS_TYPE_0030_PROTECTED_RESOURCE_SESSION_SUPPORT    = 1057,
    D3D12DDICAPS_TYPE_0030_CRYPTO_SESSION_SUPPORT                = 1058, // Deprecated, moved to D3D12DDI_CAPS_TYPE_VIDEO

    D3D12DDICAPS_TYPE_0022_CPU_PAGE_TABLE_FALSE_POSITIVES        = 1059,
    D3D12DDICAPS_TYPE_0022_TEXTURE_LAYOUT                        = 1060,
    D3D12DDICAPS_TYPE_0022_SWIZZLE_PATTERN                       = 1061,

    D3D12DDICAPS_TYPE_0023_UMD_BASED_COMMAND_QUEUE_PRIORITY      = 1062,

    D3D12DDICAPS_TYPE_0030_CONTENT_PROTECTION_SYSTEM_COUNT       = 1063, // Deprecated, moved to D3D12DDI_CAPS_TYPE_VIDEO
    D3D12DDICAPS_TYPE_0030_CONTENT_PROTECTION_SYSTEM_SUPPORT     = 1064, // Deprecated, moved to D3D12DDI_CAPS_TYPE_VIDEO
    D3D12DDICAPS_TYPE_0030_CRYPTO_SESSION_TRANSFORM_SUPPORT      = 1065, // Deprecated, moved to D3D12DDI_CAPS_TYPE_VIDEO
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
    D3D12DDICAPS_TYPE_OPTIONS_0091                               = 1078, // D3D12DDI_OPTIONS_DATA_0091
    D3D12DDICAPS_TYPE_OPTIONS_0093                               = 1079, // D3D12DDI_OPTIONS_DATA_0093
    D3D12DDICAPS_TYPE_OPTIONS_0098                               = 1080, // D3D12DDI_OPTIONS_DATA_0098
    D3D12DDICAPS_TYPE_OPTIONS_0101                               = 1081, // D3D12DDI_OPTIONS_DATA_0101
    D3D12DDICAPS_TYPE_OPTIONS_0102                               = 1082, // D3D12DDI_OPTIONS_DATA_0102
    // D3D12DDICAPS_TYPE_OPTIONS_0092                            = 1083, // D3D12DDI_OPTIONS_DATA_0092, this value is used in a serviced version of D3D and therefore cannot be used.
    D3D12DDI_FEATURE_D3D12_PREDICATION_106                       = 1084,
    D3D12DDI_FEATURE_PLACED_RESOURCE_SUPPORT_INFO_106            = 1085,
    D3D12DDI_FEATURE_HARDWARE_COPY_106                           = 1086,
    D3D12DDICAPS_TYPE_OPTIONS_0109                               = 1087, // D3D12DDI_OPTIONS_DATA_0109
    D3D12DDICAPS_TYPE_OPTIONS_0110                               = 1088, // D3D12DDI_OPTIONS_DATA_0110
    D3D12DDICAPS_TYPE_SHADER_MODEL_6_8_OPTIONS_0110              = 1091, // D3D12DDI_SHADER_MODEL_6_8_OPTIONS_0110

} D3D12DDICAPS_TYPE;
```

⚠ Three enumerators are named `D3D12DDI_FEATURE_*`, not `D3D12DDICAPS_TYPE_*` (1084–1086). They are
still `D3D12DDICAPS_TYPE` values passed to `pfnGetCaps`, and the runtime validates them:
`D3D12DDI_FEATURE_D3D12_PREDICATION_106 returned invalid support for current feature level`
(strings:7, and siblings at strings:8-9). A `match` over the enum must include them.

⚠ A **separate** `D3D12DDICAPS_TYPE_VIDEO_0020` enum exists at umddi:4327-4379 for the video
extended feature. A baseline device that reports zero extended features never sees it. Do not merge
the two enums.

#### 11.1a ✅ Measured — which types the runtime actually asks, and with what `DataSize`

`D12-G5`, 2026-08-05, against WARP 10.0.26100.8875 on this guest's own `D3D12Core.dll`. Full table
including the never-asked rows: `tmp/dx12/gates/G5/answers.md` §2.

**23 of the 43 are asked. None of the 7 deprecated ones is. No value outside the 43 ever appeared.**

⭐ **The asked set is identical across all four workloads** (device-only, +queue, +swapchain/present,
+shaders/draw). Caps are answered entirely during adapter open and device creation; nothing an
application does adds a query. So "not asked" here is the strong form, not a workload artefact.

| type | enumerator | `DataSize` | `pInfo` | calls per run |
|---:|---|---:|---|---:|
| 1002 | `MEMORY_ARCHITECTURE` | 20 | `NodeIndex` | 1 |
| 1003 | `TEXTURE_LAYOUT_SETS` | 20 | non-NULL | **3** — an *enumeration*, see below |
| 1004 | `SHADER` | 64 | NULL | 1 |
| 1005 | `ARCHITECTURE_INFO` | 4 | NULL | 1 |
| 1006 | `D3D12_OPTIONS` | **124** | NULL | 1 |
| 1007 | `3DPIPELINESUPPORT` | 4 | NULL | 1 |
| 1009 | `GPUVA_CAPS` | 4 | `NodeIndex` | 1 |
| 1012 | `0011_SHADER_MODELS` | 16 | NULL | 2 |
| 1013 | `OPTIONS1_0103` | 4 | NULL | 1 |
| 1059 | `0022_CPU_PAGE_TABLE_FALSE_POSITIVES` | 4 | non-NULL | 1 |
| 1060 | `0022_TEXTURE_LAYOUT` | 20 | **NULL** | 1 |
| 1062 | `0023_UMD_BASED_COMMAND_QUEUE_PRIORITY` | 4 | NULL | 1 |
| 1067 | `0050_HARDWARE_SCHEDULING_CAPS` | 4 | NULL | 1 |
| 1071 | `0073_SUPPORT_BATCHED_MARKERS` | 4 | NULL | 1 |
| 1074 | `0081_3DPIPELINESUPPORT1` | 8 | NULL | 1 |
| 1077 / 1078 / 1079 | `OPTIONS_0090` / `_0091` / `_0093` | 4 / 16 / 8 | NULL | 1 each |
| 1080 | `OPTIONS_0098` | 4 | NULL | 1 |
| 1082 | `OPTIONS_0102` | 16 | NULL | 1 |
| 1087 / 1088 | `OPTIONS_0109` / `_0110` | 4 / 4 | NULL | 1 each |
| 1091 | `SHADER_MODEL_6_8_OPTIONS_0110` | 8 | NULL | 1 |

**Never asked** (13 live enumerators): 1057, 1061, 1066, 1068, **1069 `EXECUTECOMMANDLISTS_PARALLELISM`**,
1070, 1072, 1073, 1075, 1081, 1084, 1085, 1086.

**Ask order** — and the first two matter structurally:

```
1074, 1007                                   <-- BEFORE pfnGetSupportedVersions, on a bare adapter
  (version negotiation, pfnCalcPrivateDeviceSize, pfnCreateDevice)
1006, 1077, 1078, 1079, 1080, 1013, 1087, 1088, 1004, 1012, 1012
  (pfnGetOptionalDDITables, pfnFillDDITable x5)
1005, 1091, 1062, 1067, 1060, 1002, 1003, 1003, 1003, 1059, 1082
  (the 91-format pfnCheckFormatSupport sweep, pfnQueryNodeMap)
1009, 1071
```

⭐ **`3DPIPELINESUPPORT1` (1074) and `3DPIPELINESUPPORT` (1007) are answered before any version is
negotiated and before any device exists.** `helios_umd12`'s `pfnGetCaps` must answer both without
knowing which DDI version it is speaking.

⭐ **`TEXTURE_LAYOUT_SETS` (1003) is an enumeration, not a query.** The runtime calls it with
`pInfo = {1,0}`, `{1,1}`, `{1,2}` and stops at the first failure. A driver that answers `S_OK`
forever loops it.

### 11.2 The caps a device MUST answer

These are the ones with an explicit "device creation fails" string. **This list is *verified*, not
inferred** — each row is a runtime error message.

| Cap | `pInfo` | `pData` type | Runtime string if unanswered |
|---|---|---|---|
| `_D3D12_OPTIONS` (1006) | NULL | `D3D12DDI_D3D12_OPTIONS_DATA_0089` (31 fields, umddi:11079-11112) | `Driver did not respond to D3D12DDICAPS_TYPE_D3D12_OPTIONS caps query` — strings:27 |
| `_ARCHITECTURE_INFO` (1005) | NULL | `D3D12DDI_ARCHITECTURE_INFO_DATA { BOOL TileBasedDeferredRenderer; }` (umddi:2917-2920) | `Driver did not respond to D3D12DDICAPS_TYPE_ARCHITECTURE_INFO caps query` — strings:26 |
| `_SHADER` (1004) | NULL | `D3D12DDI_SHADER_CAPS_0084` (16 fields, umddi:10516-10535) | `Driver did not respond to D3D12DDICAPS_TYPE_SHADER caps query` — strings:28 |
| `_0011_SHADER_MODELS` (1012) | NULL | `D3D12DDI_D3D12_SHADER_MODELS_DATA_0011` (umddi:3503-3507) | `Driver did not report any supported shader models in D3D12DDICAPS_TYPE_0011_SHADER_MODELS caps query` — strings:24; `Driver did not respond to … with a list of supported shader models.` — strings:25 |
| `_MEMORY_ARCHITECTURE` (1002) | `NodeIndex` (umddi:152-155) | `D3D12DDI_MEMORY_ARCHITECTURE_CAPS_0041` (umddi:6807-6814) | `Driver doesn't respond to D3D12DDICAPS_MEMORY_ARCHITECTURE Caps.` — strings:33 |
| `_TEXTURE_LAYOUT` / `_TEXTURE_LAYOUT_SETS` (1000/1003) | — | `D3D12DDI_TEXTURE_LAYOUT_CAPS_*` | `Driver failed D3D12DDICAPS_TEXTURE_LAYOUT or D3D12DDICAPS_TEXTURE_LAYOUT_SETS Caps.` — strings:34 |
| `_0022_TEXTURE_LAYOUT` (1060) | ⚠ **NULL is a legal `pInfo`** | `D3D12DDI_TEXTURE_LAYOUT_CAPS_0026` (umddi:5529-5536) | `Driver failed D3D12DDICAPS_TYPE_0022_TEXTURE_LAYOUT Caps with NULL pInfo.` — strings:36 |
| `_TEXTURE_LAYOUT1` (1010) | ⚠ NULL is legal | deprecated struct | `Driver failed D3D12DDICAPS_TYPE_TEXTURE_LAYOUT1 Caps with NULL pInfo.` — strings:37 |
| `_0022_CPU_PAGE_TABLE_FALSE_POSITIVES` (1059) | `NodeIndex` | `D3D12DDI_COMMAND_QUEUE_FLAGS` (umddi:1430-1433) | `Driver failed D3D12DDICAPS_TYPE_0022_CPU_PAGE_TABLE_FALSE_POSITIVES Caps.` — strings:35; `Driver responded with invalid bits …` — strings:77 |
| `_GPUVA_CAPS` (1009) | `NodeIndex` | `D3D12DDI_GPUVA_CAPS_0004 { UINT MaxGPUVirtualAddressBitsPerResource; }` (umddi:254-257) | `Driver set MaxGPUVirtualAddressBitsPerResource to 0.` — strings:94 |
| `_0023_UMD_BASED_COMMAND_QUEUE_PRIORITY` (1062) | — | — | `Driver did not correctly respond to …` — strings:22 |
| `_0050_HARDWARE_SCHEDULING_CAPS` (1067) | — | `D3D12DDICAPS_HARDWARE_SCHEDULING_CAPS_0050 { UINT ComputeQueuesPer3DQueue; }` (umddi:7005-7008) | `Driver did not correctly respond to …` — strings:23 |
| `_3DPIPELINESUPPORT` (1007) | NULL | `D3D12DDI_3DPIPELINELEVEL` | selects the feature level; §11.3 |
| `_0081_3DPIPELINESUPPORT1` (1074) | NULL | `D3D12DDI_3DPIPELINESUPPORT1_DATA_0081` (in/out) | §11.3 |
| `_SHADERCACHE_ABI_SUPPORT` | — | — | `Driver failed D3D12DDICAPS_TYPE_SHADERCACHE_ABI_SUPPORT Caps.` — strings:2 ⚠ **this name is not in the 26100 header's enum** — either a newer or a serviced-only value. Treat as "answer `E_INVALIDARG` and count." |

Caps that carry an input through `pInfo` are called out by an inline comment in the header,
e.g. umddi:152-155 for `MEMORY_ARCHITECTURE` (`*pInfo = NodeIndex`) and umddi:250-253 for
`GPUVA_CAPS`. **Always check `pInfo` for NULL before dereferencing** — two of the strings above
exist precisely because the runtime calls with `pInfo == NULL`.

✅ **ANSWERED (`D12-G5`): a failing HRESULT on a caps type is tolerated.** The `_0090` convention
("the runtime will keep requesting from the driver all `D3D12DDI_OPTION` versions whose caps it cares
about", umddi:11125, in the comment block at umddi:11122-11125) suggested "treat as
zeroed/unsupported", and that is what happens. Three independent observations:

* WARP itself answers **`1074 3DPIPELINESUPPORT1`** and **`1080 OPTIONS_0098`** with `E_UNEXPECTED`
  (0x8000ffff) on **every** run, and the device creates every time;
* `1003 TEXTURE_LAYOUT_SETS` is *designed* to end in failure — the runtime enumerates until the
  driver refuses;
* the spy's `capfail` arm returned `E_INVALIDARG` for `1088 OPTIONS_0110` without calling WARP at
  all, and `D3D12CreateDevice` still returned `S_OK`.

⛔ **This does not extend to the ~13 caps with an explicit "device creation fails" string above** —
all of those were answered `S_OK` in every run, so refusing one is untested. The safe rule is
unchanged: **answer every cap you understand; for the rest, zero `pData` up to `DataSize` and return
`S_OK`.** ⛔ Do not return `S_OK` without writing `pData` — the runtime reads whatever was in its
buffer.

### 11.3 Feature level — `3DPIPELINESUPPORT` and its trap

```c
// umddi:2922-2933 — the header's own comment is the whole semantic
// D3D12DDICAPS_TYPE_3DPIPELINESUPPORT
// For D3D12, drivers only report the maximum level they support
typedef enum D3D12DDI_3DPIPELINELEVEL
{
    D3D12DDI_3DPIPELINELEVEL_1_0_GENERIC = 1,
    D3D12DDI_3DPIPELINELEVEL_1_0_CORE = 2,
    D3D12DDI_3DPIPELINELEVEL_11_0 = 10,
    D3D12DDI_3DPIPELINELEVEL_11_1 = 11,
    D3D12DDI_3DPIPELINELEVEL_12_0 = 12,
    D3D12DDI_3DPIPELINELEVEL_12_1 = 13,
    D3D12DDI_3DPIPELINELEVEL_12_2 = 14,
} D3D12DDI_3DPIPELINELEVEL;
```

⛔⛔ **For D3D12 this is a MAXIMUM LEVEL, not a bitmask — the opposite of the D3D11 cap Helios
already ships.** `umd/src/caps.rs:46-47` documents `D3D11DDICAPS_3DPIPELINESUPPORT` as *"a BITMASK
of supported levels"* and builds `FL11_PIPELINE_MASK = LVL_10_0|LVL_10_1|LVL_11_0|LVL_11_1|LVL_12_0
= 0x8F` (`umd/src/caps.rs:57-66`). That is correct for D3D11 (memory: 30th session) and **wrong for
D3D12**: writing `0x8F` into the D3D12 slot reads as "level 143". ⛔ Do not copy `caps.rs`'s
pipeline-mask logic into `helios_umd12`.

⛔ **Also: the retired R908 body reported `D3D12DDI_3DPIPELINELEVEL_1_0_CORE`** — the *compute-only*
level. Do not resurrect that value by copy-paste.

⭐⭐ **The failure mode of not implementing cap 1074 is SILENT, and it costs FL 12_2.**
`D3D12_FeatureLevel12_2.md:116-117` (DirectX-Specs @ `2bd58ca5`, § *"DDI"* → *"Remark"*), verbatim:

> * Versions of Direct3D built into the operating system at or before Manganese (20H2) use 3DPIPELINESUPPORT.
> * Versions of Direct3D built into Iron operating system, or organized as a re-distributable use 3DPIPELINESUPPORT1, and fall back to 3DPIPELINESUPPORT if it fails.

So the modern runtime asks `D3D12DDICAPS_TYPE_0081_3DPIPELINESUPPORT1` (**1074**, umddi:134) **first**,
and on failure falls back to `D3D12DDICAPS_TYPE_3DPIPELINESUPPORT` (**1007**, umddi:103) — which the
header forbids from answering above `12_1`. ⇒ **A `helios_umd12` that does not handle 1074 is capped at
FL 12_1 with no error reported anywhere**, because a failing HRESULT on a caps query is tolerated
(§15.1 #1: WARP itself fails 1074 and 1080 and the device still creates). Both selectors are in the
`D12-G5` asked-set. **Cap 1074 is not optional for the FL 12_2 claim; implement it in the same commit
as the caps answer.**

⚠ **The runtime never infers feature level from the caps set — the driver asserts it.** That means a
12_2 driver must satisfy *both* the explicit assertion here *and*, independently, every cap floor in
§11.5(b); getting the second wrong fails device creation rather than quietly demoting the level.

⚠ **`WriteBufferImmediateSupportFlags`' BUNDLE bit is not the driver's to report.** Same spec, same
section: the runtime switches `D3D12_COMMAND_LIST_SUPPORT_FLAG_BUNDLE` on at the API level for any
driver reporting `D3D12DDI_COMMAND_QUEUE_FLAG_3D` at the DDI. Do not set it.

✅ **The driver-model floor for FL 12_2 is WDDM 2.0**, which `WddmSurface::Wddm2_1GpuMmu` clears with
room to spare — a second data point beside `D12-G5`'s finding that the WDDM level does not gate the DDI
version.

⚠ **A driver must not return anything higher than `12_1` from cap 1007.** The header spells the
reason out at umddi:10360-10377, verbatim:

> "The inbox behavior <= Vibranium, which we can't break, is that that if the driver reports any
> feature levels the runtime doesn't understand- namely, anything higher than 12_1- then the runtime
> will sanitize the feature level down to 1_0 core. Think about the combination "new driver + old
> OS." If a new driver returns a 12_2 3DPIPELINESUPPORT cap to an old OS, the old OS will sanitize
> the value to 1_0 core, which is bad.
> Therefore we mandate that for the 3DPipelineSupport cap, the driver must not return anything
> higher than 12_1.
> There is a new cap 3DPipelineSupport1 for letting the drivers return values higher than 12_1.
> As a future-proofing measure, 3DPipelineSupport1 takes a payload where the runtime passes in the
> maximum feature level it understands. The driver outputs the highest feature level it supports
> that does not exceed what the runtime understands."

```c
// umddi:10415-10420 — the negotiated form
typedef struct D3D12DDI_3DPIPELINESUPPORT1_DATA_0081
{
    D3D12DDI_3DPIPELINELEVEL HighestRuntimeSupportedFeatureLevel; // input
    D3D12DDI_3DPIPELINELEVEL MaximumDriverSupportedFeatureLevel;  // output
} D3D12DDI_3DPIPELINESUPPORT1_DATA_0081;
```

**So the rule is mechanical:**

```
cap 1007  ->  min(driver_max, D3D12DDI_3DPIPELINELEVEL_12_1)
cap 1074  ->  out = min(driver_max, in.HighestRuntimeSupportedFeatureLevel)
```

Below FL 11_0 there are only `1_0_GENERIC` and `1_0_CORE` — compute-only profiles paired with
`D3D12DDICAPS_TYPE_0033_ADAPTER_COMPUTE_ONLY` (1066), which a render+display adapter answers
**FALSE**.

### 11.4 The 16 tiered enums, with their legal values

These are the values `D3D12Core.dll` range-checks one by one. **16 in total**, and the arithmetic is
worth writing out because §16.2 quotes the same figure: **12 live in
`D3D12DDI_D3D12_OPTIONS_DATA_0089`**, one is a bitmask in the same struct
(`WriteBufferImmediateQueueFlags`), one arrived at `_0110` (`ExecuteIndirectTier`), and **two more
live outside the OPTIONS family** (`WaveMMATier`, `WorkGraphsTier`) in the second table below —
12 + 1 + 1 + 2 = 16:

| Field | Enum | umddi | Legal values |
|---|---|---|---|
| `ResourceBindingTier` | `D3D12DDI_RESOURCE_BINDING_TIER` | 694 | `_1=1, _2=2, _3=3` |
| `ConservativeRasterizationTier` | `D3D12DDI_CONSERVATIVE_RASTERIZATION_TIER` | 701 | `NOT_SUPPORTED=0, _1=1, _2=2, _3=3` |
| `TiledResourcesTier` | `D3D12DDI_TILED_RESOURCES_TIER` | 709 | `NOT_SUPPORTED=0, _1=1, _2=2, _3=3` |
| `CrossNodeSharingTier` | `D3D12DDI_CROSS_NODE_SHARING_TIER` | 725 | `NOT_SUPPORTED=0, _1_EMULATED=1, _1=2, _2=3, _0041_3=4` |
| `ResourceHeapTier` | `D3D12DDI_RESOURCE_HEAP_TIER` | 734 | `_1=1, _2=2` |
| `ProgrammableSamplePositionsTier` | `D3D12DDI_PROGRAMMABLE_SAMPLE_POSITIONS_TIER` | 5700 | `NOT_SUPPORTED=0, _1=1, _2=2` |
| `ViewInstancingTier` | `D3D12DDI_VIEW_INSTANCING_TIER` | 6370 | `NOT_SUPPORTED, _1, _2, _3` (implicit 0..3) |
| `RenderPassTier` | `D3D12DDI_RENDER_PASS_TIER` | 7645 | `NOT_SUPPORTED=0, _1=1, _2=2` |
| `RaytracingTier` | `D3D12DDI_RAYTRACING_TIER` | 7683 | `NOT_SUPPORTED=0, _1_0=10, _1_1=11` |
| `VariableShadingRateTier` | `D3D12DDI_VARIABLE_SHADING_RATE_TIER` | 8456 | `NOT_SUPPORTED=0, _1=1, _2=2` |
| `MeshShaderTier` | `D3D12DDI_MESH_SHADER_TIER` | 9353 | `NOT_SUPPORTED=0, _1=10` |
| `SamplerFeedbackTier` | `D3D12DDI_SAMPLER_FEEDBACK_TIER` | 9359 | `NOT_SUPPORTED=0, _0_9=90, _1_0=100` |
| `WriteBufferImmediateQueueFlags` | `D3D12DDI_COMMAND_QUEUE_FLAGS` (bitmask) | 1435 | see §8.1 |
| `ExecuteIndirectTier` (`D3D12DDI_OPTIONS_DATA_0110`) | `D3D12DDI_EXECUTE_INDIRECT_TIER` | 13659 | `_1_0=10, _1_1=11` |

⚠ **Note the non-contiguous encodings** — `MESH_SHADER_TIER_1 = 10`, `SAMPLER_FEEDBACK_TIER_0_9 =
90`, `RAYTRACING_TIER_1_0 = 10`. A `tier as u32 >= 1` test is wrong for all three. Compare against
the named constants.

⚠ **"16" counts the values `D3D12Core.dll` range-checks, not the header's `*_TIER` enums.** The header
declares **18** non-video `D3D12DDI_*_TIER` enums (21 including video); the three not listed above are
`HEAP_SERIALIZATION_TIER_0041`, `RESOURCE_SERIALIZATION_TIER_0041` and `RECREATE_AT_TIER` (§9.7), none
of which has a `Driver filled out an invalid value in …` string. Both figures are right about different
things — do not "fix" one to the other. ⛔ `DX12.md` risk 2 previously said **14**; that was simply
wrong and is corrected to 16.

#### 11.4.1 ⛔ `TiledResourcesTier`: the engine reports 4, the DDI cannot express it, and the runtime
will not tell you

`D3D12DDI_TILED_RESOURCES_TIER` stops at `_TIER_3 = 3` in SDK 26100 (umddi:709-715). A live vkd3d
device on this guest reports **`TiledResourcesTier 4`** at the API (`D12-G1`). Tier 4 is gated to
**DDI 0117** — eleven revisions above the `_0110` Helios negotiates — and
`D3D12TiledResourceTier4.md:43` says Microsoft gated it deliberately so older drivers cannot light it
up untested.

⇒ **`helios_umd12` must clamp vkd3d's answer to `_TIER_3` when filling the caps struct.** Writing `4`
into a field whose enum stops at 3 hits the measured `D12-G5` rule that an **out-of-range tier is
clamped silently** (tier 99 → the app sees 3, debug layer included) — so the bug would not announce
itself; it would just be a driver shipping a number nobody chose. ⛔ This is the exact shape of
CLAUDE.md rule 8: the clamp must be explicit, at the site, with the reason in a comment — never left to
the runtime.

⚠ **The same pattern, opposite direction, for `SamplerFeedbackTier`:** `SamplerFeedback.md:79` says
sampler feedback *"is not required as part of a Direct3D feature level"*, but that sentence predates
FL 12_2 and is **stale** — `D3D12_FeatureLevel12_2.md:48` lists Tier 0.9 as an FL 12_2 requirement and
vkd3d gates FL 12_2 on it. Helios' measured 12_2 is therefore already standing on a sampler-feedback
claim. Forward `D3D12DDI_SAMPLER_FEEDBACK_TIER_0_9 = 90` — **not** 100.

**Two more tiered values live outside the OPTIONS family and are validated identically:**

| Field | Enum | umddi | Legal values |
|---|---|---|---|
| `WaveMMATier` (in `D3D12DDI_SHADER_CAPS_0084`) | `D3D12DDI_WAVE_MMA_TIER` | 10436 | `NOT_SUPPORTED=0, _0_5_EXPERIMENTAL=5` |
| `WorkGraphsTier` (in `D3D12DDI_OPTIONS_DATA_0103`) | `D3D12DDI_WORK_GRAPHS_TIER` | 10537 | `NOT_SUPPORTED=0, _0_1=01, _1_0=10` |

**The runtime's own range-check messages — 15 of them, verbatim** (one per tiered enum except
`WriteBufferImmediateQueueFlags`, whose bitmask gets its own string at strings:70 instead — see
§11.5h):

> `Driver filled out an invalid value in D3D12DDI_D3D12_OPTIONS_DATA::ResourceBindingTier.` — strings:45
> `… ::ConservativeRasterizationTier.` — strings:39
> `… ::TiledResourcesTier.` — strings:48
> `… ::CrossNodeSharingTier.` — strings:40
> `… ::ResourceHeapTier.` — strings:46
> `… ::ProgrammableSamplePositionsTier.` — strings:42
> `… ::ViewInstancingTier.` — strings:50
> `… ::RenderPassesTier.` — strings:44
> `… ::RaytracingTier.` — strings:43
> `… ::VariableShadingRateTier.` — strings:49
> `… ::MeshShaderTier.` — strings:41
> `… ::SamplerFeedbackTier.` — strings:47
> `Driver filled out an invalid value in D3D12DDI_D3D12_OPTIONS_110::ExecuteIndirectTier.` — strings:38
> `Driver filled out an invalid value in D3D12DDI_OPTIONS1_DATA_0103::WorkGraphsTier` — strings:51
> `Driver filled out an invalid value in D3D12DDI_SHADER_CAPS_*::WaveMMATier` — strings:52

`D3D12DDI_D3D12_OPTIONS_DATA_0089` in full (umddi:11079-11112) — **31 fields**:

```c
typedef struct D3D12DDI_D3D12_OPTIONS_DATA_0089
{
    D3D12DDI_RESOURCE_BINDING_TIER ResourceBindingTier;
    D3D12DDI_CONSERVATIVE_RASTERIZATION_TIER ConservativeRasterizationTier;
    D3D12DDI_TILED_RESOURCES_TIER TiledResourcesTier;
    D3D12DDI_CROSS_NODE_SHARING_TIER CrossNodeSharingTier;
    BOOL VPAndRTArrayIndexFromAnyShaderFeedingRasterizerSupportedWithoutGSEmulation;
    BOOL OutputMergerLogicOp;
    D3D12DDI_RESOURCE_HEAP_TIER ResourceHeapTier;
    BOOL DepthBoundsTestSupported;
    D3D12DDI_PROGRAMMABLE_SAMPLE_POSITIONS_TIER ProgrammableSamplePositionsTier;
    BOOL CopyQueueTimestampQueriesSupported;
    D3D12DDI_COMMAND_QUEUE_FLAGS WriteBufferImmediateQueueFlags;
    D3D12DDI_VIEW_INSTANCING_TIER ViewInstancingTier;
    BOOL BarycentricsSupported;
    BOOL ReservedBufferPlacementSupported; // Actually just 64KB aligned MSAA support
    BOOL Deterministic64KBUndefinedSwizzle;
    BOOL SRVOnlyTiledResourceTier3;
    D3D12DDI_RENDER_PASS_TIER RenderPassTier;
    D3D12DDI_RAYTRACING_TIER RaytracingTier;
    D3D12DDI_VARIABLE_SHADING_RATE_TIER VariableShadingRateTier;
    BOOL PerPrimitiveShadingRateSupportedWithViewportIndexing;
    BOOL AdditionalShadingRatesSupported;
    UINT ShadingRateImageTileSize;
    BOOL BackgroundProcessingSupported;
    D3D12DDI_MESH_SHADER_TIER MeshShaderTier;
    D3D12DDI_SAMPLER_FEEDBACK_TIER SamplerFeedbackTier;
    BOOL DriverManagedShaderCachePresent;
    BOOL MeshShaderSupportsFullRangeRenderTargetArrayIndex;
    BOOL VariableRateShadingSumCombinerSupported;
    BOOL MeshShaderPerPrimitiveShadingRateSupported;
    BOOL MSPrimitivesPipelineStatisticIncludesCulledPrimitives;
    BOOL EnhancedBarriersSupported;
} D3D12DDI_D3D12_OPTIONS_DATA_0089;
```

`D3D12DDI_SHADER_CAPS_0084` in full (umddi:10516-10535) — **16 fields**:

```c
typedef struct D3D12DDI_SHADER_CAPS_0084
{
    D3D12DDI_SHADER_MIN_PRECISION MinPrecision; // Bitmask: NONE=0x0, 10_BIT=0x1, 16_BIT=0x2 (umddi:2898)
    BOOL DoubleOps;                 BOOL ShaderSpecifiedStencilRef;
    BOOL TypedUAVLoadAdditionalFormats;         BOOL ROVs;
    BOOL WaveOps;
    UINT WaveLaneCountMin;          UINT WaveLaneCountMax;      UINT TotalLaneCount;
    BOOL Int64Ops;                  BOOL Native16BitOps;
    BOOL AtomicInt64OnTypedResource;            BOOL AtomicInt64OnGroupShared;
    BOOL DerivativesInMeshAndAmplificationShaders;
    D3D12DDI_WAVE_MMA_TIER WaveMMATier;
    BOOL AtomicInt64OnDescriptorHeapResource;
} D3D12DDI_SHADER_CAPS_0084;
```

> `Driver did not set valid WaveLaneCountMin/Max or TotalLaneCount via D3D12DDICAPS_TYPE_SHADER caps query` — strings:29

### 11.5 The cross-check rules the runtime enforces

#### 11.5.0 ✅ Measured (`D12-G5`): which of these are RETAIL gates, and which are not

The strings below are extracted from `D3D12Core.dll`. Whether the retail runtime actually enforces
them was inference until `D12-G5` answered caps through the spy and watched what happened. Three
distinct behaviours, and conflating them is the easy mistake:

| what the driver does | what the retail runtime does |
|---|---|
| **an inconsistent SET of caps** (a tier that requires a shader model the driver did not list; a feature level that requires a tier it did not claim) | ⛔ **`D3D12CreateDevice` FAILS**, `DXGI_ERROR_DRIVER_INTERNAL_ERROR` (0x887A0020), with the matching English string on ETW **`Microsoft-Windows-Direct3D12`** |
| **an out-of-range tier value** (`ResourceBindingTier = 99`) | ⚠ **silently CLAMPED to the maximum legal value.** The device creates and `CheckFeatureSupport` reports the app tier **3**. The debug layer changed nothing. The fifteen `Driver filled out an invalid value in D3D12DDI_D3D12_OPTIONS_DATA::<Tier>` strings are **not** retail device-creation gates |
| **a legal but lower answer** (`ResourceBindingTier = 2`) | ✅ propagates **verbatim** to `CheckFeatureSupport`. The driver's caps answer *is* what the application sees |

Two worked failures, both on the retail path with no debug layer, both reproducible:

> `FL12+ driver incorrectly did not report support for resource binding tier 2+.`
> — forcing `ResourceBindingTier = 1` while `3DPIPELINESUPPORT` says FL 12_1

> `Drivers that expose AtomicInt64OnTypedResource, AtomicInt64OnGroupShared, AtomicInt64OnDescriptorHeapResource, DerivativesInMeshAndAmplificationShaders or WaveMMATier must expose shader model 6.6.`
> — clamping the `_0011_SHADER_MODELS` list to 6.5 while leaving `OPTIONS1_0103` alone

⛔ **The clamp is the dangerous half, not the failure.** A wrong tier does not become a loud error;
it becomes a *wrong advertised tier*, which is CLAUDE.md's "advertising a capability that is not
backed" with the loud failure removed. Answer in range, and answer consistently.

**ETW recipe** — ⚠ `Microsoft-Windows-DxgKrnl` / `AzureTriage` contributed **nothing** here; the
failure is above dxgkrnl. The provider that answers for D3D12 is `Microsoft-Windows-Direct3D12`,
exactly as `Microsoft-Windows-DXGI` was for the D3D11 feature-level work (30th session):

```powershell
logman create trace helios_d12 -p Microsoft-Windows-Direct3D12 0xFFFFFFFFFFFFFFFF 0xff -o x.etl -ets
logman update helios_d12 -p Microsoft-Windows-DXGI 0xFFFFFFFFFFFFFFFF 0xff -ets
# ... run the probe ...
logman stop helios_d12 -ets ; tracerpt x.etl -o x.xml -of XML -y   # read <Data Name="Message">
```

**(a) Shader-model coupling — 11 rules, verbatim (strings:116-126).** A tier claim *requires* a
shader model in the `_0011_SHADER_MODELS` list.

> `Drivers that support raytracing must expose shader model 6.3.` — strings:122
> `Drivers that support raytracing tier 1.1 must expose shader model 6.5.` — strings:123
> `Drivers that support mesh shader 1.0 must expose shader model 6.5.` — strings:120
> `Drivers that support sampler feedback tier 1.0 must expose shader model 6.5.` — strings:124
> `Drivers that support variable shading rate tier 2+ must expose shader model 6.4.` — strings:125
> `Drivers that report BarycentricsSupported = TRUE must expose shader model 6.1.` — strings:117
> `Drivers that support D3D12DDI_VIEW_INSTANCING_TIER_1 or greater must expose shader model 6.1.` — strings:119
> `Drivers that support Native16BitOps must expose shader model 6.2.` — strings:121
> `Drivers that expose AtomicInt64OnTypedResource, AtomicInt64OnGroupShared, AtomicInt64OnDescriptorHeapResource, DerivativesInMeshAndAmplificationShaders or WaveMMATier must expose shader model 6.6.` — strings:116
> `Drivers that support AdvancedTextureOpsSupported must expose shader model 6.7.` — strings:118
> `Drivers that support WriteableMSAATexturesSupported must expose shader model 6.7.` — strings:126

⭐ **Read those together with §11.7's SM ceiling: at SM 6.0 the driver MUST report
`RaytracingTier = NOT_SUPPORTED`, `MeshShaderTier = NOT_SUPPORTED`,
`SamplerFeedbackTier = NOT_SUPPORTED`, `VariableShadingRateTier <= 1`,
`BarycentricsSupported = FALSE`, `ViewInstancingTier = NOT_SUPPORTED`, `Native16BitOps = FALSE`, and
all four Atomic/Derivatives/WaveMMA flags FALSE — regardless of what the Vulkan substrate can
do.** This is the single largest simplification available to a first implementation, and it is
forced, not chosen.

**(b) Feature-level floors — 23 rules, verbatim (strings:168-190).** A feature-level claim *requires*
a set of tiers.

> `FL 12+ driver incorrectly did not report support for typed UAV load additional formats.` — strings:169
> `FL12+ driver incorrectly did not report support for resource binding tier 2+.` — strings:189
> `FL12+ driver incorrectly did not report support for tiled resources tier 2+.` — strings:190
> `FL 12.1+ driver incorrectly does not report support for Raster Order Views (ROVs).` — strings:168
> `FL12.1+ driver incorrectly did not report support for conservative rast tier 1+.` — strings:170

and, for FL 12_2, **eighteen** requirements at strings:171-188 — GPU VA ≥ 40 bits, 64-bit integer
shader ops, conservative raster ≥ tier 3, depth bounds, WriteBufferImmediate on direct+compute+
bundle, mesh shader ≥ tier 1, output-merger logic ops, raytracing ≥ tier 1.1, resource binding ≥
tier 3, root signature ≥ 1.1, sampler feedback ≥ tier 0.9, **shader model ≥ 6_5**,
`CastingFullyTypedFormatSupported`, `CopyQueueTimestampQueriesSupported`,
`VPAndRTArrayIndexFromAnyShaderFeedingRasterizerSupportedWithoutGSEmulation`, tiled resources ≥
tier 3, VRS ≥ tier 2, wave ops.

**So feature level is not a free parameter.** Pick the *highest* level whose entire floor you can
honestly meet, then report exactly that. `DECISIONS.md` H5 and §11.7 say what that is today.

**(c) Tier ↔ table consistency.** For the optional Render Pass table:

> `Driver reported TIER_1 or greater Render Pass support despite not implementing DDI table.` — strings:73
> `Driver reported TIER_NOT_SUPPORTED despite implementing DDI table.` — strings:74

**Both directions are errors** — reporting a tier without the table *and* providing the table
without the tier. That is a genuinely new pattern versus D3D11.

**(d) VRS tile size.**

> `Driver reported VRS TIER_1 or greater, but did not provide a valid tile size.` — strings:75
> `Driver reported VRS TIER_2, but did not provide a valid nonzero tile size.` — strings:76

(`ShadingRateImageTileSize` in `OPTIONS_DATA_0089`.)

**(e) Memory architecture coherency rules.** Both are absolute:

> `Driver set D3D12DDICAPS_MEMORY_ARCHITECTURE::CacheCoherent TRUE along with D3D12DDICAPS_MEMORY_ARCHITECTURE::UMA FALSE. CacheCoherent is only a property of UMA systems, which don'tbenefit from the usage of write-combine.` — strings:88
> `Driver set D3D12DDICAPS_MEMORY_ARCHITECTURE::IOCoherent FALSE on an x86 or amd64 system.PCIe support IOCoherence, and the hardware must use it. UMA systems must set TRUE, to avoid the runtimeflushing the CPU cache manually.` — strings:89

⇒ On Helios (amd64 guest): **`IOCoherent = TRUE` is mandatory**, and `CacheCoherent` may only be
TRUE if `UMA` is TRUE. Whether Helios reports `UMA = TRUE` is a *substrate* decision that must match
the two-segment topology the KMD advertises; `docs/dx12/SUBSTRATE.md` owns it. `MemoryPool L0/L1`
(umddi:301) and heap-type behaviour follow from it.

**(f) Texture-layout ↔ KMD linkage.** One string reaches across the UM/KM boundary:

> `Driver set D3D12DDICAPS_TEXTURE_LAYOUT::SupportsRowMajorTexture but not DXGK_VIDMMCAPS::CrossAdapterResourceTexture.` — strings:92

⇒ If `helios_umd12` ever answers `SupportsRowMajorTexture = TRUE` in
`D3D12DDI_TEXTURE_LAYOUT_CAPS_0026` (umddi:5529-5536), `kmd_render` must also set
`DXGK_VIDMMCAPS::CrossAdapterResourceTexture`. **This is the only known cap in the D3D12 set with a
KMD counterpart, and it is exactly the class of coupling that produced the AddAdapter Code 43
lesson.** Check `kmd_render/src/ddi/query_adapter_info.rs` before flipping it.

Related bounds: `Driver set D3D12DDICAPS_TEXTURE_LAYOUT::DeviceDependentLayoutCount too large.`
(strings:90), `…DeviceDependentSwizzleCount too large.` (strings:91),
`Driver uses indexable swizzle patterns, but returned an out of range ColumnOffset.` (strings:112).
A driver that reports `DeviceDependentLayoutCount = 0`, `DeviceDependentSwizzleCount = 0`,
`Supports64KStandardSwizzle = FALSE`, `SupportsRowMajorTexture = FALSE`,
`IndexableSwizzlePatterns = FALSE` clears all four.

**(g) Shader-model list rules.**

> `Driver cannot have gaps in reported support for release shader models in D3d12DDICAPS_TYPE_0011_SHADER_MODELS caps query.` — strings:19
> `For now, driver must include shader model 5.1 support in the list of shader models returned via D3D12DDICAPS_TYPE_0011_SHADER_MODELS caps query.` — strings:191

⭐ **`D3D12DDI_SHADER_MODEL_5_1_RELEASE_0011` (0x00050015) is mandatory in the list**, and the list
must be gapless. So the answer for an SM-6.0 driver is exactly
`{ 5_1_RELEASE_0011, 6_0_RELEASE_0011 }` — not `{ 6_0_RELEASE_0011 }`.

**(h) Miscellaneous.**

> `Driver claimed MSAA support when it shouldn't` — strings:20
> `Driver reported insufficient sample counts for no-output rendering` — strings:69
> `Driver reported invalid WriteBufferImmediate support flags.` — strings:70
> `Driver returned invalid pipeline caps` — strings:78
> `Driver specified a non-identity node remapping with more than 1 API-visible node` — strings:104
> `Driver specified duplicate API index in node remapping` — strings:105
> `Driver specified invalid API index in node remapping` — strings:107
> `Driver specified incompatible cross-node sharing tier` — strings:106
> `Driver specified unrecognized cross-node sharing tier` — strings:108

The four node-remapping strings constrain `pfnQueryNodeMap`. Helios has one node: write the identity
map (`pMap[0] = 0`) and `pfnGetImplicitPhysicalAdapterMask` returns `1`.
`D3D12DDI_NODE_MAP_HIDE_NODE 0xffffffff` (umddi:2722) is the "hide this node" sentinel — do not use
it with one node.

**UNVERIFIED: whether the runtime cross-validates the whole caps set as ONE contract**, the way
D3D11's `CDevice::LLOCompleteLayerConstruction` does (`umd/src/caps.rs:39-42` records that a partial
D3D11 caps edit is rejected with `DXGI_ERROR_UNSUPPORTED`). The 34 cross-check strings in this
section say *individual* rules are enforced; nothing says they are evaluated together or in what
order. Settling experiment: §15's spy, answering deliberately inconsistent caps and reading the ETW
`Microsoft-Windows-DxgKrnl` → `AzureTriage` reason (recipe in `ROADMAP.md`).

### 11.6 The three caps that must be pinned conservatively from commit 1

Each has a named Helios reality behind it and each is a silent-corruption or bugcheck hazard.

| Cap | Value | Why, with the citation |
|---|---|---|
| `D3D12DDICAPS_HARDWARE_SCHEDULING_CAPS_0050.ComputeQueuesPer3DQueue` | **0** | "0 means don't use scheduling groups" (umddi:7007). `DxgkDdiCreateHwQueue` returns `STATUS_NOT_SUPPORTED` and records `HwQRef` (`kmd_render/src/ddi/scheduler.rs:180-187`); non-zero lands on the VidSch `0x119`/Arg1=2 bugcheck. |
| `D3D12DDICAPS_TYPE_EXECUTECOMMANDLISTS_PARALLELISM` | **FALSE** | `pData = BOOL` (umddi:128). TRUE plausibly tells the runtime it may drive `ExecuteCommandLists` concurrently on multiple threads; Helios has one 3D node and a single-context submit path. ⚠ **UNVERIFIED contract** — the exact meaning is not stated in the header, in `D3D12Core.dll`'s strings (no string names this cap at all), or in any driver-docs page found. It is §15.1 item 15; the settling experiment is spelled out immediately below, not left as "go look it up". |

**Settling experiment for `EXECUTECOMMANDLISTS_PARALLELISM` (§15.1 item 15) — two arms, both
concrete, neither needing Helios code.** Run them in this order; arm 1 alone is usually enough.

*Arm 1 — read Microsoft's own answer out of WARP, via the §15.2 spy.* The spy's `pfnGetCaps` hook
already logs `Type`, `DataSize`, `pInfo`, the `HRESULT` and the first 64 bytes of `pData` **after**
the call. Filter its log for the decimal cap id:

```powershell
# win_exec, after a spy run
Select-String -Path C:\ProgramData\Helios\d3d12_spy-*.log -Pattern 'GetCaps Type=1069'
```

A line with `HRESULT=0x0` and a `pData` first dword of `00000000` means WARP — a *software*
rasterizer with no engine parallelism to expose — reports FALSE, which is the conservative answer
Helios copies with a documented precedent instead of a guess. `HRESULT != S_OK` means the cap is
optional and unanswered is legal, which is a stronger result: it removes the cap from the
must-answer list in §11.2.

*Arm 2 — observe what TRUE actually changes, on WARP.* Flip the spy's `pfnGetCaps` thunk to
**overwrite** WARP's answer for `Type == 1069` with `TRUE` after the call returns (one line: write
`*(BOOL*)pData = TRUE`), then take a DxgKrnl slice around a multi-threaded `ExecuteCommandLists`
workload and compare against the unmodified arm:

```powershell
# win_exec — ~2 s circular slice, the ROADMAP.md recipe verbatim
logman create trace helios_ecl -p Microsoft-Windows-DxgKrnl 0xFFFFFFFFFFFFFFFF 0xFF `
  -o C:\Users\Rupansh\ecl.etl -ets
# ... run the workload via schtasks (session 1) ...
logman stop helios_ecl -ets
tracerpt C:\Users\Rupansh\ecl.etl -o C:\Users\Rupansh\ecl.xml -of XML
```

Then grep the dump for `QueuePacket` / `DmaPacket` events and ask one question: **do two
`QueuePacket` submits on distinct threads overlap in time on the same context?** If they do only in
the TRUE arm, the cap is exactly "the runtime may call `pfnExecuteCommandLists` re-entrantly", and
FALSE is mandatory for Helios' single-context submit path. If both arms look identical, the cap is
advisory and FALSE costs nothing — still the right answer, now for a recorded reason.

⛔ Arm 2 modifies what the spy reports to the runtime. It is a WARP-only experiment; run it under
the `UmdD3D12Spy` gate and the three mitigations in §15.2, and re-verify the desktop with
`helios_paintcap` → `Z:\tmp\screen_copy.png` afterwards.
| `D3D12DDI_D3D12_OPTIONS_DATA_0089.Deterministic64KBUndefinedSwizzle` / `TEXTURE_LAYOUT_CAPS_0026.Supports64KStandardSwizzle` | **FALSE** | If TRUE, applications write texture tiles CPU-side in the standard 64 KiB swizzle and expect the GPU to read them back identically. On Helios the real layout is chosen **host-side** by venus/NVIDIA and is not knowable to the guest ⇒ garbage texels with **no error path at all**. vkd3d hardcodes `StandardSwizzle64KBSupported` FALSE (`libs/vkd3d/device.c:10184`) — copy that. |

**The general rule, in one sentence** (the D3D12 form of the `SupportDirectFlip` /
`FlipImmediateMmIo` landmine): *the caps that must be under-reported are the ones that change what
the application writes into memory — swizzle, tiling, heap tier, typed-UAV formats and lane counts —
because on this stack the guest does not own the layout and cannot detect the mismatch; the caps
that may be over-reported "safely" are the ones that produce an HRESULT.*

Ranked hazards beyond the three above (from `research/R8` §6, each with its Helios reality):

1. `TiledResourcesTier >= 1` without a real tile-mapping backend — reserved resources are defined by
   GPU-VA remapping and `kmd_render/src/ddi/gpummu.rs:1-14` says the guest page tables are
   *decorative*. Reads from non-resident tiles must return zero; a no-op mapping returns whatever
   was there, and `UpdateTileMappings` has no failure return the app can see.
2. `ROVsSupported = TRUE` without real fragment-shader interlock — blended/OIT results become
   non-deterministically wrong **and frame-rate dependent**, the hardest possible bug to attribute
   (this project already burned four sessions on one: memory 58th, "0ab-B scales with FRAME RATE").
3. `ConservativeRasterizationTier >= 3` ⇒ `SV_InnerCoverage` must be meaningful.
4. `TypedUAVLoadAdditionalFormats = TRUE` for a format the backend cannot type-load — garbage loads,
   no error.
5. `WaveLaneCountMin/Max/TotalLaneCount`. ⚠ **Already wrong today, whichever strategy wins**: vkd3d
   falls back to `TotalLaneCount = 32 * subgroupSize` = **1024** with a `WARN` because venus exposes
   neither `VK_AMD_shader_core_properties` nor `VK_NV_shader_sm_builtins`
   (`vkd3d-proton-helios/libs/vkd3d/device.c:10226-10233`). Apps that size persistent-thread pools
   off it under-occupy by ~24×. A performance lie, not a correctness one — but it will be blamed on
   the transport. File it in `ROADMAP.md` now.
6. `ResourceHeapTier 2` when the heap cannot hold all three resource categories — aliased placed
   resources overlap.

### 11.7 The shader-model ladder against the Helios substrate

`D3D12DDI_SHADER_MODEL`, verbatim (umddi:3478-3500) — note the EXPERIMENTAL/RELEASE pairing, where
release values are `+5`:

```c
typedef enum D3D12DDI_SHADER_MODEL
{
    D3D12DDI_SHADER_MODEL_5_1_RELEASE_0011      = 0x00050015,
    D3D12DDI_SHADER_MODEL_6_0_EXPERIMENTAL_0011 = 0x00060000,   D3D12DDI_SHADER_MODEL_6_0_RELEASE_0011 = 0x00060005,
    D3D12DDI_SHADER_MODEL_6_1_EXPERIMENTAL_0033 = 0x00060010,   D3D12DDI_SHADER_MODEL_6_1_RELEASE_0033 = 0x00060015,
    D3D12DDI_SHADER_MODEL_6_2_EXPERIMENTAL_0042 = 0x00060020,   D3D12DDI_SHADER_MODEL_6_2_RELEASE_0042 = 0x00060025,
    D3D12DDI_SHADER_MODEL_6_3_EXPERIMENTAL_0054 = 0x00060030,   D3D12DDI_SHADER_MODEL_6_3_RELEASE_0054 = 0x00060035,
    D3D12DDI_SHADER_MODEL_6_4_EXPERIMENTAL_0054 = 0x00060040,   D3D12DDI_SHADER_MODEL_6_4_RELEASE_0062 = 0x00060045,
    D3D12DDI_SHADER_MODEL_6_5_EXPERIMENTAL_0062 = 0x00060050,   D3D12DDI_SHADER_MODEL_6_5_RELEASE_0071 = 0x00060055,
    D3D12DDI_SHADER_MODEL_6_6_EXPERIMENTAL_0071 = 0x00060060,   D3D12DDI_SHADER_MODEL_6_6_RELEASE_0082 = 0x00060065,
    D3D12DDI_SHADER_MODEL_6_7_EXPERIMENTAL_0082 = 0x00060070,   D3D12DDI_SHADER_MODEL_6_7_RELEASE_0093 = 0x00060075,
    D3D12DDI_SHADER_MODEL_6_8_EXPERIMENTAL_0093 = 0x00060080,   D3D12DDI_SHADER_MODEL_6_8_RELEASE_0108 = 0x00060085,
    D3D12DDI_SHADER_MODEL_6_9_EXPERIMENTAL_0108 = 0x00060090,
} D3D12DDI_SHADER_MODEL;

// umddi:3503-3507 — ⚠ BOTH members are POINTERS. This is a two-call query, not a struct out-param.
typedef struct D3D12DDI_D3D12_SHADER_MODELS_DATA_0011
{
    UINT* pNumShaderModelsSupported;
    _Field_size_opt_(*pNumShaderModelsSupported) D3D12DDI_SHADER_MODEL* pShaderModelsSupported;
} D3D12DDI_D3D12_SHADER_MODELS_DATA_0011;
```

⚠ `pShaderModelsSupported` is `_Field_size_opt_`, i.e. it may be **NULL** on the counting call.
Write `*pNumShaderModelsSupported` first; only fill the array if the pointer is non-NULL, and never
write more entries than the count the runtime passed in.

**Where the ceiling comes from (`DECISIONS.md` H5, `research/R8` §4.3).** Both D3D12 arms — the DDI
arm and the app-local vkd3d arm — go through the same dxil-spirv over the same venus ICD, so the
shader-model ceiling is a **substrate** fact, not a strategy choice. vkd3d gates SM 6.2 on FP32
denorm control and exempts only `VK_DRIVER_ID_NVIDIA_PROPRIETARY`
(`vkd3d-proton-helios/libs/vkd3d/device.c:10694-10711`); the guest reports
`driverID = DRIVER_ID_MESA_VENUS` with `shaderDenormPreserveFloat32 = false` and
`shaderDenormFlushToZeroFloat32 = false`
(`docs/dx12/research/guest-vulkaninfo-full.txt:711, 725, 728`). Every SM above 6.2 is chained off
6.2, so the whole ladder above 6.0 is dead — **unless** the `VK_KHR_maintenance7` layered-driverID
swizzle fires (`device.c:2657-2664`, running at `:4129`, well before shader-model caps init at
`:11599`) and rewrites `driverID` to the host NVIDIA one. The guest has `maintenance7` and reports
`layeredApiCount = 1`.

**Verified ordering; unobserved outcome.** `DECISIONS.md` §5 names the ~40-line read-only probe
(`tools/vk_layered_driverid_probe.cpp`) that settles it. **Plan the caps table for SM 6.0 and
expect 6.6.**

⚠ **The canonical phrasing of the ceiling, from `DECISIONS.md` §6.1 — use it verbatim and do not
re-derive a third number:** *"SM 6.6 at minimum, and `SUBSTRATE.md` §7.1 walks vkd3d's ladder to
6.7"*. `SUBSTRATE.md` §7.1 walks `device.c:10640-10826` against the live guest and reaches **6.7**;
the `shader_model_67` profile's single miss (`VK_KHR_maintenance8`) is a *profile* entry the code
does not gate on. **All of it is downstream of H5**, so the planning rule is unchanged: build the
caps table for 6.0 and treat everything above as upside until the `driverID` probe has actually run.

| If the probe prints | Report in `_0011_SHADER_MODELS` | Report in `3DPIPELINESUPPORT` / `…1` | Forced tier consequences |
|---|---|---|---|
| `MESA_VENUS` or 0 | `{ 5_1_RELEASE, 6_0_RELEASE }` | `12_1` / `12_1` | raytracing, mesh, sampler feedback, VRS≥2, barycentrics, view instancing, Native16BitOps, all Atomic64/WaveMMA ⇒ **NOT_SUPPORTED / FALSE** (§11.5a) |
| `NVIDIA_PROPRIETARY` | `{ 5_1, 6_0, 6_1, …, 6_6 }` gapless — and **`…, 6_7` if `SUBSTRATE.md` §7.1's ladder walk is confirmed against the live device** | `12_1` / up to `12_2` | 12_2 unlocks only if all 18 FL12.2 floors in §11.5b are met — check each. Claiming 6_7 additionally arms strings:118 and strings:126 (`AdvancedTextureOpsSupported` / `WriteableMSAATexturesSupported` must then be answerable), so report 6_6 unless those two are honestly TRUE |

⛔ **Never lift the ceiling with an environment variable in a shipped configuration.**
`VKD3D_SHADER_MODEL=6_8` is a *measurement* tool (`device.c:10591-10638`), not a default. If the fix
turns out to be extending vkd3d's denorm exemption to `VK_DRIVER_ID_MESA_VENUS`, that is the
`vkd3d-proton-helios` fork's first justified patch, it must be conditioned on something venus can
actually observe about the host, and it carries the evidence in a comment at the change site —
CLAUDE.md rule 8 applies to a forked constant exactly as to a registry knob.

### 11.8 Two structural notes on answering caps

1. **Since `_0090` there is no single options struct.** The runtime asks for
   `_OPTIONS_0090`, `_0091`, `_0093`, `_0098`, `_0101`, `_0102`, `_0109`, `_0110` and
   `_OPTIONS1_0103` independently, each a small struct of one to a few fields. Implement
   `pfnGetCaps` as an exhaustive `match` over `D3D12DDICAPS_TYPE` with a counted default arm — never
   a fallthrough that writes the largest struct (`DECISIONS.md` §7.4).
2. **⚠ Two of those structs have a tag/typedef mismatch** (§9.7): `D3D12DDI_OPTIONS_0109` /
   `D3D12DDI_OPTIONS_DATA_0109` and `D3D12DDI_OPTIONS1_DATA_0103` / `D3D12DDI_OPTIONS_DATA_0103`.
   bindgen emits the **typedef** name. Grepping the header for the name in the enum comment finds
   the tag, not the typedef; do not conclude the struct is missing.

---

## 12. Shaders

### 12.1 What `pfnCalcPrivateShaderSize` / `pfnCreate*Shader` receive

Three generations exist. **`_0109` uses the `_0026` generation**, which is an argument struct
(umddi:5538-5568):

```c
typedef struct D3D12DDIARG_CREATE_SHADER_0026
{
    D3D12DDI_HROOTSIGNATURE hRootSignature;
    CONST UINT* pShaderCode;
    union
    {
        CONST D3D12DDIARG_STAGE_IO_SIGNATURES* Standard;
        CONST D3D12DDIARG_TESSELLATION_IO_SIGNATURES* Tessellation;
        CONST D3D12DDIARG_MESH_IO_SIGNATURES* Mesh;
    } IOSignatures;
    D3D12DDI_CREATE_SHADER_FLAGS Flags;
    D3D12DDI_LIBRARY_REFERENCE_0010 LibraryReference;
    D3D12DDI_SHADERCACHE_HASH ShaderCodeHash;
} D3D12DDIARG_CREATE_SHADER_0026;

typedef SIZE_T ( APIENTRY* PFND3D12DDI_CALC_PRIVATE_SHADER_SIZE_0026 )(
    D3D12DDI_HDEVICE, _In_ CONST D3D12DDIARG_CREATE_SHADER_0026* );
typedef VOID ( APIENTRY* PFND3D12DDI_CREATE_SHADER_0026 )(
    D3D12DDI_HDEVICE, _In_ CONST D3D12DDIARG_CREATE_SHADER_0026*, D3D12DDI_HSHADER );

typedef struct D3D12DDIARG_CREATE_GEOMETRY_SHADER_WITH_STREAM_OUTPUT_0026
{
    D3D12DDIARG_CREATE_SHADER_0026                      CreateShader;
    CONST D3D12DDIARG_STREAM_OUTPUT_DECLARATION_ENTRY*  pOutputStreamDecl;
    UINT                                                NumEntries;
    CONST UINT*                                         BufferStridesInBytes;
    UINT                                                NumStrides;
    UINT                                                RasterizedStream;
} D3D12DDIARG_CREATE_GEOMETRY_SHADER_WITH_STREAM_OUTPUT_0026;
```

Three details that decide the implementation:

- **`pfnCreate*Shader` returns `VOID`.** Failure is reported by leaving the handle cleared and
  calling `pfnSetErrorCb` — the same discipline `umd/src/forward/shaders.rs:68-100` already
  implements for D3D11 (`clear_handle` first, `log_error!` on failure, never a fabricated handle).
- **The union arm is selected by the stage, not by a tag.** Vertex/pixel/geometry/compute/
  amplification use `Standard`; hull/domain use `Tessellation`; mesh uses `Mesh`. Reading the wrong
  arm is the trap `umd/src/forward/shaders.rs:167-180` already documents for D3D11 ("reading a tess
  block with the 2-word accessor silently returns `n_patch` as the first entry's system value").
  **Bind the arm at the call site, one Rust `extern` fn per stage.**
- `D3D12DDI_CREATE_SHADER_FLAGS` (umddi:2201-2207) has exactly three values: `_NONE = 0x0`,
  `_ENABLE_SHADER_TRACING = 0x1`, `_DISABLE_OPTIMIZATION_0024 = 0x2`.
- `D3D12DDI_SHADERCACHE_HASH` (umddi:4243-4246) is `BYTE Hash[16]` — a **cache key** for use with
  `pfnShaderCacheGetValueCb` / `pfnShaderCacheStoreValueCb` (umddi:4248-4270, the runtime→driver
  table 10). ⛔ It is **not** the DXIL validator hash and carries no security meaning at the DDI. A
  baseline driver ignores it (and declines the shader-cache extended feature entirely).

### 12.2 ⚠ There is no length parameter. Anywhere.

```
$ grep -c "BytecodeLength\|SHADER_BYTECODE" tmp/dx12/sdk/d3d12umddi.h
0
$ grep -n "typedef struct D3D12_SHADER_BYTECODE" -A 4 tmp/dx12/sdk/d3d12.h
2196:typedef struct D3D12_SHADER_BYTECODE
2197-    {
2198-    _Field_size_bytes_full_(BytecodeLength)  const void *pShaderBytecode;
2199-    SIZE_T BytecodeLength;
2200-    } 	D3D12_SHADER_BYTECODE;
```

**The application hands D3D12 a pointer *and* a length; the runtime forwards only the pointer.** The
same is true of the raytracing library descriptor (umddi:7820-7825):

```c
typedef struct D3D12DDI_DXIL_LIBRARY_DESC_0054
{
    CONST UINT*  pDXILLibrary;
    UINT NumExports; // Optional, if 0 all exports in the library/collection will be surfaced
    _In_reads_(NumExports) D3D12DDI_EXPORT_DESC_0054* pExports;
} D3D12DDI_DXIL_LIBRARY_DESC_0054;
```

whereas its API counterpart `D3D12_DXIL_LIBRARY_DESC` (`d3d12.h:14354-14359`) holds a full
`D3D12_SHADER_BYTECODE`.

**The size must be derived from the blob**, which is therefore self-describing. The only hint the
header gives is on the *0003* generation, whose SAL annotation is `_In_reads_(pShaderCode[1])`
(umddi:2209-2212) — i.e. dword 1 is the length. ⚠ **The `_0026` generation the `_0109` table uses
has no SAL length annotation at all**, because the pointer lives inside a struct. So there are two
self-describing D3D encodings to discriminate:

| Blob | Discriminator | Length |
|---|---|---|
| raw SM4/SM5 token stream | `dword[0]` is a version token (not `'DXBC'`) | `dword[1] * 4` bytes (length in dwords, incl. the 2-token header) |
| DXBC container (the only form a DXIL blob ever takes) | `dword[0] == 'DXBC'` (`0x43425844`) | total byte size at **byte offset 24** = `dword[6]` |

⭐ **Helios already implements exactly this, with the right bounds, and it is directly reusable.**
`umd/src/forward/shaders.rs:13-39`, `shader_code_len()`, verbatim:

```rust
pub(crate) unsafe fn shader_code_len(code: *const u32) -> usize {
    if code.is_null() { return 0; }
    // D3D API bytecode is a DXBC container with the total size at byte offset 24. …
    if *code == u32::from_le_bytes(*b"DXBC") {
        let total = *code.add(6) as usize;
        if total < 32 || total > (1 << 20) * core::mem::size_of::<u32>() {
            log_error!("DDI shader_code_len: rejecting DXBC total size {total}");
            return 0;
        }
        return total;
    }
    // D3D UMD callbacks receive raw SHDR/SHEX token streams. The second DWORD
    // is the stream length in DWORDs, including the two-token shader header.
    let dwords = *code.add(1) as usize;
    if dwords < 2 || dwords > (1 << 20) { return 0; }
    dwords * core::mem::size_of::<u32>()
}
```

**Copy it verbatim into `helios_umd12`, including both bounds checks and the log line.** Those two
bounds (`total < 32 || total > 4 MiB`, `dwords < 2 || dwords > 1 Mi`) are precisely the validation
CLAUDE.md's "validate every runtime-supplied size & offset before reading" demands on this input.
`umd/src/forward/shaders.rs:41-59`, `log_shader_code()`, is the matching instrument — it prints
`len`, `dxbc=`, and the first four dwords; port that too.

⭐ ✅ **ANSWERED (`D12-G5`) — and it is neither of the two options as posed.** The runtime hands
`pfnCreateShader` a **raw stream behind the two-token header**, never a DXBC container. Verbatim
first eight dwords, from three different slots in one run:

```
pfnCreateVertexShader:  00010060 0000010a 4c495844 00000100 00000010 00000410 dec04342 00000c21
pfnCreatePixelShader:   00000060 00000102 4c495844 ...
pfnCreateComputeShader: 00050060 000000d4 4c495844 ...
```

| dword | meaning | evidence |
|---|---|---|
| `[0]` | `(programType << 16) \| (major << 4) \| minor` | `0x0001_0060` = vertex/SM 6.0, `0x0000_0060` = pixel, `0x0005_0060` = compute — the type field matched the slot it arrived on in every case |
| `[1]` | **length in dwords** | `0x10a` = 266 dwords = 1 064 bytes |
| `[2]` | **`'DXIL'` (0x4c495844)** — the DXIL part payload | — |

⇒ the second row of the table above is the live one, and `_0026`'s missing SAL annotation says the
same thing `_0003`'s `_In_reads_(pShaderCode[1])` did.

⭐ **And the stronger half: the runtime converts DXBC to DXIL before the DDI.** The `D12-G5`
`triangle` workload builds two pipelines in one process — one from `dxc -T vs_6_0` (a 3 152-byte
container) and one from `D3DCompile(…, "vs_5_1")` (a 596-byte container). **Both** of the app's blobs
start `'DXBC'`; **both** arrive at the DDI as `…0060` + length + `'DXIL'`, and neither length matches
the app's blob. The `window` workload, which has no application shaders at all, produces only the two
runtime-internal shaders, so the attribution is unambiguous.
⇒ **A D3D12 UMD on this Windows build never sees DXBC**, and never sees a shader-model token below
`0x0060`, even for a `vs_5_1` pipeline.

**Consequence for `helios_umd12`:** port `shader_code_len()` as written, but expect only its
**raw-stream branch** to execute; the DXBC-container branch is dead on this DDI. Keep it anyway — the
bounds checks are the value — and give it a named counter, so that if a container ever does arrive
the fact is recorded rather than assumed away.

### 12.3 The IO-signature structs, and the DXBC→DXIL admission

`D3D12DDIARG_STAGE_IO_SIGNATURES` (umddi:2089-2125), `D3D12DDIARG_TESSELLATION_IO_SIGNATURES`
(umddi:2127-2169) and `D3D12DDIARG_MESH_IO_SIGNATURES` (umddi:2171-2199) carry the same "union of
all registers, a superset of what this shader uses" contract as D3D11's. The entry struct gained a
field at `_0012` (umddi:2078-2087), verbatim:

```c
typedef struct D3D12DDIARG_SIGNATURE_ENTRY_0012
{
    D3D10_SB_NAME SystemValue; // D3D10_SB_NAME_UNDEFINED if the particular entry doesn't have a system name.
    UINT Register;
    BYTE Mask;// (D3D10_SB_OPERAND_4_COMPONENT_MASK >> 4), meaning 4 LSBs are xyzw respectively
    BYTE Stream; // This field was inserted in _0012 and will not break old drivers since it doesn't change struct size.
                 // It is used to help drivers that use a DXBC->DXIL converter, for GS output signatures
    D3D10_SB_REGISTER_COMPONENT_TYPE RegisterComponentType;
    D3D11_SB_OPERAND_MIN_PRECISION   MinPrecision;
} D3D12DDIARG_SIGNATURE_ENTRY_0012;
```

Two things follow:

1. **Microsoft explicitly expects some D3D12 drivers to implement only a DXIL backend and to run
   incoming DXBC through a converter.** That is exactly the posture a Helios D3D12 UMD over vkd3d is
   in: vkd3d-shader's own dispatch says *"Shader models 4 through 6.x are handled externally through
   dxil-spirv"* (`vkd3d-proton-helios/libs/vkd3d-shader/vkd3d_shader_main.c:213`).
2. **The `_0012` layout matches Helios' existing 5-word wire entry field-for-field**, including the
   `Stream` byte Helios already carries and currently logs-and-drops
   (`umd/bridge/bridge_dxbc.cpp:212-219`). `umd/src/forward/shaders.rs:129-186` defines
   `SIG_ENTRY_WORDS = 5`, `SigEntry { sysval, register_, mask, comptype, stream }`, and
   `SigHeader::{Stage(2 words), Tess(3 words)}`.

⭐ **But a forwarder can ignore the IO signatures entirely.** vkd3d recovers signatures from the
DXBC container itself (`libs/vkd3d-shader/dxbc.c` container walk). Helios' D3D11 UMD only
*synthesises* a container (`umd/bridge/bridge_dxbc.cpp`, 406 lines) because the D3D11 runtime hands
it a bare token stream with no `ISGN`/`OSGN`/`PCSG` chunks. **If §12.2's UNVERIFIED resolves to
"D3D12 always passes a container", `bridge_dxbc.cpp` is not needed at all** — and that is the single
biggest possible saving in the shader path. If it resolves to "SM 5.1 arrives as a token stream",
the container synthesiser is needed for exactly that one case, and the code to reuse is
`build_dxbc_container<N>` (`umd/bridge/bridge_dxbc.cpp:264-311`) plus `append_signature_chunk`
(`:165-241`) and `encode_signature_entry` (`:124-155`).

⭐ **Port `ShaderBytecodeDumpPath` in the first shader commit.**
`umd/bridge/bridge_dxbc.cpp:39-83` reads `HKLM\SOFTWARE\Helios!ShaderBytecodeDumpPath` (REG_SZ),
creates the directory, and writes every blob as
`shader-<pid>-<seq>-<stage>-<form>-<len>.dxbc`. It is the cheapest D3D12 bring-up instrument in the
tree and it settles §12.2's UNVERIFIED as a by-product. Reuse the same registry value name so one
knob covers both UMDs.

### 12.4 DXIL vs DXBC — who compiles, who validates, who signs

| Question | Answer | Evidence |
|---|---|---|
| Does the D3D11 shader path work for D3D12? | ⛔ **No.** `dxvk-helios/subprojects/dxbc-spirv/` has **zero** DXIL support (recursive case-insensitive grep for `dxil` → 0 hits) and there is no incremental path to one: DXIL is LLVM 3.7 bitcode, not a token stream. | `research/R8` §2.3 |
| What compiles DXIL, then? | **`dxil-spirv`**, a vkd3d-proton subproject, driven by `libs/vkd3d-shader/dxil.c` (2 474 lines of `dxil_spv_option_*` plumbing). vkd3d-proton contains **no** DXBC-TPF→SPIR-V compiler — *both* DXBC and DXIL go to dxil-spirv. (Upstream WineHQ *vkd3d* does have a TPF compiler; vkd3d-**proton** does not. Do not confuse them.) | `libs/vkd3d-shader/vkd3d_shader_main.c:196-215`, `meson.build` |
| Is it in the tree? | ⚠ **No.** `vkd3d-proton-helios/subprojects/dxil-spirv/` is an **empty directory** and is the *only* entry under `subprojects/`; the `khronos/Vulkan-Headers` and `khronos/SPIRV-Headers` submodules are registered at **repo-root paths, not under `subprojects/`**, and neither directory exists on disk at all. `git -C vkd3d-proton-helios submodule status` prints all three prefixed `-` (uninitialised). vkd3d-proton **cannot be built from this tree as-is**; `git submodule update --init` (from `vkd3d-proton-helios/`) is a prerequisite for every D3D12 gate. | `research/R8` §3.1; `vkd3d-proton-helios/.gitmodules`, `submodule status` re-run 2026-08-05 |
| Who validates the DXIL hash? | **The D3D12 runtime, in `d3d12core.dll`, before the UMD is called.** "The DirectX runtime validates the hash on each shader by computing the hash from DXIL and comparing the computed value against the value written in the shader binary." (<https://devblogs.microsoft.com/directx/open-sourcing-dxil-validator-hash/>) | |
| Does the driver sign or validate anything? | **No.** A Helios D3D12 UMD has no signing obligation and no validation obligation — the blob reached it only because the runtime accepted the hash. ⚠ **It must still bounds-check every offset it reads out of the blob** (§12.2): a correct hash says nothing about a container being well-formed against *this* parser. | CLAUDE.md rule "validate every runtime/guest-supplied size & offset before reading" |
| Does the DXBC container checksum matter? | Not to vkd3d: `dxbc.c:124` is literally `WARN("Ignoring DXBC checksum.\n"); skip_dword_unknown(&ptr, 4);`. Helios' D3D11 bridge *computes* one only because it synthesises containers (`umd/bridge/bridge_dxbc.cpp:303-304`). Neither is a security check. | |

**And when you go to build it, the build is decided** (`DECISIONS.md` §6.1, gate `D12-G0`): the
**Linux mingw cross-build is PRIMARY**. `x86_64-w64-mingw32-gcc`, `widl`, `glslangValidator`,
`meson` and `ninja` are all already on the Linux host's `PATH` (verified), so it needs zero
installation and it matches vkd3d-proton's own shipping build (`artifacts.yml`). Native MSVC x64 on
the win11 VM (upstream `test-build-windows.yml`: choco `strawberryperl` + `glslangValidator` +
`meson` + VS2022, built to a **local C:** path, ⛔ never `Z:\`) is the **fallback, taken when a
Windows debugger is wanted**. Either way `git submodule update --init` comes first.

⚠ **A conformance caveat worth writing down now.** Replacing `d3d12core.dll` (the app-local Phase-0
arm, `DECISIONS.md` D2) removes the validator-hash check entirely, because vkd3d never computes it.
So **a Helios-under-vkd3d run and a Helios-under-real-D3D12 run do not see the same shader-acceptance
behaviour**, and a conformance claim from one does not transfer to the other. Record which arm any
shader result came from.

⭐ **Copy vkd3d's posture, not just its option list.** `d3d12_device_validate_shader_meta`
(`vkd3d-proton-helios/libs/vkd3d/device.c:11671-11795`) re-reads the **emitted SPIR-V**'s
`OpCapability` set (`vkd3d_shader_extract_feature_meta`, `vkd3d_shader_main.c:750-830`) and **fails
PSO creation** when a shader needs something the reported caps disclaim — eleven checks, each a
clean `return false`, not a hang. That is the mechanism that turns an over-reported cap from silent
corruption into a clean `HRESULT`, and it is the concrete answer to §11.6 for shaders. A native
Helios D3D12 UMD gets it for free by forwarding into vkd3d — **do not bypass it.**

---

## 13. Present at the DDI

**`docs/dx12/PRESENT.md` owns the design.** This section gives only the DDI shape so the reader does
not have to re-derive it.

### 13.0 ✅ Measured (`D12-G5`) — the real per-frame present sequence

A flip-model `DXGI_SWAP_EFFECT_FLIP_DISCARD` swapchain, 20 presents, WARP behind the spy. Every
frame, identically, and the first frame is the same as the steady state:

```
cl[ 0] pfnCloseCommandList
queue[0] pfnExecuteCommandLists
core[81] pfnGetPresentPrivateDriverDataSize      <-- ONCE PER PRESENT, immediately before
cl[19] pfnPresent
```

* ⭐ **`pfnGetPresentPrivateDriverDataSize` is the private-data hook, and it is called per present.**
  WARP answers 0, so all 20 presents carried `PrivateDriverDataSize = 0` and
  `pPrivateDriverData = NULL`. A driver that returns N is handed an N-byte buffer.
  ⚠ **Whether that buffer reaches `DxgkDdiPresent` is NOT settled here** — the D3D11 answer is *no on
  DMA flips* (memory 64th), which is why `PRESENT.md` rides the identity on the Render command. This
  is a **second candidate channel to test at G8**, not a replacement for the first.
* **The argument, verbatim:** `phSurfacesToPresent` with `SurfacesToPresent = 1`,
  `hDstResource = NULL`, `Flags = 0x21`, `FlipInterval = 0`, `VidPnSourceID = 0xffffffff`,
  `DirtyRects = 0`, `OptimizeForComposition = 1`.
* **`pOut` is a 536-byte `D3D12DDI_PRESENT_0051`** and its first dword is a `D3DKMT_HANDLE` —
  `0x40000b00`, then `0x40000b80` on the next frame: the two swapchain buffers.
  `pCtx` (`D3D12DDI_PRESENT_CONTEXTS_0051`) is non-NULL; `pHwQ` was NULL at `_0110` and non-NULL at
  `_0040`.
* ⛔ **`D3D12DDI_TABLE_TYPE_DXGI` is never filled and no DXGI slot is ever called** (§2.3). Present
  reaches the driver here and only here.

`pfnPresent` is a **command-list** slot (`DECISIONS.md` P-C). Signature and structs, verbatim
(umddi:7226-7251):

```c
typedef struct D3D12DDI_PRESENT_0051
{
    D3DKMT_HANDLE   BroadcastSrcAllocation[D3DDDI_MAX_BROADCAST_CONTEXT+1];
    D3DKMT_HANDLE   BroadcastDstAllocation[D3DDDI_MAX_BROADCAST_CONTEXT+1];
    BOOL            AddedGpuWork;
    UINT            BackBufferMultiplicity;

    BOOL                        SyncIntervalOverrideValid;
    DXGI_DDI_FLIP_INTERVAL_TYPE SyncIntervalOverride;
} D3D12DDI_PRESENT_0051;

typedef struct D3D12DDI_PRESENT_CONTEXTS_0051
{
    HANDLE          hContext;
    UINT            BroadcastContextCount;
    HANDLE          BroadcastContext[D3DDDI_MAX_BROADCAST_CONTEXT];
} D3D12DDI_PRESENT_CONTEXTS_0051;

typedef struct D3D12DDI_PRESENT_HWQUEUES_0051
{
    UINT            BroadcastQueueCount;
    HANDLE          hHwQueues[D3DDDI_MAX_BROADCAST_CONTEXT+1];
} D3D12DDI_PRESENT_HWQUEUES_0051;

typedef VOID ( APIENTRY* PFND3D12DDI_PRESENT_0051 ) ( D3D12DDI_HCOMMANDLIST, D3D12DDI_HCOMMANDQUEUE,
    _In_ CONST D3D12DDIARG_PRESENT_0001*,
    _Out_ D3D12DDI_PRESENT_0051*, _Out_opt_ D3D12DDI_PRESENT_CONTEXTS_0051*, _Out_opt_ D3D12DDI_PRESENT_HWQUEUES_0051* );
```

The **input** struct is essentially `DXGI_DDI_ARG_PRESENT` (umddi:1630-1644):

```c
typedef struct D3D12DDI_ARG_PRESENTSURFACE { D3D12DDI_HRESOURCE hSurface; UINT SubResourceIndex; } D3D12DDI_ARG_PRESENTSURFACE;

typedef struct D3D12DDIARG_PRESENT_0001
{
    CONST D3D12DDI_ARG_PRESENTSURFACE*  phSurfacesToPresent;
    UINT                                SurfacesToPresent;
    D3D12DDI_HRESOURCE                  hDstResource;
    UINT                                DstSubResourceIndex;
    DXGI_DDI_PRESENT_FLAGS              Flags;
    DXGI_DDI_FLIP_INTERVAL_TYPE         FlipInterval;
    D3DDDI_VIDEO_PRESENT_SOURCE_ID      VidPnSourceID;
    CONST RECT*                         pDirtyRects;
    UINT                                DirtyRects;
    UINT                                PrivateDriverDataSize;
    VOID*                               pPrivateDriverData;
    BOOL                                OptimizeForComposition;
} D3D12DDIARG_PRESENT_0001;
```

Four facts a `PRESENT.md` reader needs from here:

1. **The driver *outputs* the KM allocation handles and the context(s).** That is materially better
   than a bare Vulkan ICD gets: a D3D12 UMD receives the destination-surface hand-off directly.
2. `pfnGetPresentPrivateDriverDataSize(hDevice, CONST D3D12DDIARG_PRESENT_0001*) -> UINT`
   (typedef at umddi:1792) is on the **device core** table and sizes `pPrivateDriverData`. It is one
   of the 124 slots and it needs a real body before any present works. ⛔ Not umddi:1795 — that line
   is inside `PFND3D12DDI_SERIALIZEOBJECT`.
3. ✅ **The identity channel transfers unchanged, with NO KMD change.** `pfnPresentCb` and
   `pfnRenderCb` are absent from `d3d12umddi.h` (§8.2, verified by absence) but they are **not** out
   of the driver's reach: `D3D12DDIARG_CREATEDEVICE_0109::pKTCallbacks` (umddi:13623) is a
   `CONST D3DDDI_DEVICECALLBACKS*` — the same 65-entry kernel thunk table `helios_umd.dll` drives
   today (`d3dumddi.h:4499`, §6.3) — and it contains both (verified; `DECISIONS.md` P-C §6.1).

   So the mechanism Helios relies on today survives verbatim: the D3D12 UMD writes a
   `HeliosPresentRenderCmd` and calls `pKTCallbacks->pfnRenderCb` exactly as
   `umd/src/forward/present.rs:795` does, landing in the KMD's **PASSIVE** `dxgkddi_render` path and
   its per-context stash (memory `d4b-snapshot-chain-closes-gt2-64th.md`: *"Present private data
   NEVER reaches DxgkDdiPresent on DMA flips — flip data rides the Render cmd → per-context
   stash"*). `DECISIONS.md` D5's KMD work list stays at three items, none of them this.

   ⛔ **Do not design a new `DxgkDdiSubmitCommandVirtual` decode for the present identity.** That
   DDI runs at **DISPATCH_LEVEL** (`kmd_render/src/ddi/submit_command.rs:723-724`), where the stash
   machinery's `diag::record*` calls are illegal (CLAUDE.md's first invariant), and it would add a
   fourth KMD item that `DECISIONS.md` D5 does not have. The `pfnRenderCb` route is the
   recommendation.

   `pPrivateDriverData` on `D3D12DDIARG_PRESENT_0001` remains available as a second carrier if
   `PRESENT.md` wants one; it is a choice, not a necessity. Everything from `dxgkrnl` down (the flip
   arm, `PresentFlipPrivate`, `set_scanout_blob`) is reused unchanged.

   ⚠ **The one residual UNVERIFIED** (`DECISIONS.md` P-C): that the D3D12 runtime *tolerates* the
   driver calling `pfnRenderCb` around `pfnPresent`. Settling experiment: `pfnRenderCb` plus a
   counting `DxgkDdiRender` on the D3D12 path at gate G7, before G8 depends on it.
4. Two runtime validations apply to what the driver writes:
   > `Driver provided too many contexts for present.` — strings:55
   > `Driver set invalid sync interval override.` — strings:93

   plus `Driver couldn't change frame latency` (strings:21).

---

## 14. The minimum viable table

### 14.0 ✅ Measured (`D12-G5`) — what a triangle actually touches

Per-slot call counts from a real driver (WARP) under a device + queue + swapchain + two PSOs + two
draws + three presents, with a counting thunk on **every** slot of all four tables:

| table | slots called | of |
|---|---:|---:|
| `DEVICE_CORE` | **47** | 124 |
| `COMMAND_LIST_3D` | **22** | 75 |
| `COMMAND_QUEUE_3D` | **1** | 7 |
| `DXGI` | **0** | 32 armed |
| **total** | **70** | 206 |

Three things this changes about how §14.2's list should be read:

* ⭐ **`D3D12CreateDevice` alone drives 27 of the 124 core slots** — before the application owns a
  single object. The runtime builds *its own* internal pipelines at device creation: root signature,
  vertex shader, blend / depth-stencil / rasterizer state, PSO, `pfnMakeResident`, then a compute
  shader + PSO + `pfnMakeResident`, plus a command pool created and destroyed. It also runs a
  **91-format `pfnCheckFormatSupport` sweep with 30 `pfnCheckMultisampleQualityLevels` calls each —
  2 730 calls**. None of that is optional and none of it is app-driven.
* ⭐ **The one queue slot called is `pfnExecuteCommandLists`.** `pfnSignalFence` and `pfnWaitForFence`
  were **never called** across 20 frames of `ID3D12CommandQueue::Signal` + `SetEventOnCompletion`,
  although `pfnCreateFence` was called three times ⇒ evidence that the *runtime* performs the queue
  signal/wait (§15.1 #12). WARP is software-scheduled, so confirm on hardware before designing on it.
* ⭐ **`pfnResetCommandList` is followed by a fixed 15-call state-reset block** on every reset,
  whether or not the application touches that state: `pfnSetPipelineState`, `pfnIaSetTopology`,
  `pfnSetDescriptorHeaps`, `pfnIASetVertexBuffers`, `pfnIASetIndexBuffer`, `pfnSOSetTargets`,
  `pfnOMSetRenderTargets`, `pfnRsSetViewports`, `pfnRsSetScissorRects`, `pfnOmSetBlendFactor`,
  `pfnOmSetFrontAndBackStencilRef`, `pfnOMSetDepthBounds`, `pfnRSSetShadingRate`,
  `pfnSetPredication`, `pfnClearRootArguments`. They are not optional for a first frame.
* ⚠ **Barriers arrive on `pfnBarrier` (`cl[68]`), not the legacy resource-barrier slot**, because
  WARP reports `EnhancedBarriersSupported = 1`. The barrier cap decides which slot the runtime calls
  — answering it wrong means implementing the slot the runtime never uses.

**70 is the measured floor for a triangle on WARP; §14.2's 99 is the design target.** They are
different questions — 99 counts slots that need a *real body* for correctness across the whole
first-frame surface, 70 counts what one specific workload hit. Diff the two lists before P4 sizes
itself, and treat a slot in 99-but-not-70 as "not exercised yet", never as "not needed".

### 14.1 Slots that are structurally mandatory

**Adapter table — all 8.** There is no versioning or optional marker on any of them and the runtime
has no other way to reach the driver. A NULL is a call through a null pointer the first time the
runtime uses it. (⚠ Possible exception: `pfnGetOptionalDDITables`. **UNVERIFIED** whether the
runtime null-checks it. Safest: implement it and return `*puEntries = 0`.)

**`DEVICE_CORE` (0) — all 124.** **`COMMAND_LIST_3D` (1) — all 75.** There is no per-slot opt-out
mechanism anywhere in the header: `pfnFillDDITable` fills a struct, and a NULL slot is a crash the
first time the runtime dispatches through it.

**What is *verified* rather than inferred:** the runtime explicitly null-checks and names at least
**twelve** slots, so at minimum these can never be NULL —

```
pfnCreateHeapAndResource   pfnDestroyHeapAndResource   pfnOpenHeapAndResource
pfnCalcPrivateHeapAndResourceSizes   pfnCalcPrivateOpenedHeapAndResourceSizes
pfnCheckResourceAllocationInfo   pfnCheckExistingResourceAllocationInfo   pfnCheckSubresourceInfo
pfnCopyBufferRegion   pfnMapHeap   pfnUnmapHeap   pfnCopyTiles
```
(strings:95-103, 54, 3.) The existence of those checks proves the runtime *does* validate some
slots and reports a named error rather than crashing — it does **not** prove any other slot may be
NULL.

**`COMMAND_QUEUE_3D` (2) — 5 of 7 semantically, all 7 in practice.** `pfnUnused`/`pfnUnused2` are
named unused; fill them with counting stubs anyway (§5).

#### 14.1.1 ⭐ "May a slot be NULL" is three questions, not one (2026-08-05)

`D12-G5` measured that WARP leaves exactly four of 206 slots NULL and still works. The corpus shows
those four are **not the same kind of NULL**, and §15.1 #2 previously conflated them:

| kind | slot | evidence | what Helios should do |
|---|---|---|---|
| **RETIRED** — the function was withdrawn and the table entry kept as a placeholder | `cl[69] pfnOmSetAlphaBlendFactor` | `VulkanOn12.md:270`: *"a previous version of this spec referred to `pfnOmSetAlphaBlendFactor` to assign the alpha blend factor. This function is no longer valid, but its entry has been retained and is marked as unused in D3D."* The header agrees, but **only in the older tables** — `_0092` (umddi:11242) and `_0094` (umddi:11381) carry a literal `// unused` comment; the shipping `_0108` table (umddi:13303-13388) declares the same `PFND3D12DDI_OM_SETALPHABLENDFACTOR_0092` typedef with **no comment at all** | counting stub. Never implement it — the *replacement* is the existing `pfnOmSetBlendFactor`, whose `FLOAT[4]` component `[3]` is the constant for `D3D12DDI_BLEND_ALPHA_FACTOR` (=20) / `_INV_ALPHA_FACTOR` (=21) |
| **OPTIONAL FEATURE** — a real function the driver may decline | `core[121] pfnImplicitShaderCacheControl` | `ShaderCache.md:219`: the runtime calls it only for the `DRIVER_MANAGED` implicit-cache kind, and *"this API will only be supported in developer mode"*. Header name is `PFND3D12DDI_IMPLICITSHADERCACHECONTROL_0080` (umddi:10356) — ⚠ the spec's `…_008n` is a placeholder, not a symbol | counting stub, and report no driver-managed cache |
| **RESERVED** — never had a function | `queue[1] pfnUnused`, `queue[2] pfnUnused2` | named in the header | counting stub |

⛔ **Do not generalise from these to "optional slots exist."** `DepthBoundsTest.md:779` states the
opposite for the command-list table — *"The existing command list v-table design does not support
optional DDIs."* — and proposes that the **runtime** substitutes its own stubs or removes the command
list entirely. ⚠ That sentence sits inside a future-tense internal implementation *plan*, so treat it as
design intent rather than shipped behaviour; but it points the same way as the header, which has no
per-slot opt-out mechanism anywhere. **Fill every slot.**

⚠ **A `NOT_SUPPORTED` tier does suppress its slots — but the mechanism differs per feature, so there is
no general rule** (§15.1 #16). Three worked examples, three different mechanisms: at `RenderPassTier 0`
the runtime **rewrites** `BeginRenderPass` into the equivalent `OMSetRenderTargets` and never calls a
render-pass slot; at `ProgrammableSamplePositionsTier NOT_SUPPORTED` the runtime **removes the device**
if an app calls in; for depth bounds the plan above is runtime-supplied **stubs**.

**`DXGI` (3) — the whole struct**, shape TBD (§2.3).

**Extended features — genuinely optional.** Answer `pfnGetSupportedExtendedFeatures` with zero
features and none of table types 4–27 is ever requested.

⚠ **Reporting a tier as `NOT_SUPPORTED` legitimately removes *work*, not *slots*.** Reporting
`RaytracingTier = NOT_SUPPORTED` means the runtime will not call `pfnDispatchRays`, but the slot must
still be non-NULL. **UNVERIFIED that the runtime honours this for every tier** — the converse
("don't advertise and hope the slot is never called") has never been tested here. That is why the
stub fill in §14.3 is not optional.

### 14.2 The 99 slots that need a real body for "a cleared render target reaches the screen"

Derived from the object graph, not from the header. Enumerated exactly so the reader can tick them
off. ⚠ **Re-sum the `n` column before quoting any figure from this section.** An earlier revision
printed the device-core subtotal as 71 and the grand total as 97; the rows below are individually
correct (each name list was checked against umddi:13453-13615) but the subtotal was mis-added. The
arithmetic is now written out in full — in the subtotal row of the device-core table, and again in
the **Total** line at the end of the section.

**Adapter — 8 (all of them):**
```
pfnCalcPrivateDeviceSize   pfnCreateDevice   pfnDestroyDevice   pfnCloseAdapter
pfnGetSupportedVersions    pfnGetCaps        pfnGetOptionalDDITables   pfnFillDDITable
```

**Device core — 73 of 124:**

| Sub-group | n | Slots |
|---|---|---|
| format queries | 2 | `pfnCheckFormatSupport`, `pfnCheckMultisampleQualityLevels` |
| command queue | 3 | `pfnCalcPrivateCommandQueueSize`, `pfnCreateCommandQueue`, `pfnDestroyCommandQueue` |
| command pool | 4 | `pfnCalcPrivateCommandPoolSize`, `pfnCreateCommandPool`, `pfnDestroyCommandPool`, `pfnResetCommandPool` |
| command recorder | 4 | `pfnCalcPrivateCommandRecorderSize`, `pfnCreateCommandRecorder`, `pfnDestroyCommandRecorder`, `pfnCommandRecorderSetCommandPoolAsTarget` |
| command list | 3 | `pfnCalcPrivateCommandListSize`, `pfnCreateCommandList`, `pfnDestroyCommandList` |
| fence | 3 | `pfnCalcPrivateFenceSize`, `pfnCreateFence`, `pfnDestroyFence` |
| descriptor heap | 3 | `pfnCalcPrivateDescriptorHeapSize`, `pfnCreateDescriptorHeap`, `pfnDestroyDescriptorHeap` |
| root signature | 3 | `pfnCalcPrivateRootSignatureSize`, `pfnCreateRootSignature`, `pfnDestroyRootSignature` |
| pipeline state | 3 | `pfnCalcPrivatePipelineStateSize`, `pfnCreatePipelineState`, `pfnDestroyPipelineState` |
| descriptor addressing | 3 | `pfnGetDescriptorSizeInBytes`, `pfnGetCPUDescriptorHandleForHeapStart`, `pfnGetGPUDescriptorHandleForHeapStart` |
| views | 6 | `pfnCreateRenderTargetView`, `pfnCreateShaderResourceView`, `pfnCreateConstantBufferView`, `pfnCreateUnorderedAccessView`, `pfnCreateDepthStencilView`, `pfnCreateSampler` |
| descriptor copy | 2 | `pfnCopyDescriptors`, `pfnCopyDescriptorsSimple` |
| heaps + resources | 9 | `pfnCalcPrivateHeapAndResourceSizes`, `pfnCreateHeapAndResource`, `pfnDestroyHeapAndResource`, `pfnCalcPrivateOpenedHeapAndResourceSizes`, `pfnOpenHeapAndResource`, `pfnMapHeap`, `pfnUnmapHeap`, `pfnMakeResident`, `pfnEvict` |
| introspection | 5 | `pfnCheckResourceAllocationInfo`, `pfnCheckSubresourceInfo`, `pfnCheckExistingResourceAllocationInfo`, `pfnCheckResourceVirtualAddress`, `pfnCheckResourceAllocationHandle` |
| shaders | 5 | `pfnCalcPrivateShaderSize`, `pfnCreateVertexShader`, `pfnCreatePixelShader`, `pfnCreateComputeShader`, `pfnDestroyShader` |
| immutable pipeline sub-state | 12 | the four Calc/Create/Destroy triples of §3.2(b) |
| misc | 3 | `pfnGetImplicitPhysicalAdapterMask`, `pfnQueryNodeMap`, `pfnGetPresentPrivateDriverDataSize` |
| **subtotal** | **73** | 2+3+4+4+3+3+3+3+3+3+6+2+9+5+5+12+3 = **73** |

**Command list — 15 of 75:**
```
pfnCloseCommandList     pfnResetCommandList
pfnClearRenderTargetView  pfnOMSetRenderTargets  pfnRsSetViewports  pfnRsSetScissorRects
pfnSetPipelineState     pfnSetGraphicsRootSignature  pfnSetDescriptorHeaps
pfnDrawInstanced        pfnResourceBarrier (or pfnBarrier)
pfnPresent              pfnResourceCopy  pfnCopyTextureRegion  pfnCopyBufferRegion
```

**Command queue — 3 of 7:** `pfnExecuteCommandLists`, `pfnSignalFence`, `pfnWaitForFence`.

**Total: 8 (adapter) + 73 (device core) + 15 (command list) + 3 (queue) = 99 slots with real
bodies, out of the 214 that must be non-NULL** (8 + 124 + 75 + 7 = 214, `DECISIONS.md` §4.1).

⚠ `DECISIONS.md` §4 rounds this to "~86", and §4.2 records the honest range as "~86–99" with
**this section named as the authoritative list**. The difference is exactly the **12 immutable
pipeline sub-state slots** (blend / rasterizer / depth-stencil / element layout) — 99 − 12 = 87 ≈
the "~86". Those twelve are *not* optional for a graphics PSO, because
`D3D12DDIARG_CREATE_PIPELINE_STATE_0099` references them by handle (§9.9). **Budget for 99.**

✅ `GATES.md` now sizes checkpoint P4 / gate `D12-G8` on **99** and cites this section, so the two
agree. If you ever see "~86" again, it came from a document written before this count was
enumerated — see §17.1.

### 14.3 The stub-then-overwrite install discipline

⭐ **`umd/src/forward/tables.rs` is the model. Copy its structure, not just its idea.**

1. **Fill every slot with a named, counting stub first, then overwrite the implemented ones.** The
   D3D11 installers are documented as running "over the stub fill" (`umd/src/forward/tables.rs:11`,
   `:43`). For D3D12 the stub must be *per-slot*, so that a counter readout names which unimplemented
   DDI was called — CLAUDE.md rule 2 ("every skipped/refused path gets a named counter"), and the
   direct analogue of the noop-DDI hit counters `CONFORMANCE.md` is driving to zero for D3D11.

2. **Make install ORDER structural with `#[must_use]` proof tokens.** `umd/src/forward/tables.rs:44-70`
   records why, and the reasoning transfers verbatim to D3D12's version ladder:

   > "R1009. Correctness of every >=11.1 device rested on TEXTUAL CALL ORDER inside `device_funcs.rs`:
   > `install()` writes 10.x-typed handlers into slots that `install_11_1()` must run AFTERWARDS to
   > replace. … These tokens make the ordering structural. `install_11_1` cannot be called without
   > the value `install` returns, so `install_11_1(f); install(f);` no longer compiles."

   ```rust
   #[must_use] pub struct Filled11_0(());     // umd/src/forward/tables.rs:59
   #[must_use] pub struct Filled11_1(());     // :63
   #[must_use] pub struct FilledWddm1_3(());  // :70
   pub unsafe fn install(funcs: *mut ddi::D3D11DDI_DEVICEFUNCS) -> Filled11_0 { … }          // :72
   pub unsafe fn install_11_1(funcs: …, base: Filled11_0) -> Filled11_1 { let Filled11_0(()) = base; … }  // :240
   pub unsafe fn install_wddm1_3(funcs: …, level_11_1: Filled11_1) -> FilledWddm1_3 { … }    // :290
   ```

   The D3D12 shape:

   ```rust
   #[must_use] pub struct StubbedCore(());
   #[must_use] pub struct FilledCore0109(());

   /// Writes a counting stub into every one of the 124 slots.
   pub unsafe fn stub_core_0109(f: *mut ddi12::D3D12DDI_DEVICE_FUNCS_CORE_0109) -> StubbedCore { … }
   /// Overwrites the 73 implemented slots (§14.2). Cannot be called before the stub fill.
   pub unsafe fn install_core_0109(f: *mut ddi12::D3D12DDI_DEVICE_FUNCS_CORE_0109,
                                   stubbed: StubbedCore) -> FilledCore0109 { let StubbedCore(()) = stubbed; … }
   ```

   and the same pair for `COMMAND_LIST_FUNCS_3D_0108` and `COMMAND_QUEUE_FUNCS_CORE_0001`.
   `pfnFillDDITable` then takes the `Filled*` token as its proof obligation before the
   `copy_nonoverlapping` of §2.2.

3. ⛔ **Never write past `TableSize`** (§2.2). The truncation counter belongs in the same commit as
   the fill.

4. ⛔ **No `panic!` / `todo!` / `unwrap` in any stub.** Many D3D12 DDIs return `VOID`, so a stub's
   only legal report channel is `pfnSetErrorCb` / `pfnSetCommandListErrorCb` plus its counter
   (`DECISIONS.md` §7.6). A panic in a DDI is a silent graphics deadlock.

5. **Declining an unimplemented interface is `DXGI_ERROR_UNSUPPORTED` (0x887A0004), never
   `DXGI_ERROR_DRIVER_INTERNAL_ERROR` (0x887A0020)** — `DECISIONS.md` §7.5, and the reason is
   written at `umd/src/adapter.rs:181-187`: the latter is recorded by the runtime and ETW as a
   *driver fault*, so an ordinary negotiation looks like a bug.

6. **The whole D3D12 path stays behind `HKLM\SOFTWARE\Helios!UmdD3D12` (`DECISIONS.md` D11),** read
   once per process at the top of `OpenAdapter12`; absent ⇒ `DXGI_ERROR_UNSUPPORTED`, bit-identical
   to a build without D3D12. ⚠ dwm.exe already calls `OpenAdapter12` in production.

---

## 15. What the header does NOT tell you

### 15.0 ✅ The spy has RUN — `D12-G5`, 2026-08-05

**Everything in §15 below is now backed by a log rather than by inference.** The full result is
`tmp/dx12/gates/G5/answers.md`; the raw captures are the `*.log` files beside it. Driver behind the
proxy: `d3d10warp.dll` **10.0.26100.8875**; runtime: this guest's own `D3D12Core.dll`.

**Route A works** — the runtime honours an app-local `d3d10warp.dll`, no registry change and no
reboot, which §15.2 had marked UNVERIFIED. **Route B also works** and is required for anything about
the *Helios* adapter, with one addition §15.2 did not have: after rewriting `UserModeDriverName[3]`
you must `pnputil /restart-device`, because dxgkrnl uses the path it cached at StartDevice.

Headline results, each expanded in place below:

| | |
|---|---|
| **Negotiated version** | `D3D12DDI_SUPPORTED_0110`, the newest of the **77** WARP offers (13 of which are D3D11-era tokens). `Interface` = high 32 bits, `Version` = low 32 — **confirmed, not inferred** (§1.5) |
| **⭐ The version floor** | **`_0040` is accepted by this Windows build and a triangle presents on it** — 96 core + 58 CL slots instead of 124 + 75 (§1.6, §15.4) |
| **⭐ Shader bytecode** | the runtime hands `pfnCreateShader` a **raw stream**, `dword[0]=(type<<16)|(major<<4)|minor`, `dword[1]=length in dwords`, `dword[2]='DXIL'` — **never a DXBC container**, and it converts SM 5.1 DXBC to DXIL first (§12.2) |
| **⭐ Caps are one contract** | cross-cap consistency is enforced **at retail**, at `D3D12CreateDevice`, `0x887A0020` + an English reason on ETW `Microsoft-Windows-Direct3D12`. Out-of-range tiers are **clamped silently**; legal answers propagate verbatim to the app (§11.5) |
| **The DXGI table** | `D3D12DDI_TABLE_TYPE_DXGI` is **never requested** — not by a flip-model swapchain, not across 20 presents (§2.3) |
| **WDDM level** | the runtime does **not** gate the DDI version on the adapter's WDDM level: forced `_0110` negotiated cleanly on the WDDM 2.1 Helios adapter (§11.7) |
| **Coverage** | a triangle + present touches **70 of 206** driver slots (47 core, 22 CL, 1 queue, 0 DXGI) |

### 15.1 The numbered list — with the `D12-G5` verdicts

The § reference is where it is discussed; the settler is in §15.2 unless stated.

**After `D12-G5` (the spy): 8 answered outright, 6 partial, 4 still UNVERIFIED with the reason stated.**
**After the DirectX-Specs pass (2026-08-05, `SPECS.md` @ pin `2bd58ca5`): 9 / 6 / 3.** ⭐ #16 moved
◑→✅ *per mechanism* (and the answers diverge, which is itself the result); #7 moved ⛔→◑ on a direct
documentary statement; #2 stayed ◑ but split into three distinct kinds of NULL; #10 stayed ⛔ **and
gained the reason** — the corpus describes no VA provenance check anywhere, and the one mechanism that
would dictate a VA is gated above this build.

| # | Question | § | Verdict (`D12-G5`, 2026-08-05) |
|---|---|---|---|
| 1 | Which caps types the runtime demands, in what order, and what a refusal does | 11.2 | ✅ **23 of the 43 asked**, order in §11.2; a failing HRESULT is tolerated (WARP itself fails 1074 and 1080 and the device still creates) |
| 2 | Whether any DDI slot may legally be NULL | 14.1 | ◑ **Sharpened by the specs, 2026-08-05 — it is three questions.** The four WARP leaves NULL are a **retired** slot (`cl[69]`, `VulkanOn12.md:270`), an **optional feature** (`core[121]`, `ShaderCache.md:219`), and two **reserved** (`queue[1..2]`) — §14.1.1. ⛔ And `DepthBoundsTest.md:779` says the command-list v-table *"does not support optional DDIs"* at all. **Fill every slot.** The other 202 still need a null-one-at-a-time arm |
| 3 | The meaning of `pfnFillDDITable`'s 5th `UINT` and of `D3D12DDI_TABLE_REQUEST::numTables` | 2.2 | ◑ **The 5th `UINT` is the command-list table INDEX** — the runtime fills type 1 twice at device creation, indices 0 and 1, with distinct `hRTTable`. `numTables` still unexercised: `pfnGetOptionalDDITables` answered 0 |
| 4 | Which `DXGI*_DDI_BASE_FUNCTIONS` shape table type 3 wants | 2.3 | ✅ **Moot — table type 3 is never requested at all** |
| 5 | The `Interface` / `Version` split of a `D3D12DDI_SUPPORTED_*` token | 1.5 | ✅ **high 32 / low 32**, matched bit-for-bit against the driver's own list |
| 6 | The DDI-version → Windows-release mapping | 1.5 | ◑ **26100.8875 negotiates `_0110` and accepts down to `_0040`.** One build does not give the table |
| 7 | Where recording memory comes from — `pfnSubmitCommandCb` vs `pfnRenderCb` | 8.2 | ◑ **MOVED FORWARD 2026-08-05 by a direct documentary statement.** `ResourceHeaps.md:1678`: *"The driver must call `SubmitCommandCB` during the call to `pfnExecuteCommandLists` from the same thread that entered the DDI. The driver must only pass DXGK context handles that were created during the command queue creation."* — plus a ≤8 `WrittenPrimaries` cap (§8.2). ⛔ It does **not** forbid `pfnRenderCb`, so P-C's identity channel stands. The spy still structurally cannot settle it (WARP called **no** `pKTCallbacks` thunk in any run); settler unchanged — ETW `DxgKrnl` `DmaPacket`/`QueuePacket`, or G7/G8 |
| 8 | Whether `pfnGetSupportedVersions` is really a two-call query | 1.3 | ✅ **Yes** — count with `pVersions == NULL`, then fill |
| 9 | How a driver obtains a second `D3D12DDI_HRTTABLE` for `pfnSetCommandListDDITableCb` | 9.3 | ✅ **The runtime hands both out at device creation** (`hRTTable` `0x3E0` and `0x638`); WARP then calls `pfnSetCommandListDDITableCb(hRTCommandList, 0x3E0)` on every command-list create — observed live through the wrapped corelayer table |
| 10 | Whether the runtime accepts GPU VAs the driver never got from the kernel | 9.7 | ⛔ **STILL UNVERIFIED, and now with the reason the corpus cannot answer it.** `RecreateAtGpuva-public.md` is the closest text in the corpus and it describes **no provenance check anywhere** — the runtime only ever *reads* VAs back out of driver-created objects, and passes recorded ranges through *"not processed in any way"* (`:185`). ⭐ It is also **moot on this build**: *"RecreateAt functionality is gated behind DDI 0111."* (`:42`) and `D3D12DDI_BUILD_VERSION_0111` is absent from SDK 26100. Settler unchanged: BDAs + the debug layer at G7/G8 |
| 11 | Whether a monitored fence advances with no GPU-side write, for a **D3D12-shaped** fence | 10.5 | ⛔ **UNVERIFIED** — out of scope for the spy; still the G-fence probe |
| 12 | Whether the runtime, not the driver, performs the kernel signal/wait for `pfnSignalFence` | 10.5 | ◑ **Evidence for "the runtime does"**: across 20 frames of `ID3D12CommandQueue::Signal` + `SetEventOnCompletion`, `pfnSignalFence`/`pfnWaitForFence` were **never called** while `pfnCreateFence` was. WARP is software-scheduled — confirm on hardware |
| 13 | Whether the runtime cross-validates the caps set as ONE contract | 11.5 | ✅ **Yes, at retail** — two worked failures with the runtime's own English strings, §11.5 |
| 14 | Whether the D3D12 runtime ever passes a raw DXIL bitstream instead of a DXBC container | 12.2 | ✅ **It ALWAYS does**, and it converts DXBC to DXIL first — §12.2 |
| 15 | The exact contract of `D3D12DDICAPS_TYPE_EXECUTECOMMANDLISTS_PARALLELISM` | 11.6 | ◑ **Arm 1 run and it came back empty: the runtime never asks for 1069** on this build, in any workload. Arm 2 (force TRUE, diff a `QueuePacket` slice) is now the only route |
| 16 | Whether the runtime honours a `NOT_SUPPORTED` tier by never calling the corresponding slot | 14.1 | ✅/⛔ **Answered per-mechanism 2026-08-05, and the answers DIVERGE — so there is no general rule.** Render passes: **yes**, and by rewriting — at Tier 0 the runtime turns `BeginRenderPass` into the equivalent `OMSetRenderTargets`, which separates suppression from disinterest and closes the `D12-G5` ambiguity for that case. Programmable sample positions: the runtime **removes the device** if an app calls in, so `cl[53]`/`cl[54]` are provably unreachable. Depth bounds: the plan is runtime-supplied **stubs** in the driver's own table. ⛔ Do not generalise from one to another (§14.1.1) |
| 17 | The oldest `D3D12DDI_SUPPORTED_*` this Windows build accepts | 1.6 | ✅ **`_0040`** — and a triangle presents on it. §15.4 |
| 18 | Whether `pfnFillDDITable`'s `SIZE_T` matches `size_of` of the bindgen'd struct | 2.2 | ✅ **At `_0110`/`_0109`, exactly** (992 / 600 / 56). ⛔ And it is version-dependent — `_0089` → 976/552, `_0040` → 768/464. Honour the argument |

### 15.2 ✅ The WARP spy proxy — BUILT AND RUN, `tools/d3d12_spy/`

**The idea.** `C:\Windows\System32\d3d10warp.dll` exports `OpenAdapter12` (§1.1): Microsoft's own
D3D12 UMD, on this exact Windows build. A shim that forwards to it and logs turns the undocumented
contract into one text file, with no Helios driver change and no reboot. `DECISIONS.md` H1 called it
"unusually good" mitigation; it delivered — see §15.0 and `tmp/dx12/gates/G5/answers.md`.

**Where it is.** `tools/d3d12_spy/` (⛔ not `probe/`, not `host/` — both retired 2026-08-05):

```
gen_slots.py          generator: reads tmp/dx12/sdk/d3d12umddi.h, writes everything below
slots_{core_0109,cl_0108,queue_0001,adapter_0109,dxgi}.inc   X(index, name) slot lists
caps_types.inc  table_types.inc                              CAP(value,name,deprecated) / TBL()
spy_thunks.asm        206 + 32 generated ABI-transparent forwarders (ml64)
d3d12_warp_spy.cpp    the proxy
d3d12_spy.def         EXPORTS OpenAdapter12, OpenAdapter, OpenAdapter10_2
spy_workload.cpp      the four workloads, one exe with a mode argument
spy_workload.hlsl     the triangle, compilable as both vs_5_1 (DXBC) and vs_6_0 (DXIL)
build.ps1             cl + ml64 to a LOCAL C: path, with the count assertions
```

⚠ **Two corrections to the recipe this section used to give, both of which simply do not build.**

1. **The toolchain is `cl` + `ml64`, not WinLibs g++.** The old sheet specified
   `__declspec(naked)` + GCC inline asm: `naked` is MSVC syntax, GCC has no `naked` attribute on
   x86-64, and the WDK headers want `cl`. `GATES.md` §4.6's command block already said `cl`.
2. **`d3d12umddi.h` does not compile out of the box in user mode.** It pulls in `d3dkmddi.h`, which
   uses `NTSTATUS`, which the user-mode `windows.h` does not define. Use the same incantation the
   D3D11 UMD's bindgen wrapper does (`umd/bindgen/d3d10umddi_wrapper.h:14-18`):
   `#ifndef _NTDEF_ typedef LONG NTSTATUS, *PNTSTATUS; #endif` before the include.

⛔ Build to a **local C: path**, never `Z:\` — the 9p/virtio share fails file IO with `OS error 87`.

**Loading the real WARP.** ⚠ A full-path `LoadLibrary` of `d3d10warp.dll` from a DLL *named*
`d3d10warp.dll` returns **itself**: the loader's already-loaded check matches on base name, so
neither `LOAD_LIBRARY_SEARCH_SYSTEM32` nor a full path is sufficient (`DECISIONS.md` P-A, §6.1). The
fix that works is a **copy under a different base name**: `build.ps1` copies System32's to
`d3d10warp_real.dll` beside the proxy and asserts the SHA-256 matches; the proxy loads that by full
path and then verifies with `GetModuleFileNameW` that it got the module it asked for, refusing with a
named counter (`warp_load_failed` / `warp_wrong_path`) and `DXGI_ERROR_UNSUPPORTED` if not.

**The thunk mechanism, as built.** 124 + 75 + 7 = 206 slots, each a differently typed
`extern "system"` pointer, and §7.3(2) forbids hand-writing D3D12 slot signatures. *The thunk does
not need the signature.* `gen_slots.py` emits one eight-instruction MASM forwarder per slot:

```asm
spy_core_5 PROC
    lock inc dword ptr [g_spy_core_hits + 20]     ; per-slot total, never saturates
    mov     r11d, 1
    lock xadd dword ptr [g_spy_trace_idx], r11d   ; global event order
    cmp     r11d, 00100000h
    jae     spy_core_5_skip                       ; keep the FIRST 1Mi events
    lea     r10, [g_spy_trace]
    mov     dword ptr [r10+r11*4], 00000005h      ; (tableTag<<24)|slotIndex
spy_core_5_skip:
    jmp     qword ptr [g_spy_core_snapshot + 40]  ; tail-jump to WARP's real pointer
spy_core_5 ENDP
```

It touches only **R10, R11 and the flags** — all volatile in the Microsoft x64 ABI, none of them an
argument register — and uses **no stack at all**, so the callee sees exactly the RSP, return address
and shadow space of a direct call. Arguments in RCX/RDX/R8/R9, XMM0-3 and on the stack pass through
untouched, and RAX/XMM0 return untouched. A handful of slots that carry an answer this gate wanted
(the eight `pfnCreate*Shader` + `pfnCalcPrivateShaderSize`, the two descriptor-handle getters and
`pfnGetDescriptorSizeInBytes`, `pfnCalcPrivateHeapAndResourceSizes`, `pfnCreateHeapAndResource`,
`pfnCreateRootSignature`, and `cl[19] pfnPresent`) additionally get a **typed** C++ hook installed
over the generic one, calling through the real header typedef.

⛔ **Preserve NULL slots.** WARP leaves four of the 206 NULL and the runtime may test a slot for NULL
to detect an unsupported feature; replacing one with a thunk answers "supported" on the driver's
behalf. The NULL set is also the data for §15.1 #2.

⚠ **Regenerate the `.inc` files whenever the SDK pin moves** — a stale slot list mislabels every line
in the log, which is worse than no log. `build.ps1` asserts 124 / 75 / 7 / 8 / 32 / 43 / 25 before
compiling. And note that the names are pinned to `_0109`/`_0108`: in a **forced-old-version** run
(§15.4) the table is a different shape and only the counts and sizes are trustworthy.

**What it logs.** Adapter-table calls with full arguments; every `pfnGetCaps` `Type`/`DataSize`/
`pInfo`/`HRESULT` plus the first 64 bytes of `pData` after the call, with `D3D12_OPTIONS` and
`SHADER_MODELS` decoded field by field; every `pfnFillDDITable` `(TableType, TableSize, 5th UINT,
hRTTable)`; `pfnCreateDevice`'s `Interface`/`Version` checked against the token list; all 18
corelayer callbacks, three of them wrapped; per-slot hit counts; and a global ordered event ring in
which the log lines themselves appear as markers, so the two orderings merge exactly.

**Mutation arms** (`HELIOS_D12SPY_MUTATE`, off by default, each logged when it fires):
`range` (an out-of-range tier), `tier` + `HELIOS_D12SPY_TIERVAL` (a legal one — the control that
separates "clamped" from "ignored"), `cross` (raytracing tier vs a clamped shader-model list),
`sm65`, `capfail` (fail one cap), `forcever` + `HELIOS_D12SPY_VER` (§15.4's floor probe).

⛔ **Three traps in the mutation arms, each of which produced a confident wrong answer first:**
1. Forcing `pfnGetSupportedVersions`' **COUNT** answer down makes WARP's own FILL hit an undersized
   buffer and return `ERROR_INSUFFICIENT_BUFFER` (0x8007007A) — indistinguishable from the runtime
   rejecting the token. Edit only the FILL answer, on the way out.
2. **Not** calling WARP at all crashes at `0xC0000005` a moment later: `pfnGetSupportedVersions` is
   where WARP initialises the state `pfnCalcPrivateDeviceSize` needs.
3. The proxy's own gate (below) returns `DXGI_ERROR_UNSUPPORTED` for **everything** when the knob is
   absent. Four "the runtime refused this token" results were the spy refusing itself. **Assert the
   knob in the same command that runs the arm.**

**How to register it — both routes work, and which you need depends on the adapter.**

*Route A — app-local, no registry change, no reboot.* Put the proxy `d3d10warp.dll` beside the test
exe and reach WARP through `IDXGIFactory4::EnumWarpAdapter`. ✅ **Verified: the runtime honours it.**
`(Get-Process …).Modules` shows both `d3d10warp.dll` and `d3d10warp_real.dll` resolving to the app
directory. This is the route for everything about the *runtime's* behaviour.
⚠ **Check the module list before trusting a null result** — and check the log with
`Get-ChildItem 'C:\ProgramData\Helios' -Filter 'd3d12_spy-*.log' -File`, **never** with a wildcard
inside the path: that directory contains a junction loop and the wildcard form silently returns
nothing. Two "the proxy was never loaded" observations here were that glob, not a null result.

*Route B — registry, and the only way to reach the Helios adapter.* Point `UserModeDriverName[3]`
(index 3 of the four-entry `REG_MULTI_SZ`) at the proxy under a **different base name**
(`helios_umd12_spy.dll`, same bytes) and then **`pnputil /restart-device`** — without the restart the
runtime keeps using the path dxgkrnl cached at StartDevice and the proxy is never loaded. The proxy
receives the *Helios* adapter handle and callbacks and forwards them to WARP.

⚠⚠ **dwm.exe calls `OpenAdapter12` in production** (`DECISIONS.md` §7.13). Route B's gate, all of it
required and all of it default-refuse:
- `HKLM\SOFTWARE\Helios!UmdD3D12Spy` must be 1, read once per process — the D11 shape;
- **and** the process must be the named workload (`HELIOS_D12SPY_PROC`, default `spy_workload.exe`),
  so that even with the knob on, dwm's `OpenAdapter12` gets `DXGI_ERROR_UNSUPPORTED` — *bit-identical
  to what `helios_umd.dll` returns today* (`umd/src/adapter.rs:177-189`). The compositor cannot be
  changed by the experiment;
- set the knob, run, clear the knob, restore `UserModeDriverName`, `pnputil /restart-device` again,
  and verify the desktop with `helios_paintcap` → `Z:\tmp\screen_copy.png` before believing anything.
  Done, and the desktop was alive and composited afterwards.

**How to run it.** ⛔ **Session 1, via a cloned scheduled task**, for anything with a window — a
windowed D3D12 sample launched from `win_exec` lands in session 0, which has no desktop, and fakes a
driver regression (memory `lease-gate-falsified-60th.md`). Console-only device probes are fine from
`win_exec`.

```powershell
schtasks /create /tn helios_d12g5_window /tr "C:\Users\Rupansh\d12g5\run-window.cmd" /sc once /st 00:00 /it /rl highest /f
schtasks /run /tn helios_d12g5_window
```

(The `.cmd` wrapper exists because a scheduled task does not inherit `HELIOS_D12SPY_*` from the
creating shell.)

**The four workloads** are one binary, `spy_workload.cpp`, with a mode argument:

| mode | what | settles |
|---|---|---|
| `device` | `D3D12CreateDevice` and nothing else | 1, 5, 6, 8, 18 |
| `queue` | + command queue, pool/recorder/list, `Close` | 3, 9 |
| `window` | + swapchain, clear, present — **no shaders at all** | 2, 4, 16 |
| `triangle` | + a draw with **two** pipelines: SM 6.0 DXIL (dxc, build time) and SM 5.1 DXBC (`D3DCompile`, run time) | 14 |

⚠ **Why not `dx-samples-research-only/.../D3D12HelloWorld`, which `GATES.md` §4.6 names.** That
sample carries the Agility SDK exports — `D3D12HelloWindow.cpp:15-16`, `D3D12SDKVersion = 618` and
`D3D12SDKPath = ".\\D3D12\\"` — so whether it runs against **this Windows build's**
`D3D12Core.dll` or a NuGet one depends on whether a `D3D12\` directory happens to sit beside the exe.
This gate measures the shipping runtime; a silent runtime substitution would invalidate every line.
The samples also need `include/d3dx12/d3dx12.h` from `Microsoft.Direct3D.D3D12` 1.618.3, which is not
vendored here. The `triangle` mode's two-pipelines-in-one-process shape is also strictly better for
#14 than `HelloTriangle`: a single shader model cannot show a conversion.

⚠ `D3D12HelloTriangle` and its siblings compile with `dxc -T*_6_x`: **178 of the 180** shader-compile
steps across `dx-samples-research-only/**/*.vcxproj` target `_6_x` and **zero** target anything else;
the two `fxc /T*_5_0` steps are both in `D3D12On7`. **"FL 11_0 + Tier 1 + SM 5.1" is a valid DDI
floor but not a runnable milestone** — the first meaningful bring-up target is **FL 11_0 + SM 6.0**.

### 15.3 The second source: `D3D12Core.dll` strings

Already used throughout this document. Re-extract if the guest is updated (read-only, changes
nothing on the VM):

```powershell
# win_exec
$b=[IO.File]::ReadAllBytes("C:\Windows\System32\D3D12Core.dll")
$s=[Text.Encoding]::ASCII.GetString($b)
[regex]::Matches($s,"[\x20-\x7e]{16,400}") | %{$_.Value} | Sort-Object -Unique |
  Out-File -Encoding ascii Z:\tmp\dx12\research\d3d12core-strings.txt
```

```bash
# Linux host: regenerate the committed 270-line driver subset
cd /home/rupansh/helios-vgpu
grep -E 'Driver|driver|DDI' tmp/dx12/research/d3d12core-strings.txt \
  > docs/dx12/research/d3d12core-driverstrings.txt
```

⚠ Line numbers in this document (`strings:NN`) are into the **committed 270-line file**. Regenerating
it against a different `D3D12Core.dll` build renumbers everything.

### 15.4 ✅ The floor probe — RUN, and `_0040` is accepted

`research/R2` §5.4's experiment. ⭐ **It needed no Helios code at all**: the spy's `forcever` arm
replaces `pfnGetSupportedVersions`' answer with a single token, so the version floor was measured
against WARP with `OpenAdapter12` still refusing in `helios_umd.dll`. The R908 guard never came into
play — which is the better shape, and it should stay that way.

`HELIOS_D12SPY_MUTATE=forcever HELIOS_D12SPY_VER=<token>`, `spy_workload device --warp`:

| forced token | `pfnCalcPrivateDeviceSize` | CORE `TableSize` | CL `TableSize` | `D3D12CreateDevice` |
|---|---:|---:|---:|---|
| `_0110` `0x000c0050_006e0000` | 4016 | **992** (124 slots) | **600** (75) | ✅ `S_OK` |
| `_0109` `0x000c0050_006d0000` | 4016 | **992** (124) | **600** (75) | ✅ `S_OK` |
| `_0089` `0x000c0050_00090000` | — | **976** (122) | **552** (69) | ✅ `S_OK` |
| `_0040` `0x000c0028_00000000` | — | **768** (96) | **464** (58) | ✅ `S_OK` |

⭐ **`D3D12DDI_SUPPORTED_0040` is accepted by this Windows build (26100.8875), and `research/R2`
§5.4's predicted "96 core + 58 CL" is exactly right.** The baseline surface at `_0040` is
**96 + 58 + 7 + 8 = 169 slots instead of 214**, and state objects, mesh shaders, enhanced barriers,
work graphs and sampler feedback leave the first milestone entirely.

**And it is not merely a device that creates:** the `triangle` workload ran **ten frames, 0
failures** at `_0040` — same DXIL shader encoding, same `pfnPresent` on the command-list table
(`tmp/dx12/gates/G5/F40-triangle.log`).

⚠ **This does not decide §1.6 on its own.** `_0040` is smaller but it is also the *old* object model:
it predates the pool + recorder split and carries the retired command-**allocator** family. The
decision at P3 is between "169 slots of an older shape" and "214 of the shape a 26100-era runtime
asks for first"; what §15.4 removes is the *uncertainty*, not the choice. ⛔ Nor does the table above
mean the runtime rejects nothing: the caps gauntlet (§11.5) is unchanged at every version and is
enforced at retail.

⚠ The traps that made this arm lie three times before it told the truth are in §15.2's mutation-arm
list; two of them return HRESULTs that read exactly like the runtime rejecting the token.

---

## 16. Sizing, against the live D3D11 UMD

### 16.1 What Helios' D3D11 UMD fills today

Measured this session by re-running `research/R1` §10.1's script over `umd/src/forward/tables.rs`:

| Installer | umd/src/forward/tables.rs | Target struct | Struct slots |
|---|---|---|---|
| `install` | :72 | `D3D11DDI_DEVICEFUNCS` | 150 |
| `install_11_1` | :240 | `D3D11_1DDI_DEVICEFUNCS` | 155 |
| `install_wddm1_3` | :290 | `D3DWDDM1_3DDI_DEVICEFUNCS` | 164 |
| `install_dxgi` | :12 | `DXGI_DDI_BASE_FUNCTIONS` | — |
| `install_dxgi_1_1` | :23 | `DXGI1_1_DDI_BASE_FUNCTIONS` | — |
| `install_dxgi_1_3` | :28 | `DXGI1_3_DDI_BASE_FUNCTIONS` | — |

**Unique slots written: 157 device-table + 18 DXGI = 175.** `umd/src/forward/` is **13 283 lines**
across **19** modules (`wc -l umd/src/forward/*.rs`), and `umd/src/*.rs` is a further **5 774**
lines (re-counted 2026-08-05; these are the figures `DECISIONS.md` §4.2 quotes).

⚠ `ROADMAP.md:3300` says "`forward.rs` implements ~220 DDI functions" — ⛔ **not** `ROADMAP.md:3289`,
which is prose about Looking Glass, `\\.\DISPLAY2` and `QDC_ALL_PATHS`. The *measured* number of
distinct table slots written is **175**; 220 presumably counts handler functions including non-slot
helpers. **175 is the comparable number.**

### 16.2 The comparison

| | D3D11 (shipping) | D3D12 (`_0109` / `_0108` / `_0001`) |
|---|---|---|
| Device-table slots | 164 available (WDDM1_3), **157 filled** | **124** |
| Command-list slots | — (immediate/deferred contexts share the device table) | **75** |
| Queue slots | — | **7** |
| Adapter slots | 3 (`D3D10DDI_ADAPTERFUNCS`) + 2 in the 10_2 form | **8** |
| DXGI slots | **18 filled** | 21–22 (whole struct; shape UNVERIFIED, §2.3) |
| **Driver-side slots that must be non-NULL** | ~175 | **~214** |
| Slots needing a real body for a first frame | — | **99** (§14.2: 8 + 73 + 15 + 3) |
| Runtime→driver callback tables consumed | 1 (`D3DDDI_DEVICECALLBACKS`, 65) | **3** — `D3DDDI_ADAPTERCALLBACKS` 3, `D3D12DDI_CORELAYER_DEVICECALLBACKS_0062` **18**, `D3DDDI_DEVICECALLBACKS` 65 |
| Caps types to answer | 8 | **43** |
| Runtime-enforced caps consistency rules | 1 aggregate check (`CDevice::LLOCompleteLayerConstruction`) | **~60**, enumerated in §11.5 |
| Tiered enums the runtime range-checks | 0 | **16** (14 in the OPTIONS family + `WaveMMATier` + `WorkGraphsTier`) |
| Rust source today | **5 774** lines in `umd/src/*.rs` + **13 283** in `umd/src/forward/` (19 modules) | — |

**≈1.2× the D3D11 UMD in slot count**, and more than that in difficulty because of the object graph
(§9.9) and the caps gauntlet (§11). But the kernel-facing half is already built, the engine already
exists, and the substrate is measured green (`DECISIONS.md` D6).

**The three things with no D3D11 analogue at all**, in the order they will cost time:

1. **Root signatures** — parsed in, `RTS0` blob out (§9.9), plus the 16 root-argument slots on the
   command-list table.
2. **PSOs from handle bundles** — four sub-state objects retained and reassembled (§9.9).
3. **Descriptor heaps** — driver-owned storage, driver-chosen stride, opaque handles. ⭐ And this is
   the one that turns out *cheap*: pass vkd3d's own handles and stride straight through (§9.6). The
   only cost is the struct-return ABI (§9.6, `bridge_guard` class).

---

## 17. Corrections to the research dossiers and to `DECISIONS.md`

Every count in this document was recomputed from `d3d12umddi.h` (SDK 10.0.26100.0) for this file.
Where a source disagrees, the measurement and the command are given so the next reader can check in
one line. **None of these changes a decision** — they change a number.

| Claim | Source | Correct value | How measured |
|---|---|---|---|
| `D3D12DDICAPS_TYPE` has 42 live values | `DECISIONS.md` §3-H4, §4; `research/R1` §5.1 | **43** (R2's 48 and R8's 44 are also wrong) | `sed -n '95,149p' d3d12umddi.h \| grep -cP '^\s+D3D12DDI\w+\s+=\s+\d+,'` → 43 |
| `D3D12DDI_CORELAYER_DEVICECALLBACKS_0062` has 28 members (19/21/27/28 across versions) | `DECISIONS.md` §4; `research/R1` §4.2 | **18** (12 / 14 / 17 / 18) | the counts double-counted `#if`/`#else` arms; with gates ON the struct is 18 pointer slots — see the verbatim dump in §6.2. R2's prose ("adds 18 D3D12-specific callbacks") was right. |
| `D3D12DDI_TABLE_TYPE` has 24 values | `research/R1` §2.1 | **25** (R2's 27 is also wrong) | `sed -n '2489,2515p' \| grep -cP '^\s+D3D12DDI_TABLE_TYPE\w+\s+='` → 25. "5, 6 and 18 are absent" is correct. |
| `DEVICE_FUNCS_CORE` has 31 versions | `research/R1` §3.1 prose | **33** (R1's own table has 33 rows) | script in §Appendix A |
| `COMMAND_LIST_FUNCS_3D` has 16 versions | `research/R1` §3.2 prose | **20** (R1's own table has 20 rows) | same script |
| `D3D12DDI_SUPPORTED_0080` = `0x000C0050_00500000` | `research/R1` §1.5 | **`0x000C0050_00000000`** — `D3D12DDI_BUILD_VERSION_0080` is **0**, not 80 | `grep "#define D3D12DDI_BUILD_VERSION_0080" d3d12umddi.h`. The build-number convention changes at `_0090`; see §1.5. |
| `D3D12DDI_HANDLETYPE` enumerates 26 classes | `research/R2` §1.1 | **28** | count of umddi:330-363 |
| "~86 slots need a real body" | `DECISIONS.md` §4; `GATES.md`:23 and :943; `research/R1` §9.2 ("≈60 of 124" for the device table) | **99** total; the device-table part is **73**, not ≈60 | enumerated in §14.2, subtotal re-added there: 2+3+4+4+3+3+3+3+3+3+6+2+9+5+5+12+3 = 73, and 8+73+15+3 = 99. 99 − 12 immutable pipeline sub-state slots = 87 ≈ the "~86". ⚠ An earlier revision of §14.2 printed 71 / 97 — a mis-addition, not a different slot list. |
| `D3D12DDI_DEVICE_FUNCS_CORE_0109` group (c) "Shaders — 13", group (d) "…— 18" | this document, §3.2, earlier revision | **(c) 14, (d) 17** | counted directly from umddi:13453-13615. The shader group is 14 *distinct* slots (11 at umddi:13473-13486 + 3 at umddi:13608-13610); group (d) is 3+4+3+3+1+3 = 17. The two errors cancelled, which is why the group table still summed to 124 and the mistake survived. |
| "The runtime's own range-check messages — 14 of them" / "§11.4 The 14 tiered enums" | this document, §11.4, earlier revision | **15 messages; 16 tiered enums** | fifteen distinct `Driver filled out an invalid value in …` strings are quoted (strings:38-52, all fifteen present and distinct); §11.4's own body enumerates 16 tiered values (12 in `OPTIONS_DATA_0089` + `WriteBufferImmediateQueueFlags` + `ExecuteIndirectTier` + `WaveMMATier` + `WorkGraphsTier`), which is what §16.2 already reported. |
| D4: vkd3d's `d3d12core` target renamed "with **one** added export" | `DECISIONS.md` D4 (pre-§6.1); `ARCHITECTURE.md`:62 and :768-810 | **two exports**: `helios_vkd3d_create_device` **and** `helios_vkd3d_serialize_root_signature` | `vkd3d_serialize_root_signature` is declared at `include/vkd3d.h:129` and defined at `libs/vkd3d/vkd3d_main.c:453`, but `libs/d3d12core/d3d12core.def` exports exactly `D3D12GetInterface` + `D3D12SDKVersion DATA PRIVATE` — nothing else. Root signatures arrive at the DDI **already parsed** (H3), so the UMD must re-serialize. Signatures in §9.9. ⚠ `DECISIONS.md` D4 now carries both; `ARCHITECTURE.md` does not, and `ARCHITECTURE.md:456` assigns `src/forward12/pso.rs` to call the serializer with no export path — that is a link failure waiting in the PSO tranche. |
| `D3D12CreateDevice` lives in `libs/d3d12core/main.c` | assorted research prose | **`libs/d3d12/main.c:143`** — the thin `d3d12.dll` target Helios does not use | the DXGI-touching path *inside* `d3d12core.dll` is `d3d12core_CreateDeviceFromFactory` (`libs/d3d12core/main.c:643`), reachable only through `D3D12GetInterface`, calling `CreateDXGIFactory1` at `:383` and `:406`. That — not `D3D12CreateDevice` — is the path D4's exports exist to bypass. |
| `… must be either monitored fences with GPU access or native fences.` cited as a general D3D12 fence statement | `research/R2` §2.4 | it is a **video-encode** validation string (`ID3D12VideoEncodeCommandList::EncodeFrame … ppSubregionFences[%d]`, fullstrings:20702) | the conclusion stands on the `D3D12DDI_FENCE` shape itself; the string is not the evidence |
| `pfnGetSupportedVersions` / `pfnFillDDITable` unaffected | — | ✔ verified unchanged | — |
| `d3d12umddi.h` is 19 031 lines; 72 `D3D12DDI_SUPPORTED_*`; adapter 8; core 124; CL 75; queue 7; `D3DDDI_DEVICECALLBACKS` 65; `D3DDDI_ADAPTERCALLBACKS` 3 | `research/R1`, `DECISIONS.md` §4 | ✔ **all confirmed** | Appendix A |

**Two new findings not in any dossier**, both load-bearing:

1. ⭐ **Shader model 5.1 is mandatory in the `_0011_SHADER_MODELS` list and the list must be
   gapless** — `For now, driver must include shader model 5.1 support…` (strings:191),
   `Driver cannot have gaps in reported support for release shader models…` (strings:19). So an
   SM-6.0 driver reports `{5_1_RELEASE, 6_0_RELEASE}`, not `{6_0_RELEASE}` (§11.5g).
2. ⚠ **`SupportsRowMajorTexture` couples to a KMD cap** —
   `Driver set D3D12DDICAPS_TEXTURE_LAYOUT::SupportsRowMajorTexture but not DXGK_VIDMMCAPS::CrossAdapterResourceTexture.`
   (strings:92). The only D3D12 cap found with a `kmd_render` counterpart (§11.5f).

Plus two smaller ones: `D3D12DDICAPS_MEMORY_ARCHITECTURE::IOCoherent` **must be TRUE on amd64**
(strings:89, §11.5e); and the Render Pass tier↔table consistency is checked in **both** directions
(strings:73-74, §11.5c).

### 17.1 Cross-document counts — reconciled 2026-08-05

`DECISIONS.md` §4.1 is the canonical count table and this document agrees with it in full. The
divergences this document originally recorded here (`D3D12DDICAPS_TYPE` 40/42, `D3D12DDI_TABLE_TYPE`
27, `CORELAYER_DEVICECALLBACKS` 28, "~86" real-body slots, one vkd3d export) were all **closed in
the same correction pass** — `ARCHITECTURE.md`, `GATES.md` and `DECISIONS.md` now carry 43 / 25 /
12-14-17-18 / 99 / two exports. Nothing in `docs/dx12/` disagrees with §4.1 today.

⚠ **The rule that produced them still applies:** a count in this directory is only trustworthy if it
is in `DECISIONS.md` §4.1 or derived here with the Appendix A script. Three of these five were
miscounted independently by more than one research lane, each in a way a `grep -c` reproduces:

| Trap | What a naive count gives | Why it is wrong |
|---|---|---|
| `D3D12DDICAPS_TYPE` | 40 | counts only the `D3D12DDICAPS_TYPE_`-prefixed names; the other three (`D3D12DDI_FEATURE_D3D12_PREDICATION_106`, `…_PLACED_RESOURCE_SUPPORT_INFO_106`, `…_HARDWARE_COPY_106`) are members of the *same* enum and the runtime validates them (strings:7-9). There are **no** versioned `D3D12DDICAPS_TYPE_00xx_*` additions elsewhere in the header |
| `D3D12DDI_TABLE_TYPE` | 27 | 27 is the highest assigned **value**, not a count; the value space has gaps at 5, 6 and 18 (§2.1) |
| `D3D12DDI_CORELAYER_DEVICECALLBACKS_*` | 28 | counts both arms of the `#if`/`#else` blocks; ten members have same-offset `void* pfnReserved…` alternates. Gates-ON gives 12 / 14 / 17 / 18 (§6.2, Appendix A) |
| real-body DDI slots | "~86" | an early estimate; §14.2 enumerates and re-adds **99** (8 adapter + 73 device core + 15 command list + 3 queue), or 87 excluding the 12 immutable pipeline sub-state slots — which is where ~86 came from |
| vkd3d added exports | 1 | `vkd3d_serialize_root_signature` is not exported from any vkd3d DLL, and root signatures arrive at the DDI already parsed, so a second export is mandatory (D4, §9.9) |

### 17.2 Citation drift corrected in this pass

This document is line-pinned; a citation that does not resolve is a defect in it. Ten were wrong and
are fixed above. Recorded here so the same mistakes are recognisable next time.

| Where | Was | Is |
|---|---|---|
| §5 table | `pfnUnused` 2731, `pfnUnused2` 2732, `pfnSignalFence` 2718, `pfnWaitForFence` 2719 | members **2732 / 2733 / 2736 / 2737**; 2731 is `pfnExecuteCommandLists`, 2718 is blank, 2719 is the `PFND3D12DDI_SIGNAL_FENCE` typedef. The column now cites **struct-member** lines throughout, with typedefs in Notes |
| §4.2 sample signatures | umddi:1750, 1751-1755, 1735, 2731 | umddi:**1750, 1751-1756, 1767, 1769** (typedef lines) |
| §3.2(k), §13 item 2 | `pfnGetPresentPrivateDriverDataSize` at umddi:1795 | typedef at umddi:**1792**; 1795 is inside `PFND3D12DDI_SERIALIZEOBJECT` |
| §3.1, §11.2 | `_0090` caps-convention comment at umddi:11121-11124 / 11123 | comment block umddi:**11122-11125**; the quoted sentence is at **11125** |
| §2.1 table | `EXTENDED_FEATURES_FUNCS_0020` at 4087 | typedef at **4086** |
| §7.1 | UMD handle types "umddi:65-89" | **65-90** — the sixteenth, `D3D12DDI_HSTATEOBJECT_0054`, is at :90 |
| §9.6 | `d3d12_desc_heap_iface *iface`, `static` dropped | `static … (ID3D12DescriptorHeap *iface, …)` — `resource.c:9146-9147` |
| §12.3 | `vkd3d_shader_main.c:212` | **:213** |
| §12.4 | `d3d12_device_validate_shader_meta` at `device.c:11670-11790` | **11671-11795** |
| §10.4 | `ROADMAP.md:2605-2610` for `vehicle_flipwait_probe.c` | **`ROADMAP.md:2616`**; 2603-2612 is the 25th-session fence-event A/B and does not mention the probe |
| §16.1 | `ROADMAP.md:3289` for "~220 DDI functions" | **`ROADMAP.md:3300`** |
| §12.4 | "`khronos/Vulkan-Headers` / `SPIRV-Headers` are empty directories [under `subprojects/`]" | they are **repo-root** submodule paths and do not exist on disk at all; only `subprojects/dxil-spirv/` exists-and-is-empty |
| §15.2 | "174 of 178 shader-compile steps use `dxc -T*_6_x`" | **178 of 180**; the two `fxc /T*_5_0` steps are both in `D3D12On7` |
| §2.3 | "the only `DXGI` tokens … are umddi:1620-1621 and 2493" | the verifiable claim is that **no `DXGI*_DDI_BASE_FUNCTIONS` struct is in the header** (`grep -c` → 0); `DXGI_RATIONAL` / `DXGI_COLOR_SPACE_TYPE` occur at 26 further sites in the video sections |

Two other host-side facts re-derived in the same pass, recorded so nobody re-measures them:
**`ROADMAP.md:2919-2926` and `:2948-2950`** are the `HELIOS_WSI_INSURANCE_BLIT` A/B (measured
**inert** at Doom resolution — it is not an unmeasured cost), **`ROADMAP.md:2919-2931`** *is* the
post-fix fullscreen vehicle measurement (the open item is only that it was taken with
`VehicleKernelFlipWait=1`, retired by R912(a), so it needs re-measuring on the shipping gate path),
and the venus-level host logging lever is **`HELIOS_VKR_DEBUG=validate`** (owner-gated relaunch),
**not** `VIRGL_LOG_LEVEL=debug` — `ROADMAP.md:1901-1903`.

---

## Appendix A — reproducing every count in this document

```bash
cd /home/rupansh/helios-vgpu/tmp/dx12/sdk

# the pin: 19031 lines, SDK 10.0.26100.0
wc -l d3d12umddi.h

# 72 version constants, and the build-number convention change at _0090
grep -c "^#define D3D12DDI_SUPPORTED_" d3d12umddi.h
grep -oP "^#define D3D12DDI_BUILD_VERSION_\K\d+\s+\d+" d3d12umddi.h
grep -oP "^#define D3D12DDI_SUPPORTED_\K\d+.*INTERFACE_VERSION_R\d" d3d12umddi.h | sed -E 's/ .*_(R[0-9])/ \1/'

# 43 live D3D12DDICAPS_TYPE values; 25 D3D12DDI_TABLE_TYPE values
sed -n '95,149p'   d3d12umddi.h | grep -cP '^\s+D3D12DDI\w+\s+=\s+\d+,'
sed -n '2489,2515p' d3d12umddi.h | grep -cP '^\s+D3D12DDI_TABLE_TYPE\w+\s+='

# every DEVICE_FUNCS_CORE / COMMAND_LIST_FUNCS_3D version, span and member count
python3 - <<'EOF'
import re
L=open('d3d12umddi.h',encoding='utf-8',errors='replace').read().split('\n')
pat=re.compile(r'^typedef struct (D3D12DDI_(DEVICE_FUNCS_CORE|COMMAND_LIST_FUNCS_3D)_\d+)\s*$')
for i,l in enumerate(L):
    m=pat.match(l)
    if not m: continue
    j=i; n=0
    while not L[j].startswith('}'):
        if re.match(r'\s*(PFN\w+|void\*|VOID\*)\s+\w+;', L[j]): n+=1
        j+=1
    print(f"{m.group(1):42s} L{i+1:5d}-{j+1:5d} members={n}")
EOF

# corelayer callback slot counts with version gates ON (12 / 14 / 17 / 18)
python3 - <<'EOF'
import re
L=open('d3d12umddi.h',encoding='utf-8',errors='replace').read().split('\n')
def count(a,b):
    n=0; skip=False
    for i in range(a,b-1):
        s=L[i].strip()
        if s.startswith('#if'):    skip=False; continue
        if s.startswith('#else'):  skip=True;  continue
        if s.startswith('#endif'): skip=False; continue
        if skip: continue
        if re.match(r'\s*(PFN\w+|void\*|VOID\*)\s+\w+;', L[i]): n+=1
    return n
for name,a,b in [('_0003',2624,2653),('_0022',4874,4905),('_0050',7178,7218),('_0062',8606,8647)]:
    print('CORELAYER'+name, count(a,b))
EOF

# the four baseline tables
for r in "13451 13616 CORE_0109" "13303 13388 CL_3D_0108" "2729 2738 QUEUE_0001" "13640 13650 ADAPTER_0109"; do
  set -- $r; echo -n "$3: "; sed -n "$1,$2p" d3d12umddi.h | grep -cP '^\s*(PFN\w+|void\*|VOID\*)\s+\w+;'
done

# the absence claims — every one must print 0
for p in GETDDITABLE GetDDITable pfnPresentCb pfnRenderCb FROM_CPU FromCpu FROMCPU \
         BytecodeLength SHADER_BYTECODE pCommandBuffer AllocationList PatchLocationList; do
  printf "%-20s %s\n" "$p" "$(grep -c "$p" d3d12umddi.h)"; done

# D3DDDI_DEVICECALLBACKS = 65, no #else arms; D3DDDI_ADAPTERCALLBACKS = 3
sed -n '4499,4586p' d3dumddi.h | grep -cP '^\s*(PFND3DDDI\w+|VOID\*|void\*)\s+\w+;'
sed -n '4499,4586p' d3dumddi.h | grep -c '#else'
sed -n '4633,4640p' d3dumddi.h

# P-C: pKTCallbacks is D3DDDI_DEVICECALLBACKS, and it carries pfnRenderCb + pfnPresentCb
sed -n '13623p' d3d12umddi.h
grep -nE '^\s*PFND3DDDI_(RENDERCB|PRESENTCB)\s' d3dumddi.h

# §3.2's group counts must re-sum to 124 -- (c) is 14 and (d) is 17, not 13 and 18
python3 -c "print(sum([3,12,14,17,12,15,11,5,3,3,5,3,6,13,2]))"     # -> 124
# §14.2's device-core subtotal, and the grand total
python3 -c "s=[2,3,4,4,3,3,3,3,3,3,6,2,9,5,5,12,3]; print(sum(s), 8+sum(s)+15+3)"   # -> 73 99

# the four baseline tables, again as one line each (8 / 124 / 75 / 7 -> 214)
python3 -c "print(8+124+75+7)"                                       # -> 214

# the command-queue triple inside CORE_0109 is members 27, 28, 29 -- NOT "slots 38-40"
sed -n '13488,13490p' d3d12umddi.h
```

```bash
cd /home/rupansh/helios-vgpu
# §11.4: 15 range-check strings, not 14
sed -n '38,52p' docs/dx12/research/d3d12core-driverstrings.txt | wc -l
grep -c 'Driver filled out an invalid value' docs/dx12/research/d3d12core-driverstrings.txt

# §12.4: all three vkd3d submodules are uninitialised ("-" prefix on every line)
git -C vkd3d-proton-helios submodule status
ls vkd3d-proton-helios/subprojects/               # dxil-spirv, and nothing else
ls vkd3d-proton-helios/khronos 2>&1               # No such file or directory
sed -n '2,5p' vkd3d-proton-helios/libs/d3d12core/d3d12core.def   # exactly two exports

# §15.2: 178 dxc -T steps, every one _6_x; 2 fxc /T*_5_0 steps, both in D3D12On7
grep -rhoP 'dxc\.exe[^<]*?-T\s*\S+' dx-samples-research-only --include=*.vcxproj | wc -l
grep -rhoP 'dxc\.exe[^<]*?-T\s*\S+' dx-samples-research-only --include=*.vcxproj | grep -vc '_6_'
grep -rlP 'fxc\.exe' dx-samples-research-only --include=*.vcxproj
```

```bash
cd /home/rupansh/helios-vgpu
# D3D11 UMD slot counts: 157 device + 18 DXGI, 13283 lines
wc -l umd/src/forward/*.rs | tail -1
python3 - <<'EOF'
import re
lines=open('umd/src/forward/tables.rs',encoding='utf-8').read().split('\n')
cur=None; sets={}
for l in lines:
    m=re.match(r'pub unsafe fn (\w+)',l)
    if m: cur=m.group(1); sets.setdefault(cur,set())
    mm=re.match(r'\s*f\.(pfn\w+)\s*=',l)
    if cur and mm: sets[cur].add(mm.group(1))
dev  = sets['install']|sets['install_11_1']|sets['install_wddm1_3']
dxgi = sets['install_dxgi']|sets['install_dxgi_1_1']|sets['install_dxgi_1_3']
print(len(dev), len(dxgi))   # -> 157 18
EOF
```

On win11, read-only, no build, no install:

```powershell
$db = "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64\dumpbin.exe"
& $db /exports C:\Windows\System32\d3d10warp.dll | Select-String "OpenAdapter"
```

## Appendix B — where the rest of the doc set picks this up

| Document | Takes from here |
|---|---|
| `docs/dx12/ARCHITECTURE.md` | §1.1 (the export and the two-DLL split), §2.2 (`pfnFillDDITable` and bindgen discipline), §7.1 (`handles.rs` reuse), §9.9 (the second vkd3d export: a root-signature serializer), §14.3 (the install discipline) |
| `docs/dx12/PRESENT.md` | §13 in full, §4.2 (`pfnPresent`/`pfnBlt` are command-list slots), §2.3 (the DXGI table shape question), §8.3 (the watermark) |
| `docs/dx12/SUBSTRATE.md` | §11.7 (the shader-model ceiling and the probe that settles it), §11.5e (UMA / IOCoherent), §12.4 (dxil-spirv submodule prerequisite), §8.3 (per-ECL watermark from vkd3d) |
| `docs/dx12/GATES.md` | §10.5 (G-fence probe), §15.2 (the spy, its thunk generator and the four workloads), §15.4 (the version-floor probe), §14.2 (the **99**-slot checklist as a gate — ⚠ `GATES.md:23`/`:943` still size G8 on "~86"; §17.1), §9.7 (the debug-layer VA acceptance run) |
| `docs/dx12/KMD_IMPACT.md` | §13 item 3 (`pfnRenderCb` via `pKTCallbacks` ⇒ **no KMD change** for the present identity channel, and ⛔ no `DxgkDdiSubmitCommandVirtual` decode), §11.5f (the `SupportsRowMajorTexture` ↔ `DXGK_VIDMMCAPS::CrossAdapterResourceTexture` coupling), §9.11 (`ComputeQueuesPer3DQueue = 0` and the `DxgkDdiCreateHwQueue` refusal), §10.5 (the G-fence probe, whose FAIL branch is a KMD work item) |
| `ROADMAP.md` | §11.6 item 5 (`TotalLaneCount = 1024` is already wrong — file it before someone spends a session on it), §11.5f (the `SupportsRowMajorTexture` ↔ `DXGK_VIDMMCAPS::CrossAdapterResourceTexture` coupling) |
| `DECISIONS.md` | §17's corrections to §3-H4 / §4's counts; §17.1's live divergences |
