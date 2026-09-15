param(
    [Parameter(Mandatory)][string]$RepoRoot,
    [Parameter(Mandatory)][string]$DriverArtifact,
    [Parameter(Mandatory)][string]$MesaArtifact,
    [Parameter(Mandatory)][string]$MesaX86Artifact,
    [Parameter(Mandatory)][string]$OpenClArtifact,
    [Parameter(Mandatory)][string]$LoadersArtifact,
    [Parameter(Mandatory)][string]$CompatibilityArtifact,
    [Parameter(Mandatory)][string]$InstallerArtifact,
    [Parameter(Mandatory)][string]$OutputDir,
    [Parameter(Mandatory)][string]$Version,
    [Parameter(Mandatory)][ValidateSet("Debug", "Release")][string]$Configuration,
    [Parameter(Mandatory)][string]$RepositoryCommit,
    [Parameter(Mandatory)][string]$MesaCommit,
    [Parameter(Mandatory)][string]$DxvkCommit,
    [Parameter(Mandatory)][string]$Vkd3dCommit,
    [Parameter(Mandatory)][string]$ClvkCommit,
    [Parameter(Mandatory)][string]$VulkanLoaderCommit,
    [Parameter(Mandatory)][string]$VulkanHeadersCommit,
    [Parameter(Mandatory)][string]$OpenClLoaderCommit,
    [Parameter(Mandatory)][string]$OpenClHeadersCommit
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Initialize-HeliosBuild.ps1")
. (Join-Path $RepoRoot "packaging\windows\Helios-PackageCommon.ps1")
. (Join-Path $RepoRoot "metadata\Read-HeliosMetadata.ps1")
$metadata = Read-HeliosMetadata $RepoRoot
if ($Version -ne $metadata.HELIOS_KMD_VERSION) {
    throw "Package version $Version differs from kmd_render/driver-version.env ($($metadata.HELIOS_KMD_VERSION))."
}
Import-VisualStudioEnvironment

function Copy-Required([string]$Source, [string]$Destination) {
    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) { throw "Required artifact is missing: $Source" }
    $parent = Split-Path -Parent $Destination
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    Copy-Item -LiteralPath $Source -Destination $Destination -Force
}

function Invoke-SignTool([string]$SignTool, [string]$Thumbprint, [string]$Path) {
    & $SignTool sign /v /fd SHA256 /sha1 $Thumbprint /s My $Path
    if ($LASTEXITCODE -ne 0) { throw "signtool failed to sign $Path." }
}

$shortCommit = $RepositoryCommit.Substring(0, 8)
$configurationSuffix = if ($Configuration -eq "Debug") { "-debug" } else { "" }
$packageId = "helios-windows-x64-$Version-$shortCommit$configurationSuffix"
$stagingRoot = Join-Path (Join-Path $OutputDir "staging") $packageId
$payload = Join-Path $stagingRoot "payload"
# Symbols are never embedded in the installer. They are useless at install time
# and are ~25 MB even after compression, so they ship as a separate artifact.
$symbolsRoot = Join-Path $OutputDir "$packageId-symbols"
if (Test-Path -LiteralPath $stagingRoot) { Remove-Item -LiteralPath $stagingRoot -Recurse -Force }
New-Item -ItemType Directory -Force -Path $payload | Out-Null

# The driver and installer artifacts must be the same configuration as this
# bundle, or a "debug" bundle silently ships release binaries.
$driverConfigurationFile = Join-Path $DriverArtifact "configuration.txt"
if (Test-Path -LiteralPath $driverConfigurationFile -PathType Leaf) {
    $driverConfiguration = (Get-Content -LiteralPath $driverConfigurationFile -Raw).Trim()
    if ($driverConfiguration -ne $Configuration) {
        throw "Driver artifact is $driverConfiguration but this bundle is $Configuration."
    }
}
$installerConfigurationFile = Join-Path $InstallerArtifact "configuration.txt"
if (Test-Path -LiteralPath $installerConfigurationFile -PathType Leaf) {
    $installerConfiguration = (Get-Content -LiteralPath $installerConfigurationFile -Raw).Trim()
    if ($installerConfiguration -ne $Configuration) {
        throw "Installer artifact is $installerConfiguration but this bundle is $Configuration."
    }
}

