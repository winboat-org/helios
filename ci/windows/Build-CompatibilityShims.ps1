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
$resolveRoot = Join-Path $compatibilityRoot "resolve-compatibility"
$lifecycleTest = Join-Path $resolveRoot "Test-Resolve-CompatibilityLifecycle.ps1"
$readme = Join-Path $compatibilityRoot "README.md"
$lifecycleFiles = @(
    "Resolve-CompatibilityCommon.ps1",
    "Install-Resolve-Compatibility.ps1",
    "Uninstall-Resolve-Compatibility.ps1"
)
$required = @($adlSource, $adlDefinition, $lifecycleTest, $readme)
$required += @($lifecycleFiles | ForEach-Object { Join-Path $resolveRoot $_ })
foreach ($path in $required) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Required compatibility-shim source is missing: $path"
    }
}

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
foreach ($obsolete in @("OpenCL.dll", "OpenCL_real.dll", "Launch Resolve (Helios).cmd")) {
    Remove-Item -LiteralPath (Join-Path $OutputDir $obsolete) -Force -ErrorAction SilentlyContinue
}
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

    foreach ($scriptPath in @(
        @($lifecycleFiles | Where-Object { $_ -like "*.ps1" } |
            ForEach-Object { Join-Path $resolveRoot $_ }) + $lifecycleTest
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
        Copy-Item -LiteralPath (Join-Path $resolveRoot $file) -Destination (Join-Path $OutputDir $file) -Force
    }
    & $lifecycleTest -ArtifactDirectory $OutputDir
    Write-Host "Compatibility-shim artifact staged at $OutputDir"
} finally {
    Remove-Item -LiteralPath $buildDir -Recurse -Force -ErrorAction SilentlyContinue
}
