# Phase 7 Handover — Archived DOD Display Pivot

> **ARCHIVED (2026-06-07):** do not use this as the active handover. The active direction is
> System-class KMDF + DeviceIoControl + Mesa Venus; see
> [`SYSTEM_CLASS_REFOCUS_2026_06_07.md`](SYSTEM_CLASS_REFOCUS_2026_06_07.md). This file is kept as a historical
> record of the DOD/`SET_SCANOUT_BLOB` pivot and the work that followed it.

---

## 0. Prompt for the next agent  (updated 2026-06-06 — read the `dod-7-1a-loads` memory first)

Continue Helios Phase 7. The DISPLAY pivot to a **WDDM Display-Only Driver (DOD)** is **largely built and
loading**; the remaining work is one focused VidPN bug + the venus-over-escape carrier. Status:

- **Phase 7.0 gate — DONE/GO** (committed): a venus blob exports a real `DRM_FORMAT_MODIFIER(LINEAR)` dma-buf
  and the host accepts `SET_SCANOUT_BLOB` (`dmabuf_fd>=0`). On-screen pixels were never visually confirmed (host
  display-backend bugs, not Helios). The Mesa modifier-gate fix + the scanout/protocol/KMD spine are committed.
- **Phase 7.1a — DONE** (commit `b3eb40f`): the driver-model flip System-class KMDF → **WDDM Display-Only
  Driver** works — Helios binds `PCI\VEN_1AF4&DEV_1050` as **Class=Display, device Code 0** on win11 24H2, no
  BSOD. `DriverEntry`→`DxgkInitializeDisplayOnlyDriver` + the full `KMDDOD_INITIALIZATION_DATA` table;
  `DxgkConfigAccess`; reused virtio transport in `DxgkDdiStartDevice`; DOD-scoped dispmprt/d3dkmddi bindgen back
  in `build.rs` (+ `displib.lib`); INF=Display-class. Modeled on the **Microsoft KMDOD sample**
  (`github.com/microsoft/Windows-driver-samples/video/KMDOD` — the canonical DOD reference; `bdd_ddi.cxx`,
  `bdd.cxx`, `bdd_dmm.cxx`). Key gotchas (each cost a reboot): `Version = DXGKDDI_INTERFACE_VERSION` (native, NOT
  WIN8 — else QueryAdapterInfo DRIVERCAPS buffer-size check fails → Code 43); `DxgkDdiResetDevice` mandatory
  (else Code 37); `displib.lib` stays (exports `DxgkInitializeDisplayOnlyDriver`).
- **Phase 7.1b — IN PROGRESS** (commits `ca8af7f`, `5ec3ef6`, `c570de4`): real VidPN mode-management ported
  from KMDOD `bdd_dmm.cxx` into `kmd/src/vidpn.rs`, `DxgkDdiPresentDisplayOnly` un-stubbed. Helios's **monitor
  now enumerates** (a "Generic Monitor" appears on the adapter) — dxgkrnl builds a functional VidPN, runs
  `EnumVidPnCofuncModality` clean, no BSOD. **BLOCKER:** even with Helios as the SOLE/PRIMARY display (standalone
  VM), **CommitVidPn (breadcrumb 0x09) and PresentDisplayOnly (0x0D) never fire** — `HeliosStep` stays `0x08`
  (EnumVidPnCofuncModality); the standalone sits at LogonUI with no present. So `EnumVidPnCofuncModality` yields
  a cofunctional VidPN dxgkrnl won't commit.

**DO THIS FIRST — find why no commit.** Build+sign through the Windows MCP mirror, install on the standalone
target, reboot, then read `HKLM\SYSTEM\CurrentControlSet\Services\
helios_kmd\HeliosMask` (sticky OR of every DDI bit) + `HeliosStep` (= `0x0800_00<flags><paths>` from
EnumVidPnCofuncModality: paths in bits[0..8], src-modeset-assigned `0x100`, tgt-modeset-assigned `0x200`). To
make dxgkrnl actually attempt a commit on the Helios display, extend the desktop onto it
(`SetDisplayConfig` CCD / display settings). Interpret: `paths==0` ⇒ empty constraining topology (upstream);
assigns==0 ⇒ create/assign failed; assigns set but no commit ⇒ **mode-set CONTENT malformed** — compare
`add_single_source_mode`/`add_single_target_mode` field-by-field vs KMDOD `bdd_dmm.cxx` (suspects: target
`VideoSignalInfo` PixelRate / the preference union; source PrimSurfSize/VisibleRegionSize/Stride). Then expand
`MODE_TABLE` (currently one 1024x768).

