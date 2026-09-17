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

# Copy a payload tree, skipping files whose destination already has identical
# content. Re-applying the same package cannot overwrite runtime DLLs that
# running processes have loaded (vulkan_virtio.dll is the recorded case), and it
# does not need to: same packageId means the bytes are identical. A genuine
# version change installs under a new installRoot, so nothing is skipped there.
function Copy-HeliosTreeIfChanged(
    [Parameter(Mandatory)][string]$Source,
    [Parameter(Mandatory)][string]$Destination,
    # A same-packageId re-apply has byte-identical CODE but the payload copies
    # are re-signed with a fresh per-build certificate, so the hashes differ and
    # the loaded runtime DLLs cannot be overwritten. In that case any existing
    # destination is already the right code, so skip it.
    [switch]$SkipExisting
) {
    if (-not (Test-Path -LiteralPath $Source -PathType Container)) { return }
    foreach ($file in (Get-ChildItem -LiteralPath $Source -File -Recurse)) {
        $relative = $file.FullName.Substring($Source.Length).TrimStart("\")
        $target = Join-Path $Destination $relative
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $target) | Out-Null
        if ((Test-Path -LiteralPath $target -PathType Leaf)) {
            if ($SkipExisting) { continue }
            if ((Get-HeliosSha256 $target) -eq (Get-HeliosSha256 $file.FullName)) { continue }
        }
        Copy-Item -LiteralPath $file.FullName -Destination $target -Force
    }
}

function Assert-HeliosPeArchitecture(
    [Parameter(Mandatory)][string]$Path,
    [Parameter(Mandatory)][ValidateSet("x64", "x86")][string]$Architecture
) {
    $stream = [IO.File]::OpenRead($Path)
    $reader = [IO.BinaryReader]::new($stream)
    try {
        if ($stream.Length -lt 64 -or $reader.ReadUInt16() -ne 0x5A4D) {
            throw "Not a DOS/PE image: $Path"
        }
        $stream.Position = 0x3C
        $peOffset = $reader.ReadUInt32()
        if ($peOffset -gt $stream.Length - 26) { throw "Invalid PE header offset: $Path" }
        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550) { throw "Invalid PE signature: $Path" }
        $machine = $reader.ReadUInt16()
        $expected = if ($Architecture -eq "x86") { 0x14C } else { 0x8664 }
        if ($machine -ne $expected) {
            throw ("Expected {0} PE image, found machine 0x{1:X4}: {2}" -f $Architecture, $machine, $Path)
        }
    } finally {
        $reader.Dispose()
        $stream.Dispose()
    }
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
        Where-Object { $_.FriendlyName -like "Helios*" } |
        Select-Object -First 1
    if (-not $device) {
        $device = $presentDevices | Select-Object -First 1
    }
    if ($device) { return [string]$device.InstanceId }

    $device = Get-CimInstance Win32_PnPEntity |
        Where-Object { $_.PNPDeviceID -like "PCI\VEN_1AF4&DEV_1050*" -and $_.Name -like "Helios*" } |
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
    # Match the driver's IDENTITY, never a bare mention of its name. The Helios
    # INF itself names viogpudo in a comment, so `(?i)\bviogpudo\b` matched the
    # Helios package and made the installer try to replace itself.
    return (Get-Content -LiteralPath $infPath -Raw) -match "(?i)\bviogpudo\.(inf|sys)\b|\bVioGpuDod\b"
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

function Ensure-HeliosRegistryKey([Parameter(Mandatory)][string]$Path) {
    # Registry New-Item -Force REPLACES existing keys and all their contents.
    # Build missing parents individually without Force; a racing creator must
    # fail safely rather than erase that creator's values or children.
    if (Test-Path -LiteralPath $Path) { return }
    $parent = Split-Path -Path $Path -Parent
    if (-not $parent -or $parent -eq $Path) { throw "No existing registry root for $Path." }
    Ensure-HeliosRegistryKey $parent
    New-Item -Path $Path -ErrorAction Stop | Out-Null
}

function Restore-HeliosRegistrySnapshot([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string]$Name, [Parameter(Mandatory)]$Snapshot) {
    if ([bool]$Snapshot.exists) {
        Ensure-HeliosRegistryKey $Path
        New-ItemProperty -LiteralPath $Path -Name $Name -Value $Snapshot.value -PropertyType ([string]$Snapshot.kind) -Force | Out-Null
    } elseif (Test-Path -LiteralPath $Path) {
        Remove-ItemProperty -LiteralPath $Path -Name $Name -ErrorAction SilentlyContinue
    }
}

