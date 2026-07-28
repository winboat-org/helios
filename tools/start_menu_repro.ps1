# start_menu_repro.ps1 — why doesn't the Start menu / Search flyout appear?
#
# Session-1 probe (scheduled task; SSH lands in session 0 and cannot see the
# desktop). Presses the Start/Search hotkey N times and, for each trial, records
# everything needed to place the failure on one side of the guest boundary:
#
#   * does a top-level window for the flyout host EVER get created?   (window tree)
#   * does the host process even WAKE UP?                             (CPU time delta)
#   * does it reach our D3D11 UMD?                                    (umd-<pid>.log growth)
#   * do the pixels in the flyout region change?                      (screen capture)
#   * did the host process get RESTARTED by the shell?                (pid/start time)
#
# The pixel comparison, not a log line, is the arbiter (CLAUDE.md evidence rule).
# Verdicts:
#   APPEARED                 window created + uncloaked + pixels changed
#   SHOWN_BUT_NOT_PAINTED    window created + uncloaked, pixels identical -> ours
#   WINDOW_NEVER_CREATED     no window ever appeared -> upstream of composition
#
# Output: C:\ProgramData\Helios\start_menu_repro.txt (+ PNGs in Z:\tmp)

param(
    [int]$Trials = 6,
    [int]$PollMs = 5000,
    [ValidateSet('start', 'search')] [string]$Target = 'start'
)

$log = 'C:\ProgramData\Helios\start_menu_repro.txt'
$sb = [System.Text.StringBuilder]::new()
function W([string]$s) { [void]$sb.AppendLine($s) }

Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class SM {
    [DllImport("user32.dll")] public static extern IntPtr GetDesktopWindow();
    [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr h, uint cmd);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, IntPtr extra);
    [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr h, int a, out int v, int sz);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@
$GW_CHILD = 5; $GW_HWNDNEXT = 2; $DWMWA_CLOAKED = 14
$VK_LWIN = 0x5B; $VK_ESCAPE = 0x1B; $VK_S = 0x53; $KEYUP = 0x2

if (-not (Test-Path 'Z:\tmp')) { New-Item -ItemType Directory -Path 'Z:\tmp' | Out-Null }
$procName = if ($Target -eq 'start') { 'StartMenuExperienceHost' } else { 'SearchHost' }

W "=== start_menu_repro $(Get-Date -Format o) target=$Target trials=$Trials pollMs=$PollMs ==="
W "session=$([System.Diagnostics.Process]::GetCurrentProcess().SessionId)"

# Top-level window walk (EnumWindows and this agree; this needs no callback).
function Walk() {
    $out = @(); $h = [SM]::GetWindow([SM]::GetDesktopWindow(), $GW_CHILD); $g = 0
    while ($h -ne [IntPtr]::Zero -and $g -lt 5000) { $out += $h; $h = [SM]::GetWindow($h, $GW_HWNDNEXT); $g++ }
    return $out
}
function WinsOf([uint32]$thePid) {
    $r = @()
    foreach ($w in Walk) {
        $p = [uint32]0; [void][SM]::GetWindowThreadProcessId($w, [ref]$p)
        if ($p -eq $thePid) {
            $cls = [System.Text.StringBuilder]::new(256); [void][SM]::GetClassName($w, $cls, 256)
            $txt = [System.Text.StringBuilder]::new(256); [void][SM]::GetWindowText($w, $txt, 256)
            $rc = New-Object SM+RECT; [void][SM]::GetWindowRect($w, [ref]$rc)
            $ck = 0; [void][SM]::DwmGetWindowAttribute($w, $DWMWA_CLOAKED, [ref]$ck, 4)
            $r += [pscustomobject]@{ H = $w; Cls = $cls.ToString(); Txt = $txt.ToString(); R = $rc
                                     Vis = [SM]::IsWindowVisible($w); Cloak = $ck }
        }
    }
    return $r
}
function Grab([int]$x, [int]$y, [int]$w, [int]$h) {
    $b = New-Object System.Drawing.Bitmap($w, $h)
    $g = [System.Drawing.Graphics]::FromImage($b); $g.CopyFromScreen($x, $y, 0, 0, $b.Size); $g.Dispose()
    return $b
}
function Fingerprint($bmp) {
    $sum = [long]0
    for ($y = 4; $y -lt $bmp.Height; $y += 24) { for ($x = 4; $x -lt $bmp.Width; $x += 24) { $sum += $bmp.GetPixel($x, $y).ToArgb() } }
    return $sum
}

$vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
W "VirtualScreen = $($vs.Width)x$($vs.Height)"
# Region the Win11 centred flyout occupies: centre column, above the taskbar.
$rw = [int]($vs.Width * 0.42); $rh = [int]($vs.Height * 0.72)
$rx = [int](($vs.Width - $rw) / 2); $ry = [math]::Max(0, $vs.Height - $rh - 56)
W "sample region = ($rx,$ry) ${rw}x${rh}"

