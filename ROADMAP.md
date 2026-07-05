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
   damage rate: ~10/s under the 10 Hz flasher vs ~6/s before). Remaining: soak + cold-boot
   verification; `HELIOS_QUEUE_PERF=1` + `HELIOS_PERF_FILE` are set machine-wide and reach
   dwm at the next reboot for steady-state submit-phase numbers.
2. **dxvk-helios device-loss hygiene — FIXED (11a30110, deployed 2026-07-05):**
   (a) dropped post-loss submissions now `notifyObjects()` + recycle (was: permanent in-use
   ref leak → dwm compositor wedge); (b) `waitForResource` bails on DEVICE_LOST (loud warn);
   (c) on lost, bounded 2 s grace wait then deliberate cmdlist leak instead of resetting a
   pool with host-pending buffers (was: the vkResetCommandPool VUs). Loss-path behavior not
   yet exercised end-to-end (needs a forced-loss test: VN_HELIOS_SEM_DEADLINE_MS=1 on a
   probe process).
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
4. **KMD wire-fence semantics** (now also the GHOSTING architectural fix): ring-0 used-ring
   returns complete WDDM DMA fences at host *decode* time (virglrenderer 1.3.0: ring 0
   retires immediately; ring ≥ 1 = real GPU completion via vkr queue sync threads). Violates
   the "never signal a wire fence before host completion" invariant at the dxgkrnl level.
   18th-session scoping: the ICD ALREADY puts `ring_idx` in the SUBMIT_VENUS escape header
   (`helios_ioctl_submit_cs`, vn_renderer_helios.c:1092), and vn queues carry ring_idx ≥ 1 —
   but with semaphore/fence FEEDBACK active (the default) vn never submits per-queue-fence
   batches, so everything on the wire is ring-0 ring-notify traffic (the 0/251,810 stat).
   Work: KMD honors hdr.ring_idx (VIRTIO_GPU_FLAG_INFO_RING_IDX + per-ring in-flight fence
   table), then cross-process present→IDD ordering via win32-sync external semaphores riding
   ring≥1 wire fences (replaces the interim present gate, moves the wait to the consumer,
   GPU-side). Also: `helios_sync_append_locked(fence_id=0)` blind-signals + clears older
   pendings (latent).
5. **dxgkrnl "Driver returned an invalid NTSTATUS 0xC00000BB"** (ETW
   AzureTriage) — some query answered STATUS_NOT_SUPPORTED where that return
   is illegal. Tolerated today; find and fix the query.
6. **WUDFRd cold-boot race** ("SCM not ready", boot+23s) — LGIdd loads late;
   pairing is resilient now but the race window is still there.
7. **In-place KMD update flakiness** — CM_PROB_FAILED_POST_START limbo until
   reboot is expected, but keep the version-coherence gotcha (three sites) and
   backup ladder in mind. 2026-07-06 state: ACTIVE driver = **oem58.inf = 22.22.51
   (pkg c393e58c1b189688)**; `UserModeDriverName` → ProgramData
   `helios_umd_1b4e26b65f718522.dll` (RELEASE profile, CS-side rotation + log fixes);
   the DriverStore UMD copy in c393e58c is stale/lock-protected — do not trust
   DriverStore file timestamps as "deployed" evidence; check the loaded-module path of a
   fresh dwm instead. Active ICD = `vulkan_virtio-7c94d802f270.dll` (strike attribution +
   pending-window deadline fix, mesa f7a816f182f).
8. **Mechanism question (understand before optimizing)**: post-cold-boot, GDI
   content renders while RenderGdi (GdiE), MapCpuHostAperture (ChMn) and
   paging (Pg*) counters all stay idle, yet 8 standard allocations sit in
   segment 2. Which path carries the GDI bytes? Candidates: UMD Lock → ICD
   escape blob mapping (coherent by construction), or dwm-side dxvk GDI
   staging. Answer determines what is hot-path and what is dead code.

## Workstream 2 — Performance

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
- **ICD sem-deadline strike log** (`helios_icd_diag.log`, always on): each strike
  line carries `sem=` (venus object id), `reason=` (vn_relax reason),
  `sig_queue=/family=/ring=` + `sig_value=/sig_age_ms=` (the most recent submitted
  signal op for that semaphore, recorded at submission prepare) and `pending_ms=`
  (how long a signal had been pending with zero movement — the quantity the
  deadline gates on). `sig_age_ms` near 0 on a strike = wait-before-signal false
  positive (should no longer happen post-f7a816f182f); large `pending_ms` = a
  genuinely stuck host channel.
- **Queue-submit phase timing**: `HELIOS_QUEUE_PERF=1` + `HELIOS_PERF_FILE`
  machine env (SET, active for dwm after the next reboot) — one aggregate line
  per 300 vkQueueSubmit2 calls (tls/wsi-flush/cache-flush/submit/fence-wait
  phase averages) to `C:\ProgramData\Helios\helios_queue_perf.log`.
