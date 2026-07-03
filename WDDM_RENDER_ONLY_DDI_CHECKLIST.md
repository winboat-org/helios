# WDDM Render-Only DDI Checklist

Status: Gate 1 bring-up tracker for `WDDM_RENDER_ONLY_3_2.md`.

Purpose: before writing code, list the driver contracts that must be designed, implemented, or explicitly marked
unsupported. This file is intentionally conservative. It should become the implementation tracker.

## Rules

- A callback is not "done" because it returns `STATUS_SUCCESS`.
- A capability is not advertised until the callback path behind it is implemented and tested.
- A "not supported" result is valid only when Windows permits it for the advertised WDDM version and caps.
- Escapes are not the render ABI.
- DXGI remains Windows DXGI.

## KMD Adapter Lifecycle

- Driver initialization through the WDDM render miniport path.
- INF/service/class setup for a render-capable display miniport.
- Add/start/stop/remove device lifecycle.
- PnP and power transitions.
- BAR/resource mapping.
- PCI config access.
- Interrupt resource selection and teardown.
- Adapter LUID and identity.
- WDDM 3.2 version/capability reporting.
- Feature support/enabled query reporting.
- TDR state capture and reset/recovery entry points.

Design questions:

- Exact WDK headers and Rust bindgen scope.
- Whether to keep Rust KMD or isolate a C/C++ WDDM bring-up shim.
- How to preserve the current System-class path during early development.

Implementation notes, 2026-06-17:

- New crate: `kmd_render`, separate from the working System-class `kmd`.
- Package identity: `helios_kmd_render.sys`, service `helios_kmd_render`, catalog `helios_kmd_render.cat`.
- INF validation: `inf2cat` clean, `infverif /v /w` valid, test-signed with `WDRLocalTestCert`.
- WDDM header target: WDK 10.0.26100, native `DXGKDDI_INTERFACE_VERSION`.
- StartDevice reports zero video present sources/children. It attempts virtio-gpu transport initialization and records
  failures, but Gate 1 keeps PnP start successful with transport disabled so install/reboot safety can be proven first.
- The render package now reaches `DxgkDdiStartDevice`; ntoseye confirmed VidMm initialization succeeds and the
  remaining Code 43 gate is later in `dxgkrnl!ADAPTER_RENDER::Initialize`.
- The package now builds `helios_umd.dll`, copies it into the cataloged package, and registers it through
  `UserModeDriverName` / `InstalledDisplayDrivers` for the current viogpu3d-style probe. The DLL entrypoints log the
  UMD load path and still fail `CreateDevice` explicitly until the UMD/KMD contract is real.
- Display/VidPn/boot-display DDIs are registered as explicit unsupported callbacks; they still must not advertise any
  scanout sources.
- `oem126.inf` / version `22.22.32.45` failed because `DXGKQAITYPE_HISTORYBUFFERPRECISION` returned
  `PrecisionBits = 10`. Dxgkrnl validates that value during render-adapter initialization and accepts only 32..64,
  then returned `STATUS_OBJECT_NAME_NOT_FOUND` (`0xC0000182`) from `ADAPTER_RENDER::Initialize+0x1633`.
- `22.22.32.46` fixed history-buffer precision (`PrecisionBits = 32`), moving past
  `ADAPTER_RENDER::Initialize`.
- `22.22.32.47` / `.48` added structurally valid UMD `OpenAdapter10`, `OpenAdapter10_2`, and `OpenAdapter12`
  adapter-open tables. ntoseye showed the active post-history failure was not UMD dispatch; the call target resolved
  to `dxgmms2!VidMmInitializePagingProcess`.
- `22.22.32.49` made `DxgkDdiCreateContext` return a real boxed context handle plus conservative DMA/list sizing
  instead of `STATUS_NOT_IMPLEMENTED`. This unblocked `dxgmms2!VidSchCreateSystemDevices` and the device now starts
  Code 0 as `oem129.inf`.
- `22.22.32.50` corrected the UMD adapter-open ABI: `OpenAdapter10` now fills the 3-entry
  `D3D10DDI_ADAPTERFUNCS`, `OpenAdapter10_2` fills the 5-entry `D3D10_2DDI_ADAPTERFUNCS`, the `GetCaps` argument
  layout includes `pInfo`, and `OpenAdapter12` exposes the full base adapter function table shape.
