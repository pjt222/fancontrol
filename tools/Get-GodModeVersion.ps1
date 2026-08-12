<#
.SYNOPSIS
Determine whether this machine takes LenovoLegionToolkit's GodMode V1 or V2 fan
curve rules, and report whether the fan max-speed properties are populated.

.DESCRIPTION
READ-ONLY. Invokes no setter, writes no fan table, changes no power mode. It
reads version numbers and table properties only, so it is safe at idle and needs
no thermal load.

The answer matters because LLT keeps two minimum-step tables and validates
element-wise, valid iff minimum[i] <= step[i] <= 10:

    GodMode V1   [0,0,0,0,0,0,0,1,3,5]   steps 0-6 may be 0
    GodMode V2   [1,1,1,1,1,1,1,1,3,5]   no step may be 0

fancontrol currently follows V1. If this machine is V2, we accept curves the
firmware may reject.

Accessors mirror LLT (archived 2025-07-24):
    SmartFanVersion       LENOVO_GAMEZONE_DATA.IsSupportSmartFan()             -> Data
    SupportedPowerModes   LENOVO_OTHER_METHOD.GetFeatureValue(0x00070000)      -> Value, bit 16 = GodMode
    LegionZoneVersion     LENOVO_OTHER_METHOD.GetFeatureValue(0x00090000)      -> Value
      fallback            LENOVO_OTHER_METHOD.Get_Support_LegionZone_Version() -> Version

.PARAMETER LogPath
Where to write the log. Defaults to Get-GodModeVersion.log beside this script.

.PARAMETER Json
Emit the result object as JSON on stdout, for machine consumption.

.EXAMPLE
.\Get-GodModeVersion.ps1
Run interactively in an elevated PowerShell and read the verdict.

.EXAMPLE
.\Get-GodModeVersion.ps1 -Json > result.json
Capture a machine-readable result.

.NOTES
Requires an elevated shell: the Lenovo classes live in root\WMI and return
"access denied" otherwise. Issue #25.
#>
#Requires -RunAsAdministrator
[CmdletBinding()]
param(
    [string]$LogPath,
    [switch]$Json
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

Import-Module (Join-Path $PSScriptRoot 'LenovoWmi.psm1') -Force

if (-not $LogPath) {
    $LogPath = Join-Path $PSScriptRoot 'Get-GodModeVersion.log'
}

Start-ToolLog -Path $LogPath -Title 'GodMode V1/V2 Version Probe (issue #25)' -ReadOnly

if (-not (Test-Elevated)) {
    Write-ToolLog "FATAL: not elevated. The Lenovo classes live in root\WMI and"
    Write-ToolLog "       will return access denied. Re-run as Administrator."
    exit 1
}

# ---------------------------------------------------------------------------
# Model and BIOS
# ---------------------------------------------------------------------------
Write-ToolLog "--- Model and BIOS ---"

$system = Get-WmiObject -Class Win32_ComputerSystem
Write-ToolLog ("  Manufacturer = " + $system.Manufacturer)
Write-ToolLog ("  Model = " + $system.Model)
Write-ToolLog ("  ProductVersion = " + (Get-WmiObject -Class Win32_ComputerSystemProduct).Version)

$bios = Get-LenovoBiosVersion
Write-ToolLog ("  BIOSVersion raw = " + $bios.Raw)
Write-ToolLog ("  Parsed per LLT: prefix = " + $bios.Prefix + ", version = " + $bios.Version)

# V1 is blocked only when the prefix MATCHES a blocklist entry and the version is
# lower. IsLowerThan returns false outright when prefixes differ.
$biosBlocklist = @{ 'G9CN' = 24; 'GKCN' = 46; 'H1CN' = 39; 'HACN' = 31; 'HHCN' = 20 }
Write-ToolLog "  V1 BIOS blocklist: G9CN 24, GKCN 46, H1CN 39, HACN 31, HHCN 20"
$biosBlocksV1 = $false
if ($biosBlocklist.ContainsKey($bios.Prefix)) {
    $minimum = $biosBlocklist[$bios.Prefix]
    if ([int]$bios.Version -lt $minimum) {
        $biosBlocksV1 = $true
        Write-ToolLog ("  BIOS gate BLOCKS V1: prefix matches and " + $bios.Version + " < " + $minimum)
    } else {
        Write-ToolLog ("  BIOS gate allows V1: prefix matches but " + $bios.Version + " >= " + $minimum)
    }
} else {
    Write-ToolLog "  BIOS gate allows V1: prefix not in blocklist, IsLowerThan returns false"
}
Write-ToolLog ""

# ---------------------------------------------------------------------------
# SmartFanVersion
# ---------------------------------------------------------------------------
Write-ToolLog "--- SmartFanVersion (LENOVO_GAMEZONE_DATA.IsSupportSmartFan) ---"
$gameZone = Get-LenovoWmiClass -ClassName 'LENOVO_GAMEZONE_DATA' -Single
$smartFanVersion = Invoke-LenovoWmiMethod -WmiObject $gameZone -Method 'IsSupportSmartFan' -Property 'Data'
if ($null -eq $smartFanVersion) { $smartFanVersion = -1 }
Write-ToolLog ("  SmartFanVersion = " + $smartFanVersion)
Write-ToolLog ""

# ---------------------------------------------------------------------------
# SupportedPowerModes and LegionZoneVersion
# ---------------------------------------------------------------------------
Write-ToolLog "--- LENOVO_OTHER_METHOD ---"
$otherMethod = Get-LenovoWmiClass -ClassName 'LENOVO_OTHER_METHOD' -Single

$powerModeMask = Invoke-LenovoWmiMethod -WmiObject $otherMethod -Method 'GetFeatureValue' `
    -Arguments @{ IDs = 0x00070000 } -PositionalArguments @(0x00070000) -Property 'Value'

if ($null -eq $powerModeMask) {
    Write-ToolLog "  GetFeatureValue(SupportedPowerModes) unavailable, trying GetSupportThermalMode"
    # Same bit layout, so the same decode applies to either source.
    $powerModeMask = Invoke-LenovoWmiMethod -WmiObject $otherMethod `
        -Method 'GetSupportThermalMode' -Property 'mode'
}

