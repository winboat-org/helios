# Handoff — Xid-109 freeze root-caused; TDR contract implemented (ICD deadline + UMD spin bounds + LGIdd watchdog fixes) (2026-07-04, eighth session)

Supersedes the *priorities* of `HANDOFF_FIRST_CONTENT_FRAMES_2026_07_03.md` (its content fixes
remain valid and deployed). **Owner directive this session: reliable IDD↔Helios pairing/acquire
is the TOP priority** — unreliable pairing makes every future change look like a regression.

## What the owner saw (cold boot, this session)

LogonUI **animated with real content** (the 7th-session content fixes hold on a clean cold
boot), then the LG client froze on a LogonUI frame at the login transition. Login had
completed guest-side; the display pipeline was dead.

## Root cause — the full chain (every link instrument-verified live)

1. **Host: `NVRM: Xid 109 CTX SWITCH TIMEOUT, name=vkr-ring-6` at 23:45:16** — the host
   NVIDIA driver killed the GPU channel serving dwm's venus context, 8 s after the last good
   frame. (`journalctl -k | grep -i xid` — check this FIRST for any future venus freeze.)
2. **The guest is never told.** The renderer keeps answering `vkGetSemaphoreCounterValue`
   with `VK_SUCCESS` + a stale counter (NVIDIA reports device loss lazily or never on this
   query). The ICD's existing warn-order device-lost probe therefore never sees
   `VK_ERROR_DEVICE_LOST`, and `vn_relax`'s ~15-min iteration abort never fires because DXVK
   re-enters waits with fresh relax states.
3. **dwm wedges permanently**: its DXVK submission thread Sleep-polls in `vn_WaitSemaphores`
   forever; its present thread spins in the UMD's `rotate_resource_backings` **unbounded**
   `while (GetData == S_FALSE) Sleep(0)` (dxvk_bridge.cpp) — 3078 CPU-seconds, one core
   pegged for 80 minutes. dwm stays alive but graphics-dead: presents stop, hotplug ignored.
4. **The IDD side then digs its own hole**: devcon-restarting the IDD hit the watchdog doom
   loop — `CD3D11Device::Init` takes 10–15 s under churn but `FIRSTFRAME_TIMEOUT_MS` was 10 s
   from bind (killed VALID pairings), the 10 s offer window replugs faster than dwm can
   rebuild (livelock), and after any Helios PnP restart (every UMD deploy!) the IDD's latched
   render-adapter LUID names a dead adapter so the OS never re-offers a swapchain at all.

On real hardware step 1 is a TDR and dwm recovers in ~2 s. Helios lacked that contract.

## Fixes (ALL implemented, deployed, and committed this session)

- **ICD `vn_queue.c` (icd/mesa a839f78f51c):** per-semaphore forward-progress deadline.
  Stalled ≥ `VN_HELIOS_SEM_DEADLINE_MS` (default 30 s; env, 0 disables) with a submitted
  signal op pending (pending sfb cmds — waits on future app submits exempt) → probe renderer
  once → DEVICE_LOST **or stale-success** latches `dev->helios_lost` and the wait returns
  `VK_ERROR_DEVICE_LOST`. DXVK latches `m_lastError` → D3D device removed → dwm restarts
  composition. Fences/queries not yet covered (see Remaining).
- **UMD `dxvk_bridge.cpp` rotate sync:** bounded (30 s deadline + `GetDeviceRemovedReason`
  check every 256 iters + `Sleep(1)` after 1024 spins); any GetData failure skips the
  rotation with a log instead of proceeding or hanging. Only unbounded spin in the bridge.
- **LGIdd (LookingGlass 7c0dd842):**
  - first-frame watchdog counts from **device-ready** (`NotifyDevicesReady` refreshes the
    state tick; `DEVICEINIT_TIMEOUT_MS` 45 s caps a wedged init);
  - **exponential replug backoff** 10/20/40/80 s (reset only by a real acquired frame);
  - **render-adapter LUID revalidation** on fruitless replugs (the once-per-adapter
    `SetRenderAdapter` latch goes stale when Helios PnP-restarts and mints a new LUID);
  - mid-stream acquire-stall = **detection-only breadcrumb** (auto-replug false-positived on
    an idle desktop within minutes — an idle desktop legitimately presents nothing; the real
    stall causes are covered by the ICD deadline and the ACCESS_LOST path).
- **`tools/hotplug-helios-umd.ps1`:** after re-enabling Helios it now also restarts
  `ROOT\DISPLAY\0000` so the IDD deterministically re-pairs against the fresh LUID.

## Verified end-state (live)

- Pairing converges after IDD restart in ~9 s: AssignSwapChain → device init → frames.
- ACCESS_LOST self-heals: abandon → unassign-confirmed replug → re-pair → frames (~50 s,
  no manual intervention, observed live).
- dwm stable (no crash-loop; the transient 0x8007001f udwm crash-loop during the deploy
  churn window self-cleared), no watchdog false positives after the detection-only change.
- Recovery ritual while anything is still wedged: **`Stop-Process dwm`** (winlogon respawns
  it with a fresh venus context). A devcon IDD restart alone does NOT unwedge a stuck dwm.

## Remaining work, in priority order

1. **Owner-visible acceptance**: interact with the LG client (mouse/typing = damage =
   presents). Only sustained owner-visible desktop closes the milestone (content pipeline is
   unchanged from the 7th session which had verified real pixels; this session's sampler
   reads only caught frame-1 zeros because the desktop was idle).
