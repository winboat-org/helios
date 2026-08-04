[CmdletBinding()]
param(
    [ValidateSet('Dump', 'Invoke')]
    [string]$Mode = 'Dump',
    [string]$NamePattern = '',
    [int]$MatchIndex = 0,
    [string]$OutFile = 'C:\ProgramData\Helios\3dmark-uia.tsv',
    [string]$ErrorFile = 'C:\ProgramData\Helios\3dmark-uia-error.txt'
)

$ErrorActionPreference = 'Stop'
trap {
    $parent = Split-Path -Parent $ErrorFile
    if ($parent) {
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
    }
    @(
        ($_ | Out-String),
        $_.ScriptStackTrace
    ) | Set-Content -LiteralPath $ErrorFile
    exit 1
}
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

function Get-SafeProperty {
    param(
        [System.Windows.Automation.AutomationElement]$Element,
        [System.Windows.Automation.AutomationProperty]$Property,
        [object]$Fallback = ''
    )

    try {
        $value = $Element.GetCurrentPropertyValue($Property, $true)
        if ($value -eq [System.Windows.Automation.AutomationElement]::NotSupported) {
            return $Fallback
        }
        return $value
    } catch {
        return $Fallback
    }
}

function Convert-Field {
    param([object]$Value)
    return ([string]$Value) -replace "[\t\r\n]", ' '
}

$processes = @(Get-Process -Name '3DMark' -ErrorAction SilentlyContinue)
if ($processes.Count -eq 0) {
    throw 'No 3DMark process is running.'
}

$pidConditions = @($processes | ForEach-Object {
    New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::ProcessIdProperty,
        $_.Id
    )
})
$processCondition = $pidConditions[0]
for ($index = 1; $index -lt $pidConditions.Count; $index++) {
    $processCondition = New-Object System.Windows.Automation.OrCondition(
        $processCondition,
        $pidConditions[$index]
    )
}

$roots = [System.Windows.Automation.AutomationElement]::RootElement.FindAll(
    [System.Windows.Automation.TreeScope]::Children,
    $processCondition
)
$elements = New-Object System.Collections.Generic.List[System.Windows.Automation.AutomationElement]
foreach ($root in $roots) {
    $elements.Add($root)
    try {
        $children = $root.FindAll(
            [System.Windows.Automation.TreeScope]::Descendants,
            [System.Windows.Automation.Condition]::TrueCondition
        )
        foreach ($child in $children) {
            $elements.Add($child)
        }
    } catch {
        # A disappearing Chromium helper window must not discard the remaining
        # exact automation tree.
    }
}

$rows = New-Object System.Collections.Generic.List[string]
$rows.Add('index`tpid`thwnd`ttype`tname`tautomation_id`tclass`tenabled`toffscreen`trect`tpatterns')
$matches = New-Object System.Collections.Generic.List[System.Windows.Automation.AutomationElement]
for ($index = 0; $index -lt $elements.Count; $index++) {
    $element = $elements[$index]
    $name = Get-SafeProperty $element ([System.Windows.Automation.AutomationElement]::NameProperty)
    $automationId = Get-SafeProperty $element ([System.Windows.Automation.AutomationElement]::AutomationIdProperty)
    $className = Get-SafeProperty $element ([System.Windows.Automation.AutomationElement]::ClassNameProperty)
    $controlType = Get-SafeProperty $element ([System.Windows.Automation.AutomationElement]::ControlTypeProperty)
    $elementPid = Get-SafeProperty $element ([System.Windows.Automation.AutomationElement]::ProcessIdProperty) 0
    $hwnd = Get-SafeProperty $element ([System.Windows.Automation.AutomationElement]::NativeWindowHandleProperty) 0
    $enabled = Get-SafeProperty $element ([System.Windows.Automation.AutomationElement]::IsEnabledProperty) $false
    $offscreen = Get-SafeProperty $element ([System.Windows.Automation.AutomationElement]::IsOffscreenProperty) $true
    $rect = Get-SafeProperty $element ([System.Windows.Automation.AutomationElement]::BoundingRectangleProperty)
    $patterns = ''
    try {
        $patterns = ($element.GetSupportedPatterns() | ForEach-Object { $_.ProgrammaticName }) -join ','
    } catch {
    }

    $typeName = if ($controlType) { $controlType.ProgrammaticName } else { '' }
    $rows.Add((@(
        $index, $elementPid, $hwnd, $typeName, (Convert-Field $name),
        (Convert-Field $automationId), (Convert-Field $className), $enabled,
        $offscreen, (Convert-Field $rect), (Convert-Field $patterns)
    ) -join "`t"))

    if ($NamePattern -and ([string]$name -match $NamePattern)) {
        $matches.Add($element)
    }
}

$parent = Split-Path -Parent $OutFile
if ($parent) {
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
}
[System.IO.File]::WriteAllLines($OutFile, $rows)

if ($Mode -eq 'Invoke') {
    if (-not $NamePattern) {
        throw 'Invoke mode requires -NamePattern.'
    }
    if ($MatchIndex -lt 0 -or $MatchIndex -ge $matches.Count) {
        throw "MatchIndex $MatchIndex is outside the $($matches.Count) matching elements."
    }

    $target = $matches[$MatchIndex]
    $pattern = $null
    if ($target.TryGetCurrentPattern(
        [System.Windows.Automation.InvokePattern]::Pattern,
        [ref]$pattern
    )) {
        ([System.Windows.Automation.InvokePattern]$pattern).Invoke()
    } else {
        throw 'The selected element does not expose InvokePattern.'
    }
}

[pscustomobject]@{
    ProcessCount = $processes.Count
    RootCount = $roots.Count
    ElementCount = $elements.Count
    MatchCount = $matches.Count
    OutFile = $OutFile
}
