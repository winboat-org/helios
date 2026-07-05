# Looking Glass IDD Pivot Notes

**Date:** 2026-06-08

The direct IDD-to-Helios scanout experiment is no longer the active direction. The IDD was able to initialize
Helios/Venus and feed nonzero test-pattern pixels into `IOCTL_HELIOS_PRESENT_BLOB`, but every host display backend
tested either failed before scanout, showed a grey window, or showed black. The active path is back to the normal
Looking Glass IDD producer with KVMFR/ivshmem transport. The current Helios System-class device is not a WDDM/DXGI
adapter, so it cannot be used by IddCx as a hardware render adapter.

## Current direction

- Keep the existing System-class Helios KMD and Mesa Venus ICD for application Vulkan/DXVK/vkd3d workloads.
- Use the Looking Glass IDD driver for the virtual desktop output.
- Use the standard Looking Glass KVMFR/ivshmem frame path for display transport.
- Disable the IDD Helios sink registry path by default:
  - `HeliosEnable=0`
  - `HeliosTestPattern=0`
- Prefer a real hardware DXGI adapter for the IDD render/copy path only if one exists independently of Helios. The
  IDD now logs every candidate adapter and supports an optional registry selector:
  - `RenderAdapter=substring of DXGI adapter description`

vkd3d is not a drop-in replacement inside the IDD UMDF driver. It is a D3D12-to-Vulkan user-mode runtime for
applications. The IDD receives compositor-owned DXGI/D3D textures from Windows; the meaningful acceleration switch
there is the DXGI render adapter selected by IddCx, not a Vulkan translation layer inside the driver. Hardware
acceleration through Helios would require a future Helios WDDM/DXGI render adapter.

## Runtime switches

Registry path:

```text
HKLM\SOFTWARE\LookingGlass\IDD
```

Values:

```text
HeliosEnable      DWORD  keep 0 for the active KVMFR/ivshmem path.
HeliosTestPattern DWORD  keep 0; diagnostic only for the retired direct-Helios path.
RenderAdapter     REG_SZ optional substring used to pick the IDD DXGI render adapter.
ExtraMode         REG_SZ optional preferred mode, currently 1920x1080@60*.
```

The standalone launcher now uses KVMFR by default for Looking Glass mode:

```text
HELIOS_DISPLAY=looking-glass ./tools/launch-helios-gtk.sh
```

This attaches `/dev/kvmfr0` as ivshmem, enables SPICE input, starts the git-built Looking Glass client, and keeps
the QEMU display backend headless. A SPICE-display client fallback remains available:

```text
HELIOS_DISPLAY=looking-glass HELIOS_LG_TRANSPORT=spice ./tools/launch-helios-gtk.sh
```

The standalone launcher currently defaults to the Intel host render node for QEMU/virglrenderer/Venus, while the
Looking Glass client defaults to Wayland/EGL on the host default GPU:

```text
HELIOS_QEMU_RENDER_GPU=intel
HELIOS_INTEL_RENDER_NODE=/dev/dri/renderD129
HELIOS_LG_RENDER_GPU=default
HELIOS_LG_DISPLAY_SERVER=wayland
HELIOS_LG_RENDERER=EGL
```

Intel is the launcher default and a known-good baseline. The NVIDIA host-renderer corruption that originally
motivated this default — Doom 2016 black/white frames plus possible Linux host framebuffer corruption or freeze —
was root-caused (guest Venus external-memory bind UB + host-visible cache-coherency) and fixed in `icd/mesa`, and
the Xid 13 FECS host freeze is fixed (2026-06-18); NVIDIA now renders the full Venus path correctly and is the
faster option. With Intel as Venus GPU0, Doom launches and plays, including a normal Steam launch. This does not
require the Looking Glass client itself to use Intel; the client can stay on the default host GPU for viewer-side
EGL compositing. The Intel Venus path is slower (observed below 20 fps in Doom), so for performance prefer the
NVIDIA Venus path now that the corruption/freeze is fixed.

For Doom/Venus performance captures, run the diagnostic launcher from the Windows desktop session:

```cmd
C:\Users\Rupansh\helios-vgpu\tools\launch-doom-diagnostic.cmd
```

