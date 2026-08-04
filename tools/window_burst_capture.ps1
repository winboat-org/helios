# Sample the composed pixels of one visible process's client area.
#
# This is an observation-only acceptance oracle. Process identity is used only
# to find the screen rectangle to sample; it is never used by the driver or UMD
# to classify a presentation resource. Run it in the interactive user session.
[CmdletBinding()]
param(
    [int]$Seconds = 75,
    [int]$IntervalMs = 100,
    [string]$ProcessName = '3DMarkICFWorkload',
    [string]$OutDir = 'C:\ProgramData\Helios\window-burst',
    [int]$MaxSavedBlack = 24,
    [int]$MaxSavedContent = 8,
    # Arm before Run: wait for the first visible target client before starting
    # the existing $Seconds capture window. Off preserves the former behavior.
    [switch]$WaitForFirstTarget,
    [int]$WaitTimeoutSeconds = 300,
    # Collect causal snapshots only after the target is first observed, so an
    # armed observer cannot age the bounded timeline before the workload starts.
    [switch]$CaptureCausalBaselines
)

$ErrorActionPreference = 'Continue'
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public static class HeliosWindowCaptureNative {
    public delegate bool EnumWindowsProc(IntPtr hwnd, IntPtr parameter);

    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }

    [StructLayout(LayoutKind.Sequential)]
    public struct POINT { public int X, Y; }

    [DllImport("user32.dll")]
    public static extern bool EnumWindows(EnumWindowsProc callback, IntPtr parameter);

    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr hwnd);

    [DllImport("user32.dll")]
    public static extern bool IsIconic(IntPtr hwnd);

    [DllImport("user32.dll")]
    public static extern bool IsWindow(IntPtr hwnd);

    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);

    [DllImport("user32.dll")]
    public static extern bool GetClientRect(IntPtr hwnd, out RECT rect);

    [DllImport("user32.dll")]
    public static extern bool ClientToScreen(IntPtr hwnd, ref POINT point);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetWindowTextW(IntPtr hwnd, StringBuilder text, int count);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetClassNameW(IntPtr hwnd, StringBuilder text, int count);

    public static string WindowText(IntPtr hwnd) {
        var text = new StringBuilder(512);
        GetWindowTextW(hwnd, text, text.Capacity);
        return text.ToString();
    }

    public static string WindowClass(IntPtr hwnd) {
        var text = new StringBuilder(256);
        GetClassNameW(hwnd, text, text.Capacity);
        return text.ToString();
    }

    public static IntPtr[] VisibleTopLevelWindows() {
        var result = new List<IntPtr>();
        EnumWindows((hwnd, parameter) => {
            if (IsWindowVisible(hwnd) && !IsIconic(hwnd)) result.Add(hwnd);
            return true;
        }, IntPtr.Zero);
        return result.ToArray();
    }

    public static IntPtr[] TopLevelWindows() {
        var result = new List<IntPtr>();
        EnumWindows((hwnd, parameter) => {
            result.Add(hwnd);
            return true;
        }, IntPtr.Zero);
        return result.ToArray();
    }
}
"@

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
Get-ChildItem -LiteralPath $OutDir -ErrorAction SilentlyContinue |
    Remove-Item -Force -ErrorAction SilentlyContinue
$csv = Join-Path $OutDir 'client.csv'
Set-Content -LiteralPath $csv -Value 't_ms,pid,hwnd,x,y,width,height,mean,min,max,signature,changed,hold_ms,black,saved'
$lifecycleCsv = Join-Path $OutDir 'lifecycle.csv'
Set-Content -LiteralPath $lifecycleCsv -Value 't_ms,event,pid,hwnd,visible,iconic,x,y,width,height,note'
$causalStatusCsv = Join-Path $OutDir 'causal-status.csv'
if ($CaptureCausalBaselines) {
    Set-Content -LiteralPath $causalStatusCsv -Value 't_ms,phase,tool,status,detail'
}

