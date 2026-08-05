# `D12-G5` — contract capture: what the WARP spy proxy actually saw

**Run 2026-08-05.** Instrument: `tools/d3d12_spy/` (proxy `d3d10warp.dll` + generated
`spy_thunks.asm`, built by `build.ps1` to `C:\Users\Rupansh\d12g5`).
Real driver behind the proxy: `C:\Windows\System32\d3d10warp.dll` **10.0.26100.8875**,
sha256 `6F44D3AA…B2192` (`warp-identity.txt`). Runtime under test: this guest's own
`C:\Windows\System32\{d3d12.dll,D3D12Core.dll,dxgi.dll}` — no Agility SDK substitution
(see `spy_workload.cpp`'s header for why the MS samples were not used).
Header pin for every slot name and enumerator: SDK **10.0.26100.0**, `d3d12umddi.h`, 19 031 lines.

⛔ Helios is **not** in this path on the Route A runs. WARP is the driver; the spy only logs.

## 0. Which route reached the runtime — and the thing that nearly faked a null result

`DDI_REFERENCE.md` §15.2 marked Route A (app-local `d3d10warp.dll`) **UNVERIFIED** and told the
next reader to *check which module is mapped before trusting a null result*. That check earned its
keep twice:

* **Route A works.** With the proxy beside the exe, `(Get-Process …).Modules` shows
  `d3d10warp.dll → C:\Users\Rupansh\d12g5\d3d10warp.dll` **and** `d3d10warp_real.dll` from the same
  directory. The runtime honours an app-local WARP; no registry change, no reboot.
* ⚠ **The first two "no log" observations were a bad glob, not a null result.**
  `Get-ChildItem "C:\ProgramData\Helios\d3d12_spy-*.log"` returned nothing while
  `Get-ChildItem 'C:\ProgramData\Helios' -Filter 'd3d12_spy-*.log' -File` listed three logs that had
  been written all along. `C:\ProgramData\Helios` contains a junction loop; use `-Filter`, never a
  wildcard inside the path.
* **Route B also works, and is needed for anything about the Helios adapter** (§7 below):
  point `UserModeDriverName[3]` at `helios_umd12_spy.dll` **and** `pnputil /restart-device` — without
  the restart the runtime keeps using the path dxgkrnl cached at StartDevice and the proxy is never
  loaded. Registry restored and desktop verified with `helios_paintcap` afterwards.

## 1. Pass criterion — containment, and it passes

| Check | Result |
|---|---|
| every `Type` in the log ⊆ the 43 `D3D12DDICAPS_TYPE` enumerators | ✅ **0 outside**; 23 distinct types observed |
| the 7 deprecated enumerators (1000, 1001, 1010, 1058, 1063, 1064, 1065) absent | ✅ **none asked** |
| each of the 36 live enumerators recorded as *asked, DataSize N* or *not asked* | ✅ table in §2 |
| the full `pfnGetSupportedVersions` / `OpenAdapter12` negotiation | ✅ §3 |
| every `pfnFillDDITable(TableType, TableSize)` pair | ✅ §4 |
| an ordered first-frame call trace | ✅ §6 |

**Workloads** (`spy_workload.cpp`; `A`/`B` from `win_exec`, `C`/`D` through a session-1 cloned
scheduled task): `A device` · `B queue` · `C window` (clear + present ×20, **no shaders**) ·
`D triangle` (two pipelines: SM 6.0 DXIL and SM 5.1 DXBC). `spy.log` is `D-triangle.log`, the
superset run; the others are kept beside it.

## 2. The caps table — every one of the 43, measured

⭐ **The asked set is identical in all four workloads.** Caps are answered entirely during adapter
open and device creation; nothing a workload does adds a query. So "not asked during HelloWindow" is
here the stronger statement "not asked by this runtime at all, for any of the four workloads".

`calls` is per run. `runs` = which workloads asked (A device / B queue / C window / D triangle).

| value | enumerator | asked | DataSize | pInfo | HRESULTs seen | calls/run |
|---:|---|---|---:|---|---|---:|
| 1000 | `D3D12DDICAPS_TYPE_TEXTURE_LAYOUT` *(deprecated)* | no | – | – | – | 0 |
| 1001 | `D3D12DDICAPS_TYPE_SWIZZLE_PATTERN` *(deprecated)* | no | – | – | – | 0 |
| 1002 | `D3D12DDICAPS_TYPE_MEMORY_ARCHITECTURE` | **yes** | **20** | non-NULL (`NodeIndex`) | `S_OK` | 1 |
| 1003 | `D3D12DDICAPS_TYPE_TEXTURE_LAYOUT_SETS` | **yes** | **20** | non-NULL | `S_OK`, then `E_UNEXPECTED` ×2 | 3 |
| 1004 | `D3D12DDICAPS_TYPE_SHADER` | **yes** | **64** | NULL | `S_OK` | 1 |
| 1005 | `D3D12DDICAPS_TYPE_ARCHITECTURE_INFO` | **yes** | **4** | NULL | `S_OK` | 1 |
| 1006 | `D3D12DDICAPS_TYPE_D3D12_OPTIONS` | **yes** | **124** | NULL | `S_OK` | 1 |
| 1007 | `D3D12DDICAPS_TYPE_3DPIPELINESUPPORT` | **yes** | **4** | NULL | `S_OK` | 1 |
| 1009 | `D3D12DDICAPS_TYPE_GPUVA_CAPS` | **yes** | **4** | non-NULL (`NodeIndex`) | `S_OK` | 1 |
| 1010 | `D3D12DDICAPS_TYPE_TEXTURE_LAYOUT1` *(deprecated)* | no | – | – | – | 0 |
| 1012 | `D3D12DDICAPS_TYPE_0011_SHADER_MODELS` | **yes** | **16** | NULL | `S_OK` | 2 |
| 1013 | `D3D12DDICAPS_TYPE_OPTIONS1_0103` | **yes** | **4** | NULL | `S_OK` | 1 |
| 1057 | `D3D12DDICAPS_TYPE_0030_PROTECTED_RESOURCE_SESSION_SUPPORT` | no | – | – | – | 0 |
| 1058 | `D3D12DDICAPS_TYPE_0030_CRYPTO_SESSION_SUPPORT` *(deprecated)* | no | – | – | – | 0 |
| 1059 | `D3D12DDICAPS_TYPE_0022_CPU_PAGE_TABLE_FALSE_POSITIVES` | **yes** | **4** | non-NULL | `S_OK` | 1 |
| 1060 | `D3D12DDICAPS_TYPE_0022_TEXTURE_LAYOUT` | **yes** | **20** | **NULL** | `S_OK` | 1 |
| 1061 | `D3D12DDICAPS_TYPE_0022_SWIZZLE_PATTERN` | no | – | – | – | 0 |
| 1062 | `D3D12DDICAPS_TYPE_0023_UMD_BASED_COMMAND_QUEUE_PRIORITY` | **yes** | **4** | NULL | `S_OK` | 1 |
| 1063 | `…_0030_CONTENT_PROTECTION_SYSTEM_COUNT` *(deprecated)* | no | – | – | – | 0 |
| 1064 | `…_0030_CONTENT_PROTECTION_SYSTEM_SUPPORT` *(deprecated)* | no | – | – | – | 0 |
| 1065 | `…_0030_CRYPTO_SESSION_TRANSFORM_SUPPORT` *(deprecated)* | no | – | – | – | 0 |
| 1066 | `D3D12DDICAPS_TYPE_0033_ADAPTER_COMPUTE_ONLY` | no | – | – | – | 0 |
| 1067 | `D3D12DDICAPS_TYPE_0050_HARDWARE_SCHEDULING_CAPS` | **yes** | **4** | NULL | `S_OK` | 1 |
| 1068 | `D3D12DDICAPS_TYPE_QUERY_META_COMMAND_CAPS_0061` | no | – | – | – | 0 |
| 1069 | `D3D12DDICAPS_TYPE_EXECUTECOMMANDLISTS_PARALLELISM` | **no** | – | – | – | 0 |
| 1070 | `D3D12DDICAPS_TYPE_SAMPLER_FEEDBACK_0073` | no | – | – | – | 0 |
| 1071 | `D3D12DDICAPS_TYPE_0073_SUPPORT_BATCHED_MARKERS` | **yes** | **4** | NULL | `S_OK` | 1 |
| 1072 | `…_0074_PROTECTED_RESOURCE_SESSION_TYPE_COUNT` | no | – | – | – | 0 |
| 1073 | `…_0074_PROTECTED_RESOURCE_SESSION_TYPES` | no | – | – | – | 0 |
| 1074 | `D3D12DDICAPS_TYPE_0081_3DPIPELINESUPPORT1` | **yes** | **8** | NULL | **`E_UNEXPECTED` (0x8000ffff)** | 1 |
| 1075 | `D3D12DDICAPS_TYPE_0103_WAVE_MMA` | no | – | – | – | 0 |
| 1077 | `D3D12DDICAPS_TYPE_OPTIONS_0090` | **yes** | **4** | NULL | `S_OK` | 1 |
| 1078 | `D3D12DDICAPS_TYPE_OPTIONS_0091` | **yes** | **16** | NULL | `S_OK` | 1 |
| 1079 | `D3D12DDICAPS_TYPE_OPTIONS_0093` | **yes** | **8** | NULL | `S_OK` | 1 |
| 1080 | `D3D12DDICAPS_TYPE_OPTIONS_0098` | **yes** | **4** | NULL | **`E_UNEXPECTED`** | 1 |
| 1081 | `D3D12DDICAPS_TYPE_OPTIONS_0101` | no | – | – | – | 0 |
| 1082 | `D3D12DDICAPS_TYPE_OPTIONS_0102` | **yes** | **16** | NULL | `S_OK` | 1 |
| 1084 | `D3D12DDI_FEATURE_D3D12_PREDICATION_106` | no | – | – | – | 0 |
| 1085 | `D3D12DDI_FEATURE_PLACED_RESOURCE_SUPPORT_INFO_106` | no | – | – | – | 0 |
| 1086 | `D3D12DDI_FEATURE_HARDWARE_COPY_106` | no | – | – | – | 0 |
| 1087 | `D3D12DDICAPS_TYPE_OPTIONS_0109` | **yes** | **4** | NULL | `S_OK` | 1 |
| 1088 | `D3D12DDICAPS_TYPE_OPTIONS_0110` | **yes** | **4** | NULL | `S_OK` | 1 |
| 1091 | `D3D12DDICAPS_TYPE_SHADER_MODEL_6_8_OPTIONS_0110` | **yes** | **8** | NULL | `S_OK` | 1 |

**23 asked, 13 of the 36 live ones never asked.** Three details worth their own line:

* **Two caps are asked before anything is negotiated**, on a bare adapter with no version and no
  device: `1074` then `1007`. A UMD's `pfnGetCaps` must answer them without knowing which DDI
  version it is speaking.
* **`1003 TEXTURE_LAYOUT_SETS` is an enumeration, not a query.** The runtime calls it with
  `pInfo = {1,0}`, `{1,1}`, `{1,2}` and stops when the driver returns a failure. Answering `S_OK`
  forever would loop it.
* **A failing HRESULT on a caps type is tolerated.** WARP itself fails `1074` and `1080` with
  `E_UNEXPECTED` on every run and the device still creates; the `capfail` arm (return
  `E_INVALIDARG` for `1088` without calling WARP) also creates the device. ⇒ §11.2's UNVERIFIED is
  answered: *unrecognised/failed cap ⇒ treated as unsupported.* ⛔ This does **not** license the
  ~13 caps with an explicit "device creation fails" string — those were all answered here.

WARP's own answers, for reference (`A-device.log`): `3DPIPELINELEVEL = 13` (FL 12_1),
`OPTIONS: ResourceBindingTier 3, TiledResourcesTier 3, ConservativeRasterTier 3, ResourceHeapTier 2,
RenderPassTier 0, RaytracingTier 1.1, MeshShaderTier 1, SamplerFeedbackTier 1.0,
EnhancedBarriers 1`; `ARCHITECTURE_INFO.TileBasedDeferredRenderer = 0`;
`GPUVA_CAPS.MaxGPUVirtualAddressBitsPerResource = 32`;
`HARDWARE_SCHEDULING_CAPS.ComputeQueuesPer3DQueue = 0`;
`SHADER_MODELS = {5.1, 6.0, 6.1, 6.2, 6.3, 6.4, 6.5, 6.6, 6.7, 6.8}` — gapless, starting at 5.1,
exactly as `DDI_REFERENCE.md` §17's finding 1 predicted.

## 3. Negotiation

```
OpenAdapter12(hRTAdapter, pAdapterCallbacks) -> S_OK, all 8 adapter slots non-NULL
pfnGetCaps 1074            -> E_UNEXPECTED
pfnGetCaps 1007            -> S_OK, level 13
pfnGetSupportedVersions(*puEntries=0, pVersions=NULL)   -> S_OK, *puEntries = 77   <-- COUNT
pfnGetSupportedVersions(*puEntries=77, pVersions=buf)   -> S_OK, 77 entries        <-- FILL
pfnCalcPrivateDeviceSize(Interface=0x000c0050, Version=0x006e0000, Flags=0) -> 4016
pfnCreateDevice(same Interface/Version, …) -> S_OK
```

* **#8 answered: yes, it is a two-call count-then-fill query**, with `pVersions == NULL` on the first.
* **#5 answered: `Interface` is the high 32 bits of the `D3D12DDI_SUPPORTED_*` token and `Version`
  the low 32.** Proven, not inferred: `((UINT64)Interface << 32) | Version` equals
  `version[76]` of the list WARP returned, bit for bit. ⚠ The first version of this spy capped its
  capture at 64 entries and reported "NO MATCH" for exactly this reason — a truncated instrument
  reads like a finding.
* **#6, this build: `D3D12DDI_SUPPORTED_0110`** (`0x000c0050_006e0000`) — the **newest** entry WARP
  offers, and the runtime takes it. WARP's 77 entries are **13 D3D11-era** tokens
  (`0x000b0020…0x000b002d`) followed by **64 D3D12** ones from `_0003` to `_0110`. So one list
  carries both DDIs.

## 4. Tables

| TableType | name | TableSize | slots | 5th `UINT` | `hRTTable` |
|---:|---|---:|---:|---:|---|
| 0 | `DEVICE_CORE` | **992** | 124 | 0 | `NULL` |
| 1 | `COMMAND_LIST_3D` | **600** | 75 | **0** | `0x3E0` |
| 1 | `COMMAND_LIST_3D` | **600** | 75 | **1** | `0x638` |
| 2 | `COMMAND_QUEUE_3D` | **56** | 7 | 0 | `NULL` |
| 27 | `0096_EXTENDED_FEATURES` | **32** | 4 | 0 | `NULL` |
| 3 | `DXGI` | — | — | — | **never requested** |

* **#18 answered: `TableSize` is exactly `size_of` the header's struct** — 992 / 600 / 56 match
  `D3D12DDI_DEVICE_FUNCS_CORE_0109`, `…COMMAND_LIST_FUNCS_3D_0108`, `…COMMAND_QUEUE_FUNCS_CORE_0001`
  to the byte. ⛔ That is not licence to write `size_of::<T>()` bytes — see §5, where an older
  negotiated version makes the runtime pass **976 / 552** and **768 / 464**.
* **#3 answered for the 5th `UINT`: it is the command-list table *index*.** The runtime fills
  `COMMAND_LIST_3D` **twice** at device creation, with indices 0 and 1 and two distinct `hRTTable`
  handles. `numTables` itself is still unexercised: `pfnGetOptionalDDITables` was called once and
  WARP answered `*puEntries = 0`.
* **#4 answered, negatively and usefully: `D3D12DDI_TABLE_TYPE_DXGI` is never requested** — not at
  device creation, not by a flip-model swapchain, not across 20 presents. 32 generic DXGI thunks were
  installed and armed; **0 of 32 were ever called.** Present reaches the driver on the *command-list*
  table (`cl[19] pfnPresent`). ⇒ On this Windows build a D3D12 UMD needs **no DXGI table at all**.
* **Table type 27 (`0096_EXTENDED_FEATURES`) is filled unconditionally**, without any
  extended-features handshake, for a plain baseline device. `DDI_REFERENCE.md` §2.1's "a baseline
  device needs exactly four: 0, 1, 2, 3" is wrong twice over: 3 is not asked and 27 is.
  ⚠ **And the type is version-dependent**: at `_0089` and `_0040` the runtime fills type **8**
  (`0020_EXTENDED_FEATURES`) instead, same 32 bytes / 4 slots.
* **NULL slots are legal and WARP uses that.** WARP left `core[121] pfnImplicitShaderCacheControl`,
  `cl[69] pfnOmSetAlphaBlendFactor`, `queue[1] pfnUnused` and `queue[2] pfnUnused2` NULL, and the
  device works. The spy preserves NULLs deliberately — replacing one with a thunk would answer
  "supported" on the driver's behalf. (**#2**, partially: *at least* these four may be NULL. Proving
  the other 202 must be non-NULL needs a null-one-at-a-time experiment this gate did not run.)

