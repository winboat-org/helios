# desktop_repaint_probe.ps1 — find Progman via EnumWindows (FindWindow is broken for it
# on this box), then force a full synchronous repaint of the desktop tree. Run in
# session 1 via scheduled task. Output: C:\ProgramData\Helios\desktop_repaint.txt
$out = 'C:\ProgramData\Helios\desktop_repaint.txt'
$sb = [System.Text.StringBuilder]::new()
function W([string]$s) { [void]$sb.AppendLine($s) }
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class Rep {
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lp);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern bool RedrawWindow(IntPtr h, IntPtr rc, IntPtr rgn, uint flags);
    [DllImport("user32.dll")] public static extern bool InvalidateRect(IntPtr h, IntPtr r, bool erase);
    [DllImport("user32.dll")] public static extern bool UpdateWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr SendMessageTimeoutW(IntPtr h, uint msg, UIntPtr wp, IntPtr lp, uint flags, uint timeout, out UIntPtr res);
    public delegate bool EnumProc(IntPtr h, IntPtr lp);
}
"@
W "=== desktop_repaint $(Get-Date -Format o) ==="
$script:progman = [IntPtr]::Zero
$cb = [Rep+EnumProc]{ param($h, $lp)
    $cls = [System.Text.StringBuilder]::new(256); [void][Rep]::GetClassName($h, $cls, 256)
    if ($cls.ToString() -eq 'Progman') { $script:progman = $h }
    return $true }
[void][Rep]::EnumWindows($cb, [IntPtr]::Zero)
W ("Progman = 0x{0:x}" -f $script:progman.ToInt64())
if ($script:progman -ne [IntPtr]::Zero) {
    # RDW_INVALIDATE|RDW_ERASE|RDW_ALLCHILDREN|RDW_UPDATENOW|RDW_ERASENOW|RDW_FRAME = 0x785
    $ok = [Rep]::RedrawWindow($script:progman, [IntPtr]::Zero, [IntPtr]::Zero, 0x785)
    W "RedrawWindow(Progman, full sync) => $ok"
    # F5-equivalent: tell DefView to refresh (WM_COMMAND 0x7103 is fragile; skip).
    # Also ask for an erase+paint cycle explicitly.
    [void][Rep]::InvalidateRect($script:progman, [IntPtr]::Zero, $true)
    [void][Rep]::UpdateWindow($script:progman)
    W "InvalidateRect+UpdateWindow done"
}
Start-Sleep -Seconds 2
W "=== end $(Get-Date -Format o) ==="
[System.IO.File]::WriteAllText($out, $sb.ToString())
