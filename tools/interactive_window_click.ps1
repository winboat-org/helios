[CmdletBinding()]
param(
    [string]$ProcessName = '3DMark',
    [Parameter(Mandatory = $true)]
    [string]$ClientPoints,
    [int]$BetweenMs = 750,
    [string]$SendKeys = '',
    [string]$LogFile = 'C:\ProgramData\Helios\interactive-window-click.txt'
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public static class HeliosInteractiveClickNative {
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
    public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);

    [DllImport("user32.dll")]
    public static extern bool GetClientRect(IntPtr hwnd, out RECT rect);

    [DllImport("user32.dll")]
    public static extern bool ClientToScreen(IntPtr hwnd, ref POINT point);

    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr hwnd);

    [DllImport("user32.dll")]
    public static extern bool SetCursorPos(int x, int y);

    [DllImport("user32.dll")]
    public static extern void mouse_event(uint flags, int dx, int dy, uint data, IntPtr extra);

    [DllImport("user32.dll")]
    public static extern IntPtr WindowFromPoint(POINT point);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetWindowTextW(IntPtr hwnd, StringBuilder text, int count);

    public static string WindowText(IntPtr hwnd) {
        var text = new StringBuilder(512);
        GetWindowTextW(hwnd, text, text.Capacity);
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
}
"@

$processIds = @{}
foreach ($process in @(Get-Process -Name $ProcessName -ErrorAction SilentlyContinue)) {
    $processIds[[uint32]$process.Id] = $true
}
if ($processIds.Count -eq 0) {
    throw "No $ProcessName process is running."
}

$target = [IntPtr]::Zero
$largestArea = 0L
foreach ($hwnd in [HeliosInteractiveClickNative]::VisibleTopLevelWindows()) {
    [uint32]$windowProcessId = 0
    [void][HeliosInteractiveClickNative]::GetWindowThreadProcessId($hwnd, [ref]$windowProcessId)
    if (-not $processIds.ContainsKey($windowProcessId)) { continue }
    $rect = New-Object HeliosInteractiveClickNative+RECT
    if (-not [HeliosInteractiveClickNative]::GetClientRect($hwnd, [ref]$rect)) { continue }
    $area = [int64]($rect.Right - $rect.Left) * [int64]($rect.Bottom - $rect.Top)
    if ($area -gt $largestArea) {
        $largestArea = $area
        $target = $hwnd
    }
}
if ($target -eq [IntPtr]::Zero) {
    throw "No visible top-level $ProcessName window was found."
}

$clientRect = New-Object HeliosInteractiveClickNative+RECT
if (-not [HeliosInteractiveClickNative]::GetClientRect($target, [ref]$clientRect)) {
    throw 'GetClientRect failed.'
}
$origin = New-Object HeliosInteractiveClickNative+POINT
if (-not [HeliosInteractiveClickNative]::ClientToScreen($target, [ref]$origin)) {
    throw 'ClientToScreen failed.'
}

$log = New-Object System.Collections.Generic.List[string]
$log.Add("time=$([DateTime]::Now.ToString('o'))")
$log.Add("process=$ProcessName hwnd=$($target.ToInt64()) title=$([HeliosInteractiveClickNative]::WindowText($target))")
$log.Add("client_origin=$($origin.X),$($origin.Y) client_size=$($clientRect.Right - $clientRect.Left)x$($clientRect.Bottom - $clientRect.Top)")
[void][HeliosInteractiveClickNative]::SetForegroundWindow($target)

$leftDown = 0x0002
$leftUp = 0x0004
foreach ($pointText in $ClientPoints.Split(';')) {
    $components = $pointText.Split(',')
    if ($components.Count -ne 2) {
        throw "Invalid client point '$pointText'."
    }
    $clientX = [int]$components[0]
    $clientY = [int]$components[1]
    if ($clientX -lt 0 -or $clientX -ge ($clientRect.Right - $clientRect.Left) -or
        $clientY -lt 0 -or $clientY -ge ($clientRect.Bottom - $clientRect.Top)) {
        throw "Client point '$pointText' is outside the target client."
    }
    $screenX = $origin.X + $clientX
    $screenY = $origin.Y + $clientY
    $probe = New-Object HeliosInteractiveClickNative+POINT
    $probe.X = $screenX
    $probe.Y = $screenY
    $hit = [HeliosInteractiveClickNative]::WindowFromPoint($probe)
    $log.Add("click client=$clientX,$clientY screen=$screenX,$screenY hit=$($hit.ToInt64()) title=$([HeliosInteractiveClickNative]::WindowText($hit))")
    [void][HeliosInteractiveClickNative]::SetCursorPos($screenX, $screenY)
    [HeliosInteractiveClickNative]::mouse_event($leftDown, 0, 0, 0, [IntPtr]::Zero)
    [HeliosInteractiveClickNative]::mouse_event($leftUp, 0, 0, 0, [IntPtr]::Zero)
    if ($BetweenMs -gt 0) {
        Start-Sleep -Milliseconds $BetweenMs
    }
}

if ($SendKeys.Length -ne 0) {
    $log.Add("send_keys=$SendKeys")
    [System.Windows.Forms.SendKeys]::SendWait($SendKeys)
}

$parent = Split-Path -Parent $LogFile
if ($parent) {
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
}
[System.IO.File]::WriteAllLines($LogFile, $log)
