<#
.SYNOPSIS
  Prove whether `debug_assert!` bodies are compiled INTO the shipped KMD image.

.DESCRIPTION
  They are, and that is a standing hazard: a failing debug_assert! is a
  KeBugCheck inside a DDI, `cargo make`'s verify-no-panics greps only
  .unwrap()/.expect(, and #![deny(clippy::panic)] does not cover it either.
  As of 22.22.186.0 FOUR ship: virtio/ctrl.rs (reap_parked), virtio/gpu.rs x2
  (begin_parked_reap), ddi/present_packet.rs (debug_assert_eq!).

  The test is that a compiled-out assert cannot leave its stringified
  expression in the binary. Run it after any change to [profile.dev] or after
  adding an assert; every needle should read present=False once the profile
  sets debug-assertions = false.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File Z:\tools\kmd-debug-assert-check.ps1
#>
# Empirical check of the claim in ROADMAP 7b / commit 1: the SHIPPED KMD image is
# the dev profile, which does NOT disable debug-assertions, so `debug_assert!`
# bodies are COMPILED IN. If they were compiled out, their stringified-expression
# panic messages could not appear in the binary.
$ErrorActionPreference = 'Stop'
$sys = 'C:\Users\Rupansh\helios-vgpu\kmd_render\target\debug\helios_kmd_render_package\helios_kmd_render.sys'
Write-Host ("image  : {0}" -f $sys)
Write-Host ("version: {0}" -f (Get-Item $sys).VersionInfo.FileVersion)

$bytes = [System.IO.File]::ReadAllBytes($sys)
$text  = [System.Text.Encoding]::ASCII.GetString($bytes)

$needles = @(
    'buffers.capacity() >= dead.len().saturating_mul(2)',   # ctrl.rs debug_assert
    'fresh.capacity() >= MAX_PARKED',                       # gpu.rs debug_assert
    'buffers.capacity() >= 2 * MAX_PARKED',                 # gpu.rs debug_assert
    'assertion failed',                                     # the panic prelude
    'ctrl.rs',
    'gpu.rs',
    'present_packet.rs'
)
foreach ($n in $needles) {
    Write-Host ("{0,-52} present={1}" -f $n, $text.Contains($n))
}
