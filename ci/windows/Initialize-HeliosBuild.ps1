Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Import-VisualStudioEnvironment {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) {
        throw "vswhere.exe was not found; Visual Studio 2022 with C++ tools is required."
    }

    $installation = (& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath).Trim()
    if (-not $installation) {
        throw "Visual Studio 2022 with the x64 C++ toolchain was not found."
    }

    $devCmd = Join-Path $installation "Common7\Tools\VsDevCmd.bat"
    $environment = & cmd.exe /s /c "`"$devCmd`" -no_logo -arch=x64 -host_arch=x64 && set"
    if ($LASTEXITCODE -ne 0) {
        throw "VsDevCmd.bat failed with exit code $LASTEXITCODE."
    }
    foreach ($line in $environment) {
        $parts = $line -split "=", 2
        if ($parts.Count -eq 2) {
            Set-Item -LiteralPath "Env:$($parts[0])" -Value $parts[1]
        }
    }
}

function Find-WindowsKitTool([Parameter(Mandatory)][string]$Name) {
    $kitsBin = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
    if (-not (Test-Path -LiteralPath $kitsBin -PathType Container)) {
        throw "Windows Kits bin directory was not found at $kitsBin."
    }
    $tools = @(Get-ChildItem -LiteralPath $kitsBin -Filter $Name -File -Recurse |
        Sort-Object { [version]($_.Directory.Parent.Name -replace "[^0-9.]", "") } -Descending |
        Where-Object { $_.Directory.Name -in @("x64", "x86") })
    $tool = $tools | Where-Object { $_.Directory.Name -eq "x64" } | Select-Object -First 1
    if (-not $tool) { $tool = $tools | Select-Object -First 1 }
    if (-not $tool) {
        throw "$Name was not found below $kitsBin. Install the Windows 11 WDK."
    }
    return $tool.FullName
}

function Find-WindowsKitInclude {
    $includeRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\Include"
    $candidate = Get-ChildItem -LiteralPath $includeRoot -Directory |
        Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName "km\ntddk.h") } |
        Sort-Object { [version]($_.Name -replace "[^0-9.]", "") } -Descending |
        Select-Object -First 1
    if (-not $candidate) {
        throw "A Windows 11 WDK include tree was not found below $includeRoot."
    }
    return $candidate.FullName
}

function Assert-Command([Parameter(Mandatory)][string]$Name) {
    $command = Get-Command $Name -ErrorAction SilentlyContinue
    if (-not $command) { throw "Required command is missing from PATH: $Name" }
    return $command.Source
}
