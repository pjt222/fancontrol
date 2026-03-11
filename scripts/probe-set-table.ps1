#Requires -RunAsAdministrator
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$logFile = Join-Path $PSScriptRoot 'probe-set-table.log'

function Log($message) {
    $line = "[{0}] {1}" -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $message
    Write-Host $line
    $line | Out-File -FilePath $logFile -Append -Encoding utf8
}

function DumpProperties($obj, $indent = "  ") {
    $obj.Properties | ForEach-Object {
        $val = $_.Value
        if ($val -is [System.Array]) {
            $val = "[$($val -join ', ')]"
        }
        Log "${indent}$($_.Name) = $val"
    }
}

# Truncate log file
"" | Out-File -FilePath $logFile -Encoding utf8

Log "=== Fan_Set_Table Probe ==="
Log "Machine: $env:COMPUTERNAME"
Log "Date: $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"
Log ""

# ============================================================
# SECTION 1: Baseline -- current state before any changes
# ============================================================
Log "=== SECTION 1: Baseline State ==="
Log ""

# --- SmartFanMode via LENOVO_GAMEZONE_DATA ---
Log "--- SmartFanMode (LENOVO_GAMEZONE_DATA) ---"
$originalSmartFanMode = $null
try {
    $gz = Get-WmiObject -Namespace root/WMI -Class LENOVO_GAMEZONE_DATA
    Log "  GAMEZONE_DATA found."
    try {
        $result = $gz.GetSmartFanMode()
        Log "  GetSmartFanMode() result properties:"
        DumpProperties $result "    "
        # Try common property names
        foreach ($prop in @('mode', 'Mode', 'Data', 'SmartFanMode')) {
            try {
                $val = $result.$prop
                if ($null -ne $val) {
                    Log "  -> $prop = $val"
                    $originalSmartFanMode = $val
                }
            } catch {}
        }
        if ($null -eq $originalSmartFanMode) {
            Log "  WARNING: Could not determine SmartFanMode value from properties"
        } else {
            Log "  Current SmartFanMode: $originalSmartFanMode"
        }
    } catch {
        Log "  ERROR calling GetSmartFanMode(): $_"
    }
} catch {
    Log "  ERROR: LENOVO_GAMEZONE_DATA not found: $_"
}
Log ""

# --- Current fan speeds ---
Log "--- Current Fan Speeds & Temps ---"
try {
    $fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD
} catch {
    Log "FATAL: Cannot access LENOVO_FAN_METHOD: $_"
    Log "=== Probe aborted ==="
    exit 1
}
foreach ($fid in @(0, 1)) {
    try {
        $speed = ($fm.Fan_GetCurrentFanSpeed($fid)).CurrentFanSpeed
        Log "  Fan $fid speed: $speed RPM"
    } catch {
        Log "  Fan $fid speed: ERROR $_"
    }
}
foreach ($sid in @(3, 4)) {
    try {
        $temp = ($fm.Fan_GetCurrentSensorTemperature($sid)).CurrentSensorTemperature
        Log "  Sensor $sid temp: ${temp} C"
    } catch {
        Log "  Sensor $sid temp: ERROR $_"
    }
}
Log ""

# --- Baseline LENOVO_FAN_TABLE_DATA ---
Log "--- Baseline LENOVO_FAN_TABLE_DATA ---"
$baselineTables = @()
try {
    $tables = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_TABLE_DATA
    foreach ($t in $tables) {
        $entry = @{
            FanId = $t.Fan_Id
            SensorId = $t.Sensor_ID
            FanTableData = $t.FanTable_Data
            InstanceName = $t.InstanceName
        }
        $baselineTables += $entry
        Log ("  Entry: Fan_Id=$($t.Fan_Id), Sensor_ID=$($t.Sensor_ID)")
        Log ("    FanTable_Data = [$($t.FanTable_Data -join ', ')]")
        Log ("    SensorTable_Data = [$($t.SensorTable_Data -join ', ')]")
        Log "    CurrentFanMaxSpeed = $($t.CurrentFanMaxSpeed)"
        Log "    CurrentFanMinSpeed = $($t.CurrentFanMinSpeed)"
    }
} catch {
    Log "  ERROR reading LENOVO_FAN_TABLE_DATA: $_"
}
Log ""

# ============================================================
# SECTION 2: Fix probe-wmi-methods.ps1 bug -- properly test Fan_Get_Table
# The original probe accessed .ReturnValue before dumping properties,
# which crashed before we could see the actual output.
# ============================================================
Log "=== SECTION 2: Fan_Get_Table (Fixed -- No .ReturnValue Access) ==="
Log ""
$pairs = @(
    @{ FanId = 0; SensorId = 3 },
    @{ FanId = 1; SensorId = 4 },
    @{ FanId = 0; SensorId = 0 }
)
foreach ($pair in $pairs) {
    $fid = $pair.FanId
    $sid = $pair.SensorId
    Log ("--- Fan_Get_Table(FanID=$fid, SensorID=$sid) ---")
    try {
        $fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD
        $result = $fm.Fan_Get_Table($fid, $sid)
        Log "  All properties:"
        DumpProperties $result "    "
    } catch {
        Log "  ERROR: $_"
    }
    Log ""
}

