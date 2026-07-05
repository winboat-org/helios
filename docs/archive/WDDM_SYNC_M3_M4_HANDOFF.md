# Helios WDDM Sync Redesign — M3 & M4 Handoff

> **⛔ SUPERSEDED (2026-06-26) — read `FABLE5_HANDOFF.md` instead.** Sync is not the blocker (the §0
> update below correctly refutes the DXVK keyed-mutex track; then it over-extended into a
> session-0-artifact "display topology" conclusion — also wrong). The real blocker is the **IDD
> failing its PnP post-start (Code 43)** on the render-only-Helios pairing. This doc is historical.

Date: 2026-06-25. Companion to `WDDM_SYNC_REDESIGN.md` (architecture + milestones).
Read that first, then this. Goal (locked): DWM composites the whole Windows desktop on the
Helios WDDM render adapter (venus → host GPU); the Looking Glass IDD displays those composed
frames. Do NOT pivot to per-app venus.

## 0. ★ UPDATE 2026-06-25 (later session) — §3 experiment RAN; M2' REFUTED

The §3 first-step experiment has been executed and is conclusive, and it **refutes the M2'
hypothesis below**. Do NOT rebuild DXVK for M2'.

**Experiment (the §3 designated first step):** With KMD v22.22.33.0 (M1) + the M2 ICD deployed
(`vulkan_virtio-879f56b158e4.dll`, 04:07), and **DWM stable and healthily compositing on Helios**
(pid 1860, up ~5 min, no crashes; its UMD log shows many `open_resource ddi-shared ok` /
`OpenDdiTexture2D hr=0x0` with KMT handles stamped), I forced a fresh `AssignSwapChain` by replugging
the IDD adapter `ROOT\DISPLAY\0000` (it was in **Error** state; replug → **OK**, and it re-ran
`AssignSwapChain` at 04:35:52). **`IddCxSwapChainSetDevice` STILL fails `0x887A0026` on all 5
attempts.** This is NOT a DWM-restart race (the earlier 04:30 failure raced a DWM recycle 2 s prior;
this one did not). The abandonment is **structural/persistent**, not transient. (Note: replugging the
*monitor child* `LGD1DDD` does nothing; you must replug the IDD *adapter* `ROOT\DISPLAY\0000` to
re-drive `AssignSwapChain`.)

**M2' is REFUTED (code read + live evidence):**
- The keyed mutex on the IddCx swapchain surface is **created and owned by dxgkrnl/OS internally**
  (on the Helios render-adapter allocation), NOT by our code. `DxvkKeyedMutex` is only instantiated
  for app-created `D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX` textures in `Export` mode
  (`dxvk-helios/src/d3d11/d3d11_texture.cpp:259-267`) — never the OS swapchain backbuffer.
- DWM (producer) does **not** go through `DxvkKeyedMutex` — confirmed live: the producer DWM UMD log
  (`umd-1860.log`) has **zero** `DxvkKeyedMutex`/AcquireSync/ReleaseSync activity.
- The IDD-process UMD log (`umd-5384.log`) shows it **never opens the swapchain surface** at all
  (only OpenAdapter/GetCaps/GetSupportedVersions/D3D12-unsupported). So `0x887A0026` fires **inside
  `IddCxSwapChainSetDevice`, before any of our UMD code (DXVK or KMD OpenAllocation) runs on the
  surface.**
