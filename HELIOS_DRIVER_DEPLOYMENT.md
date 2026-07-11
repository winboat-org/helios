# Helios Driver Deployment

This file is the canonical deployment procedure. Do not rediscover KMD, UMD, or ICD hotplug
mechanics during graphics debugging. Use the scripts, and fix the scripts if a new edge case is
found.

**Active display mode (2026-07-11):** Helios owns the VidPn/virtio-gpu output and
the VM uses `qemu-helios`, normally with egl-headless + VNC. Looking Glass/IddCx
instructions below are retained only for restoring or diagnosing the former mode.

## Rules

- KMD, UMD, and Vulkan ICD deployment are separate operations.
- A deployment is not successful unless the script verifies the destination hash against the source
  hash and prints the final device or registry state.
- Do not manually copy into DriverStore during normal iteration. DriverStore writes bypass SetupAPI
  catalog/package state and can leave Windows bound to stale or inconsistent package metadata.
- ProgramData is the normal UMD/ICD hotplug location because those paths are selected by registry
  values read by new user-mode clients.
- In the historical Looking Glass IDD mode, keep `Looking Glass (host)`
  stopped/disabled. Only `LGIddHelper` should run.
- The VM exposes no reliable ICMP. SSH failing with ping-like checks is not proof the guest is down.

## Microsoft Rules That Matter

- Modern WDDM graphics packages should run from the DriverStore (`DIRID 13`).
- The Direct3D runtime loads the UMD name from the display driver's software key. Helios uses
  `UserModeDriverName`.
- `DIRID 13` packages run from unique DriverStore directories. Do not combine `DIRID 13` with
  `COPYFLG_IN_USE_TRY_RENAME`; `infverif` rejects that combination.
- For an existing PCI device, use `devcon update <inf> <hardware-id>` when DevCon is available.
  Do not use `devcon install`; Microsoft documents that it creates a new root-enumerated devnode.
- `pnputil` is the supported built-in fallback for driver-store add/install and device state
  operations, but DevCon is preferred for binding the Helios PCI devnode to a rebuilt KMD package.

References:

- https://learn.microsoft.com/en-us/windows-hardware/drivers/develop/run-from-driver-store
- https://learn.microsoft.com/en-us/windows-hardware/drivers/display/loading-a-user-mode-display-driver
- https://learn.microsoft.com/en-us/windows-hardware/drivers/display/copy-flags-to-support-pnp-stop-directive
- https://learn.microsoft.com/en-us/windows-hardware/drivers/install/using-the-devcon-tool-to-install-a-driver-package
- https://learn.microsoft.com/en-us/windows-hardware/drivers/devtest/devcon-install
- https://learn.microsoft.com/en-us/windows-hardware/drivers/devtest/pnputil-command-syntax

## KMD Install

Build first with the Windows build helper:

```powershell
# From Codex MCP:
# win_cargo crate_dir:"kmd_render" args:["make","--makefile","Cargo.make.toml"]
```

Dry-run discovery:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\install-helios-kmd.ps1 -PlanOnly
```

When the VM is intentionally booted without the Helios `virtio-gpu-gl-pci` device, the PCI devnode
does not exist and `devcon update` cannot bind anything. Stage the signed package only:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\install-helios-kmd.ps1 -StageOnly
```

Install the KMD only:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\install-helios-kmd.ps1
```

The script:

- Resolves the active Helios PCI instance and active DriverStore package from the active INF.
- Uses `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render\ImagePath` as the first
  DriverStore source of truth. This handles in-place active package replacement where
  `C:\Windows\INF\oemNN.inf` metadata can lag the actual loaded DriverStore directory.
- Regenerates `helios_kmd_render.cat` with `Inf2Cat.exe` before signing. `Inf2Cat.exe` may only
  exist under the WDK `x86` bin directory; the script searches both `x64` and `x86`.
- Signs both `helios_kmd_render.sys` and `helios_kmd_render.cat` with a machine-store
  `CN=WDRLocalTestCert` if needed.
- Imports that cert into `LocalMachine\Root` and `LocalMachine\TrustedPublisher`. This is required
  when LoginUI/Explorer are crash-looping and CurrentUser certs are unavailable.
- Stops/disables the Looking Glass host service so it cannot fight IDD mode.
- Clears stale Helios pending rename operations.
- Backs up active DriverStore files under `C:\ProgramData\HeliosDeployBackups\<timestamp>`.
- Full KMD package installs always include `helios_umd.dll`, because the INF `CopyFiles` and catalog
  include it. A package missing the UMD fails `pnputil /add-driver` with "file not found".
- Full KMD package installs publish with `devcon update <inf> <hardware-id>` when the WDK DevCon
  tool is present. `pnputil /add-driver ... /install` is only the fallback or explicit
  `-UsePnPUtil` path.
- `-StageOnly` uses `pnputil /add-driver <inf>` for the no-Helios-device recovery boot. It does
  not attempt binding, restart, disable, enable, or active DriverStore verification because there
  is no active Helios devnode. On the next boot with Helios present, PnP should select the highest
  `DriverVer`; otherwise rerun the normal installer so DevCon can bind the present device.
- Do not raw-copy INF/SYS/CAT into the active DriverStore for a full package; Windows must register
  the catalog in its driver database and bind the package to the existing PCI devnode.
- If SetupAPI logs `Service image path changed. Restart required for any devices using this
  service`, do not repeatedly force PnP restarts from SSH. The reliable activation path is reboot.
  The script now treats this as reboot-required unless `-RestartDevice` is explicitly passed for a
  controlled test.

`-BinaryOnly` is an emergency raw-copy mode. It is disabled unless `-ForceDriverStoreEdit` is also
passed. It directly edits the active DriverStore package, backs up the active files,
regenerates/signs the active catalog, and verifies hashes afterward. Do not use it for normal KMD
iteration.

## UMD Hotplug

Build first:

```powershell
# From Codex MCP:
# win_cargo crate_dir:"umd" args:["build"]
```

Dry-run discovery:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\hotplug-helios-umd.ps1 -PlanOnly
```

