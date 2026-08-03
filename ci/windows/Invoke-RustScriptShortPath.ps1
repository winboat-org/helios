param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$RustScriptArguments
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$realExecutable = $env:HELIOS_RUST_SCRIPT_REAL
if ([string]::IsNullOrWhiteSpace($realExecutable) -or
    -not (Test-Path -LiteralPath $realExecutable -PathType Leaf)) {
    throw "HELIOS_RUST_SCRIPT_REAL does not name the installed rust-script executable."
}

$scriptIndex = -1
for ($index = 0; $index -lt $RustScriptArguments.Count; $index++) {
    $candidate = $RustScriptArguments[$index]
    if ($candidate.EndsWith(".rs", [StringComparison]::OrdinalIgnoreCase) -and
        (Test-Path -LiteralPath $candidate -PathType Leaf)) {
        $scriptIndex = $index
        break
    }
}

# Version checks, cache management and any future non-script commands should
# behave exactly like the real executable.
if ($scriptIndex -lt 0) {
    & $realExecutable @RustScriptArguments
    exit $LASTEXITCODE
}

$source = (Resolve-Path -LiteralPath $RustScriptArguments[$scriptIndex]).Path
$shortRoot = $env:HELIOS_RUST_SCRIPT_SHORT_ROOT
if ([string]::IsNullOrWhiteSpace($shortRoot)) {
    $shortRoot = "C:\rs"
}
New-Item -ItemType Directory -Force -Path $shortRoot | Out-Null

$digest = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash.ToLowerInvariant()
$shortScript = Join-Path $shortRoot ($digest.Substring(0, 16) + ".rs")
Copy-Item -LiteralPath $source -Destination $shortScript -Force

$forwarded = [Collections.Generic.List[string]]::new()
$hasBasePath = $RustScriptArguments -contains "--base-path"
for ($index = 0; $index -lt $RustScriptArguments.Count; $index++) {
    if ($index -eq $scriptIndex) {
        if (-not $hasBasePath) {
            $forwarded.Add("--base-path")
            $forwarded.Add((Split-Path -Parent $source))
        }
        $forwarded.Add($shortScript)
    } else {
        $forwarded.Add($RustScriptArguments[$index])
    }
}

& $realExecutable @forwarded
exit $LASTEXITCODE
