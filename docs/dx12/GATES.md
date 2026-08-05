# GATES.md — the D3D12 checkpoint ladder, `D12-G0` … `D12-G11`

**What this is:** the twelve checkpoints that carry Helios from "no D3D12 at all" to "a fully
working D3D12 implementation". Each gate states an entry condition, the exact commands (verbatim,
copy-pasteable), a pass criterion that is a number or a screenshot, the counters to snapshot, a
named artifact, and the traps that have already burned somebody. It is the D3D12 analogue of
`CONFORMANCE.md` §5–6 for D3D11, and it is written to be executed cold.

**What this is not:** a design document. The architecture is `docs/dx12/DECISIONS.md` (the D-, H-,
P-, K- and V-series entries) and it is authoritative — nothing here may contradict it. The
DDI contract is `docs/dx12/DDI_REFERENCE.md`, the present path `docs/dx12/PRESENT.md`, the
substrate `docs/dx12/SUBSTRATE.md`, the crate/DLL split `docs/dx12/ARCHITECTURE.md`, and the
kernel-side impact of all of it — including the K1–K3 work items §4.13 assigns to gates —
`docs/dx12/KMD_IMPACT.md`. It is also
not a performance document: only `D12-G10` reports a score, and it reports it as a 3-run median.

**Gate → phase map** (`DX12.md` §4 owns the phases; this file owns the gates):

| Phase | `DX12.md` | Gates |
|---|---|---|
| P0 Substrate proof | app-local vkd3d, zero Helios code | `G0` `G1` `G2` `G3` `G4` |
| P1 Contract capture | the WARP spy proxy | `G5` |
| P2 The split | `umd_common` + `umd12` scaffolding | `G6` |
| P3 Device | `OpenAdapter12` stops refusing | `G7` |
| P4 First frame | 99 real-body DDI slots (`DDI_REFERENCE.md` §14.2) | `G8` |
| P5 Conformance | DDI arm vs the P0 baseline | `G9` |
| P6 Real workloads | Night Raid, then Time Spy | `G10` |
| P7 Stability & ship | the stability list, packaging, CI | `G11` |

**Re-numbering from `research/R9-test-conformance.md`.** R9 is the primary source for this file and
used its own `D12-G0…G9`. Anything citing R9's numbering must be read through this table:

| R9 gate | Canonical | Note |
|---|---|---|
| R9 G0 build | **G0** | unchanged |
| R9 G1 device | **G1** | unchanged |
| R9 G2 suite | **G2** | unchanged; R9's "G2a variant" became **G9** |
| R9 G3 first frame | **G3** | unchanged |
| R9 G4 present characterisation | **G4** | unchanged |
| R9 G5 Night Raid | **G10** (first half) | |
| R9 G6 Time Spy | **G10** (second half) | one gate, two rungs |
| R9 G7 caps honesty | **G2** (produce `caps.csv`) + **G9** (reproduce it) | the app-local caps dump *is* the DDI arm's target |
| R9 G8 packaged smoke | **G11** | |
| R9 G9 CI | **G11** | |
| — | **G5** `G6` `G7` `G8` are new | the DDI half of the ladder, which R9 did not cover |

---

## 1. The rules every gate obeys

These are not advice. A gate result taken without them is not citable.

1. **⛔ Session 0 fakes driver regressions.** `win_exec` and SSH land in session 0, which has no
   desktop. Anything with a window, a swapchain or a benchmark runs in **session 1** through a
   scheduled task cloned from an interactive one. Canonical five lines,
   `tmp/perf/launch-gt1-arm.ps1:16-24`:

   ```powershell
   [xml]$xml = (schtasks /query /tn helios_perf_fs /xml ONE | Out-String)
   $xml.Task.Actions.Exec.Arguments = "-NoProfile -ExecutionPolicy Bypass -File Z:\tmp\dx12\gates\run-d12-gate.ps1 -Gate G3"
   $xml.Save($taskXml)
   schtasks /create /tn $taskName /xml $taskXml /f
   schtasks /run   /tn $taskName
   ```
   Task-name convention for this ladder: `helios_d12_<gate>` (e.g. `helios_d12_g3`).
   Existing interactive tasks to clone from: `helios_perf_fs`, `helios_paintcap`, `helios_flprobe`,
   `helios_ringprobe`, `helios_dcomp_probe`, `helios_vk_recreate`, `helios_repaint`,
   `helios_flasher`, `helios_dstate`, `helios_enum_windows`, `helios_regedit`
   (`CONFORMANCE.md:369-372`, `ROADMAP.md:3506-3508`).
   ⚠ The Vulkan loader **silently ignores** `VK_DRIVER_FILES`/`VK_ICD_FILENAMES` in elevated
   processes and win_exec shells are High-IL — any ICD A/B arm needs a `/rl LIMITED` task
   (`ROADMAP.md:3576-3580`; `CONFORMANCE.md:376` inherited a stale `ROADMAP.md:3422-3426` pointer
   for this — that range is the UMD `DDI refusals:` paragraph, not the loader trap).

2. **Registry counters persist across boots.** A single reading is not evidence. Every gate takes
   `tools/kmd-counter-snapshot.ps1 -Label <gate>-pre` before and `-post` after and diffs them; the
   script's own header states the rule (`tools/kmd-counter-snapshot.ps1:6-11`). Verify a counter
   *moved this boot*.

3. **Only owner-visible desktop state is rendering evidence.** `helios_paintcap` →
   `Z:\tmp\screen_copy.png` (`tools/desktop_paint_capture.ps1`) is ground truth. Log lines are not
   frames, a green test suite is not a frame, and `vkd3d.log` saying `Present` is not a frame.

4. **⛔ `VKD3D_FEATURE_LEVEL` and `--feature-level` must never appear in a gate command.**
   `VKD3D_FEATURE_LEVEL` (`vkd3d-proton-helios/libs/vkd3d/device.c:10888`) raises
   `TiledResourcesTier`, `ResourceBindingTier`, `ROVsSupported`, `RaytracingTier`,
   `MeshShaderTier`, `SamplerFeedbackTier` and `max_shader_model` with nothing backing them; the
   test binary's `--feature-level` (`tests/d3d12_crosstest.h:287`) does the client-side twin. This
   is the `SupportDirectFlip` / `FlipImmediateMmIo` landmine class (`DX12.md` §6.2,
   `DECISIONS.md` §7.8). `VKD3D_SHADER_MODEL` (`device.c:10617`) is permitted **only** inside an
   explicitly-labelled H5 A/B arm, never in a pass-criterion run.

5. **⛔ A frozen benchmark is a defect to root-cause, never a retry.** Owner directive (68th
   session). A knob-ON or experimental run happens only with an armed evidence trap and a
   registered hypothesis.

6. **Every gate records its binaries by SHA256 and writes to `tmp/dx12/gates/<gate>/`.**
   `tmp/` is gitignored (`.gitignore:39`), so it is scratch. The three artifacts that must
   *survive* are committed under **`docs/dx12/baselines/`**:
   `vkd3d-known-fail.txt` (G2), `d3d12-caps.csv` (G2, re-diffed at G9), `gate-binaries.txt`
   (the SHA256 manifest, appended per gate). Everything else stays in `tmp/`.

   ```powershell
   Get-FileHash -Algorithm SHA256 d3d12.dll,d3d12core.dll,d3d12.exe |
     Format-Table Hash,Path -AutoSize |
     Out-File -Encoding utf8 Z:\tmp\dx12\gates\<gate>\sha256sums.txt
   ```

7. **Refuse to read the exit code alone.** See §2.1: vkd3d's suite `skip`s when the device cannot
   be created, exits 0, and the runner prints `ALL PASSED!`. The pass criterion is always
   `(executed, failures, skipped)`.
   ⚠ **`executed` counts ASSERTIONS, not tests.** `include/private/vkd3d_test.h:317-321` prints
   `success_count + failure_count + todo_count + todo_success_count`, and `success_count` is
   incremented once per passing assertion in `vkd3d_test_check_assert_that` (`:145-152`). A
   single-test run therefore prints a number in the tens, not `1`. Never write a pass criterion
   that pins `executed` to a literal you have not captured on this box first.

8. **Do not build *Rust* onto `Z:\`, and never point a compiler's output at a directory that does
   not exist.** Two separate constraints, and conflating them has cost a cycle:

   * **Rust/cargo only:** cargo file IO fails on the `Z:\` 9p/virtio share with `OS error 87`
     (CLAUDE.md, windows-drivers-rs#481). `CARGO_TARGET_DIR` must be a local `C:` path.
   * **`cl.exe` is *not* subject to that.** Verified: `cl /nologo /EHsc Z:\tmp\clprobe\t.cpp` with
     the cwd on the share compiles and writes its `.obj` onto `Z:\` successfully. The real `cl`
     constraints are different and both bite in this file's commands:
     1. **`cl.exe` is not on `PATH` in a fresh `win_exec`/SSH shell** (`Get-Command cl.exe`
        returns nothing — there is no vcvars environment). Every compile below is therefore
        wrapped in the vcvars64 shim:

        ```powershell
        cmd /c "call `"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat`" >nul && cl <args>"
        ```
        (verified present on this VM; VS2022 Community is the only edition installed).
     2. **The `/Fe:` output directory must already exist**, or the link dies with
        `LINK : fatal error LNK1104: cannot open file '<path>'`. `C:\Windows\Temp\x` does **not**
        exist on this box (`Test-Path` = False) — either `New-Item -ItemType Directory -Force
        -Path C:\Windows\Temp\x` first, or use the per-gate `C:\Users\Rupansh\d12g<N>\`
        directory each gate already creates. The gates below use the latter.

   The Linux-side vkd3d build goes to `tmp/dx12/build/` on the native Linux fs.

9. **Never blame the host stack without host-side evidence** (CLAUDE.md rule 6). virglrenderer's
   `vkr_log`/`proxy_log` are INFO-level and silent on the release build — absence of host lines
   below WARNING proves nothing.

10. **⚠ Owner-gated actions inside this ladder.** Two gates need something only the owner may do:
    a QEMU relaunch with `HELIOS_DISPLAY=egl-vnc` (G4's host-side evidence, §4.5) and a cold boot
    (G11). Guest reboots are pre-authorised; host/QEMU relaunches are not. Stop and ask.

---

## 2. The test assets that already exist

Nothing in this section has to be written. It has to be *built and pointed at Helios*.

### 2.1 vkd3d-proton's own suite — 557 tests in one binary

Verified this session against the pinned submodule (`2c7ba22c`, `vkd3d-1.1-5456-g2c7ba22c`, zero
local commits):

| Fact | Command / citation |
|---|---|
| **557** `decl_test` entries, **557** unique names | `grep -c 'decl_test(' tests/d3d12_tests.h` = 557; `grep -o 'decl_test([a-zA-Z0-9_]*)' \| sort -u \| wc -l` = 557 |
| **12** carry `stress` → **545 in a default run** | `tests/test-runner.sh:58-66` drops `*stress*` unless `-s` |
| **34** translation units (33 `d3d12_*.c` + `d3d12.c`) → **one** exe | `d3d12_test_src` at `tests/meson.build:12-46`, `executable('d3d12', …)` at `:49`. ⚠ There are **35** `d3d12_*.c` files in `tests/`: `d3d12_test_utils.c` is compiled separately as `static_library('d3d12-test-utils', …)` at `:7`, and `d3d12_invalid_usage.c` is referenced by **no** build file at all (orphan — do not expect its tests to run) |
| two extra micro-benchmark exes | `descriptor-performance` `:56`, `pso-library-bloat` (same file) |
| tests are **off by default** | `meson_options.txt:1` `enable_tests=false`; demos need `enable_extras` (`:2`) |
| ⚠ **the three nested submodules are empty** | `khronos/Vulkan-Headers`, `khronos/SPIRV-Headers`, `subprojects/dxil-spirv` all `ls -A` = 0 entries |

**⚠ THE LOAD-BEARING TRAP — a dead adapter scores a perfect run.** Test setup is
`init_test_context_()`:

```c
/* vkd3d-proton-helios/tests/d3d12_test_utils.h:1357-1361 */
    if (!(context->device = create_device()))
    {
        skip_(line)("Failed to create device.\n");
        return false;
    }
```

`skip()` does **not** touch `failure_count`. The process return is
`return vkd3d_test_state.failure_count != 0;` (`include/private/vkd3d_test.h:329`), so it exits 0,
and `tests/test-runner.sh:152` prints **`ALL PASSED!`**. Individual bodies do the same
(`tests/d3d12_pso.c`, `tests/d3d12_mesh_shader.c`). **Therefore every gate below parses the summary
line, never the exit code:**

```
/* include/private/vkd3d_test.h:317-321 */
printf("%s: %lu tests executed (%lu failures, %lu successful todo, %lu skipped, %lu todo, %lu bugs).\n",
        vkd3d_test_name,
        (unsigned long)(vkd3d_test_state.success_count
        + vkd3d_test_state.failure_count + vkd3d_test_state.todo_count
        + vkd3d_test_state.todo_success_count), …)
```

⚠ **Read that first field carefully: "tests executed" is the ASSERTION count.** It is
`success_count + failure_count + todo_count + todo_success_count`, and `success_count` ticks once
per passing assertion inside `vkd3d_test_check_assert_that` (`:145-152`). `test_create_device`
(`tests/d3d12_device.c:25-71`) issues 4 `check_interface` calls and 13 `ok()` calls, so a **healthy**
single-test run prints roughly `17 tests executed`, not `1`. Any gate that pins this field to a
literal is a gate that fails on green.

Pass = `failures == 0` **and** `skipped == 0` (per-test arms) or `skipped ≤ baseline` (full runs)
**and** `executed > 0` **and** `#summary-lines == #test-log-files`
(a crashed test writes no summary). Without `-o <logdir>` the runner sends every test's stdout to
`/dev/null` (`test-runner.sh:90-94`) and throws the skip counts away — **`-o` is mandatory.**

**The dual-use property — the same binary tests both arms.** On Windows the harness resolves D3D12
by name from whatever the loader finds:

```c
/* vkd3d-proton-helios/tests/d3d12_crosstest.h:70-79 */
static inline void *get_d3d12_pfn_(const char *name)
{
    static HMODULE d3d12_module;
    if (!d3d12_module)
        d3d12_module = LoadLibraryA("d3d12.dll");
    return GetProcAddress(d3d12_module, name);
}
```

and `d3d12.dll` is **not** a KnownDLL on this guest (R9 §1.6, verified read of
`HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\KnownDLLs`), so the exe's own directory
wins. Hence:

* `d3d12.exe` **with** vkd3d's `d3d12.dll` + `d3d12core.dll` beside it → tests the **app-local arm**
  (G2).
* `d3d12.exe` **alone** → tests the **system `d3d12.dll`** over whatever WDDM UMD backs the adapter
  → the conformance suite for `helios_umd12.dll` (G9), with *no test changes at all*.

That is why G2's triple is directly comparable to G9's, and why the test investment survives
whichever way the DDI work goes.

**Harness argument surface** (`tests/d3d12_crosstest.h:838-870`): `--list-tests` (prints all 557,
exits 0), `--adapter <N>`, `--warp`, `--validate`, `--gbv`, `--feature-level` (⛔ rule 4).
⚠ `--adapter 0` behaves **exactly like no argument**: the harness only passes an adapter when
`use_warp_device || use_adapter_idx` is set, so index 0 falls through to vkd3d/DXGI's own default
(R9 §1.7). Two display devices exist on this VM, so the Helios index must be read with
`tools/dxgi_luid_dump.cpp` before every run.

**Environment** (`README.md:221-236`, implemented `include/private/vkd3d_test.h:277-308`):
`VKD3D_TEST_MATCH` (exact), `VKD3D_TEST_FILTER` (substring, mutually exclusive with MATCH),
`VKD3D_TEST_EXCLUDE`, `VKD3D_TEST_DEBUG=0|1|2`, `VKD3D_TEST_PLATFORM=wine|windows|other`
(auto-detected on Windows), `VKD3D_TEST_BUG=0`.

**What the suite will not tell you:** nothing about presentation (no test creates a swapchain —
vkd3d implements `IDXGIVkSwapChainFactory`, not DXGI), nothing about frame-level performance, and
nothing that satisfies the evidence rule. G3/G4/G10 exist because of this.

### 2.2 ⚠ A vkd3d-proton binary pair is already on the VM, with unknown provenance

