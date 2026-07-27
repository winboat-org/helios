# Build and run the T5 ownership soak (tools\helios_ownership_soak.cpp).
#
# The T5 gate's headline: 1000 CreateDevice/DestroyDevice cycles and 10000
# resource create/destroy cycles, with handle count, module count and process
# working set flat at the end. That is what proves the tranche's ownership
# encodings (Slot<P>, DeallocateForm, BridgeOwned::release, ScanoutTarget, the
# owned/borrowed bridge split) did not change lifetimes.
#
# Run it with the desktop up, so dwm is compositing through the same UMD while
# the harness cycles; the probe samples dwm's handle count and working set
# around the run too.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\helios-ownership-soak.ps1
#   ... -DeviceCycles 1000 -ResourceCycles 10000
#
# Exit code is the probe's: 0 flat, 2 growth, 1 setup failure.
param(
    [int]$DeviceCycles = 1000,
    [int]$ResourceCycles = 10000,
    [int]$WorkingSetToleranceKiB = 16384,
    # Run the same cycles against WARP instead of Helios. A handle delta
    # measured only against Helios cannot separate "our UMD leaks" from
    # "D3D11CreateDevice leaks on this box regardless of adapter".
    [switch]$Warp,
    [switch]$SkipBuild
)

$probeDir = 'C:\Users\Rupansh\helios-probe'
$src = 'Z:\tools\helios_ownership_soak.cpp'
$exe = Join-Path $probeDir 'helios_ownership_soak.exe'

if (-not $SkipBuild) {
    if (-not (Test-Path $probeDir)) { New-Item -ItemType Directory -Path $probeDir | Out-Null }
    Copy-Item $src (Join-Path $probeDir 'helios_ownership_soak.cpp') -Force

    # cl.exe needs the MSVC environment; vcvars64 is the only reliable way to
    # get it in a non-interactive shell.
    $vcvars = 'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'
    if (-not (Test-Path $vcvars)) {
        Write-Output "BUILD FAILED: vcvars64.bat not found at $vcvars"
        exit 1
    }
    $build = "call `"$vcvars`" >nul && cd /d `"$probeDir`" && cl /nologo /EHsc /W4 /O2 " +
             "helios_ownership_soak.cpp /link dxgi.lib d3d11.lib psapi.lib /OUT:`"$exe`""
    $out = cmd /c $build 2>&1 | Out-String
    if (-not (Test-Path $exe)) {
        Write-Output "BUILD FAILED:"
        Write-Output $out
        exit 1
    }
    Write-Output "built: $exe"
}

Write-Output "=== running ownership soak ==="
$adapterArg = if ($Warp) { "warp" } else { "helios" }
& $exe $DeviceCycles $ResourceCycles $WorkingSetToleranceKiB $adapterArg
$rc = $LASTEXITCODE
Write-Output "=== probe exit code $rc ==="
exit $rc
