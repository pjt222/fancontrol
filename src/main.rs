mod cli;
mod config;
mod errors;
mod fan;
mod gui;
mod platform;
mod tui;

use std::thread;
use std::time::Duration;

use std::fs::File;

use anyhow::Result;
use clap::Parser;
use log::info;
use serde_json::json;
use simplelog::{ConfigBuilder, LevelFilter, WriteLogger};

use cli::{Cli, Commands};
use fan::CustomFanCurve;
use platform::{create_controller, FanController};

// put id:"cli_parse", label:"Parse CLI Arguments", output:"cli_command.internal"
// put id:"setup_logging", label:"Setup File Logger", output:"fancontrol.log"
// put id:"create_ctrl", label:"Create Platform Controller", input:"cli_command.internal", output:"controller.internal"
// put id:"dispatch", label:"Dispatch CLI Command", input:"cli_command.internal, controller.internal"

fn level_from_verbosity(verbosity: u8) -> LevelFilter {
    match verbosity {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Info,
        2 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Log to fancontrol.log next to the executable.
    let log_path = std::env::current_exe()
        .unwrap_or_default()
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("fancontrol.log");
    let log_config = ConfigBuilder::new().set_time_format_rfc3339().build();
    let log_level = level_from_verbosity(cli.verbose);
    if let Ok(file) = File::create(&log_path) {
        let _ = WriteLogger::init(log_level, log_config, file);
    }
    info!("fancontrol started (log level: {})", log_level);

    let json_output = cli.json;

    match cli.command {
        Commands::Gui => {
            if json_output {
                eprintln!("Warning: --json flag has no effect with the gui subcommand");
            }
            gui::run()
        }
        Commands::Tui => {
            if json_output {
                eprintln!("Warning: --json flag has no effect with the tui subcommand");
            }
            tui::run()
        }
        other => {
            let controller = create_controller()?;
            match other {
                Commands::List => cmd_list(&*controller, json_output),
                Commands::Get { fan_id } => cmd_get(&*controller, &fan_id, json_output),
                Commands::Set { fan_id, pwm } => cmd_set(&*controller, &fan_id, pwm),
                Commands::Monitor { interval } => cmd_monitor(&*controller, interval),
                Commands::Table { fan_id } => cmd_table(&*controller, fan_id, json_output),
                Commands::SetCurve {
                    fan_id,
                    sensor_id,
                    steps,
                    save,
                } => cmd_set_curve(&*controller, fan_id, sensor_id, steps, save),
                Commands::Gui | Commands::Tui => unreachable!(),
            }
        }
    }
}

fn cmd_list(controller: &dyn FanController, json_output: bool) -> Result<()> {
    let fans = controller.discover()?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&fans)?);
        return Ok(());
    }

    if fans.is_empty() {
        println!("No fans detected.");
        return Ok(());
    }

    if fans.iter().any(|f| f.full_speed_active) {
        println!("** FULL SPEED MODE ACTIVE **\n");
    }

    println!(
        "{:<25} {:<20} {:>8} {:>6} STATUS",
        "ID", "LABEL", "RPM", "PWM"
    );
    println!("{}", "-".repeat(70));
    for fan in &fans {
        let pwm_display = fan
            .pwm
            .map(|p| format!("{}", p))
            .unwrap_or_else(|| "\u{2014}".into());
        let status = if fan.controllable {
            "controllable"
        } else {
            "read-only"
        };
        println!(
            "{:<25} {:<20} {:>8} {:>6} {}",
            fan.id, fan.label, fan.speed_rpm, pwm_display, status
        );
    }
    Ok(())
}

fn cmd_get(controller: &dyn FanController, fan_id: &str, json_output: bool) -> Result<()> {
    let rpm = controller.get_speed(fan_id)?;

    if json_output {
        println!("{}", json!({"fan_id": fan_id, "rpm": rpm}));
        return Ok(());
    }

    println!("{} RPM", rpm);
    Ok(())
}

fn cmd_set(controller: &dyn FanController, fan_id: &str, pwm: u8) -> Result<()> {
    controller.set_pwm(fan_id, pwm)?;
    println!("Set {} PWM to {}", fan_id, pwm);
    Ok(())
}

