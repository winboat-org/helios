<#
.SYNOPSIS
  Capture the bridge's DXBC container output for a fixed shader workload, as a
  build-independent fingerprint that can be diffed across two UMDs.

.DESCRIPTION
  R1014(1) collapsed three copies of the DXBC container assembly into one
  `build_dxbc_container`. Its decisive test is that the produced containers are
  BIT-IDENTICAL, including the MD5 stamp -- which catches any chunk-offset or
  padding change.

  `dump_shader_bytecode` writes `shader-<pid>-<seq>-<stage>-<raw|wrapped>-<len>.dxbc`,
  so filenames carry a pid and a per-process sequence number and cannot be
  compared directly across runs. What IS comparable is the MAPPING: each
  `wrapped` dump is emitted immediately after the `raw` dump it was built from,
  so pairing them by consecutive seq gives `sha256(raw) -> sha256(wrapped)`.
  A container change moves the right-hand side while the left-hand side stays
  put; a different shader set only changes which keys are present.

  Emits one sorted `<raw-sha> <stage> <wrapped-sha>` line per pair. Diff two
  labels: every key common to both runs must map to the same value.

.PARAMETER Label
  Becomes Z:\tmp\dxbc-<Label>.txt (and the dump directory).

.PARAMETER Tasks
  Scheduled tasks to run as the workload, in order. Must create shaders, and
  should between them reach all three container wrappers -- the plain code-only
  wrap, the >=11.1 signature wrap and the tessellation wrap.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File Z:\tools\shader-dxbc-ab.ps1 -Label t6
  powershell -ExecutionPolicy Bypass -File Z:\tools\shader-dxbc-ab.ps1 -Label t7
  # then diff Z:\tmp\dxbc-t6.txt Z:\tmp\dxbc-t7.txt
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $Label,
    [string[]] $Tasks = @('helios_d3d11_extra_suite', 'helios_tess_probe_once',
                          'helios_d3d11_knob_suite_default', 'helios_triangle'),
    [int]      $WaitSeconds = 60
)

$ErrorActionPreference = 'Continue'
$dumpDir = "C:\Users\Rupansh\helios-dxbc-$Label"
$out = "Z:\tmp\dxbc-$Label.txt"

# A fresh directory, so a previous run's files cannot be mistaken for this one's.
if (Test-Path $dumpDir) { Remove-Item $dumpDir -Recurse -Force }
New-Item -ItemType Directory -Path $dumpDir -Force | Out-Null

# The knob is read ONCE per process (a function-local static), so it must be set
# before the workload starts, and only a NEW process picks it up.
New-Item -Path 'HKLM:\SOFTWARE\Helios' -Force | Out-Null
Set-ItemProperty -Path 'HKLM:\SOFTWARE\Helios' -Name 'ShaderBytecodeDumpPath' `
    -Value $dumpDir -Type String

Write-Host "dump dir  : $dumpDir"
foreach ($t in $Tasks) {
    Write-Host "workload  : $t"
    schtasks /run /tn $t | Out-Null
    Start-Sleep -Seconds $WaitSeconds
}

# Turn the knob back off: it is an every-shader file write on a live desktop.
Remove-ItemProperty -Path 'HKLM:\SOFTWARE\Helios' -Name 'ShaderBytecodeDumpPath' `
    -ErrorAction SilentlyContinue

$files = Get-ChildItem "$dumpDir\shader-*.dxbc" -ErrorAction SilentlyContinue
Write-Host "dumps     : $($files.Count) file(s)"
if (-not $files) {
    Write-Host "NO DUMPS -- the workload created no shaders, or it never loaded a new UMD"
    "NO DUMPS" | Set-Content $out
    exit 1
}

# Parse pid + seq + stage + form out of the filename, then pair each `wrapped`
# with the `raw` at seq-1 in the same process.
$parsed = @{}
foreach ($f in $files) {
    if ($f.Name -match '^shader-(\d+)-(\d+)-([\w-]+)-(raw|wrapped)-(\d+)\.dxbc$') {
        $key = "$($Matches[1])/$([int]$Matches[2])"
        $parsed[$key] = [pscustomobject]@{
            Pid   = $Matches[1]
            Seq   = [int]$Matches[2]
            Stage = $Matches[3]
            Form  = $Matches[4]
            Path  = $f.FullName
        }
    }
}

$pairs = @()
foreach ($entry in $parsed.Values) {
    if ($entry.Form -ne 'wrapped') { continue }
    $rawKey = "$($entry.Pid)/$($entry.Seq - 1)"
    $raw = $parsed[$rawKey]
    if (-not $raw -or $raw.Form -ne 'raw') { continue }
    $pairs += ('{0} {1} {2}' -f `
        (Get-FileHash $raw.Path   -Algorithm SHA256).Hash,
        $entry.Stage,
        (Get-FileHash $entry.Path -Algorithm SHA256).Hash)
}

$pairs = $pairs | Sort-Object -Unique
$pairs | Set-Content $out
Write-Host "pairs     : $($pairs.Count) unique raw->wrapped mapping(s)"
Write-Host "written   : $out"
