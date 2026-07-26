Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Assert-HeliosAdmin {
  $id = [Security.Principal.WindowsIdentity]::GetCurrent()
  $principal = [Security.Principal.WindowsPrincipal]::new($id)
  if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Run this script from an elevated PowerShell."
  }
}

function Get-HeliosFileHash([string]$Path) {
  if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
    throw "File not found: $Path"
  }
  return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToUpperInvariant()
}

function Invoke-HeliosProcess([string]$FilePath, [string[]]$Arguments, [int]$TimeoutSeconds = 60) {
  $out = Join-Path $env:TEMP ("helios-{0}.out.txt" -f ([guid]::NewGuid()))
  $err = Join-Path $env:TEMP ("helios-{0}.err.txt" -f ([guid]::NewGuid()))
  $startArgs = @{
    FilePath = $FilePath
    NoNewWindow = $true
    PassThru = $true
    RedirectStandardOutput = $out
    RedirectStandardError = $err
  }
  if ($Arguments -and $Arguments.Count -gt 0) {
    $startArgs.ArgumentList = $Arguments
  }
  $p = Start-Process @startArgs
  if (-not $p.WaitForExit($TimeoutSeconds * 1000)) {
    Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
    $stdout = if (Test-Path -LiteralPath $out) { Get-Content -LiteralPath $out -Raw } else { "" }
    $stderr = if (Test-Path -LiteralPath $err) { Get-Content -LiteralPath $err -Raw } else { "" }
    Remove-Item -LiteralPath $out,$err -Force -ErrorAction SilentlyContinue
    throw "$FilePath $($Arguments -join ' ') timed out after ${TimeoutSeconds}s`n$stdout`n$stderr"
  }
  $stdout = if (Test-Path -LiteralPath $out) { Get-Content -LiteralPath $out -Raw } else { "" }
  $stderr = if (Test-Path -LiteralPath $err) { Get-Content -LiteralPath $err -Raw } else { "" }
  Remove-Item -LiteralPath $out,$err -Force -ErrorAction SilentlyContinue
  if ($stdout) { Write-Host $stdout.TrimEnd() }
  if ($stderr) { Write-Warning $stderr.TrimEnd() }
  return [pscustomobject]@{ ExitCode = $p.ExitCode; Stdout = $stdout; Stderr = $stderr }
}

function Invoke-HeliosPnpUtil([string[]]$Arguments, [int]$TimeoutSeconds = 90) {
  $r = Invoke-HeliosProcess -FilePath "pnputil.exe" -Arguments $Arguments -TimeoutSeconds $TimeoutSeconds
  $okByText = $r.Stdout -match "Device disabled successfully|Device enabled successfully|Driver package added successfully|Device restarted successfully|Microsoft PnP Utility"
  if ($r.ExitCode -ne 0 -and -not $okByText) {
    throw "pnputil $($Arguments -join ' ') failed with exit code $($r.ExitCode)"
  }
  return $r
}

function Grant-HeliosWritable([string]$Path) {
  if (Test-Path -LiteralPath $Path) {
    & takeown.exe /F $Path | Out-Null
    & icacls.exe $Path /grant "*S-1-5-32-544:F" | Out-Null
    & attrib.exe -R $Path | Out-Null
  } else {
    $parent = Split-Path -Parent $Path
    if ($parent -and (Test-Path -LiteralPath $parent)) {
      & icacls.exe $parent /grant "*S-1-5-32-544:F" | Out-Null
    }
  }
}

function Get-HeliosFileUsers([string]$Path) {
  $full = [IO.Path]::GetFullPath($Path)
  $users = @()
  foreach ($proc in Get-Process -ErrorAction SilentlyContinue) {
    try {
      foreach ($m in $proc.Modules) {
        if ($m.FileName -and ([IO.Path]::GetFullPath($m.FileName) -ieq $full)) {
          $users += [pscustomobject]@{ Id = $proc.Id; ProcessName = $proc.ProcessName; Module = $m.FileName }
          break
        }
      }
    } catch {
      continue
    }
  }
  return $users
}

