<#
.SYNOPSIS
  Take the UV1 reading: does the application's D3D12 fence latency grow when the
  KMD artificially holds an otherwise-READY D3D12 ECL packet?

.DESCRIPTION
  UV1 is the question the whole K-F workstream hangs on (KMD_IMPACT.md §14a.2,
  diag::knobs::WDDM_HOLD_MS): does dxgkrnl release the runtime's queued
  monitored-fence signal behind OUR DMA packets? If yes, the fence bridge is the
  right lever and carrying a real host GPU-completion fence is the fix. If no,
  none of K-F3..K-F9 is the answer and that must be said loudly.

  The instrument is `WddmHoldMs`, which delays retiring a D3D12 ECL packet at the
  WDDM FIFO head — after its three real dependencies are satisfied — so it is the
  one dependency this driver can create unilaterally. `WfBHold` counts the looks
  the hold absorbed. If the application's fence wait stretches by ~the hold,
  UV1 is ✓.

  ⛔ PRECONDITIONS, and without them a flat reading is a trusting-a-zero:
    * `WddmHoldMs` and `DiagLevel` are snapshotted at `VirtioGpu::init`, so
      setting them needs a REBOOT. `pnputil /restart-device` is not enough.
    * `DiagLevel >= 1`, or `diag::record` is a no-op and every counter reads 0.
    * `WfBHold` MUST have moved during the run. If it did not, the experiment
      did not run and the elapsed time says nothing about UV1.
  This script therefore reports `hold_attached` and refuses to call a flat
  reading meaningful when it is false.

  ⚠ It also reports the display heartbeat state indirectly: the hold is released
  by the 60 Hz display tick, so the display half must be armed (it is in the
  normal configuration).

.PARAMETER Probe
  allocator (256 ECLs, each with a fence wait — the sharpest oracle, ~1 s) or
  sync (the four native runtime synchronization cases).

.PARAMETER CompareTo
  A previous JSON from this script. When given, the script prints the paired
  delta (elapsed and WfBHold) instead of a single reading.

.EXAMPLE
  # boot with DiagLevel=1, WddmHoldMs=0
  powershell -File Z:\tools\uv1-fence-latency.ps1 -OutFile Z:\tmp\uv1-hold0.json
  # set WddmHoldMs=100, reboot
  powershell -File Z:\tools\uv1-fence-latency.ps1 -OutFile Z:\tmp\uv1-hold100.json
  # then:
  powershell -File Z:\tools\uv1-fence-latency.ps1 -CompareTo Z:\tmp\uv1-hold0.json
