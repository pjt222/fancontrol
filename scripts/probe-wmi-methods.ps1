#Requires -RunAsAdministrator
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$logFile = Join-Path $PSScriptRoot 'probe-wmi-methods.log'

function Log($message) {
    $line = "[{0}] {1}" -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $message
    Write-Host $line
    $line | Out-File -FilePath $logFile -Append -Encoding utf8
}

# Truncate log file
"" | Out-File -FilePath $logFile -Encoding utf8

Log "=== WMI Method Probe ==="
Log "Machine: $env:COMPUTERNAME"
Log ""

$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD

# --- Fan_Get_FullSpeed ---
Log "--- Fan_Get_FullSpeed() ---"
try {
    $result = $fm.Fan_Get_FullSpeed()
    Log "  Status: $($result.Status)"
    Log "  ReturnValue: $($result.ReturnValue)"
    Log "  All properties:"
    $result.Properties | ForEach-Object {
        Log "    $($_.Name) = $($_.Value)"
    }
} catch {
    Log "  ERROR: $_"
}
Log ""

# --- Fan_Get_Table for known fan/sensor pairs ---
$pairs = @(
    @{ FanId = 0; SensorId = 0 },
    @{ FanId = 0; SensorId = 3 },
    @{ FanId = 1; SensorId = 4 }
)

foreach ($pair in $pairs) {
    $fid = $pair.FanId
    $sid = $pair.SensorId
    Log "--- Fan_Get_Table(FanID=$fid, SensorID=$sid) ---"
    try {
        $result = $fm.Fan_Get_Table($fid, $sid)
        Log "  ReturnValue: $($result.ReturnValue)"
        Log "  All properties:"
        $result.Properties | ForEach-Object {
            $val = $_.Value
            if ($val -is [System.Array]) {
                $val = "[$($val -join ', ')]"
            }
            Log "    $($_.Name) = $val"
        }
    } catch {
        Log "  ERROR: $_"
    }
    Log ""
}

# --- Fan_Get_MaxSpeed for each fan ---
foreach ($fid in @(0, 1)) {
    Log "--- Fan_Get_MaxSpeed(Fan_ID=$fid) ---"
    try {
        $result = $fm.Fan_Get_MaxSpeed($fid)
        Log "  ReturnValue: $($result.ReturnValue)"
        Log "  All properties:"
        $result.Properties | ForEach-Object {
            $val = $_.Value
            if ($val -is [System.Array]) {
                $val = "[$($val -join ', ')]"
            }
            Log "    $($_.Name) = $val"
        }
    } catch {
        Log "  ERROR: $_"
    }
    Log ""
}

# --- Current speeds and temps for reference ---
Log "--- Current Fan Speeds & Temps ---"
foreach ($fid in @(0, 1)) {
    try {
        $speed = ($fm.Fan_GetCurrentFanSpeed($fid)).CurrentFanSpeed
        Log "  Fan $fid speed: $speed RPM"
    } catch {
        Log "  Fan $fid speed: ERROR $_"
    }
}
foreach ($sid in @(0, 3, 4)) {
    try {
        $temp = ($fm.Fan_GetCurrentSensorTemperature($sid)).CurrentSensorTemperature
        Log "  Sensor $sid temp: $temp C"
    } catch {
        Log "  Sensor $sid temp: ERROR $_"
    }
}

Log ""
Log "=== Probe complete ==="
