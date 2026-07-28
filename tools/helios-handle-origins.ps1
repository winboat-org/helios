# Build and run the handle-ORIGIN probe (tools\helios_handle_origins.cpp).
#
# tools\helios-handle-types.ps1 names the object type and the owning module.
# This names the call site: it IAT-hooks the handle-minting kernel32 entry
# points across every loaded module and prints a stack for each handle that a
# single D3D11 device create/release leaves behind.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\helios-handle-origins.ps1
#
# Frames print as `module+0xRVA`, plus a symbol where dbghelp resolves one. The
# venus ICD is mingw-built (DWARF), which dbghelp cannot read — resolve those
# with `addr2line -e <the ICD dll> <rva>`.
param(
    [switch]$Warp,
    [switch]$SkipBuild
)

$probeDir = 'C:\Users\Rupansh\helios-probe'
$src = 'Z:\tools\helios_handle_origins.cpp'
$exe = Join-Path $probeDir 'helios_handle_origins.exe'

if (-not $SkipBuild) {
    if (-not (Test-Path $probeDir)) { New-Item -ItemType Directory -Path $probeDir | Out-Null }
    Copy-Item $src (Join-Path $probeDir 'helios_handle_origins.cpp') -Force

    $vcvars = 'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'
    if (-not (Test-Path $vcvars)) {
        Write-Output "BUILD FAILED: vcvars64.bat not found at $vcvars"
        exit 1
    }
    # /Zi so the probe's own frames symbolise; the interesting frames are in
    # other modules, but a probe that cannot name itself is confusing to read.
    $build = "call `"$vcvars`" >nul && cd /d `"$probeDir`" && cl /nologo /EHsc /W4 /O2 /Zi " +
             "helios_handle_origins.cpp /link dxgi.lib d3d11.lib psapi.lib dbghelp.lib /OUT:`"$exe`""
    $out = cmd /c $build 2>&1 | Out-String
    if (-not (Test-Path $exe)) {
        Write-Output "BUILD FAILED:"
        Write-Output $out
        exit 1
    }
    Write-Output "built: $exe"
}

Write-Output "=== running handle-origin probe ==="
$adapterArg = if ($Warp) { "warp" } else { "helios" }
& $exe $adapterArg
$rc = $LASTEXITCODE
Write-Output "=== probe exit code $rc ==="
exit $rc
