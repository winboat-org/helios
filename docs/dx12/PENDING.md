# PENDING.md — what is left before FL 12_1 and before a real D3D12 app runs

**Written 2026-08-06** from four independent read-only inventories (DDI surface + caps, present/swapchain,
sync/queues/residency, resources/descriptors/state), each `file:line`-cited, cross-checked, and with the
load-bearing claims re-verified by the integrator. Organised as **`METHOD.md` Phase 1 subsystems**, because
that is now the unit of work.

⚠ **This is a gap list, not a plan.** Sizes are S/M/L with the reason. Nothing here is scheduled.

---

## ⭐ STATUS 2026-08-07 02:12 (DEPLOYED) — THE D3D12 SWAPCHAIN WORKS

**This block supersedes the earlier 2026-08-07 swapchain-blocker status below.**

The deployed D3D12 UMD (`SHA256 3DF16C82CD924679F1E4CE1D29C15DA1BEA278DC7B161320A0BE8F893808B64E`)
passes both acceptance rungs on the Helios adapter:

* `spy_workload share --adapter 0`: committed shared render target creation and
  `CreateSharedHandle(resource)` both return `S_OK`; the shared-fence and non-shared-resource
  controls also return `S_OK`.
* `spy_workload window --adapter 0 --frames 900`: swapchain creation, both `GetBuffer` calls and
  all **900/900** Presents succeed; the task exits 0. The per-process log records
  `PresentEntered=900`, `PresentIdentitySubmitted=900`, `EclWddmSubmitted=900`,
  `EclSubmitRenderFailed=0`, and every `PresentIdentity*` refusal counter is 0.
* `helios_paintcap_hidden` captured the unobscured probe window filled with the expected
  `(0.125, 0.375, 0.75)` medium-blue clear colour. The Looking Glass IDD was disabled and was
  not used for this acceptance; the native Helios desktop path produced the frame.

The repair is contract-defined rather than description-matched: every committed resource gets
its WDDM allocation identity at create time, because this DDI exposes no later shared-resource
declaration and the measured runtime does not query an allocation handle before sharing. The
identity table is now a fallible dynamic two-index registry, so ordinary applications do not
fail on a fixed 65th resource. The vkd3d fork gives committed buffers the same dedicated Venus
OPAQUE_FD export chain images already had. Finally, the HEPR `pfnRenderCb` is metadata-only and
submits `NumAllocations=0`; the actual present source remains in
`D3D12DDI_PRESENT_0051::BroadcastSrcAllocation`, while the D3D12 runtime owns residency. This
last distinction was measured directly: the first HEPR call with a redundant legacy allocation
list was rejected by dxgkrnl with `E_FAIL`, while the zero-list form completed all 900 frames.

Time Spy and Port Royal remain subsequent capability/compatibility goals; they are no longer
blocked on basic HWND swapchain creation or presentation.

---

## ⛔⛔ STATUS 2026-08-07 (DEPLOYED) — IT RUNS, AND THE SWAPCHAIN BLOCKER IS ROOT-CAUSED

**Read this block first. It supersedes every block below it, including the compile block.**

`22.22.256.0` is installed (`CM_PROB_NONE`), both UMDs are registered
(`UserModeDriverName[3]` → `helios_umd12`), `DiagLevel = 1` and `UmdD3D12 = 1` are set, and the
desktop still composites on Helios after all of it (paintcap 01:24). **Nothing crashed** — no
BSOD, no dead DWM, across ~6 D3D12 process lifetimes.

### ⭐ How far a real D3D12 app gets, measured

`spy_workload window --adapter 0`: `D3D12CreateDevice` **OK** → command queue **OK** →
allocator + command list **OK** → graphics **and** compute PSOs from DXIL SM 6.0 **OK** → root
signature **OK** → fences **OK** → `CheckFormatSupport` × 97 / `CheckMultisampleQualityLevels` ×
2730 answered → back buffers created and made resident. `slots = 0/206` noop hits.
⇒ **`CreateSwapChainForHwnd` → `E_INVALIDARG` is the single thing between this driver and a
D3D12 pixel.** Everything upstream of it works.

### ⛔⛔ THE BLOCKER, ROOT-CAUSED AT THREE INDEPENDENT LEVELS

**`helios_umd12.dll` mints no WDDM allocation for any resource, so the runtime has no kernel
object to share, so a flip-model swapchain cannot be created.**

| level | evidence |
|---|---|
| app | `CreateSharedHandle(resource)` → `E_INVALIDARG` on Helios, **`S_OK` on WARP**. On the same device `CreateSharedHandle(fence)` → **`S_OK`** and the same descriptor without `HEAP_FLAG_SHARED` creates fine ⇒ the failing object is the **resource**, not the fence and not the descriptor |
| runtime | `Microsoft-Windows-Direct3D12` journal entry `Message="ShareObjects" Code=0xC000000D` |
| kernel | dxgkrnl AzureTriage `"Input object handle is NULL. Returning 0xC000000D"` (`STATUS_INVALID_PARAMETER`), 44 µs earlier on the same thread |
| driver | `AllocateCbMissing/Failed/NoHandle` **all 0** and `IdentityRecorded = 0` — `pfnAllocateCb` was never *called*, not refused |

⛔ **The DXGI InfoQueue is EMPTY for this failure** (0 stored messages with
`DXGI_CREATE_FACTORY_DEBUG`). That is the same signature ROADMAP's FL11 entry records — the
reason exists only in ETW. Do not conclude "no debug message ⇒ no reason".

### ⛔⛔ THE ADMISSION PREDICATE IS AIMED AT THE WRONG CHANNEL — this doc's open question, answered

This file asked which channel `HEAP_FLAG_PRIMARY` arrives through, `HeapPrimaryVenusExport` or
`ResourceOptimizationPrimary`, and said *"both readings are one run away and neither exists"*.
**Both readings now exist and both are 0** — as are `HeapPrimaryFlagDropped` and
`HeapPrimaryWithoutResource`. ⇒ **NEITHER. `PRIMARY` never arrives at all**, so `adopt_presentable`
— the driver's only `pfnAllocateCb` call site — is unreachable on the swapchain path.

⛔ And the obvious repair, "admit on the SHARED heap flag instead", is **already refuted**:
`HeapFlagUnrepresentable = 0` on a run whose app passed `D3D12_HEAP_FLAG_SHARED` explicitly, and
`heap_flags` maps exactly four DDI bits. **There is no SHARED bit at this DDI** — the runtime
never tells the driver a resource will be shared. ⇒ the allocation cannot be minted from a flag;
the driver must give *shareable* resources a kernel allocation on some other basis. That is a
design question, not a patch, and it is the next real piece of work.

### ⚠ Also seen, not the blocker

* dxgkrnl logged **`"Driver returned an invalid NTSTATUS code: 0xC00000BB"` (`STATUS_NOT_SUPPORTED`)
  ten times in 3 ms** during D3D12 device bring-up. This is ROADMAP open defect #5, previously
  unattributed; it now has a **reproducer on the D3D12 path**.
* `D12Rec = D12Exact = D12Clr = 0` and did not move. **With `DiagLevel = 1` that zero is now
  trustworthy** — the app never reached `pfnExecuteCommandLists`, so no `pfnRenderCb` was due.
  It is not evidence about the KMD bridge, which remains untested end to end.
* `Umd12Trace = 1` is **left ON** on the VM. It is per-op chatter; a perf reading taken without
  clearing it is invalid.

### ⚠ Traps banked this session

