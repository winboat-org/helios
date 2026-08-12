param(
    [Parameter(Mandatory)][string]$OutputDir,
    [string]$SourceRoot = "C:\clvk-src",
    [string]$BuildRoot = "C:\clvk-build",
    [string]$ClvkRepository = "https://github.com/winboat-org/clvk-helios.git",
    [string]$ClvkCommit = "bcb4f3cfcb6f358fd94dbf71b34aa61262843531",
    # Patches applied to the clspv submodule after checkout. clvk-helios carries
    # clspv as a submodule of upstream google/clspv, so Helios-local compiler
    # fixes live here as patches until there is enough divergence to justify a
    # clspv fork. Applied in file-name order; any failure fails the build rather
    # than silently shipping an unpatched compiler.
    [string]$ClspvPatchDir = (Join-Path $PSScriptRoot "..\patches\clspv")
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if (Test-Path -LiteralPath $SourceRoot) { Remove-Item -LiteralPath $SourceRoot -Recurse -Force }
if (Test-Path -LiteralPath $BuildRoot) { Remove-Item -LiteralPath $BuildRoot -Recurse -Force }

& git clone --filter=blob:none --recursive $ClvkRepository $SourceRoot
if ($LASTEXITCODE -ne 0) { throw "Failed to clone clvk." }
Push-Location $SourceRoot
try {
    & git checkout --detach $ClvkCommit
    if ($LASTEXITCODE -ne 0) { throw "Failed to check out clvk $ClvkCommit." }
    & git submodule update --init --recursive
    if ($LASTEXITCODE -ne 0) { throw "Failed to initialize clvk submodules." }

    if (Test-Path -LiteralPath $ClspvPatchDir -PathType Container) {
        $patches = @(Get-ChildItem -LiteralPath $ClspvPatchDir -Filter "*.patch" -File | Sort-Object Name)
        foreach ($patch in $patches) {
            Write-Host "Applying clspv patch $($patch.Name)"
            Push-Location (Join-Path $SourceRoot "external\clspv")
            try {
                & git apply --verbose $patch.FullName
                if ($LASTEXITCODE -ne 0) { throw "Failed to apply clspv patch $($patch.Name)." }
            } finally {
                Pop-Location
            }
        }
        Write-Host "Applied $($patches.Count) clspv patch(es)."
    }

    & python.exe external/clspv/utils/fetch_sources.py --shallow --deps llvm
    if ($LASTEXITCODE -ne 0) { throw "Failed to fetch clvk LLVM sources." }
} finally {
    Pop-Location
}

& cmake.exe -S $SourceRoot -B $BuildRoot -G Ninja `
    -DCMAKE_BUILD_TYPE=Release `
    -DCMAKE_C_COMPILER_LAUNCHER=sccache `
    -DCMAKE_CXX_COMPILER_LAUNCHER=sccache `
    -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded `
    -DCLVK_CLSPV_ONLINE_COMPILER=ON `
    -DCLVK_COMPILER_AVAILABLE=ON `
    -DCLVK_VULKAN_IMPLEMENTATION=system `
    -DCLVK_BUILD_TESTS=OFF `
    -DCLVK_UNIT_TESTING=OFF `
    -DCLVK_ENABLE_ASSERTIONS=OFF
if ($LASTEXITCODE -ne 0) { throw "clvk CMake configure failed." }

& cmake.exe --build $BuildRoot --parallel
if ($LASTEXITCODE -ne 0) { throw "clvk build failed." }

$vendorDll = Get-ChildItem -LiteralPath $BuildRoot -Filter "OpenCL.dll" -File -Recurse |
    Where-Object { $_.FullName -match "Release" } |
    Select-Object -First 1
if (-not $vendorDll) {
    $vendorDll = Get-ChildItem -LiteralPath $BuildRoot -Filter "OpenCL.dll" -File -Recurse | Select-Object -First 1
}
if (-not $vendorDll) { throw "clvk build completed but OpenCL.dll was not found." }

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
Copy-Item -LiteralPath $vendorDll.FullName -Destination (Join-Path $OutputDir "clvk.dll") -Force
$vendorPdb = [IO.Path]::ChangeExtension($vendorDll.FullName, ".pdb")
if (Test-Path -LiteralPath $vendorPdb -PathType Leaf) {
    Copy-Item -LiteralPath $vendorPdb -Destination (Join-Path $OutputDir "clvk.pdb") -Force
}
Set-Content -LiteralPath (Join-Path $OutputDir "clvk-commit.txt") -Value $ClvkCommit -Encoding ascii
Set-Content -LiteralPath (Join-Path $OutputDir "clvk-repository.txt") -Value $ClvkRepository -Encoding ascii
# Record which compiler patches this artifact was built with, so a deployed
# clvk.dll can be told apart from a stock one without disassembling it.
$appliedPatches = if (Test-Path -LiteralPath $ClspvPatchDir -PathType Container) {
    @(Get-ChildItem -LiteralPath $ClspvPatchDir -Filter "*.patch" -File | Sort-Object Name | ForEach-Object { $_.Name })
} else { @() }
Set-Content -LiteralPath (Join-Path $OutputDir "clspv-patches.txt") `
    -Value ($appliedPatches -join "`n") -Encoding ascii
$licenseRoot = Join-Path $OutputDir "licenses"
$licenses = [ordered]@{
    "clvk-LICENSE" = Join-Path $SourceRoot "LICENSE"
    "clspv-LICENSE" = Join-Path $SourceRoot "external\clspv\LICENSE"
    "SPIRV-Tools-LICENSE" = Join-Path $SourceRoot "external\SPIRV-Tools\LICENSE"
    "SPIRV-Headers-LICENSE" = Join-Path $SourceRoot "external\SPIRV-Headers\LICENSE"
    "OpenCL-Headers-LICENSE" = Join-Path $SourceRoot "external\OpenCL-Headers\LICENSE"
}
New-Item -ItemType Directory -Force -Path $licenseRoot | Out-Null
foreach ($entry in $licenses.GetEnumerator()) {
    if (Test-Path -LiteralPath $entry.Value -PathType Leaf) {
        Copy-Item -LiteralPath $entry.Value -Destination (Join-Path $licenseRoot $entry.Key) -Force
    }
}
$llvmLicense = Get-ChildItem -LiteralPath (Join-Path $SourceRoot "external\clspv") -Filter "LICENSE.TXT" -File -Recurse | Select-Object -First 1
if ($llvmLicense) { Copy-Item -LiteralPath $llvmLicense.FullName -Destination (Join-Path $licenseRoot "LLVM-LICENSE.TXT") -Force }
Write-Host "CLVK artifact staged at $OutputDir"
