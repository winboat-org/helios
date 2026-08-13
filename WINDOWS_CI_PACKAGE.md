# Windows CI package

The `Windows graphics and compute bundle` GitHub Actions workflow builds one
x64 Windows archive that turns a clean Helios Windows 11 guest into a
system-wide graphics/compute installation. It includes x86 Vulkan/OpenGL
components for WoW64 applications alongside the native x64 stack.

## What the workflow builds

The jobs are independent so an error points at the actual component:

1. `driver` builds the DXVK static D3D11 core, embeds it in `helios_umd.dll`,
   and builds/packages the Rust WDDM kernel driver.
2. `mesa` and `mesa_x86` build the pinned Mesa submodule for x64 and x86 with
   both the Venus Vulkan ICD and the Zink WGL OpenGL ICD enabled.
3. `opencl` builds pinned CLVK with the clspv online compiler embedded. End-user
   machines therefore do not need `clspv.exe` or `CLVK_CLSPV_PATH`.
4. `loaders` builds the official x64 Vulkan/OpenCL loaders, the x86 Vulkan
   loader, and architecture-matched smoke probes.
5. `compatibility` builds and validates the app-local DaVinci Resolve ADL shim.
6. `package` test-signs the final driver package and compatibility shim, hashes
   every distributed binary,
   and creates `helios-windows-x64-<version>-<commit>.zip`.

The workflow runs for pull requests and pushes to `wddm`, and can be started
manually. A tag beginning with `v` also publishes the zip and its SHA-256 file
as a GitHub Release.

## Reproducibility and source pins

The Helios, Mesa, and DXVK revisions come from the checked-out commit and its
gitlinks. The Windows OpenCL build uses the `winboat-org/clvk-helios` fork for
guest DXGI/OpenCL device association. Its repository and commit, along with the
Vulkan-Loader, Vulkan-Headers, and OpenCL-ICD-Loader commits, are pinned in
`.github/workflows/windows-stack.yml`. Toolchain versions are pinned there as
well. Every resulting source revision is written to `manifest.json`.

When updating an external pin, first build and run all four packaged probes in
the VM. In particular, CLVK and Zink are consumers of the Venus ICD and can
expose synchronization/protocol mismatches that a successful compile cannot.

## Signing model

CI creates a unique, non-exportable test-signing key for each bundle. It signs
the SYS and UMD before creating the catalog, signs the final catalog, exports
only the public certificate, then destroys the CI private key. The installer
adds that public certificate to `Root` and `TrustedPublisher`.

This is intentionally a development distribution. Windows must boot with test
signing enabled, which requires Secure Boot to be disabled. The installer can
enable test signing, but never changes Secure Boot and never silently weakens
code-integrity settings. Production releases need Microsoft attestation/WHQL
signing (or another project-approved production certificate flow) in place of
the ephemeral certificate.

## Installation behavior

`Install-Helios.ps1` verifies the payload manifest before making changes, then:

- installs the Visual C++ x64 runtime and the prebuilt PnP driver package;
- installs Mesa and CLVK in a versioned directory below `Program Files`;
- installs official x64 and x86 `vulkan-1.dll` loaders and the x64 `OpenCL.dll`
  only when the matching system loader is absent;
- registers Venus and CLVK through the Khronos machine ICD registries; and
- registers the x64 and x86 `libgallium_wgl.dll` files as the Microsoft OpenGL
  ICDs on the Helios display adapter key. It does not replace Windows'
  `opengl32.dll`.

Original OpenGL registry values and every created path/hash are saved in
`C:\ProgramData\Helios\install-state.json`. The package refuses to overwrite an
installation managed by another bundle; uninstall it first so rollback state
cannot be lost.

`Verify-Helios.ps1 -RunSmokeTests` checks hashes and registrations, then creates
a Vulkan instance, creates a D3D11 device on Helios, creates a WGL context, and
compiles/runs an OpenCL kernel. The OpenCL probe validates every output value.

## Application compatibility files

The archive includes the separately deployed DaVinci Resolve ADL shim at
`compatibility\DaVinci Resolve\atiadlxx.dll`. It is not installed system-wide or
copied by `Install-Helios.ps1`. The adjacent installer safely backs up and
places the DLL beside `Resolve.exe`; no special launcher is required.

## Hosted runner requirements

The driver and package jobs require Visual Studio 2022 and the Windows 11 SDK
and WDK. The setup script uses an already installed WDK when available and
otherwise installs the official 10.0.26100 SDK/WDK packages with winget. A
self-hosted runner should preinstall those tools if winget is unavailable.

The bundle supports WoW64 Vulkan and OpenGL using independently built x86 Mesa
and Vulkan-loader binaries. WoW64 Direct3D and OpenCL still require separately
built x86 WDDM UMD/DXVK and CLVK/OpenCL-loader components; copying x64 DLLs into
`SysWOW64` is not a valid substitute.
