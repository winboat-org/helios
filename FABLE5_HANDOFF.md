# Helios vGPU — Handoff to Fable 5 (2026-06-26)

> **★ 2026-07-02 UPDATE (supersedes §1–§4 below; see the `idd-code43-double-delete-rootcause`
> memory for the full evidence chain): the one question is ANSWERED and FIRST FRAMES FLOW.**
> Code 43 = the IddCx **watchdog** deliberately terminating WUDFHost after ~1–2 min of no frame
> progress (both prior "verifier failure" theories were this watchdog; the 06-25 leak-fix also
> added a double-delete — IddCx owns the swapchain on a failure return, MS-docs-confirmed, fixed).
> The first swapchain of a boot is stillborn (render/indirect pairing-instance churn — LUID
> f962→77ad — abandons it; DWM meanwhile presents fine into the live instance). With LGIdd
> **20.23.27.300** (ownership fix + strict-fail→ABANDON) plus **one post-boot
> `pnputil /restart-device ROOT\DISPLAY\0000`**, SetDevice succeeds, frames are acquired, the
> **IDD sits at Code 0** and the LG client receives 1920x1080 BGRA frames end-to-end.
> Remaining: frame content likely black (venus §4 coherence — verify visually), boot-time
> self-convergence, KMD illegal-status DDIs (CollectDbgInfo, QueryAdapterInfo unknown-type,
> 197× C0000001), UMD deallocate_resource 0x80070057.
>
> **★★ LATER SAME DAY: black-frame root cause found (un-executed GDI ops) — read
> `HANDOFF_GDI_BLACKFRAME.md`, which is now the active handoff.**

**This is the single authoritative status doc.** Where any other doc (`WDDM_SYNC_REDESIGN.md`,
`WDDM_SYNC_M3_M4_HANDOFF.md`, `IDD_BLACK_FRAME_HANDOFF_2026_06_25.md`, `HANDOFF_NEXT_SESSION.md`)
disagrees with this one, this one wins — they are historical/superseded and carry banners saying so.
Read this + the "SKEPTICAL SYNTHESIS" at the top of the `venus-enum-adapter-probe-regression` memory,
then form your own view. **You are explicitly asked to bring fresh eyes** (§6) — the last two sessions
each over-committed to a theory and churned; do not inherit their tunnel vision.

## 0. The goal (locked, do not re-litigate)

DWM composites the whole Windows desktop **on the Helios WDDM render adapter** (venus → host GPU),
and the Looking Glass **IDD** (indirect display driver) displays those composed frames in the Looking
Glass client on the Linux host. Helios is a **render-only** WDDM adapter (no VidPN sources); the IDD
is a separate `ROOT\DISPLAY` device. The OS is supposed to pair them (IDD's frames are composed by DWM
on Helios). Do NOT pivot to per-app venus — that was already possible and is explicitly not the goal.

## 1. TL;DR — the one question

Everything currently reduces to: **why does the Looking Glass IDD device fail its PnP *post-start*
(`CM_PROB_FAILED_POST_START`, Code 43) whenever Helios is present, so the OS removes it, which abandons
the indirect swapchain, which makes `IddCxSwapChainSetDevice` return `0x887A0026` and no frames flow?**
It is NOT a sync problem, NOT a keyed-mutex problem, NOT (as far as verified) a "delivery/surface"
problem — those are downstream or were dead ends. See §2/§3/§4. But treat even *this* framing
skeptically (§6) — the initiating cause of the post-start failure was never actually pinned down.

## 2. Verified ground truth (high confidence — checked live this session)

- **Helios PCI device = Code 0** (`CM_PROB_NONE`), KMD **v22.22.33.0** deployed
  (DriverStore `helios_kmd_render.inf_amd64_1e1c0ddccb992c30`). Healthy.
