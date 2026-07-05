# find_vs_enum_probe.ps1 — resolve the FindWindow-vs-EnumWindows Progman discrepancy
# atomically (single process, single instant), and dump Progman's child tree if found.
# Run in session 1 via scheduled task. Output: C:\ProgramData\Helios\find_vs_enum.txt
$out = 'C:\ProgramData\Helios\find_vs_enum.txt'
$sb = [System.Text.StringBuilder]::new()
function W([string]$s) { [void]$sb.AppendLine($s) }
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class FvE {
    [DllImport("user32.dll", SetLastError=true)] public static extern IntPtr FindWindowW([MarshalAs(UnmanagedType.LPWStr)] string cls, [MarshalAs(UnmanagedType.LPWStr)] string win);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lp);
    [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr h, EnumProc cb, IntPtr lp);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern IntPtr GetThreadDesktop(uint tid);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
    [DllImport("user32.dll", SetLastError=true)] public static extern bool GetUserObjectInformationW(IntPtr h, int idx, StringBuilder info, int len, out int needed);
    public delegate bool EnumProc(IntPtr h, IntPtr lp);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@
W "=== find_vs_enum $(Get-Date -Format o) session=$([System.Diagnostics.Process]::GetCurrentProcess().SessionId) pid=$PID ==="
# Which desktop is this probe on?
$hd = [FvE]::GetThreadDesktop([FvE]::GetCurrentThreadId())
$name = [System.Text.StringBuilder]::new(256); $needed = 0
[void][FvE]::GetUserObjectInformationW($hd, 2, $name, 256, [ref]$needed)  # UOI_NAME=2
W "probe thread desktop = '$($name.ToString())'"

$fw = [FvE]::FindWindowW('Progman', $null)
$err = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()
W ("FindWindowW('Progman') = 0x{0:x} (gle={1})" -f $fw.ToInt64(), $err)

$script:found = [IntPtr]::Zero
$cb = [FvE+EnumProc]{ param($h, $lp)
    $cls = [System.Text.StringBuilder]::new(256); [void][FvE]::GetClassName($h, $cls, 256)
    if ($cls.ToString() -eq 'Progman') { $script:found = $h }
    return $true }
[void][FvE]::EnumWindows($cb, [IntPtr]::Zero)
W ("EnumWindows Progman     = 0x{0:x}" -f $script:found.ToInt64())

if ($script:found -ne [IntPtr]::Zero) {
    W "--- Progman children ---"
    $child = [FvE+EnumProc]{ param($h, $lp)
        $cls = [System.Text.StringBuilder]::new(256); [void][FvE]::GetClassName($h, $cls, 256)
        $r = New-Object FvE+RECT; [void][FvE]::GetWindowRect($h, [ref]$r)
        $vis = if ([FvE]::IsWindowVisible($h)) { 'VIS' } else { '   ' }
        W ("  0x{0,-10:x} {1} cls='{2}' rect=({3},{4})-({5},{6})" -f $h.ToInt64(), $vis, $cls, $r.L, $r.T, $r.R, $r.B)
        return $true }
    [void][FvE]::EnumChildWindows($script:found, $child, [IntPtr]::Zero)
}
[System.IO.File]::WriteAllText($out, $sb.ToString())
