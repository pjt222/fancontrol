<#
.SYNOPSIS
Decide whether Fan_Set_Table actually reaches the EC, by writing a curve while
the CPU is held above the lowest temperature threshold (issue #10).

.DESCRIPTION
At idle every curve produces the same 1600 RPM minimum, because the CPU sits
below the first threshold in the table (58 C on the 82RG). A write therefore
looks identical to a no-op. This tool removes that ambiguity: it holds the CPU
above the threshold with a controlled load, then walks through four observed
phases and compares fan RPM at matched temperature.

    baseline     original SmartFanMode, no write
    custom-mode  SmartFanMode set to Custom, still no write
    table-write  the curve written through fancontrol.exe set-curve
    restore      SmartFanMode returned to its original value

The custom-mode phase exists to make the result attributable. Without it, any
RPM change after the write could equally be an effect of the mode switch that
Fan_Set_Table requires. Only a change appearing in table-write and absent in
custom-mode is evidence about the write itself.

The write goes through fancontrol.exe rather than a WMI call in this script, so
what gets measured is the shipping code path.

.PARAMETER Writes
The curve writes to perform, in order, one observed phase each. Format per entry
is 'fanId:sensorId:s0,s1,...,s9'.

Entries may also be given as one semicolon-separated string, which is the form to
use when invoking this script through `powershell.exe -File`. That argument
parser is not the normal PowerShell one: it binds the second element of a
multi-value array to the next POSITIONAL parameter instead, so
`-Writes 'a' 'b'` silently lands 'b' in -ExePath. Passing `'a;b'` avoids the
whole class of problem, and both forms are accepted here.

The default is a single all-10s write to fan 0 / sensor 3: the loudest, least
ambiguous signal, and safe because it can only ask for more cooling than the
default.

Ordering carries the experimental design, so it is worth stating why. A curve
that asks for LESS cooling can only be interpreted after a curve that asks for
more has already landed. In Custom mode with no table the fans are stopped, so a
low curve that produces 0 RPM is indistinguishable from no curve at all -- the
same ambiguity that made the March 2026 probe unreadable, in a new place. Writing
a maximum curve first pins the fans at a known high RPM and proves the write
channel works; only a drop from THAT establishes what the low curve did.

Never lead with a curve that asks for less cooling than the default.

.PARAMETER CustomModeValue
Which SmartFanMode value means Custom. This repository disagrees with itself:
src/platform/lenovo.rs uses 255, while the March 2026 probe in scripts/ used 3.
If 3 is really Performance, that probe never met Fan_Set_Table's documented
prerequisite, which is a second explanation for its inconclusive result besides
idle temperatures. The tool reads the mode back after every transition and logs
what it actually got, so the run settles this rather than assuming it.

.PARAMETER SkipLoad
Use the machine's existing workload as the heat source instead of starting one.
Reproducibility is worse and the tool cannot stop the load if the abort fires,
so prefer the controlled load.

.NOTES
Writes to the EC. Requires elevation. Fans will be loud for several minutes.

Abort lever: any sample at or above AbortTempC calls Fan_Set_FullSpeed(1),
stops the load, restores the original mode and exits. Fan_Set_FullSpeed is
confirmed working on this firmware and overrides the curve, so the abort path
does not depend on the mechanism under test.
#>
[CmdletBinding()]
param(
    [string]$ExePath,
    [string]$LogPath,
    [string]$CsvPath,
    [int]$SensorId = 3,
    [string[]]$Writes = @('0:3:10,10,10,10,10,10,10,10,10,10'),
    [int[]]$ReportFanIds = @(0, 1),
    [int]$CustomModeValue = 255,
    [double]$TargetTempC = 62,
    [double]$AbortTempC = 90,
    [int]$PhaseSeconds = 90,
    [int]$WarmupTimeoutSeconds = 300,
    [int]$SampleIntervalSeconds = 2,
    [int]$LoadWorkers = 0,
    [switch]$SkipLoad,
    [switch]$Json
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'LenovoWmi.psm1') -Force

if (-not $LogPath) { $LogPath = Join-Path $PSScriptRoot 'Invoke-FanTableLoadTest.log' }
if (-not $CsvPath) { $CsvPath = Join-Path $PSScriptRoot 'Invoke-FanTableLoadTest.csv' }
if (-not $ExePath) {
    $ExePath = Join-Path (Split-Path $PSScriptRoot -Parent) 'target\x86_64-pc-windows-gnu\release\fancontrol.exe'
}

Start-ToolLog -Path $LogPath -Title 'Fan_Set_Table load test (issue #10)'
Write-ToolLog "WRITES TO THE EC. Fans will run loud. Abort lever: Fan_Set_FullSpeed(1)."
Write-ToolLog ""

# Sensor IDs sampled every tick. 3 is CPU and drives the test; 4 is the GPU,
# recorded because the GPU fan shares the chassis and its curve is separate.
# Accept both 'a','b' (native PowerShell) and 'a;b' (powershell.exe -File), and
# drop empties so a trailing separator is harmless.
$Writes = @($Writes | ForEach-Object { $_ -split ';' } | Where-Object { $_.Trim().Length -gt 0 } | ForEach-Object { $_.Trim() })
Write-ToolLog ("Write sequence (" + $Writes.Count + "):")
foreach ($w in $Writes) { Write-ToolLog ("  " + $w) }
Write-ToolLog ""

$SampleSensorIds = @(3, 4)
# Sample exactly what will be reported, so a -ReportFanIds entry can never name a
# column the sampler never collected.
$SampleFanIds = $ReportFanIds

$script:Samples = New-Object System.Collections.ArrayList
$script:WritePhases = @()
$script:LoadJobs = @()
$script:FanMethod = $null
$script:GameZone = $null
$script:OriginalMode = $null
$script:Aborted = $false
$script:AbortReason = ''

# ---------------------------------------------------------------------------
# Sampling
# ---------------------------------------------------------------------------

# Deliberately not routed through Invoke-LenovoWmiMethod: that helper dumps every
# output property to the log on each call, which at a 2 second cadence over a
# ten minute run would bury the phase transitions in thousands of lines. The
# milestone calls below do use it. This uses the adapted positional call, the
# form the existing probes in scripts/ already proved on this firmware.
function Get-FanSample {
    param([Parameter(Mandatory)][string]$Phase)

    $row = [ordered]@{
        timestamp = (Get-Date -Format 'yyyy-MM-ddTHH:mm:ss')
        phase     = $Phase
    }
    foreach ($sid in $SampleSensorIds) {
        $value = $null
        try {
            $result = $script:FanMethod.Fan_GetCurrentSensorTemperature($sid)
            $value = Get-WmiPropertyOrNull -InputObject $result -Name 'CurrentSensorTemperature'
        } catch {
            $value = $null
        }
        $row["temp_s$sid"] = $value
    }
    foreach ($fid in $SampleFanIds) {
        $value = $null
        try {
            $result = $script:FanMethod.Fan_GetCurrentFanSpeed($fid)
            $value = Get-WmiPropertyOrNull -InputObject $result -Name 'CurrentFanSpeed'
        } catch {
            $value = $null
        }
        $row["rpm_f$fid"] = $value
    }

    $sample = New-Object PSObject -Property $row
    [void]$script:Samples.Add($sample)
    return $sample
}

function Get-ControlTemp {
    param([Parameter(Mandatory)]$Sample)
    $name = "temp_s$SensorId"
    $value = $Sample.$name
    if ($null -eq $value) { return $null }
    return [double]$value
}

function Format-Sample {
    param([Parameter(Mandatory)]$Sample)
    $parts = @()
    foreach ($sid in $SampleSensorIds) {
        $v = $Sample."temp_s$sid"
        if ($null -eq $v) { $v = '?' }
        $parts += ("s{0}={1}C" -f $sid, $v)
    }
    foreach ($fid in $SampleFanIds) {
        $v = $Sample."rpm_f$fid"
        if ($null -eq $v) { $v = '?' }
        $parts += ("f{0}={1}rpm" -f $fid, $v)
    }
    return ($parts -join '  ')
}

# ---------------------------------------------------------------------------
# SmartFanMode
# ---------------------------------------------------------------------------

# Not routed through Invoke-LenovoWmiMethod either, for two specific reasons.
# SetSmartFanMode returns void, so the helper's mandatory -Property would log a
# misleading "succeeded but exposes no property" warning on every correct call.
# And the WMI parameter name is unknown here: the helper's named path would throw
# on a wrong key, get caught, and return null -- leaving the mode unset while the
# run continued. The positional call is the form the March 2026 probe already
# proved on this firmware, and correctness is established by reading the mode
# back rather than by trusting the call.
function Get-SmartFanMode {
    try {
        $result = $script:GameZone.GetSmartFanMode()
    } catch {
        Write-ToolLog ("  ERROR calling GetSmartFanMode: " + $_.Exception.Message)
        return $null
    }
    foreach ($name in @('Data', 'mode', 'Mode', 'SmartFanMode')) {
        $value = Get-WmiPropertyOrNull -InputObject $result -Name $name
        if ($null -ne $value) { return $value }
    }
    Write-ToolLog "  WARNING: GetSmartFanMode returned no recognised property."
    return $null
}

function Set-SmartFanMode {
    <#
    .OUTPUTS
    The mode read back afterwards, which is not necessarily the mode requested.
    #>
    param([Parameter(Mandatory)]$Mode)
    Write-ToolLog ("  SetSmartFanMode(" + $Mode + ")")
    try {
        [void]$script:GameZone.SetSmartFanMode($Mode)
    } catch {
        Write-ToolLog ("  ERROR calling SetSmartFanMode: " + $_.Exception.Message)
    }
    Start-Sleep -Milliseconds 500
    $readBack = Get-SmartFanMode
    Write-ToolLog ("  SmartFanMode read back: " + $readBack)
    return $readBack
}

# ---------------------------------------------------------------------------
# Abort lever
# ---------------------------------------------------------------------------

function Invoke-AbortLever {
    param([Parameter(Mandatory)][string]$Reason)

    $script:Aborted = $true
    $script:AbortReason = $Reason
    Write-ToolLog ""
    Write-ToolLog ("!!! ABORT: " + $Reason)
    Write-ToolLog "!!! Calling Fan_Set_FullSpeed(1) and stopping the load."
    try {
        [void]$script:FanMethod.Fan_Set_FullSpeed(1)
        Write-ToolLog "  Fan_Set_FullSpeed(1) returned without error."
    } catch {
        Write-ToolLog ("  ERROR: Fan_Set_FullSpeed(1) failed: " + $_.Exception.Message)
    }
    Stop-LoadWorkers
}

# ---------------------------------------------------------------------------
# Load generation
# ---------------------------------------------------------------------------

function Start-LoadWorkers {
    param([int]$Workers)
    if ($Workers -le 0) {
        $Workers = [int]$env:NUMBER_OF_PROCESSORS
        if ($Workers -le 0) { $Workers = 4 }
    }
    Write-ToolLog ("Starting " + $Workers + " CPU load workers.")
    $jobs = @()
    for ($i = 0; $i -lt $Workers; $i++) {
        # A bare spin. Nothing about the arithmetic matters; only that the core
        # stays busy and that the job dies the moment it is stopped.
        $jobs += Start-Job -ScriptBlock {
            $x = 0.0
            while ($true) { $x = [math]::Sqrt($x + 1.0) * 1.000001 }
        }
    }
    $script:LoadJobs = $jobs
    return $Workers
}

function Stop-LoadWorkers {
    if ($script:LoadJobs.Count -eq 0) { return }
    Write-ToolLog ("Stopping " + $script:LoadJobs.Count + " load workers.")
    foreach ($job in $script:LoadJobs) {
        try { Stop-Job -Job $job -ErrorAction SilentlyContinue } catch { }
        try { Remove-Job -Job $job -Force -ErrorAction SilentlyContinue } catch { }
    }
    $script:LoadJobs = @()
}

# ---------------------------------------------------------------------------
# Phase runner
# ---------------------------------------------------------------------------

function Invoke-Phase {
    <#
    .SYNOPSIS
    Sample for a fixed duration, logging each tick and honouring the abort.
    .OUTPUTS
    $true to continue, $false when the abort fired.
    #>
    param(
        [Parameter(Mandatory)][string]$Phase,
        [Parameter(Mandatory)][int]$Seconds
    )
    Write-ToolLog ""
    Write-ToolLog ("--- phase: " + $Phase + " (" + $Seconds + "s) ---")
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $sample = Get-FanSample -Phase $Phase
        Write-ToolLog ("  " + (Format-Sample -Sample $sample))
        $temp = Get-ControlTemp -Sample $sample
        if ($null -ne $temp -and $temp -ge $AbortTempC) {
            Invoke-AbortLever -Reason ("sensor " + $SensorId + " reached " + $temp + "C, limit is " + $AbortTempC + "C")
            return $false
        }
        Start-Sleep -Seconds $SampleIntervalSeconds
    }
    return $true
}

