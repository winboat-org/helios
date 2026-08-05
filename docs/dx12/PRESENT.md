# PRESENT.md — how a D3D12 frame reaches the Helios scanout

**What this is.** The presentation reference for `docs/dx12/`: the D3D11 present chain as it runs
today hop by hop, the D3D12 `pfnPresent` DDI as the SDK header actually declares it, what the Helios
Venus ICD's Win32 WSI already does for a Vulkan client, how vkd3d-proton presents, and the three
named present problems (**P-A / P-B / P-C**) from `DECISIONS.md` §3-H2 written out with their
evidence, their fixes and their experiments.

**What this is not.** It is not a plan (that is `DX12.md` §4), not the DDI contract in general
(`DDI_REFERENCE.md`), not the vkd3d/venus feature story (`SUBSTRATE.md`), and not the gate list
(`GATES.md`). It does not re-litigate any decision in `DECISIONS.md`; it cites them by id
(D1, D2, D3, D11, H1, H2, P-A, P-B, P-C, S1).

**Header pinning.** Every `d3d12umddi.h` / `d3dumddi.h` / `d3dkmthk.h` / `d3dkmddi.h` line number is
against **Windows SDK 10.0.26100.0**, staged uncommitted at `tmp/dx12/sdk/`. Re-stage with the
`win_exec` snippet in `DECISIONS.md`'s preamble before trusting a citation. Every in-tree citation is
against commit `4739649` (branch `wddm`), submodule `vkd3d-proton-helios` at `2c7ba22c`.

> ## ⚠ Read this before §3 onward — scope changed 2026-08-05
>
> **`DECISIONS.md` D2 (owner directive) removed the app-facing vkd3d arm.** Helios never ships or
> measures vkd3d's `d3d12.dll`/`d3d12core.dll` as an application's D3D12. Consequences for this
> document:
>
> - **The shipping D3D12 present path is: app → MS `dxgi.dll` → the D3D12 runtime →
>   `pfnPresent` (command-list table, plus `D3D12DDI_TABLE_TYPE_DXGI`) → `DxgkDdiPresent` → the
>   existing flip arm → `set_scanout_blob`.** It is the D3D11 chain of §2 with the DDI swapped, and
>   `pKTCallbacks` carries `pfnRenderCb`/`pfnPresentCb` so even the identity channel transfers
>   (P-C).
> - ⛔ **DXVK's `dxgi.dll` is not needed anywhere and is not used anywhere.**
>   `umd/build.rs:238-243`: a WDDM UMD sits *below* DXGI and *implements* the DXGI DDI; MS's
>   `dxgi.dll` is the frontend. DXVK's is not built in this tree (only `dxgi.dll.p` exists), not
>   deployed, not referenced. The same is true for D3D12: the app's `ID3D12CommandQueue` is the
>   **runtime's**, which MS DXGI understands natively, so `IDXGIVkSwapChainFactory` is never queried
>   and vkd3d's `swapchain.c` is never entered.
> - **§5 (how vkd3d presents) and §6 (P-A) are therefore background, not plan.** They describe what
>   would have been required had vkd3d been app-facing. P-A is **closed by construction**, not
>   mitigated — see `DECISIONS.md` §3-H2.
> - **§7 (P-B: the vehicle's ~5.57 ms serial gate and its extra frame copies) is off the D3D12
>   path.** A D3D12 frame never enters the ICD's WSI vehicle. Those costs remain real for **native
>   Vulkan clients** and should be fixed on their own merits; they are not D3D12 numbers.
> - **What survives unchanged and is the load-bearing content here:** §2 (the D3D11 chain, hop by
>   hop — the reference the D3D12 path mirrors), §3 (the `pfnPresent` DDI as declared), §8 (P-C, the
>   identity channel), §10 (the defect classes the flip arm carries), §11 (how to prove a frame, and
>   how to tell which path served it).
>
> ⚠ One thing to carry forward from P-A even though it is closed: the failure *shape* was **a
> correct-looking picture served by a path you did not intend**. §11's requirement to confirm *which
> path served the frame* — not merely that a frame appeared — exists because of it, and applies to
> `D12-G8` verbatim.

**Evidence key.** `[HDR]` the header says it. `[MS]` Microsoft documentation says it. `[CODE]`
in-tree source does it. `[MEAS]` a measurement recorded in this repo. `[INFER]` inference, marked.
**UNVERIFIED** = not established; the settling experiment is always stated inline and repeated in
§12.

---

## 1. The correction of record

⛔ **`ROADMAP.md:2396` is stale and must not be used to reason about D3D12.** The sentence

> *"CONSEQUENCE: only the VULKAN client class (native VK games + vkd3d-proton D3D12) lacks a HW
> present — a Vulkan ICD has no runtime handing it the destination surface; that missing hand-off IS
> the entire gap roads (1)/(2)/(4) exist to fill."*

was written 2026-07-06 (the block runs `ROADMAP.md:2385-2400`). The dcomp vehicle that fills that
gap was built and defaulted **ON** in the 28th session, three weeks later. The old `DX12.md`
inherited the staleness verbatim; the current `DX12.md` no longer does (it schedules "land the P-A
vehicle fix" in phase P0). `DECISIONS.md` §3-H2 records the correction. This document is the long
form.

**What is actually true, with code:**

| Claim | Status | Evidence |
|---|---|---|
| The Helios Venus ICD implements `VK_KHR_surface` + `VK_KHR_win32_surface` + `VK_KHR_swapchain` | **true** | `icd/mesa/src/virtio/vulkan/vn_instance.c:41,59-65`; `vn_physical_device.c:1342`; live guest probe `docs/dx12/research/guest-vulkaninfo-full.txt:32,35,1084` |
| A Vulkan client on Helios gets a **hardware, flip-model, DWM-composited** present | **true, default ON** | `wsi_win32_vehicle_enabled` (`icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:362-374`) returns 1 when `HELIOS_WSI_DCOMP_PRESENT` is unset |
| Those pixels move **GPU-side through venus**, not through a CPU blit | **true** | the vehicle alias-imports the frame by venus resid and does a GPU copy: `umd/src/forward/vehicle.rs:206-292` (`open_texture2d` → `present_vehicle_copy`) |
| That present re-enters **our own D3D11 UMD** | **true** | `helios_umd_set_present_source` (`umd/src/vehicle_exports.rs:25-43`) arms a TLS slot that `dxgi_present` consumes (`umd/src/forward/present.rs:1272-1308`) |
| `DxgkDdiPresent` "never fires" (`ROADMAP.md:2393-2394`) | **false** | `kmd_render/src/ddi/display.rs:179` has live BLT (`:431`) and flip (`:773`) arms with per-call counters; `PBcall` moves every frame |
| D3D12 has no driver present entry point | **false** | `PFND3D12DDI_PRESENT_0051`, `tmp/dx12/sdk/d3d12umddi.h:7250-7251` |

**The load-bearing present problems are P-A, P-B and P-C** (`DECISIONS.md` §3-H2), covered in §6,
§7 and §8 here. None of them is "the hand-off is missing".

---

## 2. The D3D11 present chain today, end to end

This is the reference chain. Every hop is cited; a D3D12 arm either reuses a hop or must replace it,
and §8 is exactly the list of hops that cannot be reused.

### 2.1 One-block summary

```
IDXGISwapChain::Present
  → MS dxgi.dll / d3d11.dll                       (DXGI_DDI_BASE_FUNCTIONS.pfnPresent)
  → helios_umd!dxgi_present                       umd/src/forward/present.rs:1231 → :1239
      ├─ vehicle arm      : alias-import ICD frame by resid, GPU copy into the DXGI backbuffer
      │                     present.rs:1272-1308 → vehicle.rs:187-297
      ├─ direct-flip arm  : NO COPY (this source IS the scanout backing)
      │                     present.rs:1317-1346, presented_primary_private() state.rs:736
      └─ windowed/BLT arm : CopySubresourceRegion(dst ← src) into win32k's redirection surface
                            present.rs:1328-1330
      → context.Flush()          (present.rs:1391 on the direct-flip/BLT arms; :1308 on the vehicle arm)
      → publish_present_order    (folded pre-Flush, present.rs:1370-1390, call at :1383;
                                  else post-Flush, :1425-1436)
      → run_present_frame_gate   (forward.rs:555; skipped only on the async-stream fast path,
                                  present.rs:1479-1528)
      → finish_present           present.rs:1090-1227  — builds DXGIDDICB_PRESENT
      → submit_runtime_present_then_call   present.rs:945-981
           1. pfnRenderCb   ← HeliosPresentRenderCmd + allocation list   ← THE IDENTITY CHANNEL
           2. pfnPresentCb  ← DXGIDDICB_PRESENT { hSrcAllocation, hDstAllocation, hContext }
  → dxgkrnl
  → helios_kmd!DxgkDdiRender    kmd_render/src/ddi/submit_command.rs:992
      → decode HeliosPresentRenderCmd, stash the D4b snapshot descriptor + stream marker
        ON THE CONTEXT (submit_command.rs:1080-1160)
  → helios_kmd!DxgkDdiPresent   kmd_render/src/ddi/display.rs:179 → :217
      → take_snapshot_stash / take_present_stream_marker_stash   display.rs:296-313
      ├─ BLT arm  (flags & 1)      display.rs:431-770  → Venus copy / WindowedBlt snapshot txn
      └─ flip arm (flags & 1<<2)   display.rs:773-917
            ├─ pDmaBuffer == NULL ⇒ MMIO flip: return SUCCESS, dxgkrnl will call
            │                        DxgkDdiSetVidPnSourceAddress   display.rs:833-838
            └─ pDmaBuffer != NULL ⇒ DMA-buffer flip: write PresentFlipPrivate into the
                                     KERNEL-ONLY DMA private data   display.rs:903-912
  → DxgkDdiSubmitCommand[Virtual]  submit_command.rs:725 / :766
      → arm_dma_flip  submit_command.rs:588-620  (PresentFlipPrivate::take, one-shot)
        (or DxgkDdiSetVidPnSourceAddress on the MMIO path, display.rs:1246-1325)
      → arm_dma_flip_programming  display.rs:1346
  → program_vidpn_source  display.rs:2221 → :2258
      → crate::virtio::ctrl::set_scanout_blob(...)   display.rs:2510
  → QEMU (qemu-helios) → egl-headless → VNC
```

### 2.2 App → DXGI → UMD

| # | Hop | Evidence |
|---|-----|----------|
| 1 | `IDXGISwapChain::Present` → MS `dxgi.dll` → `d3d11.dll` → the UMD's DXGI base DDI table | [MS] `windows-driver-docs-pr/display/dxgi-presentation-path.md` |
| 2 | UMD installs `pfnPresent = dxgi_present` into `DXGI_DDI_BASE_FUNCTIONS` | `umd/src/forward/tables.rs:12` (`install_dxgi`), also `:23` / `:28` for the 1.1 / 1.3 tables |
| 3 | `dxgi_present(*mut DXGI_DDI_ARG_PRESENT)` → `dxgi_present_impl` | `umd/src/forward/present.rs:1231` → `:1239` |
| 4 | Args decoded: `hSurfaceToPresent`→`src_h`, `hDstResource`→`dst_h`, both translated to WDDM allocation handles | `present.rs:1254-1260` |
| 5 | `DXGI_DDI_PRESENT_FLAGS`: `Blt`=0x1, `Flip`=0x2, `PreferRight`=0x4, `TemporaryMono`=0x8, `AllowTearing`=0x10, `AllowFlexibleRefresh`=0x20, `NoScanoutTransform`=0x40 | WDK `um\dxgiddi.h:64-81`. UMD reads bit 0 at `present.rs:1347`; KMD reads bit 0 and bit 2 at `display.rs:431,773` |

⚠ The bitfield is read through the raw `u32` representation, deliberately — `present.rs:1250-1252`
states why (bindgen bitfield wrapper, no operator to trust).

### 2.3 The three arms inside `dxgi_present`

Exactly one is taken, selected at `present.rs:1297-1398`:

* **Vehicle arm** — a thread-local `VehicleSlot::Armed` was set by the ICD immediately before this
  `Present()` **on the same thread** (`present.rs:1272-1281`). `vehicle_present_prepare`
  (`vehicle.rs:187-297`) alias-imports the ICD's frame by venus resid (cached, 16 entries,
  `vehicle.rs:206-270`) and GPU-copies it into the DXGI backbuffer (`vehicle.rs:273-292`). A failure
  **fails the present** (`present.rs:1303-1306`) so the ICD latches its sw fallback rather than
  flipping a stale backbuffer.
* **Direct-flip arm** — `presented_primary_private(h, src_h)` (`umd/src/forward/state.rs:736`)
  returns `Some`, i.e. this source *is* the scan-out backing. **No copy** (`present.rs:1320-1321`).
  Optionally records the D4b snapshot blit *before* the Flush (`snapshot_for_present`,
  `umd/src/forward/snapshot.rs:219`, called at `present.rs:1339-1346`).
* **Windowed/BLT arm** — `CopySubresourceRegion(dst ← src)` into win32k's redirection surface
  (`present.rs:1328-1330`), optionally with a `SnapshotPurpose::WindowedBlt` snapshot
  (`present.rs:1347-1360`), then `context.Flush()` at `:1391`.

### 2.4 ⚠ The identity channel is the Render command, not the present private data

**This is the single most important fact in this section for §8.**

`finish_present` builds `DXGIDDICB_PRESENT` and deliberately sets **`PrivateDriverDataSize = 0`**:

```rust
// umd/src/forward/present.rs:1169-1177
    // RETIRED (0ab-C close-out): this used to also ship `present_private` via
    // `cb.pPrivateDriverData`, but dxgkrnl never forwarded it to
    // `DxgkDdiPresent` on the DMA-flip path (the KMD's `PBIdOk` read
    // "no payload" across three driver generations). The ONE per-present
    // channel to the KMD's flip arm is the inline `HeliosPresentRenderCmd`
    // inside `submit_runtime_present_then_call` below — which is where the
    // (possibly snapshot-substituted) `present_private` still goes.
    cb.PrivateDriverDataSize = 0;
    cb.pPrivateDriverData = core::ptr::null_mut();
```

The KMD records the same measured fact at `kmd_render/src/ddi/display.rs:146-153`. `PBIdOk` read
**2 ("no payload") across three driver generations (c5/c6/c7)**. [MEAS]

The channel that *does* work: `submit_runtime_present_then_call` (`present.rs:945-981`) enforces
**`pfnRenderCb` first, then `pfnPresentCb`** — and `pfnRenderCb` carries an inline
`HeliosPresentRenderCmd` written into the runtime's command window:

```rust
// umd/src/forward/present.rs:829-833
            (command as *mut HeliosPresentRenderCmd).write_unaligned(HeliosPresentRenderCmd {
                magic: HELIOS_PRESENT_RENDER_MAGIC,
                version: HELIOS_PRESENT_RENDER_VERSION,
                present: private,
            });
```

`HeliosPresentRenderCmd` is 80 bytes (`protocol/src/wddm.rs:382-440`, static-asserted at `:440`),
magic `'HEPR'` = `0x5250_4548` (`wddm.rs:309`). The allocation list is written by
`RuntimePresentDependencies::write_to` (`present.rs:388-424`): source read-only (`Value = 0`),
destination written (`Value = 1`).

The KMD decodes it in **`DxgkDdiRender`** (`kmd_render/src/ddi/submit_command.rs:992`, decode at
`:1080-1096`, prefix-compatible at 48 / 56 / 72 bytes), and stashes the snapshot descriptor and the
present-stream marker **on the context**:

```rust
// kmd_render/src/ddi/submit_command.rs:1099-1112  (comment)
                // D4b: the RENDER command is the descriptor's DELIVERY ROUTE.
                // dxgkrnl never forwards the UMD's PresentCb private data to
                // DxgkDdiPresent on flip presents (PBIdOk = "no payload"
                // across three driver generations), so a flagged descriptor
                // is STASHED on the context here and taken by the Present
                // that follows it on this same context/thread
```

`ContextHandleRef::stash_snapshot` / `take_snapshot_stash` are at `kmd_render/src/device.rs:129` /
`:152`; the stream-marker twin is `stash_present_stream_marker` (`:175`) /
`take_present_stream_marker_stash` (`:184`). `DxgkDdiPresent` takes both **unconditionally on every
arm** (`display.rs:296-313`) — the clear is the orphan bound.

### 2.5 Kernel: `DxgkDdiPresent` and below

* Entry `kmd_render/src/lib.rs:186` (`data.DxgkDdiPresent = Some(ddi::dxgkddi_present)`);
  `display.rs:179` wraps `display.rs:217` `dxgkddi_present_inner` with a `PRESENT_RETURN = 27`
  timeline write (`display.rs:201-213`, ring `kmd_render/src/ddi/scanout_timeline.rs`, 32 768 slots).
* Payload union arm decoded **once** from the flags — `PresentPayload::decode`
  (`kmd_render/src/ddi/present_packet.rs:594-607`). `FlipWithMultiPlaneOverlay` (bit 12) is refused
  `STATUS_NOT_SUPPORTED`, counter `PBmpo` (`display.rs:254-261`) — the MPO3 KMD interface is not
  registered.
* **BLT arm** (`flags & 1`, `display.rs:431-770`) — validate DMA + patch capacity *before* any host
  GPU work (`validate_patch_capacity`, `present_packet.rs:728-748`), then either the two-phase
  WindowedBlt snapshot transaction (`display.rs:604-666`) or a direct `submit_present_blt` Venus
  copy (`display.rs:675-677`); fence merged into `PresentSubmissionPrivate`
  (`present_packet.rs:299-344`).
* **Flip arm** (`flags & (1<<2)`, `display.rs:773-917`) — two sub-contracts:
  * `pDmaBuffer == NULL` ⇒ **MMIO flip**: `MultipassOffset = 0`, return `STATUS_SUCCESS`, counter
    `PBMmio`; dxgkrnl names the primary later through `DxgkDdiSetVidPnSourceAddress`
    (`display.rs:833-838`).
  * `pDmaBuffer != NULL` ⇒ **DMA-buffer flip**: write `PresentFlipPrivate` (allocation handle,
    `PhysicalAddress`, optional D4b snapshot descriptor) into the kernel-only private data
    (`display.rs:903-912`; struct `present_packet.rs:66-95`; `write` at `:103-156`;
    `PRESENT_DMA_PRIVATE_DATA_BYTES = 88` at `present_packet.rs:37`). dxgkrnl will **not** call
    `SetVidPnSourceAddress` for this flip. ⚠ The design note at `present_packet.rs:42-64` records
    that advertising `FlipImmediateMmIo` to force those flips onto the MMIO path was **measured not
    to work** (2026-07-29): 80 dropped binds, 145/1245 present markers writing the on-screen buffer.
* Then unconditionally: patch list written once (`write_patch_references`,
  `present_packet.rs:765-801`), a `HeliosPresentRefreshCmd` written into the DMA buffer to keep it
  structurally non-empty (`display.rs:936-970`), and the registered stream boundary merged
  (`display.rs:972-983`).
* `DxgkDdiSubmitCommand[Virtual]` consumes the flip record: `arm_dma_flip`
  (`submit_command.rs:588-620`) → `PresentFlipPrivate::take` (`present_packet.rs:170-224`, one-shot:
  it zeroes the magic) → `arm_dma_flip_programming` (`display.rs:1346`).
* MMIO path instead reaches the same state through `dxgkddi_set_vidpn_source_address`
  (`display.rs:1246`), split DIRQL-safe (`:1624`) / PASSIVE continuation (`:1674`, `:1816`).
* `program_vidpn_source` (`display.rs:2221`) → `_inner` (`:2258`) → **`set_scanout_blob`**
  (`display.rs:2510`), carrying `ScanoutSetTimeline { request, present_epoch, carried_watermark,
  flags }`.

⚠ **Everything from `dxgkrnl` down is arm-agnostic.** A D3D12 flip present lands on exactly this
`DxgkDdiPresent` flip arm, this `PresentFlipPrivate`, this `set_scanout_blob`. `DECISIONS.md` §3-H2
P-C says so and it is correct: the reuse boundary is the DDI, not the kernel.

---

## 3. The D3D12 present DDI

### 3.1 `pfnPresent` exists, and it is on the **command-list** table

D3D12 has **no DXGI DDI table**. `pfnPresent` and `pfnBlt` sit next to each other in the 3D
command-list function table. Verified this session by direct grep of `tmp/dx12/sdk/d3d12umddi.h`:

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

`D3D12DDIARG_PRESENT_0001` is **the only** input arg struct — it is shared by all three `pfnPresent`
revisions in this header. Side by side with what the D3D11 UMD reads today:

| `DXGI_DDI_ARG_PRESENT` (D3D11, `dxgiddi.h:84-94`) | `D3D12DDIARG_PRESENT_0001` (`d3d12umddi.h:1630`) | Note |
|---|---|---|
| `hDevice` | — | D3D12 passes `HCOMMANDLIST` + `HCOMMANDQUEUE` as parameters instead |
| `hSurfaceToPresent` (one) + `SrcSubResourceIndex` | `phSurfacesToPresent[]` + `SurfacesToPresent` (array of `{hSurface, SubResourceIndex}`) | ⚠ D3D12 is **multi-surface by construction**; D3D11 needed `Present1` for that |
| `hDstResource`, `DstSubResourceIndex` | `hDstResource`, `DstSubResourceIndex` | same |
| `Flags` (`DXGI_DDI_PRESENT_FLAGS`) | `Flags` (**same type**) | same bit meanings as §2.2 row 5 |
| `FlipInterval` (`DXGI_DDI_FLIP_INTERVAL_TYPE`) | `FlipInterval` (same type) | same |
| — | `VidPnSourceID` (`D3DDDI_VIDEO_PRESENT_SOURCE_ID`) | D3D12 names the VidPn source in the arg |
| `pDXGIContext` | — | no DXGI context object in D3D12 |
| — | `pDirtyRects`, `DirtyRects` | D3D12 hands dirty rects to the driver |
| — | `PrivateDriverDataSize`, `pPrivateDriverData` | see §8 option (i) |
| `bOptimizeForComposition` (on `DXGIDDICB_PRESENT`) | `OptimizeForComposition` (on the **in** arg) | moved from callback to arg |

### 3.2 Three revisions, three out-structs

```c
/* d3d12umddi.h:1646-1657 */
typedef struct D3D12DDI_PRESENT_0003
{
    D3DKMT_HANDLE   hSrcAllocation;
    D3DKMT_HANDLE   hDstAllocation;
    HANDLE          hContext;
    UINT            BroadcastContextCount;
    HANDLE          BroadcastContext[D3DDDI_MAX_BROADCAST_CONTEXT];
    D3DKMT_HANDLE   BroadcastSrcAllocation[D3DDDI_MAX_BROADCAST_CONTEXT];
    D3DKMT_HANDLE   BroadcastDstAllocation[D3DDDI_MAX_BROADCAST_CONTEXT];
    BOOL            AddedGpuWork;
    UINT            BackBufferMultiplicity;
} D3D12DDI_PRESENT_0003;

/* d3d12umddi.h:1791 */
typedef VOID ( APIENTRY* PFND3D12DDI_PRESENT_0003 ) ( D3D12DDI_HCOMMANDLIST, D3D12DDI_HCOMMANDQUEUE,
    _In_ CONST D3D12DDIARG_PRESENT_0001*, _Out_ D3D12DDI_PRESENT_0003* );
```

`D3D12DDI_PRESENT_0028` (`:5828-5843`) = `_0003` plus `SyncIntervalOverrideValid` /
`SyncIntervalOverride`; typedef at `:5844`.

The newest revision splits the out-struct into three:

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

typedef struct D3D12DDI_PRESENT_CONTEXTS_0051
{
    HANDLE          hContext;
    UINT            BroadcastContextCount;
    HANDLE          BroadcastContext[D3DDDI_MAX_BROADCAST_CONTEXT];
} D3D12DDI_PRESENT_CONTEXTS_0051;

typedef struct D3D12DDI_PRESENT_HWQUEUES_0051
{
    UINT            BroadcastQueueCount;
    HANDLE          hHwQueues[D3DDDI_MAX_BROADCAST_CONTEXT+1];
} D3D12DDI_PRESENT_HWQUEUES_0051;

typedef VOID ( APIENTRY* PFND3D12DDI_PRESENT_0051 ) ( D3D12DDI_HCOMMANDLIST, D3D12DDI_HCOMMANDQUEUE,
    _In_ CONST D3D12DDIARG_PRESENT_0001*,
    _Out_ D3D12DDI_PRESENT_0051*, _Out_opt_ D3D12DDI_PRESENT_CONTEXTS_0051*,
    _Out_opt_ D3D12DDI_PRESENT_HWQUEUES_0051* );
```

⚠ **The `_0051` single-present handles moved into `BroadcastSrcAllocation[0]` /
`BroadcastDstAllocation[0]`** — the array grew by one (`D3DDDI_MAX_BROADCAST_CONTEXT+1`, and
`D3DDDI_MAX_BROADCAST_CONTEXT = 64` at `tmp/dx12/sdk/d3dukmdt.h:2072`) and the scalar
`hSrcAllocation`/`hDstAllocation`/`hContext` fields are gone from the primary out-struct.
`hContext` moved to the optional `_CONTEXTS` out-struct. A forwarder that fills only
`[0]` and leaves `BroadcastContextCount = 0` is the single-adapter shape.

**Table placement, verified:** `pfnPresent` appears in exactly **20** command-list function tables
in this header (`grep -c '^ *PFND3D12DDI_PRESENT_[0-9]* *pfnPresent;'` = 20), spanning revisions
0003 / 0028 / 0051. Two anchors: `D3D12DDI_COMMAND_LIST_FUNCS_3D_0003` (`:2999-…`, `pfnBlt` `:3020`,
`pfnPresent` `:3021`) and `D3D12DDI_COMMAND_LIST_FUNCS_3D_0051` (`:7254-…`, `pfnBlt` `:7274`,
`pfnPresent` `:7275`). The table is filled through `pfnFillDDITable` with
`D3D12DDI_TABLE_TYPE_COMMAND_LIST_3D = 1` (`d3d12umddi.h:2491`).

⚠ ⛔ **Honour `pfnFillDDITable`'s `SIZE_T` argument** (`DECISIONS.md` §7.3): never write
`size_of::<T>()` bytes. This is the R702 class.

### 3.3 `pfnGetPresentPrivateDriverDataSize`

```c
/* d3d12umddi.h:1792 */
typedef UINT ( APIENTRY* PFND3D12DDI_GET_PRESENT_PRIVATE_DRIVER_DATA_SIZE )
    ( D3D12DDI_HDEVICE, _In_ CONST D3D12DDIARG_PRESENT_0001* );
```

It is a **device-funcs** slot (33 occurrences across device tables; first at `:3171`), not a
command-list slot, and it is asked *per present arg*. The runtime uses the answer to size the
`pPrivateDriverData` block it then hands back in `D3D12DDIARG_PRESENT_0001`. **Returning 0 is the
correct baseline answer** for Helios until §8 says otherwise: an unread private block is a lie the
runtime pays for on every frame.

### 3.4 ⚠ There is **no** `pfnPresentCb` and **no** `pfnRenderCb` in `d3d12umddi.h`

Verify it yourself before writing a line of present code — this is the fact §8 hangs on:

```bash
grep -c 'DXGIDDICB\|PresentCb\|DXGI_DDI_BASE\|RenderCb' /home/rupansh/helios-vgpu/tmp/dx12/sdk/d3d12umddi.h
# → 0
```

The four D3D12 core-layer callback structs are strictly cumulative — each revision is the previous
one plus a tail. The list below is the **union**, attributed to the revision that introduced each
name; no single struct contains all of it. (`DECISIONS.md` §4.1 owns the live member counts: `_0003`
**12**, `_0022` **14**, `_0050` **17**, `_0062` **18** — the ten `#else void* pfnReserved…` lines
are alternates at identical offsets, not extra members.)

