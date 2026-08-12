@echo off
setlocal
set "HELIOS_OPENCL_STRIP_INTEROP="
set "HELIOS_OPENCL_STRIP_D3D11="
set "HELIOS_RESOLVE_OPENCL_MIXED_CONTEXT_COMPAT=1"
start "" /D "%~dp0" "%~dp0Resolve.exe" %*