function Get-PhaseMedian {
    param(
        [Parameter(Mandatory)][string]$Phase,
        [Parameter(Mandatory)][string]$Column
    )
    $values = @($script:Samples |
        Where-Object { $_.phase -eq $Phase } |
        ForEach-Object { $_.$Column } |
        Where-Object { $null -ne $_ } |
        ForEach-Object { [double]$_ } |
        Sort-Object)
    if ($values.Count -eq 0) { return $null }
    return $values[[int]([math]::Floor($values.Count / 2))]
}

# ---------------------------------------------------------------------------
# Run
# ---------------------------------------------------------------------------

$result = [ordered]@{
    aborted     = $false
    abortReason = ''
    verdict     = 'not reached'
}

try {
    Write-ToolLog "--- preflight ---"

    if (-not (Test-Elevated)) {
        Write-ToolLog "FATAL: not elevated. root\WMI returns access denied without it."
        exit 1
    }
    Write-ToolLog "  elevated: yes"

    if (-not (Test-Path $ExePath)) {
        Write-ToolLog ("FATAL: fancontrol.exe not found at " + $ExePath)
        exit 1
    }
    Write-ToolLog ("  exe: " + $ExePath)

    $script:FanMethod = Get-LenovoWmiClass -ClassName 'LENOVO_FAN_METHOD' -Single
    $script:GameZone = Get-LenovoWmiClass -ClassName 'LENOVO_GAMEZONE_DATA' -Single
    if ($null -eq $script:FanMethod -or $null -eq $script:GameZone) {
        Write-ToolLog "FATAL: required WMI classes unavailable."
        exit 1
    }

    # Full speed mode overrides every curve, so a run started inside it would
    # measure nothing. Check before spending ten minutes on it.
    $fullSpeed = Invoke-LenovoWmiMethod -WmiObject $script:FanMethod -Method 'Fan_Get_FullSpeed' -Property 'Status'
    Write-ToolLog ("  Fan_Get_FullSpeed: " + $fullSpeed)
    if ($fullSpeed -eq $true) {
        Write-ToolLog "FATAL: full speed mode is active. It overrides the curve, so this test cannot measure anything. Clear it first."
        exit 1
    }

    $script:OriginalMode = Get-SmartFanMode
    Write-ToolLog ("  SmartFanMode at start: " + $script:OriginalMode)
    if ($null -eq $script:OriginalMode) {
        Write-ToolLog "FATAL: could not read SmartFanMode, so it could not be restored afterwards."
        exit 1
    }
    $result['originalMode'] = $script:OriginalMode

    # Smoke-test the read path of the binary before trusting its write path.
    Write-ToolLog "  fancontrol.exe list:"
    $listOutput = & $ExePath list 2>&1 | Out-String
    foreach ($line in ($listOutput -split "`r?`n")) {
        if ($line.Trim().Length -gt 0) { Write-ToolLog ("    " + $line.TrimEnd()) }
    }

    $sample = Get-FanSample -Phase 'preflight'
    Write-ToolLog ("  " + (Format-Sample -Sample $sample))

    # --- warmup ---------------------------------------------------------
    if (-not $SkipLoad) {
        $workerCount = Start-LoadWorkers -Workers $LoadWorkers
        $result['loadWorkers'] = $workerCount
    } else {
        Write-ToolLog "SkipLoad set -- relying on the machine's existing workload for heat."
        $result['loadWorkers'] = 0
    }

    Write-ToolLog ""
    Write-ToolLog ("--- phase: warmup (until sensor " + $SensorId + " reaches " + $TargetTempC + "C, timeout " + $WarmupTimeoutSeconds + "s) ---")
    $warmupDeadline = (Get-Date).AddSeconds($WarmupTimeoutSeconds)
    $reachedTarget = $false
    while ((Get-Date) -lt $warmupDeadline) {
        $sample = Get-FanSample -Phase 'warmup'
        Write-ToolLog ("  " + (Format-Sample -Sample $sample))
        $temp = Get-ControlTemp -Sample $sample
        if ($null -ne $temp -and $temp -ge $AbortTempC) {
            Invoke-AbortLever -Reason ("sensor " + $SensorId + " reached " + $temp + "C during warmup")
            break
        }
        if ($null -ne $temp -and $temp -ge $TargetTempC) {
            $reachedTarget = $true
            Write-ToolLog ("  target reached: " + $temp + "C")
            break
        }
        Start-Sleep -Seconds $SampleIntervalSeconds
    }
    $result['reachedTarget'] = $reachedTarget

    if (-not $script:Aborted -and -not $reachedTarget) {
        # Everything downstream compares RPM at matched temperature above the
        # first threshold. Below it, both curves prescribe the same band and the
        # comparison is meaningless -- which is exactly the trap that left the
        # March 2026 probe inconclusive. Stop rather than produce that result again.
        Write-ToolLog ("FATAL: sensor " + $SensorId + " never reached " + $TargetTempC + "C within " + $WarmupTimeoutSeconds + "s.")
        Write-ToolLog "Below the first threshold every curve behaves identically, so the run would be inconclusive by construction."
        $result['verdict'] = 'inconclusive -- never reached target temperature'
    } elseif (-not $script:Aborted) {

        # --- baseline ---------------------------------------------------
        if (Invoke-Phase -Phase 'baseline' -Seconds $PhaseSeconds) {

            # --- custom-mode --------------------------------------------
            Write-ToolLog ""
            Write-ToolLog ("Setting SmartFanMode to Custom (" + $CustomModeValue + ").")
            $modeAfterSet = Set-SmartFanMode -Mode $CustomModeValue
            $result['modeAfterSet'] = $modeAfterSet
            if ("$modeAfterSet" -ne "$CustomModeValue") {
                # Not fatal. The write is still worth attempting, and a mode that
                # refuses to take is itself the finding the repo's 255-versus-3
                # disagreement needs.
                Write-ToolLog ("  WARNING: asked for " + $CustomModeValue + " but read back " + $modeAfterSet + ". Custom mode did not take.")
            }

            if (Invoke-Phase -Phase 'custom-mode' -Seconds $PhaseSeconds) {

                # --- the write sequence ---------------------------------
                # Each entry gets its own observed phase, so every write is
                # attributable to the phase that follows it rather than to the
                # accumulated state of all previous writes.
                $writeIndex = 0
                $exitCodes = @()
                foreach ($spec in $Writes) {
                    $writeIndex++
                    $parts = $spec -split ':'
                    if ($parts.Count -ne 3) {
                        Write-ToolLog ("FATAL: malformed -Writes entry '" + $spec + "', expected 'fanId:sensorId:s0,...,s9'")
                        break
                    }
                    $wFan = [int]$parts[0]
                    $wSensor = [int]$parts[1]
                    $wSteps = $parts[2]
                    $phaseName = "write$writeIndex-f$wFan"

                    Write-ToolLog ""
                    Write-ToolLog ("Write " + $writeIndex + " of " + $Writes.Count + " through fancontrol.exe: fan " + $wFan + ", sensor " + $wSensor + ", steps " + $wSteps)
                    $setOutput = & $ExePath set-curve --fan-id $wFan --sensor-id $wSensor --steps $wSteps 2>&1 | Out-String
                    foreach ($line in ($setOutput -split "`r?`n")) {
                        if ($line.Trim().Length -gt 0) { Write-ToolLog ("    " + $line.TrimEnd()) }
                    }
                    $exitCodes += $LASTEXITCODE
                    Write-ToolLog ("  exit code: " + $LASTEXITCODE)
                    if ($LASTEXITCODE -ne 0) {
                        # A rejected curve leaves the previous one in force, which
                        # is safe, but continuing would attribute the previous
                        # curve's behaviour to this phase.
                        Write-ToolLog "  WARNING: set-curve failed; the previous curve is still in force. Phase readings below are NOT this curve."
                    }

                    # One write, then observe. No re-apply loop: the GUI re-applies
                    # every 1.5s precisely because the EC is expected to fight back,
                    # and a single write is what measures whether it does and how fast.
                    if (-not (Invoke-Phase -Phase $phaseName -Seconds $PhaseSeconds)) { break }
                }
                $result['setCurveExitCodes'] = $exitCodes
                $script:WritePhases = @(1..$Writes.Count | ForEach-Object {
                    $p = ($Writes[$_ - 1] -split ':')
                    "write$_-f$($p[0])"
                })
            }

            # --- restore ------------------------------------------------
            if (-not $script:Aborted) {
                Write-ToolLog ""
                Write-ToolLog ("Restoring SmartFanMode to " + $script:OriginalMode + ".")
                $modeAfterRestore = Set-SmartFanMode -Mode $script:OriginalMode
                $result['modeAfterRestore'] = $modeAfterRestore
                [void](Invoke-Phase -Phase 'restore' -Seconds $PhaseSeconds)
            }
        }
    }
} finally {
    # Restoration must survive Ctrl-C and any error above, or the machine is left
    # in Custom mode with an all-10s curve.
    Stop-LoadWorkers

    if ($null -ne $script:GameZone -and $null -ne $script:OriginalMode) {
        try {
            $modeNow = Get-SmartFanMode
            if ("$modeNow" -ne "$($script:OriginalMode)") {
                Write-ToolLog ("Cleanup: SmartFanMode is " + $modeNow + ", restoring " + $script:OriginalMode + ".")
                [void](Set-SmartFanMode -Mode $script:OriginalMode)
            }
        } catch {
            Write-ToolLog ("Cleanup: could not restore SmartFanMode: " + $_.Exception.Message)
        }
    }

    if ($script:Samples.Count -gt 0) {
        $script:Samples | Export-Csv -Path $CsvPath -NoTypeInformation -Encoding ASCII
        Write-ToolLog ("Samples written to " + $CsvPath + " (" + $script:Samples.Count + " rows).")
    }
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

Write-ToolLog ""
Write-ToolLog "--- summary (median per phase) ---"
$tempColumn = "temp_s$SensorId"
if ($null -eq $script:WritePhases) { $script:WritePhases = @() }
$allPhases = @('baseline', 'custom-mode') + $script:WritePhases + @('restore')

$header = "  {0,-14} {1,-8}" -f 'phase', "s$SensorId"
foreach ($fid in $ReportFanIds) { $header += ("{0,-10}" -f "fan$fid") }
Write-ToolLog $header

$medians = [ordered]@{}
foreach ($phase in $allPhases) {
    $medTemp = Get-PhaseMedian -Phase $phase -Column $tempColumn
    $entry = [ordered]@{ temp = $medTemp }
    $line = "  {0,-14} {1,-8}" -f $phase, $(if ($null -eq $medTemp) { 'n/a' } else { "${medTemp}C" })
    foreach ($fid in $ReportFanIds) {
        $medRpm = Get-PhaseMedian -Phase $phase -Column "rpm_f$fid"
        $entry["rpm_f$fid"] = $medRpm
        $line += ("{0,-10}" -f $(if ($null -eq $medRpm) { 'n/a' } else { "$medRpm" }))
    }
    $medians[$phase] = $entry
    Write-ToolLog $line
}
$result['medians'] = $medians

# Per-fan divergence across the write phases is what answers "are the fans
# independently controllable". Reported as data rather than as a verdict,
# because a single run cannot separate "the write covers both fans" from
# "Custom mode governs both fans together" unless the two actually diverge.
if ($ReportFanIds.Count -gt 1 -and $script:WritePhases.Count -gt 0) {
    Write-ToolLog ""
    Write-ToolLog "--- per-fan divergence across write phases ---"
    $maxDivergence = 0
    foreach ($phase in $script:WritePhases) {
        $values = @()
        foreach ($fid in $ReportFanIds) {
            $v = $medians[$phase]["rpm_f$fid"]
            if ($null -ne $v) { $values += [double]$v }
        }
        if ($values.Count -gt 1) {
            $spread = ($values | Measure-Object -Maximum).Maximum - ($values | Measure-Object -Minimum).Minimum
            if ($spread -gt $maxDivergence) { $maxDivergence = $spread }
            Write-ToolLog ("  {0,-14} spread = {1} rpm" -f $phase, $spread)
        }
    }
    $result['maxFanDivergenceRpm'] = $maxDivergence
    if ($maxDivergence -ge 500) {
        Write-ToolLog ("  -> fans DIVERGED by up to " + $maxDivergence + " rpm: independently controllable.")
        $result['fansIndependent'] = $true
    } else {
        Write-ToolLog ("  -> fans stayed within " + $maxDivergence + " rpm of each other: no evidence of independent control.")
        $result['fansIndependent'] = $false
    }
}

$baselineRpm = $medians['baseline']['rpm_f0']
$modeRpm = $medians['custom-mode']['rpm_f0']
$firstWritePhase = if ($script:WritePhases.Count -gt 0) { $script:WritePhases[0] } else { $null }
$writeRpm = if ($null -eq $firstWritePhase) { $null } else { $medians[$firstWritePhase]['rpm_f0'] }

Write-ToolLog ""
if ($script:Aborted) {
    $result['aborted'] = $true
    $result['abortReason'] = $script:AbortReason
    $result['verdict'] = 'aborted'
    Write-ToolLog ("VERDICT: aborted -- " + $script:AbortReason)
} elseif ($null -eq $writeRpm -or $null -eq $modeRpm) {
    Write-ToolLog ("VERDICT: " + $result['verdict'])
} elseif ($null -ne $baselineRpm -and $modeRpm -gt ($baselineRpm + 500)) {
    # The custom-mode phase is only a no-curve control while the EC actually holds
    # no curve. Measured 2026-08-19: a curve written in an EARLIER run survived a
    # switch to another power mode and back, 24 minutes later, and reactivated on
    # re-entering Custom. When that happens this phase starts at the retained
    # curve's RPM, the comparison below measures nothing, and reporting a verdict
    # from it would be worse than reporting none -- so refuse rather than mislead.
    $result['verdict'] = 'control invalid -- EC held a retained curve on entering Custom mode'
    Write-ToolLog ("  custom-mode began at " + $modeRpm + " rpm against a baseline of " + $baselineRpm + " rpm.")
    Write-ToolLog "VERDICT: CONTROL INVALID -- the EC still held a curve from an earlier write, so the"
    Write-ToolLog "  custom-mode phase was not a no-curve control. Read the per-phase table above instead."
    Write-ToolLog "  To restore the control, reboot, or write a known curve and compare against THAT."
} else {
    $deltaWrite = $writeRpm - $modeRpm
    $deltaMode = if ($null -eq $baselineRpm) { 0 } else { $modeRpm - $baselineRpm }
    Write-ToolLog ("  mode switch alone changed fan 0 by " + $deltaMode + " rpm")
    Write-ToolLog ("  the first table write changed it by a further " + $deltaWrite + " rpm")
    $result['deltaModeRpm'] = $deltaMode
    $result['deltaWriteRpm'] = $deltaWrite

    # 150 rpm is a deliberately loose threshold against sampling jitter, not a
    # calibrated figure. The all-10s curve should move the fan by thousands if it
    # lands at all; anything near this bound deserves the raw CSV, not a verdict.
    if ($deltaWrite -ge 150) {
        $result['verdict'] = 'Fan_Set_Table has effect'
        Write-ToolLog "VERDICT: Fan_Set_Table HAS EFFECT -- fan sped up after the write, beyond the mode switch."
    } elseif ($deltaWrite -le -150) {
        # An all-10s curve asking for less cooling means 10 is not an index into
        # the 0-10 speed array. HandheldCompanion treats the same field as a
        # 0-100 percentage, under which 10 is nearly the slowest setting.
        $result['verdict'] = 'inverted -- step scale is probably 0-100, not 0-10'
        Write-ToolLog "VERDICT: INVERTED -- the fan slowed after an all-10s write. The step scale is probably a 0-100 percentage, not a 0-10 index."
    } else {
        $result['verdict'] = 'no measurable effect'
        Write-ToolLog "VERDICT: NO MEASURABLE EFFECT -- the write did not move the fan at held temperature."
    }
}

Write-ToolLog ""
Write-ToolLog ("Log: " + $LogPath)
Write-ToolLog ("CSV: " + $CsvPath)

if ($Json) {
    New-Object PSObject -Property $result | ConvertTo-Json -Depth 5
} else {
    New-Object PSObject -Property $result
}
