use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "fancontrol")]
#[command(about = "A minimal cross-platform app to control fan speed")]
#[command(version)]
pub struct Cli {
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

    /// Open the graphical fan control interface
    Gui,
}
