# CONFORMANCE.md — the D3D11 correctness charter

**What this is:** the working charter for D3D11 correctness on Helios — what "conformant" can
mean for this stack today, the exact instruments that report a refused or unimplemented DDI, a
catalogue of the ~60 probe sources already in `tools/` and `packaging/windows/probes/`, the
correctness gaps that have evidence behind them, and a prioritized backlog.

**What this is not:** a certification plan. There is no HLK/WHQL material in this tree (verified:
a case-insensitive grep for `HLK`, `WHQL`, `Windows Hardware Lab` and `hardware certification`
across `*.md`, `*.ps1`, `*.rs`, `*.yml`, `*.inx`, excluding the vendored submodules, returns zero
hits). It is also not a performance document — see `ROADMAP.md` Workstream 2.

---

## 1. What "conformant" means for this stack today

The D3D11 surface is a chain, and "conformance" is a property of the whole chain:

```
app / dwm
  └ d3d11.dll (Microsoft runtime)
      └ umd/            Rust d3d10umddi frontend  (helios_umd.dll)
          └ umd/bridge/ cxx bridge
              └ dxvk-helios/   forked DXVK engine  → Vulkan
                  └ icd/mesa   Venus Vulkan ICD    → virtio-gpu ring
                      └ kmd_render/  WDDM miniport → host virglrenderer/venus
```

### What the driver claims

- **DDI versions advertised** (`umd/src/adapter.rs:29-33`): `D3DWDDM1_3_DDI_SUPPORTED` (11.16.1),
  `D3D11_1_DDI_SUPPORTED` (11.15.0), `D3D11_0_DDI_SUPPORTED` (11.10.2). The negotiated interface
  is a closed set (`NegotiatedInterface`, `adapter.rs:47-95`); anything outside it is refused with
  `E_NOTIMPL` rather than defaulted.
- **Feature profile** (`umd/src/caps.rs:57-84`), default `FeatureLevel11=1`/absent:
  `pipeline_mask = LVL_10_0|LVL_10_1|LVL_11_0|LVL_11_1|LVL_12_0` where `LVL_12_0 = 1<<7`.
  Bit 7 is `D3DWDDM2_0DDI_3DPIPELINELEVEL_12_0` in the generated header
  (`umd/target/.../out/d3d10umddi.rs:24811`). Bits 4/5/6 — `9_1`, `9_2`, `9_3` — are **not** set.
  `shader_caps = COMPUTE | TYPED_UAV_LOAD_ADDITIONAL_FORMATS`; `options1 = TILED_RESOURCES_TIER_2`.
- **Tiled resources** are wired, not stubbed: `pfnUpdateTileMappings`, `pfnCopyTileMappings`,
  `pfnCopyTiles`, `pfnUpdateTiles`, `pfnTiledResourceBarrier`, `pfnGetMipPacking`,
  `pfnResizeTilePool` all forward to DXVK's `ID3D11DeviceContext2` methods
  (`umd/src/forward/tables.rs:296-303`, bodies in `umd/src/forward/tiles.rs`). They live in the
  **WDDM1_3 table only**, so they are unreachable on a device that negotiates 11.0 or 11.1.

### Which suites are and are not reachable

| Suite | Status |
|---|---|
| HLK / WHQL D3D11 tests | **Not in tree, not reachable.** No HLK material, no test-signed cert flow, no HLK controller. Would require an HLK controller + client pair; out of scope for the current box. |
| `Verify-Helios.ps1 -RunSmokeTests` | **Reachable and the only automated gate that exists** — see §3. Four probes, exit-code gated. |
| 3DMark (Fire Strike, Steel Nomad) | **Reachable.** Installed on the VM; driven through cloned scheduled tasks (`tmp/perf/launch-gt1-arm.ps1`). It is the workload that moves `gs_so_declaration_dropped` and `tess_sig_fallback`. |
| dxvk-tests | **Not in tree.** Neither `dxvk-helios/tests/` nor `dxvk-research-only/tests/` exists. ROADMAP's WS3 plan item 2 names them; nothing to run today. |
| `dx-samples-research-only/` | **Present but it is the DirectX-Graphics-Samples repo — D3D12** (its `README.md:2`: "This repo contains the DirectX 12 Graphics samples"). ROADMAP's WS3 plan cites it as a D3D11 corpus; that is a mis-citation. It belongs to `DX12.md`. |
| Vulkan CTS / vkd3d-proton test suite against the venus ICD | **UNVERIFIED.** `icd/mesa/.gitlab-ci/vkd3d-runner.sh` and `deqp-runner.sh` are upstream Mesa CI runners carried in the submodule. Nothing in this tree records a run against Helios. Settling read: run `vkd3d-runner.sh`/deqp against the guest ICD and record pass/fail counts. |
| FaceWorks / DXUT samples | Reachable, historically used (ROADMAP WS3, 32nd/33rd sessions). Both take the legacy BLT present path. |

---

## 2. The refusal / no-op surface — the actual readout

The UMD has **no registry counter surface**. Every UMD counter is a process-global `AtomicUsize`
and the log line is the readout: `C:\ProgramData\Helios\umd-<pid>.log`. `tools/umd-gate-surface.ps1`
is the reader.

