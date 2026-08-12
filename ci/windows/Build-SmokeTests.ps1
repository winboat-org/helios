param(
    [Parameter(Mandatory)][string]$RepoRoot,
    [Parameter(Mandatory)][string]$OutputDir,
    [Parameter(Mandatory)][string]$VulkanInclude,
    [Parameter(Mandatory)][string]$VulkanLibrary,
    [Parameter(Mandatory)][string]$OpenClInclude,
    [Parameter(Mandatory)][string]$OpenClLibrary
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Initialize-HeliosBuild.ps1")
Import-VisualStudioEnvironment
Assert-Command "cl.exe" | Out-Null
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$source = Join-Path $RepoRoot "packaging\windows\probes"

& cl.exe /nologo /O2 /W4 /MT (Join-Path $source "vulkan-smoke.c") "/I$VulkanInclude" "/Fe:$(Join-Path $OutputDir 'vulkan-smoke.exe')" /link $VulkanLibrary
if ($LASTEXITCODE -ne 0) { throw "Vulkan smoke probe compilation failed." }
& cl.exe /nologo /O2 /W4 /MT /EHsc `
    (Join-Path $RepoRoot "tools\vk_surface_recreate_probe.cpp") `
    "/I$VulkanInclude" `
    "/Fe:$(Join-Path $OutputDir 'vulkan-wsi-probe.exe')" `
    /link $VulkanLibrary user32.lib gdi32.lib
if ($LASTEXITCODE -ne 0) { throw "Vulkan WSI probe compilation failed." }
& cl.exe /nologo /O2 /W4 /MT /EHsc (Join-Path $source "d3d11-smoke.cpp") "/Fe:$(Join-Path $OutputDir 'd3d11-smoke.exe')" /link d3d11.lib dxgi.lib
if ($LASTEXITCODE -ne 0) { throw "D3D11 smoke probe compilation failed." }
& cl.exe /nologo /O2 /W4 /MT (Join-Path $source "opengl-smoke.c") "/Fe:$(Join-Path $OutputDir 'opengl-smoke.exe')" /link opengl32.lib gdi32.lib user32.lib
if ($LASTEXITCODE -ne 0) { throw "OpenGL smoke probe compilation failed." }
& cl.exe /nologo /O2 /W4 /MT (Join-Path $source "opencl-smoke.c") "/I$OpenClInclude" "/Fe:$(Join-Path $OutputDir 'opencl-smoke.exe')" /link $OpenClLibrary
if ($LASTEXITCODE -ne 0) { throw "OpenCL smoke probe compilation failed." }
& cl.exe /nologo /O2 /W4 /MT /EHsc `
    (Join-Path $source "opencl-gl-sharing-smoke.cpp") `
    "/I$OpenClInclude" `
    "/Fe:$(Join-Path $OutputDir 'opencl-gl-sharing-smoke.exe')" `
    /link d3d11.lib opengl32.lib gdi32.lib user32.lib
if ($LASTEXITCODE -ne 0) { throw "OpenCL/OpenGL sharing smoke probe compilation failed." }
