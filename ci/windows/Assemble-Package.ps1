param(
    [Parameter(Mandatory)][string]$RepoRoot,
    [Parameter(Mandatory)][string]$DriverArtifact,
    [Parameter(Mandatory)][string]$MesaArtifact,
    [Parameter(Mandatory)][string]$OpenClArtifact,
    [Parameter(Mandatory)][string]$LoadersArtifact,
    [Parameter(Mandatory)][string]$CompatibilityArtifact,
    [Parameter(Mandatory)][string]$OutputDir,
    [Parameter(Mandatory)][string]$Version,
    [Parameter(Mandatory)][string]$RepositoryCommit,
    [Parameter(Mandatory)][string]$MesaCommit,
    [Parameter(Mandatory)][string]$DxvkCommit,
    [Parameter(Mandatory)][string]$ClvkCommit,
    [Parameter(Mandatory)][string]$VulkanLoaderCommit,
    [Parameter(Mandatory)][string]$VulkanHeadersCommit,
    [Parameter(Mandatory)][string]$OpenClLoaderCommit,
    [Parameter(Mandatory)][string]$OpenClHeadersCommit
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Initialize-HeliosBuild.ps1")
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

function Get-OpenClExports([string]$Path) {
    $table = (& dumpbin.exe /nologo /exports $Path 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) { throw "dumpbin.exe failed to inspect $Path." }
    return @([Regex]::Matches($table, '(?m)^\s+\d+\s+[0-9A-F]+\s+(?:[0-9A-F]+\s+)?(cl[A-Za-z0-9_]+)(?:\s|$)') |
        ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique)
}

$shortCommit = $RepositoryCommit.Substring(0, 8)
$packageId = "helios-windows-x64-$Version-$shortCommit"
$stagingRoot = Join-Path $OutputDir $packageId
$payload = Join-Path $stagingRoot "payload"
if (Test-Path -LiteralPath $stagingRoot) { Remove-Item -LiteralPath $stagingRoot -Recurse -Force }
New-Item -ItemType Directory -Force -Path $payload | Out-Null

$packageSource = Join-Path $RepoRoot "packaging\windows"
foreach ($script in @("Install-Helios.cmd", "Install-Helios.ps1", "Uninstall-Helios.ps1", "Verify-Helios.ps1", "Helios-PackageCommon.ps1", "README.md")) {
    Copy-Required (Join-Path $packageSource $script) (Join-Path $stagingRoot $script)
}

$driverOut = Join-Path $payload "driver"
foreach ($name in @("helios_kmd_render.inf", "helios_kmd_render.sys", "helios_umd.dll")) {
    Copy-Required (Join-Path $DriverArtifact $name) (Join-Path $driverOut $name)
}
foreach ($optional in @("helios_kmd_render.pdb", "helios_kmd_render.map", "helios_umd.pdb")) {
    $source = Join-Path $DriverArtifact $optional
    if (Test-Path -LiteralPath $source -PathType Leaf) { Copy-Required $source (Join-Path $driverOut $optional) }
}

$mesaOut = Join-Path $payload "mesa"
foreach ($name in @("vulkan_virtio.dll", "libgallium_wgl.dll")) {
    Copy-Required (Join-Path $MesaArtifact $name) (Join-Path $mesaOut $name)
}
foreach ($dependency in Get-ChildItem -LiteralPath $MesaArtifact -Filter "lib*.dll" -File) {
    if ($dependency.Name -eq "libgallium_wgl.dll") { continue }
    Copy-Required $dependency.FullName (Join-Path $mesaOut $dependency.Name)
}

$openClOut = Join-Path $payload "opencl"
Copy-Required (Join-Path $OpenClArtifact "clvk.dll") (Join-Path $openClOut "clvk.dll")
$clvkPdb = Join-Path $OpenClArtifact "clvk.pdb"
if (Test-Path -LiteralPath $clvkPdb -PathType Leaf) { Copy-Required $clvkPdb (Join-Path $openClOut "clvk.pdb") }

$loadersOut = Join-Path $payload "loaders"
Copy-Required (Join-Path $LoadersArtifact "vulkan-1.dll") (Join-Path $loadersOut "vulkan-1.dll")
Copy-Required (Join-Path $LoadersArtifact "OpenCL.dll") (Join-Path $loadersOut "OpenCL.dll")
foreach ($probe in @(
    "vulkan-smoke.exe",
    "vulkan-wsi-probe.exe",
    "d3d11-smoke.exe",
    "opengl-smoke.exe",
    "opencl-smoke.exe",
    "opencl-gl-sharing-smoke.exe"
)) {
    Copy-Required (Join-Path $LoadersArtifact "smoke\$probe") (Join-Path $payload "smoke\$probe")
}

$resolveCompatibilityOut = Join-Path $stagingRoot "compatibility\DaVinci Resolve"
foreach ($name in @(
    "atiadlxx.dll",
    "OpenCL.dll",
    "Resolve-CompatibilityCommon.ps1",
    "Install-Resolve-Compatibility.ps1",
    "Uninstall-Resolve-Compatibility.ps1",
    "Launch Resolve (Helios).cmd",
    "README.md"
)) {
    Copy-Required (Join-Path $CompatibilityArtifact $name) (Join-Path $resolveCompatibilityOut $name)
}
Copy-Required (Join-Path $LoadersArtifact "OpenCL.dll") (Join-Path $resolveCompatibilityOut "OpenCL_real.dll")
$proxyExports = @(Get-OpenClExports (Join-Path $resolveCompatibilityOut "OpenCL.dll"))
$loaderExports = @(Get-OpenClExports (Join-Path $resolveCompatibilityOut "OpenCL_real.dll"))
if ($proxyExports.Count -ne $loaderExports.Count -or
    @(Compare-Object -ReferenceObject $loaderExports -DifferenceObject $proxyExports).Count -ne 0) {
    throw "The Resolve OpenCL proxy export set does not match the packaged Khronos loader."
}

$redist = Get-ChildItem -LiteralPath $env:VCToolsRedistDir -Filter "vc_redist.x64.exe" -File -Recurse | Select-Object -First 1
if (-not $redist) { throw "The Visual C++ x64 redistributable was not found below $env:VCToolsRedistDir." }
Copy-Required $redist.FullName (Join-Path $payload "prerequisites\vc_redist.x64.exe")

$licenseOut = Join-Path $stagingRoot "licenses"
foreach ($artifact in @($DriverArtifact, $MesaArtifact, $OpenClArtifact, $LoadersArtifact, $CompatibilityArtifact)) {
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

$subject = "CN=Helios GitHub CI Test Signing $shortCommit"
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

    # The catalog hashes the SYS and UMD. Sign those first, generate the
    # catalog over the final bytes, and sign the catalog last.
    Invoke-SignTool $signTool $certificate.Thumbprint (Join-Path $driverOut "helios_kmd_render.sys")
    Invoke-SignTool $signTool $certificate.Thumbprint (Join-Path $driverOut "helios_umd.dll")
    & $inf2Cat "/driver:$driverOut" "/os:10_X64" /uselocaltime
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $catalog -PathType Leaf)) {
        throw "Inf2Cat failed to produce the Helios catalog."
    }
    Invoke-SignTool $signTool $certificate.Thumbprint $catalog

    $signable = @(
        (Join-Path $openClOut "clvk.dll"),
        (Join-Path $loadersOut "vulkan-1.dll"),
        (Join-Path $loadersOut "OpenCL.dll"),
        (Join-Path $resolveCompatibilityOut "atiadlxx.dll"),
        (Join-Path $resolveCompatibilityOut "OpenCL.dll"),
        (Join-Path $resolveCompatibilityOut "OpenCL_real.dll")
    )
    $signable += @(Get-ChildItem -LiteralPath $mesaOut -Filter "*.dll" -File | ForEach-Object FullName)
    $signable += @(Get-ChildItem -LiteralPath (Join-Path $payload "smoke") -Filter "*.exe" -File | ForEach-Object FullName)
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
    packageId = $packageId
    version = $Version
    architecture = "x64"
    createdAtUtc = [DateTime]::UtcNow.ToString("o")
    source = [ordered]@{
        helios = $RepositoryCommit
        mesa = $MesaCommit
        dxvk = $DxvkCommit
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
        driver = [ordered]@{ version = $Version; direct3D = "DXVK embedded WDDM UMD" }
        mesa = [ordered]@{ vulkan = "Venus"; openGL = "Zink WGL ICD"; vulkanApiVersion = "1.4.352" }
        openCl = [ordered]@{ implementation = "CLVK"; onlineCompiler = $true }
        compatibility = [ordered]@{ davinciResolve = "App-local ADL detection and mixed-context OpenCL shims" }
    }
    files = $files
}
$manifest | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $stagingRoot "manifest.json") -Encoding UTF8

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$zipPath = Join-Path $OutputDir "$packageId.zip"
Remove-Item -LiteralPath $zipPath -Force -ErrorAction SilentlyContinue
Compress-Archive -LiteralPath $stagingRoot -DestinationPath $zipPath -CompressionLevel Optimal
$zipHash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
Set-Content -LiteralPath "$zipPath.sha256" -Value "$zipHash  $([IO.Path]::GetFileName($zipPath))" -Encoding ascii
Write-Host "Package: $zipPath"
Write-Host "SHA256: $zipHash"
