# desktop_window_probe.ps1 — session-1 (console) desktop-window forensics for the
# black-desktop-plate investigation (15th session, 2026-07-05).
#
# MUST run inside the interactive console session (session 1): SSH/win_exec lands in
# session 0 whose window station has no desktop windows. Run via a scheduled task:
#   schtasks /create /f /tn helios_desk_probe /sc once /st 00:00 /it /rl highest ^
#     /tr "powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\desktop_window_probe.ps1"
#   schtasks /run /tn helios_desk_probe
# Output: C:\ProgramData\Helios\desktop_window_probe.txt
#
# What it answers:
#  1. Do Progman / WorkerW / SHELLDLL_DefView / SysListView32 exist, where are their
#     rects, are they visible? (icons + wallpaper + solid color are all GDI-painted
#     into this window tree; if it is mispositioned or invisible, win32k never asks
#     it to paint and its redirection surface stays all-zero)
#  2. What monitors/virtual-screen does USER32 believe exist?
#  3. Force a full desktop repaint (RedrawWindow on Progman + UpdatePerUserSystemParameters)
#     so the KMD GdiE counter can be diffed before/after from outside.

$out = 'C:\ProgramData\Helios\desktop_window_probe.txt'
$sb = [System.Text.StringBuilder]::new()
function W([string]$s) { [void]$sb.AppendLine($s) }

W "=== desktop_window_probe $(Get-Date -Format o) session=$([System.Diagnostics.Process]::GetCurrentProcess().SessionId) ==="

Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class DeskProbe {
    [DllImport("user32.dll")] public static extern IntPtr GetDesktopWindow();
    [DllImport("user32.dll")] public static extern IntPtr FindWindow(string cls, string win);
    [DllImport("user32.dll")] public static extern IntPtr FindWindowEx(IntPtr parent, IntPtr after, string cls, string win);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lp);
    [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr h, EnumProc cb, IntPtr lp);
    [DllImport("user32.dll")] public static extern bool RedrawWindow(IntPtr h, IntPtr rc, IntPtr rgn, uint flags);
    [DllImport("user32.dll")] public static extern bool InvalidateRect(IntPtr h, IntPtr r, bool erase);
    [DllImport("user32.dll", CharSet=CharSet.Auto)] public static extern int SystemParametersInfo(int act, int p, StringBuilder v, int ini);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    public delegate bool EnumProc(IntPtr h, IntPtr lp);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@

