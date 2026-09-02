<#
.SYNOPSIS
Determine whether the power-button LED colour is readable, by invoking
Get_Lighting_Current_Status for each Lighting_Id across a SmartFanMode sweep.

.DESCRIPTION
The user reports the power-button LED is blue in Quiet, white in Balanced and
red in Performance, and appears to light all three at once in Custom. This tool
tests whether any of that is visible to software.

`Get-LenovoLedSurface.ps1` found the surface but never called the getter, so its
argument shape and -- the part that matters under this project's conventions --
its named output property names are unknown. Nothing may be parsed against a
guessed property name, so this tool dumps every property of every result rather
than reading one.

The evidence so far leans towards "not readable": LENOVO_LIGHTING_DATA exposes
Brightness_Level, Control_Interface, Default_State, Lighting_Id, Lighting_Type
and State_Type_Num, and nothing colour-shaped anywhere. If that holds, any LED
indicator in the UI is *derived* from SmartFanMode and must be labelled as such.
A negative result here is therefore a real result, not a failed run.

Pairing the read with a mode sweep is the point. A lighting reading taken in one
mode says nothing; the same reading taken in four modes either varies with the
mode or does not.

.PARAMETER SafeSteps
The curve written before the sweep. The default is constant across step
indices 0-3 on purpose. `encode_fan_table_bytes` hardcodes FSID = 0 and three
LENOVO_FAN_TABLE_DATA rows exist with different thresholds (sensor 3:
58,58,58,58,67,...; sensor 0: 34,36,43,127,...), and neither hardware run so far
can tell which row's thresholds index the table, because both used curves that
were constant across those indices. A curve that differs there -- the previous
default 0,0,0,1,... -- is safe under one reading and stops the fans at every
load temperature under the other. A leading run of 1s never stops the fan in
any band under any reading; the worst case is 1600 RPM at idle.

.PARAMETER SkipSafeCurve
Do not write a safe curve before sweeping. The sweep enters Custom mode, and
Custom mode runs whatever table the EC last received -- which after a probe
session may be a curve that stops the fans. Writing a known-good curve first is
what makes entering Custom safe, so skip this only when the EC state is already
known.

.PARAMETER DwellSeconds
How long to hold each mode. Long enough for a person to look at the power button
and report what they see, since the firmware may not tell us.

.NOTES
Writes to the EC: changes SmartFanMode, and writes a fan curve unless
-SkipSafeCurve. Requires elevation. Returns to the starting mode at the end.
#>
[CmdletBinding()]
param(
    [string]$ExePath,
    [string]$LogPath,
    [int[]]$LightingIds = @(0, 1, 2, 3, 4, 5),
    [int[]]$SweepModes = @(1, 2, 3, 255),
    [int]$DwellSeconds = 6,
    [string]$SafeSteps = '1,1,1,1,2,4,6,7,8,10',
    [switch]$SkipSafeCurve,
    [switch]$Json
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'LenovoWmi.psm1') -Force

if (-not $LogPath) { $LogPath = Join-Path $PSScriptRoot 'Get-LenovoLighting.log' }
if (-not $ExePath) {
    $ExePath = Join-Path (Split-Path $PSScriptRoot -Parent) 'target\x86_64-pc-windows-gnu\release\fancontrol.exe'
}

Start-ToolLog -Path $LogPath -Title 'Lenovo lighting readability vs SmartFanMode'
Write-ToolLog "Changes SmartFanMode, and writes a fan curve unless -SkipSafeCurve."
Write-ToolLog ""

$result = [ordered]@{ ok = $false }
$script:StartMode = $null
$gz = $null

function Get-Mode {
    try { $r = $gz.GetSmartFanMode() } catch { return $null }
    foreach ($n in @('Data', 'mode', 'Mode', 'SmartFanMode')) {
        $v = Get-WmiPropertyOrNull -InputObject $r -Name $n
        if ($null -ne $v) { return $v }
    }
    return $null
}

