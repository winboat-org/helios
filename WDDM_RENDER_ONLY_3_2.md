# Helios WDDM Render-Only 3.2 Roadmap

Status: research/design first draft, 2026-06-17. This is the planning document for the next large feature:
convert Helios from the current System-class KMDF + Vulkan ICD path into a real WDDM render-only adapter,
targeting WDDM 3.2. Implementation has started in the separate `kmd_render` crate after the research pass.

This document supersedes the earlier "WDDM render miniport is abandoned" conclusion only if we commit to doing
the full UMD + KMD work described here. The old conclusion in `ARCH.md` was correct for a shortcut path. It is
not a blocker for a no-shortcuts WDDM render adapter.

## 1. Goal

Build a real Windows graphics stack:

```
Windows app/game
  -> Windows DXGI + Direct3D runtime
  -> Helios D3D11/D3D12 UMD
      -> DXVK-derived D3D11 translation
      -> VKD3D/VKD3D-Proton-derived D3D12 translation
      -> Vulkan command stream
  -> Helios WDDM render-only KMD
      -> WDDM scheduling, memory, paging, fences, TDR
      -> virtio-gpu Venus transport
  -> QEMU/virglrenderer Venus
  -> host Vulkan driver
```

The KMD must be a WDDM display miniport/render adapter, not a System-class side-channel driver and not a
Display-Only Driver. The UMD must be a native Windows Direct3D UMD loaded by the Direct3D runtime, not a
drop-in `d3d11.dll`, `d3d12.dll`, or `dxgi.dll` replacement.

## 2. Non-Negotiables

- No success-returning stubs for required WDDM DDIs. Unsupported operations fail honestly or are not advertised.
- No fake caps. Every advertised feature level, memory model, engine capability, fence capability, format, and
  sharing path must map to real implementation.
- No DXGI replacement. DXVK's `src/dxgi` is research material only; Windows DXGI remains the system DXGI.
- No Wine-specific D3DKMT escape path as a production present or DXGI mechanism. DXVK's
  `D3DKMT_ESCAPE_SET_PRESENT_RECT_WINE` usage is not a model for Helios.
- No D3DKMTEscape render submission bypass. Escapes are acceptable for diagnostics or narrow private controls,
  not as the primary rendering ABI.
- No WDDM render adapter without WDDM memory management, scheduling, command submission, preemption, paging,
  interrupts/fences, and TDR behavior.
- No WDDM 3.2 feature flag unless the implementation satisfies the contract. If a 3.2 feature is still marked
  preliminary or not final by Microsoft, it is tracked but not depended on for the first render path.

## 2.1 VM Launch Ownership

If development or debugging requires changing the standalone VM launch command, `tools/launch-helios-gtk.sh`,
QEMU display/debug transport, or any launcher environment variable, make the source/config change and then ask
the user to run or restart the VM. Do not attempt to launch the VM from automation after such changes unless the
user explicitly asks in the same turn. The launch path depends on the user's desktop session, sudo credentials,
GPU state, and Looking Glass display context.

For kernel-debug work this means: configure guest BCD and document the required host launch command, but after
adding or changing a serial/KDNET/GDB transport, stop and ask the user to run the VM with the updated launch
command.

## 3. Microsoft Contract Summary

The WDDM architecture is split between a user-mode driver and a kernel-mode display miniport. The Direct3D
runtime loads the UMD DLL, GDI/DXGI/D3D interact with `dxgkrnl.sys`, and `dxgkrnl` includes VidMm and VidSch
for video memory management and GPU scheduling.

For D3D11, the runtime initializes the UMD by loading the driver DLL and calling the exported adapter-open entry
point (`OpenAdapter10_2` for the documented D3D11 path). The UMD reports supported DDI versions, calls runtime
callbacks such as `pfnQueryAdapterInfoCb`, and returns device function tables.

On the KMD side:

- `DxgkDdiCreateContext` creates GPU contexts for devices and must support an arbitrary number of contexts,
  failing only for real resource exhaustion.
- `DxgkDdiSubmitCommand` submits DMA buffers to a hardware execution unit. Microsoft documents that errors from
  this callback are fatal to the OS path, so the design must make submission validation happen before this point.
