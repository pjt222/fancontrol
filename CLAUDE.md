# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Fancontrol is a minimal cross-platform (Linux + Windows) application to control fan speed, written in Rust. Includes both CLI and GUI (egui/eframe).

- **Linux**: Uses sysfs/hwmon interfaces (`/sys/class/hwmon/`)
- **Windows**: Uses WMI — generic `Win32_Fan` fallback, Lenovo-specific `LENOVO_FAN_METHOD` for Legion laptops

## Build & Development

```bash
cargo build              # Debug build
cargo build --release    # Release build
cargo run                # Run the application
cargo test               # Run all tests
cargo test <test_name>   # Run a single test
cargo clippy             # Lint
cargo fmt                # Format code
cargo fmt -- --check     # Check formatting without modifying
```

**Cross-compilation** (from WSL to Windows):
```bash
rustup target add x86_64-pc-windows-gnu
sudo apt-get install mingw-w64
cargo build --release --target x86_64-pc-windows-gnu
# Binary at target/x86_64-pc-windows-gnu/release/fancontrol.exe
```

## Architecture

```
src/
├── main.rs          # Entry point, CLI dispatch, logging setup
├── cli.rs           # clap-derived CLI: list, get, set, monitor, table, set-curve, tui, gui
├── fan.rs           # Fan/FanCurve/CustomFanCurve structs, MINIMUM_STEPS, validate_custom_curve
├── config.rs        # fancontrol.json load/save; reports out-of-limit saved curves by path
├── errors.rs        # FanControlError enum (thiserror-based)
├── gui.rs           # egui/eframe GUI with worker thread
├── tui.rs           # ratatui dashboard, curve editor, enforce_safety_minimums
└── platform/
    ├── mod.rs       # FanController trait + create_controller() factory
    ├── linux.rs     # sysfs/hwmon backend
    ├── windows.rs   # Generic WMI backend (Win32_Fan) + is_lenovo() detection
    └── lenovo.rs    # Lenovo Legion backend (LENOVO_FAN_METHOD via PowerShell)
scripts/                    # One-off probes, kept for their logs
├── probe-wmi-methods.ps1   # WMI method probe (run on native Windows)
├── dump-fan-table.ps1      # Full fan table dump
├── probe-set-table.ps1     # Fan_Set_Table write probe
├── probe-wmi-methods.log   # Probe results
└── dump-fan-table.log      # Table dump results
tools/                      # Reusable tooling — prefer adding here
├── README.md               # Conventions and a template for new tools
├── LenovoWmi.psm1          # Shared module: logging, elevation, BIOS parsing, root\WMI access
└── Get-GodModeVersion.ps1  # GodMode V1/V2 detection + fan max-speed properties (#25)
```

**`scripts/` vs `tools/`**: `scripts/` holds historical one-off probes; do not
extend them. New Windows tooling goes in `tools/`, built on `LenovoWmi.psm1` so
the logging and WMI-access conventions stay in one place. See `tools/README.md`
for the conventions, each of which exists because it already cost a debugging
session — ASCII-only for PowerShell 5.1, named output properties rather than
`.ReturnValue`, and so on.

**Shell snippets in docs must work under bash *and* zsh.** Commands here are
authored in a zsh session and run by CI under bash, so a zsh-only breakage
passes every check and reaches the user. `tests/shell_portability.rs` scans the
` ```bash `/` ```sh ` fences of every tracked `.md` (and any `.sh`) and fails
`cargo test` on four hazards:

| Write this | Not this | Why |
|---|---|---|
| `--proto '=https'` | `--proto =https` | zsh reads a leading `=` as a `=command` lookup and **aborts the whole command list** |
| `--include="*.md"` | `--include=*.md` | an unmatched glob makes zsh **skip that command** while the list continues with status 0 |
| run bare, read `$?` | `${PIPESTATUS[0]}` | empty in zsh; the array is `$pipestatus`, 1-indexed |
| a `read` loop | `mapfile` / `readarray` | bash-only builtins |

To keep a snippet that is deliberately shell-specific, put
`<!-- portability-exempt: reason -->` on the line before the fence (or
`# portability-exempt: reason` in a script). State the reason — the marker is
for genuine cases, not for silencing a finding. A ` ```zsh ` fence is exempt by
declaration. The scanner does not follow symlinks, so `.claude/agents` and
`.claude/skills` are out of scope.

**Curve step limits live in one place.** `fan::MINIMUM_STEPS` (`[0,0,0,0,0,0,0,1,3,5]`,
LLT's GodMode V1 table) is the single definition of the per-step floors, and
both `validate_custom_curve` (which rejects) and the TUI's
`enforce_safety_minimums` (which repairs) read it. Do not re-state the floors
anywhere else — writing them twice is what let a doc comment and its code
disagree before #26.

The step 7 error string is quoted verbatim in `README.md` and `CHANGELOG.md` as
a breaking-change signature, and a test in `fan.rs` asserts it exactly. Rewording
it fails the build, by design.

**Saved curves are never rewritten on disk.** `fancontrol.json` is
authoritative; an out-of-limit curve is sanitized in memory and reported by
path, not corrected in the file. Rationale in the policy note above
`enforce_safety_minimums` in `src/tui.rs` (#27).

**Key pattern**: `FanController` trait in `platform/mod.rs` is the core abstraction. `create_controller()` returns `Box<dyn FanController>` using `#[cfg(target_os)]` to select the platform backend at compile time.