**Viewing — `gtk,gl=on` eglMakeCurrent is UNRESOLVED (an open task; do not assume a cause).** `gtk,gl=on` in the
standalone floods `Gdk-WARNING eglMakeCurrent failed` (black window). Two observations only, NOT a diagnosis:
(a) `gl=on` works in a separate minimal qemu launch (q35 + one virtio-gpu-gl-pci + an Ubuntu ISO) on this host;
(b) changing `tools/launch-helios-gtk.sh` to run as the desktop user with the full session env (it no longer
sudo-re-execs with a stripped env) did NOT change the failure. Root cause unknown — the next session must figure
it out. Until then the launcher (run it as your user: `bash tools/launch-helios-gtk.sh`, restart after changes)
defaults to `$HELIOS_DISPLAY=gtk` (software, no EGL — can display the 2D desktop without
GL); use `gtk,gl=on,show-cursor=on` to reproduce the bug, or `spice` (`-spice :5930`, `remote-viewer
spice://127.0.0.1:5930`). NB: `gl=on` is needed for the venus dmabuf path (7.3), so this must be solved before
then.

**Then Phase 7.2** — venus over `DxgkDdiEscape`: the DOD's `DxgkDdiEscape` is a NOT_SUPPORTED stub; port the
body from the deleted System-class `kmd/src/ioctl.rs` (recover via git), and re-wire the Mesa
`vn_renderer_helios` transport from `DeviceIoControl` → `D3DKMTOpenAdapterFromLuid` + `D3DKMTEscape`. The async
submit + the 6 op byte layouts are unchanged. Then 7.3 fullscreen venus via `HELIOS_PRESENT_BLOB` + the
scanout-0 arbiter (the gpu.rs `set_scanout_blob`/`resource_flush` + the venus `present_desktop` plumbing exist).

**Build/test discipline:** `win` MCP (`win_cargo`/`win_meson`/`win_exec`); the driver is a same-named service
so a **reboot** is needed after each `devcon update` (live-swap → Code 14). `kmd/src/diag.rs` is the TEMPORARY
breadcrumb tracer (remove once committing cleanly). `.dod-vidpn-types.md` (untracked) holds the exact bindgen
VidPN types. Owner runs the standalone (`sudo`) + does the visual confirm; Claude can't sudo and can't see the
screen.

---

## 1. Why this shape (the verified conclusions — don't re-litigate)

- **DWM can't be GPU-accelerated here** (no WDDM render adapter; a Vulkan ICD/DXVK is invisible to DWM → WARP
  composites the desktop). A full WDDM render miniport would still need a from-scratch native D3D-to-venus UMD.
  **Rejected.** (LB3/LB4, both *confirmed*.)
- **The software WSI present is a dead end** — it issues no virtio-gpu scanout flush, so the host GL backend
  never presents it and it eats a ~3.5 s/frame host-visibility lag. **Replace it with a real scanout.** (LB2,
  mechanism *refuted* against QEMU `ui/gtk-egl.c` / `virtio-gpu.c`.)
- **One driver per FDO** → Helios must BE the display driver to own a scanout. **DOD** (Display-only) loads
  cleanly — it has none of the render AddAdapter cap contract that caused Code 43. (LB5 *confirmed*.)
