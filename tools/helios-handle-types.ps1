# Build and run the handle-TYPE probe (tools\helios_handle_types.cpp).
#
# tools\helios-ownership-soak.ps1 says HOW MANY kernel handles a
# D3D11CreateDevice/Release pair on the Helios adapter leaks (5.99/device,
# WARP control 0.00). This says WHAT they are — object type, granted access,
# kernel object address and, where it is safe to ask, the object's name.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\helios-handle-types.ps1
#   ... -Cycles 5 -Warp
#
# Exit code is the probe's: 0 ran, 1 setup failure.
param(
    [int]$Cycles = 5,
    [switch]$Warp,
    # Hold the named modules loaded across the run ("all", or a module-name
    # prefix such as "helios_umd"). Each transient module's process-lifetime
    # statics are released by nothing when it unloads, so pinning subtracts
    # exactly that module's own leak from the per-device figure.
    [string]$Pin = '',
    [switch]$SkipBuild
)

$probeDir = 'C:\Users\Rupansh\helios-probe'
$src = 'Z:\tools\helios_handle_types.cpp'
$exe = Join-Path $probeDir 'helios_handle_types.exe'

if (-not $SkipBuild) {
    if (-not (Test-Path $probeDir)) { New-Item -ItemType Directory -Path $probeDir | Out-Null }
    Copy-Item $src (Join-Path $probeDir 'helios_handle_types.cpp') -Force

    $vcvars = 'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'
    if (-not (Test-Path $vcvars)) {
        Write-Output "BUILD FAILED: vcvars64.bat not found at $vcvars"
        exit 1
    }
    $build = "call `"$vcvars`" >nul && cd /d `"$probeDir`" && cl /nologo /EHsc /W4 /O2 " +
             "helios_handle_types.cpp /link dxgi.lib d3d11.lib /OUT:`"$exe`""
    $out = cmd /c $build 2>&1 | Out-String
    if (-not (Test-Path $exe)) {
        Write-Output "BUILD FAILED:"
        Write-Output $out
        exit 1
    }
    Write-Output "built: $exe"
}

Write-Output "=== running handle-type probe ==="
$adapterArg = if ($Warp) { "warp" } else { "helios" }
& $exe $Cycles $adapterArg $Pin
$rc = $LASTEXITCODE
Write-Output "=== probe exit code $rc ==="
exit $rc