## 5. §15.4's version-floor probe — run, and the answer changes the project's size

The `forcever` arm replaces `pfnGetSupportedVersions`' answer with **one** token. Every one of these
created a device on the WARP adapter:

| forced token | `pfnCalcPrivateDeviceSize` | CORE TableSize | CL TableSize | device |
|---|---|---:|---:|---|
| `_0110` `0x000c0050_006e0000` | 4016 | **992** (124) | **600** (75) | ✅ |
| `_0109` `0x000c0050_006d0000` | 4016 | **992** (124) | **600** (75) | ✅ |
| `_0089` `0x000c0050_00090000` | — | **976** (122) | **552** (69) | ✅ |
| `_0040` `0x000c0028_00000000` | — | **768** (96) | **464** (58) | ✅ |

⭐ **`D3D12DDI_SUPPORTED_0040` is accepted by this Windows build, and `research/R2` §5.4's predicted
"96 core + 58 CL" is exactly right.** A `_0040` driver's baseline surface is **96 + 58 + 7 + 8 = 169
slots instead of 214**, and state objects, mesh shaders, enhanced barriers, work graphs and sampler
feedback leave the first milestone entirely.

And it is not merely a device: the **`triangle` workload ran ten frames with 0 failures at `_0040`**
(`F40-triangle.log`, `run-F40-triangle.txt`) — same DXIL shader encoding, same `cl[…] pfnPresent`.
⚠ In that log the slot *names* come from the `_0109`/`_0108` `.inc` lists and are therefore wrong for
a `_0040`-shaped table; only the counts and sizes are trustworthy there.

