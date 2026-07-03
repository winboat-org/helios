# Looking Glass IDD ← Helios render path — findings + plan (2026-06-18)

**Goal:** make the Looking Glass Indirect Display Driver (IDD) display content **rendered by the
Helios WDDM driver** (venus → host GPU) — the desktop (DWM + Explorer) and GPU‑accelerated apps —
instead of the Microsoft Basic Render Driver (WARP) software fallback it uses today.

This doc is the research record + plan so the next session can go straight to implementation.

> **★ UPDATE 2026-06-22 — see `WDDM_COMPOSITION_HANDOFF.md` (the current path-A brief).**
> Helios is now a **stable WDDM render adapter at Code 0** (GpuMmu done; a diag-buffer overflow
> panic that deadlocked the graphics stack is fixed). `GetStandardAllocationDriverData` +
> `DescribeAllocation` are implemented, and the IDD can now select Helios (registry-gated
> `HeliosRenderAdapter`) — which **stops the DWM crash-loop**. BUT frames still don't flow:
> `QueryDisplayConfig` shows **0 display paths** while Helios is present, IddCx never assigns the
> IDD a swapchain (`AssignSwapChain` never fires), and DWM creates a device on Helios yet renders
> nothing (`SubmitCommand=0`). **D3D12 is RULED OUT as the gate** — the swapchain path is never
> entered, so the §1 "IDD uses D3D12, Helios has none" note is not what's blocking frames. The
> real blocker is §4's core: make DWM actually composite on Helios so the OS builds a display
> path. Details + next steps in `WDDM_COMPOSITION_HANDOFF.md`.

> **★ UPDATE 2026-06-23 — D3D11 device creation is no longer the blocker.**
> The probe `C:\Users\Rupansh\helios-probe\d3d11_devicecreate_probe.exe` now returns
> `D3D11CreateDevice hr=0x00000000 featureLevel=0xa000` on Helios. The old
> `0x889800b0`/`DXGI_ERROR_UNSUPPORTED` DWM failure stage was fixed in the UMD/cap path.
> The current blocker is earlier in display activation from the user-visible point of view:
> IddCx monitor arrival succeeds, but CCD has zero active/all/database paths and the helper's
> `SetDisplayConfig` attempts return `ERROR_GEN_FAILURE` (`31`). This also reproduced in a
> same-boot Helios-disabled test, but a clean Windows boot without gpu-gl/Helios verifies that
> the Looking Glass IDD works: session-1 CCD reports active/all/database paths, AssignSwapChain
> fires, D3D11/D3D12 device init succeeds, and the Looking Glass client displays the desktop.
> Treat the same-boot Helios-disabled result as contaminated by live graphics-stack state.
> `LookingGlass/idd` has no current source diff; do not assume an IDD code experiment is still
> deployed from source.

---

## 1. How IddCx actually delivers frames (RESEARCHED — do not re‑investigate)

**IddCx does NOT use the render adapter's D3D11 UMD `pfnPresent` DDI.** Confirmed by reading the LG
IDD source. The IDD gets each composed desktop frame by calling
`IddCxSwapChainReleaseAndAcquireBuffer` (or `…Buffer2` for FP16) which hands it an
`IDXGIResource`/`ID3D11Texture2D` the **OS has already composited**:

- `LookingGlass/idd/LGIdd/CSwapChainProcessor.cpp:148-179` — the acquire loop; `buffer.MetaData.pSurface`
  is the composed texture.
- `…/CSwapChainProcessor.cpp:319-327` — casts it to `ID3D11Texture2D`, then D3D12‑copies it to KVMFR/ivshmem.
- The IDD binds its D3D device to the swapchain with `IddCxSwapChainSetDevice`
  (`CSwapChainProcessor.cpp:107-115`), using the **render‑adapter LUID the OS passes** in
  `IDARG_IN_SETSWAPCHAIN.RenderAdapterLuid` (`Device.cpp:189-196`, `CIndirectMonitorContext.cpp:37-47`).
- D3D devices are created with that LUID via `EnumAdapterByLuid` (`CD3D11Device.cpp:24-32`,
  `CD3D12Device.cpp:63-79`). The IDD uses **D3D12** for the copy queue → Helios has **no D3D12**, only D3D11.

**Consequence:** the render adapter's `pfnPresent` is irrelevant (my UMD benign‑present is harmless but
does nothing for visible frames). What matters is **which render adapter the OS composites the IDD's
monitor on**, and whether that adapter can actually be composited on.

## 2. Why nothing renders on Helios today (RESEARCHED)

