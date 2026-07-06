# mover_window.ps1 — bounce a high-contrast window around the desktop for ~60s in
# session 1 to reproduce ghosting/trail artifacts (dirty-rect misattribution shows as
# stale window copies outside the reported damage). The window paints its own tick
# counter in large digits: a capture showing counter N at the live position plus a
# remnant counter N-k elsewhere is a k-frames-old ghost. Diagnostic only; closes itself.
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$f = New-Object System.Windows.Forms.Form
$f.FormBorderStyle = 'None'
$f.StartPosition = 'Manual'
$f.Location = New-Object System.Drawing.Point(50, 50)
$f.Size = New-Object System.Drawing.Size(420, 300)
$f.TopMost = $true
$f.BackColor = [System.Drawing.Color]::Magenta

$lbl = New-Object System.Windows.Forms.Label
$lbl.Dock = 'Fill'
$lbl.TextAlign = 'MiddleCenter'
$lbl.Font = New-Object System.Drawing.Font('Consolas', 72, [System.Drawing.FontStyle]::Bold)
$lbl.ForeColor = [System.Drawing.Color]::Black
$lbl.Text = '0'
$f.Controls.Add($lbl)

$script:tick = 0
$script:ci = 0
$script:dx = 23
$script:dy = 17
$colors = @([System.Drawing.Color]::Magenta, [System.Drawing.Color]::Cyan,
            [System.Drawing.Color]::Yellow,  [System.Drawing.Color]::Lime)

$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 20   # ~50 moves/sec
$timer.Add_Tick({
  $script:tick++
  $x = $f.Location.X + $script:dx
  $y = $f.Location.Y + $script:dy
  $maxX = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Width  - $f.Width
  $maxY = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Height - $f.Height
  $bounced = $false
  if ($x -lt 0 -or $x -gt $maxX) { $script:dx = -$script:dx; $x = [Math]::Max(0, [Math]::Min($x, $maxX)); $bounced = $true }
  if ($y -lt 0 -or $y -gt $maxY) { $script:dy = -$script:dy; $y = [Math]::Max(0, [Math]::Min($y, $maxY)); $bounced = $true }
  if ($bounced) { $script:ci++; $f.BackColor = $colors[$script:ci % 4] }
  $f.Location = New-Object System.Drawing.Point($x, $y)
  $lbl.Text = [string]$script:tick
  if ($script:tick -ge 3000) { $f.Close() }   # ~60s
})
$f.Add_Shown({ $timer.Start() })
[System.Windows.Forms.Application]::Run($f)
