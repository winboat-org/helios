# Orchestrate one RDP repro measurement.
#
# This script itself is safe to run from win_exec/session 0 — it only
# orchestrates. The *workload* is pushed into the interactive session by
# cloning an existing interactive task (helios_perf_fs), which is the canonical
# five-line recipe (CONFORMANCE.md 5, tmp/perf/launch-gt1-arm.ps1:16-24).
param(
    [ValidateSet('drag', 'full', 'idle')]
    [string]$Mode = 'drag',
    [int]$Seconds = 20
)

$taskName = 'helios_rdp_repro'
$taskXml = 'C:\Windows\Temp\helios_rdp_repro.xml'
$outFile = 'Z:\tmp\rdp-repro.out'

Remove-Item $outFile -EA SilentlyContinue

[xml]$xml = (schtasks /query /tn helios_perf_fs /xml ONE | Out-String)
$xml.Task.Actions.Exec.Command = 'powershell.exe'
$xml.Task.Actions.Exec.Arguments =
    "-NoProfile -ExecutionPolicy Bypass -File Z:\tools\rdp-lag-repro.ps1 -Mode $Mode -Seconds $Seconds"
$xml.Save($taskXml)

schtasks /create /tn $taskName /xml $taskXml /f | Out-Null
schtasks /run /tn $taskName | Out-Null

# Let the window come up and DWM settle into the steady-state damage pattern
# before sampling, so the create/first-paint transient is not in the average.
Start-Sleep -Seconds 3

& 'Z:\tools\rdp-sample.ps1' -Seconds ($Seconds - 6) -Label "mode=$Mode"

Start-Sleep -Seconds 4
if (Test-Path $outFile) {
    Write-Output "--- repro self-report ---"
    Get-Content $outFile
} else {
    Write-Output "--- repro produced NO output file: it may not have run in an interactive session ---"
}