$packageSource = Join-Path $RepoRoot "packaging\windows"
# Only the scripts the installer runs after extraction are embedded. The
# human-facing README is placed next to the final exe, not inside it, and the
# old Install-Helios.cmd launcher is gone now that the exe is the entry point.
foreach ($script in @("Install-Helios.ps1", "Uninstall-Helios.ps1", "Verify-Helios.ps1", "Helios-PackageCommon.ps1")) {
    Copy-Required (Join-Path $packageSource $script) (Join-Path $stagingRoot $script)
}
# The Rust skeleton is NOT copied into the payload; it is the template the
# packer appends the payload to at the end of this script.
$skeleton = Join-Path $InstallerArtifact "HeliosSetup.exe"
if (-not (Test-Path -LiteralPath $skeleton -PathType Leaf)) {
    throw "The installer skeleton is missing: $skeleton"
}

$driverOut = Join-Path $payload "driver"
foreach ($name in @("helios_kmd_render.inf", "helios_kmd_render.sys", "helios_umd.dll", "helios_umd12.dll", "helios_umd32.dll", "helios_umd12_32.dll", "toolchain.json")) {
    Copy-Required (Join-Path $DriverArtifact $name) (Join-Path $driverOut $name)
}
foreach ($name in @("helios_kmd_render.sys", "helios_umd.dll", "helios_umd12.dll", "helios_umd32.dll", "helios_umd12_32.dll")) {
    $info = (Get-Item -LiteralPath (Join-Path $driverOut $name)).VersionInfo
    if ($info.FileVersion -ne $Version -or $info.ProductVersion -ne $Version -or
        $info.ProductName -ne $metadata.HELIOS_PRODUCT -or $info.CompanyName -ne $metadata.HELIOS_PUBLISHER) {
        throw "$name has stale version/branding resources. Rebuild all five driver images from this checkout."
    }
}
foreach ($name in @("helios_kmd_render.sys", "helios_umd.dll", "helios_umd12.dll")) {
    Assert-HeliosPeArchitecture (Join-Path $driverOut $name) x64
}
foreach ($name in @("helios_umd32.dll", "helios_umd12_32.dll")) {
    Assert-HeliosPeArchitecture (Join-Path $driverOut $name) x86
}
foreach ($optional in @("helios_kmd_render.pdb", "helios_kmd_render.map", "helios_umd.pdb", "helios_umd12.pdb", "helios_umd32.pdb", "helios_umd12_32.pdb")) {
    $source = Join-Path $DriverArtifact $optional
    if (Test-Path -LiteralPath $source -PathType Leaf) { Copy-Required $source (Join-Path $symbolsRoot $optional) }
}
$installerPdb = Join-Path $InstallerArtifact "HeliosSetup.pdb"
if (Test-Path -LiteralPath $installerPdb -PathType Leaf) { Copy-Required $installerPdb (Join-Path $symbolsRoot "HeliosSetup.pdb") }

$mesaOut = Join-Path $payload "mesa"
foreach ($name in @("vulkan_virtio.dll", "libgallium_wgl.dll")) {
    Copy-Required (Join-Path $MesaArtifact $name) (Join-Path $mesaOut $name)
}
foreach ($dependency in Get-ChildItem -LiteralPath $MesaArtifact -Filter "lib*.dll" -File) {
    if ($dependency.Name -eq "libgallium_wgl.dll") { continue }
    Copy-Required $dependency.FullName (Join-Path $mesaOut $dependency.Name)
}
$mesaX86Out = Join-Path $mesaOut "x86"
foreach ($name in @("vulkan_virtio.dll", "libgallium_wgl.dll")) {
    Copy-Required (Join-Path $MesaX86Artifact $name) (Join-Path $mesaX86Out $name)
}
foreach ($dependency in Get-ChildItem -LiteralPath $MesaX86Artifact -Filter "lib*.dll" -File) {
    if ($dependency.Name -eq "libgallium_wgl.dll") { continue }
    Copy-Required $dependency.FullName (Join-Path $mesaX86Out $dependency.Name)
}