# ============================================================
# SECTION 3: SmartFanMode -- Switch to Custom
# Fan_Set_Table only works in Custom mode (mode value 3 on most firmware).
# We try common values and probe to find the right one.
# ============================================================
Log "=== SECTION 3: SmartFanMode Switch ==="
Log ""

$customModeSet = $false
if ($null -ne $originalSmartFanMode) {
    Log "Original SmartFanMode: $originalSmartFanMode"
    # Try setting to Custom mode (typically 3)
    Log "--- Attempting SetSmartFanMode(3) ---"
    try {
        $gz = Get-WmiObject -Namespace root/WMI -Class LENOVO_GAMEZONE_DATA
        $gz.SetSmartFanMode(3)
        Log "  SetSmartFanMode(3) called successfully (no error)"
        $customModeSet = $true  # Set immediately so restore runs even if verification fails

        # Verify it took effect
        Start-Sleep -Seconds 1
        $gz = Get-WmiObject -Namespace root/WMI -Class LENOVO_GAMEZONE_DATA
        $result = $gz.GetSmartFanMode()
        Log "  Verification -- GetSmartFanMode() properties:"
        DumpProperties $result "    "
    } catch {
        Log "  ERROR: $_"
    }
} else {
    Log "Skipping SmartFanMode switch -- could not read original mode"
}
Log ""

# ============================================================
# SECTION 4: Fan_Set_Table -- The actual probe
# Constructs a 64-byte buffer with safe values and calls the method.
# ============================================================
Log "=== SECTION 4: Fan_Set_Table Probe ==="
Log ""

# Construct the 64-byte buffer
# Layout:
#   Byte 0:    FSTM = 1 (write mode)
#   Byte 1:    FSID = 0 (sensor set ID)
#   Bytes 2-5: FSTL = 0x00000000 (uint32 LE)
#   Bytes 6-25: FSS0-FSS9 = 10 x uint16 LE speed step values
#   Bytes 26-63: zero padding
#
# FSS values: we use the LLT V2 safety minimum [1,1,1,1,1,1,1,1,3,5]
# These are indices (0-9) into the FanSpeeds array from LENOVO_FAN_TABLE_DATA.
# On 82RG: index 1 = 1800 RPM, index 3 = 2200 RPM, index 5 = 3400 RPM

$fssValues = @(1, 1, 1, 1, 1, 1, 1, 1, 3, 5)
Log ("FSS values (0-10 index scale): [$($fssValues -join ', ')]")
Log "Expected RPM mapping (based on FanTable_Data):"
if ($baselineTables.Count -gt 0) {
    $fanSpeeds = $baselineTables[0].FanTableData
    foreach ($i in 0..9) {
        $idx = $fssValues[$i]
        if ($idx -lt $fanSpeeds.Count) {
            Log "  FSS${i} = index $idx -> $($fanSpeeds[$idx]) RPM"
        } else {
            Log "  FSS${i} = index $idx -> (out of range)"
        }
    }
}
Log ""

# Build the byte array
[byte[]]$fanTableBytes = New-Object byte[] 64
$fanTableBytes[0] = 1   # FSTM
$fanTableBytes[1] = 0   # FSID
# Bytes 2-5: FSTL = 0 (already zero)
for ($i = 0; $i -lt 10; $i++) {
    $offset = 6 + ($i * 2)
    $val = [uint16]$fssValues[$i]
    $fanTableBytes[$offset]     = [byte]($val -band 0xFF)       # LE low byte
    $fanTableBytes[$offset + 1] = [byte](($val -shr 8) -band 0xFF)  # LE high byte
}

Log "Constructed 64-byte buffer:"
Log "  Hex: $(($fanTableBytes | ForEach-Object { $_.ToString('X2') }) -join ' ')"
Log ("  Dec: [$($fanTableBytes -join ', ')]")
Log ""

# --- Attempt 1: Call Fan_Set_Table ---
Log "--- Calling Fan_Set_Table (attempt 1: with SmartFanMode=$(if ($customModeSet) {'Custom'} else {'unknown'})) ---"
$setTableSuccess = $false
try {
    $fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD
    $result = $fm.Fan_Set_Table($fanTableBytes)
    Log "  Fan_Set_Table() returned without error!"
    if ($null -ne $result) {
        Log "  Result properties:"
        DumpProperties $result "    "
    } else {
        Log "  Result: null (void return -- expected for methods with no output parameters)"
    }
    $setTableSuccess = $true
} catch {
    Log "  ERROR: $_"
    Log "  Exception type: $($_.Exception.GetType().FullName)"
    if ($_.Exception.InnerException) {
        Log "  Inner exception: $($_.Exception.InnerException.Message)"
    }
}
Log ""

