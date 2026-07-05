# DISPLAY.md — Archived Display Pivot Notes

> **ARCHIVED (2026-06-07):** this file is no longer canonical. The active project direction is
> **System-class KMDF + DeviceIoControl + Mesa Venus**. Read
> [`SYSTEM_CLASS_REFOCUS_2026_06_07.md`](SYSTEM_CLASS_REFOCUS_2026_06_07.md) and [`ARCH.md`](ARCH.md) first.
> Keep this file only as historical reference for the DOD/`SET_SCANOUT_BLOB` display experiment and its
> assumptions.

Original title: Helios Display Engine (Phase 7): WDDM Display-Only Driver + Venus + zero-copy `SET_SCANOUT_BLOB`.

---

## 0. The pivot in one paragraph

The Mesa software WSI present (`wsi_win32` → `memcpy` + `StretchBlt`) is **architecturally incapable** of
using the host's GL-accelerated display: it issues **no virtio-gpu scanout flush**, so the host GL backend
(which only presents on a guest-driven scanout update) never sees it, and the present blocks on a host→guest
visibility lag (~3.5 s/frame → <1 fps). The fix is to stop blitting and instead make **Helios itself own the
virtio-gpu scanout** and present venus-rendered content with **`VIRTIO_GPU_CMD_SET_SCANOUT_BLOB`** of a
host-GPU-resident venus blob (its exported `dmabuf_fd`), which the host imports and displays **zero-copy,
GL-accelerated** under **`-spice gl=on`**. To own the scanout, Helios becomes a **WDDM Display-Only Driver
(DOD)** for `PCI\VEN_1AF4&DEV_1050` (Display class, replacing the inbox `VioGpuDod`) that **also** carries the
six venus ops over **`DxgkDdiEscape`**. One driver, one PCI FDO: it drives the **2D Windows desktop** (the DOD
present path, GL-displayed via spice) **and** the **fullscreen venus fast path** (`SET_SCANOUT_BLOB`,
zero-copy). This is "the venus-accelerated device displays the Windows VM's output directly."

---

## 1. Decision record — what was researched and rejected (read before re-litigating)

A 13-agent research+verification workflow (2026-06-06) established the following with the cited confidence.
Each claim was adversarially verified; the verdicts are recorded so the next session does not redo them.

