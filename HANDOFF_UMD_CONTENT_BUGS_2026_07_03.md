# Handoff — FOUR UMD content-killing contract bugs found & fixed via minimal repro; dwm's own composition still writes zero (2026-07-03, sixth session, NVIDIA boot)

## ⚡ What this session established (all instrument-verified on the live NVIDIA boot)

The ⚡⚡ K-B2 "KMD present nop" theory from the previous handoff is **falsified**:
`DxgkDdiPresent` is **never called** (the `PBcall` named diag value never exists — the
tracer mechanism itself is proven live by the `AE*` values, and `PBcall` is recorded
*before* the flags early-return, so even flip presents would bump it). The IddCx model
needs no present blit at all: **dwm's flip-model swapchain backbuffers ARE the IddCx
swapchain buffers** (verified: dwm's created backbuffer resids == the resids WUDFHost
opens — e.g. 217/218/219 both sides). The pixel path is dwm's own D3D11 rendering.

The black screen decomposed into a STACK of real UMD contract bugs, each isolated with
a minimal reproducer (`tools/d3d11_shared_draw_probe.cpp`, run via schtasks
`HeliosDrawProbe`) and fixed properly:

### 1. D3D11.1 blend-desc misread → EVERY pipeline wrote only the RED channel
The device negotiates DDI interface **0xb000f (D3D11.1)**; `fill_d3d11_1_device_funcs`
casts the 11.1 table down to the 11.0 shape and `install()` wired the 10.1-desc
`create_blend_state` into a slot that actually receives `D3D11_1_DDI_BLEND_DESC`
(inserts `LogicOpEnable` after `BlendEnable` and `LogicOp` before the write mask; NOT
prefix-compatible). The 10.1 `RenderTargetWriteMask` offset lands on the 11.1
`BlendOpAlpha` = ADD = **1** → `colorWriteMask = R_BIT` on every blend state including
the runtime defaults. Repro: PS returning (0.2,0.4,0.6,0.8) over a white clear left
`0xFF33FFFF` (only R written). **This is the red-tint class** and it silently poisoned
ALL D3D11 rendering; `selftest_triangle` was blind to it (its expected color is pure
red = the x component). Fix: `create_blend_state_11_1` (+`calc_size_blend_11_1`) using
`ID3D11Device1::CreateBlendState1` incl. LogicOp translation, installed in
`install_11_1`. Verified: probe now reads `0xCC336699` (all four components exact).
**Audit rule discovered: EVERY DDI whose arg struct changed between the 11.0 and 11.1
tables must have an 11.1-typed handler — the blend desc was one; check any new installs
against this class. (Rasterizer desc only APPENDS ForcedSampleCount → prefix-safe.)**

### 2. Typed shader I/O signatures were dropped → all inputs declared float32
The ≥11.1 create-shader DDIs pass `D3D11_1DDIARG_STAGE_IO_SIGNATURES` whose ENTRY2
carries **`RegisterComponentType`** (the audit's "the DDI signature entries lack types"
is wrong for ≥11.1). The UMD ignored them and the bridge wrapped raw tokens in a
container with NO ISGN/OSGN → dxbc-spv fell back to float32 for every input → dwm's
R16G16_SINT vertex bindings vs float32 SPIR-V inputs = VUID-Input-08733 UB (the
validate-boot storm). Fix: `create_{vertex,pixel,geometry}_shader_11_1` →
`flatten_stage_io_signatures` → bridge `create_shader_sig` →
`prepare_shader_bytecode_with_sigs` builds real ISGN/OSGN chunks (24-byte entries,
TEXCOORD<reg> names, true sysval + component type, dxbc hash stamped). Verified: the
"No signature entry for register o0" warnings are gone; an int2/R16G16_SINT + CB +
DrawIndexed quad renders exactly.

### 3. `RotateResourceIdentities` was a Flush-only stub → flip rotation never happened
dwm rendered into ONE constant allocation forever while dxgkrnl/IddCx walked the
3-buffer ring (2 of 3 acquired frames were buffers dwm never touched; the IDD's
30-frame sampler stride ≡ 0 mod 3 sampled the same dead slot forever). Fix (real, two
coordinated moves): bridge `rotate_resource_backings` rotates the DXVK image
**storages** (memory + VkImage + KMT handles — the defrag `assignStorage` machinery;
views follow storage generations natively; all shared images sit in GENERAL so the
swap is layout-safe) after a full event-query device sync, and the UMD rotates the
per-resource `{allocation, km_resource, owns_allocation}` records in lockstep.
Verified live: the runtime calls it every present and dwm's present `src` now cycles
through all three allocations.

### 4. Rect-limited Discards forwarded as FULL view discards → per-frame content wipe
dwm issues `Discard(view, rects=1)` on the incoming backbuffer (flip-discard contract:
invalidate only the newly-damaged region, then redraw it). `discard_11_1` dropped the
rects and forwarded a full `DiscardView` → DXVK reinitialized the whole image every
frame, wiping the undamaged 99% of the desktop. Discarding MORE than asked is
contract-illegal; upstream DXVK no-ops partial discards. Fix: `num_rects != 0` →
return early.

