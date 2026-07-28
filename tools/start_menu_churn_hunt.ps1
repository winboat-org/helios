# start_menu_churn_hunt.ps1 — reproduce ROADMAP WS1 defect 0w under the churn it
# actually happened in.
#
# 25 quiet restarts of StartMenuExperienceHost produced 0 wedges: on an idle box
# the caller almost always wins the race against the CS thread. The instance that
# DID wedge was spawned in the shell-restart storm that follows an adapter
# teardown, so this loop recreates exactly that: `pnputil /restart-device`, which
# access-violates dwm and the shell processes (the known, pre-existing defect 0z),
# then a mass relaunch under load.
#
# Runs from session 0 (restart-device is a device operation); the Start menu
# invoke is delegated to the session-1 `helios_pokestart` task because synthetic
# input is a no-op in session 0.
#
# ⚠ DISRUPTIVE: every cycle visibly restarts the desktop. Owner-authorised for an
# unattended VM only.
#
# Output: C:\ProgramData\Helios\churn_hunt.txt

param(
    [int]$Cycles = 12,
    [int]$SettleSec = 22
)

$out     = 'C:\ProgramData\Helios\churn_hunt.txt'
$keep    = 'C:\ProgramData\Helios\wedgehunt'
$dxvkLog = 'C:\ProgramData\Helios\StartMenuExperienceHost_helios_umd_dxvk.log'
$dev     = 'PCI\VEN_1AF4&DEV_1050&SUBSYS_11001AF4&REV_01\4&27FF4EC&0&0017'
New-Item -ItemType Directory -Force -Path $keep | Out-Null

$sb = [System.Text.StringBuilder]::new()
function W([string]$s) { [void]$sb.AppendLine($s); Write-Host $s; [System.IO.File]::WriteAllText($out, $sb.ToString()) }

W "=== start_menu_churn_hunt $(Get-Date -Format o) cycles=$Cycles ==="

for ($c = 1; $c -le $Cycles; $c++) {
    $r = & pnputil /restart-device "$dev" 2>&1 | Out-String
    $ok = ($r -match 'restarted|Success')
    Start-Sleep -Seconds $SettleSec

    # Make the shell relaunch the Start menu host and exercise the invoke.
    schtasks /run /tn helios_pokestart 2>&1 | Out-Null
    Start-Sleep -Seconds 8

    $p = Get-Process -Name StartMenuExperienceHost -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $p) {
        schtasks /run /tn helios_pokestart 2>&1 | Out-Null
        Start-Sleep -Seconds 8
        $p = Get-Process -Name StartMenuExperienceHost -ErrorAction SilentlyContinue | Select-Object -First 1
    }
    if (-not $p) { W ("cycle {0,-3}: restart-device ok={1}; StartMenu NOT running" -f $c, $ok); continue }

    # A healthy host burns CPU on an invoke; a wedged one burns exactly none.
    $p.Refresh(); $cpu1 = $p.CPU
    schtasks /run /tn helios_pokestart 2>&1 | Out-Null
    Start-Sleep -Seconds 5
    $p.Refresh()
    $cpuDelta = $p.CPU - $cpu1

    $unsat = $null
    if (Test-Path $dxvkLog) {
        $unsat = Get-Content $dxvkLog -ErrorAction SilentlyContinue | Select-String -SimpleMatch 'UNSATISFIABLE waitForResource'
        Copy-Item $dxvkLog (Join-Path $keep "churn$c-pid$($p.Id)-dxvk.log") -ErrorAction SilentlyContinue
    }

    $umd = "C:\ProgramData\Helios\umd-$($p.Id).log"
    $call = 0; $ret = 0; $lastIsCall = $false
    if (Test-Path $umd) {
        $lines = Get-Content $umd -ErrorAction SilentlyContinue
        $call = ($lines | Select-String -SimpleMatch 'calling DXVK CreateTexture2D').Count
        $ret  = ($lines | Select-String -SimpleMatch 'DXVK CreateTexture2D returned').Count
        $lastIsCall = (($lines | Select-Object -Last 1) -match 'calling DXVK CreateTexture2D')
    }

    $wedged = ($unsat -and $unsat.Count -gt 0) -or ($lastIsCall -and $cpuDelta -eq 0)
    W ("cycle {0,-3}: pid={1,-6} cpuDelta={2,6:N3}s calling={3,-3} returned={4,-3} lastIsCall={5,-5} unsat={6,-3} => {7}" -f `
        $c, $p.Id, $cpuDelta, $call, $ret, $lastIsCall, $(if ($unsat) { $unsat.Count } else { 0 }), $(if ($wedged) { 'WEDGED' } else { 'ok' }))

    if ($wedged) {
        W ""
        W "*** WEDGE REPRODUCED on cycle $c, pid $($p.Id) ***"
        if ($unsat) { foreach ($u in $unsat) { W ("    " + $u.Line) } }
        powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\take-minidump.ps1 -ProcessId $p.Id -Path "C:\ProgramData\HeliosDumps\wedge-$($p.Id).dmp" 2>&1 | ForEach-Object { W ("    dump: " + $_) }
        break
    }
}

W "=== end $(Get-Date -Format o) ==="