- `DxgkDdiBuildPagingBuffer` is the KMD side of VidMm paging operations. It must generate GPU instructions for
  transfer/fill/discard/read/write cases and handle insufficient DMA buffer space and busy allocations correctly.
- Windows 11 24H2 targets WDDM 3.2 with WDK 10.0.26100.1. Important WDDM 3.2 topics include dirty-bit tracking,
  live migration support, GPU native fences, feature support/enabled queries, and enhanced TDR diagnostics.
  User-mode work submission and allocation notification are documented as in-progress/not-final, so they are not
  the first dependency.

Local WDK confirmation: the Windows VM has WDK headers under
`C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0`. The render miniport initialization table is
`km\dispmprt.h` `DRIVER_INITIALIZATION_DATA`; WDDM 3.2 is present as `DXGKDDI_WDDMv3_2 = 0x3200` in
`shared\d3dkmddi.h`. The virtio Display-Only reference uses `KMDDOD_INITIALIZATION_DATA`, which is explicitly
not the target initialization path.

References are listed in section 13.

## 4. Current Repo Reality

Current Helios is not WDDM render-only:

- `ARCH.md` is still canonical for the active implementation: System-class KMDF, IOCTL transport, Mesa Venus ICD.
- `DISPLAY.md` and related DOD docs are archived Display-Only/scanout work, not render adapter design.
- The current Venus path already proves a lot of reusable transport: context creation, Venus submit, blob
  allocation, blob mapping, host-visible BAR handling, and Mesa Venus backend work.
- The current path does not give Windows a WDDM render adapter, does not satisfy DXGI/D3D runtime contracts, and
  cannot make D3D11/D3D12 apps see Helios as a native GPU without a real D3D UMD.

Launcher-only DXVK/VKD3D DLL injection is therefore not the target architecture. The downloaded `dxvk/` and
`vkd3d-proton/` DLL folders can remain useful for experiments, but the WDDM feature requires driver integration.

## 5. Reference Inventory

### virtio-research-only-3d

After correcting the checkout, the local reference now includes `virtio-research-only-3d/viogpu/viogpu3d` on
branch `viogpu3d` at commit `9ed3aab1` (`[viogpu3d] Introduce viogpu 3d driver`). This is a real WDDM render
miniport reference using `DxgkInitialize` and `DRIVER_INITIALIZATION_DATA`, so it is more relevant than the
Display-Only path. It is still not a blueprint to copy wholesale.

Useful render-driver references:

- `viogpu/viogpu3d/driver.cpp`: render miniport initialization, `DRIVER_INITIALIZATION_DATA` population,
  `DxgkInitialize`, adapter/device/context/allocation/render/present/submission callbacks.
- `viogpu/viogpu3d/viogpu_adapter.cpp`: dxgkrnl callback use, PCI config reads, virtio device init, capset
  query escape handling, interrupt notification.
- `viogpu/viogpu3d/viogpu_allocation.cpp`: WDDM allocation private data, `RESOURCE_CREATE_3D`, aperture
  map/unmap attaching guest pages to virtio resources.
- `viogpu/viogpu3d/viogpu_device.cpp`: device/context creation, context attach/detach, D3D present/render
  command copy into DMA buffers.
- `viogpu/viogpu3d/viogpu_command.cpp`: command worker, `SUBMIT_3D`, transfer commands, DMA completion interrupt
  notification.
- `viogpu/viogpu3d/BUILDING.md` and `viogpu3d.inx`: show the old user-mode pairing: Mesa Gallium virgl
  D3D10/WGL UMD DLLs (`viogpu_d3d10.dll`, `viogpu_wgl.dll`).
- `viogpu/common/viogpu.h` and `viogpu/common/viogpu_queue.cpp`: virtio-gpu 3D command definitions and queue
  command emission for capsets, `RESOURCE_CREATE_3D`, `SUBMIT_3D`, transfers, and scanout.
- `viogpu/viogpudo`: still useful for display-only/VidPN comparison only.

Do not inherit these reference shortcuts:

- It targets `DXGKDDI_INTERFACE_VERSION_WDDM1_3`, not WDDM 3.2.
- It is virgl/Gallium D3D10-oriented, not Venus/DXVK/VKD3D/D3D11/D3D12.
- `DxgkDdiCreateContext` aliases context to the device and states there is no separation.
- `DxgkDdiBuildPagingBuffer` handles only aperture map/unmap and returns success for missing allocation cases.
- `DxgkDdiPatch` is effectively a no-op.
- Preemption, timeout recovery, cancel, query fence, reset engine, query engine status, and debug collection log
  unsupported-preemption messages while returning success.
- `DescribeAllocation` contains explicitly random multisample/refresh values.
- The INF disables preemption through `GraphicsDrivers\Scheduler\EnablePreemption=0` and sets TDR debug mode.

Conclusion: `viogpu3d` is the best local code reference for the shape of a virtio-gpu WDDM render miniport and
for old virgl resource/submission plumbing. It must be mined selectively, with every shortcut replaced by a real
WDDM 3.2 implementation.

### dxvk-research-only

Useful references:

- `src/d3d11`: D3D11 state/device/context/resource translation to DXVK internals.
- `src/dxvk`: Vulkan backend, command lists, memory/resource abstractions, presenter.
- `src/vulkan`: Vulkan loader and dispatch plumbing.
- `src/dxgi`: research-only for behavior, not to ship as `dxgi.dll`.
- `src/wsi/win32/wsi_window_win32.cpp`: uses Wine-specific `D3DKMT_ESCAPE_SET_PRESENT_RECT_WINE`. This must not
  become a Helios ABI.

Critical mismatch: DXVK is built as app-level D3D/DXGI replacement DLLs. A Windows WDDM UMD is a DDI provider
loaded by the Direct3D runtime. Reuse means extracting/adapting translation internals behind the UMD DDI, not
dropping DXVK DLLs beside a game.

### vkd3d-proton

The local `vkd3d-proton/` directory currently contains prebuilt DLLs and setup script only:

- `x64/d3d12.dll`
- `x64/d3d12core.dll`
- `x86/d3d12.dll`
- `x86/d3d12core.dll`

That is enough for launcher experiments but not enough for UMD implementation research. For the WDDM UMD work,
we need source-level VKD3D/VKD3D-Proton internals or a vendored source reference.

## 6. Render-Only KMD Scope

The KMD must become a WDDM render adapter that provides the contracts the Direct3D runtime, DXGI, VidMm, and
VidSch expect:

- Adapter start/stop/remove/reset/power lifecycle through the WDDM display miniport path.
- Honest `DxgkDdiQueryAdapterInfo` responses for WDDM version, segments, caps, engines, scheduling, memory,
  cross-adapter/shared resource behavior, and feature support.
- Device/context lifecycle: create/destroy device and context with per-process/per-engine tracking.
- Allocation lifecycle: create/open/close/destroy allocation, private driver data, CPU visibility rules,
  residency, eviction, and shared-resource metadata.
- VidMm integration: segment model, aperture/host-visible BAR mapping, allocation placement, paging buffers,
  transfer/fill/discard/read/write, and synchronization.
- Submission path: render/present command buffer handling, patching, DMA buffer submission, queueing to virtio,
  completion, fence update, interrupt/DPC, and TDR integration.
- Preemption model and progress reporting.
- Native fence / monitored fence decisions compatible with WDDM 3.2.
- LUID and adapter identity stable enough for DXGI/D3D/Vulkan interop.

This should not reuse the current IOCTL protocol as the render ABI. The Venus protocol can remain below the
KMD/UMD boundary, but Windows-facing D3D work must flow through WDDM DDIs.

## 7. UMD Scope

There are two UMD problems:

1. D3D11 UMD: expose the D3D11 DDI expected by the Direct3D runtime and map it into DXVK-derived D3D11
   translation internals.
2. D3D12 UMD: expose the D3D12 UMD/DDI expected by the Direct3D runtime and map it into VKD3D/VKD3D-Proton
   translation internals.

The UMD cannot be a normal DXVK/VKD3D drop-in DLL. It must speak Microsoft UMD DDI upward and Vulkan/Venus
downward.

Open design choice:

- One layered UMD stack where D3D11 and D3D12 frontends share a Helios Vulkan/Venus backend.
- Separate D3D11 and D3D12 UMD DLLs sharing common internal libraries.

The second option is lower risk. It lets us bring up D3D11 and D3D12 independently while keeping the shared
Vulkan/Venus backend small and explicit.

## 8. DXGI Policy

DXGI stays Windows DXGI.

DXVK's `dxgi.dll` behavior is useful for understanding game expectations and quirks, but Helios must not ship a
replacement DXGI layer as the driver solution. Anything DXVK currently does through its DXGI replacement that is
actually required for adapter enumeration, memory budgets, shared handles, present behavior, HDR/color-space,
frame latency, or fullscreen transitions must be represented through the WDDM KMD/UMD/DXGI contracts.

Immediate research areas:

- How a render-only adapter should expose outputs: likely no physical outputs, but DXGI adapter enumeration and
  resource/present compatibility must still be correct.
- How presentation is routed when the render adapter is separate from the active display/IDD path.
- Whether the existing Looking Glass IDD remains the display target while Helios becomes the render adapter.
- Required cross-adapter/shared-resource path between render-only Helios and the display path.
- Whether D3D fullscreen exclusive can be supported without replacing DXGI.

This is load-bearing. If present cannot be made correct through Windows DXGI/WDDM contracts, the stack will hit
the same class of walls as earlier shortcuts.

## 9. Venus Integration Model

The current Helios Venus path has valuable pieces, but WDDM changes ownership:

- Current: Vulkan ICD -> IOCTL -> System-class KMD -> virtio Venus.
- Target: D3D UMD -> Vulkan/Venus backend -> WDDM KMD allocations/submissions -> virtio Venus.

The KMD should treat WDDM allocations as the authoritative lifetime/residency objects and map them to virtio-gpu
resources/blobs. Command buffers submitted through WDDM should package Venus protocol work and resource
references in a form VidSch can schedule and the KMD can complete with fences.

The existing `SUBMIT_VENUS`, `ALLOC_BLOB`, `MAP_BLOB`, and fence concepts are implementation references, not the
public ABI. They can inform internal structs, tests, and virtio operations.

## 10. Implementation Gates

No gate is considered complete if it relies on a fake success path.

### Gate 0: Research Freeze

- Finish this document and add source notes.
- Add a WDDM DDI checklist document before code.
- Inventory required WDDM/DDI headers and build support.
- Decide D3D11-first or KMD-first ordering.
- Obtain source-level VKD3D/VKD3D-Proton reference, not only DLLs.

Exit criteria: explicit checklist for KMD DDIs, UMD DDIs, DXGI/present path, memory manager, scheduler/fence,
and validation.

### Gate 1: Loadable WDDM Render Adapter

- INF/class/service converted to display miniport/render adapter.
- `DxgkInitialize` render-driver initialization path.
- Start/stop/remove/power lifecycle.
- Adapter identity, LUID, WDDM version targeting, and minimal honest caps.
- Minimal segment reporting needed by VidMm.
- Minimal allocation bookkeeping needed for adapter-load smoke testing.
- Null scheduler/paging path that prevents early Dxgkrnl timeouts without advertising render/GPU-VA capability.

Current Gate 1 implementation state, 2026-06-17:

- New separate crate `kmd_render` builds and packages as `helios_kmd_render`.
- The INF has a separate service/binary/catalog name. The current probe uses the Display setup class with
  `msdv.inf` and registers the bring-up `helios_umd.dll` through `UserModeDriverName` /
  `InstalledDisplayDrivers`, matching the viogpu3d-style third-party WDDM package shape.
- Package validation passes: `inf2cat` reports no errors/warnings and `infverif /v /w` reports `INF is VALID`.
- `DxgkDdiSubmitCommand` completes fences immediately through Dxgkrnl callbacks; this is a bring-up null engine.
- `DxgkDdiBuildPagingBuffer` is a no-DMA null paging engine for load testing only.
- `DxgkDdiCreateAllocation` creates bookkeeping handles and conservative segment metadata; no Venus resource exists yet.
- `DxgkDdiStartDevice` attempts virtio-gpu transport initialization and records failures, but Gate 1 does not fail
  PnP start when the transport is unavailable. The adapter must be boot/restart safe before render is advertised.
