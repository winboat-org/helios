# Build the UMD in the local mirror and report ONLY the Rust diagnostics.
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
# Exit code is cargo's. Full unfiltered log is left at Z:\tmp\umd-<mode>.log.
param(
    [ValidateSet('check', 'build', 'release')]
    [string]$Mode = 'check'
)

$ErrorActionPreference = 'Continue'
$mirror = 'C:\Users\Rupansh\helios-vgpu'

# Mirror Z:\ -> local disk. Rust/cargo file IO fails on the 9p share with
# OS error 87 (windows-drivers-rs#481), so the build must run off C:.
# Mirror ONLY what the UMD build reads: the crate itself and the one path
# dependency (helios_protocol = { path = "../protocol" }). There is no workspace
# root Cargo.toml, and the DXVK archives come from C:\Users\Rupansh\dxvk-helios
# / dxvk-build, which win_dxvk maintains outside this mirror.
#
# Mirroring the whole of Z:\ the way win_cargo does is both far slower and
# actively broken here: qemu-helios\build-helios, icd\mesa and .codex\wine-kd
# all contain POSIX symlinks the 9p share exposes but Windows cannot copy. Each
# fails with ERROR 123 and pushes robocopy's exit code to 8, which is
# indistinguishable from a real mirror failure.
foreach ($sub in @('umd', 'protocol')) {
    $null = robocopy "Z:\$sub" "$mirror\$sub" /MIR /NFL /NDL /NJH /NJS /NP /R:1 /W:1 `
        /XD target /XF '*.log'
    if ($LASTEXITCODE -ge 8) {
        Write-Output "ROBOCOPY FAILED on $sub ($LASTEXITCODE)"
        exit 1
    }
}

Push-Location "$mirror\umd"
$env:CARGO_TARGET_DIR = "$mirror\umd\target"
$env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'

switch ($Mode) {
    'check'   { $cargoArgs = @('check') }
    'build'   { $cargoArgs = @('build') }
    'release' { $cargoArgs = @('build', '--release') }
}
$cargoArgs += @('--message-format', 'short')

$log = "Z:\tmp\umd-$Mode.log"
& cargo @cargoArgs 2>&1 | Tee-Object -FilePath $log | Out-Null
$rc = $LASTEXITCODE
Pop-Location

$lines = Get-Content $log

# rustc diagnostics under --message-format short look like
#   src\lib.rs:120:9: error[E0609]: no field `foo` on type `Bar`
# The cc/clang noise is all prefixed "warning: helios_umd@0.1.0:".
$rust = $lines | Where-Object { $_ -notmatch '^warning: helios_umd@' }

$errors = $rust | Where-Object { $_ -match '(^|:\s)error(\[|:)' }
$crateWarn = $rust | Where-Object {
    $_ -match ':\s*warning:' -and $_ -notmatch 'd3d10umddi\.rs'
}

Write-Output "=== MODE $Mode   cargo rc=$rc ==="
Write-Output "ERRORS: $($errors.Count)"
$errors | Select-Object -First 60 | ForEach-Object { Write-Output $_ }
Write-Output "--- crate warnings: $($crateWarn.Count) ---"
$crateWarn | Select-Object -First 40 | ForEach-Object { Write-Output $_ }
Write-Output "--- status ---"
$rust | Where-Object { $_ -match '^(    Finished|error: could not compile|warning: `helios_umd`)' } |
    Select-Object -Last 4 | ForEach-Object { Write-Output $_ }
Write-Output "(full log: $log)"
exit $rc
