# LenovoWmi.psm1 -- shared scaffolding for fancontrol's Windows tooling.
#
# Import from a tool in this directory with:
#   Import-Module (Join-Path $PSScriptRoot 'LenovoWmi.psm1') -Force
#
# ASCII only. Windows PowerShell 5.1 cannot read UTF-8 without a BOM, so no em
# dashes or other non-ASCII characters anywhere in this directory.

Set-StrictMode -Version Latest

$script:LogPath = $null

# ---------------------------------------------------------------------------
# Logging
# ---------------------------------------------------------------------------

function Start-ToolLog {
    <#
    .SYNOPSIS
    Begin a tool log, truncating any previous run, and write a standard header.
    .PARAMETER Path
    Log file path. Callers normally pass a path next to the invoking script.
    .PARAMETER Title
    Human-readable tool name for the header.
    .PARAMETER ReadOnly
    State in the header that the tool invokes no setters. Use this for probes so
    the log itself records that nothing was mutated.
    #>
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Title,
        [switch]$ReadOnly
    )
    $script:LogPath = $Path
    "" | Out-File -FilePath $Path -Encoding utf8
    Write-ToolLog ("=== " + $Title + " ===")
    Write-ToolLog "Machine: $env:COMPUTERNAME"
    Write-ToolLog "Date: $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"
    if ($ReadOnly) {
        Write-ToolLog "READ-ONLY -- no setters invoked, no fan or power mode changes."
    }
    Write-ToolLog ""
}

function Write-ToolLog {
    <#
    .SYNOPSIS
    Write one timestamped line to the host and to the active tool log.
    .DESCRIPTION
    Wrap arguments containing commas in parentheses at the call site --
    Write-ToolLog ("a, b") -- or PowerShell parses them as multiple arguments.
    #>
    param([string]$Message = "")
    $line = "[{0}] {1}" -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $Message
    Write-Host $line
    if ($script:LogPath) {
        $line | Out-File -FilePath $script:LogPath -Append -Encoding utf8
    }
}

function Write-WmiProperties {
    <#
    .SYNOPSIS
    Log every property of a WMI object or method result, flattening arrays.
    #>
    param(
        [Parameter(Mandatory)]$InputObject,
        [string]$Indent = "    "
    )
    if ($null -eq $InputObject) {
        Write-ToolLog ($Indent + "(null)")
        return
    }
    $InputObject.Properties | ForEach-Object {
        $value = $_.Value
        if ($value -is [System.Array]) {
            $value = "[$($value -join ', ')]"
        }
        Write-ToolLog ($Indent + $_.Name + " = " + $value)
    }
}

# ---------------------------------------------------------------------------
# Environment
# ---------------------------------------------------------------------------

function Get-WmiPropertyOrNull {
    <#
    .SYNOPSIS
    Read a WMI property by name, returning $null when it does not exist.
    .DESCRIPTION
    Required because tools here run under Set-StrictMode -Version Latest, where
    $object.MissingProperty is a terminating error rather than $null. Firmware
    varies in which properties it exposes -- the 82RG's LENOVO_FAN_TABLE_DATA has
    CurrentFanMaxSpeed but no DefaultFanMaxSpeed -- so probing for a property that
    may be absent must not abort the run.
    #>
    param(
        [Parameter(Mandatory)][AllowNull()]$InputObject,
        [Parameter(Mandatory)][string]$Name
    )
    if ($null -eq $InputObject) { return $null }
    $property = $InputObject.Properties | Where-Object { $_.Name -eq $Name }
    if ($null -eq $property) { return $null }
    return $property.Value
}

