# desktop_state_probe.ps1 — is win32k's LIVE desktop state (background color, wallpaper,
# icon count) what the registry says it should be? And does poking it live fix the paint?
# Run in session 1 via scheduled task. Output: C:\ProgramData\Helios\desktop_state.txt
$out = 'C:\ProgramData\Helios\desktop_state.txt'
$sb = [System.Text.StringBuilder]::new()
function W([string]$s) { [void]$sb.AppendLine($s) }
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class DState {
    [DllImport("user32.dll")] public static extern uint GetSysColor(int i);
    [DllImport("user32.dll", SetLastError=true)] public static extern bool SetSysColors(int n, int[] elements, uint[] colors);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lp);
    [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr h, EnumProc cb, IntPtr lp);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr h, uint msg, IntPtr wp, IntPtr lp);
    [DllImport("user32.dll", SetLastError=true)] public static extern int SystemParametersInfoW(int act, int p, string v, int ini);
    [DllImport("user32.dll")] public static extern bool RedrawWindow(IntPtr h, IntPtr rc, IntPtr rgn, uint flags);
    public delegate bool EnumProc(IntPtr h, IntPtr lp);
}
"@
W "=== desktop_state $(Get-Date -Format o) ==="
# COLOR_BACKGROUND = 1 (desktop color)
$c = [DState]::GetSysColor(1)
W ("GetSysColor(COLOR_BACKGROUND) = 0x{0:x6} (R={1} G={2} B={3}) -- registry says R=232 G=17 B=35" -f $c, ($c -band 0xff), (($c -shr 8) -band 0xff), (($c -shr 16) -band 0xff))

# find Progman + SysListView32
$script:progman = [IntPtr]::Zero; $script:listview = [IntPtr]::Zero
$cb = [DState+EnumProc]{ param($h, $lp)
    $cls = [System.Text.StringBuilder]::new(256); [void][DState]::GetClassName($h, $cls, 256)
    if ($cls.ToString() -eq 'Progman') { $script:progman = $h }
    return $true }
[void][DState]::EnumWindows($cb, [IntPtr]::Zero)
if ($script:progman -ne [IntPtr]::Zero) {
    $cb2 = [DState+EnumProc]{ param($h, $lp)
        $cls = [System.Text.StringBuilder]::new(256); [void][DState]::GetClassName($h, $cls, 256)
        if ($cls.ToString() -eq 'SysListView32') { $script:listview = $h }
        return $true }
    [void][DState]::EnumChildWindows($script:progman, $cb2, [IntPtr]::Zero)
}
W ("Progman=0x{0:x} SysListView32=0x{1:x}" -f $script:progman.ToInt64(), $script:listview.ToInt64())
if ($script:listview -ne [IntPtr]::Zero) {
    # LVM_GETITEMCOUNT = 0x1004
    $n = [DState]::SendMessageW($script:listview, 0x1004, [IntPtr]::Zero, [IntPtr]::Zero)
    W "desktop icon count (LVM_GETITEMCOUNT) = $($n.ToInt64())"
}

# THE experiment: set the live desktop color to red via SetSysColors (bypasses explorer);
# win32k repaints the desktop immediately on success.
$ok = [DState]::SetSysColors(1, @(1), @([uint32]0x2311e8))  # COLORREF 0x00BBGGRR: B=0x23 G=0x11 R=0xE8
$gle = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()
W "SetSysColors(COLOR_BACKGROUND, red) => $ok gle=$gle"
$c2 = [DState]::GetSysColor(1)
W ("GetSysColor after = 0x{0:x6}" -f $c2)
if ($script:progman -ne [IntPtr]::Zero) { [void][DState]::RedrawWindow($script:progman, [IntPtr]::Zero, [IntPtr]::Zero, 0x785) }
Start-Sleep -Milliseconds 1500

# capture the screen after the poke
try {
    Add-Type -AssemblyName System.Windows.Forms
    $vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $bmp = New-Object System.Drawing.Bitmap($vs.Width, $vs.Height)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($vs.X, $vs.Y, 0, 0, $bmp.Size); $g.Dispose()
    $bmp.Save('Z:\tmp\screen_after_setsyscolors.png', [System.Drawing.Imaging.ImageFormat]::Png)
    W ("post-poke screen pixel (500,300) = {0}" -f $bmp.GetPixel(500,300))
    $bmp.Dispose()
} catch { W "capture failed: $_" }
W "=== end $(Get-Date -Format o) ==="
[System.IO.File]::WriteAllText($out, $sb.ToString())
