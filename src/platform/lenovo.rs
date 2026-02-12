//! Lenovo Legion fan controller backend using vendor-specific WMI.
//!
//! Uses `LENOVO_FAN_METHOD` and `LENOVO_FAN_TABLE_DATA` in the `root\WMI`
//! namespace. WMI method calls are performed via PowerShell subprocess since
//! the `wmi` crate only supports queries, not method invocation.

use std::process::Command;

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
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output()
            .map_err(|e| FanControlError::Platform(format!("failed to run powershell: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(FanControlError::Platform(format!(
                "powershell error: {}",
                stderr.trim()
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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

    /// Read sensor temperature for a given sensor ID.
    fn read_sensor_temp(sensor_id: u32) -> Result<u32, FanControlError> {
        let script = format!(
            "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
             ($fm.Fan_GetCurrentSensorTemperature({sensor_id})).CurrentSensorTemperature"
        );
        let output = Self::ps_command(&script)?;
        output
            .parse::<u32>()
            .map_err(|e| FanControlError::Platform(format!("failed to parse sensor temp: {e}")))
    }

    /// Map PWM (0–255) to RPM within the fan's operating range.
    fn pwm_to_rpm(pwm: u8) -> u32 {
        let ratio = pwm as f64 / 255.0;
        MIN_RPM + (ratio * (MAX_RPM - MIN_RPM) as f64) as u32
    }
}

impl FanController for LenovoFanController {
    fn discover(&self) -> Result<Vec<Fan>, FanControlError> {
        // Query fan table data to discover fans and their sensor mappings.
        let script =
            "$tables = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_TABLE_DATA; \
             $tables | Select-Object -Property Fan_Id, Sensor_ID, CurrentFanMinSpeed, CurrentFanMaxSpeed, InstanceName | \
             ForEach-Object { \"$($_.Fan_Id)|$($_.Sensor_ID)|$($_.CurrentFanMinSpeed)|$($_.CurrentFanMaxSpeed)|$($_.InstanceName)\" }";

        let output = Self::ps_command(script)?;

        // Deduplicate by Fan_Id (multiple table entries per fan for different sensors).
        let mut seen_fans = std::collections::HashMap::new();

        for line in output.lines() {
            let parts: Vec<&str> = line.split('|').collect();
            if parts.len() < 5 {
                continue;
            }

            let fan_id: u32 = parts[0].trim().parse().unwrap_or(0);
            let sensor_id: u32 = parts[1].trim().parse().unwrap_or(0);

            seen_fans.entry(fan_id).or_insert(sensor_id);
        }

        let mut fans = Vec::new();
        let mut fan_ids: Vec<u32> = seen_fans.keys().copied().collect();
        fan_ids.sort();

        for fan_id in fan_ids {
            let label = match fan_id {
                0 => "CPU Fan".to_string(),
                1 => "GPU Fan".to_string(),
                n => format!("Fan {n}"),
            };

            let speed_rpm = Self::read_fan_speed(fan_id).unwrap_or(0);
            let sensor_id = seen_fans[&fan_id];
            let temp = Self::read_sensor_temp(sensor_id).unwrap_or(0);

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
            // Full speed mode
            let script =
                "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
                 $fm.Fan_Set_FullSpeed(1)";
            Self::ps_command(script)?;
        } else if pwm == 0 {
            // Return to automatic control
            let script =
                "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
                 $fm.Fan_Set_FullSpeed(0)";
            Self::ps_command(script)?;
        } else {
            // Set specific speed via RPM
            let target_rpm = Self::pwm_to_rpm(pwm);
            let script = format!(
                "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
                 $fm.Fan_SetCurrentFanSpeed({numeric_id}, {target_rpm})"
            );
            Self::ps_command(&script)?;
        }

        Ok(())
    }

    fn stop_fan(&self, fan_id: &str) -> Result<(), FanControlError> {
        let numeric_id = parse_fan_id(fan_id)?;
        // Step 1: release manual override back to BIOS auto control.
        let auto_script =
            "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
             $fm.Fan_Set_FullSpeed(0)";
        Self::ps_command(auto_script)?;
        // Step 2: request lowest possible speed.
        let min_script = format!(
            "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
             $fm.Fan_SetCurrentFanSpeed({numeric_id}, 0)"
        );
        Self::ps_command(&min_script)?;
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
