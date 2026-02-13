use std::fmt;

/// A single temperature→RPM point in a fan curve.
#[derive(Debug, Clone)]
pub struct FanCurvePoint {
    /// Temperature threshold in degrees Celsius.
    pub temperature: u32,
    /// Target fan speed in RPM at this temperature.
    pub fan_speed: u32,
}

/// A fan curve mapping sensor temperatures to fan speeds.
///
/// Each curve binds one fan to one sensor. The EC takes the maximum speed
/// demanded across all sensor curves for a given fan.
#[derive(Debug, Clone)]
pub struct FanCurve {
    pub fan_id: u32,
    pub sensor_id: u32,
    pub min_speed: u32,
    pub max_speed: u32,
    pub min_temp: u32,
    pub max_temp: u32,
    pub points: Vec<FanCurvePoint>,
    pub active: bool,
}

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
    /// Minimum RPM from fan table data (if available).
    pub min_rpm: Option<u32>,
    /// Maximum RPM from fan table data (if available).
    pub max_rpm: Option<u32>,
    /// Fan curves from EC table data (if available).
    pub curves: Vec<FanCurve>,
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
