Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$kitsBin = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
$inf2Cat = Get-ChildItem -LiteralPath $kitsBin -Filter "Inf2Cat.exe" -File -Recurse -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match "\\x64\\" } | Select-Object -First 1
$stampInf = Get-ChildItem -LiteralPath $kitsBin -Filter "stampinf.exe" -File -Recurse -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match "\\x64\\" } | Select-Object -First 1
if ($inf2Cat -and $stampInf) {
    Write-Host "Windows Driver Kit already available: $($inf2Cat.Directory.Parent.Name)"
    return
}

$winget = Get-Command winget.exe -ErrorAction SilentlyContinue
if (-not $winget) {
    throw "The GitHub runner has no WDK and no winget. Use a current hosted Windows runner or preinstall the Windows 11 WDK on the self-hosted runner."
}

foreach ($package in @("Microsoft.WindowsSDK.10.0.26100", "Microsoft.WindowsWDK.10.0.26100")) {
    Write-Host "Installing $package..."
    & $winget.Source install --id $package --exact --silent --disable-interactivity --accept-package-agreements --accept-source-agreements
    if ($LASTEXITCODE -ne 0) { throw "winget failed to install $package (exit $LASTEXITCODE)." }
}

$inf2Cat = Get-ChildItem -LiteralPath $kitsBin -Filter "Inf2Cat.exe" -File -Recurse -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match "\\x64\\" } | Select-Object -First 1
if (-not $inf2Cat) { throw "WDK installation completed, but Inf2Cat.exe is still missing." }
