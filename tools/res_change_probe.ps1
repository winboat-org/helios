# Drive the primary display UP one resolution and back DOWN, so R809's
# `downres_kept` counter has something to count.
#
# WHY A SCHEDULED TASK: ChangeDisplaySettingsEx acts on the calling window
# station. SSH/win_exec land in session 0, which has no interactive desktop, so
# calling this from win_exec silently affects nothing. Run it via
#   schtasks /run /tn helios_res_change
# registered with /it /rl highest against a session-1 user.
#
# Output -> Z:\tmp\res-change.txt (the caller reads that; the task's own stdout
# is not visible from session 0).
$ErrorActionPreference = 'Continue'
$out = 'Z:\tmp\res-change.txt'
"=== res change probe $(Get-Date -Format 'HH:mm:ss') ===" | Set-Content $out
# Session 0 has no display device at all: EnumDisplayDevices returns FALSE at
# index 0 and EnumDisplaySettings fails, whatever dmSize says. So record where
# this actually ran -- a task that silently lands in session 0 looks exactly
# like a marshalling bug from the output alone.
"running as $([Environment]::UserName) in session $((Get-Process -Id $PID).SessionId), window station $([Environment]::GetEnvironmentVariable('SESSIONNAME'))" | Add-Content $out

# DEVMODEW is read and written through an UNMANAGED buffer at fixed offsets
# rather than as a marshalled struct.
#
# Why: the obvious `[StructLayout] struct DEVMODE { ... }` version of this
# marshalled to 124 bytes instead of 220, and every call then failed with
# ERROR_INVALID_PARAMETER (87) because dmSize was wrong. A struct whose size is
# silently short is exactly the class of bug the driver-side const-asserts exist
# to prevent, and there is no reason to reproduce it here: the five fields this
# probe touches are at well-known offsets in a 220-byte DEVMODEW, so it reads
# and writes them directly and asserts the size instead of deriving it.
#
# DEVMODEW offsets (CCHDEVICENAME/CCHFORMNAME = 32 WCHARs = 64 bytes each):
#   dmDeviceName   0    dmSize        68   dmFields          72
#   dmBitsPerPel 168    dmPelsWidth  172   dmPelsHeight     176
#   dmDisplayFlags 180  dmDisplayFrequency 184   sizeof     220
Add-Type @'
using System;
using System.Runtime.InteropServices;
public class Disp {
    [DllImport("user32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern bool EnumDisplaySettingsW(string dev, int mode, IntPtr dm);
    [DllImport("user32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern int ChangeDisplaySettingsExW(string dev, IntPtr dm, IntPtr hwnd, int flags, IntPtr param);

    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)]
    public struct DISPLAY_DEVICE {
        public int cb;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst=32)]  public string DeviceName;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst=128)] public string DeviceString;
        public int StateFlags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst=128)] public string DeviceID;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst=128)] public string DeviceKey;
    }
    [DllImport("user32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern bool EnumDisplayDevicesW(string dev, uint devNum, ref DISPLAY_DEVICE dd, uint flags);
}
'@

$DEVMODE_SIZE = 220
$OFF_SIZE = 68; $OFF_FIELDS = 72
$OFF_BPP = 168; $OFF_W = 172; $OFF_H = 176; $OFF_FREQ = 184

function New-Devmode {
    $p = [System.Runtime.InteropServices.Marshal]::AllocHGlobal($DEVMODE_SIZE)
    for ($i = 0; $i -lt $DEVMODE_SIZE; $i++) {
        [System.Runtime.InteropServices.Marshal]::WriteByte($p, $i, 0)
    }
    # [int16], NOT [short]: PowerShell has no `short` type accelerator, so
    # `[short]220` throws "Unable to find type" and -- with
    # $ErrorActionPreference='Continue' -- the WriteInt16 is skipped silently.
    # dmSize then stays 0 and every EnumDisplaySettings/ChangeDisplaySettings
    # call fails. That was the original failure here, misread twice as a
    # struct-layout problem and once as a session-0 problem.
    [System.Runtime.InteropServices.Marshal]::WriteInt16($p, $OFF_SIZE, [int16]$DEVMODE_SIZE)
    return $p
}
function Get-Dm([IntPtr]$p, [int]$off) {
    return [System.Runtime.InteropServices.Marshal]::ReadInt32($p, $off)
}

# Which adapter/monitor does this session actually have? A null device name is
# legal, but naming the device explicitly gives a better failure message when
# the session has no interactive desktop (which is what session 0 looks like).
$devName = $null
for ($n = 0; $n -lt 8; $n++) {
    $dd = New-Object Disp+DISPLAY_DEVICE
    # SizeOf([type]) not SizeOf($obj): PowerShell hands the object overload a
    # PSObject-wrapped value and it measures the wrapper. That is what made the
    # first version of this probe report sizeof(DEVMODE)=124 and fail every
    # call with ERROR_INVALID_PARAMETER.
    $dd.cb = [System.Runtime.InteropServices.Marshal]::SizeOf([type]'Disp+DISPLAY_DEVICE')
    if (-not [Disp]::EnumDisplayDevicesW($null, $n, [ref]$dd, 0)) { break }
    $attached = ($dd.StateFlags -band 0x1) -ne 0   # DISPLAY_DEVICE_ATTACHED_TO_DESKTOP
    "display[$n]: '$($dd.DeviceName)' '$($dd.DeviceString)' flags=0x$('{0:x}' -f $dd.StateFlags) attached=$attached" | Add-Content $out
    if ($attached -and -not $devName) { $devName = $dd.DeviceName }
}
"using device: '$devName'" | Add-Content $out
# A null device name is legal and is what works when EnumDisplayDevices reports
# nothing (it returns FALSE at index 0 on this box, in session 0 and session 1
# alike). Fall back to it rather than failing.
if (-not $devName) { "  (no adapter enumerated; falling back to a null device name)" | Add-Content $out }

$ENUM_CURRENT = -1
$dm = New-Devmode
if (-not [Disp]::EnumDisplaySettingsW($devName, $ENUM_CURRENT, $dm)) {
    $err = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()
    "FAIL: EnumDisplaySettingsW(current) on '$devName' GetLastError=$err" | Add-Content $out
    [System.Runtime.InteropServices.Marshal]::FreeHGlobal($dm)
    exit 1
}
$curW = Get-Dm $dm $OFF_W; $curH = Get-Dm $dm $OFF_H; $curHz = Get-Dm $dm $OFF_FREQ
"current: ${curW}x${curH} @ ${curHz}Hz" | Add-Content $out
[System.Runtime.InteropServices.Marshal]::FreeHGlobal($dm)

# Enumerate the modes at 32bpp.
$modes = @()
for ($i = 0; $i -lt 400; $i++) {
    $m = New-Devmode
    if (-not [Disp]::EnumDisplaySettingsW($devName, $i, $m)) {
        [System.Runtime.InteropServices.Marshal]::FreeHGlobal($m); break
    }
    if ((Get-Dm $m $OFF_BPP) -eq 32) {
        $modes += [pscustomobject]@{ W = (Get-Dm $m $OFF_W); H = (Get-Dm $m $OFF_H); Hz = (Get-Dm $m $OFF_FREQ) }
    }
    [System.Runtime.InteropServices.Marshal]::FreeHGlobal($m)
}
$modes = $modes | Sort-Object W, H -Unique | Sort-Object { $_.W * $_.H }
"modes available: $($modes.Count)" | Add-Content $out
$modes | ForEach-Object { "  $($_.W)x$($_.H)@$($_.Hz)" } | Add-Content $out

function Set-Mode([int]$w, [int]$h) {
    $m = New-Devmode
    $null = [Disp]::EnumDisplaySettingsW($devName, -1, $m)
    [System.Runtime.InteropServices.Marshal]::WriteInt32($m, $OFF_W, $w)
    [System.Runtime.InteropServices.Marshal]::WriteInt32($m, $OFF_H, $h)
    # DM_BITSPERPEL | DM_PELSWIDTH | DM_PELSHEIGHT
    [System.Runtime.InteropServices.Marshal]::WriteInt32($m, $OFF_FIELDS, (0x040000 -bor 0x080000 -bor 0x100000))
    $rc = [Disp]::ChangeDisplaySettingsExW($devName, $m, [IntPtr]::Zero, 0, [IntPtr]::Zero)
    [System.Runtime.InteropServices.Marshal]::FreeHGlobal($m)
    "ChangeDisplaySettings ${w}x${h} -> rc=$rc  (0 = DISP_CHANGE_SUCCESSFUL)" | Add-Content $out
    return $rc
}

$curArea = $curW * $curH
$bigger  = $modes | Where-Object { ($_.W * $_.H) -gt $curArea } | Select-Object -First 1
$smaller = $modes | Where-Object { ($_.W * $_.H) -lt $curArea } | Select-Object -Last 1

# UP first, then DOWN past the original: the down-resolution policy only fires
# when the offered geometry is SMALLER than what is already stored, so the run
# has to go below where it started.
if ($bigger)  { $null = Set-Mode $bigger.W $bigger.H;  Start-Sleep -Seconds 6 }
else          { "no larger mode than ${curW}x${curH} available" | Add-Content $out }
if ($smaller) { $null = Set-Mode $smaller.W $smaller.H; Start-Sleep -Seconds 6 }
else          { "no smaller mode than ${curW}x${curH} available" | Add-Content $out }

# Back to where we started, so the desktop is left as found.
$null = Set-Mode $curW $curH
Start-Sleep -Seconds 4
$fin = New-Devmode
$null = [Disp]::EnumDisplaySettingsW($devName, -1, $fin)
"restored: $(Get-Dm $fin $OFF_W)x$(Get-Dm $fin $OFF_H) @ $(Get-Dm $fin $OFF_FREQ)Hz" | Add-Content $out
[System.Runtime.InteropServices.Marshal]::FreeHGlobal($fin)
"=== done ===" | Add-Content $out