- `22.22.32.51` returns a real D3D10/D3D11 supported-version list and logs UMD calls to
  `C:\Windows\Temp\helios_umd.log`. `dxdiag` loads the DLL as `oem131.inf`, reports the adapter as render-only WDDM
  3.2 with no device problem, and reaches `OpenAdapter10_2` -> `GetCaps` -> `GetSupportedVersions` ->
  `CalcPrivateDeviceSize` -> `CreateDevice`. The next gate is the intentional `CreateDevice -> E_NOTIMPL`.
- Mesa reuse boundary: the existing Mesa Venus ICD transport is reusable for eventual rendering, but it is not itself
  a WDDM D3D UMD. Mesa's Gallium `d3d10umd` frontend is the closest reference for the adapter/device DDI tables; the
  Helios UMD still needs a WDDM-specific D3D shim that can call KMT/escape paths and later route rendering into the
  Venus transport.
- Correction to the previous correction: the in-box Basic Render Driver uses `Class=System`, but that did not prove
  the right shape for a third-party render package. A BasicRender-style System-class Helios package still failed at
  the earlier AddDevice-before-StartDevice point. The current viogpu3d-style Display-class package with UMD
  registration moves past that gate and is now failing on render-adapter capability validation.
- `oem116.inf` additionally registered scheduler/platform DDIs (`QueryDependentEngineGroup`, `QueryEngineStatus`,
  `ResetEngine`, `CancelCommand`, clock/history/power/VM hooks). The failure point did not move, so callback
  table completeness in that group is not the sole gate.
- `DxgkDdiAddDevice` now initializes the output miniport context pointer to NULL before allocation, matching
  `viogpu3d`; the Rust allocator already uses nonpaged pool. `oem117.inf` tested this cleanup and still failed with
  the unchanged AddDevice/RemoveDevice-only breadcrumb sequence.
- Local MicrosoftDocs research (`windows-driver-docs-research-only/windows-driver-docs-pr/display`):
  - `wddm-driver-and-feature-caps.md` defines render-only WDDM as either render DDIs with display DDIs NULL, or full
    DDIs with `NumVidPnSources = 0` and `NumVidPnTargets = 0`.
  - Mandatory WDDM 1.2+ render-only caps include `WDDMVersion`, preemption caps, `FlipOnVSyncMmIo`,
    `SupportPerEngineTDR`, and `SupportKernelModeCommandBuffer`.
  - `wddm-v1-2-driver-enforcement.md` states that dxgkrnl validates mandatory WDDM features and fails adapter
    creation if a driver claims a WDDM level without coherent required caps.
  - `gpu-paravirtualization.md` confirms the VRD/render-only model: the runtime is unchanged, VRD can pair with a
    display-only adapter, and Direct3D adds a logical display output when an app selects the render-only adapter.
  - INF docs still treat `InstalledDisplayDrivers` / `UserModeDriverName` as standard WDDM display-driver software
    settings; omitting them is only proven for Microsoft BasicRender, not for our third-party render package.
- Next probe intentionally targets a WDDM 1.3 `viogpu3d`-style baseline first, with coherent caps and UMD registry
  state, to isolate INF/ABI/load gates before porting the proven shape back to the WDDM 3.2 Rust implementation.
- The package is suitable for controlled binding smoke tests only. It reaches Code 0, but real D3D device creation
  still intentionally fails until the UMD/KMD render contract exists.

Local WDK inventory:

- Installed WDK header root: `C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0`.
- Render-driver initialization table: `km\dispmprt.h`, `DRIVER_INITIALIZATION_DATA`.
- WDDM version enum: `shared\d3dkmddi.h`, `DXGKDDI_WDDMv3_2 = 0x3200`.
- Do not use `KMDDOD_INITIALIZATION_DATA` / `DxgkInitializeDisplayOnlyDriver` for this feature; that is the
  display-only path used by the virtio DOD reference.

`DRIVER_INITIALIZATION_DATA` callback groups to classify before code:

