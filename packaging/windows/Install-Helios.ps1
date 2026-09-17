param(
    [switch]$EnableTestSigning,
    [switch]$RunSmokeTests,
    [Alias("Unattended")]
    [switch]$Automatic,
    # Re-apply the whole package over an existing installation without the
    # "already managed" refusal, preserving the original pre-Helios rollback
    # snapshots rather than capturing Helios's own values as the restore point.
    [switch]$Repair,
    # Overwrite semantics for unattended callers: identical to -Repair.
    [switch]$Force,
    # The GUI/silent front-end is non-interactive and cannot answer the
    # viogpudo prompt; installing Helios already implies replacing the display
    # driver, so it approves the removal without a console prompt.
    [switch]$ReplaceViogpudo
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
    $isPersistentBundle = $sourcePath.Equals($persistentPath, [StringComparison]::OrdinalIgnoreCase)
    if (-not $isPersistentBundle) {
        Remove-Item -LiteralPath $provisioningRoot -Recurse -Force -ErrorAction SilentlyContinue
        New-Item -ItemType Directory -Force -Path $provisioningRoot | Out-Null
        Get-ChildItem -LiteralPath $BundleRoot -Force |
            Copy-Item -Destination $provisioningRoot -Recurse -Force
    }

    $existingTask = Get-ScheduledTask -TaskName $provisioningTaskName -ErrorAction SilentlyContinue
    if ($isPersistentBundle -and $existingTask) { return }

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
    # The status file is the machine-readable contract any orchestrator (not
    # only WinBoat) can watch, so it is written on every path, not just
    # -Automatic.
    try { Write-HeliosProvisioningStatus "failed" $_.Exception.Message } catch {
        Write-Warning "Could not publish the provisioning failure: $($_.Exception.Message)"
    }
    # A bare `throw` here would replace the real error with a generic
    # "ScriptHalted" RuntimeException at this line, which is exactly as useful
    # as no error at all. Surface the message and rethrow the original record.
    Write-Warning "Helios setup failed: $($_.Exception.Message)"
    throw $_
}

Assert-HeliosAdministrator
if (-not [Environment]::Is64BitProcess) {
    throw "Run the installer with native 64-bit Windows PowerShell to manage both registry views and system directories."
}
if ($Force) { $Repair = $true }
Write-HeliosProgress 3 "Checking administrator rights"

$bundleRoot = $PSScriptRoot
$manifest = Read-HeliosManifest $bundleRoot
Write-Host "Verifying $(@($manifest.files).Count) package files..."
Test-HeliosManifest $bundleRoot $manifest
Write-HeliosProgress 7 "Verified $($manifest.version) package files"

if ($Automatic) { Initialize-HeliosAutomaticProvisioning $bundleRoot }
# Publish `waiting` for a fresh run, but never overwrite a terminal `finished`:
# an observer (WinBoat) that has already completed must not be pulled back.
if (-not (Test-Path -LiteralPath $provisioningStatusPath -PathType Leaf)) {
    Write-HeliosProvisioningStatus "waiting"
}

$previousState = $null
if (Test-Path -LiteralPath $statePath -PathType Leaf) {
    $existingState = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
    # `-Automatic` over an existing install is the post-reboot completion step
    # ONLY when the bundle version matches. A different version is an update and
    # must go through the re-apply path, or shipping a newer OEM bundle would be
    # a silent no-op that just reports `finished`.
    $isUpdate = $Repair -or ([string]$existingState.version -ne [string]$manifest.version)
    if ($isUpdate) {
        $previousState = $existingState
        Write-Host "Re-applying $($manifest.version) over the existing installation ($($existingState.version))."
        Write-HeliosProgress 12 "Preparing to overwrite the existing installation"
    } elseif ($Automatic) {
        & (Join-Path $stateRoot "Verify-Helios.ps1")
        Complete-HeliosAutomaticProvisioning
        exit 0
    } else {
        throw "Helios is already managed by this package installer. Run $stateRoot\Uninstall-Helios.ps1 before installing another bundle."
    }
}
if (-not $previousState) { Write-HeliosProgress 12 "Preparing the installation" }

