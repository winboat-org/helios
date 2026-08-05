# R5 — `kmd_render` gap analysis for D3D12

**Lane:** R5. **Question:** is the working hypothesis "we probably won't have to make large changes
to the KMD" true? **Answer: yes — and more strongly than the hypothesis states, for the
vkd3d-over-Venus strategy, where the KMD work list is empty. For a native `d3d12umddi` UMD the KMD
work list is short (3 small items) and there is exactly one load-bearing exception, which is in
`DxgkDdiCreateAllocation`, not in scheduling or memory management.** Evidence below.

Everything here was re-derived from source and headers this session. Where DX12.md §3 is right, it
says so; where it is wrong or incomplete, §11 lists the corrections.

**Reference copies pulled to `tmp/dx12/sdk/` this session (read-only, from the VM's WDK
10.0.26100.0):** `dispmprt.h` (`km/`, 462 KB — contains `DRIVER_INITIALIZATION_DATA`),
`d3dkmddi.h` (`shared/`), `d3dumddi.h` (`um/`). The pre-existing staged headers (`d3d12umddi.h`,
`d3d12.h`, `d3dkmthk.h`, `d3dkmdt.h`, `d3dukmdt.h`) were already there.

---

## 0. The three facts that reframe the whole question

### 0.1 The rendering path does not go through the WDDM scheduler at all

The Mesa Venus ICD submits every Vulkan command stream through **`D3DKMTEscape`**, not through
`D3DKMTRender`/`D3DKMTSubmitCommand`:

- `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c:103` — `#define HELIOS_ESCAPE_SUBMIT_VENUS 0x0001u`
- `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c:1596` — `helios_hdr_init(&hdr.hdr, HELIOS_ESCAPE_SUBMIT_VENUS, sizeof(hdr));`
- `:1257` / `:1891` — `const NTSTATUS st = D3DKMTEscape(&esc);`
- `:1607` — *"Over D3DKMTEscape the venus stream rides INSIDE the escape buffer, directly…"*

And the KMD's submission DDIs do **no** venus forwarding. `dxgkddi_submit_command`
(`kmd_render/src/ddi/submit_command.rs:766-799`) and `dxgkddi_submit_command_virtual` (`:725-760`)
only (a) count the fence, (b) `arm_dma_flip(...)` from the DMA private data, and (c) call
`note_and_maybe_signal(...)`. There is no call to `submit_venus` anywhere in that file (`grep -n
"submit_venus" kmd_render/src/ddi/submit_command.rs` → no hits).

**Consequence for D3D12:** under strategy (b) (vkd3d-proton on top of the Helios Vulkan ICD), a
D3D12 app's GPU work reaches the host by exactly the same bytes and the same KMD code path that a
Vulkan or a DXVK/D3D11 app already uses today. The WDDM scheduler surface — nodes, engines,
hardware queues, monitored fences, GPU VA — is not on the D3D12 rendering path at all. It is only
on the *present* path, and the present path is already the one DWM exercises every frame.

### 0.2 The adapter declares WDDM 2.1, so 72 of the 103 "unset" DDI slots are unreachable by construction

`kmd_render/src/ddi/wddm_surface.rs:64` — `pub(crate) const SURFACE: WddmSurface =
WddmSurface::Wddm2_1GpuMmu;` — drives `DRIVER_INITIALIZATION_DATA.Version` at
`kmd_render/src/lib.rs:103` to `DXGKDDI_INTERFACE_VERSION_WDDM2_1` (= 24579,
`tmp/dxgk_bindings.rs:200`).

In `dispmprt.h` the `DRIVER_INITIALIZATION_DATA` body (`tmp/dx12/sdk/dispmprt.h:2690-3043`) is one
flat struct whose members sit inside nested `#if (DXGKDDI_INTERFACE_VERSION >= …)` blocks. The
WDDM 2.1 block closes at `dispmprt.h:2900`; everything from `dispmprt.h:2902`
(`#if … >= DXGKDDI_INTERFACE_VERSION_WDDM2_2`) onward is a higher-version member.

Bucketing all 187 `DxgkDdi*` members by the version block they live in (script re-run this session
over `dispmprt.h:2690-3043`):

| Block | members | of which SET by Helios |
|---|---|---|
| BASE | 61 | 50 |
| WIN7 | 8 | 2 |
| WIN8 | 12 | 9 |
| WDDM1_3 | 6 | 3 |
| WDDM2_0 | 13 | 10 |
| WDDM2_1 | 6 | 1 |
| **≤ WDDM2_1 subtotal** | **106** | **75** |
| WDDM2_2 | 19 | 7 |
| WDDM2_3 | 3 | 0 |
| WDDM2_4 | 12 | 1 |
| WDDM2_5 | 5 | 1 |
| WDDM2_6 | 3 | 0 |
| WDDM2_7 | 1 | 0 |
| WDDM2_9 | 4 | 0 |
| WDDM3_0 | 4 | 0 |
| WDDM3_1 | 9 | 0 |
| WDDM3_2 | 21 | 0 |
| **> WDDM2_1 subtotal** | **81** | **9** |

So: **only 31 of the 103 unset slots are inside the surface the driver actually declares.** The
other 72 are unset *and unreachable*.

Conversely, **9 of the 84 set slots are above the declared version and are almost certainly dead
code**:

| Set slot | block | `dispmprt.h` |
|---|---|---|
| `DxgkDdiCreateHwContext` | WDDM2_2 | 2908 |
| `DxgkDdiDestroyHwContext` | WDDM2_2 | 2909 |
| `DxgkDdiCreateHwQueue` | WDDM2_2 | 2911 |
| `DxgkDdiDestroyHwQueue` | WDDM2_2 | 2912 |
| `DxgkDdiSubmitCommandToHwQueue` | WDDM2_2 | 2914 |
| `DxgkDdiSwitchToHwContextList` | WDDM2_2 | 2915 |
| `DxgkDdiExchangePreStartInfo` | WDDM2_2 | 2932 |
| `DxgkDdiSetVirtualMachineData` | WDDM2_4 | 2956 |
| `DxgkDdiPresentToHwQueue` | WDDM2_5 | 2968 |

**Live corroboration (this session, `win_exec` read of
`HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`):**

```
HwQRef  = <ABSENT>
PHQcall = <ABSENT>
PHQours = <ABSENT>
PHQst   = <ABSENT>
FlipCapV = 2      FlipQueV = 1     CapTrunc = 0     ScStale = 0     StRing = 0
```

`HwQRef` is written unconditionally on the *first statement path* of
`dxgkddi_create_hw_queue` (`kmd_render/src/ddi/scheduler.rs:180-187`:
`crate::diag::record_named_bytes(b"HwQRef", 1);`). Its absence means `DxgkDdiCreateHwQueue` has
**never** been called in this service key's lifetime — consistent with the version gate, and
independent evidence for it.

⚠ **UNVERIFIED (mechanism):** that dxgkrnl truncates the table at the declared `Version` is an
*inference* from (a) the header's version-conditional layout, (b) `HwQRef`/`PHQcall` absence.
Microsoft's `DriverEntry` doc
(`windows-driver-docs-research-only/.../display/driverentry-of-display-miniport-driver.md:44`) only
says to set `Version` to `DXGKDDI_INTERFACE_VERSION`; it does not state the truncation rule.
**Settling experiment:** in a scratch build set `DxgkDdiCreateHwQueue` to a body that writes a
`HwQEnt` counter *before* any check, deploy, run a D3D12/DXVK workload, and read the key — or, with
`ntoseye`, disassemble `dxgkrnl!DpiInitializeEx` and read the version→size table. Cheap variant:
raise `SURFACE` to `Wddm3_2GpuMmu` on a throwaway boot and see whether `HwQRef` appears (⚠ 3.2
breaks DWM at `E_NOTIMPL`, `wddm_surface.rs:25-28` — do this only as a diagnostic).

### 0.3 `DxgkDdiCreateAllocation` refuses any allocation without a Helios private-data blob

`kmd_render/src/ddi/create_allocation.rs:2291-2307`:

```rust
    if priv_ptr.is_null() || priv_len < size_of::<HeliosWddmAllocPrivate>() {
        crate::diag::record(0x0C01_0002);
        return Err(STATUS_INVALID_PARAMETER);
    }
    …
    if !ap.is_valid() {
        crate::diag::record(0x0C01_0003);
        return Err(STATUS_INVALID_PARAMETER);
    }
```

`HeliosWddmAllocPrivate::is_valid` is `self.magic == HELIOS_WDDM_MAGIC && self.version ==
HELIOS_WDDM_VERSION` (`protocol/src/wddm.rs:149-151`); the struct is 48 bytes
(`protocol/src/wddm.rs:102-120`).

