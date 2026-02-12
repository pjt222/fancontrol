mod cli;
mod errors;
mod fan;
mod gui;
mod platform;

use std::thread;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Commands};
use platform::{create_controller, FanController};

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Gui => gui::run(),
        other => {
            let controller = create_controller();
            match other {
                Commands::List => cmd_list(&*controller),
                Commands::Get { fan_id } => cmd_get(&*controller, &fan_id),
                Commands::Set { fan_id, pwm } => cmd_set(&*controller, &fan_id, pwm),
                Commands::Monitor { interval } => cmd_monitor(&*controller, interval),
                Commands::Gui => unreachable!(),
            }
        }
    }
}

fn cmd_list(controller: &dyn FanController) -> Result<()> {
    let fans = controller.discover()?;
    if fans.is_empty() {
        println!("No fans detected.");
        return Ok(());
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
            .unwrap_or_else(|| "—".into());
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

fn cmd_get(controller: &dyn FanController, fan_id: &str) -> Result<()> {
    let rpm = controller.get_speed(fan_id)?;
    println!("{} RPM", rpm);
    Ok(())
}

fn cmd_set(controller: &dyn FanController, fan_id: &str, pwm: u8) -> Result<()> {
    controller.set_pwm(fan_id, pwm)?;
    println!("Set {} PWM to {}", fan_id, pwm);
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
