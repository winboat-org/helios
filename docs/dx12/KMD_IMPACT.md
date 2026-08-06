# KMD_IMPACT.md — what D3D12 asks of `kmd_render`

**What this is:** the evidence for the claim that D3D12 needs almost nothing new from the WDDM
miniport, and the precise, ordered list of the exceptions.

**What this is not:** a survey of the miniport. It answers one question — *what changes in
`kmd_render/` when Helios gains D3D12?* — and it answers it separately for the two arms of the plan
(`DX12.md` §1): **Phase 0**, app-local vkd3d over the Vulkan ICD, and the **DDI arm**,
`helios_umd12.dll` implementing `d3d12umddi`.

Source dossiers: `research/R5-kmd-gap.md`, `research/R6-d3dkmt-surface.md`. Every claim below was
re-derived from the code this session; §15 lists where the pre-2026-08-05 `DX12.md` §3 was wrong.

⚠ Header line numbers are pinned to **Windows SDK / WDK 10.0.26100.0**, staged at `tmp/dx12/sdk/`
(not committed — re-stage with the command in `docs/dx12/DECISIONS.md`).

---

## 1. The answer, up front

**Phase 0 (app-local vkd3d): the KMD work list is empty.**

A D3D12 application running under vkd3d-proton is, at the WDDM layer, *indistinguishable from the
Vulkan and DXVK clients that already run this desktop*. Its GPU work goes down
`D3DKMTEscape`/`HELIOS_ESCAPE_SUBMIT_VENUS` (§2a). Its allocations are authored by the Venus ICD in
exactly the format `create_allocation.rs` demands (§2c). Its fences are dxgkrnl monitored fences
that already work here (§7). Its GPU virtual addresses are Vulkan device addresses the *host*
resolves (§8). Its residency is VidMm's (§9). **The KMD cannot tell a D3D12 vkd3d frame from a
D3D11 DXVK frame, and that is the point.** The open risks in Phase 0 are presentation
(`docs/dx12/PRESENT.md`) and Vulkan coverage (`docs/dx12/SUBSTRATE.md`) — not the miniport.

**The DDI arm: three items, none required for the first triangle** (K1/K2/K3, §14). Plus one fact
that is a **UMD** obligation but is the thing most likely to surprise a D3D12 implementer:
`DxgkDdiCreateAllocation` hard-refuses any allocation that does not carry a valid
`HeliosWddmAllocPrivate` blob (§2c).

Everything people expect to be needed, and is not: more engine nodes (§5), hardware queues /
GPU scheduling (§6), `DxgkDdiSignalMonitoredFence` or native fences (§7), real GPU page tables
(§8), residency DDIs (§9). Each has its evidence below.

---

## 2. The three facts that reframe the question

### 2a. The rendering path does not go through the WDDM scheduler at all

The Mesa Venus ICD submits every Vulkan command stream through **`D3DKMTEscape`**, not
`D3DKMTRender`/`D3DKMTSubmitCommand`:

| Fact | Where |
|---|---|
| `#define HELIOS_ESCAPE_SUBMIT_VENUS 0x0001u` | `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c:103` |
| `helios_hdr_init(&hdr.hdr, HELIOS_ESCAPE_SUBMIT_VENUS, sizeof(hdr));` | `vn_renderer_helios.c:1596` |
| `const NTSTATUS st = D3DKMTEscape(&esc);` | `vn_renderer_helios.c:1257`, `:1891` |
| *"Over D3DKMTEscape the venus stream rides INSIDE the escape buffer, directly…"* | `vn_renderer_helios.c:1607` |

And the KMD's submission DDIs forward **no** venus: `dxgkddi_submit_command`
(`kmd_render/src/ddi/submit_command.rs:766-799`) and `dxgkddi_submit_command_virtual` (`:725-760`)
only (a) count the fence, (b) `arm_dma_flip(...)` from the DMA private data, and (c)
`note_and_maybe_signal(...)`. `grep -n "submit_venus" kmd_render/src/ddi/submit_command.rs` returns
nothing.

**Consequence.** The WDDM scheduler surface — nodes, engines, hardware queues, monitored fences,
GPU VA — is not on the D3D12 *rendering* path under either arm, because under both arms the actual
draw commands reach the host as venus bytes through the escape channel. That surface is only on the
*present* path, and the present path is the one DWM already exercises every frame.

### 2b. The adapter declares WDDM 2.1, so most "unset" DDI slots are unreachable by construction

`kmd_render/src/ddi/wddm_surface.rs:64` — `pub(crate) const SURFACE: WddmSurface =
WddmSurface::Wddm2_1GpuMmu;` — drives `DRIVER_INITIALIZATION_DATA.Version` at
`kmd_render/src/lib.rs:103` to `DXGKDDI_INTERFACE_VERSION_WDDM2_1` (= 24579).

In `dispmprt.h` the `DRIVER_INITIALIZATION_DATA` body (`tmp/dx12/sdk/dispmprt.h:2690-3043`) is one
flat struct whose members sit inside nested `#if (DXGKDDI_INTERFACE_VERSION >= …)` blocks. The
WDDM 2.1 block closes at `:2900`. Bucketing all 187 `DxgkDdi*` members by the block they live in:

| Block | members | set by Helios |
|---|---:|---:|
| BASE | 61 | 50 |
| WIN7 | 8 | 2 |
| WIN8 | 12 | 9 |
| WDDM1_3 | 6 | 3 |
| WDDM2_0 | 13 | 10 |
| WDDM2_1 | 6 | 1 |
| **≤ WDDM2_1 subtotal** | **106** | **75** |
| WDDM2_2 … WDDM3_2 | **81** | **9** |

So of the 103 unset slots, **only 31 are reachable** at the declared level; the other 72 are unset
*and unreachable*. **§3 shows that none of the 31 is required for a baseline D3D12 device.**

Live corroboration, read from `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render` this
session: `HwQRef`, `PHQcall`, `PHQours`, `PHQst` are all **ABSENT**. `HwQRef` is written
unconditionally on the *first statement path* of `dxgkddi_create_hw_queue`
(`kmd_render/src/ddi/scheduler.rs:180-187`: `crate::diag::record_named_bytes(b"HwQRef", 1);`), so
its absence means that DDI has never been called in the key's lifetime.

⚠ **That absence is corroborating, not discriminating.** The hardware-queue DDIs are *both*
above the declared version *and* gated on a capability Helios never advertises (§6), so "never
called" has two sufficient explanations and does not by itself prove the version gate. A
discriminating experiment needs a slot that is version-gated but **not** capability-gated — e.g. an
unconditional counter as the first statement of `DxgkDdiExchangePreStartInfo` (WDDM2_2,
`dispmprt.h:2932`, registered by `lib.rs`) — or the `dxgkrnl!DpiInitializeEx` disassembly below.

⚠ **UNVERIFIED (mechanism).** That dxgkrnl *truncates* the table at the declared `Version` is an
inference from the header's version-conditional layout plus the counter absences. Microsoft's
`DriverEntry` doc only says to set `Version` to `DXGKDDI_INTERFACE_VERSION`; it does not state a
truncation rule. **Settling experiment:** add an unconditional `HwQEnt` counter as the *first*
statement of `dxgkddi_create_hw_queue`, deploy, run a GPU workload, read the key. Or disassemble
`dxgkrnl!DpiInitializeEx` under `ntoseye` and read the version→size table. *(Diagnostic-only
variant: raise `SURFACE` to `Wddm3_2GpuMmu` on a throwaway boot and see whether `HwQRef` appears —
⚠ 3.2 breaks DWM at `E_NOTIMPL`, `wddm_surface.rs:25-28`.)*

Nine **set** slots live above the declared version and are therefore almost certainly dead code:
`CreateHwContext`, `DestroyHwContext`, `CreateHwQueue`, `DestroyHwQueue`,
`SubmitCommandToHwQueue`, `SwitchToHwContextList`, `ExchangePreStartInfo` (all WDDM2_2),
`SetVirtualMachineData` (2.4), `PresentToHwQueue` (2.5). Leave them set — this driver has been
bitten twice by absent/null DDI slots (`DxgkDdiUpdateMonitorLinkInfo` Code 43, `DdiRenderGdi`
null-pointer bugcheck), so unregistering is a separate behaviour change with its own risk.

### 2c. ⚠ `DxgkDdiCreateAllocation` refuses any allocation without a Helios private-data blob

`kmd_render/src/ddi/create_allocation.rs:2316-2318` and `:2329-2330` (⚠ the citation drifted from
`:2291-2307`; re-pinned 2026-08-06), verbatim:

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

`HeliosWddmAllocPrivate` is 48 bytes (`protocol/src/wddm.rs:121-139`; `is_valid()` at `:167-170`
— ⚠ both re-pinned 2026-08-06 from `:102-120` / `:149-151`) and `is_valid()` is
`magic == HELIOS_WDDM_MAGIC && version == HELIOS_WDDM_VERSION` (`:149-151`).

**This is a UMD obligation, not a KMD defect.** The KMD is correctly refusing an allocation it
cannot back — it has no way to associate a bare allocation with a venus resource. But it means "the
D3D12 UMD just reuses the D3D11 UMD's KMD" is only true once `umd12` authors the same blob.

**What to copy, by path:** the D3D11 UMD already does exactly this. Read
`umd/src/forward/alloc.rs` (107 lines, *"Validated descriptors for the WDDM allocation path"*) for
the descriptor shape, and `umd/src/forward/resource.rs` for the call sites that build
`HeliosWddmAllocPrivate` and hand it to `pfnAllocateCb`. For standard/GDI shapes the KMD authors
the blob itself (`create_allocation.rs:3146-3180`, `PRIV_SIZE = 96`, a
`HeliosWddmAllocPrivate` + `HeliosWddmAllocMeta` pair) — that path needs nothing from `umd12`.

Under **Phase 0 this is a non-issue**: the Venus ICD authors the blob for every `VkDeviceMemory` it
allocates, and vkd3d allocates only through Vulkan.

---

## 3. The DDI table today

### 3.1 Counts, re-derived

- `_DRIVER_INITIALIZATION_DATA` has **193** fields, of which **187** are `DxgkDdi*` slots (the other
  six are `Version` and five `Reserved*`). `size_of == 1544`, `align_of == 8`.
- `kmd_render/src/lib.rs:106-222` sets **84** `DxgkDdi*` slots (`grep -c "data.DxgkDdi"
  kmd_render/src/lib.rs` → 84), plus `Version` at `:103`. No duplicates.
- **103 slots unset**, split 31 reachable / 72 unreachable per §2b.

### 3.2 The 31 unset-and-reachable slots, classified for D3D12

**(a)** irrelevant to D3D12 · **(b)** optional D3D12 feature (named) · **(c)** required for baseline.

| Slot | block | class | why |
|---|---|:--:|---|
| `SetPalette` | BASE | a | 8-bpp palettized VGA legacy |
| `AcquireSwizzlingRange`, `ReleaseSwizzlingRange` | BASE | a | pre-WDDM2 CPU swizzle apertures; superseded by `MapCpuHostAperture` |
| `RecommendVidPnTopology` | BASE | a | display topology |
| `StopCapture` | BASE | a | video capture |
| `CreateOverlay`, `UpdateOverlay`, `FlipOverlay`, `DestroyOverlay` | BASE | a | legacy hardware overlays (pre-MPO) |
| `LinkDevice` | BASE | a | LDA. D3D12 multi-adapter node masks would need it; Helios is `NodeMask=1` |
| `SetDisplayPrivateDriverFormat` | BASE | a | display private format |
| `DescribePageTable`, `UpdatePageTable`, `UpdatePageDirectory`, `MovePageDirectory` | WIN7 | a | `PVOID`-typed reserved slots; `CPU_VIRTUAL`-mode only — see §8 |
| `SubmitRender`, `CreateAllocation2` | WIN7 | a | `PVOID`-typed reserved slots, no public prototype in this WDK |
| `SetPowerComponentFState` | WIN8 | a | runtime power management (F-states) |
| `SetVidPnSourceAddressWithMultiPlaneOverlay` | WIN8 | b | MPO. D3D12 flip-model swapchains work without it (DWM composites) |
| `NotifySurpriseRemoval` | WIN8 | a | PnP surprise removal — a *stability* item, not a D3D12 one |
| `SetPowerPState` | WDDM1_3 | a | P-states |
| `ControlInterrupt2` | WDDM1_3 | a | `ControlInterrupt` is set (`lib.rs:220`) |
| `CheckMultiPlaneOverlaySupport`(1,2,3), `SetVidPnSourceAddressWithMultiPlaneOverlay2/3`, `PostMultiPlaneOverlayPresent` | 1_3/2_0/2_1 | b | MPO |
| `SetVideoProtectedRegion` | WDDM2_0 | b | hardware content protection / `ID3D12ProtectedResourceSession` |
| `ValidateUpdateAllocationProperty` | WDDM2_1 | b | `D3DKMTUpdateAllocationProperty`; not on the baseline path |
| `ControlModeBehavior` | WDDM2_1 | a | display mode behaviour |

