#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

use crate::errors::FanControlError;
use crate::fan::Fan;

/// Platform-agnostic fan controller interface.
pub trait FanController {
    /// Discover all fans on the system.
    fn discover(&self) -> Result<Vec<Fan>, FanControlError>;

    /// Read current speed (RPM) of a fan by its id.
    fn get_speed(&self, fan_id: &str) -> Result<u32, FanControlError>;

    /// Set PWM duty cycle (0–255) for a fan by its id.
    fn set_pwm(&self, fan_id: &str, pwm: u8) -> Result<(), FanControlError>;
}

/// Create the platform-appropriate controller.
pub fn create_controller() -> Box<dyn FanController> {
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxFanController::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsFanController::new())
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        compile_error!("Unsupported platform: only Linux and Windows are supported");
    }
}
