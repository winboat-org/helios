# R9 — Test, conformance and CI strategy for Helios D3D12

**Lane:** R9. **Written:** 2026-08-05. **Scope:** how a Helios D3D12 implementation gets *proven*,
checkpoint by checkpoint — the suites that exist, the workloads installed on this box, the Helios
instruments that already gate D3D11, a named gate ladder `D12-G0 … D12-G9`, and a D3D12-specific
triage playbook.

**Evidence rule used throughout.** Every claim carries a `path:line`, a URL, or the exact command
and its output. Claims that could not be verified read **UNVERIFIED** and name the read or
experiment that settles them. "the header/source says" is distinguished from "I infer".
Commands prefixed `win$` were run on the win11 VM through `win_exec` (read-only); commands prefixed
`linux$` on the Linux host. **No builds were run and nothing was installed** — this is a survey.

---

## 0. The one-paragraph answer

The test ladder does not have to be invented. vkd3d-proton carries **557 D3D12 tests in one
self-contained binary** (`vkd3d-proton-helios/tests/d3d12_tests.h`, 557 `decl_test` lines) whose
Windows build resolves D3D12 **by `LoadLibraryA("d3d12.dll")`**
(`tests/d3d12_crosstest.h:71-78`) — so the *same* binary tests **strategy (b)** (vkd3d's own
`d3d12.dll` dropped next to it) and **strategy (a)** (the system `d3d12.dll` on top of a native
Helios D3D12 UMD) with nothing changed but which DLLs sit in the exe's directory. Both toolchains
needed to build it are **already present**: the Linux host has meson/ninja/widl/glslang/mingw-w64
(`linux$ command -v`, §1.3) and the VM has meson/ninja/widl/glslang/mingw-UCRT-g++/MSVC/MSBuild
(`win$`, §1.3). A **prebuilt vkd3d-proton is already on the VM** at
`C:\Program Files\Looking Glass (IDD)\D3DTranslation\{d3d12.dll,d3d12core.dll}` (identified by the
string `vkd3d-proton/libs/d3d12core/debug.c` inside it, §1.6), so **D12-G1 can be run today with
zero builds**. Every D3D12 3DMark workload is installed and its API was verified by reading the
import strings out of each workload exe (§4.1). The single largest trap in the whole plan: when
`D3D12CreateDevice` fails, vkd3d's tests **`skip`, not fail** (`tests/d3d12_test_utils.h:1355-1358`),
the process exits 0, and `test-runner.sh` prints **`ALL PASSED!`** — a totally dead adapter scores a
perfect run. Every gate below therefore reads the *skipped* count, never the exit code alone.

---

## 1. vkd3d-proton's own test suite

### 1.1 What exists

`linux$ ls vkd3d-proton-helios/tests/` → 34 `d3d12_*.c` sources + `d3d12_tests.h`,
`d3d12_test_utils.{c,h}`, `d3d12_crosstest.h`, `d3d12_dstorage_blobs.h`, `shaders/`,
`test-runner.sh`, `meson.build`, plus three extra binaries' sources (`descriptor_performance.c`,
`pso_library_bloat.c`, `vkd3d_api.c`, `vkd3d_common.c`).
`linux$ wc -l vkd3d-proton-helios/tests/*.c *.h` → **105 265 lines total**.

The test *registry* is `tests/d3d12_tests.h` — a header included three times with `decl_test`
redefined (declare, list, run):

```
linux$ grep -o 'decl_test([a-zA-Z0-9_]*)' d3d12_tests.h | sed 's/decl_test(//;s/)//' > /tmp/tests.txt
       total: 557  unique: 557  stress: 12
       raytracing: 20  sampler_feedback: 13  workgraph: 10  vrs: 5  mesh_shader: 4  sparse: 3  dstorage: 1
       *_dxil-named: 65   *_dxbc-named: 48
```

So: **557 tests**, all names unique, **12** carrying `stress` in the name (excluded by the default
runner, `tests/test-runner.sh:58-64`) → **545 in a default run**.

`tests/meson.build:12-47` lists the 34 translation units compiled into **one** executable
(`tests/meson.build:49` `executable('d3d12', d3d12_test_src, …)` → `d3d12.exe` on Windows).
Two other executables are built from the same utils lib: `descriptor-performance`
(`:56`) and `pso-library-bloat` (`:63`).

### 1.2 Pass semantics — and the trap

`include/private/vkd3d_test.h` keeps five counters (`:110-112` + success/todo_success) and the
process return is `:329`:

```c
    printf("%s: %lu tests executed (%lu failures, %lu successful todo, %lu skipped, %lu todo, %lu bugs).\n", …);
    return vkd3d_test_state.failure_count != 0;
```
(`include/private/vkd3d_test.h:316-329`)

`ok()` bumps `failure_count` (`:158`); `todo` conditions bump `todo_count` (`:202`); `skip()` bumps
`skip_count` (`:232`) **and does not fail**.

⚠ **The load-bearing trap.** Test setup is `init_test_context_()` in
`tests/d3d12_test_utils.h:1347-1362`:

```c
    if (!(context->device = create_device()))
    {
        skip_(line)("Failed to create device.\n");
        return false;
    }
```

If `D3D12CreateDevice` fails, the test **skips**, `failure_count` stays 0, the process exits **0**,
and `test-runner.sh:152` prints **`ALL PASSED!`**. An adapter that cannot make a D3D12 device at all
produces a perfect-looking suite result. Individual test bodies do the same —
`tests/d3d12_pso.c:39,119,177`, `tests/d3d12_mesh_shader.c:108` all `skip("Failed to create device.\n")`.

**Consequence for every gate below:** the pass criterion is never "exit 0" and never "ALL PASSED".
It is *(a)* `failures == 0` **and** *(b)* `skipped` at or below a recorded baseline, both parsed out
of the per-test summary lines, which requires running the suite with `-o <logdir>` (see §1.5).

### 1.3 How it is built — both toolchains already exist

Requirements, verbatim from `vkd3d-proton-helios/README.md:68-73`:

