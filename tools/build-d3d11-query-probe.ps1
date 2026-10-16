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
$source = Join-Path $RepoRoot "tools\d3d11_query_probe.cpp"
if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "Missing probe input: $source" }
New-Item -ItemType Directory -Force -Path $OutputRoot | Out-Null
$OutputRoot = (Resolve-Path -LiteralPath $OutputRoot).Path
$architectures = if ($Architecture -eq "all") { @("x86", "x64") } else { @($Architecture) }
foreach ($targetArchitecture in $architectures) {
    Import-VisualStudioEnvironment -Architecture $targetArchitecture
    $directory = Join-Path $OutputRoot $targetArchitecture
    New-Item -ItemType Directory -Force -Path $directory | Out-Null
    $executable = Join-Path $directory "d3d11-query.exe"
    & cl.exe /nologo /EHsc /std:c++17 /O2 /W4 /WX /MT $source `
        "/Fo:$(Join-Path $directory 'd3d11-query.obj')" "/Fe:$executable"
    if ($LASTEXITCODE -ne 0) { throw "D3D11 query probe $targetArchitecture compilation failed." }
    Assert-HeliosPeArchitecture $executable $targetArchitecture
    Write-Host "Built native query probe (GPU execution intentionally separate): $executable"
}
