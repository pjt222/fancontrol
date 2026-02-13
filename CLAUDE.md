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
scripts/
├── probe-wmi-methods.ps1   # WMI method probe (run on native Windows)
├── dump-fan-table.ps1      # Full fan table dump
├── probe-wmi-methods.log   # Probe results
└── dump-fan-table.log      # Table dump results
```

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
