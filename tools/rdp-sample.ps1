# Sample the RDP graphics pipeline + the two processes that implement it on
# Helios, so a repro run can be attributed instead of described.
#
# Runs fine from session 0 (win_exec): perf counters and process CPU times are
# machine-wide. Only the *workload* has to be in session 1.
#
#   dwm (session 1)  = the producer: composes the desktop on Helios
#   WUDFHost         = the consumer: RDPIDD, opens DWM's shared texture, copies
#                      it to a STAGING texture and Map(READ)s 8.3 MB per frame
param(
    [int]$Seconds = 15,
    [string]$Label = 'sample'
)

function Get-HeliosProc {
    param([string]$Name, [int]$Session = -1)
    $p = Get-Process -Name $Name -EA SilentlyContinue
    if ($Session -ge 0) { $p = $p | Where-Object { $_.SessionId -eq $Session } }
    return $p | Select-Object -First 1
}

# Resolve the RDP session at run time. It is NOT stable: a reconnect (e.g.
# after a reboot) moves the same user to a new session id, and hardcoding one
# silently measures a session that no longer exists and reports 0% CPU.
function Get-RdpSessionId {
    foreach ($line in (query session 2>$null)) {
        if ($line -match '^\s*>?rdp-tcp#\d+\s+\S+\s+(\d+)\s+Active') { return [int]$Matches[1] }
    }
    # Fallback: the session running the interactive shell.
    $e = Get-Process -Name explorer -EA SilentlyContinue | Select-Object -First 1
    if ($e) { return [int]$e.SessionId }
    return -1
}

$rdpSession = Get-RdpSessionId
$dwm = Get-HeliosProc -Name 'dwm' -Session $rdpSession
$wudf = Get-HeliosProc -Name 'WUDFHost'

$dwmCpu0 = if ($dwm) { $dwm.TotalProcessorTime.TotalMilliseconds } else { 0 }
$wudfCpu0 = if ($wudf) { $wudf.TotalProcessorTime.TotalMilliseconds } else { 0 }
$t0 = Get-Date

$term = Get-CimInstance Win32_Service -Filter "Name='TermService'" -EA SilentlyContinue
$termProc = if ($term -and $term.ProcessId) { Get-Process -Id $term.ProcessId -EA SilentlyContinue } else { $null }
$termCpu0 = if ($termProc) { $termProc.TotalProcessorTime.TotalMilliseconds } else { 0 }

$samples = Get-Counter -Counter @(
    '\Processor(_Total)\% Processor Time',
    '\RemoteFX Network(*)\Total Sent Rate',
    '\RemoteFX Network(*)\Current TCP Bandwidth',
    '\RemoteFX Network(*)\Current TCP RTT',
    '\RemoteFX Network(*)\Current UDP Bandwidth',
    '\RemoteFX Network(*)\Loss Rate',
    '\RemoteFX Graphics(rdp-tcp 0)\Input Frames/Second',
    '\RemoteFX Graphics(rdp-tcp 0)\Output Frames/Second',
    '\RemoteFX Graphics(rdp-tcp 0)\Source Frames/Second',
    '\RemoteFX Graphics(rdp-tcp 0)\Average Encoding Time',
    '\RemoteFX Graphics(rdp-tcp 0)\Frames Skipped/Second - Insufficient Server Resources',
    '\RemoteFX Graphics(rdp-tcp 0)\Frames Skipped/Second - Insufficient Network Resources',
    '\RemoteFX Graphics(rdp-tcp 0)\Frames Skipped/Second - Insufficient Client Resources',
    '\RemoteFX Graphics(rdp-tcp 0)\Frame Quality'
) -SampleInterval 1 -MaxSamples $Seconds -EA SilentlyContinue

$elapsed = ((Get-Date) - $t0).TotalMilliseconds
$dwm = Get-HeliosProc -Name 'dwm' -Session $rdpSession
$wudf = Get-HeliosProc -Name 'WUDFHost'
$dwmCpu = if ($dwm) { $dwm.TotalProcessorTime.TotalMilliseconds - $dwmCpu0 } else { 0 }
$wudfCpu = if ($wudf) { $wudf.TotalProcessorTime.TotalMilliseconds - $wudfCpu0 } else { 0 }

Write-Output "=== $Label ==="
$rows = @{}
foreach ($s in $samples) {
    foreach ($cs in $s.CounterSamples) {
        $n = ($cs.Path -split '\\')[-1]
        if (-not $rows.ContainsKey($n)) { $rows[$n] = New-Object System.Collections.ArrayList }
        [void]$rows[$n].Add([double]$cs.CookedValue)
    }
}
foreach ($k in ($rows.Keys | Sort-Object)) {
    $v = $rows[$k] | Sort-Object
    $mean = ($v | Measure-Object -Average).Average
    $max = ($v | Measure-Object -Maximum).Maximum
    '{0,-62} mean={1,8:F2}  max={2,8:F2}' -f $k, $mean, $max
}
$termProc = if ($term -and $term.ProcessId) { Get-Process -Id $term.ProcessId -EA SilentlyContinue } else { $null }
$termCpu = if ($termProc) { $termProc.TotalProcessorTime.TotalMilliseconds - $termCpu0 } else { 0 }

'{0,-62} {1,8:F1}%' -f "dwm(rdp s$rdpSession) CPU of wall", (100.0 * $dwmCpu / $elapsed)
'{0,-62} {1,8:F1}%' -f 'WUDFHost(RDPIDD) CPU of wall', (100.0 * $wudfCpu / $elapsed)
'{0,-62} {1,8:F1}%' -f 'svchost(TermService: RDP encode) CPU of wall', (100.0 * $termCpu / $elapsed)
'{0,-62} {1,8:F0} ms' -f 'wall', $elapsed
