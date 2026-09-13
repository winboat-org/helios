# Native DX12 feature-level contract

**No-output sample counts, 2026-09-13:** the no-output sample-count mask is no
longer treated as host MSAA evidence. `SupportedSampleCountsWithNoOutputs` is the
DDI0102 *driver contract* (1/4/8/16 above FL11_0, retail-rejected otherwise); the
engine now reports the host Vulkan mask unioned with that floor and backs any
excess by clamping Vulkan's `rasterizationSamples` while the shader keeps the
count the application asked for. This unblocks native FL12_0/12_1 on AMD/RADV
hosts, whose Vulkan MSAA mask stops at 8x. See
[NO_OUTPUT_SAMPLES.md](NO_OUTPUT_SAMPLES.md).

**Owner scope, 2026-09-12:** prioritize readiness for general native FL12_0/12_1
and DXR1.0 application testing. Tools-visualization output, extreme DXR limit
audits and physical non-RT hardware validation are deferred unless a concrete
workload makes them blockers; the owner will test non-RT hardware separately.
Retain conditional DXR admission and the documented sparse compatibility fallback.
Only bounded fallback improvements justified by actual failures are in scope;
do not pursue a general sparse emulation project. These exceptions prevent a
full conformance claim but do not independently block general testing. The
F6D00A83 allocator build's completed Port Royal was visually accepted on
2026-09-12. The merged C7241DE6 build is now ready for general testing: native
admission, GPU readback, conditional-RT refusal, allocator and ordering checks
pass, along with 36 broader native feature groups and all four full benchmarks.
Port Royal completes at 12,134; the earlier owner's visual acceptance is not
automatically transferred to this build. See the current
[readiness and provenance record](../../ROADMAP.md#ready-for-general-fl12-testing-2026-09-12).

**Current optional-DXR contract, 2026-09-11:** release465CBE13 replaces unconditional
RT admission with engine-derived adapter caps and per-device identity/capability
revalidation. An otherwise eligible non-RT engine reports RT0 and retains
FL11_0..12_1. Native caps and GPU/refusal validation, including the restricted-RT
test's hardware limits, are in
[conditional DXR support](DXR_SERIALIZATION.md#conditional-dxr-support).
Full FL/DXR compliance and extreme AS-count/recursion limits remain open;
the owner-accepted Port Royal run remains the preceding057934F9 artifact.


**Native DXR, 2026-09-11:** release 057934F9 reports RT1.0/SM6.3 with matching
engine admission checks. The native Windows DXR probe passes state-object,
AS mutation/copy/serialization, shader-table, bundle and cross-queue ray readback.
[DXR_SERIALIZATION.md](DXR_SERIALIZATION.md#native-dxr-admission-and-readback-2026-09-11)
records exact source/build/deployment and loaded-runtime evidence, including
the RT1.0 payload and explicit-export association repairs. Tools
visualization remains an unsupported operation and conformance gap; its use by
Port Royal has not been demonstrated. Port Royal now completes at 12,337 / 57.12 FPS
with changing frames. The owner visually accepted that 057934F9 run on 2026-09-11
("Looks correct"); full conformance remains a separate requirement.
See the [completed run](DXR_SERIALIZATION.md#completed-native-port-royal-2026-09-11)
and [regression controls](DXR_SERIALIZATION.md#completed-regression-controls-2026-09-11)
on the same 057934F9/.271/43394BBD stack.

The current owner-directed work is genuine native FL12_0 and FL12_1. DX12 has
priority. Feature-level conformance and DXR/Port Royal are separate acceptance
requirements; neither FL12_0 nor FL12_1 alone requires ray tracing. FL12_2 and
Speed Way follow. DX11 FL12_1 is a separate optional extension. DECISIONS.md
continues to govern the native, statically linked UMD architecture.

**2026-09-09 superseding deployed checkpoint:** the owner-authorized renderer
fork now carries native DGC through the paired protocol and Mesa. The private
engine fallback is removed, and queue retirement uses authenticated wire fences
after the renderer's exportable-marker fix. [NATIVE_DGC.md](NATIVE_DGC.md) is the
current cross-stack matrix for these changes. Its 2,404 indirect GPU checks pass;
the initial compute-query failure has now been repaired for D3D12 by
[DGC_QUERIES.md](DGC_QUERIES.md), including native Windows readback. The paired .271/oem54 guest and local renderer are now activated. Native
FL12_0/12_1 admission, root/IA DGC readback, root signatures, stream output
and four ordering cases pass; tiled/inherited cases have18 passes and the
expected tier2 3D refusal. Full FL12_0/12_1 or DXR conformance is not claimed. Older
fallback and stock-renderer rows below are dated evidence of previous builds.
Time Spy, Fire Strike and Steel Nomad Vulkan subsequently complete on this
stack with archived results, matching render settings, verified loaded modules
and changing host-VNC frames. [NATIVE_DGC.md](NATIVE_DGC.md#completed-regression-controls-on-the-deployed-stack)
records scores and the Windows-runtime attribution limit. Owner visual
acceptance remains pending; these controls do not exercise native DXR.

The owner subsequently authorized reserved-resource committed compatibility
backing, with dynamic detection of advertised-but-broken color4 behavior.
[SPARSE_COMPATIBILITY.md](SPARSE_COMPATIBILITY.md) is the current policy and
explicit gap record. It supersedes the strict sparse-format refusal of the
archived 8C747 candidate; complete tiled conformance remains unestablished.

FL12_0 requires tiled tier 2, binding, typed-UAV, format/MSAA, shader and inherited
D3D12 behavior. FL12_1 additionally requires ROV and conservative rasterization.
[ExecuteIndirect tier1.0](https://microsoft.github.io/DirectX-Specs/d3d/IndirectDrawing.html#feature-tiers)
is required even at FL11_0. Native DGC now supplies root-state and IA VBV/IBV
changes in source. Previous fallback readback evidence does not validate this
replacement; see [NATIVE_DGC.md](NATIVE_DGC.md).
The current compatibility candidate admits FL12_1 with tiled2 after the
no-output rasterization repair below. Native RT1.0/SM6.3 is now admitted with
engine backing checks; admission and complete conformance remain separate requirements.

Keep WDDM 2.1. Microsoft's [FL12_2 specification](https://microsoft.github.io/DirectX-Specs/d3d/D3D12_FeatureLevel12_2.html)
lists WDDM 2.0 as its driver-model minimum. This corrects the older automatic
WDDM 2.9 claim; it does not establish that every obligation is implemented in
Helios. Any proposed version change needs a concrete requirement and evidence
from the actual native guest runtime. Archives remain historical and read-only.

## AS input repair, 2026-09-10

Current ProgramData UMD12 is `8F15F9DC…` on unchanged .271/oem54/WDDM2.1 and
Mesa `43394BBD…`. [DXR_SERIALIZATION.md](DXR_SERIALIZATION.md#as-build-inputs-and-prebuild-failures-2026-09-10)
records checked build inputs/prebuild failure cleanup and geometry strides,
34,489 engine checks on each direct NVIDIA/paired Linux Venus stack, plus
native caps and four GPU ordering cases with exact loaded identities. Native
creation remains FL11_0..12_1 successful, FL12_2 refused, SM6.0/RT0. The new
RT behavior remains unreachable through native admission; tools visualization
and native DXR/Port Royal acceptance are still open. No reporting override or
conformance claim was added. Earlier binary-bound tests below keep their scope.

## Adapter admission contract, 2026-09-10

That increment's ProgramData release UMD12 was
`898F75F99E0E76A1AA3BFEE876C6F2D67B0833C8D31F1E89F4BD0A2622AC461E`,
at `C:\ProgramData\HeliosUmd\helios_umd12_898f75f99e0e76a1.dll`.
`tmp/fl12-admission-20260910/` contains the source/build/deployment receipts and
the completed direct-DDI and native-runtime results. Root remains master at
dcdb8b38, with the accumulated uncommitted dependency work preserved. The seven
static engine archives are byte-identical to the preceding 2D90C57E build;
this increment changes only the Rust adapter/capability code and test tooling.

All seven adapter-handle callbacks now reject foreign/null handles before
reading arguments or writing outputs. Six return E_INVALIDARG; the size callback
returns zero. AdapterUnrecognised counts these refusals. The previous code logged
the mismatch but ignored its boolean result. Device destruction takes a device
handle and is outside this adapter-token check. The token remains shared by
adapter opens; this does not implement per-open lifetime or closed-token tracking.

The extended feature-level query reads only HighestRuntimeSupportedFeatureLevel
and writes only MaximumDriverSupportedFeatureLevel. Reading the whole structure
previously required an initialized output field; writing it also rewrote the
input. Selection now chooses the highest supported WDK enumerant within the
runtime limit and rejects limits below every known level without modifying the
output. This handles the enum's numeric gap between CORE=2 and 11_0=10. Future
limits above the driver's ceiling clamp to that ceiling. The legacy query still
clamps at FL12_1, and the advertised DDI remains exactly R8_0110. No feature or
shader-model cap was raised, and no capability override or new probe export was
added.

`tools/d3d12-adapter-probe.ps1` builds on local C: and runs an interactive task
against an explicitly hashed UMD via its real OpenAdapter12 adapter table. It
checks both query selectors, known/lower/future/malformed runtime limits,
unaligned and short buffers, untouched guards/tails, count/fill negotiation,
wrong DDI versions/interfaces, three foreign handles across all seven callbacks
and continued use of the valid token. A protected input page independently
detects writes to the query's input member. That is a directional-memory stress
case, not an observed retail-runtime page protection. The fixture catches the
baseline access violation and finishes with a recorded failure.

The final identical executable completes 184 checks: baseline 2D90C57E has
36 failures; candidate 898F75F9 has zero. These are direct DDI contract tests,
not native-runtime or GPU conformance. The old valid runtime-limit case already
worked; the negative fixtures do not establish a previous retail admission
failure. Linux A1 passes, including 211 KMD logic tests. The Windows release UMD
build has zero compiler errors/warnings; its receipt verifies 152 mirrored
inputs and all seven static archives. The probe uses WDK/SDK 10.0.26100.0 and
MSVC 19.51.36252 with warnings treated as errors; the driver retains
LLVM/libclang22.1.8, VulkanSDK1.4.350.0 and bindgen0.72/layout assertions.

After adapter restart, native PID8076/session1 loads this exact UMD, ICD
43394BBD… and system D3D12/Core10.0.26100.9278 / DXGI10.0.26100.9444. FL11_0,
FL11_1, FL12_0 and FL12_1 creation succeed; FL12_2 returns 0x887a0004. The runtime
actually calls the extended DDI with limit14; the driver answers13, with
AdapterUnrecognised=0. The legacy selector and other synthetic maxima are covered
by the direct DDI fixture. Native caps remain tiled2/binding3/ROV1/conservative3/
SM6.0/RT0, with the existing counted TotalLaneCount1024 estimate. Four native
ordering cases pass 65,536 words each, including CPU-held admission and
cross-process shared fences. Native module receipts exclude WARP/app-local
runtime substitution. .271/oem54/Code0, explicit UmdD3D12=1, UMD11 57C84ED4…,
ICD43394BBD… and the Sep10 21:27:40 guest boot remain. This is a ProgramData
override, not a rebuilt signed DriverStore package or hosted-CI result.

Host VNC shows a healthy desktop. No benchmark or performance comparison ran;
earlier benchmark receipts and owner visual acceptance remain separately scoped.
Full FL12_0/12_1 conformance remains open because of the authorized sparse
compatibility exception and broader behavior/lifetime coverage. Port Royal is
not ready: native RaytracingTier is still zero, tools-visualization postbuild
type1/copy mode2 are explicitly refused, and native state-object/DXIL/AS/shader-
table/DispatchRays acceptance remains pending. The current Vulkan/Venus copy
contract exposes opaque AS operations, not DXR's decoded geometry output; see
[DXR_SERIALIZATION.md](DXR_SERIALIZATION.md). The next DXR implementation is a
complete visualization representation through build/update/copy/serialization,
followed by native DXR readback before Port Royal. General consumer release,
host-loss callback, sharing/resize/teardown/async WSI stress and pending allocator
Reset/fence-worker lifetime remain separate acceptance work.

## Previous AS recording candidate, 2026-09-10

Release UMD12 `2D90C57E…` is deployed on unchanged .271/oem54/WDDM2.1,
UMD11 57C84ED4… and ICD43394BBD…. Native FL12_1/tiled2/SM6.0/RT0 remain.
[DXR_SERIALIZATION.md](DXR_SERIALIZATION.md#as-address-and-copy-range-validation-2026-09-10)
records the AS address failures, packed clone/compact readback and creation-view
versus accessed-range evidence. Current native regressions pass19 groups /
781,199 checks. The earlier inherited/tiled and benchmark evidence below is
bound to its recorded binaries; no full-conformance or new visual acceptance
follows from this candidate. Tools-visualization and native DXR remain open.

## Pipeline statistics update, 2026-09-10

[DGC_QUERIES.md](DGC_QUERIES.md) records the current release UMD12 `22C31F11…`
on .271/oem54/WDDM2.1, with unchanged UMD11/ICD/renderer. Native DGC compute
statistics now pass the previously failing continuation/replay cases and new
GPU-count/predicate/stride/readback tests. Four native groups pass 2,218 checks;
NVIDIA, Venus and Intel engine controls also pass. The raw host Vulkan counter
remains deficient, and the engine now supplies the D3D12 statistic using GPU
metadata. DGC command execution remains native. Inherited formats, typed UAVs,
descriptors and shader behavior are being validated independently of admission.

## TIR implementation update, 2026-09-09

The mixed-attachment refusal described in the dated 2AD1 evidence below is
superseded by [TIR.md](TIR.md). UMD12 `0C292592…` implements forced4/8/16
with native mixed-sample coverage reduction and validates the existing
forced-one path. Host readback and builds pass. All 13 native TIR/ROV/conservative groups now
complete with zero failures/skips; 420 TIR readback records cover 210 scenarios
replayed twice. Positive TIR GPU cases inspect the native debug queue. Native caps remain FL12_1/tiled2/RT0.

## Evidence and admission

## Native FL12_1 admission candidate, 2026-09-09

Release UMD12 `2AD1D25D480822DE675A667E30D4F756ACBD2A6B1BCEF765D351CB15E93411F2`
is hotplugged on unchanged KMD22.22.270.0/oem53.inf/WDDM2.1, UMD11 `245D1BC3` and
Mesa ICD `BF021927`. Native system runtime/Core10.0.26100.8972 and DXGI10.0.26100.9168
now create FL11_0, FL11_1, FL12_0 and FL12_1; FL12_2 returns `0x887a0004`.
PID3948 reports tiled2, binding3, ROV1, conservative3, SM6.0 and RT0 on Helios
LUID `00000000:01d1c4d3`. Exact module hashes and the unmodified native runtime
are recorded in `tmp/fl12-maintenance8-native-20260908/native-20260909-001745-560-f650e42104174fce8ca96409db873861/`.
This is admission of a compatibility candidate, not complete FL conformance.

Both DDI feature-level queries and native engine admission derive from the
same FL12_1 maximum; the extended query preserves/clamps the runtime maximum.
A new owned-device check refuses missing engine FL/SM, binding/conservative
requirements, no-output sample counts and raw/predicated tiled-copy features.
It also refuses developer feature-level/shader-model overrides. The host check
accepts the real backing and rejects disabled maintenance8. The native negative
case now confirms the same boundary: PID1392 loads exact2AD1/BF02, the engine
reports maintenance8=0 and returns `0x887a0004`; the bridge maps this to native
CreateDevice `E_FAIL` (`0x80004005`). The process terminates normally with the
expected failed creation. `native-missing-m8-validation-2ad1.json` records the
pinned driver-file hashes, loaded-path ETW evidence and zero lost events/buffers.
This denial-only test changes the child environment, not installed capabilities.

The first FL12_1 candidate `06024105` reached the engine but the native retail
runtime rejected its count1 no-output rasterization cap. Direct3D12 ETW records
"Driver reported insufficient sample counts for no-output rendering" with
`0x887a0020`, PID8672, zero lost events. The
[DDI0102 requirement](https://microsoft.github.io/DirectX-Specs/d3d/VulkanOn12.html#sample-frequency-msaa-with-no-render-targets-bound)
is counts1/4/8/16 above FL11_0. The implementation now reports the guarded 1/2/4/8/16
mask and specializes both shaders and rasterization from the effective no-output
sample count. The native `no-output-msaa` case passes 15 PSO cases over two replays,
17,280 words, pixel/sample-frequency invocation, coverage and guard regions,
with no native debug errors. It is independent of tiled resources. Forced
sampling with attachments was previously ignored in vkd3d; nontrivial mixed-sample
TIR now returns E_NOTIMPL. That inherited contract remains an explicit gap.

The 877-production-input manifest is
`9a2e7fd287229d965fdc97300745349da857b686ea611afdbb19f050c487bf9e`;
`no-output-build-final/` under the evidence directory above retains inputs,
patches, the matching Windows mirrors, UMD and seven static archives, imports,
exports and check/release logs. Linux engine and Windows builds pass; A1 is clean
including 213 KMD logic tests. Shader interface revision3 invalidates dirty-build
caches for the effective sample-count specialization (revision2 previously fixed
AS5 immediate-buffer cache invalidation).

Eight native tiled cases pass on this UMD: 53 format checks; buffer/2D tiling
including ten mip readbacks; buffer map/null/skip/reuse/alias and mapping-copy
operations; 2D CopyTiles and predication at byte offsets65536/32/1; and color4
CopyTiles with independent resolve. These are bounded tests, not validation of
the committed fallback's deliberately absent mapping/alias semantics.
The initial depth-array probe was invalid because its mips were smaller than a
tile. After correcting its dimensions, all98,304 sample words matched, but the
native debug layer correctly rejected uninitialized RT/DS destination metadata.
The clear variant first exited1 while process teardown and output-pipe draining
stalled (PID6320). That run remains failed/unresolved stability evidence, not
GPU acceptance. The authorized guest reboot completed at00:38:33 IST; QEMU and
its launcher were not restarted. The same2AD1/ICD/UMD11 identities and Code0
survived reboot. Native inventory PID9108 again admits through FL12_1 on LUID
`00000000:00007823` (`native-20260909-004315-602-46f01df8100b459eb19b111ee87137db/`).

The streamed repetition now passes the initialized D32 array case:98,304 exact
sample words, both CopyTiles directions, independent producer/consumer, raw tile
layout, special values and no debug errors. Five further cases pass: buffer
unmap/lifetime/churn, mapping-signal, a held-wait/remap with independent old/new
GPU witnesses, and two native invalid-input rejections. Those negative cases
prove runtime rejection; DDI delivery is unproven. The complete six-case receipt
is `native-depth-streamed-2ad1/run-20260909-004446-397-0e8a1987/`.
`native-tile-validation-2ad1.json` verifies archived evidence hashes and retains
failed/unrun earlier cases. Fourteen distinct tiled cases plus the separate
no-output case now have bounded native passes. Sparse compatibility mapping/
alias semantics, depth predication and all-format conformance are not thereby
validated.

The tiled runner now streams both output pipes into files during execution,
bounds final draining, freezes partial failure evidence before archiving, and
requires confirmed process termination for acceptance.33 synthetic archive
cases (including an open pipe and a failed read) and17 provenance cases pass.
This repairs the capture hang, not the cause of PID6320's driver teardown.

A dedicated native entry point reuses vkd3d's ROV and conservative-raster tests.
It pins the Helios adapter, checks the loaded UMD's full hash before GPU work,
requires the system runtime and interactive session, and never enables
experimental shader models. DXBC/DXIL ROV each pass798 assertions; conservative
rasterization each passes27 (1,650 total, zero failure/skip/todo/bug). The loader
trace loses zero events/buffers. Its D3D12 messages are four shader-cache registry
`0x80070002` diagnostics and two expected unbound-RT output warnings. Exact test
inputs (94 compiler dependencies), binaries and native receipts are in
`native-features-build/`, `features-20260909-010026-899/` and
`native-features-trace-20260909-010025-786/`. This does not validate every
inherited shader, format or raster limit.

The first remaining native inherited boundary is now demonstrated TIR with
attachments. `tir-20260909-010542-243/`, PID2356, creates ordinary/forced1
single-sample PSOs, but legal forced4/8/16→single-sample and forced1→MSAA4 PSOs
fail. The engine returns E_NOTIMPL; the runtime records a bad UMD error and
returns DEVICE_REMOVED (`0x887a0005`). This is a six-check conformance diagnostic
with four failures, not a passed negative test. NVIDIA610.57.04 exposes
`VK_NV_framebuffer_mixed_samples`; the current Venus encoder has no such
extension or mixed-sample coverage chain. The selected NVIDIA device does not
expose `VK_EXT_multisampled_render_to_single_sampled`; its presence on host
llvmpipe is irrelevant to Helios. The stock-renderer boundary is unchanged. Full TIR coverage/output/sample-mask/blending/occlusion needs
an implementation before claiming complete FL12_0/12_1 conformance.

The derived receipts `native-raster-validation-2ad1.json` and
`native-tir-validation-2ad1.json` verify the corresponding archived compiler
inputs, binaries, native module identities and zero-loss ETW traces. The TIR
runner's `Completed=false` means its pass condition failed; all six checks
executed and the process exited1 without a capture timeout. It must not be
reclassified as a successful negative test.

### Earlier 2AD1 mixed-sample boundary and substitutes

This dated diagnosis predates the authorized renderer fork and [TIR implementation](TIR.md).

The [inherited TIR contract](https://microsoft.github.io/DirectX-Specs/d3d/archive/D3D11_3_FunctionalSpec.htm#3.5.6%20Target%20Independent%20Rasterization)
requires standard forced sample patterns1/4/8/16. Counts above1 allow
single-sample color targets; forced1 allows multisample targets. Coverage and
interpolation follow the forced pattern, while the output sample mask follows
the target. Depth/stencil and sample-frequency shading are restricted. This
is separate from the now-passing no-output sampling cases. Forced1 support
also predates FL11_1; lowering the advertised level alone would not implement
that older missing behavior.

| Mechanism | Actual boundary / disposition |
|---|---|
| `VK_NV_framebuffer_mixed_samples` | NVIDIA610.57.04 exposes it. The installed guest protocol list lacks it, `VkPipelineCoverageModulationStateCreateInfoNV` and `VkAttachmentSampleCountInfoNV`. The [extension](https://docs.vulkan.org/refpages/latest/refpages/source/VK_NV_framebuffer_mixed_samples.html) supplies coverage reduction with optional modulation; a complete engine implementation would still need D3D sample-mask, shader and query semantics. |
| `VK_EXT_multisampled_render_to_single_sampled` | Supported by the Venus code, absent from this NVIDIA device. The [extension](https://docs.vulkan.org/refpages/latest/refpages/source/VK_EXT_multisampled_render_to_single_sampled.html) provides multisample storage/load/resolve behavior. Inference: ordinary end-of-pass resolve does not preserve arbitrary TIR single-target blending/logic operations, so merely substituting it would not close the contract even on another host. Host llvmpipe support is not NVIDIA/Helios support. |
| `VK_EXT_sample_locations` | Both host and Venus source support it. Coincident center locations are a possible ingredient for forced1 with multisample output, not a proven implementation; coverage, interpolation and queries still need validation. Changing locations does not itself permit raster4/8/16 with color1. |
| Fragment interlock / shader output-merger emulation | Native ROV tests establish some useful backing, not a general TIR fallback. A new implementation would need typed-format access, blending/logic operations, masks, interpolation, queries, descriptors, barriers and lifetimes. No performance or correctness claim is made for an unwritten fallback. |

The current encoder source is
`icd/mesa/src/virtio/venus-protocol/vn_protocol_driver_info.h` and its generated
struct bindings. The previously fetched Mesa object
`d253ffa22c4f8436f9a9abf976ec429bc6c02168` also has no NV/AMD mixed-sample
enablement in `vn_physical_device.c`; this check did not move the checkout or
claim a newer fetch. Adding the direct extension path would require the Venus
protocol, guest Mesa, host renderer forwarding and engine implementation;
it would change the present stock-renderer constraint. No renderer or launcher
change was made for this diagnosis.

The owner stopped other GPU work before these completed2AD1 controls, so they
are current performance observations. All selected workloads have status0;
stock definitions, actual settings, exported XML, result archives and exact
loaded native identities are verified in `controls/2ad1-*-validation.json`.

| Completed stock control | Scores | Measured FPS |
|---|---|---|
| Time Spy | overall19,410; graphics20,832; CPU13,999 | GT1 138.871033; GT2 117.138313 |
| Fire Strike | overall35,089; graphics57,616; physics40,676; combined8,479 | GT1 249.901779; GT2 251.116684; combined39.441200 |
| Steel Nomad **Vulkan** | 9,075 | 90.756996 |

Time Spy's preceding6F29 stock run was19,122 overall /20,656 graphics,
GT1=136.699768 and GT2=116.869568FPS. Completed workload settings match except
for the reboot's adapter LUID; driver environment is identical. Observed GT1
+1.59% /GT2 +0.23% is not isolated attribution to a compiler or sampling change:
the shader-cache revision and guest epoch differ. Earlier shared-GPU runs are
not performance comparison baselines for these controls.

Viewed host-VNC pairs show changing Time Spy GT2 and Steel Nomad Vulkan test
scenes. The full Fire Strike run has only a viewed demo frame; a separate
completed GT1 control supplies changing graphics-test frames at frame830/time3.23
and frame4679/time19.00. It runs identical GT1 settings and measures250.660751FPS;
its deliberately incomplete full-score fields are not a new full benchmark
score. `controls/viewed-frames-2ad1.json` keeps the attribution separate. No
paintcap/focus-taking observer, ETW or performance trace ran during benchmarks;
the read-only module observer samples every3 seconds. Owner visual acceptance
is still pending and the accepted .266 shadows/about100FPS remain separate.

The fresh native DXR probe PID4252 successfully creates FL12_1 on exact2AD1/BF02,
then exits BLOCKED77 with `RaytracingTier=0` before any RT command. Its rebuilt
probe/shader, inputs and receipt are under `native-dxr-2ad1/`. Port Royal is not
ready or completed. Full inherited/format behavior, mixed-sample TIR, the sparse
exception, DXR contracts and existing sharing/retirement/lifetime limits remain
separate acceptance work.

Final reconciliation is in `final-2ad1-{source,guest,host}.json`. All877 current
production inputs still match the frozen build manifest; the root remains
master atdcdb8b38 and no dependency HEAD moved. The final guest is Code0 with
explicit DWORD UmdD3D12=1, the same ProgramData UMD/ICD paths and loaded DWM
identities, no active graphical test process, and a viewed normal desktop.
The engine test-build option was restored to `enable_tests=false`. QEMU and
the upstream renderer retain the recorded loaded identities. No new work was
committed or pushed, and hosted CI/package validation remains unverified.


### Preceding raw-copy baseline, 2026-09-08

The owner activated upstream system virglrenderer; deployed Mesa BF021927 now
exposes maintenance8. That preceding release UMD12 was
`6F29859B2585912A241800628E2C66CA59E8079494661B3FFC707B3931ED96B5`, on unchanged
.270/oem53.inf/WDDM2.1 and UMD11. [D32_COPY.md](D32_COPY.md) records the raw D32
path and native DXBC immediate-buffer repair, the 877-input manifest, builds
and loaded identities. Native committed raw-copy/ICB controls pass 1,536 words;
IA12/48-word/query/lifetime, root12/48-word, SO34 and four ordering cases pass.
These controls do not call CopyTiles. Native inventory PID5580 still reports
FL11_0/tiled0/RT0, LUID `00000000:00d4a77b`. Full tiled/inherited/format/ROV/
conservative/DXR conformance remains open. The owner stopped other GPU work;
new completed controls can measure performance, while earlier shared-load
results remain unsuitable for attribution.

The preceding deployed UMD12 was
`BE9D0EBE5F3A021848429A8ED0F641CC908BB1809DF8EC9F3192B1BD44F343A6`,
on unchanged .270/oem53.inf/WDDM2.1, UMD11 and ICD; LUID
`00000000:0711b78c`. Predicated single-sample CopyTiles now performs GPU-controlled
raw row copies, with bounded scratch ownership, edge/offset preservation and
query isolation. Host tile regressions pass 12,151 assertions. The 876-input
manifest is `2d1989ae0f260231c5c88aeef0ade441f84f44794e4799cfbed32383148c00b0`;
`tmp/fl12-predicated-tiles-20260908/native-validation.json` records native
IA12/48-word/12-query/lifetime and four ordering passes with exact loaded
identities. The new native predicated tile case stops at the tiled0 query,
BLOCKED77; caps remain FL11_0/tiled0/RT0. Raw D32 MSAA and full native feature/
DXR acceptance remain open. See
[SPARSE_COMPATIBILITY.md](SPARSE_COMPATIBILITY.md#predicated-single-sample-copytiles).

The preceding deployed IA continuation build is
`361C9767AC9B772D5F9CEAE998E68D2669AF7DFA46E999F63471DD17170129F4`,
on unchanged .270/oem53.inf/WDDM2.1, UMD11 and ICD. The production-input manifest
is `835042eda2b114d0954ad40ca32a1090a002de0846874c387cb2479e258f3c8d`.
`tmp/fl12-indirect-ia-20260908/native-ia-validation.json` independently verifies
the Session1 native IA readbacks, queries, pending-list Reset/dependency and
loaded system runtime/UMD/ICD identities. The LUID is `00000000:06d904c0`.
This adds no FL or RT reporting and has no benchmark/owner visual acceptance.
The existing pending-allocator Reset diagnostics remain an unresolved boundary.

The preceding deployed UMD12 was
`9BDA548C4577008F237978A1658D53005227B0C0E3E0F0EF3DBA02E60BE401A6`, on
unchanged .270/oem53.inf KMD, WDDM 2.1, UMD11 and ICD. Its compatibility policy,
production-input manifest, host tests, guest Vulkan diagnosis and native
regression results are in [SPARSE_COMPATIBILITY.md](SPARSE_COMPATIBILITY.md).
Native caps remain FL11_0/tiled0/RT0. That build's LUID was
`00000000:06376d04` after adapter restart. New native devices rejected the old
probe records as unverified, then read all seven FAIL records after re-probing.
This is cache consumption evidence; native tiled GPU commands remain blocked.

The previous 22D1 baseline is
`22D1323318320016F19CA9BBD38605AB51E6A724FEAE730BFD1C01FFFA8182C1`:
the root-signature contract and CopyTiles byte-offset repair, on unchanged
.270/oem53.inf KMD, WDDM 2.1, UMD11 and ICD. ROADMAP.md and
[ROOT_SIGNATURES.md](ROOT_SIGNATURES.md) link its source/build manifest and
native readback/ordering results. All three stock controls completed with
archived/exported results and changing host-VNC frames; their exact comparison
is `tmp/fl12-root-contract-20260908/benchmark-controls-22d1.json`. The final
PID2216 native inventory loads the intended system runtime and artifacts and
again admits FL11_0 only, with tiled 0/RT0. It is archived at
`tmp/fl12-audit-20260907/native-20260908-022139-573-fa8349c66b514cfaae86a840f4217472/`.
Those 22D1 benchmark and inventory records describe the prior artifact; they
do not establish 9BDA benchmark acceptance. The other dated records below likewise
retain their original artifact identities. Neither higher-feature conformance nor owner visual
acceptance is established.

Starting source is clean root `master`,
`dcdb8b38a0556d554b005e90132ff0979fcc28ba`, with gitlinks DXVK
`e431cef9960dda65cb755d95ffc6a7300306ab53`, Mesa
`a04516a702dff81d3a2e44019cdd79abf3fb7423`, and vkd3d
`71ebda7ec9eee8f826a65844710f3ab8fa1c1dd7`. These are the audit's input,
not a claim that subsequent source edits have been built or deployed.

Fresh inventory is in
`tmp/fl12-audit-20260907/fl12-inventory-20260907-030245-308/`.
The guest is Windows 11 IoT Enterprise LTSC build 26100, KMD
22.22.270.0 / oem53.inf, Code 0, explicit `UmdD3D12=1`. System D3D12 and
D3D12Core are 10.0.26100.8972; DXGI is 10.0.26100.9168. LLVM is 22.1.8,
Vulkan SDK 1.4.350.0, glslang 16.2.0, Meson 1.11.1, Python 3.12.10 and
widl 11.5. WDK/SDK include directories contain 22621, 26100 and 28000;
the build's selected header and the runtime-negotiated DDI are separate checks.

Installed package UMD12 SHA256 is
`ADC0B0EA6F1CEB0C964A37EE6CEC74CFA8B178BB2B6B5460B5AFAB8BF01B19D0`;
at that initial inventory, the local release build was different (`21339235…`). DWM PID 1820 loads
the package's UMD11 (`245D1BC3…`) and the ICD
`C:\ProgramData\HeliosVulkan\vulkan_virtio-3349607be958.dll`
(`3349607BE95819BD7DDA37F3424CB4D91607659174A80E365911B527B296A078`).
DWM's module list is not evidence of a native DX12 process loading UMD12.
Full paths, hashes, registry values and tool outputs are retained in the inventory.

The candidate's initial Windows driver compilation used the selected WDK/SDK
10.0.26100.0, LLVM/libclang 22.1.8 and Vulkan SDK 1.4.350.0. Engine compilation,
both-UMD check/release, regenerated binding-cache comparison and static-link
import checks passed. That UMD12 SHA256 was
`EA963FA9A71717D99BAA7940E1F89802748166722D2D0262A7348B877E25CCCE`;
UMD11 remains the installed `245D1BC3…`. The immutable archive is
`tmp/fl12-audit-20260907/fl12-build-20260907-060823-690/`, with the checked
54-file `source-freeze-round3` manifest and all three Windows probe build
receipts. This build includes the second review's implementation repairs and
the expanded 34-case SO probe. It has not been deployed. The later
`fl12-build-20260907-062651-506/` archive in the same audit directory is a
provenance recapture with the 55-file `source-freeze-round4` manifest and rebuilt
probe receipts after the runner attribution repairs. It rechecks the same
UMD bytes, seven engine archives and 36 mirrored implementation inputs; it is
not another driver compilation.

After the fifth review's native DXR bundle repair, both-UMD check/release passed
again. The resulting UMD12 SHA256 is
`DE97F4AD7C5CC17070A047965548F657F0F429D1F301561474A68EDCEDA1589C`.
After the seventh review's SO-owned allocation repair, both-UMD check/release
passed again, producing
`9543C9118E7176D3218B4ED9D27C3108108937248F4D03AF6AAFCF958875FDB6`.
UMD11 and the seven engine archives are unchanged. These are new Rust UMD builds,
not new engine compilations; none has been deployed. Each later
review ledger binds its frozen source to its own build-provenance capture.
The eighth review's OOM reporting repair passed both-UMD check/release and
produced UMD12
`6344CB095418C1D7B81FB77A3287A58979378DAA367DD2FEDECDA4747381217A`,
again with unchanged UMD11 and engine archives. After two consecutive dry
whole-diff rounds (9 and 10), it was hotplugged into ProgramData at 03:44 UTC.
A PnP device restart refreshed the cached UMD path; no guest reboot was needed.
PID9380 then verified the exact candidate with the system runtime and existing
ICD, under explicit UmdD3D12=1. DriverStore UMD12 remains the older ADC artifact.
The deployment, failed cached-path check, successful native inventory and restart
receipts are retained in the audit directory. The deployed source is exactly
`source-freeze-round9-final`; subsequent SO-probe and contract/comment corrections
are not another driver build. The caps comment correction changes no noncomment
source line.

Mechanical checks passed: UMD12 host clippy with warnings denied,
unsafe-contract/ASCII/slot checks, 213 KMD logic tests and the unchanged four-site
protocol clippy baseline (`a1-mechanical-round9-final.log`). The integrated Linux
engine reported 64 deferred DXR collection assertions, all passing without
skips. The expanded SO result below supersedes the earlier 319 assertions.
These are not native Windows GPU validation. Current host engine mapping/remap
tests also pass 15,816 assertions, and sparse-buffer lifetime passes 36, without
failures or skips (`engine-sparse-round7-provenance.json`). The guarded native
HE12 admission path is not exercised by those host tests. A fresh deferred DXR
64-assertion run and its exact sources/libraries are recorded in
`dxr-deferred-round6-provenance.json`.

The first five independent whole-diff review rounds were not dry. The first
round repaired deferred DXR export indexing/renaming and temporary collection
lifetimes, sparse requirement allocation failure, probe GPU-owner unwinding,
shader replacement and build/module attribution. RDAT export renaming was an
engine-only defect: the [native DXR DDI](https://microsoft.github.io/DirectX-Specs/d3d/Raytracing.html#state-subobjects)
supplies extracted subobjects separately, and the reconstructed driver library
contains DXIL without RDAT. The second round repaired archive publication,
tiled executed-build ownership and native low-32-bit DXR stride translation.
Its original SO-unbind finding was refuted against the full inherited overflow
contract before engine changes. The corrected probe and engine now preserve
per-stream overflow with zero capture capacity for unbound declared targets,
and accept the required four-byte counter storage at an allocation boundary.
The expanded full host SO suite passes 9,246 assertions without failures or
skips; its exact source/build record is `so-round2-repair-provenance.json` in
the audit directory. The expanded 34-case native probe passes its Windows
build; its corrected native run is recorded below. The third round repaired a DXR
receipt replacement race and a parsed-runner attribution race in all three
wrappers. A Windows PowerShell 5.1.26100.9168 witness proves that an already
parsed script can keep executing after its pathname is replaced. Each wrapper
now compares its parsed source with the same bytes hashed against its receipt;
DXR archives the exact receipt bytes used to verify its executable snapshot.
The fourth round reconciled a stale SO Windows-build limitation sentence and
separated driver compilation from subsequent provenance captures. The fifth
round found that the native shared DXR guard rejected legal bundle operations.
The repaired guard permits bundle pipeline binding and ray dispatch while
continuing to reject AS build, copy and postbuild emission on bundles. Two native
readback cases cover explicit bundle roots and inherited parent roots; their
Windows build and MinGW warnings-as-errors syntax checks pass, but RT_NONE
still withholds their native execution. The sixth round was dry; the seventh
found infallible Rust allocation in SO-owned array construction. Translation now
validates and counts expanded entries first, fallibly reserves both arrays, and
returns a named E_OUTOFMEMORY error with the shader handle already clear.
OOM reporting updates its counters atomically and defers allocating diagnostic
summaries until later normal readouts, so persistent allocation failure cannot
abort this recovery path before its callback or return.
Shared shader-container and Slot allocations remain a separate, infallible OOM
boundary; this local repair does not establish complete shader-creation failure
coverage. The eighth round found the same diagnostic-allocation hazard after
fallible tiled/DXR translation and a reserved-resource engine OOM return.
Those native E_OUTOFMEMORY reporting arms now update their counters atomically,
including missing-device/callback fallback counters, and defer allocating
summaries until normal readouts. The shared RenderCb failure arm also avoids
diagnostic allocation before returning a legal E_OUTOFMEMORY from runtime
submission, so a committed tiled operation can reach cancellation and its
device error callback. Tiled cancellation and runtime notification,
DXR device/list callbacks, and reserved/state-object HRESULT returns remain the
error channels. The early tiled reservation failure occurs before commit; it
does not demonstrate an orphaned committed mapping. These source-derived repairs
do not establish native fault-injection behavior or complete shared allocation
failure handling. Native allocation-failure injection remains unexercised. Per-round
dispositions and source-freeze references are maintained in
`tmp/fl12-audit-20260907/reviews/`. Native candidate execution and owner-visible
acceptance remain separate from all of these build and static-review records.

The repaired tiled/DXR probe Windows builds pass. Sixteen synthetic archive
cases exercise successful and BLOCKED child classifications, partial copying,
evidence corruption and final-result publication failure. Evidence is in
`archive-unit-round6/` under the audit directory. Thirteen synthetic
provenance cases use the actual runner verification functions, including
matching/replaced parsed source and a receipt replaced after snapshot
verification (`provenance-unit-round6/`). The interpreter witness is in
`runner-parse-test/`. All three Windows probe builds pass after these repairs.
A Windows
file-lock test also rejects a concurrent tiled Build while all seven locked
artifact/receipt hashes remain unchanged (`file-lock-tests/`). These are runner
mechanics, not real child GPU outcomes or native failure injection.

Build and validation provenance relies on serialized VM access:
concurrent VM builds and deployment during a probe or its
evidence collection are excluded. The wrappers do not enforce a global build
or deployment lease. In particular, concurrent DXR/tiled Builds sharing a
BuildDir could hash replaced source alongside an earlier executable; a direct
emergency overwrite of a plain package DLL after child exit could invalidate
path-based module attribution. Neither interleaving occurred in these records.
Canonical UMD12 hotplug uses content-derived filenames, and normal package
publication uses a separate DriverStore package directory.

## Capability matrix

**Initial inventory, 2026-09-07; superseded binaries and admission below.**
The native inventory in `tmp/fl12-audit-20260907/native-20260907-030943-759/`
ran as PID 9124, session 1, exit 0. It loaded the installed UMD12 above and
Microsoft's system D3D12/Core, plus the content-hashed Venus ICD. The adapter was
Helios, VEN_1AF4/DEV_1050, LUID `00000000:0000784a`, without the software flag.
FL11_0 creation succeeded; FL11_1, FL12_0, FL12_1 and FL12_2 each returned
`DXGI_ERROR_UNSUPPORTED` (`0x887a0004`). The process recorded maximum FL11_0,
SM6.0 and root-signature API 1.1. Its UMD log records runtime maximum 14
(FL12_2), driver maximum 10 (FL11_0), and negotiation of `R8_0110`.
Inventory exit 0 means the inventory completed, not that the higher levels passed.

Host and exact loaded guest ICD evidence are in
`tmp/fl12-audit-20260907/vulkan-capability-matrix-20260907.json`, linked to
`host-stack-20260907/host-vulkaninfo.txt` and
`fl12-vulkan-loaded-20260907-032445-083/guest-vulkaninfo.txt` in that directory.
The latter capture records PID 4516's loaded ICD and loader hashes, not merely
an installed manifest. Host NVIDIA 610.57.04 exposes Vulkan 1.4.341. The guest
exposes 1.4.341 with an embedded Mesa `git-a717d4edd6` identifier; that string is
not a source/build provenance substitute. The installed stock virglrenderer
package is 1.3.0-2, with package integrity verification successful. The active
render-server executable's SHA256 is
`1628bce57954b8b0172d0ef89e97da3d678816d5077538ef35e9953019e68aa4`.

This matrix starts from the complete inherited [Microsoft hardware feature-level table](https://learn.microsoft.com/en-us/windows/win32/direct3d12/hardware-feature-levels),
its per-format requirements and the FL12_2 specification. Source support, queried
Vulkan support, native reporting and exercised behavior are separate columns.
The current native UMD is `2D90C57E…` on .271/oem54/WDDM2.1, ICD
`43394BBD…` and the paired renderer. Admission, engine support and complete
behavioral conformance are separate facts. Current evidence is in
`tmp/dxr-copy-ranges-20260910/`;22C31F11 evidence is retained in
`tmp/fl12-query-contract-20260910/`. Earlier GPU and sparse receipts remain
bounded to their recorded binaries. No inherited row is closed by admission alone.

| Required feature/tier | Host Vulkan support | Venus exposure | vkd3d support / boundary | Helios DDI | Native caps / admission | Validation |
|---|---|---|---|---|---|---|
| FL12_0/12_1 binding >=2 and typed UAV additional formats | Descriptor indexing, unformatted loads/stores, large descriptor limits | Corresponding features/limits exposed | Descriptor/typed view and shader paths implemented | Descriptor heap, root, view/counter translation | binding3, typed loads1 | 22C31F11 native typed loads/stores/counters, atomic and descriptor groups pass; large descriptor DXBC/DXIL cases each check >262K results; full limit exhaustion remains unexercised |
| FL12_1 ROV and conservative raster >=1 | Fragment interlock, conservative degenerates/fully-covered input | Exposed | Native raster and shader translations | Rasterizer/PSO and shader forwarding | ROV1, conservative3 | Current native 13 raster groups pass 778,722 checks, including DXBC/DXIL ROV and conservative rasterization |
| FL12_0/12_1 tiled >=2 | Sparse features and standard shapes present; some 2D format/MSAA combinations absent or broken | Per-format limitations preserved | Sparse buffers/images where supported; owner-authorized committed image fallback otherwise | Reserved creation, mapping/copy, tiling/mips and CopyTiles paths | tiled2; 3D reserved textures refused at tier2 | Current native suite has 18 PASS cases and one expected tier-2 reserved-3D refusal; committed fallback intentionally lacks mapping/alias/residency semantics, see SPARSE_COMPATIBILITY.md |
| Mandatory ExecuteIndirect tier1.0 root/IA state | EXT DGC present | Paired protocol, encoder and renderer carry native EXT DGC | Native root constants/descriptors and VBV/IBV; private command emulation removed | Signature translation, validated handles/offsets and native execution | FL12_1 admitted through guarded engine contract | Paired native root/IA readback, predication and pending-list reset pass on 41A7; current query controls exercise both root compute and IA graphics |
| Pipeline statistics across graphics/compute and internal operations | NVIDIA omits DGC CS counts; Intel counts them | Same NVIDIA boundary | GPU compute-count fragments plus physical query fragments; 64-bit gather and replay reset | Query begin/end/resolve forwarding | Inherited core contract | Current native four groups pass 2,218 checks/56 expanded readbacks; NVIDIA/Venus/Intel each pass 2,166; raw Vulkan counter remains deficient, see DGC_QUERIES.md |
| Stream output, including streams/gaps/overflow/NULL | Transform feedback; 4 buffers/streams, stride2048, data512; nonzero-stream line/triangle raster capability false | Same | Register-aware DXIL/DXBC and partial capture paths | Owned SO declarations, strides, null-unbind and VA validation | Base SO contract | Prior native 34 corrected cases pass; output-limit exhaustion remains separate |
| Sampling, BC/arrays/cubes, GS/HS/DS, shader and format limits | Corresponding shader/filter/raster/format features | Exposed; per-format restrictions preserved | Compiler and Vulkan lowering | Shader and PSO translation | SM6.0; native FL12_0/12_1 creation | Current 23 inherited groups pass 529,085 checks, including sampling, MSAA arrays, geometry/tessellation and format reports; new min/max static/dynamic sampler cases add 259 checks / 60 pixel readbacks with DXBC/DXIL; not exhaustive shader/format certification |
| Forced sampling and no-output rasterization | NV mixed samples, EXT sample locations, maintenance5 | Paired fork carries features/properties | Coverage reduction, sample-mask/A2C lowering, forced1 query normalization | ForcedSampleCount preserved in rasterizer | No-output counts1/2/4/8/16, six mandatory attachment cases | Current native no-output17,280 words and TIR420 readback records pass; optional RGBA32_FLOAT16 unavailable |
| Root signatures, bundle state, pipeline libraries | Push constants, descriptors and engine cache support | Exposed | Public64 DWORD roots; private expanded instrumentation still exceeds native DGC push-UBO support in one engine-only case | Versioned flags, clear arguments, native bundles and PSO plumbing | Root signature1.1; public64 | Prior native root flag/clear checks pass; >64 private native instrumentation unexercised, not a validated public-root refusal |
| GPU VA, queue ordering, resource epochs/lifetimes | BDA, timeline semaphores, native fences | Native authenticated retirement through paired renderer | Queue batching, allocator ownership and resource tracking | Exact runtime admission, epoch and staging/barrier handling | VA40, direct/compute/copy queues | Current build passes all four native ordering cases, 65,536 words each; pending-allocator Reset/fence-worker lifetime and loss callback remain unresolved |
| DXR for Port Royal (independent of FL12_1) | AS, ray pipeline/query/culling/indirect trace/maintenance1; required vertex formats | Features exposed; opacity micromaps absent and not required | Existing state/AS/DispatchRays engine; native guard verifies SM6.3/RT1.0; tools formats and arbitrary alias/lifetime cases incomplete | Installed state objects, reconstructed DXIL libraries, AS operations, descriptors, shader tables and DispatchRays | 057934F9 native RT1.0/SM6.3; FL12_1 ceiling | PID8620/session1: seven native DXR groups and20 ray words pass with explicit alias/local-SRV/collection lifetime; same probe fails on E21352DA; Port Royal completes at 12,337 / 57.12 FPS and is owner visually accepted on 057934F9; full DXR conformance remains separate |
| FL12_2 SM6.5/DXR1.1/mesh1/VRS2/feedback0.9 | Most backing features present; float32 denorm preservation false | Corresponding features exposed | Higher-SM exception and mesh/VRS/feedback paths require behavior checks | Mesh/VRS/feedback and DXR1.1 incomplete | FL12_2 withheld; SM6.3/RT1.0 | No FL12_2 or Speed Way acceptance |
| Remaining FL12_2 tiers/flags/limits | Binding3/tiled3/conservative3 and related shader/timing/format features require full reconciliation | Host capability is not native support | Some engine paths exist | Depth bounds, extended formats, bundles and shader obligations still pending | binding3/conservative3, but tiled2/depth bounds0 | Full Microsoft FL12_2 requirement audit and behavior suite remain subsequent |
| Both FL query forms and DDI negotiation | Not GPU capabilities | Not ICD capabilities | Guarded native engine admission | Extended query writes only output and selects a supported enumerant within runtime limit; legacy clamps <=12_1; one R8_0110 token; foreign adapter handles refused | Native runtime understands12_2, driver reports12_1 | 898F75F9:184 direct DDI checks pass across both selectors and synthetic limits/buffer/version/handle failures; native FL11_0..12_1 creation passes separately |
| Reported SM6.3 wave properties (beyond the FL12_1 SM5.1 floor) | Subgroup size is known; physical lane-count derivation needs a supported topology source | Native source still lacks a measured lane-count path | Engine uses the 32-times-subgroup fallback without vendor topology properties | shader_caps retains the counted estimate | Wave32/32, TotalLaneCount1024 | Current native inventory confirms the report, not its accuracy; CapsTotalLaneCountGuess remains nonzero. This pre-existing reporting obligation is open, see DDI_REFERENCE.md section11.7 |

Native FL12_1 admission, TIR and the demonstrated DGC compute query defect are
implemented and have native evidence. The reserved-resource compatibility
exception is still explicit; it prevents a claim of full genuine sparse
conformance. Native RT readback and the completed, owner-accepted 057934F9 Port
Royal run satisfy their bounded workload checks, not full FL/DXR conformance.
Microsoft defines [TotalLaneCount as the hardware SIMD lane count](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ns-d3d12-d3d12_feature_data_d3d12_options1);
the current 1024 estimate is not promoted into measured hardware evidence. The
cross-stack topology/reporting repair is separate from the completed DGC query
and minimum/maximum filtering behavior checks.
The following paragraphs retain the dated 2026-09-07 indirect-fallback audit;
the owner replaced that policy with native DGC on 2026-09-09.

The 2026-09-07 fallback audit checked the current dirty engine at base
`71ebda7ec9eee8f826a65844710f3ab8fa1c1dd7`, fetched upstream
`35bdee1435c94f8c3548725fcb046595b263bd7e` (2026-09-04), and implementation
history. Source snapshots, hashes and commit patches are recorded in
`tmp/fl12-audit-20260907/upstream-vkd3d-indirect-35bdee1435c9/check.json`.
That audit was source inspection. The later dirty fallback implementation and
host tests are tracked separately in [INDIRECT_EMULATION.md](INDIRECT_EMULATION.md).

The indirect changeset has completed six non-dry whole-change reviews, over
73, 76, 79, 81, 81 and 83 files. Their repairs include native OOM error delivery and explicit
per-PSO SO origin; a legal HLSL semantic no longer selects private register
matching. The current repaired source builds on Linux and Windows, and its
public SO legacy/stream cached recreation, private register capture, DXBC user
capture and indirect producer tests pass 36/36/34/33/138 assertions with Vulkan
validation; the full host SO filter passes 9,316 assertions. The third round's
private SO exception boundary and DXR wrapper timeout findings are repaired.
Eight simulated exception-boundary cases and 30 archive/timeout cases pass;
neither proves native fault injection or compiler-wide OOM cleanup/safe retry.
The fourth round repaired scratch HRESULT preservation through SO, root-state,
query and predication recording; known AS reserve/calloc OOM classification was
also corrected, still outside native RT admission. An extracted-helper
ASan/UBSan test passes synthetic errors and reuse checks. Host GPU regressions
again pass 138/1,570/140/10 assertions plus all 9,316 SO assertions with Vulkan
validation, and Linux/Windows builds pass. These remain engine/synthetic
evidence, not native execution of the new fallback.
The fifth round found an exception escaping the lazy graphics/compute factory
while holding the new cache mutex. An isolated C++ guard now returns HRESULTs
inside the C ownership scope; temporary-root release and unlock execute before
return, and only success publishes a variant. The actual extraction passes
twelve simulated cases and two cached lookups on Linux and Windows, including
production Windows mutex operations. Linux/Windows builds and focused
138/1,570/140 GPU regressions pass. Compiler-wide cleanup and safe retry remain
unestablished. The sixth round closes the local guard and repairs allocating
native diagnostics after the void ExecuteIndirect return: only the existing
atomic forwarding count remains, readable in the device summary. It counts
recording calls, not successful GPU actions or completion. Sustained OOM is
not globally validated. A1 and both-UMD Windows check/release pass; the repaired
candidate is UMD12 `CB48D9DB…`. The engine is unchanged from
the preceding GPU tests; `1400C52F…` is an earlier archive.
IR7 and IR8 subsequently achieved two consecutive dry whole-change rounds with
different lens compositions over the same 83-file freeze (`f81f76e3…`). CB48 was
hotplugged after PnP restart, with unchanged UMD11/ICD/KMD and adapter LUID
`042a5ac7`. The native graphics root-constant/CBV probe passes 12 cases/48 words,
SO passes 34 cases and all four ordering regressions pass. The independent
receipts are `native-cb48-root-validation.json` and
`native-sync-cb48d9db/root-validation.json` under the audit directory. This is a
ProgramData override; the signed DriverStore UMD12 remains `ADC0B0EA…`.
Detailed identities, evidence and the unchanged capability ceiling are in
the indirect and stream-output contracts. New evidence documentation is not a
new driver build or a retroactive change to the frozen review receipts.

| Non-DGC path | Confirmed scope and limitation |
|---|---|
| Current ordinary indirect execution | Action-only DRAW/DRAW_INDEXED/DISPATCH use Vulkan indirect commands, with existing count/predication paths. This does not implement per-command root/VBV/IBV changes. |
| Historical compute state-template fallback | `d3d12_command_signature_init_state_template_compute` and `d3d12_command_list_execute_indirect_state_template_compute` supported root constants and raw-VA root CBV/SRV/UAV changes plus DISPATCH. A compute shader prepared per-command UBOs and dispatch arguments, followed by push descriptors and indirect dispatches. It required the corresponding push-UBO/root-descriptor setup. |
| Historical graphics/mesh state changes | Required EXT DGC or the former NV DGC path. The compute fallback did not cover them. |
| Audited base's unsupported state templates | Without DGC, signature creation cleared `requires_state_template`; execution reached the `DGC skip` return for state payloads. The dirty candidate replaces this with implemented root-state translation or explicit refusal; the successful signature followed by skipped work is not retained. |
| Latest upstream application fallback | `4511d2404355c8eae9816dcb9d5dc7d6a651b42f` adds `FAIL_UNSUPPORTED_STATE_TEMPLATE`, returning `E_NOTIMPL` so applications can choose their own fallback. It supplies no engine emulation. |

The compute implementation was introduced by
[`c216a9c9`](https://github.com/HansKristian-Work/vkd3d-proton/commit/c216a9c9f940848ee91a2244fc156a0196ddafff)
and removed, together with old NV DGC paths, by
[`76c11d2e`](https://github.com/HansKristian-Work/vkd3d-proton/commit/76c11d2e2b90b0a46dc894508e67e2aaacc2c04d)
on 2026-04-08. The audited base and fetched upstream had not restored it. The
candidate reuses its GPU mapping approach with current, private root-layout and
PSO variants and graphics support. The historical path never supplied a
complete graphics ExecuteIndirect solution or native Helios conformance.

Tiled MSAA includes both engine implementation work and a demonstrated stock-host
boundary. The earlier claim that it was not a missing host feature is withdrawn:
global sparse feature flags did not establish per-format support or correct binds.
[D3D11.3](https://microsoft.github.io/DirectX-Specs/d3d/archive/D3D11_3_FunctionalSpec.htm)
sections 5.9.2.3, 5.9.2.5.1, 5.9.2.8.2 and 5.9.7.1 require four samples for
otherwise-MSAA-capable tiled formats except 128bpp.
[Resource Heaps](https://microsoft.github.io/DirectX-Specs/d3d/ResourceHeaps.html#reserved-resources)
carries reserved-resource capabilities forward, and
[CopyTiles](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12graphicscommandlist-copytiles)
defines linear sample order within each pixel. Stencil format families,
96bpp, video, R1 and the packed R8G8/G8R8 formats are excluded; BC is not
multisampled. A blanket zero-quality tiled-MSAA report cannot make tiers2/3
complete. Packed-mip array reflection also needs reconciliation with the
per-slice wording in Microsoft's `D3D12_PACKED_MIP_INFO` contract.

The old claim that tiled resources were the only FL12_1 blocker is withdrawn.
In particular, full stream-output limits, indirect execution, per-format/MSAA
requirements and every inherited obligation remain in the audit. Root flags,
ClearRootArguments and driver 128-DWORD capacity are now implemented; the latter
still needs a native runtime-instrumentation witness beyond 64. See
[ROOT_SIGNATURES.md](ROOT_SIGNATURES.md) for the precise acceptance boundaries.

The subsequent offset audit found that the candidate's CopyTiles guards wrongly
required64-KB buffer alignment. The API defines a byte offset. Both guards are
removed; single-sample image offsets that violate Vulkan texel-block/four-byte
copy alignment use one 64-KB scratch tile with transfer read/write barriers and
row copies that preserve edge padding. Ordinary aligned copies stay direct.
The combined host CopyTiles regression passes 936 assertions with no failures or
skips, including 27 new byte-offset/format cases against an independent
CopyTextureRegion readback. This covers color, BC1/BC3, D16/D32 and 3D edge tiles;
it does not implement MSAA. Source/build evidence is in
`tmp/fl12-root-contract-20260908/build-copy/`. The expanded native tiled probe
builds, but advertised tiled 0 still withholds command execution. A blanket
zero-quality MSAA report remains insufficient for tiled tier 2.

MSAA continuation must cover direct, compute and copy lists, all of which permit
[CopyTiles](https://learn.microsoft.com/en-us/windows/win32/direct3d12/recording-command-lists-and-bundles#command-list-api-restrictions).
The loaded guest exposes `shaderStorageImageMultisample`, but not maintenance8/10
color/depth-copy features. The current Mesa encoder and the inspected stock
renderer protocol headers have no corresponding feature-chain serialization;
their bundled Vulkan header definitions alone do not expose an extension.
This is a boundary for that prospective shortcut, not proof that engine
translation is impossible. No host component changed during this audit.

The deployed command-buffer split was opportunistic and could decline at its
sequence limit or on allocation/begin failure. The later candidate reserves
mandatory continuation slots, propagates errors, and selects compute or graphics
queues before recording work. The contract and remaining query/failure-path limits
are in [EXECUTION_SYNC.md](EXECUTION_SYNC.md#required-queue-continuations).

## MSAA candidate and stock-host boundary

The following is the immutable pre-compatibility 8C747 evidence. Current backing
selection and its new tests are in [SPARSE_COMPATIBILITY.md](SPARSE_COMPATIBILITY.md).
The current candidate supports committed color/D16 tile copies and explicitly
refuses raw D32 MSAA CopyTiles after readback exposed special-value loss.

The undeployed engine candidate implements lazy device-owned pipelines in
`tiled_copy_meta.h`, allocator-owned image views and recording in
`tiled_copy_command.h`, and per-sample read/write shaders. Color copies use
compatible integer views, with storage/mutability changes confined to reserved
MSAA images. Writes to depth use graphics; reads use compute. GPU predicate
snapshots and unaligned buffer offsets remain GPU operations. Copies preserve
edge padding, restore image layout/state, and order potentially aliased tiles.
No CPU readback, GPU-idle wait or backing-release shortcut is added to production.
This is implementation scope, not proof of its complete contract or performance.

`VK_EXT_depth_range_unrestricted` and `VK_EXT_depth_clamp_zero_one` are negotiated
from actual support for the proposed raw D32 write path. Exact special-value
preservation, including NaN payloads, remains unexercised; those extension names
alone cannot establish bit-exact depth copies. Unsupported sparse format creation
now returns failure instead of committed storage which ignores tile mappings.
Predicated single-sample copies and MSAA copies with unvirtualized scoped queries
are explicit `E_NOTIMPL` recording failures, not completed support.

The first sample-copy readback failures were reproduced outside the engine:

| Check on stock NVIDIA 610.57.04 | Result and attribution |
|---|---|
| D16_UNORM / D32_SFLOAT, optimal 2D sparse, usages transfer SRC/DST and transfer+depth attachment | Ordinary image properties include sample4, but sparse format properties return count0 for sample4 and count1 for sample1. Host and exact loaded Venus ICD agree. |
| RGBA8_UNORM 513x513, array2, first tile bound; clear then readback | Sample1 passes 16,384 pixels. Sample4 clear/resolve/readback fails 2,048 of 4,096 pixels: rows32..63 return zero. Both runs have zero core/synchronization validation errors. The smaller 257x131 sample4 case has the same result. |
| Same format/513x513/array2; bind last tile of layer1 at (512,512), extent(1,1) | Sample1 bind completes. Sample4 bind returns `VK_ERROR_DEVICE_LOST` while waiting for the bind fence; zero validation errors. No image copy or shader is submitted by this case. |

The raw image uses only sparse binding/residency/alias flags and ordinary
attachment/sample/transfer usages: no mutable-format, extended-usage or storage
flags. It selects the NVIDIA host directly. These failures therefore do not
depend on the new CopyTiles shader, Venus serialization, Helios DDI or KMD.
The host log also records `NV_ERR_INVALID_OFFSET` from `MapWithSecInfo`; there is
no demonstrated renderer defect or reason to change WDDM. Vulkan defines sparse
offsets/extents in texels and permits an edge extent ending at the logical image
boundary ([VkSparseImageMemoryBind](https://docs.vulkan.org/refpages/latest/refpages/source/VkSparseImageMemoryBind.html)).

Evidence under `tmp/fl12-msaa-20260908/`:

- `host-formats.log` and `guest-formats-verified/{result.json,guest-formats.log}`:
  interactive session1/PID8344, LUID `00000000:05296ca9`, loaded ICD
  `3349607BE95819BD7DDA37F3424CB4D91607659174A80E365911B527B296A078`.
  The query-only probe is `tools/vulkan_sparse_format_probe.c`; no D3D12 admission
  or GPU execution is inferred from it.
- `raw-vvl-results.json`, `raw-vvl-edge-results.json` and `raw-vvl-*.log`:
  raw Vulkan control/failure exit codes and explicit validation output.
  `raw-readback-source/bind-readback.c` reproduces the archived readback binary
  byte-for-byte; `bind-readback.c` adds the bind-only mode and validation-error
  counts on API failure. Argv119 is single-sample readback,
  argv87 is matching MSAA readback, argv241/209 are single/MSAA array-edge
  bind-only controls. All use 513x513. Argv23 is the smaller MSAA readback.
- `queue-repair-test_copy_tiles_msaa.log`: completed final host engine run, 3,393 assertions,
  189 color readback failures and 18 unavailable depth-format skips. The unsupported
  creation checks pass; skipping required formats is not a tiled-tier pass. Earlier
  test setup errors, malformed guest receipt and timeout runs remain excluded from
  acceptance. The array MSAA test remains blocked by the raw bind failure.
- `queue-repair-results.json` and `queue-repair-indirect-query.json`: final
  copy-queue6,291,594, single-sample CopyTiles936, indirect138/1570 and query140
  assertions pass with zero Vulkan validation errors. The first copy-queue run
  had correct pixels but four queue-family validation errors; it is not a pass.
  Repairing aspect/feature-aware graphics continuation selection closes those
  ordinary copy cases, not sparse MSAA or native continuation acceptance.
- `candidate-verification.json`, `source-manifest.json` and `build-windows-final/`:
  UMD12 SHA256 `8C7471DA959FF699FA11B7804580B3101D1DB271D0B0F0BC09E1686568592C0D`,
  872 verified production inputs, manifest SHA256
  `c99066205d6046e5c8e83a34766f08b1ed060a07e71df72ae1fa30bba34f0db7`.
  Linux/Windows engine and release UMD builds, imports/exports and A1 pass.
  LLVM22.1.8/VulkanSDK1.4.350.0 are verified; the engine uses VS2022/MSVC14.44
  and the query probe uses VS18/MSVC14.51. KMD/UMD11/ICD/deployed UMD12 are
  unchanged. `evidence.json` links the source, binaries and bounded outcomes.

Native sample copies, alias/remap ordering and lifetimes, failure injection,
depth-special-value preservation and full inherited requirements are unfinished.
There is no native FL12_0/12_1 acceptance, new benchmark result, visual acceptance
or measured performance result for this candidate. Full sparse behavior needs
a stock host driver that passes these reproductions; the owner-authorized
compatibility path permits continued implementation meanwhile. No host component
or launcher has been changed. IA VBV/IBV indirect emulation remains independent implementable
work. The existing native caps stay FL11_0/tiled0/DXR0.

## Post-deployment native evidence

The previous 22D1 build/deployment and its validation are recorded in ROADMAP.md
and [ROOT_SIGNATURES.md](ROOT_SIGNATURES.md). The earlier CB48 results are in
[INDIRECT_EMULATION.md](INDIRECT_EMULATION.md#reviewed-deployment-and-native-evidence).
The earlier evidence below belongs to the reviewed `6344CB09…` baseline. Its first
native inventory after hotplug (PID5984) loaded cached DriverStore ADC and failed
identity validation before emitting capability rows. After the authorized PnP
restart, PID9380 succeeded in
`native-20260907-091607-215-249ecb1ebdc0460ba42213edd437284d/`. The runtime still
understands maximum14 (FL12_2), the driver reports10 (FL11_0), and R8_0110 is
negotiated. The device remains Code 0 with LUID `00000000:022f39b4`.

The exact 124-byte OPTIONS and 64-byte SHADER DDI replies contain conservative3
and ROV1; native API fields are0/0. Microsoft's [D3D12 feature-level table](https://learn.microsoft.com/en-us/windows/win32/direct3d12/hardware-feature-levels)
lists these features as unavailable at maximum FL11_0, optional at11_1/12_0,
and required at12_1. The [ROV](https://microsoft.github.io/DirectX-Specs/d3d/RasterOrderViews.html)
and [conservative rasterization](https://microsoft.github.io/DirectX-Specs/d3d/ConservativeRasterization.html)
designs also require WDDM2.0+. This is consistent with eligibility filtering;
the current D3D12Core internal branch is untraced. No cap value or feature-level
ceiling was raised, and no ROV/conservative-raster GPU behavior is validated.

The old installed ADC SO baseline fails pipeline creation before submission.
Candidate6344 passes the first four iteration-zero cases, but the original probe
then expected SV_VertexID to include StartVertexLocation. The diagnostic records
20/gap/30/40, correct for vertex ID0. Microsoft's [HLSL contract](https://microsoft.github.io/hlsl-specs/proposals/0015-extended-command-info/)
excludes that draw offset. The repaired probe supplies explicit per-draw VS root
constants, keeping distinct freshness/order expectations and identical positive
and negative root parameters except ALLOW_STREAM_OUTPUT. Two independent reviews
closed this test-only repair. Driver and engine binaries did not change.

The rebuilt probe passes all 34 cases at PID8440/session1 in
`native-so-tagged-6344cb09/run-20260907-092322-874-8c557f35/`; all 15 evidence-file
hashes and source inputs match. Its 34 submissions each complete before checked
readback; forwarding counters alone are not that witness. This is bounded native
SO acceptance, not full feature-level, benchmark or owner visual acceptance.

The existing native synchronization probe, freshly built from unchanged source
`45345226…` to executable `1AA1853E…`, passes all four cases on candidate6344.
`native-sync-6344cb09/native/20260907-103251-988/` records session1, exit0,
Helios LUID `00000000:022f39b4` and both producer/consumer 65,536-word readbacks.
Cases cover producer-first and consumer-first cross-queue waits, CPU fence rewind
and future release, and an inherited cross-process shared-fence handle. The gated
cases retain their negative unsignaled interval and unchanged readback sentinels.
The separate `fl12-sync-loader-6344/` zero-loss process/image trace identifies
parent PID10136 and child PID9192 loading exact candidate/system-runtime/ICD hashes
through their complete lifetimes, with no WARP or app-local vkd3d. All 216 image
load/unload payloads crossmatch the independent XML decode; its malformed time
offset is not used for timing. The root validation binds 29 evidence files.
These are bounded native FL11_0 ordering checks, not sparse/DXR, full conformance,
fault injection or broader sharing/teardown acceptance.

All 12 tiled cases in `native-tiled-6344cb09/` return BLOCKED77 at tier0, with
exact candidate UMD hashes. `native-dxr-6344cb09/` returns BLOCKED77 at FL12_1
creation, before an admitted device. Its post-call module snapshot has system
runtime modules and no UMD/ICD; a per-PID UMD log exists, but that snapshot does
not prove a loaded UMD hash for this failed creation. No tiled or DXR command
behavior is exercised by these blocked results. Neither is a skipped pass.

## Candidate implementation and evidence grading

The candidate does not change FL11_0, TILED_NONE or RT_NOT_SUPPORTED. These are
intentional admission boundaries while required behavior remains missing. It
adds no force-admission option and never uses an engine feature-level override.

* **Implemented, not natively exercised:** reserved-resource/tiling translation;
  mapping preparation, heap ownership, overlap snapshots, exact runtime admission
  and sparse completion; single-sample CopyTiles; ordinary DXR state-object and
  command translation, including
  bundle SetPipelineState1/DispatchRays and inherited compute roots.
* **Refused:** malformed mapping arguments remove the affected device through
  `TileMappingsRefused`; unsupported/malformed CopyTiles invalidates its command
  list (native argument failures increment `L3cCopyTilesRefused`, engine failures
  are returned by Close). Unsupported DXR state/command arms use the existing L9
  refusal counters and API/DDI error channels. The deployed DDI forwards root-state
  signatures and IA VBV/IBV through the isolated no-DGC continuation described in
  INDIRECT_EMULATION.md. The deployed BE9D candidate implements color/D16
  MSAA tile copies, with raw D32 explicitly refused; native tiled0 still prevents
  workload admission to these paths. Predicated single-sample copies now execute
  in the host engine suite; active non-virtualized scoped queries still refuse.
* **Unreachable through advertised capabilities:** native tiled and RT workloads
  that respect the current caps. Calling an internal engine test is not native
  admission and must not be reported as such.
* **Natively exercised at a bounded scope:** all 34 corrected SO cases pass on
  exact candidate6344 and again on CB48, with system runtime, Helios adapter and ICD identities,
  authenticated per-submission completion, readback, reset/rebind ordering,
  overflow, rasterization, gaps and four-byte heap-end counters. Full SO limits,
  tessellation/clip-cull/dynamic-row cases and native OOM injection remain open.
  CB48 also passes 12 native graphics constant/CBV indirect cases/48 words and
  four ordering regression cases. Compute/indexed/SRV/UAV indirect forms remain
  host-tested but natively unexercised. The invalid-extent probe's native
  E_INVALIDARG is consistent with core-runtime interception, not an engine
  error-latch or OOM witness.

All counters are per-process and read through the UMD's `D3D12 DDI refusals:`
summary. Their interpretation is part of the acceptance contract:

| Counter | Grading |
|---|---|
| `TileMappingsForwarded` | Successful engine FIFO commit only; require nonzero for a mapping probe, never treat as admission or completion. |
| `TileMappingsAdmitted` | Render accepted and admission event queued; must equal successful mapping commits on a clean run. Still not a signaled event or GPU completion. |
| `TileMappingsRefused` / `L3cCopyTilesRefused` | Zero for valid exercised cases. A nonzero expected bad-input case proves only that error path, with matching failing API/device/list result. |
| `ResourceCreateEngineFailed` | Zero for successful creation. A reserved-resource OOM requires the matching E_OUTOFMEMORY return and cleared private handle; this counter also serves other resource creation paths and alone does not identify the failing form. |
| `QueueSetErrorUnavailable` | Must be zero. A counted missing device/callback is not successful runtime error delivery, including after tiled OOM cancellation. |
| `EclSubmitRenderFailed` | Zero for successful submission. A runtime RenderCb E_OUTOFMEMORY must reach cancellation and the matching device error callback; this shared ECL/present/tiled counter alone cannot identify the submitting operation. |
| `L6StreamOutputCreates` | Validated native declaration translation attempted; not successful PSO creation or GPU output. |
| `L6StreamOutputBadArg` | Zero for valid SO cases; expected only with an attributed malformed DDI case. |
| `L6StreamOutputOutOfMemory` | Zero without allocation failure. A nonzero proves only SO-owned array reservation failure; require the matching E_OUTOFMEMORY callback and cleared handle. It does not cover shared shader/Slot allocation. |
| `L3aSoTargetsNullArray` | Native null-array translation to explicit unbound views; a zero is allowed if runtime supplies zero views instead. Verify stopped GPU writes independently. |
| `L9RaytracingStateObjectsCreated` / `L9RaytracingStateObjectsAdded` | Successful engine state-object creation/addition, not dispatch or shader correctness. |
| `L9RaytracingPrebuildInfoForwarded` | Nonzero validated prebuild answer; not an AS build. |
| `L9RaytracingBuildForwarded` / `L9RaytracingPostbuildInfoForwarded` / `L9RaytracingCopyForwarded` / `L9RaytracingPipelineBound` / `L9RaytracingDispatchForwarded` | CPU forwarding only. Require the relevant counters to move for each intended path and separately require Close success, exact queue completion and GPU readback. |
| `L9RaytracingErrorCallbackUnavailable` | Must be zero. A missing callback is not successful error propagation. |

Deliberate absent-export and foreign-producer checks can move
`L9ShaderIdentifierAbsent` and `L9DriverMatchingIdentifierRefused`; require the
matching null/noncompatible API result. Zero is also possible when the runtime
answers before the DDI. All valid-query error counters must remain zero.

Native DXR geometry and shader-table strides use only the low 32 bits of their
64-bit DDI fields, as specified for [geometry strides](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3d12umddi/ns-d3d12umddi-d3d12ddi_gpu_virtual_address_and_stride)
and [shader-table strides](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3d12umddi/ns-d3d12umddi-d3d12ddi_gpu_virtual_address_range_and_stride).
Translation normalizes before bounds/alignment checks and COM forwarding. This
rule does not change generic public COM inputs. The Rust check passes; a native
runtime call with nonzero upper bits has not been observed.

DXR's tools-visualization copy/postbuild formats are refused with `E_NOTIMPL`
and the L9 copy/postbuild refusal counters. Serialization postbuild queries now
read the current GPU-produced header at execution, including unknown/mutated
types and closed-list replay; they no longer fail solely for a recorded type.
Cross-submission TLAS deserialization now has an execution-time metadata reader
that creates referenced Vulkan BLAS views before submitting the restore. Small
and 8,194-reference replay/readback tests plus malformed-header containment pass
on direct NVIDIA and Linux Venus. Native RT remains unadmitted; the revised
native probe requires TLAS completion before any BLAS restore is recorded.
[DXR_SERIALIZATION.md](DXR_SERIALIZATION.md) records the contract, provenance,
Vulkan AS copy-range diagnostics and remaining query-scope/lifetime limits.
Older Venus without maintenance7 also lacks the producer UUID evidence needed
for exact foreign-driver versus incompatible-version classification. Neither
limitation is a reason to invent an identifier or return compatible blindly.

These gradings require reconciliation with final code and probe
outputs. Engine invalid-list errors are attributed through the
native Close result and matching engine log, not through a fabricated native
forwarding refusal count.

## Installed workloads and CI

Fresh workload versions/hashes are in
`tmp/fl12-audit-20260907/fl12-workloads-20260907-034100-428/` and Microsoft
signatures in `fl12-workload-signatures-20260907-034206-091/` alongside it:
3DMark 2.32.8454.0, Port Royal 1.4.2.2, Speed Way 1.1.1.2, Time Spy 1.2.6.5,
Fire Strike 1.1.44.0, Steel Nomad 1.0.5.1, SystemInfo 5.92.1497.0. A catalogue's
newer 3DMark version is not the installed product. Speed Way and Steel Nomad
ship Microsoft-signed Agility D3D12Core libraries; their actual loaded modules
must be recorded during a benchmark. They are not app-local vkd3d replacements.
The candidate's completed controls and failed Port Royal attempts are recorded
below. No later owner visual acceptance or performance gain is claimed.

The installed ADC0B0EA baseline completed a full stock Time Spy run, including
demo, GT1, GT2 and CPU, with scores 18,950 overall / 20,335 graphics / 13,673 CPU.
GT1 was 134.903915 FPS and GT2 114.811371 FPS. The native runtime, exact Helios
LUID 30794 and expected UMD/ICD hashes were captured for all four workload
processes. The result, export, settings and manual evidence grading are in
`tmp/fl12-audit-20260907/controls/timespy-before-adc0b0ea/`. The graphics tests
used stock 2560x1440 settings; Time Spy requests FL11_0. Host VNC captured
changing demo and GT1 frames, with no GT2/CPU capture in this baseline. Owner
visual acceptance is pending. The three-second module observer and VNC captures
are recorded instrumentation; this is a baseline, not a candidate result or a
performance improvement.

The same installed stack completed full stock Fire Strike at 36,284 overall /
56,972 graphics, GT1 245.321411 FPS and GT2 250.140366 FPS. Demo, both graphics
tests, physics and combined workloads returned status zero. All five processes
loaded native Helios UMD11 and the expected Venus ICD. The 1920x1080 stock
definition, result/export, loaded modules and manual grading are in
`tmp/fl12-audit-20260907/controls/firestrike-before-adc0b0ea/`. Host VNC captures
show changing demo frames and rendered GT2/physics; two filenames incorrectly
say GT1, with their actual capture stages corrected in the grading. This is an
installed-driver DX11 baseline, not a candidate result or owner visual acceptance.

Two installed-driver stock Steel Nomad Vulkan collections completed at 8,934 /
89.346077 FPS and 8,848 / 88.486877 FPS. Both selected the Helios LUID and loaded
the expected Venus ICD. Their archives are `controls/steelnomad-vulkan-before-adc0b0ea/`
and `controls/steelnomad-vulkan-before-adc0b0ea-frames/` under the audit directory.
The first capture missed rendering; the repeat captured one rendered frame but
its second capture was after completion. Neither establishes changing rendered
frames or visual acceptance. These incomplete baseline captures do not replace
the candidate's required Vulkan control with completed results and frame evidence.

Candidate6344 subsequently completed full stock Time Spy, Fire Strike and Steel
Nomad Vulkan under interactive scheduled tasks. The archives are the audit's
`controls/timespy-after-6344cb09/`, `controls/firestrike-after-6344cb09/` and
`controls/steelnomad-vulkan-after-6344cb09/`. Each contains its result/export,
definition, invocation/environment, loaded module hashes and root validation.
All four Time Spy and five Fire Strike workloads, and the Vulkan workload,
return status0. Time Spy's four processes load exact candidate6344 plus the
system runtime and ICD. Fire Strike's five processes load unchanged UMD11/ICD;
the Vulkan control selects Helios, reports backend `vulkan` and loads the exact
ICD. Auxiliary Microsoft D3D modules do not change that rendering API.

| Full stock control | Candidate score | Measured candidate FPS | Earlier control FPS |
|---|---|---|---|
| Time Spy | overall19,111 / graphics20,418 / CPU14,026 | GT1 136.239441; GT2 114.720390; CPU47.125538 | GT1 134.903915; GT2 114.811371; CPU45.939766 |
| Fire Strike | overall33,605 / graphics55,791 / physics40,243 / combined7,945 | GT1 243.482605; GT2 241.663803; physics127.757706; combined36.958103 | GT1 245.321411; GT2 250.140366; physics127.507080; combined43.629429 |
| Steel Nomad Vulkan | 8,777 | 87.771965 | 88.486877 in the second baseline collection |

The executed settings match except result filenames/IDs and the renewed Helios
LUID (30794 before, 36649396 after PnP restart). The module observer remains
three-second/read-only, async WSI and retire feedback remain1, and VNC sampling
differs. No build/deployment or focus-taking observer overlaps a benchmark.
Candidate VNC pairs establish changing GT2 frames in Time Spy and Fire Strike,
and changing Vulkan graphics frames in Steel Nomad. The images were inspected;
the owner has not supplied visual acceptance. The incomplete Steel Nomad baseline
captures are not promoted into accepted changing-frame comparisons.
Fire Strike's combined FPS decreases by about 15% despite unchanged DX11/ICD
artifacts. Its cause remains unresolved; a completed benchmark does not establish
performance acceptance. These focused comparisons establish no optimization gain.
Time Spy also retains nonzero format/MSAA, root-range flag, optional-table and
allocator-reset diagnostics (`native-limit-counters.json` in its candidate archive).
Completion does not close those contracts or the pending-reset/fence-worker lifetime
question.

One focused Fire Strike repeat on the unchanged candidate completes all five
workloads at status0, with 35,126 overall / 56,747 graphics / 40,388 physics /
8,667 combined. Its GT1/GT2/physics/combined rates are respectively
246.286224 / 247.174545 / 128.216522 / 40.316246 FPS. Exact UMD11/ICD identities,
matching settings and changing combined-test frames are recorded in
`controls/firestrike-after-6344cb09-repeat/`. Candidate UMD12 is not observed by
the three-second module polling; that is not exhaustive loader absence evidence. Combined performance
remains below the 43.629429 FPS baseline and differs from the first candidate's
36.958103 FPS. The cause is not established. No optimization or performance
acceptance is inferred from the repeat.

Port Royal's two stock workloads fail at
`D3D12CreateDevice(adapter, parameters.minimum_feature_level, ...)` with
`DXGI_ERROR_UNSUPPORTED` (0x887a0004). The result archive records status10000 in
both workloads, even though the aggregate score records are zero/status0 and the
CLI returns0. No standalone export is produced; the collector returns1.
`controls/portroyal-after-6344cb09/` preserves that first attempt. The ordinary
three-second module observer misses these short-lived processes, so their logs
are not substituted for kernel loader evidence.

A second attempt, `controls/portroyal-after-6344cb09-etw/`, adds only a read-only
session0 process/image observer. `fl12-pr-loader-6344/` preserves its ETL, decoded
events, artifact hashes and independent zero-lost-events/buffers trace statistics.
Workload PIDs 10012 and 5300 run in session1 for approximately 33 and 31 ms. Each
loads the content-hashed candidate6344 and system D3D12/Core/DXGI; neither loads
Vulkan, WARP or app-local vkd3d. Their UMD logs reach the FL11_0 maximum reply,
then close the adapter without UMD CreateDevice. File hashes were taken after
the trace under the serialized no-build/no-deploy lease. The numeric requested
minimum is absent from the benchmark error and is not inferred from its name.
The observer was stopped before the next control. Neither attempt renders a
Port Royal frame, exercises RT commands or satisfies benchmark acceptance.

After the probe-only SO repair and caps comment correction, whole-diff round11
is dry with independent rotated ABI/engine/failure/completeness/claim and
lifetime/order/ownership/attribution lenses. Both reviewers reread all 55 files,
all 261 hunks and the generated 2136-byte fixture, verify the current probe receipt,
and regrade all 17 affected counters. Driver6344 still maps to the round9 build
freeze and the predeployment round9/10 saturation; later documents and probe
sources have their own `postdeploy-provenance.json` record. Newly collected
benchmark results are separate evidence, not a new driver compilation.

Hosted checkpoint [run 34055565048](https://github.com/winboat-org/helios/actions/runs/34055565048)
on root `dcdb8b38…` completed with overall failure: the driver job built and
uploaded KMD plus native DX11/DX12 UMDs successfully; both Mesa jobs and loader
probes succeeded; CLVK failed at Build CLVK. Final signing/assembly was skipped.
The public API did not provide compiler logs, so the underlying CLVK error is
not established. Raw evidence is in
`tmp/fl12-audit-20260907/hosted-ci-readonly-20260907/`.

## Acceptance

Implement the subsystem contract and use appropriate review, mechanical checks
and native validation. The mandatory review-round workflow was retired by the
owner on 2026-09-08.
Record implemented, refused, unreachable and implemented-but-unexercised paths.

The SO, tiled and DXR acceptance runners require the intended deployed UMD12's full SHA256.
Both package `helios_umd12.dll` and hotplug
`helios_umd12_<16 SHA256 hex characters>.dll` names are recognized; the loaded
full path and digest must match the intended artifact, and a filename suffix
must agree with its digest. The standalone caps inventory takes that full
digest as its sole argument. Filenames alone do not identify a build. The three
runners' build receipts must cover the local runner, source, shared identity header,
executable and shaders, and must be verified before execution. Missing or
mismatched evidence is a failure; `VKD3D_SHADER_OVERRIDE` cannot substitute an
older compiler's output into an accepted run. Tiled execution holds one complete
build and receipt against replacement for the entire case set, and archives its
actual executable with the other inputs. Each of these three runners copies a provisional
failure result first, verifies the complete evidence archive, then publishes and
checks the final result. Archive errors clear PASS/BLOCKED classification and
attempt failed receipts independently at both owned destinations.

The existing synchronization harness uses a separate wrapper in this audit to
check the full intended UMD12 hash, enforce the environment, and copy/verify its
archive. It does not share that provisional-publication procedure: a copy/hash
failure can leave an earlier `Passed=true` receipt while the wrapper fails.
Its pass requires the wrapper's exit status, raw case/readback results and
independent complete archive/loader validation to agree. The native receipt's
pass flag alone is not acceptance.


Use the native Microsoft runtime and exact Helios adapter; record loaded runtime,
UMD and ICD paths/hashes. Exclude WARP and app-local engine replacement. Never
use forced feature levels or unsupported capability overrides. A feature query,
device creation, behavior test and completed benchmark establish different facts.

Graphical probes and benchmarks run through interactive scheduled tasks using
win MCP. Capture through host VNC; no guest focus-taking observer during a
benchmark. Require complete result archives/exports, matching settings and
changing frames. The owner supplies visual correctness. Keep Time Spy, Fire
Strike and Steel Nomad Vulkan regression results separate from Port Royal and
Speed Way and from the full feature-level conformance requirements.

Preserve async WSI, authenticated wire retirement, paired renderer/Mesa, WDDM 2.1, exact
runtime admission/authenticated completion, epochs, staging/barriers, batching,
queue order, scanout protection and safe copy-buffer recycling. Producer
completion is not consumer release. General cross-API ownership/consumer release,
error-bearing host loss/disconnect retirement, sharing/resize/rotation/teardown,
async WSI stress and the pending allocator-reset/fence-worker lifetime question
remain separate acceptance work. The owner's accepted .266 shadows/~100 FPS
and the instrumented 74.26 FPS run do not accept later artifacts visually.