⛔ **Three traps in this arm, each of which produced a confident wrong answer first:**
1. Forcing the COUNT answer to 1 makes WARP's own FILL hit a 1-entry buffer for its 77-entry list and
   return `ERROR_INSUFFICIENT_BUFFER` (0x8007007A) — indistinguishable from "the runtime rejected the
   token".
2. Skipping the WARP call entirely crashes at `0xC0000005` shortly after:
   `pfnGetSupportedVersions` is where WARP initialises the state `pfnCalcPrivateDeviceSize` needs.
3. Running any arm with `HKLM\SOFTWARE\Helios!UmdD3D12Spy` **absent** makes the proxy's own gate
   return `DXGI_ERROR_UNSUPPORTED` for everything — four "tokens refused" results that were the spy
   refusing itself. **Verify the knob is 1 in the same command that runs the arm.**
   Only edit the answer on the way *out*, and only the FILL one.

## 6. The ordered first-frame call trace

From `C-window.log` (clear + present, no shaders). The second frame is byte-identical, so this is
also the steady-state frame:

```
cl[ 1] pfnResetCommandList
cl[29] pfnSetPipelineState          cl[24] pfnIaSetTopology       cl[30] pfnSetDescriptorHeaps
cl[46] pfnIASetVertexBuffers        cl[45] pfnIASetIndexBuffer    cl[47] pfnSOSetTargets
cl[48] pfnOMSetRenderTargets        cl[25] pfnRsSetViewports      cl[26] pfnRsSetScissorRects
cl[27] pfnOmSetBlendFactor          cl[70] pfnOmSetFrontAndBackStencilRef
cl[52] pfnOMSetDepthBounds          cl[65] pfnRSSetShadingRate    cl[23] pfnSetPredication
cl[50] pfnClearRootArguments                                     <-- the runtime's reset-state block
cl[68] pfnBarrier
cl[48] pfnOMSetRenderTargets
cl[ 7] pfnClearRenderTargetView
cl[68] pfnBarrier
cl[ 0] pfnCloseCommandList
queue[0] pfnExecuteCommandLists
core[81] pfnGetPresentPrivateDriverDataSize                       <-- once per present
cl[19] pfnPresent
```

