# Helios Windows x64 bundle

This archive installs the Helios WDDM driver and its x64 user-mode graphics and
compute stack:

- Direct3D 11 through the DXVK core embedded in `helios_umd.dll`
- Vulkan through Mesa Venus (`vulkan_virtio.dll`)
- desktop OpenGL through Mesa Zink's Microsoft WGL ICD
- OpenCL through CLVK with its clspv compiler embedded
- official Khronos Vulkan and OpenCL loaders when Windows has no loader yet
- the Microsoft Visual C++ x64 runtime required by the WDDM/DXVK UMD

## Install

The driver is CI/test-signed, not Microsoft production-signed. Disable Secure
Boot in the VM firmware, then double-click `Install-Helios.cmd`. On a new VM the
first run enables Windows test-signing and asks for a reboot. Run it again after
the reboot to install the stack, then reboot once more before testing it.

From an elevated 64-bit PowerShell, the equivalent command is:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\Install-Helios.ps1 -EnableTestSigning
```

The installer verifies every payload hash before changing the machine. It does
not replace `opengl32.dll`, and it never overwrites existing Khronos loader
DLLs. Vulkan and OpenCL coexist with other vendors through their standard ICD
registries. OpenGL is registered only on the Helios display adapter software
key.

Run the health check later with:

```powershell
C:\ProgramData\Helios\Verify-Helios.ps1 -RunSmokeTests
```

Run that command after the final reboot; the installer performs only the
non-rendering registration/hash checks before rebooting.

Uninstall with:

```powershell
C:\ProgramData\Helios\Uninstall-Helios.ps1
```

The uninstaller deliberately keeps Khronos loader DLLs because another vendor
installed later may use them. Add `-RemoveKhronosLoaders` to remove loaders that
this package originally installed, but only if their hashes are unchanged.
The shared Microsoft Visual C++ runtime is also left installed.

## Current limits

- This package is x64-only. Native 32-bit applications need separately built
  x86 UMD, Mesa, CLVK, and loader binaries.
- The QEMU Helios/Venus protocol changes quickly. Build the host QEMU/render
  side from a compatible source revision recorded in `manifest.json`.
- CI uses an ephemeral public test certificate whose private key is destroyed
  after signing. A public release requires Microsoft attestation/WHQL signing
  or another production signing process.
