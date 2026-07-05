# Gate 2/3 — Backing the Advertised Mandatory Caps

Status: plan, 2026-06-18. The `kmd_render` WDDM adapter advertises mandatory
render-only caps it does not yet back (the "coherence debt" tracked in
`WDDM_RENDER_ONLY_DDI_CHECKLIST.md`). This is the plan to make each advertised
cap honest — either implement the path, or report the honest minimum that still
loads at Code 0. No cap stays advertised without a real, tested path
(`WDDM_RENDER_ONLY_3_2.md` §2).

Decision (2026-06-18): do this BEFORE the Gate 5a ICD transport, because the KMD
work converges — a real `DxgkDdiCreateAllocation` + `Render`/`SubmitCommand` both
backs the caps AND is the KMD half of Gate 5a Stage 2/3.

## RESOLVED (2026-06-18): the DWM crash was a UMD inconsistency, NOT a KMD cap

Root cause found and fixed. DWM (observed calling `CreateDevice` on Helios, pids
1896/10604) crash-looped in `dwmcore.dll 0x889800b0` because Helios advertised
full render caps (Code 0) **and** a fabricated `GetSupportedVersions` list — so
the D3D runtime picked a version and called the UMD `CreateDevice`, which
honestly returns `E_NOTIMPL`, and DWM cannot handle "advertises render caps but
fails CreateDevice" → crash loop.

**Fix (`.62`): UMD `GetSupportedVersions` returns an EMPTY list** (honest: no D3D
device DDI version is backed until the device funcs table is real, Gate 5b). The
runtime then never calls `CreateDevice` on Helios and skips it for D3D, which DWM
handles gracefully. Verified: Code 0 preserved; `CreateDevice` calls dropped to 0
(`helios_umd.log`); DWM went from crashing every ~30s–2min to **0 crashes in a
3-minute window** (one install-time restart only). This also unblocks Looking
Glass (stable DWM → stable IDD swapchain). Gate-5-safe: the venus path uses
D3DKMT thunks directly, independent of this list.

**Implication for this doc:** none of the KMD mandatory caps needed real backing
to fix DWM — they are load-bearing for Code 0 only and DWM does not trip on them.
So Steps 1–3 below are now driven by **Gate 5 rendering needs**, not DWM. Back
`MemoryManagementCaps` (allocations) + `SupportKernelModeCommandBuffer`
(Render/SubmitCommand) as the KMD half of Gate 5a Stage 2/3; leave Flip /
preemption / per-engine-TDR as documented load-only coherence debt until a real
path exercises them (or HLK forces it). Re-add the correct single
`GetSupportedVersions` entry (D3D11_0 = `0x000b000a`) when the device funcs table
is implemented (Gate 5b).

## Context: the DWM crash is NOT (clearly) a kernel-engine fault

Investigated 2026-06-18: `dwm.exe` crash-loops in `dwmcore.dll` (`0x889800b0`),
**user-mode only** — no dxgkrnl TDR (4101), no bugcheck, `helios_umd.dll` not
loaded in DWM, latest `LiveKernelReports` dump is days old (06-04). DWM never
creates a device on Helios (CreateDevice → E_NOTIMPL), so it does not submit GPU
work to our null engine. Therefore backing the null *engine* may not fix DWM;
the more likely trigger is a **cap value DWM mishandles** at the DXGI/composition
layer — which makes the display-flavored caps (Flip/SectionBackedPrimary) the
prime suspects. Backing/cleaning the caps is correct regardless (the spec forbids
advertising unbacked caps), but set expectations: the DWM fix may be "stop
advertising a display cap on a render-only adapter," not "implement the engine."

## Cap-by-cap plan

| Cap | Honest action | Gate | Converges with |
|---|---|---|---|
| `WDDMVersion = 3.2` | keep (honest) | — | — |
| `FlipOnVSyncMmIo` + `MaxQueuedFlipOnVSync` | **Step 0 experiment**: try dropping. If still Code 0 → drop (render-only has no flip). If Code 43 → must implement a flip path. | 2 | — |
| `SectionBackedPrimary` | same Step-0 experiment; "primary" implies scanout we don't have | 2 | — |
| `SupportKernelModeCommandBuffer` | implement real `DxgkDdiRender`/`RenderKm` + DMA→virtio `SubmitCommand` | 3 | Gate 5a Stage 3 |
| `MemoryManagementCaps` (segment/alloc) | real `DxgkDdiCreateAllocation` (system-memory class standalone; venus-blob class needs the ICD mem id) + `DescribeAllocation` + real `BuildPagingBuffer` | 2 | Gate 5a Stage 2 |
| `PreemptionAware` + `PreemptionCaps` | implement `DxgkDdiPreemptCommand` + report granularity that matches the real engine (likely keep `DMA_BUFFER_BOUNDARY`, or `NONE` if we never preempt — and then clear `PreemptionAware` to stay consistent) | 3 | — |
| `SupportPerEngineTDR` | implement `DxgkDdiResetEngine` + `QueryEngineStatus` for the one node, or stop advertising | 3 | — |
| `MultiEngineAware` | one node today; keep only if the scheduler is genuinely engine-aware, else clear | 3 | — |

## Order

- **Step 0 (cheap, decisive): minimal-cap bisect — DONE 2026-06-18, RESULT: caps
  are MANDATORY, no honest drop.** `.59` dropped Flip + MaxQueuedFlip +
  SectionBackedPrimary → **Code 43**. `.60` re-added SectionBackedPrimary, kept
  Flip dropped → **still Code 43**. So **`FlipOnVSyncMmIo` is load-bearing** for
  dxgkrnl render-adapter init even on a render-only adapter (NumVidPnSources/
  Targets = 0). The display-flavored caps are NOT over-advertised; they cannot be
  honestly dropped — they must be **backed with real impl**. `.61` restored the
  full set → Code 0. (Side datapoint: while Helios was Code 43 (~2 min), DWM did
  not crash-loop as before — suggestive that a *started* Helios with unbacked
  caps drives the `dwmcore.dll 0x889800b0` crash, but not conclusive.) Conclusion:
  proceed to Step 1+ (implement the caps); there is no cap to remove.
- **Step 1 (Gate 2): real memory manager.** `DxgkDdiCreateAllocation` creates
  real system-memory-backed allocations (CPU-visible segment) with correct
  `DescribeAllocation`; `BuildPagingBuffer` handles transfer/fill on that segment.
  Backs `MemoryManagementCaps`. Standalone-testable with a D3DKMT allocation
  harness (no ICD/venus needed). Venus-blob allocations remain coupled to the ICD
  mem id (Gate 5a Stage 2).
- **Step 2 (Gate 3): real submission.** `DxgkDdiRender`/`Patch` accept a DMA
  command buffer; `SubmitCommand` stops faking immediate completion and runs a
  real (even if simple) engine with interrupt/DPC fence completion. Backs
  `SupportKernelModeCommandBuffer`. = Gate 5a Stage 3 KMD side.
- **Step 3 (Gate 3): preemption + per-engine TDR.** Implement
  `DxgkDdiPreemptCommand`, `ResetEngine`, `QueryEngineStatus`; reconcile
  `PreemptionAware`/`SupportPerEngineTDR`/`MultiEngineAware` with what the engine
  actually does (implement or clear — no fakes).

## Test

- Code 0 + dxdiag clean after each step (no regression).
- A D3DKMT allocation/submit harness exercises CreateAllocation/Render/
  SubmitCommand directly (no D3D11 device-funcs table needed).
- Watch DWM after Step 0 and Step 2 — does the crash loop stop?