Four things fall out of it:

1. **`pfnResetCommandList` is followed by a 15-call state-reset block.** Every one of those slots is
   called on *every* command-list reset whether or not the app touches that state. They are not
   optional for a first frame.
2. **`pfnBarrier` (`cl[68]`), not the legacy resource-barrier slot.** WARP reports
   `EnhancedBarriersSupported = 1`, and the runtime lowers `ResourceBarrier` to the *enhanced*
   entry point. A driver's barrier answer decides which slot the runtime calls.
3. ⭐ **`pfnGetPresentPrivateDriverDataSize` is called immediately before every `pfnPresent`.** That
   is the driver's opportunity to attach per-present private data. WARP returns 0, so
   `D3D12DDIARG_PRESENT_0001.PrivateDriverDataSize` arrived 0 and `pPrivateDriverData` NULL on all
   20 presents. A driver that returns N gets an N-byte buffer. ⚠ Whether that buffer reaches
   `DxgkDdiPresent` is **not** settled here — the D3D11 answer is *no on DMA flips* (memory 64th),
   which is why `PRESENT.md` rides the identity on the Render command. This is a second candidate
   channel to test at G8, not a replacement.
4. **The present argument, verbatim:** `surfaces=1  hDstResource=NULL  Flags=0x21  FlipInterval=0
   VidPnSourceID=0xffffffff  DirtyRects=0  PrivateDriverDataSize=0  OptimizeForComposition=1`,
   `pOut` = a **536-byte** `D3D12DDI_PRESENT_0051` whose first dword is a `D3DKMT_HANDLE`
   (`0x40000b00`, then `0x40000b80` on the next frame — the two swapchain buffers).

