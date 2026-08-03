param(
    [Parameter(Mandatory)][string]$OutputDir,
    [Parameter(Mandatory)][string]$RepoRoot,
    [string]$SourceRoot = "C:\khronos-src",
    [string]$BuildRoot = "C:\khronos-build",
    [string]$VulkanLoaderCommit = "06830240f7a70599053f47b5f10af543e8c3daf6",
    [string]$VulkanHeadersCommit = "11d6898377797e07dbd543aaaa367e4465074597",
    [string]$OpenClLoaderCommit = "18fdcd58286376124f938948aa8ed156079c1c16",
    [string]$OpenClHeadersCommit = "6fe718c31a45fe25151362a72ef041c3a1047cbd"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Clone-Pinned([string]$Url, [string]$Destination, [string]$Commit, [switch]$Recursive) {
    $arguments = @("clone", "--filter=blob:none")
    if ($Recursive) { $arguments += "--recursive" }
    $arguments += @($Url, $Destination)
    & git @arguments
    if ($LASTEXITCODE -ne 0) { throw "Failed to clone $Url." }
    & git -C $Destination checkout --detach $Commit
    if ($LASTEXITCODE -ne 0) { throw "Failed to check out $Commit in $Destination." }
    if ($Recursive) {
        & git -C $Destination submodule update --init --recursive
        if ($LASTEXITCODE -ne 0) { throw "Failed to initialize submodules in $Destination." }
    }
}

if (Test-Path -LiteralPath $SourceRoot) { Remove-Item -LiteralPath $SourceRoot -Recurse -Force }
if (Test-Path -LiteralPath $BuildRoot) { Remove-Item -LiteralPath $BuildRoot -Recurse -Force }
New-Item -ItemType Directory -Force -Path $SourceRoot,$BuildRoot,$OutputDir | Out-Null

$vkHeadersSource = Join-Path $SourceRoot "Vulkan-Headers"
$vkLoaderSource = Join-Path $SourceRoot "Vulkan-Loader"
$openClLoaderSource = Join-Path $SourceRoot "OpenCL-ICD-Loader"
$openClHeadersSource = Join-Path $SourceRoot "OpenCL-Headers"
Clone-Pinned "https://github.com/KhronosGroup/Vulkan-Headers.git" $vkHeadersSource $VulkanHeadersCommit
Clone-Pinned "https://github.com/KhronosGroup/Vulkan-Loader.git" $vkLoaderSource $VulkanLoaderCommit
Clone-Pinned "https://github.com/KhronosGroup/OpenCL-ICD-Loader.git" $openClLoaderSource $OpenClLoaderCommit -Recursive
Clone-Pinned "https://github.com/KhronosGroup/OpenCL-Headers.git" $openClHeadersSource $OpenClHeadersCommit

$vkHeadersBuild = Join-Path $BuildRoot "vk-headers"
$vkHeadersInstall = Join-Path $BuildRoot "vk-headers-install"
& cmake.exe -S $vkHeadersSource -B $vkHeadersBuild -A x64 "-DCMAKE_INSTALL_PREFIX=$vkHeadersInstall" -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded -DVULKAN_HEADERS_ENABLE_TESTS=OFF
if ($LASTEXITCODE -ne 0) { throw "Vulkan-Headers configure failed." }
& cmake.exe --install $vkHeadersBuild --config Release
if ($LASTEXITCODE -ne 0) { throw "Vulkan-Headers install failed." }

$vkLoaderBuild = Join-Path $BuildRoot "vk-loader"
$vkLoaderInstall = Join-Path $BuildRoot "vk-loader-install"
& cmake.exe -S $vkLoaderSource -B $vkLoaderBuild -A x64 `
    "-DCMAKE_INSTALL_PREFIX=$vkLoaderInstall" `
    -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded `
    "-DVULKAN_HEADERS_INSTALL_DIR=$vkHeadersInstall" `
    -DBUILD_TESTS=OFF `
    -DBUILD_WERROR=OFF
if ($LASTEXITCODE -ne 0) { throw "Vulkan-Loader configure failed." }
& cmake.exe --build $vkLoaderBuild --config Release --parallel
if ($LASTEXITCODE -ne 0) { throw "Vulkan-Loader build failed." }
& cmake.exe --install $vkLoaderBuild --config Release
if ($LASTEXITCODE -ne 0) { throw "Vulkan-Loader install failed." }

$openClBuild = Join-Path $BuildRoot "opencl-loader"
$openClInstall = Join-Path $BuildRoot "opencl-loader-install"
& cmake.exe -S $openClLoaderSource -B $openClBuild -A x64 `
    "-DCMAKE_INSTALL_PREFIX=$openClInstall" `
    -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded `
    "-DOPENCL_ICD_LOADER_HEADERS_DIR=$openClHeadersSource" `
    -DOPENCL_ICD_LOADER_BUILD_SHARED_LIBS=ON `
    -DENABLE_OPENCL_LAYERS=OFF `
    -DBUILD_TESTING=OFF
if ($LASTEXITCODE -ne 0) { throw "OpenCL ICD Loader configure failed." }
& cmake.exe --build $openClBuild --config Release --parallel
if ($LASTEXITCODE -ne 0) { throw "OpenCL ICD Loader build failed." }
& cmake.exe --install $openClBuild --config Release
if ($LASTEXITCODE -ne 0) { throw "OpenCL ICD Loader install failed." }

$vulkanDll = Get-ChildItem -LiteralPath @($vkLoaderInstall, $vkLoaderBuild) -Filter "vulkan-1.dll" -File -Recurse | Select-Object -First 1
$openClDll = Get-ChildItem -LiteralPath @($openClInstall, $openClBuild) -Filter "OpenCL.dll" -File -Recurse | Select-Object -First 1
if (-not $vulkanDll) { throw "Installed Vulkan loader DLL was not found." }
if (-not $openClDll) { throw "Installed OpenCL loader DLL was not found." }
Copy-Item -LiteralPath $vulkanDll.FullName -Destination (Join-Path $OutputDir "vulkan-1.dll") -Force
Copy-Item -LiteralPath $openClDll.FullName -Destination (Join-Path $OutputDir "OpenCL.dll") -Force

$vulkanLibrary = Get-ChildItem -LiteralPath @($vkLoaderInstall, $vkLoaderBuild) -Filter "vulkan-1.lib" -File -Recurse | Select-Object -First 1
$openClLibrary = Get-ChildItem -LiteralPath @($openClInstall, $openClBuild) -Filter "OpenCL.lib" -File -Recurse | Select-Object -First 1
if (-not $vulkanLibrary -or -not $openClLibrary) { throw "A loader import library required by the smoke probes was not found." }
& (Join-Path $RepoRoot "ci\windows\Build-SmokeTests.ps1") `
    -RepoRoot $RepoRoot `
    -OutputDir (Join-Path $OutputDir "smoke") `
    -VulkanInclude (Join-Path $vkHeadersInstall "include") `
    -VulkanLibrary $vulkanLibrary.FullName `
    -OpenClInclude $openClHeadersSource `
    -OpenClLibrary $openClLibrary.FullName
if ($LASTEXITCODE -ne 0) { throw "Smoke probe build failed." }

$pins = [ordered]@{
    vulkanLoader = $VulkanLoaderCommit
    vulkanHeaders = $VulkanHeadersCommit
    openClLoader = $OpenClLoaderCommit
    openClHeaders = $OpenClHeadersCommit
}
$pins | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $OutputDir "loader-commits.json") -Encoding ascii
$licenseRoot = Join-Path $OutputDir "licenses"
New-Item -ItemType Directory -Force -Path $licenseRoot | Out-Null
Copy-Item -LiteralPath (Join-Path $vkLoaderSource "LICENSE.txt") -Destination (Join-Path $licenseRoot "Vulkan-Loader-LICENSE.txt") -Force
Copy-Item -LiteralPath (Join-Path $vkHeadersSource "LICENSE.md") -Destination (Join-Path $licenseRoot "Vulkan-Headers-LICENSE.md") -Force
Copy-Item -LiteralPath (Join-Path $openClLoaderSource "LICENSE") -Destination (Join-Path $licenseRoot "OpenCL-ICD-Loader-LICENSE") -Force
Copy-Item -LiteralPath (Join-Path $openClHeadersSource "LICENSE") -Destination (Join-Path $licenseRoot "OpenCL-Headers-LICENSE") -Force
Write-Host "Khronos loader artifact staged at $OutputDir"