| # | Finding | Verdict |
|---|---------|---------|
| LB3 | **DWM GPU-composites the desktop ONLY through a WDDM render adapter's native D3D UMD.** A non-WDDM Vulkan ICD (venus) and app-process translators (DXVK/VKD3D) are invisible to DWM. With no WDDM **render** adapter, the desktop is composited on **WARP/CPU**. | **confirmed** |
| LB4 | **A loadable WDDM render miniport still cannot GPU-composite the desktop without a separately-authored native D3D10/11 DDI UMD that translates to venus** — a multi-man-year effort, no reusable starting point (Dozen/dzn is Vulkan-over-D3D12, the wrong direction; DXVK is an app-level DLL, not a WDDM UMD). | **confirmed** |
| LB1 | The <1 fps windowed-present bug is **host-side host-visible-blob visibility lag** (the empty WSI submit's feedback fence + the swapchain pixels, read through the hostmem PCI-BAR window), **not** the guest CPU blit or submit cost (both measured at tens of ms). | **confirmed** (location); the *cure* was the open part |
| LB2 | "Just switch the host to `gtk,gl=on`/`spice gl=on`" **does NOT by itself fix the software WSI path.** Per QEMU source, the host GL loop only fires on a guest **scanout flush** (`RESOURCE_FLUSH`/`SET_SCANOUT`), and `wsi_win32` issues none; host-visible-blob coherency is the KVM mapping's job, independent of the display backend. The backend change **only helps frames that go through a real scanout flush.** | mechanism **refuted** → motivates routing present *through* `SET_SCANOUT[_BLOB]` |
| LB5 | **Exactly one function driver binds `PCI\VEN_1AF4&DEV_1050`.** Helios (FDO) and `VioGpuDod` (FDO) are **mutually exclusive**; Helios currently force-installs *over* `VioGpuDod`, so there is **no `VioGpuDod` desktop scanout** while Helios is bound. A coexisting desktop must come **from Helios itself** (the DOD path) or a separate PCI device. | **confirmed** |
| LB6 | `SET_SCANOUT_BLOB` needs a **dmabuf-backed HOST3D blob** (`res->base.dmabuf_fd >= 0`, set at create via `virgl_renderer_resource_get_info()`, **not** udmabuf). Venus **always** runs in the **render-server** process (QEMU sets `VIRGL_RENDERER_VENUS\|VIRGL_RENDERER_RENDER_SERVER` together), which performs the export; it is a **real dmabuf only if the host Vulkan driver exports DMA_BUF** — **Intel ANV does**. virtio-gpu has **no overlay planes** (a scanout is a whole monitor); QEMU supports up to 16 scanouts (`max_outputs` default 1). | **partially-correct** (the "in-process / no render-server" qualifier was the only error; corrected here) |

**Conclusions baked into this spec:**

1. **WDDM render miniport: rejected** (LB3+LB4). The System-class pivot was correct; we do **not** revive it.
2. **The software WSI present is a dead end** (LB2) — it cannot be made fast/zero-copy. Present must go
   **through a virtio-gpu scanout** so the host GL backend imports it.
3. **Helios must own the scanout** (LB5) → Helios becomes the **DOD** (Display class), one FDO.
4. **The venus fast path is `SET_SCANOUT_BLOB` of an exportable venus image** (LB6), displayed by
   **`-spice gl=on`**. The **#1 load-bearing unknown** is whether the venus swapchain image's memory is
   created **exportable** so the render-server hands QEMU a real `dmabuf_fd`. **Validate this first (§8).**
5. **The IDD path was considered and is NOT the primary route.** An IDD's frames are WARP-composited CPU
   copies (no WDDM render adapter), so it does not display venus content "directly/zero-copy." It remains a
   *possible later add-on* if a full virtual-desktop-on-host is wanted beyond what the DOD primary gives — but
   it is not in this spec's critical path.

---

## 2. Architecture: Helios-as-DOD + Venus, one FDO

```
┌──────────────────────────────────────────────────────────────────────────────┐
│  Windows 11 Guest                                                              │
│                                                                                │
│   DWM (WARP / CPU-composited 2D desktop — no WDDM render adapter, unavoidable)  │
│        │ primary surface (system memory)                                        │
│        ▼                                                                        │
│   ┌─────────────────────────────────────────────────────────────────────────┐  │
│   │ Helios DOD  (helios_kmd.sys, Display class {4d36e968-…})                 │  │
│   │  • DxgkDdiPresentDisplayOnly → desktop primary → RESOURCE_CREATE_2D +    │  │
│   │    TRANSFER_TO_HOST_2D + SET_SCANOUT(0) + RESOURCE_FLUSH   (2D path)     │  │
│   │  • DxgkDdiEscape → the 6 venus ops (CTX_CREATE/SUBMIT_VENUS/ALLOC_BLOB/   │  │
│   │    MAP_BLOB/WAIT_FENCE/CTX_DESTROY) + HELIOS_PRESENT_BLOB  (venus path)  │  │
│   │  • Scanout-0 arbiter: desktop primary  ⇄  fullscreen venus blob          │  │
│   │  • virtio-gpu transport (REUSED): cap scan, VirtQueue<WdkHal>, gpu.rs    │  │
│   └───────────────┬───────────────────────────────────┬─────────────────────┘  │
│        D3DKMTEscape (venus)                            │ virtio-gpu (virtqueues)│
│        ▲                                               │ VEN_1AF4 DEV_1050      │
│   ┌────┴───────────────────────┐                       │                        │
│   │ Mesa venus Vulkan ICD      │  app fullscreen present:                       │
│   │  vn_renderer_helios →      │   render → swapchain VkImage = exportable      │
│   │  D3DKMTEscape transport    │   HOST3D blob → HELIOS_PRESENT_BLOB(res_id)    │
│   └────────────────────────────┘                       │                        │
└────────────────────────────────────────────────────────┼────────────────────────┘
                                                          ▼
┌──────────────────────────────────────────────────────────────────────────────┐
│  Linux Host:  QEMU virtio-gpu-gl (venus=on,blob=on,hostmem=…)  -spice gl=on    │
│   • desktop:  2D scanout resource → uploaded to a host GL texture → spice GL    │
│   • venus FS: SET_SCANOUT_BLOB(res_id) → res->dmabuf_fd (exported by the venus  │
│               RENDER SERVER via ANV) → spice shares the dmabuf → ZERO-COPY GL   │
│   virglrenderer (venus) ── render-server process ── Intel ANV ── Intel iGPU     │
└──────────────────────────────────────────────────────────────────────────────┘
```

**Key property of choosing "Helios-as-DOD owns scanout 0":** there is **no scanout-arbitration fight.** The
old `wsi-present-plan` worried that `VioGpuDod` owns scanout 0 and would reclaim it; here **Helios IS the DOD**,
so it owns scanout 0 outright and simply **mode-switches** that one scanout internally between the desktop
primary and a fullscreen venus blob. `max_outputs=1` suffices (a second scanout is optional, not required).

---

## 3. Driver-model change (System-class KMDF → WDDM Display-Only miniport)

The DOD is a **dxgkrnl display-only miniport** (`dispmprt.h`, `DxgkInitializeDisplayOnlyDriver`), **not** KMDF.
This re-enters the WDDM/dxgkrnl surface the System-class pivot left — but **only the display-only half**, which
**does not have the render AddAdapter capability/version contract** that produced Code 43 / `STATUS_REVISION_MISMATCH`.
A DOD implements display/VidPN/present DDIs only. Precedent: the inbox **`VioGpuDod`** (virtio-gpu DOD, BSD,
`kvm-guest-drivers-windows/viogpu/viogpudo`) and **`qxldod`** both load cleanly at this exact model.

### 3.1 What REVERTS (the System-class pivot undone for the display layer)

- **INF class:** `System {4d36e97d-…}` → **`Display {4d36e968-e325-11ce-bfc1-08002be10318}`**, with the
  display `SoftwareSettings` / VidPN bits a DOD needs. (One-time `helios_kmd.inx` rewrite — the same kind of
  exception the pivot took.) **Removes** the System-class `[.Wdf]`/`GUID_DEVINTERFACE_HELIOS` device-interface;
  **adds** the display-only service install. The driver still **out-ranks `VioGpuDod`** only via force-install
  (`devcon update`) unless the INF model string is made more specific — keep the force-install recipe.
- **Driver entry:** `WdfDriverCreate`/`WdfDeviceCreate`/`WdfDeviceCreateDeviceInterface` → **`DriverEntry`
  building `DXGKDDI_DISPLAY_ONLY_FUNCTIONS` + `DxgkInitializeDisplayOnlyDriver`** (the original Helios WDDM
  `DriverEntry` shape; recover from git `658168f:kmd/src/lib.rs`).
- **PCI config access:** `KmdfConfigAccess` (`BUS_INTERFACE_STANDARD`) → **`DxgkConfigAccess`**
  (`DxgkCbReadDeviceSpace`/`WriteDeviceSpace` via the `DXGKRNL_INTERFACE`). Both exist in history; recover
  `DxgkConfigAccess` from `658168f:kmd/src/virtio/config.rs`. `VirtioGpu::init(&DxgkConfigAccess)`.
- **Interrupt + BAR map:** the WDF interrupt object/`MmMapIoSpaceEx` move to the DOD's
  **`DxgkDdiStartDevice`** (the translated resource list arrives there) and the dxgkrnl
  interrupt callbacks (`DxgkCbQueueDpc`/`DxgkCbNotifyInterrupt`) for used-ring completion → per-fence KEVENT.
- **The `dxgk` bindgen** (`dispmprt.h`/`d3dkmddi.h`) returns — but **DOD-scoped only** (the display-only DDIs +
  the escape struct), far smaller than the deleted render bindgen. Recover/trim from `658168f:kmd/src/dxgk.rs`
  and `658168f:kmd/build.rs`. **Do NOT** bring back `d3dkmddi.h` render DDIs, `displib.lib`, `DxgkInitialize`,
  or any AddAdapter cap handlers — those are the rejected render path.

### 3.2 What is REUSED unchanged (the valuable, tested core)

- **virtio-gpu transport:** `kmd/src/virtio/{gpu.rs, hal.rs, queue.rs, pci.rs}` — cap scan (incl.
  `SHARED_MEMORY_CFG`/HOST_VISIBLE), `VirtQueue<WdkHal>`, the venus submit (`ctx_create`/`ctx_destroy`/
  `submit_venus`), `alloc_blob`/`map_blob`, `pop_used`/`ack_interrupt`. `WdkHal` is model-agnostic. Add the
  scanout commands (`SET_SCANOUT`, `SET_SCANOUT_BLOB`, `RESOURCE_CREATE_2D`, `TRANSFER_TO_HOST_2D`,
  `RESOURCE_FLUSH`) — see §5.
- **The `protocol/` crate:** all six op structs (byte layout **unchanged** — they ride `DxgkDdiEscape` exactly
  as the original M3.3 spine did before the IOCTL move; the move back is the inverse of the System-class pivot).
- **The escape dispatch body:** the current `ioctl.rs` handler logic is the same body the original
  `ddi/escape.rs` ran; it ports back to a `DxgkDdiEscape` handler 1:1 (trust-boundary validation included).
- **The Mesa venus ICD** above `vn_renderer` — unchanged. Only the backend transport changes (§4).
- **The Phase-4e async submit + the `vn_queue.c` fence fixes** — keep; do not regress.

### 3.3 Rust vs. fork-VioGpuDod (decision for the implementer)

Two ways to get the DOD DDI surface:
- **(Recommended) Stay Rust.** Resurrect the DOD-scoped `dxgk` bindgen and implement
  `DxgkDdiPresentDisplayOnly` / the VidPN DDIs (`IsSupportedVidPn`, `EnumVidPnCofuncModality`, `CommitVidPn`,
  `RecommendMonitorModes`, `SetVidPnSourceVisibility`, …) / pointer DDIs / `DxgkDdiEscape` in Rust, **using
  `VioGpuDod` (C) and `qxldod` as the reference** for the present/VidPN logic. Keeps the codebase unified and
  reuses `gpu.rs`/`hal.rs` directly. Highest continuity.
- **(Alternative) Fork `VioGpuDod` (C)** and bolt on the venus escape + `SET_SCANOUT_BLOB`. Lower DOD risk
  (proven desktop scanout) but splits the codebase and forces reimplementing/relinking the Rust venus core in C.

Recommend Rust for continuity; the VioGpuDod present path is small enough to reimplement and is the canonical
reference for "what virtio-gpu commands a DOD emits per present."

---

## 4. The venus channel: `DxgkDdiEscape` (revert from IOCTL; byte layout unchanged)

A dxgkrnl miniport's device object is owned by dxgkrnl; a DOD cannot expose its own `IRP_MJ_DEVICE_CONTROL`
interface, so the venus user→kernel channel reverts to the **WDDM-sanctioned `DxgkDdiEscape`** — which is
exactly what the original Helios M3.3 spine used (`D3DKMTEscape` → `DxgkDdiEscape`). The six op structs are
**byte-identical**; only the carrier changes.

- **KMD:** implement `DxgkDdiEscape(hAdapter, *DXGKARG_ESCAPE)`; dispatch on the `HeliosEscapeHeader.cmd_type`
  (the header that became "optional" under IOCTL is **re-required** as the verb selector). Run the same body as
  today's `ioctl.rs` (`adapter.with_virtio(|v| v.ctx_create/submit_venus/…)`). `SUBMIT_VENUS`'s large Venus
  blob rides the escape buffer (or a MAP_BLOB-backed shared region for big payloads — revisit the
  METHOD_IN_DIRECT MDL tradeoff as an escape-private-data pointer).
- **ICD (Mesa `vn_renderer_helios`):** swap the transport from `DeviceIoControl(GUID_DEVINTERFACE_HELIOS)` back
  to **`D3DKMTOpenAdapterFromLuid` + `D3DKMTEscape`** (the `vn_renderer_vtest`-shaped vtable is unchanged; only
  the submit/wait/alloc/map syscalls change). **Bonus:** because Helios is now a real **WDDM adapter** (a DOD
  has a LUID and is DXGI-enumerable), `D3DKMTOpenAdapterFromLuid` **works** — resolving the ARCH.md §12 open
  question ("does DXVK degrade gracefully when no WDDM adapter backs the LUID"). The venus ICD can also be bound
  to the adapter via the per-adapter `VulkanDriverName` registry value (cleaner than the global Khronos JSON),
  though the global JSON still works.
- **New op:** add **`HELIOS_PRESENT_BLOB`** (escape verb) = "make scanout 0 show this resource id"
  (fullscreen-enter) and its inverse "restore the desktop primary" (fullscreen-exit). This is the venus WSI
  backend's signal to the scanout arbiter (§5). It carries `{ resource_id, width, height, format, fence_id }`.

> **Alternative considered (keep the IOCTL transport):** a DOD *could* `IoCreateDevice` a **side-band control
> device** + `IoRegisterDeviceInterface(GUID_DEVINTERFACE_HELIOS)` and keep the existing `DeviceIoControl`
> venus channel (no Mesa-backend change). This is **non-standard** for a dxgkrnl miniport (dxgkrnl owns the
> primary driver-object dispatch; a side-band device's IRP routing is fiddly and unproven here) and is **not
> recommended** as the default — but if avoiding the `vn_renderer_helios` transport revert is valuable, evaluate
> it. `DxgkDdiEscape` is the sanctioned, precedented (VioGpuDod), low-risk path and the revert is cheap
> (byte layouts unchanged), so the spec defaults to it.

---

## 5. The present path (one scanout, two sources, arbitrated)

Helios owns **scanout 0** and switches it between two sources.

### 5.1 Desktop primary (2D, WARP-composited, GL-*displayed*)

`DxgkDdiPresentDisplayOnly` hands the DOD a **system-memory** source surface + dirty rects (the WARP-composited
desktop; GPU composition is impossible without a render adapter — LB3, accepted). Per present, the DOD does the
standard virtio-gpu 2D scanout (mirror `VioGpuDod`):
`RESOURCE_CREATE_2D` (once per mode) → `ATTACH_BACKING` (guest pages) → `TRANSFER_TO_HOST_2D`(dirty rect) →
`SET_SCANOUT(scanout 0, res)` (once) → `RESOURCE_FLUSH`(dirty rect). Under **`-spice gl=on`** the host uploads
the 2D resource to a GL texture and presents it via GL (no host readback). This is a normal, usable Windows
desktop on the SPICE client — equivalent to `VioGpuDod`-on-spice-gl, which is the proven baseline. Hardware
cursor via `DxgkDdiSetPointerShape`/`Position` → virtio-gpu cursor queue.

> The desktop is a guest→host **upload** per dirty rect (not zero-copy) and **WARP-composited** (CPU). That is
> the unavoidable cost of a Windows desktop with no WDDM render adapter; it is fine for 2D shell/Office/video
> and is **not** the bottleneck this pivot targets. The zero-copy win is for **venus** content (§5.2).

### 5.2 Fullscreen venus (zero-copy `SET_SCANOUT_BLOB`, the acceleration win)

When a venus/DXVK/VKD3D app goes fullscreen-exclusive, its Vulkan WSI present (in `vn_renderer_helios` /
`vn_wsi`) **stops doing the GDI blit** and instead:
1. ensures the **swapchain `VkImage` is backed by an exportable HOST3D blob** (the venus mem allocation chains
   `VkExportMemoryAllocateInfo` so the render-server exports a `dmabuf_fd` — **see §8, the load-bearing gate**),
   already wired through `ALLOC_BLOB(blob_id = venus mem id)`;
2. renders into it (host GPU, via venus — already works);
3. issues **`HELIOS_PRESENT_BLOB(resource_id, fence)`** (escape).
The KMD's scanout arbiter then issues **`SET_SCANOUT_BLOB(scanout 0, resource_id, fmt, w, h)`** +
`RESOURCE_FLUSH`. QEMU's `virgl_cmd_set_scanout_blob` uses `res->base.dmabuf_fd` (set at blob-create via
`virgl_renderer_resource_get_info`) and hands the dmabuf to spice → **zero-copy, GL-accelerated, no CPU blit,
no readback.** Double-buffer with two blobs + the `fence_id`→KEVENT so present does not tear.

**Windowed venus stays limited.** A windowed 3D app must composite into the WARP desktop, which means its
host-GPU frame round-trips host→guest to reach DWM — the same readback limit, unavoidable without a render
adapter. **Scope:** fullscreen venus = fast/zero-copy; the 2D desktop = GL-displayed; windowed 3D = limited
(deprioritized; revisit only if a venus-on-IDD virtual-desktop add-on is ever pursued).

---

## 6. Host configuration (required)

- **Display backend:** move the win11 VM off `-display egl-headless,rendernode=/dev/dri/renderD129` to
  **`-spice gl=on`** or `-display gtk,gl=on` in `tools/launch-helios-gtk.sh`. This is the chosen
  production backend (matches "SPICE + OGL"); for first bring-up debugging,
  `-display gtk,gl=on` on the local console is the easiest place to *see* the dmabuf import working. Both import
  the scanout (2D texture or blob dmabuf) as a host GL texture with **no `glReadPixels` readback** (the
  egl-headless readback is what the old path paid). **Requires a launcher edit + restart + `devcon` rebind.**
- **Device:** keep standalone `virtio-gpu-gl` with `venus=on,blob=on,hostmem=…`. `max_outputs=1` is
  sufficient (Helios owns scanout 0 for both desktop and venus). `context_init`/`render-server`/`udmabuf` stay.
- **Venus export:** venus runs in the **render-server** process (always, per LB6). The render-server is what
  exports the per-resource `dmabuf_fd` for `SET_SCANOUT_BLOB`. **Intel ANV exports DMA_BUF** (the setup's host
  driver), so the export is expected to yield a real, scan-out-able dmabuf — **but this is the gate in §8.**

---

## 7. What this delivers — and what it does NOT (be honest)

| Capability | Delivered? | How |
|---|---|---|
| A real Windows **desktop** visible on the host (SPICE) | ✅ | DOD 2D scanout, GL-displayed under spice gl=on (replaces VioGpuDod) |
| **Fullscreen** venus / DXVK / VKD3D apps, low-latency, GL-accelerated | ✅ (the win) | `SET_SCANOUT_BLOB` of the venus swapchain dmabuf — zero-copy |
| Venus device **displays the VM output directly** | ✅ | both the desktop and fullscreen venus go through the venus-owned virtio-gpu scanout + host GL |
| **Windowed** 3D apps at full speed | ❌ | WARP composites the desktop; the app frame round-trips host→guest (no WDDM render adapter) |
| **GPU-composited** 2D desktop (DWM on the GPU) | ❌ (impossible here) | needs a WDDM render adapter + a native D3D-to-venus UMD — rejected (LB3/LB4) |

If a *full GPU-composited windowed* experience is ever required, the only path is the rejected multi-man-year
D3D-to-venus WDDM UMD; this spec deliberately does not pursue it.

---

## 8. ⚠️ First milestone = the go/no-go gate (do this BEFORE the DOD rewrite)

The whole plan rests on **LB6**: that a **venus-rendered blob is exported as a real `dmabuf_fd`** so
`SET_SCANOUT_BLOB` can display it zero-copy under spice gl=on. De-risk this **without** the DOD rewrite, using
the **current System-class Helios** (which already owns the FDO and can issue virtio-gpu commands):

1. Switch the VM to `-display gtk,gl=on` (local, easiest to see) or `-spice gl=on`; rebind Helios.
2. Add a **temporary** `SET_SCANOUT_BLOB` test path to the current driver (a throwaway IOCTL or escape):
   render one venus frame into a host-visible/exportable HOST3D blob (the existing `ALLOC_BLOB(blob_id=mem_id)`
   + venus render), then issue `SET_SCANOUT_BLOB(scanout 0, that resource)` + `RESOURCE_FLUSH`.
3. **Observe the host display.** If the venus frame appears on the gtk/spice window, **zero-copy scanout of a
   venus blob works → the DOD investment is de-risked, proceed.** Verify on the host that
   `virgl_cmd_set_scanout_blob` did **not** hit `RESP_ERR_UNSPEC` ("resource not backed by dmabuf") — i.e.
   `res->base.dmabuf_fd >= 0` (the venus image exported a dmabuf via ANV/render-server).
4. **If it fails** (`dmabuf_fd < 0` / `RESP_ERR_UNSPEC`): the venus WSI image is not being created exportable,
   or ANV returned `OPAQUE_FD` only. Fix the venus allocation to chain `VkExportMemoryAllocateInfo` with the
   dma-buf handle type (virglrenderer MR !1458 context), confirm `render-server` + `udmabuf`, and re-test
   **before** committing to the DOD. This is the cheapest possible place to learn it.

Only after this gate passes do you flip the driver model to a DOD (§3) and wire `HELIOS_PRESENT_BLOB` (§4–§5).

---

## 9. Phase plan (Phase 7 = Display Engine)

- **7.0 — Go/no-go gate (§8):** spice/gtk `gl=on` + a throwaway `SET_SCANOUT_BLOB` of a venus blob on the
  *current* System-class driver. Confirm a venus frame displays zero-copy. **Decisive; do first.**
- **7.1 — DOD skeleton:** flip INF System→Display, `DxgkInitializeDisplayOnlyDriver`, the DOD DDI surface
  (recover the shape from `658168f`), `DxgkDdiStartDevice` maps BARs + inits the *reused* virtio transport,
  `DxgkConfigAccess`. Gate: device loads as a Display adapter (Code 0), desktop appears on spice (2D path).
  **STATUS (2026-06-06):** the canonical reference is the **Microsoft KMDOD sample**
  (`Windows-driver-samples/video/KMDOD`), not VioGpuDod. **7.1a DONE** — loads as Display adapter, Code 0
  (commit `b3eb40f`; `Version=DXGKDDI_INTERFACE_VERSION` native, `displib.lib` stays, `DxgkDdiResetDevice`
  mandatory). **7.1b IN PROGRESS** — real VidPN + present (KMDOD-ported, `ca8af7f`); the Helios monitor
  enumerates, but `CommitVidPn`/`PresentDisplayOnly` don't fire yet (an `EnumVidPnCofuncModality`
  cofunctional-VidPN bug). See `PHASE7_DISPLAY_HANDOVER.md` §0 + the `dod-7-1a-loads` memory. Host viewer:
  `gtk,gl=on` floods `eglMakeCurrent failed` in the standalone — **UNRESOLVED** (it works in a minimal qemu
  launch on this host, and running the script as the user with the full env didn't change it; root cause
  unknown — next session). Run `tools/launch-helios-gtk.sh` as your user; default `$HELIOS_DISPLAY=gtk`
  (software, no GL — displays the 2D desktop) or `spice`.
