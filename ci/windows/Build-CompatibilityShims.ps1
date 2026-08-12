param(
    [Parameter(Mandatory)][string]$RepoRoot,
    [Parameter(Mandatory)][string]$OutputDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Initialize-HeliosBuild.ps1")
Import-VisualStudioEnvironment

$compatibilityRoot = Join-Path $RepoRoot "packaging\windows\compat"
$adlRoot = Join-Path $compatibilityRoot "adl-shim"
$adlSource = Join-Path $adlRoot "helios-adl-shim.cpp"
$adlDefinition = Join-Path $adlRoot "helios-adl-shim.def"
$proxyRoot = Join-Path $compatibilityRoot "resolve-opencl-proxy"
$proxySource = Join-Path $proxyRoot "helios-resolve-opencl-proxy.c"
$proxyTestSource = Join-Path $proxyRoot "resolve-opencl-properties-test.c"
$lifecycleTest = Join-Path $proxyRoot "Test-Resolve-CompatibilityLifecycle.ps1"
$proxyForwarders = Join-Path $proxyRoot "opencl-forwarders.inc"
$readme = Join-Path $compatibilityRoot "README.md"
$lifecycleFiles = @(
    "Resolve-CompatibilityCommon.ps1",
    "Install-Resolve-Compatibility.ps1",
    "Uninstall-Resolve-Compatibility.ps1",
    "Launch Resolve (Helios).cmd"
)
$required = @($adlSource, $adlDefinition, $proxySource, $proxyTestSource, $proxyForwarders, $lifecycleTest, $readme)
$required += @($lifecycleFiles | ForEach-Object { Join-Path $proxyRoot $_ })
foreach ($path in $required) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Required compatibility-shim source is missing: $path"
    }
}

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$buildDir = Join-Path ([IO.Path]::GetTempPath()) ("helios-compatibility-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $buildDir | Out-Null

try {
    $adlObject = Join-Path $buildDir "helios-adl-shim.obj"
    $adlDll = Join-Path $OutputDir "atiadlxx.dll"
    $adlImportLibrary = Join-Path $buildDir "atiadlxx.lib"

    $compileArguments = @(
        "/nologo",
        "/c",
        "/std:c++17",
        "/O2",
        "/W4",
        "/WX",
        "/EHsc",
        "/MT",
        "/GS",
        "/guard:cf",
        "/Brepro",
        "/Fo$adlObject",
        $adlSource
    )
    & cl.exe @compileArguments
    if ($LASTEXITCODE -ne 0) { throw "cl.exe failed to compile the ADL compatibility shim." }

    $linkArguments = @(
        "/nologo",
        "/DLL",
        "/MACHINE:X64",
        "/OPT:REF",
        "/OPT:ICF",
        "/DYNAMICBASE",
        "/NXCOMPAT",
        "/HIGHENTROPYVA",
        "/GUARD:CF",
        "/Brepro",
        "/OUT:$adlDll",
        "/IMPLIB:$adlImportLibrary",
        "/DEF:$adlDefinition",
        $adlObject,
        "setupapi.lib",
        "user32.lib",
        "uuid.lib"
    )
    & link.exe @linkArguments
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $adlDll -PathType Leaf)) {
        throw "link.exe failed to produce atiadlxx.dll."
    }

    $exportTable = (& dumpbin.exe /nologo /exports $adlDll 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) { throw "dumpbin.exe failed to inspect atiadlxx.dll." }
    $requiredExports = @(
        "ADL_Main_Control_Create",
        "ADL_Main_Control_Destroy",
        "ADL_Adapter_NumberOfAdapters_Get",
        "ADL_Adapter_AdapterInfo_Get",
        "ADL_Adapter_ASICFamilyType_Get",
        "ADL_Graphics_Versions_Get",
        "ADL_Adapter_Primary_Get",
        "ADL2_Main_Control_Create",
        "ADL2_Main_Control_Destroy",
        "ADL2_Main_Control_GetProcAddress",
        "ADL2_Graphics_VersionsX2_Get"
    )
    foreach ($export in $requiredExports) {
        if ($exportTable -notmatch "(?m)\b$([Regex]::Escape($export))\b") {
            throw "atiadlxx.dll is missing required export $export."
        }
    }

    $proxyObject = Join-Path $buildDir "helios-resolve-opencl-proxy.obj"
    $proxyDll = Join-Path $OutputDir "OpenCL.dll"
    $proxyImportLibrary = Join-Path $buildDir "OpenCL.lib"
    $proxyCompileArguments = @(
        "/nologo", "/c", "/TC", "/std:c11", "/O2", "/W4", "/WX",
        "/MT", "/GS", "/guard:cf", "/Brepro", "/Fo$proxyObject", $proxySource
    )
    & cl.exe @proxyCompileArguments
    if ($LASTEXITCODE -ne 0) { throw "cl.exe failed to compile the Resolve OpenCL proxy." }

    $proxyLinkArguments = @(
        "/nologo", "/DLL", "/MACHINE:X64", "/OPT:REF", "/OPT:ICF",
        "/DYNAMICBASE", "/NXCOMPAT", "/HIGHENTROPYVA", "/GUARD:CF", "/Brepro",
        "/OUT:$proxyDll", "/IMPLIB:$proxyImportLibrary", $proxyObject
    )
    & link.exe @proxyLinkArguments
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $proxyDll -PathType Leaf)) {
        throw "link.exe failed to produce the Resolve OpenCL proxy."
    }

    $declaredExports = @(
        Select-String -LiteralPath $proxyForwarders -Pattern '/export:(cl[A-Za-z0-9_]+)=' -AllMatches |
            ForEach-Object { $_.Matches } | ForEach-Object { $_.Groups[1].Value }
    ) + @("clCreateContext")
    $declaredExports = @($declaredExports | Sort-Object -Unique)
    if ($declaredExports.Count -ne 124) {
        throw "The Resolve OpenCL proxy declares $($declaredExports.Count) exports; expected 124 for the pinned loader."
    }
    $proxyExportTable = (& dumpbin.exe /nologo /exports $proxyDll 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) { throw "dumpbin.exe failed to inspect the Resolve OpenCL proxy." }
    foreach ($export in $declaredExports) {
        if ($proxyExportTable -notmatch "(?m)\b$([Regex]::Escape($export))\b") {
            throw "Resolve OpenCL proxy is missing declared export $export."
        }
    }

    $testObject = Join-Path $buildDir "resolve-opencl-properties-test.obj"
    $testExe = Join-Path $buildDir "resolve-opencl-properties-test.exe"
    & cl.exe /nologo /c /TC /std:c11 /Od /W4 /WX /MT /GS "/Fo$testObject" $proxyTestSource
    if ($LASTEXITCODE -ne 0) { throw "cl.exe failed to compile the Resolve OpenCL property tests." }
    & link.exe /nologo /MACHINE:X64 /DYNAMICBASE /NXCOMPAT /OUT:$testExe $testObject
    if ($LASTEXITCODE -ne 0) { throw "link.exe failed to produce the Resolve OpenCL property tests." }
    & $testExe
    if ($LASTEXITCODE -ne 0) { throw "Resolve OpenCL property tests failed." }

    foreach ($scriptPath in @(
        @($lifecycleFiles | Where-Object { $_ -like "*.ps1" } |
            ForEach-Object { Join-Path $proxyRoot $_ }) + $lifecycleTest
    )) {
        $tokens = $null
        $parseErrors = $null
        [void][Management.Automation.Language.Parser]::ParseFile(
            $scriptPath, [ref]$tokens, [ref]$parseErrors)
        if ($parseErrors.Count -ne 0) {
            throw "PowerShell syntax validation failed for $scriptPath`: $($parseErrors[0].Message)"
        }
    }

    Copy-Item -LiteralPath $readme -Destination (Join-Path $OutputDir "README.md") -Force
    foreach ($file in $lifecycleFiles) {
        Copy-Item -LiteralPath (Join-Path $proxyRoot $file) -Destination (Join-Path $OutputDir $file) -Force
    }
    & $lifecycleTest -ArtifactDirectory $OutputDir
    Write-Host "Compatibility-shim artifact staged at $OutputDir"
} finally {
    Remove-Item -LiteralPath $buildDir -Recurse -Force -ErrorAction SilentlyContinue
}