**Device open, ordered** (`A-device.log`, consecutive repeats collapsed):

```
adapter[4] pfnGetCaps ×2 -> adapter[3] pfnGetSupportedVersions ×2 -> adapter[0] pfnCalcPrivateDeviceSize
-> adapter[1] pfnCreateDevice -> adapter[4] pfnGetCaps ×11 -> adapter[5] pfnGetOptionalDDITables
-> adapter[6] pfnFillDDITable ×5 -> adapter[4] pfnGetCaps ×4
-> (core[0] pfnCheckFormatSupport + core[1] pfnCheckMultisampleQualityLevels ×30) ×91 formats
-> core[82] pfnQueryNodeMap -> adapter[4] pfnGetCaps ×7
-> the runtime's OWN internal pipeline: root signature, VS, blend, depth-stencil, rasterizer,
   PSO, pfnMakeResident; then a compute shader + PSO + pfnMakeResident
-> core[45] pfnGetDescriptorSizeInBytes -> command pool create/destroy -> teardown
```

⭐ **`D3D12CreateDevice` alone drives 27 of the 124 core slots**, including
`pfnCreate{Vertex,Compute}Shader`, `pfnCreatePipelineState`, `pfnCreateRootSignature` and
`pfnMakeResident` — the runtime builds its own internal pipelines before the app has an object.
`pfnCheckMultisampleQualityLevels` is called **2 730** times (91 formats × 30).

## 7. Slot coverage — how much of the DDI a triangle actually touches

From `D-triangle.log` (device + queue + swapchain + 2 PSOs + 2 draws + 3 presents):

| table | slots called | of |
|---|---:|---:|
| `DEVICE_CORE` | **47** | 124 |
| `COMMAND_LIST_3D` | **22** | 75 |
| `COMMAND_QUEUE_3D` | **1** | 7 |
| `DXGI` | **0** | 32 armed |
| **total** | **70** | 206 |

