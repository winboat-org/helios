param(
  [string]$PackagingRoot = $env:WB_PACKAGING_ROOT,
    [Parameter(Mandatory)][string]$RepoRoot,
    [Parameter(Mandatory)][string]$OutputRoot,
    [Parameter(Mandatory)][string]$Dxc
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $RepoRoot "ci\windows\Initialize-HeliosBuild.ps1")
. (Join-Path $PackagingRoot "Helios-PackageCommon.ps1")
foreach ($path in @($Dxc, (Join-Path $RepoRoot "tools\d3d12_bridge_probe.hlsl"))) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing probe input: $path" }
}
New-Item -ItemType Directory -Force -Path $OutputRoot | Out-Null
$OutputRoot = (Resolve-Path -LiteralPath $OutputRoot).Path
$generated = Join-Path $OutputRoot "draw-shaders"
New-Item -ItemType Directory -Force -Path $generated | Out-Null
foreach ($stage in @("vs", "ps")) {
    & $Dxc -T "${stage}_6_0" -E "${stage}_main" -Vn "g_${stage}_main" `
        -Fh (Join-Path $generated "d3d12_bridge_probe_$stage.h") `
        -Fo (Join-Path $generated "d3d12_bridge_probe_$stage.dxil") `
        (Join-Path $RepoRoot "tools\d3d12_bridge_probe.hlsl")
    if ($LASTEXITCODE -ne 0) { throw "$stage shader compilation failed." }
}
foreach ($architecture in @("x86", "x64")) {
    Import-VisualStudioEnvironment -Architecture $architecture
    $directory = Join-Path $OutputRoot $architecture
    New-Item -ItemType Directory -Force -Path $directory | Out-Null
    $executable = Join-Path $directory "d3d12-draw.exe"
    & cl.exe /nologo /EHsc /O2 /W4 /MT /DHELIOS_G1_NATIVE "/I$generated" `
        (Join-Path $RepoRoot "tools\d3d12_bridge_probe.cpp") `
        "/Fo:$(Join-Path $directory 'd3d12-draw.obj')" "/Fe:$executable"
    if ($LASTEXITCODE -ne 0) { throw "D3D12 draw probe $architecture compilation failed." }
    Assert-HeliosPeArchitecture $executable $architecture
    Write-Host "Built Windows-runtime draw probe: $executable"
}
