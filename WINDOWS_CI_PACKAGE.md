# Windows CI package

The `Windows graphics and compute bundle` GitHub Actions workflow builds one
x64 Windows archive that turns a clean Helios Windows 11 guest into a
system-wide graphics/compute installation. It includes x86 Direct3D 11/12 and Vulkan/OpenGL
components for WoW64 applications alongside the native x64 stack.

The 2026-09-09 native-DGC source requires the paired renderer/protocol fork
described in [NATIVE_DGC.md](docs/dx12/NATIVE_DGC.md) for its state-changing
indirect path. This Windows bundle does not install the Linux renderer. Mesa's
generated driver headers must match that protocol; regenerate them with
`tools/build-native-renderer.sh` before packaging source changes. Existing
LLVM/libclang22.1.8, VulkanSDK1.4.350.0 and bindgen0.72 pins remain. The new local
builds are not hosted-CI or native guest acceptance evidence.

## What the workflow builds

The jobs are independent so an error points at the actual component:

1. `driver` builds the DXVK and vkd3d-proton static cores, embeds them in
   `helios_umd.dll` (D3D11) and `helios_umd12.dll` (D3D12) for AMD64, and
   `helios_umd32.dll` / `helios_umd12_32.dll` for WoW64. It builds/packages the
   AMD64 Rust WDDM kernel driver. All four UMDs are required package inputs.
2. `mesa` and `mesa_x86` build the pinned Mesa submodule for x64 and x86 with
   both the Venus Vulkan ICD and the Zink WGL OpenGL ICD enabled.
3. `opencl` builds pinned CLVK with the clspv online compiler embedded. End-user
   machines therefore do not need `clspv.exe` or `CLVK_CLSPV_PATH`.
4. `loaders` builds the official x64 Vulkan/OpenCL loaders, the x86 Vulkan
   loader, and architecture-matched smoke probes.
5. `compatibility` builds and validates the app-local DaVinci Resolve ADL shim.
6. `package` test-signs the final driver package and compatibility shim, hashes
   every distributed binary, builds the Rust installer for the configuration,
   embeds the whole payload into a single self-contained `HeliosSetup.exe`, and
   creates `helios-windows-x64-<version>-<commit>[-debug].zip` containing that
   exe and its `README.md`. Debug symbols (`.pdb`/`.map`) are never embedded;
   they are published separately as `<package>-symbols.zip`.

The workflow runs for pull requests and pushes to `master`, and can be started
manually. A tag beginning with `v` also publishes the zip and its SHA-256 file
as a GitHub Release.

## Reproducibility and source pins

The Helios, Mesa, DXVK, and vkd3d-proton revisions come from the checked-out commit and its
gitlinks. The Windows OpenCL build uses the `winboat-org/clvk-helios` fork for
guest DXGI/OpenCL device association. Its repository and commit, along with the
Vulkan-Loader, Vulkan-Headers, and OpenCL-ICD-Loader commits, are pinned in
`.github/workflows/windows-stack.yml`. Toolchain versions are pinned there as
well. Every resulting source revision is written to `manifest.json`.

When updating an external pin, first build and run the packaged probes in
the VM. In particular, CLVK and Zink are consumers of the Venus ICD and can
expose synchronization/protocol mismatches that a successful compile cannot.

## Signing model

CI creates a unique, non-exportable test-signing key for each bundle. It signs
the SYS and all four UMDs before creating the catalog, signs the final catalog, exports
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

- installs the prebuilt PnP driver package (the UMDs are static-CRT, so no Visual C++ runtime is needed);
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
Vulkan instances, D3D11 and D3D12 devices on Helios, and WGL contexts in both
x64 and x86 processes. Direct3D probes also clear/copy/read back textures in
both architectures. It compiles/runs an x64 OpenCL kernel. The OpenCL probe validates every output value. Run graphics probes in the
logged-in desktop session or an interactive scheduled task; session 0 is refused.
D3D11 requires feature level 11.0 and verifies every pixel of a 31x17 readback.
D3D12 runs the existing clear/readback probe with `--expect ok`; neither check
establishes presentation correctness or full conformance.

Provider verification reads the bound adapter's `DEVPKEY_Device_DriverProvider`,
falling back to CIM only when that PnP property is unavailable. CIM can omit its
provider even on a healthy activated adapter. Before reboot, `-AllowPendingReboot`
defers provider metadata missing from both sources only
while the PnP device is not `OK`; a reported wrong provider still fails. Normal
verification after reboot requires both healthy PnP status and the expected
provider from either source.

