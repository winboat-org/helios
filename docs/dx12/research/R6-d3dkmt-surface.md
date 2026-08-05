# R6 — The D3DKMT surface a D3D12 UMD consumes, vs what Helios' KMD backs

**Lane:** R6. **Date:** 2026-08-05. **Status:** research only; nothing in this file was built,
installed, or run on the VM. One read-only PowerShell query was issued against the win11 SDK
headers (recorded verbatim in §1.1).

**Evidence convention used throughout:**
- `H:` = "the SDK/WDK header says" (file:line).
- `D:` = "Microsoft's docs say" (windows-driver-docs markdown, file + quote).
- `C:` = "this code does" (in-tree file:line).
- `I:` = "I infer" — an inference from the above, never a quoted fact.
- **UNVERIFIED** = not established by any of the above; each carries a settling experiment.

---

## 0. The headline, stated once

There are **two different D3DKMT surfaces** in play, and the whole lane turns on which one the
Helios D3D12 strategy picks:

| Strategy | Who touches D3DKMT | Size of that surface |
|---|---|---|
| **(a) native D3D12 UMD in `umd/`** | The **D3D12 runtime** (`d3d12core.dll`) owns every kernel object and hands the UMD a `D3DDDI_DEVICECALLBACKS` table; the UMD calls *through* it, never `D3DKMT*` directly. | **65 callbacks** in `D3DDDI_DEVICECALLBACKS` (§1.1) + 14 `D3D12DDI_CORELAYER_DEVICECALLBACKS_0050` KM callbacks (§1.2). |
| **(b) vkd3d-proton over Vulkan/Venus** | vkd3d-proton itself calls **14 distinct `D3DKMT*` entry points**, all of them in ONE file, and **none of them for rendering, submission, allocation, or fences-in-the-hot-path** — they exist only to publish/consume *shared-resource descriptor metadata* for cross-API interop (§4). | 14 entry points, one file, `libs/vkd3d/d3dkmt.c` (449 lines). |

`C:` `vkd3d-proton-helios/libs/vkd3d/d3dkmt.c` is 449 lines and is the **only** file in the
submodule that calls `D3DKMT*` (`grep -rn "D3DKMT" vkd3d-proton-helios/ --include=*.c --include=*.h
--include=*.cpp` → hits in exactly `libs/vkd3d/d3dkmt.c`, `include/private/vkd3d_d3dkmt.h`,
`libs/vkd3d/vkd3d_private.h` (3 handle fields), `libs/vkd3d/device.c` (2 `D3DKMTShareObjects`
call sites + 1 descriptor open)). Submodule HEAD = `2c7ba22c53261458a7a204c55f3098ad9855cb15`,
2026-08-04, "tests: fix test_fp_truncate_roundtrips when it's skipped" (`git -C
vkd3d-proton-helios log -1`).

Consequence for the plan: **under strategy (b) the D3DKMT surface is essentially already
satisfied**, because the real GPU work goes Vulkan → Venus ICD → the *existing* D3DKMT path
(`icd/mesa`, §3) that has run a composited desktop for months. Under strategy (a) the surface
is the full 73-entry runtime callback table, and §2/§5 are where the cost lives.

---

## 1. The call inventory

### 1.1 `D3DDDI_DEVICECALLBACKS` — what a **native D3D12 UMD** actually gets

`H:` Read verbatim from the win11 VM, SDK **10.0.26100.0**, via
`Select-String -Path "C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0\um\d3dumddi.h"
-Pattern "typedef struct _D3DDDI_DEVICECALLBACKS" -Context 0,120`. Counted with the same query
piped through `Where-Object { $_ -match "^\s+PFND3DDDI_" }` → **65 members** across the version
gates (`pfnAllocateCb` … `pfnSubmitHistorySequenceCb`). This is the same table the live D3D11 UMD holds
(`C:` `umd/src/device_funcs.rs:375`, `pub kt_callbacks: *const ddi::D3DDDI_DEVICECALLBACKS`), and
`H:` `tmp/dx12/sdk/d3d12umddi.h:2659` shows the D3D12 create-device arg carries the identical
pointer:

```c
typedef struct D3D12DDIARG_CREATEDEVICE_0003
{
    D3D12DDI_HRTDEVICE              hRTDevice;              // in:  Runtime handle
    UINT                            Interface;              // in:  Interface version
    UINT                            Version;                // in:  Runtime Version
    CONST D3DDDI_DEVICECALLBACKS*   pKTCallbacks;           // in:  Pointer to runtime callbacks that invoke kernel
    D3D12DDI_HDEVICE                hDrvDevice;             // in:  Driver private handle/ storage.
    ...
```
(`H:` `tmp/dx12/sdk/d3d12umddi.h:2655-2670`.)

**So: a native D3D12 UMD does not call `D3DKMT*`. It calls `pKTCallbacks->pfn*Cb`, and the
runtime translates each to the corresponding `D3DKMT*` on its own kernel objects.** The
`D3DKMT_*` names below are therefore the *kernel-side identity* of each callback, not something
the UMD types.

### 1.2 `D3D12DDI_CORELAYER_DEVICECALLBACKS_*` — the D3D12-specific KM callbacks

`H:` `tmp/dx12/sdk/d3d12umddi.h:2624-2653` (`_0003`), `:4874-4905` (`_0022`, adds
`pfnAllocateCb`/`pfnDeallocateCb`), `:7178-7218` (`_0050`, adds scheduling-group + HW-queue),
`:8606-8647` (`_0062`). The `_0050` shape, verbatim in the relevant region:

```c
    // KM callbacks for 12
    PFND3D12DDI_CREATECONTEXT_CB        pfnCreateContextCb;
    PFND3D12DDI_CREATECONTEXTVIRTUAL_CB pfnCreateContextVirtualCb;
    PFND3D12DDI_DESTROYCONTEXT_CB       pfnDestroyContextCb;
    PFND3D12DDI_CREATEPAGINGQUEUE_CB    pfnCreatePagingQueueCb;
    PFND3D12DDI_DESTROYPAGINGQUEUE_CB   pfnDestroyPagingQueueCb;
    PFND3D12DDI_MAKERESIDENT_CB         pfnMakeResidentCb;
    PFND3D12DDI_EVICT_CB                pfnEvictCb;
    PFND3D12DDI_RECLAIMALLOCATIONS2_CB  pfnReclaimAllocations2Cb;
    PFND3D12DDI_OFFERALLOCATIONS_CB     pfnOfferAllocationsCb;
    PFND3D12DDI_ALLOCATE_CB_0022        pfnAllocateCb;
    PFND3D12DDI_DEALLOCATE_CB_0022      pfnDeallocateCb;
    PFND3D12DDI_CREATESCHEDULINGGROUPCONTEXT_CB_0050        pfnCreateSchedulingGroupContextCb;
    PFND3D12DDI_CREATESCHEDULINGGROUPCONTEXTVIRTUAL_CB_0050 pfnCreateSchedulingGroupContextVirtualCb;
    PFND3D12DDI_CREATEHWQUEUE_CB_0050                       pfnCreateHwQueueCb;
```

Note the *argument* types: `PFND3D12DDI_CREATEHWQUEUE_CB_0050` takes a
`D3DDDICB_CREATEHWQUEUE*` (`H:` `d3d12umddi.h:7170-7174`) — i.e. the D3D12 core layer wraps the
**same** kernel structures as D3D11, keyed by a D3D12 runtime handle
(`D3D12DDI_HRTCOMMANDQUEUE`, `D3D12DDI_HRTSCHEDULINGGROUP_0050`).

### 1.3 THE TABLE — D3DKMT entry point → miniport DDI → kmd_render status

Legend for **kmd_render**: **IMPL** = real implementation; **ACCEPT** = registered and returns
success without doing hardware work (decorative but contract-correct); **REFUSE** = registered
and returns a documented failure with a named counter; **UNSET** = the `_DRIVER_INITIALIZATION_DATA`
slot is NULL; **n/a (VidMm)** = no miniport DDI exists — dxgkrnl/VidMm services it entirely.

