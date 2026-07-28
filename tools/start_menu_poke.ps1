# start_menu_poke.ps1 — session-1 helper: invoke the Start menu, then dismiss it.
# Exists so a session-0 loop can make the shell relaunch StartMenuExperienceHost
# (it only relaunches on demand) and exercise the invoke path. Synthetic input is
# a no-op in session 0, which is why this has to be a scheduled task.
Add-Type @"
using System;using System.Runtime.InteropServices;
public class PK { [DllImport("user32.dll")] public static extern void keybd_event(byte v, byte s, uint f, IntPtr e); }
"@
[PK]::keybd_event(0x5B, 0, 0, [IntPtr]::Zero)
[PK]::keybd_event(0x5B, 0, 2, [IntPtr]::Zero)
Start-Sleep -Milliseconds 2000
[PK]::keybd_event(0x1B, 0, 0, [IntPtr]::Zero)
[PK]::keybd_event(0x1B, 0, 2, [IntPtr]::Zero)