if ($manifest.signing.mode -eq "test" -and -not (Test-HeliosTestSigningEnabled)) {
    if (-not $EnableTestSigning -and -not $Automatic) {
        throw "Windows test-signing is not active. Re-run with -EnableTestSigning (or -Automatic), reboot, then run the installer again."
    }
    if (Test-HeliosSecureBootEnabled) {
        throw "Secure Boot is enabled. Windows will not enable test-signing until Secure Boot is disabled in UEFI."
    }
    Write-Host "Enabling Windows test-signing..."
    Invoke-HeliosNative "bcdedit.exe" @("/set", "testsigning", "on")
    Write-HeliosProvisioningStatus "test-signing-restart-required"
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
# Snapshot before any PnP change: an older native-only INF cannot delete a
# UserModeDriverNameWoW value that a newer package introduces into its key.
$previousDirect3D = [ordered]@{ activeInf = $activeInfBeforeInstall; infSha256 = ""; values = [ordered]@{} }
if ($previousState -and $previousState.PSObject.Properties["previousDirect3D"] -and $previousState.previousDirect3D) {
    # A repair must keep the ORIGINAL pre-Helios Direct3D registration. Capturing
    # the registry now would record Helios's own values as the restore point, so
    # a later uninstall would "restore" Helios over itself.
    $previousDirect3D = $previousState.previousDirect3D
} elseif ($activeInfBeforeInstall -and -not $previousState) {
    $previousDirect3D.infSha256 = Get-HeliosSha256 (Join-Path $env:windir "INF\$activeInfBeforeInstall")
    $previousClassKey = Get-HeliosDisplayClassKey $instanceId
    foreach ($name in @("UserModeDriverName", "UserModeDriverNameWoW", "InstalledDisplayDrivers")) {
        $previousDirect3D.values[$name] = Get-HeliosRegistrySnapshot $previousClassKey $name
    }
}
if ($activeInfBeforeInstall -and (Test-HeliosViogpudoDriver $activeInfBeforeInstall)) {
    if (-not ($Automatic -or $ReplaceViogpudo)) {
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
        Write-Host "Unattended mode: replacing viogpudo ($activeInfBeforeInstall) with Helios."
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
$vulkanRegistryX86 = "HKLM:\SOFTWARE\WOW6432Node\Khronos\Vulkan\Drivers"
$openClRegistry = "HKLM:\SOFTWARE\Khronos\OpenCL\Vendors"
$vulkanManifestPath = Join-Path $runtimeRoot "mesa\helios_vulkan.json"
$vulkanManifestX86Path = Join-Path $runtimeRoot "mesa\x86\helios_vulkan.json"
$clvkPath = Join-Path $runtimeRoot "opencl\clvk.dll"
$wglPath = Join-Path $runtimeRoot "mesa\libgallium_wgl.dll"
$wglX86Path = Join-Path $runtimeRoot "mesa\x86\libgallium_wgl.dll"
$previousOpenGL = [ordered]@{
    OpenGLDriverName = [ordered]@{ exists = $false; kind = $null; value = $null }
    OpenGLVersion = [ordered]@{ exists = $false; kind = $null; value = $null }
    OpenGLFlags = [ordered]@{ exists = $false; kind = $null; value = $null }
    OpenGLDriverNameWow = [ordered]@{ exists = $false; kind = $null; value = $null }
    OpenGLVersionWow = [ordered]@{ exists = $false; kind = $null; value = $null }
    OpenGLFlagsWow = [ordered]@{ exists = $false; kind = $null; value = $null }
}

# -Repair must preserve the pre-Helios rollback state and the recorded loader
# ownership; only a fresh install captures the live values.
if ($previousState -and $previousState.PSObject.Properties["previousOpenGL"] -and $previousState.previousOpenGL) {
    $previousOpenGL = $previousState.previousOpenGL
}
function Get-PreviousStateValue([string]$Name, $Default) {
    if ($previousState -and $previousState.PSObject.Properties[$Name]) { return $previousState.$Name }
    return $Default
}
$installedAtUtc = Get-PreviousStateValue "installedAtUtc" ([DateTime]::UtcNow.ToString("o"))
$signingThumbprint = [string](Get-PreviousStateValue "signingCertificateThumbprint" "")
$hadVulkanLoader = [bool](Get-PreviousStateValue "installedVulkanLoader" $false)
$hadVulkanLoaderX86 = [bool](Get-PreviousStateValue "installedVulkanLoaderX86" $false)
$hadOpenClLoader = [bool](Get-PreviousStateValue "installedOpenClLoader" $false)
$vulkanLoaderHash = [string](Get-PreviousStateValue "systemVulkanLoaderHash" "")
$vulkanLoaderX86Hash = [string](Get-PreviousStateValue "systemVulkanLoaderX86Hash" "")
$openClLoaderHash = [string](Get-PreviousStateValue "systemOpenClLoaderHash" "")
$replacedViogpudoBefore = [bool](Get-PreviousStateValue "replacedViogpudo" $replacedViogpudo)

$state = [ordered]@{
    schemaVersion = 1
    packageId = [string]$manifest.packageId
    publisher = Get-HeliosPackagePublisher $manifest
    version = [string]$manifest.version
    installedAtUtc = $installedAtUtc
    installRoot = $installRoot
    instanceId = $instanceId
    classKey = $classKey
    activeInf = ""
    activeInfSha256 = ""
    signingCertificateThumbprint = $signingThumbprint
    vulkanManifest = $vulkanManifestPath
    vulkanManifestX86 = $vulkanManifestX86Path
    openClVendor = $clvkPath
    installedVulkanLoader = $hadVulkanLoader
    installedVulkanLoaderX86 = $hadVulkanLoaderX86
    installedOpenClLoader = $hadOpenClLoader
    systemVulkanLoaderHash = $vulkanLoaderHash
    systemVulkanLoaderX86Hash = $vulkanLoaderX86Hash
    systemOpenClLoaderHash = $openClLoaderHash
    previousOpenGL = $previousOpenGL
    previousDirect3D = $previousDirect3D
    installedDirect3D = [ordered]@{}
    replacedViogpudo = $replacedViogpudoBefore
    runtimeFiles = @()
    driverFiles = @()
}

New-Item -ItemType Directory -Force -Path $runtimeRoot,$stateRoot | Out-Null

# The runtime root is keyed by packageId, so an existing one is always this
# exact package's CODE (even after an update re-signed the payload copies).
# Its files may be mapped by a running process and therefore un-overwritable, so
# skip existing files instead of failing a re-apply over them.
$runtimeExists = Test-Path -LiteralPath $runtimeRoot
Copy-HeliosTreeIfChanged (Join-Path $payloadRoot "mesa") (Join-Path $runtimeRoot "mesa") -SkipExisting:$runtimeExists
Copy-HeliosTreeIfChanged (Join-Path $payloadRoot "opencl") (Join-Path $runtimeRoot "opencl") -SkipExisting:$runtimeExists
Copy-HeliosTreeIfChanged (Join-Path $payloadRoot "loaders") (Join-Path $runtimeRoot "loaders") -SkipExisting:$runtimeExists
if (Test-Path -LiteralPath (Join-Path $payloadRoot "smoke")) {
    Copy-HeliosTreeIfChanged (Join-Path $payloadRoot "smoke") (Join-Path $runtimeRoot "smoke") -SkipExisting:$runtimeExists
}

foreach ($file in Get-ChildItem -LiteralPath $runtimeRoot -File -Recurse) {
    $state.runtimeFiles += [ordered]@{ path = $file.FullName; sha256 = Get-HeliosSha256 $file.FullName }
}
# PnP owns all four UMD copies and registrations as one catalogued package.
# Retain the source hashes so verification can reject stale DriverStore images,
# including accidentally renamed AMD64 binaries in the WoW64 slots.
foreach ($name in @("helios_kmd_render.sys", "helios_umd.dll", "helios_umd12.dll", "helios_umd32.dll", "helios_umd12_32.dll")) {
    $state.driverFiles += [ordered]@{
        name = $name
        sha256 = Get-HeliosSha256 (Join-Path $payloadRoot "driver\$name")
    }
}
Write-HeliosJson $state $statePath

$certificate = [Security.Cryptography.X509Certificates.X509Certificate2]::new($certificatePath)
$state.signingCertificateThumbprint = $certificate.Thumbprint
Write-HeliosProgress 25 "Installing the Helios signing certificate"

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

# Both UMDs link the static CRT, so no Visual C++ runtime is installed or
# required. (The D3D11 UMD has always been static; the D3D12 UMD switched with
# vkd3d built `-Db_vscrt=mt`.)

$systemVulkanLoader = Join-Path $env:windir "System32\vulkan-1.dll"
Write-HeliosProgress 52 "Registering the Khronos loaders"
if (-not (Test-Path -LiteralPath $systemVulkanLoader -PathType Leaf)) {
    Copy-Item -LiteralPath (Join-Path $runtimeRoot "loaders\vulkan-1.dll") -Destination $systemVulkanLoader -Force
    $state.installedVulkanLoader = $true
    $state.systemVulkanLoaderHash = Get-HeliosSha256 $systemVulkanLoader
}
$systemVulkanLoaderX86 = Join-Path $env:windir "SysWOW64\vulkan-1.dll"
if (-not (Test-Path -LiteralPath $systemVulkanLoaderX86 -PathType Leaf)) {
    Copy-Item -LiteralPath (Join-Path $runtimeRoot "loaders\x86\vulkan-1.dll") -Destination $systemVulkanLoaderX86 -Force
    $state.installedVulkanLoaderX86 = $true
    $state.systemVulkanLoaderX86Hash = Get-HeliosSha256 $systemVulkanLoaderX86
}
$systemOpenClLoader = Join-Path $env:windir "System32\OpenCL.dll"
if (-not (Test-Path -LiteralPath $systemOpenClLoader -PathType Leaf)) {
    Copy-Item -LiteralPath (Join-Path $runtimeRoot "loaders\OpenCL.dll") -Destination $systemOpenClLoader -Force
    $state.installedOpenClLoader = $true
    $state.systemOpenClLoaderHash = Get-HeliosSha256 $systemOpenClLoader
}
Write-HeliosJson $state $statePath

Write-Host "Installing the Helios WDDM driver package..."
Write-HeliosProgress 64 "Installing the Helios WDDM driver package"
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
if (-not $previousState) {
    $state.previousOpenGL = [ordered]@{
        OpenGLDriverName = Get-HeliosRegistrySnapshot $classKey "OpenGLDriverName"
        OpenGLVersion = Get-HeliosRegistrySnapshot $classKey "OpenGLVersion"
        OpenGLFlags = Get-HeliosRegistrySnapshot $classKey "OpenGLFlags"
        OpenGLDriverNameWow = Get-HeliosRegistrySnapshot $classKey "OpenGLDriverNameWow"
        OpenGLVersionWow = Get-HeliosRegistrySnapshot $classKey "OpenGLVersionWow"
        OpenGLFlagsWow = Get-HeliosRegistrySnapshot $classKey "OpenGLFlagsWow"
    }
} else {
    Write-Host "Keeping the recorded pre-Helios OpenGL registration as the restore point."
}
$state.activeInf = $activeInf
$state.activeInfSha256 = Get-HeliosSha256 $activeInfPath
foreach ($name in @("UserModeDriverName", "UserModeDriverNameWoW", "InstalledDisplayDrivers")) {
    $state.installedDirect3D[$name] = Get-HeliosRegistrySnapshot $classKey $name
}
Write-HeliosJson $state $statePath

$vulkanDll = Join-Path $runtimeRoot "mesa\vulkan_virtio.dll"
Write-HeliosProgress 82 "Registering Vulkan, OpenGL and OpenCL"
$vulkanJson = [ordered]@{
    file_format_version = "1.0.1"
    ICD = [ordered]@{
        library_path = ($vulkanDll -replace "\\", "/")
        library_arch = "64"
        api_version = [string]$manifest.components.mesa.vulkanApiVersion
    }
}
Write-HeliosJson $vulkanJson $vulkanManifestPath -Encoding ASCII
Ensure-HeliosRegistryKey $vulkanRegistry
New-ItemProperty -LiteralPath $vulkanRegistry -Name $vulkanManifestPath -Value 0 -PropertyType DWord -Force | Out-Null

$vulkanX86Dll = Join-Path $runtimeRoot "mesa\x86\vulkan_virtio.dll"
$vulkanX86Json = [ordered]@{
    file_format_version = "1.0.1"
    ICD = [ordered]@{
        library_path = ($vulkanX86Dll -replace "\\", "/")
        library_arch = "32"
        api_version = [string]$manifest.components.mesa.vulkanApiVersion
    }
}
Write-HeliosJson $vulkanX86Json $vulkanManifestX86Path -Encoding ASCII
Ensure-HeliosRegistryKey $vulkanRegistryX86
New-ItemProperty -LiteralPath $vulkanRegistryX86 -Name $vulkanManifestX86Path -Value 0 -PropertyType DWord -Force | Out-Null

New-ItemProperty -LiteralPath $classKey -Name "OpenGLDriverName" -Value $wglPath -PropertyType String -Force | Out-Null
New-ItemProperty -LiteralPath $classKey -Name "OpenGLVersion" -Value 2 -PropertyType DWord -Force | Out-Null
New-ItemProperty -LiteralPath $classKey -Name "OpenGLFlags" -Value 1 -PropertyType DWord -Force | Out-Null
New-ItemProperty -LiteralPath $classKey -Name "OpenGLDriverNameWow" -Value $wglX86Path -PropertyType String -Force | Out-Null
New-ItemProperty -LiteralPath $classKey -Name "OpenGLVersionWow" -Value 2 -PropertyType DWord -Force | Out-Null
New-ItemProperty -LiteralPath $classKey -Name "OpenGLFlagsWow" -Value 1 -PropertyType DWord -Force | Out-Null

Ensure-HeliosRegistryKey $openClRegistry
New-ItemProperty -LiteralPath $openClRegistry -Name $clvkPath -Value 0 -PropertyType DWord -Force | Out-Null

# A version change installs under a new packageId/installRoot, so the previous
# version's Khronos registrations and runtime tree would otherwise be orphaned
# forever: the Vulkan loader would see two Helios ICDs and the old tree would
# never be removed (uninstall only knows the current paths). Retire them now
# that the new registrations exist.
if ($previousState -and [string]$previousState.installRoot -and
    ([string]$previousState.installRoot -ine $installRoot)) {
    foreach ($entry in @(
        [ordered]@{ Path = $vulkanRegistry;    Name = [string]$previousState.vulkanManifest },
        [ordered]@{ Path = $vulkanRegistryX86; Name = [string]$previousState.vulkanManifestX86 },
        [ordered]@{ Path = $openClRegistry;    Name = [string]$previousState.openClVendor }
    )) {
        if ($entry.Name -and (Test-Path -LiteralPath $entry.Path)) {
            Remove-ItemProperty -LiteralPath $entry.Path -Name $entry.Name -ErrorAction SilentlyContinue
        }
    }
    $previousRoot = [string]$previousState.installRoot
    if (Test-Path -LiteralPath $previousRoot) {
        try {
            Remove-Item -LiteralPath $previousRoot -Recurse -Force
            Write-Host "Removed the previous runtime $previousRoot."
        } catch {
            Write-Warning "The previous runtime $previousRoot is still loaded and was not removed: $($_.Exception.Message)"
        }
    }
}

Copy-Item -LiteralPath (Join-Path $bundleRoot "Helios-PackageCommon.ps1") -Destination $stateRoot -Force
Copy-Item -LiteralPath (Join-Path $bundleRoot "Uninstall-Helios.ps1") -Destination $stateRoot -Force
Copy-Item -LiteralPath (Join-Path $bundleRoot "Verify-Helios.ps1") -Destination $stateRoot -Force
# Keep the manifest beside the scripts so a stored uninstaller can read the
# version. The installer exe is NOT copied: it is self-contained and embedding it
# in its own payload would duplicate the whole bundle.
if (Test-Path -LiteralPath (Join-Path $bundleRoot "manifest.json") -PathType Leaf) {
    Copy-Item -LiteralPath (Join-Path $bundleRoot "manifest.json") -Destination $stateRoot -Force
}
# The engine licenses and the DaVinci Resolve shim live beside the stored
# uninstaller: the bundle embeds them, but its extraction directory is deleted
# when the installer exits.
foreach ($extra in @("licenses", "compatibility")) {
    $extraSource = Join-Path $bundleRoot $extra
    if (Test-Path -LiteralPath $extraSource -PathType Container) {
        Copy-HeliosTreeIfChanged $extraSource (Join-Path $stateRoot $extra)
    }
}
Write-HeliosJson $state $statePath

Write-Host ""
Write-Host "Helios $($manifest.version) is installed system-wide with x64 and WoW64 Direct3D 11/12, OpenGL, and Vulkan support."
Write-HeliosProgress 94 "Verifying the installation"
if ($RunSmokeTests) {
    & (Join-Path $stateRoot "Verify-Helios.ps1") -RunSmokeTests
} else {
    & (Join-Path $stateRoot "Verify-Helios.ps1") -AllowPendingReboot
}
# Do not pull an observer that already saw `finished` back into a reboot path.
$currentStatus = if (Test-Path -LiteralPath $provisioningStatusPath -PathType Leaf) {
    (Get-Content -LiteralPath $provisioningStatusPath -Raw | ConvertFrom-Json).status
} else { "" }
if ($currentStatus -ne "finished") { Write-HeliosProvisioningStatus "driver-restart-required" }
Write-HeliosProgress 100 "Installation complete"
Write-Warning "Reboot Windows before judging driver or desktop behavior."
exit 3010
