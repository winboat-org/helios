param(
  [string]$PackageDir = "C:\Users\Rupansh\helios-vgpu\kmd_render\target\debug\helios_kmd_render_package",
  [string]$UmdDll = "C:\Users\Rupansh\helios-vgpu\umd\target\debug\helios_umd.dll",
  [string]$InstanceId = "",
  [switch]$SkipSign,
  [switch]$BinaryOnly,
  [switch]$IncludeUmd,
  [switch]$RemoveRedHatViogpu3d,
  [switch]$AllowRebootRequired,
  [switch]$RestartDevice,
  [switch]$StageOnly,
  [switch]$KeepFailedPackage,
  [switch]$ForceDriverStoreEdit,
  [switch]$UsePnPUtil,
  [switch]$PlanOnly
)

. "$PSScriptRoot\helios-deploy-common.ps1"

function Find-Signtool {
  $cmd = Get-Command signtool.exe -ErrorAction SilentlyContinue
  if ($cmd) { return $cmd.Source }
  $kits = @(
    "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe",
    "${env:ProgramFiles}\Windows Kits\10\bin\*\x64\signtool.exe"
  )
  foreach ($glob in $kits) {
    $hit = Get-ChildItem $glob -ErrorAction SilentlyContinue | Sort-Object FullName -Descending | Select-Object -First 1
    if ($hit) { return $hit.FullName }
  }
  return $null
}

function Find-Inf2Cat {
  $cmd = Get-Command Inf2Cat.exe -ErrorAction SilentlyContinue
  if ($cmd) { return $cmd.Source }
  $kits = @(
    "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\Inf2Cat.exe",
    "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x86\Inf2Cat.exe",
    "${env:ProgramFiles}\Windows Kits\10\bin\*\x64\Inf2Cat.exe",
    "${env:ProgramFiles}\Windows Kits\10\bin\*\x86\Inf2Cat.exe"
  )
  foreach ($glob in $kits) {
    $hit = Get-ChildItem $glob -ErrorAction SilentlyContinue | Sort-Object FullName -Descending | Select-Object -First 1
    if ($hit) { return $hit.FullName }
  }
  return $null
}

function Find-DevCon {
  $cmd = Get-Command devcon.exe -ErrorAction SilentlyContinue
  if ($cmd) { return $cmd.Source }
  $kits = @(
    "${env:ProgramFiles(x86)}\Windows Kits\10\Tools\*\x64\devcon.exe",
    "${env:ProgramFiles}\Windows Kits\10\Tools\*\x64\devcon.exe",
    "${env:ProgramFiles(x86)}\Windows Kits\10\Tools\*\x86\devcon.exe",
    "${env:ProgramFiles}\Windows Kits\10\Tools\*\x86\devcon.exe"
  )
  foreach ($glob in $kits) {
    $hit = Get-ChildItem $glob -ErrorAction SilentlyContinue | Sort-Object FullName -Descending | Select-Object -First 1
    if ($hit) { return $hit.FullName }
  }
  return $null
}

