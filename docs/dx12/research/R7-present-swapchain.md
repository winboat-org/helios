# R7 — Presentation: how a D3D12 frame reaches the Helios scanout

Research dossier, lane R7. Read-only research; nothing in the tracked tree was modified.
Repo root `/home/rupansh/helios-vgpu`. All file:line citations are against the tree as of
commit `4739649` (branch `wddm`), submodule `vkd3d-proton-helios` at `2c7ba22c53261458a7a204c55f3098ad9855cb15`
(`git log -1` in the submodule, 2026-08-04).

**Evidence key.** `[HDR]` = the SDK/WDK header says it. `[MS]` = Microsoft documentation says it.
`[CODE]` = in-tree source does it. `[MEAS]` = a measurement recorded in this repo. `[INFER]` = my
inference, marked as such. **UNVERIFIED** = not established; the settling read/experiment is stated.

---

## 0. Executive summary (the five facts that matter)

1. **D3D12's UMD present DDI exists and is *not* shaped like D3D11's.** `pfnPresent` is a member of
   the **command-list** function table (`D3D12DDI_COMMAND_LIST_FUNCS_3D_00xx`), signature
   `PFND3D12DDI_PRESENT_0051(HCOMMANDLIST, HCOMMANDQUEUE, const D3D12DDIARG_PRESENT_0001*, out D3D12DDI_PRESENT_0051*, ...)`.
   It **returns** the source/destination `D3DKMT_HANDLE`s to the runtime rather than calling a
   present callback: there is **no `pfnPresentCb` anywhere in `d3d12umddi.h`**. [HDR]
2. **vkd3d-proton cannot present without a DXVK-style DXGI.** It implements
   `IDXGIVkSwapChainFactory` on its `ID3D12CommandQueue` and consumes an `IDXGIVkSurfaceFactory`
   handed to it; it never creates a `VkSurfaceKHR` itself, and it ships no DXGI. Microsoft's
   `dxgi.dll` does not know these interfaces. [CODE] + README.
3. **The Helios Venus ICD *does* implement `VK_KHR_win32_surface` + `VK_KHR_swapchain`, and it
   already has a hardware flip present** — the "dcomp vehicle" in `wsi_common_win32.cpp`, default
   **ON** since the 28th session. [CODE] + live `vulkaninfo`.
4. **Consequently `ROADMAP.md:2385` ("only the VULKAN client class … lacks a HW present") is a
   stale 2026-07-06 statement**, and `DX12.md:154-167` inherits that staleness. The hand-off it says
   is missing was built: the ICD mints the present through its own D3D11-on-Helios vehicle device,
   which re-enters the *working* `dxgi_present` path in `umd/`.
5. **The real blocker is a hard, code-proven conflict, not an unknown: the DXVK `dxgi.dll` that
   vkd3d requires *breaks the vehicle*.** The vehicle does `LoadLibraryA("dxgi.dll")` and calls
   `IDXGIFactory4::CreateSwapChainForComposition`; DXVK's DXGI returns **`E_NOTIMPL`** for that
   method by default. So naively deploying vkd3d + DXVK-DXGI app-local silently demotes every
   vkd3d frame to the software GDI blit. The fix is small and local (load MS's DXGI by full system
   path in the vehicle runtime). See §6 Risk **V2** — **CONFIRMED**, `[CODE]`.

---

## 1. How D3D11 presents today on Helios, end to end

This is the reference chain the D3D12 path must match. Every hop cited.

### 1.1 App → DXGI → UMD

| # | Hop | Evidence |
|---|-----|----------|
| 1 | `IDXGISwapChain::Present` → Microsoft `dxgi.dll` → `d3d11.dll` → the UMD's DXGI base DDI table | [MS] `windows-driver-docs-pr/display/dxgi-presentation-path.md` |
| 2 | UMD installs `pfnPresent = dxgi_present` into `DXGI_DDI_BASE_FUNCTIONS` | `umd/src/forward/tables.rs:12-21` (`install_dxgi`), reached from `umd/src/device_funcs.rs:1274` / `:1289-1298` and `umd/src/adapter.rs:303,347` |
| 3 | `dxgi_present(*mut DXGI_DDI_ARG_PRESENT)` | `umd/src/forward/present.rs:1231-1234` → `dxgi_present_impl` at `:1239` |
| 4 | Args decoded: `hSurfaceToPresent`→`src_h`, `hDstResource`→`dst_h`, both translated to WDDM allocation handles | `present.rs:1254-1260`; `DXGI_DDI_ARG_PRESENT` field list verified in the WDK: `C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0\um\dxgiddi.h:84-94` (`win_exec` `Select-String`) |
| 5 | `DXGI_DDI_PRESENT_FLAGS` bit meanings: `Blt`=0x1, `Flip`=0x2, `PreferRight`=0x4, `TemporaryMono`=0x8, `AllowTearing`=0x10, `AllowFlexibleRefresh`=0x20, `NoScanoutTransform`=0x40 | `dxgiddi.h:64-81` (same read). The UMD reads bit 0 at `present.rs:1347`; the KMD reads bit 0 and bit 2 at `kmd_render/src/ddi/display.rs:431,773` |

### 1.2 The three arms inside `dxgi_present`

`present.rs:1297-1398` selects exactly one of:

* **Vehicle arm** — a thread-local `VehicleSlot::Armed` was set by the ICD immediately before this
  `Present()` on the same thread. `vehicle_present_prepare` alias-imports the ICD's frame by venus
  resid and copies it into the DXGI backbuffer; a failure **fails the present** so the ICD latches
  its sw fallback. `present.rs:1272-1308`, body in `umd/src/forward/vehicle.rs:187-297`.
* **Direct-flip arm** — `presented_primary_private` (`umd/src/forward/state.rs:736`) returns `Some`,
  i.e. this source *is* the scan-out backing. **No copy.** Optionally records a D4b snapshot blit
  (`snapshot_for_present`, `umd/src/forward/snapshot.rs:219`). `present.rs:1318-1346`.
* **Windowed/BLT arm** — `CopySubresourceRegion(dst ← src)` into win32k's redirection surface, then
  `context.Flush()`. `present.rs:1328-1331,1391`.

Then, unconditionally:

* Cross-process present ordering publish (`publish_present_order`) — either folded into the frame
  batch before the Flush (`present.rs:1370-1390`, knob `present_batch_fold`) or after
  (`present.rs:1425-1436`).
* Frame gate (`run_present_frame_gate`, defined `umd/src/forward.rs:555`) unless the async
  present-stream marker is eligible — `present.rs:1479-1528`.
* `finish_present` (`present.rs:1090-1227`) builds `DXGIDDICB_PRESENT` (`hSrcAllocation`,
  `hDstAllocation`, `hContext`, `pDXGIContext`, `bOptimizeForComposition`) — note
  `PrivateDriverDataSize = 0` since the 0ab-C close-out (`present.rs:1169-1177`).
* `submit_runtime_present_then_call` (`present.rs:945-981`) enforces the DDI ordering:
  **`pfnRenderCb` first, then `pfnPresentCb`.** `pfnRenderCb` carries an inline
  `HeliosPresentRenderCmd` (or `HeliosPresentRefreshCmd`) plus the source/destination allocation
  list (`present.rs:779-911`, `RuntimePresentDependencies::write_to` at `:388-424`).

**The identity channel is the Render command, not the present private data.** `present.rs:1169-1176`
and `kmd_render/src/ddi/display.rs:146-153` both record the measured reason: dxgkrnl never forwarded
`DXGIDDICB_PRESENT.pPrivateDriverData` to `DxgkDdiPresent` on the DMA-flip path — `PBIdOk` read 2
("no payload") across three driver generations. [MEAS, as recorded in-tree]

### 1.3 Kernel: `DxgkDdiPresent`

`kmd_render/src/lib.rs:186` sets `data.DxgkDdiPresent = Some(ddi::dxgkddi_present)`.
`kmd_render/src/ddi/display.rs:179` is the entry; `:217` `dxgkddi_present_inner`.

* Payload union arm decoded **once** from the flags — `PresentPayload::decode`
  (`kmd_render/src/ddi/present_packet.rs:594-607`); `FlipWithMultiPlaneOverlay` (bit 12) is refused
  `STATUS_NOT_SUPPORTED` because the MPO3 KMD interface is not registered (`display.rs:254-261`).
* Fixed slots: `DXGK_PRESENT_SOURCE_INDEX` / `DXGK_PRESENT_DESTINATION_INDEX`
  (`present_packet.rs:558-559,671-673`).