- **M1 (WDDM 3.2 + GpuMmu raise) works.** `data.Version = DXGKDDI_INTERFACE_VERSION_WDDM3_2`;
  DRIVERCAPS `WDDMVersion=DXGKDDI_WDDMv3_2` + `MemoryManagementCaps |= VirtualAddressingSupported |
  GpuMmuSupported`; `GetNodeMetadata.GpuMmuSupported=1`. Gated by `RAISE_WDDM_3_2_GPUMMU` in
  `kmd_render/src/ddi/query_adapter_info.rs`. No boot-loop; the historical InitDmaPools/0x10E crashes
  did not recur (the decoratively-implemented GpuMmu DDIs held).
- **`D3D11CreateDevice(Helios) = S_OK`** (FL 0xa000), no revision mismatch — the venus stack is intact.
- **Monitored fences now accepted** (`tools/d3dkmt_sync_probe.cpp`): `D3DDDI_MONITORED_FENCE` private +
  shared-NT succeed with non-NULL CPU VA + GPU VA; shared-**KMT** rejected `0xc000000d` (monitored
  fences are NT-share-only). **The keyed-mutex mechanism works** (`tools/d3d11_keyed_mutex_probe.cpp`:
  full producer/consumer `AcquireSync`/`ReleaseSync` cycle all `hr=0`, in-process cross-device).
- **IDD device `ROOT\DISPLAY\0000` = Status Error, `CM_PROB_FAILED_POST_START` (Code 43).** No frames
  (LG client loops "waiting for the host to restart"); WUDFHost has no `helios_umd` loaded (swapchain
  inactive).
- **Helios now enumerates ONCE in DXGI under WDDM 3.2** (was twice under 1.3). May matter for the
  IDD↔render-adapter pairing (the IDD's `AssignSwapChain renderAdapter LUID` used to be Helios's 2nd
  enum).

## 3. The current best causal model (medium confidence — the mechanism, not the first-cause)

From ntoseye backtraces (last session, dxgkrnl has full public symbols; Helios/IndirectKmd do not):

```
IDD (ROOT\DISPLAY\0000) fails post-start  →  Code 43
   → PnP power arbiter: DpiPowerArbiterThread → DxgkRemoveAdapter → DXGADAPTER::Stop
     → ADAPTER_DISPLAY::Stop → ReleaseAllVidPnSourceOwners(Helios render adapter)
       → RemoveVidPnOwnership → BLTQUEUE::Reset → (BltQueueWorker) → SwapChainAbandonInternal
         → DXGSWAPCHAIN::MarkAbandoned
   → consumer IddCxSwapChainSetDevice sees the abandoned indirect swapchain → 0x887A0026
```

Two things are solidly established here: (a) **no keyed-mutex acquire ever happens** —
`dxgkrnl!DxgkAcquireKeyedMutex2` / `DXGKEYEDMUTEX::AcquireSync` breakpoints never fired; only
`MarkAbandoned` fired. So `0x887A0026` ("keyed mutex was abandoned") is just how IddCx *surfaces an
abandoned swapchain* — it is not an acquire race. (b) The abandon originates in **adapter removal**,
i.e. the IDD adapter being stopped — which is downstream of the IDD's post-start failure. **What was
never pinned down: the actual first-cause — why the IDD reports post-start failure.**

## 4. RULED OUT / dead ends — do NOT repeat these (with why)

- **The entire "WDDM sync redesign"** (monitored fences advancing on completion, host VkSemaphore
  bound to the resource, `VK_KHR_external_semaphore_win32` emulation, M1–M5 / M2'–M4'). Sync is not the
  blocker: the keyed-mutex probe passes, no acquire fires, and host-GPU ordering already exists (venus
  `submit_venus` blocks until "host-visible-complete"; DXVK `ReleaseSync` already does
  `WaitForResource(SynchronizeAll)` before releasing). `WDDM_SYNC_REDESIGN.md` /
  `WDDM_SYNC_M3_M4_HANDOFF.md` are SUPERSEDED (banners added). **Do not rebuild DXVK for a keyed-mutex
  fix** — the IddCx swapchain keyed mutex is dxgkrnl/OS-owned, and the IDD fails inside
  `IddCxSwapChainSetDevice` before our UMD ever opens the surface.
