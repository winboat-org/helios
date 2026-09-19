# Native DGC and renderer queue completion

On 2026-09-10 the owner authorized publishing the paired forks. Verified GitHub
`main` commits are [virglrenderer 2121d5d0](https://github.com/winboat-org/virglrenderer/commit/2121d5d0e82a58edc321ced3309c1ce7b7c41905)
and [venus-protocol fe08e82c](https://github.com/winboat-org/venus-protocol/commit/fe08e82c3819e8ee3c547b1ea810fde61f46fa78).
Root `.gitmodules` now selects these repositories. Protocol round-trip and
renderer queue tests pass; this publication does not change the loaded host
binaries or establish new guest acceptance. Mesa/engine/compiler commits remain
local until separately authorized for publication.

The owner authorized a virglrenderer fork on 2026-09-09, including the Vulkan
extensions needed by native DX12 and removal of the private vkd3d indirect
emulation and native-fence workaround. This supersedes the stock-renderer
constraint and the earlier requirement to enable `HELIOS_RETIRE_FEEDBACK`.
WDDM 2.1, the static native UMD architecture and `HELIOS_WSI_ASYNC_PRESENT=1`
remain unchanged. VM launcher restarts remain owner-operated; guest driver
deployment and guest reboots are authorized. Nothing in this change claims
FL12_1 conformance or Port Royal acceptance.

## Source and implementation

The new root submodules are `virglrenderer` and `venus-protocol`. Their initial
upstream bases are respectively `cf6c62da2a1384b194f463e6221371962fe99575` and
`ca19b6358d7cc491bc3e4de76f04c6700876a8fa`. These identify the fork bases,
not the modified source. All changes remain local and uncommitted; the
submodule URLs still identify upstream. No remote fork or push was performed.

| Contract | Host Vulkan | Protocol / renderer | Mesa Venus | vkd3d / Helios | Validation |
|---|---|---|---|---|---|
| Native EXT DGC | NVIDIA 610.57.04 advertises it | Appended opcodes 353–361; typed layout/execution-set objects, device cleanup, feature/property queries and command dispatch | Synchronous creation with real errors, ordered updates/destruction, preprocess and execute | Uses native engine DGC; GPU constants, root CBV/SRV/UAV, VBV/IBV, draw/dispatch/count. No private emulation path | 2,404 indirect checks pass through the new Linux Venus stack; Windows native root and IA suites each pass 12 cases / 48 words; IA also checks query, predicate and pending-list reset behavior |
| Execution sets | Pipeline binding supported | Pipeline handle replacement, create/update/destroy | Pipeline objects exposed; shader-object binding limits are zero because shader objects are not exposed | Native Vulkan probe covers two pipeline choices and cross-command-buffer preprocessing | Correct output and guard words; compute statistics fail as described below |
| Buffer usage / preprocess storage | 64-bit usage flags supported | Existing maintenance5 serialization | External-memory translation now preserves `VkBufferUsageFlags2CreateInfo`, including PREPROCESS_BUFFER | Engine scratch allocation retains native usage flags | Initial validation errors reproduced, repaired; full indirect suite then has no host validation diagnostics |
| Mixed sample rasterization | NV_framebuffer_mixed_samples advertised | Extension plus three optional dynamic coverage commands (opcodes 350–352) | Extension and commands forwarded | UMD12 0C292592 implements mixed-sample coverage and output-mask/A2C translation | Host GPU cases and native TIR readback/replay/query/MRT tests pass on0C292592; separate scope in [TIR.md](TIR.md) |
| Queue-marker completion | Ordinary internal fences avoid measured exportable-fence delay | Internal markers use ordinary VkFence; successful wait/status required for callback | Wire wait/event retirement; no feedback shadow or 0x14 escape | Generation-qualified wire receipts advance producer/HE12 progress | Production renderer worker and 211 KMD logic tests pass; all four native guest ordering/readback cases pass on .271 |

The append-only command numbers retain wire format version 1. Extension
advertisement still intersects protocol, renderer and actual host support.
Installing the new guest ICD with the old renderer does not create DGC support.
No feature-level override, renderer-name override or force-admission flag is used.

The protocol validates the DGC union selector against its enclosing structure
before decoding payloads. Action tokens whose layout union is unused do not
dereference that union. Tests cover every truncated prefix, mismatched selectors,
ignored action arms and typed pipeline-handle translation. Existing opcode
values and unrelated union formats are unchanged.

`vkCmdPreprocessGeneratedCommandsEXT` captures another command buffer's current
state. Mesa flushes that buffer's recorded prefix before encoding preprocessing,
then flushes the preprocess call before later recording can change the source
state. This serializes command recording; it adds no GPU submission, readback,
CPU prefix wait or GPU-idle wait. This follows the
[Vulkan DGC state-capture contract](https://docs.vulkan.org/spec/latest/chapters/device_generated_commands/generatedcommands.html#vkCmdPreprocessGeneratedCommandsEXT).

The vkd3d `indirect_emulation*` files, shader, private root/PSO variants,
CPU-assisted IA continuation and signature/submission hooks are removed.
Unrelated tiled-copy continuations, query scope ownership and partial-submit
failure protection remain. Native DGC requires root descriptors as GPU addresses,
so devices with native DGC select raw-VA root CBVs when the root layout is built.
Non-mesh graphics pipelines retain dynamic vertex stride even when a VBV token
updates an unused slot. Missing native DGC refuses state-changing signatures
with E_NOTIMPL; ordinary action-only indirect paths remain. Private instrumented
roots requiring push UBOs still hit the upstream native-DGC refusal and require
separate runtime evidence; the public 64-DWORD root contract is unchanged.

## Fence cause and removal

The observed delay is not evidence of a blind 10 ms sleep in the active renderer
worker. The exact GPU-fill / empty-marker / wait reproduction measures
SYNC_FD-exportable internal fences at 6.837 ms average (10.319 ms worst), ordinary
fences at 0.253 ms average (0.431 ms worst) on this host. Earlier .265 evidence
measured 8.060 / 0.329 ms. These are host sequence measurements, not a Windows
benchmark result or a promised performance gain.

Renderer internal queue-marker fences are never exported. Guest Vulkan fences
and their external-fd APIs are separate objects and retain their export behavior.
The renderer now creates ordinary marker fences, checks reset failures, and
invokes the successful retirement callback only after VK_SUCCESS. Failed waits,
device loss and teardown with an uncompleted fence do not become success.
Failed wait observations quarantine the marker until device teardown; they do
not return a possibly pending fence to the reset/reuse pool. The worker test
also injects wait OOM and verifies this ownership rule.

Mesa no longer attaches exported Win32 timelines to GPU feedback slots, polls
those slots on the retire worker or sends STREAM_FEEDBACK. The 0x14 escape is
retired and reserved, not reassigned. KMD producer/HE12 progress comes from the
exact authenticated wire receipt. Initially-zero, GPU-only stream registration,
epochs, queue ordering, independent consumer release and backing ownership remain.
Ordinary Mesa feedback used for internal Vulkan synchronization is not removed.

Host loss/disconnection still lacks an error-bearing callback across the proxy
boundary; its existing forced-retirement behavior is a separate acceptance gap.
Suppressing successful callbacks in the queue worker does not close that gap.
General DX12-to-DX11 external ownership/consumer release, pending allocator
Reset versus fence-worker lifetime, sharing/resize/rotation/teardown and async
WSI stress remain open.

### Vulkan fault diagnostics, 2026-09-18

`VKR_DEBUG=validate,fault` adds opt-in host CPU-dispatch records for image/buffer
creation and binding, imported/exported memory identity, copies, ownership
barriers, command-buffer submissions and real queue-marker results. Records
include context, sequence and UTC time, with an explicit 200,000-record limit
per context. They do not prove GPU execution or map an NVIDIA fault VA directly
to a Vulkan object. Subresource/extension coverage is diagnostic, not a complete
Vulkan replay trace.

The September19 follow-up also records command names before native dispatch,
buffer-copy ranges, pipeline bindings, primary/secondary command-buffer links,
submission semaphore values/stages and actual fence-wait results. Buffer device
addresses are logged only when the guest requests them; the diagnostic adds no
address query or GPU work. The CPU `venus_fault_dispatch` test invokes the
production dispatch with fake native entrypoints, checking disabled behavior,
handle translation, 64-bit ranges/values and unchanged device-loss/timeout
returns for both submission APIs. These records still describe CPU dispatch,
not completion of the commands they name.

Fault mode also prints the external-memory binding VUIDs normally suppressed
by the legacy Venus host-visible-memory filter. An explicitly external WSI
image/memory type mismatch was hidden by that filter during candidate8; the
absence of printed validation messages in older runs is not contract proof.

Set `VKR_FAULT_TRACE_DIR` to a fresh directory to save decoded
`vkCreateShaderModule` bytes before the native call. Files include process,
context, sequence and module ID, use exclusive creation and flush to disk, and
have a 128 MiB per-context budget with explicit failure/limit records. This
does not capture inline pipeline shader modules or shader objects. Validate
saved modules and compare hashes with process-qualified guest traces; a valid
module alone does not exclude resource corruption or a host compiler fault.
The CPU `venus_fault_trace` test checks disabled behavior, byte preservation,
non-overwrite and budget enforcement. `venus_queue_sync` retains its real
completion/error/quarantine checks. No device-idle, timeout-success, consumer
release or ownership behavior changes with this diagnostic.

`tools/vulkan_transfer_pool_probe.c` is a bounded transfer control, separate
from presentation acceptance. It checks upload → buffer → optimal 2D/3D image
→ readback bytes, using separate allocations or interleaved buffer/image memory
pools. Allocation sizes, alignment, granularity and dedicated requirements come
from the device. Four slots allow multiple outstanding submissions; each slot's
real fence protects CPU writes, readback, command reset and resource retirement.
Legacy and synchronization2 submissions carry exact timeline dependencies and
execute secondary command buffers. A timeout fails admission and retains pending
resources; there is no device-idle or timeout-success path. CPU lavapipe with
synchronization validation and both Windows v2 24-submission cases pass.
Neither closes Steel's freeze. The later `--copy-burst` control reproduces
readback corruption with mapped device-local upload and cached readback memory;
the same native Vulkan copy control passes. `vulkan_dmabuf_mapping_probe.c`
then finds disagreement between Vulkan and DMA-BUF CPU mappings of one type4
allocation, with zero GPU submissions; type3 is a passing control. These runs
precede a newly verified host reboot. September19 fresh-boot repeats now also
fail on type4, including a PROT_READ DMA-BUF alias whose native Vulkan mapping
passes a full self-check; type3 remains a passing control. An initial type4
pass is followed by repeated failures, with no QEMU or renderer running, zero
submissions by these probes, and no NVIDIA Xid recorded that boot. Separate
OPAQUE_FD Vulkan allocation/import controls pass, but the driver does not
advertise the requested dual-handle combination;
this is not permission to force a different export type. The syscall trace
selects the DRM/NVKMS export path, whose contiguous-address assumption is now a
source-supported candidate requiring physical-range evidence. Neither the
mapping failure nor that source candidate proves Steel's root cause. See
ROADMAP for exact evidence, memory-type controls and acceptance limits.

Candidate12 pairs ordinary Helios/NVIDIA resources with an internal OPAQUE_FD
allocation export, while preserving explicit DMA-BUF WSI. The renderer now
honors explicit export types rather than adding an incompatible DMA-BUF bit.
The probe's optional `--renderer-import` build exercises the production Vulkan
mapper on a separate device: three type4 and three type3 CPU comparisons pass;
a same-boot read-only DMA-BUF type4 comparison fails. Native builds and focused
CPU tests pass. The Windows build now passes, but its first attempted transfer
trial used an elevated task, which invalidates the ICD override. The producer
trace uses DMA-BUF; a new Xid31 is attributed to DWM, and the watcher pauses QEMU.
Retrieved guest evidence shows wrong bytes but no loaded-module receipt; the
installed baseline ICD remains an inference for that invalid elevated run.
The probe now rejects elevation and checks the actual ICD path/hash before
device creation; an initialization-only step checks mapping policy before a
transfer trial. After the owner reports recovery and authorizes the same boot,
the v4 Windows init-only control and all five transfer controls pass with the
candidate loaded: 240 MiB checked, no timeout, OPAQUE exports verified, and no
new Xid. The first 180-second Steel observation is incomplete; two runs with
a separate workload allowance complete at 8124 / 81.25 FPS and 8069 / 80.70 FPS,
with visible scene progression, verified candidate identities and no new Xid.
Both producer traces hit the record cap. One shutdown retains an unproven helper
source, producing a four-object `vkDestroyDevice` validation warning; the repeat
has neither that cancellation nor a host VUID. The retention guard remains
mandatory; consumer completion/teardown acceptance is still open. Neither
graphics defect is closed. See ROADMAP's candidate12 entry for the packages,
verified archives and acceptance limits. Native DGC and completion rules are unchanged.

The September19 teardown follow-up adds source-only candidate13: after window
cancellation, WSI continues sleeping waits on the already captured helper-copy
target for a bounded five seconds. It preserves the presentation error and
releases the source only on proven completion; timeout/device failure still
retain it. Focused CPU tests pass, but the Windows ICD build and lifecycle
trials have not run. Entry checks found a new 19:49:58 Xid in context67 using
DMA-BUF; its guest workload/ICD are unverified. QEMU is paused pending recovery
from that new fault. See [the copy-retirement amendment](../HPS2_REFACTOR.md#vehicle-copy-completion-fixed-target-and-pending-waits)
and ROADMAP. This changes neither native DGC nor the acceptance status of either
graphics defect.

The owner subsequently reports recovery and authorizes resuming that same boot.
Retrieved guest logs tie context67 to Steel PID2932 using the installed baseline
ICD path. Candidate13v2 then builds on Windows and passes close/resize retirement
controls with complete traces, explicit source-object destruction and no host
VUID/new Xid; both baseline controls reproduce the retained four-object leak.
Five candidate transfer controls also pass (240MiB). These scoped results do
not close either graphics defect or lost-device teardown. Native DGC is unchanged.
Two candidate13 Steel Vulkan runs subsequently complete at 81.27 and 80.83 FPS,
each retiring the final cancelled copy without a retained source, host VUID or
new Xid. Producer trace caps and broader acceptance limits remain in ROADMAP.

At the owner's September20 request, candidate13 is subsequently installed as
the machine-default ICD and activated by a Windows restart. DWM and a Vulkan
control using normal registry selection load the verified candidate; a 48MiB
transfer passes with no new Xid. The baseline and rollback script are retained.
Deployment does not establish a Steel fix or native DX12 frame order. Native
DGC and the open defect status are unchanged; see ROADMAP for the install receipt.

## Compute statistics update, 2026-09-10

[DGC_QUERIES.md](DGC_QUERIES.md) supersedes the D3D12 compute-query failure
below: native UMD12 `22C31F11…` passes 2,218 checks and 56 expanded readbacks;
direct NVIDIA, paired Venus and Intel each pass 2,166 checks. Native DGC still
executes commands. GPU query fragments derive CSInvocations from the executed
dispatch stream, with no CPU command emulation or driver allowlist. The raw
Vulkan probe's omitted counter remains a Vulkan-level observation. TIR also has
subsequent native acceptance in [TIR.md](TIR.md). Full format/shader limits and
the documented sparse compatibility exception remain separate.

## Initial validation and gaps, 2026-09-09

Evidence is under `tmp/native-dgc-renderer-20260909/`. The owner restarted
QEMU with the local renderer wrapper, then the paired guest artifacts were
deployed and the guest rebooted. `host-loaded-after-owner-boot.json` and
`windows-after-deploy.json` record activation; the original build receipts
remain separate. The earlier Linux tests used a separate vtest server on the
real NVIDIA GPU; the native tests below use the Windows system runtime.

- Protocol roundtrip and production renderer queue-worker tests pass, including
  reset/allocation/submit/wait failures and no callback before completion.
- The engine, Mesa and renderer build on Linux. Windows clang-cl static engine,
  Mesa ICD, release native UMD and signed .271 KMD package build. Bindgen 0.72
  and layout assertions remain enabled; Windows targets use local C: storage.
- GPU-produced root arguments: 138 checks pass. The complete `execute_indirect`
  selection: 2,404 checks pass, including predication, count, vertex offsets,
  graphics, compute and mesh forms, with no host validation diagnostics after
  the usage-flags repair. These counts overlap; do not add them together.
- `tools/vulkan_dgc_probe.c` verifies execution-set creation/update, two pipeline
  selections, preserved push constants, cross-command-buffer preprocessing,
  readback guards and teardown. Its output checks pass. Its stricter compute
  invocation query returns **0 instead of 2** on NVIDIA, both directly and
  through Venus; an ordinary dispatch control reports **1**, correctly.
  Implicit preprocessing reproduces zero too. The test deliberately exits 1;
  the complete probe is not a pass. The
  [Vulkan query contract](https://docs.vulkan.org/spec/latest/chapters/queries.html#queries-pipestats)
  describes implementation-dependent counters; this result does not satisfy
  the current D3D12 query acceptance test. Do not hide it or invent counts.
- Logical query continuation reports 4 rather than 6 CS invocations, then 3
  rather than 4 on replay, matching the missing DGC dispatch counts. The
  multiview query test also fails its VS-count expectation on both direct host
  Vulkan and Venus (6 versus at least 12). Native Helios reports view instancing
  NONE; this separate engine behavior is not native UMD acceptance.
- The same 295 query-continuation checks pass on the host Intel Vulkan driver
  with native DGC. That is an independent engine control, not NVIDIA or Windows
  acceptance.
- Complete FL12_0/12_1 conformance is still open: TIR, reserved-resource
  compatibility limitations, the query gap and the private expanded-root DGC
  boundary remain. RaytracingTier is still NOT_SUPPORTED in the native UMD;
  no Port Royal completion is claimed. FL12_2 and Speed Way remain subsequent.

## Build and owner-operated activation

The prepared 2026-09-09 artifact checkpoint is recorded in
`tmp/native-dgc-renderer-20260909/source-provenance.json` and
`windows-build-and-loaded-final.json`. The former includes all eight repository
heads, dirty patches and 220 changed-file snapshots; the latter verifies 102
changed inputs against the actual Windows source mirrors. Guest artifact copies
were verified by SHA256 on both Windows and Linux. An earlier direct-to-share
JSON write was malformed; only the final receipt, written on C: then copied and
hash-verified, is authoritative.

| Prepared artifact | SHA256 |
|---|---|
| Local libvirglrenderer.so.1.11.0 | `06ce39644dced89127cb40f81441bc912958b586deb154a20df643640f8e6ef0` |
| Local virgl_render_server | `afed7176d173b7c1566fbbe8c41718dde146b5df77254b20032a4ca80116a457` |
| Signed release KMD22.22.271.0 | `cd282f119b858a1d2a7c04a857692415eea3276c05152ecc04abf4e791c997e4` |
| Release helios_umd12.dll (static vkd3d) | `41a7e2905d89426a7d1311af14f07c6d19c2045eacabba23d84a986401d85c01` |
| Packaged release helios_umd.dll | `57c84ed403dc8dec476e8c8b7648eeec137a207c5c5d3edf00d2eb18c9010992` |
| Windows Mesa Vulkan ICD | `43394bbdeb29912bae531c5aa5d016ad6ec1aa285fdc7f45297f48acfd64dd36` |

The owner booted QEMU2147972 with the local renderer library `06ce3964…` and
render server2148004 `afed7176…`; both executable and mapped-library hashes
match the prepared artifacts. Guest activation reboot completed at
2026-09-09 21:33:50 +05:30. The guest now runs **22.22.271.0 / oem54.inf / Code0**,
WDDM2.1 and explicit HKLM\SOFTWARE\Helios `UmdD3D12=DWORD1`.
DWM1832/session1 loads UMD11 `57C84ED4…` and ICD `43394BBD…`; native probes load
UMD12 `41A7E290…` from the new DriverStore package. No ProgramData UMD override
is needed. Native runtime/Core are now **10.0.26100.9278**, DXGI
**10.0.26100.9444**: Windows updated them between the prepared receipt and the
activation reboot. Earlier benchmark runs used older runtime binaries.

The installer regenerated the catalog and re-signed the KMD. Deployed hashes:

- KMD: `bee454883a4800ea38abe0c271ddf48ef0a1f087c6d7e75d2972f3473cc0738b`.
- Catalog: `fd08d5f696096623780246c5dc0c1b396c5e77c89bb0318dfa6296371847ec70`.
- INF and both UMDs retain the prepared hashes above; the ICD does too.

`install-kmd.log` identifies the prior-package backup and signature operation.
`windows-after-deploy.json` records all full paths, runtime hashes and loaded
DWM modules. LLVM22.1.8, VulkanSDK1.4.350.0 and Rust/cargo1.96.0 were verified
for the prepared builds. New probe builds use MSVC/VS18.8.2.
Probe-runner verification also passes 17 synthetic provenance and 33 archive
failure-path checks; those checks do not run graphics.
Hosted CI has not been run for this uncommitted changeset. After native
validation, `source-after-native-validation.json` confirms that every recorded
non-document source input still matches the prepared build checkpoint; all
eight repository whitespace checks pass.

From the root checkout:

```bash
bash tools/build-native-renderer.sh
```

This builds/tests the protocol, synchronizes Mesa's generated driver headers,
and installs the renderer into `target/linux/virglrenderer-install`. Python
Mako and PyYAML are required; the local tools venv, if present, is used. The
helper never modifies `/usr`, restarts QEMU or deploys guest binaries.

The owner adds this selection to their existing launch command at the next
QEMU restart, preserving their other options:

```bash
HELIOS_QEMU_BIN=/home/rupansh/helios-vgpu-dx12/tools/qemu-with-native-renderer.sh
```

The wrapper selects the paired local library and render server after the
launcher's sudo transitions, and executes the existing QEMU binary with its
module directory. Merely exporting LD_LIBRARY_PATH before the ordinary launcher
is insufficient because sudo strips it. No launcher file is modified.

After restart, verify `/proc/<qemu>/maps`, render-server executable and hashes
before installing the paired .271 KMD, release UMD12 and Mesa ICD. Then run the
native feature/capability, indirect, tiled, root, SO and four ordering probes
through interactive scheduled tasks; archive loaded modules and identities.
Use host VNC for graphical evidence. Port Royal remains gated by native DXR.
Any benchmark comparison needs matching completed results and settings; the
owner remains the visual oracle. Completed controls are recorded below.

## Native Windows activation checks, 2026-09-09

All runs use interactive scheduled tasks in session1, the Helios adapter
LUID `00000000:00007866`, system D3D12 and the exact UMD12/ICD above. No WARP,
app-local engine or feature-level/shader-model override was admitted.
`HELIOS_WSI_ASYNC_PRESENT=1`; the removed retirement-feedback knob is cleared.

| Native check | Result / evidence directory |
|---|---|
| Admission/caps | FL11_0, 11_1, 12_0, 12_1 create successfully; 12_2 returns `0x887a0004`; maximum query returns12_1. SM6.0, tiled2, binding3, ROV1, conservative3, RT0. DDI `0x000c0050006e0000` / device0110; runtime asks both feature-level query forms. `native-caps/` |
| Native DGC roots | 12 cases / 48 readback words pass, including GPU-produced arguments and count0/1/3/clamping. `native-indirect-roots/` |
| Native DGC IA | 12 cases / 48 words pass; query counts, predication, VBV/IBV/indexed offsets and pending-list reset covered. `native-indirect-ia/` |
| Root signatures | Completed12 cases / 48 words. `native-roots/` |
| Stream output | Completed34 cases, including counters, overflow, null targets and raster+SO. `native-so/` |
| Tiled/inherited | 18 cases pass; 3D tiling is BLOCKED77 at reported tier2. Overall suite remains BLOCKED77, not PASS. Includes no-output MSAA15 PSOs ×2 replays /17,280 words, mapping/copy/unmap/lifetime/ordering/invalid inputs, color/depth MSAA4 and raw D32 payload cases. `native-tiled/` |
| Four ordering cases | All pass with65,536-word producer/consumer witnesses per case: producer-first, consumer-first, CPU rewind gate, cross-process signal. Live observer hashes both main/helper loaded UMD/ICD and system runtime. `native-sync/` |
| DXR | Native FL12_1 succeeds, then BLOCKED77 at RaytracingTier0 before any AS or DispatchRays command. `native-dxr/` |

`desktop-after-deploy.png` is a host-VNC desktop capture. Probe completion and
this desktop image do not establish owner acceptance of benchmark scenes.
Some PID-named driver logs append across process-ID reuse and contain older
sections. Use the new process's explicit module records and current module
section; do not attribute an entire appended log to this deployment.

## Completed regression controls on the deployed stack

The controls run sequentially through the interactive `HeliosNativeDgcControl`
scheduled task. They use stock definitions, audio off, SystemInfo on, monitoring
off and online submission off. No performance/debug or admission override is
enabled. `HELIOS_WSI_ASYNC_PRESENT=1`; the removed feedback knob is cleared.
The CLI is 2.32.8454 64 and SystemInfo 5.92.1497.0. Each receipt archives the
definition, result ZIP, exported XML, actual settings, loaded module hashes,
driver logs and before/after registry snapshots.

| Completed stock control | Scores | Measured FPS | Evidence |
|---|---|---|---|
| Time Spy 1.2 | overall21,813; graphics22,903; CPU17,181 | GT1 157.847717; GT2 125.318718 | `controls/native-dgc-timespy-validation.json` |
| Fire Strike 1.1 | overall36,018; graphics59,097; physics40,946; combined8,765 | GT1 259.785706; GT2 254.168198; combined40.768192 | `controls/native-dgc-firestrike-validation.json` |
| Steel Nomad Vulkan | 9,387 | 93.877197 | `controls/native-dgc-steelnomad-vulkan-validation.json` |

After all three controls, the scheduled task is idle, no 3DMark process remains,
the adapter is still Code0 and `desktop-after-controls.png` shows the desktop.

Time Spy's demo, both graphics tests and CPU test all have status0 in
`Arielle.xml`. Its executed stock definition matches the preceding2AD1 run
byte-for-byte, and every recorded setting matches except the adapter LUID
changed by reboot. The four workload processes load system D3D12/Core/DXGI,
UMD12 `41A7E290…` and ICD `43394BBD…`; no app-local engine or WARP is used.
The Time Spy workload executable is unchanged1.2.6.5, SHA256
`3b810d25ac67dd3da44464b67b38ace38e4d4f78cc0082117e176dc567329a94`.

Fire Strike's five workloads also have status0. The stock definition matches
the preceding2AD1 run byte-for-byte; actual settings differ only in generated
result IDs. All five workload processes load the expected native UMD11 and ICD
with system D3D11/DXGI. Host VNC captures `firestrike-frame-04.png` and
`firestrike-frame-05.png` show changing GT1 scenes, and02/03 show the demo.

Steel Nomad Vulkan completes its selected workload with status0, uses the
expected ICD and loads the system Vulkan loader. Its stock definition matches
the preceding2AD1 run byte-for-byte; actual settings differ only in adapter
LUID. `steelnomad-vulkan-frame-02.png` and03 show different rendered scenes,
including the moving vehicle. The archived result explicitly uses Vulkan;
it is the Vulkan regression control, not native D3D12 evidence.

Host VNC captures `timespy-frame-05.png` and `timespy-frame-06.png` show
different GT1 scenes (frames1830 and8783); `timespy-frame-07.png` shows GT2.
These establish changing rendered frames. Owner visual acceptance of all three
controls is pending;
the earlier accepted .266 shadows do not transfer to this build.

The new run is a current performance observation while the owner has stopped
other GPU work. Compared with preceding2AD1's GT1 138.871033 / GT2 117.138313,
the scores are higher, but the reboot also updated the Windows graphics runtime.
This is not an isolated measurement of either the DGC or marker-fence change.
No gain is attributed to an individual change.

Time Spy initially appeared stalled at loading, then progressed without a
restart or synchronization change. Its demo/loading interval was284 seconds,
versus293 seconds in the preceding2AD1 run; no new loading regression is
established. Read-only noninvasive guest debugger snapshots were taken during
the demo, before the measured graphics tests; the later attempted host attach
found that demo process had already exited. The raw evidence and attribution
limits are retained in `native-dgc-timespy-stall/interpretation.json`.
The existing pending-allocator Reset diagnostics recur; this completed run does
not resolve allocator/fence-worker ownership, nor establish zero DDI refusals.
