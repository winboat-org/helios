# Helios — next-session handoff (2026-06-18, Gate 5a venus-over-D3DKMT)

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
- VM-launch ownership: changing `tools/launch-helios-gtk.sh` / libvirt XML / the
  launcher env needs a user relaunch. A devcon install + guest reboot is in-guest
  (QEMU stays up) but still ASK the user to reboot when the stack wedges.

### State to be aware of
- Uncommitted. Worth committing the validated work (Stage 1 + real-headers +
  Stage 2a + the 2b plumbing) on a branch before the next live push.
- `.65` KMD installed, Code 0. `GpuVirtualizationFlags=0` is staged in the
  registry (a DWM-pairing experiment that did NOT fix DWM — harmless, leave it).
