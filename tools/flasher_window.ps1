# flasher_window.ps1 — show a color-cycling window for ~90s in session 1 to force a
# steady stream of dwm composition frames (advances the WUDFHost staged-probe tick
# counter to its next every-600 probe). Diagnostic only; closes itself.
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$f = New-Object System.Windows.Forms.Form
$f.FormBorderStyle = 'None'
$f.StartPosition = 'Manual'
$f.Location = New-Object System.Drawing.Point(200, 150)
$f.Size = New-Object System.Drawing.Size(900, 600)
$f.TopMost = $true
$colors = @([System.Drawing.Color]::Red, [System.Drawing.Color]::Lime, [System.Drawing.Color]::Blue, [System.Drawing.Color]::Yellow)
$i = 0
$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 100
$timer.Add_Tick({ $script:i++; $f.BackColor = $colors[$script:i % 4]; if ($script:i -ge 900) { $f.Close() } })
$f.Add_Shown({ $timer.Start() })
[System.Windows.Forms.Application]::Run($f)
