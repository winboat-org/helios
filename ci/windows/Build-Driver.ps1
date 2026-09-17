param(
    [Parameter(Mandatory)][string]$RepoRoot,
    [Parameter(Mandatory)][string]$OutputDir,
    [ValidateSet("Debug", "Release")][string]$Configuration = "Release",
    [string]$BuildRoot = "C:\helios-build"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Initialize-HeliosBuild.ps1")

# cargo's debug profile is named `dev` but writes to target/debug; the two names
# are kept apart deliberately so neither is hardcoded downstream.
$cargoMakeProfile = if ($Configuration -eq "Debug") { "dev" } else { "release" }
$profileDir = if ($Configuration -eq "Debug") { "debug" } else { "release" }
$mesonBuildType = if ($Configuration -eq "Debug") { "debug" } else { "release" }

$RepoRoot = (Resolve-Path -LiteralPath $RepoRoot).Path
# Reject stale checked-in INF/Cargo descriptions before starting engine builds.
& python (Join-Path $RepoRoot "tools\sync-metadata.py") --check
if ($LASTEXITCODE -ne 0) { throw "Metadata is stale; run tools/sync-metadata.py." }
Import-VisualStudioEnvironment
$clangCl = Assert-Command "clang-cl.exe"
$llvmLib = Assert-Command "llvm-lib.exe"
$llvmReadObj = Assert-Command "llvm-readobj.exe"
Assert-Command "meson.exe" | Out-Null
Assert-Command "ninja.exe" | Out-Null
Assert-Command "cargo.exe" | Out-Null
Assert-Command "cargo-make.exe" | Out-Null
Assert-Command "widl.exe" | Out-Null
Assert-Command "glslangValidator.exe" | Out-Null

$stampInf = Find-WindowsKitTool "stampinf.exe"
$inf2Cat = Find-WindowsKitTool "Inf2Cat.exe"
$kitBin = Split-Path -Parent $stampInf
$env:PATH = "$kitBin;$env:PATH"
$env:LIBCLANG_PATH = Split-Path -Parent $clangCl
$env:CC = $clangCl
$env:CXX = $clangCl
# Keep rustup's per-directory `nightly` override from bypassing the CI pin.
if ($env:RUST_TOOLCHAIN) { $env:RUSTUP_TOOLCHAIN = $env:RUST_TOOLCHAIN }

$dxvkSource = Join-Path $RepoRoot "dxvk-helios"
$vkd3dSource = Join-Path $RepoRoot "vkd3d-proton-helios"
$compatHeader = Join-Path $RepoRoot "umd\build-support\dxvk_c_compat.h"
New-Item -ItemType Directory -Force -Path $BuildRoot | Out-Null
$engineBuilds = @{}
foreach ($architecture in @("x64", "x86")) {
    Import-VisualStudioEnvironment -Architecture $architecture
    $env:PATH = "$env:LIBCLANG_PATH;$kitBin;$env:PATH"
    $nativeName = if ($architecture -eq "x86") { "clang-cl-x86-native.ini" } else { "clang-cl-native.ini" }
    $nativeFile = Join-Path $RepoRoot "ci\windows\$nativeName"
    $dxvkBuild = Join-Path $BuildRoot "$Configuration\dxvk-$architecture"
    $vkd3dBuild = Join-Path $BuildRoot "$Configuration\vkd3d-$architecture"
    foreach ($directory in @($dxvkBuild, $vkd3dBuild)) {
        if (Test-Path -LiteralPath $directory) { Remove-Item -LiteralPath $directory -Recurse -Force }
    }
    $dxvkCppArgs = @(
        "/D_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH"
        "-Wno-deprecated-declarations"
        "-Wno-delete-non-abstract-non-virtual-dtor"
        "-Wno-unused-private-field"
        "-Wno-unused-lambda-capture"
        "-Wno-c++20-extensions"
        "-Wno-unused-const-variable"
    ) -join " "
    & meson.exe setup $dxvkBuild $dxvkSource `
        --native-file $nativeFile --buildtype $mesonBuildType -Db_vscrt=mt `
        "-Dcpp_args=$dxvkCppArgs" "-Dc_args=/FI$compatHeader" `
        -Denable_d3d8=false -Denable_d3d9=false -Denable_d3d10=false `
        -Denable_d3d11=true -Denable_dxgi=true
    if ($LASTEXITCODE -ne 0) { throw "DXVK $architecture meson setup failed with exit code $LASTEXITCODE." }
    & meson.exe compile -C $dxvkBuild
    if ($LASTEXITCODE -ne 0) { throw "DXVK $architecture build failed with exit code $LASTEXITCODE." }

    # Static CRT for BOTH engines, so no VC++ redistributable is needed on the
    # target: DXVK and vkd3d are /MT, and the umd/umd12 crates are crt-static.
    # clang-cl uses the MSVC ABI; MinGW archives cannot be linked here.
    & meson.exe setup $vkd3dBuild $vkd3dSource `
        --native-file $nativeFile --buildtype $mesonBuildType -Db_vscrt=mt `
        -Denable_tests=false "-Dc_args=-Wno-error=incompatible-pointer-types"
    if ($LASTEXITCODE -ne 0) { throw "vkd3d $architecture meson setup failed with exit code $LASTEXITCODE." }
    & meson.exe compile -C $vkd3dBuild helios_d3d12_static
    if ($LASTEXITCODE -ne 0) { throw "vkd3d $architecture static engine build failed with exit code $LASTEXITCODE." }
    $engineBuilds[$architecture] = @{ dxvk = $dxvkBuild; vkd3d = $vkd3dBuild }
}
# The kernel remains native AMD64. cargo-make builds the two user-mode targets
# separately and remaps the *_X86 engine paths only for its i686 child builds.
Import-VisualStudioEnvironment -Architecture x64
$env:PATH = "$env:LIBCLANG_PATH;$kitBin;$env:PATH"
$dxvkBuild = $engineBuilds.x64.dxvk
$vkd3dBuild = $engineBuilds.x64.vkd3d
$env:HELIOS_DXVK_BUILD_X86 = $engineBuilds.x86.dxvk
$env:HELIOS_VKD3D_BUILD_X86 = $engineBuilds.x86.vkd3d
& rustup.exe target add i686-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { throw "Could not install the i686 Rust standard library." }

$env:HELIOS_DXVK_SRC = $dxvkSource
$env:HELIOS_DXVK_BUILD = $dxvkBuild
$env:HELIOS_VKD3D_BUILD = $vkd3dBuild
$env:HELIOS_CLANG_CL = $clangCl
$env:HELIOS_MSVC_LIB = $llvmLib
$env:HELIOS_WDK_INCLUDE = Find-WindowsKitInclude
$env:HELIOS_MSVC_INCLUDE = Join-Path $env:VCToolsInstallDir "include"

# rust-script repeats cargo-make's 64-character generated script names in its
# target paths. The normal runner profile makes those paths exceed link.exe's
# legacy MAX_PATH limit. wdk-build also force-installs its own rust-script
# version during the build, so an executable wrapper is not durable. Redirect
# the per-user cache root for child processes instead, then restore the shell
# folder immediately after cargo-make exits.
$shellFoldersKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders"
$localAppDataName = "Local AppData"
$previousLocalAppData = Get-ItemPropertyValue -LiteralPath $shellFoldersKey -Name $localAppDataName
$previousLocalAppDataEnvironment = $env:LOCALAPPDATA
$shortLocalAppData = "C:\la"
New-Item -ItemType Directory -Force -Path $shortLocalAppData | Out-Null
Set-ItemProperty -LiteralPath $shellFoldersKey -Name $localAppDataName -Value $shortLocalAppData
$env:LOCALAPPDATA = $shortLocalAppData

$kmdRoot = Join-Path $RepoRoot "kmd_render"
$previousCargoTargetDir = $env:CARGO_TARGET_DIR
$env:CARGO_TARGET_DIR = Join-Path $kmdRoot "target"
Push-Location $kmdRoot
try {
    $rustcVersion = (& rustc.exe --version) -join "`n"
    $cargoVersion = (& cargo.exe --version) -join "`n"
    & cargo.exe make --profile $cargoMakeProfile --makefile Cargo.make.toml
    if ($LASTEXITCODE -ne 0) { throw "Helios driver build failed with exit code $LASTEXITCODE." }
} finally {
    Pop-Location
    Set-ItemProperty -LiteralPath $shellFoldersKey -Name $localAppDataName -Value $previousLocalAppData
    $env:LOCALAPPDATA = $previousLocalAppDataEnvironment
    $env:CARGO_TARGET_DIR = $previousCargoTargetDir
}

$package = Join-Path $kmdRoot "target\$profileDir\helios_kmd_render_package"
$required = @("helios_kmd_render.inf", "helios_kmd_render.sys", "helios_umd.dll", "helios_umd12.dll", "helios_umd32.dll", "helios_umd12_32.dll")
foreach ($name in $required) {
    if (-not (Test-Path -LiteralPath (Join-Path $package $name) -PathType Leaf)) {
        throw "Driver package output is missing $name in $package."
    }
}

# A display UMD is loaded into arbitrary application processes. A dynamic MSVC
# or UCRT dependency binds through that application's DLL search order; CapCut,
# for example, supplies MSVCP140 14.28 to a UMD compiled against the 14.44 STL,
# which leaves driver-internal std::mutex objects ABI-incompatible and crashes
# its GPU process. Keep the shipped UMD self-contained and make CRT regressions
# a packaging failure rather than an application-specific runtime failure.
foreach ($architecture in @("x64", "x86")) {
    $d3d11Name = if ($architecture -eq "x86") { "helios_umd32.dll" } else { "helios_umd.dll" }
    $d3d12Name = if ($architecture -eq "x86") { "helios_umd12_32.dll" } else { "helios_umd12.dll" }
    $machine = if ($architecture -eq "x86") { "IMAGE_FILE_MACHINE_I386" } else { "IMAGE_FILE_MACHINE_AMD64" }
    foreach ($name in @($d3d11Name, $d3d12Name)) {
        $headers = @(& $llvmReadObj --file-headers (Join-Path $package $name) 2>&1)
        if ($LASTEXITCODE -ne 0 -or -not ($headers -match "Machine: $machine\b")) {
            throw "$name is not a $architecture PE image."
        }
        $exports = @(& $llvmReadObj --coff-exports (Join-Path $package $name) 2>&1)
        if ($LASTEXITCODE -ne 0) { throw "Could not inspect exports in $name." }
        $entrypoints = if ($name -eq $d3d11Name) { @("OpenAdapter10", "OpenAdapter10_2") } else { @("OpenAdapter12") }
        foreach ($entrypoint in $entrypoints) {
            if (-not ($exports -match "^\s*Name: $entrypoint\s*$")) {
                throw "$name does not export the required undecorated $entrypoint entry point."
            }
        }
    }
    foreach ($name in @($d3d11Name, $d3d12Name)) {
        $imports = @(& $llvmReadObj --coff-imports (Join-Path $package $name) 2>&1)
        if ($LASTEXITCODE -ne 0) { throw "Failed to inspect $name imports." }
        $dynamicCrt = @($imports | Where-Object {
            $_ -match '(?i)(MSVCP\d+|VCRUNTIME\d+(?:_\d+)?|MSVCR\d+|UCRTBASE|api-ms-win-crt-[^\s]+)\.dll'
        })
        if ($dynamicCrt.Count -ne 0) {
            throw "$name imports a dynamic CRT; the installer must not need the VC++ redistributables: $($dynamicCrt -join '; ')"
        }
    }
    $umd12Imports = @(& $llvmReadObj --coff-imports (Join-Path $package $d3d12Name) 2>&1)
    if ($LASTEXITCODE -ne 0) { throw "Failed to inspect $d3d12Name imports." }
    if ($umd12Imports -match '(?i)^\s*Name: (dxgi|d3d12|d3d12core|helios_vkd3d)\.dll\s*$') {
        throw "$d3d12Name imports a DXGI/D3D12 runtime instead of embedding its engine."
    }
}

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
Copy-Item -Path (Join-Path $package "*") -Destination $OutputDir -Recurse -Force

foreach ($crate in @("umd", "umd12")) {
    $stem = if ($crate -eq "umd") { "helios_umd" } else { "helios_umd12" }
    foreach ($architecture in @("x64", "x86")) {
        $targetProfile = if ($architecture -eq "x86") { "target\i686-pc-windows-msvc\$profileDir" } else { "target\$profileDir" }
        $source = Join-Path $RepoRoot "$crate\$targetProfile\$stem.pdb"
        $stagedStem = if ($architecture -eq "x86") {
            if ($crate -eq "umd") { "helios_umd32" } else { "helios_umd12_32" }
        } else { $stem }
        if (Test-Path -LiteralPath $source -PathType Leaf) {
            Copy-Item -LiteralPath $source -Destination (Join-Path $OutputDir "$stagedStem.pdb") -Force
        }
    }
}
New-Item -ItemType Directory -Force -Path (Join-Path $OutputDir "licenses\dxvk") | Out-Null
Copy-Item -LiteralPath (Join-Path $dxvkSource "LICENSE") -Destination (Join-Path $OutputDir "licenses\dxvk\LICENSE") -Force
foreach ($license in @(
    "LICENSE", "COPYING",
    "khronos\SPIRV-Headers\LICENSE", "khronos\Vulkan-Headers\LICENSE.md",
    "subprojects\dxil-spirv\LICENSE.MIT",
    "subprojects\dxil-spirv\third_party\spirv-headers\LICENSE",
    "subprojects\dxil-spirv\third_party\SPIRV-Tools\LICENSE",
    "subprojects\dxil-spirv\third_party\SPIRV-Cross\LICENSE",
    "subprojects\dxil-spirv\subprojects\dxbc-spirv\LICENSE",
    "subprojects\dxil-spirv\subprojects\dxbc-spirv\submodules\spirv_headers\LICENSE"
)) {
    $destination = Join-Path $OutputDir "licenses\vkd3d\$license"
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $destination) | Out-Null
    Copy-Item -LiteralPath (Join-Path $vkd3dSource $license) -Destination $destination -Force
}