Baseline counts: `_DRIVER_INITIALIZATION_DATA` has **187** `DxgkDdi*` slots
(`awk '/pub struct _DRIVER_INITIALIZATION_DATA/,/^}/' tmp/dxgk_bindings.rs | grep -c "pub Dxgk"`
→ 187); `kmd_render/src/lib.rs` sets **84** of them
(`grep -o "data\.Dxgk[A-Za-z0-9_]*" kmd_render/src/lib.rs | sort -u | wc -l` → 84); **103 unset**.

#### (A) Device / context / process creation

| D3DKMT entry (kernel identity) | UMD-side callback | Miniport DDI dxgkrnl calls | kmd_render |
|---|---|---|---|
| `D3DKMTOpenAdapterFromLuid` | *(runtime/loader, or vkd3d directly)* | none (dxgkrnl handle mint) | n/a |
| `D3DKMTEnumAdapters2` | *(ICD uses directly)* | none | n/a — `C:` ICD calls it at `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c:2465,2476` |
| `D3DKMTCreateDevice` | *(runtime; also called directly by ICD/DXVK/vkd3d)* | `DxgkDdiCreateDevice` | **IMPL** `C:` `kmd_render/src/device.rs:256-274`; registered `lib.rs:143` |
| `D3DKMTDestroyDevice` | — | `DxgkDdiDestroyDevice` | **IMPL** `C:` `device.rs:288`; also drains blob mappings in the creating process at PASSIVE or the kernel bugchecks `0x76 PROCESS_HAS_LOCKED_PAGES` (`MAPPING_DRAIN_BATCH = 64`, `device.rs:286`) |
| `D3DKMTCreateContext` | `pfnCreateContextCb` / `pfnCreateContextCb` (12) | `DxgkDdiCreateContext` | **IMPL** `C:` `device.rs:389-431`; registered `lib.rs:145` |
| `D3DKMTCreateContextVirtual` | `pfnCreateContextVirtualCb` | `DxgkDdiCreateContext` (same slot; no separate virtual slot exists in the bindings) | **IMPL**, but **`NodeOrdinal`, `EngineAffinity` and `Flags` are never read** — `C:` `device.rs:389-431` reads only `create_context` to write `hContext` and `ContextInfo` |
| `D3DKMTDestroyContext` | `pfnDestroyContextCb` | `DxgkDdiDestroyContext` | **IMPL** `C:` `device.rs:435-442` |
| `D3DKMTCreateHwContext` | `pfnCreateHwContextCb` | `DxgkDdiCreateHwContext` | registered `lib.rs:166`, `C:` `ddi/scheduler.rs:125` |
| `D3DKMTCreateHwQueue` | `pfnCreateHwQueueCb`, `pfnCreateHwQueueCb` (12, `_0050`) | `DxgkDdiCreateHwQueue` | **REFUSE** `STATUS_NOT_SUPPORTED`, counter `HwQRef` — `C:` `ddi/scheduler.rs:180-187` |
| `D3DKMTSubmitCommandToHwQueue` | `pfnSubmitCommandToHwQueueCb` | `DxgkDdiSubmitCommandToHwQueue` | **REFUSE** `STATUS_NOT_SUPPORTED` `C:` `scheduler.rs:198-208` |
| `D3DKMTSubmitWaitForSyncObjectsToHwQueue` / `…SignalSyncObjects…` | `pfnSubmitWaitForSyncObjectsToHwQueueCb` / `pfnSubmitSignalSyncObjectsToHwQueueCb` | *(HW-queue path)* | unreachable — no queue handle is ever minted (`scheduler.rs:166-177` states this is deliberate) |
| `D3DKMTSubmitPresentToHwQueue` / `SubmitPresentBltToHwQueue` | `pfnSubmitPresentToHwQueueCb` / `pfnSubmitPresentBltToHwQueueCb` | `DxgkDdiPresentToHwQueue` | **REFUSE** `STATUS_NOT_SUPPORTED`, counters `PHQcall`/`PHQours`/`PHQst` `C:` `scheduler.rs:238-252` |
| *(no D3DKMT verb — kernel-internal)* | — | `DxgkDdiCreateProcess` / `DestroyProcess` | **ACCEPT** — `C:` `device.rs:451,471`; `ProcessContext` (`device.rs:247`) is deliberately an opaque token: *"DELIBERATELY EMPTY … the object is an opaque token and nothing more"* |

**The one hard fact for D3D12 here:** exactly one engine node exists and hardware scheduling is
refused at *queue creation*. `C:` `query_adapter_info.rs:1254-1278` (single
`DXGK_ENGINE_TYPE_3D` node), `:456-464` (`NbAsymetricProcessingNodes = 1`), `:304-305,395`
(`SchedulingCaps = MultiEngineAware | PreemptionAware`), `scheduler.rs:55-123`
(`QueryDependentEngineGroup`/`QueryEngineStatus`/`ResetEngine` accept `NodeOrdinal==0 &&
EngineOrdinal==0` only).

#### (B) Memory / allocation

| D3DKMT entry | UMD-side callback | Miniport DDI | kmd_render |
|---|---|---|---|
| `D3DKMTCreateAllocation` / `D3DKMTCreateAllocation2` | `pfnAllocateCb` (11 + 12 `_0022`) | `DxgkDdiCreateAllocation` | **IMPL** `C:` `ddi/create_allocation.rs:2660`; registered `lib.rs:151`. Slot `DxgkDdiCreateAllocation2` exists in the bindings but is typed `PVOID` (`tmp/dxgk_bindings.rs:95265`) and is **UNSET** |
| `D3DKMTDestroyAllocation` / `…2` | `pfnDeallocateCb` / `pfnDeallocate2Cb` | `DxgkDdiDestroyAllocation` | **IMPL** `create_allocation.rs:2775` |
| `D3DKMTOpenResource` / `OpenResource2` / `OpenResourceFromNtHandle` | *(runtime)* | `DxgkDdiOpenAllocation` | **IMPL** `create_allocation.rs:2849`; registered `lib.rs:211` |
| *(close of the above)* | — | `DxgkDdiCloseAllocation` | **IMPL** `create_allocation.rs:3063` |
| `D3DKMTQueryResourceInfo` / `…FromNtHandle` | — | `DxgkDdiDescribeAllocation` | **IMPL** `create_allocation.rs:3091`; registered `lib.rs:213` |
| *(runtime standard allocations: primaries, shadow, staging)* | — | `DxgkDdiGetStandardAllocationDriverData` | **IMPL** `create_allocation.rs:3149`; registered `lib.rs:214` |
| `D3DKMTCreatePagingQueue` / `DestroyPagingQueue` | `pfnCreatePagingQueueCb` / `pfnDestroyPagingQueueCb` (11 **and** 12) | **none** (VidMm object) | n/a (VidMm). `D:` `device-paging-queues.md`: "Each graphics device has a dedicated paging queue … A device paging fence object is associated with the queue … The device paging fence is a regular monitored fence object" |
| `D3DKMTMakeResident` | `pfnMakeResidentCb` (11 + 12) | `DxgkDdiBuildPagingBuffer` → `DxgkDdiSubmitCommand(Flags.Paging=1)` | **IMPL** `C:` `ddi/build_paging_buffer.rs:1298` (registered `lib.rs:153`) + `submit_command.rs:766` paging arm (`SUBMIT_PAGING_COUNT`) |
| `D3DKMTEvict` | `pfnEvictCb` (11 + 12) | same paging path | **IMPL** (same) |
| `D3DKMTOfferAllocations` / `ReclaimAllocations(2/3)` | `pfnOfferAllocationsCb` / `pfnReclaimAllocations2Cb` … (11 **and** 12) | **none** — no `DxgkDdiOfferAllocations` slot exists in this WDK | n/a (VidMm). Verified: the 187-slot list contains no `Offer`/`Reclaim` DDI |
| `D3DKMTLock2` / `Unlock2` | `pfnLock2Cb` / `pfnUnlock2Cb` | `DxgkDdiMapCpuHostAperture` / `UnmapCpuHostAperture` for CPU-host-aperture segments | **IMPL, REAL** `C:` `ddi/cpu_host_aperture.rs:294,429`; registered `lib.rs:154-155`. Header comment `cpu_host_aperture.rs:10-24` states the contract: whole-allocation, consecutive-page requests only; anything else is **refused loudly** with `ChE*` counters. ⚠ that comment says "Segment 3" — **stale**: the BAR memory segment is id **2** today (`ddi/gpummu.rs:88-95`, and `gpummu.rs:92-95` records that "A sibling `BAR_SEGMENT_ID = 3` used to sit here … id 3 no longer exists") |
| `D3DKMTInvalidateCache` | `pfnInvalidateCacheCb` | *(cache maintenance; no Helios path)* | **UNVERIFIED** — see §7 |
| `D3DKMTReserveGpuVirtualAddress` | `pfnReserveGpuVirtualAddressCb` | `DxgkDdiBuildPagingBuffer(UPDATE_PAGE_TABLE)` in `GPU_PHYSICAL` mode | **ACCEPT (decorative)** — `C:` `ddi/gpummu.rs:1-14`: "the guest page tables are *decorative* — their content is never read by any hardware" |
| `D3DKMTMapGpuVirtualAddress` / `FreeGpuVirtualAddress` / `UpdateGpuVirtualAddress` | `pfnMapGpuVirtualAddressCb` / `pfnFreeGpuVirtualAddressCb` / `pfnUpdateGpuVirtualAddressCb` | same | **ACCEPT (decorative)**. `DxgkDdiUpdatePageTable`, `UpdatePageDirectory`, `MovePageDirectory`, `DescribePageTable` are all **UNSET** (consistent with `PageTableUpdateMode = GPU_PHYSICAL`, `gpummu.rs:36-40`) |
| *(root page table)* | — | `DxgkDdiSetRootPageTable` / `GetRootPageTableSize` | **ACCEPT** `C:` `build_paging_buffer.rs:1470,1487`; registered `lib.rs:217-218` |
| `D3DKMTQueryVideoMemoryInfo` | *(runtime → DXGI `QueryVideoMemoryInfo`)* | **none** | n/a (VidMm) — computed from the reported segment table (§5.5) |
| `D3DKMTChangeVideoMemoryReservation` | — | none | n/a (VidMm) |
| `D3DKMTSetAllocationPriority` / `GetAllocationPriority` / `QueryAllocationResidency` | `pfnSetPriorityCb` / `pfnQueryResidencyCb` | none | n/a (VidMm) |
| `D3DKMTUpdateAllocationProperty` | `pfnUpdateAllocationPropertyCb` | `DxgkDdiValidateUpdateAllocationProperty` | **UNSET** |
| `D3DKMTRegisterTrimNotification` / `Unregister` | — | none | n/a (VidMm); `D:` `driver-residency-in-wddm-2-0.md` names `TrimResidency` as a UMD callback |

