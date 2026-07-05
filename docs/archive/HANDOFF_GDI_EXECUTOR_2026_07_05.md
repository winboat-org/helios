# Handoff — KMD GDI executor dropped final batch commands (15th session, 2026-07-05)

Read the `gdi-executor-dropped-final-commands` memory first. Prior context:
`HANDOFF_DESKTOP_PLATE_2026_07_05.md` (14th session) — but note the CORRECTIONS below.

## State
- Committed: main `d1bbbe1`, dxvk-helios `8e11b13a`. Deployed UMD `73335327e221d438` (unchanged).
- **kmd_render fix UNCOMMITTED** in `kmd_render/src/ddi/gdi_blit.rs`; package 22.22.42.0 built,
  published via devcon to DriverStore `helios_kmd_render.inf_amd64_5d8f9d027a1fae9a`.
  **AWAITING COLD BOOT** (ImagePath changed; reboot is the activation path).
  DriverStore backup: `C:\ProgramData\HeliosDeployBackups\20260705-182830`.

## THE BUG (found via diag counters, deterministic repro, no owner needed)
`gdi_blit.rs::execute()` gated dispatch on `off + size_of::<DXGK_RENDERKM_COMMAND>() <= total`.
The struct's union is sized by its LARGEST arm (~128 B); a COLORFILL command is ~48 B; dxgkrnl sizes
`CommandLength` tightly → **the final command of a batch was silently dropped whenever its union arm
was smaller than the largest one**. Counters: GdiS=952 skipped vs GdiE=1004 executed (≈48%!), all
per-reason failure counters zero. Repro: every forced desktop repaint = +18/19 executed, **+1
skipped** (the lone plate COLORFILL); idle = 0/0.

Fix: per-arm validation (`read_arm<T>` checks `avail >= offset_of!(…, Command)+size_of::<T>()`),
new counters `GdTc` (truncated command) and `GdDs` (empty-src StretchBlt, now a counted no-op that
returns executed instead of silently skipping).

## POST-BOOT VERIFICATION (in order; all runnable without the owner except the boot itself)
1. `schtasks /run /tn helios_repaint` (session-1 forced desktop redraw), diff
   `HKLM\SYSTEM\CCS\Services\helios_kmd_render` GdiE/GdiS/GdTc/GdDs before/after.
   EXPECT: GdiS stops incrementing per repaint. If GdDs increments per repaint and the desktop is
   still black → the dropped op was a degenerate stretch (missing wallpaper source surface) — chase
   why the source is empty.
2. `schtasks /run /tn helios_paintcap` → read `Z:\tmp\screen_copy.png` from the Linux side (the Read
   tool renders it). EXPECT: desktop plate = the owner's solid RED (registry Background="232 17 35"),
   possibly icons. This is an autonomous guest screenshot — use it liberally.
3. WUDFHost dxvk-log probes: composed-frame nonzero should jump from ~34% toward ~100% if the plate
   fills red.
4. If desktop renders: commit the kmd_render fix (scoped commit), then consider the perf follow-ups
   (dirty-driven staged refresh, probe removal) per the 14th handoff.

## CORRECTIONS to the 14th-session frontier framing (verified this session)
- **Progman EXISTS, VISIBLE, full-screen (0,0)-(1896,1030)** with SHELLDLL_DefView + SysListView32
  (both VIS, full rect) and **9 icons** (LVM_GETITEMCOUNT). The desktop window tree was never absent.
- ⚠️ `FindWindow('Progman')` returns NULL on this box (gle=203) while EnumWindows+GetClassName finds
  it. ALL Progman probing must use EnumWindows. (Unexplained class-atom/SxS quirk; incidental.)
- win32k's LIVE desktop color is the red (GetSysColor(1)=0x2311e8); session is console; wallpaper=''
  with BackgroundType=1 (solid color) → dwm creating no wallpaper texture was EXPECTED, not evidence.
- Display mode is currently **1896x1030** (LG dynamic sizing): IddCx buffers are the 1896x1030
  shared set; 1896x48 = taskbar strip.