- Base PnP/lifecycle: `DxgkDdiAddDevice`, `DxgkDdiStartDevice`, `DxgkDdiStopDevice`,
  `DxgkDdiRemoveDevice`, `DxgkDdiResetDevice`, `DxgkDdiUnload`, `DxgkDdiDispatchIoRequest`.
- Interrupt/power/ETW/interface: `DxgkDdiInterruptRoutine`, `DxgkDdiDpcRoutine`, `DxgkDdiSetPowerState`,
  `DxgkDdiNotifyAcpiEvent`, `DxgkDdiQueryInterface`, `DxgkDdiControlEtwLogging`.
- Adapter/device/allocation: `DxgkDdiQueryAdapterInfo`, `DxgkDdiCreateDevice`, `DxgkDdiDestroyDevice`,
  `DxgkDdiCreateAllocation`, `DxgkDdiDestroyAllocation`, `DxgkDdiDescribeAllocation`,
  `DxgkDdiGetStandardAllocationDriverData`, `DxgkDdiOpenAllocation`, `DxgkDdiCloseAllocation`.
- Render scheduling core: `DxgkDdiCreateContext`, `DxgkDdiDestroyContext`, `DxgkDdiRender`,
  `DxgkDdiPresent`, `DxgkDdiPatch`, `DxgkDdiSubmitCommand`, `DxgkDdiPreemptCommand`,
  `DxgkDdiBuildPagingBuffer`, `DxgkDdiQueryCurrentFence`.
- Timeout/recovery/diagnostics: `DxgkDdiResetFromTimeout`, `DxgkDdiRestartFromTimeout`,
  `DxgkDdiCollectDbgInfo`, `DxgkDdiCollectDiagnosticInfo`, `DxgkDdiQueryDiagnosticTypesSupport`,
  `DxgkDdiControlDiagnosticReporting`.
- WDDM 2.x process/GPUVA/HW queue path: `DxgkDdiCreateProcess`, `DxgkDdiDestroyProcess`,
  `DxgkDdiSubmitCommandVirtual`, `DxgkDdiSetRootPageTable`, `DxgkDdiGetRootPageTableSize`,
  `DxgkDdiMapCpuHostAperture`, `DxgkDdiUnmapCpuHostAperture`, `DxgkDdiCreateHwContext`,
  `DxgkDdiDestroyHwContext`, `DxgkDdiCreateHwQueue`, `DxgkDdiDestroyHwQueue`,
  `DxgkDdiSubmitCommandToHwQueue`, `DxgkDdiSwitchToHwContextList`.
- WDDM 3.0/3.1 sync/submission path: `DxgkDdiCreateCpuEvent`, `DxgkDdiDestroyCpuEvent`,
  `DxgkDdiCreateNativeFence`, `DxgkDdiDestroyNativeFence`, `DxgkDdiOpenNativeFence`,
  `DxgkDdiCloseNativeFence`, `DxgkDdiUpdateMonitoredValues`, `DxgkDdiUpdateCurrentValuesFromCpu`,
  `DxgkDdiCreateDoorbell`, `DxgkDdiConnectDoorbell`, `DxgkDdiDisconnectDoorbell`,
  `DxgkDdiDestroyDoorbell`, `DxgkDdiNotifyWorkSubmission`.
- WDDM 3.2 additions in the installed WDK: `DxgkDdiCreateMemoryBasis`, `DxgkDdiDestroyMemoryBasis`,
  `DxgkDdiStartDirtyTracking`, `DxgkDdiStopDirtyTracking`, `DxgkDdiQueryDirtyBitData`,
  `DxgkDdiPrepareLiveMigration`, `DxgkDdiSaveImmutableMigrationData`,
  `DxgkDdiSaveMutableMigrationData`, `DxgkDdiEndLiveMigration`, `DxgkDdiRestoreImmutableMigrationData`,
  `DxgkDdiRestoreMutableMigrationData`, `DxgkDdiWriteVirtualizedInterrupt`,
  `DxgkDdiSetVirtualGpuResources2`, `DxgkDdiSetVirtualFunctionPauseState`,
  `DxgkDdiSetNativeFenceLogBuffer`, `DxgkDdiUpdateNativeFenceLogs`, `DxgkDdiCollectDbgInfo2`,
  `DxgkDdiNotifyContextPriorityChange`, `DxgkDdiResetDisplayEngine`.

