<#
.SYNOPSIS
Sample Lighting_Id 4's state index and SmartFanMode side by side while the
operator changes things other than SetSmartFanMode, to learn whether the two
can ever disagree (issue #44, the indicator's source decision).

.DESCRIPTION
The 2026-09-02 runs of Get-LenovoLighting.ps1 swept SmartFanMode through WMI
and read Lighting_Id 4 -> Current_State_Type in each mode: 0/1/2/3 for
1/2/3/255. Both that index and the operator's colour were functions of the mode
the tool had set, so those runs cannot tell whether id 4 carries anything that
GetSmartFanMode does not. This tool never calls SetSmartFanMode. It samples the
mode and every lighting id continuously while the operator applies
manipulations that reach the LED or the mode by other paths:

  baseline   no action. The control phase: how the fields behave at rest.
  unplug     AC adapter out, in Performance. Whether this firmware changes
             anything on battery is unmeasured. If it forces the button white
             while GetSmartFanMode still reads 3, id 4 either follows the LED
             (index 1) or the register (index 2), decisive either way; a mode
             that reads 2 on battery is a third outcome (the register moved
             without SetSmartFanMode); nothing changing is a null result.
  replug     AC adapter back in.
  sleepwake  sleep and wake. The action happens before Enter: the operator
             sleeps the machine, wakes it, signs in, then presses Enter, and
             the window samples the first seconds after resume. Firmware that
             restores an LED and a mode register on resume is a likely place
             for the two to skew. Skipped in Custom: curve retention across
             sleep is unmeasured (CLAUDE.md), and waking into Custom with a
             lost table is the fans-off hazard.
  fnq        Fn+Q once: the EC hotkey path. Sampled once on 2026-09-03: the
             register and id 4 moved within the same sample, about a second.
  fnspace    Fn+Space once: the usual Lenovo keyboard-backlight binding, which
             on this machine changes the keyboard colour (operator's report,
             2026-09-03). Tests whether ids 0/1/2/3/5 report any lighting
             live; on 2026-09-03 none moved while the keyboard colour did.
             Id 0's descriptor row (quoted in CLAUDE.md) looks like a
             multi-level zone, but nothing has measured what it is.
  fullspeed  only with -IncludeFullSpeed: Fan_Set_FullSpeed(1) for one window.
             The operator's colour there answers whether the LED changes under
             full speed, an indicator fact for #44 on its own. The window's
             samples count toward the verdict like any other: full speed
             leaves the mode register alone, so an id 4 that moved with the
             LED here would be the disagreement the run is looking for.
  vantage    whatever Lenovo Vantage offers for the power-button light, typed
             in verbatim by the operator. "none" is a result too.

Each phase: the operator presses Enter, the tool samples for the phase's window
while the operator performs the action, then the operator types the colour the
button shows. Every sample reads the mode, then each lighting id, then the mode
again, then Win32_Battery.BatteryStatus and Fan_Get_FullSpeed, and is stamped
with stopwatch seconds since the window opened plus the sample's own duration.
Win32_Battery is the adapter column. LENOVO_OTHER_METHOD.Get_AC_PD_Status was
read once per phase in the 2026-09-03 13:53 run and returned AC_PD_Status = 0
throughout while the barrel adapter was in, which is not what an adapter
indicator would show; it is not read any more.
Sampling runs as fast as the calls return, targeting one sample per second;
the label carries the measured time, not the target.

Disagreement, per sample: when the two mode reads agree and the mode is one of
1/2/3/255, the expected id 4 index is the measured table above. A sample whose
id 4 index differs is re-read about every 0.4 s until it agrees again or
-MismatchTimeoutSeconds pass. Agreement at the same mode makes it transient
(a switch landed mid-sample, or the index repainted late); the mode moving
during the re-reads makes it inconclusive; outlasting the timeout with the
mode holding still makes it sustained. A sustained episode that agrees again
later in the window at the same mode is reported as lag, and only a
sustained episode that never agrees again counts as a disagreement in the
verdict. A mode outside the table gives no expectation, not a disagreement.

Why a new tool rather than a switch on Get-LenovoLighting.ps1: that tool is a
write-driven sweep (it writes a curve, then sets the mode per step). This one
is read-only, operator-driven and continuous, a different safety class, and it
must not call SetSmartFanMode at all.

.PARAMETER Phases
Phase keys to run, in order. Default: baseline, unplug, replug, sleepwake,
fnq, fnspace, vantage. With -IncludeFullSpeed, fullspeed is inserted before
vantage.

.PARAMETER PhaseSeconds
Sampling window per phase, except unplug and replug.

.PARAMETER UnplugSeconds
Sampling window for unplug and replug. Longer, because a firmware downgrade on
battery may be delayed.

.PARAMETER LightingIds
Ids passed to Get_Lighting_Current_Status each sample. Must include 4 for the
verdict to mean anything.

.PARAMETER MismatchTimeoutSeconds
How long a disagreeing pair is re-read, about 0.4 s apart, before the episode
counts as sustained. The only latency figure on record is that id 4 read the
new index about a second after a WMI SetSmartFanMode (the 800 ms sleep before
the read in Get-LenovoLighting.ps1); the hotkey path is unmeasured. A repaint
slower than this timeout is logged as sustained and then as resolved when a
later sample agrees at the same mode, which the summary reports as lag.

.PARAMETER IncludeFullSpeed
Add the fullspeed phase. This is the only write the tool can make. It goes
through Invoke-LenovoWmiMethod with the parameter name and CIM type read from
GetMethodParameters rather than guessed, reads Fan_Get_FullSpeed back, and
disables full speed again after the window and in the finally block. The phase
is skipped when the mode reads 255 (Custom): disabling full speed there drops
back to whatever table the EC holds. Without this switch the tool declares
itself read-only to the module, which then throws on any Set* call.

.NOTES
Read-only unless -IncludeFullSpeed. Never calls SetSmartFanMode. Never calls
Set_Lighting_Current_Status; tests/lighting_setter_forbidden.rs fails cargo
test if any tracked script or source invokes it. The lighting getter is
called directly on the WMI object, as Get-LenovoLighting.ps1 does, so the
module's read-only guard does not see it; the read-only claim for it rests on
this file containing no such call.

Requires elevation. Attended: the prompts block, and a host that cannot prompt
stops the run. Ctrl+C at a prompt has not been measured to reach the finally
block in this tool; without -IncludeFullSpeed there is nothing for the finally
to undo. Fn+Q cannot enter Custom (255): if the run starts in Custom, the fnq
phase leaves it and this tool cannot put it back (a set-curve can). Nothing
is owed after a run: no table and no mode is written.
#>
[CmdletBinding()]
param(
    [string]$LogPath,
    [string[]]$Phases = @('baseline', 'unplug', 'replug', 'sleepwake', 'fnq', 'fnspace', 'vantage'),
    [int]$PhaseSeconds = 20,
    [int]$UnplugSeconds = 30,
    [int]$MismatchTimeoutSeconds = 5,
    [int[]]$LightingIds = @(0, 1, 2, 3, 4, 5),
    [switch]$IncludeFullSpeed,
    [switch]$Json
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'LenovoWmi.psm1') -Force

if (-not $LogPath) { $LogPath = Join-Path $PSScriptRoot 'Watch-LenovoLightingVsMode.log' }

# The read-only declaration is enforced by the module: with it armed,
# Invoke-LenovoWmiMethod throws on any Set* method. The one write this tool can
# make, Fan_Set_FullSpeed, goes through the module, so the guard is a backstop
# against that call being reached without the switch.
Start-ToolLog -Path $LogPath -Title 'Lenovo lighting state index versus SmartFanMode under operator manipulations' -ReadOnly:(-not $IncludeFullSpeed)
if ($IncludeFullSpeed) {
    Write-ToolLog ("Writes to the EC in one phase: Fan_Set_FullSpeed(1) for one window, disabled again afterwards.")
} else {
    Write-ToolLog "Never calls SetSmartFanMode or any lighting setter."
}
Write-ToolLog ""

$result = [ordered]@{ ok = $false }
$gz = $null; $lm = $null; $fm = $null
$script:FullSpeedCalled = $false
$phaseRecords = New-Object System.Collections.ArrayList

# ---------------------------------------------------------------------------
# Pure helpers (no WMI). Testable in isolation.
# ---------------------------------------------------------------------------

function Get-ExpectedIndex {
    # SmartFanMode -> Lighting_Id 4 Current_State_Type, measured 2026-09-02
    # (Get-LenovoLighting.ps1, 12:49 and 15:27 runs). Anything else has no
    # measured index and therefore no expectation.
    param([AllowNull()]$Mode)
    switch ("$Mode") {
        '1'     { return 0 }
        '2'     { return 1 }
        '3'     { return 2 }
        '255'   { return 3 }
        default { return $null }
    }
}

function Get-BatteryLabel {
    # Win32_Battery.BatteryStatus per Microsoft's documentation: 1 discharging,
    # 2 on AC, 3 fully charged, 4 low, 5 critical, 6-9 charging, 10 undefined,
    # 11 partially charged. Folded to the one distinction the unplug phase
    # needs. On this machine only 2, while on the barrel adapter, has been
    # read (2026-09-03); the raw value is logged beside the label for that
    # reason.
    param([AllowNull()]$Raw)
    switch ("$Raw") {
        { $_ -in @('1', '4', '5') }                        { return 'battery' }
        { $_ -in @('2', '3', '6', '7', '8', '9', '11') }   { return 'AC' }
        default                                            { return 'unknown' }
    }
}

function Get-SampleVerdict {
    # What one sample says about id 4 versus the mode, before any re-read.
    param([AllowNull()]$ModeBefore, [AllowNull()]$ModeAfter, [AllowNull()]$State4)
    if ($null -eq $ModeBefore -or $null -eq $ModeAfter) { return 'no-mode' }
    if ("$ModeBefore" -ne "$ModeAfter") { return 'switching' }
    $expected = Get-ExpectedIndex $ModeBefore
    if ($null -eq $expected) { return 'no-expectation' }
    if ($null -eq $State4) { return 'no-reading' }
    if ("$State4" -eq "$expected") { return 'agree' }
    return 'mismatch'
}

function Get-ChangedFields {
    # Names of the fields that differ between two samples. Empty when there is
    # no previous sample. Callers wrap the result in @().
    param([AllowNull()]$Prev, $Cur, [int[]]$Ids)
    $changed = @()
    if ($null -eq $Prev) { return $changed }
    if ("$($Prev.modeAfter)" -ne "$($Cur.modeAfter)") { $changed += 'mode' }
    foreach ($id in $Ids) {
        $p = $Prev.states["$id"]; $c = $Cur.states["$id"]
        if ("$($p.s)" -ne "$($c.s)") { $changed += ("id" + $id + ".state") }
        if ("$($p.b)" -ne "$($c.b)") { $changed += ("id" + $id + ".brightness") }
    }
    if ("$($Prev.batt)" -ne "$($Cur.batt)") { $changed += 'battery' }
    if ("$($Prev.full)" -ne "$($Cur.full)") { $changed += 'fullspeed' }
    return $changed
}

# ---------------------------------------------------------------------------
# WMI readers
# ---------------------------------------------------------------------------

function Get-Mode {
    try { $r = $gz.GetSmartFanMode() } catch { return $null }
    foreach ($n in @('Data', 'mode', 'Mode', 'SmartFanMode')) {
        $v = Get-WmiPropertyOrNull -InputObject $r -Name $n
        if ($null -ne $v) { return $v }
    }
    return $null
}

function Get-State4 {
    try { return (Get-WmiPropertyOrNull -InputObject ($lm.Get_Lighting_Current_Status(4)) -Name 'Current_State_Type') } catch { return $null }
}

function Get-BatteryRaw {
    try { $rows = @(Get-WmiObject -Namespace root\cimv2 -Class Win32_Battery -ErrorAction Stop) } catch { return $null }
    if ($rows.Count -eq 0) { return $null }
    return (Get-WmiPropertyOrNull -InputObject $rows[0] -Name 'BatteryStatus')
}

function Get-FullSpeed {
    if ($null -eq $fm) { return $null }
    try { return (Get-WmiPropertyOrNull -InputObject ($fm.Fan_Get_FullSpeed()) -Name 'Status') } catch { return $null }
}

function Read-Sample {
    # Fixed order, so a mismatch can be read against it: mode, then each id in
    # -LightingIds order, then mode again, then battery, then full speed.
    $clock = [System.Diagnostics.Stopwatch]::StartNew()
    $modeBefore = Get-Mode
    $states = [ordered]@{}
    foreach ($id in $LightingIds) {
        $bri = $null; $st = $null; $err = $null
        try {
            $r = $lm.Get_Lighting_Current_Status($id)
            $bri = Get-WmiPropertyOrNull -InputObject $r -Name 'Current_Brightness_Level'
            $st = Get-WmiPropertyOrNull -InputObject $r -Name 'Current_State_Type'
        } catch {
            $err = $_.Exception.Message
        }
        # String keys on purpose: an ordered dictionary indexed with an int
        # returns the entry at that POSITION, not the entry with that key.
        $states["$id"] = @{ b = $bri; s = $st; err = $err }
    }
    $modeAfter = Get-Mode
    $battRaw = Get-BatteryRaw
    $full = Get-FullSpeed
    return @{
        modeBefore = $modeBefore; modeAfter = $modeAfter; states = $states
        battRaw = $battRaw; batt = (Get-BatteryLabel $battRaw); full = $full
        ms = $clock.ElapsedMilliseconds
    }
}

function Format-Sample {
    param($S)
    $stList = @(); $brList = @()
    foreach ($id in $LightingIds) {
        $e = $S.states["$id"]
        $stList += $(if ($null -eq $e.s) { '-' } else { "$($e.s)" })
        $brList += $(if ($null -eq $e.b) { '-' } else { "$($e.b)" })
    }
    $s4 = '-'
    if ($S.states.Contains('4') -and $null -ne $S.states['4'].s) { $s4 = "$($S.states['4'].s)" }
    return ("mode " + $S.modeBefore + "/" + $S.modeAfter + "  id4=" + $s4 +
            "  state[" + ($LightingIds -join ',') + "]=" + ($stList -join ',') +
            "  bri=" + ($brList -join ',') +
            "  batt=" + $S.battRaw + "(" + $S.batt + ")  full=" + $S.full +
            "  (" + $S.ms + " ms)")
}

function Read-Operator {
    param([string]$Prompt)
    $answer = $null
    try {
        $answer = Read-Host -Prompt $Prompt
    } catch {
        Write-ToolLog ("FATAL: operator prompt unavailable: " + $_.Exception.Message)
        Write-ToolLog "This tool is attended; run it in an interactive console."
        throw "operator prompt unavailable"
    }
    if ($null -eq $answer) { $answer = '' }
    return ([string]$answer).Trim()
}

function Resolve-Mismatch {
    # A sample disagreed. Keep re-reading the pair, about 0.4 s apart, until
    # it agrees again or the timeout passes. A switch that lands between the
    # mode read and the id 4 read, and an index that repaints some time after
    # the register, both look like a disagreement for a while; only one that
    # outlasts the timeout with the mode holding still is sustained. A pair
    # counts as agreeing only at the mode the mismatch was seen at: if the
    # mode moved during the re-reads, agreement at the new mode says nothing
    # about the old one, and the episode is inconclusive.
    param($FirstMode, $FirstState4, $Expected, [int]$TimeoutSeconds)
    $clock = [System.Diagnostics.Stopwatch]::StartNew()
    $detail = @()
    $kind = 'sustained'
    $agreedAfter = $null
    while ($clock.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
        Start-Sleep -Milliseconds 400
        $m = Get-Mode
        $s = Get-State4
        $t = [Math]::Round($clock.Elapsed.TotalSeconds, 1)
        $detail += ("+" + $t + " s mode " + $m + " id4 " + $s)
        if ("$m" -ne "$FirstMode") { $kind = 'inconclusive'; break }
        if ($null -ne $s -and "$s" -eq "$Expected") { $kind = 'transient'; $agreedAfter = $t; break }
    }
    return @{
        kind = $kind; agreedAfter = $agreedAfter; heldFor = [Math]::Round($clock.Elapsed.TotalSeconds, 1)
        detail = ($detail -join '; '); first = ("mode " + $FirstMode + " id4 " + $FirstState4 + " expected " + $Expected)
    }
}

function Invoke-SampleWindow {
    # Sample for $Seconds, logging every sample. Returns the per-window record.
    param([int]$Seconds)
    $window = [System.Diagnostics.Stopwatch]::StartNew()
    $counts = [ordered]@{ samples = 0; agree = 0; transient = 0; sustained = 0; 'sustained-continued' = 0; inconclusive = 0; switching = 0; 'no-expectation' = 0; 'no-reading' = 0; 'no-mode' = 0 }
    $changedFields = @{}
    $battLabels = @{}
    $first = $null; $last = $null; $prev = $null
    $mismatches = New-Object System.Collections.ArrayList
    # The sustained episode still open, if any. A later sample that agrees at
    # the same mode closes it as lag rather than as a standing divergence.
    $open = $null
    while ($window.Elapsed.TotalSeconds -lt $Seconds) {
        $t0 = [Math]::Round($window.Elapsed.TotalSeconds, 1)
        $sample = Read-Sample
        if ($null -eq $first) { $first = $sample }
        $last = $sample
        $counts['samples']++
        $battLabels["$($sample.batt)"] = $true

        $state4 = $null
        if ($sample.states.Contains('4')) { $state4 = $sample.states['4'].s }
        $verdict = Get-SampleVerdict -ModeBefore $sample.modeBefore -ModeAfter $sample.modeAfter -State4 $state4
        # An empty array returned from a function arrives as $null; the filter
        # keeps @() empty rather than one $null element.
        $changed = @(Get-ChangedFields -Prev $prev -Cur $sample -Ids $LightingIds | Where-Object { $null -ne $_ })
        foreach ($f in $changed) { $changedFields[$f] = $true }
        # The mode moved since the previous sample. A mismatch right after
        # that is the shape a repaint lag takes, and is tagged so.
        $postSwitch = ($null -ne $prev -and "$($prev.modeAfter)" -ne "$($sample.modeBefore)")

        $tag = ''
        if ($changed.Count -gt 0) { $tag = '  CHANGE: ' + ($changed -join ', ') }
        Write-ToolLog ("  t=" + $t0 + " s  " + (Format-Sample $sample) + "  [" + $verdict + "]" + $tag)

        foreach ($id in $LightingIds) {
            $e = $sample.states["$id"]
            if ($null -ne $e.err) { Write-ToolLog ("      Lighting_Id " + $id + " -> ERROR: " + $e.err) }
        }

        if ($verdict -eq 'mismatch') {
            $expected = Get-ExpectedIndex $sample.modeBefore
            if ($null -ne $open -and "$($open.mode)" -eq "$($sample.modeBefore)" -and "$($open.state4)" -eq "$state4") {
                # The same divergence as the open episode; no second timeout loop.
                $counts['sustained-continued']++
                Write-ToolLog ("      still disagreeing (episode opened at t=" + $open.t + " s)")
            } else {
                $res = Resolve-Mismatch -FirstMode $sample.modeBefore -FirstState4 $state4 -Expected $expected -TimeoutSeconds $MismatchTimeoutSeconds
                $counts[$res.kind]++
                $episode = @{
                    t = $t0; kind = $res.kind; postSwitch = $postSwitch; mode = $sample.modeBefore; state4 = $state4
                    first = $res.first; rereads = $res.detail; agreedAfter = $res.agreedAfter; heldFor = $res.heldFor; resolvedAt = $null
                }
                [void]$mismatches.Add($episode)
                $switchNote = $(if ($postSwitch) { ' (the mode had moved since the previous sample)' } else { '' })
                switch ($res.kind) {
                    'transient'    { Write-ToolLog ("      transient mismatch: agreed again after " + $res.agreedAfter + " s" + $switchNote + ". First: " + $res.first + ". Re-reads: " + $res.detail) }
                    'inconclusive' { Write-ToolLog ("      mismatch inconclusive: the mode moved during the re-reads" + $switchNote + ". First: " + $res.first + ". Re-reads: " + $res.detail) }
                    default        {
                        $open = $episode
                        Write-ToolLog ("      WARNING: SUSTAINED disagreement, held " + $res.heldFor + " s with the mode at " + $sample.modeBefore + $switchNote + ". First: " + $res.first + ". Re-reads: " + $res.detail)
                    }
                }
            }
        } else {
            $counts[$verdict]++
            if ($null -ne $open -and $verdict -eq 'agree' -and "$($open.mode)" -eq "$($sample.modeBefore)") {
                $open['resolvedAt'] = $t0
                Write-ToolLog ("      agreement resumed at t=" + $t0 + " s, " + [Math]::Round($t0 - $open.t, 1) + " s after the sustained mismatch at t=" + $open.t + " s, at the same mode: consistent with a repaint lag beyond the timeout; a standing divergence would not agree again without a mode change")
                $open = $null
            }
        }

        $prev = $sample
        $rest = 1000 - $sample.ms
        if ($rest -gt 0 -and $window.Elapsed.TotalSeconds -lt $Seconds) { Start-Sleep -Milliseconds $rest }
    }
    $unresolved = 0; $resolvedLags = @()
    foreach ($ep in $mismatches) {
        if ($ep.kind -ne 'sustained') { continue }
        if ($null -eq $ep.resolvedAt) { $unresolved++ } else { $resolvedLags += [Math]::Round($ep.resolvedAt - $ep.t, 1) }
    }
    return @{
        counts = $counts; changed = @($changedFields.Keys | Sort-Object); battLabels = @($battLabels.Keys | Sort-Object)
        first = $first; last = $last; mismatches = $mismatches; unresolved = $unresolved; resolvedLags = $resolvedLags
        seconds = [Math]::Round($window.Elapsed.TotalSeconds, 1)
    }
}

function Read-ColourAtAnswer {
    # The operator's colour beside the pair re-read at the moment of the
    # answer, as Get-LenovoLighting.ps1 does.
    param([string]$Key, [System.Diagnostics.Stopwatch]$SincePhase)
    $answer = Read-Operator ("  Power button colour now, after phase " + $Key + " (blue / white / red / all three / off / other; Enter = not observed)")
    $m = Get-Mode
    $s = Get-State4
    $when = [Math]::Round($SincePhase.Elapsed.TotalSeconds, 1)
    if ($answer.Length -eq 0) {
        Write-ToolLog ("  operator (" + $when + " s into the phase): not observed (empty answer); at the answer mode " + $m + ", id4 " + $s)
    } else {
        Write-ToolLog ("  operator (" + $when + " s into the phase): '" + $answer + "'; at the answer mode " + $m + ", id4 " + $s)
    }
    return @{ colour = $answer; mode = $m; state4 = $s }
}

function Set-FullSpeedThroughModule {
    # Parameter name and CIM type from the firmware, not guessed. A setter
    # bound to the wrong parameter could act in the wrong direction.
    param([bool]$On)
    $inParams = $null
    try { $inParams = $fm.GetMethodParameters('Fan_Set_FullSpeed') } catch { $inParams = $null }
    if ($null -eq $inParams) {
        Write-ToolLog "  Fan_Set_FullSpeed: GetMethodParameters unavailable; not guessing a positional call for a setter."
        return $false
    }
    $names = @($inParams.Properties | ForEach-Object { $_.Name })
    if ($names.Count -ne 1) {
        Write-ToolLog ("  Fan_Set_FullSpeed declares " + $names.Count + " input parameters (" + ($names -join ', ') + "); expected one. Not calling it.")
        return $false
    }
    $pname = $names[0]
    $ptype = $inParams.Properties[$pname].Type
    $value = $null
    if ("$ptype" -eq 'Boolean') { $value = $On } else { $value = $(if ($On) { 1 } else { 0 }) }
    $arguments = @{}
    $arguments[$pname] = $value
    Write-ToolLog ("  Fan_Set_FullSpeed(" + $pname + " [" + $ptype + "] = " + $value + ")")
    [void](Invoke-LenovoWmiMethod -WmiObject $fm -Method 'Fan_Set_FullSpeed' -Arguments $arguments -Property 'Status' -AsObject)
    return $true
}

function Disable-FullSpeedIfCalled {
    if (-not $script:FullSpeedCalled) { return }
    # Re-read the mode now rather than trusting the phase-start read: two
    # open-ended operator prompts have passed since. Disabling full speed in
    # Custom drops straight back to the EC-held table (CLAUDE.md), and the
    # documented order is to change the power mode first. Neither this tool
    # nor Fn+Q can enter Custom, so getting here needs Vantage or another
    # process; the check is cheap and the wrong order is the fans-off hazard.
    $modeNow = Get-Mode
    if ("$modeNow" -eq '255') {
        Write-ToolLog ("  WARNING: the mode reads Custom (255) now. Disabling full speed here would drop to the EC-held table, so the power mode has to change first.")
        $left = $false
        try {
            [void](Read-Operator ("  Press Fn+Q until the OSD shows a mode other than Custom, then press Enter"))
            $modeNow = Get-Mode
            $left = ("$modeNow" -ne '255')
        } catch {
            $left = $false
        }
        if (-not $left) {
            Write-ToolLog ("  Leaving full speed ON: the mode still reads " + $modeNow + ". Change the power mode first, then run: fancontrol.exe set 0 0")
            return
        }
        Write-ToolLog ("  mode now " + $modeNow + "; disabling full speed.")
    }
    Write-ToolLog "  Disabling full speed again."
    [void](Set-FullSpeedThroughModule -On $false)
    Start-Sleep -Milliseconds 700
    $rb = Get-FullSpeed
    Write-ToolLog ("  Fan_Get_FullSpeed read back: " + $rb)
    if ("$rb" -eq 'False') {
        $script:FullSpeedCalled = $false
    } else {
        Write-ToolLog ("  ERROR: full speed did not read back False. Change the power mode first if the machine is in Custom, then run: fancontrol.exe set 0 0")
    }
}

# ---------------------------------------------------------------------------
# Phases
# ---------------------------------------------------------------------------

$catalog = [ordered]@{
    baseline  = @{ title = 'control, no action';                       instruction = 'Do nothing.';                       seconds = $PhaseSeconds }
    unplug    = @{ title = 'AC adapter out, in Performance';           instruction = 'Unplug the AC adapter.';            seconds = $UnplugSeconds }
    replug    = @{ title = 'AC adapter back in';                       instruction = 'Plug the AC adapter back in.';      seconds = $UnplugSeconds }
    sleepwake = @{ title = 'sleep and wake, sampling the first window after resume'; instruction = 'Put the machine to sleep, wake it and sign in.'; seconds = $PhaseSeconds; beforeEnter = $true }
    fnq       = @{ title = 'Fn+Q once (EC hotkey path)';               instruction = 'Press Fn+Q once.';                  seconds = $PhaseSeconds }
    fnspace   = @{ title = 'Fn+Space once (keyboard backlight)';       instruction = 'Press Fn+Space once.';              seconds = $PhaseSeconds }
    fullspeed = @{ title = 'Fan_Set_FullSpeed(1) for one window';      instruction = 'Do nothing; the tool enables full speed itself.'; seconds = $PhaseSeconds }
    vantage   = @{ title = 'Lenovo Vantage power-button setting, if any'; instruction = '(set at run time)';              seconds = $PhaseSeconds }
}

function Invoke-Phase {
    param([string]$Key)
    $spec = $catalog[$Key]
    $record = [ordered]@{
        key = $Key; title = $spec.title; label = ''; skipped = $false; skipReason = ''
        modeAtStart = $null; modeAtEnd = $null; state4AtStart = $null; state4AtEnd = $null
        battAtStart = $null; battAtEnd = $null; changed = @(); battLabels = @()
        counts = $null; colour = $null; vantageOffer = $null; keyboardSeen = $null; note = ''; measuredNothing = $false
        unresolved = 0; resolvedLags = @(); mismatches = @()
    }
    Write-ToolLog ""
    Write-ToolLog ("=== Phase " + $Key + ": " + $spec.title + " ===")
    $phaseClock = [System.Diagnostics.Stopwatch]::StartNew()
    $instruction = $spec.instruction
    $seconds = [int]$spec.seconds

    $modeNow = Get-Mode
    $record['modeAtStart'] = $modeNow
    Write-ToolLog ("  mode at phase start: " + $modeNow)

    if ($Key -eq 'unplug' -and "$modeNow" -ne '3') {
        Write-ToolLog ("  Mode reads " + $modeNow + "; the downgrade test needs Performance (3).")
        $nudge = $true
        if ("$modeNow" -eq '255') {
            # The same gate the fnq phase applies: Fn+Q leaves Custom and this
            # tool cannot put it back.
            Write-ToolLog "  WARNING: the machine is in Custom (255). Fn+Q leaves it and cannot re-enter it; this tool cannot restore it (a set-curve can)."
            $go = Read-Operator ("  Type yes to press Fn+Q anyway, anything else to run this phase in Custom")
            if ($go -ne 'yes') { $nudge = $false }
        }
        if ($nudge) {
            [void](Read-Operator ("  Press Fn+Q until the OSD shows Performance, then press Enter"))
            $modeNow = Get-Mode
            $record['modeAtStart'] = $modeNow
            Write-ToolLog ("  mode now: " + $modeNow)
        }
        if ("$modeNow" -ne '3') {
            $record['label'] = "run in mode " + $modeNow + "; the downgrade test needs 3"
            Write-ToolLog ("  Still " + $modeNow + ". Running the phase anyway, labelled: " + $record['label'])
        }
    }

    if ($Key -eq 'fnq' -and "$modeNow" -eq '255') {
        Write-ToolLog "  WARNING: the machine is in Custom (255). Fn+Q leaves it and cannot re-enter it; this tool cannot restore it (a set-curve can)."
        $go = Read-Operator ("  Type yes to press Fn+Q anyway, anything else to skip this phase")
        if ($go -ne 'yes') {
            $record['skipped'] = $true; $record['skipReason'] = 'operator declined to leave Custom'
            Write-ToolLog "  Phase skipped."
            return $record
        }
    }

    if ($Key -eq 'vantage') {
        $offer = Read-Operator ("  Open Lenovo Vantage and look for any setting that claims to control the power-button light. Type what it offers, verbatim, or none")
        $record['vantageOffer'] = $offer
        Write-ToolLog ("  Vantage offers (operator, verbatim): '" + $offer + "'")
        if ($offer.Length -eq 0 -or $offer -eq 'none') {
            $instruction = 'Do nothing (Vantage offers no power-button setting; this window is a second control).'
        } else {
            $instruction = 'Change that Vantage setting: ' + $offer
        }
    }

    if ($Key -eq 'fullspeed') {
        $before = Get-FullSpeed
        Write-ToolLog ("  Fan_Get_FullSpeed before: " + $before)
        if ("$before" -eq 'True') {
            $record['skipped'] = $true; $record['skipReason'] = 'full speed already on; nothing to attribute'
            Write-ToolLog ("  Phase skipped: " + $record['skipReason'])
            return $record
        }
        if ("$modeNow" -eq '255') {
            $record['skipped'] = $true; $record['skipReason'] = 'mode is Custom (255); disabling full speed afterwards would drop to the EC-held table'
            Write-ToolLog ("  Phase skipped: " + $record['skipReason'])
            return $record
        }
    }

    if ($Key -eq 'sleepwake' -and "$modeNow" -eq '255') {
        $record['skipped'] = $true; $record['skipReason'] = 'mode is Custom (255); curve retention across sleep is unmeasured, and waking into Custom with a lost table is the fans-off hazard'
        Write-ToolLog ("  Phase skipped: " + $record['skipReason'])
        return $record
    }

    # Most phases act inside the window. A phase marked beforeEnter acts
    # first and samples from Enter, so the window sees the state right after
    # the action rather than the action itself.
    if ($spec.ContainsKey('beforeEnter') -and $spec.beforeEnter) {
        [void](Read-Operator ("  Now: " + $instruction + " Press Enter AFTER that; sampling starts at Enter and runs " + $seconds + " s"))
    } else {
        [void](Read-Operator ("  Press Enter to start sampling, then, while it samples for " + $seconds + " s: " + $instruction))
    }

    if ($Key -eq 'fullspeed') {
        $called = Set-FullSpeedThroughModule -On $true
        if (-not $called) {
            $record['skipped'] = $true; $record['skipReason'] = 'setter not called (see above)'
            return $record
        }
        $script:FullSpeedCalled = $true
        Start-Sleep -Milliseconds 700
        $rb = Get-FullSpeed
        Write-ToolLog ("  Fan_Get_FullSpeed read back: " + $rb)
        if ("$rb" -ne 'True') {
            $record['note'] = 'enable did not read back True; the window is not a full-speed window'
            Write-ToolLog ("  ERROR: " + $record['note'])
        }
    }

    Write-ToolLog ("  sampling for " + $seconds + " s (" + $instruction + ")")
    $win = Invoke-SampleWindow -Seconds $seconds
    $record['counts'] = $win.counts
    $record['changed'] = $win.changed
    $record['battLabels'] = $win.battLabels
    $record['unresolved'] = $win.unresolved
    $record['resolvedLags'] = @($win.resolvedLags)
    $record['mismatches'] = @($win.mismatches)
    if ($null -ne $win.first) {
        $record['state4AtStart'] = $(if ($win.first.states.Contains('4')) { $win.first.states['4'].s } else { $null })
        $record['battAtStart'] = $win.first.batt
    }
    if ($null -ne $win.last) {
        $record['modeAtEnd'] = $win.last.modeAfter
        $record['state4AtEnd'] = $(if ($win.last.states.Contains('4')) { $win.last.states['4'].s } else { $null })
        $record['battAtEnd'] = $win.last.batt
    }
    $c = $win.counts
    Write-ToolLog ("  window closed after " + $win.seconds + " s: " + $c['samples'] + " samples; agree " + $c['agree'] +
                   ", transient " + $c['transient'] + ", inconclusive " + $c['inconclusive'] +
                   ", sustained " + $c['sustained'] + " (unresolved " + $win.unresolved + ", continued " + $c['sustained-continued'] + ")" +
                   ", switching " + $c['switching'] +
                   ", no-expectation " + $c['no-expectation'] + ", no-reading " + $c['no-reading'] + ", no-mode " + $c['no-mode'])
    if ($win.changed.Count -gt 0) {
        Write-ToolLog ("  fields that moved during the window: " + ($win.changed -join ', '))
    } else {
        Write-ToolLog "  no field moved during the window."
    }

    # The adapter phases measure a transition, which has to fall inside the
    # window: Win32_Battery must report both AC and battery during it. A window
    # that saw only one label measured nothing about the transition, whether
    # the operator missed the cue or acted before sampling started, and the
    # summary keeps such a phase out of the manipulation count.
    if ($Key -eq 'unplug' -or $Key -eq 'replug') {
        $sawAC = ($win.battLabels -contains 'AC')
        $sawBattery = ($win.battLabels -contains 'battery')
        if (-not ($sawAC -and $sawBattery)) {
            $record['measuredNothing'] = $true
            $record['note'] = 'Win32_Battery reported only ' + ($win.battLabels -join '/') + ' during the window, so no AC transition fell inside it; this phase measured nothing about the adapter'
            Write-ToolLog ("  NOTE: " + $record['note'])
        }
    }

    if ($Key -eq 'fnspace') {
        # Nothing objective confirms the keypress (the mode and the battery do
        # for the other phases), so the log carries the operator's word on
        # what the keyboard did beside the lighting class's silence.
        $kb = Read-Operator ("  What did the keyboard backlight do (verbatim; Enter = not observed)")
        $record['keyboardSeen'] = $kb
        if ($kb.Length -eq 0) { Write-ToolLog "  operator, keyboard: not observed (empty answer)" }
        else { Write-ToolLog ("  operator, keyboard: '" + $kb + "'") }
    }

    $obs = Read-ColourAtAnswer -Key $Key -SincePhase $phaseClock
    $record['colour'] = $obs.colour
    $record['modeAtEnd'] = $obs.mode
    $record['state4AtEnd'] = $obs.state4

    if ($Key -eq 'fullspeed') { Disable-FullSpeedIfCalled }

    return $record
}

# ---------------------------------------------------------------------------
# Run
# ---------------------------------------------------------------------------

$fatal = $null
try {
    if (-not (Test-Elevated)) { throw "not elevated" }

    $phaseList = @($Phases)
    if ($IncludeFullSpeed -and -not ($phaseList -contains 'fullspeed')) {
        # Before vantage (which is last by design), else appended. A negative
        # slice bound wraps around in PowerShell, so idx 0 is its own case.
        $idx = [Array]::IndexOf($phaseList, 'vantage')
        if ($idx -gt 0) {
            $phaseList = @($phaseList[0..($idx - 1)]) + @('fullspeed') + @($phaseList[$idx..($phaseList.Count - 1)])
        } elseif ($idx -eq 0) {
            $phaseList = @('fullspeed') + @($phaseList)
        } else {
            $phaseList = @($phaseList) + @('fullspeed')
        }
    }
    foreach ($k in $phaseList) {
        if (-not $catalog.Contains($k)) { throw ("unknown phase '" + $k + "'; known: " + (@($catalog.Keys) -join ', ')) }
    }
    if (($phaseList -contains 'fullspeed') -and -not $IncludeFullSpeed) {
        throw "the fullspeed phase writes to the EC and needs -IncludeFullSpeed"
    }
    Write-ToolLog ("Phases: " + ($phaseList -join ', '))
    if (-not ($LightingIds -contains 4)) {
        throw "-LightingIds must include 4, the one id measured to track the mode; without it no sample carries an expectation"
    }

    $gz = Get-LenovoWmiClass -ClassName 'LENOVO_GAMEZONE_DATA' -Single
    $lm = Get-LenovoWmiClass -ClassName 'LENOVO_LIGHTING_METHOD' -Single
    $fm = Get-LenovoWmiClass -ClassName 'LENOVO_FAN_METHOD' -Single
    if ($null -eq $gz) { throw "LENOVO_GAMEZONE_DATA unavailable" }
    if ($null -eq $lm) {
        Write-ToolLog "RESULT: LENOVO_LIGHTING_METHOD is absent on this firmware; there is no id 4 to compare with the mode."
        $result['lightingMethodPresent'] = $false
        throw "LENOVO_LIGHTING_METHOD absent"
    }
    $result['lightingMethodPresent'] = $true
    if ($IncludeFullSpeed -and $null -eq $fm) { throw "LENOVO_FAN_METHOD unavailable, so the fullspeed phase cannot run" }

    Write-ToolLog ""
    Write-ToolLog "--- LENOVO_LIGHTING_DATA descriptor rows (static; for reading the ids) ---"
    try {
        $rows = @(Get-WmiObject -Namespace root/WMI -Class LENOVO_LIGHTING_DATA -ErrorAction Stop)
        foreach ($row in $rows) {
            $lid = Get-WmiPropertyOrNull -InputObject $row -Name 'Lighting_Id'
            $bri = Get-WmiPropertyOrNull -InputObject $row -Name 'Brightness_Level'
            $st = Get-WmiPropertyOrNull -InputObject $row -Name 'State_Type_Num'
            $ltype = Get-WmiPropertyOrNull -InputObject $row -Name 'Lighting_Type'
            $ci = Get-WmiPropertyOrNull -InputObject $row -Name 'Control_Interface'
            Write-ToolLog ("  Lighting_Id={0}  Type={1}  Brightness={2}  StateTypeNum={3}  ControlIface={4}" -f $lid, $ltype, $bri, $st, $ci)
        }
    } catch {
        Write-ToolLog ("  ERROR: " + $_.Exception.Message)
    }

    $startMode = Get-Mode
    $result['startMode'] = $startMode
    Write-ToolLog ""
    Write-ToolLog ("SmartFanMode at start: " + $startMode + " (this tool never changes it; Fn+Q phases will)")
    if ($null -eq $startMode) { throw "cannot read SmartFanMode" }
    if ("$startMode" -eq '255') {
        Write-ToolLog "WARNING: starting in Custom (255). Fn+Q leaves Custom and cannot re-enter it; this tool cannot restore it. The unplug and fnq phases ask before any Fn+Q."
    }
    Write-ToolLog "Initial sample:"
    $initial = Read-Sample
    Write-ToolLog ("  " + (Format-Sample $initial))
    $result['initialSample'] = (Format-Sample $initial)

    foreach ($k in $phaseList) {
        $rec = Invoke-Phase -Key $k
        [void]$phaseRecords.Add($rec)
    }

    $endMode = Get-Mode
    $result['endMode'] = $endMode
    Write-ToolLog ""
    Write-ToolLog ("SmartFanMode at end: " + $endMode + " (started at " + $startMode + ")")
    if ("$endMode" -ne "$startMode") {
        if ("$startMode" -eq '255') {
            Write-ToolLog "  The run left Custom. Fn+Q cannot return to it; a set-curve re-enters Custom with the curve it writes."
        } else {
            Write-ToolLog ("  Use Fn+Q to return to " + $startMode + " (cycle 1 -> 2 -> 3 -> 1) if that is where you want the machine.")
        }
    }
    $result['ok'] = $true
} catch {
    $fatal = $_.Exception.Message
    Write-ToolLog ("FATAL: " + $fatal)
} finally {
    if ($script:FullSpeedCalled) {
        Write-ToolLog ""
        Write-ToolLog "finally: full speed was enabled by this tool and not confirmed off."
        try { Disable-FullSpeedIfCalled } catch { Write-ToolLog ("  ERROR: " + $_.Exception.Message) }
    }
}

# ---------------------------------------------------------------------------
# Summary and verdict
# ---------------------------------------------------------------------------

if ($phaseRecords.Count -gt 0) {
    Write-ToolLog ""
    Write-ToolLog "--- Per phase: mode and id 4 at start -> end, battery, fields that moved, sample verdicts, operator colour ---"
    Write-ToolLog "    (mode and id 4 at the end are the pair re-read at the operator's answer; battery at the end is the window's last sample)"
    $sustainedTotal = 0; $unresolvedTotal = 0; $transientTotal = 0; $inconclusiveTotal = 0; $expectTotal = 0
    $lagsAll = @()
    $manipulations = @()
    $unresolvedIn = @()
    foreach ($r in $phaseRecords) {
        if ($r['skipped']) {
            Write-ToolLog ("  " + $r['key'] + ": skipped (" + $r['skipReason'] + ")")
            continue
        }
        $c = $r['counts']
        $agree = 0; $tr = 0; $su = 0; $suc = 0; $inc = 0; $n = 0
        if ($null -ne $c) { $agree = $c['agree']; $tr = $c['transient']; $su = $c['sustained']; $suc = $c['sustained-continued']; $inc = $c['inconclusive']; $n = $c['samples'] }
        $un = [int]$r['unresolved']
        $sustainedTotal += $su; $unresolvedTotal += $un; $transientTotal += $tr; $inconclusiveTotal += $inc
        $expectTotal += ($agree + $tr + $su + $suc + $inc)
        $lagsAll += @($r['resolvedLags'])
        if ($un -gt 0) { $unresolvedIn += ($r['key'] + " (" + $un + ")") }
        # Baseline is the control; a vantage phase answered "none" is a second
        # control, not a manipulation, and must not pad the verdict's count.
        $isControl = ($r['key'] -eq 'baseline')
        if ($r['key'] -eq 'vantage') {
            $offer = $r['vantageOffer']
            if ($null -eq $offer -or $offer.Length -eq 0 -or $offer -eq 'none') { $isControl = $true }
        }
        # A phase whose note says it measured nothing (no AC transition fell in
        # its window) is not a manipulation either; its samples still count.
        if (-not $isControl -and -not $r['measuredNothing']) { $manipulations += $r['key'] }
        $colour = $(if ($null -eq $r['colour'] -or $r['colour'].Length -eq 0) { '(not observed)' } else { $r['colour'] })
        $moved = $(if ($r['changed'].Count -gt 0) { ($r['changed'] -join ', ') } else { 'none' })
        $label = $(if ($r['label'].Length -gt 0) { ' [' + $r['label'] + ']' } else { '' })
        if ($r['measuredNothing']) { $label = $label + ' [measured nothing]' }
        $lagText = $(if (@($r['resolvedLags']).Count -gt 0) { '; sustained episodes that agreed again later: ' + (@($r['resolvedLags']) -join ', ') + ' s' } else { '' })
        Write-ToolLog ("  " + $r['key'] + $label + ": mode " + $r['modeAtStart'] + " -> " + $r['modeAtEnd'] +
                       ", id4 " + $r['state4AtStart'] + " -> " + $r['state4AtEnd'] +
                       ", battery " + $r['battAtStart'] + " -> " + $r['battAtEnd'] +
                       "; moved: " + $moved +
                       "; " + $n + " samples, agree " + $agree + ", transient " + $tr + ", inconclusive " + $inc + ", sustained " + $su + " (unresolved " + $un + ")" + $lagText +
                       "; colour: " + $colour)
        if ($r['note'].Length -gt 0) { Write-ToolLog ("      note: " + $r['note']) }
        if ($null -ne $r['vantageOffer']) { Write-ToolLog ("      Vantage offers: '" + $r['vantageOffer'] + "'") }
        if ($null -ne $r['keyboardSeen']) { Write-ToolLog ("      keyboard (operator): '" + $r['keyboardSeen'] + "'") }
    }
    Write-ToolLog ""
    if ($expectTotal -eq 0) {
        # A run in which nothing readable happened must not print a pass.
        $verdict = "NO VERDICT: no sample carried an expectation (agree, transient, sustained and inconclusive are all 0). Read the no-reading, no-mode and no-expectation counts per phase."
        $result['ok'] = $false
    } elseif ($unresolvedTotal -gt 0) {
        $verdict = "Lighting_Id 4 DISAGREED with SmartFanMode: " + $unresolvedTotal + " sustained episode(s) never agreed again within the window, in: " + ($unresolvedIn -join ', ') + ". Read those phases' sample lines; the WARNING lines carry the re-reads."
    } elseif ($sustainedTotal -gt 0) {
        $verdict = "No standing disagreement: " + $sustainedTotal + " sustained episode(s) all agreed again later at the same mode (after " + ($lagsAll -join ', ') + " s), across " + $expectTotal + " samples with an expectation and " + $manipulations.Count + " manipulations (" + ($manipulations -join ', ') + "). Consistent with a repaint lag beyond the " + $MismatchTimeoutSeconds + " s timeout; a standing divergence would not have agreed again without a mode change."
    } else {
        $verdict = "Lighting_Id 4 did not disagree with SmartFanMode across " + $expectTotal + " samples with an expectation (" + $transientTotal + " transient mismatches that agreed on re-read, " + $inconclusiveTotal + " inconclusive) and " + $manipulations.Count + " manipulations (" + ($manipulations -join ', ') + ")."
    }
    Write-ToolLog ("VERDICT: " + $verdict)
    Write-ToolLog ("The fullspeed phase, if run, also answers whether the LED changes under full speed (the operator's colour in that row); its samples count toward the verdict like any other window.")
    $result['verdict'] = $verdict
    $result['sustained'] = $sustainedTotal
    $result['unresolved'] = $unresolvedTotal
    $result['transient'] = $transientTotal
    $result['inconclusive'] = $inconclusiveTotal
    $result['samplesWithExpectation'] = $expectTotal
    $phaseObjects = @()
    foreach ($r in $phaseRecords) { $phaseObjects += (New-Object PSObject -Property $r) }
    $result['phases'] = $phaseObjects
}

Write-ToolLog ""
if ($script:FullSpeedCalled) {
    Write-ToolLog ("OWED: full speed may still be on (see the ERROR above). Change the power mode first if in Custom, then: fancontrol.exe set 0 0")
} else {
    Write-ToolLog "Nothing owed by this run: it wrote no table and selected no mode."
}
# True of the tool, not necessarily of the machine: the operator's Fn+Q or
# Vantage can leave it in Custom, which runs whatever table the EC holds.
if ($result.Contains('endMode') -and "$($result['endMode'])" -eq '255') {
    Write-ToolLog ("The machine is in Custom (255) now, running whatever table the EC holds; this tool did not select it. If that table is not one you wrote on purpose, run tools\Reset-LenovoFanState.ps1.")
}
Write-ToolLog ("Log: " + $LogPath)

if ($Json) { New-Object PSObject -Property $result | ConvertTo-Json -Depth 6 }
else { New-Object PSObject -Property $result }

if ($null -ne $fatal) { exit 1 }