$openClOut = Join-Path $payload "opencl"
Copy-Required (Join-Path $OpenClArtifact "clvk.dll") (Join-Path $openClOut "clvk.dll")
$clvkPdb = Join-Path $OpenClArtifact "clvk.pdb"
if (Test-Path -LiteralPath $clvkPdb -PathType Leaf) { Copy-Required $clvkPdb (Join-Path $symbolsRoot "clvk.pdb") }

$loadersOut = Join-Path $payload "loaders"
Copy-Required (Join-Path $LoadersArtifact "vulkan-1.dll") (Join-Path $loadersOut "vulkan-1.dll")
Copy-Required (Join-Path $LoadersArtifact "OpenCL.dll") (Join-Path $loadersOut "OpenCL.dll")
Copy-Required (Join-Path $LoadersArtifact "x86\vulkan-1.dll") (Join-Path $loadersOut "x86\vulkan-1.dll")
foreach ($probe in @(
    "vulkan-smoke.exe",
    "vulkan-wsi-probe.exe",
    "d3d11-smoke.exe",
    "d3d12-smoke.exe",
    "d3d12-clear.exe",
    "opengl-smoke.exe",
    "opencl-smoke.exe",
    "opencl-gl-sharing-smoke.exe"
)) {
    Copy-Required (Join-Path $LoadersArtifact "smoke\$probe") (Join-Path $payload "smoke\$probe")
}
foreach ($probe in @("vulkan-smoke.exe", "vulkan-wsi-probe.exe", "opengl-smoke.exe", "d3d11-smoke.exe", "d3d12-smoke.exe", "d3d12-clear.exe")) {
    Copy-Required (Join-Path $LoadersArtifact "smoke\x86\$probe") (Join-Path $payload "smoke\x86\$probe")
}

foreach ($probe in Get-ChildItem -LiteralPath (Join-Path $payload "smoke") -Filter "*.exe" -File -Recurse) {
    $architecture = if ($probe.Directory.Name -eq "x86") { "x86" } else { "x64" }
    Assert-HeliosPeArchitecture $probe.FullName $architecture
}

$resolveCompatibilityOut = Join-Path $stagingRoot "compatibility\DaVinci Resolve"
foreach ($name in @(
    "atiadlxx.dll",
    "Resolve-CompatibilityCommon.ps1",
    "Install-Resolve-Compatibility.ps1",
    "Uninstall-Resolve-Compatibility.ps1",
    "README.md"
)) {
    Copy-Required (Join-Path $CompatibilityArtifact $name) (Join-Path $resolveCompatibilityOut $name)
}

# No VC++ redistributables are shipped: both UMDs are built with the static CRT
# (vkd3d `-Db_vscrt=mt` + crt-static), asserted in Build-Driver.ps1.

$licenseOut = Join-Path $stagingRoot "licenses"
foreach ($artifact in @($DriverArtifact, $MesaArtifact, $MesaX86Artifact, $OpenClArtifact, $LoadersArtifact, $CompatibilityArtifact)) {
    $artifactLicenses = Join-Path $artifact "licenses"
    if (Test-Path -LiteralPath $artifactLicenses -PathType Container) {
        New-Item -ItemType Directory -Force -Path $licenseOut | Out-Null
        Copy-Item -Path (Join-Path $artifactLicenses "*") -Destination $licenseOut -Recurse -Force
    }
}

$inf2Cat = Find-WindowsKitTool "Inf2Cat.exe"
$signTool = Find-WindowsKitTool "signtool.exe"
$catalog = Join-Path $driverOut "helios_kmd_render.cat"
Remove-Item -LiteralPath $catalog -Force -ErrorAction SilentlyContinue

