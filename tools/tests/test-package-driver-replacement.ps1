$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot '../../packaging/windows/Helios-PackageCommon.ps1')
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('helios-driver-replacement-' + [Guid]::NewGuid().ToString('N'))
$originalWindows = $env:windir
New-Item -ItemType Directory -Path $fixture | Out-Null
$env:windir = Join-Path $fixture 'Windows'
$store = Join-Path $env:windir 'System32/DriverStore/FileRepository/helios_kmd_render.inf_fixture'
$published = Join-Path $env:windir 'INF/oem7.inf'
$sourceInf = Join-Path $fixture 'helios_kmd_render.inf'
$statePath = Join-Path $fixture 'state.json'
$script:boot = [DateTime]'2026-01-01T00:00:00Z'
$script:failDelete = $false
$script:calls = [Collections.Generic.List[string]]::new()
$names = @('helios_kmd_render.sys','helios_umd.dll','helios_umd12.dll','helios_umd32.dll','helios_umd12_32.dll')

function Assert([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Assert-Throws([scriptblock]$Action, [string]$Message) {
    $caught = $false
    try { & $Action | Out-Null } catch { $caught = $true }
    Assert $caught $Message
}
function Get-HeliosActiveInf { return 'oem7.inf' }
function Get-HeliosDisplayClassKey { return 'HKLM:\Fixture' }
function Get-Item {
    param([string]$LiteralPath)
    if ($LiteralPath -eq 'HKLM:\Fixture') {
        $key = [pscustomobject]@{}
        $key | Add-Member ScriptMethod GetValue { param($Name,$Default); return @((Join-Path $script:store 'helios_umd.dll')) * 4 }
        return $key
    }
    return Microsoft.PowerShell.Management\Get-Item -LiteralPath $LiteralPath
}
function Get-CimInstance { return [pscustomobject]@{LastBootUpTime=$script:boot} }
function Invoke-HeliosNative {
    param([string]$FilePath,[string[]]$Arguments,[int[]]$SuccessExitCodes=@(0))
    $script:calls.Add($Arguments[0])
    Assert ($FilePath -eq 'pnputil.exe') 'Only PnP package operations may mutate the driver.'
    Assert ($Arguments[1] -eq 'oem7.inf') 'Only the selected managed INF may be changed.'
    if ($Arguments[0] -eq '/export-driver') {
        Copy-Item -LiteralPath $script:store -Destination (Join-Path $Arguments[2] 'exported') -Recurse
    } elseif ($Arguments[0] -eq '/delete-driver') {
        $saved = Get-Content -LiteralPath $script:statePath -Raw | ConvertFrom-Json
        Assert ($saved.pendingDriverReplacement.phase -eq 'exported') 'Backup identity must be durable before deletion.'
        Assert (Test-Path -LiteralPath $saved.pendingDriverReplacement.journal) 'Backup journal must precede deletion.'
        if ($script:failDelete) { throw 'Injected PnP delete failure' }
        Remove-Item -LiteralPath $script:published
    } else { throw 'Unexpected PnP command' }
}

try {
    New-Item -ItemType Directory -Path $store,(Split-Path -Parent $published) -Force | Out-Null
    Set-Content -LiteralPath $sourceInf -Value 'fixture INF'
    Copy-Item -LiteralPath $sourceInf -Destination $published
    Copy-Item -LiteralPath $sourceInf -Destination (Join-Path $store 'helios_kmd_render.inf')
    foreach ($name in $names) { Set-Content -LiteralPath (Join-Path $store $name) -Value ('old ' + $name) }
    Set-Content -LiteralPath (Join-Path $store 'helios_kmd_render.cat') -Value 'retained catalog'
    $old = [pscustomobject]@{instanceId='fixture-device';activeInf='oem7.inf';activeInfSha256=Get-HeliosSha256 $published}
    $files = @($names | ForEach-Object { @{name=$_;sha256=Get-HeliosSha256 (Join-Path $store $_)} })
    Assert ($null -eq (Get-HeliosSameInfReplacement $old $files 'fixture-device' $sourceInf)) 'Identical images must not trigger deletion.'
    $files[0].sha256 = 'NEW'
    Assert-Throws { Get-HeliosSameInfReplacement $old $files 'another-device' $sourceInf } 'An unrelated device must be refused.'
    $old.activeInfSha256 = 'CHANGED'
    Assert-Throws { Get-HeliosSameInfReplacement $old $files 'fixture-device' $sourceInf } 'Managed INF drift must be refused.'
    $old.activeInfSha256 = Get-HeliosSha256 $published
    $state = [ordered]@{instanceId='fixture-device';driverFiles=$files}
    $script:failDelete = $true
    Assert-Throws { Remove-HeliosSameInfPackage $old $state $statePath $fixture $sourceInf } 'A real deletion failure must propagate.'
    $retained = $state.pendingDriverReplacement
    Assert ($retained.files.Count -eq 7) 'The complete old package, including its catalog, must remain retained.'
    $before = @(Get-ChildItem -LiteralPath (Join-Path $fixture 'driver-backups') -Directory).Count
    $script:failDelete = $false
    $resume = [pscustomobject]@{pendingDriverReplacement=$retained}
    Assert (Remove-HeliosSameInfPackage $resume $state $statePath $fixture $sourceInf) 'Successful removal must require a boot boundary.'
    Assert (@(Get-ChildItem -LiteralPath (Join-Path $fixture 'driver-backups') -Directory).Count -eq $before) 'Resume must retain the original export.'
    Assert (Remove-HeliosSameInfPackage $resume $state $statePath $fixture $sourceInf) 'Same-boot completion must remain pending.'
    $script:boot = $script:boot.AddMinutes(1)
    Set-Content -LiteralPath $retained.files[0].path -Value 'tampered'
    Assert-Throws { Remove-HeliosSameInfPackage $resume $state $statePath $fixture $sourceInf } 'Backup tampering must prevent resume.'
    $leaf = [IO.Path]::GetFileName($retained.files[0].path)
    Copy-Item -LiteralPath (Join-Path $store $leaf) -Destination $retained.files[0].path -Force
    Copy-Item -LiteralPath $sourceInf -Destination $published
    Assert-Throws { Remove-HeliosSameInfPackage $resume $state $statePath $fixture $sourceInf } 'A still-staged old package must prevent completion.'
    Remove-Item -LiteralPath $published
    Assert (-not (Remove-HeliosSameInfPackage $resume $state $statePath $fixture $sourceInf)) 'A changed boot with the old package removed may continue.'
    Assert ($null -eq $state.pendingDriverReplacement) 'Successful resume must clear the pending marker.'
    Write-Host 'Package driver replacement fixtures passed: exact identity, complete export, failure/resume, hash drift and boot boundary.'
} finally {
    $env:windir = $originalWindows
    Remove-Item -LiteralPath $fixture -Recurse -Force
}
