<#
.SYNOPSIS
  Take the UV1 reading: does the application's D3D12 fence latency grow when the
  KMD artificially holds an otherwise-READY D3D12 ECL packet?

.DESCRIPTION
  UV1 (KMD_IMPACT.md §14a.2, diag::knobs::WDDM_HOLD_MS) is the question the whole
  K-F workstream hangs on: does dxgkrnl release the runtime's queued monitored-fence
  signal behind OUR DMA packets? If yes, the fence bridge is the right lever and
  carrying a real host GPU-completion fence can fix the D3D12 early-retirement
  defect. If no, none of K-F3..K-F9 is the answer and that must be said loudly.

  Instrument: `WddmHoldMs` delays retiring an otherwise-READY D3D12 ECL packet at
  the WDDM FIFO head — after its three real dependencies are satisfied — so it is
  the one dependency this driver can create unilaterally. `WfBHold` counts the
  looks the hold absorbed. Oracle: a probe that submits D3D12 work and waits on a
  fence per iteration (the allocator probe: 256 epochs).

  ⛔ WHAT INVALIDATED THE FIRST ATTEMPT (2026-09-13). Each is now checked here
  rather than by hand, because every one of them silently produces a plausible
  number:

    1. ARMS MUST NOT OVERLAP. The counters are adapter-global on a strictly
       head-of-line FIFO, so two concurrent probes read each other's holds. This
       script refuses to start while another D3D12 probe is running.
    2. A TIMEOUT IS NOT A LATENCY. `d3d12_allocator_probe.cpp:30` waits 10 s on a
       fence and `_Exit(1)`s, so 11.4 s and 23.0 s were ~1x and ~2x that bound: a
       whole-process wall time cannot tell "256 delayed epochs" from "one or two
       timeouts". `WtOut` (the mirror of `FENCE_WAIT_TIMEOUTS`) is watched and a
       run with any timeout is NOT a latency sample.
    3. COUNTERS NEED `DiagLevel`, which is snapshotted at `VirtioGpu::init`, so a
       missing reboot makes `diag::record` a no-op and reading the knob back proves
       nothing. Liveness is proved by hazard counters moving (`DpDrp`/`AsDone`).
    4. EXIT CODES: `$p.ExitCode` after `Dispose()` is null. The probe's result is
       recorded and a failure exits non-zero.
    5. PROVENANCE: instrument revision, boot time, uptime and knobs go in the JSON,
       so a later re-grader can tell two revisions apart.
    6. The JSON is written WITHOUT a BOM (python's json.load rejects a BOM).

  ⚠ A held run that fails the probe's CONTENT check is still a valid *latency*
  sample when `WtOut` did not move: PASS/FAIL is about content, not about the
  fence-wait latency under test. Both are recorded.

.PARAMETER Probe
  allocator (256 epochs, one fence wait each — the sharpest oracle) or sync.

.PARAMETER Baseline
  A previous JSON from this script. Grades the pair and refuses to grade when either
  arm is not a latency sample.

.EXAMPLE
  # boot with DiagLevel=1 and WddmHoldMs=0
  win-mcp --cli run-script --host vm tools/uv1-fence-latency.ps1 --purpose desktop `
    --task uv1-hold0 --arg -OutFile --arg C:\ProgramData\Helios\uv1-evidence\hold0.json
  # set WddmHoldMs=100, reboot, repeat as uv1-hold100
  # then grade (the measuring run passes both -Baseline and -OutFile):
  win-mcp --cli run-script --host vm tools/uv1-fence-latency.ps1 --purpose desktop `
    --task uv1-grade --arg -Baseline --arg C:\...\hold0.json --arg -OutFile --arg C:\...\hold100.json
#>
[CmdletBinding()]
param(
    [ValidateSet('allocator', 'sync')][string] $Probe = 'allocator',
    [string] $OutFile = "$env:TEMP\uv1-fence-latency.json",
    [string] $Baseline,
    [int] $TimeoutSec = 900
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$serviceKey = 'HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render'
# The KMD's own registry names (telemetry publisher in ddi/submit_command.rs and
# adapter/scanout.rs). The `Ecl*`/`FenceRuntimeOwned` family belongs to the UMD12
# log, NOT to this key: watching it reads a permanent "(absent)".
$watched = @(
    'WfBHold', 'WfBWire', 'WfBStrm', 'WfBBlt', 'WfBReb',
    'D12Rec', 'D12Exact', 'D12Merged', 'D12Zero',
    'WtOut', 'FENCE_WAIT_TIMEOUTS',
    'QfRet', 'GpuFncClamp', 'DpDrp', 'AsDone', 'WfDone', 'RngSub', 'RngCmp'
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

function Get-RunningProbes {
    @(Get-Process -ErrorAction SilentlyContinue |
        Where-Object { $_.ProcessName -match 'd3d12_(allocator|sync|tiled|stream_output|raytracing)_probe|d3d12-draw' })
}

function Write-JsonNoBom([string]$Path, $Object) {
    $dir = Split-Path -Parent $Path
    if ($dir -and -not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
    $json = $Object | ConvertTo-Json -Depth 8
    [IO.File]::WriteAllText($Path, $json, (New-Object System.Text.UTF8Encoding($false)))
}

$instrumentRevision = (Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash

# ── grading mode ────────────────────────────────────────────────────────────
if ($Baseline) {
    if (-not (Test-Path -LiteralPath $Baseline)) { throw "no baseline at $Baseline" }
    $b = Get-Content -LiteralPath $Baseline -Raw | ConvertFrom-Json
    if (-not $b.instrument_revision) {
        throw "baseline $Baseline has no instrument_revision: it was taken by an older script and cannot be compared (the 2026-09-13 first attempt is exactly this case)"
    }
    if ($b.instrument_revision -ne $instrumentRevision) {
        Write-Warning "baseline revision $($b.instrument_revision) differs from this script's $instrumentRevision"
    }
    $baseOk = ($b.verdict_latency_sample -eq $true)
    $held = $null
    if ($OutFile -and (Test-Path -LiteralPath $OutFile)) {
        $held = Get-Content -LiteralPath $OutFile -Raw | ConvertFrom-Json
    }
    $heldOk = ($held -and $held.verdict_latency_sample -eq $true)
    $ratio = if ($held) { [Math]::Round($held.elapsed_ms / [Math]::Max(1, $b.elapsed_ms), 2) } else { $null }
    $verdict =
        if (-not $baseOk -or ($held -and -not $heldOk)) { 'NOT-A-PAIR: an arm is not a latency sample (read its preconditions)' }
        elseif ($null -eq $held) { 'NO-HELD-ARM: pass -OutFile pointing at the held run JSON' }
        elseif ($ratio -ge 2.0) { "UV1-YES: the app fence waits on our packet (held/base = $ratio)" }
        else { "UV1-NO: hold attached and latency flat (held/base = $ratio) - say so loudly and stop" }
    $graded = [ordered]@{
        utc                 = (Get-Date).ToUniversalTime().ToString('o')
        instrument_revision = $instrumentRevision
        baseline            = $Baseline
        baseline_hold_ms    = $b.knobs.WddmHoldMs
        baseline_elapsed_ms = $b.elapsed_ms
        baseline_is_sample  = $baseOk
        held                = $OutFile
        held_hold_ms        = if ($held) { $held.knobs.WddmHoldMs } else { $null }
        held_elapsed_ms     = if ($held) { $held.elapsed_ms } else { $null }
        held_is_sample      = $heldOk
        ratio               = $ratio
        verdict             = $verdict
    }
    Write-JsonNoBom -Path ($OutFile + '.grade.json') -Object $graded
    Write-Output ($graded | ConvertTo-Json -Depth 6)
    if ($verdict -like 'UV1-*') { exit 0 } else { exit 1 }
}

# ── measurement mode ────────────────────────────────────────────────────────
# Precondition 1: no concurrent probe. The held counters are adapter-global and
# the FIFO is strictly head-of-line, so an overlapping arm contaminates both.
$running = Get-RunningProbes
if ($running.Count) {
    throw ("another D3D12 probe is running (pid(s) " + (($running | ForEach-Object { $_.Id }) -join ',') +
        "): the held counters are adapter-global, so a concurrent run would contaminate both arms")
}

$state = Get-Content 'C:\ProgramData\Helios\install-state.json' -Raw | ConvertFrom-Json
$umd12 = $state.installedDirect3D.UserModeDriverName.value[3]
if (-not (Test-Path -LiteralPath $umd12)) { throw "UMD12 not found: $umd12" }
$sha = (Get-FileHash -LiteralPath $umd12).Hash

$knobs = Read-Knobs
$countersBefore = Read-Counters
$archive = Join-Path 'C:\ProgramData\Helios\uv1-evidence' ('run-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))

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
# ⛔ BEFORE Dispose: after it $p.ExitCode is null, which is how every file from the
# first attempt recorded "exit_code": null and a FAIL read as exit 0.
$exitCode = $p.ExitCode
if (-not $finished) { & taskkill.exe /PID $p.Id /T /F | Out-Null; $p.WaitForExit(5000) | Out-Null }
$stdout = $stdoutTask.GetAwaiter().GetResult()
$stderr = $stderrTask.GetAwaiter().GetResult()
$p.Dispose()

Start-Sleep -Seconds 3   # let the PASSIVE counter mirror catch up
$countersAfter = Read-Counters

$firstLine = ($stdout -split "`r?`n" | Where-Object { $_.Trim() } | Select-Object -First 1)
$probeResult = 'UNKNOWN'
if ($firstLine -match '^PASS') { $probeResult = 'PASS' }
elseif ($firstLine -match '^FAIL') { $probeResult = 'FAIL' }
elseif ($firstLine -match '^BLOCKED') { $probeResult = 'BLOCKED' }

# Progress evidence a wall time cannot give: the probe archives one directory per
# executed case, and the last FAIL line names what stopped it.
$runDir = Get-ChildItem -LiteralPath $archive -Directory -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
$cases = @()
$lastCaseFail = $null
if ($runDir) {
    $cases = @(Get-ChildItem -LiteralPath $runDir.FullName -Directory -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Name)
    $failLine = Get-ChildItem -LiteralPath $runDir.FullName -Recurse -File -Filter 'stderr.txt' -ErrorAction SilentlyContinue |
        ForEach-Object { Select-String -LiteralPath $_.FullName -Pattern '^FAIL,' -ErrorAction SilentlyContinue } |
        Select-Object -Last 1
    if ($failLine) { $lastCaseFail = $failLine.Line.Trim() }
}

$deltas = [ordered]@{}
foreach ($name in $watched) {
    $before = if ($null -ne $countersBefore[$name]) { [int64]$countersBefore[$name] } else { 0 }
    $after = if ($null -ne $countersAfter[$name]) { [int64]$countersAfter[$name] } else { 0 }
    $deltas[$name] = $after - $before
}
$timeoutDelta = [int64]$deltas['WtOut'] + [int64]$deltas['FENCE_WAIT_TIMEOUTS']

$preconditions = [ordered]@{
    completed_without_timeout = $finished
    no_fence_wait_timeouts    = ($timeoutDelta -eq 0)
    diag_live                 = (([int64]$deltas['DpDrp'] + [int64]$deltas['AsDone']) -gt 0)
    ecl_path_live             = ([int64]$deltas['D12Rec'] -gt 0)
    hold_attached             = ([int64]$deltas['WfBHold'] -gt 0)
}
$isSample = $preconditions.completed_without_timeout -and
            $preconditions.no_fence_wait_timeouts -and
            $preconditions.diag_live -and
            $preconditions.ecl_path_live

$verdict =
    if (-not $preconditions.completed_without_timeout) { 'TIMEOUT-NOT-A-DATUM (elapsed is the timeout, not a latency)' }
    elseif (-not $preconditions.no_fence_wait_timeouts) { 'FENCE-WAIT-TIMEOUTS-NOT-A-DATUM (WtOut moved)' }
    elseif (-not $preconditions.diag_live) { 'NO-COUNTERS (DiagLevel did not take: reboot with DiagLevel=1)' }
    elseif (-not $preconditions.ecl_path_live) { 'NO-D3D12-ECL-SUBMISSION (D12Rec did not move)' }
    elseif ($knobs.WddmHoldMs -eq 0) { 'BASELINE (hold off: this is the arm to compare against)' }
    elseif (-not $preconditions.hold_attached) { 'HOLD-NEVER-ARMED (knob did not take, or no packet reached the head)' }
    else { 'HELD (grade with -Baseline: only a pair whose arms are both samples means anything)' }

$result = [ordered]@{
    utc                    = (Get-Date).ToUniversalTime().ToString('o')
    instrument_revision    = $instrumentRevision
    probe                  = $Probe
    host                   = $env:COMPUTERNAME
    session                = (Get-Process -Id $PID).SessionId
    boot_time              = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToString('o')
    uptime_min             = [Math]::Round(((Get-Date) - (Get-CimInstance Win32_OperatingSystem).LastBootUpTime).TotalMinutes, 2)
    package                = $state.packageId
    umd12                  = $umd12
    umd12_sha256           = $sha
    archive                = $archive
    knobs                  = $knobs
    elapsed_ms             = [int64]$sw.ElapsedMilliseconds
    finished               = $finished
    exit_code              = $exitCode
    probe_result           = $probeResult
    first_output           = $firstLine
    cases_completed        = $cases.Count
    cases                  = $cases
    last_case_fail         = $lastCaseFail
    counters_before        = $countersBefore
    counters_after         = $countersAfter
    deltas                 = $deltas
    preconditions          = $preconditions
    verdict                = $verdict
    verdict_latency_sample = $isSample
    stderr_tail            = (($stderr -split "`r?`n" | Select-Object -Last 6) -join "`n")
}
Write-JsonNoBom -Path $OutFile -Object $result

Write-Output ("UV1 probe={0} hold_ms={1} diag_level={2} session={3} uptime={4}min" -f $Probe, $knobs.WddmHoldMs, $knobs.DiagLevel, $result.session, $result.uptime_min)
Write-Output ("  elapsed_ms       = {0}" -f $sw.ElapsedMilliseconds)
Write-Output ("  probe_result     = {0} (exit {1}) cases_completed={2}" -f $probeResult, $exitCode, $cases.Count)
Write-Output ("  WfBHold delta    = {0}   WtOut delta = {1}" -f $deltas['WfBHold'], $timeoutDelta)
Write-Output ("  D12Rec delta     = {0}" -f $deltas['D12Rec'])
if ($lastCaseFail) { Write-Output ("  last FAIL line   = {0}" -f $lastCaseFail) }
Write-Output ("  preconditions    = {0}" -f (($preconditions.GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" }) -join ' '))
Write-Output ("  VERDICT          = {0}" -f $verdict)
Write-Output ("  json             = {0}" -f $OutFile)

if (-not $finished) { exit 3 }
if ($exitCode -ne 0 -and $probeResult -eq 'UNKNOWN') { exit 1 }
exit 0