$subject = "CN=$($metadata.HELIOS_PUBLISHER) $($metadata.HELIOS_PRODUCT) GitHub CI Test Signing $shortCommit"
$certificate = New-SelfSignedCertificate `
    -Type CodeSigningCert `
    -Subject $subject `
    -CertStoreLocation "Cert:\CurrentUser\My" `
    -KeyAlgorithm RSA `
    -KeyLength 3072 `
    -HashAlgorithm SHA256 `
    -KeyExportPolicy NonExportable `
    -NotAfter ([DateTime]::UtcNow.AddYears(2))
try {
    $certificateOut = Join-Path $stagingRoot "certificate\helios-ci-test.cer"
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $certificateOut) | Out-Null
    Export-Certificate -Cert $certificate -FilePath $certificateOut -Type CERT | Out-Null

    # The catalog hashes the SYS and all four UMDs. Sign those first, generate the
    # catalog over the final bytes, and sign the catalog last.
    Invoke-SignTool $signTool $certificate.Thumbprint (Join-Path $driverOut "helios_kmd_render.sys")
    Invoke-SignTool $signTool $certificate.Thumbprint (Join-Path $driverOut "helios_umd.dll")
    Invoke-SignTool $signTool $certificate.Thumbprint (Join-Path $driverOut "helios_umd12.dll")
    Invoke-SignTool $signTool $certificate.Thumbprint (Join-Path $driverOut "helios_umd32.dll")
    Invoke-SignTool $signTool $certificate.Thumbprint (Join-Path $driverOut "helios_umd12_32.dll")
    & $inf2Cat "/driver:$driverOut" "/os:10_X64" /uselocaltime
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $catalog -PathType Leaf)) {
        throw "Inf2Cat failed to produce the Helios catalog."
    }
    Invoke-SignTool $signTool $certificate.Thumbprint $catalog

    $signable = @(
        (Join-Path $openClOut "clvk.dll"),
        (Join-Path $loadersOut "vulkan-1.dll"),
        (Join-Path $loadersOut "x86\vulkan-1.dll"),
        (Join-Path $loadersOut "OpenCL.dll"),
        (Join-Path $resolveCompatibilityOut "atiadlxx.dll")
    )
    $signable += @(Get-ChildItem -LiteralPath $mesaOut -Filter "*.dll" -File -Recurse | ForEach-Object FullName)
    $signable += @(Get-ChildItem -LiteralPath (Join-Path $payload "smoke") -Filter "*.exe" -File -Recurse | ForEach-Object FullName)
    foreach ($file in $signable) { Invoke-SignTool $signTool $certificate.Thumbprint $file }
} finally {
    Remove-Item -LiteralPath "Cert:\CurrentUser\My\$($certificate.Thumbprint)" -Force -ErrorAction SilentlyContinue
}

