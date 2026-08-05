param(
    [switch]$EnableTestSigning,
    [switch]$RunSmokeTests,
    [Alias("Unattended")]
    [switch]$Automatic
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Helios-PackageCommon.ps1")

$stateRoot = Join-Path $env:ProgramData "Helios"
$statePath = Join-Path $stateRoot "install-state.json"
$provisioningRoot = Join-Path $stateRoot "provisioning"
$provisioningStatusPath = Join-Path $stateRoot "provisioning-status.json"
$provisioningTaskName = "HeliosGraphicsProvisioning"

function Write-HeliosProvisioningStatus(
    [Parameter(Mandatory)]
    [ValidateSet("waiting", "test-signing-restart-required", "driver-restart-required", "finished", "failed")]
    [string]$Status,
    [string]$Message = ""
) {
    $value = [ordered]@{
        status = $Status
        updatedAtUtc = [DateTime]::UtcNow.ToString("o")
    }
    if ($Message) { $value["message"] = $Message }
    Write-HeliosJson $value $provisioningStatusPath
}

function Initialize-HeliosAutomaticProvisioning([Parameter(Mandatory)][string]$BundleRoot) {
    $sourcePath = [IO.Path]::GetFullPath($BundleRoot).TrimEnd("\")
    $persistentPath = [IO.Path]::GetFullPath($provisioningRoot).TrimEnd("\")
    if (-not $sourcePath.Equals($persistentPath, [StringComparison]::OrdinalIgnoreCase)) {
        Remove-Item -LiteralPath $provisioningRoot -Recurse -Force -ErrorAction SilentlyContinue
        New-Item -ItemType Directory -Force -Path $provisioningRoot | Out-Null
        Get-ChildItem -LiteralPath $BundleRoot -Force |
            Copy-Item -Destination $provisioningRoot -Recurse -Force
    }

    $scriptPath = Join-Path $provisioningRoot "Install-Helios.ps1"
    $powerShellPath = Join-Path $env:SystemRoot "System32\WindowsPowerShell\v1.0\powershell.exe"
    $action = New-ScheduledTaskAction -Execute $powerShellPath -Argument (
        "-NoProfile -ExecutionPolicy Bypass -File `"$scriptPath`" -Automatic"
    )
    $trigger = New-ScheduledTaskTrigger -AtStartup
    Register-ScheduledTask -TaskName $provisioningTaskName -Action $action -Trigger $trigger `
        -User "SYSTEM" -RunLevel Highest -Force | Out-Null
}

function Complete-HeliosAutomaticProvisioning {
    Write-HeliosProvisioningStatus "finished"
    Unregister-ScheduledTask -TaskName $provisioningTaskName -Confirm:$false -ErrorAction SilentlyContinue
}

trap {
    if ($Automatic) {
        try { Write-HeliosProvisioningStatus "failed" $_.Exception.Message } catch {
            Write-Warning "Could not publish the automatic provisioning failure: $($_.Exception.Message)"
        }
    }
    throw
}

Assert-HeliosAdministrator
if (-not [Environment]::Is64BitProcess) {
    throw "Run the installer with 64-bit Windows PowerShell. This package contains x64 drivers only."
}

$bundleRoot = $PSScriptRoot
$manifest = Read-HeliosManifest $bundleRoot
Write-Host "Verifying $(@($manifest.files).Count) package files..."
Test-HeliosManifest $bundleRoot $manifest

if ($Automatic) {
    Initialize-HeliosAutomaticProvisioning $bundleRoot
    if (-not (Test-Path -LiteralPath $provisioningStatusPath -PathType Leaf)) {
        Write-HeliosProvisioningStatus "waiting"
    }
}

if (Test-Path -LiteralPath $statePath -PathType Leaf) {
    if ($Automatic) {
        & (Join-Path $stateRoot "Verify-Helios.ps1")
        Complete-HeliosAutomaticProvisioning
        exit 0
    }
    throw "Helios is already managed by this package installer. Run $stateRoot\Uninstall-Helios.ps1 before installing another bundle."
}

if ($manifest.signing.mode -eq "test" -and -not (Test-HeliosTestSigningEnabled)) {
    if (-not $EnableTestSigning -and -not $Automatic) {
        throw "Windows test-signing is not active. Re-run with -EnableTestSigning (or -Automatic), reboot, then run the installer again."
    }
    if (Test-HeliosSecureBootEnabled) {
        throw "Secure Boot is enabled. Windows will not enable test-signing until Secure Boot is disabled in UEFI."
    }
    Write-Host "Enabling Windows test-signing..."
    Invoke-HeliosNative "bcdedit.exe" @("/set", "testsigning", "on")
    if ($Automatic) { Write-HeliosProvisioningStatus "test-signing-restart-required" }
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
$replacedViogpudo = $false
$activeInfBeforeInstall = Get-HeliosActiveInf $instanceId
if ($activeInfBeforeInstall -and (Test-HeliosViogpudoDriver $activeInfBeforeInstall)) {
    if (-not $Automatic) {
        Write-Warning "The virtio-gpu device is currently using viogpudo ($activeInfBeforeInstall)."
        $approved = $false
        $usedGraphicalPrompt = $false
        if (-not $env:SSH_CONNECTION) {
            try {
                Add-Type -AssemblyName System.Windows.Forms
                $choice = [Windows.Forms.MessageBox]::Show(
                    "Uninstall viogpudo ($activeInfBeforeInstall) and replace it with the Helios display driver?",
                    "Helios display driver setup",
                    [Windows.Forms.MessageBoxButtons]::YesNo,
                    [Windows.Forms.MessageBoxIcon]::Warning,
                    [Windows.Forms.MessageBoxDefaultButton]::Button2
                )
                $usedGraphicalPrompt = $true
                $approved = $choice -eq [Windows.Forms.DialogResult]::Yes
            } catch {
                Write-Warning "The graphical confirmation dialog was unavailable: $($_.Exception.Message)"
            }
        }
        if (-not $usedGraphicalPrompt) {
            $answer = Read-Host "Uninstall viogpudo and replace it with the Helios display driver? [y/N]"
            $approved = $answer -match "^(?i:y|yes)$"
        }
        if (-not $approved) {
            throw "Helios installation was cancelled; viogpudo was not changed."
        }
    } else {
        Write-Host "Automatic mode: replacing viogpudo ($activeInfBeforeInstall) with Helios."
    }

    Invoke-HeliosNative "pnputil.exe" @("/delete-driver", $activeInfBeforeInstall, "/uninstall", "/force") -SuccessExitCodes @(0, 259, 3010)
    Invoke-HeliosNative "pnputil.exe" @("/scan-devices") -SuccessExitCodes @(0, 259, 3010)
    Start-Sleep -Seconds 2
    $remainingInf = Get-HeliosActiveInf $instanceId
    if ($remainingInf -and (Test-HeliosViogpudoDriver $remainingInf)) {
        $message = "Windows requires a reboot to finish removing viogpudo ($remainingInf). Run this installer again afterward."
        if ($Automatic) { throw $message }
        Write-Warning $message
        exit 3010
    }
    $replacedViogpudo = $true
}

$classKey = ""
$vulkanRegistry = "HKLM:\SOFTWARE\Khronos\Vulkan\Drivers"
$openClRegistry = "HKLM:\SOFTWARE\Khronos\OpenCL\Vendors"
$vulkanManifestPath = Join-Path $runtimeRoot "mesa\helios_vulkan.json"
$clvkPath = Join-Path $runtimeRoot "opencl\clvk.dll"
$wglPath = Join-Path $runtimeRoot "mesa\libgallium_wgl.dll"
$previousOpenGL = [ordered]@{
    OpenGLDriverName = [ordered]@{ exists = $false; kind = $null; value = $null }
    OpenGLVersion = [ordered]@{ exists = $false; kind = $null; value = $null }
    OpenGLFlags = [ordered]@{ exists = $false; kind = $null; value = $null }
}

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
    previousOpenGL = $previousOpenGL
    replacedViogpudo = $replacedViogpudo
    runtimeFiles = @()
}

New-Item -ItemType Directory -Force -Path $runtimeRoot,$stateRoot | Out-Null

# The cross-process present-order table is opened read/write by ordinary D3D
# producers, DWM (Window Manager\DWM-N), and RDPIDD (WUDFHost as LOCAL
# SERVICE). A table first created by DWM inherits an ACL that gives LOCAL
# SERVICE read-only access, so RDP silently copies unfinished frames without
# the producer-fence wait. Pre-create/repair this one coordination file before
# any new driver process starts; the UMD applies the same ACL when it creates
# the file independently of this installer.
$presentSyncPath = Join-Path $stateRoot "helios_present_sync_v2.bin"
if (-not (Test-Path -LiteralPath $presentSyncPath -PathType Leaf)) {
    New-Item -ItemType File -Path $presentSyncPath -Force | Out-Null
}
Invoke-HeliosNative "icacls.exe" @(
    $presentSyncPath,
    "/grant",
    "*S-1-5-11:(M)",  # Authenticated Users
    "*S-1-5-19:(M)",  # LOCAL SERVICE / RDPIDD
    "*S-1-5-90-0:(M)" # Window Manager group / DWM
)

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

function Test-HeliosCertificateStoreThumbprint(
    [Parameter(Mandatory)][string]$StoreName,
    [Parameter(Mandatory)][string]$Thumbprint
) {
    # The PowerShell Cert: provider can retain a stale view after a native
    # certificate-store update. Open a new X509Store for every check so a
    # certutil fallback is verified against the store itself.
    $x509Store = [Security.Cryptography.X509Certificates.X509Store]::new(
        $StoreName,
        [Security.Cryptography.X509Certificates.StoreLocation]::LocalMachine
    )
    try {
        $x509Store.Open([Security.Cryptography.X509Certificates.OpenFlags]::ReadOnly)
        $matches = $x509Store.Certificates.Find(
            [Security.Cryptography.X509Certificates.X509FindType]::FindByThumbprint,
            $Thumbprint,
            $false
        )
        return $matches.Count -gt 0
    } finally {
        $x509Store.Close()
    }
}

foreach ($store in @("Root", "TrustedPublisher")) {
    try {
        Import-Certificate -FilePath $certificatePath -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null
    } catch {
        # Some Windows builds report E_ACCESSDENIED after committing Root, but
        # fail before committing TrustedPublisher. Fall back to certutil only
        # when the exact thumbprint is absent.
        if (Test-HeliosCertificateStoreThumbprint $store $certificate.Thumbprint) {
            Write-Warning "Import-Certificate reported an error for $store, but the expected certificate is present."
        } else {
            Write-Warning "Import-Certificate failed for $store; retrying with certutil."
            Invoke-HeliosNative "certutil.exe" @("-addstore", "-f", $store, $certificatePath)
        }
    }
    if (-not (Test-HeliosCertificateStoreThumbprint $store $certificate.Thumbprint)) {
        throw "The Helios signing certificate was not installed in LocalMachine\$store."
    }
}
Write-HeliosJson $state $statePath

Write-Host "Installing/updating the Microsoft Visual C++ x64 runtime..."
Invoke-HeliosNative (Join-Path $payloadRoot "prerequisites\vc_redist.x64.exe") @("/install", "/quiet", "/norestart") -SuccessExitCodes @(0, 3010) -WaitForProcess

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
# pnputil returns ERROR_NO_MORE_ITEMS (259) when this exact package is already
# staged/active. The active-INF checks below still reject an outranking driver.
Invoke-HeliosNative "pnputil.exe" @("/add-driver", $driverInf, "/install") -SuccessExitCodes @(0, 259, 3010)
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
$classKey = Get-HeliosDisplayClassKey $instanceId
$state.classKey = $classKey
$state.previousOpenGL = [ordered]@{
    OpenGLDriverName = Get-HeliosRegistrySnapshot $classKey "OpenGLDriverName"
    OpenGLVersion = Get-HeliosRegistrySnapshot $classKey "OpenGLVersion"
    OpenGLFlags = Get-HeliosRegistrySnapshot $classKey "OpenGLFlags"
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
if ($Automatic) { Write-HeliosProvisioningStatus "driver-restart-required" }
Write-Warning "Reboot Windows before judging driver or desktop behavior."
exit 3010
