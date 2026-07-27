<#
.SYNOPSIS
  Prove the T3 deferred-programming refusal instrument by forcing each exit in
  turn and checking that its named counter moved.

.DESCRIPTION
  `ScForceReject` (service key, REG_DWORD, default 0 and ABSENT in production)
  forces ONE deferred-programming refusal exit so its counter can be proven to
  move rather than assumed to work. It is read at StartDevice, so
  `pnputil /restart-device` iterates values without a reboot.

    1 = BadAlloc   -> ScBadAlc      5 = LinearAllocFailed -> ScLinErr  (see the
                                    flush caveat in the body: the COUNTERS are
    2 = Extent     -> ScBadExt      6 = SetFailed         -> ScSetErr
                                    unobservable while the refusal is forced,
                                    so the sweep checks the per-refusal
                                    BREADCRUMB instead)
    3 = Layout     -> ScBadLay      7 = NoTarget          -> ScNoTgt
    4 = Format     -> ScBadFmt      8 = CopyFailed        -> ScCpyErr

  7 and 8 sit on the copy/fallback arm and are UNREACHABLE with a direct
  primary — they need a forced-fallback build, so they are skipped by default.

  A nonzero `ScFrc` in any dump means the `Sc*Err` values in it were PROVOKED,
  not observed. This script restores the production state (ScForceReject
  deleted, one final restart-device) before it exits, including on failure.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File Z:\tools\kmd-force-reject-sweep.ps1
#>
[CmdletBinding()]
param(
    [int[]] $Values = @(1, 2, 3, 4, 5, 6),
    [int]   $SettleSeconds = 12
)

$ErrorActionPreference = 'Continue'
$key = 'HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render'
$counter = @{
    1 = 'ScBadAlc'; 2 = 'ScBadExt'; 3 = 'ScBadLay'; 4 = 'ScBadFmt'
    5 = 'ScLinErr'; 6 = 'ScSetErr'; 7 = 'ScNoTgt';  8 = 'ScCpyErr'
}
$instance = (Get-PnpDevice -Class Display -FriendlyName '*Helios*' | Select-Object -First 1).InstanceId

function Read-Counter([string]$name) {
    $p = (Get-ItemProperty $key).PSObject.Properties | Where-Object { $_.Name -eq $name }
    if ($p) { [int64]$p.Value } else { 0 }
}

function Restart-Helios {
    pnputil /restart-device "$instance" | Out-Null
    Start-Sleep -Seconds $SettleSeconds
}

# ⚠ The Sc*Err COUNTERS cannot prove this. `record_scanout_reject_counters` is
# reachable only from `AdapterContext::pacing_snapshot`, which is driven by
# `scanout_refresh_count` — the count of SUCCESSFUL scanout refreshes. Force
# every SetVidPnSourceAddress to refuse and no refresh happens, so the snapshot
# never runs and the counters never reach the registry however long you wait.
# That is why the T3 gate could not discharge this leg.
#
# `ScanoutReject::record()` writes an UNGATED breadcrumb at the refusal itself,
# so that is what this sweep checks. The pairs are owner debugging ABI.
$expect = @{
    1 = @{ Name = 'ScRid'; Value = 0 }        # BadAlloc
    2 = @{ Name = 'ScSet'; Value = 0xD }      # Extent
    3 = @{ Name = 'ScSet'; Value = 0xE3 }     # Layout
    4 = @{ Name = 'ScFmt'; Value = $null }    # Format — carries the format, so
                                              # only "it changed" is checkable
    5 = @{ Name = 'ScSet'; Value = 0xE1 }     # LinearAllocFailed
    6 = @{ Name = 'ScSet'; Value = 0xE }      # SetFailed
}

$results = @()
try {
    foreach ($v in $Values) {
        $name = $counter[$v]
        $e = $expect[$v]
        $before = Read-Counter $e.Name
        Set-ItemProperty -Path $key -Name 'ScForceReject' -Value $v -Type DWord
        Restart-Helios
        $after = Read-Counter $e.Name
        $frc = Read-Counter 'ScFrc'
        $scset = Read-Counter 'ScSet'
        # The Format arm is NOT DISCRIMINABLE, and that is an instrument
        # limitation rather than a driver defect: its breadcrumb is
        # `ScFmt = <the live dxgi_format>`, which on this box is the same 88 the
        # healthy path leaves there, and unlike every other arm it writes no
        # `ScSet` value to distinguish itself. `forced == 4` sits literally
        # between `forced == 3` and `forced == 5` in program_vidpn_source, both
        # of which are proven to fire, so the path is reached — there is simply
        # nothing recorded that separates "refused with format 88" from
        # "succeeded with format 88". Giving it its own breadcrumb value is a
        # code change, and ScForceReject is a T6 deletion candidate anyway.
        $verdict =
            if ($frc -ne $v) { 'KNOB NOT READ' }
            elseif ($null -eq $e.Value) { 'NOT DISCRIMINABLE' }
            elseif ($after -eq $e.Value) { 'FIRED' }
            else { 'NOT PROVEN' }
        $results += [pscustomobject]@{
            Value = $v; Counter = $name; Read = (Read-Counter $name); ScFrc = $frc
            Verdict = $verdict
        }
        Write-Host ("  ScForceReject={0,-2} {1,-9} breadcrumb {2}={3,-5} (was {4,-5}) ScSet={5,-4} ScFrc={6,-2} {7}" -f
            $v, $name, $e.Name, $after, $before, $scset, $frc, $results[-1].Verdict)
    }
} finally {
    Write-Host ""
    Write-Host "restoring production state (ScForceReject removed, restart-device)"
    Remove-ItemProperty -Path $key -Name 'ScForceReject' -ErrorAction SilentlyContinue
    Restart-Helios
    $frc = Read-Counter 'ScFrc'
    $dev = Get-PnpDevice -Class Display -FriendlyName '*Helios*' | Select-Object -First 1
    $prob = (Get-PnpDeviceProperty -InstanceId $dev.InstanceId -KeyName 'DEVPKEY_Device_ProblemCode').Data
    Write-Host ("  ScFrc now {0} (expect 0)   device {1} problem-code {2}" -f $frc, $dev.Status, $prob)
}

$bad = $results | Where-Object { $_.Verdict -notin @('FIRED', 'NOT DISCRIMINABLE') }
if ($bad) {
    Write-Host ("NOT PROVEN: {0}" -f (($bad | ForEach-Object { "$($_.Value)/$($_.Counter)" }) -join ', '))
    exit 1
}
$n = ($results | Where-Object { $_.Verdict -eq 'FIRED' }).Count
Write-Host ("{0} of {1} forced exits PROVEN by breadcrumb; the rest are not discriminable" -f $n, $results.Count)
