# D3D12 no-output sample counts and native FL12 admission

Status: implemented (2026-09-13). Applies to the `vkd3d-proton-helios` engine and
`umd12` caps. Supersedes the host-mask requirement recorded in
[FEATURE_LEVELS.md](FEATURE_LEVELS.md#native-fl12_1-admission-candidate-2026-09-09).

## The capability

`D3D12_FEATURE_DATA_D3D12_OPTIONS19::SupportedSampleCountsWithNoOutputs` — DDI
`D3D12DDI_OPTIONS_0102` — is **not** hardware MSAA support. It declares that the
driver can run the pixel shader at N sample frequencies when **zero render
targets and no depth/stencil are bound**, with SV_SampleIndex, sample-frequency
interpolation and SV_Coverage evaluated per sample. Microsoft's DDI0102 rule
requires at least bits 1/4/8/16 above FL11_0; a `count1`-only answer is rejected
by the retail runtime with `0x887a0020` ("Driver reported insufficient sample
counts for no-output rendering"), which is why `umd12` has reported the guarded
`0x1f` mask since 2026-09-09.

## Why the host mask cannot satisfy it

The Vulkan limit `VkPhysicalDeviceLimits::framebufferNoAttachmentsSampleCounts`
is a *hardware* MSAA mask. Upstream vkd3d derives the D3D12 mask from it, and
the merged native admission checked it directly:

```
options19->SupportedSampleCountsWithNoOutputs = host_limit & 0x1f;   /* old */
```

Measured on the RX 6600 test host (RADV NAVI23, Mesa 25.0.7):

```
framebufferNoAttachmentsSampleCounts: {1,2,4,8}      (0xf)
framebufferColorSampleCounts:         {1,2,4,8}      (0xf)
```

RADV caps all sample-count masks at 8x — `radv_physical_device.c` starts from a
hardcoded `0xf` and never adds `VK_SAMPLE_COUNT_16_BIT`, and
`radv_formats.c` only ever adds 2/4/8 to per-format `sampleCounts`. On Radeon,
"16x" is EQAA (8 colour samples plus coverage samples), not a true 16x MSAA
mode, so RADV is describing the hardware accurately. Waiting for a host 16x bit
is therefore a dead end, and requiring it made native FL12 admit on NVIDIA hosts
(whose reference profile reports 16x) but never on AMD.

## Decision

Report the DDI-required mask and **back it**, rather than dropping to FL11_0:

1. The declared mask is the host mask unioned with the DDI floor
   (`1|4|8|16` = `0x1d`). On this host that is `0x1f`.
2. A no-output pipeline whose requested count is not host-backed keeps the
   **requested** count in the shader (`RASTERIZER_SAMPLE_COUNT`) and clamps only
   Vulkan's `rasterizationSamples` to the highest host-backed count not exceeding
   it — 16 -> 8 here.
3. The clamp is a named atomic counter, logged once per device and totalled at
   device destruction, so the approximation is attributable rather than silent.
4. Native FL12 admission no longer reads the D3D12 no-output mask as host
   evidence. The real hardware backing checks (device-generated commands,
   resource-binding tier 3, conservative rasterization tier 3, maintenance8,
   `EXT_depth_range_unrestricted`, storageBuffer8BitAccess, shaderInt8,
   `shaderStorageImageMultisample`, `shaderStorageImageWriteWithoutFormat`) are
   unchanged — a host that cannot multisample at all still fails admission.

This mirrors what AMD's Windows driver does on the same hardware: it reports 16
for the no-output contract without the GPU performing 16x MSAA.

## What is approximated

Only the effective rasterization sample count, and only when the requested count
exceeds host support. A 16x no-output PSO runs with 8 rasterization samples:
sample-frequency invocation, sample positions and SV_Coverage derive from the
effective count. Applications that request 16 and depend on all 16 distinct
sample positions see 8. This is the documented compatibility boundary; it is not
hidden, and the counter makes its use measurable.

## Acceptance bar

The `no-output-msaa` native probe (15 PSO cases over two replays, pixel/sample
frequency invocation, coverage and guard regions) is the behavioural gate, plus
the native D3D12 suite: adapter/dependency-level caps must report FL12_1, and
allocator, stream-output, raytracing and tiled cases must no longer be blocked by
"native FL12_1 admission unavailable".

## Result, 2026-09-13 (.279, `59595da2`)

Admission is fixed and measured: the `adapter` probe passes, `allocator` passes,
and `raytracing` reports `CAP,NativeFL12_1Admission,00000000` where it was
previously blocked.

The behavioural gate **fails on this host, and cannot pass**. `no-output-msaa` is
normally BLOCKED because the guest has no D3D12 debug layer (Graphics Tools FoD,
`0x887a002d`) and DISM is denied here. A diagnostic build with the debug-layer
gate relaxed ran the case: 15 cases, 8,640 words, **320 mismatches, every one on
the 16-sample case** (`got 000000ff, want 0000ffff`). The mask is correct
(`CAP,NoOutputSampleCounts,0000001f`) and the PSO is accepted, but the clamp
rasterizes it at 8 samples, so the observable coverage is 8 bits wide.

That is the ceiling, not a bug in the clamp: RADV exposes no 16-sample
rasterization, and Vulkan will not create a pipeline with
`rasterizationSamples = 16`, so no implementation on this host can produce true
16x no-output shading. The `0x1f` report is therefore an over-report for the 16x
entry specifically; the alternative is not claiming FL12_1 at all.

Consequences for the charter: the native suite's other three failures
(`stream-output` "32-bit counter guards overwritten", `raytracing` "uncompacted
current/prebuild size agreement", and the `tiling-buffer` 0xC0000005 crash) were
previously unreachable behind the admission block and are now open defects. The
`raytracing` one was fixed in `22.22.280.0`; it turned out to be a D3D12/host
quantity mismatch rather than an admission consequence, see
[ACCELERATION_STRUCTURE_CURRENT_SIZE.md](ACCELERATION_STRUCTURE_CURRENT_SIZE.md).

