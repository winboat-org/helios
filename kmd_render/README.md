# Helios WDDM Render KMD Bring-Up

This crate starts the WDDM render-only implementation. It is separate from
`../kmd`, which remains the working System-class KMDF Venus driver.

Current state:

- WDM driver crate.
- Bindgen over `dispmprt.h` and `d3dkmddi.h`.
- Links `displib`.
- Builds a `DRIVER_INITIALIZATION_DATA` table for `DxgkInitialize`.
- Uses WDK 10.0.26100 / WDDM 3.2 headers.
- Packages as `helios_kmd_render.sys` / service `helios_kmd_render`, separate from the working `helios_kmd`.
- INF currently uses the Display setup class with `msdv.inf`, matching the viogpu3d-style third-party WDDM package
  shape being tested for the next load-gate probe.
- The INF copies and registers `helios_umd.dll` as a bring-up user-mode display driver DLL. Its exported OpenAdapter
  entrypoints still fail explicitly until the DXVK/VKD3D-backed UMD path is implemented.
- Registers Display/VidPn/boot-display DDIs as explicit unsupported callbacks to keep the WDDM table shape complete
  during bring-up.
- Reports one small CPU-visible aperture segment for VidMm bring-up.
- Attempts virtio-gpu transport bring-up in `StartDevice`; failures are logged and leave render transport disabled
  instead of failing PnP start for the Gate 1 install/reboot smoke test.
- Implements minimal KMD allocation bookkeeping handles.
- Retires `DxgkDdiSubmitCommand` fences immediately through `DxgkCbNotifyInterrupt`/`DxgkCbQueueDpc`.
- Consumes paging-buffer requests with a null paging engine while GPU-VA/render caps remain unadvertised.

Policy:

- Do not install this over the working KMD. It has a separate service and binary name; keep it that way.
- Controlled binding smoke tests are acceptable after package validation, with snapshot/recovery available.
- Do not run DXVK/VKD3D/D3D workloads against this driver yet.
- Do not advertise GPU MMU, render, WDDM native fence, or DXGI-facing capability until the matching DDI path is real.
- The null scheduler/paging paths are only for adapter-load safety. They are not the production render path.
- Use `virtio-research-only-3d/viogpu/viogpu3d` as a reference for DDI shape only, not as a source of shortcuts.

Build:

```powershell
cargo build
```

From the repo tooling:

```bash
# via the Windows build VM helper
win_cargo(crate_dir = "kmd_render", args = ["build"])
win_cargo(crate_dir = "kmd_render", args = ["make", "--makefile", "Cargo.make.toml"])
```

Current validated package location on the Windows build mirror:

```text
C:\Users\Rupansh\helios-vgpu\kmd_render\target\debug\helios_kmd_render_package
```