**This is the one load-bearing exception in the whole lane.** A native `d3d12umddi` UMD that calls
`pfnAllocateCb` with its own private data — which is what a from-scratch D3D12 UMD does for every
heap, committed resource and query heap — gets `STATUS_INVALID_PARAMETER` on the first allocation.
It is not a KMD *defect*: it is the KMD correctly refusing an allocation it cannot back. But it
means "the D3D12 UMD reuses the D3D11 UMD's KMD" is only true if the D3D12 UMD also authors
`HeliosWddmAllocPrivate` (and, for standard/GDI shapes, the `HeliosWddmAllocMeta` trailer). For
strategy (b) it is a non-issue — the ICD already authors it.

---

## 1. The DDI table today

### 1.1 Counts (re-derived, not taken from DX12.md)

- `_DRIVER_INITIALIZATION_DATA` has **193** fields, of which **187** are `DxgkDdi*` slots and 6 are
  not (`Version`, `Reserved`, `Reserved1`, `Reserved2`, `Reserved3`, `Reserved4`).
  Source: `tmp/dxgk_bindings.rs:95197-95395`; `size_of == 1544`, `align_of == 8`
  (`tmp/dxgk_bindings.rs:95395-95399`). Same 187 count independently parsed out of
  `tmp/dx12/sdk/dispmprt.h:2690-3043`.
- `kmd_render/src/lib.rs:106-222` sets **85** fields, of which **84** are `DxgkDdi*` slots
  (the 85th is `Version` at `:103`). No duplicates.
- **103 `DxgkDdi*` slots are unset.** DX12.md §3's "84 of 187, 193 fields total" is **correct**.

### 1.2 The 31 unset-and-reachable slots, classified for D3D12

These are the only unset slots dxgkrnl could call at the declared WDDM 2.1 level. Classification:
**(a)** irrelevant to D3D12, **(b)** optional D3D12 feature (named), **(c)** required for baseline
D3D12.

| Slot | block | `dispmprt.h` | Class | Why |
|---|---|---|---|---|
| `DxgkDdiSetPalette` | BASE | 2721 | (a) | 8-bpp palettized VGA legacy. |
| `DxgkDdiAcquireSwizzlingRange` | BASE | 2715 | (a) | Pre-WDDM2 CPU swizzle apertures; superseded by `MapCpuHostAperture`. |
| `DxgkDdiReleaseSwizzlingRange` | BASE | 2716 | (a) | as above. |
| `DxgkDdiRecommendVidPnTopology` | BASE | 2737 | (a) | Display topology only. |
| `DxgkDdiStopCapture` | BASE | 2739 | (a) | Video capture. |
| `DxgkDdiCreateOverlay` | BASE | 2741 | (a) | Legacy hardware overlays (pre-MPO). |
| `DxgkDdiUpdateOverlay` | BASE | 2757 | (a) | as above. |
| `DxgkDdiFlipOverlay` | BASE | 2758 | (a) | as above. |
| `DxgkDdiDestroyOverlay` | BASE | 2759 | (a) | as above. |
| `DxgkDdiLinkDevice` | BASE | 2772 | (a) | LDA (linked display adapters). D3D12 multi-adapter node masks would need it; Helios is `NodeMask=1`. |
| `DxgkDdiSetDisplayPrivateDriverFormat` | BASE | 2773 | (a) | Display private format. |
| `DxgkDdiDescribePageTable` | WIN7 | 2779 | (a) | `PVOID`-typed reserved slot; and CPU_VIRTUAL-mode only. |
| `DxgkDdiUpdatePageTable` | WIN7 | 2780 | (a) | `PVOID`; CPU_VIRTUAL-mode only. See §6. |
| `DxgkDdiUpdatePageDirectory` | WIN7 | 2781 | (a) | `PVOID`; CPU_VIRTUAL-mode only. |
| `DxgkDdiMovePageDirectory` | WIN7 | 2782 | (a) | `PVOID`; CPU_VIRTUAL-mode only. |
| `DxgkDdiSubmitRender` | WIN7 | 2784 | (a) | `PVOID`-typed reserved slot — no public prototype in this WDK. |
| `DxgkDdiCreateAllocation2` | WIN7 | 2785 | (a) | `PVOID`-typed reserved slot — no public prototype. |
| `DxgkDdiSetPowerComponentFState` | WIN8 | 2801 | (a) | Runtime power management (F-states). |
| `DxgkDdiSetVidPnSourceAddressWithMultiPlaneOverlay` | WIN8 | 2833 | (b) | MPO. D3D12 flip-model swapchains work without MPO (DWM composites). |
| `DxgkDdiNotifySurpriseRemoval` | WIN8 | 2838 | (a) | PnP surprise removal — a *stability* item, not a D3D12 one. |
| `DxgkDdiSetPowerPState` | WDDM1_3 | 2851 | (a) | P-states. |
| `DxgkDdiControlInterrupt2` | WDDM1_3 | 2852 | (a) | `DxgkDdiControlInterrupt` is set (`lib.rs:220`). |
| `DxgkDdiCheckMultiPlaneOverlaySupport` | WDDM1_3 | 2857 | (b) | MPO. |
| `DxgkDdiCheckMultiPlaneOverlaySupport2` | WDDM2_0 | 2879 | (b) | MPO. |
| `DxgkDdiSetVidPnSourceAddressWithMultiPlaneOverlay2` | WDDM2_0 | 2882 | (b) | MPO. |
| `DxgkDdiSetVideoProtectedRegion` | WDDM2_0 | 2887 | (b) | Hardware content protection / `ID3D12ProtectedResourceSession`. |
| `DxgkDdiCheckMultiPlaneOverlaySupport3` | WDDM2_1 | 2893 | (b) | MPO. |
| `DxgkDdiSetVidPnSourceAddressWithMultiPlaneOverlay3` | WDDM2_1 | 2894 | (b) | MPO. |
| `DxgkDdiPostMultiPlaneOverlayPresent` | WDDM2_1 | 2895 | (b) | MPO. |
| `DxgkDdiValidateUpdateAllocationProperty` | WDDM2_1 | 2896 | (b) | `D3DKMTUpdateAllocationProperty` — used for changing an allocation's segment/priority. Not on the baseline path. |
| `DxgkDdiControlModeBehavior` | WDDM2_1 | 2898 | (a) | Display mode behaviour. |

**No slot in this table is class (c).** *Nothing* that is unset and reachable is required for a
baseline D3D12 device.

### 1.3 The 72 unset-and-unreachable slots

Grouped by the block they live in (all > WDDM2_1, so out of the declared surface): WDDM2_2 ×12,
WDDM2_3 ×3, WDDM2_4 ×11, WDDM2_5 ×4, WDDM2_6 ×3, WDDM2_7 ×1, WDDM2_9 ×4, WDDM3_0 ×4, WDDM3_1 ×9,
WDDM3_2 ×21. The D3D12-adjacent families in there, all of which are **optional features gated on
capabilities Helios does not advertise**:

- **Native GPU fences (WDDM 3.1/3.2):** `CreateNativeFence`, `DestroyNativeFence`, `OpenNativeFence`,
  `CloseNativeFence`, `SetNativeFenceLogBuffer`, `UpdateNativeFenceLogs`, `UpdateMonitoredValues`,
  `UpdateCurrentValuesFromCpu`. Gated on `DXGK_VIDSCHCAPS.NativeGpuFence` (bit 11,
  `tmp/dx12/sdk/d3dkmddi.h:2020`), which Helios leaves 0. See §5.
- **User-mode submission / doorbells (WDDM 3.1):** `CreateDoorbell`, `ConnectDoorbell`,
  `DisconnectDoorbell`, `DestroyDoorbell`, `NotifyWorkSubmission`.
- **HWS context scheduling (WDDM 2.4):** `SetContextSchedulingProperties`, `SuspendContext`,
  `ResumeContext`, `SetupPriorityBands`, `SetSchedulingLogBuffer`, `NotifyFocusPresent`;
  plus `NotifyContextPriorityChange` (WDDM 3.2).
- **Protected sessions (WDDM 2.3):** `CreateProtectedSession`, `DestroyProtectedSession`.
- **Flip queue / Display-Core (WDDM 2.9/3.0):** `SetFlipQueueLogBuffer`, `UpdateFlipQueueLog`,
  `CancelQueuedFlips`, `CancelFlips`, `SetInterruptTargetPresentId`, `CreateCpuEvent`,
  `DestroyCpuEvent`, `SetAllocationBackingStore`.
