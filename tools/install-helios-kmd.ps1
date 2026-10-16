param(
  [string]$PackagingRoot = $env:WB_PACKAGING_ROOT,
  # Artifact paths have no fallback: native paths are always required; x86
  # paths are also required when the INF declares UserModeDriverNameWoW.
  #
  # They used to carry hardcoded defaults, and on 2026-09-05 that cost a wrong
  # deploy that nothing in the output named: `cargo make` stages a DEBUG
  # helios_umd12.dll into the package, this script only ever refreshed
  # helios_umd.dll, and so the DriverStore received a debug D3D12 UMD while the
  # release build sat unused. A default is an answer nobody had to think about;
  # for the binaries that actually reach the DriverStore, the caller states them
  # or the script refuses.
  #
  # ⚠ Deliberately NOT `[Parameter(Mandatory)]`: a missing mandatory parameter
  # makes PowerShell prompt on stdin, and this script is normally invoked
  # non-interactively over SSH, where that is an indefinite hang instead of an
  # error. The explicit emptiness check below fails fast and says which one.
  [string]$PackageDir = "",
  # Sync-HeliosPackageUmd overwrites whatever copy-umd-to-package staged with
  # THIS file before the catalog is created and signed, so this parameter — not
  # the packaging task — decides which binary reaches the DriverStore. The PSC
  # stage measures present-gate timing, and the debug profile is opt-level 1
  # with no LTO, so a debug build silently makes every cadence and wake-latency
  # number a measurement of the wrong binary.
  [string]$UmdDll = "",
  # The D3D12 UMD, synced into the package on exactly the same terms as $UmdDll
  # and BEFORE the catalog is generated — the DriverStore copy is catalog-signed,
  # so it cannot be corrected by overwriting the file afterwards.
  [string]$Umd12Dll = "",
  # Required for packages that declare UserModeDriverNameWoW. Optional only to
  # allow deliberate rollback to an older package with native UMDs alone.
  [string]$Umd32Dll = "",
  [string]$Umd12_32Dll = "",
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
. (Join-Path $PackagingRoot "Helios-PackageCommon.ps1")

# No implicit fallback: name the artifacts or get a refusal, never a default.
foreach ($req in @(@("PackageDir", $PackageDir), @("UmdDll", $UmdDll), @("Umd12Dll", $Umd12Dll))) {
  if ([string]::IsNullOrWhiteSpace($req[1])) {
    throw "-$($req[0]) is required and has no default. Name the artifact explicitly; see the param block for why."
  }
}

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
  if ($prop -and $prop.PSObject.Properties["Data"] -and $prop.Data) {
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
  # Local builds are stamped from the developer machine's date, so keep the
  # historical /uselocaltime attempt first. CI artifacts are stamped on UTC
  # runners, however, and can be one calendar day ahead of a guest west of UTC.
  # Inf2Cat's default UTC comparison is the correct fallback for those packages.
  $timeModes = @(
    @{ Label = "local time"; UseLocalTime = $true },
    @{ Label = "UTC"; UseLocalTime = $false }
  )
  foreach ($timeMode in $timeModes) {
    foreach ($os in $osTargets) {
      Write-Host "Regenerating catalog with Inf2Cat /os:$os using $($timeMode.Label)"
      $arguments = @("/driver:$PackageDir", "/os:$os")
      if ($timeMode.UseLocalTime) { $arguments += "/uselocaltime" }
      & $inf2cat @arguments
      if ($LASTEXITCODE -eq 0 -and (Test-Path -LiteralPath $CatPath -PathType Leaf)) {
        return
      }
      Write-Warning "Inf2Cat failed for /os:$os using $($timeMode.Label) (exit $LASTEXITCODE)."
    }
  }

  throw "Inf2Cat failed to regenerate $CatPath"
}

function Ensure-MachineCodeSigningCert {
  $repo = Split-Path -Parent $PSScriptRoot
  . (Join-Path $repo "metadata\Read-HeliosMetadata.ps1")
  $metadata = Read-HeliosMetadata $repo
  $subject = "CN=$($metadata.HELIOS_PUBLISHER) $($metadata.HELIOS_PRODUCT) Development Test Signing"
  $now = Get-Date
  $cert = Get-ChildItem Cert:\LocalMachine\My -ErrorAction SilentlyContinue |
    Where-Object { $_.Subject -eq $subject -and $_.HasPrivateKey -and $_.NotBefore -le $now -and $_.NotAfter -gt $now.AddDays(30) } |
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

  $tmp = Join-Path $env:TEMP ("helios-dev-test-" + [Guid]::NewGuid().ToString("N") + ".cer")
  Export-Certificate -Cert $cert -FilePath $tmp | Out-Null
  foreach ($store in @("Root", "TrustedPublisher")) {
    try {
      Import-Certificate -FilePath $tmp -CertStoreLocation "Cert:\LocalMachine\$store" -ErrorAction Stop | Out-Null
    } catch {
      # Some elevated OpenSSH sessions can create LocalMachine\My keys but the
      # PKI cmdlet still returns E_ACCESSDENIED for trust-store publication.
      # certutil uses the same machine stores and succeeds in that token.
      Write-Warning "Import-Certificate failed for LocalMachine\$store; retrying with certutil. $($_.Exception.Message)"
      & certutil.exe -f -addstore $store $tmp | Out-Host
      if ($LASTEXITCODE -ne 0) { throw "certutil failed to import $tmp into LocalMachine\$store" }
    }
  }
  Remove-Item $tmp -Force -ErrorAction SilentlyContinue
  return $cert
}

function Sign-FileWithMachineCert([string]$Path) {
  if ($SkipSign) { return }
  $cert = Ensure-MachineCodeSigningCert
  $signtool = Find-Signtool
  if (-not $signtool) {
    throw "signtool.exe not found; refusing to publish an unsigned or stale package for $Path."
  }
  & $signtool sign /v /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 /sm /s My /sha1 $cert.Thumbprint $Path
  if ($LASTEXITCODE -ne 0) {
    Write-Warning "Timestamped signtool failed for $Path; retrying without timestamp."
    & $signtool sign /v /fd SHA256 /sm /s My /sha1 $cert.Thumbprint $Path
  }
  if ($LASTEXITCODE -ne 0) { throw "signtool failed for $Path" }
}

function Sign-HeliosPackage([string]$SysPath, [string]$CatPath, [string[]]$UmdPaths) {
  # A signature changes the image hash: finalize every copied image BEFORE
  # generating its catalog. Signing SYS after Inf2Cat invalidates the catalog.
  Sign-FileWithMachineCert $SysPath
  foreach ($path in $UmdPaths) { Sign-FileWithMachineCert $path }
  New-HeliosCatalog (Split-Path -Parent $SysPath) $CatPath
  Sign-FileWithMachineCert $CatPath
}

function Prepare-HeliosDeploy {
  Stop-LookingGlassHostService
  $cleared = Clear-HeliosPendingRenames
  if ($cleared -gt 0) { Write-Host "Removed $cleared stale Helios pending rename operation(s)." }
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

function Restore-HeliosPreviousPackage([string]$InstanceId, [string]$PreviousInf, [string]$CurrentInf, $RegistrationSnapshot) {
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
  # An older INF has no deletion directive for a WoW64 value introduced by a
  # newer package. Restore the previous registration explicitly on rollback.
  if ($RegistrationSnapshot -and (Get-HeliosActiveInfName $InstanceId) -eq $PreviousInf) {
    $classKey = Get-HeliosClassKey $InstanceId
    foreach ($name in $RegistrationSnapshot.Keys) {
      Restore-HeliosRegistrySnapshot $classKey $name $RegistrationSnapshot[$name]
    }
  }
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

# Archive the SYMBOLS of the image this deploy is about to install.
#
# Defect 0ac: a 0xD1 bugcheck on 22.22.212.0 had to be triaged from a raw stack,
# because the .pdb that matched the shipped .sys no longer existed anywhere -- the
# INF does not name it, so the DriverStore never receives it, and the next build
# overwrites it in the target directory. One stamped copy per deploy is the whole
# fix: a dump from KMD version N resolves against <stamp>\staged\.
#
# They go in a `staged` SUBDIRECTORY on purpose. The stamp directory's own
# .sys/.cat/.inf are the files this deploy REPLACED (the rollback set); these are
# the ones it INSTALLED. Mixing an outgoing binary with an incoming pdb in one
# flat directory reads like a matched pair and is not one.
function Save-HeliosKmdSymbols([string]$BackupDir, [string]$PackageDir) {
  $dir = Join-Path $BackupDir "staged"
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  $saved = @()
  foreach ($name in @("helios_kmd_render.sys", "helios_kmd_render.pdb", "helios_kmd_render.map")) {
    $src = Join-Path $PackageDir $name
    if (Test-Path -LiteralPath $src -PathType Leaf) {
      Copy-Item -LiteralPath $src -Destination (Join-Path $dir $name) -Force
      $saved += $name
    }
  }
  if ($saved -notcontains "helios_kmd_render.pdb") {
    # Loud, not fatal: refusing the deploy over a missing symbol file would be a
    # worse failure than deploying without it. But a bugcheck on this image will
    # not be resolvable, so say so at deploy time rather than at triage time.
    Write-Warning "No helios_kmd_render.pdb in $PackageDir -- this image will ship WITHOUT archived symbols (defect 0ac)."
  }
  Write-Host "Archived staged KMD symbols to $dir ($($saved -join ', '))"
}

function Backup-HeliosActiveFiles([string]$Store, [string[]]$Names, $RegistrationSnapshot) {
  $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
  $dir = Join-Path "C:\ProgramData\HeliosDeployBackups" $stamp
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  foreach ($name in $Names) {
    $src = Join-Path $Store $name
    if (Test-Path -LiteralPath $src -PathType Leaf) {
      Copy-Item -LiteralPath $src -Destination (Join-Path $dir $name) -Force
    }
  }
  if ($RegistrationSnapshot) {
    $RegistrationSnapshot | ConvertTo-Json -Depth 8 |
      Set-Content -LiteralPath (Join-Path $dir "display-registration.json") -Encoding UTF8
  }
  Write-Host "Backed up active DriverStore files to $dir"
  Save-HeliosKmdSymbols $dir $PackageDir
  return $dir
}

if (-not (Test-Path -LiteralPath $PackageDir -PathType Container)) { throw "PackageDir not found: $PackageDir" }
$inf = Join-Path $PackageDir "helios_kmd_render.inf"
$sys = Join-Path $PackageDir "helios_kmd_render.sys"
$cat = Join-Path $PackageDir "helios_kmd_render.cat"
foreach ($path in @($inf, $sys, $cat)) {
  if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing package file $path" }
}
$infText = (Get-Content -LiteralPath $inf | Where-Object { $_ -notmatch '^\s*;' }) -join "`n"
$packageHasWow64 = $infText -match '(?im)^\s*HKR\s*,[^\r\n;]*,\s*"?UserModeDriverNameWoW"?\s*,'
$packageUmds = @(
  @{ Name = "helios_umd.dll"; Source = $UmdDll; Architecture = "x64" },
  @{ Name = "helios_umd12.dll"; Source = $Umd12Dll; Architecture = "x64" }
)
if ($packageHasWow64) {
  foreach ($req in @(@("Umd32Dll", $Umd32Dll), @("Umd12_32Dll", $Umd12_32Dll))) {
    if ([string]::IsNullOrWhiteSpace($req[1])) {
      throw "-$($req[0]) is required: $inf declares UserModeDriverNameWoW. Name the x86 artifact explicitly."
    }
  }
  $packageUmds += @(
    @{ Name = "helios_umd32.dll"; Source = $Umd32Dll; Architecture = "x86" },
    @{ Name = "helios_umd12_32.dll"; Source = $Umd12_32Dll; Architecture = "x86" }
  )
} elseif ($Umd32Dll -or $Umd12_32Dll) {
  throw "x86 UMD paths were supplied but $inf has no UserModeDriverNameWoW registration."
}
if ($StageOnly -and $BinaryOnly) { throw "-StageOnly and -BinaryOnly cannot be combined." }
$umdNames = @($packageUmds | ForEach-Object { $_.Name })
$umdPaths = @($umdNames | ForEach-Object { Join-Path $PackageDir $_ })
foreach ($entry in $packageUmds) {
  if (-not $BinaryOnly -or $IncludeUmd) {
    Assert-HeliosPeArchitecture $entry.Source $entry.Architecture
  }
}
Assert-HeliosPeArchitecture $sys x64
Assert-HeliosAdmin
if (-not [Environment]::Is64BitProcess) { throw "Run this installer with native 64-bit PowerShell." }

if ($StageOnly) {
  Write-HeliosPlan "Helios KMD stage-only install" @{
    PackageDir = $PackageDir
    UmdSource = $UmdDll
    Umd12Source = $Umd12Dll
    Umd32Source = $Umd32Dll
    Umd12_32Source = $Umd12_32Dll
    UmdProfile = (Split-Path -Leaf (Split-Path -Parent $UmdDll))
    Umd12Profile = (Split-Path -Leaf (Split-Path -Parent $Umd12Dll))
    StageOnly = [bool]$StageOnly
  }
  if ($PlanOnly) { return }
  Prepare-HeliosDeploy
  foreach ($entry in $packageUmds) {
    Sync-HeliosPackageUmd $entry.Source (Join-Path $PackageDir $entry.Name)
  }
  Sign-HeliosPackage $sys $cat $umdPaths
  Publish-HeliosPackageOnly $inf
  Write-Host "Helios KMD package staged. Reboot/start with the Helios PCI device present, then bind with devcon update if PnP does not pick the highest DriverVer automatically."
  return
}

$id = Get-HeliosInstanceId $InstanceId
$hwid = Get-HeliosHardwareId $id
$activeInf = Get-HeliosActiveInfName $id
$store = Get-HeliosActiveStoreDir $id $activeInf
$activeClassKey = Get-HeliosClassKey $id
$registrationSnapshot = [ordered]@{}
foreach ($name in @("UserModeDriverName", "UserModeDriverNameWoW", "InstalledDisplayDrivers")) {
  $registrationSnapshot[$name] = Get-HeliosRegistrySnapshot $activeClassKey $name
}
if ($BinaryOnly) {
  $activeInfPath = Join-Path $store "helios_kmd_render.inf"
  $activeInfText = (Get-Content -LiteralPath $activeInfPath | Where-Object { $_ -notmatch '^\s*;' }) -join "`n"
  $activeHasWow64 = $activeInfText -match '(?im)^\s*HKR\s*,[^\r\n;]*,\s*"?UserModeDriverNameWoW"?\s*,'
  if ($activeHasWow64 -ne $packageHasWow64) {
    throw "-BinaryOnly cannot add or remove WoW64 registration. Install the full package with PnP."
  }
}
$copyNames = if ($BinaryOnly) { @("helios_kmd_render.sys", "helios_kmd_render.cat") } else { @("helios_kmd_render.inf", "helios_kmd_render.sys", "helios_kmd_render.cat") + $umdNames }
if ($IncludeUmd) { $copyNames += $umdNames }
if ($BinaryOnly -and -not $IncludeUmd) {
  # This mode copies a new catalog but leaves active UMDs alone. Its catalog
  # must therefore cover those exact bytes, not different staged UMDs.
  foreach ($entry in $packageUmds) {
    $staged = Join-Path $PackageDir $entry.Name
    $active = Join-Path $store $entry.Name
    if ((Get-HeliosFileHash $staged) -ne (Get-HeliosFileHash $active)) {
      throw "-BinaryOnly cannot replace the catalog while $($entry.Name) differs. Include all UMDs or stage matching active bytes."
    }
  }
}
$copyNames = $copyNames | Select-Object -Unique

Write-HeliosPlan "Helios KMD install" @{
  PackageDir = $PackageDir
  Instance = $id
  HardwareId = $hwid
  ActiveInf = $activeInf
  DriverStore = $store
  UmdSource = $UmdDll
  Umd12Source = $Umd12Dll
  Umd32Source = $Umd32Dll
  Umd12_32Source = $Umd12_32Dll
  UmdProfile = (Split-Path -Leaf (Split-Path -Parent $UmdDll))
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

Prepare-HeliosDeploy
foreach ($entry in $packageUmds) {
  Sync-HeliosPackageUmd $entry.Source (Join-Path $PackageDir $entry.Name)
}
$signUmdPaths = if (-not $BinaryOnly -or $IncludeUmd) { $umdPaths } else { @() }
Sign-HeliosPackage $sys $cat $signUmdPaths

if (-not $BinaryOnly) {
  $backup = Backup-HeliosActiveFiles $store $copyNames $registrationSnapshot
  $oldSvc = Get-ItemProperty -LiteralPath "HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render" -ErrorAction SilentlyContinue
  $oldImagePath = if ($oldSvc -and $oldSvc.PSObject.Properties["ImagePath"]) { $oldSvc.ImagePath } else { $null }
  $update = Invoke-HeliosPackageUpdate $inf $id
  $newActiveInf = Get-HeliosActiveInfName $id
  $newStore = Get-HeliosActiveStoreDir $id $newActiveInf
  $newSvc = Get-ItemProperty -LiteralPath "HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render" -ErrorAction SilentlyContinue
  $newImagePath = if ($newSvc -and $newSvc.PSObject.Properties["ImagePath"]) { $newSvc.ImagePath } else { $null }
  $activeSys = Join-Path $newStore "helios_kmd_render.sys"
  $packageSysHash = Get-HeliosFileHash $sys
  $activeSysHash = Get-HeliosFileHash $activeSys
  if ($activeSysHash -ne $packageSysHash) {
    throw "devcon/pnputil did not bind the staged KMD image. activeInf=$newActiveInf activeSys=$activeSys activeHash=$activeSysHash packageHash=$packageSysHash. Bump DriverVer or remove the stale active package with pnputil; do not continue with a stale DriverStore image."
  }
  Write-Host "Active KMD image verified: $activeSys hash $activeSysHash"
  foreach ($entry in $packageUmds) {
    $staged = Join-Path $PackageDir $entry.Name
    $active = Join-Path $newStore $entry.Name
    if ((Get-HeliosFileHash $staged) -ne (Get-HeliosFileHash $active)) {
      throw "devcon/pnputil did not bind the staged $($entry.Name): $active"
    }
    Assert-HeliosPeArchitecture $active $entry.Architecture
    Write-Host "Active UMD image verified: $active"
  }
  if (-not $packageHasWow64) {
    # Deliberate downgrade to an older native-only package. PnP does not clear
    # a software-key value just because the older INF never mentioned it.
    $newClassKey = Get-HeliosClassKey $id
    Remove-ItemProperty -LiteralPath $newClassKey -Name "UserModeDriverNameWoW" -ErrorAction SilentlyContinue
    $installedNames = @((Get-Item -LiteralPath $newClassKey).GetValue("InstalledDisplayDrivers", @()) |
      Where-Object { $_ -notin @("helios_umd32", "helios_umd12_32") })
    if ($installedNames.Count -gt 0) {
      New-ItemProperty -LiteralPath $newClassKey -Name "InstalledDisplayDrivers" -PropertyType MultiString -Value $installedNames -Force | Out-Null
    }
  }
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
    Restore-HeliosPreviousPackage $id $activeInf $newInf $registrationSnapshot
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
}
foreach ($entry in $packageUmds) { $sources[$entry.Name] = Join-Path $PackageDir $entry.Name }
$backup = Backup-HeliosActiveFiles $store $copyNames $registrationSnapshot

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