* **BLT arm** (`flags & 1`) — `display.rs:431-770`: validate DMA + patch capacity *before* any host
  GPU work (`validate_patch_capacity`, `present_packet.rs:728-748`), then either the two-phase
  WindowedBlt snapshot transaction (`display.rs:604-666`) or a direct
  `client.submit_present_blt(...)` Venus copy (`display.rs:675-677`), fence merged into
  `PresentSubmissionPrivate` (`present_packet.rs:299-344`).
* **Flip arm** (`flags & (1<<2)`) — `display.rs:773-917`. Two sub-contracts:
  * `pDmaBuffer == NULL` ⇒ **MMIO flip**: return `STATUS_SUCCESS`; dxgkrnl will name the primary
    later through `DxgkDdiSetVidPnSourceAddress` (`display.rs:832-838`).
  * `pDmaBuffer != NULL` ⇒ **DMA-buffer flip**: write `PresentFlipPrivate` (allocation handle,
    `PhysicalAddress`, optional D4b snapshot descriptor) into the kernel-only private data
    (`display.rs:900-912`, struct at `present_packet.rs:65-94`, `write` at `:103-156`). dxgkrnl will
    **not** call `SetVidPnSourceAddress` for this flip. The rationale (and the measured rejection of
    `FlipImmediateMmIo` as a substitute) is `present_packet.rs:42-64`.
* Patch list written once (`write_patch_references`, `present_packet.rs:765-801`), a
  `HeliosPresentRefreshCmd` is written into the DMA buffer to keep it structurally non-empty
  (`display.rs:936-970`), and the registered stream boundary is merged (`display.rs:972-983`).

### 1.4 Kernel: submit → bind → scanout

* `DxgkDdiSubmitCommand` consumes the flip record: `arm_dma_flip` at
  `kmd_render/src/ddi/submit_command.rs:588-620`, which takes `PresentFlipPrivate::take`
  (`present_packet.rs:170-224`, one-shot: it zeroes the magic) and calls
  `crate::ddi::display::arm_dma_flip_programming` (`display.rs:1346`).
* On the MMIO path the same state is reached through `dxgkddi_set_vidpn_source_address`
  (`display.rs:1246-1325`) — split into a DIRQL-safe half (`set_vidpn_source_address_dirql`,
  `display.rs:1624`) and a PASSIVE continuation (`process_deferred_vidpn_source_address`,
  `display.rs:1674`; `apply_vidpn_source_address`, `display.rs:1816`).
* `program_vidpn_source` (`display.rs:2221`) → `program_vidpn_source_inner` (`display.rs:2258`) →
  **`crate::virtio::ctrl::set_scanout_blob(...)`** at `display.rs:2510-2520`, carrying
  `ScanoutSetTimeline { request, present_epoch, carried_watermark, flags }`.
* Ordering telemetry rides the fixed always-on ring `kmd_render/src/ddi/scanout_timeline.rs`
  (32 768 slots, `SLOT_COUNT` at `:15`; event kinds at `:21-58`, including
  `PRESENT_RETURN = 27` written by `dxgkddi_present` itself at `display.rs:201-213`).

### 1.5 One-line summary of the D3D11 chain

```
IDXGISwapChain::Present
  → MS dxgi.dll / d3d11.dll
  → helios_umd!dxgi_present                       (umd/src/forward/present.rs:1231)
      [vehicle copy | direct-flip no-copy | CopySubresourceRegion to redirection surface]
      → context.Flush()  → publish_present_order  → run_present_frame_gate
      → pfnRenderCb  (HeliosPresentRenderCmd + allocation list)
      → pfnPresentCb (DXGIDDICB_PRESENT: hSrc/hDstAllocation, hContext)
  → dxgkrnl
  → helios kmd!DxgkDdiPresent                     (kmd_render/src/ddi/display.rs:179)
      [BLT: Venus vkCmdBlit/copy  |  Flip: PresentFlipPrivate into DMA private data]
  → DxgkDdiSubmitCommand → arm_dma_flip           (submit_command.rs:588)
    (or DxgkDdiSetVidPnSourceAddress on the MMIO path, display.rs:1246)
  → program_vidpn_source → virtio ctrl::set_scanout_blob   (display.rs:2510)
  → QEMU (qemu-helios) → egl-headless → VNC
```

---

## 2. How D3D12 presents on real Windows

### 2.1 There *is* a driver present entry point, and it is on the command-list table

`tmp/dx12/sdk/d3d12umddi.h` (Windows SDK 10.0.26100.0, 19 031 lines).

```c
/* d3d12umddi.h:1624-1628 */
typedef struct D3D12DDI_ARG_PRESENTSURFACE
{
    D3D12DDI_HRESOURCE hSurface;
    UINT               SubResourceIndex;
} D3D12DDI_ARG_PRESENTSURFACE;

/* d3d12umddi.h:1630-1644 */
typedef struct D3D12DDIARG_PRESENT_0001
{
    CONST D3D12DDI_ARG_PRESENTSURFACE*  phSurfacesToPresent;
    UINT                                SurfacesToPresent;
    D3D12DDI_HRESOURCE                  hDstResource;
    UINT                                DstSubResourceIndex;
    DXGI_DDI_PRESENT_FLAGS              Flags;
    DXGI_DDI_FLIP_INTERVAL_TYPE         FlipInterval;
    D3DDDI_VIDEO_PRESENT_SOURCE_ID      VidPnSourceID;
    CONST RECT*                         pDirtyRects;
    UINT                                DirtyRects;
    UINT                                PrivateDriverDataSize;
    VOID*                               pPrivateDriverData;
    BOOL                                OptimizeForComposition;
} D3D12DDIARG_PRESENT_0001;
```

The newest revision in this header:

```c
/* d3d12umddi.h:7226-7251 */
typedef struct D3D12DDI_PRESENT_0051
{
    D3DKMT_HANDLE   BroadcastSrcAllocation[D3DDDI_MAX_BROADCAST_CONTEXT+1];
    D3DKMT_HANDLE   BroadcastDstAllocation[D3DDDI_MAX_BROADCAST_CONTEXT+1];
    BOOL            AddedGpuWork;
    UINT            BackBufferMultiplicity;
    BOOL                        SyncIntervalOverrideValid;
    DXGI_DDI_FLIP_INTERVAL_TYPE SyncIntervalOverride;
} D3D12DDI_PRESENT_0051;

typedef struct D3D12DDI_PRESENT_CONTEXTS_0051 { HANDLE hContext; UINT BroadcastContextCount; HANDLE BroadcastContext[...]; } ...;
typedef struct D3D12DDI_PRESENT_HWQUEUES_0051 { UINT BroadcastQueueCount; HANDLE hHwQueues[...]; } ...;

typedef VOID ( APIENTRY* PFND3D12DDI_PRESENT_0051 ) ( D3D12DDI_HCOMMANDLIST, D3D12DDI_HCOMMANDQUEUE,
    _In_ CONST D3D12DDIARG_PRESENT_0001*,
    _Out_ D3D12DDI_PRESENT_0051*, _Out_opt_ D3D12DDI_PRESENT_CONTEXTS_0051*, _Out_opt_ D3D12DDI_PRESENT_HWQUEUES_0051* );
```

`pfnPresent` sits next to `pfnBlt` in the 3D command-list table — e.g.
`D3D12DDI_COMMAND_LIST_FUNCS_3D_0003` at `d3d12umddi.h:2997-3021` (`pfnBlt` `:3020`,
`pfnPresent` `:3021`) and the 0051 table at `:7262-7275`. `pfnPresent` appears in **20** distinct
command-list tables in this header (`grep -c` on the `PFND3D12DDI_PRESENT_*` lines under
`pfnPresent`), spanning revisions 0003 / 0028 / 0051.

There is also a device-funcs entry `pfnGetPresentPrivateDriverDataSize`
(`PFND3D12DDI_GET_PRESENT_PRIVATE_DRIVER_DATA_SIZE`, typedef `d3d12umddi.h:1792`; first appearance
in a table at `:3171`), asking the driver how many bytes of present private data it wants for a
given `D3D12DDIARG_PRESENT_0001`.

### 2.2 There is **no** DXGI DDI table and **no** present callback in D3D12

* `grep -n "DXGIDDICB\|PresentCb\|DXGI_DDI_BASE" tmp/dx12/sdk/d3d12umddi.h` → **zero hits.** [HDR]
* The four D3D12 core-layer callback structs — `D3D12DDI_CORELAYER_DEVICECALLBACKS_0003`
  (`:2624-2653`), `_0022` (`:4874-4905`), `_0050` (`:7178-7218`), `_0062` (`:8606-8647`) — contain
  `pfnSetErrorCb`, `pfnSetCommandListErrorCb`, `pfnSetCommandListDDITableCb`, `pfnCreateContextCb`,
  `pfnCreateContextVirtualCb`, `pfnDestroyContextCb`, `pfnCreatePagingQueueCb`,
  `pfnDestroyPagingQueueCb`, `pfnMakeResidentCb`, `pfnEvictCb`, `pfnReclaimAllocations2Cb`,
  `pfnOfferAllocationsCb`, `pfnAllocateCb`, `pfnDeallocateCb`,
  `pfnCreateSchedulingGroupContextCb`, `pfnCreateSchedulingGroupContextVirtualCb`,
  `pfnCreateHwQueueCb`, `pfnQueueBackgroundProcessingWorkCb`. **No present callback.** [HDR]

