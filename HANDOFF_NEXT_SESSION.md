# Helios — next-session handoff (2026-06-23, IDD + Helios composition)

> **⛔ SUPERSEDED (2026-06-26) — read `FABLE5_HANDOFF.md`.** Historical (pre-M1). Its display-topology
> framing is now known to rest partly on session-0-artifact readings; the current blocker is the IDD
> failing its PnP post-start (Code 43) on the render-only-Helios pairing.

Paste the section under "PROMPT" into a fresh session. The older 2026-06-18 Gate 5a prompt is
kept below as historical context only; the 2026-06-23 prompt is current.

---

## PROMPT

Continue the Helios WDDM + Looking Glass IDD bring-up in `/home/rupansh/helios-vgpu`.

Goal: make Windows compose and render the whole desktop on the Helios WDDM render adapter
(`virtio-gpu-gl`/venus on the host), then have the Looking Glass IDD receive the IddCx
swapchain frames and display them in the Looking Glass client. Do not pivot to per-app venus
or WARP composition as the final design.

Read first:

- `WDDM_COMPOSITION_HANDOFF.md` — especially the 2026-06-23 update.
- `IDD_HELIOS_RENDER_PLAN.md` — especially the 2026-06-23 correction.
- `BRINGUP_QUIRKS.md` §6c.
- `NTOSEYE.md` only if you need live kernel/user debugging.

Current verified state:

- Helios KMD loads Code 0 with gpu-gl attached.
- UMD/D3D11 device creation is fixed: `D3D11CreateDevice` on "Helios vGPU Render Adapter
  (WDDM bring-up)" returns `S_OK`, feature level `0xa000`.
- Looking Glass IDD can select Helios with
  `HKLM:\SOFTWARE\LookingGlass\IDD\HeliosRenderAdapter = 1`.
- IddCx monitor arrival succeeds and returns an OS adapter LUID + target id for the LG monitor.
- No source diff remains under `LookingGlass/idd`; the IDD code has been restored to the
  repository/submodule baseline.
- Clean gpu-gl-out baseline verified after reboot: with the Helios PCI node a phantom
  non-started device (`Get-PnpDevice Status=Unknown`, `Problem=CM_PROB_PHANTOM`; earlier probe
  surfaced this as disconnected), the Looking Glass IDD is `OK`, WMI reports the LG monitor
  active, `Win32_VideoController` reports `1920x1080`, a session-1 CCD probe reports
  `active paths=1 modes=2`, `all paths=2 modes=4`, `database paths=1 modes=2`, and the
  Looking Glass client displays the desktop. `HeliosRenderAdapter=1` remains set in the
  registry, but the IDD logs `Preferred IDD render adapter not found; IddCx render adapter
  remains OS-selected`; `AssignSwapChain` fires with render-adapter LUID
  `00000000:000076b0`, D3D11 initializes at feature level `0xb100`, and D3D12 creates the
  IVSHMEM heap plus copy/compute queues.
- Current deployed KMD test hash:
  `B0B8A079394E86609C0FCD72785981A01FB70F366F459F3C4E394B68DCC3A315`.

Observed current blocker:

- With Helios/gpu-gl present, CCD/display activation is failing. In session 1, the helper logs:
  `GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS/QDC_ALL_PATHS/QDC_DATABASE_CURRENT)` all
  return success with `paths=0 modes=0`.
- The LG target name resolves and `WmiMonitorID` sees the LG monitor as active, but the helper's
  synthetic `SetDisplayConfig(SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_APPLY | ...)` attempts all
  return `31` (`ERROR_GEN_FAILURE`).
- `EnumDisplayDevices` shows `Microsoft Basic Display Driver` and `INDIRECTKMD` with state flags
  `0x00000000`; `EnumDisplaySettings` for the LG display returns `ERROR_BUSY` (`170`).
- A controlled same-boot test with Helios disabled still produced empty CCD paths and ret 31.
  The clean gpu-gl-out boot above contradicts that same-boot result, so treat the same-boot
  Helios-disabled data as affected by live graphics-stack state.

Known negative tests:

- Do not change IDD monitor-mode `vSyncFreqDivider` to `1`; that made
  `IddCxMonitorArrival` fail with `STATUS_INVALID_PARAMETER`. The IDD source is restored.
- Do not disable `DECLARE_CROSS_ADAPTER_RESOURCE` as a "fix"; it made D3D11/Venus context creation
  regress (`D3DKMTEscape` `0xc0000185`). Cross-adapter support is restored.
- `IddCxAdapterDisplayConfigUpdate` returns `0xc00000bb`; treat it as expected for this local
  IDD path unless WPP proves otherwise.

Code changes to preserve:

- `kmd_render/src/ddi/query_adapter_info.rs`: `DriverSupportsCddDwmInterop` is no longer
  advertised. Helios is zero-source render-only; that bit claims a display-miniport CDD/DWM
  interop path.
- `kmd_render/src/virtio/gpu.rs` and `kmd_render/src/ddi/create_allocation.rs`: standard WDDM
  allocations that adopt a KMD-created Venus resource now remove the temporary owner-0 tracking
  slot without sending host commands, avoiding later double-unref on StopDevice. This addresses
  qemu logs like `virgl_cmd_resource_unref: resource does not exist` / `ctrl 0x102 error 0x1203`.

Useful next investigation tools:

1. IddCx WPP capture, if the display-topology path is under investigation:
   `logman create trace IddCx -o C:\Windows\Temp\IddCx-helios.etl -ets -ow -mode sequential -p {D92BCB52-FA78-406F-A9A5-2037509FADEA} 0x4f4 0xFF`,
   cycle `ROOT\DISPLAY\0000`, trigger helper activation, then `logman stop IddCx -ets`.
   `tracerpt` only gives `Unknown(...)`; use `tracefmt`/`tracepdb` with public `IddCx.pdb`, or
   kernel `!wmitrace.logdump IddCx`.
2. Session-1 display probes, if using Win32 CCD APIs: log full flags and returned source/target
   names, and verify with WMI/session-1 probes rather than SSH/session 0.
3. If/when CCD has an active path and IddCx calls `AssignSwapChain`, Helios composition checks:
   confirm DWM allocates standard composition surfaces on Helios, check whether render work uses
   UMD escape/venus or WDDM Render/SubmitCommand, and then fix real fence/coherency as needed.
   D3D12/vkd3d-proton is future app support; it is not the immediate blocker before swapchain
   assignment.

Build/deploy reminders:

- KMD build: `win_cargo crate_dir:"kmd_render" args:["build"]`, then manually copy/sign
  `target\debug\deps\helios_kmd_render.dll` as `helios_kmd_render.sys`, regenerate/sign cat,
  in-place copy `.sys` + `.cat` into the live e0bd DriverStore dir, and PnP disable/enable the
  Helios PCI device. This was reboot-free in the 2026-06-23 session.
- IDD build/deploy, if you change it: use `win_looking_glass_idd`, then install/rebind with
  devcon/pnputil. Do not in-place copy into the IDD DriverStore directory.
- Display checks must use WMI or active session-1 probes. SSH/session 0 is misleading for monitor
  and CCD state.

---

# Historical handoff (2026-06-18, Gate 5a venus-over-D3DKMT)

Paste the section under "PROMPT" into a fresh session.

---

## PROMPT

Continue the Helios WDDM render stack in `/home/rupansh/helios-vgpu` — specifically
**Gate 5a (venus-over-D3DKMT), increment 2b**. Read `CLAUDE.md`, then the
`gate5a-venus-d3dkmt` memory (authoritative, has every gotcha), then
`GATE5_VENUS_WDDM_DESIGN.md` + `GATE5_STAGE2_ALLOC_DESIGN.md`. The goal: a WDDM
render-only adapter (`kmd_render`) → Vulkan via the Mesa venus ICD (`icd/mesa`)
over D3DKMT → later D3D11 via DXVK + D3D12 via VKD3D-Proton.

### What already works (all validated LIVE this session)
- **Stage 1:** the Mesa venus ICD (`icd/mesa/src/virtio/vulkan/vn_renderer_helios.c`)
  reaches the `kmd_render` WDDM adapter over D3DKMT: `D3DKMTEnumAdapters2` +
  match "Helios" → `CreateDevice` → `CreateContext` → `D3DKMTEscape(CTX_CREATE)`
  → venus context up (`ctx_id`). Proven via `vulkaninfoSDK.exe` stderr breadcrumbs.
