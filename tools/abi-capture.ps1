# Capture the CreateDevice ABI evidence lines from a fresh D3D11 process.
#
# The T5 gate asks for a `UmdTrace=1` run diffed against a pre-change run:
# `CreateDevice raw args:` walks D3D10DDIARG_CREATEDEVICE by word index and
# `CreateDevice interface=...` prints the fields R802 re-typed, so together they
# prove the runtime handed the same bytes to the same offsets before and after.
#
# UmdTrace is read once per process at device init, so this needs a NEW process;
# dwm keeps whatever it resolved at its own start. The soak harness is that new
# process (2 cycles is enough -- we want the ABI line, not a soak).
#
# ⚠ UMD logs are keyed by PID and APPENDED, never truncated, and Windows reuses
# pids. So "read the log file that appeared" does not work (it usually already
# exists from an older, unrelated process, sometimes from a previous boot), and
# neither does "read the last N lines". This launches the probe, keeps its ACTUAL
# pid, and reads only the tail of that pid's log after the last `UMD module:`
# marker -- the newest session's start.
#
#   ... -Label before   -> Z:\tmp\abi-before.txt
#   ... -Label after    -> Z:\tmp\abi-after.txt
param(
    [Parameter(Mandatory = $true)][string]$Label
)

$logDir = 'C:\ProgramData\Helios'
$out = "Z:\tmp\abi-$Label.txt"

# Knob on. Absent = off, so this is the only value that matters.
New-Item -Path 'HKLM:\SOFTWARE\Helios' -Force | Out-Null
Set-ItemProperty -Path 'HKLM:\SOFTWARE\Helios' -Name 'UmdTrace' -Value 1 -Type DWord

$exe = 'C:\Users\Rupansh\helios-probe\helios_ownership_soak.exe'
if (-not (Test-Path $exe)) { Write-Output "MISSING $exe -- run helios-ownership-soak.ps1 first"; exit 1 }

$p = Start-Process -FilePath $exe -ArgumentList '2', '2', '16384', 'helios' `
        -PassThru -NoNewWindow -RedirectStandardOutput 'C:\Users\Rupansh\helios-probe\abi-run.txt'
$pid_ = $p.Id
$p.WaitForExit()

$log = Join-Path $logDir ("umd-{0}.log" -f $pid_)
if (-not (Test-Path $log)) { Write-Output "no UMD log for pid $pid_"; exit 1 }

# Anchor to the newest session in that file.
$lines = Get-Content -LiteralPath $log
$starts = @()
for ($i = 0; $i -lt $lines.Count; $i++) { if ($lines[$i] -match 'UMD module:') { $starts += $i } }
$body = if ($starts.Count) { $lines[$starts[-1]..($lines.Count - 1)] } else { $lines }

$res = @("### pid $pid_  ($($body.Count) lines in the newest session)")
$res += ($body | Where-Object { $_ -match 'CreateDevice raw args:|CreateDevice interface=' })
if ($res.Count -le 1) { $res += '(no CreateDevice lines -- is UmdTrace really on for this process?)' }
$res | Set-Content -Path $out
Write-Output "=== $Label ==="
$res | ForEach-Object { Write-Output $_ }
Write-Output "(saved: $out)"