- The "uniform black composed frame" at 17:56 was the transient explorer-dead window during a restart
  experiment — NOT a capture freeze.
- Explorer restarts and the HKLM Policies\Explorer NoActiveDesktop trio were red herrings (values are
  2024 base-image leftovers; deleted+restored, backup `C:\ProgramData\Helios\policies_explorer_backup.reg`).

## Secondary finding (recorded, unchased): explorer-only VK_ERROR_DEVICE_LOST bursts
Explorer hit a 25-submission DEVICE_LOST burst this boot (dwm/WUDFHost/others clean); host log had
vkResetCommandPool-while-pending validation errors (2 render-server workers). The 14th session filed
that VU as benign — now suspect: resetting an in-use pool is the signature of a guest fence signaling
before host completion (async-transport wire-fence path). Sporadic (a fresh explorer ran clean).
Candidate hunt after the desktop renders.

## New probe toolkit (tools/*.ps1 + registered schtasks; SSH lands in session 0 — tasks run in session 1)
`helios_desk_probe`, `helios_enum_windows`, `helios_fve`, `helios_repaint`, `helios_dstate`,
`helios_paintcap` (guest screenshot → Z:\tmp\*.png), `helios_flasher` (forces dwm frames → advances
WUDF probe ticks), `helios_explorer_restart`. Run: `schtasks /run /tn <name>`; outputs in
`C:\ProgramData\Helios\*.txt`. GOTCHAS: PowerShell 5.1 parses .ps1 as ANSI — no non-ASCII in strings;
`umd-<pid>.log` appends ACROSS BOOTS (slice from the last "UMD module:" line); the 64-line 11.1 DDI
log budget is consumed by Discards (ClearView still unlogged — only matters if the plate stays black).

Standing directives unchanged: no hacks; loud failure over fake success; evidence-first; never blame
the host; only owner-visible desktop counts; ask before cold boots.

---

# ⚠️ SESSION-END CORRECTION (owner): the "healed" desktop was WARP

During the 22.22.43.0 install churn the Helios render adapter went **CM_PROB_FAILED_ADD** (verified:
Get-PnpDevice Error; GdiE frozen; dwm/WUDF UMD logs stopped). dwm fell back to **WARP/Basic Render**
and the desktop rendered perfectly (red bg, 9 icons, ClearType labels, interactive). The earlier
"fresh epoch heals it" interpretation in this doc's history is WRONG — discard it.

What the WARP state PROVES (huge discriminator):
- LGIdd + IddCx capture + KVMFR/LG chain + session + desktop content are ALL healthy without Helios.
- The black desktop is strictly a **Helios-render-path delivery failure**: with Helios active (and the
  executor batch-drop fix in place, verified +18/+0 per repaint), the desktop plate fill + icons +
  GDI text never reach the composed output; regedit's white client fill WAS visible under Helios
  while its text was not; the resid-65 (1896x1030 host-visible) blob stays all-zero.

CURRENT VM STATE: Helios adapter CM_PROB_FAILED_ADD (desktop on WARP); instrumented KMD 22.22.43.0
(pkg be08771acf118578) is the bound package and should load at next cold boot. If FAILED_ADD persists
after a clean boot, suspect the in-place-update flakiness first (NOT the instrumentation — it only
touches gdi_blit internals); rollback = 22.22.42 pkg 5d8f9d027a1fae9a or DriverStore backup
C:\ProgramData\HeliosDeployBackups\20260705-182830.

NEXT COLD BOOT PLAN (Helios active, black desktop expected):
1. helios_repaint → read GdCn/GdCr/GdCc/GdCg, GdBn/GdBr, GdXn/GdXz:
   - No big red fill → desktop GDI ops don't reach RenderGdi under Helios (routed to a CPU/CDD path?).
   - Big red fill into resid R → diff R vs dwm's imported resids (open logs); who fails to sample R?
   - GdXz≈GdXn → ClearType alpha sources empty under Helios → trace glyph-surface writes (Lock path?).
