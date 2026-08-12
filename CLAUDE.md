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
├── cli.rs           # clap-derived CLI: list, get, set, monitor, table, gui
├── fan.rs           # Fan, FanCurve, FanCurvePoint structs
├── errors.rs        # FanControlError enum (thiserror-based)
├── gui.rs           # egui/eframe GUI with worker thread
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

Firmware stubs (return empty data): `Fan_Get_MaxSpeed`, `Fan_Get_Table`

Untested/deferred: `Fan_Set_Table`, `Fan_Set_MaxSpeed`

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

Note for issue #18: the table holds **10** entries (indices 0–9) while
`MAX_STEP_VALUE` of 10 admits **11** distinct step values. A direct-index reading
leaves step 10 out of bounds, which favours "0 = off, steps 1–10 map to entries
0–9" over the reading in `CustomFanCurve`'s doc comment. Suggestive, not proof —
the load test in #18 decides.
