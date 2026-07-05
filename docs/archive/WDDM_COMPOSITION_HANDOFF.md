# WDDM_COMPOSITION_HANDOFF.md — make DWM composite the desktop on Helios (path A)

## UPDATE 2026-06-23 — current live state after D3D11/IDD debugging

This section supersedes the 2026-06-22 "Where we are now" / "BLOCKER" bullets where they
conflict.

### Current objective

Still locked: make the Windows desktop compose and render on the Helios WDDM render adapter,
then have the Looking Glass IDD receive the IddCx swapchain frames and display them in the
Looking Glass client. Do not pivot to per-app venus or WARP composition as the final design.

### Verified working

- Helios KMD is Code 0 after PnP enable, with gpu-gl attached.
- The D3D11 UMD/device-create blocker is resolved: `D3D11CreateDevice` against
  "Helios vGPU Render Adapter (WDDM bring-up)" returns `S_OK`, feature level `0xa000`.
- DWM no longer fails at the old `dwmcore!CD3DDevice::CreateD3D11Device` / `0x889800b0`
  stage in the simple probe path. The old "DWM cannot create a D3D11 device" handoff is
  stale.
- The Looking Glass IDD can select Helios with
  `HKLM\SOFTWARE\LookingGlass\IDD\HeliosRenderAdapter=1`; IddCx monitor arrival succeeds
  and returns `OsAdapterLuid` + `OsTargetId`.
- Current deployed test KMD hash after this session's fixes:
  `B0B8A079394E86609C0FCD72785981A01FB70F366F459F3C4E394B68DCC3A315`.

### Current blocker

The blocker is now display topology / swapchain assignment, not D3D11 device creation:

- `GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS)`, `QDC_ALL_PATHS`, and
  `QDC_DATABASE_CURRENT` all return success with `paths=0 modes=0`.
- The LG monitor exists and is active in WMI:
  `DISPLAY\LGD1DDD\...\UID256`, and `DisplayConfigGetDeviceInfo(GET_TARGET_NAME)` succeeds.
- The helper sees source 0 for the IDD adapter, e.g. `\\.\DISPLAY23`, but every synthetic
  `SetDisplayConfig(SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_APPLY | ...)` attempt returns
  `31` (`ERROR_GEN_FAILURE`). Legacy `ChangeDisplaySettingsEx` also fails.
- `EnumDisplayDevices` reports both `Microsoft Basic Display Driver` and `INDIRECTKMD` with
  state flags `0x00000000`, and `EnumDisplaySettings` on the LG display returns
  `ERROR_BUSY` (`170`).
- During one controlled test, disabling Helios and re-enabling the IDD in the same current
  boot still produced the same empty CCD database and `SetDisplayConfig` ret 31. That
  same-boot result is not representative of a clean Helios-absent boot.
- Clean gpu-gl-out boot baseline verified 2026-06-23: the Helios PCI node is a phantom
  non-started device (`Get-PnpDevice Status=Unknown`, `Problem=CM_PROB_PHANTOM`; earlier probe
  surfaced this as disconnected), the Looking Glass IDD is `OK`, WMI reports the LG monitor
  active, `Win32_VideoController` reports the IDD at `1920x1080`, and a session-1 CCD probe
  reports `QDC_ONLY_ACTIVE_PATHS paths=1 modes=2`, `QDC_ALL_PATHS paths=2 modes=4`, and
  `QDC_DATABASE_CURRENT paths=1 modes=2`. The Looking Glass client displays the desktop in
  this state.
- In that clean gpu-gl-out boot, `HKLM\SOFTWARE\LookingGlass\IDD\HeliosRenderAdapter=1`
  remains set, but the IDD logs `Preferred IDD render adapter not found; IddCx render adapter
  remains OS-selected`. `AssignSwapChain` fires with render-adapter LUID
  `00000000:000076b0`; `CD3D11Device::Init` succeeds at feature level `0xb100`, then
  `CD3D12Device::Init` creates the IVSHMEM heap and copy/compute queues. One early
  `IddCxSwapChainSetDevice` attempt returned `0x887a0026` (`keyed mutex abandoned`), then
  assignment repeated and resource creation proceeded.

### Code changes from this session

- `kmd_render/src/ddi/query_adapter_info.rs`: stopped advertising
  `DXGK_PRESENTATIONCAPS::DriverSupportsCddDwmInterop` (bit 8). Microsoft's contract says
  this advertises CDD present support into DWM texture allocations, which is a display
  miniport path; Helios is a zero-source render adapter. Removing it did not fix CCD ret 31,
  but it is the correct capability surface for now.