**[INFER, high confidence]** Because the D3D12 driver has no present callback and its `pfnPresent`
*outputs* `hSrcAllocation`/`hDstAllocation`/`hContext`/`hHwQueues`, **the D3D12 runtime — not the
driver — issues the kernel present** (`D3DKMTPresent`, declared
`tmp/dx12/sdk/d3dkmthk.h:5929`, arg struct `_D3DKMT_PRESENT` at `:754`). The driver's job at
present time is (a) optionally record GPU work into the given command list and set `AddedGpuWork`,
(b) name the allocations, (c) name the context/HW-queue that must be synchronised against.

> **UNVERIFIED:** that the runtime literally calls `D3DKMTPresent` (vs. an internal variant such as
> `D3DKMTPresentRedirected`, `d3dkmthk.h:6078`). *Settling experiment:* run any D3D12 sample from
> `dx-samples-research-only/Samples/Desktop/` on a real driver under an ETW
> `Microsoft-Windows-DxgKrnl` all-keywords trace (recipe: ROADMAP tooling section) and read the
> `Present`/`QueuePacket` events, or set a breakpoint on `D3DKMTPresent` in `d3d12.dll`/`dxgi.dll`.
> This does **not** block the Helios decision — under every variant the KMD still sees
> `DxgkDdiPresent`.

### 2.3 What the runtime requires of the driver, per swapchain model

* **BLT-model swapchains: not applicable.** [MS] "In D3D12, only `DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL`
  and `DXGI_SWAP_EFFECT_FLIP_DISCARD` are supported, and the bitblt models are not"
  (learn.microsoft.com `DXGI_SWAP_EFFECT` / `direct3d12/swap-chains`; corroborated by web search).
  This removes an entire arm that D3D11 has: the UMD's `CopySubresourceRegion` into win32k's
  redirection surface has **no D3D12 analogue**.
* **`CreateSwapChainForHwnd` takes a *command queue*, not a device**, for D3D12. [MS] same sources.
  This is why vkd3d-proton hangs `IDXGIVkSwapChainFactory` off `d3d12_command_queue`
  (`vkd3d-proton-helios/libs/vkd3d/command.c:22282-22287`).
* **FLIP_DISCARD / FLIP_SEQUENTIAL windowed**: the backbuffer is handed to DWM; there is no
  driver-side src→dst copy. `D3D12DDIARG_PRESENT_0001.hDstResource` will therefore normally be
  null. **[INFER]** — the header allows a destination; nothing in the header says DXGI will not use
  it. **UNVERIFIED**; *settling experiment:* the same ETW/instrumented-driver read as above, or
  instrument `pfnPresent` if/when a Helios D3D12 UMD exists.
* **Fullscreen exclusive**: `D3D12DDIARG_PRESENT_0001` carries `VidPnSourceID` and `FlipInterval`,
  which is the same information a D3D11 flip present carries into `DXGI_DDI_ARG_PRESENT`. The
  driver-side scan-out contract below dxgkrnl is unchanged (`DxgkDdiPresent` + MMIO/DMA flip). **No
  D3D12-specific miniport DDI exists** — corroborated in-tree by `DX12.md:107-110`.

### 2.4 Net for Helios

If Helios ever ships a real D3D12 UMD, **everything from `dxgkrnl` down is reused unchanged**: the
same `DxgkDdiPresent` flip arm, the same `PresentFlipPrivate`, the same `set_scanout_blob`. What
changes is *above* dxgkrnl: no `pfnPresentCb`, no `hDstResource` copy, no `HeliosPresentRenderCmd`
riding a `pfnRenderCb` (D3D12 has `pfnCreateContextCb`/`pfnCreateContextVirtualCb` but submission is
`D3DKMTSubmitCommand`-shaped through the runtime). The per-present identity channel that Helios
relies on today (`HeliosPresentRenderCmd` → per-context stash → `DxgkDdiPresent`) **would have to be
rebuilt** on the D3D12 submission model.

---

## 3. How vkd3d-proton presents

Read: `vkd3d-proton-helios/libs/vkd3d/swapchain.c` (4 179 lines), plus
`include/vkd3d_swapchain_factory.idl`, `libs/d3d12core/main.c`, `libs/vkd3d/command.c`.

### 3.1 It is a *backend* for someone else's DXGI

* `libs/vkd3d/command.c:22282-22287`: `d3d12_command_queue_QueryInterface` answers
  `IID_IDXGIVkSwapChainFactory` with `command_queue->vk_swap_chain_factory`.
* `include/vkd3d_swapchain_factory.idl:137-145`:
  `IDXGIVkSwapChainFactory::CreateSwapChain(IDXGIVkSurfaceFactory*, const DXGI_SWAP_CHAIN_DESC1*, IDXGIVkSwapChain**)`.
  UUID `e7d6c3ca-23a0-4e08-9f2f-ea5231df6633` (`idl:135`).
* `libs/vkd3d/swapchain.c:1525-1537`: the surface is **not** created by vkd3d —
  `IDXGIVkSurfaceFactory_CreateSurface(pFactory, vk_instance, vk_physical_device, &chain->vk_surface)`.
  vkd3d then only checks `vkGetPhysicalDeviceSurfaceSupportKHR` on its queue family
  (`:1539-1552`).
* `vkd3d-proton-helios/README.md:173-174`: *"vkd3d-proton does not supply the necessary DXGI
  components on its own. Instead, DXVK (2.1+) and vkd3d-proton share a DXGI implementation."*
* `libs/d3d12/d3d12.def` and `libs/d3d12core/d3d12core.def` export only
  `D3D12CreateDevice`, `D3D12GetDebugInterface`, `D3D12GetInterface`, the root-signature helpers and
  `D3D12SDKVersion`. **No DXGI exports.** [CODE]

So: **on native Windows, vkd3d-proton presents through DXVK's `dxgi.dll`, which creates the
`VkSurfaceKHR` with `vkCreateWin32SurfaceKHR` and then hands it to vkd3d, which drives a real
`VkSwapchainKHR` with `vkQueuePresentKHR`.** There is no "wrap a DXGI swapchain" mode and no MS-DXGI
mode. Confirmed on the DXVK side in-tree:

* `dxvk-helios/src/dxgi/dxgi_factory.cpp:524-578` — `DxgiFactory::CreateSwapChainBase` does
  `pDevice->QueryInterface(IID_PPV_ARGS(&dxvkFactory))` for `IDXGIVkSwapChainFactory`; on failure it
  logs *"DXGI: CreateSwapChainForHwnd: Unsupported device type"* and returns
  `DXGI_ERROR_UNSUPPORTED` (`:573-575`).
* `dxvk-helios/src/dxgi/dxgi_surface.cpp:42-48` — `DxgiSurfaceFactory::CreateSurface` forwards to
  `wsi::createSurface(m_window, m_vkGetInstanceProcAddr, Instance, pSurface)` — i.e. it creates the
  surface **on the instance vkd3d passed in**, not on a DXVK instance.
* `dxvk-helios/src/wsi/win32/wsi_window_win32.cpp:341-359` —
  `Win32WsiDriver::createSurface` resolves and calls `vkCreateWin32SurfaceKHR`.
* `dxvk-helios/src/dxgi/meson.build:27-34` builds `dxgi.dll` in this very tree, so the fork already
  produces the required component.

### 3.2 What vkd3d requires of the Vulkan implementation, at instance/device create

`libs/d3d12core/main.c:568-590` and `:657-668`:

```c
static const char * const instance_extensions[] = {
    VK_KHR_SURFACE_EXTENSION_NAME,
#ifdef _WIN32
    VK_KHR_WIN32_SURFACE_EXTENSION_NAME,
#endif
};
static const char * const optional_instance_extensions[] = {
    VK_KHR_SURFACE_MAINTENANCE_1_EXTENSION_NAME,
    VK_EXT_SURFACE_MAINTENANCE_1_EXTENSION_NAME,
    VK_KHR_GET_SURFACE_CAPABILITIES_2_EXTENSION_NAME,
    ...
};
static const char * const device_extensions[] = { VK_KHR_SWAPCHAIN_EXTENSION_NAME, };
static const char * const optional_device_extensions[] = {
    VK_KHR_SWAPCHAIN_MAINTENANCE_1_EXTENSION_NAME,
    VK_EXT_SWAPCHAIN_MAINTENANCE_1_EXTENSION_NAME,
};
```

