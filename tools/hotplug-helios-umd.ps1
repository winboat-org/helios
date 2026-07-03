param(
  [string]$UmdDll = "C:\Users\Rupansh\helios-vgpu\umd\target\debug\helios_umd.dll",
  [ValidateSet("ProgramData", "DriverStore", "PackageUpgrade")]
  [string]$Mode = "ProgramData",
  [string]$PackageDir = "C:\Users\Rupansh\helios-vgpu\kmd_render\target\debug\helios_kmd_render_package",
  [string]$ProgramDataDir = "C:\ProgramData\HeliosUmd",
  [string]$InstanceId = "",
  [string]$Probe = "C:\Users\Rupansh\helios-probe\d3d11_devicecreate_probe.exe",
  [switch]$KillUmdUsers,
  [switch]$RestartDevice,
  [switch]$ForceDriverStoreEdit,
  [switch]$NoProbe,
  [switch]$PlanOnly
)

. "$PSScriptRoot\helios-deploy-common.ps1"

function Stop-UmdUsers([string]$DllPath) {
  $paths = @()
  if ($DllPath) { $paths += $DllPath }
  if (Test-Path -LiteralPath $ProgramDataDir -PathType Container) {
    $paths += @(Get-ChildItem -LiteralPath $ProgramDataDir -Filter "helios_umd*.dll" -ErrorAction SilentlyContinue | ForEach-Object FullName)
  }

  $seen = @{}
  foreach ($path in ($paths | Sort-Object -Unique)) {
    foreach ($u in @(Get-HeliosFileUsers $path)) {
      if ($seen.ContainsKey($u.Id)) { continue }
      $seen[$u.Id] = $true
      if ($u.ProcessName -notin @("Idle", "System", "Registry", "smss", "csrss", "wininit", "services", "lsass", "fontdrvhost")) {
        Write-Host "Stopping UMD user $($u.ProcessName)[$($u.Id)] using $path"
        Stop-Process -Id $u.Id -Force -ErrorAction SilentlyContinue
      }
    }
  }
}

Assert-HeliosAdmin
Stop-LookingGlassHostService
$cleared = Clear-HeliosPendingRenames
if ($cleared -gt 0) { Write-Host "Removed $cleared stale Helios pending rename operation(s)." }

if (-not (Test-Path -LiteralPath $UmdDll -PathType Leaf)) { throw "UMD DLL not found: $UmdDll" }
$id = Get-HeliosInstanceId $InstanceId
$srcHash = Get-HeliosFileHash $UmdDll
$classKey = Get-HeliosClassKey $id
$activeInf = Get-HeliosActiveInfName $id
$store = Get-HeliosActiveStoreDir $id $activeInf
$programDataDll = Join-Path $ProgramDataDir ("helios_umd_{0}.dll" -f $srcHash.Substring(0, 16).ToLowerInvariant())

Write-HeliosPlan "Helios UMD hotplug" @{
  Mode = $Mode
  Source = $UmdDll
  SourceHash = $srcHash
  Instance = $id
  ClassKey = $classKey
  ActiveInf = $activeInf
  DriverStore = $store
  ProgramDataDll = $programDataDll
  RestartDevice = [bool]$RestartDevice
  ForceDriverStoreEdit = [bool]$ForceDriverStoreEdit
}
if ($PlanOnly) { return }

