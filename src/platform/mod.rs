#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod lenovo;
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

    /// Force-stop a fan (0 RPM). Default calls set_pwm(0); override for
    /// platforms where PWM 0 means something else (e.g. Lenovo auto mode).
    fn stop_fan(&self, fan_id: &str) -> Result<(), FanControlError> {
        self.set_pwm(fan_id, 0)
    }

    /// Read fan curve / table data from the EC. Default returns an error
    /// indicating the platform does not support fan curves.
    fn get_fan_curves(&self) -> Result<Vec<FanCurve>, FanControlError> {
        Err(FanControlError::Platform(
            "fan curves not supported on this platform".to_string(),
        ))
    }
}

/// Create the platform-appropriate controller.
pub fn create_controller() -> Box<dyn FanController> {
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxFanController::new())
    }
    #[cfg(target_os = "windows")]
    {
        if windows::is_lenovo() {
            Box::new(lenovo::LenovoFanController::new())
        } else {
            Box::new(windows::WindowsFanController::new())
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        compile_error!("Unsupported platform: only Linux and Windows are supported");
    }
}