- **GPU-PV / live migration (WDDM 3.2):** `CreateMemoryBasis`, `DestroyMemoryBasis`,
  `StartDirtyTracking`, `StopDirtyTracking`, `QueryDirtyBitData`, `PrepareLiveMigration`,
  `Save*/Restore*MigrationData`, `EndLiveMigration`, `WriteVirtualizedInterrupt`,
  `SetVirtualGpuResources2`, `SetVirtualFunctionPauseState`.
- **Misc:** `ResetHwEngine`, `ResumeHwEngine`, `UpdateHwContextState`, `ValidateSubmitCommand`,
  `SignalMonitoredFence`, `SaveMemoryForHotUpdate`, `RestoreMemoryForHotUpdate`,
  `CollectDiagnosticInfo`, `CollectDbgInfo2`, `ControlInterrupt3`, `GetMultiPlaneOverlayCaps`,
  `GetPostCompositionCaps`, `ResetDisplayEngine`, `BeginExclusiveAccess`, `EndExclusiveAccess`,
  `QueryDiagnosticTypesSupport`, `ControlDiagnosticReporting`, `CreatePeriodicFrameNotification`,
  `DestroyPeriodicFrameNotification`, `SetTimingsFromVidPn`, `SetTargetGamma`,
  `SetTargetContentType`, `SetTargetAnalogCopyProtection`, `SetTargetAdjustedColorimetry`(2),
  `SetTrackedWorkloadPowerLevel`, `DisplayDetectControl`, `QueryConnectionChange`.

---

## 2. Adapter caps: everything `query_adapter_info.rs` reports, and its adequacy for D3D12

`dxgkddi_query_adapter_info` answers **15** `DXGKQAITYPE_*` values and returns
`STATUS_NOT_SUPPORTED` (with a `0x0200_xxxx` diag record) for everything else
(`kmd_render/src/ddi/query_adapter_info.rs:51-93`):

`DRIVERCAPS`(1), `QUERYSEGMENT`(2), `QUERYSEGMENT3`(5), `QUERYSEGMENT4`(11), `GPUMMUCAPS`(13),
`PAGETABLELEVELDESC`(14), `WDDMDEVICECAPS`(29), `PHYSICAL_MEMORY_CAPS`(34), `IOMMU_CAPS`(35),
`HARDWARERESERVEDRANGES2`(36), `GPUVERSION`(27), `ADAPTERPERFDATA_CAPS`(26),
`DIRTYBITTRACKINGCAPS`(39), `HISTORYBUFFERPRECISION`(10), `64BITONLYCAPS`(47). (Enum values from
`tmp/dx12/sdk/d3dkmddi.h:1800-1871`.)

### 2.1 `DXGK_DRIVERCAPS` — field by field

Written through a bounds-checked `VersionedOut` (`query_adapter_info.rs:122-164`) so a short buffer
truncates rather than overflows; the count of skipped fields is `CapTrunc`
(`:492`) — **live value 0** this session.

| Field | Value | Site | D3D12 adequacy |
|---|---|---|---|
| `HighestAcceptableAddress` | `-1` (64-bit) | `:216` | Fine. |
| `MaxAllocationListSlotId` | `0xFFFF` | `:217-221` | Fine. Only matters for patching contexts; see §2.5. |
| `ApertureSegmentCommitLimit` | `64 MiB` (`APERTURE_COMMIT_LIMIT`, `:598`) | `:222-226` | ⚠ Per `calculating-graphics-memory.md` this is a *global* cap that reduces the OS-computed `SharedSystemMemory`; the doc advises against reducing it and viogpu3d never sets it (`:592-597`). D3D12 residency budgets read the aggregated segment sizes; a 64 MiB shared cap is small. **Not a blocker; a tuning item.** |
| `SupportNonVGA` | 1 | `:228-229` | Fine. |
| `WDDMVersion` | `DXGKDDI_WDDMv2_1` | `:234-235` | **Above the D3D12 floor.** WDDM 2.0 is the version that introduced D3D12 (`windows-vista-display-driver-model-design-guide.md:35`: *"WDDM 2.0 \| Windows 10 (1507) \| GPU virtual addressing, driver residency model, Direct3D 12"*). |
| `PreemptionCaps.GraphicsPreemptionGranularity` | `D3DKMDT_GRAPHICS_PREEMPTION_DMA_BUFFER_BOUNDARY` | `:236-243` | Fine — see §9. |
| `PreemptionCaps.ComputePreemptionGranularity` | `D3DKMDT_COMPUTE_PREEMPTION_DMA_BUFFER_BOUNDARY` | `:244-251` | Fine — see §9. |
| `SupportPerEngineTDR` | 1 | `:252-253` | Fine (one engine). |
| `PresentationCaps` | `0` | `:345, 396` | `SupportKernelModeCommandBuffer` deliberately 0 — no GDI HW accel (`:279-296`). Irrelevant to D3D12. |
| `FlipCaps` | `FlipOnVSyncMmIo` only (bit 1) — **live `FlipCapV = 2`** | `:389-397` | Present-path only. `FlipImmediateMmIo` is deliberately NOT set (`:377-388`, defect 0ab). See §7 of the risks. |
| `SchedulingCaps` (`DXGK_VIDSCHCAPS`) | `MultiEngineAware\|PreemptionAware` = bits 0,2 | `:304-305, 395, 402` | Bit positions confirmed at `tmp/dx12/sdk/d3dkmddi.h:1994-2024`. `No64BitAtomics`(bit5)=0, `NativeGpuFence`(bit11)=0, `HwQueuePacketCap`(bits7-10)=0. All correct for a software-scheduled, dxgkrnl-writes-the-fence adapter. |
| `MemoryManagementCaps` (`DXGK_VIDMMCAPS`) | `SectionBackedPrimary` (bit3) + `VirtualAddressingSupported` (bit5) + `GpuMmuSupported` (bit6); `CrossAdapterResource` (bit4) **only if the `CrossAdaptCaps` knob is set** | `:325-341, 403` | Bit positions confirmed at `d3dkmddi.h:2255-2290`. **`CrossAdaptCaps` is ABSENT from the live service key → default 0 → cross-adapter is OFF today** (`kmd_render/src/adapter/mod.rs:247`). See §8. |
| `MaxQueuedFlipOnVSync` | 1 (knob `FlipQueueDepth`, clamp 1..16) — **live `FlipQueV = 1`** | `:426-435` | Present-path only. |
| `SupportDirectFlip` | 0 (knob `DirectFlipCaps`, default 0) | `:454-455` | Deliberate; `SupportDirectFlip=1` was an unbacked lie that stopped DWM compositing (`:439-453`). |
| `GpuEngineTopology.NbAsymetricProcessingNodes` | **1** | `:456-464` | See §3. |
| everything else | zero-filled at `:206` | | `SupportMultiPlaneOverlay`, `SupportSurpriseRemoval`, `HybridDiscrete`, … all 0. |

`REQUIRED_DRIVER_CAPS_SIZE` = `offset_of!(SupportDirectFlip) + 1` = **540**; buffers shorter than
that get `STATUS_BUFFER_TOO_SMALL` (`:193-200`). `size_of::<DXGK_DRIVERCAPS>()` on the bindgen'd
26100 headers is 592; the driver deliberately accepts short (versioned) buffers.

### 2.2 `DXGK_WDDMDEVICECAPS`

`query_wddm_device_caps` (`:507-517`) zero-fills and sets only `WDDMVersion = SURFACE.driver_caps_version()`.
Every other device cap is 0. Adequate.

### 2.3 `DXGK_GPUMMUCAPS` (`gpummu::fill_gpummu_caps`, `kmd_render/src/ddi/gpummu.rs:127-148`)

- Flags word = **0** — no `ReadOnlyMemorySupported`, `NoExecuteMemorySupported`,
  `CacheCoherentMemorySupported`, `LargePageSupported`, `DualPteSupported`, … (`:135-136`,
  and the module rule at `:122-126`: "unknown stays unadvertised").
- `PageTableUpdateMode = DXGK_PAGETABLEUPDATE_GPU_PHYSICAL` (`:141`). `CPU_VIRTUAL` was
  KD-proven fatal inside `VIDMM_PAGE_TABLE_BASE::GetCpuVisibleAddress` (`gpummu.rs:27-35`).
- `VirtualAddressBitCount = 40` (`:142`, const at `:44`).
- `LeafPageTableSizeFor64KPagesInBytes = 4096` (`:144`), `PageTableLevelCount = 4` (`:145`).

