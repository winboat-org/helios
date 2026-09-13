# ROADMAP — Stage: Correctness and D3D12

*The desktop first rendered end-to-end on 2026-07-05. The active architecture changed on
2026-07-09: Helios is a WDDM render+display adapter and owns the virtio-gpu scanout;
IddCx/Looking Glass is no longer the active display path.*

⭐ **This document was rebuilt on 2026-09-05.** It had grown to 4,472 lines, the bulk of
it dated per-defect narrative that no longer drove any decision. The whole of it is
preserved verbatim at `docs/archive/ROADMAP_HISTORY_THROUGH_2026-09-05.md` — nothing was
summarised away, and every WS number and defect id (`0ab-B`, `PresentWmk`, …) still
resolves there. What is kept below is what a reader needs *now*: the stage, the live
baseline, the priorities, per-workstream status with its open items, and the tooling
inventory. Sections retained are carried **verbatim**; only the connective text is new.

## Native FL12_1 admission on AMD/RADV, 2026-09-13

Native FL12 was refused on every AMD host because admission required the host
Vulkan mask `framebufferNoAttachmentsSampleCounts` to contain 16x. That limit is
host MSAA support; the D3D12 `SupportedSampleCountsWithNoOutputs` field is a
driver-declared sample-frequency contract that DDI0102 requires at 1/4/8/16 above
FL11_0. RADV caps MSAA at 8x, so the check admitted native FL12 on NVIDIA and
never on AMD. The engine now declares the host mask unioned with the DDI floor
and backs the excess by clamping only Vulkan's `rasterizationSamples`, keeping
the requested count in the shader; clamps are counted and reported at device
destruction. Admission no longer reads the mask as host evidence. Design record:
`docs/dx12/NO_OUTPUT_SAMPLES.md`.

Package 22.22.279.0 (`59595da2`) is installed on the WinBoat guest: five driver
images verified, `oem26.inf`, Code 0, DWM on the hardware stack. The native D3D12
suite moved `adapter` and `allocator` from FAIL to PASS and unblocked
`raytracing`, which now reports `CAP,NativeFL12_1Admission,00000000`.

Package 22.22.280.0 (`ef9c6586`) then fixed the first of the three defects
admission had exposed — the `raytracing` size disagreement — and is installed on
the guest as `oem27.inf`, Code 0, DWM on the hardware stack. Suites on `.280`:
`raytracing` PASS, every other case unchanged from `.279`.

Package 22.22.281.0 (`7bd1231`) fixed the second — the `tiled` `tiling-buffer`
crash — and is installed on the guest as `oem28.inf`, Code 0, DWM on the hardware
stack, with no reboot needed. `tiling-buffer` now completes
(`PASS,tiled,tiling-buffer,case-completed`) in both independent `tiled` passes, and
the probe runs on past it (`tiling-2d`, `mappings`, `copy-mappings`,
`copy-tiles-2d` PASS; `tiling-3d` and the two debug-layer cases BLOCKED). The rest
of the suite is unchanged: `adapter`, `indirect`, `indirect-ia`,
`root-signature`, `raytracing`, `sync` and `draw` PASS throughout;
`copy-tiles-predicated` and `copy-tiles-msaa4x` failed in one pass each with
content mismatches (below), and the diagnostic build used to localise the crash
reproduces the crash case exactly.

