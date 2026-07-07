$ErrorActionPreference = 'Stop'

$out = 'C:\Users\Rupansh\FaceWorks\move_faceworks_window.txt'

Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class FWMove {
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder text, int count);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr hWndInsertAfter, int X, int Y, int cx, int cy, uint uFlags);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
}
"@

$targets = @(Get-Process sample_d3d11 -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id)
$rows = New-Object System.Collections.Generic.List[string]
$rows.Add(("target_pids={0}" -f ($targets -join ',')))

$callback = [FWMove+EnumWindowsProc]{
  param([IntPtr]$hwnd, [IntPtr]$lparam)

  [uint32]$ownerPid = 0
  [FWMove]::GetWindowThreadProcessId($hwnd, [ref]$ownerPid) | Out-Null

  $titleBuilder = New-Object System.Text.StringBuilder 256
  [FWMove]::GetWindowText($hwnd, $titleBuilder, $titleBuilder.Capacity) | Out-Null
  $title = $titleBuilder.ToString()

  $isTarget = ($targets -contains [int]$ownerPid) -or ($title -like '*FaceWorks*') -or ($title -like '*GeForce SDK*')
  if ($isTarget) {
    [FWMove]::ShowWindow($hwnd, 9) | Out-Null
    [FWMove]::SetWindowPos($hwnd, [IntPtr]::Zero, 80, 80, 1280, 720, 0x0040) | Out-Null

    $rect = New-Object FWMove+RECT
    [FWMove]::GetWindowRect($hwnd, [ref]$rect) | Out-Null
    $rows.Add(("moved pid={0} hwnd=0x{1:x} visible={2} rect={3},{4},{5},{6} title='{7}'" -f $ownerPid, $hwnd.ToInt64(), [FWMove]::IsWindowVisible($hwnd), $rect.Left, $rect.Top, $rect.Right, $rect.Bottom, $title))
  }

  return $true
}

[FWMove]::EnumWindows($callback, [IntPtr]::Zero) | Out-Null
if ($rows.Count -eq 1) {
  $rows.Add('no matching windows')
}

$rows | Set-Content -Path $out -Encoding ASCII
