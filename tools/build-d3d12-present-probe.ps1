param(
  [string]$PackagingRoot = $env:WB_PACKAGING_ROOT,
    [Parameter(Mandatory)][string]$RepoRoot,
    [Parameter(Mandatory)][string]$OutputRoot,
    [ValidateSet("x86", "x64", "all")][string]$Architecture = "all"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $RepoRoot "ci\windows\Initialize-HeliosBuild.ps1")
. (Join-Path $PackagingRoot "Helios-PackageCommon.ps1")
$source = Join-Path $RepoRoot "tools\d3d12_present_probe.cpp"
if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "Missing probe input: $source" }
New-Item -ItemType Directory -Force -Path $OutputRoot | Out-Null
$OutputRoot = (Resolve-Path -LiteralPath $OutputRoot).Path
$architectures = if ($Architecture -eq "all") { @("x86", "x64") } else { @($Architecture) }
foreach ($targetArchitecture in $architectures) {
    Import-VisualStudioEnvironment -Architecture $targetArchitecture
    $directory = Join-Path $OutputRoot $targetArchitecture
    New-Item -ItemType Directory -Force -Path $directory | Out-Null
    $executable = Join-Path $directory "d3d12-present.exe"
    & cl.exe /nologo /EHsc /std:c++17 /O2 /W4 /WX /MT /DUNICODE /D_UNICODE `
        $source "/Fo:$(Join-Path $directory 'd3d12-present.obj')" "/Fe:$executable" `
        /link user32.lib
    if ($LASTEXITCODE -ne 0) { throw "D3D12 presentation probe $targetArchitecture compilation failed." }
    Assert-HeliosPeArchitecture $executable $targetArchitecture
    # This mode is CPU-only and is safe on build machines with no Helios GPU.
    & $executable --self-test
    if ($LASTEXITCODE -ne 0) { throw "Presentation probe $targetArchitecture CPU self-test failed." }
    Write-Host "Built Windows-runtime presentation probe: $executable"
}
