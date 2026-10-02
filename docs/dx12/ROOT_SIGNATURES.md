# Native root-signature contract

**2026-09-09:** the private 128-DWORD runtime ABI below remains, but the private
indirect fallback and its 64/128-word maps have been removed. Native DGC selects
raw-VA root CBVs up front. Push-UBO expanded roots still reach the engine's explicit
refusal and require native runtime validation; see [SUBSTRATE](SUBSTRATE.md#native-dgc-contract).
The fallback-specific implementation and acceptance paragraphs below are historical.

Root signatures are an inherited obligation for FL12_0 and FL12_1. This
implementation preserves the native D3D12 runtime and statically linked engine;
it changes no feature-level, shader-model, root-signature-version or DXR cap.

## Driver and API boundaries

The [resource-binding specification](https://microsoft.github.io/DirectX-Specs/d3d/ResourceBinding.html)
distinguishes the application's 64-DWORD limit from the driver's 128-DWORD
capacity for runtime instrumentation. The selected WDK's `_0100` creation DDI
has one parsed `pRootSignature_1_2` tree, with version tags 1.1 or 1.2. Runtime
instrumentation is not identified separately to the driver.

`forward12/pso.rs` translates that tree into owned version-1.2 API descriptors,
preserving descriptor-range, root-descriptor and static-sampler flags. The cxx
bridge calls the private `helios_vkd3d_create_root_signature` factory in engine
`state.c`; this entry accepts driver roots through 128 DWORDs. The ordinary
public COM factory retains its 64-DWORD bound. The old serializer remains for
internal empty roots and the bring-up probe, and is no longer the native DDI
translation path. Static linkage and the exclusion of a DXGI import are unchanged.

Parameter masks, arrays, root-constant uploads and indirect root layouts now
cover all 128 indices. Extended roots use raw addresses for root descriptors
and a UBO when Vulkan push-constant capacity is insufficient. Ordinary roots
retain the existing push-descriptor policy and 256-byte upload. Extended UBOs
have a 512-byte descriptor range as well as 512-byte backing; an initial host
GPU readback caught the previously truncated range.

The isolated indirect fallback selects 64 or 128 root words per signature.
Ordinary records remain 256 bytes and their defaults/mapping upload remains
528 bytes; extended records use 512 and 1,040 bytes respectively. This is a
layout statement, not a measured performance gain. Bounds are validated before
shifts/copies. The first recording HRESULT survives Close and Reset. Translation
and engine allocation errors retain E_OUTOFMEMORY and leave failed outputs clear;
the OOM diagnostic path does not allocate.

`ClearRootArguments` calls a private engine operation that zeros both graphics
and compute root arguments while preserving root signatures, heaps, pipelines
and other dynamic state. It flushes pending DGC work and dirties normal root
bindings, including the engine's legal null-descriptor representation. A bundle
has recorded setters rather than independent root shadow state: its clear does
not replay a clear into its caller or discard inherited roots. This is separate
from the API's broader ClearState operation.

## Evidence and provenance

Artifacts are under `tmp/fl12-root-contract-20260908/`. Root master remains at
`dcdb8b38a0556d554b005e90132ff0979fcc28ba`; engine and compiler bases are
`71ebda7ec9eee8f826a65844710f3ab8fa1c1dd7` and
`cc75a0c98d34d7bcc03560527c799b52e48b4d1f`, each with the recorded working diff.
No new work has been committed or pushed.

The root-only build `6125E230A20ADD158BFD49E6ECFF6DA6A58B84F62E73FC5D6D345234604F009B`
is archived in `build/`, with source-manifest SHA256
`59f2419cee69c9eebc0b93d7185f9e5fe8997f8a33a7fd66475ed62463c76994`.
It was hotplugged and natively exercised before the later tile-offset repair.
The combined build is
`22D1323318320016F19CA9BBD38605AB51E6A724FEAE730BFD1C01FFFA8182C1`, archived
in `build-copy/`, with manifest
`0b19b6a28faee04002944708518ab9bbd029c2185735b1aac96763178ba8726f`.
Both captures verify 863 mirrored production inputs. Windows LLVM/libclang
22.1.8 and Vulkan SDK 1.4.350.0, engine compilation, UMD12 check/release and
static-import/export checks passed. Rust had zero warnings/errors. The latter
build is now at `C:\ProgramData\HeliosUmd\helios_umd12_22d1323318320016.dll`.
The .270/oem53.inf KMD, WDDM 2.1, UMD11 `245D1BC3...` and ICD `3349607B...`
are unchanged. DriverStore UMD12 remains the older ADC0B0EA artifact: this is
a ProgramData hotplug, not a driver-package upgrade.

Host GPU tests, with DGC and descriptor-buffer extensions disabled:

| Test | Completed result | Boundary |
|---|---|---|
| Private driver-root contract | 174 assertions, no failures | 128 parameters/DWORDs; indices 63/64/127; descriptor table at 127; root CBV at 124; all 64 root descriptors; clear; indirect replay; bundle inheritance; invalid bounds/Close/Reset |
| Public root-signature regressions | 2,355 assertions, zero failures, four successful TODOs | Public roots and flags; does not establish native runtime instrumentation |
| ExecuteIndirect regressions | 1,921 assertions, zero failures, two skips | Implemented root paths; IA VBV/IBV still unsupported without DGC |

The native probe uses the Microsoft system runtime and Helios adapter in an
interactive scheduled task. Its verified CB48 baseline and 6125 candidate each
pass 12 cases/48 readback words: full API-cost constants, root CBV, volatile and
static/bounds-preserving tables, partial updates, ClearState/rebind and two
executions of every closed list. Candidate PID7756 loaded exact 6125 bytes and
the existing ICD, with LUID `00000000:04fbb655`. Its final process counters show
five created roots, 17 forwarded ClearRootArguments calls, zero root creation/
clear failures and 24 admitted/exact ECL boundaries. Existing native indirect
12-case/48-word, SO 34-case and four ordering controls also pass on 6125. The
ordering suite verifies 65,536 words per case; ETW identifies both processes'
loaded native UMD/ICD. The same root, indirect, SO and ordering suites pass on 22D1, with LUID
`00000000:05296ca9`; `native-validation-22d1.json` indexes exact process and
result identities. The updated native tiled case returns BLOCKED77 at tier 0
with exact 22D1/ICD identities. It supplies no native copy-command acceptance.

The final native capability inventory on 22D1 again creates FL11_0 and rejects
FL11_1/12_0/12_1/12_2 with DXGI_ERROR_UNSUPPORTED. Tiled 0 and RT0 remain reported.
All stock regression controls complete: Time Spy 19,073 overall (GT1/GT2
136.182602/116.635201 FPS), Fire Strike 35,391 overall (GT1/GT2/combined
246.294510/249.597260/41.122017 FPS), and Steel Nomad Vulkan 9,079
(90.797775 FPS). `benchmark-controls-22d1.json` links exact before/after
results, settings, modules and changing host-VNC frames. Time Spy's accepted
frame pair is from its demo, not a scored graphics test. Fire Strike combined
FPS is 6.67% below the previous control; its cause is unestablished. No causal
performance gain or owner visual acceptance follows from these controls.

Mechanical validation (`a1-copy-repair.log`) passes against the actual root
baseline: host UMD clippy, unsafe/slot/ASCII checks, 213 KMD logic tests and the
unchanged four-site protocol clippy baseline. The retired METHOD/PARALLEL review
loop is not a prerequisite.

## Remaining acceptance

The 128-DWORD path is exercised by a private host factory test. No accepted
native run has demonstrated that the runtime actually appended roots past 64
DWORDs. Requesting GPU validation alone is not that evidence. Native static
sampler 1.2 behavior and allocation-failure injection remain unexercised. Two
earlier native root-probe collections rejected duplicate module snapshots;
their raw readbacks passed, but only the corrected rerun is accepted.

These results close the implemented root translation/clear/capacity gaps, not
the full FL12_0/FL12_1 contract or visual acceptance. IA-changing indirect
commands, MSAA tiled copies, complete formats/limits and the broader lifetime
and sharing acceptance limits remain in FEATURE_LEVELS.md.
