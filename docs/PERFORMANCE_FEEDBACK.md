# DX11/DX12 performance investigation

## PassMark DX11 timer resolution, 2026-09-12

The signed **22.22.276.0** WinBoat acceptance run measured a DX11
improvement: alternating disabled/default-enabled runs score **11.4 / 20.6 /
10.9 / 21.1**. The means are **11.15 → 20.85 (+87.0%)**; individual paired gains
are +80.7% and +93.6%. This is four completed runs, not a confidence interval or
a native-GPU performance claim. On that pre-integration stack, PassMark DX12
still refused stateful command signatures. The combined native DGC stack is
tracked separately in ROADMAP.md.

This machine has a Ryzen 5 5600 and Radeon RX 6600 (RADV NAVI23), with four
guest vCPUs and 12 GB RAM. The active WinBoat renderer uses Debian Mesa
25.0.7; the previously recorded 26.2.2 belongs to the outer Fedora installation.
The container package and mapped RADV library were checked during integration.
This is a separate configuration from the earlier 3DMark measurements below.
The actual workload is the **32-bit
`PT-D3D11Test.exe`** launched by PerformanceTest64 in interactive session 2.
The 1280×800 desktop and benchmark settings stayed fixed. These reported
PassMark results include its low-resolution penalty; the live scene displays
about 31 FPS with the fix. Do not compare the penalized result directly with
raw callback throughput.

| Clean run order | `UmdTimerRes` | Reported DX11 result | Successful Presents |
|---|---|---:|---:|
| off1-276 | 0 | 11.4 | 484 |
| on1-276 | absent: shipping default | 20.6 | 879 |
| off2-276 | 0 | 10.9 | 463 |
| on2-276 | absent: shipping default | 21.1 | 902 |

All four use the same installed .276 binaries and complete HTML exports.
Their final benchmark UMD log incarnation names the same DriverStore x86
module and the expected knob value, with matching Render/Present attempts
and successes and no frame-gate, normalization or timer failures. CPU tracing,
feed tracing and VNC capture were disabled for this alternating comparison.

### Cause and independent profile check

CPU context-switch traces identified long `DelayExecution` waits in both the
DXVK CS worker and its completion worker. A separate noninvasive 32-bit CDB
stack capture puts both in `KERNELBASE!Sleep`; the exact installed ICD's return
PC maps to Mesa `os_time_sleep`, and the completion path also maps to `vn_relax`.
The main thread is waiting in the runtime's Present callback. A temporary
process-local `timeBeginPeriod(1)` diagnostic increased active throughput from
roughly 16 to 31 FPS, motivating the production change.

Separate before/after CPU/feed profiles confirm the intended wait reduction.
Both ETW traces have zero lost events/buffers. The comparison uses their first
7.5 seconds, with only complete 5 ms feed bins (7.495 seconds). Neither compared
run has a VNC collector. The earlier after trace (`trace276`) is retained as
capture evidence; `trace276-clean` supplies the after numbers below.

| Diagnostic measurement | .275 | .276 default |
|---|---:|---:|
| CS `DelayExecution`, mean / median | 10.20 / 11.20 ms | 1.51 / 1.48 ms |
| Completion `DelayExecution`, mean / median | 10.73 / 11.18 ms | 1.62 / 1.62 ms |
| CS delay count / total | 283 / 2885.9 ms | 308 / 463.6 ms |
| Completion delay count / total | 407 / 4366.7 ms | 1060 / 1713.9 ms |
| Present callback wall time per frame | 38.52 ms | 20.03 ms |
| CS work wall time per frame | 41.32 ms | 23.42 ms |
| Raw Presents / second | 19.61 | 30.55 |

Delay measurements are complete observed off-CPU intervals, including
wake-to-run scheduling; waits crossing the window boundary are excluded.
These worker intervals overlap and include waiting; do not sum them or call
them GPU execution time. The diagnostic FPS is distinct from the clean full-run
results. The fix does not bypass frame completion, snapshots, resolves or
synchronization. Remaining CPU work and the resolution penalty are not solved
by requesting finer timers; further optimization needs fresh attribution.

### Implementation, cleanup and acceptance

