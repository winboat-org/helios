# move_vkcube.ps1 — one-shot: move the "Vulkan Cube" window to a clear desktop
# area and raise it, so a paintcap shows its client content unobscured.
# Session-1 helper for the dcomp-vehicle verification ladder (23rd session).
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Win32Move {
  [DllImport("user32.dll", CharSet = CharSet.Unicode)]
  public static extern IntPtr FindWindowW(string cls, string title);
  [DllImport("user32.dll")]
  public static extern bool MoveWindow(IntPtr h, int x, int y, int w, int hgt, bool repaint);
  [DllImport("user32.dll")]
  public static extern bool SetForegroundWindow(IntPtr h);
}
"@
$h = [Win32Move]::FindWindowW($null, 'Vulkan Cube')
if ($h -ne [IntPtr]::Zero) {
  [Win32Move]::MoveWindow($h, 560, 120, 820, 650, $true) | Out-Null
  [Win32Move]::SetForegroundWindow($h) | Out-Null
  "moved $h"
} else {
  "Vulkan Cube window not found"
}