* **`DiagLevel` is a SERVICE-key knob** — `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`
  (`diag::read_config_dword` uses `RTL_REGISTRY_SERVICES`). The handoff said
  `Control\Video\{adapter}\0000`; writing it there does nothing.
* ⛔ **`C:\Users\Rupansh\d12g5` contains a `d3d10warp.dll` SPY PROXY.** Any `--warp` arm run with
  that as the working directory loads the proxy, not WARP, and fails `D3D12CreateDevice` with
  `DXGI_ERROR_UNSUPPORTED`. It cost one wrong control-arm reading. Run WARP controls from
  `C:\Users\Rupansh\d12share`.

---

## STATUS 2026-08-07 — THE CHANGESET COMPILES, AND ROUND 1 OF PHASE 2 IS DONE (10 lenses, 7 skeptics)

**Superseded in part by the block above.** It supersedes everything below it where they disagree.

### ⭐ All five components COMPILE and LINK — the first compiler ever to see ~55 commits

`umd-check.ps1 -Mode check/-Mode release -Crate both` → **0 errors**; `umd`'s warning count
**15**, exactly baseline (the crate split held); **no `… is STALE …`** from `umd12/build.rs`;
`dumpbin /IMPORTS` → **no `dxgi.dll`**, link set still `libhelios_d3d12_static.a` + `gdi32`;
`kmd-check.ps1 -Mode build` → 0 errors, 11 pre-existing dead-code warnings; `win_build_kmd`
packaged + signed with `inf2cat`/`infverif` clean; `win_vkd3d` and `win_meson` clean; slot
coverage **205/206**. ⇒ **no compile error revealed a wrong assumption**, so nothing was fed
back as a new lens under `METHOD.md` §2 Phase 4.

⛔ **The KMD was deliberately NOT installed.** `METHOD.md` §2 puts deploy in Phase 4, after
saturation; installing at Phase 1 is the rejected loop with a bigger blast radius.

### The round-1 result: ~41 raw findings → **3 refuted, 5 survived (all narrowed)**

⛔⛔ **THE METHODOLOGICAL FINDING, and it is the most important line in this block: FOUR
LENSES CONVERGED ON A DEFECT THAT DOES NOT EXIST.** ABI-tables, wire-vocabulary, cross-lane
and handles all reported that `umd12`'s `pfnPresent` sends the wrong record type (`'HEPR'`,
"declared for the VidPn primary only") and poisons the KMD's 8-slot frame-watermark LRU.
Both halves are **REFUTED**:
* the "primary only" domain is **stale prose** that calls the alternative record a *"legacy
  four-byte marker"* when it is 16/32 bytes, and the **D3D11 writer has emitted `'HEPR'` with
  a real non-zero `resource_id` for ordinary windowed presents since 2026-08-04**
  (`apply_snapshot_override` overwrites the private record unconditionally, by design);
* the proposed repair (`'HERF'`) is **actively harmful** — it has no `resource_id` field, and
  `resource_id == 0` *escalates* to `QueueImmediate`, firing a generic dirty edge at DWM's
  bound primary at app frame rate;
* the LRU half fails on occupancy (**5 live ids against 8 slots**, and eviction picks
  least-recently-*presented*, so a stopped buffer cannot pin a slot) and on instrumentation
  (`BIND_REFRESH_SAMPLED`/`BeSmp` names the symptom already).

⇒ **three of those four lenses read the same stale comment and inherited its claim.** Their
independence was illusory. **Convergence measures salience, not correctness** — it is not
evidence, and this doc set's own anti-pattern *"believing a doc claim because it is written
down"* is what it actually demonstrated, at scale.

### Refuted outright — do not re-open without new evidence

| claim | why it died |
|---|---|
| `pfnPresent` sends the wrong record / poisons the watermark LRU | above |
| `WddmHeadMs` is inert on the wire arm ⇒ head-of-line stall + 256-entry early completion | **backwards**: with `Umd12EclFence=0` the same packet falls to `Prefix`+`IncludingGpu`, which waits on *every* in-flight fence on *every* ring from *every* process — a strict **superset**. `Exact` REDUCES head-of-line blocking; it is the A4 repair, misread as the injury. The overflow half misquoted the comment's scope (*"a boundary that **may be unsatisfiable**"* = the tagged stream namespace, not a range-checked KMD-issued wire fence), and `WfBWire` already owns that residual |
| the `kobj.rs` vsync heartbeat wedge | two further lenses independently rejected it: `DisplayHalf` defaults to 1 and `vsync_armed` is 1 while started, so the shipping configuration is covered |

### Survived, narrowed — and two skeptics produced better findings than the claims they killed

1. ⛔⛔ **FIXED (`9d511b8`) — a legal wait-before-signal was REMOVING THE `ID3D12Device`.** The
   race the finding reported is real but LOW (it only decides anything on a fence's *first*
   signal). Refuting it exposed the blocker underneath, which needs **no race at all**:
   `queueB->Wait(f,N)` before `queueA->Signal(f,N)` — legal D3D12, named verbatim in
   `fence.rs`'s own module doc — reads `watermark < N && count > 0` **deterministically**, and
   that was the arm that reported `E_FAIL` through `pfnSetErrorCb`. **Time Spy's async-compute
   subtest is exactly that shape.** Now a counted, logged drop; the gap stays §S-2.
   ⚠ Both proposed fixes for the race were also refuted — no reader-side change closes a
   writer-side tear. Fixed by publishing watermark+count as **one** biased `AtomicU64`.
2. **A3 is NOT fixed as a class, and this document said "Landed".** `dev_mutex` is genuinely
   off the new path — but `umd12` now holds **vkd3d's queue mutex** across the same escape, on
   the shipping default arm, and `helios_ioctl_submit_cs` still contracts *"caller holds
   dev_mutex"* and issues that escape. ⚠ The "5 s past TdrDelay" framing is wrong in **both**
   directions: the bound is really ~80 s (`KeDelayExecutionThread` rounds to ~15.6 ms
   granularity), and the realistic cost is **one timer tick, ~15.6 ms**, self-draining, on ONE
   queue — **not** a TDR (the escape is `HardwareAccess=0` and no DMA packet is outstanding
   yet). ⇒ **latency/jitter, size S.** ⭐ The trigger is settleable with no new probe:
   `QUEUE_FULL_RETRIES` already exists as a registry counter.
3. ⛔ **FIXED (`91e6906`) — a Render that does not WRITE an `'HD12'` record must CLEAR one.**
   The unconditional write and the consuming `decode` both exist for the recycled-buffer
   hazard; **UP-9 added a second `pfnRenderCb` on the same context that writes nothing at
   offset 0**, so a stale record could only be cleared by a submitter that no longer always
   runs. Skeptic's argument for not refuting: *you cannot hold that hazard load-bearing and
   simultaneously call this unreachable.* Reachability is confined to in-place TDR/abandonment
   (preemption cannot — `decode` already zeroed the magic; a device restart cannot —
   `ForeignGeneration` rejects it). New counter **`D12Clr`**.
4. **The `d3d12` bit's "identity, never a boundary" is false** — it selects `Kind::Exact` over
   `Kind::Prefix`. Code is RIGHT, comments were wrong; fixed `81a06c4`.

### ⛔⛔ THE 19 NEW KMD COUNTERS ARE NOT READABLE AT PROBE TIMESCALES — and this changes the next run