- **The fast path = `SET_SCANOUT_BLOB` of an exportable venus blob** (`res->base.dmabuf_fd >= 0` from
  `virgl_renderer_resource_get_info`; venus always runs in the render-server which exports it; ANV exports
  dmabuf), displayed zero-copy by spice gl=on. (LB6 *partially-correct* — the only correction was that venus is
  never in-process, which doesn't change the plan.)

---

## 2. STEP 1 — the go/no-go gate (current driver, no DOD yet)

> **✅ GATE RESULT (2026-06-06): GO on the load-bearing criterion; proceed to the DOD.**
> A venus blob **exports a real, scannable `DRM_FORMAT_MODIFIER(LINEAR)` dma-buf** and the host
> **accepts `SET_SCANOUT_BLOB`** (`dmabuf_fd >= 0`, no `RESP_ERR_UNSPEC`). The whole scanout
> infrastructure (protocol + KMD `set_scanout_blob`/`resource_flush` + present IOCTL + ICD) works.
> **Required fix found + applied:** venus rejected DMA_BUF scanout images that weren't
> `TILING_DRM_FORMAT_MODIFIER` (`-11`); **removed the `#if !DETECT_OS_WINDOWS` gate on
> `EXT_image_drm_format_modifier`** in `icd/mesa/.../vn_physical_device.c` (~L1426) and the gate
> test now creates a `DRM_FORMAT_MOD_LINEAR` DMA_BUF image → `imgfmt2` now `OK+EXPORTABLE`.
> **DO NOT also remove the `EXT_external_memory_dma_buf`/`KHR_external_memory_fd` gate (~L1211) —
> that breaks `vkEnumeratePhysicalDevices` (-3) on Windows; the test doesn't need it advertised.**
>
> **On-screen pixels were NOT visually confirmed** — every backend hit a *host* display bug (not
> Helios): **gtk gl=on** → `eglMakeCurrent failed` (qemu-gtk GL on this multi-GPU Wayland host;
> irrelevant to the DOD's spice path); **spice gl=on** → GL binds to the QXL-*primary* console, so
> a *secondary* venus head is dropped (auto-fixed once Helios IS the primary display = the DOD).
> Owner-directed: stop debugging the host display, proceed to 7.1; the DOD's own 7.1 gate
> (desktop-on-spice) re-confirms the visual early. Debug tool: `tools/launch-helios-gtk.sh`
> (standalone qemu, gtk gl=on, native Wayland). See the `phase7-gate-status` memory for full detail.



1. **Host:** edit the win11 VM display `egl-headless` → **`gtk,gl=on`** (local, easiest to *see*) or
   **`spice gl=on`** (production). Restart the VM; `devcon update …\helios_kmd.inf "PCI\VEN_1AF4&DEV_1050"` to
   rebind Helios. Keep `virtio-gpu-gl venus=on,blob=on,hostmem=…`, render-server + udmabuf on.
2. **Guest:** add a **throwaway** `SET_SCANOUT_BLOB` path to the *current* System-class driver (a temp IOCTL or
   escape verb). Reuse `ALLOC_BLOB(blob_id = venus mem id)` + a venus render (the existing `helios_vk_exec`
   machinery produces a HOST3D blob); then call new `gpu.rs` helpers `set_scanout_blob(scanout=0, res_id, fmt,
   w, h)` + `resource_flush`.
3. **Observe.** If the venus frame appears on the gtk/spice window → **zero-copy venus scanout works, proceed to
   the DOD (§3).** On the host, confirm `virgl_cmd_set_scanout_blob` did **not** return `RESP_ERR_UNSPEC`
   ("resource not backed by dmabuf"), i.e. `res->base.dmabuf_fd >= 0`.
4. **If it fails** (`dmabuf_fd < 0`): the venus WSI/image memory isn't exportable, or ANV gave `OPAQUE_FD` only.
   Make the venus allocation chain `VkExportMemoryAllocateInfo` (dma-buf handle type; virglrenderer MR !1458
   context), re-confirm render-server + udmabuf, re-test. **Fix this before the DOD rewrite.**

---

## 3. STEP 2+ — the DOD (after the gate passes)

Per `DISPLAY.md` §3–§5 and §9. In order:

- **7.1 DOD skeleton. [7.1a DONE — loads as Display adapter, Code 0 (b3eb40f). 7.1b IN PROGRESS — VidPN +
  present built (ca8af7f), monitor enumerates, but CommitVidPn/Present don't fire; see §0 for the open
  EnumVidPnCofuncModality bug + the spice/gtk viewer notes.]** INF Class System→**Display** {4d36e968-…}; `DriverEntry` →
  `DxgkInitializeDisplayOnlyDriver` + `DXGKDDI_DISPLAY_ONLY_FUNCTIONS`; `DxgkDdiStartDevice` maps BARs + inits
  the **reused** virtio transport; `DxgkConfigAccess` (revert from `BUS_INTERFACE_STANDARD`). Implement
  `DxgkDdiPresentDisplayOnly` + the VidPN/pointer DDIs (reference **VioGpuDod** + **qxldod**). **Recover the
  shapes from git** `658168f` (`lib.rs`, `dxgk.rs`, `build.rs`, `virtio/config.rs`, `ddi/escape.rs`,
  `helios_kmd.inx`) — take the display-only/escape/config parts; **leave** every render/AddAdapter/UMD piece.
  Gate: loads as a Display adapter (Code 0); the Windows **desktop appears on spice** (DOD 2D path).
- **7.2 Venus over `DxgkDdiEscape`.** Port today's `ioctl.rs` body to a `DxgkDdiEscape` handler (header re-used
  as the verb). Switch Mesa `vn_renderer_helios.c` transport `DeviceIoControl` → `D3DKMTOpenAdapterFromLuid` +
  `D3DKMTEscape` (now works — Helios is a real WDDM/LUID adapter). Gate: `helios_vk_exec` `vkQueueWaitIdle => 0`
  + `vkCmdFillBuffer` round-trip `0xDEADBEEF`, through the DOD.
- **7.3 Fullscreen present.** Add escape op `HELIOS_PRESENT_BLOB(res_id, w, h, fmt, fence)`; KMD scanout-0
  arbiter switches desktop-primary ⇄ venus blob via `SET_SCANOUT_BLOB`; double-buffer with the fence table.
  Venus WSI fullscreen path emits `HELIOS_PRESENT_BLOB` instead of the GDI blit. **Gate: fullscreen vkcube on
  spice, fast AND visually correct, zero-copy.**
- **7.4 Harden + DXVK/VKD3D.** CTX_DESTROY/blob-free on escape close / process teardown (the leak that makes
  `vkCreateInstance` fail -1 after crashes); mode-set/hotplug; then fullscreen DXVK/VKD3D titles.

---

## 4. Gotchas / non-obvious facts (carried forward)

- **No scanout-arbitration fight** (unlike the old `wsi-present-plan` worry): Helios IS the DOD, so it owns
  scanout 0 outright and just mode-switches it. `max_outputs=1` is fine.
- **`D3DKMTOpenAdapterFromLuid` now works** — a DOD is a real WDDM adapter with a LUID (resolves ARCH.md §12).
- **Desktop is WARP-composited + uploaded** per dirty rect — that's inherent and fine for 2D; the zero-copy win
  is only for venus content. Windowed 3D stays limited (round-trips to WARP). State this; don't promise it.
- **Don't bring back** `displib.lib`, `DxgkInitialize`, `d3dkmddi.h` render DDIs, `query_adapter_info.rs`, the
  cap-burst handlers, or the stub UMD — those are the rejected render path (`addadapter-umd-blocker`).
- **Keep** the Phase-4e async submit + the `vn_queue.c` fence fixes; don't regress the `helios_vk_exec` gate.
- **Build/test discipline:** KMD `infverif VALID` + `devcon` rebind; ICD via `win_meson`. GUI present only via
  `schtasks /it` into the interactive session (session-0 SSH can't present). Commit ICD (submodule) + the parent
  gitlink as scoped commits.

---

## 5. Historical Reading Order

For active work, read `SYSTEM_CLASS_REFOCUS_2026_06_07.md` and `ARCH.md` first.

For historical DOD/display investigation only:

1. `DISPLAY.md` (archived spec — the old design + the §8 gate + §10 repo deltas).
2. This file (the old ordered steps).
3. Prior memory names are historical breadcrumbs only: `display-pivot`, `systemclass-pivot`,
   `addadapter-umd-blocker`, `phase5-backend-status`, `mesa-venus-icd-port`, `fence-feedback-hack`,
   `wsi-bringup-status`.
4. Git `658168f` for reusable old WDDM display/escape/config code if a future DOD experiment resumes.