⚠ **`VK_KHR_surface` + `VK_KHR_win32_surface` are *required* instance extensions and
`VK_KHR_swapchain` is a *required* device extension for `D3D12CreateDevice` itself** — not just for
presenting. `libs/vkd3d/device.c:219-235` only **logs** `ERR("Required %s extension %s is not
supported.")` and still counts the extension into the enable list (`vkd3d_enable_extensions`,
`:331-343` unconditionally copies every required extension into the array), so a missing one
produces `VK_ERROR_EXTENSION_NOT_PRESENT` from `vkCreateInstance`/`vkCreateDevice` and D3D12 device
creation fails outright. **Presentation support is a hard dependency of vkd3d-proton on this
platform, not an optional feature.**

### 3.3 Formats, colour spaces, present modes

* Format selection: `dxgi_vk_swap_chain_select_format` (`swapchain.c:1784-1804`) →
  `find_surface_format` (`:1763-1782`) → `accept_format` (`:1742-1761`). Exact-format match first;
  on `VK_COLOR_SPACE_SRGB_NONLINEAR_KHR` it falls back to any of
  `VK_FORMAT_R8G8B8A8_UNORM` / `VK_FORMAT_B8G8R8A8_UNORM` / `VK_FORMAT_A8B8G8R8_UNORM_PACK32`.
  For a non-sRGB (HDR) colour space with no match it **refuses**: *"Refuse to present unsupported
  HDR since it will look completely bogus."* (`:1800-1802`).
* Present modes: parsed from `VKD3D_SWAPCHAIN_PRESENT_MODE`
  (`IMMEDIATE`/`MAILBOX`/`FIFO`/`FIFO_RELAXED`/`FIFO_LATEST_READY`, `:37-41`). Default selection at
  `:2173-2195`: `swap_interval > 0` ⇒ `FIFO`; `swap_interval == 0` ⇒ `IMMEDIATE`, and if IMMEDIATE
  is unsupported it tries `MAILBOX` then `FIFO`.
* Swapchain create info (`:2199-2248`):
  `imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT`,
  `imageSharingMode = EXCLUSIVE`, `compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR`,
  `preTransform = IDENTITY`, `clipped = VK_TRUE`,
  **`minImageCount = max(3u, surface_caps.minImageCount)`**, and
  **`imageExtent = surface_caps.currentExtent`** clamped to min/max
  (`:2225-2229`) — i.e. vkd3d sizes the swapchain from the *surface*, not from
  `DXGI_SWAP_CHAIN_DESC1`.
* Present: `vkAcquireNextImageKHR(..., UINT64_MAX, ...)` at `:2814`, an internal blit render pass
  (`dxgi_vk_swap_chain_record_render_pass`, `:2400`; `submit_blit`, `:2616`) that copies/scales the
  *user* backbuffer into the acquired swapchain image, then `vkQueuePresentKHR` at `:3112`.
* User backbuffers are ordinary `ID3D12Resource`s vkd3d allocates itself
  (`dxgi_vk_swap_chain_allocate_user_buffer`, `:756`; `reallocate_user_buffers`, `:787`) — **there is
  always a blit between the app's backbuffer and the WSI image**.
* `IDXGIVkSwapChain2::Present` (`:1121-1240`) does no Vulkan work on the caller's thread: it fills a
  `dxgi_vk_swap_chain_present_request` in a ring and enqueues
  `dxgi_vk_swap_chain_present_callback` on the command-queue thread (`:1216`).

### 3.4 Resize / fullscreen

* `dxgi_vk_swap_chain_ChangeProperties` (`:854-910`) reallocates the user buffers; the *Vulkan*
  swapchain is recreated lazily on the present thread —
  `dxgi_vk_swap_chain_request_needs_swapchain_recreation` (`:2339-2349`) →
  `recreate_swapchain_in_present_task` (`:2077-2338`).
* Because `imageExtent` comes from `surface_caps.currentExtent` (`:2225`), a window resize is picked
  up from `GetClientRect` on the Helios side automatically (§4.2).
* **vkd3d does not itself destroy/recreate the `VkSurfaceKHR` on resize** — the surface belongs to
  the `IDXGIVkSurfaceFactory` handed in at `CreateSwapChain`, and DXVK's `DxgiSwapChain` owns that.
  Whether DXVK recreates the surface (and therefore the `VkSurfaceKHR`) on a fullscreen transition
  is **UNVERIFIED**; *settling read:* `dxvk-helios/src/dxgi/dxgi_swapchain.cpp` fullscreen/resize
  paths. This matters only because it is the exact shape ROADMAP blamed for the
  one-dcomp-target-per-HWND defect (§5.2) — which is now fixed and refcounted by HWND, so the answer
  no longer changes the design.

---

## 4. What the Helios Vulkan ICD offers for WSI

**It offers a complete Win32 WSI, including a hardware flip present. This is the single most
important correction this dossier makes to `DX12.md`.**

### 4.1 Extensions actually advertised (code + live probe)

| Extension | Code | Live guest evidence |
|---|---|---|
| `VK_KHR_surface` | `icd/mesa/src/virtio/vulkan/vn_instance.c:41` | `tmp/dx12/research/guest-vulkaninfo-full.txt:32` (`revision 25`) |
| `VK_KHR_win32_surface` | `vn_instance.c:59-65` | `guest-vulkaninfo-full.txt:35` (`revision 6`) |
| `VK_KHR_get_surface_capabilities2` | `vn_instance.c:40` | `:30` |
| `VK_KHR_surface_maintenance1` | `vn_instance.c:42` | `:33` |
| `VK_EXT_surface_maintenance1` | `vn_instance.c:44` | `:23` |
| `VK_KHR_swapchain` | `vn_physical_device.c:1343` | `:1084` (`revision 70`) |
| `VK_KHR_swapchain_maintenance1` | `vn_physical_device.c:1344` | `:1085` |
| `VK_EXT_swapchain_maintenance1` | `vn_physical_device.c:1348` | `:999` |
| `VK_KHR_incremental_present` | `vn_physical_device.c:1335` | `:1040` |
| `VK_KHR_present_id` / `present_wait` (+`2`) | **`#ifndef VK_USE_PLATFORM_WIN32_KHR`** — `vn_physical_device.c:1336-1341` | `grep -c present_wait\|present_id guest-vulkaninfo-full.txt` = **0** |

**Every extension vkd3d-proton requires (§3.2) and every one it treats as optional is present,
except `VK_KHR_present_wait`/`present_id`.** vkd3d handles their absence non-fatally:
`swapchain.c:1494-1499` sets `chain->present.wait` from `presentWait || wait2` and emits
`FIXME_ONCE("Implementation supports neither present_wait1 or present_wait2. Latency will increase.")`.

⚠ `VK_KHR_swapchain` is **conditional** on `physical_dev->renderer_sync_fd.semaphore_importable`
(`vn_physical_device.c:1334`). It is true on this box (the live `vulkaninfo` shows the extension),
but a host/renderer change that drops sync-fd semaphore import would silently remove
`VK_KHR_swapchain` and **kill `D3D12CreateDevice` outright**, not just presentation. Worth a gate
assertion.

### 4.2 How an image reaches the screen

`vn_wsi_init` (`icd/mesa/src/virtio/vulkan/vn_wsi.c:166-245`) **forces software WSI on Windows**:

```c
   const bool use_sw_device =
#ifdef _WIN32
      /* Helios: the Mesa ICD still uses wsi_common_win32 GDI/DIB presentation
       * for Vulkan application windows; ... Force software WSI. */
      true;
```
(`vn_wsi.c:169-185`; the comment explains that gating on `EXT_external_memory_dma_buf` made
`dxgi_get_factory()` fail → `VK_ERROR_INITIALIZATION_FAILED` → zero physical devices.)

and installs a Helios hook:

```c
   physical_dev->wsi_device.win32.get_helios_resource_identity =
      vn_wsi_get_helios_resource_identity;     /* vn_wsi.c:240-241 */
```

which returns `mem->base_bo->res_id` + `size` + `memory_type_index` (`vn_wsi.c:141-161`) — i.e. the
venus resource identity of a swapchain image's memory.

`wsi_common_win32.cpp` then has **three** present paths, selected in
`wsi_win32_surface_create_swapchain` (`:2448-2560`):

