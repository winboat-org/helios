# Runs the real hotplug entry point with a replacement deployment-common module.
# No registry, service, process, device, ACL or destination file is modified.
# Run on Windows PowerShell 5.1 or pwsh: & tools/tests/test-hotplug-helios-umd.ps1
param([string]$PackagingRoot = $env:WB_PACKAGING_ROOT, [string]$RepoRoot = (Split-Path (Split-Path $PSScriptRoot -Parent) -Parent))
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('helios-hotplug-test-' + [guid]::NewGuid())
$oldWindir = $env:windir
$cases = 0
$fixtureTools = Join-Path $fixture 'tools'
$fixturePackage = Join-Path $fixture 'packaging/windows'
New-Item -ItemType Directory -Path $fixtureTools, $fixturePackage -Force | Out-Null
Copy-Item -LiteralPath (Join-Path $RepoRoot 'tools/hotplug-helios-umd.ps1') -Destination $fixtureTools
Copy-Item -LiteralPath (Join-Path $PackagingRoot 'Helios-PackageCommon.ps1') -Destination $fixturePackage
@'
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
function Assert-HeliosAdmin {}
function Stop-LookingGlassHostService { $global:HeliosHotplugTest.Mutations.Add('service') }
function Clear-HeliosPendingRenames { $global:HeliosHotplugTest.Mutations.Add('renames'); return 0 }
function Get-HeliosInstanceId { return 'PCI\TEST' }
function Get-HeliosClassKey { return 'HKLM:\TEST' }
function Get-HeliosActiveInfName { return 'oem-test.inf' }
function Get-HeliosActiveStoreDir { return $global:HeliosHotplugTest.Store }
function Get-HeliosPnpState { return [pscustomobject]@{ Status = 'OK' } }
function Write-HeliosPlan {}
function Write-Host {}
function Write-Warning([string]$Message) { $global:HeliosHotplugTest.Warnings.Add($Message) }
function Start-Sleep {}
function Test-Path {
  param([string]$LiteralPath, [string]$PathType)
  return $global:HeliosHotplugTest.Hashes.ContainsKey($LiteralPath)
}
function Get-HeliosFileHash([string]$Path) {
  if (-not $global:HeliosHotplugTest.Hashes.ContainsKey($Path)) { throw "Unknown file: $Path" }
  return $global:HeliosHotplugTest.Hashes[$Path]
}
function Get-Item {
  param([string]$LiteralPath, [switch]$Force)
  if ($LiteralPath -ne 'HKLM:\TEST') {
    if ($LiteralPath -in $global:HeliosHotplugTest.ReparsePaths) {
      return [pscustomobject]@{ Attributes = [IO.FileAttributes]::ReparsePoint }
    }
    return Microsoft.PowerShell.Management\Get-Item -LiteralPath $LiteralPath -Force -ErrorAction Stop
  }
  $key = [pscustomobject]@{}
  $key | Add-Member ScriptMethod GetValueNames { return @($global:HeliosHotplugTest.Values.Keys) }
  $key | Add-Member ScriptMethod GetValueKind { param($Name); return $global:HeliosHotplugTest.Kinds[$Name] }
  $key | Add-Member ScriptMethod GetValue { param($Name); return $global:HeliosHotplugTest.Values[$Name] }
  return $key
}
function New-Item {
  param([string]$ItemType, [switch]$Force, [string]$Path)
  $global:HeliosHotplugTest.Mutations.Add("directory:$Path")
}
function Grant-HeliosReadExecute([string]$Path) { $global:HeliosHotplugTest.Mutations.Add("acl:$Path") }
function Copy-HeliosFileVerified {
  param([string]$Source, [string]$Destination, [int]$Retries, [int]$RetryDelayMs, [switch]$DisplaceInUse)
  $s = $global:HeliosHotplugTest
  $s.Mutations.Add("copy:$Destination")
  $s.Copies.Add($Destination)
  if ($s.Copies.Count -eq $s.FailCopy) { throw 'injected copy failure' }
  if ($DisplaceInUse) { throw 'unexpected file displacement' }
  $s.Hashes[$Destination] = $s.Hashes[$Source]
  return [pscustomobject]@{ Destination = $Destination }
}
function New-ItemProperty {
  param([string]$LiteralPath, [string]$Name, [string]$PropertyType, [string[]]$Value, [switch]$Force)
  $s = $global:HeliosHotplugTest
  if ($LiteralPath -ne 'HKLM:\TEST') { throw "Unexpected write path: $LiteralPath" }
  if ($Name -eq 'UserModeDriverNameWoW') { throw 'WoW64 must not be written' }
  $s.Mutations.Add("registry:$Name")
  $s.Values[$Name] = $Value.Clone()
  $s.Kinds[$Name] = $PropertyType
  if ($Name -eq 'InstalledDisplayDrivers' -and $s.CorruptReadback) {
    $s.Values[$s.CorruptReadback][0] = 'corrupt.dll'
  }
}
function Invoke-HeliosPnpUtil {
  param([string[]]$Arguments, [int]$TimeoutSeconds)
  $s = $global:HeliosHotplugTest
  $s.Mutations.Add("pnp:$($Arguments[0])")
  $s.Pnp.Add($Arguments[0])
  if ($Arguments[0] -eq '/add-driver') {
    $s.Hashes[(Join-Path $s.Store 'helios_umd.dll')] = $s.Hashes[$s.Source]
  }
}
# A reintroduction of either old DriverStore command fails even on Windows.
function takeown.exe { throw 'forbidden takeown command' }
function icacls.exe { throw 'forbidden icacls command' }
'@ | Set-Content -LiteralPath (Join-Path $fixtureTools 'helios-deploy-common.ps1') -Encoding UTF8

