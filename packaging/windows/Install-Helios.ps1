param(
    [switch]$EnableTestSigning,
    [switch]$RunSmokeTests
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Helios-PackageCommon.ps1")

Assert-HeliosAdministrator
if (-not [Environment]::Is64BitProcess) {
    throw "Run the installer with 64-bit Windows PowerShell. This package contains x64 drivers only."
}

$bundleRoot = $PSScriptRoot
$manifest = Read-HeliosManifest $bundleRoot
Write-Host "Verifying $(@($manifest.files).Count) package files..."
Test-HeliosManifest $bundleRoot $manifest

$stateRoot = Join-Path $env:ProgramData "Helios"
$statePath = Join-Path $stateRoot "install-state.json"
if (Test-Path -LiteralPath $statePath -PathType Leaf) {
    throw "Helios is already managed by this package installer. Run $stateRoot\Uninstall-Helios.ps1 before installing another bundle."
}

if ($manifest.signing.mode -eq "test" -and -not (Test-HeliosTestSigningEnabled)) {
    if (-not $EnableTestSigning) {
        throw "Windows test-signing is not active. Re-run with -EnableTestSigning, reboot, then run the installer again."
    }
    if (Test-HeliosSecureBootEnabled) {
        throw "Secure Boot is enabled. Windows will not enable test-signing until Secure Boot is disabled in UEFI."
    }
    Write-Host "Enabling Windows test-signing..."
    Invoke-HeliosNative "bcdedit.exe" @("/set", "testsigning", "on")
    Write-Warning "Test-signing was enabled in the boot configuration. Reboot Windows, then run this installer again."
    exit 3010
}

$safePackageId = ([string]$manifest.packageId) -replace "[^A-Za-z0-9._-]", "_"
$installRoot = Join-Path $env:ProgramFiles "Helios\$safePackageId"
$runtimeRoot = Join-Path $installRoot "runtime"
$payloadRoot = Join-Path $bundleRoot "payload"
$driverInf = Join-Path $payloadRoot "driver\helios_kmd_render.inf"
$certificatePath = Join-Path $bundleRoot ([string]$manifest.signing.certificate)

$instanceId = Get-HeliosDeviceInstanceId
$classKey = Get-HeliosDisplayClassKey $instanceId
$vulkanRegistry = "HKLM:\SOFTWARE\Khronos\Vulkan\Drivers"
$openClRegistry = "HKLM:\SOFTWARE\Khronos\OpenCL\Vendors"
$vulkanManifestPath = Join-Path $runtimeRoot "mesa\helios_vulkan.json"
$clvkPath = Join-Path $runtimeRoot "opencl\clvk.dll"
$wglPath = Join-Path $runtimeRoot "mesa\libgallium_wgl.dll"

$state = [ordered]@{
    schemaVersion = 1
    packageId = [string]$manifest.packageId
    version = [string]$manifest.version
    installedAtUtc = [DateTime]::UtcNow.ToString("o")
    installRoot = $installRoot
    instanceId = $instanceId
    classKey = $classKey
    activeInf = ""
    signingCertificateThumbprint = ""
    vulkanManifest = $vulkanManifestPath
    openClVendor = $clvkPath
    installedVulkanLoader = $false
    installedOpenClLoader = $false
    systemVulkanLoaderHash = ""
    systemOpenClLoaderHash = ""
    previousOpenGL = [ordered]@{
        OpenGLDriverName = Get-HeliosRegistrySnapshot $classKey "OpenGLDriverName"
        OpenGLVersion = Get-HeliosRegistrySnapshot $classKey "OpenGLVersion"
        OpenGLFlags = Get-HeliosRegistrySnapshot $classKey "OpenGLFlags"
    }
    runtimeFiles = @()
}

New-Item -ItemType Directory -Force -Path $runtimeRoot,$stateRoot | Out-Null
Copy-Item -Path (Join-Path $payloadRoot "mesa") -Destination $runtimeRoot -Recurse -Force
Copy-Item -Path (Join-Path $payloadRoot "opencl") -Destination $runtimeRoot -Recurse -Force
Copy-Item -Path (Join-Path $payloadRoot "loaders") -Destination $runtimeRoot -Recurse -Force
if (Test-Path -LiteralPath (Join-Path $payloadRoot "smoke")) {
    Copy-Item -Path (Join-Path $payloadRoot "smoke") -Destination $runtimeRoot -Recurse -Force
}

foreach ($file in Get-ChildItem -LiteralPath $runtimeRoot -File -Recurse) {
    $state.runtimeFiles += [ordered]@{ path = $file.FullName; sha256 = Get-HeliosSha256 $file.FullName }
}
Write-HeliosJson $state $statePath

$certificate = [Security.Cryptography.X509Certificates.X509Certificate2]::new($certificatePath)
$state.signingCertificateThumbprint = $certificate.Thumbprint
Import-Certificate -FilePath $certificatePath -CertStoreLocation "Cert:\LocalMachine\Root" | Out-Null
Import-Certificate -FilePath $certificatePath -CertStoreLocation "Cert:\LocalMachine\TrustedPublisher" | Out-Null
Write-HeliosJson $state $statePath

Write-Host "Installing/updating the Microsoft Visual C++ x64 runtime..."
Invoke-HeliosNative (Join-Path $payloadRoot "prerequisites\vc_redist.x64.exe") @("/install", "/quiet", "/norestart") -SuccessExitCodes @(0, 3010)

$systemVulkanLoader = Join-Path $env:windir "System32\vulkan-1.dll"
if (-not (Test-Path -LiteralPath $systemVulkanLoader -PathType Leaf)) {
    Copy-Item -LiteralPath (Join-Path $runtimeRoot "loaders\vulkan-1.dll") -Destination $systemVulkanLoader -Force
    $state.installedVulkanLoader = $true
    $state.systemVulkanLoaderHash = Get-HeliosSha256 $systemVulkanLoader
}
$systemOpenClLoader = Join-Path $env:windir "System32\OpenCL.dll"
if (-not (Test-Path -LiteralPath $systemOpenClLoader -PathType Leaf)) {
    Copy-Item -LiteralPath (Join-Path $runtimeRoot "loaders\OpenCL.dll") -Destination $systemOpenClLoader -Force
    $state.installedOpenClLoader = $true
    $state.systemOpenClLoaderHash = Get-HeliosSha256 $systemOpenClLoader
}
Write-HeliosJson $state $statePath

Write-Host "Installing the Helios WDDM driver package..."
Invoke-HeliosNative "pnputil.exe" @("/add-driver", $driverInf, "/install")
Start-Sleep -Seconds 2
$activeInf = Get-HeliosActiveInf $instanceId
if ($activeInf -notmatch "^oem\d+\.inf$") {
    throw "PnP did not select a third-party Helios driver package (active INF: '$activeInf')."
}
$activeInfPath = Join-Path $env:windir "INF\$activeInf"
$activeInfText = if (Test-Path -LiteralPath $activeInfPath) { Get-Content -LiteralPath $activeInfPath -Raw } else { "" }
if ($activeInfText -notmatch "helios_kmd_render") {
    throw "PnP selected $activeInf, but it is not the Helios driver package."
}
$state.activeInf = $activeInf
Write-HeliosJson $state $statePath

$vulkanDll = Join-Path $runtimeRoot "mesa\vulkan_virtio.dll"
$vulkanJson = [ordered]@{
    file_format_version = "1.0.1"
    ICD = [ordered]@{
        library_path = ($vulkanDll -replace "\\", "/")
        library_arch = "64"
        api_version = [string]$manifest.components.mesa.vulkanApiVersion
    }
}
Write-HeliosJson $vulkanJson $vulkanManifestPath -Encoding ASCII
New-Item -Path $vulkanRegistry -Force | Out-Null
New-ItemProperty -LiteralPath $vulkanRegistry -Name $vulkanManifestPath -Value 0 -PropertyType DWord -Force | Out-Null

New-ItemProperty -LiteralPath $classKey -Name "OpenGLDriverName" -Value $wglPath -PropertyType String -Force | Out-Null
New-ItemProperty -LiteralPath $classKey -Name "OpenGLVersion" -Value 2 -PropertyType DWord -Force | Out-Null
New-ItemProperty -LiteralPath $classKey -Name "OpenGLFlags" -Value 1 -PropertyType DWord -Force | Out-Null

New-Item -Path $openClRegistry -Force | Out-Null
New-ItemProperty -LiteralPath $openClRegistry -Name $clvkPath -Value 0 -PropertyType DWord -Force | Out-Null

Copy-Item -LiteralPath (Join-Path $bundleRoot "Helios-PackageCommon.ps1") -Destination $stateRoot -Force
Copy-Item -LiteralPath (Join-Path $bundleRoot "Uninstall-Helios.ps1") -Destination $stateRoot -Force
Copy-Item -LiteralPath (Join-Path $bundleRoot "Verify-Helios.ps1") -Destination $stateRoot -Force
Write-HeliosJson $state $statePath

Write-Host ""
Write-Host "Helios $($manifest.version) is installed system-wide for x64 applications."
if ($RunSmokeTests) {
    & (Join-Path $stateRoot "Verify-Helios.ps1") -RunSmokeTests
} else {
    & (Join-Path $stateRoot "Verify-Helios.ps1") -AllowPendingReboot
}
Write-Warning "Reboot Windows before judging driver or desktop behavior."
exit 3010