**D3D12 adequacy:** `VirtualAddressBitCount` reaches user mode through
`D3DKMT_QUERY_GPUMMU_CAPS` / `D3DKMT_GPUMMU_CAPS.VirtualAddressBitCount`
(`tmp/dx12/sdk/d3dkmthk.h:2167-2187`). The corresponding D3D12 app-visible cap is
`D3D12_FEATURE_DATA_GPU_VIRTUAL_ADDRESS_SUPPORT { MaxGPUVirtualAddressBitsPerResource;
MaxGPUVirtualAddressBitsPerProcess; }` (`tmp/dx12/sdk/d3d12.h:2637-2641`). The *per-resource* half
is a **UMD** cap, `D3D12DDI_GPUVA_CAPS_0004 { UINT MaxGPUVirtualAddressBitsPerResource; }` under
`D3D12DDICAPS_TYPE_GPUVA_CAPS` (`tmp/dx12/sdk/d3d12umddi.h:250-257`).
⚠ **UNVERIFIED:** that `MaxGPUVirtualAddressBitsPerProcess` is derived from the KMD's
`VirtualAddressBitCount`. **Settling read:** trace a `D3D12CreateDevice` +
`CheckFeatureSupport(D3D12_FEATURE_GPU_VIRTUAL_ADDRESS_SUPPORT)` on a real driver and compare
against `D3DKMTQueryAdapterInfo(KMTQAITYPE_GPUMMU_CAPS)` on the same adapter — or just do it on
Helios once a D3D12 device exists and check that 40 comes out.

### 2.4 `DXGK_PAGE_TABLE_LEVEL_DESC` (`gpummu::fill_page_table_level_desc`, `gpummu.rs:160-195`)

Levels 0..3; index bits 9/9/9/1 (`:185-189`); `PageTableSegmentId = PagingProcessPageTableSegmentId
= 0` (system memory, `:190-191`); size and alignment 4096 (`:192-193`). Internally consistent by
`const _: () = assert!` at `gpummu.rs:103-120`. Adequate.

### 2.5 Engine topology, contexts, DMA buffers

- **One node.** `dxgkddi_get_node_metadata` (`query_adapter_info.rs:1261-1287`) rejects
  `node_ordinal != 0` with `STATUS_INVALID_PARAMETER` (`:1269`), and reports
  `EngineType = DXGK_ENGINE_TYPE_3D` (`:1278`), `GpuMmuSupported = 1` (`:1283`),
  `IoMmuSupported = 0` (`:1284`). This matches the MS reference implementation's contract
  ("Node ordinal is out of bounds. Required to return STATUS_INVALID_PARAMETER",
  `windows-driver-docs-.../display/enumerating-gpu-nodes.md`).
- `NbAsymetricProcessingNodes = 1` (`:456-464`).
- `DxgkDdiQueryDependentEngineGroup` / `QueryEngineStatus` / `ResetEngine` accept only
  `(NodeOrdinal, EngineOrdinal) == (0, 0)` (`kmd_render/src/ddi/scheduler.rs:64-66, 81-83, 104-106`).
  `DxgkDdiSwitchToHwContextList` likewise (`:220-222`).
- **`DxgkDdiCreateContext`** (`kmd_render/src/device.rs:389-431`) sets
  `DmaBufferSegmentSet = 1` (the aperture, `:420`), `DmaBufferSize = 256 KiB` (`:421`),
  `DmaBufferPrivateDataSize = PRESENT_DMA_PRIVATE_DATA_BYTES` (`:425-426`),
  `AllocationListSize = PatchLocationListSize = DXGK_ALLOCATION_LIST_SIZE_GDICONTEXT` = **256**
  (`:427-428`; constant at `tmp/dx12/sdk/d3dkmddi.h:1546`).
  It **ignores `NodeOrdinal`, `EngineAffinity` and `Flags` entirely**, and never writes
  `ContextInfo.Caps` or `ContextInfo.PagingCompanionNodeId`
  (both exist — `d3dkmddi.h:1567-1580`).
- **Paging buffer size:** `PagingBufferSize = 64 KiB` on `QUERYSEGMENT4`
  (`PAGING_BUFFER_BYTES_V4`, `query_adapter_info.rs:660`), `40 KiB` on the two legacy surfaces
  (`PAGING_BUFFER_BYTES_LEGACY`, `:670`). `PagingBufferPrivateDataSize = 0` everywhere.

**D3D12 adequacy:** adequate, with two *quality* gaps (not blockers) recorded as work items W2/W3 in
§10:
1. `DXGK_CREATECONTEXTFLAGS.VirtualAddressing` (bit 2, `d3dkmddi.h:1521`) marks a GPU-VA context,
   for which the OS documents `DXGK_CONTEXTINFO_NO_PATCHING_REQUIRED` — in the header,
   `DXGK_CONTEXTINFO_CAPS.NoPatchingRequired` (`d3dkmddi.h:1550-1563`) — as the right answer, with
   "no patch location list … and only a very small allocation list (16 entries)"
   (`windows-driver-docs-.../display/residency-overview.md`). Helios asks for 256 + 256 on every
   context and no-ops `DxgkDdiPatch` (`submit_command.rs:1421-1430`). Wasteful, not wrong.
2. `NodeOrdinal` is not validated in `CreateContext` even though it *is* in `CreateHwContext`
   (`scheduler.rs:135-137`). A D3D12 UMD that asks for node 1 would get a silently-wrong context
   instead of a counted refusal. ⚠ **UNVERIFIED** whether dxgkrnl filters `NodeOrdinal >
   NbAsymetricProcessingNodes-1` before the DDI. **Settling experiment:** a UMD-side
   `D3DKMTCreateContextVirtual` with `NodeOrdinal = 1` (`tmp/dx12/sdk/d3dumddi.h:3976-3984`) from
   `tools/`, and read the returned NTSTATUS.

---

## 3. Engines / nodes: what happens when D3D12 creates a COPY or COMPUTE queue

**Settled, from the headers — no KMD change needed.**

The D3D12 UMD DDI's command-queue creation argument is:

```c
typedef struct D3D12DDIARG_CREATECOMMANDQUEUE_0001
{
    D3D12DDI_HCOMMANDQUEUE       hDrvCommandQueue;
    D3D12DDI_HRTCOMMANDQUEUE     hRTCommandQueue;
    D3D12DDI_COMMAND_QUEUE_FLAGS QueueFlags;
    UINT                         NodeMask;
} D3D12DDIARG_CREATECOMMANDQUEUE_0001;
```
— `tmp/dx12/sdk/d3d12umddi.h:1450-1456`. `QueueFlags` is
`D3D12DDI_COMMAND_QUEUE_FLAG_{NONE,3D,COMPUTE,COPY,PAGING,…}` (`:1435-1448`); `NodeMask` is the **LDA
adapter node mask**, not a WDDM engine ordinal. **There is no `NodeOrdinal` and no
`EngineAffinity` anywhere in the D3D12 queue-creation DDI.**

The UMD then creates its own kernel context through the core-layer callback
`pfnCreateContextVirtualCb` (`d3d12umddi.h:2562-2564, 2633`), whose argument
`D3DDDICB_CREATECONTEXTVIRTUAL` is:

```c
typedef struct _D3DDDICB_CREATECONTEXTVIRTUAL
{
    UINT                        NodeOrdinal;      // in:
    UINT                        EngineAffinity;   // in:
    D3DDDI_CREATECONTEXTFLAGS   Flags;            // in:
    VOID*                       pPrivateDriverData;
    UINT                        PrivateDriverDataSize;
    HANDLE                      hContext;         // out:
} D3DDDICB_CREATECONTEXTVIRTUAL;
```
— `tmp/dx12/sdk/d3dumddi.h:3976-3984`.

**So the queue-class → engine-node mapping is entirely the UMD's choice.** On a single-node adapter
a D3D12 UMD maps `DIRECT`, `COMPUTE` and `COPY` all onto `NodeOrdinal = 0`. The runtime does not
fail; dxgkrnl does not synthesise engines. This is the same thing every integrated single-engine
WDDM driver does.

- **Strategy (b) (vkd3d):** wholly irrelevant — vkd3d maps D3D12 queues to Vulkan queues, and the
  Venus ICD's queues are host-side.
- **Strategy (a) (native UMD):** a one-line policy decision in the UMD. **Zero KMD work.**

Residual risk, named: with all three queue classes on one node, a D3D12 app that overlaps a COPY
queue with a DIRECT queue gets serialization instead of parallelism. That is a performance property
of a one-engine GPU, not a correctness gap.

---

## 4. Hardware queues / GPU scheduling

**D3D12 does not require hardware scheduling, at any Windows build.**

