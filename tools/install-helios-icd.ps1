param(
  [string]$DllPath = "C:\Users\Rupansh\helios-mesa-build\src\virtio\vulkan\vulkan_virtio.dll",
  [string]$InstallDir = "C:\ProgramData\HeliosVulkan",
  [string]$ApiVersion = "1.4.352",
  [ValidateSet("Machine", "User")]
  [string]$Scope = "Machine",
  [switch]$NoRegistryCleanup,
  [switch]$NoSmoke,
  [switch]$PruneOld,
  [switch]$PlanOnly
)

. "$PSScriptRoot\helios-deploy-common.ps1"

function Test-IsAdmin {
  $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
  $principal = [Security.Principal.WindowsPrincipal]::new($identity)
  return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Remove-StaleHeliosIcdValues([string]$RegPath) {
  if (-not (Test-Path -LiteralPath $RegPath)) { return }
  $item = Get-Item -LiteralPath $RegPath
  foreach ($name in $item.GetValueNames()) {
    $lower = $name.ToLowerInvariant()
    $isHelios = $lower.Contains("heliosvulkan") -or $lower.Contains("virtio_devenv_icd") -or $lower.Contains("vulkan_virtio") -or $lower.Contains("helios-mesa")
    if ($isHelios) {
      Remove-ItemProperty -LiteralPath $RegPath -Name $name -ErrorAction SilentlyContinue
      Write-Host "Removed stale Vulkan ICD registry value: $name"
    }
  }
}

if ($Scope -eq "Machine" -and -not (Test-IsAdmin)) {
  throw "Machine-scope install writes HKLM and requires elevated PowerShell. Re-run as Administrator, or pass -Scope User."
}
if (-not (Test-Path -LiteralPath $DllPath -PathType Leaf)) { throw "ICD DLL not found: $DllPath" }

$sourceDll = (Resolve-Path -LiteralPath $DllPath).Path
$installDirFull = [IO.Path]::GetFullPath($InstallDir)
$sourceHash = Get-HeliosFileHash $sourceDll
$hashPrefix = $sourceHash.Substring(0, 12).ToLowerInvariant()
$destDll = Join-Path $installDirFull "vulkan_virtio-$hashPrefix.dll"
$manifest = Join-Path $installDirFull "virtio_devenv_icd.x86_64.json"

Write-HeliosPlan "Helios Vulkan ICD install" @{
  Source = $sourceDll
  SourceHash = $sourceHash
  Destination = $destDll
  Manifest = $manifest
  Scope = $Scope
}
if ($PlanOnly) { return }

New-Item -ItemType Directory -Force -Path $installDirFull | Out-Null
Grant-HeliosReadExecute $installDirFull
$copy = Copy-HeliosFileVerified $sourceDll $destDll 3 500
Grant-HeliosReadExecute $installDirFull

$json = [ordered]@{
  file_format_version = "1.0.1"
  ICD = [ordered]@{
    library_path = ($destDll -replace "\\", "/")
    library_arch = "64"
    api_version = $ApiVersion
  }
}
$expectedLibraryPath = $destDll -replace "\\", "/"
$manifestText = ($json | ConvertTo-Json -Depth 4) + [Environment]::NewLine
$parsed = $manifestText | ConvertFrom-Json
if ([string]$parsed.ICD.library_path -ne $expectedLibraryPath) {
  throw "Generated ICD manifest is invalid: expected library_path '$expectedLibraryPath'"
}

# A driver deploy is commonly followed immediately by a reboot. Set-Content
# alone may leave the manifest in the system cache; an abrupt reboot can then
# preserve its length while losing every data page (observed as a 253-byte file
# containing only NULs). Write the replacement through and flush it to stable
# storage before publishing its name. The old manifest remains authoritative
# until the final same-volume rename.
$manifestTmp = "$manifest.tmp.$PID.$([guid]::NewGuid().ToString('N'))"
$manifestBytes = [Text.Encoding]::ASCII.GetBytes($manifestText)
$manifestStream = $null
try {
  $manifestStream = [IO.FileStream]::new(
    $manifestTmp,
    [IO.FileMode]::CreateNew,
    [IO.FileAccess]::Write,
    [IO.FileShare]::None,
    4096,
    [IO.FileOptions]::WriteThrough)
  $manifestStream.Write($manifestBytes, 0, $manifestBytes.Length)
  $manifestStream.Flush($true)
} finally {
  if ($manifestStream) { $manifestStream.Dispose() }
}

try {
  $tmpText = Get-Content -LiteralPath $manifestTmp -Raw
  if ($tmpText.IndexOf([char]0) -ge 0) {
    throw "Generated ICD manifest contains NUL bytes: $manifestTmp"
  }
  $tmpParsed = $tmpText | ConvertFrom-Json
  if ([string]$tmpParsed.ICD.library_path -ne $expectedLibraryPath) {
    throw "Generated ICD manifest failed readback validation: $manifestTmp"
  }
  Move-Item -LiteralPath $manifestTmp -Destination $manifest -Force
} finally {
  Remove-Item -LiteralPath $manifestTmp -Force -ErrorAction SilentlyContinue
}

$installedText = Get-Content -LiteralPath $manifest -Raw
if ($installedText.IndexOf([char]0) -ge 0) {
  throw "Installed ICD manifest contains NUL bytes: $manifest"
}
$installedParsed = $installedText | ConvertFrom-Json
if ([string]$installedParsed.ICD.library_path -ne $expectedLibraryPath) {
  throw "Installed ICD manifest failed final validation: $manifest"
}
Grant-HeliosReadExecute $installDirFull

if ($PruneOld) {
  foreach ($oldDll in Get-ChildItem -LiteralPath $installDirFull -Filter "vulkan_virtio-*.dll" -ErrorAction SilentlyContinue) {
    if ($oldDll.FullName -ieq $destDll) { continue }
    $users = @(Get-HeliosFileUsers $oldDll.FullName)
    if ($users.Count -gt 0) {
      Write-Host "Keeping old ICD DLL still loaded: $($oldDll.FullName)"
    } else {
      Remove-Item -LiteralPath $oldDll.FullName -Force
      Write-Host "Removed old ICD DLL: $($oldDll.FullName)"
    }
  }
}

$cleanupRegPaths = @("HKLM:\SOFTWARE\Khronos\Vulkan\Drivers", "HKCU:\SOFTWARE\Khronos\Vulkan\Drivers")
$installRegPath = if ($Scope -eq "Machine") { "HKLM:\SOFTWARE\Khronos\Vulkan\Drivers" } else { "HKCU:\SOFTWARE\Khronos\Vulkan\Drivers" }
if (-not $NoRegistryCleanup) {
  foreach ($regPath in $cleanupRegPaths) { Remove-StaleHeliosIcdValues $regPath }
}
New-Item -Path $installRegPath -Force | Out-Null
New-ItemProperty -LiteralPath $installRegPath -Name $manifest -Value 0 -PropertyType DWord -Force | Out-Null

$destHash = Get-HeliosFileHash $destDll
if ($destHash -ne $sourceHash) { throw "ICD install failed: destination hash $destHash does not match source $sourceHash" }
Write-Host "Registered Vulkan ICD manifest: $manifest"
Write-Host "Installed ICD DLL: $($copy.Destination)"
Write-Host "SHA256: $destHash"
Get-Content -LiteralPath $manifest

if (-not $NoSmoke) {
  $vulkaninfo = Get-Command vulkaninfo.exe -ErrorAction SilentlyContinue
  if ($vulkaninfo) {
    Write-Host "Running vulkaninfo --summary with VK_DRIVER_FILES=$manifest"
    $env:VK_DRIVER_FILES = $manifest
    & $vulkaninfo.Source --summary
    if ($LASTEXITCODE -ne 0) {
      throw "vulkaninfo --summary failed with exit code $LASTEXITCODE"
    }
  } else {
    Write-Host "vulkaninfo.exe not found on PATH; skipping smoke test."
  }
}