#>
[CmdletBinding()]
param(
    [ValidateSet('allocator', 'sync')][string] $Probe = 'allocator',
    [string] $OutFile = "$env:TEMP\uv1-fence-latency.json",
    [string] $CompareTo,
    [int] $TimeoutSec = 600
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$serviceKey = 'HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render'
# ⛔ These are the KMD's own REGISTRY names (the telemetry publisher in
# `ddi/submit_command.rs`). The `Ecl*`/`FenceRuntimeOwned` names belong to the
# UMD12 log's DDI dump, not to this key — watching them reads a permanent
# "(absent)", which is a trusting-a-zero of the same family the knob's own docs
# warn about.
$watched = @(
    # The experiment's signal, and the three real dependency arms it sits behind.
    'WfBHold', 'WfBWire', 'WfBStrm', 'WfBBlt', 'WfBReb',
    # Proof the D3D12 ECL path was live at all (precondition 2): D12Rec counts
    # ACCEPTED `HeliosD3D12SubmitCmd` records.
    'D12Rec', 'D12Exact', 'D12Merged', 'D12Zero',
    # Head-of-line arithmetic and the heartbeat, so a flat run can be attributed.
    'GpuFncClamp', 'DpDrp', 'WfDone', 'AsDone'
)

function Read-Counters {
    $key = Get-ItemProperty -Path $serviceKey -ErrorAction Stop
    $out = [ordered]@{}
    foreach ($name in $watched) {
        $prop = $key.PSObject.Properties[$name]
        $out[$name] = if ($null -ne $prop) { [int64]$prop.Value } else { $null }
    }
    $out
}

function Read-Knobs {
    $key = Get-ItemProperty -Path $serviceKey -ErrorAction Stop
    [ordered]@{
        WddmHoldMs = if ($null -ne $key.PSObject.Properties['WddmHoldMs']) { [int]$key.WddmHoldMs } else { 0 }
        WddmHeadMs = if ($null -ne $key.PSObject.Properties['WddmHeadMs']) { [int]$key.WddmHeadMs } else { $null }
        DiagLevel  = if ($null -ne $key.PSObject.Properties['DiagLevel']) { [int]$key.DiagLevel } else { 0 }
    }
}

if ($CompareTo) {
    if (-not (Test-Path -LiteralPath $CompareTo)) { throw "no baseline at $CompareTo" }
    $before = Get-Content -LiteralPath $CompareTo -Raw | ConvertFrom-Json
    $now = Read-Counters
    $holdDelta = [int64]$now['WfBHold'] - [int64]$before.counters_after.WfBHold
    $elapsedDelta = $null   # filled by the caller's own two files if it wants; kept explicit here
    $verdict = if ($holdDelta -gt 0) { 'UV1-ATTACHED (compare elapsed times in the two JSON files)' } else { 'EXPERIMENT-DID-NOT-RUN (WfBHold flat)' }
    [pscustomobject]@{
        baseline             = $before.utc
        current              = (Get-Date).ToUniversalTime().ToString('o')
        hold_ms_baseline     = $before.knobs.WddmHoldMs
        counters_now         = $now
        hold_delta_vs_baseline = $holdDelta
        verdict              = $verdict
    } | ConvertTo-Json -Depth 6
    exit 0
}

$state = Get-Content 'C:\ProgramData\Helios\install-state.json' -Raw | ConvertFrom-Json
$umd12 = $state.installedDirect3D.UserModeDriverName.value[3]
if (-not (Test-Path -LiteralPath $umd12)) { throw "UMD12 not found: $umd12" }
$sha = (Get-FileHash -LiteralPath $umd12).Hash

$knobs = Read-Knobs
$countersBefore = Read-Counters
$archive = Join-Path 'C:\ProgramData\Helios\uv1-evidence' (Get-Date -Format 'yyyyMMdd-HHmmss')

$env:HELIOS_WSI_ASYNC_PRESENT = '1'
$build = "C:\ProgramData\Helios\integration-277-probes\$Probe"
$arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$build\d3d12-$Probe-probe.ps1`" -Mode Run -BuildDir `"$build`" -ArchiveRoot `"$archive`" -ExpectedUmd12SHA256 $sha"

$info = New-Object Diagnostics.ProcessStartInfo
$info.FileName = 'powershell.exe'
$info.Arguments = $arguments
$info.UseShellExecute = $false
$info.CreateNoWindow = $true
$info.RedirectStandardOutput = $true
$info.RedirectStandardError = $true
$p = New-Object Diagnostics.Process
$p.StartInfo = $info
$sw = [Diagnostics.Stopwatch]::StartNew()
[void]$p.Start()
$stdoutTask = $p.StandardOutput.ReadToEndAsync()
$stderrTask = $p.StandardError.ReadToEndAsync()
$finished = $p.WaitForExit($TimeoutSec * 1000)
$sw.Stop()
if (-not $finished) { & taskkill.exe /PID $p.Id /T /F | Out-Null; $p.WaitForExit(5000) | Out-Null }
$stdout = $stdoutTask.GetAwaiter().GetResult()
$stderr = $stderrTask.GetAwaiter().GetResult()
$p.Dispose()

# The mirror is written by the PASSIVE sampler, so give it a moment and read twice.
Start-Sleep -Seconds 2
$countersAfter = Read-Counters

$firstLine = ($stdout -split "`r?`n" | Where-Object { $_.Trim() } | Select-Object -First 1)
$holdDelta = [int64]$countersAfter['WfBHold'] - [int64]$countersBefore['WfBHold']
$preconditions = [ordered]@{
    diag_level_nonzero  = $knobs.DiagLevel -ge 1
    hold_knob_nonzero   = $knobs.WddmHoldMs -ne 0
    ecl_path_live       = ([int64]$countersAfter['D12Rec'] - [int64]$countersBefore['D12Rec']) -gt 0
    hold_attached       = $holdDelta -gt 0
}
$verdict = if (-not $preconditions.diag_level_nonzero) { 'NO-COUNTERS (DiagLevel 0: reboot with DiagLevel=1)' }
elseif (-not $preconditions.ecl_path_live) { 'NO-D3D12-ECL-SUBMISSION (D12Rec did not move: the probe did not exercise the path)' }
elseif ($knobs.WddmHoldMs -eq 0) { 'BASELINE (hold off: this is the arm to compare against)' }
elseif (-not $preconditions.hold_attached) { 'HOLD-NEVER-ARMED (knob did not take, or no packet reached the head: reboot and re-run)' }
else { 'HELD (compare elapsed_ms against the hold-off arm)' }

$result = [ordered]@{
    utc            = (Get-Date).ToUniversalTime().ToString('o')
    probe          = $Probe
    host           = $env:COMPUTERNAME
    session        = (Get-Process -Id $PID).SessionId
    package        = $state.packageId
    umd12          = $umd12
    umd12_sha256   = $sha
    archive        = $archive
    knobs          = $knobs
    elapsed_ms     = [int64]$sw.ElapsedMilliseconds
    finished       = $finished
    exit_code      = $p.ExitCode
    first_output   = $firstLine
    counters_before = $countersBefore
    counters_after  = $countersAfter
    deltas         = [ordered]@{}
    preconditions  = $preconditions
    verdict        = $verdict
    stderr_tail    = (($stderr -split "`r?`n" | Select-Object -Last 5) -join "`n")
}
foreach ($name in $watched) {
    $result.deltas[$name] = [int64]$countersAfter[$name] - [int64]$countersBefore[$name]
}

$result | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $OutFile -Encoding utf8

# Human summary first, so the reading is readable without jq.
Write-Output ("UV1 probe={0} hold_ms={1} diag_level={2}" -f $Probe, $knobs.WddmHoldMs, $knobs.DiagLevel)
Write-Output ("  elapsed_ms      = {0}" -f $sw.ElapsedMilliseconds)
Write-Output ("  WfBHold delta   = {0}" -f $holdDelta)
Write-Output ("  D12Rec delta    = {0}" -f $result.deltas['D12Rec'])
Write-Output ("  first output    = {0}" -f $firstLine)
Write-Output ("  preconditions   = {0}" -f (($preconditions.GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" }) -join ' '))
Write-Output ("  VERDICT         = {0}" -f $verdict)
Write-Output ("  json            = {0}" -f $OutFile)
if (-not $finished) { throw "probe timed out after $TimeoutSec s" }
exit $p.ExitCode