`C:\Program Files\Looking Glass (IDD)\D3DTranslation\` contains `d3d12.dll` (155 662 B) and
`d3d12core.dll` (5 963 790 B), both `LastWriteTimeUtc` **2026-05-06T11:28:51Z** (re-read on the VM
2026-08-05; an earlier "2026-06-05" here was a transposed date), **with no version resource**.
Identified by
byte scan (R9 §1.6): `"vkd3d-proton/libs/d3d12core/debug.c"` at offset 5 021 850, `VKD3D_CONFIG` at
5 028 349, `"dxil-spirv does not support SHADER_QUIRK."` at 5 431 840.

This makes **G1 runnable today with zero builds** — and it is a trap if used carelessly.

⛔ **Rules for this pair.** (a) Record its SHA256 in `docs/dx12/baselines/gate-binaries.txt` before
citing any result from it. (b) Read its version banner first — `VKD3D_DEBUG=info` prints
`vkd3d-proton - build: %015llx` at INFO (`libs/vkd3d/device.c:1479-1481`). (c) **Never use it for
G2 or later.** A conformance baseline taken against an unidentified binary cannot be diffed against
anything. G2 onwards uses the G0 build exclusively.

### 2.3 The sample ladder — vkd3d's own demos first, MS samples second

`demos/meson.build:19,25` builds **`gears`** and **`triangle`** as `gui_app : true`, depending on
`lib_dxgi` + `lib_d3d12` (`:1-17` — `lib_dxgi` is the *system import library*, so at runtime they
load whatever `dxgi.dll` the loader finds; this is exactly the P-A surface). Their shaders are
**pre-compiled DXBC blobs checked into the tree** (`demos/triangle_vs.h`, `triangle_ps.h`,
`gears_vs.h`, `gears_ps_{flat,smooth}.h`), so no runtime compiler, no nuget, no MSBuild.
`triangle.exe` is therefore the cleanest possible "one D3D12 frame" probe and is what G3 uses.
`gears.exe` is animated and is what G4 uses.

`dx-samples-research-only/Samples/Desktop/` holds exactly **24** DirectX-Graphics-Samples solution
directories. `MiniEngine/` is **not** one of them — it sits at the corpus root,
`dx-samples-research-only/MiniEngine`, a sibling of `Samples/` (alongside `Assets/`, `Libraries/`,
`TechniqueDemos/`, `Tools/`). **Two build costs, both verified** (R9 §2.2): each sample pins Agility SDK
`Microsoft.Direct3D.D3D12 1.618.3` via `packages.config` and exports
`D3D12SDKVersion = 618` / `D3D12SDKPath = ".\\D3D12\\"`
(`D3D12HelloWorld/src/HelloTriangle/D3D12HelloTriangle.cpp:15-16`), and each hard-`<Error>`s
without `Microsoft.Direct3D.DXC.1.8.2505.32`. So each sample needs **MSBuild + a nuget restore
(network)**. `nuget` is not on PATH; **VM network reach is confirmed** —
`(Invoke-WebRequest https://api.nuget.org/v3/index.json -UseBasicParsing).StatusCode` returned
**200** on 2026-08-05, closing §7.8. Bootstrapping `nuget.exe` (or `dotnet restore`) is therefore a
download, not a blocker.

⚠ Even `D3D12HelloTriangle` builds with `dxc -Tvs_6_0/-Tps_6_0` — 174 of 178 shader-compile steps
in the corpus use `dxc -T*_6_x`, only `D3D12On7` uses `fxc` (R8 §5.2, §8.10). **"FL 11_0 + Tier 1 +
SM 5.1" is a valid DDI floor but not a runnable milestone; the real floor is FL 11_0 + SM 6.0**,
which the substrate already reaches.

Ordered rungs (R8 §5.3, R9 §2.3), ascending driver demand — used by G8 and G9:

| Rung | Sample | First thing it adds | Shader |
|---|---|---|---|
| 0 | `demos/triangle.exe` (vkd3d) | queue, PSO, root sig, FLIP_DISCARD swapchain, fence | DXBC, checked in |
| 1 | `HelloWindow` | **no shaders at all** — isolates device/queue/present | none |
| 2 | `HelloTriangle` | root signature 1.0, input layout, graphics PSO | vs/ps_6_0 |
| 3 | `HelloConstBuffers` | CBV + descriptor heap | vs/ps_6_0 |
| 4 | `HelloTexture` | SRV, upload heap, `CopyTextureRegion`, sampler | vs/ps_6_0 |
| 5 | `HelloFrameBuffering` | per-frame allocators, N-buffered fence discipline | vs/ps_6_0 |
| 6 | `HelloBundles` | `D3D12_COMMAND_LIST_TYPE_BUNDLE` | vs/ps_6_0 |
| 7 | `D3D12nBodyGravity` | compute queue — **first rung that meets the single-3D-node KMD** | cs_6_0 |
| 8 | `D3D12Multithreading` | many command lists on worker threads | 6_0 |
| 9 | `D3D12ExecuteIndirect` | command signatures, indirect args | 6_0 |
| 10 | `D3D12SM6WaveIntrinsics` | wave ops, `WaveLaneCountMin/Max`, `TotalLaneCount` | 6_0 |
| 11 | `D3D12ReservedResources` / `Residency` / `SmallResources` | tiling, `MakeResident`/`Evict` | 6_0 |
| — | `MeshShaders`, `Raytracing`, `HelloWorkGraphs` | **out of reach at SM 6.0** (need ms/as_6_5, lib_6_3, 6_8) | 6_3–6_8 |

### 2.4 3DMark — which workloads are D3D12, measured not assumed

Determined by scanning each workload exe for imported DLL name strings (R9 §4.1). **Reproduce with
a `-Recurse` enumeration, never the `\*\bin\*\*.exe` glob:**

```powershell
Get-ChildItem 'C:\ProgramData\UL\3DMark\chops\dlc' -Recurse -Filter '*.exe' | ForEach-Object {
  $s = [System.Text.Encoding]::ASCII.GetString([System.IO.File]::ReadAllBytes($_.FullName))
  $hit = @('d3d12.dll','d3d11.dll','vulkan-1.dll','d3d9.dll') | Where-Object { $s.Contains($_) }
  "{0,-44} {1}" -f $_.Name, ($hit -join ', ') }
```

⛔ The obvious glob `C:\ProgramData\UL\3DMark\chops\dlc\*\bin\*\*.exe` reaches only **27 of the 50**
workload executables and misses **10 of the 17 rows below** — the packs do not share one layout:
Solar Bay / Solar Bay Extreme / Speed Way / Steel Nomad / Wild Life Extreme are at
`<dlc>\windows\bin\<arch>\`, Wild Life at `wild-life-test\performance\windows\bin\x64\`,
DirectStorage and AMD-FSR at `<dlc>\windows\bin\x64\`, PCI Express at
`pci-express-test\dist\bin\x64\`, and the VRS Tier1/Tier2 and NVIDIA-DLSS tests one level deeper at
`<dlc>\<sub>\bin\x64\`. (Table contents re-verified 2026-08-05; only the recipe was wrong.)

| Workload exe | d3d12.dll | d3d11.dll | vulkan-1.dll |
|---|---|---|---|
| `3DMarkNightRaid.exe` (x64, **Win32**, ARM, ARM64) | ✔ | | |
| `3DMarkTimeSpy.exe` | ✔ | | |
| `3DMarkPortRoyal.exe` | ✔ | | |
| `3DMarkSpeedWay.exe` | ✔ | | |
| `3DMarkSteelNomad.exe` | ✔ | | ✔ |
| `3DMarkSolarBayExtreme.exe` | ✔ | | ✔ |
| `3DMarkSolarBay.exe` | | | ✔ |
| `3DMarkWildLifeExtreme.exe` | ✔ | | ✔ |
| `3DMarkWildLife.exe` | | | ✔ |
| `3DMarkMSFeatureTest.exe` (mesh shader) | ✔ | | |
| `3DMarkSamplerFeedbackFeatureTest.exe` | ✔ | | |
| `3DMarkVRSFeatureTestTier1/2.exe` | ✔ | | |
| `3DMarkDXRFeatureTest.exe` | ✔ | | |
| `3DMarkDirectStorageFeatureTest.exe` | ✔ | | ✔ |
| `3DMarkPCIExpress.exe` | ✔ | | ✔ |
| `3DMarkIntelXeSS.exe` / `_1_1` / `_1_2` / `_1_3` | ✔ | | |
| `3DMarkAMDFSR.exe` | ✔ | | |
| `3DMarkNvidiaDLSS.exe` / `DLSS2` / `DLSS3` / `DLSS4` | ✔ | | |
| `3DMarkCPUProfile.exe` | ✔ | | |
| `3DMarkICFWorkload.exe` / `ICFDemo.exe` (Fire Strike, Cloud Gate, Ice Storm) | | ✔ | (d3d9) |

All 23 DLC packs are installed and populated. ⚠ `apioverhead.3dmdef` exists with **no**
`api-overhead` DLC directory — that test is not installed. **No D3D12 workload has ever been
attempted on this box:** `Select-String -Path 'C:\Program Files\UL\3DMark\debug.log' -Pattern
'D3D12|DirectX 12|Time Spy|TimeSpy|Night Raid|Steel Nomad'` returns nothing across a 1 179 282-byte
log.

**Deployment trick that makes G10 cheap and reversible:** each workload is its own exe in its own
directory and `d3d12.dll` is not a KnownDLL, so the app-local arm is a **per-workload** experiment
with a one-file-delete rollback. No system install, ever.

---

## 3. Existing Helios machinery, and what each gates

`tools/` holds 119 entries (~58 probe sources + PowerShell drivers + the `win` MCP server);
`CONFORMANCE.md:161-262` catalogues them for D3D11. This section says which apply to D3D12.

### 3.1 Reusable unchanged

| Tool | Use in a D3D12 gate |
|---|---|
| `tools/dxgi_luid_dump.cpp` | **Mandatory before every suite/benchmark run.** Prints `adapter[i] luid=… vendor=… device=… name=…` for every DXGI adapter — that is the `--adapter N` value and the LUID vkd3d matches against `VkPhysicalDeviceIDProperties.deviceLUID`. Build (vcvars shim + an existing output dir — §1 rule 8): `cmd /c "call \"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat\" >nul && cl /nologo /EHsc /W4 Z:\tools\dxgi_luid_dump.cpp /Fe:C:\Users\Rupansh\d12g1\dxgi_luid_dump.exe"` (it `#pragma comment(lib,"dxgi.lib")`s itself). |
| `tools/adapter_type_probe.cpp` | `D3DKMTEnumAdapters2` cross-check when DXGI index and LUID disagree (the phantom-adapter class). |
| `tools/kmd-counter-snapshot.ps1 -Label <n> -OutDir <dir>` | Pre/post snapshot around every gate; writes `<OutDir>\kmd-counters-<Label>.txt`. Header states the persists-across-boots rule (`:6-11`). |
| `tools/kmd-gate-surface.ps1` | Machine verdict: non-zero exit if any `MustBeZero` counter moved. The list is at `:23-46` — `WtOut WtTbl CtOut`, the `Sc*` deferred-programming family, `VnEncOvf VnRingFt VnRingWd VnRingSz VnMtDown CpNoDrn PBTdErr CtNotOurs ChSzMm ChSzPv MapDup PciCapOob WnRcf`, `IrqlBad`, and `ChEi ChEa ChEp ChEs ChEb ChEm ChEu`. |
| `tools/umd-gate-surface.ps1 [-AllProcesses -SinceMinutes N]` | The D3D11 UMD refusal readout. **Under the app-local arm (G1–G4, G10-a) it should stay clean, and that is itself a check** — a vkd3d run *does* re-enter `helios_umd.dll` through the ICD's dcomp vehicle, so this is not a null instrument. Under the DDI arm (G7+) the D3D12 refusal counters live beside the D3D11 **eleven** (`struct DdiRefusals`, `umd/src/forward.rs:331-385` — its own doc comment says "The eleven DDI paths"; R1010 added `alloc_meta_format_unknown` and `readback_stride_unsafe` to the original nine, and `ROADMAP.md:3423-3428` still says "nine" because it was never updated). ⚠ Every counter is FIRST-HIT-ONLY: **absence is the zero reading** (`:12-16`). |
| `tools/kmd-frame-sizes.ps1` | Only when a KMD image changes (K1/K2). The boot path has **368 bytes** of headroom on a 24 KB kernel stack; the script matches the boot symbol by *mangled substring*, so a rename silently passes vacuously. |
| `tools/desktop_paint_capture.ps1` (schtask `helios_paintcap`) | **The only rendering evidence that counts.** `Graphics.CopyFromScreen` of the composed primary → `Z:\tmp\screen_copy.png`, plus `PrintWindow(Progman)` → `Z:\tmp\progman_printwindow.png`. |
| `tools/vnc_shot.py` | Host-side single-frame PNG off QEMU's RFB. QMP `screendump` answers "no surface" under DMABUF scanout, so this is the only host-side shot. ⚠ Requires the VNC display arm — see §4.5 and §7.2. |
| `tools/vnc_frame_probe.py` + `tools/vnc_scanout_correlate.py` | The defect-0ab instrument: per-RFB-update HUD-rectangle completeness oracle on `CLOCK_REALTIME`, correlated against QEMU's `virtio_gpu_cmd_set_scanout_blob` / `_res_flush` trace lines. This is what produces a **black-frame %** and a present→scanout distribution for G4. ⚠ The correlator's own header warns that `set_scanout_blob` lines begin `id 0, res 0x..`, so a `\D*` between event name and `res` silently drops every blob line (`vnc_scanout_correlate.py:10-13`). |
| `tools/scanout_timeline_dump.c` | `--cursor` / `--dump <first> <last>` around a run; 32 768-slot ring, deployed as `C:\ProgramData\Helios\scanout_timeline_dump.exe`. Already wired into `tmp/perf/run-gt1-arm.ps1:52-95`. |
| `tools/read_ledger_dump.c` | D4a read ledger, `C:\ProgramData\Helios\read_ledger_dump.exe`. |
| `tools/vk_surface_recreate_probe.cpp` (schtask `helios_vk_recreate`) | The exact vkd3d resize/fullscreen shape — two `VkSurface`s on one HWND — that broke the per-HWND dcomp target cache. **Run it before blaming D3D12 for a resize failure.** |
| `tools/dcomp_present_probe.cpp` (schtask `helios_dcomp_probe`) | The standalone vehicle proof (1023 flip presents, dwm composing). It performs the identical `D3D11CreateDevice` → `CreateSwapChainForComposition` → dcomp sequence the ICD vehicle does and logs each stage's HRESULT — **this is the probe that separates present risk V1 from V2** (§4.4). |
| `tools/live_dump.cpp`, `tools/take-minidump.ps1` | `MiniDumpWriteDump` for a wedged test process. |
| `tools/vram_report_probe.cpp` | DXGI/VidMm vs Venus heaps; the natural home for a `QueryVideoMemoryInfo` arm once a D3D12 device exists (K3). |
| `packaging/windows/Verify-Helios.ps1` | The only automated gate today: install-state hashes, PnP status/provider, Vulkan ICD registry, `OpenGLDriverName`, OpenCL vendor key, then four smoke probes under `-RunSmokeTests` (`:68-86`). |
| `tmp/perf/run-gt1-arm.ps1` + `tmp/perf/launch-gt1-arm.ps1` | **The ready-made wrapper shape for any gated run:** pre counter snapshot → read-ledger dump → timeline cursor → workload → post cursor/dump → post snapshot → copy the newest `umd-*.log` into the artifact dir. A D3D12 gate runner is this file with the workload line swapped. |

### 3.2 The D3D12 analogues worth writing, each named after the D3D11 probe it mirrors

Compile recipe as `CONFORMANCE.md:336-349`, with the libs swapped **and both of that recipe's
prerequisites written out** — `CONFORMANCE.md:336` states `:: MSVC under vcvars64.bat` and `:347-349`
notes that its PowerShell wrappers copy the source into `C:\Users\Rupansh\helios-probe` and invoke
`cl` there. A bare `cl …` from `win_exec` dies at *"cl is not recognized"* (§1 rule 8). The
copy-pasteable form, defining `$CL` once per shell and reusing it in every gate below:

```powershell
$VC = 'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'
function Invoke-Cl([string]$ClArgs) { cmd /c "call `"$VC`" >nul && cl $ClArgs" }

# the output directory must exist BEFORE the link step, or LNK1104
New-Item -ItemType Directory -Force -Path C:\Users\Rupansh\d12g8 | Out-Null
Invoke-Cl '/nologo /EHsc /W4 Z:\tools\d3d12_triangle.cpp /Fe:C:\Users\Rupansh\d12g8\p.exe /link d3d12.lib dxgi.lib dxguid.lib'
```

⚠ `cl` writing its `.obj` next to the source **on `Z:\` is fine** (verified) — the 9p `OS error 87`
class is Rust/cargo-only. It is the `/Fe:` target that must be a local `C:` path that already
exists.

| New probe | Mirrors | Why, and which gate needs it |
|---|---|---|
| `tools/vk_layered_driverid_probe.cpp` | — | Settles **H5** (`DECISIONS.md` §5): chain `VkPhysicalDeviceLayeredApiPropertiesListKHR` → `…LayeredApiVulkanPropertiesKHR` → `VkPhysicalDeviceDriverProperties`, print `driverID`. ~40 lines, read-only, no build of anything else. **Run before G0.** |
| `tools/d3d12_devicecreate_probe.cpp` | `tools/d3d11_devicecreate_probe.cpp` | Finds the Helios adapter **by description, not index**, calls `D3D12CreateDevice` at FL 11_0, prints the HRESULT. G1 and G7. |
| `tools/d3d12_caps_dump.cpp` | `tools/d3d11_fl_probe.cpp` + `format_caps` | Dumps every `D3D12_FEATURE_*` struct (`D3D12_OPTIONS`, `OPTIONS1..21`, `SHADER_MODEL`, `FEATURE_LEVELS`, `ARCHITECTURE1`) to CSV. **Produces `docs/dx12/baselines/d3d12-caps.csv` at G2; G9 must reproduce it.** |
| `tools/d3d12_clear_probe.cpp` | `tools/helios_clear_test.cpp` | Clear → `CopyResource` into a READBACK heap → `Map` → read pixel 0. **Headless pixel correctness with no swapchain** — the one instrument that separates "rendering works" from "presenting works". G8. |
| `tools/d3d12_triangle.cpp` | `tools/d3d11_triangle.cpp` | Real HWND, explicit adapter, FLIP_DISCARD vs BLT arms, optional pre-Present readback — separates "app rendered" from "DWM composited". G8. |
| `tools/d3d12_format_matrix_probe.cpp` | `CONFORMANCE.md` C5 | `CheckFeatureSupport(D3D12_FEATURE_FORMAT_SUPPORT)` over the DXGI format range → CSV baseline. G9. |
| `tools/d3d12_fence_probe.cpp` | `tools/d3dkmt_sync_probe.cpp` + `tools/vehicle_flipwait_probe.c` | `ID3D12Fence`: CPU signal, GPU signal, `SetEventOnCompletion`, cross-queue wait. Closes the residual monitored-fence question (`DECISIONS.md` §6, "G-fence"). G7/G8. |
| `packaging/windows/probes/d3d12-smoke.cpp` | `packaging/windows/probes/d3d11-smoke.cpp` | Factory → find the "Helios" adapter → `D3D12CreateDevice` → exit 0/1/2. The shipping gate. G11. |

---

## 4. The gates

### 4.1 `D12-G0` — Build gate: vkd3d-proton + tests + demos build, artifacts hashed

**Entry:** none. (Run the H5 probe `tools/vk_layered_driverid_probe.cpp` first — it is free and it
decides whether to expect SM 6.0/FL 12_1 or SM 6.6/FL 12_2 at G2.)

**Work:** initialise vkd3d's nested submodules and cross-build for Windows on the Linux host.

⚠ **The Linux mingw cross-build is the PRIMARY arm; native MSVC on the win11 VM is the FALLBACK,
taken only when a Windows debugger is wanted** (`DECISIONS.md` §6.1 — `ARCHITECTURE.md` §8.3 and
this gate must both say it, and they do). Two reasons, both concrete:

1. **It needs zero installation.** The whole toolchain is already on the Linux host's `PATH`
   (verified): `x86_64-w64-mingw32-{gcc,g++}`, `widl`, `glslangValidator`, `meson`, `ninja`, plus
   `wine` for `--list-tests`. `build-win64.txt` wants exactly `x86_64-w64-mingw32-{gcc,g++,ar,strip}`
   and a `widl-mingw-tools-fallback`. The MSVC arm needs choco strawberryperl (~1.5 GB, for `widl`),
   a downloaded `glslangValidator`, pip meson and VS2022 before it compiles anything.
2. **It is vkd3d-proton's own shipping build.** Upstream's `artifacts.yml` (and
   `test-build-linux.yml`) produce the DLLs Proton ships this way; `test-build-windows.yml` is the
   MSVC one, and upstream itself calls MSVC builds development-only and does not stress-test them
   (`README.md:136-142`).

⛔ If the MSVC fallback is taken, it builds to a **local `C:` path, never `Z:\`** — and that is the
Rust-crate rule generalised out of caution, not a `cl` limitation (§1 rule 8). Record in
`notes.md` which arm produced the binaries that were hashed; a G2 baseline and a G9 result taken
against DLLs from different arms are not comparable.

**Commands** (Linux host — the primary arm):

```bash
cd /home/rupansh/helios-vgpu/vkd3d-proton-helios
git submodule update --init --recursive        # khronos/*, subprojects/dxil-spirv are EMPTY
mkdir -p /home/rupansh/helios-vgpu/tmp/dx12/gates/G0
meson setup --cross-file build-win64.txt --buildtype release \
      -Denable_tests=true -Denable_extras=true \
      /home/rupansh/helios-vgpu/tmp/dx12/build/vkd3d-win64 \
  2>&1 | tee /home/rupansh/helios-vgpu/tmp/dx12/gates/G0/setup.log
ninja -C /home/rupansh/helios-vgpu/tmp/dx12/build/vkd3d-win64 \
  2>&1 | tee /home/rupansh/helios-vgpu/tmp/dx12/gates/G0/build.log

B=/home/rupansh/helios-vgpu/tmp/dx12/build/vkd3d-win64
G=/home/rupansh/helios-vgpu/tmp/dx12/gates/G0
sha256sum $B/libs/d3d12/d3d12.dll $B/libs/d3d12core/d3d12core.dll \
          $B/tests/d3d12.exe $B/demos/triangle.exe $B/demos/gears.exe > $G/sha256sums.txt
git -C /home/rupansh/helios-vgpu/vkd3d-proton-helios rev-parse HEAD > $G/vkd3d-commit.txt
wine $B/tests/d3d12.exe --list-tests > $G/list-tests.txt   # or run it on the VM
wc -l < $G/list-tests.txt
```

**The fallback arm** — native MSVC x64 on the win11 VM, taken **only when a Windows debugger is
wanted** (`README.md:143-152`), in the shape of upstream's `test-build-windows.yml`. It builds to a
local `C:` path, never `Z:\`:

```
:: VS2022 x64 Native Tools Command Prompt on win11.
:: Prerequisites, none of which the VM has today (§6.2 costs them):
::   choco install strawberryperl -y      :: supplies widl
::   glslangValidator on PATH             :: prefer C:\VulkanSDK\1.4.350.0\Bin\, NOT a third-party download
::   pip install meson
cd C:\Users\Rupansh\vkd3d-proton-helios
meson setup --buildtype release --backend vs2022 -Denable_tests=true -Denable_extras=true C:\Users\Rupansh\vkd3d-build-msvc
msbuild C:\Users\Rupansh\vkd3d-build-msvc\vkd3d-proton.sln
```

**Pass criterion:** all five artifacts exist and are non-empty —
`libs/d3d12/d3d12.dll`, `libs/d3d12core/d3d12core.dll`, `tests/d3d12.exe`, `demos/triangle.exe`,
`demos/gears.exe` — and `wc -l < list-tests.txt` == **557**.

✅ **PASSED 2026-08-05, primary (mingw cross) arm, first attempt.** `--list-tests` = **557**;
`wine … --list-tests` works on the Linux host (two benign `libEGL warning:` lines on stderr — send
them to a separate file, they are not part of the list). Add a sixth artifact to the hash block:
**`libs/d3d12core/helios_vkd3d.dll`**, the D4 target added in the same session
(`ARCHITECTURE.md` §7.4) — it is the binary `D12-G1` and every later gate actually loads, and it is
20 MiB because it carries the whole static `libvkd3d-proton.a` + `dxil-spirv`.

⚠ **Re-running `meson setup --reconfigure` after adding the target rebuilds only the new DLL** — the
rest of the tree is cached, so this is ~30 s, not a full rebuild. But it does mean the artifact
hashes change; re-hash rather than assuming the old block still describes the tree.

⚠ **The `subprojects/dxil-spirv` nested-submodule question this gate was told to settle is settled:**
four of them — `subprojects/dxbc-spirv` (`doitsujin/dxbc-spirv`), `third_party/SPIRV-Cross`,
`third_party/SPIRV-Tools`, `third_party/spirv-headers`. With vkd3d's own three that is **seven**
repositories in `helios_vkd3d.dll`; `ARCHITECTURE.md` §7.4's licence table needs seven rows, not
three.

**Counters:** none (no driver involved).

**Artifact:** `tmp/dx12/gates/G0/{setup.log,build.log,sha256sums.txt,vkd3d-commit.txt,
list-tests.txt}`; the SHA256 block appended to `docs/dx12/baselines/gate-binaries.txt`.

**Known traps:**
* ⛔ `package-release.sh` is the wrong tool: `build_arch` (`:52-77`) never passes `-Denable_tests`
  and deletes the build directory afterwards unless `--dev-build`.
* ⚠ `subprojects/dxil-spirv` has its own nested submodules — **UNVERIFIED** which (§7.4). The
  `git submodule update --init --recursive` above settles it as a side effect: immediately after it
  succeeds, run `cat subprojects/dxil-spirv/.gitmodules` and paste the answer into §7.4. Do it in
  this gate — §7.4 is only open because settling it clones into the working tree, which nobody
  wanted to do outside a build.
* ⚠ The submodule's `origin` is upstream, not the `.gitmodules` fork URL. Any Helios patch to vkd3d
  needs the fork wired as a push remote first (`DX12.md` §3.3).
* ⚠ `enable_breadcrumbs = enable_trace` and `enable_trace` is `auto` → true only for
  `debug`/`debugoptimized` (`meson.build:14,24-28,57-60`). **A release build has no breadcrumbs.**
  If G8+ needs them, build a second `debugoptimized` tree; do not flip the release build.

---

### 4.2 `D12-G1` — Engine gate: vkd3d produces correct pixels on venus, headless

⚠ **Rescoped by `DECISIONS.md` D2 (owner directive: no app-facing vkd3d).** This gate no longer
proves anything through an application's `d3d12.dll`. It proves the **engine path `umd12` will
actually use**, one layer below the DDI: `LoadLibrary("helios_vkd3d.dll")` →
`helios_vkd3d_create_device(luid, IID_ID3D12Device, &device)` → render to an offscreen
`ID3D12Resource` → read it back and compare. **No `d3d12.dll`, no D3D12 runtime, no DXGI, no
swapchain, nothing on screen.**

**Entry:** G0.

**Work:** write `tools/d3d12_bridge_probe.cpp` — the D3D12 analogue of the existing `tools/` D3D11
probes, and the same thing `ARCHITECTURE.md` §11 stage S4 calls for. It must: resolve the two
Helios exports by name (D4); create the device; create a command queue, allocator and list; clear a
committed `R8G8B8A8_UNORM` render target to a known non-trivial colour; draw one triangle with the
SM 6.0 shaders from `demos/`; copy to a `READBACK` heap; `Map` and verify the pixels. Failure at any
step is the gate's answer, and each step gets its own log line so the failure is attributable.

⚠ This is the **only** thing standing between a wrong assumption about vkd3d-on-venus and ~200 DDI
slots written on top of it (`DX12.md` §6.1). Do not wave it through because nothing is on screen.

No window is involved, so session 0 is acceptable — this and G2 are the only gates for which that
is true.

---

#### ✅ PASSED 2026-08-05 — 28 steps, 0 failures, first run

**As built:** `tools/d3d12_bridge_probe.cpp` + `tools/d3d12_bridge_probe.hlsl`, driven by
`tmp/dx12/build-g1-probe.ps1` (dxc → `-Fh` headers, then `cl`, then run). Output in
`tmp/dx12/gates/G1/bridge_probe.txt`; engine log in `tmp/dx12/gates/G1/vkd3d.log`.

What it proved, in order: both Helios exports resolve by name → `helios_vkd3d_create_device` returns
an `ID3D12Device` **with no DXGI in the device path** → queue/allocator/list → a root signature
serialized by `helios_vkd3d_serialize_root_signature` and accepted by `CreateRootSignature` (the H3
path) → a DXIL SM 6.0 PSO → clear + one triangle into a committed `R8G8B8A8_UNORM` 256×256 target →
`CopyTextureRegion` to a `READBACK` heap → fence → `Map` → **five sample points exact**: three
outside the triangle read `32,96,192,255` and two inside read `255,128,64,255` → `Release()` to
refcount 0.

Caps read off the live device, which is the real prize (this is the G7 zero-point, and it settles
H5 and U14 as a side effect):
`MaxSupportedFeatureLevel = 12_2`, `HighestShaderModel = 6.8`, `ResourceBindingTier 3`,
`TiledResourcesTier 4`, `ConservativeRasterizationTier 3`, `TypedUAVLoadAdditionalFormats 1`,
`ROVsSupported 1`, `RaytracingTier 11 (= TIER_1_1)`, `RenderPassesTier 0`, RTV descriptor stride 32.
`vkd3d.log`: `Enabling support for SM 6.6.` → `6.7.` → `6.8.`, `DXR 1.1 support enabled.`,
`DX Ultimate supported!`.

**Three corrections to the instructions above, all found by building it:**

1. ⛔ **The demos' shaders are DXBC `vs_5_0`/`ps_5_0`, not SM 6.0** (`demos/triangle_vs.h:21`). The
   probe compiles its own HLSL to **DXIL SM 6.0** with the Vulkan SDK's `dxc`
   (`C:\VulkanSDK\1.4.350.0\Bin\dxc.exe`, already on `PATH`), because DXIL is the path real D3D12
   clients take and the one H5 makes reachable. Reusing the demos' blobs would have gated on the
   legacy path instead.
2. ⚠ **`include/vkd3d.h:68` is wrong about `pfn_vkGetInstanceProcAddr = NULL`.** It says libvkd3d
   loads libvulkan itself; `vkd3d_init_vk_global_procs` (`device.c:461-468`) returns `E_INVALIDARG`.
   `helios_entry.c` therefore loads the Vulkan module itself. Anyone writing the `umd12` bridge
   against that header comment gets `E_INVALIDARG` from device creation and no explanation.
3. ⛔ **Do not write `& probe.exe 2>&1 | Tee-Object` in the runner script.** The Helios ICD prints a
   `HELIOS[gate5a]:` banner on **stderr**; PowerShell turns a native process's stderr into an
   `ErrorRecord`, and with `$ErrorActionPreference = 'Stop'` that kills the script *before the
   probe's own stdout is ever printed* — the first run looked like a probe crash and was a
   PowerShell artefact. Redirect inside `cmd` instead (`build-g1-probe.ps1`).

⚠ **The probe links `dxgi.lib` for exactly one thing — reading the Helios adapter's LUID** (it
matches `VendorId == 0x1af4` and never assumes index 0; on this guest Helios is index 0 and two
Microsoft Basic Render Driver entries follow). It links **no `d3d12.lib`**, which is the property
that makes it a test of the engine rather than of the runtime. `umd12` gets the LUID from the
runtime and needs no DXGI at all.

<details><summary>Superseded: the original app-local device gate (kept for the adapter-identification
recipe, which is still needed)</summary>

**Commands** (win_exec, session 0):

```powershell
$G = 'Z:\tmp\dx12\gates\G1'; New-Item -ItemType Directory -Force -Path $G | Out-Null
$T = 'C:\Users\Rupansh\d12g1'; New-Item -ItemType Directory -Force -Path $T | Out-Null
$VC = 'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'

# 0. identify the adapter (index AND luid) -- never assume index 0 is Helios
#    cl.exe is NOT on PATH in a win_exec shell: go through vcvars64 (rule 8)
cmd /c "call `"$VC`" >nul && cl /nologo /EHsc /W4 Z:\tools\dxgi_luid_dump.cpp /Fe:$T\dxgi_luid_dump.exe"
& $T\dxgi_luid_dump.exe | Tee-Object -FilePath $G\adapters.txt

# 1. stage the binaries beside the test exe (app-local arm) and hash them
Copy-Item Z:\tmp\dx12\build\vkd3d-win64\tests\d3d12.exe            $T\
Copy-Item Z:\tmp\dx12\build\vkd3d-win64\libs\d3d12\d3d12.dll       $T\
Copy-Item Z:\tmp\dx12\build\vkd3d-win64\libs\d3d12core\d3d12core.dll $T\
Get-FileHash -Algorithm SHA256 $T\*.dll,$T\d3d12.exe |
  Format-Table Hash,Path -AutoSize | Out-File -Encoding utf8 $G\sha256sums.txt

& Z:\tools\kmd-counter-snapshot.ps1 -Label G1-pre -OutDir $G

# 2. the device test, app-local arm
$env:VKD3D_DEBUG            = 'info'
$env:VKD3D_SHADER_CACHE_PATH= '0'
$env:VKD3D_LOG_FILE         = "$G\vkd3d.log"
$env:VKD3D_TEST_MATCH       = 'test_create_device'
& $T\d3d12.exe --adapter <N> 2>&1 | Tee-Object -FilePath $G\create_device.txt

# 3. control arm: same exe, vkd3d DLLs REMOVED -> the SYSTEM d3d12.dll -> our OpenAdapter12
Remove-Item $T\d3d12.dll,$T\d3d12core.dll
& $T\d3d12.exe --adapter <N> 2>&1 | Tee-Object -FilePath $G\create_device_system.txt

& Z:\tools\kmd-counter-snapshot.ps1 -Label G1-post -OutDir $G
& Z:\tools\kmd-gate-surface.ps1; "kmd-gate-surface exit=$LASTEXITCODE" | Out-File -Append $G\notes.txt
```

**Pass criterion:** `create_device.txt`'s summary line reads
`… <N> tests executed (0 failures, 0 successful todo, 0 skipped, 0 todo, 0 bugs).` — i.e.
**`failures == 0` and `skipped == 0` and `executed > 0`**.

⛔ **Do not pin `executed` to `1`.** That field is the assertion count, not the test count (§1 rule
7, §2.1): `test_create_device` makes 4 `check_interface` + 13 `ok()` assertions, so a healthy run
prints roughly **17**. **Capture the exact `executed` number here, write it into
`tmp/dx12/gates/G1/triple.txt`, and diff *that* at G7** — a drop in `executed` between the two arms
means the DDI arm bailed out of the test body early even though nothing "failed".

**`0 skipped` is the whole point** — a `1 skipped` line is the failure, not a pass, because
`skip()` never touches `failure_count`. And `vkd3d.log`
must contain the `vkd3d-proton - build:` banner and name the Helios/venus physical device.
The control arm is expected to report `1 skipped` today (`OpenAdapter12` →
`DXGI_ERROR_UNSUPPORTED`, `umd/src/adapter.rs:178-189`) — that is the honest zero-point G7 will
move.

**Counters:** `kmd-counter-snapshot.ps1` pre/post diff; `kmd-gate-surface.ps1` exit 0;
`umd-gate-surface.ps1 -AllProcesses -SinceMinutes 30` recorded either way.

**Artifact:** `tmp/dx12/gates/G1/{adapters.txt,sha256sums.txt,create_device.txt,
create_device_system.txt,triple.txt,vkd3d.log,kmd-counters-G1-{pre,post}.txt,notes.txt}`.

**Known traps:**
* ⚠ `--adapter 0` == no argument (§2.1). If Helios is index 0, pass `VKD3D_FILTER_DEVICE_NAME` as
  the belt-and-braces instead, and record which mechanism selected the device.
* ⚠ `VK_KHR_swapchain` on this ICD is conditional on
  `physical_dev->renderer_sync_fd.semaphore_importable` (`vn_physical_device.c:1334`). If a host or
  renderer change drops sync-fd semaphore import, the extension vanishes and **`D3D12CreateDevice`
  fails outright** — not just presentation. Assert the extension is present in `vkd3d.log`.
* ⚠ `ScStale` reads ~4 k/run pre-existing, and a gate check taken *after* a device restart never
  sees run-accrued counters (memory 67th). Do not chase it; do not restart the device between the
  pre and post snapshots.

---

</details>

### 4.3 `D12-G2` — Headless conformance baseline: the suite, the triple, the known-fail list

⚠ **Scope under `DECISIONS.md` D2.** The vkd3d suite creates **zero** swapchains — verified,
`grep -rl CreateSwapChain vkd3d-proton-helios/tests/` is empty — so it is fully headless and needs
no DXGI. Running it in vkd3d-direct mode (vkd3d's `d3d12.dll` + `d3d12core.dll` beside **the test
binary only**) is a **developer harness, not a shipping path**, and is the one narrow exception to
D2's ⛔. It costs nothing extra because the same binary is needed anyway for `D12-G9` against the
system `d3d12.dll`. If the owner would rather not run it at all, G1 alone gates the DDI work; the
loss is the baseline to diff G9 against.

**Entry:** G1 green **with the G0 build** (⛔ not the §2.2 prebuilt pair).

**Work:** run all 545 non-stress tests against the app-local arm and freeze the result as the
baseline everything else is diffed against. Also produce the caps CSV that G9 must reproduce.
Upstream publishes **no** expected pass count for any driver, so there is no absolute number to
hit — the metric is a baseline diff, exactly as `CONFORMANCE.md` C10 asks on the D3D11 side.

`tests/test-runner.sh` is bash; the VM has `C:\Program Files\Git\bin\bash.exe` at
`BASH_VERSION=5.3.9(1)` (`wait -n -p` at `:114` needs ≥ 5.1) and reports 16 processors.

**Commands** (win_exec is acceptable — no window; a long run is better driven from a
`helios_d12_g2` task so an SSH drop does not kill it):

```powershell
$G = '/z/tmp/dx12/gates/G2'          # bash-side path; Z:\tmp\dx12\gates\G2 from PowerShell
New-Item -ItemType Directory -Force -Path Z:\tmp\dx12\gates\G2\logs | Out-Null
& Z:\tools\kmd-counter-snapshot.ps1 -Label G2-pre -OutDir Z:\tmp\dx12\gates\G2

$env:VKD3D_SHADER_CACHE_PATH = '0'   # the runner exports it too (test-runner.sh:10); belt+braces
$env:VKD3D_FILTER_DEVICE_NAME = '<helios substring from G1 adapters.txt>'
& 'C:\Program Files\Git\bin\bash.exe' -c `
  "cd /c/Users/Rupansh/d12g1 && /z/vkd3d-proton-helios/tests/test-runner.sh -o $G/logs -j 1 ./d3d12.exe" `
  2>&1 | Tee-Object -FilePath Z:\tmp\dx12\gates\G2\runner.txt

& Z:\tools\kmd-counter-snapshot.ps1 -Label G2-post -OutDir Z:\tmp\dx12\gates\G2
& Z:\tools\umd-gate-surface.ps1 -AllProcesses -SinceMinutes 120 |
  Tee-Object -FilePath Z:\tmp\dx12\gates\G2\umd-gate.txt
```

Reduce the per-test logs to the triple (this is the pass criterion, not the runner's banner):

```powershell
$rx = '(\d+) tests executed \((\d+) failures, (\d+) successful todo, (\d+) skipped, (\d+) todo, (\d+) bugs\)'
$rows = Get-ChildItem Z:\tmp\dx12\gates\G2\logs\*.log | ForEach-Object {
  $m = (Select-String -Path $_.FullName -Pattern $rx | Select-Object -Last 1)
  [pscustomobject]@{ test=$_.BaseName
                     executed = if($m){[int]$m.Matches[0].Groups[1].Value}else{$null}
                     failures = if($m){[int]$m.Matches[0].Groups[2].Value}else{$null}
                     skipped  = if($m){[int]$m.Matches[0].Groups[4].Value}else{$null} } }
$rows | Export-Csv -NoTypeInformation Z:\tmp\dx12\gates\G2\summary.csv
"logs={0} nosummary={1} executed={2} failures={3} skipped={4}" -f `
  $rows.Count, ($rows|?{$null -eq $_.executed}).Count, `
  ($rows|Measure-Object executed -Sum).Sum, ($rows|Measure-Object failures -Sum).Sum, `
  ($rows|Measure-Object skipped -Sum).Sum | Tee-Object -FilePath Z:\tmp\dx12\gates\G2\triple.txt
$rows | Where-Object { $_.failures -gt 0 } | Select-Object -Expand test |
  Sort-Object | Set-Content Z:\docs\dx12\baselines\vkd3d-known-fail.txt
```

Then the caps dump (session 0 is fine — no window):

```powershell
$VC = 'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'
cmd /c "call `"$VC`" >nul && cl /nologo /EHsc /W4 Z:\tools\d3d12_caps_dump.cpp /Fe:C:\Users\Rupansh\d12g1\caps.exe /link d3d12.lib dxgi.lib dxguid.lib"
& C:\Users\Rupansh\d12g1\caps.exe > Z:\docs\dx12\baselines\d3d12-caps.csv
```

**Pass criterion:** the **first** green run *is* the baseline. Record the triple
`(executed, failures, skipped)` and the failing-test-name list. Thereafter:
`nosummary == 0` **and** no new failing test name **and** `skipped` not increased.

⚠ **A run whose `skipped` count is ≈ 545 is a G1 regression masquerading as a pass.** Check
`triple.txt`, never `runner.txt`'s `ALL PASSED!`.

**Counters:** KMD pre/post diff, `kmd-gate-surface.ps1` exit 0.
`umd-gate-surface.ps1 -AllProcesses` recorded — vkd3d's frames re-enter `helios_umd.dll` through
the ICD vehicle, so this is real signal, not a null read.

**Artifact:** `tmp/dx12/gates/G2/{logs/*.log,summary.csv,triple.txt,runner.txt,umd-gate.txt,
kmd-counters-G2-{pre,post}.txt}`; **committed:** `docs/dx12/baselines/vkd3d-known-fail.txt`,
`docs/dx12/baselines/d3d12-caps.csv`.

**Known traps:**
* ⛔ **The CRLF trap — it produced a false `ALL PASSED!` on the first attempt, 2026-08-05, and it is
  the single most dangerous thing in this gate.** Upstream's `tests/test-runner.sh` does
  `mapfile -t tests < <("$d3d12_bin" --list-tests)`. Run under Git-for-Windows bash the test binary
  writes **CRLF**, `mapfile -t` strips only the LF, and every test name keeps a trailing `\r`. So
  `VKD3D_TEST_MATCH="test_foo\r"` matches nothing, each of the 545 invocations runs only the
  unconditional tests, and **every** log reads
  `3 tests executed (0 failures, 0 successful todo, 0 skipped, 0 todo, 0 bugs)` — 1 635 assertions
  for the whole suite, against 19 for `test_create_device` alone. The runner then prints
  `Finished in 23s!` and `ALL PASSED!`. The log filenames carry the CR too, which is the cheapest
  tell: `ls logs | cat -A` shows `test_x^M.log`.
  **Fixed in the fork** (`e571d71a`, `tr -d '\r'`; the same commit fixes `nr_cpus`, which reads 0
  from a missing `/proc/cpuinfo` and makes the run loop start nothing unless `-j` is passed).
  ⚠ **The generalisation, which is the reason this is written out at length:** `triple.txt` is not
  a sufficient pass criterion on its own, because a triple of `(1635, 0, 0)` is all-green. **Also
  assert a per-test floor** — a suite where *every* test reports the *same* `executed` count is a
  harness failure, not a conformance result. `executed`-per-test histogram first, `failures` second.
  ⚠ **A 23-second wall time for 545 D3D12 tests at `-j 1` is itself the alarm.** Record wall time in
  `runner.txt` (`run-g2.ps1` does) and disbelieve any run that is implausibly fast.
* ⚠ **Reduce the logs on the Linux side, not in PowerShell.** The in-script PowerShell reduction
  enumerated **zero** files through the 9p share while `ls` on the Linux side saw all 545 in the same
  instant. Whether that is 9p directory-cache staleness or the CRs, a reduction that silently reports
  `logs=0` and still writes a `triple.txt` is worse than no reduction. `run-g2.ps1` keeps the
  PowerShell pass, but the number that gets recorded comes from the Python reduction over
  `tmp/dx12/gates/G2/logs/*.log` on the host.
* ⛔ **The S1 shared-heap trap — this is the one known *crash* hazard in the substrate, and this
  gate is where it fires.** `DECISIONS.md` S1: `VK_KHR_external_memory_win32` is absent from the
  Helios ICD, but on `_WIN32` vkd3d chains `VkExportMemoryAllocateInfo` for **any**
  `D3D12_HEAP_FLAG_SHARED` allocation with **no extension check**
  (`libs/vkd3d/resource.c:4405-4429`, the `#ifdef _WIN32` branch) and later calls
  `vkGetMemoryWin32HandleKHR` (`libs/vkd3d/device.c:7651`, `libs/vkd3d/d3dkmt.c:118`) — a **NULL
  function pointer** when the extension was never enabled. Shared heaps are not degraded, they are
  *hazardous*.
  **Which tests reach it, both inside this run's 545:** `test_map_texture_validation`
  (`tests/d3d12_resource.c:3595`, `D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER | D3D12_HEAP_FLAG_SHARED`
  at `:3631-3679`) and `test_open_heap_from_address` (`tests/d3d12_win32_exclusive.c:56`, asserts on
  those flags at `:114-115` and `:177-178`).
  **What a hit looks like:** those names appear in `summary.csv` with `executed = $null` — the
  gate's `nosummary != 0` condition — because a crashed test writes no summary line. **Do not read
  a `nosummary` hit as "flaky".** Check these two names first; if they are the ones missing, it is
  S1, not a new defect.
  **Ladder consequence:** S1 must be *fixed* (the memory twin of the semaphore import the Mesa fork
  already implements natively at `vn_physical_device.c:1271-1277`) or *fenced* (vkd3d refusing
  `D3D12_HEAP_FLAG_SHARED` up front with a named counter, which is the loud-failure shape) **before
  G9 may be called conformance.** A G9 whose known-fail list silently contains two crashed tests is
  not a conformance result. Record the chosen route in `notes.md` at this gate; it is a
  `vkd3d-proton-helios` fork patch either way, and it is the second one after H5's.
* ⚠ **Start at `-j 1`.** The runner defaults to one job per CPU thread (`test-runner.sh:14`) = 16
  concurrent D3D12 devices against a single `DXGK_ENGINE_TYPE_3D` node
  (`kmd_render/src/ddi/query_adapter_info.rs:1254-1278`). Step `-j 1` → `-j 2` → `-j 4`, recording
  wall time and any wedge (§7.10).
* ⛔ No `--feature-level`, no `VKD3D_FEATURE_LEVEL` (rule 4).
* ⚠ Expect the caps CSV to read **SM 6.8 / FL 12_2** — H5 closed on the upside (`SUBSTRATE.md` §7),
  and `D12-G1` already read exactly that off a live device. A CSV that says 6.0/12_1 is a
  *regression*, not the expected baseline;
  `TotalLaneCount` will read **1024** and that number is *known wrong* (vkd3d's
  `32 * subgroupSize` fallback, `device.c:10226-10233`, because venus exposes neither
  `VK_AMD_shader_core_properties` nor `VK_NV_shader_sm_builtins`). Record it as a defect, do not
  "fix" it by editing the CSV.
* ⚠ `PSSpecifiedStencilRefSupported` reads **FALSE** — confirmed (`VK_EXT_shader_stencil_export` is
  absent from the ICD). ⛔ **`DoublePrecisionFloatShaderOps` reads TRUE, not FALSE** — that prediction
  shared H5's root cause and died with it. Tests touching stencil-ref export are expected members of
  the known-fail list; double-precision ones are not.
* ⛔ **`test_uav_counter_null_behavior_{dxbc,dxil}` is a DETERMINISTIC Xid-109 REPRODUCER — the first
  one this project has had.** Found 2026-08-05 when it stopped the suite dead. This is the headline
  result of the first real G2 run and it matters far beyond D3D12: `ROADMAP.md`'s Xid-109
  "CTX SWITCH TIMEOUT" defect has been intermittent under Fire Strike (2 of 3 fast-path GT1 runs)
  since the 68th session, with an evidence trap armed and never sprung. **These two tests fire it on
  demand, in about six seconds, from a 30-second headless run.**

  The chain, end to end:

  | Layer | Evidence |
  |---|---|
  | Guest process | 0.17 s of CPU over six minutes, 5 MiB WS, all nine threads in `Wait` |
  | Guest stacks | vkd3d's `vkd3d_fence` thread parked in `SleepEx` **inside the venus ICD** (`vulkan_virtio_…!…+0x74f4f`, called from `d3d12core`); `vkd3d_queue` on a condition variable; the test's main thread in `WaitForSingleObject` |
  | Guest log | stops immediately after `DX Ultimate supported!` — it wedges after device creation, inside the test body's dispatch |
  | **Host** | `journalctl -k`: **`NVRM: Xid (PCI:0000:02:00): 109, pid=…, name=vkr-ring-346, channel 0x0000001b, errorString CTX SWITCH TIMEOUT`** |
  | Correlation | dxbc test starts 16:02:20 → Xid at **16:02:26**; dxil test starts 16:11:23 → Xid at **16:11:31**. Two for two, ~6-8 s in, a different `vkr-ring-NNN` each time |

  **What the test does:** `test_uav_counters_null_behavior` (`tests/d3d12_descriptors.c:4440`) builds
  UAVs with a **null counter resource** and dispatches a compute shader that performs counter ops on
  them — behaviour the test itself calls *"technically undefined, but all drivers behave robustly
  here, we should too"*. Its neighbour in the same file records **"Observed on NV: Blue screen of
  death (?!?!)"** for the analogous root-descriptor case, so this family genuinely hard-faults NVIDIA
  hardware rather than returning zeroes.

  ⚠ **It is NOT a transport wedge**, and that is the useful part: re-running the `D12-G1` bridge
  probe *while the wedged process was still alive* passed all 28 steps. Xid 109 kills **one channel**
  (`0x1b`) and the `vkr-ring-NNN` thread bound to it, so a fresh device gets a fresh context and works.
  Do not reach for the 66th/67th sessions' whole-transport story.
  ⚠ **Nothing host-side reports it to the guest.** `/tmp/helios-qemu-stderr.log` has **no entry at
  all** for either Xid — the render server loses the context and the guest simply waits forever.
  ⭐ **And the 67th session's bound cannot cover it, structurally.** That fix bounds a *ring* wait
  (`VN_HELIOS_RING_WAIT_BOUND_MS`, default 8000, `icd/mesa/…/vn_queue.c:2824`), and venus's own
  escalation ladder `vn_relax` (`vn_common.c:248`) checks `VK_RING_STATUS_FATAL_BIT_MESA` and the
  `ALIVE` bit via `vn_watchdog_timeout()`. **All of those are ring-liveness signals**, and Xid 109
  kills the GPU *channel* while the `vkr-ring-NNN` thread stays healthy and keeps marking itself
  alive. The watchdog is watching the wrong thing. ⛔ Do not shorten the ring bound in response; the
  missing signal is *host submission/fence failure*, which today reaches neither QEMU's log nor the
  guest. A lost host context must become a guest-visible error (device removal / TDR).
  ⚠ **These two are excluded by name from routine G2/G9 runs** — `tests/test-runner.sh -x <name>`
  (fork `fd205b2c`), which prints `EXCLUDED <name>` for each and `WARNING` for an `-x` that matches
  nothing, because a silently skipped test is indistinguishable from a passing one. The exclusion is
  a scheduling decision, not a verdict: they stay the Xid-109 repro, and "a guest app can fault the
  host GPU context" stays an open robustness defect in `ROADMAP.md`.
  **Instrument:** `tmp/dx12/g2-hang-watchdog.ps1`, run as a second scheduled task beside the suite.
  It waits for a `d3d12.exe` older than 240 s whose CPU has not moved in 120 s, names the victim (the
  newest log with no `tests executed` line), **captures `~*k` stacks and a `/ma` dump first**, appends
  to `hangs.txt`, and only then kills it so the run continues. A killed test writes no summary line,
  so it lands in `nosummary` — which is the honest bucket for a hang, and is why `nosummary != 0`
  must be triaged rather than treated as flake.
* ✅ **`graphics_hook64.dll` (OBS Studio's game-capture hook) — found injected, and REMOVED
  2026-08-05 before the baseline was taken.** Two of the nine threads in the wedged process were its:
  a third-party overlay hooking the same surfaces the driver owns, i.e. the foreign-stack hazard
  `DECISIONS.md` H2(a) describes, live, and an uncontrolled variable in every G2/G9 number. It did
  not prevent `D12-G1` from passing, so it was never proven to be *doing* anything — but a
  conformance baseline is the wrong place to carry an unexplained third party.
  ⛔ **The run in flight when it was removed was discarded and restarted**, because a baseline whose
  first 245 tests ran with an overlay and whose remainder ran without it is worth less than either
  half: every later diff against it would inherit an unknown split.
  ⚠ **Re-check before trusting any G2/G9 delta** — it is one line, and an overlay can come back with
  any app install:
  `(Get-Process d3d12).Modules | ? ModuleName -like 'graphics_hook*'` (or `lm` in a dump).

---

### 4.4 `D12-G3` — ⛔ RETIRED by `DECISIONS.md` D2

This gate was "first D3D12 frame on screen via the app-local vkd3d arm". **There is no app-facing
vkd3d arm** (owner directive, 2026-08-05), so it has no subject. The first D3D12 pixels on the
Helios desktop are now **`D12-G8`**, drawn through `helios_umd12.dll`, and that is also the first
exercise of the D3D12 present path.

The id is retired rather than renumbered so every existing cross-reference in `docs/dx12/` still
resolves. Its two durable pieces moved: the **screen-evidence procedure** (paintcap twice ≥2 s
apart, crop to the window rect, RMSE compare, and the promoted/maximized paintcap blind spot) is now
part of G8's pass criterion, and the **path-confirmation discipline** — never accept "a frame
appeared" without confirming *which path served it* — is stated in G8 as a hard requirement.

<details><summary>Superseded content, kept for the evidence recipes it contains</summary>

**Entry:** G2 baseline recorded, **and the P-A fix landed** (see traps).

**Work:** put `demos/triangle.exe` on the Helios desktop, in session 1, and photograph it.

⛔ **Land P-A first or this gate lies.** `DECISIONS.md` P-A: vkd3d implements no DXGI and never
creates a `VkSurfaceKHR`; it needs DXVK's `dxgi.dll`. But the ICD's present vehicle does bare
`LoadLibraryA("d3d11.dll")` / `LoadLibraryA("dxgi.dll")` / `LoadLibraryA("dcomp.dll")`
(`icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:486-488`) and DXVK's
`CreateSwapChainForComposition` returns **`E_NOTIMPL`** by default
(`dxvk-helios/src/dxgi/dxgi_factory.cpp:282-298`, `dxgi_options.cpp:178`). The result is a
**correct-looking picture served by the software GDI blit**.

**The fix, exactly (`DECISIONS.md` §6.1 / P-A) — ~10 lines, ICD-local, in
`wsi_win32_vehicle_runtime_init_locked` (`wsi_common_win32.cpp:480-535`):** load each of
`dxgi.dll` / `d3d11.dll` / `dcomp.dll` **by explicit full `%SystemRoot%\System32` path, AND then
verify what you got** — call `GetModuleFileNameW` on the returned `HMODULE` and refuse, with a
**named counter**, if the resolved path is not under `%SystemRoot%\System32`.

⛔ **Neither half is sufficient alone.** `LoadLibraryExA(…, LOAD_LIBRARY_SEARCH_SYSTEM32)` is *not*
enough, and a full path is *not* enough either: the loader's already-loaded check matches on **base
name**, so a DXVK `dxgi.dll` that the *application* loaded first is handed back to the vehicle no
matter how the vehicle asks. The `GetModuleFileNameW` verification is the entire point of the fix —
it converts a silent wrong-DLL bind into a loud, counted refusal. A P-A "fix" that only changes the
load call has not fixed anything and this gate will still lie.

**Pre-step, no build, separates present risk V1 from V2:** drop `dxvk-helios`'s `dxgi.dll`
(`C:\Users\Rupansh\dxvk-build\src\dxgi\dxgi.dll`) beside the existing `helios_dcomp_probe` binary
and run that task. Expected on today's code: failure at `CreateSwapChainForComposition` with
`hr=0x80004001`. A failure *earlier*, at `D3D11CreateDevice`, means V1 also bites (MS `d3d11.dll`
does not accept a DXVK `IDXGIAdapter` — **UNVERIFIED**, §7.5).

**Commands:**

```powershell
$G='Z:\tmp\dx12\gates\G3'; New-Item -ItemType Directory -Force -Path $G | Out-Null
$T='C:\Users\Rupansh\d12g3'; New-Item -ItemType Directory -Force -Path $T | Out-Null
Copy-Item Z:\tmp\dx12\build\vkd3d-win64\demos\triangle.exe              $T\
Copy-Item Z:\tmp\dx12\build\vkd3d-win64\libs\d3d12\d3d12.dll            $T\
Copy-Item Z:\tmp\dx12\build\vkd3d-win64\libs\d3d12core\d3d12core.dll    $T\
Copy-Item C:\Users\Rupansh\dxvk-build\src\dxgi\dxgi.dll                 $T\   # DXVK's, required
Get-FileHash -Algorithm SHA256 $T\* | Format-Table Hash,Path -AutoSize |
  Out-File -Encoding utf8 $G\sha256sums.txt
& Z:\tools\kmd-counter-snapshot.ps1 -Label G3-pre -OutDir $G

# session 1, cloned task
[xml]$xml = (schtasks /query /tn helios_perf_fs /xml ONE | Out-String)
$xml.Task.Actions.Exec.Arguments =
  "-NoProfile -ExecutionPolicy Bypass -Command `"`$env:HELIOS_WSI_PERF='1'; " +
  "`$env:VKD3D_DEBUG='warn'; `$env:VKD3D_LOG_FILE='Z:\tmp\dx12\gates\G3\vkd3d.log'; " +
  "`$env:VKD3D_SHADER_CACHE_PATH='0'; Start-Process -FilePath '$T\triangle.exe'`""
$x="$env:TEMP\helios_d12_g3.xml"; $xml.Save($x)
schtasks /create /tn helios_d12_g3 /xml $x /f
schtasks /run   /tn helios_d12_g3

Start-Sleep -Seconds 10
schtasks /run /tn helios_paintcap     # -> Z:\tmp\screen_copy.png
Start-Sleep -Seconds 5
Copy-Item Z:\tmp\screen_copy.png $G\shot-1.png
Start-Sleep -Seconds 3
schtasks /run /tn helios_paintcap
Start-Sleep -Seconds 5
Copy-Item Z:\tmp\screen_copy.png $G\shot-2.png
& Z:\tools\kmd-counter-snapshot.ps1 -Label G3-post -OutDir $G
& Z:\tools\kmd-gate-surface.ps1
```

Host side (only if the VM is on the `egl-vnc` arm — see §4.5 traps):

```bash
python3 /home/rupansh/helios-vgpu/tools/vnc_shot.py \
  --out /home/rupansh/helios-vgpu/tmp/dx12/gates/G3/host-shot.png
```

**Pass criterion:** **an owner-visible triangle in `Z:\tmp\screen_copy.png`** — two captures ≥ 2 s
apart, both showing the demo window. Log lines are not frames.

**Corroborate the path taken**, so a software-GDI fallback is not read as success (`R7` §7.5): the
vehicle diag under `C:\ProgramData\Helios\` must contain `READY chain=… adapter=<Helios>` and
`LIVE chain=… visual content bound`; the `HELIOS_WSI_PERF` line must show `presents` climbing with
`fails=0`; the UMD log must show `vehicle present #N` lines
(`umd/src/forward/present.rs:1560-1571`).

**Counters:** KMD pre/post diff, `kmd-gate-surface.ps1` exit 0, the `Sc*` scanout family at 0
(except pre-existing `ScStale`); `umd-gate-surface.ps1 -AllProcesses` clean.

**Artifact:** `tmp/dx12/gates/G3/{shot-1.png,shot-2.png,host-shot.png,vkd3d.log,sha256sums.txt,
vehicle-diag.txt,umd-*.log,kmd-counters-G3-{pre,post}.txt,notes.md}`.

**Known traps:**
* ⚠ **A maximized/promoted vehicle window is ABSENT from GDI paintcaps.** dwm promotes an eligible
  fullscreen-sized flip visual to direct/independent flip; the result is correct on the display and
  black in a `CopyFromScreen` shot (`wsi_common_win32.cpp:852-859`). **Keep the demo window small
  and partially overlapped**, or the gate reads a working frame as a failure.
* ⚠ `dxgi.enableDummyCompositionSwapchain` is a red herring: even turned on, `CreateSwapChainBase`
  needs `IDXGIVkSwapChainFactory` on the device, which an MS `d3d11.dll` device does not have.
* ⚠ Any *other* MS-D3D11 hwnd swapchain in the same process gets `DXGI_ERROR_UNSUPPORTED` from
  DXVK's DXGI (risk V3). Relevant when a title also creates a D3D11 overlay.
* ⚠ Expect a live ~5 ms/frame serialization: `helios_umd_get_present_result` returns −1
  unconditionally (`umd/src/vehicle_exports.rs:56-84`), so the ICD's acquire-side gate is never
  armed and every vehicle present takes the worker-serial `wait_last_present` fallback, measured at
  **avg 5.57 ms** (P-B). That is a *cost*, not a failure, and it belongs in G4's numbers.

---

</details>

### 4.5 `D12-G4` — Present-path characterisation: black-frame %, present→scanout, resize/fullscreen

⚠ **Re-sequenced by `DECISIONS.md` D2: this gate now runs AFTER `D12-G8`, not before it.** Its
subject is the **DDI** present path (MS DXGI → the D3D12 runtime → `pfnPresent` → `DxgkDdiPresent`
→ the existing flip arm → `set_scanout_blob`), because that is the only D3D12 present path that
exists. The measurements, oracles and pass thresholds below are unchanged and still correct — only
the entry condition and the client under test change.

**Entry:** `D12-G8` green (not G3, which is retired).

⚠ The ICD's dcomp vehicle, its ~5.57 ms serial gate and its extra frame copies are **not** on this
path (`DECISIONS.md` P-B). Do not attribute a D3D12 present number to them without evidence that
the vehicle was involved — under D2 it should not be.

**Entry:** G3.

**Work:** produce the three numbers that make a D3D12 present path comparable to the D3D11 one —
black-frame percentage, present→scanout latency distribution, and the resize/fullscreen verdict.
This is exactly what closed defect 0ab on the D3D11 side, with the same instruments.

**Commands:**

Host side, before the run (⚠ owner-gated, see traps):

```bash
# enable the two virtio-gpu trace events over QMP on the running VM.
# `on` is a MANDATORY positional (`action`, choices on|off|list, qmp_trace.py:59-62; usage line
# at :4). Omit it and argparse exits 2 with
#   "invalid choice: 'virtio_gpu_cmd_set_scanout_blob'".
python3 /home/rupansh/helios-vgpu/tools/qmp_trace.py on \
  virtio_gpu_cmd_set_scanout_blob virtio_gpu_cmd_res_flush

# --out is a DIRECTORY, not a file: the probe does os.makedirs(args.out, exist_ok=True) at :157
# and opens os.path.join(args.out, "frames.jsonl") at :163. Pass a .jsonl path and you get a
# DIRECTORY named frames.jsonl containing frames.jsonl, and the correlator (which does
# open(sys.argv[1]) at vnc_scanout_correlate.py:52) then raises IsADirectoryError.
# --seconds defaults to 60 (:141); the guest arm below runs gears.exe for 120 s, so overrun it.
# --hud is "x0,y0,x1,y1" -- two CORNERS (:148-152) -- NOT x,y,w,h.
python3 /home/rupansh/helios-vgpu/tools/vnc_frame_probe.py \
  --out /home/rupansh/helios-vgpu/tmp/dx12/gates/G4/cap \
  --seconds 130 --hud <x0,y0,x1,y1>
```

Start the probe **before** the guest task and let it outlive the workload; it picks the workload
window itself from the densest run of binds/s.

Guest side, session 1, 120 s of `gears.exe`:

```powershell
[xml]$xml = (schtasks /query /tn helios_perf_fs /xml ONE | Out-String)
$xml.Task.Actions.Exec.Arguments =
  "-NoProfile -ExecutionPolicy Bypass -Command `"`$env:HELIOS_WSI_PERF='1'; " +
  "`$p=Start-Process -PassThru 'C:\Users\Rupansh\d12g3\gears.exe'; Start-Sleep 120; " +
  "Stop-Process -Id `$p.Id`""
$x="$env:TEMP\helios_d12_g4.xml"; $xml.Save($x)
schtasks /create /tn helios_d12_g4 /xml $x /f ; schtasks /run /tn helios_d12_g4
```

Then correlate on the host, and run the resize probe on the guest:

```bash
python3 /home/rupansh/helios-vgpu/tools/vnc_scanout_correlate.py \
  tmp/dx12/gates/G4/cap/frames.jsonl /tmp/helios-qemu-stderr.log d12-gears \
  | tee tmp/dx12/gates/G4/correlate.txt
```
```powershell
schtasks /run /tn helios_vk_recreate      # tools/vk_surface_recreate_probe.cpp
```

**Pass criterion — numbers, not adjectives:**
* a recorded **black-frame percentage**, at or below the D3D11 0ab-C close-out figure of **0.02 %**
  (memory 64th); anything above it is a filed ROADMAP defect with a reproducer, not a soft pass;
* a recorded present→scanout distribution (median and p95), stated alongside **both** confounds so
  the number is interpretable:
  1. the P-B ~5.57 ms vehicle serialization (`helios_umd_get_present_result` returns −1
     unconditionally since R912(a), so every vehicle present takes the worker-serial
     `wait_last_present` fallback), and
  2. ⚠ **`gears.exe` presents at sync interval 2.** `demos/demo_win32.h:303` calls
     `IDXGISwapChain3_Present(swapchain, 2, 0)` — capped at **half refresh** — and `:272-273`
     take a frame-latency waitable object with `SetMaximumFrameLatency(2)`, then block on it every
     frame (`:304`). The demo is therefore *deliberately* rate-limited and *deliberately*
     2-frames-deep; a present→scanout distribution read off it measures a paced pipeline, not a
     saturated one. **Record the interval next to the number**, and if a saturated distribution is
     wanted, take the second arm from a locally-patched `gears.exe` with interval 0 (or from the
     G10 Night Raid run) and label the two separately. Comparing a `2` distribution against a
     D3D11 `0` distribution is the resolution/mode-mismatch mistake in a different costume;
* **zero** occurrences of the one-dcomp-target-per-HWND failure (`hr=0x88980800`) from
  `helios_vk_recreate`.

**Counters:** KMD pre/post diff; scanout timeline dump around the run
(`scanout_timeline_dump.exe --cursor` before, `--dump <first> <last>` after — the ring is 32 768
slots and the desktop keeps filling it, so clamp the request as `run-gt1-arm.ps1:71-78` does).

**Artifact:** `tmp/dx12/gates/G4/{cap/frames.jsonl,correlate.txt,timeline.csv,numbers.md,
wsi-perf.txt,vk_recreate.log}`.

**Known traps:**
* ⚠ **The host-side half of this gate is unavailable on the current boot.** The VM is running
  `-display sdl,gl=on` with no `-vnc` (verified: `pgrep -af qemu-system-x86_64`;
  `tools/launch-helios-gtk.sh:464-466` only adds `-vnc` for `HELIOS_DISPLAY=egl-vnc|vnc`). Both
  `vnc_shot.py` and `vnc_frame_probe.py` therefore have nothing to connect to. **Switching the
  display arm is an owner-gated QEMU relaunch** (CLAUDE.md "VM launch ownership"; memory: "Same-boot
  QEMU scanout evidence … Needs an owner-run relaunch with `HELIOS_DISPLAY=egl-vnc`"). Ask, and
  until then run the **guest-only arm**: `helios_paintcap` sampling + the scanout timeline ring,
  which gives ordering but not a black-frame %. Record which arm produced the numbers.
* ⚠ The correlator drops every blob line if a `\D*` is placed between the event name and `res`
  (`vnc_scanout_correlate.py:10-13`). That cost a whole cycle once.
* ⚠ The vehicle path is **not** the path 0ab-A/B/C were fixed on. A vkd3d client takes the vehicle
  arm of `dxgi_present` (`umd/src/forward/present.rs:1297-1308`), which copies into a DXGI
  backbuffer and lets dcomp own the flip — so it **inherits neither 0ab nor the 0ab fixes**. Its own
  analogue is the copy-vs-rerender torn-frame class, gated by
  `wsi_win32_vehicle_arm_release_gate` (`wsi_common_win32.cpp:1967-2062`). Do not import the D3D11
  black-frame mechanism wholesale.
* ⚠ `HELIOS_WSI_INSURANCE_BLIT=0` removes a per-present image→buffer blit that is ON by
  default on vehicle-serving chains. It is a legitimate A/B arm for the *numbers*, never for the
  pass criterion. ⛔ **It is not an unmeasured cost — the A/B already landed and came back inert.**
  `ROADMAP.md:2919-2926` records the owner Doom verdict run (same-process windowed→fullscreen,
  `kwait=1` + `insurance=0`): **no fps change**, `insurance_skipped 13176/13200`; `:2948-2950`
  states the conclusion, *"insurance knob keep (no measurable cost either way at Doom res — the
  copy hides under GPU latency)"*. Do not re-open it as if it were unknown. What *is* open is
  whether it stays inert at D3D12 resolutions — if you run the arm, run it for that, and say so.

---

### 4.6 `D12-G5` — Contract capture: the WARP spy-proxy log

**Entry:** G4 (or any time — this gate touches no Helios code and needs no working D3D12).

**Work:** answer the undocumented-DDI questions from a log instead of from inference. **H1**: the
D3D12 UMD DDI has ~600 auto-generated reference stubs, no Remarks, zero conceptual articles, and
there has never been a public D3D12 UMD. The mitigation is unusually good:
`C:\Windows\System32\d3d10warp.dll` **exports `OpenAdapter12`** (verified by `dumpbin /exports`).

Build a shim DLL that exports `OpenAdapter12`, forwards to WARP's, and logs:
`pfnGetCaps(Type, DataSize)` for all **43** `D3D12DDICAPS_TYPE` enumerators and their answers
(`DECISIONS.md` §4.1; `tmp/dx12/sdk/d3d12umddi.h:94-150`, values 1000–1091 with gaps — 40 carry the
`D3D12DDICAPS_TYPE_` prefix, the other 3 are `D3D12DDI_FEATURE_D3D12_PREDICATION_106`,
`..._PLACED_RESOURCE_SUPPORT_INFO_106`, `..._HARDWARE_COPY_106`; **7 are marked `// Deprecated`** —
1000, 1001, 1010, 1058, 1063, 1064, 1065 — leaving **36 live**);
every `pfnFillDDITable(TableType, TableSize, …)` call with its `SIZE_T`;
`pfnGetSupportedVersions` results; the interface/version negotiation sequence; and every
table-slot call in order for one `HelloWindow` run. About a day's work, no driver change.
Second source, already captured: `docs/dx12/research/d3d12core-driverstrings.txt` (270 lines of the
runtime's own English validation strings).

**Commands** (shape; the shim lives in `tools/`, is built with `cl`, and is pointed at by a copy of
the target exe's directory — it is **not** installed as a driver):

```powershell
$VC = 'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'
New-Item -ItemType Directory -Force -Path C:\Users\Rupansh\d12g5, Z:\tmp\dx12\gates\G5 | Out-Null
cmd /c "call `"$VC`" >nul && cl /nologo /LD /EHsc /W4 Z:\tools\d3d12_warp_spy.cpp /Fe:C:\Users\Rupansh\d12g5\d3d10warp_spy.dll"
$env:HELIOS_D12SPY_LOG = 'Z:\tmp\dx12\gates\G5\spy.log'
# run the target through a session-1 task; the harness is the same clone recipe as G3
```

**Pass criterion:** `spy.log` contains, for one complete `HelloWindow` run:
(a) the full `pfnGetSupportedVersions` / `OpenAdapter12` negotiation, (b) **every** `pfnGetCaps`
`Type` value the runtime asks and the `DataSize` it passes for each, (c) every `pfnFillDDITable`
`(TableType, TableSize)` pair, (d) an ordered call trace of the first frame.

**Which caps set the log must cover, stated as a number so this is checkable.** The shim logs every
`Type` the *runtime* asks — it does not choose. The criterion is therefore a **containment** check
against the header, not an equality check:

* the set of `Type` values in `spy.log` **⊆ the 43** enumerators of `D3D12DDICAPS_TYPE`; any value
  outside that set means the shim mis-decoded the argument, or the SDK the runtime is built against
  is newer than 10.0.26100.0 — either way, stop and resolve it;
* **the 7 deprecated enumerators are expected to be ABSENT.** A modern runtime should never ask for
  1000, 1001, 1010, 1058, 1063, 1064 or 1065. If it does, record which — that is a finding;
* for each of the **36 live** enumerators, `answers.md` records one of: *asked, with `DataSize` N*
  / *not asked during a `HelloWindow` run*. The second is a legitimate outcome (HelloWindow does
  not exercise raytracing or sampler-feedback caps) — but it must be written down per enumerator,
  because "the runtime never asked" and "we forgot to log it" look identical in an absent line.

Each of the questions listed in `DDI_REFERENCE.md` §15 ("What the header does NOT tell you") is
then either answered from the log or re-marked UNVERIFIED with the reason the log did not answer
it, and `DDI_REFERENCE.md` §11 (Caps) is updated with the observed `DataSize` per type.

**Counters:** none — WARP is the driver here, Helios is not in the path.

**Artifact:** `tmp/dx12/gates/G5/{spy.log,answers.md}` and the corresponding edits to
`docs/dx12/DDI_REFERENCE.md` §15.

**Known traps:**
* ⚠ Two questions this gate is uniquely positioned to answer, and both are cheap once the shim
  exists: **Q1** — does the runtime hand `pfnCreateShader` a DXBC container or a raw stream, per
  shader model (dump the first 8 dwords; the DDI passes **no length parameter anywhere**, verified
  `grep BytecodeLength d3d12umddi.h` → nothing). **Q2** — does the runtime cross-validate the caps
  set as one contract, the way `CDevice::LLOCompleteLayerConstruction` does for D3D11
  (`umd/src/caps.rs:39-42`)? Answer deliberately inconsistent caps and read ETW
  `Microsoft-Windows-DxgKrnl` → `AzureTriage`.
* ⛔ Do not let the shim become the start of the real UMD. `DECISIONS.md` §7.1 (R908) is the
  standing record: unreachable D3D12 scaffolding gets deleted.

---

### 4.7 `D12-G6` — Split gate: `umd_common` + `umd12` scaffolding land, D3D11 provably unchanged

**Entry:** G5 answered, or explicitly deferred with the open questions listed.

**Work:** `docs/dx12/ARCHITECTURE.md` owns the shape. This gate proves the *refactor* is inert.
`OpenAdapter12` still returns `DXGI_ERROR_UNSUPPORTED`; `helios_umd12.dll` builds, is signed,
installs, and is referenced by `UserModeDriverName[3]` — but D11 (`DECISIONS.md`) says the D3D12
path is behind `HKLM\SOFTWARE\Helios!UmdD3D12`, absent ⇒ off, so nothing changes behaviour.

**Commands:**

⛔ **`tools/umd-check.ps1` cannot build this gate's output as it stands, and fixing it is part of
G6's work — not an afterthought.** `:34` mirrors exactly two subtrees,
`foreach ($sub in @('umd', 'protocol'))`, and `:43-44` pushes into `$mirror\umd` and runs cargo
there. It can neither mirror nor build the `umd_common` rlib nor the `umd12` cdylib that D3b/D3
introduce, so its single build command **cannot produce `helios_umd12.dll`** — which this gate's own
pass criteria then require to be signed, installed and hashed in the DriverStore.

**The concrete edit, and it belongs in the same commit as the crate split:**

1. `umd-check.ps1:34` → `foreach ($sub in @('umd', 'umd_common', 'umd12', 'protocol'))`.
2. Add a second build after the existing `Push-Location $mirror\umd` block (`:43-59`):
   `Push-Location "$mirror\umd12"`, same `CARGO_TARGET_DIR`/`LIBCLANG_PATH` preamble, same
   `$cargoArgs`, log to `Z:\tmp\umd12-$Mode.log`. (`umd_common` needs no invocation of its own — it
   is a path `rlib` dependency and both cdylibs pull it in.)
3. Or, equivalently, give the script a `-Crate <umd|umd12|both>` parameter defaulting to `both`.
   Either shape is fine; what is not fine is a G6 whose build step silently produces one DLL.

```powershell
# build BOTH UMDs (requires the umd-check.ps1 edit above), and verify the D3D11 one is
# byte-identical in BEHAVIOUR -- not in bytes: the split relinks it, so the hash WILL change.
powershell -File Z:\tools\umd-check.ps1 -Mode release
# ... win_install_umd with the ABSOLUTE mirror path (C:\Users\Rupansh\helios-vgpu\umd\target\release)
Get-FileHash -Algorithm SHA256 C:\Windows\System32\DriverStore\FileRepository\<pkg>\helios_umd.dll,`
  C:\Windows\System32\DriverStore\FileRepository\<pkg>\helios_umd12.dll |
  Out-File Z:\tmp\dx12\gates\G6\deployed-hashes.txt

# the registry layout D3 mandates -- capture BOTH values, they are NOT the same shape
$k = 'HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}\0000'
(Get-ItemProperty $k).UserModeDriverName      | Out-File Z:\tmp\dx12\gates\G6\umd-driver-names.txt
(Get-ItemProperty $k).InstalledDisplayDrivers | Out-File -Append Z:\tmp\dx12\gates\G6\umd-driver-names.txt
```

**The `umd_common` extraction's own validation instrument, which this gate must run.**
`DECISIONS.md` D3b calls the extraction ordering *load-bearing* and names the instrument in the same
breath: *"`log_knob_inventory()`'s output must come out byte-identical, which is its own validation
instrument"*. Neither a Fire Strike parity run, nor a DriverStore hash list, nor the id-1000 log
detects a silently-changed knob table — only this does:

```powershell
# BEFORE the split (current shipping UMD) and AFTER, from the same workload:
#   umd/src/log.rs:235 log_knob_inventory(), called once per adapter open at umd/src/adapter.rs:219
Select-String -Path C:\ProgramData\Helios\umd-*.log -Pattern 'knob ' |
  ForEach-Object { $_.Line -replace '^\S+\s+', '' } | Sort-Object |
  Set-Content Z:\tmp\dx12\gates\G6\knobs-<before|after>.txt
Compare-Object (Get-Content Z:\tmp\dx12\gates\G6\knobs-before.txt) `
               (Get-Content Z:\tmp\dx12\gates\G6\knobs-after.txt)
```
An empty `Compare-Object` is the pass. A non-empty one means the move changed a knob's name,
default or read site — which is exactly the class D3b's ordering rule exists to prevent, and it is
cheaper to catch here than after `umd12` exists and the extraction has become a merge.

Then the D3D11 parity run: a full Fire Strike through `tmp/perf/launch-gt1-arm.ps1`, plus the
desktop evidence and the fault log.

**Pass criterion:**
* `UserModeDriverName` is a `REG_MULTI_SZ` with **exactly four** entries (DX9, DX10, DX11, DX12)
  **and `entry[3]` equals the deployed `helios_umd12.dll` DriverStore path**, while `entry[0..2]`
  all equal the deployed `helios_umd.dll` path.
  ⛔ **"Four entries" alone is not a criterion — today's shipping build already satisfies it.**
  Read live on 2026-08-05, `HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}\0000`
  (`DriverDesc` = *"Helios vGPU Render Adapter (WDDM bring-up)"*) holds four entries, **all four
  pointing at `…\helios_kmd_render.inf_amd64_3383a0e561ea9ca2\helios_umd.dll`**. A gate that only
  counts entries passes on the pre-split driver and proves nothing. Assert the *value* of index 3:

  ```powershell
  $n = (Get-ItemProperty $k).UserModeDriverName
  if ($n.Count -ne 4) { throw "UserModeDriverName has $($n.Count) entries, want 4" }
  if ($n[3] -notmatch 'helios_umd12\.dll$') { throw "entry[3] is '$($n[3])', want the deployed helios_umd12.dll" }
  if ($n[0..2] | Where-Object { $_ -notmatch 'helios_umd\.dll$' }) { throw "entries 0-2 must stay on helios_umd.dll" }
  if (-not (Test-Path -LiteralPath $n[3])) { throw "entry[3] path does not exist on disk" }
  ```
  ⛔ Never six: `D3DKMTQueryAdapterInfo(KMTQAITYPE_UMDRIVERNAME)` returns
  `STATUS_INVALID_PARAMETER` for versions 4/5 on this adapter (`DECISIONS.md` D3).
* **`InstalledDisplayDrivers` is `helios_umd,helios_umd12` — exactly TWO entries** (`DECISIONS.md`
  §6.1). It is a flat list of the distinct package binaries, **not** index-parallel to
  `UserModeDriverName`. ⚠ The live value today is `{helios_umd, helios_umd, helios_umd, helios_umd}`
  — four copies of one name, which is semantically wrong and is fixed as part of this gate's INF
  change, not filed as a separate item. A four-entry `InstalledDisplayDrivers` after G6 is a
  failure even if `UserModeDriverName` is correct.
* `D3D12CreateDevice` on the Helios adapter still fails — the kill switch is absent, so the D3D12
  path is bit-identical to a build without it.
* **`log_knob_inventory()` output byte-identical before and after the split** (empty
  `Compare-Object` above). This is the `umd_common` extraction's own instrument and the only one in
  this gate that can see a silently-changed knob table.
* **Fire Strike Graphics at parity with the pre-split baseline (≈ 49k), 3-run median**, and GT1/GT2
  within the known ±5–6 % single-run spread.
* `umd-gate-surface.ps1` on one dwm session: unchanged counter set.
* Zero `helios_umd.dll` entries in `Get-WinEvent -FilterHashtable
  @{LogName='Application';Id=1000;StartTime=$boot}`.

**Counters:** KMD unchanged (no KMD image change in this phase); UMD gate surface unchanged;
`tools/kmd-frame-sizes.ps1` **not** required unless the KMD image changed.

**Artifact:** `tmp/dx12/gates/G6/{deployed-hashes.txt,umd-driver-names.txt,knobs-before.txt,
knobs-after.txt,firestrike-medians.md,umd-gate.txt,app-faults.txt}`.

**Known traps:**
* ⚠ **A picture on screen does not prove the compositor is alive.** On the direct primary the KMD
  keeps scanning out the last composited buffer, so a crash-looping dwm looks like a *static,
  correct desktop*. Always quote the id-1000 Application log before calling any display observation
  a pass.
* ⚠ **A warm `pnputil /restart-device` is not verification** — it never re-exercises LogonUI or a
  cold device create. That is how a black screen shipped once.
* ⚠ Expect id-1000 faults in `vulkan_virtio-*.dll` for dwm/Explorer/SearchHost/ApplicationFrameHost
  after any restart-device (WS1 defect 0z). `helios_umd.dll` must be zero.
* ⚠ `tools/umd-check.ps1 -Mode release` builds into the **mirror**
  (`C:\Users\Rupansh\helios-vgpu\umd\target\release`). That absolute path, not the packaging task,
  decides which DLL reaches the DriverStore. Verify the deployed hash — Defender has silently
  blocked the copy before.

---

### 4.8 `D12-G7` — DDI device gate: `D3D12CreateDevice` succeeds through `helios_umd12.dll`

**Entry:** G6.

**Work:** adapter funcs, the caps answer, `CreateDevice`, and the vkd3d bridge.

**`DECISIONS.md` D4 specifies TWO added exports on the Helios-built `helios_vkd3d.dll`, not one** —
both must exist before the bridge compiles, so add them together:

```c
HRESULT helios_vkd3d_create_device(LUID adapter_luid, REFIID iid, void **device);
HRESULT helios_vkd3d_serialize_root_signature(const D3D12_ROOT_SIGNATURE_DESC *desc,
                                              D3D_ROOT_SIGNATURE_VERSION version,
                                              ID3DBlob **blob, ID3DBlob **error_blob);
```

The first calls `vkd3d_create_instance` + `vkd3d_create_device` (`include/vkd3d.h:104,110`)
**directly**. The second wraps `vkd3d_serialize_root_signature` (`include/vkd3d.h:129`), which
exists in the library but is **exported from no vkd3d DLL** — `libs/d3d12core/d3d12core.def`
exports exactly two symbols, `D3D12GetInterface` and the `D3D12SDKVersion` data symbol. It is
needed because `d3d12umddi` delivers root signatures **already parsed** as
`D3D12DDI_ROOT_SIGNATURE` while vkd3d's `CreateRootSignature` wants a serialized DXBC `RTS0` blob
(H3), so the UMD must re-serialize. G8 depends on it; adding it here costs nothing and avoids a
second `.def` change mid-gate.

**Why the exports exist at all — the DXGI-reentrancy hazard, stated precisely.** ⚠ The exported
`D3D12CreateDevice` is **not** in `libs/d3d12core/main.c`; it lives in `libs/d3d12/main.c:143`, the
separate thin `d3d12.dll` target that **Helios does not use at all**. Inside `d3d12core.dll` the
DXGI-touching path is `d3d12core_CreateDeviceFromFactory` (`libs/d3d12core/main.c:643`), reachable
only through `D3D12GetInterface`, which calls `CreateDXGIFactory1` at `:383` and `:406` to resolve
the adapter. A WDDM UMD sits *below* DXGI and must not depend on `dxgi.dll` (`umd/build.rs:240-243`
states the rule for the D3D11 side and D3D12 inherits it), with the added hazard that a UMD loading
dxgi during device creation can re-enter adapter enumeration that loads the UMD. **The new exports
exist precisely to bypass `d3d12core_CreateDeviceFromFactory`.**

⛔ `OpenAdapter12` stops refusing **in the same commit** that makes its body reachable
(`DECISIONS.md` §7.1).

**Commands:**

```powershell
$G='Z:\tmp\dx12\gates\G7'; New-Item -ItemType Directory -Force -Path $G | Out-Null
$T='C:\Users\Rupansh\d12g7'; New-Item -ItemType Directory -Force -Path $T | Out-Null
$VC='C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'
reg add HKLM\SOFTWARE\Helios /v UmdD3D12 /t REG_DWORD /d 1 /f     # D11 kill switch ON
& Z:\tools\kmd-counter-snapshot.ps1 -Label G7-pre -OutDir $G

cmd /c "call `"$VC`" >nul && cl /nologo /EHsc /W4 Z:\tools\d3d12_devicecreate_probe.cpp /Fe:$T\devcreate.exe /link d3d12.lib dxgi.lib dxguid.lib"
& $T\devcreate.exe | Tee-Object -FilePath $G\devicecreate.txt

# the same one-test arm as G1, but with NO vkd3d DLLs beside the exe -> system d3d12.dll -> us
$env:VKD3D_TEST_MATCH='test_create_device'
& C:\Users\Rupansh\d12g1\d3d12.exe --adapter <N> 2>&1 | Tee-Object -FilePath $G\create_device.txt

# the caps answer must match the app-local baseline
& C:\Users\Rupansh\d12g1\caps.exe > $G\caps-ddi.csv
Compare-Object (Get-Content Z:\docs\dx12\baselines\d3d12-caps.csv) (Get-Content $G\caps-ddi.csv) |
  Out-File $G\caps-diff.txt

& Z:\tools\kmd-counter-snapshot.ps1 -Label G7-post -OutDir $G
& Z:\tools\kmd-gate-surface.ps1
& Z:\tools\umd-gate-surface.ps1 -AllProcesses -SinceMinutes 30 | Out-File $G\umd-gate.txt
```

**Pass criterion:**
* `devicecreate.txt` reports `S_OK` from `D3D12CreateDevice` on the **Helios adapter selected by
  description**, at FL 11_0.
* `create_device.txt` reads `<N> tests executed (0 failures, …, 0 skipped, …)` — with **no vkd3d
  DLLs in the directory**. That is the whole gate. ⛔ `<N>` is the assertion count, **not** `1`
  (§1 rule 7): compare it against the `executed` number captured in
  `tmp/dx12/gates/G1/triple.txt`. `failures == 0 && skipped == 0 && executed == G1's executed` is
  the criterion; an `executed` lower than G1's with `failures == 0` means the DDI arm returned
  early out of the test body and is a defect, not a pass.
* `caps-diff.txt` is **empty**, or every line is an intentional, justified divergence recorded in
  `notes.md` with the reason. An unexplained divergence is the caps-honesty failure this gate
  exists to catch.
* Setting `UmdD3D12` back to 0 (or deleting it) restores the refusal exactly.

**Counters:** KMD pre/post diff, `kmd-gate-surface.ps1` exit 0. **`HwQRef` must not move** — a
D3D12 device must never reach `DxgkDdiCreateHwQueue` (which refuses at
`kmd_render/src/ddi/scheduler.rs:180-187`); the D3D12 caps answer must therefore report
`HARDWARE_SCHEDULING_CAPS_0050.ComputeQueuesPer3DQueue = 0`. The new D3D12 refusal counters
(`umd12`'s analogue of the eleven at `umd/src/forward.rs:331-385`) must be readable and recorded
even when zero.
**K1 lands here** (§4.13): the D3D12 device is the first client that creates contexts through a
second UMD, so this is the gate that reads the new `CtxNode` counter. Record it in both snapshots;
`CtxNode = 0` at G7 is the evidence that today's callers all pass `NodeOrdinal = 0`, which is the
precondition D5/K1 requires before the *refusal* half may ship.

**Artifact:** `tmp/dx12/gates/G7/{devicecreate.txt,create_device.txt,caps-ddi.csv,caps-diff.txt,
umd-gate.txt,kmd-counters-G7-{pre,post}.txt,notes.md}`.

**Known traps:**
* ⚠ **Honour `pfnFillDDITable`'s `SIZE_T`.** Never write `size_of::<T>()` bytes. R702 class: 24H2
  passed 576 bytes for a 592-byte `DRIVERCAPS`, and D3D12 parameterises the size explicitly.
* ⚠ **The struct-return ABI on descriptor handles.** `pfnGetCPU/GPUDescriptorHandleForHeapStart`
  return by value; vkd3d's C implementation returns via hidden pointer. Same class as the
  `bridge_guard` truncation (`ead692e`) that crash-looped dwm and LogonUI at cold boot.
* ⚠ **Declining an unimplemented interface is `DXGI_ERROR_UNSUPPORTED` (0x887A0004)**, never
  `DXGI_ERROR_DRIVER_INTERNAL_ERROR` (0x887A0020) — the latter is recorded by the runtime and ETW
  as a driver fault.
* ⚠ The runtime enumerates ~60 consistency rules across the **43** caps types (`DECISIONS.md` §4.1;
  36 of them live, 7 deprecated) and says so in English:
  `"Driver did not respond to D3D12DDICAPS_TYPE_D3D12_OPTIONS caps query."`,
  `"Driver did not report any supported shader models…"`,
  `"Driver did not set valid WaveLaneCountMin/Max or TotalLaneCount…"`,
  `"Drivers that support raytracing must expose shader model 6.3."`
  (`docs/dx12/research/d3d12core-driverstrings.txt`). Read them **before** guessing at a caps
  failure.
* ⚠ dwm already calls `OpenAdapter12` in production. The first boot with `UmdD3D12=1` is a change
  to the compositor. Have the id-1000 log open.

---

### 4.9 `D12-G8` — DDI first frame: a triangle through the DDI, owner-visible

**Entry:** G7.

**Work:** the 99 real-body slots (`DDI_REFERENCE.md` §14.2; 87 of them if the 12 immutable
pipeline sub-state slots are excluded, which is where the older "~86" came from) — descriptor heaps, root signatures (⚠ they arrive **parsed** as
`D3D12DDI_ROOT_SIGNATURE`; vkd3d wants a serialized `RTS0` blob, so the UMD must re-serialize
through **`helios_vkd3d_serialize_root_signature`**, the second of D4's two added exports, added at
G7 — `vkd3d_serialize_root_signature` itself (`include/vkd3d.h:129`) is exported from no vkd3d DLL,
`libs/d3d12core/d3d12core.def` exporting only `D3D12GetInterface` and `D3D12SDKVersion`), PSOs
(handle bundles that must be reassembled into a `D3D12_GRAPHICS_PIPELINE_STATE_DESC`), resources
(`pfnCreateHeapAndResource` with two independently-nullable argument pointers), command recording,
`ExecuteCommandLists`, fences, and present.

**P-C, as `DECISIONS.md` §6.1 resolved it — narrower than it first looked, and it needs NO KMD
change.** `pfnPresent` does reach the driver: `PFND3D12DDI_PRESENT_0051` sits on the *command-list*
table (`tmp/dx12/sdk/d3d12umddi.h:7250`), takes `D3D12DDIARG_PRESENT_0001`, and **outputs** the
src/dst `D3DKMT_HANDLE`s and the context. It is true that neither `pfnPresentCb` nor `pfnRenderCb`
is declared in `d3d12umddi.h` itself — **but that is not the whole callback surface.**
✅ `D3D12DDIARG_CREATEDEVICE_0109.pKTCallbacks` (`d3d12umddi.h:13623`) is a
`CONST D3DDDI_DEVICECALLBACKS*` — **the same 65-entry kernel thunk table the D3D11 UMD already
drives** (`tmp/dx12/sdk/d3dumddi.h:4499`) — and it **contains both `pfnRenderCb` and
`pfnPresentCb`** (verified).

**Consequence for this gate: the identity channel transfers unchanged.** The D3D12 UMD writes a
`HeliosPresentRenderCmd` and calls `pfnRenderCb` exactly as `umd/src/forward/present.rs:795` does,
landing in the KMD's **PASSIVE** `dxgkddi_render` path and its per-context stash. Everything from
`dxgkrnl` down — the flip arm, `PresentFlipPrivate`, `set_scanout_blob` — is reused as-is. There is
nothing to "rebuild".

⛔ **Do not design a `DxgkDdiSubmitCommandVirtual` decode for the identity.** That DDI runs at
**DISPATCH_LEVEL** (`kmd_render/src/ddi/submit_command.rs:723-724`, *"Runs at DISPATCH_LEVEL"*),
where the stash machinery's `diag::record*` registry writes are illegal (CLAUDE.md's first
invariant), and it would add a **fourth** KMD work item that `DECISIONS.md` D5 does not have.
`pfnRenderCb` is the recommendation, and it is the only one.

⚠ **The one thing still unverified** is whether the D3D12 *runtime* tolerates the driver calling
`pfnRenderCb` around `pfnPresent` (§7.19). Settle it in this gate, before G8's later rungs depend
on it: `pfnRenderCb` plus a counting `DxgkDdiRender` on the D3D12 path, and confirm the count moves.

**Commands** — three rungs, in order, and do not skip rung 0:

```powershell
# rung 0: headless pixel correctness, no swapchain, no present path involved
$T='C:\Users\Rupansh\d12g8'; New-Item -ItemType Directory -Force -Path $T,Z:\tmp\dx12\gates\G8 | Out-Null
$VC='C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'
cmd /c "call `"$VC`" >nul && cl /nologo /EHsc /W4 Z:\tools\d3d12_clear_probe.cpp /Fe:$T\clear.exe /link d3d12.lib dxgi.lib dxguid.lib"
& $T\clear.exe | Tee-Object -FilePath Z:\tmp\dx12\gates\G8\clear.txt

# rung 1: our own windowed triangle, explicit adapter, session 1 task (clone recipe as G3)
# rung 2: dx-samples HelloWindow then HelloTriangle, session 1
```

Evidence: `schtasks /run /tn helios_paintcap` twice, ≥ 2 s apart, into
`tmp/dx12/gates/G8/shot-{1,2}.png`.

**Pass criterion:**
* rung 0 — `clear.exe` reads back the **exact** clear colour at pixel 0. This is a number, and it
  proves rendering independently of presentation.
* rung 1/2 — an **owner-visible triangle** in `Z:\tmp\screen_copy.png`, plus `HelloWindow`'s clear
  colour verified by sampling the screenshot. **The number, stated so this is executable:**

  | Quantity | Value | Source |
  |---|---|---|
  | float clear colour | `{ 0.0f, 0.2f, 0.4f, 1.0f }` | `dx-samples-research-only/Samples/Desktop/D3D12HelloWorld/src/HelloWindow/D3D12HelloWindow.cpp:210` |
  | back-buffer format | `DXGI_FORMAT_R8G8B8A8_UNORM` | same file, `:89` — **UNORM, not `_SRGB`**, so the encode is a plain ×255 with no gamma curve |
  | client size | 1280 × 720 | `…/HelloWindow/Main.cpp:18` |
  | **expected 8-bit pixel** | **RGB (0, 51, 102)** | 0.0·255 = 0, 0.2·255 = **51.0**, 0.4·255 = **102.0** — all three land exactly on integers, so there is no rounding slack to argue about |

  **Where to sample and what passes:** take the **modal** RGB of a 32 × 32 patch at the centre of
  the window's client area (modal, not mean — a mean is corrupted by one stray border pixel).
  **Pass = exactly (0, 51, 102).** A ±1-per-channel result is a *soft* pass that must be recorded
  with the deviation in `notes.md`; anything outside ±1 is a colour-pipeline defect, not tolerance,
  and is filed.

  ```powershell
  Add-Type -AssemblyName System.Drawing
  $b = [System.Drawing.Bitmap]::FromFile('Z:\tmp\dx12\gates\G8\shot-1.png')
  $cx = <client-centre-x>; $cy = <client-centre-y>; $h = @{}
  foreach ($dx in 0..31) { foreach ($dy in 0..31) {
    $p = $b.GetPixel($cx-16+$dx, $cy-16+$dy); $k = "$($p.R),$($p.G),$($p.B)"
    $h[$k] = 1 + $h[$k] } }
  $h.GetEnumerator() | Sort-Object Value -Descending | Select-Object -First 3
  ```

  ⚠ **Bit-exactness through DWM/paintcap is expected but is itself part of what is being tested.**
  The composed SDR desktop applies no colour conversion to an `R8G8B8A8_UNORM` source, and
  `Graphics.CopyFromScreen` reads the composed primary, so `(0, 51, 102)` should survive intact. If
  it does not, that is a finding about the *composition* path — record which of the two rungs
  disagrees (rung 0's readback is the control: it never touches DWM), do not widen the tolerance
  until it passes.
* **no `DDI refusals:` line in the `umd12` log that was not there before**, and the noop-DDI hit
  count recorded.

**Counters:** KMD pre/post diff, `kmd-gate-surface.ps1` exit 0, `Sc*` family clean (except
pre-existing `ScStale`), `HwQRef` unmoved; `umd-gate-surface.ps1 -AllProcesses` for both UMDs.

**Artifact:** `tmp/dx12/gates/G8/{clear.txt,shot-1.png,shot-2.png,umd12-*.log,timeline.csv,
kmd-counters-G8-{pre,post}.txt}`.

**Known traps:**
* ⚠ **Many D3D12 DDIs return `VOID`.** Errors go out through `pfnSetErrorCb` /
  `pfnSetCommandListErrorCb`, not a return value. A `panic!`/`todo!`/`unwrap` in any DDI is a
  silent graphics deadlock.
* ⚠ `D3D12_FEATURE_LEVEL_11_0` + SM 5.1 is a *DDI* floor and not a runnable one: even
  `HelloTriangle` compiles at `-Tvs_6_0`. Aim at FL 11_0 + SM 6.0.
* ⚠ Rung 0 exists because a failure at rung 1 with no rung-0 result cannot be attributed. Run it.
* ⚠ If the frame is black, check `HelloWindow` before `HelloTriangle` — it has **no shaders at
  all**, so it isolates device/queue/present from the DXIL path entirely.

---

### 4.10 `D12-G9` — DDI conformance: the same suite against the system `d3d12.dll`

**Entry:** G8.

**Work:** re-run G2's exact command with **the vkd3d DLLs removed from the test directory**, so the
harness resolves the system `d3d12.dll` and the runtime drives `helios_umd12.dll`. Drive the delta
against the G2 baseline to zero. Then re-run the caps and format matrices and diff.

**Commands:** identical to §4.3, with one change and two additions:

```powershell
Remove-Item C:\Users\Rupansh\d12g1\d3d12.dll,C:\Users\Rupansh\d12g1\d3d12core.dll -ErrorAction SilentlyContinue
& 'C:\Program Files\Git\bin\bash.exe' -c `
  "cd /c/Users/Rupansh/d12g1 && /z/vkd3d-proton-helios/tests/test-runner.sh -o /z/tmp/dx12/gates/G9/logs -j 1 ./d3d12.exe"
# then the same summary.csv / triple.txt reduction as G2

# additions available ONLY on this arm: the real D3D12 debug layer
& C:\Users\Rupansh\d12g1\d3d12.exe --validate --adapter <N> ...
& C:\Users\Rupansh\d12g1\d3d12.exe --gbv      --adapter <N> ...

$T='C:\Users\Rupansh\d12g9'; New-Item -ItemType Directory -Force -Path $T | Out-Null
$VC='C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'
cmd /c "call `"$VC`" >nul && cl /nologo /EHsc /W4 Z:\tools\d3d12_format_matrix_probe.cpp /Fe:$T\fmt.exe /link d3d12.lib dxgi.lib dxguid.lib"
& $T\fmt.exe > Z:\tmp\dx12\gates\G9\format-matrix.csv
```

**Pass criterion:** **parity with G2.** `failures == 0` and `skipped` within the G2 baseline, with
every remaining failing name already in `docs/dx12/baselines/vkd3d-known-fail.txt`. Any new failing
name is a DDI defect; any increase in `skipped` is a capability the DDI arm lost.
`caps-diff` against `docs/dx12/baselines/d3d12-caps.csv` empty or fully justified.

⛔ **`nosummary == 0` — and S1 must be closed for that to be honest.** A crashed test writes no
summary, so a `nosummary` hit here is not a parity result at all. The S1 shared-heap hazard
(G2's first trap: `libs/vkd3d/resource.c:4405-4429` chaining `VkExportMemoryAllocateInfo` with no
`VK_KHR_external_memory_win32` present, then calling a NULL `vkGetMemoryWin32HandleKHR`) reaches
`test_map_texture_validation` and `test_open_heap_from_address`, both inside this run.
**This gate may not be called conformance while S1 is open** — it must have been fixed (the memory
twin of the ICD's native semaphore import) or fenced (vkd3d refusing `D3D12_HEAP_FLAG_SHARED` up
front with a named counter) at G2, and this gate quotes which.

**Counters:** as G2, plus the `umd12` refusal counters driven to zero (this is the D3D12 analogue
of `CONFORMANCE.md`'s charter for D3D11).

**Artifact:** `tmp/dx12/gates/G9/{logs/*.log,summary.csv,triple.txt,caps-diff.txt,
format-matrix.csv,validate.txt,gbv.txt,umd-gate.txt}`.

**Known traps:**
* ⚠ **The D3D12 debug layer is a G9-only instrument.** `C:\Windows\System32\d3d12SDKLayers.dll` is
  present, so `--validate`/`--gbv` work against the system `d3d12.dll`. Under vkd3d they silently
  do nothing: `libs/d3d12core/main.c:783-805` returns `DXGI_ERROR_SDK_COMPONENT_MISSING` for every
  IID except `IID_ID3D12DeviceRemovedExtendedDataSettings`, and the harness only enables the layer
  `if (SUCCEEDED(...))`. **This is a standing argument for keeping the G2 app-local arm alive
  permanently** — the two arms have disjoint instruments.
* ⚠ `D3D12EnableExperimentalFeatures` returns `E_NOINTERFACE` under vkd3d
  (`libs/d3d12core/main.c:807-813`), so "experimental shader models" is not a knob on the G2 arm.
  On the G9 arm it is real, which changes what a test can ask for. Note which arm a result came
  from.
* ⚠ A skip-count *decrease* relative to G2 is not automatically good — check it is not the harness
  silently taking a different path (e.g. `--warp` left set).

---

### 4.11 `D12-G10` — Real workload: 3DMark Night Raid, then Time Spy Graphics

**Entry:** G9 (or G4 for the app-local arm — G10 is run once per arm and the results are labelled;
the app-local arm's number is the control the DDI arm must not be worse than).

**Work — the requirement ladder, and why Night Raid is the milestone.** Night Raid is the DX12
engine optimised for integrated graphics (1 GB VRAM); Time Spy is DX12 at feature level 11_0 (4 GB
VRAM); Speed Way needs DX12 Ultimate (DXR 1.1 + mesh shaders, 6 GB). Ordering by driver demand:
**Night Raid → Time Spy → Steel Nomad (DX12) → Port Royal / Solar Bay (DXR) → Speed Way**.

* **`NightRaidGt1P` completing with a score and an owner-visible frame is the milestone.** It is the
  lowest-demand installed D3D12 workload, its failures are most likely to be real driver defects
  rather than missing tiers, and it ships a **Win32** build that gives a free WOW64 arm — blocked
  today by S2 (`HKLM\SOFTWARE\WOW6432Node\Khronos\Vulkan\Drivers` does not exist, so a 32-bit
  client finds zero physical devices; either ship a 32-bit venus ICD or declare 64-bit-only in
  writing).
* **Time Spy Graphics is the headline number** — the D3D12 analogue of Fire Strike Graphics ≈ 49k
  and directly comparable in kind. It is the *second* result, not the milestone.
* The feature tests (`meshshaderft`, `samplerfeedbackft`, `vrs`, `vrs2`, `directxraytracingft`) are
  **tier probes, not benchmarks** — run them first, cheaply, to find out what is actually backed
  before attempting a benchmark that needs a tier.

**Commands:**

```powershell
# 1. a single-graphics-test definition, in the shape of tmp/perf/fs_gt1.3dmdef
@'
<?xml version="1.0" encoding="utf-8"?>
<benchmark>
  <application_info>
    <selected_workloads>
      <selected_workload name="NightRaidGt1P"/>
    </selected_workloads>
  </application_info>
</benchmark>
'@ | Set-Content -Encoding utf8 Z:\tmp\dx12\gates\G10\nightraid_gt1.3dmdef

# 2. app-local arm ONLY: per-workload DLL drop, one-file-delete rollback
$NR = 'C:\ProgramData\UL\3DMark\chops\dlc\night-raid-test\bin\x64'
Copy-Item Z:\tmp\dx12\build\vkd3d-win64\libs\d3d12\d3d12.dll            $NR\
Copy-Item Z:\tmp\dx12\build\vkd3d-win64\libs\d3d12core\d3d12core.dll    $NR\
# ⛔ NO dxvk dxgi.dll and NO vkd3d d3d12*.dll — under DECISIONS D2 the workload runs on the
# system d3d12.dll + dxgi.dll and reaches Helios through UserModeDriverName[3].

# 3. run through a run-gt1-arm.ps1-shaped wrapper, in session 1
powershell -File Z:\tmp\perf\launch-gt1-arm.ps1 -Label d12-nightraid-1 `
           -Def Z:\tmp\dx12\gates\G10\nightraid_gt1.3dmdef
```

⛔ **The unmodified wrapper cannot extract a Night Raid or Time Spy score — fix this before run 1.**
`launch-gt1-arm.ps1:16-24` clones the task and forwards to `run-gt1-arm.ps1`, which calls
`run-fs.ps1` at `:59`. `run-fs.ps1`'s **only** fps extraction is

```powershell
# tmp/perf/run-fs.ps1:149
$fps = Select-String -Path $log -Pattern '\[local\] (gt1|gt2|combined|physics) = '
```

— **Fire-Strike-specific label keys.** A Night Raid or Time Spy run through this wrapper writes a
report containing a duration, an exit code and **zero score lines**, which is indistinguishable from
the "Graphics = 0 with a result file" failure this gate's own trap warns about. §3.1 says *"a D3D12
gate runner is this file with the workload line swapped"*; the label line has to be swapped too.

**Two acceptable fixes — pick one and record which in `medians.md`:**

1. **Widen the regex** (smallest change): parameterise the alternation, e.g.
   `'\[local\] (gt1|gt2|combined|physics|graphics|cpu) = '`, after reading one Night Raid
   `3DMark.log` to learn the actual `[local]` keys that workload emits. ⚠ Do not guess the keys —
   run once, grep `'\[local\]'`, then write the regex.
2. **Parse `Result.xml` out of the `.3dmark-result` zip** (authoritative, and the gate already tells
   you to do this in the traps below):

   ```powershell
   Add-Type -AssemblyName System.IO.Compression.FileSystem
   $z = [System.IO.Compression.ZipFile]::OpenRead($resultPath)
   $e = $z.Entries | Where-Object { $_.Name -eq 'Result.xml' }
   $sr = New-Object System.IO.StreamReader($e.Open()); [xml]$r = $sr.ReadToEnd(); $z.Dispose()
   $r.SelectNodes('//Result//*[contains(local-name(),"Score")]') | ForEach-Object { "$($_.Name) = $($_.InnerText)" }
   ```
   ⚠ `Expand-Archive` silently produces nothing on a `.3dmark-result` — this is why.

Repeat for runs 2 and 3, then `TimeSpyGt1P`, then the full `timespy.3dmdef`.

**Pass criterion:**
* **Night Raid:** the run completes with **duration > 0 and a score present**, a screenshot shows
  the workload, all KMD failure counters at 0, and no `DDI refusals:` line that was not there
  before. ⚠ A Fire Strike run can report Graphics = 0 and still write a result file — **print the
  duration** (memory 64th; a real full Fire Strike is ~6.3 min).
* **Time Spy:** a recorded Graphics score as a **3-run median with its spread**. ⛔ Never a single
  run — GT scores swing ±5–6 % on identical code, and GT1 drifts across a session, so arms must be
  interleaved, not run all-A-then-all-B (`tmp/perf/ab-presentwmk.ps1`, `ab-env.ps1`).

**Counters:** the full `run-gt1-arm.ps1` capture set — KMD pre/post, read ledger pre/post, scanout
timeline slice, the newest `umd-*.log`, `HELIOS_PERF` venus line, guest CPU CSV.
**K2 and K3 are decided here** (§4.13). K2's paired A/B rides this gate's interleaved arms —
it is a KMD change to the per-context allocation list, so it may not be measured all-A-then-all-B.
K3's trigger is a *reading*: capture `QueryVideoMemoryInfo` from `tools/vram_report_probe.cpp`
during the run and compare the D3D12 budget against the 64 MiB `ApertureSegmentCommitLimit`. No
squeeze in the reading ⇒ K3 closes as "measured, no change needed", which is a result, not a skip.

**Artifact:** `tmp/dx12/gates/G10/{nightraid_gt1.3dmdef,runs/*.3dmark-result,medians.md,
shot-*.png,counter-diffs/,timeline.csv}`.

**Known traps:**
* ⛔ **A frozen benchmark is a defect to root-cause, never a retry.**
* ⛔ **Never launch 3DMark from `win_exec`.** `3DMarkCmd` from session 0 fakes a driver regression
  (memory 60th).
* ✅ **The Night Raid workload name is SETTLED — `NightRaidGt1P` is correct.** Read on the VM
  2026-08-05: `Get-Content 'C:\Program Files\UL\3DMark\nightraid.3dmdef'` lists exactly four
  entries — `NightRaidDemoP`, **`NightRaidGt1P`**, `NightRaidGt2P`, `NightRaidCpuP`. (Formerly
  §7.11; closed.) The trap it replaces still stands as a *rule*: never run three arms against a
  definition whose `<selected_workload>` you have not read out of the shipped `.3dmdef` first — a
  definition that selects nothing still writes a result file.
* ⚠ `Expand-Archive` silently produces nothing on a `.3dmark-result`. Read it with
  `System.IO.Compression.ZipFile` and parse `Result.xml`.
* ⚠ **Resolution and mode must match** for any fps comparison across arms.
* ⚠ A stray `UmdTrace=1` fakes a ~9 % cadence regression. Check the knob before believing a delta.
* ⚠ On the app-local arm, DXVK's DXGI becomes the app's view of the display. `IDXGIOutput::
  GetDisplayModeList` on the Helios output was a real blocker for a benchmark once (31st session).

---

### 4.12 `D12-G11` — Stability + packaging + CI

**Entry:** G10.

**Work:** the CLAUDE.md stability list, then the shipping surface. Stability is non-negotiable and
is not graded on a curve.

| Item | How it is exercised | Status on this box |
|---|---|---|
| **Buffer rotation** | a 120 s `gears.exe` / Night Raid loop with the scanout timeline ring dumped; every backbuffer index must appear | testable |
| **Device restart** | `pnputil /restart-device` with a D3D12 client live, then again with it dead | testable — ⚠ expect id-1000 faults in `vulkan_virtio-*.dll` for dwm/Explorer/SearchHost (WS1 0z); `helios_umd12.dll` must be **zero** |
| **DWM recovery** | `Stop-Process dwm`, confirm the desktop returns and a D3D12 client survives or fails cleanly | testable |
| **TDR** | provoke a hang and confirm the recovery path; the Xid-109 class already has a clean device-restart recovery ×2 with no QEMU relaunch | testable, trap-armed only |
| **Cold boot** | full guest reboot, then LogonUI + desktop + one D3D12 client | testable — ⚠ **guest reboots are pre-authorised; check the screen after every boot** |
| **Resize** | ⚠ **partially untestable.** `ChangeDisplaySettingsEx` cannot change resolution on this box (this is what blocks the WS1 resize item). *Reachable substitute:* window resize + fullscreen transitions via `tools/vk_surface_recreate_probe.cpp` and a windowed→fullscreen D3D12 client, which is the surface-recreate shape that actually broke the dcomp target cache | partial |
| **Suspend/resume** | ⛔ **UNTESTABLE ON THIS BOX. Stop trying.** `powercfg /a` reports S1, S2, S3, hibernate and S0ix all unsupported by the VM firmware. Consequence beyond the gate: the same-context PnP stop/start carry-over path (`StRst`, `RfUnb`) can never be provoked here — **`StRst=0` means "never exercised", not "clean"** | untestable — record as such, do not claim a pass |

**Packaging commands:**

```powershell
# 1. the fifth smoke probe
#    - packaging/windows/probes/d3d12-smoke.cpp   (new)
#    - ci/windows/Build-SmokeTests.ps1            (new cl.exe line, pattern at :18-25)
#    - ci/windows/Assemble-Package.ps1:76         (add "d3d12-smoke.exe" to the loop)
#    - packaging/windows/Verify-Helios.ps1:70-75  (fifth entry in $tests)
powershell -File Z:\packaging\windows\Verify-Helios.ps1 -RunSmokeTests |
  Tee-Object -FilePath Z:\tmp\dx12\gates\G11\verify.txt
```

**CI:** §6.

**Pass criterion:**
* every **testable** row above passes, with the evidence named per row (screenshot for the visual
  ones, counter diff for the rest), and the two non-testable rows recorded as non-testable **in
  ROADMAP**, not silently omitted;
* `Verify-Helios.ps1 -RunSmokeTests` exits 0 **with the D3D12 probe actually present and run**;
* ⛔ **fix `Verify-Helios.ps1:78-80` first.** A missing probe is currently a `Write-Warning` +
  `continue`, so this gate can pass on a bundle containing no D3D12 probe at all
  (`CONFORMANCE.md` C8). Missing must fail when `-RunSmokeTests` was explicitly requested.
* the CI job builds and uploads the vkd3d artifacts with SHA256s, and the `package` job records the
  vkd3d commit next to `mesa`/`dxvk` in the metadata step
  (`.github/workflows/windows-stack.yml:218-225`).

**Counters:** a KMD counter diff per stability item, plus `kmd-gate-surface.ps1` exit 0 after each.
`tools/kmd-frame-sizes.ps1` if any KMD image changed (K1/K2) — the boot path has **368 bytes** of
headroom; compare the frame *number*, not pass/fail.

**Artifact:** `tmp/dx12/gates/G11/{stability/<item>/…,verify.txt,ci-run-url.txt,
package-manifest.txt}` and the ROADMAP entries for the two untestable rows.

**Known traps:**
* ⚠ Shipping a `d3d12.dll` in the bundle is **not** a system-wide install and must never become
  one. Replacing the system D3D12 runtime for every process is a far larger blast radius than any
  Helios component has today. Per-app drop or explicit opt-in only.
* ⚠ D11's kill switch must survive packaging: a fresh install with no `UmdD3D12` value is
  bit-identical to a build without the D3D12 path.
* ⚠ A new KMD image only loads at **boot**; `restart-device` cannot enable the S-ring
  (`DiagLevel` is cached at driver load).

---

### 4.13 `K1` `K2` `K3` — the three KMD work items, and the gate that owns each

`DECISIONS.md` D5 names three KMD items and says none is on the critical path;
`KMD_IMPACT.md` §14 costs them. Neither assigns them to a gate, and an item that falls outside every
gate is an item that never gets a "done". **This subsection is that assignment.** None of the three
is a gate of its own — each is a criterion inside a gate that was going to run anyway.

| # | Item | Owning gate | What "done" is, concretely | Why there |
|---|---|---|---|---|
| **K1** | Validate `NodeOrdinal`/`EngineAffinity` in `DxgkDdiCreateContext`, count refusals as **`CtxNode`** | **G7**, read again at **G9** | Two-step, in this order: (1) the **counter** ships and `CtxNode` is recorded in G7's pre/post diff — `CtxNode = 0` across G7 **and** G9 is the evidence that every live caller passes node 0; (2) only then may the **refusal** ship, and it re-runs G7 + a Fire Strike parity run to prove DWM's contexts are unaffected. ⛔ Shipping the refusal before the counter has moved-or-not-moved on a real workload is a new refusal on a live path with no evidence behind it | G7 is the first gate where a second UMD creates contexts, so it is the first reading that is not just DWM |
| **K2** | `ContextInfo.Caps.NoPatchingRequired = 1` + shrink `AllocationListSize`/`PatchLocationListSize` for `VirtualAddressing` contexts | **G10** | Behind a knob (default OFF), with a **paired, interleaved** GT1/GT2 A/B in the shape of `tmp/perf/ab-presentwmk.ps1` — never all-A-then-all-B, because GT1 drifts across a session. Done = the paired delta is recorded with its spread, the default is set to whichever value was measured, and the opposite value stays reachable as the disable (CLAUDE.md rule 8) | It touches the **Present allocation list**, i.e. every client including DWM. D5 explicitly flags it as "knob + paired A/B", and G10 is the only gate that runs interleaved arms |
| **K3** | Revisit `ApertureSegmentCommitLimit` (64 MiB) | **G10** (measurement), **G7** (first reading) | Done = a **number**, not a change. Capture `QueryVideoMemoryInfo` (`tools/vram_report_probe.cpp` — §3.1 already names it as the natural home for a D3D12 arm) at G7 once a device exists, and again under load at G10. If the D3D12 budget is not squeezed by the 64 MiB limit, K3 closes as *"measured 2026-xx-xx, no change needed"* in ROADMAP. Only a squeeze reopens it | D5: *"Only if D3D12 residency budgets read too small. Needs a measurement first."* The measurement is the item |

**Gate on this:** `D12-G11` may not be signed off while any of the three is unresolved. Each must
be one of: *landed* (with its evidence), *measured and closed with no change* (with the number), or
*deferred* — and a deferral is a ROADMAP entry with a trigger, not silence. `KMD_IMPACT.md` §14 is
where the item text lives; this table is where its gate lives, and the two must agree.

⚠ **`tools/kmd-frame-sizes.ps1` is mandatory for K1 and K2 and only for them.** Both change a KMD
image, the boot path has **368 bytes** of headroom on a 24 KB kernel stack, and the script matches
its boot symbol by *mangled substring* — a rename makes it pass vacuously. Compare the frame
*number* before and after, not the pass/fail.

---

## 5. The failure-triage playbook for D3D12

### 5.1 vkd3d-proton's own instrumentation (names verified in the source/README)

⚠ **The `README.md` column is pinned to submodule `2c7ba22c`** and was re-derived against it on
2026-08-05 (an earlier revision of this table was systematically 1–2 lines off). Every **name** and
**effect** below was correct even then; it is only the pointers that moved. Re-derive the column
after any submodule bump — `grep -n '^ *- \`VKD3D' README.md` regenerates it in one command.

| Variable | Effect | Source |
|---|---|---|
| `VKD3D_DEBUG=none\|err\|info\|fixme\|warn\|trace` | log level; the banner `vkd3d-proton - build: %015llx` is **INFO** | `README.md:211-212`; `libs/vkd3d/device.c:1479-1481` |
| `VKD3D_SHADER_DEBUG=<same>` | shader-compiler log level | `README.md:213-214` |
| `VKD3D_LOG_FILE=<path>` | redirect the log to a file — **essential in session 1** | `README.md:215` |
| `VKD3D_CONFIG=vk_debug` | Vulkan debug extensions + validation layer | `README.md:197` |
| `VKD3D_CONFIG=single_queue` | no async compute/transfer queues — **the first thing to try on a single-3D-node adapter** | `README.md:204` |
| `VKD3D_CONFIG=no_upload_hvv` | block host-visible VRAM for the UPLOAD heap | `README.md:205-208` |
| `VKD3D_CONFIG=nodxr` / `dxr` / `dxr12` | force DXR off/on/experimental | `README.md:200-202` |
| `VKD3D_CONFIG=breadcrumbs` | instrument command lists with `VK_AMD_buffer_marker` / `VK_NV_device_checkpoints`; on device-lost/timeout dumps the executing lists. ⚠ **trace-enabled builds only** — `enable_breadcrumbs = enable_trace` (`meson.build:57`), `enable_trace` auto = `vkd3d_debug` (`:14,25`), so a **release build has none** | `README.md:312-314` |
| `VKD3D_DISABLE_EXTENSIONS=<list>` | bisect a suspect Vulkan extension | `README.md:219-220` |
| `VKD3D_VULKAN_DEVICE=<idx>`, `VKD3D_FILTER_DEVICE_NAME=<substr>` | force the physical device | `README.md:216-218` |
| `VKD3D_SHADER_DUMP_PATH`, `VKD3D_SHADER_OVERRIDE` | dump `$hash.{spv,dxbc,dxil}`; substitute a SPIR-V | `README.md:291-295` |
| `VKD3D_SHADER_CACHE_PATH=0` | disable the on-disk SPIR-V cache — **set it for every gate run**, or a stale cache hides a shader-translation regression | `README.md:266-267`; the runner exports it at `test-runner.sh:10` |
| `VKD3D_SWAPCHAIN_PRESENT_MODE=IMMEDIATE\|MAILBOX\|FIFO\|FIFO_RELAXED\|FIFO_LATEST_READY` | force a present mode — a direct lever on the vehicle's `Present(interval, flags)` mapping | `README.md:239-241` |
| `VKD3D_FRAME_RATE` | frame-rate cap | `README.md:243-244` |
| `-Denable_descriptor_qa=true` + `VKD3D_DESCRIPTOR_QA_LOG` + `VKD3D_CONFIG=descriptor_qa_checks` | GPU-assisted descriptor validation | `README.md:362-372` |
| ⛔ `VKD3D_FEATURE_LEVEL` | **never in a gate command** — rule 4 | `libs/vkd3d/device.c:10888` |
| `VKD3D_SHADER_MODEL=6_8` | H5 A/B arm **only**, never a shipped configuration | `libs/vkd3d/device.c:10617` |

### 5.2 Windows-side instruments

**ETW providers, all verified present on this VM** (`logman query providers`):

```
Microsoft-Windows-Direct3D12    {5D8087DD-3A9B-4F56-90DF-49196CDC4F11}
Microsoft-Windows-DXGI          {CA11C036-0102-4A2D-A6AD-F03CFED5D3C9}
Microsoft-Windows-DXGIDebug     {F1FF64EF-FAF3-5699-8E51-F6EC2FBD97D1}
Microsoft-Windows-DxgKrnl       {802EC45A-1E99-4B83-9920-87C98277BA9D}
Microsoft-Windows-Direct3D11    {DB6F6DDB-AC77-4E88-8253-819DF9BBF140}
Microsoft-Windows-DxgKrnl-SysMm {9DE90B19-62C4-511D-A1C5-9E990812D18B}
```

**"Why did device creation fail?"** — the DXGI provider prints the runtime's exact rejection string
when there is no device to hold an InfoQueue. This is the recipe that cracked the FL11 story on the
D3D11 side (`ROADMAP.md:3162-3170`); **the only change for D3D12 is swapping
`Microsoft-Windows-Direct3D11` for `Microsoft-Windows-Direct3D12`**:

```
logman start helios_d3d12 -p Microsoft-Windows-DXGI 0xFFFFFFFFFFFFFFFF 0xff -o x.etl -ets
logman update helios_d3d12 -p Microsoft-Windows-Direct3D12 0xFFFFFFFFFFFFFFFF 0xff -ets
<run the probe>
logman stop helios_d3d12 -ets
tracerpt x.etl -o x.xml -of XML -y
:: read <Data Name="Message"> and Code
```

**"What is dxgkrnl doing to my thread?"** — `logman create trace -p Microsoft-Windows-DxgKrnl
0xFFFFFFFFFFFFFFFF 0xFF` → tracerpt → grep **`AzureTriage`** for failure reasons in plain text
(`ROADMAP.md:3463-3465`; `:3450-3456` is the qemu-helios EGL/GTK/Wayland paragraph, not this).
The same provider, taken as a ~2 s circular slice mid-run, gives the
`Present` / `Flip` / `QueuePacket` / `DmaPacket` / `BlockThread` events — that is how the
present-queue stall was found (WS2).

**DRED is available on the app-local arm and is the right first instrument for a D3D12
device-removed/hang.** `libs/d3d12core/main.c:796-801` answers
`IID_ID3D12DeviceRemovedExtendedDataSettings` while returning
`DXGI_ERROR_SDK_COMPONENT_MISSING` for everything else.

**Helios instruments:** `tools/umd-gate-surface.ps1` (first-hit-only counters — **absence is the
zero reading**), `tools/kmd-gate-surface.ps1` + a `tools/kmd-counter-snapshot.ps1` diff (registry,
persists across boots), `tools/scanout_timeline_dump.c`, `tools/read_ledger_dump.c`.

**Host / venus:** `/tmp/helios-qemu-stderr.log` (launcher tee). ⚠ virglrenderer's `vkr_log` /
`proxy_log` are INFO-level and **silent on the release build** — absence of host lines below WARNING
proves nothing.

⛔ **For a *venus*-level question the lever is `HELIOS_VKR_DEBUG=validate`, NOT
`VIRGL_LOG_LEVEL=debug`** (`DECISIONS.md` §6.1). `ROADMAP.md:1901-1903` states it explicitly:
*"host-side `HELIOS_VKR_DEBUG=validate` relaunch (ask owner) — NOT VIRGL_LOG_LEVEL (HOST.md §5.1:
venus runs in the render-server child; only WARN+ reaches the qemu stderr log)"*. **The two are not
interchangeable and ROADMAP contradicts itself about them:** `:3586-3588` recommends
`VIRGL_LOG_LEVEL=debug` for a "real host-side bisect", which is true for *virglrenderer's own*
logging in the QEMU process and false for anything venus does in the render-server child. Both are
**owner-gated relaunches**, so you get one shot — pick by which side of the split the question is
on:

| Question is about | Lever | Where its output lands |
|---|---|---|
| venus command decode, Vulkan object lifetime, ring behaviour | **`HELIOS_VKR_DEBUG=validate`** (host validation layers in the render-server child) | render-server child; ask the owner where it is teed |
| virglrenderer/QEMU-side scanout, blob, fence plumbing | `VIRGL_LOG_LEVEL=debug` | `/tmp/helios-qemu-stderr.log` |

⚠ `OPTIMAL DMA-BUF shape mismatch` in that log is **pre-existing** (first seen
2026-07-26T21:41:56) — `grep -n` for the first occurrence before blaming anything.

**KD:** `NTOSEYE.md`; `tools/take-minidump.ps1` / `tools/live_dump.cpp` for a wedged test process;
`.frame /r`, not `.trap` (memory 62nd).

### 5.3 Triage decision order — check these in this order

```
D3D12 failure
 │
 ├─1. Does test_create_device still pass?  (re-run G1's one-test arm)
 │      NO  → everything else is noise. Go to §5.2 "why did device creation fail" (DXGI+D3D12 ETW).
 │            Also assert VK_KHR_swapchain is still advertised (vn_physical_device.c:1334).
 │
 ├─2. Is the failing name already in docs/dx12/baselines/vkd3d-known-fail.txt?
 │      YES → baseline failure, not a regression. Stop.
 │
 ├─3. Which ARM is it?  app-local (G2) or DDI (G9)?
 │      Reproduce on the OTHER arm. Same binary, only the DLLs beside it differ.
 │      Fails on both  → substrate / ICD / KMD.       Fails on one → that arm's layer.
 │
 ├─4. VKD3D_CONFIG=single_queue → isolates multi-queue against the single-3D-node adapter.
 ├─5. VKD3D_SHADER_CACHE_PATH=0 → isolates a stale SPIR-V cache.
 ├─6. d3d12.exe --warp            → a driver-free control on the same OS + runtime.
 │      Fails on WARP too → the test, the harness or the OS. Not us.
 │
 ├─7. Rendering or presenting?  tools/d3d12_clear_probe.cpp (headless readback, no swapchain).
 │      Clear reads back correct → the defect is in the present path, go to PRESENT.md.
 │
 ├─8. Caps or code?  diff the caps CSV against docs/dx12/baselines/d3d12-caps.csv, then read
 │      docs/dx12/research/d3d12core-driverstrings.txt for the runtime's own English complaint.
 │
 ├─9. Vulkan layer: VKD3D_CONFIG=vk_debug, then VN_DEBUG=init,result,wsi on the ICD.
 │
 └─10. Host: /tmp/helios-qemu-stderr.log. ⛔ Never blame the host without host-side evidence,
        and remember the release build is silent below WARNING.
```

Three shortcuts worth remembering: a **hang** goes to DRED + `VKD3D_CONFIG=breadcrumbs` (trace
build) before anything else; a **black frame** goes to the paintcap-promotion trap (§4.4) before
the driver; a **device-removed** goes to the id-1000 Application log before the graphics stack.

---

## 6. CI

### 6.1 What `windows-stack.yml` builds today

Five jobs, all `runs-on: windows-2022`: `driver` (WDDM driver + D3D11 UMD via
`ci/windows/Build-Driver.ps1`, which meson-builds DXVK with clang-cl), `mesa` (msys2 UCRT64 →
`ci/windows/build-mesa.sh`), `opencl` (CLVK), `loaders` (Khronos loaders **and the four smoke
probes**: `Build-KhronosLoaders.ps1` → `Build-SmokeTests.ps1`), and `package` (Inf2Cat + signtool +
`Assemble-Package.ps1`). Every job checks out with `submodules: false` and then does a targeted
`git submodule update --init`. **There is no vkd3d step anywhere.**

### 6.2 The concrete diff

**The toolchain problem, stated plainly:** vkd3d needs **widl, glslang, meson and a C toolchain in
one image**. The Helios image has meson (pip) and the Vulkan SDK (which carries
`glslangValidator`) in the `driver`/`opencl` jobs — but **no widl**. Two credible shapes, both
taken from upstream's own workflows:

**(a) MSVC on `windows-2022`** — upstream's `test-build-windows.yml:17-52`:

```yaml
  vkd3d:
    runs-on: windows-2022
    steps:
      - uses: actions/checkout@v4
        with: { submodules: false }
      - run: git submodule update --init --recursive vkd3d-proton-helios
      - run: |
          choco install strawberryperl -y                     # ships widl
          Invoke-WebRequest -Uri "https://raw.githubusercontent.com/HansKristian-Work/vkd3d-proton-ci/main/glslangValidator.exe" `
            -OutFile "C:\Strawberry\c\bin\glslangValidator.exe"
          echo "C:\Strawberry\c\bin" >> $env:GITHUB_PATH
      - run: pip install meson
      - run: meson setup -Denable_tests=True -Denable_extras=True --buildtype release `
               --backend vs2022 build-msvc-x64
      - run: msbuild -m build-msvc-x64/vkd3d-proton.sln
      - uses: actions/upload-artifact@v4
        with: { name: helios-vkd3d, path: build-msvc-x64/ }
```

⚠ Upstream itself says MSVC builds are development-only and are not stress-tested
(`README.md:136-142`) — fine for producing `d3d12.exe`, questionable for a shipped `d3d12.dll`.

**(b) mingw cross-build on Linux** — upstream's *release* path (`test-build-linux.yml`,
`artifacts.yml`, both `ubuntu-24.04` using `misyltoad/arch-mingw-github-action@v8`). This produces
the DLLs Proton ships and is the same command that works on this Linux host today (§4.1). It adds a
**non-Windows job** to a Windows-only workflow — a structural change, but a small one, and it is
the shape that should own the shipped DLL.

**Recommendation:** (b) for the shipped `d3d12.dll`/`d3d12core.dll`, (a) additionally if a
debuggable `d3d12.exe` is wanted in CI.

**Either way the workflow also needs** `git submodule update --init --recursive` for
`vkd3d-proton-helios` including its three *nested* submodules — the existing pattern is a targeted
init (`windows-stack.yml:42-44`, the "Initialize DXVK sources" step; `icd/mesa` gets the same
treatment at `:94`), which will not reach them.

**Packaging deltas:** `Assemble-Package.ps1` gains a `vkd3d` payload dir and
`"d3d12-smoke.exe"` in the probe loop at `:76`; `Install-Helios.ps1` records the files in
`install-state.json.runtimeFiles`; `Verify-Helios.ps1` gains the fifth `$tests` entry at `:70-75`;
`Build-SmokeTests.ps1` gains a `cl.exe` line (pattern at `:18-25`); the `package` job's metadata
step gains `"vkd3d=$(git rev-parse HEAD:vkd3d-proton-helios)"` beside `mesa` and `dxvk`
(`windows-stack.yml:222-225`).

**Toolchain risks, named:**
* `choco install strawberryperl` pulls ~1.5 GB and is a network dependency; its `widl` version is
  **UNVERIFIED** (settling command in CI: `widl -V`).
* The `glslangValidator.exe` upstream downloads is a raw binary from a third-party repo — a
  supply-chain input this project would be **adding**. Prefer the Vulkan SDK's copy, already
  installed by the `driver`/`opencl` jobs (on the VM at `C:\VulkanSDK\1.4.350.0\Bin\`).
* MSVC vs mingw: vkd3d's meson handles both (`meson.build` `vkd3d_is_msvc` covers `msvc` and
  `clang-cl`), but the `.def`-file path differs —
  `d3d12_needs_defs = (not vkd3d_is_msvc) and (vkd3d_platform == 'windows')`
  (`libs/d3d12/meson.build:20`, same shape at `libs/d3d12core/meson.build:14`).
* `tools/win-mcp/src/main.rs:734-776` has `win_dxvk`, which mirrors `Z:\dxvk-helios` →
  `C:\Users\Rupansh\dxvk-helios` (constants at `:65-66`) and builds at `C:\Users\Rupansh\dxvk-build`
  with `PATH=LLVM;… && call vcvars64 && meson …`. A `win_vkd3d` tool is a ~40-line copy with new
  constants — ⚠ but vkd3d additionally needs **widl and glslang on PATH**, which `win_dxvk`'s
  command line does not set. ⛔ And per the standing owner directive, **building must not depend on
  win-mcp**: the MCP tool is a convenience, the CI job and the §4.1 shell commands are the contract.

---

## 7. UNVERIFIED, each with its settling experiment

| # | Claim / question | Settling experiment |
|---|---|---|
| 7.1 | **Does vkd3d-proton create a device on the Helios ICD at all?** Nothing in this tree has ever tried. | `D12-G1`. |
| 7.2 | **The host-side evidence tools are unusable on the current boot.** The VM runs `-display sdl,gl=on` with no `-vnc` (verified `pgrep -af qemu-system-x86_64`; `tools/launch-helios-gtk.sh:464-466`), so `vnc_shot.py` / `vnc_frame_probe.py` have nothing to connect to. | Owner-run relaunch with `HELIOS_DISPLAY=egl-vnc bash tools/launch-helios-gtk.sh`. **Ask; do not do it.** Until then G4 runs its guest-only arm. |
| 7.3 | **Which vkd3d-proton version is the Looking Glass prebuilt pair?** No version resource. | Run any client against those DLLs with `VKD3D_DEBUG=info VKD3D_LOG_FILE=…` and read the `vkd3d-proton - build: %015llx` banner (`libs/vkd3d/device.c:1479-1481`). |
| 7.4 | **What does `dxil-spirv` pull in transitively?** The directory is empty, so its `.gitmodules` cannot be read. ⚠ Still open only because settling it *mutates the working tree* (it clones a submodule) — it is not a hard question, and **G0's first command settles it as a side effect**. | `git -C vkd3d-proton-helios submodule update --init subprojects/dxil-spirv && cat vkd3d-proton-helios/subprojects/dxil-spirv/.gitmodules` — run it as part of G0, then paste the answer here. |
| 7.5 | ✅ **MOOT under `DECISIONS.md` D2** — was: does MS `d3d11.dll` accept a DXVK `IDXGIAdapter`? It only arose because an app directory would hold a DXVK `dxgi.dll`. No app-local DLLs ⇒ never asked. ⚠ Keep the underlying hardening (the ICD's bare-name `LoadLibraryA("dxgi.dll")`) as ordinary stability work — any process shipping its own DXGI hands the vehicle a foreign compositor stack. | Not a D3D12 gate any more. |
| 7.6 | ✅ **MOOT under D2** — was: does `--adapter N` reach Helios with DXVK's DXGI in the path? DXVK's DXGI is never in the path. ⚠ The live half still applies: **two display devices exist on this VM**, so `dxgi_luid_dump.exe` must be read before every suite run and the index passed explicitly. | `tools/dxgi_luid_dump.cpp`, every run. |
| 7.7 | **What `skipped` count does a *healthy* driver produce on this suite?** Upstream publishes none. | Establish Helios' own baseline at G2; if a second machine is ever available, take a `--warp` baseline on the same OS build as an upper bound. |
| ~~7.8~~ | ✅ **SETTLED 2026-08-05 — the VM HAS network access.** `(Invoke-WebRequest https://api.nuget.org/v3/index.json -UseBasicParsing).StatusCode` returned **200** from a `win_exec` shell. So the MS-samples nuget restore (§2.3) and CI shape (a)'s `choco install` are downloads, not blockers. Kept as a row so §2.3's cross-reference resolves. | Closed. Re-run the one-liner if a network change is suspected. |
| 7.9 | **Which choco/msys2 package supplies `widl` on a GitHub `windows-2022` runner, and at what version?** Upstream pins nothing. | `choco install strawberryperl -y; widl -V` in a throwaway CI run. |
| 7.10 | **Does the 16-way parallel `test-runner.sh` default wedge this adapter?** One `DXGK_ENGINE_TYPE_3D` node; the ring-wait wedge class was only *bounded* in `icd f0c7bcd3465`. | `-j 1`, then `-j 2`, then `-j 4`, recording wall time and any wedge. G2. |
| ~~7.11~~ | ✅ **SETTLED 2026-08-05 — `NightRaidGt1P` is correct.** `Get-Content 'C:\Program Files\UL\3DMark\nightraid.3dmdef'` lists exactly `NightRaidDemoP`, **`NightRaidGt1P`**, `NightRaidGt2P`, `NightRaidCpuP`. G10's command block is safe to run as written. | Closed. |
| 7.12 | **Does the D3D12 runtime cross-validate the caps set as one contract** (the D3D11 `LLOCompleteLayerConstruction` analogue, `umd/src/caps.rs:39-42`)? The in/out `D3D12DDICAPS_TYPE_0081_3DPIPELINESUPPORT1` negotiation (`d3d12umddi.h:10416-10420`) suggests some, but nothing states it. | `D12-G5` shim answering deliberately inconsistent caps; read ETW `Microsoft-Windows-DxgKrnl` → `AzureTriage`. |
| 7.13 | **Does the runtime hand `pfnCreateShader` a DXBC container or a raw stream, per shader model?** The DDI passes **no length parameter anywhere** (`grep BytecodeLength d3d12umddi.h` → nothing). | `D12-G5` shim dumping the first 8 dwords during a `HelloWindow` run. |
| 7.14 | **Exact contract of `D3D12DDICAPS_TYPE_EXECUTECOMMANDLISTS_PARALLELISM`** (1069, `pData = BOOL`, `d3d12umddi.h:128`). Report FALSE until proven. | WDK doc read, or an ETW `DxgKrnl` trace showing concurrent `QueuePacket` submits with the cap set. |
| 7.15 | **`TypedUAVLoadAdditionalFormats` and `ResourceHeapTier` on this ICD** — neither is derivable from `vulkaninfo`. | `tools/d3d12_caps_dump.cpp` at G2 (this is one of the reasons G2 produces the CSV). |
| 7.16 | **Is `guest-vulkaninfo-full.txt` perturbed by `VK_LAYER_OBS_HOOK`?** An OBS layer is loaded in the guest (capture lines 1-2). | Re-capture with `VK_LOADER_LAYERS_DISABLE=*` and diff against `docs/dx12/research/guest-vulkaninfo-full.txt`. |
| 7.17 | **Does a WDDM 2.1 adapter constrain the shader models a D3D12 UMD may report?** The WDDM history table maps 2.1 → SM 6.0 and 3.2 → SM 6.8; Helios declares `Wddm2_1GpuMmu` (`kmd_render/src/ddi/wddm_surface.rs`). | `D12-G5` shim reporting SM 6.5 at WDDM 2.1 and reading whether the runtime accepts it; interacts with the `E_NOTIMPL`/MPO3 reason WDDM 3.2 is unselected. |
| 7.18 | **Post-fix fullscreen vehicle behaviour, on the SHIPPING gate path.** ⛔ Narrowed — the claim "no measurement exists" is **wrong** (`DECISIONS.md` §6.1): `ROADMAP.md:2919-2931` **is** that measurement — the fullscreen 1896×1030 chain went VEHICLE, READY+LIVE on the same hwnd as the windowed chain, after the target-registry fix, with `kwait_armed 6144/6144`, 0 arm/queue fails and `queue_present_avg 5.96→2.81 ms`. What is actually open is narrower: **those numbers were taken with `VehicleKernelFlipWait=1`, which R912(a) has since retired**, so they do not describe the path that ships today. | Re-measure on the shipping gate path (no `VehicleKernelFlipWait`): a fullscreen D3D12 client via a session-1 schtask with `HELIOS_WSI_PERF=1`; read `creates=/fails=/ready=` in the WSI perf line and `helios_paintcap`, and state the delta against `ROADMAP.md:2919-2931`. G4. |
| 7.19 | **P-C, narrowed twice.** ✅ *Resolved:* the callback surface is **not** limited to `d3d12umddi.h` — `D3D12DDIARG_CREATEDEVICE_0109.pKTCallbacks` (`d3d12umddi.h:13623`) is a `CONST D3DDDI_DEVICECALLBACKS*`, the same 65-entry table the D3D11 UMD drives (`d3dumddi.h:4499`), and it carries **`pfnRenderCb` and `pfnPresentCb`**. So the `HeliosPresentRenderCmd` identity channel transfers with **no KMD change** (`DECISIONS.md` §6.1). ⛔ Do **not** re-open this as "decode the identity in `DxgkDdiSubmitCommandVirtual`" — that DDI is DISPATCH_LEVEL (`kmd_render/src/ddi/submit_command.rs:723-724`) where `diag::record*` is illegal. **Still open:** (a) whether the D3D12 runtime tolerates the driver calling `pfnRenderCb` around `pfnPresent`, and (b) whether `D3D12DDIARG_PRESENT_0001.pPrivateDriverData` reaches `DxgkDdiPresent` at all — the D3D11 answer is *no on DMA flips* (memory 64th), which is exactly why the identity rides the Render command. | (a) `pfnRenderCb` + a counting `DxgkDdiRender` on the D3D12 path at **G8**, confirming the count moves — before any later rung depends on it. (b) ETW `Microsoft-Windows-DxgKrnl` around a D3D12 sample **on a real driver**; does not need Helios and pairs naturally with `D12-G5`. |
| 7.20 | **Which `[local]` label keys does a Night Raid / Time Spy run emit into 3DMark's log?** `tmp/perf/run-fs.ps1:149` matches only `(gt1\|gt2\|combined\|physics)`, which are Fire-Strike-specific, so the G10 wrapper as shipped extracts **zero** score lines from a DX12 workload (§4.11). The regex cannot be widened correctly without seeing the real keys — guessing them reproduces the same silent-zero failure. | Run `NightRaidGt1P` **once**, unscored, then `Select-String -Path <3DMark.log> -Pattern '\[local\]'` and read the keys. Then either widen the alternation or switch to the `Result.xml` parser in §4.11. **G10, before run 1 of 3.** |
| 7.21 | **Does `HELIOS_WSI_INSURANCE_BLIT` stay inert at D3D12 resolutions?** It was measured **inert at Doom resolution** — `ROADMAP.md:2919-2926` (owner Doom verdict, `insurance=0`, no fps change, `insurance_skipped 13176/13200`) and `:2948-2950` (*"no measurable cost either way at Doom res — the copy hides under GPU latency"*). That is a settled result, not an open question; what is untested is whether the copy still hides under GPU latency at Night Raid / Time Spy resolutions. | A paired, interleaved `insurance=1` vs `insurance=0` arm inside **G10** (`tmp/perf/ab-env.ps1` shape), reported as a paired delta with its spread. ⛔ Never as a single-run comparison, and never as a G4 pass criterion. |
| 7.22 | **What HUD rectangle does `vnc_frame_probe.py --hud` take for `gears.exe`?** The probe's completeness oracle needs a region that is **bright in EVERY completed app frame** (`vnc_frame_probe.py:148-152`); its default `410,695,870,735` is 3DMark's fps bar at 1280×800 and is meaningless for the vkd3d demo. `gears.exe` renders rotating geometry on a clearing background, so no *fixed* rectangle is trivially bright-every-frame — without a valid one, G4's black-frame percentage is not computable and the gate silently degrades to an ordering-only result. | Capture ~200 frames with `--hudthresh -1` (oracle disabled) and pick the rectangle with the highest minimum mean across frames; or overlay a constant bright patch by running the demo behind a small always-on-top window. Failing both, run `gears.exe` **and** a second arm with a workload that has a HUD, and state which arm produced the black-frame %. **G4, before the 120 s run.** |
