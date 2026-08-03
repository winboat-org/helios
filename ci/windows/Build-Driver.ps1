param(
    [Parameter(Mandatory)][string]$RepoRoot,
    [Parameter(Mandatory)][string]$OutputDir,
    [string]$BuildRoot = "C:\helios-build"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Initialize-HeliosBuild.ps1")

$RepoRoot = (Resolve-Path -LiteralPath $RepoRoot).Path
Import-VisualStudioEnvironment
$clangCl = Assert-Command "clang-cl.exe"
$llvmLib = Assert-Command "llvm-lib.exe"
Assert-Command "meson.exe" | Out-Null
Assert-Command "ninja.exe" | Out-Null
Assert-Command "cargo.exe" | Out-Null
Assert-Command "cargo-make.exe" | Out-Null

$stampInf = Find-WindowsKitTool "stampinf.exe"
$inf2Cat = Find-WindowsKitTool "Inf2Cat.exe"
$kitBin = Split-Path -Parent $stampInf
$env:PATH = "$kitBin;$env:PATH"
$env:LIBCLANG_PATH = Split-Path -Parent $clangCl

$dxvkSource = Join-Path $RepoRoot "dxvk-helios"
$dxvkBuild = Join-Path $BuildRoot "dxvk"
$nativeFile = Join-Path $RepoRoot "ci\windows\clang-cl-native.ini"
$compatHeader = Join-Path $RepoRoot "umd\build-support\dxvk_c_compat.h"
New-Item -ItemType Directory -Force -Path $BuildRoot | Out-Null

if (Test-Path -LiteralPath $dxvkBuild) {
    Remove-Item -LiteralPath $dxvkBuild -Recurse -Force
}

& meson.exe setup $dxvkBuild $dxvkSource `
    --native-file $nativeFile `
    --buildtype release `
    -Db_vscrt=md `
    -Dcpp_args=/D_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH `
    "-Dc_args=/FI$compatHeader" `
    -Denable_d3d8=false `
    -Denable_d3d9=false `
    -Denable_d3d10=false `
    -Denable_d3d11=true `
    -Denable_dxgi=true
if ($LASTEXITCODE -ne 0) { throw "DXVK meson setup failed with exit code $LASTEXITCODE." }

& meson.exe compile -C $dxvkBuild
if ($LASTEXITCODE -ne 0) { throw "DXVK build failed with exit code $LASTEXITCODE." }

$env:HELIOS_DXVK_SRC = $dxvkSource
$env:HELIOS_DXVK_BUILD = $dxvkBuild
$env:HELIOS_CLANG_CL = $clangCl
$env:HELIOS_MSVC_LIB = $llvmLib
$env:HELIOS_WDK_INCLUDE = Find-WindowsKitInclude
$env:HELIOS_MSVC_INCLUDE = Join-Path $env:VCToolsInstallDir "include"
$env:RUST_SCRIPT_CACHE_DIR = "C:\rs"
New-Item -ItemType Directory -Force -Path $env:RUST_SCRIPT_CACHE_DIR | Out-Null

$kmdRoot = Join-Path $RepoRoot "kmd_render"
Push-Location $kmdRoot
try {
    & cargo.exe make --profile release --makefile Cargo.make.toml
    if ($LASTEXITCODE -ne 0) { throw "Helios driver build failed with exit code $LASTEXITCODE." }
} finally {
    Pop-Location
}

$package = Join-Path $kmdRoot "target\release\helios_kmd_render_package"
$required = @("helios_kmd_render.inf", "helios_kmd_render.sys", "helios_umd.dll")
foreach ($name in $required) {
    if (-not (Test-Path -LiteralPath (Join-Path $package $name) -PathType Leaf)) {
        throw "Driver package output is missing $name in $package."
    }
}

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
Copy-Item -Path (Join-Path $package "*") -Destination $OutputDir -Recurse -Force

$umdPdb = Join-Path $RepoRoot "umd\target\release\helios_umd.pdb"
if (Test-Path -LiteralPath $umdPdb -PathType Leaf) {
    Copy-Item -LiteralPath $umdPdb -Destination $OutputDir -Force
}
New-Item -ItemType Directory -Force -Path (Join-Path $OutputDir "licenses\dxvk") | Out-Null
Copy-Item -LiteralPath (Join-Path $dxvkSource "LICENSE") -Destination (Join-Path $OutputDir "licenses\dxvk\LICENSE") -Force

Write-Host "Driver artifact staged at $OutputDir"