`record_present_handoff_telemetry` is their **sole** writer; it publishes only from the
scanout pacing tick, at `DiagLevel = 0`, once per **600 queued refreshes** — **≥10 s per
mirror write**, and unbounded while DWM is idle. No new counter is a `CounterEntry
{ failure: true }`, so nothing flushes on change. Only `RngSub`/`RngCmp` have a second reader
(words 33/34 of the `'HDBG'` TDR report — i.e. provoke a TDR and decode a minidump).

⛔ **This document's own "shortest path to a pixel" is therefore wrong as written.** It names
`tools/d3d12_spy/spy_workload.cpp` and claims *"every counter this lane added becomes readable
at once"*. Its defaults are **`mode = "device"` — no window at all — and `frames = 5`,
`hold = 0`**: a sub-second, windowless run that would produce neither a pixel nor a moved
counter, and the zero would read as *"the D3D12 path never reached the KMD"*.
⇒ **run `spy_workload window --frames 3600`** (~60 s vsync-locked) and take the post-snapshot
after the desktop has composited ~600 more frames; **or** set `DiagLevel = 1` and **reboot**
(`restart-device` cannot enable it — `DIAG_LEVEL` is cached once per image load).
⚠ Corrections to that finding: `ClkCal`/`ClkNoGpu`/`ClkFreq` are boot-cumulative and are NOT
hidden by the throttle; `kmd-counter-snapshot.ps1` DOES dump all 19 to file (only its printed
`-Watch` subset omits them); a multi-minute Fire Strike/Time Spy run IS readable.

### Instrument gradings — ✅ ALL REPAIRED LATER THE SAME DAY (`a1c7f38`)

⛔ **The list below is the FINDING, kept as the record; it is no longer the state.** Read it
for what was wrong and why, not for what to do. Two of its own claims expired within hours and
are struck here rather than edited away: `RENDER_COUNT`'s two re-commitments in `KMD_IMPACT.md`
are **removed** (replaced by `D12Rec`), and `DxgkDdiSetStablePowerState` **is** counted now
(`StblPwr`/`StblPwrEn`, plus `HistBuf` for its sibling).
⭐ The repair also found the decisive error in `D12Exact`'s identity that the review missed — a
**unit mismatch**: the `D12*` family counts `DxgkDdiRender` CALLS, `D12Exact` counts
`DxgkDdiSubmitCommand` PACKETS, and dxgkrnl batches Renders into one DMA buffer, so no
expression over the first set can ever equal the second. The identity is retired in favour of a
sound bound, `D12Exact <= D12Rec - D12Zero`, which every omitted term slackens in the same
direction. And `WfBReb` is now diagnosable rather than merely re-graded: `WfBRebS`/`WfBRebB`
partition it by the arm unsatisfied **at expiry**.

### The findings, as found (historical)

`EscSub == 0` and `EscSubRing == 0` are **unsatisfiable readings** — DWM's own ICD makes both
non-zero on an idle desktop, so the branch written as "the informative one" can never be
taken · `D12Exact`'s identity subtracts two **adapter-global** counters (`GpuFncClamp`,
`GpuFncGen`) from a D3D12-scoped one, omits `D12Merged`, and ignores replay fallbacks — and
now also `D12Clr` · `GpuFncClamp`/`GpuFncGen`/`FncIdGen` are attributed to "the UMD" but share
a rejection site with the D3D11 present BLT marker, and none is reset at `StartDevice` ·
`WfBReb`'s "whichever of `WfBStrm`/`WfBBlt` moved with it" cannot discriminate, since both
climb continuously under DWM · `RENDER_COUNT` is re-committed **twice** in `KMD_IMPACT.md` as
what K-F1 settles, 30 lines from where the same changeset strikes it (and it is adapter-global,
16-bit-truncated in its only ring reader, and has no reader on a default deployment) ·
`WddmHoldMs`'s UV1 table grades a flat reading **UV1 ✗** with no precondition that `WfBHold`
ever moved, and the hold is snapshotted at `StartDevice` so setting the knob without a device
restart leaves it 0.

### Contract gaps — ✅ ALL THREE CLOSED LATER THE SAME DAY (`0e188a9`, `a1c7f38`)

The three caps are **raised** — and the raise gained a safety argument the review did not have:
`d3d12_device_validate_shader_meta` fails PSO creation against **vkd3d's own** copy of the same
facts, so raising them cannot admit an unbacked shader, only stop the runtime refusing a backed
one. ⛔ Re-derived: all three are FL **12_2** floors, not 12_1, so no level moved and
`TiledResourcesTier` is still the only open 12_1 floor.
`DxgkDdiSetStablePowerState` keeps its no-op (there are no guest clocks to lock — the defect was
that a no-op was indistinguishable from a working one) and is now counted.
⛔ `BackBufferMultiplicity` got **no counter, deliberately**: it is a field of the `_Out_`
struct and the twelve-member IN struct has no multiplicity member, so the runtime cannot request
one and the counter could never move — a permanent zero reading as "nobody asked" is the same
trusting-a-zero pattern relocated from a comment into an instrument. Both comments corrected.

### The gaps, as found (historical)

Three caps are left at absent values with **no argument, no counter, and no gap-list entry**,
in the commit whose purpose was to raise exactly this class:
`VPAndRTArrayIndexFromAnyShaderFeedingRasterizerSupportedWithoutGSEmulation`, `WaveOps`,
`Int64Ops` — all three measured **1** on this guest (`baselines/d3d12-caps.csv`), all three
slot-free (vkd3d derives them from Vulkan features / DXIL lowering), and the FALSE answer
costs a **geometry-shader emulation pass per cubemap face and shadow cascade**.
⚠ `caps12.rs` already fills `WaveLaneCountMin/Max/TotalLaneCount` with substrate values while
reporting `WaveOps = FALSE`.
Also: `DxgkDdiSetStablePowerState` is an **empty body** returning void — it fabricates success
with no counter, the same shape `DxgkDdiCalibrateGpuClock` was just fixed for, and it is
reachable today (`SetStablePowerState(TRUE)` is the first call of every timing harness) ·
`pfnPresent` writes `BackBufferMultiplicity` and **twice** cites an instrument for it that
does not exist anywhere in `umd12`.

### Citation drift is systemic, not incidental

Every `gpu/mod.rs` citation in `KMD_IMPACT.md` §14a has drifted **250–600 lines** (the file
grew 983 lines) · `ROADMAP.md` WS4's five `gpu/mod.rs` pointers are all stale · this
document's own §1 A2/A3 evidence columns now point at **code the same changeset added** (the
submodules were bumped and the citations into them were not re-derived) · §6 has three bullets
the changeset itself paid · `KMD_IMPACT.md` UP-4 contains a **citation-drift correction that
has itself drifted** · `CLAUDE.md`'s vkd3d entry went stale **a fourth time**, inside this
changeset (fixed: it no longer names a count, and it now names the hand-mirrored
`VKD3D_HEAP_FLAG_HELIOS_VENUS_EXPORT` bit as the highest-risk divergence).
⭐ `caps12.rs`'s "symbols, not lines" rule works — the six citations that drifted (+118, two of
them the sole evidence for a cap decision) were into `misc.rs`, the one file not on its list.

### ⚠ Round 1 is NOT saturation

`METHOD.md` §3 needs **two consecutive dry rounds with different lens compositions**, a
completeness critic returning nothing, and every grading re-checked at the END of the merge.
Round 1 was not dry. Round 2 must rotate at least two lenses — and the four-lens false
convergence above says one of the new ones should be **"which of these findings share a
source?"**

---

## ⛔⛔ STATUS 2026-08-06 (later) — THE INTEGRATION LANE: §S-3 IS COMPLETE IN CODE, AND THE THING IN THE WAY IS A PROBE