- **7.2 — Venus over `DxgkDdiEscape`:** port the escape dispatch body back; switch the Mesa
  `vn_renderer_helios` transport to `D3DKMTEscape`. Gate: `vulkaninfo`/`helios_vk_exec` pass through the DOD
  (the Phase-5 gate, now over escape) — `vkQueueWaitIdle => 0`, `vkCmdFillBuffer` round-trip `0xDEADBEEF`.
- **7.3 — Fullscreen present:** `HELIOS_PRESENT_BLOB` + the scanout-0 arbiter (desktop ⇄ venus blob),
  double-buffered with the fence table. Gate: **fullscreen vkcube renders on the spice display, fast AND
  visually correct, zero-copy** (the metric the old WSI path failed).
- **7.4 — Harden + DXVK/VKD3D:** fix the ctx_destroy/blob leak on app crash (`DxgkDdiEscape` close /
  process-teardown CTX_DESTROY), mode-set/hotplug robustness, then bring up fullscreen DXVK/VKD3D titles.
- **7.5 (optional) — windowed/virtual-desktop add-on:** only if wanted beyond the DOD primary; evaluate an IDD
  virtual monitor whose composed frames are `SET_SCANOUT_BLOB`'d (accepting WARP composition + a copy). Not in
  the critical path.

