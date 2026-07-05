# WDDM_FAKE_VIDMM_RESEARCH.md — Implementing a fake-but-coherent WDDM GpuMmu memory model backed by venus

**Project:** Helios vGPU — `helios_kmd_render`, a WDDM **render-only** miniport over the virtio-gpu PCI device (`VEN_1AF4 & DEV_1050`), backed by **venus** (Vulkan-command passthrough to a host GPU over virtio-gpu).

**Status:** This is a **Step-1 RESEARCH deliverable**. It contains no driver code and changes no source. A separate **Step-2** session implements from it. It is intended to be complete enough that Step 2 never has to re-research the WDDM memory-model DDI contract, the exact Rust types, the viogpu3d template, or the coherence design.

**The locked decision (do not re-litigate — see `IDD_HELIOS_RENDER_PLAN.md` §4–5 and the `wddm-hwaccel-desktop-is-the-goal` memory).** To get a *hardware-accelerated Windows desktop* into Looking Glass, make DWM composite the **whole desktop ON Helios** (venus → host GPU); the Looking Glass IDD then captures the OS-composed frame via the standard `IddCxSwapChainReleaseAndAcquireBuffer` path. The mechanism is a **fake-but-coherent WDDM GpuMmu memory model**: the host GPU owns the *real* MMU, so the guest GpuMmu (GPU virtual addresses + page tables) is **decorative** — venus addresses resources by opaque id and the host GPU never reads guest page tables, so VidMm cannot verify the addressing. WDDM allocations map to venus resources. Coherence is required at exactly three points: **fences** (the venus submit must drive the WDDM fence), **CPU/IDD readback** (host-visible venus blobs), and **residency** (over-size the segment so nothing evicts). The lighter cross-adapter / DWM-on-WARP path was considered and **rejected** (it leaves the desktop software-composited).

---

## TL;DR for the implementer — the decisions this research establishes

1. **The GpuMmu opt-in is not where you'd guess.** `DXGK_VIDMMCAPS` is **not** a standalone query — it is an inline member of `DXGK_DRIVERCAPS` named `MemoryManagementCaps` (Section A1, B1). The memory **model** (GpuMmu vs IoMmu) is selected **per engine node** via `DXGK_NODEMETADATA::GpuMmuSupported` / `IoMmuSupported` (booleans), and the GpuMmu geometry is then described by a follow-up `DXGKQAITYPE_GPUMMUCAPS = 13` query (Section A3, A9). There is **no** `IOMMUCAPS` query and no `_DXGK_IOMMUCAPS` struct — IoMmu is just a boolean. Helios currently declares **neither** model (`get_node_metadata` leaves `GpuMmuSupported` zeroed — Section E).

2. **Recommendation: GpuMmu = TRUE, IoMmu = FALSE, `ParavirtualizationSupported` = FALSE** (Section G). Choosing GpuMmu is mandated by the render-only/proxied precedent (GPU-PV: *"The driver must support GpuMmu"*), matched by viogpu3d's implicit default, and favored by the asymmetric bindgen surface. **Do NOT set `ParavirtualizationSupported`** (bit 10 of `DXGK_VIDMMCAPS`): it is a *host-KMD GPU-PV* contract whose DDIs Helios does not implement.

3. **The decorative-GpuMmu bet is within the letter of the WDDM contract** (Section A3.7). The docs state the GPU page-table hardware format is *"unknown to VidMm and is abstracted through DDIs"* and the KMD merely *"uses this information to build hardware-specific page table entries"* — a KMD that translates `DXGK_PTE` into *nothing* is compliant. What is **not** optional is the *structural bookkeeping* VidMm maintains regardless: declared caps must be self-consistent, the segment that backs page-table allocations (`PageTableSegmentId`) must be real and never evict, and every page-table DDI (`QueryAdapterInfo` level descs, `GetRootPageTableSize`, `SetRootPageTable`, `BuildPagingBuffer`/`UpdatePageTable`) must return success and consistent values even when their hardware effect is nil. **The one open unknown (Section H, risk #1): does VidMm ever read back PTE content it wrote?** The docs give no indication it does; the Step-2 kernel-debugger session must confirm.

4. **viogpu3d — the closest *working* virtio-gpu WDDM driver — is the structural template but a different memory model.** It declares **no** GpuMmu/IoMmu/Paravirtualization, runs as **WDDM 1.3**, exposes a single **aperture** segment (`BaseAddress = 0xC0000000`, `Aperture = TRUE`, `CacheCoherent = TRUE`, `CpuVisible = FALSE`, `DirectFlip = TRUE`), and moves memory with virtio-gpu **TRANSFER** copies (a copy model). Helios reuses its DDI-table shape, allocation lifecycle, and **fence wiring**, but replaces transfer-queue backing with **zero-copy host-visible blobs** (Sections A8, A9, D).

5. **The fence is the #1 coherence task** (Sections A6, C). Today `dxgkddi_submit_command` signals `DXGK_INTERRUPT_DMA_COMPLETED` **immediately and synchronously** — a null engine that asserts completion before any venus work runs. viogpu3d's template: `SubmitCommand` only *queues*; the fence is signaled from the **virtio used-ring completion** via a real ISR + DPC. Helios's `dxgkddi_interrupt_routine` and `dxgkddi_dpc_routine` are **stubs** and must become real. There is an architectural mismatch to resolve (venus submits out-of-band via `D3DKMTEscape`, but the WDDM fence is driven by `DxgkDdiSubmitCommand`); **the recommended fix is to route the desktop/composition venus stream through `DxgkDdiRender` + `DxgkDdiSubmitCommand`** so the WDDM `SubmissionFenceId` is authoritative (Section C.1).

6. **`DxgkDdiGetStandardAllocationDriverData` is `STATUS_NOT_IMPLEMENTED` today — the #1 *functional* gap for DWM** (Sections A5, E). DWM/the runtime allocate their composition surfaces (shared primary, shadow, staging, GDI redirection) through it. It must be implemented.

7. **The segment-acceptance gate is the early go/no-go.** A CPU-visible **memory** segment (the zero-copy ideal) is currently *rejected by VidMm right after `DxgkDdiCreateDevice`* because no memory model is declared (verbatim in `query_adapter_info.rs:249–262`). The fake-GpuMmu declaration is precisely what should make a real memory segment acceptable; until then the driver falls back to a placeholder CpuVisible **aperture** segment to stay at Code 0 (Sections A2, A7-C). Pin every allocation (`EvictionSegmentSet = 0`), over-size the segment, and do **not** set `DXGK_SEGMENTFLAGS2::ApplicationTarget`, so nothing ever evicts (Section A7-C-residency).

8. **VRD-pairing means there is no soft launch** (Sections A9.7, H). Because a render-only adapter paired with a display-only adapter does *all* UI rendering, DWM runs `D3D11CreateDevice` and per-frame compositing **on Helios** the instant it binds at Code 0. Any defect is a **fatal DWM fail-fast crash-loop**, not a degraded-but-running state. The `GpuVirtualizationFlags` 0x8 "disable pairing" lever does **not** apply (Helios is not a paravirtual adapter — confirmed dead). The only proof of success is **Looking-Glass frames arriving on the Linux host**, not merely Code 0.

9. **IDD changes (Section F):** drop the WARP force-select in `CIndirectDeviceContext.cpp:182–213` so the OS composites on Helios, and replace the IDD's **D3D12** copy path with **D3D11** (Helios has no D3D12), or read the composed surface via the existing (gated-off) `CHeliosSink` Vulkan import.

---

## How this document was produced (methodology & confidence)

This doc was assembled from a 30-agent research workflow: **15 parallel mining agents** each owned a vertical slice (a WDDM doc cluster + the corresponding viogpu3d code + the matching bindgen types + the current `kmd_render` state), and **15 adversarial citation-verifier agents** independently re-read the cited files to catch hallucinated line numbers, wrong struct fields, and fabricated quotes. **Result: zero `MAJOR_ISSUES` across all 15 sections**; six sections had `MINOR_ISSUES` (line-number ±1/±2 nits and two field-name fixes), **all of which were applied** before assembly. The cross-cutting design sections (C.1 fence decision, H risks) and this front-matter were written by the orchestrator against first-hand reads of `submit_command.rs`, `build_paging_buffer.rs`, `query_adapter_info.rs`, `create_allocation.rs`, `lib.rs`, `protocol/src/wddm.rs`, `viogpu_command.cpp`, and `viogpu_adapter.cpp`.

**Source of authority for types:** all verbatim Rust types are quoted from `dxgk_bindings_dump.rs` — the generated `$OUT_DIR/dxgk_bindings.rs` from the live `.72` `kmd_render` build (bindgen 0.71 over WDK 10.0.26100 `dispmprt.h` + `d3dkmddi.h`, `DXGKDDI_INTERFACE_VERSION` = WDDM 3.2 / `0x11007`). That file is a temporary scratch copy on the repo root for this session only; Step 2 regenerates it from its own build (the line numbers below are valid for that exact dump and should be re-confirmed against a fresh build, but the field names/types are ABI-stable for WDK 10.0.26100).

**Citation conventions:** code is cited as `path:line`; WDDM conceptual docs are cited by `<name>.md:line` (they live under `windows-driver-docs-research-only/windows-driver-docs-pr/display/`); bindgen types are cited as `dxgk_bindings_dump.rs:line`. bindgen lowers anonymous unions to `__BindgenUnionField<T>` (access via `.as_ref()`/`.as_mut()`) and bitfields to `__BindgenBitfieldUnit<[u8; N]>` with `X()`/`set_X()` accessor methods — Step 2 **must** use these exact accessors; the doc shows them inline.

---

## Table of contents

- **Section A — The full WDDM GpuMmu DDI contract** (what each DDI is, when VidMm/dxgkrnl calls it, the viogpu3d template, the current Helios state)
  - A0. viogpu3d registered DDI table (the structural anchor)
  - A1. Adapter caps — `DXGK_DRIVERCAPS` / `DXGK_VIDMMCAPS` / `DXGK_GPUMMUCAPS`
  - A2. Memory segments — `QUERYSEGMENT3/4`, `DXGK_SEGMENTDESCRIPTOR4`
  - A3. The GpuMmu VA model — per-process VA spaces, page tables, `DXGK_PTE`
  - A4. `DxgkDdiBuildPagingBuffer` — the operation union & page-table path
  - A5. Allocation DDIs — Create/Describe/Open/GetStandardAllocationDriverData
  - A6. Command / submission / fence / interrupt path
  - A7-lockmap. CPU-visible Lock/Map path (the readback half lives in Section C)
  - A8. Present / VidPN DDIs Helios can omit
- **Section B — Verbatim Rust types & field layouts** (B1 caps/segments · B2 alloc/paging/PTE · B3 command/device/fence)
- **Section C — Coherence design** (C.1 fence decision · C.2 readback · C.3 residency)
- **Section D — Venus mapping** (WDDM allocation → venus resource)
- **Section E — Current `kmd_render` per-DDI state** (real / partial / stub / missing)
- **Section F — IDD side** (selecting Helios; reading the composed frame; no D3D12)
- **Section G — GPU-PV vs viogpu3d vs ours; the GpuMmu-vs-IoMmu recommendation; VRD pairing**
- **Section H — Ranked open questions & risks for Step 2** (+ the kernel-debugger plan and implementation order)

---

## Section A — The full WDDM GpuMmu DDI contract

This section walks the DDI surface a WDDM render adapter must present so VidMm/dxgkrnl accepts it and DWM can composite on it: the registered DDI table (A0, the concrete anchor — the exact set of callbacks a *working* virtio-gpu WDDM driver provides), adapter capabilities and the memory-model opt-in (A1), memory segments (A2), the GpuMmu virtual-address model and page tables (A3), `DxgkDdiBuildPagingBuffer` (A4), the allocation lifecycle DDIs (A5), and the command/submission/fence/interrupt path (A6). It closes with the present/VidPN DDIs Helios can omit (A8, since Helios is render-only and the IDD owns display). For each DDI: its purpose and when it is called, the viogpu3d implementation as template (`path:line` + quoted code), the exact bindgen field names, and the current Helios state.

The CPU-visible **Lock/Map** path that Section A's scope also names is documented in **Section C.2** — it is inseparable from the readback-coherence design, so it lives there. Exact Rust struct/field layouts for every type named in this section are in **Section B**; A-subsections quote the specific fields they discuss inline.

---

### A0. viogpu3d registered DDI table (driver.cpp/.h) — the structural anchor

This section is the structural anchor for the whole doc: it shows the **complete** set of WDDM DDIs that a *working* virtio-gpu WDDM 3D driver (viogpu3d, Red Hat, Vadim Rozenfeld) registers with `dxgkrnl` and how the registration table is wired. Step 2 should mirror this table shape and then prune/replace the display+VidPN half (Helios is render-only and uses the Looking Glass IDD for scanout), and replace the TRANSFER-queue allocation/paging/submit bodies with venus-id + host-visible-BAR bodies.

**Initialization model used (important):** viogpu3d initializes as a **full WDDM render+display miniport** via `DxgkInitialize` — it does **NOT** use `DxgkInitializeDisplayOnlyDriver`. Confirmed by grep over the whole driver directory: the only `DxgkInitialize*` reference is the single `DxgkInitialize` call, and the interface version is `DXGKDDI_INTERFACE_VERSION_WDDM1_3`:

```
/home/rupansh/helios-vgpu/virtio-research-only-3d/viogpu/viogpu3d/driver.cpp:83:    InitialData.Version = DXGKDDI_INTERFACE_VERSION_WDDM1_3;
/home/rupansh/helios-vgpu/virtio-research-only-3d/viogpu/viogpu3d/driver.cpp:152:    NTSTATUS Status = DxgkInitialize(pDriverObject, pRegistryPath, &InitialData);
```

So this template registers the *render* DDIs (CreateDevice/CreateContext/CreateAllocation/Render/Patch/SubmitCommand/BuildPagingBuffer/QueryCurrentFence/etc.) **and** the *display/VidPN* DDIs (IsSupportedVidPn/CommitVidPn/SetVidPnSourceAddress/SystemDisplay*/pointer/etc.) in one `DRIVER_INITIALIZATION_DATA`. Helios will keep the render half and drop the display/VidPN half.

#### 1. `DriverEntry` and the full `DRIVER_INITIALIZATION_DATA` fill — verbatim

`driver.cpp:76-161` (the `DRIVER_INITIALIZATION_DATA InitialData` is the WDDM equivalent of the "DDI table"; there is no separate `DXGKRNL_INTERFACE` table to fill in `DriverEntry` — `DXGKRNL_INTERFACE` is the *callback* table that dxgkrnl hands the driver later, in `StartDevice`):

```cpp
// driver.cpp:76
extern "C" NTSTATUS DriverEntry(_In_ DRIVER_OBJECT *pDriverObject, _In_ UNICODE_STRING *pRegistryPath)
{
    PAGED_CODE();
    WPP_INIT_TRACING(pDriverObject, pRegistryPath)
    DbgPrint(TRACE_LEVEL_FATAL, ("---> VIOGPU FULL build on on %s %s\n", __DATE__, __TIME__));
    DRIVER_INITIALIZATION_DATA InitialData = {0};

    InitialData.Version = DXGKDDI_INTERFACE_VERSION_WDDM1_3;             // :83

    InitialData.DxgkDdiAddDevice = VioGpu3DAddDevice;                    // :85
    InitialData.DxgkDdiStartDevice = VioGpu3DStartDevice;               // :86
    InitialData.DxgkDdiStopDevice = VioGpu3DStopDevice;                 // :87
    InitialData.DxgkDdiRemoveDevice = VioGpu3DRemoveDevice;             // :88

    InitialData.DxgkDdiDispatchIoRequest = VioGpu3DDispatchIoRequest;   // :90
    InitialData.DxgkDdiInterruptRoutine = VioGpu3DInterruptRoutine;     // :91
    InitialData.DxgkDdiDpcRoutine = VioGpu3DDpcRoutine;                 // :92

    InitialData.DxgkDdiQueryChildRelations = VioGpu3DQueryChildRelations;   // :94
    InitialData.DxgkDdiQueryChildStatus = VioGpu3DQueryChildStatus;         // :95
    InitialData.DxgkDdiQueryDeviceDescriptor = VioGpu3DQueryDeviceDescriptor; // :96
    InitialData.DxgkDdiSetPowerState = VioGpu3DSetPowerState;               // :97
    InitialData.DxgkDdiResetDevice = VioGpu3DResetDevice;                   // :98
    InitialData.DxgkDdiUnload = VioGpu3DUnload;                             // :99

    InitialData.DxgkDdiQueryAdapterInfo = VioGpu3DQueryAdapterInfo;                         // :101
    InitialData.DxgkDdiEscape = VioGpu3DEscape;                                             // :102
    InitialData.DxgkDdiCreateAllocation = VioGpu3DCreateAllocation;                         // :103
    InitialData.DxgkDdiOpenAllocation = VioGpu3DOpenAllocation;                             // :104
    InitialData.DxgkDdiCloseAllocation = VioGpu3DCloseAllocation;                           // :105
    InitialData.DxgkDdiDescribeAllocation = VioGpu3DDescribeAllocation;                     // :106
    InitialData.DxgkDdiDestroyAllocation = VioGpu3DDestroyAllocation;                       // :107
    InitialData.DxgkDdiGetStandardAllocationDriverData = VioGpu3DGetStandardAllocationDriverData; // :108
    InitialData.DxgkDdiBuildPagingBuffer = VioGpu3DBuildPagingBuffer;                       // :109

    InitialData.DxgkDdiCreateContext = VioGpu3DDdiCreateContext;         // :111
    InitialData.DxgkDdiDestroyContext = VioGpu3DDdiDestroyContext;       // :112

    InitialData.DxgkDdiPresent = VioGpu3DPresent;                       // :114
    InitialData.DxgkDdiRender = VioGpu3DRender;                         // :115
    InitialData.DxgkDdiPatch = VioGpu3DPatch;                           // :116
    InitialData.DxgkDdiSubmitCommand = VioGpu3DSubmitCommand;           // :117

    InitialData.DxgkDdiSetPointerPosition = VioGpu3DSetPointerPosition; // :119
    InitialData.DxgkDdiSetPointerShape = VioGpu3DSetPointerShape;       // :120
    InitialData.DxgkDdiIsSupportedVidPn = VioGpu3DIsSupportedVidPn;     // :121
    InitialData.DxgkDdiRecommendFunctionalVidPn = VioGpu3DRecommendFunctionalVidPn;     // :122
    InitialData.DxgkDdiEnumVidPnCofuncModality = VioGpu3DEnumVidPnCofuncModality;       // :123
    InitialData.DxgkDdiSetVidPnSourceVisibility = VioGpu3DSetVidPnSourceVisibility;     // :124
    InitialData.DxgkDdiCommitVidPn = VioGpu3DCommitVidPn;               // :125
    InitialData.DxgkDdiUpdateActiveVidPnPresentPath = VioGpu3DUpdateActiveVidPnPresentPath; // :126
    InitialData.DxgkDdiSetVidPnSourceAddress = VioGpu3DSetVidPnSourceAddress;           // :127
    InitialData.DxgkDdiRecommendMonitorModes = VioGpu3DRecommendMonitorModes;           // :128
    InitialData.DxgkDdiQueryVidPnHWCapability = VioGpu3DQueryVidPnHWCapability;          // :129
    InitialData.DxgkDdiSystemDisplayEnable = VioGpu3DSystemDisplayEnable;               // :130
    InitialData.DxgkDdiSystemDisplayWrite = VioGpu3DSystemDisplayWrite;                 // :131

    InitialData.DxgkDdiStopDeviceAndReleasePostDisplayOwnership = VioGpu3DStopDeviceAndReleasePostDisplayOwnership; // :133

    InitialData.DxgkDdiCreateDevice = VioGpu3DCreateDevice;             // :135
    InitialData.DxgkDdiDestroyDevice = VioGpu3DDestroyDevice;           // :136

    InitialData.DxgkDdiPreemptCommand = VioGpu3DDdiPreemptCommand;           // :138
    InitialData.DxgkDdiResetFromTimeout = VioGpu3DDdiResetFromTimeout;       // :139
    InitialData.DxgkDdiRestartFromTimeout = VioGpu3DDdiRestartFromTimeout;   // :140
    InitialData.DxgkDdiCollectDbgInfo = VioGpu3DDdiCollectDbgInfo;           // :141
    InitialData.DxgkDdiQueryCurrentFence = VioGpu3DDdiQueryCurrentFence;     // :142

    InitialData.DxgkDdiQueryEngineStatus = VioGpu3DDdiQueryEngineStatus;     // :144
    InitialData.DxgkDdiResetEngine = VioGpu3DDdiResetEngine;                 // :145
    InitialData.DxgkDdiCancelCommand = VioGpu3DDdiCancelCommand;             // :146

    InitialData.DxgkDdiGetNodeMetadata = VioGpu3DDdiGetNodeMetadata;         // :148
    InitialData.DxgkDdiControlInterrupt = VioGpu3DDdiControlInterrupt;       // :149
    InitialData.DxgkDdiGetScanLine = VioGpu3DDdiGetScanLine;                 // :150

    NTSTATUS Status = DxgkInitialize(pDriverObject, pRegistryPath, &InitialData);   // :152

    if (!NT_SUCCESS(Status))
    {
        DbgPrint(TRACE_LEVEL_ERROR, ("DxgkInitialize failed with Status: 0x%X\n", Status));
    }

    DbgPrint(TRACE_LEVEL_VERBOSE, ("<--- %s\n", __FUNCTION__));
    return Status;
}
```
(verbatim, `driver.cpp:76-161`)

Note: the comment at `driver.cpp:80` says **"VIOGPU FULL build"** — this is the full 3D/render variant (as opposed to the display-only `viogpudo`). That is exactly the structural template Helios wants.

**Two delegation patterns** are visible in `driver.cpp`. The DDI thunks are thin and forward into C++ objects on the `VioGpuAdapter` (`reinterpret_cast` of the `hAdapter`/context handle):

- Render-engine DDIs forward into `pAdapter->commander.*` (a `VioGpuCommander`): e.g. `VioGpu3DPatch` → `pAdapter->commander.Patch(pPatch);` (`driver.cpp:583`); `VioGpu3DSubmitCommand` → `pAdapter->commander.SubmitCommand(pSubmitCommand);` (`driver.cpp:601`), both gated on `pAdapter->IsDriverActive()`.
- Display/VidPN DDIs forward into `pAdapter->vidpn.*` (a `VioGpuVidPN`): e.g. `VioGpu3DSetVidPnSourceAddress` → `pAdapter->vidpn.SetVidPnSourceAddress(pSetVidPnSourceAddress);` (`driver.cpp:951`).
- Device/Context: `hDevice` is a heap `VioGpuDevice`. `VioGpu3DCreateDevice` does `pCreateDevice->hDevice = new (NonPagedPoolNx) VioGpuDevice(pAdapter);` (`driver.cpp:620`). **`CreateContext` aliases the context to the device** — there is no separate context object: `pCreateContext->hContext = hDevice;` with `DmaBufferSize = 256*1024`, `DmaBufferPrivateDataSize = 40`, and `AllocationListSize = PatchLocationListSize = DXGK_ALLOCATION_LIST_SIZE_GDICONTEXT` (`driver.cpp:656-666`). Step 2 can reuse this "context == device" simplification, or split if venus needs per-context ring state.
- `Present`/`Render` forward into the device object: `pDxContext->Present(pPresent)` (`driver.cpp:691`), `pDxContext->Render(pRender)` (`driver.cpp:703`).

#### 2. `driver.h` declarations of the registered DDIs — verbatim

All declarations are in `driver.h:33-278`. The header begins with `extern "C" DRIVER_INITIALIZE DriverEntry;` (`driver.h:33`). Representative signatures (each registered function, exact `_In_/_Out_/_Inout_` annotations and arg types preserved):

```cpp
// driver.h
extern "C" DRIVER_INITIALIZE DriverEntry;                                                   // :33
VOID VioGpu3DUnload(VOID);                                                                   // :35
NTSTATUS VioGpu3DAddDevice(_In_ DEVICE_OBJECT *pPhysicalDeviceObject, _Outptr_ PVOID *ppDeviceContext); // :38
NTSTATUS VioGpu3DRemoveDevice(_In_ VOID *pDeviceContext);                                    // :41
NTSTATUS VioGpu3DStartDevice(_In_ VOID *pDeviceContext,
                    _In_ DXGK_START_INFO *pDxgkStartInfo,
                    _In_ DXGKRNL_INTERFACE *pDxgkInterface,
                    _Out_ ULONG *pNumberOfViews,
                    _Out_ ULONG *pNumberOfChildren);                                         // :44-48
NTSTATUS VioGpu3DStopDevice(_In_ VOID *pDeviceContext);                                       // :51
VOID VioGpu3DResetDevice(_In_ VOID *pDeviceContext);                                          // :53
NTSTATUS VioGpu3DDispatchIoRequest(_In_ VOID *pDeviceContext, _In_ ULONG VidPnSourceId,
                          _In_ VIDEO_REQUEST_PACKET *pVideoRequestPacket);                    // :56-58
NTSTATUS VioGpu3DSetPowerState(_In_ VOID *pDeviceContext, _In_ ULONG HardwareUid,
                      _In_ DEVICE_POWER_STATE DevicePowerState, _In_ POWER_ACTION ActionType); // :61-64
NTSTATUS VioGpu3DQueryChildRelations(_In_ VOID *pDeviceContext,
       _Out_writes_bytes_(ChildRelationsSize) DXGK_CHILD_DESCRIPTOR *pChildRelations,
       _In_ ULONG ChildRelationsSize);                                                       // :67-69
NTSTATUS VioGpu3DQueryChildStatus(_In_ VOID *pDeviceContext, _Inout_ DXGK_CHILD_STATUS *pChildStatus,
                         _In_ BOOLEAN NonDestructiveOnly);                                    // :72-74
NTSTATUS VioGpu3DQueryDeviceDescriptor(_In_ VOID *pDeviceContext, _In_ ULONG ChildUid,
                              _Inout_ DXGK_DEVICE_DESCRIPTOR *pDeviceDescriptor);             // :77-79
BOOLEAN  VioGpu3DInterruptRoutine(_In_ VOID *pDeviceContext, _In_ ULONG MessageNumber);       // :82
VOID     VioGpu3DDpcRoutine(_In_ VOID *pDeviceContext);                                        // :84
NTSTATUS APIENTRY VioGpu3DQueryAdapterInfo(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_QUERYADAPTERINFO *pQueryAdapterInfo); // :87-88
NTSTATUS APIENTRY VioGpu3DDdiGetNodeMetadata(_In_ CONST HANDLE hAdapter, UINT NodeOrdinal,
                           _Out_ DXGKARG_GETNODEMETADATA *pGetNodeMetadata);                  // :92-94
NTSTATUS APIENTRY VioGpu3DSetPointerPosition(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_SETPOINTERPOSITION *pSetPointerPosition); // :97-98
NTSTATUS APIENTRY VioGpu3DSetPointerShape(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_SETPOINTERSHAPE *pSetPointerShape);          // :101-102
NTSTATUS APIENTRY VioGpu3DEscape(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_ESCAPE *pEscape);                                     // :105-106
NTSTATUS APIENTRY VioGpu3DCreateAllocation(_In_ CONST HANDLE hAdapter, _Inout_ DXGKARG_CREATEALLOCATION *pCreateAllocation);          // :109-110
NTSTATUS APIENTRY VioGpu3DOpenAllocation(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_OPENALLOCATION *pOpenAllocation);             // :113-114
NTSTATUS APIENTRY VioGpu3DCloseAllocation(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_CLOSEALLOCATION *pCloseAllocation);          // :117-118
NTSTATUS APIENTRY VioGpu3DDescribeAllocation(_In_ CONST HANDLE hAdapter, _Inout_ DXGKARG_DESCRIBEALLOCATION *pDescribeAllocation);    // :121-122
NTSTATUS APIENTRY VioGpu3DDestroyAllocation(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_DESTROYALLOCATION *pDestroyAllocation);    // :125-126
NTSTATUS APIENTRY VioGpu3DGetStandardAllocationDriverData(_In_ CONST HANDLE hAdapter,
                                        _Inout_ DXGKARG_GETSTANDARDALLOCATIONDRIVERDATA *pStandardAllocation); // :130-131
NTSTATUS APIENTRY VioGpu3DBuildPagingBuffer(_In_ CONST HANDLE hAdapter, _In_ DXGKARG_BUILDPAGINGBUFFER *pCreateAllocation); // :133-135
NTSTATUS APIENTRY VioGpu3DPatch(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_PATCH *pPatch);                              // :137-139
NTSTATUS APIENTRY VioGpu3DSubmitCommand(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_SUBMITCOMMAND *pSubmitCommand);      // :141-143
NTSTATUS APIENTRY VioGpu3DCreateDevice(_In_ CONST HANDLE hAdapter, _Inout_ DXGKARG_CREATEDEVICE *pCreateDevice);            // :145-147
NTSTATUS APIENTRY VioGpu3DDestroyDevice(_In_ VOID *pDeviceContext);                                                         // :149-151
NTSTATUS APIENTRY VioGpu3DDdiCreateContext(_In_ CONST HANDLE hDevice, _Inout_ DXGKARG_CREATECONTEXT *pCreateContext);       // :153-155
NTSTATUS APIENTRY VioGpu3DDdiDestroyContext(_In_ CONST HANDLE hContext);                                                    // :157-159
NTSTATUS APIENTRY VioGpu3DPresent(_In_ CONST HANDLE hDevice, _Inout_ DXGKARG_PRESENT *pPresent);                            // :161-163
NTSTATUS APIENTRY VioGpu3DRender(_In_ CONST HANDLE hDevice, _Inout_ DXGKARG_RENDER *pRender);                               // :165-167
NTSTATUS APIENTRY VioGpu3DIsSupportedVidPn(_In_ CONST HANDLE hAdapter, _Inout_ DXGKARG_ISSUPPORTEDVIDPN *pIsSupportedVidPn); // :169-171
NTSTATUS APIENTRY VioGpu3DRecommendFunctionalVidPn(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_RECOMMENDFUNCTIONALVIDPN *CONST pRecommendFunctionalVidPn); // :173-176
NTSTATUS APIENTRY VioGpu3DRecommendVidPnTopology(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_RECOMMENDVIDPNTOPOLOGY *CONST pRecommendVidPnTopology); // :178-181  (declared; NOT registered)
NTSTATUS APIENTRY VioGpu3DRecommendMonitorModes(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_RECOMMENDMONITORMODES *CONST pRecommendMonitorModes); // :183-186
NTSTATUS APIENTRY VioGpu3DEnumVidPnCofuncModality(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_ENUMVIDPNCOFUNCMODALITY *CONST pEnumCofuncModality); // :188-191
NTSTATUS APIENTRY VioGpu3DSetVidPnSourceVisibility(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_SETVIDPNSOURCEVISIBILITY *pSetVidPnSourceVisibility); // :193-196
NTSTATUS VioGpu3DSetVidPnSourceAddress(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_SETVIDPNSOURCEADDRESS *pSetVidPnSourceAddress); // :198-199 (note: no APIENTRY in decl)
NTSTATUS APIENTRY VioGpu3DCommitVidPn(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_COMMITVIDPN *CONST pCommitVidPn); // :201-203
NTSTATUS APIENTRY VioGpu3DUpdateActiveVidPnPresentPath(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_UPDATEACTIVEVIDPNPRESENTPATH *CONST pUpdateActiveVidPnPresentPath); // :205-208
NTSTATUS APIENTRY VioGpu3DQueryVidPnHWCapability(_In_ CONST HANDLE hAdapter, _Inout_ DXGKARG_QUERYVIDPNHWCAPABILITY *pVidPnHWCaps); // :210-212
NTSTATUS APIENTRY VioGpu3DDdiControlInterrupt(_In_ CONST HANDLE hAdapter, _In_ CONST DXGK_INTERRUPT_TYPE InterruptType, _In_ BOOLEAN EnableInterrupt); // :214-218
NTSTATUS APIENTRY VioGpu3DDdiGetScanLine(_In_ CONST HANDLE hAdapter, _Inout_ DXGKARG_GETSCANLINE *pGetScanLine); // :220-222
NTSTATUS APIENTRY VioGpu3DStopDeviceAndReleasePostDisplayOwnership(_In_ VOID *pDeviceContext, _In_ D3DDDI_VIDEO_PRESENT_TARGET_ID TargetId, _Out_ DXGK_DISPLAY_INFORMATION *DisplayInfo); // :224-228
NTSTATUS APIENTRY VioGpu3DSystemDisplayEnable(_In_ VOID *pDeviceContext, _In_ D3DDDI_VIDEO_PRESENT_TARGET_ID TargetId, _In_ PDXGKARG_SYSTEM_DISPLAY_ENABLE_FLAGS Flags, _Out_ UINT *Width, _Out_ UINT *Height, _Out_ D3DDDIFORMAT *ColorFormat); // :230-237
VOID     APIENTRY VioGpu3DSystemDisplayWrite(_In_ VOID *pDeviceContext, _In_ VOID *Source, _In_ UINT SourceWidth, _In_ UINT SourceHeight, _In_ UINT SourceStride, _In_ UINT PositionX, _In_ UINT PositionY); // :239-245
NTSTATUS APIENTRY VioGpu3DDdiPreemptCommand(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_PREEMPTCOMMAND *pPreemptCommand); // :247-249
NTSTATUS APIENTRY VioGpu3DDdiRestartFromTimeout(_In_ CONST HANDLE hAdapter); // :251-253
NTSTATUS APIENTRY VioGpu3DDdiCancelCommand(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_CANCELCOMMAND *pCancelCommand); // :255-257
NTSTATUS APIENTRY VioGpu3DDdiQueryCurrentFence(_In_ CONST HANDLE hAdapter, _Inout_ DXGKARG_QUERYCURRENTFENCE *pCurrentFence); // :259-261
NTSTATUS APIENTRY VioGpu3DDdiResetEngine(_In_ CONST HANDLE hAdapter, _Inout_ DXGKARG_RESETENGINE *pResetEngine); // :263-265
NTSTATUS APIENTRY VioGpu3DDdiQueryEngineStatus(_In_ CONST HANDLE hAdapter, _Inout_ DXGKARG_QUERYENGINESTATUS *pQueryEngineStatus); // :267-269
NTSTATUS APIENTRY VioGpu3DDdiCollectDbgInfo(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_COLLECTDBGINFO *pCollectDbgInfo); // :271-273
NTSTATUS APIENTRY VioGpu3DDdiResetFromTimeout(_In_ CONST HANDLE hAdapter); // :275-277
```
(verbatim signatures, `driver.h:33-277`)

**Two header/registration asymmetries Step 2 must note:**
- `VioGpu3DRecommendVidPnTopology` is **declared** in `driver.h:178-181` but is **never assigned** in the `DriverEntry` table (there is no `InitialData.DxgkDdiRecommendVidPnTopology = ...` line) and has a body at `driver.cpp:760-776`. It is dead in this driver. `DxgkDdiGetNodeMetadata`, by contrast, *is* registered (`driver.cpp:148`).
- `VioGpu3DSetVidPnSourceAddress` is declared **without** `APIENTRY` (`driver.h:198`), unlike its siblings; it is still assigned at `driver.cpp:127`. The body is at `driver.cpp:945-954` and just calls `pAdapter->vidpn.SetVidPnSourceAddress(...)` then returns `STATUS_SUCCESS`.

#### 3. DDI → implementation table (registered? / purpose / impl site)

Every row's "registered?" is the literal presence/absence of an `InitialData.DxgkDdi... = ...` line in `driver.cpp:85-150`. Impl line is the function body in `driver.cpp` unless noted. "Helios" column flags render-only relevance.

| DDI (DRIVER_INITIALIZATION_DATA field) | Registered? | One-line purpose | viogpu3d impl (path:line) | Helios needs? |
|---|---|---|---|---|
| `DxgkDdiAddDevice` | yes (`:85`) | PnP: allocate per-adapter context (`new VioGpuAdapter`) | `VioGpu3DAddDevice` `driver.cpp:179-205` | YES |
| `DxgkDdiStartDevice` | yes (`:86`) | Map HW, receive `DXGKRNL_INTERFACE`, report #views/#children | `VioGpu3DStartDevice` `driver.cpp:224-237` → `pAdapter->StartDevice` | YES (render init; drop view/child semantics) |
| `DxgkDdiStopDevice` | yes (`:87`) | PnP stop: tear down HW | `VioGpu3DStopDevice` `driver.cpp:239-248` → `pAdapter->StopDevice` | YES |
| `DxgkDdiRemoveDevice` | yes (`:88`) | PnP remove: `delete` adapter | `VioGpu3DRemoveDevice` `driver.cpp:207-222` | YES |
| `DxgkDdiDispatchIoRequest` | yes (`:90`) | Legacy VIDEO_REQUEST_PACKET dispatch (kernel-VGA) | `VioGpu3DDispatchIoRequest` `driver.cpp:250-266` | likely stub (no VGA) |
| `DxgkDdiInterruptRoutine` | yes (`:91`) | ISR: ack device IRQ at DIRQL, queue DPC | `VioGpu3DInterruptRoutine` `driver.cpp:935-943` | YES (venus fence signal) |
| `DxgkDdiDpcRoutine` | yes (`:92`) | DPC: post-ISR completion (drain used ring, signal fences) | `VioGpu3DDpcRoutine` `driver.cpp:920-933` | YES |
| `DxgkDdiQueryChildRelations` | yes (`:94`) | Enumerate display child targets (monitors) | `VioGpu3DQueryChildRelations` `driver.cpp:286-297` | **NO** (display child; IDD owns display) |
| `DxgkDdiQueryChildStatus` | yes (`:95`) | Report child (connector) status | `VioGpu3DQueryChildStatus` `driver.cpp:299-310` | **NO** (display) |
| `DxgkDdiQueryDeviceDescriptor` | yes (`:96`) | Return EDID for a child | `VioGpu3DQueryDeviceDescriptor` `driver.cpp:312-328` | **NO** (display/EDID) |
| `DxgkDdiSetPowerState` | yes (`:97`) | Device/component power transitions | `VioGpu3DSetPowerState` `driver.cpp:268-284` | YES (minimal) |
| `DxgkDdiResetDevice` | yes (`:98`) | Reset to known state (mandatory, else Code 37) | `VioGpu3DResetDevice` `driver.cpp:956-963` → `pAdapter->ResetDevice` | YES |
| `DxgkDdiUnload` | yes (`:99`) | Driver unload (WPP cleanup) | `VioGpu3DUnload` `driver.cpp:172-177` | YES |
| `DxgkDdiQueryAdapterInfo` | yes (`:101`) | Report caps / segments / memory model to dxgkrnl | `VioGpu3DQueryAdapterInfo` `driver.cpp:330-340` → `pAdapter->QueryAdapterInfo` | **YES — CORE** (the fake-VidMm caps surface) |
| `DxgkDdiEscape` | yes (`:102`) | Private UMD↔KMD channel | `VioGpu3DEscape` `driver.cpp:398-414` → `pAdapter->Escape` | YES (venus pass-through path) |
| `DxgkDdiCreateAllocation` | yes (`:103`) | Create VidMm allocations (→ venus resources) | `VioGpu3DCreateAllocation` `driver.cpp:416-432` | **YES — CORE** |
| `DxgkDdiOpenAllocation` | yes (`:104`) | Per-device open handle to a shared allocation | `VioGpu3DOpenAllocation` `driver.cpp:448-458` | YES |
| `DxgkDdiCloseAllocation` | yes (`:105`) | Close per-device allocation handle | `VioGpu3DCloseAllocation` `driver.cpp:460-478` | YES |
| `DxgkDdiDescribeAllocation` | yes (`:106`) | Describe a standard-allocation's surface params | `VioGpu3DDescribeAllocation` `driver.cpp:434-446` | YES |
| `DxgkDdiDestroyAllocation` | yes (`:107`) | Destroy allocations (free venus resources) | `VioGpu3DDestroyAllocation` `driver.cpp:480-508` | YES |
| `DxgkDdiGetStandardAllocationDriverData` | yes (`:108`) | Driver-private data for OS standard allocations (shared primary, staging) | `VioGpu3DGetStandardAllocationDriverData` `driver.cpp:510-520` | YES |
| `DxgkDdiBuildPagingBuffer` | yes (`:109`) | Emit DMA for VidMm transfers/map/fill/transfer | `VioGpu3DBuildPagingBuffer` `driver.cpp:522-569` | **YES — but REPLACE body** (no TRANSFER; over-size segment → no paging; nop/decorative) |
| `DxgkDdiCreateContext` | yes (`:111`) | Create render context (DMA buffer size, alloc/patch list sizes) | `VioGpu3DDdiCreateContext` `driver.cpp:648-667` (aliases ctx=device) | YES |
| `DxgkDdiDestroyContext` | yes (`:112`) | Destroy render context | `VioGpu3DDdiDestroyContext` `driver.cpp:669-680` (nop) | YES |
| `DxgkDdiPresent` | yes (`:114`) | Build present DMA (blt/flip) into DMA buffer | `VioGpu3DPresent` `driver.cpp:682-692` → `pDxContext->Present` | maybe (per memory, IDD uses IddCx swapchain, not pfnPresent; present "was never the blocker") |
| `DxgkDdiRender` | yes (`:115`) | Validate/translate UMD command buffer → DMA buffer | `VioGpu3DRender` `driver.cpp:694-704` → `pDxContext->Render` | **YES — CORE** (venus cmd path) |
| `DxgkDdiPatch` | yes (`:116`) | Patch allocation phys addrs into DMA buffer | `VioGpu3DPatch` `driver.cpp:571-584` → `pAdapter->commander.Patch` | YES (decorative/id-based patch) |
| `DxgkDdiSubmitCommand` | yes (`:117`) | Submit a DMA buffer to the GPU engine | `VioGpu3DSubmitCommand` `driver.cpp:586-602` → `pAdapter->commander.SubmitCommand` | **YES — CORE** (venus submit must drive WDDM fence) |
| `DxgkDdiSetPointerPosition` | yes (`:119`) | HW cursor position | `VioGpu3DSetPointerPosition` `driver.cpp:360-377` | **NO** (display/cursor) |
| `DxgkDdiSetPointerShape` | yes (`:120`) | HW cursor shape | `VioGpu3DSetPointerShape` `driver.cpp:379-396` | **NO** (display/cursor) |
| `DxgkDdiIsSupportedVidPn` | yes (`:121`) | Validate a proposed VidPN | `VioGpu3DIsSupportedVidPn` `driver.cpp:725-740` | **NO** (VidPN/display) |
| `DxgkDdiRecommendFunctionalVidPn` | yes (`:122`) | Recommend a functional VidPN | `VioGpu3DRecommendFunctionalVidPn` `driver.cpp:742-758` | **NO** (VidPN) |
| `DxgkDdiEnumVidPnCofuncModality` | yes (`:123`) | Enumerate cofunctional modes | `VioGpu3DEnumVidPnCofuncModality` `driver.cpp:796-812` | **NO** (VidPN) |
| `DxgkDdiSetVidPnSourceVisibility` | yes (`:124`) | Show/hide a VidPN source | `VioGpu3DSetVidPnSourceVisibility` `driver.cpp:814-830` | **NO** (VidPN) |
| `DxgkDdiCommitVidPn` | yes (`:125`) | Commit/realize a VidPN topology | `VioGpu3DCommitVidPn` `driver.cpp:832-847` | **NO** (VidPN) |
| `DxgkDdiUpdateActiveVidPnPresentPath` | yes (`:126`) | Update an active present path | `VioGpu3DUpdateActiveVidPnPresentPath` `driver.cpp:849-865` | **NO** (VidPN) |
| `DxgkDdiSetVidPnSourceAddress` | yes (`:127`) | Set scanout base addr for a source (flip) | `VioGpu3DSetVidPnSourceAddress` `driver.cpp:945-954` → `pAdapter->vidpn.SetVidPnSourceAddress` | **NO** (scanout/display) |
| `DxgkDdiRecommendMonitorModes` | yes (`:128`) | Recommend monitor mode list | `VioGpu3DRecommendMonitorModes` `driver.cpp:778-794` | **NO** (display) |
| `DxgkDdiQueryVidPnHWCapability` | yes (`:129`) | Report per-path HW scaling/rotation caps | `VioGpu3DQueryVidPnHWCapability` `driver.cpp:867-882` | **NO** (VidPN) |
| `DxgkDdiSystemDisplayEnable` | yes (`:130`) | Bugcheck/fallback display enable | `VioGpu3DSystemDisplayEnable` `driver.cpp:965-979` | **NO** (display) |
| `DxgkDdiSystemDisplayWrite` | yes (`:131`) | Bugcheck/fallback pixel write | `VioGpu3DSystemDisplayWrite` `driver.cpp:981-994` | **NO** (display) |
| `DxgkDdiStopDeviceAndReleasePostDisplayOwnership` | yes (`:133`) | Hand off post-display (boot fb) ownership | `VioGpu3DStopDeviceAndReleasePostDisplayOwnership` `driver.cpp:706-723` → `pAdapter->StopDeviceAndReleasePostDisplayOwnership` | **NO** (display) |
| `DxgkDdiCreateDevice` | yes (`:135`) | Create per-process device (`new VioGpuDevice`) | `VioGpu3DCreateDevice` `driver.cpp:604-628` | YES |
| `DxgkDdiDestroyDevice` | yes (`:136`) | Destroy device (`delete VioGpuDevice`) | `VioGpu3DDestroyDevice` `driver.cpp:630-646` | YES |
| `DxgkDdiPreemptCommand` | yes (`:138`) | Preempt in-flight DMA on a node | `VioGpu3DDdiPreemptCommand` `driver.cpp:996-1009` | YES (GPU scheduler contract) |
| `DxgkDdiResetFromTimeout` | yes (`:139`) | TDR: reset engine after timeout | `VioGpu3DDdiResetFromTimeout` `driver.cpp:1081-...` | YES (TDR contract) |
| `DxgkDdiRestartFromTimeout` | yes (`:140`) | TDR: restart scheduling after reset | `VioGpu3DDdiRestartFromTimeout` `driver.cpp:1011-1019` | YES (TDR) |
| `DxgkDdiCollectDbgInfo` | yes (`:141`) | Collect debug info on TDR | `VioGpu3DDdiCollectDbgInfo` `driver.cpp:1069-1079` | YES (TDR) |
| `DxgkDdiQueryCurrentFence` | yes (`:142`) | Return last-completed fence id for a node | `VioGpu3DDdiQueryCurrentFence` `driver.cpp:1033-1043` | **YES — CORE** (venus→WDDM fence) |
| `DxgkDdiQueryEngineStatus` | yes (`:144`) | Report engine busy/idle (TDR diag) | `VioGpu3DDdiQueryEngineStatus` `driver.cpp:1057-1067` | YES |
| `DxgkDdiResetEngine` | yes (`:145`) | Per-engine reset (TDR) | `VioGpu3DDdiResetEngine` `driver.cpp:1045-1055` | YES (TDR) |
| `DxgkDdiCancelCommand` | yes (`:146`) | Cancel a queued DMA buffer | `VioGpu3DDdiCancelCommand` `driver.cpp:1021-1031` | YES |
| `DxgkDdiGetNodeMetadata` | yes (`:148`) | Report engine-node metadata (type/name) | `VioGpu3DDdiGetNodeMetadata` `driver.cpp:342-358` | YES (declare 3D node) |
| `DxgkDdiControlInterrupt` | yes (`:149`) | Enable/disable an interrupt type | `VioGpu3DDdiControlInterrupt` `driver.cpp:884-898` | YES |
| `DxgkDdiGetScanLine` | yes (`:150`) | Report current scanline / vblank | `VioGpu3DDdiGetScanLine` `driver.cpp:900-918` | **NO** (display) |
| `DxgkDdiRecommendVidPnTopology` | **NO** (declared `driver.h:178-181`, body `driver.cpp:760-776`, never assigned) | Recommend a VidPN topology | (unregistered, dead) | **NO** (VidPN) |

#### 4. What Helios will NOT need (display/VidPN half) — explicit

Per the LOCKED goal (render-only; Looking Glass **IDD** owns display; DWM composites on Helios and IddCx captures the OS-composed texture via `IddCxSwapChainReleaseAndAcquireBuffer`, *not* the UMD present path), Step 2 should **omit** the following registered viogpu3d DDIs (and not write their bodies). These are all the display/VidPN/scanout/cursor/system-display callbacks:

- `DxgkDdiQueryChildRelations` (`:94`), `DxgkDdiQueryChildStatus` (`:95`), `DxgkDdiQueryDeviceDescriptor` (`:96`) — display child/connector/EDID enumeration.
- `DxgkDdiSetPointerPosition` (`:119`), `DxgkDdiSetPointerShape` (`:120`) — HW cursor.
- `DxgkDdiIsSupportedVidPn` (`:121`), `DxgkDdiRecommendFunctionalVidPn` (`:122`), `DxgkDdiEnumVidPnCofuncModality` (`:123`), `DxgkDdiSetVidPnSourceVisibility` (`:124`), `DxgkDdiCommitVidPn` (`:125`), `DxgkDdiUpdateActiveVidPnPresentPath` (`:126`), `DxgkDdiSetVidPnSourceAddress` (`:127`), `DxgkDdiRecommendMonitorModes` (`:128`), `DxgkDdiQueryVidPnHWCapability` (`:129`) — the entire VidPN/mode-set/scanout surface.
- `DxgkDdiSystemDisplayEnable` (`:130`), `DxgkDdiSystemDisplayWrite` (`:131`), `DxgkDdiStopDeviceAndReleasePostDisplayOwnership` (`:133`) — bugcheck/post-display fallback ownership.
- `DxgkDdiGetScanLine` (`:150`) — scanline/vblank reporting.

**Caveat for Step 2:** dxgkrnl distinguishes a render-only WDDM adapter from a display-capable one by how the driver answers `DxgkDdiQueryAdapterInfo` (the caps/`DXGK_DRIVERCAPS` surface, covered in other sections) and by whether it reports display children. A render-only miniport legitimately omits the VidPN/scanout DDIs. The relevant memory note also warns that DWM's own `D3D11CreateDevice(Helios)` must succeed for it to composite on Helios — that depends on the UMD (Gate 5b / DXVK), not on these miniport DDIs. So the list above is "not registered by the *miniport*", but the *render* DDIs (`QueryAdapterInfo`, `CreateAllocation`, `Render`, `Patch`, `SubmitCommand`, `BuildPagingBuffer`, `QueryCurrentFence`, the TDR set, the interrupt/DPC pair, `CreateDevice/Context`, `Escape`) are all mandatory and must be kept.

**Bodies Step 2 must rewrite (not just copy) vs. keep-as-thunk:**
- Rewrite: `BuildPagingBuffer` (no virtio TRANSFER; over-size segment ⇒ paging is decorative/nop), `CreateAllocation`/`DestroyAllocation` (map to venus resource ids + host-visible BAR blobs, not guest-backing-store transfers), `SubmitCommand`+`QueryCurrentFence` (venus submit must drive the WDDM fence; viogpu3d's `commander.SubmitCommand` is the structural template but its mechanism is virtio control-queue, not venus), `Patch` (id-based / decorative since the host owns the real MMU).
- Keep the thin-thunk shape: PnP DDIs (`AddDevice`/`StartDevice`/`StopDevice`/`RemoveDevice`/`ResetDevice`/`Unload`), `CreateDevice`/`DestroyDevice`/`CreateContext`/`DestroyContext`, interrupt/DPC, and the TDR family — adapting only the underlying virtio init/fence plumbing.

This A8 table is the canonical "which DDIs exist and where" reference; the per-DDI argument structs (`DXGKARG_*`) and the caps/segment payloads of `QueryAdapterInfo`/`CreateAllocation`/`BuildPagingBuffer` are detailed in the bindgen-backed sections elsewhere in this doc.

### A1. Adapter capabilities — DxgkDdiQueryAdapterInfo: DXGK_DRIVERCAPS, DXGK_VIDMMCAPS, DXGK_GPUMMUCAPS

**Orientation / the single most important structural fact.** In WDDM 10.0.26100, `DXGK_VIDMMCAPS` is **not** queried by its own QAITYPE. It is an *inline member* of `DXGK_DRIVERCAPS`, named `MemoryManagementCaps`. So when a driver fills `DXGKQAITYPE_DRIVERCAPS`, it is *simultaneously* declaring its video-memory-manager capability surface (including the GpuMmu/IoMmu/Paravirtualization opt-in bits). This is confirmed verbatim in the bindgen dump:

```rust
// /home/rupansh/helios-vgpu/dxgk_bindings_dump.rs:51069-51110
pub struct _DXGK_DRIVERCAPS {
    pub HighestAcceptableAddress: PHYSICAL_ADDRESS,
    pub MaxAllocationListSlotId: UINT,
    pub ApertureSegmentCommitLimit: SIZE_T,
    ...
    pub PresentationCaps: DXGK_PRESENTATIONCAPS,
    pub MaxQueuedFlipOnVSync: UINT,
    pub FlipCaps: DXGK_FLIPCAPS,
    pub SchedulingCaps: DXGK_VIDSCHCAPS,
    pub MemoryManagementCaps: DXGK_VIDMMCAPS,      // <-- VIDMMCAPS lives HERE (line 51084)
    pub GpuEngineTopology: DXGK_GPUENGINETOPOLOGY,
    pub WDDMVersion: DXGK_WDDMVERSION,
    ...
}
```

`DXGK_GPUMMUCAPS`, by contrast, *is* a separate query type, `DXGKQAITYPE_GPUMMUCAPS = 13` (`dxgk_bindings_dump.rs:44526`), driven with a `DXGK_QUERYGPUMMUCAPSIN` input carrying a `PhysicalAdapterIndex` (`dxgk_bindings_dump.rs:47884-47899`). dxgkrnl only issues this query *after* the driver has opted into the GpuMmu model via `DXGK_VIDMMCAPS::GpuMmuSupported`. So the opt-in gate is VIDMMCAPS-inside-DRIVERCAPS; GPUMMUCAPS is the follow-up that describes page-table geometry.

The conceptual doc states the opt-in requirement plainly:

> "WDDM v2 supports two distinct models for GPU virtual addressing, *GpuMmu* and *IoMmu*. A driver must [opt-in] to support either or both of the models. A single GPU node can support both modes simultaneously." — `gpu-virtual-memory-in-wddm-2-0.md:31` (the "opt-in" link in the doc points at `ns-d3dkmddi-_dxgk_vidmmcaps`, i.e. DXGK_VIDMMCAPS).

---

#### DXGK_DRIVERCAPS (DXGKQAITYPE_DRIVERCAPS)

**Purpose / when dxgkrnl reads it.** During `DxgkDdiQueryAdapterInfo` at adapter start (AddDevice/StartDevice → adapter init). It is the master capability struct; dxgkrnl reads `WDDMVersion`, the scheduling/flip/presentation caps, and the embedded `MemoryManagementCaps` (= VIDMMCAPS) to decide the memory model and whether the adapter is internally consistent. Inconsistent version/cap surfaces are rejected during AddAdapter (Helios's own comment notes this — see below).

**viogpu3d quoted impl** (`viogpu_adapter.cpp:473-518`):

```cpp
        case DXGKQAITYPE_DRIVERCAPS:
            {
                ...
                DXGK_DRIVERCAPS *pDriverCaps = (DXGK_DRIVERCAPS *)pQueryAdapterInfo->pOutputData;
                ...
                RtlZeroMemory(pDriverCaps, pQueryAdapterInfo->OutputDataSize /*sizeof(DXGK_DRIVERCAPS)*/);
                pDriverCaps->WDDMVersion = DXGKDDI_WDDMv1_3;                       // :491
                pDriverCaps->HighestAcceptableAddress.QuadPart = (ULONG64)-1;     // :492

                pDriverCaps->FlipCaps.FlipOnVSyncMmIo = TRUE;                     // :494
                pDriverCaps->MaxQueuedFlipOnVSync = 1;                            // :496

                pDriverCaps->MemoryManagementCaps.SectionBackedPrimary = TRUE;    // :498  <-- the ONLY VIDMMCAPS bit set

                pDriverCaps->SupportDirectFlip = 1;                              // :500
                pDriverCaps->SchedulingCaps.MultiEngineAware = 1;                // :501
                pDriverCaps->SchedulingCaps.PreemptionAware = 1;                 // :502
                pDriverCaps->GpuEngineTopology.NbAsymetricProcessingNodes = 1;   // :504
                pDriverCaps->SupportSmoothRotation = FALSE;                      // :506
                pDriverCaps->SupportNonVGA = IsVgaDevice();                      // :507
                ...
                return STATUS_SUCCESS;
            }
```

**The strongest single piece of evidence in this whole section:** viogpu3d — a *working* virtio-gpu WDDM 3D driver — sets **exactly one** VIDMMCAPS bit: `MemoryManagementCaps.SectionBackedPrimary = TRUE` (`viogpu_adapter.cpp:498`). It sets **no** `GpuMmuSupported`, **no** `VirtualAddressingSupported`, **no** `IoMmuSupported`, **no** `ParavirtualizationSupported` anywhere in the driver (grep over `viogpu_adapter.cpp`, `driver.cpp`, `driver.h`, `viogpu_adapter.h` returns nothing for those identifiers — only the three `case DXGKQAITYPE_*` labels appear). It declares `WDDMVersion = DXGKDDI_WDDMv1_3` (note: WDDM 1.3, not 2.x — a 1.x driver predates the GpuMmu opt-in entirely), uses an **aperture** segment (`pSegmentDesc[0].Flags.Aperture = TRUE`, `viogpu_adapter.cpp:554`), and reaches the host via TRANSFER copies. So viogpu3d proves a virtio guest can be a fully-functional render path **without declaring any GPU memory model at all** at the WDDM-1.3 level.

**Exact bindgen field/accessor names (DRIVERCAPS scalar fields, verbatim, all `dxgk_bindings_dump.rs:51069-51110`):**
- `HighestAcceptableAddress: PHYSICAL_ADDRESS`
- `MaxAllocationListSlotId: UINT`
- `ApertureSegmentCommitLimit: SIZE_T`
- `PresentationCaps: DXGK_PRESENTATIONCAPS`
- `MaxQueuedFlipOnVSync: UINT`
- `FlipCaps: DXGK_FLIPCAPS`
- `SchedulingCaps: DXGK_VIDSCHCAPS`
- `MemoryManagementCaps: DXGK_VIDMMCAPS`  ← the VIDMMCAPS surface
- `GpuEngineTopology: DXGK_GPUENGINETOPOLOGY`
- `WDDMVersion: DXGK_WDDMVERSION`
- `PreemptionCaps: D3DKMDT_PREEMPTION_CAPS`
- `SupportNonVGA: BOOLEAN`, `SupportPerEngineTDR: BOOLEAN`, `SupportDirectFlip: BOOLEAN`
- `__bindgen_anon_1: _DXGK_DRIVERCAPS__bindgen_ty_1` (union of `GammaRampCaps` / `ColorTransformCaps`, `:51113-51116`)
- `MiscCaps: _DXGK_DRIVERCAPS__bindgen_ty_2` (a bitfield union; e.g. `set_SupportContextlessPresent`, bit 0, `:51163-51174`)

Note the bindgen union pattern for the cap sub-structs: each (`PresentationCaps`, `FlipCaps`, `SchedulingCaps`, `MemoryManagementCaps`) is a `union { __bindgen_anon_1: <bitfield struct>, Value: UINT }`, so a single named bit is set either through the typed accessor (`set_X(1)`) or by ORing a mask into `.__bindgen_anon_1.Value`.

**kmd_render current state: REAL (but minimal; does NOT set any memory-model bit).** Helios *does* handle `DXGKQAITYPE_DRIVERCAPS` with a real body (`query_adapter_info.rs:47, 82-148`). It sets WDDM version 3.2 and the mandatory render-load bits, but **leaves the entire VIDMMCAPS memory-model surface zeroed** (it only touches `MemoryManagementCaps.__bindgen_anon_1.Value = SectionBackedPrimary`, bit 3):

```rust
// /home/rupansh/helios-vgpu/kmd_render/src/ddi/query_adapter_info.rs:91-145
    caps.HighestAcceptableAddress.QuadPart = -1;                // :91
    caps.MaxAllocationListSlotId = 0xFFFF;                      // :92
    caps.ApertureSegmentCommitLimit = 64 * 1024 * 1024;        // :93
    caps.SupportNonVGA = 1;                                     // :95
    caps.WDDMVersion = DXGKDDI_WDDMv3_2;                        // :100
    caps.PreemptionCaps.GraphicsPreemptionGranularity =
        D3DKMDT_GRAPHICS_PREEMPTION_DMA_BUFFER_BOUNDARY;        // :101-102
    caps.PreemptionCaps.ComputePreemptionGranularity =
        D3DKMDT_COMPUTE_PREEMPTION_DMA_BUFFER_BOUNDARY;         // :103-104
    caps.SupportPerEngineTDR = 1;                              // :105
    ...
    const PRESENTATIONCAPS_SUPPORT_KERNEL_MODE_COMMAND_BUFFER: u32 = 1 << 2;  // :131
    const FLIPCAPS_FLIP_ON_VSYNC_MMIO: u32 = 1 << 1;                          // :132
    const SCHEDULINGCAPS_MULTI_ENGINE_AWARE: u32 = 1 << 0;                    // :133
    const SCHEDULINGCAPS_PREEMPTION_AWARE: u32 = 1 << 2;                      // :134
    const MEMORYMANAGEMENTCAPS_SECTION_BACKED_PRIMARY: u32 = 1 << 3;          // :135

    caps.PresentationCaps.__bindgen_anon_1.Value =
        PRESENTATIONCAPS_SUPPORT_KERNEL_MODE_COMMAND_BUFFER;     // :137-138
    caps.FlipCaps.__bindgen_anon_1.Value = FLIPCAPS_FLIP_ON_VSYNC_MMIO;       // :139
    caps.SchedulingCaps.__bindgen_anon_1.Value =
        SCHEDULINGCAPS_MULTI_ENGINE_AWARE | SCHEDULINGCAPS_PREEMPTION_AWARE;  // :140-141
    caps.MemoryManagementCaps.__bindgen_anon_1.Value =
        MEMORYMANAGEMENTCAPS_SECTION_BACKED_PRIMARY;            // :142-143
    caps.MaxQueuedFlipOnVSync = 1;                             // :144
    caps.GpuEngineTopology.NbAsymetricProcessingNodes = 1;    // :145
```

Helios independently rediscovered viogpu3d's `MemoryManagementCaps.SectionBackedPrimary` (it uses the raw `__bindgen_anon_1.Value = 1<<3` instead of the named `set_SectionBackedPrimary(1)`, but it is the identical bit — see VIDMMCAPS offsets below). A load-bearing empirical finding is recorded inline: dropping `FlipOnVSyncMmIo` regresses to Code 43 even with `SectionBackedPrimary` present (`query_adapter_info.rs:124-130`, citing `GATE2_3_CAPS_BACKING.md`). Helios's `WDDMVersion = DXGKDDI_WDDMv3_2` is the meaningful divergence from viogpu3d's `DXGKDDI_WDDMv1_3`: a WDDM 3.2 render adapter is firmly in the GpuMmu-era contract, so the cap surface Helios must satisfy is *stricter* than viogpu3d's 1.3 surface — this is exactly why the fake-VidMm work is needed.

---

#### DXGK_VIDMMCAPS (member `DXGK_DRIVERCAPS.MemoryManagementCaps`; no standalone QAITYPE)

**Purpose / when dxgkrnl reads it.** Read inline as part of `DXGKQAITYPE_DRIVERCAPS` at adapter init. This is the *memory-model opt-in*. The bits here tell VidMm: whether GpuMmu (`GpuMmuSupported`) or IoMmu (`IoMmuSupported`) virtual addressing is supported, whether the adapter is paravirtualized (`ParavirtualizationSupported`), and various allocation/cross-adapter behaviors. Per `gpu-virtual-memory-in-wddm-2-0.md:31`, this is the struct a driver must opt-in through to enable a GPU virtual-addressing model.

**Struct shape (bindgen, `dxgk_bindings_dump.rs:49550-49559`):**
```rust
pub struct _DXGK_VIDMMCAPS {
    pub __bindgen_anon_1: _DXGK_VIDMMCAPS__bindgen_ty_1,   // union { bitfield struct, Value: UINT }
    pub PagingNode: UINT,
}
pub union _DXGK_VIDMMCAPS__bindgen_ty_1 {
    pub __bindgen_anon_1: _DXGK_VIDMMCAPS__bindgen_ty_1__bindgen_ty_1,   // the bitfields
    pub Value: UINT,                                                     // 32-bit flat view
}
```
Total size 8 bytes (`:50517`), `PagingNode` at offset 4 (`:50523`). The bitfield carrier is `_bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>` (`:49565`).

**Exact bindgen bitfield accessor names + verified bit positions (each is `fn X(&self) -> UINT` / `set_X(&mut self, val: UINT)` / `X_raw` / `set_X_raw`). Bit positions read directly from each getter's `self._bitfield_1.get(N, 1u8)`:**

| Bit | Accessor (getter / `set_`+name) | Source line |
|----:|----------------------------------|-------------|
| 0 | `OutOfOrderLock` | `:49578` (get N=0 `:49579`) |
| 1 | `DedicatedPagingEngine` | `:49614` (`:49615`) |
| 2 | `PagingEngineCanSwizzle` | `:49650` (`:49651`) |
| 3 | `SectionBackedPrimary` | `:49686` (`:49687`) ← the one viogpu3d & Helios set |
| 4 | `CrossAdapterResource` | `:49722` (`:49723`) |
| 5 | `VirtualAddressingSupported` | `:49758` (`:49759`) |
| 6 | `GpuMmuSupported` | `:49794` (`:49795`) ← the GpuMmu opt-in |
| 7 | `IoMmuSupported` | `:49830` (`:49831`) |
| 8 | `ReplicateGdiContent` | `:49866` (`:49867`) |
| 9 | `NonCpuVisiblePrimary` | `:49902` (`:49903`) |
| 10 | `ParavirtualizationSupported` | `:49938` (`:49939`) ← the GPU-PV cap |
| 11 | `IoMmuSecureModeSupported` | `:49974` (`:49975`) |
| 12 | `DisableSelfRefreshVRAMInS3` | `:50010` (`:50011`) |
| 13 | `IoMmuSecureModeRequired` | `:50046` (`:50047`) |
| 14 | `MapAperture2Supported` | `:50082` (`:50083`) |
| 15 | `CrossAdapterResourceTexture` | `:50118` (`:50119`) |
| 16 | `CrossAdapterResourceScanout` | `:50154` (`:50155`) |
| 17 | `AlwaysPoweredVRAM` | `:50190` (`:50191`) |
| 18..31 | `Reserved` (14 bits) | `:50226` (get N=18 width=14 `:50227`) |

The accessors most relevant to the fake-VidMm model: **`GpuMmuSupported` (bit 6), `VirtualAddressingSupported` (bit 5), `IoMmuSupported` (bit 7), `ParavirtualizationSupported` (bit 10), `SectionBackedPrimary` (bit 3), `CrossAdapterResourceScanout` (bit 16), `CrossAdapterResourceTexture` (bit 15), `CrossAdapterResource` (bit 4)**. Confirmed: the struct exists in the dump (`_DXGK_VIDMMCAPS`, alias `DXGK_VIDMMCAPS = _DXGK_VIDMMCAPS`, `:50534`).

**Relationship between bits 5 and 6 (important for Step 2):** `VirtualAddressingSupported` (bit 5) is the umbrella "this adapter does WDDM 2.0 GPU virtual addressing"; `GpuMmuSupported` (bit 6) and `IoMmuSupported` (bit 7) select *which* model. The conceptual doc text in `gpu-paravirtualization.md:133` ties PV explicitly to this struct: "A KMD that supports GPU paravirtualization needs to set the **DXGK_VIDMMCAPS::ParavirtualizationSupported** capability." And `gpu-paravirtualization.md:442-446` notes the registry escape hatch when a driver omits it: `HKLM\System\CurrentControlSet\Control\GraphicsDrivers\GpuVirtualizationFlags = 1` ("Some drivers don't set the ParavirtualizationSupported cap. In this case, add the following registry…"). (NB: the Helios DWM-root-cause memory records that `GpuVirtualizationFlags=0` did **not** fix the prior failure, so PV-flag toggling alone is not the lever for the render-adapter path.)

**kmd_render current state: PARTIALLY filled — only `SectionBackedPrimary` (bit 3). The entire memory-model opt-in (`VirtualAddressingSupported`/`GpuMmuSupported`/`IoMmuSupported`/`ParavirtualizationSupported`) is ZEROED.** This matches the DWM-root-cause memory's claim that Helios "does NOT query VIDMMCAPS" — more precisely, it never *sets* any VIDMMCAPS opt-in bit beyond SectionBackedPrimary. The code comments make the deliberate omission explicit: `query_adapter_info.rs:258` ("A real memory segment needs a declared GPU memory model (GpuMmu/IoMmu) we don't yet provide") and `:337` ("Do not set GpuMmuSupported until GPU-VA/page-table DDIs are real"). Grep across `kmd_render/src/**` for `GpuMmuSupported|VirtualAddressingSupported|IoMmuSupported|ParavirtualizationSupported|set_GpuMmu` finds **only comments**, never an assignment.

---

#### DXGK_GPUMMUCAPS (DXGKQAITYPE_GPUMMUCAPS = 13, with DXGK_QUERYGPUMMUCAPSIN input)

**Purpose / when dxgkrnl reads it.** Queried *only after* `DXGK_VIDMMCAPS::GpuMmuSupported` is set. dxgkrnl calls `DxgkDdiQueryAdapterInfo` with `Type = DXGKQAITYPE_GPUMMUCAPS (13)` and an input buffer `DXGK_QUERYGPUMMUCAPSIN { PhysicalAdapterIndex: UINT }` (`dxgk_bindings_dump.rs:47884-47899`), expecting `DXGK_GPUMMUCAPS` back. This describes the page-table geometry VidMm will manage: how the GPU MMU is addressed, VA bit width, page-table level count, leaf-table size, and "legacy behaviors." The conceptual model: VidMm owns the GPU VA space and the page tables; "The hardware format of the page tables used by the GPU MMU is unknown to *VidMm* and is abstracted through device driver interfaces (DDIs)… supports a multilevel level translation, including a fixed size page table and a resizable root page table." (`gpummu-model.md:16`).

**Struct shape (bindgen, `dxgk_bindings_dump.rs:47944-47951`, total size 24 bytes `:48794`):**
```rust
pub struct _DXGK_GPUMMUCAPS {
    pub __bindgen_anon_1: _DXGK_GPUMMUCAPS__bindgen_ty_1,        // union { bitfields, Value: UINT } at off 0
    pub PageTableUpdateMode: DXGK_PAGETABLEUPDATEMODE,          // off 4  (:48800)
    pub VirtualAddressBitCount: UINT,                          // off 8  (:48803)
    pub LeafPageTableSizeFor64KPagesInBytes: UINT,             // off 12 (:48806)
    pub PageTableLevelCount: UINT,                             // off 16 (:48810)
    pub LegacyBehaviors: _DXGK_GPUMMUCAPS__bindgen_ty_2,       // off 20 (:48813) — second bitfield struct
}
```

`PageTableUpdateMode` is the enum `DXGK_PAGETABLEUPDATEMODE` (`dxgk_bindings_dump.rs:47467-47473`):
```rust
pub const DXGK_PAGETABLEUPDATE_CPU_VIRTUAL:  Type = 0;
pub const DXGK_PAGETABLEUPDATE_GPU_VIRTUAL:  Type = 1;
pub const DXGK_PAGETABLEUPDATE_GPU_PHYSICAL: Type = 2;
```
For the fake/decorative model the most relevant value is **`DXGK_PAGETABLEUPDATE_CPU_VIRTUAL = 0`** — it lets the (fake) page tables be written by the CPU through a normal VA, i.e. no real GPU-side PTE walk is implied by the *update path* (the host owns the real MMU regardless).

**Exact bindgen bitfield accessor names + verified bit positions for the main cap word (`_DXGK_GPUMMUCAPS__bindgen_ty_1__bindgen_ty_1`, getters at `:47973`+, each `fn X(&self)->UINT` / `set_X` / `X_raw` / `set_X_raw`):**

| Bit | Accessor | Source line |
|----:|----------|-------------|
| 0 | `ReadOnlyMemorySupported` | `:47975` (get N=0 `:47976`) |
| 1 | `NoExecuteMemorySupported` | `:48011` (`:48012`) |
| 2 | `ZeroInPteSupported` | `:48047` (`:48048`) |
| 3 | `ExplicitPageTableInvalidation` | `:48083` (`:48084`) |
| 4 | `CacheCoherentMemorySupported` | `:48119` (`:48120`) ← cache-coherency cap |
| 5 | `PageTableUpdateRequireAddressSpaceIdle` | `:48155` (`:48156`) |
| 6 | `LargePageSupported` | `:48194` (`:48195`) |
| 7 | `DualPteSupported` | `:48230` (`:48231`) |
| 8 | `AllowNonAlignedLargePageAddress` | `:48266` (`:48267`) |
| 9 | `SysMem64KBPageSupported` | `:48302` (`:48303`) |
| 10 | `InvalidTlbEntriesNotCached` | `:48338` (`:48339`) |
| 11 | `SysMemLargePageSupported` | `:48374` (`:48375`) |
| 12 | `CachedPageTables` | `:48410` (`:48411`) |
| 13..31 | `Reserved` (19 bits) | `:48446` (get N=13 width=19 `:48447`) |

**`LegacyBehaviors` second bitfield struct (`_DXGK_GPUMMUCAPS__bindgen_ty_2`, `:48677-48701`):**
- bit 0: `SourcePageTableVaInTransfer` / `set_SourcePageTableVaInTransfer` (`:48692` / `:48696`).

For the fake model the GPUMMUCAPS scalars Step 2 must choose: `VirtualAddressBitCount` (e.g. 40–48), `PageTableLevelCount` (e.g. 4), `LeafPageTableSizeFor64KPagesInBytes`, and `PageTableUpdateMode = CPU_VIRTUAL`. The cache-coherency-relevant cap here is `CacheCoherentMemorySupported` (bit 4). Confirmed: struct exists (`_DXGK_GPUMMUCAPS`, alias `DXGK_GPUMMUCAPS = _DXGK_GPUMMUCAPS`, `:48824`); `DXGK_QUERYGPUMMUCAPSIN` exists (`:47884`); `DXGKQAITYPE_GPUMMUCAPS = 13` exists (`:44526`).

**viogpu3d quoted impl: NONE.** viogpu3d does not handle `DXGKQAITYPE_GPUMMUCAPS` at all — its `QueryAdapterInfo` switch (`viogpu_adapter.cpp:454-575`) has only `DXGKQAITYPE_UMDRIVERPRIVATE` (`:456`), `DXGKQAITYPE_DRIVERCAPS` (`:473`), `DXGKQAITYPE_QUERYSEGMENT3` (`:520`), and `default: return STATUS_NOT_SUPPORTED` (`:570-573`). It never declares GpuMmu, so dxgkrnl never asks for GPUMMUCAPS. (Its `GetNodeMetadata` likewise declares nothing GpuMmu-ish: `pGetNodeMetadata->EngineType = DXGK_ENGINE_TYPE_3D; pGetNodeMetadata->Flags.Value = 0;` — `driver.cpp:354-355`.)

**kmd_render current state: MISSING.** `query_adapter_info.rs:46-79` has no arm for `DXGKQAITYPE_GPUMMUCAPS`; type 13 falls to the catch-all `other => STATUS_NOT_SUPPORTED` (`:71-77`). Helios's `dxgkddi_get_node_metadata` also explicitly leaves any GpuMmu declaration off: "Do not set GpuMmuSupported until GPU-VA/page-table DDIs are real. FriendlyName, Flags, IoMmuSupported stay zeroed." (`query_adapter_info.rs:337-338`); it sets only `node.EngineType = DXGK_ENGINE_TYPE_3D` (`:336`).

---

#### DXGKQAITYPE_UMDRIVERPRIVATE (relevance)

Not part of the memory model, but viogpu3d uses it as the UMD↔KMD private capability channel (`viogpu_adapter.cpp:456-471`): it returns a `VIOGPU_ADAPTERINFO { IamVioGPU; Flags.Supports3d; SupportedCapsetIDs }`. Helios does **not** handle `DXGKQAITYPE_UMDRIVERPRIVATE` (absent from the `query_adapter_info.rs:46` match → `STATUS_NOT_SUPPORTED`). For the fake-VidMm goal this matters only insofar as a future DXVK-based UMD may need a private cap handshake; it is orthogonal to whether VidMm accepts the adapter.

---

#### Segment declaration (DXGKQAITYPE_QUERYSEGMENT*) — how the memory model shows up in segment flags

The memory model is *also* implied by the segment descriptors, because a CPU-visible **memory** segment (Aperture=0) is what VidMm rejects without a declared GpuMmu/IoMmu model. viogpu3d (WDDM 1.3) sidesteps this by declaring an **aperture** segment:

```cpp
// viogpu_adapter.cpp:553-564  (DXGKQAITYPE_QUERYSEGMENT3 path)
pSegmentDesc[0].BaseAddress.QuadPart = 0xC0000000;
pSegmentDesc[0].Flags.Aperture = TRUE;
pSegmentDesc[0].Flags.CacheCoherent = TRUE;
pSegmentDesc[0].Flags.CpuVisible = FALSE;
pSegmentDesc[0].Size = 256 * 1024 * 4096;
pSegmentDesc[0].CommitLimit = 256 * 1024 * 4096;
pSegmentDesc[0].Flags.DirectFlip = TRUE;
```

Helios mirrors this with the v4 query and the equivalent `DXGK_SEGMENTDESCRIPTOR4`/`DXGK_SEGMENTFLAGS2` accessors (`query_adapter_info.rs:298-306`): `seg.Flags.__bindgen_anon_1.__bindgen_anon_1.set_CpuVisible(1)` and `set_Aperture(1)`. The relevant bindgen accessors for `DXGK_SEGMENTDESCRIPTOR4`'s flags (`_DXGK_SEGMENTFLAGS2`, `dxgk_bindings_dump.rs:53473`+): `Aperture` = bit 0 (`:53501`/get `:53502`), `PopulatedFromSystemMemory` = bit 1 (`:53537`), `SystemMemoryReservedByBios` = bit 2 (`:53573`), `CpuVisible` = bit 3 (`:53609`/get `:53610`), `Reserved` = bits 4..31 (`:53645`). (The older `DXGK_SEGMENTFLAGS` for v1/v3 has `Aperture` bit 0 `:52205`, `CpuVisible` `:52277`, `CacheCoherent` `:52349`, `DirectFlip` `:52565`.) The inline code comment at `query_adapter_info.rs:249-262` records the empirical wall: a CPU-visible *memory* segment whose `CpuTranslatedAddress` points at the host-visible BAR is "REJECTED by VidMm right after `DxgkDdiCreateDevice` (clean-boot Code 43 / FAILED_POST_START)… independent of segment size" — i.e., the missing memory-model opt-in is *exactly* what blocks a real memory segment. The doc rationale: VidMm must manage all non-hidden memory and enforce per-process fairness, so it will not accept a CPU-visible memory segment it cannot address-validate (`using-memory-segments-to-describe-the-gpu-address-space.md:38, 58`; `configuring-memory-segment-types.md:15-21` distinguishes memory-space vs aperture-space segments).

---

#### Minimal cap combination that declares GpuMmu (best read of the evidence)

To make VidMm accept a **GpuMmu** memory model on a WDDM 3.2 render adapter, the evidence points to this minimal, mutually-consistent set — and *nothing more* than the paths actually backed:

1. **In `DXGK_DRIVERCAPS` (DXGKQAITYPE_DRIVERCAPS):**
   - `WDDMVersion = DXGKDDI_WDDMv3_2` (already set, `query_adapter_info.rs:100`).
   - Keep the proven-mandatory load bits Helios already sets: `PresentationCaps.SupportKernelModeCommandBuffer`, `FlipCaps.FlipOnVSyncMmIo` (regress→Code43 if dropped, `:124-130`), `SchedulingCaps.MultiEngineAware|PreemptionAware`, `MemoryManagementCaps.SectionBackedPrimary`.
   - **Add to `MemoryManagementCaps` (= DXGK_VIDMMCAPS):** `set_VirtualAddressingSupported(1)` (bit 5) **and** `set_GpuMmuSupported(1)` (bit 6). These two together are the GpuMmu opt-in per `gpu-virtual-memory-in-wddm-2-0.md:31`. Do **not** also set `IoMmuSupported` (bit 7) unless the IoMmu path is implemented — they are independent opt-ins ("either or both"). `ParavirtualizationSupported` (bit 10) is **not** required to declare GpuMmu and should be left **off** (the DWM-root-cause memory shows the PV/`GpuVirtualizationFlags` lever did not govern our path); set it only if the GPU-PV VM-bus contract is genuinely implemented. Leave `PagingNode = 0` (single node).

2. **Add a handler for `DXGKQAITYPE_GPUMMUCAPS` (type 13)** returning `DXGK_GPUMMUCAPS` with the *decorative* geometry: `PageTableUpdateMode = DXGK_PAGETABLEUPDATE_CPU_VIRTUAL (0)`, a plausible `VirtualAddressBitCount`, `PageTableLevelCount`, and `LeafPageTableSizeFor64KPagesInBytes`. Keep optional behavior bits **off** unless backed (`LargePageSupported`, `DualPteSupported`, etc. all 0). `CacheCoherentMemorySupported` (bit 4) is the only behavior bit worth considering on, since the host-visible-blob coherence story is real at fence/readback points; but it must match the segment `CacheCoherent` flag and the BuildPagingBuffer behavior, so treat it as opt-in-when-backed.

3. **Segment surface (DXGKQAITYPE_QUERYSEGMENT4):** with GpuMmu declared, a real CPU-visible **memory** segment (Aperture=0, CpuVisible=1) becomes *legal* (it was the missing model that got it rejected, `query_adapter_info.rs:249-258`). Step 2's decision is whether to keep the proven aperture segment (Aperture=1) or switch to the over-sized memory segment that the fake model is meant to enable — that choice belongs to the segment/residency section, but the *gate* that unlocks it is the VIDMMCAPS `GpuMmuSupported` bit set here.

In short: the **minimum** is `DXGK_VIDMMCAPS.VirtualAddressingSupported (bit5) + GpuMmuSupported (bit6)` inside the existing DRIVERCAPS fill, plus a new `DXGKQAITYPE_GPUMMUCAPS (13)` handler returning a decorative page-table geometry with `PageTableUpdateMode = CPU_VIRTUAL`. Everything else (`IoMmuSupported`, `ParavirtualizationSupported`, the GPUMMUCAPS behavior bits) stays **off** until its DDI path is real — which is consistent with both the checklist rule Helios cites ("unknown must stay unadvertised", `query_adapter_info.rs:64-65`) and viogpu3d's proven minimalism (one VIDMMCAPS bit, aperture segment, no model declaration at WDDM 1.3). The crucial unknown flagged in the project memory — *does VidMm accept a decorative GpuMmu?* — reduces specifically to: does setting bits 5+6 here plus a CPU_VIRTUAL GPUMMUCAPS, while the BuildPagingBuffer/page-table-update DDIs are no-ops backed by venus-by-resource-id, survive past `DxgkDdiCreateDevice` without Code 43. That is the question Step 2's kernel-debugger session must answer.

### A2. Memory segments — QUERYSEGMENT3/4, DXGK_SEGMENTDESCRIPTOR4, segment flags & types

#### A2.0 The segment contract (what VidMm is asking and why it matters for fake-GpuMmu)

During adapter bring-up VidMm calls `DxgkDdiQueryAdapterInfo` with `Type = DXGKQAITYPE_QUERYSEGMENT3` (older) or `DXGKQAITYPE_QUERYSEGMENT4` (current/WDDM 3.2). The driver describes the GPU's address space — how many segments, their kind (memory vs aperture), CPU visibility, cache coherency, base address, size, and commit limit. This is the single place where the driver tells VidMm "here is the memory model you may place allocations into." Two enum values are confirmed in the bindgen dump:

```
44518:    pub const DXGKQAITYPE_QUERYSEGMENT3: Type = 5;
44524:    pub const DXGKQAITYPE_QUERYSEGMENT4: Type = 11;
```
(/home/rupansh/helios-vgpu/dxgk_bindings_dump.rs:44518, :44524 — also present: `QUERYSEGMENT=2`, `QUERYSEGMENT2=4`, `QUERYSEGMENTCOUNT=43`, `QUERYSEGMENT5=44`.)

The query is two-pass: VidMm first calls with a NULL descriptor pointer to learn `NbSegment`, then calls again with an array sized for that count. Both viogpu3d (line 534) and Helios (line 288) branch on the descriptor-pointer being NULL/non-NULL.

#### A2.1 Display-doc passages: memory segments vs aperture segments, CpuVisible, VA mapping, DMA buffers

**Linear memory-space segment** (real VRAM the GPU reads directly — the GPU owns the bits):

> "A linear memory-space segment is the classical type of segment that display hardware uses. The linear memory-space segment conforms to the following model: It virtualizes video memory located on the graphics adapter. The GPU accesses it directly; that is, without redirection through page mapping. It's managed linearly in a one-dimensional address space."
> "The driver sets the **Flags** member of the [DXGK_SEGMENTDESCRIPTOR] structure to 0 to specify a linear memory-space segment. However, the driver can set the following bit-field flags to indicate other segment support: **CpuVisible** to indicate that the segment is CPU-accessible. **UseBanking** ..."
— `linear-memory-space-segments.md:14-26`

**Linear aperture-space segment** (an *address space only*, no bits; system pages are redirected into it by the KMD's paging buffer):

> "A linear aperture-space segment is similar to a linear memory-space segment. However, the aperture-space segment is only an address space and can't hold bits."
> "To hold the bits, system memory pages must be allocated, and the address-space range must be redirected to refer to those pages. The kernel-mode display miniport driver (KMD) must implement the [DxgkDdiBuildPagingBuffer] function for DXGK_OPERATION_MAP_APERTURE_SEGMENT and DXGK_OPERATION_UNMAP_APERTURE_SEGMENT operation types to handle the redirection ... Dxgkrnl calls DxgkDdiBuildPagingBuffer with the address-space range to be redirected and the MDL that references the physical system memory pages that were allocated."
> "The KMD typically accomplishes the redirection of the address-space range by programming a page table, which is unknown to the video memory manager (VidMm)."
> "The driver must set the **Aperture** bit-field flag ... to specify a linear aperture-space segment. The driver can also set the following bit-field flags ...: **CpuVisible** ... **CacheCoherent** to indicate that the segment maintains cache coherency with the CPU for the pages to which the segment redirects."
— `linear-aperture-space-segments.md:14-24`

**CpuVisible flag definition** (which segment kind is lockable by the CPU):

> "The driver indicates whether a segment is CPU-accessible through the **CpuVisible** flag, which is in the **Flags** member of the DXGK_SEGMENTDESCRIPTOR structure."
> "Cached CPU-accessible allocations must reside within an aperture segment or not be resident in order to be locked. We can't guarantee cache coherency between the CPU and a memory segment on the graphics processing unit (GPU)."
> "CPU-accessible allocations located in a fully CPU-accessible memory segment (resized using the resizable BAR) are guaranteed to be lockable and able to return a virtual address. No special constraints are required in this scenario."
> "CPU-accessible allocations located within a non-CPU-accessible memory segment (with or without access to a CpuHostAperture) can fail to be mapped ... all CPU-accessible allocations in non-CPU-accessible memory segments must contain an aperture segment in their supported segment set."
— `allocation-usage-tracking.md:30,34-36`

**CpuHostAperture** (the PCI-aperture page manager VidMm uses to map non-CPU-visible VRAM into a CPU VA without resizable BAR — this is the second union member of SEGMENTDESCRIPTOR4):

> "To better support locking with non-CPU-accessible memory segments when resizing the BAR fails, a CpuHostAperture is provided in the PCI aperture. The CpuHostAperture behaves as a page-based manager, which can then be mapped directly to regions of video memory via the [DxgkDdiMapCpuHostAperture] DDI function. The VidMm can then map a range of virtual address space directly to a noncontiguous range of the CpuHostAperture ..."
> "The maximum amount of lockable memory that the CPU can reference within non-CPU-accessible memory segments is limited to the size of the CpuHostAperture."
— `allocation-usage-tracking.md:49-51`

**Per-process VA-to-segment mapping** — allocations declare their *supported segment set* (a bitmask of segment ids) at creation; VidMm picks where to page them in:

> "The display miniport driver specifies and returns information about its memory segments that it prefers the video memory manager use when the video memory manager calls the driver's [DxgkDdiCreateAllocation] function. ... The driver returns identifiers of supported segments and segment preferences in the DXGK_ALLOCATIONINFO structures ... From the returned segment information, the video memory manager determines the appropriate memory segment to page-in for the given operation."
— `specifying-segments-when-creating-allocations.md:17-19`

**How DMA buffers specify segments** (DmaBufferSegmentSet at CreateDevice selects whether DMA buffers come from aperture segments or contiguous nonpaged memory):

> "When the Microsoft DirectX graphics kernel subsystem calls the display miniport driver's [DxgkDdiCreateDevice] function ..., the display miniport driver can specify a segment set from which the video memory manager can allocate DMA buffers. If the display miniport driver sets the **DmaBufferSegmentSet** member of the DXGK_DEVICEINFO structure to 0, then the video memory manager will allocate contiguous nonpaged memory for DMA buffers ... If the display miniport driver sets DmaBufferSegmentSet to nonzero, then the video memory manager will allocate pageable memory and will map the pages to the specified aperture segments. The pages within the aperture segments are revealed to the display miniport driver in a call to its [DxgkDdiSubmitCommand] function."
> "Note that the basic video memory manager model does not support DMA buffers in local video memory."
— `specifying-segments-for-dma-buffers.md:22-24`

**AGP-type aperture** (VidMm maps via the GART driver, not the KMD — not relevant to Helios but distinguishes the third aperture flavor): set the **Agp** bit; "VidMm uses the GART driver to map and unmap system pages. That is, VidMm doesn't involve the KMD" — `agp-type-aperture-space-segments.md:14,16`.

#### A2.2 viogpu3d's segment descriptor — the working virtio template (CONFIRM/CORRECT vs gate5a memory)

viogpu3d handles **only `DXGKQAITYPE_QUERYSEGMENT3`** (confirmed by grep: there is no QUERYSEGMENT4 case in viogpu_adapter.cpp/.h). It reports **exactly one segment**. The verbatim descriptor:

```cpp
520:        case DXGKQAITYPE_QUERYSEGMENT3:
...
533:                DXGK_QUERYSEGMENTOUT3 *pSegmentInfo = (DXGK_QUERYSEGMENTOUT3 *)pQueryAdapterInfo->pOutputData;
534:                if (!pSegmentInfo[0].pSegmentDescriptor)
535:                {
536:                    pSegmentInfo->NbSegment = 1;
537:                }
538:                else
539:                {
540:                    DXGK_SEGMENTDESCRIPTOR3 *pSegmentDesc = pSegmentInfo->pSegmentDescriptor;
541:                    memset(&pSegmentDesc[0], 0, sizeof(pSegmentDesc[0]));
542:
543:                    pSegmentInfo->PagingBufferPrivateDataSize = 0;
544:
545:                    pSegmentInfo->PagingBufferSegmentId = 1;
546:                    pSegmentInfo->PagingBufferSize = 10 * PAGE_SIZE;
...
553:                    pSegmentDesc[0].BaseAddress.QuadPart = 0xC0000000;
554:                    pSegmentDesc[0].Flags.Aperture = TRUE;
555:                    pSegmentDesc[0].Flags.CacheCoherent = TRUE;
556:                    // pSegmentDesc[0].CpuTranslatedAddress.QuadPart = 0xFFFFFFFE00000000;
557:
558:                    pSegmentDesc[0].Flags.CpuVisible = FALSE;
559:
560:                    // pSegmentDesc[0].Flags.DirectFlip = TRUE;
561:                    pSegmentDesc[0].Size = 256 * 1024 * 4096;
562:                    pSegmentDesc[0].CommitLimit = 256 * 1024 * 4096;
563:
564:                    pSegmentDesc[0].Flags.DirectFlip = TRUE;
565:                }
```
— /home/rupansh/helios-vgpu/virtio-research-only-3d/viogpu/viogpu3d/viogpu_adapter.cpp:520-565

**Verbatim descriptor (exact values):**
- 1 segment (`NbSegment = 1`), segment id 1.
- `BaseAddress.QuadPart = 0xC0000000` (GPU-VA base of the aperture window).
- `Flags.Aperture = TRUE` → linear aperture-space segment.
- `Flags.CacheCoherent = TRUE`.
- `Flags.CpuVisible = FALSE`.
- `Flags.DirectFlip = TRUE` (set at line 564; note line 560's earlier DirectFlip is commented out, but 564 sets it live).
- `CpuTranslatedAddress` left at 0 (the `0xFFFFFFFE00000000` assignment at line 556 is commented out).
- `Size = 256 * 1024 * 4096` = 0x4000_0000 = **1 GiB**; `CommitLimit` = same 1 GiB.
- Paging buffer: `PagingBufferSegmentId = 1`, `PagingBufferSize = 10 * PAGE_SIZE` (40 KiB), `PagingBufferPrivateDataSize = 0`.

**CONFIRMATION of the gate5a memory claim**: the gate5a note ("viogpu3d uses an APERTURE segment Aperture=1, CacheCoherent=1, CpuVisible=0, BaseAddress=0xC0000000, DirectFlip — backed by OS system-memory MDLs + TRANSFER_TO_HOST") is **CONFIRMED EXACTLY** by lines 553-564. One correction/precision: the memory does not state the size — it is **1 GiB** (`Size == CommitLimit == 256*1024*4096`), and viogpu3d uses **SEGMENTDESCRIPTOR3 / QUERYSEGMENTOUT3**, not the 4 variant. The MDL/TRANSFER_TO_HOST backing is the aperture-segment redirection model from `linear-aperture-space-segments.md` (KMD implements MAP/UNMAP_APERTURE_SEGMENT in DxgkDdiBuildPagingBuffer); confirming that backing path is in viogpu_command.cpp/viogpu_allocation.cpp (out of this section's scope).

For reference, the v3 descriptor struct viogpu3d writes:
```
53855:pub struct _DXGK_SEGMENTDESCRIPTOR3 {
53856:    pub Flags: DXGK_SEGMENTFLAGS,
53857:    pub BaseAddress: PHYSICAL_ADDRESS,
53858:    pub CpuTranslatedAddress: PHYSICAL_ADDRESS,
53859:    pub Size: SIZE_T,
53860:    pub NbOfBanks: UINT,
53861:    pub pBankRangeTable: *mut SIZE_T,
53862:    pub CommitLimit: SIZE_T,
53863:    pub SystemMemoryEndAddress: SIZE_T,
53864:    pub Reserved: SIZE_T,
53865:}
```
— /home/rupansh/helios-vgpu/dxgk_bindings_dump.rs:53855-53865 (size 72, align 8). Note v3 has `CpuTranslatedAddress` as a *direct* field (not a union); v4 promotes it to a union with `CpuHostAperture` (below).

#### A2.3 Exact bindgen types — DXGK_SEGMENTDESCRIPTOR4, DXGK_SEGMENTFLAGS, the union, the OUT containers

**DXGK_QUERYSEGMENTOUT4** (the container Helios writes; `pSegmentDescriptor` is a *raw byte pointer* + a stride, unlike v3's typed pointer):
```
54083:#[derive(Debug, Copy, Clone)]
54084:pub struct _DXGK_QUERYSEGMENTOUT4 {
54085:    pub NbSegment: UINT,
54086:    pub pSegmentDescriptor: *mut BYTE,
54087:    pub PagingBufferSegmentId: UINT,
54088:    pub PagingBufferSize: UINT,
54089:    pub PagingBufferPrivateDataSize: UINT,
54090:    pub SegmentDescriptorStride: SIZE_T,
54091:}
```
— /home/rupansh/helios-vgpu/dxgk_bindings_dump.rs:54084-54091 (size 40, align 8). Because `pSegmentDescriptor` is `*mut BYTE` and the array is walked by `SegmentDescriptorStride`, Step 2 MUST set `SegmentDescriptorStride = size_of::<DXGK_SEGMENTDESCRIPTOR4>()` and index by `base + i*stride`, NOT by `[i]` on a typed pointer.

**DXGK_QUERYSEGMENTOUT3** (for the v3 path):
```
53915:pub struct _DXGK_QUERYSEGMENTOUT3 {
53916:    pub NbSegment: UINT,
53917:    pub pSegmentDescriptor: *mut DXGK_SEGMENTDESCRIPTOR3,
53918:    pub PagingBufferSegmentId: UINT,
53919:    pub PagingBufferSize: UINT,
53920:    pub PagingBufferPrivateDataSize: UINT,
53921:}
```
— :53915-53921 (size 32). **DXGK_QUERYSEGMENTIN4** is just `{ pub PhysicalAdapterIndex: UINT }` (:53959-53961).

**DXGK_SEGMENTDESCRIPTOR4** — the struct Helios reports:
```
53976:pub struct _DXGK_SEGMENTDESCRIPTOR4 {
53977:    pub Flags: DXGK_SEGMENTFLAGS,
53978:    pub BaseAddress: PHYSICAL_ADDRESS,
53979:    pub Size: SIZE_T,
53980:    pub CommitLimit: SIZE_T,
53981:    pub SystemMemoryEndAddress: SIZE_T,
53982:    pub __bindgen_anon_1: _DXGK_SEGMENTDESCRIPTOR4__bindgen_ty_1,
53983:    pub NumInvalidMemoryRanges: UINT,
53984:    pub VprRangeStartOffset: SIZE_T,
53985:    pub VprRangeSize: SIZE_T,
53986:    pub VprAlignment: UINT,
53987:    pub NumVprSupported: UINT,
53988:    pub VprReserveSize: UINT,
53989:    pub NumUEFIFrameBufferRanges: UINT,
53990:}
```
— :53976-53990 (size 96, align 8). `Flags` at offset 0, `BaseAddress` at 8, `Size` at 16, `CommitLimit` at 24, `SystemMemoryEndAddress` at 32, the union at 40.

**The union member** `CpuTranslatedAddress` vs `CpuHostAperture` (bindgen represents a C union with `__BindgenUnionField` wrappers over a backing byte/word array):
```
53992:pub struct _DXGK_SEGMENTDESCRIPTOR4__bindgen_ty_1 {
53993:    pub CpuTranslatedAddress: __BindgenUnionField<PHYSICAL_ADDRESS>,
53994:    pub CpuHostAperture: __BindgenUnionField<DXGK_CPUHOSTAPERTURE>,
53995:    pub bindgen_union_field: [u64; 2usize],
53996:}
```
— :53992-53996 (size 16, both members at offset 0). To write a member, Step 2 uses `__BindgenUnionField::as_mut()` (e.g. `*seg.__bindgen_anon_1.CpuTranslatedAddress.as_mut() = paddr;`). For a **CPU-visible memory segment** you'd set `CpuTranslatedAddress` (guest-physical of the CPU window); `CpuHostAperture` is the alternate page-manager path described in A2.1.

`DXGK_CPUHOSTAPERTURE` (the second union arm):
```
49449:pub struct _DXGK_CPUHOSTAPERTURE {
49450:    pub PhysicalAddress: UINT64,
49451:    pub SizeInPages: UINT32,
49452:}
```
— :49449-49452 (size 16, align 8).

**DXGK_SEGMENTFLAGS** — nested anonymous union of a bitfield struct and a `Value: UINT`:
```
52177:pub struct _DXGK_SEGMENTFLAGS {
52178:    pub __bindgen_anon_1: _DXGK_SEGMENTFLAGS__bindgen_ty_1,
52179:}
52182:pub union _DXGK_SEGMENTFLAGS__bindgen_ty_1 {
52183:    pub __bindgen_anon_1: _DXGK_SEGMENTFLAGS__bindgen_ty_1__bindgen_ty_1,
52184:    pub Value: UINT,
52185:}
52189:pub struct _DXGK_SEGMENTFLAGS__bindgen_ty_1__bindgen_ty_1 {
52190:    pub _bitfield_align_1: [u16; 0],
52191:    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
52192:}
```
— :52177-52192. So the accessor chain is `flags.__bindgen_anon_1.__bindgen_anon_1.set_X(1)` (two nested anon levels), exactly as Helios uses. Each `set_X` takes a `UINT` value.

**Exact bitfield accessor list, by bit position** (all `get(N, 1u8)` / `set(N, 1u8, ...)` 1-bit fields except the last, observed verbatim from the dump):

| Bit | Getter / `set_` accessor | dump line |
|----:|--------------------------|----------|
| 0 | `Aperture` / `set_Aperture` | :52205 / :52209 |
| 1 | `Agp` / `set_Agp` | :52241 |
| 2 | `CpuVisible` / `set_CpuVisible` | :52277 / :52281 |
| 3 | (bit 3 — UseBanking; getter at :52313) | :52314 |
| 4 | `CacheCoherent` / `set_CacheCoherent` | :52349 |
| 5 | `PitchAlignment` | :52385 |
| 6 | `PopulatedFromSystemMemory` | :52421 |
| 7 | `PreservedDuringStandby` | :52457 |
| 8 | `PreservedDuringHibernate` | :52493 |
| 9 | (bit 9 — PartiallyPreservedDuringHibernate; getter at :52529) | :52530 |
| 10 | `DirectFlip` | :52565 |
| 11 | `Use64KBPages` | :52601 |
| 12 | `ReservedSysMem` | :52637 |
| 13 | `SupportsCpuHostAperture` | :52673 |
| 14 | `SupportsCachedCpuHostAperture` | :52709 |
| 15 | `ApplicationTarget` | :52745 |
| 16 | `VprSupported` | :52781 |
| 17 | `VprPreservedDuringStandby` | :52817 |
| 18 | `EncryptedPagingSupported` | :52853 |
| 19 | `LocalBudgetGroup` | :52889 |
| 20 | `NonLocalBudgetGroup` | :52925 |
| 21 | `PopulatedByReservedDDRByFirmware` | :52961 |
| 22 | `Reserved` (10-bit field: `get(22usize, 10u8)`) | :52997 |

All getters and `set_*` setters are `#[inline]` and take/return `UINT` (`u32`); each also has unsafe `_raw` variants (`X_raw`/`set_X_raw`). The bitfield is `__BindgenBitfieldUnit<[u8; 4usize]>` (4 bytes total). For Step 2: set the flag via `seg.Flags.__bindgen_anon_1.__bindgen_anon_1.set_CpuVisible(1)` etc., or write the whole word via `seg.Flags.__bindgen_anon_1.Value`.

Note: bit 3 is **UseBanking** (per `linear-memory-space-segments.md:26`) and bit 9 is **PartiallyPreservedDuringHibernate** by convention; the bindgen getter names for those two lines were not re-quoted by name above (getters are at :52313 and :52529 respectively) — Step 2 should confirm those two names directly from the dump rather than trust the convention.

#### A2.4 Current Helios state — kmd_render `query_segments` (REAL, but placeholder shape)

State: **REAL** (compiles, runs, keeps the adapter at Code 0) but a **deliberate placeholder** — it reports one small CPU-visible *aperture* segment that is explicitly NOT advertised as render-capable GPU memory. Verbatim:

```rust
263:unsafe fn query_segments(args: &DXGKARG_QUERYADAPTERINFO) -> NTSTATUS {
264:    if (args.OutputDataSize as usize) < size_of::<DXGK_QUERYSEGMENTOUT4>() {
265:        return STATUS_BUFFER_TOO_SMALL;
266:    }
...
270:    let out = unsafe { &mut *(args.pOutputData as *mut DXGK_QUERYSEGMENTOUT4) };
271:    let segment_descriptor = out.pSegmentDescriptor;
...   // zero the OUT struct
282:    out.NbSegment = 1;
283:    out.SegmentDescriptorStride = size_of::<DXGK_SEGMENTDESCRIPTOR4>() as u64;
284:    out.PagingBufferSegmentId = 1;
285:    out.PagingBufferSize = 64 * 1024;
286:    out.PagingBufferPrivateDataSize = 0;
287:
288:    if !segment_descriptor.is_null() {
...
291:        let seg = unsafe { &mut *(segment_descriptor as *mut DXGK_SEGMENTDESCRIPTOR4) };
292:        unsafe {
293:            core::ptr::write_bytes(seg as *mut _ as *mut u8, 0, size_of::<DXGK_SEGMENTDESCRIPTOR4>());
299:            seg.Flags.__bindgen_anon_1.__bindgen_anon_1.set_CpuVisible(1);
302:            seg.Flags.__bindgen_anon_1.__bindgen_anon_1.set_Aperture(1);
303:        }
304:        seg.BaseAddress.QuadPart = 0;
305:        seg.Size = 64 * 1024 * 1024;
306:        seg.CommitLimit = 64 * 1024 * 1024;
307:    }
308:
309:    STATUS_SUCCESS
310:}
```
— /home/rupansh/helios-vgpu/kmd_render/src/ddi/query_adapter_info.rs:263-310

So Helios currently reports: **1 segment, id 1, `CpuVisible=1 + Aperture=1`, BaseAddress 0, Size = CommitLimit = 64 MiB**, paging-buffer segment id 1, paging-buffer size 64 KiB, descriptor stride = `size_of::<DXGK_SEGMENTDESCRIPTOR4>()`. Helios uses the **v4** path (the dispatch at line 48 maps `DXGKQAITYPE_QUERYSEGMENT4 => query_segments`); v3 is not handled. It does NOT set `CacheCoherent` or `DirectFlip` (viogpu3d sets both); it sets `CpuVisible=1` where viogpu3d sets `CpuVisible=FALSE`.

The load-bearing comment documenting what was tried and rejected (verbatim):

```rust
249:// Gate 5a Stage 2b finding (2026-06-18): a CPU-visible **memory** segment
250:// (Aperture=0) whose `CpuTranslatedAddress` points at the host-visible BAR — the
251:// approach this function briefly carried (.66–.70) — is REJECTED by VidMm right
252:// after `DxgkDdiCreateDevice` (clean-boot Code 43 / FAILED_POST_START, confirmed
253:// independent of segment size: tested 8 GiB and a 256 MiB sub-window). The proven
254:// virtio-gpu WDDM driver (`virtio-research-only-3d/.../viogpu_adapter.cpp`) uses an
255:// **aperture** segment instead, backing allocations with OS system-memory MDLs via
256:// `BuildPagingBuffer` MAP_APERTURE_SEGMENT and reaching the host with explicit
257:// TRANSFER_TO_HOST copies — never a CPU-visible memory segment. A real memory
258:// segment needs a declared GPU memory model (GpuMmu/IoMmu) we don't yet provide.
259:// So we restore the proven aperture segment here (keeps the adapter at Code 0);
260:// mapping the host-visible BAR to a `D3DKMTLock2` VA needs a different mechanism
261:// (see GATE5_STAGE2_ALLOC_DESIGN.md / the gate5a memory). The host-visible window
262:// scan + `resource_map_blob` plumbing stays in `virtio/gpu.rs` for that work.
```
— /home/rupansh/helios-vgpu/kmd_render/src/ddi/query_adapter_info.rs:249-262

**Key historical finding for Step 2 (verbatim above + corroborated in GATE5_STAGE2_ALLOC_DESIGN.md:38-45, 83-101 and HANDOFF_NEXT_SESSION.md:33-41):** a bare **CpuVisible MEMORY segment** (`set_CpuVisible(1)`, `set_Aperture(0)`) with `CpuTranslatedAddress` = host-visible BAR was rejected by VidMm immediately post-`DxgkDdiCreateDevice` (Code 43 / FAILED_POST_START), *independent of size* (8 GiB and 256 MiB both failed). The stated reason: **"A real memory segment needs a declared GPU memory model (GpuMmu/IoMmu) we don't yet provide."** This is exactly the gap the fake-GpuMmu work must close — the memory segment is only accepted once the driver declares a GpuMmu memory model in WDDMDEVICECAPS/DXGK_VIDMMCAPS and supplies the page-table/GPU-VA DDIs that make VidMm trust the addressing.

#### A2.5 What segment shape the fake-GpuMmu model should report, and why

The fake-but-coherent GpuMmu model needs VidMm to accept a **CPU-visible MEMORY segment** (not an aperture segment), because:

1. **Memory vs aperture.** An *aperture* segment "is only an address space and can't hold bits" (`linear-aperture-space-segments.md:14`) — it forces the viogpu3d model: system-memory MDLs redirected via MAP_APERTURE_SEGMENT + explicit TRANSFER_TO_HOST guest↔host copies. That is the *opposite* of Helios's locked zero-copy design (host-visible BAR via MAP_BLOB, no TRANSFER queues). The desktop/DWM render targets must live in a segment that **holds bits** and is **directly CPU/IDD-readable** — i.e. a CPU-visible *memory* segment whose backing is the host-visible venus BAR. Per `allocation-usage-tracking.md:35`, "allocations located in a fully CPU-accessible memory segment (resized using the resizable BAR) are guaranteed to be lockable and able to return a virtual address. No special constraints are required" — this is the path Lock2/IDD readback wants.

2. **Why it was rejected and what unblocks it.** The bare CpuVisible memory segment is rejected without a declared GPU memory model. The fix is to pair this segment with a **declared GpuMmu memory model** (the decorative page tables): the host owns the real MMU, venus addresses by opaque resource id, so the guest GpuMmu/page-table content is never consulted by hardware — it exists only to satisfy VidMm's verification so it will accept the memory segment + return Lock2 VAs.

3. **CacheCoherent.** Set `set_CacheCoherent(1)` (viogpu3d sets it on its aperture segment, :555) so VidMm treats the segment as coherent — combined with the existing host-visible cache-coherency handling this avoids the manual Invalidate-Cache dance described in `allocation-usage-tracking.md:53-62`.

4. **Over-size for no-eviction (residency invariant).** Set `Size == CommitLimit` to a value far larger than any plausible working set so VidMm never needs to evict (the locked-goal residency strategy: "over-size the segment so nothing evicts"). viogpu3d uses 1 GiB (`256*1024*4096`); the gate5a memory tried 8 GiB on the memory-segment attempt. The fake model should report a large memory segment (multi-GiB, bounded by the host-visible BAR window length reported by `scan_host_visible_window()` if that is the backing) so the desktop's render targets + DWM compositor surfaces all fit resident.

5. **The descriptor to write (DXGK_SEGMENTDESCRIPTOR4, v4 path Helios already uses):**
   - `Flags.set_CpuVisible(1)`, `Flags.set_Aperture(0)` (memory segment), `Flags.set_CacheCoherent(1)`; leave `DirectFlip` off unless a flip path needs it.
   - `BaseAddress.QuadPart` = the GPU-VA base for the segment.
   - Union arm `CpuTranslatedAddress` = guest-physical base of the host-visible BAR window (`__bindgen_anon_1.CpuTranslatedAddress.as_mut()`), **OR** use `CpuHostAperture { PhysicalAddress, SizeInPages }` + `Flags.set_SupportsCpuHostAperture(1)` if going the CpuHostAperture route (`allocation-usage-tracking.md:47-51`).
   - `Size = CommitLimit =` over-sized window length.
   - `out.NbSegment = 1` (segment id 1), `out.SegmentDescriptorStride = size_of::<DXGK_SEGMENTDESCRIPTOR4>()`, plus a small paging-buffer segment (`PagingBufferSegmentId`, `PagingBufferSize`) as today.
   - Keep the two-pass NULL-pointer protocol (Helios line 288).

**Open verification for Step 2 (from the rejected-attempt history):** the memory segment is only accepted once GpuMmu caps + the GPU-VA/page-table DDIs are declared — A2.4's comment says the failure was "FAILED_POST_START" right after CreateDevice. Step 2 must land the GpuMmu cap declaration *and* this memory-segment shape together, then confirm with a kernel debugger that VidMm accepts the decorative GpuMmu (the open unknown recorded in the project memory). If the BAR-backed memory segment still fails, the documented fallback is to keep the current 64 MiB CpuVisible aperture placeholder (Code-0 safe) while iterating.

—

Sources consulted (all paths absolute): `/home/rupansh/helios-vgpu/dxgk_bindings_dump.rs` (lines cited inline), `/home/rupansh/helios-vgpu/virtio-research-only-3d/viogpu/viogpu3d/viogpu_adapter.cpp:520-565`, `/home/rupansh/helios-vgpu/kmd_render/src/ddi/query_adapter_info.rs:48,249-310`, and display docs `linear-memory-space-segments.md`, `linear-aperture-space-segments.md`, `agp-type-aperture-space-segments.md`, `allocation-usage-tracking.md`, `specifying-segments-for-dma-buffers.md`, `specifying-segments-when-creating-allocations.md` (all under `/home/rupansh/helios-vgpu/windows-driver-docs-research-only/windows-driver-docs-pr/display/`), plus corroborating project notes `/home/rupansh/helios-vgpu/GATE5_STAGE2_ALLOC_DESIGN.md:38-101` and `/home/rupansh/helios-vgpu/HANDOFF_NEXT_SESSION.md:33-41`.

### A3. The GpuMmu virtual-address model — per-process VA spaces, page tables, DXGK_PTE

This section establishes the *reality VidMm believes it is managing* the moment Helios reports `DXGK_VIDMMCAPS` GpuMmu support. Everything below is what dxgkrnl/VidMm assumes is true about GPU virtual addressing in WDDM 2.0+. The crux for Helios: because venus addresses host resources by opaque id and the host GPU owns the real MMU, almost all of the *content* of this model is unobserved by the host — but VidMm still *maintains the bookkeeping internally* and still *calls our DDIs to build the page-table abstraction*. The decorative question is precisely "which of these calls can we no-op vs which must produce structurally-valid state VidMm later reads back."

#### A3.1 Why GpuMmu exists, and the two memory models (GpuMmu vs IoMmu)

From `gpu-virtual-memory-in-wddm-2-0.md` (the "Introduction" + "GPU memory models" sections, lines 17–43), the WDDM 2.0 rationale and the GpuMmu/IoMmu split, verbatim:

> "Before WDDM 2.0, the device driver interface (DDI) was built such that GPU engines were expected to reference memory through segment physical addresses. As segments were shared across applications and over-committed, resources got relocated through their lifetime and their assigned physical addresses changed. This process required memory references to be tracked inside command buffers through allocation and patch location lists. … This tracking and patching was expensive. It essentially imposed a scheduling model where the video memory manager (VidMm) had to inspect every packet before it could be submitted to an engine." (`gpu-virtual-memory-in-wddm-2-0.md:17`)

> "To do so, WDDM supports GPU virtual addressing starting in WDDM 2.0. In this model, each process gets assigned a unique GPU virtual address (GPUVA) space that every GPU context can execute in. An allocation created or opened by a process gets assigned a unique GPUVA within that process's GPU virtual address space. This assigned GPUVA remains constant and unique for the lifetime of the allocation. The user-mode display driver (UMD) is thus able to reference allocations through their GPU virtual address without having to worry about the underlying physical memory changing through its lifetime." (`gpu-virtual-memory-in-wddm-2-0.md:21`)

The two models, verbatim from the same file:

> "WDDM v2 supports two distinct models for GPU virtual addressing, *GpuMmu* and *IoMmu*. A driver must [opt-in] to support either or both of the models. A single GPU node can support both modes simultaneously." (`gpu-virtual-memory-in-wddm-2-0.md:31`)

> "### GpuMmu model … In the *GpuMmu* model, VidMm manages the GPU memory management unit and underlying page tables. VidMm also exposes services to the UMD that allow it to manage GPU virtual address mapping to allocations. GpuMmu implies that the GPU uses GPU page tables to access data. The page tables could point to system memory or local device memory." (`gpu-virtual-memory-in-wddm-2-0.md:33-35`)

> "### IoMmu model … In the *IoMmu* model, both the CPU and GPU share a common address space and CPU page tables. Only system memory can be accessed in this case, so IoMmu is suitable for integrated GPUs. … There's no need to manage a separate set of page tables in GPU-accessible memory." (`gpu-virtual-memory-in-wddm-2-0.md:39-41`)

Note also the engine-mode distinction, because it bears directly on how much addressing reality Helios must produce. From the same file:

> "* In physical mode, the scheduling model remains the same as it is with WDDM v1.x. The UMD continues to generate the allocation and patch location lists. … * In virtual mode, an engine references memory through GPU virtual addresses. The UMD generates command buffers directly from user mode and uses new services to submit those commands to the kernel. The UMD doesn't generate allocation or patch location lists, although it's still responsible for managing the residency of allocations." (`gpu-virtual-memory-in-wddm-2-0.md:25-27`)

#### A3.2 Who owns the page tables (VidMm builds them; the driver only fills PTEs via paging operations)

The authoritative ownership statement is in `gpummu-model.md` (line 16), verbatim:

> "Each process has separate CPU and GPU virtual address spaces that use distinct page tables. The video memory manager (*VidMm*) manages the GPU virtual address space of all processes. *VidMm* is also responsible for allocating, growing, updating, ensuring residency, and freeing page tables. The hardware format of the page tables used by the GPU MMU is unknown to *VidMm* and is abstracted through device driver interfaces (DDIs). The abstraction supports a multilevel level translation, including a fixed size page table and a resizable root page table." (`gpummu-model.md:16`)

And critically (the line that decides whether a decorative GpuMmu can work) — **VidMm does not invent addresses; the UMD assigns them, and the KMD only translates the abstract PTE into hardware PTEs**:

> "Although *VidMm* is responsible for managing the GPU virtual address space and its underlying page tables, *VidMm* doesn't automatically assign GPU virtual addresses to allocations. This responsibility falls on the user-mode driver (UMD)." (`gpummu-model.md:18`)

The division of labor on the PTE itself is stated in `gpu-virtual-address.md:40`:

> "The [**DXGK_PTE**] structure is used through the DDI to represent a page table entry. This structure represents information about each entry, which the DirectX graphics kernel (*Dxgkrnl*) manages. The driver uses this information to build hardware-specific page table entries." (`gpu-virtual-address.md:40`)

So the contract is: **Dxgkrnl/VidMm owns the abstract page-table content and hands the KMD an array of `DXGK_PTE`; the KMD's only job is to translate those into whatever the hardware needs.** For Helios, "the hardware" is venus, which never reads page tables at all — so the KMD's PTE-translation step is the natural place where the model becomes decorative (the KMD can simply consume the `DXGK_PTE` array and do nothing hardware-meaningful with it, because the host GPU resolves addresses by resource id).

Page tables are *implicit allocations* VidMm creates itself — they have no UMD/KMD handle (`gpu-virtual-address.md:42-46`):

> "## Creation of page table allocations … Page tables are created as implicit allocations and don't have a user-mode driver (UMD) or a KMD handle. To allocate a page table, *VidMm* allocates an allocation of size [**DXGK_PAGE_TABLE_LEVEL_DESC**]::**PageTableSizeInBytes** from the segment, specified in **DXGK_PAGE_TABLE_LEVEL_DESC**::**PageTableSegmentId**. After creation, *VidMm* initializes every entry in the page table to *invalid*. Page tables never change size, except for the root page table in the two-level translation scheme." (`gpu-virtual-address.md:44-46`)

This is load-bearing for Helios: **VidMm allocates the page tables themselves out of a segment Helios declares** (`PageTableSegmentId`). That segment must be real enough that VidMm can place page-table allocations in it and (in the over-size design) never need to evict them. The PTE *content* is decorative; the *backing segment* is not (VidMm physically allocates page-table-sized blocks there).

#### A3.3 The multilevel translation scheme, root page table, and per-process VA space

From `gpu-virtual-address.md` (lines 12–40), the VA layout and the level model, verbatim:

> "GPUVAs are managed in logical 4-KB or 64-KB pages at the device driver interface (DDI) level. … The video memory manager (*VidMm*) supports a multilevel virtual address translation scheme, where several levels of page tables are used to translate a virtual address: * The levels are numbered from zero. Level zero is assigned to the leaf level. * Translation starts from the root level page table." (`gpu-virtual-address.md:12-20`)

> "When the number of page table levels is two, the root level page table can be resized to accommodate a process with variable GPUVA space size. Every level is described by the [**DXGK_PAGE_TABLE_LEVEL_DESC**] structure which the kernel-mode display driver (KMD) fills in during a [*DxgkDdiQueryAdapterInfo*] call. The KMD also fills out the [**DXGK_GPUMMUCAPS**] caps structure to describe the GPUVA support." (`gpu-virtual-address.md:22`)

The per-process binding of the root page table (the call Step 2 must answer, even if trivially):

> "Each process has its own GPUVA space. Before a graphics context of a process can be set for execution, KMD's [*DxgkDdiSetRootPageTable*] function is called to set the root page table address." (`gpu-virtual-address.md:24`)

The bit layout of a GPUVA (so Step 2 knows what `VirtualAddressBitCount` / `PageTableIndexBitCount` it is implicitly promising):

> "* The GPUVA has [**DXGK_GPUMMUCAPS**]::**VirtualAddressBitCount** bits. * The low bits \[0 - 11\] represent an offset in bytes in a page. * The next [**DXGK_PAGE_TABLE_LEVEL_DESC**]::**PageTableIndexBitCount** bits represent the index of a page table entry in a leaf level page table. * The number of entries in a page table is 2^DXGK_PAGE_TABLE_LEVEL_DESC::PageTableIndexBitCount and the page table size is [**DXGK_PAGE_TABLE_LEVEL_DESC**]::**PageTableSizeInBytes** bytes. * The rest of the bits represent an index to a page table entry in the root page table. The root page table is resizable for the two-level translation scheme. The [*DxgkDdiGetRootPageTableSize*] DDI obtains its size." (`gpu-virtual-address.md:30-38`)

Root-table resize lifecycle (VidMm grows/shrinks it itself and re-binds via SetRootPageTable):

> "*VidMm* supports resizing of the root page table in the two-level translation scheme. When a root page table, covering a specified amount of address space, is being created, *VidMm* calls [*DxgkDdiGetRootPageTableSize*] to determine the required allocation size for it. *VidMm* then allocates an allocation of that size in the segment, specified by [**DXGK_PAGE_TABLE_LEVEL_DESC**]::**PageTableSegmentId** for the root level. After creation, *VidMm* initializes every entry in the page table to invalid using the new [*UpdatePageTable*] paging operation. … Once the root page table is created, *VidMm* calls [*DxgkDdiSetRootPageTable*] to associate the newly created root page table with the various contexts that will execute within." (`gpu-virtual-address.md:48`)

> "## Updating page table As surfaces move around in memory, *VidMm* updates the content of page tables to reflect the new location of surfaces." (`gpu-virtual-address.md:64-66`)

> "## Moving a page table *VidMm* can relocate or evict page tables when a device is idle or suspended. When *VidMm* moves a page table, it updates the higher levels page table to reference the new location of the page table. When the root page table itself is relocated, *VidMm* calls [*DxgkDdiSetRootPageTable*] to inform impacted contexts of the new location of their page directory." (`gpu-virtual-address.md:68-72`)

Per-process VA-space structure (two address spaces per process; UMD assigns addresses through callbacks; addresses are *queued/async* against a paging fence) from `per-process-gpu-virtual-address-spaces.md` (lines 11–21), verbatim:

> "Each process is associated with two graphics processing unit (GPU) virtual address spaces, an application GPU virtual address space and a privileged virtual address space." (`per-process-gpu-virtual-address-spaces.md:11`)

> "The application GPU virtual address space is the address space that command buffers, generated by the user mode driver, execute within. This address space is managed by the user mode driver using services provided by the video memory manager. Before an allocation can be accessed by a GPU engine operating in the virtual mode, the user mode driver must assign a GPU virtual address range to the allocation. For regular allocations, this is done using the new [*MapGpuVirtualAddress*] service, exposed by the video memory manager. … *MapGpuVirtualAddress* queues a request to the video memory manager and returns to the user mode driver immediately while the request is processed. The request is queued on the device paging queue and the user mode driver must ensure it synchronizes against the returned device paging fence value." (`per-process-gpu-virtual-address-spaces.md:16`)

The privileged/second VA space is tile-resource-only and is not needed unless Helios declares tile-resource support:

> "Processes using tile resources get a second virtual address space associated with them on the first call to [*ReserveGpuVirtualAddress*]. This address space is used to update the page table of the process synchronously with rendering." (`per-process-gpu-virtual-address-spaces.md:21`)

#### A3.4 How a GPUVA resolves to a segment physical address (and the CPU-visible/aperture mapping)

`mapping-virtual-addresses-to-a-memory-segment.md` is about the *CPU-visible / aperture* mapping of allocations into a segment (the linear-aperture model), which is the same machinery Helios relies on for host-visible BAR readback. Verbatim, lines 19–21:

> "The display miniport driver can specify, for each memory-space or aperture-space segment that it defines, whether CPU virtual addresses can map directly to an allocation located in the segment by setting the **CpuVisible** bit-field flag in the **Flags** member of the [**DXGK_SEGMENTDESCRIPTOR**] structure for the segment." (`mapping-virtual-addresses-to-a-memory-segment.md:19`)

> "To map a CPU virtual address to a segment, the segment should have linear access through the PCI aperture. In other words, the offset of any allocation within the segment should be the same as the offset in the PCI aperture. Therefore, the video memory manager can calculate the bus-relative physical address of any allocation based on the allocation's offset within the given segment." (`mapping-virtual-addresses-to-a-memory-segment.md:21`)

The eviction-to-system-memory machinery (which the over-size-segment design exists to suppress) is described in lines 31–35:

> "If the GPU resources that are associated with an allocation currently mapped for direct application access are evicted, the content of the allocation is transferred to system memory … To set up the transfer, the video memory manager calls the display miniport driver's [**DxgkDdiBuildPagingBuffer**] function to create a paging buffer, and the GPU scheduler calls the driver's [**DxgkDdiSubmitCommand**] function to queue the paging buffer to the GPU execution unit. … However, the driver must ensure that the byte ordering of an allocation through the PCI aperture exactly matches the byte ordering of the allocation when the allocation is evicted." (`mapping-virtual-addresses-to-a-memory-segment.md:35`)

Implication for Helios: the GPUVA→segment-physical resolution in the GpuMmu model is exactly what `DXGK_PTE::Segment` + `DXGK_PTE::PageAddress` encode (see below). VidMm computes a segment+offset, writes it into a `DXGK_PTE`, and hands the array to the KMD; the KMD is supposed to program the GPU MMU so the GPU can later resolve `GPUVA → (Segment, PageAddress)`. The host GPU never does this resolution (venus uses resource ids), so the KMD can accept the PTE array and discard it. But VidMm still computes those `(Segment, PageAddress)` pairs *internally* and *believes* they describe real backing — which is why the segment Helios declares must be physically large/coherent enough to be a self-consistent fiction.

#### A3.5 The `DXGK_PTE` Rust type (verbatim, with exact bitfield accessors)

`DXGK_PTE` is 16 bytes: a flags qword (anonymous union of a bitfield struct and a raw `Flags: ULONGLONG`) followed by an address qword (anonymous union `PageAddress`/`PageTableAddress`). Source: `dxgk_bindings_dump.rs:12266-12878`.

The top-level struct (`dxgk_bindings_dump.rs:12264-12269`):

```rust
#[repr(C)]
#[derive(Copy, Clone)]
pub struct _DXGK_PTE {
    pub __bindgen_anon_1: _DXGK_PTE__bindgen_ty_1,   // flags qword
    pub __bindgen_anon_2: _DXGK_PTE__bindgen_ty_2,   // address qword
}
// ...
pub type DXGK_PTE = _DXGK_PTE;   // dxgk_bindings_dump.rs:12878
```

The flags union (`dxgk_bindings_dump.rs:12270-12281`):

```rust
#[repr(C)]
#[derive(Copy, Clone)]
pub union _DXGK_PTE__bindgen_ty_1 {
    pub __bindgen_anon_1: _DXGK_PTE__bindgen_ty_1__bindgen_ty_1,  // the bitfields
    pub Flags: ULONGLONG,                                        // raw 64-bit overlay
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct _DXGK_PTE__bindgen_ty_1__bindgen_ty_1 {
    pub _bitfield_align_1: [u64; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 8usize]>,
}
```

The **exact bitfields and their bit positions/widths** (each emitted by bindgen with getters `X()`, setters `set_X(val: ULONGLONG)`, plus `X_raw`/`set_X_raw` pointer variants; all values are `ULONGLONG`). From the getters in `dxgk_bindings_dump.rs:12291-12687` and the constructor `new_bitfield_1` at `:12688-12811`:

| Field (accessor) | Bit offset | Width | Bindings line |
|---|---|---|---|
| `Valid()` / `set_Valid()` | 0 | 1 | `:12293` |
| `Zero()` / `set_Zero()` | 1 | 1 | `:12329` |
| `CacheCoherent()` / `set_CacheCoherent()` | 2 | 1 | `:12365` |
| `ReadOnly()` / `set_ReadOnly()` | 3 | 1 | `:12401` |
| `NoExecute()` / `set_NoExecute()` | 4 | 1 | `:12437` |
| `Segment()` / `set_Segment()` | 5 | 5 | `:12473` |
| `LargePage()` / `set_LargePage()` | 10 | 1 | `:12509` |
| `PhysicalAdapterIndex()` / `set_PhysicalAdapterIndex()` | 11 | 6 | `:12545` |
| `PageTablePageSize()` / `set_PageTablePageSize()` | 17 | 2 | `:12581` |
| `SystemReserved0()` / `set_SystemReserved0()` | 19 | 1 | `:12617` |
| `Reserved()` / `set_Reserved()` | 20 | 44 | `:12653` |

Example of the exact accessor shape Step 2 must use (verbatim, `Segment`, `dxgk_bindings_dump.rs:12472-12482`):

```rust
    #[inline]
    pub fn Segment(&self) -> ULONGLONG {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(5usize, 5u8) as u64) }
    }
    #[inline]
    pub fn set_Segment(&mut self, val: ULONGLONG) {
        unsafe {
            let val: u64 = ::core::mem::transmute(val);
            self._bitfield_1.set(5usize, 5u8, val as u64)
        }
    }
```

The address union (`dxgk_bindings_dump.rs:12834-12839`):

```rust
#[repr(C)]
#[derive(Copy, Clone)]
pub union _DXGK_PTE__bindgen_ty_2 {
    pub PageAddress: ULONGLONG,        // leaf PTE: physical page number in the segment
    pub PageTableAddress: ULONGLONG,   // non-leaf PTE: physical page of the next-level page table
}
```

Size/align asserts confirm the layout: `size_of::<_DXGK_PTE>() == 16`, `align_of == 8` (`dxgk_bindings_dump.rs:12866-12867`); each union is 8 bytes (`:12816`, `:12843`). `Default` for all three zero-fills via `write_bytes(.., 0, 1)` (`:12825`, `:12855`, `:12869`).

Semantic read of `DXGK_PTE` for Helios: VidMm fills `Segment` (5 bits → which declared segment backs the page), `PageAddress` (the page number within that segment), and the flag bits (`Valid`, `ReadOnly`, `NoExecute`, `CacheCoherent`, `LargePage`, `Zero`), then hands the array to the KMD via the UpdatePageTable paging operation. **The host GPU never sees any of these fields.** The KMD must accept the array and may discard it (decorative), but the `Segment` index it sees must be one Helios actually declared, and `Valid` PTEs imply the corresponding segment page is backed.

The page-size enum referenced by `PageTablePageSize` (`dxgk_bindings_dump.rs:12258-12263`):

```rust
pub mod _DXGK_PTE_PAGE_SIZE {
    pub type Type = ::core::ffi::c_int;
    pub const DXGK_PTE_PAGE_TABLE_PAGE_4KB: Type = 0;
    pub const DXGK_PTE_PAGE_TABLE_PAGE_64KB: Type = 1;
}
pub use self::_DXGK_PTE_PAGE_SIZE::Type as DXGK_PTE_PAGE_SIZE;
```

#### A3.6 Page-table-describing structs/enums (verbatim)

**`DXGK_PAGE_TABLE_LEVEL_DESC`** — KMD fills one per level in `DxgkDdiQueryAdapterInfo`; tells VidMm how big each page table is and which segment to carve page tables from. (`dxgk_bindings_dump.rs:47537-47543`, alias `:47573`; struct is 20 bytes, align 4):

```rust
pub struct _DXGK_PAGE_TABLE_LEVEL_DESC {
    pub PageTableIndexBitCount: UINT,             // offset 0
    pub PageTableSegmentId: UINT,                 // offset 4  — segment VidMm allocates this level's page tables from
    pub PagingProcessPageTableSegmentId: UINT,    // offset 8
    pub PageTableSizeInBytes: UINT,               // offset 12 — size VidMm allocates per page table at this level
    pub PageTableAlignmentInBytes: UINT,          // offset 16
}
pub type DXGK_PAGE_TABLE_LEVEL_DESC = _DXGK_PAGE_TABLE_LEVEL_DESC;
```

**`DXGK_GPUMMUCAPS`** — the top-level GpuMmu caps struct the KMD fills to declare VA support (`dxgk_bindings_dump.rs:47944-47951`, alias `:48824`):

```rust
pub struct _DXGK_GPUMMUCAPS {
    pub __bindgen_anon_1: _DXGK_GPUMMUCAPS__bindgen_ty_1,   // flags union (4 bytes)
    pub PageTableUpdateMode: DXGK_PAGETABLEUPDATEMODE,      // offset 4
    pub VirtualAddressBitCount: UINT,                      // total GPUVA bit width
    pub LeafPageTableSizeFor64KPagesInBytes: UINT,
    pub PageTableLevelCount: UINT,                         // number of translation levels
    pub LegacyBehaviors: _DXGK_GPUMMUCAPS__bindgen_ty_2,
}
```

Its flags union (`dxgk_bindings_dump.rs:47952-47963`):

```rust
pub union _DXGK_GPUMMUCAPS__bindgen_ty_1 {
    pub __bindgen_anon_1: _DXGK_GPUMMUCAPS__bindgen_ty_1__bindgen_ty_1,  // bitfields
    pub Value: UINT,                                                    // raw 32-bit overlay
}
```

The capability bitfields (each `pub fn X(&self) -> UINT` / `set_X(val: UINT)`, 1 bit each unless noted), in order, from `dxgk_bindings_dump.rs:47973-48481`:

| Field | Bit | Width | Bindings line |
|---|---|---|---|
| `ReadOnlyMemorySupported` | 0 | 1 | `:47975` |
| `NoExecuteMemorySupported` | 1 | 1 | `:48011` |
| `ZeroInPteSupported` | 2 | 1 | `:48047` |
| `ExplicitPageTableInvalidation` | 3 | 1 | `:48083` |
| `CacheCoherentMemorySupported` | 4 | 1 | `:48119` |
| `PageTableUpdateRequireAddressSpaceIdle` | 5 | 1 | `:48155` |
| `LargePageSupported` | 6 | 1 | `:48194` |
| `DualPteSupported` | 7 | 1 | `:48230` |
| `AllowNonAlignedLargePageAddress` | 8 | 1 | `:48266` |
| `SysMem64KBPageSupported` | 9 | 1 | `:48302` |
| `InvalidTlbEntriesNotCached` | 10 | 1 | `:48338` |
| `SysMemLargePageSupported` | 11 | 1 | `:48374` |
| `CachedPageTables` | 12 | 1 | `:48410` |
| `Reserved` | (remaining) | — | `:48446` |

**`DXGK_PAGETABLEUPDATEMODE`** — the mode VidMm uses to write page-table content (CPU-virtual write vs GPU paging operation). This is the single most important lever for "can the page table be decorative" (`dxgk_bindings_dump.rs:47467-47473`):

```rust
pub mod _DXGK_PAGETABLEUPDATEMODE {
    pub type Type = ::core::ffi::c_int;
    pub const DXGK_PAGETABLEUPDATE_CPU_VIRTUAL: Type = 0;
    pub const DXGK_PAGETABLEUPDATE_GPU_VIRTUAL: Type = 1;
    pub const DXGK_PAGETABLEUPDATE_GPU_PHYSICAL: Type = 2;
}
pub use self::_DXGK_PAGETABLEUPDATEMODE::Type as DXGK_PAGETABLEUPDATEMODE;
```

**`DXGK_PAGETABLEUPDATEADDRESS`** — how VidMm names *where* a page table lives when it asks the KMD to write into it; a union over the three update modes (`dxgk_bindings_dump.rs:47476-47485`):

```rust
pub struct _DXGK_PAGETABLEUPDATEADDRESS {
    pub __bindgen_anon_1: _DXGK_PAGETABLEUPDATEADDRESS__bindgen_ty_1,
}
pub union _DXGK_PAGETABLEUPDATEADDRESS__bindgen_ty_1 {
    pub CpuVirtual: PVOID,                       // valid when UpdateMode == CPU_VIRTUAL
    pub GpuPhysical: D3DGPU_PHYSICAL_ADDRESS,    // valid when UpdateMode == GPU_PHYSICAL
    pub GpuVirtual: D3DGPU_VIRTUAL_ADDRESS,      // valid when UpdateMode == GPU_VIRTUAL
}
```

**`DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE`** — the actual paging-operation payload the KMD receives (inside `DxgkDdiBuildPagingBuffer`) carrying the `DXGK_PTE` array VidMm wants written. This is *the* call where decorative-vs-real is decided. (`dxgk_bindings_dump.rs:67573-67588`; struct is 104 bytes, align 8):

```rust
pub struct _DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE {
    pub PageTableLevel: UINT,
    pub hAllocation: HANDLE,
    pub PageTableAddress: DXGK_PAGETABLEUPDATEADDRESS,
    pub pPageTableEntries: *mut DXGK_PTE,            // the abstract PTE array VidMm computed
    pub StartIndex: UINT,
    pub NumPageTableEntries: UINT,
    pub Reserved0: UINT,
    pub Flags: DXGK_UPDATEPAGETABLEFLAGS,
    pub DriverProtection: UINT64,
    pub AllocationOffsetInBytes: UINT64,
    pub hProcess: HANDLE,
    pub UpdateMode: DXGK_PAGETABLEUPDATEMODE,
    pub pPageTableEntries64KB: *mut DXGK_PTE,        // parallel 64KB array when DualPte
    pub FirstPteVirtualAddress: D3DGPU_VIRTUAL_ADDRESS,
}
```

**`DXGKARG_SETROOTPAGETABLE`** — passed to `DxgkDdiSetRootPageTable` to bind a context's root page table (`dxgk_bindings_dump.rs:70256-70260`, alias `:70288`):

```rust
pub struct _DXGKARG_SETROOTPAGETABLE {
    pub hContext: HANDLE,
    pub Address: D3DGPU_PHYSICAL_ADDRESS,   // physical location of the root page table
    pub NumEntries: UINT,
}
```

**`DXGKARG_GETROOTPAGETABLESIZE`** — passed to `DxgkDdiGetRootPageTableSize`; the KMD returns `NumberOfPte` for a given address-space size (`dxgk_bindings_dump.rs:70292-70295`, alias `:70312`):

```rust
pub struct _DXGKARG_GETROOTPAGETABLESIZE {
    pub NumberOfPte: UINT,
    pub PhysicalAdapterIndex: UINT,
}
```

The DDI function pointer `DxgkDdiGetRootPageTableSize` exists in the driver-init table at `dxgk_bindings_dump.rs:95290` (`pub DxgkDdiGetRootPageTableSize: PDXGKDDI_GETROOTPAGETABLESIZE`), confirming these are live, registered DDIs Helios must supply.

#### A3.7 What the host GPU NEVER observes vs what VidMm maintains regardless — the decorative-GpuMmu verdict

Grounding this strictly in the quotes above:

**Host GPU (venus) never observes any of the GpuMmu addressing reality.** The doc says GpuMmu "implies that the GPU uses GPU page tables to access data" (`gpu-virtual-memory-in-wddm-2-0.md:35`) and that the KMD "uses this information to build hardware-specific page table entries" (`gpu-virtual-address.md:40`). For Helios there *is no guest GPU MMU and no hardware PTE format*: venus addresses host resources by opaque resource id, and the host GPU owns the real MMU. Therefore everything in the translation chain — the `DXGK_PTE` array content (`Valid`/`Segment`/`PageAddress`/`ReadOnly`/`NoExecute`/`CacheCoherent`/`LargePage`), the root-page-table physical address in `DXGKARG_SETROOTPAGETABLE::Address`, the GPUVA bit decomposition into level indices, and the per-process root binding — is **content the host never reads**. The KMD's PTE-translation step (the loop over `pPageTableEntries` in `DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE`) can be a no-op: there is no hardware page table to program. `DxgkDdiSetRootPageTable` can record-and-ignore. `DxgkDdiGetRootPageTableSize` only has to return a self-consistent `NumberOfPte`.

**VidMm maintains the following internally regardless of the driver — these cannot be faked away, only satisfied:**

1. **VA-space and page-table bookkeeping per process.** "*VidMm* is also responsible for allocating, growing, updating, ensuring residency, and freeing page tables" (`gpummu-model.md:16`). VidMm tracks the full GPUVA address space, the page-table tree, and residency in its own data structures independent of what the KMD does with the PTE bytes. We do not get to skip the DDIs that feed this bookkeeping (QueryAdapterInfo level descs, GetRootPageTableSize, SetRootPageTable, BuildPagingBuffer/UpdatePageTable, MapGpuVirtualAddress) — they are *called by VidMm* and must return success and structurally-consistent values.

2. **Physical page-table allocations out of a declared segment.** "Page tables are created as implicit allocations… *VidMm* allocates an allocation of size **DXGK_PAGE_TABLE_LEVEL_DESC::PageTableSizeInBytes** from the segment, specified in **DXGK_PAGE_TABLE_LEVEL_DESC::PageTableSegmentId**" (`gpu-virtual-address.md:44-46`). VidMm physically carves page-table-sized blocks from the segment Helios names in `PageTableSegmentId`. That segment must exist and have enough room. This is real, not decorative — VidMm will place allocations there and account residency against it. (The over-size-segment design exists precisely so these never get evicted: see `gpu-virtual-address.md:68` "VidMm can relocate or evict page tables.")

3. **The GPUVA assignment itself.** "*VidMm* doesn't automatically assign GPU virtual addresses to allocations. This responsibility falls on the user-mode driver (UMD)." (`gpummu-model.md:18`); UMD `MapGpuVirtualAddress` "queues a request to the video memory manager… queued on the device paging queue" (`per-process-gpu-virtual-address-spaces.md:16`). VidMm still *issues and tracks* a unique GPUVA per allocation and synchronizes it against a device paging fence. Helios can let VidMm pick the address automatically (so the GPUVA value is decorative to us), but the *allocation→GPUVA mapping lifecycle and its paging fence* are VidMm-internal and must be honored.

4. **The caps contract is binding.** `DXGK_GPUMMUCAPS` (`VirtualAddressBitCount`, `PageTableLevelCount`, `PageTableUpdateMode`) and the per-level `DXGK_PAGE_TABLE_LEVEL_DESC` are read by VidMm and *define* the geometry VidMm will then exercise through the page-table DDIs. We cannot under-declare these and then violate them: e.g. if we report `PageTableLevelCount` and a `PageTableUpdateMode`, VidMm will drive UpdatePageTable/SetRootPageTable accordingly with addresses in the form `DXGK_PAGETABLEUPDATEADDRESS` selects. The values are *driver-reported* (we choose them) but once reported they are *internally enforced* — VidMm's later calls assume our declared geometry.

5. **Fault behavior is real.** "Access to an invalid range of GPU virtual addresses results in an access violation and termination of the context… *VidMm* initiates an engine reset which gets promoted to an adapter wide… (TDR)" (`gpummu-model.md:30`). Since the host never faults on guest VAs, this VidMm-side fault machinery is only triggered by VidMm's own consistency checks — another reason the reported geometry must be self-consistent rather than contradictory.

**Decorative (driver-reported, can be hollow):** the *content* of every `DXGK_PTE` (no hardware reads it), the hardware PTE-translation step inside `BuildPagingBuffer`/UpdatePageTable, the actual root-page-table memory layout, the meaning of `DXGKARG_SETROOTPAGETABLE::Address`, and the GPUVA bit decomposition. **Cannot be faked (VidMm-internal):** the *existence and success* of the page-table DDIs themselves, the segment that backs page-table allocations (`PageTableSegmentId`), the GPUVA-assignment/paging-fence lifecycle, the residency accounting (handled by over-sizing the segment), and the internal self-consistency of the declared `DXGK_GPUMMUCAPS`/`DXGK_PAGE_TABLE_LEVEL_DESC` geometry.

**Net verdict for Step 2:** A decorative GpuMmu is plausible because the docs explicitly state the GPU's page-table *hardware format is unknown to VidMm and abstracted through DDIs* (`gpummu-model.md:16`) and the KMD merely *translates* `DXGK_PTE` into hardware PTEs (`gpu-virtual-address.md:40`) — so a KMD that translates into nothing is within the letter of the contract. The risk is not the PTE bytes; it is the *structural bookkeeping* VidMm maintains regardless: declared caps must be self-consistent, the page-table segment must be real and never evict, and every page-table DDI (QueryAdapterInfo level descs, GetRootPageTableSize, SetRootPageTable, BuildPagingBuffer UpdatePageTable, the MapGpuVirtualAddress paging-fence loop) must be implemented to return success and consistent values, even when their hardware effect is nil. The one open unknown the memory index already flags — *does VidMm accept a GpuMmu whose page tables are never meaningfully programmed* — reduces precisely to whether VidMm ever reads back PTE content it wrote (the docs give no indication it does; it tracks mappings in its own structures), and that is what the Step-2 kernel-debugger session must confirm.

### A4. DxgkDdiBuildPagingBuffer — the full operation union and the GpuMmu page-table path

`DxgkDdiBuildPagingBuffer` is the single DDI through which VidMm asks the miniport to emit GPU DMA into a *paging buffer* (`pDmaBuffer`) to move/map/fill allocation memory and, under the GpuMmu model, to **update GPU page tables**. The conceptual contract is stated verbatim in `paging-video-memory-resources.md:23`:

> "The special purpose DMA buffers that contain the commands for transferring data between video and system memory are known as paging buffers. The video memory manager calls the display miniport driver's **DxgkDdiBuildPagingBuffer** function to create paging buffers to which the driver writes hardware-specific data transfer commands." — `paging-video-memory-resources.md:23`

This is the **most consequential DDI for the fake-GpuMmu model**: it is where the VidMm-assigned segment offset for an allocation first becomes known to the driver (via the operation arguments), and therefore where Helios must learn "resource id R now lives at segment S, page-offset O" so it can drive the corresponding venus host-visible blob mapping. The rest of this section gives (1) the verbatim `DXGKARG_BUILDPAGINGBUFFER` bindgen types, (2) the viogpu3d dispatch as template, (3) the current kmd_render stub, (4) the doc passages on the GpuMmu page-table path, and (5) which operations the fake model must actually implement vs no-op.

---

#### A4.1 The operation discriminant — `DXGK_BUILDPAGINGBUFFER_OPERATION` (verbatim)

The union is selected by `Operation: DXGK_BUILDPAGINGBUFFER_OPERATION`. bindgen emitted the enum as a module of `pub const`s with `pub type Type = ::core::ffi::c_int` (`dxgk_bindings_dump.rs:66765`), re-exported as `DXGK_BUILDPAGINGBUFFER_OPERATION` at `:66791`. All 23 variants verbatim (`dxgk_bindings_dump.rs:66767-66789`):

```rust
pub mod _DXGK_BUILDPAGINGBUFFER_OPERATION {     // :66765
    pub const DXGK_OPERATION_TRANSFER: Type = 0;                  // :66767
    pub const DXGK_OPERATION_FILL: Type = 1;                      // :66768
    pub const DXGK_OPERATION_DISCARD_CONTENT: Type = 2;           // :66769
    pub const DXGK_OPERATION_READ_PHYSICAL: Type = 3;             // :66770
    pub const DXGK_OPERATION_WRITE_PHYSICAL: Type = 4;            // :66771
    pub const DXGK_OPERATION_MAP_APERTURE_SEGMENT: Type = 5;      // :66772
    pub const DXGK_OPERATION_UNMAP_APERTURE_SEGMENT: Type = 6;    // :66773
    pub const DXGK_OPERATION_SPECIAL_LOCK_TRANSFER: Type = 7;     // :66774
    pub const DXGK_OPERATION_VIRTUAL_TRANSFER: Type = 8;          // :66775
    pub const DXGK_OPERATION_VIRTUAL_FILL: Type = 9;             // :66776
    pub const DXGK_OPERATION_INIT_CONTEXT_RESOURCE: Type = 10;    // :66777
    pub const DXGK_OPERATION_UPDATE_PAGE_TABLE: Type = 11;        // :66778
    pub const DXGK_OPERATION_FLUSH_TLB: Type = 12;                // :66779
    pub const DXGK_OPERATION_UPDATE_CONTEXT_ALLOCATION: Type = 13;// :66780
    pub const DXGK_OPERATION_COPY_PAGE_TABLE_ENTRIES: Type = 14;  // :66781
    pub const DXGK_OPERATION_NOTIFY_RESIDENCY: Type = 15;         // :66782
    pub const DXGK_OPERATION_SIGNAL_MONITORED_FENCE: Type = 16;   // :66783
    pub const DXGK_OPERATION_MAP_APERTURE_SEGMENT2: Type = 17;    // :66784
    pub const DXGK_OPERATION_NOTIFY_FENCE_RESIDENCY: Type = 18;   // :66785
    pub const DXGK_OPERATION_MAP_MMU: Type = 19;                  // :66786
    pub const DXGK_OPERATION_UNMAP_MMU: Type = 20;                // :66787
    pub const DXGK_OPERATION_NOTIFY_RESIDENCY2: Type = 21;        // :66788
    pub const DXGK_OPERATION_NOTIFY_ALLOC: Type = 22;             // :66789
}
```

**Key mapping of variant → union member** (the union field names are at `:69034-69088`): note that the union member ORDER is not 1:1 with the enum value; you must select by name based on `Operation`:

| `Operation` value | union field to read | member struct |
|---|---|---|
| `TRANSFER` (0) | `.Transfer` | `__bindgen_ty_1__bindgen_ty_1` |
| `FILL` (1) | `.Fill` | `__bindgen_ty_1__bindgen_ty_2` |
| `DISCARD_CONTENT` (2) | `.DiscardContent` | `__bindgen_ty_1__bindgen_ty_3` |
| `READ_PHYSICAL` (3) | `.ReadPhysical` | `__bindgen_ty_1__bindgen_ty_4` |
| `WRITE_PHYSICAL` (4) | `.WritePhysical` | `__bindgen_ty_1__bindgen_ty_5` |
| `MAP_APERTURE_SEGMENT` (5) | `.MapApertureSegment` | `__bindgen_ty_1__bindgen_ty_6` |
| `UNMAP_APERTURE_SEGMENT` (6) | `.UnmapApertureSegment` | `__bindgen_ty_1__bindgen_ty_7` |
| `SPECIAL_LOCK_TRANSFER` (7) | `.SpecialLockTransfer` | `__bindgen_ty_1__bindgen_ty_8` |
| `INIT_CONTEXT_RESOURCE` (10) | `.InitContextResource` | `__bindgen_ty_1__bindgen_ty_9` |
| `VIRTUAL_TRANSFER` (8) | `.TransferVirtual` | `DXGK_BUILDPAGINGBUFFER_TRANSFERVIRTUAL` |
| `VIRTUAL_FILL` (9) | `.FillVirtual` | `DXGK_BUILDPAGINGBUFFER_FILLVIRTUAL` |
| `UPDATE_PAGE_TABLE` (11) | `.UpdatePageTable` | `DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE` |
| `FLUSH_TLB` (12) | `.FlushTlb` | `DXGK_BUILDPAGINGBUFFER_FLUSHTLB` |
| `UPDATE_CONTEXT_ALLOCATION` (13) | `.UpdateContextAllocation` | `DXGK_BUILDPAGINGBUFFER_UPDATECONTEXTALLOCATION` |
| `COPY_PAGE_TABLE_ENTRIES` (14) | `.CopyPageTableEntries` | `DXGK_BUILDPAGINGBUFFER_COPYPAGETABLEENTRIES` |
| `NOTIFY_RESIDENCY` (15) | `.NotifyResidency` | `DXGK_BUILDPAGINGBUFFER_NOTIFYRESIDENCY` |
| `SIGNAL_MONITORED_FENCE` (16) | `.SignalMonitoredFence` | `DXGK_BUILDPAGINGBUFFER_SIGNALMONITOREDFENCE` |
| `MAP_APERTURE_SEGMENT2` (17) | `.MapApertureSegment2` | `__bindgen_ty_1__bindgen_ty_10` |
| `NOTIFY_FENCE_RESIDENCY` (18) | `.NotifyFenceResidency` | `DXGK_BUILDPAGINGBUFFER_NOTIFY_FENCE_RESIDENCY` |
| `MAP_MMU` (19) | `.MmapMmu` | `DXGK_BUILDPAGINGBUFFER_MAPMMU` |
| `UNMAP_MMU` (20) | `.UnmapMmu` | `DXGK_BUILDPAGINGBUFFER_UNMAPMMU` |
| `NOTIFY_RESIDENCY2` (21) | `.NotifyResidency2` | `DXGK_BUILDPAGINGBUFFER_NOTIFYRESIDENCY2` |
| `NOTIFY_ALLOC` (22) | `.NotifyAllocation` | `DXGK_BUILDPAGINGBUFFER_NOTIFYALLOC` |

---

#### A4.2 The top-level argument struct `_DXGKARG_BUILDPAGINGBUFFER` (verbatim)

`dxgk_bindings_dump.rs:69020-69031`:

```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER {
    pub pDmaBuffer: *mut ::core::ffi::c_void,            // :69021  paging DMA buffer to write into
    pub DmaSize: UINT,                                  // :69022
    pub pDmaBufferPrivateData: *mut ::core::ffi::c_void, // :69023
    pub DmaBufferPrivateDataSize: UINT,                 // :69024
    pub Operation: DXGK_BUILDPAGINGBUFFER_OPERATION,    // :69025  ← the discriminant
    pub MultipassOffset: UINT,                          // :69026  multi-pass continuation
    pub __bindgen_anon_1: _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1, // :69027 the operation union
    pub hSystemContext: HANDLE,                         // :69028
    pub DmaBufferGpuVirtualAddress: D3DGPU_VIRTUAL_ADDRESS, // :69029
    pub DmaBufferWriteOffset: UINT,                     // :69030
}
```

The driver advances `pDmaBuffer` (returning the new write pointer in `pDmaBuffer`) by however many bytes of hardware DMA it emitted; if a single call could not fit the whole operation it sets `MultipassOffset` and returns `STATUS_GRAPHICS_ALLOCATION_BUSY`/`STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER` so VidMm re-invokes with the continuation offset.

**The operation union `__bindgen_ty_1`** — bindgen represents the C union with the `__BindgenUnionField<T>` pattern: every member is `__BindgenUnionField<MemberStruct>` and the actual backing storage is `bindgen_union_field: [u64; 32usize]` (256 bytes). To read a member in Rust you call its `.as_ref()` / `.as_mut()` accessor (bindgen's `__BindgenUnionField` exposes those). Verbatim (`dxgk_bindings_dump.rs:69033-69089`):

```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1 {
    pub Transfer:        __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1>, // :69034
    pub Fill:            __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_2>, // :69037
    pub DiscardContent:  __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_3>, // :69040
    pub ReadPhysical:    __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_4>, // :69043
    pub WritePhysical:   __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_5>, // :69046
    pub MapApertureSegment:   __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_6>, // :69049
    pub UnmapApertureSegment: __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_7>, // :69052
    pub SpecialLockTransfer:  __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_8>, // :69055
    pub InitContextResource:  __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_9>, // :69058
    pub TransferVirtual:      __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_TRANSFERVIRTUAL>,   // :69061
    pub FillVirtual:          __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_FILLVIRTUAL>,       // :69062
    pub UpdatePageTable:      __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE>,    // :69063  ← GpuMmu
    pub FlushTlb:             __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_FLUSHTLB>,           // :69064  ← GpuMmu
    pub CopyPageTableEntries: __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_COPYPAGETABLEENTRIES>, // :69065
    pub UpdateContextAllocation: __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_UPDATECONTEXTALLOCATION>, // :69068
    pub NotifyResidency:      __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_NOTIFYRESIDENCY>,    // :69071
    pub SignalMonitoredFence: __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_SIGNALMONITOREDFENCE>, // :69072
    pub MapApertureSegment2:  __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_10>, // :69075
    pub NotifyFenceResidency: __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_NOTIFY_FENCE_RESIDENCY>, // :69078
    pub MmapMmu:   __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_MAPMMU>,    // :69081  ← GpuMmu MapMmu
    pub UnmapMmu:  __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_UNMAPMMU>,  // :69082  ← GpuMmu UnmapMmu
    pub NotifyResidency2:  __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_NOTIFYRESIDENCY2>, // :69083
    pub NotifyAllocation:  __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_NOTIFYALLOC>,       // :69084
    pub Reserved:  __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_11>, // :69085
    pub bindgen_union_field: [u64; 32usize],            // :69088  backing storage = 256 bytes
}
```

> Step-2 note: because this is the `__BindgenUnionField` flavor (not a `#[repr(C)] union`), there is no `unsafe { args.__bindgen_anon_1.UpdatePageTable }` field access. Use the generated accessor: `args.__bindgen_anon_1.UpdatePageTable.as_ref()` returns `Option<&DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE>` (bindgen's `__BindgenUnionField::as_ref`). You must gate which accessor you call on the value of `args.Operation` yourself — the union does not validate.

---

#### A4.3 The data-movement union members (Transfer / Fill / Discard / Read/Write physical)

**Transfer** — `_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1` (`:69091-69099`). This is the classic "copy this allocation between two segments / system memory":

```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1 {
    pub hAllocation: HANDLE,        // :69092
    pub TransferOffset: UINT,       // :69093
    pub TransferSize: SIZE_T,       // :69094
    pub Source:      _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1__bindgen_ty_1, // :69095
    pub Destination: _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1__bindgen_ty_2, // :69096
    pub Flags: DXGK_TRANSFERFLAGS,  // :69097
    pub MdlOffset: UINT,            // :69098
}
```

The `Source`/`Destination` sub-structs each carry `SegmentId: UINT` plus an anonymous union of `SegmentAddress: LARGE_INTEGER` **or** `pMdl: *mut MDL` (`:69101-69110` for Source, `:69174-69183` for Destination). bindgen rendered that inner union, again, with `__BindgenUnionField`:

```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1__bindgen_ty_1 {  // :69101  Source
    pub SegmentId: UINT,                                       // :69102
    pub __bindgen_anon_1: ...__bindgen_ty_1__bindgen_ty_1,     // :69103
}
pub struct ...__bindgen_ty_1__bindgen_ty_1__bindgen_ty_1__bindgen_ty_1 {           // :69106
    pub SegmentAddress: __BindgenUnionField<LARGE_INTEGER>,    // :69107
    pub pMdl:           __BindgenUnionField<*mut MDL>,         // :69108
    pub bindgen_union_field: u64,                             // :69109
}
```
> Interpretation: `SegmentId == 0` means system memory and the address field is `pMdl` (the MDL describing the guest pages); a nonzero `SegmentId` means the address field is `SegmentAddress` (offset within that segment). This is exactly where the **VidMm-assigned segment offset** for a Transfer appears.

**Fill** — `_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_2` (`:69302-69307`):
```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_2 {
    pub hAllocation: HANDLE,   // :69303
    pub FillSize: SIZE_T,      // :69304
    pub FillPattern: UINT,     // :69305
    pub Destination: _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_2__bindgen_ty_1, // :69306
}
// Destination (:69309-69312): SegmentId: UINT; SegmentAddress: LARGE_INTEGER  (no MDL alternative here)
```

**DiscardContent** — `_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_3` (`:69387-69392`):
```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_3 {
    pub hAllocation: HANDLE,             // :69388
    pub Flags: DXGK_DISCARDCONTENTFLAGS, // :69389
    pub SegmentId: UINT,                 // :69390
    pub SegmentAddress: PHYSICAL_ADDRESS,// :69391
}
```

**ReadPhysical** / **WritePhysical** — identical layout, `__bindgen_ty_1__bindgen_ty_4` (`:69434-69437`) and `__bindgen_ty_1__bindgen_ty_5` (`:69469-69472`):
```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_4 {  // ReadPhysical
    pub SegmentId: UINT,                  // :69435
    pub PhysicalAddress: PHYSICAL_ADDRESS,// :69436
}
```

---

#### A4.4 The aperture union members — the viogpu3d path (MOST RELEVANT to Helios)

**MapApertureSegment** — `_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_6` (`:69505-69514`). This is the member viogpu3d actually consumes; it is the cleanest carrier of "allocation backing pages (MDL) → assigned offset within the aperture segment":

```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_6 {
    pub hDevice: HANDLE,                  // :69506
    pub hAllocation: HANDLE,              // :69507
    pub SegmentId: UINT,                  // :69508
    pub OffsetInPages: SIZE_T,            // :69509  ← the VidMm-assigned segment offset (pages)
    pub NumberOfPages: SIZE_T,            // :69510
    pub pMdl: PMDL,                       // :69511  ← guest physical pages backing the allocation
    pub Flags: DXGK_MAPAPERTUREFLAGS,     // :69512
    pub MdlOffset: ULONG,                 // :69513
}
```

**UnmapApertureSegment** — `_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_7` (`:69576-69583`):
```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_7 {
    pub hDevice: HANDLE,                  // :69577
    pub hAllocation: HANDLE,              // :69578
    pub SegmentId: UINT,                  // :69579
    pub OffsetInPages: SIZE_T,            // :69580
    pub NumberOfPages: SIZE_T,            // :69581
    pub DummyPage: PHYSICAL_ADDRESS,      // :69582  (page to point freed PTEs at)
}
```

**MapApertureSegment2** — `_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_10` (`:69975-69985`) is the newer variant that uses an *address descriptor list* `DXGK_ADL` instead of an MDL and additionally surfaces a `CpuVisibleAddress`:
```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_10 {
    pub hDevice: HANDLE,                  // :69976
    pub hAllocation: HANDLE,              // :69977
    pub SegmentId: UINT,                  // :69978
    pub OffsetInPages: SIZE_T,            // :69979
    pub NumberOfPages: SIZE_T,            // :69980
    pub Adl: DXGK_ADL,                    // :69981  (DXGK_ADL = _DXGK_ADL @ :56807)
    pub Flags: DXGK_MAPAPERTUREFLAGS,     // :69982
    pub AdlOffset: ULONG,                 // :69983
    pub CpuVisibleAddress: PVOID,         // :69984  ← CPU-visible VA for the mapping
}
```

##### viogpu3d dispatch — the template (verbatim)

viogpu3d registers the DDI in `driver.cpp:109` (`InitialData.DxgkDdiBuildPagingBuffer = VioGpu3DBuildPagingBuffer;`). Its handler **only implements the two aperture operations and returns `STATUS_NOT_SUPPORTED` for everything else** — i.e. it is an *aperture* driver, not a GpuMmu page-table driver, so it never sees `UPDATE_PAGE_TABLE`/`MAP_MMU` (`driver.cpp:524-569`):

```cpp
VioGpu3DBuildPagingBuffer(_In_ CONST HANDLE hAdapter, _In_ DXGKARG_BUILDPAGINGBUFFER *pBuildPagingBuffer)
{
    ...
    switch (pBuildPagingBuffer->Operation)                                  // driver.cpp:532
    {
        case DXGK_OPERATION_MAP_APERTURE_SEGMENT:                           // :534
            {
                if (pBuildPagingBuffer->MapApertureSegment.hAllocation == NULL) { ... return STATUS_SUCCESS; }
                VioGpuAllocation *allocation =
                    reinterpret_cast<VioGpuAllocation *>(pBuildPagingBuffer->MapApertureSegment.hAllocation); // :543
                NTSTATUS Status = allocation->MapApertureSegment(pBuildPagingBuffer);                          // :544
                return Status;
            }
        case DXGK_OPERATION_UNMAP_APERTURE_SEGMENT:                         // :548
            {
                if (pBuildPagingBuffer->UnmapApertureSegment.hAllocation == NULL) { ... return STATUS_SUCCESS; }
                VioGpuAllocation *allocation =
                    reinterpret_cast<VioGpuAllocation *>(pBuildPagingBuffer->UnmapApertureSegment.hAllocation); // :557
                NTSTATUS Status = allocation->UnmapApertureSegment(pBuildPagingBuffer);                          // :558
                return Status;
            }
        default:                                                            // :562
            {
                DbgPrint(TRACE_LEVEL_ERROR, ("<--- %s (unknown operation %d)\n", __FUNCTION__, pBuildPagingBuffer->Operation));
                return STATUS_NOT_SUPPORTED;                                // :566
            }
    };
}
```

The per-allocation `MapApertureSegment` reads `NumberOfPages`, `MdlOffset`, `pMdl`, and crucially `OffsetInPages` to fix the allocation's physical address inside the aperture segment, then wires the guest pages to the host via the virtio-gpu `ATTACH_BACKING` control-queue command (`viogpu_allocation.cpp:316-330`):

```cpp
NTSTATUS VioGpuAllocation::MapApertureSegment(DXGKARG_BUILDPAGINGBUFFER *pBuildPagingBuffer)
{
    PAGED_CODE();
    size_t pageCount    = pBuildPagingBuffer->MapApertureSegment.NumberOfPages;   // :321
    size_t mdlPageOffset = pBuildPagingBuffer->MapApertureSegment.MdlOffset;      // :322
    MDL *pMdl           = pBuildPagingBuffer->MapApertureSegment.pMdl;            // :324
    AttachBacking(pMdl, pageCount, mdlPageOffset);                               // :326
    SetDxPhysicalAddress(pBuildPagingBuffer->MapApertureSegment.OffsetInPages * PAGE_SIZE); // :327  ← assigned offset
    return STATUS_SUCCESS;
}
```

`AttachBacking` translates each MDL PFN into a `GPU_MEM_ENTRY{addr,length,padding}` array and submits it via `ctrlQueue.AttachBacking(m_Id, ents, pageCount)` — i.e. the host is told *these guest physical pages back resource id m_Id* (`viogpu_allocation.cpp:39-57`):

```cpp
void VioGpuAllocation::AttachBacking(MDL *pMDL, size_t pageCount, size_t pageOffset)
{
    m_pMDL = pMDL; m_pageCount = pageCount; m_pageOffset = pageOffset;            // :43-45
    GPU_MEM_ENTRY *ents = new (NonPagedPoolNx) GPU_MEM_ENTRY[pageCount];          // :47
    for (UINT i = 0; i < pageCount; i++) {
        ents[i].addr   = MmGetMdlPfnArray(pMDL)[pageOffset + i] * PAGE_SIZE;      // :51
        ents[i].length = PAGE_SIZE; ents[i].padding = 0;                         // :52-53
    }
    m_adapter->ctrlQueue.AttachBacking(m_Id, ents, (UINT)pageCount);             // :56
}
```

`UnmapApertureSegment` simply `DetachBacking()` (`viogpu_allocation.cpp:332-340`). The matching segment is declared **Aperture, CpuVisible=FALSE** in `DXGKQAITYPE_QUERYSEGMENT3` (`viogpu_adapter.cpp:540-564`: `pSegmentDesc[0].Flags.Aperture = TRUE; pSegmentDesc[0].Flags.CpuVisible = FALSE; pSegmentDesc[0].Size = 256*1024*4096; pSegmentInfo->PagingBufferSize = 10*PAGE_SIZE;`). This is why viogpu3d sees `MAP_APERTURE_SEGMENT`, not `UPDATE_PAGE_TABLE`.

---

#### A4.5 The GpuMmu page-table union members (UpdatePageTable / FlushTlb / Copy / MapMmu)

These are the members a **GpuMmu** driver (which Helios's locked model declares) will actually receive. They are the heart of the "fake but coherent" question because they carry the GPU-page-table writes VidMm expects the GPU MMU to honor — but Helios's host owns the real MMU.

**UpdatePageTable** — `DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE = _DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE` (`:67573-67588`, typedef `:67666`). This is the single most important GpuMmu operation: VidMm hands the driver an array of `DXGK_PTE`s to write into a page-table allocation:

```rust
pub struct _DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE {
    pub PageTableLevel: UINT,                       // :67574
    pub hAllocation: HANDLE,                         // :67575  (the page-table allocation)
    pub PageTableAddress: DXGK_PAGETABLEUPDATEADDRESS, // :67576  where the page table lives
    pub pPageTableEntries: *mut DXGK_PTE,            // :67577  ← the 4KB-page PTEs to write
    pub StartIndex: UINT,                            // :67578
    pub NumPageTableEntries: UINT,                   // :67579
    pub Reserved0: UINT,                             // :67580
    pub Flags: DXGK_UPDATEPAGETABLEFLAGS,            // :67581
    pub DriverProtection: UINT64,                    // :67582
    pub AllocationOffsetInBytes: UINT64,             // :67583
    pub hProcess: HANDLE,                            // :67584
    pub UpdateMode: DXGK_PAGETABLEUPDATEMODE,        // :67585
    pub pPageTableEntries64KB: *mut DXGK_PTE,        // :67586  ← the 64KB-page PTEs (dual PTE)
    pub FirstPteVirtualAddress: D3DGPU_VIRTUAL_ADDRESS, // :67587
}
```

Supporting types:

- `DXGK_PAGETABLEUPDATEADDRESS` (`_DXGK_PAGETABLEUPDATEADDRESS` @ `:47476`) wraps an anonymous union (`:47481-47485`) of where the page table is addressed:
  ```rust
  pub union _DXGK_PAGETABLEUPDATEADDRESS__bindgen_ty_1 {
      pub CpuVirtual: PVOID,                       // :47482
      pub GpuPhysical: D3DGPU_PHYSICAL_ADDRESS,    // :47483
      pub GpuVirtual: D3DGPU_VIRTUAL_ADDRESS,      // :47484
  }
  ```
  This is a real `#[repr(C)] union` (note: NOT `__BindgenUnionField`), so access is `unsafe { addr.__bindgen_anon_1.CpuVirtual }`.
- `DXGK_PAGETABLEUPDATEMODE` (`:47467-47472`): `DXGK_PAGETABLEUPDATE_CPU_VIRTUAL = 0`, `DXGK_PAGETABLEUPDATE_GPU_VIRTUAL = 1`, `DXGK_PAGETABLEUPDATE_GPU_PHYSICAL = 2`. When `UpdateMode == CPU_VIRTUAL`, VidMm has already CPU-mapped the page table and the driver can `memcpy` the PTEs directly (the easiest path for a fake MMU).
- `DXGK_UPDATEPAGETABLEFLAGS` (`_DXGK_UPDATEPAGETABLEFLAGS` @ `:47576`) is a `__BindgenBitfieldUnit<[u8; 4usize]>` with accessor methods named (verbatim, bindgen emits `X()`/`set_X()` pairs): **`Repeat`, `InitialUpdate`, `NotifyEviction`, `Use64KBPages`**. `support-for-64kb-pages.md:19` confirms `Use64KBPages` selects the 64KB-vs-4KB page-table type.

**`DXGK_PTE`** — `_DXGK_PTE = _DXGK_PTE` (`:12266-12268`, typedef `DXGK_PTE` @ `:12878`) is what the driver actually writes per entry. It has two anonymous members: a flags bitfield union (`__bindgen_anon_1`) and an address union (`__bindgen_anon_2`):

```rust
pub struct _DXGK_PTE {
    pub __bindgen_anon_1: _DXGK_PTE__bindgen_ty_1,   // :12267  flags (bitfield) or raw Flags: ULONGLONG
    pub __bindgen_anon_2: _DXGK_PTE__bindgen_ty_2,   // :12268  PageAddress / PageTableAddress
}
pub union _DXGK_PTE__bindgen_ty_1 {                  // :12272
    pub __bindgen_anon_1: _DXGK_PTE__bindgen_ty_1__bindgen_ty_1, // bitfield struct (8 bytes)
    pub Flags: ULONGLONG,                            // :12274  whole-word view
}
pub union _DXGK_PTE__bindgen_ty_2 {                  // :12836
    pub PageAddress: ULONGLONG,                      // :12837  (page frame number, for leaf PTE)
    pub PageTableAddress: ULONGLONG,                 // :12838  (next-level table PFN, for directory PTE)
}
```

The flags bitfield struct `_DXGK_PTE__bindgen_ty_1__bindgen_ty_1` is `_bitfield_1: __BindgenBitfieldUnit<[u8; 8usize]>` (`:12278-12281`). bindgen emitted `X()`/`set_X()`/`X_raw()`/`set_X_raw()` accessors, each returning/taking `ULONGLONG`. The field set and their bit positions (from the `_bitfield_1.get(pos,width)` calls) are, in order:

| field accessor | bit offset | width |
|---|---|---|
| `Valid` | 0 | 1 (`:12293`) |
| `Zero` | 1 | 1 |
| `CacheCoherent` | 2 | 1 (`:12365`) |
| `ReadOnly` | 3 | 1 |
| `NoExecute` | 4 | 1 |
| `Segment` | 5 | 5 |
| `LargePage` | 10 | 1 |
| `PhysicalAdapterIndex` | 11 | 6 |
| `PageTablePageSize` | 17 | 2 |
| `SystemReserved0` | 19 | 1 |
| `Reserved` | 20 | 44 |

> Step-2 usage example for a "valid leaf PTE pointing at host-visible resource page": `pte.__bindgen_anon_1.__bindgen_anon_1.set_Valid(1); pte.__bindgen_anon_1.__bindgen_anon_1.set_Segment(seg_id as u64); pte.__bindgen_anon_2.PageAddress = pfn;`. `PageTablePageSize` is the field `support-for-64kb-pages.md:32` says to set "only for PTEs of the level 1 page table" to distinguish a 64KB-paged from a 4KB-paged sub-table.

**FlushTlb** — `DXGK_BUILDPAGINGBUFFER_FLUSHTLB` (`:67531-67536`, typedef `:67570`):
```rust
pub struct _DXGK_BUILDPAGINGBUFFER_FLUSHTLB {
    pub RootPageTableAddress: D3DGPU_PHYSICAL_ADDRESS, // :67532
    pub hProcess: HANDLE,                              // :67533
    pub StartVirtualAddress: D3DGPU_VIRTUAL_ADDRESS,   // :67534
    pub EndVirtualAddress: D3DGPU_VIRTUAL_ADDRESS,     // :67535
}
```

**CopyPageTableEntries** — `DXGK_BUILDPAGINGBUFFER_COPYPAGETABLEENTRIES` (`:68176-68179`, typedef `:68206`):
```rust
pub struct _DXGK_BUILDPAGINGBUFFER_COPYPAGETABLEENTRIES {
    pub NumRanges: UINT,                              // :68177
    pub pRanges: *mut DXGK_BUILDPAGINGBUFFER_COPY_RANGE, // :68178
}
// DXGK_BUILDPAGINGBUFFER_COPY_RANGE (:67492-67498):
//   NumPageTableEntries: UINT; SrcPageTableAddress: D3DGPU_VIRTUAL_ADDRESS;
//   DstPageTableAddress: D3DGPU_VIRTUAL_ADDRESS; SrcStartPteIndex: UINT; DstStartPteIndex: UINT;
```

**MapMmu / UnmapMmu** (the newer flat MMU-map ops) — `DXGK_BUILDPAGINGBUFFER_MAPMMU` (`:68452-68459`) and `DXGK_BUILDPAGINGBUFFER_UNMAPMMU` (`:68500-68507`):
```rust
pub struct _DXGK_BUILDPAGINGBUFFER_MAPMMU {
    pub hAllocation: HANDLE,            // :68453
    pub VirtualAddress: UINT64,         // :68454
    pub MmuId: UINT16,                  // :68455
    pub SegmentId: UINT16,              // :68456
    pub AllocationOffsetInPages: UINT32,// :68457
    pub Adl: DXGK_ADL,                  // :68458
}
pub struct _DXGK_BUILDPAGINGBUFFER_UNMAPMMU {
    pub hAllocation: HANDLE,            // :68501
    pub VirtualAddress: UINT64,         // :68502
    pub MmuId: UINT16,                  // :68503
    pub Reserved0: UINT16,              // :68504
    pub AllocationOffset: UINT32,       // :68505
    pub NumberOfPages: UINT32,          // :68506
}
```

**Virtual transfer/fill** (used when an engine references memory by GPU VA rather than segment) — `DXGK_BUILDPAGINGBUFFER_TRANSFERVIRTUAL` (`:67944-67953`) and `DXGK_BUILDPAGINGBUFFER_FILLVIRTUAL` (`:67669-67674`):
```rust
pub struct _DXGK_BUILDPAGINGBUFFER_TRANSFERVIRTUAL {
    pub hAllocation: HANDLE; pub AllocationOffsetInBytes: UINT64; pub TransferSizeInBytes: UINT64;
    pub SourceVirtualAddress: D3DGPU_VIRTUAL_ADDRESS; pub DestinationVirtualAddress: D3DGPU_VIRTUAL_ADDRESS;
    pub SourcePageTable: D3DGPU_VIRTUAL_ADDRESS; pub TransferDirection: DXGK_MEMORY_TRANSFER_DIRECTION;
    pub Flags: DXGK_TRANSFERVIRTUALFLAGS; pub DestinationPageTable: D3DGPU_VIRTUAL_ADDRESS;
}
// DXGK_MEMORY_TRANSFER_DIRECTION (:67717): LOCAL_TO_SYSTEM=0, SYSTEM_TO_LOCAL=1, LOCAL_TO_LOCAL=2
pub struct _DXGK_BUILDPAGINGBUFFER_FILLVIRTUAL {
    pub hAllocation: HANDLE; pub AllocationOffsetInBytes: UINT64; pub FillSizeInBytes: UINT64;
    pub FillPattern: UINT; pub DestinationVirtualAddress: D3DGPU_VIRTUAL_ADDRESS;
}
```

**SpecialLockTransfer** (`__bindgen_ty_1__bindgen_ty_8`, `:69635-69643`) mirrors Transfer with two extra fields `SwizzlingRangeId`/`SwizzlingRangeData` and the same `Source`/`Destination` MDL-or-SegmentAddress unions. **InitContextResource** (`__bindgen_ty_1__bindgen_ty_9`, `:69852-69861`): `hAllocation: HANDLE` + a `Destination{ SegmentId, SegmentAddress|pMdl union, VirtualAddress: PVOID, GpuVirtualAddress: D3DGPU_VIRTUAL_ADDRESS }`. **Reserved** (`__bindgen_ty_1__bindgen_ty_11`, `:70053-70055`): `pub Reserved: [UINT; 64usize]` (256 bytes — this is what sizes `bindgen_union_field: [u64; 32]`).

---

#### A4.6 kmd_render current state — `ddi/build_paging_buffer.rs` (verbatim, STUB)

The current Helios implementation is a **null paging engine: it emits no DMA and writes no PTEs**. It records only the operation discriminant via DISPATCH-safe atomics (because the DDI can run at `DISPATCH_LEVEL`, so `diag::record`'s `RtlWriteRegistryValue` would be an IRQL violation). Verbatim (`kmd_render/src/ddi/build_paging_buffer.rs:24-43`):

```rust
pub unsafe extern "C" fn dxgkddi_build_paging_buffer(
    h_adapter: *mut c_void,
    build_paging_buffer: *mut DXGKARG_BUILDPAGINGBUFFER,
) -> NTSTATUS {
    if h_adapter.is_null() || build_paging_buffer.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // Do not advance pDmaBuffer. Dxgkrnl supplied a buffer, but this bring-up
    // engine has no real page-table or aperture commands to write yet.
    let args = unsafe { &*build_paging_buffer };
    PAGING_LAST_OP.store(args.Operation as u32, Ordering::Relaxed);   // :40
    PAGING_CALL_COUNT.fetch_add(1, Ordering::Relaxed);                // :41
    STATUS_SUCCESS
}
```

The breadcrumb atomics (`kmd_render/src/ddi/build_paging_buffer.rs:16-17`):
```rust
pub static PAGING_LAST_OP: AtomicU32 = AtomicU32::new(0xFFFF_FFFF);
pub static PAGING_CALL_COUNT: AtomicU32 = AtomicU32::new(0);
```
The module doc explicitly states the intent for Step 2 (`build_paging_buffer.rs:36-38`): *"Stage 2b will read the op here (ntoseye breakpoint) to learn which op carries the VidMm segment offset, then issue `resource_map_blob(resource_id, offset)`."*

The companion GpuMmu root-page-table DDIs in the same file are also stubs: `dxgkddi_set_root_page_table` is an empty no-op (`:49-53`), and `dxgkddi_get_root_page_table_size` returns `0` (`:58-63`).

**Status: STUB / null engine.** It does not dispatch on `Operation` (it only stores it), implements none of the union members, advances neither `pDmaBuffer` nor `DmaBufferWriteOffset`, and never calls a venus `resource_map_blob`. Every other op falls through to `STATUS_SUCCESS` with no side effect.

---

#### A4.7 Doc passages on the GpuMmu / paging usage of BuildPagingBuffer

- `paging-video-memory-resources.md:21`: *"The GPU can have multiple DMA buffers in its pipeline. The video memory resources that are referenced by these active DMA buffers must be in video memory. Other idle video memory resources can be paged out to system memory."* (This is the residency premise the over-sized-segment "nothing evicts" approach is designed to defeat.)
- `paging-video-memory-resources.md:23` (the canonical statement, quoted in full at the top of A4): VidMm calls `DxgkDdiBuildPagingBuffer` "to create paging buffers to which the driver writes hardware-specific data transfer commands."
- `gpummu-model.md:16`: *"The video memory manager (VidMm) manages the GPU virtual address space of all processes. VidMm is also responsible for allocating, growing, updating, ensuring residency, and freeing page tables. The hardware format of the page tables used by the GPU MMU is unknown to VidMm and is abstracted through device driver interfaces (DDIs)."* — this abstraction is exactly the `UPDATE_PAGE_TABLE` + `DXGK_PTE` path; the "hardware format is unknown to VidMm" is the lever that makes a **decorative** page table viable.
- `gpu-virtual-memory-in-wddm-2-0.md:35`: *"In the GpuMmu model, VidMm manages the GPU memory management unit and underlying page tables… GpuMmu implies that the GPU uses GPU page tables to access data."* and `:31`: *"A driver must opt-in to support either or both of the models."*
- `support-for-64kb-pages.md:19`: *"The UpdatePageTable operation has a DXGK_UPDATEPAGETABLEFLAGS::Use64KBPages flag that indicates the type of the page table to be updated."* and `:32`: the `DXGK_PTE::PageTablePageSize` field "should be used only for PTEs of the level 1 page table … This field tells the kernel-mode driver the type of the corresponding page table (using 64KB or 4KB pages)."
- `gpummu-model.md:30`: *"Access to an invalid range of GPU virtual addresses results in an access violation and termination of the context and/or device that caused the access fault."* — this is the failure mode if the fake page table is so malformed that real hardware (which Helios does not have) is expected to fault; under venus the host owns translation, so the practical requirement is only that VidMm's bookkeeping accepts the writes, not that any GPU MMU consumes them.

(Note: the `windows-driver-docs-pr/display/` tree contains **no** per-DDI API reference page for `DxgkDdiBuildPagingBuffer` or the individual operations — the conceptual docs link out to `/windows-hardware/drivers/ddi/...` URLs that are not present in this repo. The authoritative field-level signatures are the bindgen types in A4.1–A4.5.)

---

#### A4.8 Fake-GpuMmu contract — which operations MUST be real vs which can be no-ops

The gate5a problem ("map a host-visible blob to the VidMm-assigned segment offset") lands here. Because Helios declares **GpuMmu** (not aperture, unlike viogpu3d), VidMm will not send `MAP_APERTURE_SEGMENT`; it will instead send `UPDATE_PAGE_TABLE` (and possibly `MAP_MMU`). The contract:

**MUST be handled (carry the VidMm-assigned offset / make a mapping land):**

- **`UPDATE_PAGE_TABLE` (11)** — *the* operation that tells Helios "GPU-VA range starting at `FirstPteVirtualAddress` now maps, via `NumPageTableEntries` PTEs starting at `StartIndex`, to these physical pages." Each `DXGK_PTE.__bindgen_anon_2.PageAddress` is the (guest) page frame and `.Segment` names the segment. This is where a render allocation's segment offset becomes concrete. For the fake model the *page table itself can be decorative* (the host never reads it), but the driver MUST scan the incoming `pPageTableEntries[StartIndex..StartIndex+NumPageTableEntries]`, recover `hAllocation`/`AllocationOffsetInBytes` (or derive the resource id from the PTE → allocation mapping built in CreateAllocation/A2), and, for any allocation that is a host-visible venus blob, issue the venus `resource_map_blob(resource_id, offset)` so a later `Lock2` / IDD readback resolves to the right host bytes at the right offset. If `UpdateMode == DXGK_PAGETABLEUPDATE_CPU_VIRTUAL`, VidMm has CPU-mapped the page table at `PageTableAddress.CpuVirtual`, so the driver may simply `memcpy` the PTEs there (satisfying VidMm's "did you write the table?" expectation) **and** perform the venus map as a side effect.
- **`MAP_MMU` (19) / `UNMAP_MMU` (20)** — the flat-MMU equivalent; if dxgkrnl on WDDM 3.2 issues these instead of `UPDATE_PAGE_TABLE` for our caps, the same logic applies (`VirtualAddress`, `SegmentId`, `AllocationOffsetInPages`, `Adl`/`hAllocation` → venus blob map). Step 2 must instrument `PAGING_LAST_OP` to learn which of {UPDATE_PAGE_TABLE, MAP_MMU} the OS actually sends for our `DXGK_GPUMMUCAPS`.
- **`TRANSFER` (0) — only if it appears.** If a host-visible blob is created in system memory and VidMm wants it "in segment," the `Source`/`Destination` MDL-or-SegmentAddress union here carries the offset. In the zero-copy model the goal is to avoid real transfers (the BAR *is* the storage), so the right response is usually to treat Transfer as a no-op that still advances `pDmaBuffer` — but the *offset* it names may still need to drive a venus map. Confirm via breadcrumb whether Transfer is ever issued once segments are sized correctly.

**Can be no-ops (return `STATUS_SUCCESS`, do not advance `pDmaBuffer`, no venus call):**

- **`FILL` (1) / `VIRTUAL_FILL` (9)** — pattern-clear; venus resources are initialized host-side, so a guest-side fill is unnecessary. (If a guest app observes uninitialized memory through a Lock2 readback, revisit.)
- **`DISCARD_CONTENT` (2)** — pure hint; safe to drop.
- **`READ_PHYSICAL` (3) / `WRITE_PHYSICAL` (4)** — used to read/write specific physical pages (e.g. fence/semaphore memory); for the venus model these are not GPU-engine commands and can be dropped unless a specific feature needs them.
- **`FLUSH_TLB` (12)** — there is no real guest TLB (the host owns translation), so flushing is a no-op. Return success.
- **`COPY_PAGE_TABLE_ENTRIES` (14)** — copies PTE ranges between page tables; since the page table is decorative, this can be a no-op (or a literal memcpy of the decorative entries to keep VidMm's view consistent, which is cheap and safer).
- **`UPDATE_CONTEXT_ALLOCATION` (13), `NOTIFY_RESIDENCY` (15), `NOTIFY_FENCE_RESIDENCY` (18), `NOTIFY_RESIDENCY2` (21), `NOTIFY_ALLOC` (22)** — residency/notification bookkeeping; with the over-sized never-evict segment these carry no work. No-op success.
- **`SIGNAL_MONITORED_FENCE` (16)** — writes a fence value into a monitored-fence allocation from the paging engine; relevant only to the fence-coherence design (Section on fences), not to memory mapping. If used, it must write the value where the fence reader expects it; otherwise no-op.
- **`SPECIAL_LOCK_TRANSFER` (7), `INIT_CONTEXT_RESOURCE` (10), `VIRTUAL_TRANSFER` (8)** — only seen if the relevant caps/features are advertised; with a minimal GpuMmu opt-in they should not arrive. Treat as no-op-success initially and add real handling only if breadcrumbs show them.
- **`MAP_APERTURE_SEGMENT` (5) / `MAP_APERTURE_SEGMENT2` (17) / `UNMAP_APERTURE_SEGMENT` (6)** — only if an Aperture segment is declared. The locked Helios model is GpuMmu/decorative-page-table, **not** the viogpu3d aperture model, so these are not expected; if the gate5a debugging shows the OS preferring the aperture path, viogpu3d's `MapApertureSegment` (A4.4) is the drop-in template (`OffsetInPages * PAGE_SIZE` → venus map; `pMdl` → backing pages), and this would be a simpler offset carrier than UPDATE_PAGE_TABLE.

**Cross-cutting requirement for all handled ops:** the handler must correctly manage `pDmaBuffer`/`DmaBufferWriteOffset`/`MultipassOffset` (advance the write pointer by the bytes "emitted," or, for a null-DMA fake engine, leave them untouched and return `STATUS_SUCCESS`) and must run safely at `DISPATCH_LEVEL` (no pageable code, no `RtlWriteRegistryValue`) — exactly the constraint the current stub already respects via its atomics. The first Step-2 task is to replace the bare `PAGING_LAST_OP.store(...)` with a real `match args.Operation { DXGK_OPERATION_UPDATE_PAGE_TABLE => ... }` that reads `args.__bindgen_anon_1.UpdatePageTable.as_ref()`, walks `pPageTableEntries`, and drives `resource_map_blob` for host-visible allocations.

Relevant file paths: bindgen types `/home/rupansh/helios-vgpu/dxgk_bindings_dump.rs` (lines cited above); viogpu3d template `/home/rupansh/helios-vgpu/virtio-research-only-3d/viogpu/viogpu3d/driver.cpp:524-569`, `.../viogpu_allocation.cpp:39-57,316-340`, `.../viogpu_adapter.cpp:520-568`; current Helios stub `/home/rupansh/helios-vgpu/kmd_render/src/ddi/build_paging_buffer.rs`; docs `/home/rupansh/helios-vgpu/windows-driver-docs-research-only/windows-driver-docs-pr/display/{paging-video-memory-resources,gpummu-model,gpu-virtual-memory-in-wddm-2-0,support-for-64kb-pages}.md`.

### A5. Allocation DDIs — CreateAllocation, DescribeAllocation, OpenAllocation, GetStandardAllocationDriverData

This section covers the allocation-lifecycle DDIs. For Helios the central design fact (from `IDD_HELIOS_RENDER_PLAN.md:47-54`) is that `DxgkDdiCreateAllocation` is *already real* (it makes venus HOST3D blobs), but **`DxgkDdiGetStandardAllocationDriverData` returns `STATUS_NOT_IMPLEMENTED`** and that is the **#1 missing piece** blocking DWM from compositing on Helios: "DWM uses this for the standard primary/staging/shared composition surfaces" (`IDD_HELIOS_RENDER_PLAN.md:48-49`). viogpu3d implements all four DDIs fully and is the structural template.

---

#### A5.0 Authoritative bindgen types (verbatim from `dxgk_bindings_dump.rs`)

##### `DXGKARG_CREATEALLOCATION` — `dxgk_bindings_dump.rs:64212-64257`
Bindgen size assertion: `size_of == 40usize`, `align == 8usize` (lines 64223-64227).

```rust
pub struct _DXGKARG_CREATEALLOCATION {
    pub pPrivateDriverData: *const ::core::ffi::c_void,   // offset 0
    pub PrivateDriverDataSize: UINT,                      // offset 8
    pub NumAllocations: UINT,                             // offset 12
    pub pAllocationInfo: *mut DXGK_ALLOCATIONINFO,        // offset 16
    pub hResource: HANDLE,                                // offset 24
    pub Flags: DXGK_CREATEALLOCATIONFLAGS,                // offset 32
}
pub type DXGKARG_CREATEALLOCATION = _DXGKARG_CREATEALLOCATION;            // 64257
pub type INOUT_PDXGKARG_CREATEALLOCATION = *mut DXGKARG_CREATEALLOCATION; // 64258
```

`DXGK_CREATEALLOCATIONFLAGS` (`dxgk_bindings_dump.rs:64042-64209`) is a 4-byte struct wrapping an anonymous union of a bitfield + `Value: UINT`. The only named bit is `Resource` at bit 0 (the bitfield accessor is `set_Resource(&mut self, val: UINT)` / `Resource(&self) -> UINT` on `_DXGK_CREATEALLOCATIONFLAGS__bindgen_ty_1__bindgen_ty_1`, lines 64069-64105; `Reserved` is bits 1..32, lines 64106-64141). Note: viogpu3d reads `pCreateAllocation->Flags.Resource` (see A5.1); in the bindgen layout this is reached via `args.Flags.__bindgen_anon_1.__bindgen_anon_1.Resource()`.

##### `DXGK_ALLOCATIONINFO` — `dxgk_bindings_dump.rs:63745-63966`
This is the central per-allocation OUT struct the driver fills. Bindgen size `88usize`, align `8usize` (lines 63915-63919). It contains **four anonymous unions** (`__bindgen_anon_1` .. `__bindgen_anon_4`); Step 2 MUST use the exact accessor paths shown.

```rust
pub struct _DXGK_ALLOCATIONINFO {
    pub pPrivateDriverData: *mut ::core::ffi::c_void,         // offset 0
    pub PrivateDriverDataSize: UINT,                         // offset 8
    pub __bindgen_anon_1: _DXGK_ALLOCATIONINFO__bindgen_ty_1, // offset 12 (Alignment | {MinimumPageSize,RecommendedPageSize})
    pub Size: SIZE_T,                                        // offset 16
    pub PitchAlignedSize: SIZE_T,                            // offset 24
    pub HintedBank: DXGK_SEGMENTBANKPREFERENCE,              // offset 32
    pub PreferredSegment: DXGK_SEGMENTPREFERENCE,            // offset 36
    pub __bindgen_anon_2: _DXGK_ALLOCATIONINFO__bindgen_ty_2, // offset 40 (SupportedReadSegmentSet | MmuSet)
    pub SupportedWriteSegmentSet: UINT,                      // offset 44
    pub EvictionSegmentSet: UINT,                            // offset 48
    pub __bindgen_anon_3: _DXGK_ALLOCATIONINFO__bindgen_ty_3, // offset 52 (MaximumRenamingListLength | PhysicalAdapterIndex)
    pub hAllocation: HANDLE,                                 // offset 56
    pub __bindgen_anon_4: _DXGK_ALLOCATIONINFO__bindgen_ty_4, // offset 64 (Flags | FlagsWddm2)
    pub pAllocationUsageHint: *mut DXGK_ALLOCATIONUSAGEHINT, // offset 72
    pub AllocationPriority: UINT,                            // offset 80
    pub Flags2: DXGK_ALLOCATIONINFOFLAGS2,                   // offset 84
}
pub type DXGK_ALLOCATIONINFO = _DXGK_ALLOCATIONINFO;        // 63966
```

The four anonymous unions (exact field names from lines 63763-63911):

- **`__bindgen_anon_1`** (`_DXGK_ALLOCATIONINFO__bindgen_ty_1`, 63765-63768): `Alignment: UINT` **or** `__bindgen_anon_1: { MinimumPageSize: UINT16, RecommendedPageSize: UINT16 }`. Helios writes `info.__bindgen_anon_1.Alignment`.
- **`__bindgen_anon_2`** (`_DXGK_ALLOCATIONINFO__bindgen_ty_2`, 63819-63822): `SupportedReadSegmentSet: UINT` **or** `MmuSet: UINT`. Helios writes `info.__bindgen_anon_2.SupportedReadSegmentSet`.
- **`__bindgen_anon_3`** (`_DXGK_ALLOCATIONINFO__bindgen_ty_3`, 63851-63854): `MaximumRenamingListLength: UINT` **or** `PhysicalAdapterIndex: UINT`.
- **`__bindgen_anon_4`** (`_DXGK_ALLOCATIONINFO__bindgen_ty_4`, 63884-63887): `Flags: DXGK_ALLOCATIONINFOFLAGS` **or** `FlagsWddm2: DXGK_ALLOCATIONINFOFLAGS_WDDM2_0`.

Note: `SupportedWriteSegmentSet` (offset 44) and `EvictionSegmentSet` (offset 48) are **plain fields, not in a union** — write them directly.

There is also a `_DXGK_ALLOCATIONINFO_TEST` flattened mirror at `dxgk_bindings_dump.rs:63967-64040` (size 64). It is a layout-validation helper, not a DDI struct; do not use it.

##### `DXGK_ALLOCATIONINFOFLAGS` (the `Flags` union arm) — `dxgk_bindings_dump.rs:59635-...`
Bit accessor for the WDDM1 flags arm: `CpuVisible` at bit 0 (`fn CpuVisible(&self) -> UINT` / `set_CpuVisible(&mut self, val: UINT)`, lines 59664-59672, `self._bitfield_1.get(0usize, 1u8)`). Other named bits in this arm include `Cached` (59736), `Swizzled` (59916), `Overlay` (59952), `AccessedPhysically` (60204), `CpuVisibleOnDemand` (60312).

##### `DXGK_ALLOCATIONINFOFLAGS_WDDM2_0` (the `FlagsWddm2` union arm — what Helios uses) — `dxgk_bindings_dump.rs:61212-...`
This is the modern flags layout. Full bit map (name → bit index, from `_bitfield_1.get(N usize, 1u8)`):

| Field | Bit | Line |
|---|---|---|
| `CpuVisible` | 0 | 61243 |
| `PermanentSysMem` | 1 | 61279 |
| `Cached` | 2 | 61315 |
| `Protected` | 3 | 61351 |
| `ExistingSysMem` | 4 | 61387 |
| `ExistingKernelSysMem` | 5 | 61423 |
| `FromEndOfSegment` | 6 | 61459 |
| `DisableLargePageMapping` | 7 | 61495 |
| `Overlay` | 8 | 61531 |
| `Capture` | 9 | 61567 |
| `CreateInVpr` | 10 | 61603 |
| `DXGK_ALLOC_RESERVED17` | 11 | 61639 |
| `Reserved02` | 12 | 61675 |
| `MapApertureCpuVisible` | 13 | 61711 |
| `HistoryBuffer` | 14 | 61747 |
| `AccessedPhysically` | 15 | 61783 |
| `ExplicitResidencyNotification` | 16 | 61819 |
| `HardwareProtected` | 17 | 61855 |
| `CpuVisibleOnDemand` | 18 | 61891 |

The exact accessor for the one Helios sets today (`dxgk_bindings_dump.rs:61243-61251`):
```rust
pub fn CpuVisible(&self) -> UINT { ... self._bitfield_1.get(0usize, 1u8) ... }
pub fn set_CpuVisible(&mut self, val: UINT) { ... self._bitfield_1.set(0usize, 1u8, val as u64) }
```
Helios reaches it via `info.__bindgen_anon_4.FlagsWddm2.__bindgen_anon_1.__bindgen_anon_1.set_CpuVisible(1)` (see A5.3).

##### `DXGK_SEGMENTPREFERENCE` = `D3DDDI_SEGMENTPREFERENCE` — `dxgk_bindings_dump.rs:35361` (alias), struct at `15724-...`
```rust
pub type DXGK_SEGMENTPREFERENCE = D3DDDI_SEGMENTPREFERENCE;          // 35361
pub struct _D3DDDI_SEGMENTPREFERENCE { pub __bindgen_anon_1: ... }   // 15724 (union of bitfield | Value: UINT)
```
Bitfield accessors on `_D3DDDI_SEGMENTPREFERENCE__bindgen_ty_1__bindgen_ty_1`:
- `SegmentId0` → bits 0..5 (`fn SegmentId0` / `set_SegmentId0`, lines 15753-15757, `get(0usize, 5u8)`).
- `Direction0` → bit 5 (`fn Direction0` / `set_Direction0`, lines 15789-15797, `get(5usize, 1u8)`).

This is the structural analogue of viogpu3d's `allocationInfo->PreferredSegment.SegmentId0 = 1; .Direction0 = 0;` (see A5.1). Helios does **not** set `PreferredSegment` today (left zero).

##### `DXGKARG_DESCRIBEALLOCATION` — `dxgk_bindings_dump.rs:64752-64810`
Size `48usize`, align `8usize` (64766-64770).
```rust
pub struct _DXGKARG_DESCRIBEALLOCATION {
    pub hAllocation: HANDLE,                              // offset 0
    pub Width: UINT,                                      // offset 8
    pub Height: UINT,                                     // offset 12
    pub Format: D3DDDIFORMAT,                             // offset 16
    pub MultisampleMethod: D3DDDI_MULTISAMPLINGMETHOD,    // offset 20
    pub RefreshRate: D3DDDI_RATIONAL,                     // offset 28
    pub PrivateDriverFormatAttribute: UINT,              // offset 36
    pub Flags: DXGK_DESCRIBEALLOCATIONFLAGS,             // offset 40
    pub Rotation: D3DDDI_ROTATION,                       // offset 44
}
pub type DXGKARG_DESCRIBEALLOCATION = _DXGKARG_DESCRIBEALLOCATION;            // 64809
pub type INOUT_PDXGKARG_DESCRIBEALLOCATION = *mut DXGKARG_DESCRIBEALLOCATION; // 64810
```

##### `DXGKARG_OPENALLOCATION` + `DXGK_OPENALLOCATIONINFO` — `dxgk_bindings_dump.rs:41925-41978` and `41670-41708`
`DXGKARG_OPENALLOCATION` size `56usize`, align `8usize` (41938-41942):
```rust
pub struct _DXGKARG_OPENALLOCATION {
    pub NumAllocations: UINT,                          // offset 0
    pub pOpenAllocation: *mut DXGK_OPENALLOCATIONINFO, // offset 8
    pub pPrivateDriverData: *mut ::core::ffi::c_void,  // offset 16
    pub PrivateDriverSize: UINT,                       // offset 24
    pub Flags: DXGK_OPENALLOCATIONFLAGS,               // offset 28
    pub SubresourceIndex: UINT,                        // offset 32
    pub SubresourceOffset: SIZE_T,                     // offset 40
    pub Pitch: UINT,                                   // offset 48
}
pub type DXGKARG_OPENALLOCATION = _DXGKARG_OPENALLOCATION;                  // 41977
pub type IN_CONST_PDXGKARG_OPENALLOCATION = *const DXGKARG_OPENALLOCATION;  // 41978
```
`DXGK_OPENALLOCATIONINFO` size `32usize`, align `8usize` (41679-41683). **`hDeviceSpecificAllocation` is the OUT field the driver fills** (offset 24):
```rust
pub struct _DXGK_OPENALLOCATIONINFO {
    pub hAllocation: D3DKMT_HANDLE,                  // offset 0  (the dxgkrnl global handle)
    pub pPrivateDriverData: *mut ::core::ffi::c_void,// offset 8
    pub PrivateDriverDataSize: UINT,                 // offset 16
    pub hDeviceSpecificAllocation: HANDLE,           // offset 24 (OUT: driver's device-local handle)
}
pub type DXGK_OPENALLOCATIONINFO = _DXGK_OPENALLOCATIONINFO;  // 41708
```
`DXGK_OPENALLOCATIONFLAGS` (41711-...) named bits: `Create` bit 0 (41739), `ReadOnly` bit 1 (41775). The related close struct: `DXGKARG_CLOSEALLOCATION { NumAllocations: UINT; pOpenHandleList: *const HANDLE }` (`41981-42010`, `IN_CONST_PDXGKARG_CLOSEALLOCATION` at 42010) — `pOpenHandleList` is the list of the `hDeviceSpecificAllocation` handles returned by OpenAllocation.

##### `DXGKARG_GETSTANDARDALLOCATIONDRIVERDATA` — `dxgk_bindings_dump.rs:64892-65012`
Size `48usize`, align `8usize` (64966-64970).
```rust
pub struct _DXGKARG_GETSTANDARDALLOCATIONDRIVERDATA {
    pub StandardAllocationType: D3DKMDT_STANDARDALLOCATION_TYPE,           // offset 0
    pub __bindgen_anon_1: _DXGKARG_GETSTANDARDALLOCATIONDRIVERDATA__bindgen_ty_1, // offset 8 (the per-type pointer union)
    pub pAllocationPrivateDriverData: *mut ::core::ffi::c_void,            // offset 16
    pub AllocationPrivateDriverDataSize: UINT,                            // offset 24
    pub pResourcePrivateDriverData: *mut ::core::ffi::c_void,             // offset 32
    pub ResourcePrivateDriverDataSize: UINT,                             // offset 40
    pub PhysicalAdapterIndex: UINT,                                      // offset 44
}
pub type DXGKARG_GETSTANDARDALLOCATIONDRIVERDATA = _DXGKARG_GETSTANDARDALLOCATIONDRIVERDATA;          // 65011
pub type INOUT_PDXGKARG_GETSTANDARDALLOCATIONDRIVERDATA = *mut DXGKARG_GETSTANDARDALLOCATIONDRIVERDATA;// 65012
```
The per-type pointer is an anonymous union `__bindgen_anon_1` (`_DXGKARG_GETSTANDARDALLOCATIONDRIVERDATA__bindgen_ty_1`, lines 64903-64910), size 8 (one pointer):
```rust
pub union _DXGKARG_GETSTANDARDALLOCATIONDRIVERDATA__bindgen_ty_1 {
    pub pCreateSharedPrimarySurfaceData: *mut D3DKMDT_SHAREDPRIMARYSURFACEDATA,
    pub pCreateShadowSurfaceData:        *mut D3DKMDT_SHADOWSURFACEDATA,
    pub pCreateStagingSurfaceData:       *mut D3DKMDT_STAGINGSURFACEDATA,
    pub pCreateGdiSurfaceData:           *mut D3DKMDT_GDISURFACEDATA,
    pub pCreateVirtualGpuSurfaceData:    *mut D3DKMDT_VIRTUALGPUSURFACEDATA,
    pub pCreateFenceStorageData:         *mut D3DKMDT_FENCESTORAGESURFACEDATA,
}
```
Step-2 access pattern: `(*arg).__bindgen_anon_1.pCreateSharedPrimarySurfaceData`, etc. **Two-call contract** (confirmed by viogpu3d at A5.2): runtime first calls with the `p*Data` and the `p*PrivateDriverData` pointers NULL to query sizes — the driver writes `ResourcePrivateDriverDataSize` + `AllocationPrivateDriverDataSize`; the runtime then re-calls with buffers and the surface-data pointer populated.

##### `D3DKMDT_STANDARDALLOCATION_TYPE` enum — `dxgk_bindings_dump.rs:27867-27876`
Module `_D3DKMDT_STANDARDALLOCATION_TYPE` with `type Type = c_int`:
```rust
pub const D3DKMDT_STANDARDALLOCATION_SHAREDPRIMARYSURFACE: Type = 1; // 27869
pub const D3DKMDT_STANDARDALLOCATION_SHADOWSURFACE:        Type = 2; // 27870
pub const D3DKMDT_STANDARDALLOCATION_STAGINGSURFACE:       Type = 3; // 27871
pub const D3DKMDT_STANDARDALLOCATION_GDISURFACE:           Type = 4; // 27872
pub const D3DKMDT_STANDARDALLOCATION_VGPU:                 Type = 5; // 27873
pub const D3DKMDT_STANDARDALLOCATION_FENCESTORAGE:         Type = 6; // 27874
```

##### Per-type descriptor structs

- `D3DKMDT_SHAREDPRIMARYSURFACEDATA` (`28538-28580`, size 24): `Width: UINT` (0), `Height: UINT` (4), `Format: D3DDDIFORMAT` (8), `RefreshRate: D3DDDI_RATIONAL` (12), `VidPnSourceId: D3DDDI_VIDEO_PRESENT_SOURCE_ID` (20). All IN to the driver.
- `D3DKMDT_SHADOWSURFACEDATA` (`28582-28619`, size 16): `Width: UINT` (0), `Height: UINT` (4), `Format: D3DDDIFORMAT` (8), `Pitch: UINT` (12). `Pitch` is OUT (the driver fills it).
- `D3DKMDT_STAGINGSURFACEDATA` (`28621-28645`, size 12): `Width: UINT` (0), `Height: UINT` (4), `Pitch: UINT` (8). `Pitch` is OUT. (Note: no `Format` member.)
- `D3DKMDT_GDISURFACEDATA` (`28781-28825`, size 24): `Width: UINT` (0), `Height: UINT` (4), `Format: D3DDDIFORMAT` (8), `Type: D3DKMDT_GDISURFACETYPE` (12), `Flags: D3DKMDT_GDISURFACEFLAGS` (16), `Pitch: UINT` (20). This is the one the conceptual docs say DWM uses for the CPU-visible redirection surface (see A5.4). `D3DKMDT_GDISURFACETYPE` variants (`28766-28777`): `INVALID=0, TEXTURE=1, STAGING_CPUVISIBLE=2, STAGING=3, LOOKUPTABLE=4, EXISTINGSYSMEM=5, TEXTURE_CPUVISIBLE=6, TEXTURE_CROSSADAPTER=7, TEXTURE_CPUVISIBLE_CROSSADAPTER=8` (`28776`). `D3DKMDT_GDISURFACEFLAGS` (`28646-28765`) is a 4-byte struct with a single `Reserved` bitfield (bits 0..32, accessor `set_Reserved`/`Reserved`, 28676-28685) plus `Value: UINT` (28655).
- `D3DKMDT_VIRTUALGPUSURFACEDATA` (`28828-28857`, size 24): `Size: UINT64` (0), `Alignment: UINT` (8), `DriverSegmentId: UINT` (12), `PrivateDriverData: UINT` (16).
- `D3DKMDT_FENCESTORAGESURFACEDATA` (`64831-64889`, size 120): includes an embedded `AllocationInfo: DXGK_ALLOCATIONINFO` at offset 32.

---

#### A5.1 `DxgkDdiCreateAllocation` — purpose, viogpu3d template, Helios state

**Purpose:** the runtime (via `D3DKMTCreateAllocation`) asks the KMD to back one or more allocations with GPU/aperture/CPU-visible memory. The KMD reads its per-allocation private driver data, creates the backing resource, fills each `DXGK_ALLOCATIONINFO` (size, segment sets, preferred segment, flags), and returns a driver allocation handle in `hAllocation`.

**Dispatch wiring (viogpu3d):** `InitialData.DxgkDdiCreateAllocation = VioGpu3DCreateAllocation;` (`driver.cpp:103`); thunk `VioGpu3DCreateAllocation` (`driver.cpp:418-432`) checks `IsDriverActive()` then calls `VioGpuAllocation::DxgkCreateAllocation(pAdapter, pCreateAllocation)`.

**viogpu3d quoted impl — `viogpu_allocation.cpp:237-293`:**
```cpp
NTSTATUS VioGpuAllocation::DxgkCreateAllocation(VioGpuAdapter *adapter, DXGKARG_CREATEALLOCATION *pCreateAllocation)
{
    ...
    DXGK_ALLOCATIONINFO *allocationInfo = pCreateAllocation->pAllocationInfo;

    if (max(allocationInfo->PrivateDriverDataSize, pCreateAllocation->PrivateDriverDataSize) <
        sizeof(VIOGPU_CREATE_ALLOCATION_EXCHANGE))
    { ... return STATUS_INVALID_PARAMETER; }

    VIOGPU_CREATE_ALLOCATION_EXCHANGE *resourceExchange =
        (VIOGPU_CREATE_ALLOCATION_EXCHANGE *)allocationInfo->pPrivateDriverData;
    if (pCreateAllocation->PrivateDriverDataSize > allocationInfo->PrivateDriverDataSize)
    { resourceExchange = (VIOGPU_CREATE_ALLOCATION_EXCHANGE *)pCreateAllocation->pPrivateDriverData; }

    VioGpuAllocation *allocation = new (NonPagedPoolNx) VioGpuAllocation(adapter, &resourceExchange->ResourceOptions);
    allocationInfo->hAllocation = allocation;

    if (pCreateAllocation->Flags.Resource)
    { VioGpuResource *resource = new (NonPagedPoolNx) VioGpuResource(); pCreateAllocation->hResource = resource; }

    allocationInfo->Alignment = 0;
    allocationInfo->Size = (SIZE_T)resourceExchange->Size;
    allocationInfo->PitchAlignedSize = 0;
    allocationInfo->HintedBank.Value = 0;
    allocationInfo->AllocationPriority = D3DDDI_ALLOCATIONPRIORITY_NORMAL;
    allocationInfo->EvictionSegmentSet = 1; // don't use apperture for eviction
    allocationInfo->Flags.Value = 0;

    allocationInfo->PreferredSegment.Value = 0;
    allocationInfo->PreferredSegment.SegmentId0 = 1;
    allocationInfo->PreferredSegment.Direction0 = 0;

    allocationInfo->Flags.CpuVisible = TRUE;

    allocationInfo->HintedBank.Value = 0;
    allocationInfo->MaximumRenamingListLength = 0;
    allocationInfo->pAllocationUsageHint = NULL;
    allocationInfo->PhysicalAdapterIndex = 0;
    allocationInfo->PitchAlignedSize = 0;

    allocationInfo->SupportedReadSegmentSet = 0b1;
    allocationInfo->SupportedWriteSegmentSet = 0b1;
    ...
    return STATUS_SUCCESS;
}
```
Key template facts:
- **Private data:** viogpu3d reads `VIOGPU_CREATE_ALLOCATION_EXCHANGE` (`viogpum.h:163-167`: `{ VIOGPU_RESOURCE_OPTIONS ResourceOptions; ULONGLONG Size; }`, `#pragma pack(1)`), preferring `pCreateAllocation->pPrivateDriverData` (resource-level) over the per-allocation `allocationInfo->pPrivateDriverData` when the resource-level size is larger. `VIOGPU_RESOURCE_OPTIONS` is `{ target, format, bind, width, height, depth, array_size, last_level, nr_samples, flags }` (`viogpum.h:141-153`).
- **The real backing happens in the `VioGpuAllocation` ctor** (`viogpu_allocation.cpp:8-28`): `m_Id = m_adapter->resourceIdr.GetId(); m_adapter->ctrlQueue.CreateResource3D(m_Id, options);` — i.e. virtio-gpu `RESOURCE_CREATE_3D`. (viogpu3d does NOT use blobs; backing pages are attached later in `MapApertureSegment`/`AttachBacking`, `viogpu_allocation.cpp:39-58`, 316-330. This is the TRANSFER model Helios replaces.)
- **Segment model:** everything points at segment id 1 (`SupportedReadSegmentSet=0b1`, `SupportedWriteSegmentSet=0b1`, `PreferredSegment.SegmentId0=1`), `Flags.CpuVisible=TRUE`, eviction routed away from the aperture (`EvictionSegmentSet=1`).

**Helios state: REAL.** `kmd_render/src/ddi/create_allocation.rs:132-166` (`dxgkddi_create_allocation`) + `create_one` (62-130). It reads `HeliosWddmAllocPrivate` (`protocol/src/wddm.rs:43-54`; magic `'HWDM'`=0x4857444D, version 1, 48 bytes; fields `blob_id, size, magic, version, blob_mem, blob_flags, ctx_id, map_cache, kind, _pad`), validates `is_valid()`, then:
```rust
let resource_id = match adapter
    .with_virtio(|v| v.resource_create_blob(ap.ctx_id, ap.blob_mem, ap.blob_flags, ap.blob_id, ap.size)) { ... };
```
i.e. the venus HOST3D **blob** path (`create_blob` + `ctx_attach`), NOT `CreateResource3D`. It then fills `DXGK_ALLOCATIONINFO` (lines 113-128):
```rust
info.hAllocation = Box::into_raw(ctx) as HANDLE;
info.Size = size;
info.PitchAlignedSize = size;
info.SupportedWriteSegmentSet = 1; // segment id 1 (bit 0)
info.EvictionSegmentSet = 0; // host-visible blob is pinned; never evicted
unsafe {
    info.__bindgen_anon_1.Alignment = PAGE as UINT;
    info.__bindgen_anon_2.SupportedReadSegmentSet = 1;
    info.__bindgen_anon_3.MaximumRenamingListLength = 0;
    info.__bindgen_anon_4.FlagsWddm2.__bindgen_anon_1.__bindgen_anon_1.set_CpuVisible(1);
}
```
Differences vs the template Step-2 should reconcile: Helios uses `FlagsWddm2.set_CpuVisible(1)` (the WDDM2_0 arm) rather than viogpu3d's `Flags.CpuVisible` (WDDM1 arm); Helios sets `EvictionSegmentSet=0` ("pinned; never evicted") vs viogpu3d's `1`; Helios does **not** set `PreferredSegment` (left zero) whereas viogpu3d sets `PreferredSegment.SegmentId0=1`. Helios stores an `AllocationContext { ctx_id, resource_id, blob_id, size, map_offset, map_len, mapped }` (`create_allocation.rs:26-37`) boxed into `hAllocation`. Multi-allocation calls unwind already-created allocations on error (147-163). `dxgkddi_destroy_allocation` (168-190) frees each boxed ctx via `destroy_allocation_ctx` (47-58: unmap-if-mapped → `ctx_detach_resource` → `resource_unref`).

---

#### A5.2 `DxgkDdiGetStandardAllocationDriverData` — purpose, viogpu3d template, Helios state (THE #1 GAP)

**Purpose:** for runtime-defined "standard" allocations (shared primary, shadow, staging, GDI surface used by DWM/GDI redirection) the runtime asks the *driver* to produce the private-driver-data blob it would have produced for an equivalent app `CreateAllocation`. The driver fills `pAllocationPrivateDriverData` (and optionally `pResourcePrivateDriverData`) so the subsequent `CreateAllocation` can re-parse them. This is the path **DWM and GDI Hardware Acceleration use to create the composition surfaces** — without it, DWM cannot allocate its primary/staging/shared surfaces on Helios. `IDD_HELIOS_RENDER_PLAN.md:48-49` flags Helios's `STATUS_NOT_IMPLEMENTED` here as the reason DWM abandons the adapter.

**Dispatch wiring (viogpu3d):** `InitialData.DxgkDdiGetStandardAllocationDriverData = VioGpu3DGetStandardAllocationDriverData;` (`driver.cpp:108`); thunk `driver.cpp:510-520` → `VioGpuAllocation::GetStandardAllocationDriverData(pStandardAllocation)`.

**viogpu3d quoted impl — `viogpu_allocation.cpp:135-235`:**
The size-query phase (lines 140-145):
```cpp
if (!pStandardAllocation->pResourcePrivateDriverData && !pStandardAllocation->pResourcePrivateDriverData)
{
    pStandardAllocation->ResourcePrivateDriverDataSize = sizeof(VIOGPU_CREATE_RESOURCE_EXCHANGE);
    pStandardAllocation->AllocationPrivateDriverDataSize = sizeof(VIOGPU_CREATE_ALLOCATION_EXCHANGE);
    return STATUS_SUCCESS;
}
```
(Note the source bug — it tests `pResourcePrivateDriverData` twice instead of also `pAllocationPrivateDriverData`; Step 2 should test both pointers. The intent is the two-call contract.)

Then it pre-fills a default `VIOGPU_CREATE_ALLOCATION_EXCHANGE` (lines 147-161): `target=2`, `format=VIRTIO_GPU_FORMAT_R8G8B8X8_UNORM`, `bind = VIRGL_BIND_RENDER_TARGET | VIRGL_BIND_SAMPLER_VIEW | VIRGL_BIND_DISPLAY_TARGET | VIRGL_BIND_SCANOUT`, `width=1024 height=768 depth=1 array_size=1 last_level=0 nr_samples=0 flags=0`. (`virgl_hw.h`: `VIRGL_BIND_RENDER_TARGET=(1<<1)`, `SAMPLER_VIEW=(1<<3)`, `DISPLAY_TARGET=(1<<7)`, `SCANOUT=(1<<18)`, `VIRGL_RESOURCE_FLAG_MAP_COHERENT=(1<<2)`.)

Per standard-type mapping (`viogpu_allocation.cpp:163-234`):
```cpp
case D3DKMDT_STANDARDALLOCATION_SHAREDPRIMARYSURFACE: {
    D3DKMDT_SHAREDPRIMARYSURFACEDATA *surfaceData = pStandardAllocation->pCreateSharedPrimarySurfaceData;
    allocationExchange->ResourceOptions.width  = surfaceData->Width;
    allocationExchange->ResourceOptions.height = surfaceData->Height;
    allocationExchange->ResourceOptions.format = ColorFormat(surfaceData->Format);
    allocationExchange->Size = (ULONGLONG)surfaceData->Width * (ULONGLONG)surfaceData->Height * 4;
    return STATUS_SUCCESS;
}
case D3DKMDT_STANDARDALLOCATION_SHADOWSURFACE: {
    D3DKMDT_SHADOWSURFACEDATA *surfaceData = pStandardAllocation->pCreateShadowSurfaceData;
    allocationExchange->ResourceOptions.width  = surfaceData->Width;
    allocationExchange->ResourceOptions.height = surfaceData->Height;
    allocationExchange->ResourceOptions.format = ColorFormat(surfaceData->Format);
    allocationExchange->Size = (ULONGLONG)surfaceData->Width * (ULONGLONG)surfaceData->Height * 4;
    allocationExchange->ResourceOptions.flags |= VIRGL_RESOURCE_FLAG_MAP_COHERENT;
    surfaceData->Pitch = surfaceData->Width * 4;          // OUT
    return STATUS_SUCCESS;
}
case D3DKMDT_STANDARDALLOCATION_STAGINGSURFACE: {
    D3DKMDT_STAGINGSURFACEDATA *surfaceData = pStandardAllocation->pCreateStagingSurfaceData;
    allocationExchange->ResourceOptions.width  = surfaceData->Width;
    allocationExchange->ResourceOptions.height = surfaceData->Height;
    allocationExchange->ResourceOptions.format = VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM;
    allocationExchange->Size = (ULONGLONG)surfaceData->Width * (ULONGLONG)surfaceData->Height * 4;
    allocationExchange->ResourceOptions.flags |= VIRGL_RESOURCE_FLAG_MAP_COHERENT;
    surfaceData->Pitch = surfaceData->Width * 4;          // OUT
    return STATUS_SUCCESS;
}
default:
    return STATUS_NOT_SUPPORTED;
}
```
Template facts Step 2 must mirror:
- It writes into `pStandardAllocation->pAllocationPrivateDriverData` (cast to `VIOGPU_CREATE_ALLOCATION_EXCHANGE*`) — exactly the same struct shape that `DxgkCreateAllocation` later re-parses, so the two DDIs share one private-data format.
- **Shadow + staging surfaces get `VIRGL_RESOURCE_FLAG_MAP_COHERENT`** and have their **`Pitch` filled (OUT) as `Width * 4`** — these are the CPU-readback surfaces; for Helios the coherent-map flag is the analogue of "host-visible blob, cache-coherent" which is exactly the CPU/IDD-readback coherence point in the locked plan.
- `ColorFormat(UINT)` (`viogpu_adapter.cpp:74-89`) maps `D3DDDIFMT_A8R8G8B8→B8G8R8A8`, `X8R8G8B8→B8G8R8X8`, `A8B8G8R8→R8G8B8A8`, `X8B8G8R8→R8G8B8X8`, default `B8G8R8A8`. viogpu3d does NOT handle GDISURFACE/VGPU/FENCESTORAGE (`default → STATUS_NOT_SUPPORTED`).

**Helios state: STUB (returns `STATUS_NOT_IMPLEMENTED`).** `kmd_render/src/ddi/create_allocation.rs:242-248`:
```rust
pub unsafe extern "C" fn dxgkddi_get_standard_allocation_driver_data(
    _h_adapter: IN_CONST_HANDLE,
    _standard_allocation: INOUT_PDXGKARG_GETSTANDARDALLOCATIONDRIVERDATA,
) -> NTSTATUS {
    crate::diag::record(0x0C02_0002);
    STATUS_NOT_IMPLEMENTED
}
```

**What Step 2 must implement here (the #1 gap):**
1. Honor the **two-call contract**: on the first call (when both `pAllocationPrivateDriverData` and `pResourcePrivateDriverData` are NULL) set `AllocationPrivateDriverDataSize = size_of::<HeliosWddmAllocPrivate>()` (48) and `ResourcePrivateDriverDataSize` (its resource-level analogue, e.g. 0 or a small magic struct) and return `STATUS_SUCCESS`. On the second call, fill the buffers.
2. On the populated call, switch on `(*arg).StandardAllocationType` and read the matching union arm (`pCreateSharedPrimarySurfaceData` / `pCreateShadowSurfaceData` / `pCreateStagingSurfaceData` / `pCreateGdiSurfaceData`). At minimum implement `SHAREDPRIMARYSURFACE`, `SHADOWSURFACE`, `STAGINGSURFACE`, and **`GDISURFACE`** (the conceptual docs, A5.4, say DWM/GDI redirection uses the GDI surface; viogpu3d omits it).
3. Write a `HeliosWddmAllocPrivate` into `pAllocationPrivateDriverData` describing a venus blob sized `Width*Height*4` (round to page), `kind = HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY` or `..._SHMEM`, `blob_mem = VIRTIO_GPU_BLOB_MEM_HOST3D`, `blob_flags` with mappable, and **`map_cache` requesting a coherent/cached mapping** for the surfaces DWM/IDD read back — this is the coherence point. For shadow/staging surfaces set the OUT `Pitch = Width*4`. Because Helios's `CreateAllocation` already re-parses `HeliosWddmAllocPrivate`, the same struct flows straight through GSADD → CreateAllocation → `resource_create_blob`.

---

#### A5.3 `DxgkDdiDescribeAllocation`

**Purpose:** report an existing allocation's surface metadata (dimensions, format, multisample, refresh, rotation) — used by the runtime when it needs to describe a shared/standard allocation it did not itself parameterize.

**viogpu3d quoted impl — `viogpu_allocation.cpp:295-314`** (thunk `driver.cpp:434-446` reads `pDescribeAllocation->hAllocation` back as a `VioGpuAllocation*`):
```cpp
NTSTATUS VioGpuAllocation::DescribeAllocation(DXGKARG_DESCRIBEALLOCATION *pDescribeAllocation)
{
    pDescribeAllocation->Width  = m_options.width;
    pDescribeAllocation->Height = m_options.height;
    pDescribeAllocation->PrivateDriverFormatAttribute = 0;
    pDescribeAllocation->Format = VioGpuToD3DDDIColorFormat((virtio_gpu_formats)m_options.format);
    // this values are RANDOM
    pDescribeAllocation->MultisampleMethod.NumQualityLevels = 2;
    pDescribeAllocation->MultisampleMethod.NumSamples = 2;
    pDescribeAllocation->RefreshRate.Numerator   = 148500000;
    pDescribeAllocation->RefreshRate.Denominator = 2475000;
    return STATUS_SUCCESS;
}
```
(`VioGpuToD3DDDIColorFormat`, the inverse mapping, is at `viogpu_allocation.cpp:96-113`.) Note it does NOT set `Rotation` or `Flags`.

**Helios state: STUB (`STATUS_NOT_IMPLEMENTED`).** `kmd_render/src/ddi/create_allocation.rs:232-238`:
```rust
pub unsafe extern "C" fn dxgkddi_describe_allocation(
    _h_adapter: IN_CONST_HANDLE,
    _describe_allocation: INOUT_PDXGKARG_DESCRIBEALLOCATION,
) -> NTSTATUS {
    crate::diag::record(0x0C02_0001);
    STATUS_NOT_IMPLEMENTED
}
```
The Helios source comment notes it is "Filled with real metadata in Stage 3 (once submits reference allocations)." Step 2 should fill `Width`/`Height`/`Format` from the stored `AllocationContext` (which today stores no width/height/format — it stores `size`/`resource_id`/`blob_id`; Step 2 must extend `AllocationContext` and/or `HeliosWddmAllocPrivate`/GSADD to carry surface dimensions so DescribeAllocation can answer).

---

#### A5.4 `DxgkDdiOpenAllocation` (and CloseAllocation)

**Purpose:** binds an allocation to a *device* context, producing a device-local handle (`hDeviceSpecificAllocation`) the submission path uses. Critical finding (gate5a, recorded in the Helios source comment `create_allocation.rs:194-200`): "dxgkrnl calls this for **EVERY** allocation (including ones the same device just created via `CreateAllocation`, not only cross-process opens), so it must succeed or `D3DKMTCreateAllocation` fails with the open status."

**viogpu3d quoted impl — `viogpu_device.cpp:312-326`** (thunk `driver.cpp:448-458` casts `hDevice` to `VioGpuDevice*` → `OpenAllocation`):
```cpp
NTSTATUS VioGpuDevice::OpenAllocation(_In_ CONST DXGKARG_OPENALLOCATION *pOpenAllocation)
{
    for (UINT i = 0; i < pOpenAllocation->NumAllocations; i++)
    {
        DXGK_OPENALLOCATIONINFO *openAllocationInfo = &pOpenAllocation->pOpenAllocation[i];
        VioGpuAllocation *allocation = m_pAdapter->AllocationFromHandle(openAllocationInfo->hAllocation);
        openAllocationInfo->hDeviceSpecificAllocation = new (NonPagedPoolNx) VioGpuDeviceAllocation(this, allocation);
    }
    return STATUS_SUCCESS;
}
```
The `VioGpuDeviceAllocation` ctor (`viogpu_device.cpp:335-346`) does the real bind: `m_pDevice->GetCtrlQueue()->CtxResource(true, m_pDevice->GetId(), m_pAllocation->GetId());` (virtio-gpu `CTX_ATTACH_RESOURCE`); its dtor (348-355) calls `CtxResource(false, ...)` (detach). `CloseAllocation` (`driver.cpp:462-478`) deletes each `VioGpuDeviceAllocation` from `pOpenHandleList`. Note `pOpenAllocation[i].hAllocation` is the dxgkrnl global `D3DKMT_HANDLE`, resolved to the driver allocation via `AllocationFromHandle`; the OUT `hDeviceSpecificAllocation` is later read in submit (`viogpu_device.cpp:188-206` casts it back to `VioGpuDeviceAllocation`).

**Helios state: REAL (echo-handle).** `kmd_render/src/ddi/create_allocation.rs:201-220`:
```rust
pub unsafe extern "C" fn dxgkddi_open_allocation(
    _h_device: IN_CONST_HANDLE,
    open_allocation: IN_CONST_PDXGKARG_OPENALLOCATION,
) -> NTSTATUS {
    crate::diag::record(0x0C02_0003);
    if open_allocation.is_null() { return STATUS_INVALID_PARAMETER; }
    let args = unsafe { &*open_allocation };
    if args.NumAllocations != 0 && args.pOpenAllocation.is_null() { return STATUS_INVALID_PARAMETER; }
    for i in 0..args.NumAllocations as usize {
        let info = unsafe { &mut *args.pOpenAllocation.add(i) };
        info.hDeviceSpecificAllocation = info.hAllocation as usize as HANDLE;
    }
    STATUS_SUCCESS
}
```
It echoes the dxgkrnl global handle as the device-local handle (the source comment: "Stage 3 maps it to the `AllocationContext`"). `dxgkddi_close_allocation` (223-228) is a no-op returning `STATUS_SUCCESS`. Difference from the template: viogpu3d allocates a per-device wrapper and does `CTX_ATTACH_RESOURCE` here; Helios already did `ctx_attach` inside `resource_create_blob` at CreateAllocation time, so the echo is sufficient for the same-device case — but for true cross-process opens Step 2 will need `info.hAllocation` (a `D3DKMT_HANDLE`, not a pointer) resolved through an adapter-side handle table, since echoing a global D3DKMT handle as a kernel pointer is only valid because the submit path does not yet dereference it.

---

#### A5.5 Conceptual-doc support for the GDI/CPU-visible surface (DWM path)

From `windows-driver-docs-pr/display/setting-the-size-and-pitch-of-the-memory-allocation.md` (lines 22-29):
- (line 22) "Allocations are visible to the CPU if the **pGetStandardAllocationDriverData**->**pCreateGdiSurfaceData**->**Type** member is set to D3DKMDT_GDISURFACE_STAGING_CPUVISIBLE or D3DKMDT_GDISURFACE_EXISTINGSYSMEM."
- (lines 25-29) "When the driver processes a call to *DxgkDdiGetStandardAllocationDriverData* for an allocation that is visible to the CPU, it should: 1. Set the ...->**StandardAllocationType** member to D3DKMDT_STANDARDALLOCATION_GDISURFACE. 2. Set the description of a surface that can be used for redirection by GDI Hardware Acceleration and the Desktop Windows Manager (DWM) through the **D3DKMDT_GDISURFACEDATA** structure ... set the pitch of the allocation through the **Pitch** member of D3DKMDT_GDISURFACEDATA."

This is direct guidance for Step 2: the DWM/GDI-redirection CPU-visible surface arrives as `StandardAllocationType == D3DKMDT_STANDARDALLOCATION_GDISURFACE` (=4) with `pCreateGdiSurfaceData` pointing at a `D3DKMDT_GDISURFACEDATA` whose `Type` is `D3DKMDT_GDISURFACE_STAGING_CPUVISIBLE` (=2) or `EXISTINGSYSMEM` (=5); the driver must fill `Pitch`. viogpu3d does NOT handle this case (it returns `STATUS_NOT_SUPPORTED` in `default`), so for the DWM-composites-on-Helios goal the GDISURFACE arm is something **Step 2 must add beyond the viogpu3d template** — backing it with a coherent host-visible venus blob (so the IDD/CPU readback path stays coherent, matching the locked plan's CPU/IDD-readback coherence point).

Other doc files referencing GSADD (for cross-reference only, not quoted): `mcdm-implementation-guidelines.md`, `gdi-hardware-acceleration.md`, `threading-and-synchronization-zero-level.md`, `rendering-on-a-discrete-gpu-using-cross-adapter-resources.md`.

---

#### A5.6 Summary table (real / stub / missing)

| DDI | bindgen arg type (line) | viogpu3d impl | Helios `kmd_render` state |
|---|---|---|---|
| `DxgkDdiCreateAllocation` | `DXGKARG_CREATEALLOCATION` (64212) | `viogpu_allocation.cpp:237-293` (RESOURCE_CREATE_3D, CpuVisible seg 1) | **REAL** — `create_allocation.rs:62-166` (`resource_create_blob` HOST3D blob; `FlagsWddm2.set_CpuVisible(1)`, seg sets=1, `EvictionSegmentSet=0`) |
| `DxgkDdiDescribeAllocation` | `DXGKARG_DESCRIBEALLOCATION` (64752) | `viogpu_allocation.cpp:295-314` (W/H/Format from m_options) | **STUB** — `create_allocation.rs:232-238` → `STATUS_NOT_IMPLEMENTED` |
| `DxgkDdiOpenAllocation` | `DXGKARG_OPENALLOCATION` (41925) / `DXGK_OPENALLOCATIONINFO` (41670) | `viogpu_device.cpp:312-326` (per-device wrapper + CTX_ATTACH_RESOURCE) | **REAL (echo)** — `create_allocation.rs:201-220` (echoes `hAllocation`→`hDeviceSpecificAllocation`) |
| `DxgkDdiGetStandardAllocationDriverData` | `DXGKARG_GETSTANDARDALLOCATIONDRIVERDATA` (64892) | `viogpu_allocation.cpp:135-235` (two-call sizes; PRIMARY/SHADOW/STAGING → VIOGPU_CREATE_ALLOCATION_EXCHANGE; MAP_COHERENT on shadow/staging) | **STUB / #1 MISSING** — `create_allocation.rs:242-248` → `STATUS_NOT_IMPLEMENTED`; Step 2 must implement two-call contract + PRIMARY/SHADOW/STAGING/**GDISURFACE** → `HeliosWddmAllocPrivate` coherent blobs |

### A6. Command / submission / fence / interrupt path — SubmitCommand, Patch, scheduler, DxgkCbNotifyInterrupt, DXGK_INTERRUPT_DMA_COMPLETED

This section establishes the **submit → complete → fence-signal contract** that is the #1 coherence task for the fake-VidMm model: the WDDM scheduler fence advertised to dxgkrnl must signal *only after* the corresponding venus work has actually completed on the host GPU. It gives (1) the verbatim bindgen types for every struct on that path, (2) viogpu3d quoted as the working wiring template (virtio-gpu fence completion → `DxgkCbNotifyInterrupt(DMA_COMPLETED)`), (3) the current kmd_render state (a *wrong* immediate null-engine fence), and (4) a precise statement of the architectural mismatch between venus-over-Escape and the WDDM scheduler fence.

---

#### A6.1 The DDI entry points (bindgen function-pointer typedefs)

The scheduler-facing DDIs are stored in `_DRIVER_INITIALIZATION_DATA` (the structure passed to `DxgkInitialize`). Their slots, verbatim (`dxgk_bindings_dump.rs:95204-95226`):

```rust
    pub DxgkDdiInterruptRoutine: PDXGKDDI_INTERRUPT_ROUTINE,   // :95204
    pub DxgkDdiDpcRoutine: PDXGKDDI_DPC_ROUTINE,               // :95205
    pub DxgkDdiPatch: PDXGKDDI_PATCH,                          // :95223
    pub DxgkDdiSubmitCommand: PDXGKDDI_SUBMITCOMMAND,          // :95224
    pub DxgkDdiPreemptCommand: PDXGKDDI_PREEMPTCOMMAND,        // :95225
    pub DxgkDdiBuildPagingBuffer: PDXGKDDI_BUILDPAGINGBUFFER,  // :95226
```

The function-pointer typedefs (`dxgk_bindings_dump.rs:88367-88384`):

```rust
pub type PDXGKDDI_PATCH = ::core::option::Option<
    unsafe extern "C" fn(
        arg1: IN_CONST_HANDLE,
        arg2: IN_CONST_PDXGKARG_PATCH,
    ) -> NTSTATUS,
>;
pub type PDXGKDDI_SUBMITCOMMAND = ::core::option::Option<
    unsafe extern "C" fn(
        arg1: IN_CONST_HANDLE,
        arg2: IN_CONST_PDXGKARG_SUBMITCOMMAND,
    ) -> NTSTATUS,
>;
pub type PDXGKDDI_PREEMPTCOMMAND = ::core::option::Option<
    unsafe extern "C" fn(
        arg1: IN_CONST_HANDLE,
        arg2: IN_CONST_PDXGKARG_PREEMPTCOMMAND,
    ) -> NTSTATUS,
>;
```

ISR/DPC typedefs (`dxgk_bindings_dump.rs:94368-94376`, aliased at `:94828-94829` as `PDXGKDDI_INTERRUPT_ROUTINE = DXGKDDI_INTERRUPT_ROUTINE` / `PDXGKDDI_DPC_ROUTINE = DXGKDDI_DPC_ROUTINE`):

```rust
pub type DXGKDDI_INTERRUPT_ROUTINE = ::core::option::Option<
    unsafe extern "C" fn(
        MiniportDeviceContext: IN_CONST_PVOID,
        MessageNumber: IN_ULONG,
    ) -> BOOLEAN,
>;
pub type DXGKDDI_DPC_ROUTINE = ::core::option::Option<
    unsafe extern "C" fn(MiniportDeviceContext: IN_CONST_PVOID),
>;
```

Note the ISR/DPC entry point passes the **miniport device context** (raw `PVOID`), whereas `SubmitCommand`/`Patch`/`Preempt` pass the **adapter handle** (`IN_CONST_HANDLE`). Both ultimately resolve to the same per-adapter object.

---

#### A6.2 `DXGKARG_SUBMITCOMMAND` (the inbound submission descriptor)

Verbatim (`dxgk_bindings_dump.rs:66209-66227`; total size asserted 96 bytes at `:66262`):

```rust
pub struct _DXGKARG_SUBMITCOMMAND {
    pub __bindgen_anon_1: _DXGKARG_SUBMITCOMMAND__bindgen_ty_1,   // hDevice / hContext union, offset 0
    pub DmaBufferSegmentId: UINT,                                 // offset 8
    pub DmaBufferPhysicalAddress: PHYSICAL_ADDRESS,               // offset 16
    pub DmaBufferSize: UINT,                                      // offset 24
    pub DmaBufferSubmissionStartOffset: UINT,                     // offset 28
    pub DmaBufferSubmissionEndOffset: UINT,                       // offset 32
    pub pDmaBufferPrivateData: *mut ::core::ffi::c_void,          // offset 40
    pub DmaBufferPrivateDataSize: UINT,                           // offset 48
    pub DmaBufferPrivateDataSubmissionStartOffset: UINT,          // offset 52
    pub DmaBufferPrivateDataSubmissionEndOffset: UINT,            // offset 56
    pub SubmissionFenceId: UINT,                                  // offset 60  ← THE FENCE
    pub VidPnSourceId: D3DDDI_VIDEO_PRESENT_SOURCE_ID,            // offset 64
    pub FlipInterval: D3DDDI_FLIPINTERVAL_TYPE::Type,             // offset 68
    pub Flags: DXGK_SUBMITCOMMANDFLAGS,                           // offset 72
    pub EngineOrdinal: UINT,                                      // offset 76
    pub DmaBufferVirtualAddress: D3DGPU_VIRTUAL_ADDRESS,          // offset 80
    pub NodeOrdinal: UINT,                                        // offset 88
}
```

The `hDevice`/`hContext` union (`:66230-66233`):

```rust
pub union _DXGKARG_SUBMITCOMMAND__bindgen_ty_1 {
    pub hDevice: HANDLE,
    pub hContext: HANDLE,
}
```

**The load-bearing field is `SubmissionFenceId: UINT`** (offset 60). The driver must remember it, do the work, and only then report this exact id back to dxgkrnl via `DXGK_INTERRUPT_DMA_COMPLETED`. `NodeOrdinal`/`EngineOrdinal` identify which engine on which node — for a single-node single-engine adapter both are 0. `pDmaBufferPrivateData` is the per-submission private blob the UMD attached (viogpu3d stashes its `VioGpuCommand*` here).

`DXGK_SUBMITCOMMANDFLAGS` (`dxgk_bindings_dump.rs:65662-65670`, 4 bytes) is a bitfield union; bindgen emitted the bits as accessor methods on `_DXGK_SUBMITCOMMANDFLAGS__bindgen_ty_1__bindgen_ty_1` over a `__BindgenBitfieldUnit<[u8; 4usize]>` (`:65673-65676`). Bit accessors observed: `Paging()`/`set_Paging()` (bit 0, `:65689-65698`), `Present()`/`set_Present()` (bit 1, `:65726-65735`), `RedirectedPresent()`/`set_RedirectedPresent()` (bit 2, `:65762-65771`), `NullRendering()`/`set_NullRendering()` (bit 3, `:65798-65807`), `Flip()`/`set_Flip()` (bit 4, `:65834-65843`). To read a flag the driver calls e.g. `submit.Flags.__bindgen_anon_1.__bindgen_anon_1.Paging()`. **`Paging` (bit 0) is the one that matters for the fake-VidMm model**: when set, this is a paging DMA buffer built by `DxgkDdiBuildPagingBuffer` and submitted with `hDevice == NULL` — kmd_render's comment at `submit_command.rs:21-25` already flags that this slot must exist precisely because paging buffers route through it.

---

#### A6.3 `DXGKARG_PATCH` (allocation-list patching, the step before submit)

Verbatim (`dxgk_bindings_dump.rs:65531-65552`; total size asserted 120 bytes at `:65585`):

```rust
pub struct _DXGKARG_PATCH {
    pub __bindgen_anon_1: _DXGKARG_PATCH__bindgen_ty_1,          // hDevice/hContext union, offset 0
    pub DmaBufferSegmentId: UINT,                               // offset 8
    pub DmaBufferPhysicalAddress: PHYSICAL_ADDRESS,             // offset 16
    pub pDmaBuffer: *mut ::core::ffi::c_void,                   // offset 24
    pub DmaBufferSize: UINT,                                    // offset 32
    pub DmaBufferSubmissionStartOffset: UINT,                   // offset 36
    pub DmaBufferSubmissionEndOffset: UINT,                     // offset 40
    pub pDmaBufferPrivateData: *mut ::core::ffi::c_void,        // offset 48
    pub DmaBufferPrivateDataSize: UINT,                         // offset 56
    pub DmaBufferPrivateDataSubmissionStartOffset: UINT,        // offset 60
    pub DmaBufferPrivateDataSubmissionEndOffset: UINT,          // offset 64
    pub pAllocationList: *const DXGK_ALLOCATIONLIST,            // offset 72
    pub AllocationListSize: UINT,                              // offset 80
    pub pPatchLocationList: *const D3DDDI_PATCHLOCATIONLIST,    // offset 88
    pub PatchLocationListSize: UINT,                          // offset 96
    pub PatchLocationListSubmissionStart: UINT,               // offset 100
    pub PatchLocationListSubmissionLength: UINT,              // offset 104
    pub SubmissionFenceId: UINT,                              // offset 108
    pub Flags: DXGK_PATCHFLAGS,                               // offset 112
    pub EngineOrdinal: UINT,                                  // offset 116
}
```

Union (`:65555-65558`): `pub union _DXGKARG_PATCH__bindgen_ty_1 { pub hDevice: HANDLE, pub hContext: HANDLE }`.

`DxgkDdiPatch` is where, in a *real* MMU driver, the driver would rewrite GPU virtual addresses inside the DMA buffer using `pAllocationList`/`pPatchLocationList`. **For Helios this is decorative**: the host GPU owns the real MMU and venus addresses resources by opaque id, so there are no guest GPU VAs to patch — `pDmaBuffer` carries no real hardware command stream. viogpu3d's `Patch` (below) is a no-op `return STATUS_SUCCESS`, which is the correct template for the fake model.

`DXGKARG_PREEMPTCOMMAND` for completeness (`dxgk_bindings_dump.rs:66457-66462`, 16 bytes): fields `PreemptionFenceId: UINT`, `NodeOrdinal: UINT`, `EngineOrdinal: UINT`, `Flags: DXGK_PREEMPTCOMMANDFLAGS`. (There is also `DXGKARG_CANCELCOMMAND` at `:66497-66515`, 112 bytes.)

---

#### A6.4 The completion-notification structures (`DXGKARGCB_NOTIFY_INTERRUPT_DATA`)

This is the structure the driver fills in and hands to the `DxgkCbNotifyInterrupt` callback to tell dxgkrnl "fence N completed."

The interrupt-type discriminant (`dxgk_bindings_dump.rs:39470-39492`) — a C-enum `mod` with `Type = ::core::ffi::c_int`. The relevant variants:

```rust
pub const DXGK_INTERRUPT_DMA_COMPLETED: Type = 1;              // :39472  ← fence completion
pub const DXGK_INTERRUPT_DMA_PREEMPTED: Type = 2;              // :39473
pub const DXGK_INTERRUPT_CRTC_VSYNC: Type = 3;                 // :39474
pub const DXGK_INTERRUPT_DMA_FAULTED: Type = 4;                // :39475
pub const DXGK_INTERRUPT_DISPLAYONLY_VSYNC: Type = 5;          // :39476
pub const DXGK_INTERRUPT_DISPLAYONLY_PRESENT_PROGRESS: Type = 6;
pub const DXGK_INTERRUPT_CRTC_VSYNC_WITH_MULTIPLANE_OVERLAY: Type = 7;
pub const DXGK_INTERRUPT_MICACAST_CHUNK_PROCESSING_COMPLETE: Type = 8;
pub const DXGK_INTERRUPT_DMA_PAGE_FAULTED: Type = 9;
pub const DXGK_INTERRUPT_CRTC_VSYNC_WITH_MULTIPLANE_OVERLAY2: Type = 10;
pub const DXGK_INTERRUPT_MONITORED_FENCE_SIGNALED: Type = 11;   // :39482
pub const DXGK_INTERRUPT_HWQUEUE_PAGE_FAULTED: Type = 12;
pub const DXGK_INTERRUPT_HWCONTEXTLIST_SWITCH_COMPLETED: Type = 13;
pub const DXGK_INTERRUPT_PERIODIC_MONITORED_FENCE_SIGNALED: Type = 14;
pub const DXGK_INTERRUPT_SCHEDULING_LOG_INTERRUPT: Type = 15;
pub const DXGK_INTERRUPT_GPU_ENGINE_TIMEOUT: Type = 16;
pub const DXGK_INTERRUPT_SUSPEND_CONTEXT_COMPLETED: Type = 17;
pub const DXGK_INTERRUPT_CRTC_VSYNC_WITH_MULTIPLANE_OVERLAY3: Type = 18;
pub const DXGK_INTERRUPT_NATIVE_FENCE_SIGNALED: Type = 19;      // :39490
pub const DXGK_INTERRUPT_GPU_ENGINE_STATE_CHANGE: Type = 20;
```

Confirmed: `DXGK_INTERRUPT_DMA_COMPLETED = 1`, `DXGK_INTERRUPT_DMA_PREEMPTED = 2`, `DXGK_INTERRUPT_CRTC_VSYNC = 3`, plus monitored-fence variants 11/14 and native-fence 19.

The outer notify-data struct (`dxgk_bindings_dump.rs:40459-40463`):

```rust
pub struct _DXGKARGCB_NOTIFY_INTERRUPT_DATA {
    pub InterruptType: DXGK_INTERRUPT_TYPE,                         // offset 0
    pub __bindgen_anon_1: _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1,   // the per-type union
    pub Flags: DXGKCB_NOTIFY_INTERRUPT_DATA_FLAGS,
}
```

**Critical bindgen representation:** the per-interrupt-type union is *not* a Rust `union` — bindgen lowered it to a `#[repr(C)]` struct of `__BindgenUnionField<T>` members backed by `bindgen_union_field: [u64; 8usize]` (`dxgk_bindings_dump.rs:40465-40530`). Each variant is accessed through the `__BindgenUnionField` accessor (`.as_ref()` / `.as_mut()`), NOT a normal field read. The members (`:40466-40528`):

```rust
pub struct _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1 {
    pub DmaCompleted: __BindgenUnionField<_DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_1>,   // :40466
    pub DmaPreempted: __BindgenUnionField<_DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_2>,   // :40469
    pub DmaFaulted:   __BindgenUnionField<_DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_3>,   // :40472
    pub CrtcVsync:    __BindgenUnionField<_DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_4>,
    pub DisplayOnlyVsync: __BindgenUnionField<...__bindgen_ty_5>,
    pub CrtcVsyncWithMultiPlaneOverlay: __BindgenUnionField<...__bindgen_ty_6>,
    pub DisplayOnlyPresentProgress: __BindgenUnionField<DXGKARGCB_PRESENT_DISPLAYONLY_PROGRESS>,
    pub MiracastEncodeChunkCompleted: __BindgenUnionField<...__bindgen_ty_7>,
    pub DmaPageFaulted: __BindgenUnionField<...__bindgen_ty_8>,
    pub CrtcVsyncWithMultiPlaneOverlay2: __BindgenUnionField<...__bindgen_ty_9>,
    pub MonitoredFenceSignaled: __BindgenUnionField<...__bindgen_ty_10>,   // :40496
    pub HwContextListSwitchCompleted: __BindgenUnionField<...__bindgen_ty_11>,
    pub HwQueuePageFaulted: __BindgenUnionField<...__bindgen_ty_12>,
    pub PeriodicMonitoredFenceSignaled: __BindgenUnionField<...__bindgen_ty_13>,
    pub SchedulingLogInterrupt: __BindgenUnionField<...__bindgen_ty_14>,
    pub GpuEngineTimeout: __BindgenUnionField<...__bindgen_ty_15>,
    pub SuspendContextCompleted: __BindgenUnionField<...__bindgen_ty_16>,
    pub CrtcVsyncWithMultiPlaneOverlay3: __BindgenUnionField<...__bindgen_ty_17>,
    pub NativeFenceSignaled: __BindgenUnionField<...__bindgen_ty_18>,      // :40520
    pub EngineStateChange: __BindgenUnionField<...__bindgen_ty_19>,
    pub Reserved: __BindgenUnionField<...__bindgen_ty_20>,
    pub bindgen_union_field: [u64; 8usize],                                // :40529
}
```

**The `DmaCompleted` member struct** — `_DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_1` (`dxgk_bindings_dump.rs:40533-40537`, size 12 bytes asserted at `:40544`):

```rust
#[derive(Debug, Default, Copy, Clone)]
pub struct _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_1 {
    pub SubmissionFenceId: UINT,   // offset 0   ← the fence id from DXGKARG_SUBMITCOMMAND
    pub NodeOrdinal: UINT,         // offset 4
    pub EngineOrdinal: UINT,       // offset 8
}
```

Confirmed `SubmissionFenceId` / `NodeOrdinal` / `EngineOrdinal` all present. **This is the exact struct kmd_render must fill in** to signal completion. The accessor pattern (already used correctly by kmd_render, see A6.6) is `interrupt.__bindgen_anon_1.DmaCompleted.as_mut()`.

The `DmaPreempted` member — `..._bindgen_ty_2` (`:40568-40573`, 16 bytes): `PreemptionFenceId: UINT`, `LastCompletedFenceId: UINT`, `NodeOrdinal: UINT`, `EngineOrdinal: UINT`. The `DmaFaulted` member — `..._bindgen_ty_3` (`:40609`+): `FaultedFenceId`, `Status`, `NodeOrdinal`, `EngineOrdinal`.

**Monitored-fence variant** — `..._bindgen_ty_10` (`dxgk_bindings_dump.rs:40994-40997`, 8 bytes):

```rust
pub struct _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_10 {
    pub NodeOrdinal: UINT,
    pub EngineOrdinal: UINT,
}
```

Note that `DXGK_INTERRUPT_MONITORED_FENCE_SIGNALED` carries **no fence id in this struct** — for monitored fences the *value* is written by the GPU to a shared fence-storage allocation and dxgkrnl reads it; the interrupt only says "go re-scan the monitored-fence region for node/engine." This matters if Step 2 ever moves to GPU monitored fences (the modern context-fence model) instead of the legacy `DMA_COMPLETED` packet-fence model. The **native-fence** variant `..._bindgen_ty_18` (`:41388-41394`, 32 bytes) carries `NodeOrdinal`, `EngineOrdinal`, `SignaledNativeFenceCount: UINT`, `pSignaledNativeFenceArray: *mut HANDLE`, `hHWQueue: HANDLE`.

**The notify-data flags** — `DXGKCB_NOTIFY_INTERRUPT_DATA_FLAGS` (`dxgk_bindings_dump.rs:39536-39544`, 4-byte bitfield union). Bit accessor observed: `ValidPhysicalAdapterMask()`/`set_ValidPhysicalAdapterMask()` (bit 0, `:39565-39574`). For a single-physical-adapter (non-LDA) render adapter this is left 0.

---

#### A6.5 The completion callbacks (`DxgkCbNotifyInterrupt` / `DxgkCbNotifyDpc` / `DxgkCbQueueDpc`)

The callback typedefs (`dxgk_bindings_dump.rs:41659-41667`):

```rust
pub type DXGKCB_NOTIFY_INTERRUPT = ::core::option::Option<
    unsafe extern "C" fn(
        hAdapter: IN_CONST_HANDLE,
        arg1: IN_CONST_PDXGKARGCB_NOTIFY_INTERRUPT_DATA,
    ),
>;
pub type DXGKCB_NOTIFY_DPC = ::core::option::Option<
    unsafe extern "C" fn(hAdapter: IN_CONST_HANDLE),
>;
```

These live in `_DXGKRNL_INTERFACE` (`dxgk_bindings_dump.rs:93985-93986`, offsets 128 / 136 per `:94102-94106`):

```rust
    pub DxgkCbNotifyInterrupt: DXGKCB_NOTIFY_INTERRUPT,   // :93985
    pub DxgkCbNotifyDpc: DXGKCB_NOTIFY_DPC,               // :93986
```

(`DxgkCbQueueDpc` and `DxgkCbSynchronizeExecution`, used by viogpu3d below, are also members of `_DXGKRNL_INTERFACE`.)

**The contract:** `DxgkCbNotifyInterrupt` must be called at DIRQL (from the ISR) **or** inside a `DxgkCbSynchronizeExecution` callback that raises to DIRQL (viogpu3d's approach, see below); it records the fence-completion packet into dxgkrnl's interrupt queue. It does NOT by itself wake the scheduler — the driver must then call `DxgkCbQueueDpc` (from the ISR) or `DxgkCbNotifyDpc` (from the DPC) so dxgkrnl drains the queued packets at DISPATCH_LEVEL and advances the software fence. So the canonical cycle is: **ISR claims interrupt → `DxgkCbQueueDpc` → DPC drains hw completion → per fence `DxgkCbNotifyInterrupt(DMA_COMPLETED, fenceId)` → `DxgkCbNotifyDpc`.**

---

#### A6.6 viogpu3d — the working wiring template

viogpu3d is a *full DISPLAY* driver that moves memory with virtio-gpu TRANSFER queues, but its **submit → virtio fence → DPC → `DxgkCbNotifyInterrupt(DMA_COMPLETED)`** chain is exactly the template Helios must reproduce (substituting venus submission for the TRANSFER path). The chain has five links.

**Link 1 — DDI entry points wired in `driver.cpp`** (`viogpu3d/driver.cpp:91-138`):

```cpp
    InitialData.DxgkDdiInterruptRoutine = VioGpu3DInterruptRoutine;   // :91
    InitialData.DxgkDdiDpcRoutine = VioGpu3DDpcRoutine;               // :92
    InitialData.DxgkDdiPatch = VioGpu3DPatch;                         // :116
    InitialData.DxgkDdiSubmitCommand = VioGpu3DSubmitCommand;         // :117
    InitialData.DxgkDdiPreemptCommand = VioGpu3DDdiPreemptCommand;    // :138
```

`VioGpu3DSubmitCommand` (`driver.cpp:588-602`) and `VioGpu3DPatch` (`:573-584`) just forward to the commander:

```cpp
VioGpu3DSubmitCommand(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_SUBMITCOMMAND *pSubmitCommand)
{
    ...
    return pAdapter->commander.SubmitCommand(pSubmitCommand);        // :601
};
```
```cpp
VioGpu3DPatch(_In_ CONST HANDLE hAdapter, _In_ CONST DXGKARG_PATCH *pPatch)
{
    ...
    return pAdapter->commander.Patch(pPatch);                        // :583
};
```

**Link 2 — `SubmitCommand` queues the command (does NOT complete it inline)** (`viogpu3d/viogpu_command.cpp:304-330`):

```cpp
NTSTATUS VioGpuCommander::SubmitCommand(const DXGKARG_SUBMITCOMMAND *pSubmitCommand)
{
    ...
    VioGpuCommand *cmd = NULL;
    if (pSubmitCommand->pDmaBufferPrivateData)
    {
        VioGpuCommand **priv = (VioGpuCommand **)pSubmitCommand->pDmaBufferPrivateData;   // :313
        if (*priv != NULL) { cmd = *priv; }
    }
    if (!cmd) { cmd = new (NonPagedPoolNx) VioGpuCommand(m_pAdapter); }   // :322
    cmd->PrepareSubmit(pSubmitCommand);   // :325  captures SubmissionFenceId
    QueueSubmitted(cmd);                  // :326  hands off to worker thread
    ...
    return STATUS_SUCCESS;                // :329  returns BEFORE the work completes
}
```

`PrepareSubmit` captures the fence id and the DMA range (`viogpu_command.cpp:29-40`): `m_FenceId = pSubmitCommand->SubmissionFenceId;` (`:33`). `Patch` is a no-op (`viogpu_command.cpp:289-298`):

```cpp
NTSTATUS VioGpuCommander::Patch(const DXGKARG_PATCH *pPatch)
{
    ...
    UNREFERENCED_PARAMETER(pPatch);
    return STATUS_SUCCESS;   // :297  — no GPU-VA patching; this is the fake-MMU template
}
```

A worker thread (`ThreadWorkRoutine`, `viogpu_command.cpp:239-278`) dequeues submitted commands and calls `command->Run()` (`:275`). `Run()` walks the DMA buffer and, for a submit command, hands the bytes to the virtio control queue **with a completion callback** (`viogpu_command.cpp:64-74`):

```cpp
            case VIOGPU_CMD_SUBMIT:
                {
                    PBYTE submitCmd = new (NonPagedPoolNx) BYTE[cmdHdr->size];
                    RtlCopyMemory(submitCmd, cmdBody, cmdHdr->size);
                    m_pAdapter->ctrlQueue.SubmitCommand(submitCmd,
                                                        cmdHdr->size,
                                                        m_pContext->GetId(),
                                                        VioGpuCommand::QueueRunningCb,   // :72  completion cb
                                                        this);
                    return;   // :74  do NOT signal the WDDM fence yet — wait for virtio completion
                }
```

**Link 3 — the virtio-gpu device interrupt fires when the host finishes the command.** The ISR (`viogpu_adapter.cpp:1515-1572`) reads the ISR status, records the reason, and queues a DPC (`:1563-1567`):

```cpp
    if (serviced)
    {
        InterlockedOr((PLONG)&m_PendingWorks, intReason);
        m_DxgkInterface.DxgkCbQueueDpc(m_DxgkInterface.DeviceHandle);   // :1566
    }
    ...
    return serviced;
```

**Link 4 — the DPC drains the virtio used ring and invokes each command's completion callback** (`viogpu_adapter.cpp:788-848`):

```cpp
VOID VioGpuAdapter::DpcRoutine(VOID)
{
    PGPU_VBUFFER pvbuf = NULL;
    UINT len = 0;
    ULONG reason;
    while ((reason = InterlockedExchange((PLONG)&m_PendingWorks, 0)) != 0)
    {
        if ((reason & ISR_REASON_DISPLAY))
        {
            while ((pvbuf = ctrlQueue.DequeueBuffer(&len)) != NULL)   // :798  pop the used ring
            {
                ...
                if (pvbuf->complete_cb != NULL)
                {
                    pvbuf->complete_cb(pvbuf->complete_ctx);   // :821  → VioGpuCommand::QueueRunningCb
                }
                if (pvbuf->auto_release) { ctrlQueue.ReleaseBuffer(pvbuf); }
            };
        }
        ...
    }
    m_DxgkInterface.DxgkCbNotifyDpc((HANDLE)m_DxgkInterface.DeviceHandle);   // :846
}
```

`complete_cb` is `VioGpuCommand::QueueRunningCb` (`viogpu_command.cpp:157-160`), which re-queues the command so the worker thread runs it again from where it left off — and once the DMA buffer is fully drained, `Run()` reaches the tail.

**Link 5 — the tail of `Run()` signals the WDDM fence with `DXGK_INTERRUPT_DMA_COMPLETED`** (`viogpu3d/viogpu_command.cpp:113-122`) — **this is the exact pattern Helios must reproduce**:

```cpp
    DXGKARGCB_NOTIFY_INTERRUPT_DATA interrupt;
    interrupt.InterruptType = DXGK_INTERRUPT_DMA_COMPLETED;        // :114
    interrupt.DmaCompleted.SubmissionFenceId = m_FenceId;         // :115  the captured fence id
    interrupt.DmaCompleted.NodeOrdinal = 0;                       // :116
    interrupt.DmaCompleted.EngineOrdinal = 0;                     // :117
    m_pAdapter->NotifyInterrupt(&interrupt, true);                // :118  triggerDpc = TRUE
    m_pCommander->CommandFinished();                              // :120
    delete this;
```

`NotifyInterrupt` does the call at DIRQL via `DxgkCbSynchronizeExecution` (`viogpu_adapter.cpp:50-72`):

```cpp
BOOLEAN NotifyRoutine(PVOID ctx_void)
{
    NOTIFY_CONTEXT *ctx = (NOTIFY_CONTEXT *)ctx_void;
    DXGKRNL_INTERFACE *pDxgkInterface = ctx->pDxgkInterface;
    pDxgkInterface->DxgkCbNotifyInterrupt(pDxgkInterface->DeviceHandle, ctx->interrupt);   // :55
    if (ctx->triggerDpc)
    {
        pDxgkInterface->DxgkCbQueueDpc(pDxgkInterface->DeviceHandle);   // :58
    }
    return TRUE;
}
NTSTATUS VioGpuAdapter::NotifyInterrupt(DXGKARGCB_NOTIFY_INTERRUPT_DATA *interruptData, BOOL triggerDpc)
{
    NOTIFY_CONTEXT notify;
    notify.pDxgkInterface = &m_DxgkInterface;
    notify.interrupt = interruptData;
    notify.triggerDpc = triggerDpc;
    BOOLEAN bRet;
    return m_DxgkInterface.DxgkCbSynchronizeExecution(m_DxgkInterface.DeviceHandle, NotifyRoutine, &notify, 0, &bRet);   // :71
}
```

**Summary of the viogpu3d template:** `SubmitCommand` returns immediately after queueing; the *virtio-gpu used-ring interrupt* (real host completion) is what eventually drives `DxgkCbNotifyInterrupt(DMA_COMPLETED, m_FenceId)`. The WDDM fence therefore signals only *after* the host actually finished the command — this is the coherence guarantee Helios needs. viogpu3d's `VioGpu3DDdiPreemptCommand` is an unsupported no-op that still returns `STATUS_SUCCESS` (`driver.cpp:998-1009`).

---

#### A6.7 kmd_render — current state (REAL for the null path, but WRONG for venus coherence)

**`dxgkddi_submit_command` — REAL but a synchronous null engine** (`kmd_render/src/ddi/submit_command.rs:26-65`). It signals the fence *immediately and unconditionally*, inside the SubmitCommand call itself, with no GPU/venus work in between:

```rust
pub unsafe extern "C" fn dxgkddi_submit_command(
    h_adapter: IN_CONST_HANDLE,
    submit_command: IN_CONST_PDXGKARG_SUBMITCOMMAND,
) -> NTSTATUS {
    ...
    // Phase-1 scheduler bring-up: complete the submitted DMA buffer immediately.
    // This is intentionally a null engine ...
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let submit = unsafe { &*submit_command };
    let fence = submit.SubmissionFenceId;                                    // :39
    adapter.last_completed_fence.store(fence, Ordering::Release);            // :40

    let dxgkrnl = match adapter.dxgkrnl() { ... };

    let mut interrupt = unsafe { core::mem::zeroed::<DXGKARGCB_NOTIFY_INTERRUPT_DATA>() };
    interrupt.InterruptType = DXGK_INTERRUPT_DMA_COMPLETED;                  // :48
    let completed = unsafe { interrupt.__bindgen_anon_1.DmaCompleted.as_mut() };  // :49  ← correct __BindgenUnionField accessor
    completed.SubmissionFenceId = fence;                                    // :50
    completed.NodeOrdinal = 0;                                              // :51
    completed.EngineOrdinal = 0;                                            // :52

    if let Some(notify_interrupt) = dxgkrnl.DxgkCbNotifyInterrupt {
        unsafe { notify_interrupt(dxgkrnl.DeviceHandle, &mut interrupt) };  // :55
    } else { return STATUS_DEVICE_NOT_READY; }

    if let Some(queue_dpc) = dxgkrnl.DxgkCbQueueDpc {
        let _ = unsafe { queue_dpc(dxgkrnl.DeviceHandle) };                 // :61
    }
    STATUS_SUCCESS
}
```

This uses the correct types and the correct `DmaCompleted.as_mut()` accessor — but it is **architecturally wrong for coherence**: it asserts completion *before any venus work has been issued or completed*. It exists only to keep dxgkrnl's *paging* path from timing out (comment `:34-36`, `:21-25`). `last_completed_fence` is an `AtomicU32` on the adapter (`adapter.rs:24`, init `:55`), read back by `dxgkddi_query_current_fence` (`submit_command.rs:111-131`, `query.CurrentFence = adapter.last_completed_fence.load(...)`) and by `dxgkddi_reset_engine` (`scheduler.rs:72-74`).

**`dxgkddi_patch` — STUB / MISSING.** Returns `STATUS_NOT_IMPLEMENTED` (`submit_command.rs:103-108`). (viogpu3d returns `STATUS_SUCCESS` from its no-op Patch; kmd_render's `NOT_IMPLEMENTED` is acceptable only because the render path is disabled.)

**`dxgkddi_preempt_command` — STUB.** `STATUS_NOT_IMPLEMENTED` (`submit_command.rs:67-72`). `dxgkddi_render`/`dxgkddi_render_km` — STUB `STATUS_NOT_IMPLEMENTED` (`:87-100`).

**`dxgkddi_interrupt_routine` — STUB / MISSING** (`kmd_render/src/ddi/interrupt.rs:13-19`): "No MSI-X wired yet → never claim the interrupt." Returns `0` (FALSE) always.

```rust
pub unsafe extern "C" fn dxgkddi_interrupt_routine(
    _miniport_device_context: *mut c_void,
    _message_number: u32,
) -> BOOLEAN {
    // No MSI-X wired yet → never claim the interrupt.
    0
}
```

**`dxgkddi_dpc_routine` — STUB / EMPTY** (`interrupt.rs:22-24`): `// Nothing to do until we process completions.` **`dxgkddi_control_interrupt` — STUB** `STATUS_NOT_IMPLEMENTED` (`:31-37`).

**Scheduler hooks (`scheduler.rs`) — partial/REAL-conservative.** `dxgkddi_create_hw_context`/`destroy` allocate a zero-size `HwContext` box (`:78-103`); `dxgkddi_create_hw_queue`/`destroy` a zero-size `HwQueue` box (`:105-126`); `dxgkddi_submit_command_to_hw_queue` returns `STATUS_NOT_SUPPORTED` (`:128-138`); `dxgkddi_preempt`-equivalents, `dxgkddi_query_engine_status` zeroes `DXGK_ENGINESTATUS` (`:35-56`); `dxgkddi_reset_engine` reports `LastAbortedFenceId = last_completed_fence` (`:58-76`). The model is a single node (`NodeOrdinal == 0`) single engine (`EngineOrdinal == 0`); every handler rejects nonzero node/engine.

---

#### A6.8 The submit → complete → fence-signal contract (what Step 2 must build)

The contract dxgkrnl enforces:

1. `DxgkDdiSubmitCommand(h_adapter, &arg)` is called at DISPATCH_LEVEL with `arg.SubmissionFenceId = N` and a DMA range. The driver **may return `STATUS_SUCCESS` immediately** (the work need not be done yet — viogpu3d returns right after queueing).
2. When the work for fence N has *actually completed on the GPU*, the driver fills a `DXGKARGCB_NOTIFY_INTERRUPT_DATA` with `InterruptType = DXGK_INTERRUPT_DMA_COMPLETED` and `DmaCompleted.SubmissionFenceId = N` (`NodeOrdinal`/`EngineOrdinal` = 0), and calls `DxgkCbNotifyInterrupt(DeviceHandle, &data)` at DIRQL.
3. The driver calls `DxgkCbQueueDpc` (from ISR) / `DxgkCbNotifyDpc` (from DPC) so dxgkrnl drains the packet and advances the monotonic software fence; any thread blocked in `D3DKMTWaitForSynchronizationObject`/UMD wait on fence ≤ N is then released.
4. `DxgkDdiQueryCurrentFence` must return the highest completed fence id consistent with the packets already delivered.

The coherence invariant from `CLAUDE.md` — *"Venus commands must be flushed before signaling the fence (ordering)"* (`CLAUDE.md:143`) — maps directly onto step 2: `DxgkCbNotifyInterrupt(DMA_COMPLETED, N)` must not be issued until the venus stream associated with fence N has round-tripped to the host.

---

#### A6.9 The architectural mismatch — venus goes through Escape, the WDDM fence is driven by SubmitCommand

This is the central unresolved problem and must be flagged loudly for Step 2.

**Two disjoint paths exist today, and they never meet:**

- **The venus path is out-of-band via `D3DKMTEscape`.** The ICD submits venus command streams through `HELIOS_ESCAPE_SUBMIT_VENUS` → `escape_submit_venus` (`kmd_render/src/ddi/escape.rs:117-149`), which stages the bytes into a `DmaBuffer` and calls `adapter.with_virtio(|v| v.submit_venus(ctx_id, fence_id, ...))` (`:143`). `submit_venus` **blocks on the virtio-gpu used ring** until the device acknowledges the fenced command — confirmed by the comment at `escape.rs:152-156`: *"`submit_venus` blocks on the used ring until the device acknowledges the fenced command, so any fence the ICD asks to wait on has already completed by the time SUBMIT_VENUS returned."* `escape_wait_fence` (`:157-162`) is therefore a no-op `STATUS_SUCCESS`. **This fence_id is the ICD/venus fence namespace, carried in `HeliosEscapeSubmitVenus`, and is entirely invisible to dxgkrnl.**

- **The WDDM scheduler fence is driven by `DxgkDdiSubmitCommand`.** dxgkrnl issues `DXGKARG_SUBMITCOMMAND.SubmissionFenceId` from the *graphics scheduler*, which only ever calls SubmitCommand for DMA buffers the runtime built via `DxgkDdiRender`/`BuildPagingBuffer`. Because `dxgkddi_render` is `STATUS_NOT_IMPLEMENTED` (`submit_command.rs:87-92`) and the venus stream never flows through a WDDM DMA buffer, **the only thing the WDDM fence ever marks complete is the empty null-engine/paging buffer — and it does so synchronously and unconditionally** (`submit_command.rs:40,48-55`).

**Concretely, they cannot reconcile as built:** the venus work is submitted by a *user-mode Escape ioctl* that does not carry a `DXGKARG_SUBMITCOMMAND.SubmissionFenceId`; the WDDM `DxgkDdiSubmitCommand` fence is generated by the *kernel scheduler* and currently carries no venus work. There is no shared fence-id namespace and no callback from the venus completion into `DxgkCbNotifyInterrupt`. The result is that the WDDM scheduler believes every submission completes instantly (a null engine), which is fine for paging-buffer liveness but is *not* coherent: if DWM (composing the desktop on Helios) ever submits real GPU work through the WDDM render path expecting the fence to gate readback/present, the fence will signal before the venus frame is on the host GPU.

**Reconciliation options Step 2 must choose between (research, not prescription):**

1. **Route venus through `DxgkDdiRender` + `DxgkDdiSubmitCommand`, not Escape.** Make the UMD record venus bytes into the WDDM DMA buffer (as `pDmaBufferPrivateData` / DMA payload) so that `DxgkDdiSubmitCommand` is the single submission point. Then mirror viogpu3d exactly: SubmitCommand queues the venus stream to the virtio control queue with a completion callback, returns `STATUS_SUCCESS`, and the **virtio used-ring interrupt** (via a *real* `dxgkddi_interrupt_routine` + `dxgkddi_dpc_routine`, both currently stubs) drives `DxgkCbNotifyInterrupt(DMA_COMPLETED, submit.SubmissionFenceId)`. This is the cleanest fit to the WDDM contract and makes `escape_submit_venus` unnecessary for the desktop path. Cost: the venus encoder/ICD path must stop using Escape for submit and instead drive the D3DKMT render/submit DDIs.

2. **Bridge the two fence namespaces.** Keep venus on Escape, but have the Escape submit record a mapping (venus fence_id → pending WDDM SubmissionFenceId), make `submit_venus` *asynchronous* (don't block on the used ring), enable the virtio interrupt/DPC, and on used-ring completion of a venus fence, look up the paired WDDM fence and call `DxgkCbNotifyInterrupt(DMA_COMPLETED, wddmFenceId)`. This requires `dxgkddi_submit_command` to stop completing synchronously (remove the immediate `last_completed_fence.store` + `NotifyInterrupt` at `submit_command.rs:40-61`) and instead defer until the paired venus completion arrives. The risk is ordering/aliasing between two independently-incremented id spaces and the fact that the scheduler-side and Escape-side submissions are not 1:1.

Either way, the two stubs that gate the whole coherent path — `dxgkddi_interrupt_routine` (`interrupt.rs:13-19`, returns FALSE always) and `dxgkddi_dpc_routine` (`interrupt.rs:22-24`, empty) — **must become real** (MSI-X claim + virtio used-ring drain), because in viogpu3d those two functions are the only thing that converts a real host completion into `DxgkCbNotifyInterrupt(DMA_COMPLETED)`. The current `dxgkddi_submit_command` immediate fence is a placeholder that must be deleted once a real completion source exists, or it will defeat coherence by signalling fences that the host has not yet finished.

### A8. Present / VidPN DDIs Helios can omit (viogpu3d reference)

**Purpose / scope note.** This section maps how viogpu3d (a *full display* WDDM 3D driver over virtio-gpu) presents and scans out, purely as **contrast and omission guidance** for Helios. Helios is **render-only**: it has no VidPN, exposes no monitor, never owns a scanout, and the OS-composited primary is handed to the Looking Glass IDD by IddCx — not flipped by Helios. The deliverable is to know which present/VidPN DDIs Helios can **omit** and which (if any) the OS still demands of a render-only adapter. viogpu3d is the structural template for the memory-model / VidMm side; on the *present* side it is mostly a list of things Helios does **not** implement.

---

#### A11.1 Which present/VidPN DDIs viogpu3d registers

viogpu3d wires up the full display + present DDI set in its `DriverInitialize` `DRIVER_INITIALIZATION_DATA` (`InitialData`). Verbatim, the present/VidPN-relevant entries:

```cpp
    InitialData.DxgkDdiPresent = VioGpu3DPresent;
    InitialData.DxgkDdiRender = VioGpu3DRender;
    InitialData.DxgkDdiPatch = VioGpu3DPatch;
    InitialData.DxgkDdiSubmitCommand = VioGpu3DSubmitCommand;

    InitialData.DxgkDdiSetPointerPosition = VioGpu3DSetPointerPosition;
    InitialData.DxgkDdiSetPointerShape = VioGpu3DSetPointerShape;
    InitialData.DxgkDdiIsSupportedVidPn = VioGpu3DIsSupportedVidPn;
    InitialData.DxgkDdiRecommendFunctionalVidPn = VioGpu3DRecommendFunctionalVidPn;
    InitialData.DxgkDdiEnumVidPnCofuncModality = VioGpu3DEnumVidPnCofuncModality;
    InitialData.DxgkDdiSetVidPnSourceVisibility = VioGpu3DSetVidPnSourceVisibility;
    InitialData.DxgkDdiCommitVidPn = VioGpu3DCommitVidPn;
    InitialData.DxgkDdiUpdateActiveVidPnPresentPath = VioGpu3DUpdateActiveVidPnPresentPath;
    InitialData.DxgkDdiSetVidPnSourceAddress = VioGpu3DSetVidPnSourceAddress;
    InitialData.DxgkDdiRecommendMonitorModes = VioGpu3DRecommendMonitorModes;
    InitialData.DxgkDdiQueryVidPnHWCapability = VioGpu3DQueryVidPnHWCapability;
    InitialData.DxgkDdiSystemDisplayEnable = VioGpu3DSystemDisplayEnable;
    InitialData.DxgkDdiSystemDisplayWrite = VioGpu3DSystemDisplayWrite;
```
— viogpu3d/driver.cpp:114-131.

So viogpu3d has **both** a per-app/Blt present (`DxgkDdiPresent`) and the VidPN scanout chain (`IsSupportedVidPn` → `EnumVidPnCofuncModality` → `CommitVidPn` → `SetVidPnSourceVisibility` → `SetVidPnSourceAddress`). Note: these are a superset of what a render-only adapter exposes — every entry in this list is a *display-driver* DDI.

---

#### A11.2 IsSupportedVidPn (the VidPN entry gate)

```cpp
NTSTATUS VioGpuVidPN::IsSupportedVidPn(_Inout_ DXGKARG_ISSUPPORTEDVIDPN *pIsSupportedVidPn)
{
    ...
    if (pIsSupportedVidPn->hDesiredVidPn == 0)
    {
        pIsSupportedVidPn->IsVidPnSupported = TRUE;
        ...
    }
    pIsSupportedVidPn->IsVidPnSupported = FALSE;
    ...
    NTSTATUS Status = m_pDxgkInterface->DxgkCbQueryVidPnInterface(pIsSupportedVidPn->hDesiredVidPn, ...);
    ...
    Status = pVidPnInterface->pfnGetTopology(pIsSupportedVidPn->hDesiredVidPn, ...);
    ...
    pIsSupportedVidPn->IsVidPnSupported = TRUE;
```
— viogpu3d/viogpu_vidpn.cpp:791, 799-857. This is the standard topology-validation dance every display driver implements; it has no analog on a render-only adapter (no VidPN is ever presented to it).

---

#### A11.3 CommitVidPn — realizes the pinned source mode (no scanout here)

`CommitVidPn` walks the functional VidPN, acquires topology / source-mode-set / pinned source mode, validates fields, and for each path calls `SetSourceModeAndPath`:

```cpp
NTSTATUS VioGpuVidPN::CommitVidPn(_In_ CONST DXGKARG_COMMITVIDPN *CONST pCommitVidPn)
{
    ...
    Status = m_pDxgkInterface->DxgkCbQueryVidPnInterface(pCommitVidPn->hFunctionalVidPn,
                                                         DXGK_VIDPN_INTERFACE_VERSION_V1, &pVidPnInterface);
    ...
    Status = pVidPnInterface->pfnGetTopology(pCommitVidPn->hFunctionalVidPn, &hVidPnTopology, &pVidPnTopologyInterface);
    ...
    Status = pVidPnInterface->pfnAcquireSourceModeSet(pCommitVidPn->hFunctionalVidPn,
                                                      pCommitVidPn->AffectedVidPnSourceId,
                                                      &hVidPnSourceModeSet, &pVidPnSourceModeSetInterface);
    ...
    Status = pVidPnSourceModeSetInterface->pfnAcquirePinnedModeInfo(hVidPnSourceModeSet, &pPinnedVidPnSourceModeInfo);
    ...
        Status = SetSourceModeAndPath(pPinnedVidPnSourceModeInfo, pVidPnPresentPath);
```
— viogpu3d/viogpu_vidpn.cpp:180, 204-206, 216, 238-241, 253-254, 336. `CommitVidPn` sets up the *mode* (resolution/format) and the framebuffer object; it does **not** itself push a scanout per-frame. The actual virtio-gpu `SET_SCANOUT` happens later, in the flip path keyed off `SetVidPnSourceAddress`.

---

#### A11.4 SetVidPnSourceVisibility — gates source visibility

```cpp
NTSTATUS VioGpuVidPN::SetVidPnSourceVisibility(_In_ CONST DXGKARG_SETVIDPNSOURCEVISIBILITY *pSetVidPnSourceVisibility)
{
    ...
        if (pSetVidPnSourceVisibility->Visible)
        {
            m_CurrentModes[SourceId].Flags.FullscreenPresent = TRUE;
        }
        else
        {
            BlackOutScreen(&m_CurrentModes[SourceId]);
        }
        m_CurrentModes[SourceId].Flags.SourceNotVisible = !(pSetVidPnSourceVisibility->Visible);
```
— viogpu3d/viogpu_vidpn.cpp:1572, 1589-1598. Pure display-state bookkeeping (visible/blackout). No render-adapter relevance.

---

#### A11.5 The present/scanout heart: SetVidPnSourceAddress → Flip → SetScanout + ResFlush

This is the load-bearing path that turns "the OS told me the primary surface's address" into a virtio-gpu scanout. viogpu3d does it **asynchronously**: `SetVidPnSourceAddress` just records the address/allocation and raises a flag; a dedicated kernel thread (`FlipThread`) drains it at ~60 Hz.

`SetVidPnSourceAddress` (note: lives in the **non-paged** code segment, runs at elevated IRQL — it only stashes state):

```cpp
NTSTATUS VioGpuVidPN::SetVidPnSourceAddress(const DXGKARG_SETVIDPNSOURCEADDRESS *pSetVidPnSourceAddress)
{
    m_sourceAddress = pSetVidPnSourceAddress->PrimaryAddress;
    m_sourceRes = reinterpret_cast<VioGpuAllocation *>(pSetVidPnSourceAddress->hAllocation);
    InterlockedOr(&m_shouldFlip, 1);

    return STATUS_SUCCESS;
};
```
— viogpu3d/viogpu_vidpn.cpp:2012-2019. The driver reads `pSetVidPnSourceAddress->PrimaryAddress` (a `PHYSICAL_ADDRESS`) and `->hAllocation` (the WDDM allocation handle, cast straight to `VioGpuAllocation*`).

The flip thread spins on a 16.67 ms delay (`interval.QuadPart = -166666LL` → 16.6 ms ≈ 60 Hz) and calls `Flip()`:

```cpp
void VioGpuVidPN::Flip()
{
    ...
    if (InterlockedExchange(&m_shouldFlip, 0))
    {
        if (m_sourceAddress.QuadPart != 0 && m_sourceRes != NULL)
        {
            m_sourceRes->FlushToScreen(0);
        }
        else
        {
            m_pAdapter->ctrlQueue.SetScanout(0, 0, 0, 0, 0, 0);
        }
    }
    DXGKARGCB_NOTIFY_INTERRUPT_DATA interrupt;
    interrupt.InterruptType = DXGK_INTERRUPT_CRTC_VSYNC;
    interrupt.CrtcVsync.VidPnTargetId = 0;
    interrupt.CrtcVsync.PhysicalAddress = m_sourceAddress;
    m_pAdapter->NotifyInterrupt(&interrupt, true);
}

void VioGpuVidPN::FlipThread(void *ctx)
{
    ...
    interval.QuadPart = -166666LL;
    while (true)
    {
        KeDelayExecutionThread(KernelMode, false, &interval);
        if (vidpn->m_shouldFlipStop) { return; }
        vidpn->Flip();
    }
}
```
— viogpu3d/viogpu_vidpn.cpp:1962-1984 (Flip), 1986-2002 (FlipThread). Two things to note for the IDD contrast:
1. The flip is realized by `m_sourceRes->FlushToScreen(0)` — i.e. by issuing a virtio-gpu **SET_SCANOUT + RESOURCE_FLUSH** on the primary allocation's host resource id.
2. After flipping, viogpu3d **synthesizes a fake vsync** to dxgkrnl via `NotifyInterrupt` with `DXGK_INTERRUPT_CRTC_VSYNC`. This is how a display driver tells the OS "the flip is live." A render-only adapter has no CRTC and emits no such interrupt.

The scanout-from-primary-address logic itself (`FlushToScreen`):

```cpp
void VioGpuAllocation::FlushToScreen(UINT scan_id)
{
    ...
    GPU_BOX box;
    box.x = 0; box.y = 0; box.z = 0;
    box.width = m_options.width;
    box.height = m_options.height;
    box.depth = 1;

    m_adapter->ctrlQueue.SetScanout(scan_id, m_Id, m_options.width, m_options.height, 0, 0);
    m_adapter->ctrlQueue.ResFlush(m_Id, m_options.width, m_options.height, 0, 0);
}
```
— viogpu3d/viogpu_allocation.cpp:115-133. So a primary-surface flip becomes exactly two virtio-gpu commands on the control queue: `SET_SCANOUT(scanout_id, res_id, w, h)` then `RESOURCE_FLUSH(res_id, w, h)`, keyed by the allocation's host **resource id** (`m_Id`).

The underlying queue ops (in the shared common library, not viogpu3d itself):

```cpp
void CtrlQueue::SetScanout(UINT scan_id, UINT res_id, UINT width, UINT height, UINT x, UINT y)
{
    ...
    cmd->hdr.type = VIRTIO_GPU_CMD_SET_SCANOUT;
    cmd->resource_id = res_id;
    cmd->scanout_id = scan_id;
    cmd->r.width = width; cmd->r.height = height; cmd->r.x = x; cmd->r.y = y;
    QueueBuffer(vbuf);
}
```
— viogpu/common/viogpu_queue.cpp:765-786.

```cpp
void CtrlQueue::ResFlush(UINT res_id, UINT width, UINT height, UINT x, UINT y)
{
    ...
    cmd->hdr.type = VIRTIO_GPU_CMD_RESOURCE_FLUSH;
    cmd->resource_id = res_id;
    cmd->r.width = width; cmd->r.height = height; cmd->r.x = x; cmd->r.y = y;
    QueueBuffer(vbuf);
}
```
— viogpu/common/viogpu_queue.cpp:513-533. `SetScanout(0,0,0,0,0,0)` (disable scanout) is also issued from `DestroyFrameBufferObj` on reset — viogpu3d/viogpu_vidpn.cpp:714 — and from `Flip()` when there is no source resource (viogpu3d/viogpu_vidpn.cpp:1974).

The `DXGKARG_SETVIDPNSOURCEADDRESS` struct viogpu3d reads from (verbatim bindgen fields, for Step-2 reference only — Helios never receives this call):

```rust
pub struct _DXGKARG_SETVIDPNSOURCEADDRESS {
    pub VidPnSourceId: D3DDDI_VIDEO_PRESENT_SOURCE_ID,
    pub PrimarySegment: UINT,
    pub PrimaryAddress: PHYSICAL_ADDRESS,
    pub hAllocation: HANDLE,
    pub ContextCount: UINT,
    pub Context: [HANDLE; 65usize],
    pub Flags: DXGK_SETVIDPNSOURCEADDRESS_FLAGS,
    pub Duration: UINT,
    pub PrimaryData: [DXGK_PRIMARYDATA; 64usize],
    pub DriverPrivateDataSize: UINT,
    pub pDriverPrivateData: PVOID,
}
```
— dxgk_bindings_dump.rs:74959-74971 (`size = 2112`, `align = 8`; `PrimaryAddress` at offset 8, `hAllocation` at 16, `Flags` at 552 — dxgk_bindings_dump.rs:74976-75000).

---

#### A11.6 DxgkDdiPresent (per-app Blt present DMA-buffer handling)

This is the *other* present path — `DxgkDdiPresent`, which viogpu3d uses to build a DMA buffer that the GPU scheduler later submits. It is the per-app Blt/Flip present, not the scanout flip. The DDI thunk:

```cpp
VioGpu3DPresent(_In_ CONST HANDLE hDevice, _Inout_ DXGKARG_PRESENT *pPresent)
{
    ...
    VioGpuDevice *pDxContext = reinterpret_cast<VioGpuDevice *>(hDevice);
    return pDxContext->Present(pPresent);
}
```
— viogpu3d/driver.cpp:682-692.

`VioGpuDevice::Present` — for a Flip it does nothing; for a Blt it writes patch-location entries for the source/destination allocations and builds a DMA command buffer:

```cpp
NTSTATUS VioGpuDevice::Present(_Inout_ DXGKARG_PRESENT *pPresent)
{
    ...
    if (pPresent->Flags.Flip)
    {
        return STATUS_SUCCESS;
    }
    ...
    VioGpuCommand *cmd = new (NonPagedPoolNx) VioGpuCommand(m_pAdapter);
    if (pPresent->pDmaBuffer)
    {
        VioGpuCommand **privateData = (VioGpuCommand **)pPresent->pDmaBufferPrivateData;
        *privateData = cmd;
    }
    cmd->SetDmaBuf((char *)pPresent->pDmaBuffer);

    DXGK_ALLOCATIONLIST *dxgk_src = &pPresent->pAllocationList[DXGK_PRESENT_SOURCE_INDEX];
    DXGK_ALLOCATIONLIST *dxgk_dst = &pPresent->pAllocationList[DXGK_PRESENT_DESTINATION_INDEX];
    ...
    if (dxgk_src->hDeviceSpecificAllocation != NULL)
    {
        ...
        pPresent->pPatchLocationListOut->AllocationIndex = DXGK_PRESENT_DESTINATION_INDEX;
        ...
        pPresent->pPatchLocationListOut += 1;
    }
    ...
    if (pPresent->Flags.Blt)
    {
        if (pPresent->pDmaBuffer && dst && src)
        {
            GenerateBltPresent(pPresent, src, dst);
        }
    }
    else
    {
        // emit a NOP command into the DMA buffer
        VIOGPU_COMMAND_HDR *cmd_hdr = (VIOGPU_COMMAND_HDR *)pPresent->pDmaBuffer;
        cmd_hdr->type = VIOGPU_CMD_NOP;
        cmd_hdr->size = 0;
        ...
    }
    return STATUS_SUCCESS;
}
```
— viogpu3d/viogpu_device.cpp:151-241 (Flip early-return 155-158; allocation list / patch-locations 181-218; Blt vs NOP 220-238).

`GenerateBltPresent` emits the actual GPU work — optionally a `VIOGPU_CMD_TRANSFER_TO_HOST` if the source is coherent (staging/shadow), then a `VIOGPU_CMD_SUBMIT` carrying `VIRGL_CCMD_RESOURCE_COPY_REGION` commands per dst sub-rect, then `VIOGPU_CMD_TRANSFER_FROM_HOST` if the destination is coherent:

```cpp
NTSTATUS VioGpuDevice::GenerateBltPresent(DXGKARG_PRESENT *pPresent, VioGpuAllocation *src, VioGpuAllocation *dst)
{
    UCHAR *dmaBuf = (UCHAR *)pPresent->pDmaBuffer;
    ...
    if (src->IsCoherent())
    {
        VIOGPU_COMMAND_HDR *cmd_hdr = (VIOGPU_COMMAND_HDR *)dmaBuf;
        cmd_hdr->type = VIOGPU_CMD_TRANSFER_TO_HOST;
        ...
    }
    {
        ...
        cmd_hdr->type = VIOGPU_CMD_SUBMIT;
        ...
        for (UINT i = 0; i < rectCnt; i++)
        {
            ...
            cmdBody[0] = VIRGL_CMD0(VIRGL_CCMD_RESOURCE_COPY_REGION, 0, VIRGL_CMD_RESOURCE_COPY_REGION_SIZE);
            cmdBody[1] = dst->GetId();
            ...
            cmdBody[6] = src->GetId();
            ...
        }
    }
    if (dst->IsCoherent())
    {
        ...
        cmd_hdr->type = VIOGPU_CMD_TRANSFER_FROM_HOST;
        ...
    }
    pPresent->pDmaBuffer = dmaBuf;
    return STATUS_SUCCESS;
}
```
— viogpu3d/viogpu_device.cpp:42-149. Key takeaway: viogpu3d's `Present` (the Blt path) is **resource-copy + virtio-gpu TRANSFER** — the very copy-engine model Helios deliberately does *not* use (Helios is zero-copy host-visible-blob). And the `Flip` flavor of `DxgkDdiPresent` is a pure no-op (returns `STATUS_SUCCESS` immediately), because the actual on-screen flip is driven elsewhere by `SetVidPnSourceAddress`/`FlipThread`.

The DMA buffer this builds is later consumed by `DxgkDdiSubmitCommand`/`VioGpuCommand::Run`, which walks the `VIOGPU_COMMAND_HDR` stream and dispatches `VIOGPU_CMD_SUBMIT`/`VIOGPU_CMD_TRANSFER_*`/`VIOGPU_CMD_NOP` onto the control queue, then signals `DXGK_INTERRUPT_DMA_COMPLETED` with the submission fence id — viogpu3d/viogpu_command.cpp:46-123 (dispatch loop 62-96; `DXGK_INTERRUPT_DMA_COMPLETED` + `SubmissionFenceId = m_FenceId` at 113-118). That fence-completion interrupt is the same mechanism Helios *does* need (Section: fences/scheduler) — but it is decoupled from the present/scanout machinery above.

---

#### A11.7 The IDD acquire model (why Helios omits all of the above)

On the Looking Glass IDD side, the OS-composited primary is delivered to the IDD as an OS-composed texture that the IDD's swap-chain processor *acquires* — it is never flipped/scanned-out by the render adapter:

```cpp
      hr = IddCxSwapChainReleaseAndAcquireBuffer2(m_hSwapChain, &acquireIn, &buffer);
      ...
      hr = IddCxSwapChainReleaseAndAcquireBuffer(m_hSwapChain, &buffer);
      ...
        SwapChainNewFrame(surface, dirtyRectCount);
      ...
        hr = IddCxSwapChainFinishedProcessingFrame(m_hSwapChain);
```
— LookingGlass/idd/LGIdd/CSwapChainProcessor.cpp:159, 172, 204, 207 (also `IddCxSwapChainSetDevice` at 110, swap-chain processor thread at 62-77). The IDD's monitor + swapchain are what DWM scans the composed desktop into; the IDD pulls each composed frame via `IddCxSwapChainReleaseAndAcquireBuffer[2]`. Present was never the blocker — there is no `pfnPresent`/`SetVidPnSourceAddress` involvement on the render-only Helios adapter; the OS composes onto Helios and hands the result to the IDD.

---

#### A11.8 DELIVERABLE — viogpu3d present DDI map and Helios omit/keep table

**viogpu3d present/scanout map (what a *full display* virtio driver does):**
- `DxgkDdiIsSupportedVidPn` → validate a desired VidPN topology (viogpu_vidpn.cpp:791).
- `DxgkDdiEnumVidPnCofuncModality` → enumerate cofunctional modes (viogpu_vidpn.cpp:1142).
- `DxgkDdiCommitVidPn` → realize the pinned source mode + framebuffer (viogpu_vidpn.cpp:180; `SetSourceModeAndPath` at :336).
- `DxgkDdiSetVidPnSourceVisibility` → visible/blackout bookkeeping (viogpu_vidpn.cpp:1572).
- `DxgkDdiSetVidPnSourceAddress` → record primary `PHYSICAL_ADDRESS` + `hAllocation`, flag a flip (viogpu_vidpn.cpp:2012).
- `FlipThread`/`Flip` → ~60 Hz: `FlushToScreen` → `SetScanout` + `ResFlush` on the primary resource id, then synth `DXGK_INTERRUPT_CRTC_VSYNC` (viogpu_vidpn.cpp:1962, 1986; viogpu_allocation.cpp:115; queue ops viogpu_queue.cpp:765, 513).
- `DxgkDdiPresent` (Blt flavor) → build a DMA buffer of `RESOURCE_COPY_REGION` + optional `TRANSFER_TO/FROM_HOST` (viogpu_device.cpp:151, 42). Flip flavor → no-op return.
- `DxgkDdiSystemDisplayEnable` / `DxgkDdiSystemDisplayWrite` → bugcheck/boot fallback blit path (viogpu_vidpn.cpp:2036, 2093).

**What Helios can OMIT (render-only; no VidPN; IDD acquires the composed frame):**
- `DxgkDdiSetVidPnSourceAddress` — **OMIT.** Helios never owns a primary surface address and never flips. No `m_sourceAddress`/`FlipThread`/`Flip`.
- `DxgkDdiSetVidPnSourceVisibility` — **OMIT.** No source to make visible/invisible.
- `DxgkDdiCommitVidPn`, `DxgkDdiIsSupportedVidPn`, `DxgkDdiEnumVidPnCofuncModality`, `DxgkDdiRecommendFunctionalVidPn`, `DxgkDdiRecommendVidPnTopology`, `DxgkDdiUpdateActiveVidPnPresentPath`, `DxgkDdiRecommendMonitorModes`, `DxgkDdiQueryVidPnHWCapability` — **OMIT.** These are the VidPN/mode-enumeration surface of a *display* driver; a render-only adapter exposes no VidPN topology, no targets/monitors.
- `DxgkDdiSystemDisplayEnable` / `DxgkDdiSystemDisplayWrite` — **OMIT.** These are the boot/bugcheck "I own a display" fallback. Render-only Helios is not a display owner.
- `DxgkDdiSetPointerPosition` / `DxgkDdiSetPointerShape` — **OMIT.** Hardware-cursor DDIs belong to the display adapter that scans out; the Looking Glass IDD owns the cursor.
- `DxgkDdiStopDeviceAndReleasePostDisplayOwnership` — **OMIT.** Only meaningful if the adapter held POST display ownership.
- The virtio-gpu `SET_SCANOUT` / `RESOURCE_FLUSH` / `TRANSFER_TO/FROM_HOST` machinery (viogpu_queue.cpp:765, 513, 535) — **OMIT entirely.** Helios is zero-copy host-visible-blob + venus; it never scans out and never uses the virtio-gpu copy engine. viogpu3d's `FlushToScreen` and `GenerateBltPresent` are the *anti-pattern* for Helios.
- The synthesized `DXGK_INTERRUPT_CRTC_VSYNC` (viogpu_vidpn.cpp:1977-1983) — **OMIT.** No CRTC on a render-only adapter.

**What the OS still requires from a render-only adapter (KEEP — but these are render/scheduler DDIs, covered in the fence/scheduler sections, NOT present DDIs):**
- `DxgkDdiPresent` — **status: UNCERTAIN / likely still registrable but trivial.** viogpu3d's `DxgkDdiPresent` exists to service GDI/D3D Blt/Flip presents that target this adapter's allocations. For Helios, the **OS composes the desktop and the IDD acquires it via `IddCxSwapChainReleaseAndAcquireBuffer` — the composed-primary path does NOT go through Helios's `pfnPresent`** (confirmed by the IDD acquire model, CSwapChainProcessor.cpp:159-207). The D3D11 UMD (DXVK, Gate 5b) drives rendering through `DxgkDdiRender`/`DxgkDdiSubmitCommand` + the venus submit, not through a kernel Blt present. So a render-only Helios most likely needs no functional `DxgkDdiPresent` (a Flip-style no-op stub at most, mirroring viogpu_device.cpp:155-158, if dxgkrnl insists on a registered entry). Step 2 should treat `DxgkDdiPresent` as *not the mechanism* and confirm at the kernel debugger whether dxgkrnl ever invokes it on a render-only node; do not build the Blt/RESOURCE_COPY_REGION DMA path.
- `DxgkDdiRender` / `DxgkDdiPatch` / `DxgkDdiSubmitCommand` — **KEEP** (these are the render/command-submission DDIs; the present-side analysis here only notes that `DxgkDdiSubmitCommand` is where viogpu3d's DMA stream is consumed and where the `DXGK_INTERRUPT_DMA_COMPLETED` + `SubmissionFenceId` fence is signaled — viogpu_command.cpp:113-118 — which is the mechanism Helios's venus-driven WDDM fence must replicate). Details belong to the fence/scheduler section, not the present section.

**Bottom line for Step 2:** the entire VidPN block (`IsSupported/EnumCofuncModality/CommitVidPn/SetVidPnSourceVisibility/SetVidPnSourceAddress` + monitor-mode + system-display + pointer DDIs) and viogpu3d's scanout/flip/TransferToHost/Blt copy path are **display-driver-only** and are omitted on render-only Helios. The composed desktop reaches Looking Glass through the IDD's `IddCxSwapChainReleaseAndAcquireBuffer` acquire of an OS-composited texture — Helios never scans out, never flips, and never sees `SetVidPnSourceAddress`. The only present-adjacent thing Helios must get right is the fence-completion interrupt (`DXGK_INTERRUPT_DMA_COMPLETED`/`SubmissionFenceId`) driven by the venus submit, which is the scheduler/fence section's concern, not this one.

## Section B — Verbatim Rust types & field layouts

This is the reference appendix: the complete bindgen Rust definitions (from `dxgk_bindings_dump.rs`) for the structs, unions, enums, bitfield accessors, and DDI/callback typedefs named in Section A, grouped by subsystem (B1 caps & segments · B2 allocations, paging buffer, page tables · B3 command submission, device/context, fences, interrupts). Reproduced **verbatim** — including the `__BindgenUnionField` / `__BindgenBitfieldUnit` lowering and the `X()` / `set_X()` accessor methods Step 2 must call — because the anonymous-union/bitfield access pattern is exactly where a hand-written struct would silently diverge from the real ABI. Line numbers are valid for the `.72` dump (see front-matter); field names and types are ABI-stable for WDK 10.0.26100.

---

### B1. Verbatim Rust types — adapter caps & segments

This appendix reproduces, verbatim, the bindgen-emitted Rust definitions of every adapter-caps and segment struct that Step 2 must populate in `DxgkDdiQueryAdapterInfo`. Source file for all blocks below: `/home/rupansh/helios-vgpu/dxgk_bindings_dump.rs` (rust-bindgen 0.71.1, generated from WDK 10.0.26100 `d3dkmddi.h`+`dispmprt.h`, `DXGKDDI_INTERFACE_VERSION` = WDDM 3.2). The dump opens with `pub use wdk_sys::*;` (line 3), so primitive aliases not redefined locally (notably `PHYSICAL_ADDRESS`) resolve through `wdk_sys`.

Note on conventions used throughout: bindgen names every C `struct _FOO` as `pub struct _FOO` and emits `pub type FOO = _FOO;` immediately after. Bitfields are packed into a generic `__BindgenBitfieldUnit<[u8; N]>` and accessed through generated `get/set` (offset+width based) methods; anonymous C unions become a wrapper struct with a nested `__bindgen_ty_1` union exposing both `__bindgen_anon_1` (the bitfield view) and `Value: UINT` (the raw u32 view). **For caps Step 2 should prefer writing the raw `Value` field** (set whole-word) or the named `set_X()` accessors — both write the same backing `_bitfield_1`.

#### B1.0 The two bindgen helper types (load-bearing — every caps/segment struct uses them)

`__BindgenBitfieldUnit<Storage>` — the backing store + accessor engine for all bitfields. The `get`/`set`/`raw_get`/`raw_set` methods are what the per-field `X()`/`set_X()` accessors call.

```rust
// dxgk_bindings_dump.rs:5-15
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct __BindgenBitfieldUnit<Storage> {
    storage: Storage,
}
impl<Storage> __BindgenBitfieldUnit<Storage> {
    #[inline]
    pub const fn new(storage: Storage) -> Self {
        Self { storage }
    }
}
```
The `Storage: AsRef<[u8]> + AsMut<[u8]>` impl (lines 16-146) provides `get_bit`/`set_bit`/`raw_get_bit`/`raw_set_bit` and the width-based `get(bit_offset, bit_width) -> u64`, `set(bit_offset, bit_width, val: u64)`, `raw_get`, `raw_set`. These are the primitives behind every `set_X()`/`X()` below.

`__BindgenUnionField<T>` — the zero-sized placeholder bindgen uses inside a `#[repr(C)]` *struct* that models a C union (used by `_DXGK_SEGMENTDESCRIPTOR4__bindgen_ty_1`). Access is `unsafe`: `.as_ref()` / `.as_mut()` transmute the field placeholder to `&T` / `&mut T`. The real storage is the sibling `bindgen_union_field: [u64; N]` array.

```rust
// dxgk_bindings_dump.rs:147-162
#[repr(C)]
pub struct __BindgenUnionField<T>(::core::marker::PhantomData<T>);
impl<T> __BindgenUnionField<T> {
    #[inline]
    pub const fn new() -> Self {
        __BindgenUnionField(::core::marker::PhantomData)
    }
    #[inline]
    pub unsafe fn as_ref(&self) -> &T {
        ::core::mem::transmute(self)
    }
    #[inline]
    pub unsafe fn as_mut(&mut self) -> &mut T {
        ::core::mem::transmute(self)
    }
}
```
(Followed by `Default`, `Clone`, `Copy`, `Debug`, `Hash`, `PartialEq`, `Eq` impls at lines 163-189.)

Primitive aliases referenced by the structs below (all from the dump):
```rust
// dxgk_bindings_dump.rs (line numbers as noted)
pub type UINT64 = ::core::ffi::c_ulonglong;        // :379
pub type UINT32 = ::core::ffi::c_uint;             // :378
pub type SIZE_T = ULONG_PTR;                       // :383
pub type ULONG_PTR = ::core::ffi::c_ulonglong;     // :382
pub type ULONGLONG = ::core::ffi::c_ulonglong;     // :407
pub type BOOLEAN = UCHAR;                           // :514
pub type UINT = ::core::ffi::c_uint;               // :11796
pub type BYTE = ::core::ffi::c_uchar;              // :11798
pub type D3DGPU_VIRTUAL_ADDRESS = ULONGLONG;       // :11841
pub type PPHYSICAL_ADDRESS = *mut LARGE_INTEGER;   // :512
```
`PHYSICAL_ADDRESS` itself is **not** redefined in this dump; it resolves through `pub use wdk_sys::*;` (line 3), where it is the `LARGE_INTEGER` union (8 bytes). `LARGE_INTEGER` is likewise re-exported from `wdk_sys`.

---

#### B1.1 `DXGK_DRIVERCAPS` (struct `_DXGK_DRIVERCAPS`) — bindgen :51069, size 592, align 8

The top-level adapter caps blob returned for `DXGKQAITYPE_DRIVERCAPS`. Two embedded unions: `__bindgen_anon_1` (gamma/color-transform caps) and `MiscCaps` (`_DXGK_DRIVERCAPS__bindgen_ty_2`, a flags word). `MemoryManagementCaps` is the embedded `DXGK_VIDMMCAPS` (B1.2).

```rust
// dxgk_bindings_dump.rs:51068-51110
#[repr(C)]
pub struct _DXGK_DRIVERCAPS {
    pub HighestAcceptableAddress: PHYSICAL_ADDRESS,
    pub MaxAllocationListSlotId: UINT,
    pub ApertureSegmentCommitLimit: SIZE_T,
    pub MaxPointerWidth: UINT,
    pub MaxPointerHeight: UINT,
    pub PointerCaps: DXGK_POINTERFLAGS,
    pub InterruptMessageNumber: UINT,
    pub NumberOfSwizzlingRanges: UINT,
    pub MaxOverlays: UINT,
    pub __bindgen_anon_1: _DXGK_DRIVERCAPS__bindgen_ty_1,
    pub PresentationCaps: DXGK_PRESENTATIONCAPS,
    pub MaxQueuedFlipOnVSync: UINT,
    pub FlipCaps: DXGK_FLIPCAPS,
    pub SchedulingCaps: DXGK_VIDSCHCAPS,
    pub MemoryManagementCaps: DXGK_VIDMMCAPS,
    pub GpuEngineTopology: DXGK_GPUENGINETOPOLOGY,
    pub WDDMVersion: DXGK_WDDMVERSION,
    pub Reserved: DXGK_VIRTUALADDRESSCAPS_DEPRECATED,
    pub Reserved1: DXGK_DMABUFFERCAPS_DEPRECATED,
    pub PreemptionCaps: D3DKMDT_PREEMPTION_CAPS,
    pub SupportNonVGA: BOOLEAN,
    pub SupportSmoothRotation: BOOLEAN,
    pub SupportPerEngineTDR: BOOLEAN,
    pub SupportDirectFlip: BOOLEAN,
    pub SupportMultiPlaneOverlay: BOOLEAN,
    pub SupportRuntimePowerManagement: BOOLEAN,
    pub SupportSurpriseRemovalInHibernation: BOOLEAN,
    pub HybridDiscrete: BOOLEAN,
    pub MaxOverlayPlanes: UINT,
    pub HybridIntegrated: BOOLEAN,
    pub InternalGpuVirtualAddressRangeStart: D3DGPU_VIRTUAL_ADDRESS,
    pub InternalGpuVirtualAddressRangeEnd: D3DGPU_VIRTUAL_ADDRESS,
    pub SupportSurpriseRemoval: BOOLEAN,
    pub SupportMultiPlaneOverlayImmediateFlip: BOOLEAN,
    pub CursorScaledWithMultiPlaneOverlayPlane0: BOOLEAN,
    pub HybridAcpiChainingRequired: BOOLEAN,
    pub MaxQueuedMultiPlaneOverlayFlipVSync: UINT,
    pub MiscCaps: _DXGK_DRIVERCAPS__bindgen_ty_2,
    pub MaxHwQueuedFlips: UINT,
    pub HwQueuedFlipCaps: DXGK_HWQUEUEDFLIP_CAPS,
}
```

The first embedded union (gamma vs. color-transform caps):
```rust
// dxgk_bindings_dump.rs:51111-51116
#[repr(C)]
#[derive(Copy, Clone)]
pub union _DXGK_DRIVERCAPS__bindgen_ty_1 {
    pub GammaRampCaps: DXGK_GAMMARAMPCAPS,
    pub ColorTransformCaps: DXGK_COLORTRANSFORMCAPS,
}
```

The `MiscCaps` flags union and its bitfield struct:
```rust
// dxgk_bindings_dump.rs:51142-51153
#[repr(C)]
#[derive(Copy, Clone)]
pub union _DXGK_DRIVERCAPS__bindgen_ty_2 {
    pub __bindgen_anon_1: _DXGK_DRIVERCAPS__bindgen_ty_2__bindgen_ty_1,
    pub Value: UINT,
}
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct _DXGK_DRIVERCAPS__bindgen_ty_2__bindgen_ty_1 {
    pub _bitfield_align_1: [u32; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}
```

`MiscCaps` bit layout (from the `impl` block at :51163-51656). Each accessor is `pub fn NAME(&self) -> UINT` / `pub fn set_NAME(&mut self, val: UINT)` (plus `unsafe NAME_raw`/`set_NAME_raw`), backed by `self._bitfield_1.get(offset, width)` / `.set(offset, width, val)`:

| bit offset | width | field | accessor pair |
|---|---|---|---|
| 0 | 1 | `SupportContextlessPresent` | `SupportContextlessPresent()` / `set_SupportContextlessPresent()` |
| 1 | 1 | `Detachable` | `Detachable()` / `set_Detachable()` |
| 2 | 1 | `VirtualGpuOnly` | `VirtualGpuOnly()` / `set_VirtualGpuOnly()` |
| 3 | 1 | `ComputeOnly` | `ComputeOnly()` / `set_ComputeOnly()` |
| 4 | 1 | `IndependentVidPnVSyncControl` | `IndependentVidPnVSyncControl()` / `set_…()` |
| 5 | 1 | `NoHybridDiscreteDListDllSupport` | `NoHybridDiscreteDListDllSupport()` / `set_…()` |
| 6 | 1 | `DisplayableSupport` | `DisplayableSupport()` / `set_DisplayableSupport()` |
| 7 | 1 | `NoHybridDiscreteDListDllMuxSupport` | `NoHybridDiscreteDListDllMuxSupport()` / `set_…()` |
| 8 | 1 | `CursorDoesNotSupportXorBlendWithMultiPlaneOverlay` | `…()` / `set_…()` |
| 9 | 23 | `Reserved` | `Reserved()` / `set_Reserved()` |

The constructor helper (verbatim signature, :51532-51544):
```rust
pub fn new_bitfield_1(
    SupportContextlessPresent: UINT,
    Detachable: UINT,
    VirtualGpuOnly: UINT,
    ComputeOnly: UINT,
    IndependentVidPnVSyncControl: UINT,
    NoHybridDiscreteDListDllSupport: UINT,
    DisplayableSupport: UINT,
    NoHybridDiscreteDListDllMuxSupport: UINT,
    CursorDoesNotSupportXorBlendWithMultiPlaneOverlay: UINT,
    Reserved: UINT,
) -> __BindgenBitfieldUnit<[u8; 4usize]> { /* ... */ }
```

Selected field offsets verified by the bindgen size-assert block (:51678-51808), relevant to Step 2 layout: `MemoryManagementCaps` @ **68**, `WDDMVersion` @ **336**, `SupportNonVGA` @ **536**, `HybridDiscrete` @ **543**, `MiscCaps` @ **576**; total size **592**, align **8**. `_DXGK_DRIVERCAPS` has **no** `#[derive(Default)]` but a hand-written zeroing `impl Default` (:51809-51817), and `pub type DXGK_DRIVERCAPS = _DXGK_DRIVERCAPS;` (:51818).

---

#### B1.2 `DXGK_VIDMMCAPS` (struct `_DXGK_VIDMMCAPS`) — bindgen :49550, size 8, align 4

The video-memory-manager caps. **This is the struct that carries `GpuMmuSupported`, `VirtualAddressingSupported`, `IoMmuSupported`, and `ParavirtualizationSupported`** — the bits that select the fake-GpuMmu model. It is a 1-word flags union + a `PagingNode: UINT`.

```rust
// dxgk_bindings_dump.rs:49549-49566
#[repr(C)]
pub struct _DXGK_VIDMMCAPS {
    pub __bindgen_anon_1: _DXGK_VIDMMCAPS__bindgen_ty_1,
    pub PagingNode: UINT,
}
#[repr(C)]
#[derive(Copy, Clone)]
pub union _DXGK_VIDMMCAPS__bindgen_ty_1 {
    pub __bindgen_anon_1: _DXGK_VIDMMCAPS__bindgen_ty_1__bindgen_ty_1,
    pub Value: UINT,
}
#[repr(C)]
#[repr(align(4))]
#[derive(Debug, Default, Copy, Clone)]
pub struct _DXGK_VIDMMCAPS__bindgen_ty_1__bindgen_ty_1 {
    pub _bitfield_align_1: [u16; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}
```

Full flags bit layout (from `impl` block :49576-50493). Accessor pairs are `pub fn NAME(&self) -> UINT` / `pub fn set_NAME(&mut self, val: UINT)` (+ `unsafe NAME_raw`/`set_NAME_raw`):

| bit offset | width | field | accessor pair |
|---|---|---|---|
| 0 | 1 | `OutOfOrderLock` | `OutOfOrderLock()` / `set_OutOfOrderLock()` |
| 1 | 1 | `DedicatedPagingEngine` | `DedicatedPagingEngine()` / `set_DedicatedPagingEngine()` |
| 2 | 1 | `PagingEngineCanSwizzle` | `PagingEngineCanSwizzle()` / `set_…()` |
| 3 | 1 | `SectionBackedPrimary` | `SectionBackedPrimary()` / `set_…()` |
| 4 | 1 | `CrossAdapterResource` | `CrossAdapterResource()` / `set_…()` |
| 5 | 1 | `VirtualAddressingSupported` | `VirtualAddressingSupported()` / `set_…()` |
| 6 | 1 | `GpuMmuSupported` | `GpuMmuSupported()` / `set_GpuMmuSupported()` |
| 7 | 1 | `IoMmuSupported` | `IoMmuSupported()` / `set_IoMmuSupported()` |
| 8 | 1 | `ReplicateGdiContent` | `ReplicateGdiContent()` / `set_…()` |
| 9 | 1 | `NonCpuVisiblePrimary` | `NonCpuVisiblePrimary()` / `set_…()` |
| 10 | 1 | `ParavirtualizationSupported` | `ParavirtualizationSupported()` / `set_…()` |
| 11 | 1 | `IoMmuSecureModeSupported` | `IoMmuSecureModeSupported()` / `set_…()` |
| 12 | 1 | `DisableSelfRefreshVRAMInS3` | `DisableSelfRefreshVRAMInS3()` / `set_…()` |
| 13 | 1 | `IoMmuSecureModeRequired` | `IoMmuSecureModeRequired()` / `set_…()` |
| 14 | 1 | `MapAperture2Supported` | `MapAperture2Supported()` / `set_…()` |
| 15 | 1 | `CrossAdapterResourceTexture` | `CrossAdapterResourceTexture()` / `set_…()` |
| 16 | 1 | `CrossAdapterResourceScanout` | `CrossAdapterResourceScanout()` / `set_…()` |
| 17 | 1 | `AlwaysPoweredVRAM` | `AlwaysPoweredVRAM()` / `set_…()` |
| 18 | 14 | `Reserved` | `Reserved()` / `set_Reserved()` |

Constructor helper signature (verbatim, :50262-50281):
```rust
pub fn new_bitfield_1(
    OutOfOrderLock: UINT,
    DedicatedPagingEngine: UINT,
    PagingEngineCanSwizzle: UINT,
    SectionBackedPrimary: UINT,
    CrossAdapterResource: UINT,
    VirtualAddressingSupported: UINT,
    GpuMmuSupported: UINT,
    IoMmuSupported: UINT,
    ReplicateGdiContent: UINT,
    NonCpuVisiblePrimary: UINT,
    ParavirtualizationSupported: UINT,
    IoMmuSecureModeSupported: UINT,
    DisableSelfRefreshVRAMInS3: UINT,
    IoMmuSecureModeRequired: UINT,
    MapAperture2Supported: UINT,
    CrossAdapterResourceTexture: UINT,
    CrossAdapterResourceScanout: UINT,
    AlwaysPoweredVRAM: UINT,
    Reserved: UINT,
) -> __BindgenBitfieldUnit<[u8; 4usize]> { /* ... */ }
```

Size assert (:50515-50524): `_DXGK_VIDMMCAPS` size **8**, align **4**, `PagingNode` @ **4**. Hand-written zeroing `impl Default` (:50525-50533); alias `pub type DXGK_VIDMMCAPS = _DXGK_VIDMMCAPS;` (:50534).

---

#### B1.3 `DXGK_GPUMMUCAPS` (struct `_DXGK_GPUMMUCAPS`) — bindgen :47944, size 24, align 4

Returned for `DXGKQAITYPE_GPUMMUCAPS` (enum value 13, see B1.5). For the fake-GpuMmu model this is where `VirtualAddressBitCount`, `PageTableLevelCount`, and `PageTableUpdateMode` (the decorative page-table shape) are declared. Two flags words: the leading anonymous flags union and the trailing `LegacyBehaviors` (`__bindgen_ty_2`).

```rust
// dxgk_bindings_dump.rs:47943-47963
#[repr(C)]
pub struct _DXGK_GPUMMUCAPS {
    pub __bindgen_anon_1: _DXGK_GPUMMUCAPS__bindgen_ty_1,
    pub PageTableUpdateMode: DXGK_PAGETABLEUPDATEMODE,
    pub VirtualAddressBitCount: UINT,
    pub LeafPageTableSizeFor64KPagesInBytes: UINT,
    pub PageTableLevelCount: UINT,
    pub LegacyBehaviors: _DXGK_GPUMMUCAPS__bindgen_ty_2,
}
#[repr(C)]
#[derive(Copy, Clone)]
pub union _DXGK_GPUMMUCAPS__bindgen_ty_1 {
    pub __bindgen_anon_1: _DXGK_GPUMMUCAPS__bindgen_ty_1__bindgen_ty_1,
    pub Value: UINT,
}
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct _DXGK_GPUMMUCAPS__bindgen_ty_1__bindgen_ty_1 {
    pub _bitfield_align_1: [u32; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}
```

`PageTableUpdateMode` is the enum `DXGK_PAGETABLEUPDATEMODE` (:47467-47473):
```rust
pub mod _DXGK_PAGETABLEUPDATEMODE {
    pub type Type = ::core::ffi::c_int;
    pub const DXGK_PAGETABLEUPDATE_CPU_VIRTUAL: Type = 0;
    pub const DXGK_PAGETABLEUPDATE_GPU_VIRTUAL: Type = 1;
    pub const DXGK_PAGETABLEUPDATE_GPU_PHYSICAL: Type = 2;
}
pub use self::_DXGK_PAGETABLEUPDATEMODE::Type as DXGK_PAGETABLEUPDATEMODE;
```

`__bindgen_anon_1` flags bit layout (impl :47973-48652). Accessor pairs `NAME()`/`set_NAME()` (+ `_raw` variants):

| bit offset | width | field |
|---|---|---|
| 0 | 1 | `ReadOnlyMemorySupported` |
| 1 | 1 | `NoExecuteMemorySupported` |
| 2 | 1 | `ZeroInPteSupported` |
| 3 | 1 | `ExplicitPageTableInvalidation` |
| 4 | 1 | `CacheCoherentMemorySupported` |
| 5 | 1 | `PageTableUpdateRequireAddressSpaceIdle` |
| 6 | 1 | `LargePageSupported` |
| 7 | 1 | `DualPteSupported` |
| 8 | 1 | `AllowNonAlignedLargePageAddress` |
| 9 | 1 | `SysMem64KBPageSupported` |
| 10 | 1 | `InvalidTlbEntriesNotCached` |
| 11 | 1 | `SysMemLargePageSupported` |
| 12 | 1 | `CachedPageTables` |
| 13 | 19 | `Reserved` |

`__bindgen_anon_1::new_bitfield_1` signature (verbatim, :48482-48497):
```rust
pub fn new_bitfield_1(
    ReadOnlyMemorySupported: UINT,
    NoExecuteMemorySupported: UINT,
    ZeroInPteSupported: UINT,
    ExplicitPageTableInvalidation: UINT,
    CacheCoherentMemorySupported: UINT,
    PageTableUpdateRequireAddressSpaceIdle: UINT,
    LargePageSupported: UINT,
    DualPteSupported: UINT,
    AllowNonAlignedLargePageAddress: UINT,
    SysMem64KBPageSupported: UINT,
    InvalidTlbEntriesNotCached: UINT,
    SysMemLargePageSupported: UINT,
    CachedPageTables: UINT,
    Reserved: UINT,
) -> __BindgenBitfieldUnit<[u8; 4usize]> { /* ... */ }
```

The trailing `LegacyBehaviors` word (`__bindgen_ty_2`, :48675-48791) — note it is a plain struct (not a union; there is **no** `Value` field on this one), so it is written only via accessors:
```rust
// dxgk_bindings_dump.rs:48675-48680
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct _DXGK_GPUMMUCAPS__bindgen_ty_2 {
    pub _bitfield_align_1: [u32; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}
```
| bit offset | width | field |
|---|---|---|
| 0 | 1 | `SourcePageTableVaInTransfer` |
| 1 | 31 | `Reserved` |

Size assert (:48792-48814): `_DXGK_GPUMMUCAPS` size **24**, align **4**; field offsets — `PageTableUpdateMode` @ **4**, `VirtualAddressBitCount` @ **8**, `LeafPageTableSizeFor64KPagesInBytes` @ **12**, `PageTableLevelCount` @ **16**, `LegacyBehaviors` @ **20**. Hand-written zeroing `impl Default` (:48815-48823); alias `pub type DXGK_GPUMMUCAPS = _DXGK_GPUMMUCAPS;` (:48824).

---

#### B1.4 `DXGK_SEGMENTFLAGS` (struct `_DXGK_SEGMENTFLAGS`) — bindgen :52177, size 4, align 4

The per-segment flags word embedded as `Flags` in `_DXGK_SEGMENTDESCRIPTOR4` (B1.4b). For the fake-VidMm aperture/host-visible segment, the load-bearing bits are `Aperture` (0), `CpuVisible` (2), `CacheCoherent` (4), and `SupportsCpuHostAperture` (13) / `SupportsCachedCpuHostAperture` (14).

```rust
// dxgk_bindings_dump.rs:52175-52192
#[repr(C)]
#[derive(Copy, Clone)]
pub struct _DXGK_SEGMENTFLAGS {
    pub __bindgen_anon_1: _DXGK_SEGMENTFLAGS__bindgen_ty_1,
}
#[repr(C)]
#[derive(Copy, Clone)]
pub union _DXGK_SEGMENTFLAGS__bindgen_ty_1 {
    pub __bindgen_anon_1: _DXGK_SEGMENTFLAGS__bindgen_ty_1__bindgen_ty_1,
    pub Value: UINT,
}
#[repr(C)]
#[repr(align(4))]
#[derive(Debug, Default, Copy, Clone)]
pub struct _DXGK_SEGMENTFLAGS__bindgen_ty_1__bindgen_ty_1 {
    pub _bitfield_align_1: [u16; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}
```

Full bit layout (impl :52203-53301). Accessor pairs `NAME()`/`set_NAME()` (+ `_raw`):

| bit offset | width | field |
|---|---|---|
| 0 | 1 | `Aperture` |
| 1 | 1 | `Agp` |
| 2 | 1 | `CpuVisible` |
| 3 | 1 | `UseBanking` |
| 4 | 1 | `CacheCoherent` |
| 5 | 1 | `PitchAlignment` |
| 6 | 1 | `PopulatedFromSystemMemory` |
| 7 | 1 | `PreservedDuringStandby` |
| 8 | 1 | `PreservedDuringHibernate` |
| 9 | 1 | `PartiallyPreservedDuringHibernate` |
| 10 | 1 | `DirectFlip` |
| 11 | 1 | `Use64KBPages` |
| 12 | 1 | `ReservedSysMem` |
| 13 | 1 | `SupportsCpuHostAperture` |
| 14 | 1 | `SupportsCachedCpuHostAperture` |
| 15 | 1 | `ApplicationTarget` |
| 16 | 1 | `VprSupported` |
| 17 | 1 | `VprPreservedDuringStandby` |
| 18 | 1 | `EncryptedPagingSupported` |
| 19 | 1 | `LocalBudgetGroup` |
| 20 | 1 | `NonLocalBudgetGroup` |
| 21 | 1 | `PopulatedByReservedDDRByFirmware` |
| 22 | 10 | `Reserved` |

`new_bitfield_1` signature (verbatim, :53033-53056):
```rust
pub fn new_bitfield_1(
    Aperture: UINT,
    Agp: UINT,
    CpuVisible: UINT,
    UseBanking: UINT,
    CacheCoherent: UINT,
    PitchAlignment: UINT,
    PopulatedFromSystemMemory: UINT,
    PreservedDuringStandby: UINT,
    PreservedDuringHibernate: UINT,
    PartiallyPreservedDuringHibernate: UINT,
    DirectFlip: UINT,
    Use64KBPages: UINT,
    ReservedSysMem: UINT,
    SupportsCpuHostAperture: UINT,
    SupportsCachedCpuHostAperture: UINT,
    ApplicationTarget: UINT,
    VprSupported: UINT,
    VprPreservedDuringStandby: UINT,
    EncryptedPagingSupported: UINT,
    LocalBudgetGroup: UINT,
    NonLocalBudgetGroup: UINT,
    PopulatedByReservedDDRByFirmware: UINT,
    Reserved: UINT,
) -> __BindgenBitfieldUnit<[u8; 4usize]> { /* ... */ }
```

Size asserts: inner union (:53303-53313) — `_DXGK_SEGMENTFLAGS__bindgen_ty_1` size **4**, `Value` @ 0. Outer (:53324-53332) — `_DXGK_SEGMENTFLAGS` size **4**, align **4**. Hand-written zeroing `impl Default` for both (:53315-53322 and :53333-53341); alias `pub type DXGK_SEGMENTFLAGS = _DXGK_SEGMENTFLAGS;` (:53342). (Bindgen also emits a parallel `_DXGK_SEGMENTFLAGS2` at :53473 — distinct type, not used by `_DXGK_SEGMENTDESCRIPTOR4`, omitted here.)

---

#### B1.4b `DXGK_SEGMENTDESCRIPTOR4` (struct `_DXGK_SEGMENTDESCRIPTOR4`) — bindgen :53976, size 96, align 8

One descriptor per segment in the `DXGKQAITYPE_QUERYSEGMENT4` reply array. The address union (`__bindgen_anon_1`) selects between a plain `CpuTranslatedAddress` (`PHYSICAL_ADDRESS`/`LARGE_INTEGER`) and a `CpuHostAperture` struct — relevant when `Flags.SupportsCpuHostAperture` is set (the host-visible-BAR path).

```rust
// dxgk_bindings_dump.rs:53975-53990
#[repr(C)]
pub struct _DXGK_SEGMENTDESCRIPTOR4 {
    pub Flags: DXGK_SEGMENTFLAGS,
    pub BaseAddress: PHYSICAL_ADDRESS,
    pub Size: SIZE_T,
    pub CommitLimit: SIZE_T,
    pub SystemMemoryEndAddress: SIZE_T,
    pub __bindgen_anon_1: _DXGK_SEGMENTDESCRIPTOR4__bindgen_ty_1,
    pub NumInvalidMemoryRanges: UINT,
    pub VprRangeStartOffset: SIZE_T,
    pub VprRangeSize: SIZE_T,
    pub VprAlignment: UINT,
    pub NumVprSupported: UINT,
    pub VprReserveSize: UINT,
    pub NumUEFIFrameBufferRanges: UINT,
}
```

The address union — note this is the `__BindgenUnionField` pattern (a `#[repr(C)]` *struct* with a real `bindgen_union_field: [u64; 2usize]` backing 16 bytes); the two named fields are zero-sized placeholders accessed `unsafe`-ly via `.as_ref()`/`.as_mut()`:
```rust
// dxgk_bindings_dump.rs:53991-53996
#[repr(C)]
pub struct _DXGK_SEGMENTDESCRIPTOR4__bindgen_ty_1 {
    pub CpuTranslatedAddress: __BindgenUnionField<PHYSICAL_ADDRESS>,
    pub CpuHostAperture: __BindgenUnionField<DXGK_CPUHOSTAPERTURE>,
    pub bindgen_union_field: [u64; 2usize],
}
```
where `DXGK_CPUHOSTAPERTURE` (:49449-49468, size 16, align 8) is:
```rust
pub struct _DXGK_CPUHOSTAPERTURE {
    pub PhysicalAddress: UINT64,
    pub SizeInPages: UINT32,
}
pub type DXGK_CPUHOSTAPERTURE = _DXGK_CPUHOSTAPERTURE;
```

Size asserts (:54024-54071): `_DXGK_SEGMENTDESCRIPTOR4` size **96**, align **8**; field offsets — `Flags` @ **0**, `BaseAddress` @ **8**, `Size` @ **16**, `CommitLimit` @ **24**, `SystemMemoryEndAddress` @ **32**, (`__bindgen_anon_1` @ 40, 16 bytes), `NumInvalidMemoryRanges` @ **56**, `VprRangeStartOffset` @ **64**, `VprRangeSize` @ **72**, `VprAlignment` @ **80**, `NumVprSupported` @ **84**, `VprReserveSize` @ **88**, `NumUEFIFrameBufferRanges` @ **92**. The address-union assert (:53997-54014): size **16**, align **8**, both union members @ offset 0. Hand-written zeroing `impl Default` (:54072-54080 for the descriptor, :54015-54023 for the union); alias `pub type DXGK_SEGMENTDESCRIPTOR4 = _DXGK_SEGMENTDESCRIPTOR4;` (:54081).

---

#### B1.5 Query-segment out/in structs + `DXGKARG_QUERYADAPTERINFO` + the `DXGK_QUERYADAPTERINFOTYPE` enum

`DXGK_QUERYSEGMENTOUT4` (struct `_DXGK_QUERYSEGMENTOUT4`) — bindgen :54084, size 40, align 8. This is the v4 segment-enumeration output. Note `pSegmentDescriptor` is typed `*mut BYTE` (not `*mut DXGK_SEGMENTDESCRIPTOR4`) and the array is walked using the explicit `SegmentDescriptorStride: SIZE_T` — Step 2 must stride by `SegmentDescriptorStride`, not `size_of::<DXGK_SEGMENTDESCRIPTOR4>()`:
```rust
// dxgk_bindings_dump.rs:54082-54091
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct _DXGK_QUERYSEGMENTOUT4 {
    pub NbSegment: UINT,
    pub pSegmentDescriptor: *mut BYTE,
    pub PagingBufferSegmentId: UINT,
    pub PagingBufferSize: UINT,
    pub PagingBufferPrivateDataSize: UINT,
    pub SegmentDescriptorStride: SIZE_T,
}
pub type DXGK_QUERYSEGMENTOUT4 = _DXGK_QUERYSEGMENTOUT4;   // :54130
```
Offsets (:54092-54119): `NbSegment` @ 0, `pSegmentDescriptor` @ 8, `PagingBufferSegmentId` @ 16, `PagingBufferSize` @ 20, `PagingBufferPrivateDataSize` @ 24, `SegmentDescriptorStride` @ 32. Zeroing `impl Default` (:54121-54129).

`DXGK_QUERYSEGMENTOUT3` (struct `_DXGK_QUERYSEGMENTOUT3`) — bindgen :53915, size 32, align 8. The v3 form; here `pSegmentDescriptor` is a typed `*mut DXGK_SEGMENTDESCRIPTOR3` and there is **no** stride field (fixed-size array):
```rust
// dxgk_bindings_dump.rs:53913-53921
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct _DXGK_QUERYSEGMENTOUT3 {
    pub NbSegment: UINT,
    pub pSegmentDescriptor: *mut DXGK_SEGMENTDESCRIPTOR3,
    pub PagingBufferSegmentId: UINT,
    pub PagingBufferSize: UINT,
    pub PagingBufferPrivateDataSize: UINT,
}
pub type DXGK_QUERYSEGMENTOUT3 = _DXGK_QUERYSEGMENTOUT3;   // :53956
```
Offsets (:53922-53945): `NbSegment` @ 0, `pSegmentDescriptor` @ 8, `PagingBufferSegmentId` @ 16, `PagingBufferSize` @ 20, `PagingBufferPrivateDataSize` @ 24. Zeroing `impl Default` (:53947-53954).

`DXGK_QUERYSEGMENTIN4` (struct `_DXGK_QUERYSEGMENTIN4`) — bindgen :53959, size 4, align 4. The v4 input (only a physical-adapter index):
```rust
// dxgk_bindings_dump.rs:53957-53961
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct _DXGK_QUERYSEGMENTIN4 {
    pub PhysicalAdapterIndex: UINT,
}
pub type DXGK_QUERYSEGMENTIN4 = _DXGK_QUERYSEGMENTIN4;   // :53974
```

**`DXGK_QUERYSEGMENTIN3` does NOT exist in this dump.** A grep for `_DXGK_QUERYSEGMENTIN3` / `DXGK_QUERYSEGMENTIN3` returned no matches. The only non-v4 input struct is the unversioned `_DXGK_QUERYSEGMENTIN` (struct `_DXGK_QUERYSEGMENTIN`) — bindgen :53394, size 24, align 8 — used by the original (v1/v2) `DXGKQAITYPE_QUERYSEGMENT`/`QUERYSEGMENT2` path:
```rust
// dxgk_bindings_dump.rs:53393-53398
#[repr(C)]
pub struct _DXGK_QUERYSEGMENTIN {
    pub AgpApertureBase: PHYSICAL_ADDRESS,
    pub AgpApertureSize: LARGE_INTEGER,
    pub AgpFlags: DXGK_SEGMENTFLAGS,
}
pub type DXGK_QUERYSEGMENTIN = _DXGK_QUERYSEGMENTIN;   // :53426
```

`DXGKARG_QUERYADAPTERINFO` (struct `_DXGK_ARG...` — actual name `_DXGKARG_QUERYADAPTERINFO`) — bindgen :59497, size 48, align 8. This is the single argument passed to `DxgkDdiQueryAdapterInfo`; Step 2 dispatches on `Type`, reads `pInputData`/`InputDataSize`, and fills `pOutputData` (size `OutputDataSize`):
```rust
// dxgk_bindings_dump.rs:59495-59505
#[repr(C)]
#[derive(Copy, Clone)]
pub struct _DXGKARG_QUERYADAPTERINFO {
    pub Type: DXGK_QUERYADAPTERINFOTYPE,
    pub pInputData: *mut ::core::ffi::c_void,
    pub InputDataSize: UINT,
    pub pOutputData: *mut ::core::ffi::c_void,
    pub OutputDataSize: UINT,
    pub Flags: DXGK_QUERYADAPTERINFOFLAGS,
    pub hKmdProcessHandle: HANDLE,
}
pub type DXGKARG_QUERYADAPTERINFO = _DXGKARG_QUERYADAPTERINFO;   // :59545
pub type IN_CONST_PDXGKARG_QUERYADAPTERINFO = *const DXGKARG_QUERYADAPTERINFO;  // :59546
```
Offsets (:59507-59534): `Type` @ 0, `pInputData` @ 8, `InputDataSize` @ 16, `pOutputData` @ 24, `OutputDataSize` @ 32, `Flags` @ 36, `hKmdProcessHandle` @ 40. Zeroing `impl Default` (:59536-59544).

`DXGK_QUERYADAPTERINFOTYPE` enum (bindgen module `_DXGK_QUERYADAPTERINFOTYPE`, :44511-44561; the `Type` is `::core::ffi::c_int`). Reproduced **verbatim and complete** — the values Step 2 dispatches on (`DXGKQAITYPE_DRIVERCAPS=1`, `DXGKQAITYPE_QUERYSEGMENT4=11`, `DXGKQAITYPE_GPUMMUCAPS=13` are the load-bearing ones for the fake-VidMm model):
```rust
// dxgk_bindings_dump.rs:44511-44562
pub mod _DXGK_QUERYADAPTERINFOTYPE {
    pub type Type = ::core::ffi::c_int;
    pub const DXGKQAITYPE_UMDRIVERPRIVATE: Type = 0;
    pub const DXGKQAITYPE_DRIVERCAPS: Type = 1;
    pub const DXGKQAITYPE_QUERYSEGMENT: Type = 2;
    pub const DXGKQAITYPE_RESERVED: Type = 3;
    pub const DXGKQAITYPE_QUERYSEGMENT2: Type = 4;
    pub const DXGKQAITYPE_QUERYSEGMENT3: Type = 5;
    pub const DXGKQAITYPE_NUMPOWERCOMPONENTS: Type = 6;
    pub const DXGKQAITYPE_POWERCOMPONENTINFO: Type = 7;
    pub const DXGKQAITYPE_PREFERREDGPUNODE: Type = 8;
    pub const DXGKQAITYPE_POWERCOMPONENTPSTATEINFO: Type = 9;
    pub const DXGKQAITYPE_HISTORYBUFFERPRECISION: Type = 10;
    pub const DXGKQAITYPE_QUERYSEGMENT4: Type = 11;
    pub const DXGKQAITYPE_SEGMENTMEMORYSTATE: Type = 12;
    pub const DXGKQAITYPE_GPUMMUCAPS: Type = 13;
    pub const DXGKQAITYPE_PAGETABLELEVELDESC: Type = 14;
    pub const DXGKQAITYPE_PHYSICALADAPTERCAPS: Type = 15;
    pub const DXGKQAITYPE_DISPLAY_DRIVERCAPS_EXTENSION: Type = 16;
    pub const DXGKQAITYPE_INTEGRATED_DISPLAY_DESCRIPTOR: Type = 17;
    pub const DXGKQAITYPE_UEFIFRAMEBUFFERRANGES: Type = 18;
    pub const DXGKQAITYPE_QUERYCOLORIMETRYOVERRIDES: Type = 19;
    pub const DXGKQAITYPE_DISPLAYID_DESCRIPTOR: Type = 20;
    pub const DXGKQAITYPE_FRAMEBUFFERSAVESIZE: Type = 21;
    pub const DXGKQAITYPE_HARDWARERESERVEDRANGES: Type = 22;
    pub const DXGKQAITYPE_INTEGRATED_DISPLAY_DESCRIPTOR2: Type = 23;
    pub const DXGKQAITYPE_NODEPERFDATA: Type = 24;
    pub const DXGKQAITYPE_ADAPTERPERFDATA: Type = 25;
    pub const DXGKQAITYPE_ADAPTERPERFDATA_CAPS: Type = 26;
    pub const DXGKQAITYPE_GPUVERSION: Type = 27;
    pub const DXGKQAITYPE_DEVICE_TYPE_CAPS: Type = 28;
    pub const DXGKQAITYPE_WDDMDEVICECAPS: Type = 29;
    pub const DXGKQAITYPE_GPUPCAPS: Type = 30;
    pub const DXGKQAITYPE_QUERYTARGETGAMMACAPS: Type = 31;
    pub const DXGKQAITYPE_SCANOUT_CAPS: Type = 33;
    pub const DXGKQAITYPE_PHYSICAL_MEMORY_CAPS: Type = 34;
    pub const DXGKQAITYPE_IOMMU_CAPS: Type = 35;
    pub const DXGKQAITYPE_HARDWARERESERVEDRANGES2: Type = 36;
    pub const DXGKQAITYPE_NATIVE_FENCE_CAPS: Type = 37;
    pub const DXGKQAITYPE_USERMODESUBMISSION_CAPS: Type = 38;
    pub const DXGKQAITYPE_DIRTYBITTRACKINGCAPS: Type = 39;
    pub const DXGKQAITYPE_DIRTYBITTRACKINGSEGMENTCAPS: Type = 40;
    pub const DXGKQAITYPE_SCATTER_RESERVE: Type = 41;
    pub const DXGKQAITYPE_QUERYPAGINGBUFFERINFO: Type = 42;
    pub const DXGKQAITYPE_QUERYSEGMENTCOUNT: Type = 43;
    pub const DXGKQAITYPE_QUERYSEGMENT5: Type = 44;
    pub const DXGKQAITYPE_QUERYMMUCOUNT: Type = 45;
    pub const DXGKQAITYPE_QUERYMMUS: Type = 46;
    pub const DXGKQAITYPE_64BITONLYCAPS: Type = 47;
    pub const DXGKQAITYPE_PAGINGPROCESSGPUVASIZE: Type = 48;
}
pub use self::_DXGK_QUERYADAPTERINFOTYPE::Type as DXGK_QUERYADAPTERINFOTYPE;
```
(Note value **32** is absent — there is a gap between `DXGKQAITYPE_QUERYTARGETGAMMACAPS=31` and `DXGKQAITYPE_SCANOUT_CAPS=33`; this is verbatim from the dump, not an omission.)

---

**Cross-references / notes for Step 2:**
- `_DXGK_DRIVERCAPS`, `_DXGK_SEGMENTFLAGS`, `_DXGK_SEGMENTDESCRIPTOR4`, `_DXGK_QUERYSEGMENTIN`, and `_DXGKARG_QUERYADAPTERINFO` are **not** `#[derive(Default)]`; each instead has a hand-written `impl Default` that zero-writes via `MaybeUninit` + `write_bytes(.., 0, 1)`. The flags *unions* (`__bindgen_ty_*`) and the inner `__bindgen_ty_*__bindgen_ty_1` bitfield structs do have `Default` (the inner structs `#[derive(Debug, Default, Copy, Clone)]`, the unions a hand-written zeroing `Default`).
- To set a caps flags word, either write the union's `Value: UINT` directly (e.g. `caps.MemoryManagementCaps.__bindgen_anon_1.Value = …`) or, more readably, call `set_GpuMmuSupported(1)` etc. on `caps.MemoryManagementCaps.__bindgen_anon_1.__bindgen_anon_1`. `_DXGK_GPUMMUCAPS::LegacyBehaviors` (`__bindgen_ty_2`) has **no** `Value` field, so only the `set_*` accessors apply there.
- `DXGK_QUERYSEGMENTOUT4.pSegmentDescriptor` is `*mut BYTE` and the array must be strided by `SegmentDescriptorStride` (the OS supplies the buffer/stride), distinct from `OUT3` which uses a typed `*mut DXGK_SEGMENTDESCRIPTOR3`.

### B2. Verbatim Rust types — allocations, paging buffer, page tables

All quotes below are verbatim from `/home/rupansh/helios-vgpu/dxgk_bindings_dump.rs` (bindgen of WDK 10.0.26100 `d3dkmddi.h` + `dispmprt.h`, `DXGKDDI_INTERFACE_VERSION` = WDDM 3.2). bindgen names the C struct `_DXGK_FOO` and emits `pub type DXGK_FOO = _DXGK_FOO;`. Anonymous unions become `__bindgen_anon_N` fields of synthetic `_..._bindgen_ty_N` types; flag bitfields are stored in a `__BindgenBitfieldUnit<[u8; N]>` accessed via generated `get/set/raw` methods. Step 2 **must** use these exact accessor names and bit positions — they are the authoritative source (the conceptual `.md` docs under `windows-driver-docs-pr/display/` contain no per-DDI struct reference).

---

#### B2.1 `DXGK_ALLOCATIONINFOFLAGS` — the per-allocation flags bitfield (the CpuVisible/Cached lever)

This is the flag word Helios writes per allocation in `DxgkDdiCreateAllocation`. The struct is a union of a `Value: UINT` and a 32×1-bit bitfield. **`CpuVisible` is bit 0, `Cached` is bit 2** — these are the two levers most relevant to the host-visible-blob coherence model.

Outer struct + union (dxgk_bindings_dump.rs:59633-59661):
```rust
#[repr(C)]
#[derive(Copy, Clone)]
pub struct _DXGK_ALLOCATIONINFOFLAGS {
    pub __bindgen_anon_1: _DXGK_ALLOCATIONINFOFLAGS__bindgen_ty_1,
}
#[repr(C)]
#[derive(Copy, Clone)]
pub union _DXGK_ALLOCATIONINFOFLAGS__bindgen_ty_1 {
    pub __bindgen_anon_1: _DXGK_ALLOCATIONINFOFLAGS__bindgen_ty_1__bindgen_ty_1,
    pub Value: UINT,
}
#[repr(C)]
#[repr(align(4))]
#[derive(Debug, Default, Copy, Clone)]
pub struct _DXGK_ALLOCATIONINFOFLAGS__bindgen_ty_1__bindgen_ty_1 {
    pub _bitfield_align_1: [u8; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}
```
Size assertions confirm the whole thing is 4 bytes (dxgk_bindings_dump.rs:59654-59660, 61193-61198).

Representative accessor (the `CpuVisible` getter/setter at bit 0 — pattern repeats for every flag, dxgk_bindings_dump.rs:59663-59698):
```rust
impl _DXGK_ALLOCATIONINFOFLAGS__bindgen_ty_1__bindgen_ty_1 {
    #[inline]
    pub fn CpuVisible(&self) -> UINT {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(0usize, 1u8) as u32) }
    }
    #[inline]
    pub fn set_CpuVisible(&mut self, val: UINT) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(0usize, 1u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn CpuVisible_raw(this: *const Self) -> UINT { /* ... raw_get(.., 0usize, 1u8) .. */ }
    #[inline]
    pub unsafe fn set_CpuVisible_raw(this: *mut Self, val: UINT) { /* ... raw_set(.., 0usize, 1u8, ..) .. */ }
```

**Complete bit map** (each is `_bitfield_1.get(POS, 1u8)` — verified verbatim by reading every accessor at dxgk_bindings_dump.rs:59663-60814 and the `new_bitfield_1` constructor argument list at 59816-61168). Accessor name → bit position → emitting line of the getter:

| Accessor (`fn NAME` / `set_NAME`) | Bit pos | Getter line |
|---|---|---|
| `CpuVisible` | 0 | 59664 |
| `PermanentSysMem` | 1 | 59700 |
| `Cached` | 2 | 59736 |
| `Protected` | 3 | 59772 |
| `ExistingSysMem` | 4 | 59808 |
| `ExistingKernelSysMem` | 5 | 59844 |
| `FromEndOfSegment` | 6 | 59880 |
| `Swizzled` | 7 | 59916 |
| `Overlay` | 8 | 59952 |
| `Capture` | 9 | 59988 |
| `UseAlternateVA` | 10 | 60024 |
| `SynchronousPaging` | 11 | 60060 |
| `LinkMirrored` | 12 | 60096 |
| `LinkInstanced` | 13 | 60132 |
| `HistoryBuffer` | 14 | 60168 |
| `AccessedPhysically` | 15 | 60204 |
| `ExplicitResidencyNotification` | 16 | 60240 |
| `HardwareProtected` | 17 | 60276 |
| `CpuVisibleOnDemand` | 18 | 60312 |
| `DXGK_ALLOC_RESERVED16` | 19 | 60348 |
| `Reserved15` | 20 | 60384 |
| `Reserved14` | 21 | 60420 |
| `Reserved13` | 22 | 60456 |
| `Reserved12` | 23 | 60492 |
| `Reserved11` | 24 | 60528 |
| `Reserved10` | 25 | 60564 |
| `Reserved9` | 26 | 60600 |
| `Reserved4` | 27 | 60636 |
| `Reserved3` | 28 | 60672 |
| `Reserved2` | 29 | 60708 |
| `Reserved1` | 30 | 60744 |
| `Reserved0` | 31 | 60780 |

The constructor signature (verbatim, dxgk_bindings_dump.rs:60816-60849) lists all 32 in order:
```rust
    pub fn new_bitfield_1(
        CpuVisible: UINT, PermanentSysMem: UINT, Cached: UINT, Protected: UINT,
        ExistingSysMem: UINT, ExistingKernelSysMem: UINT, FromEndOfSegment: UINT,
        Swizzled: UINT, Overlay: UINT, Capture: UINT, UseAlternateVA: UINT,
        SynchronousPaging: UINT, LinkMirrored: UINT, LinkInstanced: UINT,
        HistoryBuffer: UINT, AccessedPhysically: UINT, ExplicitResidencyNotification: UINT,
        HardwareProtected: UINT, CpuVisibleOnDemand: UINT, DXGK_ALLOC_RESERVED16: UINT,
        Reserved15: UINT, Reserved14: UINT, Reserved13: UINT, Reserved12: UINT,
        Reserved11: UINT, Reserved10: UINT, Reserved9: UINT, Reserved4: UINT,
        Reserved3: UINT, Reserved2: UINT, Reserved1: UINT, Reserved0: UINT,
    ) -> __BindgenBitfieldUnit<[u8; 4usize]> { ... }
```
Alias (dxgk_bindings_dump.rs:61209): `pub type DXGK_ALLOCATIONINFOFLAGS = _DXGK_ALLOCATIONINFOFLAGS;`

---

#### B2.2 `DXGK_ALLOCATIONINFOFLAGS_WDDM2_0` — the WDDM2+ variant of the same word

`DXGK_ALLOCATIONINFO::Flags` is a union of `Flags: DXGK_ALLOCATIONINFOFLAGS` and `FlagsWddm2: DXGK_ALLOCATIONINFOFLAGS_WDDM2_0` (see B2.3). The WDDM2_0 layout **differs** at bits 7, 10, 11, 12, 13 from the legacy word. Head + union (dxgk_bindings_dump.rs:61211-61224):
```rust
pub struct _DXGK_ALLOCATIONINFOFLAGS_WDDM2_0 {
    pub __bindgen_anon_1: _DXGK_ALLOCATIONINFOFLAGS_WDDM2_0__bindgen_ty_1,
}
pub union _DXGK_ALLOCATIONINFOFLAGS_WDDM2_0__bindgen_ty_1 {
    pub __bindgen_anon_1: _DXGK_ALLOCATIONINFOFLAGS_WDDM2_0__bindgen_ty_1__bindgen_ty_1,
    pub Value: UINT,
}
```
Bit map (observed accessor names + positions, dxgk_bindings_dump.rs:61224-62791): `CpuVisible`=0, `PermanentSysMem`=1, `Cached`=2, `Protected`=3, `ExistingSysMem`=4, `ExistingKernelSysMem`=5, `FromEndOfSegment`=6, **`DisableLargePageMapping`=7** (legacy had `Swizzled` here), `Overlay`=8, `Capture`=9, **`CreateInVpr`=10** (legacy `UseAlternateVA`), **`DXGK_ALLOC_RESERVED17`=11** (legacy `SynchronousPaging`), **`Reserved02`=12** (legacy `LinkMirrored`), **`MapApertureCpuVisible`=13** (legacy `LinkInstanced`), `HistoryBuffer`=14, `AccessedPhysically`=15, `ExplicitResidencyNotification`=16, `HardwareProtected`=17, `CpuVisibleOnDemand`=18, `DXGK_ALLOC_RESERVED16`=19, then `Reserved15`..`Reserved0` at bits 20-31 (same tail as B2.1).
Alias (dxgk_bindings_dump.rs:62791): `pub type DXGK_ALLOCATIONINFOFLAGS_WDDM2_0 = _DXGK_ALLOCATIONINFOFLAGS_WDDM2_0;`

---

#### B2.3 `DXGK_ALLOCATIONINFOFLAGS2` — the secondary flags word (`DXGK_ALLOCATIONINFO::Flags2`)

Head + union + bitfield carrier (dxgk_bindings_dump.rs:62794-62808):
```rust
pub struct _DXGK_ALLOCATIONINFOFLAGS2 {
    pub __bindgen_anon_1: _DXGK_ALLOCATIONINFOFLAGS2__bindgen_ty_1,
}
pub union _DXGK_ALLOCATIONINFOFLAGS2__bindgen_ty_1 {
    pub __bindgen_anon_1: _DXGK_ALLOCATIONINFOFLAGS2__bindgen_ty_1__bindgen_ty_1,
    pub Value: UINT,
}
pub struct _DXGK_ALLOCATIONINFOFLAGS2__bindgen_ty_1__bindgen_ty_1 {
    pub _bitfield_align_1: [u32; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}
```
Bit map (verified verbatim — getter name : `get(POS, WIDTH)`, dxgk_bindings_dump.rs:62820-62920+):
```rust
ShareBackingStoreWithKmd   -> get(0usize, 1u8)
NoImplicitSynchronization  -> get(1usize, 1u8)
DisablePartialResidency    -> get(2usize, 1u8)
RestrictedToSingleSegment  -> get(3usize, 1u8)
NotifyEviction             -> get(4usize, 1u8)
NotifyIoMmuUnmap           -> get(5usize, 1u8)
Reserved                   -> get(6usize, 26u8)   // remaining 26 bits reserved
```
Alias (dxgk_bindings_dump.rs:63201): `pub type DXGK_ALLOCATIONINFOFLAGS2 = _DXGK_ALLOCATIONINFOFLAGS2;`
(Note: the `PrivateFormat/Swizzled/MipMap/Cube/Volume/Vertex/Index` accessors that grep returns near this range belong to a **different** struct — the WDDM2_0 `__bindgen_ty_1` sub-struct group — not to FLAGS2. FLAGS2's only live bits are the seven above.)

---

#### B2.4 `DXGK_ALLOCATIONINFO` (`struct _DXGK_ALLOCATIONINFO`) — the per-allocation descriptor Helios fills

This is the central struct: it carries `Size`, the segment-set masks (`SupportedReadSegmentSet`/`SupportedWriteSegmentSet`/`EvictionSegmentSet`), `PreferredSegment`, the `Flags`/`FlagsWddm2` union, and `Flags2`. **Note the bindgen union members**: `SupportedReadSegmentSet` is inside `__bindgen_anon_2` (aliased with `MmuSet`), and `Flags`/`FlagsWddm2` is inside `__bindgen_anon_4`. Verbatim (dxgk_bindings_dump.rs:63745-63762):
```rust
pub struct _DXGK_ALLOCATIONINFO {
    pub pPrivateDriverData: *mut ::core::ffi::c_void,
    pub PrivateDriverDataSize: UINT,
    pub __bindgen_anon_1: _DXGK_ALLOCATIONINFO__bindgen_ty_1,   // Alignment | {MinimumPageSize,RecommendedPageSize}
    pub Size: SIZE_T,
    pub PitchAlignedSize: SIZE_T,
    pub HintedBank: DXGK_SEGMENTBANKPREFERENCE,
    pub PreferredSegment: DXGK_SEGMENTPREFERENCE,
    pub __bindgen_anon_2: _DXGK_ALLOCATIONINFO__bindgen_ty_2,   // SupportedReadSegmentSet | MmuSet
    pub SupportedWriteSegmentSet: UINT,
    pub EvictionSegmentSet: UINT,
    pub __bindgen_anon_3: _DXGK_ALLOCATIONINFO__bindgen_ty_3,   // MaximumRenamingListLength | PhysicalAdapterIndex
    pub hAllocation: HANDLE,
    pub __bindgen_anon_4: _DXGK_ALLOCATIONINFO__bindgen_ty_4,   // Flags | FlagsWddm2
    pub pAllocationUsageHint: *mut DXGK_ALLOCATIONUSAGEHINT,
    pub AllocationPriority: UINT,
    pub Flags2: DXGK_ALLOCATIONINFOFLAGS2,
}
```
The four anonymous unions (verbatim, dxgk_bindings_dump.rs:63763-63911):
```rust
pub union _DXGK_ALLOCATIONINFO__bindgen_ty_1 {
    pub Alignment: UINT,
    pub __bindgen_anon_1: _DXGK_ALLOCATIONINFO__bindgen_ty_1__bindgen_ty_1,
}
pub struct _DXGK_ALLOCATIONINFO__bindgen_ty_1__bindgen_ty_1 {
    pub MinimumPageSize: UINT16,
    pub RecommendedPageSize: UINT16,
}
pub union _DXGK_ALLOCATIONINFO__bindgen_ty_2 {
    pub SupportedReadSegmentSet: UINT,
    pub MmuSet: UINT,
}
pub union _DXGK_ALLOCATIONINFO__bindgen_ty_3 {
    pub MaximumRenamingListLength: UINT,
    pub PhysicalAdapterIndex: UINT,
}
pub union _DXGK_ALLOCATIONINFO__bindgen_ty_4 {
    pub Flags: DXGK_ALLOCATIONINFOFLAGS,
    pub FlagsWddm2: DXGK_ALLOCATIONINFOFLAGS_WDDM2_0,
}
```
Layout asserts (dxgk_bindings_dump.rs:63914-63955): total **88 bytes**, align 8; key field offsets — `Size`=16, `PitchAlignedSize`=24, `HintedBank`=32, `PreferredSegment`=36, `SupportedWriteSegmentSet`=44, `EvictionSegmentSet`=48, `hAllocation`=56, `AllocationPriority`=80, `Flags2`=84. Alias (dxgk_bindings_dump.rs:63966): `pub type DXGK_ALLOCATIONINFO = _DXGK_ALLOCATIONINFO;`

(There is also a layout-test mirror `_DXGK_ALLOCATIONINFO_TEST` at dxgk_bindings_dump.rs:63969-64040 that flattens these unions into plain named fields — `SupportedReadSegmentSet`, `PhysicalAdapterIndex`, `FlagsWddm2` — useful as documentation of which union arm is canonical, but it is *not* the struct passed by dxgkrnl.)

---

#### B2.5 `DXGKARG_CREATEALLOCATION` (`struct _DXGKARG_CREATEALLOCATION`) + its flags

Verbatim (dxgk_bindings_dump.rs:64210-64219):
```rust
pub struct _DXGKARG_CREATEALLOCATION {
    pub pPrivateDriverData: *const ::core::ffi::c_void,
    pub PrivateDriverDataSize: UINT,
    pub NumAllocations: UINT,
    pub pAllocationInfo: *mut DXGK_ALLOCATIONINFO,
    pub hResource: HANDLE,
    pub Flags: DXGK_CREATEALLOCATIONFLAGS,
}
```
Layout (dxgk_bindings_dump.rs:64222-64246): **40 bytes**, align 8; offsets `pPrivateDriverData`=0, `PrivateDriverDataSize`=8, `NumAllocations`=12, `pAllocationInfo`=16, `hResource`=24, `Flags`=32. Aliases (dxgk_bindings_dump.rs:64257-64258):
```rust
pub type DXGKARG_CREATEALLOCATION = _DXGKARG_CREATEALLOCATION;
pub type INOUT_PDXGKARG_CREATEALLOCATION = *mut DXGKARG_CREATEALLOCATION;
```
`DXGK_CREATEALLOCATIONFLAGS` (dxgk_bindings_dump.rs:64041-64209) is a union of `Value: UINT` and a bitfield with exactly two members: `Resource` at bit 0 and `Reserved` at bits 1-31:
```rust
pub union _DXGK_CREATEALLOCATIONFLAGS__bindgen_ty_1 {
    pub __bindgen_anon_1: _DXGK_CREATEALLOCATIONFLAGS__bindgen_ty_1__bindgen_ty_1,
    pub Value: UINT,
}
// accessors (dxgk_bindings_dump.rs:64069-64141):
//   Resource -> get(0usize, 1u8)
//   Reserved -> get(1usize, 31u8)
pub type DXGK_CREATEALLOCATIONFLAGS = _DXGK_CREATEALLOCATIONFLAGS;  // :64209
```

---

#### B2.6 `DXGKARG_DESCRIBEALLOCATION` (`struct _DXGKARG_DESCRIBEALLOCATION`) + its flags

Verbatim (dxgk_bindings_dump.rs:64750-64762):
```rust
pub struct _DXGKARG_DESCRIBEALLOCATION {
    pub hAllocation: HANDLE,
    pub Width: UINT,
    pub Height: UINT,
    pub Format: D3DDDIFORMAT,
    pub MultisampleMethod: D3DDDI_MULTISAMPLINGMETHOD,
    pub RefreshRate: D3DDDI_RATIONAL,
    pub PrivateDriverFormatAttribute: UINT,
    pub Flags: DXGK_DESCRIBEALLOCATIONFLAGS,
    pub Rotation: D3DDDI_ROTATION,
}
```
Layout (dxgk_bindings_dump.rs:64765-64798): **48 bytes**, align 8; offsets `hAllocation`=0, `Width`=8, `Height`=12, `Format`=16, `MultisampleMethod`=20, `RefreshRate`=28, `PrivateDriverFormatAttribute`=36, `Flags`=40, `Rotation`=44. Aliases (dxgk_bindings_dump.rs:64809-64810):
```rust
pub type DXGKARG_DESCRIBEALLOCATION = _DXGKARG_DESCRIBEALLOCATION;
pub type INOUT_PDXGKARG_DESCRIBEALLOCATION = *mut DXGKARG_DESCRIBEALLOCATION;
```
`DXGK_DESCRIBEALLOCATIONFLAGS` (dxgk_bindings_dump.rs:64578-64749) is a union of `Value: UINT` and a bitfield: `CheckDisplayMode` at bit 0, `Reserved` at bits 1-31 (accessors at dxgk_bindings_dump.rs:64607-64680; `CheckDisplayMode -> get(0usize, 1u8)`, `Reserved -> get(1usize, 31u8)`). Alias: `pub type DXGK_DESCRIBEALLOCATIONFLAGS = _DXGK_DESCRIBEALLOCATIONFLAGS;` (:64749).

---

#### B2.7 `DXGKARG_OPENALLOCATION` + `DXGK_OPENALLOCATIONINFO` + `DXGK_OPENALLOCATIONFLAGS`

`DXGK_OPENALLOCATIONINFO` (verbatim, dxgk_bindings_dump.rs:41670-41675):
```rust
pub struct _DXGK_OPENALLOCATIONINFO {
    pub hAllocation: D3DKMT_HANDLE,
    pub pPrivateDriverData: *mut ::core::ffi::c_void,
    pub PrivateDriverDataSize: UINT,
    pub hDeviceSpecificAllocation: HANDLE,
}
```
Layout (dxgk_bindings_dump.rs:41678-41697): **32 bytes**, align 8; offsets `hAllocation`=0, `pPrivateDriverData`=8, `PrivateDriverDataSize`=16, `hDeviceSpecificAllocation`=24. Alias `pub type DXGK_OPENALLOCATIONINFO = _DXGK_OPENALLOCATIONINFO;` (:41708).

`DXGKARG_OPENALLOCATION` (verbatim, dxgk_bindings_dump.rs:41925-41934):
```rust
pub struct _DXGKARG_OPENALLOCATION {
    pub NumAllocations: UINT,
    pub pOpenAllocation: *mut DXGK_OPENALLOCATIONINFO,
    pub pPrivateDriverData: *mut ::core::ffi::c_void,
    pub PrivateDriverSize: UINT,
    pub Flags: DXGK_OPENALLOCATIONFLAGS,
    pub SubresourceIndex: UINT,
    pub SubresourceOffset: SIZE_T,
    pub Pitch: UINT,
}
```
Layout (dxgk_bindings_dump.rs:41937-41966): **56 bytes**, align 8; offsets `NumAllocations`=0, `pOpenAllocation`=8, `pPrivateDriverData`=16, `PrivateDriverSize`=24, `Flags`=28, `SubresourceIndex`=32, `SubresourceOffset`=40, `Pitch`=48. Aliases (dxgk_bindings_dump.rs:41977-41978):
```rust
pub type DXGKARG_OPENALLOCATION = _DXGKARG_OPENALLOCATION;
pub type IN_CONST_PDXGKARG_OPENALLOCATION = *const DXGKARG_OPENALLOCATION;
```
`DXGK_OPENALLOCATIONFLAGS` (dxgk_bindings_dump.rs:41711-…) is a union of `Value: UINT` and a bitfield; the first flag is `Create` at bit 0 (accessor at dxgk_bindings_dump.rs:41737-41759: `Create -> get(0usize, 1u8)`).

---

#### B2.8 `DXGKARG_GETSTANDARDALLOCATIONDRIVERDATA` + `D3DKMDT_STANDARDALLOCATION_TYPE` + per-type surface-data structs

The standard-allocation type enum bindgen emits it as `D3DKMDT_STANDARDALLOCATION_TYPE` (note the underscore — the prompt's `D3DKMDT_STANDARDALLOCATIONTYPE` without the second underscore does **not** exist; the actual name is `_D3DKMDT_STANDARDALLOCATION_TYPE`). Verbatim variants (dxgk_bindings_dump.rs:27867-27876):
```rust
pub mod _D3DKMDT_STANDARDALLOCATION_TYPE {
    pub type Type = ::core::ffi::c_int;
    pub const D3DKMDT_STANDARDALLOCATION_SHAREDPRIMARYSURFACE: Type = 1;
    pub const D3DKMDT_STANDARDALLOCATION_SHADOWSURFACE: Type = 2;
    pub const D3DKMDT_STANDARDALLOCATION_STAGINGSURFACE: Type = 3;
    pub const D3DKMDT_STANDARDALLOCATION_GDISURFACE: Type = 4;
    pub const D3DKMDT_STANDARDALLOCATION_VGPU: Type = 5;
    pub const D3DKMDT_STANDARDALLOCATION_FENCESTORAGE: Type = 6;
}
pub use self::_D3DKMDT_STANDARDALLOCATION_TYPE::Type as D3DKMDT_STANDARDALLOCATION_TYPE;
```

`DXGKARG_GETSTANDARDALLOCATIONDRIVERDATA` (verbatim, dxgk_bindings_dump.rs:64892-64910):
```rust
pub struct _DXGKARG_GETSTANDARDALLOCATIONDRIVERDATA {
    pub StandardAllocationType: D3DKMDT_STANDARDALLOCATION_TYPE,
    pub __bindgen_anon_1: _DXGKARG_GETSTANDARDALLOCATIONDRIVERDATA__bindgen_ty_1,
    pub pAllocationPrivateDriverData: *mut ::core::ffi::c_void,
    pub AllocationPrivateDriverDataSize: UINT,
    pub pResourcePrivateDriverData: *mut ::core::ffi::c_void,
    pub ResourcePrivateDriverDataSize: UINT,
    pub PhysicalAdapterIndex: UINT,
}
pub union _DXGKARG_GETSTANDARDALLOCATIONDRIVERDATA__bindgen_ty_1 {
    pub pCreateSharedPrimarySurfaceData: *mut D3DKMDT_SHAREDPRIMARYSURFACEDATA,
    pub pCreateShadowSurfaceData: *mut D3DKMDT_SHADOWSURFACEDATA,
    pub pCreateStagingSurfaceData: *mut D3DKMDT_STAGINGSURFACEDATA,
    pub pCreateGdiSurfaceData: *mut D3DKMDT_GDISURFACEDATA,
    pub pCreateVirtualGpuSurfaceData: *mut D3DKMDT_VIRTUALGPUSURFACEDATA,
    pub pCreateFenceStorageData: *mut D3DKMDT_FENCESTORAGESURFACEDATA,
}
```
Layout (dxgk_bindings_dump.rs:64964-64993): **48 bytes**, align 8; offsets `StandardAllocationType`=0, union `__bindgen_anon_1`=8, `pAllocationPrivateDriverData`=16, `AllocationPrivateDriverDataSize`=24, `pResourcePrivateDriverData`=32, `ResourcePrivateDriverDataSize`(40), `PhysicalAdapterIndex`(44).

The per-type data structs the union arms point at (verbatim):

`D3DKMDT_SHAREDPRIMARYSURFACEDATA` (dxgk_bindings_dump.rs:28539-28545, 24 bytes):
```rust
pub struct _D3DKMDT_SHAREDPRIMARYSURFACEDATA {
    pub Width: UINT,
    pub Height: UINT,
    pub Format: D3DDDIFORMAT,
    pub RefreshRate: D3DDDI_RATIONAL,
    pub VidPnSourceId: D3DDDI_VIDEO_PRESENT_SOURCE_ID,
}
```
`D3DKMDT_SHADOWSURFACEDATA` (dxgk_bindings_dump.rs:28583-28588, 16 bytes):
```rust
pub struct _D3DKMDT_SHADOWSURFACEDATA {
    pub Width: UINT,
    pub Height: UINT,
    pub Format: D3DDDIFORMAT,
    pub Pitch: UINT,
}
```
`D3DKMDT_STAGINGSURFACEDATA` (dxgk_bindings_dump.rs:28622-28626, 12 bytes):
```rust
pub struct _D3DKMDT_STAGINGSURFACEDATA {
    pub Width: UINT,
    pub Height: UINT,
    pub Pitch: UINT,
}
```
`D3DKMDT_GDISURFACEDATA` (dxgk_bindings_dump.rs:28781-28788, 24 bytes):
```rust
pub struct _D3DKMDT_GDISURFACEDATA {
    pub Width: UINT,
    pub Height: UINT,
    pub Format: D3DDDIFORMAT,
    pub Type: D3DKMDT_GDISURFACETYPE,
    pub Flags: D3DKMDT_GDISURFACEFLAGS,
    pub Pitch: UINT,
}
```
Its `Type` enum `D3DKMDT_GDISURFACETYPE` (dxgk_bindings_dump.rs:28766-28778) — relevant because `..._CPUVISIBLE` and `..._CROSSADAPTER` variants are exactly the shapes a render-only + IDD-readback path may be asked for:
```rust
pub mod _D3DKMDT_GDISURFACETYPE {
    pub type Type = ::core::ffi::c_int;
    pub const D3DKMDT_GDISURFACE_INVALID: Type = 0;
    pub const D3DKMDT_GDISURFACE_TEXTURE: Type = 1;
    pub const D3DKMDT_GDISURFACE_STAGING_CPUVISIBLE: Type = 2;
    pub const D3DKMDT_GDISURFACE_STAGING: Type = 3;
    pub const D3DKMDT_GDISURFACE_LOOKUPTABLE: Type = 4;
    pub const D3DKMDT_GDISURFACE_EXISTINGSYSMEM: Type = 5;
    pub const D3DKMDT_GDISURFACE_TEXTURE_CPUVISIBLE: Type = 6;
    pub const D3DKMDT_GDISURFACE_TEXTURE_CROSSADAPTER: Type = 7;
    pub const D3DKMDT_GDISURFACE_TEXTURE_CPUVISIBLE_CROSSADAPTER: Type = 8;
}
```
`D3DKMDT_GDISURFACEFLAGS` (dxgk_bindings_dump.rs:28646-28765) is a union of `Value: UINT` and a single `Reserved` bitfield spanning all 32 bits (`Reserved -> get(0usize, 32u8)` at dxgk_bindings_dump.rs:28674-28710). The remaining two arms `D3DKMDT_VIRTUALGPUSURFACEDATA` (begins dxgk_bindings_dump.rs:28827) and `D3DKMDT_FENCESTORAGESURFACEDATA` (dxgk_bindings_dump.rs:64831-64839, 120 bytes, embeds a full `AllocationInfo: DXGK_ALLOCATIONINFO`) round out the union.

---

#### B2.9 `DXGKARG_BUILDPAGINGBUFFER` — full struct, operation discriminant, the entire operation union, and every member struct

This is the single most important struct for the fake-VidMm model: dxgkrnl calls `DxgkDdiBuildPagingBuffer` to ask the driver to emit DMA for memory motion / page-table updates. In the Helios decorative-GpuMmu model most operations are no-ops, but the driver must still accept every variant. The outer struct (verbatim, dxgk_bindings_dump.rs:69020-69031):
```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER {
    pub pDmaBuffer: *mut ::core::ffi::c_void,
    pub DmaSize: UINT,
    pub pDmaBufferPrivateData: *mut ::core::ffi::c_void,
    pub DmaBufferPrivateDataSize: UINT,
    pub Operation: DXGK_BUILDPAGINGBUFFER_OPERATION,
    pub MultipassOffset: UINT,
    pub __bindgen_anon_1: _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1,
    pub hSystemContext: HANDLE,
    pub DmaBufferGpuVirtualAddress: D3DGPU_VIRTUAL_ADDRESS,
    pub DmaBufferWriteOffset: UINT,
}
```
Layout (dxgk_bindings_dump.rs:70205-70241): **320 bytes**, align 8; offsets `pDmaBuffer`=0, `DmaSize`=8, `pDmaBufferPrivateData`=16, `DmaBufferPrivateDataSize`=24, `Operation`=28, `MultipassOffset`=32, **union `__bindgen_anon_1`=… (256 bytes, ending at 288)**, `hSystemContext`=296, `DmaBufferGpuVirtualAddress`=304, `DmaBufferWriteOffset`=312. Aliases (dxgk_bindings_dump.rs:70252-70253):
```rust
pub type DXGKARG_BUILDPAGINGBUFFER = _DXGKARG_BUILDPAGINGBUFFER;
pub type IN_PDXGKARG_BUILDPAGINGBUFFER = *mut DXGKARG_BUILDPAGINGBUFFER;
```

**The `Operation` discriminant enum** `DXGK_BUILDPAGINGBUFFER_OPERATION` (verbatim, dxgk_bindings_dump.rs:66765-66791). There is no separate `DXGK_OPERATION` enum — the variant *constants* are spelled `DXGK_OPERATION_*` but the enum module is `_DXGK_BUILDPAGINGBUFFER_OPERATION`:
```rust
pub mod _DXGK_BUILDPAGINGBUFFER_OPERATION {
    pub type Type = ::core::ffi::c_int;
    pub const DXGK_OPERATION_TRANSFER: Type = 0;
    pub const DXGK_OPERATION_FILL: Type = 1;
    pub const DXGK_OPERATION_DISCARD_CONTENT: Type = 2;
    pub const DXGK_OPERATION_READ_PHYSICAL: Type = 3;
    pub const DXGK_OPERATION_WRITE_PHYSICAL: Type = 4;
    pub const DXGK_OPERATION_MAP_APERTURE_SEGMENT: Type = 5;
    pub const DXGK_OPERATION_UNMAP_APERTURE_SEGMENT: Type = 6;
    pub const DXGK_OPERATION_SPECIAL_LOCK_TRANSFER: Type = 7;
    pub const DXGK_OPERATION_VIRTUAL_TRANSFER: Type = 8;
    pub const DXGK_OPERATION_VIRTUAL_FILL: Type = 9;
    pub const DXGK_OPERATION_INIT_CONTEXT_RESOURCE: Type = 10;
    pub const DXGK_OPERATION_UPDATE_PAGE_TABLE: Type = 11;
    pub const DXGK_OPERATION_FLUSH_TLB: Type = 12;
    pub const DXGK_OPERATION_UPDATE_CONTEXT_ALLOCATION: Type = 13;
    pub const DXGK_OPERATION_COPY_PAGE_TABLE_ENTRIES: Type = 14;
    pub const DXGK_OPERATION_NOTIFY_RESIDENCY: Type = 15;
    pub const DXGK_OPERATION_SIGNAL_MONITORED_FENCE: Type = 16;
    pub const DXGK_OPERATION_MAP_APERTURE_SEGMENT2: Type = 17;
    pub const DXGK_OPERATION_NOTIFY_FENCE_RESIDENCY: Type = 18;
    pub const DXGK_OPERATION_MAP_MMU: Type = 19;
    pub const DXGK_OPERATION_UNMAP_MMU: Type = 20;
    pub const DXGK_OPERATION_NOTIFY_RESIDENCY2: Type = 21;
    pub const DXGK_OPERATION_NOTIFY_ALLOC: Type = 22;
}
pub use self::_DXGK_BUILDPAGINGBUFFER_OPERATION::Type as DXGK_BUILDPAGINGBUFFER_OPERATION;
```

**The operation union** `_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1` (256 bytes; verbatim, dxgk_bindings_dump.rs:69032-69089). bindgen represents this C union as a struct of `__BindgenUnionField<T>` members all overlapping a `bindgen_union_field: [u64; 32usize]` backing store — each named member is read via `.as_ref()`/`.as_mut()` on the `__BindgenUnionField`:
```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1 {
    pub Transfer: __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1>,
    pub Fill: __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_2>,
    pub DiscardContent: __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_3>,
    pub ReadPhysical: __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_4>,
    pub WritePhysical: __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_5>,
    pub MapApertureSegment: __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_6>,
    pub UnmapApertureSegment: __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_7>,
    pub SpecialLockTransfer: __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_8>,
    pub InitContextResource: __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_9>,
    pub TransferVirtual: __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_TRANSFERVIRTUAL>,
    pub FillVirtual: __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_FILLVIRTUAL>,
    pub UpdatePageTable: __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE>,
    pub FlushTlb: __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_FLUSHTLB>,
    pub CopyPageTableEntries: __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_COPYPAGETABLEENTRIES>,
    pub UpdateContextAllocation: __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_UPDATECONTEXTALLOCATION>,
    pub NotifyResidency: __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_NOTIFYRESIDENCY>,
    pub SignalMonitoredFence: __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_SIGNALMONITOREDFENCE>,
    pub MapApertureSegment2: __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_10>,
    pub NotifyFenceResidency: __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_NOTIFY_FENCE_RESIDENCY>,
    pub MmapMmu: __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_MAPMMU>,
    pub UnmapMmu: __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_UNMAPMMU>,
    pub NotifyResidency2: __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_NOTIFYRESIDENCY2>,
    pub NotifyAllocation: __BindgenUnionField<DXGK_BUILDPAGINGBUFFER_NOTIFYALLOC>,
    pub Reserved: __BindgenUnionField<_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_11>,
    pub bindgen_union_field: [u64; 32usize],
}
```
All 24 arms sit at offset 0 (asserts dxgk_bindings_dump.rs:70089-70192); the union is 256 bytes because of the `Reserved` arm (a `[UINT; 64]`, see below).

**Every member struct, verbatim:**

`Transfer` = `..._ty_1` (dxgk_bindings_dump.rs:69090-69099, 64 bytes). Note `Source`/`Destination` are themselves nested structs whose inner `__bindgen_anon_1` is a union of `SegmentAddress: LARGE_INTEGER` and `pMdl: *mut MDL`:
```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1 {
    pub hAllocation: HANDLE,
    pub TransferOffset: UINT,
    pub TransferSize: SIZE_T,
    pub Source: _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1__bindgen_ty_1,
    pub Destination: _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1__bindgen_ty_2,
    pub Flags: DXGK_TRANSFERFLAGS,
    pub MdlOffset: UINT,
}
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1__bindgen_ty_1 {   // Source
    pub SegmentId: UINT,
    pub __bindgen_anon_1: _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1__bindgen_ty_1__bindgen_ty_1,
}
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1__bindgen_ty_1__bindgen_ty_1 {  // Source.{SegmentAddress|pMdl}
    pub SegmentAddress: __BindgenUnionField<LARGE_INTEGER>,
    pub pMdl: __BindgenUnionField<*mut MDL>,
    pub bindgen_union_field: u64,
}
```
(`Destination` = `..._ty_1__bindgen_ty_2` at dxgk_bindings_dump.rs:69174-69183 has the identical `{SegmentId, {SegmentAddress|pMdl}}` shape. Transfer field offsets dxgk_bindings_dump.rs:69256-69290: `hAllocation`=0, `TransferOffset`=8, `TransferSize`=16, `Source`=24, `Destination`=40, `Flags`=56, `MdlOffset`=60.)

`Fill` = `..._ty_2` (dxgk_bindings_dump.rs:69301-69312, 40 bytes):
```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_2 {
    pub hAllocation: HANDLE,
    pub FillSize: SIZE_T,
    pub FillPattern: UINT,
    pub Destination: _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_2__bindgen_ty_1,
}
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_2__bindgen_ty_1 {
    pub SegmentId: UINT,
    pub SegmentAddress: LARGE_INTEGER,
}
```
(offsets dxgk_bindings_dump.rs:69356-69375: `hAllocation`=0, `FillSize`=8, `FillPattern`=16, `Destination`=24.)

`DiscardContent` = `..._ty_3` (dxgk_bindings_dump.rs:69387-69392, 24 bytes):
```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_3 {
    pub hAllocation: HANDLE,
    pub Flags: DXGK_DISCARDCONTENTFLAGS,
    pub SegmentId: UINT,
    pub SegmentAddress: PHYSICAL_ADDRESS,
}
```
`ReadPhysical` = `..._ty_4` (dxgk_bindings_dump.rs:69434-69437, 16 bytes) and `WritePhysical` = `..._ty_5` (dxgk_bindings_dump.rs:69469-69472, 16 bytes) are identical:
```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_4 {  // and __bindgen_ty_5
    pub SegmentId: UINT,
    pub PhysicalAddress: PHYSICAL_ADDRESS,
}
```
`MapApertureSegment` = `..._ty_6` (dxgk_bindings_dump.rs:69505-69514, 56 bytes):
```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_6 {
    pub hDevice: HANDLE,
    pub hAllocation: HANDLE,
    pub SegmentId: UINT,
    pub OffsetInPages: SIZE_T,
    pub NumberOfPages: SIZE_T,
    pub pMdl: PMDL,
    pub Flags: DXGK_MAPAPERTUREFLAGS,
    pub MdlOffset: ULONG,
}
```
`UnmapApertureSegment` = `..._ty_7` (dxgk_bindings_dump.rs:69576-69583, 48 bytes):
```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_7 {
    pub hDevice: HANDLE,
    pub hAllocation: HANDLE,
    pub SegmentId: UINT,
    pub OffsetInPages: SIZE_T,
    pub NumberOfPages: SIZE_T,
    pub DummyPage: PHYSICAL_ADDRESS,
}
```
`SpecialLockTransfer` = `..._ty_8` (dxgk_bindings_dump.rs:69635-69644, 72 bytes; same `Source`/`Destination` `{SegmentId,{SegmentAddress|pMdl}}` nesting as Transfer):
```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_8 {
    pub hAllocation: HANDLE,
    pub TransferOffset: UINT,
    pub TransferSize: SIZE_T,
    pub Source: _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_8__bindgen_ty_1,
    pub Destination: _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_8__bindgen_ty_2,
    pub Flags: DXGK_TRANSFERFLAGS,
    pub SwizzlingRangeId: UINT,
    pub SwizzlingRangeData: UINT,
}
```
`InitContextResource` = `..._ty_9` (dxgk_bindings_dump.rs:69851-69862, 40 bytes):
```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_9 {
    pub hAllocation: HANDLE,
    pub Destination: _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_9__bindgen_ty_1,
}
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_9__bindgen_ty_1 {
    pub SegmentId: UINT,
    pub __bindgen_anon_1: _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_9__bindgen_ty_1__bindgen_ty_1, // {SegmentAddress|pMdl}
    pub VirtualAddress: PVOID,
    pub GpuVirtualAddress: D3DGPU_VIRTUAL_ADDRESS,
}
```
`MapApertureSegment2` = `..._ty_10` (dxgk_bindings_dump.rs:69975-69985, 72 bytes; carries an `Adl: DXGK_ADL` and `CpuVisibleAddress: PVOID`):
```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_10 {
    pub hDevice: HANDLE,
    pub hAllocation: HANDLE,
    pub SegmentId: UINT,
    pub OffsetInPages: SIZE_T,
    pub NumberOfPages: SIZE_T,
    pub Adl: DXGK_ADL,
    pub Flags: DXGK_MAPAPERTUREFLAGS,
    pub AdlOffset: ULONG,
    pub CpuVisibleAddress: PVOID,
}
```
`Reserved` = `..._ty_11` — this is what makes the union 256 bytes (dxgk_bindings_dump.rs:70053-70055):
```rust
pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_11 {
    pub Reserved: [UINT; 64usize],
}
```

The remaining arms point at **named, top-level** op structs (defined outside the union). The page-table-relevant ones, verbatim:

`UpdatePageTable` → `DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE` (dxgk_bindings_dump.rs:67573-67588, 104 bytes) — this is the page-table write op; `pPageTableEntries: *mut DXGK_PTE` is the array of PTEs:
```rust
pub struct _DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE {
    pub PageTableLevel: UINT,
    pub hAllocation: HANDLE,
    pub PageTableAddress: DXGK_PAGETABLEUPDATEADDRESS,
    pub pPageTableEntries: *mut DXGK_PTE,
    pub StartIndex: UINT,
    pub NumPageTableEntries: UINT,
    pub Reserved0: UINT,
    pub Flags: DXGK_UPDATEPAGETABLEFLAGS,
    pub DriverProtection: UINT64,
    pub AllocationOffsetInBytes: UINT64,
    pub hProcess: HANDLE,
    pub UpdateMode: DXGK_PAGETABLEUPDATEMODE,
    pub pPageTableEntries64KB: *mut DXGK_PTE,
    pub FirstPteVirtualAddress: D3DGPU_VIRTUAL_ADDRESS,
}
```
`CopyPageTableEntries` → `DXGK_BUILDPAGINGBUFFER_COPYPAGETABLEENTRIES` (dxgk_bindings_dump.rs:68176-68179, 16 bytes):
```rust
pub struct _DXGK_BUILDPAGINGBUFFER_COPYPAGETABLEENTRIES {
    pub NumRanges: UINT,
    pub pRanges: *mut DXGK_BUILDPAGINGBUFFER_COPY_RANGE,
}
```
…and the range it points at, `DXGK_BUILDPAGINGBUFFER_COPY_RANGE` (dxgk_bindings_dump.rs:67492-67498, 32 bytes):
```rust
pub struct _DXGK_BUILDPAGINGBUFFER_COPY_RANGE {
    pub NumPageTableEntries: UINT,
    pub SrcPageTableAddress: D3DGPU_VIRTUAL_ADDRESS,
    pub DstPageTableAddress: D3DGPU_VIRTUAL_ADDRESS,
    pub SrcStartPteIndex: UINT,
    pub DstStartPteIndex: UINT,
}
```
`FlushTlb` → `DXGK_BUILDPAGINGBUFFER_FLUSHTLB` (dxgk_bindings_dump.rs:67531-67536, 40 bytes):
```rust
pub struct _DXGK_BUILDPAGINGBUFFER_FLUSHTLB {
    pub RootPageTableAddress: D3DGPU_PHYSICAL_ADDRESS,
    pub hProcess: HANDLE,
    pub StartVirtualAddress: D3DGPU_VIRTUAL_ADDRESS,
    pub EndVirtualAddress: D3DGPU_VIRTUAL_ADDRESS,
}
```
`MmapMmu` → `DXGK_BUILDPAGINGBUFFER_MAPMMU` (dxgk_bindings_dump.rs:68452-68459, 40 bytes):
```rust
pub struct _DXGK_BUILDPAGINGBUFFER_MAPMMU {
    pub hAllocation: HANDLE,
    pub VirtualAddress: UINT64,
    pub MmuId: UINT16,
    pub SegmentId: UINT16,
    pub AllocationOffsetInPages: UINT32,
    pub Adl: DXGK_ADL,
}
```
`UnmapMmu` → `DXGK_BUILDPAGINGBUFFER_UNMAPMMU` (dxgk_bindings_dump.rs:68500-68507, 32 bytes):
```rust
pub struct _DXGK_BUILDPAGINGBUFFER_UNMAPMMU {
    pub hAllocation: HANDLE,
    pub VirtualAddress: UINT64,
    pub MmuId: UINT16,
    pub Reserved0: UINT16,
    pub AllocationOffset: UINT32,
    pub NumberOfPages: UINT32,
}
```
`UpdateContextAllocation` → `DXGK_BUILDPAGINGBUFFER_UPDATECONTEXTALLOCATION` (dxgk_bindings_dump.rs:68209-68214, 32 bytes):
```rust
pub struct _DXGK_BUILDPAGINGBUFFER_UPDATECONTEXTALLOCATION {
    pub ContextAllocation: D3DGPU_VIRTUAL_ADDRESS,
    pub ContextAllocationSize: UINT64,
    pub pDriverPrivateData: PVOID,
    pub DriverPrivateDataSize: UINT,
}
```
`SignalMonitoredFence` → `DXGK_BUILDPAGINGBUFFER_SIGNALMONITOREDFENCE` (dxgk_bindings_dump.rs:68258-68261, 16 bytes) — directly relevant to the "venus submit must drive the WDDM fence" requirement:
```rust
pub struct _DXGK_BUILDPAGINGBUFFER_SIGNALMONITOREDFENCE {
    pub MonitoredFenceGpuVa: D3DGPU_VIRTUAL_ADDRESS,
    pub MonitoredFenceValue: UINT64,
}
```
`FillVirtual` → `DXGK_BUILDPAGINGBUFFER_FILLVIRTUAL` (dxgk_bindings_dump.rs:67669-67675, 40 bytes):
```rust
pub struct _DXGK_BUILDPAGINGBUFFER_FILLVIRTUAL {
    pub hAllocation: HANDLE,
    pub AllocationOffsetInBytes: UINT64,
    pub FillSizeInBytes: UINT64,
    pub FillPattern: UINT,
    pub DestinationVirtualAddress: D3DGPU_VIRTUAL_ADDRESS,
}
```
(The remaining named arms `TransferVirtual` → `DXGK_BUILDPAGINGBUFFER_TRANSFERVIRTUAL`, `NotifyResidency` → `DXGK_BUILDPAGINGBUFFER_NOTIFYRESIDENCY`, `NotifyFenceResidency` → `DXGK_BUILDPAGINGBUFFER_NOTIFY_FENCE_RESIDENCY`, `NotifyResidency2` → `DXGK_BUILDPAGINGBUFFER_NOTIFYRESIDENCY2` (head at dxgk_bindings_dump.rs:68550), and `NotifyAllocation` → `DXGK_BUILDPAGINGBUFFER_NOTIFYALLOC` exist as named structs in the dump but were not individually expanded here; they follow the same `#[repr(C)] pub struct … { … } pub type … = _…;` pattern.)

`DXGK_UPDATEPAGETABLEFLAGS` (the `Flags` member of `UPDATEPAGETABLE`) is a bare bitfield struct (dxgk_bindings_dump.rs:47576-47579):
```rust
pub struct _DXGK_UPDATEPAGETABLEFLAGS {
    pub _bitfield_align_1: [u32; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}
```
Its accessor bits (verified getters at dxgk_bindings_dump.rs:47590-47760): `Repeat`=0, `InitialUpdate`=1, `NotifyEviction`=2, `Use64KBPages`=3, `NativeFence`=4 (each `get(POS, 1u8)`).

The `PageTableAddress`/`UpdateMode` helper types referenced by `UPDATEPAGETABLE`:
```rust
// dxgk_bindings_dump.rs:47467-47473
pub mod _DXGK_PAGETABLEUPDATEMODE {
    pub type Type = ::core::ffi::c_int;
    pub const DXGK_PAGETABLEUPDATE_CPU_VIRTUAL: Type = 0;
    pub const DXGK_PAGETABLEUPDATE_GPU_VIRTUAL: Type = 1;
    pub const DXGK_PAGETABLEUPDATE_GPU_PHYSICAL: Type = 2;
}
pub use self::_DXGK_PAGETABLEUPDATEMODE::Type as DXGK_PAGETABLEUPDATEMODE;
// dxgk_bindings_dump.rs:47475-47485
pub struct _DXGK_PAGETABLEUPDATEADDRESS {
    pub __bindgen_anon_1: _DXGK_PAGETABLEUPDATEADDRESS__bindgen_ty_1,
}
pub union _DXGK_PAGETABLEUPDATEADDRESS__bindgen_ty_1 {
    pub CpuVirtual: PVOID,
    pub GpuPhysical: D3DGPU_PHYSICAL_ADDRESS,
    pub GpuVirtual: D3DGPU_VIRTUAL_ADDRESS,
}
```

---

#### B2.10 `DXGK_PTE` (`struct _DXGK_PTE`) + accessors, and page-table-level descriptors

The page-table entry written by `UPDATE_PAGE_TABLE`. In the Helios decorative-GpuMmu model the host owns the real MMU and never reads these, but VidMm still hands them to the driver. The struct is two unions: a 64-bit flags word (bitfields) and a `PageAddress`/`PageTableAddress` union (verbatim, dxgk_bindings_dump.rs:12264-12281):
```rust
pub struct _DXGK_PTE {
    pub __bindgen_anon_1: _DXGK_PTE__bindgen_ty_1,   // Flags bitfield | raw Flags: ULONGLONG
    pub __bindgen_anon_2: _DXGK_PTE__bindgen_ty_2,   // PageAddress | PageTableAddress
}
pub union _DXGK_PTE__bindgen_ty_1 {
    pub __bindgen_anon_1: _DXGK_PTE__bindgen_ty_1__bindgen_ty_1,
    pub Flags: ULONGLONG,
}
pub struct _DXGK_PTE__bindgen_ty_1__bindgen_ty_1 {
    pub _bitfield_align_1: [u64; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 8usize]>,
}
```
**PTE flag bit map** (verified getters at dxgk_bindings_dump.rs:12291-12687; each is `_bitfield_1.get(POS, WIDTH)` returning `ULONGLONG`):

| Accessor | get(POS, WIDTH) | Getter line |
|---|---|---|
| `Valid` | (0, 1) | 12293 |
| `Zero` | (1, 1) | 12329 |
| `CacheCoherent` | (2, 1) | 12365 |
| `ReadOnly` | (3, 1) | 12401 |
| `NoExecute` | (4, 1) | 12437 |
| `Segment` | (5, 5) | 12473 |
| `LargePage` | (10, 1) | 12509 |
| `PhysicalAdapterIndex` | (11, 6) | 12545 |
| `PageTablePageSize` | (17, 2) | 12581 |
| `SystemReserved0` | (19, 1) | 12617 |
| `Reserved` | (20, 44) | 12653 |

(The `new_bitfield_1` constructor at dxgk_bindings_dump.rs:12689-12701 enumerates them in this exact order.) Second union (dxgk_bindings_dump.rs:12834-12839):
```rust
pub union _DXGK_PTE__bindgen_ty_2 {
    pub PageAddress: ULONGLONG,
    pub PageTableAddress: ULONGLONG,
}
```
Whole `DXGK_PTE` is **16 bytes**, align 8 (asserts dxgk_bindings_dump.rs:12866-12867). Alias (dxgk_bindings_dump.rs:12878): `pub type DXGK_PTE = _DXGK_PTE;`

The `PageTablePageSize` field draws from the enum `DXGK_PTE_PAGE_SIZE` (dxgk_bindings_dump.rs:12258-12263):
```rust
pub mod _DXGK_PTE_PAGE_SIZE {
    pub type Type = ::core::ffi::c_int;
    pub const DXGK_PTE_PAGE_TABLE_PAGE_4KB: Type = 0;
    pub const DXGK_PTE_PAGE_TABLE_PAGE_64KB: Type = 1;
}
pub use self::_DXGK_PTE_PAGE_SIZE::Type as DXGK_PTE_PAGE_SIZE;
```

**Page-table-level descriptor** `DXGK_PAGE_TABLE_LEVEL_DESC` (the per-level geometry the driver reports via caps; verbatim, dxgk_bindings_dump.rs:47537-47543, 20 bytes):
```rust
pub struct _DXGK_PAGE_TABLE_LEVEL_DESC {
    pub PageTableIndexBitCount: UINT,
    pub PageTableSegmentId: UINT,
    pub PagingProcessPageTableSegmentId: UINT,
    pub PageTableSizeInBytes: UINT,
    pub PageTableAlignmentInBytes: UINT,
}
pub type DXGK_PAGE_TABLE_LEVEL_DESC = _DXGK_PAGE_TABLE_LEVEL_DESC;  // :47573
```
Related GpuMmu level-count constants (dxgk_bindings_dump.rs:215-216):
```rust
pub const DXGK_MAX_PAGE_TABLE_LEVEL_COUNT: u32 = 6;
pub const DXGK_MIN_PAGE_TABLE_LEVEL_COUNT: u32 = 2;
```

**Names searched for but NOT present** (reported per instructions): there is **no** struct literally named `PTE_PAGE` or `DXGK_PAGE_TABLE` (the only `PTE_PAGE` matches are the `DXGK_PTE_PAGE_TABLE_PAGE_4KB/64KB` enum constants above and the OS-feature flags `DXGK_OS_FEATURE_PER_PTE_PAGE_SIZE`=2 / `DXGK_FEATURE_PER_PTE_PAGE_SIZE`=268435458 at dxgk_bindings_dump.rs:23546/23573; no struct). The prompt's `D3DKMDT_STANDARDALLOCATIONTYPE` is actually `D3DKMDT_STANDARDALLOCATION_TYPE` (with the underscore). The prompt's `DXGK_OPERATION` enum does not exist as a distinct type — the discriminant type is `DXGK_BUILDPAGINGBUFFER_OPERATION` whose constants are spelled `DXGK_OPERATION_*`. Also note that the `D3DKMT_*` (user-mode `d3dkmthk.h`) cousins of these structs are present in the same dump (e.g. `_D3DDDI_ALLOCATIONINFO` at dxgk_bindings_dump.rs:12930) and must not be confused with the kernel `DXGK*` DDI structs above.

### B3. Verbatim Rust types — command submission, device/context, fences, interrupts

All quotes below are verbatim from `/home/rupansh/helios-vgpu/dxgk_bindings_dump.rs` (bindgen of WDK 10.0.26100 `d3dkmddi.h`+`dispmprt.h`, `DXGKDDI_INTERFACE_VERSION` = WDDM 3.2). Each block carries its bindgen line number. The bindgen pattern is: each C struct `FOO` becomes `pub struct _FOO` with a trailing `pub type FOO = _FOO;` alias; anonymous unions/bitfields become nested `__bindgen_ty_N` structs/unions; union members reachable by name use `__BindgenUnionField<T>` plus a `bindgen_union_field` payload; bitfields use `__BindgenBitfieldUnit<[u8; N]>` with `get(offset,width)`/`set(offset,width,val)` accessors and per-field `X()`/`set_X()`/`X_raw()`/`set_X_raw()` methods. Step 2 MUST use these exact accessor names.

---

#### B3.1 `DXGKARG_SUBMITCOMMAND` (the DDI arg passed to `DxgkDdiSubmitCommand`)

The first field is an anonymous union of `hDevice`/`hContext` (`__bindgen_anon_1`). Struct size = 96 bytes, align 8.

```rust
// dxgk_bindings_dump.rs:66209
pub struct _DXGKARG_SUBMITCOMMAND {
    pub __bindgen_anon_1: _DXGKARG_SUBMITCOMMAND__bindgen_ty_1,
    pub DmaBufferSegmentId: UINT,
    pub DmaBufferPhysicalAddress: PHYSICAL_ADDRESS,
    pub DmaBufferSize: UINT,
    pub DmaBufferSubmissionStartOffset: UINT,
    pub DmaBufferSubmissionEndOffset: UINT,
    pub pDmaBufferPrivateData: *mut ::core::ffi::c_void,
    pub DmaBufferPrivateDataSize: UINT,
    pub DmaBufferPrivateDataSubmissionStartOffset: UINT,
    pub DmaBufferPrivateDataSubmissionEndOffset: UINT,
    pub SubmissionFenceId: UINT,
    pub VidPnSourceId: D3DDDI_VIDEO_PRESENT_SOURCE_ID,
    pub FlipInterval: D3DDDI_FLIPINTERVAL_TYPE::Type,
    pub Flags: DXGK_SUBMITCOMMANDFLAGS,
    pub EngineOrdinal: UINT,
    pub DmaBufferVirtualAddress: D3DGPU_VIRTUAL_ADDRESS,
    pub NodeOrdinal: UINT,
}
// dxgk_bindings_dump.rs:66228
#[repr(C)]
#[derive(Copy, Clone)]
pub union _DXGKARG_SUBMITCOMMAND__bindgen_ty_1 {
    pub hDevice: HANDLE,
    pub hContext: HANDLE,
}
// ... (size/offset asserts elided) ...
// dxgk_bindings_dump.rs:66333
pub type DXGKARG_SUBMITCOMMAND = _DXGKARG_SUBMITCOMMAND;
pub type IN_CONST_PDXGKARG_SUBMITCOMMAND = *const DXGKARG_SUBMITCOMMAND;
```
Key offsets (from the bindgen asserts, `dxgk_bindings_dump.rs:66258-66323`): `DmaBufferSegmentId @8`, `DmaBufferPhysicalAddress @16`, `DmaBufferSize @24`, `DmaBufferSubmissionStartOffset @28`, `DmaBufferSubmissionEndOffset @32`, `pDmaBufferPrivateData @40`, `DmaBufferPrivateDataSize @48`, `SubmissionFenceId @60`, `VidPnSourceId @64`, `FlipInterval @68`, `Flags @72`, `EngineOrdinal @76`, `DmaBufferVirtualAddress @80`, `NodeOrdinal @88`.

`SubmissionFenceId` is the value the driver must hand back to dxgkrnl at completion via the `DxgkCbNotifyInterrupt` `DmaCompleted` path (see B3.7/B3.8). This is the load-bearing field that makes the venus submit DRIVE the WDDM fence.

##### `DXGK_SUBMITCOMMANDFLAGS` (the `Flags` field type)

A 4-byte struct wrapping a union of a bitfield-struct and a `Value: UINT`. Bit layout (from `dxgk_bindings_dump.rs:65688-66208`, positions via `_bitfield_1.get(pos, width)`):

```rust
// dxgk_bindings_dump.rs:65662
pub struct _DXGK_SUBMITCOMMANDFLAGS {
    pub __bindgen_anon_1: _DXGK_SUBMITCOMMANDFLAGS__bindgen_ty_1,
}
// dxgk_bindings_dump.rs:65665
#[repr(C)]
#[derive(Copy, Clone)]
pub union _DXGK_SUBMITCOMMANDFLAGS__bindgen_ty_1 {
    pub __bindgen_anon_1: _DXGK_SUBMITCOMMANDFLAGS__bindgen_ty_1__bindgen_ty_1,
    pub Value: UINT,
}
// dxgk_bindings_dump.rs:65671
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct _DXGK_SUBMITCOMMANDFLAGS__bindgen_ty_1__bindgen_ty_1 {
    pub _bitfield_align_1: [u32; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}
// dxgk_bindings_dump.rs:66207
pub type DXGK_SUBMITCOMMANDFLAGS = _DXGK_SUBMITCOMMANDFLAGS;
```
Bitfield accessor methods on `_DXGK_SUBMITCOMMANDFLAGS__bindgen_ty_1__bindgen_ty_1` (name → `get(pos,width)`):
- `Paging()` / `set_Paging()` → bit 0, width 1 (`dxgk_bindings_dump.rs:65690`)
- `Present()` / `set_Present()` → bit 1, width 1 (`:65726`)
- `RedirectedPresent()` → bit 2, width 1 (`:65762`)
- `NullRendering()` → bit 3, width 1 (`:65798`)
- `Flip()` → bit 4, width 1 (`:65834`)
- `FlipWithNoWait()` → bit 5, width 1
- `ContextSwitch()` → bit 6, width 1
- `Resubmission()` → bit 7, width 1
- `VirtualMachineData()` → bit 8, width 1
- `Reserved()` → bit 9, width 23

---

#### B3.2 `DXGKARG_PATCH` (the DDI arg passed to `DxgkDdiPatch`)

Same `hDevice`/`hContext` anonymous union head. Size = 120 bytes, align 8.

```rust
// dxgk_bindings_dump.rs:65531
pub struct _DXGKARG_PATCH {
    pub __bindgen_anon_1: _DXGKARG_PATCH__bindgen_ty_1,
    pub DmaBufferSegmentId: UINT,
    pub DmaBufferPhysicalAddress: PHYSICAL_ADDRESS,
    pub pDmaBuffer: *mut ::core::ffi::c_void,
    pub DmaBufferSize: UINT,
    pub DmaBufferSubmissionStartOffset: UINT,
    pub DmaBufferSubmissionEndOffset: UINT,
    pub pDmaBufferPrivateData: *mut ::core::ffi::c_void,
    pub DmaBufferPrivateDataSize: UINT,
    pub DmaBufferPrivateDataSubmissionStartOffset: UINT,
    pub DmaBufferPrivateDataSubmissionEndOffset: UINT,
    pub pAllocationList: *const DXGK_ALLOCATIONLIST,
    pub AllocationListSize: UINT,
    pub pPatchLocationList: *const D3DDDI_PATCHLOCATIONLIST,
    pub PatchLocationListSize: UINT,
    pub PatchLocationListSubmissionStart: UINT,
    pub PatchLocationListSubmissionLength: UINT,
    pub SubmissionFenceId: UINT,
    pub Flags: DXGK_PATCHFLAGS,
    pub EngineOrdinal: UINT,
}
// dxgk_bindings_dump.rs:65553
#[repr(C)]
#[derive(Copy, Clone)]
pub union _DXGKARG_PATCH__bindgen_ty_1 {
    pub hDevice: HANDLE,
    pub hContext: HANDLE,
}
// alias (not shown in dump excerpt but follows pattern):
// pub type DXGKARG_PATCH = _DXGKARG_PATCH;
```
Offsets (`dxgk_bindings_dump.rs:65583-65630`+): `DmaBufferSegmentId @8`, `DmaBufferPhysicalAddress @16`, `pDmaBuffer @24`, `DmaBufferSize @32`, `pDmaBufferPrivateData @48`, `pAllocationList @72`, `AllocationListSize @80`, `pPatchLocationList @88`, `PatchLocationListSize @96`.

##### `DXGK_PATCHFLAGS` (the `Flags` field type)

4-byte struct, union of bitfield-struct + `Value: UINT` (`dxgk_bindings_dump.rs:65226`, alias at `:65529`). Bits (from `_bitfield_1.get`):
- `Paging()` → bit 0, width 1
- `Present()` → bit 1, width 1
- `RedirectedPresent()` → bit 2, width 1
- `NullRendering()` → bit 3, width 1
- `Reserved()` → bit 4, width 28

##### `D3DDDI_PATCHLOCATIONLIST` (the per-entry patch record)

Size = 24 bytes, align 4. `AllocationIndex @0`, anon union `@4`, `DriverId @8`, `AllocationOffset @12`, `PatchOffset @16`, `SplitOffset @20`.

```rust
// dxgk_bindings_dump.rs:13878
pub struct _D3DDDI_PATCHLOCATIONLIST {
    pub AllocationIndex: UINT,
    pub __bindgen_anon_1: _D3DDDI_PATCHLOCATIONLIST__bindgen_ty_1,
    pub DriverId: UINT,
    pub AllocationOffset: UINT,
    pub PatchOffset: UINT,
    pub SplitOffset: UINT,
}
// dxgk_bindings_dump.rs:13886
#[repr(C)]
#[derive(Copy, Clone)]
pub union _D3DDDI_PATCHLOCATIONLIST__bindgen_ty_1 {
    pub __bindgen_anon_1: _D3DDDI_PATCHLOCATIONLIST__bindgen_ty_1__bindgen_ty_1,
    pub Value: UINT,
}
// dxgk_bindings_dump.rs:13892
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct _D3DDDI_PATCHLOCATIONLIST__bindgen_ty_1__bindgen_ty_1 {
    pub _bitfield_align_1: [u32; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}
// dxgk_bindings_dump.rs:14063
pub type D3DDDI_PATCHLOCATIONLIST = _D3DDDI_PATCHLOCATIONLIST;
```
Inner bitfield accessors (`dxgk_bindings_dump.rs:13909-14007`): `SlotId()` → bit 0, width 24; `Reserved()` → bit 24, width 8; constructor `new_bitfield_1(SlotId, Reserved)`.

##### `DXGK_ALLOCATIONLIST` (the per-entry allocation record referenced by Patch/SubmitCommand/Render)

Note: this struct is named `_DXGK_ALLOCATIONLIST`, not `_DXGK_PATCHLOCATIONLIST`. Size = 24, align 8.

```rust
// dxgk_bindings_dump.rs:35809
pub struct _DXGK_ALLOCATIONLIST {
    pub hDeviceSpecificAllocation: HANDLE,
    pub __bindgen_anon_1: _DXGK_ALLOCATIONLIST__bindgen_ty_1,
    pub __bindgen_anon_2: _DXGK_ALLOCATIONLIST__bindgen_ty_2,
}
// dxgk_bindings_dump.rs:35814 (anon_1: bitfields)
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct _DXGK_ALLOCATIONLIST__bindgen_ty_1 {
    pub _bitfield_align_1: [u32; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}
// dxgk_bindings_dump.rs:35977 (anon_2: union of physical/virtual address)
#[repr(C)]
pub struct _DXGK_ALLOCATIONLIST__bindgen_ty_2 {
    pub PhysicalAddress: __BindgenUnionField<PHYSICAL_ADDRESS>,
    pub VirtualAddress: __BindgenUnionField<D3DGPU_VIRTUAL_ADDRESS>,
    pub bindgen_union_field: u64,
}
// dxgk_bindings_dump.rs:36030
pub type DXGK_ALLOCATIONLIST = _DXGK_ALLOCATIONLIST;
```
`anon_1` bitfields (`dxgk_bindings_dump.rs:35829-35975`): `WriteOperation()` → bit 0, width 1; `SegmentId()` → bit 1, width 5; `Reserved()` → bit 6, width 26; constructor `new_bitfield_1(WriteOperation, SegmentId, Reserved)`.

---

#### B3.3 `DXGKARG_RENDER` (the DDI arg passed to `DxgkDdiRender` / `DxgkDdiRenderKm`)

Present. Size = 112 bytes, align 8.

```rust
// dxgk_bindings_dump.rs:36046
pub struct _DXGKARG_RENDER {
    pub pCommand: *const ::core::ffi::c_void,
    pub CommandLength: UINT,
    pub pDmaBuffer: *mut ::core::ffi::c_void,
    pub DmaSize: UINT,
    pub pDmaBufferPrivateData: *mut ::core::ffi::c_void,
    pub DmaBufferPrivateDataSize: UINT,
    pub pAllocationList: *mut DXGK_ALLOCATIONLIST,
    pub AllocationListSize: UINT,
    pub pPatchLocationListIn: *mut D3DDDI_PATCHLOCATIONLIST,
    pub PatchLocationListInSize: UINT,
    pub pPatchLocationListOut: *mut D3DDDI_PATCHLOCATIONLIST,
    pub PatchLocationListOutSize: UINT,
    pub MultipassOffset: UINT,
    pub DmaBufferSegmentId: UINT,
    pub DmaBufferPhysicalAddress: PHYSICAL_ADDRESS,
}
// dxgk_bindings_dump.rs:36124
pub type DXGKARG_RENDER = _DXGKARG_RENDER;
pub type INOUT_PDXGKARG_RENDER = *mut DXGKARG_RENDER;
```

---

#### B3.4 `DXGKARG_CREATEDEVICE` + `DXGK_CREATEDEVICEFLAGS`

`DXGK_CREATEDEVICEFLAGS` is 4 bytes; union of bitfield-struct + `Value: UINT` (`dxgk_bindings_dump.rs:42556`, alias `:42815`):

```rust
// dxgk_bindings_dump.rs:42556
pub struct _DXGK_CREATEDEVICEFLAGS {
    pub __bindgen_anon_1: _DXGK_CREATEDEVICEFLAGS__bindgen_ty_1,
}
// dxgk_bindings_dump.rs:42559
#[repr(C)]
#[derive(Copy, Clone)]
pub union _DXGK_CREATEDEVICEFLAGS__bindgen_ty_1 {
    pub __bindgen_anon_1: _DXGK_CREATEDEVICEFLAGS__bindgen_ty_1__bindgen_ty_1,
    pub Value: UINT,
}
// dxgk_bindings_dump.rs:42815
pub type DXGK_CREATEDEVICEFLAGS = _DXGK_CREATEDEVICEFLAGS;
```
Bitfields (`dxgk_bindings_dump.rs:42582-42774`): `SystemDevice()` → bit 0, width 1; `GdiDevice()` → bit 1, width 1; `Reserved()` → bit 2, width 29; `Reserved0()` → bit 31, width 1; constructor `new_bitfield_1(SystemDevice, GdiDevice, Reserved, Reserved0)`.

`DXGKARG_CREATEDEVICE` — size 32, align 8. Note the second field is an anonymous **union** of `Flags` and `pInfo` (`Flags` on input, `pInfo` on output):

```rust
// dxgk_bindings_dump.rs:42818
pub struct _DXGKARG_CREATEDEVICE {
    pub hDevice: HANDLE,
    pub __bindgen_anon_1: _DXGKARG_CREATEDEVICE__bindgen_ty_1,
    pub Pasid: ULONG,
    pub hKmdProcess: HANDLE,
}
// dxgk_bindings_dump.rs:42824
#[repr(C)]
#[derive(Copy, Clone)]
pub union _DXGKARG_CREATEDEVICE__bindgen_ty_1 {
    pub Flags: DXGK_CREATEDEVICEFLAGS,
    pub pInfo: *mut DXGK_DEVICEINFO,
}
// dxgk_bindings_dump.rs:42881
pub type DXGKARG_CREATEDEVICE = _DXGKARG_CREATEDEVICE;
pub type INOUT_PDXGKARG_CREATEDEVICE = *mut DXGKARG_CREATEDEVICE;
```
Offsets: `hDevice @0`, `Pasid @16`, `hKmdProcess @24`.

---

#### B3.5 `DXGKARG_CREATECONTEXT` + `DXGK_CREATECONTEXTFLAGS`

`DXGK_CREATECONTEXTFLAGS` — 4 bytes; union of bitfield-struct + `Value: UINT` (`dxgk_bindings_dump.rs:42885`). Bitfields (`dxgk_bindings_dump.rs:42913+`; positions via the condensed grep):
- `SystemContext()` → bit 0, width 1
- `GdiContext()` → bit 1, width 1
- `VirtualAddressing()` → bit 2, width 1
- `SystemProtectedContext()` → bit 3, width 1
- `HwQueueSupported()` → bit 4, width 1
- `TestContext()` → bit 5, width 1
- `Reserved()` → bit 6, width 26

```rust
// dxgk_bindings_dump.rs:42885
pub struct _DXGK_CREATECONTEXTFLAGS {
    pub __bindgen_anon_1: _DXGK_CREATECONTEXTFLAGS__bindgen_ty_1,
}
// dxgk_bindings_dump.rs:42888
#[repr(C)]
#[derive(Copy, Clone)]
pub union _DXGK_CREATECONTEXTFLAGS__bindgen_ty_1 {
    pub __bindgen_anon_1: _DXGK_CREATECONTEXTFLAGS__bindgen_ty_1__bindgen_ty_1,
    pub Value: UINT,
}
```

`DXGKARG_CREATECONTEXT` — size 72, align 8:

```rust
// dxgk_bindings_dump.rs:43610
pub struct _DXGKARG_CREATECONTEXT {
    pub hContext: HANDLE,
    pub NodeOrdinal: UINT,
    pub EngineAffinity: UINT,
    pub Flags: DXGK_CREATECONTEXTFLAGS,
    pub pPrivateDriverData: *mut ::core::ffi::c_void,
    pub PrivateDriverDataSize: UINT,
    pub ContextInfo: DXGK_CONTEXTINFO,
}
// dxgk_bindings_dump.rs:43658
pub type DXGKARG_CREATECONTEXT = _DXGKARG_CREATECONTEXT;
pub type INOUT_PDXGKARG_CREATECONTEXT = *mut DXGKARG_CREATECONTEXT;
```
Offsets (`dxgk_bindings_dump.rs:43619-43647`): `hContext @0`, `NodeOrdinal @8`, `EngineAffinity @12`, `Flags @16`, `pPrivateDriverData @24`, `PrivateDriverDataSize @32`, `ContextInfo @36`.

> Note: bindgen emitted `VirtualAddressing` bit (#2) in the context flags but did NOT emit a `NoPatchingRequired`/`DriverManagesResidency`/`UseIoMmu` set in CREATECONTEXTFLAGS — those names (bits 0/1/2 of a *different* flags struct, also surfaced at `dxgk_bindings_dump.rs:43427/43463/43499`) belong to a neighboring flags type (`DXGK_CONTEXTINFO`/`DXGK_HWCONTEXT` area), not to `_DXGK_CREATECONTEXTFLAGS`.

---

#### B3.6 `DXGK_INTERRUPT_TYPE` (enum)

bindgen renders C enums as a module of `pub const NAME: Type = N;` plus a `pub use … as DXGK_INTERRUPT_TYPE;`. Confirmed present: `DXGK_INTERRUPT_DMA_COMPLETED` (=1), `DXGK_INTERRUPT_DMA_PREEMPTED` (=2), `DXGK_INTERRUPT_DMA_FAULTED` (=4). Verbatim:

```rust
// dxgk_bindings_dump.rs:39470
pub mod _DXGK_INTERRUPT_TYPE {
    pub type Type = ::core::ffi::c_int;
    pub const DXGK_INTERRUPT_DMA_COMPLETED: Type = 1;
    pub const DXGK_INTERRUPT_DMA_PREEMPTED: Type = 2;
    pub const DXGK_INTERRUPT_CRTC_VSYNC: Type = 3;
    pub const DXGK_INTERRUPT_DMA_FAULTED: Type = 4;
    pub const DXGK_INTERRUPT_DISPLAYONLY_VSYNC: Type = 5;
    pub const DXGK_INTERRUPT_DISPLAYONLY_PRESENT_PROGRESS: Type = 6;
    pub const DXGK_INTERRUPT_CRTC_VSYNC_WITH_MULTIPLANE_OVERLAY: Type = 7;
    pub const DXGK_INTERRUPT_MICACAST_CHUNK_PROCESSING_COMPLETE: Type = 8;
    pub const DXGK_INTERRUPT_DMA_PAGE_FAULTED: Type = 9;
    pub const DXGK_INTERRUPT_CRTC_VSYNC_WITH_MULTIPLANE_OVERLAY2: Type = 10;
    pub const DXGK_INTERRUPT_MONITORED_FENCE_SIGNALED: Type = 11;
    pub const DXGK_INTERRUPT_HWQUEUE_PAGE_FAULTED: Type = 12;
    pub const DXGK_INTERRUPT_HWCONTEXTLIST_SWITCH_COMPLETED: Type = 13;
    pub const DXGK_INTERRUPT_PERIODIC_MONITORED_FENCE_SIGNALED: Type = 14;
    pub const DXGK_INTERRUPT_SCHEDULING_LOG_INTERRUPT: Type = 15;
    pub const DXGK_INTERRUPT_GPU_ENGINE_TIMEOUT: Type = 16;
    pub const DXGK_INTERRUPT_SUSPEND_CONTEXT_COMPLETED: Type = 17;
    pub const DXGK_INTERRUPT_CRTC_VSYNC_WITH_MULTIPLANE_OVERLAY3: Type = 18;
    pub const DXGK_INTERRUPT_NATIVE_FENCE_SIGNALED: Type = 19;
    pub const DXGK_INTERRUPT_GPU_ENGINE_STATE_CHANGE: Type = 20;
}
// dxgk_bindings_dump.rs:39493
pub use self::_DXGK_INTERRUPT_TYPE::Type as DXGK_INTERRUPT_TYPE;
// dxgk_bindings_dump.rs:66625
pub use self::DXGK_INTERRUPT_TYPE as IN_CONST_DXGK_INTERRUPT_TYPE;
```

---

#### B3.7 `DXGKARGCB_NOTIFY_INTERRUPT_DATA` (the struct the driver hands to `DxgkCbNotifyInterrupt`)

This is the payload that turns a venus completion into a WDDM fence signal. The big middle field `__bindgen_anon_1` is the union over all interrupt-type payloads (bindgen used the `__BindgenUnionField<T>` representation: every member is a zero-size field offering typed access into the shared `bindgen_union_field: [u64; 8]`). Top struct size = (assert at `dxgk_bindings_dump.rs:41636`); alias and const-pointer alias at `:41657-41658`.

```rust
// dxgk_bindings_dump.rs:40459
pub struct _DXGKARGCB_NOTIFY_INTERRUPT_DATA {
    pub InterruptType: DXGK_INTERRUPT_TYPE,
    pub __bindgen_anon_1: _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1,
    pub Flags: DXGKCB_NOTIFY_INTERRUPT_DATA_FLAGS,
}
// dxgk_bindings_dump.rs:40464  — the interrupt-payload union
#[repr(C)]
pub struct _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1 {
    pub DmaCompleted: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_1,
    >,
    pub DmaPreempted: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_2,
    >,
    pub DmaFaulted: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_3,
    >,
    pub CrtcVsync: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_4,
    >,
    pub DisplayOnlyVsync: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_5,
    >,
    pub CrtcVsyncWithMultiPlaneOverlay: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_6,
    >,
    pub DisplayOnlyPresentProgress: __BindgenUnionField<
        DXGKARGCB_PRESENT_DISPLAYONLY_PROGRESS,
    >,
    pub MiracastEncodeChunkCompleted: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_7,
    >,
    pub DmaPageFaulted: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_8,
    >,
    pub CrtcVsyncWithMultiPlaneOverlay2: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_9,
    >,
    pub MonitoredFenceSignaled: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_10,
    >,
    pub HwContextListSwitchCompleted: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_11,
    >,
    pub HwQueuePageFaulted: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_12,
    >,
    pub PeriodicMonitoredFenceSignaled: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_13,
    >,
    pub SchedulingLogInterrupt: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_14,
    >,
    pub GpuEngineTimeout: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_15,
    >,
    pub SuspendContextCompleted: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_16,
    >,
    pub CrtcVsyncWithMultiPlaneOverlay3: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_17,
    >,
    pub NativeFenceSignaled: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_18,
    >,
    pub EngineStateChange: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_19,
    >,
    pub Reserved: __BindgenUnionField<
        _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_20,
    >,
    pub bindgen_union_field: [u64; 8usize],
}
// dxgk_bindings_dump.rs:41657
pub type DXGKARGCB_NOTIFY_INTERRUPT_DATA = _DXGKARGCB_NOTIFY_INTERRUPT_DATA;
pub type IN_CONST_PDXGKARGCB_NOTIFY_INTERRUPT_DATA = *mut DXGKARGCB_NOTIFY_INTERRUPT_DATA;
```

To write the `DmaCompleted` payload in Step 2, set `InterruptType = DXGK_INTERRUPT_DMA_COMPLETED` and write through `__bindgen_anon_1.DmaCompleted.as_mut()` (the `__BindgenUnionField::as_mut()` helper). The member payload struct definitions follow.

##### Member payloads (verbatim struct bodies)

`__bindgen_ty_1` = **DmaCompleted** (size 12, align 4):
```rust
// dxgk_bindings_dump.rs:40532
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_1 {
    pub SubmissionFenceId: UINT,
    pub NodeOrdinal: UINT,
    pub EngineOrdinal: UINT,
}
```
`SubmissionFenceId` here MUST equal the `DXGKARG_SUBMITCOMMAND::SubmissionFenceId` from the completing submit — this is the exact value that advances dxgkrnl's per-node fence.

`__bindgen_ty_2` = **DmaPreempted** (size 16, align 4):
```rust
// dxgk_bindings_dump.rs:40567
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_2 {
    pub PreemptionFenceId: UINT,
    pub LastCompletedFenceId: UINT,
    pub NodeOrdinal: UINT,
    pub EngineOrdinal: UINT,
}
```

`__bindgen_ty_3` = **DmaFaulted** (size assert at `:40615`):
```rust
// dxgk_bindings_dump.rs:40608
#[repr(C)]
pub struct _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_3 {
    pub FaultedFenceId: UINT,
    pub Status: NTSTATUS,
    pub NodeOrdinal: UINT,
    pub EngineOrdinal: UINT,
}
```

`__bindgen_ty_4` = **CrtcVsync** (size 24, align 8):
```rust
// dxgk_bindings_dump.rs:40658
pub struct _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_4 {
    pub VidPnTargetId: D3DDDI_VIDEO_PRESENT_TARGET_ID,
    pub PhysicalAddress: PHYSICAL_ADDRESS,
    pub PhysicalAdapterMask: UINT,
}
```

`__bindgen_ty_8` = **DmaPageFaulted** (size 64, align 8):
```rust
// dxgk_bindings_dump.rs:40833
pub struct _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_8 {
    pub FaultedFenceId: UINT,
    pub FaultedPrimitiveAPISequenceNumber: UINT64,
    pub FaultedPipelineStage: DXGK_RENDER_PIPELINE_STAGE,
    pub FaultedBindTableEntry: UINT,
    pub PageFaultFlags: DXGK_PAGE_FAULT_FLAGS,
    pub FaultedVirtualAddress: D3DGPU_VIRTUAL_ADDRESS,
    pub NodeOrdinal: UINT,
    pub EngineOrdinal: UINT,
    pub PageTableLevel: UINT,
    pub FaultErrorCode: DXGK_FAULT_ERROR_CODE,
    pub FaultedProcessHandle: HANDLE,
}
```

`__bindgen_ty_10` = **MonitoredFenceSignaled** (size 8, align 4):
```rust
// dxgk_bindings_dump.rs:40994
pub struct _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_10 {
    pub NodeOrdinal: UINT,
    pub EngineOrdinal: UINT,
}
```

`__bindgen_ty_13` = **PeriodicMonitoredFenceSignaled** (size 8, align 4):
```rust
// dxgk_bindings_dump.rs:41197
pub struct _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_13 {
    pub VidPnTargetId: D3DDDI_VIDEO_PRESENT_TARGET_ID,
    pub NotificationID: UINT,
}
```

`__bindgen_ty_18` = **NativeFenceSignaled** (size 32, align 8):
```rust
// dxgk_bindings_dump.rs:41388
pub struct _DXGKARGCB_NOTIFY_INTERRUPT_DATA__bindgen_ty_1__bindgen_ty_18 {
    pub NodeOrdinal: UINT,
    pub EngineOrdinal: UINT,
    pub SignaledNativeFenceCount: UINT,
    pub pSignaledNativeFenceArray: *mut HANDLE,
    pub hHWQueue: HANDLE,
}
```
(The remaining members `__bindgen_ty_5/6/7/9/11/12/14/15/16/17/19/20` cover DisplayOnlyVsync, CrtcVsyncWithMultiPlaneOverlay, MiracastEncodeChunkCompleted, …CrtcVsync2, HwContextListSwitchCompleted, HwQueuePageFaulted, SchedulingLogInterrupt, GpuEngineTimeout, SuspendContextCompleted, CrtcVsync3, EngineStateChange, Reserved — not reproduced here; not needed for the venus→fence path.)

The `Flags` field type alias: `pub type DXGKCB_NOTIFY_INTERRUPT_DATA_FLAGS = _DXGKCB_NOTIFY_INTERRUPT_DATA_FLAGS;` (`dxgk_bindings_dump.rs:39804`).

---

#### B3.8 Callback fn-pointer typedefs (`DxgkCbNotifyInterrupt` / `DxgkCbNotifyDpc` / `DxgkCbQueueDpc`)

These are the exact fn-pointer types stored in `DXGKRNL_INTERFACE` (B3.10). All are `::core::option::Option<unsafe extern "C" fn(...)>`.

```rust
// dxgk_bindings_dump.rs:41659
pub type DXGKCB_NOTIFY_INTERRUPT = ::core::option::Option<
    unsafe extern "C" fn(
        hAdapter: IN_CONST_HANDLE,
        arg1: IN_CONST_PDXGKARGCB_NOTIFY_INTERRUPT_DATA,
    ),
>;
// dxgk_bindings_dump.rs:41665
pub type DXGKCB_NOTIFY_DPC = ::core::option::Option<
    unsafe extern "C" fn(hAdapter: IN_CONST_HANDLE),
>;
// dxgk_bindings_dump.rs:92051
pub type DXGKCB_QUEUE_DPC = ::core::option::Option<
    unsafe extern "C" fn(DeviceHandle: HANDLE) -> BOOLEAN,
>;
```
Note: `IN_CONST_PDXGKARGCB_NOTIFY_INTERRUPT_DATA` is `*mut DXGKARGCB_NOTIFY_INTERRUPT_DATA` (alias `:41658`) despite the `IN_CONST_` prefix.

---

#### B3.9 Monitored-fence DDI arg + paging-buffer signal + DDI fn-pointer

`DXGKARG_SIGNALMONITOREDFENCE` (arg to `DxgkDdiSignalMonitoredFence`), size 80, align 8. This is the load-bearing struct for "venus submit DRIVES the WDDM fence" — `MonitoredFenceGpuVa`/`MonitoredFenceValue`/`MonitoredFenceCpuVa` are where the driver writes/advances the monitored fence value the GPU "would" have written:

```rust
// dxgk_bindings_dump.rs:71947
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct _DXGKARG_SIGNALMONITOREDFENCE {
    pub KernelSubmissionType: DXGK_KERNEL_SUBMISSION_TYPE,
    pub pDmaBuffer: *mut ::core::ffi::c_void,
    pub DmaBufferGpuVirtualAddress: D3DGPU_VIRTUAL_ADDRESS,
    pub DmaSize: UINT,
    pub pDmaBufferPrivateData: *mut ::core::ffi::c_void,
    pub DmaBufferPrivateDataSize: UINT,
    pub MultipassOffset: UINT,
    pub MonitoredFenceGpuVa: D3DGPU_VIRTUAL_ADDRESS,
    pub MonitoredFenceValue: UINT64,
    pub MonitoredFenceCpuVa: *mut ::core::ffi::c_void,
    pub hHwQueue: HANDLE,
}
```
Offsets: `MonitoredFenceGpuVa @48`, `MonitoredFenceValue @56`, `MonitoredFenceCpuVa @64`, `hHwQueue @72`.

`DXGK_BUILDPAGINGBUFFER_SIGNALMONITOREDFENCE` (the paging-buffer operation; reachable via the `DXGKARG_BUILDPAGINGBUFFER` union member `SignalMonitoredFence`, `dxgk_bindings_dump.rs:69072`), size 16, align 8:
```rust
// dxgk_bindings_dump.rs:68257
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct _DXGK_BUILDPAGINGBUFFER_SIGNALMONITOREDFENCE {
    pub MonitoredFenceGpuVa: D3DGPU_VIRTUAL_ADDRESS,
    pub MonitoredFenceValue: UINT64,
}
// dxgk_bindings_dump.rs:68281
pub type DXGK_BUILDPAGINGBUFFER_SIGNALMONITOREDFENCE = _DXGK_BUILDPAGINGBUFFER_SIGNALMONITOREDFENCE;
```

The DDI fn-pointer type for the signal callback (stored in `DRIVER_INITIALIZATION_DATA::DxgkDdiSignalMonitoredFence`):
```rust
// dxgk_bindings_dump.rs:88926
pub type PDXGKDDI_SIGNALMONITOREDFENCE = ::core::option::Option<
    unsafe extern "C" fn(
        arg1: IN_CONST_HANDLE,
        arg2: INOUT_PDXGKARG_SIGNALMONITOREDFENCE,
    ) -> NTSTATUS,
>;
```
Also relevant: the paging-buffer op enum constant `DXGK_OPERATION_SIGNAL_MONITORED_FENCE: Type = 16` (`dxgk_bindings_dump.rs:66783`), and `DXGKARG_QUERYCURRENTFENCE { CurrentFence: UINT, NodeOrdinal: UINT, EngineOrdinal: UINT }` (`dxgk_bindings_dump.rs:66600`, alias+`INOUT_PDXGKARG_QUERYCURRENTFENCE` at `:66623-66624`) — the legacy fence-query path.

> No struct literally named `MONITORED_FENCE`/`SignaledFence`/`SIGNALEDFENCE` exists as a standalone top-level type in the dump. The monitored-fence surface is exactly the set above: the `DXGKARG_SIGNALMONITOREDFENCE` DDI arg, the `DXGK_BUILDPAGINGBUFFER_SIGNALMONITOREDFENCE` paging op, the `MonitoredFenceSignaled`/`PeriodicMonitoredFenceSignaled` interrupt union members (B3.7), the `DXGK_INTERRUPT_MONITORED_FENCE_SIGNALED`/`…PERIODIC…`/`…NATIVE_FENCE_SIGNALED` enum values (B3.6), and the `D3DDDI_MONITORED_FENCE`/`D3DDDI_PERIODIC_MONITORED_FENCE` sync-object kinds at `dxgk_bindings_dump.rs:18826-18827` whose payloads are `_D3DDDI_SYNCHRONIZATIONOBJECTINFO2__bindgen_ty_1::MonitoredFence`/`PeriodicMonitoredFence` (`:22339-22340`).

---

#### B3.10 `DXGKRNL_INTERFACE` (the dxgkrnl callback table)

Size 576, align 8. `Size @0`, `Version @4`, `DeviceHandle @8`. The three callbacks Step 2 needs for the fence path are at the offsets noted (asserts `dxgk_bindings_dump.rs:94072/94102/94105`): `DxgkCbQueueDpc @48`, `DxgkCbNotifyInterrupt @128`, `DxgkCbNotifyDpc @136`.

```rust
// dxgk_bindings_dump.rs:93967
pub struct _DXGKRNL_INTERFACE {
    pub Size: ULONG,
    pub Version: ULONG,
    pub DeviceHandle: HANDLE,
    pub DxgkCbEvalAcpiMethod: DXGKCB_EVAL_ACPI_METHOD,
    pub DxgkCbGetDeviceInformation: DXGKCB_GET_DEVICE_INFORMATION,
    pub DxgkCbIndicateChildStatus: DXGKCB_INDICATE_CHILD_STATUS,
    pub DxgkCbMapMemory: DXGKCB_MAP_MEMORY,
    pub DxgkCbQueueDpc: DXGKCB_QUEUE_DPC,                       // @48
    pub DxgkCbQueryServices: DXGKCB_QUERY_SERVICES,
    pub DxgkCbReadDeviceSpace: DXGKCB_READ_DEVICE_SPACE,
    pub DxgkCbSynchronizeExecution: DXGKCB_SYNCHRONIZE_EXECUTION,
    pub DxgkCbUnmapMemory: DXGKCB_UNMAP_MEMORY,
    pub DxgkCbWriteDeviceSpace: DXGKCB_WRITE_DEVICE_SPACE,
    pub DxgkCbIsDevicePresent: DXGKCB_IS_DEVICE_PRESENT,
    pub DxgkCbGetHandleData: DXGKCB_GETHANDLEDATA,
    pub DxgkCbGetHandleParent: DXGKCB_GETHANDLEPARENT,
    pub DxgkCbEnumHandleChildren: DXGKCB_ENUMHANDLECHILDREN,
    pub DxgkCbNotifyInterrupt: DXGKCB_NOTIFY_INTERRUPT,         // @128
    pub DxgkCbNotifyDpc: DXGKCB_NOTIFY_DPC,                     // @136
    pub DxgkCbQueryVidPnInterface: DXGKCB_QUERYVIDPNINTERFACE,
    pub DxgkCbQueryMonitorInterface: DXGKCB_QUERYMONITORINTERFACE,
    pub DxgkCbGetCaptureAddress: DXGKCB_GETCAPTUREADDRESS,
    pub DxgkCbLogEtwEvent: DXGKCB_LOG_ETW_EVENT,
    pub DxgkCbExcludeAdapterAccess: DXGKCB_EXCLUDE_ADAPTER_ACCESS,
    pub DxgkCbCreateContextAllocation: DXGKCB_CREATECONTEXTALLOCATION,
    pub DxgkCbDestroyContextAllocation: DXGKCB_DESTROYCONTEXTALLOCATION,
    // ... (power/residency/handle/MDL/framebuffer/IoMmu/diagnostic callbacks elided;
    //      full list runs dxgk_bindings_dump.rs:93994-94040, e.g.
    //      DxgkCbReserveGpuVirtualAddressRange, DxgkCbMapContextAllocation,
    //      DxgkCbUpdateContextAllocation, DxgkCbAllocatePagesForMdl, DxgkCbSignalEvent,
    //      DxgkCbCreatePhysicalMemoryObject, DxgkCbMapPhysicalMemory, ...) ...
}
// dxgk_bindings_dump.rs:94316
pub type PDXGKRNL_INTERFACE = *mut _DXGKRNL_INTERFACE;
// dxgk_bindings_dump.rs:94323
pub type IN_PDXGKRNL_INTERFACE = PDXGKRNL_INTERFACE;
```
There is no top-level `pub type DXGKRNL_INTERFACE = _DXGKRNL_INTERFACE;` line in this region; the consumed forms are the two pointer aliases above (`PDXGKRNL_INTERFACE` and `IN_PDXGKRNL_INTERFACE`). `DxgkDdiStartDevice` receives this table as `DxgkInterface: IN_PDXGKRNL_INTERFACE` (`dxgk_bindings_dump.rs:94336`).

---

#### B3.11 `DRIVER_INITIALIZATION_DATA` (the full DDI fn-pointer table registered in `DriverEntry`)

Size 1544 bytes (assert `dxgk_bindings_dump.rs:95395`). First field is `Version: ULONG`; everything else is a `PDXGKDDI_*` fn-pointer (or `PVOID`/`*mut c_void` for reserved/page-table slots). The exact field name → DDI mapping Step 2 needs:

```rust
// dxgk_bindings_dump.rs:95197
pub struct _DRIVER_INITIALIZATION_DATA {
    pub Version: ULONG,
    pub DxgkDdiAddDevice: PDXGKDDI_ADD_DEVICE,
    pub DxgkDdiStartDevice: PDXGKDDI_START_DEVICE,
    pub DxgkDdiStopDevice: PDXGKDDI_STOP_DEVICE,
    pub DxgkDdiRemoveDevice: PDXGKDDI_REMOVE_DEVICE,
    pub DxgkDdiDispatchIoRequest: PDXGKDDI_DISPATCH_IO_REQUEST,
    pub DxgkDdiInterruptRoutine: PDXGKDDI_INTERRUPT_ROUTINE,
    pub DxgkDdiDpcRoutine: PDXGKDDI_DPC_ROUTINE,
    pub DxgkDdiQueryChildRelations: PDXGKDDI_QUERY_CHILD_RELATIONS,
    pub DxgkDdiQueryChildStatus: PDXGKDDI_QUERY_CHILD_STATUS,
    pub DxgkDdiQueryDeviceDescriptor: PDXGKDDI_QUERY_DEVICE_DESCRIPTOR,
    pub DxgkDdiSetPowerState: PDXGKDDI_SET_POWER_STATE,
    pub DxgkDdiNotifyAcpiEvent: PDXGKDDI_NOTIFY_ACPI_EVENT,
    pub DxgkDdiResetDevice: PDXGKDDI_RESET_DEVICE,
    pub DxgkDdiUnload: PDXGKDDI_UNLOAD,
    pub DxgkDdiQueryInterface: PDXGKDDI_QUERY_INTERFACE,
    pub DxgkDdiControlEtwLogging: PDXGKDDI_CONTROL_ETW_LOGGING,
    pub DxgkDdiQueryAdapterInfo: PDXGKDDI_QUERYADAPTERINFO,
    pub DxgkDdiCreateDevice: PDXGKDDI_CREATEDEVICE,
    pub DxgkDdiCreateAllocation: PDXGKDDI_CREATEALLOCATION,
    pub DxgkDdiDestroyAllocation: PDXGKDDI_DESTROYALLOCATION,
    pub DxgkDdiDescribeAllocation: PDXGKDDI_DESCRIBEALLOCATION,
    pub DxgkDdiGetStandardAllocationDriverData: PDXGKDDI_GETSTANDARDALLOCATIONDRIVERDATA,
    pub DxgkDdiAcquireSwizzlingRange: PDXGKDDI_ACQUIRESWIZZLINGRANGE,
    pub DxgkDdiReleaseSwizzlingRange: PDXGKDDI_RELEASESWIZZLINGRANGE,
    pub DxgkDdiPatch: PDXGKDDI_PATCH,
    pub DxgkDdiSubmitCommand: PDXGKDDI_SUBMITCOMMAND,
    pub DxgkDdiPreemptCommand: PDXGKDDI_PREEMPTCOMMAND,
    pub DxgkDdiBuildPagingBuffer: PDXGKDDI_BUILDPAGINGBUFFER,
    pub DxgkDdiSetPalette: PDXGKDDI_SETPALETTE,
    pub DxgkDdiSetPointerPosition: PDXGKDDI_SETPOINTERPOSITION,
    pub DxgkDdiSetPointerShape: PDXGKDDI_SETPOINTERSHAPE,
    pub DxgkDdiResetFromTimeout: PDXGKDDI_RESETFROMTIMEOUT,
    pub DxgkDdiRestartFromTimeout: PDXGKDDI_RESTARTFROMTIMEOUT,
    pub DxgkDdiEscape: PDXGKDDI_ESCAPE,
    pub DxgkDdiCollectDbgInfo: PDXGKDDI_COLLECTDBGINFO,
    pub DxgkDdiQueryCurrentFence: PDXGKDDI_QUERYCURRENTFENCE,
    pub DxgkDdiIsSupportedVidPn: PDXGKDDI_ISSUPPORTEDVIDPN,
    pub DxgkDdiRecommendFunctionalVidPn: PDXGKDDI_RECOMMENDFUNCTIONALVIDPN,
    pub DxgkDdiEnumVidPnCofuncModality: PDXGKDDI_ENUMVIDPNCOFUNCMODALITY,
    pub DxgkDdiSetVidPnSourceAddress: PDXGKDDI_SETVIDPNSOURCEADDRESS,
    pub DxgkDdiSetVidPnSourceVisibility: PDXGKDDI_SETVIDPNSOURCEVISIBILITY,
    pub DxgkDdiCommitVidPn: PDXGKDDI_COMMITVIDPN,
    pub DxgkDdiUpdateActiveVidPnPresentPath: PDXGKDDI_UPDATEACTIVEVIDPNPRESENTPATH,
    pub DxgkDdiRecommendMonitorModes: PDXGKDDI_RECOMMENDMONITORMODES,
    pub DxgkDdiRecommendVidPnTopology: PDXGKDDI_RECOMMENDVIDPNTOPOLOGY,
    pub DxgkDdiGetScanLine: PDXGKDDI_GETSCANLINE,
    pub DxgkDdiStopCapture: PDXGKDDI_STOPCAPTURE,
    pub DxgkDdiControlInterrupt: PDXGKDDI_CONTROLINTERRUPT,
    pub DxgkDdiCreateOverlay: PDXGKDDI_CREATEOVERLAY,
    pub DxgkDdiDestroyDevice: PDXGKDDI_DESTROYDEVICE,
    pub DxgkDdiOpenAllocation: PDXGKDDI_OPENALLOCATIONINFO,
    pub DxgkDdiCloseAllocation: PDXGKDDI_CLOSEALLOCATION,
    pub DxgkDdiRender: PDXGKDDI_RENDER,
    pub DxgkDdiPresent: PDXGKDDI_PRESENT,
    pub DxgkDdiUpdateOverlay: PDXGKDDI_UPDATEOVERLAY,
    pub DxgkDdiFlipOverlay: PDXGKDDI_FLIPOVERLAY,
    pub DxgkDdiDestroyOverlay: PDXGKDDI_DESTROYOVERLAY,
    pub DxgkDdiCreateContext: PDXGKDDI_CREATECONTEXT,
    pub DxgkDdiDestroyContext: PDXGKDDI_DESTROYCONTEXT,
    pub DxgkDdiLinkDevice: PDXGKDDI_LINK_DEVICE,
    pub DxgkDdiSetDisplayPrivateDriverFormat: PDXGKDDI_SETDISPLAYPRIVATEDRIVERFORMAT,
    pub DxgkDdiDescribePageTable: PVOID,
    pub DxgkDdiUpdatePageTable: PVOID,
    pub DxgkDdiUpdatePageDirectory: PVOID,
    pub DxgkDdiMovePageDirectory: PVOID,
    pub DxgkDdiSubmitRender: PVOID,
    pub DxgkDdiCreateAllocation2: PVOID,
    pub DxgkDdiRenderKm: PDXGKDDI_RENDER,
    pub Reserved: *mut ::core::ffi::c_void,
    pub DxgkDdiQueryVidPnHWCapability: PDXGKDDI_QUERYVIDPNHWCAPABILITY,
    pub DxgkDdiSetPowerComponentFState: PDXGKDDISETPOWERCOMPONENTFSTATE,
    pub DxgkDdiQueryDependentEngineGroup: PDXGKDDI_QUERYDEPENDENTENGINEGROUP,
    pub DxgkDdiQueryEngineStatus: PDXGKDDI_QUERYENGINESTATUS,
    pub DxgkDdiResetEngine: PDXGKDDI_RESETENGINE,
    pub DxgkDdiStopDeviceAndReleasePostDisplayOwnership: PDXGKDDI_STOP_DEVICE_AND_RELEASE_POST_DISPLAY_OWNERSHIP,
    pub DxgkDdiSystemDisplayEnable: PDXGKDDI_SYSTEM_DISPLAY_ENABLE,
    pub DxgkDdiSystemDisplayWrite: PDXGKDDI_SYSTEM_DISPLAY_WRITE,
    pub DxgkDdiCancelCommand: PDXGKDDI_CANCELCOMMAND,
    pub DxgkDdiGetChildContainerId: PDXGKDDI_GET_CHILD_CONTAINER_ID,
    pub DxgkDdiPowerRuntimeControlRequest: PDXGKDDIPOWERRUNTIMECONTROLREQUEST,
    pub DxgkDdiSetVidPnSourceAddressWithMultiPlaneOverlay: PDXGKDDI_SETVIDPNSOURCEADDRESSWITHMULTIPLANEOVERLAY,
    pub DxgkDdiNotifySurpriseRemoval: PDXGKDDI_NOTIFY_SURPRISE_REMOVAL,
    pub DxgkDdiGetNodeMetadata: PDXGKDDI_GETNODEMETADATA,
    pub DxgkDdiSetPowerPState: PDXGKDDISETPOWERPSTATE,
    pub DxgkDdiControlInterrupt2: PDXGKDDI_CONTROLINTERRUPT2,
    pub DxgkDdiCheckMultiPlaneOverlaySupport: PDXGKDDI_CHECKMULTIPLANEOVERLAYSUPPORT,
    pub DxgkDdiCalibrateGpuClock: PDXGKDDI_CALIBRATEGPUCLOCK,
    pub DxgkDdiFormatHistoryBuffer: PDXGKDDI_FORMATHISTORYBUFFER,
    pub DxgkDdiRenderGdi: PDXGKDDI_RENDERGDI,
    pub DxgkDdiSubmitCommandVirtual: PDXGKDDI_SUBMITCOMMANDVIRTUAL,
    pub DxgkDdiSetRootPageTable: PDXGKDDI_SETROOTPAGETABLE,
    pub DxgkDdiGetRootPageTableSize: PDXGKDDI_GETROOTPAGETABLESIZE,
    pub DxgkDdiMapCpuHostAperture: PDXGKDDI_MAPCPUHOSTAPERTURE,
    pub DxgkDdiUnmapCpuHostAperture: PDXGKDDI_UNMAPCPUHOSTAPERTURE,
    pub DxgkDdiCheckMultiPlaneOverlaySupport2: PDXGKDDI_CHECKMULTIPLANEOVERLAYSUPPORT2,
    pub DxgkDdiCreateProcess: PDXGKDDI_CREATEPROCESS,
    pub DxgkDdiDestroyProcess: PDXGKDDI_DESTROYPROCESS,
    pub DxgkDdiSetVidPnSourceAddressWithMultiPlaneOverlay2: PDXGKDDI_SETVIDPNSOURCEADDRESSWITHMULTIPLANEOVERLAY2,
    pub Reserved1: *mut ::core::ffi::c_void,
    pub Reserved2: *mut ::core::ffi::c_void,
    pub DxgkDdiPowerRuntimeSetDeviceHandle: PDXGKDDI_POWERRUNTIMESETDEVICEHANDLE,
    pub DxgkDdiSetStablePowerState: PDXGKDDI_SETSTABLEPOWERSTATE,
    pub DxgkDdiSetVideoProtectedRegion: PDXGKDDI_SETVIDEOPROTECTEDREGION,
    pub DxgkDdiCheckMultiPlaneOverlaySupport3: PDXGKDDI_CHECKMULTIPLANEOVERLAYSUPPORT3,
    pub DxgkDdiSetVidPnSourceAddressWithMultiPlaneOverlay3: PDXGKDDI_SETVIDPNSOURCEADDRESSWITHMULTIPLANEOVERLAY3,
    pub DxgkDdiPostMultiPlaneOverlayPresent: PDXGKDDI_POSTMULTIPLANEOVERLAYPRESENT,
    pub DxgkDdiValidateUpdateAllocationProperty: PDXGKDDI_VALIDATEUPDATEALLOCATIONPROPERTY,
    pub DxgkDdiControlModeBehavior: PDXGKDDI_CONTROLMODEBEHAVIOR,
    pub DxgkDdiUpdateMonitorLinkInfo: PDXGKDDI_UPDATEMONITORLINKINFO,
    pub DxgkDdiCreateHwContext: PDXGKDDI_CREATEHWCONTEXT,
    pub DxgkDdiDestroyHwContext: PDXGKDDI_DESTROYHWCONTEXT,
    pub DxgkDdiCreateHwQueue: PDXGKDDI_CREATEHWQUEUE,
    pub DxgkDdiDestroyHwQueue: PDXGKDDI_DESTROYHWQUEUE,
    pub DxgkDdiSubmitCommandToHwQueue: PDXGKDDI_SUBMITCOMMANDTOHWQUEUE,
    pub DxgkDdiSwitchToHwContextList: PDXGKDDI_SWITCHTOHWCONTEXTLIST,
    pub DxgkDdiResetHwEngine: PDXGKDDI_RESETHWENGINE,
    pub DxgkDdiCreatePeriodicFrameNotification: PDXGKDDI_CREATEPERIODICFRAMENOTIFICATION,
    pub DxgkDdiDestroyPeriodicFrameNotification: PDXGKDDI_DESTROYPERIODICFRAMENOTIFICATION,
    pub DxgkDdiSetTimingsFromVidPn: PDXGKDDI_SETTIMINGSFROMVIDPN,
    pub DxgkDdiSetTargetGamma: PDXGKDDI_SETTARGETGAMMA,
    pub DxgkDdiSetTargetContentType: PDXGKDDI_SETTARGETCONTENTTYPE,
    pub DxgkDdiSetTargetAnalogCopyProtection: PDXGKDDI_SETTARGETANALOGCOPYPROTECTION,
    pub DxgkDdiSetTargetAdjustedColorimetry: PDXGKDDI_SETTARGETADJUSTEDCOLORIMETRY,
    pub DxgkDdiDisplayDetectControl: PDXGKDDI_DISPLAYDETECTCONTROL,
    pub DxgkDdiQueryConnectionChange: PDXGKDDI_QUERYCONNECTIONCHANGE,
    pub DxgkDdiExchangePreStartInfo: PDXGKDDI_EXCHANGEPRESTARTINFO,
    pub DxgkDdiGetMultiPlaneOverlayCaps: PDXGKDDI_GETMULTIPLANEOVERLAYCAPS,
    pub DxgkDdiGetPostCompositionCaps: PDXGKDDI_GETPOSTCOMPOSITIONCAPS,
    pub DxgkDdiUpdateHwContextState: PDXGKDDI_UPDATEHWCONTEXTSTATE,
    pub DxgkDdiCreateProtectedSession: PDXGKDDI_CREATEPROTECTEDSESSION,
    pub DxgkDdiDestroyProtectedSession: PDXGKDDI_DESTROYPROTECTEDSESSION,
    pub DxgkDdiSetSchedulingLogBuffer: PDXGKDDI_SETSCHEDULINGLOGBUFFER,
    pub DxgkDdiSetupPriorityBands: PDXGKDDI_SETUPPRIORITYBANDS,
    pub DxgkDdiNotifyFocusPresent: PDXGKDDI_NOTIFYFOCUSPRESENT,
    pub DxgkDdiSetContextSchedulingProperties: PDXGKDDI_SETCONTEXTSCHEDULINGPROPERTIES,
    pub DxgkDdiSuspendContext: PDXGKDDI_SUSPENDCONTEXT,
    pub DxgkDdiResumeContext: PDXGKDDI_RESUMECONTEXT,
    pub DxgkDdiSetVirtualMachineData: PDXGKDDI_SETVIRTUALMACHINEDATA,
    pub DxgkDdiBeginExclusiveAccess: PDXGKDDI_BEGINEXCLUSIVEACCESS,
    pub DxgkDdiEndExclusiveAccess: PDXGKDDI_ENDEXCLUSIVEACCESS,
    pub DxgkDdiQueryDiagnosticTypesSupport: PDXGKDDI_QUERYDIAGNOSTICTYPESSUPPORT,
    pub DxgkDdiControlDiagnosticReporting: PDXGKDDI_CONTROLDIAGNOSTICREPORTING,
    pub DxgkDdiResumeHwEngine: PDXGKDDI_RESUMEHWENGINE,
    pub DxgkDdiSignalMonitoredFence: PDXGKDDI_SIGNALMONITOREDFENCE,
    pub DxgkDdiPresentToHwQueue: PDXGKDDI_PRESENTTOHWQUEUE,
    pub DxgkDdiValidateSubmitCommand: PDXGKDDI_VALIDATESUBMITCOMMAND,
    pub DxgkDdiSetTargetAdjustedColorimetry2: PDXGKDDI_SETTARGETADJUSTEDCOLORIMETRY2,
    pub DxgkDdiSetTrackedWorkloadPowerLevel: PDXGKDDI_SETTRACKEDWORKLOADPOWERLEVEL,
    pub DxgkDdiSaveMemoryForHotUpdate: PDXGKDDI_SAVEMEMORYFORHOTUPDATE,
    pub DxgkDdiRestoreMemoryForHotUpdate: PDXGKDDI_RESTOREMEMORYFORHOTUPDATE,
    pub DxgkDdiCollectDiagnosticInfo: PDXGKDDI_COLLECTDIAGNOSTICINFO,
    pub Reserved3: *mut ::core::ffi::c_void,
    pub DxgkDdiControlInterrupt3: PDXGKDDI_CONTROLINTERRUPT3,
    pub DxgkDdiSetFlipQueueLogBuffer: PDXGKDDI_SETFLIPQUEUELOGBUFFER,
    pub DxgkDdiUpdateFlipQueueLog: PDXGKDDI_UPDATEFLIPQUEUELOG,
    pub DxgkDdiCancelQueuedFlips: PDXGKDDI_CANCELQUEUEDFLIPS,
    pub DxgkDdiSetInterruptTargetPresentId: PDXGKDDI_SETINTERRUPTTARGETPRESENTID,
    pub DxgkDdiSetAllocationBackingStore: PDXGKDDI_SETALLOCATIONBACKINGSTORE,
    pub DxgkDdiCreateCpuEvent: PDXGKDDI_CREATECPUEVENT,
    pub DxgkDdiDestroyCpuEvent: PDXGKDDI_DESTROYCPUEVENT,
    pub DxgkDdiCancelFlips: PDXGKDDI_CANCELFLIPS,
    pub DxgkDdiCreateNativeFence: PDXGKDDI_CREATENATIVEFENCE,
    pub DxgkDdiDestroyNativeFence: PDXGKDDI_DESTROYNATIVEFENCE,
    pub DxgkDdiUpdateMonitoredValues: PDXGKDDI_UPDATEMONITOREDVALUES,
    pub DxgkDdiUpdateCurrentValuesFromCpu: PDXGKDDI_UPDATECURRENTVALUESFROMCPU,
    pub DxgkDdiCreateDoorbell: PDXGKDDI_CREATEDOORBELL,
    pub DxgkDdiConnectDoorbell: PDXGKDDI_CONNECTDOORBELL,
    pub DxgkDdiDisconnectDoorbell: PDXGKDDI_DISCONNECTDOORBELL,
    pub DxgkDdiDestroyDoorbell: PDXGKDDI_DESTROYDOORBELL,
    pub DxgkDdiNotifyWorkSubmission: PDXGKDDI_NOTIFYWORKSUBMISSION,
    pub Reserved4: *mut ::core::ffi::c_void,
    pub DxgkDdiCreateMemoryBasis: PDXGKDDI_CREATEMEMORYBASIS,
    pub DxgkDdiDestroyMemoryBasis: PDXGKDDI_DESTROYMEMORYBASIS,
    pub DxgkDdiStartDirtyTracking: PDXGKDDI_STARTDIRTYTRACKING,
    pub DxgkDdiStopDirtyTracking: PDXGKDDI_STOPDIRTYTRACKING,
    pub DxgkDdiQueryDirtyBitData: PDXGKDDI_QUERYDIRTYBITDATA,
    pub DxgkDdiPrepareLiveMigration: PDXGKDDI_PREPARELIVEMIGRATION,
    pub DxgkDdiSaveImmutableMigrationData: PDXGKDDI_SAVEIMMUTABLEMIGRATIONDATA,
    pub DxgkDdiSaveMutableMigrationData: PDXGKDDI_SAVEMUTABLEMIGRATIONDATA,
    pub DxgkDdiEndLiveMigration: PDXGKDDI_ENDLIVEMIGRATION,
    pub DxgkDdiRestoreImmutableMigrationData: PDXGKDDI_RESTOREIMMUTABLEMIGRATIONDATA,
    pub DxgkDdiRestoreMutableMigrationData: PDXGKDDI_RESTOREMUTABLEMIGRATIONDATA,
    pub DxgkDdiWriteVirtualizedInterrupt: PDXGKDDI_WRITEVIRTUALIZEDINTERRUPT,
    pub DxgkDdiSetVirtualGpuResources2: PDXGKDDI_SETVIRTUALGPURESOURCES2,
    pub DxgkDdiSetVirtualFunctionPauseState: PDXGKDDI_SETVIRTUALFUNCTIONPAUSESTATE,
    pub DxgkDdiOpenNativeFence: PDXGKDDI_OPENNATIVEFENCE,
    pub DxgkDdiCloseNativeFence: PDXGKDDI_CLOSENATIVEFENCE,
    pub DxgkDdiSetNativeFenceLogBuffer: PDXGKDDI_SETNATIVEFENCELOGBUFFER,
    pub DxgkDdiUpdateNativeFenceLogs: PDXGKDDI_UPDATENATIVEFENCELOGS,
    pub DxgkDdiCollectDbgInfo2: PDXGKDDI_COLLECTDBGINFO2,
    pub DxgkDdiNotifyContextPriorityChange: PDXGKDDI_NOTIFYCONTEXTPRIORITYCHANGE,
    pub DxgkDdiResetDisplayEngine: PDXGKDDI_RESETDISPLAYENGINE,
}
```
Step-2-critical field names confirmed present: `DxgkDdiQueryAdapterInfo`, `DxgkDdiCreateDevice`, `DxgkDdiCreateAllocation`, `DxgkDdiDescribeAllocation`, `DxgkDdiGetStandardAllocationDriverData`, `DxgkDdiBuildPagingBuffer`, `DxgkDdiPatch`, `DxgkDdiSubmitCommand`, `DxgkDdiPreemptCommand`, `DxgkDdiRender`/`DxgkDdiRenderKm`, `DxgkDdiQueryCurrentFence`, `DxgkDdiSignalMonitoredFence`, `DxgkDdiCreateContext`/`DxgkDdiDestroyContext`, `DxgkDdiOpenAllocation`/`DxgkDdiCloseAllocation`, `DxgkDdiInterruptRoutine`, `DxgkDdiDpcRoutine`, `DxgkDdiControlInterrupt`/`…2`/`…3`, `DxgkDdiEscape`, `DxgkDdiCreateNativeFence`/`DxgkDdiDestroyNativeFence`/`DxgkDdiUpdateMonitoredValues`. Note `DxgkDdiCreateAllocation2` is a bare `PVOID` slot (`dxgk_bindings_dump.rs:95265`), and `DxgkDdiDescribePageTable`/`UpdatePageTable`/`UpdatePageDirectory`/`MovePageDirectory`/`SubmitRender` are all `PVOID` (`:95260-95264`) — the GpuMmu page-table DDIs are deliberately untyped-pointer placeholders in this bindgen, consistent with the "decorative GpuMmu" model.

**Relevant Step-2 fn-pointer typedefs (verbatim) for the fence/interrupt/submit path:**
- `PDXGKDDI_SIGNALMONITOREDFENCE = Option<unsafe extern "C" fn(arg1: IN_CONST_HANDLE, arg2: INOUT_PDXGKARG_SIGNALMONITOREDFENCE) -> NTSTATUS>` (`dxgk_bindings_dump.rs:88926`).
- The interrupt/DPC routine and submit DDI fn-pointer typedefs (`PDXGKDDI_INTERRUPT_ROUTINE`, `PDXGKDDI_DPC_ROUTINE`, `PDXGKDDI_SUBMITCOMMAND`, `PDXGKDDI_PATCH`, `PDXGKDDI_RENDER`, `PDXGKDDI_QUERYCURRENTFENCE`) are referenced here as field types; their full signatures live elsewhere in the dump (search `pub type PDXGKDDI_<NAME>` to pull them in Step 2 — not all were read in this pass).

## Section C — Coherence design (fences, readback, residency)

This section is the integrated design for the three coherence points the locked model requires. **C.1 (the fence) is the orchestrator's synthesis + decision** — it builds on the contract, types, viogpu3d template, and architectural-mismatch analysis in Section A6, and chooses a path. **C.2 (readback)** and **C.3 (residency)** are the detailed, code-grounded designs (the CPU-pointer path, cache coherency, the pin/over-size/no-evict reasoning), carried verbatim from the venus-mapping research; they are the same material Section D builds on, presented here under their coherence framing.

> **Where the detail lives:** the exact bindgen types for `DXGKARG_SUBMITCOMMAND` / `DXGKARGCB_NOTIFY_INTERRUPT_DATA` / the `DmaCompleted` member, the verbatim viogpu3d wiring (`SubmitCommand` → worker thread → virtio used-ring completion → `DxgkCbNotifyInterrupt(DMA_COMPLETED)`), and the current `submit_command.rs` immediate-fence code are all in **Section A6** (read A6.4–A6.9 alongside this). C.1 does not repeat them; it decides what to build.

### C.1 The fence path — the decision (synthesis of A6)

**The problem, stated once.** For DWM to composite correctly on Helios, a WDDM fence `N` must signal **only after** the venus work associated with `N` has actually completed on the host GPU. Today it does the opposite: `dxgkddi_submit_command` (`submit_command.rs:26–65`) stores `last_completed_fence = SubmissionFenceId` and calls `DxgkCbNotifyInterrupt(DMA_COMPLETED, N)` **synchronously, inside the SubmitCommand call, with no GPU/venus work in between** (A6.7). It is a null engine that exists only to keep dxgkrnl's paging path from timing out. And the venus stream that actually does the work flows through a **different, disjoint path** — `D3DKMTEscape → HELIOS_ESCAPE_SUBMIT_VENUS` (`escape.rs:117–149`), whose fence id lives in the venus/ICD namespace and is invisible to dxgkrnl (A6.9). The two paths never meet, so the WDDM scheduler believes every submission completes instantly.

**The template (A6.6).** viogpu3d shows the correct shape end-to-end: `DxgkDdiSubmitCommand` only *queues* the command and returns `STATUS_SUCCESS`; a worker thread hands the bytes to the virtio control queue with a completion callback; the **virtio used-ring interrupt** (real host completion) drives the DPC, which pops the ring and, at the tail of the command's `Run()`, fills `DXGKARGCB_NOTIFY_INTERRUPT_DATA { InterruptType = DXGK_INTERRUPT_DMA_COMPLETED; DmaCompleted.SubmissionFenceId = m_FenceId }` and calls `DxgkCbNotifyInterrupt` (at DIRQL, via `DxgkCbSynchronizeExecution`) then `DxgkCbQueueDpc`. The WDDM fence therefore advances only after the host finished — exactly Helios's requirement, and exactly the model the System-class driver already proved with its async SUBMIT_VENUS + in-flight pool + used-ring drain (`phase4e-async-submit`).

**Decision — adopt Option 1: route the desktop/composition venus stream through `DxgkDdiRender` + `DxgkDdiSubmitCommand`, not Escape.** Of the two reconciliation options in A6.9, Option 1 (single WDDM submission point) is recommended over Option 2 (bridge two fence namespaces) for these reasons:

- **It is the model the codebase already intends.** `protocol/src/wddm.rs:88–94` documents the `D3DKMTRender` command-buffer path with `HeliosWddmCmdBuf` heading the buffer and *"the authoritative GPU fence is WDDM's `SubmissionFenceId`."* The wire contract for this path already exists and is size-asserted (Section D). Option 1 finishes a design that is half-built; Option 2 is a parallel bookkeeping layer that must keep two independently-incremented id spaces aligned with no natural 1:1 mapping — strictly more state and more aliasing risk.
- **It matches viogpu3d 1:1**, so the proven template transfers without translation: `DxgkDdiSubmitCommand` reads `pDmaBufferPrivateData` / the DMA range, forwards the venus stream (carried in the DMA buffer behind `HeliosWddmCmdBuf`) to the virtio control queue *with a completion callback*, returns `STATUS_SUCCESS`; the virtio used-ring interrupt drives the DPC; the DPC signals `DXGK_INTERRUPT_DMA_COMPLETED` with `submit.SubmissionFenceId`.
- **It makes `escape_submit_venus` unnecessary for the desktop path** and removes the dual-namespace problem at the root: there is one fence space (WDDM's), one submission point, one completion source.

**Concrete wiring Step 2 must build (Option 1):**

1. **UMD/ICD side:** stop using `D3DKMTEscape` for *submit* on the composition path; emit the venus byte stream into a WDDM DMA buffer via `DxgkDdiRender` (today `STATUS_NOT_IMPLEMENTED`, `submit_command.rs:87–92`), with `HeliosWddmCmdBuf { ctx_id, ring_idx, venus_offset, venus_size }` at the head (the layout already in `protocol/src/wddm.rs`). Allocation references travel in the `D3DDDI_ALLOCATIONLIST` / patch list (`DXGKARG_PATCH`), not inline. (`DxgkDdiPatch` can stay a no-op `STATUS_SUCCESS` like viogpu3d's — there are no guest GPU-VAs to patch; A6.3.)
2. **`dxgkddi_submit_command` (rewrite):** stop completing synchronously. Read `HeliosWddmCmdBuf` from the DMA buffer; submit the venus stream to the host via the virtio control queue **asynchronously**, tagging the in-flight entry with `submit.SubmissionFenceId`; return `STATUS_SUCCESS`. Delete the immediate `last_completed_fence.store` + `NotifyInterrupt` (`submit_command.rs:40,48–61`). Honor the `Flags.Paging` bit (bit 0, A6.2): a paging buffer (`hDevice == NULL`) carries no venus stream — for it, either complete immediately (it is a true null engine for the decorative model) or run it through the same async pipe with no host work; either way its fence must still be signaled.
3. **`dxgkddi_interrupt_routine` (make real, `interrupt.rs:13–19`):** claim the virtio-gpu MSI-X interrupt, record the ISR reason, call `DxgkCbQueueDpc`. (Today returns FALSE always.)
4. **`dxgkddi_dpc_routine` (make real, `interrupt.rs:22–24`):** drain the virtio used ring; for each completed in-flight entry, fill `DXGKARGCB_NOTIFY_INTERRUPT_DATA` with `DmaCompleted.SubmissionFenceId = <that entry's fence>`, `NodeOrdinal = 0`, `EngineOrdinal = 0`, and call `DxgkCbNotifyInterrupt` then `DxgkCbNotifyDpc`. Update `last_completed_fence` here (so `dxgkddi_query_current_fence` stays correct, `submit_command.rs:111–131`). Use the exact `interrupt.__bindgen_anon_1.DmaCompleted.as_mut()` accessor the current code already uses correctly (A6.7).
5. **IRQL discipline:** `DxgkCbNotifyInterrupt` must run at DIRQL — either call it from the ISR, or (viogpu3d's portable approach) from a `DxgkCbSynchronizeExecution` callback that raises to the interrupt's IRQL (A6.6, `viogpu_adapter.cpp:50–72`). The DPC drain itself runs at DISPATCH_LEVEL.

**Fallback — Option 2 (bridge the namespaces), only if routing through Render proves infeasible.** Keep venus on Escape, but: make `submit_venus` asynchronous (don't block on the used ring); on `escape_submit_venus`, record a mapping `venus_fence_id → pending WDDM SubmissionFenceId`; enable the real ISR/DPC; on used-ring completion of a venus fence, look up the paired WDDM fence and `DxgkCbNotifyInterrupt(DMA_COMPLETED, wddmFenceId)`. This still requires deleting the synchronous fence in `dxgkddi_submit_command` and making the ISR/DPC real — i.e. it shares most of Option 1's work but adds a fragile id-pairing layer. Recommended only as a contingency.

**Invariant to preserve (CLAUDE.md:143, A6.8):** *"Venus commands must be flushed before signaling the fence (ordering)."* Under either option, `DxgkCbNotifyInterrupt(DMA_COMPLETED, N)` must not fire until the venus stream for fence `N` has round-tripped to the host. Note that the *current* `escape_submit_venus` is *synchronous* (it blocks on the used ring, `escape.rs:152–156`), which is accidentally coherent for the Escape path but defeats overlap and does not help the WDDM scheduler fence; Option 1 replaces this with proper async completion so the GPU and CPU can overlap while the WDDM fence still reflects true completion.

---

### C.2 / C.3 — Readback and residency (code-grounded design)

The two subsections that follow are the detailed designs for the other two coherence points — the CPU-pointer / readback path (C-readback) and the residency / no-eviction design (C-residency) — grounded in the current `kmd_render` venus-blob plumbing and the WDDM residency docs. The same venus-blob machinery underpins **Section D** (the allocation→resource mapping); it is presented once here under its coherence framing, and Section D carries the mapping table that builds on it.

#### C-readback.1 — The two CPU-pointer paths, and why Helios bypasses Lock2

There is **no `DxgkDdiLock` callback in WDDM**. `D3DKMTLock`/`D3DKMTLock2` is serviced by dxgkrnl/VidMm itself from the segment descriptor: VidMm knows the allocation's segment id + segment offset, and if the owning segment is CPU-visible it maps the segment's CPU-visible aperture/physical pages into the calling process and returns the VA. The KMD never sees the Lock call. This is the model viogpu3d relies on (its allocations live in a CPU-visible aperture segment, and Lock returns a VA into MDL-backed system memory pages that the host later TRANSFERs).

Helios cannot use that path, because a CPU-visible **memory** segment whose `CpuTranslatedAddress` points at the host-visible BAR is rejected by VidMm right after `DxgkDdiCreateDevice`. This is recorded verbatim in the current driver at `kmd_render/src/ddi/query_adapter_info.rs:249-262`:

```rust
// Gate 5a Stage 2b finding (2026-06-18): a CPU-visible **memory** segment
// (Aperture=0) whose `CpuTranslatedAddress` points at the host-visible BAR — the
// approach this function briefly carried (.66–.70) — is REJECTED by VidMm right
// after `DxgkDdiCreateDevice` (clean-boot Code 43 / FAILED_POST_START, confirmed
// independent of segment size: tested 8 GiB and a 256 MiB sub-window). The proven
// virtio-gpu WDDM driver (`virtio-research-only-3d/.../viogpu_adapter.cpp`) uses an
// **aperture** segment instead, backing allocations with OS system-memory MDLs via
// `BuildPagingBuffer` MAP_APERTURE_SEGMENT and reaching the host with explicit
// TRANSFER_TO_HOST copies — never a CPU-visible memory segment. A real memory
// segment needs a declared GPU memory model (GpuMmu/IoMmu) we don't yet provide.
```

So Helios produces the CPU pointer **out-of-band**, through `D3DKMTEscape` `HELIOS_ESCAPE_MAP_BLOB`, mapping the host-visible BAR straight into the process with `MmMapLockedPagesSpecifyCache(UserMode)` + `MDL_IO_SPACE`. The Escape map path does not involve VidMm at all — the guest, not VidMm, chooses the window offset, and the host backs exactly that range. This is the "zero-copy BAR" model. (Note: per the LOCKED goal, a future implementation must instead make a real CPU-visible memory segment acceptable so that DWM's own Lock2 calls on the composed primary land on venus-backed pages; the Escape path is the proven mechanism, and Section D below maps it onto the composition allocation kinds.)

---

#### C-readback.2 — The Helios Escape MAP_BLOB path, end to end

**(a) The venus blob machinery (`kmd_render/src/virtio/gpu.rs`).**

The host-visible window is discovered from the SHARED_MEMORY_CFG/HOST_VISIBLE virtio capability (`gpu.rs:62-70`):

```rust
#[derive(Clone, Copy)]
pub struct HostVisibleWindow {
    /// Guest-physical base of the window (BAR base + the cap's offset).
    pub base: u64,
    /// Window length in bytes (== QEMU `hostmem=`).
    pub len: u64,
}
```

`scan_host_visible_window` walks the PCI capability list for the vendor cap of type `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` with `shmid == VIRTIO_GPU_SHM_ID_HOST_VISIBLE`, reads the `virtio_pci_cap64` offset/length, and returns `HostVisibleWindow { base: base + off, len }` (`gpu.rs:112-148`). `VIRTIO_GPU_SHM_ID_HOST_VISIBLE = 1` (`protocol/src/virtio_gpu.rs:91`).

A blob is created with `resource_create_blob` — `create_blob` followed immediately by `ctx_attach_resource` (`gpu.rs:503-524`):

```rust
pub fn resource_create_blob(
    &mut self,
    ctx_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
) -> Result<u32, VirtioError> {
    let resource_id = self.next_resource_id.fetch_add(1, Ordering::Relaxed);
    let mut cmd = VirtioGpuResourceCreateBlob::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB;
    cmd.hdr.ctx_id = ctx_id;
    cmd.resource_id = resource_id;
    cmd.blob_mem = blob_mem;
    cmd.blob_flags = blob_flags;
    cmd.nr_entries = 0;
    cmd.blob_id = blob_id;
    cmd.size = size;
    self.ctrl_roundtrip(bytemuck::bytes_of(&cmd))?;
    self.ctx_attach_resource(ctx_id, resource_id)?;
    Ok(resource_id)
}
```

The two load-bearing facts about the blob shape are stated in its doc comment (`gpu.rs:491-502`): a HOST3D mappable blob with `blob_id == 0` is rejected by the host with `RESP_ERR_UNSPEC` (no venus memory to bind), so `blob_id` must be a real venus mem id from the UMD's `vkAllocateMemory`; and `nr_entries = 0` because **HOST3D blobs are host-backed, so no guest page list follows the command**. This is the structural contrast with viogpu3d (Section D).

`alloc_blob` wraps `resource_create_blob` and records a `BlobSlot` so a later MAP_BLOB can size the MDL (`gpu.rs:528-555`):

```rust
pub fn alloc_blob(
    &mut self,
    ctx_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
    owner: usize,
) -> Result<u32, VirtioError> {
    ...
    let resource_id = self.resource_create_blob(ctx_id, blob_mem, blob_flags, blob_id, size)?;
    self.blobs.push(BlobSlot {
        owner,
        ctx_id,
        resource_id,
        size,
        mapped: false,
        map_offset: 0,
        map_len: 0,
    });
    Ok(resource_id)
}
```

`BlobSlot` (`gpu.rs:189-208`) carries `owner: usize` (the `DXGKARG_ESCAPE.hDevice` as an opaque usize, used for device-teardown reclamation), `ctx_id`, `resource_id`, `size`, `mapped`, `map_offset`, `map_len`. The table is `blobs: Vec<BlobSlot>` reserved to `MAX_BLOBS = 256` at init so `push` under the spinlock never reallocates (`Vec::with_capacity(MAX_BLOBS)` at `gpu.rs:365`; `MAX_BLOBS = 256` const at `gpu.rs:160-167`).

The window-offset allocator is a bump high-water + coalescing free list. `next_window_offset: u64` is the bump pointer and `free_window_ranges: Vec<WindowRange>` (reserved to `MAX_WINDOW_RANGES = 64`) is the free list (`gpu.rs:210-215`, `gpu.rs:253-256`). The allocator `alloc_window_range` reuses a free range if one fits, else bumps the high-water mark, bounded by `window_len` (`gpu.rs:675-693`):

```rust
fn alloc_window_range(&mut self, len: u64, window_len: u64) -> Result<u64, VirtioError> {
    if let Some(idx) = self.free_window_ranges.iter().position(|r| r.len >= len) {
        let offset = self.free_window_ranges[idx].offset;
        ...
        return Ok(offset);
    }
    let offset = self.next_window_offset;
    let end = offset.checked_add(len).ok_or(VirtioError::OutOfMemory)?;
    if end > window_len {
        return Err(VirtioError::OutOfMemory);
    }
    self.next_window_offset = end;
    Ok(offset)
}
```

`map_blob_prepare` is the under-lock (DISPATCH_LEVEL) phase: it looks up the blob's `size`, page-rounds it (`round_up_page`, `BLOB_PAGE = 4096`, `gpu.rs:159`/`gpu.rs:170-172`), allocates a window offset, issues `RESOURCE_MAP_BLOB` and returns `BlobMapPrep { gpa: window.base + offset, size: map_len, map_cache }` (`gpu.rs:562-596`). The crucial design note (`gpu.rs:557-561`): *"The guest chooses the window offset, so VidMm is never involved — the host backs exactly the `host_visible.base + offset` range we report back."*

```rust
pub fn map_blob_prepare(&mut self, resource_id: u32) -> Result<BlobMapPrep, VirtioError> {
    let window = self.host_visible.ok_or(VirtioError::DeviceError)?;
    let size = self.blobs.iter().find(|s| s.resource_id == resource_id).map(|s| s.size)...?;
    let map_len = round_up_page(size);
    if map_len == 0 || map_len > MAX_BLOB_MAP_BYTES { return Err(...); }
    let offset = self.alloc_window_range(map_len, window.len)?;
    let map_cache = match self.resource_map_blob(resource_id, offset) { ... };
    ...
    Ok(BlobMapPrep { gpa: window.base + offset, size: map_len, map_cache })
}
```

`resource_map_blob` sends `VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB` at the chosen `offset` and returns the host caching nibble masked with `VIRTIO_GPU_MAP_CACHE_MASK` (= `0x0f`) (`gpu.rs:779-786`, `protocol/src/virtio_gpu.rs:106`). `map_blob_roundtrip` reads the `RESP_OK_MAP_INFO` reply's `map_info` word (`gpu.rs:799-823`). The per-map cap `MAX_BLOB_MAP_BYTES = 256 << 20` also bounds the `IoAllocateMdl` ULONG length downstream (`gpu.rs:167`).

`BlobMapPrep` (`gpu.rs:178-186`): `pub gpa: u64`, `pub size: u64`, `pub map_cache: u32`.

**(b) The user-VA mapping primitive (`kmd_render/src/ddi/blob_map.rs`).**

`map_io_pages_to_user(gpa, size, cache)` builds an MDL over the guest-physical BAR range and maps it into the current process. The two flags are load-bearing (`blob_map.rs:26-39`, `blob_map.rs:95`):

- `MDL_PAGES_LOCKED = 0x0002` — device BAR pages are inherently non-pageable.
- `MDL_IO_SPACE = 0x0800` — the frames are PCI-BAR pages above guest RAM with **no PFN-database entry**; without this flag `MmMapLockedPagesSpecifyCache` would index `MmPfnDatabase[pfn]` → wild access. The flag tells MM to build user PTEs straight from the PFN array.

```rust
let mdl = IoAllocateMdl(core::ptr::null_mut(), size as ULONG, 0, 0, core::ptr::null_mut());
if mdl.is_null() { return None; }
(*mdl).MdlFlags |= MDL_PAGES_LOCKED | MDL_IO_SPACE;
// The PFN array immediately follows the MDL header.
let pfns = (mdl as *mut u8).add(size_of::<MDL>()) as *mut u64;
let pages = (size >> PAGE_SHIFT) as usize;
let pfn0 = gpa >> PAGE_SHIFT;
for i in 0..pages { *pfns.add(i) = pfn0 + i as u64; }
let priority = NORMAL_PAGE_PRIORITY | MDL_MAPPING_NO_EXECUTE;
let va = MmMapLockedPagesSpecifyCache(mdl, USER_MODE, cache, core::ptr::null_mut(), 0, priority);
```

(`blob_map.rs:76-116`). `PAGE_SHIFT = 12`, `NORMAL_PAGE_PRIORITY = 16`, `MDL_MAPPING_NO_EXECUTE = 0x4000_0000`, `USER_MODE = 1` (`blob_map.rs:23-39`). There is a documented hardening TODO: for UserMode, `MmMapLockedPagesSpecifyCache` **RAISES on failure rather than returning NULL**, and this `no_std` crate has no SEH, so a failure unwinds → bugcheck; exposure is bounded by the per-map cap and the trusted-ICD-only caller (`blob_map.rs:71-75`).

**Cache coherency** is driven by the host's `map_info` nibble. `map_cache_to_mm` (`blob_map.rs:42-49`) translates the virtio nibble to a Windows cache type:

```rust
pub fn map_cache_to_mm(map_cache: u32) -> _MEMORY_CACHING_TYPE::Type {
    match map_cache {
        VIRTIO_GPU_MAP_CACHE_CACHED => _MEMORY_CACHING_TYPE::MmCached,
        VIRTIO_GPU_MAP_CACHE_WC => _MEMORY_CACHING_TYPE::MmWriteCombined,
        VIRTIO_GPU_MAP_CACHE_UNCACHED => _MEMORY_CACHING_TYPE::MmNonCached,
        _ => _MEMORY_CACHING_TYPE::MmNonCached,
    }
}
```

The cache nibble constants (`protocol/src/virtio_gpu.rs:106-110`): `VIRTIO_GPU_MAP_CACHE_MASK = 0x0f`, `VIRTIO_GPU_MAP_CACHE_NONE = 0x00`, `VIRTIO_GPU_MAP_CACHE_CACHED = 0x01`, `VIRTIO_GPU_MAP_CACHE_UNCACHED = 0x02`, `VIRTIO_GPU_MAP_CACHE_WC = 0x03`. `effective_map_cache(requested, host)` honors the ICD's request if it is one of CACHED/WC/UNCACHED, else falls back to the host's (`blob_map.rs:52-59`). This is the **only** coherency lever in the Helios CPU-readback path: the host (virglrenderer/venus) declares whether the mapped BAR pages are cached/WC/uncached, and the guest maps them with the matching `MEMORY_CACHING_TYPE` so CPU loads/stores of the composed-primary blob stay coherent with what the host GPU wrote. (Per MEMORY.md, the cache-coherency fix for host-visible blobs is exactly this; a `MmCached` mapping requires the host to perform clflush sweeps, a `MmWriteCombined` mapping does not but needs an SFENCE before the host reads.)

**(c) The owner-tagged mapping registry (`kmd_render/src/mapping.rs`).**

Each successful user map is recorded in `MappingTable` so it is torn down in the creating process before that process exits, else the kernel bugchecks `0x76 PROCESS_HAS_LOCKED_PAGES` (`mapping.rs:1-8`). A `Mapping` (`mapping.rs:44-57`) holds `owner: usize` (the owning handle), `resource_id: u32`, `user_va: u64`, and `mdl: usize` (the `*mut MDL` as an opaque token). `MappingTable` (`mapping.rs:60-67`) is `{ lock: UnsafeCell<KSPIN_LOCK>, entries: UnsafeCell<Vec<Mapping>> }`, reserved to `MAX_MAPPINGS = 256` (`mapping.rs:41`, `mapping.rs:78-83`). Methods: `contains(resource_id)` for the double-map guard (`mapping.rs:87-95`); `insert(owner, resource_id, user_va, mdl)` returning false at capacity without allocating (`mapping.rs:102-119`); `take_one_for(owner)` for per-handle cleanup loops (`mapping.rs:127-137`); `take_for_resource(owner, resource_id)` for explicit release while alive (`mapping.rs:141-154`).

**(d) The Escape dispatch + owner tagging (`kmd_render/src/ddi/escape.rs`).**

`dxgkddi_escape` validates the `HeliosEscapeHeader`, derives `owner = args.hDevice as usize` (`escape.rs:64`), and dispatches the blob verbs:

```rust
HELIOS_ESCAPE_ALLOC_BLOB   => escape_alloc_blob(adapter, buf, owner),
HELIOS_ESCAPE_MAP_BLOB     => escape_map_blob(adapter, buf, owner),
HELIOS_ESCAPE_RELEASE_BLOB => escape_release_blob(adapter, buf, owner),
```

(`escape.rs:71-73`). The owner-tagging rationale (`escape.rs:60-64`): dxgkrnl passes the DeviceContext handle returned from `DxgkDdiCreateDevice` as `hDevice`, and the same handle to `DxgkDdiDestroyDevice`, so a mapping tagged with it is unmapped at the right time, in the creating process.

`escape_map_blob` (`escape.rs:195-247`) is the full three-phase choreography:
- **Phase 1 (under virtio spinlock, DISPATCH):** reject a duplicate map via `adapter.mappings.contains(req.resource_id)`; call `map_blob_prepare(req.resource_id)` to run `RESOURCE_MAP_BLOB` and get `prep` (gpa/size/map_cache).
- **Phase 2 (PASSIVE_LEVEL, caller's process, no lock):** `eff_cache = effective_map_cache(req.map_cache, prep.map_cache)`, `cache = map_cache_to_mm(eff_cache)`, then `map_io_pages_to_user(prep.gpa, prep.size, cache)` → `(user_va, mdl)`.
- **Phase 3:** `adapter.mappings.insert(owner, req.resource_id, user_va, mdl as usize)`; on table-full, immediately `unmap_io_pages_from_user(user_va, mdl)`. Then write `out.out_user_va = user_va` and `out.map_cache = eff_cache` back into the in/out buffer.

`escape_release_blob` (`escape.rs:251-270`) unmaps this device's user view via `take_for_resource(owner, req.resource_id)` then `release_blob(ctx_id, resource_id)` (which itself unmaps + detaches + unrefs, `gpu.rs:600-615`).

**(e) The alternate KMD path: `DxgkDdiCreateAllocation` (`kmd_render/src/ddi/create_allocation.rs`).**

When an allocation is created through the WDDM-native path (`D3DKMTCreateAllocation`), `create_one` reads the per-allocation `HeliosWddmAllocPrivate`, validates its magic/version, and creates the backing blob with the same `resource_create_blob` (`create_allocation.rs:62-130`):

```rust
let resource_id = match adapter
    .with_virtio(|v| v.resource_create_blob(ap.ctx_id, ap.blob_mem, ap.blob_flags, ap.blob_id, ap.size))
{ ... };
```

It then fills the VidMm metadata declaring the allocation a **CPU-visible blob in segment 1, pinned (never evicted)** (`create_allocation.rs:113-129`):

```rust
info.hAllocation = Box::into_raw(ctx) as HANDLE;
info.Size = size;
info.PitchAlignedSize = size;
info.SupportedWriteSegmentSet = 1; // segment id 1 (bit 0)
info.EvictionSegmentSet = 0; // host-visible blob is pinned; never evicted
unsafe {
    info.__bindgen_anon_1.Alignment = PAGE as UINT;
    info.__bindgen_anon_2.SupportedReadSegmentSet = 1;
    info.__bindgen_anon_3.MaximumRenamingListLength = 0;
    info.__bindgen_anon_4
        .FlagsWddm2
        .__bindgen_anon_1
        .__bindgen_anon_1
        .set_CpuVisible(1);
}
```

The exact `DXGK_ALLOCATIONINFO` fields these touch, verbatim from the bindgen dump (`dxgk_bindings_dump.rs:63745-63762`):

```rust
pub struct _DXGK_ALLOCATIONINFO {
    pub pPrivateDriverData: *mut ::core::ffi::c_void,
    pub PrivateDriverDataSize: UINT,
    pub __bindgen_anon_1: _DXGK_ALLOCATIONINFO__bindgen_ty_1,   // union { Alignment: UINT, .. }
    pub Size: SIZE_T,
    pub PitchAlignedSize: SIZE_T,
    pub HintedBank: DXGK_SEGMENTBANKPREFERENCE,
    pub PreferredSegment: DXGK_SEGMENTPREFERENCE,
    pub __bindgen_anon_2: _DXGK_ALLOCATIONINFO__bindgen_ty_2,   // union { SupportedReadSegmentSet: UINT, MmuSet: UINT }
    pub SupportedWriteSegmentSet: UINT,
    pub EvictionSegmentSet: UINT,
    pub __bindgen_anon_3: _DXGK_ALLOCATIONINFO__bindgen_ty_3,   // union { MaximumRenamingListLength: UINT, PhysicalAdapterIndex: UINT }
    pub hAllocation: HANDLE,
    pub __bindgen_anon_4: _DXGK_ALLOCATIONINFO__bindgen_ty_4,   // union { FlagsWddm2: DXGK_ALLOCATIONINFOFLAGS_WDDM2_0, .. }
    pub pAllocationUsageHint: *mut DXGK_ALLOCATIONUSAGEHINT,
    pub AllocationPriority: UINT,
    pub Flags2: DXGK_ALLOCATIONINFOFLAGS2,
}
```

The anonymous unions Step 2 must use exactly as above: `__bindgen_anon_1` = `union { Alignment: UINT, __bindgen_anon_1: { MinimumPageSize: UINT16, RecommendedPageSize: UINT16 } }` (`dxgk_bindings_dump.rs:63763-63774`); `__bindgen_anon_2` = `union { SupportedReadSegmentSet: UINT, MmuSet: UINT }` (`:63817-63822`); `__bindgen_anon_3` = `union { MaximumRenamingListLength: UINT, PhysicalAdapterIndex: UINT }` (`:63849-63854`); `__bindgen_anon_4` holds `FlagsWddm2: DXGK_ALLOCATIONINFOFLAGS_WDDM2_0` (`:63886`).

The `set_CpuVisible(1)` accessor reaches through `FlagsWddm2.__bindgen_anon_1.__bindgen_anon_1`, i.e. `DXGK_ALLOCATIONINFOFLAGS_WDDM2_0` → `__bindgen_ty_1` (union of `{ __bindgen_anon_1, Value: UINT }`) → `__bindgen_ty_1__bindgen_ty_1` (the bitfield struct). The bitfield representation is verbatim (`dxgk_bindings_dump.rs:61221-61227`):

```rust
pub struct _DXGK_ALLOCATIONINFOFLAGS_WDDM2_0__bindgen_ty_1__bindgen_ty_1 {
    pub _bitfield_align_1: [u8; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}
```

with `CpuVisible` at bit 0, `PermanentSysMem` at bit 1, `Cached` at bit 2 (`:61243-61322`):

```rust
pub fn CpuVisible(&self) -> UINT { unsafe { ::core::mem::transmute(self._bitfield_1.get(0usize, 1u8) as u32) } }
pub fn set_CpuVisible(&mut self, val: UINT) { unsafe { let val: u32 = ::core::mem::transmute(val); self._bitfield_1.set(0usize, 1u8, val as u64) } }
pub fn set_PermanentSysMem(&mut self, val: UINT) { ... self._bitfield_1.set(1usize, 1u8, val as u64) }
pub fn set_Cached(&mut self, val: UINT) { ... self._bitfield_1.set(2usize, 1u8, val as u64) }
```

Note for Step 2: `PermanentSysMem` (bit 1) is the bindgen-confirmed flag that maps cleanly onto the "pinned / permanently resident" design below, and `Cached` (bit 2) is the WDDM-native equivalent of the venus `MAP_CACHE_CACHED` nibble. The current driver sets only `CpuVisible`.

The per-allocation `AllocationContext` KMD state (`create_allocation.rs:26-37`): `ctx_id: u32`, `resource_id: u32`, `blob_id: u64`, `size: SIZE_T`, `map_offset: u64`, `map_len: u64`, `mapped: bool` — stashed in `info.hAllocation` via `Box::into_raw` and reclaimed in `destroy_allocation_ctx` (unmap → detach → unref, `create_allocation.rs:47-58`).

**Design (a): producing + keeping coherent a CPU pointer to the composed-primary venus blob.** The composed primary is created as a HOST3D blob backed by a venus `VkDeviceMemory` that is `HOST_VISIBLE` (so the host force-exports it into the host-visible BAR window). The CPU pointer is produced in whichever process needs to read it:
- the **DWM/render** side writes into it as a render target via the venus stream (no CPU map needed for the GPU write);
- the **IDD capture** side (Looking Glass `CSwapChainProcessor`/`CHeliosSink`) gets a CPU pointer via `D3DKMTEscape MAP_BLOB`, which maps `host_visible.base + offset` into that process with the host-declared cache type.

Coherency is anchored at exactly the three points named in the goal: (1) the venus submit fence — the IDD must only read after the WDDM fence that the venus submit drove has signalled (so the host GPU's writes to the blob are complete); (2) CPU/IDD readback — the mapping's `MEMORY_CACHING_TYPE` matches the host's `map_info` nibble (`MmWriteCombined` for streaming readout avoids needing host clflush; `MmCached` requires the host to clflush before the fence signals); (3) residency — the blob is pinned (`EvictionSegmentSet = 0`) so its BAR offset never changes under the live user mapping.

---

#### C-residency.2 — Residency requirements and the over-size-segment justification

**What the docs require.** From `residency-overview.md`:

> "With the introduction of the new residency model, residency is being moved to an explicit list on the device instead of the per-command buffer list. The video memory manager will ensure that all allocations on a particular device residency requirement list are resident before any contexts belonging to that device are scheduled for execution." (`residency-overview.md:19`)

> "**Important**  Residency in the WDDM v2 is controlled exclusively by the device residency requirement list. This is true across all engines of the GPU and for every API." (`residency-overview.md:27`)

The UMD owns residency through `MakeResidentCb`/`EvictCb` and must implement `TrimResidencyCb`, with reference counting (`residency-overview.md:21-23`). `driver-residency-in-wddm-2-0.md` adds that under the new model `TrimResidency` is called when VidMm needs the UMD to reduce its requirement (`residency-overview.md:21`), and that allocation-usage / direct-CPU-access synchronization is now the UMD's responsibility (`driver-residency-in-wddm-2-0.md:17`).

The hard failure mode if residency is violated, from `access-to-non-resident-allocation.md`:

> "GPU access to allocations that aren't resident is illegal. Such access results in a device being removed for the application that generated the error." (`access-to-non-resident-allocation.md:10`)

and for the no-GPU-VA case: "an invalid access occurs when the user-mode driver submits an allocation list that references an allocation that isn't resident ... the graphics kernel puts the faulty context/device in error" (`access-to-non-resident-allocation.md:16`).

`process-residency-budgets.md` explains that eviction pressure is budget-driven and only imposed under memory pressure, surfaced as `Trim` notifications and `MakeResident` failures with `STATUS_NO_MEMORY` / `NumBytesToTrim` (`process-residency-budgets.md:11`). The key budgeting lever it names is the **`ApplicationTarget` bit in `DXGK_SEGMENTFLAGS2`**: "a new **ApplicationTarget** bit ... needs to be set on segments that the kernel mode driver wishes to be included in the budgeting logic" (`process-residency-budgets.md:15`). `locking-memory.md` warns that a large allocation paged to disk can stall the GPU while the scheduler pages it in (`locking-memory.md:18`).

**Why over-sizing the segment to avoid eviction is sound.** The Helios design makes nothing evictable, for three reinforcing reasons grounded in the above:

1. **Pin every allocation.** Each Helios allocation already sets `info.EvictionSegmentSet = 0` ("host-visible blob is pinned; never evicted", `create_allocation.rs:118`). A blob backed by host-visible venus memory has no guest-RAM backing to page out — the bytes physically live in the host-visible BAR window and on the host GPU — so there is nothing for VidMm to evict to. Pinning is therefore not merely an optimization; it is the only correct state for a decorative-GpuMmu, host-owned-MMU model.

2. **Over-size the segment so VidMm never hits a budget that triggers Trim/Evict.** `process-residency-budgets.md` says eviction is "generally only imposed when the system is under memory pressure" (`process-residency-budgets.md:11`). If the declared segment `Size`/`CommitLimit` dwarfs the working set (DWM's composition surfaces + the composed primary + per-app render targets), VidMm's residency requirement list always fits, no `Trim`/`Evict` ever fires, and the device never enters the device-removed state of `access-to-non-resident-allocation.md`. The current `query_segments` already over-provisions a 64 MiB segment (`Size = 64*1024*1024`, `CommitLimit = 64*1024*1024`, `query_adapter_info.rs:305-306`) and `ApertureSegmentCommitLimit = 64*1024*1024` in caps (`query_adapter_info.rs:93`); Step 2 must scale this to comfortably exceed the desktop-composition working set (the host-visible window `HostVisibleWindow.len` from QEMU `hostmem=`, today defaulted at 512 MiB per CLAUDE.md, is the physical ceiling, so the declared segment should be sized to it or just under).

3. **Do NOT set `ApplicationTarget`.** `process-residency-budgets.md:15` is explicit that only segments marked `DXGK_SEGMENTFLAGS2::ApplicationTarget` participate in the budgeting logic the kernel aggregates and presents to apps. Leaving the Helios segment **un-marked** keeps it out of the per-process budget entirely — so apps never see a budget that would prompt them to trim, and VidMm never computes a `NumBytesToTrim` against it. (This is the converse of the discrete-GPU example in the doc, where the primary VRAM segment is the one marked.)

**Soundness caveat (must be flagged for Step 2):** the over-size segment is sound only once VidMm *accepts the segment shape at all*. The bindgen finding in `query_adapter_info.rs:249-262` is that a CPU-visible **memory** segment is rejected post-`CreateDevice` regardless of size (8 GiB and 256 MiB both failed) because there is no declared GpuMmu/IoMmu model — i.e. over-sizing does not, by itself, get past the segment-acceptance gate. The current driver therefore falls back to a small CPU-visible **aperture** segment (`set_CpuVisible(1)` + `set_Aperture(1)`, `Size = 64 MiB`, `query_adapter_info.rs:298-306`) purely to stay at Code 0. The LOCKED goal's "fake-but-coherent GpuMmu" is the missing piece that lets the over-size **memory** segment be accepted; the residency reasoning above (pin + over-size + no-ApplicationTarget) is correct and applies the moment that segment is accepted, but it presupposes the GpuMmu declaration that Sections A2–A4 of this deliverable design. (The open unknown noted in MEMORY.md — "does VidMm accept a decorative GpuMmu" — is the gating question for whether this residency design can run on a memory segment rather than the placeholder aperture segment.)

---

## Section D — Venus mapping (WDDM allocation → venus resource)

How a WDDM allocation becomes a venus resource (a virtio-gpu HOST3D blob): which blob memory type / flags fit composition render targets vs the host-visible composed primary vs the command/staging ring, how the forwarder UMD's venus rendering targets the right resource id, and the decisive contrast with viogpu3d's transfer-queue backing. This is the `D3DKMTCreateAllocation` → `HeliosWddmAllocPrivate` → `resource_create_blob` path, with the wire contract and the per-kind mapping table. (The CPU-pointer/readback mechanics this builds on are in Section C.2.)

---

#### D — WDDM allocation → venus resource (HOST3D blob) mapping

**The wire contract (`protocol/src/wddm.rs`).** Two private-data structs cross the D3DKMT boundary. `HeliosWddmAllocPrivate` (48 bytes, `wddm.rs:41-54`) carries the venus-resource description for `D3DKMTCreateAllocation`:

```rust
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosWddmAllocPrivate {
    pub blob_id: u64,    // in:  venus device-memory id backing the blob (0 = scratch shmem)
    pub size: u64,       // in:  blob size in bytes
    pub magic: u32,      // == HELIOS_WDDM_MAGIC
    pub version: u32,    // == HELIOS_WDDM_VERSION
    pub blob_mem: u32,   // in:  VIRTIO_GPU_BLOB_MEM_* (HOST3D)
    pub blob_flags: u32, // in:  VIRTIO_GPU_BLOB_FLAG_* (USE_MAPPABLE)
    pub ctx_id: u32,     // in:  owning venus context id
    pub map_cache: u32,  // in/out: requested/effective VIRTIO_GPU_MAP_CACHE_*
    pub kind: u32,       // in:  HELIOS_WDDM_ALLOC_KIND_*
    pub _pad: u32,
}
```

The two `kind` values (`wddm.rs:28-33`): `HELIOS_WDDM_ALLOC_KIND_SHMEM = 0` (host-visible command/staging ring blob, `blob_id == 0`); `HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY = 1` (blob bound to a venus `VkDeviceMemory`, `blob_id == venus mem id` from `vkAllocateMemory`). `HELIOS_WDDM_MAGIC = 0x4857_444D` ('HWDM'), `HELIOS_WDDM_VERSION = 1` (`wddm.rs:24-26`).

`HeliosWddmCmdBuf` (32 bytes, `wddm.rs:95-105`) heads a `D3DKMTRender` command buffer; the opaque venus stream begins at `venus_offset` for `venus_size` bytes and is forwarded to `submit_venus(ctx_id, fence, stream)`:

```rust
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosWddmCmdBuf {
    pub seq: u64,          // in:  ICD submission sequence (debug/ordering)
    pub magic: u32,        // == HELIOS_WDDM_MAGIC
    pub version: u32,      // == HELIOS_WDDM_VERSION
    pub ctx_id: u32,       // in:  owning venus context id
    pub ring_idx: u32,     // in:  venus per-queue host timeline (0 = CPU ring)
    pub venus_offset: u32, // in:  byte offset of the venus stream within the command buffer
    pub venus_size: u32,   // in:  venus stream length in bytes
}
```

The doc comment (`wddm.rs:88-94`) makes the fence ownership explicit: *"the authoritative GPU fence is WDDM's `SubmissionFenceId`"* (i.e. the venus submit must drive that fence — Section C-fences). Allocation references travel in the `D3DDDI_ALLOCATIONLIST`/patch list, not inline (`wddm.rs:15-16`). A static assert pins both sizes (`wddm.rs:114-117`): `size_of::<HeliosWddmAllocPrivate>() == 48`, `size_of::<HeliosWddmCmdBuf>() == 32`.

**The venus blob mem/flag constants (`protocol/src/virtio_gpu.rs:94-99`):** `VIRTIO_GPU_BLOB_MEM_GUEST = 1`, `VIRTIO_GPU_BLOB_MEM_HOST3D = 2`, `VIRTIO_GPU_BLOB_MEM_HOST3D_GUEST = 3`; `VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE = 1`, `VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE = 2`, `VIRTIO_GPU_BLOB_FLAG_USE_CROSS_DEVICE = 4`. The venus context capset is `VIRTIO_GPU_CAPSET_VENUS = 4` (`virtio_gpu.rs:83`).

**The mapping table (which WDDM allocation kind → which venus blob mem/flags):**

| WDDM allocation kind (use) | `HeliosWddmAllocPrivate.kind` | `blob_mem` | `blob_flags` | `blob_id` | CPU-mapped? | `map_cache` | Coherence point |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Composition render target / per-app RT / depth (DWM + app render surfaces) | `KIND_DEVICE_MEMORY = 1` | `BLOB_MEM_HOST3D = 2` | `0` (no `USE_MAPPABLE`) — GPU-only, never CPU-read | venus mem id from `vkAllocateMemory` (DEVICE_LOCAL) | No | n/a | fence only (host GPU read after venus submit) |
| Composed primary (the surface the IDD captures) | `KIND_DEVICE_MEMORY = 1` | `BLOB_MEM_HOST3D = 2` | `USE_MAPPABLE = 1` (+ `USE_SHAREABLE = 2` if cross-device IDD capture is wired) | venus mem id from `vkAllocateMemory` of a `HOST_VISIBLE` `VkDeviceMemory` | Yes (IDD via Escape MAP_BLOB) | host `MmWriteCombined` (WC) preferred, or `MmCached` if host clflushes | fence + CPU readback (WC mapping; SFENCE/clflush per cache nibble) |
| Command / staging / venus ring shmem (the venus command stream transport) | `KIND_SHMEM = 0` | `BLOB_MEM_HOST3D = 2` | `USE_MAPPABLE = 1` | `0` (no venus device memory) | Yes (ICD via Escape MAP_BLOB) | `MmCached` or `MmWriteCombined` | CPU producer / host consumer ring |

Notes grounding each row in code:
- The HOST3D-mappable blob with `blob_id == 0` is the only legal shmem case; a HOST3D blob with `blob_id == 0` that is *not* a pure shmem ring is rejected `RESP_ERR_UNSPEC` (`gpu.rs:495-502`). This is why composition RTs and the composed primary must supply a real venus mem id (`kind = 1`).
- `nr_entries = 0` is hard-set in `resource_create_blob` (`gpu.rs:518`) for **every** Helios blob — HOST3D blobs are host-backed, so no guest page list ever follows. This is the structural inverse of viogpu3d (below).
- The KMD reads `ap.blob_mem`/`ap.blob_flags`/`ap.blob_id`/`ap.size`/`ap.ctx_id` straight from `HeliosWddmAllocPrivate` and forwards them unmodified to `resource_create_blob` (`create_allocation.rs:85-86`); the KMD does not interpret `kind` beyond a diagnostic (`create_allocation.rs:84`: `record(0x0C01_0010 | (ap.kind & 0xFF))`). So the *policy* of which mem/flags a given surface uses lives in the ICD, and the KMD is a faithful conduit.

**How the forwarder UMD targets the right resource id.** The forwarder UMD (the DXVK-backed D3D11 UMD, per the Gate 5b memory) creates each D3D resource as a Helios allocation: it allocates a venus `VkDeviceMemory` (producing a venus mem id), packs a `HeliosWddmAllocPrivate { kind, ctx_id, blob_id = venus_mem_id, size, blob_mem = HOST3D, blob_flags, map_cache }` into the `D3DKMTCreateAllocation` per-allocation private data, and the KMD's `DxgkDdiCreateAllocation` binds it to a guest-assigned virtio `resource_id` (`create_allocation.rs:85-98`), stashed in `AllocationContext`. At render time the venus stream the UMD emits references its Vulkan objects by venus handle; the binding "this VkImage/VkBuffer ⇄ this virtio resource_id ⇄ this WDDM allocation handle" is what `ctx_attach_resource` (`gpu.rs:733-743`) establishes per venus context. Because the host GPU owns the real MMU and addresses resources by opaque venus id (the guest GpuMmu/page-table is decorative), the venus stream never needs a guest GPU virtual address — it names the resource by venus id, and the host resolves it. `HeliosWddmCmdBuf.ctx_id`/`ring_idx` route the submit to the matching venus context+timeline so the WDDM `SubmissionFenceId` and the venus fence stay paired (`wddm.rs:88-94`). The composed-primary resource id is the one the IDD then maps for readback via Escape MAP_BLOB.

**Contrast with viogpu3d's transfer-queue backing (`viogpu_allocation.cpp`).** viogpu3d does the exact opposite of zero-copy: it backs every allocation with **OS system-memory MDL pages** and reaches the host with explicit virtio-gpu TRANSFER copies.

- On create it makes a *3D resource* (not a blob) and leaves the backing detached: `m_adapter->ctrlQueue.CreateResource3D(m_Id, options); m_pMDL = NULL;` (`viogpu_allocation.cpp:17-19`). VidMm metadata declares a CPU-visible allocation in segment 1 (`PreferredSegment.SegmentId0 = 1`, `Flags.CpuVisible = TRUE`, `SupportedReadSegmentSet = 0b1`, `SupportedWriteSegmentSet = 0b1`, `EvictionSegmentSet = 1`; `viogpu_allocation.cpp:272-288`).
- The backing is attached **later**, when VidMm maps the allocation into the **aperture segment** via `DxgkDdiBuildPagingBuffer` → `MapApertureSegment` (`viogpu_allocation.cpp:316-330`):

```cpp
NTSTATUS VioGpuAllocation::MapApertureSegment(DXGKARG_BUILDPAGINGBUFFER *pBuildPagingBuffer)
{
    size_t pageCount = pBuildPagingBuffer->MapApertureSegment.NumberOfPages;
    size_t mdlPageOffset = pBuildPagingBuffer->MapApertureSegment.MdlOffset;
    MDL *pMdl = pBuildPagingBuffer->MapApertureSegment.pMdl;
    AttachBacking(pMdl, pageCount, mdlPageOffset);
    SetDxPhysicalAddress(pBuildPagingBuffer->MapApertureSegment.OffsetInPages * PAGE_SIZE);
    return STATUS_SUCCESS;
}
```

- `AttachBacking` walks the MDL's PFN array and builds a per-page `GPU_MEM_ENTRY` scatter-gather list, one entry per guest physical page, then issues `RESOURCE_ATTACH_BACKING` (`viogpu_allocation.cpp:39-58`):

```cpp
void VioGpuAllocation::AttachBacking(MDL *pMDL, size_t pageCount, size_t pageOffset)
{
    m_pMDL = pMDL;
    m_pageCount = pageCount;
    m_pageOffset = pageOffset;
    GPU_MEM_ENTRY *ents = new (NonPagedPoolNx) GPU_MEM_ENTRY[pageCount];
    for (UINT i = 0; i < pageCount; i++)
    {
        ents[i].addr = MmGetMdlPfnArray(pMDL)[pageOffset + i] * PAGE_SIZE;
        ents[i].length = PAGE_SIZE;
        ents[i].padding = 0;
    }
    m_adapter->ctrlQueue.AttachBacking(m_Id, ents, (UINT)pageCount);
}
```

The queue side sends `VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING` with `nr_entries = nents` and the SG-list as the data buffer (`virtio-research-only-3d/viogpu/common/viogpu_queue.cpp:581-602`). `GPU_MEM_ENTRY` is the wire SG entry (`virtio-research-only-3d/viogpu/common/viogpu.h:209-214`): `ULONGLONG addr; ULONG length; ULONG padding;`.

- Memory then physically moves with explicit TRANSFER commands — `VIOGPU_CMD_TRANSFER_TO_HOST` / `VIOGPU_CMD_TRANSFER_FROM_HOST` dispatch to `ctrlQueue.TransferHostCmd(...)` which sends `VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D` / `..._FROM_HOST_3D` (`virtio-research-only-3d/viogpu/viogpu3d/viogpu_command.cpp:77-88` — TransferHostCmd dispatch at :82, `virtio-research-only-3d/viogpu/common/viogpu_queue.cpp:653-668`).

The contrast for Step 2 is decisive:

| Axis | viogpu3d (template) | Helios (zero-copy + venus) |
| --- | --- | --- |
| Backing | OS system-memory pages (MDL → per-page `GPU_MEM_ENTRY` SG list) | Host-visible BAR window; bytes live host-side |
| Resource type | virtio 3D resource (`CreateResource3D`) | virtio HOST3D **blob** (`RESOURCE_CREATE_BLOB`, `nr_entries=0`) |
| Page list | `RESOURCE_ATTACH_BACKING` with N entries | none (`nr_entries = 0`, `gpu.rs:518`) |
| When backing attaches | in `BuildPagingBuffer`/`MapApertureSegment` | at create (blob bound to venus mem id) |
| CPU/host data movement | explicit `TRANSFER_TO_HOST_3D`/`FROM_HOST_3D` copies | none — `RESOURCE_MAP_BLOB` maps the same physical bytes into the process |
| Segment | CPU-visible **aperture** segment | host-visible BAR mapped via Escape (placeholder aperture segment for Code 0) |
| CPU pointer | VidMm `D3DKMTLock2` returns the system-memory VA | Escape `MAP_BLOB` → `MmMapLockedPagesSpecifyCache(MDL_IO_SPACE)` |
| Eviction | `EvictionSegmentSet = 1` (don't use aperture for eviction) | `EvictionSegmentSet = 0` (pinned, never evicted) |
| `BuildPagingBuffer` | real (attaches/detaches aperture backing) | null engine, no DMA emitted (`build_paging_buffer.rs:32-43`) |

Helios's `DxgkDdiBuildPagingBuffer` is deliberately a no-op today — it records `Operation` into DISPATCH-safe atomics (`PAGING_LAST_OP`, `PAGING_CALL_COUNT`) and returns `STATUS_SUCCESS` without advancing `pDmaBuffer`, because the zero-copy blob never needs aperture/page-table DMA (`build_paging_buffer.rs:32-43`). The comment notes Stage 2b intended to read the op here to learn which op carries the VidMm segment offset and then issue `resource_map_blob(resource_id, offset)` — but the proven path moved offset selection to the guest (`map_blob_prepare`, `gpu.rs:557-561`), so the paging buffer stays null. Step 2 should keep it null unless a real GpuMmu page-table model is declared.

## Section E — Current `kmd_render` per-DDI state (real / partial / stub / missing)

The exact starting point for Step 2: every DDI Helios registers in `build_ddi_table` (`kmd_render/src/lib.rs:77–197`), classified REAL / PARTIAL / STUB / MISSING with the decisive `path:line` evidence, plus a summary of the biggest gaps.

---

### E.1 — Per-DDI status table and gap summary

This section gives a holistic, line-cited per-DDI status table for the **current** `helios_kmd_render` driver, so Step 2 knows the exact starting point. Status legend:

- **REAL** — functionally implemented (does the work the DDI contract demands for the current bring-up goal).
- **PARTIAL** — does *some* real work but is incomplete / a placeholder for the full contract.
- **STUB** — registered and returns `STATUS_SUCCESS`/`STATUS_NOT_IMPLEMENTED`/`STATUS_NOT_SUPPORTED` without doing the work.
- **MISSING** — *not registered* in `build_ddi_table()` (left NULL in `DRIVER_INITIALIZATION_DATA`).

#### E.1 The DDI table (what Helios registers)

The entire function-pointer table is filled in `build_ddi_table()` in `kmd_render/src/lib.rs:77-197`. `DriverEntry` (`lib.rs:59-74`) calls it and passes the result to `DxgkInitialize`:

```rust
// kmd_render/src/lib.rs:67-70
let mut init = build_ddi_table();
let status = unsafe { DxgkInitialize(driver_object, registry_path, &mut init) };
```

The version field is the native WDDM 3.2 interface (`lib.rs:85`):

```rust
data.Version = DXGKDDI_INTERFACE_VERSION;
```

Every `data.DxgkDdi* = Some(...)` assignment in `lib.rs:88-194` is enumerated in the table below with its target Rust fn. Note that the comment block at `lib.rs:178-181` explicitly states the render/GPU-VA DDIs "are registered so the table shape is explicit, but return unsupported until the corresponding capability is implemented and advertised."

#### E.2 Per-DDI status table

| DDI (field in `DRIVER_INITIALIZATION_DATA`) | Rust fn | Status | Evidence (path:line) | Note |
|---|---|---|---|---|
| `DxgkDdiAddDevice` | `ddi::dxgkddi_add_device` | **REAL** | `add_device.rs:11-39` | Allocates `AdapterContext::new`, `Box::into_raw` → out-pointer; NULLs out-ptr on failure (`add_device.rs:24,33-35`). |
| `DxgkDdiStartDevice` | `ddi::dxgkddi_start_device` | **REAL (render-only)** | `start_device.rs:15-80` | Saves `DXGKRNL_INTERFACE` (`start_device.rs:37`), brings up virtio transport via `VirtioGpu::init` (`start_device.rs:56-69`), reports **0 video-present sources / 0 children** (`start_device.rs:73-76`). Transport failure is non-fatal at Gate 1 (`start_device.rs:62-68`). |
| `DxgkDdiStopDevice` | `ddi::dxgkddi_stop_device` | **REAL** | `start_device.rs:83-93` | `adapter.set_virtio(None)` tears down transport. |
| `DxgkDdiRemoveDevice` | `ddi::dxgkddi_remove_device` | **REAL** | `start_device.rs:96-105` | `Box::from_raw` frees `AdapterContext`. |
| `DxgkDdiDispatchIoRequest` | `ddi::dxgkddi_dispatch_io_request` | **STUB** | `start_device.rs:109-115` | Returns `STATUS_NOT_IMPLEMENTED` (legacy VRP, unused). |
| `DxgkDdiSetPowerState` | `ddi::dxgkddi_set_power_state` | **STUB** | `start_device.rs:119-126` | Accepts all transitions, returns `STATUS_SUCCESS`, no device work. |
| `DxgkDdiUnload` | `ddi::dxgkddi_unload` | **REAL** | `start_device.rs:166-169` | Calls `WdkHal::unmap_all()` to release cached BAR MMIO. |
| `DxgkDdiQueryInterface` | `ddi::dxgkddi_query_interface` | **STUB** | `start_device.rs:172-189` | Logs GUID, returns `STATUS_NOT_SUPPORTED` (exposes no interface). |
| `DxgkDdiControlEtwLogging` | `ddi::dxgkddi_control_etw_logging` | **STUB** | `start_device.rs:193-198` | Empty no-op (emits no ETW). |
| `DxgkDdiResetDevice` | `ddi::dxgkddi_reset_device` | **STUB** | `start_device.rs:202` | Empty no-op. |
| `DxgkDdiNotifyAcpiEvent` | `ddi::dxgkddi_notify_acpi_event` | **STUB** | `start_device.rs:205-213` | Returns `STATUS_NOT_IMPLEMENTED`. |
| `DxgkDdiQueryChildRelations` | `ddi::dxgkddi_query_child_relations` | **REAL (render-only)** | `start_device.rs:129-136` | No children → leaves array untouched, `STATUS_SUCCESS`. |
| `DxgkDdiQueryChildStatus` | `ddi::dxgkddi_query_child_status` | **REAL (render-only)** | `start_device.rs:139-145` | No children, `STATUS_SUCCESS`. |
| `DxgkDdiQueryDeviceDescriptor` | `ddi::dxgkddi_query_device_descriptor` | **STUB** | `start_device.rs:148-154` | `STATUS_NOT_SUPPORTED` (no EDID/monitor). |
| `DxgkDdiQueryAdapterInfo` | `ddi::dxgkddi_query_adapter_info` | **PARTIAL** | `query_adapter_info.rs:24-80` | Dispatches on `args.Type`; see E.3 for which cap types are real vs zeroed vs rejected. |
| `DxgkDdiStopDeviceAndReleasePostDisplayOwnership` | `ddi::dxgkddi_stop_device_and_release_post_display_ownership` | **STUB** | `display.rs:102-108` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiSystemDisplayEnable` | `ddi::dxgkddi_system_display_enable` | **STUB** | `display.rs:110-119` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiSystemDisplayWrite` | `ddi::dxgkddi_system_display_write` | **STUB** | `display.rs:121-130` | Empty no-op. |
| `DxgkDdiInterruptRoutine` | `ddi::dxgkddi_interrupt_routine` | **STUB** | `interrupt.rs:13-19` | Returns `0` (never claims interrupt; no MSI-X wired). |
| `DxgkDdiDpcRoutine` | `ddi::dxgkddi_dpc_routine` | **STUB** | `interrupt.rs:22-24` | Empty no-op. |
| `DxgkDdiCreateDevice` | `device::dxgkddi_create_device` | **REAL** | `device.rs:38-53` | Allocates `DeviceContext`, hands handle back via `args.hDevice` (`device.rs:51`). |
| `DxgkDdiDestroyDevice` | `device::dxgkddi_destroy_device` | **REAL** | `device.rs:63-102` | Drains this device's blob mappings + unmaps (`device.rs:72-78`), reclaims leaked virtio blobs/contexts by owner (`device.rs:89-97`), frees `DeviceContext` (`device.rs:99`). |
| `DxgkDdiCreateContext` | `device::dxgkddi_create_context` | **PARTIAL** | `device.rs:107-129` | Allocates `ContextContext`, sets `DmaBufferSize=256K`, `DmaBufferPrivateDataSize=40`, alloc/patch list sizes (`device.rs:122-126`) — but creates **no Venus virtio-gpu context** (marked `// STUB: Phase 4` at `device.rs:106`). |
| `DxgkDdiDestroyContext` | `device::dxgkddi_destroy_context` | **PARTIAL** | `device.rs:133-139` | Frees `ContextContext` only; no Venus teardown (`// STUB: Phase 4` at `device.rs:132`). |
| `DxgkDdiCreateProcess` | `device::dxgkddi_create_process` | **REAL (placeholder object)** | `device.rs:149-166` | Allocates `ProcessContext` so `hKmdProcess` is non-NULL (`device.rs:164`); tracks **no** GPU VA space (host-owned VA). |
| `DxgkDdiDestroyProcess` | `device::dxgkddi_destroy_process` | **REAL** | `device.rs:169-179` | Frees `ProcessContext`. |
| `DxgkDdiCreateAllocation` | `ddi::dxgkddi_create_allocation` | **PARTIAL/REAL** | `create_allocation.rs:132-166`, `create_one` `62-130` | Reads ICD `HeliosWddmAllocPrivate`, creates a real virtio blob via `resource_create_blob` (`create_allocation.rs:85-86`), fills VidMm metadata (`create_allocation.rs:113-128`). See E.4 caveats (segment-set, `CpuVisible`, no aperture/transfer model). |
| `DxgkDdiDestroyAllocation` | `ddi::dxgkddi_destroy_allocation` | **REAL** | `create_allocation.rs:168-190`, `destroy_allocation_ctx` `47-58` | Unmaps (if mapped) → detach → unref → free per-alloc ctx. |
| `DxgkDdiBuildPagingBuffer` | `ddi::dxgkddi_build_paging_buffer` | **STUB (null engine)** | `build_paging_buffer.rs:24-43` | Does **not advance `pDmaBuffer`**; only bumps DISPATCH-safe atomics (`build_paging_buffer.rs:40-41`) then `STATUS_SUCCESS`. No page-table/aperture commands. |
| `DxgkDdiSubmitCommand` | `ddi::dxgkddi_submit_command` | **PARTIAL (immediate-fence null engine)** | `submit_command.rs:26-65` | Stores `SubmissionFenceId` into `last_completed_fence` (`submit_command.rs:39-40`) and **immediately** signals `DXGK_INTERRUPT_DMA_COMPLETED` via `DxgkCbNotifyInterrupt` + `DxgkCbQueueDpc` (`submit_command.rs:47-62`). No real GPU/Venus submission; just keeps dxgkrnl's paging path from TDR. |
| `DxgkDdiSubmitCommandVirtual` | `ddi::dxgkddi_submit_command_virtual` | **STUB** | `submit_command.rs:14-19` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiPreemptCommand` | `ddi::dxgkddi_preempt_command` | **STUB** | `submit_command.rs:67-72` | `STATUS_NOT_IMPLEMENTED`. |
| `DxgkDdiResetFromTimeout` | `ddi::dxgkddi_reset_from_timeout` | **STUB** | `submit_command.rs:75-77` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiRestartFromTimeout` | `ddi::dxgkddi_restart_from_timeout` | **STUB** | `submit_command.rs:80-82` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiQueryDependentEngineGroup` | `ddi::dxgkddi_query_dependent_engine_group` | **REAL (single node)** | `scheduler.rs:18-33` | Validates node/engine 0, sets `DependentNodeOrdinalMask=0`. |
| `DxgkDdiQueryEngineStatus` | `ddi::dxgkddi_query_engine_status` | **REAL (zeroed)** | `scheduler.rs:35-56` | Validates node/engine 0, zeroes `EngineStatus`, `STATUS_SUCCESS`. |
| `DxgkDdiResetEngine` | `ddi::dxgkddi_reset_engine` | **PARTIAL** | `scheduler.rs:58-76` | Reports `LastAbortedFenceId = last_completed_fence`; no real engine reset. |
| `DxgkDdiCreateHwContext` | `ddi::dxgkddi_create_hw_context` | **PARTIAL (placeholder)** | `scheduler.rs:78-95` | Allocates empty `HwContext` (`scheduler.rs:14,92-93`), hands back handle; node/affinity must be 0. |
| `DxgkDdiDestroyHwContext` | `ddi::dxgkddi_destroy_hw_context` | **REAL** | `scheduler.rs:97-103` | Frees `HwContext`. |
| `DxgkDdiCreateHwQueue` | `ddi::dxgkddi_create_hw_queue` | **PARTIAL (placeholder)** | `scheduler.rs:105-118` | Allocates empty `HwQueue` (`scheduler.rs:16,115-116`). |
| `DxgkDdiDestroyHwQueue` | `ddi::dxgkddi_destroy_hw_queue` | **REAL** | `scheduler.rs:120-126` | Frees `HwQueue`. |
| `DxgkDdiSubmitCommandToHwQueue` | `ddi::dxgkddi_submit_command_to_hw_queue` | **STUB** | `scheduler.rs:128-138` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiSwitchToHwContextList` | `ddi::dxgkddi_switch_to_hw_context_list` | **STUB** | `scheduler.rs:140-155` | Validates node/engine 0, `STATUS_SUCCESS`, no work. |
| `DxgkDdiPresentToHwQueue` | `ddi::dxgkddi_present_to_hw_queue` | **STUB** | `scheduler.rs:157-167` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiCancelCommand` | `ddi::dxgkddi_cancel_command` | **STUB** | `scheduler.rs:169-178` | `STATUS_SUCCESS`, no work. |
| `DxgkDdiCalibrateGpuClock` | `ddi::dxgkddi_calibrate_gpu_clock` | **STUB (zeroed)** | `scheduler.rs:180-198` | Zeroes the clock-data struct, `STATUS_SUCCESS`. |
| `DxgkDdiFormatHistoryBuffer` | `ddi::dxgkddi_format_history_buffer` | **STUB** | `scheduler.rs:200-212` | Sets `NumTimestamps=0`, `Offset=0`. |
| `DxgkDdiPowerRuntimeControlRequest` | `ddi::dxgkddi_power_runtime_control_request` | **STUB** | `scheduler.rs:241-254` | Sets `bytes_returned=0`, `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiPowerRuntimeSetDeviceHandle` | `ddi::dxgkddi_power_runtime_set_device_handle` | **STUB** | `scheduler.rs:234-239` | `STATUS_SUCCESS`. |
| `DxgkDdiSetStablePowerState` | `ddi::dxgkddi_set_stable_power_state` | **STUB** | `scheduler.rs:214-221` | Void no-op. |
| `DxgkDdiSetVirtualMachineData` | `ddi::dxgkddi_set_virtual_machine_data` | **STUB** | `scheduler.rs:223-232` | `STATUS_SUCCESS`. |
| `DxgkDdiEscape` | `ddi::dxgkddi_escape` | **REAL** | `escape.rs:32-77` | The live venus channel; dispatches CTX_CREATE/DESTROY/SUBMIT_VENUS/WAIT_FENCE/ALLOC_BLOB/MAP_BLOB/RELEASE_BLOB. See E.5. |
| `DxgkDdiPresent` | `ddi::dxgkddi_present` | **STUB** | `display.rs:11-16` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiSetPointerPosition` | `ddi::dxgkddi_set_pointer_position` | **STUB** | `display.rs:18-23` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiSetPointerShape` | `ddi::dxgkddi_set_pointer_shape` | **STUB** | `display.rs:25-30` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiIsSupportedVidPn` | `ddi::dxgkddi_is_supported_vidpn` | **STUB** | `display.rs:32-37` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiRecommendFunctionalVidPn` | `ddi::dxgkddi_recommend_functional_vidpn` | **STUB** | `display.rs:39-44` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiEnumVidPnCofuncModality` | `ddi::dxgkddi_enum_vidpn_cofunc_modality` | **STUB** | `display.rs:46-51` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiSetVidPnSourceVisibility` | `ddi::dxgkddi_set_vidpn_source_visibility` | **STUB** | `display.rs:53-58` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiCommitVidPn` | `ddi::dxgkddi_commit_vidpn` | **STUB** | `display.rs:60-65` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiUpdateActiveVidPnPresentPath` | `ddi::dxgkddi_update_active_vidpn_present_path` | **STUB** | `display.rs:67-72` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiSetVidPnSourceAddress` | `ddi::dxgkddi_set_vidpn_source_address` | **STUB** | `display.rs:74-79` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiRecommendMonitorModes` | `ddi::dxgkddi_recommend_monitor_modes` | **STUB** | `display.rs:81-86` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiQueryVidPnHWCapability` | `ddi::dxgkddi_query_vidpn_hw_capability` | **STUB** | `display.rs:88-93` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiGetScanLine` | `ddi::dxgkddi_get_scan_line` | **STUB** | `display.rs:95-100` | `STATUS_NOT_SUPPORTED`. |
| `DxgkDdiExchangePreStartInfo` | `ddi::dxgkddi_exchange_pre_start_info` | **STUB (accept)** | `display.rs:132-142` | Null-checks then `STATUS_SUCCESS`; does not consume the pre-start info. |
| `DxgkDdiRender` | `ddi::dxgkddi_render` | **STUB** | `submit_command.rs:87-92` | `STATUS_NOT_IMPLEMENTED`. |
| `DxgkDdiRenderKm` | `ddi::dxgkddi_render_km` | **STUB** | `submit_command.rs:95-100` | `STATUS_NOT_IMPLEMENTED`. **Note:** `SupportKernelModeCommandBuffer` cap is advertised but this is unimplemented (coherence debt — see E.3). |
| `DxgkDdiPatch` | `ddi::dxgkddi_patch` | **STUB** | `submit_command.rs:103-108` | `STATUS_NOT_IMPLEMENTED`. |
| `DxgkDdiOpenAllocation` | `ddi::dxgkddi_open_allocation` | **PARTIAL** | `create_allocation.rs:201-220` | Echoes the dxgkrnl global handle into `hDeviceSpecificAllocation` (`create_allocation.rs:217`); succeeds for every allocation. No Stage-3 mapping to `AllocationContext` yet. |
| `DxgkDdiCloseAllocation` | `ddi::dxgkddi_close_allocation` | **STUB** | `create_allocation.rs:223-228` | `STATUS_SUCCESS`, no work. |
| `DxgkDdiDescribeAllocation` | `ddi::dxgkddi_describe_allocation` | **STUB** | `create_allocation.rs:232-238` | `STATUS_NOT_IMPLEMENTED` ("Filled with real metadata in Stage 3"). |
| `DxgkDdiGetStandardAllocationDriverData` | `ddi::dxgkddi_get_standard_allocation_driver_data` | **STUB** | `create_allocation.rs:242-248` | `STATUS_NOT_IMPLEMENTED`. **Major gap** — see E.6. |
| `DxgkDdiGetNodeMetadata` | `ddi::dxgkddi_get_node_metadata` | **REAL (single node)** | `query_adapter_info.rs:319-340` | Reports one node, `EngineType = DXGK_ENGINE_TYPE_3D` (`query_adapter_info.rs:336`); deliberately does **not** set `GpuMmuSupported` (`query_adapter_info.rs:337`). |
| `DxgkDdiSetRootPageTable` | `ddi::dxgkddi_set_root_page_table` | **STUB** | `build_paging_buffer.rs:49-53` | Empty no-op (no GPU-VA). |
| `DxgkDdiGetRootPageTableSize` | `ddi::dxgkddi_get_root_page_table_size` | **STUB** | `build_paging_buffer.rs:58-63` | Returns `0`. |
| `DxgkDdiCollectDbgInfo` | `ddi::dxgkddi_collect_dbg_info` | **STUB** | `submit_command.rs:134-139` | `STATUS_NOT_IMPLEMENTED`. |
| `DxgkDdiControlInterrupt` | `ddi::dxgkddi_control_interrupt` | **STUB** | `interrupt.rs:31-37` | `STATUS_NOT_IMPLEMENTED`. |
| `DxgkDdiQueryCurrentFence` | `ddi::dxgkddi_query_current_fence` | **REAL (null engine)** | `submit_command.rs:111-131` | Reports `CurrentFence = last_completed_fence` (`submit_command.rs:127`), node/engine 0. Consistent with the immediate-fence `SubmitCommand`. |

**MISSING (not registered in `build_ddi_table`):** there is **no** `DxgkDdiSetVidPnSourceAddressWithMultiPlaneOverlay*`, **no** `DxgkDdiUpdateMonitorLinkInfo`, **no** flip/MPO DDIs, **no** `DxgkDdiCreateAllocation2`/`DxgkDdiOpenAllocationInfo2` variants, and **no** GPU-VA DDIs beyond `SetRootPageTable`/`GetRootPageTableSize` (e.g. no `DxgkDdiMapGpuVirtualAddress`, `UnmapGpuVirtualAddress`, `FreeGpuVirtualAddress`, `ReclaimAllocations`, `Escape`-adjacent residency DDIs). Step 2 should confirm against the bindgen dump which of those `DRIVER_INITIALIZATION_DATA` fields exist before relying on them; they are not assigned in `lib.rs:88-194`.

#### E.3 `DxgkDdiQueryAdapterInfo` — caps + segments detail (the heart of the memory model)

Dispatch on `args.Type` (`query_adapter_info.rs:46-79`):

- **`DXGKQAITYPE_DRIVERCAPS`** → `query_driver_caps` (`query_adapter_info.rs:82-148`). Sets:
  - `caps.HighestAcceptableAddress.QuadPart = -1`, `MaxAllocationListSlotId = 0xFFFF`, `ApertureSegmentCommitLimit = 64 MiB`, `SupportNonVGA = 1` (`query_adapter_info.rs:91-95`).
  - `caps.WDDMVersion = DXGKDDI_WDDMv3_2` (`query_adapter_info.rs:100`).
  - Preemption: `GraphicsPreemptionGranularity = D3DKMDT_GRAPHICS_PREEMPTION_DMA_BUFFER_BOUNDARY`, `ComputePreemptionGranularity = D3DKMDT_COMPUTE_PREEMPTION_DMA_BUFFER_BOUNDARY`, `SupportPerEngineTDR = 1` (`query_adapter_info.rs:101-105`).
  - Cap bitfields set **via the `.__bindgen_anon_1.Value` UINT union view**, not named accessors (`query_adapter_info.rs:137-143`):
    ```rust
    // query_adapter_info.rs:131-143
    const PRESENTATIONCAPS_SUPPORT_KERNEL_MODE_COMMAND_BUFFER: u32 = 1 << 2;
    const FLIPCAPS_FLIP_ON_VSYNC_MMIO: u32 = 1 << 1;
    const SCHEDULINGCAPS_MULTI_ENGINE_AWARE: u32 = 1 << 0;
    const SCHEDULINGCAPS_PREEMPTION_AWARE: u32 = 1 << 2;
    const MEMORYMANAGEMENTCAPS_SECTION_BACKED_PRIMARY: u32 = 1 << 3;
    caps.PresentationCaps.__bindgen_anon_1.Value = PRESENTATIONCAPS_SUPPORT_KERNEL_MODE_COMMAND_BUFFER;
    caps.FlipCaps.__bindgen_anon_1.Value = FLIPCAPS_FLIP_ON_VSYNC_MMIO;
    caps.SchedulingCaps.__bindgen_anon_1.Value = SCHEDULINGCAPS_MULTI_ENGINE_AWARE | SCHEDULINGCAPS_PREEMPTION_AWARE;
    caps.MemoryManagementCaps.__bindgen_anon_1.Value = MEMORYMANAGEMENTCAPS_SECTION_BACKED_PRIMARY;
    ```
  - `caps.MaxQueuedFlipOnVSync = 1`, `caps.GpuEngineTopology.NbAsymetricProcessingNodes = 1` (`query_adapter_info.rs:144-145`).
  - **COHERENCE DEBT (authoritative comment, `query_adapter_info.rs:118-130`):** these caps "are MANDATORY for a WDDM 3.2 render-only adapter to load, but the paths behind them are still the null bring-up engine." The cap-bisect result is recorded inline: dropping `FlipOnVSyncMmIo` regressed to **Code 43** even with `SectionBackedPrimary`, so `FlipOnVSyncMmIo` is **mandatory** for render-adapter load — the caps cannot be honestly dropped and must be backed by real impl (Gate 2/3).
- **`DXGKQAITYPE_QUERYSEGMENT4`** → `query_segments` (`query_adapter_info.rs:263-310`). Reports exactly **one** segment:
  - `out.NbSegment = 1`, `SegmentDescriptorStride = size_of::<DXGK_SEGMENTDESCRIPTOR4>()`, `PagingBufferSegmentId = 1`, `PagingBufferSize = 64 KiB`, `PagingBufferPrivateDataSize = 0` (`query_adapter_info.rs:282-286`).
  - The single descriptor is a **CPU-visible aperture** segment, 64 MiB, set via the nested bindgen accessors (`query_adapter_info.rs:298-306`):
    ```rust
    // query_adapter_info.rs:298-306
    seg.Flags.__bindgen_anon_1.__bindgen_anon_1.set_CpuVisible(1);
    seg.Flags.__bindgen_anon_1.__bindgen_anon_1.set_Aperture(1);
    seg.BaseAddress.QuadPart = 0;
    seg.Size = 64 * 1024 * 1024;
    seg.CommitLimit = 64 * 1024 * 1024;
    ```
  - The long comment at `query_adapter_info.rs:249-262` is critical for Step 2: a CPU-visible **memory** segment (`Aperture=0`) pointing `CpuTranslatedAddress` at the host-visible BAR was **REJECTED by VidMm** right after `DxgkDdiCreateDevice` (clean-boot Code 43 / `FAILED_POST_START`), independent of size (tested 8 GiB and 256 MiB). The current code falls back to the proven **aperture** segment "as a Code-0 placeholder"; the host-visible BAR reaches user space via the Escape MAP_BLOB path (E.5), not via a WDDM memory segment.
- **`DXGKQAITYPE_WDDMDEVICECAPS`** → `query_wddm_device_caps` (`query_adapter_info.rs:160-176`): zeroes the struct, sets only `caps.WDDMVersion = DXGKDDI_WDDMv3_2`.
- **`DXGKQAITYPE_PHYSICAL_MEMORY_CAPS`** → `query_physical_memory_caps` (`query_adapter_info.rs:178-194`): zeroes, sets `HighestVisibleAddress.QuadPart = -1`.
- **`DXGKQAITYPE_GPUVERSION`** → `query_gpu_version` (`query_adapter_info.rs:196-217`): writes `BiosVersion = "helios-virtio-gpu"`, `GpuArchitecture = "virtio-gpu"` (unaligned WCHAR writes).
- **`DXGKQAITYPE_HISTORYBUFFERPRECISION`** → `query_history_buffer_precision` (`query_adapter_info.rs:219-237`): `PrecisionBits = 32`.
- **Zeroed-success caps** (`query_zeroed::<T>`, `query_adapter_info.rs:150-158`): `IOMMU_CAPS`, `HARDWARERESERVEDRANGES2`, `ADAPTERPERFDATA_CAPS`, `DIRTYBITTRACKINGCAPS`, `64BITONLYCAPS` (`query_adapter_info.rs:51-63`).
- **Everything else** (the `other =>` arm, `query_adapter_info.rs:71-78`) → `STATUS_NOT_SUPPORTED`. The comment at `query_adapter_info.rs:64-70` explicitly notes `PHYSICALADAPTERCAPS (0x0F)` is deferred and that there is **no** `DXGKQAITYPE_GPUMMUCAPS` / VIDMM-caps handling.

**Confirmed missing cap surfaces (the locked goal needs these):** there is **no `DXGKQAITYPE_GPUMMUCAPS`** handler and **no VIDMM/`DXGK_VIDMMCAPS`-style** handler — they fall to `STATUS_NOT_SUPPORTED`. `DxgkDdiGetNodeMetadata` deliberately leaves `GpuMmuSupported = 0` (`query_adapter_info.rs:337`). So the driver today advertises **no GPU MMU model at all**; the fake-but-coherent GpuMmu model the project requires is entirely unimplemented at the cap level.

#### E.4 `DxgkDdiCreateAllocation` caveats

`create_one` (`create_allocation.rs:62-130`) reads the ICD's `HeliosWddmAllocPrivate`, bounds-checks `PrivateDriverDataSize` and validates magic/version (`create_allocation.rs:67-81`), then creates the backing virtio blob (`create_allocation.rs:85-86`). VidMm metadata is set as a CPU-visible blob in **segment 1** (`create_allocation.rs:113-128`):

```rust
// create_allocation.rs:114-128
info.hAllocation = Box::into_raw(ctx) as HANDLE;
info.Size = size;
info.PitchAlignedSize = size;
info.SupportedWriteSegmentSet = 1; // segment id 1 (bit 0)
info.EvictionSegmentSet = 0; // host-visible blob is pinned; never evicted
info.__bindgen_anon_1.Alignment = PAGE as UINT;
info.__bindgen_anon_2.SupportedReadSegmentSet = 1;
info.__bindgen_anon_3.MaximumRenamingListLength = 0;
info.__bindgen_anon_4.FlagsWddm2.__bindgen_anon_1.__bindgen_anon_1.set_CpuVisible(1);
```

This is driven by an ICD-side `D3DKMTCreateAllocation` with private data — i.e. it is the **venus-blob-over-Escape model**, not the OS-driven allocation path DWM uses. There is no aperture/TRANSFER population (unlike viogpu3d), no `DescribeAllocation`, and no `GetStandardAllocationDriverData`, so the **OS/DWM-driven** "standard allocation" path (shared primaries, the path DWM composition actually exercises) cannot work yet.

#### E.5 `DxgkDdiEscape` — the live venus verbs

`dxgkddi_escape` (`escape.rs:32-77`) validates a `HeliosEscapeHeader` (`escape.rs:46-58`), derives the owner token from `args.hDevice` (`escape.rs:64`), and dispatches (`escape.rs:66-76`):

| Verb | Handler | Status | Evidence |
|---|---|---|---|
| `HELIOS_ESCAPE_CTX_CREATE` | `escape_ctx_create` | **REAL** — `v.ctx_create(capset_id, owner)`, writes `out_ctx_id` | `escape.rs:81-97` |
| `HELIOS_ESCAPE_CTX_DESTROY` | `escape_ctx_destroy` | **REAL** — `v.ctx_destroy(ctx_id)` | `escape.rs:100-111` |
| `HELIOS_ESCAPE_SUBMIT_VENUS` | `escape_submit_venus` | **REAL** — stages the opaque venus stream into a `DmaBuffer`, `v.submit_venus(ctx_id, fence_id, ...)` | `escape.rs:117-149` |
| `HELIOS_ESCAPE_WAIT_FENCE` | `escape_wait_fence` | **PARTIAL (interim synchronous)** — only validates shape, returns `STATUS_SUCCESS`; relies on `submit_venus` blocking on the used ring (comment `escape.rs:151-156`) | `escape.rs:157-162` |
| `HELIOS_ESCAPE_ALLOC_BLOB` | `escape_alloc_blob` | **REAL** — `v.alloc_blob(...)`, writes `out_resource_id` | `escape.rs:166-187` |
| `HELIOS_ESCAPE_MAP_BLOB` | `escape_map_blob` | **REAL** — two-phase: `map_blob_prepare` under virtio lock → `map_io_pages_to_user` at PASSIVE, records in `adapter.mappings` | `escape.rs:195-247`; primitives `blob_map.rs:76-126` |
| `HELIOS_ESCAPE_RELEASE_BLOB` | `escape_release_blob` | **REAL** — unmaps this device's user view, then `v.release_blob(...)` | `escape.rs:251-270` |

This is the **only** functionally complete render/memory data path in the driver: it is the zero-copy host-visible BAR model ported 1:1 from the System-class `kmd/src/ioctl.rs` (comment `blob_map.rs:9-11`). It is driven by `D3DKMTEscape` from the ICD, entirely **outside** the WDDM command/GPU-VA/VidMm path that DWM uses.

#### E.6 Biggest gaps (prose summary for Step 2)

1. **No GPU MMU / VidMm cap surface at all.** `DxgkDdiQueryAdapterInfo` has **no `DXGKQAITYPE_GPUMMUCAPS`** and **no VIDMMCAPS** handler — both fall through to `STATUS_NOT_SUPPORTED` (`query_adapter_info.rs:71-78`), and `DxgkDdiGetNodeMetadata` leaves `GpuMmuSupported = 0` (`query_adapter_info.rs:337`). The locked "fake-but-coherent GpuMmu" model is therefore **not started** at the capability level; the driver currently presents a single CPU-visible **aperture** segment as a Code-0 placeholder only (`query_adapter_info.rs:282-306`, with the rejection history at `:249-262`).

2. **`GetStandardAllocationDriverData` is a `STATUS_NOT_IMPLEMENTED` stub** (`create_allocation.rs:242-248`). This DDI is what the OS/runtime calls to describe standard allocations (shared primaries, GDI surfaces) — precisely the allocations DWM composition needs. Until it is real, DWM cannot create its compositable surfaces on Helios. Closely paired, **`DescribeAllocation` is also a stub** (`create_allocation.rs:232-238`).

3. **`BuildPagingBuffer` is a true null engine.** It never advances `pDmaBuffer` and writes no page-table/aperture commands; it only bumps diagnostic atomics (`build_paging_buffer.rs:32-43`). No residency/paging/aperture-map work happens. `SetRootPageTable` is an empty no-op and `GetRootPageTableSize` returns `0` (`build_paging_buffer.rs:49-63`).

4. **`SubmitCommand` is an immediate-fence null engine.** It signals `DXGK_INTERRUPT_DMA_COMPLETED` synchronously the moment a DMA/paging buffer is submitted (`submit_command.rs:34-64`) and `QueryCurrentFence` echoes `last_completed_fence` (`submit_command.rs:127`). There is **no** linkage from a real venus submit to the WDDM fence — the memory directive's requirement that "the venus submit must DRIVE the WDDM fence" is unimplemented. `SubmitCommandVirtual`, `Render`, `RenderKm`, `Patch`, `SubmitCommandToHwQueue`, and all preempt/TDR DDIs are `STATUS_NOT_SUPPORTED`/`STATUS_NOT_IMPLEMENTED` stubs (`submit_command.rs:14-19,67-108`; `scheduler.rs:128-138`).

5. **Cap/impl mismatch already documented in-tree.** `DRIVERCAPS` advertises `SupportKernelModeCommandBuffer`, `FlipOnVSyncMmIo`, `MultiEngineAware`, `PreemptionAware`, `SectionBackedPrimary` (`query_adapter_info.rs:137-143`) but the backing paths (`RenderKm`, flip, preemptible scheduler) are the null engine — explicitly flagged as "COHERENCE DEBT … do not treat their presence as proof of support" (`query_adapter_info.rs:118-130`). The bisect note proves they are load-bearing for adapter load (dropping `FlipOnVSyncMmIo` → Code 43) and therefore must be *backed*, not dropped.

6. **The entire VidPN/display table and the render/HW-queue path are stubs** returning `STATUS_NOT_SUPPORTED` (`display.rs`, `scheduler.rs` HW-queue/present entries). Consistent with render-only (0 sources / 0 children at `start_device.rs:73-76`), but it means there is no OS-visible present/flip path inside the driver — the present is intended to come from the Looking Glass IDD capturing the OS-composed frame, which depends on items 1–4 above being real so that DWM will actually run on Helios.

**Net:** the only end-to-end-working data path today is `DxgkDdiEscape` → venus (CTX/SUBMIT/blob map), driven by the ICD via `D3DKMTEscape`, completely outside the WDDM VidMm/GpuMmu/command-scheduler path. The adapter loads at Code 0 only because of placeholder caps + a placeholder aperture segment + null paging/submit engines. To reach the locked goal (DWM composites on Helios), Step 2 must build the fake GpuMmu cap surface (`GpuMmuCaps`/node `GpuMmuSupported`, segment model), implement `GetStandardAllocationDriverData`/`DescribeAllocation`, replace the null `BuildPagingBuffer` with real residency/aperture handling, and make `SubmitCommand` drive the WDDM fence from a real venus submit — none of which exist today.

## Section F — IDD side (selecting Helios; reading the composed frame)

The Looking Glass IDD changes Step 2 must make: stop force-selecting WARP so the OS composites on Helios, and read the OS-composed frame without **D3D12** (Helios has no D3D12) — either by porting the IDD copy path to D3D11 or by enabling the existing (gated-off) `CHeliosSink` Vulkan dma-buf import.

---

### F.1 — Selecting Helios and reading the OS-composed frame (no D3D12)

This section covers the Looking Glass IDD changes needed so the IDD (a) stops force-selecting the Microsoft Basic Render Driver (WARP) and lets the OS-selected Helios render adapter stand (or actively redirects to Helios), and (b) reads the Helios-composed frame using only D3D11 (Helios has **no** D3D12). All citations are verbatim from the LGIdd sources at `/home/rupansh/helios-vgpu/LookingGlass/idd/LGIdd/`.

#### F.1 — The WARP force-select block (must be removed or redirected)

`CIndirectDeviceContext::InitAdapter()` currently enumerates DXGI adapters and forces the IDD's render adapter onto the Microsoft Basic Render Driver (WARP), explicitly to keep IddCx away from the "incomplete Helios render adapter". The intent comment is verbatim at `CIndirectDeviceContext.cpp:182-184`:

```cpp
  // During Helios WDDM bring-up, force the IDD compositor onto the software
  // render adapter. If IddCx chooses by itself, it can pick the incomplete
  // Helios render adapter and pull DWM/Explorer into the experimental UMD path.
```

The full force-select block, `CIndirectDeviceContext.cpp:185-223`:

```cpp
  IDXGIFactory * factory = NULL;
  IDXGIAdapter * dxgiAdapter;
  bool selectedBasicRenderDriver = false;
  if (SUCCEEDED(CreateDXGIFactory(__uuidof(IDXGIFactory), (void **)&factory)))
  {
    for (UINT i = 0; factory->EnumAdapters(i, &dxgiAdapter) != DXGI_ERROR_NOT_FOUND; ++i)
    {
      DXGI_ADAPTER_DESC adapterDesc;
      dxgiAdapter->GetDesc(&adapterDesc);
      dxgiAdapter->Release();

      const bool isBasicRenderDriver =
        (adapterDesc.VendorId == 0x1414 && adapterDesc.DeviceId == 0x008c) ||
        wcsstr(adapterDesc.Description, L"Microsoft Basic Render Driver") != nullptr;

      DEBUG_INFO(L"IDD render adapter[%u]: %s vendor=0x%04x device=0x%04x%s",
        i,
        adapterDesc.Description,
        adapterDesc.VendorId,
        adapterDesc.DeviceId,
        isBasicRenderDriver ? L" selected" : L" skipped");

      if (!isBasicRenderDriver)
        continue;

      IDARG_IN_ADAPTERSETRENDERADAPTER args = {};
      args.PreferredRenderAdapter = adapterDesc.AdapterLuid;
      IddCxAdapterSetRenderAdapter(m_adapter, &args);
      DEBUG_INFO(L"IDD selected Microsoft Basic Render Driver: %s", adapterDesc.Description);
      selectedBasicRenderDriver = true;
      break;
    }
    factory->Release();
  }
  else
    DEBUG_WARN("CreateDXGIFactory failed while selecting IDD render adapter");

  if (!selectedBasicRenderDriver)
    DEBUG_WARN("Microsoft Basic Render Driver not found; IddCx render adapter remains OS-selected");
```

Key facts to note for Step 2:
- The WARP match is `adapterDesc.VendorId == 0x1414 && adapterDesc.DeviceId == 0x008c`, OR the description substring `L"Microsoft Basic Render Driver"` (`CIndirectDeviceContext.cpp:196-198`).
- The override is applied via `IddCxAdapterSetRenderAdapter(m_adapter, &args)` where `args.PreferredRenderAdapter = adapterDesc.AdapterLuid` and `args` is an `IDARG_IN_ADAPTERSETRENDERADAPTER` (`CIndirectDeviceContext.cpp:210-212`).
- This whole block runs inside `InitAdapter()`, immediately after `m_adapter = initOut.AdapterObject;` (`CIndirectDeviceContext.cpp:180`) and before `m_heliosSink.Init()` (`CIndirectDeviceContext.cpp:228`).

**What Step 2 must change (F.1 change-list):**
- **Option A — stop overriding (let the OS pick Helios):** Delete the entire `CIndirectDeviceContext.cpp:185-223` block. With no `IddCxAdapterSetRenderAdapter` call, IddCx falls back to the OS-selected render adapter; once Helios is the only/primary WDDM render adapter and DWM composites on it, IddCx will hand the IDD a swapchain whose `RenderAdapterLuid` is Helios. The existing `DEBUG_WARN("... IddCx render adapter remains OS-selected")` already describes this intended fallback.
- **Option B — actively redirect to Helios:** Keep the enumeration loop but invert the match: instead of `isBasicRenderDriver`, match the Helios adapter (by its `DXGI_ADAPTER_DESC.Description` string, or by the LUID the Helios KMD reports) and pass *that* LUID to `IddCxAdapterSetRenderAdapter`. The match against `0x1414/0x008c` (WARP) and the `selectedBasicRenderDriver` bookkeeping must be replaced by a Helios match. Note: the OS-composed surface delivered to the swapchain (Section F.3) is independent of which adapter the *copy* device opens — the override only affects which GPU IddCx asks DWM to composite on. To get a hardware-accelerated Helios desktop, the OS must already be compositing DWM on Helios; the override at most pins it.

Either way the load-bearing change is: **the IDD must no longer force WARP.** Today this block unconditionally overrides whenever a Basic Render Driver adapter is found.

#### F.2 — The D3D12 copy path that must move to D3D11 (Helios has no D3D12)

The IDD's frame copy path is built on D3D12 throughout. The construction of `CSwapChainProcessor` shows both devices being threaded in, `CSwapChainProcessor.cpp:27-39`:

```cpp
CSwapChainProcessor::CSwapChainProcessor(IDDCX_MONITOR monitor, CIndirectDeviceContext* devContext, IDDCX_SWAPCHAIN hSwapChain,
    std::shared_ptr<CD3D11Device> dx11Device, std::shared_ptr<CD3D12Device> dx12Device, HANDLE newFrameEvent) :
  m_monitor(monitor),
  m_devContext(devContext),
  m_hSwapChain(hSwapChain),
  m_dx11Device(dx11Device),
  m_dx12Device(dx12Device),
  m_newFrameEvent(newFrameEvent)
{
  m_resPool.Init(dx11Device, dx12Device);
  m_fbPool.Init(this);
  if (!m_postProcessor.Init(dx12Device))
    DEBUG_ERROR("Failed to initialize post processor");
```

##### F.2.1 — Acquire loop and where the composed texture is obtained

The acquire loop is `IddCxSwapChainReleaseAndAcquireBuffer` (and the 1.10 variant `...Buffer2`). The composed surface comes out as `buffer.MetaData.pSurface`, an `IDXGIResource`. From `CSwapChainProcessor.cpp:144-179`:

```cpp
    UINT frameNumber = 0;
    UINT dirtyRectCount = 0;
    ComPtr<IDXGIResource> surface;

#if defined(IDDCX_VERSION_MAJOR) && defined(IDDCX_VERSION_MINOR) && \
  (IDDCX_VERSION_MAJOR > 1 || (IDDCX_VERSION_MAJOR == 1 && IDDCX_VERSION_MINOR >= 10))
    if (m_devContext->CanProcessFP16())
    {
      IDARG_IN_RELEASEANDACQUIREBUFFER2 acquireIn = {};
      acquireIn.Size = sizeof(acquireIn);
      acquireIn.AcquireSystemMemoryBuffer = FALSE;

      IDARG_OUT_RELEASEANDACQUIREBUFFER2 buffer = {};
      buffer.MetaData.Size = sizeof(buffer.MetaData);

      hr = IddCxSwapChainReleaseAndAcquireBuffer2(m_hSwapChain, &acquireIn, &buffer);
      if (SUCCEEDED(hr))
      {
        frameNumber = buffer.MetaData.PresentationFrameNumber;
        dirtyRectCount = buffer.MetaData.DirtyRectCount;
        surface = buffer.MetaData.pSurface;
      }
    }
    else
#endif
    {
      IDARG_OUT_RELEASEANDACQUIREBUFFER buffer = {};

      hr = IddCxSwapChainReleaseAndAcquireBuffer(m_hSwapChain, &buffer);
      if (SUCCEEDED(hr))
      {
        frameNumber = buffer.MetaData.PresentationFrameNumber;
        dirtyRectCount = buffer.MetaData.DirtyRectCount;
        surface = buffer.MetaData.pSurface;
      }
    }
```

On a new frame the acquired `IDXGIResource surface` is passed to `SwapChainNewFrame(surface, dirtyRectCount)` (`CSwapChainProcessor.cpp:204`). **Important:** `acquireIn.AcquireSystemMemoryBuffer = FALSE` (`CSwapChainProcessor.cpp:154`) means IddCx hands back a *GPU surface*, not a CPU-mapped buffer — the IDD is expected to read it on a GPU device. This is the OS-composed Helios frame.

##### F.2.2 — The cast to `ID3D11Texture2D`

`SwapChainNewFrame` first casts the acquired `IDXGIResource` to an `ID3D11Texture2D` — this part is already D3D11 and is the natural read handle. `CSwapChainProcessor.cpp:319-334`:

```cpp
bool CSwapChainProcessor::SwapChainNewFrame(ComPtr<IDXGIResource> acquiredBuffer, unsigned dirtyRectCount)
{
  ComPtr<ID3D11Texture2D> texture;
  HRESULT hr = acquiredBuffer.As(&texture);
  if (FAILED(hr))
  {
    DEBUG_ERROR_HR(hr, "Failed to obtain the ID3D11Texture2D from the acquiredBuffer");
    return false;
  }

  CInteropResource * srcRes = m_resPool.Get(texture);
  if (!srcRes)
  {
    DEBUG_ERROR("Failed to get a CInteropResource from the pool");
    return false;
  }
```

`m_resPool.Get(texture)` returns a `CInteropResource` that wraps the D3D11 texture **and shares it into D3D12** (the resource pool is initialized with both devices at `CSwapChainProcessor.cpp:36`: `m_resPool.Init(dx11Device, dx12Device);`). The comment at `CSwapChainProcessor.cpp:336-340` makes the D3D11→D3D12 sharing explicit:

```cpp
  /**
   * Even though we have not performed any copy/draw operations we still need to
   * use a fence. Because we share this texture with DirectX12 it is able to
   * read from it before the desktop duplication API has finished updating it.
   */
  srcRes->Signal();
```

Note `srcRes->GetRes()` then returns the **D3D12** resource (`ComPtr<ID3D12Resource>`), used at `CSwapChainProcessor.cpp:365` (`D3D12_RESOURCE_DESC srcDesc = srcRes->GetRes()->GetDesc();`) and `CSwapChainProcessor.cpp:416` (`ComPtr<ID3D12Resource> copySrcResource = srcRes->GetRes();`).

##### F.2.3 — The D3D12 copy to KVMFR (every D3D12 dependency)

After acquiring, the copy from the composed texture into the LGMP/KVMFR shared-memory frame buffer is done entirely with D3D12 — the copy queue, the command list, the `GetCopyableFootprints` layout, the `CopyTextureRegion` calls, and the asynchronous completion callback. From `CSwapChainProcessor.cpp:381-500`:

```cpp
  const D12FrameFormat& dstFormat = m_postProcessor.GetOutputFormat();

  D3D12_PLACED_SUBRESOURCE_FOOTPRINT layout;
  m_dx12Device->GetDevice()->GetCopyableFootprints(
    &dstFormat.desc,
    0,
    1,
    0,
    &layout,
    NULL,
    NULL,
    NULL);
  ...
  auto copyQueue = m_dx12Device->TryGetCopyQueue();
  if (!copyQueue)
  {
    const LONG drops = InterlockedIncrement(&m_copyBusyDrops);
    if (drops == 1 || (drops % 300) == 0)
      DEBUG_WARN("D3D12 copy queues busy, dropping frame (drops=%ld)", drops);
    return false;
  }

  ComPtr<ID3D12Resource> copySrcResource = srcRes->GetRes();
  CD3D12CommandQueue * computeQueue = nullptr;
  if (m_postProcessor.HasActiveEffects())
  {
    computeQueue = m_dx12Device->GetComputeQueue();
    ...
    srcRes->Sync(*computeQueue);
    copySrcResource = m_postProcessor.Run(
      computeQueue->GetGfxList(), copySrcResource,
      currentDirtyRects, &nbDirtyRects);

    computeQueue->Execute();
    copyQueue->WaitFor(*computeQueue);
  }
  else
    srcRes->Sync(*copyQueue);
  ...
  copyQueue->SetCompletionCallback(&CompletionFunction, this, fbRes);

  D3D12_TEXTURE_COPY_LOCATION srcLoc = {};
  srcLoc.pResource        = copySrcResource.Get();
  srcLoc.Type             = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
  srcLoc.SubresourceIndex = 0;

  D3D12_TEXTURE_COPY_LOCATION dstLoc = {};
  dstLoc.pResource       = fbRes->Get().Get();
  dstLoc.Type            = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
  dstLoc.PlacedFootprint = layout;

  if (IsFullDamage(currentDirtyRects, nbDirtyRects, dstFormat.desc) ||
      nbDirtyRects > KVMFR_MAX_DAMAGE_RECTS || m_nbDirtyRects == 0)
  {
    copyQueue->GetGfxList()->CopyTextureRegion(
      &dstLoc, 0, 0, 0, &srcLoc, NULL);
  }
  ...
  copyQueue->Execute();
```

The destination resource `fbRes->Get()` (a `ComPtr<ID3D12Resource>`) is a D3D12 buffer that aliases the KVMFR/ivshmem frame slot. The async completion callback `CompletionFunction` (`CSwapChainProcessor.cpp:228-251`) is where the bytes actually land in KVMFR and the frame is posted:

```cpp
void CSwapChainProcessor::CompletionFunction(
  CD3D12CommandQueue * queue, bool result, void * param1, void * param2)
{
  UNREFERENCED_PARAMETER(queue);

  auto sc    = (CSwapChainProcessor *)param1;
  auto fbRes = (CFrameBufferResource*)param2;

  // fail gracefully
  if (!result)
  {
    sc->m_devContext->FinalizeFrameBuffer(fbRes->GetFrameIndex());
    return;
  }

  if (sc->m_dx12Device->IsIndirectCopy())
    sc->m_devContext->WriteFrameBuffer(
      fbRes->GetFrameIndex(),
      fbRes->GetMap(), 0, fbRes->GetFrameSize(), true);
  else
    sc->m_devContext->FinalizeFrameBuffer(fbRes->GetFrameIndex());

  sc->m_devContext->PresentHeliosFrame(fbRes->GetFrameIndex());
}
```

**Enumerated D3D12 dependencies that must be replaced with D3D11 (F.2 change-list):**

1. **`CD3D12Device` construction itself** — created in `CIndirectMonitorContext::AssignSwapChain` (`CIndirectMonitorContext.cpp:50-64`):
   ```cpp
   m_dx12Device = std::make_shared<CD3D12Device>(renderAdapter);
   switch (m_dx12Device->Init(m_devContext->GetIVSHMEM(), alignSize))
   { ... }
   ```
   On Helios this `CD3D12Device::Init` will fail (no D3D12 on the Helios LUID). Step 2 must either stop creating `CD3D12Device` for the Helios LUID, or replace it with a D3D11-only copy device.
2. **Resource pool D3D12 sharing** — `m_resPool.Init(dx11Device, dx12Device)` (`CSwapChainProcessor.cpp:36`) and the `CInteropResource` that opens the D3D11 texture as a shared `ID3D12Resource`. Replace with a pure-D3D11 staging texture (the acquired `ID3D11Texture2D` is already D3D11; no cross-API share is needed).
3. **`m_postProcessor.Init(dx12Device)`** (`CSwapChainProcessor.cpp:38`) and `m_postProcessor.Run(computeQueue->GetGfxList(), ...)` (`CSwapChainProcessor.cpp:428-430`) — the HDR/effects post-processor is a D3D12 compute pass. For a first cut on Helios, disable post-processing (no `HasActiveEffects()` path) or reimplement it as a D3D11 compute shader.
4. **`m_dx12Device->GetDevice()->GetCopyableFootprints(...)`** (`CSwapChainProcessor.cpp:384-392`) — used to compute `layout.Footprint.RowPitch` and the placed footprint. With D3D11 the equivalent is a `D3D11_TEXTURE2D_DESC` staging texture plus `ID3D11DeviceContext::Map` to get the `RowPitch` from `D3D11_MAPPED_SUBRESOURCE`.
5. **`m_dx12Device->TryGetCopyQueue()` / `GetComputeQueue()`** (`CSwapChainProcessor.cpp:407-433`) and the `CD3D12CommandQueue` machinery — the D3D12 copy/compute queues. Replace with the D3D11 immediate (or deferred) context. Note the "drop frame if copy queue busy" behavior at `CSwapChainProcessor.cpp:407-414` exists because D3D12 queues can saturate; a D3D11 synchronous `CopyResource`/`CopySubresourceRegion` removes the async-queue concept entirely (at the cost of synchronous blocking).
6. **`D3D12_TEXTURE_COPY_LOCATION` + `CopyTextureRegion`** (`CSwapChainProcessor.cpp:461-493`, plus the helpers `CopyDirtyRect` at `CSwapChainProcessor.cpp:265-279`, `ClipDirtyRect`/`ClipDirtyRects` at `CSwapChainProcessor.cpp:281-305`, and `IsFullDamage` at `CSwapChainProcessor.cpp:254-263` which take `const D3D12_RESOURCE_DESC&`) — replace with `ID3D11DeviceContext::CopyResource` / `CopySubresourceRegion` (with a `D3D11_BOX` per dirty rect), and re-type the helpers to take `D3D11_TEXTURE2D_DESC`.
7. **`copyQueue->SetCompletionCallback(&CompletionFunction, this, fbRes)` + `CompletionFunction(CD3D12CommandQueue*, ...)`** (`CSwapChainProcessor.cpp:228-251, 459`) — the async D3D12 fence-completion callback. With a synchronous D3D11 copy, the IDD can `Map`/read the staging texture inline and call `WriteFrameBuffer`/`FinalizeFrameBuffer`/`PresentHeliosFrame` directly after the copy, eliminating the callback signature dependency on `CD3D12CommandQueue`.
8. **`srcRes->Signal()` / `srcRes->Sync(queue)`** (`CSwapChainProcessor.cpp:341, 427, 436`) — the D3D11→D3D12 shared fence. Unneeded in a pure-D3D11 path: read the acquired `ID3D11Texture2D` on the same D3D11 device that produced it; IddCx's own frame ordering plus a `CopyResource` to a staging texture suffices.

The **good news** for Step 2: the byte-level frame plumbing into KVMFR is already D3D-agnostic. `PrepareFrameBuffer` (`CIndirectDeviceContext.cpp:853-975`), `WriteFrameBuffer` (`CIndirectDeviceContext.cpp:977-988`), `FinalizeFrameBuffer` (`CIndirectDeviceContext.cpp:990-994`), and `PresentHeliosFrame` (`CIndirectDeviceContext.cpp:996-1007`) take plain `void*`/indices and `memcpy` into the LGMP frame slot — none of them touch D3D12. A D3D11 staging-texture `Map` (giving a CPU pointer + RowPitch) feeds directly into `WriteFrameBuffer`/`PrepareFrameBuffer`. The only D3D12 type leaking into `CIndirectDeviceContext` is `D12FrameFormat` / `D3D12_RESOURCE_DESC` carried in `PrepareFrameBuffer(unsigned pitch, const D12FrameFormat& srcFormat, const D12FrameFormat& dstFormat, ...)` (`CIndirectDeviceContext.cpp:853-855`) — that struct's `.desc.Width/.Height/.Format` fields have direct `D3D11_TEXTURE2D_DESC` equivalents and must be re-typed.

#### F.3 — How the render-adapter LUID is plumbed (Device.cpp / CIndirectMonitorContext.cpp)

When IddCx assigns a swapchain to the monitor it supplies the render-adapter LUID it chose (or that we forced via `IddCxAdapterSetRenderAdapter`). The DDI entry point `LGIddMonitorAssignSwapChain` extracts it from `IDARG_IN_SETSWAPCHAIN`, `Device.cpp:189-196`:

```cpp
NTSTATUS LGIddMonitorAssignSwapChain(IDDCX_MONITOR monitor, const IDARG_IN_SETSWAPCHAIN* inArgs)
{
  auto * wrapper = WdfObjectGet_CIndirectMonitorContextWrapper(monitor);
  wrapper->context->AssignSwapChain(
    inArgs->hSwapChain, inArgs->RenderAdapterLuid, inArgs->hNextSurfaceAvailable);
  wrapper->context->GetDeviceContext()->OnAssignSwapChain();
  return STATUS_SUCCESS;
}
```

`inArgs->RenderAdapterLuid` is the OS/IddCx-chosen render adapter (this is what the F.1 override influences). It flows into `CIndirectMonitorContext::AssignSwapChain`, where it is used to construct **both** the D3D11 and D3D12 copy devices, `CIndirectMonitorContext.cpp:37-73`:

```cpp
void CIndirectMonitorContext::AssignSwapChain(IDDCX_SWAPCHAIN swapChain, LUID renderAdapter, HANDLE newFrameEvent)
{
reInit:
  UnassignSwapChain();

  m_dx11Device = std::make_shared<CD3D11Device>(renderAdapter);
  if (FAILED(m_dx11Device->Init()))
  {
    WdfObjectDelete(swapChain);
    return;
  }

  UINT64 alignSize = CPlatformInfo::GetPageSize();
  m_dx12Device = std::make_shared<CD3D12Device>(renderAdapter);
  switch (m_dx12Device->Init(m_devContext->GetIVSHMEM(), alignSize))
  {
    case CD3D12Device::SUCCESS:
      break;

    case CD3D12Device::FAILURE:
      WdfObjectDelete(swapChain);
      return;

    case CD3D12Device::RETRY:
      m_dx12Device.reset();
      m_dx11Device.reset();
      goto reInit;
  }
  ...
  m_swapChain.reset(new CSwapChainProcessor(m_monitor, m_devContext, swapChain, m_dx11Device, m_dx12Device, newFrameEvent));
```

So the LUID is `renderAdapter` here, opened by `CD3D11Device(renderAdapter)` (D3D11, `.Init()`) and `CD3D12Device(renderAdapter)` (D3D12, `.Init(...)`). On Helios, `CD3D12Device::Init` returns `CD3D12Device::FAILURE` (no D3D12), which deletes the swapchain and bails — **this is the second hard blocker after F.1**: even if Helios is selected as the render adapter, `AssignSwapChain` will reject the swapchain at `CIndirectMonitorContext.cpp:56-58` because the D3D12 device on the Helios LUID fails to init.

The swapchain is then bound to the *copy* device inside the swapchain thread via `IddCxSwapChainSetDevice` using a `DXGIDevice` obtained from the D3D11 device, `CSwapChainProcessor.cpp:84-115`:

```cpp
  ComPtr<IDXGIDevice> dxgiDevice;
  HRESULT hr = m_dx11Device->GetDevice().As(&dxgiDevice);
  ...
  IDARG_IN_SWAPCHAINSETDEVICE setDevice = {};
  setDevice.pDevice = dxgiDevice.Get();

  hr = IddCxSwapChainSetDevice(m_hSwapChain, &setDevice);
  if (FAILED(hr))
  {
    DEBUG_ERROR_HR(hr, "IddCxSwapChainSetDevice Failed");
    return;
  }
```

`IddCxSwapChainSetDevice.pDevice` is the D3D11 `IDXGIDevice`, so the *acquire-side* device is already D3D11. The only thing the LUID is currently *also* used for is constructing the now-unwanted `CD3D12Device`.

**F.3 change-list:**
- In `CIndirectMonitorContext::AssignSwapChain` (`CIndirectMonitorContext.cpp:50-64`): when `renderAdapter` is the Helios LUID, **do not** create `CD3D12Device` (or make `CD3D12Device::Init` succeed in a degenerate D3D11-backed mode). Otherwise the `CD3D12Device::FAILURE` path deletes the swapchain.
- Keep `m_dx11Device = std::make_shared<CD3D11Device>(renderAdapter)` (`CIndirectMonitorContext.cpp:42`) — D3D11 on the Helios LUID is the supported path (Helios's D3D11 UMD is the Gate 5b work). The `IddCxSwapChainSetDevice` call already uses the D3D11 `IDXGIDevice`, so no change is needed there.
- `UnassignSwapChain` (`CIndirectMonitorContext.cpp:76-81`) resets `m_swapChain`, `m_dx11Device`, `m_dx12Device`; drop the `m_dx12Device.reset()` if the device is removed.

#### F.4 — The alternative `CHeliosSink` Vulkan-import path (gated off; runs in the swapchain thread)

`CHeliosSink` is a parallel, **opt-in** sink that mirrors a completed LG IDD frame into a Venus-backed Vulkan image and asks the Helios KMD to scan out the resulting virtio-gpu blob. The file header states this verbatim, `CHeliosSink.cpp:1-8`:

```cpp
/**
 * Looking Glass Helios IDD Vulkan sink.
 *
 * This opt-in bridge mirrors completed LG IDD frames into a Venus-backed Vulkan
 * image and asks the Helios KMD to scan out the resulting virtio-gpu blob. It is
 * intentionally conservative: the IDD/KVMFR path remains authoritative, and this
 * sink is enabled only by HKLM\SOFTWARE\LookingGlass\IDD\HeliosEnable.
 */
```

**The `HeliosEnable` gate** — `Init()` early-returns unless the registry value is set, `CHeliosSink.cpp:306-310`:

```cpp
bool CHeliosSink::Init()
{
  m_enabled = g_settings.ReadBoolValue(L"HeliosEnable", false);
  if (!m_enabled)
    return true;
```

`Init()` is called from `CIndirectDeviceContext::InitAdapter()` at `CIndirectDeviceContext.cpp:228-229` (after the WARP force-select). So with `HeliosEnable=0` (the default), the sink is constructed but inert.

**`OpenDevice()` — the Helios device interface** opens `GUID_DEVINTERFACE_HELIOS` via SetupDi + `CreateFileW`. The GUID is defined at `CHeliosSink.cpp:21-24`:

```cpp
static const GUID GUID_DEVINTERFACE_HELIOS = {
  0xC8F84237, 0xCD89, 0x48F5,
  { 0xAF, 0xC5, 0x32, 0x94, 0x45, 0x24, 0x62, 0x5C }
};
```

and the open path is `SetupDiGetClassDevsW(&GUID_DEVINTERFACE_HELIOS, ... DIGCF_PRESENT | DIGCF_DEVICEINTERFACE)` → `SetupDiEnumDeviceInterfaces` → `SetupDiGetDeviceInterfaceDetailW` → `CreateFileW(detail->DevicePath, GENERIC_READ | GENERIC_WRITE, ...)` (`CHeliosSink.cpp:101-145`).

**`vulkan-1.dll` load + device bring-up** — `InitVulkan()` loads `vulkan-1.dll` and resolves `vkGetInstanceProcAddr`, `CHeliosSink.cpp:147-159`:

```cpp
bool CHeliosSink::InitVulkan()
{
  m_vulkan = LoadLibraryW(L"vulkan-1.dll");
  if (!m_vulkan)
  {
    DEBUG_ERROR("Helios: failed to load vulkan-1.dll");
    return false;
  }

  m_gipa = (PFN_vkGetInstanceProcAddr)(void *)
    GetProcAddress(m_vulkan, "vkGetInstanceProcAddr");
```

It then creates an instance, enumerates physical devices, and selects the device whose `deviceName` contains the configured GPU-match string (default `L"Intel"`, see `Init()` at `CHeliosSink.cpp:314-315`), `CHeliosSink.cpp:205-213`:

```cpp
  m_phys = devs[0];
  for (uint32_t i = 0; i < count; ++i)
  {
    VkPhysicalDeviceProperties pp;
    pProps(devs[i], &pp);
    DEBUG_INFO("Helios: Vulkan device[%u]: %s", i, pp.deviceName);
    if (strstr(pp.deviceName, m_gpuMatch))
      m_phys = devs[i];
  }
```

**The external-memory / dma-buf import extensions** are requested in `InitVulkan`, `CHeliosSink.cpp:238-244`:

```cpp
  const char * wanted[] =
  {
    "VK_KHR_external_memory",
    "VK_KHR_external_memory_fd",
    "VK_EXT_external_memory_dma_buf",
    "VK_EXT_image_drm_format_modifier",
  };
```

and the image is created as a DRM-format-modifier (LINEAR), dma-buf-exportable, host-visible image. From `CreateImage` — the handle type `VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT` appears on both the image create-info and the memory export-info, `CHeliosSink.cpp:394-403`:

```cpp
  uint64_t mods[1] = { DRM_FORMAT_MOD_LINEAR };
  VkImageDrmFormatModifierListCreateInfoEXT modList = {};
  modList.sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT;
  modList.drmFormatModifierCount = 1;
  modList.pDrmFormatModifiers = mods;

  VkExternalMemoryImageCreateInfo extImg = {};
  extImg.sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO;
  extImg.pNext = &modList;
  extImg.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
```

and at allocation, `CHeliosSink.cpp:449-451`:

```cpp
  VkExportMemoryAllocateInfo exp = {};
  exp.sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO;
  exp.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
```

**The resid gate file** — after binding/mapping the image, the sink reads the virtio-gpu resource id back out of a gate file keyed by allocation size, `CHeliosSink.cpp:488-494`:

```cpp
  m_resourceId = read_resid_for_size(m_gateFile, (uint64_t)req.size);
  if (!m_resourceId)
  {
    DEBUG_ERROR("Helios: no resource id found for image allocation size %llu",
        (unsigned long long)req.size);
    return false;
  }
```

The gate file path defaults to `C:\Users\Rupansh\helios_lg_idd_resid.txt` and is also exported as the env var `HELIOS_GATE_RESID_FILE` so the venus ICD (the producer) writes the resid/size pairs there. From `Init()`, `CHeliosSink.cpp:312-322`:

```cpp
  const std::wstring gate =
    g_settings.ReadStringValue(L"HeliosGateFile", L"C:\\Users\\Rupansh\\helios_lg_idd_resid.txt");
  ...
  narrow_copy(m_gateFile, sizeof(m_gateFile), gate);
  ...
  SetEnvironmentVariableA("HELIOS_GATE_RESID_FILE", m_gateFile);
  _putenv_s("HELIOS_GATE_RESID_FILE", m_gateFile);
  DeleteFileA(m_gateFile);
```

(The matching consumer side: `read_resid_for_size` scans `"%u %llu"` rid/size pairs and returns the rid whose size matches `wantSize`, `CHeliosSink.cpp:70-85`. The ICD producer writes those lines — `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c:1091` reads the same `HELIOS_GATE_RESID_FILE`.)

**Present path → KMD scanout escape** — `Present` does the CPU copy of the KVMFR frame into the mapped Vulkan image, optionally flushes (if not HOST_COHERENT), submits an empty queue submit + `vkQueueWaitIdle`, then issues `IOCTL_HELIOS_PRESENT_BLOB` carrying the resource id. From `CHeliosSink.cpp:551-572`:

```cpp
  m_pSubmit(m_queue, 0, NULL, VK_NULL_HANDLE);
  m_pQueueWaitIdle(m_queue);

  helios_escape_present_blob pb = {};
  pb.hdr.magic = HELIOS_ESCAPE_MAGIC;
  pb.hdr.cmd_type = HELIOS_ESCAPE_PRESENT_BLOB;
  pb.hdr.version = HELIOS_ESCAPE_VERSION;
  pb.hdr.size = sizeof(pb);
  pb.resource_id = m_resourceId;
  pb.width = m_width;
  pb.height = m_height;
  pb.format = VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM;
  pb.stride = m_stride;
  pb.offset = m_offset;

  DWORD returned = 0;
  if (!DeviceIoControl(m_hdev, IOCTL_HELIOS_PRESENT_BLOB, &pb, sizeof(pb),
        &pb, sizeof(pb), &returned, NULL))
  {
    DEBUG_ERROR("Helios: PRESENT_BLOB failed: %lu", GetLastError());
    DestroyImage();
  }
```

`IOCTL_HELIOS_PRESENT_BLOB` is `0x0022E418u` and `HELIOS_ESCAPE_PRESENT_BLOB` is `0x0007u` (`CHeliosSink.cpp:26, 29`).

**It runs inside the swapchain thread.** `Present` is invoked through `CIndirectDeviceContext::PresentHeliosFrame`, `CIndirectDeviceContext.cpp:996-1007`:

```cpp
void CIndirectDeviceContext::PresentHeliosFrame(unsigned frameIndex)
{
  ...
  m_heliosSink.Present(frame, fb->data);
}
```

and `PresentHeliosFrame` is called from `CSwapChainProcessor::CompletionFunction` (`CSwapChainProcessor.cpp:250`), which is the D3D12 copy-queue completion callback driven off the swapchain processing path. So **the Vulkan sink does its `vkQueueSubmit`/`vkQueueWaitIdle`/`DeviceIoControl` synchronously on the swapchain/copy completion path** — it is not a separate transport thread. That matters for Step 2: it is a *blocking, per-frame* synchronous import + escape, gated off by default.

**F.4 takeaways for the change-list:** `CHeliosSink` is an **alternative read/scanout route that bypasses the IddCx-composed-surface→KVMFR pipeline** — it does the opposite direction (it takes an already-completed KVMFR frame and re-imports it into a venus blob to drive a *direct* Helios scanout via `IOCTL_HELIOS_PRESENT_BLOB`). Per the project's LOCKED goal and the `wddm-hwaccel-desktop-is-the-goal` memory, the direct-venus-scanout (CHeliosSink resid path) is **NOT the goal**; the goal is the OS-composed Helios surface captured by the standard IddCx swapchain (Section F.2/F.3, D3D11). So `CHeliosSink` should remain gated off (`HeliosEnable=0`); Step 2's primary work is the D3D11 read of the composed surface, not this Vulkan import.

#### F.5 — Concrete Section F change-list (summary)

**(a) Force-select removal / redirect** (`CIndirectDeviceContext.cpp:185-223`):
- Remove the WARP force-select block entirely (Option A) so IddCx uses the OS-selected render adapter (Helios), OR invert the `isBasicRenderDriver` match to a Helios match and pass the Helios LUID to `IddCxAdapterSetRenderAdapter` (Option B). The current code unconditionally overrides to `0x1414/0x008c` / `"Microsoft Basic Render Driver"`.

**(b) D3D12 → D3D11 copy path** (Helios has no D3D12):
1. Stop creating `CD3D12Device` on the Helios LUID — `CIndirectMonitorContext.cpp:50-64` (the `CD3D12Device::FAILURE` branch deletes the swapchain). Either skip it or back it with D3D11.
2. Replace the D3D12 copy machinery in `CSwapChainProcessor::SwapChainNewFrame` (`CSwapChainProcessor.cpp:381-500`): `GetCopyableFootprints` → D3D11 staging-texture `Map` for RowPitch; `TryGetCopyQueue`/`GetComputeQueue`/`CD3D12CommandQueue` → D3D11 immediate context; `D3D12_TEXTURE_COPY_LOCATION`+`CopyTextureRegion` → `CopyResource`/`CopySubresourceRegion`(+`D3D11_BOX`); async `SetCompletionCallback`+`CompletionFunction(CD3D12CommandQueue*,...)` (`CSwapChainProcessor.cpp:228-251`) → synchronous inline read.
3. Drop the D3D11→D3D12 share/fence (`m_resPool.Init(dx11,dx12)`, `CInteropResource`, `srcRes->Signal()`/`Sync()` at `CSwapChainProcessor.cpp:341,427,436`).
4. Replace/disable the D3D12 post-processor (`m_postProcessor.Init(dx12Device)`, `m_postProcessor.Run(computeQueue->GetGfxList(),...)`).
5. Re-type the D3D12-typed helpers (`IsFullDamage`, `CopyDirtyRect`, `ClipDirtyRect`, `ClipDirtyRects`, the `D12FrameFormat`/`D3D12_RESOURCE_DESC` carried into `PrepareFrameBuffer`) onto `D3D11_TEXTURE2D_DESC`.
6. Keep the unchanged KVMFR plumbing: `PrepareFrameBuffer`/`WriteFrameBuffer`/`FinalizeFrameBuffer`/`PresentHeliosFrame` (`CIndirectDeviceContext.cpp:853-1007`) are D3D-agnostic and feed off a CPU pointer + pitch.

**(c) Reading the OS-composed Helios surface** — use the **D3D11 path**, not `CHeliosSink`:
- The composed frame arrives as `buffer.MetaData.pSurface` (`IDXGIResource`) from `IddCxSwapChainReleaseAndAcquireBuffer[2]` (`CSwapChainProcessor.cpp:159-178`) with `AcquireSystemMemoryBuffer = FALSE` (GPU surface).
- It is already cast to `ID3D11Texture2D` via `acquiredBuffer.As(&texture)` (`CSwapChainProcessor.cpp:321-322`) — read it on the D3D11 copy device (`CD3D11Device(renderAdapter)`), `CopyResource` into a CPU-readable D3D11 staging texture, `Map`, and `memcpy` into the KVMFR frame via the existing `WriteFrameBuffer`/`PrepareFrameBuffer`.
- The acquire-side device is already D3D11 (`IddCxSwapChainSetDevice` is fed the D3D11 `IDXGIDevice`, `CSwapChainProcessor.cpp:107-110`), so no API change is needed there.
- Leave `CHeliosSink` (the Vulkan dma-buf import + `IOCTL_HELIOS_PRESENT_BLOB` direct-scanout) gated off (`HeliosEnable=0`); it is the NOT-the-goal per-blob direct path and runs synchronously inside the swapchain completion callback (`CSwapChainProcessor.cpp:250` → `CIndirectDeviceContext.cpp:1006` → `CHeliosSink.cpp:509-573`).

## Section G — GPU-PV vs viogpu3d vs ours; the memory-model recommendation; VRD pairing

Situates the fake/decorative-GpuMmu venus model against the two real precedents Windows ships — **GPU paravirtualization** (real-but-proxied) and **viogpu3d** (real, transfer-queue) — makes the **GpuMmu-vs-IoMmu recommendation** with the bindgen evidence, and analyzes the **VRD-pairing** consequence: why DWM composites on a render-only adapter (the behavior Helios exploits) and why that same pairing makes DWM's stability strictly gated on Helios's render correctness.

---

### G.1 — GPU-PV vs viogpu3d vs Helios; VRD pairing; cross-adapter

This section situates the Helios fake/decorative-GpuMmu venus model against the two real precedents that Windows already ships — Microsoft's **GPU paravirtualization (GPU-PV)** and the QEMU virtio-gpu **viogpu3d** 3D miniport — and then makes the GpuMmu-vs-IoMmu memory-model recommendation, with the **VRD-pairing** consequence (why DWM composites on the render-only adapter, which is both what we want and why a broken Helios crash-loops DWM).

All three precedents are *real* memory models. Helios's model is *fake/decorative*: the guest GpuMmu/page-table machinery exists only to satisfy VidMm's bookkeeping; the **host GPU owns the real MMU**, and venus addresses resources by opaque id, so the guest GPU virtual addresses are never dereferenced by hardware.

---

#### A9.1 GPU-PV (`gpu-paravirtualization.md`): real-but-proxied; the VRD; default pairing with the display-only adapter

GPU-PV is the closest *architectural cousin* to Helios — a render-only adapter, no local VidMm/VidSch, rendering proxied across a VM boundary — and its documented VRD-pairing behavior is the **load-bearing reason DWM picks a render-only adapter at all**, which is the mechanism Helios is deliberately exploiting.

**No guest VidMm/VidSch; the VRD replaces the KMD.** From `gpu-paravirtualization.md` lines 53–57:

> * There's no KMD in the guest, only UMD. The Virtual Render Device (VRD) KMD replaces the KMD. VRD's purpose is to facilitate the loading of *Dxgkrnl*.
> * There's no video memory manager (*VidMm*) or scheduler (*VidSch*) in the guest.
> * *Dxgkrnl* in a VM gets thunk calls and marshalls them to the host partition via VM bus channels. *Dxgkrnl* in the guest also creates local objects for allocations, processes, devices, and other resources, which reduces traffic with the host.

**The VRD section — the default pairing.** This is the exact text the project memory cites as the documented root cause of the Helios DWM crash-loop. `gpu-paravirtualization.md` lines 59–72:

> ## Virtual render device (VRD)
>
> When a paravirtualized GPU isn't present in a VM, the VM's Device Manager shows the "Microsoft Hyper-V Video" adapter. This display-only adapter is paired by default with the BasicRender adapter for rendering.
>
> When you add a paravirtualized GPU to a VM, the VM's Device Manager shows two adapters:
>
> * Microsoft Hyper-V Video Adapter or Microsoft Remote Display Adapter
> * Microsoft Virtual Render Driver (The actual name is the name of the GPU adapter on the host)
>
> By default, the VRD is paired with the Hyper-V Video adapter, so all UI rendering occurs with the VRD adapter.
>
> If you encounter rendering issues, you can disable this pairing using the [GpuVirtualizationFlags](#gpuvirtualizationflags) registry setting. In this case, the render-only adapter (VRD) is used when an application specifically picks it. For example, some DirectX samples allow you to change the rendering device. The Direct3D runtimes add a logical display output to the VRD when an application decides to use it.
>
> When you add multiple virtual GPUs to the VM, there can be multiple VRD adapters in the guest. However, only one of them can be paired with the Hyper-V Video adapter. There's no way to specify which one; the OS chooses.

The phrase **"all UI rendering occurs with the VRD adapter"** is exactly the behavior Helios wants: a render-only adapter paired with a display-only adapter (in our case the Looking Glass IDD instead of Hyper-V Video), with DWM compositing the whole desktop on the render adapter. The "render-only adapter is used when an application specifically picks it" alternative (with pairing disabled) is the *non-goal* — that is the per-app path we explicitly reject.

**The `ParavirtualizationSupported` cap is the GPU-PV opt-in.** `gpu-paravirtualization.md` line 133:

> A KMD that supports GPU paravirtualization needs to set the [**DXGK_VIDMMCAPS::ParavirtualizationSupported**](/windows-hardware/drivers/ddi/d3dkmddi/ns-d3dkmddi-_dxgk_vidmmcaps) capability.

And line 1057–1059:

> ### Added DXGK_VIDMMCAPS cap
>
> The **ParavirtualizationSupported** capability is added to the [**DXGK_VIDMMCAPS**](/windows-hardware/drivers/ddi/d3dkmddi/ns-d3dkmddi-_dxgk_vidmmcaps) structure. The host KMD sets this cap if it implements all the DDIs described in this section.

Note the qualifier "**The host KMD**" — `ParavirtualizationSupported` is set by the *host* GPU's real KMD, not by a guest driver. Helios is a guest driver and is **not** a GPU-PV component (it is a virtio-gpu function driver), so Helios should **not** set `ParavirtualizationSupported`; doing so would lie about implementing the GPU-PV host DDI surface (`DxgkDdiSetVirtualMachineData`, VM-process `DxgkDdiCreateProcess` flags, host-side handle translation) that we do not implement.

**GPU-PV requires GpuMmu, not physical access.** `gpu-paravirtualization.md` lines 1091–1093:

> ### Physical access to GPU allocations
>
> Currently, the driver doesn't implement physical access to the allocations. The driver must support [GpuMmu](gpummu-model.md).

This is the single most important sentence for the Helios recommendation: **the WDDM virtualization model mandates GpuMmu**. There is no IoMmu-only or physical-addressing GPU-PV.

**`GpuVirtualizationFlags` bits** — `gpu-paravirtualization.md` lines 1016–1021:

> | Bit | Description |
> | --- | ----------- |
> | 0x1 | Force the [ParavirtualizationSupported](/windows-hardware/drivers/ddi/d3dkmddi/ns-d3dkmddi-_dxgk_vidmmcaps) cap for all hardware adapters. Use this bit in the host. |
> | 0x2 | Force the ParavirtualizationSupported cap for BasicRender. Use this bit in the host. |
> | 0x4 | Force secure virtual machine mode, where all virtual machines will be treated as secure. In this mode, there are restrictions on the user-mode driver. For example, the driver can't use Escape calls, so they'll fail. Use this bit in the host. |
> | 0x8 |  Enable pairing of paravirtualized adapters with the display-only adapter. Use this bit in the guest VM. Pairing is enabled by default. |

Bit **0x8** is the pairing lever the DWM-crash memory records as TRIED-and-rejected (`dwm-crash-vrd-pairing-rootcause.md`: "bit 0x8 doesn't govern our non-paravirtual adapter"). This is consistent: bit 0x8 governs *paravirtualized* (GPU-PV) adapters; Helios is a virtio-gpu function adapter, not a paravirtualized adapter, so the bit does not apply — the pairing of Helios-render with the LG-IDD-display is being driven by ordinary WDDM render/display adapter selection, not by GPU-PV's VRD pairing machinery. The GPU-PV text is the *model/analogy* for why a render-only adapter ends up doing all UI rendering, not a knob we can flip.

**IoMmu in GPU-PV is about isolation/security, not the addressing model.** `gpu-paravirtualization.md` line 85 ("IoMmu isolation is enabled. VM creation fails if the driver doesn't support IoMmu isolation"), line 92 (dev-time disable), and the `IoMmuFlags = 8` registry escape at lines 1033–1039. GPU-PV still requires GpuMmu for addressing (line 1093) *and* IoMmu for secure-VM isolation — they are orthogonal. This matters for the Helios recommendation below: choosing "GpuMmu" is not in tension with any IoMmu requirement.

---

#### A9.2 The cross-adapter alternative (`rendering-on-a-discrete-gpu-using-cross-adapter-resources.md`): the REJECTED model

The cross-adapter model is the lighter hybrid-discrete path the project memory records as REJECTED (`wddm-hwaccel-desktop-is-the-goal.md`: "The lighter hybrid-discrete/cross-adapter path (DWM stays on WARP, apps cross-adapter in) was REJECTED (leaves desktop software-composited)"). The doc confirms why: in every cross-adapter scenario the **DWM composites using the *integrated* GPU's copy of the resource**, and the discrete/render GPU only renders into a copied cross-adapter buffer. The render adapter never composites the desktop.

`rendering-on-a-discrete-gpu-using-cross-adapter-resources.md` lines 15–20 — the integrated GPU is the compositor and primary owner:

> An [integrated GPU](using-cross-adapter-resources-in-a-hybrid-system.md) uses a [cross-adapter resource](using-cross-adapter-resources-in-a-hybrid-system.md) as:
>
> * A texture during composition by the Desktop Window Manager (DWM).
> * A render target for [GDI hardware acceleration](gdi-hardware-acceleration.md).
> * A display primary.
> * Not as a render target for 3-D operations.

Lines 32–34 (redirected bitblt model) — the discrete adapter only does a **Present/copy** into a cross-adapter resource, and **DWM composes from the integrated GPU's copy**:

> 5. When a DirectX application calls a **Present** method, the Direct3D runtime calls the [*PresentDXGI*](/windows-hardware/drivers/ddi/dxgiddi/ns-dxgiddi-dxgi_ddi_base_functions) (or *pfnPresent*) function of the discrete GPU's user-mode driver to copy the back buffer to the cross-adapter resource. See the "Present" operation in the figure.
> ...
> 7. The DWM process opens the cross-adapter resource in the integrated GPU and uses it during composition as a source texture. See the "Composition" operation in the figure.

Line 45 (direct flip model) — again the DWM composes "using the resource from the integrated GPU":

> 6. The DWM performs its composition using the resource from the integrated GPU. If a Direct Flip operation is needed ([**DXGK_SEGMENTFLAGS**](/windows-hardware/drivers/ddi/d3dkmddi/ns-d3dkmddi-_dxgk_segmentflags).**DirectFlip** is set), DWM instructs the integrated GPU's display miniport driver to perform a flip operation from one cross-adapter allocation to another.

**Why rejected for Helios:** in this model the *whole desktop is composited by the "integrated" GPU* — for Helios that integrated/primary side would be a software adapter (WARP/BasicRender behind the LG-IDD), so the desktop is software-composited and Helios only accelerates individual apps' back-buffers via a copy. That is per-app acceleration, the explicitly-rejected non-goal. It also requires real cross-adapter shared allocations with a real backing store on both adapters (line 28: "created in kernel mode as a standard allocation on the integrated GPU"; line 29 re-creates "a new resource on the discrete GPU using the same backing store"), which is the opposite of the single decorative venus-backed segment Helios wants. The Helios goal is the GPU-PV-style outcome ("all UI rendering occurs with the VRD adapter") where DWM composites the *entire* desktop on Helios, not cross-adapter copy-in of individual surfaces.

---

#### A9.3 viogpu3d: real, transfer-queue, and — decisively — neither GpuMmu nor IoMmu is *explicitly* declared (so it defaults to GpuMmu)

I searched the entire viogpu3d tree for any GpuMmu/IoMmu/Paravirtualization/VidMmCaps declaration. **There are zero occurrences** of `GpuMmu`, `IoMmu`, `Paravirtualization`, `VirtualMmu`, or `VIDMMCAPS` anywhere in `/home/rupansh/helios-vgpu/virtio-research-only-3d/viogpu/viogpu3d/` (`grep -rniE "Paravirtualization|VirtualMmu|MmuModel|GpuMmu|IoMmu|VidMmCaps|VIDMMCAPS"` returned exit code 1, no matches).

What viogpu3d *does* declare in `DxgkDdiQueryAdapterInfo` is a zeroed `DXGK_DRIVERCAPS` plus a single **aperture** memory segment. From `viogpu_adapter.cpp`:

`DXGKQAITYPE_DRIVERCAPS` handler — `viogpu_adapter.cpp:485-507`:

```cpp
DXGK_DRIVERCAPS *pDriverCaps = (DXGK_DRIVERCAPS *)pQueryAdapterInfo->pOutputData;
...
RtlZeroMemory(pDriverCaps, pQueryAdapterInfo->OutputDataSize /*sizeof(DXGK_DRIVERCAPS)*/);
pDriverCaps->WDDMVersion = DXGKDDI_WDDMv1_3;
pDriverCaps->HighestAcceptableAddress.QuadPart = (ULONG64)-1;
pDriverCaps->FlipCaps.FlipOnVSyncMmIo = TRUE;
pDriverCaps->MaxQueuedFlipOnVSync = 1;
pDriverCaps->MemoryManagementCaps.SectionBackedPrimary = TRUE;
pDriverCaps->SupportDirectFlip = 1;
pDriverCaps->SchedulingCaps.MultiEngineAware = 1;
pDriverCaps->SchedulingCaps.PreemptionAware = 1;
pDriverCaps->GpuEngineTopology.NbAsymetricProcessingNodes = 1;
pDriverCaps->SupportSmoothRotation = FALSE;
pDriverCaps->SupportNonVGA = IsVgaDevice();
```

Critically, the only `MemoryManagementCaps` (= `DXGK_VIDMMCAPS`) bit it sets is `SectionBackedPrimary = TRUE` (line 498). It **never sets `MemoryManagementCaps.ParavirtualizationSupported`** and never declares an MMU model bit here. The MMU model in WDDM 2.x+ is not chosen via `DXGK_DRIVERCAPS` at all (see A9.4) — it is reported per engine-node from `DXGKQAITYPE_PHYSICALADAPTERCAPS`/`DXGK_NODEMETADATA`, which viogpu3d does not implement. Per the WDDM rules, **a driver that does not opt into IoMmu defaults to GpuMmu**, and GPU-PV's mandate ("The driver must support GpuMmu", `gpu-paravirtualization.md:1093`) is the model viogpu3d implicitly follows.

viogpu3d's segment is a single **aperture** segment, `CpuVisible = FALSE`, with `DirectFlip` — `viogpu_adapter.cpp:533-564`:

```cpp
DXGK_QUERYSEGMENTOUT3 *pSegmentInfo = (DXGK_QUERYSEGMENTOUT3 *)pQueryAdapterInfo->pOutputData;
if (!pSegmentInfo[0].pSegmentDescriptor)
{
    pSegmentInfo->NbSegment = 1;
}
else
{
    DXGK_SEGMENTDESCRIPTOR3 *pSegmentDesc = pSegmentInfo->pSegmentDescriptor;
    memset(&pSegmentDesc[0], 0, sizeof(pSegmentDesc[0]));
    pSegmentInfo->PagingBufferPrivateDataSize = 0;
    pSegmentInfo->PagingBufferSegmentId = 1;
    pSegmentInfo->PagingBufferSize = 10 * PAGE_SIZE;
    //
    // Fill out aperture segment descriptor
    //
    memset(&pSegmentDesc[0], 0, sizeof(pSegmentDesc[0]));
    pSegmentDesc[0].BaseAddress.QuadPart = 0xC0000000;
    pSegmentDesc[0].Flags.Aperture = TRUE;
    pSegmentDesc[0].Flags.CacheCoherent = TRUE;
    // pSegmentDesc[0].CpuTranslatedAddress.QuadPart = 0xFFFFFFFE00000000;
    pSegmentDesc[0].Flags.CpuVisible = FALSE;
    // pSegmentDesc[0].Flags.DirectFlip = TRUE;
    pSegmentDesc[0].Size = 256 * 1024 * 4096;
    pSegmentDesc[0].CommitLimit = 256 * 1024 * 4096;
    pSegmentDesc[0].Flags.DirectFlip = TRUE;
}
```

This is **direct evidence for the GpuMmu recommendation**: the working virtio-gpu 3D miniport declares **no IoMmu**, a single aperture segment, and lets WDDM use the default **GpuMmu** addressing — exactly matching Gate-5a's "aperture+TRANSFER, never a CPU-visible mem segment" finding recorded in `gate5a-venus-d3dkmt.md`. (viogpu3d differs from Helios only in *how the segment's memory moves*: it uses virtio-gpu TRANSFER queues for guest↔host copies; Helios will keep the decorative aperture segment for VidMm's sake but back actual data with zero-copy host-visible venus blobs via MAP_BLOB.)

---

#### A9.4 The bindgen evidence: how the MMU model is actually selected (and the exact accessors)

The MMU model is **not** a field of `DXGK_DRIVERCAPS`. From `dxgk_bindings_dump.rs`, the full `_DXGK_DRIVERCAPS` struct (lines 51069–51110) contains `MemoryManagementCaps: DXGK_VIDMMCAPS` (line 51084) but no GpuMmu/IoMmu caps member. Verbatim relevant fields:

```rust
pub struct _DXGK_DRIVERCAPS {
    pub HighestAcceptableAddress: PHYSICAL_ADDRESS,          // line 51070
    ...
    pub MemoryManagementCaps: DXGK_VIDMMCAPS,                // line 51084
    pub GpuEngineTopology: DXGK_GPUENGINETOPOLOGY,           // line 51085
    pub WDDMVersion: DXGK_WDDMVERSION,                       // line 51086
    pub Reserved: DXGK_VIRTUALADDRESSCAPS_DEPRECATED,        // line 51087
    ...
}
```

Note `Reserved: DXGK_VIRTUALADDRESSCAPS_DEPRECATED` (line 51087) — the old WDDM 1.x `VirtualAddressCaps` is **deprecated**; addressing-model reporting moved to the per-node/physical-adapter caps path.

**`ParavirtualizationSupported` is a bitfield in `DXGK_VIDMMCAPS`.** Struct (lines 49550–49553):

```rust
pub struct _DXGK_VIDMMCAPS {
    pub __bindgen_anon_1: _DXGK_VIDMMCAPS__bindgen_ty_1,     // line 49551
    pub PagingNode: UINT,                                   // line 49552
}
```

The bits live in the anonymous union's `_bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>` (lines 49556–49565). The accessor Step 2 must use (lines 49938–49946):

```rust
pub fn ParavirtualizationSupported(&self) -> UINT {
    unsafe { ::core::mem::transmute(self._bitfield_1.get(10usize, 1u8) as u32) }
}
pub fn set_ParavirtualizationSupported(&mut self, val: UINT) {
    unsafe {
        let val: u32 = ::core::mem::transmute(val);
        self._bitfield_1.set(10usize, 1u8, val as u64)
    }
}
```

i.e. `ParavirtualizationSupported` is **bit 10** of the `DXGK_VIDMMCAPS` bitfield (`get(10usize, 1u8)`). **Recommendation: Helios should NOT call `set_ParavirtualizationSupported(1)`** (see A9.5).

**The MMU model selector — `DXGK_NODEMETADATA`.** The real per-engine-node switch carries BOTH booleans (lines 31671–31677):

```rust
pub struct _DXGK_NODEMETADATA {
    pub EngineType: DXGK_ENGINE_TYPE::Type,    // line 31672
    pub FriendlyName: [WCHAR; 32usize],        // line 31673
    pub Flags: DXGK_NODEMETADATA_FLAGS,        // line 31674
    pub GpuMmuSupported: BOOLEAN,              // line 31675
    pub IoMmuSupported: BOOLEAN,               // line 31676
}
```

A node advertises `GpuMmuSupported` and/or `IoMmuSupported` here (returned via `DXGKQAITYPE_PHYSICALADAPTERCAPS` / node metadata). The GpuMmu cap detail struct is `_DXGK_GPUMMUCAPS` (line 47944), queried via `DXGKQAITYPE_GPUMMUCAPS: Type = 13` (`dxgk_bindings_dump.rs:44526`):

```rust
pub struct _DXGK_GPUMMUCAPS {
    pub __bindgen_anon_1: _DXGK_GPUMMUCAPS__bindgen_ty_1,    // line 47945
    pub PageTableUpdateMode: DXGK_PAGETABLEUPDATEMODE,       // line 47946
    pub VirtualAddressBitCount: UINT,                       // line 47947
    pub LeafPageTableSizeFor64KPagesInBytes: UINT,
    pub PageTableLevelCount: UINT,
    pub LegacyBehaviors: _DXGK_GPUMMUCAPS__bindgen_ty_2,
}
```

**There is NO `DXGKQAITYPE_IOMMUCAPS` and no `_DXGK_IOMMUCAPS` struct** in the WDDM 3.2 dump — the QAITYPE list goes `...QUERYSEGMENT4 = 11, SEGMENTMEMORYSTATE = 12, GPUMMUCAPS = 13, PAGETABLELEVELDESC = 14, PHYSICALADAPTERCAPS = 15...` (`dxgk_bindings_dump.rs:44524-44528`). IoMmu support is reported by the `IoMmuSupported: BOOLEAN` node flag (line 31676) plus a `UseIoMmu` bit (bit 2 of a flags struct — `get(2usize, 1u8)`, lines 43393–43394) and features `DXGK_DRIVER_FEATURE_GPUVAIOMMU` (line 23532) and `DXGK_FEATURE_GPUVAIOMMU` (line 23567), both `Type = 36`; there is no dedicated IoMmu *caps* query analogous to `DXGK_GPUMMUCAPS`. This asymmetry — a rich GpuMmu caps surface vs a single IoMmu boolean — is itself evidence that GpuMmu is the primary/expected render-adapter model.

---

#### A9.5 Three-way comparison

| Axis | **GPU-PV** (real, proxied) | **viogpu3d** (real, transfer-queue) | **Helios fake-VidMm + venus** (target) |
|---|---|---|---|
| **Memory model** | No guest VidMm/VidSch at all; allocations are local `Dxgkrnl` objects marshalled to the host real KMD over VM bus (`gpu-paravirtualization.md:53-57`). GpuMmu **mandatory** (`:1093`). | Real VidMm-managed allocations; single **aperture** segment, 256 K × 4 KB = 4 GiB, `CpuVisible=FALSE`, `DirectFlip` (`viogpu_adapter.cpp:553-564`); memory moved by virtio-gpu TRANSFER queues. | Real VidMm bookkeeping over a **decorative** GpuMmu; one over-sized aperture-style segment so nothing evicts; WDDM allocations ↦ venus resource ids; actual data zero-copy via host-visible MAP_BLOB BAR (not TRANSFER). |
| **Who owns the MMU** | **Host** real WDDM KMD owns the real MMU; guest is GpuMmu but page-table updates proxy to host. | **Guest** declares GpuMmu (default; no IoMmu declared — `grep` = 0 hits); virtio-gpu host owns backing store, guest aperture maps it. | **Host GPU owns the real MMU.** Guest GpuMmu page tables are *decorative*: venus addresses by opaque resource id; host GPU never reads guest page tables. |
| **How addressing is validated** | VidMm/Dxgkrnl + host KMD validate via real GpuMmu page tables on the host. | Guest VidMm validates GPU-VA against the aperture segment; virtio TRANSFER honors it. | **Cannot be validated by VidMm** — host never dereferences guest GPU-VA. We satisfy VidMm's *form* (segment + page-table DDIs + `BuildPagingBuffer`) without any address ever being used by hardware. This is the core "fake-but-coherent" bet. |
| **Fence path** | Host KMD signals guest events via `DxgkCbSignalEvent` (`gpu-paravirtualization.md:1101`); submit/signal/wait are async VM-bus messages (`:376-381`). | Real virtio-gpu fence on the control queue drives the WDDM fence/DPC. | **venus submit must DRIVE the WDDM fence** (memory: "null-engine is wrong"): the venus completion/fence id maps to `DXGKARGCB_NOTIFY_INTERRUPT_DATA` DMA-complete → VidSch fence progress. |
| **Display path** | VRD (render-only) **paired by default** with Hyper-V Video / Remote Display (display-only); "all UI rendering occurs with the VRD adapter" (`gpu-paravirtualization.md:61-68`); desktop remoted via IDD/terminal-session (`:94-106`). | Full **display** driver: its own VidPN, scanout, present, EDID, monitor child. | Render-**only**; DWM composites the whole desktop on Helios; the **Looking Glass IDD** is the paired display-only adapter and captures the OS-composed frame via the standard IddCx swapchain. *Same pairing shape as GPU-PV's VRD↔Hyper-V-Video, with LG-IDD substituted.* |

---

#### A9.6 RECOMMENDATION: GpuMmu (decorative), NOT IoMmu

**Choose GpuMmu.** The evidence is unanimous:

1. **GPU-PV mandates it.** `gpu-paravirtualization.md:1093`: *"The driver must support [GpuMmu]."* The render-only/proxied model that Helios is imitating is GpuMmu-only; there is no physical-access or IoMmu-only render variant documented.

2. **viogpu3d, the working virtio-gpu 3D template, uses it by default.** Zero `IoMmu` declarations anywhere in the tree; it ships a single aperture segment and lets WDDM default to GpuMmu (`viogpu_adapter.cpp:533-564`, and the no-match `grep`). The closest *real working virtio-gpu precedent* picked GpuMmu, which is the strongest single data point.

3. **The bindgen surface is asymmetric in GpuMmu's favor.** There is a full `DXGK_GPUMMUCAPS` query (`DXGKQAITYPE_GPUMMUCAPS=13`, struct at `dxgk_bindings_dump.rs:47944`) but **no** `DXGKQAITYPE_IOMMUCAPS` / `_DXGK_IOMMUCAPS`; IoMmu is just a `_DXGK_NODEMETADATA::IoMmuSupported: BOOLEAN` (line 31676) + a `UseIoMmu` bit (line 43393) + feature 36. IoMmu in WDDM means *the GPU dereferences guest system-physical pages directly through the platform IOMMU* — the exact opposite of zero-copy venus, where the **host** GPU owns the real MMU and we never want Windows to hand the GPU guest physical addresses. IoMmu would also pull in the GPU-PV isolation requirements (`gpu-paravirtualization.md:85` "VM creation fails if the driver doesn't support IoMmu isolation"), which are irrelevant to a virtio-gpu function driver.

   Concretely for Step 2: report `GpuMmuSupported = TRUE`, `IoMmuSupported = FALSE` in `_DXGK_NODEMETADATA`, and answer `DXGKQAITYPE_GPUMMUCAPS` with a benign `_DXGK_GPUMMUCAPS` (a modest `VirtualAddressBitCount`, a `PageTableUpdateMode`, `PageTableLevelCount` ≥ 1). The page tables produced via `BuildPagingBuffer` are decorative — never read by the host GPU.

4. **Do NOT set `ParavirtualizationSupported` (bit 10 of `DXGK_VIDMMCAPS`, accessor `set_ParavirtualizationSupported`, `dxgk_bindings_dump.rs:49942`).** That cap is a *host-KMD* contract (`gpu-paravirtualization.md:133`, "The host KMD sets this cap if it implements all the DDIs described in this section") — `DxgkDdiSetVirtualMachineData`, VM-worker/VM-process `DxgkDdiCreateProcess` flags, `hKmdProcessHandle` host-process-context translation, `DxgkCbSignalEvent` guest-event signaling — none of which Helios implements. Setting it would make Dxgkrnl drive the GPU-PV proxying path we cannot service. viogpu3d, correctly, leaves it unset.

**Net model:** GpuMmu = TRUE / IoMmu = FALSE / ParavirtualizationSupported = FALSE; one over-sized aperture-flavored segment (à la viogpu3d's `DXGK_SEGMENTDESCRIPTOR3` with `Aperture=TRUE, CacheCoherent=TRUE`, but over-sized so nothing evicts and **no paging**); WDDM allocations ↦ venus resource ids; fence driven by venus submit completion; CPU/IDD readback via host-visible MAP_BLOB blobs.

---

#### A9.7 The VRD-pairing consequence (Section H risk input): why DWM picks Helios, and why a broken Helios crash-loops DWM

The GPU-PV VRD text (`gpu-paravirtualization.md:68`, "By default, the VRD is paired with the Hyper-V Video adapter, so **all UI rendering occurs with the VRD adapter**") describes the *exact behavior Helios depends on*: a render-only adapter paired with a display-only adapter ends up doing all of DWM's compositing. For Helios the pairing is Helios-render ↔ Looking-Glass-IDD-display.

- **This is what we WANT.** The LOCKED goal is precisely "DWM composites the WHOLE desktop ON Helios." The render-only-adapter-does-all-UI-rendering behavior is the mechanism that achieves it, with the IDD then capturing the OS-composed frame via the standard IddCx swapchain (no UMD `pfnPresent` needed). This is also why the *cross-adapter* alternative (A9.2) is wrong — there DWM would composite on the software/integrated side and the desktop stays software-composited.

- **This is also exactly why a broken Helios crash-loops DWM.** Because DWM does its compositing *on the render adapter*, DWM's own `D3D11CreateDevice` runs against Helios. The crash-loop root cause (`dwm-crash-vrd-pairing-rootcause.md`): "DWM composites on render-only Helios (paired w/ display-only LG IDD), its `D3D11CreateDevice` fails (`DXGI_ERROR_UNSUPPORTED`) → fatal fail-fast." Any defect in Helios's render path is therefore not a degraded-but-running condition — it is a **fatal-fast DWM crash**, because the desktop compositor itself is the first and most demanding client of the adapter. The pairing that gives us a hardware-accelerated desktop is the same pairing that makes DWM's stability strictly gated on Helios's render correctness.

- **Risk corollary for Step 2:** there is no "soft launch." The moment Helios binds at Code 0 and the VidMm model is accepted, DWM will select it and immediately exercise `D3D11CreateDevice` + present-class compositing on it. The fake-VidMm model must be coherent *enough that DWM's device creation and per-frame composite succeed* before first desktop paint, or the system enters the crash-loop. The `GpuVirtualizationFlags` 0x8 "disable pairing" lever does **not** apply (Helios is not a paravirtualized adapter — confirmed dead in `dwm-crash-vrd-pairing-rootcause.md`), so we cannot opt out of the pairing to buy time; correctness of the GpuMmu/fence/readback model is the only path. Validation per project memory ("DWM NOT FIXED — only proof = LG frames in Linux") must be end-to-end: Looking-Glass frames arriving on the Linux host, not merely Code 0.

**Files referenced (all absolute):**
- `/home/rupansh/helios-vgpu/windows-driver-docs-research-only/windows-driver-docs-pr/display/gpu-paravirtualization.md`
- `/home/rupansh/helios-vgpu/windows-driver-docs-research-only/windows-driver-docs-pr/display/rendering-on-a-discrete-gpu-using-cross-adapter-resources.md`
- `/home/rupansh/helios-vgpu/virtio-research-only-3d/viogpu/viogpu3d/viogpu_adapter.cpp` (lines 447–568)
- `/home/rupansh/helios-vgpu/dxgk_bindings_dump.rs` (`_DXGK_VIDMMCAPS` 49550–49552 + `ParavirtualizationSupported` accessor 49938–49946; `_DXGK_NODEMETADATA` 31671–31677; `_DXGK_GPUMMUCAPS` 47944–47950; `_DXGK_DRIVERCAPS` 51069–51110; QAITYPE consts 44524–44528; `UseIoMmu` 43393)

## Section H — Ranked open questions & risks for Step 2 (+ debug plan & implementation order)

This is the implementer's risk register, ordered by how likely each item is to *block the whole approach* (not by effort). It synthesizes the findings across Sections A–G. Each risk names the failure mode, the evidence, and the concrete mitigation / how to resolve it early.

### H.1 — RISK #1 (the bet): does VidMm accept a *fully decorative* GpuMmu? — UNPROVEN

**This is the gating question for the entire approach.** The locked model declares GpuMmu but never programs a real page table (venus addresses by opaque id; the host GPU owns the real MMU). Section A3.7 argues this is *within the letter of the contract*: the docs say the page-table hardware format is *"unknown to VidMm and is abstracted through DDIs"* (`gpummu-model.md:16`) and the KMD merely *"uses this information to build hardware-specific page table entries"* (`gpu-virtual-address.md:40`) — so a KMD that translates `DXGK_PTE` into nothing is compliant. **But there is no working precedent for a *declared decorative* GpuMmu on a virtio guest** — viogpu3d, the closest analogue, declares *no* memory model at all (WDDM 1.3 + aperture + transfer; Section A9.3). So the bet is genuinely untested.

- **What is safe** (A3.7, "decorative"): the *content* of every `DXGK_PTE`, the PTE-translation step in `BuildPagingBuffer`/`UpdatePageTable`, the root-page-table memory layout, `DXGKARG_SETROOTPAGETABLE::Address`, the GPUVA bit decomposition. Nothing reads these.
- **What is NOT optional** (A3.7, "VidMm-internal"): the *existence and success* of every page-table DDI; a **real, non-evicting segment** that backs page-table allocations (`DXGK_PAGE_TABLE_LEVEL_DESC::PageTableSegmentId` — VidMm physically carves page-table-sized blocks there); the GPUVA-assignment/paging-fence lifecycle; and **self-consistent** `DXGK_GPUMMUCAPS` / `DXGK_PAGE_TABLE_LEVEL_DESC` geometry (once declared, VidMm drives `UpdatePageTable`/`SetRootPageTable` against exactly that geometry).
- **The precise unknown to settle first:** *does VidMm ever read back PTE content it wrote, or cross-check the page tables against allocations?* The docs give no indication it does (it tracks mappings in its own structures). If it does not, decorative works. If it does, the page tables must be structurally real (a much larger job). **Resolve this with the kernel debugger before building anything else** (H.10, step 0–1).
- **Fallback ladder if decorative GpuMmu is rejected:** (a) make the page tables *structurally* real (VidMm-allocated page-table segment + `UpdatePageTable` writes self-consistent `DXGK_PTE`s into the page-table allocation, still never read by hardware) — heavier but still decorative-in-effect; (b) drop to viogpu3d's exact model (no declared MMU, aperture segment + real `BuildPagingBuffer MapApertureSegment` + `RESOURCE_ATTACH_BACKING` + `TRANSFER_TO_HOST`) — **proven to work**, but it is a *copy* model that abandons zero-copy host-visible blobs (Section D contrast). The fundamental tension (H.4) is that zero-copy wants a CpuVisible *memory* segment, while the only *proven* virtio WDDM model is aperture + copy.

### H.2 — RISK #2: there is no soft launch — any defect fatally crash-loops DWM

Because a render-only adapter paired with a display-only adapter does *all* UI rendering (GPU-PV VRD behavior, Section A9.7), the instant Helios binds at Code 0 the OS pairs it with the LG IDD and **DWM runs `D3D11CreateDevice` + per-frame compositing on Helios**. Any failure is a **fatal fail-fast** (`dwm-crash-vrd-pairing-rootcause`: DWM treats a failing `D3D11CreateDevice` on its committed adapter as `RaiseFailFastException`), and `LogonUI` likewise crash-loops on a half-working device (observed with the Gate-5b stub UMD). The `GpuVirtualizationFlags` 0x8 "disable pairing" lever does **not** apply (Helios is not a paravirtual adapter — confirmed dead in `dwm-crash-vrd-pairing-rootcause`), so we cannot opt out of the pairing to buy iteration time.

- **Mitigation:** do *all* memory-model + fence bring-up against a **standalone D3DKMT harness** with the adapter forced over Red Hat viogpudo (or `HELIOS_DISABLE_VIRTIO_GPU=1`), so DWM never sees a half-working Helios. Keep a known-good rollback DriverStore `.sys`/`helios_umd.dll`. Only bind Code 0 once CreateDevice → allocation → SubmitCommand+fence → a smoke compose path all pass in the harness. **Detach ntoseye while DWM/LogonUI run** — a first-chance `int3` halts the whole VM (Gate-5b finding).
- **Validation gate:** per project memory, the *only* proof DWM works is **Looking-Glass frames arriving on the Linux host** — not Code 0, not the absence of crash-log entries.

### H.3 — RISK #3: fence reconciliation is a real ICD/UMD change, not just a KMD edit

The coherent fence (Section C.1, A6.9) requires routing the composition venus stream through `DxgkDdiRender` + `DxgkDdiSubmitCommand` (Option 1) instead of out-of-band `D3DKMTEscape`, making `dxgkddi_interrupt_routine` + `dxgkddi_dpc_routine` real (both stubs today), and an async submit + used-ring drain. Risk: getting DIRQL/DISPATCH discipline and the in-flight-fence pool right; the current synchronous immediate-fence *masks* ordering bugs that will surface once it is removed. **Mitigation:** port the System-class async-submit pattern (`phase4e-async-submit`); validate fence ordering with a D3DKMT submit+wait harness before DWM is in the loop.

### H.4 — RISK #4: segment acceptance & where DWM's composed primary gets its CPU pointer

A CpuVisible **memory** segment (the zero-copy ideal) is *rejected by VidMm right after `DxgkDdiCreateDevice`* today, regardless of size, because no memory model is declared (`query_adapter_info.rs:249–262`; Section A2, A7-C). Whether declaring GpuMmu (H.1) flips that to accepted is **the second unknown**, and it is coupled to a design fork: **how does the OS-composed primary get a CPU pointer?**

- DWM and the runtime call `D3DKMTLock2`, which VidMm services from the **segment descriptor** — there is no `DxgkDdiLock` callback (Section C.2). For that to land on venus-backed pages, the CpuVisible *memory* segment must be accepted (depends on H.1).
- Helios's proven mechanism is `D3DKMTEscape MAP_BLOB`, which DWM/the runtime will **not** call (only the ICD/IDD do). So either the composed primary must be Lock2-able (needs the accepted memory segment), **or** the IDD reads it as a HOST3D blob via Escape/`CHeliosSink` (Section F) while DWM writes it as a render target (no CPU read on the DWM side).
- **Resolve with the kernel debugger:** breakpoint the allocation/Lock path and observe what DWM and the IDD actually call on the composed-primary allocation.

### H.5 — RISK #5: `DxgkDdiGetStandardAllocationDriverData` is missing — DWM cannot make its surfaces

`STATUS_NOT_IMPLEMENTED` today (`create_allocation.rs:242–248`; Sections A5, E). DWM/the runtime allocate their composition surfaces (shared primary, shadow, staging, GDI redirection) through this DDI; with it unimplemented they cannot create a primary on Helios at all. Architecturally low-risk (mechanical) but a **hard functional blocker**: Step 2 must map each `D3DKMDT_STANDARDALLOCATIONTYPE` (and the GDI surface types incl. `..._CPUVISIBLE_CROSSADAPTER`) to a venus-backed blob with the right size/pitch (Section A5, B2). `DxgkDdiDescribeAllocation` (also `NOT_IMPLEMENTED`) is the paired read-back DDI and must become real once submits reference allocations.

### H.6 — RISK #6: how the composed primary reaches a render-only adapter's IDD

Helios is render-only — no VidPN, no scanout — so it never executes `SetVidPnSourceAddress`/flip (Section A11). The composed primary is an OS-composited texture handed to the IDD via the standard `IddCxSwapChainReleaseAndAcquireBuffer` (no UMD `pfnPresent` involved). Open question: **does the OS require any present/flip DDI from a render-only adapter that is the composition target, or is pure IddCx acquire sufficient?** A11 concludes Helios can omit the present DDIs; confirm with the debugger once DWM composites. The concrete IDD work (Section F): drop the WARP force-select (`CIndirectDeviceContext.cpp:182–213`) and replace the IDD's **D3D12** copy queue with **D3D11** (Helios has no D3D12) or wire `CHeliosSink`'s Vulkan dma-buf import.

### H.7 — RISK #7: caps you must declare drive VidMm into paths you must then back

Two coupled cap hazards:
- **Existing load-bearing caps are unbacked** (`kmd-render-wddm-bringup`): `FlipOnVSyncMmIo`, `SectionBackedPrimary`, `SupportKernelModeCommandBuffer`, `PreemptionAware` are mandatory for Code-0 load but sit on null paths. Declaring GpuMmu adds `DXGK_GPUMMUCAPS` geometry (`VirtualAddressBitCount`, `PageTableLevelCount`, `PageTableUpdateMode`) + per-level `DXGK_PAGE_TABLE_LEVEL_DESC` that VidMm will then *exercise* via `SetRootPageTable`/`GetRootPageTableSize`/`UpdatePageTable`. Declare the **minimum self-consistent** geometry (e.g. modest `VirtualAddressBitCount`, `PageTableLevelCount = 1` if a single-level scheme is accepted, a `PageTableUpdateMode` whose `DXGK_PAGETABLEUPDATEADDRESS` form you can service as a no-op).
- **Do NOT set `ParavirtualizationSupported`** (bit 10 of `DXGK_VIDMMCAPS`, Section A9.6). It is a *host-KMD* GPU-PV contract (`DxgkDdiSetVirtualMachineData`, VM-process flags, host handle translation, `DxgkCbSignalEvent`) Helios does not implement; setting it makes dxgkrnl drive the GPU-PV proxy path. viogpu3d correctly leaves it unset.

### H.8 — RISK #8: residency soundness is conditional on segment acceptance

The pin + over-size + no-`ApplicationTarget` residency design (Section C.3) is correct and means nothing ever evicts — **but only once VidMm accepts the segment shape** (H.4). Over-sizing alone does not pass the segment-acceptance gate; the GpuMmu declaration (H.1) is the prerequisite. Set `EvictionSegmentSet = 0` per allocation (already done, `create_allocation.rs:118`); consider `PermanentSysMem` (bit 1, Section C.2) for the WDDM-native "pinned" semantics; leave `DXGK_SEGMENTFLAGS2::ApplicationTarget` unset so the segment stays out of per-process budgeting.

### H.9 — RISK #9: the duplicate-LUID / DXGI-index anomaly

`dwm-crash-vrd-pairing-rootcause` records that one Helios PnP device enumerates as **two** dxgkrnl LUIDs at DXGI indices [0] and [1], ahead of Basic Render. DWM picks index [0]. This is unexplained and may interact with pairing/selection. Investigate during bring-up (it may be benign once `D3D11CreateDevice` succeeds, but a duplicate adapter could confuse IDD render-adapter selection in Section F).

---

### H.10 — The kernel-debugger plan (how to resolve H.1/H.4/H.6 cheaply)

The unknowns above are answered by watching VidMm's behavior under the debugger, not by guessing. The proven Helios debug loop (from `kmd-render-wddm-bringup`, `gate5a-venus-d3dkmt`):

1. **Drive a standalone D3DKMT harness** (no DWM): open the adapter by LUID, `D3DKMTCreateDevice` → `CreateContext` → declare-GpuMmu path → `CreateAllocation` → `Lock2`/Escape → `Render`/`SubmitCommand` + `WaitForSynchronizationObject`. This exercises the whole memory+fence model with the adapter forced over viogpudo and **DWM uninvolved** (sidesteps H.2). Reuse `tools/d3dkmt_alloc_probe.c`.
2. **ntoseye address breakpoints via the linker `.map`:** RVAs from `target/.../helios_kmd_render.map` (cols: `sect:off  _RNvNt…name  preferredVA = 0x180000000 + RVA`); loaded base from `kernel_modules helios_kmd_render.sys`; `bp` at `base + RVA` on `query_adapter_info` (the GPUMMUCAPS/level-desc/segment fills), `build_paging_buffer` (`UpdatePageTable`), `create_allocation`, `set_root_page_table`. **Watch the post-`CreateDevice` reject signature** — the gate5a finding was `CreateDevice` → (nothing) → `RemoveDevice` (`S57/58 → S59/60`), i.e. VidMm rejects the segment/model *after* `CreateDevice` returns. If declaring GpuMmu changes that to "accepted + first `CreateAllocation` + first `SetRootPageTable`", the decorative bet holds.
3. **Diag-ring gotcha:** the circular `diag::record` ring is flooded within ~1 s by steady-state `QueryAdapterInfo` perf-polls (`NODEPERFDATA 0x18` / `ADAPTERPERFDATA 0x19`) — already gated in `query_adapter_info.rs:40–44`. For one-shot bring-up breadcrumbs use **dedicated registry values** or **DISPATCH-safe atomics** (as `build_paging_buffer.rs` does with `PAGING_LAST_OP`/`PAGING_CALL_COUNT`), not the ring; `RtlWriteRegistryValue` is PASSIVE-only, illegal in the DISPATCH-level paging/submit DDIs.
4. **Host Vulkan validation:** run the render server with `HELIOS_VKR_DEBUG=validate` (needs `vulkan-validation-layers` on the host) to catch venus-side spec violations during compose bring-up (this is how the NVIDIA external-memory-bind UB was found).

---

### H.11 — Recommended implementation order for Step 2

This sequence front-loads the two unknowns (H.1, H.4) so the approach is validated or refuted before the expensive DWM-integration work, and keeps DWM out of the loop (H.2) until the model is proven.

0. **Standalone-harness bring-up (no DWM).** Force Helios over viogpudo at Code 0 with the adapter *not* the desktop compositor's target, attach ntoseye, run the D3DKMT harness. Goal: a controlled environment to answer H.1/H.4.
1. **Declare the decorative GpuMmu** (Section A1/A3/B): set `DXGK_NODEMETADATA::GpuMmuSupported = TRUE`, `IoMmuSupported = FALSE`; answer `DXGKQAITYPE_GPUMMUCAPS` with minimal self-consistent geometry; fill `DXGK_PAGE_TABLE_LEVEL_DESC` per level; implement `GetRootPageTableSize` (return a consistent `NumberOfPte`), `SetRootPageTable` (record-and-ignore), and `BuildPagingBuffer/UpdatePageTable` (consume the `DXGK_PTE` array, no-op). **Then re-attempt the CpuVisible MEMORY segment** (the one rejected today) and confirm via debugger whether VidMm now accepts it post-`CreateDevice`. **This step answers RISK #1 and #4.** If rejected, drop to the H.1 fallback ladder before proceeding.
2. **Make the fence coherent** (Section C.1, Option 1): real `dxgkddi_interrupt_routine` (claim MSI-X, `QueueDpc`) + `dxgkddi_dpc_routine` (drain used ring → `DxgkCbNotifyInterrupt(DMA_COMPLETED, fence)` → `NotifyDpc`); async submit; route the venus stream through `DxgkDdiRender` + `DxgkDdiSubmitCommand` with `HeliosWddmCmdBuf`; delete the synchronous immediate fence. Validate fence ordering in the harness.
3. **Make allocations DWM-complete** (Section A5): implement `DxgkDdiGetStandardAllocationDriverData` (map each standard type → venus blob) and `DxgkDdiDescribeAllocation`. Confirm the runtime can create a standard primary/staging/shadow surface on Helios.
4. **Wire the venus mapping** (Section D): the composed-primary blob is `KIND_DEVICE_MEMORY`, `BLOB_MEM_HOST3D`, `USE_MAPPABLE`, backed by a `HOST_VISIBLE` venus `VkDeviceMemory`; composition RTs are GPU-only (no `USE_MAPPABLE`); the ring is `KIND_SHMEM`. Keep `EvictionSegmentSet = 0`.
5. **Bind Code 0 and let DWM composite** (accepting RISK #2): rollback DLL staged, ntoseye detached. Watch for fatal-fast; iterate.
6. **IDD** (Section F): drop the WARP force-select; port the IDD copy path D3D12→D3D11 (or enable `CHeliosSink`). **Validate end-to-end by Looking-Glass frames on the Linux host** (the only real proof, RISK #2).

---
