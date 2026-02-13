//! Lenovo Legion fan controller backend using vendor-specific WMI.
//!
//! Uses `LENOVO_FAN_METHOD` and `LENOVO_FAN_TABLE_DATA` in the `root\WMI`
//! namespace. WMI method calls are performed via PowerShell subprocess since
//! the `wmi` crate only supports queries, not method invocation.

use std::process::Command;

use log::{debug, info, warn};

use crate::errors::FanControlError;
use crate::fan::Fan;
use super::FanController;

/// Fan speed range reported by LENOVO_FAN_TABLE_DATA.
const MIN_RPM: u32 = 1600;
const MAX_RPM: u32 = 4800;

/// Lenovo Legion fan controller backed by vendor-specific WMI classes.
pub struct LenovoFanController;

impl LenovoFanController {
    pub fn new() -> Self {
        Self
    }

    /// Call a WMI method via PowerShell and return the raw stdout.
    fn ps_command(script: &str) -> Result<String, FanControlError> {
        debug!("ps_command: {}", script);
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output()
            .map_err(|e| {
                warn!("ps_command failed to launch: {e}");
                FanControlError::Platform(format!("failed to run powershell: {e}"))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("ps_command stderr: {}", stderr.trim());
            return Err(FanControlError::Platform(format!(
                "powershell error: {}",
                stderr.trim()
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        debug!("ps_command stdout: {}", stdout);
        Ok(stdout)
    }

    /// Read current fan speed in RPM for a given fan ID (0 or 1).
    fn read_fan_speed(fan_id: u32) -> Result<u32, FanControlError> {
        let script = format!(
            "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
             ($fm.Fan_GetCurrentFanSpeed({fan_id})).CurrentFanSpeed"
        );
        let output = Self::ps_command(&script)?;
        output
            .parse::<u32>()
            .map_err(|e| FanControlError::Platform(format!("failed to parse fan speed: {e}")))
    }

    /// Map PWM (0–255) to RPM within the fan's operating range.
    fn pwm_to_rpm(pwm: u8) -> u32 {
        let ratio = pwm as f64 / 255.0;
        MIN_RPM + (ratio * (MAX_RPM - MIN_RPM) as f64) as u32
    }
}

impl FanController for LenovoFanController {
    fn discover(&self) -> Result<Vec<Fan>, FanControlError> {
        // Single PowerShell invocation: discover fans, read speeds and temps.
        // Group by Fan_Id taking the highest Sensor_ID per fan (the one
        // that actually returns temperature data on this hardware).
        let script =
            "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
             $tables = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_TABLE_DATA; \
             $best = @{}; \
             foreach ($t in $tables) { \
               $fid = $t.Fan_Id; \
               if (-not $best.ContainsKey($fid) -or $t.Sensor_ID -gt $best[$fid]) { \
                 $best[$fid] = $t.Sensor_ID \
               } \
             }; \
             foreach ($fid in ($best.Keys | Sort-Object)) { \
               $sid = $best[$fid]; \
               $speed = ($fm.Fan_GetCurrentFanSpeed($fid)).CurrentFanSpeed; \
               $temp = ($fm.Fan_GetCurrentSensorTemperature($sid)).CurrentSensorTemperature; \
               Write-Output \"$fid|$sid|$speed|$temp\" \
             }";

        let output = Self::ps_command(script)?;

        let mut fans = Vec::new();
        for line in output.lines() {
            let parts: Vec<&str> = line.split('|').collect();
            if parts.len() < 4 {
                continue;
            }

            let fan_id: u32 = parts[0].trim().parse().unwrap_or(0);
            let speed_rpm: u32 = parts[2].trim().parse().unwrap_or(0);
            let temp: u32 = parts[3].trim().parse().unwrap_or(0);

            let label = match fan_id {
                0 => "CPU Fan".to_string(),
                1 => "GPU Fan".to_string(),
                n => format!("Fan {n}"),
            };

            fans.push(Fan {
                id: format!("fan{fan_id}"),
                label: format!("{label} ({temp}°C)"),
                speed_rpm,
                pwm: Some(rpm_to_pwm(speed_rpm)),
                controllable: true,
            });
        }

        Ok(fans)
    }

    fn get_speed(&self, fan_id: &str) -> Result<u32, FanControlError> {
        let numeric_id = parse_fan_id(fan_id)?;
        Self::read_fan_speed(numeric_id)
    }

    fn set_pwm(&self, fan_id: &str, pwm: u8) -> Result<(), FanControlError> {
        let numeric_id = parse_fan_id(fan_id)?;

        if pwm == 255 {
            info!("set_pwm({fan_id}, 255) -> Fan_Set_FullSpeed(1)");
            let script =
                "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
                 $fm.Fan_Set_FullSpeed(1)";
            Self::ps_command(script)?;
        } else if pwm == 0 {
            info!("set_pwm({fan_id}, 0) -> Fan_Set_FullSpeed(0) [auto]");
            let script =
                "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
                 $fm.Fan_Set_FullSpeed(0)";
            Self::ps_command(script)?;
        } else {
            let target_rpm = Self::pwm_to_rpm(pwm);
            info!("set_pwm({fan_id}, {pwm}) -> Fan_SetCurrentFanSpeed({numeric_id}, {target_rpm})");
            let script = format!(
                "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
                 $fm.Fan_SetCurrentFanSpeed({numeric_id}, {target_rpm})"
            );
            Self::ps_command(&script)?;
        }

        Ok(())
    }

    fn stop_fan(&self, _fan_id: &str) -> Result<(), FanControlError> {
        // Release to BIOS auto control — same WMI call that previously
        // achieved 0 RPM when invoked via set_pwm(0) from the slider.
        let script =
            "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
             $fm.Fan_Set_FullSpeed(0)";
        Self::ps_command(script)?;
        Ok(())
    }
}

/// Parse a fan ID string like "fan0" or "fan1" into a numeric ID.
fn parse_fan_id(fan_id: &str) -> Result<u32, FanControlError> {
    fan_id
        .strip_prefix("fan")
        .and_then(|n| n.parse::<u32>().ok())
        .ok_or_else(|| FanControlError::FanNotFound(fan_id.to_string()))
}

/// Map RPM back to approximate PWM (0–255) for display.
fn rpm_to_pwm(rpm: u32) -> u8 {
    if rpm <= MIN_RPM {
        return 0;
    }
    if rpm >= MAX_RPM {
        return 255;
    }
    let ratio = (rpm - MIN_RPM) as f64 / (MAX_RPM - MIN_RPM) as f64;
    (ratio * 255.0) as u8
}
