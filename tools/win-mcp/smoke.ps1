# winrun smoke test — asserts the environment a purpose promises.
#
# Run it detached under each principal; it prints what it observed and exits
# non-zero if the promise the caller asked for was not kept. This is the check
# that would have caught "the GPU probe ran in session 0 and lied" before the
# results were believed.
#
#   win-mcp --cli run-script --host slave smoke.ps1 --task smoke-build --purpose build
#   win-mcp --cli run-script --host vm    smoke.ps1 --task smoke-desktop --purpose desktop
#   win-mcp --cli task status --host slave --name smoke-build

$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'

$saw = [ordered]@{
    hostname    = $env:COMPUTERNAME
    user        = "$env:USERDOMAIN\$env:USERNAME"
    session_id  = (Get-Process -Id $PID).SessionId
    ps_version  = $PSVersionTable.PSVersion.ToString()
    cwd         = (Get-Location).Path
    path_first  = ($env:PATH -split ';' | Where-Object { $_ } | Select-Object -First 3) -join ';'
    git_safe    = @(& git config --global --get-all safe.directory 2>$null)
    utc         = [DateTime]::UtcNow.ToString('o')
}

$fail = @()

# The MSYS2 usr/bin git shadows Windows Git and reports a Windows-path repo as
# "Not a git repository" — the reason Purpose::Build rewrites PATH.
if ($saw.path_first -match 'msys64\\usr\\bin') {
    $fail += 'PATH starts with (or early-includes) msys64\usr\bin'
}

# Desktop truth: this must NOT be session 0 for a GPU/desktop probe to mean
# anything. The wrapper refuses before reaching here; this is the belt.
if ($env:WINRUN_EXPECT_SESSION -and "$($saw.session_id)" -ne $env:WINRUN_EXPECT_SESSION) {
    $fail += "session $($saw.session_id), expected $env:WINRUN_EXPECT_SESSION"
}

Write-Output 'WINRUN_JSON_BEGIN'
$saw | ConvertTo-Json -Depth 4 -Compress
Write-Output 'WINRUN_JSON_END'

if ($fail.Count) {
    Write-Output ("SMOKE FAIL: " + ($fail -join '; '))
    exit 1
}
Write-Output 'SMOKE PASS'
exit 0
