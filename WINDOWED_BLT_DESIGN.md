# WINDOWED_BLT_DESIGN.md — Legacy BLT-model windowed present on Helios (priority #1)

**Status:** root-caused end-to-end (2026-07-08, 34th session). This doc is the
design + implementation plan for the fix. It is grounded in the Microsoft driver
docs under `windows-driver-docs-research-only/windows-driver-docs-pr/display/`
(cited by `file:line`) and the current code (cited by `path:line`). Companion
memory: `windowed-blt-occluded-root-34th-session`.

---

## 1. TL;DR

Legacy **`DXGI_SWAP_EFFECT_DISCARD`** (BLT-model) **windowed** D3D apps render
transparent on Helios: DXGI returns **`DXGI_STATUS_OCCLUDED`** on every `Present`
and never blits to the window. **Flip-model composites fine.** DXUT/FaceWorks and
older 3DMark default to the BLT model → this is the whole "windowed D3D transparent"
defect.

**Root cause (grounded):** a legacy DISCARD windowed present needs a **real, active
VidPn output somewhere in the present path**. Our stack has none:
- The only monitor is the **indirect Looking Glass IddCx** display, which DXGI
  enumerates on the **Helios render adapter** as a **runtime-synthesized *logical*
  (facade) output** with **no VidPn source** (`NumOfSources=0`).
- There is **no real WDDM display adapter anywhere** to cross-adapter-present to.

Flip works because it rides DWM's shared-surface composition path, which never
needs a scanout output. DISCARD needs one; the facade has none → occluded.

**Fix direction:** introduce a **real (virtual) VidPn output** into the path — give
Helios a genuine VidPn source/target (the RDP / display-miniport model) so the
window's monitor resolves a real output for same-adapter DISCARD redirection.
Alternative: coerce DISCARD onto the flip path (harder; DXGI owns swap-effect).

**Do Stage 0 (the WARP A/B) FIRST** — it empirically confirms whether a presentable
render adapter is sufficient before building the display half.

---

## 2. What is proven (evidence chain — do not re-derive)

All via `tools/d3d11_triangle.cpp` (minimal windowed D3D11: clears blue, draws a green
triangle, self-reads the backbuffer before Present; topmost so captures are unoccluded)
+ `helios_capture_faceworks` on-screen ground truth.

| run | swap | Present hr | on-screen |
|---|---|---|---|
| default | FLIP_DISCARD | `0x00000000` S_OK | ✅ blue + green triangle |
| default / adapter[1] / adapter[0] | BLT (DISCARD) | `0x087a0001` OCCLUDED | ❌ transparent |

- App **renders correctly** every time (self-readback blue+green) — pure compositing bug.
- **Falsified:** alpha, IDD path, two-memory-split, phantom-LUID/EnumOutputs race,
  adapter selection, and the cross-adapter cap (see §5.3).
- With the `CrossAdaptCaps` cap ON, DXGI *does* create per-window 1280×720 redirection
  surfaces and the UMD `pfnPresent`→`pfnPresentCb` returns S_OK — but DXGI still
  occludes and recreates the surface every frame (`dst=0x0, hDstRes=0x0, copied=false`;
  UMD log `C:\ProgramData\Helios\umd-<pid>.log`). The redirect never resolves a real output.

---

## 3. Topology (grounded)

Two paired adapters (D3DKMT `KMTQAITYPE_ADAPTERTYPE`, via `tools/adapter_type_probe.cpp`):

| Adapter | D3DKMT type | NumOfSources | DXGI |
|---|---|---|---|
| **Helios render** (virtio-gpu, our KMD) | `Render=1, Display=0` | **0** | named "Helios", **outputs=1** (facade `\\.\DISPLAYn`, attached) |
| **Looking Glass IddCx** (`ROOT\DISPLAY\0000`) | `IndirectDisplay=1, Display=1` | **1** (real source) | named "Helios" too (inherits render-adapter desc), outputs=0 |