- `kmd_render/src/virtio/gpu.rs` + `kmd_render/src/ddi/create_allocation.rs`: fixed a
  KMD-internal Venus resource ownership hazard. `VenusClient::allocate_memory_blob()` records
  a temporary owner-0 blob-table entry; standard WDDM allocations then adopt that same
  `res_id`. The code now removes the temporary owner-0 tracking entry without sending host
  commands, so `DestroyAllocation` is the sole detach/unref owner. This targets the qemu
  `VIRTIO_GPU_CMD_RESOURCE_UNREF` (`ctrl 0x102`) `RESOURCE_NOT_FOUND` noise.
- No current source diff remains under `LookingGlass/idd`; IDD code is restored to the
  repository/submodule baseline.

### Negative tests / do not repeat blindly

- Disabling `DECLARE_CROSS_ADAPTER_RESOURCE` made the stack worse: the D3D11 probe stopped
  completing cleanly and the UMD's Venus `CTX_CREATE` escape failed with
  `STATUS_IO_DEVICE_ERROR` (`0xc0000185`). Cross-adapter support was restored.
- Changing IDD monitor signal `vSyncFreqDivider` for monitor modes to `1` made
  `IddCxMonitorArrival` fail with `STATUS_INVALID_PARAMETER`. The original IDD behavior
  (`monitorMode ? 0 : 1`) is restored.
- `IddCxAdapterDisplayConfigUpdate` returns `0xc00000bb` on this local IDD path; Microsoft
  docs identify it as a remote-driver API path. Treat that failure as expected unless a
  trace proves otherwise.

### Useful next investigation tools

1. IddCx WPP capture, if the display-topology path is under investigation:
   `logman create trace IddCx -o C:\Windows\Temp\IddCx-helios.etl -ets -ow -mode sequential -p {D92BCB52-FA78-406F-A9A5-2037509FADEA} 0x4f4 0xFF`.
   `tracerpt` captures events but does not decode WPP format strings by itself. Use
   `tracefmt.exe` / `tracepdb.exe` with public `IddCx.pdb` symbols or kernel
   `!wmitrace.logdump IddCx`.
2. Session-1/WMI display probes are preferred over SSH/session 0 for monitor and CCD state.
   Log full `SetDisplayConfig` flags, target names, and source names if using Win32 CCD APIs.
3. If/when IddCx calls `AssignSwapChain`, resume Helios render/composition checks. At that
   point D3D12 copy support or a D3D11/Vulkan IDD copy path may matter.

**Date:** 2026-06-22. **Read first:** this doc, then `IDD_HELIOS_RENDER_PLAN.md` (§4 is the
locked design), the `wddm-hwaccel-desktop-is-the-goal` + `step2-gpummu-implemented` memories,
`BRINGUP_QUIRKS.md`, `NTOSEYE.md`.

## The one-line goal (unchanged, locked)
DWM composites the **whole desktop ON Helios** (venus → host GPU); the Looking Glass IDD
captures the composed frame via the standard IddCx swapchain → Looking Glass shows a
hardware-accelerated desktop. NOT per-app venus (the old System-class driver already did that).