* **`WSI_IMAGE_TYPE_DXGI`** (`:2513-2519`) — requires `wsi->dxgi.factory && wsi->dxgi.dcomp &&
  wsi->wsi->win32.get_d3d12_command_queue`. Venus does **not** set `get_d3d12_command_queue`
  (only `get_helios_resource_identity` is set, `vn_wsi.c:240`), so this branch is **dead on
  Helios**. It is the branch `dzn` uses, and it is a real zero-copy composition path
  (`CreateSwapChainForComposition` + `IDCompositionVisual::SetContent`, `:2374-2446`) —
  see §6 option (vi).
* **The Helios "dcomp vehicle"** — `WSI_IMAGE_TYPE_CPU` images + a private D3D11 composition
  swapchain. **Default ON** (`wsi_win32_vehicle_enabled`, `:361-376`: env
  `HELIOS_WSI_DCOMP_PRESENT`, `cached = 1` when unset).
* **The sw GDI/DIB blit** — the fallback whenever the vehicle is `INIT` or `FAILED`
  (`wsi_win32_queue_present`, `:2265-2300`).

**The vehicle, in full** (`wsi_common_win32.cpp:229-268` design comment; build at `:770-928`;
present at `:2067-2258`):

1. On `vkCreateSwapchainKHR`, `wsi_win32_vehicle_start` (`:956-1082`) snapshots hwnd/extent/format
   /buffer count and creates a **named exported timeline semaphore**
   `Global\HeliosPresentFence_<pid>_<start>_<fence_id>` (`:1013-1046`).
2. A **dedicated worker thread** (`:930-955`) does all COM work:
   `D3D11CreateDevice(NULL, D3D_DRIVER_TYPE_HARDWARE, ..., D3D11_CREATE_DEVICE_BGRA_SUPPORT)`
   (`:788-795`) — *the default adapter, i.e. Helios, i.e. our own `helios_umd.dll`* —
   then `CreateSwapChainForComposition` with
   `SwapEffect = DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL`, `Scaling = STRETCH`,
   `BufferCount = max(3, minImageCount)` (`:801-836`, `:984`), a
   `FRAME_LATENCY_WAITABLE_OBJECT` for non-FIFO chains (`:834-845`), a process-global
   refcounted **hwnd→DirectComposition target/visual** entry (`:857-869`), and resolution of the
   three `helios_umd_*` exports by name (`:871-885`).
3. Per present (`wsi_win32_queue_present_vehicle`, `:2067-2258`):
   non-FIFO drop check on the latency waitable (`:2076-2094`) → resolve the frame's venus resid
   (`:2096-2118`) → `wsi_helios_present_sync_publish(resid, pid, fence_id, value)` (`:2130`) →
   `helios_umd_set_present_source(...)` (`:2140`) → `IDXGISwapChain3::Present(interval, flags)`
   with FIFO⇒`Present(1)`, IMMEDIATE⇒`Present(0, ALLOW_TEARING)`, MAILBOX⇒`Present(0)`
   (`:2150-2163`) → first success binds `visual->SetContent(sc)` + `dcomp->Commit()`
   (`:2202-2229`) → recycle gate.
4. That `Present()` re-enters **our own D3D11 UMD** on the same thread:
   `helios_umd_set_present_source` (`umd/src/vehicle_exports.rs:25-43`) armed a thread-local
   (`umd/src/forward/vehicle.rs:85-132`), and `dxgi_present` consumes it
   (`umd/src/forward/present.rs:1272-1308`), imports the ICD frame by resid, copies it into the
   DXGI backbuffer (`vehicle.rs:187-297`), and then runs the ordinary
   `pfnRenderCb`+`pfnPresentCb` path of §1.

**So the "missing hand-off" is not missing.** A Vulkan client on Helios gets a flip-model,
DWM-composited hardware present today, with the pixels moving GPU-side through venus.

### 4.3 Surface capabilities the ICD reports (live)

`wsi_win32_surface_get_capabilities` (`wsi_common_win32.cpp:1172-1224`) plus the live probe
(`tmp/dx12/research/guest-vulkaninfo-full.txt:139-186`):

* formats: `B8G8R8A8_UNORM`, `R8G8B8A8_UNORM`, `B8G8R8A8_SRGB`, **all `SRGB_NONLINEAR` only**
  (source array `wsi_common_win32.cpp:1300-1306`) ⇒ **HDR swapchains from vkd3d will be refused**
  by `dxgi_vk_swap_chain_select_format` (`swapchain.c:1800-1802`).
* present modes: `IMMEDIATE`, `MAILBOX`, `FIFO` — because the vehicle is enabled
  (`wsi_common_win32.cpp:1370-1400`; `present_modes_gdi` is FIFO-only and is used only with the
  vehicle knob off).
* `minImageCount = 1`, `maxImageCount = 0` ⇒ vkd3d's `max(3, ...)` is honoured.
* `currentExtent` = `GetClientRect` of the HWND (`:1197-1200`).
* `supportedUsageFlags` includes `COLOR_ATTACHMENT` + `TRANSFER_DST`, which is exactly what vkd3d
  asks for.
* `supportedCompositeAlpha` = OPAQUE / PRE_MULTIPLIED / POST_MULTIPLIED; vkd3d asks for OPAQUE,
  which the vehicle maps to `DXGI_ALPHA_MODE_IGNORE` (`wsi_common_win32.cpp:990-1000`).

### 4.4 Vehicle-relevant format restriction

`wsi_win32_vehicle_dxgi_format` (`:616-629`) accepts only
`VK_FORMAT_B8G8R8A8_UNORM`, `VK_FORMAT_B8G8R8A8_SRGB` → `DXGI_FORMAT_B8G8R8A8_UNORM`, and
`VK_FORMAT_R8G8B8A8_UNORM` → `DXGI_FORMAT_R8G8B8A8_UNORM`. Anything else ⇒ the chain is
`WSI_VEHICLE_FAILED` at create and falls to the sw GDI path. The three surface formats above are
all covered, so a vkd3d SDR swapchain lands on the vehicle.

### 4.5 The per-present cost that is still in the vehicle path

`wsi_common.c:3697-3716` gives the frame images a shareable export
(`VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT`) when the vehicle is enabled and the chain is
`WSI_SWAPCHAIN_BUFFER_BLIT`, but the sw fallback's per-present image→buffer "insurance blit"
**still runs by default** on vehicle-serving chains — `HELIOS_WSI_INSURANCE_BLIT=0` skips it
(counter `insurance_skipped` in the WSI perf line), recorded as *"default ON until the Doom A/B
numbers land"* (`ROADMAP.md:2895-2900`). For a D3D12 workload this is a known,
measurable, switchable per-frame cost.

---

## 5. The existing vehicles, and the defects a vkd3d client inherits

### 5.1 What exists

| Piece | Path |
|---|---|
| Standalone proof probe (1023 flip presents, dwm composed) | `tools/dcomp_present_probe.cpp`; schtask `helios_dcomp_probe` (ROADMAP `:2391-2400`) |
| Surface-recreate regression probe | `tools/vk_surface_recreate_probe.cpp`; schtask `helios_vk_recreate` (ROADMAP ~`:2870`) |
| ICD half of the vehicle | `icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:229-1108`, `:1960-2300` |
| UMD exports the ICD resolves **by name** | `umd/src/vehicle_exports.rs` (all three MUST keep existing — `vehicle_exports.rs:7-11,63-67`) |
| UMD vehicle body / TLS state machine | `umd/src/forward/vehicle.rs` |
| D4a read-ledger + async present-stream escape plumbing | `umd/src/scanout_acquire.rs:1-41` |

### 5.2 Defects a vkd3d/D3D12 client lands on

**(a) One-dcomp-target-per-HWND — FIXED, and vkd3d was the named suspect.**
ROADMAP `:2854-2860`: *"NEW DEFECT: vehicle re-create for the same hwnd fails
(one-dcomp-target-per-hwnd on resize/fullscreen; vkd3d likely creates a NEW VkSurface for the same
hwnd → per-surface target cache misses → second `CreateTargetForHwnd` fails)"*, hr `0x88980800`.
Fixed in the 28th session (ROADMAP `:2862-2874`, mesa `bbf5e33f314`): a process-global refcounted
hwnd→target registry under the vehicle runtime mutex, with the visual's content owner
(`current_swapchain`) on the shared entry. Code: `wsi_win32_hwnd_comp_acquire_locked`
(`wsi_common_win32.cpp:540-587`), `wsi_win32_hwnd_comp_release` (`:588-615`), the
`surface->vehicle_comp` field (`:679-687`), and the rebind at first successful present
(`:2202-2229`). Proven by `tools/vk_surface_recreate_probe.cpp`.

