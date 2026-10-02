# HPS2 replacement on the existing WDDM surface

**Archived implementation record, 2026-09-20.** The implementation work is
complete. Dated observations below retain their original acceptance limits;
current defect status is in [ROADMAP](../../ROADMAP.md). Active contracts live
in [PRESENT](../dx12/PRESENT.md#current-presentation-contract),
[EXECUTION_SYNC](../dx12/EXECUTION_SYNC.md#allocation-bound-producer-completion),
and [SUBSTRATE](../dx12/SUBSTRATE.md#native-dgc-contract); host activation is
in [TOOLCHAIN](../../TOOLCHAIN.md#12-build-the-paired-virglrenderer-and-venus-protocol-forks).
Relative Markdown links have been adjusted for this archive location.

**Implementation status:** the bounded source cutover is implemented and built.
The investigation below records the starting point; the implementation and
runtime acceptance section at the end records the current contract. The .266
HE12 v2 execution repair and GPU feedback completion workaround are deployed
with stock virglrenderer. All four native ordering cases pass; the owner confirms
realtime Time Spy shadows are fixed and observed approximately 100 FPS in their
benchmark. General mixed-API and lifecycle acceptance remain open. See
[EXECUTION_SYNC.md](../dx12/EXECUTION_SYNC.md) and the current task in ROADMAP.md.

Investigation, 2026-09-05, in `/home/rupansh/helios-vgpu-dx12`.
Inspected root `ba3083917af8a25c76c88eb6727014ae25c6e75c`,
DXVK `8148189e699a53997941852a4690bab656ba9419`,
Mesa `4468ef026359f27b6eeb8d38e45c8e48f0aff13d`, and
vkd3d `f3918d5e40a0e8201b223c001bd5589f667174aa`.
Rediscover these pointers before implementation. ROADMAP explicitly excludes
the abandoned /home/rupansh/helios-vgpu tree as an implementation reference.

## Decision and feasibility

**Use allocation-bound producer completion state in the KMD, its existing
read-only mapping/event infrastructure, and a small common ICD interface.**
Replace WSI's in-process producer discovery with an explicit dependency passed
through its existing helper interface. Keep WddmSurface::Wddm2_1GpuMmu.

This is the smallest credible replacement found for the actual HPS2 consumers.
It retains a private driver synchronization protocol. The KMD owns identity,
publication validation, completion and cleanup; user mode observes status and
requests waits. It needs no new WDDM submission executor, memory manager,
presenter, Vulkan layer or D3D11 DDI table.

**Planning estimate: approximately one concentrated implementation day for
the vertical source change, focused tests and builds.** A few hours is not a
credible estimate for a fully validated migration. Windows runtime acceptance,
fault testing and paired performance measurements may extend it. The estimate belongs to the original investigation; implementation and deployed
observations are recorded below. It is not a runtime acceptance claim.

The earlier queue-admission recommendation was too broad for the requested
budget. Real rendering currently reaches Venus before its outer WDDM metadata
packets. Making those packets control Vulkan dispatch, and proving D3D11
acquisition-notification coverage, is a separate subsystem project.

| Option | Fit for this task |
|---|---|
| Pagefile-backed named mapping or common DLL around HPS2 | Quick mechanical change; retains the resource-to-producer directory and its protocol problems |
| Allocation-bound KMD completion state, cached read-only status, event waits | Recommended bounded replacement; reuses live primitives and keeps per-draw work in user mode |
| Per-resource named fences alone | Still lacks the pending target, producer association and staged-image change notification |
| Runtime-controlled Vulkan dispatch and resource ownership throughout the stack | Larger work; not required to replace this file's current mechanisms |
| WDDM 3.2/native fences | Outside scope; would also change the display/DDI contract |

## Mechanisms that must survive removal

The file is C:\ProgramData\Helios\helios_present_sync_v2.bin: a mapped
32-byte header plus 4096 32-byte slots, **not disk I/O per frame**. Its values
describe announced work; the separately named fence proves completion.

| Responsibility | Current source seam | Replacement |
|---|---|---|
| Publish the dependency for the resource the consumer reads | umd/bridge/dxvk_bridge.cpp, publish_present_order | Validated allocation binding and registered stream/value dependency |
| Order sampled imports | dxvk_context.cpp, heliosEmitImportedWaits | Resource/epoch dependency retained with the command list and enforced before submission |
| Order imported copy sources | heliosPresentWaitBeforeRefresh, including alias-image copies | Wait for the captured source epoch, or consume an explicit WSI dependency |
| Detect stale private images | d3d11_context.cpp, HeliosGateStagedSrvFreshness | Cached announced epoch versus last staged epoch, using mapped loads |
| Stamp refreshed contents | refreshHeliosStagedImages | Stamp the same epoch whose dependency the recorded copy consumed |
| Find foreign producer fences | heliosProducerFence, PID/start/fence-name cache | KMD completion state |
| WSI source-copy dependency | wsi_win32_queue_present_vehicle | Unnamed, retained NT semaphore handle and exact value through the existing helper seam |
| Lifetime and cleanup | DxvkResourceAllocation destructor; WSI image destruction | Allocation/open references, command-list references, terminal status and device teardown |
| File/ACL setup | packaging/windows/Install-Helios.ps1; DXVK mapping code | Remove HPS2 setup after all active consumers have replacements |
| Diagnostics | noteGateFlush, gateFlushCount | Ordinary local counters |

There are **two live publisher call sites**, despite the broad consumer
fan-out: the D3D11 bridge and Mesa WSI. UMD12/vkd3d currently publish nothing
to HPS2. Neither live publisher sets kwaitOrdered=true. That optional skip
branch is not an active producer contract to recreate; stale mapped rows can
still contain historical values until old processes are gone.

The dedicated Present-buffer read/write handshake, scanout-read ledger,
snapshot retention and WSI image recycling are separate contracts. Preserve
them. A producer-ready epoch does not mean a consumer has finished reading.

## Existing implementation and OS contracts

1. **Read-only mapping and cleanup already exist.**
   kmd_render/src/ddi/blob_map.rs::map_nonpaged_page_to_user_readonly uses
   the SEH mapping shim with read-only/non-executable protection.
   escape_map_read_ledger in ddi/escape.rs maps once per owning device;
   mapping.rs and device teardown reclaim user mappings.
   adapter/read_ledger.rs keeps backing memory alive across StopDevice until
   its mappings are gone. Reuse the mechanics, keeping producer status
   separate from scanout-reader accounting and its 65-slot ABI.

2. **Exact asynchronous completion already exists.**
   VirtioGpu::register_present_stream assigns owner/context/generation-scoped
   cookies. Submitted/retired values and marker-boundary logic distinguish
   announced work from host completion.
   vn_queue.c::vn_signal_win32_external_semaphore tags the batch following
   the exact queue ring sequence. Reuse that boundary and retirement, never
   the adapter's newest wire fence.

3. **Allocation lookup and pinning fit WDDM 2.1.**
   Microsoft's [AcquireHandleData contract](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/nc-d3dkmddi-dxgkcb_acquirehandledata)
   and [ReleaseHandleData contract](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/nc-d3dkmddi-dxgkcb_releasehandledata)
   date to WDDM 2.0 and allow a KMD reference to prevent concurrent allocation
   destruction. Both require IRQL at most APC_LEVEL. Use this build's WDK
   types and verify callback availability.
   [GetHandleData](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/nc-d3dkmddi-dxgkcb_gethandledata)
   distinguishes global allocation data from device-specific open data.
   A bare lookup pointer must not escape its protected lifetime.

4. **The UMD seams already have allocation identity.**
   UMD11 resource create/open paths call stamp_dxvk_resource_kmt_handles with
   the actual allocation handle. Bind before state.rs's existing helper can
   substitute its resource handle for a zero allocation handle; that fallback
   is not valid input to an allocation lookup. UMD12's AllocationIdentity carries
   h_allocation; present12.rs resolves the exact presented resource and queue.
   Mesa's NT-memory open path also receives a local allocation handle.
   Extend those seams; PID, geometry, Venus ID and the last active device are
   not substitutes.

5. **DX12 already has a worker insertion point.**
   vkd3d-proton-helios/libs/vkd3d/command.c implements
   d3d12_command_queue_enqueue_callback and
   VKD3D_SUBMISSION_QUEUE_USING_CALLBACK. Its existing swapchain uses this
   for ordered presentation and wait-before-signal handling. It is internal,
   not an exported Helios API: add a narrow bridge entry to enqueue the
   retained completion operation. Do not recursively drain the queue from
   that worker or mistake sample-only queue locking for flushing engine work.

Ordinary monitored fences already work on this adapter.
[Microsoft's native-fence documentation](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/native-gpu-fence-objects)
distinguishes WDDM 2.x CPU-managed waits from WDDM 3.2 native GPU waits. This
proposal uses the existing completion/event route and needs neither native
fence capabilities nor a display-version bump.

## Bounded design contract

### Bind once to the actual allocation

Expose one versioned ICD interface for binding, publication, status sampling,
waiting and release. UMD11/UMD12 provide the exact local WDDM allocation
handle at their resource boundary. Resolve/pin it through dxgkrnl in the
caller's context and validate the Helios adapter and relevant device/open
association. Scope the returned binding to that caller's live device and the
underlying allocation incarnation.

Different opens of the same allocation must reach the same completion state.
Allocation, device-specific open, resource and Venus handles are different
types. Do not cast between them or use the copied OpenAllocationContext::present
metadata as a lifetime pin. Callback reference release requires a PASSIVE/APC
cleanup path, including when completion is observed at DISPATCH.

For staged imports the binding belongs to the external source, even when
DXVK allocates a private destination. Backing rotation must rotate its binding.
Do not recover a missing binding through a global resource-ID search.

### Announce work and complete it at exact host retirement

Each bound resource conceptually exposes an allocation generation, announced
epoch, completed epoch and live/error status. KMD-private state connects each
pending epoch to a validated producer stream and value.

User mode never writes these fields. A cached slot/index plus generation
permits direct loads without per-draw scans or Escapes. Snapshot retries are
bounded; contention is not an unpublished/ready result. Capacity exhaustion
is an explicit error. Size the producer status storage independently of the
existing 65-slot scanout ledger.

Publication accepts an already committed producer operation, including an
operation enqueued on an engine worker whose tagged host submission has yet
to arrive. It also handles completion arriving before publication. Bind
publication and pending waits to the exact stream generation.

Completion advances only through a proven completed prefix on that resource.
Out-of-order retirements must not turn max(retired) into false readiness.
Do not serialize unrelated resources behind this prefix. A later producer
must not inherit or reset another producer's timeline value namespace.

Allocation/device/transport teardown invalidates bindings and wakes waiters
with terminal status. An abandoned publication is failure, not success. Keep
status storage and dependencies alive until mappings, readers and in-flight
work release them; generation checks do not make freed pointers safe.

The mapped page is a **status view of live driver objects**. Mapping it grants
no publication or wait authority; control operations require the validated
binding. Shared driver status memory and a private ABI are explicit tradeoffs
to keep this change small.

### Preserve freshness and consumer execution boundaries

The staged-SRV gate checks the announced epoch, including draws with unchanged
bindings, and retains per-epoch flush deduplication. A refresh copies against
one captured epoch and stamps that epoch. Today's second lookup can stamp
a newer value than the copy waited for; do not carry that race forward.

Keep sampled-import dependencies attached to the command list until submission.
Enforce them on the submission worker before any relevant transfer, sparse or
graphics work reads the source. DxvkCommandList::submit is a local insertion
seam for retained resource/epoch waits. Do not move all waits onto the
application recording thread.

Sample status first. If pending, atomically check-and-register a referenced
event with the KMD, sleep in user mode with cancellation/device-loss handling,
then recheck status after wakeup. Reuse the existing fence-event pattern.
A raw wire-fence event cannot cover an announced stream value whose wire
submission does not exist yet: that predicate needs explicit handling.
Never park inside an Escape, hold the submission mutex while waiting, or
interpret timeout as permission to copy/sample.

### Add one thin DX12 producer hook

At the existing UMD12 present boundary, capture the exact resource binding,
engine queue, stream and value. Enqueue through the vkd3d worker callback seam,
after prior work and before later work. Retain the engine resource/queue and
callback data through execution and cleanup. Signal the common ICD stream
on the actual VkQueue; the existing HEPR correlation can name that stream/value.

Do not add producer-side GPU-idle waits or per-draw/per-resource-use tracking
to vkd3d. UMD12's current sample-only ECL fence is insufficient: the worker may
not have emitted the frame yet. Signal scope must cover the actual final
submission, including split-submit and prior-wait cases.

### Pass WSI dependencies explicitly

Pass the pre-present semaphore's **unnamed NT handle and exact value**, tied to
the source image, through the existing WSI-to-vehicle interface. Import/cache
it for that helper device and retain it through the copy. Record the dependency
for the actual source read. Specify duplication and close ownership; do not
borrow a handle past its lifetime.

**Do not trust the comment saying the caller already waited for the frame.**
wsi_common.c::wsi_helios_present_execute skips WaitForFences while
helios_vehicle_serving is true; the inline path has the same guard. Source
readiness cannot become an always-ready flag.

Preserve the copy-completion/recycle guard. helios_umd_get_present_result is
currently a retired stub returning -1; the live route uses
helios_umd_wait_last_present. Do not delete that route with HPS2 or claim the
named release-fence arm works. This extends the helper seam already in use.

## Performance and scope limits

The fast path replaces repeated 4096-slot lookup/seqlock work and producer
cache/name resolution with direct cached status loads. Pending reads can use
KMD completion directly, bypassing foreign named-fence import and its
user-mode completion relay.

Current Mesa's vn_queue_submission_fix_batch_semaphores CPU-waits for imported
Win32 sync before mirroring it into the host semaphore. Retaining a submission
worker wait therefore does not replace an existing native host GPU wait.

Publication introduces KMD work where HPS2 used a mapped write. Batch/piggyback
where the existing boundary permits, preserve DX11's folded present signal and
existing submission batching. No per-draw kernel query, added whole-queue drain,
unconditional staging copy or per-frame object creation. These give a credible
parity path, **not measured parity**.

The patch spans KMD/protocol, common ICD wrappers, DXVK consumer hooks, the two
existing publishers and the UMD12/vkd3d worker seam. Expect a focused
multi-repository change. Deleting implementations and the installer ACL block
is the final mechanical step.

This replaces producer discovery, readiness, freshness and cleanup. It does
not by itself repair every D3D12 application-fence/CPU-signal/shared-fence DDI
gap. Readiness also does not prove Vulkan external ownership or buffer reuse.
Preserve existing barriers and ownership handshakes. The vkd3d fork has no
generic external queue-family release/acquire implementation to assume: if the
target native-DX12 sharing path requires additional transfers, implement its
exact resource boundary or report that extra work as a blocker to claiming
mixed-API runtime correctness. Never suppress it with a ready bit.

## Implementation acceptance

- Put focused tests of the production state logic in kmd_logic: pending before
  submit, retire before publish, out-of-order completion, independent resources,
  stale bindings/generations, producer failure, reset, cancellation and waiter
  registration races. Keep protocol ABI/layout checks enabled.
- Build KMD, both UMDs, DXVK, Mesa and the affected vkd3d bridge. Linux cargo
  uses CARGO_TARGET_DIR=target/linux; Windows builds use local C:.
- Exercise mixed DX11/DX12 devices in one process, cross-process shared opens,
  delayed producer work, unchanged SRV bindings across frames, copy sources,
  rotation, resize and producer exit. Cover async and inline WSI, including
  the steady-state wait skip.
- Source searches must find no active HPS2 mapping/calls, producer PID/start/
  fence discovery or installer ACL setup. Preserve unrelated named fences.
- For runtime acceptance use compatible old/new stacks and interleaved
  workload runs. Compare publication CPU cost, submission/refresh counts,
  wait time, frame-time tails and visible changing frames. Include working
  DX11/Vulkan workloads and native DX12. Builds are not rendering/perf evidence.
- Cut over compatible artifacts together. Do not delete the file while an old
  process can still map it. Follow repository deployment/reboot authority;
  source implementation does not itself require either.

The first vertical implementation must establish exact allocation binding,
announced-to-retired completion, mapped freshness and a consumer wait together.
If that requires a new WDDM surface, scheduler executor or guessed identity,
report the concrete obstruction rather than expanding the project.

## Implementation and runtime acceptance

The source cutover keeps `WddmSurface::Wddm2_1GpuMmu`. It removes the DXVK and
Mesa HPS2 source/header pairs, their active callers, the foreign named-fence
cache, the optional unordered-read/kwait skip controls and installer file/ACL
setup. The old file is neither consulted nor deleted by this implementation.
The existing scanout reader and Present-buffer protocols remain separate.

### Allocation state and interface

`kmd_logic/src/producer_completion.rs` is the production state machine used by
`kmd_render/src/adapter/producer.rs`. `CreateAllocation` registers the exact
global allocation private pointer. `OpenAllocation` acquires the global private
data for the runtime's exact allocation handle and records its association with
the new open and `hKmdProcess`. Binding acquires the device-specific private data
and retains the generation already associated with that exact open. Each operation
uses one `AcquireHandleData`/`ReleaseHandleData` pair on the same PASSIVE thread,
with release outside driver locks. It never pairs independently resolved handles.
The production `Opens` registry and scoped-reference cleanup, including null private
data with a valid release token, are tested in `kmd_logic`.
No callback pointer is dereferenced or retained after that resolution. The returned binding
retains driver-owned status; it does not pin an allocation until DestroyDevice
and create a circular teardown dependency. Different opens reach one state.

**Runtime correction (.264):** `.263` crashed during DWM startup with
`0x113/0x26/1` in `GetHandleData`, called with an acquired reference outstanding.
The matching kernel dump identifies dxgkrnl's explicit rejection of a WDDM2
driver calling the WDDM1.x callback. The earlier null result during
`OpenAllocation` was this same callback restriction; the initial explanation
that the allocation handle was not yet published was incorrect. Both private-data
views now use the WDDM2 acquire/release callbacks. `.264 / oem48.inf` subsequently
booted with virtio-gpu, with no new bugcheck, no open/bind refusal and a fresh
visible desktop capture. DWM loaded the verified replacement UMD/ICD. This closes
the observed startup crash; the broader runtime acceptance packet remains pending.

The table reserves 8192 allocation slots, 8192 pending operations and 16384
resource/stream writer records; the KMD also reserves 32768 opens, 16384 bindings
and 1024 waits. Allocation, binding, stream and transport generations cannot
wrap into a live identity. Capacity exhaustion fails explicitly. Storage is
reserved at StartDevice; retirement and event wakeup allocate nothing.
The 512 KiB status view uses the existing read-only MDL and device-mapping
teardown path and survives transport Stop until adapter teardown.

`protocol/include/helios_producer.h` defines ABI v1, exported as
`helios_venus_producer_interface`. It includes stream registration, exact
allocation binding, publication, cached status, wait, retain/release and abort.
DXVK and vkd3d resolve that export from the module containing their live
device's `vkGetSemaphoreCounterValue` dispatch entry. This supports the
installer's content-hashed ICD filenames without choosing a module by name or
list order. An intercepting layer without the interface is refused explicitly.
The C control record is 96 bytes; status slots are 64 bytes. Rust layout asserts
and `python3 tools/sync-producer-abi.py --check` protect the three build-mirror
copies. The preexisting stream export remains for the separate Present-buffer
reader handshake; producer clients use the versioned interface.

Publication follows commitment of a retained signal operation. It records a
resource epoch independently of that stream's value namespace. Retirement
comes from the existing registered stream's exact tagged Venus ring boundary.
Each resource advances through its completed prefix, so another queue's later
epoch cannot hide an unfinished earlier epoch. Retirement before publication
is checked under the same transport serialization. Abort, stream closure,
allocation destruction and transport reset wake pending readers with failure;
they do not advance completed epochs. Releasing a consumer binding cancels only
its own waits. The binding's Vulkan device must outlive the binding.

Cached status uses bounded seqlock loads from read-only memory. A wait first
samples status, then atomically checks/registers a referenced event in the KMD,
waits in user mode and rechecks after wakeup. Timeout and cancellation include
contention on the reusable event mutex. They never authorize a read. The KMD
Escape itself does not sleep. Diagnostics are `PrInitF`, `PrGenF`, `PrRef`,
`PrPub` and `PrRet`; publication/retirement registry snapshots are rate limited.

### Engine and WSI boundaries

UMD11 stamps DXVK storage with the actual runtime allocation at create/open;
the former resource-handle substitution is removed. Storage rotation carries
the binding, including the external source of private staged images. The
unchanged-SRV gate reads cached generation/announced epochs. Each refresh
captures one dependency and stamps that same generation/epoch after recording
the copy. Imported-read dependencies live with the command list and are waited
by the submission worker before sparse, transfer or graphics submission, with
queue mutexes dropped. The worker checks shutdown/device loss between bounded
event waits; immediate-context/device teardown cancels these waits before
draining the workers. Folded producer signals, staging copies, external barriers,
snapshots and submission batching remain in place.

UMD12's Present calls `helios_vkd3d_enqueue_producer` with the exact engine
resource, allocation and queue. The existing callback FIFO orders an
ALL_COMMANDS signal after preceding work, including split submissions and
flushed queue waits. The resource has an internal reference through completion;
the queue's STOP/join lifecycle retains the queue through callbacks and fence
worker cleanup. No COM self-reference is released on the worker that would
make it join itself. HEPR now carries the registered context/cookie/value.
Handled failures that drop predecessor submissions, waits or initial transitions
mark the device removed, preventing a later empty signal from claiming success.
No sample-only ECL fence or GPU-idle wait is used by this hook.

Each present allocates a small retained operation payload for the existing
command/callback machinery. Semaphore, mapping and event objects are reused;
there is no per-frame NT/Vulkan synchronization-object creation. Measure this
payload and publication cost in the performance comparison.

WSI exports one unnamed NT semaphore handle per chain and passes it with the
exact pre-present value through `helios_umd_set_present_source_v4`. The v3 seam
also borrows the source's actual `VkImageCreateInfo` through the same-thread
Present. Dedicated external-memory imports must use that template, not flags/usage
inferred from D3D11 bind flags. DXVK deep-copies supported format-list and
queue-family arrays before caching the import, preserving creation fields and
letting the backend append the matching external-memory declaration. Ordinary
exclusive, unprotected 2D WSI sources are supported; unsupported creation
chains fail through the existing import-failure counters. Compression-control
and concurrent/protected sources require separate helper-device support and
are not silently reconstructed. The v2 export retains its old ABI for older
callers; the new WSI requires v4 and reports missing exports explicitly.
The source import's DXGI format describes its actual Vulkan format, separately
from the vehicle swapchain's flip-compatible format. In particular, a BGRA8
sRGB source remains an sRGB dedicated image; a compatible transfer to the
UNORM swapchain preserves its encoded bytes, without sampling the source or
inventing SAMPLED usage. Numeric packed-format conversions remain separate.
The v4 handoff additionally requires matched source ownership. App-to-PRESENT
keeps producer ownership through WSI's fallback buffer read, then the WSI blit
releases the original image to EXTERNAL in GENERAL before its exact producer
signal. The helper acquires/releases that source around the actual transfer
in its execution command buffer; it is excluded from generic eager self-acquire
on the next command list. Moving this copy to the init command buffer would put
it ahead of the acquire barrier. The app's next transition out of PRESENT
acquires from EXTERNAL. Ordinary prime blits and FOREIGN scanout retain their
existing behavior. v2/v3 callers retain their old ABI and do not opt in to v4
ownership; neither partial deployment silently gains this new contract.
The source image and its dedicated allocation must also request the same
renderer handle type. The resource-id helper uses DMA_BUF. A host trace on
2026-09-18 exposed a WSI OPAQUE_FD image bound to DMA_BUF export memory, hidden
by the renderer's legacy VUID-02728 filter. WSI now requests DMA_BUF explicitly
for both. DXVK's exact-template validator accepts DMA_BUF while retaining
legacy OPAQUE_FD metadata support; mixed or unrelated handle types are refused.
The exact alias itself explicitly uses DMA_BUF. A queued 4,000-frame pattern
control completes through the helper with matching pixels and handle types;
this validates the repaired contract, while Vulkan-freeze acceptance is open.

Shared-image command ordering is part of that ownership contract. The
September19 DWM trace proves an init-buffer transition executing ahead of an
older execution-buffer transition recorded under a rotated backing's other
logical image identity. `requiresHeliosOrderedAccess` now prevents transition
and transfer promotion for external shared images (including explicit v4
sources); private staged/debug stand-ins remain exempt. Ownership barriers and
accesses stay on the execution buffer without new waits or lifetime shortcuts.
The deployed FCFF7F28 candidate removes the observed layout-order VUIDs and
passes the same-image trace replay plus the steady portion of a queued pattern
control. A startup mixed-frame capture and the original freeze remain open;
see ROADMAP for the full evidence and acceptance limits.

The KMD's raw Venus allocator must use the renderer's DMA_BUF contract for both
Present-buffer creation and its dedicated export; Mesa normalizes the matching
DXVK import but cannot normalize KMD commands. External LINEAR images start
UNDEFINED. Their allocation-time submission transitions to GENERAL and releases
to EXTERNAL, then waits for that submission's real fence before exposing the
HOST3D backing to CPU writes or cross-device consumers. Failed/ambiguous setup
retains its image, memory and command objects. Readiness checks on existing
LINEAR copies replace late initialization, which could otherwise discard pixels
already written. This initial fence says nothing about later consumer release;
the existing copy/scanout retirement and allocation teardown guards still apply.

WSI owns
the handle until chain teardown; the helper duplicates it, caches an imported
Vulkan semaphore using exact kernel-object comparison, and retains the import
through the recorded copy. `clear_present_source_v2` ends the borrowed scope
and reports whether the DDI recorded a copy, including Present HRESULT failures.
The helper copy wait remains the mandatory copy-completion guard; the
fixed-target pending protocol below supersedes its original timeout policy.
Steady-state WSI still skips its frame-fence wait. GDI fallback and dropped or
resize-rejected frames wait their own source fence before reading/recycling.
A failed copy wait makes the chain terminal and retains the source allocation
through device teardown, including immediate swapchain destruction. This is
failure containment, not completion or a steady-state resource leak.

### Source/build checks

All checks below passed on the implementation checkout; none is runtime proof:

- `CARGO_TARGET_DIR=target/linux cargo test --manifest-path kmd_logic/Cargo.toml`:
  198 tests, including 20 new production producer-state/reference/wait tests. They cover
  publication before submission, retirement before publication, independent
  opens/writers, out-of-order completion, stale generations, capacity rejection,
  stream failure, allocation/reset teardown and event registration/cancellation
  orderings with exactly one reference transfer.
- `CARGO_TARGET_DIR=target/linux cargo test --manifest-path protocol/Cargo.toml`:
  14 tests plus ABI layout assertions.
- Windows release builds: `kmd_render`, `umd`, `umd12`, DXVK's static engine,
  Mesa `vulkan_virtio.dll`, and vkd3d static engines/DLLs. Cargo targets and build
  artifacts are on C:, with Linux targets under `target/linux`. Removing the
  stale generated Mesa WSI thin archive was necessary after deleting its old
  HPS2 object; rebuilding then linked successfully.
- Byte-exact ABI-copy check and root/nested `git diff --check`.

### Initial deployed runtime checks

The authorized `.264 / oem48.inf` deployment ran the complete installed 3DMark
definitions interactively on 2026-09-06, Fire Strike followed by Time Spy:

| Benchmark | Overall | Graphics | GT1 FPS | GT2 FPS | CPU / Physics | Combined |
|---|---:|---:|---:|---:|---:|---:|
| Fire Strike | 35669 | 57396 | 249.06 | 250.04 | 40451 | 8881 |
| Time Spy | 15934 | 16222 | 101.28 | 96.74 | 14482 | — |

Every workload in both result archives has successful status. Host VNC captures
show changing Fire Strike demo/GT2/Combined and Time Spy demo/GT1 scenes. Time
Spy loaded the verified native UMD12 and content-hashed ICD. The first Fire
Strike attempt was cancelled by display/focus loss coincident with the guest
capture task; its incomplete result is preserved and excluded. The successful
retry used a hidden task wrapper and host-only capture.

The four Time Spy processes recorded 21237 native presents in total, each with
`PresentProducerFailed=0`. Their logs still record `EclFenceNoDrain` and
`FenceBottomOfPipeUnproven`; passing this workload does not close the broader
DX12 submission/fence contracts.

After both benchmarks, the desktop remained visible on the same boot and DWM
process, with no new System bugcheck/shutdown event. KMD publication/retirement
snapshots advanced, and producer initialization/open/bind and IRQL failure
counters remained zero. Result archives, invocation records, module hashes,
captures and post-run health are under `tmp/hps2-264-runtime/`. These single
runs do not establish performance parity or replace the acceptance matrix below.

### Runtime acceptance packet — pending

Run only after a separately authorized compatible-stack installation. Record
root and nested revisions, dirty diffs, artifact hashes and actually loaded
modules for each arm. Do not mix an old UMD/ICD/KMD with this ABI. Keep the
shipping `UmdD3D12` default unchanged; native-DX12 activation is a separate
runtime step. Use interactive scheduled tasks, not session-0 launches.

| Case | Required stimulus and pass evidence |
|---|---|
| Mixed DX11/DX12 | Create DX11 and native UMD12 devices in one process; also run visible DX11 and DX12 applications together. Change distinct numbered/color frames; verify each window and desktop composition advance. Capture UMD12 `PresentProducerFailed`, KMD producer counters and exact frame content. |
| Cross-process sharing | Export a real shared allocation and open it twice in another process. Verify both opens observe one generation/epoch and an unrelated allocation remains independent. Change frame content repeatedly; close/reopen handles while retaining the allocation, then destroy/recreate and require a new generation. |
| Unchanged SRV binding | Bind the imported SRV once, publish at least 120 changing producer frames, and draw without SetShaderResources again. Compare staged pixels with the numbered producer frames; check recorded refresh epochs never exceed the consumed epoch. |
| Delayed and reordered work | Delay the actual vkd3d worker/queue work, including a queue wait before its signal and a split submission. Announced must advance while completed remains behind; the consumer must stay blocked until the registered boundary. Complete a later independent writer first and require the resource prefix to remain pending. `Umd12EclDelayUs` alone does not prove this boundary. |
| Rotation and resize | Alternate at least three buffers, resize repeatedly, minimize/restore and destroy/recreate the chain. Verify bindings follow backing rotation, frames change, old generations cannot satisfy new reads, and Present-buffer/scanout-reader protection still holds. |
| WSI | Exercise async WSI (`HELIOS_WSI_ASYNC_PRESENT=1`), FIFO and non-FIFO, delayed source signals, drops, occlusion, resize and forced helper-copy failure. Confirm the explicit handle/value reaches the copy, steady-state frame prep skips its fence, and no image recycles before the helper read completes. The owner excludes the inline path from this work. |
| Cancellation/teardown | Exit the producer while an epoch is pending; close a waiting consumer/device; cancel an event; stop/reset through a separately authorized lifecycle test. Waiters must terminate with failure, completed must not advance, and normal create/destroy cycles must not leak handles/mappings. Check the documented retained-allocation failure path separately. |

Existing `d3d11_shared_content_probe.cpp` and `d3d11_shared_draw_probe.cpp`
are useful sharing/readback controls; their single-frame, same-process results
do not cover the cross-process or unchanged-binding cases above.
`d3d12_clear_probe.cpp` and `d3d12_bridge_probe.cpp` provide native/engine
controls, not substitutes for native mixed-API acceptance. Require visibly
changing frames and correct content. Do not introduce a focus-stealing capture
during 3DMark; use the owner's visible observations and source/runtime ordering
evidence. Counter deltas and one frozen frame cannot pass this packet.

The owner initially reported an approximately 10% performance regression and
directed correctness work first. On .266 they confirm the shadows are fixed at
about 100 FPS and request investigation of a further 10–20% gain in both APIs.
Keep `HELIOS_WSI_ASYNC_PRESENT=1`; the inline path remains outside this work.
Use clean matching-settings before/after benchmarks per API, without a complex
interleaved campaign, and preserve visibly correct changing frames. The automated
74.26 FPS GT1 result used instrumentation and is a separate observation; settings
equivalence with the owner's benchmark is unproven. That acceptance did not
establish further gains; the separate transport-capacity wake measurements are
reported in `PERFORMANCE_FEEDBACK.md`.

### Explicit remaining DX12 synchronization gaps

The bounded HPS2 hook supplies the presented resource's producer boundary.
The subsequent [HE12 v2 repair](../dx12/EXECUTION_SYNC.md) replaces the general
sample-only ECL bridge and removes the above-watermark private-fence policy.
It adds runtime-context admission plus exact registered worker completion for
ECL and Present callbacks. This repair completed independent review, was
deployed on .266 and is retained in .270. It passes the four native ordering
cases described in EXECUTION_SYNC.md. Those cases and the owner's .266 Time Spy shadow acceptance do
not establish all runtime fence, resource-use or lifecycle contracts.
Nonzero monitored-fence GPU placements and direct D3D12 queue fence DDIs remain
explicit E_NOTIMPL paths pending their negotiated-contract analysis. These are
not the optional WDDM 3.2 native GPU fence feature, which is outside this
WDDM 2.1 repair and is not an acceptance blocker.

In particular, a completed producer epoch does not perform a Vulkan external
queue-family ownership transfer or release a consumer. The narrow vkd3d hook
does not record the general resource release/acquire protocol needed to prove
all DX12-to-DX11 shared-image uses. Adding that requires tracking the exact
resource layout/owner and recording matched barriers on both sides; a later
queue signal alone cannot supply it. Existing staging/external barriers and
Present-buffer/scanout-reader contracts are preserved. Mixed-API acceptance
must establish the concrete path's ownership proof; if absent, that path remains
blocked on this separate contract rather than being declared fixed by HPS2
retirement. No scheduler, memory-manager or presenter rewrite was introduced.

### Vehicle copy completion: fixed target and pending waits

The 2026-09-06 Steel Nomad Vulkan control failed at acquire after WSI's
post-Present copy guard exhausted its 32 ms wait. That error does not establish
a host device failure. The old helper return also conflated pending work with
a missing context or bridge exception, and each wait selected a fresh flush
boundary. The repair was deployed in .268 and remains in .270/oem53.inf with release UMD11
`245D1BC3...` and ICD `3349607B...`; UMD12 is unchanged. The finite-work
regression probe passed on .268, where Steel Nomad Vulkan completed at
93.228233 FPS. The final .270 control also completes, at 90.683228 FPS.
Owner acceptance of the moving scene remains unreceived.

* The vehicle's existing explicit copy flush captures its DXVK submission ID
  once. The same-thread token retains that ID and its helper device identity;
  later Present work cannot replace the target. Capture failure keeps a zero,
  invalid token and the recorded-read obligation, even if Present later fails.
* `helios_umd_wait_present_copy_v2` returns completed / pending / failure
  separately. Pending waits never flush. Any non-success device status before
  or after waiting is failure: CPU cleanup notifications cannot prove a copy
  completed after submission failure. A stopped CS worker's recording-exception
  latch also makes the wait fail, including exceptions before queue submission.
  The old export remains for old ICDs;
  new ICDs require v2 and report missing exports before falling back to GDI.
* WSI waits in sleeping slices, default 32 ms, for the fixed copy target.
  `HELIOS_WSI_VEHICLE_WAIT_US` selects a slice clamped to 1..32 ms, not a
  device-loss deadline. The first pending slice is counted in the existing
  `vehicle_wait_timeouts` telemetry and logged with resource/producer identity;
  eventual completion is logged with elapsed time. A pending slice alone is
  informational. `wait_err` counts terminal helper/capture errors and must stay
  zero in healthy acceptance runs; `wait_cancel` counts retained-read cancellation
  and is expected only in deliberate resize/stop cases. Both are printed in WSI
  telemetry and on the always-readable terminal diagnostic line.
* Only confirmed copy completion clears `read_unproven` and permits recycling.
  In the deployed implementation, chain error, changed/destroyed surface,
  helper failure or async-worker stop cancels the wait without releasing the
  image. The source-only candidate13 amendment below separates cancellation
  from retirement of that read. The helper COM device is released only after
  the async worker joins. The raw UMD device registry is not itself a lifetime pin.
* Acquire's own timeout and status signaling remain unchanged. Pending work
  keeps an image unavailable; it does not invent `VK_ERROR_DEVICE_LOST`.
  This follows [Vulkan acquire semantics](https://docs.vulkan.org/refpages/latest/refpages/source/vkAcquireNextImageKHR.html).

No producer wait, consumer-release guard, staging barrier, submission batch or
scanout protection is removed. No GPU-idle wait is added. Reported engine
failure and swapchain cancellation end the retry; the previously documented
host-disconnect/device-loss propagation gap can still prevent an engine from
reporting failure. Inline WSI and fault/cancellation stress require separate
runtime acceptance. A completed benchmark will not establish those paths or
the owner's visible correctness acceptance.
The new CS-exception latch makes this copy wait fail; it does not repair the
pre-existing general immediate-context destructor synchronization on a dead
CS worker. Full CS-failure teardown remains unaccepted.

`tools/vk_vehicle_completion_probe.cpp` is the focused pending regression test:
warm the real vehicle, submit finite ordered GPU work before the frame, acquire
the other images and require the submit fence still pending after 80 ms.
Require NOT_READY/TIMEOUT, wait that exact submission and reacquire the original
image. All semaphore dependencies are submitted before Present; no future
host signal or idle wait is used. A GPU that finishes too soon makes this
test inconclusive, not a pass.
Require same-PID vehicle LIVE and pending/completed diagnostics alongside its
PASS result; software GDI can otherwise satisfy the application's checks.

Runtime evidence is `tmp/steel-fix-20260906/candidate-completion-exit`: exit 0,
same-PID vehicle LIVE, exact delayed producer 602 pending then completed, and
reacquisition of the original image. The initial run also passed its application
checks but had a wrapper exit-code collection error; it was not counted as a
passing harness run. `steel-vulkan-fixed` completed with workload status 0,
archive/export and 4814 successful helper Presents. Copy waits exceeding 32 ms
completed without invented device loss. Its final surface-loss cancellation
retained the outstanding read as the window closed (`wait_cancel=1`); this is
not evidence for general fault teardown or consumer release.

**September19, candidate13 source amendment — cancelled-copy retirement:**
candidate12's second completed Steel observation retained one source image,
its staging buffer and two memory allocations after surface loss; host
`vkDestroyDevice` validation reported those four remaining children. Its next
run had no cancellation and no warning. Neither observation establishes safe
cancellation teardown.

`wsi_copy_retirement.h` now contains the actual wait loop used by Win32 WSI.
OUT_OF_DATE/SURFACE_LOST and async-worker stop latch a presentation error but
keep the helper device and same-thread submission target alive. Sleeping waits
continue on that fixed target for up to five seconds after cancellation.
Proven completion clears `read_unproven`, allowing normal swapchain destruction
to destroy the source; the cancelled presentation still returns its error and
does not recycle the image. A restored window cannot restart the budget or
make that presentation successful. Device failure before/after a wait and
deadline expiry retain the source. Ordinary presentation has no new deadline,
flush or submission. `wait_cancel` now counts presentation cancellations even
when their read retires; `source_retained`, `completed` and `drain_expired`
diagnostics distinguish the outcomes.

The five-second bound contains the existing host-loss reporting gap; it does
not solve resource cleanup after an unproven read or lost device. That retained
resource path still has a lifetime/validation gap. In particular,
[device loss does not implicitly destroy device children](https://docs.vulkan.org/spec/latest/chapters/devsandqueues.html).
No timeout grants permission to release their storage.

The production-loop CPU test covers cancellation/completion races, deadline
expiry, window restoration and failure notifications. It and five surrounding
Mesa allocation/ownership/feature tests pass; sanitizer checks and MinGW
compilation of the CPU test and modified vehicle probe also pass. The probe's
`--teardown-close` and `--teardown-resize` modes cancel during a demonstrated
pending read, then fence the application's own submission before destroying
the chain, as required by
[swapchain destruction](https://docs.vulkan.org/refpages/latest/refpages/source/vkDestroySwapchainKHR.html).
Application completion alone is insufficient: require the same PID and exact
producer's helper retirement plus host object destruction, no VUID and no new
Xid. A new host Xid before this continuation's trials leaves QEMU paused.
Windows ICD compilation and baseline/candidate lifecycle trials are pending;
candidate13 is not deployed or accepted. See ROADMAP for the preserved evidence.

After the owner reports host recovery on the same boot, Windows candidate13v2
(`9AE82DE7…`) builds and passes both close/resize comparisons. Candidate12v4
retains four source objects in both controls; candidate13 retires the exact
pending helper read, preserves SURFACE_LOST/OUT_OF_DATE, and destroys those
objects without a VUID or new Xid. Complete producer/helper traces and loaded
module hashes support this narrow result. All five transfer controls also pass
240MiB. Lost-device and expired-drain cleanup remain unaccepted, as do both
broader graphics defects. This isolated runtime candidate is not a global
driver deployment; ROADMAP records the verified archives and Steel follow-up.
Two subsequent Steel Vulkan runs complete at 8126 / 81.27 FPS and 8083 /
80.83 FPS. Both final surface-loss cancellations retire their exact helper copy
with `source_retained=0`, no host VUID and no new Xid. Their producer traces hit
the record cap, so full producer destruction history remains unavailable for
these benchmarks. The focused lifecycle controls retain complete traces.

On September20 the owner explicitly requests default installation after
confirming recovery from another baseline Steel fault. Candidate13 (`9AE82DE7…`)
is registered machine-wide and Windows is restarted. DWM's loaded hash and a
normal registry-selected Vulkan control verify activation; a 48MiB transfer
and desktop capture pass, with no new Xid. The baseline and rollback script are
preserved. This deployment adds no Steel benchmark acceptance or lost-device
lifetime result; both graphics defects remain open. ROADMAP records the receipt.

The owner's subsequent September20 report confirms normal Steel Vulkan use.
Together with the two completed candidate13 benchmarks, this resolves the
loading-freeze symptom in the tested configuration. Native DX12 rollback and
lost-device lifetime coverage remain open. A later recurring VUID09860 is
traced to a deployment mismatch: the default QEMU launch still uses the old
host renderer, which adds DMA_BUF to explicit OPAQUE_FD exports. The verified
renderer library and server have now replaced that default pair, with backups;
the owner's full QEMU restart activates the verified pair. The fresh desktop
trace shows separate OPAQUE_FD/DMA_BUF exports, no combined exports, no VUIDs
and no Xid, with the expected ICD and UMD loaded in DWM. This resolves the
reported deployment validation issue in that observed path. Guest ICD
installation alone does not deploy or reload the host renderer. See ROADMAP
for identities and evidence. No ownership or consumer-retention guard changes.