function Remove-HeliosDisplacedCopies([string]$Destination) {
  # Reap `<name>.inuse-<stamp>` files left by -DisplaceInUse once their holders
  # have exited. Best effort: a file still mapped by a live process is skipped
  # and picked up by a later deploy.
  #
  # Get-HeliosFileUsers is an OPTIMISTIC filter here, not an authority:
  # ProcessModule.FileName reports the path as resolved at LOAD time and does
  # not follow a rename, so a holder of the displaced image still reports the
  # original name. The real backstop is Remove-Item itself — Windows refuses to
  # delete a mapped image — which is why the removal is verified with Test-Path
  # rather than assumed. Observed 2026-07-27: the first displaced copy survived
  # its own deploy's reap for exactly this reason, which is the correct outcome.
  $dir = Split-Path -Parent $Destination
  if (-not (Test-Path -LiteralPath $dir -PathType Container)) { return 0 }
  $leaf = [IO.Path]::GetFileName($Destination)
  $removed = 0
  foreach ($f in @(Get-ChildItem -LiteralPath $dir -Filter "$leaf.inuse-*" -ErrorAction SilentlyContinue)) {
    if (@(Get-HeliosFileUsers $f.FullName).Count -gt 0) { continue }
    Remove-Item -LiteralPath $f.FullName -Force -ErrorAction SilentlyContinue
    if (-not (Test-Path -LiteralPath $f.FullName)) { $removed++ }
  }
  return $removed
}

function Copy-HeliosFileVerified([string]$Source, [string]$Destination, [int]$Retries = 3, [int]$RetryDelayMs = 500, [switch]$DisplaceInUse) {
  if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) { throw "Missing source file $Source" }
  $destDir = Split-Path -Parent $Destination
  New-Item -ItemType Directory -Force -Path $destDir | Out-Null
  Grant-HeliosWritable $destDir
  $sourceHash = Get-HeliosFileHash $Source
  if ((Test-Path -LiteralPath $Destination -PathType Leaf) -and ((Get-HeliosFileHash $Destination) -eq $sourceHash)) {
    return [pscustomobject]@{ Source = $Source; Destination = $Destination; Hash = $sourceHash }
  }
  $tmp = Join-Path $destDir (".{0}.{1}.tmp" -f ([IO.Path]::GetFileName($Destination)), ([guid]::NewGuid()))
  Copy-Item -LiteralPath $Source -Destination $tmp -Force
  $tmpHash = Get-HeliosFileHash $tmp
  if ($tmpHash -ne $sourceHash) {
    Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
    throw "Temporary copy hash mismatch for $Destination. source=$sourceHash tmp=$tmpHash"
  }

  $lastError = $null
  for ($i = 1; $i -le $Retries; $i++) {
    try {
      Grant-HeliosWritable $Destination
      Move-Item -LiteralPath $tmp -Destination $Destination -Force
      $destHash = Get-HeliosFileHash $Destination
      if ($destHash -ne $sourceHash) {
        throw "Destination hash mismatch for $Destination. source=$sourceHash dest=$destHash"
      }
      return [pscustomobject]@{ Source = $Source; Destination = $Destination; Hash = $destHash }
    } catch {
      $lastError = $_
      if (-not (Test-Path -LiteralPath $tmp)) {
        Copy-Item -LiteralPath $Source -Destination $tmp -Force
      }
      if ($DisplaceInUse -and (Test-Path -LiteralPath $Destination -PathType Leaf)) {
        # A LOADED image is held by the loader with no write sharing, so this is
        # a SHARING violation, not an ACL one — takeown/icacls cannot clear it,
        # and the holders may be processes we must not kill (on the DriverStore
        # path they are shell infrastructure: ShellHost, SystemSettings,
        # CrossDeviceResume, observed 2026-07-27).
        #
        # Windows does allow RENAMING an open file: the holders' handles follow
        # the rename and keep running the old image, while the fresh file lands
        # at the original path for every later load. That is what makes this
        # safe without a reboot — and a reboot-scheduled replace is NOT an
        # option here anyway, because Clear-HeliosPendingRenames deliberately
        # strips any pending helios_umd rename at the start of every deploy.
        $displaced = "{0}.inuse-{1}" -f $Destination, (Get-Date -Format "yyyyMMdd-HHmmss-fff")
        try {
          Grant-HeliosWritable $Destination
          Move-Item -LiteralPath $Destination -Destination $displaced -Force -ErrorAction Stop
          Write-Host "Displaced in-use $([IO.Path]::GetFileName($Destination)) -> $([IO.Path]::GetFileName($displaced))"
        } catch {
          # Fall through to the normal backoff/retry below; if the rename is
          # refused too, the loop exhausts and the caller gets the loud throw
          # naming the holders.
        }
      }
      Start-Sleep -Milliseconds $RetryDelayMs
    }
  }

  $users = @(Get-HeliosFileUsers $Destination)
  Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
  if ($users.Count -gt 0) {
    $text = ($users | ForEach-Object { "$($_.ProcessName)[$($_.Id)]" }) -join ", "
    throw "Could not replace $Destination after $Retries attempts. Loaded by: $text. Last error: $($lastError.Exception.Message)"
  }
  throw "Could not replace $Destination after $Retries attempts. Last error: $($lastError.Exception.Message)"
}

