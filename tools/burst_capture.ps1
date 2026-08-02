# Burst guest-side screenshots with per-frame luminance, to separate
# "the app's CONTENT is black" from "the DISPLAY PATH shows black".
#
# `Graphics.CopyFromScreen` reads the COMPOSED PRIMARY inside the guest. It
# never touches the virtio scan-out, QEMU, SDL or the host GPU. So:
#
#   dark frames HERE      -> the blackness is already in what Windows composed,
#                            i.e. upstream of the whole scan-out path, and every
#                            bind/flush/flip fix is the wrong tree.
#   clean frames HERE     -> Windows composed a good frame and the blackness is
#                            introduced between the primary and the host window.
#
# Must run in the INTERACTIVE session (session 0 has no desktop) — go through a
# scheduled task.
#
# Writes C:\ProgramData\Helios\burst\lum.csv (t_ms,mean,min,max) every sample,
# and saves the PNG only for samples below -DarkBelow, so a long run does not
# fill the disk with identical good frames.
param(
    [int]$Seconds = 60,
    [int]$IntervalMs = 250,
    [int]$DarkBelow = 24,
    [string]$OutDir = 'C:\ProgramData\Helios\burst'
)

$ErrorActionPreference = 'Continue'
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
Get-ChildItem "$OutDir\*" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue

$w = [System.Windows.Forms.SystemInformation]::VirtualScreen.Width
$h = [System.Windows.Forms.SystemInformation]::VirtualScreen.Height
if (-not $w -or $w -le 0) {
    Add-Type -AssemblyName System.Windows.Forms
    $w = [System.Windows.Forms.SystemInformation]::VirtualScreen.Width
    $h = [System.Windows.Forms.SystemInformation]::VirtualScreen.Height
}
$csv = Join-Path $OutDir 'lum.csv'
Set-Content -Path $csv -Value 't_ms,mean,min,max,saved'

$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$n = 0

# Sample a coarse grid rather than every pixel: 24x16 = 384 points is plenty to
# tell "this frame is black" from "this frame has content", and it keeps the
# per-sample cost far below the interval so the sampler does not become the
# thing being measured.
$cols = 24; $rows = 16
while ($sw.Elapsed.TotalSeconds -lt $Seconds) {
    $t = [int]$sw.Elapsed.TotalMilliseconds
    try { $g.CopyFromScreen(0, 0, 0, 0, $bmp.Size) } catch { continue }
    $sum = 0; $mn = 255; $mx = 0
    for ($i = 1; $i -lt $cols; $i++) {
        for ($j = 1; $j -lt $rows; $j++) {
            $px = $bmp.GetPixel([int]($w * $i / $cols), [int]($h * $j / $rows))
            $l = [int](($px.R + $px.G + $px.B) / 3)
            $sum += $l
            if ($l -lt $mn) { $mn = $l }
            if ($l -gt $mx) { $mx = $l }
        }
    }
    $mean = [int]($sum / (($cols - 1) * ($rows - 1)))
    $saved = 0
    if ($mean -lt $DarkBelow) {
        $bmp.Save((Join-Path $OutDir ("dark_{0:d5}_{1:d3}.png" -f $t, $mean)),
                  [System.Drawing.Imaging.ImageFormat]::Png)
        $saved = 1
    }
    Add-Content -Path $csv -Value ('{0},{1},{2},{3},{4}' -f $t, $mean, $mn, $mx, $saved)
    $n++
    Start-Sleep -Milliseconds $IntervalMs
}
$g.Dispose(); $bmp.Dispose()
"burst_capture: $n samples over $Seconds s -> $csv"