function Get-HeliosHardwareId([string]$InstanceId) {
  $prop = Get-PnpDeviceProperty -InstanceId $InstanceId -KeyName DEVPKEY_Device_HardwareIds -ErrorAction SilentlyContinue
  if ($prop -and $prop.Data) {
    foreach ($hwid in @($prop.Data)) {
      if ([string]$hwid -like "PCI\VEN_1AF4&DEV_1050*") { return [string]$hwid }
    }
  }

  $lastSlash = $InstanceId.LastIndexOf("\")
  if ($lastSlash -gt 0) { return $InstanceId.Substring(0, $lastSlash) }
  return $InstanceId
}

function New-HeliosCatalog([string]$PackageDir, [string]$CatPath) {
  $inf2cat = Find-Inf2Cat
  if (-not $inf2cat) {
    throw "Inf2Cat.exe not found; refusing to install KMD package with a stale catalog."
  }

  Grant-HeliosWritable $CatPath
  & icacls.exe $PackageDir /grant "*S-1-5-32-544:F" | Out-Null
  Remove-Item -LiteralPath $CatPath -Force -ErrorAction SilentlyContinue
  $osTargets = @("10_GE_X64", "10_NI_X64", "10_X64")
  foreach ($os in $osTargets) {
    Write-Host "Regenerating catalog with Inf2Cat /os:$os"
    & $inf2cat /driver:$PackageDir /os:$os /uselocaltime
    if ($LASTEXITCODE -eq 0 -and (Test-Path -LiteralPath $CatPath -PathType Leaf)) {
      return
    }
    Write-Warning "Inf2Cat failed for /os:$os (exit $LASTEXITCODE)."
  }

  throw "Inf2Cat failed to regenerate $CatPath"
}

function Ensure-MachineCodeSigningCert {
  $subject = "CN=WDRLocalTestCert"
  $cert = Get-ChildItem Cert:\LocalMachine\My -ErrorAction SilentlyContinue |
    Where-Object { $_.Subject -eq $subject -and $_.HasPrivateKey } |
    Sort-Object NotAfter -Descending |
    Select-Object -First 1

  if (-not $cert) {
    Write-Host "Creating machine code-signing certificate $subject"
    $cert = New-SelfSignedCertificate `
      -Subject $subject `
      -Type CodeSigningCert `
      -CertStoreLocation Cert:\LocalMachine\My `
      -KeyAlgorithm RSA `
      -KeyLength 2048 `
      -HashAlgorithm SHA256 `
      -KeyUsage DigitalSignature `
      -NotAfter (Get-Date).AddYears(10)
  }

  $tmp = Join-Path $env:TEMP "WDRLocalTestCert.cer"
  Export-Certificate -Cert $cert -FilePath $tmp | Out-Null
  Import-Certificate -FilePath $tmp -CertStoreLocation Cert:\LocalMachine\Root | Out-Null
  Import-Certificate -FilePath $tmp -CertStoreLocation Cert:\LocalMachine\TrustedPublisher | Out-Null
  Remove-Item $tmp -Force -ErrorAction SilentlyContinue
  return $cert
}

function Sign-FileWithMachineCert([string]$Path) {
  if ($SkipSign) { return }
  $cert = Ensure-MachineCodeSigningCert
  $signtool = Find-Signtool
  if (-not $signtool) {
    Write-Warning "signtool.exe not found; leaving existing signature unchanged for $Path."
    return
  }
  & $signtool sign /v /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 /sm /s My /sha1 $cert.Thumbprint $Path
  if ($LASTEXITCODE -ne 0) {
    Write-Warning "Timestamped signtool failed for $Path; retrying without timestamp."
    & $signtool sign /v /fd SHA256 /sm /s My /sha1 $cert.Thumbprint $Path
  }
  if ($LASTEXITCODE -ne 0) { throw "signtool failed for $Path" }
}

function Sign-HeliosPackage([string]$SysPath, [string]$CatPath) {
  Sign-FileWithMachineCert $SysPath
  Sign-FileWithMachineCert $CatPath
}

function Sync-HeliosPackageUmd([string]$Source, [string]$Destination) {
  if ($BinaryOnly -and -not $IncludeUmd) { return }
  if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) {
    throw "UMD DLL not found: $Source"
  }
  $srcHash = Get-HeliosFileHash $Source
  $dstHash = if (Test-Path -LiteralPath $Destination -PathType Leaf) { Get-HeliosFileHash $Destination } else { "" }
  if ($srcHash -ne $dstHash) {
    Copy-Item -LiteralPath $Source -Destination $Destination -Force
    $newHash = Get-HeliosFileHash $Destination
    if ($newHash -ne $srcHash) {
      throw "Package UMD copy hash mismatch. source=$srcHash package=$newHash"
    }
    Write-Host "Refreshed package UMD: $Destination hash $newHash"
  } else {
    Write-Host "Package UMD already current: $Destination hash $dstHash"
  }
}

function Restore-HeliosPreviousPackage([string]$InstanceId, [string]$PreviousInf, [string]$CurrentInf) {
  if ($CurrentInf -and $CurrentInf -ne $PreviousInf) {
    Write-Warning "Removing failed Helios package $CurrentInf and returning to $PreviousInf."
    & pnputil.exe /delete-driver "$CurrentInf" /uninstall /force | Out-Host
  } else {
    Write-Warning "Current Helios package is still $CurrentInf; cycling device to recover $PreviousInf."
  }
  & pnputil.exe /scan-devices | Out-Host
  & pnputil.exe /disable-device "$InstanceId" /force | Out-Host
  Start-Sleep -Seconds 2
  & pnputil.exe /enable-device "$InstanceId" | Out-Host
  Start-Sleep -Seconds 3
}

