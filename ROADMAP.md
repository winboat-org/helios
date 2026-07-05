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

## Workstream 1 — Stability (NEXT SESSION'S FOCUS)

Open defects, roughly ordered:

1. **IDD frame freeze** (live at stage start). Evidence bundle:
   `C:\ProgramData\Helios\freeze_evidence_20260705\` +
   `tmp/freeze_evidence_20260705/qemu-stderr-tail.log`. Snapshot: IDD
   (WUDFHost) loops `ResolveSharedResource` on the same 1896x1030 alloc; dwm
   presented to #1561 then went log-quiet; host shows live
   `vkResetCommandPool ... is in use` validation errors.
2. **Early-fence suspicion**: host `vkResetCommandPool`-while-pending VUs
   correlate with explorer `VK_ERROR_DEVICE_LOST` bursts — suspected guest
   fence signaling before host completion in the async-transport wire-fence
   path (`kmd_render` C3/M3.4). This may be the root of both #1 and the
   DEVICE_LOST bursts.
3. **dwm shared-resource creation failure**: `create_resource(tex2d): DXVK
   memory not importable (res_id=0, offset≠0)` — suballocated VkDeviceMemory
   cannot be exported/shared; DXVK must use dedicated allocations for
   shareable resources. Seen in the freeze-window dwm log.
4. **dxgkrnl "Driver returned an invalid NTSTATUS 0xC00000BB"** (ETW
   AzureTriage) — some query answered STATUS_NOT_SUPPORTED where that return
   is illegal. Tolerated today; find and fix the query.
5. **WUDFRd cold-boot race** ("SCM not ready", boot+23s) — LGIdd loads late;
   pairing is resilient now but the race window is still there.
6. **In-place KMD update flakiness** — CM_PROB_FAILED_POST_START limbo until
   reboot is expected, but keep the version-coherence gotcha (three sites) and
   backup ladder in mind.
7. **Mechanism question (understand before optimizing)**: post-cold-boot, GDI
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
- **Deploy**: `win_cargo` → `tools/install-helios-kmd.ps1` (ExecutionPolicy
  Bypass, `-AllowRebootRequired`); version bump = build.rs numerics + strings
  + Cargo.make stampinf (all three or FAILED_ADD); backups under
  `C:\ProgramData\HeliosDeployBackups`.