It writes Helios IOCTL timing to `%USERPROFILE%\helios-doom-perf.txt` and Mesa software WSI timing to
`%USERPROFILE%\helios-doom-wsi-perf.txt`. The WSI file distinguishes common software-present fence wait and memory
invalidate time from Win32 copy/`GetDC`/`StretchDIBits` time. Keep live IOCTL logging off for Doom runs because it
can become part of the measured bottleneck. The Win32 WSI backend now defaults back to copying into the normal DIB
before GDI present; `HELIOS_WSI_DIRECT_MAP=1` can re-enable direct mapped-image present for controlled A/B tests.
The direct KVMFR producer path was removed on 2026-06-17. Mesa WSI no longer talks to a dedicated
`\\.\pipe\LookingGlassIDDHelios` pipe or posts client-side overlay frames; the supported path is the normal
DIB-shadow + `BitBlt` Win32 present feeding the standard Looking Glass IDD capture queue. Testing showed the real
bottleneck was cache coherency when GDI read Venus-mapped BAR memory directly, not GDI/BitBlt itself.

The NVIDIA path is a fully supported Venus render target (the FECS host freeze that previously made it testing-only
was fixed 2026-06-18); Intel stays the launcher default purely as a config choice. Use the explicit `nvidia` mode
rather than the vague host `default` mode; the launcher checks `nvidia-smi` first and forces the NVIDIA GLVND/EGL
environment for QEMU:

```text
HELIOS_QEMU_RENDER_GPU=nvidia \
  HELIOS_DISPLAY=looking-glass ./tools/launch-helios-gtk.sh
```

In the rare event the NVIDIA kernel module reports NVRM register-read errors or `nvidia-smi` cannot talk to the
driver after a run, the host driver is wedged below QEMU/virglrenderer. Reboot or reload the NVIDIA driver before
collecting further Venus data; guest Helios IOCTL timings are not meaningful for that failure mode.

Looking Glass KVMFR defaults to 512 MiB for the normal desktop stream. The host `/dev/kvmfr0` backing device must be
created at the same size, or the launcher must be overridden with `HELIOS_KVMFR_SIZE`.

The launcher also defaults Windows to a single-socket topology (`16,sockets=1,cores=16,threads=1`). Override with
`HELIOS_SMP`, `HELIOS_SOCKETS`, `HELIOS_CORES`, and `HELIOS_THREADS` if a different topology is needed.

## Build tooling

`tools/win-mcp` now has a dedicated `win_looking_glass` tool. It mirrors the Linux source tree to the local Windows
build mirror with `robocopy`, configures `LookingGlass\host` with CMake/Ninja, and builds the Windows host from
local disk. The current MCP process must be restarted before the new tool appears in the available tool schema.

Default behavior:

```text
source_root: Z:\
mirror:      C:\Users\Rupansh\helios-vgpu
build dir:   C:\Users\Rupansh\helios-lookingglass-host-build
configure:   cmake -G Ninja -DCMAKE_BUILD_TYPE=RelWithDebInfo -DUSE_NVFBC=OFF
```

If the VM source share is not mounted as `Z:\`, call `win_looking_glass` with `source_root` set to the active share
path.

The Windows VM currently sees the repository at `Z:\` again after updating the host virtiofsd package. The
dedicated MCP build path has also been verified manually with the same commands used by `win_looking_glass`.
The tool normalizes `source_root` before forming the absolute `icd\mesa` exclude path; without that normalization,
`Z:\` became `Z:\\icd\mesa`, robocopy did not exclude Mesa, and the sync failed on Linux-only Mesa filenames.

```text
robocopy Z:\ C:\Users\Rupansh\helios-vgpu /MIR /XD target .git icd\mesa Z:\icd\mesa ...
cmake -S C:\Users\Rupansh\helios-vgpu\LookingGlass\host \
      -B C:\Users\Rupansh\helios-lookingglass-host-build \
      -G Ninja -DCMAKE_BUILD_TYPE=RelWithDebInfo -DUSE_NVFBC=OFF