- WDDM 2.0 is the D3D12 prerequisite (`windows-vista-display-driver-model-design-guide.md:35`).
  Hardware-accelerated GPU scheduling arrived in **WDDM 2.6 / Windows 10 1903**
  (same table, `:41`) — four versions *after* D3D12 shipped, and it is an opt-in driver capability
  ([DirectX Developer Blog, "Hardware Accelerated GPU
  Scheduling"](https://devblogs.microsoft.com/directx/hardware-accelerated-gpu-scheduling/);
  [`D3DKMT_WDDM_2_7_CAPS`](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmdt/ns-d3dkmdt-d3dkmt_wddm_2_7_caps)).
- HWS has never become mandatory: it is a per-driver capability with a user-facing Settings toggle,
  and Windows 11 24H2/25H2 still runs D3D12 on non-HWS adapters (WARP, older discrete parts,
  virtualized adapters).
- Helios declares **no** hardware-scheduling capability anywhere. `DXGK_VIDSCHCAPS.HwQueuePacketCap`
  (bits 7-10, `d3dkmddi.h:2016`) is 0; `NativeGpuFence` (bit 11) is 0;
  `DXGKQAITYPE_USERMODESUBMISSION_CAPS` (=38) and `DXGKQAITYPE_NATIVE_FENCE_CAPS` (=37) fall into
  the `other => STATUS_NOT_SUPPORTED` arm (`query_adapter_info.rs:85-92`).
- `DxgkDdiCreateHwQueue` refuses at the **first** step with `STATUS_NOT_SUPPORTED` and records
  `HwQRef` (`scheduler.rs:180-187`); the doc comment at `:166-179` records why refusing at *create*
  rather than at *submit* matters (the succeed-then-fail shape is the VidSch `0x119`/Arg1=2
  bugcheck).
- **Live: `HwQRef` and `PHQcall` are ABSENT from the service key** (this session) — those DDIs have
  never been called. Consistent with §0.2's version gate.

**KMD work for D3D12: none.** The right thing on this adapter is to stay non-HWS, and the refusal
is already correctly shaped and counted.

---

## 5. Monitored fences — what the miniport actually has to implement

**Answer: nothing beyond what is already there.** `ID3D12Fence` maps to a WDDM monitored fence, and
on a non-HWS, non-native-fence adapter *dxgkrnl owns the entire mechanism*.

Microsoft's own description of the pre-WDDM-3.2 model
(`windows-driver-docs-.../display/native-gpu-fence-objects.md`), verbatim:

> WDDM 2.x's monitored fence synchronization object supports the following operations:
> * The CPU waits on a monitored fence value, either by: Polling using a CPU virtual address (VA).
>   Queuing a blocking wait inside *Dxgkrnl* that gets signaled when the CPU observes the new
>   monitored fence value.
> * CPU signal of a monitored value.
> * GPU signal of a monitored value by writing to the monitored fence GPU VA and raising a monitored
>   fence signaled interrupt …
>
> **What wasn't supported was a native on-the-GPU wait for a monitored fence value. Instead, the OS
> held GPU work that depends on the waited value on the CPU. It only released this work to the GPU
> when the value is signaled.**

And `windows-driver-docs-.../display/context-monitoring.md`, verbatim:

> **GPU signal** — If a GPU engine isn't capable of writing to a monitored fence using its virtual
> address, the UMD uses the *SignalSynchronizationObjectFromGpuCb* callback to queue a software
> signal packet to the GPU context.
>
> **GPU wait** — To wait on a monitored fence on a GPU engine, the UMD first needs to flush its
> pending command buffer then call *WaitForSynchronizationObjectFromGpuCb* … *Dxgkrnl* queues the
> dependency to its internal database, then returns immediately to the UMD … Command buffers
> submitted after the wait operation aren't scheduled for execution until the wait operation is
> satisfied.
>
> **CPU signal** — … *Dxgkrnl* updates the fence memory location with the signaled value.

Every one of those is a dxgkrnl/VidSch action. The miniport's only involvement is retiring DMA
packets — which Helios does, via `DXGK_INTERRUPT_DMA_COMPLETED`
(`kmd_render/src/ddi/submit_command.rs:346-350`, `DmaCompleted.SubmissionFenceId = fence`).
The only interrupt types Helios ever raises are `DMA_COMPLETED` (`submit_command.rs:346`),
`CRTC_VSYNC` (`:372`) and `DMA_PREEMPTED` (`:390`). `MONITORED_FENCE_SIGNALED` is never raised —
correct, because Helios never writes fence memory.

**Required vs optional miniport DDIs:**

| DDI | Required? | Why |
|---|---|---|
| `DxgkDdiSignalMonitoredFence` | **Optional** — WDDM 2.5 slot (`dispmprt.h:2967`), above our declared 2.1 | The GPU-writes-the-fence-VA fast path. Unset; the software path is used instead. |
| `DxgkDdiCreateNativeFence` / `Destroy` / `Open` / `Close` / `SetNativeFenceLogBuffer` / `UpdateNativeFenceLogs` / `UpdateMonitoredValues` / `UpdateCurrentValuesFromCpu` | **Optional** — WDDM 3.1/3.2, gated on `DXGK_VIDSCHCAPS.NativeGpuFence` = 0 | Native GPU fences = HWS stage 2, "supported starting in Windows 11, version 24H2 (WDDM 3.2)" (`native-gpu-fence-objects.md`). |
| `DXGKQAITYPE_NATIVE_FENCE_CAPS` (37) | Optional | Rejected `NOT_SUPPORTED` — the honest answer. |

**Local proof it already works.** `tools/vehicle_flipwait_probe.c:1-24` queues
`WAIT(F >= 1)` then `SIGNAL(G = 5)` on a context and asserts G stays 0 until the CPU signals F.
`ROADMAP.md:2605-2610` records the result verbatim:

> `tools/vehicle_flipwait_probe.c` **PROVES the primitive live on our software-scheduled adapter**
> (queued signal held behind an unsatisfied wait, drained ~10 ms after the CPU signal; **ZERO KMD
> changes**) and the topology: raw cross-device sync handles are REJECTED 0xC000000D — the fence
> must be NT-shared (`D3DKMTShareObjects`) and reopened via `OpenSyncObjectFromNtHandle2` on the
> device owning the waiting context.

That last clause is the one thing a D3D12 implementer must know: **cross-device monitored-fence
handles must go through `D3DKMTShareObjects` + `OpenSyncObjectFromNtHandle2`**, not raw handles.
That is a UMD/runtime concern (and `ID3D12Fence` shared handles already work that way), not a KMD
gap.

`DXGK_VIDSCHCAPS.No64BitAtomics` = 0 (bit 5, `d3dkmddi.h:2011`) — i.e. Helios claims 64-bit atomic
fence updates. Correct: the writes are dxgkrnl's CPU writes, which are atomic.

**KMD work for D3D12 fences: none.**

---

## 6. GPU virtual addressing — walking the actual chain

**Who assigns the VA a D3D12 resource reports?** VidMm does, and it already does it today.

1. UMD calls `D3DKMTReserveGpuVirtualAddress` / `MapGpuVirtualAddress` / `UpdateGpuVirtualAddress`
   (`tmp/dx12/sdk/d3dkmthk.h:5760-5763, 6026-6029`) — or gets a VA back from
   `D3DKMTCreateAllocation` on a VA-capable context.
2. VidMm picks the address out of its own per-process GPU VA space, bounded by
   `DXGK_GPUMMUCAPS.VirtualAddressBitCount` = 40 (`gpummu.rs:142`).
3. VidMm materialises the mapping by pushing PTEs at the miniport as
   `DXGK_OPERATION_UPDATE_PAGE_TABLE` (=11, `tmp/dxgk_bindings.rs:66778`) through
   `DxgkDdiBuildPagingBuffer` — because `PageTableUpdateMode` is `GPU_PHYSICAL`
   (`gpummu.rs:141`), not `CPU_VIRTUAL`.
4. `dxgkddi_build_paging_buffer` handles that op at `build_paging_buffer.rs:1329-1353`: it records
   the leaf mapping in `adapter.paging_pte_shadow` and harvests BAR placements, then returns
   `STATUS_SUCCESS`. It **never advances `pDmaBuffer`** — "no hardware command is ever emitted"
   (`:1306-1308`).
5. Nothing in the guest or on the host ever reads those PTEs. `gpummu.rs:1-14`, verbatim:
   > Helios has no guest GPU MMU: venus addresses host resources by opaque id and the **host GPU
   > owns the real MMU**, so the guest page tables are *decorative* — their content is never read by
   > any hardware.

**So the VAs are real, OS-allocated, non-overlapping addresses. They are simply not honoured by
anything.** That is a UMD-layer problem, and only for one of the two strategies:

- **Strategy (b) (vkd3d over Venus):** a non-issue. vkd3d uses Vulkan buffer-device-address, which
  the *host* GPU resolves natively. `ID3D12Resource::GetGPUVirtualAddress` is answered by
  vkd3d out of `vkGetBufferDeviceAddress` — the WDDM VA never enters the picture.
- **Strategy (a) (native `d3d12umddi` UMD):** the UMD must maintain its own VA→venus-resource-id
  map and rewrite every root descriptor, indirect-argument buffer and RTAS address before it
  reaches the host. **That is a UMD cost, not a KMD one.** The KMD needs no change: the paging
  DDIs it has already answer everything VidMm asks.

The four page-table DDIs `DescribePageTable`/`UpdatePageTable`/`UpdatePageDirectory`/
`MovePageDirectory` are unset (`dispmprt.h:2779-2782`) and are `PVOID`-typed reserved slots in this
WDK. They belong to the `CPU_VIRTUAL` update mode, which Helios does not use. Consistent.

`DXGK_OPERATION_MAP_MMU` (=19) / `UNMAP_MMU` (=20) reach `PagingOperation::Other` and return
`STATUS_SUCCESS` (`build_paging_buffer.rs:1215, 1456-1461`); they are IoMmu-mode operations, and
`IoMmuSupported = 0` (`query_adapter_info.rs:1284`), so they should never arrive.

---

## 7. Residency & paging

**There is no `DxgkDdiOfferAllocations` / `ReclaimAllocations` slot in this WDK.** Confirmed by the
187-name field list parsed this session: neither name appears. DX12.md §3.2's finding stands.

What reaches the miniport on the residency path is `DxgkDdiBuildPagingBuffer`. Helios handles
exactly six of the 23 `DXGK_OPERATION_*` values (`tmp/dxgk_bindings.rs:66767-66789`), via
`PagingOperation::parse` (`build_paging_buffer.rs:1195-1218`):

| Op | value | Helios |
|---|---|---|
| `TRANSFER` | 0 | real — `bar_transfer` (`:1388`) |
| `FILL` | 1 | real — `bar_fill` (`:1389`) |
| `DISCARD_CONTENT` | 2 | real (`:1390`) |
| `VIRTUAL_TRANSFER` | 8 | real (`:1443`) |
| `VIRTUAL_FILL` | 9 | real (`:1402`) |
| `UPDATE_PAGE_TABLE` | 11 | shadow + harvest, `STATUS_SUCCESS` (`:1329-1353`) |
| everything else (incl. `MAP_APERTURE_SEGMENT`, `SIGNAL_MONITORED_FENCE`, `NOTIFY_RESIDENCY`, `INIT_CONTEXT_RESOURCE`, `FLUSH_TLB`) | — | `PagingOperation::Other` → `STATUS_SUCCESS` no-op (`:1215, 1456-1461`) |

**D3D12 residency model.** `MakeResident`/`Evict` are UMD-level callbacks
(`pfnMakeResidentCb`/`pfnEvictCb`), and VidMm turns them into paging operations. There is nothing
extra the miniport has to implement. `driver-residency-in-wddm-2-0.md` and `residency-overview.md`
place the whole model in VidMm + UMD.

`DXGK_CONTEXTINFO_CAPS.DriverManagesResidency` (`d3dkmddi.h:1550-1563`) — the flag by which a
driver takes residency over — is **not** set (Helios never writes `ContextInfo.Caps` at all,
`device.rs:414-428`). That is the correct default.

**Residency budgets** (which D3D12 apps read via `IDXGIAdapter3::QueryVideoMemoryInfo`) require the
`ApplicationTarget` segment bit: *"There is a new **ApplicationTarget** bit in `DXGK_SEGMENTFLAGS2`
… that needs to be set on segments that the kernel mode driver wishes to be included in the
budgeting logic"* (`process-residency-budgets.md`). Helios sets it, and `LocalBudgetGroup` with it,
on the BAR segment when `vidmm_vram_size(knobs).is_some()`
(`query_adapter_info.rs:1002-1010, 814-818`) — and `VIDMM_VRAM_MB_DEFAULT = 4096`
(`kmd_render/src/adapter/mod.rs:117`), which is in `256..=65536`
(`kmd_render/src/ddi/bar_segment.rs:18-27`), so **`ApplicationTarget` is ON by default and
`VidVram = 4096` reads back from the live key**. So D3D12 sees a ~4 GiB local budget group.

**Segment topology and the aperture-LAST invariant.** Live shape is `[Aperture(id 1),
Bar(id 2)]` (`kmd_render/src/ddi/segment_table.rs:107-129`, `bar_segment.rs:167-180`), with
`PagingBufferSegmentId = APERTURE_SEGMENT_ID = 1` (`query_adapter_info.rs:1076`, `gpummu.rs:78`).
The invariant is *"a `SupportsCpuHostAperture` segment must be the LAST reported segment"*
(`segment_table.rs:5-9`), enforced by construction in `SegmentTable::new` (`:136-160`) with a
counted refusal, `SegRule` — **live `SegRule = 0`, `SegCntMis = 0`**.

**Risk from the invariant for D3D12: none, but it is a hard ceiling.** `SegmentTable::MAX = 2`
(`:119`). If someone later wants a *separate* D3D12 upload heap segment or a non-CPU-visible local
segment, the two-slot cap plus the cpu-host-must-be-last rule means the only legal third topology
is `[Aperture, <new non-cpu-host segment>, Bar]` — and `MAX` would have to be raised. Anything that
puts a segment *after* the cpu-host BAR segment is Code 43 (ETW-proven 2026-07-05,
`bar_segment.rs:44-55`).

---

## 8. Allocations: D3D12 shapes vs `create_allocation.rs`

| D3D12 shape | What reaches the miniport | Helios today |
|---|---|---|
| **Committed resource** | one `DxgkDdiCreateAllocation` with UMD private data | ✅ if the private data is `HeliosWddmAllocPrivate` — see §0.3. |
| **Heap + placed resources** (`D3D12_HEAP_FLAG_*`, `CreatePlacedResource`) | one allocation for the heap; placed resources are pure VA mappings (`UPDATE_PAGE_TABLE`) | ✅ structurally — the heap is just another allocation; the VA mappings are no-ops. Semantics live in the UMD. |
| **Reserved (tiled) resources** | VA reservation + `UpdateGpuVirtualAddress` tile remaps | ⚠ Structurally accepted (`UPDATE_PAGE_TABLE` succeeds), **semantically unmodelled** — the guest page tables are decorative (`gpummu.rs:1-14`), so a tile remap has no effect anywhere. Correct sparse behaviour must come from the layer that talks to venus. `DXGK_GPUMMUCAPS` advertises none of the sparse-adjacent bits (`gpummu.rs:135-136`), which is the honest state. |
| **Cross-adapter** (`D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER`) | needs `DXGK_VIDMMCAPS.CrossAdapterResource` | ❌ **OFF by default.** `cross_adapter: read_config_dword(knobs::CROSS_ADAPT_CAPS, 0) != 0` (`adapter/mod.rs:247`); `CrossAdaptCaps` is ABSENT from the live service key. A knob flip (`reg add` + `pnputil /restart-device`) turns it on; the KMD's cross-adapter pitch logic already exists (`create_allocation.rs:1437-1441` → `kmd_logic/src/lib.rs:120`). |
| **Shared heaps / NT-shared resources** | standard allocation + `D3DKMTShareObjects` | ✅ the open path exists (`dxgkddi_open_allocation`, `create_allocation.rs:2860-3060`), with a liveness gate that fails a dead venus resid loudly (`:2917-2936`). |
| **GPU upload heaps** (`D3D12_HEAP_TYPE_GPU_UPLOAD`) | a CPU-visible device-local allocation | ⚠ **UNVERIFIED.** The BAR memory segment is CPU-host-aperture-mapped, which is structurally the right thing, but the surfacing cap is a UMD/`D3D12DDICAPS` one. **Settling read:** R1/R2's `D3D12DDICAPS_TYPE_*` inventory for the `OPTIONS16` GPU-upload-heap cap, then check whether it depends on any KMD segment flag. |
| **Query heaps / command allocators** | ordinary allocations | ✅ same as committed. |
| **Standard allocations** (`GetStandardAllocationDriverData`) | KMD authors the private data itself | ✅ `create_allocation.rs:3146-3180` authors a `HeliosWddmAllocPrivate` + `HeliosWddmAllocMeta` pair, `PRIV_SIZE = 96`. |

Bounded-table headroom, for a D3D12 app that allocates far more objects than a D3D11 one:
`MAX_BLOBS = 8192`, `MAX_RESOURCES = 16384`, `MAX_CONTEXTS = 1024`
(`kmd_render/src/virtio/gpu/mod.rs:125, 129, 131`), `MAX_MAPPINGS = 8192`
(`kmd_render/src/mapping.rs:41`). These are adapter-global and already sized for a DOOM level load
(`mapping.rs:34-41`). ⚠ A D3D12 title with tens of thousands of placed resources on a handful of
heaps stays well inside them (heaps are the allocations, not the resources); a title that
committed-allocates per-resource could not.

---

## 9. Preemption & TDR

**D3D12 expectations:** none beyond WDDM 1.2's. `gpu-preemption.md` requires: compile at
`>= DXGKDDI_INTERFACE_VERSION_WIN8`; set `PreemptionAware` and `MultiEngineAware`; report a
granularity; support `FlipOnVSyncMmIo`. Helios does all four (`query_adapter_info.rs:236-253, 297,
389, 395`).

**What Helios does:**
- `DxgkDdiPreemptCommand` (`submit_command.rs:905-931`) drops every pending WDDM fence under the
  notification lock and acknowledges with a `DMA_PREEMPTED` packet (`:390`,
  `AbandonOutcome::Preempted`).
- `DxgkDdiResetFromTimeout` (`:936-963`) drops pending fences silently (dxgkrnl owns post-reset
  state), purges present streams, and reports transport failure through `StRing`
  — **live `StRing = 0`**.
- `DxgkDdiRestartFromTimeout` (`:970-976`) returns `STATUS_SUCCESS`.
- `DxgkDdiResetEngine` (`scheduler.rs:95-123`) reports `LastAbortedFenceId` and purges streams.
- `SupportPerEngineTDR = 1` (`query_adapter_info.rs:252-253`).
- `ABANDONED_FENCES` (`submit_command.rs:806`) counts fences discarded by any of the three.

**Gap for D3D12: none.** ⚠ The *known* residual is the Xid-109 / host-context-death class
(memory `phase-d-sweeps-insufficient-xid-gates-68th.md`), which is a host+ICD stability item that
D3D12 would inherit unchanged, not a D3D12-specific KMD gap.

---

## 10. THE ANSWER — ordered KMD work list for D3D12

### If the strategy is (b), vkd3d-proton over the Venus ICD: **the list is empty.**

Defence: a D3D12 app under vkd3d is, at the WDDM layer, indistinguishable from the Vulkan and DXVK
clients that already run. Its GPU work goes down `D3DKMTEscape`/`HELIOS_ESCAPE_SUBMIT_VENUS`
(§0.1), its allocations are authored by the ICD in the format `create_allocation.rs` demands
(§0.3), its fences are dxgkrnl monitored fences that already work (§5), its GPU VAs are Vulkan
device addresses the host resolves (§6), and its residency is VidMm's (§7). **The KMD does not know
the difference between a D3D11 DXVK frame and a D3D12 vkd3d frame, and that is the point.** The
open risks under (b) are presentation (R7's lane) and Vulkan feature coverage (R12's lane) — not
the KMD.

Three *conditional* items, not required for a first triangle:

| # | Item | Trigger | Size | Risk to D3D11 |
|---|---|---|---|---|
| C1 | Flip `CrossAdaptCaps` on | only if a D3D12 sample needs a cross-adapter / BLT-model swapchain | S (registry knob, no rebuild) | Low, but it is a cap-surface change: re-run the boot gate. It changes `MemoryManagementCaps` (`0x01D4` diag record). |
| C2 | Raise `SegmentTable::MAX` and add a segment | only if D3D12 needs a distinct heap segment | M | **High** — the cpu-host-must-be-last rule is Code-43 territory (`segment_table.rs:5-9`). |
| C3 | Revisit `ApertureSegmentCommitLimit` (64 MiB) | only if D3D12 residency budgets read too small | S (one const) | Medium — it is an advertised capability; needs a measurement first (`query_adapter_info.rs:592-597`). |

### If the strategy is (a), a native `d3d12umddi` UMD: **three items, one of them load-bearing.**

| # | Item | Why | Evidence | Size | First triangle? | Risk to the D3D11 desktop |
|---|---|---|---|---|---|---|
| **W1** | **The D3D12 UMD must author `HeliosWddmAllocPrivate` (and, where applicable, the `HeliosWddmAllocMeta` trailer) on every `pfnAllocateCb`.** Alternatively, teach `create_one` a second, D3D12-shaped private-data kind — but the honest shape is to reuse the existing one. | `DxgkDdiCreateAllocation` returns `STATUS_INVALID_PARAMETER` for anything else. **This is the one thing that turns "no KMD work" into "some work" for strategy (a).** | `create_allocation.rs:2291-2307`; `protocol/src/wddm.rs:102-151` | **M** (mostly UMD; S if the KMD is untouched) | **YES** — nothing allocates without it | **None** if the KMD is untouched. If a second private-data kind is added, it is a change to the hottest allocation path — every write must be per-arm length-validated (CLAUDE.md invariant). |
| W2 | Validate `NodeOrdinal`/`EngineAffinity` in `DxgkDdiCreateContext` and count refusals (`CtxNode`), the way `CreateHwContext` already does. | Today a context for a node that does not exist is accepted silently. A D3D12 UMD asking for a COPY node would get a wrong context instead of a loud refusal. CLAUDE.md rule 2. | `device.rs:389-431` (no check) vs `scheduler.rs:135-137` (checked) | **S** | No | **Low** — but it is a *new refusal on a live path*. Gate it behind evidence that today's callers always pass 0 (add the counter first, ship the refusal second). |
| W3 | Set `ContextInfo.Caps.NoPatchingRequired = 1` and shrink `AllocationListSize`/`PatchLocationListSize` for contexts created with `DXGK_CREATECONTEXTFLAGS.VirtualAddressing`. | The documented shape for a GPU-VA context: "no patch location list will be allocated and only a very small allocation list (16 entries)" (`residency-overview.md`). Helios asks for 256+256 on every context and no-ops `DxgkDdiPatch`. | `device.rs:414-428`; `d3dkmddi.h:1550-1580`; `submit_command.rs:1421-1430` | **S** | No | **Medium** — it changes what dxgkrnl allocates per context for *every* client including DWM. The allocation list is also how the Present path receives its surfaces (`residency-overview.md`: "the allocation list is used in the kernel mode driver *Present* path today"), so shrinking it without an A/B is exactly the class of change that has bitten this driver before. Do it last, behind a knob, with a paired GT1/GT2 measurement. |

**What is explicitly NOT on the list, and why:**

- ❌ More engine nodes — the queue→node mapping is a UMD choice (§3).
- ❌ Hardware queues / HWS — not required by D3D12 at any Windows build (§4).
- ❌ `DxgkDdiSignalMonitoredFence` / native fences — optional, and the software path is proven live
  (§5).
- ❌ Real GPU page tables — the VAs are OS-assigned and already consistent; translation is a UMD
  problem (§6).
- ❌ Residency DDIs — there are none to implement in this WDK (§7).
- ❌ Any of the 31 reachable-unset slots — none is class (c) (§1.2).

---

## 11. Where DX12.md §3 is right, wrong, or stale

**Right (re-derived and confirmed):**
- 84 of 187 `DxgkDdi*` slots set; 193 struct fields; 103 unset. ✅
- One `DXGK_ENGINE_TYPE_3D` node; `NbAsymetricProcessingNodes = 1`;
  `SchedulingCaps = MultiEngineAware|PreemptionAware`; the `(0,0)`-only scheduler DDIs;
  `CreateContext` ignoring `NodeOrdinal`/`EngineAffinity`/`Flags`; `DmaBufferSegmentSet=1`,
  `DmaBufferSize=256 KiB`, `AllocationListSize = PatchLocationListSize =
  DXGK_ALLOCATION_LIST_SIZE_GDICONTEXT`; `MaxAllocationListSlotId = 0xFFFF`. ✅ (all line refs check out)
- Hardware scheduling refused at `CreateHwQueue` with `HwQRef`; `PHQcall` absent. ✅ **and I can now
  add that `HwQRef` is absent too.**
- No `Offer`/`Reclaim` DDI slot exists in this WDK. ✅
- `WddmSurface::Wddm2_1GpuMmu` is the live level; 40-bit VA, 4 KiB pages, 4 levels (9+9+9+1),
  system-memory segment 0, `GPU_PHYSICAL`, all optional `GPUMMUCAPS` bits 0;
  `UpdatePageTable`/`UpdatePageDirectory`/`MovePageDirectory`/`DescribePageTable` unset;
  `SetRootPageTable`/`GetRootPageTableSize` set. ✅
- The unset-fence / doorbell / context-scheduling / protected-content / MPO families. ✅
- "The KMD has no D3D12 awareness of any kind, which is correct — WDDM has no D3D12-specific
  miniport DDI." ✅

**Wrong:**
1. **§3.2: "`DXGK_VIDMMCAPS.DriverManagesResidency` exists in the bindings
   (`tmp/dxgk_bindings.rs:43357`) and is not set by this driver."** — `DriverManagesResidency` is
   **not** a `DXGK_VIDMMCAPS` bit. `DXGK_VIDMMCAPS`'s bits are listed at
   `tmp/dx12/sdk/d3dkmddi.h:2255-2290` and it is not among them. It is
   `DXGK_CONTEXTINFO_CAPS.DriverManagesResidency`, bit 1 of a **per-context** flags word
   (`d3dkmddi.h:1550-1563`), which is why the bindgen accessor at `tmp/dxgk_bindings.rs:43357`
   sits next to `NoPatchingRequired` (bit 0). The conclusion ("residency is dxgkrnl's job, which is
   the right default") is unaffected, but the *mechanism* is per-context, and Helios never writes
   `ContextInfo.Caps` at all.
2. **§1.4: "A case-insensitive grep for `d3d12` across `kmd_render/src/` returns one hit:
   `create_allocation.rs:1436-1438`."** — the hit is at **`create_allocation.rs:1438`**, and there
   is a second, sibling hit in the same lineage at `kmd_logic/src/lib.rs:120`. Immaterial, but the
   line number is off by two.

**Incomplete / stale (the big one):**
3. **§3 treats "103 unset slots" as one population.** It is two populations: **31 unset-and-reachable**
   and **72 unset-and-unreachable-at-the-declared-WDDM-2.1-level**. Every fence, doorbell,
   context-scheduling and protected-session slot §3.5 lists is in the *unreachable* bucket. That
   materially changes the shape of the gap: the question is not "which of 103 do we implement" but
   "does the WDDM 2.1 surface carry D3D12" (it does).
4. **§3.1's "Hardware scheduling is refused, deliberately and consistently"** is true but
   understated: at WDDM 2.1 the hardware-scheduling slots are almost certainly **never reached at
   all**, so the refusal is defence in depth, not the operative mechanism. §3.1's own evidence
   (`PHQcall` absent) is better explained by the version gate than by the refusal.
5. **§3.1 marks "whether dxgkrnl synthesises COPY/COMPUTE queues over one node" UNVERIFIED.** It is
   now settled from the headers: `D3D12DDIARG_CREATECOMMANDQUEUE_0001` has no node ordinal
   (`d3d12umddi.h:1450-1456`) and the UMD picks the node in `D3DDDICB_CREATECONTEXTVIRTUAL`
   (`d3dumddi.h:3976-3984`). No KMD involvement.
6. **§3.5 omits `DxgkDdiUpdateMonitoredValues` and `DxgkDdiUpdateCurrentValuesFromCpu`** from the
   fence family (both WDDM 3.1, both unset). Harmless.
7. **§3 does not mention the `CreateAllocation` private-data gate**, which is the single most
   consequential KMD fact for a native D3D12 UMD (§0.3).
8. **§3 does not mention that `CrossAdaptCaps` defaults to 0**, so the cross-adapter cap it
   discusses is *not* advertised on the live box.

---

## 12. UNVERIFIED, with settling experiments

| # | Question | Settling experiment |
|---|---|---|
| U1 | Does dxgkrnl truncate `DRIVER_INITIALIZATION_DATA` at the declared `Version`, making the 9 above-2.1 set slots dead? | Add an unconditional `HwQEnt` counter as the *first* statement of `dxgkddi_create_hw_queue`, deploy, run a GPU workload, read the key. Or disassemble `dxgkrnl!DpiInitializeEx` under `ntoseye`. Or (diagnostic only) raise `SURFACE` to 3.2 on a throwaway boot and see whether `HwQRef` appears. |
| U2 | Does the D3D12 runtime tolerate `DXGKQAITYPE_PHYSICALADAPTERCAPS` (=15) returning `STATUS_NOT_SUPPORTED`, the way DXGI/D3D11 does? Answering it is currently a **bugcheck 0x3B in dxgmms2** (`query_adapter_info.rs:78-84`). | Once `OpenAdapter12` returns something, call `D3D12CreateDevice` and check for failure; separately, ETW `Microsoft-Windows-DxgKrnl` all-keywords → `AzureTriage` around the create. This is the highest-probability D3D12-specific KMD surprise. |
| U3 | Is `DXGKARG_CREATECONTEXT.ContextInfo` zero-initialised by dxgkrnl before the DDI? Helios writes 5 of its 8 fields and leaves `Reserved`, `Caps`, `PagingCompanionNodeId` untouched. | Read `ContextInfo` at entry in a debug build (or `ntoseye` breakpoint on `dxgkddi_create_context`) and dump the 32 bytes. |
| U4 | Does dxgkrnl reject `NodeOrdinal > 0` before reaching `DxgkDdiCreateContext`? | A `tools/` probe calling `D3DKMTCreateContextVirtual` with `NodeOrdinal = 1`; record the NTSTATUS. Pairs with W2. |
| U5 | Is `D3D12_FEATURE_DATA_GPU_VIRTUAL_ADDRESS_SUPPORT.MaxGPUVirtualAddressBitsPerProcess` derived from the KMD's `VirtualAddressBitCount` (40)? | `CheckFeatureSupport` on Helios once a D3D12 device exists, compared with `D3DKMTQueryAdapterInfo(KMTQAITYPE_GPUMMU_CAPS)` on the same adapter. |
| U6 | Are `MAX_BLOBS = 8192` / `MAX_MAPPINGS = 8192` enough for a real D3D12 title? | Run a D3D12 sample under vkd3d and read `HELIOS_ESCAPE_QUERY_STATS` (`escape.rs:966`) / the blob-table counters; the existing `tools/read-vehicle-counters.ps1` shape. |
| U7 | Does `ApertureSegmentCommitLimit = 64 MiB` under-report D3D12's shared budget? | `IDXGIAdapter3::QueryVideoMemoryInfo` on both `LOCAL` and `NON_LOCAL` pools under a D3D12 device; `tools/vram_report_probe.cpp` already does the DXGI/VidMm/Venus comparison for D3D11. |
| U8 | Do D3D12 GPU upload heaps (`D3D12_HEAP_TYPE_GPU_UPLOAD`) need a KMD segment flag Helios does not set? | R1/R2's `D3D12DDICAPS_TYPE_*` inventory for the `OPTIONS16` cap; then check whether any `DXGK_SEGMENTFLAGS2` bit gates it. |

---

## 13. Load-bearing facts other lanes must not contradict

1. **All Vulkan/venus GPU submission goes through `D3DKMTEscape`, not `D3DKMTRender`.**
   `vn_renderer_helios.c:103, 1596, 1257`; `submit_command.rs:725-799` forwards no venus.
2. **`DxgkDdiCreateAllocation` hard-refuses allocations without a valid 48-byte
   `HeliosWddmAllocPrivate` (magic + version).** `create_allocation.rs:2291-2307`;
   `protocol/src/wddm.rs:102-151`.
3. **The adapter declares WDDM 2.1 (`DXGKDDI_INTERFACE_VERSION_WDDM2_1` = 24579), not 3.2.**
   `wddm_surface.rs:64`; `lib.rs:103`. WDDM 2.0 is the D3D12 floor, so 2.1 clears it.
4. **One engine node, `DXGK_ENGINE_TYPE_3D`, ordinal 0 only.**
   `query_adapter_info.rs:1261-1287, 456-464`.
5. **D3D12 queue class → WDDM node is a UMD decision.** `d3d12umddi.h:1450-1456` (no node ordinal);
   `d3dumddi.h:3976-3984` (the UMD supplies it).
6. **Monitored fences work today with zero KMD support beyond DMA-packet retirement**, proven by
   `tools/vehicle_flipwait_probe.c` (`ROADMAP.md:2605-2610`) and explained by
   `context-monitoring.md` + `native-gpu-fence-objects.md`.
7. **Guest GPU page tables are decorative; the host GPU owns the real MMU.** `gpummu.rs:1-14`.
8. **Segment topology is `[Aperture id 1, Bar id 2]`, `SegmentTable::MAX = 2`, and a
   `SupportsCpuHostAperture` segment must be LAST.** `segment_table.rs:5-9, 107-129`.
9. **`ApplicationTarget` + `LocalBudgetGroup` are ON for the BAR segment by default
   (`VidMmVramMB` default 4096, live key confirms).** `adapter/mod.rs:117`;
   `query_adapter_info.rs:814-818, 1002-1010`.
10. **`CrossAdapterResource` is OFF by default** (`CrossAdaptCaps` absent from the live key).
    `adapter/mod.rs:247`.
11. **`FlipImmediateMmIo` must not be re-added** — it is defect 0ab.
    `query_adapter_info.rs:377-388`. Live `FlipCapV = 2`.