2. Screenshot via helios_paintcap for the Helios-active ground truth; WARP screenshots exist for diff.

---

# UPDATE (19:10): FAILED_ADD root-caused + fixed; frontier statement finalized

- FAILED_ADD (0xc0000182 STATUS_DEVICE_CONFIGURATION_ERROR, deterministic, survived cold boot) =
  **INF DriverVer vs .sys FIXED version-resource mismatch**: the 22.22.43 bump changed build.rs
  string FileVersion/ProductVersion but not the numeric `FILEVERSION 22,22,42,0` lines, so
  FileVersionRaw stayed 22.22.42.0 while the INF said 22.22.43.0. dxgkrnl AddAdapter rejects that
  (ring: AddDevice OK -> abort pre-StartDevice -> RemoveDevice). Proof both ways: rebinding 22.22.42
  bound instantly on the same system; a coherent 22.22.44 (numerics+strings+stampinf) binds OK.
  GOTCHAS: bump all THREE version sites (build.rs numeric FILEVERSION/PRODUCTVERSION, build.rs
  strings, Cargo.make.toml stampinf -v); reinstalling the SAME DriverVer silently rebinds the
  already-imported package.
- With Helios genuinely active mid-session (22.22.42 fix build, adapter freshly restarted, GdiE
  moving +19/+0): desktop STILL BLACK (PrintWindow(Progman) black, screen black). **The "adapter
  restart heals it" observation was always the WARP fallback.** Final frontier statement:
  WARP = everything perfect; Helios = app-window fills deliver, desktop plate/icons/GDI-text do not.
- STATE: instrumented+coherent **22.22.44.0 (pkg 1f82503224045c56) bound, device OK, old image still
  loaded**. NEXT COLD BOOT loads it; then: helios_repaint -> read GdCn/GdCr/GdCc/GdCg + GdBn/GdBr +
  GdXn/GdXz -> where does the desktop's big red fill go, and does dwm import that resid? Plus
  helios_paintcap screenshot. Backups: 20260705-182830, 20260705-190844.

---

# ★ FINAL (19:20): BLACK-DESKTOP ROOT CAUSE = TWO-MEMORY SPLIT (aperture pages vs venus blob)

Instrumented 22.22.44 live under Helios (black state): desktop repaint → NO red fill at RenderGdi
(only the probe console's own erase); **GdXn=0 — zero ClearType ops ever**. PrintWindow CPU-paints
the full correct desktop. ⇒ desktop erase/icons/all GDI text are CPU-rasterized into the allocation's
VidMm backing = APERTURE-SEGMENT SYSTEM PAGES (create_allocation.rs:554), while dwm samples the
allocation's venus HOST-VISIBLE BLOB, which only the RenderGdi executor writes. BuildPagingBuffer is
a null engine ("venus needs no transfer copies" — wrong for CPU content); nothing syncs the two.
WARP = one memory = everything renders.

NEXT SESSION = design + implement the fix (no hacks): **Option A — CPU-visible BAR memory segment
placement for GDI/shared surfaces**: real segment placement (id 2), RESOURCE_MAP_BLOB each surface's
blob at the VidMm-assigned SegmentAddress, implement TRANSFER/FILL paging ops as CPU copies,
pre-map the BAR window at StartDevice for DISPATCH-safe access. Open questions: VidMm segment
allocator vs our blob-window offsets; interaction with existing dwm GDI-staging (dxvk side) and the
executor's map_blob_prepare; eviction semantics.

Also landed this session (kmd_render, UNCOMMITTED): per-arm executor fix (real ~48% op-drop bug),
big-op/ClearType diag counters, version 22.22.44. GOTCHA: KMD version bump = build.rs numeric
FILEVERSION/PRODUCTVERSION + string values + Cargo.make stampinf -v (mismatch ⇒ AddAdapter
0xc0000182 FAILED_ADD); same-DriverVer reinstall rebinds the old package.
