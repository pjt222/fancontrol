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

This tool disarms that trap. It writes a safe curve, logs the fan and
temperature readings for the record, then returns the machine to the requested
mode.

Run it after any session that wrote an experimental curve -- in particular after
`Invoke-FanTableLoadTest.ps1`, whose whole purpose is writing curves that would
be wrong to leave behind.

.PARAMETER Steps
The curve to leave loaded. The default rises across the bands and respects the
V1 floors, so entering Custom mode after this is uneventful. It is deliberately
NOT the minimum table: the floors are what the validator accepts, not a curve
anyone should be left running.

It is also constant across step indices 0-3 on purpose. `encode_fan_table_bytes`
hardcodes FSID = 0, three LENOVO_FAN_TABLE_DATA rows exist with different
thresholds (sensor 3: 58,58,58,58,67,...; sensor 0: 34,36,43,127,...), and no
hardware run so far can tell which row's thresholds index the table, because
every curve measured has been constant across those indices. The previous
default 0,0,0,1,... differs there: safe under one reading, fans off at every
load temperature under the other. A leading run of 1s never stops the fan in
any band under any reading; the worst case is 1600 RPM at idle.

.PARAMETER Mode
SmartFanMode to select at the end. 1=Quiet, 2=Balanced, 3=Performance,
255=Custom. Defaults to whatever the machine was in when the tool started.

.PARAMETER VerifySeconds
How long to watch the fans after the write before switching modes. The readings
are logged, not evaluated: at idle the machine sits below the lowest temperature
threshold, where every curve stops the fans, so a zero reading proves nothing
unless the CPU is above about 58 C, and this tool cannot arrange that. Read the
log with the temperature beside each reading in mind. A failed RPM read is
counted separately rather than recorded as 0.

.NOTES
Writes to the EC. Requires elevation.
#>
[CmdletBinding()]
param(
    [string]$ExePath,
    [string]$LogPath,
    [string]$Steps = '1,1,1,1,2,4,6,7,8,10',
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
# Under PowerShell 5.1 with $ErrorActionPreference = 'Stop', the first stderr
# line of a native command merged by 2>&1 becomes a terminating
# NativeCommandError (measured 2026-09-02 on 5.1.26100). The exe writes stderr
# only on failure, so with 'Stop' in force the failure branch below could never
# run in the one case it exists for.
$ErrorActionPreference = 'Continue'
try {
    $output = & $ExePath set-curve --fan-id 0 --sensor-id 3 --steps $Steps 2>&1 | Out-String
} finally {
    $ErrorActionPreference = 'Stop'
}
foreach ($line in ($output -split "`r?`n")) {
    if ($line.Trim().Length -gt 0) { Write-ToolLog ("    " + $line.TrimEnd()) }
}
$writeExit = $LASTEXITCODE
$result['writeExitCode'] = $writeExit
Write-ToolLog ("  exit code: " + $writeExit)

if ($writeExit -ne 0) {
    # Leaving Custom selected with a failed write is the exact hazard this tool
    # exists to clear, so get out of Custom before reporting the failure.
    # $Mode defaults to the starting mode, and every successful set-curve
    # leaves the machine in Custom (255), so from the state this branch most
    # needs to handle, "return to $Mode" would select Custom again. Balanced
    # (2) is fan::smart_fan_mode::SAFE_FALLBACK, the same choice the TUI makes
    # when it clears its held curves.
    $exitMode = if ($Mode -eq 255) { 2 } else { $Mode }
    Write-ToolLog ("ERROR: the curve write failed. Selecting mode " + $exitMode + " so Custom is not left selected.")
    try { [void]$gameZone.SetSmartFanMode($exitMode) } catch { Write-ToolLog ("  restore failed: " + $_.Exception.Message) }
    Write-ToolLog ("SmartFanMode now: " + (Get-Mode))
    $result['ok'] = $false
    New-Object PSObject -Property $result
    exit 1
}

Write-ToolLog ""
Write-ToolLog ("Watching fans for " + $VerifySeconds + "s ...")
$deadline = (Get-Date).AddSeconds($VerifySeconds)
$readings = @()
$nullReads = 0
while ((Get-Date) -lt $deadline) {
    $t = $null
    try {
        $tr = $fanMethod.Fan_GetCurrentSensorTemperature(3)
        $t = Get-WmiPropertyOrNull -InputObject $tr -Name 'CurrentSensorTemperature'
    } catch { $t = $null }
    $r0 = Get-Rpm -FanId 0
    $r1 = Get-Rpm -FanId 1
    # [int]$null is 0, which would record a failed read as "fan stopped".
    if ($null -ne $r0) { $readings += [int]$r0 } else { $nullReads += 1 }
    Write-ToolLog ("  s3={0}C  f0={1}rpm  f1={2}rpm" -f $t, $r0, $r1)
    Start-Sleep -Seconds 2
}
$result['rpmReadings'] = $readings
$result['rpmReadNulls'] = $nullReads
if ($nullReads -gt 0) { Write-ToolLog ("  " + $nullReads + " fan 0 read(s) returned nothing and are not in rpmReadings.") }

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