- The IDD selects **Helios** as its render adapter via `IddCxAdapterSetRenderAdapter`
  (`LGIdd/CIndirectDeviceContext.cpp:325`, gated by `HKLM\SOFTWARE\LookingGlass\IDD\HeliosRenderAdapter=1`, currently **1**). DWM composes the desktop on Helios; the IDD *captures* the composed swapchain (`EvtIddCxMonitorAssignSwapChain`).
- MS docs confirm this is **expected**: the DX runtime enumerates the ID monitor on
  the *render* adapter (`iddcx1.4-updates-for-console-and-remote-idds.md:46-47`), and a
  render-only adapter is a legitimate render adapter (`iddcx1.6-updates.md:29` WARP;
  `gpu-paravirtualization.md:61`). The render-only adapter's output is **runtime-added
  and logical**: *"The Direct3D runtimes add a logical display output to the [render-only
  adapter] when an application decides to use it."* (`gpu-paravirtualization.md:70`).

---

## 4. Why flip works and DISCARD doesn't (the mechanism + the hybrid-GPU sanity check)

- **Flip / DWM-on windowed path** = a **shared-surface `BltDXGI` into DWM's composition**
  (`dxgi-presentation-path.md:17`, `specialized-monitors-compositor.md:85`). It needs no
  scanout output on the presenting adapter → works on Helios.
- **Legacy DISCARD windowed** = needs a **real presentable VidPn output** for the window's
  monitor, either same-adapter or cross-adapter to a real display adapter. There is **no
  documented DISCARD-to-an-IddCx-virtual-monitor path** — it is an *undocumented/unsupported
  combination* (agent-A doc sweep found neither support nor a named limitation).

**Hybrid-dGPU sanity check (why a monitor-less dGPU is NOT a counter-example):** a dGPU
with no monitor handles DISCARD fine — but **never via its own facade output**. Either
(a) the window is on the iGPU's panel and the present is **cross-adapter to the iGPU's
real output** (DWM composes on the iGPU), or (b) an external monitor is plugged into the
dGPU giving it a **real** VidPn source. A monitor-less dGPU **never has a window on its
facade output** (no monitor there ⇒ no window there), so DISCARD-to-a-facade never happens.
**Our config is the pathological one the hybrid model structurally avoids:** our *only*
monitor sits on Helios's facade output (so windows *do* live on it) **and** there is no
real display adapter to cross-adapter to. That is precisely why enabling the cross-adapter
cap (§5.3) minted redirection surfaces that had nowhere real to land.

**Confirming real-world config:** headless/RDP Windows *does* support legacy DISCARD apps —
through a **real WDDM display miniport with a (virtual) VidPn source** (the RDP display
driver), **not** IddCx. That is the model the fix should follow.

---

## 5. Options

### 5.1 Option A — give Helios a real (virtual) VidPn output  ★ recommended primary
Make Helios expose a genuine VidPn source + child monitor target so the window's monitor
resolves a **real** output (RDP/display-miniport model) instead of the runtime facade.
DISCARD's same-adapter redirection then has a real output to present to.
- **Pro:** the only option the docs positively support for legacy windowed present;
  the RDP precedent shows it works for a purely-virtual display.
- **Con:** contradicts the render-only (MCDM) charter; risk to boot stability; and it
  raises the **IddCx-coordination question** (§6.3): the IddCx already represents the
  monitor — a Helios VidPn source must not create a *second* rival monitor.