Default UMD hotplug:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\hotplug-helios-umd.ps1
```

The default mode installs the UMD to:

```text
C:\ProgramData\HeliosUmd\helios_umd.dll
```

and rewrites the active display software key:

```text
UserModeDriverName = C:\ProgramData\HeliosUmd\helios_umd.dll x4
InstalledDisplayDrivers = helios_umd
```

This avoids repeated active DriverStore writes during UMD-only iteration. The script still rebinds
the display software key so new D3D processes load the new DLL. It does not disable/re-enable the
Helios PCI adapter by default; pass `-RestartDevice` only for a controlled adapter-restart test.

Alternative modes:

```powershell
# Active DriverStore fallback. Use only if ProgramData override is suspected.
powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\hotplug-helios-umd.ps1 -Mode DriverStore -ForceDriverStoreEdit

# Microsoft-supported package-upgrade shape for DIRID 13 packages.
powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\hotplug-helios-umd.ps1 -Mode PackageUpgrade
```

Use `-KillUmdUsers` only when a mapped DLL blocks replacement and you accept DWM/WUDF/Explorer
process termination:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\hotplug-helios-umd.ps1 -KillUmdUsers
```

The UMD script:

- Stops/disables the Looking Glass host service.
- Clears stale Helios pending rename operations.
- Copies through a temporary file, moves into place, and verifies SHA256.
- Fails loudly if the destination is still loaded and cannot be replaced.
- Leaves the Helios adapter running by default. With `-RestartDevice`, rebinds Helios with
  `pnputil /disable-device /force` and `/enable-device`.
- Runs `C:\Users\Rupansh\helios-probe\d3d11_devicecreate_probe.exe` unless `-NoProbe` is passed.

The UMD/DXVK bridge must not hardcode `vulkan_virtio*.dll` names. It resolves Helios ICD helper
exports by following the same path normal Vulkan apps use: `VK_DRIVER_FILES` /
`VK_ICD_FILENAMES`, then the Khronos `SOFTWARE\Khronos\Vulkan\Drivers` registry values, then the
canonical ProgramData manifest. If the ICD DLL path changes, rerun `install-helios-icd.ps1`; do
not patch UMD code or copy an arbitrary DLL beside the UMD.

## Vulkan ICD Hotplug

After Mesa Venus ICD rebuild:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\install-helios-icd.ps1 -PlanOnly -NoSmoke
powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\install-helios-icd.ps1
```

The ICD script:

- Copies `vulkan_virtio.dll` to a content-hashed ProgramData path:
  `C:\ProgramData\HeliosVulkan\vulkan_virtio-<hash>.dll`.
- Writes `C:\ProgramData\HeliosVulkan\virtio_devenv_icd.x86_64.json` atomically.
- Parses the generated JSON before installing it.
- Registers that manifest under the Khronos Vulkan Drivers key.
- Cleans stale Helios/Virtio ICD registry values unless `-NoRegistryCleanup` is passed.
- Does not remove old content-hashed DLLs unless `-PruneOld` is passed.
- Grants read/execute ACLs to normal app containers/users.

For process-local testing without registry:

```powershell
$env:VK_DRIVER_FILES = "C:\ProgramData\HeliosVulkan\virtio_devenv_icd.x86_64.json"
```

## Post-Deploy Checks

```powershell
Get-CimInstance Win32_PnPEntity |
  ? { $_.PNPDeviceID -like 'PCI\VEN_1AF4&DEV_1050*' } |
  select Name,Status,ConfigManagerErrorCode,PNPDeviceID | fl

Get-Service LGIddHelper
Get-Service "Looking Glass (host)" -ErrorAction SilentlyContinue

C:\Users\Rupansh\helios-probe\d3d11_devicecreate_probe.exe
Get-Content C:\Windows\Temp\helios_umd.log -Tail 120
```

Expected baseline before chasing IDD frames:

- Helios render adapter is Code 0.
- Looking Glass IDD is present and `LGIddHelper` is running.
- `Looking Glass (host)` is stopped/disabled.
- `D3D11CreateDevice` on Helios returns `S_OK`.
- QEMU logs do not show recurring malformed virtio-gpu commands after a clean deploy.