| Struct | Where | Live members | Tail it adds |
|---|---|---:|---|
| `D3D12DDI_CORELAYER_DEVICECALLBACKS_0003` | `:2624-2653` | 12 | `pfnSetErrorCb`, `pfnSetCommandListErrorCb`, `pfnSetCommandListDDITableCb`, `pfnCreateContextCb`, `pfnCreateContextVirtualCb`, `pfnDestroyContextCb`, `pfnCreatePagingQueueCb`, `pfnDestroyPagingQueueCb`, `pfnMakeResidentCb`, `pfnEvictCb`, `pfnReclaimAllocations2Cb`, `pfnOfferAllocationsCb` |
| `_0022` | `:4874-4905` | 14 | `pfnAllocateCb` (`:4903`), `pfnDeallocateCb` (`:4904`) |
| `_0050` | `:7178-7218` | 17 | `pfnCreateSchedulingGroupContextCb` (`:7210`), `pfnCreateSchedulingGroupContextVirtualCb` (`:7211`), `pfnCreateHwQueueCb` (`:7212`) |
| `_0062` | `:8606-8647` | 18 | `pfnQueueBackgroundProcessingWorkCb` (`:8646`) |

**No present callback in any of the four.** [HDR]

⚠⚠ **But the D3D12 UMD still gets the full D3DKMT callback table, and it contains both.**

```c
/* d3d12umddi.h:13618-13636, D3D12DDIARG_CREATEDEVICE_0109 */
    CONST D3DDDI_DEVICECALLBACKS*   pKTCallbacks;  // in: Pointer to runtime callbacks that
                                                   //     invoke kernel
```

and `_D3DDDI_DEVICECALLBACKS` (`tmp/dx12/sdk/d3dumddi.h:4499`) has **`pfnPresentCb` at `:4506`,
`pfnRenderCb` at `:4507`, `pfnSubmitCommandCb` at `:4551`** — 65 entries, the identical table the
D3D11 UMD uses. `pKTCallbacks` also appears on the older `D3D12DDIARG_CREATEDEVICE_0003`
(`d3d12umddi.h:2660`).

So the correct statement is narrower than "no analogue", and `DECISIONS.md` §3-H2 P-C now records
exactly this narrowing as settled (it was amended in the 2026-08-05 verification round; this
document does not overturn it, it expands it):

> The *header-level* D3D12 present surface has no present callback, and the D3D12 runtime — not the
> driver — issues the kernel present. But the **kernel callbacks Helios's identity channel is built
> on (`pfnRenderCb` / `pfnPresentCb` / `pfnSubmitCommandCb`) are handed to the D3D12 UMD
> unchanged.** What changes is the *trigger*, not the *mechanism*. See §8.

**[INFER, high confidence]** Because the driver has no present callback and `pfnPresent` *outputs*
the allocation handles, the context and the HW queues, **the D3D12 runtime issues the kernel
present** (`D3DKMTPresent`, declared `tmp/dx12/sdk/d3dkmthk.h:5929`, arg struct `_D3DKMT_PRESENT` at
`:754`). The driver's job at present time is (a) optionally record GPU work into the given command
list and set `AddedGpuWork`, (b) name the allocations, (c) name the context / HW queue that must be
synchronised against.

> **UNVERIFIED — U1:** that the runtime literally calls `D3DKMTPresent` rather than an internal
> variant such as `D3DKMTPresentRedirected` (`d3dkmthk.h:6078`). *Settling experiment:* the WARP spy
> proxy (`DX12.md` phase P1 / `DECISIONS.md` §3-H1) or an ETW `Microsoft-Windows-DxgKrnl`
> all-keywords trace around a D3D12 sample on a real driver. **Does not block anything** — under
> every variant the KMD still sees `DxgkDdiPresent`.

> **UNVERIFIED — U2:** `D3D12DDI_TABLE_TYPE_DXGI = 3` is declared (`d3d12umddi.h:2493`) but **no
> function-table struct for it exists anywhere in the header** (`grep -n 'D3D12DDI_DXGI'` → 0 hits).
> Whether the runtime ever calls `pfnFillDDITable` with type 3, and with what size, is unknown.
> *Settling experiment:* the WARP spy proxy logs every `(TableType, TableSize)` pair.

### 3.4a ⭐ What the D3D12 *primary* actually is at the DDI — and four rules that come with it

*(2026-08-05, from `ResourceHeaps.md`, DirectX-Specs @ pin `2bd58ca5`. This document previously had
nothing on how a D3D12 back buffer becomes a DXGK allocation; §3.5 picks up downstream of it.)*