function Test-HeliosRebootRequiredFailure([string]$InstanceId) {
  $text = (& pnputil.exe /enum-devices /instanceid "$InstanceId" /drivers) -join "`n"
  return ($text -match "Problem Status:\s+0xC0000182")
}

function Test-HeliosNeedRestartState($State) {
  if (-not $State) { return $false }
  $code = [string]$State.ConfigManagerErrorCode
  return ($code -eq "14" -or $code -eq "CM_PROB_NEED_RESTART")
}

function Invoke-HeliosPackageUpdate([string]$InfPath, [string]$InstanceId) {
  $hwid = Get-HeliosHardwareId $InstanceId
  if (-not $UsePnPUtil) {
    $devcon = Find-DevCon
    if ($devcon) {
      Write-Host "Publishing Helios package with devcon update"
      $result = Invoke-HeliosProcess -FilePath $devcon -Arguments @("update", $InfPath, $hwid) -TimeoutSeconds 180
      if ($result.ExitCode -eq 0 -or $result.Stdout -match "Drivers installed successfully|Restart required|reboot") {
        return [pscustomobject]@{
          Tool = "devcon"
          RestartRequired = [bool]($result.Stdout -match "(?i)(restart required|reboot required|requires (a )?reboot|reboot is required)")
          Stdout = $result.Stdout
          Stderr = $result.Stderr
        }
      }
      throw "devcon update failed with exit $($result.ExitCode)"
    }
    Write-Warning "devcon.exe not found; falling back to pnputil /add-driver /install."
  }

  Write-Host "Publishing Helios package with pnputil /add-driver /install"
  $result = Invoke-HeliosProcess -FilePath "pnputil.exe" -Arguments @("/add-driver", $InfPath, "/install") -TimeoutSeconds 180
  if ($result.ExitCode -ne 0 -and $result.Stdout -notmatch "Driver package added successfully") {
    throw "pnputil /add-driver failed with exit $($result.ExitCode)"
  }
  return [pscustomobject]@{
    Tool = "pnputil"
    RestartRequired = [bool]($result.Stdout -match "(?i)(restart required|reboot required|requires (a )?reboot|reboot is required)")
    Stdout = $result.Stdout
    Stderr = $result.Stderr
  }
}

function Publish-HeliosPackageOnly([string]$InfPath) {
  Write-Host "Staging Helios package with pnputil /add-driver"
  $result = Invoke-HeliosProcess -FilePath "pnputil.exe" -Arguments @("/add-driver", $InfPath) -TimeoutSeconds 120
  if ($result.ExitCode -ne 0 -and $result.Stdout -notmatch "Driver package added successfully") {
    throw "pnputil /add-driver failed with exit $($result.ExitCode)"
  }
  if ($result.Stdout -match "Published Name:\s+(oem\d+\.inf)") {
    Write-Host "Published Helios package as $($Matches[1])"
  }
}

function Backup-HeliosActiveFiles([string]$Store, [string[]]$Names) {
  $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
  $dir = Join-Path "C:\ProgramData\HeliosDeployBackups" $stamp
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  foreach ($name in $Names) {
    $src = Join-Path $Store $name
    if (Test-Path -LiteralPath $src -PathType Leaf) {
      Copy-Item -LiteralPath $src -Destination (Join-Path $dir $name) -Force
    }
  }
  Write-Host "Backed up active DriverStore files to $dir"
  return $dir
}

Assert-HeliosAdmin
Stop-LookingGlassHostService
$cleared = Clear-HeliosPendingRenames
if ($cleared -gt 0) { Write-Host "Removed $cleared stale Helios pending rename operation(s)." }

