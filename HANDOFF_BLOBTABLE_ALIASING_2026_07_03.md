# Handoff — blob-table exhaustion root-caused+fixed, swapchain double-delete fixed, STABLE BOUND FEED; frontier = aliased-image content divergence (2026-07-03, fourth session)

Continues `HANDOFF_C1_IDDCX_WATCHDOG_2026_07_03.md` (its §4 was this session's brief).
Everything here is committed: parent through `106e531` (+ this doc), LookingGlass `e8a6a40b`.
Deployed live: KMD **22.22.42.0**, UMD b89f9918 (unchanged), LGIdd **15.36.22.667**.

## 1. THE 5-HOUR NO-OFFERS STATE — root-caused end-to-end and fixed

Walking in, the IDD had been in the offer-timeout replug loop for 5+ hours (1742
cycles, zero `AssignSwapChain`). The whole chain reduced to **one root**:

**`MAX_BLOBS = 256` — the KMD's bounded blob-tracking table — filled under normal
desktop churn.** Every venus ring/reply/fence shmem, every host-visible DXVK chunk,
every exported shared surface and every KMD-standard GDI surface is one slot. Once
full, every new ALLOC_BLOB failed **guest-side** (`STATUS_INSUFFICIENT_RESOURCES`,
nothing reaches the host — which is why the host log was silent):

- New processes failed `vkCreateInstance` → `VK_ERROR_OUT_OF_HOST_MEMORY` (the ring
  shmem is a blob; `helios_icd_shmem.log` shows `res=0` for every create after
  ~06:44Z, including the 131268-byte ring).
- dwm could not create the 1896×1030 IddCx swapchain backing (export allocs are
  blobs) — the 10,020× `DxvkMemoryAllocator: Memory allocation failed Size: 8773632
  Mem types: 0,1` storm with near-empty heaps. dwm cycled its Helios device 9×
  against that wall, then destroyed it for good at 04:56Z → **no dwm device on
  Helios → dxgkrnl never offers a swapchain** → the replug loop.
- The single host-side `virgl blob create error: Operation not permitted` (03:49:45Z)
  was the ONE attempt that found a free slot; per `HANDOFF_GDI_BLACKFRAME.md` §6g
  the proxy EPERM is the generic can't-materialize error, not a distinct class.

**Measured, not inferred**: new `tools/blob_capacity_probe.c` (discovers Helios via
CTX_CREATE escape probe — `KMTQAITYPE_ADAPTERREGISTRYINFO` now fails for every
adapter, name discovery is dead) read **38 free slots, 39th alloc → 0xC000009A**.
Corroborating live evidence: the probe's own release of its 38 slots gave dwm
enough headroom that dxgkrnl assigned a swapchain seconds later (09:40:17Z) — the
first offer in 5 hours, before any deploy.

**Fix (KMD 22.22.42.0, commit `8930b7a`):** MAX_BLOBS 8192, MAX_RESOURCES 16384,
MAX_CONTEXTS 1024, MAX_WINDOW_RANGES 1024 (all still init-reserved; no realloc
under the spinlock). All bounded-table rejections/drops now counted in
DISPATCH-safe atomics; the in-lock `diag::record` calls (PASSIVE-only registry
writes under the device spinlock — latent IRQL bug) in `take_live_resource` /
`adopt_blob_for_allocation` / `resource_create_blob` replaced with atomics. New
**`HELIOS_ESCAPE_QUERY_STATS`** verb (0x000A, 88 B) exposes occupancy / caps /
high-waters / rejects / window usage / ctrl-timeouts; `DxgkDdiCollectDbgInfo`
HDBG report bumped to v2 (21 DWORDs). Verified after deploy: 87/8192 live at
desktop steady state (high-water 599 including the probe's own 512), zero rejects,
`vkCreateInstance` works again, and `blob_capacity_probe` releases 512/512 clean.

⚠️ MAX_BLOBS reachable again = a leak, not a workload — the QUERY_STATS counters
make that decidable now; watch `blobs_live` drift across days.

## 2. WUDFHost kill #N+1 — the dtor double-delete — root-caused with a dump, fixed

With offers flowing again, the first bind killed WUDFHost (09:40:25Z, dump
`WUDFHost.exe-(PID-11016)`, stacks in `C:\HeliosDumps\stacks-11016.txt`):

```
thread 9:  LGIdd!CSwapChainProcessor::~CSwapChainProcessor   (last shared_ptr ref, thread exit)
           → imp_WdfObjectDelete → FxObject::DeleteObject
           → RtlEnterCriticalSection → KiUserExceptionDispatch   (freed FxObject lock)
           → UnhandledExceptionFilter → TerminateProcess
```

**The IddCx object contract (windows-driver-docs `iddcx-objects.md`) resolves the
June-25/June-26 ownership confusion:** the OS creates AND destroys IDDCX_SWAPCHAIN
objects; the swapchain is a **child of IDDCX_MONITOR**; `IddCxMonitorDeparture`
destroys the monitor object (and hence the child swapchain). Our replug primitive
departs monitors constantly, so the dtor's `WdfObjectDelete(m_hSwapChain)` was a
guaranteed eventual double delete of an already-destroyed child. The
`stacks-8464.txt` dump later PROVED the destruction chain live:
`IddCxImplMonitorDeparture → WdfObjectDelete → DisposeChildrenWorker →
IddSwapChain::Cleanup → CDXGIIndirectSwapChain::Release`.

**Fix (LookingGlass `e8a6a40b`, driver 15.36.22.667):** the driver never deletes
the swapchain object at teardown. The doc-sanctioned mid-processing
release-to-force-re-offer delete is unused by design — thread-exit errors escalate
to the replug primitive, and departure/arrival produces the new offer. (Deploy
gotcha rediscovered: `devcon` is not on PATH — use
`C:\Program Files (x86)\Windows Kits\10\Tools\10.0.26100.0\x64\devcon.exe`. Also:
every `win_cargo`/`win_looking_glass_idd` robocopy re-sync DELETES the other
tool's build outputs in the mirror, including LGIdd.pdb — rebuild from the same
commit and force-load symbols with `.symopt+0x40` to symbolize older dumps.)

## 3. RESULT: stable bound swapchain, continuous acquires — §4.1 CLOSED (instrument-level)

Since ~10:07Z: the binding stays bound, **49 acquires in the first hour** at dwm's
present cadence (`IddCx acquire returned hr=0 frame=N dirty=1`, correct idle
"pending" gaps between activity) — no replug loop, no stale kills. The stale-binding
watchdog is **vindicated, do not tune it**: its earlier firings were correct
detections of dwm being genuinely unable to render (table exhaustion); transition
frames disarm the first-frame timeout as designed.

One NEW kill class observed once (10:25:52Z, dump `WUDFHost.exe-(PID-8464)`,
self-recovered): `IddCx!ReportBugcheckForSwapChainTimeoutDriverDidNotReleaseFrame`
— IddCx kills the host if an acquired frame is not released within its deadline;
it fired with a departure mid-flight. Open item: defer ReplugMonitor while a frame
is outstanding + bound venus work between acquire and FinishedProcessingFrame.

## 4. THE FRONTIER: acquired frames are ALL ZEROS — aliased-image content divergence, minimally reproduced

The acquired 1896×1030 frames sample all-zero (`sampleNonZero=0/357`). This is NOT
plumbing: identity is verified end-to-end (dwm's 435 desktop draws target alloc
0x80004a00 = res 1179; the IDD opens res 1179 `ddi-shared ok`, exact size
8773632, mem_type 1 — creator and opener alias the same venus resource).

**Minimal reproducer: `tools/d3d11_shared_content_probe.cpp`** (~2 min via
schtasks; output `C:\Users\Rupansh\helios-probe\shared_probe_out.txt`). One
process, two D3D11 devices on Helios, one shared NTHANDLE BGRA RT:

| Step | Result |
|---|---|
| A: dev1 clear #1 → dev1 readback | color ✓ (first clear reaches raw memory) |
| B: dev2 open → readback | color ✓ (initial content crosses) |
| C: dev1 clear #2 + Flush → dev2 readback (immediately AND +3 s) | **STALE — still clear #1** |
| D0: dev2 clear #3 → dev2 readback | color3 ✓ (own writes visible) |
| D: → dev1 readback (+3 s) | **sees its OWN clear #2, never dev2's #3** |
| E: dev1 UpdateSubresource (copy-engine write) → dev2 readback | **PROPAGATES ✓** |

**Copies propagate; clears diverge per-image.** One physical allocation (E proves
it), but each host VkImage keeps private fast-clear/compression metadata — the
NVIDIA VUID-02726-class UB shape again, now on the sharing path. dwm's composite
(clears + raster into its image) never reaches raw memory as far as the IDD's
image can see → black frames. Same-device clear+readback passes
(helios_clear_test), so it is specifically the two-images-one-memory alias.

**Verified NOT the cause:** guest-side handle-type mismatch — the ICD's
`vn_image.c` `fix_external` rewrites app handleTypes to
`renderer_handle_type` (the validated ladder in `vn_physical_device.c` picks
DMA_BUF on NVIDIA 610.43); DXVK chains `VkExternalMemoryImageCreateInfo` on both
create and open paths (the D-A2 OPAQUE_FD hardcode is rewritten by the ICD before
the wire).

**Next leads, in order:**
1. Host-side truth: does the opener's host `vkAllocateMemory` (vkr translates
   `VkImportMemoryResourceInfoMESA` → `VkImportMemoryFdInfoKHR` in place,
   preserving the rest of the pNext chain — virglrenderer 1.3.0
   `vkr_device_memory.c:246-259`) still carry the
   `VkMemoryDedicatedAllocateInfo` for the opener's image, and does the creator's
   export allocate dedicated-to-image? NVIDIA stores image metadata in dedicated
   allocations; a non-dedicated import alias would explain exactly
   clears-diverge/copies-propagate. `HELIOS_VKR_DEBUG=validate` (needs host
   `vulkan-validation-layers` + render-server restart = **owner VM relaunch**)
   would flag every VUID violation on this path.
2. Intel comparison (`HELIOS_QEMU_RENDER_GPU` default Intel — owner relaunch): if
   the probe passes on ANV, the class is NVIDIA-metadata-specific, and the fix is
   making the host images/allocations properly external+dedicated end-to-end
   (C7.2), not a guest workaround.
3. If the mechanism resists: force the shared-surface class to LINEAR tiling
   guest-side (linear images are metadata-free) as a measured, documented
   contract — NOT as a silent fallback. This also converges with the P2/C6
   linear-GDI-surface work (`import size override 368640 < requirement 487424`
   warnings still stand for KMD-standard GDI surfaces).

## 4b. OWNER-VISIBLE OBSERVATION (after the fixes, during the probe runs) — load-bearing

The owner saw, live in the LG client during this session's testing: **a few frames
of the real Windows desktop (with a dark red overlay), which then went black again.**

Interpretation (matches the reproducer exactly): at (re)bind, the initial surface
content crosses the alias once (probe step B passes) → real desktop frames reach
the client; then dwm's ongoing composition (clears + draws through its own host
VkImage) diverges from the raw memory the IDD's image reads (steps C/D) → black.
Two conclusions:
1. **The full pipeline dwm→venus→IddCx→KVMFR→client renders real desktop content
   end-to-end when content exists.** The remaining defect is only the §4 aliasing
   divergence.