# ============================================================
# SECTION 5: Verify -- Check if Fan_Set_Table had any effect
# ============================================================
Log "=== SECTION 5: Post-Write Verification ==="
Log ""

# Wait a moment for EC to process
Start-Sleep -Seconds 2

# Re-read fan speeds
Log "--- Post-write Fan Speeds ---"
foreach ($fid in @(0, 1)) {
    try {
        $fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD
        $speed = ($fm.Fan_GetCurrentFanSpeed($fid)).CurrentFanSpeed
        Log "  Fan $fid speed: $speed RPM"
    } catch {
        Log "  Fan $fid speed: ERROR $_"
    }
}
Log ""

# Re-read LENOVO_FAN_TABLE_DATA
Log "--- Post-write LENOVO_FAN_TABLE_DATA ---"
try {
    $tables = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_TABLE_DATA
    $idx = 0
    foreach ($t in $tables) {
        Log ("  Entry: Fan_Id=$($t.Fan_Id), Sensor_ID=$($t.Sensor_ID)")
        Log ("    FanTable_Data = [$($t.FanTable_Data -join ', ')]")
        Log ("    SensorTable_Data = [$($t.SensorTable_Data -join ', ')]")

        # Compare with baseline
        if ($idx -lt $baselineTables.Count) {
            $baseline = $baselineTables[$idx].FanTableData
            $current = $t.FanTable_Data
            $changed = $false
            for ($i = 0; $i -lt [Math]::Min($baseline.Count, $current.Count); $i++) {
                if ($baseline[$i] -ne $current[$i]) {
                    $changed = $true
                    Log "    CHANGED at index ${i}: $($baseline[$i]) -> $($current[$i])"
                }
            }
            if (-not $changed) {
                Log "    (no change from baseline)"
            }
        }
        $idx++
    }
} catch {
    Log "  ERROR reading LENOVO_FAN_TABLE_DATA: $_"
}
Log ""

# Re-read Fan_Get_Table to see if the method now returns different data
Log ("--- Post-write Fan_Get_Table (FanID=0, SensorID=3) ---")
try {
    $fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD
    $result = $fm.Fan_Get_Table(0, 3)
    Log "  All properties:"
    DumpProperties $result "    "
} catch {
    Log "  ERROR: $_"
}
Log ""

# ============================================================
# SECTION 6: Restore -- Return to original SmartFanMode
# ============================================================
Log "=== SECTION 6: Restore ==="
Log ""

$restoreSucceeded = $false
if ($null -ne $originalSmartFanMode -and $customModeSet) {
    Log "--- Restoring SmartFanMode to $originalSmartFanMode ---"
    try {
        $gz = Get-WmiObject -Namespace root/WMI -Class LENOVO_GAMEZONE_DATA
        $gz.SetSmartFanMode($originalSmartFanMode)
        Log "  SetSmartFanMode($originalSmartFanMode) called successfully"
        $restoreSucceeded = $true

        # Verify
        Start-Sleep -Seconds 1
        $gz = Get-WmiObject -Namespace root/WMI -Class LENOVO_GAMEZONE_DATA
        $result = $gz.GetSmartFanMode()
        Log "  Verification -- GetSmartFanMode() properties:"
        DumpProperties $result "    "
    } catch {
        Log "  ERROR restoring SmartFanMode: $_"
        Log "  WARNING: System may be stuck in Custom fan mode!"
    }
} else {
    Log "  No SmartFanMode restoration needed"
}
Log ""

# ============================================================
# SECTION 7: Summary
# ============================================================
Log "=== SECTION 7: Summary ==="
Log ""
Log "SmartFanMode read:     $(if ($null -ne $originalSmartFanMode) { 'YES (value: ' + $originalSmartFanMode + ')' } else { 'FAILED' })"
Log "SmartFanMode set:      $(if ($customModeSet) { 'YES' } else { 'NOT ATTEMPTED or FAILED' })"
Log "Fan_Set_Table call:    $(if ($setTableSuccess) { 'SUCCESS (no error)' } else { 'FAILED' })"
Log "SmartFanMode restored: $(if ($restoreSucceeded) { 'YES' } elseif ($customModeSet) { 'FAILED -- check manually!' } else { 'N/A' })"
Log ""
Log "NEXT STEPS:"
if ($setTableSuccess) {
    Log "  Fan_Set_Table did not error. Check SECTION 5 above to see if"
    Log "  LENOVO_FAN_TABLE_DATA values changed. If they changed, the"
    Log "  method is functional on this firmware. Proceed to Phase 1."
    Log ""
    Log "  If values did NOT change, the method may be a firmware stub"
    Log "  that silently ignores input. The feature cannot be implemented"
    Log "  on this hardware."
} else {
    Log "  Fan_Set_Table returned an error. Check the error message in"
    Log "  SECTION 4. If it is an ACPI/method error, the method is a"
    Log "  firmware stub. The feature cannot be implemented on this hardware."
}
Log ""
Log "=== Probe complete ==="
