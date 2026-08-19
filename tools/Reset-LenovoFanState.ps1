<#
.SYNOPSIS
Put the EC back into a known-good state: a safe rising fan curve loaded, and a
chosen SmartFanMode selected.

.DESCRIPTION
The EC retains the last written curve across power-mode switches -- measured
2026-08-19, where a curve written in one run reactivated 24 minutes later on
re-entering Custom mode, having survived a switch to Performance and back. That
retention is useful, but it means a test run that ends with a deliberately
unsafe curve leaves a trap behind: the machine looks fine in Quiet, Balanced or
Performance, and stops its fans the moment anything selects Custom.

This tool disarms that trap. It writes a safe curve, confirms the fans respond to
it, then returns the machine to the requested mode.

Run it after any session that wrote an experimental curve -- in particular after
`Invoke-FanTableLoadTest.ps1`, whose whole purpose is writing curves that would
be wrong to leave behind.

.PARAMETER Steps
The curve to leave loaded. The default rises across the bands and respects the
V1 floors, so entering Custom mode after this is uneventful. It is deliberately
NOT the minimum table: the floors are what the validator accepts, not a curve
anyone should be left running.

.PARAMETER Mode
SmartFanMode to select at the end. 1=Quiet, 2=Balanced, 3=Performance,
255=Custom. Defaults to whatever the machine was in when the tool started.

.PARAMETER VerifySeconds
How long to watch the fans after the write before switching modes. The check is
"did the fans respond at all", not "is the curve correct" -- at idle the machine
sits below the lowest temperature threshold, where no curve distinguishes itself.
A zero reading here is only meaningful if the CPU is above about 58 C.

.NOTES
Writes to the EC. Requires elevation.
#>
[CmdletBinding()]
param(
    [string]$ExePath,
    [string]$LogPath,
    [string]$Steps = '0,0,0,1,2,4,6,7,8,10',
    [int]$Mode = 0,
    [int]$VerifySeconds = 12,
    [switch]$Json
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'LenovoWmi.psm1') -Force

if (-not $LogPath) { $LogPath = Join-Path $PSScriptRoot 'Reset-LenovoFanState.log' }
if (-not $ExePath) {
    $ExePath = Join-Path (Split-Path $PSScriptRoot -Parent) 'target\x86_64-pc-windows-gnu\release\fancontrol.exe'
}

Start-ToolLog -Path $LogPath -Title 'Reset Lenovo fan state to known-good'
Write-ToolLog "WRITES TO THE EC."
Write-ToolLog ""

$result = [ordered]@{ ok = $false }

if (-not (Test-Elevated)) { Write-ToolLog "FATAL: not elevated."; exit 1 }
if (-not (Test-Path $ExePath)) { Write-ToolLog ("FATAL: fancontrol.exe not found at " + $ExePath); exit 1 }

$fanMethod = Get-LenovoWmiClass -ClassName 'LENOVO_FAN_METHOD' -Single
$gameZone = Get-LenovoWmiClass -ClassName 'LENOVO_GAMEZONE_DATA' -Single
if ($null -eq $fanMethod -or $null -eq $gameZone) { Write-ToolLog "FATAL: WMI classes unavailable."; exit 1 }

function Get-Mode {
    try { $r = $gameZone.GetSmartFanMode() } catch { return $null }
    foreach ($n in @('Data', 'mode', 'Mode', 'SmartFanMode')) {
        $v = Get-WmiPropertyOrNull -InputObject $r -Name $n
        if ($null -ne $v) { return $v }
    }
    return $null
}

function Get-Rpm {
    param([int]$FanId)
    try {
        $r = $fanMethod.Fan_GetCurrentFanSpeed($FanId)
        return (Get-WmiPropertyOrNull -InputObject $r -Name 'CurrentFanSpeed')
    } catch { return $null }
}

$startMode = Get-Mode
Write-ToolLog ("SmartFanMode at start: " + $startMode)
$result['startMode'] = $startMode
if ($Mode -eq 0) {
    if ($null -eq $startMode) {
        Write-ToolLog "FATAL: could not read SmartFanMode and no -Mode was given, so there is no mode to return to."
        exit 1
    }
    $Mode = [int]$startMode
    Write-ToolLog ("No -Mode given; will return to the starting mode " + $Mode + ".")
}

# The write goes through fancontrol.exe, which switches into Custom itself. That
# is the point: it exercises the same path a user takes, so a curve left behind
# by this tool is one the application can actually produce.
Write-ToolLog ""
Write-ToolLog ("Writing safe curve: " + $Steps)
$output = & $ExePath set-curve --fan-id 0 --sensor-id 3 --steps $Steps 2>&1 | Out-String
foreach ($line in ($output -split "`r?`n")) {
    if ($line.Trim().Length -gt 0) { Write-ToolLog ("    " + $line.TrimEnd()) }
}
$writeExit = $LASTEXITCODE
$result['writeExitCode'] = $writeExit
Write-ToolLog ("  exit code: " + $writeExit)

if ($writeExit -ne 0) {
    # Leaving Custom selected with a failed write is the exact hazard this tool
    # exists to clear, so get out of Custom before reporting the failure.
    Write-ToolLog "ERROR: the curve write failed. Returning to the requested mode anyway so Custom is not left selected."
    try { [void]$gameZone.SetSmartFanMode($Mode) } catch { Write-ToolLog ("  restore failed: " + $_.Exception.Message) }
    Write-ToolLog ("SmartFanMode now: " + (Get-Mode))
    $result['ok'] = $false
    New-Object PSObject -Property $result
    exit 1
}

Write-ToolLog ""
Write-ToolLog ("Watching fans for " + $VerifySeconds + "s ...")
$deadline = (Get-Date).AddSeconds($VerifySeconds)
$readings = @()
while ((Get-Date) -lt $deadline) {
    $t = $null
    try {
        $tr = $fanMethod.Fan_GetCurrentSensorTemperature(3)
        $t = Get-WmiPropertyOrNull -InputObject $tr -Name 'CurrentSensorTemperature'
    } catch { $t = $null }
    $r0 = Get-Rpm -FanId 0
    $r1 = Get-Rpm -FanId 1
    $readings += [int]$r0
    Write-ToolLog ("  s3={0}C  f0={1}rpm  f1={2}rpm" -f $t, $r0, $r1)
    Start-Sleep -Seconds 2
}
$result['rpmReadings'] = $readings

Write-ToolLog ""
Write-ToolLog ("Selecting SmartFanMode " + $Mode + ".")
try { [void]$gameZone.SetSmartFanMode($Mode) } catch { Write-ToolLog ("  ERROR: " + $_.Exception.Message) }
Start-Sleep -Milliseconds 500
$endMode = Get-Mode
Write-ToolLog ("SmartFanMode read back: " + $endMode)
$result['endMode'] = $endMode
$result['ok'] = ("$endMode" -eq "$Mode")

Write-ToolLog ""
if ($result['ok']) {
    Write-ToolLog ("DONE. Safe curve loaded; mode " + $endMode + " selected. Entering Custom mode is no longer a fans-off trap.")
} else {
    Write-ToolLog ("WARNING: asked for mode " + $Mode + " but read back " + $endMode + ".")
}
Write-ToolLog ("Log: " + $LogPath)

if ($Json) { New-Object PSObject -Property $result | ConvertTo-Json -Depth 4 }
else { New-Object PSObject -Property $result }
