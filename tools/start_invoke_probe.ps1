# start_invoke_probe.ps1 — does the Start/Search INVOKE ever reach its host process?
#
# start_menu_repro.ps1 showed StartMenuExperienceHost burning 0.000 s of CPU across
# six Win-key presses: no window, no UMD traffic, no pixels. That is only meaningful
# if the synthetic input is real input, so this probe carries its own controls and
# compares input paths side by side. Each step reports the CPU delta of every
# participant, so "who woke up" is a measurement rather than an inference.
#
#   CONTROL-1  Win+R  -> a Run dialog (#32770) must appear. Proves injection works
#                        and that explorer services a shell hotkey at all.
#   CONTROL-2  explorer CPU delta on a bare Win press. Proves the keystroke is
#                        at least delivered to the shell.
#   STEP-A     Win key
#   STEP-B     real mouse click on the Start button (the owner's repro)
#   STEP-C     real mouse click on the Search box
#
# Output: C:\ProgramData\Helios\start_invoke_probe.txt

$log = 'C:\ProgramData\Helios\start_invoke_probe.txt'
$sb = [System.Text.StringBuilder]::new()
function W([string]$s) { [void]$sb.AppendLine($s) }

Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class IN {
    [DllImport("user32.dll")] public static extern IntPtr GetDesktopWindow();
    [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr h, uint cmd);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string c, string w);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, IntPtr extra);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, int dx, int dy, uint data, IntPtr extra);
    [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
    [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr h, int a, out int v, int sz);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
}
"@
$GW_CHILD = 5; $GW_HWNDNEXT = 2; $DWMWA_CLOAKED = 14
$VK_LWIN = 0x5B; $VK_ESC = 0x1B; $VK_R = 0x52; $KEYUP = 0x2
$MOUSEEVENTF_LEFTDOWN = 0x0002; $MOUSEEVENTF_LEFTUP = 0x0004

$WATCH = 'StartMenuExperienceHost', 'SearchHost', 'explorer', 'dwm', 'ShellHost', 'ShellExperienceHost'

function Walk() {
    $o = @(); $h = [IN]::GetWindow([IN]::GetDesktopWindow(), $GW_CHILD); $g = 0
    while ($h -ne [IntPtr]::Zero -and $g -lt 5000) { $o += $h; $h = [IN]::GetWindow($h, $GW_HWNDNEXT); $g++ }
    return $o
}
function Snap() {
    $m = @{}
    foreach ($n in $WATCH) {
        $p = Get-Process -Name $n -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($p) { $m[$n] = [pscustomobject]@{ Id = $p.Id; Cpu = $p.CPU; Thr = $p.Threads.Count } }
    }
    return $m
}
function WinCount([string]$n) {
    $p = Get-Process -Name $n -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $p) { return -1 }
    $c = 0
    foreach ($w in Walk) { $wp = [uint32]0; [void][IN]::GetWindowThreadProcessId($w, [ref]$wp); if ($wp -eq [uint32]$p.Id) { $c++ } }
    return $c
}
function Report([string]$label, $before, $after, [int]$ms) {
    W "  [$label] after ${ms}ms"
    foreach ($n in $WATCH) {
        if ($before.ContainsKey($n) -and $after.ContainsKey($n)) {
            $d = $after[$n].Cpu - $before[$n].Cpu
            $restart = if ($after[$n].Id -ne $before[$n].Id) { " RESTARTED(pid $($before[$n].Id)->$($after[$n].Id))" } else { '' }
            W ("    {0,-26} cpuDelta={1,8:N3}s threads {2}->{3}{4}" -f $n, $d, $before[$n].Thr, $after[$n].Thr, $restart)
        } elseif ($after.ContainsKey($n)) { W ("    {0,-26} STARTED (pid {1})" -f $n, $after[$n].Id) }
        elseif ($before.ContainsKey($n)) { W ("    {0,-26} EXITED" -f $n) }
    }
    W ("    windows: StartMenu={0} SearchHost={1}" -f (WinCount 'StartMenuExperienceHost'), (WinCount 'SearchHost'))
}

W "=== start_invoke_probe $(Get-Date -Format o) ==="
W "session=$([System.Diagnostics.Process]::GetCurrentProcess().SessionId)"