The IDD **force‑selects WARP** to stay off the incomplete Helios adapter:
`LookingGlass/idd/LGIdd/CIndirectDeviceContext.cpp:182-213` enumerates DXGI adapters, finds
"Microsoft Basic Render Driver" (VendorId 0x1414 / DeviceId 0x008c) and calls
`IddCxAdapterSetRenderAdapter(PreferredRenderAdapter = WARP LUID)`. Live log confirms:
`InitAdapter:205 IDD render adapter[0]: Helios … skipped` / `[1]: Microsoft Basic Render Driver … selected`.

Helios genuinely **cannot be composited on** (DWM/apps call `D3D11CreateDevice` → `S_OK` then
`DestroyDevice` immediately, **zero render DDIs in between** — they probe + abandon at the DXGI/VidMm
adapter level). From `kmd_render/src/ddi/query_adapter_info.rs`:
- `query_segments` (QUERYSEGMENT4) exposes **one 64 MiB aperture segment** (`CpuVisible=1, Aperture=1`) —
  NOT a GPU‑visible memory segment. DWM can't allocate render targets in it.
- The code comment (≈ lines 249‑262) records that a real **memory** segment (`Aperture=0`) pointing at the
  host‑visible BAR was already tried (.66–.70) and **rejected by VidMm** right after `DxgkDdiCreateDevice`
  (clean‑boot Code 43) because it needs a declared GPU memory model (**GpuMmu/IoMmu**) Helios doesn't provide.
- `DxgkDdiGetStandardAllocationDriverData` → `STATUS_NOT_IMPLEMENTED`
  (`kmd_render/src/ddi/create_allocation.rs:242-248`) — DWM uses this for the standard primary/staging/shared
  composition surfaces.
- `DxgkDdiRender` / `RenderKm` / `Patch` → `STATUS_NOT_IMPLEMENTED`; `DxgkDdiBuildPagingBuffer` emits no DMA;
  `DxgkDdiSubmitCommand` is a null engine (completes the fence immediately). Note: actual Helios rendering
  goes through the **ICD → D3DKMTEscape (venus)** out‑of‑band path, NOT `DxgkDdiRender`, so the command path
  being stubbed is fine — **the gap is the WDDM memory/allocation model, not the command model.**
- `DxgkDdiCreateAllocation` IS real (`create_allocation.rs:132-166`, makes venus HOST3D blobs).

**Current correction (2026-06-23):** loading Helios still leaves `QueryDisplayConfig` at
`active/all/database paths=0`. A same-boot controlled test with Helios disabled also returned
0 paths and `SetDisplayConfig` ret 31, but a clean boot without gpu-gl/Helios does not: WMI
reports the LG monitor active, `Win32_VideoController` reports the Looking Glass IDD at
`1920x1080`, a session-1 CCD probe reports active/all/database paths, `AssignSwapChain` fires,
and the Looking Glass client displays the desktop. Use that clean gpu-gl-out boot as the
baseline; treat the same-boot Helios-disabled result as contaminated by live graphics-stack
state.

## 3. The CHeliosSink direct path (scaffolded, currently OFF)

There is already a **direct Helios‑venus → IDD** path, gated behind a setting:

- **Consumer (IDD):** `CHeliosSink` (`LookingGlass/idd/LGIdd/CHeliosSink.cpp`, wired in
  `CIndirectDeviceContext.cpp:228` Init / `:1006` `PresentHeliosFrame`). Enabled only if registry/settings
  `HeliosEnable == true` (default **false** → no‑op today, `CHeliosSink.cpp:308`). When on, it:
  1. opens a device (`OpenDevice()`), loads `vulkan-1.dll` (the Helios venus ICD), creates a VkDevice;
  2. reads a **resource id** from a *gate file* (`HeliosGateFile`, default
     `C:\Users\Rupansh\helios_lg_idd_resid.txt`; also exported as env `HELIOS_GATE_RESID_FILE`) via
     `read_resid_for_size(gateFile, size)` (`CHeliosSink.cpp:70, 488`);
  3. imports that venus resource as a Vulkan image — `VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT`,
     `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` + `DRM_FORMAT_MOD_LINEAR`, `VK_FORMAT_B8G8R8A8_UNORM`
     (`CreateImage`, `CHeliosSink.cpp:~400`);
  4. `Present(frame, fb->data)` copies it into the LGMP/KVMFR frame buffer → Looking Glass client.
