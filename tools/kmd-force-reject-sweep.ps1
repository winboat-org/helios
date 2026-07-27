<#
.SYNOPSIS
  Prove the T3 deferred-programming refusal instrument by forcing each exit in
  turn and checking that its named counter moved.

.DESCRIPTION
  `ScForceReject` (service key, REG_DWORD, default 0 and ABSENT in production)
  forces ONE deferred-programming refusal exit so its counter can be proven to
  move rather than assumed to work. It is read at StartDevice, so
  `pnputil /restart-device` iterates values without a reboot.

    1 = BadAlloc   -> ScBadAlc      5 = LinearAllocFailed -> ScLinErr
    2 = Extent     -> ScBadExt      6 = SetFailed         -> ScSetErr
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

$results = @()
try {
    foreach ($v in $Values) {
        $name = $counter[$v]
        Set-ItemProperty -Path $key -Name 'ScForceReject' -Value $v -Type DWord
        Restart-Helios
        $val = Read-Counter $name
        $frc = Read-Counter 'ScFrc'
        $ok = ($val -gt 0) -and ($frc -eq $v)
        $results += [pscustomobject]@{
            Value = $v; Counter = $name; Read = $val; ScFrc = $frc
            Verdict = $(if ($ok) { 'MOVED' } else { 'NOT PROVEN' })
        }
        Write-Host ("  ScForceReject={0,-2} {1,-9} = {2,-4} ScFrc={3,-2} {4}" -f
            $v, $name, $val, $frc, $results[-1].Verdict)
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

$bad = $results | Where-Object { $_.Verdict -ne 'MOVED' }
if ($bad) {
    Write-Host ("NOT PROVEN: {0}" -f (($bad | ForEach-Object { "$($_.Value)/$($_.Counter)" }) -join ', '))
    exit 1
}
Write-Host "ALL FORCED EXITS PROVEN"
