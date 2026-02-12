use std::fmt;

/// Represents a single fan discovered on the system.
#[derive(Debug, Clone)]
pub struct Fan {
    /// Unique identifier (e.g. "hwmon2/fan1" on Linux, WMI instance path on Windows)
    pub id: String,
    /// Human-readable label (e.g. "CPU Fan", "Chassis Fan #1")
    pub label: String,
    /// Current speed in RPM
    pub speed_rpm: u32,
    /// PWM duty cycle 0–255 (if controllable)
    pub pwm: Option<u8>,
    /// Whether this fan supports speed control
    pub controllable: bool,
}

impl fmt::Display for Fan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let control_status = if self.controllable { "controllable" } else { "read-only" };
        write!(
            f,
            "{}: {} RPM [{}]",
            self.label, self.speed_rpm, control_status
        )
    }
}
