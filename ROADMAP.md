# ROADMAP — Stage: Performance, Stability, Conformance (PSC)

*Started 2026-07-05, the day the desktop first rendered end-to-end under Helios
(DWM composites on Helios → venus → host GPU → IddCx → Looking Glass). Bring-up
is over; this stage makes it reliable, fast, and D3D11-conformant. Archived
bring-up knowledge lives in `docs/archive/`; operational debug knowledge stays
in `NTOSEYE.md` and `BRINGUP_QUIRKS.md`.*

## Verified baseline (2026-07-05, KMD 22.22.50)

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
    UMD's dst-copy no-ops). Remaining engineering, bounded: (a) lazy vehicle
    device creation from the ICD outside its locks (D3D11CreateDevice +
    factory + dcomp), (b) a private texture→venus-resid query on the UMD
    (C1 identity trailer already knows it), (c) ICD-side WS1 #4 publish of
    (backbuffer resid, copy-fence value) — dwm's consumer wait then orders
    the read with ZERO dwm-side changes, (d) present-mode mapping (immediate
    = ALLOW_TEARING + Present(0), fifo = Present(1)), (e) swapchain
    resize/teardown lifecycle. The async sw worker stays as fallback for
    windows where the vehicle fails.
- **Venus submit/fence latency**: ARCH.md's original benchmark item. The
  async/interrupt transport (C3/M3.4) landed; measure round-trip and
  present-to-scanout latency.
- **Capture path**: IddCx frame drop policy vs D3D12 copy queue saturation;
  KVMFR bandwidth; 10 bpc default.
- Candidates list from the NVIDIA fix era lives in ICD.md.

## Workstream 3 — D3D11 Conformance

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