function Get-HeliosInstanceId([string]$InstanceId = "") {
  if ($InstanceId) { return $InstanceId }
  $dev = Get-CimInstance Win32_PnPEntity |
    Where-Object { $_.PNPDeviceID -like "PCI\VEN_1AF4&DEV_1050*" -and $_.Name -like "Helios vGPU Render Adapter*" } |
    Select-Object -First 1
  if (-not $dev) {
    $dev = Get-CimInstance Win32_PnPEntity |
      Where-Object { $_.PNPDeviceID -like "PCI\VEN_1AF4&DEV_1050*" } |
      Select-Object -First 1
  }
  if (-not $dev) { throw "Helios PCI device not found." }
  return $dev.PNPDeviceID
}

function Get-HeliosActiveInfName([string]$InstanceId) {
  $prop = Get-PnpDeviceProperty -InstanceId $InstanceId -KeyName DEVPKEY_Device_DriverInfPath -ErrorAction SilentlyContinue
  if ($prop -and $prop.PSObject.Properties["Data"] -and $prop.Data) { return [string]$prop.Data }
  $text = & pnputil.exe /enum-devices /instanceid "$InstanceId" /drivers
  $line = $text | Where-Object { $_ -match "Driver Name:\s+(oem\d+\.inf)" } | Select-Object -First 1
  if ($line -match "Driver Name:\s+(oem\d+\.inf)") { return $Matches[1] }
  throw "Could not resolve active INF name for $InstanceId."
}

