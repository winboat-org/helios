<#
.SYNOPSIS
  The UMD-side gate surface: the counters T5 added, read from the DWM UMD log.

.DESCRIPTION
  `kmd-gate-surface.ps1` reads the KMD's registry counters. The UMD has no
  registry counter surface at all -- its counters are process-global
  `AtomicUsize` values, observable only through log lines in
  `C:\ProgramData\Helios\umd-<pid>.log`. This reads them the same way, so a
  tranche gate checks both halves without doing the UMD half by hand.

  Two shapes of counter live here and they are read differently:

    * SUMMARISED -- the four scan-out counters are printed as a
      `direct_over_linear= downres_kept= zero_extent= zero_pitch=` group on
      every scan-out decision, so the LAST occurrence is the current value.
      Absence of the group means no scan-out target was ever established in
      this process, which is itself a finding for dwm.

    * FIRST-HIT-ONLY -- the rest log once (or for the first N hits) when they
      fire and are silent otherwise, so ABSENCE IS THE ZERO READING. Present =
      failure. This is why the script reports "clean" for a missing line rather
      than "unknown".

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
        # R809 -- a scan-out target with a zero width or height.
        'scanout target refused: zero extent',
        'scanout import refused: zero extent',
        # R801 -- an adapter handle that is not the token we handed out.
        'adapter handle not ours',
        # R812 -- we create no deferred contexts, so this must never be called.
        'CheckDeferredContextHandleSizes called',
        # R817 -- flip-wait setup called with parameters differing from the
        # armed ctx (dxvk_bridge.cpp). Unreachable until a device reset with a
        # new monitored fence; if it appears, that path is now live.
        'flip-kwait setup REFUSED',
        # R826 -- present-sync publish found no free slot.
        'NO SLOT published',
        # Generic UMD fault vocabulary that should not appear on a healthy
        # session. `present_frame_gate DxvkError` is the C++ gate's own catch.
        'present_frame_gate DxvkError',
        'DEVICE REMOVED'
    )
)

$ErrorActionPreference = 'Continue'
$logDir = 'C:\ProgramData\Helios'

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

Write-Host ""
Write-Host "--- scan-out counters (last value wins; group is printed per decision) ---"
$sawGroup = $false
foreach ($f in $logs) {
    $hit = Select-String -Path $f.FullName -Pattern 'direct_over_linear=' |
        Select-Object -Last 1
    if ($hit) {
        $sawGroup = $true
        if ($hit.Line -match '(direct_over_linear=\S+\s+downres_kept=\S+\s+zero_extent=\S+\s+zero_pitch=\S+)') {
            Write-Host ("  {0,-20} {1}" -f $f.Name, $matches[1])
        } else {
            Write-Host ("  {0,-20} {1}" -f $f.Name, $hit.Line)
        }
    }
}
if (-not $sawGroup) {
    Write-Host "  <no scan-out decision recorded in scope>"
    Write-Host "  NOTE: for dwm this means no scan-out target was ever established."
}

Write-Host ""
Write-Host "--- must NOT appear (absence IS the zero reading) ---"
$failed = @()
foreach ($pat in $MustNotAppear) {
    foreach ($f in $logs) {
        $hits = Select-String -Path $f.FullName -Pattern $pat -SimpleMatch
        if ($hits) {
            Write-Host ("  FAIL {0}  [{1}]  x{2}" -f $pat, $f.Name, $hits.Count)
            Write-Host ("       first: {0}" -f ($hits | Select-Object -First 1).Line)
            $failed += $pat
        }
    }
}
if ($failed.Count -eq 0) { Write-Host "  all clear" }

Write-Host ""
Write-Host "--- present gate (C++ bridge; cost of the 10 ms producer gate) ---"
foreach ($f in $logs) {
    $g = Select-String -Path $f.FullName -Pattern 'present-gate:' | Select-Object -Last 1
    if ($g) { Write-Host ("  {0,-20} {1}" -f $f.Name, $g.Line.Trim()) }
}

Write-Host ""
if ($failed.Count -ne 0) {
    Write-Host ("UMD GATE SURFACE FAILED: {0}" -f (($failed | Sort-Object -Unique) -join ', '))
    exit 1
}
Write-Host "UMD GATE SURFACE CLEAN"