- **"IDD has 0 active display paths / make SetDisplayConfig / the display topology work."** This was
  the last session's terminal conclusion and it is a **session-0 artifact**: `win_exec` = SSH =
  session 0, whose `GetDisplayConfigBufferSizes` / `Screen.AllScreens` always reports 0 paths /
  `WinDisc 1024x768` regardless. It also inverts cause/effect — a Code-43 IDD has no active path *by
  definition*, and `AssignSwapChain` *does* fire (only happens for an active monitor). Display state
  MUST be read via WMI or a genuine **session-1** probe. LGIdd is a *console/local* IDD (not remote);
  `SetDisplayConfig` is a remote-IDD-only mechanism (returns `ERROR_GEN_FAILURE` on console) — the
  console IDD monitor is supposed to be OS-auto-activated on arrival (which is what happens
  gpu-gl-OUT). Do not chase SetDisplayConfig.
- **KMD present-blit in `dxgkddi_present`** — the DDI never fires on a render-only adapter.
- **Advertising native fences / `DXGK_VIDSCHCAPS::NativeGpuFence`** — OS-feature-gated; advertising
  unprovoked fails AddAdapter. Leave off.
- **Bumping the adapter to WDDM 2.0** (not 3.2) → `STATUS_REVISION_MISMATCH` (too old for the 24H2
  UMD). 3.2 is correct and is what's deployed.

## 5. KEPT — real progress from the last two sessions

- **M1 (WDDM 3.2 + GpuMmu), KMD v22.22.33.0** — Code 0, monitored fences, keyed-mutex mechanism.
  Keep the deployed `.33.0` KMD. (`RAISE_WDDM_3_2_GPUMMU=true` in `query_adapter_info.rs`.)
- **LGIdd swapchain-leak fix (deployed v18.56.15.903)** — a REAL UMDF object-lifetime verifier bug:
  `IDDCX_SWAPCHAIN` was leaked on the `AssignSwapChain` failure paths (D3D11-init / SetupLGMP /
  `Start()`-fails), tripping `FxVerifierDriverReportedBugcheck` → WUDFHost terminated. Fixed in
  `LookingGlass/idd/LGIdd/{CIndirectMonitorContext,CSwapChainProcessor}.cpp`
  (`WdfObjectDelete((WDFOBJECT)swapChain)` + return `STATUS_GRAPHICS_INDIRECT_DISPLAY_ABANDON_SWAPCHAIN`
  on the failure paths; `~CSwapChainProcessor` deletes `m_hSwapChain` if the thread never started).
  This removed the WUDFHost crash but did NOT fix Code 43 (Code 43 predates it). **NOTE (a fresh-look
  item, §6): this fix changed AssignSwapChain to actively *abandon* the swapchain on failure — verify
  it isn't part of a self-reinforcing failure loop.**
- **IDD-process logging is now visible** — UMD + ICD logs write per-pid `C:\ProgramData\Helios\
  umd-<pid>.log` / `helios_icd_*.log` (the restricted IddCx WUDFHost process can't write
  `C:\Windows\Temp`). This makes the IDD process's `open_resource`/`OpenDdiTexture2D` outcomes
  observable. Deploy caveat: WDDM loads the **DriverStore** copy of the UMD, not the ProgramData copy
  — use rename-aside to replace a mapped DLL.

## 6. ★ FRESH LOOK — reconsider these; they are where the last two sessions may have gone wrong

You are asked to look at what could have been missed. Concrete angles, roughly by value:

