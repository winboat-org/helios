# DXVK + VKD3D-Proton Bring-Up

Status: initial launcher and documentation, 2026-06-17.

## Goal

Run Windows D3D8/9/10/11/12 games through app-local translation DLLs:

```text
game.exe
  -> DXVK d3d8/d3d9/d3d10core/d3d11/dxgi.dll
  -> VKD3D-Proton d3d12/d3d12core.dll
  -> Windows Vulkan loader
  -> Helios Mesa Venus ICD
  -> Helios KMDF IOCTL transport
  -> virtio-gpu Venus / virglrenderer / host GPU
```

DXVK and VKD3D-Proton stay unmodified. The first implementation surface is a launcher that installs the downloaded DLLs next to the game executable and launches the process with the Helios ICD selected through Vulkan loader environment variables.

## Local Inputs

The expected downloaded DLL layout is already present in this tree:

```text
dxvk/x64/{d3d8,d3d9,d3d10core,d3d11,dxgi}.dll
dxvk/x32/{d3d8,d3d9,d3d10core,d3d11,dxgi}.dll
vkd3d-proton/x64/{d3d12,d3d12core}.dll
vkd3d-proton/x86/{d3d12,d3d12core}.dll
```

The default Helios Vulkan ICD manifest is:

```text
C:\ProgramData\HeliosVulkan\virtio_devenv_icd.x86_64.json
```

Install or refresh that manifest with `tools\install-helios-icd.ps1` after rebuilding the Mesa ICD.

## Launcher

Use:

```powershell
powershell -ExecutionPolicy Bypass -File Z:\tools\launch-translated-d3d-game.ps1 C:\Games\Game\game.exe -arg1 -arg2
```

For Steam, set the game's launch options to the output of:

```powershell
powershell -ExecutionPolicy Bypass -File Z:\tools\launch-translated-d3d-game.ps1 -PrintSteamCommand
```

That currently prints:

```text
"Z:\tools\launch-translated-d3d-game.cmd" %command%
```

If Steam on Windows refuses `%command%` replacement for a title, do a one-time app-local install and leave the Steam launch options empty:

```powershell
powershell -ExecutionPolicy Bypass -File Z:\tools\launch-translated-d3d-game.ps1 -InstallOnly C:\Games\Game\game.exe
```

This copies the translation DLLs and writes `dxvk.conf` next to the game. The Helios ICD is already registered globally through `tools\install-helios-icd.ps1`, so the game can still reach the Helios Vulkan driver when launched normally by Steam.

The launcher:

- Detects x86 vs x64 from the game executable PE header.
- Copies the selected DXVK/VKD3D DLLs next to the game executable.
- Creates `<game>.exe.local` unless `-NoLocalRedirectFile` is used.
- Creates `dxvk.conf` if none exists.
- Sets `VK_DRIVER_FILES` and `VK_ICD_FILENAMES` to the Helios ICD manifest.
- Sets `DXVK_CONFIG_FILE` and `DXVK_LOG_LEVEL`.
- Supports `-InstallOnly` for Steam titles where launch-option command replacement is unreliable.

Useful options:

```powershell
# DX11-only title.
powershell -ExecutionPolicy Bypass -File Z:\tools\launch-translated-d3d-game.ps1 -Apis d3d11 C:\Games\Game\game.exe

# D3D12 title that also needs DXGI from DXVK.
powershell -ExecutionPolicy Bypass -File Z:\tools\launch-translated-d3d-game.ps1 -Apis d3d12 C:\Games\Game\game.exe

# Use DXVK fullscreen exclusive instead of the safer default.
powershell -ExecutionPolicy Bypass -File Z:\tools\launch-translated-d3d-game.ps1 -AllowFse C:\Games\Game\game.exe

# Launch without copying DLLs again.
powershell -ExecutionPolicy Bypass -File Z:\tools\launch-translated-d3d-game.ps1 -NoInstallDlls C:\Games\Game\game.exe
```

## Windows-Specific DXVK Notes

The DXVK Windows wiki says Windows use is not officially supported, even though it can work on clean systems. Treat bugs as local integration issues until reproduced on Linux/Proton or narrowed to a DXVK regression.

Do not replace DLLs in `C:\Windows\System32` or `C:\Windows\SysWOW64`. Use app-local DLLs only. Match DLL architecture to the game architecture; Windows will not load the wrong architecture.

Some games load `dxgi.dll` from `System32` even when D3D DLLs are app-local. That can crash or silently fall back to native D3D. The launcher creates a `.local` redirect marker, but some titles may still need deeper DLL-load debugging before considering registry DevOverride.

Prefer borderless fullscreen first. DXVK disables fullscreen exclusive by default on Windows; enable it only when a game needs it with `-AllowFse`, which writes `dxvk.allowFse = True`.

Disable overlays and capture tools while debugging. Launcher overlays, RTSS, OBS, driver overlays, and mods that hook D3D/Vulkan can interfere with DXVK.

D3D12 games should use VKD3D-Proton together with DXVK. Some mixed D3D11/D3D12 titles create both device types, and DXVK's wiki calls out that the paired setup is required for those cases.

## First Implementation Tasks

1. Run a known-good D3D11 sample through `-Apis d3d11` and confirm `d3d11.log` shows DXVK selecting the Helios Venus ICD.
2. Run a simple D3D12 sample through `-Apis d3d12` and confirm VKD3D-Proton reaches Vulkan device creation.
3. Add a small log collector that copies `dxgi.log`, `d3d11.log`, `d3d12.log`, and Vulkan loader output into a per-game diagnostics directory.
4. Decide whether the Venus ICD needs stable LUID/device-id reporting for DXVK interop paths, as tracked in `ARCH.md`.
5. Build a compatibility table for tested titles with API, architecture, launch options, symptom, and host GPU.

## References

- DXVK Windows wiki: https://github.com/doitsujin/dxvk/wiki/Windows
- DXVK repository: https://github.com/doitsujin/dxvk
- VKD3D-Proton repository: https://github.com/HansKristian-Work/vkd3d-proton
