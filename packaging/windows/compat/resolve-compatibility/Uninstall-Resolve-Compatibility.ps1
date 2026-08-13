[CmdletBinding()]
param(
    [string]$StateDirectory = (Join-Path $env:ProgramData "Helios\compatibility\DaVinci Resolve")
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Resolve-CompatibilityCommon.ps1")

Assert-ResolveIsStopped
$StateDirectory = [IO.Path]::GetFullPath($StateDirectory)
$statePath = Join-Path $StateDirectory "install-state.json"
if (-not (Test-Path -LiteralPath $statePath -PathType Leaf)) {
    Write-Host "DaVinci Resolve compatibility is not installed."
    return
}

$state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
if ($state.schemaVersion -ne 1) { throw "Unsupported compatibility state schema." }
Assert-HeliosStateLocation $StateDirectory $state.resolveDirectory
if (Restore-HeliosCompatibilityState $state $statePath) {
    Remove-HeliosCompatibilityStateFiles $state $statePath $StateDirectory
    Write-Host "DaVinci Resolve compatibility files were restored to their pre-Helios state."
} else {
    throw "Uninstall stopped because managed files changed. State and backups were retained in $StateDirectory."
}
