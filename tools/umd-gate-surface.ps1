<#
.SYNOPSIS
  The UMD-side gate surface: the counters T5 added, read from the DWM UMD log.

.DESCRIPTION
  `kmd-gate-surface.ps1` reads the KMD's registry counters. The UMD has no
  registry counter surface at all -- its counters are process-global
  `AtomicUsize` values, observable only through log lines in
  `C:\ProgramData\Helios\umd-<pid>.log`. This reads them the same way, so a
  tranche gate checks both halves without doing the UMD half by hand.

  Every counter here is FIRST-HIT-ONLY: it logs once (or for the first N hits)
  when it fires and is silent otherwise, so ABSENCE IS THE ZERO READING.
  Present = failure. This is why the script reports "clean" for a missing line
  rather than "unknown".

  T6 removed the summarised half. The
  `direct_over_linear= downres_kept= zero_extent= zero_pitch=` group came from
  `scanout_counter_summary()`, which went with R910's deletion of the UMD LINEAR
  scan-out copy -- three of those four counters described a path that no longer
  exists. `zero_pitch` survives and reports itself inline on the
  `SCAN-OUT PRIMARY ZERO PITCH` line, which is already in the list below.

  Reads dwm's log by default, since that is the process whose UMD behaviour the
  desktop depends on. `-AllProcesses` widens it to every UMD log written since
  the given time, which is what you want after a probe-suite run.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File Z:\tools\umd-gate-surface.ps1
  powershell -ExecutionPolicy Bypass -File Z:\tools\umd-gate-surface.ps1 -AllProcesses -SinceMinutes 30
#>
[CmdletBinding()]
param(
    # Widen from dwm's log to every UMD log touched in the window below.
    [switch] $AllProcesses,
    [int]    $SinceMinutes = 60,
    # Lines whose PRESENCE fails the gate. Each is emitted only when its
    # counter fires, so a missing line is the healthy reading.
    # Every string below was taken from the emitting source, not invented; a
    # pattern that never matches because the text differs is a silent pass.
    [string[]] $MustNotAppear = @(
        # R806 sub-commit 2 -- scan-out primary refused for a zero row pitch.
        # (forward.rs, "SCAN-OUT PRIMARY ZERO PITCH ... -> refused")
        'SCAN-OUT PRIMARY ZERO PITCH',
        # Pre-existing sibling arm: the primary create failed outright. Not a
        # T5 counter, but it is the one failure that turns the desktop black,
        # so a gate that reads this log should not walk past it.
        'SCAN-OUT PRIMARY CREATE FAILED',
        # R801 -- an adapter handle that is not the token we handed out.
        'adapter handle not ours',
        # R812 -- we create no deferred contexts, so this must never be called.
        'CheckDeferredContextHandleSizes called',
        # Generic UMD fault vocabulary that should not appear on a healthy
        # session. `present_frame_gate DxvkError` is the C++ gate's own catch.
        'present_frame_gate DxvkError',
        'DEVICE REMOVED'
    )
)

$ErrorActionPreference = 'Continue'
$logDir = 'C:\ProgramData\Helios'

# ⚠ UMD logs are keyed by PID ONLY and are APPENDED, never truncated. Windows
# reuses pids across boots, so `umd-<pid>.log` routinely contains several
# unrelated sessions from different builds stacked end to end -- the first time
# this script ran against a fresh deploy it read `present-gate:` lines written
# on 09-07 by a different dwm that happened to get the same pid, and reported
# them as the current build's. "Last occurrence wins" is only correct WITHIN a
# session.
#
# `UMD module: <path>` is emitted once per module load, so the last one marks
# the start of the newest session. Everything before it belongs to some earlier
# process and must not be read.
function Get-CurrentSession {
    param([string] $Path)
    $lines = Get-Content -LiteralPath $Path -ErrorAction SilentlyContinue
    if (-not $lines) { return @() }
    $starts = @()
    for ($i = 0; $i -lt $lines.Count; $i++) {
        if ($lines[$i] -match 'UMD module:') { $starts += $i }
    }
    if ($starts.Count -eq 0) { return $lines }   # no marker: read it all
    return $lines[$starts[-1]..($lines.Count - 1)]
}

if (-not (Test-Path $logDir)) {
    Write-Host "no $logDir -- the UMD has not run"
    exit 1
}

