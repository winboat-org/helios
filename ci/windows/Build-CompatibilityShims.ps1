param(
    [Parameter(Mandatory)][string]$RepoRoot,
    [Parameter(Mandatory)][string]$OutputDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Initialize-HeliosBuild.ps1")
Import-VisualStudioEnvironment

$sourceRoot = Join-Path $RepoRoot "packaging\windows\compat\adl-shim"
$source = Join-Path $sourceRoot "helios-adl-shim.cpp"
$definition = Join-Path $sourceRoot "helios-adl-shim.def"
$readme = Join-Path $sourceRoot "README.md"
foreach ($path in @($source, $definition, $readme)) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Required compatibility-shim source is missing: $path"
    }
}

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$buildDir = Join-Path ([IO.Path]::GetTempPath()) ("helios-adl-shim-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $buildDir | Out-Null

try {
    $object = Join-Path $buildDir "helios-adl-shim.obj"
    $dll = Join-Path $OutputDir "atiadlxx.dll"
    $importLibrary = Join-Path $buildDir "atiadlxx.lib"

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
        "/Fo$object",
        $source
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
        "/OUT:$dll",
        "/IMPLIB:$importLibrary",
        "/DEF:$definition",
        $object,
        "setupapi.lib",
        "user32.lib",
        "uuid.lib"
    )
    & link.exe @linkArguments
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $dll -PathType Leaf)) {
        throw "link.exe failed to produce atiadlxx.dll."
    }

    $exportTable = (& dumpbin.exe /nologo /exports $dll 2>&1 | Out-String)
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

    Copy-Item -LiteralPath $readme -Destination (Join-Path $OutputDir "README.md") -Force
    Write-Host "Compatibility-shim artifact staged at $OutputDir"
} finally {
    Remove-Item -LiteralPath $buildDir -Recurse -Force -ErrorAction SilentlyContinue
}
