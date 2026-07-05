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

Open defects, roughly ordered:

1. **Stall (primary, owner-visible as freeze/slowness)**: per-present full device drain in
   `HeliosDxvkDevice::rotate_resource_backings` (bring-up shim, self-documented as awaiting the
   async-fence rework); plus a suspected next-frame-unblocks-previous-frame wait chain
   (mid-stall dump: all dxvk threads idle while host timeline sits behind for 8 s).
   MEASURE FIRST: `HELIOS_PERF`/`HELIOS_QUEUE_PERF` (machine env + dwm restart).
2. **dxvk-helios device-loss hygiene** (fixes the permanent-wedge escalation):
   (a) dropped post-loss submissions must still `notifyObjects()`;
   (b) `waitForResource`/`synchronizeUntil` must bail on `m_lastError == DEVICE_LOST`;
   (c) on lost, skip `vkResetCommandPool` of pending pools (leak deliberately).
3. **dwm shared-resource creation failure**: `create_resource(tex2d): DXVK
   memory not importable (res_id=0, offset≠0)` — suballocated VkDeviceMemory
   cannot be exported/shared; DXVK must use dedicated allocations for
   shareable resources. Likely drives the IDD's per-frame `ResolveSharedResource` loop.
4. **KMD wire-fence semantics**: ring-0 used-ring returns complete WDDM DMA fences at host
   *decode* time (virglrenderer 1.3.0: ring 0 retires immediately; ring ≥ 1 = real GPU
   completion via vkr queue sync threads). Violates the "never signal a wire fence before host
   completion" invariant at the dxgkrnl level. Guest already assigns per-queue ring_idx ≥ 1 +
   `VkDeviceQueueTimelineInfoMESA`; unused on the SUBMIT_3D wire today. Also:
   `helios_sync_append_locked(fence_id=0)` blind-signals + clears older pendings (latent).
5. **dxgkrnl "Driver returned an invalid NTSTATUS 0xC00000BB"** (ETW
   AzureTriage) — some query answered STATUS_NOT_SUPPORTED where that return
   is illegal. Tolerated today; find and fix the query.
6. **WUDFRd cold-boot race** ("SCM not ready", boot+23s) — LGIdd loads late;
   pairing is resilient now but the race window is still there.
7. **In-place KMD update flakiness** — CM_PROB_FAILED_POST_START limbo until
   reboot is expected, but keep the version-coherence gotcha (three sites) and
   backup ladder in mind.
8. **Mechanism question (understand before optimizing)**: post-cold-boot, GDI
   content renders while RenderGdi (GdiE), MapCpuHostAperture (ChMn) and
   paging (Pg*) counters all stay idle, yet 8 standard allocations sit in
   segment 2. Which path carries the GDI bytes? Candidates: UMD Lock → ICD
   escape blob mapping (coherent by construction), or dwm-side dxvk GDI
   staging. Answer determines what is hot-path and what is dead code.

## Workstream 2 — Performance

Known costs, unmeasured — measure before optimizing:

- **Frame-update slowness** (owner-visible): known suspects = dxvk-helios
  persistent-refresh (14th session, alias-image staging + per-frame refresh)
  and the diagnostic probes. Quantify, then decide what to gate/remove.
- **Diagnostics overhead**: per-batch registry writes (RenderGdi counter
  dumps), per-op logging, the 11.1-DDI log budget. Introduce a master
  diag kill-switch (registry knob, like `BarSegMode`) and default it off
  once stability holds.
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
- **Deploy**: `win_cargo` → `tools/install-helios-kmd.ps1` (ExecutionPolicy
  Bypass, `-AllowRebootRequired`); version bump = build.rs numerics + strings
  + Cargo.make stampinf (all three or FAILED_ADD); backups under
  `C:\ProgramData\HeliosDeployBackups`.