### 5.2 Option B — coerce DISCARD apps onto the flip/composition path
Get the DISCARD swapchain to use DWM's shared-surface path (which already works).
- **Pro:** no topology/architecture change; matches the documented windowed contract.
- **Con:** the **system DXGI owns the swap-effect decision** — there is no clean UMD/KMD
  lever to convert DISCARD→flip. Would rely on app-compat/registry shims (fragile,
  per-app) or on discovering exactly what DXGI's DISCARD occlusion check queries and
  satisfying it minimally (which converges back onto Option A's minimal form).

### 5.3 Option C — re-point the render adapter (WARP)  → diagnostic only, not a solution
`IddCxAdapterSetRenderAdapter` can move DWM's composition to WARP/BasicRender, which the
OS treats as a *presentable* software adapter (`idd-evtiddcxmonitorassignswapchain-error-handling.md:62`).
- Loses Helios hardware acceleration → **not** a shippable fix.
- **But it is the Stage-0 diagnostic** (§6.1): if DISCARD composes with WARP as render
  adapter, "a presentable render adapter is sufficient" is proven and Option A is validated.

### 5.4 Falsified / rejected
- **Cross-adapter resource cap** (`DECLARE_CROSS_ADAPTER_RESOURCE`/`CrossAdaptCaps`): the
  present is **same-adapter**, not cross-adapter (§3–4). The cap only made DXGI mint
  redirection surfaces with no real display adapter to land them on. Knob shipped
  (v22.22.62.0) but **reverted to 0**; keep for the record, not the fix.

---

## 6. Recommended plan (diagnostic-first, staged, reboot-frugal)

> **✅ STAGE 0 DONE (2026-07-08) — OPTION A VALIDATED.** Disabled the Helios PnP device →
> Windows fell back to **WARP** (Basic Render Driver, SOFTWARE, `outputs=1`) as both app-render
> and IddCx-render adapter → BLT triangle **Present=S_OK and composited on screen** (blue+green,
> paintcap-confirmed). A *presentable* render adapter makes legacy DISCARD work; render-only
> Helios doesn't. → **Commit to Option A (Stage 1).** Trap: the session-0 `adapter_type_probe`
> hung on DXGI EnumOutputs while Helios was disabled — use session-1 tasks during the disable.

### Stage 0 — WARP/fallback A/B (confirm the fix class before writing display code) — DO FIRST
Goal: does DISCARD compose when Helios is entirely out of the path? **Preferred method:
temporarily DISABLE the Helios PnP device** — fully reliable (removes Helios by every means;
setting `HeliosRenderAdapter=0` only changes the IDD's *preference* and doesn't stop apps/DWM
from using Helios by other paths). Recovery is safe: `win_exec` (SSH, session 0) is independent
of the display, so Helios can always be re-enabled even if the desktop composition blips.
1. Disable Helios: `Disable-PnpDevice -InstanceId 'PCI\VEN_1AF4&DEV_1050&...' -Confirm:$false`
   (or `pnputil /disable-device`). Windows must fall back for both DWM composition and app
   rendering.
2. Determine the fallback (re-run `helios_atp` / `ccd_out`): which adapter is now the IDD's
   render adapter + the app's default? This is itself informative —
   - **WARP-render** (Basic Render Driver, `Render=1, Display=0`) is *also* render-only/facade;
   - **Microsoft Basic Display Adapter** (basicdisplay.sys on the QEMU VGA — note the ghost
     QEMU/DEFAULT monitors seen in probes) is a **real display miniport with a real VidPn source**.
3. Run the BLT triangle (`helios_triangle`, `default blt 20`) + capture (works off the composed
   framebuffer regardless of the LG feed).
   - **Composites** ⇒ a real/presentable output resolves DISCARD → **Option A** validated (make
     Helios present a real VidPn output). Note *which* fallback adapter made it work.
   - **Still occluded** (even on a real display adapter fallback) ⇒ DISCARD-to-an-IddCx-monitor
     is fundamentally unsupported → pursue **Option B** or scope DISCARD out.
4. **Re-enable Helios:** `Enable-PnpDevice ...`; confirm CM_PROB_NONE + hw-accel desktop back
   (paintcap). If disable wedges the desktop, re-enable via SSH; reboot only if that fails.
> Owner-gated (disables the live composition adapter — desktop will blip to software/basic
> composition). Reversible via SSH. No KMD rebuild/reboot needed. Lighter (less reliable)
> variant if a full disable is too disruptive: `HeliosRenderAdapter=0` + restart the IDD
> device (`ROOT\DISPLAY\0000`), confirm the LGIdd log shows a non-Helios render LUID.

### Stage 1 — minimal display surface on Helios (Option A), incremental + gated
Do **not** build a full display miniport up front. Add the display half **incrementally**,
testing DISCARD after each step, to find the *minimal* surface that un-occludes it. Each
step is a KMD change → rebuild (`win_build_kmd`) → **reboot** (KMD image loads at boot).
Reference template: the **viogpu display miniport** (`mvisor-win-vgpu-driver` / kvm viogpu)
implements exactly this VidPn surface for virtio-gpu — mirror its DDI bodies.

Order (each behind a `DisplayHalf` service knob, default 0, so boot stays safe — see §7):
1. **Advertise 1 source + 1 child** — `start_device.rs:231-232`:
   `*number_of_video_present_sources = 1; *number_of_children = 1;`
   Populate `dxgkddi_query_child_relations` (`start_device.rs:314`) with one
   `DXGK_CHILD_DESCRIPTOR` (`ChildDeviceType = TypeVideoOutput`,
   `HpdAwareness = INTERRUPTIBLE` or `POLLED`, `AcpiUid=0`, a stable child uid), and
   `dxgkddi_query_child_status` (`:326`) → `DXGK_CHILD_STATUS` HPD present/connected.
   Register **`DxgkDdiGetChildContainerId`** (currently ABSENT from the `lib.rs` table)
   returning a stable container id. Provide an EDID from `dxgkddi_query_device_descriptor`
   (`:340`, currently NOT_SUPPORTED) — or return NOT_SUPPORTED to let the OS use a default
   monitor. This is the step that flips the adapter from render-only to display-capable.
2. **VidPn topology DDIs** — implement in `ddi/display.rs` (all currently NOT_SUPPORTED):
   `is_supported_vidpn` (`:234`) → set `*IsVidPnSupported = 1` after basic validation;
   `enum_vidpn_cofunc_modality` (`:255`) → pin each path's modality (iterate the VidPn
   topology, set the pivot mode); `recommend_functional_vidpn` (`:247`) / `recommend_monitor_modes`
   (`:318`) → offer the IddCx monitor's mode(s); `commit_vidpn` (`:279`) →
   `STATUS_SUCCESS`; `update_active_vidpn_present_path` (`:291`) → `STATUS_SUCCESS`;
   `query_vidpn_hw_capability` (`:326`) → report no HW enhancements.
   *(These four are the ones the OS drives to stand up a source; the viogpu bodies are
   the concrete reference. This is the bulk of the work — VidPn iteration is fiddly.)*