The display/VidPN callbacks are still present in `DRIVER_INITIALIZATION_DATA`, but a render-only adapter should
not accidentally become a DOD/display owner. Each display callback must be classified as required, optional, or
intentionally unsupported for render-only operation.

## Local viogpu3d Reference

The corrected `virtio-research-only-3d` checkout includes a WDDM render miniport reference at
`virtio-research-only-3d/viogpu/viogpu3d`:

- Branch/commit observed locally: `viogpu3d`, `9ed3aab1 [viogpu3d] Introduce viogpu 3d driver`.
- Initialization: `driver.cpp` uses `DRIVER_INITIALIZATION_DATA`, `DxgkInitialize`, and
  `DXGKDDI_INTERFACE_VERSION_WDDM1_3`.
- UMD pairing: `BUILDING.md` and `viogpu3d.inx` expect Mesa virgl Gallium D3D10/WGL DLLs, not DXVK/VKD3D.
- Useful files: `driver.cpp`, `viogpu_adapter.cpp`, `viogpu_allocation.cpp`, `viogpu_device.cpp`,
  `viogpu_command.cpp`, plus `viogpu/common/viogpu.h` and `viogpu/common/viogpu_queue.cpp`.

Reference-only patterns to avoid:

- No WDDM 1.3 target. Our target is WDDM 3.2.
- No device/context aliasing. The reference `CreateContext` reuses `hDevice`; ours needs real context objects.
- No partial paging. The reference paging path only handles aperture map/unmap; ours must handle required VidMm
  operations for the advertised caps.
- No no-op patching unless the advertised command model truly requires no patching.
- No success-returning unsupported preemption/TDR/debug callbacks.
- No random allocation description fields.
- No scheduler registry hack disabling preemption.
- No virgl/Gallium D3D10 UMD dependency as a substitute for D3D11/D3D12 UMD work.

## KMD QueryAdapterInfo

Required areas to enumerate before implementation:

- Adapter caps.
- Segment caps.
- Node/engine caps.
- Scheduling caps.
- Memory budget and segment sizes.
- Cross-adapter/resource sharing caps.
- WDDM version and feature support.
- Preemption caps.
- Presentation/display relationship for render-only mode.
- Native/monitored fence capabilities.
- Virtualization/live-migration/dirty-bit tracking flags, if any.

No placeholder caps. Unknown must stay unadvertised.

Gate 1 caps-hardening pass, 2026-06-18 (`22.22.32.55`):

- Verified every advertised `DXGK_DRIVERCAPS` cap bit field-by-field against WDK 10.0.26100 `shared/d3dkmddi.h`:
  `PresentationCaps` bit 2 = `SupportKernelModeCommandBuffer`, `FlipCaps` bit 1 = `FlipOnVSyncMmIo`,
  `SchedulingCaps` bit 0 = `MultiEngineAware` / bit 2 = `PreemptionAware`, `MemoryManagementCaps` bit 3 =
  `SectionBackedPrimary`. All four were correct; the raw `1 << N` writes are now named constants with the WDK
  bit references inline so a future edit cannot silently set the wrong cap.
- **Coherence debt (explicit):** `SupportKernelModeCommandBuffer`, `FlipOnVSyncMmIo`, and `PreemptionAware` are
  MANDATORY for a WDDM 3.2 render-only adapter to load, but their paths are still the null bring-up engine
  (no real `DxgkDdiRenderKm`, flip, or preemptible scheduler). They are advertised because dxgkrnl requires them
  for Code 0, not because they are backed — Gate 2/3 must make them real or stop advertising them.
- The types dxgkrnl asks for at AddAdapter are all answered except `DXGKQAITYPE_PHYSICALADAPTERCAPS` (0x0F), which
  stays `NOT_SUPPORTED`: its `DxgkPhysicalAdapterHandle`/`Flags` contract is undefined for us until we expose real
  execution nodes (defer to Gate 2/3). dxgkrnl tolerates the rejection (Code 0).
