# Target independent rasterization

This contract implements inherited FL11_1/FL12_0/FL12_1 forced sampling with
color attachments. Native FL12_1 admission, full feature-level conformance and
DXR/Port Royal remain separate requirements. WDDM2.1 and the native static UMD
architecture are unchanged. The implementation uses the owner-authorized
renderer/protocol pair in [SUBSTRATE](SUBSTRATE.md#native-dgc-contract).

## Required behavior and implementation

The [Direct3D functional specification](https://microsoft.github.io/DirectX-Specs/d3d/archive/D3D11_3_FunctionalSpec.htm)
sections 3.5.6.2, 17.18 and 20.4.6 distinguish the rasterizer sample pattern,
color-target samples and occlusion counts. Forced counts are 1/4/8/16. Forced
counts above one require single-sample color targets. Forced one permits MSAA
color targets. Depth/stencil testing, shader depth/stencil outputs and sample
frequency shading are invalid. The fixed output mask addresses target samples;
input coverage and pull interpolation follow the rasterizer pattern.

| Required behavior | Host Vulkan | Venus / renderer | Engine / native UMD | Validation |
|---|---|---|---|---|
| Forced4/8/16 into color1 | NVIDIA610.57.04 NV_framebuffer_mixed_samples | Paired fork exposes extension and attachment sample-count structure | Native MERGE coverage reduction, one stored color sample, full forced raster pattern; UMD preserves ForcedSampleCount | Host GPU readback passes; native results below |
| Forced1 into multisample color | EXT_sample_locations, variable locations and center in supported coordinate range | Existing feature/property and pipeline chains forwarded | Coincident pixel-center locations, coverage input reduced to logical bit0, effective sample-count specialization | Host2/4/8 target samples pass; RGBA32_FLOAT target16 unavailable |
| Fixed mask and shader output coverage | Native fixed/shader masks precede coverage reduction | Normal pipeline/shader forwarding | For color1, bit0 broadcasts to all raster bits; fixed mask0 suppresses invocations | DXBC/DXIL coverage and UAV invocation readback |
| Alpha-to-coverage | Physical Vulkan A2C would use raster count | Shader forwarding | Color1 uses a monotonic one-step alpha mask at 0.5; ordered comparison rejects NaN. Explicit SV_Coverage output disables this mask | Endpoints, quarter/three-quarter alpha, NaN and explicit-mask precedence |
| Blending, logic operations and write masks | Native single-sample output merger | Normal Vulkan state | No intermediate multisample resolve or shader blend emulation; independent target state retained | Float additive/reverse-subtract, MRT write masks, two uint targets with XOR |
| Occlusion queries | maintenance5 early multisample/sample-mask ordering properties | Properties forwarded | Early fragment tests count original raster coverage. Forced1 duplicates are divided per physical query fragment before reduction; PSO divisor changes fragment the logical query | Occlusion/binary counts, output-mask changes, shader discard/coverage/A2C, two replays |
| Failure and lifetime | Normal pipeline failures and fences | Existing error/retirement paths | Invalid counts, forced>1 with MSAA targets, depth/stencil and sample-rate shaders reject with E_INVALIDARG. Required missing extension/property rejects with E_NOTIMPL. Native resources remain alive through authenticated completion | Invalid PSOs leave device usable; replay, barriers, guards, completed readback |

The [Vulkan fragment-operations contract](https://docs.vulkan.org/spec/latest/chapters/fragops.html)
places coverage reduction before the color output merger. Default coverage
modulation is NONE. `VkAttachmentSampleCountInfoNV` belongs on the graphics
pipeline create chain, including fragment-output libraries and monolithic
fallback variants. Library cache keys contain values with null pointers;
creation fixes up pointers to the copied descriptor. Forced rasterization counts
must survive dynamic attachment-state selection.

D3D permits occlusion queries to retain original raster coverage when late
shader discard, output coverage or alpha-to-coverage disables all output. The
implementation deliberately uses that permitted count. It does not fabricate
query results. Both maintenance5 ordering properties are required before this
path is admitted; vendor names do not substitute for these properties.

The existing forced-one implementation and query normalization were already in
the prepared .271 source. This change enables the mixed-sample extension in the
engine, implements the forced4/8/16 output path and adds comprehensive bounded
behavior tests for both branches. Private shader options55/56 describe these
translations. Shader-interface revision5 invalidates earlier dirty-build caches.

## Builds and native validation

Evidence lives under `tmp/tir-native-20260909/`. Linux and Windows engine builds,
release UMD12 and the A1 mechanical checks pass (211 KMD logic tests). Direct
NVIDIA tests pass mixed-sample DXBC/DXIL coverage/query cases, MRT/logic cases and
invalid pipelines. Host validation-layer runs report no Vulkan errors. An initial
combined host run failed a later Vulkan instance creation; a fresh DXIL process
completed all three forced counts. The failed combined run remains failed evidence.
An initial MRT test used an uninitialized viewport because the shared test helper
returns early for no-render-target setup; explicit viewport/scissor setup fixed
that fixture before the successful MRT run.

Release UMD12 SHA256:
`0C292592A61ED15E7C134060C6804AC58261564E9C1B7A5559B391141A83B26C`.
It is hotplugged at
`C:\ProgramData\HeliosUmd\helios_umd12_0c292592a61ed15e.dll` after an adapter
restart, on .271/oem54/Code0. No guest reboot or host restart was needed.
The signed package's UMD12 remains the earlier 41A7 build; this is a ProgramData
iteration, not a new signed package. UMD11 stays57C84ED4, Mesa43394BBD, and the
local host renderer/server stay06ce3964/afed7176. Registry UmdD3D12 remains1.

`build-windows/provenance.json` records 143 mirrored source hashes, the release
UMD and seven static archives. The separate final native-test build receipt
records compiler dependencies and the test executable. Native execution pins the
Helios adapter, verifies exact UMD12 and system runtime paths, and records loaded
ICD/runtime hashes. Positive GPU cases inspect the native debug message queue
after fence completion. Invalid-PSO cases intentionally expect API rejection.

The final interactive run `tir-20260909-230217-569/` completes all 13 groups,
778,722 assertions with zero failures/skips/todos/bugs. There are 420 TIR readback
records:210 scenarios, each replayed twice. The nine TIR groups alone have 777,072
assertions; the remaining 1,650 are ROV/conservative regressions. All six legal
PSO configurations that previously had four failures now create successfully.

| Native group | Assertions | GPU readback records |
|---|---:|---:|
| Legal attachment creation | 6 | 0 |
| Forced1, DXBC and DXIL | 500,720 | 168 |
| Forced4/8/16 color1, DXBC and DXIL | 251,472 | 228 |
| MRT independent blending/masks and integer logic, both formats | 24,856 | 24 |
| Invalid PSO inputs, both formats | 18 | 0 |
| ROV and conservative rasterization regressions | 1,650 | separate existing fixtures |

`native-validation.json` independently grades completion, exact runtime/UMD/ICD
identity, archive hashes and the positive TIR debug-message results. The test
executable is `2059A5E63B4C8414A020AAE56D1CE4BFF15680C987ACC73037C9EE62C5675E70`.
`native-inputs/receipt.json` supplements the header dependency inventory with
13 actual translation units, object hashes and the binary (139 recorded inputs).
The native debug helper uses the Windows SDK separately from the engine's
partial generated debug interface; its build uses SDK10.0.28000.0 and LLVM22.1.8.
The GPU run uses the system runtime/Core10.0.26100.9278 and DXGI10.0.26100.9444.
The final test fixture change only prevents the generic Windows suite from
requiring a debug layer unless the pinned native runner selected it; the native
runner still requires the expected hash and enables the layer before creation.

The production inputs remain byte-identical to the archived 0C292 release build;
no feature-level/shader-model/RT reporting changed. These GPU tests close the
previously demonstrated mixed-attachment refusal. The ROV/conservative fixtures
do not inspect InfoQueue, so the TIR debug-queue result must not be attributed
to them. Existing pending-command-list allocator-reset diagnostics recur in
engine logs despite successful completion/readback, and remain unresolved.

## Acceptance limits

These are bounded TIR tests, not every FL12_0/FL12_1 shader/format/rasterization
obligation. RGBA32_FLOAT target16 is unexercised when its quality query returns
zero. Broader formats, centroid/input-inner-coverage combinations, line/point
rasterization, pipeline libraries across processes and every allocation-failure
injection remain separate coverage. No performance benchmark was run for this
change. Host VNC confirms the desktop after deployment; offscreen readbacks do
not establish owner visual acceptance of any benchmark scene.

Native DXR remains unadvertised pending complete state-object, shader-table,
AS/address/dispatch validation. Current `vkd3d_acceleration_structure_deserialize`
creates only the destination AS view; it does not prepare future referenced BLAS
views. The [Vulkan deserialization contract](https://docs.vulkan.org/refpages/latest/refpages/source/vkCmdCopyMemoryToAccelerationStructureKHR.html)
requires referenced BLAS objects to exist before deserialization even though
their contents need not be built. This is a rechecked source/spec boundary, not
a newly observed native crash. Next: implement and validate that cross-submission
reference/lifetime contract, then coherently admit native DXR and SM6.3 and run
the native AS/state-object/shader-table tests before Port Royal. Port Royal has not completed. The sparse
compatibility semantic gap, native DGC statistics discrepancy, general sharing
and consumer release, host-loss retirement callback, allocator-reset lifetime,
resize/rotation and async WSI stress limits remain open.
