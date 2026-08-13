# DaVinci Resolve compatibility

This x64, app-local package contains one narrowly scoped compatibility piece:

- `atiadlxx.dll` supplies the small read-only AMD ADL enumeration surface that
  Resolve's Windows GPU detector requires. It reports the real Helios display
  adapter through the synthetic AMD candidate expected by Resolve.

Resolve 21.0.4 also sends a nonstandard mixed GL/WGL/D3D11 OpenCL context
request. CLVK accepts that request directly, matching AMD and Intel runtime
behavior, so no OpenCL proxy or special launcher is required.

## Install

Close Resolve. From an elevated PowerShell in this directory, run:

```powershell
.\Install-Resolve-Compatibility.ps1
```

Resolve can then be started normally from its usual shortcut.

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

Never install this DLL in `System32`, `SysWOW64`, or an AMD driver directory.
