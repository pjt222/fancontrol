# LenovoWmi.psm1 -- shared scaffolding for fancontrol's Windows tooling.
#
# Import from a tool in this directory with:
#   Import-Module (Join-Path $PSScriptRoot 'LenovoWmi.psm1') -Force
#
# ASCII only. Windows PowerShell 5.1 cannot read UTF-8 without a BOM, so no em
# dashes or other non-ASCII characters anywhere in this directory.

Set-StrictMode -Version Latest

$script:LogPath = $null
$script:ReadOnlyMode = $false

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
    Declare that the tool invokes no setters. This is enforced, not merely
    recorded: it arms a module flag that makes Invoke-LenovoWmiMethod refuse any
    method whose name begins Set or Fan_Set. A tool that claims to be read-only
    therefore cannot quietly become one that writes to the EC.
    #>
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Title,
        [switch]$ReadOnly
    )
    $script:LogPath = $Path
    $script:ReadOnlyMode = [bool]$ReadOnly
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
        # AllowNull for the same reason as in Invoke-LenovoWmiMethod: Mandatory
        # alone rejects $null at binding time, so the "(null)" branch below
        # never ran. A setter with no output object, Fan_Set_FullSpeed among
        # them, returns $null from InvokeMethod, and dumping that threw
        # "Das Argument kann nicht an den Parameter "InputObject" gebunden
        # werden, da es NULL ist" inside Invoke-LenovoWmiMethod's try, which
        # then reported "ERROR calling Fan_Set_FullSpeed" for a call that had
        # taken effect (read back True; measured 2026-09-03 13:58).
        [Parameter(Mandatory)][AllowNull()]$InputObject,
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
    True when the current process is running as Administrator.
    .DESCRIPTION
    A proxy for "can reach root\WMI", not a direct test of it. The Lenovo classes
    live in root\WMI and return "access denied" without elevation, so checking
    the Administrator role up front turns a confusing mid-run failure into a clear
    message. It does not prove access -- policy could deny an elevated process.
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
    # Fully guarded: this runs before the first try block in a calling tool, and
    # with $ErrorActionPreference = 'Stop' any throw here kills the run just after
    # the log header is written. A missing registry value throws under StrictMode,
    # and [regex]::Match($null, ...) throws ArgumentNullException -- so guarding
    # only the Trim, as an earlier version did, left both regex calls exposed.
    $raw = $null
    try {
        $raw = (Get-ItemProperty 'HKLM:\HARDWARE\DESCRIPTION\System\BIOS' -ErrorAction Stop).BIOSVersion
    } catch {
        Write-ToolLog ("  WARNING: could not read BIOSVersion from the registry: " + $_.Exception.Message)
    }

    if (-not $raw) {
        # Prefix and Version are empty strings, NOT $null, while Raw stays $null as
        # the "unavailable" signal. This is load-bearing: consumers look the prefix
        # up in a hashtable, and Hashtable.ContainsKey($null) throws
        # ArgumentNullException rather than returning false, whereas
        # ContainsKey('') safely misses. Returning nulls here moved the crash from
        # this function into its caller -- in exactly the missing-BIOSVersion case
        # this guard exists for.
        #
        # Empty also matches upstream on both halves of the asymmetry it produces.
        # LLT gates V1 with `affectedBiosVersions.Any(bv => biosVersion?.IsLowerThan(bv) ?? false)`,
        # so a wholly unknown BIOS coalesces to false and *allows* V1 -- which is
        # what an unmatched prefix gives. A known-blocklisted prefix with an
        # unparseable version still blocks, because IsLowerThan returns true when
        # either version is null (Structs.cs:69-78). Unknown prefix allows, unknown
        # version blocks: deliberate, and upstream's.
        Write-ToolLog "  WARNING: BIOS version unavailable; prefix and version reported as empty"
        return [pscustomobject]@{ Raw = $null; Prefix = ''; Version = '' }
    }

    $raw = $raw.Trim()
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
        # Distinguish "class absent" from "class present but empty". Indexing [0]
        # on an empty array is a StrictMode error that the catch below would
        # otherwise report as "not available", which is a different and misleading
        # diagnosis.
        if ($instances.Count -eq 0) {
            Write-ToolLog ("  WARNING: " + $ClassName + " exists but has no instances")
            return $null
        }
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
    for any method taking arguments, because GetMethodParameters is not available
    for every method on every firmware.

    Two distinct failures look alike here and only one is recoverable. If the
    method EXISTS but GetMethodParameters is unsupported for it, this fallback
    works. If the method is ABSENT, neither path can help -- $WmiObject.$Method
    throws before .Invoke is reached. LENOVO_OTHER_METHOD.GetFeatureValue on the
    82RG is the second kind, not the first, so it is not an example of this
    fallback succeeding.

    Caveat when it does fire: the GetMethodParameters path assigns through
    $inParams[$key] and coerces to the declared CIM type, whereas this path
    relies on PowerShell marshalling raw values in positional order with no
    validation. An Int32 literal passed where the method declares UInt32 can
    mis-bind rather than error. Prefer the named path wherever it is available.
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
    # Deliberately outside the try below, so this propagates as a terminating
    # error rather than being logged and swallowed. A read-only tool reaching a
    # setter is a bug in the tool, not a firmware quirk to tolerate.
    if ($script:ReadOnlyMode -and $Method -match '^(Set|Fan_Set)') {
        throw ("refusing to call " + $Method + " -- this tool declared itself read-only via Start-ToolLog -ReadOnly")
    }
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
                $positional = @(@($Arguments.Values)[0])
            }
            # Guard on "arguments are needed but none are available positionally".
            # Comparing counts against $Arguments would reject a caller who
            # supplied only -PositionalArguments -- which is the supported shape --
            # and would report the opposite of what happened.
            if ($positional.Count -eq 0 -and $Arguments.Count -gt 0) {
                throw ("cannot call " + $Method + " -- GetMethodParameters unavailable and no PositionalArguments supplied")
            }
            Write-ToolLog ("  (GetMethodParameters unavailable for " + $Method + ", using adapted call)")
            $result = $WmiObject.$Method.Invoke($positional)
        }

        Write-ToolLog ("  " + $Method + "() output:")
        Write-WmiProperties $result

        if ($AsObject) { return $result }

        # Read via Get-WmiPropertyOrNull so an absent property is reported as such
        # rather than surfacing as "ERROR calling <method>" from the catch below.
        # Under Set-StrictMode a direct $result.$Property access on a missing
        # property throws, which would misattribute the failure to the call.
        $value = Get-WmiPropertyOrNull -InputObject $result -Name $Property
        if ($null -eq $value) {
            Write-ToolLog ("  WARNING: " + $Method + " succeeded but exposes no '" + $Property + "' property")
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