3. **Scanout accept (no-op)** — `set_vidpn_source_address` (`:307`) records the primary
   allocation handle but does NOT physically scan out (the pixels still reach the host via
   the venus composition the IddCx captures); `set_vidpn_source_visibility` (`:268`) →
   `STATUS_SUCCESS`. `MaxQueuedFlipOnVSync`/`SupportDirectFlip` become meaningful — revisit
   `query_adapter_info.rs:247`.
4. **Test DISCARD after each of 1→3.** The likely-sufficient point is (1)+(2): once DXGI
   sees an active VidPn source on Helios for the monitor, the DISCARD occlusion check should
   pass. Stop at the first step where the triangle composites.

### Stage 2 — resolve the IddCx-coordination question (§6.3) and re-verify the desktop
Ensure the new Helios VidPn source does not create a **second** monitor that fights the
IddCx's. Re-verify the hw-accel desktop + flip apps still work (paintcap), cold-boot.

---

## 6.3 THE hard open question — Helios VidPn source vs the IddCx monitor
The IddCx already *is* the monitor (`IddCxMonitorArrival`). If Helios also reports a child
monitor + VidPn source, the OS may enumerate **two** monitors for one physical display.
Three sub-approaches, in rising order of change (decide in Stage 1, informed by Stage 0):
- **(i) Headless source, no child monitor:** advertise a VidPn *source* whose target is the
  *existing* IddCx monitor (no new child). Cleanest if WDDM lets a render adapter own a
  source whose target lives on the paired IddCx — **verify against viogpu/docs; may not be
  expressible.**
- **(ii) Helios owns the monitor; IddCx captures Helios's scanout:** Helios becomes the
  display miniport for the LG monitor; the IDD's `SetVidPnSourceAddress` primary is what LG
  captures. Requires re-pointing the IDD capture from "DWM composition swapchain" to
  "Helios primary" — **a real IddCx change**, possibly dropping the IddCx swapchain model.