- → Forcing `hasVulkanSyncObject()`=false / no-op `initKmtHandles` (M2') is **dead code for this
  path** and cannot change the abandonment. The redesign's "keyed mutex works post-M1" assumption was
  wrong: the **in-process** `d3d11_keyed_mutex_probe` passing was misleading — the cross-process
  OS-swapchain keyed mutex on a Helios allocation does NOT work.

**Corrected direction — the locus is KMD/dxgkrnl, not DXVK/ICD.** `0x887A0026` even on the *first*
post-clean-reboot attempt (per `WDDM_SYNC_REDESIGN.md §1`) argues this is not literally "a prior
process died" but that **dxgkrnl reads the keyed mutex's kernel state as abandoned because the Helios
allocation backing the OS swapchain/keyed-mutex surface isn't a coherent real/shared allocation** (the
KMD registers **no synchronization-object DDIs** and self-backs allocations; the keyed-mutex state
page may sit on an allocation that isn't properly shareable cross-process). Next step is a
**kernel-debugger (ntoseye) inspection** of the keyed-mutex object + the swapchain allocation at
`SetDevice` time (who/what state), then fix the KMD's CreateAllocation/OpenAllocation backing for the
OS-created shared keyed-mutex surface. M2'/M3'/M4' below are superseded by this.

### 0.5 ⚠️ NOT A NEW FINDING — this is the KNOWN display-activation blocker (see `HANDOFF_NEXT_SESSION.md`)

**Correction:** the "deeper root" below re-derives a blocker that was ALREADY documented in
`HANDOFF_NEXT_SESSION.md` (and reportedly re-investigated ~10× this week). Do NOT re-derive it again.
The known, documented state with Helios/gpu-gl present: **CCD/display activation fails** —
`GetDisplayConfigBufferSizes` returns `paths=0 modes=0` **even in session 1**; `WmiMonitorID` sees the LG
monitor active but `SetDisplayConfig(SDC_USE_SUPPLIED_DISPLAY_CONFIG|SDC_APPLY)` returns
**`31 ERROR_GEN_FAILURE`**, `EnumDisplaySettings`→`ERROR_BUSY(170)`, `IddCxAdapterDisplayConfigUpdate`→
`0xc00000bb`. The clean **gpu-gl-OUT** baseline (no Helios) works: WMI monitor active, session-1 CCD
`active paths=1`, LG client shows the desktop. **Display checks MUST use WMI / active session-1 probes —
SSH/session-0 (win_exec) CCD queries are misleading.** My session this turn used a session-0
`GetDisplayConfigBufferSizes` and over-claimed it as a fresh discovery — that was the mistake.

**★ CORRECTION (user, 2026-06-25): `SetDisplayConfig` is the WRONG mechanism — don't chase it.** Per
IddCx docs (`iddcx1.4-updates-for-remote-idds.md`): display-config control is REMOTE-IDD-only
(`IDDCX_ADAPTER_FLAGS_REMOTE_SESSION_DRIVER` + `IddCxAdapterDisplayConfigUpdate`, NOT `SetDisplayConfig`,
which "Fails" in remote sessions). LGIdd is a CONSOLE/local IDD (`CIndirectDeviceContext.cpp:134` sets
`USE_SMALLEST_MODE`, NOT the REMOTE flag; `IddCxAdapterDisplayConfigUpdate→0xc00000bb` confirms console)
and does NOT call `SetDisplayConfig` (grep: only `LGIddHelper/CPipeClient.cpp:315 ChangeDisplaySettingsEx`).
The old "helper SetDisplayConfig→ERROR_GEN_FAILURE" was a prior DEBUG PROBE, a red herring. A console IDD
monitor is meant to be **auto-activated by the OS** on arrival (which works gpu-gl-OUT).

The REAL open question: **why does the OS fail to auto-activate the connected console IDD monitor (0
active paths) ONLY when Helios is present (works gpu-gl-OUT)?** Suspect = Helios's display-adapter
enumeration perturbing the OS VidPN topology (render-only WDDM adapter maybe exposing a display
target/source the OS can't build a path through; and/or the IDD↔render-adapter pairing — IDD logs
"Preferred IDD render adapter not found; IddCx render adapter remains OS-selected"). Tools: IddCx WPP
capture (`logman ... {D92BCB52-FA78-406F-A9A5-2037509FADEA}`) to see what the OS rejects during topology
build with Helios present; check `DriverSupportsCddDwmInterop` (now NOT advertised); check what VidPN
targets/sources Helios exposes (a render-only adapter should expose ZERO display targets).

**GENUINELY NEW from this session (keep):** the `LGIdd.dll` swapchain-leak → UMDF-verifier crash
(§0.4) is a real bug NOT in the prior handoffs; fixed, built, deployed (v18.56.15.903), and verified to
eliminate the WUDFHost crash. The kernel-dig chain below (DWM `FinalRelease` / `DxgkRemoveAdapter` →
swapchain abandon → `0x887A0026`) is all DOWNSTREAM of the known activation blocker — accurate mechanism,
but it is the same blocker, not a new root.

### (superseded framing) the IDD monitor never gets an ACTIVE display path; DWM drops the swapchain

After fixing the LGIdd crash (§0.4) and re-digging on a clean boot, the `0x887A0026` persists and resolves
to a **display-topology** problem, found via live KD `backtrace` (HTTP) + Win32 display-config queries:

- `MarkAbandoned` on the IDD swapchain fires from **TWO** producer-side paths: (1) `DxgkRemoveAdapter`
  (the IDD ADAPTER_DISPLAY being stopped, §0.3) AND (2) **`dwm.exe` releasing the indirect swapchain's
  COM object**: `dxgi!CDXGIIndirectSwapChain::FinalRelease → kernelbase!CloseHandle → nt!NtClose →
  dxgkrnl!SwapChainObCloseProcedure → DXGSWAPCHAIN::DestroyLocal → MarkAbandoned` (cr3 = dwm pid).
- Timing (with KD stretching the guest clock, the `AssignSwapChain`→`SetDevice` gap widened to ~3 min,
  which let me catch it): the DWM `FinalRelease` happens **inside** the `AssignSwapChain`→`SetDevice`
  window — i.e. **DWM destroys the indirect swapchain right after `AssignSwapChain` creates it, before
  the IDD's `SetDevice` can bind it** → `SetDevice` sees it abandoned → `0x887A0026`. It's a race the IDD
  always loses because DWM drops the swapchain.
- WHY DWM drops it — the display state explains it: **`GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS)`
  = 0 active paths**; the only "screen" is **`WinDisc` 1024×768** (the headless/no-display placeholder);
  the IDD monitor `DISPLAY\LGD1DDD` is **Unknown/inactive**. So the IDD monitor never becomes an ACTIVE
  display path, DWM has nothing to composite onto it, and it releases the swapchain it was handed.

**So the true blocker is: the IDD monitor never gets activated into the display topology on the
render-only Helios pairing** (0 active paths, desktop stuck on headless WinDisc). Everything downstream
(DWM drops swapchain → abandon → IDD `SetDevice` 0x887A0026 → eventually Code 43 / adapter removal) flows
from that. This is NOT sync, NOT the LGIdd crash (fixed), NOT a blt-op failure, NOT TDR.

**NEXT (shifts from kernel-bp digging to display-topology):** find why the IDD monitor can't be made an
active display path. Leads: the historical `SetDisplayConfig EXTEND → ERROR_GEN_FAILURE`; whether the
IDD monitor's preferred mode/EDID is accepted; whether a render-only render adapter can back an active
IddCx VidPN path at all; whether something must call `SetDisplayConfig`/`IddCxMonitorArrival` differently;
and the LG host/client side (does the client request the monitor be activated?). Check `CIndirectDevice
Context` mode/EDID setup in `LookingGlass/idd/LGIdd/` and the OS display-config path. The LGIdd
swapchain-leak fix from §0.4 stays (real bug, deployed v18.56.15.903).

---

## 0.4 ★★★★★ (downstream of §0.5) `LGIdd.dll` crashed (WDF verifier violation) — FIXED; was a co-bug masking §0.5

The Code-43 post-start failure (§0.3) has a concrete cause, found in the Windows logs: **our own
`LGIdd.dll` (Looking Glass IDD UMDF driver) crashes**, terminating WUDFHost. Evidence:
- `Microsoft-Windows-DriverFrameworks-UserMode/Operational` **Event 4000 (Error):** "A runtime failure
  has occurred in user-mode driver **LGIdd.dll** and the hosting process has been terminated." + Event
  1009 "host process has a problem (8) and is being terminated."
- WER (Event 1001 / Report.wer), `WUDFVerifierFailure`:
  `P4/ErrorNumber=0x50100040000010f`, `P5=minkernel\wdf\framework\shared\object\fxverifierbugcheck.cpp:188
  (FxVerifierDriverReportedBugcheck)`, `P6=LGIdd.dll`, `UMDFVersion=2.35.0`. = a WDF/UMDF **framework
  verifier violation** (WDF_VIOLATION-class) in LGIdd → the framework terminates the host.

Full end-to-end chain (supersedes the "blt"/"sync"/"cap" framings entirely):
```
LGIdd.dll WDF verifier violation  →  FxVerifierDriverReportedBugcheck  →  WUDFHost terminated
  →  IDD device offline  →  Code 43 / CM_PROB_FAILED_POST_START
    →  DpiPowerArbiterThread → DxgkRemoveAdapter → DXGADAPTER::Stop → ADAPTER_DISPLAY::Stop
       → ReleaseAllVidPnSourceOwners(Helios) → BLTQUEUE::Reset → ResetWorker → MarkAbandoned
      →  IddCxSwapChainSetDevice → 0x887A0026 ("keyed mutex abandoned")  →  LG client: no frames
```
So `0x887A0026` is **three layers downstream of an LGIdd.dll crash.** The whole `WDDM_SYNC_REDESIGN`
(keyed-mutex/monitored-fence) AND the dxgkrnl-indirect-blt-contract theories are moot. **The fix is in
the LGIdd source** (`LookingGlass/idd/LGIdd/`).

**★ FIX FOUND + APPLIED + BUILDS (2026-06-25):** The violation is an **`IDDCX_SWAPCHAIN` WDF-object LEAK
on the swapchain bring-up FAILURE paths**, in `LookingGlass/idd/LGIdd/`. The OS hands the driver an
`IDDCX_SWAPCHAIN` in `AssignSwapChain`; the driver must `WdfObjectDelete` it. On SUCCESS, the swapchain
processing thread deletes it on teardown (`CSwapChainProcessor::SwapChainThread`, then sets
`m_hSwapChain=nullptr`). But on FAILURE the thread never starts, and **nothing deletes the handle** →
leak → the UMDF object-lifetime verifier terminates WUDFHost. Three leak paths in
`CIndirectMonitorContext::AssignSwapChain`: D3D11-init fail (line ~50), SetupLGMP fail (~78), and
`CSwapChainProcessor::Start()` fail (~83, the IddCxSwapChainSetDevice-failure path the logs show). FIX:
(a) `CSwapChainProcessor::~CSwapChainProcessor` now deletes `m_hSwapChain` if non-null after joining the
threads (covers the Start-fail path; thread-join makes the success-path nullptr store visible → no
double-delete); (b) the two no-processor paths (D3D11/SetupLGMP) `WdfObjectDelete((WDFOBJECT)swapChain)`
before returning ABANDON. Built clean via `win_looking_glass_idd` → signed `LGIdd.dll` + `lgidd.cat`.
**Next: deploy (devcon/LGIddInstall — NOT in-place copy; TrustedInstaller zeroes the DriverStore IDD
DLL) + reboot, then verify the IDD stays out of Code 43, WUDFHost survives, SetDevice succeeds, and
frames flow.** If WUDFHost still crashes, get the live verifier stack to find a second violation.

**★ DEPLOY + TEST RESULT (2026-06-25): the LGIdd leak fix WORKS for the crash, but `0x887A0026` is a
SEPARATE primary issue.** Deployed v18.56.15.903 via `pnputil /add-driver LGIdd.inf /install` (published
oem143.inf, installed on ROOT\DISPLAY\0000). VERIFIED on both a live re-install AND a clean reboot:
**Event 4000 ("runtime failure in LGIdd.dll") and `WUDFVerifierFailure` are GONE** — WUDFHost no longer
crashes on the swapchain-failure path (it survived ~1.5 min vs immediate before). So the swapchain-leak
verifier crash is genuinely fixed. **BUT** `IddCxSwapChainSetDevice` STILL fails `0x887A0026` on attempt
1 of a CLEAN boot (13:49:50), and the IDD still ends Code 43. So the earlier conclusion "the LGIdd crash
is THE root cause of 0x887A0026" was WRONG — the crash was a real co-bug that MASKED the primary issue;
`0x887A0026` is independent and precedes the crash each cycle. RULED OUT for the primary `0x887A0026`:
keyed-mutex sync (no acquire), the LGIdd verifier crash (fixed), and **TDR** (`0x887A0026`=ACCESS_LOST
looks like a post-TDR state, but there are ZERO Display/dxgkrnl/TDR events this boot — the WER 0x117
LiveKernelEvents were stale batch-uploads from an old 06-04 watchdog dump). The abandon still traces to
`DxgkRemoveAdapter` (a display adapter removed by `DpiPowerArbiterThread` on boot) — but NOT from the
LGIdd crash now, so the trigger is something else (a boot-time PnP rebalance/stop of the IDD or Helios
adapter, or an IddCx-internal restart). **NEXT: re-run the ntoseye `BLTQUEUE::Reset` backtrace dig on
THIS clean-boot + fixed-driver state (uncontaminated by the crash and by replug-induced removals) to find
what initiates the adapter removal** — arm `BLTQUEUE::Reset` (dxgkrnl base + 0x284f48) EARLY in boot
before the first AssignSwapChain, catch the FIRST hit, `backtrace` via the HTTP endpoint. The KEEP: the
LGIdd leak fix is correct and should stay regardless.

**(historical) Original next-step before the WER finding:** find the specific WDF/IddCx verifier
violation in LGIdd. The `.wer` has no stack
(CallerAddress=0, dumped separately). Either (a) read the LGIdd source around the swapchain
bring-up/teardown — most likely the `IddCxSwapChainSetDevice`-failure path in `CSwapChainProcessor` /
`CIndirectMonitorContext::AssignSwapChain` (it fails `0x887A0026` ×5 then the host dies), an
object-lifetime/double-complete/wrong-state WDF or IddCx API misuse; or (b) get the live stack: attach
ntoseye to the IDD WUDFHost, bp the UMDF verifier bugcheck path, reproduce, backtrace into LGIdd. NOTE:
the UMDF verifier is evidently ON for this driver (that's why it's a clean `FxVerifierDriverReportedBugcheck`
rather than a raw AV) — the violation is a real bug, but worth checking whether it's strictly-illegal vs
a genuine crash. Decode `ErrorNumber 0x50100040000010f` against WDF_VIOLATION subcodes to name the rule.

---

### 0.3 ★★★★ (mechanism, downstream of §0.4) IDD adapter fails POST-START → removed → swapchain abandoned

Using the ntoseye HTTP endpoint (`http://127.0.0.1:8080/mcp`) to reach the schema-broken `disassemble`
/`backtrace`, I traced the abandon to its true origin. **It is an ADAPTER-LIFECYCLE failure, not sync
and not a blt-op failure.**

Full causal chain (confirmed on a clean natural boot, no replug; identical stack to the replug case):
```
IDD display adapter (ROOT\DISPLAY\0000, "Looking Glass Indirect Display Device") fails POST-START
   → Code 43 / CM_PROB_FAILED_POST_START   (verified via Get-PnpDeviceProperty ProblemCode=43)
   → power-arbiter thread tears it down:
        nt!PspSystemThreadStartup → dxgkrnl!DpiPowerArbiterThread+0x54f → DpiRemoveAdapter → DxgkRemoveAdapter
          → DXGADAPTER::Stop → ADAPTER_DISPLAY::Stop → ReleaseAllVidPnSourceOwners(ADAPTER_RENDER*=Helios)
            → ADAPTER_DISPLAY::RemoveVidPnOwnership → BLTQUEUE::Reset(1)
              → [sets this->0x2A4 |= 2] → worker BltQueueWorker → ResetWorker → SwapChainAbandonInternal
                → DXGSWAPCHAIN::MarkAbandoned
   → consumer IddCxSwapChainSetDevice sees the abandoned indirect swapchain → returns 0x887A0026
```
How each step was nailed: `BLTQUEUE::Reset` is the only `or [reg+0x2A4],2` site in dxgkrnl (found via
HTTP `search`); breakpointing it + HTTP `backtrace` gave the chain above; `Get-PnpDevice ROOT\DISPLAY\0000`
= Status **Error**, ProblemCode **43** (`CM_PROB_FAILED_POST_START`); Helios PCI adapter stays Code 0.
The `BLTQUEUE` is the indirect-swapchain blt queue and it belongs to the IDD **ADAPTER_DISPLAY**, whose
`ReleaseAllVidPnSourceOwners` takes the **render** adapter (Helios) — i.e. this is the IDD↔Helios
indirect-display pairing being torn down.

**Meaning:** the IddCx indirect-display adapter, paired to the **render-only** Helios adapter, **fails its
post-start** — IddCx reports the device failed (Code 43 = a function driver returning `PNP_DEVICE_FAILED`
to `IRP_MN_QUERY_PNP_DEVICE_STATE` after start), so PnP/the power arbiter removes/restarts it; the removal
releases the VidPN source ownership it held on Helios and resets the indirect blt queue, abandoning the
swapchain that the consumer's `SetDevice` is concurrently trying to bind. **The whole keyed-mutex sync
redesign AND the "blt-op fails at runtime" refinement are both wrong** — there is no acquire race and no
failed blt; the indirect-display **adapter pairing fails post-start on render-only Helios** and the
adapter teardown abandons the swapchain.

**Earlier "blt fails at runtime" framing (§0.2 below) is SUPERSEDED:** the `ResetWorker`/`BltQueueWorker`
path is real but it is the *consequence* of `BLTQUEUE::Reset` being called from the adapter-stop path —
not an independent blt completion error.

**Next step (the true root):** find WHY the IDD adapter fails post-start on the Helios pairing. Catch the
INITIATION of the removal — breakpoint where the IDD device reports `PNP_DEVICE_FAILED` /
`IoInvalidateDeviceState`, or the IddCx post-start completion that fails — i.e. what the indirect-display
post-start validates about the paired render adapter (Helios) that render-only Helios doesn't satisfy.
Likely a capability/DDI the IddCx delivery path requires of the render adapter. Tooling: use the HTTP
endpoint for `disassemble`/`backtrace`/`search` (schema-broken in the MCP client); `BLTQUEUE::Reset` =
dxgkrnl base + 0x284f48; `search` for `or [reg+disp],imm` byte patterns works to find flag-writers.

---

### 0.2 ★★★ (SUPERSEDED by §0.3) earlier finding — abandon via blt queue, framed as a runtime blt failure

The ntoseye dig succeeded (3rd reboot, bps armed early during boot). **The IDD `0x887A0026` is the
indirect (IddCx) swapchain being marked ABANDONED as a SWAPCHAIN-OBJECT — there is no keyed-mutex
acquire involved.** Breakpoints on BOTH the user thunk `DxgkAcquireKeyedMutex2` AND the internal
`DXGKEYEDMUTEX::AcquireSync` **never fired** across the entire IDD bring-up. Instead, `MarkAbandoned`
fired, with this chain (all on the SAME `DXGSWAPCHAIN 0xffffe68729e55d10`, the IDD swapchain):

**Producer side — `dwm.exe` (pid 1924, the compositor) context — the CAUSE:**
```
IndirectKmd.sys+0x1f76                                  (OS Indirect-Display KMD / IddCx kernel half)
  → dxgkrnl!BLTQUEUE::SetIndirectSwapChainHandles+0x55  (early abandon/error path)
    → dxgkrnl!SwapChainAbandonInternal
      → dxgkrnl!DXGSWAPCHAIN::MarkAbandoned(false)
```
**Consumer side — `WUDFHost.exe` (pid 1604, LGIdd/IddCx) context — downstream CLEANUP:**
```
dxgkrnl!DxgkAbandonSwapChain+0x127   (= D3DKMTAbandonSwapChain, called by IddCx after SetDevice fails)
  → dxgkrnl!SwapChainAbandonInternal → DXGSWAPCHAIN::MarkAbandoned
```

So: dxgkrnl's **`BLTQUEUE::SetIndirectSwapChainHandles`** — which binds the indirect swapchain's
surface/allocation handles to the **system BLIT QUEUE** that copies the producer's composed frame into
the indirect-swapchain surface for the IDD to read — takes an **early abandon path on the render-only
Helios adapter**, in the compositor's context, during swapchain handle setup. That marks the swapchain
abandoned; `IddCxSwapChainSetDevice` (consumer) then sees the abandoned swapchain and returns
`0x887A0026` ("the keyed mutex was abandoned" is just how IddCx surfaces an abandoned swapchain). It is
the FIRST `AssignSwapChain` after a clean reboot (no prior owner could have died), and the same
swapchain is abandoned in both dwm and WUDFHost contexts — confirming it's the IDD swapchain and that
the producer abandon is the cause, the consumer abandon the cleanup.

**This INVALIDATES the entire sync direction** (`WDDM_SYNC_REDESIGN.md` M1–M5, M2', monitored fences,
the keyed-mutex object model). There is no keyed-mutex acquire race to fix. The real blocker is that
**dxgkrnl's indirect-display blt-queue handle setup rejects/abandons the Helios adapter** — a KMD
capability/DDI gap around what the IddCx system blit queue requires of the swapchain surface
allocations (consistent with the long-standing "`DxgkDdiPresent` never fires on render-only Helios" /
delivery gap). M1 (WDDM 3.2 + GpuMmu) is still fine to keep; M2'/M3'/M4' and the monitored-fence work
are moot for this bug.

**★ REFINEMENT (2026-06-25, 2nd ntoseye session) — the producer SETUP SUCCEEDS; the blt queue fails at
RUNTIME.** Caught `BLTQUEUE::SetIndirectSwapChainHandles` entry on a fresh `AssignSwapChain` and read it
end-to-end:
- The `handles` arg (`rdx`) is a **D3DKMT handle value** (e.g. `0x10d0`), NOT a pointer. A real setup
  call passes a non-zero handle; teardown passes `0`.
- Decoded the prologue: at `+0x3d` it does `mov rax,[this+0xB10]` (the currently-bound swapchain),
  `test/jz`, and if non-zero `call SwapChainAbandonInternal` (the `+0x55` site) — i.e. it **abandons
  the PREVIOUSLY-bound swapchain** whenever (re)set. On a clean first setup `[this+0xB10]==0`, so it does
  NOT abandon.
- **The setup call returns `STATUS_SUCCESS` (`rax=0`).** Caller is
  `ADAPTER_DISPLAY::DodSetIndirectSwapchain+0x28d`. So the producer binds the indirect swapchain
  successfully.
- Therefore the abandon that breaks the IDD is **NOT a setup failure**. It comes later, via
  `BLTQUEUE::ResetWorker → SwapChainAbandonInternal → MarkAbandoned` (seen firing in a System worker
  thread) and the teardown `SetIndirectSwapChainHandles(handles=0)`. I.e. **the indirect-display blt
  queue is set up fine, then FAILS at runtime (when it tries to actually blt/copy the composed frame
  producer→consumer) and resets, abandoning the swapchain** — which the consumer `SetDevice` then
  reports as `0x887A0026`.

This is the **frame-delivery mechanism for indirect display** (the system blt queue that copies the
compositor's surface into the IddCx swapchain surface), failing at runtime on render-only Helios —
exactly the long-standing "frames but black / `DxgkDdiPresent` never fires / res_id mismatch" delivery
gap, now localized to dxgkrnl's `BLTQUEUE`.

**★★ FURTHER REFINEMENT (2026-06-25, 3rd ntoseye session) — localized to `BLTQUEUE::BltQueueWorker`
+ a reset flag.** Caught `BLTQUEUE::ResetWorker` (entry `0xfffff801715550b0` this boot) firing on the
SAME blt queue (`this=0xffff940d4da9a0a0`) that set up successfully. Its caller is
**`BLTQUEUE::BltQueueWorker+0xc00`** — the blt-queue worker loop itself. Decoded the call site:
```
BltQueueWorker: ... (two cross-module calls, ~KeWait*) ...
  mov rbx, [this+0x2a4]      ; a "pending-action" flags field
  test bl, 1  → jnz: call <handler A>(this)
  test bl, 2  → jnz: call ResetWorker(this)     ; <-- bit1 set ⇒ RESET  (rbx=2 confirmed at entry)
```
So `BltQueueWorker` waits, then if **`[this+0x2a4] & 2`** ("reset needed") is set it calls `ResetWorker`
→ `SwapChainAbandonInternal` → `MarkAbandoned`. The reset flag is set by the blt
completion/error path (the actual producer→consumer copy failing on render-only Helios). **The last
mile = who sets `[this+0x2a4] |= 2` and why.** That needs a DATA watchpoint on `[this+0x2a4]` (ntoseye
exposes only CODE breakpoints — no watchpoint tool) OR finding the blt submit/complete function (ntoseye
symbol-search returns EMPTY even for known `BLTQUEUE` symbols this session — only `closest_symbol` on
computed addresses works; `BltQueueWorker` is a big func, ~>0xc00 bytes). Most efficient route to the
last mile is now **KMD-side correlation**: instrument `kmd_render` to log what the blt worker submits to
Helios during an IDD attempt (DxgkDdiSubmitCommand / paging / present) and where it errors — that is also
where the actual fix lives. The dxgkrnl side only records "blt failed → set reset flag → reset → abandon".
Repeated `AssignSwapChain` replugs wedge venus (consumer side), but the producer blt worker still runs on
a replug, so this is catchable without a reboot while the same-boot dxgkrnl addresses hold.

---

### 0.1 ntoseye dig — setup confirmed feasible; aborted by two environmental walls (reboot needed)

Started the kernel dig (user-chosen). Findings + a tighter protocol for the retry:

**Good news — the dig IS feasible:** `dxgkrnl` has **full public symbols** for the keyed-mutex path:
`dxgkrnl!DxgkAcquireKeyedMutex2` (0x…c59e50), `DxgkOpenKeyedMutex2`, `DxgkReleaseKeyedMutex2`,
`?CreateAndOpenKeyedMutex@DXGGLOBAL@@`, `?OpenKeyedMutexFromNtHandle@DXGGLOBAL@@`,
`?CreateSharedKeyedMutexNtObject@@`, and the object type `g_pDxgkSharedKeyedMutexObjectType`.
**Caveat:** only PUBLIC symbols — **no private type layouts** (`DXGKEYEDMUTEX` struct is unavailable;
`read_struct` can't be used), and `disassemble`/`backtrace`/`capabilities` are schema-broken. So the
mutex object must be read as **raw memory + inferred layout**. Also confirmed: Helios does **NOT** use
the `DXG_GUEST_GLOBAL_VMBUS::VmBusSend*KeyedMutex*` paravirtual-vGPU path (that's VMBus vGPU); it uses
the **local** `DxgkAcquireKeyedMutex2` path.

**Two walls hit this attempt (both avoidable next time):**
1. **Benign user-mode `int3` / debug-print storm.** Resuming the guest produces a near-continuous
   stream of `stop:"exception" exception_code 0x80000003` at `0x7fff…` user addresses (OutputDebugString/
   DbgPrint surfacing to KD because it's attached). Each freezes the guest network, so `win_exec`
   (replug trigger) intermittently gets "no route to host" and the IDD process makes no progress while
   halted. Mitigation: resume through them to reach `{stop:"running"}` before any `win_exec`; ideally
   quiet the source (LG client/IDD retry loop) or pass these exceptions.
2. **Venus repeated-device-create wedge.** Triggering `AssignSwapChain` repeatedly (PnP replug of
   `ROOT\DISPLAY\0000`) runs a fresh `D3D11CreateDevice` on Helios each time; after a few, venus wedges
   — `D3D11CreateDevice` hangs at "begin" and **WUDFHost dies** (known: "venus wedges on repeated
   device-create → reboot to recover"). So `SetDevice`/the keyed-mutex acquire is never reached, and the
   `DxgkAcquireKeyedMutex2` bp never fires (INCONCLUSIVE — we don't yet know if the abandonment goes
   through the public acquire thunk or an internal acquire).

**Tighter retry protocol (do this next):** (a) **Reboot the guest** to recover venus. (b) Attach KD;
**before** the first `AssignSwapChain`, set bps on `DxgkAcquireKeyedMutex2` +
`?OpenKeyedMutexFromNtHandle@DXGGLOBAL@@` + `?CreateAndOpenKeyedMutex@DXGGLOBAL@@`. (c) Let the **first,
single** post-boot `AssignSwapChain` happen naturally (LG client connect) — do NOT replug repeatedly
(the first `D3D11CreateDevice` succeeds; only repeats wedge). (d) At the hit: `registers` (mutex ptr +
key in arg regs), `describe_address` the mutex ptr, then `read_memory` the mutex object and infer the
owner/abandoned/key fields; correlate the mutex to the swapchain allocation. Goal: determine whether
"abandoned" = a real dead prior owner vs. uninitialized/garbage state (⇒ KMD allocation-backing fix).

---

## 1. Where we are (M1 ✅, M2 code-complete + dormant)

**M1 (adapter raised to WDDM 3.2 + GpuMmu) — DONE + VERIFIED.**
- `RAISE_WDDM_3_2_GPUMMU` (in `kmd_render/src/ddi/query_adapter_info.rs`) gates an atomic raise:
  `lib.rs` `data.Version → DXGKDDI_INTERFACE_VERSION_WDDM3_2`; DRIVERCAPS + WDDMDEVICECAPS
  `WDDMVersion → DXGKDDI_WDDMv3_2`; `MemoryManagementCaps |= VirtualAddressingSupported(bit5) |
  GpuMmuSupported(bit6)`; `GetNodeMetadata.GpuMmuSupported = 1`. KMD **v22.22.33.0**, deployed,
  binds **Code 0**. No InitDmaPools/0x10E boot-loop (the decorative GpuMmu DDIs already in-tree
  held). `D3D11CreateDevice` = S_OK (no revision mismatch).
- `tools/d3dkmt_sync_probe.cpp` proves `D3DDDI_MONITORED_FENCE` now succeeds (private + **shared-NT**)
  with non-NULL CPU VA + GPU VA. **shared-KMT monitored fences are rejected (`0xc000000d`)** →
  monitored fences are **NT-share-ONLY**. This is a hard constraint for M4.

**M1 BONUS — the keyed-mutex MECHANISM now works.** `tools/d3d11_keyed_mutex_probe.cpp`
(NT-handle keyed-mutex, producer+consumer on two devices in one process) now passes fully:
`AcquireSync`/`ReleaseSync` all `hr=0` (was `0x887A0026` "abandoned" before). The fix came from
the WDDM 3.2 raise making the `D3DKMTCreateKeyedMutex2` kernel object behave — NOT from the venus
fence path (the probe produced ZERO `sync_create` lines in `helios_icd_diag.log`).

**M2 (monitored fence advances on real venus completion) — CODE-COMPLETE + DEPLOYED, but DORMANT.**
- `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c::helios_wddm_sync_create` now creates a
  `D3DDDI_MONITORED_FENCE` (NT-share when `nt_shared`), captures `FenceValueCPUVirtualAddress` into
  `out_cpu_va`, falls back to legacy on rejection. Deployed (ICD DLL rename-aside).
- The existing retire→signal infra (`helios_sync_retire_locked` / `helios_sync_mark_fence_locked`
  → `helios_wddm_sync_signal`, fired when a venus fence retires on the host used-ring ack) now
  drives a *real* monitored fence (it was a silent no-op with legacy fences, which can't be
  CPU-signaled).
- **Why dormant:** `helios_wddm_sync_create` is only reached via the venus *semaphore export* path
  (`vn_renderer_helios_sync_export_win32`), which DXVK only triggers when its keyed-mutex
  **GPU-ordering arm** is armed — and that arm is currently dropped (`DxvkFence::initKmtHandles`
  early-returns leaving `m_kmtLocal == 0`). So nothing exports a venus semaphore yet → M2's fence
  never runs in the live stack. Arming it is M4.

## 2. The mental model (three sync layers — keep them distinct)

1. **Keyed-mutex kernel object** (`D3DKMTCreateKeyedMutex2`) — CPU-side producer/consumer mutual
   exclusion on the shared surface. dxgkrnl-managed. **Works now (M1).** This is what
   `IddCxSwapChainSetDevice` acquires; `0x887A0026` = a prior owner died without releasing.
2. **venus monitored fence** (ICD `helios_wddm_sync_*`, now real after M1/M2) — the guest-visible
   64-bit fence (CPU VA + GPU VA) reflecting venus completion to the OS/CPU. Vehicle for the
   DXVK keyed-mutex GPU-ordering arm + cross-process "frame ready" signal.
3. **host VkSemaphore** (M3) — the only thing that actually orders the *real host GPU* work:
   DWM's composition render must signal it before the IDD's read waits on it. Wired via venus
   `VkImportSemaphoreResourceInfoMESA` bound to the shared composition resource.

"Frames flow but black/torn" needs all three. "Swapchain starts at all" needs only #1 (now fixed).

## 3. FIRST STEP for the next session (cheap, high-information)

The last IDD `AssignSwapChain` failure (`looking-glass-idd.txt`, `0x887A0026`) was at **boot 03:57,
BEFORE the M2 ICD deploy**, and WUDFHost currently has no UMD (swapchain inactive). Since the
keyed-mutex mechanism now works, **re-trigger a fresh IDD `AssignSwapChain` and see if it now
succeeds** before assuming M4 is needed:
- Reconnect the Looking Glass client, or replug the IDD monitor (PnP disable/enable of the
  `Generic Monitor (Looking Glass)` / `LGD1DDD` monitor, NOT the Helios adapter — replug can wedge
  the swapchain; a full guest reboot recovers).
- Watch `C:\ProgramData\Looking Glass (IDD)\looking-glass-idd.txt` for a *new* `AssignSwapChain` →
  `IddCxSwapChainSetDevice` result, and `WUDFHost` for `helios_umd` load.
- If `SetDevice` now **succeeds** → the swapchain processor starts → the IDD reads frames. Then the
  problem reverts to the original **"frames but black" delivery** question (M3' below) — NOT a sync
  problem. Use the per-pid `C:\ProgramData\Helios\umd-<pid>.log` (logging fix from earlier this
  session) to see the IDD process's `open_resource`/`OpenDdiTexture2D` outcome for the swapchain
  surface.
- If `SetDevice` still returns `0x887A0026` → the producer (DWM) side leaves the swapchain-surface
  keyed mutex abandoned → do M2' (§4): delete the DXVK Vulkan-semaphore arm so the keyed mutex is
  pure CPU exclusion + CPU-ordered release (the broken arm is the likeliest abandonment cause).

## 4. ★ DESIGN PIVOT — drop the `VK_KHR_external_semaphore_win32` emulation

**Confirmed from the code (2026-06-25):** we do NOT need to emulate Vulkan external semaphores or
build a host-to-host VkSemaphore. Host-GPU completion ordering already exists end-to-end:
- `kmd_render/src/virtio/gpu.rs::submit_venus` sends `VIRTIO_GPU_CMD_SUBMIT_3D | FLAG_FENCE` and
  **"blocks (polled) until the device acknowledges it on the used ring, so by the time it returns
  the work is host-visible-complete."**
- A venus *wait* command (what `vkWaitForFences`/`vkQueueWaitIdle` lower to) blocks on the host's
  real `VkFence` → GPU-accurate (proven: games render correctly, `vkQueueWaitIdle` works).
- DXVK's `D3D11DXGIKeyedMutex::ReleaseSync` (`dxvk-helios/src/d3d11/d3d11_resource.cpp`) **already**
  calls `context->WaitForResource(*image, DxvkCsThread::SynchronizeAll, ...)` — a real
  GPU-completion wait — **before** `keyedMutex->ReleaseSync(Key)`. So the producer cannot release
  the surface until its host-GPU render is done.

Therefore the cross-process contract is: **D3DKMT keyed mutex (OS-facing, CPU exclusion + handoff,
works post-M1) + DXVK's existing CPU-ordered release.** The monitored fence, the host VkSemaphore,
and the Win32-handle export are all unnecessary. The old M3/M4 above are SUPERSEDED.

### M2' — delete the emulation, make the keyed mutex pure-CPU (then frames should be unblocked)
- `dxvk-helios/src/dxvk/dxvk_image.cpp` (`DxvkKeyedMutex`): the `vkSignalSemaphore`/`vkWaitSemaphores`
  arm is gated by `hasVulkanSyncObject()` = `m_fence && m_fence->kmtLocal()!=0`. For Helios, make
  this arm a no-op (e.g. force `hasVulkanSyncObject()` false / skip creating `m_fence`): the keyed
  mutex then does only `D3DKMTAcquire/ReleaseKeyedMutex2`, and CPU ordering comes from
  `ReleaseSync`'s `WaitForResource`.
- `dxvk-helios/src/dxvk/dxvk_fence.cpp::initKmtHandles` (the `vkGetSemaphoreWin32HandleKHR` →
  `D3DKMTOpenSynchronizationObject` export round-trip) — no longer needed; stop calling it / let it
  no-op for Helios. This also removes the `kmtLocal()==0 → arm-dropped` failure mode entirely.
- `d3d11_texture.cpp:~751` publishing `keyedMutex->kmtLocal()` as the shared handle: with NT-handle
  shared resources (`D3D11_RESOURCE_MISC_SHARED_NTHANDLE`, which the `d3d11_keyed_mutex_probe`
  already exercises successfully) the OS shares the keyed mutex by NT handle natively — verify the
  consumer opens the right object (the probe proves the round-trip works in-process; the IDD is the
  cross-process case).
- ICD `helios_wddm_sync_create` monitored-fence change (M2) is now unused on this path — leave it
  (harmless, dormant) or revert.
- **Test:** the `d3d11_keyed_mutex_probe` still passes; then re-trigger the IDD swapchain (§3) and
  confirm `IddCxSwapChainSetDevice` succeeds (no `0x887A0026`) and WUDFHost loads the UMD.

### M3' — DELIVERY / surface unification (the real remaining problem — the original "black frame")
Once the swapchain starts, the question is the one from the start of the session: **does the IDD's
acquired swapchain surface resolve to the same venus resource DWM composes into?** Earlier evidence:
DWM composed into `res_id 52/54/55` (ctx=8, content) while the IDD read `res_id 147` (ctx=2,
KMD-self-backed, never written). Path-(a) = make the OS-created swapchain backbuffer (the KMD
self-backs it in `create_allocation.rs::GetStandardAllocationDriverData`) be the SAME venus resource
DWM renders into, via `open_ddi_texture2d` (which now works — earlier this session DWM's log showed
`open_resource ddi-shared ok`). Use the per-pid `C:\ProgramData\Helios\umd-<pid>.log` to read the
**IDD process's** `open_resource`/`OpenDdiTexture2D` outcome for the swapchain surface (the logging
fix this session made the restricted IddCx host process's log visible).
- Key files: `umd/src/forward.rs` (`create_resource` proxy mint, `open_ddi_texture2d` import ~1121),
  `kmd_render/src/ddi/create_allocation.rs` (self-back vs adopt),
  `LookingGlass/idd/LGIdd/CSwapChainProcessor.cpp` (`SwapChainNewFrameD3D11` readback).
- **Re-check IDD adapter pairing:** under WDDM 3.2 Helios now enumerates **once** in DXGI (was
  twice); the IDD's `AssignSwapChain renderAdapter LUID` was the 2nd enum. Confirm the OS still
  composites the IDD on Helios.

### M4' — ONLY IF frames tear (fallback): resource-id-bound host semaphore
If the IddCx *producer* (the OS compositor) releases the keyed mutex without DXVK's
`ReleaseSync` GPU-wait (i.e. it bypasses our CPU-ordered path), add real host-GPU ordering the
"use-our-impl-directly" way: set `VkImportSemaphoreResourceInfoMESA.resourceId` (currently hardcoded
`0` at 3 sites in `icd/.../vn_queue.c`) to the **shared composition resource id**, so both processes
import the SAME host semaphore by venus resource id (no Win32 handle). Producer's venus stream
signals it, consumer's waits it, host GPU serializes. Test CPU-ordered (M2'/M3') first — only do
this if a tear is actually observed.

## 6. Build / deploy / test recipes

- **KMD:** edit → `win_cargo kmd_render ["make","--makefile","Cargo.make.toml"]` (bump the version
  in `build.rs` + `Cargo.make.toml`, currently `22.22.33.0`, so the package is distinguishable) →
  `install-helios-kmd.ps1 -AllowRebootRequired` (binds, marks restart-required, does NOT restart
  the in-use device → no live crash) → `Restart-Computer -Force` to activate. Backups land in
  `C:\ProgramData\HeliosDeployBackups\`.
- **ICD:** `win_meson ["compile","-C","C:\\Users\\Rupansh\\helios-mesa-build"]` → deploy by
  **rename-aside** over `C:\ProgramData\HeliosVulkan\vulkan_virtio-879f56b158e4.dll` (a loaded DLL
  can't be overwritten in place; rename it `*.old.<rnd>` then copy the new one). Reboot-free; new
  processes pick it up.
- **UMD / DXVK:** rebuild `win_cargo umd ["build"]` (+ DXVK via its meson) → deploy by rename-aside
  over the **DriverStore** copy
  `C:\WINDOWS\System32\DriverStore\FileRepository\helios_kmd_render.inf_amd64_<hash>\helios_umd.dll`
  (the ProgramData-UMD hotplug mode is IGNORED — WDDM loads the DriverStore copy). Reboot-free.
- **Probes** (compile on the guest, output to local C: to avoid the Z: artifact-write bug):
  `cl /nologo /EHsc /W4 Z:\tools\<probe>.cpp /I"Z:\icd\win-build\wdk-include" /Fe:C:\Windows\Temp\x\p.exe /link gdi32.lib`
  (D3D probes link `d3d11.lib dxgi.lib`). Run under a `vcvars64.bat` env.
  - `d3dkmt_sync_probe.cpp` — monitored/legacy/cpu-notification fence acceptance (M1).
  - `d3d11_keyed_mutex_probe.cpp` — NT-handle keyed-mutex producer/consumer (M2/M4).
- **Logs:** per-pid `C:\ProgramData\Helios\umd-<pid>.log` (UMD, both Rust + `[dxvk-bridge]`) and
  `helios_icd_{diag,shmem,submit,av}.log` (ICD) — all redirected there this session so the
  restricted IddCx host process can write them. IDD: `C:\ProgramData\Looking Glass (IDD)\
  looking-glass-idd.txt`. LG client (host): `/tmp/helios-looking-glass-client.log`.
- **KMD diag ring:** `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render` (registry values);
  `diag::record_named` writes fixed-name values that survive the `S<idx>` flood.

## 7. ntoseye (kernel debugger) — how to use it this session's way

The VM is launched with `HELIOS_KD_SERIAL=socket` → an isa-serial KD transport on
`/tmp/ntoseye-kd.sock`; the ntoseye MCP runs at `127.0.0.1:8080` and its tools are available via
ToolSearch (`mcp__ntoseye__*`). The user attaches it specifically to debug boot wedges.

**Run-control split** (single debugger session — don't block it):
- `status` — read-only "where am I" (`running`, `rip`, `symbol`, `coherent`). `coherent:false`
  right after a reboot = rediscovery in progress; `wait_for_stop` rather than reading stale state.
- `resume` — go (non-blocking). `wait_for_stop {timeout_ms}` — poll for the next stop WITHOUT
  resuming (max 20000; returns `{stop:"running"}` if it elapsed — call again). `interrupt` — pause.
- `registers` / `bugcheck` / breakpoint-mutation / `step` / `set_register` need the VM **halted**.
  `write_memory` works live.

**Distinguishing a benign stop from a real wedge (critical — the user warned about benign breaks):**
- Boot produces many stops at `nt!DebugService2` that `wait_for_stop` labels `stop:"bugcheck"`, and
  user-mode `stop:"exception"` with `exception_code 0x80000003` (STATUS_BREAKPOINT). **These are
  benign** (DbgPrint / debugger breaks). The tell: call the **`bugcheck`** tool — it returns
  **null** (a schema "expected record, received null" error) for benign stops, and a populated
  record (code/name/args/fault) for a REAL bugcheck. Benign → `resume` and keep going.
- As a deterministic real-crash backstop, set a breakpoint on `nt!KeBugCheckEx`
  (`set_breakpoint "nt!KeBugCheckEx"`) before resuming through boot; if it fires (rip at that
  address), read `registers` — `rcx` = bugcheck code, `rdx/r8/r9` = args.
- After resuming through the early-boot benign stops, `wait_for_stop` eventually returns
  `{stop:"running"}` (free-running) — then the guest boots; verify via `win_exec` (SSH) that the
  adapter is Code 0.

**Known-broken tools (a documented ntoseye quirk):** `backtrace` and `disassemble` return arrays
but the MCP client rejects them ("expected record, received array") — **unusable**. Substitute:
`registers` + `describe_address {addr}` (gives module+offset+section, e.g. `ntoskrnl.exe+0x4f90d0`)
+ `read_memory` / `read_struct` / `pte_walk`. There are no Helios PDB symbols → reason in
base+RVA. `processes` / `modules` / `kernel_modules` / `driver_objects` enumerate live (no halt
needed) once `coherent:true`.

**This session's boot-watch sequence (reuse it):** deploy KMD + `Restart-Computer -Force` →
`status` (wait for `coherent` to flip) → loop {`resume`; `wait_for_stop`; if `bugcheck` non-null →
diagnose; else benign → continue} until `{stop:"running"}` → SSH-verify Code 0. A boot-loop would
show a real bugcheck record (likely `0x10E` VIDEO_MEMORY_MANAGEMENT_INTERNAL or `0x119`
VIDEO_SCHEDULER_INTERNAL_ERROR if a GpuMmu/submit DDI regresses) — fix forward (the user does NOT
want reverts).

## 8. Negative results / gotchas (don't repeat)

- KMD present-blit in `dxgkddi_present` — the DDI never fires on a render-only adapter.
- Setting `DXGK_DRIVERCAPS.WDDMVersion` is reserved-must-be-0 for modern drivers per MS docs, but
  this codebase's working pattern sets it to match the interface version; we set
  `DXGKDDI_WDDMv3_2` and it binds Code 0 — leave it.
- Bumping the adapter to WDDM **2.0** (not 3.2) → `STATUS_REVISION_MISMATCH` (too old for the 24H2
  UMD). Target 3.2 (the header/struct-ABI default).
- Native fences (`DxgkDdiCreateNativeFence`, `DXGK_VIDSCHCAPS::NativeGpuFence`) are OS-feature-gated;
  advertising them unprovoked fails AddAdapter. Not needed for monitored fences. Leave OFF.
- Monitored fences are **NT-share-only** (shared-KMT → `0xc000000d`). Any cross-process share must
  use NT handles.
- The big uncommitted tree (KMD/ICD/UMD/DXVK, ~3000+ lines) — do NOT commit. KMD `.33.0` deployed is
  GOOD; keep it. The `RAISE_WDDM_3_2_GPUMMU` lever can flip false to revert M1 if ever needed (but
  the user does not want reverts — fix forward).
