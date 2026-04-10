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

    /// Output in JSON format (for list, get, table commands)
    #[arg(long, global = true)]
    pub json: bool,

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

    /// Set a custom fan curve (Lenovo only, requires Custom SmartFanMode)
    SetCurve {
        /// Fan ID (0 = CPU fan, 1 = GPU fan on V1 hardware)
        #[arg(long)]
        fan_id: u32,

        /// Sensor ID (3 = CPU temp, 4 = GPU temp on V1 hardware)
        #[arg(long)]
        sensor_id: u32,

        /// 10 comma-separated speed step indices (0–10 scale).
        /// Each value indexes into the hardware's FanSpeeds array.
        /// Example: "0,0,0,1,2,4,6,7,8,10"
        #[arg(long, value_parser = parse_steps)]
        steps: [u8; 10],

        /// Save the curve to fancontrol.json for automatic re-application
        #[arg(long)]
        save: bool,
    },

    /// Open the graphical fan control interface
    Gui,

    /// Open the interactive terminal UI dashboard
    Tui,
}

/// Parse 10 comma-separated step values into a fixed-size array.
fn parse_steps(s: &str) -> Result<[u8; 10], String> {
    let values: Vec<u8> = s
        .split(',')
        .map(|v| {
            v.trim()
                .parse::<u8>()
                .map_err(|e| format!("invalid step value '{}': {}", v.trim(), e))
        })
        .collect::<Result<Vec<_>, _>>()?;

    if values.len() != 10 {
        return Err(format!("expected 10 step values, got {}", values.len()));
    }

    values
        .try_into()
        .map_err(|_| "expected exactly 10 values".to_string())
}