The single queue slot is `pfnExecuteCommandLists`. ⭐ **`pfnSignalFence` and `pfnWaitForFence` were
never called**, although the app called `ID3D12CommandQueue::Signal` + `SetEventOnCompletion` on
every one of 20 frames and `pfnCreateFence` *was* called three times. ⇒ **#12 answered as far as WARP
can answer it: the runtime, not the driver, performs the queue signal/wait.** ⚠ WARP is
software-scheduled; a hardware driver may be driven differently, so this is evidence, not proof.

`DDI_REFERENCE.md` §14.2's **99** real-body slots stands as the *design* target; **70** is the
measured floor for a triangle on WARP, and the two lists should be diffed before P4 sizes itself.

## 8. Q1 — what the runtime hands `pfnCreateShader`. Settled, and it is neither option

Every shader, in every run, at every negotiated version:

```
pfnCreateVertexShader:  dwords 00010060 0000010a 4c495844 00000100 00000010 00000410 dec04342 00000c21
pfnCreatePixelShader:   dwords 00000060 00000102 4c495844 …
pfnCreateComputeShader: dwords 00050060 000000d4 4c495844 …
```

* `dword[0]` = **`(programType << 16) | (major << 4) | minor`** — `0x0001_0060` is *vertex,
  SM 6.0*; `0x0000_0060` pixel; `0x0005_0060` compute. The type field matches the slot it arrived
  on, in every case.
* `dword[1]` = **length in dwords** (`0x10a` = 266 dwords = 1 064 bytes).
* `dword[2]` = **`'DXIL'` (0x4c495844)** — the DXIL part payload.

⇒ **The runtime does NOT pass a DXBC container.** `dword[0]` is never `'DXBC'` (0x43425844). It
strips the container and hands the driver a **raw stream behind the two-token D3D10-style header**,
exactly the second row of `DDI_REFERENCE.md` §12.2's table — which is also what the `_0003`
generation's `_In_reads_(pShaderCode[1])` SAL said all along.