> - [wine](https://www.winehq.org/) (for `widl`) [for native builds]
>   - On Windows this may be substituted for [Strawberry Perl](http://strawberryperl.com/) as it ships `widl` …
> - [Meson](http://mesonbuild.com/) build system (at least version 0.49)
> - [glslang](https://github.com/KhronosGroup/glslang) compiler
> - [Mingw-w64](http://mingw-w64.org/) compiler, headers and tools (at least version 7.0) [for cross-builds for d3d12.dll which are default]

Tests are **off by default** — `meson_options.txt:1` `option('enable_tests', type:'boolean', value:false)`;
demos/programs need `enable_extras` (`:2`, gated at `meson.build:202-211`).

**Linux host (verified):**
```
linux$ for t in meson ninja widl glslangValidator x86_64-w64-mingw32-gcc x86_64-w64-mingw32-g++ wine; do command -v $t; done
/usr/bin/meson /usr/bin/ninja /usr/bin/widl /usr/bin/glslangValidator
/usr/bin/x86_64-w64-mingw32-gcc /usr/bin/x86_64-w64-mingw32-g++ /usr/bin/wine
```
The cross file `vkd3d-proton-helios/build-win64.txt` wants exactly `x86_64-w64-mingw32-{gcc,g++,ar,strip}`
and a `widl-mingw-tools-fallback` binary — all present. **The upstream cross build is available on
this host with zero setup.**

**win11 VM (verified):**
```
win$ meson  -> C:\Users\Rupansh\AppData\Local\Programs\Python\Python312\Scripts\meson.exe   (1.11.1)
win$ ninja  -> …\Scripts\ninja.exe                                                          (1.13.0)
win$ glslangValidator, glslang -> C:\VulkanSDK\1.4.350.0\Bin\
win$ widl   -> C:\Users\Rupansh\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs…\mingw64\bin\widl.exe
win$ & widl -V  ->  "Wine IDL Compiler version 11.5"
win$ g++    -> …WinLibs…\mingw64\bin\g++.exe  (MinGW-W64 x86_64-ucrt-posix-seh … 16.1.0)
win$ MSBuild -> C:\Program Files\Microsoft Visual Studio\2022\Community\Msbuild\Current\Bin\MSBuild.exe
win$ SDK Include dirs: 10.0.22621.0, 10.0.26100.0, wdf ;  d3d12.lib/dxgi.lib/dxguid.lib/dxcompiler.lib present in Lib\10.0.26100.0\um\x64
win$ dxc.exe, fxc.exe present in bin\10.0.26100.0\x64
```
So **both** the mingw-native path (README:155-166) and the MSVC path (README:143-152) are available
on the VM without installing anything.

⚠ **Submodules are NOT populated.** `linux$ for d in khronos/Vulkan-Headers khronos/SPIRV-Headers
subprojects/dxil-spirv; do ls -A $d | wc -l; done` → `0 0 0`. `.gitmodules` in the submodule
declares those three (`vkd3d-proton-helios/.gitmodules`). A build therefore needs
`git submodule update --init --recursive` **inside** `vkd3d-proton-helios/` first — a network fetch,
and `dxil-spirv` has its own nested submodules (**UNVERIFIED** which, because the directory is empty;
settling read: `git -C vkd3d-proton-helios submodule update --init subprojects/dxil-spirv && cat
subprojects/dxil-spirv/.gitmodules`).

Pinned HEAD: `linux$ git submodule status` → `2c7ba22c53261458a7a204c55f3098ad9855cb15
vkd3d-proton-helios (vkd3d-1.1-5456-g2c7ba22c)`; `git log --oneline -1` →
`2c7ba22c tests: fix test_fp_truncate_roundtrips when it's skipped`.

### 1.4 Exact build commands

**Cross-build on the Linux host** (upstream's own default; produces the exact artifacts Proton ships):

```bash
cd /home/rupansh/helios-vgpu/vkd3d-proton-helios
git submodule update --init --recursive          # required: khronos/*, subprojects/dxil-spirv are empty
meson setup --cross-file build-win64.txt --buildtype release \
      -Denable_tests=true -Denable_extras=true build.64
ninja -C build.64
# artifacts:
#   build.64/libs/d3d12/d3d12.dll          (libs/d3d12/meson.build:22, name_prefix '')
#   build.64/libs/d3d12core/d3d12core.dll  (libs/d3d12core/meson.build:16)
#   build.64/tests/d3d12.exe               (tests/meson.build:49)
#   build.64/demos/triangle.exe, gears.exe (demos/meson.build:20,26 — need enable_extras)
```
The `-Denable_tests=True -Denable_extras=True … --cross-file=build-win64.txt` pair is exactly what
upstream CI runs (`vkd3d-proton-helios/.github/workflows/test-build-linux.yml`, "Build MinGW x64").

`package-release.sh` is **not** the right tool for a test build: `build_arch` (`package-release.sh:52-77`)
never passes `-Denable_tests`, and `:70-76` deletes the build directory afterwards unless `--dev-build`.

**Native MSVC on the VM** (README:143-152 — for debugger work):
```
:: in a VS2022 x64 native tools prompt, with widl+glslang on PATH
meson setup --buildtype release --backend vs2022 -Denable_tests=true -Denable_extras=true build-msvc
msbuild build-msvc\vkd3d-proton.sln
```

### 1.5 Exact run commands, and what "pass" means

`tests/d3d12.exe` argument surface (`tests/d3d12_crosstest.h:838-854`):
`--list-tests` (prints all 557 names, exits 0), `--adapter <N>` (DXGI adapter index),
`--warp`; plus `--validate` / `--gbv` (`:857-870`, D3D12 debug layer / GPU-based validation) and
`--feature-level {11_0|11_1|12_0|12_1|12_2}` (`:268-305`; default `D3D_FEATURE_LEVEL_11_0`,
`tests/d3d12_test_utils.c:28`).

Environment (README:211-236, implemented at `include/private/vkd3d_test.h:277-291`):
`VKD3D_TEST_MATCH` (exact name), `VKD3D_TEST_FILTER` (substring; mutually exclusive with MATCH,
`:285-289`), `VKD3D_TEST_EXCLUDE`, `VKD3D_TEST_DEBUG` (0/1/2), `VKD3D_TEST_PLATFORM`
(`wine|windows|other` — controls `todo()/bug_if()/broken()`; auto-detected on Windows at `:301-308`),
`VKD3D_TEST_BUG=0`.

**Whole-suite run.** `tests/test-runner.sh` is bash; the VM has `C:\Program Files\Git\bin\bash.exe`
(`win$ Test-Path` → True) at `BASH_VERSION=5.3.9(1)-release` with `/proc/cpuinfo` reporting **16**
processors — `wait -n -p` (`test-runner.sh:106`) needs bash ≥ 5.1, satisfied.

```
win$ & 'C:\Program Files\Git\bin\bash.exe' -c \
      "./test-runner.sh -o /c/Users/Rupansh/vkd3d-logs -j 2 /c/Users/Rupansh/vkd3d/tests/d3d12.exe"
```
Runner behaviour that matters: it forks **one process per test** with
`VKD3D_TEST_MATCH=<name>` (`:91,93`), defaults to **one job per CPU thread** (`:14`) — on this
adapter that is 16 concurrent D3D12 devices against a single `DXGK_ENGINE_TYPE_3D` node
(`kmd_render/src/ddi/query_adapter_info.rs:1254-1278`, cited in `DX12.md:207`), so **start at
`-j 1`, then `-j 2`**; it drops `*stress*` unless `-s` (`:58-64`); it exports
`VKD3D_SHADER_CACHE_PATH=0` (`:10`) to dodge a `test_object_interface` race; and **without `-o` it
sends every test's stdout to `/dev/null`** (`:91`) — which throws away the skip counts. `-o` is
mandatory for a Helios gate.

**Pass criterion (the Helios form).** Parse each `<logdir>/<test>.log` for the summary line
(format at `vkd3d_test.h:316-324`) and require:
* `failures == 0` for every test, **and**
* total `skipped` ≤ the recorded baseline for that build (first green run *is* the baseline), **and**
* `tests executed` summed over logs == number of test logs (a crashed test writes no summary).

Record the triple `(executed, failures, skipped)` per run. Upstream does not publish an expected
pass count for any driver, so **there is no absolute number to hit** — the metric is a *baseline
diff*, exactly like `CONFORMANCE.md` C10 asks for on the D3D11 side.

### 1.6 Which `d3d12.dll` does it test? — the dual-use finding

`tests/d3d12_crosstest.h:70-81`:
```c
#if defined(_WIN32) && !defined(VKD3D_FORCE_UTILS_WRAPPER)
#define get_d3d12_pfn(name) get_d3d12_pfn_(#name)
static inline void *get_d3d12_pfn_(const char *name)
{
    static HMODULE d3d12_module;
    if (!d3d12_module)
        d3d12_module = LoadLibraryA("d3d12.dll");
    return GetProcAddress(d3d12_module, name);
}
```
It resolves `D3D12CreateDevice` etc. **by name from whatever `d3d12.dll` the loader finds**. And
`d3d12.dll` is **not** a KnownDLL on this box:
```
win$ (Get-Item 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\KnownDLLs').GetValueNames() |
       Where-Object { $_ -match 'd3d|dxgi' }        # → no output; 38 KnownDLLs total, none d3d/dxgi
```
so the exe's own directory wins. **Therefore:**
* `tests/d3d12.exe` **alone** → tests the *system* `d3d12.dll` (`C:\Windows\System32\d3d12.dll`,
  `win$` version **10.0.26100.8737**) on top of whatever WDDM UMD backs the adapter — i.e. it is
  a ready-made conformance suite for **strategy (a)**, a native Helios D3D12 UMD.
* `tests/d3d12.exe` **with `d3d12.dll` + `d3d12core.dll` copied beside it** → tests
  **strategy (b)**, vkd3d-proton over Vulkan/venus.

That single fact makes the same 557-test binary the gate for both strategies and means the test
investment is not wasted whichever way `DX12.md` §2 resolves.

**A vkd3d-proton binary is already on this VM.** `C:\Program Files\Looking Glass (IDD)\D3DTranslation\`
contains `d3d12.dll` (155 662 B) and `d3d12core.dll` (5 963 790 B), both dated 2026-06-05, with no
version resource. Identified by byte-scan:
```
win$ FOUND 'vkd3d-proton'   at 5021850 : "vkd3d-proton/libs/d3d12core/debug.c"
win$ FOUND 'VKD3D_CONFIG'   at 5028349
win$ FOUND 'dxil-spirv'     at 5431840 : "dxil-spirv does not support SHADER_QUIRK."
```
Its exact vkd3d-proton version is **UNVERIFIED** (no version string found; `libs/vkd3d/device.c:1479-1481`
prints `vkd3d-proton - build: %015llx` at INFO level — settling experiment: run any client against
those DLLs with `VKD3D_DEBUG=info VKD3D_LOG_FILE=…` and read the banner).

### 1.7 How vkd3d picks the adapter (needed to aim the suite at Helios)

`libs/d3d12core/main.c:375-436` — with `adapter == NULL` it takes `EnumAdapters(factory, 0)`
(`:389-394`), i.e. **DXGI adapter 0**; with an `IDXGIAdapter` it uses it directly (`:428-434`).
Then `:708` copies `adapter_desc.AdapterLuid` into `device_create_info.adapter_luid` and `:506`
matches it against `VkPhysicalDeviceIDProperties.deviceLUID`, skipping any physical device below
`VKD3D_MIN_API_VERSION` = `VK_API_VERSION_1_3` (`include/vkd3d.h:53`, checked at `main.c:492`).
The 30th-session memory records that venus already reports the WDDM adapter LUID, so this match is
expected to work — but that is a prior-session claim, not a measurement on the D3D12 path.

The test harness's own adapter choice (`tests/d3d12_crosstest.h:445-465`) is subtle: it only passes
an adapter when `use_warp_device || use_adapter_idx` is true, so **`--adapter 0` behaves exactly
like no argument** (adapter = NULL → vkd3d/DXGI default). On this VM two display devices exist
(`win$ Get-CimInstance Win32_VideoController` → "Looking Glass Indirect Display Device" and
"Helios vGPU Render Adapter (WDDM bring-up)", `PCI\VEN_1AF4&DEV_1050…`), so **which DXGI index is
Helios must be read with `tools/dxgi_luid_dump.cpp` before every suite run** and passed as
`--adapter N`. `VKD3D_FILTER_DEVICE_NAME` / `VKD3D_VULKAN_DEVICE` (README:217-219) are the
belt-and-braces on the Vulkan side.

### 1.8 What the suite will *not* tell you

* **Nothing about presentation.** No test creates a swapchain (`libs/vkd3d` implements
  `IDXGIVkSwapChainFactory`, not DXGI itself — §2.4). Screen evidence needs the demos or a probe.
* **Nothing about performance.** `descriptor-performance` and `pso-library-bloat` are separate
  binaries and are micro-benchmarks, not the frame-level numbers this project reports.
* **Not a substitute for owner-visible evidence.** Per CLAUDE.md rule 6, a green suite is a log,
  not a frame.

---

## 2. Sample corpora — the bring-up ladder

### 2.1 The best first rungs are vkd3d's own demos, not the MS samples

`demos/meson.build:20,26` builds **`triangle`** and **`gears`** as `gui_app : true` executables
depending only on `lib_dxgi` + `lib_d3d12` (`:14-16`). `demos/demo_win32.h:248-266` creates a real
HWND swapchain: `CreateDXGIFactory1(IID_IDXGIFactory2)` → `swapchain_desc.SwapEffect =
DXGI_SWAP_EFFECT_FLIP_DISCARD` (`:257`) → `IDXGIFactory2_CreateSwapChainForHwnd(factory,
(IUnknown *)command_queue, …)` (`:261`). Their shaders are **pre-compiled DXBC blobs checked into
the tree** (`demos/triangle_vs.h:1-12` — "Generated by Microsoft (R) D3D Shader Disassembler",
SM5-style signature block), so no runtime compiler, no nuget, no MSBuild, and **no dxil-spirv
path exercised** — which makes `triangle` the cleanest possible "one D3D12 frame" probe.

### 2.2 DirectX-Graphics-Samples inventory

`linux$ ls dx-samples-research-only/Samples/Desktop/` → 24 sample solutions:
`D3D1211On12, D3D12Bundles, D3D12DepthBoundsTest, D3D12DynamicIndexing, D3D12ExecuteIndirect,
D3D12Fullscreen, D3D12HDR, D3D12HelloWorld, D3D12HeterogeneousMultiadapter, D3D12LinkedGpus,
D3D12MeshShaders, D3D12Multithreading, D3D12nBodyGravity, D3D12On7, D3D12PipelineStateCache,
D3D12PredicationQueries, D3D12Raytracing, D3D12ReservedResources, D3D12Residency,
D3D12SM6WaveIntrinsics, D3D12SmallResources, D3D12StateObjectDatabase, D3D12VariableRateShading,
D3D12xGPU`; plus `MiniEngine/` (Core, Model, ModelViewer, ModelConverter, Tools) and
`TechniqueDemos/D3D12MemoryManagement`.

`D3D12HelloWorld/src/` holds 15 sub-samples: `HelloWindow, HelloTriangle, HelloTexture,
HelloConstBuffers, HelloFrameBuffering, HelloBundles, HelloTightAlignment, HelloGenericPrograms,
HelloPartialGraphicsPrograms, HelloMeshNodes, HelloWorkGraphs, WorkGraphsSandbox, HelloVADecode,
HelloVAEncode, HelloVAResourceInterop`.

**Two build costs, both verified:**
1. **Agility SDK dependency.** `HelloWindow/D3D12HelloWindow.cpp:15` and
   `HelloTriangle/D3D12HelloTriangle.cpp:15-16`:
   `extern "C" { __declspec(dllexport) extern const UINT D3D12SDKVersion = 618; }` and
   `… const char* D3D12SDKPath = u8".\\D3D12\\";`. 31 `packages.config` files pin
   `Microsoft.Direct3D.D3D12 1.618.3`.
2. **DXC/DXIL dependency.** `HelloTriangle/D3D12HelloTriangle.vcxproj:3,163,171` hard-`<Error>`s
   unless `Microsoft.Direct3D.DXC.1.8.2505.32` is restored; `D3D12HelloTriangle.cpp:161-162`
   loads pre-built `shaders_VSMain.cso` / `shaders_PSMain.cso`.

So each sample needs **MSBuild + a nuget restore (network)**. MSBuild is present
(`win$ …\2022\Community\Msbuild\Current\Bin\MSBuild.exe`); `nuget` is **not on PATH** (`win$ MISS nuget`)
and whether the VM can reach nuget.org is **UNVERIFIED** (settling command: `win$ (Invoke-WebRequest
https://api.nuget.org/v3/index.json -UseBasicParsing).StatusCode`).

**Recommendation:** treat the MS samples as *stage-2* corpus. The project's own idiom — a
self-contained single-file probe in `tools/` compiled with one `cl.exe` line
(`CONFORMANCE.md:307-320`) — is cheaper and matches the ~40 existing probes. Write
`tools/d3d12_*.cpp` probes and use vkd3d's `triangle`/`gears` for the first frames.

### 2.3 The ladder (ascending driver demand)

| # | Rung | What it needs from the driver | Windowed? | Pass criterion |
|---|---|---|---|---|
| 0 | `VKD3D_TEST_MATCH=test_create_device tests/d3d12.exe` | device creation only | no | log summary `1 tests executed (0 failures, …, 0 skipped, …)` |
| 1 | `tools/d3d12_devicecreate_probe.cpp` *(to write)* — mirror of `tools/d3d11_devicecreate_probe.cpp`: find the Helios adapter by description, `D3D12CreateDevice` at FL11_0, dump `CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS…)` | device + caps | no | exit 0 + a checked-in caps dump |
| 2 | `demos/triangle.exe` (vkd3d, DXBC) | direct queue, 1 command list, PSO, root sig, FLIP_DISCARD swapchain, fence | **yes** | **owner-visible** triangle in `Z:\tmp\screen_copy.png` |
| 3 | `Samples/Desktop/D3D12HelloWorld/src/HelloWindow` | clear-only: `ResourceBarrier` PRESENT↔RENDER_TARGET, `ClearRenderTargetView` clear colour `{0.0,0.2,0.4,1.0}`, `Present(1,0)`, fence wait (`D3D12HelloWindow.cpp:191-215`) | **yes** | the exact clear colour visible; a colour-probe on the screenshot is a *numeric* criterion, not a judgement call |
| 4 | `…/HelloTriangle` | + vertex buffer, input layout, DXIL PSO from `.cso`, root signature (`D3D12HelloTriangle.cpp:150-151,161-175`) | **yes** | triangle visible; no `DDI refusals:` line in the UMD log |
| 5 | `…/HelloTexture` | + SRV descriptor heap, `UpdateSubresources` upload heap, sampler-less static sampler | yes | textured quad |
| 6 | `…/HelloFrameBuffering` | + per-frame allocators, N-buffered fence discipline (the real present cadence) | yes | 60 s soak, no stall, black-frame % measured (§3.4) |
| 7 | `…/HelloBundles` | `ID3D12GraphicsCommandList` bundles (`D3D12_COMMAND_LIST_TYPE_BUNDLE`) | yes | same image as HelloTriangle |
| 8 | `…/D3D12nBodyGravity` | compute queue + async compute, UAVs, `D3D12_COMMAND_LIST_TYPE_COMPUTE` | yes | animated particles; **this is the first rung that stresses the single-3D-node story** (`DX12.md:207-213`) |
| 9 | `…/D3D12Multithreading` | many command lists recorded on worker threads | yes | correctness + no device-removed |
| 10 | `…/D3D12ExecuteIndirect` | command signatures, indirect args (GPU VA visible to the app — see `DX12.md:301-308`) | yes | correctness |
| 11 | `…/D3D12ReservedResources`, `…/D3D12Residency`, `…/D3D12SmallResources` | tiled/reserved resources, `MakeResident`/`Evict`, small-alignment placed resources | yes | each is a known-risk area; expect a filed defect, not a pass, on the first attempt |
| 12 | `…/D3D12MeshShaders`, `…/D3D12VariableRateShading`, `…/D3D12Raytracing` | DX12 Ultimate tiers | yes | out of scope until §4's tier evidence says the substrate has them |

Rungs 0-4 are the *bring-up* ladder; 5-9 the *correctness* ladder; 10-12 are stretch.
Everything from rung 2 down is a window and therefore a **session-1 scheduled task** (§3.5).

---

## 3. Existing Helios test machinery, and exactly how each gates a D3D12 checkpoint

`linux$ ls tools/ | wc -l` → **119** entries (~58 probe sources + PowerShell drivers + the `win`
MCP server). `CONFORMANCE.md:180-268` already catalogues them one line each; this section only says
*which* apply to D3D12 and how.

### 3.1 Directly reusable, unchanged

| Tool | Use in a D3D12 gate |
|---|---|
| `tools/dxgi_luid_dump.cpp` | **Mandatory before every suite run** — gives the DXGI index + LUID of the Helios adapter for `--adapter N` (§1.7). |
| `tools/adapter_type_probe.cpp` | `D3DKMTEnumAdapters2` cross-check when DXGI index and LUID disagree (the phantom-adapter class, memory 33rd). |
| `tools/kmd-counter-snapshot.ps1 -Label <n> -OutDir <dir>` | Pre/post snapshot around every gate. Header (`:6-11`) states the rule: registry counters **persist across boots**, so only a *diff* is evidence. |
| `tools/kmd-gate-surface.ps1` | Machine verdict: non-zero exit if any `MustBeZero` failure counter moved (`:20-31` lists `WtOut WtTbl CtOut ScBadAlc … HpdStTo`). |
| `tools/umd-gate-surface.ps1 [-AllProcesses -SinceMinutes N]` | The D3D11 UMD's refusal readout. Under **strategy (b)** it should stay clean *and that is itself the check* — if a vkd3d run moves D3D11 UMD counters, something is routing through `helios_umd.dll` that should not be. Under **strategy (a)** it is the direct instrument (the D3D12 refusal counters would live beside the eleven at `umd/src/forward.rs:331-385`). |
| `tools/kmd-frame-sizes.ps1` | Only if a KMD image changes for D3D12 — the 368-byte boot-stack headroom rule (`DX12.md:426-429`). |
| `tools/desktop_paint_capture.ps1` (schtask `helios_paintcap`) | **The only rendering evidence that counts** — writes `Z:\tmp\screen_copy.png` (GDI `CopyFromScreen` of the composed primary) and `Z:\tmp\progman_printwindow.png`. |
| `tools/vnc_shot.py` | Host-side single-frame PNG off QEMU's RFB (`:1-15`; QMP `screendump` returns "no surface" under DMABUF scanout, so this is the only host-side shot). |
| `tools/vnc_frame_probe.py` + `tools/vnc_scanout_correlate.py` | The 0ab instrument: per-update HUD-rectangle completeness oracle on `CLOCK_REALTIME`, correlated with QEMU `virtio_gpu_cmd_set_scanout_blob` / `_res_flush` trace lines. Gives a D3D12 client a **black-frame %** and a present→scanout distribution comparable to the D3D11 numbers. |
| `tools/scanout_timeline_dump.c` | `--cursor` / `--dump first last` around a run (32 768-slot ring); already wired into `tmp/perf/run-gt1-arm.ps1:47-95`. |
| `tools/vk_surface_recreate_probe.cpp` | The exact vkd3d resize/fullscreen shape (two `VkSurface`s on one HWND) that broke the per-HWND dcomp target cache — `DX12.md:163-167`. Run it *before* blaming D3D12 for a resize failure. |
| `tools/live_dump.cpp` | `MiniDumpWriteDump` for a wedged test process. |
| `tools/vram_report_probe.cpp` | DXGI/VidMm vs Venus heaps; the natural home for a `QueryVideoMemoryInfo` arm once a D3D12 device exists (`DX12.md:250-254`). |

### 3.2 Worth writing as D3D12 analogues (each mirrors an existing D3D11 probe)

| New probe | Mirrors | Why |
|---|---|---|
| `tools/d3d12_devicecreate_probe.cpp` | `d3d11_devicecreate_probe.cpp` | proves the runtime reaches `D3D12CreateDevice` on the Helios adapter, by description not index |
| `tools/d3d12_caps_dump.cpp` | `d3d11_fl_probe.cpp` + `format_caps` | dumps every `D3D12_FEATURE_*` struct to CSV; the honesty baseline for §6's G7 |
| `tools/d3d12_clear_probe.cpp` | `helios_clear_test.cpp` | clear → `CopyResource` to READBACK heap → `Map` → read pixel 0. **Headless correctness with no swapchain** |
| `tools/d3d12_triangle.cpp` | `d3d11_triangle.cpp` | real HWND, explicit adapter, FLIP_DISCARD vs BLT arms, optional pre-Present readback — separates "app rendered" from "DWM composited" |
| `tools/d3d12_format_matrix_probe.cpp` | `CONFORMANCE.md` C5 | `CheckFeatureSupport(D3D12_FEATURE_FORMAT_SUPPORT)` over the DXGI format range, CSV, checked-in baseline |
| `tools/d3d12_fence_probe.cpp` | `d3dkmt_sync_probe.c` + `vehicle_flipwait_probe.c` | `ID3D12Fence` → monitored fence: CPU signal, GPU signal, `SetEventOnCompletion`, cross-queue wait |
| `packaging/windows/probes/d3d12-smoke.cpp` | `probes/d3d11-smoke.cpp` | factory → find "Helios" adapter → `D3D12CreateDevice` → exit 1/2; the shipping gate (§5) |

Compile recipe (from `CONFORMANCE.md:307-315`), unchanged except the libs:
```
cl /nologo /EHsc /W4 Z:\tools\d3d12_triangle.cpp /Fe:C:\Windows\Temp\x\p.exe /link d3d12.lib dxgi.lib dxguid.lib
```
Never build onto `Z:\` (`CLAUDE.md` — `OS error 87` on the 9p share).

### 3.3 The one automated gate today, and what D3D12 adds to it

`packaging/windows/Verify-Helios.ps1` verifies install-state hashes, PnP status/provider, the
Vulkan ICD registry entry, `OpenGLDriverName`, and the OpenCL vendor key (`:17-66`), then with
`-RunSmokeTests` runs four probes from `<installRoot>\runtime\smoke` in order — Vulkan, Direct3D 11,
OpenGL, OpenCL (`:70-85`) — failing on any non-zero exit (`:84`) and throwing at `:89-91`.
⚠ A **missing** probe is a warning, not a failure (`:78-80`) — `CONFORMANCE.md` C8. Adding D3D12
means: a fifth entry in the `$tests` array at `:70-75`, a `cl.exe` line in
`ci/windows/Build-SmokeTests.ps1` (pattern at `:18-25`), and a copy line in
`ci/windows/Assemble-Package.ps1:76-77` (the `@("vulkan-smoke.exe","d3d11-smoke.exe",
"opengl-smoke.exe","opencl-smoke.exe")` loop).

### 3.4 Present-path measurement (rungs 2-6)

Mirror what WS2 did for D3D11: `tools/vnc_frame_probe.py` with a completeness-oracle rectangle over
the demo's own animated region, correlated by `tools/vnc_scanout_correlate.py` against
`/tmp/helios-qemu-stderr.log` with `HELIOS_QEMU_TRACE` enabling
`virtio_gpu_cmd_set_scanout_blob` / `_res_flush` (`ROADMAP.md:3440-3444`). ⚠ the correlator's own
header warns that `set_scanout_blob` lines begin `id 0, res 0x..` and a `\D*` between event name and
`res` silently drops every blob line (`vnc_scanout_correlate.py:10-13`).

### 3.5 Session-1 execution — non-negotiable

`win_exec`/SSH land in **session 0**, which has no desktop; a session-0 benchmark fakes a driver
regression (memory 60th). The canonical five lines are `tmp/perf/launch-gt1-arm.ps1:16-24`:

```powershell
[xml]$xml = (schtasks /query /tn helios_perf_fs /xml ONE | Out-String)
$xml.Task.Actions.Exec.Arguments = "-NoProfile -ExecutionPolicy Bypass -File Z:\path\runner.ps1 …"
$xml.Save($taskXml)
schtasks /create /tn $taskName /xml $taskXml /f
schtasks /run   /tn $taskName
```
Existing tasks to clone from: `helios_perf_fs` (interactive benchmark principal), `helios_paintcap`,
`helios_flprobe`, `helios_ringprobe`, `helios_dcomp_probe`, `helios_repaint`, `helios_flasher`,
`helios_dstate`, `helios_enum_windows` (`CONFORMANCE.md:329-333`).
⚠ Elevated processes silently ignore `VK_DRIVER_FILES`/`VK_ICD_FILENAMES`, and win_exec shells are
High-IL — ICD A/B arms need a `/rl LIMITED` task (`CONFORMANCE.md:335-338`, `ROADMAP.md:3422-3426`).

`tmp/perf/run-gt1-arm.ps1` is the ready-made *wrapper shape* for any gated run: pre snapshot →
`read_ledger_dump` → timeline cursor → the workload → post cursor/dump → post snapshot → copy the
newest `umd-*.log` into the artifact dir (`:41-113`). A D3D12 gate runner should be a copy of it
with the workload line swapped.

---

## 4. Benchmarks and real workloads

### 4.1 What is installed — verified, not assumed

`win$ Get-ChildItem 'C:\ProgramData\UL\3DMark\chops\dlc' -Directory` → 23 DLC packs, all populated
(sizes in MB): `time-spy-test 1840.6, speed-way-test 1696.3, directstorage-feature-test 1661.6,
steel-nomad-test 1346, port-royal-test 796.8, fire-strike-test 717.1, vrs-feature-test 714.7,
night-raid-test 703.3, directx-raytracing-feature-test 426.8, sampler-feedback-feature-test 391.7,
wild-life-test 389.7, solar-bay-extreme-test 279.5, nvidia-dlss-test 269.2, ice-storm-test 238.3,
cloud-gate-test 236.5, intel-xess-feature-test 215.7, mesh-shader-feature-test 169.8,
pci-express-test 166.5, solar-bay-test 144.4, storage-test 83.9, amd-fsr-feature-test 57.4,
cpu-profile-test 54.3, systeminfo 3.5`.

The graphics API of each workload was determined by scanning each workload exe for imported DLL
name strings (`win$` loop over `[System.IO.File]::ReadAllBytes` + `Contains`):

| Workload exe | d3d12.dll | d3d11.dll | vulkan-1.dll |
|---|---|---|---|
| `3DMarkTimeSpy.exe` | ✔ | | |
| `3DMarkNightRaid.exe` (x64, Win32, ARM, ARM64) | ✔ | | |
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
| `3DMarkCPUProfile.exe` | ✔ | | |
| `3DMarkICFWorkload.exe` / `ICFDemo.exe` (Fire Strike, Cloud Gate, Ice Storm) | | ✔ | (d3d9.dll) |

That is **hard local evidence** for the D3D11 vs D3D12 vs Vulkan split — no need to trust a memory
of UL's marketing. Note `3DMarkNightRaid.exe` exists as **Win32** as well as x64
(`…\night-raid-test\bin\Win32\3DMarkNightRaid.exe`), which gives a WOW64 D3D12 arm for free.

Benchmark definitions: `C:\Program Files\UL\3DMark\{timespy,nightraid,portroyal,speedway,
steelnomad_dx12,solarbay*,vrs,vrs2,meshshaderft,samplerfeedbackft,directxraytracingft,
directstorageft,apioverhead}.3dmdef` and their `custom_*` / `stresstest_*` variants
(`win$ Get-ChildItem 'C:\Program Files\UL\3DMark' -File`). ⚠ `apioverhead.3dmdef` exists but there
is **no** `api-overhead` DLC directory — that test is not installed.

Definition shape, e.g. `timespy.3dmdef` selects workloads `TimeSpyDemoP, TimeSpyGt1P, TimeSpyGt2P,
TimeSpyCpuP` and carries an empty `dxgi_adapter_luid` setting; `steelnomad_dx12.3dmdef` selects
`SteelNomadGt1DX` with an `adapter_luid` setting. A single-graphics-test def (the fast-iteration
trick) is `tmp/perf/fs_gt1.3dmdef` — copy that shape for `TimeSpyGt1P` / `NightRaidGt1P`.

**No D3D12 workload has ever been attempted on this box:**
```
win$ Select-String -Path 'C:\Program Files\UL\3DMark\debug.log' `
       -Pattern 'D3D12|DirectX 12|Time Spy|TimeSpy|Night Raid|Steel Nomad'   # → no matches (1 179 282 B log)
```

### 4.2 Requirement ladder and the milestone

Per UL's own support pages: Time Spy uses **DirectX 12 feature level 11_0** with 4 GB VRAM;
Night Raid is the **DX12 engine optimised for integrated graphics**, 1 GB VRAM; Speed Way requires
**DX12 Ultimate** (DXR tier 1.1 + mesh shaders) and 6 GB VRAM
([Time Spy overview](https://support.benchmarks.ul.com/support/solutions/articles/44002136075-overview-of-3dmark-time-spy-benchmark),
[Time Spy sysreqs](https://support.benchmarks.ul.com/support/solutions/articles/44002136088-3dmark-time-spy-system-requirements),
[Night Raid sysreqs](https://support.benchmarks.ul.com/support/solutions/articles/44002135995-3dmark-night-raid-system-requirements),
[Speed Way overview](https://support.benchmarks.ul.com/support/solutions/articles/44002378655-overview-of-3dmark-speed-way-benchmark)).

Ordering by driver demand: **Night Raid → Time Spy → Steel Nomad (DX12) → Port Royal / Solar Bay
(DXR) → Speed Way (Ultimate)**. Feature tests (mesh shader, sampler feedback, VRS, DXR) are *tier
probes*, not benchmarks — run them to find out what is backed, cheaply, before attempting the
benchmarks that need those tiers.

**The milestone should be `NightRaidGt1P` completing with a score and an owner-visible frame**, not
Time Spy: it is the lowest-demand installed D3D12 workload, it is the one whose failure is most
likely to be a real driver defect rather than a missing tier, and it has a Win32 build for the
WOW64 arm. **Time Spy Graphics score** is the credible *second* result and the natural headline
number, directly comparable in kind to the existing Fire Strike Graphics ≈ 49k
(`CLAUDE.md` stage paragraph).

**Deployment trick that makes this cheap and reversible:** each workload is its own exe in its own
directory (`C:\ProgramData\UL\3DMark\chops\dlc\night-raid-test\bin\x64\3DMarkNightRaid.exe`), and
`d3d12.dll` is not a KnownDLL (§1.6) — so **strategy (b) can be A/B-tested per workload by copying
`d3d12.dll` + `d3d12core.dll` (+ DXVK's `dxgi.dll`, §2.4/§7) next to that one exe**, with no
system-wide install and a one-file-delete rollback.

⚠ Directive in force (memory, 68th): **a frozen benchmark is a defect to root-cause, never a
retry.** Knob-ON / experimental runs happen trap-armed with a registered hypothesis.

---

## 5. CI

### 5.1 What `windows-stack.yml` builds today

Five jobs (`.github/workflows/windows-stack.yml`): `driver` (WDDM driver + D3D11 UMD, via
`ci/windows/Build-Driver.ps1`, which meson-builds **DXVK with clang-cl** — note `-Denable_d3d11=true
-Denable_dxgi=true` at `Build-Driver.ps1:55-56` — then `cargo make` for the KMD), `mesa` (msys2
UCRT64 → `ci/windows/build-mesa.sh`), `opencl` (CLVK), `loaders` (Khronos loaders **and the four
smoke probes**, `Build-KhronosLoaders.ps1` → `Build-SmokeTests.ps1`), and `package` (Inf2Cat +
signtool + `Assemble-Package.ps1`). All jobs are `runs-on: windows-2022`.
There is **no vkd3d step anywhere** — consistent with `DX12.md:80-82`.

### 5.2 What has to change, concretely

Adding vkd3d-proton to the bundle needs **widl, glslang, meson, and a C toolchain in one image**.
The Helios image has meson (pip), and the Vulkan SDK (which carries `glslangValidator`) in the
`driver`/`opencl` jobs, but **no widl**. Two credible shapes, both taken from upstream vkd3d CI:

**(a) MSVC on `windows-2022`** — exactly upstream's `test-build-windows.yml`:
```yaml
- run: |
    choco install strawberryperl -y                       # ships widl
    Invoke-WebRequest -Uri "https://raw.githubusercontent.com/HansKristian-Work/vkd3d-proton-ci/main/glslangValidator.exe" `
      -OutFile "C:\Strawberry\c\bin\glslangValidator.exe"
    echo "C:\Strawberry\c\bin" >> $env:GITHUB_PATH
- run: pip install meson
- run: meson setup -Denable_tests=True -Denable_extras=True --buildtype release --backend vs2022 build-msvc-x64
- run: msbuild -m build-msvc-x64/vkd3d-proton.sln
```
(verbatim shape from `vkd3d-proton-helios/.github/workflows/test-build-windows.yml:17-52`).
⚠ Upstream itself says MSVC builds are "only expected to be used for testing and development …
we do not stress test these builds at all" (`README.md:138-142`) — fine for producing `d3d12.exe`,
questionable for the shipped `d3d12.dll`.

**(b) mingw cross-build on Linux** — upstream's *release* path
(`test-build-linux.yml`, `artifacts.yml`, both `runs-on: ubuntu-24.04` using
`misyltoad/arch-mingw-github-action@v8`). This is the shape that produces the DLLs Proton ships,
and it is the same command that works on this Linux host today (§1.4). It adds a **non-Windows job**
to a Windows-only workflow, which is a structural change but a small one.

**Either way the workflow also needs:** `submodules: recursive` for `vkd3d-proton-helios` (the
existing jobs use `submodules: false` + a targeted `git submodule update --init`, e.g.
`.github/workflows/windows-stack.yml:39-41` for dxvk) — and vkd3d's *nested* submodules
(`khronos/*`, `subprojects/dxil-spirv`) must come with it.

**Packaging.** If vkd3d ships in the bundle, `Assemble-Package.ps1` gains a `vkd3d` payload dir,
`Install-Helios.ps1` records the files in `install-state.json.runtimeFiles` (`:75,87`), and
`Verify-Helios.ps1` gains a D3D12 smoke entry (§3.3). ⚠ Shipping a `d3d12.dll` is **not** a
system-wide install — it must be a per-app drop or an explicit opt-in, because replacing the system
D3D12 runtime for every process is a far larger blast radius than any Helios component has today.

**Toolchain risks named honestly:**
* `choco install strawberryperl` on `windows-2022` pulls ~1.5 GB and is a network dependency; its
  `widl` version is **UNVERIFIED** (settling command in CI: `widl -V`).
* The `glslangValidator.exe` upstream downloads is a raw binary from a third-party repo — a supply-chain
  input this project would be adding. Alternative: the Vulkan SDK already installed by the
  `driver`/`opencl` jobs carries `glslangValidator` (on the VM at `C:\VulkanSDK\1.4.350.0\Bin\`).
* MSVC vs mingw ABI: vkd3d-proton's meson handles both (`meson.build:9-10` `vkd3d_is_msvc` covers
  `msvc` and `clang-cl`), but the `.def`-file path differs (`libs/d3d12/meson.build:18`
  `d3d12_needs_defs = (not vkd3d_is_msvc) and (vkd3d_platform == 'windows')`).
* `tools/win-mcp/src/main.rs` has `win_dxvk` (`:736-776`) mirroring `Z:\dxvk-helios` →
  `C:\Users\Rupansh\dxvk-helios` and building at `C:\Users\Rupansh\dxvk-build` with
  `PATH=LLVM;… && call vcvars64 && meson …`. A `win_vkd3d` tool is a ~40-line copy with new
  constants — but note vkd3d additionally needs widl+glslang on PATH, which `win_dxvk`'s command
  line does not set.

---

## 6. The gate ladder: `D12-G0 … D12-G9`

Rules every gate obeys (from `CLAUDE.md` and the memory index):
* **Only screen evidence counts as rendering evidence** — `helios_paintcap` → `Z:\tmp\screen_copy.png`.
* **Registry counters persist across boots** — always a pre/post `kmd-counter-snapshot.ps1` diff.
* **Anything with a window runs in session 1** through a cloned scheduled task.
* **No caps lies.** `VKD3D_FEATURE_LEVEL` (`libs/vkd3d/device.c:10888-10935`) and the test binary's
  `--feature-level` (`tests/d3d12_crosstest.h:287`) **must not appear in any gate command**; they
  raise advertised tiers without backing them, which is landmine #5 in `DX12.md:416-420`.
* Every gate writes to `tmp/dx12/gates/<gate>/` and records the exact binaries by SHA256.

---

### D12-G0 — Build gate (no driver involved)
**Entry:** none.
**Commands:** §1.4 (Linux cross-build preferred; MSVC on the VM if a debugger is wanted).
**Pass:** all four artifacts exist and are non-empty — `libs/d3d12/d3d12.dll`,
`libs/d3d12core/d3d12core.dll`, `tests/d3d12.exe`, `demos/triangle.exe`; `d3d12.exe --list-tests |
wc -l` == **557**.
**Counters:** none.
**Artifact:** `tmp/dx12/gates/G0/build.log`, `sha256sums.txt`, `list-tests.txt`.
**Note:** G1-G3 can be run *before* G0 using the prebuilt DLLs at
`C:\Program Files\Looking Glass (IDD)\D3DTranslation\` (§1.6) — but a gate result is only citable
once the binary's provenance is recorded, so record the SHA256 of whichever pair was used.

### D12-G1 — Device gate: does a D3D12 device exist on Helios at all?
**Entry:** G0 (or the prebuilt pair).
**Commands** (session 0 is fine — no window):
```powershell
# 1. identify the adapter
C:\Windows\Temp\x\dxgi_luid_dump.exe > G1\adapters.txt      # tools/dxgi_luid_dump.cpp
# 2. strategy (b): vkd3d DLLs beside the exe
copy d3d12.dll d3d12core.dll  <testdir>\
$env:VKD3D_DEBUG='info'; $env:VKD3D_LOG_FILE='Z:\tmp\dx12\gates\G1\vkd3d.log'
$env:VKD3D_TEST_MATCH='test_create_device'
<testdir>\d3d12.exe --adapter <N>   > G1\create_device.txt 2>&1
# 3. strategy (a) control arm: same exe, vkd3d DLLs REMOVED -> system d3d12.dll
```
**Pass:** the summary line reads `1 tests executed (0 failures, 0 successful todo, **0 skipped**,
…)`; the vkd3d log contains the `vkd3d-proton - build:` banner and names the Helios/venus physical
device. **`0 skipped` is the whole point** (§1.2).
**Counters:** `kmd-counter-snapshot.ps1` pre/post + `kmd-gate-surface.ps1` (exit 0).
**Artifact:** `tmp/dx12/gates/G1/{adapters.txt,create_device.txt,vkd3d.log,counters-pre,counters-post}`.

### D12-G2 — Headless correctness: the 545-test suite
**Entry:** G1 green.
**Commands:**
```
& 'C:\Program Files\Git\bin\bash.exe' -c \
  "./test-runner.sh -o /z/tmp/dx12/gates/G2/logs -j 1 /c/…/tests/d3d12.exe"
```
(add `--adapter <N>` by wrapping the exe, or set `VKD3D_FILTER_DEVICE_NAME`; `-j 1` first, `-j 2`
once stable — §1.5.)
**Pass:** *first* run establishes the baseline triple `(executed, failures, skipped)` and the
known-fail list; thereafter **no new failing test name** and `skipped` not increased. A run whose
`skipped` count is ≈ 545 is a **G1 regression masquerading as a pass**.
**Counters:** pre/post KMD snapshot; `umd-gate-surface.ps1 -AllProcesses -SinceMinutes 60`
(expected: no D3D11 UMD activity under strategy (b) — record it either way).
**Artifact:** `tmp/dx12/gates/G2/logs/*.log` + `summary.csv` (test, executed, failures, skipped,
todo) + `known-fail.txt`.
**Variant `D12-G2a`:** the identical run with the vkd3d DLLs removed = the conformance gate for a
native Helios D3D12 UMD (strategy (a)). Today it will show 545 skips against
`OpenAdapter12 → DXGI_ERROR_UNSUPPORTED` (`umd/src/adapter.rs:177-189`, quoted in `DX12.md:20-27`) —
which is the honest zero-point to measure future UMD work against.

### D12-G3 — First D3D12 frame on the screen
**Entry:** G2 baseline recorded.
**Commands:** clone `helios_perf_fs` into `helios_d12_triangle` running
`demos\triangle.exe`, wait ~10 s, then `schtasks /run /tn helios_paintcap`; also
`linux$ python3 tools/vnc_shot.py --out tmp/dx12/gates/G3/host-shot.png`.
**Pass:** **owner-visible** triangle in `Z:\tmp\screen_copy.png` *and* in the host RFB shot. Log
lines are not frames.
**Counters:** KMD pre/post diff with `kmd-gate-surface.ps1` exit 0; scanout counters
(`ScBadAlc…ScGateCx`) at 0.
**Artifact:** both PNGs, the counter diff, `tmp/dx12/gates/G3/notes.md`.

### D12-G4 — Present-path characterisation
**Entry:** G3.
**Commands:** run `demos\gears.exe` (animated) for 120 s in session 1 while
`tools/vnc_frame_probe.py` samples with a completeness rectangle; enable the QEMU trace
(`HELIOS_QEMU_TRACE=virtio_gpu_cmd_set_scanout_blob,virtio_gpu_cmd_res_flush`) and run
`tools/vnc_scanout_correlate.py frames.jsonl /tmp/helios-qemu-stderr.log d12-gears`. Then
`tools/vk_surface_recreate_probe.cpp` for the resize/second-surface shape.
**Pass:** a **black-frame percentage** and a present→scanout distribution, both recorded, with the
black-frame % at or below the D3D11 0ab-C close-out figure (0.02 %, memory 64th); zero occurrences
of the one-target-per-HWND failure, or a filed ROADMAP defect with a reproducer.
**Artifact:** `tmp/dx12/gates/G4/{frames.jsonl,correlate.txt,numbers.md}`.

### D12-G5 — First real D3D12 application
**Entry:** G4.
**Commands:** copy `d3d12.dll`,`d3d12core.dll`(+`dxgi.dll` if required, §7) beside
`C:\ProgramData\UL\3DMark\chops\dlc\night-raid-test\bin\x64\3DMarkNightRaid.exe`; build a
`nightraid_gt1.3dmdef` in the shape of `tmp/perf/fs_gt1.3dmdef` selecting `NightRaidGt1P`; run
through a `run-gt1-arm.ps1`-shaped wrapper via a cloned task.
**Pass:** the run completes (**duration > 0 and a score present** — a Fire Strike run can report
Graphics=0 and still write a result file, memory 64th), a screenshot shows the workload, all KMD
failure counters 0, and no `DDI refusals:` line appears that was not there before.
**Artifact:** `.3dmark-result`, the run log, screenshot, counter diff.
**Directive:** a freeze here is a defect to root-cause, not a retry.

### D12-G6 — The headline benchmark
**Entry:** G5 green three runs running.
**Commands:** `TimeSpyGt1P` (then the full `timespy.3dmdef`) through the same wrapper; **3-run
median**, never a single run (GT-score spread is ±5-6 % run to run, memory 62nd/65th).
**Pass:** a recorded Time Spy Graphics score with its 3-run median and spread — this is the D3D12
analogue of Fire Strike Graphics ≈ 49k and the number the stage is judged on.
**Artifact:** `tmp/dx12/gates/G6/results/*.3dmark-result`, `medians.md`.

### D12-G7 — Caps honesty gate
**Entry:** G5.
**Commands:** `tools/d3d12_caps_dump.cpp` (to write) dumping every `D3D12_FEATURE_*` struct; the
3DMark **feature tests** (`meshshaderft`, `samplerfeedbackft`, `vrs`, `vrs2`,
`directxraytracingft`) as independent confirmation of each advertised tier.
**Pass:** every advertised tier is exercised by *something* that passes, or is demoted. No
`VKD3D_FEATURE_LEVEL` anywhere in the run. Each unbacked-but-advertised tier is a filed defect.
**Artifact:** `caps.csv` (checked in as the baseline), the feature-test results.

### D12-G8 — Packaged smoke gate
**Entry:** G5.
**Commands:** `packaging/windows/Verify-Helios.ps1 -RunSmokeTests` with a new
`d3d12-smoke.exe` entry.
**Pass:** exit 0 with the D3D12 probe actually **present and run** — and fix
`Verify-Helios.ps1:78-80` so a missing probe fails when `-RunSmokeTests` was requested
(`CONFORMANCE.md` C8), otherwise this gate can pass on a bundle with no D3D12 probe in it at all.
**Artifact:** the Verify transcript.

### D12-G9 — CI gate
**Entry:** G0 reproducible by hand.
**Commands:** the new workflow job (§5.2).
**Pass:** the job builds `d3d12.dll`, `d3d12core.dll`, `tests/d3d12.exe`, `demos/triangle.exe`,
uploads them as an artifact with SHA256s, and the `package` job records the vkd3d commit next to
`mesa`/`dxvk` in the metadata step (`windows-stack.yml:222-226`).
**Artifact:** the CI artifact + a green run URL.

---

## 7. D3D12 failure-triage playbook

### 7.1 vkd3d-proton's own instrumentation (names verified in the source/README)

| Variable | Effect | Source |
|---|---|---|
| `VKD3D_DEBUG=none\|err\|info\|fixme\|warn\|trace` | vkd3d log level. The build banner `vkd3d-proton - build: %015llx` is **INFO** | README:211-213; `libs/vkd3d/device.c:1479-1481` |
| `VKD3D_SHADER_DEBUG=<same values>` | shader-compiler log level | README:213-214 |
| `VKD3D_LOG_FILE=<path>` | redirect the log to a file (essential in session 1) | README:215 |
| `VKD3D_CONFIG=vk_debug` | enable Vulkan debug extensions + validation layer | README:201 |
| `VKD3D_CONFIG=single_queue` | no async compute/transfer queues — **the first thing to try on a single-node adapter** | README:204 |
| `VKD3D_CONFIG=no_upload_hvv` | block host-visible VRAM for the UPLOAD heap | README:205-208 |
| `VKD3D_CONFIG=nodxr` / `dxr` / `dxr12` | force DXR off/on | README:200-202 |
| `VKD3D_CONFIG=breadcrumbs` | instrument command lists with `VK_AMD_buffer_marker` / `VK_NV_device_checkpoints`; on device-lost/timeout, dumps the executing command lists. **Trace-enabled builds only** (`meson.build:57-60`: `enable_breadcrumbs = enable_trace`, and `enable_trace` is `auto` → true only for debug/debugoptimized, `meson.build:26-30`) | README:305-312 |
| `VKD3D_DISABLE_EXTENSIONS=<list>` | bisect a suspect Vulkan extension | README:220-221 |
| `VKD3D_VULKAN_DEVICE=<idx>`, `VKD3D_FILTER_DEVICE_NAME=<substr>` | force the physical device | README:216-219 |
| `VKD3D_SHADER_DUMP_PATH`, `VKD3D_SHADER_OVERRIDE` | dump `$hash.{spv,dxbc,dxil}`; substitute a SPIR-V | README:291-295 |
| `VKD3D_SHADER_CACHE_PATH=0` | disable the on-disk SPIR-V cache — **set it for every gate run**, or a stale cache hides a shader-translation regression | README:262-265; the runner already exports it (`test-runner.sh:10`) |
| `VKD3D_FRAME_RATE` | frame-rate cap | README:243 |
| `-Denable_descriptor_qa=true` + `VKD3D_DESCRIPTOR_QA_LOG` + `VKD3D_CONFIG=descriptor_qa_checks` | GPU-assisted descriptor validation; prints `Enabling descriptor QA checks!` | README:362-372 |

⚠ **Never leave `VKD3D_FEATURE_LEVEL` set.** It raises `TiledResourcesTier`, `ResourceBindingTier`,
`ROVsSupported`, `RaytracingTier`, `MeshShaderTier`, `SamplerFeedbackTier` and `max_shader_model`
without any backing (`libs/vkd3d/device.c:10906-10935`). That is precisely the class of unbacked
advertisement that cost this project `SupportDirectFlip` and `FlipImmediateMmIo`
(`DX12.md:416-420`).

### 7.2 Windows-side instruments

**ETW providers — all three verified present on this VM** (`win$ logman query providers`):
```
Microsoft-Windows-Direct3D12   {5D8087DD-3A9B-4F56-90DF-49196CDC4F11}
Microsoft-Windows-DXGI         {CA11C036-0102-4A2D-A6AD-F03CFED5D3C9}
Microsoft-Windows-DXGIDebug    {F1FF64EF-FAF3-5699-8E51-F6EC2FBD97D1}
Microsoft-Windows-DxgKrnl      {802EC45A-1E99-4B83-9920-87C98277BA9D}
Microsoft-Windows-Direct3D11   {DB6F6DDB-AC77-4E88-8253-819DF9BBF140}
Microsoft-Windows-DxgKrnl-SysMm{9DE90B19-62C4-511D-A1C5-9E990812D18B}
```
Recipes, straight from the D3D11 experience:
* **"why did device creation fail"** — the DXGI provider prints the runtime's exact rejection
  string when no device exists to hold an InfoQueue (`ROADMAP.md:3150-3156`):
  `logman start helios_d3d12 -p Microsoft-Windows-DXGI 0xFFFFFFFFFFFFFFFF 0xff -o x.etl -ets`,
  `logman update helios_d3d12 -p Microsoft-Windows-Direct3D12 0xFFFFFFFFFFFFFFFF 0xff -ets`,
  run the probe, `logman stop helios_d3d12 -ets`, `tracerpt x.etl -o x.xml -of XML -y`, read
  `<Data Name="Message">` / `Code`. **Swap `Microsoft-Windows-Direct3D11` for
  `Microsoft-Windows-Direct3D12`** — that is the only change from the D3D11 recipe.
* **"what is dxgkrnl doing to my thread"** —
  `logman create trace -p Microsoft-Windows-DxgKrnl 0xFFFFFFFFFFFFFFFF 0xFF` → tracerpt → grep
  `AzureTriage` for failure reasons in plain text (`ROADMAP.md:3452-3454`); a ~2 s circular slice
  mid-run and read `Present`/`Flip`/`QueuePacket`/`DmaPacket`/`BlockThread` (`CLAUDE.md` §When
  You're Stuck 2 — how the present-queue stall was found).

**D3D12 debug layer — available for strategy (a), a no-op for strategy (b).**
`C:\Windows\System32\d3d12SDKLayers.dll` is present (`win$`), so `--validate` / `--gbv` on the test
binary (`tests/d3d12_crosstest.h:857`) work **against the system `d3d12.dll`**. Under vkd3d they
silently do nothing: `libs/d3d12/main.c:197-204` forwards `D3D12GetDebugInterface` to
`libs/d3d12core/main.c:783-805`, which returns `DXGI_ERROR_SDK_COMPONENT_MISSING` (`:803-804`) for
every IID **except** `IID_ID3D12DeviceRemovedExtendedDataSettings` (`:796-801`), and the test's
`enable_d3d12_debug_layer` only enables the layer `if (SUCCEEDED(...))`. Two consequences:
* the D3D12 debug layer is a **strategy-(a)-only** instrument — a strong argument for keeping the
  G2a control arm alive whichever strategy wins;
* **DRED is available under vkd3d** (`ID3D12DeviceRemovedExtendedDataSettings`) and is the right
  first instrument for a D3D12 device-removed / hang, alongside `VKD3D_CONFIG=breadcrumbs`.

Also verified: `D3D12EnableExperimentalFeatures` under vkd3d returns `E_NOINTERFACE`
(`libs/d3d12core/main.c:807-813`, logged `FIXME … stub!`), so the tests' SM6.3+ experimental enable
(`tests/d3d12_crosstest.h:459-460`) is a no-op there — harmless, since vkd3d decides shader models
from Vulkan caps, but it means "experimental shader models" is not a knob on that path.

**Helios instruments:** `tools/umd-gate-surface.ps1` (UMD log, first-hit-only counters — absence is
the zero reading), `tools/kmd-gate-surface.ps1` + `tools/kmd-counter-snapshot.ps1` diff (registry,
persists across boots), `tools/scanout_timeline_dump.c`, `tools/read_ledger_dump.c`.

**Host / venus:** `/tmp/helios-qemu-stderr.log` (launcher tee). ⚠ virglrenderer's `vkr_log`/`proxy_log`
are INFO-level and therefore **silent on the release build** — absence of host lines below WARNING
proves nothing; a real host bisect needs a relaunch with `VIRGL_LOG_LEVEL=debug`
(`ROADMAP.md:3431-3434`), and VM/QEMU relaunches are owner-gated. `HELIOS_VKR_DEBUG=validate`
enables host validation layers (`CLAUDE.md`).

**KD:** `NTOSEYE.md`; `tools/take-minidump.ps1` / `tools/live_dump.cpp` for a wedged test process;
`.frame /r`, not `.trap` (memory 62nd).

### 7.3 Triage decision order for a D3D12 failure

1. Does `test_create_device` still pass (**G1**)? If not, everything else is noise.
2. Is it in the G2 known-fail list? If yes it is a *baseline* failure, not a regression.
3. Does it reproduce under `VKD3D_CONFIG=single_queue`? (isolates multi-queue against the
   single-3D-node adapter).
4. Does it reproduce with `VKD3D_SHADER_CACHE_PATH=0`? (isolates a stale SPIR-V cache).
5. Does the same test pass against **WARP** (`d3d12.exe --warp`, `tests/d3d12_crosstest.h:844`)?
   WARP is a driver-free control arm on the same OS and runtime — it separates "our stack" from
   "the test/harness/OS".
6. Vulkan-layer attribution: `VKD3D_CONFIG=vk_debug` for validation, then the venus/host log.
   **Never blame the host without host-side evidence.**

---

## 8. UNVERIFIED, with the settling experiment

1. **Does vkd3d-proton create a device on the Helios ICD at all?** → D12-G1. (`DX12.md:170` records
   that nothing in the tree has ever tried.)
2. **Which vkd3d-proton version is the Looking Glass prebuilt?** → run it with `VKD3D_DEBUG=info
   VKD3D_LOG_FILE=…` and read the `vkd3d-proton - build: %015llx` banner
   (`libs/vkd3d/device.c:1479-1481`).
3. **Does the VM have network access for nuget/choco?** (needed for the MS samples and for CI
   shape (a)) → `win$ (Invoke-WebRequest https://api.nuget.org/v3/index.json -UseBasicParsing).StatusCode`.
4. **What does `dxil-spirv` pull in transitively?** → `git -C vkd3d-proton-helios submodule update
   --init subprojects/dxil-spirv && cat subprojects/dxil-spirv/.gitmodules`.
5. **Does a vkd3d D3D12 swapchain present on this stack without DXVK's `dxgi.dll`?**
   vkd3d implements only `IDXGIVkSwapChainFactory` (`libs/vkd3d/swapchain.c:3942-3968`,
   `libs/vkd3d/command.c:22282-22284` exposes it from the command queue) and DXVK's DXGI is what
   consumes it (`dxvk-helios/src/dxgi/dxgi_factory.cpp:556-573`: QI for `IDXGIVkSwapChainFactory`,
   else `DXGI_ERROR_UNSUPPORTED`). README:170-173 says DXVK 2.1+ and vkd3d share a DXGI
   implementation. `dxvk-helios` **does** build a `dxgi.dll` (`src/dxgi/meson.build:27`) and one
   already exists on the VM (`win$ C:\Users\Rupansh\dxvk-build\src\dxgi\dxgi.dll`, 4 190 208 B).
   → settle by running `demos/triangle.exe` with, and without, `dxgi.dll` beside it and reading the
   `CreateSwapChainForHwnd` HRESULT. **This is R7's lane; flagged here because G3 depends on it.**
6. **Does the system DXGI accept a vkd3d `ID3D12CommandQueue`?** Implied "no" by (5), but not
   directly verified → same experiment, the no-`dxgi.dll` arm.
7. **Does `--adapter N` reliably reach the Helios adapter with DXVK's DXGI in the path?** DXVK's
   DXGI enumerates via Vulkan, not the WDDM adapter list, so indices may differ from
   `dxgi_luid_dump` → run `dxgi_luid_dump.exe` twice, with and without `dxgi.dll` beside it.
8. ~~Is the D3D12 debug layer usable under vkd3d?~~ **SETTLED during this survey** — no
   (`libs/d3d12core/main.c:783-805` returns `DXGI_ERROR_SDK_COMPONENT_MISSING` for every IID but
   DRED). See §7.2. Remaining sub-question: does the *system* debug layer produce useful output
   against strategy (a) once `OpenAdapter12` stops refusing → `d3d12.exe --validate` at G2a.
9. **What `skipped` count does a *healthy* driver produce on this suite?** No upstream number is
   published → establish Helios' own baseline at G2 and, if a second machine is ever available,
   take a WARP baseline (`--warp`) on the same OS build as an upper bound.
10. **Does the 16-way parallel `test-runner.sh` default wedge this adapter?** → run `-j 1`, then
    `-j 2`, then `-j 4`, recording wall time and any wedge; the ring-wait wedge class was only
    bounded in `icd f0c7bcd3465` (memory 67th).
11. **Which msys2/choco package supplies `widl` on a GitHub `windows-2022` runner?** Upstream uses
    Strawberry Perl; the version is unpinned → `choco install strawberryperl -y; widl -V` in a
    throwaway CI run.

---

## 9. Direct implications for the plan

* **The test asset already exists and is strategy-agnostic.** 557 tests in one binary that picks its
  `d3d12.dll` from the exe directory means the *same* gate ladder measures a vkd3d front end and a
  native Helios D3D12 UMD. Building it is a prerequisite for `DX12.md` D0, not a later step.
* **`DX12.md` D0's "run vkd3d's own test suite" is a one-afternoon task, not a project** — both
  toolchains are installed, and a prebuilt vkd3d-proton is already on the VM for a zero-build first
  answer.
* **Guard the skip counter or the whole ladder is fake.** This is the single most likely way a
  D3D12 bring-up reports success while nothing works.
* **Night Raid, not Time Spy, is the first real-app milestone**, and per-workload DLL drop makes it a
  reversible, per-exe experiment with no system install.
* **The present path is the unknown, exactly as `DX12.md:154-167` says.** G3/G4 are where the plan
  will actually hurt, and the instruments for them (RFB sampler + correlator + paintcap) are already
  built and were the tools that closed 0ab.
* **CI is the cheap part.** Upstream's own two workflows are copy-pasteable; the only new inputs are
  widl and glslang.
