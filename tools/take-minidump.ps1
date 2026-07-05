# Take a minidump of a process via dbghelp!MiniDumpWriteDump (P/Invoke).
# Usage: take-minidump.ps1 -ProcessId 1900 -Path C:\ProgramData\Helios\dwm.dmp [-Full]
param(
    [Parameter(Mandatory = $true)][int]$ProcessId,
    [Parameter(Mandatory = $true)][string]$Path,
    [switch]$Full
)

$src = @"
using System;
using System.Runtime.InteropServices;
public static class MiniDump {
    [DllImport("dbghelp.dll", SetLastError = true)]
    public static extern bool MiniDumpWriteDump(
        IntPtr hProcess, uint processId, IntPtr hFile, uint dumpType,
        IntPtr exceptionParam, IntPtr userStreamParam, IntPtr callbackParam);
}
"@
Add-Type -TypeDefinition $src

$proc = Get-Process -Id $ProcessId -ErrorAction Stop
$fs = [System.IO.File]::Create($Path)
# 0x2 = MiniDumpWithFullMemory; 0x0 = MiniDumpNormal (stacks + module list)
# 0x1041 = Normal | WithHandleData | WithThreadInfo | WithUnloadedModules
$dumpType = if ($Full) { 0x2 } else { 0x1041 }
$ok = [MiniDump]::MiniDumpWriteDump(
    $proc.Handle, [uint32]$ProcessId, $fs.SafeFileHandle.DangerousGetHandle(),
    [uint32]$dumpType, [IntPtr]::Zero, [IntPtr]::Zero, [IntPtr]::Zero)
$err = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
$fs.Close()
if ($ok) { "OK $((Get-Item $Path).Length) bytes" } else { "FAILED gle=$err" }
