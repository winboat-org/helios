# Sample the Helios ICD's process-wide vehicle counters from a live process
# via ReadProcessMemory (no redeploy, no perf-line dependency — the win32
# HELIOS_WSI_PERF line never prints on pure-vehicle runs).
# Usage: read_vehicle_counters.ps1 -ProcessName DOOMx64vk -Seconds 20
param(
  [string]$ProcessName = "DOOMx64vk",
  [int]$Seconds = 20,
  # RVAs for vulkan_virtio-d49cd875438d.dll (ImageBase 0x2ece80000):
  [long]$RvaDrops = 0x25c320,
  [long]$RvaPresents = 0x25c390,
  [long]$RvaGateArms = 0x25c3b0,
  [long]$RvaWaitTo = 0x25c420,
  [long]$RvaGateFb = 0x25c430
)

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class Rpm {
  [DllImport("kernel32.dll", SetLastError=true)]
  public static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
  [DllImport("kernel32.dll", SetLastError=true)]
  public static extern bool ReadProcessMemory(IntPtr h, IntPtr addr, byte[] buf, int n, out IntPtr read);
  [DllImport("kernel32.dll")]
  public static extern bool CloseHandle(IntPtr h);
}
"@

$p = Get-Process $ProcessName -ErrorAction Stop
$m = $p.Modules | Where-Object { $_.ModuleName -like 'vulkan_virtio*' } | Select-Object -First 1
if (-not $m) { throw "no vulkan_virtio module in $ProcessName" }
$base = [long]$m.BaseAddress
Write-Host ("module={0} base=0x{1:x}" -f $m.ModuleName, $base)

$h = [Rpm]::OpenProcess(0x0410, $false, $p.Id)
if ($h -eq [IntPtr]::Zero) { throw "OpenProcess failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())" }

function Read-Counter([long]$rva) {
  $buf = New-Object byte[] 4
  $read = [IntPtr]::Zero
  if (-not [Rpm]::ReadProcessMemory($h, [IntPtr]($base + $rva), $buf, 4, [ref]$read)) {
    throw "RPM failed at rva 0x$($rva.ToString('x')): $([Runtime.InteropServices.Marshal]::GetLastWin32Error())"
  }
  [BitConverter]::ToInt32($buf, 0)
}

$t0 = Get-Date
$s0 = @{ drops = Read-Counter $RvaDrops; presents = Read-Counter $RvaPresents;
         arms = Read-Counter $RvaGateArms; waitto = Read-Counter $RvaWaitTo; fb = Read-Counter $RvaGateFb }
Start-Sleep -Seconds $Seconds
$t1 = Get-Date
$s1 = @{ drops = Read-Counter $RvaDrops; presents = Read-Counter $RvaPresents;
         arms = Read-Counter $RvaGateArms; waitto = Read-Counter $RvaWaitTo; fb = Read-Counter $RvaGateFb }
[Rpm]::CloseHandle($h) | Out-Null

$dt = ($t1 - $t0).TotalSeconds
$dp = $s1.presents - $s0.presents
$dd = $s1.drops - $s0.drops
Write-Host ("presents={0} (+{1}) drops={2} (+{3}) gate_arms={4} (+{5}) gate_fb={6} (+{7}) wait_to={8} over {9:n1}s" -f `
  $s1.presents, $dp, $s1.drops, $dd, $s1.arms, ($s1.arms - $s0.arms), $s1.fb, ($s1.fb - $s0.fb), $s1.waitto, $dt)
Write-Host ("minted_fps={0:n1} app_fps={1:n1}" -f ($dp / $dt), (($dp + $dd) / $dt))