- The steady-state poll of `DXGKQAITYPE_NODEPERFDATA` (0x18) and `DXGKQAITYPE_ADAPTERPERFDATA` (0x19) returning
  `NOT_SUPPORTED` is correct, not a gap: those feed the Task Manager GPU tab and virtio-gpu exposes no such
  telemetry. Faking them would violate the "no placeholder caps" rule.

## Device and Context Management

- Create/destroy device.
- Create/destroy context.
- Per-process ownership.
- Per-engine/node routing.
- Context private data lifetime.
- Fault attribution.
- Context scheduling priority policy.
- Cleanup on process exit.

Microsoft notes that `DxgkDdiCreateContext` must support arbitrary context counts except for real memory
exhaustion. The implementation must not bake in a one-context shortcut.

## Allocation and VidMm

- Create allocation.
- Open allocation.
- Close allocation.
- Destroy allocation.
- Describe allocation private data.
- Allocation backing type: system memory, host-visible BAR, Venus blob, staging.
- Allocation placement.
- Residency tracking.
- Eviction handling.
- CPU map/unmap.
- Cache policy.
- Shared handles.
- Keyed mutex/fence/shared sync metadata.
- Resource identity mapping between WDDM allocation and virtio-gpu resource/blob.
- Process cleanup and reference counting.

The existing Helios blob table is an implementation reference only. WDDM allocations become the authoritative
Windows-visible object.

Gate 1 implementation state:

- `DxgkDdiCreateAllocation` creates a KMD allocation handle and fills conservative VidMm metadata.
- `DxgkDdiDestroyAllocation` frees those KMD handles.
- `DxgkDdiCloseAllocation` is a teardown no-op.
- No virtio-gpu resource/blob is created yet.
- The Gate 1 adapter may load with no active virtio transport; real allocation/resource gates must require it.
- No shared-resource, residency, eviction, CPU map/unmap, or Venus object identity is implemented yet.

Gate 2 progress, 2026-06-18 (`22.22.32.57`):

- Added venus resource transport primitives to `kmd_render` (`VirtioGpu::resource_create_blob(ctx_id, blob_mem,
  blob_flags, blob_id, size)` = create_blob → `ctx_attach_resource`, plus `resource_unref`), mirroring the proven
  System-class `kmd::alloc_blob` sequence. The transport already rides the `virtio-drivers` crate (PciTransport /
  VirtQueue / `WdkHal: Hal`), shared with the System-class `kmd`.
- **Key finding (empirical + reference-confirmed): a venus-backed (HOST3D mappable) WDDM allocation cannot be
  created standalone in the KMD.** A bare `RESOURCE_CREATE_BLOB(HOST3D, blob_id=0)` is rejected by the host with
  `RESP_ERR_UNSPEC` (verified live on `.56`), because a HOST3D blob must reference a venus device-memory id produced
  by the UMD's `vkAllocateMemory` venus stream (matches `kmd/src/virtio/gpu.rs::alloc_blob` + the phase4-blob-plan
  record). So the venus-resource allocation path is **coupled to the UMD** (Gate 4+): UMD allocates venus memory →
  hands the KMD the mem id → KMD does create_blob + ctx_attach. CPU-visible/system-memory allocations are a separate
  class that may need no virtio resource at all.
- **Validated standalone (`.57`, live device):** the venus 3D-context lifecycle
  (`ctx_create(VIRTIO_GPU_CAPSET_VENUS)` → `ctx_destroy`) succeeds (`RESP_OK_NODATA`) — a real prerequisite for the
  venus allocation flow — via a diagnostic StartDevice self-test (breadcrumb `0x0B000010` → `0x1100`).
- Gate-order implication: full venus-backed `CreateAllocation` needs UMD-supplied venus memory ids, so it lands
  alongside/after the UMD handshake (Gate 4). The next standalone-testable KMD step is a D3DKMT allocation harness
  driving `CreateAllocation`/`DescribeAllocation` for system-memory allocations (no virtio resource), exercising the
  VidMm bookkeeping path.

## Paging

`DxgkDdiBuildPagingBuffer` needs a real design for:

- Transfer.
- Fill.
- Discard.
- Read physical.
- Write physical.
- Allocation location transitions.
- DMA buffer too small retry behavior.
- Busy allocation behavior.
- Synchronization with virtio/Venus command completion.