**No slot in this table is class (c).** Nothing unset-and-reachable is required for a baseline
D3D12 device.

### 3.3 The 72 unreachable slots, by family

All above WDDM 2.1, all gated on capabilities Helios does not advertise:

- **Native GPU fences (3.1/3.2)** — `CreateNativeFence`, `DestroyNativeFence`, `OpenNativeFence`,
  `CloseNativeFence`, `SetNativeFenceLogBuffer`, `UpdateNativeFenceLogs`, `UpdateMonitoredValues`,
  `UpdateCurrentValuesFromCpu`. Gated on `DXGK_VIDSCHCAPS.NativeGpuFence` (bit 11,
  `d3dkmddi.h:2020`), which Helios leaves 0. See §7.
- **User-mode submission / doorbells (3.1)** — `CreateDoorbell`, `ConnectDoorbell`,
  `DisconnectDoorbell`, `DestroyDoorbell`, `NotifyWorkSubmission`.
- **HWS context scheduling (2.4)** — `SetContextSchedulingProperties`, `SuspendContext`,
  `ResumeContext`, `SetupPriorityBands`, `SetSchedulingLogBuffer`, `NotifyFocusPresent`,
  `NotifyContextPriorityChange` (3.2).
- **Protected sessions (2.3)** — `CreateProtectedSession`, `DestroyProtectedSession`.
- **Flip queue / Display-Core (2.9/3.0)** — `SetFlipQueueLogBuffer`, `UpdateFlipQueueLog`,
  `CancelQueuedFlips`, `CancelFlips`, `SetInterruptTargetPresentId`, `CreateCpuEvent`,
  `DestroyCpuEvent`, `SetAllocationBackingStore`.
- **GPU-PV / live migration (3.2)** — `CreateMemoryBasis`, `DestroyMemoryBasis`,
  `StartDirtyTracking`, `StopDirtyTracking`, `QueryDirtyBitData`, `PrepareLiveMigration`,
  `Save*/Restore*MigrationData`, `EndLiveMigration`, `WriteVirtualizedInterrupt`,
  `SetVirtualGpuResources2`, `SetVirtualFunctionPauseState`.
- **Misc** — `ResetHwEngine`, `ResumeHwEngine`, `UpdateHwContextState`, `ValidateSubmitCommand`,
  `SignalMonitoredFence`, `SaveMemoryForHotUpdate`, `RestoreMemoryForHotUpdate`,
  `CollectDiagnosticInfo`, `CollectDbgInfo2`, `ControlInterrupt3`, `GetMultiPlaneOverlayCaps`,
  `GetPostCompositionCaps`, `ResetDisplayEngine`, `Begin/EndExclusiveAccess`,
  `QueryDiagnosticTypesSupport`, `ControlDiagnosticReporting`,
  `Create/DestroyPeriodicFrameNotification`, `SetTimingsFromVidPn`, `SetTargetGamma`,
  `SetTargetContentType`, `SetTargetAnalogCopyProtection`, `SetTargetAdjustedColorimetry`(×2),
  `SetTrackedWorkloadPowerLevel`, `DisplayDetectControl`, `QueryConnectionChange`.

**Every fence, doorbell, HWS and protected-session slot a D3D12 reader would go looking for is in
this bucket.** That is the shape of the gap: not "which of 103 do we implement", but "does the
WDDM 2.1 surface carry D3D12" — and it does.

---

## 4. Adapter caps, field by field

`dxgkddi_query_adapter_info` answers **15** `DXGKQAITYPE_*` values and returns
`STATUS_NOT_SUPPORTED` for everything else (`query_adapter_info.rs:51-93`): `DRIVERCAPS`,
`QUERYSEGMENT`, `QUERYSEGMENT3`, `QUERYSEGMENT4`, `GPUMMUCAPS`, `PAGETABLELEVELDESC`,
`WDDMDEVICECAPS`, `PHYSICAL_MEMORY_CAPS`, `IOMMU_CAPS`, `HARDWARERESERVEDRANGES2`, `GPUVERSION`,
`ADAPTERPERFDATA_CAPS`, `DIRTYBITTRACKINGCAPS`, `HISTORYBUFFERPRECISION`, `64BITONLYCAPS`.

### 4.1 `DXGK_DRIVERCAPS`

Written through a bounds-checked `VersionedOut` (`query_adapter_info.rs:122-164`) so a short buffer
truncates rather than overflows; skipped fields are counted in `CapTrunc` (live value **0**).
`REQUIRED_DRIVER_CAPS_SIZE` = 540; `size_of::<DXGK_DRIVERCAPS>()` on the 26100 headers is 592, and
the driver deliberately accepts short (versioned) buffers.

| Field | Value | Site | D3D12 adequacy |
|---|---|---|---|
| `HighestAcceptableAddress` | `-1` | `:216` | fine |
| `MaxAllocationListSlotId` | `0xFFFF` | `:217-221` | fine |
| `ApertureSegmentCommitLimit` | 64 MiB | `:222-226` | ⚠ a *global* cap that reduces the OS-computed `SharedSystemMemory`. Not a blocker; a tuning item — **K3** |
| `SupportNonVGA` | 1 | `:228` | fine |
| `WDDMVersion` | `DXGKDDI_WDDMv2_1` | `:234` | **above the D3D12 floor** — WDDM 2.0 is the version that introduced D3D12 |
| `PreemptionCaps.Graphics/Compute` | `DMA_BUFFER_BOUNDARY` | `:236-251` | fine — §11 |
| `SupportPerEngineTDR` | 1 | `:252` | fine (one engine) |
| `PresentationCaps` | 0 | `:345,396` | `SupportKernelModeCommandBuffer` deliberately 0 (no GDI HW accel). Irrelevant to D3D12 |
| `FlipCaps` | `FlipOnVSyncMmIo` only — live `FlipCapV = 2` | `:389-397` | present-path only. ⛔ `FlipImmediateMmIo` must not return (defect 0ab) |
| `SchedulingCaps` | `MultiEngineAware \| PreemptionAware` | `:304-305,395,402` | correct for a software-scheduled, dxgkrnl-writes-the-fence adapter. `NativeGpuFence`=0, `HwQueuePacketCap`=0 |
| `MemoryManagementCaps` | `SectionBackedPrimary \| VirtualAddressingSupported \| GpuMmuSupported` (+ `CrossAdapterResource` only if the `CrossAdaptCaps` knob is set) | `:325-341,403` | ⚠ cross-adapter is **OFF** by default — §10 |
| `MaxQueuedFlipOnVSync` | 1 (knob `FlipQueueDepth`) | `:426-435` | present-path only |
| `SupportDirectFlip` | 0 | `:454` | deliberate; `=1` was an unbacked lie that stopped DWM compositing |
| `GpuEngineTopology.NbAsymetricProcessingNodes` | **1** | `:456-464` | §5 |
| everything else | zero-filled `:206` | | `SupportMultiPlaneOverlay`, `SupportSurpriseRemoval`, `HybridDiscrete`, … all 0 |

### 4.2 The others

- **`DXGK_WDDMDEVICECAPS`** (`:507-517`) — zero-filled, `WDDMVersion` only. Adequate.
- **`DXGK_GPUMMUCAPS`** (`gpummu.rs:127-148`) — flags word **0** (no `ReadOnlyMemorySupported`,
  `NoExecuteMemorySupported`, `CacheCoherentMemorySupported`, `LargePageSupported`,
  `DualPteSupported`; the module rule at `:122-126` is *"unknown stays unadvertised"*);
  `PageTableUpdateMode = GPU_PHYSICAL`; `VirtualAddressBitCount = 40`;
  `LeafPageTableSizeFor64KPagesInBytes = 4096`; `PageTableLevelCount = 4`.
  The D3D12 cap this feeds is `D3D12DDI_GPUVA_CAPS_0004.MaxGPUVirtualAddressBitsPerResource`
  (`d3d12umddi.h:250-257`), which the runtime validates: it rejects 0 and requires ≥ 40 bits for an
  FL 12.2 driver. **40 clears that bar exactly.**
- **`DXGK_PAGE_TABLE_LEVEL_DESC`** (`gpummu.rs:160-195`) — levels 0..3, index bits 9/9/9/1,
  segment 0, size and alignment 4096, internally consistent by `const _: () = assert!`. Adequate.

### 4.3 ⚠ The one query most likely to surprise D3D12

`DXGKQAITYPE_PHYSICALADAPTERCAPS` (0x0F) is **deliberately rejected**, and the comment at
`query_adapter_info.rs:78-84` records why: answering it (tested 2026-06-22) pulls dxgmms2 into
per-physical-adapter / per-execution-node setup that dereferences a null node structure the null
engine never provides — **bugcheck 0x3B, AV on null-base+0x210 in `dxgmms2+0x9775d`**. viogpu3d
leaves it unimplemented too. DXGI and D3D11 tolerate the rejection.

⚠ **UNVERIFIED: whether the D3D12 runtime tolerates it.** This is the highest-probability
D3D12-specific KMD surprise in the whole document. **Settling experiment:** once `OpenAdapter12`
returns something, call `D3D12CreateDevice` and take a `Microsoft-Windows-DxgKrnl` all-keywords ETW
slice around the create, then `tracerpt` → grep `AzureTriage`. If D3D12 requires it, the fix is not
"answer it" — it is to give the adapter a real execution-node structure first, which is a
substantially larger change and would need its own workstream.

---

## 5. Engines and nodes — settled, zero KMD work

**The D3D12 queue-creation DDI carries no engine node.**

```c
/* tmp/dx12/sdk/d3d12umddi.h:1450-1456 */
typedef struct D3D12DDIARG_CREATECOMMANDQUEUE_0001
{
    D3D12DDI_HCOMMANDQUEUE       hDrvCommandQueue;
    D3D12DDI_HRTCOMMANDQUEUE     hRTCommandQueue;
    D3D12DDI_COMMAND_QUEUE_FLAGS QueueFlags;      /* NONE|3D|COMPUTE|COPY|PAGING|VIDEO_* , :1435-1448 */
    UINT                         NodeMask;        /* the LDA adapter mask, NOT an engine ordinal */
} D3D12DDIARG_CREATECOMMANDQUEUE_0001;
```

The UMD then creates its own kernel context through `pfnCreateContextVirtualCb`
(`d3d12umddi.h:2562-2564`), whose argument *does* carry the ordinal:

```c
/* tmp/dx12/sdk/d3dumddi.h:3976-3984 */
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

**So the queue-class → engine-node mapping is entirely the UMD's choice** — that much is settled by
the headers. On a single-node adapter a D3D12 UMD maps DIRECT, COMPUTE and COPY all onto
`NodeOrdinal = 0`, which is what every integrated single-engine WDDM driver does.
⚠ **The other half is inference:** that the runtime *accepts* three queue classes on one node
without complaint, and that dxgkrnl does not synthesise engines, does not follow from the headers.
It is strongly implied by single-engine parts shipping D3D12 for a decade, but on this adapter it is
**UNVERIFIED** until G7 creates all three queue types — see U6.

Helios' node surface, for reference: `dxgkddi_get_node_metadata`
(`query_adapter_info.rs:1261-1287`) rejects `node_ordinal != 0` with `STATUS_INVALID_PARAMETER` and
reports `EngineType = DXGK_ENGINE_TYPE_3D`, `GpuMmuSupported = 1`, `IoMmuSupported = 0`;
`NbAsymetricProcessingNodes = 1` (`:456-464`); `QueryDependentEngineGroup` / `QueryEngineStatus` /
`ResetEngine` accept only `(0, 0)` (`scheduler.rs:64-66, 81-83, 104-106`).

**Residual, named:** with all three queue classes on node 0, a D3D12 app that overlaps a COPY queue
with a DIRECT queue gets serialization instead of parallelism *at the WDDM layer*. That is a
performance property of a one-engine GPU, not a correctness gap — and under the vkd3d engine the
real work is out-of-band anyway, so host-side parallelism is unaffected.

---

## 6. Hardware queues / GPU scheduling — not required, correctly refused

**D3D12 does not require hardware scheduling, at any Windows build.** WDDM 2.0 is the D3D12
prerequisite. The hardware-*queue* DDI surface (`CreateHwQueue`, `SubmitCommandToHwQueue`, …) is
**WDDM 2.2** per `dispmprt.h`'s own version blocks, and the user-facing
hardware-accelerated GPU scheduling feature with its Settings toggle is **WDDM 2.7 / Windows 10
2004** — seven versions after D3D12 shipped. Neither has ever been mandatory: Windows 11 24H2/25H2
still runs D3D12 on non-HWS adapters (WARP, older discrete parts, virtualized adapters).

Helios advertises none of it: `DXGK_VIDSCHCAPS.HwQueuePacketCap` = 0, `NativeGpuFence` = 0, and
`DXGKQAITYPE_USERMODESUBMISSION_CAPS` / `NATIVE_FENCE_CAPS` fall into the
`other => STATUS_NOT_SUPPORTED` arm.

`dxgkddi_create_hw_queue` refuses at the **first** statement and records `HwQRef`
(`scheduler.rs:180-187`). The doc comment at `:166-179` records why refusing at *create* rather
than at *submit* matters, verbatim:

> It used to hand back a magic-tagged `Box`ed queue and succeed, while `SubmitCommandToHwQueue`
> returned `STATUS_NOT_SUPPORTED` — the worst possible pairing, because the scheduler has already
> committed to the queue by the time the submission fails … Failing here means no queue handle
> exists, so a submission against one is unrepresentable.

**KMD work for D3D12: none.** The right posture on this adapter is to stay non-HWS, and the refusal
is already correctly shaped and counted. Note that vkd3d never touches `CreateHwQueue` at all.

---

## 7. Monitored fences — nothing beyond what is already there

`ID3D12Fence` maps to a WDDM monitored fence. On a non-HWS, non-native-fence adapter **dxgkrnl owns
the entire mechanism.** Microsoft's own words:

> **GPU signal** — If a GPU engine isn't capable of writing to a monitored fence using its virtual
> address, the UMD uses the *SignalSynchronizationObjectFromGpuCb* callback to queue a software
> signal packet to the GPU context.
>
> **GPU wait** — To wait on a monitored fence on a GPU engine, the UMD first needs to flush its
> pending command buffer then call *WaitForSynchronizationObjectFromGpuCb* … *Dxgkrnl* queues the
> dependency to its internal database … Command buffers submitted after the wait operation aren't
> scheduled for execution until the wait operation is satisfied.
>
> **CPU signal** — … *Dxgkrnl* updates the fence memory location with the signaled value.
>
> — `windows-driver-docs-research-only/windows-driver-docs-pr/display/context-monitoring.md`

Every one of those is a dxgkrnl/VidSch action. The miniport's only involvement is retiring DMA
packets, which Helios does via `DXGK_INTERRUPT_DMA_COMPLETED`
(`submit_command.rs:346-350`, `DmaCompleted.SubmissionFenceId = fence`). The only interrupt types
Helios ever raises are `DMA_COMPLETED` (`:346`), `CRTC_VSYNC` (`:372`) and `DMA_PREEMPTED` (`:390`).
`MONITORED_FENCE_SIGNALED` is never raised — correct, because Helios never writes fence memory.

| DDI | Required? | Why |
|---|---|---|
| `DxgkDdiSignalMonitoredFence` | **Optional** — WDDM 2.5 slot, above the declared 2.1 | the GPU-writes-the-fence-VA fast path; the software path is used instead |
| `CreateNativeFence` / `Destroy` / `Open` / `Close` / `SetNativeFenceLogBuffer` / `UpdateNativeFenceLogs` / `UpdateMonitoredValues` / `UpdateCurrentValuesFromCpu` | **Optional** — WDDM 3.1/3.2, gated on `DXGK_VIDSCHCAPS.NativeGpuFence` = 0 | native GPU fences are HWS stage 2, Windows 11 24H2 / WDDM 3.2 |
| `DXGKQAITYPE_NATIVE_FENCE_CAPS` | Optional | rejected `NOT_SUPPORTED` — the honest answer |

**Local proof it already works.** `tools/vehicle_flipwait_probe.c` queues `WAIT(F >= 1)` then
`SIGNAL(G = 5)` on a context and asserts G stays 0 until the CPU signals F. `ROADMAP.md:2616-2621`
records the result verbatim:

> **PROVES the primitive live on our software-scheduled adapter** (queued signal held behind an
> unsatisfied wait, drained ~10 ms after the CPU signal; **ZERO KMD changes**) and the topology:
> raw cross-device sync handles are REJECTED 0xC000000D — the fence must be NT-shared
> (`D3DKMTShareObjects`) and reopened via `OpenSyncObjectFromNtHandle2` on the device owning the
> waiting context.

⚠ **The one clause a D3D12 implementer must know** is that last one: **cross-device monitored-fence
handles must go through `D3DKMTShareObjects` + `OpenSyncObjectFromNtHandle2`**, never raw handles.
`ID3D12Fence` shared handles already work that way, so this is a "do not invent a shortcut" note.

`DXGK_VIDSCHCAPS.No64BitAtomics` = 0 — i.e. Helios claims 64-bit atomic fence updates. Correct: the
writes are dxgkrnl's CPU writes, which are atomic.

~~**KMD work for D3D12 fences: none.**~~

### ⛔⛔ CORRECTION 2026-08-06 — this section's CONCLUSION is refuted; its FACTS still stand

`D12-G8` rung 0 failed and was measured (`tmp/dx12/gates/G8-r0-settle/`): the application's
`ID3D12Fence` completes in **0.8–1.1 µs** against WARP's **561 µs**, while the surface goes from
**0/65536 exact at T+0** to **65536/65536 at +2000 ms** through the same live mapping. The GPU work
lands; nothing orders the fence behind it. See `DECISIONS.md` **D5a** and §14a.

**What was wrong, and why it survived review:**

1. ⛔ ***"Local proof it already works"* overstates its own evidence.** `tools/vehicle_flipwait_probe.c`
   issues `CreateSynchronizationObject2`, `WaitForSynchronizationObjectFromGpu`,
   `SignalSynchronizationObjectFromGpu/Cpu` — and **no `D3DKMTRender`, no `D3DKMTSubmitCommand`,
   no present**; its own header says *"with no rendering"*. It proves VidSch orders **software sync
   packets** FIFO on a Helios context. **The DMA-packet dependency — which is the entire question —
   it never exercises.** ⇒ the residual it left open (§7's `D12-G-fence`, whose step (3) is
   *"`D3DKMTSubmitCommand` an empty DMA buffer"*) was the load-bearing half, and it was never run.
2. ⛔ **The sentence this section is built on was mis-applied.** `context-monitoring.md`'s *"the UMD
   uses the `SignalSynchronizationObjectFromGpuCb` callback to queue a software signal packet"* is
   about a UMD signalling a fence **it created and holds the handle for** — which D3D11 does. A
   D3D12 `ID3D12Fence` is the *runtime's*, and the driver is handed no handle for it at all. See
   `DDI_REFERENCE.md` §10.4's correction block.
3. ⚠ Citation drift: §7 cites `ROADMAP.md:2616-2621` for the probe result; that passage is now at
   **`ROADMAP.md:2812-2818`**.

**What still stands, unchanged and load-bearing:** dxgkrnl owns the monitored-fence mechanism; the
miniport implements none of the fence DDIs; `MONITORED_FENCE_SIGNALED` is never raised (the only
three interrupt types are `DMA_COMPLETED`, `CRTC_VSYNC`, `DMA_PREEMPTED`); and **the miniport's only
involvement is retiring DMA packets.** That last clause is exactly why the conclusion inverted: the
mechanism is sound and this driver puts **nothing on the context for it to order behind**. The fix
is a real submission, not a new fence DDI — §14a.

**Residual unknown, and its probe.** What is proven is the *primitive*. What has not been observed
is a **D3D12-shaped** fence: the runtime creating a monitored fence, handing the driver its two GPU
VAs (`D3D12DDI_FENCE_PLACEMENT`, `d3d12umddi.h:1575-1598`), and reading an advancing value at
`FenceValueCPUVirtualAddress` (`d3dkmthk.h:1707`) after a queued software signal on this adapter.
**Settling experiment (`D12-G-fence`, ~half a day, no D3D12 code):** extend
`tools/vehicle_flipwait_probe.c` into a standalone `tools/` probe that (1) `D3DKMTCreateDevice` +
`D3DKMTCreateContextVirtual` on the Helios adapter, NodeOrdinal 0; (2)
`D3DKMTCreateSynchronizationObject2` with a monitored fence, capturing both VAs; (3)
`D3DKMTSubmitCommand` an empty DMA buffer then `D3DKMTSignalSynchronizationObjectFromGpu` for
value 1; (4) poll `*FenceValueCPUVirtualAddress` and `D3DKMTWaitForSynchronizationObjectFromCpu`.
**Pass:** the CPU-visible value reaches 1 with nothing writing the GPU VA. **Fail:** Helios needs a
monitored-fence notification path (`DXGKCB_NOTIFY_INTERRUPT` with
`DXGK_INTERRUPT_MONITORED_FENCE_SIGNALED`) before the DDI arm is possible — which would be the one
large KMD item this document does not currently list. Run in **session 1** via a cloned scheduled
task.

---

## 8. GPU virtual addressing — real addresses, honoured by nothing

**Who assigns the VA a D3D12 resource reports?** VidMm does, and it already does it today.

1. The UMD calls `D3DKMTReserveGpuVirtualAddress` / `MapGpuVirtualAddress` /
   `UpdateGpuVirtualAddress` (`d3dkmthk.h:5760-5763, 6026-6029`), or gets a VA back from
   `D3DKMTCreateAllocation` on a VA-capable context.
2. VidMm picks the address out of its own per-process GPU VA space, bounded by
   `DXGK_GPUMMUCAPS.VirtualAddressBitCount` = 40 (`gpummu.rs:142`).
3. VidMm materialises the mapping by pushing PTEs at the miniport as
   `DXGK_OPERATION_UPDATE_PAGE_TABLE` (= 11) through `DxgkDdiBuildPagingBuffer` — because
   `PageTableUpdateMode` is `GPU_PHYSICAL` (`gpummu.rs:141`), not `CPU_VIRTUAL`.
   *(`CPU_VIRTUAL` was proven fatal by KD: it dies inside
   `VIDMM_PAGE_TABLE_BASE::GetCpuVisibleAddress`, `gpummu.rs:27-35`.)*
4. `dxgkddi_build_paging_buffer` handles that op at `build_paging_buffer.rs:1329-1353`: it records
   the leaf mapping in `adapter.paging_pte_shadow` and harvests BAR placements, then returns
   `STATUS_SUCCESS`. It **never advances `pDmaBuffer`** — *"no hardware command is ever emitted"*
   (`:1306-1308`).
5. Nothing in the guest or on the host ever reads those PTEs. `gpummu.rs:1-14`, verbatim:

   > Helios has no guest GPU MMU: venus addresses host resources by opaque id and the **host GPU
   > owns the real MMU**, so the guest page tables are *decorative* — their content is never read
   > by any hardware.

**So the VAs are real, OS-allocated, non-overlapping addresses that are simply not honoured by
anything.** That is a UMD-layer concern, and under the chosen architecture it does not arise:

- **Under the vkd3d engine (both arms),** `ID3D12Resource::GetGPUVirtualAddress` returns
  `resource->res.va` (`vkd3d-proton-helios/libs/vkd3d/resource.c:2656-2663`), which is a **Vulkan
  buffer device address** obtained via `vkGetBufferDeviceAddress` and resolved natively by the host
  GPU. vkd3d's `va_map.c` is a *reverse* lookup (VA → resource), not an allocator. The decorative
  guest VA never enters the picture, and the UMD would return vkd3d's addresses from
  `pfnCheckResourceVirtualAddress` (`d3d12umddi.h:2476`).
- **Only if a from-scratch D3D12 translator were ever written** would the UMD have to maintain its
  own VA → venus-resource-id map and rewrite every root descriptor, indirect-argument buffer and
  raytracing acceleration-structure address before it reached the host. **That is a UMD cost in a
  design nobody is proposing; the KMD needs no change either way.**

⚠ **UNVERIFIED:** whether the D3D12 runtime and its debug layer accept GPU virtual addresses the
driver never obtained from the kernel. **Settling experiment:** report
`MaxGPUVirtualAddressBitsPerResource = 40`, return vkd3d's BDAs, and run a `D3D12HelloWorld` sample
with the D3D12 debug layer enabled, watching for the `MaxGPUVirtualAddressBitsPerResource` error
string that exists in `D3D12Core.dll` and for any GPU-VA validation break. If the debug layer only
tracks VA ranges for self-consistency, BDAs satisfy it.

The four page-table DDIs (`DescribePageTable`, `UpdatePageTable`, `UpdatePageDirectory`,
`MovePageDirectory`) are unset and are `PVOID`-typed reserved slots in this WDK; they belong to the
`CPU_VIRTUAL` mode Helios does not use. Consistent. `DXGK_OPERATION_MAP_MMU`/`UNMAP_MMU` reach
`PagingOperation::Other` and return `STATUS_SUCCESS`; they are IoMmu-mode operations and
`IoMmuSupported = 0`, so they should never arrive.

---

## 9. Residency and paging

**There is no `DxgkDdiOfferAllocations` / `ReclaimAllocations` slot in this WDK** — confirmed by
parsing the 187-name field list. So "the KMD does not implement them" is not a meaningful gap
statement.

What reaches the miniport is `DxgkDdiBuildPagingBuffer`. Helios handles six of the 23
`DXGK_OPERATION_*` values (`build_paging_buffer.rs:1195-1218`):

| Op | value | Helios |
|---|---:|---|
| `TRANSFER` | 0 | real — `bar_transfer` (`:1388`) |
| `FILL` | 1 | real — `bar_fill` (`:1389`) |
| `DISCARD_CONTENT` | 2 | real (`:1390`) |
| `VIRTUAL_TRANSFER` | 8 | real (`:1443`) |
| `VIRTUAL_FILL` | 9 | real (`:1402`) |
| `UPDATE_PAGE_TABLE` | 11 | shadow + harvest, `STATUS_SUCCESS` (`:1329-1353`) |
| everything else (incl. `MAP_APERTURE_SEGMENT`, `SIGNAL_MONITORED_FENCE`, `NOTIFY_RESIDENCY`, `INIT_CONTEXT_RESOURCE`, `FLUSH_TLB`) | — | `PagingOperation::Other` → `STATUS_SUCCESS` no-op (`:1215, 1456-1461`) |

**D3D12's residency model** (`MakeResident`/`Evict`, paging queues) is a UMD-level callback surface
(`pfnMakeResidentCb`/`pfnEvictCb` on `D3D12DDI_CORELAYER_DEVICECALLBACKS_*`) that VidMm turns into
paging operations. Nothing extra reaches the miniport. `DXGK_CONTEXTINFO_CAPS.DriverManagesResidency`
(`d3dkmddi.h:1550-1563`) — the flag by which a driver takes residency over — is **not** set; Helios
never writes `ContextInfo.Caps` at all (`device.rs:414-428`). That is the correct default.

⚠ **And that default is now load-bearing, not merely convenient (2026-08-05).** `ResourceHeaps.md`
(DirectX-Specs @ `2bd58ca5`) states that publishing **non-NULL** `pfnMakeResident` / `pfnEvict` takes
on two coupled obligations: the driver must **stop creating its DXGK allocations resident at creation
time**, and the KMD must set `DXGK_CONTEXTINFO_CAPS::DriverManagesResidency` on **every** context.

⇒ **The UMD-side and KMD-side choices are one decision, not two.** A `helios_umd12` that publishes real
residency entry points while `kmd_render` leaves `ContextInfo.Caps` zeroed would be an incoherent pair —
and it is exactly the kind of split-brain the "advertise only what is backed" rule exists to prevent.
⛔ So the residency plan is: keep `ContextInfo.Caps` unwritten in the KMD **and** keep the UMD's
residency entry points as honest thin forwards that claim nothing (`S_OK` / no-op), together, in the
same review. If residency is ever taken over, both halves move in one commit.

⚠ **Evidence class:** this contradicts a repo *claim* (that residency is purely VidMm's job and the
driver does nothing), not a measurement — `D12-G5`'s spy never exercised `MakeResident`/`Evict`, so
there is no Helios measurement of residency at all. The conclusion "VidMm owns it" is unchanged; what
changes is that the *reason* is now a choice Helios is making, with a stated cost if it is ever
reversed.

⚠ The one trap is the `E_PENDING` + paging-fence protocol on `pfnMakeResident`
(`D3D12DDIARG_MAKERESIDENT_0001.pPagingFenceValue` / `WaitMask`, `d3d12umddi.h:494-514`): it must
not be faked. A thin honest forward, or `S_OK`, is fine; a fabricated fence value is not.

**Residency budgets.** D3D12 apps read `IDXGIAdapter3::QueryVideoMemoryInfo`, which requires the
`ApplicationTarget` bit in **`DXGK_SEGMENTFLAGS`** (`tmp/dx12/sdk/d3dkmddi.h:2582`, written through
`DXGK_SEGMENTDESCRIPTOR4.Flags` at `:2722`) on segments the driver wants included in budgeting.
⚠ Not `DXGK_SEGMENTFLAGS2` — that struct has exactly four bits (`Aperture`,
`PopulatedFromSystemMemory`, `SystemMemoryReservedByBios`, `CpuVisible`, `:2650-2664`) and none of
them is a budgeting bit. ⚠ The header marks `ApplicationTarget` *"Deprecated, replaced by
LocalBudgetGroup and NonLocalBudgetGroup flags"* (`:2582`, `:2586-2587`) — which is why Helios sets
`LocalBudgetGroup` alongside it, and why a future cleanup should drop the deprecated bit rather than
the pair.
Helios sets it, and `LocalBudgetGroup` with it, on the BAR segment when `vidmm_vram_size(knobs)` is
`Some` (`query_adapter_info.rs:1002-1010, 814-818`); `VIDMM_VRAM_MB_DEFAULT = 4096`
(`kmd_render/src/adapter/mod.rs:117`), inside the legal `256..=65536` range. **So
`ApplicationTarget` is ON by default and D3D12 sees a ~4 GiB local budget group** (live key:
`VidVram = 4096`).

**Segment topology and the aperture-LAST invariant.** The live shape is `[Aperture(id 1),
Bar(id 2)]` (`segment_table.rs:107-129`, `bar_segment.rs:167-180`) with
`PagingBufferSegmentId = APERTURE_SEGMENT_ID = 1`. The rule, from the module's own header
(`segment_table.rs:1-12`):

> **A `SupportsCpuHostAperture` segment must be the LAST reported segment.** ETW-proven 2026-07-05:
> any segment reported after a cpu-host segment fails AddAdapter with *"Invalid flags specified for
> segment #2"* and the device lands in Code 43.

It is enforced by construction in `SegmentTable::new` with a counted refusal, `SegRule` (live
`SegRule = 0`, `SegCntMis = 0`). ⚠ `SegmentTable::MAX = 2`. If D3D12 ever wants a distinct heap
segment or a non-CPU-visible local segment, the only legal third topology is
`[Aperture, <new non-cpu-host segment>, Bar]` **and `MAX` must be raised** — anything placed *after*
the cpu-host BAR segment is Code 43.

---

## 10. Allocations: D3D12 shapes vs `create_allocation.rs`

| D3D12 shape | What reaches the miniport | Helios today |
|---|---|---|
| **Committed resource** | one `DxgkDdiCreateAllocation` with UMD private data | ✅ **if** the private data is `HeliosWddmAllocPrivate` — §2c |
| **Heap + placed resources** (`CreatePlacedResource`) | one allocation for the heap; placed resources are pure VA mappings (`UPDATE_PAGE_TABLE`) | ✅ structurally — the heap is just another allocation, the VA mappings are no-ops. Semantics live in the UMD/engine |
| **Reserved (tiled) resources** | VA reservation + `UpdateGpuVirtualAddress` tile remaps | ⚠ structurally accepted, **semantically unmodelled** — the guest page tables are decorative, so a tile remap has no effect anywhere. Correct sparse behaviour must come from the layer that talks to venus (it does: venus supports sparse binding end to end — `docs/dx12/SUBSTRATE.md` §6). `DXGK_GPUMMUCAPS` advertises none of the sparse-adjacent bits, which is the honest state |
| **Cross-adapter** (`D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER`) | needs `DXGK_VIDMMCAPS.CrossAdapterResource` | ❌ **OFF by default.** `cross_adapter: read_config_dword(knobs::CROSS_ADAPT_CAPS, 0) != 0` (`adapter/mod.rs:247`); `CrossAdaptCaps` is absent from the live service key. A knob flip + `pnputil /restart-device` turns it on; the cross-adapter pitch logic already exists (`create_allocation.rs:1438` → `kmd_logic/src/lib.rs:120`) |
| **Shared heaps / NT-shared resources** | standard allocation + `D3DKMTShareObjects` | ✅ the open path exists (`dxgkddi_open_allocation`, `create_allocation.rs:2860-3060`) with a liveness gate that fails a dead venus resid loudly (`:2917-2936`) |
| **GPU upload heaps** (`D3D12_HEAP_TYPE_GPU_UPLOAD`) | a CPU-visible device-local allocation | ⚠ **UNVERIFIED, but not a KMD question.** The BAR segment is CPU-host-aperture-mapped, which is structurally right. The app-visible cap is `D3D12_FEATURE_DATA_D3D12_OPTIONS16.GPUUploadHeapSupported` (`tmp/dx12/sdk/d3d12.h:2880-2884`), and **this WDK's `d3d12umddi.h` exposes no corresponding DDI cap at all** — so it is runtime/UMD-derived and the question belongs to vkd3d and `umd12`, not to `kmd_render`. No `DXGK_SEGMENTFLAGS` bit gates it |
| **Query heaps / command allocators** | ordinary allocations | ✅ same as committed |
| **Standard allocations** (`GetStandardAllocationDriverData`) | the KMD authors the private data itself | ✅ `create_allocation.rs:3146-3180`, `PRIV_SIZE = 96` |

**Bounded-table headroom** for a D3D12 app that allocates more objects than a D3D11 one:
`MAX_BLOBS = 8192`, `MAX_RESOURCES = 16384`, `MAX_CONTEXTS = 1024`
(`kmd_render/src/virtio/gpu/mod.rs:125,129,131`), `MAX_MAPPINGS = 8192` (`kmd_render/src/mapping.rs:41`).
These are adapter-global and already sized for a DOOM level load. ⚠ A title with tens of thousands
of *placed* resources on a handful of heaps stays well inside them (the heaps are the allocations);
a title that committed-allocates per resource would not. Settling measurement: run a D3D12 sample
and read `HELIOS_ESCAPE_QUERY_STATS` (`escape.rs:966`) / the blob-table counters via the
`tools/read-vehicle-counters.ps1` shape.

---

## 11. Preemption and TDR

**D3D12 adds no requirement beyond WDDM 1.2's.** `gpu-preemption.md` requires: compile at
`>= DXGKDDI_INTERFACE_VERSION_WIN8`; set `PreemptionAware` and `MultiEngineAware`; report a
granularity; support `FlipOnVSyncMmIo`. Helios does all four
(`query_adapter_info.rs:236-253, 297, 389, 395`).

What Helios does:

- `DxgkDdiPreemptCommand` (`submit_command.rs:905-931`) drops every pending WDDM fence under the
  notification lock and acknowledges with a `DMA_PREEMPTED` packet (`AbandonOutcome::Preempted`).
- `DxgkDdiResetFromTimeout` (`:936-963`) drops pending fences silently (dxgkrnl owns post-reset
  state), purges present streams, and reports transport failure through `StRing` (live `StRing = 0`).
- `DxgkDdiRestartFromTimeout` (`:970-976`) returns `STATUS_SUCCESS`.
- `DxgkDdiResetEngine` (`scheduler.rs:95-123`) reports `LastAbortedFenceId` and purges streams.
- `SupportPerEngineTDR = 1`; `ABANDONED_FENCES` (`submit_command.rs:806`) counts fences discarded by
  any of the three.

**Gap for D3D12: none.** ⚠ The known residual is the Xid-109 / host-context-death class — a host+ICD
stability item that D3D12 inherits unchanged, not a D3D12-specific KMD gap.

---

## 12. The D3DKMT ↔ miniport-DDI surface

Which surface is in play depends on the arm:

| Arm | Who touches D3DKMT | Size |
|---|---|---|
| **DDI arm** | The D3D12 runtime owns every kernel object and hands the UMD a `D3DDDI_DEVICECALLBACKS` table; the UMD calls *through* it, never `D3DKMT*` directly | **65** callbacks in `D3DDDI_DEVICECALLBACKS` + the D3D12-specific KM callbacks on `D3D12DDI_CORELAYER_DEVICECALLBACKS_*` |
| **Phase 0** | **13** distinct `D3DKMT*` entry points in one file, plus `D3DKMTShareObjects` in `libs/vkd3d/device.c:7633` and `:7704` — 14 in all, **none for rendering, submission, allocation, or hot-path fences** | `vkd3d-proton-helios/libs/vkd3d/d3dkmt.c`, 449 lines (declarations at `include/private/vkd3d_d3dkmt.h:352-366`) |

The mapping that matters, entry point → miniport DDI → Helios status:

| D3DKMT entry point | Miniport DDI it reaches | `kmd_render` |
|---|---|---|
| `CreateDevice` / `DestroyDevice` | `DxgkDdiCreateDevice` / `DestroyDevice` | ✅ `lib.rs` |
| `CreateContext` / `CreateContextVirtual` / `DestroyContext` | `DxgkDdiCreateContext` / `DestroyContext` | ✅ `device.rs:389-431` — ⚠ ignores `NodeOrdinal`/`EngineAffinity`/`Flags` (K1) |
| `CreateHwQueue` / `SubmitCommandToHwQueue` | `DxgkDdiCreateHwQueue` / `SubmitCommandToHwQueue` | ⛔ refused `STATUS_NOT_SUPPORTED`, counted `HwQRef` — correct (§6) |
| `CreateAllocation2` / `DestroyAllocation2` | `DxgkDdiCreateAllocation` / `DestroyAllocation` | ✅ — ⚠ private-data gate (§2c) |
| `OpenResource*` / `QueryResourceInfo*` | `DxgkDdiOpenAllocation` / `DescribeAllocation` | ✅ `create_allocation.rs:2860-3060` |
| `Lock2` / `Unlock2` | `DxgkDdiMapCpuHostAperture` / `UnmapCpuHostAperture` (+ `Lock`/`Unlock`) | ✅ `cpu_host_aperture.rs` |
| `CreatePagingQueue` / `MakeResident` / `Evict` | `DxgkDdiBuildPagingBuffer` + `SubmitCommand` | ✅ §9 |
| `ReserveGpuVirtualAddress` / `MapGpuVirtualAddress` / `UpdateGpuVirtualAddress` / `FreeGpuVirtualAddress` | `BuildPagingBuffer(UPDATE_PAGE_TABLE)` | ✅ (decorative, §8) |
| `SubmitCommand` / `Render` | `DxgkDdiSubmitCommandVirtual` / `SubmitCommand` / `Render`+`Patch` | ✅ `submit_command.rs:725-799` |
| `CreateSynchronizationObject2` / `Signal*` / `Wait*` / `OpenSyncObjectFromNtHandle2` | none — dxgkrnl-internal on this adapter; the miniport only retires DMA packets | ✅ §7 |
| `QueryVideoMemoryInfo` | segment reporting + `ApplicationTarget` | ✅ §9 |
| `QueryAdapterInfo` | `DxgkDdiQueryAdapterInfo` | ✅ 15 types, §4 |
| `Escape` | `DxgkDdiEscape` | ✅ `escape.rs` — §13 |

**Proof by existence:** the Venus ICD already drives this whole surface. It opens the adapter by
LUID, creates a device and a virtual context, allocates through `CreateAllocation2` with
`HeliosWddmAllocPrivate`, submits through `Escape`, and its fences ride the existing chain. That is
the code path a D3D12 client uses under Phase 0, unchanged.

---

## 13. Escapes

`kmd_render/src/ddi/escape.rs` carries the existing verb set (venus submit, stats query, present
stream registration, counter reads). ⚠ **Four rules any new escape verb inherits**, each paid for:

1. **PASSIVE only.** No pageable code or `diag::record` (registry writes) above PASSIVE; IRQL-gate
   anything that round-trips.
2. **`HardwareAccess = 0`** on every escape — the flip-kwait deadlock class.
3. **Ownership derived from `hDevice`**, with the NULL-is-forgeable guard.
4. **Per-arm size validation** (never a max-union) with a **named counter for every refusal** — the
   RenderGdi ~48 % drop bug is the record of what a max-union costs.

**Would D3D12 need a new escape?** Not for the DDI arm and not for Phase 0 rendering. The one place
it is technically justified is **cross-API D3D12↔D3D11 resource sharing**: vkd3d publishes shared
resource descriptors through a **Wine-private** escape,
`D3DKMT_ESCAPE_UPDATE_RESOURCE_WINE = 0x80000000`
(`vkd3d-proton-helios/include/private/vkd3d_d3dkmt.h:119-122`), whose return value it **ignores**
(`d3dkmt.c:195`). ⚠ **UNVERIFIED:** the expected consequence is that the escape type is
unrecognised on Helios, so the resource silently has no runtime descriptor and a later
`OpenSharedHandle` fails with `E_INVALIDARG` — but nobody has observed it, and
`research/R6-d3dkmt-surface.md` §4.1 reasons that `DXGKARG_ESCAPE` may not carry the type through to
the miniport at all. **Settling experiment:** watch whether DXVK's existing `SET_PRESENT_RECT_WINE`
escape reaches `dxgkddi_escape` on this adapter — the escape verb counters answer it with no new
code. Supplying a Helios
`DRIVERPRIVATE` verb would be a two-submodule change. ⛔ **Do not scope it before a first
milestone** — nothing in single-process D3D12 needs it.

---

## 14. The work list

### Phase 0 — empty

Three *conditional* items, none required for a first frame:

| # | Item | Trigger | Size | Risk to D3D11 |
|---|---|---|---|---|
| C1 | Flip `CrossAdaptCaps` on | only if a D3D12 sample needs a cross-adapter swapchain | S (registry knob, no rebuild) | Low, but it changes the advertised cap surface (`MemoryManagementCaps`, diag `0x01D4`) — re-run the boot gate |
| C2 | Raise `SegmentTable::MAX` and add a segment | only if D3D12 needs a distinct heap segment | M | **High** — the cpu-host-must-be-LAST rule is Code-43 territory |
| C3 | Revisit `ApertureSegmentCommitLimit` (64 MiB) | only if D3D12 residency budgets read too small | S (one const) | Medium — it is an advertised capability; needs a measurement first |

### ⛔⛔ DDI arm — REWRITTEN 2026-08-06. The three items below are HYGIENE; the real list is §14a

The "three items, none required for the first triangle" framing was **wrong**, and it was wrong for
a reason worth stating: it was derived from §7, whose conclusion (*"KMD work for D3D12 fences:
none"*) rested on a probe that never issued a DMA packet. `D12-G8` rung 0 then failed, was measured,
and the cause is a **missing kernel submission** — see §7's correction block and `DECISIONS.md`
**D5a**. K1/K2/K3 remain valid and remain optional; they are simply not the D3D12 work.

⇒ **The live work list is §14a.** K1/K2/K3 are retained below unchanged.

| # | Item | Why | Evidence | Size | First triangle? | Risk to the working D3D11 desktop |
|---|---|---|---|---|:--:|---|
| **K1** | Validate `NodeOrdinal` / `EngineAffinity` in `DxgkDdiCreateContext` and count refusals (`CtxNode`) | A context for a node that does not exist is accepted **silently** today. `DxgkDdiCreateHwContext` already checks (`scheduler.rs:135-137`). CLAUDE.md rule 2: every skipped/refused path gets a named counter | `device.rs:389-431` (no check) vs `scheduler.rs:135-137` (checked) | **S** | No | **Low** — but it is a *new refusal on a live path*. ⚠ Ship the counter first, the refusal second, once evidence shows today's callers always pass 0 |
| **K2** | Set `ContextInfo.Caps.NoPatchingRequired = 1` and shrink `AllocationListSize`/`PatchLocationListSize` for contexts created with `DXGK_CREATECONTEXTFLAGS.VirtualAddressing` | The documented shape for a GPU-VA context: *"no patch location list … and only a very small allocation list (16 entries)"*. Helios asks 256+256 on **every** context and no-ops `DxgkDdiPatch` | `device.rs:414-428`; `d3dkmddi.h:1550-1580`; `submit_command.rs:1421-1430` | **S** | No | **Medium** — changes what dxgkrnl allocates per context for *every* client including DWM, and the allocation list is how the Present path receives its surfaces. ⚠ Do it last, behind a knob, with a paired GT1/GT2 measurement |
| **K3** | Revisit `ApertureSegmentCommitLimit` | as C3 | `query_adapter_info.rs:592-597` | **S** | No | Medium |

### ⛔ Explicitly NOT on the list, with the reason

- **More engine nodes** — the queue→node mapping is a UMD choice (§5).
- **Hardware queues / HWS** — not required by D3D12 at any Windows build (§6).
- **`DxgkDdiSignalMonitoredFence` / native fences** — optional, and the software path is proven live
  (§7).
- **Real GPU page tables** — the VAs are OS-assigned and already consistent; under the vkd3d engine
  the app-visible VA is a Vulkan device address the host resolves (§8).
- **Residency DDIs** — there are none to implement in this WDK (§9).
- **Any of the 31 reachable-unset slots** — none is class (c) (§3.2).

---

## 14a. ⭐ THE LIVE WORK LIST — the D3D12 kernel submission, fences and present

**Written 2026-08-06, after `D12-G8` rung 0 was measured and re-attributed.** Owner decision
(`DECISIONS.md` **D5a**): *"stop gaps are not acceptable … doesn't matter if its complex or if
changes are needed to be done in KMD."* There is no stopgap arm on this list, by instruction.

### 14a.0 The one sentence

The GPU work lands; nothing orders the application's `ID3D12Fence` behind it, because
`pfnExecuteCommandLists` makes **no kernel submission** and the D3D12 runtime's only lever on a
driver is *what dxgkrnl orders its monitored-fence signal behind* — the DMA packets on the queue's
WDDM context. Measured: Helios `WaitForSingleObject` returns in **0.8–1.1 µs** against WARP's
**561 µs**; the surface is **0/65536 exact at T+0 and 65536/65536 at +2000 ms**
(`tmp/dx12/gates/G8-r0-settle/`). Present sits on top of the same context and the same callback, so
the two are one piece of work.

### 14a.1 ⛔ Two unknowns gate everything, and they are separable in ONE experiment

Do not write anything below the experiment until the experiment has run.

| id | Question | Why it decides the design |
|---|---|---|
| **UV1** | Does dxgkrnl release the runtime's queued monitored-fence signal behind **our** DMA packets? | If no, no amount of submission helps and the design needs a different lever entirely. Doc support is good (`context-monitoring.md:35,47`); **local proof is absent** — §7's cited `tools/vehicle_flipwait_probe.c` issues no `D3DKMTRender` and no `D3DKMTSubmitCommand`, so it proves sync-packet-vs-sync-packet ordering only. |
| **UV3** | Does vkd3d's venus work retire at **host GPU completion** or at **decode**? | `RetireDomain::IncludingGpu` only means GPU completion for work on `ring_idx >= 1`. The ICD says outright: *"the synchronous SUBMIT_VENUS path does not yet propagate `batch->ring_idx` … per-ring async fencing is a later refinement"* (`icd/mesa/src/virtio/vulkan/vn_renderer_helios.c:3969-3973`). If D3D12 submits land on ring 0, a D3D12 fence gated `IncludingGpu` still reports **decode**, and it lies even with a perfect packet. |

**The experiment, in three readings.** Land the UMD's bare `pfnRenderCb` (K-F1, below — zero KMD
change), then read the probe's own `WaitForSingleObject signalled in N us` against the measured
0.8–1.1 µs baseline:

| reading | conclusion |
|---|---|
| N → real GPU time, no hold | **UV1 ✓ and UV3 ✓.** The design stands; everything after K-F5 is performance. |
| N flat; with `WddmHoldMs=100` **scoped to that context**, N → ~100 ms | **UV1 ✓, UV3 ✗.** The packet works, the venus retirement domain is the bug. Fix the ring, not the submission. |
| N flat under both | **UV1 ✗.** Say so loudly and stop — none of K-F3..K-F9 is the answer. |

~~⭐ **UV3 is separately pre-checkable with ZERO code**: read `RING_SUBMIT_COUNT` /
`RING_COMPLETE_COUNT` (`kmd_render/src/virtio/gpu/mod.rs:3502-3504`, `:4027-4029`) before and after
a D3D12 run. Do this first; it costs one run.~~

#### ⛔⛔ CORRECTION 2026-08-06 — UV3 IS ANSWERED (✗) FROM SOURCE, AND THE TABLE ABOVE IS UNSOUND

**UV3: ✗, and worse than "decode" — for a D3D12 frame this driver usually sees no wire fence at
all.** Answered without a run, and the two things that made it look unanswerable were both wrong.

1. ⛔ **UV3's own cited evidence is STALE IN BOTH HALVES.** `vn_renderer_helios.c:3969-3973` says
   `SUBMIT_VENUS` *"does not yet propagate `batch->ring_idx`"* and is *"synchronous"*. It propagates
   it at **`:1702`** (`hdr.ring_idx = ring_idx`), and the function's own header at **`:1672-1678`**
   says it is **ASYNC** and *"returns at QUEUE time"*. UV3's conclusion happens to be right, for a
   reason its premise never mentions.
2. ⭐ **The command stream never touches virtio.** `vn_QueueSubmit2` writes the cs into the shared
   venus ring (`vn_ring.c:630-636`) and stores the tail; there is no `SUBMIT_3D`. The only virtio
   submission a frame can produce is the `vkNotifyRingMESA` doorbell, hardcoded `.ring_idx = 0`
   (`vn_renderer_util.h:26-35`).
3. ⛔⛔ **And that doorbell is usually not sent.** It goes only when the host ring advertises IDLE,
   and then only past a 1 ms limiter (`vn_ring.c:673-690`, `VN_RING_IDLE_TIMEOUT_NS` at `:22`). A
   ring busier than 1 ms — every D3D12 frame — emits nothing. So `next_wire_fence` is **frozen**,
   every entry below it has already retired, and `async_retired_up_to` returns true instantly.
   ⇒ **That is the measured 0.8–1.1 µs.** It is an empty watermark, not a fence bug.
4. ⛔ **A ring-0 wire fence is not even a venus decode fence on this host.** The KMD sets
   `VIRTIO_GPU_FLAG_INFO_RING_IDX` only for `ring_idx != 0` (`gpu/mod.rs:3482`), and QEMU routes a
   fence without that flag to the legacy `virgl_renderer_create_fence`, which ignores `ctx_id`
   entirely (`qemu-helios/hw/display/virtio-gpu-virgl.c:1167-1186`). SUSPECTED beyond that boundary
   (virglrenderer is not checked out): it becomes an empty `glFenceSync` in vrend's GL context 0.
5. ⛔ **The only `ring_idx >= 1` producer on Windows is `vn_signal_win32_external_semaphore`**
   (`vn_queue.c:1714-1724`, called at `:1986-1994`), gated on `payload->win32_sync` — non-NULL only
   for an exported/imported OPAQUE_WIN32 semaphore. vkd3d signals one *internal* timeline semaphore
   per submit (`command.c:24482-24485`) and a non-shared `ID3D12Fence` has no Vulkan semaphore at
   all (`command.c:1728-1786`, `:23785-23818`). It never asks. ⇒ **DXVK's present path does produce
   this traffic (~one per present, which is how D3D11 gets a truthful boundary); vkd3d produces
   none.**

**⛔ THEREFORE THE THREE-READING TABLE ABOVE MUST NOT BE USED.** Its bottom row — *"N flat under
both ⇒ UV1 ✗"* — is unsound: a flat reading on the **unheld** arm is fully explained by (3), with no
bearing on whether dxgkrnl would have ordered anything. The bare `pfnRenderCb` (K-F1) therefore
settles **the plumbing only** — that dxgkrnl accepts the callback on a D3D12 queue's *legacy*
context, that `RENDER_COUNT` moves (§14a.4 item 3's free settlement of **P7**), that the three
windows come back, and that nothing bugchecks. **UV1's only clean test is a deliberate KMD-side
hold** (`WddmHoldMs`, K-F0), scoped by the presence of `HeliosD3D12SubmitCmd`, which is why K-F0 is
no longer conditional on K-F1's reading.

⛔ **And the "ZERO code" pre-check above cannot answer UV3 either**: `SCANOUT_RING_IDX = 1`
(`gpu/mod.rs:386`), so `RING_SUBMIT_COUNT` is dominated by **this driver's own** scanout and BLT
copies — three internal producers (`gpu/mod.rs:3375`, `:3442`, and `ctrl.rs:1541-1547` through the
generic enqueue), not two. Nor were those counters readable at all: they appeared only in the
`'HDBG'` `DxgkDdiCollectDbgInfo` report, i.e. only after provoking a TDR. Both are now mirrored
(`RngSub`/`RngCmp`) alongside a guest-originated split (`EscSub`/`EscSubRing`) counted at the escape
wrapper — attribution, not a subtraction, because a subtraction goes stale the next time a fourth
internal producer appears.

### 14a.2 The fence/completion bridge

⭐ **The producer half needs no vkd3d fork patch.** `vkd3d_acquire_vk_queue` /
`vkd3d_release_vk_queue` are upstream public interop API (`include/vkd3d.h:120-121`), and
`d3d12_command_queue_acquire_serialized` (`libs/vkd3d/command.c:25202-25218`) pushes a
`VKD3D_SUBMISSION_DRAIN` marker and waits until the worker has **submitted** everything enqueued —
a CPU-side wait for `vkQueueSubmit`, **not** for GPU completion, so it costs no CPU/GPU overlap.
This is the same discipline `HeliosWaitFrameSubmitted` gives the D3D11 present path.

| # | Item | Where | Size | Class |
|---|---|---|---|---|
| **FB-1** | Latch the three context windows in `QueueState` and re-latch them after every `pfnRenderCb`. Today they are logged and **dropped on purpose** (`umd12/src/forward12/queue.rs:920-940`). ⚠ **Shared by both `pfnRenderCb` users** — the fence carrier and the present identity. Do it once. Copy `umd/src/forward/present.rs:868-897` and `umd_common/src/window.rs`, which already documents the D3D12 case. | `umd12` | S | **[C]** |
| **K-F1** | The submission: drain (`vkd3d_acquire_vk_queue`) → `pfnRenderCb` on `QueueState::h_context` → re-latch. ⛔ **"No boundary record" is SUPERSEDED** — it carries `HeliosD3D12SubmitCmd` from the start, with `gpu_wire_fence = 0` until ICD-1 lands, because the record's presence is what scopes K-F0 and a second payload shape would be one more thing to keep in sync. ⭐ **Zero KMD change.** ⛔ **But it does NOT make `ID3D12Fence` truthful on its own, and it is not the UV1 test** — with a zero fence it takes the fall-through at `gpu/mod.rs:5641` (`watermark = next_wire_fence`), which §14a.1's correction shows is **already satisfied** during a D3D12 frame. What it settles is the **plumbing**: dxgkrnl accepts `pfnRenderCb` on a legacy D3D12 context, `RENDER_COUNT` moves (**P7**, free), the windows come back, nothing bugchecks. | `umd12` | S | **[C]** |
| **K-F0** | ⛔ **NO LONGER CONDITIONAL** (was *"only if K-F1's reading is flat"*): per §14a.1's correction, K-F1's unheld reading cannot answer **UV1** at all, so the hold is the *only* clean test and lands in the same KMD deploy. `WddmHoldMs` + a fourth block arm in `take_one_ready_wddm` (`gpu/mod.rs:5735-5752`, three already, exactly one counter per blocked look) + `WfBHold`, released by the existing 60 Hz DISPATCH heartbeat (`adapter/kobj.rs:461-540`) with one added `request_wddm_completion_dpc`. ⛔ **Scoped by the presence of `HeliosD3D12SubmitCmd`** — carried as a flag on `WddmPending` — or the adapter-global head-of-line FIFO stalls the whole desktop, DWM included. N ≪ TdrDelay, **clamped in code**: an operator typo must not be a TDR. **Reading:** N grows by ~the hold ⇒ **UV1 ✓**; N stays at the 0.8–1.1 µs baseline ⇒ **UV1 ✗**, and none of this design is the answer. | `kmd_render` | S | **[E]** |
| **K-F2** | ⛔ **A LIVE DEFECT ON THE SHIPPING PRESENT PATH, independent of D3D12 — but ⛔⛔ NOT the fix this row prescribed until 2026-08-06.** `present_stream_marker_boundary` (`gpu/mod.rs:4721-4744`) validates `ctx_id`/`value != 0`/`cookie`/`creator_process` and slot liveness, and bounds `value` in **no** way, so a guest writing an absurd `present_value` gets a *live* boundary `present_stream_slot_ready` (`:945-953`) can never satisfy; `wddm_pending` is an **adapter-global head-of-line FIFO** (`take_one_ready_wddm:5718-5765`, never bypassed), so it blocks every context including DWM's — bounded only by the FIFO's own 256-entry overflow escape (`:5664-5685`, `WDDM_PENDING_OVERFLOWS`, which then *drops* fences) or TDR, whichever lands first. ⛔⛔ **Copying the tag-path comparison (`value <= slot.submitted_value`) is REFUTED**: the tag path (`prepare:4770`, `commit:4790`) is the PRODUCER advancing the stream and requires the new value to be strictly AHEAD; the marker is a CONSUMER predicate, and on the shipping default the marker is delivered **before** the frame's `vkQueueSubmit` **by design**. `UmdAsyncPresentStream` is absent = ON (`umd/src/knobs.rs:129`), and `async_stream_eligible` **skips** `HeliosWaitFrameSubmitted` (`umd/src/forward/present.rs:1479-1528`) *because* the marker is what carries the dependency; the value is minted on the app thread (`umd/bridge/dxvk_bridge.cpp:1316`) while the tag that sets `submitted_value` rides DXVK's submission thread (`HeliosSignalPresentFence` is `EmitCs` only — `d3d11_context_imm.cpp:1150-1162` → `vn_queue.c:1994` → `:1736-1744`). So `value == submitted_value + 1` is the steady state, the check would refuse ~every legitimate frame, and each refusal falls back to `wire_fence_watermark()` (`adapter/scanout.rs:256-270`) — which does **not** cover the unsubmitted frame. With the UMD's gate skipped too, that is the **0ab-B stale/black-frame class returning**, plus `PresentWmk` silently demoted: a correctness regression, not a hardening. ⇒ **the guard is consumer-side liveness** — bound how long the FIFO head may block on a stream boundary, then rebase to the legacy watermark, loudly counted (K-F0's fourth-arm plumbing in `take_one_ready_wddm`), which also covers "submitted but never retired". An acceptance-side lookahead bound cannot stand alone: legitimate lookahead reaches DXVK's `MaxNumQueuedCommandBuffers = 32` (`dxvk-helios/src/dxvk/dxvk_limits.h:17`), and a forged value whose process then stops presenting is unsatisfiable at any bound. Its own commit; its own counter (**not** `PRESENT_STREAM_REJECTS`, which every other refusal already shares). | `kmd_render` | S | **[C]** |
| **K-F5** | Counters for the new arm, atomics only (DISPATCH), mirrored from the PASSIVE site `record_present_handoff_telemetry` (`submit_command.rs:108-181`). The `PmHit`/`PwExact`/`WfB*` family is the template. | `kmd_render` | XS | **[C]** |
| **ICD-1** | ⛔⛔ **REPLACED 2026-08-06, not relocated.** ~~`helios_venus_last_submitted_wire_fence(VkDevice, …)` reading a new `last_wire_fence_id` stored at `vn_renderer_helios.c:1739`~~ would return **whatever `SUBMIT_VENUS` happened last, process-wide, on any thread** — and for a vkd3d workload every submission that reaches that line is `ring_idx == 0` (§14a.1's correction): the ring doorbell, `vn_ring_submit_roundtrip`, ring create/destroy, the wait-alloc ordering batch. The KMD would then take it as a `gpu_completion_fence` and gate on `watermark = id + 1` (`gpu/mod.rs:5606-5610`) — **a fence that resolves at decode at best, and on this host at an empty `glFenceSync` in QEMU's GL context 0.** It would make the D3D12 fence *look* ordered while ordering nothing, and nondeterministically, since it names another thread's submit. ⇒ The shape is **per-queue**: `helios_venus_queue_gpu_fence(VkQueue, uint64_t *out_wire_fence)`, which encodes `vkWaitRingSeqnoMESA(vn_ring_get_id(dev->primary_ring), vn_ring_current_seqno(...))` — the barrier proving the host issued our `vkQueueSubmit` — and submits it with `.ring_idx = queue->ring_idx` (`vn_queue.h:26`, assigned `vn_device.c:83-88`, **never 0**), returning the KMD-assigned wire fence. Template: `vn_signal_win32_external_semaphore` (`vn_queue.c:1710-1755`). ⚠ A non-empty cs is **mandatory** — `helios_submit` skips the escape entirely when `cs_size == 0` (`vn_renderer_helios.c:3038-3044`) and no fence id comes back; and it discards the id unless a sync is attached (`:3037-3060`), so this calls `helios_ioctl_submit_cs` directly under one explicit `dev_mutex` acquisition (`mtx_plain`, **non-recursive**, `:454`). ⛔ Must refuse `ring_idx == 0` loudly: the host fails a fence on an unbound/out-of-range ring (`vkr_context.c:151-155`), the virtio response never comes, and the KMD's in-flight entry is **immortal** — the wedge at `gpu/mod.rs:3522-3537`. | `icd/mesa` | S | **[P]** |
| **K-F3** | ✅ **LANDED 2026-08-06** (`629d1da`) as `HeliosD3D12SubmitCmd` — `{magic `'HE12'`, version, gpu_wire_fence: u64}`, **16 bytes**, declared once in `protocol/src/wddm.rs` per D13. ⭐ The length is load-bearing in **both** directions and the `const _` block asserts both: it must reach 16 to be recognisable at all (`DxgkDdiRender` gates its refresh decode on `cmd_len >= 16`) and stay under the 48-byte typed-scanout prefix so that arm is never attempted. Both existing arms then reject on magic — which, not length, is the real guard. ⛔ Not `HeliosPresentRenderCmd` — `HeliosPresentPrivateData::is_valid()` requires `resource_id != 0` and an ECL has no primary. ⛔ Not `HeliosPresentRefreshCmd` — its arm unconditionally arms a scanout refresh (`submit_command.rs:1064`), which a compute or graphics ECL must not do. ⭐ `is_valid()` deliberately does **not** check the fence: `0` is legal and means *"submit the packet, order it against nothing"* — the plumbing arm and the A/B disable — and the record's **presence**, not its content, is what scopes K-F0's hold to this path instead of the adapter-global FIFO. ⭐ A guest-supplied fence is safe by asymmetry: `gpu/mod.rs:5609-5616` clamps a value at or beyond `next_wire_fence`, so the worst case is naming an *earlier* fence and under-waiting (a lie, with a counter) — no unsatisfiable boundary, and therefore no wedge, is reachable from this field. | `protocol` | S | ✅ |
| **K-F4** | Decode K-F3 in `dxgkddi_render` and write the wire fence into `PresentSubmissionPrivate.gpu_fence_id` via `merge_fence` (`present_packet.rs:299-344`). ⚠ **`dxgkddi_render` never touches `pDmaBufferPrivateData` today** — every hit in `submit_command.rs` is on the `submit` arg. `DXGKARG_RENDER` has both fields. Write at offset 0, do not advance the pointer, which is the shape Present already proves. ⭐ Everything downstream is already correct **and already guarded**: `gpu/mod.rs:5609-5616` rejects a guest value `>= next_wire_fence` — *"must not manufacture an impossible future dependency"*. | `kmd_render` | S | **[P]** |

⚠ **Do not add a new private-data record.** `PRESENT_DMA_PRIVATE_DATA_BYTES = 88` with
`PresentSubmissionPrivate` 32 at offset 0 and `PresentFlipPrivate` 56 at offset 32 — **exactly
full**, with compile-time asserts (`present_packet.rs:231-241`) written to force the issue. Reusing
`gpu_fence_id` costs zero bytes.

⚠ **One silent-lie hazard to guard**: `DmaGpuFence=0` routes a D3D12 packet to
`RetireDomain::DecodeOnly` (`gpu/mod.rs:5599`) and makes the fence lie quietly. Default is 1.

### 14a.3 Present — the kernel-allocation-identity bridge

⭐ **The shape is not "the ICD hands over its `D3DKMT_HANDLE`" (it has none that means anything —
its only `D3DKMTCreateAllocation2` mints a `kind = TRACKING` VidMm charge the KMD forbids from
carrying identity, `create_allocation.rs:2333-2344`) and not "`umd12` allocates and the ICD
imports" (backwards). It is the third shape, the one D3D11 ships: the engine allocates the Vulkan
memory, and the UMD ADOPTS it** by calling `pfnAllocateCb` with
`HeliosWddmAllocPrivate.adopt_resource_id = <venus resid>`.

**The KMD already accepts exactly that** — `create_allocation.rs:2377-2379`,
`kind == DEVICE_MEMORY && adopt_resource_id != 0` → `AllocationBacking::AdoptedUmdResource`, with
`write_open_identity` stamping `HeliosWddmOpenIdentity` back so DWM's D3D11 opener works unchanged.
⇒ **no new allocation shape, no new KMD verb.**

| # | Item | Where | Size | Rung |
|---|---|---|---|---|
| **UP-1** | Take the `helios_protocol` dependency and reuse `HeliosWddmAllocPrivate` / `Meta` / `OpenIdentity` / `PresentPrivateData` / `PresentRenderCmd` **byte for byte** (D13). `resource12.rs:76-96` states their absence as a deliberate outcome — this is the commit that ends it. | `umd12` | XS | 1 |
| **UP-2** | ⭐ **A ~20-line vkd3d method, not an M-sized patch.** `ID3D12DXVKInteropDevice3::GetVulkanHeapInfo` (`libs/vkd3d/device_vkd3d_ext.c:1123-1146`) already returns `{VkDeviceMemory, offset, vk_memory_type}` for an `ID3D12Heap`; a **committed** resource has no `ID3D12Heap`, and `GetVulkanResourceInfo1` returns the `VkImage`/`VkBuffer`, not the memory. Add the sibling — an `ID3D12DXVKInteropDevice4::GetVulkanResourceMemoryInfo(ID3D12Resource*, …)` reading `resource->mem.device_allocation.{vk_memory, vk_memory_type}` and `.offset` — same file, same shape, same guards. | vkd3d fork | S | 1 |
| **UP-3** | Force **dedicated + venus-exportable** memory for resources created under `D3D12DDI_HEAP_FLAG_PRIMARY`. Two hazards, both real: vkd3d suballocates committed textures unless `prefersDedicatedAllocation` (`libs/vkd3d/resource.c:4434-4461`) — one venus resid covering several D3D12 resources breaks both the one-resource-one-allocation rule and D3D11's `memory_offset == 0` precondition (`umd/src/forward/resource.rs:488-490`); and vkd3d chains `VkExportMemoryAllocateInfo` only for `D3D12_HEAP_FLAG_SHARED`, which a back buffer does not set. ⛔ Do **not** reach it by passing `SHARED` through — `VK_KHR_external_memory_win32` is absent on this device and vkd3d chains the export info unguarded. | vkd3d fork | M | 1 |
| **UP-4** | The resource→identity table: every `pfnCreateHeapAndResource` records `{engine res*, vk_memory, offset, size, venus res_id, venus_alloc_size, memory_type_index, geometry, is_primary}`. Stays local to `umd12` per D13's refinement. ⭐ The trigger is **already detected and counted**: `D3D12DDI_HEAP_FLAG_PRIMARY = 16` arrives and is dropped at `resource12.rs:630-645` (`HeapPrimaryFlagDropped`); `D3D12DDI_RESOURCE_OPTIMIZATION_FLAG_PRIMARY = 4` is the second signal. | `umd12` | M | 1 |
| **UP-5** | Call the corelayer `pfnAllocateCb` (`D3D12DDICB_ALLOCATE_0022`) for presentable resources with `kind = DEVICE_MEMORY`, `adopt_resource_id`, `Flags = PRIMARY`, `Reserved[5]` **zeroed** (`Reserved fields in D3D12DDI_ALLOCATION_INFO_0022 were not zero.` is a runtime string), then `helios_venus_memory_transfer_resource_ownership` (`vn_renderer_helios.c:806`). Unwind on failure. | `umd12` | M | 1 |
| **UP-6** | `pfnCheckResourceAllocationHandle` returns the real handle; `pfnGetDebugAllocationInfo` fills its four fields. ⚠ **Re-grade both counters** — today they are graded for a driver that has no handles. | `umd12` | XS | 1 |
| **UP-7/8/9** | The three L8 slots: `pfnGetPresentPrivateDriverDataSize` (0, with a 72-byte arm behind a knob for U6's arrival half), `pfnPresent` (fill `BroadcastSrc/DstAllocation[0]`, `AddedGpuWork=FALSE`, `SyncIntervalOverrideValid=FALSE`, `_CONTEXTS.hContext = QueueState::h_context`, `_HWQUEUES.BroadcastQueueCount = 0`; **null-check both `_Out_opt_`s** — `D12-G5` saw `pHwQ` non-NULL at `_0040` and NULL at `_0110`), and the identity `pfnRenderCb` carrying `HeliosPresentRenderCmd`. | `umd12` | M | 1 |
| **KP-1..5** | KMD present items: **counters and one measurement, no structural work.** `P12sub`/`P12take`/`P12ref` (≤14 chars, `diag.rs:471`); the `PBIdOk` decode behind `DiagLevel >= 1`; and ⚠ **attribute which `DxgkDdiPresent` arm a D3D12 windowed present takes** — the measured `pfnPresent` carries `DXGI_DDI_PRESENT_FLAG_Blt` (`Flags = 0x21`), and `DXGI_DDI_PRESENT_FLAGS` ≠ `DXGK_PRESENTFLAGS`; the mapping is established nowhere. | `kmd_render` | XS | 1 |

⭐ **Rung 1 needs no new KMD scanout work.** DWM composites, so the app's back buffer is opened by
**DWM's D3D11 device** through `helios_umd.dll`'s existing `pfnOpenResource` reading
`HeliosWddmOpenIdentity`. `umd12`'s `pfnOpenHeapAndResource` serves the *other* direction of D3c and
is **not** on rung 1's path. Fullscreen flip is where `PresentFlipPrivate`, the D2 identity/epoch
gate and `set_scanout_blob` engage — later, and deliberately not scoped here.

### 14a.4 Ordering, and why

1. ⛔ **The fence bridge first, and it is not negotiable.** A present built on an untruthful fence
   presents an unfinished frame — and would be misread as a present bug. Rung 1 cannot pass while
   rung 0 fails.
2. **FB-1 is shared by both `pfnRenderCb` users.** Land it once, in the fence work.
3. **`P7` — does `DxgkDdiRender` fire on the D3D12 path — is settled by the fence work for free**,
   but ⛔ **NOT by the counter this row named.** ~~`RENDER_COUNT` (`submit_command.rs:996`) moving is
   the whole test.~~ `RENDER_COUNT` is **adapter-global and incremented from three sites**
   (`submit_command.rs:1046`, `:1377`, `:1434` — the citation had also drifted from `:996`), and
   DWM's own D3D11 present path calls `pfnRenderCb` on every frame (`umd/src/forward/present.rs:860`).
   So it moves continuously with no D3D12 client at all. P7 is settled by the **record-seen counter**
   on K-F3's decode arm, which is D3D12-specific by construction.
   ⭐ **The general trap, and this document has now walked into it three times in one family:** every
   KMD counter here is adapter-global, and **DWM is always running**, so any counter read to
   attribute a *client-specific* behaviour needs a client-specific arm or a client-specific counter.
   `WfBWire` (dominated by DWM's presents), `RING_SUBMIT_COUNT` (dominated by this driver's own
   scanout copies, §14a.1) and `RENDER_COUNT` (dominated by DWM's `pfnRenderCb`) were each proposed
   as the test for something they cannot attribute.
4. **The allocation-identity bridge does NOT block the fence bridge**: `D3DDDICB_RENDER` names its
   target by `hContext` and needs no driver-minted `D3DKMT_HANDLE`. ⇒ UP-1…UP-6 can be written in
   parallel with the fence measurement, and should be — the vkd3d work plus a new table is the long
   pole.
5. **K-F4 and UP-9 touch the same `dxgkddi_render` region.** Land the KMD side once, with both
   callers in view.
6. **K-F2 is independent of all of it** and is a live defect today. It can land first, alone.

### 14a.5 ⛔ What must NOT be done

* **No producer-side CPU stall to "fix" the ordering** — rejected by the owner and by
  `umd/src/knobs.rs:31-43`'s standing directive. `tmp/dx12/FENCE-BRIDGE-DESIGN.md` design A is
  recorded there as rejected; do not re-propose it.
* **No design routed through `pfnSignalFence`.** Measured: it is **never called** on this driver
  (`FenceSignalForwarded=0`, and no trace line ever emitted), matching the WARP observation in
  `DDI_REFERENCE.md` §14.0.
* **No `pfnSignal*Cb` for the application's fence.** `D3D12DDI_FENCE` carries no `D3DKMT_HANDLE` and
  no `hRTFence`, and every such callback names its target by `D3DKMT_HANDLE` — see
  `DDI_REFERENCE.md` §10.4's correction block.
* **No `pfnSubmitCommandCb`.** This queue's context is legacy by a decision taken inside
  `pfnCreateCommandQueue`; the pair is `pfnCreateContextCb` → `pfnRenderCb`.

---

## 15. Corrections to the pre-2026-08-05 `DX12.md` §3

**Right, re-derived and confirmed:** 84 of 187 `DxgkDdi*` slots set, 193 struct fields, 103 unset ·
one `DXGK_ENGINE_TYPE_3D` node, `NbAsymetricProcessingNodes = 1` ·
`SchedulingCaps = MultiEngineAware|PreemptionAware` · the `(0,0)`-only scheduler DDIs ·
`CreateContext` ignoring `NodeOrdinal`/`EngineAffinity`/`Flags` · `DmaBufferSegmentSet = 1`,
`DmaBufferSize = 256 KiB`, `AllocationListSize = PatchLocationListSize =
DXGK_ALLOCATION_LIST_SIZE_GDICONTEXT` · `MaxAllocationListSlotId = 0xFFFF` · HW scheduling refused
at `CreateHwQueue` with `HwQRef` · no `Offer`/`Reclaim` slot in this WDK ·
`WddmSurface::Wddm2_1GpuMmu`, 40-bit VA, 4 KiB pages, 4 levels, `GPU_PHYSICAL`, all optional
`GPUMMUCAPS` bits 0 · the unset fence/doorbell/context-scheduling/protected-content/MPO families ·
*"the KMD has no D3D12 awareness of any kind, which is correct — WDDM has no D3D12-specific miniport
DDI."*

**Wrong:**

1. **§3.2: "`DXGK_VIDMMCAPS.DriverManagesResidency` … is not set by this driver."**
   `DriverManagesResidency` is **not a `DXGK_VIDMMCAPS` bit**. It is
   `DXGK_CONTEXTINFO_CAPS.DriverManagesResidency`, bit 1 of a **per-context** flags word
   (`d3dkmddi.h:1550-1563`) — which is why the bindgen accessor sits next to `NoPatchingRequired`.
   The conclusion (residency is dxgkrnl/VidMm's job) is unaffected; the mechanism is per-context and
   Helios never writes `ContextInfo.Caps` at all.
2. **§1.4: the `d3d12` grep hit is at `create_allocation.rs:1436-1438`.** It is at **`:1438`**, and
   there is a second sibling hit in the same lineage at `kmd_logic/src/lib.rs:120`.

**Incomplete or stale:**

3. **§3 treats "103 unset slots" as one population.** It is two: **31 unset-and-reachable** and
   **72 unset-and-unreachable** at WDDM 2.1. Every fence, doorbell, context-scheduling and
   protected-session slot §3.5 lists is in the *unreachable* bucket. The question is not "which of
   103 do we implement" but "does the WDDM 2.1 surface carry D3D12" — and it does.
4. **§3.1's "hardware scheduling is refused, deliberately and consistently"** is true but
   understated: at WDDM 2.1 those slots are almost certainly never reached at all, so the refusal is
   defence in depth, not the operative mechanism. §3.1's own evidence (`PHQcall` absent) is better
   explained by the version gate than by the refusal.
5. **§3.1 marks "whether dxgkrnl synthesises COPY/COMPUTE queues over one node" UNVERIFIED.** Half
   of it is now settled from the headers (§5): the D3D12 queue-creation DDI has no node ordinal, so
   the ordinal is the UMD's to supply and no KMD change is implied. Whether the runtime accepts all
   three classes on node 0 stays UNVERIFIED (U6).
6. **§3.5 omits `DxgkDdiUpdateMonitoredValues` and `DxgkDdiUpdateCurrentValuesFromCpu`** from the
   fence family (both WDDM 3.1, both unset). Harmless.
7. **§3 does not mention the `CreateAllocation` private-data gate** — the single most consequential
   KMD fact for a D3D12 UMD (§2c).
8. **§3 does not mention that `CrossAdaptCaps` defaults to 0**, so the cross-adapter cap it
   discusses is *not* advertised on the live box.

---

## 16. UNVERIFIED, with settling experiments

| # | Question | Settling experiment |
|---|---|---|
| U1 | Does dxgkrnl truncate `DRIVER_INITIALIZATION_DATA` at the declared `Version`, making the nine above-2.1 set slots dead? | Add an unconditional `HwQEnt` counter as the *first* statement of `dxgkddi_create_hw_queue`, deploy, run a GPU workload, read the service key. Or disassemble `dxgkrnl!DpiInitializeEx` under `ntoseye`. |
| U2 | ★ Does the D3D12 runtime tolerate `DXGKQAITYPE_PHYSICALADAPTERCAPS` (0x0F) returning `STATUS_NOT_SUPPORTED`, the way DXGI/D3D11 does? Answering it is a **bugcheck 0x3B in dxgmms2** today (`query_adapter_info.rs:78-84`) | Once `OpenAdapter12` returns something, call `D3D12CreateDevice`; separately take a `Microsoft-Windows-DxgKrnl` all-keywords ETW slice around the create → `tracerpt` → grep `AzureTriage`. **Highest-probability D3D12-specific KMD surprise.** |
| U3 | ★ Does a **D3D12-shaped** monitored fence advance on this adapter with no GPU-side write? | The `D12-G-fence` probe in §7. |
| U4 | Does the D3D12 runtime accept GPU VAs the driver never obtained from the kernel (vkd3d's Vulkan BDAs)? | §8 — a `D3D12HelloWorld` sample with the debug layer enabled, watching for the `MaxGPUVirtualAddressBitsPerResource` validation string. |
| U5 | Is `DXGKARG_CREATECONTEXT.ContextInfo` zero-initialised by dxgkrnl before the DDI? Helios writes 5 of its 8 fields and leaves `Reserved`, `Caps`, `PagingCompanionNodeId` untouched | Read `ContextInfo` at entry in a debug build, or an `ntoseye` breakpoint on `dxgkddi_create_context` dumping the 32 bytes. Blocks K2. |
| U6 | Does dxgkrnl reject `NodeOrdinal > 0` before reaching `DxgkDdiCreateContext`? | A `tools/` probe calling `D3DKMTCreateContextVirtual` with `NodeOrdinal = 1`; record the NTSTATUS. Pairs with K1. |
| U7 | Is `D3D12_FEATURE_DATA_GPU_VIRTUAL_ADDRESS_SUPPORT.MaxGPUVirtualAddressBitsPerProcess` derived from the KMD's `VirtualAddressBitCount` (40)? | `CheckFeatureSupport` on Helios once a D3D12 device exists, compared with `D3DKMTQueryAdapterInfo(KMTQAITYPE_GPUMMU_CAPS)`. |
| U8 | Are `MAX_BLOBS = 8192` / `MAX_MAPPINGS = 8192` enough for a real D3D12 title? | Run a D3D12 sample under vkd3d and read `HELIOS_ESCAPE_QUERY_STATS` (`escape.rs:966`) / the blob-table counters. |
| U9 | Does `ApertureSegmentCommitLimit = 64 MiB` under-report D3D12's shared budget? | `IDXGIAdapter3::QueryVideoMemoryInfo` on both `LOCAL` and `NON_LOCAL` pools under a D3D12 device; `tools/vram_report_probe.cpp` already does the DXGI/VidMm/Venus comparison for D3D11. |
| U10 | Do D3D12 GPU upload heaps (`D3D12_HEAP_TYPE_GPU_UPLOAD`) need a KMD segment flag Helios does not set? | The `OPTIONS16` cap in `docs/dx12/DDI_REFERENCE.md`, then whether any `DXGK_SEGMENTFLAGS2` bit gates it. |

---

## 17. Load-bearing facts other documents must not contradict

1. All Vulkan/venus GPU submission goes through **`D3DKMTEscape`**, not `D3DKMTRender`.
2. `DxgkDdiCreateAllocation` hard-refuses allocations without a valid 48-byte
   `HeliosWddmAllocPrivate` (magic + version).
3. The adapter declares **WDDM 2.1** (`DXGKDDI_INTERFACE_VERSION_WDDM2_1` = 24579), not 3.2.
   WDDM 2.0 is the D3D12 floor, so 2.1 clears it.
4. **One** engine node, `DXGK_ENGINE_TYPE_3D`, ordinal 0 only.
5. D3D12 queue class → WDDM node is a **UMD** decision.
6. Monitored fences work today with **zero** KMD support beyond DMA-packet retirement. ⚠ **And the
   rider is the whole D3D12 fence defect (§7's correction, §14a):** "beyond DMA-packet retirement"
   is not a small print — retirement of packets *on the waiting context* is the entire lever, and a
   client that submits no packets gets a fence that signals immediately. What was proven is the
   primitive between two **software sync packets**; the DMA-packet dependency is **UV1** and is
   unmeasured.
7. Guest GPU page tables are **decorative**; the host GPU owns the real MMU.
6a. ⭐ **A venus wire fence means host GPU completion only on `ring_idx >= 1`, and vkd3d produces
   none.** The command stream rides the shared venus ring and never touches virtio
   (`vn_ring.c:630-636`); the only virtio submission a frame can make is the ring-0
   `vkNotifyRingMESA` doorbell, which is itself sent only when the host ring advertises IDLE and
   then only past a 1 ms limiter (`vn_ring.c:673-690`) — so a busy ring emits **nothing** and
   `next_wire_fence` freezes. The sole `ring_idx >= 1` producer on Windows is
   `vn_signal_win32_external_semaphore`, which needs an OPAQUE_WIN32 semaphore: DXVK's present path
   has one, vkd3d never asks for one. And a ring-0 fence carries no
   `VIRTIO_GPU_FLAG_INFO_RING_IDX` (`gpu/mod.rs:3482`), so QEMU routes it to the legacy
   `virgl_renderer_create_fence`, which never sees the venus context
   (`qemu-helios/hw/display/virtio-gpu-virgl.c:1167-1186`). ⇒ **Any design that gates a D3D12
   completion on "the last wire fence" is gating on nothing.**
8. Segment topology is `[Aperture id 1, Bar id 2]`, `SegmentTable::MAX = 2`, and a
   `SupportsCpuHostAperture` segment must be **LAST**.
9. `ApplicationTarget` + `LocalBudgetGroup` are ON for the BAR segment by default
   (`VidMmVramMB` default 4096).
10. `CrossAdapterResource` is **OFF** by default.
11. ⛔ `FlipImmediateMmIo` must not be re-added — it is defect 0ab.
12. `DXGKQAITYPE_PHYSICALADAPTERCAPS` is deliberately rejected because answering it bugchecks 0x3B.