# Locate the taskbar and its Start button / Search box by geometry.
$tray = [IN]::FindWindowW('Shell_TrayWnd', $null)
$tr = New-Object IN+RECT
if ($tray -ne [IntPtr]::Zero) { [void][IN]::GetWindowRect($tray, [ref]$tr) }
W ("Shell_TrayWnd = 0x{0:x} rect=({1},{2})-({3},{4})" -f $tray.ToInt64(), $tr.L, $tr.T, $tr.R, $tr.B)
$vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
# Win11 centred taskbar: Start button is the leftmost icon of the centre cluster.
$startX = 770; $startY = [int](($tr.T + $tr.B) / 2); if ($startY -le 0) { $startY = $vs.Height - 24 }
$searchX = 900; $searchY = $startY
W "Start button target = ($startX,$startY) ; Search box target = ($searchX,$searchY)"
$pt = New-Object IN+POINT; $pt.X = $startX; $pt.Y = $startY
$hw = [IN]::WindowFromPoint($pt)
$cls = [System.Text.StringBuilder]::new(256); [void][IN]::GetClassName($hw, $cls, 256)
$hp = [uint32]0; [void][IN]::GetWindowThreadProcessId($hw, [ref]$hp)
$hpn = (Get-Process -Id $hp -ErrorAction SilentlyContinue).ProcessName
W ("WindowFromPoint(start button) = 0x{0:x} cls='{1}' pid={2} ({3})" -f $hw.ToInt64(), $cls, $hp, $hpn)
$pt2 = New-Object IN+POINT; $pt2.X = $searchX; $pt2.Y = $searchY
$hw2 = [IN]::WindowFromPoint($pt2)
$cls2 = [System.Text.StringBuilder]::new(256); [void][IN]::GetClassName($hw2, $cls2, 256)
W ("WindowFromPoint(search box)   = 0x{0:x} cls='{1}'" -f $hw2.ToInt64(), $cls2)

# ---------------- CONTROL-1: Win+R must open a Run dialog ----------------
W ""
W "--- CONTROL-1: Win+R (does synthetic input work at all?) ---"
$b = Snap
[IN]::keybd_event($VK_LWIN, 0, 0, [IntPtr]::Zero)
[IN]::keybd_event($VK_R, 0, 0, [IntPtr]::Zero)
[IN]::keybd_event($VK_R, 0, $KEYUP, [IntPtr]::Zero)
[IN]::keybd_event($VK_LWIN, 0, $KEYUP, [IntPtr]::Zero)
Start-Sleep -Milliseconds 2500
$run = $null
foreach ($w in Walk) {
    $c = [System.Text.StringBuilder]::new(256); [void][IN]::GetClassName($w, $c, 256)
    $tx = [System.Text.StringBuilder]::new(256); [void][IN]::GetWindowText($w, $tx, 256)
    if ($c.ToString() -eq '#32770' -and $tx.ToString() -match 'Run') { $run = $w }
}
if ($run) { W ("  RUN DIALOG APPEARED hwnd=0x{0:x} -> synthetic input WORKS" -f $run.ToInt64()) }
else { W "  NO RUN DIALOG -> synthetic input may be a no-op; treat later steps with suspicion" }
Report 'CONTROL-1 Win+R' $b (Snap) 2500
[IN]::keybd_event($VK_ESC, 0, 0, [IntPtr]::Zero); [IN]::keybd_event($VK_ESC, 0, $KEYUP, [IntPtr]::Zero)
Start-Sleep -Milliseconds 1200

# ---------------- STEP-A: bare Win key ----------------
W ""
W "--- STEP-A: Win key ---"
$b = Snap
[IN]::keybd_event($VK_LWIN, 0, 0, [IntPtr]::Zero); [IN]::keybd_event($VK_LWIN, 0, $KEYUP, [IntPtr]::Zero)
Start-Sleep -Milliseconds 3000
Report 'STEP-A Win' $b (Snap) 3000
[IN]::keybd_event($VK_ESC, 0, 0, [IntPtr]::Zero); [IN]::keybd_event($VK_ESC, 0, $KEYUP, [IntPtr]::Zero)
Start-Sleep -Milliseconds 1200

# ---------------- STEP-B: real mouse click on the Start button ----------------
W ""
W "--- STEP-B: mouse click on Start button ($startX,$startY) ---"
$b = Snap
[void][IN]::SetCursorPos($startX, $startY); Start-Sleep -Milliseconds 300
[IN]::mouse_event($MOUSEEVENTF_LEFTDOWN, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 60
[IN]::mouse_event($MOUSEEVENTF_LEFTUP, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 3000
Report 'STEP-B click Start' $b (Snap) 3000
[IN]::keybd_event($VK_ESC, 0, 0, [IntPtr]::Zero); [IN]::keybd_event($VK_ESC, 0, $KEYUP, [IntPtr]::Zero)
Start-Sleep -Milliseconds 1200

# ---------------- STEP-C: real mouse click on the Search box ----------------
W ""
W "--- STEP-C: mouse click on Search box ($searchX,$searchY) ---"
$b = Snap
[void][IN]::SetCursorPos($searchX, $searchY); Start-Sleep -Milliseconds 300
[IN]::mouse_event($MOUSEEVENTF_LEFTDOWN, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 60
[IN]::mouse_event($MOUSEEVENTF_LEFTUP, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 3000
Report 'STEP-C click Search' $b (Snap) 3000

# Final screen capture for the record.
try {
    $bmp = New-Object System.Drawing.Bitmap($vs.Width, $vs.Height)
    $g = [System.Drawing.Graphics]::FromImage($bmp); $g.CopyFromScreen(0, 0, 0, 0, $bmp.Size); $g.Dispose()
    $bmp.Save('Z:\tmp\start_invoke_final.png', [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
    W ""
    W "final screen -> Z:\tmp\start_invoke_final.png"
} catch { W "capture failed: $_" }

W "=== end $(Get-Date -Format o) ==="
[System.IO.File]::WriteAllText($log, $sb.ToString())