**(b) The dwm direct/independent-flip promotion (two alternating stale frames) — MITIGATED by a
default, and the mitigation's rationale is now stale.**
ROADMAP `:2786-2800`: *"dwm promotes the eligible dcomp vehicle visual (flip-model + IGNORE-alpha +
unoccluded) to direct/independent flip and STOPS COMPOSING it — two alternating stale frames …"*
Fix: deny `DXGK_DRIVERCAPS.SupportDirectFlip` and the three aperture DirectFlip flags by default
(`kmd_render/src/ddi/query_adapter_info.rs:439-455`; knob `DirectFlipCaps` default 0,
`kmd_render/src/diag.rs:566`, `kmd_render/src/adapter/mod.rs:164`).
⚠ **The comment justifying the denial is written against the retired architecture** — it says
*"Helios has zero scanout (all VidPn DDIs NOT_SUPPORTED; the display is an IddCx driver capturing
dwm's COMPOSED output)"* (`query_adapter_info.rs:441-444`), which is no longer true: Helios now owns
a real VidPn source. The **default is still correct for the vehicle** (an unoccluded fullscreen-sized
D3D12 game is precisely the eligible shape), but the recorded reason no longer matches the stack.
Flag for the implementer: do not "fix" this comment by flipping the default.

**(c) The 0ab black-frame family — the vehicle path is *not* the one those were fixed on.**
0ab-A (`ROADMAP.md:501`), 0ab-B (`:597`), 0ab-C (`:1219`) were all diagnosed and fixed on the
**direct-primary / DWM flip** path — the D2 identity/epoch gate, the D4b snapshot chain
(`kmd_render/src/ddi/present_packet.rs:65-94`, `umd/src/forward/snapshot.rs`). A vehicle present
takes a **different** arm of `dxgi_present` (`present.rs:1297-1308`): it copies into a DXGI
backbuffer and lets DXGI/dcomp own the flip, so the direct-scan-out substitution machinery is not
engaged. Consequence for a vkd3d client: **it does not inherit 0ab, but it also does not inherit the
0ab fixes' evidence base** — the frame-completeness oracle work (`tools/vnc_frame_probe.py`,
`tools/vnc_scanout_correlate.py`) has never been pointed at a vehicle chain in anger.
The vehicle's own analogue is the **copy-vs-rerender torn-frame class**, closed by the acquire-side
release gate (`wsi_win32_vehicle_arm_release_gate`, `wsi_common_win32.cpp:1967-2062`;
`wsi_win32_acquire_gate_vehicle_release`, `:1791-1841`) with the serial
`helios_umd_wait_last_present` fallback (`umd/src/forward/vehicle.rs:138-177`).

**(d) A retired-stub landmine.** `helios_umd_get_present_result` **always returns -1** since R912(a)
(`umd/src/vehicle_exports.rs:56-84`). The ICD's preferred acquire-side gate is therefore **never
armed** in the current build: `wsi_common_win32.cpp:2245-2255` requires
`v->get_result(...) == 0` before `arm_release_gate`, so every vehicle present takes the
`gate_fallbacks` branch and calls the **worker-serial** `wait_last_present` with a 32 ms bound
(`wsi_win32_vehicle_wait_us`, `:388-405`). ROADMAP `:2848-2851` measured that serial flip gate at
**avg 5.57 ms** (Doom, ~105 fps). **This is a live, named, ~5 ms/frame serialization that any
vkd3d D3D12 client would pay.** [CODE + MEAS]

**(e) Fullscreen.** ROADMAP `:2852-2856` measured the fullscreen (1896×1030) chain's vehicle build
**failing** at `stage='dcomp target/visual' hr=0x88980800` and latching to the sw path at
~0.85 ms/frame CPU. That specific failure is (a), now fixed — but the ROADMAP's own note says *"The
honest fullscreen-vehicle Doom A/B is now unblocked (owner run …)"*, and **no post-fix fullscreen
vehicle measurement is recorded anywhere in the tree**. **UNVERIFIED.** *Settling experiment:* run
a fullscreen Vulkan or vkd3d client via a session-1 schtask with `HELIOS_WSI_PERF=1`, read
`creates=/fails=/ready=` in the WSI perf line and `helios_paintcap → Z:\tmp\screen_copy.png`.

**(f) Evidence trap.** A maximized/promoted vehicle window is **absent from GDI-based paintcaps**
(`wsi_common_win32.cpp:852-859`: *"a maximized chain gets promoted to direct/independent flip —
correct on the display, but ABSENT from GDI-based paintcaps: eyeball vehicle windows through
Looking Glass"*). Any D3D12 evidence run must account for this or it will read a working frame as a
black one.

---

## 6. The options

Each option is stated as: **path**, **what must be built**, **defects inherited**, **how it is
proven** (project rule: only owner-visible desktop state counts — `helios_paintcap` →
`Z:\tmp\screen_copy.png`).

---

### (i) Real D3D12 UMD → Microsoft DXGI, reusing the D3D11 present machinery

**Path.** App → MS `dxgi.dll` → MS `d3d12.dll`/`D3D12Core.dll` → `helios_umd!OpenAdapter12` → D3D12
DDI → `pfnPresent` on the command list → runtime issues the kernel present → `DxgkDdiPresent` →
existing flip/BLT arms → `set_scanout_blob`.

**What must exist.**
* The whole D3D12 UMD DDI (`OpenAdapter12` currently refuses — `umd/src/adapter.rs:178-181`).
* A `pfnPresent` implementation that fills `D3D12DDI_PRESENT_0051.BroadcastSrcAllocation[0]` /
  `BroadcastDstAllocation[0]` / `AddedGpuWork` and, when the runtime asks,
  `pfnGetPresentPrivateDriverDataSize` (`d3d12umddi.h:1792`).
* **A replacement for the `HeliosPresentRenderCmd` identity channel.** Today the KMD's flip arm gets
  the per-present venus identity from a Render-command stash written by `pfnRenderCb`
  (`present.rs:829-834`, consumed `display.rs:295-303`). D3D12 has no `pfnRenderCb`. Either
  `D3D12DDIARG_PRESENT_0001.pPrivateDriverData` reaches `DxgkDdiPresent` (**UNVERIFIED**, and the
  D3D11 equivalent measurably did **not** — `display.rs:146-153`), or the identity must ride a
  command in the D3D12 submission itself.
* Engine: DXVK is D3D9/10/11 only. Either vkd3d's `libs/vkd3d` behind the DDI (= option (b) in a
  hat) or a new translator. Out of my lane (R3).

**Defects inherited.** All of the direct-primary family (0ab-A/B/C machinery, snapshot/epoch/lease
bookkeeping), because a D3D12 flip present lands on exactly the same `DxgkDdiPresent` flip arm the
0ab work hardened. That is a *feature*: those fixes are already in place and measured
(GT2 black 3.9-4.1% → 0.02%, memory `d4b-snapshot-chain-closes-gt2-64th`).
**Not** inherited: the vehicle's ~5 ms serial gate, the dcomp target lifetime, the paintcap
blind spot.

**Cost.** Highest by far. Two full DDI frontends and a new engine story.

**Proof.** A D3D12 sample from `dx-samples-research-only/Samples/Desktop/` composing on the desktop,
`helios_paintcap` diff advancing, plus KMD `PBcall`/`PBFlip`/`PBsrc` counters moving this boot.

---

### (ii) vkd3d over Vulkan WSI → guest ICD swapchain → **the dcomp vehicle** → scanout

**This is what actually happens today if vkd3d runs at all**, because vkd3d's only surface source is
`vkCreateWin32SurfaceKHR` (§3.1) and the Helios ICD's Win32 WSI routes that to the vehicle (§4.2).
It is **not** a separate design from (iii) — (iii) is the *deployment* that makes vkd3d able to
create a swapchain at all.

**Path.** app → vkd3d `d3d12.dll` → `IDXGIVkSwapChainFactory::CreateSwapChain` → `VkSurfaceKHR`
(win32) → venus ICD `wsi_common_win32` → vehicle D3D11 device **on Helios** →
`helios_umd!dxgi_present` vehicle arm → `pfnRenderCb`/`pfnPresentCb` → `DxgkDdiPresent` →
`set_scanout_blob`.

**What must exist.** Nothing new in `umd/` or `kmd_render/`. Deployment of vkd3d + a DXGI (option iii).

**Blocking issue.** Risk **V2** in (iii) — CONFIRMED by code — must be fixed first, or this path
silently degrades to the sw GDI blit.

**Defects inherited.** §5.2 (d) — the ~5 ms serial recycle gate, unconditionally, because
`helios_umd_get_present_result` is a permanent `-1`. §5.2 (b) — the direct-flip promotion, mitigated
by a default that must not be flipped. §5.2 (e)/(f) — fullscreen unmeasured, paintcap blind spot.
Plus a structural cost: **two blits per frame** — vkd3d's own user-backbuffer→WSI-image blit
(`swapchain.c:2616`) and the vehicle's WSI-image→DXGI-backbuffer copy
(`vehicle.rs:274-294`) — plus, by default, the sw insurance blit (§4.5). **Three copies of the
frame** unless `HELIOS_WSI_INSURANCE_BLIT=0`.