Read this block before the wave-1 block below it. It closes `§S-3`'s items 2–6 and the fence
bridge's unlanded half, and it replaces three of the wave's *"what stands in the way"* items with
verified answers.

**Landed.** `pfnPresent`'s missing `hContext` (`queue::present_context`) · **UP-9**, the identity
`pfnRenderCb` carrying `HeliosPresentRenderCmd`, with the mandatory allocation list
(`PresentDependencies`, source `Value = 0`) · `pfnGetDebugAllocationInfo`'s four fields ·
`bridge12::sample_queue_fence` **plus its call site**, so the fence bridge stops shipping inert ·
`identity12::pitch` · the `bind_flags` wire-vocabulary fix below.

### ⭐ Three of the four items believed to be in the way, RE-DERIVED

1. ✅ **The hand-declared 22-slot interop vtbl is VERIFIED, not unverified.** Its order was compared
   slot-for-slot against the fork's *shipped* `d3d12_dxvk_interop_device_vtbl`
   (`libs/vkd3d/device_vkd3d_ext.c`) — **22/22 identical**, three IUnknown plus nineteen. The IID
   matches the IDL exactly (`9c0850e7-70f1-4229-ae05-440b387ec517`), `device.c:4677` has the
   QueryInterface arm, and `GetVulkanResourceMemoryInfo`'s five-parameter signature matches the IDL
   term for term. ⇒ *"a wrong slot is a wild call"* is retired as a risk; it is a checked property.
2. ⛔⛔ **DWM's `pfnOpenResource` was NOT free — `HeliosWddmAllocMeta::bind_flags` had no declared
   vocabulary and `umd12` wrote the other enum into it.** `adopt_presentable` wrote a
   `D3D12DDI_RESOURCE_FLAGS_0003` word; the reader is `api_bind_flags`
   (`umd/src/forward/state.rs:1211`) → `desc.BindFlags` in `umd/bridge/dxvk_bridge.cpp`'s open path →
   the `VkImageUsageFlags` DXVK aliases with. A back buffer's `RENDER_TARGET | SHADER_RESOURCE`
   (`0x11` in the D3D12 DDI) reads as **`VERTEX_BUFFER | STREAM_OUTPUT`** in the D3D11 DDI, so DWM
   would import an image with no render-target and no shader-resource usage and could not sample the
   frame. Fixed: `protocol/` now declares the vocabulary (`HELIOS_WDDM_BIND_*`) and `umd12`
   translates. ⇒ **this was a silent wrong-picture defect on the critical path, and it was found by
   reading the reader.**
3. ⛔⛔ **The admission-predicate prior is UNINFORMATIVE, and the framing that it favours
   `ResourceOptimizationPrimary` does not survive.** The quoted evidence —
   `HeapPrimaryFlagDropped` 0 in all 150 runs vs `ResourceOptimizationFlagsIgnored` 1..3 in 101 of
   150 — cannot discriminate, for two independent reasons: **(a) not one logged run created a
   swapchain** (`grep -c SwapChain` over all five `tools/d3d12_*probe*.cpp` is **0**), so
   `D3D12DDI_HEAP_FLAG_PRIMARY` could not have arrived through *either* channel; and **(b)** the
   optimisation aggregate absorbs `SHADER_RESOURCE`/`UNORDERED_ACCESS`/`DETERMINISTIC`, which any
   ordinary texture create sets, so 1..3 is not a PRIMARY signal at all. The counter that *can*
   discriminate, `ResourceOptimizationPrimary`, was added in the wave and has never been read.
   ⇒ **do not move the predicate; run a swapchain and read `HeapPrimaryVenusExport` against
   `ResourceOptimizationPrimary`.** Both readings are one run away and neither exists.
4. **The fork's venus-export arm still has never executed.** Read and coherent — `resource.c:4444`
   chains `VkExportMemoryAllocateInfo` with `OPAQUE_FD` and forces
   `prefersDedicatedAllocation`, and `:752` declares the matching
   `VkExternalMemoryImageCreateInfo` on the image (VUID-VkMemoryAllocateInfo-pNext-00639) — but
   nothing has run it. Unchanged.

### ⭐ The shortest path to a pixel: an artefact that already exists

⛔ **No D3D12 tool in this tree has ever called `pfnPresent`.** `tools/d3d12_clear_probe.cpp` and the
other four `d3d12_*` probes contain **zero** occurrences of `SwapChain`. That, not the driver, is
what has kept every present-path counter at 0.

⭐ **`tools/d3d12_spy/spy_workload.cpp` is already the vehicle and needs no new code**: it creates a
real window, a flip-model `CreateSwapChainForHwnd` on an `ID3D12CommandQueue`, and calls
`Present(1, 0)` per frame — that is what produced `D12-G5`'s 20 measured presents against WARP. It
takes **`--adapter <n>`** as well as `--warp`, and it creates its device at `D3D_FEATURE_LEVEL_11_0`,
which Helios reports. ⇒ point it at the Helios adapter, from a **scheduled task** (session 0 fakes a
regression, `GATES.md` §1), and every counter this lane added becomes readable at once.

### ⚠ Debt recorded and deliberately NOT paid: D13's `RuntimeAllocPrivate` / `AdoptedAllocPrivate`

The `{HeliosWddmAllocPrivate, HeliosWddmAllocMeta}` pair is declared twice —
`umd/src/forward/state.rs`'s `RuntimeAllocPrivate` and `umd12/src/forward12/resource12.rs`'s
`AdoptedAllocPrivate` — and D13 says the pair belongs in `protocol/`. **Not moved, and the reason is
verifiability rather than ownership:** `umd/build.rs` hard-codes Windows MSVC/WDK paths
(`C:\Program Files\...`), so `umd` **cannot be type-checked on this host at all**, and one
declaration requires editing it. Moving the declaration to `protocol/` while leaving `umd`'s copy
would give the tree *two* authoritative names instead of one, i.e. it would not discharge the
directive. ⇒ the remaining move is exactly two lines in `umd/src/forward/state.rs` (delete the
struct, `pub(crate) use helios_protocol::… as RuntimeAllocPrivate;`) and belongs in a session that
can build `umd`. The `const _` asserts in both crates pin the 48 + 48 layout meanwhile, so the
duplicate cannot drift without failing to compile.

---

## ⭐ STATUS 2026-08-06 — WAVE 1 LANDED, and five of this document's own claims were wrong

A four-lane `METHOD.md` Phase 1 wave landed after this document was written. **Read this block before
any row below it.** Everything landed is still **`implemented-but-never-exercised`** — `kmd_render`
does not typecheck on Linux at all (`bytemuck_derive` proc-macro for the linux target, *not* only
bindgen), so no compiler has seen the KMD half; the mitigation was to extract every decision table
into `kmd_logic`, which went **149 → 172 tests**.

**Landed:** §1 **A2** (deleted, see below) · **A3** · **A4** exact D3D12 boundary + `D12Exact` ·
**A5** `WddmHeadMs` head bound + `WfBReb` + a lock-free armed-deadline shadow · **A6** two-sided
generation bound + `GpuFncGen` · §2 **S-1** the GPU clock now answers `GpuFrequency = 1e9` and a real
`CpuClockCounter` · **S-2** dropped waits split by provenance · **S-4** `ExecuteIndirect` for the
four native classes, state-templates refused **at create** · §3 **four of the five FL 12_1 floors**.
Also: the five unrunnable `kmd_render` tests recovered into `kmd_logic`.

### ⛔ Five corrections to this document