2. The dark-red tint matches the earlier wallpaper sighting — likely the
   10-bpc/HDR path or a format/swizzle issue; re-evaluate only after frames are
   sustained.

## 4c. Design note — VK_KHR_external_memory_win32 emulation in the ICD (owner question)

Current state: the ICD emulates external **semaphores** ICD-side (over D3DKMT sync
objects) but does NOT emulate `VK_KHR_external_memory_win32`. DXVK sidesteps it
with the helios-specific KMT bridge (`heliosKmtOnlySharedResources()`:
OPAQUE_FD wire fiction + `VkImportMemoryResourceInfoMESA` typed resid import) —
hence the `proceeding without VK_KHR_external_memory_win32` warnings in every
DXVK log. Emulating memory-win32 in the ICD (HANDLE ↔ {resid, identity} mapping
ICD-side, DXVK using its stock win32 sharing path) is the cleaner C7.2 shape:
one place owns external-memory correctness (handle types, dedicated-allocation
linkage, external image info end-to-end), instead of a DXVK special case per
call site — and it is plausibly the right vehicle for the §4 aliasing fix, since
the fix must guarantee both sides' host images/allocations are properly
external+dedicated. Take this decision when §4's mechanism is confirmed.

## 5. Residual C1 boot-path hole (unchanged priority)

