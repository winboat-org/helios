param(
    [switch]$KeepDriver,
    [switch]$RemoveKhronosLoaders
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Helios-PackageCommon.ps1")

Assert-HeliosAdministrator
$stateRoot = Join-Path $env:ProgramData "Helios"
$statePath = Join-Path $stateRoot "install-state.json"
$resolveCompatibilityState = Join-Path $stateRoot "compatibility\DaVinci Resolve\install-state.json"
if (Test-Path -LiteralPath $resolveCompatibilityState -PathType Leaf) {
    Write-Warning "DaVinci Resolve compatibility remains installed and has an independent rollback state."
    Write-Warning "Close Resolve and run 'C:\ProgramData\Helios\compatibility\DaVinci Resolve\Uninstall-Resolve-Compatibility.ps1' to remove it."
}
Unregister-ScheduledTask -TaskName "HeliosGraphicsProvisioning" -Confirm:$false -ErrorAction SilentlyContinue
if (-not (Test-Path -LiteralPath $statePath -PathType Leaf)) {
    throw "No package-managed Helios installation was found at $statePath."
}
$state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json

$vulkanRegistry = "HKLM:\SOFTWARE\Khronos\Vulkan\Drivers"
if (Test-Path -LiteralPath $vulkanRegistry) {
    Remove-ItemProperty -LiteralPath $vulkanRegistry -Name ([string]$state.vulkanManifest) -ErrorAction SilentlyContinue
}
$vulkanRegistryX86 = "HKLM:\SOFTWARE\WOW6432Node\Khronos\Vulkan\Drivers"
if (Test-Path -LiteralPath $vulkanRegistryX86) {
    Remove-ItemProperty -LiteralPath $vulkanRegistryX86 -Name ([string]$state.vulkanManifestX86) -ErrorAction SilentlyContinue
}
$openClRegistry = "HKLM:\SOFTWARE\Khronos\OpenCL\Vendors"
if (Test-Path -LiteralPath $openClRegistry) {
    Remove-ItemProperty -LiteralPath $openClRegistry -Name ([string]$state.openClVendor) -ErrorAction SilentlyContinue
}

$classKey = [string]$state.classKey
if ($classKey -and (Test-Path -LiteralPath $classKey)) {
    Restore-HeliosRegistrySnapshot $classKey "OpenGLDriverName" $state.previousOpenGL.OpenGLDriverName
    Restore-HeliosRegistrySnapshot $classKey "OpenGLVersion" $state.previousOpenGL.OpenGLVersion
    Restore-HeliosRegistrySnapshot $classKey "OpenGLFlags" $state.previousOpenGL.OpenGLFlags
    Restore-HeliosRegistrySnapshot $classKey "OpenGLDriverNameWow" $state.previousOpenGL.OpenGLDriverNameWow
    Restore-HeliosRegistrySnapshot $classKey "OpenGLVersionWow" $state.previousOpenGL.OpenGLVersionWow
    Restore-HeliosRegistrySnapshot $classKey "OpenGLFlagsWow" $state.previousOpenGL.OpenGLFlagsWow
}

# If a VM configuration change moved the adapter after installation, remove a
# Helios registration from the current instance as well. Do not touch values
# that no longer point at this package's WGL ICD.
try {
    $currentInstanceId = Get-HeliosDeviceInstanceId
    $currentClassKey = Get-HeliosDisplayClassKey $currentInstanceId
    if ($currentClassKey -and $currentClassKey -ine $classKey) {
        $expectedWgl = Join-Path ([string]$state.installRoot) "runtime\mesa\libgallium_wgl.dll"
        $currentWgl = (Get-Item -LiteralPath $currentClassKey).GetValue("OpenGLDriverName", $null)
        if ($currentWgl -and ([string]$currentWgl -ieq $expectedWgl)) {
            foreach ($name in @("OpenGLDriverName", "OpenGLVersion", "OpenGLFlags")) {
                Remove-ItemProperty -LiteralPath $currentClassKey -Name $name -ErrorAction SilentlyContinue
            }
        }
        $expectedWglX86 = Join-Path ([string]$state.installRoot) "runtime\mesa\x86\libgallium_wgl.dll"
        $currentWglX86 = (Get-Item -LiteralPath $currentClassKey).GetValue("OpenGLDriverNameWow", $null)
        if ($currentWglX86 -and ([string]$currentWglX86 -ieq $expectedWglX86)) {
            foreach ($name in @("OpenGLDriverNameWow", "OpenGLVersionWow", "OpenGLFlagsWow")) {
                Remove-ItemProperty -LiteralPath $currentClassKey -Name $name -ErrorAction SilentlyContinue
            }
        }
    }
} catch {
    Write-Warning "Could not inspect the current adapter software key during cleanup: $($_.Exception.Message)"
}

$driverRemovalFailed = $false
if (-not $KeepDriver -and [string]$state.activeInf) {
    Write-Host "Removing driver package $($state.activeInf)..."
    try {
        Invoke-HeliosNative "pnputil.exe" @("/delete-driver", [string]$state.activeInf, "/uninstall", "/force")
    } catch {
        $driverRemovalFailed = $true
        Write-Warning "Driver removal did not complete: $($_.Exception.Message)"
        Write-Warning "The runtime registrations were removed; pnputil may require a reboot before retrying."
    }
} elseif (-not $KeepDriver) {
    $driverRemovalFailed = $true
    Write-Warning "The installed OEM INF name was not recorded, so the driver and test certificate were kept."
}

if ($RemoveKhronosLoaders) {
    $loaderCandidates = @(
        [ordered]@{
            installed = [bool]$state.installedVulkanLoader
            path = Join-Path $env:windir "System32\vulkan-1.dll"
            hash = [string]$state.systemVulkanLoaderHash
        },
        [ordered]@{
            installed = [bool]$state.installedVulkanLoaderX86
            path = Join-Path $env:windir "SysWOW64\vulkan-1.dll"
            hash = [string]$state.systemVulkanLoaderX86Hash
        },
        [ordered]@{
            installed = [bool]$state.installedOpenClLoader
            path = Join-Path $env:windir "System32\OpenCL.dll"
            hash = [string]$state.systemOpenClLoaderHash
        }
    )
    foreach ($loader in $loaderCandidates) {
        if (-not $loader.installed -or -not (Test-Path -LiteralPath $loader.path -PathType Leaf)) { continue }
        if ((Get-HeliosSha256 $loader.path) -ne $loader.hash) {
            Write-Warning "Keeping $($loader.path): it changed after Helios installed it."
            continue
        }
        Remove-Item -LiteralPath $loader.path -Force
        Write-Host "Removed package-installed loader $($loader.path)"
    }
} else {
    Write-Host "Keeping Khronos loader DLLs. Pass -RemoveKhronosLoaders to remove unchanged loaders installed by Helios."
}

$thumbprint = [string]$state.signingCertificateThumbprint
if ($thumbprint -and -not $KeepDriver -and -not $driverRemovalFailed) {
    foreach ($store in @("Root", "TrustedPublisher")) {
        $certificate = "Cert:\LocalMachine\$store\$thumbprint"
        if (Test-Path -LiteralPath $certificate) {
            Remove-Item -LiteralPath $certificate -Force
        }
    }
}

$installRoot = [string]$state.installRoot
if (Test-Path -LiteralPath $installRoot) {
    try { Remove-Item -LiteralPath $installRoot -Recurse -Force } catch {
        Write-Warning "Some runtime files are still loaded and could not be removed: $installRoot"
    }
}
if ($driverRemovalFailed) {
    throw "Runtime registrations were removed, but driver removal is incomplete. The state and signing certificate were kept so the uninstall can be retried."
}
Remove-Item -LiteralPath $statePath -Force
Remove-Item -LiteralPath (Join-Path $stateRoot "provisioning") -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath (Join-Path $stateRoot "provisioning-status.json") -Force -ErrorAction SilentlyContinue
Write-Host "Helios runtime registrations were removed. Reboot Windows to finish unloading the driver."
