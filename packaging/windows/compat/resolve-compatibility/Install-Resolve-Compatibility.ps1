[CmdletBinding()]
param(
    [string]$ResolveDirectory = "C:\Program Files\Blackmagic Design\DaVinci Resolve",
    [string]$StateDirectory = (Join-Path $env:ProgramData "Helios\compatibility\DaVinci Resolve")
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Resolve-CompatibilityCommon.ps1")

function New-StateEntry([string]$Name, [string]$Target, [string]$BackupDirectory) {
    $exists = Test-Path -LiteralPath $Target -PathType Leaf
    $backup = $null
    $originalHash = $null
    if ($exists) {
        $backup = Join-Path $BackupDirectory $Name
        Copy-Item -LiteralPath $Target -Destination $backup -Force
        $originalHash = Get-HeliosFileHash $Target
        if ((Get-HeliosFileHash $backup) -ne $originalHash) {
            throw "Backup verification failed for $Target."
        }
    }
    return [pscustomobject][ordered]@{
        name = $Name
        target = $Target
        originalExists = $exists
        originalHash = $originalHash
        backup = $backup
        managed = $false
        desiredHash = $null
        pendingHash = $null
        previousHash = $null
    }
}

function Find-StateEntry($State, [string]$Name) {
    $entry = @($State.files) | Where-Object { $_.name -eq $Name } | Select-Object -First 1
    if ($null -eq $entry) { throw "Compatibility state has no $Name entry." }
    return $entry
}

Assert-ResolveIsStopped
$ResolveDirectory = [IO.Path]::GetFullPath($ResolveDirectory)
$StateDirectory = [IO.Path]::GetFullPath($StateDirectory)
Assert-HeliosStateLocation $StateDirectory $ResolveDirectory
if (-not (Test-Path -LiteralPath (Join-Path $ResolveDirectory "Resolve.exe") -PathType Leaf)) {
    throw "Resolve.exe was not found in $ResolveDirectory."
}

$sources = [ordered]@{
    "atiadlxx.dll" = Join-Path $PSScriptRoot "atiadlxx.dll"
}
foreach ($source in $sources.Values) {
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "Required compatibility file is missing: $source"
    }
}

$statePath = Join-Path $StateDirectory "install-state.json"
if (Test-Path -LiteralPath $statePath -PathType Leaf) {
    $state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
    if ($state.schemaVersion -ne 1) { throw "Unsupported compatibility state schema." }
    if ([IO.Path]::GetFullPath($state.resolveDirectory) -ne $ResolveDirectory) {
        throw "The existing compatibility state belongs to $($state.resolveDirectory)."
    }
    if ($state.status -notin @("installed", "upgrading")) {
        throw "Compatibility state is '$($state.status)'. Run the saved uninstaller before retrying."
    }
    $obsoleteEntries = @(@($state.files) |
        Where-Object { $_.name -notin $sources.Keys })
    if ($obsoleteEntries.Count -ne 0) {
        throw "An older Resolve compatibility package is installed. Run the saved uninstaller, then install this ADL-only package."
    }
    foreach ($name in $sources.Keys) {
        $entry = Find-StateEntry $state $name
        $current = Get-HeliosFileHash $entry.target
        if ($null -ne $entry.pendingHash -and $current -eq $entry.pendingHash) {
            $entry.desiredHash = $entry.pendingHash
            $entry.pendingHash = $null
            $entry.previousHash = $null
            Write-HeliosCompatibilityState $state $statePath
        } elseif ($null -ne $entry.pendingHash -and $current -eq $entry.previousHash) {
            $entry.pendingHash = $null
            $entry.previousHash = $null
            Write-HeliosCompatibilityState $state $statePath
        } elseif ($current -ne $entry.desiredHash) {
            throw "Managed file changed outside the installer: $($entry.target)"
        }
    }

    foreach ($script in @("Resolve-CompatibilityCommon.ps1", "Uninstall-Resolve-Compatibility.ps1")) {
        $sourceScript = Join-Path $PSScriptRoot $script
        $savedScript = Join-Path $StateDirectory $script
        if ([IO.Path]::GetFullPath($sourceScript) -ne [IO.Path]::GetFullPath($savedScript)) {
            Copy-HeliosFileAtomic $sourceScript $savedScript
        }
    }

    $state.status = "upgrading"
    Write-HeliosCompatibilityState $state $statePath
    foreach ($name in $sources.Keys) {
        $entry = Find-StateEntry $state $name
        $newHash = Get-HeliosFileHash $sources[$name]
        if ($entry.desiredHash -ne $newHash) {
            Set-HeliosManagedFile $state $statePath $entry $sources[$name]
        }
    }
    $state.status = "installed"
    Write-HeliosCompatibilityState $state $statePath
    Write-Host "DaVinci Resolve compatibility files upgraded in $ResolveDirectory"
    return
}

if ((Test-Path -LiteralPath $StateDirectory -PathType Container) -and
    @((Get-ChildItem -LiteralPath $StateDirectory -Force)).Count -ne 0) {
    throw "The new compatibility state directory must be absent or empty: $StateDirectory"
}

New-Item -ItemType Directory -Force -Path $StateDirectory | Out-Null
$backupDirectory = Join-Path $StateDirectory ("backup-" + [Guid]::NewGuid().ToString("N"))
try {
    New-Item -ItemType Directory -Path $backupDirectory | Out-Null
    foreach ($script in @("Resolve-CompatibilityCommon.ps1", "Uninstall-Resolve-Compatibility.ps1")) {
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot $script) -Destination (Join-Path $StateDirectory $script) -Force
    }

    $state = [pscustomobject][ordered]@{
        schemaVersion = 1
        status = "preparing"
        createdAtUtc = [DateTime]::UtcNow.ToString("o")
        updatedAtUtc = [DateTime]::UtcNow.ToString("o")
        resolveDirectory = $ResolveDirectory
        backupDirectory = $backupDirectory
        files = @(
            (New-StateEntry "atiadlxx.dll" (Join-Path $ResolveDirectory "atiadlxx.dll") $backupDirectory)
        )
    }
    Write-HeliosCompatibilityState $state $statePath
} catch {
    Remove-Item -LiteralPath $StateDirectory -Recurse -Force -ErrorAction SilentlyContinue
    throw
}

try {
    foreach ($name in $sources.Keys) {
        Set-HeliosManagedFile $state $statePath (Find-StateEntry $state $name) $sources[$name]
    }
    $state.status = "installed"
    Write-HeliosCompatibilityState $state $statePath
    Write-Host "DaVinci Resolve ADL compatibility installed in $ResolveDirectory"
    Write-Host "Resolve can now be started normally."
} catch {
    $installError = $_
    $state.status = "install-failed"
    Write-HeliosCompatibilityState $state $statePath
    if (Restore-HeliosCompatibilityState $state $statePath) {
        Remove-HeliosCompatibilityStateFiles $state $statePath $StateDirectory
    }
    throw $installError
}