#### (C) Submission

| D3DKMT entry | UMD-side callback | Miniport DDI | kmd_render |
|---|---|---|---|
| `D3DKMTRender` | `pfnRenderCb` | `DxgkDdiRender` (legacy) / `DxgkDdiRenderKm` / `DxgkDdiRenderGdi` | **IMPL** `C:` `submit_command.rs:992` / `:1323` / `:1380`; registered `lib.rs:207-209` |
| `D3DKMTSubmitCommand` | `pfnSubmitCommandCb` | **`DxgkDdiSubmitCommandVirtual`** for GpuMmu contexts — not `DxgkDdiSubmitCommand` | **IMPL** `C:` `submit_command.rs:725-762`. The doc comment there is the authoritative in-tree statement: *"Because Helios declares the GpuMmu model (`VirtualAddressingSupported` + `GpuMmuSupported`), VidSch routes a GpuMmu context's command buffers HERE, not to `DxgkDdiSubmitCommand`… gets NOT_SUPPORTED (0xC00000BB), and bugchecks **0x119 (VIDEO_SCHEDULER_INTERNAL_ERROR) Arg1=2**"* |
| *(paging submission)* | — | `DxgkDdiSubmitCommand` with `Flags.Paging` | **IMPL** `submit_command.rs:766-798` |
| *(patching)* | — | `DxgkDdiPatch` | **ACCEPT (no-op)** `C:` `submit_command.rs:1421-1430` — null-checks then `STATUS_SUCCESS` |
| — | — | `DxgkDdiPreemptCommand` / `ResetFromTimeout` / `RestartFromTimeout` / `CancelCommand` | **IMPL** `submit_command.rs:905,936,970`; `scheduler.rs:255` |
| — | — | `DxgkDdiQueryCurrentFence` | **IMPL** `C:` `submit_command.rs:1433-1453`; returns `adapter.completed_fence()`, `NodeOrdinal=0`, `EngineOrdinal=0` |
| `D3DKMTSetContextSchedulingPriority` / `Get…` | — | `DxgkDdiSetContextSchedulingProperties` | **UNSET** — D3D12 command-queue priorities have nowhere to land |
| — | — | `DxgkDdiSuspendContext` / `ResumeContext` / `SetupPriorityBands` / `NotifyContextPriorityChange` | **UNSET** |

#### (D) Synchronization

| D3DKMT entry | UMD-side callback | Miniport DDI | kmd_render |
|---|---|---|---|
| `D3DKMTCreateSynchronizationObject2` (`D3DDDI_MONITORED_FENCE`) | `pfnCreateSynchronizationObject2Cb` | **none** in the software-scheduled path | n/a (VidSch). `D:` `context-monitoring.md`: "The Direct3D runtime creates a monitored fence object by calling the user-mode driver's *pfnCreateSynchronizationObject2Cb*" |
| `D3DKMTSignalSynchronizationObjectFromCpu` | `pfnSignalSynchronizationObjectFromCpuCb` | none | n/a. `D:` `context-monitoring.md`: "*Dxgkrnl* updates the fence memory location with the signaled value" |
| `D3DKMTWaitForSynchronizationObjectFromCpu` | `pfnWaitForSynchronizationObjectFromCpuCb` | none | n/a |
| `D3DKMTSignalSynchronizationObjectFromGpu(2)` / `WaitForSynchronizationObjectFromGpu` | `pfnSignalSynchronizationObjectFromGpuCb` / `pfnWaitForSynchronizationObjectFromGpuCb` | VidSch queues a software signal/wait packet against the context; the miniport sees only the resulting DMA ordering | **works today**, via `DXGK_INTERRUPT_DMA_COMPLETED` from the DPC — `C:` `ddi/interrupt.rs:11` header: "(`DXGK_INTERRUPT_DMA_COMPLETED` at DIRQL via `signal_dma_completed`)" |
| `D3DKMTSignalSynchronizationObject2` / `WaitForSynchronizationObject2` | `pfnSignalSynchronizationObject2Cb` / `pfnWaitForSynchronizationObject2Cb` | same | same |
| `D3DKMTOpenSyncObjectFromNtHandle(2)` / `OpenSyncObjectNtHandleFromName` / `ShareObjects` | — | none | n/a — pure dxgkrnl object plumbing |
| *(hardware monitored-fence signal)* | — | `DxgkDdiSignalMonitoredFence` | **UNSET** |
| *(native fence, WDDM 3.0)* | `D3DKMTCreateNativeFence` etc. | `DxgkDdiCreateNativeFence` / `DestroyNativeFence` / `OpenNativeFence` / `CloseNativeFence` / `SetNativeFenceLogBuffer` / `UpdateNativeFenceLogs` | **all UNSET** |
| *(user-mode submission)* | `D3DKMTCreateDoorbell` etc. | `DxgkDdiCreateDoorbell` / `ConnectDoorbell` / `DisconnectDoorbell` / `DestroyDoorbell` / `NotifyWorkSubmission` | **all UNSET** |

#### (E) Misc