2. **H1 — RESOLVED to a concrete suspect (same session, validate boot).** Owner rebooted
   with `HELIOS_VKR_DEBUG=validate`; the FULL host validation log contains exactly ONE
   violation class: dwm pipelines binding `R16G16_SINT`/`R32_SINT` vertex attributes against
   **float32-typed shader inputs** (`VUID-VkGraphicsPipelineCreateInfo-Input-08733`,
   TEXCOORD0–4) — vertex-fetch UB; garbage SINT→float reinterpretation feeding dwm's
   composition shaders is the prime Xid-109 trigger (two more Xids fired at 00:51/01:11
   during churn, each as dwm rebuilt composition; the venus/ICD layer itself was
   validation-clean). Root cause of the float typing: **cold-boot dwm devices hit the
   UNTYPED legacy create_vertex_shader** because boot-time UMD resolution served a **stray
   pre-typed-fix `helios_umd.dll` at the DriverStore FileRepository ROOT** (dated 06-24;
   hash DC207B58, untracked) — same negotiated interface 0xb000f everywhere (raw-args dump
   proved the struct read correct); post-hotplug devices loaded the current DLL and were
   typed. The stray is DELETED; the UMD now logs its own module path per process
   (`UMD module:` line) and dumps raw CreateDevice args. **Second, deeper cause found on the
   next cold boot (violations persisted with the current DLL): the D3D11 runtime NEVER
   fills `ENTRY2.RegisterComponentType` — every signature entry arrives 0/UNKNOWN, for
   dwm's SM4 shaders AND the SM5 probe alike** (the session-6 "typed signatures" fix never
   actually received types; it silently defaulted everything to float32). Real fix (commit
   d8a2d97): **layout-driven VS input-class variants.** The input layout's DXGI format
   classes are the only available ground truth (any (layout, VS) pair the runtime lets bind
   matched the app's original ISGN); at `bind_input_layout`, a non-float layout triggers a
   (VS, class-set)-cached recompile with the synthesized ISGN's component types patched from
   the layout, and the variant is bound in place of the original. Verified live: dwm
   compiles and binds SINT variants, desktop frames flow. **Confirm on the next cold boot
   (validate on): ZERO Input-08733 in a fresh /tmp/helios-qemu-stderr.log** (this boot's
   render server exhausted the validation layer's per-VUID duplicate limit of 10, so
   mid-boot silence proves nothing), no Xid 109, live desktop through the login transition.
   ~30 stale DriverStore package dirs remain (only the root stray was load-bearing); prune
   opportunistically.
3. **Cold-boot verification PASSED (validate on): ZERO Input-08733 on the fresh boot** — the
   layout-driven variants hold from dwm's first composition. Killing the flood exposed two
   previously-drowned validation classes:
   - **Image memory-requirements inconsistency — FIXED (icd/mesa 3671722a8f2)**: the image
     twin of the session-7 buffer bug. `vn_GetDeviceImageMemoryRequirements` forwarded the
     PLAIN create info while `vn_CreateImage` injects the external handle type into LINEAR
     images → undersized dedicated allocations + disallowed memory types (VUID 02964/01615/
     01617) from dwm's composition images. Verified in-boot: zero new instances after the
     ICD redeploy (dedupe budget for those VUIDs was not exhausted, so silence is meaningful).
   - **Command-buffer lifecycle violations — NEXT FRONTIER (open)**: `vkResetCommandPool`/
     `vkBeginCommandBuffer`/`vkDestroyCommandPool` on PENDING command buffers, bursting in
     CHURN windows (boot device handoff, dwm restarts/kills). Working hypothesis:
     teardown-under-load — device/context teardown proceeds while the host GPU still
     executes (either guest teardown paths skipping an effective wait-idle, the vn feedback
     recycler resetting its own pending feedback CBs, or forced process kills reaching vkr
     context destroy with work in flight). This ALSO matches every observed Xid-109 timing
     (login transition = dwm device teardown; kill windows). Investigate with per-line
     timestamps + correlate against guest device-destroy logs.
4. **Extend the forward-progress deadline to fence and query feedback waits** (same shape as
   semaphores in vn_queue.c). dwm's kill path was semaphores; games may wedge on fences.
4. Backlog unchanged: rotation cost (C3 async-fence), buffer-reqs cache always-miss, §5
   residual C1 boot hole, instrument hygiene (`RotateSample` still 16 — set 0 for production
   runs), probe verdict-line cleanup.

## Debug recipes that cracked this (reusable)

- ntoseye **memory backend needs no halt**: `threads(pid)` wait reasons → read
  `KTHREAD+0x90` TrapFrame → user RIP/RSP → scan user-stack qwords against `modules` ranges.
  Spinning thread = `Get-Process` CPU delta over a few seconds.
- The MinGW ICD DLL carries DWARF: `llvm-addr2line -e vulkan_virtio.dll <ImageBase+RVA>`
  symbolizes guest stacks on the Linux side (ImageBase from `objdump -p`).
- Scan-walked stacks contain STALE frames — a frame mid-stack (e.g. `CompletePnPTransition`)
  is not proof of the live call chain; verify against the wait object
  (`KTHREAD.WaitBlockList → KWAIT_BLOCK.Object → inspect_object_header/describe_address`).
- schtasks `/it` tasks (notepad, cmd loops) are NOT a reliable desktop-damage generator for
  forcing dwm presents; owner interaction through the LG client is.
