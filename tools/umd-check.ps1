# Build the UMD(s) in the local mirror and report ONLY the Rust diagnostics.
#
# Why this exists: a `win_cargo` UMD build emits ~115 clang warnings from the
# vendored dxvk-helios headers on every run. That is enough to blow the MCP
# tool's output cap, and when it does the rustc errors -- the only part anyone
# wants -- are the lines that get dropped. Filtering has to happen on the VM,
# before the output crosses the tool boundary.
#
#   -Mode check    (default) cargo check, fastest edit loop
#   -Mode build    cargo build (dev profile)
#   -Mode release  cargo build --release, the shippable UMD
#
#   -Crate both    (default) helios_umd.dll AND helios_umd12.dll
#   -Crate umd     the D3D11 UMD only
#   -Crate umd12   the D3D12 UMD only
#
# Exit code is the WORST cargo exit code across the crates built (0 only if
# every crate succeeded). Full unfiltered logs are left at
# Z:\tmp\umd-<mode>.log and Z:\tmp\umd12-<mode>.log.
param(
    [ValidateSet('check', 'build', 'release')]
    [string]$Mode = 'check',
    [ValidateSet('both', 'umd', 'umd12')]
    [string]$Crate = 'both'
)

$ErrorActionPreference = 'Continue'
$mirror = 'C:\Users\Rupansh\helios-vgpu'

# Mirror Z:\ -> local disk. Rust/cargo file IO fails on the 9p share with
# OS error 87 (windows-drivers-rs#481), so the build must run off C:.
# Mirror ONLY what the UMD builds read: the two cdylib crates and the two path
# dependencies they share.
#
# ⛔ `umd_common` is NOT optional here even when building `-Crate umd`: since
# the D3b extraction, `umd/Cargo.toml` carries
# `helios_umd_common = { path = "../umd_common" }`, so a mirror without it fails
# to resolve the dependency and cargo stops before compiling anything. That is
# why this list grew from @('umd','protocol') -- a G6-era edit that actually
# blocks S1.
#
# Mirroring the whole of Z:\ the way win_cargo does is both far slower and
# actively broken here: qemu-helios\build-helios, icd\mesa and .codex\wine-kd
# all contain POSIX symlinks the 9p share exposes but Windows cannot copy. Each
# fails with ERROR 123 and pushes robocopy's exit code to 8, which is
# indistinguishable from a real mirror failure.
foreach ($sub in @('umd', 'umd_common', 'umd12', 'protocol')) {
    if (-not (Test-Path -LiteralPath "Z:\$sub" -PathType Container)) {
        # A crate that does not exist yet is not an error -- this script has to
        # keep working across the staged migration (ARCHITECTURE.md §11), where
        # umd12 appears at S3. Silently skipping a MISSING source is safe;
        # silently skipping a PRESENT one would not be.
        Write-Output "umd-check: skipping absent Z:\$sub"
        continue
    }
    $null = robocopy "Z:\$sub" "$mirror\$sub" /MIR /NFL /NDL /NJH /NJS /NP /R:1 /W:1 `
        /XD target /XF '*.log'
    if ($LASTEXITCODE -ge 8) {
        Write-Output "ROBOCOPY FAILED on $sub ($LASTEXITCODE)"
        exit 1
    }
}

switch ($Mode) {
    'check'   { $cargoArgs = @('check') }
    'build'   { $cargoArgs = @('build') }
    'release' { $cargoArgs = @('build', '--release') }
}
$cargoArgs += @('--message-format', 'short')

# Build one crate in the mirror and report its Rust diagnostics.
#
# ⚠ It reports through Write-Output and communicates the cargo exit code through
# `$script:LastCrateRc`, NOT through a return value. In PowerShell every
# uncaptured Write-Output inside a function joins that function's OUTPUT, so
# `$rc = Invoke-HeliosCrateBuild ...` would swallow the entire diagnostic report
# into $rc and print nothing -- which is exactly what the first version of this
# script did: it emitted only the final summary line and hid every error.
function Invoke-HeliosCrateBuild([string]$CrateDir) {
    $log = "Z:\tmp\$CrateDir-$Mode.log"

    Push-Location "$mirror\$CrateDir"
    $env:CARGO_TARGET_DIR = "$mirror\$CrateDir\target"
    $env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
    & cargo @cargoArgs 2>&1 | Tee-Object -FilePath $log | Out-Null
    $rc = $LASTEXITCODE
    Pop-Location

    $lines = Get-Content $log

    # rustc diagnostics under --message-format short look like
    #   src\lib.rs:120:9: error[E0609]: no field `foo` on type `Bar`
    # The cc/clang noise is all prefixed "warning: helios_umd@0.1.0:" (and the
    # same shape for helios_umd12/helios_umd_common), so match the family.
    $rust = $lines | Where-Object { $_ -notmatch '^warning: helios_umd(12|_common)?@' }

    $errors = $rust | Where-Object { $_ -match '(^|:\s)error(\[|:)' }
    # d3d10umddi.rs / d3d12umddi.rs are bindgen OUTPUT -- their unused-import
    # warnings are generated noise, not crate defects, and drown the real ones.
    $crateWarn = $rust | Where-Object {
        $_ -match ':\s*warning:' -and $_ -notmatch 'd3d1[02]umddi\.rs'
    }

    Write-Output "=== CRATE $CrateDir   MODE $Mode   cargo rc=$rc ==="
    Write-Output "ERRORS: $($errors.Count)"
    $errors | Select-Object -First 60 | ForEach-Object { Write-Output $_ }
    Write-Output "--- crate warnings: $($crateWarn.Count) ---"
    $crateWarn | Select-Object -First 40 | ForEach-Object { Write-Output $_ }
    Write-Output "--- status ---"
    $rust | Where-Object { $_ -match '^(    Finished|error: could not compile|warning: `helios_umd)' } |
        Select-Object -Last 4 | ForEach-Object { Write-Output $_ }
    Write-Output "(full log: $log)"
    Write-Output ""
    $script:LastCrateRc = $rc
}

$crates = switch ($Crate) {
    'both'  { @('umd', 'umd12') }
    'umd'   { @('umd') }
    'umd12' { @('umd12') }
}

$worst = 0
foreach ($c in $crates) {
    if (-not (Test-Path -LiteralPath "$mirror\$c\Cargo.toml" -PathType Leaf)) {
        # ⛔ Asked for explicitly but absent = a real failure. Only the mirror
        # loop above may skip silently, and only for a source that is not there.
        Write-Output "=== CRATE $c   ABSENT (no $mirror\$c\Cargo.toml) ==="
        if ($Crate -ne 'both') { exit 1 }
        $worst = [Math]::Max($worst, 1)
        continue
    }
    $script:LastCrateRc = 0
    Invoke-HeliosCrateBuild $c
    $worst = [Math]::Max($worst, $script:LastCrateRc)
}

Write-Output "=== umd-check: worst cargo rc=$worst across [$($crates -join ', ')] ==="
exit $worst
