# tools/

Reusable Windows tooling for fancontrol. Unlike `scripts/`, which holds one-off
probe scripts kept for their logs, everything here is meant to be run repeatedly
and built on.

## Layout

| File | Purpose |
|---|---|
| `LenovoWmi.psm1` | Shared module: logging, elevation check, BIOS parsing, root\WMI access and method invocation |
| `Get-GodModeVersion.ps1` | Determines whether this machine takes LLT's GodMode V1 or V2 fan-curve rules, and whether the fan max-speed properties are populated (issue #25) |
| `Invoke-FanTableLoadTest.ps1` | Holds the CPU above the lowest curve threshold and writes a curve through `fancontrol.exe`, to decide whether `Fan_Set_Table` reaches the EC (issue #10). **Writes to the EC** |
| `Reset-LenovoFanState.ps1` | Leaves a safe curve loaded and a chosen SmartFanMode selected. Run after any session that wrote an experimental curve. **Writes to the EC** |
| `Get-LenovoLedSurface.ps1` | Enumerates the `LENOVO_*` classes and the lighting surface, for the power-button LED indicator work. Read-only |
| `Get-LenovoLighting.ps1` | Invokes `Get_Lighting_Current_Status` per `Lighting_Id` across a SmartFanMode sweep. Measured 2026-09-02 (#44): `Lighting_Id 4 -> Current_State_Type` tracks the mode, 0/1/2/3 = blue/white/red/multi by the operator's report. After each switch it asks the operator what colour the button shows and logs the answer beside the firmware's state index, so one attended run yields the index-to-colour mapping (`-NoPrompt` for unattended runs). The prompt holds the selected mode until answered, so answer it. Ctrl+C is expected to run the mode restore but is unmeasured at the prompt; closing the window skips it. Writes a safe curve first, so entering Custom mid-sweep is not a fans-off trap. **Writes to the EC** |
| `Watch-LenovoLightingVsMode.ps1` | Samples `Lighting_Id 4`'s state index beside `GetSmartFanMode`, targeting one sample a second (the log carries the measured time), while the operator changes things by paths other than `SetSmartFanMode` (AC unplug in Performance, sleep and wake, Fn+Q, Fn+Space, whatever Vantage offers), to learn whether the two can ever disagree (#44, the indicator's source). Attended: each phase is Enter, a sampling window, then the colour the operator sees. Never calls `SetSmartFanMode`, enforced by `tests/lighting_setter_forbidden.rs`. Read-only unless `-IncludeFullSpeed`, which adds one `Fan_Set_FullSpeed(1)` window and disables it again; if the disable does not read back False the tool says so and what is owed. Otherwise the tool owes nothing after a run, having selected no mode and written no table; the operator's Fn+Q does move the mode, and the tool reports where it left the machine and warns when that is Custom |

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

**Target: Windows PowerShell 5.1**, measured as `5.1.26100.9168` (Desktop edition,
CLR 4.0.30319) on the test machine, 2026-08-16. That is what `powershell.exe`
resolves to, and `powershell.exe` is what both these tools and
`src/platform/lenovo.rs` invoke.

PowerShell Core **7.6.4 is also installed** as `pwsh.exe`. It is not used, and
checking `pwsh --version` is a good way to talk yourself into a 7.x-only feature
that then fails under the interpreter the code actually launches. Write for 5.1
unless you have changed the binary being invoked.

These conventions exist because each one has already cost a debugging session:

- **ASCII only.** Windows PowerShell 5.1 assumes ANSI for a BOM-less file, so an
  em dash or curly quote in a `.ps1` is mangled and can break parsing. Use `--`.
  Note the logs themselves are BOM'd: `-Encoding utf8` on 5.1 writes UTF-8 *with*
  a BOM. Harmless, but it is why the log's first line looks blank.
- **Read the named output property, never `.ReturnValue`.** Lenovo WMI methods
  return their result in a named property: `Data`, `Value`, `Status`, `Version`,
  `CurrentFanSpeed`. Earlier probes in `scripts/` read `.ReturnValue` and errored.
  `Invoke-LenovoWmiMethod` makes `-Property` mandatory so this cannot recur.
- **Wrap comma-containing arguments in parentheses.** `Write-ToolLog ("a, b")`,
  otherwise PowerShell parses them as separate arguments.
- **`(...) -join ' '` rather than `Join-String`,** which needs PowerShell 6.2+.
- **`${var}:` before a colon in a string,** since `$var:` parses as a
  drive-qualified variable.
- **`Get-WmiObject`, not `Get-CimInstance`,** for the pattern used here.
  `Invoke-CimMethod` *does* invoke methods on `root\WMI`; the accurate reason is
  narrower: `Get-CimInstance` returns inert objects with no adapted methods, so
  the `GetMethodParameters` / `$obj.Method()` approach these tools use needs
  `Get-WmiObject`.
- **Pass `-ReadOnly` to `Start-ToolLog` when a tool only reads.** It is enforced,
  not just recorded: it arms a module flag that makes `Invoke-LenovoWmiMethod`
  throw on any method named `Set*` or `Fan_Set*`, so a read-only tool cannot
  quietly grow a write. Anything that writes a fan table or changes a power mode
  must not pass it.

## Writing a new tool

Do not add `#Requires -RunAsAdministrator`: it refuses to launch the script, so
nothing reaches the log and the `Test-Elevated` block becomes dead code. Let the
tool start its log, record why it stopped, and exit.

```powershell
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
the risk in its help block. Three tools here write: `Invoke-FanTableLoadTest.ps1`,
`Reset-LenovoFanState.ps1` and `Get-LenovoLighting.ps1`;
`Watch-LenovoLightingVsMode.ps1` writes only behind `-IncludeFullSpeed`. `Invoke-FanTableLoadTest.ps1` shows the shape
the next one should copy: a curve that can only ask for *more* cooling than the default, an abort
lever on `Fan_Set_FullSpeed(1)` which overrides the curve and so does not depend
on the mechanism under test, and restoration of the original `SmartFanMode` in a
`finally` block so that Ctrl-C does not leave the machine in Custom mode.

Measuring a curve write requires load. Below the lowest threshold in the table
(58 C on the 82RG) every curve prescribes the same band, so a write and a no-op
are indistinguishable -- which is what left the March 2026 probe in `scripts/`
inconclusive. A tool that writes a curve and observes at idle has measured
nothing, however clean its log looks.

**A curve you write outlives your script.** Measured 2026-08-19: a curve written
in one run reactivated 24 minutes later on re-entering Custom mode, having
survived a switch to Performance and back. Restoring `SmartFanMode` in a
`finally` therefore hides an experimental curve rather than clearing it -- the
machine looks well-behaved in Quiet, Balanced and Performance and inherits your
experiment the moment anything selects Custom. Finish with
`Reset-LenovoFanState.ps1`. Retention across reboot and sleep/wake is unmeasured;
the older claim that curves are "lost on reboot, sleep/wake, and power-mode
change" is now known to be wrong for the power-mode case.

Known-dangerous or dead-end methods, documented in `CLAUDE.md`:
`Fan_SetCurrentFanSpeed` is silently ignored by the EC, `Fan_Set_MaxSpeed` has no
ACPI handler, and `Fan_Set_Table` is only meaningful with `SmartFanMode=Custom`.
