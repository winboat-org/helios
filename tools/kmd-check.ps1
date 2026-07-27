# Build the KMD in the local mirror and report ONLY the Rust diagnostics.
#
# The sibling of tools/umd-check.ps1, and it exists for a sharper version of the
# same problem: `win_build_kmd` builds the UMD too, and the UMD's ~115 clang
# warnings from the vendored dxvk-helios headers push the KMD's own rustc
# diagnostics off the top of the captured output. On 2026-07-28 that made the
# KMD warning count -- a T4a/T6 gate line -- unreadable from a full package
# build. Filtering has to happen on the VM, before the output crosses the tool
# boundary.
#
# This does NOT package or sign: use win_build_kmd for a shippable image. It is
# the fast edit loop and the warning-count reader.
#
#   -Mode check    (default) cargo check
#   -Mode build    cargo build (the dev profile the driver ships)
#
# Exit code is cargo's. Full unfiltered log at Z:\tmp\kmd-<mode>.log.
param(
    [ValidateSet('check', 'build')]
    [string]$Mode = 'check'
)

$ErrorActionPreference = 'Continue'
$mirror = 'C:\Users\Rupansh\helios-vgpu'

# Mirror Z:\ -> local disk. Rust/cargo file IO fails on the 9p share with
# OS error 87 (windows-drivers-rs#481), so the build must run off C:.
# Mirror ONLY what the KMD build reads: the crate, the host-testable logic crate
# and the shared wire-struct crate. Mirroring all of Z:\ the way win_cargo does
# is far slower and trips over the POSIX symlinks in qemu-helios\build-helios,
# icd\mesa and .codex\wine-kd, each of which fails with ERROR 123 and pushes
# robocopy's exit code to 8 -- indistinguishable from a real mirror failure.
foreach ($sub in @('kmd_render', 'kmd_logic', 'protocol')) {
    $null = robocopy "Z:\$sub" "$mirror\$sub" /MIR /NFL /NDL /NJH /NJS /NP /R:1 /W:1 `
        /XD target /XF '*.log'
    if ($LASTEXITCODE -ge 8) {
        Write-Output "ROBOCOPY FAILED on $sub ($LASTEXITCODE)"
        exit 1
    }
}

Push-Location "$mirror\kmd_render"
$env:CARGO_TARGET_DIR = "$mirror\kmd_render\target"
$env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'

$cargoArgs = @($Mode, '--message-format', 'short')

$log = "Z:\tmp\kmd-$Mode.log"
& cargo @cargoArgs 2>&1 | Tee-Object -FilePath $log | Out-Null
$rc = $LASTEXITCODE
Pop-Location

$lines = Get-Content $log

Write-Output "=== MODE $Mode   cargo rc=$rc ==="

$errors = $lines | Where-Object { $_ -match '^[^:]*: error' -or $_ -match 'error\[E\d+\]' -or $_ -match '^error' }
Write-Output "ERRORS: $($errors.Count)"
$errors | Select-Object -First 40 | ForEach-Object { Write-Output $_ }

# Only OUR crate's warnings. The wdk-build `assert_matches` note and the
# thousands of bindgen `unused import` lines from the generated WDK bindings are
# dependency noise and are not what a gate counts.
$ours = $lines | Where-Object { $_ -match '^src\\' -and $_ -match 'warning' }
Write-Output "--- kmd_render warnings: $($ours.Count) ---"
$ours | ForEach-Object { Write-Output $_ }

Write-Output "--- status ---"
$lines | Where-Object { $_ -match 'generated \d+ warnings|^\s+Finished|could not compile' } |
    ForEach-Object { Write-Output $_ }
Write-Output "(full log: $log)"
exit $rc
