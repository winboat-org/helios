# PENDING.md — what is left before FL 12_1 and before a real D3D12 app runs

**Written 2026-08-06** from four independent read-only inventories (DDI surface + caps, present/swapchain,
sync/queues/residency, resources/descriptors/state), each `file:line`-cited, cross-checked, and with the
load-bearing claims re-verified by the integrator. Organised as **`METHOD.md` Phase 1 subsystems**, because
that is now the unit of work.

⚠ **This is a gap list, not a plan.** Sizes are S/M/L with the reason. Nothing here is scheduled.

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
| **A4** | ⛔ **The boundary is a PREFIX, not the frame's own fence — a standing invariant violated on the new arm.** `watermark = gpu_fence_id + 1` and `async_retired_up_to` is satisfied only when **no** in-flight venus entry has `fence_id < watermark`, every ring, every process — including DWM's ring-1 scanout copies. This is the exact superset the present path had to relax away (`PRESENT_EXACT_WATERMARK_USED`), and CLAUDE.md's table forbids it: *"a WDDM fence may wait on the frame's OWN boundary, never on the whole `next_wire_fence` backlog"*. **CONFIRMED.** | `gpu/mod.rs:5028-5042`, `:5676-5684` | **S** (exact test + its own counter) |
| **A5** | **`wddm_pending` overflow completes 256 packets EARLY**, while their host work is still running, and forces `release_all_scanout_leases(Teardown)`. A4 makes reaching 256 likelier than the code's "practically unreachable" note assumes. The fix is the consumer-side head deadline ROADMAP already names for K-F2 — now covering **two** writers. | `gpu/mod.rs:312`, `:5445-5459`, `submit_command.rs:586-616` | **S** |
| **A2** | The 64-entry seqno cache **can never hit**: `vkd3d_release_vk_queue` issues a real `vkQueueSubmit2` *after* we sample, advancing the ring tail that is the cache key. ⛔ Its own doc describes behaviour it cannot deliver. | `command.c:25591-25612`, `vn_ring.c:373-383`, `vn_renderer_helios.c:2034-2043` | **XS** doc / **S** repair |
| **A3** | `dev_mutex` — which serialises **every venus submit in the process** — is held across a kernel escape that retries up to **5 s** on `QueueFull`, and QueueFull is reachable at frame rate (64 descriptors, 3/chain ⇒ ~21 concurrent; 3 queues × 2 ECL × 200 fps is at the ceiling). | `vn_renderer_helios.c:2025-2069`, `ctrl.rs:100`, `gpu/mod.rs:83` | **S** |
| **A6** | A fence sampled across a StopDevice/StartDevice cycle passes the clamp **uncounted** (`next_wire_fence` restarts at `BASE + instance·2^32`), names a fence the new instance never issued, and completes immediately — **the fence lies early**, and `GpuFncClamp` will not flag it. No owner check either. SUSPECTED. | `gpu/mod.rs:5680`, `:2214`, `:1377` | **S** |

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

### S-3 · Present identity bridge — ⛔ nothing displays
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

* `DDI_REFERENCE.md:3036-3041` still carries the tiled-resources reasoning `DX12.md` §4.4 **struck** — the
  third document to carry it after the correction reached four other sites.
* `caps12.rs`'s ROV claim — **false**, corrected `63b8f1b`. Its citation was also wrong.
* `caps12.rs:557-559`'s binding-tier reason — **stale** since all 15 descriptor slots landed.
* `DX12.md` §4.4's tiled lane attribution names two tile slots; the header has **four**.
* `queue.rs:2118-2121`'s stated blocker for command signatures is **discharged**.
* `SPECS.md:136` logged the timestamp-frequency question against `KMD_IMPACT.md`, which has **zero**
  occurrences of "timestamp". It fell through, and it is S-1 — the top benchmark blocker.
* `PRESENT.md` §8.2's acceptance criterion `P12sub == P12take` is **unsatisfiable for windowed by
  construction** (`P12take` fires in `DxgkDdiPresent`, which a DWM-composited windowed present never reaches) —
  a *"trusting a zero"* pre-installed into the acceptance criterion of work that has not started.
* The absent-snapshot case is **uncounted** — neither `SnSub` nor `SnFbk` moves, so "no identity ever arrived"
  reads as a healthy zero (**XS**).
* `PBFlip`/`PBCpy`/`PBMmio` are last-value markers, not accumulators — use `VpPres`/`VpBlt`/`FlR<n>` instead.
* `kmd_render/src/virtio/gpu/mod.rs:5950` — a `#[cfg(test)] mod present_stream_tests` with six tests that
  **can never run** in a `panic=abort` cdylib, covering the present-stream helpers the D3D12 fence bridge now
  depends on. A CLAUDE.md invariant violated in-tree; the helpers are pure and belong in `kmd_logic`.
