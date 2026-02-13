#[cfg(any(target_os = "windows", test))]
mod lenovo;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

use crate::errors::FanControlError;
use crate::fan::{Fan, FanCurve};

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
