@echo off
pwsh.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0Invoke-RustScriptShortPath.ps1" %*
exit /b %ERRORLEVEL%