**1. `DXGI_DDI_PRIMARY_DESC` is gone; a heap *flag* declares the primary.** *"The existing
`DXGI_DDI_PRIMARY_DESC` is no longer passed to the UMD during heap & resource creation. Instead, two
primary flags are told to the user mode driver at two different points in time"* — a resource
**optimization** flag (which influences the driver's swizzle choice) and a **heap** flag. Receiving the
heap flag obliges the driver to create a resource **simultaneously with the heap**. ⭐ This is the
mechanism behind the measured *"no DXGI table"* result: the same spec says outright that *"The following
DXGI DDIs are not coming forward, and the entire table is deprecated."* — so `D12-G5` seeing
`D3D12DDI_TABLE_TYPE_DXGI` never requested across 20 presents is the **design**, not an artefact of the
workload.

**2. A presentable resource must occupy exactly ONE DXGK allocation** — and the obligation is scoped:
it binds only **committed** resources (heap and resource created together) that are `Texture2D`,
single-mip, non-MSAA, in a flip-model present format, with array size ≤ 2. Other resources may span
more. ⚠ Helios' scanout already assumes one allocation per presentable surface; this is the rule that
makes that assumption correct rather than lucky.

**3. `AllocateCB` must be called inside the create-heap-and-resource DDI, on the entering thread**, and
the UMD must pass **`D3DDDI_ID_UNINITIALIZED`** as `VidPnSourceId` for allocations containing runtime
primaries — the runtime overwrites that field for every `D3DDDI_ALLOCATIONINFO` with `Flags.Primary`
set anyway. The driver is *encouraged* to keep a sentinel of its own in the private data to recognise
D3D12 primaries later.

**4. The back buffer must be in `D3D12_BARRIER_LAYOUT_COMMON` at present time.** `COMMON == _PRESENT ==
0`, and the faster `D3D12_BARRIER_LAYOUT_DIRECT_QUEUE_COMMON` is explicitly **not** a legal present
layout. Under the enhanced-barrier arm (`DDI_REFERENCE.md` §9.10.1) that is a rule `helios_umd12` must
not optimise away.

⚠ **KMD-side, and advisory rather than mandatory:** the spec says the KMD *should* fill
`DXGKARG_DESCRIBEALLOCATION::Rotation` with `D3DDDI_ROTATION_IDENTITY` for D3D12-created managed
primaries, by analogy with the already-unused `RefreshRate`. **"Should", not "must"** — so it is a
candidate `kmd_render` item, not a fifth entry on `DECISIONS.md` D5's list. It costs nothing to honour
and is recorded here so it is not rediscovered at G8.

### 3.5 Swapchain-model consequences

* ⛔ **D3D12 has no BLT-model swapchain.** Only `DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL` and
  `DXGI_SWAP_EFFECT_FLIP_DISCARD` are supported. [MS] `direct3d12/swap-chains`, `DXGI_SWAP_EFFECT`.
  **The entire windowed/BLT arm of §2.3 — `CopySubresourceRegion` into win32k's redirection
  surface — has no D3D12 analogue.** That also removes the `SnapshotPurpose::WindowedBlt` path
  (`present.rs:1347-1360`) from the D3D12 picture.
* **`CreateSwapChainForHwnd` takes a *command queue*, not a device.** [MS] same sources. This is why
  vkd3d-proton hangs `IDXGIVkSwapChainFactory` off `d3d12_command_queue`
  (`vkd3d-proton-helios/libs/vkd3d/command.c:22282-22287`).
* **FLIP_DISCARD / FLIP_SEQUENTIAL windowed:** the backbuffer goes to DWM; there is no driver-side
  src→dst copy, so `hDstResource` will normally be null. **[INFER]** — the header permits a
  destination and nothing states DXGI will not use one. **UNVERIFIED — U3**; *settling experiment:*
  the WARP spy proxy, or instrument `pfnPresent` once a Helios D3D12 UMD exists.
* **Fullscreen exclusive:** `D3D12DDIARG_PRESENT_0001` carries `VidPnSourceID` and `FlipInterval` —
  the same information a D3D11 flip present carries. **No D3D12-specific miniport DDI exists**; the
  scan-out contract below dxgkrnl is unchanged.

**Runtime validations that constrain the out-structs** (from `D3D12Core.dll`'s own strings,
`docs/dx12/research/d3d12core-driverstrings.txt`):

```
:31  Driver didn't provide any HwQueues for a hardware scheduling command queue present.
:55  Driver provided too many contexts for present.
```

plus, recorded by R2 §2.12 from the same binary: `Driver set invalid sync interval override.`
Helios is software-scheduled (no HW queues advertised — `DECISIONS.md` D5), so the first string is
the one a wrong `HwQueues` answer would trip.

---

## 4. The Helios Vulkan WSI and the dcomp vehicle

### 4.1 The ICD forces **software** WSI on Windows, then adds a Helios hook

`vn_wsi_init` (`icd/mesa/src/virtio/vulkan/vn_wsi.c:165-245`):

```c
/* icd/mesa/src/virtio/vulkan/vn_wsi.c:167-186 */
   const bool use_sw_device =
#ifdef _WIN32
      /* Helios: the Mesa ICD still uses wsi_common_win32 GDI/DIB presentation
       * for Vulkan application windows; ... dxgi_get_factory() fails on this guest ->
       * VK_ERROR_INITIALIZATION_FAILED -> vkEnumeratePhysicalDevices returns 0 devices.
       * Force software WSI. */
      true;
```

⚠ **Consequence worth knowing:** because `sw_device = true`, `wsi_win32_init_wsi`'s
`dxgi_get_factory()` / `dcomp_get_device()` branch (`wsi_common_win32.cpp:2652-2666`) is **never
taken**, so `wsi->dxgi.factory` and `wsi->dxgi.dcomp` are NULL for venus. That fact matters twice:
it kills the `WSI_IMAGE_TYPE_DXGI` branch (§4.3), and it means the *only* live `dxgi.dll` load in
the ICD today is the vehicle's (§6).

Then the Helios identity hook:

```c
/* icd/mesa/src/virtio/vulkan/vn_wsi.c:240-241 */
   physical_dev->wsi_device.win32.get_helios_resource_identity =
      vn_wsi_get_helios_resource_identity;
```

which returns `mem->base_bo->res_id` plus the exact `alloc_size` and `memory_type_index`
(`vn_wsi.c:140-161`) — the typed-import identity the UMD needs (vkr's OPAQUE-fd import demands an
exact size/type match).

### 4.2 Extensions actually advertised

| Extension | Code | Live guest evidence |
|---|---|---|
| `VK_KHR_surface` | `vn_instance.c:41` | `guest-vulkaninfo-full.txt:32` (rev 25) |
| `VK_KHR_win32_surface` | `vn_instance.c:59-65` | `:35` (rev 6) |
| `VK_KHR_get_surface_capabilities2` | `vn_instance.c:40` | `:30` |
| `VK_KHR_surface_maintenance1` | `vn_instance.c:42` | `:33` |
| `VK_EXT_surface_maintenance1` | `vn_instance.c:44` | `:23` |
| `VK_KHR_swapchain` | `vn_physical_device.c:1342` | `:1084` (rev 70) |
| `VK_KHR_swapchain_maintenance1` | `vn_physical_device.c:1343` | `:1085` |
| `VK_EXT_swapchain_maintenance1` | `vn_physical_device.c:1346` | `:999` |
| `VK_KHR_incremental_present` | `vn_physical_device.c:1335` | `:1040` |
| `VK_KHR_present_id` / `present_wait` (+`2`) | **`#ifndef VK_USE_PLATFORM_WIN32_KHR`** — `vn_physical_device.c:1336-1341` | **absent** (grep = 0) |

⚠ `VK_KHR_swapchain` is **conditional** on `physical_dev->renderer_sync_fd.semaphore_importable`
(`vn_physical_device.c:1334`). It is true on this box, but a host/renderer change that drops sync-fd
semaphore import silently removes `VK_KHR_swapchain` and **kills `D3D12CreateDevice` outright**
(§5), not just presentation. Worth a gate assertion (`GATES.md`).

### 4.3 Three present paths, one live

`wsi_win32_surface_create_swapchain` (`wsi_common_win32.cpp:2448-2578`) picks the image params at
`:2516-2520`:

```c
   bool supports_dxgi = wsi->dxgi.factory &&
                        wsi->dxgi.dcomp &&
                        wsi->wsi->win32.get_d3d12_command_queue;
```

* **`WSI_IMAGE_TYPE_DXGI`** — **dead on Helios.** Venus sets only
  `get_helios_resource_identity`, never `get_d3d12_command_queue` (`vn_wsi.c:240`), and
  `wsi->dxgi.factory` is NULL anyway (§4.1). This is `dzn`'s branch and a genuine zero-copy
  composition path (`wsi_win32_surface_create_swapchain_dxgi`, `:2374-2446`) — see §9 option (vi).
* **The Helios "dcomp vehicle"** — `WSI_IMAGE_TYPE_CPU` images plus a private D3D11 composition
  swapchain. **Default ON** (`wsi_win32_vehicle_enabled`, `:362-374`; env
  `HELIOS_WSI_DCOMP_PRESENT`, `cached = 1` when unset). **This is the live path.**
* **The sw GDI/DIB blit** — the fallback whenever the vehicle is `INIT` or `FAILED`
  (`wsi_win32_queue_present`, `:2264-2300`).

### 4.4 The vehicle, in full

Design comment `wsi_common_win32.cpp:229-268`; build `:770-928`; start `:957-1108`; present
`wsi_win32_queue_present_vehicle` `:2066-2262` (the one span; §7.1 and §11.2 use the same figures).

1. **`vkCreateSwapchainKHR` → `wsi_win32_vehicle_start`** (`:957`), synchronous, no D3D:
   snapshots `hwnd` / extent / `DXGI_FORMAT` / `buffer_count = MAX2(3, minImageCount)` (`:975-984`),
   maps `compositeAlpha` (OPAQUE/INHERIT → `DXGI_ALPHA_MODE_IGNORE`, `:985-997`), and creates a
   **named exported timeline semaphore** (`:999-1054`):
   ```c
   L"Global\\HeliosPresentFence_%lu_%llu_%u"   /* pid, process start, fence_id */
   ```
   with `VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_BIT` and `GENERIC_ALL`. Failure ⇒
   `WSI_VEHICLE_FAILED`, chain latched to the sw path, counted.
2. **A dedicated worker thread** (`wsi_win32_vehicle_thread`, `:929-955`, named `helios-vehicle`)
   does *all* COM work and then parks until destroy — the COM release and the nested DXVK→ICD2
   teardown it triggers must run on that thread, never on an ICD1 teardown path (`:938-941`).
   `wsi_win32_vehicle_build` (`:769-927`) runs the following in order, each guarded by a `stage`
   string that lands in the failure diag line:
   - `runtime` (`:778`) — `wsi_win32_vehicle_runtime_init_locked` (`:479-535`, the one span; §6.4
     uses the same figures): `LoadLibraryA`
     d3d11/dxgi/dcomp, `CreateDXGIFactory2` → `IDXGIFactory4`, `DCompositionCreateDevice(NULL, …)`.
     ⚠ **This is where P-A bites — §6.**
   - `D3D11CreateDevice` (`:787-796`) — `(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL,
     D3D11_CREATE_DEVICE_BGRA_SUPPORT, …)`. **The default adapter is Helios**, i.e. our own
     `helios_umd.dll`, inside the client's process.
   - `CreateSwapChainForComposition` (`:801-828`) — `SwapEffect = FLIP_SEQUENTIAL`,
     `Scaling = STRETCH`, `BufferUsage = RENDER_TARGET_OUTPUT`, `BufferCount = v->buffer_count`,
     `AlphaMode = v->alpha_mode`, `Flags |= ALLOW_TEARING` when IMMEDIATE.
   - `frame latency waitable` (`:834-845`) — non-FIFO only: `SetMaximumFrameLatency(2)` +
     `GetFrameLatencyWaitableObject()`.
   - `dcomp target/visual` (`:857-869`) — the process-global **refcounted hwnd→target registry**,
     `wsi_win32_hwnd_comp_acquire_locked` (`:540-587`).
   - `helios_umd exports` (`:872-888`) — resolve `helios_umd_set_present_source` /
     `_wait_last_present` / `_get_present_result` **by name**; any miss ⇒ `E_NOINTERFACE` and
     `helios_vehicle_export_miss`++.
   - success ⇒ `READY chain=… hwnd=… WxH fmt=… buffers=… tearing=… adapter=<name>` (`:905-908`).
   - any failure ⇒ `FAILED chain=%p stage='%s' hr=0x%08lx %ux%u fmt=%u` (`:916-918`),
     `helios_vehicle_create_fails`++ (`:919`), `WSI_VEHICLE_FAILED` (`:921`).
3. **Per present — `wsi_win32_queue_present_vehicle`** (`:2066-2262`):
   1. non-FIFO drop check on the latency waitable, *before any side effect* (`:2078-2094`);
      long drop streaks are logged at 64 then every doubling.
   2. resolve the frame's venus resid once per image via `get_helios_resource_identity`
      (`:2100-2122`).
   3. `wsi_helios_present_sync_publish(resid, pid, fence_id, value)` (`:2130-2138`).
   4. `v->set_source(resid, value, w, h, format, alloc_size, mem_type)` (`:2140-2148`).
   5. `v->sc->Present(interval, flags)` — FIFO ⇒ `Present(1)`; IMMEDIATE ⇒
      `Present(0, DXGI_PRESENT_ALLOW_TEARING)`; MAILBOX/other ⇒ `Present(0)` (`:2150-2166`).
      ⚠ SUCCESS-but-not-`S_OK` statuses (`DXGI_STATUS_OCCLUDED` 0x087A0001 …) mean the frame was
      **not displayed** and still pass `FAILED()` — logged on transition with
      `[SUCCESS-STATUS: NOT DISPLAYED]` (`:2172-2188`).
   6. first success binds `comp->visual->SetContent(v->sc)` + `rt->dcomp->Commit()` and logs
      `LIVE chain=… visual content bound (WxH)` (`:2201-2231`; the `LIVE` diag itself is `:2228`).
   7. recycle gate (§7).
4. **That `Present()` re-enters our own D3D11 UMD on the same thread.** `helios_umd_set_present_source`
   (`umd/src/vehicle_exports.rs:25-43` → `umd/src/forward/vehicle.rs:85-132`) armed the TLS slot;
   `dxgi_present` consumes it (`present.rs:1272-1281`), imports the ICD frame by resid and copies it
   into the DXGI backbuffer (`vehicle.rs:187-297`), then runs the ordinary `pfnRenderCb` +
   `pfnPresentCb` path of §2.

⛔ **All three `helios_umd_*` exports must keep existing** — including the permanently-`-1` stub.
`umd/src/vehicle_exports.rs:7-11` and `:63-67` say so; a UMD-only deploy that drops one kills the
vehicle for the whole process with `E_NOINTERFACE`.

### 4.5 Surface capabilities the ICD reports, vs exactly what vkd3d asks for

`wsi_win32_surface_get_capabilities` (`wsi_common_win32.cpp:1172-1224`), formats
`available_surface_formats[]` (`:1300-1306`), present modes (`:1370-1403`):

| Capability | Helios ICD value | vkd3d's ask | Verdict |
|---|---|---|---|
| formats | `B8G8R8A8_UNORM`, `R8G8B8A8_UNORM`, `B8G8R8A8_SRGB` — **all `SRGB_NONLINEAR` only** | exact match, else any of `R8G8B8A8_UNORM` / `B8G8R8A8_UNORM` / `A8B8G8R8_UNORM_PACK32` on `SRGB_NONLINEAR` (`swapchain.c:1742-1802`) | ✅ SDR. ⛔ **HDR is refused** — `dxgi_vk_swap_chain_select_format` returns false for a non-sRGB colour space with no match: *"Refuse to present unsupported HDR since it will look completely bogus."* (`swapchain.c:1798-1801`) |
| present modes | `IMMEDIATE`, `MAILBOX`, `FIFO` (`present_modes_dxgi`, `:1373-1377`) — because the vehicle is enabled (`:1387-1398`). With `HELIOS_WSI_DCOMP_PRESENT=0` it is **FIFO only** (`present_modes_gdi`, `:1370-1372`) | `swap_interval > 0` ⇒ FIFO; `== 0` ⇒ IMMEDIATE, else MAILBOX, else FIFO (`swapchain.c:2187-2196`) | ✅ |
| `minImageCount` / `maxImageCount` | `1` / `0` (`:1183-1195`) | `minImageCount = max(3u, surface_caps.minImageCount)` (`swapchain.c:2211`), clamped to `maxImageCount` when nonzero (`:2222-2223`) | ✅ 3 honoured; the vehicle independently uses `MAX2(3, minImageCount)` (`:984`) |
| `currentExtent` | `GetClientRect(hwnd)` (`:1197-1200`) | `imageExtent = surface_caps.currentExtent`, clamped (`swapchain.c:2225-2229`) | ✅ resize is picked up automatically |
| `supportedUsageFlags` | `wsi_caps_get_image_usage()` (COLOR_ATTACHMENT + TRANSFER_DST + …) (`:1216`) | `COLOR_ATTACHMENT_BIT | TRANSFER_DST_BIT` (`swapchain.c:2204`) | ✅ |
| `supportedCompositeAlpha` | OPAQUE / PRE_MULTIPLIED / POST_MULTIPLIED (`:1211-1214`) | `OPAQUE` (`swapchain.c:2205`) | ✅ → `DXGI_ALPHA_MODE_IGNORE` (`:995`) |

**Vehicle-only format restriction:** `wsi_win32_vehicle_dxgi_format` (`:616-628`) accepts only
`VK_FORMAT_B8G8R8A8_UNORM` / `_SRGB` → `DXGI_FORMAT_B8G8R8A8_UNORM`, and `VK_FORMAT_R8G8B8A8_UNORM`
→ `DXGI_FORMAT_R8G8B8A8_UNORM`. Anything else ⇒ `WSI_VEHICLE_FAILED` at create → sw GDI path. All
three advertised surface formats are covered, so **a vkd3d SDR swapchain lands on the vehicle**.

⚠ `VK_KHR_present_wait` / `present_id` are absent. vkd3d handles that non-fatally:
`chain->present.wait = presentWait || wait2` (`swapchain.c:1493-1495`) and
`FIXME_ONCE("Implementation supports neither present_wait1 or present_wait2. Latency will
increase.")` (`:1498`).

---

## 5. How vkd3d-proton presents

### 5.1 It is a **backend** for someone else's DXGI

vkd3d-proton implements no DXGI and never creates a `VkSurfaceKHR`:

* `libs/vkd3d/command.c:22282-22287` — `d3d12_command_queue_QueryInterface` answers
  `IID_IDXGIVkSwapChainFactory` with `command_queue->vk_swap_chain_factory`.
* `include/vkd3d_swapchain_factory.idl:138-145` —
  `IDXGIVkSwapChainFactory::CreateSwapChain(IDXGIVkSurfaceFactory*, const DXGI_SWAP_CHAIN_DESC1*,
  IDXGIVkSwapChain**)`, uuid `e7d6c3ca-23a0-4e08-9f2f-ea5231df6633` (`idl:138`).
* `libs/vkd3d/swapchain.c:1525-1537` — **the surface is created by the caller**:
  ```c
  vr = IDXGIVkSurfaceFactory_CreateSurface(pFactory, vk_instance, vk_physical_device,
                                           &chain->vk_surface);
  ```
  vkd3d then only checks `vkGetPhysicalDeviceSurfaceSupportKHR` on its queue family (`:1539-1552`).
* `vkd3d-proton-helios/README.md:173-174`: *"vkd3d-proton does not supply the necessary DXGI
  components on its own. Instead, DXVK (2.1+) and vkd3d-proton share a DXGI implementation."*
* `libs/d3d12/d3d12.def` / `libs/d3d12core/d3d12core.def` export **no DXGI entry points**.

**The matching implementation already exists in this tree, and the UUIDs match exactly:**

| Interface | vkd3d (`include/vkd3d_swapchain_factory.idl`) | DXVK (`dxvk-helios/src/dxgi/dxgi_interfaces.h`) |
|---|---|---|
| `IDXGIVkSurfaceFactory` | `1e7895a1-1bc3-4f9c-a670-290a4bc9581a` (`:43`) | `1e7895a1-1bc3-4f9c-a670-290a4bc9581a` (`:59`, `__CRT_UUID_DECL` `:478`) |
| `IDXGIVkSwapChainFactory` | `e7d6c3ca-23a0-4e08-9f2f-ea5231df6633` (`:138`) | `e7d6c3ca-23a0-4e08-9f2f-ea5231df6633` (`:150`, `__CRT_UUID_DECL` `:482`) |

DXVK side:
* `dxvk-helios/src/dxgi/dxgi_factory.cpp:524-579` — `CreateSwapChainBase` does
  `pDevice->QueryInterface(IID_PPV_ARGS(&dxvkFactory))` for `IDXGIVkSwapChainFactory` (`:558`); on
  failure it logs *"DXGI: CreateSwapChainForHwnd: Unsupported device type"* (`:572`) and returns
  `DXGI_ERROR_UNSUPPORTED` (`:573`).
* `dxvk-helios/src/dxgi/dxgi_surface.cpp:42-48` — `DxgiSurfaceFactory::CreateSurface` forwards to
  `wsi::createSurface(m_window, m_vkGetInstanceProcAddr, Instance, pSurface)` — on **the instance
  vkd3d passed in**, not a DXVK instance.
* `dxvk-helios/src/wsi/win32/wsi_window_win32.cpp:341-359` — resolves and calls
  `vkCreateWin32SurfaceKHR`.
* `dxvk-helios/src/dxgi/meson.build:27-34` builds `dxgi.dll`, exporting `CreateDXGIFactory`,
  `CreateDXGIFactory1`, `CreateDXGIFactory2`, … (`src/dxgi/dxgi.def:1-8`).

### 5.2 ⚠ Presentation is a **hard dependency of D3D12 device creation itself**

⚠ **Name the right function.** `D3D12CreateDevice` as an *export* lives in
`vkd3d-proton-helios/libs/d3d12/main.c:143` — the thin `d3d12.dll` target, which the Helios DDI arm
does **not** use (`DECISIONS.md` D4). Inside `d3d12core.dll` the device path is
`d3d12core_CreateDeviceFromFactory` (`libs/d3d12core/main.c:643`), reachable only through
`D3D12GetInterface`, and it is the one that touches DXGI — `CreateDXGIFactory1` at `:383` and
`:406`. That is exactly why D4 adds `helios_vkd3d_create_device` (and
`helios_vkd3d_serialize_root_signature`) rather than calling the stock entry points.

The required-extension arrays are `vkd3d_create_instance_global` (`libs/d3d12core/main.c:569`) at
`:574-580`, and `d3d12core_CreateDeviceFromFactory` at `:659-662`:

```c
static const char * const instance_extensions[] = {
    VK_KHR_SURFACE_EXTENSION_NAME,
#ifdef _WIN32
    VK_KHR_WIN32_SURFACE_EXTENSION_NAME,
#endif
};
...
static const char * const device_extensions[] = { VK_KHR_SWAPCHAIN_EXTENSION_NAME, };
```

`libs/vkd3d/device.c:219-235` (`vkd3d_check_extensions`) only **logs**
`ERR("Required %s extension %s is not supported.")` (`:232-233`) and
still copies every required extension into the enable array (`vkd3d_enable_extensions`, `:331-343`).
A missing one therefore surfaces as `VK_ERROR_EXTENSION_NOT_PRESENT` from
`vkCreateInstance`/`vkCreateDevice` and **D3D12 device creation fails outright** — not "presents
badly". This is why §4.2's ⚠ about `VK_KHR_swapchain` being conditional matters.

### 5.3 Formats, present modes, swapchain creation

`recreate_swapchain_in_present_task` (`swapchain.c:2077-2338`) is where a Vulkan swapchain is built,
lazily, on the present thread:

* format selection `dxgi_vk_swap_chain_select_format` (`:1784-1802`) → `find_surface_format`
  (`:1763-1781`) → `accept_format` (`:1742-1761`), semantics in §4.5.
* present mode: `VKD3D_SWAPCHAIN_PRESENT_MODE` env override (`IMMEDIATE`/`MAILBOX`/`FIFO`/
  `FIFO_RELAXED`/`FIFO_LATEST_READY`), else the default at `:2163-2196`.
* create info (`:2199-2248`): `imageUsage = COLOR_ATTACHMENT | TRANSFER_DST`,
  `imageSharingMode = EXCLUSIVE`, `compositeAlpha = OPAQUE`, `preTransform = IDENTITY`,
  `clipped = VK_TRUE`, **`minImageCount = max(3u, surface_caps.minImageCount)`** (`:2211`),
  **`imageExtent = surface_caps.currentExtent`** clamped (`:2225-2229`) — i.e. vkd3d sizes the
  swapchain **from the surface**, not from `DXGI_SWAP_CHAIN_DESC1`.
* present: `vkAcquireNextImageKHR(…, UINT64_MAX, …)` (`:2814`), an internal blit render pass
  (`record_render_pass` `:2400`, `submit_blit` `:2616`, called `:3016`) copying/scaling the *user*
  backbuffer into the acquired WSI image, then `vkQueuePresentKHR` (`:3112`).
* ⚠ **User backbuffers are ordinary `ID3D12Resource`s vkd3d allocates itself**
  (`allocate_user_buffer` `:756`, `reallocate_user_buffers` `:787`). **There is always a blit between
  the app's backbuffer and the WSI image** — this is copy #1 of §7's count, and it is unavoidable
  in vkd3d's design.
* `IDXGIVkSwapChain2::Present` (`:1121-1240`) does no Vulkan work on the caller's thread: it fills a
  present request in a ring and enqueues `dxgi_vk_swap_chain_present_callback` on the command-queue
  thread (`:1216`).

### 5.4 Resize / fullscreen

* `ChangeProperties` (`:854-910`) reallocates the *user* buffers; the Vulkan swapchain is recreated
  lazily on the present thread (`request_needs_swapchain_recreation` → `recreate_…_in_present_task`,
  `:2077`).
* Because `imageExtent` comes from `surface_caps.currentExtent`, a window resize is picked up from
  `GetClientRect` on the Helios side automatically (§4.5).
* vkd3d does **not** destroy/recreate the `VkSurfaceKHR` on resize — the surface belongs to the
  `IDXGIVkSurfaceFactory` handed in at `CreateSwapChain`, and DXVK's `DxgiSwapChain` owns that.
  **UNVERIFIED — U4:** whether DXVK recreates the surface on a fullscreen transition. *Settling
  read:* `dxvk-helios/src/dxgi/dxgi_swapchain.cpp` fullscreen/resize paths. Low stakes now that the
  hwnd→target registry is refcounted by HWND (§10a).

---

## 6. ⚠ P-A — the confirmed Phase-0 blocker

**`DECISIONS.md` §3-H2 P-A. Confirmed by code on both sides. This must land before any Phase-0
measurement.**

### 6.1 The conflict, in one sentence

vkd3d requires a DXVK `dxgi.dll` in the application directory (§5.1); the Helios ICD's vehicle
resolves DXGI **by bare module name** and calls a method DXVK's DXGI refuses; so deploying
vkd3d + DXVK-DXGI app-local silently demotes every vkd3d frame to the software GDI blit.

### 6.2 Evidence chain

**(a) The vehicle loads DXGI by bare name.**

```c
/* icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:486-488 */
   HMODULE d3d11_mod = LoadLibraryA("d3d11.dll");
   HMODULE dxgi_mod = LoadLibraryA("dxgi.dll");
   HMODULE dcomp_mod = LoadLibraryA("dcomp.dll");
...
/* :498-500 */
   PFN_CREATE_DXGI_FACTORY2 create_factory2 =
      (PFN_CREATE_DXGI_FACTORY2)GetProcAddress(dxgi_mod, "CreateDXGIFactory2");
/* :515 */
   HRESULT hr = create_factory2(0, IID_PPV_ARGS(&rt->factory));   /* IDXGIFactory4 */
```

and later, on the worker thread:

```c
/* icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:825-826 */
      hr = rt->factory->CreateSwapChainForComposition(v->dev, &desc, NULL, &sc1);
```

**(b) `dxgi.dll` is not a `KnownDLL` on this guest**, so app-directory redirection genuinely wins.
Verified by reading `HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\KnownDLLs` — the list
contains only kernel32/gdi32/user32/ole32/… . Re-verify in one line:

```powershell
# win_exec
(Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\KnownDLLs').PSObject.Properties.Name
```

**(c) DXVK's DXGI answers `CreateDXGIFactory2` and `IDXGIFactory4`, so runtime init *succeeds*** —
there is no early, obvious failure. `dxvk-helios/src/dxgi/dxgi.def:5`;
`dxvk-helios/src/dxgi/dxgi_factory.cpp:155`; class is `DxgiObject<IDXGIFactory7>`
(`dxgi_factory.h:55`).

**(d) …and then refuses the one method the vehicle needs:**

```cpp
/* dxvk-helios/src/dxgi/dxgi_factory.cpp:282-298 */
  HRESULT STDMETHODCALLTYPE DxgiFactory::CreateSwapChainForComposition(
          IUnknown*             pDevice,
    const DXGI_SWAP_CHAIN_DESC1* pDesc,
          IDXGIOutput*          pRestrictToOutput,
          IDXGISwapChain1**     ppSwapChain) {
    InitReturnPtr(ppSwapChain);

    if (!m_options->enableDummyCompositionSwapchain) {
      Logger::err("DxgiFactory::CreateSwapChainForComposition: Not implemented");
      return E_NOTIMPL;
    }

    Logger::warn("DxgiFactory::CreateSwapChainForComposition: Creating dummy swap chain");

    return CreateSwapChainBase(pDevice,
      nullptr, pDesc, nullptr, pRestrictToOutput, ppSwapChain);
  }
```

`dxgi.enableDummyCompositionSwapchain` **defaults to `false`**
(`dxvk-helios/src/dxgi/dxgi_options.cpp:178`). And even turned on, `CreateSwapChainBase` requires
`IDXGIVkSwapChainFactory` on the device (`dxgi_factory.cpp:556-574`) — an MS `d3d11.dll` device does
not have it, so it would return `DXGI_ERROR_UNSUPPORTED` (`:573`) instead.

### 6.3 The predicted symptom — and why it is dangerous

```
C:\ProgramData\Helios\helios_icd_diag.log
  <ts> pid=NNNN wsi-vehicle: FAILED chain=0x… stage='CreateSwapChainForComposition'
                             hr=0x80004001 1280x720 fmt=87
```

`helios_vehicle_create_fails`++ (`wsi_common_win32.cpp:919`), `v->state = WSI_VEHICLE_FAILED`
(`:921`), and from then on `wsi_win32_queue_present` (`:2264-2300`) serves **every frame through the
software GDI/DIB blit**.

⚠ **The picture is correct.** A screenshot shows a moving D3D12 triangle. What you measured is the
CPU blit path, not the hardware present — a Phase-0 fps number taken this way is meaningless and a
"D3D12 works on Helios" claim taken this way is false in the way that costs the most later. This is
the exact failure shape `CLAUDE.md` rule 6 exists for: *log lines are not frames, and a correct
picture is not the path you think you measured.*

⚠ Note the demotion is **silent to the screen but loud in the diag** — §11 makes reading that diag
mandatory.

### 6.4 The fix — ICD-local, ~10 lines, in `wsi_win32_vehicle_runtime_init_locked`

**This is `DECISIONS.md` §3-H2 P-A verbatim, not a variation of it.** DECISIONS prescribes two
halves: load by **explicit full System32 path**, *and then verify what you got* with
`GetModuleFileNameW` on the returned `HMODULE`, refusing with a named counter if the result does not
resolve under `%SystemRoot%\System32`. Both halves are required — see the ⛔ below for why the path
alone is not sufficient.

**File:** `icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp`, function
`wsi_win32_vehicle_runtime_init_locked` (`:479-535`). **Replace lines 486-488.**

```c
   /* P-A: the vehicle deliberately wants the SYSTEM compositor stack, not
    * whatever the application directory holds. An app-local DXVK dxgi.dll
    * (required by vkd3d-proton) answers CreateDXGIFactory2 and IDXGIFactory4
    * but returns E_NOTIMPL from CreateSwapChainForComposition, which latches
    * every chain in the process to the software GDI path with a correct-looking
    * picture. Load by FULL SYSTEM PATH so the resolution is deterministic.
    * dxgi.dll is not a KnownDLL on this guest, so a bare-name LoadLibrary is
    * redirectable AND, worse, returns any already-loaded module of that base
    * name regardless of where it came from. */
   WCHAR sysdir[MAX_PATH];
   UINT sysdir_len = GetSystemDirectoryW(sysdir, ARRAY_SIZE(sysdir));
   if (!sysdir_len || sysdir_len >= ARRAY_SIZE(sysdir)) {
      helios_wsi_vehicle_diag("runtime FAILED: GetSystemDirectoryW %lu",
                              (unsigned long)GetLastError());
      return false;
   }
   HMODULE d3d11_mod = wsi_win32_load_system_dll(sysdir, L"d3d11.dll");
   HMODULE dxgi_mod  = wsi_win32_load_system_dll(sysdir, L"dxgi.dll");
   HMODULE dcomp_mod = wsi_win32_load_system_dll(sysdir, L"dcomp.dll");
```

with a small static helper beside it that does **both halves**:

```c
/* Load by full path AND prove what came back. The loader's already-loaded
 * check matches on BASE NAME, so a DXVK dxgi.dll the application mapped
 * first can be handed back no matter how we ask -- the only way to know is
 * to ask the module where it lives. */
static HMODULE
wsi_win32_load_system_dll(const WCHAR *sysdir, const WCHAR *base)
{
   WCHAR full[MAX_PATH];
   if (FAILED(StringCchCopyW(full, ARRAY_SIZE(full), sysdir)) ||
       FAILED(StringCchCatW(full, ARRAY_SIZE(full), L"\\")) ||
       FAILED(StringCchCatW(full, ARRAY_SIZE(full), base)))
      return NULL;

   HMODULE mod = LoadLibraryW(full);
   if (!mod)
      return NULL;

   WCHAR got[MAX_PATH];
   DWORD n = GetModuleFileNameW(mod, got, ARRAY_SIZE(got));
   if (n == 0 || n >= ARRAY_SIZE(got) ||
       _wcsnicmp(got, sysdir, wcslen(sysdir)) != 0) {
      /* Loud, not silent: this is the whole point of the fix. */
      InterlockedIncrement(&helios_vehicle_syslib_hijacked);
      helios_wsi_vehicle_diag("runtime FAILED: %ls resolved to '%ls', not %ls",
                              base, got, sysdir);
      FreeLibrary(mod);
      return NULL;
   }
   return mod;
}
```

⛔ **Neither `LoadLibraryExA("dxgi.dll", NULL, LOAD_LIBRARY_SEARCH_SYSTEM32)` nor a full path alone
is sufficient.** The loader's already-loaded check matches on **base name**, so in a vkd3d process —
where DXVK's `dxgi.dll` is mapped before any swapchain is created — either form can return DXVK's
module. The `GetModuleFileNameW` check is what turns a silent wrong answer into a counted refusal,
and `helios_vehicle_syslib_hijacked` is the named counter CLAUDE.md rule 2 requires for it. [INFER
from documented loader behaviour, adopted as the decision in `DECISIONS.md` §3-H2 P-A.]

⚠ **The §6.6 probe does not test this.** `tools/dcomp_present_probe.cpp` links `-ldxgi`
(`:19`) and reaches `CreateDXGIFactory2` through its **import table** (`:81`); it never calls
`LoadLibraryEx` at all. It can therefore demonstrate import-table redirection (which is what V1 in
§6.5 is about) and **not** the already-loaded-module-wins behaviour of a name-based load. Settling
the loader half needs a probe arm that calls `LoadLibraryExA("dxgi.dll", NULL,
LOAD_LIBRARY_SEARCH_SYSTEM32)` and prints `GetModuleFileNameW` on the result — see **U10** in §12.

**Harden the two latent siblings in the same commit** (they are dead today only because venus forces
`sw_device = true`, §4.1 — a future change to that flag re-arms them):

* `dxgi_get_factory` — `LoadLibraryA("DXGI.DLL")` at `wsi_common_win32.cpp:2583`
* `dcomp_get_device` — `LoadLibraryA("DComp.DLL")` at `wsi_common_win32.cpp:2612`

**Build/deploy:** `win_meson` (see `TOOLCHAIN.md`); the ICD is a Mesa fork build, no KMD or UMD
rebuild is needed, and no reboot — a new process picks up the new ICD.

### 6.5 Risk V1 — still open, and only reachable after P-A is fixed

The vehicle's first COM step is

```c
/* icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:791-795 */
      hr = rt->create_device(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL,
                             D3D11_CREATE_DEVICE_BGRA_SUPPORT, NULL, 0,
                             D3D11_SDK_VERSION, &v->dev, &fl, &v->ctx);
```

*inside the vkd3d process*. `rt->create_device` comes from Microsoft's `d3d11.dll` (Helios ships no
DXVK `d3d11.dll`) — but **MS `d3d11.dll` imports `dxgi.dll` by name**, and import binding resolves
against the already-loaded module list by base name. If DXVK's `dxgi.dll` loaded first, MS
`d3d11.dll`'s NULL-adapter enumeration runs against DXVK's DXGI.