if (-not (Test-Path -LiteralPath $PackageDir -PathType Container)) { throw "PackageDir not found: $PackageDir" }
$inf = Join-Path $PackageDir "helios_kmd_render.inf"
$sys = Join-Path $PackageDir "helios_kmd_render.sys"
$umd = Join-Path $PackageDir "helios_umd.dll"
$cat = Join-Path $PackageDir "helios_kmd_render.cat"
foreach ($path in @($inf, $sys, $cat)) {
  if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing package file $path" }
}
if ((-not $BinaryOnly -or $IncludeUmd) -and -not (Test-Path -LiteralPath $umd -PathType Leaf)) { throw "Missing package UMD file $umd" }

if ($StageOnly) {
  Write-HeliosPlan "Helios KMD stage-only install" @{
    PackageDir = $PackageDir
    UmdSource = $UmdDll
    StageOnly = [bool]$StageOnly
  }
  if ($PlanOnly) { return }
  Sync-HeliosPackageUmd $UmdDll $umd
  New-HeliosCatalog $PackageDir $cat
  Sign-HeliosPackage $sys $cat
  Publish-HeliosPackageOnly $inf
  Write-Host "Helios KMD package staged. Reboot/start with the Helios PCI device present, then bind with devcon update if PnP does not pick the highest DriverVer automatically."
  return
}

$id = Get-HeliosInstanceId $InstanceId
$hwid = Get-HeliosHardwareId $id
$activeInf = Get-HeliosActiveInfName $id
$store = Get-HeliosActiveStoreDir $id $activeInf
$copyNames = if ($BinaryOnly) { @("helios_kmd_render.sys", "helios_kmd_render.cat") } else { @("helios_kmd_render.inf", "helios_kmd_render.sys", "helios_kmd_render.cat", "helios_umd.dll") }
if ($IncludeUmd) { $copyNames += "helios_umd.dll" }
$copyNames = $copyNames | Select-Object -Unique

Write-HeliosPlan "Helios KMD install" @{
  PackageDir = $PackageDir
  Instance = $id
  HardwareId = $hwid
  ActiveInf = $activeInf
  DriverStore = $store
  UmdSource = $UmdDll
  BinaryOnly = [bool]$BinaryOnly
  IncludeUmd = [bool]$IncludeUmd
  AllowRebootRequired = [bool]$AllowRebootRequired
  RestartDevice = [bool]$RestartDevice
  StageOnly = [bool]$StageOnly
  KeepFailedPackage = [bool]$KeepFailedPackage
  ForceDriverStoreEdit = [bool]$ForceDriverStoreEdit
  FullInstallTool = $(if ($UsePnPUtil) { "pnputil" } elseif (Find-DevCon) { "devcon" } else { "pnputil fallback" })
  Files = ($copyNames -join ", ")
}
if ($PlanOnly) { return }

Sync-HeliosPackageUmd $UmdDll $umd
New-HeliosCatalog $PackageDir $cat
Sign-HeliosPackage $sys $cat