This boot (02:00:09Z) reproduced `invalid res_id 45` → CS error → dead ring at
02:04:20Z with the C1-fixed stack deployed and running (757 identity lines / 756
`ddi-shared ok` in dwm's log — exactly one failed open). The ICD honesty kept dwm
from abort-looping; Windows restarted it. A second CS error (`vkAllocateMemory`,
worker 571734, no invalid-res_id line) appeared at ~09:40Z. Root-cause still
needed: the attach-to-opener-ctx ordering at boot (open before the opener's venus
context exists?) — catch it on the next cold boot with the now-clean host log.

## 5b. Housekeeping performed (2026-07-03, pre-cold-boot)

- Deleted all dumps in `C:\HeliosDumps` (25.9 GB freed incl. the 19.5 GB full dwm
  dump; C: now 229 GB free). Kept the three `stacks-*.txt` analysis records.
- dwm WER LocalDumps: **FULL → minidump** (DumpType 1, DumpCount 3) — crash
  evidence stays cheap for the cold boot.
- WUDFHost WER LocalDumps → minidump; **WUDFHost SilentProcessExit + IFEO
  GlobalFlag stay ARMED with FULL dumps** — deliberate deviation from the earlier
  "disarm when stable" item: two kill classes are still open
  (DriverDidNotReleaseFrame §3; C1 boot hole §5) and full dumps were decisive
  twice today. Disarm only after ten clean cold boots.
- WER ReportQueue purged (~44 MB). Guest `C:\ProgramData\Helios\*.log` cleared
  (live processes recreated fresh ones). Host `/tmp/helios-qemu-stderr.log`
  rotated to `.2026-07-03-session4` so the next boot's host log is unambiguous
  (vkr lines carry no timestamps — never mix boots).
- The KMD diag registry ring (3000 one-shot) is still burned in minutes by the
  per-submit 0x0D10/0x0D12 tracers — strip with the audit C-class cleanup (code
  change, not done today).

## 6. Ops learned this session

- `blob_capacity_probe` + `vk_probe` run over SSH; D3D11 probes need
  `schtasks /IT` (session 1) — stdout is buffered, redirect to a file and poll.
- WUDFHost SilentProcessExit dumps land in a SUBFOLDER:
  `C:\HeliosDumps\WUDFHost.exe-(PID-N)-M\WUDFHost.exe-(PID-N).dmp`.
- `dwm.exe.1908.dmp` is **19.5 GB** (FULL LocalDumps) — the housekeeping item to
  reduce dwm LocalDumps is now urgent disk-wise (~21 GB total in C:\HeliosDumps).
- The diag registry ring (3000 one-shot) was full by early boot — the per-submit
  `diag::record` tracers (0x0D10/0x0D12) burn it in minutes; strip with the
  C-class cleanup.
- KMD deploy verified end-to-end with the new stats verb: deploy → run
  `blob_capacity_probe` → `[before]` line shows live counts.

## 6b. COLD-BOOT CHECKLIST (the boot the owner runs after this session)

What to check, in order, on the untouched cold boot (deployed: KMD 22.22.42.0,
UMD b89f9918, LGIdd 15.36.22.667, all committed):

1. **Owner eyes on the LG client** (the only closing evidence): expected per the
   current model — desktop frames appear at bind, then fade to black (the §4
   divergence). Sustained frames would mean the divergence is
   churn-window-dependent; note EXACTLY what is seen and when relative to boot.
2. **Self-convergence**: did the IDD reach a bound, acquiring swapchain with zero
   manual actions? (`looking-glass-idd.txt`: MonitorArrival → AssignSwapChain →
   `acquire returned hr=0` lines; no FAILED_POST_START, no kill dumps in
   C:\HeliosDumps.)
3. **C1 boot hole**: the rotated host log (`/tmp/helios-qemu-stderr.log`) — any
   `invalid res_id` / CS error in the first ~5 min? Pair with dwm's
   `umd-<pid>.log` (identity lines vs `ddi-shared ok` count) to find the failed
   open. This is task the next session root-causes with clean evidence.
4. **Blob table at boot steady state**: run
   `C:\Users\Rupansh\helios-probe\blob_capacity_probe.exe` via
   `schtasks /run /tn HeliosBlobProbe` → `[before] blobs=N/8192` line
   (output `C:\Users\Rupansh\helios-probe\blob_probe_out.txt`). Expect low
   hundreds; watch drift over hours for a leak.
5. **The aliasing reproducer on a clean boot**:
   `schtasks /run /tn HeliosSharedProbe` (output `shared_probe_out.txt`) —
   confirm the C/D divergence reproduces cold (it should; it is
   state-independent).

## 7. Copy-paste prompt for the next session

> You are continuing the Helios vGPU project in /home/rupansh/helios-vgpu. Read
> `HELIOS_FIRST_PRINCIPLES_AUDIT.md` (contracts C1–C7), then
> `HANDOFF_BLOBTABLE_ALIASING_2026_07_03.md` in full — the frontier is its §4.
> STATE: the 5-hour no-offers state was KMD blob-table exhaustion (MAX_BLOBS=256),
> fixed in KMD 22.22.42.0 with the new HELIOS_ESCAPE_QUERY_STATS observability;
> the WUDFHost dtor double-delete of the OS-owned child swapchain is fixed in
> LGIdd e8a6a40b/15.36.22.667 (the IddCx contract: the OS creates AND destroys
> swapchain objects; they are children of the monitor). The IDD then held a
> STABLE bound swapchain acquiring at dwm's cadence for over an hour — and the
> OWNER SAW real desktop frames (dark-red tint) in the LG client that faded to
> black, matching the §4 model: initial content crosses the alias once, then
> dwm's clears/compose diverge per host VkImage. Minimal repro
> `tools/d3d11_shared_content_probe.cpp` (schtasks HeliosSharedProbe): copies
> propagate, CLEARS diverge — NVIDIA per-VkImage fast-clear/compression metadata,
> the VUID-02726 UB shape on the sharing path. A cold boot happened right after
> the session — FIRST read its evidence per §6b (owner sighting, self-convergence,
> C1 boot-hole signature in the rotated-clean host log, blob occupancy, repro).
> THEN work §4's leads in order: (1) host-side dedicated-allocation/external-info
> linkage through vkr's import (virglrenderer-1.3.0 vkr_device_memory.c:246 —
> clone is in the session scratchpad or re-clone tag virglrenderer-1.3.0), with
> HELIOS_VKR_DEBUG=validate (host validation layers; needs owner VM relaunch);
> (2) Intel-host comparison (owner relaunch); (3) consider ICD-side
> VK_KHR_external_memory_win32 emulation as the C7.2 vehicle for the fix (§4c —
> owner asked for this direction; only semaphores are emulated today); (4) only
> then LINEAR-tiling shared surfaces as an explicit documented contract. Also
> open: §3's DriverDidNotReleaseFrame kill (defer replug while a frame is held,
> dump stacks-8464.txt), §5's residual C1 boot hole, P2/C6 linear GDI-surface
> open mismatches, and the audit C-class diag-tracer strip (the registry ring
> burns its 3000 cap in minutes). Housekeeping state per §5b: dwm=minidumps,
> WUDFHost SilentProcessExit FULL dumps deliberately still armed. The overseer's
> standing directive: no hacks, no kick rituals, loud failure over fake success;
> only owner-visible LG-client output closes milestones. Ask before cold boots or
> VM relaunches. Tree committed through this doc; LGIdd deploys via the full
> devcon path (`C:\Program Files (x86)\Windows Kits\10\Tools\10.0.26100.0\x64\devcon.exe`),
> KMD version bump before every deploy (build.rs + Cargo.make.toml), UMD hotplug
> AFTER KMD install with -UmdDll ...\umd\target\release\helios_umd.dll.
