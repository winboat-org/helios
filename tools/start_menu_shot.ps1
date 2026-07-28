# start_menu_shot.ps1 — press Win, let the flyout settle, screenshot it.
# Session-1 probe. Screen pixels are the only rendering evidence that counts
# (CLAUDE.md), so this exists to photograph the Start menu rather than infer it
# from counters. Output: Z:\tmp\start_menu_open.png
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;using System.Runtime.InteropServices;
public class K { [DllImport("user32.dll")] public static extern void keybd_event(byte v, byte s, uint f, IntPtr e); }
"@
[K]::keybd_event(0x5B, 0, 0, [IntPtr]::Zero)
[K]::keybd_event(0x5B, 0, 2, [IntPtr]::Zero)
Start-Sleep -Milliseconds 2000
$vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
$b = New-Object System.Drawing.Bitmap($vs.Width, $vs.Height)
$g = [System.Drawing.Graphics]::FromImage($b)
$g.CopyFromScreen(0, 0, 0, 0, $b.Size)
$g.Dispose()
$b.Save('Z:\tmp\start_menu_open.png', [System.Drawing.Imaging.ImageFormat]::Png)
$b.Dispose()
[K]::keybd_event(0x1B, 0, 0, [IntPtr]::Zero)
[K]::keybd_event(0x1B, 0, 2, [IntPtr]::Zero)
