# desktop_paint_capture.ps1 — two discriminating captures from session 1:
#  1. PrintWindow(Progman) -> Z:\tmp\progman_printwindow.png
#     Asks the desktop window tree to paint itself into OUR bitmap. Red fill +
#     icons here = the window's paint path is healthy and the failure is in
#     redirection-surface delivery/composition. Black = the window itself
#     doesn't produce content even on request.
#  2. Graphics.CopyFromScreen -> Z:\tmp\screen_copy.png
#     GDI screen readback of the composed primary = a guest-side screenshot.
# Run via scheduled task in session 1.
$log = 'C:\ProgramData\Helios\desktop_paint_capture.txt'
$sb = [System.Text.StringBuilder]::new()
function W([string]$s) { [void]$sb.AppendLine($s) }
W "=== desktop_paint_capture $(Get-Date -Format o) ==="
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class Cap {
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lp);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll", SetLastError=true)] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
    public delegate bool EnumProc(IntPtr h, IntPtr lp);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@
if (-not (Test-Path 'Z:\tmp')) { New-Item -ItemType Directory -Path 'Z:\tmp' | Out-Null }

$script:progman = [IntPtr]::Zero
$cb = [Cap+EnumProc]{ param($h, $lp)
    $cls = [System.Text.StringBuilder]::new(256); [void][Cap]::GetClassName($h, $cls, 256)
    if ($cls.ToString() -eq 'Progman') { $script:progman = $h }
    return $true }
[void][Cap]::EnumWindows($cb, [IntPtr]::Zero)
W ("Progman = 0x{0:x}" -f $script:progman.ToInt64())

if ($script:progman -ne [IntPtr]::Zero) {
    $r = New-Object Cap+RECT
    [void][Cap]::GetWindowRect($script:progman, [ref]$r)
    $wd = $r.R - $r.L; $ht = $r.B - $r.T
    W "Progman rect ${wd}x${ht}"
    $bmp = New-Object System.Drawing.Bitmap($wd, $ht)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    # PW_RENDERFULLCONTENT (2) asks even DX-redirected windows to render.
    $ok = [Cap]::PrintWindow($script:progman, $hdc, 2)
    $gle = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()
    $g.ReleaseHdc($hdc); $g.Dispose()
    W "PrintWindow(PW_RENDERFULLCONTENT) => $ok gle=$gle"
    $bmp.Save('Z:\tmp\progman_printwindow.png', [System.Drawing.Imaging.ImageFormat]::Png)
    # quick stats
    $c1 = $bmp.GetPixel(500, 300); $c2 = $bmp.GetPixel(948, 515); $c3 = $bmp.GetPixel(100, 100)
    W ("sample pixels: (500,300)={0} (948,515)={1} (100,100)={2}" -f $c1, $c2, $c3)
    $bmp.Dispose()
}

# Screen copy
try {
    $vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
    W "VirtualScreen = $vs"
    $bmp2 = New-Object System.Drawing.Bitmap($vs.Width, $vs.Height)
    $g2 = [System.Drawing.Graphics]::FromImage($bmp2)
    $g2.CopyFromScreen($vs.X, $vs.Y, 0, 0, $bmp2.Size)
    $g2.Dispose()
    $bmp2.Save('Z:\tmp\screen_copy.png', [System.Drawing.Imaging.ImageFormat]::Png)
    $s1 = $bmp2.GetPixel(500, 300); $s2 = $bmp2.GetPixel(948, 900)
    W ("screen sample pixels: (500,300)={0} (948,900)={1}" -f $s1, $s2)
    $bmp2.Dispose()
    W "CopyFromScreen saved"
} catch { W "CopyFromScreen FAILED: $_" }
W "=== end $(Get-Date -Format o) ==="
[System.IO.File]::WriteAllText($log, $sb.ToString())