$files = @()
foreach ($file in Get-ChildItem -LiteralPath $stagingRoot -File -Recurse | Where-Object { $_.Name -ne "manifest.json" } | Sort-Object FullName) {
    $relative = $file.FullName.Substring($stagingRoot.Length + 1).Replace("\", "/")
    if ($relative -notlike "payload/*" -and $relative -notlike "certificate/*" -and $relative -notlike "compatibility/*") { continue }
    $files += [ordered]@{
        path = $relative
        size = $file.Length
        sha256 = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToUpperInvariant()
    }
}

$manifest = [ordered]@{
    schemaVersion = 1
    productName = $metadata.HELIOS_PRODUCT
    publisher = $metadata.HELIOS_PUBLISHER
    packageId = $packageId
    version = $Version
    architecture = "x64"
    configuration = $Configuration
    applicationArchitectures = @("x64", "x86")
    createdAtUtc = [DateTime]::UtcNow.ToString("o")
    source = [ordered]@{
        helios = $RepositoryCommit
        mesa = $MesaCommit
        dxvk = $DxvkCommit
        vkd3d = $Vkd3dCommit
        clvk = $ClvkCommit
        vulkanLoader = $VulkanLoaderCommit
        vulkanHeaders = $VulkanHeadersCommit
        openClLoader = $OpenClLoaderCommit
        openClHeaders = $OpenClHeadersCommit
    }
    signing = [ordered]@{
        mode = "test"
        subject = $subject
        thumbprint = $certificate.Thumbprint
        certificate = "certificate/helios-ci-test.cer"
    }
    components = [ordered]@{
        driver = [ordered]@{
            version = $Version
            direct3D = "DXVK D3D11 and vkd3d-proton D3D12 embedded WDDM UMDs"
            direct3D12DefaultEnabled = $true
            architectures = @("x64", "x86")
        }
        mesa = [ordered]@{ vulkan = "Venus"; openGL = "Zink WGL ICD"; architectures = @("x64", "x86"); vulkanApiVersion = "1.4.352" }
        openCl = [ordered]@{ implementation = "CLVK"; onlineCompiler = $true; architectures = @("x64") }
        compatibility = [ordered]@{ davinciResolve = "App-local AMD ADL detection shim" }
    }
    files = $files
}
$manifest | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $stagingRoot "manifest.json") -Encoding UTF8

# Finalize the self-contained installer. The Rust packer appends the whole
# payload folder (scripts, driver, Mesa, CLVK, loaders, certificate, manifest)
# to the skeleton, so the shipped artifact is one HeliosSetup.exe. The packer is
# a GUI-subsystem exe, so it must be waited on explicitly.
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$finalDir = Join-Path $OutputDir $packageId
if (Test-Path -LiteralPath $finalDir) { Remove-Item -LiteralPath $finalDir -Recurse -Force }
New-Item -ItemType Directory -Force -Path $finalDir | Out-Null
$selfContained = Join-Path $finalDir "HeliosSetup.exe"
$pack = Start-Process -FilePath $skeleton -ArgumentList @("--bundle", $stagingRoot, $selfContained) -Wait -PassThru
if ($pack.ExitCode -ne 0) {
    throw "The installer packer failed with exit code $($pack.ExitCode)."
}
if (-not (Test-Path -LiteralPath $selfContained -PathType Leaf)) {
    throw "The self-contained installer was not produced at $selfContained."
}
Copy-Item -LiteralPath (Join-Path $packageSource "README.md") -Destination $finalDir -Force

$zipPath = Join-Path $OutputDir "$packageId.zip"
Remove-Item -LiteralPath $zipPath -Force -ErrorAction SilentlyContinue
Compress-Archive -LiteralPath $finalDir -DestinationPath $zipPath -CompressionLevel Optimal
$zipHash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
Set-Content -LiteralPath "$zipPath.sha256" -Value "$zipHash  $([IO.Path]::GetFileName($zipPath))" -Encoding ascii
Write-Host "Package: $zipPath"
Write-Host "SHA256: $zipHash"

# Separate symbols archive, produced only when the build actually emitted
# symbols. It is uploaded alongside the installer and, on a tagged release,
# published as its own asset.
if (Test-Path -LiteralPath $symbolsRoot -PathType Container) {
    $symbolsZip = Join-Path $OutputDir "$packageId-symbols.zip"
    Remove-Item -LiteralPath $symbolsZip -Force -ErrorAction SilentlyContinue
    Compress-Archive -LiteralPath $symbolsRoot -DestinationPath $symbolsZip -CompressionLevel Optimal
    $symbolsHash = (Get-FileHash -LiteralPath $symbolsZip -Algorithm SHA256).Hash.ToLowerInvariant()
    Set-Content -LiteralPath "$symbolsZip.sha256" -Value "$symbolsHash  $([IO.Path]::GetFileName($symbolsZip))" -Encoding ascii
    Write-Host "Symbols: $symbolsZip"
}
