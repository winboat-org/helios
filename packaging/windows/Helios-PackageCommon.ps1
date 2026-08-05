Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Assert-HeliosAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "Run this script from an elevated PowerShell window."
    }
}

function Get-HeliosSha256([Parameter(Mandatory)][string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "File not found: $Path" }
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToUpperInvariant()
}

function Read-HeliosManifest([Parameter(Mandatory)][string]$BundleRoot) {
    $manifestPath = Join-Path $BundleRoot "manifest.json"
    if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
        throw "Package manifest is missing: $manifestPath"
    }
    $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    if ($manifest.schemaVersion -ne 1 -or $manifest.architecture -ne "x64") {
        throw "Unsupported Helios package schema or architecture."
    }
    return $manifest
}

function Test-HeliosManifest([Parameter(Mandatory)][string]$BundleRoot, [Parameter(Mandatory)]$Manifest) {
    $root = [IO.Path]::GetFullPath($BundleRoot).TrimEnd("\") + "\"
    foreach ($entry in @($Manifest.files)) {
        $relative = [string]$entry.path
        if ([IO.Path]::IsPathRooted($relative) -or $relative -match "(^|[\\/])\.\.([\\/]|$)") {
            throw "Unsafe path in package manifest: $relative"
        }
        $full = [IO.Path]::GetFullPath((Join-Path $BundleRoot ($relative -replace "/", "\")))
        if (-not $full.StartsWith($root, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Package manifest path escapes the bundle root: $relative"
        }
        $actual = Get-HeliosSha256 $full
        if ($actual -ne ([string]$entry.sha256).ToUpperInvariant()) {
            throw "Package hash mismatch for $relative. Expected $($entry.sha256), got $actual."
        }
        if ((Get-Item -LiteralPath $full).Length -ne [int64]$entry.size) {
            throw "Package size mismatch for $relative."
        }
    }
}

function Get-HeliosDeviceInstanceId {
    # Prefer a currently present device. WinBoat can change QEMU's device set
    # between bootstrap and accelerated boots, leaving a non-present PCI
    # instance in CIM that must not receive the WGL registration.
    $presentDevices = @(Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
        Where-Object { $_.InstanceId -like "PCI\VEN_1AF4&DEV_1050*" })
    $device = $presentDevices |
        Where-Object { $_.FriendlyName -like "Helios vGPU Render Adapter*" } |
        Select-Object -First 1
    if (-not $device) {
        $device = $presentDevices | Select-Object -First 1
    }
    if ($device) { return [string]$device.InstanceId }

    $device = Get-CimInstance Win32_PnPEntity |
        Where-Object { $_.PNPDeviceID -like "PCI\VEN_1AF4&DEV_1050*" -and $_.Name -like "Helios vGPU Render Adapter*" } |
        Select-Object -First 1
    if (-not $device) {
        $device = Get-CimInstance Win32_PnPEntity |
            Where-Object { $_.PNPDeviceID -like "PCI\VEN_1AF4&DEV_1050*" } |
            Select-Object -First 1
    }
    if (-not $device) { throw "The Helios virtio-gpu PCI device (1af4:1050) was not found." }
    return [string]$device.PNPDeviceID
}

function Get-HeliosDisplayClassKey([Parameter(Mandatory)][string]$InstanceId) {
    $root = "HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}"

    # The Enum key's Driver value identifies the exact software key for this
    # PCI instance. MatchingDeviceId alone is ambiguous when Windows retains a
    # stale instance of the same adapter after its PCI address changes.
    $enumPath = "HKLM:\SYSTEM\CurrentControlSet\Enum\$InstanceId"
    $driverKeyName = ""
    if (Test-Path -LiteralPath $enumPath) {
        $enumProperties = Get-ItemProperty -LiteralPath $enumPath -Name "Driver" -ErrorAction SilentlyContinue
        if ($enumProperties -and $enumProperties.PSObject.Properties["Driver"]) {
            $driverKeyName = [string]$enumProperties.Driver
        }
    }
    if (-not $driverKeyName) {
        $driverProperty = Get-PnpDeviceProperty -InstanceId $InstanceId -KeyName DEVPKEY_Device_Driver -ErrorAction SilentlyContinue
        if ($driverProperty -and $driverProperty.PSObject.Properties["Data"]) {
            $driverKeyName = [string]$driverProperty.Data
        }
    }
    if ($driverKeyName -match "^\{4d36e968-e325-11ce-bfc1-08002be10318\}\\(\d{4})$") {
        $exactPath = Join-Path $root $Matches[1]
        if (Test-Path -LiteralPath $exactPath) { return $exactPath }
    }
    throw "Could not locate the display adapter software key for the present device instance $InstanceId."
}

function Get-HeliosActiveInf([Parameter(Mandatory)][string]$InstanceId) {
    $property = Get-PnpDeviceProperty -InstanceId $InstanceId -KeyName DEVPKEY_Device_DriverInfPath -ErrorAction SilentlyContinue
    if ($property -and $property.PSObject.Properties["Data"] -and $property.Data) {
        return [string]$property.Data
    }
    return ""
}

function Test-HeliosViogpudoDriver([Parameter(Mandatory)][string]$InfName) {
    $leafName = [IO.Path]::GetFileName($InfName)
    if ($leafName -ne $InfName -or $leafName -notmatch "^[A-Za-z0-9._-]+\.inf$") { return $false }
    $infPath = Join-Path $env:windir "INF\$leafName"
    if (-not (Test-Path -LiteralPath $infPath -PathType Leaf)) { return $false }
    return (Get-Content -LiteralPath $infPath -Raw) -match "(?i)\bviogpudo(?:\.inf|\.sys)?\b"
}

function Get-HeliosRegistrySnapshot([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string]$Name) {
    if (-not (Test-Path -LiteralPath $Path)) {
        return [ordered]@{ exists = $false; kind = $null; value = $null }
    }
    $key = Get-Item -LiteralPath $Path
    if ($key.GetValueNames() -notcontains $Name) {
        return [ordered]@{ exists = $false; kind = $null; value = $null }
    }
    return [ordered]@{
        exists = $true
        kind = $key.GetValueKind($Name).ToString()
        value = $key.GetValue($Name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    }
}

function Restore-HeliosRegistrySnapshot([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string]$Name, [Parameter(Mandatory)]$Snapshot) {
    if ([bool]$Snapshot.exists) {
        New-Item -Path $Path -Force | Out-Null
        New-ItemProperty -LiteralPath $Path -Name $Name -Value $Snapshot.value -PropertyType ([string]$Snapshot.kind) -Force | Out-Null
    } elseif (Test-Path -LiteralPath $Path) {
        Remove-ItemProperty -LiteralPath $Path -Name $Name -ErrorAction SilentlyContinue
    }
}

function Invoke-HeliosNative(
    [Parameter(Mandatory)][string]$FilePath,
    [Parameter(Mandatory)][string[]]$Arguments,
    [int[]]$SuccessExitCodes = @(0),
    [switch]$WaitForProcess
) {
    if ($WaitForProcess) {
        # Windows PowerShell can return immediately for GUI-subsystem programs
        # without defining $LASTEXITCODE. Start-Process supplies a reliable exit
        # code and waits for bootstrapper child activity to complete.
        $process = Start-Process -FilePath $FilePath -ArgumentList $Arguments -NoNewWindow -Wait -PassThru
        $exitCode = $process.ExitCode
    } else {
        $output = & $FilePath @Arguments 2>&1
        $exitCode = $LASTEXITCODE
        if ($output) { $output | ForEach-Object { Write-Host $_ } }
    }
    if ($SuccessExitCodes -notcontains $exitCode) {
        throw "$FilePath $($Arguments -join ' ') failed with exit code $exitCode."
    }
}

function Test-HeliosTestSigningEnabled {
    if (-not ("Helios.Package.CodeIntegrity" -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace Helios.Package {
    [StructLayout(LayoutKind.Sequential)]
    public struct CodeIntegrityInformation {
        public UInt32 Length;
        public UInt32 Options;
    }
    public static class CodeIntegrity {
        [DllImport("ntdll.dll")]
        private static extern UInt32 NtQuerySystemInformation(
            UInt32 infoClass,
            ref CodeIntegrityInformation info,
            UInt32 infoLength,
            IntPtr returnLength);

        public static bool TestSigningEnabled() {
            var info = new CodeIntegrityInformation();
            info.Length = (UInt32)Marshal.SizeOf(info);
            UInt32 status = NtQuerySystemInformation(103, ref info, info.Length, IntPtr.Zero);
            if (status != 0) throw new InvalidOperationException("NtQuerySystemInformation failed: 0x" + status.ToString("X8"));
            return (info.Options & 0x2U) != 0;
        }
    }
}
'@
    }
    return [Helios.Package.CodeIntegrity]::TestSigningEnabled()
}

function Test-HeliosSecureBootEnabled {
    try { return [bool](Confirm-SecureBootUEFI -ErrorAction Stop) } catch { return $false }
}

function Write-HeliosJson(
    [Parameter(Mandatory)]$Value,
    [Parameter(Mandatory)][string]$Path,
    [ValidateSet("UTF8", "ASCII")][string]$Encoding = "UTF8"
) {
    $directory = Split-Path -Parent $Path
    New-Item -ItemType Directory -Force -Path $directory | Out-Null
    $temporary = "$Path.tmp"
    $Value | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $temporary -Encoding $Encoding
    Move-Item -LiteralPath $temporary -Destination $Path -Force
}
