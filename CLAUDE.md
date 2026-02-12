# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Fancontrol is a minimal cross-platform (Linux + Windows) application to control fan speed, written in Rust.

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

## Architecture

```
src/
├── main.rs          # Entry point, CLI dispatch (cmd_list/get/set/monitor)
├── cli.rs           # clap-derived CLI with subcommands: list, get, set, monitor
├── fan.rs           # Fan struct (id, label, speed_rpm, pwm, controllable)
├── errors.rs        # FanControlError enum (thiserror-based)
└── platform/
    ├── mod.rs       # FanController trait + create_controller() factory
    ├── linux.rs     # sysfs/hwmon backend (/sys/class/hwmon/hwmon*/fan*_input)
    ├── windows.rs   # Generic WMI backend (Win32_Fan) + is_lenovo() detection
    └── lenovo.rs    # Lenovo Legion backend (LENOVO_FAN_METHOD via PowerShell)
```

**Key pattern**: `FanController` trait in `platform/mod.rs` is the core abstraction. `create_controller()` returns `Box<dyn FanController>` using `#[cfg(target_os)]` to select the platform backend at compile time.

**Linux backend**: Scans sysfs hwmon directories, reads `fan*_input` for RPM, `fan*_label` for names, `pwm*` for duty cycle. Sets PWM by writing `pwm*_enable=1` (manual mode) then `pwm*=<value>`. Tests use `tempfile` to create fake hwmon trees.

**Windows generic backend**: Queries WMI `Win32_Fan` class via `wmi` crate. Most hardware doesn't expose fans through this class. `set_pwm` returns `NotControllable`.

**Lenovo backend**: Detected at runtime via `Win32_ComputerSystem.Manufacturer`. Uses `LENOVO_FAN_METHOD` (root\WMI namespace) for `Fan_GetCurrentFanSpeed`, `Fan_GetCurrentSensorTemperature`, `Fan_Set_FullSpeed`. Discovers fans via `LENOVO_FAN_TABLE_DATA`. WMI method calls go through PowerShell subprocess since the `wmi` crate only supports queries. PWM 0=auto, 255=full speed, 1-254 maps to RPM range.

**Cross-compilation** (from WSL to Windows):
```bash
rustup target add x86_64-pc-windows-gnu
sudo apt-get install mingw-w64
cargo build --release --target x86_64-pc-windows-gnu
# Binary at target/x86_64-pc-windows-gnu/release/fancontrol.exe
```
