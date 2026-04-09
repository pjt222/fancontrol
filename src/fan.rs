// put id:"fan_structs", label:"Fan/FanCurve Data Structs", node_type:"database"

use serde::Serialize;
use std::fmt;

/// A single temperature→RPM point in a fan curve.
#[derive(Debug, Clone, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
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
    /// Whether full speed mode is currently active (Lenovo-specific).
    pub full_speed_active: bool,
}

/// A user-defined custom fan curve to write to the EC via Fan_Set_Table.
///
/// The `steps` array contains 10 speed step indices (0–10 scale) that index
/// into the hardware's FanSpeeds array from `LENOVO_FAN_TABLE_DATA`.
/// For example, on an 82RG with FanSpeeds = [1600,1800,...,4800]:
///   step index 0 → 1600 RPM, step index 9 → 4800 RPM.
#[derive(Debug, Clone)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub struct CustomFanCurve {
    /// Fan identifier (0 = CPU fan, 1 = GPU fan on V1 hardware).
    pub fan_id: u32,
    /// Sensor identifier (3 = CPU temp, 4 = GPU temp on V1 hardware).
    pub sensor_id: u32,
    /// 10 speed step indices, each 0–10. These are indices into the
    /// FanSpeeds array from LENOVO_FAN_TABLE_DATA, NOT RPM values.
    pub steps: [u8; 10],
}

impl fmt::Display for Fan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let control_status = if self.controllable {
            "controllable"
        } else {
            "read-only"
        };
        write!(
            f,
            "{}: {} RPM [{}]",
            self.label, self.speed_rpm, control_status
        )
    }
}