1. ⛔⛔ **§1 A1's stated trigger is REFUTED.** `queue->Wait(f,N); ECL(); …Signal(N)` **cannot** hang:
   `FenceState::signal_reachable` is `value <= signalled_watermark` (`fence.rs:331-333`), so a wait
   for a value this driver never signalled is **dropped, not forwarded** — no
   `VKD3D_SUBMISSION_WAIT` is ever enqueued, and the drain cannot sit behind one.
   `d3d12_command_queue_Wait` is vkd3d's only producer of that type, so this is exhaustive.
   **What survives:** the drain *is* an untimed, unbounded `pthread_cond_wait` inside a DDI; a
   *permanent* hang additionally needs a cycle among queue workers, unconstructible while the
   watermark gate stands.
   ⭐ **And the refutation produced a better finding than the claim: the watermark gate is
   LOAD-BEARING for the drain.** Forwarding waits above the watermark — the obvious fix for
   `FenceWaitNotForwarded` — destroys that acyclicity and makes the hang genuinely reachable. ⇒ that
   change and a bounded/WAIT-skipping acquire **must land together**.
2. ⛔ **A1's containment costs more than this document said.** *"Default the drain OFF"* does not
   shrink the boundary — it **removes** it. The wire fence is sampled from the `VkQueue` that only
   `vkd3d_acquire_vk_queue` returns, *inside* the drain function, so with the drain off
   `gpu_wire_fence = 0` always and the whole fence bridge is inert. ⇒ a **sample-only** bridge path
   (vkd3d's existing `vkd3d_lock_vk_queue`, queue mutex, no DRAIN) is required for the containment to
   mean anything. Its honest cost: the boundary may name **less** work than the frame contains.
3. ⛔ **A2 is not repairable and was deleted.** The ring tail is **ring-global**, so *"advanced only
   by the release's empty batch"* cannot be distinguished from *"advanced by another `VkQueue`'s real
   work on the same primary ring"* — a tolerant key would return an **under-ordering** fence, the one
   hazard the export exists to prevent. A sound key is a per-`VkQueue` submission count, which
   `struct vn_queue` does not have. ⭐ Deleting also removed a latent bug: `ring_idx` **is** recycled
   and nothing invalidated the cache slot, so a hit that ever started working would have handed back
   a fence minted on a destroyed queue's timeline.
4. ⛔ **S-1's *"no counter and no diag record"* is misleading: the diag record is FORBIDDEN, not
   omitted.** `DXGKDDI_CALIBRATEGPUCLOCK` is `_IRQL_requires_max_(DISPATCH_LEVEL)` and *"called on
   timer"*, so a registry write there is illegal. **Do not let anyone "fix" it by adding one.**
   Relatedly, `GpuClockCounter` stays **0 with a counter** by deliberate refusal: synthesising it
   from an interrupt-time source is the right *rate* with the wrong *epoch*, which makes every
   GPU↔CPU correlation silently wrong while every counter reads healthy.
5. ⛔ **§6's "six" unrunnable tests was FIVE.** A `grep -c` over-counted and this document inherited
   the number.

### ⛔ Three errors in the wave's own briefs, worth recording because two were nearly shipped

* **`WriteBufferImmediateQueueFlags = 15` is the wrong enum.** 15 is
  `D3D12_COMMAND_LIST_SUPPORT_FLAGS`; the DDI field is `D3D12DDI_COMMAND_QUEUE_FLAGS`
  (3D=1, COMPUTE=2, COPY=4, **PAGING=8**), so 15 would have **advertised a paging queue**. Correct
  value **7**. Same class as the `3DPIPELINESUPPORT` bitmask-vs-level trap `caps12.rs` already warns
  about. And BUNDLE has no DDI queue flag at all, which is why the BUNDLE refusal has nothing to
  withhold.
* **"Forward the engine's answer, do not pin" is not available at `pfnGetCaps`** — it is
  *adapter*-scoped with no `ID3D12Device` in scope, so every number in `d3d12_options` is necessarily
  pinned. ⭐ But `SUBSTRATE.md` §6.3's heap-tier UNVERIFIED is **discharged**: the probe it asks for
  is the committed CSV, which reads 2, and a clamp can only lower.
* **`MaxSamplerDescriptorHeapSize` must stay 4000.** The substrate's real ceiling is 2048 and
  2049..=4000 samplers will fail `E_INVALIDARG` — but 2048 is what the runtime rejects as *too
  small*, which fails **device creation**. The repair is a named counter, not a lower cap.

### ⭐ Why four FL 12_1 floors moved with no implementation work

The doc and the code both said the level and its floors *"move together or not at all"*. **That
implication is one-way**: you must not raise the level without the floors, but you *may* report a
backed floor without raising the level. That misreading held three fully-backed caps at their absent
values with their slots already forwarding verbatim. **Only `TiledResourcesTier` (§3 S-6) now
remains, and the level is still not raised.**

---

## 0. The two things to understand before reading the list

### 0a. ⛔ "FL 12_1" and "real apps and benchmarks" are DIFFERENT targets, and the second is harder

**Night Raid and Time Spy require FL 11_0, which this driver already reports.** So the feature level is *not*
what stands between Helios and a benchmark score. And FL 12_1 is far cheaper than the doc set says: the
measured engine backs **all five** floors (`docs/dx12/baselines/d3d12-caps.csv` — binding tier 3, tiled tier 4,
typed-UAV-load 1, ROVs 1, conservative raster 3), and four of the five are **caps constants whose slots are
already real**. Only tiled/reserved resources needs bodies, and that is **S across five sites**, not the "long
pole" §4.4 implies.

⇒ **Two of the five were held back by stale reasons written in code comments** — claims that named a blocker
which no longer exists (`ROVs`, corrected in `63b8f1b`; `ResourceBindingTier`'s "L5 has not written a
descriptor handler yet", stale since all 15 descriptor slots landed).

### 0b. ⛔ Slot coverage is being read as capability, and the gap is enormous

`203/206 slots filled` is quoted as the state of P4. The counters say the **entire D3D12 evidence base is one
clear of one committed render target**: 1 RTV, 2 shaders, 2 PSOs, 1 root signature, 1 legacy barrier.

Never executed once: **8 of 15 descriptor slots** (SRV, UAV, DSV, CBV, Sampler, both `CopyDescriptors`,
SamplerFeedbackUAV — i.e. ~95 % of view translation, every cube/array/3D/MSAA/mip-subrange arm), the **whole
map/upload path** (`MapHeapCalls = 0`), **every query slot**, **every placed resource**, every tiled resource,
`pfnWriteBufferImmediate`, `pfnExecuteIndirect`, and `pfnCheckResourceAllocationHandle`.

⇒ This is `METHOD.md` saturation criterion 6 — *implemented but never exercised* — and it is the single
largest unsized quantity in the doc set.

---

## 1. ⛔⛔ MUST FIX BEFORE ANY DEPLOY — defects in code landed 2026-08-06

The fence bridge (`b32c584`, `f253baa`, `56483b2`, `2e8d3fe`, ICD `49acb236`) has **never run** and has six
known defects. Found by one adversarial reader after four agents wrote and cross-checked it. **Not one is
reachable by `tools/d3d12_clear_probe.cpp`**, which uses one queue and never calls `Wait`.

| # | Defect | Evidence | Size |
|---|---|---|---|
| **A1** | ⛔⛔ **HARD DEADLOCK, no TDR.** The drain (`vkd3d_acquire_vk_queue` → `d3d12_command_queue_acquire_serialized`, `command.c:25202-25217`) enqueues `VKD3D_SUBMISSION_DRAIN` and `pthread_cond_wait`s until the worker drains **everything ahead of it, FIFO**. A queued `VKD3D_SUBMISSION_WAIT` resolves through `d3d12_fence_block_until_pending_value_reaches_locked` → `pthread_cond_wait` with **no timeout** (`:1229`). ⇒ `queue->Wait(f,N); ExecuteCommandLists(...); …later… Signal(N)` — entirely legal — hangs the app's own thread inside the DDI, permanently, with no GPU packet outstanding for TDR to catch. **CONFIRMED from source.** | `command.c:25202-25217`, `:1229`, `:23725`, `:23848` | **M** fork fix / **XS** containment (default the drain OFF) |
| **A4** | ⛔ **The boundary is a PREFIX, not the frame's own fence — a standing invariant violated on the new arm.** `watermark = gpu_fence_id + 1` and `async_retired_up_to` is satisfied only when **no** in-flight venus entry has `fence_id < watermark`, every ring, every process — including DWM's ring-1 scanout copies. This is the exact superset the present path had to relax away (`PRESENT_EXACT_WATERMARK_USED`), and CLAUDE.md's table forbids it: *"a WDDM fence may wait on the frame's OWN boundary, never on the whole `next_wire_fence` backlog"*. **CONFIRMED.** | `gpu/mod.rs`'s `note_wddm_submission` + `async_retired_up_to` — ⚠ **the decision itself has MOVED OUT of `kmd_render`**: it is `helios_kmd_logic::wddm_boundary::select`, with `wddm_boundary_tests` as its oracle (was `:5028-5042`, `:5676-5684`) | **S** (exact test + its own counter) |
| **A5** | **`wddm_pending` overflow completes 256 packets EARLY**, while their host work is still running, and forces `release_all_scanout_leases(Teardown)`. A4 makes reaching 256 likelier than the code's "practically unreachable" note assumes. The fix is the consumer-side head deadline ROADMAP already names for K-F2 — now covering **two** writers. | `gpu/mod.rs`'s `MAX_WDDM_PENDING`, the `wddm_pending.len() >= MAX_WDDM_PENDING` arm of `note_wddm_submission` → `overflow_wddm_pending`, and `submit_command.rs`'s `note_and_maybe_signal` (was `:312`, `:5445-5459`, `:586-616` — all three drifted 250–600 lines) | **S** |
| **A2** | The 64-entry seqno cache **can never hit**: `vkd3d_release_vk_queue` issues a real `vkQueueSubmit2` *after* we sample, advancing the ring tail that is the cache key. ⛔ Its own doc describes behaviour it cannot deliver. | `command.c:25591-25612` (unchanged by the bump), `vn_ring.c`'s `vn_ring_current_seqno` — ⛔ **`vn_renderer_helios.c:2034-2043` NO LONGER EXISTS AS CITED**: the ICD submodule was bumped in this changeset and that range is now brand-new unrelated code the same changeset added (`helios_venus_queue_gpu_fence`'s decode/refuse block). ✅ **The cache was DELETED** (`6993dd4a`); what survives is the *bound*, `HELIOS_QUEUE_GPU_FENCE_RING_LIMIT = 64`, whose comment records the deletion and why the key could never match | **XS** doc / **S** repair |
| **A3** | `dev_mutex` — which serialises **every venus submit in the process** — is held across a kernel escape that retries up to **5 s** on `QueueFull`, and QueueFull is reachable at frame rate (64 descriptors, 3/chain ⇒ ~21 concurrent; 3 queues × 2 ECL × 200 fps is at the ceiling). | ⛔ **`vn_renderer_helios.c:2025-2069` is STALE IN THE MOST MISLEADING WAY**: the ICD submodule was bumped in this changeset and that range is now brand-new code *the same changeset added* — `helios_venus_queue_gpu_fence`'s decode/refuse block — so a reader re-deriving it lands on the very function written to AVOID this defect. The live evidence is `helios_ioctl_submit_cs`'s header (*"Caller MUST hold dev_mutex (ordering)"*) against `helios_submit_gpu_fence_cs`'s header, which enumerates item by item what `dev_mutex` actually protects and why the new path needs none of it. `ctrl.rs`'s `ENQUEUE_RETRY_MAX_MS = 5_000` (still `:100`) and `gpu/mod.rs`'s `CTRL_QUEUE_SIZE = 64` (still `:83`) are both current. ⛔⛔ **NOT FIXED AS A CLASS, and the wave-1 block above listing A3 as "Landed" OVERSTATES it**: `dev_mutex` is genuinely off the new gpu-fence path, but `helios_ioctl_submit_cs` still contracts *"caller holds dev_mutex"* and still issues the same 5 s-retry escape, and `umd12` now holds **vkd3d's queue mutex** across that escape on the shipping default arm. ⭐ The trigger is settleable with no new probe: `QUEUE_FULL_RETRIES` (`virtio/counters.rs`) already exists as a registry counter | **S** |
| **A6** | A fence sampled across a StopDevice/StartDevice cycle passes the clamp **uncounted** (`next_wire_fence` restarts at `BASE + instance·2^32`), names a fence the new instance never issued, and completes immediately — **the fence lies early**, and `GpuFncClamp` will not flag it. No owner check either. SUSPECTED. | `helios_kmd_logic::wddm_boundary::select`'s `Rejection::ForeignGeneration` arm (the clamp moved OUT of `gpu/mod.rs`), `gpu/mod.rs`'s `wire_fence_base` assignment in `init` and its `NEXT_WIRE_FENCE_BASE` / `WIRE_FENCE_INSTANCE_STRIDE` statics (was `:5680`, `:2214`, `:1377`) | **S** |

⭐ **Minimum before the first VM run:** default the drain OFF (A1), land A4's exact watermark, land A5's head
deadline. The deadlock is unreachable for the current probe and reachable for every real engine; the
head-of-line arm decides whether the desktop survives the first D3D12 frame.

---

## 2. BLOCKS a real app — by subsystem

### S-1 · GPU clock calibration — ⛔ **a benchmark reports zero regardless of everything else**
`dxgkddi_calibrate_gpu_clock` **zero-fills `DXGKARG_CALIBRATEGPUCLOCK` and returns `STATUS_SUCCESS`**, with no
counter and no diag record (`kmd_render/src/ddi/scheduler.rs:266-284`). ⇒ `GpuFrequency = 0`. There is no
`pfnGetTimestampFrequency` in the DDI header and no `KMTQAITYPE_*` for it, so this is the **only** channel.
The self-consistent answer is **1 000 000 000**: vkd3d computes `1e9 / timestampPeriod` (`command.c:23247`) and
this guest reports `timestampPeriod = 1` (`guest-vulkaninfo-full.txt:424`). ⛔ It is also the one DDI in the KMD
that **fabricates data, returns success, and has no counter** — a rule-2 violation on the exact path a score
depends on. **XS** to instrument, **S** to answer, **M** to derive honestly (an escape carrying the ICD's
`timestampPeriod`, plus real `GpuClockCounter`/`CpuClockCounter` via `VK_KHR_calibrated_timestamps`).

### S-2 · Cross-queue synchronization — ⛔ **wrong pixels, not slow pixels**
`FenceSignalForwarded = 0` in every readout, and `FenceSignalDelayed = 0` even in the delay arm — the DDI is
**not entered**. Our Vulkan work is submitted inside `pfnExecuteCommandLists`; dxgkrnl's monitored-fence wait
delays *DMA packet execution*, and this driver's DMA packets carry no GPU commands. ⇒ **a `Wait` enforced only
in the kernel orders nothing**, and compute/graphics/copy run concurrently with no dependency. The working
channel is the engine forward `fence_operation` already implements. **XS to settle** (add a COMPUTE queue to
`d3d12_clear_probe.cpp`, `Signal` on one and `Wait` on the other, read the two counters); **S–M to fix**.
Related: dropped GPU waits must become **loud** (`pfnSetErrorCb`), and `fence.rs:81-84`'s claim that the gap
*"closes when §10.4's `pfnWaitForSynchronizationObjectFromGpuCb` half lands"* is **refuted and forbidden**.

### S-3 · Present identity bridge — ✅ **items 1–6 LANDED; item 7 is fullscreen-only and still open**
⛔ **Read the integration-lane status block at the top first.** Items 1–6 are code as of
2026-08-06 and every one of them is `implemented-but-never-exercised`, because **no tool in this tree
has ever called `pfnPresent`** — that block names the artefact that would. ⚠ Two prescriptions below
were wrong and are corrected there: UP-9's *"one added parameter"* also required splitting
`EclWddmSubmitted` out of `submit_wddm_render` (a present sharing it breaks the documented
`EclForwarded == EclWddmSubmitted + EclNoWddmSubmission` invariant), and the *"believed free"*
`pfnOpenResource` half was a live wrong-picture defect in `HeliosWddmAllocMeta::bind_flags`.

Dependency-ordered; the first item is a **hard prerequisite**, not a parallel hazard:
1. **UP-3′ — venus-exportable memory. S (~15 lines C + ~10 Rust).** ⛔ Nothing in this driver can obtain a
   non-zero venus resid today: `helios_venus_memory_res_id` returns `mem->base_bo ? … : 0`, and only the
   **export** arm of `vn_device_memory_alloc` sets `base_bo` — so a plain `HEAP_TYPE_DEFAULT` committed texture
   has resid 0, which is exactly what makes the KMD *create* instead of *adopt*. One `VkExportMemoryAllocateInfo`
   chain under a **private bit** buys both properties (`memory.c:2051-2063`: any non-NULL `pNext` defeats
   suballocation ⇒ dedicated **and** `offset == 0` for free). ⛔ Not via `SHARED` — that reaches
   `vkGetMemoryWin32HandleKHR` on a **NULL PFN**.
2. **UP-2c — bridge accessors. S.** Hand-declare `ID3D12DXVKInteropDevice4` (GUID + 22-slot vtbl) in
   `umd12/bridge/vkd3d_bridge.cpp`, which deliberately includes no vkd3d headers; resolve
   `helios_venus_memory_res_id` / `_alloc_info` / `_instance_ctx_id` off the existing S4b anchor.
3. **UP-5 create — `pfnAllocateCb`. M.** ⭐ The `hResource` hazard is **one character**: the callback runs
   *inside* `pfnCreateHeapAndResource` where `_h_rt_resource` is a live parameter. Store the **output**
   `hAllocation`/`hKMResource` in `identity12`'s table, never in `ResourceState`.
4. **UP-5 destroy — `pfnDeallocateCb`. S.** Absent today ⇒ **one leaked WDDM allocation per back buffer per
   `ResizeBuffers`**, and `ResizeBuffers` fires on every window drag. The 54th session's leak class.
5. **UP-6 — `pfnCheckResourceAllocationHandle` returns the real handle. XS.** Re-grade both counters.
6. **UP-7/8/9 — `pfnPresent` + the size hook + the identity `pfnRenderCb`. M.** ⭐ `submit_wddm_render<T>` is
   already generic, so UP-9 is **S**, one added parameter.
7. **Fullscreen only: `HELIOS_WDDM_ALLOC_MISC_DIRECT_SCANOUT`. XS but blocking.** Without it the DMA flip is
   **hard-refused** `STATUS_INVALID_PARAMETER` + `PBFlip=0xE6`. ⚠ Set it only once the stride question is
   settled — the primary's stride is a **frozen agreement with the host** (`align(width*bpp, 256)`), not the
   image's, and whether the host honours `strides[0]` for an ordinary OPTIMAL vkd3d back buffer is unestablished.
   Setting the bit while the stride disagrees turns a hard failure into a **sheared picture**.

### S-4 · `ExecuteIndirect` — ⛔ app fails at init
`pfnCreateCommandSignature` → `E_NOTIMPL`; `pfnExecuteIndirect` → a **silent** counted noop. Meanwhile
`ExecuteIndirectTier = 1_0` is advertised **unconditionally**, because the DDI enum has no not-supported value.
Every engine with GPU-driven rendering calls `CreateCommandSignature` at startup. **S** for the native classes
(all 12 argument types are value-identical to the API's; the guest has `drawIndirectCount`/`multiDrawIndirect`).
⛔ **Classify and refuse the root-argument classes in the driver, loudly** — `VK_EXT_device_generated_commands`
is absent and vkd3d then **silently skips** (`command.c:17811-17818`), so a naive forward converts a loud
`E_NOTIMPL` into an empty scene **with a score**.

### S-5 · Binding and heap tiers — modern engines fail at startup
`ResourceBindingTier = 1` (engine: **3**) and `ResourceHeapTier = 1` (engine: **2**). Tier-1 binding imposes
per-table limits modern engines size past, and is an FL 12_0 floor string. Heap tier 1 forbids
`ALLOW_ALL_BUFFERS_AND_TEXTURES` — flag value 0, the default, what D3D12MemoryAllocator uses — so a mixed-category
heap allocator fails on placed resources. Both **XS–S**; a forwarding UMD has no per-tier code. ⚠ Forward the
engine's heap-tier answer rather than pinning a number (`SUBSTRATE.md:1055-1078` records it as substrate-dependent).

---

## 3. FL 12_1 itself — four constants and one subsystem

| floor | engine | driver | in the way |
|---|---|---|---|
| `TypedUAVLoadAdditionalFormats` | 1 | 0 | one const + one format mask |
| `ResourceBindingTier` | 3 | 1 | ⛔ nothing — the stated reason is **stale** |
| `ROVs` | 1 | 0 | ⛔ nothing — the stated reason was **false**, corrected `63b8f1b` |
| `ConservativeRasterizationTier` | 3 | 0 | ⛔ nothing — `pfnCreateRasterizerState` already forwards the mode verbatim |
| `TiledResourcesTier` | 4 | 0 | **S-6 below** |

**S-6 · Tiled/reserved resources. S across five sites** — the create arm (`E_NOTIMPL`), two tile-mapping slots
(VOID counted noops, so an app gets **no error**), `pfnCopyTiles`, `pfnGetMipPacking`, plus two caps
withholding sites. Near-pure forwards: the structs are field-identical and the flag enums value-identical.
⭐ **Verified: it is not a KMD dependency.** vkd3d derives the tier purely from Vulkan sparse features and all
ten preconditions hold on this guest; tile mapping is `vkQueueBindSparse`, ordered inside vkd3d by its own
timeline semaphore, imposing **no new WDDM ordering requirement**. The venus wire protocol carries the command
and the Mesa ICD implements it — the guest side is complete end to end.

Then the level itself, in **one commit with its floors** (the rule at `caps12.rs:246-249`).

---

## 4. DEGRADES — runs, wrong or slow

Root signatures down-converted to 1.0, losing **1.2 static-sampler flags** (correctness, **S**) · single-slice
array/cube views collapse to non-array dimensions ⇒ wrong `VkImageViewType` (**M**) · stream output dropped
*including* `RasterizedStream`, so a discard-only GS **rasterizes** (**S**) · pipeline libraries refused while
vkd3d ships a complete implementation ⇒ every PSO recompiled per launch (**L**) · `WriteBufferImmediateQueueFlags`,
`DepthBoundsTest`, `OutputMergerLogicOp`, `ViewInstancing`, `CopyQueueTimestampQueries` all reported unsupported
while backed (**XS** each) · `MaxSamplerDescriptorHeapSize = 4000` vs a possible engine ceiling of 2048 (**S**)
· `pfnClearRootArguments` mid-list `ClearState` (**S**) · per-fence completion unbatched at ~1800/s (**M**) ·
three WDDM contexts × 256+256 lists + 256 KiB each (**S**, knob-gated) · `QueryVideoMemoryInfo(NON_LOCAL).Budget`
reads small ⇒ engines trim, pop-in (**XS** to measure).

**Residency needs nothing.** D3D12 heaps are venus-ICD memory, never dxgkrnl segment commitments, so
`ApertureSegmentCommitLimit = 64 MiB` **cannot fail an allocation**; `MakeResident`/`Evict` return `S_OK`
honestly and `E_PENDING` is unreachable by construction.

**Command-allocator lifetime needs nothing from us** — retirement is vkd3d's own per-VkQueue timeline, genuinely
GPU-truthful, no `ID3D12Fence` involved. ⚠ But `pool_reset_engine_failed` **can never fire** (vkd3d returns
`S_OK` on the early-reset path), so umd12 is blind to it: re-grade that counter.

---

## 5. NOT REACHED — with the evidence

Scheduling groups (`ComputeQueuesPer3DQueue = 0`; a non-zero answer lands on the VidSch `0x119` bugcheck) ·
`pfnSetSamplePositions` (the runtime removes the device at tier NONE — the runtime is the gate) ·
`pfnAtomicCopyBufferRegion` (vkd3d stubs both forms) · `pfnOmSetAlphaBlendFactor` (RETIRED) · meta commands
(we publish zero, so no `CommandId` can legally arrive) · raytracing/mesh/VRS/sampler-feedback (**forced off by
the SM 6.0 list**, not chosen — one short list gates the whole L9 tail against a measured SM 6.8 substrate) ·
tearing (0 hits anywhere; 3DMark does not need it) · MPO (refused, MPO3 not registered) · stereo.

⚠ **Cross-lane, and it precedes all D3D12 work:** `VK_KHR_swapchain` is **conditional** on
`renderer_sync_fd.semaphore_importable` (`vn_physical_device.c:1334`), and its absence kills `D3D12CreateDevice`
outright. One `vulkaninfo` assertion, **XS**.

---

## 6. Doc and instrument corrections banked by these inventories

⚠ **Bullets marked ✅ PAID were discharged by the changeset itself and are kept, not deleted** — a
struck correction still tells the next reader which claim was wrong and where the wrong version may
survive in someone's notes. Everything unmarked is still open.

* `DDI_REFERENCE.md:3036-3041` still carries the tiled-resources reasoning `DX12.md` §4.4 **struck** — the
  third document to carry it after the correction reached four other sites.
* `caps12.rs`'s ROV claim — **false**, corrected `63b8f1b`. Its citation was also wrong.
* ✅ **PAID** — `caps12.rs`'s binding-tier reason was **stale** since all 15 descriptor slots
  landed. `ResourceBindingTier` is now `BINDING_TIER_MAX` (3), and the stale claim (*"L5 has not
  written a descriptor handler yet"*) is quoted at the read site as the thing being retired, with
  each of the 14 verbatim-forwarding slots enumerated and the ONE genuine refusal
  (`pfnCreateSamplerFeedbackUnorderedAccessView`) named as gated on `SamplerFeedbackTier`, not on
  the binding tier. ⚠ Was cited `:557-559`; symbol now — `d3d12_options`.
* `DX12.md` §4.4's tiled lane attribution names two tile slots; the header has **four**.
* ✅ **PAID** — `queue.rs`'s stated blocker for command signatures was **discharged**, and the row
  has since gone further: `pfnCreateCommandSignature` is *implemented*
  (`queue12`'s `create_command_signature` + the `ID3D12CommandSignature` handle slot), so the
  `E_NOTIMPL` §2 S-4 describes is gone for the native argument classes. ⚠ Was cited `:2118-2121`,
  which is now the `CreateCommandList` class-refusal arm — a re-deriving reader would land on an
  unrelated refusal and read it as confirmation. Symbol now.
* ✅ **PAID (2026-08-07)** — `SPECS.md:136` logged the timestamp-frequency question against
  `KMD_IMPACT.md`, which had **zero** occurrences of "timestamp" *or* "CalibrateGpuClock". It fell
  through, and it is S-1, the top benchmark blocker. ⛔⛔ **AND IT FELL THROUGH A SECOND TIME**: the
  changeset shipped the GPU clock — the largest KMD item in it — and `KMD_IMPACT.md` still had zero
  occurrences of either word afterwards. Two identical misses on the same item is a process finding,
  not an oversight. Now carried as `KMD_IMPACT.md` §14a.2 **K-F6**, with the derivation
  (`GPU_TIMESTAMP_FREQUENCY_HZ = 1e9` because vkd3d answers `1e9 / timestampPeriod` and this guest
  reports `timestampPeriod = 1`), the three counters (`ClkCal`/`ClkNoGpu`/`ClkFreq`, boot-cumulative
  and NOT hidden by the 600-refresh publish throttle), the deliberate `GpuClockCounter = 0`, and the
  ⛔ *a diag record here is FORBIDDEN, not omitted* IRQL rule.
* `PRESENT.md` §8.2's acceptance criterion `P12sub == P12take` is **unsatisfiable for windowed by
  construction** (`P12take` fires in `DxgkDdiPresent`, which a DWM-composited windowed present never reaches) —
  a *"trusting a zero"* pre-installed into the acceptance criterion of work that has not started.
* The absent-snapshot case is **uncounted** — neither `SnSub` nor `SnFbk` moves, so "no identity ever arrived"
  reads as a healthy zero (**XS**).
* `PBFlip`/`PBCpy`/`PBMmio` are last-value markers, not accumulators — use `VpPres`/`VpBlt`/`FlR<n>` instead.
* ✅ **PAID** — `kmd_render/src/virtio/gpu/mod.rs` carried a `#[cfg(test)] mod present_stream_tests`
  that **could never run** in a `panic=abort` cdylib, covering the present-stream helpers the D3D12
  fence bridge now depends on. A CLAUDE.md invariant violated in-tree. Moved, with the pure helpers,
  to `helios_kmd_logic::present_stream` + `present_stream_boundary_tests`; `grep -c 'cfg(test)'`
  over `kmd_render/src` is now **0**, and a `grep` for the old module name finds only the note in
  `gpu/mod.rs` recording the move (*"Do not reintroduce tests in this file"*). ⚠ Was cited `:5950`.
  ⛔ **THE COUNT: it was FIVE, not six** — the wave-1 correction #5 above is right and this bullet
  was wrong; `git show 3e750c0:…/gpu/mod.rs` counts 5 `#[test]`s. ⚠ But `present_stream_boundary_tests`
  holds **six** today, because the move recovered five and **added one**
  (`slot_63_and_new_generation_never_alias`) — which is why `kmd_logic`'s own doc comment still says
  *"These six tests lived in `kmd_render`"*. That sentence is wrong about provenance, not arithmetic,
  and `gpu/mod.rs`'s note (*"FIVE tests (not six …)"*) is the one to trust for the original.
