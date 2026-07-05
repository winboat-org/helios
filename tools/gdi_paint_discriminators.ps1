# gdi_paint_discriminators.ps1 - session-1 probes, post-executor-fix:
#  1. Launch regedit (pure Win32 GDI app), wait, CopyFromScreen -> Z:\tmp\regedit_screen.png
#     (does classic GDI window CONTENT reach the screen at all?)
#  2. PrintWindow(flags=0, classic WM_PRINT) of Progman and of SysListView32 ->
#     Z:\tmp\progman_pw0.png / Z:\tmp\listview_pw0.png (pure user-mode paint into our DIB,
#     no RenderGdi: do the 9 icons draw ANYWHERE?)
# Output log: C:\ProgramData\Helios\gdi_discriminators.txt
$out = 'C:\ProgramData\Helios\gdi_discriminators.txt'
$sb = [System.Text.StringBuilder]::new()
function W([string]$s) { [void]$sb.AppendLine($s) }
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class Gp {
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lp);
    [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr h, EnumProc cb, IntPtr lp);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll", SetLastError=true)] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
    public delegate bool EnumProc(IntPtr h, IntPtr lp);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@
W "=== gdi_discriminators $(Get-Date -Format o) ==="

function NonzeroFrac([System.Drawing.Bitmap]$bmp) {
    $n = 0; $tot = 0
    for ($y = 10; $y -lt $bmp.Height; $y += 37) {
        for ($x = 10; $x -lt $bmp.Width; $x += 37) {
            $c = $bmp.GetPixel($x, $y); $tot++
            if ($c.R -ne 0 -or $c.G -ne 0 -or $c.B -ne 0) { $n++ }
        }
    }
    if ($tot -eq 0) { return 0 }
    return [math]::Round(100.0 * $n / $tot, 1)
}

function PW([IntPtr]$h, [string]$name, [uint32]$flags) {
    $r = New-Object Gp+RECT
    [void][Gp]::GetWindowRect($h, [ref]$r)
    $wd = [Math]::Max($r.R - $r.L, 1); $ht = [Math]::Max($r.B - $r.T, 1)
    $bmp = New-Object System.Drawing.Bitmap($wd, $ht)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    $ok = [Gp]::PrintWindow($h, $hdc, $flags)
    $g.ReleaseHdc($hdc); $g.Dispose()
    $bmp.Save("Z:\tmp\$name.png", [System.Drawing.Imaging.ImageFormat]::Png)
    W ("PrintWindow({0}, flags={1}) => {2}  size={3}x{4} nonzero~{5}%" -f $name, $flags, $ok, $wd, $ht, (NonzeroFrac $bmp))
    $bmp.Dispose()
}

# find Progman + its SysListView32
$script:progman = [IntPtr]::Zero; $script:lv = [IntPtr]::Zero
$cb = [Gp+EnumProc]{ param($h, $lp)
    $cls = [System.Text.StringBuilder]::new(256); [void][Gp]::GetClassName($h, $cls, 256)
    if ($cls.ToString() -eq 'Progman') { $script:progman = $h }
    return $true }
[void][Gp]::EnumWindows($cb, [IntPtr]::Zero)
if ($script:progman -ne [IntPtr]::Zero) {
    $cb2 = [Gp+EnumProc]{ param($h, $lp)
        $cls = [System.Text.StringBuilder]::new(256); [void][Gp]::GetClassName($h, $cls, 256)
        if ($cls.ToString() -eq 'SysListView32') { $script:lv = $h }
        return $true }
    [void][Gp]::EnumChildWindows($script:progman, $cb2, [IntPtr]::Zero)
}
W ("Progman=0x{0:x} listview=0x{1:x}" -f $script:progman.ToInt64(), $script:lv.ToInt64())
if ($script:progman -ne [IntPtr]::Zero) { PW $script:progman 'progman_pw0' 0 }
if ($script:lv -ne [IntPtr]::Zero) { PW $script:lv 'listview_pw0' 0 }

# regedit test
$p = Start-Process regedit -PassThru
Start-Sleep -Seconds 5
$vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
$bmp2 = New-Object System.Drawing.Bitmap($vs.Width, $vs.Height)
$g2 = [System.Drawing.Graphics]::FromImage($bmp2)
$g2.CopyFromScreen($vs.X, $vs.Y, 0, 0, $bmp2.Size); $g2.Dispose()
$bmp2.Save('Z:\tmp\regedit_screen.png', [System.Drawing.Imaging.ImageFormat]::Png)
W "regedit screenshot saved"
$bmp2.Dispose()
Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
W "=== end $(Get-Date -Format o) ==="
[System.IO.File]::WriteAllText($out, $sb.ToString())
