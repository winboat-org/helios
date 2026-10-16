param(
  [string]$PackagingRoot = $env:WB_PACKAGING_ROOT,
  # RELEASE by default, matching install-helios-kmd.ps1. Pass
  # -UmdDll ...\target\debug\helios_umd.dll for a deliberate debug deploy.
  [string]$UmdDll = "C:\Users\Rupansh\helios-vgpu\umd\target\release\helios_umd.dll",
  # Optional native D3D12 replacement. When omitted, preserve the installed
  # UserModeDriverName[3] exactly; the package may already enable D3D12.
  [string]$Umd12Dll = "",
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
. (Join-Path $PackagingRoot "Helios-PackageCommon.ps1")

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

function Get-UmdRegistration($Key, [string]$Name, [switch]$Optional) {
  if ($Name -notin $Key.GetValueNames()) {
    if ($Optional) { return }
    throw "$Name is missing; activate a complete Helios package before hotplug."
  }
  if ($Key.GetValueKind($Name) -ne [Microsoft.Win32.RegistryValueKind]::MultiString) {
    throw "$Name must be REG_MULTI_SZ."
  }
  $paths = @($Key.GetValue($Name))
  if ($paths.Count -ne 4) { throw "$Name has $($paths.Count) entries, want 4." }
  foreach ($path in $paths) {
    if ($path -isnot [string] -or [string]::IsNullOrWhiteSpace($path) -or
        $path -ne $path.Trim() -or $path -notmatch '(?i)(^|[\\/])[^\\/:*?"<>|]+\.dll$') {
      throw "$Name contains an invalid DLL registration: '$path'."
    }
  }
  return $paths
}

function Assert-UmdRegistrationEqual([string]$Name, [string[]]$Expected, [string[]]$Actual) {
  if ($Expected.Count -ne $Actual.Count) { throw "$Name changed length unexpectedly." }
  for ($i = 0; $i -lt $Expected.Count; $i++) {
    if ($Expected[$i] -cne $Actual[$i]) {
      throw "$Name[$i] is '$($Actual[$i])', expected '$($Expected[$i])'."
    }
  }
}

Assert-HeliosAdmin
if (-not (Test-Path -LiteralPath $UmdDll -PathType Leaf)) { throw "UMD DLL not found: $UmdDll" }
Assert-HeliosPeArchitecture $UmdDll x64
$deployUmd12 = -not [string]::IsNullOrWhiteSpace($Umd12Dll)
if ($deployUmd12) {
  if (-not (Test-Path -LiteralPath $Umd12Dll -PathType Leaf)) { throw "D3D12 UMD DLL not found: $Umd12Dll" }
  Assert-HeliosPeArchitecture $Umd12Dll x64
  if ($Mode -ne "ProgramData") {
    throw "-Umd12Dll is only supported in -Mode ProgramData; use the complete package installer to update packaged D3D12 binaries."
  }
}
$id = Get-HeliosInstanceId $InstanceId
$srcHash = Get-HeliosFileHash $UmdDll
$src12Hash = if ($deployUmd12) { Get-HeliosFileHash $Umd12Dll } else { "" }
$classKey = Get-HeliosClassKey $id
$activeInf = Get-HeliosActiveInfName $id
$store = Get-HeliosActiveStoreDir $id $activeInf
if ($Mode -eq "ProgramData") {
  # This local debug helper accepts ordinary filesystem paths, not UNC or
  # extended/device namespaces that can alias a protected DriverStore path.
  if ($ProgramDataDir -match '^(\\\\|//)') {
    throw "ProgramDataDir must use an ordinary local drive path: $ProgramDataDir"
  }
  $ProgramDataDir = [IO.Path]::GetFullPath($ProgramDataDir)
  if ([IO.Path]::DirectorySeparatorChar -eq '\' -and $ProgramDataDir -notmatch '^[A-Za-z]:\\') {
    throw "ProgramDataDir must use an ordinary local drive path: $ProgramDataDir"
  }
}
$programDataDll = Join-Path $ProgramDataDir ("helios_umd_{0}.dll" -f $srcHash.Substring(0, 16).ToLowerInvariant())
$programData12Dll = if ($deployUmd12) {
  Join-Path $ProgramDataDir ("helios_umd12_{0}.dll" -f $src12Hash.Substring(0, 16).ToLowerInvariant())
} else { "" }

if ($Mode -eq "ProgramData") {
  # Validate the existing registration before any file, service or device changes.
  # Native-only historical packages may omit WoW64, but a present value must be
  # a complete table. This helper never writes UserModeDriverNameWoW.
  $key = Get-Item -LiteralPath $classKey
  $nativePaths = @(Get-UmdRegistration $key "UserModeDriverName")
  $wowPaths = @(Get-UmdRegistration $key "UserModeDriverNameWoW" -Optional)
  $umdPaths = @($programDataDll, $programDataDll, $programDataDll, $nativePaths[3])
  if ($deployUmd12) { $umdPaths[3] = $programData12Dll }
  # InstalledDisplayDrivers is a flat inventory, not an indexed DDI table.
  $seenNames = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
  $umdNames = @(foreach ($path in ($umdPaths + $wowPaths)) {
    $name = [IO.Path]::GetFileNameWithoutExtension($path)
    if ($seenNames.Add($name)) { $name }
  })

  # ProgramData mode must not become a DriverStore edit via an alternate path.
  $destinationRoot = [IO.Path]::GetFullPath($ProgramDataDir).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
  foreach ($protected in @($store, (Join-Path $env:windir 'System32\DriverStore'))) {
    $protectedRoot = [IO.Path]::GetFullPath($protected).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    if ($destinationRoot.StartsWith($protectedRoot, [StringComparison]::OrdinalIgnoreCase)) {
      throw "ProgramDataDir must be outside DriverStore: $ProgramDataDir"
    }
  }
  # Lexical path checks do not follow junctions. Refuse any reparse point in
  # the destination ancestry, including an existing leaf or a dangling link.
  # Missing directories are allowed; access/provider errors remain fatal.
  $ancestor = $ProgramDataDir
  while ($ancestor) {
    $item = $null
    try { $item = Get-Item -LiteralPath $ancestor -Force -ErrorAction Stop }
    catch [System.Management.Automation.ItemNotFoundException] { }
    if ($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
      throw "ProgramDataDir must not traverse a reparse point: $ancestor"
    }
    $parent = Split-Path -Parent $ancestor
    if ($parent -eq $ancestor) { break }
    $ancestor = $parent
  }
} elseif ($Mode -eq "DriverStore" -and -not $ForceDriverStoreEdit) {
  throw "-Mode DriverStore edits the active DriverStore package. Pass -ForceDriverStoreEdit only for an emergency debug override."
} elseif ($Mode -eq "PackageUpgrade") {
  $inf = Join-Path $PackageDir "helios_kmd_render.inf"
  if (-not (Test-Path -LiteralPath $inf -PathType Leaf)) { throw "Package INF not found: $inf" }
}

Write-HeliosPlan "Helios UMD hotplug" @{
  Mode = $Mode
  Source = $UmdDll
  SourceHash = $srcHash
  Source12 = if ($deployUmd12) { $Umd12Dll } else { "(not deployed)" }
  Source12Hash = if ($deployUmd12) { $src12Hash } else { "(n/a)" }
  Instance = $id
  ClassKey = $classKey
  ActiveInf = $activeInf
  DriverStore = $store
  ProgramDataDll = $programDataDll
  RestartDevice = [bool]$RestartDevice
  ForceDriverStoreEdit = [bool]$ForceDriverStoreEdit
}
if ($PlanOnly) { return }

Stop-LookingGlassHostService
$cleared = Clear-HeliosPendingRenames
if ($cleared -gt 0) { Write-Host "Removed $cleared stale Helios pending rename operation(s)." }

if ($Mode -eq "PackageUpgrade") {
  Invoke-HeliosPnpUtil @("/add-driver", $inf, "/install") 120 | Out-Null
  if ($RestartDevice) {
    Invoke-HeliosPnpUtil @("/restart-device", $id) 90 | Out-Null
  } else {
    Write-Host "Package upgraded. Skipping adapter restart; reboot or pass -RestartDevice for a controlled test."
  }
} elseif ($Mode -eq "DriverStore") {
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

    if ($deployUmd12) {
      $copy12 = Copy-HeliosFileVerified $Umd12Dll $programData12Dll 10 1000
      Grant-HeliosReadExecute $ProgramDataDir
      Write-Host "Installed ProgramData D3D12 UMD: $($copy12.Destination)"
    }
    New-ItemProperty -LiteralPath $classKey -Name "UserModeDriverName" -PropertyType MultiString -Value $umdPaths -Force | Out-Null
    New-ItemProperty -LiteralPath $classKey -Name "InstalledDisplayDrivers" -PropertyType MultiString -Value $umdNames -Force | Out-Null
    Write-Host "Installed ProgramData UMD: $($copy.Destination)"
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
$deployedUmd = if ($Mode -eq "ProgramData") { $programDataDll } else { Join-Path (Get-HeliosActiveStoreDir $id (Get-HeliosActiveInfName $id)) "helios_umd.dll" }
$deployedHash = Get-HeliosFileHash $deployedUmd
Write-Host "Deployed UMD file: $deployedUmd"
Write-Host "Deployed SHA256:   $deployedHash"
if ($deployedHash -ne $srcHash) { throw "UMD hotplug failed: destination hash $deployedHash does not match source $srcHash" }

if ($Mode -eq "ProgramData") {
  if ($deployUmd12) {
    $deployed12Hash = Get-HeliosFileHash $programData12Dll
    Write-Host "Deployed D3D12 UMD file: $programData12Dll"
    Write-Host "Deployed D3D12 SHA256:   $deployed12Hash"
    if ($deployed12Hash -ne $src12Hash) { throw "D3D12 UMD hotplug failed: destination hash $deployed12Hash does not match source $src12Hash" }
  }

  $key = Get-Item -LiteralPath $classKey
  $registered = @(Get-UmdRegistration $key "UserModeDriverName")
  Assert-UmdRegistrationEqual "UserModeDriverName" $umdPaths $registered
  $registeredWow = @(Get-UmdRegistration $key "UserModeDriverNameWoW" -Optional)
  Assert-UmdRegistrationEqual "UserModeDriverNameWoW" $wowPaths $registeredWow
  if ($key.GetValueKind("InstalledDisplayDrivers") -ne [Microsoft.Win32.RegistryValueKind]::MultiString) {
    throw "InstalledDisplayDrivers must be REG_MULTI_SZ."
  }
  $installed = @($key.GetValue("InstalledDisplayDrivers"))
  Assert-UmdRegistrationEqual "InstalledDisplayDrivers" $umdNames $installed
  Write-Host "Registered native UMD paths: $($registered -join ', ')"
  Write-Host "InstalledDisplayDrivers:     $($installed -join ', ')"
  Write-Warning "Registration and file hashes are verified; loaded modules are not. Windows may retain a cached UMD path, even for new processes. If the override is not selected, activate a complete package through the normal installer and perform its required restart, then verify the loaded module paths. DriverStore was not modified."
}

if (-not $NoProbe -and (Test-Path -LiteralPath $Probe -PathType Leaf)) {
  Write-Host "Running D3D11 probe: $Probe"
  $probeResult = Invoke-HeliosProcess -FilePath $Probe -Arguments @() -TimeoutSeconds 20
  if ($probeResult.ExitCode -ne 0) {
    Write-Warning "Probe exit code $($probeResult.ExitCode). Check output above."
  }
} elseif (-not $NoProbe) {
  Write-Warning "Probe not found: $Probe"
}