function Restore-HeliosDirect3DAfterRemoval(
    [Parameter(Mandatory)]$State,
    [string]$CurrentInf,
    [string]$CurrentClassKey,
    [string]$CurrentInfSha256
) {
    # Older bundles had no WoW64 UMDs and saved neither snapshot.
    if (-not $State.PSObject.Properties["installedDirect3D"]) { return }
    $restoredKey = ""
    if ($CurrentInf -and $CurrentClassKey -and $State.PSObject.Properties["previousDirect3D"] -and
        $State.previousDirect3D.activeInf -and $CurrentInf -ieq $State.previousDirect3D.activeInf -and
        $CurrentInf -ine $State.activeInf -and
        $State.previousDirect3D.infSha256 -and $CurrentInfSha256 -ieq $State.previousDirect3D.infSha256) {
        foreach ($name in @("UserModeDriverName", "UserModeDriverNameWoW", "InstalledDisplayDrivers")) {
            Restore-HeliosRegistrySnapshot $CurrentClassKey $name $State.previousDirect3D.values.$name
        }
        $restoredKey = $CurrentClassKey
    }
    $wowSnapshot = $State.installedDirect3D.PSObject.Properties["UserModeDriverNameWoW"]
    if (-not $wowSnapshot -or -not $wowSnapshot.Value.exists) { return }
    $ownedPaths = @($wowSnapshot.Value.value)
    $ownedNames = @($ownedPaths | ForEach-Object { [IO.Path]::GetFileNameWithoutExtension([string]$_) } | Select-Object -Unique)
    $keys = @(@([string]$State.classKey, $CurrentClassKey) | Where-Object { $_ } | Select-Object -Unique)
    foreach ($keyPath in $keys) {
        if ($keyPath -ieq $restoredKey -or -not (Test-Path -LiteralPath $keyPath)) { continue }
        $wow = Get-HeliosRegistrySnapshot $keyPath "UserModeDriverNameWoW"
        if ($wow.exists) {
            $paths = @($wow.value)
            $owned = @($paths | Where-Object { $_ -in $ownedPaths })
            if ($owned.Count -gt 0 -and $owned.Count -eq $paths.Count) {
                Remove-ItemProperty -LiteralPath $keyPath -Name "UserModeDriverNameWoW" -ErrorAction Stop
            } elseif ($owned.Count -gt 0) {
                # A REG_MULTI_SZ is indexed by API; filtering individual slots
                # would change their meaning. Preserve a mixed/manual override.
                Write-Warning "Keeping mixed WoW64 registration on ${keyPath}; only some slots reference the removed package."
            }
        }
        # Do not delete the inventory entry of a newly selected driver that
        # uses the same filename in a different DriverStore directory.
        $registeredNames = @(
            foreach ($name in @("UserModeDriverName", "UserModeDriverNameWoW")) {
                $snapshot = Get-HeliosRegistrySnapshot $keyPath $name
                if ($snapshot.exists) {
                    @($snapshot.value) | ForEach-Object { [IO.Path]::GetFileNameWithoutExtension([string]$_) }
                }
            }
        )
        $inventory = Get-HeliosRegistrySnapshot $keyPath "InstalledDisplayDrivers"
        if ($inventory.exists) {
            $remaining = @($inventory.value | Where-Object { $_ -notin $ownedNames -or $_ -in $registeredNames })
            if ($remaining.Count -ne @($inventory.value).Count) {
                if ($remaining.Count -eq 0) {
                    Remove-ItemProperty -LiteralPath $keyPath -Name "InstalledDisplayDrivers" -ErrorAction Stop
                } else {
                    New-ItemProperty -LiteralPath $keyPath -Name "InstalledDisplayDrivers" -Value $remaining -PropertyType MultiString -Force | Out-Null
                }
            }
        }
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

# Machine-readable progress for the GUI installer. Write-Host (not Write-Output)
# keeps it out of the pipeline; the GUI's stdout reader consumes any line that
# begins with this marker and never shows it raw. Plain `powershell -File` users
# just see the marker as ordinary console text, which is harmless.
function Write-HeliosProgress(
    [Parameter(Mandatory)][int]$Percent,
    [Parameter(Mandatory)][string]$Message
) {
    if ($Percent -lt 0) { $Percent = 0 }
    if ($Percent -gt 100) { $Percent = 100 }
    Write-Host "HELIOS-PROGRESS $Percent $Message"
}

# Old bundles predate explicit publisher metadata. Keep their known identity for
# upgrade/uninstall verification; new bundles carry the publisher from the source.
function Get-HeliosPackagePublisher($Package) {
    if ($Package.PSObject.Properties["publisher"] -and
        -not [string]::IsNullOrWhiteSpace([string]$Package.publisher)) {
        return [string]$Package.publisher
    }
    return "Helios Project"
}