| D3DKMT entry | UMD-side callback | Miniport DDI | kmd_render |
|---|---|---|---|
| `D3DKMTEscape` | `pfnEscapeCb` | `DxgkDdiEscape` | **IMPL, and it is the whole Helios transport** — `C:` `ddi/escape.rs:254-390`; registered `lib.rs:182`. §6 |
| `D3DKMTQueryAdapterInfo` (driver-private types) | *(adapter callbacks)* | `DxgkDdiQueryAdapterInfo` | **IMPL** `C:` `ddi/query_adapter_info.rs:27`; registered `lib.rs:132` |
| `D3DKMTQueryAdapterInfo(KMTQAITYPE_ADAPTERREGISTRYINFO)` | — | **none** — dxgkrnl reads the registry | n/a; `C:` the ICD records that it "has been observed returning `STATUS_OBJECT_NAME_NOT_FOUND` (0xc0000034) for every adapter" (`vn_renderer_helios.c:2509-2515`) |
| `D3DKMTGetDeviceState` | — | none (dxgkrnl device-error state) | n/a |
| `D3DKMTMarkDeviceAsError` | — | none | n/a |
| `D3DKMTSetStablePowerState` | — | `DxgkDdiSetStablePowerState` | registered `lib.rs:179`, `C:` `scheduler.rs:300` |
| `D3DKMTPresent` / `PresentMultiPlaneOverlay*` | `pfnPresentCb` / `pfnPresentMultiPlaneOverlayCb` | `DxgkDdiPresent` | **IMPL** `C:` `ddi/display.rs:179`; the MPO family (`CheckMultiPlaneOverlaySupport*`, `SetVidPnSourceAddressWithMultiPlaneOverlay*`, `GetMultiPlaneOverlayCaps`, `PostMultiPlaneOverlayPresent`) is **UNSET** |

---

## 2. Who calls what — layer ownership in the Helios architecture

Three candidate owners exist per call. Assigning them is the design decision R6 exists to expose.

| Layer | What it owns today (D3D11) | What it would own under D3D12 (a) native UMD | Under D3D12 (b) vkd3d |
|---|---|---|---|
| **D3D runtime** (`d3d11.dll` / `d3d12core.dll`) | Mints the `D3DKMT` device + context, allocates via `pfnAllocateCb`, drives Present. | Same, for D3D12 objects: `D3DKMTCreateDevice`, `CreateContext(Virtual)`, `CreatePagingQueue`, monitored fences for every `ID3D12Fence`, `MakeResident`/`Evict` for the residency budget. | **Nothing** — vkd3d *is* the runtime; `d3d12core.dll` is replaced. |
| **Helios UMD** (`umd/`) | Calls **13 distinct** kernel callbacks. `C:` `grep -rn "pfn[A-Za-z0-9]*Cb" umd/src/ -o` → `pfnSetErrorCb`(12), `pfnRenderCb`(11), `pfnPresentCb`(8), `pfnAllocateCb`(8), `pfnDeallocateCb`(7), `pfnMakeResidentCb`(5), `pfnEscapeCb`(5), `pfnPresentMultiplaneOverlayCb`(4), `pfnCreateContextCb`(4), `pfnSetDisplayModeCb`(3), `pfnCreatePagingQueueCb`(3), `pfnEvictCb`(2), `pfnDestroyPagingQueueCb`(2), `pfnWaitForSynchronizationObjectFromCpuCb`(1), `pfnDestroyContextCb`(1). | Would need a second, larger set: everything in §1.2 plus GPU-VA reserve/map/free (D3D12 exposes VAs to apps). | **Not in the path at all**; `OpenAdapter12` keeps refusing. |
| **Vulkan/Venus ICD** (`icd/mesa`) | Owns its **own** adapter/device/context/paging-queue and does the real submission over `D3DKMTEscape` (§3). | Unchanged — it is underneath either way. | Unchanged; it becomes the *only* kernel-facing layer for D3D12 work. |
| **DXVK / vkd3d engine** | DXVK **already** opens a **third** `D3DKMT` adapter+device in the same process — `C:` `dxvk-helios/src/dxvk/dxvk_adapter.cpp:33-40` (`D3DKMTOpenAdapterFromLuid` on `VkPhysicalDeviceVulkan11Properties::deviceLUID`) and `dxvk-helios/src/dxvk/dxvk_device.cpp:47-52` (`D3DKMTCreateDevice`). | Same. | vkd3d does the identical thing: `C:` `vkd3d-proton-helios/libs/vkd3d/d3dkmt.c:25-42` `d3d12_device_open_kmt()`. |

**The ownership rule that falls out:** in Helios the **ICD is the submission owner**, and every
layer above it holds D3DKMT objects that are *bookkeeping only*. That is already true today and
is why (b) is architecturally cheap: vkd3d's `device->kmt_local` is exactly the same kind of
inert handle DXVK's `DxvkDevice::m_kmtLocal` already is on this stack.

---

## 3. What the Venus ICD already does — proof by existence

All citations `C:` `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c` (4035 lines).

### 3.1 Adapter discovery and device/context creation
- `helios_open_d3dkmt()` at **:2457**. Calls `D3DKMTEnumAdapters2` twice (count, then list) at
  **:2465** / **:2476**.
- For each adapter it tries `D3DKMTQueryAdapterInfo(KMTQAITYPE_ADAPTERREGISTRYINFO)` (**:2501**)
  and matches `AdapterString` against `L"VIRTIO GPU"` or `L"Helios"` — **as a hint only**.
  The authoritative discriminator is a `CTX_CREATE` escape probe (**:2509-2515**), because the
  registry query "has been observed returning `STATUS_OBJECT_NAME_NOT_FOUND` (0xc0000034) for
  every adapter".
- `helios_probe_d3dkmt_adapter()` at **:1511**: `D3DKMTCreateDevice` (**:1529**) →
  `D3DKMTCreateContext` with `NodeOrdinal = 0, EngineAffinity = 0` (**:1544**) → a Helios
  `CTX_CREATE(VENUS)` escape. Failure of `CreateContext` is **non-fatal**: `*out_context` stays 0
  and the probe continues (**:1546-1552**).
- `D3DKMTCreatePagingQueue` at **:2570**; failure is explicitly non-fatal —
  *"Accounting is diagnostic metadata, never a reason to make Vulkan initialization fail"*
  (**:2575-2580**).
- Teardown mirrors it at **:3778-3805** (`DestroyPagingQueue`, `DestroyContext`, `DestroyDevice`,
  `CloseAdapter`).

### 3.2 How it submits
It does **not** use `D3DKMTRender` or `D3DKMTSubmitCommand`. Every venus command rides
`D3DKMTEscape` — `helios_escape_ex()` at **:1232-1266**:

```c
   esc.hAdapter = helios->adapter; /* KMD escape is adapter-scoped (design §8.3) */
   esc.hDevice  = helios->device;  /* pass the device too (some OS builds require it) */
   esc.hContext = helios->context;
   esc.Type     = D3DKMT_ESCAPE_DRIVERPRIVATE;
   esc.Flags.HardwareAccess = hardware_access ? 1 : 0;
```

and the header comment at **:1207-1219** carries the hard-won rule:

> `hardware_access` maps to `D3DDDI_ESCAPEFLAGS.HardwareAccess`. HardwareAccess=1 escapes
> serialize EXCLUSIVELY on the dxgkrnl adapter lock … A blocking WAIT_FENCE must therefore pass 0

with the full deadlock derivation at **:1270-1295** (`DXGADAPTER::AcquireCoreResourceExclusive` →
`DXGPROCESS::FlushAllDevice` → `VidSchWaitForCompletionEvent`; MEMORY.DMP 2026-07-07).

### 3.3 How its fences work
- `helios_wddm_sync_create()` at **:724-783** creates `D3DDDI_MONITORED_FENCE` with
  `Flags.Shared = 1` and `Flags.NtSecuritySharing = nt_shared`. There is **no legacy-fence
  fallback**, deliberately: *"monitored+Shared-without-NtSecuritySharing is rejected 0xc000000d,
  the legacy fallback engaged, and the KMT ring probe wedged in an unbounded kernel wait. Loud
  failure over fake success."* (**:773-782**).
- Signal: `D3DKMTSignalSynchronizationObjectFromCpu` first, `SignalSynchronizationObject2`
  (context-scoped) as fallback (**:886-925**).
- Wait: `D3DKMTWaitForSynchronizationObjectFromCpu`; for a bounded timeout it passes
  `hAsyncEvent` and then `WaitForSingleObject` (**:930-995**).
- Cross-process sharing uses **named NT** objects via `D3DKMTShareObjects` (**:865**, **:3551**)
  plus `D3DKMTOpenSyncObjectFromNtHandle2` with `D3DKMTOpenSyncObjectFromNtHandle` fallback
  (**:824-845**).

### 3.4 How it charges VidMm — the shadow-allocation mechanism
This is the single most load-bearing prior art for §5. `C:`
`icd/mesa/src/virtio/vulkan/vn_device_memory.c:599-616`: on **every** non-imported
`vkAllocateMemory`, on `_WIN32`, the ICD calls `vn_renderer_helios_vidmm_alloc(...)`. That
function (`vn_renderer_helios.c:2620-2708`) does:

1. `D3DKMTCreateAllocation2` with `Flags.CreateResource = 1`, `NumAllocations = 1`, and a
   `struct helios_wddm_alloc_private { .kind = HELIOS_WDDM_ALLOC_KIND_TRACKING, .ctx_id, .size,
   .blob_flags }` (**:2640-2666**).
2. `D3DKMTMakeResident` with `hPagingQueue = helios->paging_queue`,
   `PriorityList = {D3DDDI_ALLOCATIONPRIORITY_MAXIMUM}` (**:2668-2683**).
3. If `resident.PagingFenceValue != 0`, `D3DKMTWaitForSynchronizationObjectFromCpu` on
   `helios->paging_sync` (**:2685-2703**).
4. Free path (`vn_FreeMemory`, `vn_device_memory.c:648-653`) → `D3DKMTEvict` +
   `D3DKMTDestroyAllocation` (`vn_renderer_helios.c:2592-2618`).

The KMD side of that contract is `C:` `kmd_render/src/ddi/create_allocation.rs:2308-2320`:
a `TRACKING` allocation must have `size != 0`, `ctx_id != 0`, `blob_id == 0`,
`adopt_resource_id == 0`, else `STATUS_INVALID_PARAMETER` — *"A tracking allocation is
deliberately only a VidMm charge."* Budget placement is chosen at `:2500-2510`
(`TrackingBudget::Local` vs `NonLocal`, driven by `HELIOS_WDDM_BLOB_FLAG_NONLOCAL_TRACKING =
0x4000_0000`, `protocol/src/wddm.rs:60`, and the `VidMmVramMB` knob).

**Therefore: VidMm accounting for Vulkan memory already works on this stack, and it will work for
a vkd3d D3D12 client for free, because vkd3d allocates through Vulkan.**

### 3.5 The LUID identity
`C:` `vn_renderer_helios.c:3699-3720`: `helios->adapter_luid` (captured from
`D3DKMTEnumAdapters2`) is reported as `VkPhysicalDeviceIDProperties::deviceLUID` with
`has_luid = true, node_mask = 1`. The comment explicitly names the consumers: *"3DMark/UL
benchmarks, dxvk findAdapterByLuid, dxvk_adapter D3DKMTOpenAdapterFromLuid"*.
`ROADMAP.md:3128-3141` records the fix and the same-boot verification
(`deviceLUID dfb16300-00000000` == WDDM `luid 00000000:0063b1df`).

**This is why vkd3d's `d3d12_device_open_kmt()` (`d3dkmt.c:27-30`, `open_adapter.AdapterLuid =
device->adapter_luid`) will resolve to the Helios adapter at all.** Without the LUID fix it
would open nothing and `device->kmt_local` would stay 0 — which vkd3d treats as *"D3DKMT API
isn't supported"* and silently degrades (`d3dkmt.c:58-62`, `:98-103`, `:214-219`).

---

## 4. What vkd3d-proton actually uses — the whole surface, enumerated

`C:` `vkd3d-proton-helios/libs/vkd3d/d3dkmt.c` (449 lines) + `include/private/vkd3d_d3dkmt.h`
(381 lines). Built unconditionally: `libs/vkd3d/meson.build:72` lists `'d3dkmt.c'`. Everything is
inside `#ifdef _WIN32`; the `#else` arm (`:399-449`) is five `WARN("Not implemented on this
platform")` stubs.

**The 14 entry points it declares** (`include/private/vkd3d_d3dkmt.h:352-366`, verbatim list):
`D3DKMTCloseAdapter`, `D3DKMTCreateDevice`, `D3DKMTDestroyAllocation`, `D3DKMTDestroyDevice`,
`D3DKMTDestroyKeyedMutex`, `D3DKMTDestroySynchronizationObject`, `D3DKMTEscape`,
`D3DKMTOpenAdapterFromLuid`, `D3DKMTOpenResource2`, `D3DKMTOpenResourceFromNtHandle`,
`D3DKMTOpenSyncObjectFromNtHandle`, `D3DKMTQueryResourceInfo`,
`D3DKMTQueryResourceInfoFromNtHandle`, `D3DKMTShareObjects`.

**What is NOT there, and this is the finding:** no `CreateAllocation`, no `MakeResident`, no
`Evict`, no `CreatePagingQueue`, no `CreateContext`, no `SubmitCommand`, no `Render`, no
`CreateSynchronizationObject2`, no `Signal*`/`Wait*`, no GPU-VA verb, no
`QueryVideoMemoryInfo`. vkd3d-proton does **zero** kernel submission and **zero** kernel
residency management. All of that is Vulkan.

**The four call sites** (`grep` across `libs/vkd3d/`):
1. `device.c:11617` → `d3d12_device_open_kmt(device)` — one `D3DKMT` device per `ID3D12Device`.
2. `command.c:2227` → `d3d12_shared_fence_open_export_kmt(object, device)` — exports the
   `VkSemaphore` with `VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_D3D12_FENCE_BIT` via
   `vkGetSemaphoreWin32HandleKHR`, then re-opens it as a KMT sync object so *other* APIs can
   find it (`d3dkmt.c:51-77`).
3. `resource.c:4469` → `d3d12_resource_open_export_kmt(object, device, allocation)` — exports
   `VkDeviceMemory` as `VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT`, opens it with
   `D3DKMTOpenResourceFromNtHandle`, then **publishes a `struct d3dkmt_d3d12_desc` through an
   escape** (`d3dkmt.c:88-198`).
4. `device.c:7633,7704` → `D3DKMTShareObjects` for `ID3D12Device::CreateSharedHandle`;
   `device.c:7770` → `d3d12_device_open_resource_descriptor` for `OpenSharedHandle`.

### 4.1 ⚠ The Wine escape — a real, concrete blocker for interop on Helios

`H:` `vkd3d-proton-helios/include/private/vkd3d_d3dkmt.h:115-118`:

```c
typedef enum _D3DKMT_ESCAPETYPE
{
    D3DKMT_ESCAPE_UPDATE_RESOURCE_WINE = 0x80000000
} D3DKMT_ESCAPETYPE;
```

`C:` used at `d3dkmt.c:191-195`:
```c
        escape.Type = D3DKMT_ESCAPE_UPDATE_RESOURCE_WINE;
        escape.hContext = resource->kmt_local;
        escape.pPrivateDriverData = &desc;
        escape.PrivateDriverDataSize = sizeof(desc);
        D3DKMTEscape(&escape);          /* return value ignored */
```

`H:` The Windows SDK `D3DKMT_ESCAPETYPE` enum (`tmp/dx12/sdk/d3dkmthk.h:2611-2661`) contains
**no** value `0x80000000`; the defined range is 0–39 plus a few gaps. `H:` `DXGKARG_ESCAPE`
(`tmp/dxgk_bindings.rs:72481-72488`) is:

```rust
pub struct _DXGKARG_ESCAPE {
    pub hDevice: HANDLE,
    pub Flags: D3DDDI_ESCAPEFLAGS,
    pub pPrivateDriverData: *mut c_void,
    pub PrivateDriverDataSize: UINT,
    pub hContext: HANDLE,
    pub hKmdProcessHandle: HANDLE,
}
```

— i.e. **the miniport is never told the escape `Type`**. `I:` dxgkrnl therefore dispatches on
`Type` itself and only `D3DKMT_ESCAPE_DRIVERPRIVATE` reaches `DxgkDdiEscape`; a `Type` of
`0x80000000` cannot reach the Helios KMD at all and must fail (or be dropped) inside dxgkrnl.
**UNVERIFIED** — settling experiment in §7.

`C:` The **same** Wine-escape pattern already exists in the D3D11 engine:
`dxvk-helios/src/wsi/win32/wsi_window_win32.cpp:215-220`, `:259-264`, `:320-325` all issue
`escape.Type = D3DKMT_ESCAPE_SET_PRESENT_RECT_WINE` and ignore the result. So whatever happens
today for that call is what will happen for vkd3d's — which is a cheap way to settle it (§7).