1. **The debugger perturbs the very thing being measured.** ntoseye/KD stretches the guest clock
   (last session explicitly noted the `AssignSwapChain`→`SetDevice` gap widening to **~3 min**, which
   is how it "caught DWM dropping the swapchain in the window"). That DWM-drops-swapchain-in-the-window
   observation may be a **KD artifact**, not real behavior. **First reproduce the failure with NO
   debugger attached** and confirm Code 43 / no-frames without KD before trusting any KD-derived timing.
2. **WARP is also render-only and (per the memory) works with the IDD — Helios doesn't. DIFF THEM.**
   This is the highest-value angle: it disproves "a render-only adapter can't back an IDD" and points
   at a *specific Helios capability gap*. Enumerate exactly what the OS asks of / gets from WARP vs
   Helios during the IDD pairing + post-start (caps, VidPN target/source exposure,
   `DriverSupportsCddDwmInterop`, cross-adapter, the DXGI enum shape). Whatever WARP has that Helios
   lacks is likely the answer.
3. **Reconcile a real contradiction: the IDD *did* acquire frames earlier.** Pre-session memory
   records the IDD acquiring `1920x1080` frames (black) — `CSwapChainProcessor::SwapChainNewFrameD3D11`,
   `frame=4`, then idle `polls=300`. If the IDD can start a swapchain and acquire frames at all, "always
   fails post-start" is incomplete — is Code 43 a **regression** (from M1's WDDM-3.2 raise? the DXGI
   single-enum change? the LGIdd leak-fix's new ABANDON behavior? a specific boot ordering?), or is the
   failure **intermittent/conditional**? Nail whether frames EVER flow on the current `.33.0` + leak-fix
   build before assuming a hard structural block.
4. **Establish the true first-cause of the post-start failure with the event log FIRST, not
   backtraces.** `Microsoft-Windows-Kernel-PnP` + `DriverFrameworks-UserMode/Operational` + an IddCx
   WPP trace (`logman ... {D92BCB52-FA78-406F-A9A5-2037509FADEA}`) will show *who* reports the failure
   and *what status* — cheaper and less artifact-prone than the elaborate dxgkrnl backtrace chains the
   last sessions built (which never actually reached the initiating event). Is it LGIdd returning
   failure, IddCx on its behalf, or dxgkrnl removing the adapter for its own reason?
5. **Question the whole render-only-IDD-pairing shape.** The `wddm-hwaccel-desktop-is-the-goal` memory
   locks "DWM composites on Helios; IDD reads it," and rejected the hybrid/cross-adapter model. But if
   the OS structurally cannot keep an IddCx indirect-display adapter alive when its only render pairing
   is a render-only vGPU, that constraint deserves to be re-examined against evidence (not assumed
   either way). Be careful — the user has been firm about not abandoning compositable-WDDM — but a
   fresh, evidence-based read of "what does IddCx *require* of the render adapter" is fair game.
6. **Don't assume the ntoseye causal chain is the whole story.** Both prior sessions built increasingly
   elaborate chains and each declared a "definitive root cause" ~5 times, then walked them back. The
   simplest explanations (an enumeration/resource conflict when Helios is present; an EDID/mode the OS
   rejects; the LG host not feeding a valid config) were not exhausted. gpu-gl-OUT works cleanly — a
   careful **diff of the working vs broken configuration** may be more productive than deeper KD dives.

## 7. Tooling — build / deploy / logs / ntoseye