function Test-Elevated {
    <#
    .SYNOPSIS
    True when the current process can reach the root\WMI namespace.
    .DESCRIPTION
    The Lenovo WMI classes live in root\WMI and return "access denied" without
    elevation. Calling this first turns a confusing mid-run failure into a clear
    up-front message.
    #>
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-LenovoBiosVersion {
    <#
    .SYNOPSIS
    Read and parse the BIOS version the way LenovoLegionToolkit does.
    .DESCRIPTION
    Returns an object with Raw, Prefix and Version. LLT reads the raw string from
    the registry rather than WMI, takes the prefix as ^[A-Z0-9]{4} and the
    version as the first [0-9]{2} match. Its BiosVersion.IsLowerThan returns
    false outright when two prefixes differ, so a prefix that appears in no
    blocklist can never be "lower than" a blocklisted entry.
    #>
    $raw = (Get-ItemProperty 'HKLM:\HARDWARE\DESCRIPTION\System\BIOS').BIOSVersion
    if ($raw) { $raw = $raw.Trim() }
    [pscustomobject]@{
        Raw     = $raw
        Prefix  = [regex]::Match($raw, '^[A-Z0-9]{4}').Value
        Version = [regex]::Match($raw, '[0-9]{2}').Value
    }
}

# ---------------------------------------------------------------------------
# WMI access
# ---------------------------------------------------------------------------

function Get-LenovoWmiClass {
    <#
    .SYNOPSIS
    Fetch instances of a root\WMI class, logging a clear reason on failure.
    .PARAMETER ClassName
    e.g. LENOVO_GAMEZONE_DATA, LENOVO_FAN_METHOD, LENOVO_FAN_TABLE_DATA.
    .PARAMETER Single
    Return the first instance instead of the array.
    #>
    param(
        [Parameter(Mandatory)][string]$ClassName,
        [switch]$Single
    )
    try {
        $instances = @(Get-WmiObject -Namespace root/WMI -Class $ClassName -ErrorAction Stop)
        Write-ToolLog ("  " + $ClassName + " found, instances: " + $instances.Count)
        if ($Single) { return $instances[0] }
        return $instances
    } catch {
        Write-ToolLog ("  ERROR: " + $ClassName + " not available: " + $_.Exception.Message)
        return $null
    }
}

function Invoke-LenovoWmiMethod {
    <#
    .SYNOPSIS
    Invoke a WMI method and return one named output property.
    .DESCRIPTION
    Property is mandatory by design. These methods return their result in a
    named property -- Data, Value, Status, Version, CurrentFanSpeed -- and NOT in
    .ReturnValue. Earlier probe scripts in this repo read .ReturnValue and
    errored, so requiring the name here makes that mistake unrepresentable.

    Prefers GetMethodParameters plus InvokeMethod, which passes named arguments
    correctly and returns an object carrying the output properties. Falls back to
    PowerShell's adapted call for parameterless methods where that path is not
    available.
    .PARAMETER Arguments
    Hashtable of WMI parameter name to value, e.g. @{ IDs = 0x00070000 }. Used by
    the GetMethodParameters path.
    .PARAMETER PositionalArguments
    Same values in declaration order, for the adapted-call fallback. Supply both
    for any method taking arguments: GetMethodParameters is not available for
    every method on every firmware -- LENOVO_OTHER_METHOD.GetFeatureValue on the
    82RG is one that fails -- and without positional values such a method cannot
    be called at all.
    .PARAMETER AsObject
    Return the whole output object rather than a single property.
    #>
    param(
        # AllowNull is required, not decorative: Mandatory alone rejects $null at
        # binding time, so the guard below would never run and a missing WMI class
        # would become a terminating error under $ErrorActionPreference = 'Stop'.
        # Callers pass the result of Get-LenovoWmiClass straight in, which is null
        # when the class is unavailable.
        [Parameter(Mandatory)][AllowNull()]$WmiObject,
        [Parameter(Mandatory)][string]$Method,
        [hashtable]$Arguments = @{},
        [object[]]$PositionalArguments = @(),
        [Parameter(Mandatory)][string]$Property,
        [switch]$AsObject
    )
    if ($null -eq $WmiObject) {
        Write-ToolLog ("  SKIP " + $Method + " -- no WMI object")
        return $null
    }
    try {
        $result = $null
        $inParams = $null
        try {
            $inParams = $WmiObject.GetMethodParameters($Method)
        } catch {
            $inParams = $null
        }

        if ($null -ne $inParams) {
            foreach ($key in $Arguments.Keys) {
                $inParams[$key] = $Arguments[$key]
            }
            $result = $WmiObject.InvokeMethod($Method, $inParams, $null)
        } else {
            # GetMethodParameters is unavailable for some methods on some
            # firmware. Fall back to PowerShell's adapted call, which needs the
            # arguments positionally.
            $positional = $PositionalArguments
            if ($positional.Count -eq 0 -and $Arguments.Count -eq 1) {
                $positional = @($Arguments.Values)[0]
                $positional = @($positional)
            }
            if ($positional.Count -ne $Arguments.Count) {
                throw ("cannot call " + $Method + " -- GetMethodParameters unavailable and no PositionalArguments supplied")
            }
            Write-ToolLog ("  (GetMethodParameters unavailable for " + $Method + ", using adapted call)")
            $result = $WmiObject.$Method.Invoke($positional)
        }

        Write-ToolLog ("  " + $Method + "() output:")
        Write-WmiProperties $result

        if ($AsObject) { return $result }

        $value = $result.$Property
        if ($null -eq $value) {
            Write-ToolLog ("  WARNING: " + $Method + " returned no '" + $Property + "' property")
        }
        return $value
    } catch {
        Write-ToolLog ("  ERROR calling " + $Method + ": " + $_.Exception.Message)
        return $null
    }
}

Export-ModuleMember -Function Start-ToolLog, Write-ToolLog, Write-WmiProperties,
    Get-WmiPropertyOrNull, Test-Elevated, Get-LenovoBiosVersion, Get-LenovoWmiClass,
    Invoke-LenovoWmiMethod