cmake --build C:\Users\Rupansh\helios-lookingglass-host-build
```

Result:

```text
[15/15] Linking C executable looking-glass-host.exe
```

One upstream Looking Glass source file needed a MinGW `-Werror=format-truncation` fix in the DXGI RGB24
postprocessor before the Windows host would compile.

The IDD driver is a WDK/MSBuild project, not part of the `win_looking_glass` host-server CMake build. Build it with
the dedicated MCP helper:

```text
win_looking_glass_idd {}
```

`win_looking_glass_idd` mirrors the tree to local NTFS with robocopy, excludes `icd\mesa`, copies only
`icd\mesa\include\vulkan` for the IDD Vulkan headers, then runs MSBuild on
`C:\Users\Rupansh\helios-vgpu\LookingGlass\idd\LGIdd.sln`. This avoids both WDK build I/O on `Z:\` and Windows
reserved-name failures from the full Mesa include tree.

The current IDD build completes and emits:

```text
C:\Users\Rupansh\helios-vgpu\LookingGlass\idd\x64\Release\LGIdd\LGIdd.dll
C:\Users\Rupansh\helios-vgpu\LookingGlass\idd\x64\Release\LGIdd\LGIdd.inf
C:\Users\Rupansh\helios-vgpu\LookingGlass\idd\x64\Release\LGIdd\lgidd.cat
```

## Service-safe Vulkan ICD

The IDD runs inside WUDFHost as `NT AUTHORITY\LOCAL SERVICE`. Elevated/service Vulkan loader paths ignore
`VK_DRIVER_FILES`, so the Helios ICD must be registered in HKLM and loadable from a service-readable directory.

Current working deployment:

```text
C:\ProgramData\HeliosVulkan\vulkan_virtio.dll
C:\ProgramData\HeliosVulkan\virtio_devenv_icd.x86_64.json
HKLM\SOFTWARE\Khronos\Vulkan\Drivers
  C:\ProgramData\HeliosVulkan\virtio_devenv_icd.x86_64.json = DWORD 0
```

The JSON uses a relative library path:

```json
{
  "file_format_version": "1.0.1",
  "ICD": {
    "api_version": "1.4.352",
    "library_arch": "64",
    "library_path": ".\\vulkan_virtio.dll"
  }
}
```

`icd/win-build/mingw-native.ini` links the Mesa ICD with the MinGW runtime statically (`-static`,
`-static-libgcc`, `-static-libstdc++`). This removes the `libwinpthread-1.dll` runtime dependency that prevented
LocalService from loading `vulkan_virtio.dll` with loader error 126.

Validation command used through a LocalService scheduled task:

```text
C:\Windows\System32\vulkaninfo.exe --summary
```

Expected result:

```text
GPU0: Virtio-GPU Venus (Intel(R) Graphics (ARL)), driverName=venus
GPU1: Virtio-GPU Venus (llvmpipe ...), driverName=venus
```

## Current validation

Validated/changed on 2026-06-08:

- `vulkaninfo --summary` works both interactively and as LocalService using the ProgramData ICD registration.
- Direct IDD-to-Helios scanout was abandoned after black/grey output despite nonzero test-pattern input.
- IDD rollback build installed as a new driver package; `HeliosEnable=0` and `HeliosTestPattern=0`.
- IDD now logs DXGI render-adapter candidates and selection.
- `HELIOS_DISPLAY=looking-glass` in the standalone script defaults back to KVMFR/ivshmem transport.
- The standalone script defaults QEMU/Venus to Intel host rendering and passes the Intel render node explicitly to
  `egl-headless`; the Looking Glass client defaults to Wayland/EGL on the default host renderer.
- The standalone script defaults KVMFR to 512 MiB for the normal desktop stream.
- The standalone script now presents Windows as one socket with sixteen cores by default, instead of sixteen
  one-core sockets.
- The standalone script shuts down the VM through QMP/ACPI when the Looking Glass client exits.
- IDD capture now drops a frame instead of waiting up to 100 ms when all D3D12 copy queues are busy. This favors
  fresh display output over freeze-then-catch-up behavior.
- The IddCx 1.10 HDR/WCG path now advertises 10 bpc only, so Windows is pushed toward `FRAME_TYPE_RGBA10` instead
  of selecting the 8-bit BGRA path.

## Open items

- Boot with the standalone KVMFR path and confirm the IDD monitor is not phantom.
- Inspect `C:\ProgramData\Looking Glass (IDD)\looking-glass-idd.txt` for:
  - `IDD render adapter[...]`
  - `IDD selected render adapter: ...`
  - `Created CD3D12Device`
  - `D3D12 copy queues busy, dropping frame` only under real capture pressure
- Inspect the Linux client log for `FRAME_TYPE_RGBA10` after a display replug or guest reboot.
- If a non-Helios hardware DXGI adapter exists and the selected adapter is wrong, set:

```powershell
New-ItemProperty -Path HKLM:\SOFTWARE\LookingGlass\IDD `
  -Name RenderAdapter -Value "<substring>" -PropertyType String -Force
```