try {
    if (-not (Test-Elevated)) { Write-ToolLog "FATAL: not elevated."; exit 1 }

    $gz = Get-LenovoWmiClass -ClassName 'LENOVO_GAMEZONE_DATA' -Single
    $lm = Get-LenovoWmiClass -ClassName 'LENOVO_LIGHTING_METHOD' -Single
    $fm = Get-LenovoWmiClass -ClassName 'LENOVO_FAN_METHOD' -Single
    if ($null -eq $gz) { Write-ToolLog "FATAL: LENOVO_GAMEZONE_DATA unavailable."; exit 1 }
    if ($null -eq $lm) {
        # Not a failure of the run -- it answers the question, negatively.
        Write-ToolLog "RESULT: LENOVO_LIGHTING_METHOD is absent on this firmware."
        Write-ToolLog "Any LED indicator must be derived from SmartFanMode and labelled as derived."
        $result['lightingMethodPresent'] = $false
        New-Object PSObject -Property $result
        exit 0
    }
    $result['lightingMethodPresent'] = $true

    $script:StartMode = Get-Mode
    Write-ToolLog ("SmartFanMode at start: " + $script:StartMode)
    $result['startMode'] = $script:StartMode
    if ($null -eq $script:StartMode) { Write-ToolLog "FATAL: cannot read SmartFanMode, so it could not be restored."; exit 1 }

    # --- safe curve, so entering Custom mid-sweep is uneventful -------------
    if (-not $SkipSafeCurve) {
        if (-not (Test-Path $ExePath)) {
            Write-ToolLog ("FATAL: fancontrol.exe not found at " + $ExePath + ". Pass -SkipSafeCurve only if the EC state is known.")
            exit 1
        }
        Write-ToolLog ""
        Write-ToolLog ("Writing safe curve first (" + $SafeSteps + "), so Custom mode is not a fans-off trap.")
        # Under PowerShell 5.1 with $ErrorActionPreference = 'Stop', the first
        # stderr line of a native command merged by 2>&1 becomes a terminating
        # NativeCommandError (measured 2026-09-02 on 5.1.26100). The exe writes
        # stderr only on failure, so with 'Stop' in force the exit-code check
        # below could never run in the one case it exists for.
        $ErrorActionPreference = 'Continue'
        try {
            $out = & $ExePath set-curve --fan-id 0 --sensor-id 3 --steps $SafeSteps 2>&1 | Out-String
        } finally {
            $ErrorActionPreference = 'Stop'
        }
        foreach ($line in ($out -split "`r?`n")) {
            if ($line.Trim().Length -gt 0) { Write-ToolLog ("    " + $line.TrimEnd()) }
        }
        Write-ToolLog ("  exit code: " + $LASTEXITCODE)
        $result['safeCurveExitCode'] = $LASTEXITCODE
        if ($LASTEXITCODE -ne 0) {
            Write-ToolLog "FATAL: the safe curve did not land, so entering Custom mode is not safe. Stopping."
            exit 1
        }
    }

    # --- baseline lighting dump, current mode ------------------------------
    Write-ToolLog ""
    Write-ToolLog "--- Get_Lighting_Current_Status, all ids, at the starting mode ---"
    Write-ToolLog "Every property is dumped: the output property names are unknown and must not be guessed."
    foreach ($id in $LightingIds) {
        Write-ToolLog ("  Lighting_Id " + $id + ":")
        try {
            $r = $lm.Get_Lighting_Current_Status($id)
            Write-WmiProperties $r "      "
        } catch {
            Write-ToolLog ("      ERROR: " + $_.Exception.Message)
        }
    }

    # --- the sweep ---------------------------------------------------------
    $sweep = @()
    foreach ($mode in $SweepModes) {
        Write-ToolLog ""
        Write-ToolLog ("=== SmartFanMode -> " + $mode + " ===")
        try { [void]$gz.SetSmartFanMode($mode) } catch { Write-ToolLog ("  SetSmartFanMode error: " + $_.Exception.Message) }
        Start-Sleep -Milliseconds 800
        $readBack = Get-Mode
        Write-ToolLog ("  mode read back: " + $readBack)
        if ("$readBack" -ne "$mode") {
            Write-ToolLog ("  WARNING: asked for " + $mode + ", got " + $readBack + ". Readings below are for " + $readBack + ".")
        }

        Write-ToolLog ("  >>> LOOK AT THE POWER BUTTON NOW -- holding this mode for " + $DwellSeconds + "s <<<")

        foreach ($id in $LightingIds) {
            try {
                $r = $lm.Get_Lighting_Current_Status($id)
                Write-ToolLog ("  Lighting_Id " + $id + " ->")
                Write-WmiProperties $r "      "
            } catch {
                Write-ToolLog ("  Lighting_Id " + $id + " -> ERROR: " + $_.Exception.Message)
            }
        }

        # LENOVO_LIGHTING_DATA is static per instance, but re-read it per mode:
        # if any field tracks the mode, that is the readable signal we are after.
        Write-ToolLog "  LENOVO_LIGHTING_DATA instances:"
        try {
            $rows = @(Get-WmiObject -Namespace root/WMI -Class LENOVO_LIGHTING_DATA -ErrorAction Stop)
            foreach ($row in $rows) {
                $lid = Get-WmiPropertyOrNull -InputObject $row -Name 'Lighting_Id'
                $bri = Get-WmiPropertyOrNull -InputObject $row -Name 'Brightness_Level'
                $st = Get-WmiPropertyOrNull -InputObject $row -Name 'State_Type_Num'
                $ltype = Get-WmiPropertyOrNull -InputObject $row -Name 'Lighting_Type'
                $ci = Get-WmiPropertyOrNull -InputObject $row -Name 'Control_Interface'
                Write-ToolLog ("      Lighting_Id={0}  Type={1}  Brightness={2}  StateTypeNum={3}  ControlIface={4}" -f $lid, $ltype, $bri, $st, $ci)
            }
        } catch {
            Write-ToolLog ("      ERROR: " + $_.Exception.Message)
        }

        $fanRpm = $null
        if ($null -ne $fm) {
            try { $fanRpm = Get-WmiPropertyOrNull -InputObject ($fm.Fan_GetCurrentFanSpeed(0)) -Name 'CurrentFanSpeed' } catch { $fanRpm = $null }
        }
        Write-ToolLog ("  fan 0: " + $fanRpm + " rpm (0.8 s after the switch; spin-up not yet visible)")

        Start-Sleep -Seconds $DwellSeconds

        # Second reading after the dwell. The first is taken before a fan can
        # spin up from 0, so it cannot distinguish "off" from "starting". This
        # one can, and in Custom mode at idle it is a free measurement: 0 RPM
        # here means the sensor 3 row (lowest band 58 C) indexes the table and
        # the EC runs the fan off below its lowest band. 1600 RPM is consistent
        # with either the sensor 0 row (lowest band 34 C) or an EC that uses
        # index 0 below band, so only the 0 result is decisive.
        $fanRpmAfter = $null
        if ($null -ne $fm) {
            try { $fanRpmAfter = Get-WmiPropertyOrNull -InputObject ($fm.Fan_GetCurrentFanSpeed(0)) -Name 'CurrentFanSpeed' } catch { $fanRpmAfter = $null }
        }
        Write-ToolLog ("  fan 0: " + $fanRpmAfter + " rpm (after " + $DwellSeconds + " s dwell)")

        $sweep += New-Object PSObject -Property ([ordered]@{ requested = $mode; readBack = $readBack; fanRpm = $fanRpm; fanRpmAfterDwell = $fanRpmAfter })
    }
    $result['sweep'] = $sweep
    $result['ok'] = $true
} finally {
    if ($null -ne $gz -and $null -ne $script:StartMode) {
        Write-ToolLog ""
        Write-ToolLog ("Restoring SmartFanMode to " + $script:StartMode + ".")
        try { [void]$gz.SetSmartFanMode($script:StartMode) } catch { Write-ToolLog ("  ERROR: " + $_.Exception.Message) }
        Start-Sleep -Milliseconds 500
        Write-ToolLog ("  read back: " + (Get-Mode))
    }
}

Write-ToolLog ""
Write-ToolLog "Read the dumps above with one question in mind: does ANY field differ between modes?"
Write-ToolLog "If none does, the colour is not readable here and the UI indicator must be derived and labelled so."
Write-ToolLog ("Log: " + $LogPath)

if ($Json) { New-Object PSObject -Property $result | ConvertTo-Json -Depth 5 }
else { New-Object PSObject -Property $result }
