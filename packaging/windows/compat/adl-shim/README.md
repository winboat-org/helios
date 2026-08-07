# Helios ADL compatibility shim

This x64-only DLL supplies the small, read-only AMD Display Library surface
used by DaVinci Resolve's Windows GPU detector. It discovers the present Helios
display adapter through SetupAPI and reports one synthetic ADL adapter with the
real display name and PnP identity.

The ADL vendor ID is deliberately reported as AMD (`0x1002`). This seeds the
AMD candidate that Resolve otherwise requires from a physical AMD Windows
driver. Resolve still performs its normal DXGI enumeration and uses
`clGetDeviceIDsFromD3D10KHR` to associate that DXGI adapter with the real Helios
OpenCL device.

Resolve does not use that successful DXGI association when joining an AMD
candidate to OpenCL. Instead, it compares the bus and device fields from AMD's
legacy `cl_amd_device_attribute_query` extension. CLVK does not advertise this
vendor-only extension, so those OpenCL fields are unknown (`-1`). The shim also
reports the ADL PCI location as unknown, allowing Resolve's equality matcher to
join the two records. The real adapter identity remains available through PnP,
the display name, and Resolve's independent DXGI enumeration.

The shim does not implement display controls, tuning, telemetry, encoding, or
any other AMD functionality. Deploy it only beside applications that need this
compatibility path; do not install it as a system-wide replacement for AMD's
`atiadlxx.dll`.

## Use with DaVinci Resolve

Close Resolve, then copy `atiadlxx.dll` from this directory beside
`Resolve.exe`, normally at:

```text
C:\Program Files\Blackmagic Design\DaVinci Resolve\atiadlxx.dll
```

Relaunch Resolve. To remove the workaround, close Resolve and delete only that
app-local copy. Never copy the shim into `System32`, `SysWOW64`, or an AMD
driver directory. A guest with a real AMD display adapter should use AMD's ADL
implementation instead of this shim.

The implemented entry points are:

- ADL/ADL2 initialization and destruction
- adapter count and `AdapterInfo`
- ASIC family and primary-adapter queries
- ADL/ADL2 driver-version queries
- ADL2 function lookup

The local ABI declarations match the public AMD ADL SDK structures and are
guarded with size and member-offset assertions.