---

## 10. Repo deltas & references

```
kmd/src/
  lib.rs            REWRITE  DriverEntry → DxgkInitializeDisplayOnlyDriver + DXGKDDI_DISPLAY_ONLY_FUNCTIONS
                            (recover shape from git 658168f:kmd/src/lib.rs)
  dod.rs            NEW      DxgkDdiPresentDisplayOnly + VidPN/pointer DDIs (ref VioGpuDod, qxldod)
  escape.rs         NEW      DxgkDdiEscape → the 6 venus ops + HELIOS_PRESENT_BLOB (body = today's ioctl.rs)
  scanout.rs        NEW      scanout-0 arbiter: desktop primary ⇄ fullscreen venus blob; SET_SCANOUT[_BLOB]
  dxgk.rs           NEW      DOD-scoped dispmprt.h bindgen (trim from 658168f:kmd/src/dxgk.rs — NO render DDIs)
  pnp.rs            EDIT     BAR map + interrupt move into DxgkDdiStartDevice/StopDevice
  ioctl.rs          DELETE/RECAST  (System-class IOCTL dispatch → escape.rs)
  virtio/
    config.rs       REVERT   KmdfConfigAccess → DxgkConfigAccess (recover from 658168f)
    gpu.rs          EDIT     add set_scanout / set_scanout_blob / resource_create_2d / transfer_to_host_2d /
                            resource_flush; keep ctx/submit/alloc_blob/map_blob
  build.rs          EDIT     re-add the DOD-scoped dxgk bindgen (NOT displib/DxgkInitialize)
  helios_kmd.inx    REWRITE  Class=Display {4d36e968-…} + DOD service install (one-time INF exception)
icd/mesa/src/virtio/vulkan/
  vn_renderer_helios.c  EDIT  DeviceIoControl transport → D3DKMTOpenAdapterFromLuid + D3DKMTEscape
  vn_wsi.c / present     EDIT  fullscreen path → HELIOS_PRESENT_BLOB(exportable swapchain blob); keep sw blit
                              only as the windowed fallback
protocol/src/
  escape.rs         EDIT     re-require HeliosEscapeHeader as the verb; add HELIOS_PRESENT_BLOB op; the six op
                            byte layouts UNCHANGED
```