- **Producer (ICD):** the venus ICD writes the rendered resource's resid to `HELIOS_GATE_RESID_FILE`
  (`icd/mesa/src/virtio/vulkan/vn_renderer_helios.c:1091`); `icd/win-build/helios_vk_present.c` is the
  present helper that sets the env var before `LoadLibrary(vulkan-1.dll)`.

**Critical constraint:** `PresentHeliosFrame` is called from **inside the IddCx swapchain processor thread**
(`CSwapChainProcessor.cpp:250`), so CHeliosSink still only runs when the OS has assigned the IDD a swapchain
(i.e. is compositing the IDD monitor on *some* render adapter). It overwrites the OS‑composed frame with the
Helios‑imported one. So **it is NOT independent of the OS composition / IddCx swapchain** as written.

**The Mesa ICD's "native" present is GDI** (`icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp` + `ICD.md:449-451`):
venus image → readback → DIB section → `BitBlt` to the window HDC. That gets a Helios‑rendered *window* onto
the desktop, which DWM (on WARP) composites and the IDD captures — i.e. Helios apps are already visible in LG
**via WARP composition** when the display works. The direct `HELIOS_LG_DIRECT` producer was removed 2026‑06‑17;
CHeliosSink (Vulkan dma‑buf import) is its replacement, gated off.

## 4. DECISION (locked 2026‑06‑18): fake VidMm + maintain coherence → DWM composites on Helios

**Goal: a truly hardware‑accelerated Windows desktop — DWM composites the whole desktop ON Helios (venus → host
GPU) — captured by the IDD and shown in Looking Glass.**

We are committing to the **heavy path on purpose.** The lighter "hybrid‑discrete + cross‑adapter" alternative
(apps render on Helios, DWM stays on WARP — `using-cross-adapter-resources-in-a-hybrid-system.md`) is **rejected**:
it leaves the desktop *software*‑composited, which is not the goal. We want DWM itself on the GPU. The cost is
making VidMm accept Helios as a render adapter it can allocate + composite into — i.e. a **fake‑but‑coherent WDDM
GpuMmu memory model backed by venus.** This is judged worth the debugging pain; we have the tools (host Vulkan
validation, Windows kernel debugger, the near‑ready WDDM driver + forwarder UMD).

**The model — "fake but coherent":**
- The **host GPU already owns the real MMU.** The guest GpuMmu (GPU virtual addresses, page tables) is
  **decorative**: venus addresses resources by opaque id (the UMD's venus command stream over `D3DKMTEscape`), and
  the host GPU never reads the guest page tables. So the addressing half of VidMm's model can be fiction — VidMm
  cannot verify the GPU honored it.
- **WDDM allocations map to venus resources** (`DxgkDdiCreateAllocation` already makes venus HOST3D blobs). DWM's
  composition render targets become host‑GPU‑backed venus resources; the forwarder UMD renders into them via venus.
- **Coherence is required only where DWM / the GPU / the IDD actually observe state** — three points:
  1. **Fences/sync (the hard one):** the venus submission must *drive* the WDDM fence (fence reflects real venus
     completion). The current null‑engine immediate‑fence (`submit_command.rs`) is wrong — DWM would composite/present
     before the frame is rendered. This is the #1 coherence task.
  2. **CPU/IDD‑readable backing:** anything DWM/the runtime `Lock`/`Map`s, and the composed primary the IDD reads,
     must be a **host‑visible venus blob** (`MAP_BLOB` BAR path) with correct cache coherency, so reads see the
     venus‑rendered pixels (same surface as the already‑fixed host‑visible cache‑coherency bugs).
  3. **No paging:** size the segment(s) so VidMm never evicts — everything always resident (venus resources are
     permanently on the host). Sidesteps eviction/page‑in, which is meaningless for venus.
- **The open unknown to resolve early:** does VidMm accept a *fully decorative* GpuMmu, or do its consistency checks
  (page‑table format/validation, GpuMmu cap verification that already rejected the bare CpuVisible segment) demand a
  more‑real model? The kernel debugger answers this.

Contrast: GPU‑PV (`gpu-paravirtualization.md`) is **real‑but‑proxied** (guest GPU‑VA forwarded to the host's real
WDDM driver over VM bus). Ours is **fake/decorative + venus‑backed** — a different, unproven shape; the risk is
VidMm's GpuMmu contract having enough teeth that pure fiction breaks. Once VidMm is satisfied, the IDD selects
Helios (drop the WARP force‑select), the OS composites the desktop on Helios, and the IDD acquires the composed
frame via the standard `IddCxSwapChainReleaseAndAcquireBuffer`; since Helios has no D3D12, the IDD reads the
composed venus surface via D3D11 or the CHeliosSink Vulkan import.

## 5. Two‑step execution (separate sessions)

**★ PRIMARY IMPLEMENTATION REFERENCE: `virtio-research-only-3d/viogpu/viogpu3d/`** — a complete, working WDDM 3D
driver over **virtio‑gpu** (the same device family Helios uses) that implements a **real VidMm model**. It is the
closest existing analogue to what we're building. Key files: `viogpu_adapter.cpp/.h` (DRIVERCAPS, memory‑model caps,
segment descriptors, QueryAdapterInfo), `viogpu_allocation.cpp/.h` (CreateAllocation / DescribeAllocation /
GetStandardAllocationDriverData / BuildPagingBuffer / the segment+residency model), `viogpu_command.cpp/.h`
(SubmitCommand / Patch / fence+interrupt path), `driver.cpp/.h` (the registered DDI table), `viogpu_vidpn.cpp/.h`
(display/scanout — less relevant since Helios is render‑only, but shows the present side). **Difference to mind:**
viogpu3d moves allocation memory with virtio‑gpu **TRANSFER queues** (guest↔host copies) and is a full **display**
driver; Helios uses **zero‑copy host‑visible BAR mappings** (`MAP_BLOB`) and is **render‑only**. So use viogpu3d as
the structural template for the VidMm DDIs (how a virtio guest declares segments/caps, implements paging/allocations,
wires fences), but keep our zero‑copy + render‑only + venus (Vulkan‑command, not 2D/3D virgl) specifics.