function Describe([IntPtr]$h, [string]$tag) {
    if ($h -eq [IntPtr]::Zero) { W "$tag : NOT FOUND"; return }
    $r = New-Object DeskProbe+RECT
    [void][DeskProbe]::GetWindowRect($h, [ref]$r)
    $cls = [System.Text.StringBuilder]::new(256); [void][DeskProbe]::GetClassName($h, $cls, 256)
    $txt = [System.Text.StringBuilder]::new(256); [void][DeskProbe]::GetWindowText($h, $txt, 256)
    $pid2 = 0; [void][DeskProbe]::GetWindowThreadProcessId($h, [ref]$pid2)
    $vis = [DeskProbe]::IsWindowVisible($h)
    W ("{0} : hwnd=0x{1:x} class='{2}' title='{3}' pid={4} visible={5} rect=({6},{7})-({8},{9}) {10}x{11}" -f `
        $tag, $h.ToInt64(), $cls, $txt, $pid2, $vis, $r.L, $r.T, $r.R, $r.B, ($r.R-$r.L), ($r.B-$r.T))
}

W "--- virtual screen ---"
W ("SM_CXSCREEN x SM_CYSCREEN     = {0}x{1}" -f [DeskProbe]::GetSystemMetrics(0), [DeskProbe]::GetSystemMetrics(1))
W ("SM_XVIRTUALSCREEN,Y / CX,CY   = {0},{1} / {2}x{3}" -f [DeskProbe]::GetSystemMetrics(76), [DeskProbe]::GetSystemMetrics(77), [DeskProbe]::GetSystemMetrics(78), [DeskProbe]::GetSystemMetrics(79))
W ("SM_CMONITORS                  = {0}" -f [DeskProbe]::GetSystemMetrics(80))
W ("SM_REMOTESESSION              = {0}" -f [DeskProbe]::GetSystemMetrics(4096))

W "--- monitors (System.Windows.Forms.Screen) ---"
Add-Type -AssemblyName System.Windows.Forms
foreach ($s in [System.Windows.Forms.Screen]::AllScreens) {
    W ("screen dev='{0}' primary={1} bounds={2} working={3}" -f $s.DeviceName, $s.Primary, $s.Bounds, $s.WorkingArea)
}

W "--- desktop window tree ---"
Describe ([DeskProbe]::GetDesktopWindow()) "GetDesktopWindow"
$progman = [DeskProbe]::FindWindow('Progman', $null)
Describe $progman "Progman"
if ($progman -ne [IntPtr]::Zero) {
    $defview = [DeskProbe]::FindWindowEx($progman, [IntPtr]::Zero, 'SHELLDLL_DefView', $null)
    Describe $defview "Progman/SHELLDLL_DefView"
    if ($defview -ne [IntPtr]::Zero) {
        Describe ([DeskProbe]::FindWindowEx($defview, [IntPtr]::Zero, 'SysListView32', $null)) "Progman/DefView/SysListView32"
    }
}
# WorkerW windows (wallpaper host after a WorkerW split) are top-level siblings.
W "--- all WorkerW top-levels (and whether DefView lives in one) ---"
$script:workerNote = ''
$enumCb = [DeskProbe+EnumProc]{ param($h, $lp)
    $cls = [System.Text.StringBuilder]::new(256); [void][DeskProbe]::GetClassName($h, $cls, 256)
    if ($cls.ToString() -eq 'WorkerW') {
        Describe $h "WorkerW"
        $dv = [DeskProbe]::FindWindowEx($h, [IntPtr]::Zero, 'SHELLDLL_DefView', $null)
        if ($dv -ne [IntPtr]::Zero) {
            Describe $dv "WorkerW/SHELLDLL_DefView"
            Describe ([DeskProbe]::FindWindowEx($dv, [IntPtr]::Zero, 'SysListView32', $null)) "WorkerW/DefView/SysListView32"
        }
    }
    return $true }
[void][DeskProbe]::EnumWindows($enumCb, [IntPtr]::Zero)

W "--- SPI_GETDESKWALLPAPER ---"
$wp = [System.Text.StringBuilder]::new(512)
[void][DeskProbe]::SystemParametersInfo(0x0073, 512, $wp, 0)
W ("wallpaper path = '{0}'" -f $wp.ToString())

W "--- forcing desktop repaint (RedrawWindow Progman: INVALIDATE|ERASE|ERASENOW|UPDATENOW|ALLCHILDREN|FRAME) ---"
if ($progman -ne [IntPtr]::Zero) {
    # RDW_INVALIDATE 0x1 | RDW_ERASE 0x4 | RDW_ALLCHILDREN 0x80 | RDW_UPDATENOW 0x100 | RDW_ERASENOW 0x200 | RDW_FRAME 0x400
    $ok = [DeskProbe]::RedrawWindow($progman, [IntPtr]::Zero, [IntPtr]::Zero, 0x785)
    W "RedrawWindow(Progman) => $ok"
}
# Also poke the shell to re-apply per-user system params (re-sets wallpaper/color).
try { rundll32.exe user32.dll,UpdatePerUserSystemParameters ; W "UpdatePerUserSystemParameters invoked" } catch { W "UPUSP failed: $_" }

Start-Sleep -Seconds 2
W "=== end $(Get-Date -Format o) ==="
[System.IO.File]::WriteAllText($out, $sb.ToString())