# Only the PE header is needed by the real architecture validator. These
# fixture files are never executable or passed to a real deployment command.
function Write-PeFixture([string]$Path, [uint16]$Machine = 0x8664) {
  $bytes = [byte[]]::new(128)
  [BitConverter]::GetBytes([uint16]0x5A4D).CopyTo($bytes, 0)
  [BitConverter]::GetBytes([uint32]64).CopyTo($bytes, 0x3C)
  [BitConverter]::GetBytes([uint32]0x4550).CopyTo($bytes, 64)
  [BitConverter]::GetBytes($Machine).CopyTo($bytes, 68)
  [IO.File]::WriteAllBytes($Path, $bytes)
}

function Assert([bool]$Condition, [string]$Message) {
  if (-not $Condition) { throw $Message }
}
function Assert-Array($Actual, $Expected, [string]$Message) {
  Assert (@($Actual).Count -eq @($Expected).Count) "$Message (length)"
  for ($i = 0; $i -lt @($Expected).Count; $i++) {
    Assert ($Actual[$i] -ceq $Expected[$i]) "$Message (entry $i)"
  }
}
function New-Fixture {
  $root = Join-Path $fixture 'mock'
  New-Item -ItemType Directory -Path $root -Force | Out-Null
  $store = Join-Path $root 'DriverStore'
  $source = Join-Path $root 'source11.dll'
  $source12 = Join-Path $root 'source12.dll'
  Write-PeFixture $source
  Write-PeFixture $source12
  $dll = Join-Path $store 'helios_umd.dll'
  $dll12 = Join-Path $store 'helios_umd12.dll'
  $wow = Join-Path $store 'helios_umd32.dll'
  $wow12 = Join-Path $store 'helios_umd12_32.dll'
  $global:HeliosHotplugTest = @{
    Store = $store; Source = $source; Source12 = $source12
    Values = @{
      UserModeDriverName = @($dll, $dll, $dll, $dll12)
      UserModeDriverNameWoW = @($wow, $wow, $wow, $wow12)
      InstalledDisplayDrivers = @('helios_umd', 'helios_umd12', 'helios_umd32', 'helios_umd12_32')
    }
    Kinds = @{ UserModeDriverName = 'MultiString'; UserModeDriverNameWoW = 'MultiString'; InstalledDisplayDrivers = 'MultiString' }
    Hashes = @{}; FailCopy = 0; CorruptReadback = ''; ReparsePaths = @()
    Mutations = [Collections.Generic.List[string]]::new()
    Copies = [Collections.Generic.List[string]]::new()
    Pnp = [Collections.Generic.List[string]]::new()
    Warnings = [Collections.Generic.List[string]]::new()
  }
  $s = $global:HeliosHotplugTest
  $s.Hashes[$source] = 'A' * 64
  $s.Hashes[$source12] = 'B' * 64
  $s.Hashes[$dll] = 'C' * 64
  $s.Hashes[(Join-Path $root 'helios_kmd_render.inf')] = 'D' * 64
  $env:windir = Join-Path $root 'Windows'
  return @{ UmdDll = $source; ProgramDataDir = (Join-Path $root 'ProgramData'); PackageDir = $root; NoProbe = $true }
}
function Invoke-Fixture($Parameters, [string]$ExpectedFailure = '') {
  $caught = ''
  try { & (Join-Path $fixtureTools 'hotplug-helios-umd.ps1') -PackagingRoot $fixturePackage @Parameters | Out-Null }
  catch { $caught = $_.Exception.Message }
  if ($ExpectedFailure) {
    Assert ($caught -like "*$ExpectedFailure*") "Expected '$ExpectedFailure', got '$caught'"
  } else { Assert (-not $caught) "Unexpected error: $caught" }
  $script:cases++
}