**UNVERIFIED — U5: does MS `d3d11.dll` accept a DXVK `IDXGIAdapter`?** The P-A fix does **not** cover
this: it fixes the *vehicle's own* factory, not `d3d11.dll`'s import table.

**Fallback if V1 bites** (~5 more lines, same function + `wsi_win32_vehicle_build`): obtain an
adapter from the system-path factory the vehicle already holds and call

```c
      hr = rt->create_device(sys_adapter, D3D_DRIVER_TYPE_UNKNOWN, NULL,
                             D3D11_CREATE_DEVICE_BGRA_SUPPORT, NULL, 0,
                             D3D11_SDK_VERSION, &v->dev, &fl, &v->ctx);
```

(`D3D_DRIVER_TYPE_UNKNOWN` is mandatory when an explicit adapter is passed.) Pick the adapter whose
`DXGI_ADAPTER_DESC.Description` matches the Helios one the READY line already prints
(`wsi_common_win32.cpp:891-908`).

**Risk V3 — process-wide D3D11 collateral.** Any *other* MS-D3D11 HWND swapchain in the same process
gets `DXGI_ERROR_UNSUPPORTED` from DXVK's `CreateSwapChainBase` (`dxgi_factory.cpp:573-575`).
Relevant if a D3D12 title also creates a D3D11 overlay/UI swapchain. Not fixable ICD-side; record it.