- **(iii) Full merge (RDP model):** Helios is a single render+display miniport; LG captures
  its scanout via the host (venus), no IddCx. **Largest rearchitecture; cleanest end state.**

Recommend prototyping **(i)** first (least invasive); fall back to **(ii)**.

---

## 7. Reversibility / safety (non-negotiable — prior display work crash-looped)
- Gate the entire display half behind a **`DisplayHalf`** REG_DWORD service knob
  (default 0), mirroring `CrossAdaptCaps` (`query_adapter_info.rs:457`) and the working
  `read_config_dword` (verified this session). Default 0 ⇒ boot behaves exactly as today.
  `start_device` reads it once; every new DDI checks it and falls back to NOT_SUPPORTED
  when 0. This lets each increment A/B via `reg` + reboot without risking an unbootable
  default.
- KMD image loads only at BOOT → each Stage-1 increment needs an owner-consented reboot.
  Bump the version at all three sites (`win_build_kmd` handles it) — INF/FILEVERSION
  mismatch = FAILED_ADD 0xc0000182.
- A panic in any VidPn DDI = silent graphics deadlock. No `panic!`/`todo!`; return only
  legal NTSTATUS from each DDI's documented set.

---

## 8. Tooling already built (reuse; don't rebuild)
- `tools/d3d11_triangle.cpp` — the confound-free repro (topmost). Build: WinLibs g++ at
  `C:\Users\Rupansh\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin\g++.exe`
  (NOT on SSH PATH; invoke by full path; output to LOCAL C:\; kill a running `d3d11_triangle`
  first — the exe locks). Task `helios_triangle` (session-1 Interactive; I created it) runs
  `Z:\tmp\tri_run.cmd [adapter] [swap] [secs]` → `C:\Users\Rupansh\d3d11_triangle.txt`.
  Re-register the task's `-Argument` to A/B. Capture: `helios_capture_faceworks` →
  `Z:\tmp\screen_copy.png`. NEVER `win_exec` a windowed app (blocks in session 0).
- `tools/adapter_type_probe.cpp` — D3DKMT adapter type flags (task `helios_atp` →
  `C:\Users\Rupansh\adapter_type_probe.txt`). Re-run after any adapter/LUID change (LUIDs
  churn every adapter-restart).
- `tools/ccd_adapter_probe.cpp` — CCD (authoritative monitor↔adapter). `ccd_out.txt`.
- KMD diag: set `DiagLevel=1` for fresh `record()` S-ring; caps in `0x01D1..0x01D8`; the
  `GdiM`/`AlcC`/`AE*` records use `record_named` (always written, boot-fresh). Beware the
  **persist-across-boots** trap — verify a record moved THIS boot.
- UMD trace: `HKLM\SOFTWARE\Helios!UmdTrace=1` adds per-frame `trace_line!`; log always at
  `C:\ProgramData\Helios\umd-<pid>.log` (note: the `DXGI Present:` log field `presentCb=`
  prints `present_hr`, NOT a pointer — `umd/src/forward.rs:7976`).

## 9. Current deployed state
- KMD **v22.22.62.0** (`CrossAdaptCaps` knob, reverted to 0) — commit-worthy.
- Uncommitted: `kmd_render/{build.rs,Cargo.make.toml,src/ddi/query_adapter_info.rs}`,
  `tools/d3d11_triangle.cpp` (topmost), `tools/adapter_type_probe.cpp` (new).
- `HeliosRenderAdapter=1` (DWM on Helios). Desktop healthy.

## 10. Open questions to answer in the next session
1. **Stage 0 result** — does WARP-render compose DISCARD? (Decides A vs B.)
2. **Does WDDM allow §6.3(i)** — a render adapter owning a VidPn source whose target is a
   paired IddCx monitor, with no new child? (Read viogpu display miniport + WDDM VidPn docs.)
3. **Minimal DDI set** — is (1)+(2) of Stage 1 enough, or is a real `SetVidPnSourceAddress`
   scanout required? (Incremental testing answers this.)
4. Does adding a Helios child monitor create a duplicate desktop monitor? (Stage 2.)