- The current render package reaches `DxgkDdiStartDevice`; ntoseye confirmed VidMm initialization succeeds. The
  active Code 43 gate is later in `dxgkrnl!ADAPTER_RENDER::Initialize`, where `DXGKQAITYPE_HISTORYBUFFERPRECISION`
  returned `PrecisionBits = 10`. Dxgkrnl's render-adapter validation accepts 32..64 bits and returned
  `STATUS_OBJECT_NAME_NOT_FOUND` (`0xC0000182`) from `ADAPTER_RENDER::Initialize+0x1633`.
- `22.22.32.49` reaches Code 0 as `oem129.inf`. The two concrete gates fixed after ntoseye inspection were:
  `DXGKQAITYPE_HISTORYBUFFERPRECISION` must report 32..64 bits, and `DxgkDdiCreateContext` must return a valid
  context handle plus context DMA/list sizing for VidSch system-device creation. UMD `OpenAdapter10`,
  `OpenAdapter10_2`, and `OpenAdapter12` now expose minimal adapter-open tables, but real D3D `CreateDevice` remains
  intentionally unsupported.
- `22.22.32.51` is installed as `oem131.inf` and remains Code 0. It fixes the D3D10 vs D3D10.2 adapter function-table
  ABI, adds a non-empty D3D10/D3D11 supported-version list, and logs the runtime path in
  `C:\Windows\Temp\helios_umd.log`. `dxdiag` reports the adapter as a render-only WDDM 3.2 device with no device
  problem and reaches the expected next gate: `CreateDevice -> E_NOTIMPL`.
- `helios_umd.dll` now builds as a package member and exports `OpenAdapter10`, `OpenAdapter10_2`, and `OpenAdapter12`
  as explicit unsupported bring-up entrypoints. It is copied into the package and registered for the current load-gate
  probe; the entrypoints still fail honestly until the UMD/KMD contract is real.
- Mesa's Windows Venus ICD work remains valuable for the eventual rendering backend, but it cannot be dropped in as
  the WDDM UMD. The WDDM UMD needs D3D10/11/12 DDI entrypoints and runtime/KMT callback handling; Mesa's Gallium
  `d3d10umd` frontend is the better source reference for those tables.
- The KMD DDI table now also registers Display/VidPn/boot-display callbacks that return unsupported, plus the
  scheduler/platform callbacks that WDDM 3.2 exposes around engine status, reset, cancel, clock calibration, history
  buffer formatting, runtime power, stable power state, and virtual-machine metadata. These callbacks are conservative
  bring-up implementations around a single node/engine and do not advertise real render capability.
- `DxgkDdiAddDevice` initializes the output context pointer to NULL before allocation, matching `viogpu3d`. The Rust
  WDK allocator already allocates from nonpaged pool, so context pool type is not the current differentiator.
  `oem117.inf` tested the NULL-initialization cleanup and still failed with the unchanged AddDevice/RemoveDevice-only
  breadcrumb sequence.