### 6.6 The cheap experiment that separates V1 from P-A

`tools/dcomp_present_probe.cpp` already performs the *identical* sequence — `D3D11CreateDevice(NULL,
HARDWARE, …, BGRA_SUPPORT)` (`:67-69`) → `CreateDXGIFactory2` → `IDXGIFactory4` (`:81`) →
`CreateSwapChainForComposition` (`:95`) → `DCompositionCreateDevice(NULL, …)` (`:100`) →
`CreateTargetForHwnd`/`SetContent`/`Commit` (`:103-108`) — and its `CHECK` macro prints **every
stage's HRESULT** (`:27-34`). It links `-ldxgi` (`:19`), so **app-local import-table redirection**
reaches it exactly as it reaches MS `d3d11.dll` inside the ICD's process. That is the V1 question of
§6.5; per §6.4 it is *not* the loader question, so do not read a pass here as "P-A is imaginary".

⚠ **It is not a no-build experiment.** The DXVK `dxgi.dll` this run needs does **not** exist in the
tree today — `dxvk-helios/build.w64/src/dxgi/` contains only the object directory `dxgi.dll.p`.
Build it first. `build.w64` is the ready-configured Linux **mingw cross** build dir
(`build-win64.txt`, `x86_64-w64-mingw32-g++`), which is the primary arm per `DECISIONS.md` §6.1, so
this costs one ninja invocation on the Linux host and no VM state at all:

```bash
# Linux host — 0. build DXVK's dxgi.dll (it is a `shared_library` target,
#    dxvk-helios/src/dxgi/meson.build:27-34, built even though the tree
#    otherwise defaults to default_library=static)
ninja -C /home/rupansh/helios-vgpu/dxvk-helios/build.w64 src/dxgi/dxgi.dll
ls -l /home/rupansh/helios-vgpu/dxvk-helios/build.w64/src/dxgi/dxgi.dll   # must exist
```

The guest-side facts, read off the live VM 2026-08-05 (`schtasks /query /tn helios_dcomp_probe /xml`
→ `cmd /c C:\ProgramData\Helios\dcomp_probe.cmd`, whose body is
`C:\Users\Rupansh\helios-probe\dcomp_present_probe.exe 25 > C:\ProgramData\Helios\dcomp_probe_out.txt 2>&1`):

* probe binary — `C:\Users\Rupansh\helios-probe\dcomp_present_probe.exe` (**`helios-probe`**,
  singular; there is no `helios-probes` directory)
* output — `C:\ProgramData\Helios\dcomp_probe_out.txt`
* the task runs as the interactive user (`LogonType=InteractiveToken`), i.e. session 1

```powershell
# win_exec — one run separates V1 from P-A. No driver change, no reboot.
$probeDir = 'C:\Users\Rupansh\helios-probe'
$out      = 'C:\ProgramData\Helios\dcomp_probe_out.txt'

# 1. put DXVK's dxgi.dll BESIDE THE PROBE BINARY (app-local redirection is
#    per-directory: the wrong directory is a silent no-op).
Copy-Item 'Z:\dxvk-helios\build.w64\src\dxgi\dxgi.dll' "$probeDir\dxgi.dll" -Force

# 2. PRECONDITION ASSERT — a missing copy makes the probe pass cleanly and the
#    result table below would read that as "falsifies P-A". Refuse instead.
if (-not (Test-Path "$probeDir\dxgi.dll")) { throw "no app-local dxgi.dll in $probeDir - ABORT" }
Remove-Item $out -ErrorAction SilentlyContinue   # never read a stale run

# 3. run it in SESSION 1 (it has a window; session 0 fakes results)
schtasks /run /tn helios_dcomp_probe

# 4. read the staged HRESULTs
Start-Sleep -Seconds 30; Get-Content $out

# 5. SECOND PRECONDITION — prove the probe process actually mapped OUR copy and
#    not System32's. Run this while the probe's 25 s window is still up.
Get-Process dcomp_present_probe -ErrorAction SilentlyContinue |
  ForEach-Object { $_.Modules } |
  Where-Object { $_.ModuleName -eq 'dxgi.dll' } |
  Select-Object FileName
# must print C:\Users\Rupansh\helios-probe\dxgi.dll — if it prints
# C:\WINDOWS\System32\dxgi.dll the arm never ran; fix step 1 and repeat.
```

| Observed | Meaning |
|---|---|
| Fails at `CreateSwapChainForComposition` with `hr=0x80004001` (`E_NOTIMPL`) | **P-A confirmed empirically, V1 does not bite.** Land §6.4 and re-run to green. |
| Fails *earlier*, at `D3D11CreateDevice` or `GetAdapter` | **V1 also bites.** Land §6.4 **and** the §6.5 fallback. |
| Full pass **and** step 5 printed the app-local path | **Falsifies P-A's app-local half** — the probe resolves DXGI differently from the ICD. Re-read `wsi_common_win32.cpp:486-488` and the probe's import table before believing it. |
| Full pass **and** step 5 printed `System32` | **Nothing was tested.** The copy did not take effect; this is the manufactured false negative the asserts exist to catch. |

⚠ Re-derive the paths if the task is ever recreated — `schtasks /query /tn helios_dcomp_probe /xml`
prints the wrapper, and the wrapper prints the binary and the output file. The probe's own build
recipe is in its header comment (`tools/dcomp_present_probe.cpp:18-19`).

---

## 7. P-B — the costs already in the vehicle path

**`DECISIONS.md` §3-H2 P-B. Read this before quoting any Phase-0 fps number.**

### 7.1 The permanently-`-1` stub forces the worker-serial gate

`helios_umd_get_present_result` has returned `-1` unconditionally since R912(a):

```rust
// umd/src/vehicle_exports.rs:72-84
#[no_mangle]
pub extern "system" fn helios_umd_get_present_result(fence_id: *mut u32, value: *mut u64) -> i32 {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ANNOUNCED: AtomicBool = AtomicBool::new(false);
    if !ANNOUNCED.swap(true, Ordering::Relaxed) {
        log_error!(
            "get_present_result: retired stub (R912a) -- always -1; the ICD's \
             serial wait_last_present is the recycle gate"
        );
    }
    let _ = (fence_id, value);
    -1
}
```

The ICD's preferred **acquire-side** gate requires `get_result(...) == 0`:

```c
/* icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:2245-2256 */
   if (v->get_result(&rel_fence_id, &rel_value) == 0 &&
       rel_fence_id != 0 && rel_value != 0 &&
       wsi_win32_vehicle_arm_release_gate(chain, rel_fence_id)) {
      image->vehicle.release_value = rel_value;
      InterlockedIncrement(&helios_vehicle_gate_arms);
      gated = true;
   }
   if (!gated) {
      InterlockedIncrement(&helios_vehicle_gate_fallbacks);
      if (v->wait_present(wsi_win32_vehicle_wait_us()) != 0)
         InterlockedIncrement(&helios_vehicle_wait_timeouts);
   }
```

So `gated` is **always false**, and every vehicle present takes the **worker-serial**
`helios_umd_wait_last_present` with a 32 ms bound (`wsi_win32_vehicle_wait_us`, `:387-401`;
`umd/src/forward/vehicle.rs:138-177`, which waits `present_frame_gate(timeout_us,
PRESENT_ORDER_COMPLETE)` — GPU **completion**, not submission).

**[MEAS]** `ROADMAP.md:2858-2862` (28th session, "FIX CONFIRMED … WINDOWED DOOM PERF"), Doom pid
9444, 1880×943 windowed, immediate/tearing, ~105 fps. ⚠ Do not cite `:2864-2868` for this table —
that is the *fullscreen sw-path* paragraph and the `hr=0x88980800` defect filing (§10a/§10e):

| Gate | Measured | Timeouts |
|---|---|---|
| flip gate (worker-serial `wait_last_present`) | **avg 5.57 ms** | 0 |
| acquire gate (app thread) | avg 4.06 ms | 0 |
| `queue_present_avg` | 5.96 ms | — |

**⚠ This is a live, named, ~5 ms/frame serialization on the vehicle's worker thread that any vkd3d
D3D12 client pays, unconditionally, today.** It is `gate_fb` in the WSI perf line and it is the
first named optimisation target after Phase 0 proves the path.

⛔ Do **not** "fix" it by reviving the kwait/present-result producer: R912(a) retired that subsystem
because its only producer sat behind a knob that defaulted off (measured `kwait_armed = 0`, misses ==
presents — `ROADMAP` 7g(d)). Rebuilding the acquire-side gate is a design task with its own evidence,
not a stub edit. ⛔ And per `DECISIONS.md` §7.9 (owner directive 2026-07-29) the answer is never a
producer-side CPU present gate.

### 7.2 The insurance blit

```c
/* icd/mesa/src/vulkan/wsi/wsi_common.c:134-142 (comment) */
/* Insurance-blit control (WS2, 28th session): a vehicle-served chain still
 * submits the pre-recorded image->buffer blit with every present so the sw
 * GDI fallback always has fresh bytes — but the vehicle-fail latch is
 * TERMINAL, so the per-frame insurance only ever buys the handful of
 * in-flight frames around one latch, at the cost of a full-frame
 * device->host-visible GPU copy per present (~7 MB at Doom res) ...
 * HELIOS_WSI_INSURANCE_BLIT=0 skips the blit while the vehicle serves
 * (counted; A/B lever, default ON until measured). */
```

Skip decision at `wsi_common.c:2748-2758`; counter `helios_insurance_blits_skipped` reported as
`insurance_skipped=` in the `Helios WSI common` perf line (`wsi_common.c:100-128`).

⚠ **The A/B landed — do not present this as an open, unmeasured saving.** `ROADMAP.md:2909` is the
*original* default (*"default ON until the Doom A/B numbers land"*), but `ROADMAP.md:2919-2926` is
the owner Doom verdict run taken with `insurance=0` — *"no fps change"*, `insurance_skipped
13176/13200` — and `ROADMAP.md:2948-2950` closes it: *"insurance knob keep (no measurable cost
either way at Doom res — the copy hides under GPU latency)"*. [MEAS]

So copy #3 is a real, switchable per-frame GPU copy that was **measured to cost nothing at Doom
resolution (1880×943 windowed / 1896×1030 fullscreen)**. It remains worth re-measuring at D3D12
resolutions and frame sizes before claiming it costs anything *or* that it is free — but the claim
"the numbers never landed" is false and must not be reintroduced (`DECISIONS.md` §6.1).

### 7.3 The copy count per frame

