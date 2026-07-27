<#
.SYNOPSIS
  The standing per-image gate surface: driver identity, device state, the
  direct-primary per-flip values, and every failure counter that must be 0.

.DESCRIPTION
  Every tranche gate re-checks the same things, and doing it by hand is how a
  check gets skipped. This prints them in one pass and returns a non-zero exit
  code if any failure counter moved, so "the gate passed" is a machine answer
  rather than a reading of a wall of numbers.

  It deliberately does NOT judge cadence (DComp fps) or anything needing a
  screenshot — those are separate, and cadence is an open WS2 question with a
  boot-to-boot spread wider than any tranche's effect.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File Z:\tools\kmd-gate-surface.ps1
#>
[CmdletBinding()]
param(
    # Counters that must read 0. A missing name is fine (never created = never
    # hit); a present, nonzero one fails the gate.
    [string[]] $MustBeZero = @(
        # transport. NOTE: PARKED_LEAKS has no registry mirror — read it from
        # tools/escape_owner_probe.c (QUERY_STATS) or a CollectDbgInfo dump.
        'WtOut', 'WtTbl', 'CtOut',
        # T3 deferred-programming refusals
        'ScBadAlc', 'ScBadExt', 'ScBadLay', 'ScBadFmt', 'ScLinErr', 'ScSetErr',
        'ScNoTgt', 'ScCpyErr', 'ScUnav', 'ScRetry', 'ScGaveUp', 'ScStale',
        'ScGateCx', 'HpdStTo',
        # T4a
        'VnEncOvf', 'VnRingFt', 'VnRingWd', 'VnRingSz', 'VnMtDown', 'CpNoDrn',
        'PBTdErr', 'CtNotOurs', 'ChSzMm', 'ChSzPv', 'MapDup',
        'PciCapOob', 'WnRcf',
        # R614. Packed (count << 8) | last_irql, so ANY nonzero value means a
        # `PassiveLevel::assume()` site was reached above PASSIVE_LEVEL — i.e. a
        # `// SAFETY:` claim at one of the twelve audited mints is FALSE on this
        # machine. It is the one counter in this list that indicts the driver's
        # own reasoning rather than the host or the transport, and a nonzero
        # value is a design-gap escalation, not something to absorb: the DDI it
        # names needs its own IRQL split, like MapCpuHostAperture already has.
        'IrqlBad',
        # aperture / paging failure family
        'ChEi', 'ChEa', 'ChEp', 'ChEs', 'ChEb', 'ChEm', 'ChEu'
    ),
    # Values printed for eyeball comparison against the previous boot, not judged.
    [string[]] $Report = @(
        'VpSA', 'ScSet', 'ScFlu', 'ScRid', 'VpDSt', 'DspMd', 'ScCpy', 'ScPch',
        'ScFrc', 'VsCnt', 'SaCnt', 'PkHi', 'IfHi', 'AsSub', 'AsDone', 'ChSzDl',
        # QfRet is BACKPRESSURE, not a failure: it moves under real load (63 per
        # Fire Strike run, measured on both sides of T4a). Judged by order of
        # magnitude, not by zero, so it is reported rather than gated.
        'QfRet',
        # AbnDrop is likewise NOT a failure counter. DxgkDdiPreemptCommand is
        # "TDR probe OR PRIORITY SCHEDULING", and VidSch legitimately preempts
        # under load: 45 fences across three Fire Strike runs on 22.22.184.0,
        # with dwm's pid unchanged and zero 4101/dxgkrnl events in the System
        # log. It reads 0 on a clean idle session; if it moves, check for a TDR
        # rather than assuming one.
        'AbnDrop'
    )
)

$ErrorActionPreference = 'Continue'
$key = 'HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render'
$props = (Get-ItemProperty $key).PSObject.Properties

$dev = Get-PnpDevice -Class Display -FriendlyName '*Helios*' | Select-Object -First 1
$ver = (Get-PnpDeviceProperty -InstanceId $dev.InstanceId -KeyName 'DEVPKEY_Device_DriverVersion').Data
$prob = (Get-PnpDeviceProperty -InstanceId $dev.InstanceId -KeyName 'DEVPKEY_Device_ProblemCode').Data

Write-Host "driver    : $ver"
Write-Host "device    : $($dev.Status)  problem-code $prob  (0 = CM_PROB_NONE)"
Write-Host "last boot : $((Get-CimInstance Win32_OperatingSystem).LastBootUpTime)"
$dwm = Get-Process dwm -ErrorAction SilentlyContinue | Select-Object -First 1
Write-Host "dwm       : pid $($dwm.Id) started $($dwm.StartTime)"

Write-Host ""
Write-Host "--- must be 0 or absent ---"
$failed = @()
foreach ($n in $MustBeZero) {
    $p = $props | Where-Object { $_.Name -eq $n }
    if (-not $p) { continue }
    if ([int64]$p.Value -ne 0) {
        Write-Host ("  FAIL {0,-12} {1}" -f $n, $p.Value)
        $failed += $n
    }
}
if ($failed.Count -eq 0) { Write-Host "  all clear" }

Write-Host ""
Write-Host "--- reported (compare to the previous boot; not judged) ---"
foreach ($n in $Report) {
    $p = $props | Where-Object { $_.Name -eq $n }
    $v = if ($p) { $p.Value } else { '<absent>' }
    Write-Host ("  {0,-12} {1}" -f $n, $v)
}
$sd = $props | Where-Object { $_.Name -eq 'ScanoutDiag' }
Write-Host ("  {0,-12} {1}" -f 'ScanoutDiag', $(if ($sd) { "$($sd.Value)  <-- MUST be absent or 0 in production" } else { '<absent>  OK' }))

Write-Host ""
if ($failed.Count -ne 0) {
    Write-Host ("GATE SURFACE FAILED: {0}" -f ($failed -join ', '))
    exit 1
}
Write-Host "GATE SURFACE CLEAN"
