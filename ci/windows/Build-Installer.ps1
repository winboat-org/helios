param(
    [Parameter(Mandatory)][string]$RepoRoot,
    [Parameter(Mandatory)][string]$OutputDir,
    [ValidateSet("Debug", "Release")][string]$Configuration = "Release"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Initialize-HeliosBuild.ps1")

$RepoRoot = (Resolve-Path -LiteralPath $RepoRoot).Path
Import-VisualStudioEnvironment -Architecture x64
# Keep the selected LLVM toolchain first after VsDevCmd rewrites PATH.
if ($env:LIBCLANG_PATH) { $env:PATH = "$env:LIBCLANG_PATH;$env:PATH" }
$cargo = Assert-Command "cargo.exe"
$llvmReadObj = Assert-Command "llvm-readobj.exe"

$crateRoot = Join-Path $RepoRoot "installer"
if (-not (Test-Path -LiteralPath (Join-Path $crateRoot "Cargo.toml") -PathType Leaf)) {
    throw "installer\Cargo.toml was not found below $RepoRoot."
}
$profileDir = if ($Configuration -eq "Debug") { "debug" } else { "release" }
$targetDir = Join-Path $crateRoot "target"

# rustup's per-directory override must not bypass the CI toolchain pin.
if ($env:RUST_TOOLCHAIN) { $env:RUSTUP_TOOLCHAIN = $env:RUST_TOOLCHAIN }
# A display-driver installer must not depend on the VC++ runtime; static CRT.
if ($env:RUSTFLAGS) {
    $env:RUSTFLAGS = "-C target-feature=+crt-static $env:RUSTFLAGS"
} else {
    $env:RUSTFLAGS = "-C target-feature=+crt-static"
}

Push-Location $crateRoot
try {
    $arguments = @("build", "--locked", "--target-dir", $targetDir)
    if ($Configuration -eq "Release") { $arguments += "--release" }
    & $cargo @arguments
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE." }
} finally {
    Pop-Location
}

$exe = Join-Path $targetDir "$profileDir\HeliosSetup.exe"
if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
    throw "The installer executable was not produced at $exe."
}

# Reject a dynamic CRT or a console subsystem in the shipped binary: the whole
# point of /MT and windows_subsystem is that a bare double-click works even
# before any graphics driver exists.
$imports = @(& $llvmReadObj --coff-imports $exe 2>&1)
if ($LASTEXITCODE -ne 0) { throw "Could not inspect imports in $exe." }
$dynamic = @($imports | Where-Object {
    $_ -match '(?i)(MSVCP\d+|VCRUNTIME\d+(?:_\d+)?|MSVCR\d+|UCRTBASE|api-ms-win-crt-[^\s]+)\.dll'
})
if ($dynamic.Count -ne 0) {
    throw "HeliosSetup.exe imports a dynamic CRT: $($dynamic -join '; ')"
}
$headers = @(& $llvmReadObj --file-headers $exe 2>&1)
if (-not ($headers -match "Subsystem: IMAGE_SUBSYSTEM_WINDOWS_GUI")) {
    throw "HeliosSetup.exe is not a GUI-subsystem executable."
}

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$outputDir = (Resolve-Path -LiteralPath $OutputDir).Path
Copy-Item -LiteralPath $exe -Destination $outputDir -Force
$pdb = [IO.Path]::ChangeExtension($exe, ".pdb")
if (Test-Path -LiteralPath $pdb -PathType Leaf) {
    Copy-Item -LiteralPath $pdb -Destination $outputDir -Force
}
Set-Content -LiteralPath (Join-Path $outputDir "configuration.txt") -Value $Configuration -Encoding ascii

Write-Host "Helios installer skeleton ($Configuration) staged at $outputDir\HeliosSetup.exe"
