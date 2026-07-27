<#
.SYNOPSIS
  Compare two `t5-abi-capture.ps1` outputs structurally.

.DESCRIPTION
  The T5 gate asks that "every DDI entry point still receives the same argument
  values (spot-check via one UmdTrace=1 run diffed against a pre-change run)".
  A literal text diff of the two captures is useless: the raw-args dump prints
  heap pointers, which differ every run. What must match is the STRUCTURE:

    * the word COUNT (11 for a D3D11_1/WDDM1_3-negotiated device, 10 for 11.0) --
      this is what bounds the positional dump, and R802's const-asserts pin the
      88-byte struct it walks;
    * the three constants packed into words 1 and 9 (Interface, Version, Flags);
    * which WORD INDEX each named field in the `CreateDevice interface=` line
      resolves to. This is the real ABI check -- pDeviceFuncs must still be at
      word 3 (byte 24), hDrvDevice at word 4 (byte 32), and so on. If the
      runtime moved a field, or if our re-typing read the wrong offset, the
      named value stops matching the indexed word.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File Z:\tools\abi-diff.ps1 `
      -Before Z:\tmp\abi-before.txt -After Z:\tmp\abi-after.txt
#>
param(
    [string]$Before = 'Z:\tmp\abi-before.txt',
    [string]$After  = 'Z:\tmp\abi-after.txt'
)

function Get-Facts([string]$path) {
    $raw   = (Select-String -Path $path -Pattern 'CreateDevice raw args:'   | Select-Object -First 1).Line
    $named = (Select-String -Path $path -Pattern 'CreateDevice interface='  | Select-Object -First 1).Line
    if (-not $raw -or -not $named) { return $null }

    $words = @{}
    foreach ($m in [regex]::Matches($raw, '\[(\d+)\]=0x([0-9a-f]+)')) {
        $words[[int]$m.Groups[1].Value] = [convert]::ToUInt64($m.Groups[2].Value, 16)
    }
    $fields = @{}
    foreach ($m in [regex]::Matches($named, '(\w+)=0x([0-9a-f]+)')) {
        $fields[$m.Groups[1].Value] = [convert]::ToUInt64($m.Groups[2].Value, 16)
    }
    # Which word index does each named pointer live at?
    $where = @{}
    foreach ($k in $fields.Keys) {
        if ($fields[$k] -eq 0) { continue }
        foreach ($i in $words.Keys) {
            if ($words[$i] -eq $fields[$k]) { $where[$k] = $i; break }
        }
    }
    return [pscustomobject]@{
        WordCount = $words.Count
        Word1     = $words[1]
        Word9     = $words[9]
        Interface = $fields['interface']
        Version   = $fields['version']
        Flags     = $fields['flags']
        Where     = $where
    }
}

$b = Get-Facts $Before
$a = Get-Facts $After
if (-not $b) { Write-Host "no CreateDevice lines in $Before"; exit 1 }
if (-not $a) { Write-Host "no CreateDevice lines in $After";  exit 1 }

$fail = @()
function Check($name, $bv, $av) {
    $bs = if ($bv -is [uint64]) { '0x{0:x}' -f $bv } else { "$bv" }
    $as = if ($av -is [uint64]) { '0x{0:x}' -f $av } else { "$av" }
    if ($bv -eq $av) { Write-Host ("  OK   {0,-16} {1}" -f $name, $bs) }
    else { Write-Host ("  DIFF {0,-16} before={1} after={2}" -f $name, $bs, $as); $script:fail += $name }
}

Write-Host "--- constants and shape (must be identical) ---"
Check 'word count' $b.WordCount $a.WordCount
Check 'word[1]'    $b.Word1     $a.Word1
Check 'word[9]'    $b.Word9     $a.Word9
Check 'interface'  $b.Interface $a.Interface
Check 'version'    $b.Version   $a.Version
Check 'flags'      $b.Flags     $a.Flags

Write-Host ""
Write-Host "--- field -> word index (the actual ABI check; pointers themselves differ per run) ---"
$keys = ($b.Where.Keys + $a.Where.Keys) | Sort-Object -Unique
foreach ($k in $keys) {
    $bi = if ($b.Where.ContainsKey($k)) { $b.Where[$k] } else { '<unmatched>' }
    $ai = if ($a.Where.ContainsKey($k)) { $a.Where[$k] } else { '<unmatched>' }
    if ($bi -eq $ai) { Write-Host ("  OK   {0,-16} word[{1}]" -f $k, $bi) }
    else { Write-Host ("  DIFF {0,-16} before=word[{1}] after=word[{2}]" -f $k, $bi, $ai); $fail += $k }
}

Write-Host ""
if ($fail.Count) { Write-Host ("ABI DIFF FAILED: {0}" -f (($fail | Sort-Object -Unique) -join ', ')); exit 1 }
Write-Host "ABI IDENTICAL (shape, constants and every field's word index)"
