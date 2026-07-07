# ROADMAP — Stage: Performance, Stability, Conformance (PSC)

*Started 2026-07-05, the day the desktop first rendered end-to-end under Helios
(DWM composites on Helios → venus → host GPU → IddCx → Looking Glass). Bring-up
is over; this stage makes it reliable, fast, and D3D11-conformant. Archived
bring-up knowledge lives in `docs/archive/`; operational debug knowledge stays
in `NTOSEYE.md` and `BRINGUP_QUIRKS.md`.*

## Verified baseline (2026-07-05, KMD 22.22.50 — current build 22.22.61)

- Adapter binds `CM_PROB_NONE` across cold boots and `devcon restart`.
- Segment topology: aperture (id 1) + **BAR window head as CpuHostAperture
  memory segment (id 2, 1 GiB)** — `BarSegMode 10`, the compiled default.
  Rule discovered via ETW: dxgmms requires a SupportsCpuHostAperture segment
  to be the LAST segment; the classic CpuVisible shape is rejected outright.
- Desktop renders: solid-color plate, icons with ClearType labels, taskbar/tray
  text, live window updates, regedit classic-GDI text. dwm/explorer on
  `helios_umd.dll` (no WARP).
- Doom 2016 previously verified 120+ fps through venus on the NVIDIA host
  (offscreen path; pre-WDDM-desktop milestone).

## Current priorities (2026-07-07, 33rd session — owner-set, in order)

These supersede the day-to-day defect lists below where they conflict; the
workstream sections hold the detailed evidence.

1. **D3D11 windowed apps render transparent — investigate & fix.** Windowed
   D3D11 swapchains (FaceWorks, Fire Strike windowed) show a transparent/black
   client area even when placed on-screen at the right size, while the desktop
   and window frames composite fine. Established this session: the app DOES
   render correct content (`HELIOS_PRESENT_READBACK` source non-black at
   1264×681); it is NOT alpha (`HELIOS_PRESENT_FORCE_OPAQUE` no-op, owner-
   confirmed); it is NOT a two-memory split (the KMD adopt path backs the alloc
   with the DXVK venus image — an earlier "KMD zeroes the resid" reading was a
   UMD struct-layout misread, see below); and it is NOT the IDD (both the D3D11
   fallback and the dead D3D12 path capture the same single composed IddCx
   surface — D3D12 is dead only because our UMD has no D3D12). The live thread:
   **DXGI `EnumOutputs`/`GetDisplayModeList` racily return `0x887a0022`** on the
   Helios adapters, DWM never imports the app's flip backbuffer (its max
   imported resid trails the app's), and there are **two identically-named
   "Helios vGPU Render Adapter" DXGI entries that both resolve to the same
   physical WDDM adapter** — a stale-LUID residue from repeated device restarts.
   CCD pins the live output to one LUID; the other is a phantom. NEXT: reboot to
   clear stale adapter LUIDs (owner approval), re-run
   `adapter_live_probe`/`ccd_adapter_probe`/`dxgi_output_modes_probe`; if a
   single clean adapter still fails `EnumOutputs`, fix the render-adapter↔IddCx
   output exposure so DXGI enumerates the desktop output (31st-session split,
   broader). Detail: memory `phantom-adapter-luid-enumoutputs-33rd`,
   `faceworks-black-d3dflip-twomemory-split-33rd`.

2. **Slow first-paint on some windows — our UMD makes DWM wait.** Settings app,
   parts of Explorer on fresh open, and (easiest repro) the **UAC dimmed
   window** take several seconds to render. Suspected a UMD-side present/consumer
   wait or a per-window gate stalling DWM's first composition of these surfaces.
   Likely related to #1's consumer/import path. NEXT: measure — instrument the
   present-wait / gate-flush / consumer-wait counters against a UAC-window repro,
   find which wait blocks and why it only bites first-paint.

3. **Codebase cleanup (HIGH).** Many paths accreted across bring-up sessions add
   overhead or cause minor misbehaviours: retired diagnostic scaffolding, dead
   knobs, superseded present/staging paths, force-* diagnostics, staged-probe
   machinery, and now-falsified experiments (e.g. the `DECLARE_CROSS_ADAPTER_RESOURCE`
   line, broad-adopted-BAR remnants). Audit the UMD present path, dxvk-helios
   staging/refresh layers, and KMD segment/adopt code; delete or gate what is not
   load-bearing, with before/after behaviour verified. Do this before large new
   feature work so #1/#2/#4 land on a clean base.

4. **Performance — Fire Strike fullscreen (~100 fps @1080p, GT1).** Owner
   believes near-2× is reachable. Render path is healthy (fullscreen renders
   correctly). Measure first (venus submit/fence latency, copy/acquire gates,
   present-to-scanout) then remove known costs. See WS2 for the levers already
   mapped (feedback-shadow retire, dcomp vehicle, copy-latency).

## Workstream 1 — Stability

**IDD frame freeze: DIAGNOSED 2026-07-05 (17th session), live on the frozen boot** — full chain
in memory `idd-freeze-root-cause-chain`. Summary: (1) routine multi-second completion stalls
(per-present full-GPU drain in `rotate_resource_backings` + event-cadence desktop) →
(2) the 4×8 s sem-deadline latch declares CONTEXT LOST on a healthy-but-slow stack →
(3) dxvk teardown on DEVICE_LOST resets command pools with host work pending (= the
`vkResetCommandPool` VUs; symptom, not cause) → (4) post-loss, `submitCmdLists` drops cmdlists
WITHOUT `notifyObjects()` → in-use refs leak → next `Map` → `waitForResource` (no timeout, no
lost-check) wedges dwm permanently; win32k session-1 GDI hangs behind it. Falsified: the
early-fence/helios_sync theory for the steady-state stall — 0 of 251,810 submissions carry
ring≠0; the vn win32-sync signal path never fires; cross-process sync is dxvk-helios-internal.
**Status 2026-07-06 (18th session): the whole chain is now closed** — (1) the "stall" was the
sem-deadline misreading idle wait-before-signal waits (fixed, defect 1 below) plus the rotate
drain (fixed, WS2); (2) the latch no longer fires on idle desktops; (3)+(4) fixed 17th session.
Remaining: cold-boot + multi-hour soak, and the forced-loss test for the loss path (defect 2).

Open defects, roughly ordered:

0a. **Ghosting — ROOT-CAUSED AND FIXED IN LAYERS (21st session, 2026-07-06).** Owner
   insight proved out: the dirty-rect *attribution* was fine; STUTTER caused the
   ghosting. Evidence chain (all same-day): dwm's composed primary is paintcap-CLEAN
   under a bouncing-window probe while the LG client trails catastrophically → the
   corruption is in the IDD→KVMFR→client delivery; 1843 consecutive IddCx acquires
   show frame-delta exactly 1 (no skipped presents) and zero move-regions; WUDFHost
   consumer waits showed timeouts=0 while trails persisted → the wait "succeeded"
   against the WRONG instant. Root cause: the WS1 #4 consumer wait ran at cmdlist
   START (refreshHeliosStagedImages), re-reading the publish slot when the list
   BEGAN — under load that predates the acquire the list's copy serves by a full
   consumer cycle, so the wait targeted an already-retired value and the copy read
   ring-stale content inside freshly-reported damage rects. Fixes landed:
   - dxvk-helios `6eab004c`: bounded present-wait ON THE CS THREAD at copy-execution
     time (copyImage/copyImageToBuffer, imported sources). The acquired buffer can't
     be re-presented while held, so the slot value at that moment IS the acquired
     present's value. Silent no-slot returns (unordered reads) + fast-path hits now
     counted in the `present-wait:` line (`fast=`, `noslot=`).
   - dxvk-helios `35fe0912`: WUDFHost.exe app profile heliosPresentWaitUs=500000 —
     the copy-time wait exposed real producer lag (dwm fence frozen 250ms+ behind
     its publishes under churn; 32ms bound timed out exactly when ordering mattered).
     dwm keeps the tight 32ms default.
   - LGIdd `13630d7f`: D3D11 readback path (the ACTIVE path — D3D12 device creation
     fails 0x887A0004 by design, the D3D12 partial-copy path is DEAD code) finishes
     the acquired frame AFTER the staging Map (was: at CopyResource submit — dwm
     could re-render the buffer before the copy executed), reuses the staging
     texture (was: CreateTexture2D per frame = venus alloc/free per frame), adds
     per-stage QPC telemetry (`D3D11 path stats:` 1 line/300 frames), and a
     `HeliosForceFullDamage` registry knob (diagnostic; owner-verified to hide
     ghosting).
   - LGIdd `37356f83`: pending-damage debt — frames dropped after their dirty rects
     were consumed (LGMP queue full fired exactly at the login→desktop transition:
     giant permanent cold-boot ghosts, owner screenshot) bank their damage for the
     next delivered frame; new-subscriber re-post now carries full damage
     (damageRectsCount=0) instead of the last frame's partial rects.
   - LGIdd `5d36c512`: IddCx MOVE REGIONS delivered as damage (DestRect per move) —
     previously never queried, silently dropped (window drags are moves+edge-dirt;
     zero moves observed from programmatic MoveWindow, so this wasn't the trail
     cause, but it was a real hole).
   - LG client `b27eb1d0`: malformed frames (zero geometry) and transient
     onFrameFormat failures skip-and-retry instead of exiting — a client exit tears
     down the whole VM via the launcher (observed live: render-server killed dwm's
     venus context → torn frame → client exit → VM shutdown).
   Remaining for 60fps IDD (owner target): the serialized IDD cycle measured
   map(wait-inclusive)+memcpy ≈ 60-95ms under churn. `AllocCached` KMD fix (22.22.53)
   targets the 36ms WC-read memcpy; producer completion lag (dwm CS submission lag +
   a RECURRING ~1.49s stall, constant across configs — hunt with QMP fence tracing)
   is the other half. See WS2. **22nd session: the ~1.49s stall is ROOT-CAUSED AND
   FIXED (our own staged-probe diagnostics — see WS2); present-wait timeouts now 0.**

