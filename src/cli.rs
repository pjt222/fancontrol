// put id:"cli_def", label:"CLI Definition (clap)", output:"cli_command.internal"

use clap::{ArgAction, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "fancontrol")]
#[command(about = "A minimal cross-platform app to control fan speed")]
#[command(version)]
pub struct Cli {
    /// Increase log verbosity (-v = info, -vv = debug, -vvv = trace)
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// List all detected fans
    List,

    /// Get the current speed of a fan
    Get {
        /// Fan ID (use 'list' to see available fans)
        fan_id: String,
    },

    /// Set the PWM duty cycle of a fan (0–255)
    Set {
        /// Fan ID (use 'list' to see available fans)
        fan_id: String,

        /// PWM value (0 = off, 255 = full speed)
        #[arg(value_parser = clap::value_parser!(u8))]
        pwm: u8,
    },

    /// Monitor all fans in real-time
    Monitor {
        /// Refresh interval in seconds
        #[arg(short, long, default_value = "1")]
        interval: u64,
    },

    /// Display EC fan curve / table data
    Table {
        /// Show curves for a specific fan ID only (e.g. 0, 1)
        #[arg(long)]
        fan_id: Option<u32>,
    },

    /// Open the graphical fan control interface
    Gui,
}