**Impact if the escape is indeed a no-op on Helios:** vkd3d↔DXVK (D3D12↔D3D11) shared-resource
interop degrades. `d3d12_device_open_resource_descriptor` (`d3dkmt.c:212-449`) reads back the
runtime data through `D3DKMTQueryResourceInfo(FromNtHandle)` and switches on
`d3dkmt.dxgi.size` / `.version`; with no publisher it returns `E_INVALIDARG`/`E_NOTIMPL` and
`ID3D12Device::OpenSharedHandle` of a foreign resource fails. **Plain D3D12 rendering and
presentation are unaffected** — this is an interop-only path.

---

## 5. The double-bookkeeping problem

Under (b), the same GPU work has **two or three** D3DKMT owners in one process. This is not
hypothetical: it is **already the shipping D3D11 topology**, which is the strongest single
argument that (b) is survivable.

**Live count of D3DKMT devices per rendering process today (D3D11 path):**
1. the D3D11 runtime's device (created for the UMD; the UMD holds `pKTCallbacks` —
   `C:` `umd/src/device_funcs.rs:375`) and its context (`pfnCreateContextCb`,
   `umd/src/device_funcs.rs:1053`);
2. **DXVK's own** — `C:` `dxvk-helios/src/dxvk/dxvk_adapter.cpp:33-40` +
   `dxvk_device.cpp:47-52` (`m_kmtLocal`);
3. **the Venus ICD's own** — `C:` `vn_renderer_helios.c:2457` (adapter + device + context +
   paging queue).

vkd3d adds a fourth of the *same shape as #2*. `I:` there is nothing structurally new here.

### 5.1 Residency
- `D:` `residency-overview.md`: *"Residency in the WDDM v2 is controlled exclusively by the
  device residency requirement list."* Each `D3DKMT` device has its own list.
- Today: the ICD makes its shadow tracking allocations resident on **its** device
  (`vn_renderer_helios.c:2668-2683`); the UMD makes runtime allocations resident on the
  **runtime's** device (`pfnMakeResidentCb`, 5 uses in `umd/src/`). They never reference each
  other's allocations.
- Under (b) the D3D12 residency list is Vulkan's problem (vkd3d does not call `MakeResident`),
  so **only one list exists** — the ICD's. This is *simpler* than D3D11 today.
- Under (a) the D3D12 runtime would drive `pfnMakeResidentCb`/`pfnEvictCb` for D3D12 heaps while
  the ICD independently charges the same host memory. **That is genuine double-counting** and is
  the residency risk of strategy (a), not (b).

### 5.2 TDR / device-removed
- The KMD's TDR-adjacent DDIs are `DxgkDdiPreemptCommand` / `ResetFromTimeout` /
  `RestartFromTimeout` / `ResetEngine`, and all of them funnel through one "drop every pending
  WDDM fence" step with an explicit `AbandonOutcome` enum (`C:` `submit_command.rs:802-830`),
  counting into `ABANDONED_FENCES`.
- A TDR marks **every** device on the adapter in the process as removed, `I:` including the
  ICD's. The ICD's escape path already has a device-gone arm — `C:` `escape.rs:178`
  `escape_device_gone()`, surfaced as `out_escape_device_gone` (`escape.rs:1068`).
- Under (b), a D3D12 `DXGI_ERROR_DEVICE_REMOVED` must be synthesised by **vkd3d** from
  `VK_ERROR_DEVICE_LOST`, because vkd3d never asks dxgkrnl for device state
  (no `D3DKMTGetDeviceState` in its 14-entry surface). **UNVERIFIED** whether vkd3d's
  device-removed reporting is adequate for the D3D12 debug layer / apps; §7.

### 5.3 Fence ordering
- `D:` `context-monitoring.md` — CPU signal *"immediately unwaits any satisfied waits"*; GPU
  wait *"Command buffers submitted after the wait operation aren't scheduled for execution until
  the wait operation is satisfied."*
- The project's standing invariant (CLAUDE.md): *"A WDDM fence may wait on the frame's OWN
  boundary, never on the whole `next_wire_fence` backlog"* — implemented as `PresentWmk`.
- Under (b) a D3D12 `ID3D12Fence` is a **Vulkan timeline semaphore inside vkd3d**, not a WDDM
  monitored fence — vkd3d only exports it to KMT for *sharing* (`d3dkmt.c:51-77`). So D3D12
  fence ordering never enters VidSch. `I:` the ordering risk moves entirely into the ICD's
  existing wire-fence machinery, which is the machinery that already has the
  `VN_HELIOS_RING_WAIT_BOUND_MS` bound (memory: 67th session).
- Under (a) the D3D12 runtime *would* create one monitored fence per `ID3D12Fence` and queue
  GPU waits into the single 3D node. `I:` with one node and no hardware queues, a D3D12 app that
  fences between its DIRECT and COPY queues would serialise everything onto that node.

### 5.4 Allocation lifetime
- The KMD's ownership token is the `hDevice` the escape carries — `C:` `escape.rs:293-310`:
  *"ZERO IS NOT A NEUTRAL VALUE… `hDevice` is optional at the D3DKMTEscape API, so NULL is the
  one owner value a caller can forge"* (defect `k-capsescape-01`). Blob mappings are reclaimed in
  `DxgkDdiDestroyDevice` in the creating process, or the kernel bugchecks
  `0x76 PROCESS_HAS_LOCKED_PAGES` (`C:` `device.rs:~278-286`).
- **Risk under (b):** vkd3d's `d3d12_resource_open_export_kmt` opens a KMT *resource* on
  `device->kmt_local`, a device the KMD has no Helios context for. `I:` `DxgkDdiOpenAllocation`
  would be invoked on that device for a venus-backed allocation the ICD created on a *different*
  device. Whether `create_allocation.rs`'s open path tolerates that is **UNVERIFIED** (§7). Note
  vkd3d ignores the return of `D3DKMTOpenResourceFromNtHandle` failure (`d3dkmt.c:122-144` only
  acts on success), so a refusal is a silent degradation, not a crash.

### 5.5 VidMm budget accounting
- The reported topology is exactly two segments, `[Aperture(id 1), Bar(id 2)]`, and a
  `SupportsCpuHostAperture` segment **must be last** — enforced by construction with a named
  refusal (`C:` `ddi/segment_table.rs:1-29`, `:MAX = 2` at `:119`, `SegmentRuleViolation` codes at
  `:96-101`). `D3DKMTQueryVideoMemoryInfo` (`H:` `d3dkmthk.h:5271-5283`, fields `Budget`,
  `CurrentUsage`, `CurrentReservation`, `AvailableForReservation`) is computed by VidMm from that
  table plus per-process usage.
- The ICD's shadow allocations are what make Vulkan memory visible in those numbers (§3.4), and
  `tools/vram_report_probe.cpp` exists specifically to compare "DXGI/VidMm numbers with the Vulkan
  heaps exposed by Venus" (`tools/vram_report_probe.cpp:1-19`).
- **Risk:** under (b) a D3D12 app reads `IDXGIAdapter3::QueryVideoMemoryInfo` and sizes its
  residency budget from it. If the ICD's tracking charge and the app's expectation disagree, the
  app either over-commits or under-uses. **UNVERIFIED** whether the two agree for a D3D12
  workload; §7.

