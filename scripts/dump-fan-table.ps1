#Requires -RunAsAdministrator
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$logFile = Join-Path $PSScriptRoot 'dump-fan-table.log'

function Log($message) {
    $line = "[{0}] {1}" -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $message
    Write-Host $line
    $line | Out-File -FilePath $logFile -Append -Encoding utf8
}

# Truncate log file
"" | Out-File -FilePath $logFile -Encoding utf8

Log "=== LENOVO_FAN_TABLE_DATA Full Dump ==="
Log "Machine: $env:COMPUTERNAME"
Log ""

# --- Dump all table entries with all properties ---
$tables = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_TABLE_DATA

foreach ($t in $tables) {
    Log "--- Entry: $($t.InstanceName) ---"
    $t.Properties | ForEach-Object {
        $val = $_.Value
        if ($val -is [System.Array]) {
            $val = "[$($val -join ', ')]"
        }
        Log "  $($_.Name) = $val"
    }
    Log ""
}

# --- Dump LENOVO_FAN_METHOD class methods ---
Log "=== LENOVO_FAN_METHOD Methods ==="
$fmClass = [wmiclass]'root\WMI:LENOVO_FAN_METHOD'
$fmClass.Methods | ForEach-Object {
    $method = $_
    Log "--- Method: $($method.Name) ---"

    if ($method.InParameters) {
        Log "  Input parameters:"
        $method.InParameters.Properties | ForEach-Object {
            Log "    $($_.Name) : $($_.Type) (Qualifiers: $($_.Qualifiers | ForEach-Object { "$($_.Name)=$($_.Value)" }))"
        }
    } else {
        Log "  Input parameters: (none)"
    }

    if ($method.OutParameters) {
        Log "  Output parameters:"
        $method.OutParameters.Properties | ForEach-Object {
            Log "    $($_.Name) : $($_.Type) (Qualifiers: $($_.Qualifiers | ForEach-Object { "$($_.Name)=$($_.Value)" }))"
        }
    } else {
        Log "  Output parameters: (none)"
    }
    Log ""
}

Log "=== Dump complete ==="
