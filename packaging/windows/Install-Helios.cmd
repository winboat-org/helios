@echo off
setlocal
fltmc.exe >nul 2>&1
if errorlevel 1 (
  powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "Start-Process -Verb RunAs -FilePath '%ComSpec%' -ArgumentList '/c','""%~f0""'"
  exit /b
)

powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0Install-Helios.ps1" -EnableTestSigning
set "HELIOS_RC=%ERRORLEVEL%"
if "%HELIOS_RC%"=="194" echo A reboot is required. Run this installer again after Windows restarts.
pause
exit /b %HELIOS_RC%