⭐ **And the stronger half: the runtime converts DXBC to DXIL before the DDI.** The `triangle`
workload builds two pipelines in one process — one from `dxc -T vs_6_0` (3 152-byte container) and
one from `D3DCompile(…, "vs_5_1")` (596-byte container). The app's own blobs both start `'DXBC'`
(printed in `run-D-triangle.txt`). At the DDI, **both** arrive as `…0060` + length + `'DXIL'`, and
neither length matches the app's blob. The `window` workload, which has no app shaders at all,
produces only the two runtime-internal shaders — so the attribution is unambiguous.
⇒ **A D3D12 UMD on this build never sees DXBC.** (**#14 answered.**)

Consequence for `helios_umd12`: `umd/src/forward/shaders.rs:13-39`'s `shader_code_len()` ports over,
but only its **raw-stream branch** will ever execute; the DXBC-container branch is dead on the D3D12
DDI. Keep both — the bounds checks are the value — and count the container branch if it ever fires.

## 9. Q2 — does the runtime cross-validate the caps set as ONE contract? **Yes.**

Four mutation arms, all on the retail path with no debug layer:

| arm | mutation | `D3D12CreateDevice` | runtime's own reason (ETW `Microsoft-Windows-Direct3D12`) |
|---|---|---|---|
| `capfail` | `E_INVALIDARG` for `OPTIONS_0110` | ✅ `S_OK` | — |
| `range` | `ResourceBindingTier` = **99** | ✅ `S_OK` | — |
| `tier` 2 | `ResourceBindingTier` = **2** | ✅ `S_OK` | — |
| `tier` 1 | `ResourceBindingTier` = **1** | ❌ **`0x887A0020`** | `FL12+ driver incorrectly did not report support for resource binding tier 2+.` |
| `sm65` | shader-model list clamped to 6.5 | ❌ **`0x887A0020`** | `Drivers that expose AtomicInt64OnTypedResource, AtomicInt64OnGroupShared, AtomicInt64OnDescriptorHeapResource, DerivativesInMeshAndAmplificationShaders or WaveMMATier must expose shader model 6.6.` |
| `cross` | RaytracingTier 1.1 + shader models clamped to 6.0 | ❌ **`0x887A0020`** | same string |

**Three separate facts, and they are easy to conflate:**

1. **Cross-cap consistency IS enforced at retail**, at `D3D12CreateDevice`, failing with
   `DXGI_ERROR_DRIVER_INTERNAL_ERROR` and a plain-English reason on the
   `Microsoft-Windows-Direct3D12` provider. The rules are cap↔cap (`OPTIONS1_0103` vs the
   shader-model list) and cap↔feature-level (`3DPIPELINESUPPORT` vs `ResourceBindingTier`).
2. **Out-of-range tier values are CLAMPED, not rejected.** Tier 99 created a device and
   `CheckFeatureSupport(D3D12_OPTIONS)` then reported the app **3** — the maximum legal value. With
   the debug layer enabled the outcome was identical and no extra message appeared. So the fifteen
   `Driver filled out an invalid value in D3D12DDI_D3D12_OPTIONS_DATA::<Tier>` strings in
   `d3d12core-driverstrings.txt` are **not** retail device-creation gates.
   ⛔ Do not read that as permission to answer out of range: the clamp is silent, so a wrong tier
   becomes a wrong *advertised* tier rather than a loud failure.
3. **A legal answer propagates verbatim.** Tier 2 came back to the app as tier 2. The driver's caps
   answer *is* what the application sees.

**ETW recipe used** (the D3D11 recipe from the 30th session, retargeted):

```powershell
logman create trace helios_d12g5 -p Microsoft-Windows-Direct3D12 0xFFFFFFFFFFFFFFFF 0xff -o x.etl -ets
logman update helios_d12g5 -p Microsoft-Windows-DXGI 0xFFFFFFFFFFFFFFFF 0xff -ets
# run the arm, then:
logman stop helios_d12g5 -ets ; tracerpt x.etl -o x.xml -of XML -y
# read <Data Name="Message">
```

⚠ `Microsoft-Windows-DxgKrnl` / `AzureTriage` contributed **nothing** here — the failure is above
dxgkrnl. `Microsoft-Windows-Direct3D12` is the provider that answers for D3D12, exactly as
`Microsoft-Windows-DXGI` was for the D3D11 feature-level work.

## 10. 7.17 — does a WDDM 2.1 adapter constrain a D3D12 UMD?

Run through **Route B** on the real Helios adapter (`UserModeDriverName[3]` → the proxy,
`pnputil /restart-device`, restored afterwards, desktop verified).

* **Unmutated (`R-helios.log`):** WARP, opened on the Helios adapter, answered
  `pfnGetSupportedVersions` with **exactly one** entry — `0x000b0010_00010000`, a **D3D11-era**
  token, no D3D12 token at all. The runtime found no match and `D3D12CreateDevice` returned
  `DXGI_ERROR_UNSUPPORTED`. The same WARP binary offers 77 versions on its own adapter.
  ⇒ **WARP derives the DDI versions it will offer from the adapter it was opened on.**
* **Forced (`R-forcever.log`):** with the answer replaced by `_0110`, on the *same* WDDM 2.1 adapter,
  the runtime accepted it and went straight on to
  `pfnCalcPrivateDeviceSize(Interface=0x000c0050, Version=0x006e0000) -> 4016` and a full
  `pfnCreateDevice` with the 18-slot `D3D12DDI_CORELAYER_DEVICECALLBACKS_0062` and
  `NumReserveRanges = 0`. WARP then failed the create with `E_OUTOFMEMORY` — expected, it cannot
  run on a foreign adapter — **after** the negotiation had already succeeded.

⇒ **Answered, and in the good direction: the D3D12 runtime does not gate the DDI version on the
adapter's declared WDDM level.** `kmd_render`'s `Wddm2_1GpuMmu` is not, by itself, a barrier to
`helios_umd12.dll` negotiating `_0110`. What WARP does is WARP's own policy.

⚠ **What this does NOT establish:** the narrower original question — whether a WDDM 2.1 adapter caps
the *shader models* a D3D12 UMD may report. WARP never reached its caps sequence on the Helios
adapter, so no shader-model answer was ever offered there. The `sm65`/`cross` arms show the ceiling
is enforced against the **OPTIONS caps**, never against a WDDM version, in every message the runtime
emitted — but that is evidence from the WARP adapter. **Re-mark as: the WDDM-version-to-shader-model
coupling remains UNVERIFIED; the settling experiment is now G7, on `helios_umd12.dll`'s own caps.**

## 11. `DDI_REFERENCE.md` §15.1 — the eighteen, answered or re-marked

| # | Question | Verdict |
|---|---|---|
| 1 | Which caps types, in what order, and what a refusal does | ✅ **§2** — 23 of 43 asked, order recorded, a failing HRESULT is tolerated |
| 2 | Whether any DDI slot may legally be NULL | ◑ **Partial** — 4 named slots are NULL in a working driver (§4). Proving the other 202 must be non-NULL needs a null-one-at-a-time arm this gate did not run |
| 3 | `pfnFillDDITable`'s 5th `UINT` / `TABLE_REQUEST::numTables` | ◑ **5th UINT = the command-list table index** (0 and 1 observed). `numTables` still UNVERIFIED: `pfnGetOptionalDDITables` answered 0, so no extra table was ever requested |
| 4 | Which `DXGI*_DDI_BASE_FUNCTIONS` shape table type 3 wants | ✅ **Moot — type 3 is never requested**, including across 20 flip-model presents |
| 5 | The `Interface`/`Version` split of a `D3D12DDI_SUPPORTED_*` token | ✅ **high 32 / low 32**, matched bit-for-bit against WARP's own list |
| 6 | The DDI-version → Windows-release mapping | ◑ **This build negotiates `_0110`** and accepts down to `_0040` (§5). One build does not give the whole table |
| 7 | Where recording memory comes from — `pfnSubmitCommandCb` vs `pfnRenderCb` | ⛔ **UNVERIFIED, and the spy cannot settle it.** WARP is a software rasterizer: it called none of the `pKTCallbacks` kernel thunks in any run. Settling experiment unchanged — ETW `DxgKrnl` `DmaPacket`/`QueuePacket` around a *hardware* D3D12 driver, or G7/G8 on Helios |
| 8 | Is `pfnGetSupportedVersions` really a two-call query | ✅ **Yes** — count with `pVersions == NULL`, then fill |
| 9 | How a driver obtains a second `HRTTABLE` for `pfnSetCommandListDDITableCb` | ✅ **The runtime hands both out at device creation**: two `pfnFillDDITable(TableType=1)` calls, indices 0/1, `hRTTable` `0x3E0`/`0x638`. WARP then calls `pfnSetCommandListDDITableCb(hRTCommandList, 0x3E0)` on every command-list create — observed through the wrapped corelayer table |
| 10 | Whether the runtime accepts GPU VAs the driver never got from the kernel | ⛔ **UNVERIFIED** — needs a driver that fabricates a VA; the spy only forwards WARP's. Settling experiment unchanged (§9.7, debug-layer run) |
| 11 | Monitored fence advance for a D3D12-shaped fence | ⛔ **UNVERIFIED** — out of scope, still the G-fence probe |
| 12 | Whether the runtime, not the driver, performs the kernel signal/wait for `pfnSignalFence` | ◑ **Evidence for "the runtime does"**: `pfnSignalFence`/`pfnWaitForFence` never called across 20 frames of `Signal` + `SetEventOnCompletion`, while `pfnCreateFence` was. WARP is software-scheduled, so confirm on hardware |
| 13 | Whether the runtime cross-validates the caps set as ONE contract | ✅ **Yes** — §9, two worked examples with the runtime's own strings, retail path |
| 14 | Whether the runtime ever passes a raw DXIL bitstream instead of a DXBC container | ✅ **It ALWAYS does**, and it converts SM 5.1 DXBC to DXIL first — §8 |
| 15 | The contract of `D3D12DDICAPS_TYPE_EXECUTECOMMANDLISTS_PARALLELISM` | ◑ **Arm 1 run: the runtime never asks for 1069** on this build, in any of the four workloads. So the cap cannot be read off WARP's answer. Arm 2 (force TRUE and diff a `QueuePacket` slice) is untouched and now the only route |
| 16 | Whether the runtime honours a `NOT_SUPPORTED` tier by never calling the corresponding slot | ◑ **Weak support**: WARP reports `RenderPassTier = 0` and no render-pass slot was ever called. But the workloads never *asked* for a render pass, so this does not separate "the runtime suppressed it" from "the app never wanted it". Needs an app that uses the feature against a driver that declines it |
| 17 | The oldest `D3D12DDI_SUPPORTED_*` this Windows build accepts | ✅ **`_0040` is accepted, and a triangle presents on it** — §5. 96 core + 58 CL slots |
| 18 | Whether `pfnFillDDITable`'s `SIZE_T` matches `size_of` of the bindgen'd struct | ✅ **At `_0110`/`_0109` yes, exactly** (992/600/56). ⛔ And it is version-dependent: `_0089` → 976/552, `_0040` → 768/464. Honour the argument |

**Score: 8 answered outright, 6 partially, 4 re-marked UNVERIFIED with a stated reason.**
`DDI_REFERENCE.md` §15.1 predicted "the spy settles 1–9, 13, 14, 16 and 18 — thirteen of eighteen".
Measured: it settled **1, 4, 5, 8, 9, 13, 14, 17, 18** outright and moved **2, 3, 6, 12, 15, 16**
forward; **7, 10, 11** it cannot touch, and #7's reason is structural — WARP never enters the kernel.

## 12. Artifacts

| file | what |
|---|---|
| `spy.log` | = `D-triangle.log`, the superset run |
| `A-device.log` `B-queue.log` `C-window.log` `D-triangle.log` | the four workloads |
| `M-{capfail,range,range2,rangedbg,cross,sm65,tier1,tier2}.log` | the caps mutation arms |
| `F-000c00{28000000,5000090000,50006d0000,50006e0000}.log` `F40-triangle.log` | the version-floor arms |
| `R-helios.log` `R-forcever.log` | Route B, on the real Helios adapter |
| `run-*.txt` | each workload's stdout |
| `warp-identity.txt` | the WARP build and hash the whole capture is against |
| `umd-name-backup.txt` | the pre-experiment `UserModeDriverName`, restored |

Source: `tools/d3d12_spy/` (`gen_slots.py` regenerates every `.inc` and `spy_thunks.asm` from
`tmp/dx12/sdk/d3d12umddi.h`; `build.ps1` asserts the 124/75/7/8/43/25 counts before compiling).
