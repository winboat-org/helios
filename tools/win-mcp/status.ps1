# winrun status — one call that answers "what is actually running on this host".
#
# Written because the same five probes (install-state, PnP, DriverStore hashes,
# ICD path, DWM module list) were rewritten by hand at the start of nearly every
# session, each time slightly differently.
#
#   -Role vm     the guest: installed Helios package and the live stack
#   -Role slave  the build slave: tree pins, warm build dirs, staged artifacts
#
# Emits one JSON object between WINRUN_JSON_BEGIN/END.

param(
    [Parameter(Mandatory)][ValidateSet('vm', 'slave')][string]$Role
)

$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'

function FileState([string]$Path) {
    if (-not $Path -or -not (Test-Path -LiteralPath $Path)) {
        return [pscustomobject]@{ path = $Path; exists = $false; size = $null; sha256 = $null; version = $null }
    }
    $i = Get-Item -LiteralPath $Path
    [pscustomobject]@{
        path    = $Path
        exists  = $true
        size    = $i.Length
        sha256  = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
        version = $i.VersionInfo.FileVersion
    }
}

function DriveState {
    Get-PSDrive -PSProvider FileSystem -ErrorAction SilentlyContinue |
        Where-Object { $_.Free -ne $null } |
        ForEach-Object { [pscustomobject]@{ name = $_.Name; free_gb = [Math]::Round($_.Free / 1GB, 2) } }
}

$result = [ordered]@{}
$result.utc = [DateTime]::UtcNow.ToString('o')
$result.role = $Role
$result.hostname = $env:COMPUTERNAME
$result.session_id = (Get-Process -Id $PID).SessionId
$result.user = "$env:USERDOMAIN\$env:USERNAME"