- Microsoft display docs research, 2026-06-17:
  - `gpu-paravirtualization.md` documents the Virtual Render Device (VRD): Direct3D runtimes remain unchanged, the
    render-only adapter can be paired with a display-only adapter, and if unpaired it is used when an app explicitly
    selects it. This confirms that our target is a WDDM render miniport/UMD path, not a DXGI replacement.
  - `wddm-driver-and-feature-caps.md` states the render-only classification rule: implement all render-specific DDIs
    and leave display-specific DDIs NULL, or implement the full WDDM DDI surface while reporting
    `DISPLAY_ADAPTER_INFO.NumVidPnSources = 0` and `NumVidPnTargets = 0`.
  - The same caps doc says render-only WDDM 1.2+ mandatory caps include `DXGK_DRIVERCAPS.WDDMVersion`,
    `PreemptionCaps`, `DXGK_FLIPCAPS.FlipOnVSyncMmIo`, `DXGK_DRIVERCAPS.SupportPerEngineTDR`, and
    `DXGK_PRESENTATIONCAPS.SupportKernelModeCommandBuffer`. Our current caps response is intentionally sparse and is
    not a valid long-term WDDM 3.2 render-only caps surface.
  - `wddm-v1-2-driver-enforcement.md` says dxgkrnl validates mandatory WDDM features and fails adapter creation when
    a driver claims a WDDM level without implementing/reporting the required features. This matches the class of
    failure we are seeing, although current breadcrumbs show `QueryAdapterInfo` has not yet been called before
    `RemoveDevice`.
  - `adding-software-registry-settings.md`, `installed-display-drivers-directive.md`, and
    `graphics-inf-requirements.md` all describe UMD registration as normal WDDM display-driver INF state. The
    BasicRender System-class INF is a Microsoft/system exception, not enough evidence that our third-party render
    package should omit UMD registration forever.
  - The local `viogpu3d` reference targets WDDM 1.3 and sets driver caps coherently:
    `WDDMVersion = DXGKDDI_WDDMv1_3`, `FlipOnVSyncMmIo = TRUE`, `SectionBackedPrimary = TRUE`,
    `SupportDirectFlip = TRUE`, scheduler awareness/preemption awareness, and one asymmetric processing node. This is
    the right minimal probe baseline before trying to prove a WDDM 3.2 Rust table.
- Correction after testing: the in-box Microsoft Basic Render Driver installs as `Class=System`, not `Class=Display`,
  but that did not resolve the third-party Helios load gate. Display-class packages (`oem106.inf` through
  `oem114.inf`) and the BasicRender-style System-class probe (`oem115.inf`) both failed after `DxgkDdiAddDevice` and
  before `DxgkDdiStartDevice`, then called `RemoveDevice`. The current viogpu3d-style Display-class INF with UMD
  registration moves past that gate; current failures are in later render-adapter capability validation rather than
  the Microsoft BasicRender class exception.
- It is acceptable for controlled install/bind smoke testing with VM snapshot/recovery. It is not acceptable for
  DXVK/VKD3D/D3D workloads until the UMD/KMD render-device contract is implemented.
- No advertised 3D/D3D feature support yet unless runtime can create a real device.

Exit criteria: Code 0 device, dxgkrnl load/unload stability, dxdiag/DXGI visibility matching advertised caps,
no fake D3D device creation.

### Gate 2: Memory Manager and Allocations

- Segment model for host-visible BAR/system memory/device-local illusion.
- WDDM allocation create/open/close/destroy.
- Residency and eviction accounting.
- Paging buffers for required operations.
- CPU map/unmap semantics and cache policy.
- Shared resource metadata path.

Exit criteria: allocation stress tests, paging tests, no invalid VidMm assumptions, no leak on process exit.

### Gate 3: Scheduler, Submission, Fences, TDR

- Context/device lifecycle.
- Command buffer format and patching.
- DMA buffer submission to virtio-gpu/Venus.
- Interrupt/DPC completion.
- Monitored/native fence policy.
- TDR progress and recovery path.
- Preemption model.

Exit criteria: submit/completion tests, fence wait tests, forced timeout/TDR diagnostics, no `SubmitCommand`
error path in normal operation.

### Gate 4: Minimal UMD Handshake

- D3D11 UMD loader handshake (`OpenAdapter10_2` path and required DDI tables).
- Minimal `CreateDevice` implementation that fills a valid D3D11-era device function table or fails earlier with a
  precisely documented capability reason.
- Query adapter info bridge to KMD.
- Device creation only for honestly supported baseline.
- No translation yet beyond clear no-op validation if the runtime permits it.

Exit criteria: runtime loads the Helios UMD and fails or succeeds honestly; no app sees fake feature levels.

### Gate 5: D3D11 via DXVK Internals

- Adapt DXVK D3D11 frontend/internals behind the D3D11 UMD DDI.
- Replace DXVK DXGI replacement assumptions with Windows DXGI/KMD-backed behavior.
- Route Vulkan calls into Helios Venus/WDDM backend.
- Implement resource sharing/keyed mutex/fence paths required by real games.

Exit criteria: D3D11 device creation, resource creation, shader path, draw/clear/readback, simple swapchain,
then real game smoke tests.

