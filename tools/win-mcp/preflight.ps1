# winrun preflight — what can this host actually do, in one call.
#
# Answers the questions that cost a diagnose cycle when they are discovered
# mid-build instead of up front: is the toolchain here, is the tree here, is
# there disk, and is git usable under the account this runs as.
#
# Emits one JSON object between the WINRUN_JSON_BEGIN/END markers; everything
# else is progress output and is ignored by the caller.

$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'

function Tool([string]$Name, [string]$VersionArg = '--version') {
    $cmd = Get-Command $Name -ErrorAction SilentlyContinue
    if (-not $cmd) { return [pscustomobject]@{ present = $false; path = $null; version = $null } }
    $version = $null
    try {
        $raw = & $cmd.Source $VersionArg 2>&1 | Select-Object -First 1
        $version = [string]$raw
    } catch { }
    [pscustomobject]@{ present = $true; path = $cmd.Source; version = $version }
}

$result = [ordered]@{}
$result.utc = [DateTime]::UtcNow.ToString('o')
$result.hostname = $env:COMPUTERNAME
$result.user = "$env:USERDOMAIN\$env:USERNAME"
$result.session_id = (Get-Process -Id $PID).SessionId
$result.is_admin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
$result.ps_version = $PSVersionTable.PSVersion.ToString()
$result.ps_edition = $PSVersionTable.PSEdition

$result.tools = [ordered]@{
    git      = Tool 'git'
    python   = Tool 'python'
    ninja    = Tool 'ninja'
    meson    = Tool 'meson'
    cargo    = Tool 'cargo'
    rustc    = Tool 'rustc'
    clang    = Tool 'clang'
    clangcl  = Tool 'clang-cl'
    cl       = Tool 'cl'
    cmake    = Tool 'cmake'
    msbuild  = Tool 'msbuild' '-version'
    pwsh7    = [pscustomobject]@{
        present = (Test-Path 'C:\Program Files\PowerShell\7\pwsh.exe')
        path    = 'C:\Program Files\PowerShell\7\pwsh.exe'
        version = $(if (Test-Path 'C:\Program Files\PowerShell\7\pwsh.exe') { (& 'C:\Program Files\PowerShell\7\pwsh.exe' -NoProfile -Command '$PSVersionTable.PSVersion.ToString()' 2>&1 | Select-Object -First 1) } else { $null })
    }
}

$result.vswhere = [pscustomobject]@{
    present = (Test-Path 'C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe')
    installs = @(
        if (Test-Path 'C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe') {
            & 'C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe' -products * -format value -property installationPath 2>$null
        }
    )
}

# ⛔ `cl`, `link` and `msbuild` are NOT on PATH outside a vcvars shell, so a
# PATH-only check reports "no compiler" for a machine that builds the driver
# perfectly well. Resolve them through the vswhere installs (and LLVM through its
# known roots) so this section is trustworthy.
$vsRoots = @($result.vswhere.installs)
function FindVs([string]$Relative) {
    foreach ($root in $vsRoots) {
        if (-not $root) { continue }
        $p = Join-Path $root $Relative
        if (Test-Path -LiteralPath $p) { return $p }
    }
    return $null
}
$msvc = @(
    foreach ($root in $vsRoots) {
        if (-not $root) { continue }
        $tools = Join-Path $root 'VC\Tools\MSVC'
        if (Test-Path -LiteralPath $tools) {
            Get-ChildItem -LiteralPath $tools -Directory -ErrorAction SilentlyContinue |
                Sort-Object Name -Descending |
                ForEach-Object { Join-Path $_.FullName 'bin\Hostx64\x64' }
        }
    }
)
$clPath = $null
$linkPath = $null
foreach ($dir in $msvc) {
    if (-not $clPath -and (Test-Path -LiteralPath (Join-Path $dir 'cl.exe'))) { $clPath = Join-Path $dir 'cl.exe' }
    if (-not $linkPath -and (Test-Path -LiteralPath (Join-Path $dir 'link.exe'))) { $linkPath = Join-Path $dir 'link.exe' }
}
$clangCandidates = @(
    'C:\helios\llvm-22.1.8\bin\clang-cl.exe',
    'C:\Program Files\LLVM\bin\clang-cl.exe',
    (FindVs 'VC\Tools\Llvm\x64\bin\clang-cl.exe')
) | Where-Object { $_ }
$clangCl = $clangCandidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
$msbuildPath = FindVs 'MSBuild\Current\Bin\MSBuild.exe'

$result.compilers = [ordered]@{
    cl = [pscustomobject]@{ present = [bool]$clPath; path = $clPath; note = 'resolved through vswhere, not PATH' }
    link = [pscustomobject]@{ present = [bool]$linkPath; path = $linkPath }
    clang_cl = [pscustomobject]@{ present = [bool]$clangCl; path = $clangCl }
    msbuild = [pscustomobject]@{ present = [bool]$msbuildPath; path = $msbuildPath }
    msvc_dirs = $msvc
}

$result.msys2 = [ordered]@{
    usr_bin     = (Test-Path 'C:\msys64\usr\bin\bash.exe')
    ucrt64_bin  = (Test-Path 'C:\msys64\ucrt64\bin\ninja.exe')
    mingw32_bin = (Test-Path 'C:\msys64\mingw32\bin\ninja.exe')
    # Load-bearing: PATH must not start with the MSYS2 usr/bin git.
    usr_bin_git = (Test-Path 'C:\msys64\usr\bin\git.exe')
}

$result.workspaces = @(
    foreach ($p in @('C:\src', 'C:\src\helios-integration-20260912', 'C:\Users\Tibix', 'C:\ProgramData\Helios')) {
        [pscustomobject]@{ path = $p; exists = (Test-Path -LiteralPath $p) }
    }
)

$gitRepos = @(
    foreach ($p in @('C:\src\helios-integration-20260912', 'C:\Users\Tibix\helios-vgpu')) {
        if (Test-Path -LiteralPath $p) {
            $sha = (& git -C $p rev-parse HEAD 2>&1 | Select-Object -First 1)
            $dirty = (& git -C $p status --porcelain 2>&1 | Measure-Object).Count
            [pscustomobject]@{ path = $p; head = "$sha".Trim(); changed_entries = $dirty; readable = ($LASTEXITCODE -eq 0) }
        }
    }
)
$result.git_repos = $gitRepos
$result.git_safe_directory = @(& git config --global --get-all safe.directory 2>$null)

$result.drives = @(
    Get-PSDrive -PSProvider FileSystem -ErrorAction SilentlyContinue |
        Where-Object { $_.Free -ne $null } |
        ForEach-Object { [pscustomobject]@{ name = $_.Name; free_gb = [Math]::Round($_.Free / 1GB, 2); used_gb = [Math]::Round($_.Used / 1GB, 2) } }
)

$result.build_processes = @(
    Get-Process -ErrorAction SilentlyContinue |
        Where-Object { $_.ProcessName -match 'cargo|rustc|ninja|meson|clang|link|msbuild|cl$' } |
        ForEach-Object { [pscustomobject]@{ id = $_.Id; name = $_.ProcessName; started = $_.StartTime.ToString('o') } }
)

$result.tasks = @(
    Get-ScheduledTask -ErrorAction SilentlyContinue |
        Where-Object { $_.TaskName -like 'Helios*' -or $_.TaskName -like 'winrun*' } |
        ForEach-Object { [pscustomobject]@{ name = $_.TaskName; state = "$($_.State)" } }
)

Write-Output 'WINRUN_JSON_BEGIN'
$result | ConvertTo-Json -Depth 6 -Compress
Write-Output 'WINRUN_JSON_END'
