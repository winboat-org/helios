param([Parameter(Mandatory)][string]$Specification)
. (Join-Path $env:WINBOAT_CONTROL_ROOT 'Control.ps1')
$spec = Read-ControlJson $Specification
$repo = Join-Path $spec.sourceRoot $spec.sources.helios.relativePath
[void](Assert-ControlPath $repo 'C:\WinBoatDev\src')
$cargo = @($spec.prerequisites | Where-Object kind -eq 'cargo')
if ($cargo.Count -ne 1) { throw 'Exactly one verified offline Cargo closure is required' }
$vendor = $cargo[0].root
foreach ($crate in @('kmd_render','umd','umd12')) {
    if ((Get-FileHash (Join-Path $repo "$crate\Cargo.lock") -Algorithm SHA256).Hash -ne
        (Get-FileHash (Join-Path $vendor "manifests\$crate.lock") -Algorithm SHA256).Hash) {
        throw "Offline dependency lock differs from the mirrored $crate source"
    }
}
$env:CARGO_HOME = Join-Path $spec.buildRoot 'cargo-home'
$env:CARGO_NET_OFFLINE = 'true'
New-Item -ItemType Directory -Path $env:CARGO_HOME -Force | Out-Null
$env:HELIOS_DXVK_SRC = Join-Path $spec.sourceRoot $spec.sources.dxvk.relativePath
$env:HELIOS_CLANG_CL = 'C:\WinBoatDev\tools\LLVM\bin\clang-cl.exe'
$env:HELIOS_MSVC_LIB = 'C:\WinBoatDev\tools\LLVM\bin\llvm-lib.exe'
$env:HELIOS_WDK_INCLUDE = 'C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0'
$profile = if ($spec.configuration -eq 'debug') {'dev'} else {'release'}
$profileDirectory = if ($profile -eq 'dev') {'debug'} else {$profile}
$package = Join-Path $spec.buildRoot 'package'
New-Item -ItemType Directory -Path $package -Force | Out-Null
$kitRoot = 'C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0'
$kit = Join-Path $kitRoot 'x64'
# The matched WDK supplies Inf2Cat as an x86 host utility even for amd64
# packages. Check every packaging tool before starting the expensive build.
$inf2cat = Join-Path $kitRoot 'x86\Inf2Cat.exe'
if ($spec.architecture -eq 'x64') {
    foreach ($tool in @($inf2cat,(Join-Path $kit 'stampinf.exe'),(Join-Path $kit 'signtool.exe'))) {
        if (-not (Test-Path -LiteralPath $tool -PathType Leaf)) { throw "Matched kit packaging tool is missing: $tool" }
    }
}
& python.exe (Join-Path $repo 'tools\sync-metadata.py') --check
if ($LASTEXITCODE) { exit $LASTEXITCODE }
function Build-RustCrate([string]$Crate, [string]$Architecture) {
    Import-ControlBuildEnvironment $Architecture
    $env:HELIOS_MSVC_INCLUDE = Join-Path $env:VCToolsInstallDir 'include'
    $env:CARGO_TARGET_DIR = Join-Path $spec.buildRoot "cargo\$Crate-$Architecture"
    $crateRoot=$repo
    if($Crate -eq 'kmd_render') {
        # The pinned wdk-build discovers the top manifest by walking OUT_DIR
        # ancestors to Cargo.lock. Keep its ordinary target layout inside an
        # owned local source copy, preserving the verified immutable mirror.
        $crateRoot=Join-Path $spec.buildRoot 'cargo\kmd-source'
        New-Item -ItemType Directory -Path $crateRoot -Force | Out-Null
        foreach($directory in @('kmd_render','kmd_logic','protocol','metadata')) {
            Copy-Item -LiteralPath (Join-Path $repo $directory) -Destination $crateRoot -Recurse
        }
        $env:CARGO_TARGET_DIR=Join-Path $crateRoot 'kmd_render\target'
    }
    $configuration = (Get-Content -Raw (Join-Path $vendor "manifests\$Crate.config.toml")).Replace('directory = "cargo-vendor-dir"',
        ('directory = "' + (Join-Path $vendor 'vendor').Replace('\','/') + '"'))
    [IO.File]::WriteAllText((Join-Path $env:CARGO_HOME 'config.toml'), $configuration)
    $arguments = @('build','--locked','--offline','--profile',$profile,'--manifest-path',(Join-Path $crateRoot "$Crate\Cargo.toml"))
    $target = if ($Architecture -eq 'x86') {'i686-pc-windows-msvc'} else {'x86_64-pc-windows-msvc'}
    $arguments += @('--target',$target)
    Push-Location (Join-Path $crateRoot $Crate)
    try { & cargo.exe @arguments; if ($LASTEXITCODE) { exit $LASTEXITCODE } } finally { Pop-Location }
    return Join-Path $env:CARGO_TARGET_DIR "$target\$profileDirectory"
}
$architectures = if ($spec.architecture -eq 'x86') {@('x86')} else {@('x64','x86')}
foreach ($architecture in $architectures) {
    foreach ($target in @('dxvk','vkd3d')) {
        $engine = @($spec.componentDependencies | Where-Object target -eq "$target-engine-$architecture")
        if ($engine.Count -ne 1) { throw "Missing exact $target $architecture engine" }
        foreach ($file in $engine[0].files) { Assert-ControlFile (Join-Path $engine[0].root $file.path) $file.sha256 $file.size }
        [Environment]::SetEnvironmentVariable($(if ($target -eq 'dxvk') {'HELIOS_DXVK_BUILD'} else {'HELIOS_VKD3D_BUILD'}),$engine[0].root,'Process')
    }
    foreach ($crate in @('umd','umd12')) {
        $directory = Build-RustCrate $crate $architecture
        $base = if ($crate -eq 'umd') {'helios_umd'} else {'helios_umd12'}
        $name = $base + $(if ($architecture -eq 'x86') {$(if($crate -eq 'umd') {'32'} else {'_32'})} else {''}) + '.dll'
        Copy-Item -LiteralPath (Join-Path $directory "$base.dll") -Destination (Join-Path $package $name)
    }
}
if ($spec.architecture -eq 'x64') {
    $directory = Build-RustCrate 'kmd_render' 'x64'
    # Preserve the existing package gate when bypassing cargo-make's dynamic
    # rust-script downloads. Comments are stripped exactly as its scanner does.
    foreach ($file in Get-ChildItem (Join-Path $repo 'kmd_render\src') -Recurse -File -Filter '*.rs') {
        foreach ($line in [IO.File]::ReadLines($file.FullName)) {
            $code=($line -split '//',2)[0]
            if ($code.Contains('.unwrap()') -or $code.Contains('.expect(')) {throw "KMD package gate: panic call site in $($file.FullName): $line"}
        }
    }
    Copy-Item -LiteralPath (Join-Path $directory 'helios_kmd_render.dll') -Destination (Join-Path $package 'helios_kmd_render.sys')
    Copy-Item -LiteralPath (Join-Path $repo 'kmd_render\helios_kmd_render.inx') -Destination (Join-Path $package 'helios_kmd_render.inf')
    . (Join-Path $repo 'metadata\Read-HeliosMetadata.ps1')
    $metadata = Read-HeliosMetadata $repo
    & (Join-Path $kit 'stampinf.exe') -f (Join-Path $package 'helios_kmd_render.inf') -d '*' -a amd64 -c helios_kmd_render.cat -v $metadata.HELIOS_KMD_VERSION
    if ($LASTEXITCODE) { exit $LASTEXITCODE }
    $inventory = Read-ControlJson 'C:\ProgramData\WinBoatDev\provisioning.json'
    $certificate = Get-Item ('Cert:\LocalMachine\My\' + $inventory.certificateThumbprint)
    Export-Certificate -Cert $certificate -FilePath (Join-Path $package 'helios-dev-test.cer') | Out-Null
    & (Join-Path $kit 'signtool.exe') sign /fd SHA256 /sm /sha1 $certificate.Thumbprint (Join-Path $package 'helios_kmd_render.sys')
    if ($LASTEXITCODE) { exit $LASTEXITCODE }
    & $inf2cat "/driver:$package" /os:10_x64 /uselocaltime
    if ($LASTEXITCODE) { exit $LASTEXITCODE }
    & (Join-Path $kit 'signtool.exe') sign /fd SHA256 /sm /sha1 $certificate.Thumbprint (Join-Path $package 'helios_kmd_render.cat')
    if ($LASTEXITCODE) { exit $LASTEXITCODE }
    $version=(Get-Item (Join-Path $package 'helios_kmd_render.sys')).VersionInfo.FileVersion
    if ($version.Trim() -ne $metadata.HELIOS_KMD_VERSION) { throw 'SYS resource version differs from INF metadata' }
}