$results = @()
for ($t = 1; $t -le $Trials; $t++) {
    W ""
    W "--- trial $t ---"
    $p0 = Get-Process -Name $procName -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $p0) { W "  $procName NOT RUNNING at trial start" }
    else {
        W ("  pre : pid={0} start={1} cpu={2:N2}s threads={3} windows={4}" -f `
            $p0.Id, $p0.StartTime.ToString('HH:mm:ss'), $p0.CPU, $p0.Threads.Count, (WinsOf ([uint32]$p0.Id)).Count)
    }
    $umd = if ($p0) { "C:\ProgramData\Helios\umd-$($p0.Id).log" } else { $null }
    $umdLen0 = if ($umd -and (Test-Path $umd)) { (Get-Item $umd).Length } else { -1 }
    $cpu0 = if ($p0) { $p0.CPU } else { 0 }

    $base = Grab $rx $ry $rw $rh
    $fpBase = Fingerprint $base
    $base.Dispose()

    if ($Target -eq 'start') {
        [SM]::keybd_event($VK_LWIN, 0, 0, [IntPtr]::Zero); [SM]::keybd_event($VK_LWIN, 0, $KEYUP, [IntPtr]::Zero)
    } else {
        [SM]::keybd_event($VK_LWIN, 0, 0, [IntPtr]::Zero); [SM]::keybd_event($VK_S, 0, 0, [IntPtr]::Zero)
        [SM]::keybd_event($VK_S, 0, $KEYUP, [IntPtr]::Zero); [SM]::keybd_event($VK_LWIN, 0, $KEYUP, [IntPtr]::Zero)
    }
    $tk = [System.Diagnostics.Stopwatch]::StartNew()

    $sawWindow = $false; $winMs = -1; $sawUncloaked = $false; $uncloakMs = -1
    $sawPixels = $false; $pixMs = -1; $lastSig = ''
    while ($tk.ElapsedMilliseconds -lt $PollMs) {
        $pc = Get-Process -Name $procName -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($pc) {
            $ws = WinsOf ([uint32]$pc.Id)
            $sig = ($ws | ForEach-Object { "{0:x}:{1}:{2}:{3}" -f $_.H.ToInt64(), $_.Cls, $_.Vis, $_.Cloak }) -join ','
            if ($sig -ne $lastSig) {
                W ("  t+{0,-5}ms windows={1} :: {2}" -f $tk.ElapsedMilliseconds, $ws.Count, $sig)
                foreach ($w in $ws) {
                    W ("      0x{0:x} cls='{1}' title='{2}' vis={3} cloak=0x{4:x} rect=({5},{6})-({7},{8})" -f `
                        $w.H.ToInt64(), $w.Cls, $w.Txt, $w.Vis, $w.Cloak, $w.R.L, $w.R.T, $w.R.R, $w.R.B)
                }
                $lastSig = $sig
            }
            if ($ws.Count -gt 0 -and -not $sawWindow) { $sawWindow = $true; $winMs = $tk.ElapsedMilliseconds }
            foreach ($w in $ws) { if ($w.Vis -and $w.Cloak -eq 0 -and -not $sawUncloaked) { $sawUncloaked = $true; $uncloakMs = $tk.ElapsedMilliseconds } }
        }
        if (-not $sawPixels) {
            $cur = Grab $rx $ry $rw $rh
            if ((Fingerprint $cur) -ne $fpBase) { $sawPixels = $true; $pixMs = $tk.ElapsedMilliseconds; W "  t+$($tk.ElapsedMilliseconds)ms PIXELS CHANGED" }
            $cur.Dispose()
        }
        Start-Sleep -Milliseconds 80
    }

    $p1 = Get-Process -Name $procName -ErrorAction SilentlyContinue | Select-Object -First 1
    $restarted = if ($p0 -and $p1) { $p1.Id -ne $p0.Id } else { $false }
    $cpuDelta = if ($p1 -and -not $restarted) { $p1.CPU - $cpu0 } else { -1 }
    $umdLen1 = if ($umd -and (Test-Path $umd)) { (Get-Item $umd).Length } else { -1 }
    $verdict =
        if ($sawPixels -and $sawWindow) { 'APPEARED' }
        elseif ($sawUncloaked -and -not $sawPixels) { 'SHOWN_BUT_NOT_PAINTED' }
        elseif ($sawWindow -and -not $sawUncloaked) { 'WINDOW_CREATED_BUT_CLOAKED' }
        elseif ($sawPixels) { 'PIXELS_ONLY' }
        else { 'WINDOW_NEVER_CREATED' }
    W ("  RESULT trial {0}: {1}" -f $t, $verdict)
    W ("    window@{0}ms uncloak@{1}ms pixels@{2}ms | cpuDelta={3:N3}s umdLog {4}->{5} (+{6}) | restarted={7} pid={8}" -f `
        $winMs, $uncloakMs, $pixMs, $cpuDelta, $umdLen0, $umdLen1, ($umdLen1 - $umdLen0), $restarted, $(if ($p1) { $p1.Id } else { 'gone' }))
    $results += $verdict

    if ($verdict -ne 'APPEARED') {
        $m = Grab 0 0 $vs.Width $vs.Height
        $m.Save("Z:\tmp\startmenu_${Target}_miss_t$t.png", [System.Drawing.Imaging.ImageFormat]::Png); $m.Dispose()
        W "    saved Z:\tmp\startmenu_${Target}_miss_t$t.png"
    }

    [SM]::keybd_event($VK_ESCAPE, 0, 0, [IntPtr]::Zero); [SM]::keybd_event($VK_ESCAPE, 0, $KEYUP, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 1500
}

W ""
W "=== SUMMARY ==="
$results | Group-Object | ForEach-Object { W ("  {0,-28} {1}/{2}" -f $_.Name, $_.Count, $Trials) }
W "=== end $(Get-Date -Format o) ==="
[System.IO.File]::WriteAllText($log, $sb.ToString())
