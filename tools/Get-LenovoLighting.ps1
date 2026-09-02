<#
.SYNOPSIS
Determine whether the power-button LED colour is readable, by invoking
Get_Lighting_Current_Status for each Lighting_Id across a SmartFanMode sweep.

.DESCRIPTION
The user reports the power-button LED is blue in Quiet, white in Balanced and
red in Performance, and appears to light all three at once in Custom. This tool
tests whether any of that is visible to software, and records what the operator
sees beside what the firmware reports in the same mode.

Measured 2026-09-02 (issue #44): `Get_Lighting_Current_Status(<id>)` takes one
integer and returns `Current_Brightness_Level` and `Current_State_Type`. Across
the sweep exactly one field moved, `Lighting_Id 4 -> Current_State_Type`: 0 in
Quiet, 1 in Balanced, 2 in Performance, 3 in Custom. That is a state index, not
a colour. Which colour each index means still rests on a person looking at the
button, so after every mode switch the tool asks the operator what they see and
logs the answer beside the index read in that mode. One attended run therefore
yields the index-to-colour mapping, printed as a summary block at the end of the
log. Pass -NoPrompt for an unattended run.

Every property of every result is still dumped rather than parsed, so a firmware
that names its outputs differently shows up as a dump, not as a silent null. The
one parsed field is `Current_State_Type` for id 4, read null-safely against the
measured name.

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
Minimum time to hold each mode before the second fan reading. The time the
operator spends answering the prompt counts toward it, so an attended run is no
longer than an unattended one unless the operator is slower than this. The
second fan reading needs the hold: a fan spinning up from 0 is not visible in
the first reading, which is taken under two seconds after the switch. Both
readings log the measured seconds since the switch, not a nominal figure.

.PARAMETER NoPrompt
Do not ask the operator what colour the power button shows after each mode
switch. Use for unattended runs. Without it the tool blocks at a Read-Host
prompt in each mode. An empty answer is logged as "not observed", and a host
that cannot prompt (for example under -NonInteractive) is logged and skipped
rather than failing the sweep, since the state index is still worth recording
without a colour beside it.

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
    [switch]$NoPrompt,
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
$sweep = @()

function Get-Mode {
    try { $r = $gz.GetSmartFanMode() } catch { return $null }
    foreach ($n in @('Data', 'mode', 'Mode', 'SmartFanMode')) {
        $v = Get-WmiPropertyOrNull -InputObject $r -Name $n
        if ($null -ne $v) { return $v }
    }
    return $null
}

function Get-FanReading {
    # Fan 0 RPM and sensor 3 temperature together: an RPM without the
    # temperature beside it cannot be read against any curve.
    $rpm = $null; $temp = $null
    if ($null -ne $fm) {
        try { $rpm = Get-WmiPropertyOrNull -InputObject ($fm.Fan_GetCurrentFanSpeed(0)) -Name 'CurrentFanSpeed' } catch { $rpm = $null }
        try { $temp = Get-WmiPropertyOrNull -InputObject ($fm.Fan_GetCurrentSensorTemperature(3)) -Name 'CurrentSensorTemperature' } catch { $temp = $null }
    }
    return @{ rpm = $rpm; temp = $temp }
}

function Get-SecondsSince {
    # Elapsed seconds on a stopwatch, to one decimal. PowerShell converts
    # numbers to strings with the invariant culture, so this concatenates as
    # "2.3" on every locale; a -f format string would follow the OS culture.
    param([System.Diagnostics.Stopwatch]$Stopwatch)
    return [Math]::Round($Stopwatch.Elapsed.TotalSeconds, 1)
}

function Read-LedObservation {
    # Ask the operator what the power button shows and return the answer
    # verbatim, trimmed. Three outcomes are kept apart, because they mean
    # different things in the summary: text (observed), an empty string (the
    # prompt was shown and nothing was entered), and $null (never asked:
    # -NoPrompt, or a host that cannot prompt). Read-Host throws under
    # -NonInteractive and in hosts without a console; that is logged and the
    # sweep continues, since the state index is worth recording on its own.
    param(
        [int]$Mode,
        [System.Diagnostics.Stopwatch]$SinceSwitch
    )
    if ($NoPrompt) { return $null }
    $answer = $null
    try {
        $answer = Read-Host -Prompt ("  Power button colour in mode " + $Mode + " (blue / white / red / all three / off / other; Enter = not observed)")
    } catch {
        Write-ToolLog ("  operator prompt unavailable: " + $_.Exception.Message)
        return $null
    }
    if ($null -eq $answer) { $answer = '' }
    $answer = ([string]$answer).Trim()
    $when = (Get-SecondsSince $SinceSwitch)
    if ($answer.Length -eq 0) {
        Write-ToolLog ("  operator (" + $when + " s after the switch): not observed (empty answer)")
    } else {
        Write-ToolLog ("  operator (" + $when + " s after the switch): '" + $answer + "'")
    }
    return $answer
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
    Write-ToolLog "Every property is dumped, so a firmware that names its outputs differently shows as a dump and not as a silent null."
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
    foreach ($mode in $SweepModes) {
        Write-ToolLog ""
        Write-ToolLog ("=== SmartFanMode -> " + $mode + " ===")
        try { [void]$gz.SetSmartFanMode($mode) } catch { Write-ToolLog ("  SetSmartFanMode error: " + $_.Exception.Message) }
        # Every timing below is measured from here. The operator prompt makes
        # the hold variable, so no reading may carry a nominal "N s" label.
        $sinceSwitch = [System.Diagnostics.Stopwatch]::StartNew()
        Start-Sleep -Milliseconds 800
        $readBack = Get-Mode
        Write-ToolLog ("  mode read back: " + $readBack)
        if ("$readBack" -ne "$mode") {
            Write-ToolLog ("  WARNING: asked for " + $mode + ", got " + $readBack + ". Readings below are for " + $readBack + ".")
        }

        if ($NoPrompt) {
            Write-ToolLog ("  >>> LOOK AT THE POWER BUTTON NOW -- holding this mode for at least " + $DwellSeconds + "s <<<")
        } else {
            Write-ToolLog "  >>> LOOK AT THE POWER BUTTON NOW -- you will be asked what colour it shows <<<"
        }

        $state4 = $null
        foreach ($id in $LightingIds) {
            try {
                $r = $lm.Get_Lighting_Current_Status($id)
                Write-ToolLog ("  Lighting_Id " + $id + " ->")
                Write-WmiProperties $r "      "
                # The one parsed field. The property name is measured
                # (2026-09-02), not guessed, and Get-WmiPropertyOrNull returns
                # $null rather than throwing on a firmware that lacks it.
                if ($id -eq 4) { $state4 = Get-WmiPropertyOrNull -InputObject $r -Name 'Current_State_Type' }
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

        $before = Get-FanReading
        Write-ToolLog ("  fan 0: " + $before.rpm + " rpm, sensor 3: " + $before.temp + " C (" + (Get-SecondsSince $sinceSwitch) + " s after the switch; spin-up not yet visible)")

        # The early fan reading is taken before the prompt so that it stays
        # early. The prompt blocks for as long as the operator takes; that time
        # counts toward the hold, and only the remainder is slept.
        $ledSeen = Read-LedObservation -Mode $mode -SinceSwitch $sinceSwitch

        # The index above was read about a second after the switch; the colour
        # arrives some seconds later. Re-read the index now, so the pairing in
        # the summary is between two readings taken at the same moment and not
        # on the assumption that the index held still while the operator typed.
        $state4AtAnswer = $null
        if ($null -ne $ledSeen -and ($LightingIds -contains 4)) {
            try {
                $state4AtAnswer = Get-WmiPropertyOrNull -InputObject ($lm.Get_Lighting_Current_Status(4)) -Name 'Current_State_Type'
            } catch {
                Write-ToolLog ("  Lighting_Id 4 re-read at the answer -> ERROR: " + $_.Exception.Message)
            }
            if ("$state4AtAnswer" -ne "$state4") {
                Write-ToolLog ("  WARNING: Lighting_Id 4 Current_State_Type was " + $state4 + " after the switch and " + $state4AtAnswer + " when the operator answered.")
            } else {
                Write-ToolLog ("  Lighting_Id 4 Current_State_Type at the answer: " + $state4AtAnswer + " (unchanged)")
            }
        }

        $remainingSeconds = $DwellSeconds - $sinceSwitch.Elapsed.TotalSeconds
        if ($remainingSeconds -gt 0) { Start-Sleep -Milliseconds ([int][Math]::Ceiling($remainingSeconds * 1000)) }

        # Second reading after the hold. The first is taken before a fan can
        # spin up from 0, so it cannot distinguish "off" from "starting". This
        # one can. In Custom mode it bears on the open table-mapping question
        # (see CLAUDE.md), but only together with the temperature beside it:
        # 0 RPM with sensor 3 between 34 and 58 C means the sensor 3 row
        # (lowest band 58 C) indexes the table and the EC runs the fan off
        # below its lowest band, since under the sensor 0 row (lowest band
        # 34 C) a band would already match. Any non-zero reading, and any
        # reading at 58 C or above, is consistent with both rows. The 2026-09-02
        # run sat at 2000-2500 RPM in every mode with no temperature logged, so
        # it settled nothing; that is why the temperature is logged now.
        $after = Get-FanReading
        Write-ToolLog ("  fan 0: " + $after.rpm + " rpm, sensor 3: " + $after.temp + " C (" + (Get-SecondsSince $sinceSwitch) + " s after the switch; hold was at least " + $DwellSeconds + " s)")

        $sweep += New-Object PSObject -Property ([ordered]@{
            requested = $mode; readBack = $readBack
            stateType4 = $state4; stateType4AtAnswer = $state4AtAnswer; ledSeen = $ledSeen
            fanRpm = $before.rpm; sensor3C = $before.temp
            fanRpmAfterDwell = $after.rpm; sensor3CAfterDwell = $after.temp
            secondsAfterSwitch = (Get-SecondsSince $sinceSwitch)
        })
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

if ($sweep.Count -gt 0) {
    # The #44 answer in one place: the firmware's state index and the colour a
    # person saw, per mode, instead of 150 lines apart.
    Write-ToolLog ""
    Write-ToolLog "--- Operator observations beside the firmware state index (Lighting_Id 4, Current_State_Type) ---"
    foreach ($row in $sweep) {
        $seen = if ($null -eq $row.ledSeen) { '(not asked)' }
                elseif ($row.ledSeen.Length -eq 0) { '(not observed)' }
                else { $row.ledSeen }
        $state = if ($null -eq $row.stateType4) { '(no reading)' } else { [string]$row.stateType4 }
        if ($null -ne $row.stateType4AtAnswer -and "$($row.stateType4AtAnswer)" -ne "$($row.stateType4)") {
            $state = $state + " after the switch, " + $row.stateType4AtAnswer + " at the answer"
        }
        Write-ToolLog ("  mode " + $row.requested + " (read back " + $row.readBack + "): state " + $state + " -> " + $seen)
    }
}

Write-ToolLog ""
Write-ToolLog "Read the dumps above with one question in mind: does ANY field differ between modes?"
Write-ToolLog "On 2026-09-02 only Lighting_Id 4's Current_State_Type did. If nothing does, the colour is not readable here and the UI indicator must be derived and labelled so."
Write-ToolLog ("Log: " + $LogPath)

if ($Json) { New-Object PSObject -Property $result | ConvertTo-Json -Depth 5 }
else { New-Object PSObject -Property $result }