### Gate 6: D3D12 via VKD3D/VKD3D-Proton Internals

- Source-level integration plan for VKD3D/VKD3D-Proton.
- D3D12 UMD/DDI loader and device/queue/resource/fence contracts.
- Vulkan/Venus backend shared with D3D11 where possible.

Exit criteria: D3D12 device creation, command queue/list execution, resource barriers, simple render/readback,
then real game smoke tests.

### Gate 7: DXGI/Present Correctness

- Adapter enumeration and memory budget correctness.
- Present path through Windows DXGI, not replacement DXGI.
- Render-only to display/IDD/Looking Glass interaction.
- Fullscreen/windowed transitions.
- Frame latency, HDR/color-space policy if advertised.

Exit criteria: Steam launch path works without DLL launchers, DXGI debug layer does not report structural
driver misuse, Looking Glass remains stable.

### Gate 8: WDDM 3.2 Completeness

- WDDM 3.2 feature query/reporting path.
- Native fence support if selected.
- Dirty-bit/live-migration features only if truly implemented.
- Enhanced TDR data.
- HLK-oriented validation.

Exit criteria: feature flags match implementation; no "target 3.2" branding unsupported by behavior.

## 11. First Development Tasks After Docs

Do these only after Gate 0 docs/checklists are complete:

1. Expand each `DRIVER_INITIALIZATION_DATA` callback in `WDDM_RENDER_ONLY_DDI_CHECKLIST.md` into
   required/optional/unsupported status for a render-only adapter.
2. Add DXGI render-only/present research notes.
3. Obtain source-level VKD3D/VKD3D-Proton inventory; the current local folder only has DLLs.
4. Create a WDDM render branch or feature directory so the current working System-class path remains recoverable.
5. Prototype build plumbing for `DxgkInitialize` without changing installed driver behavior.
6. Add compile-time WDK/DDI version checks for WDDM 3.2 / WDK 10.0.26100.1.
7. Write negative tests for "unsupported means unsupported", so fake success stubs fail review.

## 12. Risks

- UMD effort is the dominant risk. DXVK/VKD3D are not WDDM UMDs; adapting them is closer to building a driver
  frontend than packaging a launcher.
- DXGI/present is the second major risk. The no-DXGI-replacement rule is correct, but it forces us to satisfy
  real Windows presentation/shared-resource contracts.
- WDDM memory manager integration cannot be postponed. If Venus blobs are not represented as WDDM allocations,
  residency, sharing, paging, and TDR behavior will break.
- The virtio reference tree is DOD-oriented, not render-oriented.
- `vkd3d-proton/` currently lacks source. DLLs are not enough for UMD integration.

## 13. Sources

- Microsoft: Windows Vista and later display driver model architecture:
  https://learn.microsoft.com/en-us/windows-hardware/drivers/display/windows-vista-and-later-display-driver-model-architecture
- Microsoft: User-mode display drivers:
  https://learn.microsoft.com/en-us/windows-hardware/drivers/display/user-mode-display-drivers
- Microsoft: Initializing communication with the Direct3D version 11 DDI:
  https://learn.microsoft.com/en-us/windows-hardware/drivers/display/initializing-communication-with-the-direct3d-version-11-ddi
- Microsoft: `DxgkDdiCreateContext`:
  https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/nc-d3dkmddi-dxgkddi_createcontext
- Microsoft: `DxgkDdiSubmitCommand`:
  https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/nc-d3dkmddi-dxgkddi_submitcommand
- Microsoft: `DxgkDdiBuildPagingBuffer`:
  https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/nc-d3dkmddi-dxgkddi_buildpagingbuffer
- Microsoft: Windows 11 display and graphics driver updates:
  https://learn.microsoft.com/en-us/windows-hardware/drivers/display/what-s-new-for-windows-11-display-and-graphics-drivers
- Microsoft: Driver changes for Windows 11 version 24H2:
  https://learn.microsoft.com/en-us/windows-hardware/drivers/driver-changes-for-windows-11-version-24h2
- Local reference: `virtio-research-only-3d/viogpu`.
- Local reference: `dxvk-research-only/src`.
- Local binaries only: `vkd3d-proton/`.
