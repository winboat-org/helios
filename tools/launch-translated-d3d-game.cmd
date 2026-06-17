@echo off
setlocal
set "SCRIPT=%~dp0launch-translated-d3d-game.ps1"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%SCRIPT%" %*
exit /b %ERRORLEVEL%
