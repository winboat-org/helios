<#
.SYNOPSIS
  Snapshot the Helios KMD service-key counters to a file, and print the subset a
  transport/venus soak cares about.

.DESCRIPTION
  Registry counter values PERSIST ACROSS BOOTS, so a snapshot is only evidence
  when it is diffed against another snapshot from the same boot (or when the
  counter is one the driver explicitly zeroes at StartDevice). Take one before a
  workload and one after; diff the files.

  The full key is ~3000 values once the S-ring breadcrumbs are counted, which is
  why the dump goes to a file and only the named subset is printed.

.PARAMETER Label
  Becomes the filename: Z:\tmp\kmd-counters-<Label>.txt

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File Z:\tools\kmd-counter-snapshot.ps1 -Label pre-t4a
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $Label,
    [string] $OutDir = 'Z:\tmp',
    # Counters the T4a (transport / venus / allocation) gate reads. Prefix match.
    [string[]] $Watch = @(
        # transport health
        'FENCE_WAIT', 'CTRL_TIMEOUT', 'QUEUE_FULL', 'PARKED_LEAKS', 'REAP_',
        'ASYNC_SUBMIT', 'ASYNC_COMPLETE', 'ASYNC_CTRL', 'DRAIN_BAD_TOKEN',
        'RING_SUBMIT', 'RING_COMPLETE', 'DMA_POOL', 'PREEMPT_COUNT',
        'FENCE_EVENT', 'RESOURCE_HIGH_WATER', 'MAP_PAGES_FAILS',
        'WINDOW_ALLOC_REJECTS', 'WINDOW_RANGE_DROPS', 'DMA_STALE_SKIP',
        # escape-visible transport rollups (short names)
        'WtOut', 'CtOut', 'QfRet', 'PkHi', 'IfHi', 'AsSub', 'AsDone',
        # scanout / present surface that must not move
        'VpSA', 'ScSet', 'ScFlu', 'ScRid', 'VpDSt', 'DspMd', 'ScCpy', 'ScPch',
        'ScanoutDiag', 'ScFrc', 'VsCnt', 'SaCnt',
        # T3 refusal counters (zeroed at StartDevice)
        'ScBadAlc', 'ScBadExt', 'ScBadLay', 'ScBadFmt', 'ScLinErr', 'ScSetErr',
        'ScNoTgt', 'ScCpyErr', 'ScUnav', 'ScRetry', 'ScGaveUp', 'ScStale',
        'ScGateCx', 'HpdStTo',
        # counters T4a ADDS -- must be ABSENT on a healthy boot
        'VnEncOvf', 'VnRingFt', 'VnRingWd', 'VnRingSz', 'VnMtDown', 'VnEchoNe',
        'CpNoDrn', 'PBTdErr', 'CtNotOurs', 'AbnDrop', 'ABANDONED_FENCES',
        'ChSzMm', 'MapDup', 'PciCapOob', 'WINDOW_NOT_CONFIGURED',
        'FENCE_WAIT_TABLE_FULL'
    )
)

$ErrorActionPreference = 'Stop'

$key = 'HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render'
$props = (Get-ItemProperty $key).PSObject.Properties |
         Where-Object { $_.Name -notmatch '^PS' } |
         Sort-Object Name

if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }
$out = Join-Path $OutDir ("kmd-counters-{0}.txt" -f $Label)

$header = @(
    "# helios KMD counter snapshot"
    "# label     : $Label"
    "# taken     : $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"
    "# last boot : $((Get-CimInstance Win32_OperatingSystem).LastBootUpTime)"
    "# driver    : $((Get-PnpDeviceProperty -InstanceId (Get-PnpDevice -Class Display -FriendlyName '*Helios*' | Select-Object -First 1 -ExpandProperty InstanceId) -KeyName 'DEVPKEY_Device_DriverVersion' -ErrorAction SilentlyContinue).Data)"
    "# values    : $($props.Count)"
)
$body = $props | ForEach-Object { "{0}={1}" -f $_.Name, $_.Value }
($header + $body) | Set-Content -Path $out -Encoding UTF8

Write-Host "wrote $out ($($props.Count) values)"
Write-Host "--- watched ---"
foreach ($w in $Watch) {
    $hits = $props | Where-Object { $_.Name -like "$w*" }
    if ($hits) { $hits | ForEach-Object { Write-Host ("{0,-24} {1}" -f $_.Name, $_.Value) } }
    else       { Write-Host ("{0,-24} <absent>" -f $w) }
}
