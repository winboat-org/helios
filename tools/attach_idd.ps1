# Attach cdb to the next WUDFHost and log every IddCx!StatusException::ThrowForHRWithMsg
# with its HR (rcx) and stack, to catch the call that fails inside IddSwapChain::Open.
$out = 'C:\ProgramData\Helios\iddcx_throw.log'
Remove-Item $out -ErrorAction SilentlyContinue
$before = @(Get-Process WUDFHost -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })
Start-Process pnputil -ArgumentList '/restart-device','ROOT\DISPLAY\0000' -WindowStyle Hidden
$pid2 = $null
for ($i = 0; $i -lt 400; $i++) {
    $now = @(Get-Process WUDFHost -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })
    $new = $now | Where-Object { $_ -notin $before }
    if ($new) { $pid2 = $new[0]; break }
    Start-Sleep -Milliseconds 50
}
if (-not $pid2) { 'NO NEW WUDFHOST' | Out-File $out; exit 1 }
"attaching to WUDFHost pid=$pid2" | Out-File $out
& 'C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe' -p $pid2 -y 'srv*C:\symbols*https://msdl.microsoft.com/download/symbols' -c ".logappend $out; bp IddCx!StatusException::ThrowForHRWithMsg `"r rcx; kb 14; gc`"; g" 2>&1 | Out-Null
"cdb exited" | Out-File $out -Append
