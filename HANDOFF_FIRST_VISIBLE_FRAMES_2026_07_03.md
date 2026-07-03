# Handoff — FIRST USER-VISIBLE FRAMES (2026-07-03)

> ## §0c Cold boot #3 (07:30): SELF-CONVERGED, zero manual actions — and the boot taxonomy
>
> Third cold boot, nothing touched: **the system converged on its own at boot+4.5 min** and
> the bound swapchain now acquires at the idle-desktop cadence (exactly one frame per minute
> — the taskbar clock redraw — at 02:05:00/02:06:00/02:07:00Z; a fed steady-state binding,
> which also answers the §3 "is idle-zero-presents legitimate" question for the idle case).
> Sequence (CORRECTED after the owner spotted the host-side lines — the first venus
> activity of the boot in `/tmp/helios-qemu-stderr.log`):
> **the dwm crash was a C1 ALLOCATION-IDENTITY failure, not boot slowness.**
> dwm opened a shared surface → ICD imported by venus res_id 45 → host:
> `vkr: failed to import resource: invalid res_id 45` (the resource was never attached to
> dwm's venus context — the exact C1 hole: no CTX_ATTACH_RESOURCE for the opener) →
> `vkAllocateMemory resulted in CS error` → `ring_submit_cmd: vn_dispatch_command failed`
> → dwm's ring dead → the now-SYNC `vn_call_vkAllocateMemory` waited on the dead ring →
> **mesa's watchdog abort()ed dwm** (dump `dwm.exe.1904.dmp`, stack resolved via mingw
> addr2line with the PE ImageBase added: `abort ← vn_ring_wait_seqno vn_ring.c:258 ←
> vn_call_vkAllocateMemory ← vn_device_memory_import_resource_id vn_device_memory.c:277`).
> While dwm hung on that dead ring it could not offer a swapchain — which is WHY this
> boot had no offer during post-start. Then Windows restarted dwm (07:33:19), the C5
> replug loop (pacing arrival→10 s offer-timeout→replug the whole time, ~11 s/cycle,
> CommitModes every cycle) re-offered, and the post-dwm-restart cycle **bound
> successfully** (07:34:55) → frames. Note the P0.1 payoff: under the old async alloc this
> same identity failure was SILENT phantom-object ring poison; now it is loud and
> attributable host-side. The guest's failure mode (stall→abort instead of clean
> VK_ERROR_DEVICE_LOST) is the remaining I-A4/I-A5/I-A6 work; the ROOT is C1.
>
> **The three-boot taxonomy (the discriminator for the fix):**
> - Boot #1: swapchain offered at +2 s → born abandoned → no re-offer → host killed at
>   ASSIGN+25 s → FAILED_POST_START (permanent).
> - Boot #2: same, callback stuck in 22 s SetDevice stalls → killed at +25 s.
> - Boot #3: NO offer during post-start (dwm stalled/aborted) → **no kill** → post-start
>   completed → replug loop converged once dwm was reborn.
>
> ⇒ **The lethal condition for the IDD is specifically "an abandoned swapchain assigned
> during the post-start window"** — not the abandon itself (warm abandons recover), not
> callback duration, not the replug loop (it ran for minutes unharmed and then delivered
> the convergence). And **the #1 crash driver at boot is now proven to be C1** (the
> invalid-res_id import above), which is very likely also the black-window content class
> (failed imports → metadata-texture fallback → black). Fix order, updated:
> 1. **C1 (promoted, hard evidence)**: KMD attaches the resource to the OPENER's venus
>    context at OpenAllocation (CTX_ATTACH_RESOURCE with the opener's ctx id) + the
>    versioned identity ABI + exact-size import + DELETE the metadata fallback. This
>    removes the invalid-res_id class entirely.
> 2. ICD failure-mode honesty (I-A4/I-A5/I-A6): host CS error → clean VkResult /
>    VK_ERROR_DEVICE_LOST instead of dead-ring stall → watchdog abort() of the process.
> 3. The born-abandoned first swapchain at boot (June-26 DxgkRemoveAdapter→BLTQUEUE::Reset
>    chain) — root of the boot-#1/#2 kill; option: gate the FIRST IddCxMonitorArrival on
>    desktop readiness (arrival gating moves the fragile assignment out of post-start).
> No WUDF dump this boot (nothing crashed WUDFHost); the writer stays armed for the next
> boot-#1-shaped failure.

> ## ⚠️ §0 COLD-BOOT RESULT (added same day, after the owner's hard reboot): TRANSIENT.
> The visible frames did NOT survive a cold boot. After a hard reboot (07:09) the owner sees
> only the LG client placeholder. Diagnosis from that boot's evidence (nothing was restarted;
> the failed state was left in place):
>
> - **dwm: STABLE.** Single instance since boot, zero dumps — the P0 crash-loop class did not
>   return. Helios KMD: Code 0.
> - **The IDD devnode is Code 43 `CM_PROB_FAILED_POST_START`** — the pre-existing cold-boot
>   failure mode (June-26 memory), which predates this session's changes.
> - **Root cause class found: LGIdd.dll crashed WUDFHost — a UMDF VERIFIER FAILFAST.**
>   System log 07:10:09 Critical: "A runtime failure has occurred in user-mode driver
>   LGIdd.dll and the hosting process has been terminated"; WER report
>   `NonCritical_VerifierFailure_…` → `fxverifierbugcheck.cpp:188
>   (FxVerifierDriverReportedBugcheck)`, ErrorNumber `050100040000010f`, Driver=LGIdd.dll.
>   No stack in the report (no dump was configured at the time).
> - **Boot log sequence** (`looking-glass-idd.1.txt` after the next rotation; times 01:39Z):
>   clean init → SelectRenderAdapter(Helios @ LUID fa5d) → MonitorArrival OK → CommitModes
>   paths=1 → AssignSwapChain at +2 s **paired to LUID 77eb — an OLDER pairing instance than
>   the live Helios adapter** (boot-time pairing churn, same signature as the warm case) →
>   SetDevice ×5 `0x887A0026` → ABANDON returned at 01:39:44 → **log ends**. CDebug flushes
>   per line, and the state machine's offer-timeout would have logged at +10 s (01:39:51)
>   before acting — no such line exists ⇒ **the crash happened in the ~7 s window right
>   after the boot-time ABANDON, BEFORE the new watchdog ever ran.** The watchdog machinery
>   is exonerated for the crash itself (and died with the process — an in-driver watchdog
>   cannot recover a FAILED_POST_START devnode).
> - Contrast with the warm case (06:53, same driver): identical ABANDON → OS re-offered a
>   new swapchain 1 s later → bind → frames. At cold boot the OS (or the crash) never got
>   there. UMDF said it would retry the device 5 times; no retry ever reached DriverEntry
>   (the log never rotated again) — the devnode settled at FAILED_POST_START.
> - **Action taken: WUDFHost.exe LocalDumps enabled** (C:\HeliosDumps, full, DumpCount=3) —
>   the next boot-time failfast leaves a stack. That dump is the #1 input for the next
>   session.
>
> **Next-session priority therefore shifts:** before the §3/§4 items below, root-cause the
> boot-time UMDF verifier failfast (reboot with dumps armed → `cdb -z` the WUDFHost dump
> with the LGIdd PDB from `LookingGlass\idd\x64\Release`). Suspect surface: the
> ABANDON-return aftermath at boot (IddCx deleting the abandoned swapchain during
> post-start while the monitor-object teardown/our EvtCleanupCallback interleave — the
> same FxObject double-management class as the June-25 swapchain-leak verifier bug), and
> any interaction with the stale-pairing (LUID 77eb) instance being torn down under our
> live CD3D11Device. The IDD was deliberately left in the failed state; a
> `pnputil /restart-device ROOT\DISPLAY\0000` (or reboot) will bring it back when the
> owner chooses.
>
> ### §0b Second cold boot (07:20, same day) — reproduced; the deadline is the tell
>
> Identical outcome (same WER bucket `3f94fe28…`, IDD FAILED_POST_START), but the log
> bracketed the kill differently and exposed the key invariant:
>
> - Boot #2's log ends **inside the SetDevice retry loop**: attempts 1-2 at 01:51:08Z, then
>   **one IddCxSwapChainSetDevice call blocked for 22 s** (attempt 3 at 01:51:30), attempt 4
>   at 01:51:32, host killed ≈01:51:33 — no attempt 5, no abandon line.
> - **Both boots: host terminated at AssignSwapChain-entry + ~25 s**, regardless of whether
>   our callback had returned (boot #1 returned in 1 s and was still killed at +25 s; boot
>   #2 was still inside the callback). ⇒ Not a callback-duration violation and not our
>   watchdog: this looks like an **IddCx/UMDF post-start deadline** — the first swapchain
>   never becomes functional (born abandoned: the June-26 DxgkRemoveAdapter →
>   BLTQUEUE::Reset → MarkAbandoned chain), dxgkrnl does not re-offer during post-start,
>   and ~25 s after assignment the framework terminates the host
>   (FxVerifierDriverReportedBugcheck) → CM_PROB_FAILED_POST_START. Warm, the re-offer
>   arrives ~1 s after the abandon and everything proceeds.
> - LUID note: a pairing render-adapter LUID older than the physical Helios enumeration
>   LUID is NORMAL (the indirect pairing instance has its own LUID and survives physical
>   re-enumeration) — the warm 06:53 bind SUCCEEDED on such a LUID. Drop the
>   "stale LUID = broken" reading; the question is why the BOOT pairing's blt queue is
>   reset/abandoned and never re-created in time.
> - **WUDF dump writer now armed** (`HKLM\...\CurrentVersion\WUDF` `LogEnable=1`,
>   `LogMinidumpType=0x1122` full-memory; dumps land in
>   `C:\Windows\System32\LogFiles\WUDF\`) — the WER LocalDumps route does NOT fire for
>   framework-reported failfasts (confirmed empty across two crashes). Next cold boot
>   should finally yield the terminating stack; verify the theory against it before
>   designing the fix. Fix directions if confirmed: eliminate whatever resets the boot
>   pairing (the churn source behind MarkAbandoned — the actual root), or make the OS
>   re-offer/pairing re-create fast enough inside the post-start window; handling it from
>   inside LGIdd is impossible (the host process is the thing being killed).

**Milestone, owner-confirmed by eyes on the Looking Glass client** (the only evidence that
counts, per `HELIOS_FIRST_PRINCIPLES_AUDIT.md` §4): a very dark-red desktop background with
black padding top and bottom, the Notepad window (content completely black; the Notepad logo
was visible briefly, then gone), a visible live cursor, and the frame still occasionally
dropping to black. **This is the first time anything has ever been visibly displayed through
the full DWM-on-Helios → venus → IddCx → KVMFR → client pipeline.** All previous "frames
flowed" claims were instrument-level; this one is not.

Everything below is committed at parent `c8f9091` (+ submodule commits `05ea81f21f2` mesa,
`87f31263` dxvk-helios, `59482903` LookingGlass). Read
`HELIOS_FIRST_PRINCIPLES_AUDIT.md` — §1–3 for the contracts and plan, **§4b for the full
implementation/verification record of this session**. Do not re-derive it here.

---

## 1. Deployed stack (all live on the VM right now, all committed)

| Component | Version / hash | Deploy method |
|---|---|---|
| KMD `helios_kmd_render` | **22.22.40.0** (bump per deploy — verification depends on it) | `install-helios-kmd.ps1 -RestartDevice` (devcon), active image hash-verified |
| UMD `helios_umd` | ProgramData hash **f9609819** | `hotplug-helios-umd.ps1 -Mode ProgramData -NoProbe -RestartDevice` |
| ICD (mesa venus) | sync-alloc build, vulkaninfo smoke OK | `win_meson` + `install-helios-icd.ps1` |
| dxvk-helios | DxvkBuffer null-storage + unregister-before-throw | copied to `C:\Users\Rupansh\dxvk-helios`, `meson compile -C C:\Users\Rupansh\dxvk-build`, UMD fingerprint purge + rebuild |
| LGIdd | **6.53.8.135** (C5 state machine) | `win_looking_glass_idd` + `devcon update <pkg>\LGIdd.inf "Root\LGIdd"` |

P0 (all 5 items) and P1 (C5 state machine) are DONE — audit §4b itemizes them with the
verification evidence (ETW zero live invalid-NTSTATUS; the 194 events still replaying are a
frozen pre-fix triage buffer, clears on reboot; SEH shim linkage verified via the .map; etc.).

## 2. How to read what the owner saw (working hypotheses, NOT verified)

- **Dark-red background + black top/bottom padding** — the desktop wallpaper composed by DWM
  on Helios, letterboxed: the IDD mode is 1896×1030 inside a 1920×1080 client window.
  Expected geometry, not a bug.
- **Notepad window black (logo flashed once)** — the P2 content classes, exactly as the audit
  predicts: notepad is a GDI app, its window content lives in GDI redirection surfaces —
  the **C6** CPU-bytes↔venus-blob divergence — and/or the **C1** shared-surface identity /
  metadata-texture-fallback class in dwm's open path. P2's acceptance criterion is literally
  "GDI apps (notepad/cmd) legible in the capture; no black windows".
- **Cursor visible and live** — the IddCx hardware-cursor → LGMP pointer-queue path works
  end-to-end continuously (it is independent of the frame path).
- **Frame occasionally goes black** — OPEN. Two candidate classes: (a) real frames arriving
  with black content (e.g. one of dwm's alternating backbuffers never gets valid content —
  a C1-class per-buffer identity/import miss), or (b) client-side redraw artifacts. Note the
  tension with §3 below: if frames arrive steady-state at all, the stale-binding picture is
  "sporadically fed", not "never fed" — re-measure acquires while the owner interacts.

## 3. The sharpened open problem (reproducible ON DEMAND now)

With a bound swapchain and an idle desktop:
```powershell
schtasks /create /tn HeliosPoke /tr notepad.exe /sc once /st 23:59 /IT /f; schtasks /run /tn HeliosPoke
```
advanced dwm's presents on Helios (`C:\ProgramData\Helios\umd-<dwmpid>.log`, "DXGI Present: #N"
went 2→12) while the IDD acquired **zero** frames in the same window
(`looking-glass-idd.txt` stays at "pending"). The bind-transition itself delivered 3 frames
(these disarm the first-frame watchdog by design). So the §4 question stands but is now
cheap to iterate: **which pairing/swapchain instance do dwm's steady-state presents feed?**
Leads, in order:
1. Re-run the poke test while the owner watches the client (does the visible image change
   when acquires stay 0? if yes, the client is being fed by something other than acquire-path
   frames — re-examine; if the image changes exactly when acquires happen, count them).
2. ntoseye (KD-less: `ntoseye -b memory mcp --http 127.0.0.1:8080`) into dxgkrnl's blt
   queue / indirect-swapchain objects to see where present #N lands. `NTOSEYE.md` quirks doc.
3. If stale-binding confirms: the IDD alone cannot decide staleness (static display and
   fed-elsewhere look identical). Design lead: the UMD sees dwm's Helios presents, the IDD
   sees acquires — plumb a present counter over the existing `CPipeServer`/registry so
   "presents advancing ∧ acquires pinned for N s" triggers the (now always-completing)
   replug primitive.

## 4. Other open items, ranked

1. **P2 content correctness** (the black-notepad class): C1 allocation identity end-to-end
   (KMD attaches resources to the opener's venus context at OpenAllocation; versioned
   identity ABI replacing the `_pad` smuggling; ICD imports with recorded exact size;
   DELETE the metadata-texture fallback) then C6 CPU↔blob coherence (RESOURCE_MAP_BLOB at
   the VidMm segment offset — `WDDM_FAKE_VIDMM_RESEARCH.md` §C). Audit §1 C1/C6.
2. **P1 acceptance not run**: ten consecutive cold boots → owner sees the live desktop, zero
   manual actions. Needs owner-approved VM power cycles. The state machine handled this
   boot's stillborn-swapchain → abandon → re-offer → bind chain autonomously (log:
   `looking-glass-idd.txt` 01:23:52–53Z), and the previous driver's log captured the old
   D-B1 stall live (ReplugMonitor 00:15Z, then 68 min silence) — but one warm success ≠
   acceptance.
3. **Adapter restarts AV venus processes in the ICD** (new finding): all 5 dwm dumps today
   were restart collateral (AV inside vulkan_virtio under a yanked device — dangling
   BAR/ring mappings), zero steady-state crashes in ~5.7 h before + after. Restarts are
   already banned as ritual; longer-term the ICD needs adapter-loss hardening
   (VK_ERROR_DEVICE_LOST propagation — pairs with audit I-A6).
4. **C00000BB source never identified**: the frozen ETW buffer held 41× STATUS_NOT_SUPPORTED
   from some DDI. After the next reboot clears the buffer, re-run the ETW check
   (§5 recipe); if C00000BB reappears, hunt that DDI (suspects: K-B5
   GetStandardAllocationDriverData unhandled types, QueryAdapterInfo unknown types).
5. **P3 unchanged**: real venus-driven fences (C3), capability-driven sharing (C7.2/7.3),
   loud-fail DDI stub table.
6. Housekeeping: `HKLM\...\LocalDumps\dwm.exe` still set to FULL dumps (215 MB each,
   C:\HeliosDumps) — reduce to minidumps or delete the key once dwm is trusted stable.

## 5. Recipes proven this session (beyond §6f-2/§6g of HANDOFF_GDI_BLACKFRAME.md)

- **ETW invalid-NTSTATUS check** (the "AzureTriage" recipe, now concrete):
  ```powershell
  logman create trace HeliosTriage -o C:\Windows\Temp\ht.etl -ets -ow -mode sequential -p Microsoft-Windows-DxgKrnl 0x40000400 0xFF
  # ...exercise... ; logman stop HeliosTriage -ets
  tracerpt C:\Windows\Temp\ht.etl -o C:\Windows\Temp\ht.xml -of XML -y   # grep 'invalid NTSTATUS'
  ```
  Keyword 0x40000000 = AzureTriageLogging, 0x400 = DriverEvents. **Interpretation trap:** the
  provider replays a per-boot triage ring as DCStart-opcode events on adapter teardown —
  identical counts across captures = frozen history, not live emission. Steady-state capture
  with no device restart is the clean signal.
- **KMD version bump is mandatory per deploy** (`kmd_render/build.rs` FILEVERSION +
  `Cargo.make.toml` stampinf `-v`): deploy verification is by
  `DEVPKEY_Device_DriverVersion` + service ImagePath hash; identical versions made a stale
  active image undetectable. Also: the first `install-helios-kmd.ps1` WITHOUT
  `-RestartDevice` exits 1 at "reboot required" with the OLD image still live — pass
  `-RestartDevice`.
- **IDD deploy**: `win_looking_glass_idd` then
  `devcon update ...\x64\Release\LGIdd\LGIdd.inf "Root\LGIdd"` (never in-place DriverStore
  copy). The MSBuild "InfVerif.dll not found" error line is nonfatal noise (exit 0, cat
  signed).
- IDD log: `C:\ProgramData\Looking Glass (IDD)\looking-glass-idd.txt` (rotates per WUDFHost
  start). Client log: `/tmp/helios-looking-glass-client.log`. dwm DXVK log:
  `C:\ProgramData\Helios\dwm_helios_umd_dxvk.log`; per-pid UMD logs beside it. Host venus:
  `/tmp/helios-qemu-stderr.log`.
- Desktop-activity injection from SSH: the `schtasks /IT` poke above (session 1,
  interactive). D3D from SSH still fails 0x887a0004 — that limitation stands.

## 6. Invariants / warnings for the next session

- **No hacks, no kick rituals, loud failure over fake success** — standing directive.
- **Only owner-visible client output closes a milestone.** Instrument-level results are
  leads, not proof. (This session's milestone WAS owner-confirmed.)
- Never `taskkill dwm` and restart the Helios PCI device together (hard guest wedge).
- Every Helios device restart AVs venus processes (finding #3 above) — restarts also
  invalidate any crash statistics you are collecting.
- Ask the owner before rebooting/relaunching the VM. QMP: `/tmp/helios-tpm/mon.sock`.
- The virtio transport + venus client now POISON on timeout/fatal (fail-fast by design): a
  wedged host turns into loud DeviceError/alloc failures, not hangs. `CTRL_TIMEOUT_COUNT`
  lands in the CollectDbgInfo 'HDBG' report (13 DWORDs, magic 0x48444247).
- The tree is now COMMITTED (c8f9091) — keep committing scoped work from here on.

## 7. Copy-paste prompt for the next session

> You are continuing the Helios vGPU project in /home/rupansh/helios-vgpu. Read
> `HELIOS_FIRST_PRINCIPLES_AUDIT.md` in full (esp. §4b), then
> `HANDOFF_FIRST_VISIBLE_FRAMES_2026_07_03.md`. State: first owner-confirmed visible frames
> (wallpaper + window frames + cursor in the LG client); notepad content black; frame
> occasionally drops to black. P0+P1 are deployed and committed (parent c8f9091). Work in
> order: (1) resolve the steady-state feed question with the on-demand repro in handoff §3 —
> re-run the notepad poke while the owner watches, then ntoseye into dxgkrnl's
> indirect-swapchain/blt-queue if acquires stay 0 while the image changes are unexplained;
> (2) P2 content correctness per audit C1 then C6 (black GDI windows are the expected
> casualty of those two contracts — acceptance: notepad/cmd legible in the client);
> (3) P1 acceptance: ten consecutive cold boots, owner sees the live desktop, zero manual
> actions (ask the owner to run the boots). Evidence discipline: only what the owner sees in
> the client counts. No hacks, no restart rituals, loud failure over fake success. Ask
> before rebooting the VM. Commit scoped work as you go.
