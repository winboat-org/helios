<#
.SYNOPSIS
  Post-image check the tranche gates were MISSING: TDR/bugcheck entries in the
  System log AND user-mode faults in the APPLICATION log.

.DESCRIPTION
  Every gate up to T4a checked the System log for 4101/dxgkrnl/LiveKernelEvent and
  called a nine-cycle restart-device soak clean. Those entries stay legitimately
  empty for a USER-MODE fault, so the soak was in fact access-violating dwm and
  three shell processes inside the venus ICD on every cycle and nothing noticed
  (ROADMAP WS1 defect 0z, found 2026-07-27 during the R614 gate).

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File Z:\tools\kmd-app-fault-check.ps1
#>
# R614 gate: any TDR / display-driver-recovery / LiveKernelEvent since this boot?
$ErrorActionPreference = 'Continue'
$boot = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime
Write-Host ("boot: {0}" -f $boot)
$ids = 4101, 4102, 1001, 41, 6008, 219
$found = 0
Get-WinEvent -FilterHashtable @{LogName = 'System'; StartTime = $boot } -ErrorAction SilentlyContinue |
    Where-Object {
        $ids -contains $_.Id -or
        $_.ProviderName -match 'dxgkrnl|Display|LiveKernelEvent|BugCheck'
    } |
    ForEach-Object {
        $found++
        Write-Host ("[{0}] id={1} provider={2}" -f $_.TimeCreated, $_.Id, $_.ProviderName)
        Write-Host ("    " + ($_.Message -replace "`r?`n", ' ').Substring(0, [Math]::Min(200, $_.Message.Length)))
    }
if ($found -eq 0) { Write-Host "NO TDR / dxgkrnl / bugcheck / LiveKernelEvent entries since boot" }

Write-Host "`n--- Helios / dwm application errors since boot ---"
$a = Get-WinEvent -FilterHashtable @{LogName = 'Application'; StartTime = $boot; Level = 1, 2 } -ErrorAction SilentlyContinue |
    Where-Object { $_.Message -match 'dwm|helios|3DMark' }
if ($a) {
    $a | ForEach-Object {
        Write-Host ("[{0}] id={1} {2}" -f $_.TimeCreated, $_.Id, $_.ProviderName)
        Write-Host ("    " + ($_.Message -replace "`r?`n", ' ').Substring(0, [Math]::Min(220, $_.Message.Length)))
    }
} else {
    Write-Host "none"
}