**Linux backend**: Scans sysfs hwmon directories, reads `fan*_input` for RPM, `fan*_label` for names, `pwm*` for duty cycle. Sets PWM by writing `pwm*_enable=1` (manual mode) then `pwm*=<value>`. Tests use `tempfile` to create fake hwmon trees.

**Windows generic backend**: Queries WMI `Win32_Fan` class via `wmi` crate. Most hardware doesn't expose fans through this class. `set_pwm` returns `NotControllable`.

**Lenovo backend**: Detected at runtime via `Win32_ComputerSystem.Manufacturer`. Single `discover()` PowerShell invocation reads fan speeds, sensor temps, table data (fan curves + RPM ranges), and full speed status. Uses `LENOVO_FAN_METHOD` and `LENOVO_FAN_TABLE_DATA` (root\WMI namespace). WMI method calls go through PowerShell subprocess since the `wmi` crate only supports queries. PWM 0=auto, 255=full speed, 1-254 maps to RPM range.

**GUI**: Worker thread communicates with egui UI via mpsc channels. Worker re-applies held PWM values each poll cycle (1.5s) to resist BIOS overrides. Full speed mode shows a red banner. Fan curves displayed in collapsible sections.

## Lenovo WMI Methods

Working on test hardware (Legion 82RG):
- `Fan_GetCurrentFanSpeed(fan_id)` → `CurrentFanSpeed: UInt16`
- `Fan_GetCurrentSensorTemperature(sensor_id)` → `CurrentSensorTemperature: UInt16`
- `Fan_Get_FullSpeed()` → `Status: Boolean` (NOT `.ReturnValue`)
- `Fan_Set_FullSpeed(bool)` — enables/disables full speed mode
- `Fan_SetCurrentFanSpeed(fan_id, rpm)` — set manual fan speed