if ($Role -eq 'vm') {

    # ── installed package ──────────────────────────────────────────────────
    $statePath = 'C:\ProgramData\Helios\install-state.json'
    if (Test-Path -LiteralPath $statePath) {
        $state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
        $result.install = [ordered]@{
            present          = $true
            packageId        = $state.packageId
            version          = $state.version
            activeInf        = $state.activeInf
            activeInfSha256  = $state.activeInfSha256
            instanceId       = $state.instanceId
            classKey         = $state.classKey
            installedAtUtc   = $state.installedAtUtc
            packageRoot      = $state.installRoot
            vulkanManifest   = $state.vulkanManifest
        }
    }
    else {
        $result.install = [ordered]@{ present = $false }
    }

    # ── device + kernel service ───────────────────────────────────────────
    if ($state) {
        $pnp = Get-PnpDevice -InstanceId $state.instanceId -ErrorAction SilentlyContinue
        $result.device = [pscustomobject]@{
            instanceId = $state.instanceId
            status     = "$($pnp.Status)"
            problem    = [int]$pnp.Problem
            friendly   = "$($pnp.FriendlyName)"
        }
        $reg = Get-ItemProperty -LiteralPath $state.classKey -ErrorAction SilentlyContinue
        $driverDir = if ($reg -and $reg.UserModeDriverName) { Split-Path -Parent ([string]$reg.UserModeDriverName[0]) } else { $null }
        $result.driver_dir = $driverDir
        $result.driver_images = @(
            foreach ($n in @('helios_kmd_render.sys', 'helios_umd.dll', 'helios_umd12.dll', 'helios_umd32.dll', 'helios_umd12_32.dll')) {
                if ($driverDir) { FileState (Join-Path $driverDir $n) }
            }
        )
        $result.user_mode_driver_name = if ($reg) { @($reg.UserModeDriverName) } else { @() }
    }

    $svc = Get-CimInstance Win32_SystemDriver -Filter "Name='helios_kmd_render'" -ErrorAction SilentlyContinue
    if ($svc) {
        $kmdPath = $svc.PathName.Replace('\SystemRoot', $env:windir).Replace('\??\', '')
        $result.kmd_service = [ordered]@{ state = $svc.State; start_mode = $svc.StartMode; image = FileState $kmdPath }
    }
    else {
        $result.kmd_service = [ordered]@{ state = '(absent)' }
    }

    # ── graphics runtimes actually loaded by the desktop ───────────────────
    $result.dwm = @(
        Get-Process dwm -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -ne 0 } | ForEach-Object {
            $p = $_
            [pscustomobject]@{
                pid     = $p.Id
                session = $p.SessionId
                started = $p.StartTime.ToString('o')
                modules = @(
                    $p.Modules | Where-Object { $_.ModuleName -in 'helios_umd.dll', 'helios_umd12.dll', 'vulkan_virtio.dll', 'libgallium_wgl.dll' } |
                        ForEach-Object { [pscustomobject]@{ name = $_.ModuleName; path = $_.FileName; sha256 = (Get-FileHash -LiteralPath $_.FileName -Algorithm SHA256).Hash } }
                )
            }
        }
    )

    $result.vulkan_registered = @(
        $key = 'HKLM:\SOFTWARE\Khronos\Vulkan\Drivers'
        if (Test-Path $key) {
            $item = Get-ItemProperty -Path $key
            $item.PSObject.Properties |
                Where-Object { $_.Name -notlike 'PS*' } |
                ForEach-Object { [pscustomobject]@{ manifest = $_.Name; value = $_.Value } }
        }
    )

    $result.icd_expected = @(
        if ($state -and $state.runtimeFiles) {
            $state.runtimeFiles | Where-Object { $_.path -like '*vulkan_virtio.dll' } | ForEach-Object { FileState $_.path }
        }
    )
    $result.runtime_files = @(
        if ($state -and $state.runtimeFiles) {
            $state.runtimeFiles | ForEach-Object { FileState $_.path }
        }
    )

    # Pending reboot is the difference between "installed" and "loaded".
    $pending = @()
    foreach ($p in @('HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending',
                     'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired')) {
        if (Test-Path $p) { $pending += $p }
    }
    $result.pending_reboot = $pending

    $evidenceRoot = 'C:\ProgramData\Helios'
    $result.evidence_dirs = @(
        Get-ChildItem -LiteralPath $evidenceRoot -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -like '*evidence*' -or $_.Name -like 'integration-*' } |
            ForEach-Object {
                $newest = Get-ChildItem -LiteralPath $_.FullName -Recurse -File -ErrorAction SilentlyContinue |
                    Sort-Object LastWriteTime -Descending | Select-Object -First 1
                [pscustomobject]@{ path = $_.FullName; entries = @(Get-ChildItem -LiteralPath $_.FullName -ErrorAction SilentlyContinue).Count; newest_utc = if ($newest) { $newest.LastWriteTimeUtc.ToString('o') } else { $null } }
            }
    )
}
else {

    # ── build slave ───────────────────────────────────────────────────────
    $repo = 'C:\src\helios-integration-20260912'
    $result.repo = $repo
    if (Test-Path -LiteralPath $repo) {
        $head = (& git -C $repo rev-parse HEAD 2>&1 | Select-Object -First 1)
        $statusLines = @(& git -C $repo status --short 2>&1)
        $result.head = "$head".Trim()
        $result.status = $statusLines
        $result.version = "$(Get-Content -LiteralPath (Join-Path $repo 'kmd_render\driver-version.env') -ErrorAction SilentlyContinue | Where-Object { $_ -match 'HELIOS_KMD_VERSION' })".Trim()
        $result.submodules = @(
            foreach ($rel in @('.', 'icd\mesa', 'vkd3d-proton-helios', 'dxvk-helios', 'qemu-helios', 'virglrenderer', 'venus-protocol', 'LookingGlass')) {
                $path = Join-Path $repo $rel
                $sha = if ($rel -eq '.') { "$head".Trim() } else { "$(& git -C $path rev-parse HEAD 2>&1 | Select-Object -First 1)".Trim() }
                [pscustomobject]@{ path = $rel; head = $sha }
            }
        )
    }

    $result.warm_dirs = @(
        foreach ($d in @('C:\helios-build-integration-20260912', 'C:\helios-mesa-integration-20260912-x64', 'C:\helios-mesa-integration-20260912-x86')) {
            if (Test-Path -LiteralPath $d) {
                $ninja = Join-Path $d 'build.ninja'
                [pscustomobject]@{ path = $d; configured = (Test-Path -LiteralPath $ninja); last_write_utc = (Get-Item -LiteralPath $d).LastWriteTimeUtc.ToString('o') }
            }
            else {
                [pscustomobject]@{ path = $d; configured = $false; last_write_utc = $null }
            }
        }
    )

    $result.staged_artifacts = @(
        Get-ChildItem -LiteralPath 'C:\src\out' -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -like 'integration-*' } | Sort-Object Name | ForEach-Object {
                $out = $_.FullName
                $zip = Get-ChildItem -LiteralPath (Join-Path $out 'package') -Filter '*.zip' -ErrorAction SilentlyContinue | Select-Object -First 1
                [pscustomobject]@{
                    out          = $out
                    driver       = (Test-Path -LiteralPath (Join-Path $out 'driver\helios_kmd_render.sys'))
                    mesa_x64     = (Test-Path -LiteralPath (Join-Path $out 'mesa-x64\vulkan_virtio.dll'))
                    mesa_x86     = (Test-Path -LiteralPath (Join-Path $out 'mesa-x86\vulkan_virtio.dll'))
                    package_zip  = if ($zip) { $zip.Name } else { $null }
                    package_sha  = if ($zip) { (Get-FileHash -LiteralPath $zip.FullName -Algorithm SHA256).Hash } else { $null }
                    validation   = (Test-Path -LiteralPath (Join-Path $out 'validation.json'))
                }
            }
    )

    $result.build_processes = @(
        Get-Process -ErrorAction SilentlyContinue |
            Where-Object { $_.ProcessName -match 'cargo|rustc|ninja|meson|clang|link' } |
            ForEach-Object { [pscustomobject]@{ id = $_.Id; name = $_.ProcessName; started = $_.StartTime.ToString('o') } }
    )

    $result.recent_logs = @(
        Get-ChildItem -LiteralPath 'C:\src' -Filter '*.log' -File -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime -Descending | Select-Object -First 8 | ForEach-Object {
                [pscustomobject]@{ name = $_.Name; size = $_.Length; last_write_utc = $_.LastWriteTimeUtc.ToString('o') }
            }
    )
}

$result.tasks = @(
    Get-ScheduledTask -ErrorAction SilentlyContinue |
        Where-Object { $_.TaskName -like 'Helios*' -or $_.TaskName -like 'winrun*' } |
        ForEach-Object { [pscustomobject]@{ name = $_.TaskName; state = "$($_.State)" } }
)
$result.drives = @(DriveState)

Write-Output 'WINRUN_JSON_BEGIN'
$result | ConvertTo-Json -Depth 8 -Compress
Write-Output 'WINRUN_JSON_END'