**Risk.** Also `MAILBOX`/`IMMEDIATE` frames are **dropped** when the latency waitable is unsignaled
(`wsi_common_win32.cpp:2076-2094`) — correct semantics, but it means an fps number from a vkd3d
client on a non-FIFO chain is not a display rate.

**Proof.** Same as (i), plus the WSI perf line
(`HELIOS_WSI_PERF=1` → `helios_vehicle_ready/creates/presents/drops/gate_*`,
`wsi_common_win32.cpp:164-210`) and `C:\ProgramData\Helios\...` vehicle diag lines.

---

### (iii) vkd3d over a **DXVK-provided DXGI** (the Proton model) — the mandatory deployment shape

**This is not optional and it is not an alternative to (ii): without it, vkd3d cannot create a
swapchain at all.** MS `dxgi.dll` does not implement `IDXGIVkSurfaceFactory`, and vkd3d never calls
`vkCreateWin32SurfaceKHR` itself.

**What must exist.** `dxvk-helios` already builds `dxgi.dll` (`src/dxgi/meson.build:27-34`). Deploy
`d3d12.dll` + `d3d12core.dll` (vkd3d) + `dxgi.dll` (DXVK) **app-local**. Verified: `dxgi.dll`,
`d3d11.dll` and `d3d12.dll` are **not** in `HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\KnownDLLs`
on the win11 guest (`win_exec` `Get-ItemProperty` — full list captured, contains only
kernel32/gdi32/user32/ole32/… ), so app-directory redirection genuinely works and no system-wide
override is needed.

**⚠ Risk V2 — CONFIRMED BY CODE. The DXVK `dxgi.dll` this option requires breaks the vehicle.**

The vehicle's process-lifetime runtime resolves DXGI **by bare module name**:

```c
/* icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:485-487 */
   HMODULE d3d11_mod = LoadLibraryA("d3d11.dll");
   HMODULE dxgi_mod  = LoadLibraryA("dxgi.dll");
   HMODULE dcomp_mod = LoadLibraryA("dcomp.dll");
...
/* :497-500 */
   PFN_CREATE_DXGI_FACTORY2 create_factory2 =
      (PFN_CREATE_DXGI_FACTORY2)GetProcAddress(dxgi_mod, "CreateDXGIFactory2");
/* :515 */
   HRESULT hr = create_factory2(0, IID_PPV_ARGS(&rt->factory));   /* IDXGIFactory4 */
```

and later `rt->factory->CreateSwapChainForComposition(v->dev, &desc, NULL, &sc1)`
(`wsi_common_win32.cpp:825-826`). With DXVK's `dxgi.dll` in the executable's directory,
`LoadLibraryA("dxgi.dll")` resolves to it — `dxgi.dll` is **not** a `KnownDLL` on this guest
(verified, `win_exec` read of `HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\KnownDLLs`) —
and:

* DXVK exports `CreateDXGIFactory2` (`dxvk-helios/src/dxgi/dxgi.def:5`) and answers
  `IDXGIFactory4` (`dxvk-helios/src/dxgi/dxgi_factory.cpp:155`; class is
  `DxgiObject<IDXGIFactory7>`, `dxgi_factory.h:55`), so **runtime init succeeds** — no early,
  obvious failure.
* But `DxgiFactory::CreateSwapChainForComposition` is:
  ```cpp
  /* dxvk-helios/src/dxgi/dxgi_factory.cpp:282-298 */
    if (!m_options->enableDummyCompositionSwapchain) {
      Logger::err("DxgiFactory::CreateSwapChainForComposition: Not implemented");
      return E_NOTIMPL;
    }
    Logger::warn("DxgiFactory::CreateSwapChainForComposition: Creating dummy swap chain");
    return CreateSwapChainBase(pDevice, nullptr, pDesc, nullptr, pRestrictToOutput, ppSwapChain);
  ```
  and `dxgi.enableDummyCompositionSwapchain` **defaults to `false`**
  (`dxvk-helios/src/dxgi/dxgi_options.cpp:178`). Even with it on, `CreateSwapChainBase` requires
  `IDXGIVkSwapChainFactory` on the device (`dxgi_factory.cpp:556-575`) — an MS `d3d11.dll` device
  does not have it, so it would return `DXGI_ERROR_UNSUPPORTED`.

**Predicted symptom:** vehicle diag line
`FAILED chain=… stage='CreateSwapChainForComposition' hr=0x80004001`, `helios_vehicle_create_fails`
increments, `v->state = WSI_VEHICLE_FAILED`, and **every vkd3d frame is served by the software GDI
blit** (`wsi_win32_queue_present`, `:2286-2300`). The picture would be correct — which is exactly
what makes this dangerous: it looks like success.

**Fix (small, ICD-local).** In `wsi_win32_vehicle_runtime_init_locked`
(`wsi_common_win32.cpp:479-539`), load DXGI (and d3d11/dcomp) by **full system path** —
`GetSystemDirectoryW` + `LoadLibraryW(L"…\\dxgi.dll")`, or `LoadLibraryExW(..., LOAD_LIBRARY_SEARCH_SYSTEM32)`
— so the vehicle is guaranteed Microsoft's DXGI regardless of what the app directory holds. This is
independently correct hardening: the vehicle deliberately wants the *system* compositor stack, not
whatever the app shipped.