**Recover the reusable old WDDM pieces from git:** `git show 658168f:kmd/src/lib.rs`,
`…:kmd/src/dxgk.rs`, `…:kmd/src/build.rs`, `…:kmd/src/virtio/config.rs`, `…:kmd/src/ddi/escape.rs`,
`…:kmd/helios_kmd.inx`. Take the **display-only / escape / config** parts; **leave** every render/AddAdapter/
`query_adapter_info.rs`/UMD piece (those are the rejected path — see the `addadapter-umd-blocker` memory).

**References:**
- `VioGpuDod` (the canonical virtio-gpu DOD, BSD): https://github.com/virtio-win/kvm-guest-drivers-windows/tree/master/viogpu/viogpudo
- `qxldod` (QXL DOD, present/VidPN reference): https://github.com/virtio-win/kvm-guest-drivers-windows/tree/master/qxldod
- WDDM Display-Only model: https://learn.microsoft.com/en-us/windows-hardware/drivers/display/supporting-the-display-only-feature
- `DxgkDdiPresentDisplayOnly`: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/dispmprt/nc-dispmprt-dxgkddi_presentdisplayonly
- `DxgkDdiEscape`: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/nc-d3dkmddi-dxgkddi_escape
- virtio-gpu `SET_SCANOUT_BLOB` / blob scanout: virtio spec §GPU + QEMU `hw/display/virtio-gpu-virgl.c`
  (`virgl_cmd_set_scanout_blob`, `virgl_cmd_resource_create_blob` + `virgl_renderer_resource_get_info`).
- QEMU spice gl / dmabuf scanout: `ui/spice-display.c`, `qemu_spice_gl_scanout_dmabuf`.
- The rejected paths (do not redo): `addadapter-umd-blocker` (Code 43 render saga), `systemclass-pivot`
  (why render WDDM was dropped), `wsi-present-plan` / `wsi-bringup-status` (why software WSI is a dead end).