- **Real WDK headers:** mingw lacks the D3DKMT headers; the 3 WDK-only headers
  (`d3dkmthk.h`/`d3dkmdt.h`/`d3dukmdt.h`) are vendored in `icd/win-build/wdk-include`
  and `-I`'d in `icd/mesa/src/virtio/vulkan/meson.build`. The ICD `#include`s the
  real `<d3dkmthk.h>` (authoritative ABI). The old hand-shim is deleted.
- **Stage 2a (allocations) COMPLETE:** `D3DKMTCreateAllocation` for a venus HOST3D
  blob succeeds end-to-end — `DxgkDdiCreateAllocation` (reads `HeliosWddmAllocPrivate`
  → `resource_create_blob`, **`blob_id=0` works**, the `.56` blocker is gone) →
  `DxgkDdiOpenAllocation` (binds it to the device) → `DxgkDdiDestroyAllocation`
  (unmap+detach+unref). `D3DKMTCreateAllocation`/`Lock2`/`DestroyAllocation` =
  `0x0` in `tools/d3dkmt_alloc_probe.c`.
- **Stage 2b first light:** `D3DKMTLock2` returns a writable CPU VA (sentinel
  write/read works) with NO segment reshape / NO real BuildPagingBuffer — the
  WDDM CPU-mapping machinery just works. BUT VidMm backed the CpuVisible
  allocation with **system memory**, not the host-visible BAR, so the host
  (venus) can't see it yet.

### THE NEXT TASK — increment 2b convergence
Make `D3DKMTLock2` map the **host-visible BAR** (so the host sees the venus ring),
not system memory:
1. Reshape `kmd_render/src/ddi/query_adapter_info.rs::query_segments` → a
   **CpuVisible MEMORY segment** (`Flags`: `set_CpuVisible(1)`, `set_Aperture(0)`)
   with `CpuTranslatedAddress = host_visible.base`, `BaseAddress = host_visible.base`,
   `Size = host_visible.len`. Thread the `AdapterContext` in to read
   `adapter.with_virtio(|v| v.host_visible())` (the scan + `HostVisibleWindow` are
   already ported into `kmd_render/src/virtio/gpu.rs`; StartDevice breadcrumb
   `0x0B000005` confirmed the window is found live). **LOAD-BEARING: re-verify
   Code 0 + that `D3DKMTCreateAllocation` still succeeds after the reshape.**
2. Observe (ntoseye breakpoint on `dxgkddi_build_paging_buffer`, address =
   helios_kmd_render.sys base + RVA-from-`.map`) whether dxgkrnl calls
   BuildPagingBuffer to assign the allocation a segment offset. If yes →
   `resource_map_blob(resource_id, offset)` there (the fn is ported; fill
   `AllocationContext.map_offset/len/mapped` — those 4 fields are currently
   dead-code warnings, expected). If Lock2 maps the BAR directly without paging,
   even simpler.
3. Confirm with the harness: `D3DKMTLock2` VA writes land in the host-visible blob
   (host-side readback, or a venus roundtrip later). Then wire the ICD
   `shmem_ops.create`/`bo_ops.create_from_device_memory`/`map` → `D3DKMTCreateAllocation`
   + `D3DKMTLock2` (replace the fail-clean `helios_ioctl_*` stubs; submit/wait stay
   stubbed until Stage 3). Mirror `HeliosWddmAllocPrivate` (48B, `protocol/src/wddm.rs`)
   in C in `vn_renderer_helios.c`.