## ⚡⚡ THE REMAINING BLOCKER (sharply bounded now)

With all four fixes deployed (UMD hash `d3e791612d16e2ec`), **dwm's own presented
buffer still samples all-zero on dwm's own device**: the new registry-gated write-side
instrument (`HKLM\SOFTWARE\Helios!RotateSample` = N → the bridge rotation handler
staging-reads the just-presented buffer every Nth present on a fully-synced device,
logs `rotate-sample: ... nonzero=X/510` into `umd-<dwmpid>.log`) reads `0/510` on
every sampled frame. Meanwhile the SAME probe device renders and reads back every
reproducible shape correctly **on this same boot**: clears, non-indexed float draws,
indexed SINT-input CB-driven quads, textured+src-over-blended quads, cross-device,
cross-process (`tools/d3d11_xproc_draw_probe.cpp`), and even in **session 0 as SYSTEM**
(`HeliosDrawProbeSys` task — passes identically, so the session-0 theory is dead).

So the delta is inside dwm's actual composition stream, not in any independently
reproducible pipeline shape or environment. Facts to steer the next session:
- dwm's DDI stream around a frame: OMSetRenderTargets(1896x1030) → tiny delta
  DrawIndexed (a=6, big StartIndexLocation e.g. b=3948, one big shared index buffer) or
  DrawIndexedInstanced (instanceCount up to ~10) → flip Present → Rotate → partial
  Discard. Full-composition bursts (~67 draws) only at swapchain init.
- Draw forwarders pass all params (start index / base vertex / instances verified in
  source). Shaders create OK via the typed path (26 ok, 0 failed). SRV/sampler/scissor/
  viewport/CB1 (incl. FirstConstant offsets) all implemented and forwarded.
- dwm's compiled SPIR-V was captured (72 shaders, `Z:\tmp\dwmshaders\`, dumped via the
  new registry gate `HKLM\SOFTWARE\Helios!ShaderDumpPath` → the bridge _putenv_s's
  `DXVK_SHADER_DUMP_PATH` into every UMD process — currently DELETED/off). VS
  declarations look sane (typed inputs, Position builtin, CBs).
- Next candidate deltas, in order: (a) **DrawIndexedInstanced with per-instance vertex
  streams** (input layout slot class INSTANCE_DATA + InstanceDataStepRate — the probe
  never tested instancing; dwm's composition quads are instanced); (b) dwm's depth/
  stencil state (probe binds no DSV; dwm might enable depth-test against a bound DSV
  whose clear/state is broken → all fragments fail); (c) predication / occlusion
  queries gating draws (pfnSetPredication — CHECK if it's a silent stub! dwm uses
  predicated rendering!); (d) replay dwm's captured shader pair + state in the probe.
  **(c) is the sharpest suspect: a silently-succeeding SetPredication stub with a
  never-satisfied predicate would no-op ALL draws while every probe passes.**

## State / deploy
- Deployed: UMD `d3e791612d16e2ec` (all four fixes + rotate-sample instrument), KMD
  22.22.42.0 (unchanged), LGIdd 16.41.16.666 (unchanged), ICD unchanged.
- dxvk-helios: unchanged this session (a temporary pipeline-state log was added and
  removed; ecbd8f78 still HEAD).
- Registry gates: `HKLM\SOFTWARE\Helios!RotateSample`=16 (write-side sampler, cheap);
  `ShaderDumpPath` deleted (off). The rotation handler does a full device sync per
  present regardless — acceptable at bring-up cadence, revisit with C3.
- THIS BOOT IS CHURN-SATURATED (≈9 deploy cycles: repeated WUDFHost verifier kills,
  two dwm collateral crashes — dumps in C:\HeliosDumps, all in restart windows;
  `FAIL_FAST_FATAL_APP_EXIT_c0000409_ucrtbase!abort` and
  `BREAKPOINT_80000003_ucrtbase!common_exit` classes = the known deploy-window
  collateral, not new code faults). A clean cold boot is needed to validate the fixed
  stack end-to-end (guest reboot was auto-denied per the standing ask-first directive).
- The probe's final "propagate=PASS/FAIL" verdict line compares stale expected values
  after the added passes — read the raw hex lines, not the verdict.
- Ops gotchas added this session: schtasks /TR chokes on `&` (wrap commands in a .cmd);
  the shared `helios_icd_submit.log` was unwritable by session-0 processes until
  `icacls ... /grant Everyone:(M)` (that "dwm never submits" artifact cost hours);
  `win_cargo` truncates the head of its output — get rust errors via
  `cargo check --message-format=short` through win_exec.
- Also confirmed this session: the shared-content probe PASSES on NVIDIA (the pending
  re-verify — clears/copies/draws propagate cross-device and cross-process).