if (-not $BinaryOnly) {
  $backup = Backup-HeliosActiveFiles $store $copyNames
  $oldImagePath = (Get-ItemProperty -LiteralPath "HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render" -ErrorAction SilentlyContinue).ImagePath
  $update = Invoke-HeliosPackageUpdate $inf $id
  $newActiveInf = Get-HeliosActiveInfName $id
  $newStore = Get-HeliosActiveStoreDir $id $newActiveInf
  $newImagePath = (Get-ItemProperty -LiteralPath "HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render" -ErrorAction SilentlyContinue).ImagePath
  $activeSys = Join-Path $newStore "helios_kmd_render.sys"
  $packageSysHash = Get-HeliosFileHash $sys
  $activeSysHash = Get-HeliosFileHash $activeSys
  if ($activeSysHash -ne $packageSysHash) {
    throw "devcon/pnputil did not bind the staged KMD image. activeInf=$newActiveInf activeSys=$activeSys activeHash=$activeSysHash packageHash=$packageSysHash. Bump DriverVer or remove the stale active package with pnputil; do not continue with a stale DriverStore image."
  }
  Write-Host "Active KMD image verified: $activeSys hash $activeSysHash"
  $serviceImageChanged = ($oldImagePath -and $newImagePath -and ([string]$oldImagePath -ne [string]$newImagePath))
  if ($serviceImageChanged -or $update.RestartRequired) {
    Write-Warning "The Helios service image path changed or SetupAPI reported restart required. A reboot is the reliable KMD activation path for this package."
    Write-Host "Old ImagePath: $oldImagePath"
    Write-Host "New ImagePath: $newImagePath"
    if (-not $RestartDevice) {
      if ($AllowRebootRequired) {
        Write-Host "Leaving the package installed for the next boot. Backup: $backup"
        return
      }
      throw "KMD package is staged and bound but requires reboot to activate. Re-run with -AllowRebootRequired to leave it installed intentionally."
    }
  }

  if ($RestartDevice) {
    Write-Host "Restarting Helios device after package update"
    Invoke-HeliosPnpUtil @("/restart-device", $id) 120 | Out-Null
  } else {
    Write-Host "Skipping PnP device restart. Use -RestartDevice only for controlled tests; reboot is safer after KMD image path changes."
  }

  Start-Sleep -Seconds 3
  $state = Get-HeliosPnpState $id
  $state | Format-List
  if ($state.ConfigManagerErrorCode -ne 0) {
    Write-Warning "Helios did not return Code 0. Backup is at $backup"
    & pnputil.exe /enum-devices /instanceid "$id" /drivers
    if ($AllowRebootRequired -and ((Test-HeliosNeedRestartState $state) -or (Test-HeliosRebootRequiredFailure $id))) {
      Write-Warning "Helios requires a reboot to finish the KMD package change; leaving the new package installed by request."
      Write-Host "Reboot, then verify Helios is Code 0 before continuing. Backup: $backup"
      return
    }
    if ($KeepFailedPackage) {
      Write-Warning "Leaving failed Helios package installed by request for debugging. Backup is at $backup"
      return
    }
    $newInf = Get-HeliosActiveInfName $id
    Restore-HeliosPreviousPackage $id $activeInf $newInf
    throw "Helios KMD package published, but device is Code $($state.ConfigManagerErrorCode)."
  }
  Write-Host "Helios KMD package install verified. Backup: $backup"
  return
}

if (-not $ForceDriverStoreEdit) {
  throw "-BinaryOnly edits the active DriverStore package. This is disabled unless -ForceDriverStoreEdit is also passed."
}

if ($RemoveRedHatViogpu3d) {
  & pnputil.exe /enum-drivers /class Display | Out-String | Select-String -Pattern "Published Name|Provider Name|Class Name" | Out-Host
  Write-Warning "RemoveRedHatViogpu3d is intentionally manual: remove only the specific Red Hat viogpu3d oem#.inf after checking pnputil output."
}

$sources = @{
  "helios_kmd_render.inf" = $inf
  "helios_kmd_render.sys" = $sys
  "helios_kmd_render.cat" = $cat
  "helios_umd.dll" = $umd
}
$backup = Backup-HeliosActiveFiles $store $copyNames

Write-Warning "-BinaryOnly directly edits the active DriverStore package. Use it only as an emergency debug override; normal KMD installs must use devcon update or pnputil."
Write-Host "Stopping Helios device with pnputil /disable-device /force"
Invoke-HeliosPnpUtil @("/disable-device", $id, "/force") 90 | Out-Null
try {
  foreach ($name in $copyNames) {
    $dst = Join-Path $store $name
    $copy = Copy-HeliosFileVerified $sources[$name] $dst 5 750
    Write-Host "Installed $name hash $($copy.Hash)"
  }
} finally {
  Write-Host "Starting Helios device with pnputil /enable-device"
  Invoke-HeliosPnpUtil @("/enable-device", $id) 90 | Out-Null
}

Start-Sleep -Seconds 3
$state = Get-HeliosPnpState $id
$state | Format-List
if ($state.ConfigManagerErrorCode -ne 0) {
  Write-Warning "Helios did not return Code 0. Backup is at $backup"
  & pnputil.exe /enum-devices /instanceid "$id" /drivers
  throw "Helios KMD install completed copy, but device is Code $($state.ConfigManagerErrorCode)."
}

foreach ($name in $copyNames) {
  $dst = Join-Path $store $name
  $srcHash = Get-HeliosFileHash $sources[$name]
  $dstHash = Get-HeliosFileHash $dst
  if ($srcHash -ne $dstHash) { throw "Post-enable hash mismatch for $name. source=$srcHash active=$dstHash" }
}
Write-Host "Helios KMD install verified. Backup: $backup"