fn cmd_table(
    controller: &dyn FanController,
    filter_fan_id: Option<u32>,
    json_output: bool,
) -> Result<()> {
    // Prefer curves already attached to fans from discover(), falling back
    // to the dedicated get_fan_curves() method.
    let fans = controller.discover()?;

    let full_speed_active = fans.iter().any(|f| f.full_speed_active);
    let has_embedded_curves = fans.iter().any(|f| !f.curves.is_empty());

    let curves = if has_embedded_curves {
        fans.into_iter().flat_map(|f| f.curves).collect::<Vec<_>>()
    } else {
        controller.get_fan_curves()?
    };

    let has_any_curves = !curves.is_empty();

    let filtered: Vec<_> = match filter_fan_id {
        Some(fid) => curves.into_iter().filter(|c| c.fan_id == fid).collect(),
        None => curves,
    };

    if json_output {
        println!("{}", serde_json::to_string_pretty(&filtered)?);
        return Ok(());
    }

    if full_speed_active {
        println!("** FULL SPEED MODE ACTIVE **\n");
    }

    if filtered.is_empty() {
        if has_any_curves {
            println!("No fan curves found for the specified fan ID.");
        } else {
            println!("No fan curve data available on this platform.");
        }
        return Ok(());
    }

    for curve in &filtered {
        let fan_label = match curve.fan_id {
            0 => "CPU Fan",
            1 => "GPU Fan",
            _ => "Fan",
        };
        let active_tag = if curve.active { "Active" } else { "Inactive" };
        println!(
            "Fan {} ({}) \u{2014} Sensor {} [{}]",
            curve.fan_id, fan_label, curve.sensor_id, active_tag
        );
        println!(
            "  Speed: {}\u{2013}{} RPM | Temp: {}\u{2013}{}\u{00B0}C",
            curve.min_speed, curve.max_speed, curve.min_temp, curve.max_temp
        );
        for point in &curve.points {
            println!(
                "  {}{}\u{00B0}C \u{2192} {} RPM",
                if point.temperature < 100 { " " } else { "" },
                point.temperature,
                point.fan_speed
            );
        }
        println!();
    }

    Ok(())
}

fn cmd_set_curve(
    controller: &dyn FanController,
    fan_id: u32,
    sensor_id: u32,
    steps: [u8; 10],
    save: bool,
) -> Result<()> {
    let curve = CustomFanCurve {
        fan_id,
        sensor_id,
        steps,
    };

    controller.set_custom_curve(&curve)?;

    println!(
        "Custom fan curve set for fan {} sensor {}",
        fan_id, sensor_id
    );
    println!("Steps: {:?}", steps);

    if save {
        let mut cfg = config::load_config();
        // Upsert: replace existing curve for this fan+sensor, or add new
        cfg.custom_curves
            .retain(|c| !(c.fan_id == fan_id && c.sensor_id == sensor_id));
        cfg.custom_curves.push(curve);
        config::save_config(&cfg)?;
        println!("Saved to {}", config::config_path().display());
    } else {
        println!();
        println!("Note: Custom curves require SmartFanMode=Custom and are volatile");
        println!("      (lost on reboot, sleep, or power mode change).");
        println!("      Use --save to persist to config file.");
    }

    Ok(())
}

fn cmd_monitor(controller: &dyn FanController, interval_secs: u64) -> Result<()> {
    println!("Monitoring fans (Ctrl+C to stop)...\n");
    loop {
        // Clear screen with ANSI escape
        print!("\x1B[2J\x1B[H");
        println!("Fan Monitor (every {}s) — Ctrl+C to stop\n", interval_secs);

        let fans = controller.discover()?;
        if fans.is_empty() {
            println!("No fans detected.");
        } else {
            if fans.iter().any(|f| f.full_speed_active) {
                println!("** FULL SPEED MODE ACTIVE **\n");
            }
            println!("{:<25} {:>8} {:>6}", "FAN", "RPM", "PWM");
            println!("{}", "-".repeat(45));
            for fan in &fans {
                let pwm_display = fan
                    .pwm
                    .map(|p| format!("{}", p))
                    .unwrap_or_else(|| "—".into());
                println!("{:<25} {:>8} {:>6}", fan.label, fan.speed_rpm, pwm_display);
            }
        }

        thread::sleep(Duration::from_secs(interval_secs));
    }
}