## Application compatibility files

The archive includes the separately deployed DaVinci Resolve ADL shim at
`compatibility\DaVinci Resolve\atiadlxx.dll`. It is not installed system-wide or
copied by `Install-Helios.ps1`. The adjacent installer safely backs up and
places the DLL beside `Resolve.exe`; no special launcher is required.

D3D12 is enabled when `HKLM\SOFTWARE\Helios!UmdD3D12` is absent. Explicit
DWORD `0` disables it, and the installer preserves that override. The D3D12
smoke then expects device creation to fail. Deleting the value restores the
enabled default. Resource ownership and failure-path limits remain documented
in [EXECUTION_SYNC.md](docs/dx12/EXECUTION_SYNC.md) and
[HPS2_REFACTOR.md](docs/HPS2_REFACTOR.md); the default change does not close them.

## Hosted runner requirements

The driver and OpenCL jobs use `ci/windows/Install-VulkanSdk.ps1` to run the
official LunarG installer in unattended copy-only mode. The complete versioned
SDK directory is cached. Both jobs validate the Vulkan header, x64 loader
import library, and shader compiler on cache hits as well as fresh installs.
Extracting the installer with 7-Zip is insufficient for SDK 1.4.350.0: it can
produce working shader tools while omitting the development files CLVK needs.
`Build-OpenCL.ps1` checks those files before deleting build trees or fetching LLVM.

The driver and package jobs require Visual Studio 2022 and the Windows 11 SDK
and WDK. The setup script uses an already installed WDK when available and
otherwise installs the official 10.0.26100 SDK/WDK packages with winget. A
self-hosted runner should preinstall those tools if winget is unavailable.

The bundle builds native x86 Direct3D UMDs alongside the independent x86 Mesa
and Vulkan-loader binaries. `UserModeDriverNameWoW` registers the x86 UMDs in
API slots 0–2 (D3D11) and 3 (D3D12), using distinct DriverStore filenames.
`InstalledDisplayDrivers` lists all four UMDs. PnP installs/removes both
architectures together, including rollback to the previous complete package.
OpenCL remains x64-only.

The driver job installs native `widl` through MSYS2's
`mingw-w64-ucrt-x86_64-tools` package and initializes vkd3d's recursive submodules.
It builds only `helios_d3d12_static`; no app-local `d3d12.dll`, `d3d12core.dll`,
or `helios_vkd3d.dll` is shipped. The build verifies PE machine types and undecorated `OpenAdapter10`,
`OpenAdapter10_2`, and `OpenAdapter12` exports, rejects DXGI/D3D12 runtime imports
in both D3D12 UMDs, and rejects dynamic CRT imports in ALL FOUR UMDs. DXVK and vkd3d are both
built `/MT` and the UMD crates are `crt-static`, so no Visual C++ runtime ships.
Engine licenses, optional UMD PDBs, vkd3d source provenance, and the actual driver
build tool versions (`payload/driver/toolchain.json`) travel with the package.

The VM comparison on 2026-09-07 found LLVM/clang-cl/libclang **22.1.8** in both
active engine builds and Vulkan SDK **1.4.350.0** (glslang **16.2.0**). CI now
pins those versions instead of LLVM 17.0.6 / SDK 1.4.309.0. VM Meson is 1.11.1,
Python 3.12.10, widl 11.5, and cargo-make 0.37.24; CI retains Meson 1.11.2,
Python 3.12, and cargo-make 0.37.24, and records the resolved widl version.
The VM has both VS 2022 and VS 18 and several SDKs; CI uses its Windows 2022
runner's installed MSVC/WDK. `toolchain.json` records their selected versions.
The VM's nightly is dated 2026-06-03 and its default Rust is 1.96.0; CI retains
its explicit nightly-2026-07-14 pin and applies it to cargo-make and both UMDs.

The local build requires `HELIOS_DXVK_BUILD_X86` and `HELIOS_VKD3D_BUILD_X86`
for `cargo make` packaging; `Build-Driver.ps1` sets these after building the
engines with the x86 Visual Studio environment and
`ci/windows/clang-cl-x86-native.ini`. x86 Cargo outputs live under each crate's
`target/i686-pc-windows-msvc/<profile>` and are renamed only when staged.
Verifier checks installed image hashes against the bundle, PE architectures,
and both four-slot registrations. The shared VC runtimes remain installed on
uninstall, as before.