try {
  foreach ($artifact in @('UmdDll', 'Umd12Dll')) {
    $p = New-Fixture
    $s = $global:HeliosHotplugTest
    $p.Umd12Dll = $s.Source12
    Write-PeFixture $p[$artifact] 0x14C
    Invoke-Fixture $p 'Expected x64 PE image'
    Assert ($s.Mutations.Count -eq 0) "x86 $artifact mutated native deployment"
  }

  foreach ($with12 in @($false, $true)) {
    $p = New-Fixture
    $s = $global:HeliosHotplugTest
    $old12 = $s.Values.UserModeDriverName[3]
    $oldWow = $s.Values.UserModeDriverNameWoW.Clone()
    if ($with12) { $p.Umd12Dll = $s.Source12 }
    Invoke-Fixture $p
    $new11 = Join-Path $p.ProgramDataDir 'helios_umd_aaaaaaaaaaaaaaaa.dll'
    $new12 = if ($with12) { Join-Path $p.ProgramDataDir 'helios_umd12_bbbbbbbbbbbbbbbb.dll' } else { $old12 }
    Assert-Array $s.Values.UserModeDriverName @($new11, $new11, $new11, $new12) 'Native paths'
    Assert-Array $s.Values.UserModeDriverNameWoW $oldWow 'WoW64 preservation'
    $name12 = if ($with12) { 'helios_umd12_bbbbbbbbbbbbbbbb' } else { 'helios_umd12' }
    Assert-Array $s.Values.InstalledDisplayDrivers @('helios_umd_aaaaaaaaaaaaaaaa', $name12, 'helios_umd32', 'helios_umd12_32') 'Flat inventory'
    Assert ($s.Copies.Count -eq (1 + [int]$with12)) 'Only selected native files copied'
    Assert (@($s.Copies | Where-Object { -not $_.StartsWith($p.ProgramDataDir) }).Count -eq 0) 'Copy escaped ProgramData'
    Assert ($s.Hashes[(Join-Path $s.Store 'helios_umd.dll')] -eq ('C' * 64)) 'DriverStore hash changed'
    Assert ($s.Pnp.Count -eq 0) 'Default unexpectedly restarted a device'
    Assert (($s.Warnings -join ' ') -match 'loaded modules are not') 'Loaded-module limitation missing'
  }

  $p = New-Fixture
  $s = $global:HeliosHotplugTest
  $s.Values.Remove('UserModeDriverNameWoW')
  $s.Kinds.Remove('UserModeDriverNameWoW')
  $s.Values.UserModeDriverName[3] = 'legacy_umd.dll'
  Invoke-Fixture $p
  Assert ($s.Values.UserModeDriverName[3] -ceq 'legacy_umd.dll') 'Legacy slot 3 changed'
  Assert (-not $s.Values.ContainsKey('UserModeDriverNameWoW')) 'Absent WoW64 synthesized'
  Assert-Array $s.Values.InstalledDisplayDrivers @('helios_umd_aaaaaaaaaaaaaaaa', 'legacy_umd') 'Legacy inventory'

  $p = New-Fixture
  $s = $global:HeliosHotplugTest
  $s.Values.UserModeDriverName[3] = 'HELIOS_UMD_AAAAAAAAAAAAAAAA.dll'
  $s.Values.UserModeDriverNameWoW[1] = $s.Values.UserModeDriverNameWoW[1].ToUpperInvariant()
  Invoke-Fixture $p
  Assert-Array $s.Values.InstalledDisplayDrivers @('helios_umd_aaaaaaaaaaaaaaaa', 'helios_umd32', 'helios_umd12_32') 'Case-insensitive inventory'

  foreach ($mode in @('ProgramData', 'DriverStore', 'PackageUpgrade')) {
    $p = New-Fixture
    $p.Mode = $mode; $p.PlanOnly = $true; $p.ForceDriverStoreEdit = $true; $p.RestartDevice = $true
    Invoke-Fixture $p
    Assert ($global:HeliosHotplugTest.Mutations.Count -eq 0) "PlanOnly mutated $mode"
  }

  foreach ($defect in @('native-missing', 'native-short', 'native-long', 'native-kind', 'native-blank', 'native-extension', 'wow-empty', 'wow-kind', 'wow-null')) {
    $p = New-Fixture
    $s = $global:HeliosHotplugTest
    switch ($defect) {
      'native-missing' { $s.Values.Remove('UserModeDriverName') }
      'native-short' { $s.Values.UserModeDriverName = @('a.dll') * 3 }
      'native-long' { $s.Values.UserModeDriverName = @('a.dll') * 6 }
      'native-kind' { $s.Kinds.UserModeDriverName = 'String' }
      'native-blank' { $s.Values.UserModeDriverName[3] = ' ' }
      'native-extension' { $s.Values.UserModeDriverName[3] = 'wrong.exe' }
      'wow-empty' { $s.Values.UserModeDriverNameWoW = @() }
      'wow-kind' { $s.Kinds.UserModeDriverNameWoW = 'String' }
      'wow-null' { $s.Values.UserModeDriverNameWoW[2] = $null }
    }
    Invoke-Fixture $p 'UserModeDriverName'
    Assert ($s.Mutations.Count -eq 0) "Malformed $defect mutated state"
  }

  $p = New-Fixture; $p.ProgramDataDir = Join-Path $global:HeliosHotplugTest.Store 'nested'
  Invoke-Fixture $p 'outside DriverStore'
  Assert ($global:HeliosHotplugTest.Mutations.Count -eq 0) 'DriverStore destination mutated state'

  foreach ($alias in @('\\?\C:\Windows\System32\DriverStore', '\\.\C:\Windows\System32\DriverStore', '\\localhost\C$\Windows\System32\DriverStore')) {
    $p = New-Fixture; $p.ProgramDataDir = $alias
    Invoke-Fixture $p 'ordinary local drive path'
    Assert ($global:HeliosHotplugTest.Mutations.Count -eq 0) 'Alternate namespace mutated state'
  }

  foreach ($linkAtLeaf in @($false, $true)) {
    $p = New-Fixture
    $s = $global:HeliosHotplugTest
    $s.ReparsePaths = @(if ($linkAtLeaf) { $p.ProgramDataDir } else { Split-Path -Parent $p.ProgramDataDir })
    Invoke-Fixture $p 'reparse point'
    Assert ($s.Mutations.Count -eq 0) 'Junction destination mutated state'
  }

  # Verify the actual provider behavior too, including a dangling junction/link.
  # Both the target and link live under this test's temporary directory.
  $linkTarget = Join-Path $fixture 'link-target'
  $linkPath = Join-Path $fixture 'programdata-link'
  New-Item -ItemType Directory -Path $linkTarget | Out-Null
  $linkType = if ($env:OS -eq 'Windows_NT') { 'Junction' } else { 'SymbolicLink' }
  New-Item -ItemType $linkType -Path $linkPath -Target $linkTarget | Out-Null
  $p = New-Fixture; $p.ProgramDataDir = Join-Path $linkPath 'new-child'
  Invoke-Fixture $p 'reparse point'
  Assert ($global:HeliosHotplugTest.Mutations.Count -eq 0) 'Real linked ancestor mutated state'
  Remove-Item -LiteralPath $linkTarget -Force
  $p = New-Fixture; $p.ProgramDataDir = $linkPath
  Invoke-Fixture $p 'reparse point'
  Assert ($global:HeliosHotplugTest.Mutations.Count -eq 0) 'Dangling link mutated state'

  $p = New-Fixture; $p.Mode = 'DriverStore'
  Invoke-Fixture $p 'ForceDriverStoreEdit'
  Assert ($global:HeliosHotplugTest.Mutations.Count -eq 0) 'Unguarded emergency mode mutated state'
  $p.ForceDriverStoreEdit = $true
  Invoke-Fixture $p
  Assert-Array $global:HeliosHotplugTest.Copies @((Join-Path $global:HeliosHotplugTest.Store 'helios_umd.dll')) 'Explicit emergency destination'
  Assert (@($global:HeliosHotplugTest.Mutations | Where-Object { $_ -like 'registry:*' }).Count -eq 0) 'Emergency mode rewrote registration'

  $p = New-Fixture; $p.Mode = 'PackageUpgrade'
  Invoke-Fixture $p
  Assert-Array $global:HeliosHotplugTest.Pnp @('/add-driver') 'Package mode activation'
  Assert ($global:HeliosHotplugTest.Copies.Count -eq 0) 'Package mode raw-copied a file'

  $p = New-Fixture; $p.RestartDevice = $true; $p.Umd12Dll = $global:HeliosHotplugTest.Source12
  $s = $global:HeliosHotplugTest; $s.FailCopy = 2
  $oldNative = $s.Values.UserModeDriverName.Clone()
  Invoke-Fixture $p 'injected copy failure'
  Assert-Array $s.Pnp @('/disable-device', '/enable-device') 'Failure must re-enable adapter'
  Assert-Array $s.Values.UserModeDriverName $oldNative 'Copy failure changed registration'

  foreach ($value in @('UserModeDriverName', 'UserModeDriverNameWoW', 'InstalledDisplayDrivers')) {
    $p = New-Fixture; $global:HeliosHotplugTest.CorruptReadback = $value
    Invoke-Fixture $p 'expected'
  }
  Write-Host "PASS: $cases hotplug deployment cases; no real deployment performed."
} finally {
  $env:windir = $oldWindir
  Remove-Variable HeliosHotplugTest -Scope Global -ErrorAction SilentlyContinue
  Remove-Item -LiteralPath $fixture -Recurse -Force
}