function Write-Lifecycle {
    param(
        [int64]$TimeMs,
        [string]$Event,
        [uint32]$TargetProcessId,
        [IntPtr]$Hwnd,
        [int]$Visible = 0,
        [int]$Iconic = 0,
        [int]$X = 0,
        [int]$Y = 0,
        [int]$Width = 0,
        [int]$Height = 0,
        [string]$Note = ''
    )
    $safeNote = $Note -replace '[\r\n,]', ' '
    Add-Content -LiteralPath $lifecycleCsv -Value (@(
        $TimeMs, $Event, $TargetProcessId, $Hwnd.ToInt64(), $Visible, $Iconic,
        $X, $Y, $Width, $Height, $safeNote
    ) -join ',')
}

function Get-TargetWindowLifecycleState {
    param([IntPtr]$Hwnd, [uint32]$TargetProcessId)

    $visible = [int][HeliosWindowCaptureNative]::IsWindowVisible($Hwnd)
    $iconic = [int][HeliosWindowCaptureNative]::IsIconic($Hwnd)
    $rect = New-Object HeliosWindowCaptureNative+RECT
    $topLeft = New-Object HeliosWindowCaptureNative+POINT
    $bottomRight = New-Object HeliosWindowCaptureNative+POINT
    $width = 0
    $height = 0
    if ([HeliosWindowCaptureNative]::GetClientRect($Hwnd, [ref]$rect)) {
        $bottomRight.X = $rect.Right
        $bottomRight.Y = $rect.Bottom
        if ([HeliosWindowCaptureNative]::ClientToScreen($Hwnd, [ref]$topLeft) -and
            [HeliosWindowCaptureNative]::ClientToScreen($Hwnd, [ref]$bottomRight)) {
            $width = $bottomRight.X - $topLeft.X
            $height = $bottomRight.Y - $topLeft.Y
        }
    }
    return [pscustomobject]@{
        Hwnd = $Hwnd
        Pid = $TargetProcessId
        Visible = $visible
        Iconic = $iconic
        X = $topLeft.X
        Y = $topLeft.Y
        Width = $width
        Height = $height
        Title = [HeliosWindowCaptureNative]::WindowText($Hwnd)
        Class = [HeliosWindowCaptureNative]::WindowClass($Hwnd)
    }
}

$knownTargetProcesses = @{}
$knownTargetWindows = @{}