- **KMD:** `win_cargo kmd_render ["make","--makefile","Cargo.make.toml"]` (bump the version in
  `build.rs` + `Cargo.make.toml`, currently `22.22.33.0`) → `install-helios-kmd.ps1
  -AllowRebootRequired` (binds, marks restart-required, does NOT restart the in-use device) →
  `Restart-Computer -Force` to activate. Backups: `C:\ProgramData\HeliosDeployBackups\`.
- **ICD (venus):** `win_meson ["compile","-C","C:\\Users\\Rupansh\\helios-mesa-build"]` → deploy by
  **rename-aside** over `C:\ProgramData\HeliosVulkan\vulkan_virtio-879f56b158e4.dll`. Reboot-free.
- **UMD/DXVK:** `win_cargo umd ["build"]` (+ DXVK meson) → rename-aside over the **DriverStore**
  `...\helios_kmd_render.inf_amd64_<hash>\helios_umd.dll` (ProgramData-UMD hotplug mode is IGNORED).
- **LGIdd:** build via `win_looking_glass_idd` MCP; deploy `pnputil /add-driver <pkg>\LGIdd.inf
  /install` (NOT in-place copy — TrustedInstaller zeroes the DriverStore IDD DLL). Artifact:
  `LookingGlass\idd\x64\Release\LGIdd\`.
- **Probes** (compile on guest, output to local C: to dodge the Z: artifact-write bug, under a
  `vcvars64.bat` env): `d3dkmt_sync_probe.cpp` (fence acceptance), `d3d11_keyed_mutex_probe.cpp`
  (keyed-mutex producer/consumer).
- **Logs:** IDD swapchain = `C:\ProgramData\Looking Glass (IDD)\looking-glass-idd.txt`; UMD/ICD =
  `C:\ProgramData\Helios\umd-<pid>.log` / `helios_icd_*.log`; KMD diag ring = registry
  `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`; LG client (host) =
  `/tmp/helios-looking-glass-client.log`.
- **Display state MUST be WMI or session-1**, never session-0 (`win_exec`/SSH). Device states via
  `Get-PnpDevice` are reliable (session-agnostic).
- **ntoseye (KD; reconnect it — it disconnected at the end of this session):** VM launches with
  `HELIOS_KD_SERIAL=socket` on `/tmp/ntoseye-kd.sock`; MCP at `127.0.0.1:8080`. Run-control is split:
  `resume` (go), `wait_for_stop {timeout_ms}` (poll), `status`, `interrupt`; `registers`/breakpoint
  edits need the VM halted. **Benign-stop heuristic:** boot floods `nt!DebugService2` stops that
  `wait_for_stop` labels `stop:"bugcheck"` + user-mode `0x80000003` — all benign; the `bugcheck` tool
  returns **null** for them; a real crash returns a populated record. Set a `nt!KeBugCheckEx` breakpoint
  as a real-crash backstop, resume through the benign flood to `{stop:"running"}`, then SSH-verify.
  `backtrace`/`disassemble`/`search`/`capabilities` are **schema-broken via the MCP** — hit the ntoseye
  **HTTP endpoint directly** (`http://127.0.0.1:8080/mcp`: init handshake → `Mcp-Session-Id` →
  `tools/call`); a helper exists at `scratchpad/ntcall.py`. dxgkrnl has full public symbols;
  Helios/IndirectKmd/LGIdd do not (reason base+RVA / read raw memory). **Caveat (see §6.1): KD stretches
  the guest clock and freezes the network while halted — resume before `win_exec`, and don't trust
  KD-derived timing.**

## 8. Doc / memory map (all reconciled to this doc)

- **This doc** — authoritative status.
- **Memory** `venus-enum-adapter-probe-regression` — read the "SKEPTICAL SYNTHESIS" at the top; the
  giant entry below it is the last session's flip-flopping log (kept for the ntoseye traces, not the
  conclusions). `idd-display-activation-blocker-known` — carries a correction banner; its only reliable
  content is the "use WMI/session-1 not session-0" method warning.
- `WDDM_SYNC_REDESIGN.md`, `WDDM_SYNC_M3_M4_HANDOFF.md` — SUPERSEDED (banners added); sync is not the
  blocker. `IDD_BLACK_FRAME_HANDOFF_2026_06_25.md`, `HANDOFF_NEXT_SESSION.md` — historical.
- The working tree is a large uncommitted pile (KMD/ICD/UMD/DXVK/LGIdd, ~3000+ lines). Do NOT commit.
  Deployed-and-good: KMD `.33.0`, LGIdd leak-fix `v18.56.15.903`.