**Risk V1 — still open, and only reachable after V2 is fixed.** The vehicle's first COM step is
`D3D11CreateDevice(NULL, D3D_DRIVER_TYPE_HARDWARE, ..., D3D11_CREATE_DEVICE_BGRA_SUPPORT, ...)`
(`wsi_common_win32.cpp:791-795`) *inside the vkd3d process*. `rt->create_device` comes from
`LoadLibraryA("d3d11.dll")` — MS's, since Helios does not ship DXVK's `d3d11.dll` — but MS
`d3d11.dll`'s own NULL-adapter enumeration links against `dxgi.dll` by import, which app-local
redirection would also point at DXVK. **UNVERIFIED** whether MS `d3d11.dll` accepts a DXVK
`IDXGIAdapter`. The V2 fix does not cover this (it fixes only the *vehicle's* factory, not
`d3d11.dll`'s import). If V1 bites, the vehicle must call
`D3D11CreateDevice(<MS-DXGI adapter>, D3D_DRIVER_TYPE_UNKNOWN, ...)` with an adapter obtained from
the system-path factory it already holds.
*Settling experiment (cheap, no build):* put `dxvk-helios`'s `dxgi.dll` next to the existing
`tools/dcomp_present_probe.cpp` binary and run the `helios_dcomp_probe` schtask (session 1). That
probe already performs the identical `D3D11CreateDevice` → `CreateSwapChainForComposition` → dcomp
sequence and logs each stage's HRESULT, so one run separates V1 from V2.

**Risk V3 — process-wide D3D11 collateral.** Any *other* MS-D3D11 hwnd swapchain in the same
process gets `DXGI_ERROR_UNSUPPORTED` from `CreateSwapChainBase` (`dxgi_factory.cpp:573-575`).
Relevant if a D3D12 title also creates a D3D11 overlay/UI swapchain.

**Defects inherited.** Everything in (ii), plus DXVK-DXGI adapter/output enumeration becoming the
app's view of the display — note ROADMAP `:2400`-era history that
`IDXGIOutput::GetDisplayModeList` on the Helios output was a real blocker for a benchmark
(ROADMAP 31st-session entry).

**Proof.** `helios_paintcap` of a D3D12 sample window advancing; `HELIOS_WSI_PERF` line showing
`ready>=1 fails=0` and `presents` climbing.

---

### (iv) The dcomp vehicle "as an option"

Listed separately in the brief; in reality it is the *mechanism* under (ii)/(iii), not an
alternative. The only genuinely separate variant is **a vkd3d-specific vehicle**: give vkd3d a
Helios-owned `IDXGIVkSurfaceFactory` + a private presenter, bypassing the ICD's WSI. That buys
nothing the ICD's vehicle does not already do, and duplicates a hardened, measured code path.
**Recommend against.**

---

### (v) Interop / shared-texture hand-off: vkd3d renders, a small D3D11 presenter blits

**Path.** vkd3d renders into an `ID3D12Resource` created as a shared/committed resource; a tiny
Helios-side presenter opens it on a D3D11-on-Helios device and presents it through the ordinary
`dxgi_present` path.

**What must exist.** vkd3d's D3D12 shared-handle support over venus external memory
(`VK_KHR_external_memory_win32` is advertised — `guest-vulkaninfo-full.txt:1032` — but the *image*
side and the KMT-handle plumbing are R12's lane), plus a new presenter component, plus a swapchain
abstraction for the app to talk to. **This is re-implementing DXGI.**

**Assessment.** Strictly more work than (iii) for the same destination, and it is what the vehicle
already is, minus the vehicle's proven lifecycle containment. **Recommend against**, but record it:
it is the shape to fall back to if V1 *and* its fallback both fail.

---

### (vi) Give the ICD a real `get_d3d12_command_queue` and use Mesa's native DXGI WSI branch

Mesa's `wsi_common_win32` already contains a **zero-copy** composition path: `WSI_IMAGE_TYPE_DXGI`
images are the D3D12 swapchain's own backbuffers, obtained via `ID3D12Resource` and imported as
`VkImage`s (`wsi_create_dxgi_image_mem`, `wsi_common_win32.cpp:1439-1503`;
`wsi_win32_surface_create_swapchain_dxgi`, `:2374-2446`; present via
`IDXGISwapChain3::Present1` at `:1908` and `visual->SetContent` at `:1919`). It is gated on
`wsi->wsi->win32.get_d3d12_command_queue` (`:2513-2515`), a driver hook venus does not set.
On Helios this is **circular**: the hook needs a working D3D12, which is what we are trying to
build. Include for completeness only. It becomes interesting *after* (i) ships, as a way to give
native-Vulkan clients a zero-copy present.

---

### Option comparison

| | Code to write | Frame copies | Inherits 0ab machinery | Inherits vehicle defects | Blocking unknown |
|---|---|---|---|---|---|
| (i) native D3D12 UMD | Very high (2nd DDI + engine) | 0-1 | yes (good) | no | identity channel for `DxgkDdiPresent`; engine |
| (ii)+(iii) vkd3d + DXVK dxgi | one ~10-line ICD fix (V2) + deploy | 2-3 | no | **yes** (~5 ms gate) | **V2 CONFIRMED** (fixable); **V1** open |
| (iv) vkd3d-specific vehicle | medium | 2-3 | no | yes, re-implemented | none, but no benefit |
| (v) interop presenter | high | 2+ | partial | partial | shared-handle over venus |
| (vi) native DXGI WSI branch | medium, **after (i)** | 0 | n/a | no | needs a D3D12 to exist |

---

## 7. Recommendation

**Do (iii)+(ii) first, as a measurement, before writing a line of D3D12 DDI code** — but land the
V2 fix first, because without it the experiment will *look* like it passed while running on the
software path. The deployment is three DLLs in an app directory; the only tracked-tree change is a
~10-line hardening of the vehicle's module loading; and the entire present path it needs already
exists and is default-ON.

**Confidence: medium-high** that a windowed vkd3d D3D12 swapchain will produce visible, advancing
frames on the Helios desktop *through the vehicle* once V2 is fixed, and **high** that it will
produce visible frames one way or another (the sw GDI path is the always-available floor).
Everything on the Vulkan side checks out against the live guest probe — every required extension
present, surface formats / present modes / usage flags / `minImageCount` all compatible with
vkd3d's exact requests — and the present mechanism is the same one that has run vkcube and Doom.

**The experiment that confirms it, in order:**

1. **Separate V1 from V2, no build.** Drop `dxvk-helios`'s `dxgi.dll` next to the existing
   `helios_dcomp_probe` binary; run the schtask (session 1). Expected on today's code: failure at
   `CreateSwapChainForComposition` with `hr=0x80004001`. A failure *earlier*, at
   `D3D11CreateDevice`, means V1 also bites. A full pass would falsify V2 (e.g. if the probe
   resolves DXGI differently from the ICD).
2. **Land the V2 fix** (system-path module loading in
   `wsi_win32_vehicle_runtime_init_locked`, `wsi_common_win32.cpp:479-539`) and re-run step 1 to
   green.
3. **Build vkd3d-proton for Windows** (R3's lane) and run **one** windowed D3D12 sample from
   `dx-samples-research-only/Samples/Desktop/` (e.g. `D3D12HelloTriangle`) via a session-1 schtask
   with `HELIOS_WSI_PERF=1`, `VKD3D_DEBUG=warn`.
4. **Evidence gate:** `helios_paintcap` → `Z:\tmp\screen_copy.png` twice, ≥2 s apart, showing the
   sample's window with *changing* content. Log lines are not frames. Remember §5.2(f): if the
   window is maximized/promoted it will be absent from a GDI paintcap — keep it windowed and
   partially overlapped, or use Looking Glass.
5. **Corroborate the path taken**, so a sw-GDI fallback is not mistaken for success:
   `C:\ProgramData\Helios\*` vehicle diag must contain `READY chain=… adapter=<Helios>` and
   `LIVE chain=… visual content bound`, the WSI perf line must show `presents` climbing with
   `fails=0`, and the UMD log must show `vehicle present #N` lines
   (`umd/src/forward/present.rs:1560-1571`).
6. **Then** measure the ~5 ms serial gate (§5.2 d) as the first named optimisation target, and only
   then revisit (i).

---

## 8. Corrections this dossier makes to existing docs

| Doc | Claim | Status |
|---|---|---|
| `ROADMAP.md:2385-2387` | *"only the VULKAN client class … lacks a HW present"* | **STALE** (2026-07-06). The dcomp vehicle was built and defaulted ON in the 28th session; `wsi_common_win32.cpp:361-376`. |
| `DX12.md:154-167` | repeats the above as *"the load-bearing unknown"* | **STALE by inheritance.** The load-bearing unknown is Risk V1, not the hand-off. |
| `DX12.md:145-152` (option b) | *"What it requires from the UMD: nothing. `umd/` is not in this path."* | **WRONG.** The vehicle runs `D3D11CreateDevice` on the Helios adapter and every vkd3d frame goes through `helios_umd!dxgi_present`; the three `helios_umd_*` exports are load-bearing (`umd/src/vehicle_exports.rs:7-11`). Also: it requires a **DXVK `dxgi.dll`**, which `DX12.md` never mentions. |
| `ROADMAP.md:2378-2382` | *"that's why DxgkDdiPresent never fires — the blit already happened in the UMD"* | **CONTRADICTED by the current code**: `display.rs` has live BLT (`:431`) and flip (`:773`) arms with per-call counters, and the 0ab-C close-out note (`display.rs:146-153`) records `PBIdOk` readings *taken inside `DxgkDdiPresent`* across three driver generations. *Settling read:* the `PBcall` registry counter after a present workload, confirming it moves this boot. |
| `query_adapter_info.rs:441-444` | *"Helios has zero scanout … the display is an IddCx driver"* (justifying `SupportDirectFlip=0`) | **Rationale stale** (Helios now owns a real VidPn source); the **default is still right** for the vehicle. Do not flip it without re-running the two-stale-frame repro. |

---

## 9. Loose ends I could not close

1. **UNVERIFIED — does MS `d3d11.dll` accept a DXVK `IDXGIAdapter`?** (Risk V1.) *Experiment:* §7 step 1.
2. **CLOSED (was: does DXVK's `CreateSwapChainForComposition` work?).** It returns `E_NOTIMPL`
   unless `dxgi.enableDummyCompositionSwapchain` (default false), and even then routes to
   `CreateSwapChainBase` which requires `IDXGIVkSwapChainFactory`.
   `dxvk-helios/src/dxgi/dxgi_factory.cpp:282-298`, `dxgi_options.cpp:178`. This is Risk V2.
3. **UNVERIFIED — whether the D3D12 runtime issues `D3DKMTPresent` or a redirected variant**, and
   whether `D3D12DDIARG_PRESENT_0001.pPrivateDriverData` reaches `DxgkDdiPresent`.
   *Experiment:* ETW `Microsoft-Windows-DxgKrnl` around a D3D12 sample on a real driver
   (does not need Helios).
4. **UNVERIFIED — post-fix fullscreen vehicle behaviour.** No measurement exists in the tree after
   the hwnd→target registry fix. *Experiment:* §5.2(e).
5. **UNVERIFIED — whether DXVK's `DxgiSwapChain` recreates the `VkSurfaceKHR` on resize/fullscreen.**
   *Settling read:* `dxvk-helios/src/dxgi/dxgi_swapchain.cpp`. Low stakes now that the registry is
   refcounted by HWND.
6. **Not investigated (other lanes):** whether vkd3d-proton's *rendering* requirements are met by
   venus (R12), DXIL/shader (R8), residency/fence semantics (R2/R5/R6), and the separability of
   `libs/vkd3d` (R3). This dossier only asserts that **presentation** is not the blocker.