function Update-TargetLifecycle {
    param([int64]$TimeMs)

    $currentProcesses = @{}
    foreach ($process in @(Get-Process -Name $ProcessName -ErrorAction SilentlyContinue)) {
        $pidKey = [string]$process.Id
        $currentProcesses[$pidKey] = $process
        if (-not $knownTargetProcesses.ContainsKey($pidKey)) {
            Write-Lifecycle -TimeMs $TimeMs -Event 'process_appear' -TargetProcessId ([Convert]::ToUInt32($process.Id)) -Hwnd ([IntPtr]::Zero) -Note $ProcessName
        }
    }
    foreach ($pidKey in @($knownTargetProcesses.Keys)) {
        if (-not $currentProcesses.ContainsKey($pidKey)) {
            Write-Lifecycle -TimeMs $TimeMs -Event 'process_exit' -TargetProcessId ([Convert]::ToUInt32($pidKey)) -Hwnd ([IntPtr]::Zero) -Note $ProcessName
        }
    }
    $script:knownTargetProcesses = $currentProcesses

    $seenWindows = @{}
    foreach ($hwnd in [HeliosWindowCaptureNative]::TopLevelWindows()) {
        [uint32]$windowPid = 0
        [void][HeliosWindowCaptureNative]::GetWindowThreadProcessId($hwnd, [ref]$windowPid)
        if (-not $windowPid -or -not $currentProcesses.ContainsKey([string]$windowPid)) { continue }
        $state = Get-TargetWindowLifecycleState -Hwnd $hwnd -TargetProcessId $windowPid
        $windowKey = '{0}:{1}' -f $windowPid, $hwnd.ToInt64()
        $seenWindows[$windowKey] = $state
        if (-not $knownTargetWindows.ContainsKey($windowKey)) {
            Write-Lifecycle -TimeMs $TimeMs -Event 'window_create' -TargetProcessId $windowPid -Hwnd $hwnd `
                -Visible $state.Visible -Iconic $state.Iconic -X $state.X -Y $state.Y `
                -Width $state.Width -Height $state.Height `
                -Note ("class={0};title={1}" -f $state.Class, $state.Title)
            continue
        }
        $previous = $knownTargetWindows[$windowKey]
        if ($previous.Visible -ne $state.Visible) {
            $event = if ($state.Visible) { 'window_show' } else { 'window_hide' }
            Write-Lifecycle -TimeMs $TimeMs -Event $event -TargetProcessId $windowPid -Hwnd $hwnd `
                -Visible $state.Visible -Iconic $state.Iconic -X $state.X -Y $state.Y `
                -Width $state.Width -Height $state.Height `
                -Note ("class={0};title={1}" -f $state.Class, $state.Title)
        }
        if ($previous.Iconic -ne $state.Iconic) {
            $event = if ($state.Iconic) { 'window_iconic' } else { 'window_restore' }
            Write-Lifecycle -TimeMs $TimeMs -Event $event -TargetProcessId $windowPid -Hwnd $hwnd `
                -Visible $state.Visible -Iconic $state.Iconic -X $state.X -Y $state.Y `
                -Width $state.Width -Height $state.Height `
                -Note ("class={0};title={1}" -f $state.Class, $state.Title)
        }
    }
    foreach ($windowKey in @($knownTargetWindows.Keys)) {
        if ($seenWindows.ContainsKey($windowKey)) { continue }
        $previous = $knownTargetWindows[$windowKey]
        $event = if ([HeliosWindowCaptureNative]::IsWindow($previous.Hwnd)) {
            'window_disappear'
        } else {
            'window_destroy'
        }
        Write-Lifecycle -TimeMs $TimeMs -Event $event -TargetProcessId $previous.Pid -Hwnd $previous.Hwnd `
            -Visible $previous.Visible -Iconic $previous.Iconic -X $previous.X -Y $previous.Y `
            -Width $previous.Width -Height $previous.Height `
            -Note ("class={0};title={1}" -f $previous.Class, $previous.Title)
    }
    $script:knownTargetWindows = $seenWindows
    return [pscustomobject]@{
        ProcessCount = $currentProcesses.Count
        WindowCount = $seenWindows.Count
    }
}

function Write-CausalStatus {
    param([int64]$TimeMs, [string]$Phase, [string]$Tool, [string]$Status, [string]$Detail = '')
    if (-not $CaptureCausalBaselines) { return }
    Add-Content -LiteralPath $causalStatusCsv -Value (@(
        $TimeMs, $Phase, $Tool, $Status, ($Detail -replace '[\r\n,]', ' ')
    ) -join ',')
}

$timelineTool = 'C:\ProgramData\Helios\scanout_timeline_dump.exe'
$ledgerTool = 'C:\ProgramData\Helios\read_ledger_dump.exe'
$counterTool = 'Z:\tools\kmd-counter-snapshot.ps1'
$causalErrorLog = Join-Path $OutDir 'causal-errors.txt'
$causalCounterLog = Join-Path $OutDir 'causal-counter-output.txt'
$causalPreCursor = $null
$causalPreTaken = $false

function Get-TimelineCursor {
    param([int64]$TimeMs, [string]$Phase)
    if (-not (Test-Path -LiteralPath $timelineTool)) {
        Write-CausalStatus -TimeMs $TimeMs -Phase $Phase -Tool 'timeline_cursor' -Status 'missing' -Detail $timelineTool
        return $null
    }
    try {
        $output = & $timelineTool --cursor 2>> $causalErrorLog
        if ($LASTEXITCODE -ne 0) {
            Write-CausalStatus -TimeMs $TimeMs -Phase $Phase -Tool 'timeline_cursor' -Status 'failed' -Detail "exit=$LASTEXITCODE"
            return $null
        }
        $text = (($output | Out-String).Trim())
        [uint64]$cursor = 0
        if (-not [uint64]::TryParse($text, [ref]$cursor)) {
            Write-CausalStatus -TimeMs $TimeMs -Phase $Phase -Tool 'timeline_cursor' -Status 'invalid' -Detail $text
            return $null
        }
        Set-Content -LiteralPath (Join-Path $OutDir ("timeline-{0}-cursor.txt" -f $Phase)) -Value $cursor
        Write-CausalStatus -TimeMs $TimeMs -Phase $Phase -Tool 'timeline_cursor' -Status 'ok' -Detail $cursor
        return $cursor
    } catch {
        Write-CausalStatus -TimeMs $TimeMs -Phase $Phase -Tool 'timeline_cursor' -Status 'exception' -Detail $_.Exception.Message
        return $null
    }
}

function Invoke-CausalSnapshot {
    param([int64]$TimeMs, [string]$Phase)
    if (-not $CaptureCausalBaselines) { return }

    $cursor = Get-TimelineCursor -TimeMs $TimeMs -Phase $Phase
    if ($Phase -eq 'pre') { $script:causalPreCursor = $cursor }

    if (Test-Path -LiteralPath $ledgerTool) {
        try {
            & $ledgerTool 2>> $causalErrorLog | Set-Content -LiteralPath (Join-Path $OutDir ("read-ledger-{0}.csv" -f $Phase))
            $status = if ($LASTEXITCODE -eq 0) { 'ok' } else { 'failed' }
            Write-CausalStatus -TimeMs $TimeMs -Phase $Phase -Tool 'read_ledger' -Status $status -Detail "exit=$LASTEXITCODE"
        } catch {
            Write-CausalStatus -TimeMs $TimeMs -Phase $Phase -Tool 'read_ledger' -Status 'exception' -Detail $_.Exception.Message
        }
    } else {
        Write-CausalStatus -TimeMs $TimeMs -Phase $Phase -Tool 'read_ledger' -Status 'missing' -Detail $ledgerTool
    }

    if (Test-Path -LiteralPath $counterTool) {
        try {
            & $counterTool -Label ("window-burst-{0}" -f $Phase) -OutDir $OutDir `
                2>> $causalErrorLog 6>> $causalCounterLog | Out-Null
            $status = if ($LASTEXITCODE -eq 0) { 'ok' } else { 'failed' }
            Write-CausalStatus -TimeMs $TimeMs -Phase $Phase -Tool 'kmd_counters' -Status $status -Detail "exit=$LASTEXITCODE"
        } catch {
            Write-CausalStatus -TimeMs $TimeMs -Phase $Phase -Tool 'kmd_counters' -Status 'exception' -Detail $_.Exception.Message
        }
    } else {
        Write-CausalStatus -TimeMs $TimeMs -Phase $Phase -Tool 'kmd_counters' -Status 'missing' -Detail $counterTool
    }
}

function Complete-CausalCapture {
    param([int64]$TimeMs)
    if (-not $CaptureCausalBaselines -or -not $causalPreTaken) { return }
    Invoke-CausalSnapshot -TimeMs $TimeMs -Phase 'post'
    if ($null -eq $causalPreCursor) { return }
    $postText = Get-Content -LiteralPath (Join-Path $OutDir 'timeline-post-cursor.txt') -ErrorAction SilentlyContinue | Select-Object -First 1
    [uint64]$postCursor = 0
    if (-not [uint64]::TryParse($postText, [ref]$postCursor) -or $postCursor -le $causalPreCursor) {
        Write-CausalStatus -TimeMs $TimeMs -Phase 'post' -Tool 'timeline_dump' -Status 'empty_or_invalid' -Detail "pre=$causalPreCursor post=$postText"
        return
    }
    try {
        & $timelineTool --dump ($causalPreCursor + 1) $postCursor 2>> $causalErrorLog |
            Set-Content -LiteralPath (Join-Path $OutDir 'scanout-timeline.csv')
        $status = if ($LASTEXITCODE -eq 0) { 'ok' } else { 'failed' }
        Write-CausalStatus -TimeMs $TimeMs -Phase 'post' -Tool 'timeline_dump' -Status $status -Detail "first=$($causalPreCursor + 1) last=$postCursor exit=$LASTEXITCODE"
    } catch {
        Write-CausalStatus -TimeMs $TimeMs -Phase 'post' -Tool 'timeline_dump' -Status 'exception' -Detail $_.Exception.Message
    }
}

function Find-TargetWindow {
    foreach ($hwnd in [HeliosWindowCaptureNative]::VisibleTopLevelWindows()) {
        [uint32]$targetPid = 0
        [void][HeliosWindowCaptureNative]::GetWindowThreadProcessId($hwnd, [ref]$targetPid)
        if (-not $targetPid) { continue }
        try { $process = Get-Process -Id $targetPid -ErrorAction Stop } catch { continue }
        if ($process.ProcessName -ne $ProcessName) { continue }

        $rect = New-Object HeliosWindowCaptureNative+RECT
        if (-not [HeliosWindowCaptureNative]::GetClientRect($hwnd, [ref]$rect)) { continue }
        $topLeft = New-Object HeliosWindowCaptureNative+POINT
        $bottomRight = New-Object HeliosWindowCaptureNative+POINT
        $bottomRight.X = $rect.Right
        $bottomRight.Y = $rect.Bottom
        if (-not [HeliosWindowCaptureNative]::ClientToScreen($hwnd, [ref]$topLeft)) { continue }
        if (-not [HeliosWindowCaptureNative]::ClientToScreen($hwnd, [ref]$bottomRight)) { continue }
        $width = $bottomRight.X - $topLeft.X
        $height = $bottomRight.Y - $topLeft.Y
        if ($width -lt 64 -or $height -lt 64) { continue }
        return [pscustomobject]@{
            Hwnd = $hwnd
            Pid = $targetPid
            X = $topLeft.X
            Y = $topLeft.Y
            Width = $width
            Height = $height
        }
    }
    return $null
}

$bitmap = $null
$graphics = $null
$bitmapWidth = 0
$bitmapHeight = 0
$lastSignature = $null
$lastChangeMs = 0
$savedBlack = 0
$savedContent = 0
$samples = 0
$missing = 0
$stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
$captureStarted = -not $WaitForFirstTarget
$captureStartMs = 0
$waitTimedOut = $false

try {
    while ($true) {
        $sampleStart = [int64]$stopwatch.Elapsed.TotalMilliseconds
        $lifecycle = Update-TargetLifecycle -TimeMs $sampleStart
        if ($CaptureCausalBaselines -and -not $causalPreTaken -and $lifecycle.ProcessCount -gt 0) {
            # The first target process is the earliest external lifecycle edge
            # available to this observer; it is deliberately not a resource
            # identity and only arms the read-only causal baselines.
            $causalPreTaken = $true
            Invoke-CausalSnapshot -TimeMs $sampleStart -Phase 'pre'
        }
        $window = Find-TargetWindow

        if (-not $captureStarted) {
            if ($null -ne $window) {
                $captureStarted = $true
                $captureStartMs = $sampleStart
            } elseif ($sampleStart -ge ([int64]$WaitTimeoutSeconds * 1000)) {
                $waitTimedOut = $true
                break
            } else {
                Start-Sleep -Milliseconds ([Math]::Max(50, $IntervalMs))
                continue
            }
        }
        if (($sampleStart - $captureStartMs) -ge ([int64]$Seconds * 1000)) {
            break
        }
        if ($null -eq $window) {
            $missing++
            Start-Sleep -Milliseconds ([Math]::Max(50, $IntervalMs))
            continue
        }

        if ($bitmapWidth -ne $window.Width -or $bitmapHeight -ne $window.Height) {
            if ($null -ne $graphics) { $graphics.Dispose(); $graphics = $null }
            if ($null -ne $bitmap) { $bitmap.Dispose(); $bitmap = $null }
            $bitmap = [System.Drawing.Bitmap]::new([int]$window.Width, [int]$window.Height)
            $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
            $bitmapWidth = $window.Width
            $bitmapHeight = $window.Height
            $lastSignature = $null
            $lastChangeMs = $sampleStart
        }

        try {
            $graphics.CopyFromScreen($window.X, $window.Y, 0, 0, $bitmap.Size)
        } catch {
            Start-Sleep -Milliseconds ([Math]::Max(50, $IntervalMs))
            continue
        }

        # A 31x17 interior grid is enough to distinguish a held image from a
        # changing one without making this observer a material workload.
        $columns = 32
        $rows = 18
        [int64]$sum = 0
        [int64]$signature = 0
        $minimum = 255
        $maximum = 0
        $index = 1
        for ($column = 1; $column -lt $columns; $column++) {
            for ($row = 1; $row -lt $rows; $row++) {
                $x = [int]($bitmapWidth * $column / $columns)
                $y = [int]($bitmapHeight * $row / $rows)
                $pixel = $bitmap.GetPixel($x, $y)
                $luma = [int](($pixel.R + $pixel.G + $pixel.B) / 3)
                $sum += $luma
                # Two spatially weighted color terms keep this deterministic
                # and safely inside Int64 for the bounded grid.
                $signature += [int64]$index * ($pixel.R + 3 * $pixel.G + 7 * $pixel.B)
                if ($luma -lt $minimum) { $minimum = $luma }
                if ($luma -gt $maximum) { $maximum = $luma }
                $index++
            }
        }
        $pointCount = ($columns - 1) * ($rows - 1)
        $mean = [int]($sum / $pointCount)
        $changed = if ($null -eq $lastSignature -or $signature -ne $lastSignature) { 1 } else { 0 }
        if ($changed) { $lastChangeMs = $sampleStart }
        $holdMs = $sampleStart - $lastChangeMs
        $lastSignature = $signature

        # A genuinely black client has no sampled highlight. Mean alone would
        # misclassify the intentionally dark parts of Combined.
        $black = if ($maximum -le 8) { 1 } else { 0 }
        $saved = 0
        if ($black -and $savedBlack -lt $MaxSavedBlack) {
            $bitmap.Save(
                (Join-Path $OutDir ("black_{0:d6}_{1:d3}.png" -f $sampleStart, $mean)),
                [System.Drawing.Imaging.ImageFormat]::Png)
            $savedBlack++
            $saved = 1
        } elseif (-not $black -and $savedContent -lt $MaxSavedContent) {
            $bitmap.Save(
                (Join-Path $OutDir ("content_{0:d6}_{1:d3}.png" -f $sampleStart, $mean)),
                [System.Drawing.Imaging.ImageFormat]::Png)
            $savedContent++
            $saved = 1
        }

        $line = @(
            ($sampleStart - $captureStartMs), $window.Pid, $window.Hwnd.ToInt64(), $window.X, $window.Y,
            $window.Width, $window.Height, $mean, $minimum, $maximum, $signature,
            $changed, $holdMs, $black, $saved
        ) -join ','
        Add-Content -LiteralPath $csv -Value $line
        $samples++

        $elapsed = [int64]$stopwatch.Elapsed.TotalMilliseconds - $sampleStart
        $remaining = $IntervalMs - $elapsed
        if ($remaining -gt 0) { Start-Sleep -Milliseconds $remaining }
    }
} finally {
    Complete-CausalCapture -TimeMs ([int64]$stopwatch.Elapsed.TotalMilliseconds)
    if ($null -ne $graphics) { $graphics.Dispose() }
    if ($null -ne $bitmap) { $bitmap.Dispose() }
}

@(
    "samples=$samples"
    "missing_window_polls=$missing"
    "saved_black=$savedBlack"
    "saved_content=$savedContent"
    "csv=$csv"
    "lifecycle_csv=$lifecycleCsv"
    "wait_for_first_target=$(if ($WaitForFirstTarget) { 1 } else { 0 })"
    "wait_timed_out=$([int]$waitTimedOut)"
    "causal_baselines=$(if ($CaptureCausalBaselines) { 1 } else { 0 })"
) | Set-Content -LiteralPath (Join-Path $OutDir 'summary.txt')