### 2.1 `DDI refusals:` — eleven counters

Declared in `umd/src/forward.rs:331-385`, initialized at `:387-399`, formatted by
`ddi_refusal_summary()` at `:416-437`, bumped by `note_ddi_refusal()` at `:444-448`.

> ⚠ `ROADMAP.md:3269-3280` says **nine** names. The code has **eleven**: R1010 added
> `alloc_meta_format_unknown` and `readback_stride_unsafe` (documented in the field comments at
> `forward.rs:364-384`). `tools/umd-gate-surface.ps1:136` and its regex at `:152-155` already carry
> all eleven. Trust the script and the source, not the ROADMAP paragraph.

| Counter | What it means | Incremented at |
|---|---|---|
| `srv_raw_hazard` | `pfnResourceReadAfterWriteHazard` for an SRV — empty body | `umd/src/forward/transfer.rs:286` |
| `resource_raw_hazard` | same DDI for a resource — empty body | `transfer.rs:293` |
| `text_filter_size_ignored` | `pfnSetTextFilterSize` — empty body | `umd/src/forward/pipeline.rs:215` |
| `staging_busy_assumed_free` | `pfnResourceIsStagingBusy` returns 0. **Not a no-op** — it is the semantic claim "never busy", which the runtime acts on | `transfer.rs:178` |
| `discard_partial` | `pfnDiscard` with `num_rects != 0`; the partial discard is dropped (forwarding it as a full-view discard wiped the undamaged 99% of DWM's flip backbuffer) | `transfer.rs:326` |
| `clear_view_unsupported` | `pfnClearView` on a non-RTV view type; the clear is dropped. Already logs its own refusal | `transfer.rs:400` |
| `gs_so_declaration_dropped` | `pfnCreateGeometryShaderWithStreamOutput` discards the SO declaration and creates a plain GS. `SOSetTargets` then binds buffers nothing writes and `DrawAuto` reads zero vertices | `umd/src/forward/shaders.rs:684` |
| `tess_sig_fallback` | Hull/domain shader create took the signature-less fallback. The UB against SINT inputs at that fallback is **not** fixed — it was made countable | `shaders.rs:739, 779, 821, 861` |
| `unhandled_resource_dimension` | `create_resource` with a dimension outside the four handled | `umd/src/forward/resource.rs:766` |
| `alloc_meta_format_unknown` | A DXGI format with no legacy D3DDDIFORMAT spelling stamped as `D3DDDIFMT_UNKNOWN` into the KMD alloc meta. `format::to_d3dddi` (`umd/src/format.rs:268-274`) knows exactly **two** formats: `R8G8B8A8_UNORM` and `B8G8R8A8_UNORM` | `umd/src/forward.rs:205` |
| `readback_stride_unsafe` | `maybe_log_present_readback` refused to sample a mapped surface whose stride would leave the mapped row. Env-gated (`HELIOS_PRESENT_READBACK`), capped at 8 | `umd/src/forward/present.rs:105` |

**Emission cadence** (`forward.rs:412-415`, `umd/src/device_funcs.rs:971-974`): the whole line is
written at `DestroyDevice` **and** on each counter's FIRST hit. Deliberately never on a per-present
path. So absence of the line means "no counter ever fired AND no device was destroyed" — that is
what `umd-gate-surface.ps1:160-163` prints.

**Known to move under real workloads.** `ROADMAP.md:3276-3279`:

> All nine should read 0 on a healthy DWM session; **`gs_so_declaration_dropped` and
> `tess_sig_fallback` are expected to MOVE under 3DMark** and each names a real WS3 conformance
> gap. The UMD still has no registry counter surface, so the log line is the readout — check the
> line exists, not just the `fetch_add`.

### 2.2 The noop-DDI mechanism

`umd/src/device_funcs.rs:717-747`. Every device-funcs table slot is bulk-filled with
`ddi_noop_device` (`device_funcs.rs:1155-1172`) before the real handlers are installed over it, so
an unfilled slot lands on a counted stub rather than a wild pointer.

- `ddi_noop_device` (`:717`) increments `DEVICE_NOOP_LOG_COUNT` (`:679`) unconditionally, but only
  **logs** when `n < 512 && crate::trace_enabled()`; the first hit logs a captured backtrace.
- `ddi_noop_dxgi` (`:737`) increments `DXGI_NOOP_LOG_COUNT` (`:680`), logs when
  `n < 256 && trace_enabled()`. Its doc comment records it as **PROVEN DEAD** (T2/R419,
  name-diffed against the generated structs): `install_dxgi` / `install_dxgi_1_1` /
  `install_dxgi_1_3` overwrite all 7 / 8 / 18 slots, so no slot points here.

**⚠ Finding — the WS3 metric is currently unreadable.** CLAUDE.md's stage charter says "drive the
noop-DDI hit counters to zero", and `device_funcs.rs:711-713` calls the counter "the WS3 metric".
But there is **no `noop_summary()`**, nothing loads `DEVICE_NOOP_LOG_COUNT`, and
`umd-gate-surface.ps1` has no pattern for it. The only readout is throttled `DDI noop(device)
hit=N` trace lines, which require `UmdTrace` on (cached at device init — a process restart is
needed to change it). By the project's own T5 rule ("an instrument nothing reads is not an
instrument", `forward.rs:406-407`) this counter does not currently qualify. Backlog item C1.

### 2.3 Adjacent counted surfaces on the same log line

- **Deferred contexts / command lists** — `deferred_summary()` (`umd/src/forward/deferred.rs:91-122`),
  15 fields: `dc_created dc_destroyed dc_recycled cl_finished cl_executed cl_abandoned
  cl_exec_empty cl_recycle_destroyed cl_recycle_handed_off cl_recycle_empty cl_recycle_dropped
  cl_replaced_stale dc_open_empty dc_unexpected_slot check_sizes_calls`. Gated on
  `HKLM\SOFTWARE\Helios!UmdDeferredDiagnostics`; with the knob off only three are reported
  (`deferred.rs:92-99`). Counter meanings at `deferred.rs:58-85`.
- **`ADAPTER_UNRECOGNISED`** (`umd/src/adapter.rs:130`) — adapter handles not carrying
  `ADAPTER_TOKEN`. Count-and-log only, never a refusal. Its one by-design caller was deleted in
  T6/R909, so a nonzero reading is now unexplained.
- **`DEVICE_TAG_MISMATCH`** (`umd/src/device_funcs.rs:339`) — an `HDEVICE` private block carrying
  neither `HELIOS_TAG_DEVICE` nor `HELIOS_TAG_DEFERRED`. Counted and refused, never cast.

### 2.4 How an engineer reads all of this after a run

```powershell
# whole-suite scope, after a probe or benchmark run
powershell -ExecutionPolicy Bypass -File Z:\tools\umd-gate-surface.ps1 -AllProcesses -SinceMinutes 30
# dwm only (the desktop's own UMD behaviour)
powershell -ExecutionPolicy Bypass -File Z:\tools\umd-gate-surface.ps1
```

Two traps the script already handles, both recorded in its header:

1. `umd-<pid>.log` is **appended, never truncated**, and Windows reuses pids across boots — one
   file routinely stacks several unrelated sessions from different builds. `Get-CurrentSession`
   (`:89-99`) anchors to the last `UMD module:` line. Never `Get-Content -First`.
2. Every pattern in `$MustNotAppear` (`:41-72`) is taken from the emitting source. Two of them
   were corrected on 2026-08-05 because the text differed and the check was a **silent pass**
   (`DDI CheckDeferredContextHandleSizes` had no "called"; `present_frame_gate: DxvkError` needed
   the colon), and `DEVICE REMOVED` was deleted outright because nothing ever logs it.

The KMD half is `tools/kmd-gate-surface.ps1` and `tools/kmd-counter-snapshot.ps1 -Label <name>`
(registry values persist across boots — take one snapshot before and one after and diff).

---

## 3. The existing probe suite, catalogued

~58 sources under `tools/` plus 4 under `packaging/windows/probes/`. They have never been
documented as a suite; this is that catalogue. One line each = what it proves.

### D3D11 device / feature level
- `tools/d3d11_devicecreate_probe.cpp` — `D3D11CreateDevice` on the Helios adapter by description; proves the runtime loads `helios_umd.dll` and reaches `CreateDevice`, not just OpenAdapter/GetCaps.
- `tools/d3d11_fl_probe.cpp` — one create per single-element feature-level array (11_1/11_0/10_1/10_0/9_1) plus the default list; localizes which level the runtime cannot satisfy. Runs as schtask `helios_flprobe` (ROADMAP:3036).
- `tools/d3d11_dbg_probe.cpp` — create with `D3D11_CREATE_DEVICE_DEBUG`; separates a debug-layer refusal from a missing SDK component.
- `tools/d3d11_fl_debug_probe.cpp` — FL11_0 create + full `IDXGIInfoQueue` dump via `dxgidebug.dll!DXGIGetDebugInterface1`.
- `tools/fl_reason_probe.cpp` — same question via the **global** DXGI InfoQueue exported from `dxgi.dll`, which works when CreateDevice fails and no `ID3D11InfoQueue` exists. This is how d3d11.dll's exact rejection string is obtained.
- `tools/dbwin_flprobe.cpp` — `DBWIN_BUFFER` `OutputDebugString` listener + FL11_0 DEBUG create; captures the rejection text with no debugger attached.
- `tools/d3d11_trace_probe.cpp` — waits on `C:\Windows\Temp\helios_go.txt`, then loops CreateDevice 30× at 3 s so a breakpoint can be set on an already-paged-in `d3d11!D3D11CreateDevice`.
- `tools/d3d11_dwm_probe.cpp` — replays dwmcore's composition-device recipe step by step (BGRA create, `ID3D11Device5`, shared cross-adapter fence, row-major cross-adapter shared texture, OPTIONS/OPTIONS2); pins which capability DWM needs.
- `tools/helios_ownership_soak.cpp` — N device + resource create/destroy cycles sampling handle count, module list and working set (self and dwm); the T5 ownership gate.
- `tools/helios_handle_types.cpp` — `NtQuerySystemInformation(SystemExtendedHandleInformation)` snapshots around device cycles; names the *type* of each leaked handle.
- `tools/helios_handle_origins.cpp` — IAT-hooks the handle-minting kernel32 entry points and captures a stack per stranded handle; names the *call site*.
- `packaging/windows/probes/d3d11-smoke.cpp` — packaged smoke: factory → find "Helios" adapter → `D3D11CreateDevice`, exit 1/2 on failure.

### D3D11 resources
- `tools/d3d11_tex3d_probe.cpp` — 32×16×8 Texture3D with initial data + TEXTURE3D SRV; proves the 3D create/initial-data/view path.
- `tools/d3d11_upload_integrity_probe.cpp` — position-dependent pattern through initial data / `UpdateSubresource` / dynamic Map at several sizes, verified back; catches large-upload zeroing/corruption.
- `tools/d3d11_tess_probe.cpp` — compiles hs_5_0 + ds_5_0 (tri domain, integer partitioning) and drives them on FL11_0; proves the tessellation shader create/bind path is reachable.
- `tools/dump_shader_tokens.c` — writes the DXBC container and the extracted SHDR/SHEX token stream the runtime hands the UMD, for offline dxbc-spirv repro.
- `tools/d3d11_triangle.cpp` — real windowed HWND, explicit adapter/output/swap-effect (`flip` vs `blt`), clear-to-blue + green triangle, optional pre-Present self-readback; separates "app rendered" from "DWM composited".

### D3D11 views
- `tools/d3d11_uav_probe.cpp` — four buffer-UAV shapes (typed R32_UINT, RAW, structured stride 16, DrawIndirect args); returns a failure count.
- `tools/d3d11_msaa_view_probe.cpp` — MSAA Texture2D RTV/DSV/SRV creation, clear, `ResolveSubresource`, staging read of one pixel. The regression test for the 32nd-session MSAA view-dimension bug.

### Shared resources / keyed mutex
- `tools/d3d11_open_shared_probe.cpp` — shared texture created on device 1, NT handle exported, opened on device 2; minimal `OpenResource` exercise.
- `tools/d3d11_keyed_mutex_probe.cpp` — keyed-mutex shared texture across two devices + `IDXGIKeyedMutex` Acquire/Release, dumping vtable slots with owning module.
- `tools/d3d11_kmt_shared_probe.cpp` — inspects/opens the D3D11-minted shared NT handle with raw D3DKMT; proves it is a well-formed WDDM shared object.
- `tools/d3d11_shared_content_probe.cpp` — dev1 clears a SHARED-NTHANDLE RT and flushes, dev2 opens/copies/reads; the discriminating test for whether *content* survives the alias.
- `tools/d3d11_shared_draw_probe.cpp` — same shape with a real compiled-shader draw plus self-readback on dev1; proves draws propagate across the alias.
- `tools/d3d11_xproc_draw_probe.cpp` — cross-**process** replica (`write`/`read` modes over a published global KMT handle); the dwm→IDD route.
- `tools/d3d11_shared_blob_truth_probe.cpp` — shared-content shape plus `HELIOS_ESCAPE_MAP_BLOB` of the venus blob, histogramming raw dwords per step; separates write-side from read-side divergence.
- `tools/d3d11_live_surface_probe.cpp` — opens supplied global KMT handles from a fresh process and histograms bytes 3× at 2 s; ground truth for where dwm's pixels land.
- `tools/d3d11_dwm_shared_repro.cpp` — DWM's exact failing creates (1896×1030 BGRA, misc 0x2 and 0x802) so the swallowed DxvkError reaches the DXVK log.
- `tools/d3d11_shared_wedge_repro.cpp` — WS1 defect 0w: contended 704×576 A8 SHARED\|SHARED_NTHANDLE creates to wedge inside `SyncSharedTexture → waitForResource`; exit 2 = wedged.

### Staging & readback
- `tools/d3d11_staging_readback_probe.cpp` — DEFAULT BGRA RT → STAGING → `Map(READ)`; the IddCx D3D11 fallback path.
- `tools/helios_clear_test.cpp` — pick a `VendorId==0x1af4` adapter, create RT, clear, `CopyResource` to staging, Map, read pixel 0. No Present; the resource/view/clear/copy/map forwarders end to end.

### DXGI enumeration / outputs / formats
- `tools/dxgi_enum.cpp` — every adapter's `GetDesc1` to a per-line-flushed file, each `D3D11CreateDevice` wrapped in SEH so a faulting adapter is distinguishable from a clean HRESULT.
- `tools/dxgi_luid_dump.cpp` — minimal `EnumAdapters1` {name, vendor, device, LUID, flags}; the authoritative adapter identity.
- `tools/dxgi_output_modes_probe.cpp` — per output, `GetDisplayModeList` across RGBA8/BGRA8/R10G10B10A2/RGBA16F (+ INTERLACED) and `GetDisplayModeList1`. The 31st-session `0x887a0022` probe.
- `tools/adapter_type_probe.cpp` — `D3DKMTEnumAdapters2` LUID / `NumOfSources` / decoded `D3DKMT_ADAPTERTYPE`, correlated with DXGI; the phantom-adapter diagnosis.
- `tools/ccd_adapter_probe.cpp` — CCD active paths → source/target LUID + GDI device name, cross-listed against every D3DKMT adapter.
- `tools/display_config_probe.cpp` — CCD path/mode counts for ONLY_ACTIVE/ALL/DATABASE_CURRENT; `extend` applies `SDC_TOPOLOGY_EXTEND`.
- `tools/display_enum_probe.cpp` — `EnumDisplayDevicesW` walk + first 80 `EnumDisplaySettingsW` modes per adapter.
- `tools/display_activate_probe.cpp` — `ChangeDisplaySettingsExW` with `CDS_SET_PRIMARY` on a named GDI display, before/after dump.
- `tools/display_set_path_probe.cpp` — `SetDisplayConfig` across a matrix of source ids × {virtual-aware, include-target-mode, save-to-DB}.
- `tools/dcomp_present_probe.cpp` — `CreateSwapChainForComposition` + dcomp target/visual on an HWND, animated by clear+Present(0); the Road-4 vehicle proof.

### D3DKMT
- `tools/d3dkmt_sync_probe.cpp` — which synchronization-object forms (monitored / legacy / CPU-notification) the adapter accepts, per-form NTSTATUS.
- `tools/d3dkmt_keyed_mutex_probe.cpp` — `D3DKMTCreateKeyedMutex2` → Acquire(0) → Release(1) → Destroy with no D3D11 involvement.
- `tools/d3dkmt_display_mode_list_probe.cpp` — `D3DKMTOpenAdapterFromGdiDisplayName` + two-pass `D3DKMTGetDisplayModeList` per device.
- `tools/d3dkmt_alloc_probe.c` — venus context over `D3DKMTEscape`, then `D3DKMTCreateAllocation` with a HOST3D mappable blob private struct.
- `tools/blob_capacity_probe.c` — allocates 4 KiB blobs until failure; measures free slots in the KMD's bounded blob table, state-neutral.
- `tools/blob_map_size_probe.c` — sweeps blob sizes to find where `MAP_BLOB` starts returning `0xC000009A` (the single-MDL `CSHORT Size` ceiling).
- `tools/escape_owner_probe.c` — the T1b escape trust boundary: bad magic, unknown verb, and (under `--attack`) a forged `hDevice=NULL` RELEASE_BLOB against the live DWM primary and a cross-device CTX_DESTROY, both of which must be refused.
- `tools/vidmm_tracking_probe.c` — TRACKING allocations made resident, per-process segment usage before/after (`nonlocal` switches budget).
- `tools/vehicle_flipwait_probe.c` — queue `WAIT(F>=1)` then `SIGNAL(G=5)`; proves VidSch honours queued GPU-side monitored-fence waits on this software-scheduled adapter.
- `tools/read_ledger_dump.c` — `HELIOS_ESCAPE_MAP_READ_LEDGER` consumer emitting stable CSV with re-claim detection.
- `tools/scanout_timeline_dump.c` — `HELIOS_ESCAPE_QUERY_SCANOUT_TIMELINE` META/READ to CSV; never submits work.
- `tools/vram_report_probe.cpp` — DXGI/VidMm numbers vs Venus Vulkan heaps, with `--d3d11-allocs`/`--vulkan-allocs` to see what the tracker charges.

### Vulkan / OpenGL / OpenCL
- `tools/vk_ring_fence_probe.cpp` — exportable OPAQUE_WIN32 timeline semaphore re-imported; an early short wait must `VK_TIMEOUT` and the full wait must elapse ≈ T_gpu. Proves signals retire at host GPU completion, not decode. schtask `helios_ringprobe`.
- `tools/vk_surface_recreate_probe.cpp` — two surfaces/swapchains on one HWND, A destroyed while B presents; the per-HWND dcomp target cache and the vkd3d resize/fullscreen shape.
- `tools/vk_fence_wake_probe.c` / `tools/vk_fence_wake_probe_win.c` — empty `vkQueueSubmit` + fence wake timing, host side and guest side.
- `tools/vk_export_alloc_probe.cpp` — raw-Vulkan replica of dxvk-helios' MISC_SHARED export path across every compatible memory type; finds which leg the host rejects.
- `packaging/windows/probes/vulkan-smoke.c` — instance → physical devices → properties.
- `packaging/windows/probes/opengl-smoke.c` — `CS_OWNDC` window, pixel format, `wglCreateContext`/`MakeCurrent`, print `GL_VENDOR`/`GL_RENDERER`/`GL_VERSION`.
- `packaging/windows/probes/opencl-smoke.c` — first platform with a `CL_DEVICE_TYPE_GPU` device; print platform + device name.
- `tools/live_dump.cpp` — not a graphics probe: `MiniDumpWriteDump` for a pid; the capture tool for wedged probes.

### The one automated gate: `Verify-Helios.ps1 -RunSmokeTests`

`packaging/windows/Verify-Helios.ps1` first verifies the install: every entry in
`%ProgramData%\Helios\install-state.json` `runtimeFiles` must exist and SHA256-match (`:17-26`),
the PnP device status must be `OK` and its provider must start with `Helios` (`:28-48`), the
Vulkan ICD manifest must be enabled under `HKLM\SOFTWARE\Khronos\Vulkan\Drivers` (`:50-54`),
`OpenGLDriverName` on the class key must point at the bundled `libgallium_wgl.dll` (`:56-60`), and
the CLVK vendor DLL must be enabled under `HKLM\SOFTWARE\Khronos\OpenCL\Vendors` (`:62-66`).

With `-RunSmokeTests` it then runs the four probes from `<installRoot>\runtime\smoke` in order —
Vulkan, Direct3D 11, OpenGL, OpenCL — and adds a failure for any non-zero exit (`:68-85`). Any
failure throws at `:88-91`. A missing probe exe is a **warning, not a failure** (`:78-80`), so a
bundle assembled without smoke tests still reports "healthy" — worth knowing before treating a
green run as coverage.

The probes are compiled by `ci/windows/Build-SmokeTests.ps1` under `vcvars64` with `cl.exe`
(`:18-25`), and copied into the payload by `ci/windows/Assemble-Package.ps1:76-77`.

---

## 4. Known open conformance gaps

**(a) FL11 MSAA is a table assertion, not a capability probe.** `ROADMAP.md:3027-3034`:

> (3) **"MSAA quality reported to be 0"** — FL11 requires every render-target format to support 4x
> MSAA and does NOT exempt 96-bit R32G32B32; CheckMultisampleQualityLevels now floors RT formats to
> >=1 at 1/2/4/8 + check_format_support advertises MULTISAMPLE_RENDERTARGET for RTs — PARTIAL: the
> runtime advances past formats 5-8 but still hits the MSAA error on a later format/count.
> **NEXT (owner directive): conform to the D3D11.3 functional spec** … exact per-format/per-sample-count
> FL11 MSAA requirements.

The code state, `umd/src/forward/queries.rs:129-164`: `helios_multisample_quality_levels` answers 1
for `sample_count ∈ {1,2,4,8,16}` whenever `dxgi_msaa_bits_per_sample(fmt, caps)` is `Some`. The
doc comment at `:122-128` is explicit about what that is worth:

> `dxgi_msaa_bits_per_sample` resolves to a static format table plus the DXVK caps word. It never
> asks whether that SAMPLE COUNT is supported, so today's "8x on a 128-bit format" is a table
> assertion, not a capability probe.

R829 was an owner decision to correct the doc rather than the code. The spec is in-tree at
`D3D11_3_FunctionalSpec.htm` (repo root). Whether the residual runtime rejection ROADMAP describes
still reproduces on the current UMD is **UNVERIFIED** — settling read: run `tools/d3d11_fl_probe.cpp`
via schtask `helios_flprobe` on the current build and capture the `Microsoft-Windows-DXGI` ETW
rejection string (recipe at `ROADMAP.md:3018-3022`).

**(b) DXGI format coverage.** Three separate, verifiable narrownesses:

1. `umd/src/format.rs:268-274` — `to_d3dddi` knows **two** formats. Everything else becomes
   `D3DDDIFMT_UNKNOWN` in the KMD allocation meta and bumps `alloc_meta_format_unknown`. The exact
   format travels separately in `dxgi_format`, so this is a documented downgrade, not a bug — but
   any consumer that reads the legacy field sees `UNKNOWN`.
2. `umd/src/format.rs:31-37, 91` — `bytes_per_pixel` defaults to **4** for every format past 115
   and for every block-compressed format. The comment justifies over-reporting ("only pads
   `linear_size`"), which is safe for sizing but means BC and video formats have no real pitch
   arithmetic. UNVERIFIED whether any live path depends on a correct BC pitch; settling read: a BC7
   staging Map round-trip probe (backlog item C5).
3. `umd/src/forward/format_caps.rs:169-262` — a hand-maintained set of WARP-matching overrides for
   the depth/typeless families, a `SO_BUFFER` scrub over 13 formats, and one special case:
   `R10G10B10_XR_BIAS_A2_UNORM` (89) must answer the explicit `NOT_SUPPORTED` sentinel `0x80000000`
   rather than 0, or `D3D11CreateDevice` fails `0x887a0020` and dwm crash-loops (`:244-262`).
   There is no test that any of these overrides is still correct.

The ROADMAP's own item (`:3170-3171`): "DXGI format coverage audit (the format round-trip carrier
landed at `bfb5121`; verify beyond BGRA8)."

**(c) Remaining 11.1 DDI plumbing.** `ROADMAP.md:3173-3174`:

> Map remaining 11.1 features (deferred contexts? threading modes? UAVs at FL11_0) against DXVK
> capabilities — most exist in the engine; the work is the DDI plumbing.

Current state: deferred contexts and command lists **are** implemented (`umd/src/forward/deferred.rs`),
and `threading_caps()` (`umd/src/device_funcs.rs:314-323`) advertises `FREETHREADED` plus
`COMMANDLISTS_BUILD_2` when the `UmdCommandLists` knob is on. Per the memory index, that knob is
currently **OFF** on measured perf grounds, not correctness grounds. Tiled resources are wired but
only in the WDDM1_3 table (§1). The FL9 pipeline bits (`9_1`/`9_2`/`9_3`, bits 4/5/6) are not
advertised — **UNVERIFIED** whether the runtime requires them for a FL11 driver; settling read: the
D3D11.3 functional spec's caps section, or a `D3D11CreateDevice` at `D3D_FEATURE_LEVEL_9_3`
through `tools/d3d11_fl_probe.cpp`.

**(d) The two counters ROADMAP says will move.** `gs_so_declaration_dropped` and
`tess_sig_fallback` (quoted in §2.1). Each names a real gap: stream-output capture is silently
discarded, and the signature-less hull/domain fallback carries known UB against SINT inputs
(`forward.rs:357-361`). Whether they have actually moved on the **current** build is
**UNVERIFIED** — registry/atomic values from previous boots prove nothing; run 3DMark through
`tmp/perf/launch-gt1-arm.ps1` and read `umd-gate-surface.ps1 -AllProcesses`.

---

## 5. How to add a conformance test

**Write it as a single self-contained C/C++ file in `tools/`.** There is no probe harness, no
shared header, no CMake. Every probe carries its own compile command in its header comment.

**Compile it on the VM**, to a **local C: path** — never onto `Z:\`:

```
:: MSVC under vcvars64.bat — the generic recipe (docs/archive/WDDM_SYNC_M3_M4_HANDOFF.md:520).
:: D3DKMT probes add gdi32.lib + the wdk-include path; shader probes add d3dcompiler.lib.
cl /nologo /EHsc /W4 Z:\tools\<probe>.cpp /I"Z:\icd\win-build\wdk-include" ^
   /Fe:C:\Windows\Temp\x\p.exe /link d3d11.lib dxgi.lib
:: WinLibs g++ (mingw-w64), used where MSVC is inconvenient. There is NO clang-cl on this box
:: (ROADMAP.md:3422), so any clang-cl line in an old probe header is stale.
g++ -O2 -o C:\Users\Rupansh\<probe>.exe Z:\tools\<probe>.cpp -ld3d11 -ldxgi -ldxguid
```

The MSVC-driving PowerShell wrappers (`tools/helios-handle-types.ps1:24-37`,
`helios-handle-origins.ps1:19-34`, `helios-ownership-soak.ps1:29-44`) all copy the source from
`Z:\tools\` into `C:\Users\Rupansh\helios-probe` and invoke `cl` there — copy that pattern if the
probe needs a wrapper. If the probe belongs in the shipping bundle, add it to
`packaging/windows/probes/` plus a `cl.exe` line in `ci/windows/Build-SmokeTests.ps1`, a copy line
in `ci/windows/Assemble-Package.ps1:76-77`, and an entry in the `$tests` array of
`packaging/windows/Verify-Helios.ps1:70-75`. Exit non-zero on failure — that is the whole contract.

**Run it in session 1 if it touches a window, a desktop, or a swapchain.** `win_exec`/SSH land in
**session 0**, which has no desktop; a session-0 run can fake a driver regression (memory:
60th session). The mechanism is to clone an existing interactive task's XML and rewrite its
arguments — `tmp/perf/launch-gt1-arm.ps1:16-24` is the canonical five lines:

```powershell
[xml]$xml = (schtasks /query /tn helios_perf_fs /xml ONE | Out-String)
$xml.Task.Actions.Exec.Arguments = "-NoProfile -ExecutionPolicy Bypass -File Z:\path\to\runner.ps1 ..."
$xml.Save($taskXml)
schtasks /create /tn $taskName /xml $taskXml /f
schtasks /run   /tn $taskName
```

Existing named tasks to clone from or reuse: `helios_perf_fs` (interactive benchmark principal),
`helios_flprobe`, `helios_ringprobe` / `helios_ringprobe_named`, `helios_paintcap`,
`helios_dcomp_probe`, `helios_repaint`, `helios_flasher`, `helios_dstate`, `helios_enum_windows`,
`helios_regedit` (`ROADMAP.md:3352-3355`, `:3036`, `:3416-3419`).

⚠ Two Vulkan-probe traps that each cost a diagnosis: the Vulkan loader **silently ignores**
`VK_DRIVER_FILES`/`VK_ICD_FILENAMES` in elevated processes, and win_exec/SSH shells are High-IL —
so ICD A/B probes must run through a `/rl LIMITED` scheduled task (`ROADMAP.md:3422-3426`).

**Then read the instruments, not the exit code alone.** `tools/umd-gate-surface.ps1 -AllProcesses`
for the UMD half, `tools/kmd-gate-surface.ps1` + a `tools/kmd-counter-snapshot.ps1` diff for the
KMD half. Per CLAUDE.md's evidence rule, only user-visible desktop state counts as *rendering*
evidence (`helios_paintcap` → `Z:\tmp\screen_copy.png`); a probe's log line is not a frame.

---

## 6. Prioritized backlog

Each item: the evidence that motivates it, and what "done" means.

**C1 — Make the noop-DDI hit count readable.** *Evidence:* CLAUDE.md names it as the WS3 metric
and `device_funcs.rs:711-713` calls it that, but nothing loads `DEVICE_NOOP_LOG_COUNT` and the only
output is throttled and `trace_enabled()`-gated (§2.2) — the exact T5 defect `ddi_refusal_summary`
exists to prevent. *Done:* a `noop_summary()` emitted beside it at `DestroyDevice` and on first hit;
a pattern in `umd-gate-surface.ps1`; a recorded reading from one dwm session and one 3DMark run.

**C2 — Run 3DMark and record the eleven refusal counters on the current build.** *Evidence:*
`ROADMAP.md:3276-3279` predicts `gs_so_declaration_dropped` and `tess_sig_fallback` move; nothing in
the tree records an actual reading, and counters from prior boots prove nothing. *Done:* one
`-AllProcesses` gate-surface capture for GT1, GT2 and Combined, recorded in ROADMAP as the WS3
baseline, each non-zero counter named and attributed.

**C3 — Implement stream-output GS creation (`gs_so_declaration_dropped`).** *Evidence:*
`forward.rs:352-355` — the SO declaration is discarded, `SOSetTargets` then binds buffers nothing
writes and `DrawAuto` reads zero vertices. DXVK implements SO; this is DDI plumbing. *Done:*
`pfnCreateGeometryShaderWithStreamOutput` forwards the declaration; a `tools/d3d11_so_probe.cpp`
captures vertices through an SO buffer and reads them back; the counter stays 0 under 3DMark.

**C4 — Close the `tess_sig_fallback` SINT UB.** *Evidence:* `forward.rs:357-361` — the fallback is
taken at four sites (`shaders.rs:739, 779, 821, 861`) and its UB against SINT inputs is explicitly
*not* fixed, only counted; VUID-Input-08733 has bitten this driver before (`adapter.rs:305-309`).
*Done:* signature-carrying hull/domain creates on the paths 3DMark exercises;
`tools/d3d11_tess_probe.cpp` extended with a SINT patch-constant case; counter 0 on that workload.

**C5 — A DXGI format round-trip matrix probe.** *Evidence:* `ROADMAP.md:3170-3171` ("verify beyond
BGRA8"); `format.rs:268-274` covers two formats; `format_caps.rs:169-262` is a hand-maintained
override table with no test. *Done:* `tools/d3d11_format_matrix_probe.cpp` over formats 1..115 ×
{create, CheckFormatSupport, RTV/SRV, staging Map round-trip where legal} emitting CSV; the first
run checked in as the baseline; `alloc_meta_format_unknown` explained per format, not in bulk.

**C6 — Settle the FL11 MSAA question against the in-tree spec.** *Evidence:* `ROADMAP.md:3027-3034`
(PARTIAL; owner directive to conform to the D3D11.3 functional spec) vs `queries.rs:122-128`
(R829's decision to correct the doc instead). Both cannot be the final state. *Done:* the
per-format/per-sample-count table read out of `D3D11_3_FunctionalSpec.htm` and either implemented,
or recorded as intentionally-above-floor with DXGI-ETW evidence that no runtime rejection remains.

**C7 — Decide and document the tiled-resource claim.** *Evidence:* `caps.rs:81` advertises
`TILED_RESOURCES_TIER_2_SUPPORTED` for **every** FL11 device, but the seven tiled DDIs are installed
only in `install_wddm1_3` (`tables.rs:290-306`) — an 11.1-negotiated device gets the claim without
the entry points. *Done:* either gate the cap on the negotiated interface or install the DDIs in the
11.1 table, plus a probe that creates a tile pool and calls `UpdateTileMappings` on each interface.

**C8 — Give the smoke gate teeth.** *Evidence:* `Verify-Helios.ps1:78-80` treats a missing probe as
a warning, so a bundle assembled without smoke tests still verifies "healthy". *Done:* missing
probes fail when `-RunSmokeTests` was explicitly requested, plus a D3D11 smoke probe that does more
than create a device (clear + staging readback — `helios_clear_test.cpp` is the shape).

**C9 — Establish a Vulkan-layer conformance baseline for the venus ICD.** *Evidence:* the D3D11
surface sits on the ICD and no in-tree record exists of any CTS or vkd3d-proton run against it (§1,
UNVERIFIED row); without it, attributing a D3D11 failure to the UMD risks the blame-the-wrong-layer
pattern this project has hit repeatedly. *Done:* one deqp-vk or vkd3d-proton suite run against the
guest ICD with the pass/fail count and known-fail list recorded, so future triage can subtract it.

**C10 — Record expected results for the probe suite.** *Evidence:* §3 is the first catalogue; none
of the ~58 probes has a recorded expected outcome, so "it printed some HRESULTs" is the whole signal
for most of them. *Done:* a runner that executes the D3D11/DXGI subset through one scheduled task
and diffs against a checked-in expected-output file, so a regression is a diff, not a reading
exercise.
