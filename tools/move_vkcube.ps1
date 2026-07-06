# move_vkcube.ps1 — one-shot: move the "Vulkan Cube" window to a clear desktop
# area and raise it, so a paintcap shows its client content unobscured.
# Session-1 helper for the dcomp-vehicle verification ladder (23rd session).
#
# Target override (24th session): the schtask invocation is arg-less, so a
# one-shot override rides Z:\tmp\movewin_target.txt — first line = window
# TITLE to foreground (consumed and deleted; move skipped for non-vkcube
# targets, foreground only). Needed because idTech throttles background
# windows to 60 fps — unattended perf runs must foreground the game first.
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
$target = 'Vulkan Cube'
$overrideFile = 'Z:\tmp\movewin_target.txt'
$isOverride = $false
if (Test-Path -LiteralPath $overrideFile) {
  $line = (Get-Content -LiteralPath $overrideFile -First 1).Trim()
  Remove-Item -LiteralPath $overrideFile -Force
  if ($line) { $target = $line; $isOverride = $true }
}
$h = [Win32Move]::FindWindowW($null, $target)
if ($h -ne [IntPtr]::Zero) {
  if (-not $isOverride) {
    [Win32Move]::MoveWindow($h, 560, 120, 820, 650, $true) | Out-Null
  }
  [Win32Move]::SetForegroundWindow($h) | Out-Null
  "foregrounded '$target' ($h)"
} else {
  "window '$target' not found"
}
