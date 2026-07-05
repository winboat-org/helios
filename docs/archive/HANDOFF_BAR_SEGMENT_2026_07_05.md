# HANDOFF (2026-07-05, 16th session): Option A implemented — CPU-visible BAR segment (id 3)

Fix for the two-memory split (HANDOFF_GDI_EXECUTOR_2026_07_05.md ★FINAL): CPU-rasterized
GDI/standard surfaces now live in a real **CPU-visible BAR memory segment** whose bytes ARE each
allocation's venus blob.

## What changed (kmd_render only; UMD/dxvk untouched)

- **Segment 3 (new)**: head `min(window/2, 1 GiB)` of the 8 GiB host-visible window, reported as a
  classic `CpuVisible=1` + `CpuTranslatedAddress=window.base` + `CacheCoherent` memory segment
  (`query_adapter_info.rs`). Segment 1 (aperture, paging-buffer host) and segment 2 (paging RAM,
  CpuHostAperture) are untouched; seg 3 is only reported when both exist (positional id stability).
- **Window partition** (`virtio/gpu.rs`): `reserve_window_prefix(seg3.size)` starts the KMD blob
  allocator above the VidMm partition; `free_window_range` refuses offsets inside it. StartDevice
  pre-maps the whole partition into kernel VA (`adapter.bar_segment`), torn down in StopDevice.
- **Placement inversion** (`ctrl::map_blob_at` + `gpu::blob_remap_begin`): a blob is host-mapped AT
  the VidMm-assigned SegmentAddress; prior placements and stale overlapping mappings are unmapped
  first. Blob content is intrinsic to the host memory object → remaps are content-preserving.
- **CreateAllocation**: KMD-backed standard allocations (`venus_memory_id != 0` — GDI, shadow,
  staging, shared-primary) get `PreferredSegment/Supported*SegmentSet = seg 3`; UMD-adopted
  allocations stay on the aperture. `AllocationContext` gained a magic ("HALC"), `bar_placed`
  atomic, and `bar_eligible`; `paging_alloc_info`/`set_bar_placement` resolve paging-op handles.
- **BuildPagingBuffer** is now a real engine for seg-3 ops (null engine otherwise):
  - TRANSFER sys→seg: `map_blob_at` then CPU copy MDL→partition VA (synchronous, before the fence).
  - TRANSFER seg→sys (evict): copy out, unmap on the final pass.
  - TRANSFER seg→seg: pure remap. FILL: pattern fill. DISCARD_CONTENT: unmap.
  - UPDATE_PAGE_TABLE (leaf): placement harvest from PTEs (Segment==3), both PageAddress
    interpretations tried. VIRTUAL_FILL via recorded placement; VIRTUAL_TRANSFER = loud counter.
  - Seg-3 work is IRQL-gated (PASSIVE) and never silent: failure counters below.

## Registry counters (HKLM\SYSTEM\CCS\Services\helios_kmd_render)

PgMn placements · PgMr/PgMo last resid/offset-pages · PgTi/PgTo/PgTm transfers in/out/moves ·
PgFn fills · PgDn discards · PgUn PTE harvests · PgSf/PgTs/PgTd last transfer flags/offset/mdl-off.
**Failure (must stay 0)**: PgEi IRQL>PASSIVE · PgEm map fail · PgEb bounds · PgEc discontiguous PTEs
· PgEv virtual op unresolvable · PgEx MDL map fail.

## State

- Deployed **22.22.45.0**, DriverStore pkg `helios_kmd_render.inf_amd64_80fb294a79f63234`, version
  coherence pre-verified (INF DriverVer == .sys FileVersionRaw). Device shows
  CM_PROB_FAILED_POST_START = normal old-image-loaded limbo → **cold boot activates**.
- Rollback: `C:\ProgramData\HeliosDeployBackups\20260705-195224` (or rebind pkg 1f8250 / 22.22.44).
- Committed: executor per-arm fix + diag counters as `9058355`. Option A changes **UNCOMMITTED**
  pending boot verification.

## Post-boot verification

1. Adapter binds (no FAILED_ADD). 2. PgMn > 0 and all PgE* == 0. 3. `schtasks /run /tn
helios_repaint` → GdCn/GdCc red fill into a dwm-imported resid (GdXn may legitimately stay 0 now —
CPU text writes the BAR directly, bypassing RenderGdi). 4. `schtasks /run /tn helios_paintcap` →
`Z:\tmp\screen_copy.png` shows the RED desktop + 9 icons + GDI text. Only screenshot/owner-visible
desktop counts.

Boot-answered open questions: does VidMm CPU-touch seg-3 before backing it (bugcheck risk — historic
unbacked-BAR lesson); classic vs virtual paging ops (PgEv); PTE PageAddress semantics (PgEb);
multipass evict flags (PgSf).

# ★ FINAL: DESKTOP RENDERS UNDER HELIOS — CpuHostAperture segment id 2 (mode 10)

The 22.22.45 classic-CpuVisible shape AND the 22.22.46 CpuHostAperture-as-id-3 shape were both
rejected at AddAdapter. Registry-knob bisect (BarSegMode/BarSegFlags/BarSegBaseMB + devcon restart;
AddAdapter re-runs without reboot) + dxgkrnl ETW AzureTriage ("Invalid flags specified for segment
#2") found the rule: **dxgmms requires a SupportsCpuHostAperture segment to be the LAST segment**.
Every 3-segment layout died because the RAM cpu-host segment (id 2) had a successor.

**Production shape (BarSegMode 10, code default since 22.22.50):** aperture (id 1) + BAR window
head (id 2, CpuHostAperture, 1 GiB); the paging-RAM segment is dropped (vestigial). With GDI
surfaces in the device segment, win32k routes rasterization through DxgkDdiRenderGdi — the
(15th-session-fixed) executor writes the venus blob bytes dwm samples. GdXn (ClearType ops at
RenderGdi) moved off zero for the first time ever. MapCpuHostAperture is real for the BAR segment
(blob mapped at the dxgkrnl-chosen aperture offset; whole-allocation runs only, loud Ch* counters).

**Verified mid-session (screenshot Z:\tmp\screen_copy.png 20:53): red desktop + icons with
ClearType labels + taskbar/tray text; dwm/explorer on helios_umd.dll, no d3d10warp.** Committed
`0c8f44b` + `e5d45d9`; deployed 22.22.50 pkg 621a0c75c625bfaa. Cold-boot verification pending.

Rollback ladder: BarSegMode=0 (reg only, binds without the feature) → DriverStore backups
20260705-19*/20*. Secondary opens: 0xC00000BB invalid-NTSTATUS AzureTriage complaint (unchased),
conhost body blank in screenshot, helios_repaint no longer moves GdiE, explorer DEVICE_LOST hunt.