0b. **NEW DEFECT — venus pipeline-layout lookup failure killed dwm's context
   (2026-07-06, host-side evidence).** virgl render server:
   `vkr: failed to look up object 626238 of type 19 (pipeline layout)` →
   `vkCreateGraphicsPipelines resulted in CS error` → fatal decoder state → context
   1613 (dwm.exe) destroyed → all subsequent blob creates refused → LG client choked
   on the torn frame and exited → launcher shut the VM down. First host-side proof
   of a guest venus protocol violation.
   **22nd session (2026-07-06): the fork-early-out suspicion is FALSIFIED as the
   cause.** Static audit: vn_pipeline.c/vn_common.c are byte-identical to upstream
   (the relaxed tail load in wait_all is upstream's own); the fork's divergence is
   confined to vn_ring.c. Evidence: the never-read ring-diag file at
   `C:\Windows\Temp\helios_icd_diag.log` (13.5 MB across 6 boots) shows the
   shared-valid early-out NEVER fired and the wait_seqno fatal-abandon fired
   exactly twice — both AFTER the host had already latched ring-FATAL
   (consequences of the CS error, not causes; on the death boot the first
   guest-side anomaly IS the post-FATAL abandon, head frozen at 141452 with
   ~1.2 KB undecoded). The sync-vs-async structure is also sound: TLS-ring
   pipeline creates are `vn_call_` (synchronous), so destroy-after-create races
   are excluded. The initiating violation left NO guest-side trace. Landed
   (mesa `a0412461012`, ICD `vulkan_virtio-7c9ddf378055` deployed):
   `vn_ring_wait_all` barrier skips/abandons now log LOUD
   (`BARRIER SKIPPED/ABANDONED in wait_all`) to the ProgramData diag log —
   either line on a healthy process is the smoking gun; ring diag rerouted
   from `C:\Windows\Temp` (unwritable for WUDFHost — its ring diags silently
   vanished) to `C:\ProgramData\Helios\helios_icd_diag.log`; post-fatal
   per-submit spam rate-limited (power-of-two); NEW env knob
   `VN_HELIOS_PIPELINE_TRACE` traces (ring, primary_tail seqno, object id)
   for layout create/destroy + the barrier + pipeline creates. NEXT on
   recurrence: read the BARRIER/PL-TRACE lines before theorizing; host-side
   `HELIOS_VKR_DEBUG=validate` relaunch (ask owner) — NOT VIRGL_LOG_LEVEL
   (HOST.md §5.1: venus runs in the render-server child; only WARN+ reaches
   the qemu stderr log).

1. **Stall — ROOT-CAUSED AND FIXED (18th session, 2026-07-06).** The strike attribution
   (mesa `f7a816f182f`: sem id, wait reason, signal queue/family/ring, signal value+age in the
   strike line) cracked it in one boot: **every observed strike had `sig_age_ms ≤ 14`** — the
   waiter had legally parked a wait-before-signal on the NEXT frame's timeline value while the
   desktop idled; the moment the next frame's burst submitted the signal, the old deadline
   (which measured time-since-counter-movement, gated only on a-signal-is-pending-NOW) fired
   against a milliseconds-old signal. Explorer striking at exactly hh:mm:00 = the taskbar clock
   tick submitting that signal. The strikes were false positives BY CONSTRUCTION on an idle
   desktop, and 4 of them during login churn are what tripped the DEVICE_LOST latch (act one
   of the 2026-07-05 freeze cascade). FIX: the deadline now measures how long a submitted
   signal has been pending with zero movement (`pending_signal_since_ns`); a genuine zombie
   (Xid-109: pending forever) still latches at strikes×deadline. RESULT: strikes went from
   ~1/min/process to ZERO across dwm+explorer+WUDFHost (fixed ICD, ~10 min incl. idle).
   The second stall contributor — the rotate drain — is fixed under WS2 (presents now track
   damage rate: ~10/s under the 10 Hz flasher vs ~6/s before). 19th-session soak data
   point (2026-07-06, warm boot): ZERO non-forced strikes and zero DEVICE_LOST over the
   whole session (~1 h incl. heavy GPU probe workloads); present-gate timeouts flat at the
   startup value; rotate-perf 1–4 µs; HELIOS_QUEUE_PERF live in dwm, submit phases all
   µs-class (submit_avg ~4 µs). Remaining: multi-hour/day soak.
2. **dxvk-helios device-loss hygiene — FIXED and FORCED-LOSS-VERIFIED (19th session,
   2026-07-06):** (a) dropped post-loss submissions now `notifyObjects()` + recycle (was:
   permanent in-use ref leak → dwm compositor wedge); (b) `waitForResource` bails on
   DEVICE_LOST (loud warn); (c) on lost, bounded 2 s grace wait then deliberate cmdlist
   leak instead of resetting a pool with host-pending buffers (was: the vkResetCommandPool
   VUs). End-to-end forced-loss test PASSED (`VN_HELIOS_SEM_DEADLINE_MS=1` +
   `_STRIKES=1` on d3d11_upload_integrity_probe): latch fired on the first genuine 1 ms
   pending window with full attribution, probe ran to completion with loud FAILs (reads
   return zeros post-loss) and exited — no wedge; all three hygiene paths logged in the
   dxvk log (`leaking command list with unretired host work`, `waitForResource aborted`,
   repeated loud submit failures); dwm untouched. Note: once the ICD latches, host
   retirement becomes unobservable, so the deliberate leak fires even on a healthy host —
   by design.
3. **dwm shared-resource creation failure — FALSIFIED as written (18th session, 2026-07-05)**:
   every `DXVK memory not importable` line (current boot AND the freeze-evidence tail) belongs
   to `misc=0x0` PRIVATE textures, where suballocation is correct. Shared/present creations
   already get dedicated importable memory (`forceDedicated`, dxvk_image.cpp:634): dwm's
   backbuffers allocate `kind=DEVICE_MEMORY` blobs (blob=venus mem id, KMD creates the wire
   resource) and the IDD imports them (resids 32/33/36, live content probe-verified). The
   `res_id=0` in the allocate log is normal for KMD-created blobs, not a failure. Log line now
   scoped: loud `SHARED RESOURCE WITHOUT IMPORTABLE BACKING` only when the resource actually
   needs one (shared/keyedmutex/present/primary); private-texture noise trace-gated. The IDD's
   per-acquire re-resolve is the alias-staging refresh design, not a missing-export symptom.
   Residual noise, harmless: dwm warns `Failed to write shared resource info` 9× (dxvk shared
   metadata runs before the UMD stamps KMT handles; WINE-escape fallback fails by design).
4. **KMD wire-fence semantics / consumer-side present ordering — MECHANISM PROVEN
   END-TO-END (19th session, 2026-07-06).** `tools/vk_ring_fence_probe.cpp` (schtasks
   `helios_ringprobe`, MEDIUM integrity — see tooling gotcha) demonstrates the full chain
   on the live stack: vkQueueSubmit2 signaling an exported win32 timeline semaphore →
   vn ring_idx≥1 fence-only SUBMIT_VENUS (`vkWaitRingSeqnoMESA` cs, orders the fence
   behind the ring-decoded queue submission) → KMD INFO_RING_IDX wire fence (the transport
   already honored ring_idx; in-flight table is per-fence token-matched, out-of-order-safe)
   → QEMU 11.0.1 → proxy → render-server vkr queue sync thread → host GPU completion
   (qemu fence trace: retire at 263 ms on a 269 ms workload; ring-0 retires in µs at
   decode) → used-ring completion → **new ICD retire thread** (mesa `4a6aa14f17b`: on
   every SHARED-sync signal, a per-renderer thread WAIT_FENCEs the wire fence and signals
   the shared WDDM monitored fence in retire order; helios_sync now refcounted; also fixed
   the `helios_sync_append_locked(fence_id=0)` blind-signal that cleared older pendings)
   → consumer `vkWaitSemaphores` on the re-imported semaphore returns at 97–98 % of
   T_gpu, never early. KMD `22.22.52` (BUILT, NOT yet installed — needs owner reboot):
   WDDM pending-FIFO watermark now counts ring-0 fences only (ring≥1 stay in flight for
   the whole GPU-work duration; counting them would couple GDI/paging DMA pacing to
   multi-ms GPU work) + RING_SUBMIT/RING_COMPLETE counters (TDR report v4).
   **PRODUCTION INTEGRATION DEPLOYED (20th session, 2026-07-06) — A/B running with
   `PresentGateUs=0`.** The design changed twice against reality:
   - **KMT is impossible**: dxgkrnl rejects a MONITORED fence with `Shared=1` and no
     `NtSecuritySharing` (0xc000000d, proven live), i.e. no global-DWORD flavor exists.
     The ICD's legacy-D3DDDI_FENCE fallback silently produced syncs whose FromCpu waits
     hang forever (wedged the probe) — fallback REMOVED, KMT semaphore caps no longer
     advertised (mesa `d5d698aaec5`). Rendezvous = **NAMED NT sharing**: standard
     `VkExportSemaphoreWin32HandleInfoKHR::name` / import-by-name
     (`D3DKMTShareObjects` w/ named OBJECT_ATTRIBUTES → `D3DKMTOpenSyncObjectNtHandleFromName`).
     dwm CAN create `Global\` names (SeCreateGlobalPrivilege verified on its token).
     `vk_ring_fence_probe named` rc=0 (98 % of T_gpu; NT regression 97 %).
   - **LGIdd needed NO changes**: its per-acquire copy runs on OUR UMD/dxvk inside
     WUDFHost (that instance imports dwm's backbuffers as alias images — resids
     probe-verified) — the consumer wait lives in dxvk-helios's
     `refreshHeliosStagedImages`, `dxvk.heliosPresentWaitUs` (default 0, WUDFHost.exe
     profile = 100000; imported-mode images only, so dwm never grows a CS-thread wait).
   Shipped shape: producer (`present_sync_publish`, umd `9d22620`+`a7f2dfa`, dxvk
   `b15bc42f`+`8c612b3d`) creates one named fence per D3D11 device
   (`Global\HeliosPresentFence_<pid>_<fenceId>` — dwm owns SEVERAL devices, per-pid
   names collided live; Everyone-DACL), records signalFence(++counter) on the frame's
   OPEN cmdlist (no present-thread wait), publishes (resid → pid, fenceId, value) in a
   seqlock-slotted mapped FILE `C:\ProgramData\Helios\helios_present_sync.bin` (both
   principals have ProgramData rights; do NOT delete the live file — mapped views
   split-brain until the mappers restart). Consumer imports by name (cached per
   pid+fenceId), `getValue` fast-path, bounded wait, timeout → copy anyway + loud
   `present-wait:` telemetry. Kill switches: `HKLM\SOFTWARE\Helios!PresentSyncPublish=0`
   (producer), `dxvk.heliosPresentWaitUs=0` (consumer). Deployed: UMD
   `helios_umd_8301bc9779a48b99.dll`, ICD `vulkan_virtio-cf1280da750c.dll`, verified in
   dwm+WUDFHost; cross-session import proven live ("imported fence of producer pid").
   First numbers (gate=0, 10 Hz flasher stress): ~900 consumer waits, avg 5–6 ms,
   ~4 % bounded timeouts in bursts when dwm's CS runs >100 ms behind (structural:
   the consumer chases the latest published value; bounded + loud by design), ZERO
   strikes, desktop paintcap-clean, submit_avg ~4 µs unchanged.
   **Owner cold-boot report (same session): ghosting + frame drops PERSISTED — the
   WUDFHost-only scoping was WRONG.** The old gate had been ordering EVERY producer's
   present (apps included), so registry gate=0 also unordered the app-backbuffer →
   dwm-composition edge; the artifacts were in dwm's OWN composed primary (stale
   sliver + content overhang in the owner's screenshot). Fix (dxvk `90b76c5c`, UMD
   `helios_umd_f2c6f833d7d293eb.dll`): `heliosPresentWaitUs` defaults to **32000 for
   every consumer** — the wait fires only for imported surfaces with a published
   slot (dwm/IDD cross-process edges), the wait DAG is acyclic (IDD → dwm → apps),
   every edge bounded; uniform 32 ms also caps the IDD stall bursts that read as
   frame drops (was 100 ms). Verified live: dwm imports app producer fences
   ("imported fence 1 of producer pid <app>"), zero dwm-side timeouts, churn
   paintcaps coherent. Timeout warns now log the fence's current value — churn
   bursts are retire-lag (publish outruns GPU completion by a few frames).
   **Remaining:** owner eyeball via Looking Glass + multi-hour soak with gate=0;
   then retire `PresentGateUs` (flip compiled default to 0) and drop the gate from
   the hot path. Differential levers if ghosting recurs: `DXVK_CONFIG
   "dxvk.heliosPresentWaitUs = N"` per process, or registry `PresentGateUs=32000`
   restore. Known residue: old-ICD probe runs leaked 4 idle render-server workers
   host-side (virgl-63/65/95/97); clears on VM restart, did not recur with the new ICD.
5. **dxgkrnl "Driver returned an invalid NTSTATUS 0xC00000BB"** (ETW
   AzureTriage) — some query answered STATUS_NOT_SUPPORTED where that return
   is illegal. Tolerated today; find and fix the query.
6. **WUDFRd cold-boot race** ("SCM not ready", boot+23s) — LGIdd loads late;
   pairing is resilient now but the race window is still there.
7. **In-place KMD update flakiness** — CM_PROB_FAILED_POST_START limbo until
   reboot is expected, but keep the version-coherence gotcha (three sites) and
   backup ladder in mind. 2026-07-06 state (19th session END, reboot-verified): ACTIVE
   driver = **oem59.inf = 22.22.52 (pkg 155b7345f9360525)** — per-ring watermark +
   counters live. `UserModeDriverName` → ProgramData `helios_umd_b3615be0ce9de13e.dll`
   (RELEASE profile), dwm ICD = **`vulkan_virtio-5535366186bd.dll`** (retire thread) —
   both verified by the fresh dwm's loaded-module list + paintcap. **NEW GOTCHA: a KMD
   `devcon update` install creates a new DriverStore dir and RESETS `UserModeDriverName`
   to the DriverStore copy, which ships the package's DEBUG-profile UMD — after EVERY
   KMD install, rerun `win_install_umd` (release dll + `-KillUmdUsers -RestartDevice
   -NoProbe`) and re-verify dwm's loaded module. The DriverStore UMD copy staying locked
   during that redeploy (script exit 1) is benign — ProgramData + registry are what
   load.** Backup: `C:\ProgramData\HeliosDeployBackups\20260706-021734`.
8. **GDI byte-path / executor retirement (REFRAMED 20th session)**: post-cold-boot,
   GDI content renders while RenderGdi (GdiE), MapCpuHostAperture (ChMn) and
   paging (Pg*) counters all stay idle — the canonical CPU path likely already
   carries the bytes. RESEARCH CONFIRMED (2026-07-06): GDI HW acceleration is
   OPTIONAL (`SupportKernelModeCommandBuffer` is a "should… only if" per
   gdi-hardware-acceleration.md); **viogpu3d** (vendored kvm-guest-drivers, the
   closest existing WDDM driver to Helios, near-identical cap set) never sets the
   bit and implements NO RenderKm/RenderGdi; WARP works render-only in the IDD
   slot. The in-tree "LOAD-MANDATORY" bisect (query_adapter_info.rs:166,
   2026-07-02 v22.22.34) predates Option A and is confounded by the then-broken
   CPU-visible story — retest under BarSegMode 10 via a new `GdiAccelMode`
   service-key knob + pnputil restart-device (AzureTriage names any FAILED_ADD).
   If the desktop renders accel-off: verify byte flow (GdXn stays 0, watch for a
   two-memory-split regression = black/stale GDI windows), verify the BAR CPU
   mapping is WB-cacheable (CPU GDI is read-modify-write), re-run the Doom
   stutter differential (its WSI BitBlt storm currently traverses the executor),
   then retire the gdi_blit executor (a guest-CPU blitter behind a DMA round
   trip — strictly worse than win32k's rasterizer, source of the 48%-drop bug
   class).
   **21st session (2026-07-06): both knobs SHIPPED in 22.22.53** (`85ad16a`):
   `GdiAccelMode` (default 1) gates SupportKernelModeCommandBuffer per the plan
   above; `AllocCached` (default 1) additionally answers the WB-cacheable
   question for ALL CpuVisible allocations — user views were mapped WC (only
   CpuVisible was set at create_allocation; the WDDM2 `Cached` flag was never
   set), measured at ~200 MB/s reads = 36 ms per 7.8 MiB IDD readback frame.
   **22nd session: GdiAccelMode=0 A/B PASSED — the 2026-07-02 "LOAD-MANDATORY"
   bisect is OVERTURNED under BarSegMode 10.** Evidence (all same-boot, via
   `pnputil /restart-device`): adapter re-adds `CM_PROB_NONE`; desktop composes;
   classic-GDI text renders (fresh cmd-console canary echoes + regedit tree text
   + desktop-listview labels, paintcap-verified); every Gd* counter FROZEN this
   boot (GdiE 339, GdXn 24 — zero executor traffic; the KMD's `GdiM` mirror
   flipped 1→0 proving the mode took); no two-memory-split regression under
   mover churn across three device restarts. **The knob is LIVE at 0 for soak**
   (service-key value set; revert = `reg delete ...\helios_kmd_render /v
   GdiAccelMode /f` + restart-device). NEXT: owner-attended Doom stutter
   differential (its WSI BitBlt storm no longer traverses the executor), then
   retire the gdi_blit executor + flip the compiled default.

## Workstream 2 — Performance

- **IDD delivered-frame cycle — MEASURED (21st session, `D3D11 path stats:` line,
  1/300 frames in the LGIdd log):** the swapchain thread is fully serialized, so
  stage sums ARE the delivered fps. Under a 50Hz bouncing-window probe:
  rects ~14µs | staging ~0 (reuse landed; was a venus CreateTexture2D per frame) |
  copy-submit ~25µs | map 13-59ms avg with a RECURRING ~1.5s max (suspiciously
  constant across configs/builds — a fixed timeout somewhere; hunt with QMP fence
  tracing `virtio_gpu_fence_ctrl/resp` during a mover run) | memcpy 34-38ms
  (7.8 MiB at ~200 MB/s = WC reads) | lgmp ~0. Owner target: 60fps IDD.
  **RESOLVED IN TWO STEPS (same session):** (1) `AllocCached` (22.22.53) did NOT
  move the stage — the ICD maps venus blobs through its own escape path using the
  host's per-blob map_info (the WDDM2 Cached flag only affects dxgkrnl-owned
  mappings); a one-shot BW probe (now permanent in LGIdd) split the sides:
  src strided-read 622 MB/s, memcpy 157 MB/s, IVSHMEM memset 27 GB/s — the READS
  of the WC venus mapping were the whole cost. (2) LGIdd `1d03b685`: MOVNTDQA
  streaming loads (CopyFromWC) → probe streamcpy 5.5 GB/s, frame memcpy stage
  36.5ms → **1.4ms**, delivered rate 14-18fps → **32fps = the probe's damage
  rate** (map now ~8ms avg; pipeline headroom ~100fps — a 60Hz producer should
  see 60).
- **The RECURRING ~1.5s stall — ROOT-CAUSED AND FIXED (22nd session,
  2026-07-06, dxvk-helios `bdbbc2ea`): it was OUR OWN staged-content-probe
  diagnostics.** QMP fence tracing (`virtio_gpu_fence_ctrl/resp` during a mover
  run) split it in one pass: host ctrl→resp ≤ 11.5 ms across 7891 fences (p50
  0.17 ms — host exonerated again), but ALL guest submissions went SILENT in
  ~1.49 s beats, 3 per cluster, every 600 IddCx frames, landing at tick
  600k+30 = the probe HARVEST tick (`HeliosProbeRecurTick=600`,
  `HeliosProbeHarvestTick=30` in refreshHeliosStagedImages). The harvest
  scanned the ~7.8 MiB probe readback BYTE-WISE through the WC venus mapping
  ON THE CS THREAD (~0.75 s per probe at WC single-byte rates × 2 probes per
  staged image × 3 imported dwm backbuffers = 3 beats/cluster), while WUDFHost
  held the acquired IddCx frame — starving dwm and every producer behind it.
  Corroboration: probe issue/result log lines at ticks 2400/3000/3600 match
  the stall frames exactly; the 48-probe cap matched the observed stall
  cutoff (last cluster at tick 3600, none after); this also explains the
  "constant across configs" property (the probes shipped in every build), the
  LGIdd map-max 1.47-1.54 s, the "dwm fence frozen ≥500 ms" producer-lag
  signature, AND all 10 consumer present-wait timeouts (fence exactly one
  behind = dwm's next signal parked behind the block). FIX:
  `dxvk.heliosStagedProbes` config knob, default OFF (re-enable per process
  via DXVK_CONFIG for black-surface triage); harvest now bulk-copies WC →
  cacheable before scanning (~50 ms, not ~1.5 s, if ever re-enabled).
  VERIFIED live (mover churn re-trace, UMD `helios_umd_81a033e237bef769.dll`):
  max silence 48.5 ms (was 1493 ms), submission rate flat through every
  600-frame boundary, `present-wait: timeouts=0`, desktop paintcap-clean.
- **Producer (dwm) completion lag:** publishes outrun the fence by 1-4+ presents
  under churn; vkQueueSubmit2 phases are µs-class (queue_perf), so any residual
  lag is dxvk-CS-thread backlog + venus decode/retire path, NOT the submit
  call. The dominant "frozen fence" signature was the staged-probe stall
  (above, fixed). RE-MEASURE under 60Hz content before more work here;
  if residue: instrument dwm CS latency (record→execute) and the ICD retire
  thread's WAIT_FENCE throughput; consider moving dwm's own consumer waits from
  list-start to copy/sample-time like the IDD fix.

- **Per-present full-GPU drain — FIXED (18th session, `3579ef7` + `ef6689b`):**
  `rotate_resource_backings` drained the whole device (event query + `Sleep(1)` spin —
  the timer quantization WAS the 15–25 ms) on dwm's present thread every present. Now a
  CS-side identity rotation mirroring upstream `D3D11SwapChain::RotateBackBuffers` via
  `InjectCsOrderedAfterPending` (dispatch open chunk + ordered inject, NO CS-thread wait —
  the first iteration's `SynchronizeCsThread` blocked up to **1.9 s/present** behind login-
  churn CS backlogs = the post-cold-boot "occasional framerate dips"). `rotate-perf` after:
  **1–4 µs/rotation**; presents track damage rate (~10/s under the 10 Hz flasher, was ~6/s).
  WARNING for future work: do NOT per-image `waitForResource` on the present thread — a
  bound backbuffer RTV is re-recorded into every new open cmdlist, so `isInUse` never
  clears and dwm wedges (proven with a live minidump).
- **Ghosting after the drain removal — GATED (18th session cont., `4f0a96c`):** the drain
  had been accidentally serializing dwm's GPU work against the IddCx consumer's per-acquire
  copy; with it gone, the copy occasionally read a just-presented buffer whose GPU writes
  were in flight (dwm's venus rendering produces NO dxgkrnl-visible DMA fences — nothing
  else orders the cross-process read). Fix: bounded frame-completion gate in the DXGI
  present DDI (`HeliosWaitFrameComplete`: flush, then poll the submission fence — signals
  at GPU completion) before `pfnPresentCb` makes the flip visible.
  `HKLM\SOFTWARE\Helios!PresentGateUs` (default 32000; 0 = off, the A/B lever). Measured:
  avg 3.6–5.3 ms, timeouts only during startup churn, present rate unaffected
  (`present-gate:` telemetry, 1 line/128). The ARCHITECTURAL fix stays WS1 #4: per-ring
  wire fences + cross-process win32-sync so the consumer waits GPU-side.
- **Release-profile UMD deployed (18th session, `32cf4a4`):** dev profile was opt-level 1;
  deploys now build `--release` (opt 2 + thin LTO, `debug="full"` keeps the GUID-matched
  PDB for minidump symbolization). Deploy with
  `win_install_umd ["-UmdDll","C:\\Users\\Rupansh\\helios-vgpu\\umd\\target\\release\\helios_umd.dll", ...]`.
  dxvk-helios stays meson debugoptimized (-O2 already).
- **Frame-update slowness** (owner-visible): remaining suspects after the drain fix =
  dxvk-helios persistent-refresh (14th session, alias-image staging + per-frame refresh).
  Quantify with `HELIOS_QUEUE_PERF` (machine env set 18th session; reaches dwm after the
  next reboot; one aggregate line per 300 submits to
  `C:\ProgramData\Helios\helios_queue_perf.log`).
- **Diagnostics overhead — GATED 2026-07-05** (default quiet; knobs restore):
  UMD per-op log I/O was open/write/close per line on per-frame paths → now a
  persistent handle + `trace_line!` behind `HKLM\SOFTWARE\Helios!UmdTrace`
  (e88f2c6; umd log 8612→681 lines/30 s under churn). ICD submit/shmem trace
  (33 MB+1.9 MB per session, fopen per submission) behind `HELIOS_SUBMIT_TRACE`
  env (mesa 4bb43194e5d). KMD: S-ring `diag::record` off + gdi_blit's 20-value
  per-batch registry dump deferred to every 64th batch behind service-key
  `DiagLevel` (bf0ab37, **22.22.51 ACTIVE since the 2026-07-05 reboot**, pkg
  c393e58c1b189688 / oem58). Also cleared the stale
  `HKLM\SOFTWARE\Helios!RotateSample=16` debug knob (was CPU-reading back the
  whole swapchain ring every 16 rotations). GOTCHA found while measuring: pids
  get reused and `umd-<pid>.log` appends — delete logs before comparing runs.
  Present-rate deltas across dwm restarts are confounded by IDD-pairing state;
  compare within one dwm session only. 18th-session regression found+fixed
  (`a6780f1`): the persistent Rust log handle made the bridge's `fopen_s`
  (deny-sharing) fail on every call — ALL `[dxvk-bridge]` lines incl.
  rotate-perf were silently dead in post-e88f2c6 builds; bridge now uses
  `_fsopen(_SH_DENYNO)`. If a telemetry stream goes quiet, check for a
  string-in-binary vs lines-in-log mismatch before trusting it.
- **Doom present path — async WSI worker LANDED (22nd session cont.,
  mesa `808c7e4a786`, ICD `vulkan_virtio-1b44e8d36fe4` deployed): 120 → 160 fps.**
  The sw WSI present serialized the frame-fence wait (5-6 ms: GPU frame +
  venus retire) + StretchDIBits (0.65-0.75 ms) on Doom's present thread
  (`helios-doom-wsi-perf.txt` wait_avg/stretch_avg = the 120 fps ceiling).
  Now: per-swapchain worker thread does wait+invalidate+blit; acquire's
  IDLE condvar is the back-pressure (run-ahead = image_count-1); kill
  switch `HELIOS_WSI_ASYNC_PRESENT=0`. vkcube 6,600 fps; worker wait_avg
  4.3 ms now overlaps the app's next frame. **CRASH POST-MORTEM in the same
  arc: an unconditional 5-image sw-swapchain bump crashed Doom at renderer
  init ×2 (idTech sizes per-image arrays to the REQUESTED count — unhandled
  C++ FatalError, Crash.00003/00004). Spec-legal ≠ app-safe; extra depth is
  now opt-in `HELIOS_WSI_EXTRA_IMAGES=N` (default 0), for engines that
  re-query.** OWNER DECISION (2026-07-06): the sw present path is FROZEN at
  160 fps; the next present work is HW-accelerated present. The async sw
  worker remains the fallback path + kill switches. Host-side during Doom:
  p50 fence 0.05 ms; a tight ~10.7 ms class at 53/s = ring≥1 GPU-completion
  fences seeing the pipelined queue backlog (healthy).
  **D3DKMTPresent FEASIBILITY RESEARCH (same session, 26100 SDK d3dkmthk.h +
  live-counter evidence):**
  - `D3DKMT_PRESENT` itself is fully documented (hContext — gate5a already
    owns one — hWindow, hSource allocation, Flags, INLINE PresentHistoryToken
    built by the CALLER). The wall is the TOKEN: every redirected model
    (GDI/GDI_SYSMEM/BLT/FLIP/COMPOSITION) requires `hLogicalSurface`/
    `hPhysicalSurface` (win32k logical-surface handles) or dxgi-private
    rendezvous state (`dxgContext`, `hCompSurf`, `confirmationCookie`,
    `PresentLimitSemaphoreId`) minted only by the D3D runtimes through
    win32k-private (win32u NtGdiDdDDI*) calls. NO documented path lets an
    ICD mint a valid redirected token.
  - **Blt-model is DEAD on 26100 regardless**: the KMD's DxgkDdiPresent
    feasibility trace (display.rs PBcall/PBsrc/PBdst, present since .34-era)
    has NEVER fired across weeks of desktop uptime (dwm, steamwebhelper,
    Settings, taskmgr, D3D11 apps) — modern win32k drives flip/composition
    models exclusively; a real KMD present-blit would serve a path Windows
    no longer invokes. (Also: a KMD GPU blit needs venus Vulkan encoding —
    viogpu3d's equivalent rides VIRGL_CCMD_RESOURCE_COPY_REGION, a virgl
    primitive venus lacks; a kernel venus Vulkan client is weeks of work
    for that dead path.)
  - Composition-swapchain API (documented "present from Vulkan" API) needs
    a real D3D11/D3D12 device at CreatePresentationFactory → recursion veto
    (and no D3D12 UMD exists). DEAD.
  - **Viable roads, pick after owner gameplay numbers:** (1) Helios-private
    independent-flip-at-the-consumer: for an unoccluded fullscreen-sized
    Vulkan window, the ICD publishes (resid, fence, geometry) via the WS1 #4
    present-sync channel and LGIdd's per-acquire copy sources the APP's blob
    instead of dwm's composition — semantically DXGI independent flip
    implemented at the IDD; every piece exists (publisher wire format,
    LGIdd dxvk import-by-resid, bounded consumer waits); the ONE design risk
    is the foreground/occlusion contract (must provably fall back to dwm
    composition the moment the window isn't the whole visible screen).
    (2) Pinned-build flip-model RE: reverse the dxgi↔win32k rendezvous
    (win32u NtGdiDdDDI* on 26100, which Helios pins as the guest build) to
    mint real flip tokens = true zero-copy dwm flip for windowed too; high
    RE effort, build-pinned fragility. (3) sw path stays the composed-window
    contract (already async; blit 0.7 ms; the dwm-side staged upload is the
    remaining per-composite cost). **(4) NEW — the dzn/zink-style dcomp road
    (in-tree PROOF in wsi_common_win32.cpp's dxgi branch, used by dzn):
    flip-model presents WITHOUT minting tokens — DXGI + DirectComposition
    mint them: `CreateSwapChainForComposition(device_or_queue, FLIP_SEQUENTIAL,
    ALLOW_TEARING)` + `DCompositionCreateDevice(NULL)` (dcomp needs NO
    rendering device) + `CreateTargetForHwnd(hwnd)` → `visual->SetContent
    (swapchain)` → `Commit()` — all documented public API. The one real
    device required is the swapchain's presenting device; a WARP D3D11
    device satisfies it with NO helios_umd recursion (d3d10warp.dll,
    self-contained). Frame flow: venus host-visible frame (cached mapping)
    → CPU copy into the WARP backbuffer → Present(0); dwm opens the WARP
    buffer cross-adapter (proven path — this box ran a WARP-composited
    desktop pre-milestone). Throughput ≈ the sw path (the bytes still cross
    guest→host once per composite, plus one extra CPU copy vs StretchDIBits);
    the wins are flip-model pacing/damage semantics, tear control, and no
    GDI redirection surface. Zink itself is only a consumer: it rides the
    underlying ICD's VK_KHR_win32_surface via kopper — on venus that is our
    sw path; dzn is the driver that exercises the dcomp branch.**
    **OUR OWN D3D11 PRESENT MODEL (read 2026-07-06, umd forward.rs
    `dxgi_present`): windowed D3D11 is ALREADY hardware-presented end-to-end.
    The DXGI runtime solves the rendezvous FOR the UMD — `DXGI_DDI_ARG_PRESENT`
    delivers BOTH `hSurfaceToPresent` and `hDstResource` (the win32k
    redirection/destination surface as a first-class UMD resource); the UMD
    does `CopySubresourceRegion(dst←src)` = the present blit runs GPU-side
    through venus (zero CPU bytes), then WS1 #4 present-fence publish + the
    bounded frame gate + `pfnPresentCb(hSrc, hDst)` — the KERNEL mints the
    history token (allowed in kernel mode; that's why DxgkDdiPresent never
    fires — the blit already happened in the UMD). dwm's own presents are
    flip-model onto the IddCx buffers (no dst → copy no-ops). CONSEQUENCE:
    only the VULKAN client class (native VK games + vkd3d-proton D3D12) lacks
    a HW present — a Vulkan ICD has no runtime handing it the destination
    surface; that missing hand-off IS the entire gap roads (1)/(2)/(4) exist
    to fill. Gotcha: `UmdTrace` is cached at device init — toggling it needs
    a process restart to take effect.**
    **ROAD 4 ON OUR OWN ADAPTER — PROVEN LIVE (2026-07-06,
    `tools/dcomp_present_probe.cpp`, schtask `helios_dcomp_probe`):** a D3D11
    device on the HELIOS adapter (our UMD; owner-approved vehicle use — the
    nesting is bounded and dwm already runs multiple DXVK→ICD stacks per
    process) + `CreateSwapChainForComposition(FLIP_SEQUENTIAL)` + dcomp
    target/visual on the HWND: every call S_OK, 1023 flip presents, dwm
    composes the animation (paintcap ×2, gradient advancing). This upgrades
    road 4 from "WARP + CPU copy" to the REAL zero-CPU-byte design:
    the DXGI backbuffers are OUR KMD allocations = venus blobs, so the ICD
    copies its frame into the current backbuffer GPU-side with its OWN venus
    device (import-by-resid, the dwm/WUDFHost machinery), then Present() on
    the vehicle device mints the token via pfnPresentCb (flip model → the
    UMD's dst-copy no-ops).
    **DESIGN REFINED (owner, 2026-07-06): reverse the import direction and
    special-case in the UMD present DDI via D3D10DDI_HRESOURCE.** The ICD
    never learns backbuffer resids (kills the COM→DDI texture-query problem
    entirely): the ICD publishes ITS OWN frame resid + fence value (WS1 #4
    slot), hands (resid, value, geometry) to a tiny in-process UMD export
    that stores it in TLS, and calls Present() on the same thread.
    `dxgi_present` consumes the TLS slot: the vehicle's DXVK imports the ICD
    frame by resid — the battle-tested alias-import path INCLUDING the
    `6eab004c` copy-time consumer wait, so the published slot orders the
    copy against the ICD's GPU writes for free — copies into
    `hSurfaceToPresent`'s pDrvPrivate DXVK texture, and publishes with its
    OWN fence (correct: the vehicle genuinely wrote the backbuffer). The
    double-publish conflict stops existing — single writer, single
    publisher, gate on the real writing device. Same GPU copy count, zero
    CPU bytes; WSI images become device-local/tiled (no linear cpu_map
    constraint — a rendering win). Remaining engineering: (a) vehicle
    lifecycle off the ICD hot path (the residual-risk contract), (b) the
    UMD export + TLS present-source slot + dxgi_present special case,
    (c) ICD publish of its frame resid (C writer for the seqlock format),
    (d) present-mode mapping, (e) resize/teardown lifecycle. Verify item:
    the ICD's WSI images must be created as shared-importable dedicated
    blobs (wire resources) so the vehicle context can import them. The
    async sw worker stays as fallback wherever the vehicle fails.
    **ROAD 4 IMPLEMENTED END-TO-END (23rd session, 2026-07-06; mesa
    `8a4e331ea9e..737cb2309b3`, dxvk-helios `6069f323`, main `6521d93` +
    `41fa1c4`). Kill switch `HELIOS_WSI_DCOMP_PRESENT`, DEFAULT OFF —
    flip-on is owner-gated (ladder e). Shipped shape:** vehicle lifecycle
    on a dedicated worker (parks after READY and owns the COM release, so
    the nested D3D11→UMD→DXVK→ICD2 teardown never runs on an ICD1 thread);
    frame images stay OPTIMAL buffer-blit images but their dedicated memory
    exports OPAQUE_FD → USE_SHAREABLE blobs; per-chain named present-order
    timeline (`Global\HeliosPresentFence_<pid>_<0x8000_0000|n>` — ICD id
    space is the high-bit half, the UMD producer counter owns the low half)
    signaled on every pre-present submit; seqlock publish from a
    byte-compatible C writer (wsi_helios_present_sync.c); UMD exports
    `helios_umd_set_present_source`/`_wait_last_present` (TLS, same-thread
    contract); dxgi_present special case alias-imports the frame by resid
    (typed identity, per-resid cache — 3 imports per geometry for 30k+
    presents) and copies at the DXVK image level from the LIVE storage
    (staging ALIAS if present; the COM CopySubresourceRegion path would
    read the never-refreshed private image), publishes the backbuffer with
    the vehicle's own fence, no gate, pfnPresentCb as today. Ladder:
    (a) probe PASS 1082 presents; (b) vkcube READY→LIVE, ~64 fps FIFO
    vsync-paced, 30k+ presents ZERO import/copy/geometry/overwrite
    failures, copy-time wait timeouts=0 noslot=0, dwm imports the vehicle
    backbuffers + fence (val tracking live), venus ctx destroyed at exit,
    resize-recreate exercised live (800x600→1896x959); (d) mover-churn
    paintcap clean, WUDFHost timeouts=0, BARRIER=0. **Flip no-copy
    invariant CONFIRMED: every vehicle 'DXGI Present:' has dst=0x0.**
    GOTCHAS: a maximized vehicle chain gets promoted to direct/independent
    flip — correct on the display but ABSENT from GDI-based paintcaps
    (eyeball via Looking Glass; owner-confirmed live); UmdTrace was ON for
    the invariant check — it confounds fps measurements.
    **Doom (immediate, menu): 40 fps v1 → 75 fps** after two measured
    fixes (frame-latency-waitable DROP for non-FIFO — windowed Present()
    otherwise blocks at dwm's compose pace; skip the worker's 4.1 ms
    frame-fence wait while the vehicle serves — ordering is the copy-time
    wait + pre-present throttle). Remaining gap to the 160 sw baseline is
    the image-recycle guard `wait_last_present` (6.1 ms serial,
    present-gate telemetry) stacked on the copy-time wait (8.4 ms
    overlapped): both are venus fence-OBSERVATION latency (ring≥1 retire →
    retire thread → NT signal; ICD2 submission-fence poll), not GPU time
    (~0.3 ms copy). NEXT LEVERS: (1) GPU-side recycle gating — export the
    vehicle's (fenceId, value) back through the UMD, import the named
    fence in ICD1 once, and make image reuse a timeline WAIT at acquire
    instead of a worker-serial CPU wait (fully pipelined; the run-ahead
    absorbs the latency); (2) shave the retire→signal path itself (helps
    every WS1 #4 consumer). sw async worker remains the default and the
    per-frame fallback (the pre-present blit still runs in vehicle mode —
    skip it only with numbers, it is what makes fallback seamless).
    **LEVER 1 IMPLEMENTED (24th session, 2026-07-06; mesa `10f72c50104` +
    `3e5fa9eb1ce`, main `1c34671`; UMD `helios_umd_c78f1a7e01df20b4`, ICD
    `vulkan_virtio-d49cd875438d`).** dxgi_present hands the vehicle's
    (fenceId, value) back via `helios_umd_get_present_result` (TLS,
    same-thread, counted misses/overwrites); the WSI imports
    `Global\HeliosPresentFence_<pid>_<fenceId>` once per chain and gates
    image reuse at ACQUIRE (bound `HELIOS_WSI_VEHICLE_WAIT_US`, 0=off;
    `acquire-gate:` diag telemetry per 512; drops recycle ungated;
    fallback = the old serial wait, `gate_fb` counter; teardown drains max
    pending value). Measured: worker cycle 13→4.4 ms, present-gate serial
    lines GONE, gate avg 2.7 ms overlapped, timeouts 0, gate_fb 0; vkcube
    FIFO ~60 clean; ladder clean (mover, WUDFHost timeouts flat,
    BARRIER=0).
    **CRASH FOUND+FIXED en route (first gated vkcube run froze the guest
    and degraded the whole desktop): vkr resolves vkSignalSemaphore only
    for >=1.2 devices or with KHR_timeline_semaphore enabled at create and
    dispatches it with NO null check** — vn_WaitSemaphores' imported-win32
    post-wait counter sync on a Vulkan 1.0 app (vkcube) = ip=0 segfault of
    the per-context render worker (`journalctl: vkr-ring-<ctx> segfault at
    0`), which the guest NEVER sees: qemu logs only
    `virgl_renderer_context_create_fence: Operation not permitted`
    (proxy_context_submit_fence → dead worker socket → -1), the poisoned
    submission's fence never retires, the app wedges in vkWaitForFences
    and everything else crawls on the backpressure. Fix (mesa
    `3e5fa9eb1ce`): append KHR_timeline_semaphore at device create for WSI
    devices on <1.2 instances (also legitimizes the pre-existing
    present-order timeline signals on 1.0/1.1 apps) + skip the host
    counter sync when the procs cannot exist. dwm/probe never hit it (1.3
    devices). Recovery for a wedged desktop: Stop-Process dwm.
    **OPEN — Doom menu capped at exactly 60.0 fps on BOTH paths today**
    (sw AND vehicle, gate on/off identical, QMP fence-trace on/off
    identical, foreground attempt identical; drops=0, gate timeouts=0,
    worker 4.4 ms, sw wait_avg 3.7 ms — nothing visible adds to 16.6 ms).
    The 22nd/23rd 120/160/75 numbers came from the same unattended
    launcher, so this is a NEW environmental cap, not the acquire gate
    (gate-off A/B proves it). Suspects to disambiguate with the owner:
    idTech background/unfocused throttle state, Steam relaunch settings,
    LGIdd/dwm restart state vs yesterday's boot. Tool:
    `tools/read-vehicle-counters.ps1` samples live minted/drops/gate
    counters from a running process (the perf line never prints on
    pure-vehicle runs); `Z:\tmp\movewin_target.txt` + helios_movewin
    foregrounds an arbitrary window title. Owner gameplay: ~70-80 fps.
    **STALE-FRAME STUTTER FIXED (24th session, cont.; owner-confirmed
    "much better"; main `5a9d5d5`).** Root cause: direct/independent-flip
    presents (Doom's near-fullscreen window) are ordered only on the KMD's
    decode-complete DMA fence — the backbuffer scanned out before the
    venus copy landed, showing the buffer's 3-presents-old occupant. dwm's
    consumer wait protects only COMPOSED presents. Fix: vehicle presents
    now run the frame-completion gate on the WORKER before pfnPresentCb
    (`VehicleFlipGateUs`, default 32 ms, 0=off; 6.7 ms avg = the copy's
    fence-observation latency; acquire-gate wait dropped 2.7→1.7 ms).
    Pipelined follow-up: dxgkrnl WaitForSynchronizationObjectFromGpu on
    the present packet + the producer fence before the copy flush (also
    retires the copy-time CPU wait for vehicle copies). Related cleanup
    (dxvk `7255c1e6`): the redundant pre-6eab004c list-start consumer wait
    in refreshHeliosStagedImages removed for the alias variant.
    **THE 200-fps LEVER FOUND — escape-parked fence waits convoy submits
    (mesa `fbf38f4cc63` has the telemetry + stopgap).** Phase splits
    ([prep/ring/win32] on QueueSubmit2, [mutex/escape/sync] on
    helios_submit) proved: the pre-present submit's 2.5-2.9 ms is entirely
    the win32-signal SUBMIT_VENUS D3DKMTEscape, which serializes at the
    dxgkrnl escape layer behind the retire thread's blocking WAIT_FENCE
    escapes (parked up to 250 ms/slice; processes with no parked waits —
    dwm, the sw path — submit at 3-5 µs). Slice 250 ms → 2 ms bounded the
    convoy (escape 2.9 → ~1.0-1.6 ms) and CONFIRMED the mechanism; Doom
    stays ~60-70 because its OWN vkWaitForFences ride the same parked
    escapes. NEXT SESSION (the fix): **KMD-signaled usermode fence
    events** — non-blocking REGISTER_FENCE_EVENT escape (fence_id, event
    HANDLE), KMD DPC KeSetEvent at wire-fence retirement (in-flight table
    already has ISR→DPC), retire thread + helios_wait park on the EVENT
    in usermode. Kills the convoy class entirely AND collapses the
    fence-observation latency every consumer pays (app fence waits, flip
    gate, acquire gate, retire→signal chain). KMD unit: event table with
    ObReferenceObjectByHandle lifetime + process-teardown cleanup +
    version bump; ICD unit: register/park/cancel in helios_ioctl_wait_fence
    path. Deployed at session end: UMD `helios_umd_89e95497a7d6d08f`, ICD
    `vulkan_virtio-2900b457e859`, dxvk `7255c1e6`, mesa `fbf38f4cc63`.
    **USERMODE FENCE EVENTS SHIPPED (25th session, 2026-07-06; main
    `16da0eb` = KMD 22.22.54, mesa `e5f35c18bf9`; deployed ICD
    `vulkan_virtio-9384eb059a8f`, UMD unchanged `89e95497a7d6d08f`).**
    REGISTER/UNREGISTER_FENCE_EVENT escapes: PASSIVE
    ObReferenceObjectByHandle → bounded (fence_id → PKEVENT) table;
    retirement DPC KeSetEvent + ObDereferenceObjectDeferDelete; one-shot;
    already-retired = atomic check under the device spinlock (no lost
    wakeups); capability probe (fence_id=0/handle=0 → PROBE_ACK, old KMD
    → NOT_IMPLEMENTED → loud diag + blocking-escape fallback). ICD:
    per-thread tss event, register → WaitForSingleObject → cancel;
    UNREGISTER NOT_FOUND + signaled event = raced (complete), +
    unsignaled = teardown purge (loud, INCOMPLETE). Retire thread: 2 ms
    slice stopgap retired from the event path — one WFMO on {fence
    event, retire_stop_event}, 60 s deadline; slice loop survives only
    as the old-KMD fallback. All counters in QUERY_STATS v2 + a
    `fence_events` perf-summary line.
    **BEFORE/AFTER (Doom vehicle, unit-0 baseline 23:00 → post-deploy
    23:35): submit_phases escape 465-786 µs → 111-126 µs; QueueSubmit2
    [prep 11.2/ring 45.5/win32 351 µs] → [5.2/2.3/87 µs] (submit_avg
    390 → 89 µs); acquire-gate (vkcube) 2.6-2.9 ms avg max 11-21 ms →
    196-273 µs avg max <1 ms; fence_events waits=2455 imm=245 raced=0
    timeouts=0 fallbacks=0 lost=0; vkcube FIFO ~60 clean; WUDFHost/dwm
    timeouts 0, BARRIER 0.** Gates passed: escape µs-class ✓, win32
    <100 µs ✓. PENDING: owner Doom gameplay fps + stale-frame stutter
    eyeball on the new stack (baseline gameplay flip-gate max was
    20-29.7 ms = the 2-3-vblank hitch signature; event waits should
    collapse it), present/flip-gate gameplay averages, ladder e soak.
    **DOOM LEVEL-LOAD FATAL ROOT-CAUSED + FIXED (same session; main
    `aedd8ba` = KMD 22.22.55): "Cannot map buffer with usage BU_STATIC"
    = MAP_BLOB → STATUS_INSUFFICIENT_RESOURCES = the adapter-global
    user-VA MappingTable still sized to the ORIGINAL MAX_BLOBS (256)
    after blobs grew to 8192** — desktop held ~223 mappings, the level
    load's vkMapMemory burst blew the remaining ~33 (probe-proven:
    map-and-hold refused at held=33 with 0xC000009A; size sweep 1-256
    MiB all passed, falsifying the MDL-CSHORT hypothesis; failure
    signature present at 05:28 pre-change = NOT a fence-event
    regression). Fix: MAX_MAPPINGS 8192 + the three indistinguishable
    0xC000009A refusal sites individually counted (mapping-full /
    map-pages / window-alloc) in QUERY_STATS v2.
    `tools/blob_map_size_probe.c` = size sweep + concurrent-headroom +
    v2 stats reader (gcc on the VM, no vcvars needed).
    **STALE-FRAME A/B VERDICT + KERNEL-ENFORCED FLIP ORDERING PROVEN
    (25th session cont., 2026-07-07; main `77dffe2`).** Owner A/B on the
    fence-event stack: **sw path 200 fps (was 160 — the event waits
    bought sw +40 fps) with ZERO stale frames; vehicle path 120-130 fps
    with stale-frame stutter still present** → the leak is the
    vehicle/UMD flip path, pre-existing, fps-scaled. Counted leak sites
    that window: flip gate 11 timeouts + acquire gate 3 timeouts per
    ~41k presents (each "proceeds loudly" = a stale flip candidate);
    fence-event machinery itself clean (0 fallbacks/0 lost). NEXT ROAD —
    **kernel-enforced vehicle flip ordering** (replaces the bounded CPU
    worker gate): queue D3DKMTWaitForSynchronizationObjectFromGpu on the
    copy's monitored fence AHEAD of the present packet, so dxgkrnl holds
    the flip until the retire thread's CPU signal lands — no usermode
    cap to leak, and the 6.6 ms worker serial gate retires (fps win).
    `tools/vehicle_flipwait_probe.c` PROVES the primitive live on our
    software-scheduled adapter (queued signal held behind an unsatisfied
    wait, drained ~10 ms after the CPU signal; ZERO KMD changes) and the
    topology: raw cross-device sync handles are REJECTED 0xC000000D —
    the fence must be NT-shared (D3DKMTShareObjects) and reopened via
    OpenSyncObjectFromNtHandle2 on the device owning the waiting context
    (the WS1 #4 named-share consumer pattern). Implementation sketch:
    UMD opens the vehicle fence (fence_id known from the present result)
    on the device owning `dev.h_context`, queues the wait right before
    pfnPresentCb; CPU gate kept as fallback knob + wedge watchdog
    (a never-signaled fence would park the context queue). Second half
    (same primitive): producer-fence wait before the copy flush retires
    the 7 ms copy-time CPU wait. Ladder: wait-honored telemetry → flip
    gate CPU path off → owner stutter eyeball → fps before/after.
    **IMPLEMENTED + WEDGED (main `e9cdae6`, default OFF; VM registry
    VehicleKernelFlipWait=0 guards the deployed default-ON UMD
    `fb564d5072f8a58e`).** First live test (vkcube): present #1 armed +
    queued OK, but the enqueueWait signal NEVER fired (exported present
    fence counter never read ≥1 in the producer's own process — likely
    an untested vn_GetSemaphoreCounterValue/vn_WaitSemaphores
    exported-win32 path), the watchdog unwedge released the flip but the
    app never presented again (the WSI worker parks on something after
    pfnPresentCb — find it), and TerminateProcess on the wedged instance
    HUNG THE ENTIRE GUEST — no bugcheck, no dump (kernel deadlock;
    dxgkrnl/KMD teardown of a context with a parked queue + queued
    monitored-fence wait whose signal source died). QMP system_reset
    recovered. **26TH-SESSION TOP PRIORITY: root-cause + fix all three
    (see the flip-kwait-wedge memory: NMI-crash/live-KD recipe, suspect
    KMD teardown paths, retire-thread-signal design fallback).**
    **26TH SESSION (2026-07-07): DEFECT (a) ROOT-CAUSED + FIXED (mesa
    `b2f47c780d2`, deployed ICD `vulkan_virtio-e31ec528ac79`); the
    GUEST "HANG" CLASS SOLVED — it was the SERIAL KERNEL DEBUGGER.**
    (a) The producer's EXPORTED present fence was unobservable in its
    own process: `is_external` skips the feedback slot, the queue
    signal routes out-of-band onto the helios_sync/WDDM fence (retire
    thread only), and both vn_GetSemaphoreCounterValue (host ring
    round-trip: 0 forever) and vn_WaitSemaphores (win32 fast-path was
    imported-only) never read the WDDM fence. Fix: fold
    vn_renderer_sync_read into the counter read for ANY win32-backed
    payload + widen the wait fast-path (event wait); host-counter sync
    stays imported-only (exported host object has a pending GPU signal
    op). Verified: knob=1 vkcube presents #1-2 armed AND RELEASED
    (was: #1 wedged); first-rescue diag `wddm=852/1307 host=0`.
    (c) The whole-guest freeze reproduced WITHOUT any kill (knob=1
    vkcube after ONE wedge+watchdog-unwedge cycle, ~3 min fuse) —
    QMP `inject-nmi` → bugcheck 0x80 minidump `070726-4890-01.dmp`
    (copy in Z:\tmp): 15/16 vCPUs in nt!KiFreezeTargetExecution, CPU#6
    polling kdcom!READ_PORT_UCHAR — the guest had dropped into the
    SERIAL KERNEL DEBUGGER (bcdedit debug=Serial port 1, NO kdcom
    client exists — ntoseye is a gdbstub) and froze forever. With KD
    enabled, dxgkrnl asserts / the ERESOURCE deadlock detector / TDR
    BREAK instead of bugchecking — that is why neither hang ever left
    a dump. TerminateProcess was never the root cause.
    (b) Caught red-handed in the same dump (stack scavenge of the
    frozen vkcube thread): a runtime thread inside
    DxgkWaitForSynchronizationObjectFromGpu blocked MINUTES in
    nt!ExpWaitForResource on a dxgkrnl ERESOURCE (holder unknown —
    triage dump has one stack) until nt!ExpResourceTimeoutCaptureLiveDump
    → KiBreakpointTrap → KD entry. So the worker's forever-park after a
    flip-kwait stall = a dxgkrnl-internal ERESOURCE convoy seeded by
    the stalled flip, not a WSI wait (all WSI waits audited bounded).
    RESIDUAL OPEN: present #3's wire fence never retired (fence stalled
    at 2 with 3 queued; #1-2 clean) — the remaining true stall; and the
    WSI perf-counter oddity (presents=0/ready=0 while 3 vehicle
    presents + gate-ARM demonstrably ran). TOOLKIT NOW ARMED:
    CrashDumpEnabled=2 (full kernel dump next time → !locks names the
    ERESOURCE holder); PROPOSED: bcdedit /debug off so every future
    freeze self-converts to dump+reboot (testsigning + ntoseye/gdbstub
    unaffected). Registry knob back to 0; ICD fix deployed + desktop
    cold-verified healthy on it.
    **26TH SESSION CONT.: THE DEADLOCK CLASS KILLED — ERESOURCE HOLDER
    NAMED AND FIXED (mesa `2cc63d82468`, deployed ICD
    `vulkan_virtio-178211300d5f`; bcdedit /debug off applied).** The
    owner-approved second NMI (full kernel dump, CrashDumpEnabled=2)
    caught it red-handed: the exclusive holder of all 3 contended
    ERESOURCEs was the process's own next VENUS ESCAPE —
    HardwareAccess=1 escapes run dxgkrnl
    AcquireCoreResourceExclusive → DXGPROCESS::FlushAllDevice →
    VidSchWaitForCompletionEvent, i.e. they WAIT FOR EVERY CONTEXT
    QUEUE TO DRAIN while holding the core resource; with a kwait-parked
    queue whose signal comes from user mode, every
    SignalSynchronizationObjectFromCpu (dxvk waiter, watchdog — user
    dump caught the watchdog parked IN the signal syscall, so the
    25th-session "unwedge released the flip" was FALSE — and the ICD
    retire thread) convoys behind it = deadlock; wedge point #1/#2/#3
    varied by race. Fix = unit 3's HardwareAccess=0 (correctness, not
    just perf; `HELIOS_ESCAPE_HW=1` env kill switch) + the exported-sem
    fold + a signalTo monotonicity guard (main `6d41f8e`, deployed UMD
    `helios_umd_3b4a9e66394e9523`). **LADDER RESULTS (2026-07-07
    02:45-02:53): 24,064 kwait presents, 0 wedges / 0 arm fails /
    0 queue fails / 0 signal fails; mid-run kill (owner) → clean
    teardown, no zombie, guest healthy. BUT THE OWNER EYEBALL RUNG
    FAILED: vkcube showed the Doom-class STALE-FRAME STUTTER from
    ~40 s in — kernel flip ordering alone does NOT close the stale
    class. Plus a new observation: a freshly launched (unfocused)
    vkcube alternates between TWO stale frames until the window is
    clicked, then rendering progresses. DEFAULTS STAY OFF
    (VehicleKernelFlipWait code default OFF, registry knob back to 0).**
    **OWNER CONFIRMED: both issues are ABSENT on the sw present path**
    — they are properties of the dcomp VEHICLE presentation layer, not
    of fences/venus in general (matches the A/B: sw 200 fps zero
    stale). 27th-session hypotheses, vehicle-layer-first:
    (c1) DXGI_STATUS_OCCLUDED — Present() on an occluded/background
    window returns a SUCCESS status (0x087A0001) and does not display;
    wsi_win32_queue_present_vehicle checks only FAILED(hr)
    (wsi_common_win32.cpp:1985-1995) so occluded presents "succeed"
    silently → add per-present hr!=S_OK logging (cheap, likely explains
    the click-gated launch behavior); (c2) dwm/dcomp consumption pacing
    for background visuals, direct-flip vs composed transitions;
    (a) backbuffer clobber — the venus copy lands at Present-call time
    while up to frame-latency flips sit parked, so rotation may
    overwrite a buffer whose flip has not scanned out (instrument
    backbuffer ptr + flip completion pacing; consider bounding armed
    depth); (b) U:=V fires at ring-fence retirement measured at 97-98%
    of T_gpu — check the tail. First step: knob=0 vehicle A/B of the
    unfocused-launch behavior + the hr logging.
    **27TH SESSION (2026-07-07): (c1) FALSIFIED + full static analysis of
    the HW present path.** hr-transition/drop-streak instrumentation
    deployed (mesa `e3d2fdf61c1`, ICD `vulkan_virtio-1545839fc535`;
    `odd_hr` counter in the perf line; paintcap_hidden schtask = capture
    without the console occluder/focus steal): under a 45 s full-screen
    occluder the knob=0 chain presented at a steady 60 Hz, hr == S_OK
    throughout, acquire-gate timeouts 0 — composition swapchains never
    report OCCLUDED and dwm keeps consuming occluded chains; presents
    flowing says NOTHING about display. Owner clarified the property set:
    launch alternation + random self-healing stutter are both cured by ANY
    rendering activity (notepad open — no focus change!) ⇒ the lever is
    activity, not focus. Static analysis (3-leg map, agent reports in the
    session transcript): every vehicle ordering protection is a bounded
    32 ms wait that LEAKS stale on timeout (consumer copy-wait
    dxvk_context.cpp:9507 "copying anyway"; flip gate forward.rs:5902
    "flipping anyway"; acquire release-gate wsi_common_win32.cpp:1683
    proceed; dwm-side staged-refresh wait ditto) — all funnel into named-
    fence advancement by the per-process ICD retire thread, whose event
    wait is the ONE chain link with NO self-heal: KMD fence events are
    interrupt-edge-driven (drain_used from the ISR/DPC or opportunistic
    drains on the process's OWN escape traffic only; no KMD timer;
    register-vs-retire race proven closed, gpu.rs:1317/escape.rs:186), so
    a lost/deferred INTx for a mostly-IDLE process (dwm on a static
    desktop!) parks its retire thread ≤60 s with nothing to kick it.
    PRIME SUSPECT (fits all 4 properties + why vkcube-side counters were
    all clean): the stall is in DWM's process — dwm consumes the vehicle
    backbuffers through the alias-staging refresh + 32 ms consumer wait;
    a parked dwm retire thread ⇒ dwm composes stale staged copies of the
    2 ever-refreshed backbuffers (= the TWO alternating frames) while the
    app's flips rotate healthily; dwm idle ⇒ few escapes ⇒ no drains ⇒
    self-sustaining until any dirty region makes dwm submit (notepad,
    click) ⇒ drain ⇒ unstick. Secondary: retire-thread serial FIFO
    convoy (one stuck head delays every later signal = stutter burst,
    self-heals = P3/P4); vehicle named release fence created LAZILY
    inside present #1 (dxvk_bridge.cpp:1434) + consumer import-failure
    negative-cache of 256 LOOKUPS (dxvk_context.cpp:9482) = long
    unordered window at chain birth. SOLUTION CHOSEN (owner: NO HACKS):
    prove the failing link with ONE owner repro, then fix that link at
    its ROOT — (a) event registered-but-never-signaled + INT_ROUTINE
    stalled ⇒ interrupt delivery ⇒ MSI-X (MSISupported=0 is a bring-up
    leftover); (b) used entry never posted ⇒ submission/doorbell path;
    (c) event signaled but fence behind ⇒ ICD retire logic. The sliced-
    polling retire wait + KMD timer backstop are REJECTED as hacks;
    eager vehicle fence + time-based import retry are clean but staged
    AFTER the repro so they cannot mask it. Evidence kit already
    complete: dwm_helios_umd_dxvk.log present-wait lines (import lines
    prove the channel live; ZERO timeout lines in healthy runs),
    helios_icd_diag.log retire "GIVING UP ... stays UNSIGNALED" lines
    (PROVEN to fire — 2 instances 2026-07-06), QUERY_STATS v2
    FENCE_EVENT_*/INT_ROUTINE via tools/blob_map_size_probe.c,
    paintcap_hidden content diffs. REPRO IS OWNER-ONLY: schtasks
    launches always take foreground (fg=1 across 4 steal attempts), and
    automated probing IS the curing activity (self-defeating). Owner
    recipe: reproduce the alternation, hands off ~90 s (>60 s arms the
    GIVING-UP diag), note the time; then read the four channels.
    **27TH SESSION VERDICT + FIX DEPLOYED (KMD 22.22.56, main `1a07814`).**
    Owner repro delivered the discriminator: during a LIVE two-frame
    alternation (~60 s, hands-off recovery) EVERY sync in the DAG was
    green — vkcube 60 fps/acquire-gate 64 µs/0 timeouts, vehicle 29k
    copies 0 fails/flip-gate 390 µs, dwm waits 7.9 ms 0 timeouts 0
    noslot on the live chain, WUDFHost 6.3 ms 0/0 — so the fence class
    was EXONERATED (retire/interrupt fixes rejected as the wrong tree).
    Second audit pair: our flip-emulation identity rotation is provably
    self-consistent (copy dst == flip src per present; storage+identity
    rotations = same permutation; publish resid tracks rotation; no
    in-tree pairing skew), BUT the caps surface lied:
    `DXGK_DRIVERCAPS.SupportDirectFlip=1` (NO bisect provenance — never
    load-mandatory) + aperture-segment DirectFlip flags, on an adapter
    with ZERO scanout displaying through IddCx-captured COMPOSITION.
    dwm promotes the eligible dcomp vehicle visual (flip-model +
    IGNORE-alpha + unoccluded) to direct/independent flip and STOPS
    COMPOSING it — two alternating stale frames = dwm's last composed
    pair; any dirty-region recompose demotes it (taskbar clock minute
    repaint = the hands-off ~60 s recovery; console-overlapped schtasks
    launches never repro'd because occlusion kills eligibility; the
    23rd-session "maximized chains vanish from paintcaps" was the same
    promotion). The UMD already denied CheckDirectFlipSupport — the KMD
    now agrees: SupportDirectFlip + the 3 aperture DirectFlip flags are
    DENIED by default behind the `DirectFlipCaps` service knob (0 =
    deny, 1 = legacy A/B via reg add + devcon restart; state in diag
    0x01D7 bit 2, DiagLevel-gated). Display-less-adapter guidance
    (mcdm-implementation-guidelines.md) mandates 0. DEPLOYED + cold-boot
    verified: 22.22.56 CM_PROB_NONE, release UMD re-pinned
    (helios_umd_3b4a9e66394e9523; devcon reset it to debug — known
    trap; DriverStore copy sync failed file-in-use, cosmetic), desktop
    composited healthy. OWNER VERDICT: NO CHANGE — direct-flip denial
    falsified as the mechanism (kept as the truthful cap surface).
    **TRUE ROOT CAUSE FOUND + FIX DEPLOYED (dxvk `1cdf0837`, mesa
    `10cefe67c64`; UMD `helios_umd_746b0242cf664825`, ICD
    `vulkan_virtio-390d1b583dba`).** Discriminating evidence: owner
    repro with per-window counter sampling — during a LIVE dance dwm
    performed ZERO consumer reads for 20+ s while composing 60 fps
    (waits/fast/noslot all frozen), and mouse movement cured it
    INSTANTLY (both dance and stutter) — consumer freshness tracked
    dxvk COMMAND-LIST CADENCE, not producer progress: staged imports
    re-stage only at list starts, an idle dwm's CS chunks span many
    frames (~60 s to fill when idle = the hands-off recovery), so its
    sampled staged copies froze while every fence stayed green; the
    front-buffer rotation cycled 2-3 differently-aged frozen copies =
    the two-frame dance; chunk-threshold oscillation = the stutter;
    activity = per-frame flushes = cure. sw path immune because
    GdiAccelMode=0 routes GDI content via CPU redirection surfaces
    (not our staged imports). THE FIX (dxvk-helios, 3 parts):
    (1) bind-time staleness gate — staged-SRV binds arm a sticky
    per-context flag; draws/dispatches on the immediate context compare
    each bound staged image's published present-sync value against the
    value its last re-stage observed and Flush() when the producer
    advanced (≤1 flush per published value per image via a claim CAS;
    non-consumer processes pay one branch per draw); (2) zombie-refresh
    unenroll — texture dtor flags the image, the refresh loop erases it
    (was: 15k+ no-slot full-image copies per DEAD vkcube chain until
    the 3600-tick prune, Rc-pinning the corpse's venus resources);
    (3) present-sync slot recycling by PRODUCER CREATION TIME
    (reserved2 repurposed; pid liveness alone let cross-boot pid reuse
    keep 64/64 slots stale — publishes were being DROPPED table-full).
    Along the way: 366-368-vs-389-391 resid "mismatch" resolved as two
    chain generations (live-chain publish/lookup rendezvous verified
    healthy, 3 rotating slots advancing per frame). AWAITING owner
    verdict on the fix. Then: ladder rungs (Doom fps vs 120-130,
    kwait default decision, HELIOS_WSI_DCOMP_PRESENT default), and the
    deferred cleanups: per-present sw-fallback insurance blit on
    vehicle chains ("measure before removing"), in-process present-sync
    slot/named-fence round-trips (set_source carries a dead
    fence_value), eager vehicle fence at init, time-based import
    negative cache, DirectFlipCaps knob retirement decision.
    **FIX CONFIRMED (owner: "working very well"); 28TH SESSION =
    WINDOWED DOOM PERF.** Doom telemetry (pid 9444, 1880x943 windowed
    vehicle, immediate/tearing=1, ~105 fps): flip gate avg 5.57 ms
    (worker-serial) + acquire gate avg 4.06 ms (app), both timeouts=0 —
    pure fence-observation latency of the vehicle copy;
    queue_present_avg 5.96 ms. FULLSCREEN "200 fps rock-stable" = THE
    SW PATH: the fullscreen (1896x1030) chain's vehicle build FAILED at
    stage='dcomp target/visual' hr=0x88980800 and latched → sw direct
    path at ~0.85 ms/frame CPU (creates=2 fails=1 in
    helios-doom-wsi-perf.txt). NEW DEFECT: vehicle re-create for the
    same hwnd fails (one-dcomp-target-per-hwnd on resize/fullscreen;
    vkd3d likely creates a NEW VkSurface for the same hwnd → per-surface
    target cache misses → second CreateTargetForHwnd fails). dwm
    post-fix: noslot=54 (was 68k), fence_events 0 fallbacks/0 lost,
    escape 64 µs. Doom perf files: C:\Users\Rupansh\helios-doom-perf.txt
    + helios-doom-wsi-perf.txt (owner's launcher tees them).
    **28TH SESSION: the dcomp target re-create defect FIXED (mesa
    `bbf5e33f314`, ICD `vulkan_virtio-437986e5fcc4` deployed).** One
    composition target per hwnd is a Windows rule; the target/visual
    cache was per-VkSurface, so a new VkSurface for the same hwnd
    (vkd3d resize/fullscreen) failed CreateTargetForHwnd 0x88980800 and
    latched onto the sw path. Fix: process-global refcounted
    hwnd→target registry under the vehicle runtime mutex; the visual's
    content owner (current_swapchain) lives on the shared entry so a
    retired chain on the OLD surface cannot blank the new chain's
    content; `tgt_reuse` counter added to the perf line. Proven by
    tools/vk_surface_recreate_probe.cpp (schtask `helios_vk_recreate`,
    session 1, time-based phases — frame-count phases finish before the
    ~5 s async vehicle build, first attempt exercised nothing): chain A
    LIVE holding the target → chain B (new surface, SAME hwnd, A alive)
    READY+LIVE → chain+surface A destroyed under live B, B presented on
    (acquire-gate 0 timeouts). The honest fullscreen-vehicle Doom A/B
    is now unblocked (owner run — expect creates=2 fails=0 and the
    fullscreen chain LIVE instead of the 0x88980800 latch).
    **28TH SESSION cont. — kwait rung 1 GREEN + copy-side wait FALSIFIED
    + both cleanup counters live (mesa `06e27a05ea3`, dxvk `7c82271f`;
    deployed ICD `vulkan_virtio-43feb2709167`, UMD
    `helios_umd_801a8571aff69c67`, `VehicleKernelFlipWait=1` in the
    registry).** (a) Kernel flip-wait smoke: vkcube dcomp 4608/4608
    presents kwait_armed, 0 arm/queue fails, acquire-gate 66 µs avg
    0 timeouts, 40 s no wedge, content advancing in paintcap diffs —
    the 5.6 ms worker-serial flip gate is retired whenever the knob is
    on; Doom A/B vs the 105 fps baseline = OWNER RUNG (knob already
    live). (b) The 25th-session "producer-fence wait before copy flush"
    idea is FALSIFIED as a lever: Doom's 28k-present run logged ZERO
    copy-time consumer waits (no present-wait/unordered/noslot lines in
    umd-9444.log) — the copy-time CPU wait always fast-paths; the
    4.06 ms acquire gate is copy COMPLETION+OBSERVATION latency, not a
    CS-thread wait. Not built (measure-first). (c) Insurance blit:
    HELIOS_WSI_INSURANCE_BLIT=0 skips the per-present image->buffer
    fallback blit on vehicle-serving chains (insurance_skipped counter
    in the common perf line; vkcube A/B clean, 2780 skips, content
    correct) — default ON until the Doom A/B numbers land. (d)
    dwm-side staleness-gate cost now visible: gate_flushes in the
    present-wait line — ~50 flushes / 13.7k consumer reads (~0.4%) on
    a live desktop, the 27th-session fix is cheap. OWNER LADDER: Doom
    windowed (kwait live) fps + gates vs 105; fullscreen re-try
    (target-registry fix) — expect vehicle LIVE, honest fullscreen
    number vs the 200 fps sw path; optional HELIOS_WSI_INSURANCE_BLIT=0
    leg; stale class must stay dead throughout (vkcube shows no
    regression). THEN: kwait + DCOMP default decisions, residual
    stutter triage (105-vs-60 Hz judder, gate max-spikes).
    **OWNER DOOM VERDICT (same-process windowed→fullscreen, kwait=1 +
    insurance=0): no fps change; stutter still present WINDOWED, GONE
    FULLSCREEN.** Evidence (pid 6220): the fullscreen 1896x1030 chain
    went VEHICLE (READY+LIVE on the same hwnd as the windowed chain —
    the target-registry fix confirmed in the wild); kwait_armed
    6144/6144, 0 arm/queue fails, no wedge; insurance_skipped
    13176/13200; queue_present_avg 5.96→2.81 ms (flip gate retired) BUT
    acquire-gate 4.06→7.69 ms avg (max ~20 ms, timeouts 0) — the
    latency the worker gate used to absorb moved to acquire: the fps
    limiter is the vehicle copy's COMPLETION+OBSERVATION latency
    (~1 host-GPU frame at saturation), not CPU gates. Gates are now
    honest pipeline measurements, nothing left to delete guest-CPU-side.
    STUTTER LOCALIZED: same process/gates/fps, stutter only when dwm
    COMPOSES the chain (windowed); fullscreen (no dwm compose leg)
    smooth ⇒ the stutter lives in dwm's consumption leg. dwm telemetry
    during the run: gate_flushes 2480 (~60/s — the freshness gate does
    per-frame work under a game, expected) and consumer waits avg
    8.9 ms when they fire (~8% of reads) — 9 ms stalls ON DWM'S CS
    THREAD = composition hitches. HYPOTHESIS (next session): the
    staged-refresh loop re-stages ALL enrolled vehicle backbuffers at
    list start, including the just-presented one whose copy is still
    in flight — dwm waits ~9 ms for a buffer it is not composing this
    frame. Fix shape (no hacks — matches real-hw dwm semantics of
    composing the newest COMPLETE frame): skip-if-unretired in the
    refresh (keep the current staged bytes, let the bind-time gate
    force the re-stage next list) instead of the bounded 32 ms wait;
    kwait guarantees the flip itself never outruns its copy, so
    skipping cannot resurrect the stale class. DEFAULTS PROPOSAL:
    kwait code-default ON (Doom+vkcube green, deadlock class dead);
    insurance knob keep (no measurable cost either way at Doom res —
    the copy hides under GPU latency); DCOMP default AFTER the dwm
    stutter fix.
    **STUTTER FIX IMPLEMENTED + DEPLOYED (dxvk `6bcbd282`, main
    `83f9697`; UMD `helios_umd_b70f3e5b23cc5e03`): skip-if-unretired
    staged refresh for kwait-ordered publishes.** Producers advertise
    kernel-held flips in the present-sync slot (fenceId bit 30 — free
    in both id spaces; UMD sets it on vehicle publishes when
    flip_wait_setup succeeded, never on dwm→IddCx publishes; mesa
    publishes never set it); the consumer refresh keeps its current
    staged bytes for an unretired kwait value (that image cannot be
    the sampled front buffer — dxgkrnl holds its flip), re-arms the
    bind-time gate, counts (refresh_skips in the present-wait line;
    dxvk.heliosSkipUnretiredRefresh default ON = kill switch).
    heliosProducerFence factors the import cache; VehicleKernelFlipWait
    CODE DEFAULT NOW ON (registry 0 = kill switch). VERIFIED LIVE:
    dwm skips ~60/s on vkcube's 3 backbuffers with ZERO blocking waits
    since restart and content advancing (paintcap pair); WUDFHost
    refresh_skips=0, waits unchanged 5.8 ms/0 timeouts (the IddCx
    orderer untouched); vkcube kwait 4096/4096 armed via the code
    default. AWAITING owner windowed-Doom eyeball: the 8.9 ms dwm CS
    stalls are gone — stutter verdict decides the DCOMP default next.
    **OWNER VERDICT: STUTTER FIXED (fps ~same, as expected — the
    limiter is copy completion+observation, a separate lever). DCOMP
    DEFAULT FLIPPED ON (mesa `f5037d701ad`, ICD
    `vulkan_virtio-d6e68f7a3322` deployed): the vehicle now serves
    every Vulkan swapchain by default; HELIOS_WSI_DCOMP_PRESENT=0 =
    per-process kill switch. Verified: env-free vkcube goes vehicle
    LIVE, kwait 1536/1536 armed, acquire-gate 84 µs / 0 timeouts,
    content advancing (paintcap pair). The vehicle class (WS2 road 4)
    is DONE: kwait ON by default, skip-if-unretired ON by default,
    hwnd→target registry, insurance knob available. REMAINING WS2
    PERF LEVER: the vehicle copy's completion+observation latency
    (acquire gate ≈ 7.7 ms under Doom ≈ 1 host-GPU frame) — measure
    host-side scheduling before touching. Deferred cleanups: in-process
    slot round-trips (dead set_source fence_value), eager vehicle
    fence at init, WSI perf-counter oddity, cold-boot re-verify,
    GdiAccelMode retirement, DirectFlipCaps knob retirement.**
    **29TH SESSION — COPY-LATENCY ROOT CAUSE FOUND: QEMU delivers venus
    wire-fence completions by POLLING (10 ms fence_poll timer + an
    opportunistic poll on every guest ctrl-queue kick); the async
    fence-callback path is NOT active in the running config.** The whole
    stack below it is fast. Measurement chain (all landed, deployed):
    (a) `copy-lat` in the umd bridge (publish→waiter-observation of the
    vehicle copy fence; umd log, every 512): vkcube on an IDLE GPU avg
    ~13.6 ms, dominant bucket 10-20 ms — the Doom 7.69 ms acquire gate is
    just the tail of this past the app's natural slack. (b) `retire_lat`
    in the ICD (submit→wire-fence-retirement-observed, HELIOS_PERF
    summary + histogram): BIMODAL — ~half <1 ms, ~half 10-20 ms, max
    ≈11 ms (vehicle) / ≈16 ms (producer), RATE-INDEPENDENT (260 fps
    immediate-mode vkcube: slow mode persists and stacks, max 28 ms).
    (c) Host driver EXONERATED: tools/vk_fence_wake_probe.c (Linux,
    empty vkQueueSubmit+WaitForFences on the idle NVIDIA queue) =
    0.23 ms avg / 0.48 ms worst. (d) HOST-SIDE PROOF from the 07-06
    traced run in /tmp/helios-qemu-stderr.log (virtio_gpu_fence_ctrl/
    resp): a lone in-flight fence gets its response +10.5 ms; two
    fences submitted together both complete fast (the second submit's
    handle_ctrl kick polls the first through) — the exact
    poll+kick signature of hw/display/virtio-gpu-gl.c
    virtio_gpu_gl_handle_ctrl → virtio_gpu_virgl_fence_poll and the
    10 ms fence_poll timer. QEMU 11.0.1 enables async fences only when
    `qemu_egl_display` is set + virglrenderer ≥1.1.2 (both look
    satisfied: egl-headless display, virgl 1.3.0, callbacks v4) — WHY
    it is not active is the open host-side question (verify via gdb
    `print qemu_egl_display` on the live process or a traced relaunch:
    `--trace 'virtio_gpu_fence_*'`). FIX DIRECTION (owner decision, VM
    relaunch territory): get VIRGL_RENDERER_ASYNC_FENCE_CB active —
    expected win: every venus fence wait in the system (vehicle copy
    observation, dwm consumer waits avg 8.9 ms, IddCx waits 5.8 ms,
    D3D11 fence waits) drops from ~5-15 ms to sub-ms; the ~105 fps
    windowed Doom ceiling should lift substantially. Vkr map (host,
    for later levers): per-context worker PROCESSES, no cross-context
    CPU locks; per-guest-queue host VkQueue, family passed verbatim;
    ring≥1 retirement = empty marker submit + per-queue sync thread;
    VK_EXT/KHR_global_priority plumbed end-to-end; transfer-family
    queues map 1:1 (both usable if GPU-side contention ever becomes
    the limiter — it is NOT today). Waiter named: the vehicle device's
    `wait_calls fast=0 timeout≈2571` = DxvkFence::run() (dxvk_fence.cpp
    10 ms slice loop) servicing present_flip_wait_arm enqueueWaits —
    slices are benign event-backed re-loops. New tooling: schtask
    `helios_vkcube_imm` (immediate-mode cube, perf files
    vkcube_imm_*); helios_vkcube now also sets HELIOS_PERF →
    vkcube_renderer_perf.txt.
    **29TH SESSION cont. (owner-collaborative) — REFINED + WORKAROUND
    SHIPPED: feedback-shadow retire (mesa `b578e7d42a3`, ICD
    `vulkan_virtio-32e87a6fc919` deployed, device restarted, desktop
    verified).** Refinements from live host debugging (owner gdb/strace/
    sysctls): the async fence path IS configured (proxy flags 962);
    worker retires fences in 50-150 µs (strace); the io_uring fdmon
    theory was FALSIFIED (kernel.io_uring_disabled=2 + reboot: epoll
    shows kick→IRQ p50 33 µs yet guest retire_lat unchanged); vkr ring
    thread + guest KMD interrupt handling audited clean (spec + Linux
    virtgpu cross-checked by the owner; real non-10 ms inefficiencies
    noted: INTx shares IRQ 22 with the balloon → KMD MSI-X someday;
    serial retire-thread waits). Guest no-WSI probe
    (tools/vk_fence_wake_probe_win.c): the vn FEEDBACK channel is
    0.61 ms while the WIRE-fence channel carries the 10-20 ms class.
    DOOM (instrumented, owner run): copy-lat 11.4 ms avg (65% in
    10-20 ms, 0% <1 ms), acquire-gate 6.9 ms of the 9.5 ms frame, game
    device 97% of fences 6-20 ms — CONFIRMED as THE fps ceiling.
    OWNER DECISION: QEMU fix out of scope → WORKAROUND: the ICD retire
    thread now observes exported-fence completion via the semaphore's
    GPU-written vn feedback slot (slots now allocated for exported
    timelines — self-signaled only on this stack; counter VA attached to
    the sync, detached before pool recycling; poll ladder yield/Sleep to
    a 50 ms budget; wire path = fallback + kill switch).
    `HELIOS_RETIRE_FEEDBACK` DEFAULT ON, =0 restores wire behavior
    (A/B-proven). vkcube: retire_lat 5.6-9.2 ms → 0.25-0.33 ms (100%
    <1 ms, fb fast=100%); copy-lat 13.6 ms → 0.8 ms; content advancing;
    kill-switch run bimodal again. retire_fb fast/fallback/wire counters
    in the perf summary. PENDING: owner Doom re-run (expect the acquire
    gate to collapse and fps to rise toward the ~200 sw-path bound);
    HELIOS_RING_NOTIFY_EAGER diag knob (mesa `4d0c7d21514`, default off,
    falsified as a lever); host sysctls to revert when debugging ends:
    kernel.yama.ptrace_scope=0 (→1), kernel.io_uring_disabled=2 (owner's
    call — QEMU fence behavior identical either way).
- **Capture path**: IddCx frame drop policy vs D3D12 copy queue saturation;
  KVMFR bandwidth; 10 bpc default.
- Candidates list from the NVIDIA fix era lives in ICD.md.

## Workstream 3 — D3D11 Conformance

**30th session (2026-07-07) — 3DMark bring-up: LUID gap FIXED, FL11 ceiling
root-caused.**

- **Vulkan/DXGI LUID identity gap — FIXED, DEPLOYED, VERIFIED (mesa
  `23b10bb6d80`, main `e0b462f`; ICD `vulkan_virtio-c2919595f95d`).** The venus
  ICD reported `VkPhysicalDeviceIDProperties::deviceLUIDValid=false` ("Phase 6
  concern" stub in `helios_init_renderer_info`), so no VkPhysicalDevice carried
  the guest WDDM adapter LUID that DXGI reports. UL/3DMark Steel Nomad (Vulkan)
  selects an adapter via DXGI then matches into Vulkan by `deviceLUID` → found
  nothing → "VkPhysicalDevice with device LUID X not found". Fix: plumb
  `helios->adapter_luid` (captured from D3DKMTEnumAdapters2 at open, == the DXGI
  AdapterLuid) into `info->id` (has_luid=true, node_mask=1, luid verbatim).
  Verified same-boot: vulkaninfo `deviceLUID dfb16300-00000000` == WDDM adapter
  `luid 00000000:0063b1df`, `deviceLUIDValid=true`. (LUIDs change per
  device-restart — the fix reads adapter_luid dynamically, tracks it. dxvk's
  own findAdapterByLuid / D3DKMTOpenAdapterFromLuid also benefit.) **Owner to
  re-test Steel Nomad.**
- **Fire Strike (D3D11 FL11_0) "no GPU" — the Helios adapter is FL10_0; being
  raised gate-by-gate. It is NOT a KMD/adapter ceiling (that theory falsified)
  — it's a sequence of UMD caps bugs.** The engine is genuinely FL11 (bridge
  creates the dxvk device at `D3D_FEATURE_LEVEL_11_0`; all FL11 DDIs wired).
  Everything is behind `HKLM\SOFTWARE\Helios!FeatureLevel11`, now an integer
  MODE (0=FL10 default/proven, 1=full FL11, 2=diagnostic pipeline-only); knob=0
  = exact FL10 baseline, dwm-safe. **THE tool that cracked it: the
  `Microsoft-Windows-DXGI` ETW provider prints d3d11.dll's exact rejection
  string** (the debug layer / DXGI InfoQueue / DBWIN / DxgKrnl-AzureTriage all
  gave 0 messages — the failure is device-less). Recipe: `logman start
  helios_dxgi -p Microsoft-Windows-DXGI 0xFFFFFFFFFFFFFFFF 0xff -o x.etl -ets` +
  `logman update helios_dxgi -p Microsoft-Windows-Direct3D11 ... -ets`, run the
  probe, `logman stop`, `tracerpt x.etl -o x.xml -of XML -y`, read `<Data
  Name="Message">`/`Code`. Gates cleared: (1) **"Driver returned invalid
  pipeline caps"** — 3DPIPELINESUPPORT is a BITMASK
  `(1<<Level)` OR'd, not the bare enum; we wrote 11_0=2 = bit1-only = invalid;
  fixed FL11=0x7, FL10=0x1 (the old 10_1=1 worked by luck = the 10_0 bit).
  (2) **"Driver doesn't support compute on FL11"** — SHADER caps now advertise
  0x2 (compute). (3) **"MSAA quality reported to be 0"** — FL11 requires every
  render-target format to support 4x MSAA and does NOT exempt 96-bit R32G32B32;
  CheckMultisampleQualityLevels now floors RT formats to >=1 at 1/2/4/8 +
  check_format_support advertises MULTISAMPLE_RENDERTARGET for RTs — PARTIAL:
  the runtime advances past formats 5-8 but still hits the MSAA error on a later
  format/count. **NEXT (owner directive): conform to the D3D11.3 functional
  spec** (https://microsoft.github.io/DirectX-Specs/d3d/archive/D3D11_3_FunctionalSpec.htm)
  — exact per-format/per-sample-count FL11 MSAA requirements. Committed main
  `ff14979` (WIP, un-deployed source; the all-count MSAA-log build was not
  installed). Probe: `tools/d3d11_fl_probe.cpp`, schtasks `helios_flprobe`,
  session 1 (session-0 win_exec fails all levels for a context reason, not FL).

**31st session (2026-07-07) — Fire Strike launch blocker moved past FL11:
legacy DXGI output mode-list fails on the IddCx logical output.**

- Owner rebooted after an IDD/client wedge; desktop composition is healthy again
  (`HeliosRenderAdapter=1`, both Display devices `OK`, IDD frames flowing with
  dirty-rect D3D11 fallback copies). The latest Fire Strike result
  (`3DMark-FireStrike-FAILED-20260707155504.3dmark-result`) exits both Demo and
  GT1 during `SINGLE_INIT_BEGIN` before workload rendering:
  `IDXGIOutput::GetDisplayModeList` returns `DXGI_ERROR_NOT_CURRENTLY_AVAILABLE`
  (`0x887a0022`), then 3DMark reports "Workload produced no results".
- Repro probe on the same boot: DXGI enumerates `\\.\DISPLAY6` under a fixed
  logical Helios adapter LUID `00000000:000078c5`; every
  `IDXGIOutput::GetDisplayModeList` / `GetDisplayModeList1` call returns
  `0x887a0022` for the tested formats. `D3DKMTOpenAdapterFromGdiDisplayName` on
  that same `\\.\DISPLAY6` opens the real Helios render adapter LUID
  `00000000:0038c127`, and `D3DKMTGetDisplayModeList` succeeds with 1064 modes.
  DXGI ETW around the probe records `IDXGIOutput_GetDisplayModeList` stop events
  with `m_Ret=2289696802` (`0x887a0022`) and the output object bound to
  `\\.\DISPLAY6`, but no richer rejection string.
- Falsified: RDP/session-context cause. The workload and probes run in active
  console session 1, not an RDS session. Also falsified: missing IDD modes in
  general (CCD/KMT mode lists exist), and UMD WDDM1.3 Present1/MPO callback
  table as the immediate launch blocker (current UMD logs show the DXGI 1.3 base
  table populated and D3D11 device creation succeeds in probes).
- Current read: this is the architectural split between the IddCx/IndirectKmd
  display adapter that owns the output and the real Helios render adapter that
  owns D3D/Venus. Fire Strike's legacy fullscreen init assumes
  `IDXGIOutput::GetDisplayModeList` works on the visible output. A proper fix is
  not a UMD present-path hack; it likely requires a real display/VidPN owner for
  the visible output (or equivalent OS-supported path that makes DXGI's output
  mode-list resolve on the render/display adapter pair). If reviving KMD display
  ownership, use the archived viogpu3d/VidPN research and treat it as a full
  display-miniport implementation, not as stubbed VidPN callbacks.

**32nd session (2026-07-07) — Fire Strike/FaceWorks missing surfaces: first
real D3D11 correctness bug fixed, owner visual validation pending.**

- Owner used Windows Settings -> Display -> Detect multiple displays; there was
  no visible display change, but Fire Strike moved past the legacy
  `IDXGIOutput::GetDisplayModeList` launch blocker and now reaches fullscreen
  rendering. Current symptom: many missing surfaces. Do not spend time on
  `DxgkDdiPresent`: this is a render-only adapter path, and the desktop already
  proves the presentation/capture leg is alive.
- FaceWorks is the faster repro. Its initial dxbc-spv assertion in
  `shd_instruction.cpp` came from D3D11 DDI tessellation patch-constant sysval
  names; `umd/bridge/dxvk_bridge.cpp` now remaps the DDI tess-factor sysvals to
  DXBC `SV_TessFactor` / `SV_InsideTessFactor` signatures. The sample then
  opened a black window in owner-visible testing. A scheduled-task run is not a
  valid visual proxy yet: it exits during DXUT validation before any present.
- Concrete bug found in `umd/src/forward.rs`: DDI Texture2D views were always
  translated to non-MS D3D11 view dimensions. For multisampled Texture2D
  resources this is wrong: RTV/DSV/SRV must use
  `TEXTURE2DMS`/`TEXTURE2DMSARRAY`, not `TEXTURE2D`/`TEXTURE2DARRAY`. This is a
  real correctness hole for Fire Strike-class MSAA workloads and can make view
  creation fail or bind the wrong resource interpretation.
- Fix deployed in UMD `helios_umd_ac10566f81de7294.dll` (SHA256
  `AC10566F81DE72944B472131DF1AE8CBAD719938DCC804C9A936FC14F6643B19`):
  `rtv_desc`, `dsv_desc`, and `srv_desc` now query the underlying
  `ID3D11Texture2D::GetDesc().SampleDesc.Count` and select the MSAA view
  dimensions/unions when `Count > 1`. Adapter hotplug only; no guest reboot.
  Active registry and live DWM/explorer modules point at the new ProgramData
  UMD, Helios device is `CM_PROB_NONE`.
- Evidence: new `tools/d3d11_msaa_view_probe.cpp` passes on Helios FL11_0. It
  creates 4x `R8G8B8A8_UNORM` RT/SRV and `D24_UNORM_S8_UINT` depth resources,
  creates explicit 2DMS RTV/SRV/DSV views, clears, resolves, stages, maps, and
  reads pixel `64,127,191,255`. UMD log for pid 10120 shows
  `MSAA q fmt=28 c=4 -> 1`, successful `create_rtv`, `create_srv`, and
  `create_dsv` on `dim=3` MSAA resources. **NEXT:** owner reruns FaceWorks and
  Fire Strike against this UMD; if surfaces are still missing, inspect fresh UMD
  logs for failed Create*View, ResolveSubresource, Copy/Discard/ClearView, and
  noop-DDI counter movement.

**33rd session (2026-07-07) — FaceWorks/Fire Strike "missing surfaces" reframed:
render is FINE, the problem is windowed compositing. Two earlier theories
FALSIFIED.**

- **FaceWorks is not a render/coherence bug.** `HELIOS_PRESENT_READBACK` shows the
  present source non-black at 1264×681 (multi-pass scene: 1060 draws to the
  backbuffer, 937 to a 184×161 SSS RT, DrawIndexed to 1024×1024).
  `tools/d3d11_shared_draw_probe.cpp` proves float + SINT-indexed+CB + textured/
  blend draws propagate cross-device via OpenSharedResource1. Owner: **Fire Strike
  renders fine fullscreen** — the render pipeline is healthy.
- **FALSIFIED — "two-memory split / KMD zeroes the adopted resid."** The UMD's
  `allocate_wddm_resource` "private mutated" log (`res_id 22301→0, blob→0x3b4000`)
  is the UMD reading the KMD's NEW `HeliosWddmOpenIdentity` writeback with the OLD
  `HeliosWddmAllocPrivate` layout — offset 0 is `venus_alloc_size` (0x3b4000 =
  3883008), and the fields it prints as res_id/kind are `reserved` words. The KMD
  adopt path (`create_allocation.rs adopt_blob_for_allocation`) preserves resid
  22301 and mints no redundant blob; the allocation IS backed by the DXVK venus
  image. Do not re-chase this.
- **FALSIFIED — alpha.** Owner confirmed `HELIOS_PRESENT_FORCE_OPAQUE` changes
  nothing; DWM composites the flip swapchain opaque.
- **FALSIFIED — the IDD.** Both the live D3D11 fallback (`SwapChainNewFrameD3D11`)
  and the D3D12 path capture the same single composed IddCx surface; the D3D12
  path is dead only because our UMD implements no D3D12. The IDD faithfully
  forwards whatever DWM composited (`CopyFromScreen`/paintcap show the same black
  client area).
- **The live root cause is the render-adapter / DXGI-output topology (→ priority
  #1 above).** DXGI enumerates TWO identically-named "Helios vGPU Render Adapter"
  entries (LUIDs …fba8, …7896) that both resolve to the same physical WDDM adapter
  (a stale-LUID residue from repeated device restarts) plus 2× WARP; every
  adapter's `EnumOutputs` returns `0x887a0022` intermittently (one run showed
  `outputs=1` on …7896). CCD `QueryDisplayConfig(ONLY_ACTIVE_PATHS)` pins the live
  Looking Glass output to …fba8 (`\\.\DISPLAY2`), while `QDC_ALL_PATHS` fails
  `ERROR_GEN_FAILURE` on a broken second path (ghost QEMU/DEFAULT monitors). Apps
  that need an `IDXGIOutput` get `NOT_CURRENTLY_AVAILABLE`, fabricate a mode, and
  drop the window off-screen; and DWM never imports the app's flip backbuffer.
  Probes: `tools/{dxgi_output_modes,ccd_adapter}_probe.cpp` (session 1). Memory:
  `phantom-adapter-luid-enumoutputs-33rd`, `faceworks-black-d3dflip-twomemory-split-33rd`.
- KMD 22.22.61: `create_allocation` now writes the `HeliosWddmOpenIdentity`
  trailer for adopted allocations too (live resid for cross-process openers).

Current state (surveyed 2026-07-05):

- UMD (`umd/`, Rust d3d10umddi frontend → cxx bridge → DXVK C++ engine):
  advertises `D3D11_1_DDI_SUPPORTED` (11.15.0) + `D3D11_0` (11.10.2), fills
  `D3D11_1DDI_DEVICEFUNCS`; `forward.rs` implements ~220 DDI functions;
  unfilled slots route to counted noop handlers (`ddi_noop_device/dxgi` —
  loud, not silent). Feature level 11_0 reported to the runtime.
- dxvk-helios: ~58 files / +3.8k lines diverged from upstream DXVK (venus
  import model, GDI staging, alias-image detile, persistent refresh,
  undersized-import refusal).
- Known DDI gaps (from bring-up sessions): ClearView logging blind (log
  budget), partial Discard handling fixed for the common case, Rotate
  implemented minimally; keyed mutex path exercised only by probes.

Plan:
1. Enumerate the noop-slot hit counters after a real workload day — every
   nonzero noop is a conformance gap with a caller.
2. Run dxvk-tests / d3d11-triangle / d3d9-on-11 samples inventory
   (`dx-samples-research-only/`), then 3DMark (installed on the VM).
3. DXGI format coverage audit (the format round-trip carrier landed at
   `bfb5121`; verify beyond BGRA8).
4. Map remaining 11.1 features (deferred contexts? threading modes? UAVs at
   FL11_0) against DXVK capabilities — most exist in the engine; the work is
   the DDI plumbing.

## Tooling (keep alive; this stage depends on it)

- **Registry knobs** (service key): `BarSegMode` (segment topology; 0 = safe
  recovery shape), `BarSegFlags`/`BarSegBaseMB` (descriptor bisect).
- **Counters** (service key): Gd* (RenderGdi executor), Ch* (CpuHostAperture),
  Pg* (paging engine) — all failure counters must stay 0; S-ring breadcrumbs
  (NOTE: ring persists across boots; high indices go stale after short boots).
- **ETW**: `logman create trace -p Microsoft-Windows-DxgKrnl 0xFFFFFFFFFFFFFFFF
  0xFF` → tracerpt → grep `AzureTriage` = dxgkrnl failure reasons in plain
  text. Found the segment rule in minutes.
- **AddAdapter iteration**: `pnputil /restart-device` re-runs AddAdapter with
  the loaded image — registry-knob experiments need no reboot.
- **Guest probes** (schtasks, session 1; SSH lands in session 0):
  `helios_paintcap` (screenshot → `Z:\tmp\screen_copy.png`), `helios_repaint`,
  `helios_flasher`, `helios_dstate`, `helios_enum_windows`, `helios_regedit`.
  `FindWindow('Progman')` is broken on this box — EnumWindows only.
- **User-mode stack dumps**: `tools/take-minidump.ps1 -ProcessId <pid> -Path <dmp>`
  (P/Invoke MiniDumpWriteDump; the `rundll32 comsvcs.dll,MiniDump` trick writes
  TRUNCATED dumps on this box — do not use). Analyze on Linux:
  `~/.cargo/bin/minidump-stackwalk --symbols-path <breakpad-syms> <dmp>`;
  make syms with `~/.cargo/bin/dump_syms <pdb>` (dir layout
  `syms/<name>.pdb/<GUID+age>/<name>.sym`; fix the MODULE line name if the pdb
  was renamed). The deployed UMD build's PDB must GUID-match the dump's module
  (check with `llvm-pdbutil dump --summary`).
- **KMD build/deploy**: `win_build_kmd` (bumps the three version sites with a
  coherence check, then cargo-make package build) → `win_install_kmd`
  (install script + recommended, toggleable graceful guest reboot — the only
  reliable activation path). Manual fallback: `win_cargo` +
  `tools/install-helios-kmd.ps1` (ExecutionPolicy Bypass,
  `-AllowRebootRequired`); version bump = build.rs numerics + strings +
  Cargo.make stampinf (all three or FAILED_ADD); backups under
  `C:\ProgramData\HeliosDeployBackups`. New tools appear after the win MCP
  server restarts (new session).
- **dxvk staged-content probes** (`dxvk.heliosStagedProbes`, default OFF since
  `bdbbc2ea` — they were the ~1.5 s stall): full-surface raw+post-copy readback
  characterization at fixed refresh ticks for black-surface triage. Re-enable
  per process via `DXVK_CONFIG "dxvk.heliosStagedProbes = True"` (no rebuild).
- **Venus pipeline object trace** (`VN_HELIOS_PIPELINE_TRACE` env, per-process,
  default off): (ring, primary_tail seqno, object id) lines in the ICD diag log
  for pipeline-layout create/destroy, the vn_get_target_ring wait_all barrier,
  and graphics-pipeline creates — the defect-0b recurrence kit. The barrier
  skip/abandon lines (`BARRIER SKIPPED/ABANDONED in wait_all`) are ALWAYS on.
- **ICD sem-deadline strike log** (`helios_icd_diag.log`, always on): each strike
  line carries `sem=` (venus object id), `reason=` (vn_relax reason),
  `sig_queue=/family=/ring=` + `sig_value=/sig_age_ms=` (the most recent submitted
  signal op for that semaphore, recorded at submission prepare) and `pending_ms=`
  (how long a signal had been pending with zero movement — the quantity the
  deadline gates on). `sig_age_ms` near 0 on a strike = wait-before-signal false
  positive (should no longer happen post-f7a816f182f); large `pending_ms` = a
  genuinely stuck host channel.
- **Queue-submit phase timing**: `HELIOS_QUEUE_PERF=1` + `HELIOS_PERF_FILE`
  machine env (live in dwm since the 2026-07-06 reboot) — one aggregate line
  per 300 vkQueueSubmit2 calls (tls/wsi-flush/cache-flush/submit/fence-wait
  phase averages) to `C:\ProgramData\Helios\helios_queue_perf.log`.
- **Ring-fence probe**: `tools/vk_ring_fence_probe.cpp` → schtasks
  `helios_ringprobe` (wrapper `C:\Users\Rupansh\helios-probe\run_ring_probe.cmd`,
  `/rl LIMITED`; `helios_ringprobe_named` runs the NAMED-import mode against the
  dev ICD build via `icd_devbuild.json`). Proves/regression-tests the WS1 #4
  chain: rc=0 + "consumer wait tracked GPU completion". Build on the VM with
  the WinLibs g++ (`g++ -O2 -o ... Z:\tools\vk_ring_fence_probe.cpp -I <VulkanSDK>\Include
  C:\Windows\System32\vulkan-1.dll`) — no clang-cl on the box. **GOTCHA (cost a diagnosis detour): the Vulkan loader
  silently ignores `VK_DRIVER_FILES`/`VK_ICD_FILENAMES` in ELEVATED processes** —
  win_exec/SSH shells are High-IL (and `runas /trustlevel:0x20000` still reads as
  elevated), so an "env-override" probe actually tests the REGISTRY ICD. Run ICD
  A/B probes through a `/rl LIMITED` scheduled task.
- **QEMU fence tracing without restart**: QMP on `/tmp/helios-tpm/mon.sock` →
  `trace-event-set-state` for `virtio_gpu_fence_ctrl`/`virtio_gpu_fence_resp`
  (output → `/tmp/helios-qemu-stderr.log`; ctrl→resp gap per fence id = decode-
  vs GPU-completion retirement; disable after use — it logs 2 lines per fence).
  NOTE: `-d guest_errors` is already on, but virglrenderer's vkr_log/proxy_log
  are INFO-level = SILENT on the release build — absence of host log lines
  proves nothing below WARNING; a real host-side bisect needs a relaunch with
  `VIRGL_LOG_LEVEL=debug`.
