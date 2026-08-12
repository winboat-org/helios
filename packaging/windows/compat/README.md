# DaVinci Resolve compatibility

This x64, app-local package contains two narrowly scoped compatibility pieces:

- `atiadlxx.dll` supplies the small read-only AMD ADL enumeration surface that
  Resolve's Windows GPU detector requires. It reports the real Helios display
  adapter through the synthetic AMD candidate expected by Resolve.
- `OpenCL.dll` forwards the complete Khronos loader ABI to the packaged,
  version-pinned `OpenCL_real.dll`. When activated by the Helios launcher, it
  removes `CL_CONTEXT_D3D11_DEVICE_KHR` only from Resolve's observed invalid
  mixed GL/WGL/D3D11 context request. GL-only, D3D11-only, null-valued,
  duplicated, and unrelated property lists pass through unchanged.

CLVK itself remains conformant: a client that sends the mixed graphics-API
request directly receives the required `CL_INVALID_OPERATION`. The workaround
exists only beside `Resolve.exe`, and filtering is enabled only in the process
started by `Launch Resolve (Helios).cmd`.

## Install

Close Resolve. From an elevated PowerShell in this directory, run:

```powershell
.\Install-Resolve-Compatibility.ps1
```

Then start Resolve with `Launch Resolve (Helios).cmd`, which the installer
places beside `Resolve.exe`. The launcher sets one process-local environment
variable; it does not modify the user or machine environment.

The installer verifies hashes, backs up every pre-existing target below
`C:\ProgramData\Helios\compatibility\DaVinci Resolve`, and stages replacements
through temporary files. Re-running it performs a verified upgrade. It refuses
to overwrite files changed outside its recorded lifecycle or to operate while
Resolve is running.

For a non-default Resolve installation, pass its directory explicitly:

```powershell
.\Install-Resolve-Compatibility.ps1 -ResolveDirectory D:\Apps\Resolve
```

## Uninstall

Close Resolve, then run the saved uninstaller from an elevated PowerShell:

```powershell
& "C:\ProgramData\Helios\compatibility\DaVinci Resolve\Uninstall-Resolve-Compatibility.ps1"
```

It restores the exact files present before installation. If a managed target
has since changed, uninstall leaves it and all recovery state in place and
reports the conflict instead of deleting or overwriting third-party data.

Never install these DLLs in `System32`, `SysWOW64`, an AMD driver directory, or
as a system-wide OpenCL loader.
