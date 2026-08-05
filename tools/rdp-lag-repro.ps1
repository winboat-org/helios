# Helios RDP lag repro — generates the exact damage pattern the owner reports.
#
# Mode 'drag'  : a mid-size window swept across the desktop (~60 Hz), the
#                "moving a window slows the whole desktop down" case.
# Mode 'full'  : a maximized window repainting entirely each frame, the
#                upper bound on damage area.
# Mode 'idle'  : window shown, never moved — the control. Damage ~0.
#
# MUST run in session 1 (the interactive/RDP session): a session-0 run has no
# desktop, no DWM composition and no RDP capture, and would fake a clean result.
# Launch via a cloned interactive schtask, never via win_exec directly.
param(
    [ValidateSet('drag', 'full', 'idle')]
    [string]$Mode = 'drag',
    [int]$Seconds = 20
)

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$form = New-Object System.Windows.Forms.Form
$form.Text = "helios rdp repro [$Mode]"
$form.StartPosition = 'Manual'
$form.BackColor = [System.Drawing.Color]::FromArgb(30, 144, 255)

if ($Mode -eq 'full') {
    $form.WindowState = 'Maximized'
} else {
    $form.Size = New-Object System.Drawing.Size(760, 540)
    $form.Location = New-Object System.Drawing.Point(120, 120)
}

$form.Show()
$form.Activate()

$sw = [System.Diagnostics.Stopwatch]::StartNew()
$frames = 0

while ($sw.Elapsed.TotalSeconds -lt $Seconds) {
    $t = $sw.Elapsed.TotalSeconds

    switch ($Mode) {
        'drag' {
            # Sweep a large ellipse so old+new rects both dirty each frame.
            $x = [int](480 + 420 * [Math]::Cos($t * 2.2))
            $y = [int](250 + 220 * [Math]::Sin($t * 3.1))
            $form.Location = New-Object System.Drawing.Point($x, $y)
        }
        'full' {
            # Whole-window repaint: full-screen damage every frame.
            $c = [int](128 + 127 * [Math]::Sin($t * 4.0))
            $form.BackColor = [System.Drawing.Color]::FromArgb($c, 64, 255 - $c)
            $form.Invalidate()
        }
        'idle' { }
    }

    [System.Windows.Forms.Application]::DoEvents()
    $frames++
    Start-Sleep -Milliseconds 8
}

$form.Close()

# Record the session we actually ran in. Session 2 is the QEMU SDL/console
# session (locked, LogonUI); a repro that landed there measures the SDL scanout
# path, not RDP, and would be a silently wrong result.
$sid = (Get-Process -Id $PID).SessionId
"repro mode=$Mode seconds=$Seconds loop_iterations=$frames session=$sid" |
    Out-File -FilePath 'Z:\tmp\rdp-repro.out' -Encoding ascii
