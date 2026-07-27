<#
.SYNOPSIS
  A/B a `vulkan_virtio` ICD fault signature across images: the Application log
  persists across boots, so grouping every id-1000 fault by time and comparing
  against the boot history answers "is this new to my image?" without a rebuild.

.DESCRIPTION
  This is how ROADMAP WS1 defect 0z was attributed as PRE-EXISTING rather than
  blamed on R614: 889 faults spanning three weeks, including T4a's own
  nine-restart-device soak on the previous image.
  Output is large - redirect to a file and grep it.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File Z:\tools\icd-fault-history.ps1 > Z:\tmp\icd-faults.txt
#>
# Is the vulkan_virtio 0xc0000005 fault at restart-device NEW to 22.22.186.0, or
# has it been happening on earlier images too? The Application log persists across
# boots, so this is the A/B: group every such fault by day+time and compare
# against the boot history.
$ErrorActionPreference = 'Continue'

Write-Host "=== every vulkan_virtio faulting-module crash in the Application log ==="
$ev = Get-WinEvent -FilterHashtable @{LogName = 'Application'; Id = 1000 } -ErrorAction SilentlyContinue |
    Where-Object { $_.Message -match 'vulkan_virtio' }
Write-Host ("total: {0}" -f ($ev | Measure-Object).Count)
$ev | Sort-Object TimeCreated | ForEach-Object {
    $app = ''
    if ($_.Message -match 'Faulting application name: ([^,]+)') { $app = $Matches[1] }
    $code = ''
    if ($_.Message -match 'Exception code: (0x[0-9a-fA-F]+)') { $code = $Matches[1] }
    Write-Host ("{0}  {1,-34} {2}" -f $_.TimeCreated, $app, $code)
}

Write-Host "`n=== boot history (System log id 6005 = event log started) ==="
Get-WinEvent -FilterHashtable @{LogName = 'System'; Id = 6005 } -MaxEvents 12 -ErrorAction SilentlyContinue |
    Sort-Object TimeCreated | ForEach-Object { Write-Host ("boot: {0}" -f $_.TimeCreated) }
