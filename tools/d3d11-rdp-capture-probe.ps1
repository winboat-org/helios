# Build + run tools/d3d11_rdp_capture_probe.cpp on the VM.
# Follows the established probe-wrapper pattern (helios-handle-types.ps1): copy
# the source off the Z:\ share to a local C: path, build there under vcvars64.
param(
    [int]$Iterations = 60,
    [switch]$SkipBuild
)

$probeDir = 'C:\Users\Rupansh\helios-probe'
$src = 'Z:\tools\d3d11_rdp_capture_probe.cpp'
$exe = Join-Path $probeDir 'd3d11_rdp_capture_probe.exe'

if (-not $SkipBuild) {
    if (-not (Test-Path $probeDir)) { New-Item -ItemType Directory -Path $probeDir | Out-Null }
    Copy-Item $src (Join-Path $probeDir 'd3d11_rdp_capture_probe.cpp') -Force

    $vcvars = 'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'
    if (-not (Test-Path $vcvars)) {
        Write-Output "BUILD FAILED: vcvars64.bat not found at $vcvars"
        exit 1
    }
    $build = "call `"$vcvars`" >nul && cd /d `"$probeDir`" && cl /nologo /EHsc /W4 /O2 " +
             "d3d11_rdp_capture_probe.cpp /link d3d11.lib dxgi.lib /OUT:`"$exe`""
    $out = cmd /c $build 2>&1 | Out-String
    if (-not (Test-Path $exe)) {
        Write-Output "BUILD FAILED:"
        Write-Output $out
        exit 1
    }
    Write-Output "built: $exe"
}

& $exe $Iterations
