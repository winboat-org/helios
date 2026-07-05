# enum_all_windows.ps1 — dump EVERY top-level window (visible or not) in the console
# session: hwnd, class, title, pid+process, visible, rect. Run via the helios_desk_probe
# scheduled-task mechanism (session 1). Output: C:\ProgramData\Helios\all_windows.txt
$out = 'C:\ProgramData\Helios\all_windows.txt'
$sb = [System.Text.StringBuilder]::new()
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class WinEnum {
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lp);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    public delegate bool EnumProc(IntPtr h, IntPtr lp);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@
$procs = @{}
Get-Process | ForEach-Object { $procs[[uint32]$_.Id] = $_.ProcessName }
[void]$sb.AppendLine("=== all top-level windows $(Get-Date -Format o) session=$([System.Diagnostics.Process]::GetCurrentProcess().SessionId) ===")
$cb = [WinEnum+EnumProc]{ param($h, $lp)
    $r = New-Object WinEnum+RECT
    [void][WinEnum]::GetWindowRect($h, [ref]$r)
    $cls = [System.Text.StringBuilder]::new(256); [void][WinEnum]::GetClassName($h, $cls, 256)
    $txt = [System.Text.StringBuilder]::new(256); [void][WinEnum]::GetWindowText($h, $txt, 256)
    $wpid = [uint32]0; [void][WinEnum]::GetWindowThreadProcessId($h, [ref]$wpid)
    $pname = $procs[$wpid]; if (-not $pname) { $pname = '?' }
    $vis = if ([WinEnum]::IsWindowVisible($h)) { 'VIS' } else { '   ' }
    [void]$sb.AppendLine(("0x{0,-10:x} {1} pid={2,-6} {3,-24} cls='{4}' title='{5}' rect=({6},{7})-({8},{9})" -f `
        $h.ToInt64(), $vis, $wpid, $pname, $cls, $txt, $r.L, $r.T, $r.R, $r.B))
    return $true }
[void][WinEnum]::EnumWindows($cb, [IntPtr]::Zero)
[System.IO.File]::WriteAllText($out, $sb.ToString())
