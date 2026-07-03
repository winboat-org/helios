# Handoff — FIRST USER-VISIBLE FRAMES (2026-07-03)

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
