# WDDM_COMPOSITION_HANDOFF.md — make DWM composite the desktop on Helios (path A)

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
- **Deploy the IDD with `devcon`, not in-place copy.** In-place writes to the IDD DriverStore
  dir are TrustedInstaller-blocked and silently produce a 0-byte DLL (which then fails to load /
  the device falls back to a stale oem copy). `devcon update <pkg>\LGIdd.inf Root\LGIdd` installs
  a fresh DriverStore copy and rebinds. (The KMD `e0bd` dir DOES take in-place writes after
  `takeown`.) `inf2cat` is x86-only (`…\bin\10.0.26100.0\x86\`); `signtool` is x64.
- The IDD `win_looking_glass_idd` build emits an `InfVerif.dll`-missing error — **non-fatal**,
  the DLL + signed cat are still produced.
- A new KMD build needs a **reboot** to load (disable→enable re-runs bring-up on the
  already-loaded image). Recover a wedged guest by booting **gpu-gl-OUT** (Helios absent → clean
  SSH, `.sys` unlocked).
