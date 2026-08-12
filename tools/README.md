# tools/

Reusable Windows tooling for fancontrol. Unlike `scripts/`, which holds one-off
probe scripts kept for their logs, everything here is meant to be run repeatedly
and built on.

## Layout

| File | Purpose |
|---|---|
| `LenovoWmi.psm1` | Shared module: logging, elevation check, BIOS parsing, root\WMI access and method invocation |
| `Get-GodModeVersion.ps1` | Determines whether this machine takes LLT's GodMode V1 or V2 fan-curve rules, and whether the fan max-speed properties are populated (issue #25) |

## Running

Most tools need an **elevated** PowerShell. The Lenovo classes live in the
`root\WMI` namespace, which returns `access denied` otherwise:

```powershell
# In an elevated Windows PowerShell, from the repo root
cd tools
.\Get-GodModeVersion.ps1
```

Each tool writes a timestamped log beside itself (`<ToolName>.log`, gitignored)
and returns a result object. Pass `-Json` for machine-readable output:

```powershell
.\Get-GodModeVersion.ps1 -Json > godmode.json
```

WSL can reach `Win32_*` classes through `powershell.exe` without elevation, which
is enough for model and BIOS reads, but not for anything under `root\WMI`.

## Conventions

These exist because each one has already cost a debugging session:

- **ASCII only.** Windows PowerShell 5.1 cannot read UTF-8 without a BOM, so an
  em dash or a curly quote anywhere in a `.ps1` breaks parsing. Use `--`.
- **Read the named output property, never `.ReturnValue`.** Lenovo WMI methods
  return their result in a named property: `Data`, `Value`, `Status`, `Version`,
  `CurrentFanSpeed`. Earlier probes in `scripts/` read `.ReturnValue` and errored.
  `Invoke-LenovoWmiMethod` makes `-Property` mandatory so this cannot recur.
- **Wrap comma-containing arguments in parentheses.** `Write-ToolLog ("a, b")`,
  otherwise PowerShell parses them as separate arguments.
- **`(...) -join ' '` rather than `Join-String`,** which needs PowerShell 6.2+.
- **`${var}:` before a colon in a string,** since `$var:` parses as a
  drive-qualified variable.
- **`Get-WmiObject`, not `Get-CimInstance`,** for method invocation.
- **Say so in the log when a tool is read-only.** Pass `-ReadOnly` to
  `Start-ToolLog`. Anything that writes a fan table or changes a power mode must
  not claim it.

## Writing a new tool

```powershell
#Requires -RunAsAdministrator
[CmdletBinding()]
param([string]$LogPath, [switch]$Json)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'LenovoWmi.psm1') -Force

if (-not $LogPath) { $LogPath = Join-Path $PSScriptRoot 'My-Tool.log' }
Start-ToolLog -Path $LogPath -Title 'What this does' -ReadOnly

if (-not (Test-Elevated)) { Write-ToolLog "FATAL: not elevated."; exit 1 }

$fanMethod = Get-LenovoWmiClass -ClassName 'LENOVO_FAN_METHOD' -Single
$speed = Invoke-LenovoWmiMethod -WmiObject $fanMethod -Method 'Fan_GetCurrentFanSpeed' `
    -Arguments @{ FanID = 0 } -Property 'CurrentFanSpeed'
```

Name tools `Verb-Noun.ps1` per PowerShell convention. Prefer adding a parameter
to an existing tool over copying one.

## Safety

Anything that writes to the EC belongs behind an explicit switch and must state
the risk in its help block. Known-dangerous or dead-end methods, documented in
`CLAUDE.md`: `Fan_SetCurrentFanSpeed` is silently ignored by the EC,
`Fan_Set_MaxSpeed` has no ACPI handler, and `Fan_Set_Table` is only meaningful
with `SmartFanMode=Custom`. Custom curves are volatile and lost on reboot,
sleep/wake, and power-mode change.
