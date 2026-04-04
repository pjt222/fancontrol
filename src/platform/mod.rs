#[cfg(any(target_os = "windows", test))]
mod lenovo;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

use crate::errors::FanControlError;
use crate::fan::{CustomFanCurve, Fan, FanCurve};

/// Platform-agnostic fan controller interface.
pub trait FanController {
    /// Discover all fans on the system.
    fn discover(&self) -> Result<Vec<Fan>, FanControlError>;

    /// Read current speed (RPM) of a fan by its id.
    fn get_speed(&self, fan_id: &str) -> Result<u32, FanControlError>;

    /// Set PWM duty cycle (0–255) for a fan by its id.
    fn set_pwm(&self, fan_id: &str, pwm: u8) -> Result<(), FanControlError>;

    /// Read fan curve / table data from the EC. Default returns an error
    /// indicating the platform does not support fan curves.
    fn get_fan_curves(&self) -> Result<Vec<FanCurve>, FanControlError> {
        Err(FanControlError::Platform(
            "fan curves not supported on this platform".to_string(),
        ))
    }

    /// Write a custom fan curve to the EC. Requires Lenovo hardware in
    /// Custom SmartFanMode. Default returns not-supported.
    fn set_custom_curve(&self, _curve: &CustomFanCurve) -> Result<(), FanControlError> {
        Err(FanControlError::Platform(
            "custom fan curves not supported on this platform".to_string(),
        ))
    }

    // TODO: These SmartFanMode methods are only implemented by the Lenovo
    // backend and have no callers on Linux. Investigate whether to:
    //   1. Expose them via CLI/TUI commands (e.g. `fancontrol smart-fan-mode`)
    //   2. Move them to a Lenovo-specific trait extension
    //   3. Gate them behind #[cfg(target_os = "windows")]

    /// Read the current SmartFanMode (Lenovo-specific). Returns `None` on
    /// platforms that don't support it.
    #[allow(dead_code)]
    fn get_smart_fan_mode(&self) -> Result<Option<u32>, FanControlError> {
        Ok(None)
    }

    /// Set SmartFanMode (Lenovo-specific). Default returns not-supported.
    #[allow(dead_code)]
    fn set_smart_fan_mode(&self, _mode: u32) -> Result<(), FanControlError> {
        Err(FanControlError::Platform(
            "SmartFanMode not supported on this platform".to_string(),
        ))
    }
}

// put id:"platform_select", label:"Platform Detection", node_type:"decision", output:"controller.internal"

/// Create the platform-appropriate controller.
pub fn create_controller() -> Result<Box<dyn FanController>, FanControlError> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::LinuxFanController::new()))
    }
    #[cfg(target_os = "windows")]
    {
        if windows::is_lenovo() {
            Ok(Box::new(lenovo::LenovoFanController::new()))
        } else {
            Ok(Box::new(windows::WindowsFanController::new()?))
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        compile_error!("Unsupported platform: only Linux and Windows are supported");
    }
}