$logs = @()
if ($AllProcesses) {
    $cut = (Get-Date).AddMinutes(-$SinceMinutes)
    $logs = Get-ChildItem "$logDir\umd-*.log" -ErrorAction SilentlyContinue |
        Where-Object { $_.LastWriteTime -ge $cut }
    Write-Host "scope     : all UMD logs written in the last $SinceMinutes min ($($logs.Count) file(s))"
} else {
    $dwm = Get-Process dwm -ErrorAction SilentlyContinue | Sort-Object StartTime | Select-Object -First 1
    if (-not $dwm) { Write-Host "dwm not running"; exit 1 }
    $p = Join-Path $logDir ("umd-{0}.log" -f $dwm.Id)
    if (-not (Test-Path $p)) {
        Write-Host "dwm pid $($dwm.Id) has no UMD log at $p"
        Write-Host "(dwm may predate the current UMD, or has not loaded it yet)"
        exit 1
    }
    $logs = @(Get-Item $p)
    Write-Host "scope     : dwm pid $($dwm.Id), started $($dwm.StartTime)"
}

# Read each log ONCE, anchored to its newest session.
$sessions = @{}
foreach ($f in $logs) {
    $body = Get-CurrentSession $f.FullName
    $sessions[$f.Name] = $body
    $mod = ($body | Where-Object { $_ -match 'UMD module:' } | Select-Object -First 1)
    Write-Host ("  {0,-20} {1} line(s) in the newest session" -f $f.Name, $body.Count)
    if ($mod) { Write-Host ("  {0,-20} {1}" -f '', $mod.Trim()) }
}

Write-Host ""
Write-Host "--- direct-scanout registry (bounded-growth check) ---"
Write-Host "    (the exact-primary identity path; the LINEAR import went with T6/R910)"
foreach ($name in $sessions.Keys) {
    $hit = $sessions[$name] | Where-Object { $_ -match 'DDI scanout registry:' } | Select-Object -Last 1
    if ($hit) { Write-Host ("  {0,-20} {1}" -f $name, $hit.Trim()) }
    else      { Write-Host ("  {0,-20} <no entry dropped this session>" -f $name) }
}

Write-Host ""
Write-Host "--- vehicle present (R912a: the kwait producer is retired) ---"
Write-Host "    (get_present_result is a stub returning -1; the ICD's serial"
Write-Host "     wait_last_present is the recycle gate. One line per process.)"
foreach ($name in $sessions.Keys) {
    $v = $sessions[$name] | Where-Object { $_ -match 'vehicle present #' } | Select-Object -Last 1
    if ($v) { Write-Host ("  {0,-20} {1}" -f $name, $v.Trim()) }
    $r = $sessions[$name] | Where-Object { $_ -match 'get_present_result: retired stub' } | Select-Object -Last 1
    if ($r) { Write-Host ("  {0,-20} {1}" -f '', $r.Trim()) }
}

Write-Host ""
Write-Host "--- must NOT appear (absence IS the zero reading) ---"
$failed = @()
foreach ($pat in $MustNotAppear) {
    foreach ($name in $sessions.Keys) {
        $hits = @($sessions[$name] | Where-Object { $_.Contains($pat) })
        if ($hits.Count -gt 0) {
            Write-Host ("  FAIL {0}  [{1}]  x{2}" -f $pat, $name, $hits.Count)
            Write-Host ("       first: {0}" -f $hits[0].Trim())
            $failed += $pat
        }
    }
}
if ($failed.Count -eq 0) { Write-Host "  all clear" }

Write-Host ""
Write-Host "--- present gate (C++ bridge; cost of the 10 ms producer gate) ---"
Write-Host "    (a T5 UMD appends 'failed= noctx='; a T4b one stops at 'timeouts=' -- R826)"
foreach ($name in $sessions.Keys) {
    $g = $sessions[$name] | Where-Object { $_ -match 'present-gate:' } | Select-Object -Last 1
    if ($g) { Write-Host ("  {0,-20} {1}" -f $name, $g.Trim()) }
    else    { Write-Host ("  {0,-20} <none this session>" -f $name) }
}

Write-Host ""
if ($failed.Count -ne 0) {
    Write-Host ("UMD GATE SURFACE FAILED: {0}" -f (($failed | Sort-Object -Unique) -join ', '))
    exit 1
}
Write-Host "UMD GATE SURFACE CLEAN"