if ($Mode -eq "PackageUpgrade") {
  $inf = Join-Path $PackageDir "helios_kmd_render.inf"
  if (-not (Test-Path -LiteralPath $inf -PathType Leaf)) { throw "Package INF not found: $inf" }
  Invoke-HeliosPnpUtil @("/add-driver", $inf, "/install") 120 | Out-Null
  if ($RestartDevice) {
    Invoke-HeliosPnpUtil @("/restart-device", $id) 90 | Out-Null
  } else {
    Write-Host "Package upgraded. Skipping adapter restart; reboot or pass -RestartDevice for a controlled test."
  }
} elseif ($Mode -eq "DriverStore") {
  if (-not $ForceDriverStoreEdit) {
    throw "-Mode DriverStore edits the active DriverStore package. Pass -ForceDriverStoreEdit only for an emergency debug override."
  }
  $dst = Join-Path $store "helios_umd.dll"
  if ($RestartDevice) { Invoke-HeliosPnpUtil @("/disable-device", $id, "/force") 90 | Out-Null }
  try {
    if ($KillUmdUsers) { Stop-UmdUsers $dst }
    $copy = Copy-HeliosFileVerified $UmdDll $dst 5 750
    Write-Host "Installed DriverStore UMD: $($copy.Destination)"
  } finally {
    if ($RestartDevice) { Invoke-HeliosPnpUtil @("/enable-device", $id) 90 | Out-Null }
  }
} else {
  New-Item -ItemType Directory -Force -Path $ProgramDataDir | Out-Null
  Grant-HeliosReadExecute $ProgramDataDir
  if ($RestartDevice) {
    Write-Host "Disabling Helios before replacing ProgramData UMD."
    Invoke-HeliosPnpUtil @("/disable-device", $id, "/force") 90 | Out-Null
  }
  try {
    if ($KillUmdUsers) { Stop-UmdUsers $programDataDll }
    $copy = Copy-HeliosFileVerified $UmdDll $programDataDll 10 1000
    Grant-HeliosReadExecute $ProgramDataDir
    $umdNames = @("helios_umd", "helios_umd", "helios_umd", "helios_umd")
    $umdPaths = @($programDataDll, $programDataDll, $programDataDll, $programDataDll)
    New-ItemProperty -LiteralPath $classKey -Name "UserModeDriverName" -PropertyType MultiString -Value $umdPaths -Force | Out-Null
    New-ItemProperty -LiteralPath $classKey -Name "InstalledDisplayDrivers" -PropertyType MultiString -Value $umdNames -Force | Out-Null
    Write-Host "Installed ProgramData UMD: $($copy.Destination)"
    # ALSO sync the active DriverStore package copy: at COLD BOOT dxgkrnl's
    # first UMD-path resolution loads the package's helios_umd.dll (before the
    # registry override takes effect for later device creates), so a stale
    # DriverStore copy means dwm's first — composition — device runs an old
    # UMD every boot (proven 2026-07-03: two different handler generations in
    # one dwm process, early devices on the stale DLL).
    $storeDll = Join-Path $store "helios_umd.dll"
    if (Test-Path -LiteralPath $storeDll -PathType Leaf) {
      & takeown.exe /F $storeDll | Out-Null
      & icacls.exe $storeDll /grant "Administrators:F" | Out-Null
      Copy-Item -LiteralPath $UmdDll -Destination $storeDll -Force
      Write-Host "Synced DriverStore UMD: $storeDll"
    } else {
      Write-Warning "DriverStore UMD not found at $storeDll - cold boots may load a stale UMD"
    }
  } finally {
    if ($RestartDevice) {
      Write-Host "Re-enabling Helios after ProgramData UMD replacement."
      Invoke-HeliosPnpUtil @("/enable-device", $id) 90 | Out-Null
      # The Helios PnP restart mints a new adapter LUID; the IDD's latched
      # render-adapter pairing then names a dead adapter and the OS never
      # re-offers a swapchain (observed 2026-07-04: endless no-AssignSwapChain
      # replug loop after a deploy). Restart the IDD so it re-pairs against
      # the fresh LUID. (LGIdd also revalidates the LUID on fruitless replugs
      # now — this keeps deploys deterministic rather than waiting on that.)
      $devcon = "C:\Program Files (x86)\Windows Kits\10\Tools\10.0.26100.0\x64\devcon.exe"
      if (Test-Path -LiteralPath $devcon) {
        Write-Host "Restarting the LG IDD so it re-pairs with the new Helios adapter LUID."
        & $devcon restart '@ROOT\DISPLAY\0000' | Out-Null
      } else {
        Write-Warning "devcon not found at $devcon; restart ROOT\DISPLAY\0000 manually so the IDD re-pairs."
      }
    }
  }
}

Start-Sleep -Seconds 2
$state = Get-HeliosPnpState $id
$state | Format-List
$activeUmd = if ($Mode -eq "ProgramData") { $programDataDll } else { Join-Path (Get-HeliosActiveStoreDir $id (Get-HeliosActiveInfName $id)) "helios_umd.dll" }
$activeHash = Get-HeliosFileHash $activeUmd
Write-Host "Active UMD:  $activeUmd"
Write-Host "Active hash: $activeHash"
if ($activeHash -ne $srcHash) { throw "UMD hotplug failed: active hash $activeHash does not match source $srcHash" }

if (-not $NoProbe -and (Test-Path -LiteralPath $Probe -PathType Leaf)) {
  Write-Host "Running D3D11 probe: $Probe"
  $probeResult = Invoke-HeliosProcess -FilePath $Probe -Arguments @() -TimeoutSeconds 20
  if ($probeResult.ExitCode -ne 0) {
    Write-Warning "Probe exit code $($probeResult.ExitCode). Check output above."
  }
} elseif (-not $NoProbe) {
  Write-Warning "Probe not found: $Probe"
}