This is a hard gate. Do not bring up rendering while paging is fake.

Gate 1 implementation state:

- `DxgkDdiBuildPagingBuffer` records the requested operation and returns success without emitting DMA.
- This is a null paging engine for adapter-load smoke testing only.
- GPU MMU caps remain unadvertised and root page table size remains zero.

## Command Submission

Required design:

- UMD command buffer format.
- KMD validation responsibilities before submit.
- Patch-location model.
- DMA buffer creation and patching.
- `DxgkDdiSubmitCommand` path that cannot fail during normal execution.
- Virtio queue submission.
- Venus command stream packaging.
- Submission fence ID handling.
- Completion path via interrupt/DPC or safe polling bring-up path.
- Queue drain and teardown.
- TDR progress tracking.
- Preemption model.

The current synchronous `SUBMIT_VENUS` path is not sufficient as the WDDM render submission path.

Gate 1 implementation state:

- `DxgkDdiSubmitCommand` immediately completes the submitted fence with
  `DXGK_INTERRUPT_DMA_COMPLETED`, then queues the Dxgkrnl DPC.
- `DxgkDdiQueryCurrentFence` reports the last completed fence.
- `DxgkDdiRender`, `DxgkDdiRenderKm`, `DxgkDdiPatch`, and `DxgkDdiSubmitCommandVirtual` remain disabled.
- This avoids scheduler timeouts during bring-up; it is not real virtio/Venus submission.

## Fences and Synchronization

- Monitored fence strategy.
- WDDM 3.2 native fence strategy, if used.
- CPU wait.
- GPU wait.
- Cross-process sync.
- D3D11 keyed mutex requirements.
- D3D12 fence requirements.
- Vulkan timeline/binary fence mapping in the internal backend.
- Reset and lost-device semantics.

No fence is "signaled" until the host/Venus/virtio completion is real.

## UMD Loader Contracts

D3D11:

- Exported adapter-open entry point.
- DDI version reporting.
- Adapter info query through runtime callbacks.
- Device creation.
- Device function tables.
- Resource, shader, pipeline, command, query, and synchronization DDIs.
- DXGI interop surfaces expected by Windows DXGI.

D3D12:

- Source-level VKD3D/VKD3D-Proton research.
- D3D12 UMD DDI entry/adapter/device/command queue contracts.
- Resource heaps and residency.
- Command list/queue/fence model.
- Descriptor heap and pipeline state mapping.

DXVK/VKD3D code must be adapted behind these loader contracts. Their app-local DLL ABI is not the driver ABI.

Gate 4 handshake mapping, 2026-06-18 (`22.22.32.58`), verified live against WDK 10.0.26100 `d3d10umddi.h`:

- **Handshake order (D3D11):** `OpenAdapter10_2` (runtime opens the adapter at
  `Interface=0x000b0011` = D3DWDDM2_0 minor 17, fills the 5-entry
  `D3D10_2DDI_ADAPTERFUNCS`) → `GetSupportedVersions` → `GetCaps` →
  `CalcPrivateDeviceSize` → `CreateDevice`. Enumeration alone (DXGI/dxdiag) stops
  before `CreateDevice`; only `D3D11CreateDevice` against the Helios adapter
  exercises it (`tools/d3d11_devicecreate_probe.cpp`).
- **`PFND3D10DDI_CREATEDEVICE` = `HRESULT(D3D10DDI_HADAPTER, D3D10DDIARG_CREATEDEVICE*)`.**
  `D3D10DDIARG_CREATEDEVICE` (x64 offsets): `hRTDevice@0`, `Interface@8`,
  `Version@12`, `pKTCallbacks@16` (`D3DDDI_DEVICECALLBACKS`, kernel-thunk
  callbacks), `pDeviceFuncs@24` (the in/out device funcs table union — which
  member is keyed by `Interface`), `hDrvDevice@32` (driver private storage sized
  by `CalcPrivateDeviceSize`), `DXGIBaseDDI@40` (16B: `pDXGIBaseCallbacks` in +
  `pDXGIDDIBaseFunctions*` in/out — **a successful `CreateDevice` MUST fill the
  DXGI base function table**), `hRTCoreLayer@56`, `pUMCallbacks@64` (usermode
  callbacks incl. `SetError`), `Flags@72` (`DISABLE_EXTRA_THREAD_CREATION=0x1`),
  `ppfnRetrieveSubObject@80` (minor>=3). `umd/src/lib.rs` mirrors this struct.