Test target for 2b: `tools/d3dkmt_alloc_probe.c` shows Lock2 mapping the blob, then
`vulkaninfoSDK.exe --summary` gets its venus command-ring shmem (it still can't
finish until Stage 3 submit — that's expected/honest). NO vkcube (needs DWM/present).

### After 2b — Stage 3 (NOT started)
Submit over `D3DKMTRender` (`HeliosWddmCmdBuf` in `protocol/src/wddm.rs`) →
`DxgkDdiRender`/`Patch`/`SubmitCommand` → `submit_venus` + a monitored fence;
ICD `ops.submit`/`ops.wait`. Then `vulkaninfo` full + the offscreen
`C:\Users\Rupansh\helios_vk_*.exe` tests over the WDDM ICD = Gate 5a done.

### KEY GOTCHAS (also in the gate5a memory)
- `D3DKMTEscape` REQUIRES `hDevice` set (adapter-only fails). 
- The `diag::record` ring (S0..S159) is FLOODED by steady-state QueryAdapterInfo
  perf-polls within <1s — UNRELIABLE for one-shot DDI breadcrumbs. Use **ntoseye
  breakpoints** (func address = sys base from `kernel_modules` + RVA from the
  linker `.map` at `target\debug\deps\helios_kmd_render.map`; preferred image base
  in the map is `0x180000000`, so RVA = mapVA − 0x180000000).
- **devcon churn degrades the graphics stack** (repeated reloads + the DWM
  crash-loop → `CreateDXGIFactory1` `ERROR_GEN_FAILURE`, adapter un-enumerable
  though Code 0) → needs a reboot. Keep installs minimal between reboots; if the
  harness says "no Helios adapter" but Code 0, the stack is wedged → ask the user
  to reboot.
- DWM is STILL CRASH-LOOPING (Code-0 Helios; root cause = VRD pairing, see the
  `dwm-crash-vrd-pairing-rootcause` memory). The ONLY real fix is D3D11-via-DXVK
  (Gate 5b) — do NOT chase it now; the Vulkan stack is validated via vulkaninfo +
  offscreen tests, which don't need DWM/present.

### Build / install / test / recovery
- **ICD:** `win_meson ["compile","-C","C:\\Users\\Rupansh\\helios-mesa-build"]`
  (mingw, reads `Z:\icd\mesa`). Install: copy
  `...\helios-mesa-build\src\virtio\vulkan\vulkan_virtio.dll` →
  `C:\ProgramData\HeliosVulkan\vulkan_virtio-9e1534dc4ffc.dll` (the registered
  Khronos manifest's path). Bring-up breadcrumbs are unconditional
  `fprintf(stderr,"HELIOS[gate5a]:...")` — gate/remove once Gate 5a is done.
- **KMD:** compile-check `win_cargo kmd_render ["build"]`; installable package
  `win_cargo kmd_render ["make","--makefile","Cargo.make.toml"]` — **bump the
  version first** in `kmd_render/build.rs` (`22,22,32,NN` ×2 + `22.22.32.NN` ×2)
  AND `kmd_render/Cargo.make.toml` `-v` (currently **.65**). Install:
  `devcon update <pkg>\helios_kmd_render.inf "PCI\VEN_1AF4&DEV_1050&SUBSYS_11001AF4&REV_01"`
  (devcon at `C:\Program Files (x86)\Windows Kits\10\Tools\10.0.26100.0\x64\devcon.exe`).
  After install, wait ~9s for the adapter to settle before running the harness.
- **Harness:** `tools/d3dkmt_alloc_probe.c` — build with `cl /EHsc d3dkmt_alloc_probe.c
  /I"Z:\icd\win-build\wdk-include" /link gdi32.lib` (vcvars64); at
  `C:\Users\Rupansh\helios-probe\alloc_probe.exe`. Other probes there:
  `probe.exe` (D3D11CreateDevice), `dxgi_enum.exe`.
- **ntoseye** is the kernel debugger (coherent after reboot). `debug_log` is empty
  here — use breakpoints. Our driver's symbols don't auto-resolve; use base+RVA.
- VM-launch ownership: changing `tools/launch-helios-gtk.sh` / the
  launcher env needs a user relaunch. A devcon install + guest reboot is in-guest
  (QEMU stays up) but still ASK the user to reboot when the stack wedges.

### State to be aware of
- Uncommitted. Worth committing the validated work (Stage 1 + real-headers +
  Stage 2a + the 2b plumbing) on a branch before the next live push.
- `.65` KMD installed, Code 0. `GpuVirtualizationFlags=0` is staged in the
  registry (a DWM-pairing experiment that did NOT fix DWM — harmless, leave it).
