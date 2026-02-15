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

    /// Write a custom fan curve (temperature→RPM pairs)
    SetCurve {
        /// Fan ID (numeric, e.g. 0 or 1)
        #[arg(long)]
        fan_id: u32,

        /// Sensor ID (numeric, e.g. 3 or 4)
        #[arg(long)]
        sensor_id: u32,

        /// Temperature→RPM pairs as "temp:rpm" (e.g. 50:1600 60:2100 70:3200 85:4800)
        #[arg(required = true, num_args = 2..)]
        points: Vec<String>,
    },

    /// Back up current fan curves to a JSON file
    BackupCurves {
        /// Output file path (default: fan_curves_backup.json)
        #[arg(short, long, default_value = "fan_curves_backup.json")]
        output: String,
    },

    /// Restore fan curves from a JSON backup file
    RestoreCurves {
        /// Input file path
        #[arg(short, long, default_value = "fan_curves_backup.json")]
        input: String,
    },

    /// Open the graphical fan control interface
    Gui,
}