### 5.6 Per-process / per-adapter table limits
Hard ceilings in the KMD's virtio resource tables (`C:` `kmd_render/src/virtio/gpu/mod.rs`):
`MAX_BLOBS = 8192` (`:125`), `MAX_RESOURCES = 16384` (`:129`), `MAX_CONTEXTS = 1024` (`:131`);
CPU mappings `MAX_MAPPINGS = 8192` (`C:` `kmd_render/src/mapping.rs:41`) — the comment there
records that the pre-2026-07-03 smaller cap **was** the Doom level-load fatal. These are
**adapter-global, not per-process** (`mapping.rs:34`: *"adapter-global: dwm + WUDFHost + every
game share it"*). `I:` a D3D12 title that allocates far more `VkDeviceMemory` objects than a
D3D11 one is the realistic way to hit `MAX_BLOBS`; the exhaustion signature is a named counter,
not a crash.

### 5.7 Summary of what a D3D12 design must do

| Hazard | D3D11 today | (a) native UMD | (b) vkd3d |
|---|---|---|---|
| Two residency lists for one byte of host memory | avoided — the lists are disjoint | **must be solved** | avoided (only the ICD has a list) |
| Fence ordering across owners | solved by `PresentWmk` frame-boundary rule | new monitored fences per `ID3D12Fence`, one node | no WDDM fences involved |
| Allocation lifetime across devices | UMD ↔ ICD are disjoint | disjoint | **new**: vkd3d opens KMT resources on its own device (§5.4) |
| VidMm budget truth | ICD shadow allocations (§3.4) | double-charge risk | inherits ICD's charge |
| Device-removed propagation | dxgkrnl marks every device | dxgkrnl | vkd3d must synthesise from `VK_ERROR_DEVICE_LOST` |
| Table caps | shared, adapter-global | same | same, higher pressure |

---

## 6. Escapes

### 6.1 What exists
`C:` `protocol/src/escape.rs` defines 17 live verbs plus one dead one:

| Verb | Value | Verb | Value |
|---|---|---|---|
| `SUBMIT_VENUS` | 0x0001 | `QUERY_STATS` | 0x000A |
| `CTX_CREATE` | 0x0002 | `REGISTER_FENCE_EVENT` | 0x000B |
| `CTX_DESTROY` | 0x0003 | `UNREGISTER_FENCE_EVENT` | 0x000C |
| `ALLOC_BLOB` | 0x0004 | `QUERY_SCANOUT` | 0x000D |
| `MAP_BLOB` | 0x0005 | `MAP_READ_LEDGER` | 0x000E |
| `WAIT_FENCE` | 0x0006 | `SCANOUT_EVENT` | 0x000F |
| `PRESENT_BLOB` | 0x0007 **(dead — lands in the unknown-verb arm)** | `PRESENT_STREAM` | 0x0010 |
| `RELEASE_BLOB` | 0x0008 | `QUERY_SCANOUT_TIMELINE` | 0x0011 |
| `ATTACH_RESOURCE` | 0x0009 | | |

Header: `HELIOS_ESCAPE_MAGIC = 0x4845_4C53`, `HELIOS_ESCAPE_VERSION = 1`
(`protocol/src/escape.rs:25,27`); 16-byte `HeliosEscapeHeader`
(`_Static_assert(sizeof(struct helios_escape_header) == 16)`, `vn_renderer_helios.c:282`).
Entry point `C:` `kmd_render/src/ddi/escape.rs:254`; dispatch `match hdr.cmd_type` at `:321`.
Unknown verbs return `STATUS_NOT_IMPLEMENTED` **and are counted** (`ESCAPE_UNKNOWN_VERB`
incremented at `:384`) — *"an unhandled verb is how an ICD/KMD protocol skew presents"*.

### 6.2 The rules the project already paid for
1. **PASSIVE only.** `C:` `escape.rs:319` (`PassiveLevel::assume()`), with the SAFETY comment
   immediately above it: *"`DxgkDdiEscape` is documented 'IRQL: PASSIVE_LEVEL' … a DISPATCH
   arrival here would already be a deadlock rather than a new one. Counted by `IrqlBad` if that
   ever changes."*
2. **`HardwareAccess` must be 0.** `C:` `escape.rs:191-192` names the bits
   (bit 0 `ESCAPE_FLAG_HARDWARE_ACCESS`, bit 3 `ESCAPE_FLAG_NO_ADAPTER_SYNC`), `:202-204` are the
   counters, `:214` is `count_escape_flag`, and `:272-279` reads `args.Flags.__bindgen_anon_1.Value`
   once at PASSIVE outside any lock. They **count**, they do not yet refuse — deliberately:
   *"refusing is a behaviour change that must be evidence-gated on these reading 0 across a
   desktop session plus a game run"* (`:196-201`). The deadlock this guards against is fully
   derived at `vn_renderer_helios.c:1270-1295`.
3. **Ownership is `hDevice`, and NULL is forgeable.** `C:` `escape.rs:312`
   (`DeviceOwner::new(args.hDevice as usize)`) with the derivation at `:295-311` (§5.4). Every
   ownership-bearing verb takes a `DeviceOwner` so the null case is answered at exactly one site;
   refusals counted as `EscNoDev` (`refuse_no_device`, `:243`), foreign-context refusals as
   `EscCtxOwn` (`refuse_foreign_context`, `:233`).
4. **Buffer validation is per-arm.** `C:` `escape.rs:280-292`: null check, `buf_len >=
   sizeof(header)`, then `hdr.is_valid() && hdr.size as usize <= buf_len` with
   `ESCAPE_BAD_HEADER`.
5. **Device-gone is a first-class arm.** `C:` `escape.rs:178` `escape_device_gone()`, reported
   as `out_escape_device_gone` (`:1068`).

### 6.3 Would D3D12 need new escapes?
- **Under (b): no new escapes for rendering.** vkd3d's work reaches the KMD as ordinary
  `SUBMIT_VENUS` from the ICD. What (b) *might* want is:
  - a **`D3DKMT_ESCAPE_UPDATE_RESOURCE_WINE` equivalent** for cross-API shared-resource
    descriptors (§4.1) — but note it cannot be implemented as a Helios escape verb, because
    dxgkrnl owns the `Type` dispatch and the miniport never sees it (`DXGKARG_ESCAPE` has no
    `Type` field). Any Helios answer must be a `DRIVERPRIVATE` verb with a *new* protocol op,
    and **both** vkd3d-proton and dxvk-helios would have to be patched to use it. That is a fork
    change in two submodules, and it is the only place in this lane where forking vkd3d-proton is
    clearly justified.
  - a present-path hook: `PRESENT_STREAM` (0x0010) already exists and is the mechanism the D3D11
    path uses to name a frame boundary; a Vulkan-class D3D12 client would need the equivalent
    hand-off. That is lane R7's problem, not R6's.
- **Under (a): probably none.** A native D3D12 UMD reaches the kernel through
  `pKTCallbacks->pfnEscapeCb`, which is the same `DxgkDdiEscape` the D3D11 UMD already uses
  (`C:` `umd/src/scanout_acquire.rs:173-200`, and the note at `umd/src/adapter.rs:221` that
  `pfnEscapeCb`'s first argument is the **runtime** adapter handle).

---

## 7. UNVERIFIED, with settling experiments

Each is one command or one small probe under `tools/`. None requires a build of the driver, a
reboot, or a VM relaunch.

| # | Question | Settling experiment |
|---|---|---|
| U1 | Does `D3DKMTEscape` with a **non-SDK `Type`** (`0x80000000` = `D3DKMT_ESCAPE_UPDATE_RESOURCE_WINE`) reach `DxgkDdiEscape`, get rejected by dxgkrnl, or silently succeed? Everything in §4.1 hangs on this. | Add ~20 lines to `tools/escape_owner_probe.c`: open the Helios adapter + device, issue one `D3DKMT_ESCAPE` with `Type = (D3DKMT_ESCAPETYPE)0x80000000` and a 16-byte buffer, print the `NTSTATUS`; then snapshot `EscUnk`/`ESCAPE_UNKNOWN_VERB` via `tools/kmd-counter-snapshot.ps1` before/after to see whether it reached the KMD at all. Repeat with `D3DKMT_ESCAPE_SET_PRESENT_RECT_WINE` (which DXVK already issues live) as the control. |
| U2 | Does `D3DKMTOpenResourceFromNtHandle` on a **second** `D3DKMT` device (vkd3d's `kmt_local`), for a venus-backed allocation created by the ICD on a **different** device, succeed — and what does `DxgkDdiOpenAllocation` do with it? | Extend `tools/d3dkmt_alloc_probe.c`: create the allocation on device A, `D3DKMTShareObjects` it, `D3DKMTCreateDevice` a second device B on the same adapter, `D3DKMTOpenResourceFromNtHandle` on B, print status. Snapshot the `create_allocation.rs` open-path counters around it. |
| U3 | Do `QueryVideoMemoryInfo` `Budget`/`CurrentUsage` stay coherent when a **Vulkan** client allocates the way a D3D12 title would (many mid-size device-local allocations)? | `tools/vram_report_probe.exe --vulkan-allocs N` already does exactly this (`tools/vram_report_probe.cpp:15-18`); run it at N = 64/256 and compare against `tools/vidmm_tracking_probe.exe`. Read-only. |
| U4 | Does dxgkrnl let a **D3D12 device** be created at all against a single-node, `MultiEngineAware`-only adapter, and does it synthesise COPY/COMPUTE queues over node 0? | Not answerable until `OpenAdapter12` stops refusing (strategy (a)) — **or**, for (b), it is moot because vkd3d never asks. For (a), the cheap precursor is a WDK-doc read on the minimum node/engine requirement for D3D12; nothing in `windows-driver-docs-pr/display/` states one (searched). |
| U5 | Does `D3DKMTInvalidateCache` / `pfnInvalidateCacheCb` ever fire on this adapter, and would it need a KMD path? | `grep -rn "InvalidateCache" kmd_render/src/` → no hits; there is no miniport slot for it in the 187-slot list either. Confirm it is purely a VidMm cache-maintenance verb by a WDK doc read; low priority — no Helios caller exists today (`umd/src/` has 0 uses). |
| U6 | How does vkd3d report device-removed, and is it adequate for a D3D12 app / the debug layer? | Read `vkd3d-proton-helios/libs/vkd3d/device.c` for `GetDeviceRemovedReason` and the `VK_ERROR_DEVICE_LOST` handling (a source read, not an experiment). Not done in this lane — belongs to R3. |
| U7 | Is `dxvk-helios`'s `D3DKMT_ESCAPE_SET_PRESENT_RECT_WINE` currently a live no-op on Helios (i.e. is the Wine-escape class already known-dead here)? | Same probe as U1's control arm; alternatively read `EscHwA`/`EscUnk`/`EscNoDev` from a running DWM session with `tools/kmd-counter-snapshot.ps1` and check whether anything moved. |
| U8 | Does `DxgkDdiOpenAllocation` tolerate an open that names **no** Helios ctx? | Covered by U2's counter snapshot; the KMD arm to watch is `create_allocation.rs:2849-3060`. |

---

## 8. Direct implications for the Helios D3D12 plan

1. **The D3DKMT surface does not select between (a) and (b) — it strongly favours (b).** Under
   (b), 100% of the kernel-facing work is code that already runs (§3), and vkd3d adds 14 calls in
   one file, none of them in a rendering path (§4).
2. **`OpenAdapter12` can stay refusing under (b) forever, honestly**, because no D3D12 UMD DDI is
   in the path. The refusal comment should then cite DX12.md §2(b).
3. **The one place a vkd3d fork is technically justified by this lane** is the Wine escape
   (§4.1/§6.3): cross-API D3D12↔D3D11 shared resources need a publisher, and Helios must supply a
   `DRIVERPRIVATE` verb because dxgkrnl owns the escape `Type` dispatch. That is a two-submodule
   change and should be scoped only after U1 settles.
4. **The single-node/no-HW-queue posture is a real D3D12 constraint but is not a D3DKMT problem
   under (b)**, because vkd3d never touches `CreateHwQueue`/`SubmitCommandToHwQueue`. Under (a)
   it is a first-class blocker: the KMD refuses HW queues at creation with `HwQRef`
   (`scheduler.rs:180-187`) and that refusal is deliberate and correct.
5. **VidMm budget for D3D12 comes for free under (b)** via the ICD's per-`VkDeviceMemory` shadow
   allocation (§3.4). Nothing new is needed; what is needed is a *measurement* (U3) that the
   numbers a D3D12 title reads are sane.
6. **Do not add a second residency owner.** The whole double-bookkeeping hazard list (§5) reduces
   to one item under (b) (§5.4, allocation lifetime across devices) and expands to five under (a).
7. **Any new escape verb inherits four rules** (§6.2): PASSIVE only, `HardwareAccess = 0`,
   `hDevice`-derived ownership with the NULL-is-forgeable guard, and per-arm size validation with
   a named counter for every refusal.

---

## 9. Appendix — the 103 unset miniport slots

Reproduce with:
```
awk '/pub struct _DRIVER_INITIALIZATION_DATA/,/^}/' tmp/dxgk_bindings.rs \
  | grep -o "pub Dxgk[A-Za-z0-9_]*" | sed 's/pub //' | sort > /tmp/all_ddi.txt
grep -o "data\.Dxgk[A-Za-z0-9_]*" kmd_render/src/lib.rs | sed 's/data\.//' | sort -u > /tmp/set_ddi.txt
comm -23 /tmp/all_ddi.txt /tmp/set_ddi.txt
```
Output (103 names), grouped by relevance to this lane:

- **Fences / sync:** `SignalMonitoredFence`, `CreateNativeFence`, `DestroyNativeFence`,
  `OpenNativeFence`, `CloseNativeFence`, `SetNativeFenceLogBuffer`, `UpdateNativeFenceLogs`,
  `UpdateMonitoredValues`, `UpdateCurrentValuesFromCpu`, `CreateCpuEvent`, `DestroyCpuEvent`.
- **User-mode submission:** `CreateDoorbell`, `ConnectDoorbell`, `DisconnectDoorbell`,
  `DestroyDoorbell`, `NotifyWorkSubmission`, `SubmitRender` (typed `PVOID` in the bindings,
  `tmp/dxgk_bindings.rs:95264`), `ValidateSubmitCommand`.
- **Memory / paging:** `CreateAllocation2` (typed `PVOID`, `:95265`), `SetAllocationBackingStore`,
  `UpdatePageTable`, `UpdatePageDirectory`, `MovePageDirectory`, `DescribePageTable`,
  `AcquireSwizzlingRange`, `ReleaseSwizzlingRange`, `ValidateUpdateAllocationProperty`,
  `CreateMemoryBasis`, `DestroyMemoryBasis`.
- **Scheduling / context:** `SetContextSchedulingProperties`, `SuspendContext`, `ResumeContext`,
  `SetupPriorityBands`, `NotifyContextPriorityChange`, `SetSchedulingLogBuffer`,
  `UpdateHwContextState`, `ResetHwEngine`, `ResumeHwEngine`.
- **MPO / display:** `CheckMultiPlaneOverlaySupport{,2,3}`,
  `SetVidPnSourceAddressWithMultiPlaneOverlay{,2,3}`, `GetMultiPlaneOverlayCaps`,
  `PostMultiPlaneOverlayPresent`, `GetPostCompositionCaps`, `CreateOverlay`, `UpdateOverlay`,
  `FlipOverlay`, `DestroyOverlay`, `CancelFlips`, `CancelQueuedFlips`, `SetFlipQueueLogBuffer`,
  `UpdateFlipQueueLog`, `SetInterruptTargetPresentId`, `NotifyFocusPresent`.
- **Protected content:** `CreateProtectedSession`, `DestroyProtectedSession`,
  `SetVideoProtectedRegion`.
- **Live migration / hot update:** `PrepareLiveMigration`, `EndLiveMigration`,
  `Save{Immutable,Mutable}MigrationData`, `Restore{Immutable,Mutable}MigrationData`,
  `SaveMemoryForHotUpdate`, `RestoreMemoryForHotUpdate`, `SetVirtualFunctionPauseState`,
  `SetVirtualGpuResources2`, `WriteVirtualizedInterrupt`.
- **Diagnostics / misc:** `CollectDbgInfo2`, `CollectDiagnosticInfo`,
  `ControlDiagnosticReporting`, `QueryDiagnosticTypesSupport`, `ControlInterrupt2`,
  `ControlInterrupt3`, `ControlModeBehavior`, `DisplayDetectControl`, `BeginExclusiveAccess`,
  `EndExclusiveAccess`, `LinkDevice`, `NotifySurpriseRemoval`, `QueryConnectionChange`,
  `QueryDirtyBitData`, `StartDirtyTracking`, `StopDirtyTracking`, `StopCapture`,
  `RecommendVidPnTopology`, `ResetDisplayEngine`, `SetDisplayPrivateDriverFormat`, `SetPalette`,
  `SetPowerComponentFState`, `SetPowerPState`, `SetTargetAdjustedColorimetry{,2}`,
  `SetTargetAnalogCopyProtection`, `SetTargetContentType`, `SetTargetGamma`,
  `SetTimingsFromVidPn`, `SetTrackedWorkloadPowerLevel`, `CreatePeriodicFrameNotification`,
  `DestroyPeriodicFrameNotification`.