function Get-HeliosClassKey([string]$InstanceId) {
  $classRoot = "HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}"
  $lastSlash = $InstanceId.LastIndexOf("\")
  $needle = if ($lastSlash -gt 0) { $InstanceId.Substring(0, $lastSlash).ToUpperInvariant() } else { $InstanceId.ToUpperInvariant() }
  for ($i = 0; $i -lt 256; $i++) {
    $path = Join-Path $classRoot ("{0:d4}" -f $i)
    if (-not (Test-Path -LiteralPath $path)) { continue }
    $p = Get-ItemProperty -LiteralPath $path -ErrorAction SilentlyContinue
    if (-not $p) { continue }
    $mid = [string]$p.MatchingDeviceId
    if ($mid -and $needle.StartsWith($mid.ToUpperInvariant())) { return $path }
  }
  throw "Could not locate Helios display class key for $InstanceId."
}

function Get-HeliosActiveStoreDir([string]$InstanceId, [string]$ActiveInfName) {
  $svc = Get-ItemProperty -LiteralPath "HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render" -ErrorAction SilentlyContinue
  if ($svc -and $svc.PSObject.Properties["ImagePath"] -and $svc.ImagePath) {
    $imagePath = [string]$svc.ImagePath
    $imagePath = $imagePath -replace "^\\SystemRoot", $env:windir
    $imagePath = [Environment]::ExpandEnvironmentVariables($imagePath)
    $imageDir = Split-Path -Parent $imagePath
    if ($imageDir -and
        (Test-Path -LiteralPath (Join-Path $imageDir "helios_kmd_render.sys") -PathType Leaf) -and
        (Test-Path -LiteralPath (Join-Path $imageDir "helios_kmd_render.inf") -PathType Leaf)) {
      return $imageDir
    }
  }

  $infPath = Join-Path $env:windir "INF\$ActiveInfName"
  $dirs = Get-ChildItem "$env:windir\System32\DriverStore\FileRepository" -Directory -Filter "helios_kmd_render.inf_amd64_*" -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending
  if (Test-Path -LiteralPath $infPath) {
    $activeHash = Get-HeliosFileHash $infPath
    foreach ($dir in $dirs) {
      $candidate = Join-Path $dir.FullName "helios_kmd_render.inf"
      if ((Test-Path -LiteralPath $candidate) -and ((Get-HeliosFileHash $candidate) -eq $activeHash)) {
        return $dir.FullName
      }
    }

    $activeText = Get-Content -LiteralPath $infPath -Raw -ErrorAction SilentlyContinue
    $activeDriverVer = $null
    if ($activeText -and $activeText -match "(?im)^\s*DriverVer\s*=\s*([^\r\n]+)") {
      $activeDriverVer = $Matches[1].Trim()
    }
    if ($activeDriverVer) {
      $driverStoreMatches = @()
      foreach ($dir in $dirs) {
        $candidate = Join-Path $dir.FullName "helios_kmd_render.inf"
        if (-not (Test-Path -LiteralPath $candidate)) { continue }
        $candidateText = Get-Content -LiteralPath $candidate -Raw -ErrorAction SilentlyContinue
        if ($candidateText -and $candidateText -match "(?im)^\s*DriverVer\s*=\s*([^\r\n]+)" -and $Matches[1].Trim() -eq $activeDriverVer) {
          $driverStoreMatches += $dir
        }
      }
      if ($driverStoreMatches.Count -eq 1) {
        return $driverStoreMatches[0].FullName
      }
      if ($driverStoreMatches.Count -gt 1) {
        $chosen = $driverStoreMatches | Sort-Object LastWriteTime -Descending | Select-Object -First 1
        Write-Warning "Multiple DriverStore packages match active DriverVer $activeDriverVer; using newest $($chosen.FullName)"
        return $chosen.FullName
      }
    }
  }

  $classKey = Get-HeliosClassKey $InstanceId
  $p = Get-ItemProperty -LiteralPath $classKey -ErrorAction Stop
  foreach ($name in @($p.UserModeDriverName)) {
    if (-not $name) { continue }
    $expanded = [Environment]::ExpandEnvironmentVariables([string]$name)
    if ([IO.Path]::IsPathRooted($expanded)) {
      $dir = Split-Path -Parent $expanded
      if ((Test-Path -LiteralPath (Join-Path $dir "helios_kmd_render.inf")) -and (Test-Path -LiteralPath (Join-Path $dir "helios_umd.dll"))) {
        return $dir
      }
    }
  }
  throw "No active Helios DriverStore package directory found from $classKey / $ActiveInfName."
}

function Get-HeliosPnpState([string]$InstanceId) {
  return Get-CimInstance Win32_PnPEntity | Where-Object { $_.PNPDeviceID -eq $InstanceId } |
    Select-Object Name,Status,ConfigManagerErrorCode,PNPDeviceID
}

function Stop-LookingGlassHostService {
  $svc = Get-Service -Name "Looking Glass (host)" -ErrorAction SilentlyContinue
  if (-not $svc) { $svc = Get-Service -Name "looking-glass-host" -ErrorAction SilentlyContinue }
  if (-not $svc) {
    $svc = Get-Service -ErrorAction SilentlyContinue |
      Where-Object { $_.DisplayName -eq "Looking Glass (host)" -or $_.DisplayName -like "Looking Glass*host*" } |
      Select-Object -First 1
  }
  if ($svc) {
    if ($svc.Status -ne "Stopped") {
      Stop-Service -Name $svc.Name -Force -ErrorAction SilentlyContinue
      $svc.WaitForStatus("Stopped", [TimeSpan]::FromSeconds(10))
    }
    Set-Service -Name $svc.Name -StartupType Disabled -ErrorAction SilentlyContinue
    Write-Host "Looking Glass host service is stopped/disabled. IDD mode uses LGIddHelper, not the host service."
  }
}

function Clear-HeliosPendingRenames {
  $path = "HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager"
  $name = "PendingFileRenameOperations"
  $item = Get-ItemProperty -LiteralPath $path -Name $name -ErrorAction SilentlyContinue
  if (-not $item) { return 0 }
  $value = $item.$name
  if (-not $value) { return 0 }
  $pairs = @($value)
  $kept = @()
  $removed = 0
  for ($i = 0; $i -lt $pairs.Count; $i += 2) {
    $a = [string]$pairs[$i]
    $b = if (($i + 1) -lt $pairs.Count) { [string]$pairs[$i + 1] } else { "" }
    if ($a -match "HeliosUmd|helios_umd|HeliosVulkan|vulkan_virtio" -or $b -match "HeliosUmd|helios_umd|HeliosVulkan|vulkan_virtio") {
      $removed++
      continue
    }
    $kept += $a
    $kept += $b
  }
  if ($removed -gt 0) {
    if ($kept.Count -gt 0) {
      Set-ItemProperty -LiteralPath $path -Name $name -Value ([string[]]$kept)
    } else {
      Remove-ItemProperty -LiteralPath $path -Name $name -ErrorAction SilentlyContinue
    }
  }
  return $removed
}

function Grant-HeliosReadExecute([string]$Path) {
  New-Item -ItemType Directory -Force -Path $Path | Out-Null
  & icacls.exe $Path /grant "*S-1-1-0:RX" /T | Out-Null
  & icacls.exe $Path /grant "*S-1-15-2-1:RX" /T | Out-Null
}

function Write-HeliosPlan([string]$Title, [hashtable]$Values) {
  Write-Host "== $Title =="
  foreach ($k in ($Values.Keys | Sort-Object)) {
    Write-Host ("{0}: {1}" -f $k, $Values[$k])
  }
}
