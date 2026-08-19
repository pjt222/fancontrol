<#
.SYNOPSIS
Enumerate what this firmware exposes for the power-button LED and for lighting
generally, so a virtual indicator can be driven from a real reading rather than
inferred from SmartFanMode.

.DESCRIPTION
Read-only discovery. Lists every LENOVO_* class in root\WMI with its properties
and methods, then looks specifically for lighting and LED surfaces and dumps
their current values.

The question behind it: the power-button LED is blue in Quiet, white in Balanced
and red in Performance, and appears to light all three at once in Custom. If the
colour is readable, the indicator reflects hardware. If it is not, any indicator
is derived from SmartFanMode and must be labelled as derived -- which matters
precisely because Custom seems not to map to a single colour.

.NOTES
Requires elevation: root\WMI returns access denied otherwise.
#>
[CmdletBinding()]
param([string]$LogPath, [switch]$Json)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'LenovoWmi.psm1') -Force

if (-not $LogPath) { $LogPath = Join-Path $PSScriptRoot 'Get-LenovoLedSurface.log' }
Start-ToolLog -Path $LogPath -Title 'Lenovo LED / lighting surface discovery' -ReadOnly

if (-not (Test-Elevated)) { Write-ToolLog "FATAL: not elevated."; exit 1 }

$result = [ordered]@{}

# ---------------------------------------------------------------------------
# 1. Every LENOVO_* class in root\WMI
# ---------------------------------------------------------------------------
Write-ToolLog "--- LENOVO_* classes in root\WMI ---"
$classNames = @()
try {
    $classes = Get-WmiObject -Namespace root\WMI -List -ErrorAction Stop |
        Where-Object { $_.Name -like 'LENOVO*' } | Sort-Object Name
    foreach ($cls in $classes) {
        $classNames += $cls.Name
        $methods = @($cls.Methods | ForEach-Object { $_.Name })
        $props = @($cls.Properties | ForEach-Object { $_.Name } | Where-Object { $_ -notlike '__*' })
        Write-ToolLog ("  " + $cls.Name)
        if ($props.Count -gt 0) { Write-ToolLog ("      props:   " + ($props -join ', ')) }
        if ($methods.Count -gt 0) { Write-ToolLog ("      methods: " + ($methods -join ', ')) }
    }
} catch {
    Write-ToolLog ("  ERROR enumerating classes: " + $_.Exception.Message)
}
$result['classes'] = $classNames
Write-ToolLog ""

# ---------------------------------------------------------------------------
# 2. Anything whose name suggests lighting
# ---------------------------------------------------------------------------
Write-ToolLog "--- classes matching light/led/lamp/rgb/spectrum ---"
$lightingClasses = @($classNames | Where-Object {
    $_ -match 'LIGHT|LED|LAMP|RGB|SPECTRUM|BACKLIGHT'
})
if ($lightingClasses.Count -eq 0) {
    Write-ToolLog "  none by name."
} else {
    foreach ($name in $lightingClasses) {
        Write-ToolLog ("  --- " + $name + " instances ---")
        $instances = Get-LenovoWmiClass -ClassName $name
        if ($null -ne $instances) {
            foreach ($inst in @($instances)) { Write-WmiProperties $inst }
        }
    }
}
$result['lightingClasses'] = $lightingClasses
Write-ToolLog ""

# ---------------------------------------------------------------------------
# 3. GAMEZONE_DATA, where the power-mode LED accessors would live
# ---------------------------------------------------------------------------
Write-ToolLog "--- LENOVO_GAMEZONE_DATA: full property dump ---"
$gz = Get-LenovoWmiClass -ClassName 'LENOVO_GAMEZONE_DATA' -Single
if ($null -ne $gz) {
    Write-WmiProperties $gz
    Write-ToolLog ""
    Write-ToolLog "--- LENOVO_GAMEZONE_DATA: methods mentioning light/led/colour ---"
    $gzClass = Get-WmiObject -Namespace root\WMI -List -Class 'LENOVO_GAMEZONE_DATA'
    $ledMethods = @($gzClass.Methods | ForEach-Object { $_.Name } |
        Where-Object { $_ -match 'Light|LED|Color|Colour|Lamp' })
    if ($ledMethods.Count -eq 0) {
        Write-ToolLog "  none."
    } else {
        foreach ($m in $ledMethods) { Write-ToolLog ("  " + $m) }
    }
    $result['gamezoneLedMethods'] = $ledMethods

    # Read the current mode alongside, so the log ties any LED reading to the
    # mode it was taken under. Without that pairing the dump says nothing about
    # the mapping the indicator needs.
    Write-ToolLog ""
    Write-ToolLog "--- current SmartFanMode (for pairing) ---"
    try {
        $modeResult = $gz.GetSmartFanMode()
        Write-WmiProperties $modeResult
        $result['smartFanMode'] = (Get-WmiPropertyOrNull -InputObject $modeResult -Name 'Data')
    } catch {
        Write-ToolLog ("  ERROR: " + $_.Exception.Message)
    }
}
Write-ToolLog ""

# ---------------------------------------------------------------------------
# 4. The known LLT lighting entry points, probed by name
# ---------------------------------------------------------------------------
Write-ToolLog "--- probing known lighting class names directly ---"
foreach ($name in @('LENOVO_LIGHTING_METHOD', 'LENOVO_LIGHTING_DATA',
                    'LENOVO_SPECTRUM_METHOD', 'LENOVO_GAMEZONE_LIGHT_PROFILE_DATA')) {
    if ($classNames -contains $name) {
        Write-ToolLog ("  " + $name + ": present (dumped above if it matched the filter)")
    } else {
        Write-ToolLog ("  " + $name + ": ABSENT on this firmware")
    }
}

Write-ToolLog ""
Write-ToolLog ("Log: " + $LogPath)

if ($Json) {
    New-Object PSObject -Property $result | ConvertTo-Json -Depth 5
} else {
    New-Object PSObject -Property $result
}