$godModeSupported = $false
if ($null -ne $powerModeMask) {
    $mask = [int]$powerModeMask
    Write-ToolLog ("  Power mode bitmask = " + $mask)
    Write-ToolLog ("    bit 0  Quiet       = " + [bool]($mask -band 1))
    Write-ToolLog ("    bit 1  Balance     = " + [bool]($mask -band 2))
    Write-ToolLog ("    bit 2  Performance = " + [bool]($mask -band 4))
    $godModeSupported = [bool]($mask -band 65536)
    Write-ToolLog ("    bit 16 GodMode     = " + $godModeSupported)
} else {
    Write-ToolLog "  WARNING: no power mode bitmask available from either method"
}

$legionZoneVersion = Invoke-LenovoWmiMethod -WmiObject $otherMethod -Method 'GetFeatureValue' `
    -Arguments @{ IDs = 0x00090000 } -PositionalArguments @(0x00090000) -Property 'Value'
if ($null -eq $legionZoneVersion) {
    Write-ToolLog "  Falling back to Get_Support_LegionZone_Version"
    $legionZoneVersion = Invoke-LenovoWmiMethod -WmiObject $otherMethod `
        -Method 'Get_Support_LegionZone_Version' -Property 'Version'
}
if ($null -eq $legionZoneVersion) { $legionZoneVersion = -1 }
Write-ToolLog ("  LegionZoneVersion = " + $legionZoneVersion)
Write-ToolLog ""

