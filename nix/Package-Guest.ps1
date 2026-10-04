param([Parameter(Mandatory)][string]$Specification)
. (Join-Path $env:WINBOAT_CONTROL_ROOT 'Control.ps1')
$spec=Read-ControlJson $Specification
if($spec.symbolStorage -ne 'component-artifacts') {throw 'Package requires the shared component-artifact symbol policy'}
$repository=Join-Path $spec.sourceRoot $spec.sources.helios.relativePath
. (Join-Path $repository 'metadata\Read-HeliosMetadata.ps1')
$metadata=Read-HeliosMetadata $repository
$bundle=Join-Path $spec.buildRoot 'bundle'
New-Item -ItemType Directory -Path $bundle | Out-Null
$artifacts=@{}
foreach($artifact in $spec.componentDependencies) {
    # The shared GuestBuild wrapper verifies the dependency trees and bytes
    # consumed here. Symbols remain in their already exported component builds.
    if($artifacts.ContainsKey($artifact.target)) {throw 'Duplicate component artifact'}
    $artifacts[$artifact.target]=$artifact
}
foreach($target in @('helios-guest-x64','mesa-guest-x64','mesa-guest-x86','clvk-helios')) {
    if(-not $artifacts.ContainsKey($target)) {throw "Required component artifact missing: $target"}
}
function Copy-BundleFile([string]$InputPath,[string]$Relative) {
    $destination=Assert-ControlPath (Join-Path $bundle $Relative) $bundle
    New-Item -ItemType Directory -Path (Split-Path $destination -Parent) -Force | Out-Null
    Copy-Item -LiteralPath $InputPath -Destination $destination
}
foreach($script in @('Install-Helios.ps1','Uninstall-Helios.ps1','Verify-Helios.ps1','Helios-PackageCommon.ps1')) {
    Copy-BundleFile (Join-Path $repository "packaging\windows\$script") $script
}
$driver=$artifacts['helios-guest-x64']
foreach($name in @('helios_kmd_render.sys','helios_kmd_render.inf','helios_kmd_render.cat','helios_umd.dll','helios_umd12.dll','helios_umd32.dll','helios_umd12_32.dll')) {
    Copy-BundleFile (Join-Path $driver.root "package\$name") "payload\driver\$name"
}
Copy-BundleFile (Join-Path $driver.root 'package\helios-dev-test.cer') 'certificate\helios-dev-test.cer'
Copy-BundleFile (Join-Path $driver.root 'toolchain.json') 'payload\driver\toolchain.json'
foreach($architecture in @('x64','x86')) {
    $mesa=$artifacts["mesa-guest-$architecture"]
    $relative=if($architecture -eq 'x86') {'payload\mesa\x86'} else {'payload\mesa'}
    Copy-BundleFile (Join-Path $mesa.root 'mesa\src\virtio\vulkan\vulkan_virtio.dll') "$relative\vulkan_virtio.dll"
    Copy-BundleFile (Join-Path $mesa.root 'mesa\src\gallium\targets\wgl\libgallium_wgl.dll') "$relative\libgallium_wgl.dll"
    Copy-BundleFile (Join-Path $mesa.root 'mesa\src\gallium\targets\libgl-gdi\opengl32.dll') "$relative\opengl32.dll"
}
$opencl=$artifacts['clvk-helios']
Copy-BundleFile (Join-Path $opencl.root 'package\clvk.dll') 'payload\opencl\clvk.dll'
foreach($name in @('vulkan-1.dll','OpenCL.dll','x86\vulkan-1.dll')) {
    Copy-BundleFile (Join-Path $opencl.root "package\$name") "payload\loaders\$name"
}
Get-ChildItem (Join-Path $opencl.root 'package\smoke') -Recurse -File -Filter '*.exe' | ForEach-Object {
    Copy-BundleFile $_.FullName ('payload\smoke\'+$_.FullName.Substring((Join-Path $opencl.root 'package\smoke').Length+1))
}
foreach($artifact in $spec.componentDependencies) {
    Get-ChildItem (Join-Path $artifact.root 'licenses') -Recurse -File | ForEach-Object {
        Copy-BundleFile $_.FullName ('licenses\'+$artifact.target+'\'+$_.FullName.Substring((Join-Path $artifact.root 'licenses').Length+1))
    }
}
$certificate=[Security.Cryptography.X509Certificates.X509Certificate2]::new((Join-Path $bundle 'certificate\helios-dev-test.cer'))
$fixed=Read-ControlJson (Join-Path $opencl.root 'package\source-revisions.json')
$source=@{helios=$driver.sources.helios.revision;mesa=$artifacts['mesa-guest-x64'].sources.'mesa-helios'.revision;
    dxvk=$driver.sources.dxvk.revision;vkd3d=$driver.sources.'vkd3d-proton'.revision;clvk=$opencl.sources.'clvk-helios'.revision;
    vulkanLoader=$fixed.vulkanLoader;vulkanHeaders=$fixed.vulkanHeaders;openClLoader=$fixed.openClLoader;openClHeaders=$fixed.openClHeaders}
$manifest=@{schemaVersion=1;productName=$metadata.HELIOS_PRODUCT;publisher=$metadata.HELIOS_PUBLISHER;
    packageId=('helios-development-'+$spec.operationId);version=$metadata.HELIOS_KMD_VERSION;architecture='x64';
    configuration=$spec.configuration;applicationArchitectures=@('x64','x86');createdAtUtc=[DateTime]::UtcNow.ToString('o');source=$source;
    signing=@{mode='test';subject=$certificate.Subject;thumbprint=$certificate.Thumbprint;certificate='certificate/helios-dev-test.cer'};
    components=@{driver=@{version=$metadata.HELIOS_KMD_VERSION;architectures=@('x64','x86')};
        mesa=@{vulkan='Venus';openGL='Zink WGL ICD';architectures=@('x64','x86');vulkanApiVersion='1.4.352'};
        openCl=@{implementation='CLVK';onlineCompiler=$true;architectures=@('x64')}};
    symbolStorage='component-artifacts';artifacts=@($spec.componentDependencies);files=@()}
$manifest.files=@(Get-ChildItem $bundle -Recurse -File | Sort-Object FullName | ForEach-Object {
    @{path=$_.FullName.Substring($bundle.Length+1).Replace('\','/');sha256=(Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower();size=$_.Length}
})
Write-ControlJson $manifest (Join-Path $bundle 'manifest.json')