**STEP 1 — RESEARCH (the next session; produces a doc, NO code).** Gather everything needed to implement the fake
VidMm: every WDDM GpuMmu memory‑model DDI + struct + cap the driver must implement, with the exact `wdk-sys`/
bindgen type names and field layouts; the GpuMmu page‑table / paging‑buffer operation set; the coherence design
(fence path, readback, residency); how WDDM allocations map to venus resources; **how viogpu3d implements each of
these (it's the working template)**; the GPU‑PV reference for ideas; the current `kmd_render` state (what's stubbed
vs real); the forwarder‑UMD render path; the IDD changes; and the ranked open questions/risks for Step 2. Output: a
self‑contained implementation‑reference doc (see the Step‑1 handoff for the exact contents). No driver code in Step 1.

**STEP 2 — IMPLEMENT + DEBUG (a later session).** Implement the GpuMmu DDIs + segment(s) + allocation→venus mapping
+ fence coherence + readback, then debug with the kernel debugger (does VidMm accept the decorative GpuMmu? does
DWM get past CreateDevice + allocate composition surfaces?) and Vulkan validation, iterating until DWM composites
on Helios and the IDD displays the desktop in Looking Glass.

## 6. State of the codebase right now (so the new session knows the baseline)

- **KMD blob/context leak: FIXED + verified** (commit `5227827`; owner‑tagged reclaim at
  `DxgkDdiDestroyDevice`; zero `0xc000009a` across testing; `0x0E` diag confirms owner match + reclaim fires).
  The DEPLOYED e0bd `.sys` carries this **plus temporary `0x0E`/`MAX_STEPS=3000` diagnostics** (uncommitted in
  the tree — clean them up or rebuild from `HEAD` before shipping).
- **D3D11 forwarder UMD: works** (commits up to `042c6a0`) — DXVK over venus; clear/triangle/constant‑buffer
  validated; input layouts (ISGN‑resolved, lazy) / VB‑IB / blend / benign present added (compile + load‑clean,
  venus‑untested). This is what lets DWM/apps create a *working* D3D11 device on Helios — they still abandon it
  for the §2 memory reason, not a UMD reason.
- **Deploy reality:** use `Z:\tools\install-helios-kmd.ps1` for KMD packages. It signs with a
  machine-store `WDRLocalTestCert`, publishes to the existing PCI devnode with
  `devcon update <inf> <hardware-id>` when DevCon is present, falls back to
  `pnputil /add-driver /install` only when requested/needed, and verifies Code 0. Do not manually
  overwrite DriverStore files during normal iteration. The script's `-BinaryOnly` mode is an
  emergency already-trusted-package override; it backs up the active store, regenerates/signs the
  active catalog, and hash-verifies the copy.
- **Looking Glass:** IDD (`ROOT\DISPLAY\0000`) + monitor enumerate OK; logs to
  `C:\ProgramData\Looking Glass (IDD)\looking-glass-idd.txt`. CHeliosSink path present but OFF.
- The Mesa ICD is a submodule at `icd/mesa` (checked out); win build glue in `icd/win-build`.
