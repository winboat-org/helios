param(
    [switch]$RunSmokeTests,
    [switch]$AllowPendingReboot
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Helios-PackageCommon.ps1")

$statePath = Join-Path $env:ProgramData "Helios\install-state.json"
if (-not (Test-Path -LiteralPath $statePath -PathType Leaf)) {
    throw "No package-managed Helios installation was found at $statePath."
}
$state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
$failures = [Collections.Generic.List[string]]::new()
$instanceId = ""
$classKey = ""

foreach ($entry in @($state.runtimeFiles)) {
    if (-not (Test-Path -LiteralPath ([string]$entry.path) -PathType Leaf)) {
        $failures.Add("Missing runtime file: $($entry.path)")
        continue
    }
    $actual = Get-HeliosSha256 ([string]$entry.path)
    if ($actual -ne ([string]$entry.sha256).ToUpperInvariant()) {
        $failures.Add("Runtime hash mismatch: $($entry.path)")
    }
}

try {
    $instanceId = Get-HeliosDeviceInstanceId
    $classKey = Get-HeliosDisplayClassKey $instanceId
    $device = Get-PnpDevice -InstanceId $instanceId -ErrorAction Stop
    Write-Host "Driver: $($device.FriendlyName) [$($device.Status)]"
    if ($device.Status -ne "OK") {
        if ($AllowPendingReboot) {
            Write-Warning "Helios PnP status is $($device.Status); the installer has not rebooted yet."
        } else {
            $failures.Add("Helios PnP device status is $($device.Status).")
        }
    }
    $signedDriver = Get-CimInstance Win32_PnPSignedDriver | Where-Object { $_.DeviceID -eq $instanceId } | Select-Object -First 1
    if ($signedDriver) {
        Write-Host "Driver provider/version: $($signedDriver.DriverProviderName) $($signedDriver.DriverVersion)"
        if ($signedDriver.DriverProviderName -notlike "Helios*") {
            $failures.Add("The active display driver provider is $($signedDriver.DriverProviderName), not Helios.")
        }
    }
} catch {
    $failures.Add($_.Exception.Message)
}

$vulkanRegistry = "HKLM:\SOFTWARE\Khronos\Vulkan\Drivers"
$vulkanValue = if (Test-Path -LiteralPath $vulkanRegistry) { (Get-Item -LiteralPath $vulkanRegistry).GetValue([string]$state.vulkanManifest, $null) } else { $null }
if ($null -eq $vulkanValue -or [int]$vulkanValue -ne 0) {
    $failures.Add("The Vulkan ICD manifest is not enabled in the machine registry.")
} else { Write-Host "Vulkan: registered $($state.vulkanManifest)" }

$openGlDriver = if ($classKey -and (Test-Path -LiteralPath $classKey)) { (Get-Item -LiteralPath $classKey).GetValue("OpenGLDriverName", $null) } else { $null }
if (-not $openGlDriver -or ([string]$openGlDriver -ine (Join-Path ([string]$state.installRoot) "runtime\mesa\libgallium_wgl.dll"))) {
    $failures.Add("The Helios OpenGL ICD is not registered on the display adapter.")
} else { Write-Host "OpenGL: registered $openGlDriver" }

$openClRegistry = "HKLM:\SOFTWARE\Khronos\OpenCL\Vendors"
$openClValue = if (Test-Path -LiteralPath $openClRegistry) { (Get-Item -LiteralPath $openClRegistry).GetValue([string]$state.openClVendor, $null) } else { $null }
if ($null -eq $openClValue -or [int]$openClValue -ne 0) {
    $failures.Add("The CLVK vendor DLL is not enabled in the OpenCL registry.")
} else { Write-Host "OpenCL: registered $($state.openClVendor)" }

if ($RunSmokeTests) {
    $smokeRoot = Join-Path ([string]$state.installRoot) "runtime\smoke"
    $tests = @(
        [ordered]@{ name = "Vulkan"; exe = "vulkan-smoke.exe"; arguments = @() },
        [ordered]@{ name = "Direct3D 11"; exe = "d3d11-smoke.exe"; arguments = @() },
        [ordered]@{ name = "OpenGL"; exe = "opengl-smoke.exe"; arguments = @() },
        [ordered]@{ name = "OpenCL"; exe = "opencl-smoke.exe"; arguments = @() },
        [ordered]@{
            name = "OpenGL/OpenCL sharing with a D3D11 context"
            exe = "opencl-gl-sharing-smoke.exe"
            arguments = @("rgba16f", "d3d11-context")
        }
    )
    foreach ($test in $tests) {
        $executable = Join-Path $smokeRoot $test.exe
        if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
            Write-Warning "$($test.name) smoke probe is not present in this bundle."
            continue
        }
        Write-Host "Running $($test.name) smoke probe..."
        $probeArguments = @($test.arguments)
        & $executable @probeArguments
        if ($LASTEXITCODE -ne 0) { $failures.Add("$($test.name) smoke probe failed with exit code $LASTEXITCODE.") }
    }
}

if ($failures.Count -gt 0) {
    foreach ($failure in $failures) { Write-Error $failure -ErrorAction Continue }
    throw "Helios verification failed with $($failures.Count) problem(s)."
}
Write-Host "Helios Vulkan, Direct3D 11, OpenGL, and OpenCL registrations are healthy."
