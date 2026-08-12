[CmdletBinding()]
param([Parameter(Mandatory)][string]$ArtifactDirectory)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "Lifecycle test failed: $Message" }
}

function Write-TestFile([string]$Path, [string]$Text) {
    Set-Content -LiteralPath $Path -Value $Text -Encoding UTF8
}

$testRoot = Join-Path ([IO.Path]::GetTempPath()) ("helios-resolve-lifecycle-" + [Guid]::NewGuid().ToString("N"))
$package = Join-Path $testRoot "package"
$resolve = Join-Path $testRoot "resolve"
$state = Join-Path $testRoot "state"
$unsafeState = Join-Path $testRoot "existing-state"
try {
    New-Item -ItemType Directory -Path $package, $resolve, $unsafeState | Out-Null
    Copy-Item -Path (Join-Path $ArtifactDirectory "*") -Destination $package -Force
    # The real loader is supplied by Assemble-Package. Any DLL suffices for this
    # file-lifecycle test because no code is loaded from the fake Resolve tree.
    Copy-Item -LiteralPath (Join-Path $ArtifactDirectory "atiadlxx.dll") `
        -Destination (Join-Path $package "OpenCL_real.dll")

    Write-TestFile (Join-Path $resolve "Resolve.exe") "fake Resolve executable"
    Write-TestFile (Join-Path $resolve "OpenCL.dll") "original OpenCL loader"
    Write-TestFile (Join-Path $resolve "OpenCL_real.dll") "original real-loader name"
    Write-TestFile (Join-Path $resolve "atiadlxx.dll") "original ADL DLL"
    Write-TestFile (Join-Path $resolve "Launch Resolve (Helios).cmd") "original launcher"
    $originalHashes = @{}
    foreach ($name in @("OpenCL.dll", "OpenCL_real.dll", "atiadlxx.dll", "Launch Resolve (Helios).cmd")) {
        $originalHashes[$name] = (Get-FileHash -LiteralPath (Join-Path $resolve $name) -Algorithm SHA256).Hash
    }

    $installer = Join-Path $package "Install-Resolve-Compatibility.ps1"
    & $installer -ResolveDirectory $resolve -StateDirectory $state
    foreach ($name in @("OpenCL.dll", "OpenCL_real.dll", "atiadlxx.dll", "Launch Resolve (Helios).cmd")) {
        Assert-True ((Get-FileHash -LiteralPath (Join-Path $resolve $name) -Algorithm SHA256).Hash -eq
            (Get-FileHash -LiteralPath (Join-Path $package $name) -Algorithm SHA256).Hash) `
            "$name was not installed exactly"
    }
    & $installer -ResolveDirectory $resolve -StateDirectory $state

    Write-TestFile (Join-Path $resolve "OpenCL.dll") "third-party modification"
    $blocked = $false
    try {
        & (Join-Path $state "Uninstall-Resolve-Compatibility.ps1") -StateDirectory $state
    } catch {
        $blocked = $true
    }
    Assert-True $blocked "uninstall must reject a modified managed DLL"
    Assert-True (Test-Path -LiteralPath (Join-Path $state "install-state.json")) `
        "blocked uninstall must retain state"
    Assert-True ((Get-Content -LiteralPath (Join-Path $resolve "OpenCL.dll") -Raw) -match "third-party") `
        "blocked uninstall must not overwrite the modified DLL"

    Copy-Item -LiteralPath (Join-Path $package "OpenCL.dll") -Destination (Join-Path $resolve "OpenCL.dll") -Force
    & (Join-Path $state "Uninstall-Resolve-Compatibility.ps1") -StateDirectory $state
    foreach ($name in $originalHashes.Keys) {
        Assert-True ((Get-FileHash -LiteralPath (Join-Path $resolve $name) -Algorithm SHA256).Hash -eq
            $originalHashes[$name]) "$name was not restored exactly"
    }
    Assert-True (-not (Test-Path -LiteralPath $state)) "empty dedicated state directory must be removed"

    $sentinel = Join-Path $unsafeState "keep-me.txt"
    Write-TestFile $sentinel "do not remove"
    $rejected = $false
    try {
        & $installer -ResolveDirectory $resolve -StateDirectory $unsafeState
    } catch {
        $rejected = $true
    }
    Assert-True $rejected "nonempty new state directory must be rejected"
    Assert-True (Test-Path -LiteralPath $sentinel) "state-directory rejection must preserve existing data"
    Write-Host "Resolve compatibility lifecycle tests passed."
} finally {
    Remove-Item -LiteralPath $testRoot -Recurse -Force -ErrorAction SilentlyContinue
}
