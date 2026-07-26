<#
.SYNOPSIS
  Report the kernel stack frame size of chosen KMD functions, straight from the
  built .sys + linker .map. No PDB and no debugger needed.

.DESCRIPTION
  The x64 kernel stack is 24 KB and dxgkrnl's own frames sit above ours on the
  boot path. 22.22.181.0 shipped a DxgkDdiStartDevice / VirtioGpu::init pair
  totalling 18800 bytes and would not boot (0xc0000001 / Startup Repair, with NO
  crash dump and NO bugcheck 1001 event -- an early double fault cannot write
  one). 22.22.180.0's 17936 bytes is the known-good ceiling for that nested pair.

  A function with a frame larger than one page is compiled with a __chkstk
  probe, so its prologue reads:

      movl  $0xNNNN, %eax
      callq <__chkstk>          # llvm-objdump has no name for the thunk and
      subq  %rax, %rsp          # prints it as __GSHandlerCheck_EH4+0xb8

  and 0xNNNN is the frame size. This script finds each symbol's address in the
  .map and reads that prologue out of `llvm-objdump -d`, anchoring on the
  `subq %rax, %rsp` rather than on the unnamed thunk. Functions with small
  frames have no __chkstk call; they are reported as "<4096 (no chkstk)".

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File Z:\tools\kmd-frame-sizes.ps1
#>
[CmdletBinding()]
param(
    [string]   $Package = 'C:\Users\Rupansh\helios-vgpu\kmd_render\target\debug\helios_kmd_render_package',
    [string]   $ObjDump = 'C:\Program Files\LLVM\bin\llvm-objdump.exe',
    # Substrings matched against the mangled names in the .map. The defaults are
    # the boot path: everything DxgkDdiStartDevice keeps live plus the callee
    # that dominates it.
    [string[]] $Symbols = @('12start_device20dxgkddi_start_device', '9VirtioGpu4init'),
    [int]      $Window  = 24
)

$ErrorActionPreference = 'Stop'

$sys = Join-Path $Package 'helios_kmd_render.sys'
$map = Join-Path $Package 'helios_kmd_render.map'
foreach ($f in @($sys, $map, $ObjDump)) {
    if (-not (Test-Path $f)) { throw "missing: $f" }
}

Write-Host ("image     : {0}" -f $sys)
Write-Host ("version   : {0}" -f (Get-Item $sys).VersionInfo.FileVersion)

$dis = & $ObjDump -d $sys
# "18000e340: 48 83 ec ..." -> index by the address text llvm-objdump prints.
$index = @{}
for ($i = 0; $i -lt $dis.Count; $i++) {
    $line = $dis[$i]
    $c = $line.IndexOf(':')
    if ($c -gt 0 -and $c -le 16) {
        $addr = $line.Substring(0, $c)
        if ($addr -match '^[0-9a-f]+$' -and -not $index.ContainsKey($addr)) { $index[$addr] = $i }
    }
}

$total = 0
foreach ($sym in $Symbols) {
    $hit = Select-String -Path $map -Pattern ([regex]::Escape($sym)) -SimpleMatch |
           Select-Object -First 1
    if (-not $hit) { Write-Host ("{0,-40} SYMBOL NOT IN .map" -f $sym); continue }

    # map line: "0001:0000d340  <mangled>  000000018000e340 f  <obj>"
    $va = ($hit.Line -split '\s+' | Where-Object { $_ -match '^[0-9a-fA-F]{16}$' } | Select-Object -First 1)
    if (-not $va) { Write-Host ("{0,-40} NO VA IN MAP LINE" -f $sym); continue }

    $key = ($va.TrimStart('0')).ToLower()
    if (-not $index.ContainsKey($key)) { Write-Host ("{0,-40} VA {1} NOT IN DISASSEMBLY" -f $sym, $va); continue }

    $start = $index[$key]
    $frame = $null
    $pending = $null
    for ($i = $start; $i -lt [Math]::Min($start + $Window, $dis.Count); $i++) {
        $t = $dis[$i]
        if ($t -match 'movl\s+\$0x([0-9a-f]+),\s*%eax') { $pending = [Convert]::ToInt32($matches[1], 16); continue }
        if ($t -match 'subq\s+%rax,\s*%rsp') { $frame = $pending; break }
    }

    if ($null -eq $frame) {
        Write-Host ("{0,-40} <4096 (no chkstk in first {1} insns)" -f $sym, $Window)
    } else {
        Write-Host ("{0,-40} {1,6} bytes  (0x{1:x})" -f $sym, $frame)
        $total += $frame
    }
}

Write-Host ("{0,-40} {1,6} bytes  <-- keep <= 17936 (the 22.22.180.0 known-good ceiling)" -f 'NESTED TOTAL', $total)
