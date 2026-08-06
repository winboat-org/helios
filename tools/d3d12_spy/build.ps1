# build.ps1 -- build the D12-G5 WARP spy proxy and its workloads on win11.
#
# [STOP] Builds to a LOCAL C: path, never Z:\ -- the 9p/virtio share fails file IO with
#    OS error 87 and the same class bites linkers (CLAUDE.md, BRINGUP_QUIRKS.md).
# [STOP] Never touches C:\Windows\System32. It COPIES d3d10warp.dll out of it (a read) and
#    records both hashes, because the proxy must load the real WARP under a different base
#    name: the loader's already-loaded check matches on base name, so a full-path load of
#    "d3d10warp.dll" from a module named d3d10warp.dll hands back the module itself
#    (DECISIONS.md P-A / sec.6.1).
#
# Usage:  powershell -File Z:\tools\d3d12_spy\build.ps1
$ErrorActionPreference = 'Stop'

$src = 'Z:\tools\d3d12_spy'
$dir = 'C:\Users\Rupansh\d12g5'
$G   = 'Z:\tmp\dx12\gates\G5'
$vcvars = 'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'
$dxc = 'C:\VulkanSDK\1.4.350.0\Bin\dxc.exe'

New-Item -ItemType Directory -Force -Path $dir | Out-Null
New-Item -ItemType Directory -Force -Path $G   | Out-Null

# --- 1. the generated slot lists must match DECISIONS.md sec.4.1, or every log line is
#        mislabelled. Two banner lines precede the X() lines in each .inc.
$expect = @{
  'slots_core_0109.inc'    = 124
  'slots_cl_0108.inc'      = 75
  'slots_queue_0001.inc'   = 7
  'slots_adapter_0109.inc' = 8
  'slots_dxgi.inc'         = 32
  'caps_types.inc'         = 43
  'table_types.inc'        = 25
}
foreach ($f in $expect.Keys) {
  $n = (Get-Content (Join-Path $src $f) | Where-Object { $_ -notmatch '^\s*[/ ]\*' }).Count
  if ($n -ne $expect[$f]) {
    throw "$f has $n entries, expected $($expect[$f]) -- re-run tools/d3d12_spy/gen_slots.py"
  }
  "{0,-24} {1,4} entries OK" -f $f, $n
}

Copy-Item "$src\*.cpp", "$src\*.inc", "$src\*.asm", "$src\*.def", "$src\*.hlsl" $dir -Force

# --- 2. the real WARP, under a base name the loader cannot confuse with the proxy's
$sys = Join-Path $env:SystemRoot 'System32\d3d10warp.dll'
Copy-Item $sys "$dir\d3d10warp_real.dll" -Force
$h1 = (Get-FileHash -Algorithm SHA256 $sys).Hash
$h2 = (Get-FileHash -Algorithm SHA256 "$dir\d3d10warp_real.dll").Hash
if ($h1 -ne $h2) { throw "d3d10warp_real.dll is not a faithful copy of System32's" }
$ver = (Get-Item $sys).VersionInfo.FileVersion
"real WARP  $ver  sha256=$h1"
"$ver $h1" | Out-File -Encoding ascii "$G\warp-identity.txt"

# --- 3. shaders: DXIL SM 6.0 for the triangle workload's first pipeline
if (-not (Test-Path $dxc)) { throw "dxc not found at $dxc" }
& $dxc -T vs_6_0 -E vs_main -Vn g_vs_main -Fh "$dir\spy_workload_vs.h" "$dir\spy_workload.hlsl"
if ($LASTEXITCODE -ne 0) { throw "dxc vs_6_0 failed, exit $LASTEXITCODE" }
& $dxc -T ps_6_0 -E ps_main -Vn g_ps_main -Fh "$dir\spy_workload_ps.h" "$dir\spy_workload.hlsl"
if ($LASTEXITCODE -ne 0) { throw "dxc ps_6_0 failed, exit $LASTEXITCODE" }

# --- 4. compile. The 206 generic slot forwarders are ml64 assembly: GCC has no `naked`
#        attribute on x86-64 and MSVC has no inline asm there, so a generated .asm is the
#        only way to write an ABI-transparent thunk that does not know its slot's
#        signature (sec.7.3(2) forbids hand-writing 206 D3D12 prototypes).
# [STOP] Build the command string FIRST: in PowerShell argument mode `+` is a literal token.
# [STOP] Redirect inside cmd -- `& exe 2>&1 | Tee-Object` under $ErrorActionPreference='Stop'
#    dies on the first stderr line, before the program's stdout is printed (72nd session).
# [STOP] No /Fo"$dir\" -- a quoted path ending in a backslash escapes the closing quote in
#    cmd and yields "The filename, directory name, or volume label syntax is incorrect."
#    The `cd /d` above already puts the objects where they belong.
# [STOP] One string per line, no `+` continuation: inside an @() literal a trailing `+`
#    does NOT reliably join the two halves, and the array silently gains an element -- the
#    tail then runs as its own command and cmd answers "The filename, directory name, or
#    volume label syntax is incorrect."
$cmds = @(
  "ml64 /nologo /c /Fo spy_thunks.obj spy_thunks.asm",
  "cl /nologo /LD /EHsc /W4 /O2 /I. d3d12_warp_spy.cpp spy_thunks.obj /Fe:d3d10warp.dll /link /DEF:d3d12_spy.def advapi32.lib",
  "cl /nologo /EHsc /W4 /O2 /I. spy_workload.cpp /Fe:spy_workload.exe /link d3d12.lib dxgi.lib dxguid.lib d3dcompiler.lib user32.lib"
)
# [STOP] `cd /d "$dir"` INSIDE the already-quoted cmd string loses its quoting and cmd
#    answers "The filename, directory name, or volume label syntax is incorrect." -- with a
#    zero exit code from the compiler that already ran, so it reads like a compiler error.
#    $dir and the log path contain no spaces; pass them bare.
$step = 0
foreach ($c in $cmds) {
  $step++
  "--- build step ${step} - $c"
  cmd /c "call `"$vcvars`" >nul && cd /d $dir && $c > $dir\build-step.log 2>&1"
  $rc = $LASTEXITCODE
  Get-Content "$dir\build-step.log"
  if ($rc -ne 0) { throw "build step failed (exit $rc): $c" }
}

# --- 4b. Route B copy. Route A puts the proxy beside the exe UNDER WARP's own base name;
#         Route B points the Helios adapter's UserModeDriverName[3] at it, and there the
#         base name must NOT be d3d10warp.dll -- the loader keys the loaded-module list on
#         base name, so a module called d3d10warp.dll in the process would then be handed
#         back for any later System32 d3d10warp load. Same bytes, different name.
Copy-Item "$dir\d3d10warp.dll" "$dir\helios_umd12_spy.dll" -Force

# --- 5. the proxy must export exactly the three entry points WARP does
$dumpbin = (Get-ChildItem 'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\MSVC' -Directory |
            Select-Object -First 1).FullName + '\bin\Hostx64\x64\dumpbin.exe'
& $dumpbin /exports "$dir\d3d10warp.dll" | Select-String 'OpenAdapter'
Get-FileHash -Algorithm SHA256 "$dir\d3d10warp.dll", "$dir\spy_workload.exe" |
  Format-Table Hash, Path -AutoSize

"built into $dir"