# ---------------------------------------------------------------------------
# Fan max-speed properties
# ---------------------------------------------------------------------------
Write-ToolLog "--- LENOVO_FAN_TABLE_DATA max-speed properties ---"
Write-ToolLog "  Checking CurrentFanMaxSpeed / DefaultFanMaxSpeed, which LLT reads"
Write-ToolLog "  instead of the stubbed Fan_Get_MaxSpeed method."
$maxSpeeds = @()
$tables = Get-LenovoWmiClass -ClassName 'LENOVO_FAN_TABLE_DATA'
if ($tables) {
    foreach ($table in $tables) {
        Write-ToolLog ("  --- Fan_Id=" + $table.Fan_Id + " Sensor_ID=" + $table.Sensor_ID + " ---")
        Write-WmiProperties $table
        # Read through Get-WmiPropertyOrNull: DefaultFanMaxSpeed is absent on the
        # 82RG, and under Set-StrictMode a direct access would abort the run.
        $maxSpeeds += [pscustomobject]@{
            FanId              = Get-WmiPropertyOrNull $table 'Fan_Id'
            SensorId           = Get-WmiPropertyOrNull $table 'Sensor_ID'
            CurrentFanMinSpeed = Get-WmiPropertyOrNull $table 'CurrentFanMinSpeed'
            CurrentFanMaxSpeed = Get-WmiPropertyOrNull $table 'CurrentFanMaxSpeed'
            DefaultFanMaxSpeed = Get-WmiPropertyOrNull $table 'DefaultFanMaxSpeed'
            FanTableLen        = Get-WmiPropertyOrNull $table 'FanTable_Len'
        }
    }

    # Step-scale observation, relevant to issue #18. fancontrol allows step values
    # 0..MAX_STEP_VALUE (10), which is eleven distinct values, while the firmware
    # table holds FanTable_Len entries. If the table has ten, the counts do not
    # line up under a direct-index reading, and "0 = off, 1..10 index the ten
    # entries" fits. Suggestive, not proof -- the load test in #18 decides.
    $firstLen = Get-WmiPropertyOrNull $tables[0] 'FanTable_Len'
    if ($null -ne $firstLen) {
        Write-ToolLog ""
        Write-ToolLog ("  FanTable_Len = " + $firstLen + " entries, indices 0.." + ([int]$firstLen - 1))
        Write-ToolLog "  fancontrol step range = 0..10, i.e. 11 distinct values"
        if ([int]$firstLen -eq 10) {
            Write-ToolLog "  => 11 step values over a 10-entry table. A direct-index reading"
            Write-ToolLog "     leaves step 10 out of bounds, which favours 0 = off with"
            Write-ToolLog "     steps 1..10 mapping to entries 0..9. See issue #18."
        }
    }
}
Write-ToolLog ""

# ---------------------------------------------------------------------------
# Verdict
# ---------------------------------------------------------------------------
Write-ToolLog "--- Verdict ---"
Write-ToolLog ("  SmartFanVersion   = " + $smartFanVersion)
Write-ToolLog ("  LegionZoneVersion = " + $legionZoneVersion)
Write-ToolLog ("  GodMode supported = " + $godModeSupported)
Write-ToolLog ("  BIOS blocks V1    = " + $biosBlocksV1)

$matchesV1 = $godModeSupported -and (-not $biosBlocksV1) -and `
    (($smartFanVersion -in 4, 5) -or ($legionZoneVersion -in 1, 2))
$matchesV2 = $godModeSupported -and `
    (($smartFanVersion -in 6, 7) -or ($legionZoneVersion -in 3, 4))

Write-ToolLog ("  Matches V1: " + $matchesV1 + "  (smartFan 4/5 or legionZone 1/2)")
Write-ToolLog ("  Matches V2: " + $matchesV2 + "  (smartFan 6/7 or legionZone 3/4)")

$verdict = 'UNDETERMINED'
if ($matchesV1) {
    # LLT's dispatcher checks V1 first, so V1 wins even when V2 also matches.
    $verdict = 'V1'
    Write-ToolLog "  => V1. LLT checks V1 first, so V1 wins even if V2 also matches."
    Write-ToolLog "     Minimum table [0,0,0,0,0,0,0,1,3,5]; steps 0-6 may be 0."
    Write-ToolLog "     fancontrol's current floors are correct."
} elseif ($matchesV2) {
    $verdict = 'V2'
    Write-ToolLog "  => V2. Minimum table [1,1,1,1,1,1,1,1,3,5]; no step may be 0."
    Write-ToolLog "     fancontrol is more permissive than the firmware. Curves with a"
    Write-ToolLog "     0 in steps 0-6 may be rejected at the WMI boundary."
} else {
    Write-ToolLog "  => NEITHER matched. Record the raw values above and re-read"
    Write-ToolLog "     LLT's Compatibility.cs before concluding."
}

Write-ToolLog ""
Write-ToolLog ("Log written to " + $LogPath)

$result = [pscustomobject]@{
    Manufacturer      = $system.Manufacturer
    Model             = $system.Model
    BiosRaw           = $bios.Raw
    BiosPrefix        = $bios.Prefix
    BiosVersion       = $bios.Version
    BiosBlocksV1      = $biosBlocksV1
    SmartFanVersion   = $smartFanVersion
    LegionZoneVersion = $legionZoneVersion
    GodModeSupported  = $godModeSupported
    Verdict           = $verdict
    FanMaxSpeeds      = $maxSpeeds
}

if ($Json) {
    $result | ConvertTo-Json -Depth 4
} else {
    $result
}