One defect that admission had been hiding remains open:
- `stream-output`: **the D3D12 `ExecuteCommandLists` path retires its WDDM DMA
  packet on Venus *worker* completion, not on host GPU completion.** A raw stride
  dump shows a correct 32-byte stride with gap and padding intact on a passing
  run, the failing check differs every run (`32-bit counter guards overwritten`,
  `stride padding overwritten`, `counter32 guard changed`), and the probe does
  wait on a queue fence before `Map` — so the fence is advancing early. Measured:
  3 of 11 runs fail without help; with `HELIOS_SO_DIAG_DELAY_MS=500` between the
  fence wait and the readback, 11 of 11 pass (delay hook is a local diagnostic
  build only, not committed). The same hazard now has a second, content-oracle
  reproducer: over the `.281` session the `allocator` probe failed 4 of 10 runs
  with `FAIL,GPU epoch pattern` — its 256-epoch reuse loop maps a readback after a
  fence wait and found a previous epoch's pattern — while `stream-output` failed 5
  of 10 with its usual varying text (`counter32 guard changed`, `NULL SO
  padding/tail overwritten`, `32-bit counter guards overwritten`). The rate is
  batch-dependent, not case-dependent: one 12-run batch of
  `allocator`/`stream-output`/`raytracing` had zero failures and the next had six,
  which is what an early fence looks like from the outside. `allocator`'s oracle
  (exact content, known epoch, one second per run) is the sharper of the two and
  is worth keeping for the K-F verification.
  Cause, located: `kmd_render/src/ddi/submit_command.rs:688` derives
  `gpu_completion_fence` **only from the Present BLT marker**
  (`present_packet.rs`'s `gpu_fence_id`). For D3D12 the KMD receives only
  `execution_boundary` — `HeliosD3D12SubmitCmd`'s worker-completion `value`
  (`protocol/src/wddm.rs:579`, 24 bytes, `{magic, version, ctx_id, value,
  cookie}`; no fence id exists in the record). So Present is gated on real GPU
  completion and D3D12 is not. This is the documented `D12-G8` rung 0 /
  `KMD_IMPACT.md` §14a.1 **UV1** gap, and it is why the fence returns in
  ~1 µs while the pixels land seconds later.
  Proper fix, cross-stack (the designed K-F workstream): mint a per-queue
  GPU-completion wire fence with the ICD export `helios_venus_queue_gpu_fence`
  (ICD-1, **already landed** in `vn_renderer_helios.c:2024`) after the engine's
  submission drain, carry it on a **versioned** `HeliosD3D12SubmitCmd`, and pass
  it to `note_wddm_submission` as `gpu_completion_fence` so the existing
  `wddm_boundary::select` D3D12 arm gates the DMA packet on host completion.
  Today nothing calls that export and the record has no field for it.
  **UV1 reading taken 2026-09-13 on `.281` (`oem28.inf`): UV1 ✓.** The instrument
  is `WddmHoldMs`, which delays retiring an otherwise-READY D3D12 ECL packet at the
  WDDM FIFO head — the one dependency this driver can create unilaterally — and the
  oracle is the `allocator` probe (256 epochs, a fence wait each, ~1.5 s). Both
  knobs are snapshotted at `VirtioGpu::init`, so each arm took a reboot, and
  `DiagLevel=1` was required for any counter to be written:
  ```
  hold 0   : 1260 / 1403 / 1461 / 1632 / 1756 / 2364 ms   WfBHold Δ 0      D12Rec Δ 513–897
  hold 100 : 11403 / 22992 ms                             WfBHold Δ 2227 / 3892   D12Rec Δ 11 / 68
  ```
  The hold demonstrably armed (`WfBHold` moved — without that, a flat reading is a
  trusting-a-zero, which is what the knob's own doc warns about), and the
  application's fence wait stretched by ~45–90 ms per epoch. **dxgkrnl does release
  the runtime's monitored fence behind our DMA packet**: the submission path is
  sound and the defect is exactly the packet's completion domain (Venus worker vs
  host GPU). That is the precondition K-F3..K-F9 was waiting on. ⚠ The held runs
  still FAIL the probe's content check, so this reading *licenses* the fix; it does
  not fix it. Instrument `tools/uv1-fence-latency.ps1`, evidence `tmp/uv1-20260913/`.
- ~~`raytracing` FAIL "uncompacted current/prebuild size agreement"~~ — **FIXED in
  22.22.280.0 (`ef9c6586`); design record `docs/dx12/ACCELERATION_STRUCTURE_CURRENT_SIZE.md`.**
  It was never a D3D12 admission problem: `CURRENT_SIZE` was answered with
  `VK_QUERY_TYPE_ACCELERATION_STRUCTURE_SIZE_KHR`, and RADV's `bvh/header.comp`
  reports the packed size **plus the 128-byte serialization-header alignment
  padding**, a quantity that for a small structure exceeds the prebuild
  requirement and for a compacted or cloned one exceeds the allocation itself.
  Measured on bare RADV in the WinBoat container (Mesa 25.0.7, RX 6600, no Venus
  or guest in the loop) with `tools/host_as_size_probe.c`, which reproduces the
  guest's 72/72/56-byte deltas exactly. The engine now records the D3D12 answer
  per structure — through the same helper the prebuild uses, so the two cannot
  disagree — and replays it: built structures get the recorded constant,
  compacted and cloned ones follow the `COMPACTED_SIZE` query, and unrecorded
  (deserialized) ones keep the old host fallback. Cost: one extra
  `vkGetAccelerationStructureBuildSizesKHR` (a synchronous Venus round trip) per
  D3D12 AS build; a content-keyed prebuild cache is the follow-up if that ever
  shows up in a profile.
  Original characterisation, kept for provenance: the check is a legitimate D3D12
  invariant — an uncompacted structure's `CURRENT_SIZE` **is** the
  `ResultDataMaxSizeInBytes` prebuild reported, not merely at most it — and the
  diagnostic probe build that dumped the raw postbuild-info buffer reported
  ```
  compacted [320, 320, 0]   current [392, 392, 568]   prebuild max [320, 320, 512]
  ```
  Both numbers come from Vulkan: the prebuild is
  `vkGetAccelerationStructureBuildSizesKHR`'s `accelerationStructureSize`
  (`device.c:8993`) and current was `VK_QUERY_TYPE_ACCELERATION_STRUCTURE_SIZE_KHR`
  (`acceleration_structure.c:459`). The guest ICD translated that query correctly
  and forwarded it (`vn_query_pool.c:103`), which is what the host-side probe then
  confirmed: the disagreement is in the host's answer, not in the translation.
- ~~`tiled` `tiling-buffer` FAIL with 0xC0000005~~ — **FIXED in 22.22.281.0
  (`7bd1231`), Mesa fork `913491b0394`; design record
  `docs/dx12/SPARSE_SUBMIT_TIMELINE.md`.** It was never a D3D12 admission problem
  and never a KMD problem. `tiling-buffer` maps tiles, so vkd3d reaches the host
  through `vkQueueBindSparse` (`d3d12_command_queue_flush_bind_sparse`, one wait
  and one signal, both the queue's own timeline), and the guest ICD prepared that
  sparse submission through the semaphore-feedback path. The two timeline
  **counter** accessors had cases for `VkSubmitInfo`/`VkSubmitInfo2` only, so the
  sparse batch fell into `default: UNREACHABLE("unexpected batch type")` — which
  in a release build is `__builtin_unreachable()`, not an error return, so the
  sparse batch got whatever the optimizer laid out. That is the WER-minidump read
  at `0x18` in `vulkan_virtio.dll` RVA `0x379205` =
  `vn_queue_submission_prepare + 0x325`. The fix is the two missing
  `VK_STRUCTURE_TYPE_BIND_SPARSE_INFO` cases (17 lines), the same shape current
  upstream Mesa carries; the neighbouring count and handle accessors already had
  it, and only the counter accessors — added later by the feedback work — did not.
  The probe that faulted now reports `PASS,tiled,tiling-buffer,case-completed`
  (`TILING,total,4`) both from the diagnostic build and from the shipped `.281`
  package, and `tiling-2d`, `mappings`, `copy-mappings` and `copy-tiles-2d` pass
  after it.
- `tiled` `copy-tiles-predicated` / `copy-tiles-msaa4x` FAIL with content
  mismatches (`CopyTiles input differs from independent CopyTextureRegion
  witness`; `MSAA CopyTiles sample roundtrip mismatch`) are **newly reachable, not
  new**: the probe stops at the first failing case, so neither had ever run.
  They are also intermittent — pass 1 failed `copy-tiles-predicated` (the
  diagnostic build did too), pass 2 passed it and failed `copy-tiles-msaa4x`
  instead — and both are content read back after a fence wait, i.e. the shape of
  the open defect below. This work does not attribute them: recorded so the next
  pass does not read them as a regression, and `SPARSE_COMPATIBILITY.md` remains
  the governing record for what tiled behaviour is claimed.

`no-output-msaa` and `tiled-format-caps` stay BLOCKED on this guest by the absent
D3D12 debug layer (Graphics Tools FoD, `0x887a002d`); DISM is denied in this
environment. A diagnostic build with the debug-layer gate relaxed measured the
approximation directly: 15 cases / 8,640 words, 320 mismatches, all on the
16-sample case (`got 000000ff, want 0000ffff`). RADV cannot rasterize 16 samples,
so a 16x no-output PSO is accepted but produces 8-sample coverage. That is the
documented boundary, not an implementation error.

Evidence: `tmp/integration-20260912/` (`verify-loaded279.json`, `results279.txt`,
`nooutput-task.out`, `driver279-build.log`, `assemble279.log`) for `.279`, and
`tmp/rtas-size-20260913/` for the `.280` CURRENT_SIZE fix and the `.281` sparse
fix (`helios-windows-x64-22.22.280.0-ef9c6586.zip` sha256
`ef77e4cf0ab16e43fcefa47c5f891e8527d8a3439c6b37b066270e5282eb7f5e`,
`helios-windows-x64-22.22.281.0-7bd12310.zip` sha256
`922b9f0f3c64719066b44731ddcf5f8240647652790d2c7b09fe69a6e66c3a72`,
`evidence/verify-loaded280.out`, `evidence/verify-loaded281.out`,
`evidence/verdicts281.txt`, `evidence/results281-summary.txt`,
`evidence/raytracing/`, `evidence/verdict-*.txt`, `evidence/recover280.log`,
`evidence/build280c.log`, `evidence/assemble280b.log`,
`evidence/build281c.log`, `evidence/assemble281.log`,
`evidence/engine-inputs281.json`, `evidence/validation281.json` and
`evidence/manifest281.json`; host-side attribution output and the probe source
are `tools/host_as_size_probe.c`).

The `.280` install is the same package flow as `.279`, with one wrinkle worth
recording: the ring-3 upgrade driven from a scheduled task was killed by
`STATUS_CONTROL_C_EXIT` at the `pnputil /add-driver` step (a console control
event during the display-driver swap), which left `install-state.json` written
but incomplete. Recovery is the documented path — `Uninstall-Helios.ps1
-KeepDriver`, then `Install-Helios.ps1` — run as `SYSTEM` in session 0 so no
console event can interrupt it (`tmp/rtas-size-20260913/recover280.ps1`).

`.281` installed with that same `Uninstall -KeepDriver` → `Install` sequence run
as `SYSTEM` in session 0 (`tmp/rtas-size-20260913/upgrade281.ps1`) and completed
first try with `3010`; the device restarted into the new driver inside the same
install, no guest reboot, `oem28.inf`, five driver images verified against the
manifest and DWM reloaded on the new `helios_umd.dll` and the new ICD
(`evidence/verify-loaded281.out`).

## Combined DX12/WoW64 integration, 2026-09-12

The owner requested merging the published native FL12/DXR work with the tested
WoW64, presentation and PassMark timer fixes. The combined source retains the
new queue-fence retirement contract and the read-only snapshot status escape;
retired STREAM_FEEDBACK opcode 0x14 stays reserved. New tiled-resource and DXR
callbacks use the same Windows system ABI as the other x86 DDI slots. The
pipeline-stream layout assertions now include the new stream-output record.

The recorded Mesa, vkd3d/DXIL, Venus protocol and renderer revisions are checked
out recursively. DXVK retains the published PassMark resolve and failure-handling
commits, with its canonical WinBoat repository URL. Both Rust architecture
checks and 215 KMD logic tests pass. The paired renderer/protocol build and their
queue-sync/decoder tests pass inside the WinBoat container. Version .277 is
reserved for this combined package; Windows build and installed acceptance are
pending. The guest still runs the accepted .276 package at this checkpoint.
Evidence: `tmp/integration-20260912/`.

## WoW64 Direct3D 11/12 support and PassMark, 2026-09-12

PassMark's `PerformanceTest64.exe` starts a **32-bit** `PT-D3D11Test.exe`.
On the .271 guest, `UserModeDriverNameWoW` was absent: an interactive x86
`D3D11CreateDevice(HARDWARE, FL_11_0)` probe returned **0x887A0004**, while the
identical x64 probe succeeded. WARP worked in both, and the display was landscape.
This explains the generic initialization dialog before swapchain creation.

The .272 source builds separate `helios_umd32.dll` / `helios_umd12_32.dll`,
registered in WoW64 slots 0–2 / 3 alongside their native counterparts. Both
DXVK and vkd3d engines build with the MSVC x86 ABI. All DDI callbacks, including
fallbacks and deferred-context adapters, preserve their exact WDK signatures;
bindgen selects Cargo's target, including separate x86/x64 D3D12 caches. Vulkan
memory handles retain 64 bits, process pointers and PSO streams follow target
width/alignment, and both architectures read the same Helios registry knobs.
No KMD wire layout changed. OpenCL remains x64-only.

Packaging includes four UMDs, both VC runtimes, matching D3D device/readback
probes, PE/export/CRT checks, catalog signing over final image bytes, and
registration/hash verification. Rollback preserves or removes WoW64 registration
by package ownership. Independent ABI, lifetime, build and rollback reviews
closed the findings before guest deployment.

Build-box validation on `firstheberg2-win`: all four release UMDs linked; x86
D3D11 exports the expected undecorated entry points with no dynamic CRT imports;
D3D12 table tests passed **54 x86 / 53 x64 checks**, including x86 callee stack
cleanup. Shared UMD tests passed **5** on Windows x86; protocol tests passed
**11** on Windows x86 and **14** on Linux (three Linux UAPI comparisons remain
Linux-only). Both D3D12 target checks and strict Clippy passed. Generated layout
assertions stay enabled. Raw evidence: `tmp/wow64-20260912/`; initial failure:
`tmp/passmark-20260912/`.

**GPU acceptance on .272:** after the approved reboot and console login, both
architectures passed D3D11 rendering, D3D12 device creation, a 65,536-pixel exact
clear/readback, four cross-queue/CPU/cross-process ordering cases, and a real
shader draw with clean COM teardown. Strict D3D12 presentation runs checked every
RGBA pixel over 1,201–1,202 frames per architecture, including all three back
buffers and final zero device references. Host VNC captured advancing, correct
frames from both runs; a separate x86 D3D11 triangle was visibly rendered.
Installed-package smoke verification also passed native/x86 Vulkan and OpenGL,
native OpenCL and GL sharing. OpenCL x86 is still outside this implementation.

**PassMark exposed a second bug after device creation:** its DDI pipeline
statistics query (kind 8, 88 bytes) was cast directly to the API enum (kind 8,
16-byte stream-output statistics). `GetData` failed, the runtime removed the
device, and PassMark ignored a later failed `CreateBuffer` before mapping NULL.
The .273 source explicitly translates query kinds and legacy 64-byte statistics,
keeps pending/error output untouched, and honors `DONOTFLUSH`. A native query
regression probe reproduces removal on .272 in both architectures. PassMark
now completes on the corrected loaded DLLs. The query probe initially failed a
separate `S_FALSE` payload assertion, resolved in the senior review below.
Seven pure query tests, both Windows UMD checks, and independent reviews pass.

A separate DXVK worker-failure path could strand DWM's device destruction in a
condition-variable wait after the command-stream worker exited. Failure now
wakes waiters and rejects new work; the .275 review below closes a remaining
Present error-propagation gap. The 96 actual
worker-failure cases per architecture and present/query guard fixtures pass.
The original worker exception trigger remains unconfirmed. Guest GDI screen
capture also coincided with strict D3D12 fence stalls, whereas runs without that
capture and runs captured externally via VNC passed; recheck this interaction
after activating .273 before assigning its cause.

All four .273 UMDs and the KMD built, linked, passed PE/export/CRT checks, and
were signed before catalog generation. Catalog membership and all 44 package
manifest entries passed. The installed bundle is
`helios-windows-x64-22.22.273.0-5de1710e.zip`, SHA256
`fd1df48752b2457bb9fe0af238d3bb2cb609d6d0c59dd3b25901594c7a133474`.
The runtime build remains `f7d477e0`; `5de1710e` adds the reviewed registry
scripts. `runtime-source-equivalence.json` records the checked source difference,
and `engine-inputs.json` preserves the original linked archive provenance.

A registration-only ProgramData trial did **not** load the new DLLs: fresh
processes still logged cached .272 DriverStore paths. During rollback, a
preexisting `New-Item -Force` helper replaced the entire class key, deleting
neighboring metadata. The same-INF PnP repair restored INF entries; missing
standard metadata was then recovered from current PnP properties and the verified
.272 INF, and WGL paths from hash-verified installed files. The temporary override
and its certificate trust were removed. The repaired helper preserves keys and
creates missing ancestors without Force; all seven unsafe sites in package/ICD/
knob tools were repaired. Real registry tests and independent checks passed on
PS7 x64 and PS5 x64/x86, including typed neighbors, child keys and ACLs; the old
helper fails the negative control. CI runs the regression in both shells.

Native UMD hotplug also overwrote DX12 slot 3 during DX11-only updates and
silently wrote DriverStore in ProgramData mode. It now preserves DX12/WoW64,
verifies the resulting inventory, and keeps ProgramData updates outside
DriverStore, including junction/path aliases. Its 31-case harness passed Linux
and Windows PowerShell; independent review closed both path-alias findings.
Registration/file checks must be followed by actual loaded-module verification.

**.274 acceptance baseline:** signed .274 (`7d6412df`) was active with Code 0 after
reboot under the owner's standing authorization. This WinBoat launch stops the
container on guest reboot; `docker start WinBoat` resumes it. The installed
bundle is `helios-windows-x64-22.22.274.0-7d6412df.zip`, SHA256
`5f1a99384d92d27aafef1fbd73f2ec1ccde6873717c27886bc07074ecf8ca5c5`.
PassMark and the x86/x64 probes logged the .274 DriverStore modules; their
versions and hashes match the installed package. Previous .273 rollback state
and scripts remain under `C:\ProgramData\Helios\wow64-evidence\before274`.

**Closed: MSAA/sRGB presentation corruption.** PassMark's x86 helper presents
1280x800, DXGI format 29 (sRGB), 4x MSAA through `Present1-single` with BLT set
and no destination handle. The old snapshot format whitelist refused sRGB,
so KMD imported the original four-sample backing under its fixed single-sample
image contract. The allocation metadata has no sample count; that raw fallback
cannot represent this source correctly. The owner confirmed the visible glitch.

The native-runtime `tools/d3d11_msaa_present_probe.cpp` isolates the defect.
On .273, all eight x86/x64 × 1x/4x × UNORM/sRGB source-readback cases exited 0
with exact RGBA agreement over **3,794 frames / 3,885,056,000 pixels**, and zero
final device references. Host VNC nevertheless showed scrambled geometry in
both 4x sRGB cases, black instead of the 128 average in both 4x UNORM sample
bands, and 128 instead of 188 midtones in both 1x sRGB cases. Only the two 1x
UNORM controls passed the visible-frame comparison. This is a common
presentation defect exposed by WoW64, not an architecture-specific draw error.
The confirmed baseline supersedes an earlier launcher that lost process exit
codes. Evidence: `tmp/wow64-20260912/msaa-baseline273-confirm/` and its `-vnc/`
folder, plus `passmark-msaa-presentation-review.txt`.

The .274 correction resolves MSAA in its source format, then preserves sRGB
encoded bytes in the corresponding UNORM snapshot. Required normalization
stays enabled with optional snapshot isolation off. Missing capability,
producer ordering, geometry, cache capacity or copy fails Present explicitly.
The KMD admits only the exact canonical format pairs. All 12 snapshot validation
tests pass, including wrong-format/extent/purpose rejection. All five driver
images build, pass PE/export/CRT and INF checks, and have verified catalog
membership; all 44 package manifest/signature checks pass.

**.274 rendering acceptance:** all eight cases pass both source readback and
host VNC comparison, with advancing frame serials, exact geometry and midtones:
**3,724 frames / 3,813,376,000 source pixels**. With `ScanoutSnapshot=0`, the same
eight cases pass again: **3,770 frames / 3,860,480,000 source pixels**. Image
comparison excludes the separately checked serial strip and the visually
identified mouse cursor; both baseline and fix allow three seconds for process
startup. Disabling `UmdAsyncPresentStream` makes 4x sRGB Present fail immediately
in both architectures: named required-normalization refusal count 1, probe exit
2, API result `DXGI_ERROR_DEVICE_REMOVED` (the DDI returns `E_FAIL`). Original
registry values were restored, and the adapter remains Code 0. A fresh x86
4x sRGB run then exited 0 after 292 frames with correct host VNC output and
zero final device references (`msaa-recovery274-confirm/`).

Real PassMark DX11 completed using `helios_umd32.dll` .274. Multiple host VNC
frames show coherent, changing jellyfish/terrain geometry without the old
scrambling. The report records 10.7 fps with its resolution penalty; this is
correctness evidence, not a performance comparison. All **14 installed-package
smoke cases** pass, including native/x86 D3D11 and D3D12 creation plus exact
65,536-pixel D3D12 clear/readback in each architecture. Evidence under
`tmp/wow64-20260912/`: `msaa-fixed274{,-vnc}/`,
`msaa-snapshot-off274{,-vnc}/`, `msaa-no-stream274/`, `passmark-274{,-vnc}/`,
and `verify-274/`. The senior review below resolves the separate query probe's
`S_FALSE` assertion; full conformance is not claimed.

**Senior review repairs (.275, accepted):** full-change reviews
covered ABI/tables, lifetimes/concurrency, error propagation, deployment, and claim
integrity. The review found and repaired these concrete defects:

- The C++ frame gate collapsed submission/device failure into a false value that
  Rust treated as an ignorable timeout. HRESULTs now distinguish completion,
  bounded vehicle timeout, and failure; both Present callers return before
  publication on failure. Actual-source fixtures cover 13 C++ and 10 Rust outcomes.
- Mandatory normalization inherited a permanent eight-geometry cache limit.
  WindowedBlt rings can now be reclaimed only after the new capability-negotiated,
  read-only `SNAPSHOT_STATUS` escape confirms all slots idle, including context
  stashes, queued GPU work, and pending CPU mirrors. Direct-scanout rings remain
  retained. Count/byte limits still apply; older KMDs do not permit reclamation.
  Prepared KMD blits resolve stable command identity at submission, so reclaiming
  an unrelated cache entry cannot invalidate a deferred vector index.
- Native hotplug now checks both input PE machine types before any mutation;
  its 33-case harness and the seven win-MCP tests pass. The KMD resource helper
  explicitly starts PowerShell with execution-policy bypass. Stale tool/deployment
  instructions and snapshot fallback comments were corrected.
- The query probe incorrectly required the public API's `S_FALSE` payload to stay
  untouched. That requirement belongs to the DDI, where private staging and seven
  contract tests enforce it. The API probe now validates payload on `S_OK` and
  always checks buffer canaries. On .274 the corrected probe passes all sixteen
  D3D11 query variants and legacy D3D10 pipeline statistics in x86 and x64.

The resize probe adds twelve geometries in one process. On .274 both architectures
fail on the ninth geometry with the cache-limit and required-normalization refusal;
both had exact source readback through the failure. The probe's resized windows
now stay within the desktop work area so the taskbar cannot obscure the oracle.
Source/readback plus
host VNC, positive `ring_reclaims`, and no `SnQrF`/required-normalization refusals
are the acceptance checks; .275 passes all of them. The new status refusal counter is in the KMD gate;
frame-gate and required-normalization failures are in the UMD gate.

**.275 review acceptance:** signed .275 (`46f79f01`) was installed, rebooted, and Code 0.
All five DriverStore images match the package's versions and SHA256 hashes;
probe logs name those native/x86 UMD modules. Both architectures completed all
**twelve resized geometries**, with four explicit ring reclamations apiece,
exact source pixels, and zero final COM references. All eight 1x/4x UNORM/sRGB
presentation cases and both query suites also pass. Independent host VNC grading
checks **122 exact frames**, including every resized geometry and advancing frame
serials; fixed phase-interior sampling excludes startup and resize transitions.
The same DWM process survived the suite; `SnQrF` stayed absent before/after, and
no frame-gate or required-normalization refusal appeared in the probe processes.
All **14 installed-package smoke cases** pass, including native/x86 DX12 exact
clear/readback. Submission-failure propagation was checked with actual-source
fixtures, not a new guest fault injection.

Bundle: `helios-windows-x64-22.22.275.0-46f79f01.zip`, SHA256
`020e432234cc0068d747aaa5165f69a6e2c6d11388e54152022106e729b42ecb`.
Its 44 manifest entries and catalog membership of all five driver images pass;
594 DXVK source files and 18 linked archives were checked for provenance.
Later `a5df271` only refines the external resize probe and comments/diagnosis;
it changes no driver behavior. Raw evidence: `tmp/review-20260912/`, especially
`resize-workarea274{,-vnc}/`, `acceptance275{,-vnc}/`, `verify-275/`, the before/after
guest inventory, and build/catalog logs. Rollback state remains in
`C:\ProgramData\Helios\wow64-evidence\before275`.

**PassMark DX11 performance, 2026-09-12:** the local WinBoat guest now runs
signed **22.22.276.0 / oem23.inf**, default enabled with no `UmdTimerRes`
override. Profiling found Mesa's 1 ms polling delays taking roughly 10–11 ms
on DXVK workers. Each DX11 device now owns a balanced 1 ms Windows timer
request, released after its workers; API failures are counted and gated.
Both native/x86 lifetime probes confirm the request takes effect and releases.
The timer request can increase wakeups/power use while a DX11 device exists.

Four clean alternating disabled/default runs score **11.4 / 20.6 / 10.9 /
21.1**, averaging **11.15 → 20.85 (+87.0%)**. They use the same .276 binaries,
settings and 1280×800 desktop, without tracing/capture. PassMark penalizes this
resolution; the reported score is distinct from the roughly 31 FPS live scene.
This RX 6600 / Ryzen 5 5600 machine is not the earlier 3DMark baseline.

The signed bundle is `helios-windows-x64-22.22.276.0-bad9ff18.zip`, SHA256
`90461b12568a32cc712b939fab369adb41872afcb2828eeaa6adbce38831ad32`.
All five DriverStore versions/hashes and 44 manifest entries pass. After reboot,
all twelve resize/query/MSAA cases pass, with exact pattern checks across **122 host VNC frames**,
all twelve geometries per architecture and advancing serials. All **14 packaged
smokes** pass, including native/x86 DX12 exact clear/readback. DWM survives,
Code 0 remains, `SnQrF` stays absent and `QSpErr` stays zero. No timer,
frame-gate or normalization failures appear in the tested DX11 processes.
The source passed two dry whole-change review rounds; native/x86 builds and
six timer-lifetime failure/unwind cases pass. Evidence and profiling detail:
[PERFORMANCE_FEEDBACK.md](docs/PERFORMANCE_FEEDBACK.md#passmark-dx11-timer-resolution-2026-09-12),
`tmp/passmark-perf-20260912/`. Rollback is saved under
`C:\ProgramData\Helios\wow64-evidence\before276`.

**Pre-integration workload diagnosis (.274–.276):** the owner reports PassMark's DX12 initialization dialog
on .274 despite successful native/x86 D3D12 probes. The DX12 log identifies the
native `PT-D3D12Test64.exe`: device
and root-signature creation succeed, then a two-argument state-changing
`CreateCommandSignature` is refused with `E_NOTIMPL`. That deployed Venus
protocol/ICD lacks EXT device-generated commands, and this process's vkd3d
log confirms effective DGC is disabled. Without it, the engine can accept that
signature and then skip its action or ignore its state; removing the UMD guard
would produce incorrect rendering. There is no active NV or stateful-compute
fallback. The proper fix is SUBSTRATE S10 transport support plus full DDI
argument-union translation. The exact PassMark argument types are not logged;
only the state-plus-action shape is established. Evidence:
`tmp/review-20260912/umd12-4788{,-vkd3d}.log`. The timer fix above leaves this
DX12 contract gap unchanged in .276. The combined source above now includes
native DGC transport and DDI argument translation; PassMark acceptance on this
host remains to be checked.
An independent preexisting conformance gap also remains: DXVK predication is
stubbed, and SO-overflow predicate `QueryInterface(ID3D11Predicate)` fails. Query
result translation alone does not claim predicate-controlled rendering support.

## GitHub publication, 2026-09-12

The owner authorized publishing all accumulated changes. Root master incorporates
upstream through `0752648`, preserving its metadata, packaging, capture, signing,
UTF-8 and lockfile fixes alongside the native FL12/DXR work. Mesa merge
`d2ee351e185` preserves its matching branding changes and the tested native Venus
work. DXVK, QEMU, virglrenderer and venus-protocol revisions are unchanged.

The owner created the missing DXIL fork. Its `f4651bd0` contains required
stream-output and mixed-sample lowering; engine `9d4731f1` points the compiler
submodule at that fork. The compiler and engine `master` branches and Mesa `main`
are pushed and verified before publishing the root pointers:

| GitHub repository | Branch | Published revision |
|---|---|---|
| winboat-org/dxil-spirv | master | f4651bd076a2613728823ec289abc121a348a48a |
| winboat-org/vkd3d-proton | master | 9d4731f154ee3dc45f33eb900aba1f2233e2f14b |
| winboat-org/mesa-helios | main | d2ee351e185db9b28e7dfcc1d9d91dd1a168577e |

The root submodule URL now uses the engine's canonical WinBoat location, confirmed
by GitHub's repository-move response. Compiler, engine and root history are
preserved without force pushes. The existing renderer/protocol publications and
the unchanged DXVK/QEMU branch references are also verified.

Metadata synchronization, two resource-parser tests, 213 KMD logic tests, native
UMD host clippy, DDI slot coverage and log checks pass after integration. A1's
text scan reports seven panic-pattern matches, all in metadata tests or Cargo
build scripts; inspection confirms they are not driver-runtime paths. Other A1
checks pass; its unmodified aggregate exit status remains 1. Receipts are in
`tmp/publish-fl12-20260912/`. These source/publication changes were not deployed.
Metadata and both updated lockfiles pass the final consistency/parse checks,
including bindgen 0.72.1. The general-testing evidence below remains tied to UMD12
C7241DE6 and its exact compiled sources. The incoming Windows package-validation
record remains scoped to upstream `6e8de383`; no new Windows build or guest
validation is claimed for this combined publication merge.

## Metadata consistency, 2026-09-12

The product/adapter/monitor name is **Helios vGPU**, published and developed by
**WinBoat**. `metadata/helios.env` owns branding, component roles and monitor model
year; the release version stays in `kmd_render/driver-version.env`. KMD, both UMDs
and the ADL shim use the same Windows resource template. Cargo authors, INF,
package publisher verification, local/CI signer labels, and the Windows Mesa
branding now follow the shared source. Legacy package verification remains valid.

The EDID now reports model year 2026, mode-derived aspect ratio, standard sRGB
coordinates and WinBoat publisher text. It no longer invents a manufacture week,
physical panel size, physical connector, or a 200 MHz range limit that contradicts
4K60. The base-block encoder rejects oversized extents/clocks; StartDevice counts
these under `EdidModeRejectCount` and uses the matching 1920x1080 fallback rather
than publishing truncated timing fields. Supporting widths >=4096 or clocks above
655.35 MHz still requires an EDID extension/DisplayID implementation. HLS, product
code and container GUID remain stable virtual-display identities; zero serial and
empty standard-3D engine FriendlyName are correct, not unfinished metadata.

See `metadata/README.md` for the final field table and validation boundaries.
215 host logic tests, resource compilation for all four Helios binaries,
PowerShell parsing/publisher checks, vendor mapping tests and EDID conformity at
1080p, 4K UHD and portrait pass. Windows driver/engine, Mesa x64/x86, loader and
compatibility builds/tests also passed at source commit `6e8de383` on 2026-09-12.
The test-signed .271 bundle passed 35 manifest hash/size checks, WinBoat resources,
INF metadata and certificate identity checks; its self-signed root remains
untrusted on the builder. Unchanged CLVK was reused with source/patch/hash
provenance verified. Build fixes cover explicit UTF-8 metadata I/O, PowerShell
signing module isolation and stale lockfiles. Installed-device and visible
rendering validation remain pending. No deployment or version bump occurred.


## Ready for general FL12 testing, 2026-09-12

The owner prioritizes general native FL12_0/12_1 and optional DXR1.0 application
testing over exhaustive conformance. Tools-visualization output and extreme RT
limits are deferred unless an actual workload makes them blockers. Physical
non-RT hardware testing belongs to the owner's later work; retain conditional
RT support and the automated missing-feature refusal/readback checks. Keep the
authorized sparse fallback and its explicit semantics/memory-cost exceptions;
do not start broad sparse emulation work. General testing readiness does not
mean complete FL/DXR conformance or general sharing/host-loss/WSI acceptance.

Root master now merges origin/master `7133483`, including the AMD/RADV exported
texture layout fix and the complete Windows Vulkan SDK installation path.
Engine merge `f17f7936` combines upstream `fd348a8a` with the native DXR and
allocator changes. Mesa merge `259f8e4e` retains our Venus work and includes the
Gallium buffer-map failure repair `b7033eee`; that Gallium-only change does not
alter the deployed Venus ICD. No unrelated dependency was reset or moved.

Release UMD12 `C7241DE6` is ready for general native FL12_0/FL12_1 and optional
DXR1.0 application testing. It is deployed through ProgramData on unchanged
WDDM2.1/.271/oem54, UMD11 `57C84ED4` and ICD `43394BBD`. Linux/Windows engine
builds, Windows release UMD and A1 pass, including 211 KMD logic tests. The
engine allocator test and all seven extracted threaded-map sanitizer cases pass.
Native session1 caps, allocator, DXR, RT-disabled refusal/readback and all four
ordering cases pass on the identified C7241DE6 UMD and system runtime. The
allocator probe completes 832 resets; eight DXR behavior groups read back 20
ray-result words. The restricted-RT fixture refuses a valid RT state object and
reads back 4,096 correct words; it does not simulate another GPU's
entire behavior. All four queue-ordering cases read back 65,536 correct words each.
The broader native suite passes 23 inherited-feature groups / 529,085 checks and
13 raster groups / 778,722 checks, with no failures, skips or todos. Positive
raster cases grade the debug InfoQueue; the inherited suite's debug-layer setup
does not establish that every InfoQueue message was graded.

All four full stock benchmarks complete in interactive scheduled tasks, with
matching rendering settings against the preceding completed 057934F9 controls,
archived `.3dmark-result` and XML exports, per-workload loaded-module attribution,
and changing rendered scenes captured through host VNC. No paintcap/focus-taking
observer runs during them. Native D3D12 workloads load the system runtime and
C7241DE6; Fire Strike uses UMD11, and Steel Nomad is explicitly the Vulkan control.
No WARP or app-local D3D12/engine substitution is present in the recorded modules.

| Benchmark | Completed workloads | Graphics score | Graphics FPS | VNC frames |
|---|---:|---:|---|---:|
| Port Royal | 2/2 | 12,134 | 56.18 | 73 |
| Time Spy | 4/4 | 23,878 | 164.27 / 130.84 | 84 |
| Fire Strike | 5/5 | 59,315 | 259.37 / 256.43 | 70 |
| Steel Nomad Vulkan | 1/1 | 8,282 | 82.82 | 20 |

Every workload returns status 0. Port Royal and Time Spy have no recorded
pending-allocator-reset errors. These are single completed controls, not a
focused performance comparison or evidence of a gain. In particular, the Vulkan
score is below the preceding 9,409 result; this run does not establish a cause.
Agent inspection confirms changing benchmark scenes, while the owner's visual
acceptance remains scoped to the earlier F6D00A83 Port Royal build. The final
inventory reports Code0, explicit UmdD3D12=1 and no running benchmark/probe.
The guest is left idle with C7241DE6 installed for general application testing;
report concrete application failures before reopening the deferred work.

Evidence: `tmp/fl12-general-testing-20260912/`. `source-provenance.json` corrects
the captured manifest template's stale descriptive fields while retaining its
72 verified source digests and original manifest hash. The Windows build receipt
checks 67 mirrored production files, seven static archives and DLL imports/exports.
The compiled root is `e1882037bceb3d88cba1bef72d55038ee98133ec`, engine is
`f17f79366b91dbab77c85707d612d4147c58b57b`, and Mesa source is
`259f8e4e306c7c4a02686071f98a0636c4dbc16d`. UMD12 SHA256 is
`C7241DE61AABE4F8AAF31F02941AC2BC8109BEEE14C86C4FC86B082242755044`.
`readiness-summary.json`, `source-provenance.json`, `guest-final.json`,
`feature-review.json` and the four `fl12-general-*-review.json` / settings receipts
record exact artifacts, completed checks and retained limitations. Native
runtime D3D12/Core is 10.0.26100.9278 with DDI _0110; native FL11_0..12_1 creation
passes, FL12_2 is refused, maximum FL is 12_1, SM is 6.3 and RT tier is 1.0.
The Windows build uses LLVM/libclang 22.1.8 and Vulkan SDK 1.4.350.0.
This is a local integration and ProgramData update, not a new signed package,
hosted-CI acceptance, host renderer change or VM-launcher restart. Nothing was
pushed during that validation; the publication checkpoint above supersedes this.

## Native allocator generations, 2026-09-11

Preceding ProgramData UMD12 `F6D00A83…` gives a DDI command pool a separate
allocator generation when the engine still owns its previous execution storage.
Completed generations are recycled through the existing fence-worker reference
release; no GPU-idle wait or completion shortcut is added. Two allocator OOM
paths now preserve recorded command storage, check recycling-array allocation
and propagate sticky E_OUTOFMEMORY from Close.
Linux/Windows builds, A1, the focused native allocator readbacks, native DXR,
no-RT refusal/readback and all four ordering cases pass. The small allocator
probe passes 832 resets. Final Port Royal completes at **12,163 / 56.31 FPS**,
with matching settings, changing rendered frames and zero pending-reset errors.
The graphics test performs 42,353 rotations, including 42,180 reuses, without
reset failures. On 2026-09-12 the owner accepted the final build's demo 20.png
and graphics-test 62.png as visually correct. This is not a performance-gain claim.

[ALLOCATOR_LIFETIME.md](docs/dx12/ALLOCATOR_LIFETIME.md) records the contract,
source/build/deployment provenance, counters, tests and remaining acceptance.
WDDM2.1/.271/oem54, UMD11, Mesa and async WSI are unchanged. Full FL12_1/DXR
compliance remains open independently of this build's accepted Port Royal rendering.

## Conditional DXR admission, 2026-09-11

Preceding release UMD12 `465CBE13…` makes DXR optional. Adapter caps are discovered
through the actual engine and revalidated at native device creation; non-RT
engines retain otherwise-backed FL11_0..12_1 support, with RT0 and an engine-bounded
shader-model list. Native Windows admission/caps checks pass with RT enabled and
with RT extensions removed from engine discovery. WDDM2.1/.271/oem54, UMD11,
Mesa, native completion and async WSI are unchanged. Eight DXR behavior groups
and20 ray words pass. The no-RT fixture refuses a valid RT pipeline and reads
back4,096 correct words; all four native ordering cases pass in ordinary RT,
engine-disabled RT and Venus-disabled RT configurations. The first candidate's
cross-process startup failure is repaired by transferring the discovery engine
to the native device, with the original test deadlines preserved.

[Conditional DXR support](docs/dx12/DXR_SERIALIZATION.md#conditional-dxr-support)
records source/build/deployment identities, native GPU/refusal checks and the
limits of the restricted-feature test. It is not a run on a different GPU.
The accepted Port Royal result below remains tied to 057934F9; this increment
has no new benchmark or performance claim. Full FL12_1/DXR compliance is still
open, including sparse mapping, tools visualization, allocator retirement and
newly recorded extreme AS-count/recursion-limit audit items.

## Native DXR and completed Port Royal, 2026-09-11

Release UMD12 `057934F9…` completes native Port Royal with **12,337 / 57.12 FPS**.
Both the demo and graphics test return workload status 0; the stock definition,
resolved settings, `.3dmark-result`, XML export, loaded module identities and
changing host-VNC frames are archived. The graphics test renders at 2560×1440
with ray-traced reflections and RT shadows enabled. This is one completed run,
not evidence of a performance gain. **The owner visually accepted this Port Royal
run on 2026-09-11: "Looks correct"**, responding to the demo 13.png and graphics 35.png
captures. This acceptance is scoped to 057934F9 and the recorded settings; the
regression controls and full FL/DXR conformance retain their separate acceptance.

The native frontend reuses vkd3d's existing DXR engine. This increment admits
RT1.0/SM6.3 with matching engine backing checks, preserves 64-bit shader-table
strides, reads the correct four-byte RT1.0 pipeline config, and resolves runtime
summary associations against explicit public export names and aliases. The
last two defects were reached by Port Royal itself. The same aliased-export /
local-SRV native probe fails before the namespace fix and passes all seven
behavior groups and 20 ray-result words afterward, including collection and
local-root lifetimes. Final native FL11_0..12_1 creation, FL12_2 refusal and all
four ordering cases (65,536 words each) pass on the exact installed build.

Linux/Windows engine builds, Windows release UMD and A1 pass, including 211 KMD
logic tests. KMD .271/oem54/Code0/WDDM2.1, UMD11 and Mesa ICD remain unchanged;
this is a ProgramData deployment, not a new signed package or hosted-CI result.
[DXR_SERIALIZATION.md](docs/dx12/DXR_SERIALIZATION.md#completed-native-port-royal-2026-09-11)
records exact source/build/runtime provenance, results, the two diagnosed DDI
failures, and unexercised/refused behavior. Time Spy, Fire Strike and Steel Nomad
Vulkan also complete all workloads with matching rendering settings and changing
frames: graphics scores 23,816 / 59,231 / 9,409 respectively. These are single-run
controls, not a performance comparison. Root implementation `c65b77e` and engine
`54e759e1` / `bb46e7c6` are committed locally; nothing new was pushed. That validation left the guest idle. The [control receipt](docs/dx12/DXR_SERIALIZATION.md#completed-regression-controls-2026-09-11)
records exact hashes, runtime identities and the remaining acceptance limits.

Complete FL12_0/12_1 and DXR conformance remain open: the authorized sparse
compatibility gap, unsupported tools visualization, estimated lane count and
broader sharing/ownership/host-loss/WSI limits are unchanged. Port Royal emits
91,993 demo and 41,715 graphics-test pending-allocator-reset diagnostics; successful
completion does not settle the allocator/fence-worker lifetime question. The next
concrete correctness work is to distinguish completed GPU work with delayed
reference retirement from premature pool reuse, using a bounded native ordering
probe. Do not remove retention, skip synchronization or insert a GPU-idle wait.

## DXR integration sequencing correction, 2026-09-11

vkd3d already implements raytracing pipelines, DXIL compilation, AS operations,
shader tables and ray dispatch. Helios installs native forwarding callbacks for
those operations in `forward12/misc.rs`, implemented by `raytracing.rs`. Current
native admission is still hard-coded RT0 and the shader-model list stops at6.0.
The next work is checking that native DDI contract and capability agreement with
the actual engine, then exercising the native Windows probe. Engine changes
need a demonstrated forwarding/backing defect, rather than a replacement DXR
implementation.

The tools-visualization layout experiment was set aside uncommitted and never
deployed. Its library compiled, but its standalone test did not link; no passing
validation is claimed. Tools visualization remains an explicit unsupported
engine/DDI operation and conformance gap. No Port Royal trace establishes that
it uses this operation, so the older "next subsystem" statements below are not
evidence that an inverse-AS implementation is a prerequisite for Port Royal.
No capability was raised and no complete DXR conformance claim follows from this
sequencing correction. The tested installed8F15F9DC build remains unchanged.

## Committed dependency checkpoint, 2026-09-10

The owner authorized committing the accumulated native FL/DXR work and pushing
only the new renderer/protocol forks. Their GitHub `main` refs are verified at
virglrenderer `2121d5d0e82a58edc321ced3309c1ce7b7c41905` and venus-protocol
`fe08e82c3819e8ee3c547b1ea810fde61f46fa78`; `.gitmodules` uses those owner forks.
The upstream remotes remain available separately. Mesa `2d4e910bd04`, engine
`10efa8af` and nested DXIL compiler `f4651bd0` are committed locally and are not
pushed. The root checkpoint therefore still has unpublished dependencies.
DXVK, QEMU and unrelated dependencies remain at their existing revisions.

This records source publication, not a new deployment. Installed release UMD12
remains `8F15F9DC…` with the validation below. Commit preparation removed one
trailing blank line in Mesa and normalized whitespace in generated TIR test
headers without changing shader tokens. Protocol round-trip and renderer queue
tests pass again. No rebuild or benchmark result is inferred from committing.
Native RT0/tools visualization still block Port Royal. Local receipts are in
`tmp/commit-dxr-20260910/`; no memory or archive files were changed.

## DXR AS input and allocation failures, 2026-09-10

Current ProgramData release UMD12 `8F15F9DC…` validates AS descriptor envelopes
and primitive-count narrowing, clears failed prebuild output, checks/frees all
three temporary arrays and applies DXR's low-32-bit vertex/AABB stride rule.
[DXR_SERIALIZATION.md](docs/dx12/DXR_SERIALIZATION.md#as-build-inputs-and-prebuild-failures-2026-09-10)
records 34,489 passing engine checks on each of direct NVIDIA and paired Linux
Venus, including allocation fault injection and build/update/copy/serialization
ray readback. The old engine reproduces stale prebuild output. Raw Vulkan
creation-view overlap diagnostics retain fixture-specific attribution.

Linux/Windows/release/A1 pass. Native interactive FL creation/caps and all four
GPU ordering cases pass with exact loaded 8F15F9DC/43394BBD identities; .271,
oem54, Code0 and WDDM2.1 remain. These native tests do not reach the changed RT
code: RT0 and the tools visualization refusals remain. The next subsystem is
decoded AS data with complete GPU mutation/copy/serialization lifetimes, then
native DXR acceptance and Port Royal. No benchmark, performance or owner visual
acceptance is transferred to this build. No commit or push was made.

## FL12 adapter admission, 2026-09-10

Release UMD12 `898F75F9…` was deployed on unchanged .271/oem54/WDDM2.1,
UMD11/ICD and engine archives. All seven adapter-handle callbacks now refuse
foreign handles before forwarding. The extended feature-level query reads only
the runtime input, writes only the output and selects a supported enumerant
within that limit; the legacy ceiling remains12_1. The exact same direct DDI
probe passes184 checks on this build versus36 failures on2D90C57E. A1 and the
Windows release build pass. Native system-runtime creation succeeds through
FL12_1 and refuses12_2; all four native GPU ordering cases pass. Exact loaded
identities and the direct-versus-native evidence boundary are in
[FEATURE_LEVELS.md](docs/dx12/FEATURE_LEVELS.md#adapter-admission-contract-2026-09-10)
and `tmp/fl12-admission-20260910/`.

Port Royal is not ready: RaytracingTier remains0 and tools-visualization AS
query/copy modes remain explicitly unsupported. The complete representation and
its build/update/copy/serialization lifetimes precede native DXR acceptance.
FL12_0/12_1 compatibility admission is working; full genuine conformance remains
open under the sparse exception and existing lifetime/ownership limits. No
benchmark, performance claim, guest/QEMU reboot, commit or push in this increment.

## Previous DXR copy and address contracts, 2026-09-10

[DXR_SERIALIZATION.md](docs/dx12/DXR_SERIALIZATION.md#as-address-and-copy-range-validation-2026-09-10)
records deployed release UMD12 `2D90C57E…` on .271/oem54/WDDM2.1, unchanged
UMD11/ICD and the paired renderer. The engine now fails Close on malformed AS
build/update/copy/query addresses, preserving batch rollback and backing
ownership. Thirteen recording-rejection cases and expanded packed clone/compact,
serialization/replay and ray-readback tests pass33,859 checks on each of direct
NVIDIA and Linux Venus. Linux/Windows/UMD/A1 pass. Copy-stage masks are explicit;
the final synchronization run has no hazards. Its27 raw copy-range diagnostics
compare whole creation views, while every fixture pair's accessed-size bounds
are disjoint. The evidence distinguishes this layer discrepancy from general
AS aliasing conformance; no message was suppressed and no performance gain is
claimed. On this exact build, all19 native query/filter/raster groups pass
781,199 checks and all four native ordering cases pass65,536 words each. Native
caps retain FL12_1/tiled2/SM6.0/RT0; the native DXR probe exits BLOCKED77 before
RT commands. Exact loaded runtime/UMD/ICD receipts and Code0 desktop capture
are archived. No benchmark or performance comparison was run on2D90C57E.

FL12_0/FL12_1 compatibility admission remains; the sparse exception still
prevents a full genuine-conformance claim. Native RT0 and missing tools decode
remain the Port Royal boundary. The next subsystem is tools visualization with
complete AS mutation/copy/serialization lifetimes, then native DXR validation.
The benchmark results below belong to22C31F11 and are not current-build visual
or performance acceptance.

## FL12_1 pipeline statistics, 2026-09-10

[DGC_QUERIES.md](docs/dx12/DGC_QUERIES.md) records the native DGC compute
statistics repair. Native Windows reproduced missing dispatch counts on
5B8A411E; release UMD12 `22C31F11…` now passes all four query groups: 2,218
checks and 56 expanded readback records, with shader counters independently
matching query results. Direct NVIDIA, paired Venus and Intel each pass 2,166
checks. Expanded NVIDIA synchronization validation has no diagnostics. Native
DGC execution remains; the removed command emulation is not restored. The pass
runs only for queried DGC compute, with allocator-owned GPU scratch and no CPU
argument readback or idle wait. Linux/Windows/release UMD/A1 checks pass.

The new UMD is a ProgramData hotplug on unchanged .271/oem54/Code0/WDDM2.1,
UMD11 `57C84ED4…` and ICD `43394BBD…`; the signed package remains older. Native
FL12_1 admission is retained and RT remains withheld. Native inherited
format/UAV/shader coverage passes 23 groups / 529,085 checks; all 13 raster
groups pass 778,722 checks / 420 TIR readbacks. The four native ordering cases
pass, including cross-process GPU completion. The tiled suite passes 18 cases;
reserved 3D textures return the expected tier-2 refusal (overall suite exit77,
not an all-pass result). New min/max filter tests pass 259 native checks and
60 pixel readbacks across DXBC/DXIL and static/dynamic samplers, including mip
reduction and zero-weight texels; NVIDIA, Intel and Venus each pass 249 checks.
The native inventory creates FL11_0 through FL12_1 and refuses FL12_2.
The authorized reserved-resource
compatibility exception, native DXR/Port Royal and existing lifetime/sharing
acceptance limits remain explicit. The old TotalLaneCount1024 estimate is also
an open reporting issue, distinct from the FL12_1 SM5.1 floor.

Full stock Time Spy, Fire Strike and Steel Nomad Vulkan now complete on this
stack, with archived results/XML, exact loaded identities and changing host-VNC
benchmark frames. Graphics scores / measured FPS: Time Spy23,071 / GT1 154.94 /
GT2 128.93; Fire Strike59,806 / GT1 265.00 / GT2 255.24; Steel Nomad Vulkan9,384 /
93.84. Workload settings match the earlier2AD1 stock controls after excluding
adapter/result identity fields. These are single-run measurements with no
attributed performance gain. The owner has not visually accepted these runs.
The guest remains Code0 with no remaining benchmark processes. This is usable
native FL12_0/FL12_1 compatibility support; complete genuine conformance is not
claimed. Native RT0 is still the first Port Royal admission boundary; the next
DXR work is AS accessed-range correctness and tools-visualization operations,
followed by native state-object/AS/DispatchRays validation.

## DXR serialization queries, 2026-09-10

[DXR_SERIALIZATION.md](docs/dx12/DXR_SERIALIZATION.md) now records the
execution-time serialization postbuild implementation. The query no longer
relies on a recorded AS type. Direct NVIDIA and paired Linux Venus each pass
33,642 checks, including a query list recorded before its producer and replayed
over TLAS -> BLAS -> TLAS at one GPU address. Vulkan synchronization validation
reports no hazards; the later copy-range analysis above attributes the six
fixture diagnostics to creation-view extents.
Linux/Windows engine, release UMD12 and A1 pass. ProgramData UMD12 `5B8A411E…`
is hotplugged on unchanged .271/oem54/Code0/WDDM2.1; UMD11 and ICD are unchanged.
All 13 native rasterization groups pass 778,722 checks/420 readback records, and
all four native ordering cases pass. Exact loaded modules are recorded; the
revised native DXR probe admits FL12_1 and exits BLOCKED77 at RT0. The signed
DriverStore UMD12 remains 41A7. Valid scoped queries are virtualized on the native DGC surface; the generic
non-DGC split refusal is not a native admission blocker. Tools visualization,
arbitrary AS alias/lifetime cases and native DXR behavior remain open. No
performance or owner-visible benchmark acceptance is claimed.

## DXR deserialization, 2026-09-09

[DXR_SERIALIZATION.md](docs/dx12/DXR_SERIALIZATION.md) records the new bounded
metadata reader and execution boundary for TLAS-first deserialization. Small and
8,194-reference replay/readback tests and five malformed-header cases pass on
direct NVIDIA and the paired Linux Venus stack: 33,518 checks per stack, no
failures/skips. Linux/Windows engine, release UMD12 and A1 build/checks pass.
UMD12 `CBABC0B8…` is hotplugged on unchanged .271/oem54/Code0, WDDM2.1,
UMD11 `57C84ED4…` and ICD `43394BBD…`; no reboot was needed. All 13 native
rasterization groups pass (778,722 checks, 420 TIR readback records), as do all
four native ordering cases. Exact loaded hashes and system D3D12/Core/DXGI
paths are recorded. The revised native DXR probe admits FL12_1 and exits
BLOCKED77 at RT0; its AS paths remain unexercised. The signed DriverStore UMD12
still carries 41A7. Native RT remains unadmitted. AS view-range/type
validation diagnostics, serialization postbuild queries after GPU-only type
changes, tools visualization and native DXR behavior still require work before
Port Royal. No performance or owner-visible scene acceptance is claimed.

## TIR implementation, 2026-09-09

[TIR.md](docs/dx12/TIR.md) records the forced-sampling contract and current
validation. UMD12 `0C292592…` is hotplugged on unchanged .271/oem54/Code0,
UMD11 `57C84ED4…`, ICD `43394BBD…` and the paired renderer. Host mixed-sample,
MRT/logic and invalid-PSO tests pass; Linux/Windows builds and A1 pass. All 13 native groups now complete with zero failures/skips, including 420 TIR
readback records (210 scenarios replayed twice), six legal creation cases,
invalid inputs, ROV and conservative rasterization. TIR positive GPU cases
have no native debug-queue errors. Optional RGBA32_FLOAT target16 is unavailable;
pending-allocator diagnostics remain unresolved. This is a ProgramData UMD12
hotplug; the signed package still carries 41A7. Full FL12_0/12_1 conformance and
DXR/Port Royal remain open. The subsequent deserialization work is recorded
above; RT/SM capability reporting has not been raised. No benchmark
performance or owner scene acceptance is claimed for this TIR build.

## Native renderer fork, 2026-09-09 — deployed validation

The owner now authorizes virglrenderer changes and requires removal of the
private vkd3d DGC emulation and feedback-fence workaround. The new paired
`virglrenderer` / `venus-protocol` submodules implement native EXT DGC and
NV mixed-sample extension forwarding through Mesa. The engine's private GPU
root and CPU-assisted IA paths are removed. The renderer uses ordinary internal
queue-marker fences; Mesa/KMD retire through authenticated wire receipts and
the 0x14 feedback escape is retired. Keep WDDM2.1, native static UMD and async WSI.

The full contract, capability matrix, provenance, build/activation commands and
remaining limits are in [NATIVE_DGC.md](docs/dx12/NATIVE_DGC.md). Linux Venus on
the actual NVIDIA GPU passes 2,404 indirect checks with no host validation
diagnostics after repairing Mesa's dropped 64-bit buffer-usage chain. Protocol,
renderer worker and 211 KMD logic tests pass. Windows engine/ICD/release UMD and
signed .271 package builds pass. The owner restarted QEMU with the local paired
renderer and the guest package was installed/rebooted: **.271/oem54.inf/Code0**,
WDDM2.1, UmdD3D12=1. Loaded host hashes match; DWM uses release UMD11 `57C84ED4…`
and ICD `43394BBD…`; native probes use release UMD12 `41A7E290…`. The installer
re-signed the KMD to `BEE45488…` (prepared hash `CD282F11…`). Full source/build
and deployed receipts are under `tmp/native-dgc-renderer-20260909/`.
No push or host system installation occurred.

Native FL12_0/12_1 creation and maximum query pass on updated system runtime/Core
10.0.26100.9278 and DXGI10.0.26100.9444. Native DGC root and IA each pass12 cases /
48 words; root signatures12 cases /48 words, SO34 cases and all four native
ordering cases pass. Tiled/inherited suite:18 passes, 3D tiling BLOCKED at tier2;
the overall suite is not a pass. DXR remains BLOCKED77 at RT0. Host VNC confirms
the desktop; benchmark scene acceptance remains the owner's decision.

The stricter DGC query probe remains **failed**: GPU output/guards pass, but
NVIDIA reports zero compute invocations through both direct Vulkan and Venus;
an ordinary-dispatch control counts correctly. The broader multiview query
expectation also fails on direct host and Venus; native view instancing remains
unadvertised. This is not full FL12_0/12_1 conformance. DXR/Port Royal, TIR,
expanded-root native-DGC support and existing sharing/lifetime/host-loss limits
remain open.

All three stock interactive controls complete and export results on this stack:

| Control | Scores | FPS |
|---|---|---|
| Time Spy | overall21,813; graphics22,903; CPU17,181 | GT1 157.847717; GT2 125.318718 |
| Fire Strike | overall36,018; graphics59,097; physics40,946; combined8,765 | GT1 259.785706; GT2 254.168198; combined40.768192 |
| Steel Nomad Vulkan | 9,387 | 93.877197 |

All selected workloads have status0, exact loaded native identities and changing
host-VNC scene captures. Render settings and stock definitions match the earlier
2AD1 controls; only generated result IDs/adapter LUID differ. The updated Windows
runtime prevents isolating either the DGC or marker-fence change's performance
effect. Owner visual acceptance remains pending. The allocator Reset diagnostics
recur; benchmark completion does not close that lifetime question. Full receipts
are in `tmp/native-dgc-renderer-20260909/controls/native-dgc-*-validation.json`.

TIR and native DXR were outstanding at the 41A7 checkpoint; the TIR implementation
and native results above supersede that rasterization boundary. Earlier stock-renderer and feedback/fallback
sections below describe prior deployments and no longer set implementation policy.

## Native feature levels and ray tracing, 2026-09-08

The current owner-directed work is genuine native FL12_0 and FL12_1. Neither
feature level requires advertising DXR; the DXR/Port Royal acceptance remains
separate, followed by FL12_2/Speed Way. Preserve the existing higher-feature
implementation work. DX12 has priority. Keep WDDM 2.1 and the native UMD architecture;
the withdrawn automatic WDDM 2.9 target is not a requirement. The complete
capability/evidence matrix and acceptance limits are in
[`docs/dx12/FEATURE_LEVELS.md`](docs/dx12/FEATURE_LEVELS.md).

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

The owner restarted QEMU after installing upstream system virglrenderer
`cf6c62da`. The new server/library are loaded, and the deployed guest Mesa
`BF0219279E958E18BD76170FE8D2BB1AB3937BCB5ED98B5EDF2B2C2312966767` now exposes
maintenance8. The normal launcher suffices; the isolated wrapper is unnecessary.
No host Mesa update is needed on the NVIDIA Vulkan path. See
[D32_COPY.md](docs/dx12/D32_COPY.md) for exact host/guest identities.

Current release UMD12 is
`6F29859B2585912A241800628E2C66CA59E8079494661B3FFC707B3931ED96B5`, hotplugged
with Code0 on unchanged .270/oem53.inf/WDDM2.1 and UMD11. It includes raw D32
MSAA transfers and the subsequent native immediate-buffer compiler repair.
The runtime converts DXBC ICB words into float arrays in DXIL address space5;
SPIR-V float constant handling quieted signaling NaNs before the depth copy.
The engine now stores these words as integers, preserves pointer aliases and
bitcasts float users, and implements mandatory zero OOB reads. It adds no GPU
wait or CPU readback. The 877-input source manifest is
`64fc0a76321b336425a3b6c21b72a6b93da60e3c935dc15339ef488169afb38c`.

On native Windows, both committed raw-copy controls and the ICB compute control
pass all 1,536 words, including special bits, four samples and two array layers.
These committed-resource controls do not exercise CopyTiles or bypass tiled0.
Native IA12/48-word/12-query/lifetime, root12/48-word, SO34 and all four ordering
cases also pass. Loader traces lose no events; exact system runtime/UMD/ICD
identities are archived. Ten compiler pointer/indexing fixtures validate, and
44 existing shaders produce unchanged output with the same DXC. A1, including
213 KMD logic tests, Linux engine and Windows check/release builds pass.
Evidence is under `tmp/fl12-maintenance8-native-20260908/`.

Native admission remains FL11_0/tiled0/RT0. Full tiled/inherited/format/ROV/
conservative/DXR acceptance remains open, separately from the authorized sparse
compatibility exception. Port Royal has not run on these artifacts. The owner
has stopped other GPU workloads, so new completed controls can establish
performance; earlier shared-GPU results remain unsuitable comparisons.

The owner now authorizes reserved-resource committed backing as an explicit
compatibility exception. [SPARSE_COMPATIBILITY.md](docs/dx12/SPARSE_COMPATIBILITY.md)
records its ignored mappings/aliases and full-allocation cost. Color4 backing is
selected from an identity-bound GPU behavior probe, with no driver allowlist;
unknown/stale/failed results select compatibility backing. This restores progress
past the stock sparse-MSAA failure without claiming that the host defect or the
full tiled contract is fixed. Native caps remain FL11_0/tiled0/RT0. The current
implementation/build/diagnostic work is under `tmp/fl12-sparse-compat-20260908/`;
the following 22D1 and 8C747 records describe distinct, older artifacts.

The preceding deployed UMD12 was
`BE9D0EBE5F3A021848429A8ED0F641CC908BB1809DF8EC9F3192B1BD44F343A6`,
adding GPU-predicated single-sample CopyTiles for buffers, color, BC and raw
depth images. One allocator-owned 64-KB scratch tile preserves byte offsets,
edge padding and false-predicate destinations; internal compute does not enter
application query counts. Source/build/deploy records and the 876-input manifest
`2d1989ae0f260231c5c88aeef0ade441f84f44794e4799cfbed32383148c00b0`
are in `tmp/fl12-predicated-tiles-20260908/`. Host tile regressions pass 12,151
assertions with no skips/failures or Vulkan validation errors. Mechanical checks
and Windows release builds pass. See
[SPARSE_COMPATIBILITY.md](docs/dx12/SPARSE_COMPATIBILITY.md#predicated-single-sample-copytiles).

Code0, .270/oem53.inf/WDDM2.1, UMD11 and ICD are unchanged; the native adapter
LUID is `00000000:0711b78c`. Native BE9D IA12/48-word/12-query/lifetime and four
ordering cases pass with exact loaded identities; the loader trace loses no
events. The predicated tiled probe is built but BLOCKED77 at tiled0, before GPU
tile commands. FL11_0/tiled0/RT0 remain the admitted surface. Raw D32 MSAA copies,
full formats/inherited/ROV/conservative/DXR validation and the documented sparse
compatibility exception still prevent complete FL12_0/12_1 and Port Royal
acceptance. No commit, push, KMD or launcher change occurred.

All three BE9D stock controls completed in Session1 with matching workload
settings, loaded artifact identities, agreeing archives/exports and changing
host-VNC rendered frames. The graded receipt is
`tmp/fl12-predicated-tiles-20260908/benchmark-controls-be9d.json`.

| Control | Prior 9BDA FPS | BE9D FPS | BE9D score |
|---|---|---|---|
| Time Spy | GT1 132.444885 / GT2 115.121292 | GT1 134.943024 / GT2 95.854523 | overall 16,344 / graphics 18,374 |
| Fire Strike | GT1 242.445877 / GT2 254.851700 / combined 44.970276 | GT1 167.547668 / GT2 196.592453 / combined 45.349815 | overall 31,238 / graphics 41,609 |
| Steel Nomad Vulkan | 88.768120 | 90.496490 | 9,049 |

Those shared-GPU runs do not establish performance regressions: Time Spy GT2
is down16.74% and Fire Strike
GT1/GT2 down30.89%/22.86%; causes are not established. Time Spy CPU FPS also
drops from45.704536 to33.772419. DX11 UMD/ICD/KMD are unchanged. A ten-sample
host GPU observer during the later Vulkan control finds two unrelated Python
workers resident in57,056 MiB; their utilization fields are unavailable/dash,
so neither concurrent execution nor its effect on earlier controls is proven.
No unrelated process was changed. Sampled benchmark frames need owner visual
acceptance, and these controls do not exercise native tiled or RT commands.
The owner confirmed concurrent GPU use for those runs. Their completion and
rendered frames remain correctness evidence; their FPS/scores cannot attribute
a performance change. The owner subsequently stopped the other GPU workloads,
as recorded above.

The BE9D final guest state was Code0 with no remaining benchmark/probe processes.
`final-guest-state.json` verifies ProgramData BE9D, unchanged UMD11/ICD/KMD and
the older packaged UMD12 ADC0B0EA in DriverStore. The hotplug script's warning
that DriverStore has no DX12 UMD is stale; this deployment still does not prove
cold-boot or rebuilt-package acceptance. Hosted CI for the dirty candidate is
unverified. The D32 investigation and implementation have advanced as recorded
above. Renderer activation and the guest updates are now complete. Native
format/inherited/ROV/conservative/DXR checks remain necessary before increasing
admission.

The preceding IA continuation build is
`361C9767AC9B772D5F9CEAE998E68D2669AF7DFA46E999F63471DD17170129F4`,
hotplugged with Code0 on unchanged .270/oem53.inf/WDDM2.1, UMD11 and ICD.
Its 875-input production manifest is
`835042eda2b114d0954ad40ca32a1090a002de0846874c387cb2479e258f3c8d`;
source, build, deployment and native receipts are in
`tmp/fl12-indirect-ia-20260908/`. Its native adapter LUID was
`00000000:06d904c0`. Caps remain FL11_0/tiled0/RT0.

VBV/IBV ExecuteIndirect now uses an isolated, execution-time CPU continuation
when native DGC is absent. Root-only signatures retain GPU processing. Each ECL
owns staging and a private recorder, waits only its exact prefix outside the
Vulkan queue lock, and completes HE12 after generated work and the suffix.
Native Session1 PID9632 passes 12 GPU-producer/replay cases, 48 words, 12 query
results, false predication and pending public-list Reset behind another queue's
dependency. The first readback failure was a probe SV_VertexID assumption:
an ordinary indexed-draw control failed identically and passed after correcting
the triangle. No production change was needed for that failure. Host IA/query
tests and A1/Windows release checks also pass. See
[INDIRECT_EMULATION.md](docs/dx12/INDIRECT_EMULATION.md#ia-continuation-implementation-and-validation).

This is bounded IA acceptance, not complete feature-level or DXR admission.
Two existing pending-allocator Reset diagnostics occurred after native fence
completion; the allocator/fence-worker retirement question remains open.
Broader IA topology, inherited descriptor state and asynchronous failure paths
still need native validation. No IA performance comparison or benchmark result
has yet been collected on 361C; the completed controls below belong to 9BDA.
That artifact still refused predicated single-sample CopyTiles, now implemented
in BE9D. Raw D32 MSAA copies remain a concrete implementation gap, alongside the
documented sparse compatibility exception and remaining format/inherited/ROV/
conservative/DXR obligations.

The preceding combined build was
`9BDA548C4577008F237978A1658D53005227B0C0E3E0F0EF3DBA02E60BE401A6`, hotplugged
with Code0 on unchanged .270/oem53.inf/WDDM2.1, UMD11 and ICD. Its 874-input
production manifest is
`0223d596610fe81c050d285b773ac08eae6b954d21595c3cc4fac99ce7dadad8`.
Host tests pass for compatibility mappings, color/D16 sample copies, tile/queue
regressions and cache parsing. Raw D32 MSAA copies explicitly refuse after
special-value bit loss. All seven color4 sparse Vulkan probes fail on both host
and guest while committed controls pass. Native 9BDA root12/48-word,
indirect12/48-word, SO34 and four ordering cases pass with loaded identities;
tiled commands remain BLOCKED77. Adapter restart changes the LUID to
`00000000:06376d04`; native initialization rejects the old cache and reads the
seven FAIL records after re-probing. This establishes cache consumption, not
native tiled conformance. See SPARSE_COMPATIBILITY.md for receipts and remaining
failure/lifetime/format limits. No commit, push, KMD or launcher change occurred.

All three 9BDA controls completed through interactive scheduled tasks with exact
loaded identities, stock settings and archived/exported results. Comparison:
`tmp/fl12-sparse-compat-20260908/benchmark-controls-9bda.json`.

| Control | Prior 22D1 FPS | 9BDA FPS | 9BDA score |
|---|---|---|---|
| Time Spy | GT1 136.182602 / GT2 116.635201 | GT1 132.444885 / GT2 115.121292 | overall 18,824 / graphics 20,192 |
| Fire Strike | GT1 246.294510 / GT2 249.597260 / combined 41.122017 | GT1 242.445877 / GT2 254.851700 / combined 44.970276 | overall 36,557 / graphics 57,153 |
| Steel Nomad Vulkan | 90.797775 | 88.768120 | 8,876 |

Changing host-VNC frames cover Time Spy demo, Fire Strike demo and the Steel
Nomad Vulkan graphics test. The first Steel Nomad run also completed (88.469772
FPS, score8,846), but its captures missed rendered frames; that result is
preserved and the control was repeated once for timed capture. No owner visual
acceptance or causal performance gain is established. Time Spy GT1/GT2 are
2.74%/1.30% lower and the unchanged Vulkan control is 2.24% lower; Fire Strike
varies in both directions despite unchanged UMD11/ICD. These are single
comparisons, with different VNC sampling. The existing allocator-reset/fence-worker
question remains open. Final inspection records Code0, exact configured9BDA,
unchanged DWM UMD11/ICD, a visible desktop and no active probe/benchmark; the
host capture loop completed. Full receipts are in
`tmp/fl12-sparse-compat-20260908/evidence.json`.

Root signatures now preserve the parsed DDI's range/root/static-sampler flags
through a private versioned engine factory. The driver path accepts 128-DWORD
roots while the public API remains limited to 64; masks, ordinary uploads and
indirect layouts cover the full driver range. ClearRootArguments zeros only
root arguments and preserves other command-list state and bundle inheritance.
The contract, source/build provenance and test limits are in
[ROOT_SIGNATURES.md](docs/dx12/ROOT_SIGNATURES.md). Root-only candidate 6125
passes native root 12/48-word, indirect 12/48-word, SO 34 and four ordering cases.
Host tests cover the private 128-DWORD path; native runtime-added roots beyond 64
and native sampler 1.2/OOM injection remain unexercised.

A further CopyTiles regression exposed an invalid 64-KB buffer-offset refusal
in the UMD and engine. Buffer offsets are byte offsets. The repair preserves
aligned direct copies and uses one allocator-owned 64-KB tile for Vulkan-unaligned
image copies, including transfer barriers and edge-row preservation. The original
364-assertion host test and 27 new format/offset cases pass; the combined latest
run has 936 assertions, no failures or skips. The native tiled probe now includes
64-KB, 32-byte and 1-byte offsets, but tiled 0 still blocks native execution. The
deployed build refuses MSAA CopyTiles; the subsequent candidate below does not
yet satisfy tier 2.

The prior root/byte-offset UMD12 build was
`22D1323318320016F19CA9BBD38605AB51E6A724FEAE730BFD1C01FFFA8182C1`,
hotplugged as `C:\ProgramData\HeliosUmd\helios_umd12_22d1323318320016.dll`.
Windows engine/UMD12 check+release, native probe compilation and A1 checks pass.
Source/build and deployment receipts are in `tmp/fl12-root-contract-20260908/`;
the 863-input production manifest is
`0b19b6a28faee04002944708518ab9bbd029c2185735b1aac96763178ba8726f`.
The KMD remains 22.22.270.0/oem53.inf, Code 0, WDDM 2.1; UMD11 and ICD are unchanged.
DriverStore UMD12 remains ADC0B0EA; this is a ProgramData override. Native 22D1
root and indirect suites each pass 12 cases/48 words; SO passes 34 cases and
the four ordering cases each verify 65,536 words. The verified receipt is
`tmp/fl12-root-contract-20260908/native-validation-22d1.json`; both ordering
processes have ETW-confirmed system-runtime/UMD/ICD identities. That deployment
used Helios LUID `00000000:05296ca9`. The expanded native CopyTiles case returns
BLOCKED77 at tiled tier 0, without executing tile copies. Benchmark controls
complete on this build; no new work has been committed or pushed.

All three stock controls completed through interactive scheduled tasks, with
archived/exported results, matching settings and loaded artifact identities.
The exact comparison is `tmp/fl12-root-contract-20260908/benchmark-controls-22d1.json`.

| Control | Previous CB48 FPS | Prior 22D1 FPS | 22D1 score |
|---|---|---|---|
| Time Spy | GT1 133.570267 / GT2 116.756340 | GT1 136.182602 / GT2 116.635201 | overall 19,073 / graphics 20,598 |
| Fire Strike | GT1 245.357132 / GT2 247.547211 / combined 44.061520 | GT1 246.294510 / GT2 249.597260 / combined 41.122017 | overall 35,391 / graphics 57,025 |
| Steel Nomad Vulkan | 91.006409 | 90.797775 | 9,079 |

The VNC pairs show changing Time Spy **demo**, Fire Strike GT2 and Steel Nomad
Vulkan graphics frames. They do not establish owner visual acceptance. These
single completed comparisons establish no causal performance gain; Fire Strike's
combined FPS is 6.67% lower despite unchanged UMD11/ICD, with the cause unresolved.
The owner's accepted .266 Time Spy shadows/about 100 FPS and the instrumented
74.26 FPS run remain separate evidence.

The final native inventory, PID2216/session1, loads exact 22D1 with system
D3D12/Core and the expected ICD, on LUID `00000000:05296ca9`. FL11_0 creation
succeeds; FL11_1/12_0/12_1/12_2 return `0x887a0004`. Maximum FL11_0, tiled 0,
RT0, SM6.0 and root-signature API 1.1 remain unchanged. Evidence is
`tmp/fl12-audit-20260907/native-20260908-022139-573-fa8349c66b514cfaae86a840f4217472/`.
This inventory does not establish full FL11_0 conformance.
`tmp/fl12-root-contract-20260908/validated-checkpoint-22d1.json` links all current
receipts. Final guest inspection confirms Code 0, the unchanged DriverStore UMD12,
the intended configured UMD12, loaded DWM UMD11/ICD and a visible desktop. No probe
or benchmark remains running; all host capture loops completed. The 863 production
input hashes matched that deployed checkpoint. The following failed 8C747
checkpoint was never deployed; the subsequent compatibility build is described above.

The archived 8C747 candidate implements isolated per-sample MSAA CopyTiles shaders, raw
color views, depth attachment writes, byte offsets and GPU predication, plus
mandatory compute/graphics queue continuations with error propagation and retained
allocator ownership. It removes the committed-resource substitution for an
unsupported reserved format. Single-sample predicated CopyTiles and unvirtualized
scoped queries now refuse explicitly; those contracts remain unfinished.
Ordinary depth copies also use the required graphics continuation when Vulkan
cannot execute them on compute/transfer queues. See
[FEATURE_LEVELS.md](docs/dx12/FEATURE_LEVELS.md#msaa-candidate-and-stock-host-boundary)
and [EXECUTION_SYNC.md](docs/dx12/EXECUTION_SYNC.md#required-queue-continuations).

**A stock-host blocker is now demonstrated.** The prior global sparse-MSAA
inventory was insufficient: NVIDIA 610.57.04 and the loaded Venus ICD both return
zero sparse format properties for four-sample D16/D32, with transfer-only and
depth-attachment usages. A standalone Vulkan color control additionally passes
single-sample readback but loses half a four-sample tile; an array-edge sparse
bind returns device lost. Explicit core/synchronization validation reports zero
errors in these reproductions. The color failures occur without vkd3d, Helios,
Venus, storage usage or the new shaders. Evidence is under
`tmp/fl12-msaa-20260908/`; the guest query is interactive, records the exact loaded
`3349607B…` ICD, and does not execute D3D12 commands.

The final undeployed UMD12 SHA256 is
`8C7471DA959FF699FA11B7804580B3101D1DB271D0B0F0BC09E1686568592C0D`.
Linux engine and Windows engine/release UMD builds pass; all 872 production
inputs match the Windows mirrors (manifest `c9906620…`). The final host copy-queue
suite passes 6,291,594 assertions, single-sample CopyTiles936, indirect138/1570
and query continuation140, with zero Vulkan validation errors. A1 passes,
including213 KMD logic tests. The MSAA suite instead completes with189 readback
failures and18 required-depth-format skips; it is a failed acceptance run.
`tmp/fl12-msaa-20260908/evidence.json` records exact source/build hashes and
validation attribution. That pre-compatibility guest inspection found configured22D1, unchanged
loaded DWM UMD11/ICD, Code0 and a visible VNC desktop. No benchmark, performance
measurement or owner visual acceptance belongs to the new candidate.

The failed 8C747 candidate was not deployed. The compatibility implementation
supersedes its strict refusal policy, while the independent IA VBV/IBV work,
remaining sample/predication cases and native validation still prevent a
capability increase. A corrected stock host can be rechecked with the dynamic
probe to restore sparse backing; current work does not depend on a driver-name
exception. No host driver, renderer or VM-launcher change has been made.

Initial native inventory on .270/oem53.inf, Code 0, explicit `UmdD3D12=1` loads the
packaged UMD12 (`ADC0B0EA…`) with Microsoft D3D12/D3D12Core 10.0.26100.8972 and
Venus ICD `3349607B…`. FL11_0 creation succeeds; FL11_1/12_0/12_1/12_2 return
`DXGI_ERROR_UNSUPPORTED`. The runtime advertises understanding FL12_2 and
negotiates R8_0110 with Helios. This is admission evidence, not full FL11_0
conformance. Source and guest artifacts are distinct; the separately built
default-change DLL (`21339235…`) is not the installed DLL.

The first confirmed stock-stack boundary for the current engine's state-changing
ExecuteIndirect path is `VK_EXT_device_generated_commands`: present on the host,
absent from the loaded Venus ICD and the stock renderer protocol. This is an engine
dependency, not a Vulkan extension mandated by a D3D feature level. FL12_0 still
inherits this mandatory D3D12 behavior; lowering the target does not remove the
boundary. Ordinary indirect draws and
dispatches already have non-DGC paths. The upstream/history audit found a separate
compute root-parameter fallback, removed in `76c11d2e` on 2026-04-08 and still absent
at fetched upstream `35bdee1435c94f8c3548725fcb046595b263bd7e` on 2026-09-07.
That historical implementation did not cover graphics VBV/IBV/root changes.
The owner authorized an isolated, removable engine fallback using available
extensions, with native EXT DGC preferred automatically when present. Its
contract is [`INDIRECT_EMULATION.md`](docs/dx12/INDIRECT_EMULATION.md).
Owned root/PSO creation inputs, lazy shader-layout variants, GPU argument
patching and root-state execution now build on Linux. Native signature creation
translates constants and root CBV/SRV/UAV changes into this engine path.
The old non-DGC silent skip is not an accepted fallback. The detailed source receipt and
application-fallback distinction are in `docs/dx12/FEATURE_LEVELS.md`.
Keep unsupported IA/optional-tier refusals and the FL11_0 ceiling. Sparse binding and the RT feature
chain reach the guest, including recursion31, but engine/native correctness must
still be established. Remaining inherited obligations include complete stream-output limits and
native runtime instrumentation of expanded root signatures. FL12_0 still needs
complete tiled tier 2, format/MSAA, binding, typed-UAV and inherited behavior.
FL12_1 additionally requires ROV and conservative-raster behavior. Native
admission remains FL11_0 until each higher contract is backed and validated.

The fallback has host tests passing for root descriptors (33 assertions), partial
constants/state clearing (25), graphics/compute predication (1,570), indexed draw
offsets (26) and query continuation (140), with DGC and descriptor-buffer
extensions disabled. The latter four enabled Vulkan validation. Independent
review then repaired raw-CBV visibility, predicated internal reductions, query
address arithmetic and failure propagation through Close/Reset. A new
GPU-producer test caught indirect hoisting across combined read states. Its
expanded 138 assertions now pass with Vulkan validation, covering COMMON
promotion and buffer aliasing as well as replay, counts, predicates and
root-constant clearing. The earlier NULL-CBV clearing oracle was invalid and
removed; the corrected test rebinds a valid CBV before drawing. The repaired
query continuation passes 140. A host
DGC comparison instead refuses the root-CBV signature under NVIDIA's existing
push-descriptor policy; this is a separate recorded native-DGC boundary. The
multiview statistics test has 194 failures in 605 assertions, an explicit engine
gap outside native ViewInstancingNONE and FL12_0. The first 73-file whole-change
review was not dry: repairs cover AS HRESULT propagation, signature OOM
diagnostics, the tiled probe's loss-sentinel check and the invalid test oracle.
The host AS recording-failure test passes ten assertions; allocation/device
loss fault injection remains unexercised. The next 76-file whole-change round
was also not dry: it found allocating diagnostics before native Close/Reset
could deliver OOM, and a legal public SO semantic colliding with the private
DDI register marker. Repairs defer OOM diagnostics and carry explicit per-PSO
DDI origin through a private engine factory, owned compiler metadata and cache
keys. Public legacy/stream SO creation plus cached recreation each pass 36 host
assertions; private physical capture passes 34, ordinary DXBC user capture 33,
and the indirect GPU-producer regression 138, all with Vulkan validation.
Fresh Windows engine and both-UMD check/release builds pass after the repairs.
The third round covered all 79 files and was not dry: it found the private SO
factory's unguarded exception boundary and the DXR harness's unbounded wait
after requesting child termination. The bridge now uses the shared guard with
an explicit throwing C declaration, preserves E_OUTOFMEMORY without allocating
diagnostics, and leaves failed outputs clear. Eight synthetic cases exercise
the actual extracted wrapper under clang-cl /EHsc. This contains escaping
exceptions; compiler-wide OOM cleanup and safe retry remain unestablished.
The DXR wrapper now bounds termination/output waits and archives failure even
when the child may remain running. The native indirect probe builds; 17
provenance and 30 archive/timeout synthetic cases pass. The full host SO filter
passes 9,316 assertions with Vulkan validation. Both-UMD Windows check/release
and A1 pass after these repairs; A1 uses the task's dcdb8b38 starting commit.
The fourth round read all 81 files and found scratch-allocation HRESULTs
collapsing to boolean failure: NULL-SO backing OOM became E_INVALIDARG, while
other new recording paths assumed every Vulkan failure was OOM. The allocator
now preserves its HRESULT for these paths, retaining the existing boolean API
for unrelated callers. Definite AS reserve/calloc failures also retain OOM;
native RT admission still withholds those paths. Focused host regressions pass
138/1,570/140/10 assertions and the full SO filter passes 9,316 with validation.
An extracted-helper test with ASan/UBSan passes synthetic error propagation,
first-error preservation and scratch reuse checks; it is not native fault
injection. Linux engine and Windows engine/UMD12 check/release builds pass.
The fifth round also read all 81 files and found an unguarded lazy pipeline
compilation inside the new source mutex. A private C++ callback guard now
returns allocation/other exceptions as HRESULTs within that ownership scope;
temporary-root release and mutex unlock still run, and only success publishes
the variant. Twelve synthetic graphics/compute cases and two cached lookups
pass on Linux and Windows using the actual factory extraction and Windows
mutex operations. Compiler-wide allocation cleanup/safe retry remains open.
Linux engine, Windows engine/both UMD builds and the 138/1,570/140 focused GPU
regressions pass after this repair. The sixth round read all 83 files, closed
the local factory guard and found allocating native diagnostics after the void
ExecuteIndirect call. Under sustained OOM those diagnostics could abort before
Close delivered the latched error. Forwarding now bumps its existing atomic
counter without a per-call trace or first-hit summary. The counter remains
readable through the device summary and counts recording calls, not completed
GPU actions. Sibling recording handlers have no equivalent post-call allocation.
A1 and both-UMD Windows check/release pass after this repair. The candidate is
UMD12 `CB48D9DB…`; the earlier `1400C52F…` archive predates it. This is a local
return-path repair, not proof of general sustained-OOM tolerance or fault injection.
Whole-change rounds IR7 and IR8 are consecutive dry rounds with different lens
compositions over the same 83-file freeze. Every reviewer covered the complete
diff, peer-refuted hypotheses and reverified the source/build receipts. The
manifest is `f81f76e3…`, in `source-freeze-indirect-round7/`; review dispositions
are `reviews/indirect-round7.json` and `indirect-round8.json` under the audit
directory. Subsequent evidence documentation does not change those driver bytes.

UMD12 `CB48D9DB…` was hotplugged from
`fl12-build-20260907-214407-218/`, with unchanged UMD11 `245D1BC3…`, ICD
`3349607B…` and KMD .270/oem53.inf. PnP restart succeeded, Code 0 and the desktop
returned, and the new adapter LUID is `042a5ac7`. No guest reboot or launcher
change was needed. This is a ProgramData override; DriverStore still contains
UMD12 `ADC0B0EA…`, so it is not a signed-package upgrade.

On the exact loaded candidate and Microsoft system runtime, the interactive
native indirect probe passes all 12 cases/48 readback words: GPU-produced root
constants/CBVs, three producer-barrier routes and four closed-list executions
with counts 3/1/0/7. It records one variant, three patch recordings, nine maximum
action slots and 4,176 scratch payload bytes; those counters are not executed
action counts. The 34-case SO regression and all four native ordering cases
also pass, including 65,536-word producer/consumer readbacks and negative wait
intervals. Zero-loss loader ETW identifies both ordering processes. Receipts are
`native-cb48-root-validation.json` and
`native-sync-cb48d9db/root-validation.json`. Native compute/indexed/SRV/UAV
indirect forms, real OOM injection and broader lifetime/failure acceptance remain
unexercised. Caps still report FL11_0/tiled 0/RT0/ViewInstancingNONE; full FL12_0
and owner visual acceptance are not established.
A fresh completed Time Spy baseline on the installed `6344CB09…` build scored
18,907 (GT1 136.838531 FPS, GT2 112.928223 FPS), with exported results, native
module identities and changing rendered stages. This precedes deployment of
the fallback and establishes no gain over the older nonmatching ~100 FPS run.
Fresh matching baseline controls also complete: Fire Strike GT1/GT2/combined
246.812607/249.133408/39.256428 FPS, and Steel Nomad Vulkan 89.869423 FPS.
All three have exported/archive results, workload module identities and
changing rendered frames; none establishes owner visual acceptance. The first
candidate Time Spy attempt (`timespy-after-indirect-cb48d9db`) was cancelled
after GT1, with no completed score/export. Its result records GT1 status1000
and CANCEL because the workload reported windowed mode despite fullscreen
settings. VNC captures show a persistent Start menu overlay; who opened it
and what caused the mode transition are unproven. The invalid partial result
is excluded from performance comparisons.
The fullscreen retry completed with matching settings and all four workload
statuses0: score19,036, GT1 133.570267 FPS and GT2 116.756340 FPS. This is -2.39%
and +3.39% respectively against the preceding6344 baseline, with total score
+0.68%; no consistent gain is established. Exact loaded identities, export and
changing unobstructed scored GT1 frames are graded in
`controls/timespy-after-indirect-cb48d9db-fullscreen/root-validation.json`.
The matching Fire Strike control completes at GT1/GT2/combined
245.357132/247.547211/44.061520 FPS. Its graphics tests change by less than 1%;
the combined-test increase is on the unchanged DX11 UMD. Steel Nomad Vulkan
completes at 91.006409 FPS (+1.27%), with exact ICD identity, exported results
and changing scored frames. All three comparisons and excluded attempts are
recorded in `controls/indirect-cb48-comparison.json`; they establish no fallback
speedup. Owner visual acceptance remains pending. An optional native direct/indirect
measurement mode is under review; it separates QPC recording costs from
same-queue GPU timestamp deltas and grades every pixel. No measurement from it
is claimed yet.
Fixed-function indirect
VBV/IBV execution remains unimplemented; see INDIRECT_EMULATION.md for exact
implemented/refused/unreachable/unexercised behavior and diagnostic grading.

In-progress source implements admitted sparse mappings with heap retirement,
reserved creation/tiling/copies, native DXR state/AS/dispatch translation and SO
translation/compiler work. The earlier reviewed `6344CB09…` UMD supplied the
baseline evidence below and is now superseded by `CB48D9DB…`. Acceptance remains
bounded. Exact
unsupported forms, mechanical results and review/validation status belong in the
live feature contract, never inferred from slot coverage or an engine cap dump.
The engine build and latest both-UMD Windows check/release passed
(`6344CB09…` UMD12), including SO overflow/counter handling, DXR bundle admission,
fallible SO-owned array construction and OOM reporting without Rust diagnostic
allocation in the new reserved/tiled/DXR failure paths. UMD11 and the engine
archives are unchanged by these Rust repairs. The full host SO suite passes
9,246 assertions, and the deferred DXR collection suite passes 64, with no
failures/skips. SO's corrected 34-case native probe now passes on the candidate,
including authenticated GPU readback. Host engine mapping/remap and sparse-buffer lifetime checks pass
15,816 and 36 assertions; they do not enter guarded native admission. Review
rounds 1–5, 7 and 8 were not dry; round 6 was dry. Repairs cover DXR collection/stride
translation, allocation failure, native probe lifetime, executable/parsed-runner
attribution and archive publication. Sixteen synthetic archive cases, thirteen
synthetic provenance cases and the Windows build-lock check pass. All three
probe builds pass after the latest wrapper repairs, and the
`fl12-build-20260907-062651-506/` capture binds their receipts and the unchanged
driver bytes to that earlier reviewed source. The fourth round corrected stale
validation text. The fifth repaired the shared DXR guard to allow bundle pipeline
binding and ray dispatch; AS operations remain prohibited in bundles. Two added
native bundle readback cases build but remain unexercised behind RT_NONE. The
seventh round repaired SO-owned allocation failure with a named E_OUTOFMEMORY
callback and a cleared shader handle. Shared shader/Slot allocation still has
infallible OOM paths, and native allocation-failure injection is unexercised.
The eighth round extended nonallocating OOM diagnostics to reserved-resource
creation, tiled mapping and DXR, including their missing-error-channel counters.
Its closure review also repaired diagnostic allocation before returning a legal
RenderCb E_OUTOFMEMORY from the shared submission helper to tiled cancellation.
The original tiled reservation witness occurs before mapping commit and proves
lost failure propagation, not an orphaned committed mapping. Per-round
review dispositions live in `tmp/fl12-audit-20260907/reviews/`. Rounds 9 and 10
were consecutive dry whole-diff rounds with rotated lenses over all 55 files.

UMD12 `6344CB09…` is deployed through the ProgramData hotplug, with unchanged
UMD11 `245D1BC3…`, KMD .270/oem53, ICD `3349607B…`, explicit `UmdD3D12=1` and
Code 0. The first native check caught the old cached DriverStore UMD; a successful
PnP device restart refreshed it without a guest reboot. PID9380 then loaded the
exact candidate and system runtime. DriverStore UMD12 remains `ADC0B0EA…`;
this hotplug is not an updated signed package. The current Helios LUID is
`00000000:022f39b4`. Both async WSI and retire feedback remain enabled.

Native SO PID8440 passes all 34 cases, with 34 authenticated submissions and
GPU-readback checks, plus the negative SO root-permission case. The first runs
exposed a probe-oracle error: SV_VertexID excludes StartVertexLocation. Explicit
VS root-constant draw tags now retain distinct iteration/phase data; the negative
root has identical parameters and differs only in ALLOW_STREAM_OUTPUT. Two
independent reviewers closed this probe-only repair; driver bytes are unchanged.
The exact receipt is `native-so-tagged-6344cb09/run-20260907-092322-874-8c557f35/`
in the audit directory. Full SO limits and allocation-failure injection remain open.

The existing four native synchronization cases also pass on candidate6344 in
session1, with both producer and consumer 65,536-word GPU readback patterns and
the negative unsignaled intervals. The freshly built, unchanged probe is archived
in `native-sync-6344cb09/`. A zero-loss process/image trace in
`fl12-sync-loader-6344/` identifies parent PID10136 and shared-fence child PID9192
loading the exact candidate, system D3D12/Core/DXGI and ICD. No WARP or app-local
engine substitution is present in their complete traced lifetimes. This is bounded
ordering regression acceptance at FL11_0; sparse/DXR and broader lifetime/failure
obligations remain separate.

All 12 tiled cases return BLOCKED77 at TiledResourcesTier0, with exact loaded
candidate identities. The DXR probe returns BLOCKED77 at native FL12_1 creation;
its post-call snapshot contains system runtime modules, no admitted UMD/ICD.
Neither result exercises tiled or ray-tracing commands. Native caps remain
FL11_0/tiled 0/RT0. DDI ROV1/conservative3 replies become API0/0, consistent with
Microsoft's published FL11_1+ eligibility requirements; the runtime's internal
branch is untraced. These results are not full FL12_1 or Port Royal acceptance.

The installed ADC0B0EA stack completed full stock Time Spy at **18,950 overall /
20,335 graphics / 13,673 CPU**, GT1 **134.903915 FPS**, GT2 **114.811371 FPS**.
All four workload processes loaded the expected native stack on Helios; the
archive/export and settings are in
`tmp/fl12-audit-20260907/controls/timespy-before-adc0b0ea/`. Host VNC captured
changing demo and GT1 frames; GT2/CPU were not captured. This baseline has no
owner visual acceptance and establishes neither a candidate result nor a gain.
The installed stack also completed full stock Fire Strike: **36,284 overall /
56,972 graphics**, GT1 **245.321411 FPS**, GT2 **250.140366 FPS**. All five native
DX11 workloads loaded the expected UMD11/ICD and returned status zero. The
archive/export, 1920x1080 settings, host VNC and corrected capture-stage grading
are in `controls/firestrike-before-adc0b0ea/` under the same audit directory.
Owner visual acceptance remains separate; no candidate result is implied.
Two stock Steel Nomad Vulkan baseline collections also completed (8,934 and
8,848), but the captures establish at most one rendered frame per run. Their
results/exports and exact ICD identities are retained under the audit's controls
directory; changing-frame and owner visual acceptance are not established.

Candidate6344 completed the same full stock controls through interactive tasks:

| Control | Before | Candidate6344 | Evidence |
|---|---|---|---|
| Time Spy | 18,950 overall / 20,335 graphics; GT1 134.903915, GT2 114.811371 FPS | 19,111 / 20,418; GT1 136.239441, GT2 114.720390 FPS | All four workloads status0, exact candidate/system runtime/ICD in every process; changing GT2 frames |
| Fire Strike | 36,284 overall / 56,972 graphics; GT1 245.321411, GT2 250.140366 FPS | 33,605 / 55,791; GT1 243.482605, GT2 241.663803 FPS | All five workloads status0, unchanged native UMD11/ICD; changing demo and GT2 frames |
| Steel Nomad Vulkan | 8,848 / 88.486877 FPS in the second baseline collection | 8,777 / 87.771965 FPS | Vulkan backend, Helios adapter and exact ICD; changing rendered frames |

Results, exports, settings comparisons and module identities are in the audit's
`controls/*-after-6344cb09/root-validation.json` records. Benchmark settings and
the three-second read-only module observer match; the PnP restart changes the
Helios LUID, result paths/IDs differ, and VNC sampling differs. Fire Strike's
combined test falls from 43.629429 to 36.958103 FPS in this comparison despite
unchanged DX11 UMD/ICD bytes. Its cause is unresolved, not an established DX12
implementation regression or an accepted performance result. No gain or owner
visual acceptance is claimed. The earlier Steel Nomad captures remain incomplete.

One focused Fire Strike repeat on the unchanged candidate completes at
35,126 overall / 56,747 graphics, GT1 246.286224 and GT2 247.174545 FPS.
Combined performance is 40.316246 FPS, still below the 43.629429 baseline.
All five workloads return status0 and load the same native UMD11/ICD. Candidate
UMD12 is not observed by the three-second module polling. Settings match, and
host VNC captures changing combined frames.
Its result/export and grading are in `controls/firestrike-after-6344cb09-repeat/`.
The two candidate combined results vary; neither establishes the cause of the
decrease. Performance acceptance remains unresolved, and no optimization follows
from this observation.

Port Royal fails both stock workloads at native `D3D12CreateDevice` with
`DXGI_ERROR_UNSUPPORTED` (0x887a0004). The CLI exits0 and writes a result containing
workload status10000/zero scores, but produces no export; the wrapper correctly
returns1. A separate loader-trace repeat in `fl12-pr-loader-6344/` records both
31–33 ms session1 workload processes loading candidate6344 and Microsoft's system
D3D12/Core/DXGI. Neither loads the Vulkan ICD or reaches UMD CreateDevice.
There are zero lost ETW events/buffers, and no WARP/app-local vkd3d module.
The saved error does not expose the numeric requested minimum feature level.
This diagnoses native admission failure; it is not a completed Port Royal run.

Postdeployment whole-diff round11 is dry under rotated independent lenses,
including current SO probe attribution and the comments-only caps correction.
Its frozen source and 17 counter gradings are recorded alongside the predeployment
round9/10 saturation. The driver binary remains the round9 build; these probe,
comment and evidence updates do not constitute a new driver compilation.
## D3D12 on AMD/RADV: every D3D12 present scrambled, root-caused and fixed, 2026-09-09

**Symptom (first AMD host run of the D3D12 stack, RX 6600 / RADV, WinBoat guest, .270):**
every frame a D3D12 swapchain presents reaches the screen as horizontal stripes
in 128-px columns - Steel Nomad Light, and equally a 30-line D3D12 test that only
clears rectangles (`tmp/steel-nomad-20260909/d12pat.cpp`). The test's own readback
of its back buffer is pixel-exact, so the app renders correctly; the buffer is
misread when DWM opens it as a D3D11 shared surface. The stripe geometry is exact:
128-px source bars become 12.8-row stripes, i.e. a 64 KB-tiled image read as linear
rows of 5120 bytes. Time Spy is affected the same way on AMD; on the owner's NVIDIA
host none of this shows.

**Cause.** UMD12's fused `pfnCreateHeapAndResource` arm forwards a swapchain buffer
as an explicit vkd3d heap (`VKD3D_HEAP_FLAG_HELIOS_VENUS_EXPORT`) plus a texture
placed at offset zero. `d3d12_heap_init()` allocated that heap's memory at
CreateHeap time - before any image existed - as a plain exportable, buffer-backed
allocation (API dump of the live path: `vkAllocateMemory` with
`VkMemoryAllocateFlagsInfo` + `VkExportMemoryAllocateInfo`, no
`VkMemoryDedicatedAllocateInfo`, then `vkBindBufferMemory2`, then the image). RADV
only records an image's tiling metadata on exported memory when that memory is a
dedicated allocation of the image (`radv_GetMemoryFdKHR` ->
`radv_image_bo_set_metadata`), and DWM's DXVK import is a dedicated import that
re-derives its image layout from that metadata, falling back to LINEAR when the
metadata is absent (`radv_patch_surface_from_metadata`). NVIDIA's layout is a
function of the create parameters alone, which is why the buffer-backed export
was never noticed. Reproduced in isolation by `tmp/steel-nomad-20260909/vkshare.cpp`
(in-guest Vulkan: export/import round trip is correct for every dedicated-image
variant and wrong only when the exported memory has no image attached).

**Fix (vkd3d fork, `libs/vkd3d/{heap.c,resource.c,vkd3d_private.h}`):** an export
heap no longer allocates in `d3d12_heap_init()`; it is marked pending and
`d3d12_resource_create_placed()` materialises it at the first placement through
`d3d12_heap_helios_allocate_pending()`. A texture placed at offset zero on a
GPU-local heap makes the exported memory a `VkMemoryDedicatedAllocateInfo`
allocation of that image, sized exactly to the image (VUID 02964); buffers,
non-zero offsets and CPU-accessible heaps materialise the previous plain
exportable heap. The committed fallback for memory-less heaps is preserved. The
D3D11 side, the ICD and the KMD are unchanged. Packaged as **22.22.271.0**.

## D3D12 default and Windows CI, 2026-09-07

Hosted run `34055565048` built the driver and both UMDs successfully, but CLVK
failed to configure because the SDK extraction action omitted Vulkan headers
and the loader import library; final bundle assembly was skipped. The CI setup
now uses the official unattended copy-only installer with a complete-directory
cache and checks development files before CLVK starts. On `firstheberg2-win`,
SDK 1.4.350.0 download/install took 53.4 seconds; a second validation took 0.1
seconds. A CMake Vulkan discovery/compile/link probe passes, and a missing SDK
is rejected before existing source/build trees are removed. CLVK also configures
and builds against the new SDK in 133.5 seconds using a warm tree whose source
pin, LLVM dependency, and two clspv patches were verified. Hosted validation
of this installer change remains pending.

The full 22.22.270.0 package was also built and test-signed on `firstheberg2-win`
from `dcdb8b38`, using a separate checkout to preserve existing QA source edits.
DXVK, vkd3d, the KMD, both UMDs, both Mesa architectures, loaders and probes
were rebuilt; CLVK reused only its verified warm compiler tree. Driver INF and
UMD import/export checks, compatibility lifecycle tests, and all 35 package
manifest entries/signing-certificate checks passed. The package records actual
tool versions, including LLVM 22.1.8, Meson 1.12.0 and widl 11.12; this is a
build-box validation, not a new Helios GPU/runtime acceptance result.

The same package is installed in the local WinBoat guest as `oem21.inf`,
upgrading 22.22.259.0. After the owner-approved reboot, Windows reports the
active Helios driver as 22.22.270.0 with status OK, and package verification
passes. Interactive rendering acceptance remains pending a desktop login;
`quser` reports no logged-in user, so session-0 graphics probes were not run.
Payload verification and DriverStore hashes for the KMD and both UMDs pass,
the device reports Code 0, and UMD12 occupies `UserModeDriverName[3]` with no
`UmdD3D12` override. No Compose changes were needed. The prior installation and
exported driver are backed up under
`C:\ProgramData\HeliosDeployBackups\before-dx12-20260907`.
This upgrade exposed a prerequisite bug: bundled VC runtime 14.44.35211.0
rejects installed 14.51.36247.0 with error 1638. The installer now checks the
registered x64 runtime version and keeps an equal or newer installation;
the successful retry exercised this path. Guest installation logs are under
`C:\Users\Tibix\HeliosDX12-20260907`.

Local metadata refresh on 2026-09-12: after the owner-approved start/reboot,
22.22.271.0 (`6e8de383`) is active as `oem0.inf`, named Helios vGPU, provider
WinBoat, Code 0. Runtime registrations/hashes, five DriverStore files and 24
trusted package signatures pass. The already-installed Resolve ADL shim was
upgraded too. Removed unused `oem21.inf` and .259/.270 runtime leftovers
(157,032,640 bytes); post-cleanup verification passes. Rollback is saved under
`C:\ProgramData\HeliosDeployBackups\before-metadata-20260912`; local evidence is
`tmp/deploy/metadata-20260912`. No user is logged in, so interactive graphics
acceptance remains pending.

The owner requested default DX12 admission and a Windows CI bundle containing
the native D3D12 UMD. `UmdD3D12` now defaults ON; explicit DWORD `0` still
refuses admission, and installation preserves that override. CI initializes
both engine trees, builds their static archives and both release UMDs, signs
both before catalog generation, and records vkd3d provenance and tool versions.
LLVM **22.1.8** and Vulkan SDK **1.4.350.0** match the active VM engine builds;
see `WINDOWS_CI_PACKAGE.md` for the remaining runner/VM toolchain differences.

Validation in `tmp/ci-dx12-20260907/`: the isolated Windows driver build and INF
validation pass. Bundle assembly rejects a missing UMD12; hashes and catalog
membership pass for the KMD and both UMDs. That assembly test uses real driver
outputs and labelled inert fixtures for unrelated components, not a deployable
full-stack bundle. Separate processes loading the built DLL with a private
registry override reach argument validation for absent/one and return
`DXGI_ERROR_UNSUPPORTED` for zero. The newly compiled native D3D12 device smoke
passes on the existing enabled guest stack via an interactive scheduled task.

Hosted run [34055565048](https://github.com/winboat-org/helios/actions/runs/34055565048)
at the exact root checkpoint completed: KMD/native DX11/DX12 UMD and Mesa jobs
succeeded; CLVK failed at its build step, so final signing/bundle assembly was
skipped. Candidate `6344CB09…` includes the default-ON policy in its ProgramData
hotplug; this validation retains explicit `UmdD3D12=1` and does not repeat the
absent/zero policy checks. The signed package has not been updated. The original
validation preceded hosted CI. Existing performance/visual evidence below
belongs to the earlier deployed artifacts; broader ownership and failure-path
gaps in `docs/dx12/EXECUTION_SYNC.md` and `docs/HPS2_REFACTOR.md` remain open.

## Current baseline and next work, 2026-09-06

**The owner confirms that realtime Time Spy shadows are fixed on .266 and
observed approximately 100 FPS in their benchmark.** This supersedes the .265
20 FPS visual check, where low throughput could hide a race. The automated
74.26 FPS result below is a separate, instrumented GT1-only run; its settings
are not established as equivalent to the owner's run. Do not use 75 FPS as the
owner's baseline or attribute the difference to instrumentation without evidence.

The owner's accepted stack used KMD **22.22.266.0 / oem50.inf**, the updated Mesa ICD,
release UMD11/UMD12, `UmdD3D12=1`, `HELIOS_WSI_ASYNC_PRESENT=1` and the existing
`HELIOS_RETIRE_FEEDBACK` workaround with stock virglrenderer. Source/build and
four-case native ordering checks pass; broad sharing, unchanged SRV bindings,
rotation/resize, teardown and WSI stress remain separate acceptance work.

**Performance follow-up: DX12 first, Steel Nomad Vulkan as the control.**
The initial capacity-wake comparison improves Time Spy **112.16 → 137.72 FPS
(+22.79%)**, with Fire Strike **244.77 → 245.57 FPS (+0.33%, effectively flat)**.
On the final default-enabled .270 package after reboot, the first checks are
**118.75 FPS Time Spy (+5.87%) / 248.23 FPS Fire Strike (+1.41%)**. One same-build
Time Spy repeat reaches **136.25 FPS (+21.47%)**, reproducing the initial larger
DX12 gain later in the boot. Keep the lower early run and unresolved variability
explicit; this is not a minimum-gain guarantee.
No 10–20% gain in both APIs has been demonstrated. Earlier standard GT1 baselines
were **243.93 FPS Fire Strike / 112.78 FPS Time Spy**. A reviewed coherent-
cached feedback allocation experiment measured **203.73 FPS DX11 (-16.48%) /
111.50 FPS DX12 (-1.13%)**. It was reversed; the exact accepted ICD restored
Fire Strike to **244.43 FPS**. Do not repeat that allocation experiment or the
archived WS2 queue-depth/allocation-cache sweeps without new causal evidence.
The old 3.7 ms producer floor is historical, not an assumed current bound.

The subsequent native Time Spy CPU/queue profile identifies approximately
**eight graphics EXECUTEs and two compute EXECUTEs per frame**. In an 8.014 s
window, the graphics worker spends **5.003 s before Vulkan execution, including
4.805 s blocked**, 0.303 s in the Vulkan execution region and 1.712 s afterward.
Actual loaded-DLL disassembly and context-switch stacks locate the long waits
in the runtime-admission event. Required cross-queue dependencies have not yet
been separated from excess completion/admission delivery latency. Those waits
must remain; queue spans overlap and are not GPU hardware timings.

Raw PCs also prove **23.87% of process CPU samples spinning on the vkd3d
logger lock**, over the full 8.898 s CPU trace. A reviewed logger mutex change
preserved every diagnostic and removed that sampled body hotspot, but clean
Time Spy measured **113.56 → 111.94 FPS (-1.43%)**: no demonstrated FPS gain.
The small difference is not a statistically established regression. The
candidate was tested through .267/oem51.inf (version stamp only; executable
KMD sections unchanged), then reversed from source and deployment. The guest
was restored to **.266/oem50.inf** with the exact original release UMDs and ICD.
Final restoration GT1 checks completed at **249.11 FPS Fire Strike / 108.33 FPS
Time Spy**, with original DLLs verified. These are restoration results, not
gains; the Time Spy variation also precludes treating -1.43% as a proven
regression. Candidate moving-scene/shadow acceptance was not received from the
owner. Steel Nomad Vulkan again failed at swapchain acquisition on restored
.266 at that stage.

**Steel Nomad Vulkan repair landed in .268 and remains deployed on .270/oem53.inf.** The previous
32 ms consumer-copy timeout was incorrectly treated as device loss. The helper
now captures one exact DXVK submission and distinguishes pending/completed/error;
WSI waits in sleeping slices while retaining the consumer read and source image.
Two dry independent review rounds, the finite-work pending/reacquire probe and
the standard Vulkan benchmark pass: **93.228233 FPS / score 9322**, status 0,
archive/export, 4814 successful helper Presents. .268's KMD executable sections
and UMD12 were unchanged; .269 retains that UMD12 and adds capacity wakes below.
Owner moving-scene acceptance and broader failure/inline
WSI stress remain open; see HPS2_REFACTOR and the performance report.

Guest capture/interactive observer tasks caused benchmark-isolation concerns;
the owner did not interact with either workload. Captures now use host VNC;
profiling observers run through win MCP in session 0. Benchmarks alone run in
interactive scheduled tasks. Isolated matching GT1 baselines on .268 completed
at **244.769699 FPS Fire Strike / 112.164719 FPS Time Spy**. Two prior Fire
Strike attempts failed entering fullscreen and are excluded.

The measured improvement targets transport backpressure. In a one-second Time
Spy CSwitch slice, 14 graphics completion submits spent **92.846 ms** in the KMD
QueueFull retry sleep; a fresh isolated .268 profile corroborated **104.056 ms**
across 12 such delays. .269 adds a stable
adapter event notified by real descriptor/parked-capacity reclamation; every wake
retries the same protected enqueue. .269 was measured with `SubSpaceWake=1`;
.270 selects that measured default and is deployed as oem53.inf, Code 0, with
the override absent. It passed the Windows build, two dry finalization reviews
and all four native ordering cases. Both final per-API runs complete; the Vulkan
control also completes at **90.68 FPS / score 9068**, with 4721 successful helper
Presents. The same-build Time Spy repeat also completes; all final settings and
artifacts are verified, with no new wake errors.
`SubSpaceWake=0` remains the timed-polling disable.
`QSpOn` records the arm; healthy **QSpErr=0**. `QSpNtf`, `QSpWake` and `QSpTout`
are notification/wake/fallback counts, never completion or performance proof.
Two consecutive dry reviews, the Windows build, 213 existing logic tests and all
four native synchronization cases pass on .269. Both clean after benchmarks have
status 0, archive/export, matching completed workload settings (excluding run
identifiers, output paths and reboot-dependent LUIDs) and matching non-KMD
binaries. QSpErr remained 0. In separate profiles, graphics post-execution
waiting falls **0.807 → 0.122 ms per frame callback (-84.8%)**; exact completion
submit waits in the one-second stack slice fall **104.056 → 13.239 ms**. These
are CPU worker waits, not hardware GPU time. Host VNC confirms changing Time
Spy frames; owner shadow acceptance remains open. No queue capacity,
batching, wire/GPU retirement, ownership or consumer-release rule is relaxed.

The [performance report](docs/PERFORMANCE_FEEDBACK.md) records completed
comparisons, exact artifacts, raw-PC and clock-alignment evidence, rejected
patches, diagnostic limitations and the Vulkan-control acquisition repair.
**The clean Time Spy baseline emitted 77691 pending-allocator-reset
diagnostics**; fence-worker reference-release lag and premature pool reuse
remain unresolved. Reducing logger contention does not repair that lifetime
question. Keep it and the broader DX12 ownership/failure-path gaps explicit.

The owner explicitly requests a focused completed before/after benchmark per
API, without a complex interleaved A/B campaign. Repeat only to resolve a failure
or material uncertainty. Preserve visibly changing frames; the owner remains
the shadow oracle. Keep async WSI enabled, use the paired renderer, and retain
exact runtime admission, GPU completion, producer epochs and independent
consumer release. No gain is promised and broader DX12 gaps remain explicit.

## Branch state, 2026-09-05 — `wddm-dx12`

Development moved off `/home/rupansh/helios-vgpu` (KMD 22.22.501.0) to this tree,
`/home/rupansh/helios-vgpu-dx12`, branch `wddm-dx12`, KMD **22.22.257.0**. ⛔ The
abandoned tree is **not** a reference: the two have diverged in knob sets, launcher shape
and toolchain assumptions, so reading it produces confident wrong answers.

Landed with the move, all verified on hardware:

- **Toolchain floor: bindgen 0.72 everywhere**, because the installed libclang is 22.1.8
  and 0.70/0.71 bind the forward declaration instead of the definition (1-byte opaque
  structs *plus* the real layout assertion). `kmd_render` needs the wdk crates pinned to
  an upstream git rev to get there; see `AGENTS.md`. ⛔ Never answer this by disabling
  bindgen's layout tests — they are the only reason a 1-byte `_IRP` was a build failure
  rather than a running driver.
- **`HardwareInformation.AdapterString` is now set by the INF.** It was never written by
  this package, so the display class key retained `"Microsoft Basic Display Adapter"` from
  a previously bound driver. The ICD matches on `wcsstr(AdapterString, "Helios")` and only
  falls back to probing every adapter when the query *fails* — a query that *succeeds with
  a foreign name* silently removed the real adapter from the candidate set. Full chain:
  ICD probes only the two Microsoft Basic Render Drivers → their escapes correctly refuse
  with `0xc00000bb` → no `Virtio-GPU Venus` VkPhysicalDevice → DXVK `DxvkError` → D3D11
  `CreateDevice` E_FAIL → `dwm.exe` crash-loops on `0x889800b0` in `dwmcore.dll`. ⚠ The
  `0xc00000bb` never reached the KMD (`EscNoDev`/`EscCtxOwn` stayed absent) — it was
  dxgkrnl refusing an escape on foreign adapters, which is correct behaviour. Nothing in
  the symptom named the registry value.
- **The D3D12 UMD ships in the signed DriverStore package** and survives a cold boot
  (`UserModeDriverName[3]`). `install-helios-kmd.ps1` gained `-Umd12Dll`, and all three
  artifact paths lost their defaults: `cargo make` stages a *debug* `helios_umd12.dll` and
  only `helios_umd.dll` was ever refreshed, so the first install shipped a debug D3D12 UMD
  that nothing in any tool's output named.
- **D3D12 is ON in the test VM**: `HKLM\SOFTWARE\Helios!UmdD3D12 = 1`, with
  `OpenAdapter12=0` refusals and a real `CreateDevice` observed in `umd12-<pid>.log`.

## Stage pivot, 2026-08-05

The **Performance, Stability, Conformance (PSC)** stage is closed as a *stage*;
its stability contracts remain permanently in force and its performance record
is kept below as WS2 — read it before opening any new perf work, because it is
mostly a list of things that have already been tried and measured.

**Why now.** The present-queue stall was root-caused and fixed (WS2, `PresentWmk`,
KMD 22.22.244.0), and the remaining limit is named rather than suspected: the WDDM
FIFO head now blocks on `stream_ready` — the frame's own producer completion on
the host — at `WfBStrm`/`WfBWire` ≈ 15220/161, against a render-thread producer
floor of ~3.7 ms/frame. There is no further sweep to run; the next perf gain needs
a new causal hypothesis, not another arm.

**The new order of business:**

1. **D3D11 correctness / conformance** — charter in `CONFORMANCE.md`, plan in WS3.
2. **D3D12** — charter in `DX12.md`, detail in `docs/dx12/`. **The strategy question is
   CLOSED as of 2026-08-05**: Helios ships a real D3D12 UMD, `helios_umd12.dll`,
   implementing `d3d12umddi` and forwarding into vkd3d-proton's `ID3D12*` COM
   objects — the D3D11 architecture with DXVK swapped for vkd3d and
   `UserModeDriverName[2]` swapped for `[3]`. The app-local vkd3d arm is Phase 0
   of that plan, not an alternative: it proves the whole lower half (vkd3d +
   dxil-spirv + venus + KMD + present) with zero Helios code. Decisions and the
   twelve-lane evidence merge: `docs/dx12/DECISIONS.md`. Checkpoints:
   `docs/dx12/GATES.md` (`D12-G0 … D12-G11`). ⭐ **S5 has since LANDED** (2026-08-06, cold-boot half
   2026-09-05): the INF registers `UserModeDriverName[3]`, `umd`'s duplicate
   `OpenAdapter12` export is gone, and `adapter12::OpenAdapter12`'s body is reachable
   behind the `UmdD3D12` kill switch. **Source default is ON as of the owner's
   2026-09-07 direction**; explicit DWORD 0 disables it for new processes.
   The enabled .270 runtime evidence is above; this default change does not
   establish broader ownership/failure-path or new owner visual acceptance.
   *Measured up front:* the guest satisfies vkd3d-proton's
   `VP_D3D12_FL_12_2_baseline` in full (zero feature/extension misses), and the
   KMD work list is empty for Phase 0 / three small items for the DDI arm.
3. **Stability** — WS1, unchanged and non-negotiable.
4. **Performance** — WS2, PAUSED. Do not reopen without a new hypothesis.

**Also landed with the pivot (2026-08-05), because a stage change is the right
time to stop shipping something nobody measured:**

- **Sane values are now the defaults.** Three knobs whose code default was OFF
  had been ON in the test VM's registry since 2026-08-03, so every accepted
  score was measured on a configuration no fresh install produced. A fresh
  install got the runtime's *emulated* command-list path — GT1 ≈ 184,
  Graphics ≈ 43.5k — instead of the measured GT1 221-227 / Graphics 49-52k.
  Flipped to ON, each with the evidence in the comment at its read site:
  `HELIOS_DXVK_CL_RETAIN_SAMPLER_REFS` (isolated same-boot A/B, GT1
  **53.609 → 181.938**), `UmdCommandLists`, `HELIOS_DXVK_CL_INLINE_REPLAY`.
  `VidMmVramMB` likewise went 0 → 4096, the configuration the VidMm work
  actually validated, re-confirmed on 22.22.251.0 before the flip.
  `HELIOS_DXVK_KMT_SHARED` was forced to "1" by the UMD in every process it
  ever created, so it was not a tunable at all; the engine now defaults it ON
  and the `_putenv_s` is gone. **Verified**: with `HKLM\SOFTWARE\Helios`
  completely empty and no service-key overrides, KMD 22.22.252.0 runs GT1
  **222.857**.
- **Retired**: the `probe/` and `host/` crates (orphans — no workspace, no CI,
  no build, cited only by already-archived docs); the write-only
  `TransportGeneration::page_table_window` the tree itself scheduled for
  deletion at R510; the duplicate unread `AdapterKnobs::dma_gpu_fence`;
  `tools/kmd-force-reject-sweep.ps1` (its knob was retired in T6),
  `tools/attach_idd.ps1` (IddCx-only), and the two completed one-shot DXVK
  source patchers.
- **Gates that could only pass are gone or fixed.** `kmd-gate-surface.ps1` and
  `kmd-counter-snapshot.ps1` were watching four counter/knob names the driver
  no longer writes; `umd-gate-surface.ps1` had three log patterns that could
  never match the emitted text. A gate that cannot fail is worse than no gate.
- **Four silent failure counters were surfaced** as `WdSigF` / `DmaNtfF` /
  `TxGone` / `RclBadH`. Each was incremented on a real refusal path and loaded
  by nobody, which is AGENTS.md's "every refused path gets a named counter"
  rule being violated invisibly. **All four must read 0 on a healthy session.**
- **Docs archived**: `ARCH.md`, `OVERVIEW.md`, `KMD.md`, `ICD.md`,
  `WINDOWED_BLT_DESIGN.md`, `SCANOUT_DRM_MODIFIER_DESIGN.md` → `docs/archive/`.
  `TRANSPORT.md` deliberately stayed at root: its §1/§2 wire format is still
  ground truth and six `protocol/` comments cite it by section; its banner now
  says which sections are live and which are archived.
- **One real bug fell out of the audit**: `tools/escape_owner_probe.c` defined
  `HELIOS_ESCAPE_QUERY_SCANOUT` as `0x000B`, which is
  `HELIOS_ESCAPE_REGISTER_FENCE_EVENT`. The probe had been aiming a
  query-scanout buffer at the fence-event registrar. Fixed to `0x000D`;
  every other escape constant in that file was checked against
  `protocol/src/escape.rs` and is correct.

## Current verified correction (2026-08-04, KMD 22.22.238.0)

- **Fullscreen presentation is not currently a broken SDL scanout path.** The
  owner corrected the viewer identity after the `.238` visible test: the
  hold/judder that looked like roughly 30–40 fps was observed through **VNC**,
  not SDL. Native QEMU SDL is owner-verified rock solid and smooth, and the
  tearing is gone. Treat the earlier claim that SDL independently reproduced
  the hold/burst defect as retracted. A VNC cadence observation is evidence
  about VNC update/encoding/client delivery only; it must not be used to blame
  KMD scanout, QEMU readback, or the D3D11 render path without a correlated
  boundary trace. Smooth SDL means smooth at the display refresh ceiling, not
  that all 150–220 rendered frames per second can be shown on a 60 Hz output.
- `.238` replaced the coarse fallback VSync timer with a high-resolution
  `ExAllocateTimer(EX_TIMER_HIGH_RESOLUTION)` source. In the targeted Combined
  trace its active VSync samples were stable at about 16.6 ms (p95 about
  17.1 ms, no gaps over 40 ms), and the owner now sees no tearing. This closes
  the fullscreen tearing/cadence symptom for SDL; VNC fluidity remains a
  separate frontend/client concern and is not a blocker for D3D11 throughput
  work.
- **Windowed 3D11 presentation remains open and is a different defect.** In
  the interactive standard Fire Strike flow, a blank titled `3DMark Workload`
  window appears and then disappears while 3DMark continues the workload and
  ultimately reports a score. The scheduled custom `FireStrikeCombinedC`
  window trace (`tmp/cadence-238-window-blt-accept.csv`) rendered successfully,
  but it does **not** validate this interactive path. Instrument the actual
  runtime entry point (ordinary Present, single/multi-surface Present1, or MPO)
  and its exact handles/allocations before changing policy. In particular,
  current `dxgi_present1` many-surface code deliberately passes no snapshot or
  stream correlation; that is a source-backed lead, not yet the proven cause.
- **The remaining Fire Strike performance gap is not a scanout-cadence
  diagnosis.** The current multithreaded command-list path recorded GT1
  221.337, GT2 220.996, Physics 125.986, and Combined 41.952 fps in
  `tmp/perf/fs-std.txt`; a later targeted Combined run reached 43.593 fps.
  Nevertheless, the owner observes only roughly 50–60% host-GPU utilization
  in Fire Strike/DX11, versus a sustained roughly 80–90% in Steel Nomad's
  Vulkan path. Use that differential to find where the D3D11-specific
  runtime/UMD/DXVK command-production pipeline fails to keep the GPU fed.
  Steel Nomad exonerates generic Vulkan throughput, but not D3D11 per-draw,
  command-list, synchronization, or submission economics. Do not spend the
  next performance session tuning scanout unless an epoch-correlated trace
  actually shows scanout back-pressure reaching rendering.

## Current priorities

1. **DONE (2026-07-28) — the Phase-1 quality refactor of `kmd_render` and
   `umd` is COMPLETE.** Eleven tranches (T0, T1a, T1b, T2, T3, T4a, R614, T4b,
   T5, T6, T7, T8) from `REFACTOR_REVIEW.md`'s 300 findings / 177
   recommendations, every one landed and gated on hardware. Final image:
   **KMD 22.22.190.0 + UMD `DB343F02…`**, T8 gate passed on the 2026-07-28
   15:39:45 cold boot.

   **The tranche-by-tranche record — every gate result, every scope
   correction, every dropped item and its evidence — is
   `docs/archive/REFACTOR_TRANCHES_T0_T8.md`.** The review itself, its two
   kickoff prompts and the T7-crash brief are archived beside it. Code
   comments cite the review by NAME (`REFACTOR_REVIEW.md R802`); those
   citations still resolve, the same convention the other archived design docs
   use.

   Two directives from that work stay in force for all later changes: never
   fold a `BUG` fix into a structure move, and preserve the direct primary,
   completion ordering, loud-failure contracts, registry ABI and diagnostic
   names unless a reviewed change explicitly migrates them.

   **Owed, recorded with the measurements that justify deferring them** (see
   7m/7n in the archived record):
   - **R1103's `VirtioGpu` sub-structs.** `ResourceTables` is genuinely
     field-disjoint; `CtrlQueue`+`FenceTables` needs **six** method hoists on
     the completion path, not the three the review budgeted. Needs its own
     tranche and gate.
   - **R1108's vehicle-TLS sealing** — `take_present_source()` plus the four
     `dxgi_present` call sites that touch the cell.
   - **R1015** — whether the production surface ever takes the
     QUERYSEGMENT3/legacy paths. Needs a `DiagLevel=1` boot.
   - ~~The pre-existing **6-handles-per-device teardown leak** (7d(b))~~ —
     **CLOSED 2026-07-28**, root-caused and fixed. See the WS1 entry below.
   - **WS1 defect 0z** — `pnputil /restart-device` access-violates dwm,
     Explorer, SearchHost and ApplicationFrameHost inside
     `vulkan_virtio-*.dll`. Pre-existing, reproduced on every restart.
   - ~~**WS1 defect 0aa** — fullscreen scan-out pinned to ONE resource~~ —
     **ROOT-CAUSED AND FIXED 2026-07-29** (KMD 22.22.201.0), host-verified.
   - **WS1 defect 0ab — black-frame flashes. SPLIT IN TWO 2026-07-29, one half
     FIXED, one half OPEN.** First measured directly on the displayed surface
     (VNC RFB sampler + QEMU trace, both on the host clock) instead of inferred.
     - **0ab-A — the bind-edge RESOURCE_FLUSH was submission-ordered**, firing
       ~10 ms before the frame it named finished on the host, so the host read
       the frame's clear. **FIXED, KMD 22.22.206.0**: Fire Strike Combined
       (23 fps) unfinished displayed frames **22.0 % → 0.7 %**.
     - **0ab-B — at ~165 fps (GT1 fullscreen) the flashes REMAIN**: ~15 % of
       published frames are entirely black in EVERY configuration we own. Five
       mechanisms built, deployed, falsified; then a same-boot **2×2 factorial**
       (lease × BindFlushMode, 9 runs, 46 681 frames, 2026-07-29 evening) closed
       the whole ordering family WITH data: whole-flush black is 14.5–16.6 % in
       all four cells, and the knobs only move black between populations
       (bind-triggered first reads vs surplus refresh re-reads). **The mechanism
       is now PROVEN, not inferred**: the first read of a binding — the very
       event that ends its lease — finds the buffer already cleared 13–17 % of
       the time under a live lease gate, which no WDDM release chain can permit.
       The app's clear rides venus and never enters a DMA buffer, so the
       scheduler-side allocation sync that real flip-model relies on to defer it
       DOES NOT EXIST in this stack. The one variable that predicts black is
       bind→read age (<3 ms ⇒ 0.4–5.6 %; 6–12 ms ⇒ 34–60 %).
       **FIX SHIPPED — KMD 22.22.217.0 (owner-approved D1+D2+D3, 2026-07-29
       late evening): GT1 whole-flush black 14.5–16.6 % → 2.1 / 0.7 / 2.0 %**
       (age-standardised 2.2/0.9/2.0 — not an age-mix artifact), fps 169–186
       (UP: 25–33 % fewer synchronous host readbacks), Combined 0ab-A gate
       PASS (1.3 %, completion ordering intact), desktop 1:1 binds:flushes,
       Start menu opens, windowed-app coexistence verified, `WvTorn` 0.
       The win is the OWNERSHIP GATE (D2): the 34–49 %-black 2nd-read
       population (1090–1669/run) collapsed to 9–26; the 6–12 ms bucket kept
       its flush share but went 56 % → 0.5 % black — the wrong reads stopped
       being issued, not the timing. See the build-1 subsection below +
       `tmp/handoff-0ab-b-lease/analysis/build1-results.md`.
       **OWNER-CONFIRMED BY EYE 2026-07-29 late night: GT1 visually clean,
       overall Fire Strike >25k (was ~20k). 0ab-B's main population is
       CLOSED.**
     - **0ab-C — residual black-frame stuttering in GRAPHICS TEST 2 at
       ~210 fps. CLASSIFIED 2026-07-29/30: the first-publish bind-edge margin
       race (population (a)), the exact population build 1 left open.** Two
       oracle GT2 runs on .217: whole-flush black 7.3 %/6.0 % (GT1 post-fix
       0.7–2.1 %), all first reads at 1–3 ms bind age; the ownership gate
       holds unchanged (6–12 ms bucket 0.2–0.4 %, rereads ~1 %). Guest half:
       worker bind cadence bimodal (1–3 ms vs 10–14 ms stall modes),
       `BeOvw` ×~30 GT1's rate. Minorities: 0ad's transition window
       (~12–23 %), coalesce-holds (dup 3–5 %). **Fix arc = the D1(ii)
       DISPATCH-bind family, four builds in one night**: .218 bugchecked (a
       PRE-EXISTING `wait_block` TOCTOU the new load armed — root-caused
       from dumps, fixed in .219, three clean batteries since); .219 halved
       GT2 black (4.0/3.4 %); .220/.221 closed the fast-path coverage gap to
       99 % and thereby PROVED the GT2 residual is not bind timing (x = y;
       0/439 black at 0–1 ms bind age — the venus-executed clear lands in
       the READ window). **GT1's residual was eliminated outright
       (1.9 → 0.3 %, best recorded). SHIPPING: 22.22.221.0. GT2 residual
       ~3.5–4 % needs D4 (venus acquire, owner-gated). 0ab-C = reduced, not
       closed; owner's eye pending.** Corpus:
       `tmp/handoff-0ab-c-gt2/analysis/{CLASSIFICATION,FIX-DESIGN-d1ii,BUGCHECK-0xA-218,build219-results,build220-results,build221-results}.md`.

   ⚠ **One standing gate line remains NOT OBTAINABLE on this box** and should
   not be retried as written: **suspend/resume** (`powercfg /a` reports every
   sleep state unsupported by the VM firmware — which also means the
   same-context PnP stop/start carry-over path, `StRst`/`RfUnb`, can never be
   provoked here). The other one — **same-boot QEMU scanout evidence** — is
   RESOLVED: since 2026-07-29 the VM runs `HELIOS_DISPLAY=egl-vnc` and the
   per-flush oracle (`tools/qmp_trace.py` + `tools/scanout_oracle_report.py`)
   provides it routinely; verify with `/proc/<qemu>/cmdline` before relying
   on it.


2. Continue soaking the current direct-primary path across DWM buffer rotation,
   resize, device restart and cold boot. **Suspend/resume is struck from this
   list**: `powercfg /a` on this VM reports S1, S2, S3, hibernate and S0ix all
   unsupported by the firmware, so it is untestable here until the machine type
   changes — and with it, the same-context PnP stop/start carry-over path
   (`StRst`, `RfUnb`) has no way to be provoked on this box at all.
3. Pursue true host zero-copy only with a layout contract the display importer
   can consume. An explicit DRM modifier is one possible route, but enabling the
   modifier/DMA_BUF extensions on every DXVK device is prohibited: it inflated
   ordinary shared OPTIMAL import requirements and caused valid undersized-import
   refusal, DWM failures, and NVIDIA Xid 31 when bypassed.
4. Continue D3D11 stability and conformance work now that the quality pass is done.

## Open defects and per-workstream status

⭐ Each workstream keeps its framing verbatim below. **The full dated record — every
measurement, every falsified hypothesis, every rejected lever with its numbers — is in
`docs/archive/ROADMAP_HISTORY_THROUGH_2026-09-05.md` under the same headings.** Read it
before reopening any of these; most of what looks unexplored has already been measured.

### Fullscreen scan-out — 0ab-B STILL OPEN

`0aa` FIXED (2026-07-29). `0ab-A` FIXED. **`0ab-B` — at ~180 fps the flashes REMAIN —
is OPEN**, and `0ab-C` (the first-publish margin race at GT2's operating point) is
CLASSIFIED with its population split still partly open. ~970 lines of evidence for these,
including the wedge experiments, are in the archive under
"Fullscreen scan-out — 0aa FIXED, 0ab STILL OPEN".

**Windowed D3D11 presentation remains open and is a different defect** — see "Current
verified correction" above.

## Workstream 1 — Stability

**2026-09-08 — WinBoat Blender / Mesa buffer-map failure (open).** The installed
`.270/dcdb8b38` bundle's `libgallium_wgl.dll` COFF symbols resolve Blender 5.2's
recorded write to address `0x143` to `tc_buffer_map+0x23c`, not the nearest
export (`stw_unbind_context`) printed by Blender's crash reporter. That
instruction writes through an unchanged transfer pointer after the driver's
`buffer_map` call. Gallium explicitly permits a failed map to return NULL
without changing the transfer output (`docs/gallium/context.rst`, Transfers).
The Mesa fix pinned by the submodule checks the return before initializing the transfer;
it also frees incomplete CPU shadow storage and returns failure if the initial
GPU-to-CPU copy cannot be mapped. No map failure is reported as success.
`CC=clang python tools/test_tc_buffer_map.py` runs the actual function body
against a fake pipe driver under ASan/UBSan: all seven cases pass, including
failure cleanup/retry and synchronized/unsynchronized success. The same test
with `--revision a04516a702dff81d3a2e44019cdd79abf3fb7423` crashes in all four failure cases and passes the three
success cases. This harness does not validate the Windows ABI or driver stack.
The source fix is **not deployed**. The original map failure's cause and the
stalled RDP session's relationship to it remain unproven; both factory-startup
and normal-argument Blender reached their viewports under CDB after the
owner-authorized VM restart without triggering the first-chance AV handler.

**IDD frame freeze: DIAGNOSED 2026-07-05 (17th session), live on the frozen boot** — full chain
in memory `idd-freeze-root-cause-chain`. Summary: (1) routine multi-second completion stalls
(per-present full-GPU drain in `rotate_resource_backings` + event-cadence desktop) →
(2) the 4×8 s sem-deadline latch declares CONTEXT LOST on a healthy-but-slow stack →
(3) dxvk teardown on DEVICE_LOST resets command pools with host work pending (= the
`vkResetCommandPool` VUs; symptom, not cause) → (4) post-loss, `submitCmdLists` drops cmdlists
WITHOUT `notifyObjects()` → in-use refs leak → next `Map` → `waitForResource` (no timeout, no
lost-check) wedges dwm permanently; win32k session-1 GDI hangs behind it. Falsified: the
early-fence/helios_sync theory for the steady-state stall — 0 of 251,810 submissions carry
ring≠0; the vn win32-sync signal path never fires; cross-process sync is dxvk-helios-internal.
**Status 2026-07-06 (18th session): the whole chain is now closed** — (1) the "stall" was the
sem-deadline misreading idle wait-before-signal waits (fixed, defect 1 below) plus the rotate
drain (fixed, WS2); (2) the latch no longer fires on idle desktops; (3)+(4) fixed 17th session.
Remaining: cold-boot + multi-hour soak, and the forced-loss test for the loss path (defect 2).


⇒ **The open defect list (0w … and the numbered stability defects, several still open —
notably the dxgkrnl "invalid NTSTATUS 0xC00000BB" entry, the WUDFRd cold-boot race, and
in-place KMD update flakiness) is carried in full in the archive under "Workstream 1 —
Stability".** Its contracts remain permanently in force regardless of stage.

## Workstream 2 — Performance

- **THE PRESENT BLOCK IS ATTRIBUTED AND HALVED (2026-08-04/05, KMD
  22.22.243.0 → 22.22.244.0).** `umd_present_callback` (548–661 µs/frame, the
  single largest ours-attributable cost on the app's render thread) is not CPU
  and not our `DxgkDdiPresent` (7.9 µs mean): it is **one dxgkrnl wait**.
  A `Microsoft-Windows-DxgKrnl` ETW slice names it — `BlockThread` `Reason=2`
  on 21.1 % of presents, mean 2448 µs, **516 µs amortised = the whole callback**
  — and pins the mechanism exactly: **89 of 91 blocks began with exactly 3
  `PresentQueuePacket`s outstanding** (non-blocked presents saw 0/1/2) and
  **90 of 91 unblocks landed within 200 µs of a `PresentQueuePacket Stop`**
  (median 12.1 µs). dxgkrnl allows three outstanding present packets; the
  fourth present blocks until one retires.
  **Why the queue filled was OUR defect:** `note_wddm_submission` gated every
  non-paging WDDM fence on `async_retired_up_to(next_wire_fence, IncludingGpu)`

⇒ **The historical sweep is closed; bounded investigation resumes on 2026-09-06
under the current task above.** Do not open a perf sweep without a new causal hypothesis: the
archive's "Workstream 2 — Performance" is ~980 lines that are mostly a list of levers
already tried, measured and rejected, with numbers. Its measured limit was the
frame's own producer completion on the host, at ~3.7 ms/frame. Re-establish the
bottleneck on the repaired stack before selecting a new mechanism.

## Workstream 3 — D3D11 Conformance  ← **PRIORITY 1 since 2026-08-05**

**The charter is `CONFORMANCE.md`** — what "conformant" means for this stack,
the refusal/no-op counter surface and how to read it, the ~40 `tools/` probes
catalogued into a suite, the open gaps, and how to add a test. Everything below
this line in WS3 is the session-by-session record that produced it; read the
charter first.

### Open items carried into the new stage

1. **The `DDI refusals:` counters must reach 0 against real workloads.** Two are
   known to move under 3DMark and each names a real gap:
   `gs_so_declaration_dropped` and `tess_sig_fallback`. Definition of done is a
   3DMark standard run plus a desktop session with every counter at 0, read
   through `tools/umd-gate-surface.ps1`.
   ⚠ Two corrections found while writing `CONFORMANCE.md`: the line carries
   **eleven** counters, not the nine this document used to claim (R1010 added
   `alloc_meta_format_unknown` and `readback_stride_unsafe`) — and **the
   noop-DDI hit counter, which AGENTS.md names as the headline WS3 metric, is
   currently unreadable**: `DEVICE_NOOP_LOG_COUNT` is incremented and loaded by
   nobody, with no summary line and no gate pattern. Making it readable is
   backlog item C1 in `CONFORMANCE.md` and is a prerequisite for the rest of
   this item.
2. **3DMark Fire Strike reports `103 Display Mode List not found for given
   format` and `402`** on a failed 2026-07-24 run
   (`3DMark-Firestrike-FAILED-20260724221433.3dmark-result`). This was sitting
   in a scratch file at the repo root rather than in the roadmap; it is a real
   DXGI mode-enumeration conformance datapoint and belongs to this workstream.
   Not reproduced since — first job is to establish whether it still occurs.
3. **DXGI format coverage audit** — the format round-trip carrier landed; the
   coverage matrix does not exist.
4. **Remaining 11.1 DDI plumbing.** The threading/command-list surface is now
   real and on by default (see the 2026-08-05 stage-pivot note), which changes
   what "remaining" means — re-survey before planning.
5. **FL11 MSAA** — status recorded below as PARTIAL with un-deployed WIP
   (`ff14979`). Verify whether that is still true before treating it as open.
6. ✅ **RESOLVED 2026-08-06 — `kmd_render` had five `#[test]` functions that could never run**
   (`present_stream_tests` in `src/virtio/gpu/mod.rs`): the crate is a
   `panic=abort` no_std cdylib and cannot host a libtest harness, and CI runs no
   `cargo test` at all. They were assurance that is not real. They and the pure
   helpers they cover were moved into `kmd_logic` as
   `helios_kmd_logic::present_stream_boundary_tests`, beside the
   `present_stream` module; `grep -c 'cfg(test)'` over `kmd_render/src` is now
   **0**, and the only trace left in `gpu/mod.rs` is the note recording the move
   (*"Do not reintroduce tests in this file"*). ⚠ **The count is five, and three
   places disagree about it** — `git show 3e750c0:…/gpu/mod.rs` counts **5**
   `#[test]`s, which is what this item and `gpu/mod.rs`'s note say and what
   `PENDING.md`'s wave-1 correction #5 established against `PENDING.md` §6's
   "six". But `present_stream_boundary_tests` now holds **six**: the move
   recovered five and **added one** (`slot_63_and_new_generation_never_alias`),
   which is why `kmd_logic`'s own doc comment says *"These six tests lived in
   `kmd_render`"* — that sentence is wrong about provenance, not about arithmetic.

## Workstream 4 — D3D12  ← **PRIORITY 2 since 2026-08-05**

**HPS2 removal investigation, 2026-09-05:**
[`docs/HPS2_REFACTOR.md`](docs/HPS2_REFACTOR.md) inventories the live file users
and recommends allocation-bound KMD completion state, cached read-only status
and event waits on WDDM 2.1, plus explicit WSI dependencies and a narrow DX12
worker-queue hook. Approximately one implementation day is a planning target;
runtime/performance validation may extend it. The broader queue-admission
redesign was outside the original scope. The proposal preceded the implementation
and owner-directed execution repair recorded below.

**Bounded HPS2 implementation follow-up (source/build validated):** the vertical
replacement described in that investigation is now implemented in this checkout.
Exact dxgkrnl allocation/open references bind one KMD producer state; ABI v1
publishes resource epochs against registered stream boundaries and exposes cached
read-only status and cancellable event waits. DXVK retains dependencies through
submission and stamps the consumed refresh epoch. UMD12 uses the vkd3d callback
FIFO for its exact queue/resource signal and HEPR correlation. WSI carries an
unnamed NT semaphore/value through the helper seam and retains its separate copy
completion/recycle guard. Both HPS2 implementations/callers and installer ACL
setup are removed. WDDM stays `Wddm2_1GpuMmu`.

`kmd_logic` passes 198 tests (20 new producer state/reference/wait tests); protocol
passes 14. The initial cutover passed Windows release builds for KMD, UMD11,
UMD12, DXVK, Mesa and vkd3d. The `.264` correction also passed normal and release
KMD package builds and the complete normal-profile stack gate (4936 bytes). One
complete-change review repaired dropped-predecessor error handling and WSI
fallback/failed-copy lifetime paths. Runtime acceptance remains pending: mixed
API/cross-process sharing, unchanged bindings, delayed/reordered producer work,
rotation, resize, teardown/cancellation and async WSI. The owner initially reported
an approximately 10% performance regression and paused performance work for DX12
correctness. The .266 shadow acceptance and next task above supersede that pause. Keep
`HELIOS_WSI_ASYNC_PRESENT=1`; the inline path is outside this acceptance work.
The required stimuli and pass evidence are in
[`HPS2_REFACTOR.md`](docs/HPS2_REFACTOR.md#runtime-acceptance-packet--pending).
The original hook did not close general DX12 ECL/fence/wait coverage. Its
HE12 v2 successor is described below. Missing external queue-family ownership
transfers remain an obstruction to general mixed-API runtime correctness.

**.265 synchronization checkpoint, superseded by .266 acceptance below:**
The owner saw no realtime Time Spy shadow/black-flash defect on .265 at about
20 FPS, and explicitly cautioned that low throughput could conceal a remaining
race. That observation alone did not confirm a visual fix. The subsequent .266
work recovered throughput while retaining the demonstrated wait/signal guarantees.
The original Time Spy shadows disagreed with the current frame; Steel Nomad DX12
was unaffected. This is distinct from whole-frame presentation order. UL documents Time
Spy overlapping light culling, SSAO and unshadowed illumination with shadow
rendering, plus render-target heap aliasing. Steel Nomad also uses async compute,
for its first volume-illumination pass, and has a different contact-shadow path.
Sources: [Time Spy engine](https://support.benchmarks.ul.com/support/solutions/articles/44002136148-time-spy-engine),
[Steel Nomad engine](https://support.benchmarks.ul.com/support/solutions/articles/44002528067-steel-nomad-engine).
HE12 v2 replaces the undrained sampled ECL boundary with an authenticated
registered worker-stream value and uses the exact runtime context's
`SignalAtSubmission | EnqueueCpuEvent` admission before executing work. Present
callbacks receive the same pair, covering Queue::Wait -> Present without ECL.
The separate KMD private tail preserves batched predecessors and preemption
replay; stream teardown, timed rebasing and FIFO overflow cannot fake execution
completion. The private engine fence/shadow watermark and sample/drain switches
are removed. Nonzero monitored-fence GPU placements and direct D3D12 queue fence
DDIs are currently refused; zero-VA software fences remain runtime-owned.
These older paths must not be conflated with optional WDDM 3.2 native GPU fence
objects. The latter are outside the WDDM 2.1 contract and are not a blocker.

The .265 candidate passes 206 KMD logic tests, 14 protocol tests, UMD12 host
Clippy `-D warnings`, Windows vkd3d, release UMD11/UMD12 and normal KMD package
builds. Its stack gate remains 4936/17936 bytes. The native cross-queue/CPU/
shared-fence readback probe builds with `/W4 /WX`. On deployed .264 it now
reproduces early completion: a signaled event precedes the required GPU readback
bytes (word 0 is zero, expected `3c6ef372`). The recent Time Spy log additionally
has 2956 allocator resets with command lists awaiting execution. **Independent
whole-change review resumed and completed; it found and verified a repair for
premature cancellation during normal queue teardown.** The repaired .265 is
deployed as oem49.inf with the explicit release UMDs. The native Windows runtime
suite now passes all four queue/CPU/shared-fence cases, with exact data in both
producer and consumer readbacks and no writes through the deliberately blocked
waits. Evidence: `tmp/dx12-sync-265-runtime/20260906-040507-991/`.
Time Spy's visual acceptance remained open at this checkpoint. The native probe
establishes its exercised ordering cases, not the shadow defect's cause or
correctness at higher throughput; the owner subsequently accepted .266 shadows.
The exact contract, monitored-fence routing and
external-ownership gaps, and scheduled-task acceptance commands are in
[`EXECUTION_SYNC.md`](docs/dx12/EXECUTION_SYNC.md). The following investigation
addressed the .265 execution/admission regression with async WSI enabled.

**Completion delay isolated; reuse shipped feedback workaround, 2026-09-06:**
Two GT1-only .265 runs completed at 19.02 and 20.03 FPS. The aligned render-phase
ETW slice measured direct/compute DMA medians of 10.19/10.58 ms and about 68 ms
median queue-to-admission delay. Live QEMU debugging verified async context-fence
callbacks were enabled; callback-to-dispatch averaged 0.410 ms, while the delay
occurred before the callback. A native NVIDIA 610.57.04 GPU-fill plus empty-marker
reproduction measured the full work-submit / marker-submit / wait sequence at
8.060 ms average with `SYNC_FD`-exportable fences and `vkWaitForFences`, 0.329 ms
with ordinary fences, and 0.220 ms waiting on the exported fd.
Bare empty submissions were insufficient to reproduce the wait cost.

**Historical owner constraint, withdrawn 2026-09-09: stock virglrenderer.** The unaccepted private
server patch/build helper and launcher override have been withdrawn; the
candidate was never activated. Evidence remains in `tmp/dx12-sync-265-perf/`,
with the withdrawn proposal under `withdrawn-virglrenderer-candidate/`.

The [archived WS2 workaround](docs/archive/ROADMAP_HISTORY_THROUGH_2026-09-05.md)
(lines 3096–3127) is `HELIOS_RETIRE_FEEDBACK`, default on: the ICD observes the
exported semaphore's GPU-written feedback counter instead of waiting for the
slow wire response. Historical retirement was 5.6–9.2 ms before and 0.25–0.33 ms
after. The .270 deployed code implements it; the .271 candidate removes it. The same .265 Time Spy capture
reports `retire_fb fast=4607 fallback=0 wire=0`; it is not a missing environment
toggle. On .265 that observation advanced the ICD sync and its WDDM external
fence only, leaving KMD allocation producer state and HE12 execution completion
on the slow tagged AsyncVenus response.

**.266 deployed; throughput recovered and shadows accepted:** the
ICD retire worker sends an exact GPU feedback notification (escape 0x14). KMD
validates the owner/context/cookie/value and original admitted wire-fence receipt.
`execution_completion::Progress` separates GPU completion from wire retirement;
producer publications and HE12 submissions consult GPU progress, while transport,
Present readers and closing stream reclamation remain wire-owned. Feedback reads
hold the same mutex as detach/recycle. Registered private streams must start at
zero, with no prior signal/import, and refuse CPU signal or payload replacement.
A non-feedback queue permanently detaches the backend slot before CPU resync;
timeout/detach/refusal falls back to real wire completion. New telemetry is
`stream_fb accepted/wire_retired/rejected` beside the existing retire counters.
213 production logic tests and 14 protocol tests pass, including both response
orders, late publication/submission, exact receipts, generation reuse, cancellation
and independent consumer retirement. Windows Mesa and the normal signed KMD
package build. The conservative startup unwind gate is 5248/17936 bytes,
including saved registers and return addresses (the older 4936 figure counted
stack allocations only). Two independent review rounds are dry. Deployed as
oem50.inf with the new content-hashed ICD, rebooted to Code 0 with D3D12 enabled
and a visible desktop. All four native synchronization cases pass in session 1
(`tmp/dx12-sync-266-runtime/20260906-155029-223/`). The completed instrumented GT1 run reports
**74.26 FPS versus 20.03 on .265 (3.71 times)** with the same definition/options
and async WSI enabled, `HELIOS_PERF=1`, `--debug-log` and a four-second ETW slice.
Render PID 5556 loaded the new driver pair; 49240 KMD
feedback notifications were accepted, 720 matched an already-retired exact wire
receipt and 151 were conservatively refused (the counter does not classify the
refusal reason). Local sync retirement reports 50111 feedback completions and
zero wire fallback. Post-run device status remains Code 0. Results/hashes are in
`tmp/dx12-sync-266-perf/validation.json`. The exported 3DMark result supplies FPS;
the ETW parser's mixed-offset negative durations are not acceptance evidence.
The owner subsequently confirmed the shadows are fixed and observed about
100 FPS in their own benchmark. This is the visual acceptance at recovered
throughput; it is separate from the instrumented GT1 result. Sharing, unchanged
bindings, rotation/resize and WSI stress remain open. Debugger/native timings are
diagnostic, not VM performance acceptance. Upstream host device-loss/disconnect retirement
still lacks an error status through the callback/proxy interface, an explicit
remaining failure-path gap in `EXECUTION_SYNC.md`.

**Resolution-dependent VNC recovery:** after .265 boot, QEMU's 2397x1517
scanout import required 14745600 bytes but the DMA-BUF carried 14565376; VNC was
black although the guest composed desktop had content. Restored the previously
working 1280x800 through the existing RFB SetDesktopSize request, then restarted
the exact Helios display device so its start-time mode cache refreshed. Actual
VNC pixels are visible in `tmp/dx12-sync-265-runtime/desktop-restored-vnc.png`.
This restores runtime visibility; it does not fix general arbitrary-resolution
import or dynamic VidPn mode refresh. QEMU source and the launcher are unchanged.


**Deployment and crash repair, 2026-09-06:** the authorized stack deployment found and
fixed a hardcoded ICD basename in both new producer resolvers; they now resolve
the live device dispatch module, supporting the installer's content-hashed DLLs.
DXVK/vkd3d and both release UMDs rebuilt successfully. The deployed replacement
uses the normal KMD package profile, which passed the complete stack gate at
4936 / 17936 bytes. Release KMD also built, but its inlined symbols leave the
existing stack gate incomplete, so it is not the selected deployment image.
Per-crate packaging builds now set
their own local `CARGO_TARGET_DIR` to prevent inherited-target stale UMD copies.

Reboot was subsequently authorized. `.261/.262` refused allocation opens; `.263`
then bugchecked in DWM startup. The matching dump proves **0x113/0x26/1** at
`dxgkrnl!DxgGetHandleDataCB`, reached from the new producer BIND while an acquired
allocation reference remained outstanding. Dxgkrnl explicitly diagnoses a WDDM2
driver calling a WDDM1.x callback. The prior explanation of the open-time null
result as unpublished handles was incorrect: the legacy callback is rejected on
this WDDM2 path. `.264` removes the legacy lookup: OpenAllocation associates the
global state under an acquired reference; BIND acquires only the exact open.
Both use the tested scoped acquire/release helper on one PASSIVE thread.

The user booted without virtio-gpu for repair and explicitly prohibited rollback.
`.264` was signed/staged as **oem48.inf**, with both explicit release UMDs;
`pnputil /add-driver /install` marked the non-present GPU for reinstall. The user
restored virtio-gpu and booted at **01:16:27 on 2026-09-06**. `.264 / oem48.inf`
was live at that checkpoint, Code 0; DWM's UMD/ICD hashes matched the replacement artifacts. A fresh
desktop capture renders, `PrOpenF=PrBindAt=IrqlBad=0`, and no new bugcheck was
recorded. The observed startup crash is repaired; this is not broader runtime
or performance acceptance.
The normal-profile SYS SHA256 is
`E66C19BBFD212F16228DB8483B3A0E6F1E2894401D39F79DF7B9C9EC112199F8`.
Full Fire Strike completed at **01:31:06**: **35669 overall / 57396 graphics**,
GT1 **249.06 FPS**, GT2 **250.04 FPS**, Physics **40451**, Combined **8881**.
All five workload statuses are successful; host VNC captures show changing
demo, GT2 and Combined frames. An earlier run was cancelled by display/focus
loss coincident with the guest capture task; that incomplete result is preserved
and excluded. The successful retry used host-only capture and a hidden task
wrapper. Full Time Spy then completed at **01:41:56**: **15934 overall /
16222 graphics**, GT1 **101.28 FPS**, GT2 **96.74 FPS**, CPU **14482**.
All four workload statuses are successful; the native UMD12/ICD module hashes
match the deployed artifacts, with changing demo/GT1 captures and a GT2 scene
capture. After both benchmarks the desktop is visible, the original DWM process
and boot remain live, and no new System bugcheck/shutdown event is recorded.
`PrPub/PrRet` advanced to **95233/95232** (rate-limited snapshots), with
`PrInitF=PrOpenF=PrBindAt=IrqlBad=D12MrgF=0`.
These are single observed results, not interleaved performance comparisons.
Activation and benchmark evidence is under `tmp/hps2-264-runtime/`.
Four minidump files were preserved; CDB could parse only the latest,
which matches the full dump. Dumps and analysis are under `tmp/hps2-263-crash/`;
the full dump and matching `.263` SYS/PDB are preserved in the guest at
`C:\ProgramData\HeliosDeployBackups\producer263-crash`. Earlier deployment
evidence is under `tmp/hps2-261-acceptance/`.

**The charter is `DX12.md`; the implementation set is `docs/dx12/`.
`docs/dx12/DECISIONS.md` governs architecture.**

**Owner update, 2026-09-08:** the mandatory multi-agent review loop, fixed lane
ownership and two-dry-round deployment requirement are retired. Use review and
validation appropriate to the concrete change. The prior CB48 driver completed
its recorded IR7/IR8 reviews; the later IR11 measurement-harness review was
cancelled by this directive, not completed or counted as dry.
`GATES.md` remains an acceptance suite. Its observations establish only the
behavior exercised: the historical delayed `D12-G8` run produced correct pixels
while its fence wait stayed 0.6 us and the required dependency was absent.
Real D3D12 workloads, native correctness and the owner's visual acceptance remain
the targets.

### ⭐⭐ THE GOAL, set by the owner 2026-08-06 — three deliverables, in this order

> **1. Visible D3D12 pixels the owner can see. 2. Time Spy success. 3. Port Royal success.**

Not a triangle, not a rung, not a green suite. `docs/dx12/PENDING.md` is the full gap inventory;
this is the **critical path through it**, and the ordering is forced by dependencies rather than
chosen. A `D12-G*` gate passing is an acceptance observation, not proof of the
complete subsystem contract.

⇒ The critical path, the gap inventory and the full D3D12 session record live in
`DX12.md`, `docs/dx12/` (`DECISIONS.md` governs architecture,
`PENDING.md` the gap list) and, for the dated narrative, the archive under
"Workstream 4 — D3D12".

## Tooling (keep alive; this stage depends on it)

- **What is on the SCREEN, sampled at ~30/s** — `tools/vnc_frame_probe.py` +
  `tools/vnc_scanout_correlate.py` (added 2026-07-29 for defect 0ab; needs
  numpy + pillow, host-side only, a venv is fine).
  The probe is an RFB client against QEMU's VNC server. It stamps every
  framebuffer update with `time.time()` — the SAME CLOCK as the
  `virtio_gpu_cmd_*` lines QEMU's `log` trace backend writes to
  `/tmp/helios-qemu-stderr.log` — so a displayed frame can be attributed to a
  specific `res_flush`. Enable the events over QMP first:
  `python3 qmp trace-event-set-state virtio_gpu_cmd_set_scanout_blob /
  _res_flush / _res_unref` on `/tmp/helios-tpm/mon.sock`.
  Its **completeness oracle** is what makes it decisive: `--hud x0,y0,x1,y1`
  names a rectangle that is bright in every FINISHED application frame
  (3DMark's fps bar by default), which separates "the app rendered a dark
  scene" from "we displayed a frame the app had not finished". Whole-frame
  brightness cannot do that and led two sessions astray.
  ⚠ `screendump` is not an alternative under `sdl,gl=on` OR `egl-vnc`: the
  console's scanout kind is DMABUF, so QMP answers `"no surface"`.
  ⚠ Use `--exclusive`; QEMU refuses a SHARED client while an exclusive viewer
  (most viewers) is connected, and drops it silently after ClientInit.
- **Registry knobs** (service key, active KMD reads) — this list is now the
  complete set and is checked against `kmd_render/src/diag.rs`'s `pub mod knobs`:
  `DiagLevel`, `AllocCached`, `DmaGpuFence`, `BindFlushMode`, `DispatchBind`,
  `PresentProbe`, `DisplayHalf`, `DirectFlipCaps`, `CrossAdaptCaps`,
  `BarSegFlags`, `BarSegBaseMB`, `BarSegMode`, `VidMmVramMB`, `FlipCapsX`,
  `FlipQueueN`, `PresentWmk`.
  It used to list `ScanoutDiag`, which the very next bullet says was RETIRED in
  T6/R901, and to omit six knobs that do exist. Do not add a knob here without
  adding it there, or the reverse.
  **`PresentWmk` (default 1 since 22.22.244.0)** gates a WDDM submission that
  carries a live present stream boundary on that boundary alone instead of on
  the whole `next_wire_fence` backlog; `0` restores the historical superset for
  a same-boot A/B. Advertised value mirrored in `PwExact`; the FIFO-head block
  reason in `WfBWire`/`WfBStrm`/`WfBBlt`. **`FlipQueueN` (default 1)** sets
  `DXGK_DRIVERCAPS::MaxQueuedFlipOnVSync` (mirrored in `FlipQueV`); depth 4 was
  MEASURED INERT on 2026-08-04 with and without `FlipCapsX=3`, so it exists as a
  bisect handle only. Both are read at AddAdapter/transport init, so
  `pnputil /restart-device` applies them with no reboot.
  `DisplayHalf=1` enables the render+display adapter shape. `AllocCached=0`
  is the CpuVisible cached-allocation kill switch. `DirectFlipCaps` and `CrossAdaptCaps` are
  explicit cap-advertisement probes; leave off unless bisecting.
  `BarSegFlags`/`BarSegBaseMB` bisect BAR descriptor flags/base. `DiagLevel`
  enables the generic S-ring registry breadcrumbs.
  **`BarSegMode` now has exactly TWO legal values** (T4b/R904, KMD 22.22.187.0):
  `10` (default, absent = production: aperture id 1 + BAR id 2) and `0` (the
  recovery baseline: aperture id 1 + paging-RAM cpu-host id 2, no BAR). The
  historic Code-43 bisect arms `1`, `2`, `5` and `11` are DELETED, along with the
  `probe_only` BAR segment and its 16 MiB contiguous RAM block. Any other value
  is coerced to `10` and recorded in the new `BarMCo` counter carrying the stale
  number — so a VM left set from an old bisect now binds and says so instead of
  reporting a segment no allocation may use. Nothing reports segment id 3 any more.
- **ScanoutDiag — RETIRED in T6/R901 (KMD 22.22.188.0).** The knob, its 16 modes
  and `ddi/scanout_diag.rs` are GONE from the driver; any `ScanoutDiag` value or
  `Sdg*`/`S2d*` name still in the service key is a stale leftover. What it bought
  and why it went: the lab published its colour-bar blobs through the PRODUCTION
  publish word, so at the type level a KMD-owned fill image was indistinguishable
  from the Windows-designated primary, and a leftover `ScanoutDiag >= 4` selected
  a 5-extension `VkDevice` (the 38th-session global-modifier-enable regression
  class) on the one device every render/scanout/GDI path uses. Neither is
  representable now. **`Sdg*` names that SURVIVE, written by the production
  LINEAR fallback:** `SdgLStg SdgLReq SdgLBit SdgLTyc SdgLImg SdgLMem SdgLPch
  SdgLOff` (zeroed each StartDevice by `zero_linear_scanout_breadcrumbs`), plus
  `SdgMt SdgMf SdgBFl` and `SdgDevR SdgDevX` (ext tier; numbering unchanged, 1 =
  export trio, 2 = none).
- **Scanout counters** (service key fixed names): `Sc*` =
  `SetVidPnSourceAddress` scanout, `CSc*` = create-time scanout bind attempt,
  `PSc*` = Present/HWQ diagnostic-only scanout candidate, `Sdg*` = diagnostic
  scanout allocator/bind path, `Rf*` = periodic active-scanout refresh. Values
  persist across boots; trust movement plus same-boot QEMU traces.
- **SAMPLED counters, 22.22.180.0+** (R316): the `PB*` IDENTITY values written by
  `DxgkDdiPresent` — `PBcall PBflag PBcnt PBalst PBDma PBPatch PBpdsz PBkpsz`,
  the `PBs*`/`PBd*` surface identity sets, `PBstrk`/`PBdtrk`, and the flip arm's
  `PBsrc PBsw PBsh PBsDir PBIdOk PBFlip=1` — refresh on the 1st present and
  every 600th thereafter at `DiagLevel=0`, NOT per frame. **A `PB*` identity
  value can therefore be up to ~10 s stale; do not read one as live.** Set
  `DiagLevel=1` (+ `pnputil /restart-device`) to restore the per-call cadence.
  `PBRet=STATUS_SUCCESS` follows the same first/every-600th cadence beginning
  with 22.22.240.0; every non-success `PBRet` remains immediate. UNTHROTTLED,
  always current: `PBCpy` (all arms), `PBFnc`, `PBSyWt`, `PBSyCp`, and
  `PBFlip`'s `0xE1`/`0xE2` failure arms.
- **RETIRED 22.22.180.0** (R903/x-dup-dead-20 — do not look for these; they are
  gone from the driver, and any value still in the service key is a stale
  leftover): the `GdiAccelMode` knob and the whole `Gd*` counter family —
  `GdiM`, `GdiE`, `GdiS`, `GdFa`, `GdFg`, `GdFs`, `GdFb`, `GdFm`, `GdFi`,
  `GdFr`, `GdTc`, `GdDs`, `GdCn`, `GdCr`, `GdCc`, `GdCg`, `GdBn`, `GdBr`,
  `GdBg`, `GdXn`, `GdXz`, `GdXr`. The KMD no longer advertises
  `SupportKernelModeCommandBuffer` in any configuration and no longer contains a
  GDI raster executor; GDI renders through win32k's CPU redirection path.
- **Counters** (service key): Ch* (CpuHostAperture),
  Pg* (paging engine; `PgEv` nonzero means unresolved virtual paging transfer),
  AE* (8-slot allocation create/open ring: resid, dimensions, ctx/open marker)
  — all failure counters must stay 0; S-ring breadcrumbs persist across boots
  and high indices go stale after short boots.
- **Direct-primary producer gate — DELETED, do not reintroduce.** `PresentGateUs`
  and `PresentOrder` were removed on 2026-07-29 by owner directive and this
  inventory entry described them as live for a week afterwards.
  `umd/src/knobs.rs` carries the reasoning: a producer-side CPU stall hides an
  ordering defect instead of fixing it, it costs Fire Strike GT1 158 -> 136 fps
  when it holds, and it publishes the present anyway when it expires. Ordering
  belongs on the GPU timeline (`ScanoutAcquire` + a consumer-side wait), never
  on a blocked CPU thread.
- **UMD `DDI refusals:` counters, T6/R911** — nine names on ONE bounded log
  line, read by `tools/umd-gate-surface.ps1`: `srv_raw_hazard`,
  `resource_raw_hazard`, `text_filter_size_ignored`,
  `staging_busy_assumed_free`, `discard_partial`, `clear_view_unsupported`,
  `gs_so_declaration_dropped`, `tess_sig_fallback`,
  `unhandled_resource_dimension`. Emitted at `DestroyDevice` and on each
  counter's FIRST hit — never on a per-present path (that cost is what T2
  measured and reduced). All nine should read 0 on a healthy DWM session;
  **`gs_so_declaration_dropped` and `tess_sig_fallback` are expected to MOVE
  under 3DMark** and each names a real WS3 conformance gap. The UMD still has
  no registry counter surface, so the log line is the readout — check the line
  exists, not just the `fetch_add`.
- **RETIRED in T6** — do not look for these; they are gone from the driver:
  the `ScanoutDiag` knob and the whole diagnostic `Sdg*`/`S2d*` lab (R901; the
  production `SdgL*` LINEAR ladder plus `SdgMt SdgMf SdgBFl SdgDevR SdgDevX`
  SURVIVE), `ScForceReject`/`ScFrc` (owner-approved), `RbRid`/`RbFail` (R902,
  replaced by `RfUnb`), and on the UMD side the `PresentSyncPublish` and
  `VehicleKernelFlipWait` knobs with the whole kwait subsystem (R912a).
  `helios_umd_get_present_result` REMAINS EXPORTED, returning -1 — the mesa ICD
  resolves it by name and fails the dcomp vehicle with `E_NOINTERFACE` if it is
  absent. Two verbs now have no in-tree consumer and are kept as read-only ABI:
  `HELIOS_ESCAPE_QUERY_SCANOUT` / `helios_venus_query_scanout` (R910), and the
  UMD-side `HeliosPresentRefreshCmd` sender (R910 — the KMD still issues its own
  'HERF' marker in `display.rs`, so the refresh-marker ordering is intact).
- **Launcher/display path**: `tools/launch-helios-gtk.sh` supports
  `HELIOS_DISPLAY=egl-vnc` for `-display egl-headless` + VNC, intended as the
  reliable display-output inspection path, and `HELIOS_DISPLAY=sdl` is visually
  verified on native Wayland. It uses the `qemu-helios` submodule build, whose
  egl-headless/GTK/SDL OpenGL backends share exact OPTIMAL Vulkan readback when
  EGL cannot import a modifier-less native image. Interactive modes leave EGL
  vendor selection to the compositor while NVIDIA remains selected for
  Venus/readback Vulkan; globally forcing NVIDIA EGL breaks Wayland context
  creation on the development host. GTK is still blocked by its later GDK
  `eglMakeCurrent` failure.
  `HELIOS_QEMU_RENDER_GPU=nvidia` is the current owner preference; render-node
  defaults are tracked in the script. The old force-LINEAR LD_PRELOAD shims were
  experiments, not supported display paths. `HELIOS_QEMU_TRACE` can enable
  `virtio_gpu_cmd_set_scanout_blob`, `virtio_gpu_cmd_res_flush`,
  `virtio_gpu_cmd_res_create_blob`, and `virtio_gpu_cmd_ctx_submit`; the trace
  file `/tmp/helios-qemu-stderr.log` is ground truth for scanout shape.
- **ETW**: `logman create trace -p Microsoft-Windows-DxgKrnl 0xFFFFFFFFFFFFFFFF
  0xFF` → tracerpt → grep `AzureTriage` = dxgkrnl failure reasons in plain
  text. Found the segment rule in minutes.
- **AddAdapter iteration**: `pnputil /restart-device` re-runs AddAdapter with
  the loaded image — registry-knob experiments need no reboot.
- **T3 refusal instrument `ScForceReject` — RETIRED in T6 (owner-approved).**
  ⚠ **This leaves the T3 gate line "force each of the seven deferred-programming
  exits and confirm the matching counter moved" with NO mechanism behind it.**
  The `Sc*Err` counters below are still written at their real sites; what is gone
  is the only way to provoke them. Knob names are still capped at **14 chars**
  (`diag::MAX_CONFIG_NAME`) — a build failure, previously a silent always-default.
- **T3 counters** (all must read 0; reset at StartDevice so movement is
  this-boot): `ScBadAlc ScBadExt ScBadLay ScBadFmt ScLinErr ScSetErr ScNoTgt
  ScCpyErr` (one per refusal class), `ScUnav` (HPD dropped a dirty bit),
  `ScRetry`/`ScGaveUp` (R506's bounded retry), `ScStale` (a completion tried to
  clear an interval that was not its own), `ScGateCx` (the DIRQL raise CAS
  exhausted its budget), `HpdStTo` (the HPD prologue fell back to its 500 ms
  bound instead of the real start edge).
- ⚠ **Kernel stack budget on the boot path**: `DxgkDdiStartDevice` +
  `VirtioGpu::init` are the binding chain — **17568 B of the 17936-B known-good
  ceiling as of 22.22.184.0** (8408 + 9160), i.e. 368 B of headroom in a 24 KB
  kernel stack. Overflow at boot = `0xc0000001`/Startup Repair with **no dump and
  no bugcheck event**, and it does NOT reproduce on a live `devcon` restart.
  **Run `tools/kmd-frame-sizes.ps1` on every image** — it reads the frames out of
  the built `.sys` + linker `.map` with `llvm-objdump` (no PDB, no debugger),
  handles sub-page frames, sums the declared call CHAINS rather than every symbol
  measured, and **exits 1 over the ceiling**. `-Symbols`/`-Chains` extend it.
- **Counter snapshots**: `tools/kmd-counter-snapshot.ps1 -Label <name>` dumps the
  whole service key to `Z:\tmp\kmd-counters-<name>.txt` and prints the
  transport/venus/scanout subset. Registry values PERSIST ACROSS BOOTS — take one
  before a workload and one after and diff the files; a single read proves nothing.
- **T4a counters** (22.22.184.0+; every one must read 0 or be absent on a healthy
  boot): `VnEncOvf` (venus command-stream overflow, absent), `VnRingFt`/`VnRingWd`
  (ring fatal latch / head-wait ms, absent), `VnRingSz` (undersized ring mapping,
  absent), `VnMtDown` (memory-type downgrade, absent), `CpNoDrn` (prepared-copy
  drain skipped because nothing was submitted), `PBTdErr` (partial Present-BLT
  teardown), `CtNotOurs` (sync token named another entry), `WtTbl`
  (`FENCE_WAIT_TABLE_FULL`, split out of `WtOut`), `AbnDrop` (fences discarded by
  a TDR/preempt/reset epoch), `ChSzMm`/`ChSzDl`/`ChSzPv` (aperture size-provenance
  cross-checks), `MapDup` (duplicate blob map refused at commit time),
  `PciCapOob` (PCI capability tail outside config space), `WnRcf` (window-reserve
  reconfiguration refused). CollectDbgInfo is **version 6 / `[u32; 38]`**, with
  `FENCE_WAIT_TABLE_FULL` at index 37; word 25 is still `FENCE_WAIT_TIMEOUTS`.
- **Guest probes** (schtasks, session 1; SSH lands in session 0):
  `helios_paintcap` (screenshot → `Z:\tmp\screen_copy.png`), `helios_repaint`,
  `helios_flasher`, `helios_dstate`, `helios_enum_windows`, `helios_regedit`.
  `FindWindow('Progman')` is broken on this box — EnumWindows only.
- **Handle-leak instruments** (`handle.exe` is NOT installed on this box and
  these replace it; both run from SSH, no scheduled task needed):
  `tools/helios-handle-types.ps1` answers *what* — per-type counts either side
  of a run of device cycles, each new handle's type / granted access / kernel
  object address / name, the handles that CLOSED during the run, the
  transient-module set around ONE device cycle, and TlsAlloc/FlsAlloc
  high-water. `-Pin <module-prefix|all>` holds modules loaded, which attributes
  a leak to a module with no hooking at all: the per-device rate drops by
  exactly that module's own never-released statics.
  `tools/helios-handle-origins.ps1` answers *where* — IAT-hooks the
  handle-minting kernel32 entry points (matching slots by resolved address, so
  the kernel32/KernelBase/api-ms-win-core aliasing needs no spelling list) and
  prints a stack per handle that one device leaves behind. **Two traps, each
  cost a run:** the provider modules must be excluded or `kernel32!CreateFileW`
  recurses through its own IAT into the hook (`0xC00000FD`, no output), and the
  **ANSI** spellings are not redundant — the ICD is mingw-built, so its
  `CreateSemaphore` IS `CreateSemaphoreA`. ICD frames are DWARF, which dbghelp
  cannot read: resolve `module+0xRVA` with
  `x86_64-w64-mingw32-addr2line -f -C -e <dll> $((ImageBase + RVA))` on the
  Linux side (get ImageBase from `objdump -p`).
- **User-mode stack dumps**: `tools/take-minidump.ps1 -ProcessId <pid> -Path <dmp>`
  (P/Invoke MiniDumpWriteDump; the `rundll32 comsvcs.dll,MiniDump` trick writes
  TRUNCATED dumps on this box — do not use). Analyze on Linux:
  `~/.cargo/bin/minidump-stackwalk --symbols-path <breakpad-syms> <dmp>`;
  make syms with `~/.cargo/bin/dump_syms <pdb>` (dir layout
  `syms/<name>.pdb/<GUID+age>/<name>.sym`; fix the MODULE line name if the pdb
  was renamed). The deployed UMD build's PDB must GUID-match the dump's module
  (check with `llvm-pdbutil dump --summary`).
- **KMD build/deploy**: `win_build_kmd` (bumps the three version sites with a
  coherence check, then cargo-make package build) → `win_install_kmd`
  (install script + recommended, toggleable graceful guest reboot — the only
  reliable activation path). Manual fallback: `win_cargo` +
  `tools/install-helios-kmd.ps1` (ExecutionPolicy Bypass,
  `-AllowRebootRequired`); version bump = the single `HELIOS_KMD_VERSION` line in
  `kmd_render/driver-version.env` (build.rs renders the FILEVERSION numerics and
  the version strings from it; Cargo.make stampinf reads it via `env_files`);
  backups under
  `C:\ProgramData\HeliosDeployBackups`. New tools appear after the win MCP
  server restarts (new session).
- **dxvk staged-content probes** (`dxvk.heliosStagedProbes`, default OFF since
  `bdbbc2ea` — they were the ~1.5 s stall): full-surface raw+post-copy readback
  characterization at fixed refresh ticks for black-surface triage. Re-enable
  per process via `DXVK_CONFIG "dxvk.heliosStagedProbes = True"` (no rebuild).
- **Venus pipeline object trace** (`VN_HELIOS_PIPELINE_TRACE` env, per-process,
  default off): (ring, primary_tail seqno, object id) lines in the ICD diag log
  for pipeline-layout create/destroy, the vn_get_target_ring wait_all barrier,
  and graphics-pipeline creates — the defect-0b recurrence kit. The barrier
  skip/abandon lines (`BARRIER SKIPPED/ABANDONED in wait_all`) are ALWAYS on.
- **ICD sem-deadline strike log** (`helios_icd_diag.log`, always on): each strike
  line carries `sem=` (venus object id), `reason=` (vn_relax reason),
  `sig_queue=/family=/ring=` + `sig_value=/sig_age_ms=` (the most recent submitted
  signal op for that semaphore, recorded at submission prepare) and `pending_ms=`
  (how long a signal had been pending with zero movement — the quantity the
  deadline gates on). `sig_age_ms` near 0 on a strike = wait-before-signal false
  positive (should no longer happen post-f7a816f182f); large `pending_ms` = a
  genuinely stuck host channel.
- **Queue-submit phase timing**: `HELIOS_QUEUE_PERF=1` + `HELIOS_PERF_FILE`
  machine env (live in dwm since the 2026-07-06 reboot) — one aggregate line
  per 300 vkQueueSubmit2 calls (tls/wsi-flush/cache-flush/submit/fence-wait
  phase averages) to `C:\ProgramData\Helios\helios_queue_perf.log`.
- **Ring-fence probe**: `tools/vk_ring_fence_probe.cpp` → schtasks
  `helios_ringprobe` (wrapper `C:\Users\Rupansh\helios-probe\run_ring_probe.cmd`,
  `/rl LIMITED`; `helios_ringprobe_named` runs the NAMED-import mode against the
  dev ICD build via `icd_devbuild.json`). Proves/regression-tests the WS1 #4
  chain: rc=0 + "consumer wait tracked GPU completion". Build on the VM with
  the WinLibs g++ (`g++ -O2 -o ... Z:\tools\vk_ring_fence_probe.cpp -I <VulkanSDK>\Include
  C:\Windows\System32\vulkan-1.dll`) — no clang-cl on the box. **GOTCHA (cost a diagnosis detour): the Vulkan loader
  silently ignores `VK_DRIVER_FILES`/`VK_ICD_FILENAMES` in ELEVATED processes** —
  win_exec/SSH shells are High-IL (and `runas /trustlevel:0x20000` still reads as
  elevated), so an "env-override" probe actually tests the REGISTRY ICD. Run ICD
  A/B probes through a `/rl LIMITED` scheduled task.
- **QEMU fence tracing without restart**: QMP on `/tmp/helios-tpm/mon.sock` →
  `trace-event-set-state` for `virtio_gpu_fence_ctrl`/`virtio_gpu_fence_resp`
  (output → `/tmp/helios-qemu-stderr.log`; ctrl→resp gap per fence id = decode-
  vs GPU-completion retirement; disable after use — it logs 2 lines per fence).
  NOTE: `-d guest_errors` is already on, but virglrenderer's vkr_log/proxy_log
  are INFO-level = SILENT on the release build — absence of host log lines
  proves nothing below WARNING; a real host-side bisect needs a relaunch with
  `VIRGL_LOG_LEVEL=debug`.