| # | Copy | Where | Avoidable? |
|---|---|---|---|
| 1 | vkd3d user backbuffer → WSI swapchain image (internal blit render pass) | `vkd3d-proton-helios/libs/vkd3d/swapchain.c:2616` / `:3016` | No — structural in vkd3d |
| 2 | WSI image → DXGI backbuffer (the vehicle's GPU copy) | `umd/src/forward/vehicle.rs:273-292` | No, on the vehicle arm |
| 3 | WSI image → host-visible buffer (sw-fallback insurance blit) | `icd/mesa/src/vulkan/wsi/wsi_common.c:2748-2758` | **Yes, but measured inert** — `HELIOS_WSI_INSURANCE_BLIT=0` removes it and cost **no fps** at Doom resolution (`ROADMAP.md:2919-2926`, `:2948-2950`); §7.2 |

**Three copies of every frame by default; two with the knob off — and the third one was measured to
be free at Doom res, so "three copies" is a structural statement, not a cost claim.** A native D3D12
UMD on the direct flip path (§9 option i) does zero or one.

### 7.4 Non-FIFO frames are dropped, so fps ≠ display rate

`wsi_win32_queue_present_vehicle` drops the frame outright when the latency waitable is unsignaled
(`wsi_common_win32.cpp:2078-2094`), incrementing `helios_vehicle_drops`. Correct IMMEDIATE/MAILBOX
semantics — but **an fps number from a vkd3d client on a non-FIFO chain is a render rate, not a
display rate.** Always report `drops=` beside it.

---

## 8. P-C — the identity channel for the DDI arm

**`DECISIONS.md` §3-H2 P-C. This is the DDI arm's real design problem; it does not affect Phase 0.**

### 8.1 The problem, precisely

The KMD's flip arm needs, per present, the **venus identity of the frame being flipped** — a
`HeliosPresentPrivateData` (resource id, width, height, pitch, DXGI format, plane offset,
`venus_alloc_size`, snapshot memory type/purpose, and the present-stream `(ctx_id, value, cookie)`
tail). It uses it for: the D4b snapshot bind-target substitution (`display.rs:876-898`), the
present-stream boundary (`display.rs:972-983`), and the frame-completion watermark.

Today that data reaches the kernel by **one** route, and the other two candidate routes are
*measured dead*:

| Route | Status |
|---|---|
| `DXGIDDICB_PRESENT.pPrivateDriverData` → `DXGKARG_PRESENT.pPrivateDriverData` | ⛔ **dead.** `PBIdOk` read 2 ("no payload") across three driver generations; UMD stopped writing it (`present.rs:1169-1177`, `display.rs:146-153`) [MEAS] |
| The DMA buffer contents at `DxgkDdiPresent` | ⛔ not a channel — dxgkrnl owns the present DMA buffer; the KMD *writes* a `HeliosPresentRefreshCmd` into it (`display.rs:936-970`) rather than reading one |
| **`pfnRenderCb` → `DxgkDdiRender` → per-context stash → `DxgkDdiPresent` takes it** | ✅ **live.** `present.rs:795` (the `pfnRenderCb` resolve) → `:829-833` (the command) → `submit_command.rs:1080-1160` → `display.rs:296-313` |

In D3D12 the *trigger* is gone: the UMD no longer issues the present (§3.4), so there is no
UMD-controlled call that is guaranteed to land immediately before `DxgkDdiPresent` on the same
context — unless the UMD makes one.

### 8.2 The three options

#### (i) `D3D12DDIARG_PRESENT_0001.pPrivateDriverData`

The header gives the D3D12 driver a present private-data block, sized by the driver's own
`pfnGetPresentPrivateDriverDataSize` (`d3d12umddi.h:1792`). Write `HeliosPresentPrivateData` into it
and hope dxgkrnl forwards it to `DXGKARG_PRESENT.pPrivateDriverData`.

⚠ **The D3D11 equivalent measurably did not arrive** (§8.1 row 1). There is no reason on the face of
the header to think the D3D12 plumbing differs, and one reason to think it is the *same* plumbing:
the field ends up in the same `_D3DKMT_PRESENT`/`DXGKARG_PRESENT` structures.

**UNVERIFIED — U6**, but cheaply settleable and worth settling because it would be the smallest
possible implementation. *Settling experiment:* the WARP spy proxy (`DX12.md` P1) answers whether
`pfnGetPresentPrivateDriverDataSize` is even called and with what; the *arrival* half needs a Helios
KMD counter (`PBIdOk` is still in the KMD's vocabulary — re-add the decode behind
`DiagLevel >= 1` and read it once a D3D12 present exists).

**Verdict: try it, do not depend on it.**

#### (ii) ✅ RECOMMENDED — the UMD writes its own `HeliosPresentRenderCmd` via `pfnRenderCb`

**The mechanism transfers verbatim, with NO KMD CHANGE; only the trigger changes.** Per §3.4, the
D3D12 UMD receives the full 65-entry `D3DDDI_DEVICECALLBACKS` through
`D3D12DDIARG_CREATEDEVICE_0109.pKTCallbacks` (`d3d12umddi.h:13623`), and that table contains
`pfnPresentCb` (`d3dumddi.h:4506`), **`pfnRenderCb` (`:4507`)** and `pfnSubmitCommandCb` (`:4551`).
So the D3D12 UMD can write a `HeliosPresentRenderCmd` and call `pfnRenderCb` **exactly as
`umd/src/forward/present.rs:795` does today**, landing in the KMD's **PASSIVE** `dxgkddi_render`
(`kmd_render/src/ddi/submit_command.rs:992`) and its per-context stash. This is `DECISIONS.md`
§3-H2 P-C as amended in the 2026-08-05 verification round, and it is the reason D5's KMD work list
does not grow: **K1/K2/K3 stay three items, none required for the first triangle.**

⛔ **Do NOT design a `DxgkDdiSubmitCommandVirtual` decode for this instead.** An earlier draft of
this section did, and it is wrong on the project's most-repeated invariant:

* `dxgkddi_submit_command_virtual` **runs at DISPATCH_LEVEL** — its own doc comment says so
  (`kmd_render/src/ddi/submit_command.rs:723-724`).
* The `dxgkddi_render` region that would be "reused unchanged" contains four
  `crate::diag::record_named_bytes` calls — `PRset` 0xE1 (`:1103`), `PRset` 0xE2 (`:1167`), `PRsrc`
  (`:1172`) and `PRset` 2 (`:1173`).
* `record_named_bytes` is **PASSIVE_LEVEL-only with no internal IRQL gate**
  (`kmd_render/src/diag.rs:468-469`; module note `:12-13`: *"`RtlWriteRegistryValue` requires
  PASSIVE_LEVEL — only call [`record`] from PASSIVE DDIs (never the DPC/ISR or DISPATCH paging
  paths)"*).

Copying that code into a DISPATCH-level DDI violates the CLAUDE.md invariant *"No pageable code /
`diag::record` (registry writes) above PASSIVE"* — a BSOD/deadlock class this project has already
paid for — and it would add a **fourth** KMD item that `DECISIONS.md` D5 does not have. The
`pfnRenderCb` route has neither problem: it lands at PASSIVE, in code that already exists.

⚠ For the record, had that route been taken it would also have had to validate against
**`DmaBufferUmdPrivateDataSize`** (`tmp/dx12/sdk/d3dkmddi.h:5225`), not `DmaBufferPrivateDataSize`
(`:5224`): the UMD's `D3DDDICB_SUBMITCOMMAND.pPrivateDriverData` (`d3dumddi.h:4006-4007`) occupies
only the *UMD sub-region* of `DXGKARG_SUBMITCOMMANDVIRTUAL.pDmaBufferPrivateData` (`:5223`).
Validating the wrong bound is the RenderGdi-class bug wearing a new hat. Recorded here so the idea
is not re-proposed as if it were untried.

**Shape of the work, in order:**

1. **UMD, `pfnPresent` (command-list table slot).** Immediately *before* filling the out-structs,
   build the `HeliosPresentPrivateData` for the frame from
   `D3D12DDIARG_PRESENT_0001.phSurfacesToPresent[0]`, wrap it in a `HeliosPresentRenderCmd` and
   issue it with `pKTCallbacks->pfnRenderCb` on the queue's WDDM context. Copy the command
   construction from `umd/src/forward/present.rs:829-833` (the `write_unaligned` of magic +
   version + payload), the allocation-list shape from `RuntimePresentDependencies::write_to`
   (`present.rs:388-424` — source read-only `Value = 0`, destination written `Value = 1`), and the
   ordering rule from `submit_runtime_present_then_call` (`present.rs:945-981`): **the Render call
   comes first**.

   ⚠ **The genuinely new work is the resource→identity lookup, and it is not specified anywhere
   yet.** On the D3D11 side `presented_primary_private` (`umd/src/forward/state.rs:736`) resolves
   the venus resid / `venus_alloc_size` / `memory_type_index` / pitch / plane offset out of the
   UMD's own resource table, keyed by the allocation dxgkrnl is presenting. D3D12 hands
   `pfnPresent` a `D3D12DDI_HRESOURCE`, and the D3D12 UMD's equivalent table does not exist yet —
   it has to be built as part of `pfnCreateHeapAndResource` (`DECISIONS.md` §3-H3), because the
   same lookup is what `pfnAllocateCb` needs to author `HeliosWddmAllocPrivate` (§9 option i). Do
   not plan this step as "copy `state.rs:736`"; plan it as "build the D3D12 resource table, then
   copy `state.rs:736`". **UNVERIFIED — U11.**

2. **KMD — nothing.** `dxgkddi_render` already decodes the command (`submit_command.rs:1080-1096`,
   prefix-compatible at 48 / 56 / 72 bytes) and stashes on the context (`:1099-1160`) through
   `ContextHandleRef::stash_snapshot` (`kmd_render/src/device.rs:129`) and
   `stash_present_stream_marker` (`:175`). The magic + version + per-arm length validation the
   `CLAUDE.md` invariant demands is **already there** and already counted — a flagged-but-short
   command falls back through `scanout_trace::note_snapshot_fallback()` (`:1140`, `:1162`).

   ⚠ `PresentFlipPrivate` occupies the *present packet's* kernel-only private data
   (`present_packet.rs:66-95`, `PRESENT_DMA_PRIVATE_DATA_BYTES = 88` at `:37`) — a different buffer
   from the Render command's, owned by dxgkrnl. No aliasing, then or now.

3. **KMD, `display.rs`.** Also nothing: `DxgkDdiPresent` already takes the stash unconditionally on
   every arm (`:296-313`), and the orphan bound (clear-on-take) already covers a Render whose
   present never came.

4. **New counters (CLAUDE.md rule 2).** The route is unchanged but the *caller* is new, so the
   pairing must be observable from the D3D12 arm alone. Add, all in the existing
   `HKLM\SOFTWARE\Helios` namespace and all short — `diag::record_named_bytes` truncates the value
   name at **14** characters (`kmd_render/src/diag.rs:471`):

   | Counter | Site | Meaning |
   |---|---|---|
   | `P12sub` | UMD `pfnPresent`, after a successful `pfnRenderCb` | identity commands issued on the D3D12 path |
   | `P12take` | KMD `DxgkDdiPresent`, when `take_snapshot_stash` returns `Some` on a D3D12 context | identity commands consumed |
   | `P12ref` | UMD `pfnPresent`, every early return that skips the Render call | **the named refusal** — no resource-table entry, `SurfacesToPresent != 1`, unmappable format, `pfnRenderCb` failure |

   `P12sub` == `P12take` is the pairing proof; `P12ref` moving is the loud failure the invariant
   requires instead of a silently identity-less frame. A missing stash is *not* fatal — the KMD
   binds the flipped allocation exactly as today and counts `SnFbk` (`display.rs:882-897`) — which
   is precisely why this option has no new trust boundary.

**Why this is the recommendation:** it is the only option that reuses the *entire* validated
pipeline (`snapshot_bind::validate`, the epoch/lease bookkeeping, `PresentFlipPrivate`,
`set_scanout_blob`) with **zero KMD change and zero new trust boundaries**, and its failure mode is
the one the KMD already handles.

**Two orderings to settle, both cheap:**

* **UNVERIFIED — U7:** that the D3D12 runtime tolerates the driver calling `pfnRenderCb` around
  `pfnPresent` at all, and that `pfnPresent` is invoked before the runtime's own submission on that
  queue with nothing of the runtime's own in between. *Settling experiment (`DECISIONS.md` §3-H2
  P-C):* `pfnRenderCb` + a counting `DxgkDdiRender` on the D3D12 path at `GATES.md` **G7**, before
  G8 depends on it; the WARP spy proxy gives the real call order for free. Live fallback: the
  `P12sub`/`P12take` pair above makes a pairing failure loud rather than silent.
* **UNVERIFIED — U12:** which context-creation callback the D3D12 UMD ends up using. The D3D11 UMD
  creates a **legacy** context (`pfnCreateContextCb`, `umd/src/device_funcs.rs:1053-1061`), which is
  why `DxgkDdiRender` fires for it today; `DECISIONS.md` D5 notes the D3D12 UMD picks its node in
  `D3DDDICB_CREATECONTEXTVIRTUAL`. If the D3D12 arm is forced onto a `VirtualAddressing` context,
  confirm `DxgkDdiRender` still fires for `pfnRenderCb` on it before assuming step 2 is empty. Same
  G7 experiment answers it — `RENDER_COUNT` (`submit_command.rs:996`) moving is the whole test.

#### (iii) A per-context stash keyed by the queue's WDDM context, filled at command-list execute time

Instead of a dedicated submission around `pfnPresent`, piggyback the identity on the *last* command
buffer the UMD submits for that queue before the present. Cheaper by one submission per frame.

⛔ **Recommend against as the first implementation.** It couples the identity to whatever the app
happened to submit last, which is exactly the "identity of the wrong frame" class the D2 identity/
epoch gate (61st session) and the D4b snapshot chain (64th) were built to kill. Keep it as a later
optimisation once (ii) is proven and the extra submission is *measured* to cost something.

### 8.3 Summary

| Option | Reuses KMD unchanged? | New trust boundary | Verdict |
|---|---|---|---|
| (i) `pPrivateDriverData` | yes | none | try, do not depend (U6) |
| **(ii) own `HeliosPresentRenderCmd` via `pfnRenderCb`** | **yes — literally zero KMD lines** | none | ✅ **recommended** (`DECISIONS.md` §3-H2 P-C; D5's list stays at three) |
| (ii′) the same identity via `pfnSubmitCommandCb` → `DxgkDdiSubmitCommandVirtual` | no — a new decode site | **IRQL**: that DDI is DISPATCH_LEVEL | ⛔ **rejected**, see §8.2(ii) |
| (iii) piggyback on the last command buffer | yes | frame/identity pairing | later, if (ii) is measurably expensive |

---

## 9. The options — every route from a D3D12 frame to the scanout

Project rule (`DECISIONS.md` §7.11): **only owner-visible desktop state counts as proof** —
`helios_paintcap` → `Z:\tmp\screen_copy.png`.

### (i) Native D3D12 UMD → Microsoft DXGI → the existing D3D11 present machinery

**Path.** app → MS `dxgi.dll` → MS `d3d12.dll`/`D3D12Core.dll` → `helios_umd12!OpenAdapter12` →
D3D12 DDI → `pfnPresent` on the command list → runtime issues the kernel present →
`DxgkDdiPresent` → existing flip arm → `set_scanout_blob`.

**Must be built.** The D3D12 UMD (`DECISIONS.md` D1/D3 — today `OpenAdapter12` refuses at
`umd/src/adapter.rs:178-190`); a `pfnPresent` filling `D3D12DDI_PRESENT_0051.BroadcastSrcAllocation[0]`
/ `BroadcastDstAllocation[0]` / `AddedGpuWork` (+ `_CONTEXTS_0051.hContext`);
`pfnGetPresentPrivateDriverDataSize`; **the §8 identity channel**; and — a KMD-side prerequisite from
`R5-kmd-gap.md` W1 — the D3D12 UMD must author `HeliosWddmAllocPrivate` on every `pfnAllocateCb`,
because `DxgkDdiCreateAllocation` returns `STATUS_INVALID_PARAMETER` for anything else
(`kmd_render/src/ddi/create_allocation.rs:2291-2307`; `protocol/src/wddm.rs:102-151`).

**Defects inherited.** The whole direct-primary family — 0ab-A/B/C machinery, snapshot/epoch/lease
bookkeeping — because a D3D12 flip present lands on exactly the `DxgkDdiPresent` flip arm that work
hardened. **That is a feature:** those fixes are in place and measured (GT2 black 3.9-4.1% → 0.02%).
**Not** inherited: the vehicle's ~5 ms serial gate, the dcomp target lifetime, the paintcap blind
spot.

**Frame copies.** 0 (direct flip) to 1.

**Blocking unknowns.** §8 (U6/U7/U11/U12); the engine story is D1's answer (vkd3d behind the DDI).
⚠ Note what is **not** here: a KMD change. Per §8.2(ii) the identity channel is `pfnRenderCb` into
the existing PASSIVE `dxgkddi_render`, so `DECISIONS.md` D5's KMD list stays at K1/K2/K3, none of
them required for the first triangle.

**Proof.** A D3D12 sample composing on the desktop, `helios_paintcap` diff advancing, KMD
`PBcall`/`PBFlip`/`PBsrc` counters moving **this boot**.

### (ii) vkd3d over Vulkan WSI → the dcomp vehicle → scanout

**This is what happens today if vkd3d runs at all**, because vkd3d's only surface source is
`vkCreateWin32SurfaceKHR` (§5.1) and the Helios ICD routes that to the vehicle (§4.3). It is **not**
separate from (iii): (iii) is the deployment that lets vkd3d create a swapchain at all.

**Path.** app → vkd3d `d3d12.dll` → `IDXGIVkSwapChainFactory::CreateSwapChain` → DXVK
`IDXGIVkSurfaceFactory` → `vkCreateWin32SurfaceKHR` → venus ICD `wsi_common_win32` → vehicle D3D11
device **on Helios** → `helios_umd!dxgi_present` vehicle arm → `pfnRenderCb`/`pfnPresentCb` →
`DxgkDdiPresent` → `set_scanout_blob`.

**Must be built.** Nothing in `umd/` or `kmd_render/`. Deployment (iii) plus the §6.4 ICD fix.

**Defects inherited.** §10 (b), (d), (e), (f). **Frame copies: 3** by default, 2 with
`HELIOS_WSI_INSURANCE_BLIT=0` (§7.3).

**Proof.** §11.

### (iii) vkd3d over a **DXVK-provided DXGI** (the Proton model) — the mandatory deployment shape

**Not optional.** MS `dxgi.dll` does not implement `IDXGIVkSurfaceFactory`, and vkd3d never calls
`vkCreateWin32SurfaceKHR` itself.

**Must be built** — and it is **not** built in the tree today. `dxvk-helios` *declares* `dxgi.dll` as
a `shared_library` target (`src/dxgi/meson.build:27-34`), but
`dxvk-helios/build.w64/src/dxgi/` currently holds only the object directory `dxgi.dll.p`. Two
commands, both on the Linux host, both against already-configured build dirs
(`DECISIONS.md` §6.1: the mingw cross-build is the primary arm):

```bash
# DXVK dxgi.dll — the IDXGIVkSurfaceFactory/IDXGIVkSwapChainFactory provider
ninja -C /home/rupansh/helios-vgpu/dxvk-helios/build.w64 src/dxgi/dxgi.dll
#   -> dxvk-helios/build.w64/src/dxgi/dxgi.dll   (= Z:\dxvk-helios\build.w64\src\dxgi\dxgi.dll)

# vkd3d d3d12.dll + d3d12core.dll — the full recipe is GATES.md §4.1 (D12-G0);
# run that gate, do not improvise a second build dir.
```

⚠ `win_dxvk` is **not** the tool for this DLL: it mirrors to `C:\Users\Rupansh\dxvk-helios` and
builds into `C:\Users\Rupansh\dxvk-build` with clang-cl, for the UMD's static archives. Use it only
if you specifically want an MSVC-ABI `dxgi.dll`, in which case the artifact is
`C:\Users\Rupansh\dxvk-build\src\dxgi\dxgi.dll`.

Deploy `d3d12.dll` + `d3d12core.dll` (vkd3d) + `dxgi.dll` (DXVK) **app-local**; app-directory
redirection genuinely works because none of the three is a `KnownDLL` on this guest (§6.2b).

**⚠ Blocking issue: P-A (§6) — CONFIRMED. Land the fix first or the measurement is a lie.**
**Open: V1 (§6.5), V3.**

**Defects inherited.** Everything in (ii), plus DXVK-DXGI adapter/output enumeration becoming the
app's view of the display — note that `IDXGIOutput::GetDisplayModeList` on the Helios output was a
real blocker for a benchmark in the 31st session (`ROADMAP.md` 31st-session entry).

### (iv) A vkd3d-specific vehicle (Helios-owned `IDXGIVkSurfaceFactory` + private presenter)

⛔ **Recommend against.** It buys nothing the ICD's vehicle does not already do and duplicates a
hardened, measured code path — including the hwnd→target registry, the drop semantics and the
recycle gate.

### (v) Interop / shared-texture hand-off: vkd3d renders, a small D3D11 presenter blits

vkd3d renders into a shared `ID3D12Resource`; a Helios-side presenter opens it on a D3D11-on-Helios
device and presents it through `dxgi_present`.

⛔ **Recommend against**, but record it. It needs D3D12 shared-handle support over venus external
memory — and `DECISIONS.md` §2 **S1** says `VK_KHR_external_memory_win32` is *absent* while
vkd3d chains `VkExportMemoryAllocateInfo` unguarded (`libs/vkd3d/resource.c:4405-4429`), so shared
heaps are hazardous, not merely degraded — plus a new presenter component **plus a swapchain
abstraction for the app**, i.e. re-implementing DXGI. Strictly more work than (iii) for the same
destination. It is the shape to fall back to only if V1 *and* its fallback both fail.

### (vi) Give the ICD a real `get_d3d12_command_queue` and use Mesa's native DXGI WSI branch

Mesa's `wsi_common_win32` already has a **zero-copy** composition path: `WSI_IMAGE_TYPE_DXGI` images
are the D3D12 swapchain's own backbuffers imported as `VkImage`s (`wsi_create_dxgi_image_mem`,
`wsi_common_win32.cpp:1439-1503`; `wsi_win32_surface_create_swapchain_dxgi`, `:2374-2446`; present
via `IDXGISwapChain3::Present1` `:1908` + `visual->SetContent` `:1919`). Gated on
`wsi->wsi->win32.get_d3d12_command_queue` (`:2518`), a hook venus does not set — **and it is
circular**: the hook needs a working D3D12. Interesting *after* (i) ships, as a way to give
native-Vulkan clients a zero-copy present.

### Comparison

| | Code to write | Frame copies | Inherits 0ab machinery | Inherits vehicle defects | Blocking unknown |
|---|---|---|---|---|---|
| (i) native D3D12 UMD | very high (2nd DDI + engine) | 0-1 | **yes (good)** | no | §8 identity channel (U6/U7/U11/U12) — but **no KMD change**, §8.2(ii) |
| (ii)+(iii) vkd3d + DXVK dxgi | one ~10-line ICD fix (§6.4) + deploy | **2-3** | no | **yes** (~5 ms gate) | **P-A CONFIRMED** (fixable); **V1** open (U5) |
| (iv) vkd3d-specific vehicle | medium | 2-3 | no | yes, re-implemented | none, but no benefit ⛔ |
| (v) interop presenter | high | 2+ | partial | partial | shared handles over venus (S1) ⛔ |
| (vi) native DXGI WSI branch | medium, **after (i)** | **0** | n/a | no | needs a D3D12 to exist |

**Recommendation, unchanged from `DECISIONS.md` D2:** do (iii)+(ii) first as a *measurement*, with
the §6.4 fix landed first, before writing a line of D3D12 DDI code — then (i), reusing everything
below dxgkrnl and rebuilding only the identity channel per §8.

---

## 10. Defects a D3D12 client inherits

**(a) One-dcomp-target-per-HWND — FIXED, and vkd3d was the named suspect.**
`ROADMAP.md:2866-2869` filed it: *"NEW DEFECT: vehicle re-create for the same hwnd fails … vkd3d
likely creates a NEW VkSurface for the same hwnd → per-surface target cache misses → second
`CreateTargetForHwnd` fails"*, `hr = 0x88980800`. Fixed in the 28th session
(`ROADMAP.md:2874-2892`, mesa `bbf5e33f314`):
a process-global refcounted hwnd→target registry under the vehicle runtime mutex, with the visual's
content owner (`current_swapchain`) on the shared entry. Code: `struct wsi_win32_hwnd_comp`
(`wsi_common_win32.cpp:434-441`), `wsi_win32_hwnd_comp_acquire_locked` (`:540-587`),
`wsi_win32_hwnd_comp_release` (`:588-615`), `surface->vehicle_comp` (`:857-869`), rebind at first
successful present (`:2202-2231`). Proven by `tools/vk_surface_recreate_probe.cpp` (schtask
`helios_vk_recreate`). Counter `tgt_reuse` in the WSI perf line. **Status: closed.**

**(b) dwm direct/independent-flip promotion (two alternating stale frames) — MITIGATED by a default
that must not be flipped.**
`ROADMAP.md:2786-2800`: dwm promotes the eligible dcomp vehicle visual (flip-model + IGNORE-alpha +
unoccluded) to direct/independent flip and **stops composing it**. Mitigation: deny
`DXGK_DRIVERCAPS.SupportDirectFlip` by default (`kmd_render/src/ddi/query_adapter_info.rs:439-455`;
knob `DirectFlipCaps` default 0, `kmd_render/src/diag.rs:566`, `kmd_render/src/adapter/mod.rs:164`);
the UMD independently denies `CheckDirectFlipSupport` unconditionally
(`umd/src/forward/transfer.rs:369-380`, installed `tables.rs:267`).

⚠ **The comment justifying the denial is written against the retired architecture.** It says

```rust
// kmd_render/src/ddi/query_adapter_info.rs:442-445
    // a LIE: Helios has zero scanout (all VidPn DDIs NOT_SUPPORTED; the display
    // is an IddCx driver capturing dwm's COMPOSED output), so a dwm direct/
    // independent-flip promotion of an eligible visual (flip-model + IGNORE-alpha
    // + unoccluded — exactly the dcomp vehicle chain) makes dwm STOP COMPOSING it
```

Helios now owns a real VidPn source, so the *premise* is false. ⛔ **The default is still correct
for the vehicle** — an unoccluded, fullscreen-sized D3D12 game is precisely the eligible shape — so
do **not** "fix" this comment by flipping the default. Rewriting the comment to state the true
current reason (the vehicle's promotion, not the absent scanout) is a welcome separate change; the
opposite value stays reachable as the A/B (`reg add … /v DirectFlipCaps /d 1` + device restart,
`adapter/mod.rs:234`).

**(c) The 0ab black-frame family — the vehicle arm does NOT inherit it, and does not inherit its
evidence base either.**
0ab-A (`ROADMAP.md:512`, *"### Defect 0ab — 0ab-A FIXED 2026-07-29, 0ab-B STILL OPEN"*), 0ab-B
(`:608`, *"### 0ab-B — at ~180 fps the flashes REMAIN (OPEN)"*), 0ab-C (`:1230`, *"### 0ab-C —
CLASSIFIED 2026-07-29/30 night…"*) were diagnosed and fixed on the
**direct-primary / DWM flip** path — the D2 identity/epoch gate and the D4b snapshot chain
(`kmd_render/src/ddi/present_packet.rs:66-95`; `umd/src/forward/snapshot.rs`). A vehicle present takes
a *different* arm of `dxgi_present` (`present.rs:1297-1308`): it copies into a DXGI backbuffer and
lets DXGI/dcomp own the flip, so the direct-scan-out substitution machinery is never engaged.

⚠ Consequence for a vkd3d client: **it does not inherit 0ab, but it also does not inherit the 0ab
fixes' evidence base.** The frame-completeness oracle work (`tools/vnc_frame_probe.py`,
`tools/vnc_scanout_correlate.py`) has never been pointed at a vehicle chain in anger. The vehicle's
own analogue is the copy-vs-rerender torn-frame class, closed by the acquire-side release gate
(`wsi_win32_vehicle_arm_release_gate`, `wsi_common_win32.cpp:1967-2062`;
`wsi_win32_acquire_gate_vehicle_release`, `:1792-1841`) with the serial `wait_last_present`
fallback — which, per §7.1, is the branch that always runs.

⚠ Conversely: option (i) **does** inherit the 0ab machinery, and that is a *benefit* — but it also
inherits the obligation to keep the D4b snapshot chain fed, which is exactly §8.

**(d) The permanently-`-1` stub** — §7.1. Status: live, named, unfixed by design (R912a).

**(e) Fullscreen vehicle — MEASURED post-fix, but on a knob that no longer exists.**
`ROADMAP.md:2862-2866` measured the *pre-fix* fullscreen (1896×1030) chain's vehicle build
**failing** at `stage='dcomp target/visual' hr=0x88980800` and latching to the sw path at
~0.85 ms/frame CPU. That failure is (a), now fixed, and `ROADMAP.md:2888-2890` predicted the re-run
(*"The honest fullscreen-vehicle Doom A/B is now unblocked … expect creates=2 fails=0 and the
fullscreen chain LIVE instead of the 0x88980800 latch"*).

⚠ **That re-run happened.** `ROADMAP.md:2919-2931` is the owner Doom verdict (same process,
windowed→fullscreen): *"the fullscreen 1896x1030 chain went VEHICLE (READY+LIVE on the same hwnd as
the windowed chain — the target-registry fix confirmed in the wild)"*, with `queue_present_avg`
5.96→2.81 ms and the acquire gate 4.06→7.69 ms (max ~20 ms, 0 timeouts). The conclusion recorded
there is that the fps limiter is the vehicle copy's completion+observation latency, not a CPU gate.
[MEAS] **Do not write "no post-fix fullscreen measurement exists" — it does** (`DECISIONS.md` §6.1).

The **actual** open item is narrower: that run was taken with `VehicleKernelFlipWait=1`
(`ROADMAP.md:2919-2920`, *"kwait=1 + insurance=0"*), and R912(a) has since retired the kwait
producer (§7.1). So the numbers describe a configuration the shipping gate path no longer has.
**UNVERIFIED — U8** (narrowed); *settling experiment:* re-run §11.1 with a fullscreen client on the
shipping defaults, reading `creates=/fails=/ready=/gate_arms=/gate_fb=` in the WSI perf line, and
compare against the 2.81 ms / 7.69 ms pair above.

**(f) ⚠ The paintcap blind spot — the evidence trap that will make you call a working frame black.**

```c
/* icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:852-859 (comment) */
 * Verified live 23rd session (windowed
 * vkcube composes; a maximized chain gets promoted to direct/
 * independent flip — correct on the display, but ABSENT from GDI-based
 * paintcaps: eyeball vehicle windows through Looking Glass).
```

A maximized or promoted vehicle window is **absent from `helios_paintcap`**. Any D3D12 evidence run
must keep the window **windowed and partially overlapped**, or use Looking Glass / the VNC path, or
it will read a working frame as a black one. This is the mirror image of (b): the same promotion
that (b)'s default suppresses for the *desktop* still happens for a maximized composition visual.

---

## 11. How to prove a D3D12 frame

⛔ **Log lines are not frames** (`CLAUDE.md` rule 6, `DECISIONS.md` §7.11). ⛔ **Anything with a
window runs in session 1** via a scheduled task — `win_exec`/SSH land in session 0 and a session-0
run fakes results. ⛔ **Registry counters persist across boots** — prove a counter *moves this boot*.

### 11.1 The procedure

⛔ **Environment variables set in the `win_exec` shell do NOT reach a `schtasks /run` process.**
`schtasks /run` asks the Task Scheduler service to launch the task; the new process is a child of
the service, not of the PowerShell that issued the command, and inherits the service's environment.
Setting `$env:HELIOS_WSI_PERF` and then running the task gives you a run with **no perf file at
all** — which §11.2 signal 2 would read as `presents=0`, i.e. the *software-fallback signature*. A
wrong diagnosis, silently.

**Bake the variables into the task's `.cmd` wrapper.** That is the working guest pattern already —
`helios_vkcube_noins` runs `cmd /c C:\ProgramData\Helios\vkcube_noins.cmd`, whose body is literally
`set HELIOS_WSI_PERF=1` / `set HELIOS_WSI_PERF_FILE=…` / `set HELIOS_WSI_INSURANCE_BLIT=0` followed
by the exe. Clone that shape:

```powershell
# win_exec — 0. create the wrapper + the session-1 task, ONCE.
#    Template task to clone: helios_vkcube_noins (env-carrying wrapper, session 1,
#    InteractiveToken). Template wrapper: C:\ProgramData\Helios\vkcube_noins.cmd.
$cmd = @'
@echo off
set HELIOS_WSI_PERF=1
set HELIOS_WSI_PERF_INTERVAL=100
set HELIOS_WSI_PERF_FILE=C:\ProgramData\Helios\helios-d3d12-wsi-perf.txt
set VKD3D_DEBUG=warn
"C:\Users\Rupansh\helios-d3d12\triangle.exe"
'@
Set-Content -Path C:\ProgramData\Helios\d3d12_sample.cmd -Value $cmd -Encoding ASCII

schtasks /create /tn helios_d3d12_sample /sc once /st 00:00 /f `
  /tr "cmd /c C:\ProgramData\Helios\d3d12_sample.cmd" `
  /ru "$env:USERDOMAIN\$env:USERNAME" /it
#   /it == InteractiveToken == SESSION 1. Without it the window never appears
#   and the run is a session-0 lie (CLAUDE.md, 60th-session trap).
```

*(The `[Environment]::SetEnvironmentVariable(…, 'Machine')` route also works and survives reboots,
but it changes the variable for **every** process on the box including dwm — use the wrapper.)*

```powershell
# win_exec — 1. baseline the KMD present counters (values persist across boots!)
reg query "HKLM\SOFTWARE\Helios" /v PBcall
reg query "HKLM\SOFTWARE\Helios" /v PBFlip
reg query "HKLM\SOFTWARE\Helios" /v PBsrc

# 2. clear last run's perf file so a stale one cannot be read as this run's
Remove-Item C:\ProgramData\Helios\helios-d3d12-wsi-perf.txt -ErrorAction SilentlyContinue

# 3. run it in SESSION 1. Keep the window WINDOWED and partially overlapped (§10f).
schtasks /run /tn helios_d3d12_sample

# 3b. record the sample window's rect — step 6 needs it, and "the whole desktop
#     differs" is not evidence (see below).
schtasks /run /tn helios_enum_windows ; Start-Sleep -Seconds 2
Select-String -Path C:\ProgramData\Helios\all_windows.txt -Pattern 'triangle|d3d12'

# 4. TWO screenshots >= 2 s apart
schtasks /run /tn helios_paintcap ; Start-Sleep -Seconds 1
Copy-Item Z:\tmp\screen_copy.png Z:\tmp\d3d12_a.png
Start-Sleep -Seconds 3
schtasks /run /tn helios_paintcap ; Start-Sleep -Seconds 1
Copy-Item Z:\tmp\screen_copy.png Z:\tmp\d3d12_b.png

# 5. re-read the counters — they must have MOVED
reg query "HKLM\SOFTWARE\Helios" /v PBcall
```

⛔ **Do not `cmp` the whole capture.** A whole-desktop diff passes on a **frozen** sample: the
taskbar clock alone repaints every minute, and the cursor moves. That is the same false-positive
class §6.3 and §11.2 exist to prevent, sitting in the one step CLAUDE.md rule 6 makes ground truth.
Crop to the window rect from step 3b first — `magick`/`compare` are on the Linux host:

```bash
# Linux host — 6. compare ONLY the sample's client rect.
#    W H X Y from step 3b: rect=(X,Y)-(R,B)  =>  W=R-X  H=B-Y
W=800; H=600; X=100; Y=100
cd /home/rupansh/helios-vgpu/tmp
magick d3d12_a.png -crop ${W}x${H}+${X}+${Y} +repage d3d12_a_win.png
magick d3d12_b.png -crop ${W}x${H}+${X}+${Y} +repage d3d12_b_win.png
magick compare -metric RMSE d3d12_a_win.png d3d12_b_win.png null: 2>&1
# prints "<abs> (<normalised>)".
#   normalised == 0        -> IDENTICAL WINDOW — NOT A FRAME (fail)
#   normalised <  0.002    -> below the noise floor; treat as NOT A FRAME
#   normalised >= 0.002    -> content advanced inside the window (pass)
# Sanity: `magick compare -metric RMSE X.png X.png null:` must print `0 (0)`.
```

⚠ For a *scanout*-level oracle rather than a GDI paintcap — mandatory if the window is maximized or
promoted (§10f) — use the region-scoped RFB samplers instead: `tools/vnc_frame_probe.py --hud
x0,y0,x1,y1 --hudthresh …` takes exactly this kind of sub-rectangle, and
`tools/vnc_scanout_correlate.py` ties it to the QEMU `virtio_gpu_cmd_*` trace.

### 11.2 ⚠ Telling a real hardware present from the software fallback

**A correct picture proves nothing about the path.** Check all four:

| # | Signal | Hardware vehicle present | Software GDI fallback |
|---|---|---|---|
| 1 | `C:\ProgramData\Helios\helios_icd_diag.log` (`helios_wsi_vehicle_diag`, `wsi_common_win32.cpp:407-421`) | `wsi-vehicle: READY chain=… adapter=<Helios name>` (`:905-908`) **and** `wsi-vehicle: LIVE chain=… visual content bound (WxH)` (`:2228`) | `wsi-vehicle: FAILED chain=… stage='…' hr=0x…` (`:916`). For P-A specifically: `stage='CreateSwapChainForComposition' hr=0x80004001` |
| 2 | WSI perf line, `C:\ProgramData\Helios\helios-d3d12-wsi-perf.txt` — the exact path `HELIOS_WSI_PERF_FILE` is set to in §11.1's wrapper (`helios_win32_wsi_perf_write`, `wsi_common_win32.cpp:163-207`) | `vehicle: ready=1 creates=1 fails=0 exp_miss=0 … presents=<climbing> pfails=0` | `ready=0 … fails>=1`, `presents=0`, and `Helios WSI win32 frames=` climbing with `copy_ms`/`stretch_ms` nonzero. ⚠ **A missing file is not this signature** — it means the env vars never reached the process (§11.1); fix the wrapper and re-run rather than diagnosing a fallback |
| 3 | UMD log `C:\ProgramData\Helios\umd-<pid>.log` (`umd/src/log.rs:25-28`) | `vehicle present #N: imports_failed=0 copies_failed=0 geom_mismatch=0 overwrites=0` (`present.rs:1560-1571`; logged at N<4 then every 512) | **no `vehicle present` lines at all** |
| 4 | KMD counters, `HKLM\SOFTWARE\Helios` | `PBcall` moving; on the vehicle arm expect the **BLT** counters (`PBCpy`) since the vehicle's Present is an ordinary windowed DXGI present | `PBcall` still moves (dwm composes the GDI-painted window) — ⚠ **counter movement alone does NOT distinguish the arms**; use signals 1-3 |

⚠ **Signal 4 is the trap.** The KMD sees presents either way, because in the software case DWM is
still compositing the window that GDI painted. Only signals 1-3 distinguish the vehicle from the
fallback. This is why §6.3 calls the demotion "silent to the screen, loud in the diag".

Additional cross-checks:

* `odd_hr=` in the perf line counts SUCCESS-but-not-`S_OK` presents (`DXGI_STATUS_OCCLUDED` &c.) —
  those frames were **not displayed** (`wsi_common_win32.cpp:2172-2188`).
* `drops=` must be read beside any fps number on a non-FIFO chain (§7.4).
* `gate_arms=` vs `gate_fb=` — today `gate_arms` is **always 0** and `gate_fb` == `presents`
  (§7.1). If `gate_arms` is ever nonzero, someone revived `helios_umd_get_present_result` and the
  5.57 ms number no longer applies.
* If nothing at all appears in the vehicle diag, check `exp_miss` — a UMD-only deploy that dropped
  one of the three `helios_umd_*` exports kills the vehicle process-wide with `E_NOINTERFACE`
  (`vehicle_exports.rs:7-11`).
* **Host-side, when the guest evidence runs out:** the venus-level lever is
  **`HELIOS_VKR_DEBUG=validate`** on the QEMU relaunch, which enables the host validation layers and
  puts venus's own complaints into `/tmp/helios-qemu-stderr.log` (`ROADMAP.md:1901-1903`). ⛔ It is
  **not** `VIRGL_LOG_LEVEL=debug` — venus runs in the render-server child and only WARN+ reaches the
  qemu stderr log (`HOST.md` §5.1). ⚠ Changing it is a QEMU relaunch: **owner-gated**, ask first
  (CLAUDE.md, VM launch ownership). And per the standing directive, never reach for host evidence to
  explain a guest failure until the guest evidence above is exhausted.

### 11.3 The Phase-0 experiment in order

0. **Build DXVK's `dxgi.dll`** — it does not exist in the tree (§9 iii). One command on the Linux
   host, against the already-configured mingw cross build dir:
   ```bash
   ninja -C /home/rupansh/helios-vgpu/dxvk-helios/build.w64 src/dxgi/dxgi.dll
   ```
1. **§6.6** — separate V1 from P-A with `helios_dcomp_probe` and the `dxgi.dll` from step 0.
   No driver change, no reboot. Honour both precondition asserts.
2. **§6.4** — land the P-A fix (`win_meson`, see `TOOLCHAIN.md`; ICD-only, no KMD/UMD rebuild, no
   reboot — a new process picks up the new ICD), re-run step 1 to green.
3. **Build vkd3d-proton for Windows.** `GATES.md` §4.1 (`D12-G0`) owns the exact recipe and the pass
   criterion; `DECISIONS.md` §6.1 settles the arm: **Linux mingw cross is primary** (the whole
   toolchain — `x86_64-w64-mingw32-{gcc,g++}`, `widl`, `glslangValidator`, `meson`, `ninja` — is
   already on the host's `PATH`, and it matches vkd3d-proton's own shipping build). Do not improvise
   a second build dir; run the gate. In outline:
   ```bash
   cd /home/rupansh/helios-vgpu/vkd3d-proton-helios
   git submodule update --init --recursive     # khronos/*, subprojects/dxil-spirv are EMPTY
   meson setup --cross-file build-win64.txt --buildtype release \
         -Denable_tests=true -Denable_extras=true \
         /home/rupansh/helios-vgpu/tmp/dx12/build/vkd3d-win64
   ninja -C /home/rupansh/helios-vgpu/tmp/dx12/build/vkd3d-win64
   # artifacts: libs/d3d12/d3d12.dll, libs/d3d12core/d3d12core.dll,
   #            tests/d3d12.exe, demos/triangle.exe, demos/gears.exe
   ```
   ⛔ `package-release.sh` is the wrong tool (it never passes `-Denable_tests` and deletes the build
   dir). Native MSVC x64 on the VM is the **fallback**, taken only when a Windows debugger is
   wanted, and it must build to a **local `C:` path, never `Z:\`**.
4. Deploy `d3d12.dll` + `d3d12core.dll` + DXVK `dxgi.dll` **app-local** beside **one windowed**
   D3D12 sample (`demos/triangle.exe` is the obvious first one) and run it via the session-1
   schtask of §11.1.
5. **§11.1** evidence gate — including the cropped, window-scoped image compare.
6. **§11.2** path corroboration — all four signals.
7. Only then quote a number, and quote it with §7's copy count and the 5.57 ms gate stated.

---

## 12. UNVERIFIED — the list, with settling experiments

| id | Claim not established | Settling experiment | Blocks |
|---|---|---|---|
| **U1** | The D3D12 runtime issues `D3DKMTPresent` (`d3dkmthk.h:5929`) rather than `D3DKMTPresentRedirected` (`:6078`) or another internal variant | WARP spy proxy (`DX12.md` P1); or ETW `Microsoft-Windows-DxgKrnl` all-keywords around a D3D12 sample on a real driver, read the `Present`/`QueuePacket` events | nothing — the KMD sees `DxgkDdiPresent` either way |
| **U2** | `D3D12DDI_TABLE_TYPE_DXGI = 3` (`d3d12umddi.h:2493`) is declared with **no** function-table struct in the header. Whether the runtime ever fills it, and with what size | WARP spy proxy logs every `pfnFillDDITable(TableType, TableSize)` pair | option (i), table negotiation |
| **U3** | Whether DXGI ever passes a non-null `hDstResource` in `D3D12DDIARG_PRESENT_0001` for a flip-model windowed swapchain | WARP spy proxy; or instrument `pfnPresent` once a Helios D3D12 UMD exists | option (i) `pfnPresent` body |
| **U4** | Whether DXVK's `DxgiSwapChain` recreates the `VkSurfaceKHR` on a fullscreen transition | read `dxvk-helios/src/dxgi/dxgi_swapchain.cpp` fullscreen/resize paths | low stakes — the hwnd→target registry is refcounted by HWND (§10a) |
| **U5** | **Risk V1:** does MS `d3d11.dll` accept a DXVK `IDXGIAdapter` when app-local redirection binds its `dxgi.dll` import to DXVK's? | **§6.6, no build** — DXVK `dxgi.dll` beside `helios_dcomp_probe`, session-1 schtask, read the staged HRESULTs | Phase 0 (options ii/iii) |
| **U6** | Whether `D3D12DDIARG_PRESENT_0001.pPrivateDriverData` reaches `DXGKARG_PRESENT.pPrivateDriverData` (the D3D11 equivalent measurably did **not** — `PBIdOk` = "no payload" ×3 generations) | WARP spy proxy for the *call* half; re-add the KMD `PBIdOk` decode behind `DiagLevel >= 1` for the *arrival* half, once a D3D12 present exists | §8 option (i) only; (ii) does not need it |
| **U7** | That the D3D12 runtime tolerates the driver calling `pfnRenderCb` around `pfnPresent` at all, and that `pfnPresent` runs before the runtime's own submission on that queue with nothing of the runtime's own in between | `DECISIONS.md` §3-H2 P-C's experiment: `pfnRenderCb` + a counting `DxgkDdiRender` on the D3D12 path at `GATES.md` **G7**, before G8 depends on it. WARP spy proxy gives the call order for free; live fallback = KMD counter pair `P12sub`/`P12take`, so a pairing failure is loud | §8 option (ii) |
| **U8** | Post-fix **fullscreen** vehicle behaviour **on the shipping gate path**. ⚠ Not "no measurement exists" — `ROADMAP.md:2919-2931` measured it (fullscreen 1896×1030 chain VEHICLE/READY+LIVE, `queue_present_avg` 5.96→2.81 ms, acquire gate 4.06→7.69 ms). It was taken with `VehicleKernelFlipWait=1`, which R912(a) has since retired | Re-run §11.1 with a fullscreen client on shipping defaults, session-1 schtask, `HELIOS_WSI_PERF=1` via the `.cmd` wrapper; read `creates=/fails=/ready=/gate_arms=/gate_fb=` and take the picture through Looking Glass or the VNC samplers (§10f). Compare against 2.81 ms / 7.69 ms | Phase-0 fullscreen numbers **quoted as current** (the historical pair stands as-is) |
| **U9** | Whether `VK_KHR_swapchain` can silently disappear — it is conditional on `renderer_sync_fd.semaphore_importable` (`vn_physical_device.c:1334`), and its absence kills D3D12 device creation outright, not just presentation (§5.2) | add a gate assertion: `vulkaninfo` on the guest must list `VK_KHR_swapchain` before any D3D12 gate runs (`GATES.md`) | every D3D12 arm |
| **U10** | The *loader* half of P-A: that `LoadLibraryExA("dxgi.dll", NULL, LOAD_LIBRARY_SEARCH_SYSTEM32)` returns an already-loaded app-local DXVK `dxgi.dll` rather than System32's. §6.6's probe **cannot** show this — it links `-ldxgi` and never calls `LoadLibraryEx` | Add an arm to `tools/dcomp_present_probe.cpp`: call `LoadLibraryExA("dxgi.dll", NULL, LOAD_LIBRARY_SEARCH_SYSTEM32)` **and** `LoadLibraryW(L"C:\\Windows\\System32\\dxgi.dll")`, print `GetModuleFileNameW` on both handles, run it with a DXVK `dxgi.dll` app-local. ~15 lines, one rebuild of the probe | Nothing — §6.4 ships the `GetModuleFileNameW` verification either way, which is correct under both outcomes. It only decides whether the ⛔ is a proven rule or a documented-behaviour inference |
| **U11** | How the D3D12 UMD gets from a `D3D12DDI_HRESOURCE` to a venus resid / `venus_alloc_size` / `memory_type_index` / pitch / plane offset. The D3D11 side reads its own resource table (`presented_primary_private`, `umd/src/forward/state.rs:736`); the D3D12 equivalent does not exist yet | Design it with `pfnCreateHeapAndResource` (`DECISIONS.md` §3-H3), not with `pfnPresent` — the same table is what `pfnAllocateCb` needs to author `HeliosWddmAllocPrivate` (§9 option i, `create_allocation.rs:2291-2307`). Settled by writing it, at `GATES.md` G7/G8 | §8 option (ii) step 1; §9 option (i) |
| **U12** | Which context-creation callback the D3D12 UMD ends up on, and whether `DxgkDdiRender` fires for `pfnRenderCb` on a `VirtualAddressing` context. The D3D11 UMD uses legacy `pfnCreateContextCb` (`umd/src/device_funcs.rs:1053-1061`), which is why `DxgkDdiRender` fires today; `DECISIONS.md` D5 has the D3D12 UMD picking its node in `D3DDDICB_CREATECONTEXTVIRTUAL` | Same G7 experiment as U7: `RENDER_COUNT` (`kmd_render/src/ddi/submit_command.rs:996`) moving on the D3D12 path is the whole test | §8 option (ii) step 2 being empty |

⚠ Note that **U1, U2, U3, U6 and U7 are all answered by the same artefact** — the WARP spy proxy of
`DECISIONS.md` §3-H1 (`C:\Windows\System32\d3d10warp.dll` exports `OpenAdapter12`). That is five of
this document's twelve open questions for roughly one day's work and no driver change, which is the
strongest argument in this file for building it early.
