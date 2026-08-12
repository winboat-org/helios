Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Get-HeliosFileHash([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $null }
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToUpperInvariant()
}

function Write-HeliosCompatibilityState($State, [string]$StatePath) {
    $temporary = "$StatePath.$PID.$([Guid]::NewGuid().ToString('N')).tmp"
    $State.updatedAtUtc = [DateTime]::UtcNow.ToString("o")
    $State | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $temporary -Encoding UTF8
    Move-Item -LiteralPath $temporary -Destination $StatePath -Force
}

function Copy-HeliosFileAtomic([string]$Source, [string]$Destination) {
    $parent = Split-Path -Parent $Destination
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    $temporary = Join-Path $parent ("." + [IO.Path]::GetFileName($Destination) +
        ".$PID.$([Guid]::NewGuid().ToString('N')).tmp")
    try {
        Copy-Item -LiteralPath $Source -Destination $temporary -Force
        Move-Item -LiteralPath $temporary -Destination $Destination -Force
    } finally {
        Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
    }
}

function Assert-ResolveIsStopped {
    if (Get-Process -Name "Resolve" -ErrorAction SilentlyContinue) {
        throw "DaVinci Resolve is running. Close it before changing its compatibility files."
    }
}

function Test-HeliosPathContains([string]$Parent, [string]$Child) {
    $parentFull = [IO.Path]::GetFullPath($Parent).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    $childFull = [IO.Path]::GetFullPath($Child).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    return $childFull.StartsWith($parentFull, [StringComparison]::OrdinalIgnoreCase)
}

function Assert-HeliosStateLocation([string]$StateDirectory, [string]$ResolveDirectory) {
    $stateFull = [IO.Path]::GetFullPath($StateDirectory)
    $resolveFull = [IO.Path]::GetFullPath($ResolveDirectory)
    if ($stateFull -eq [IO.Path]::GetPathRoot($stateFull)) {
        throw "The compatibility state directory cannot be a filesystem root."
    }
    if ((Test-HeliosPathContains $stateFull $resolveFull) -or
        (Test-HeliosPathContains $resolveFull $stateFull)) {
        throw "The compatibility state and Resolve directories must not contain one another."
    }
}

function Remove-HeliosCompatibilityStateFiles(
    $State,
    [string]$StatePath,
    [string]$StateDirectory
) {
    $stateFull = [IO.Path]::GetFullPath($StateDirectory)
    $backupFull = [IO.Path]::GetFullPath($State.backupDirectory)
    $backupName = [IO.Path]::GetFileName($backupFull)
    if ((Split-Path -Parent $backupFull) -ine $stateFull -or
        $backupName -notmatch '^backup-[0-9a-f]{32}$') {
        throw "Recorded compatibility backup is not a managed direct child of its state directory."
    }
    Remove-Item -LiteralPath $backupFull -Recurse -Force -ErrorAction SilentlyContinue
    foreach ($path in @(
        $StatePath,
        (Join-Path $stateFull "Resolve-CompatibilityCommon.ps1"),
        (Join-Path $stateFull "Uninstall-Resolve-Compatibility.ps1")
    )) {
        Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
    }
    if ((Test-Path -LiteralPath $stateFull -PathType Container) -and
        @((Get-ChildItem -LiteralPath $stateFull -Force)).Count -eq 0) {
        Remove-Item -LiteralPath $stateFull -Force
    }
}

function Set-HeliosManagedFile(
    $State,
    [string]$StatePath,
    $Entry,
    [string]$Source
) {
    $expected = Get-HeliosFileHash $Source
    if ($null -eq $expected) { throw "Managed source is missing: $Source" }

    $Entry.managed = $true
    $Entry.previousHash = Get-HeliosFileHash $Entry.target
    $Entry.pendingHash = $expected
    Write-HeliosCompatibilityState $State $StatePath
    Copy-HeliosFileAtomic $Source $Entry.target
    $actual = Get-HeliosFileHash $Entry.target
    if ($actual -ne $expected) { throw "Hash verification failed after installing $($Entry.target)." }
    $Entry.desiredHash = $expected
    $Entry.pendingHash = $null
    $Entry.previousHash = $null
    Write-HeliosCompatibilityState $State $StatePath
}

function Restore-HeliosCompatibilityState($State, [string]$StatePath) {
    $blocked = @()
    $entries = @($State.files)
    [Array]::Reverse($entries)
    $State.status = "uninstalling"
    Write-HeliosCompatibilityState $State $StatePath

    foreach ($entry in $entries) {
        if (-not $entry.managed) { continue }
        $current = Get-HeliosFileHash $entry.target
        if (($entry.originalExists -and $current -eq $entry.originalHash) -or
            (-not $entry.originalExists -and $null -eq $current)) {
            $entry.managed = $false
            $entry.desiredHash = $null
            $entry.pendingHash = $null
            $entry.previousHash = $null
            Write-HeliosCompatibilityState $State $StatePath
            continue
        }
        $accepted = @($entry.desiredHash, $entry.pendingHash, $entry.previousHash) |
            Where-Object { $null -ne $_ -and $_ -ne "" } | Select-Object -Unique
        if ($null -eq $current -or $current -notin $accepted) {
            $blocked += $entry.target
            continue
        }

        if ($entry.originalExists) {
            $backupHash = Get-HeliosFileHash $entry.backup
            if ($backupHash -ne $entry.originalHash) {
                $blocked += $entry.target
                continue
            }
            Copy-HeliosFileAtomic $entry.backup $entry.target
        } else {
            Remove-Item -LiteralPath $entry.target -Force
        }
        $entry.managed = $false
        $entry.desiredHash = $null
        $entry.pendingHash = $null
        $entry.previousHash = $null
        Write-HeliosCompatibilityState $State $StatePath
    }

    if ($blocked.Count -ne 0) {
        $State.status = "rollback-blocked"
        Write-HeliosCompatibilityState $State $StatePath
        foreach ($path in $blocked) {
            Write-Warning "Not restoring modified or missing managed file: $path"
        }
        return $false
    }
    return $true
}