Each DX11 bridge device owns a balanced 1 ms timer request, declared before its
DXVK members so it outlives worker destruction and partial initialization.
Successful requests receive one `timeEndPeriod(1)`; failed requests receive
none. API failures are counted, logged and included in the UMD failure gate.
`HKLM\SOFTWARE\Helios\UmdTimerRes=0` disables this driver's request for new
processes; the absent default is enabled and the override was removed afterward.
Other code can independently request fine timers. Microsoft documents the
[matched request contract](https://learn.microsoft.com/en-us/windows/win32/api/timeapi/nf-timeapi-timebeginperiod)
and Windows 11's possible reduction of timer resolution for fully occluded
processes. More frequent timer wakeups while a DX11 device exists are a power
and scheduling tradeoff; this change does not disable Windows' occlusion policy.

A real D3D11 device-lifetime probe measured average `Sleep(1)` duration before,
during and after release: **14.81 / 1.90 / 15.17 ms (x86)** and **15.46 / 1.89 /
15.16 ms (x64)**, with zero remaining COM references. On .275 the same probe
stayed around 15 ms throughout. An actual-source fixture separately exercises
six success/failure/unwind/overlap cases. Both architectures build with WinMM
imports; the gate's literal failure patterns were exercised. Whole-change
review completed two dry rounds with different lenses after repairing a gate
pattern that initially used regex syntax in a literal substring matcher.

All twelve resize/query/MSAA cases pass on the installed .276 drivers.
Independent host VNC grading checks the complete pattern body across **122
captured frames**, all twelve resized geometries in each architecture,
sRGB midpoint 188, UNORM midpoint 128,
and advancing serials. The bottom 16-pixel serial strip is decoded separately;
window borders and the surrounding desktop are outside the exact pattern oracle.
Both resize runs reclaim four idle rings. All **14 packaged smoke cases** pass,
including native/x86 DX12 clear/readback of 65536
exact pixels each. The same DWM process (PID 5000) survives, the device remains
Code 0, all five DriverStore hashes/versions stay correct, `SnQrF` stays absent
and `QSpErr` stays zero. A separate PassMark host capture shows changing terrain,
buildings and translucent objects
without the earlier scrambling; it is visual progress evidence, not an exact
reference oracle for the benchmark.

The signed bundle is `helios-windows-x64-22.22.276.0-bad9ff18.zip`, SHA256
`90461b12568a32cc712b939fab369adb41872afcb2828eeaa6adbce38831ad32`.
It was built from `bad9ff18`; all 44 manifest entries and catalog membership
of all five driver images pass. The unchanged DXVK engine's 594 source inputs
and 18 linked archives were revalidated. Rollback state is saved under
`C:\ProgramData\Helios\wow64-evidence\before276`.

Evidence lives in `tmp/passmark-perf-20260912/`: `ab-summary.json`, the four
run directories and original exports, `profile-comparison.json`, `trace275/`,
`trace276{,-clean}/`, `stacks275/`, `lifetime27{5,6}/`, `acceptance276{,-vnc}/`,
`verify-276/`, and build, manifest, catalog and guest-inventory records.
Production commits are
`b02260e` (timer lifetime) and `bad9ff18` (version stamp).

## Earlier 3DMark investigation, 2026-09-06

**Steel Nomad Vulkan is repaired and the measured submission retry improvement
is deployed in .270/oem53.inf**, enabled by default with no override. Code 0,
the desktop and all four native runtime ordering cases pass.

| Final clean check | Isolated .268 baseline | .270 result | Observed change |
|---|---:|---:|---:|
| Time Spy GT1, first run after reboot | 112.164719 FPS | 118.746094 FPS | +5.87% |
| Time Spy GT1, same-build repeat | 112.164719 FPS | 136.251602 FPS | +21.47% |
| Fire Strike GT1 | 244.769699 FPS | 248.231491 FPS | +1.41% |

The later Time Spy result reproduces the initial .269 candidate's
**137.724655 FPS (+22.79%)**, but the lower early run remains part of the result.
Post-boot variability is unresolved; these are completed observations, not a
minimum-gain guarantee. No 10–20% gain in both APIs has been demonstrated.
The final Vulkan control completes at **90.683228 FPS / score 9068**; the first
repaired .268 control was 93.228233 FPS. Owner moving-scene/shadow acceptance
and broader documented correctness gaps remain open.

Measured 2026-09-06 against checkpoint `fe8cb4a` in the recovered
`helios-vgpu-dx12` checkout. The earlier bounded cached-feedback candidate
regressed Fire Strike and did not improve
Time Spy. Its source change was removed and the exact accepted ICD restored.
This rules out this candidate; it does not establish that all further gains
are impossible. The owner subsequently prioritized DX12 profiling, with Steel
Nomad **Vulkan** as the control. The follow-up below isolates a real CPU hotspot
but its logger experiment did not demonstrate an FPS gain either. The later
transport-capacity wake comparison is reported separately below.

## Earlier feedback-memory comparison

| GT1 workload | Initial accepted stack | Cached-feedback candidate | Change | Accepted ICD restored |
|---|---:|---:|---:|---:|
| Fire Strike, DX11 | 243.930298 FPS | 203.728149 FPS | -16.48% | 244.428558 FPS |
| Time Spy, native DX12 | 112.782494 FPS | 111.504173 FPS | -1.13% | No additional run |

Every listed result has workload status 0, a completed archive and an export.
These are GT1 frame rates, not full benchmark graphics scores. The single
restoration run resolved the material DX11 regression uncertainty: returning
to the original ICD returned Fire Strike to within 0.21% of its initial result.
The small Time Spy difference is not a statistically established regression.
No interleaved campaign or parameter sweep was performed.

The installed standard definitions were restricted to GT1: Fire Strike
1920x1080, Time Spy 2560x1440, fullscreen, vsync and triple buffering off.
Completed-result settings match between each API's before/after runs except
result identifiers and output paths. The guest scanout is 1280x800; this is
distinct from benchmark rendering resolution and stayed the same. 3DMark is
2.32.8454. Audio, SystemInfo, monitoring and online upload were disabled.
All runs used interactive scheduled tasks in session 1. No ETW, feed trace,
`HELIOS_PERF` or debug-log option was enabled in these comparison runs.

The owner's approximately 100 FPS shadow acceptance and the earlier automated
74.26 FPS instrumented run remain separate observations. Neither was used as
the baseline here. An initial Time Spy harness attempt failed while parsing
whitespace in an empty numeric selector, before starting its workload. It has
no result archive and is excluded; compact XML fixed the harness.

## Profile findings and limits

Separate diagnostic GT1 runs completed at 236.233124 FPS (DX11) and 111.311523
FPS (DX12). Each has an eight-second render-phase CPU/DxgKrnl trace with zero
lost buffers/events, plus workload-specific ICD telemetry. Full-process ICD
averages include startup/teardown; they are not critical-path frame times.

| Observation | DX11 | DX12 |
|---|---:|---:|
| Vulkan QueueSubmit2 average | 23.09 us | 125.08 us |
| Renderer submission mutex acquisition average | 3.58 us | 46.07 us |
| Renderer submit escape average | 147.55 us | 185.52 us |
| Feedback retirement observation average | 1.021 ms | 0.874 ms |
| Feedback fast retirements / fallback / wire | 7575 / 0 / 0 | 73217 / 0 / 0 |

Time Spy's submission/lock overhead is materially larger. The render-phase
DxgKrnl DMA start-to-stop median is 4.10 ms for Fire Strike and 0.69/0.81 ms for
Time Spy's two busy execution contexts. These are scheduled packet lifetimes,
not isolated shader execution time. Packet types and concurrent queues must
not be summed into a frame budget. The archived WS2 3.7 ms producer floor was
not assumed to be the current floor.

The DX11 feed trace's active bins contain about 19 Vulkan submissions per
Present and 1.85 command buffers per submission. Present callbacks average
273 us/frame and Render callbacks 31 us/frame. Submission batching was retained.
The measured ICD WSI flush/wait hooks are below 0.1 us per QueueSubmit2 on these
native DXGI routes. That does **not** measure all DWM staging/copy GPU time.
Present snapshots, staging freshness, barriers and copy recycling were retained.

The hottest sampled ICD function in Fire Strike is the semaphore feedback
counter reader (1985 samples). Time Spy has 704 samples in the fence feedback reader
and substantial retirement polling/spin. Original workload map counts are
cached/WC = 0/2 and 1/48 respectively. These observations motivated a storage
choice experiment; sampled polling time is not automatically removable frame
time. Release vkd3d lacks sufficient symbols, so xperf nearest-symbol names alone
were not accepted as attribution. The later raw-PC/disassembly investigation
below does prove the logger lock hotspot. Other nearest-symbol names remain
unreliable; raw PCs and the ICD's DWARF mapping were retained.

The small KMD producer-completion CPU sample contribution did not justify an
8192-slot table redesign. The archived WS2 rejected flip-queue-depth and local
allocation-cache experiments were not repeated, and no flush/wait was removed.

## Candidate, checks and disposition

The experiment preferred a compatible host memory type genuinely advertising
HOST_VISIBLE, HOST_COHERENT and HOST_CACHED for the shared feedback allocator.
The mask was captured before Venus synthesizes compatibility flags. Absent
support retained the original selection; allocation/map failure remained an
error. It was opt-in through `HELIOS_FEEDBACK_CACHED=1`, covering fence,
semaphore, event and query feedback. The [Vulkan memory-type contract](https://docs.vulkan.org/refpages/latest/refpages/source/VkMemoryPropertyFlagBits.html)
and [stock vkr 1.3.0 mapping code](https://gitlab.freedesktop.org/virgl/virglrenderer/-/blob/1.3.0/src/venus/vkr_device_memory.c)
support coherent cached backing. All existing feedback barriers, exact values,
host fence/semaphore waits, admission and independent consumer release remained.

Two independent review rounds with different lenses were dry, including a
completeness critic. Windows Mesa built with only existing warnings. The four
native synchronization cases passed with 65536-word GPU witnesses per case,
including blocked waits and cross-process signaling. Probe telemetry confirmed
cached mappings. No owner observation of the candidate's moving scenes or
shadows was received, so **visible correctness was not accepted**. Completed
benchmarks and ordering probes do not substitute for that observation.

The candidate was rejected on performance, reversed exactly, and the original
Mesa source rebuilt successfully. The reason cached feedback hurts DX11 has
not been isolated; cheaper CPU loads were only a hypothesis. Do not enable it
or repeat this allocation change without a new causal investigation.

Existing DX12 gaps remain explicit: general DX12-to-DX11 external queue-family
ownership/resource-use tracking; host device-loss/disconnect error reporting;
the refused nonzero monitored GPU fence placements/direct queue fence DDIs;
and sharing, unchanged bindings, resize/rotation, teardown and WSI stress.
The clean Time Spy baseline also logs **77691 pending allocator resets**, and
the candidate 93574. Engine Reset returns S_OK while retaining the pool when
internal references remain; fence-worker reference-release lag and premature
reuse have not been separated. This is an unresolved diagnostic/lifetime issue,
not proof of early GPU completion. The later logging experiment preserves all
of these diagnostics; reducing contention does not fix allocator lifetime.
`ClearRootArguments` and the DX11 raw-SRV-hazard census also remain nonzero.

## Earlier experiment evidence and restoration

All task artifacts are under `tmp/perf-20260906/`: `run-benchmark.ps1`, per-run
invocation/settings, hashes, registry snapshots, result archives, driver logs,
`profile-*` traces and summaries, `cached-sync`, and `cached-feedback.patch`.
`analyze-dxg.py` checks trace duration/loss and ignores tracerpt's inconsistent
+05:29/+05:30 suffixes while preserving the common local event clock; using
those suffixes literally invents one-minute gaps. CPU traces are approximately
8.5 seconds. The later Time Spy host GPU sample is a separate time window and
must not be presented as aligned with that CPU slice.

Source and loaded-artifact records are `source-host-before.json`, per-workload
`modules.json`, and `deployed-final.json`. Root remained at `fe8cb4a`; Mesa
`a717d4e`, DXVK `d5854f0`, vkd3d `71ebda7`, QEMU `5834375` are this experiment's
fixed source snapshot. Nested source trees were clean at that restoration. That first experiment
made no commit, push, KMD/UMD change, guest reboot, launcher change or host
renderer change. The guest stayed on KMD 22.22.266.0/oem50.inf and WDDM 2.1 with release
UMDs, async WSI and retire feedback enabled. Stock virglrenderer is 1.3.0-2.

SHA256 prefixes (full hashes are in the artifacts): accepted/deployed ICD
`FC38CC4AC0FF`, rejected candidate `CFA3251101B0`, release UMD11 `C624581347DD`,
release UMD12 `ADC0B0EA6F1C`, KMD `B13D491E8952`. The final rebuild from restored
Mesa source is `10D5E7156611`; it was **not deployed**. The exact original
`FC38CC4AC0FF` binary was restored and verified in the restoration benchmark.
The follow-up also retains this exact ICD.


## DX12 follow-up: actual CPU and queue attribution

The accepted .266 / release UMD12 `ADC0B0EA6F1C` / Mesa `FC38CC4AC0FF`
stack completed `timespy-queue-profile` at **110.339043 FPS**. This is a
separate diagnostic run, not a clean baseline. Existing vkd3d queue telemetry
and an eight-second CPU/DxgKrnl ETW slice were enabled together. Both ETW
traces have zero lost events/buffers. The analyzed interval is **8.014027 s**,
with 854 complete graphics-queue callbacks, 6838 graphics EXECUTEs and 1708
compute EXECUTEs: approximately eight and two per frame respectively.

| CPU queue-worker region, totals in that interval | Graphics queue | Compute queue |
|---|---:|---:|
| Before Vulkan execution | 5.003 s | 5.604 s |
| Of that, thread blocked | 4.805 s | 5.508 s |
| Vulkan execution region, CPU wall span | 0.303 s | 0.153 s |
| After execution, including exact completion publication | 1.712 s | 0.551 s |
| Present producer callbacks | 0.279 s | — |

On the graphics queue, dividing by those 854 callbacks gives **5.86 ms before
execution, 0.36 ms in the Vulkan execution region, 2.00 ms after execution and
0.33 ms in callbacks per frame**. These are queue-worker wall spans, including
blocked time, not GPU pass timings. Graphics/compute queues and app workers
operate concurrently: do not sum them into a frame budget. Enqueue-to-worker
start has a 3.56 ms median on the graphics queue; that backlog also overlaps
other work.

CPU context-switch stacks locate the long pre-execution waits in
`vkd3d_helios_execution_wait`: the actual loaded DLL's return PC has RVA
`0x7ef15`, and disassembly shows the admission event's zero/100 ms
`WaitForSingleObject` loop. A one-second stack slice contains 655 such graphics
blocks (622.4 ms blocked) and 234 compute blocks (719.6 ms blocked). Most wakes
come from the Windows GPU scheduler. These waits enforce runtime queue order.
The trace does **not** yet separate required cross-queue dependencies from
excess GPU-completion observation or runtime admission delivery latency.
Removing the waits is not an admissible optimization.

The second concrete finding, over the full **8.898 s CPU ETW trace** (not
just the queue-analysis interval), is **11528 / 48305 process CPU samples
(23.87%)** inside the unbuffered diagnostic logger's actual spinlock loop, RVAs
`0x85ad0..0x85aeb`. The disassembly contains the pause/load/test/retry loop
before `fprintf`/`vfprintf`; this attribution no longer depends on xperf's
nearest symbol. The main thread spends 688 / 3961 samples there, and an app
worker 761 / 3167. This is aggregate sampled CPU activity, **not 23.87% of frame
wall time**. The run emits 70779 pending-allocator-reset diagnostics; the
underlying lifetime question remains open.

Host NVML samples aligned using the host birth times of profile marker files
average **41.8% GPU utilization**, range 26–54%, and 241.6 W over 32 samples.
This is a coarse occupancy proxy. Guest wall time was about 2.27 seconds behind
host time; directly aligning wall-clock timestamps would include idle time and
produce a false lower estimate. Marker-copy latency is uncalibrated. Neither
NVML nor vkd3d's misleadingly named `gpu` trace lane provides per-pass GPU
hardware timestamps: that lane terminates at a CPU fence-worker callback and
clamps overlapping spans. Only the CPU `regions`/`overhead` lanes were used here.
CPU ETW and queue telemetry are aligned independently using 24 identical raw
QPC context-switch/ready events; the offset spread is 0.8 us.

## Bounded logger experiment

The only engine change replaces the global logger spinlock with the existing
platform `pthread_mutex_t` abstraction at all three acquisition sites. Message
filtering, message contents, buffering, flush boundaries and error reporting
are unchanged. No GPU ordering or completion code changes.

Two independent review rounds with different lenses and a completeness critic
were dry. The Windows release engine and release UMD12 builds passed. A
CPU-only stress test with 16 writers and a concurrent flush thread produced
exactly 32000 distinct intact messages in each of buffered/unbuffered modes,
with no loss or duplicates. Following activation and reboot, all four native
runtime synchronization cases passed again with 65536-word GPU witnesses.
The desktop capture is intact. No owner observation accepting the candidate's
moving scenes or realtime shadows was received.

| Clean native Time Spy GT1 | Before | Logger mutex | Change |
|---|---:|---:|---:|
| FPS | 113.561684 | 111.940567 | -1.43% |
| Reciprocal average frame time | 8.806 ms | 8.933 ms | +0.128 ms |

Both workloads completed with status 0 and an archive/export. The 71 recorded
settings differ only in output path and the adapter LUID assigned at reboot;
rendering remains standard 2560x1440 fullscreen, async compute on, vsync/triple
buffering off. Definition hashes and controlled environments match. This is
one focused before/after comparison crossing a reboot/package activation,
not statistical proof of a 1.43% regression. It demonstrates **no FPS gain**.
The candidate diagnostic run completed at **111.373055 FPS** (not a clean
comparison). Its 8.711 s CPU trace has zero lost events/buffers and 39920
process samples; none lands inside the actual rebuilt `vkd3d_dbg_printf` body
(RVA `0x85860..0x85cc8`, from the matching PDB and disassembly). Imported lock
and stdio code can still consume CPU; this does not claim zero logging cost.
Across the respective 8.014/8.009 s queue-analysis windows, aggregate process
running time per graphics callback is 48.38/39.73 CPU-ms. These sums span
concurrent threads and instrumented windows, not frame latency or a clean-run
CPU benchmark. Graphics pre-execution blocking remains 4.805/4.829 s. Thus the
CPU saving did not establish a throughput gain; the patch is rejected for this
FPS investigation and preserved as evidence. No diagnostics were suppressed.

Activation required signed package **22.22.267.0 / oem51.inf**. The experiment's only root
code/config change was the version stamp: the .266 and .267 KMD `.text`, `fothk`,
`.data`, `.pdata`, `PAGE`, `.edata`, `INIT` and `.reloc` sections are byte
identical. `.rdata` build metadata and `.rsrc` version differ. Candidate release
UMD12 is `7257288A93EC`; loaded-module capture verifies it inside Time Spy.
UMD11 `C624581347DD` and ICD `FC38CC4AC0FF` are unchanged. A prior ProgramData
hotplug attempt and same-version package attempt did not activate the candidate;
`timespy-logging-after` loaded old `ADC0...` and is explicitly excluded.

All follow-up evidence is in `tmp/dx12-profile-20260906/`: clean run directories,
`logging-comparison.json`, `timespy-queue-profile` (ETLs, raw PCs, queue/CPU/wait
summaries and clock alignment), preserved `logging-mutex.patch`, stress-test
logs, `logging-sync`, `kmd-sections.json` and deployment logs. Work remains on
root `fe8cb4a` with the nested source heads recorded above. No commits or pushes
were made. The engine patch and version stamp were reversed exactly after the
diagnostic profile. The original signed **.266/oem50.inf** package was restored,
and the experiment package oem51 removed. Guest boot at 19:01:09 local time reports PnpStatus OK;
original KMD/UMD11/UMD12 hashes match byte for byte and the desktop capture is
intact. The restored Windows engine and release UMD12 rebuilds also pass; that
rebuilt UMD12 is `07198743B4C7`, **not deployed**. The accepted `ADC0...` binary
remained selected at that restoration. The Windows KMD build/package output
then still contained the preserved .267 experiment, distinct from the deployed
and source version at that checkpoint.

Final restoration checks completed with status 0, archive/export, matching
workload settings and original loaded DLLs: **249.107391 FPS Fire Strike /
108.3311 FPS Time Spy**. These are restoration checks, not gains. Restored Time
Spy is lower than both focused logger-comparison arms; this variation reinforces
that the candidate's -1.43% difference is not an established regression. No
further repetitions were used to select a favorable result. Final source and
deployment inventories at that restoration were `source-final.json` and
`deployed-final.json`; only ROADMAP and this report were then changed, with all
nested trees clean. Task-owned benchmark/profile tasks were disabled and no
task-owned ETW logger remained active. The Steel/capacity continuation below
supersedes that source and deployment inventory.

## Vulkan control and remaining limits

Steel Nomad **Vulkan** is the requested control. The first CLI attempt lost
focus; subsequent observers are hidden and start before the benchmark. The
next standard-definition attempt failed during swapchain acquisition with
`VK_ERROR_DEVICE_LOST` and workload status 10000. A third attempt after
restoring .266 and rebooting (`steel-vulkan-restored`) reproduced the same
acquisition failure, with original ICD/UMD11 loaded. It has an archive but no
valid metric/export. None is a valid control score. The owner's near-saturating
observation has not yet been reproduced by this harness; these failed runs do not measure Vulkan throughput and do not
justify blaming the host/Vulkan execution path.

Every benchmark explicitly sets `HELIOS_WSI_ASYNC_PRESENT=1` and
`HELIOS_RETIRE_FEEDBACK=1`. Persistent machine/user overrides for those two are
unset, matching the starting state; the unchanged ICD enables both by default.
All work retains async WSI, retire feedback, stock virglrenderer 1.3.0-2,
WDDM 2.1, exact runtime admission, actual GPU completion, resource epochs,
staging/barriers, submission batching and independent Present/scanout/copy
release. There are no GPU-idle waits or skipped synchronization. No launcher
or host renderer changes were made. The broader ownership, failure reporting,
sharing/lifecycle and allocator-reset gaps listed above remain unresolved.

## Steel Nomad repair and transport retry candidate

The failed acquire followed a consumer-copy wait exceeding 32 ms. The repaired
UMD11 captures one fixed DXVK submission at the existing explicit copy flush;
the new v2 export distinguishes pending from failure and never flushes while
waiting. WSI sleeps in bounded slices and permits source recycling only after
confirmed copy completion. Surface cancellation and errors retain the read/image.
The CS-error latch and before/after device-status checks prevent cleanup signals
from becoming false success. See `HPS2_REFACTOR.md` for the complete contract.

The .268 package contains release UMD11
`245D1BC36A61B7B92E83CE5D90F06786544419170B42BB0D534F1913F370D66A`,
unchanged UMD12 `ADC0B0EA6F1CEB0C964A37EE6CEC74CFA8B178BB2B6B5460B5AFAB8BF01B19D0`,
and uses ICD `3349607BE95819BD7DDA37F3424CB4D91607659174A80E365911B527B296A078`.
KMD executable sections are identical to .266; its signed .268 SYS is
`B27F9E02092259CE7B2E1821D5B47DAAB10AE6D9EC61F3488CE6F23D32F31FCC`.
Two consecutive dry review rounds preceded deployment. The finite-work probe
`candidate-completion-exit` passes with exact delayed producer 602, matching
loaded binaries, vehicle LIVE, pending then completion and original-image
reacquisition. Steel `steel-vulkan-fixed` completes with status 0 and export;
its 4814 successful helper Presents rule out the suspected persistent stall.
The final window-close cancellation retains a read (`wait_cancel=1`), and does
not establish fault teardown. No owner moving-scene acceptance was received.

Two Fire Strike attempts subsequently failed `SetFullscreenState` with
`DXGI_ERROR_NOT_CURRENTLY_AVAILABLE`, before rendering. The owner confirms no
interaction with either workload. Guest capture tasks and interactive observers
are now disabled; host VNC supplies captures and win MCP session 0 supplies
profiling. The benchmark itself still uses an interactive scheduled task.
Clean isolated runs completed at 244.769699/112.164719 FPS. An earlier post-fix
Time Spy completion at 91.604279 FPS is retained but is not selected as the
comparison baseline given that desktop-isolation uncertainty.

Further exact-DLL stack attribution of the original Time Spy profile locates
**14 graphics-worker QueueFull sleeps, 92.846 ms total**, in one second of
CSwitch stacks. The compute worker has one, 5.934 ms. Return RVA `0x817b9` is the
Helios completion `vkQueueSubmit2`, proved by disassembly of the loaded UMD12;
the KMD stack reaches the retry-only `KeDelayExecutionThread` in
`submit_venus_async_inner`. This is a subset of the post-execution wait cost,
not GPU time, a removable frame budget, or a DX11 gain.

The fresh isolated .268 profile, `timespy-space-before-profile`, corroborates
that target: no lost CPU events/buffers, 8.017645 s, 910 graphics callbacks and
7279/1819 graphics/compute EXECUTEs. Graphics post-execution time is 1.401752 s,
including 734499 us waiting. Its one-second stack slice attributes 12 graphics
completion-submit kernel delays totaling **104056 us**; compute has one,
8244 us. The full-run, driver-global QfRet delta is 1762, with in-flight high
water 21 and parked high water 24. These counters do not locate a GPU bottleneck
or prove how much faster an event will make the application.

The .269 candidate adds an adapter-owned NotificationEvent. It was measured with
`SubSpaceWake=1`; .270 source now defaults to that value. `SubSpaceWake=0`
preserves timed polling. When enabled, registration is
lazy after the first QueueFull; a fresh protected retry precedes any wait. Only
a failed protected enqueue clears the event. Successful used-ring reclamation,
parked-capacity reclamation/reaper progress, terminal failure and transport
replacement wake registered callers. The event outlives transport replacement.
Every wake or 1 ms fallback timeout repeats the protected enqueue/capacity
checks and, for tagged submissions, stream revalidation. Untagged submissions
retain the existing owner resolution before the retry loop; this change does
not add per-retry owner validation. It grants no descriptor, GPU/wire completion
or buffer release.

Early wakes must meet both the legacy >5000 failed-attempt threshold and at
least five seconds of monotonic elapsed time before exhaustion. This preserves
the old minimum retry opportunities and no-wake timing; it is not a new hard
five-second deadline or a stronger contention-fairness guarantee. Existing
exact admission, batching, resource epochs and consumer/scanout guards remain.
Healthy counter grading: `QSpOn=1` for the candidate, **QSpErr=0**;
`QSpNtf/QSpWake/QSpTout` are driver-global retry telemetry only. Same-boot deltas
are required; DWM and other clients can contribute.
The .269 implementation passed two consecutive dry final review rounds and a
Windows build. All 213 existing `kmd_logic` tests pass; they do not establish
the new event's concurrency behavior. All four native runtime synchronization
cases pass on .269, including cross-process fencing and independent 65536-word
GPU readback witnesses (`tmp/space-wake-20260906/native-sync/20260906-211252-341`).

| GT1 workload | Isolated .268 baseline | .269, SubSpaceWake=1 | Observed change |
|---|---:|---:|---:|
| Time Spy, DX12 | 112.164719 FPS | 137.724655 FPS | +22.79% |
| Fire Strike, DX11 | 244.769699 FPS | 245.570770 FPS | +0.33% |

Both after runs completed with workload status 0, archive and export. Completed
settings match apart from run identifiers, result paths and reboot-dependent
adapter LUIDs. All 71 Time Spy and 78 Fire Strike settings were checked;
definition hashes, environments and every non-KMD binary hash match.
No queue profiling, ETW or capture
ran during these clean comparisons. Frame time changed from 8.91546 to 7.26086 ms
in Time Spy and 4.08547 to 4.07215 ms in Fire Strike. These are focused single
before/after observations, not confidence intervals; the small DX11 difference
does not establish an improvement.

Same-boot candidate counter deltas are QSpNtf/Wake/Tout = 1487/1388/112 for
Time Spy and 1097/1050/198 for Fire Strike; QSpErr stays 0. The respective QfRet
deltas are 3070 and 2454. Early waking can increase retry counts even while
reducing elapsed wait, so these are activation/error checks, not saved-time
estimates. The adapter-global counts include other clients.

The .269 signed SYS is
`3C3544232555A5812F7F50C4C6C0383B1570D377ECCFBB1274A58DB7B0B389E2`;
release UMD11/UMD12 and Mesa remain the repaired .268 binaries above. The clean
Time Spy process loaded UMD12 `ADC0B0EA…` and Mesa `3349607B…` in session 1.
`tmp/dx12-profile-20260906/space-comparison.json` records the full comparison.
The separate after profile, `timespy-space-after-profile`, has zero lost CPU
events/buffers and an 8.016104 s queue window with 976 graphics callbacks,
7807 graphics EXECUTEs and 1951 compute EXECUTEs. The same exact-DLL completion
submit now reaches the capacity event: the one-second graphics stack slice has
16 such waits totaling **13.239 ms**, versus 12 polling delays totaling
**104.056 ms** before. No corresponding kernel-delay stack is observed after.

| Graphics worker CPU phase, normalized per frame callback | Before | After |
|---|---:|---:|
| Pre-execution region | 5.700 ms | 5.830 ms |
| Vulkan execution region | 0.332 ms | 0.332 ms |
| Post-execution region | 1.540 ms | 0.872 ms |
| Waiting within post-execution | 0.807 ms | 0.122 ms |

The post-execution region fell 43.4%; its waiting component fell 84.8%. The
remaining admission-dominated pre-execution wait is 5.608 ms per callback.
These are overlapping CPU worker phases from separate diagnostic traces, not
hardware GPU time or a claim that this accounts for the entire clean FPS gain.
`space-profile-comparison.json` records normalization and source counts.

Host VNC captured Time Spy's changing scene at frames 5300 and 5649, 2.28 s
apart (`timespy-space-profile-vnc`), after the ETW slice during this diagnostic
run. This supports visible frame progress only; owner shadow/motion acceptance
remains open.

## Default-enabled final package

.270 passed the Windows build and two consecutive dry finalization reviews with
rotated lenses and a completeness critic. Its only production behavior change
from measured .269 is selecting `SubSpaceWake=1` by default; explicit `0` remains
reachable. The evidence comment is at the registry read site and DEFAULTS agrees.
The installer used the explicitly selected repaired release UMDs, and reboot
activated **22.22.270.0 / oem53.inf**, Code 0, boot 2026-09-06 21:38:53.5 +05:30.
The task's registry override was removed; QSpOn=1 with SubSpaceWake absent.

The final signed SYS SHA256 is
`C0C5BEAE6E3B815E52702D531842D491AE956EC78C4224159231E5E16DA2E257`.
UMD11, UMD12 and ICD hashes remain `245D1BC3…`, `ADC0B0EA…` and `3349607B…`
as recorded in full above. `tmp/space-wake-20260906/deployed-default.json`
contains final paths/hashes; `source-default.json` records the eight changed KMD
source hashes, all verified against the Windows build mirror. The install backup
is `C:\ProgramData\HeliosDeployBackups\20260906-213831`. The unrelated Windows
mirror `timespy-broken-shadow.png` was restored after the mirror/build and its
original SHA256 verified.

The four native runtime ordering cases pass again on this default with 65536-word
GPU witnesses (`native-sync/20260906-213959-830`, session 1, exit 0). Host VNC shows
the desktop after reboot. The first final clean .270 runs complete at
**118.746094 FPS Time Spy / 248.231491 FPS Fire Strike**, status 0 and export.
All 71/78 settings match except run metadata, environments/UMD configuration
match, and the non-KMD binaries match the .268 baselines. QSpOn=1 with no
SubSpaceWake override in both invocations. QSpErr stays 0; same-boot QSpNtf/Wake/
Tout deltas are 1470/996/78 and 994/828/150 respectively.

The first final check is **+5.87% DX12 / +1.41% DX11** against the isolated .268
baselines. A single same-build Time Spy repeat then completed at
**136.251602 FPS (+21.47%)**, status 0, archive/export. Its identical KMD and
non-KMD artifacts, boot, environment, UMD configuration and 71 workload settings
were checked; only result-path metadata differs. It again has QSpOn=1 with no
override and QSpErr=0. QSpNtf/Wake/Tout deltas are 1144/1040/64.

The later repeat reproduces the initial .269 candidate's larger improvement;
the lower early run is retained. This remains unexplained run variability;
no owner interaction or unmeasured host fault is inferred. The demonstrated
profile wait reduction is separate from the amount of clean FPS improvement.
The lower final Time Spy run began 111.8 seconds after
reboot, versus 374.1 seconds for the initial .269 candidate and 1598.6 seconds for
the isolated .268 baseline; the final repeat began 630.6 seconds after reboot.
This is a possible confounder, not a proven cause; there was no aligned
background-CPU trace in those clean runs. No interleaved campaign was used.
`space-default-comparison.json`, `space-default-repeat-comparison.json` and
`space-default-repeat-provenance.json` preserve all comparisons and the same-build
check. Further optimization needs another measured bottleneck, not removal of
the admission-dominated wait or a repeat of the rejected experiments above.

The final Steel Nomad Vulkan control, `steel-vulkan-space-default`, completes
with status 0, archive/export, **90.683228 FPS / score 9068**. PID 9872 loaded
the expected repaired release UMD11 and ICD, and recorded 4721 successful helper
Presents with no Render/Present failures. The final window-close cancellation
retains the outstanding read. QSpNtf/Wake/Tout deltas are 20/18/0; QSpErr stays 0.
The earlier repaired .268 control was 93.228233 FPS; no Vulkan performance gain
or hardware-saturation claim is made from these two runs.
