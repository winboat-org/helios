# tools/capture-knob-inventory.ps1 -- the S2 validation instrument.
#
# `log_knob_inventory()` (umd/src/log.rs) writes one `UMD knob: NAME=VALUE` line
# per registry knob, once per DLL load. `ARCHITECTURE.md` §11's S2-check makes
# that block the proof that moving `log`/`knobs` to `umd_common` changed nothing:
# capture it BEFORE the move and AFTER, and the diff must be empty.
#
# ⛔ Capture the "before" side BEFORE moving any code. It cannot be
# reconstructed afterwards.
#
# ── THREE TRAPS THIS SCRIPT EXISTS TO AVOID ────────────────────────────────────
#
# 1. ⚠ `C:\ProgramData\Helios\umd-<pid>.log` is APPEND-ONLY and keyed by PID,
#    and Windows reuses PIDs across boots. Measured 2026-08-05: umd-2628.log
#    held THREE different DriverStore generations
#    (..._e314278a09c4b25f with 5 knobs, ..._dcd660d40ba658bd with 5, and
#    ..._56ef2c2d61c1213c with 10). Reading "the knob lines in the log" gives a
#    union across driver generations, not this build's inventory. This script
#    takes only the block after the LAST `UMD module:` line and asserts that
#    module is the DLL currently in the DriverStore.
#
# 2. ⚠ Globbing every umd-*.log and `Sort-Object -Unique` is worse, not better:
#    on this box it returned 17 lines including `PresentGateUs` and
#    `PresentOrder`, two knobs DELETED by owner directive on 2026-07-29, plus
#    both arms of three past A/B runs. A "before" file containing knobs the code
#    no longer has cannot produce an empty diff and would be read as a
#    regression.
#
# 3. ⚠ The line is `[pid=N tid=M] UMD knob: NAME=VALUE` -- the prefix is TWO
#    whitespace-separated tokens and BOTH vary per run, so stripping one token
#    leaves `tid=M]` in the output and no two captures can ever match. The
#    regex below drops the whole bracketed prefix.
#
# ⚠ Never `-Recurse` under C:\ProgramData\Helios: it contains a junction loop.
[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)] [string] $Out,
  # A D3D11 client whose only job is to load the UMD once. Built here if absent.
  [string] $ProbeDir = 'C:\Users\Rupansh\knobcap'
)
$ErrorActionPreference = 'Stop'

$exe = Join-Path $ProbeDir 'dcprobe.exe'
if (-not (Test-Path $exe)) {
  New-Item -ItemType Directory -Force -Path $ProbeDir | Out-Null
  Copy-Item Z:\tools\d3d11_devicecreate_probe.cpp $ProbeDir -Force
  $vcvars = 'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'
  cmd /c "call `"$vcvars`" >nul && cd /d `"$ProbeDir`" && cl /nologo /EHsc /W3 /O2 d3d11_devicecreate_probe.cpp /link /OUT:`"$exe`" d3d11.lib dxgi.lib"
  if ($LASTEXITCODE -ne 0) { throw "probe build failed, exit $LASTEXITCODE" }
}

# The DLL the DriverStore will actually load. The captured block must come from
# THIS module or the capture describes some other build.
$store = Get-ChildItem 'C:\WINDOWS\System32\DriverStore\FileRepository\helios_kmd_render.inf_amd64_*\helios_umd.dll' |
         Sort-Object LastWriteTime -Descending | Select-Object -First 1
Write-Host "deployed UMD : $($store.FullName)"
Write-Host "         hash: $((Get-FileHash -Algorithm SHA256 $store.FullName).Hash)"
Write-Host "        mtime: $($store.LastWriteTime)"

$t0 = Get-Date
cmd /c "`"$exe`" > `"$ProbeDir\dcprobe.txt`" 2>&1"
$rc = $LASTEXITCODE
Write-Host "device-create probe exit = $rc"
if ($rc -ne 0) { throw "the probe must create a D3D11 device; without a load there is no inventory" }
Start-Sleep -Seconds 1

$log = Get-ChildItem C:\ProgramData\Helios\umd-*.log |
       Where-Object { $_.LastWriteTime -ge $t0 } |
       Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $log) { throw "no umd-*.log was written after the probe started" }
Write-Host "log          : $($log.FullName)"

# Take the LAST `UMD module:` line and everything after it. That is one DLL
# load by the build under test -- see trap 1.
$all = Get-Content $log.FullName
$idx = -1
for ($i = $all.Count - 1; $i -ge 0; $i--) { if ($all[$i] -match 'UMD module:') { $idx = $i; break } }
if ($idx -lt 0) { throw "no 'UMD module:' line in $($log.FullName)" }

$module = ($all[$idx] -replace '^\[[^\]]*\]\s*', '')
Write-Host "last load    : $module"

# ⚠ Compare HASHES, not paths. In the default ProgramData hotplug mode the
# module that actually loads is C:\ProgramData\HeliosUmd\helios_umd_<hash>.dll,
# NOT the DriverStore copy -- hotplug-helios-umd.ps1 writes both and points
# UserModeDriverName at the former. A path check rejects a perfectly good
# capture; a hash check proves the block came from the bytes just deployed,
# which is the thing that matters, and works in either mode.
$modPath = ($module -replace '^UMD module:\s*', '')
if (-not (Test-Path $modPath)) { throw "the last-loaded UMD is gone: $modPath" }
$modHash = (Get-FileHash -Algorithm SHA256 $modPath).Hash
$storeHash = (Get-FileHash -Algorithm SHA256 $store.FullName).Hash
Write-Host "loaded hash  : $modHash"
if ($modHash -ne $storeHash) {
  throw ("the last UMD load hashed $modHash but the DriverStore copy is $storeHash - " +
         "install first, or a stale process held the old DLL (memory 6TH: a DriverStore copy " +
         "stale by one build runs dwm's first device)")
}

$lines = $all[$idx..($all.Count - 1)] |
         Where-Object { $_ -match 'UMD knob:' } |
         ForEach-Object { $_ -replace '^\[[^\]]*\]\s*', '' }
if ($lines.Count -eq 0) { throw "the last UMD load emitted no knob inventory" }

# ⛔ ORDER IS PRESERVED, deliberately not sorted. `resolved_inventory()` returns
# the knobs in declaration order and that order is part of what the move must
# not change; sorting would hide a reordering.
#
# ⚠⚠ TRAP 4, measured here 2026-08-05: `Set-Content` to a path on the `Z:\` 9p
# share does NOT truncate an existing file -- it APPENDS. Four capture runs
# produced a 42-line file that read as an inventory with 32 extra knobs,
# including `PresentGateUs` and `PresentOrder`, which the code deleted in July.
# A before/after diff over that is meaningless in the direction that looks like
# a regression. Delete first, and verify the line count after writing.
if (Test-Path $Out) { Remove-Item -Force $Out }
$lines | Set-Content $Out
$written = @(Get-Content $Out)
if ($written.Count -ne $lines.Count) {
  throw "wrote $($lines.Count) lines but $Out now holds $($written.Count) - the Z:\ share did not truncate"
}
Write-Host "wrote $($lines.Count) knob lines -> $Out"
$lines
