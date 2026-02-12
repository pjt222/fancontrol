# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Fancontrol is a minimal cross-platform (Linux + Windows) application to control fan speed, written in Rust.

- **Linux**: Uses sysfs/hwmon interfaces (`/sys/class/hwmon/`)
- **Windows**: Uses WMI/ACPI for fan control

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
    └── windows.rs   # WMI backend (Win32_Fan, read-only — set_pwm returns NotControllable)
```

**Key pattern**: `FanController` trait in `platform/mod.rs` is the core abstraction. `create_controller()` returns `Box<dyn FanController>` using `#[cfg(target_os)]` to select the platform backend at compile time.

**Linux backend**: Scans sysfs hwmon directories, reads `fan*_input` for RPM, `fan*_label` for names, `pwm*` for duty cycle. Sets PWM by writing `pwm*_enable=1` (manual mode) then `pwm*=<value>`. Tests use `tempfile` to create fake hwmon trees.

**Windows backend**: Queries WMI `Win32_Fan` class via `wmi` crate. Fan speed control is not possible through standard WMI — `set_pwm` returns `NotControllable` with guidance toward vendor-specific tools.
