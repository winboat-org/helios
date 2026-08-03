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
if (-not (Test-Path -LiteralPath $statePath -PathType Leaf)) {
    throw "No package-managed Helios installation was found at $statePath."
}
$state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json

$vulkanRegistry = "HKLM:\SOFTWARE\Khronos\Vulkan\Drivers"
if (Test-Path -LiteralPath $vulkanRegistry) {
    Remove-ItemProperty -LiteralPath $vulkanRegistry -Name ([string]$state.vulkanManifest) -ErrorAction SilentlyContinue
}
$openClRegistry = "HKLM:\SOFTWARE\Khronos\OpenCL\Vendors"
if (Test-Path -LiteralPath $openClRegistry) {
    Remove-ItemProperty -LiteralPath $openClRegistry -Name ([string]$state.openClVendor) -ErrorAction SilentlyContinue
}

$classKey = [string]$state.classKey
if (Test-Path -LiteralPath $classKey) {
    Restore-HeliosRegistrySnapshot $classKey "OpenGLDriverName" $state.previousOpenGL.OpenGLDriverName
    Restore-HeliosRegistrySnapshot $classKey "OpenGLVersion" $state.previousOpenGL.OpenGLVersion
    Restore-HeliosRegistrySnapshot $classKey "OpenGLFlags" $state.previousOpenGL.OpenGLFlags
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
Write-Host "Helios runtime registrations were removed. Reboot Windows to finish unloading the driver."