## Where we are now (2026-06-22, verified live)
- **Helios is a stable WDDM render adapter at device Code 0.** The GpuMmu bring-up is DONE
  (codex's `DmaBufferSegmentSet=1` fix + the aperture-first segment shape). It survives DWM
  hammering it (2800+ DDI calls) with no wedge.
- **The guest-wedge that blocked this session was a diag bug, now fixed:** `diag.rs` indexed a
  3-byte `digits` buffer with a 4-digit breadcrumb index once the counter passed 1000
  (`MAX_STEPS=3000`). The no_std `#[panic_handler]` is `loop{}` (`eb fe`), so the panic spun a
  thread forever **while it held dxgkrnl's adapter `ERESOURCE`** → the whole graphics stack
  deadlocked (6 waiters) → watchdog live-dump. Fixed (buffers sized for full u32). Lesson:
  ANY panic in a Helios DDI = silent graphics deadlock; keep DDI paths panic-free.
- **`GetStandardAllocationDriverData` + `DescribeAllocation` are implemented** (this session,
  `kmd_render/src/ddi/create_allocation.rs`): DWM/IddCx can now get driver data for the
  shared-primary / shadow / staging / GDI standard surfaces, and `CreateAllocation` self-backs
  them with a host-allocated HOST3D mappable blob (`blob_id=0`, the KMD venus ctx). This let
  DWM get **past** the old `DXGI_ERROR_UNSUPPORTED`-at-CreateDevice abandon.
- **The IDD can now select Helios** (registry-gated, `LookingGlass/idd/LGIdd`):
  `HKLM\SOFTWARE\LookingGlass\IDD\HeliosRenderAdapter=1` → `IddCxAdapterSetRenderAdapter(Helios)`
  instead of the WARP force-select. Default 0 (WARP) for safety; flip + restart the IDD
  (`devcon restart Root\LGIdd`) to point it at Helios.
- **Flipping the IDD to Helios STOPPED the DWM crash-loop.** On WARP-while-Helios-present DWM
  crashed every 1-2 min (`Application Error`, exit `0x889800b0`, "Primary display device ID:
  INDIRECTKMD"). Selecting Helios → DWM stable.

## THE BLOCKER (this is path A)
**There are ZERO display paths while Helios is present** (`QueryDisplayConfig`: ALL paths=0,
ACTIVE paths=0, modes=0). The LG monitor (`DISPLAY\LGD1DDD`) arrives and is status-OK, but the
OS never puts it in a display path, so IddCx never assigns it a swapchain
(`CIndirectMonitorContext::AssignSwapChain` is **never called** — confirmed with instrumentation
this session). Consequences:
- The IDD's D3D11/D3D12 device creation is **never reached** → **D3D12 is NOT the gate.** Do
  NOT build a D3D12 UMD to fix frames; we proved the swapchain path is never entered. (If a
  D3D12 UMD is ever wanted it's for app D3D12, not this.)
- Engine atomics show **`SubmitCommand=0, Render=0`** even though DWM creates a device on
  Helios — DWM probes Helios but **renders nothing** and the OS builds no path. This is the
  §2/§4 "Helios present → display dies" problem: Helios is not yet *compositable enough* for
  the OS to build a display path on it.

This matches `IDD_HELIOS_RENDER_PLAN.md` §2 exactly (with Helios ABSENT the WARP→IDD→LG path
works and LG shows the desktop; with Helios loaded it collapses).

## What path A has to figure out / build
The question is **why the OS won't build a display path with DWM compositing the IDD's monitor
on Helios**, and then make it. Likely sub-problems (in rough order):

1. **Diagnose the 0-paths / no-composition first (don't guess).** DWM creates a D3D11 device on
   Helios (the DXVK-based `helios_umd.dll` works) but submits no work. Find out *why DWM/the OS
   abandons Helios as a composition/display-path target*:
   - Is DWM's `D3D11CreateDevice(Helios)` succeeding now, and what does DWM do next before it
     stops? (ETW: `Microsoft-Windows-DxgKrnl`, `Microsoft-Windows-Dwm-Core`; or attach ntoseye
     to dwm.exe and trace the composition setup / where it bails.)
   - Does the OS need Helios to expose a VidPN **source** to pair with the IDD's monitor
     **target**? Helios currently reports `NumberOfVideoPresentSources=0` (render-only,
     `start_device.rs`). A cross-adapter "render on Helios, scan out on the IDD" topology may
     require Helios to participate in the VidPN/topology (the IddCx cross-adapter model), or the
     OS may expect the *IDD's* source paired with Helios as the render LUID — confirm which.
   - Decode the DWM crash code `0x889800b0` (facility 0x098) from the WARP runs — it's the
     symptom of DWM failing composition; it may name the missing capability.

2. **The §4 coherence work (the real engine), once #1 says it's needed:**
   - **Real submission + venus-driven fence (the #1 coherence task).** `DxgkDdiSubmitCommand`
     is still a null engine (immediate fence). For DWM to composite correctly, the WDDM fence
     must reflect *real venus completion*. NOTE: actual Helios rendering today goes through the
     **ICD → D3DKMTEscape (venus)** out-of-band path, NOT `DxgkDdiRender`; so first determine
     whether DWM's composition submits via the UMD's escape-venus path (works) or via
     `DxgkDdiRender`/`SubmitCommand` (stubbed) — that decides whether SubmitCommand/Render must
     be made real or whether the fence just needs to track the escape submissions.
   - **CPU/IDD-readable composed primary (§4 #2).** The composed primary the IDD reads must be a
     host-visible venus blob (`MAP_BLOB` BAR) with correct cache coherency. The standard
     allocations from `GetStandardAllocationDriverData` are currently `blob_id=0` HOST3D blobs;
     verify they end up host-visible + mapped where DWM/the IDD read them, and that
     `BuildPagingBuffer`/`RESOURCE_MAP_BLOB` maps them into the window (Stage 2b machinery).
   - **No paging / always resident (§4 #3).** Size segments so VidMm never evicts.

3. **Then the IDD capture side.** Only once a path activates and IddCx calls `AssignSwapChain`
   does the IDD's D3D device + copy path run. THEN the D3D11-vs-D3D12 question matters (the IDD
   uses D3D12 for the copy queue; Helios is D3D11-only — switch the IDD copy to D3D11, or use
   the CHeliosSink Vulkan-import path, `IDD_HELIOS_RENDER_PLAN.md` §3). The
   `AssignSwapChain`/CD3D11Device/CD3D12Device instrumentation added this session will light up
   the moment a swapchain is assigned.

## Reusable assets / reference
- **`virtio-research-only-3d/viogpu/viogpu3d/`** — the working WDDM-over-virtio-gpu driver;
  structural template for the VidMm DDIs (segments, allocations, BuildPagingBuffer,
  SubmitCommand/Patch/fence+interrupt). Mind the differences: viogpu3d uses TRANSFER queues +
  is a full display driver; Helios is zero-copy BAR + render-only + venus.
- **`vkd3d-proton-helios`** submodule is checked out (for a future D3D12 path — NOT needed for
  frames here).
- The KMD venus client (`kmd_render/src/virtio/venus.rs`) self-allocates host-visible blobs
  over venus — reuse for composition surfaces if needed.
- Engine/page-table diag atomics are dumped at `DxgkDdiDestroyDevice` (codes `0x0F01..0x0F0E`,
  legend in `BRINGUP_QUIRKS.md` §6): watch `0x0F06` (SubmitCommand) / `0x0F09` (Render) go
  nonzero as the signal DWM is actually rendering on Helios.

## Current deployed state at handover
- KMD: fixed build (diag fix + alloc DDIs), live `.sys` = 88576, Code 0, stable.
- IDD: instrumented build installed via `devcon` (oem-bound to `Root\LGIdd`), `preferHelios=1`
  set (DWM stable). LG shows no frames (0 paths) — expected until path A lands.
- Helios PCI id `PCI\VEN_1AF4&DEV_1050&…&0017`; gpu-gl device `ua-heliosgpu`.

## Mechanics reminders (hard-won this session)
- KMD, UMD, and ICD deployment rules are now captured in `HELIOS_DRIVER_DEPLOYMENT.md`.
  - Use `Z:\tools\install-helios-kmd.ps1 -PlanOnly`, then `Z:\tools\install-helios-kmd.ps1`.
    It signs through the machine-cert fallback, backs up active DriverStore files, preserves UMD
    by default, verifies hashes, and requires Code 0.
  - Use `Z:\tools\hotplug-helios-umd.ps1 -PlanOnly`, then `Z:\tools\hotplug-helios-umd.ps1`.
    Default UMD mode is the verified ProgramData override plus Helios PnP rebind; `-Mode
    DriverStore` and `-Mode PackageUpgrade` are explicit fallback modes.
  - Use `Z:\tools\install-helios-icd.ps1 -PlanOnly -NoSmoke`, then
    `Z:\tools\install-helios-icd.ps1` after Mesa ICD rebuilds. It installs a content-hashed
    ProgramData ICD DLL and atomically rewrites the Khronos manifest.
  - Do not manually copy KMD/UMD/ICD files during normal iteration. If a deploy edge case appears,
    fix the script so the next agent does not rediscover the same hotplug step.
- **Deploy the IDD with `devcon`, not in-place copy.** In-place writes to the IDD DriverStore
  dir are TrustedInstaller-blocked and silently produce a 0-byte DLL (which then fails to load /
  the device falls back to a stale oem copy). `devcon update <pkg>\LGIdd.inf Root\LGIdd` installs
  a fresh DriverStore copy and rebinds. Apply the same discipline to Helios KMD: use
  `Z:\tools\install-helios-kmd.ps1`, which publishes with `devcon update` when available and
  verifies Code 0. `inf2cat` is x86-only (`…\bin\10.0.26100.0\x86\`); `signtool` is x64.
- The IDD `win_looking_glass_idd` build emits an `InfVerif.dll`-missing error — **non-fatal**,
  the DLL + signed cat are still produced.
- A new KMD build does **not always need a reboot** if the active device can be PnP-stopped.
  Prefer the deployment script first. Recover a wedged guest by booting **gpu-gl-OUT** only when
  PnP stop/start or ntoseye/SSH access is gone.