- **Device funcs table is keyed by `Interface`:** `D3D11_0_DDI_INTERFACE_VERSION`
  (`0x000b000a`) → `p11DeviceFuncs` (`D3D11DDI_DEVICEFUNCS`, **152 fn pointers**);
  `D3D11_1` (`0x000b000f`) → `p11_1DeviceFuncs`; D3DWDDM2_x → the WDDM2_x device
  funcs. A successful `CreateDevice` must fill **every** pointer in the chosen
  table (NULL/garbage entries are the `.52/.53` corruption class) plus the DXGI
  base table, and return `S_OK`. Functions that return `void` (e.g. `pfnDraw`)
  cannot "fail honestly" — they no-op silently, which only advances the gate if
  backed by real translation (Gate 5). So a minimal `S_OK` `CreateDevice` is NOT
  a safe middle ground: either it's backed by DXVK internals (Gate 5) or device
  creation fails downstream anyway.
- **Honest failure proven (`.58`):** `create_device` returns `E_NOTIMPL`;
  `D3D11CreateDevice` surfaces `0x80004001` to the app, no crash, Explorer
  survives. The runtime negotiated `Interface=0x000a0009` (= our advertised but
  bogus `ddi_supported(10,9,0)`). **Fix before the real table:**
  `GetSupportedVersions` must advertise only interface(s) whose device funcs
  table is actually implemented (e.g. `D3D11_0_DDI_SUPPORTED`).

## DXGI and Present

Required decisions:

- Render-only adapter enumeration behavior.
- Output enumeration behavior.
- How Windows DXGI creates swapchains for a render-only adapter.
- How rendered frames reach the active display path.
- Looking Glass IDD interaction.
- Cross-adapter shared resource requirements.
- Fullscreen exclusive behavior.
- Flip model support.
- Frame latency object support.
- HDR/color-space reporting.
- Multi-monitor behavior.

This section must be settled before claiming game support.

## Venus/Virtio Mapping

- WDDM allocation to virtio resource/blob mapping.
- Host-visible BAR mapping under VidMm.
- Venus context ownership per WDDM context/process.
- Resource attach/detach.
- Capset query.
- Queue/ring management.
- Error propagation and device-lost mapping.
- Host reset behavior.

Reusable current code:

- Virtio cap scan and queue handling.
- Venus context creation/destruction concepts.
- Blob allocation/map implementation lessons.
- Fence table lessons.
- Mesa Venus backend research.

Not reusable as-is:

- IOCTL ABI as public render submission.
- Launcher-level DXVK/VKD3D DLL routing.
- DXVK `dxgi.dll`.

## Validation Matrix

Bring-up:

- Driver install/uninstall/reboot.
- Device Code 0.
- dxgkrnl load/unload.
- DXGI adapter enumeration.
- D3D runtime attempts with expected success/failure.
- Kernel verifier.
- TDR forced timeout.

KMD:

- Allocation create/destroy stress.
- Paging stress.
- Context create/destroy stress.
- Submit/fence stress.
- Process kill cleanup.
- Device reset cleanup.

UMD:

- D3D11 device creation.
- D3D11 clear/draw/readback.
- D3D11 resource sharing/keyed mutex.
- D3D12 device creation.
- D3D12 command queue/list/fence.
- D3D12 clear/draw/readback.

Integration:

- Windows DXGI debug layer.
- Simple D3D11 sample.
- Simple D3D12 sample.
- Steam-launched game without launch-option DLL injection.
- Looking Glass stability during render workload.
- Host Venus/virglrenderer logs under load.

## Immediate Documentation TODO

- Add exact DDI table names and required callbacks from WDK 10.0.26100 headers.
- Add DXGI render-only/present research notes.
- Add source-level VKD3D/VKD3D-Proton inventory once source is available.
- Add implementation branch plan.
- Add "forbidden stubs" review checklist to PR template or commit review notes.