- `Fan_Set_Table(bytes)` — **works** (measured 2026-08-19 under load, #10). The
  step scale is a 0–10 index, not a 0–100 percentage: an all-10s curve produced
  4800 RPM, exactly `CurrentFanMaxSpeed`.

Firmware stubs (return empty data): `Fan_Get_MaxSpeed`, `Fan_Get_Table`

Untested/deferred: `Fan_Set_MaxSpeed`

**`SmartFanMode` Custom is 255.** Values: 1=Quiet, 2=Balanced, 3=Performance,
255=Custom, confirmed by read-back. The March 2026 probe in `scripts/` used 3 and
called it Custom; the machine was already sitting in 3, so that probe never
changed the mode and never met `Fan_Set_Table`'s prerequisite. Do not reintroduce
3 from that log.

**Custom mode with no curve stops the fans, at any temperature.** Measured
2026-08-19: `SetSmartFanMode(255)` without a subsequent `Fan_Set_Table` held both
fans at 0 RPM across 61–67 °C under full load. Anything that switches into Custom
must guarantee a curve write lands, or restore the previous mode on failure.
`set_custom_curve` now does both: the mode switch and the table write happen in
**one** PowerShell invocation whose `finally` restores the previous mode, and a
Rust-side `CustomModeGuard` is the backstop for the subprocess failing to launch
or dying. Do not split those back into two calls — the single invocation is what
bounds the fans-off window to one process.

**The EC retains the last written curve across power-mode switches.** Measured
2026-08-19: a curve written in one run reactivated 24 minutes later on
re-entering Custom, having survived a switch to Performance and back. Two
consequences. First, "Custom with no curve" really means "Custom with whatever
was last written", which may be nothing (fresh boot), stale, or an experimental
curve from a probe — so the fans-off hazard is *latent*, sitting harmless through
Quiet/Balanced/Performance and firing when something selects Custom. Second,
restoring the mode on a failed write is non-destructive, because the mode change
does not clear the stored table. Reboot and sleep/wake retention remain
unmeasured; the "lost on reboot, sleep, or power mode change" note is now known
to be wrong for the power-mode case only.

After any session that writes an experimental curve, run
`tools/Reset-LenovoFanState.ps1` to leave a safe curve loaded. Otherwise the
next thing to select Custom mode inherits the experiment.

**Step 0 means the fan is off — settled 2026-08-19 (#18 AC-2).** With
`MINIMUM_STEPS` in force, fan 0 read 0 RPM across 17 consecutive in-band samples
from 58–62 °C under sustained load, while `restore` to mode 3 brought 2200 RPM
straight back at 63 °C. `CurrentFanMinSpeed = 1600` is the fan's minimum *while
spinning*, not a floor the curve must produce. The earlier caution in this file
— that the EC's own minimum would govern and step 0 might not mean off — was
right to withhold judgement and is now resolved by measurement.

Absent entirely on this firmware: `LENOVO_OTHER_METHOD.GetFeatureValue`. LLT's
preferred capability accessor does not exist here ("property not found"), so the
older per-feature methods are the working path — `GetSupportThermalMode` and
`Get_Support_LegionZone_Version` both succeed.

### GodMode version: V1 (measured 2026-08-12, issue #25)

Determined by `tools/Get-GodModeVersion.ps1`:

| Input | Value |
|---|---|
| `SmartFanVersion` (`IsSupportSmartFan` → `Data`) | **5** → V1 range (4 or 5) |
| `LegionZoneVersion` (`Get_Support_LegionZone_Version` → `Version`) | **2** → V1 range (1 or 2) |
| Power mode mask (`GetSupportThermalMode` → `mode`) | **65543** = `0x10007`; bit 16 set, so GodMode is supported |
| BIOS | `JUCN68WW` → prefix `JUCN`, version `68`; not in LLT's V1 blocklist, so the gate passes |

**This machine is GodMode V1**, so the minimum step table is
`[0,0,0,0,0,0,0,1,3,5]` and fancontrol's floors in `validate_custom_curve` are
correct. Steps 0–6 may legally be 0.

### Fan table properties, per (Fan_Id, Sensor_ID)

All three entries report `FanTable_Len = 10`, `CurrentFanMinSpeed = 1600`,
`CurrentFanMaxSpeed = 4800`. **`DefaultFanMaxSpeed` does not exist** on this
firmware, so LLT's `GetDefaultFanMaxSpeedAsync` would fail here;
`CurrentFanMaxSpeed` is the usable source and makes the stubbed
`Fan_Get_MaxSpeed` unnecessary.

**Power-button LED state is readable — measured 2026-09-02 (#44).**
`LENOVO_LIGHTING_METHOD` exists with `Get_Lighting_Current_Status` /
`Set_Lighting_Current_Status`; `LENOVO_LIGHTING_DATA` reports 6 instances of which
only `Lighting_Id` 0 and 4 are real — the other four carry `Lighting_Id = 255`.
`LENOVO_SPECTRUM_METHOD` and `LENOVO_GAMEZONE_LIGHT_PROFILE_DATA` are absent.
`Get_Lighting_Current_Status(<id>)` takes one integer and returns
`Current_Brightness_Level` and `Current_State_Type`. Across a SmartFanMode sweep
by `tools/Get-LenovoLighting.ps1`, exactly one field moved:
`Lighting_Id 4 → Current_State_Type` is 0 in Quiet, 1 in Balanced, 2 in
Performance, 3 in Custom. It is a state *index*, not a colour; the index-to-colour
mapping (blue / white / red / all three) still rests on the eye report. Ids 0, 1,
2, 5 read `0 / 0` and id 3 reads `0 / 1` in every mode; brightness is 0 everywhere.

**Which sensor row's thresholds index a written table is unmeasured.**
`encode_fan_table_bytes` hardcodes `FSID = 0`, and the fan 0 / sensor 3 row
starts `58,58,58,58,67` while the fan 0 / sensor 0 row starts `34,36,43,127`.
Both load tests used curves constant across indices 0–3, so neither can tell. A
curve that differs there is safe under one reading and stops the fans at every
load temperature under the other, which is why the tools' default safe curve is
`1,1,1,1,2,4,6,7,8,10` and not `0,0,0,1,…`. Settling it needs a curve that
differs across those indices, a hold at 58–66 °C, the sensor temperature logged
beside the RPM, and a dwell longer than the fan's ~30 s ramp.

Issue #18, **resolved 2026-08-19 by the AC-2 load test**: reading (a) is
correct — **step 0 means the fan is off.** The table holds 10 entries (indices
0–9) while `MAX_STEP_VALUE` of 10 admits 11 distinct step values, and the
resolution is that 0 is off with steps 1–10 mapping onto entries 0–9.

The two measurements that previously pointed away from (a) were real but did not
mean what they seemed to. `CurrentFanMinSpeed = 1600` equals `FanTable_Data[0]`
because 1600 is the slowest the fan turns *while turning*, which says nothing
about whether it may stop. `DesignMaxFanSpeedNumber = 9` is consistent with both
readings. Neither was evidence against 0 = off; they were evidence that indexing
starts at entry 0, which is compatible.

This is what the load test buys that no amount of table reading could: 17
consecutive samples at 58–62 °C, under load, with `MINIMUM_STEPS` in force and
both fans at 0 RPM.

~~Do not read the V1 minimum table as licence for fans-fully-off across the
seven lowest bands.~~ **Superseded 2026-08-19.** That caution assumed the EC's
1600 RPM minimum would govern regardless of the curve. It does not: a curve of
`MINIMUM_STEPS` stopped both fans outright at 58–62 °C under load. V1 permitting
a step of 0 turns out to be licence for exactly that, so a curve whose low bands
are 0 really will run the machine with its fans stopped up to the first non-zero
band — which is a design decision to make deliberately, not a limit the firmware
will quietly impose for you.
