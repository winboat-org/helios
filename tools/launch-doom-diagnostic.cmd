@echo off
setlocal

rem Run this from the Windows desktop session, not over SSH.
rem It keeps Steam/Doom visible while applying Helios/Venus diagnostics.

taskkill /IM DOOMx64vk.exe /F >NUL 2>NUL
taskkill /IM DOOMx64.exe /F >NUL 2>NUL
taskkill /IM steam.exe /F >NUL 2>NUL
taskkill /IM steamwebhelper.exe /F >NUL 2>NUL
taskkill /IM gameoverlayui.exe /F >NUL 2>NUL

set VK_DRIVER_FILES=C:\ProgramData\HeliosVulkan\virtio_devenv_icd.x86_64.json
set VK_LOADER_LAYERS_DISABLE=VK_LAYER_VALVE_steam_overlay,VK_LAYER_VALVE_steam_fossilize
set DISABLE_VK_LAYER_VALVE_steam_overlay_1=1
set DISABLE_VK_LAYER_VALVE_steam_fossilize_1=1
set MESA_VK_WSI_PRESENT_MODE=immediate
set VN_DEBUG=init,wsi,result,log_ctx_info
set HELIOS_PERF=1
set HELIOS_PERF_LIVE=
set HELIOS_PERF_FILE=%USERPROFILE%\helios-doom-perf.txt
set HELIOS_QUEUE_PERF=1
set HELIOS_QUEUE_PERF_INTERVAL=300
set HELIOS_WSI_PERF=1
set HELIOS_WSI_PERF_INTERVAL=300
set HELIOS_WSI_PERF_FILE=%USERPROFILE%\helios-doom-wsi-perf.txt
set HELIOS_WSI_DIRECT_MAP=1
rem Road-4 dcomp present vehicle (23rd session): flip-model HW present for
rem the Vulkan class. Kill switch: delete this line (sw async worker path).
rem A/B 2026-07-07 verdict: sw path 200 fps ZERO stale frames — the stale-
rem frame leak is in the vehicle/UMD flip path (kernel-enforced ordering WS).
set HELIOS_WSI_DCOMP_PRESENT=1
set HELIOS_WSI_INSURANCE_BLIT=0

del "%HELIOS_PERF_FILE%" >NUL 2>NUL
del "%HELIOS_WSI_PERF_FILE%" >NUL 2>NUL

start "" "C:\Program Files (x86)\Steam\Steam.exe" -silent -applaunch 379720 +com_skipIntroVideo 1 +r_fullscreen 0 +r_swapInterval 0

endlocal
