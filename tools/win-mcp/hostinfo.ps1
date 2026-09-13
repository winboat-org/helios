# winrun hostinfo — identity and session facts for one host, in one call.
#
# Separate from preflight because the question "am I actually talking to the
# machine I think I am, in the session I think I am" is asked far more often
# than the toolchain one, and it must be cheap.

$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'

$result = [ordered]@{
    utc            = [DateTime]::UtcNow.ToString('o')
    hostname       = $env:COMPUTERNAME
    user           = "$env:USERDOMAIN\$env:USERNAME"
    session_id     = (Get-Process -Id $PID).SessionId
    is_admin       = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    ps_version     = $PSVersionTable.PSVersion.ToString()
    ps_edition     = $PSVersionTable.PSEdition
    os             = (Get-CimInstance Win32_OperatingSystem -ErrorAction SilentlyContinue).Caption
    os_build       = (Get-CimInstance Win32_OperatingSystem -ErrorAction SilentlyContinue).BuildNumber
    interactive    = @(
        (Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -ne 0 } | Select-Object -ExpandProperty SessionId -Unique)
    )
    explorer       = @(Get-Process explorer -ErrorAction SilentlyContinue | ForEach-Object { [pscustomobject]@{ pid = $_.Id; session = $_.SessionId } })
    free_gb        = @(
        Get-PSDrive -PSProvider FileSystem -ErrorAction SilentlyContinue |
            Where-Object { $_.Free -ne $null } |
            ForEach-Object { [pscustomobject]@{ name = $_.Name; free_gb = [Math]::Round($_.Free / 1GB, 2) } }
    )
}

Write-Output 'WINRUN_JSON_BEGIN'
$result | ConvertTo-Json -Depth 6 -Compress
Write-Output 'WINRUN_JSON_END'