# Record the selected tools, rather than treating the workflow pins as proof
# of which compiler Meson or rustup actually used.
$toolchain = [ordered]@{
    clang = (& $clangCl --version) -join "`n"
    libclang = (Get-Item (Join-Path $env:LIBCLANG_PATH "libclang.dll")).VersionInfo.FileVersion
    rustc = $rustcVersion
    cargo = $cargoVersion
    meson = (& meson.exe --version) -join "`n"
    ninja = (& ninja.exe --version) -join "`n"
    widl = (& widl.exe -V) -join "`n"
    glslang = (& glslangValidator.exe --version) -join "`n"
    msvc = $env:VCToolsVersion
    wdkInclude = $env:HELIOS_WDK_INCLUDE
    dxvk = Get-Content (Join-Path $dxvkBuild "meson-info\intro-compilers.json") -Raw | ConvertFrom-Json
    vkd3d = Get-Content (Join-Path $vkd3dBuild "meson-info\intro-compilers.json") -Raw | ConvertFrom-Json
    dxvkX86 = Get-Content (Join-Path $engineBuilds.x86.dxvk "meson-info\intro-compilers.json") -Raw | ConvertFrom-Json
    vkd3dX86 = Get-Content (Join-Path $engineBuilds.x86.vkd3d "meson-info\intro-compilers.json") -Raw | ConvertFrom-Json
}
$toolchain | ConvertTo-Json -Depth 10 | Set-Content (Join-Path $OutputDir "toolchain.json") -Encoding UTF8
Set-Content -LiteralPath (Join-Path $OutputDir "configuration.txt") -Value $Configuration -Encoding ascii

Write-Host "Driver artifact ($Configuration) staged at $OutputDir"
