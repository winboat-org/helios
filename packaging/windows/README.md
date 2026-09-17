# Helios Windows x64 bundle with WoW64 Direct3D/OpenGL/Vulkan

This archive installs the Helios WDDM driver, its x64 user-mode graphics and
compute stack, and the 32-bit Direct3D/Vulkan/OpenGL components needed by WoW64 games:

- Direct3D 11 through DXVK embedded in `helios_umd.dll` (x64) / `helios_umd32.dll` (x86)
- Direct3D 12 through vkd3d-proton embedded in `helios_umd12.dll` (x64) / `helios_umd12_32.dll` (x86)
- Vulkan through Mesa Venus (`vulkan_virtio.dll`)
- desktop OpenGL through Mesa Zink's Microsoft WGL ICD
- 32-bit Vulkan through a separately built x86 Mesa Venus ICD
- 32-bit desktop OpenGL through a separately built x86 Zink WGL ICD
- OpenCL through CLVK with its clspv compiler embedded
- official Khronos Vulkan and OpenCL loaders when Windows has no loader yet
- optional, app-local DaVinci Resolve GPU-detection shim

## Install

The driver is CI/test-signed, not Microsoft production-signed. Disable Secure
Boot in the VM firmware, then run `HeliosSetup.exe`. It is a single
self-contained executable — the installer, the graphics payload, the PowerShell
install logic and the manifest are all inside it (see
`installer-rs/README.md`); there is no folder of loose files. It offers Install /
Repair / Update / Uninstall and streams the log with a progress bar.

For unattended installs, `HeliosSetup.exe --silent` installs or repairs without
a UI and overwrites any existing installation, writing a log to
`C:\ProgramData\Helios\logs\setup.log` and returning the payload's exit code
(`3010` = reboot required). `HeliosSetup.exe --silent --automatic` is WinBoat's
unattended provisioning mode and is what the OEM `install.bat` calls. On a new VM
the first run enables Windows test-signing and asks for a reboot. Run it again
after the reboot to install the stack, then reboot once more before testing it.

To update an existing installation, run a newer `HeliosSetup.exe`; it shows
**Update** with the version transition, and `--silent` performs the same
overwrite. The uninstall/verify scripts are copied into `C:\ProgramData\Helios`,
so Helios can be removed later even without the original installer.

For debugging, the installer can be built from the payload scripts directly (they
are embedded but also shipped in the source bundle):

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\Install-Helios.ps1 -EnableTestSigning
```

The installer verifies every payload hash before changing the machine. It does
not replace `opengl32.dll`, and it never overwrites existing Khronos loader
DLLs. Vulkan and OpenCL coexist with other vendors through their standard ICD
registries. OpenGL is registered only on the Helios display adapter software
key.

If the virtio-gpu device is using Red Hat's `viogpudo` driver, desktop setup
shows a Yes/No dialog (default No) before uninstalling that driver package and
replacing it with Helios. A remote console uses the equivalent `[y/N]` prompt.
For WinBoat or another unattended orchestrator, use automatic mode:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\Install-Helios.ps1 -Automatic
```

`-Automatic` enables test-signing when needed and replaces `viogpudo` without a
prompt. It copies the bundle to `C:\ProgramData\Helios\provisioning`, installs a
SYSTEM startup task, and reports its durable state in
`C:\ProgramData\Helios\provisioning-status.json`. An orchestrator should reboot
at `test-signing-restart-required`, then reboot again at
`driver-restart-required`. After the second reboot, automatic verification
publishes `finished` and removes the startup task. `failed` includes an error
message. `-Unattended` is accepted as an alias.

Run the health check later with:

```powershell
C:\ProgramData\Helios\Verify-Helios.ps1 -RunSmokeTests
```

Run that command in the logged-in desktop session after the final reboot; the installer performs only the
non-rendering registration/hash checks before rebooting.

The smoke-test pass includes a 1920x1080 RGBA16F WGL/OpenCL sharing case. It
requires the matching Helios host image as well as the Windows bundle and
verifies texture import, acquire, pixel readback, release, and queue finish.

D3D12 is enabled by default. To disable it, set DWORD `UmdD3D12=0` under
`HKLM\SOFTWARE\Helios` and restart affected applications (reboot for DWM).
Deleting the value restores the enabled default. Installation preserves an
existing explicit disable. The Direct3D smoke probes run as both x64 and x86
processes, testing device creation and texture clear/readback. D3D12 creation
expects failure when that disable is present and then skips its readback test.

## DaVinci Resolve compatibility

Resolve's Windows GPU detector requires a vendor-specific enumeration path and
does not admit a generic DXGI/OpenCL adapter by itself. The app-local ADL shim
reports the real Helios display adapter through the AMD enumeration surface
Resolve expects. CLVK directly accepts Resolve 21.0.4's nonstandard context
combining WGL and D3D11 sharing for compatibility with AMD and Intel runtimes.

Close Resolve and run the compatibility directory's
`Install-Resolve-Compatibility.ps1` from an elevated PowerShell. Resolve can
then be started normally; no special launcher is required. The compatibility
installer is explicit and separate from the system-stack installer. It backs
up and hash-tracks its target, supports verified upgrades, and includes a saved
uninstaller that restores the pre-Helios file. See the adjacent README for the
exact command, implementation scope, and rollback behavior.

Uninstall with:

```powershell
C:\ProgramData\Helios\Uninstall-Helios.ps1
```

The uninstaller deliberately keeps Khronos loader DLLs because another vendor
installed later may use them. Add `-RemoveKhronosLoaders` to remove loaders that
this package originally installed, but only if their hashes are unchanged.

## Current limits

- D3D12 resource ownership across APIs, host-loss/error propagation, and some
  teardown paths remain incomplete. Default enablement and device creation
  smoke coverage do not establish full conformance or visual correctness.

- The bundle includes separate x86 Direct3D 11/12 UMDs and Vulkan/OpenGL
  components. Device creation and offscreen readback are smoke checks;
  application and presentation acceptance are separate. OpenCL remains x64-only.
- The QEMU Helios/Venus protocol changes quickly. Build the host QEMU/render
  side from a compatible source revision recorded in `manifest.json`.
- CI uses an ephemeral public test certificate whose private key is destroyed
  after signing. A public release requires Microsoft attestation/WHQL signing
  or another production signing process.
