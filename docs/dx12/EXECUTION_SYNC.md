# DX12 runtime admission and exact execution completion

## Allocation-bound producer completion

HPS2 removal and the HE12 execution cutover are implemented. This is the live
producer contract; the [migration record](../archive/HPS2_REFACTOR_2026-09-20.md)
retains dated implementation and validation history. [ROADMAP](../../ROADMAP.md)
owns current defect status. [PRESENT](PRESENT.md#current-presentation-contract)
owns external image ownership, consumer completion and source recycling.

`kmd_logic/src/producer_completion.rs` supplies the production state machine
used by `kmd_render/src/adapter/producer.rs`. Exact runtime allocation/open
associations reach one generation-qualified producer state. Resolve each view
with a paired WDDM2 AcquireHandleData/ReleaseHandleData on the same PASSIVE
thread, releasing outside driver locks; never retain callback-private pointers
or substitute a resource/Venus ID. Bindings retain driver-owned status, avoiding
an allocation pin that would create a teardown cycle. Backing rotation carries
the corresponding binding.

`protocol/include/helios_producer.h` defines ABI v1 for stream registration,
binding, publication, cached status, event waits, retain/release and abort.
Resolve `helios_venus_producer_interface` from the ICD that owns the live
device's Vulkan dispatch, not by module-name guessing. Layout assertions and
`python3 tools/sync-producer-abi.py --check` protect its build mirrors.

Publish only after committing a retained signal operation. Allocation epochs
are independent of stream value namespaces and advance through a completed
prefix, never the maximum of out-of-order retirements. The exact tagged Venus
ring boundary and generation-qualified wire receipt prove completion. Handle
retirement before publication under the same serialization. Abort, stream
closure, allocation destruction and transport reset wake readers with failure;
they never advance completed epochs. Capacity exhaustion and stale generations
fail explicitly. Retirement/wakeup must not allocate at elevated IRQL.

Cached status uses bounded read-only seqlock loads. Pending readers atomically
check/register a referenced event, sleep in user mode, and recheck after waking;
the KMD Escape never sleeps. Contention, cancellation and timeout grant no read
permission. Status and dependencies outlive mappings and in-flight readers.
Releasing a binding cancels only its own waits; its Vulkan device outlives it.

DXVK captures and stamps the same generation/epoch for each staged refresh,
including unchanged SRV bindings. Command lists retain imported dependencies;
the submission worker waits before sparse, transfer or graphics reads with
queue mutexes dropped, and teardown cancels waits before draining workers.
UMD12 uses the retained vkd3d callback FIFO operation described in PRESENT;
the HE12 contract below also covers preceding Queue::Wait without an ECL.
WDDM 2.1, async WSI and independent consumer/scanout release remain mandatory.
Host-loss error propagation and broader failure/lifecycle coverage stay open;
see [ALLOCATOR_LIFETIME](ALLOCATOR_LIFETIME.md) for allocator-specific limits.

## Execution implementation and dated validation

**Allocator update, 2026-09-11:** the native frontend now rotates and recycles
allocator generations when engine retirement references remain. This preserves
pending backing without waiting for GPU idle or treating reference counts as
completion. UMD12 F6D00A83 passes the focused native readback, DXR, no-RT and
four ordering cases. See [ALLOCATOR_LIFETIME.md](ALLOCATOR_LIFETIME.md) for the
exact contract, OOM repairs, provenance and unexercised failure/stress paths;
older pending-reset observations below describe the preceding implementation.


**Current source policy:** the owner-authorized renderer fork replaces the
feedback workaround with authenticated wire completion. See the current contract
below and the [native DGC contract](SUBSTRATE.md#native-dgc-contract).
Older deployment receipts remain historical.

**Current native DXR deployment, 2026-09-11:** UMD12 `057934F9…` passes all four
native ordering cases after the completed Port Royal run, each with 65,536 exact
readback words. Loaded system-runtime, native UMD and ICD identities are recorded
in `tmp/dxr-native-admission-20260911/final-native-sync/`. The .271/oem54/WDDM2.1
KMD and 43394BBD ICD are unchanged. Port Royal has 91,993 demo and 41,715 GT1 pending
allocator Reset diagnostics. Source still defers allocator release to the fence
worker, and Reset returns without resetting while internal references remain.
A completed benchmark cannot distinguish delayed retirement from premature reuse;
that focused native lifetime investigation remains open. No synchronization or
backing-retention policy changes are part of this DXR admission increment.
Time Spy, Fire Strike and Steel Nomad Vulkan also complete on that same stack;
[the control receipt](DXR_SERIALIZATION.md#completed-regression-controls-2026-09-11)
records settings, loaded identities and changing host-VNC frames.

**Earlier deployment, 2026-09-09:** the paired local renderer and .271/oem54
guest package are active. All four native ordering/readback cases pass using
authenticated wire retirement, including the cross-process signal case with
both process module identities verified. Time Spy, Fire Strike and Steel Nomad
Vulkan complete with exported results and changing frames; owner visual
acceptance remains pending. [The native DGC archive](../archive/NATIVE_DGC_2026-09-20.md) records exact hashes,
updated system runtime versions and evidence. Pending allocator Reset diagnostics
remain unresolved, as do the broader ownership and host-loss boundaries below.

**Earlier deployment, 2026-09-08:** UMD12 `BE9D0EBE…` adds GPU-predicated
single-sample CopyTiles with allocator-owned scratch, internal query suspension
and required queue continuations. No new CPU prefix wait is needed. Host
readbacks pass; native tiled commands remain blocked at tier0. Native IA and
all four existing ordering cases pass on this artifact, with authenticated
completion, exact loaded runtime/UMD/ICD identities and a zero-loss loader trace.
The receipt is `tmp/fl12-predicated-tiles-20260908/native-validation.json`.
The two existing allocator Reset diagnostics recur in the native IA check;
this does not settle the allocator/fence-worker question. WDDM2.1 and the
broader ownership/sharing/host-loss acceptance boundaries below are unchanged.
Time Spy, Fire Strike and Steel Nomad Vulkan subsequently complete with exact
identities and changing VNC frames. ROADMAP.md records matching settings and
the unresolved Time Spy GT2/Fire Strike graphics slowdowns. Completion is not
performance or owner visual acceptance; no new synchronization shortcut was
introduced in response to the measurements.

**Preceding deployment, 2026-09-08:** UMD12 `361C9767…` adds isolated IA continuation
on the unchanged .270/oem53.inf/WDDM2.1 stack. Native IA GPU readback, query
continuation and a pending public-list Reset behind an unsignaled dependency
pass; another queue submits and releases that dependency before final completion.
The receipt is `tmp/fl12-indirect-ia-20260908/native-ia-validation.json`.
The worker releases the Vulkan queue mutex for its exact prefix wait and owns
private recording storage until the last successful submission retires.
HE12 admission is still one operation and its completion follows every generated
draw and suffix. Host-loss retirement and the pending allocator/fence-worker
question remain open; two existing allocator Reset diagnostics were observed.
See [INDIRECT_EMULATION.md](INDIRECT_EMULATION.md#ia-continuation-implementation-and-validation).
The four older ordering controls and benchmark results below are separate
evidence until rerun on this build.

**Preceding deployment, 2026-09-08:** UMD12 `9BDA548C…` is hotplugged on the same
.270/oem53.inf/WDDM2.1 stack. All four native ordering cases pass on that exact
build; both process identities are verified in
`tmp/fl12-sparse-compat-20260908/native-validation-9bda.json`. See ROADMAP.md and
[SPARSE_COMPATIBILITY.md](SPARSE_COMPATIBILITY.md) for complete provenance and
separate benchmark/visual limits. The required queue continuations are deployed,
but native MSAA tiled work remains unreachable at tiled0.

**Earlier deployment, 2026-09-07:** KMD .270/oem53.inf remains Code0 with WDDM2.1;
native UMD12 candidate `CB48D9DB…` is hotplugged with unchanged UMD11 and ICD.
All four existing native ordering cases pass again in session1 on this candidate,
including both producer and consumer 65,536-word GPU readbacks and the gated
negative intervals. `tmp/fl12-audit-20260907/native-sync-cb48d9db/` retains the
same verified probe executable `1AA1853E…` and source `45345226…` used for the
preceding6344 check. The sibling `fl12-sync-loader-cb48/`
zero-loss ETW trace identifies parent PID2056 and shared-fence child PID5544
loading exact candidate/system-runtime/ICD artifacts, with no WARP or app-local
vkd3d. Async WSI and retire feedback remain1. These FL11_0 regression checks do
not exercise sparse/DXR commands or close the broader obligations below. The
independent `native-sync-cb48d9db/root-validation.json` verifies 35 evidence files,
all four readback cases and both exact loader identities. The PnP restart used
for this hotplug changed the adapter LUID to `042a5ac7`; no reboot was needed.
DriverStore still carries the older packaged UMD12, so this is not a package
upgrade or cold-boot validation.

**Earlier HE12 baseline:** the .266 implementation is retained in .270/oem53.inf,
with the unchanged release UMD12 and a reviewed Steel Nomad Vulkan vehicle-copy
repair in UMD11/Mesa. .268 changed only the KMD version stamp; .269 adds a
transport-capacity notification whose wake grants only another protected enqueue
attempt. It grants no HE12 admission, GPU completion or consumer release. All
four native ordering cases pass on .269 and .270 with independent GPU readback witnesses.
The first .269 clean Time Spy comparison improves 112.16 → 137.72 FPS; the
first .270 check is lower at 118.75 FPS, then one same-build repeat reaches
136.25 FPS. The larger improvement reproduces later in the boot; the lower
early run and unresolved variability remain explicit. Final Fire Strike is
248.23 versus 244.77 FPS.
See `docs/PERFORMANCE_FEEDBACK.md` for settings, artifacts and limits. .270
selects the measured capacity-wake default and is active with the override absent,
Code 0 and the visible desktop. The Vulkan control completes at 90.68 FPS, and
the same-build Time Spy repeat also completes. Ordering acceptance does not
establish a stable FPS gain or broader ownership/failure-path correctness.
The original
.266/oem50.inf was deployed with the updated Mesa ICD after two
consecutive dry independent review rounds. Guest reboot and Code 0 are verified;
D3D12 remains enabled and the desktop is visible. All four native runtime ordering
cases pass on .266 (session 1, GPU readback witnesses). The owner confirms
realtime Time Spy shadows are fixed and observed approximately 100 FPS in their
benchmark. The separate instrumented GT1-only comparison completed at 74.26 FPS
versus 20.03 on .265 (3.71 times), with async WSI enabled; settings equivalence
with the owner's run is unproven. The implementation uses the shipped ICD feedback workaround with stock
virglrenderer; no launcher or host renderer change is required.
This is the HE12 v2 successor to the
sampled ECL bridge. It keeps `WddmSurface::Wddm2_1GpuMmu`, the existing software
scheduler, submission workers, Present ownership and scanout protection.
The native probe covers only its exercised ordering cases; the owner supplies
the shadow acceptance at recovered throughput. Broader acceptance remains below.

## Required queue continuations

The new engine candidate distinguishes primary, compute and graphics command
streams. A copy may require a more capable Vulkan queue even on a D3D12 copy or
compute list. Depth buffer/image copies require graphics without maintenance10;
MSAA depth image copies and internal attachment writes always require graphics.
Advertised format bits cannot replace enabling the relevant Vulkan feature.
Queue selection happens before recording the operation's barriers and commands.

Two optional primary streams leave capacity for mandatory compute then graphics
continuations. A stream never moves back to a less capable queue. Pools are lazy
and allocator-owned; allocation/begin/end errors retain their HRESULT and make
Close fail. Failure to prepare a replacement leaves the original stream intact;
failure ending the original is fatal even for an optional split. No required
operation silently consumes an unavailable stream slot.

The split drains pending transfers, ends physical rendering/conditional state,
invalidates bindings and preserves the logical root/query/predicate state. Meta
indirect/predicate setup stays in the current stream instead of moving into an
initializer from the wrong queue family. Unvirtualized active queries that cannot
cross the operation are explicit failures. MSAA internal shader work likewise
refuses unvirtualized statistics scopes; complete cross-backend query behavior
and OOM/failure injection remain open.

Submission batches coalesce only equal queue tags, including the low-cost
staggering path. Existing GPU timeline edges order each family transition and
the final primary/serializing boundary. Locks progress transfer to compute or
graphics, or compute to graphics; graphics does not acquire a less capable queue.
Allocator-owned views, scratch and command pools retire through existing GPU
completion. HE12 admission, producer feedback, consumer release, present/scanout
ownership and the unresolved pending-reset/fence-worker lifetime question are
unchanged. Native completion of the new continuations remains unexercised.

The candidate's host-only queue/copy tests and discovered stock sparse MSAA
failures are recorded in
[FEATURE_LEVELS.md](FEATURE_LEVELS.md#msaa-candidate-and-stock-host-boundary).
That 8C747 record predates the owner-authorized committed fallback. Current
host engine tests exercise the D16 depth-write shader through compatibility
backing. Raw D32 copies now explicitly refuse after the expanded test demonstrated
special-value bit loss. Native MSAA/queue-continuation behavior remains unexercised
at tiled0; see [SPARSE_COMPATIBILITY.md](SPARSE_COMPATIBILITY.md).

## Contract and ordering

There are two different edges. A runtime queue wait must prevent engine work
from starting early. A runtime queue signal must follow the actual engine work,
including split submissions and any engine fallback queue. A sampled Venus wire
fence could precede worker execution and supplied neither guarantee.

For each ECL, `forward12/queue.rs` performs the following on the entering DDI
thread, under the queue's context-operation mutex:

1. Create an unnamed, initially unsignaled manual-reset event. The engine
   prepares and retains the complete command batch, duplicates that handle,
   and reserves its stream value in the same FIFO commit as the work.
2. Submit the 24-byte `HeliosD3D12SubmitCmd` v2 through `pfnRenderCb` on the
   exact runtime context created for that queue. The command carries the
   registered stream handle, value and cookie; zero and v1 are refused.
3. Call `pfnSignalSynchronizationObject2Cb` on that context with
   `SignalAtSubmission | EnqueueCpuEvent`, `ObjectCount=0`, no broadcasts and
   the owned CPU event. Then release the caller's event handle.

The event is queued **after** Render and signals at submission, before packet
completion. Using completion here would deadlock: the worker would wait for the
same work it has yet to execute. Microsoft defines these flag bits and event
restrictions in [D3DDDICB_SIGNALFLAGS](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dukmdt/ns-d3dukmdt-_d3dddicb_signalflags)
and the exact context/event fields in
[D3DDDICB_SIGNALSYNCHRONIZATIONOBJECT2](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dumddi/ns-d3dumddi-_d3dddicb_signalsynchronizationobject2).
The public contract supports this ordering. The native runtime suite below now
exercises software fence waits/signals through the created contexts, including
independent GPU-write witnesses. See `DDI_REFERENCE.md` section 10.3.

The engine worker waits for admission before processing that ECL. It holds no
Vulkan queue lock while waiting, so a producer on another queue can satisfy a
wait-before-signal dependency. Its 100 ms event-wait interval only checks device
loss/cancellation; it never constitutes completion. After execution, it joins
the engine's external/serializing waiters and submits an `ALL_COMMANDS` signal
on the exact registered stream, alongside the existing engine timeline signal.
Existing command batching, split submits and allocator/fence-worker lifetimes
remain in force. There is no new caller-side worker drain or GPU-idle wait.

Present producer callbacks use their own event/HE12 pair before HEPR. This
covers `Queue::Wait -> Present` with no intervening ECL. The callback retains
the exact resource and allocation binding. Its publication epoch and stream
reservation share the queue FIFO lock with ECL reservations, so concurrent
publications cannot invert signal values. HEPR and its runtime present
descriptor retain their existing identity and ownership duties; inability to
submit that required identity is now an error.

## Kernel proof and lifetime

### Sparse mapping candidate (2026-09-07, deployed but unexercised)

The current [compatibility extension](SPARSE_COMPATIBILITY.md) also prepares
mapping operations for committed fallback images. After complete validation it
commits a zero-bind operation through the same admission/event/stream path.
CopyTileMappings involving either fallback endpoint cannot mutate real sparse
maps. Ignored mappings retain exact queue ordering and GPU completion; they do
not imply aliasing, sparse unmapping or a CPU completion shortcut. The committed
image owns its allocation independently of mapping heaps. Native exercise of
this extension remains blocked by the reported tiled tier0.

`forward12/tiles.rs` and the private engine `helios_sparse.h` apply the same
context-operation mutex, FIFO stream reservation, HE12 Render and submission
event to UpdateTileMappings and CopyTileMappings. Preparation validates counts,
coordinates, range flags and heap bounds before committing owned descriptions.
The worker waits for the exact operation's admission before inspecting source
mappings or changing destination mappings. A Queue::Wait followed by mappings
therefore cannot change bindings before the runtime releases that wait.

Consecutive already-admitted mappings retain sparse batching. Before waiting
for another admission, consuming a different operation, or sleeping on an
empty FIFO, the worker submits pending sparse binds and emits the highest
covered HE12 stream value. Mapping-only work needs this boundary even when no
command list follows it. QueueBindSparse joins the original queue timeline;
when a different physical sparse queue is necessary, its completion is joined
back before later queue work. No GPU-idle wait or submission-worker drain is
introduced.

Pending updates retain their heap and destination, and copies retain both
resources. Copy source bindings are snapshotted before destination mutation,
including overlapping self-copy. Each mapped native tile owns its heap.
Replacing/unmapping a tile transfers its former heap reference into the sparse
completion record; destination resources and retired heaps survive until the
fence worker observes successful completion. Resource destruction releases
remaining mapped heap references after destroying its Vulkan resource.

A refused commit/admission or failed sparse submission removes the device;
neither publishes invented completion. Failed retirement without a proven
completion/quiescence witness quarantines references instead of releasing
backing. This is a deliberate failure-path leak, not a solution to the existing
host-loss/disconnect callback limitation below. Two consecutive dry whole-diff
review rounds preceded candidate6344 hotplug. Native tiled probes stop at tier0;
ordering, lifetime and failure-path validation remain unexercised. Advertised
tiled support remains NONE because the full contract, including MSAA CopyTiles,
is not complete. See `FEATURE_LEVELS.md` for per-path status and counter grading.

`DxgkDdiRender` authenticates the stream/cookie against the context's exact
`ContextContext -> DeviceContext -> hKmdProcess` chain. Each context binds one
generation-qualified stream, and each new Render must strictly advance its
value. No PID, resource geometry or private fence object supplies authority.

The KMD requests 104 private bytes per context. The existing Present prefix
occupies bytes 0..31 and the flip/snapshot record occupies 32..87. A separate
16-byte execution record occupies 88..103. Compile-time assertions prevent
overlap. A same-context batch merges to the largest exact stream value. A
different context/generation cannot inherit the previous record. Submit reads
the record without consuming it, preserving the proof for preemption replay.

`kmd_logic::execution_completion::{Record, Wait}` is production logic used by
the Render and retirement paths. Submission checks already-observed retirement
under the same lock that publishes pending waits. Later stream retirement
latches completion before registration teardown can erase the live slot.
Only the matching generation/value can complete the wait. Execution packets
stay in the retryable WDDM FIFO even if already ready, and explicitly request a
completion DPC so they do not depend on a future transport interrupt.

Legacy dead-stream discharge and timed head rebasing cannot discharge an
execution entry, including a packet also carrying Present copy obligations.
A FIFO overflow is terminal and retains outstanding obligations for reset;
locally detected missing transport/device loss never reports successful execution.
Host failures hidden behind an upstream retirement callback remain the gap
listed below. Preemption or reset discards the cancelled waiter without reporting
DMA completion.

Queue destruction removes the public queue slot, then destroys the runtime
context while keeping worker admission and the engine stream alive.
[`pfnDestroyContextCb`](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dumddi/nc-d3dumddi-pfnd3dddi_destroycontextcb)
drains queued work; cancelling an unsignaled admission before that drain would
discard valid work still awaiting runtime submission. After the callback returns,
engine cancellation precedes Release joining the workers. Context-destroy failure
also removes the engine device. Failed submission/admission removes the device and
aborts the corresponding producer publication. A failed submitted operation
does not release resources whose GPU use remains unproven.

## Fence authority and remaining gaps

**Scope clarification:** WDDM 3.2 native GPU fence objects are an optional,
separately advertised feature (`DXGK_VIDSCHCAPS::NativeGpuFence`). They are not
required for this WDDM 2.1 repair and their absence is not an acceptance blocker.
See [native GPU fence objects](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/native-gpu-fence-objects).
Ordinary monitored fences already have GPU virtual addresses in WDDM 2.0;
Microsoft also documents a software signal path for engines that cannot write
those addresses. A nonzero fence GPU address therefore does not identify the
WDDM 3.2 feature. See [context monitoring](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/context-monitoring).

The software WDDM runtime owns fence values, initial values, CPU signals,
rewinds, shared handles and queue waits. `D3D12DDIARG_CREATE_FENCE` supplies GPU
placements, not a KMT synchronization handle, CPU mapping or initial value. The
previous private `ID3D12Fence(0)` and guessed signal watermark were removed.
The UMD's opaque fence state now retains only its exact creating-device
association.

| Path | Current implementation and acceptance boundary |
|---|---|
| Zero-VA software runtime fences | Runtime owns values; HE12 admission/completion orders actual engine work. Native cross-queue/CPU/shared-fence cases pass on .265 with independent producer/consumer witnesses. Broader teardown and application acceptance remain open. |
| Nonzero monitored-fence GPU placements | Currently `FenceGpuVaRefused`, `E_NOTIMPL`. This is an unsupported older monitored-fence path, not a WDDM 3.2 requirement. Direct GPU access would require valid backing and fence writes; the documented software alternative requires exact runtime synchronization-object association. Neither may use a private shadow timeline as the application's fence. Whether this path is needed depends on the negotiated contract, not the presence of a GPU VA alone. |
| Direct D3D12 `pfnSignalFence` / `pfnWaitForFence` calls | These predate WDDM 3.2 native fences. Directional entry counters are retained; valid calls currently fail with `FenceNativeRefused` and `E_NOTIMPL` (the counter name is misleading but retained). These entry counters were zero in the deployed .264 Time Spy logs. The required software-context route must be validated; the 3.2 feature need not be implemented. |
| DX12 to DX11 shared-image ownership | Producer completion exists. General matching Vulkan external queue-family release/acquire and consumer-release tracking remain unimplemented on the vkd3d side. This needs exact layout/owner/resource-use tracking and matched barriers; a later queue signal cannot provide it. |
| Host device loss / render-server disconnection | Upstream `vkr_queue_thread` retires after any non-timeout wait result, including device loss, and the proxy forces retirement after server disconnection. The callback carries only a sequence number, not failure status. The guest cannot infer failure from that successful-looking retirement alone. A complete solution needs an error-bearing observation across this boundary. Reusing feedback for successful GPU completion does not by itself close this gap. |
| WSI | Keep `HELIOS_WSI_ASYNC_PRESENT=1`. The explicit unnamed NT semaphore/value dependency and separate copy/recycling guard remain unchanged. |

`Umd12EclSubmit`, `Umd12EclDrain` and `Umd12EclFence` registry switches are
retired. Existing inventory positions report fixed behavior and the appended
`ExecutionSyncVersion=2` distinguishes it. Retired sample/shadow counters stay
at their diagnostic indices with value zero. New counters distinguish ECL
worker commit, HE12 acceptance, admission-event queuing and refusal. None is
pixel or runtime-fence correctness evidence by itself.

## Time Spy versus Steel Nomad

The owner originally saw stale in-frame shadows in Time Spy and not Steel Nomad DX12.
UL documents Time Spy overlapping SSAO, light culling and unshadowed lighting
with shadow rendering, plus render-target heap aliasing. This makes queue
dependencies and aliasing relevant investigation targets.
[Time Spy engine](https://support.benchmarks.ul.com/support/solutions/articles/44002136148-time-spy-engine).
Steel Nomad also uses async compute, for its first volume-illumination pass,
and uses a different contact-shadow path.
[Steel Nomad engine](https://support.benchmarks.ul.com/support/solutions/articles/44002528067-steel-nomad-engine).
These differences are leads, not proof that a particular shadow pass caused
the observed defect. Whole-frame ordering and stale in-frame shadows need
separate visible acceptance.

## Authenticated wire completion (2026-09-09 source candidate)

The owner authorized the renderer fork and removal of the native-fence workaround.
The renderer's internal markers are ordinary, non-exportable VkFence objects;
guest external fence APIs remain unchanged. The measured delay belongs to the
unnecessary SYNC_FD-exportable marker path, not proof of an active 10 ms polling
sleep. Successful callbacks now require a successful Vulkan completion result.
Reset/submit/wait failures and pending teardown do not manufacture completion.

Mesa's retire worker waits on KMD's exact wire response. Exported Win32 timelines
have no feedback slots or counter polling; STREAM_FEEDBACK escape 0x14 is removed
and its number remains reserved. KMD `execution_completion::Progress` has one
wire-retired watermark, advanced only by generation-qualified receipts. Producer
publication and HE12 waits use this progress; consumer claims, scanout leases and
backing recycling still require their own release conditions. Initially-zero,
GPU-only stream registration and import/CPU-signal refusals remain.

The [native DGC archive](../archive/NATIVE_DGC_2026-09-20.md) records the
implementation, tests and measured host marker times. Deployment progressed
after that initial candidate; current loaded identities are in ROADMAP and
activation rules are in [TOOLCHAIN](../../TOOLCHAIN.md#12-build-the-paired-virglrenderer-and-venus-protocol-forks).
Host device-loss/disconnect error delivery and broader cross-API ownership
remain separate from completed producer implementation and allocator repairs.

## Historical .265/.266 validation (superseded implementation)

The evidence below belongs to the retired feedback workaround and its earlier
stock-renderer constraint. It establishes no acceptance for the new wire-only
candidate. The owner's .266 shadows/~100 FPS acceptance remains separate from
the instrumented74.26 FPS run and later builds.

## Validation and runtime acceptance

The .265 slowdown now has host-side evidence. A render-aligned GT1 trace at
20.03 FPS shows direct/compute DMA medians of 10.19/10.58 ms. The async renderer
callback is enabled and QEMU dispatch follows it promptly. A standalone GPU
fill followed by the same empty queue marker reproduces an 8.060 ms average
work-submit / marker-submit / wait sequence with `SYNC_FD`-exportable fences
on NVIDIA 610.57.04, versus 0.329 ms for ordinary fences. This measures the
complete sequence, not `vkWaitForFences` alone. The private host patch proposal
was withdrawn at that time; the owner authorized the new fork on 2026-09-09.

The then-deployed solution was the
[archived WS2 feedback workaround](../archive/ROADMAP_HISTORY_THROUGH_2026-09-05.md)
(lines 3096–3127), `HELIOS_RETIRE_FEEDBACK`, then on by default. That
`vn_renderer_helios.c::helios_sync_retire_thread` read the exact semaphore's
GPU-written counter and advances its external sync; the captured .265 run
already reports `retire_fb fast=4607 fallback=0 wire=0`. On .265 KMD producer/HE12
progress still advanced only from the tagged AsyncVenus response. The .266
candidate implements the separate exact GPU completion edge described above.
213 production logic tests (including seven new feedback/receipt/race cases)
and 14 protocol tests pass. Windows Mesa and the normal signed .266 KMD package
build. The conservative startup unwind gate is 5248/17936 bytes, including
saved registers and return addresses; the prior 4936 figure counted stack
allocations only. Build receipts and the gate are in
`tmp/feedback-completion-baseline/`. Two independent rounds with different lens
compositions are dry. Deployment and boot receipts plus the passing native suite
are in `tmp/dx12-sync-266-runtime/20260906-155029-223/` and its parent directory.
The completed GT1-only run in `tmp/dx12-sync-266-perf/` reports 74.26 FPS versus
20.03 on .265, using the same definition/options with async WSI enabled. This run
used `HELIOS_PERF=1`, `--debug-log` and a four-second ETW slice. Render
PID 5556 loaded the new content-hashed ICD and packaged release UMD12. The final
reconstructed counters are `stream_fb accepted=49240 wire_retired=720 rejected=151`
and `retire_fb fast=50111 fallback=0 wire=0`. Refused notifications leave KMD
completion on the wire; this counter does not distinguish older removed receipts
from teardown/refusal and must not be described as zero or classified without
additional evidence. The adapter remains Code 0 after the benchmark. This is
one completed before/after benchmark comparison, not a broader performance claim.
The ETW artifact's mixed UTC offsets produce invalid durations in the earlier
parser; those durations are excluded from acceptance. The exported 3DMark XML
is the FPS source. The owner subsequently confirmed the realtime shadows are
fixed and observed about 100 FPS in their own benchmark; this is separate from
the instrumented GT1 measurement. Mixed sharing, unchanged bindings,
rotation/resize, teardown and WSI stress remain runtime acceptance work.
The prior 20 FPS visual check in `tmp/dx12-sync-265-perf/` was insufficient
because low throughput could conceal a timing-dependent race.

Source checks for this change: 206 `kmd_logic` tests, including eight execution
boundary cases; 14 protocol tests; UMD12 host Clippy with `-D warnings`; Windows
vkd3d, release UMD11/UMD12 and normal KMD package builds. The .265 normal KMD
stack gate measures 4936 bytes against a 17936-byte ceiling. The native probe
builds with MSVC `/W4 /WX`. Build logs, source snapshots and artifact hashes are
in `tmp/dx12-sync-20260906/`. Builds are not runtime acceptance.

`tools/d3d12_sync_probe.cpp` selects the Helios PCI adapter through the system
DXGI/D3D12 runtime and checks four direct/compute queue cases: signal before
wait, wait before signal, future CPU signal after a fence rewind, and a CPU
signal from another process opening the exact inherited unnamed shared-fence
handle. Every case requires 65536 exact readback words for its distinct epoch
in each of two independent producer/consumer witnesses.
Negative intervals must neither signal nor alter the readback sentinel through
the unsignaled wait. The producer is already queued for both CPU gate cases;
otherwise the negative interval would test only the consumer's ready fence.
Positive waits require an actual event and correct GPU bytes. A timeout is failure. The wrapper records binary/source
hashes, session, stdout/stderr and a JSON result.

Build without executing:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\d3d12-sync-acceptance.ps1 -Mode Build
```

After independent review and deployment of the matching KMD plus explicit
release UMDs, register/run in the interactive guest session (never session 0):

```powershell
$action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -ExecutionPolicy Bypass -File Z:\tools\d3d12-sync-acceptance.ps1 -Mode Run'
$principal = New-ScheduledTaskPrincipal -UserId 'Rupansh' -LogonType Interactive -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 2)
Register-ScheduledTask -TaskName 'helios_d3d12_sync_acceptance' -Action $action -Principal $principal -Settings $settings -Force
Start-ScheduledTask -TaskName 'helios_d3d12_sync_acceptance'
```

Native execution acceptance passed on .265 in interactive session 1:
`tmp/dx12-sync-265-runtime/20260906-040507-991/result.json` records all four
cases, exit 0, with both producer and consumer witness checks. That process
loaded the .265 DriverStore UMD12 and logged `ExecutionSyncVersion=2`, exact
HE12 boundaries and queued admissions; direct fence/refusal counters remained
zero. Native runtime software-fence routing is therefore exercised, including
the cross-process CPU signal. This does not prove all resource-sharing or
presentation paths.

On .266, the owner accepted realtime Time Spy shadows at recovered throughput.
That acceptance is not new visual acceptance of .270; the current performance
investigation and its remaining visual gate are recorded in ROADMAP.md. Broader
acceptance still requires:

- Queue/device teardown while waiting, producer loss, cancellation and normal
  completed teardown; no stuck worker, synthetic DMA completion or new crash.
- Native Time Spy GT1/GT2 coverage with visibly changing frames and correct
  moving shadows; the owner's .266 shadow pass did not specify individual
  subtests. Steel Nomad DX12 was the historically reported unaffected rendering
  control; the current performance investigation explicitly uses Steel Nomad
  **Vulkan**, as recorded in `docs/PERFORMANCE_FEEDBACK.md`.
  Collect runtime/ETW ordering evidence for a regression. Do not infer rendering
  correctness from scores or introduce a focus-stealing capture during 3DMark.
- Mixed DX11/DX12 and cross-process resource sharing, unchanged SRV bindings,
  staged-refresh epoch accuracy, buffer rotation, resize and async WSI with
  delayed source completion. Producer completion and consumer release require
  separate evidence; the external ownership gap above cannot be waived.

The independent reviewer resumed and completed the whole-change review. Its
confirmed defect was cancellation of valid, not-yet-admitted work during normal
queue destruction. The repaired drain-before-cancel ordering above was checked
by that reviewer; no further concrete defect survived the callback ABI/scope,
worker ordering, batching/replay, retirement and failure-path review. Windows
vkd3d and release UMD12 rebuilt after the repair, host Clippy passed, and the
matching .265 package was deployed and rebooted. Remaining runtime acceptance
is listed above.

Diagnosis on the deployed .264 stack (interactive session 1) now independently
reproduces an ordering failure: the case-1 completion event signals while the
readback contains `00000000`, where epoch 1 requires `3c6ef372` at word 0.
Evidence: `tmp/dx12-sync-264-diagnosis/20260906-035033-150/`. That wrapper recorded
a null exit code; its stderr contains the explicit readback failure, and the
wrapper now owns the native process handle through exit to preserve the code.
The strengthened CPU-gate case independently proves early execution: with its
future queue wait still unsignaled, the GPU overwrote the readback sentinel with
epoch-3 bytes (`78dde6e4` at word 0). It exited 1 in
`tmp/dx12-sync-264-diagnosis/20260906-040105-502/`. An earlier event/readback-only
version of that case passed because the deliberate 100 ms negative interval
allowed early GPU work to finish; it was insufficient admission evidence.
The .264 Time Spy `umd12-3252-vkd3d.log` also contains 2956 allocator resets
with command lists still awaiting execution. These support an early-completion
defect; they do not establish which shadow resource supplied the old pixels.
The owner originally reported frames from the first 1–10 frames flashing later (example
frame 799), plus black frames, in realtime only; individual image-tool frames
are clean. The owner is the visual oracle for the shadow repair. After a
2397x1517 scanout import failure, the existing QEMU display-size request and
Helios device restart restored actual VNC output at 1280x800; the screenshot is
`tmp/dx12-sync-265-runtime/desktop-restored-vnc.png`. No scanout source change or
arbitrary-resolution fix is claimed. The .266 recovery and owner shadow
acceptance remove the 20 FPS timing confounder. Further performance work must
retain the repaired ordering and the owner's visibly correct rendering.
