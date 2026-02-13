//! Lenovo Legion fan controller backend using vendor-specific WMI.
//!
//! Uses `LENOVO_FAN_METHOD` and `LENOVO_FAN_TABLE_DATA` in the `root\WMI`
//! namespace. WMI method calls are performed via PowerShell subprocess since
//! the `wmi` crate only supports queries, not method invocation.

use std::collections::HashMap;
use std::process::Command;

use log::{debug, info, warn};

use crate::errors::FanControlError;
use crate::fan::{Fan, FanCurve, FanCurvePoint};
use super::FanController;

/// Fallback RPM range used when table data is unavailable.
const DEFAULT_MIN_RPM: u32 = 1600;
const DEFAULT_MAX_RPM: u32 = 4800;

/// Per-fan RPM range learned from table data.
#[derive(Debug, Clone)]
struct FanRpmRange {
    min_rpm: u32,
    max_rpm: u32,
}

/// Lenovo Legion fan controller backed by vendor-specific WMI classes.
pub struct LenovoFanController {
    /// Per-fan RPM ranges, populated on first discover().
    fan_ranges: std::cell::RefCell<HashMap<u32, FanRpmRange>>,
}

impl LenovoFanController {
    pub fn new() -> Self {
        Self {
            fan_ranges: std::cell::RefCell::new(HashMap::new()),
        }
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

    /// Map PWM (0-255) to RPM using per-fan range, falling back to defaults.
    fn pwm_to_rpm(&self, fan_numeric_id: u32, pwm: u8) -> u32 {
        let ranges = self.fan_ranges.borrow();
        let (min_rpm, max_rpm) = match ranges.get(&fan_numeric_id) {
            Some(range) => (range.min_rpm, range.max_rpm),
            None => (DEFAULT_MIN_RPM, DEFAULT_MAX_RPM),
        };
        let ratio = pwm as f64 / 255.0;
        min_rpm + (ratio * (max_rpm - min_rpm) as f64) as u32
    }

    /// Map RPM back to approximate PWM (0-255) using per-fan range.
    fn rpm_to_pwm(&self, fan_numeric_id: u32, rpm: u32) -> u8 {
        let ranges = self.fan_ranges.borrow();
        let (min_rpm, max_rpm) = match ranges.get(&fan_numeric_id) {
            Some(range) => (range.min_rpm, range.max_rpm),
            None => (DEFAULT_MIN_RPM, DEFAULT_MAX_RPM),
        };
        if rpm <= min_rpm {
            return 0;
        }
        if rpm >= max_rpm {
            return 255;
        }
        let ratio = (rpm - min_rpm) as f64 / (max_rpm - min_rpm) as f64;
        (ratio * 255.0) as u8
    }
}

impl FanController for LenovoFanController {
    fn discover(&self) -> Result<Vec<Fan>, FanControlError> {
        // Single PowerShell invocation: discover fans, read speeds, temps,
        // and full fan table data (curves + RPM ranges).
        //
        // Output format:
        //   FAN|fan_id|sensor_id|speed|temp          — one per fan (best sensor)
        //   TABLE|fan_id|sensor_id|active|min_speed|max_speed|min_temp|max_temp|speeds_csv|temps_csv
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
             foreach ($t in $tables) { \
               $fid = $t.Fan_Id; \
               $sid = $t.Sensor_ID; \
               $active = if ($t.Active) { '1' } else { '0' }; \
               $speeds = ($t.FanTable_Data -join ','); \
               $temps = ($t.SensorTable_Data -join ','); \
               $minSpd = ($t.FanTable_Data | Measure-Object -Minimum).Minimum; \
               $maxSpd = ($t.FanTable_Data | Measure-Object -Maximum).Maximum; \
               $minTmp = ($t.SensorTable_Data | Measure-Object -Minimum).Minimum; \
               $maxTmp = ($t.SensorTable_Data | Measure-Object -Maximum).Maximum; \
               Write-Output \"TABLE|$fid|$sid|$active|$minSpd|$maxSpd|$minTmp|$maxTmp|$speeds|$temps\" \
             }; \
             foreach ($fid in ($best.Keys | Sort-Object)) { \
               $sid = $best[$fid]; \
               $speed = ($fm.Fan_GetCurrentFanSpeed($fid)).CurrentFanSpeed; \
               $temp = ($fm.Fan_GetCurrentSensorTemperature($sid)).CurrentSensorTemperature; \
               Write-Output \"FAN|$fid|$sid|$speed|$temp\" \
             }";

        let output = Self::ps_command(script)?;

        // First pass: parse TABLE lines to build curves and RPM ranges.
        let mut curves_by_fan: HashMap<u32, Vec<FanCurve>> = HashMap::new();
        // Track overall min/max RPM per fan across all its sensor curves.
        let mut rpm_ranges: HashMap<u32, FanRpmRange> = HashMap::new();

        for line in output.lines() {
            if !line.starts_with("TABLE|") {
                continue;
            }
            let parts: Vec<&str> = line.split('|').collect();
            if parts.len() < 10 {
                warn!("TABLE line too short: {line}");
                continue;
            }

            let fan_id: u32 = parts[1].trim().parse().unwrap_or(0);
            let sensor_id: u32 = parts[2].trim().parse().unwrap_or(0);
            let active = parts[3].trim() == "1";
            let min_speed: u32 = parts[4].trim().parse().unwrap_or(0);
            let max_speed: u32 = parts[5].trim().parse().unwrap_or(0);
            let min_temp: u32 = parts[6].trim().parse().unwrap_or(0);
            let max_temp: u32 = parts[7].trim().parse().unwrap_or(0);

            let speeds: Vec<u32> = parts[8]
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect();
            let temps: Vec<u32> = parts[9]
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect();

            let point_count = speeds.len().min(temps.len());
            let points: Vec<FanCurvePoint> = (0..point_count)
                .map(|i| FanCurvePoint {
                    temperature: temps[i],
                    fan_speed: speeds[i],
                })
                .collect();

            debug!(
                "TABLE: fan={fan_id} sensor={sensor_id} active={active} \
                 speed={min_speed}-{max_speed} temp={min_temp}-{max_temp} points={point_count}"
            );

            let curve = FanCurve {
                fan_id,
                sensor_id,
                min_speed,
                max_speed,
                min_temp,
                max_temp,
                points,
                active,
            };

            curves_by_fan.entry(fan_id).or_default().push(curve);

            // Update per-fan RPM range (take the widest range across curves).
            let range = rpm_ranges.entry(fan_id).or_insert(FanRpmRange {
                min_rpm: min_speed,
                max_rpm: max_speed,
            });
            if min_speed < range.min_rpm {
                range.min_rpm = min_speed;
            }
            if max_speed > range.max_rpm {
                range.max_rpm = max_speed;
            }
        }

        // Store learned RPM ranges for pwm_to_rpm/rpm_to_pwm.
        *self.fan_ranges.borrow_mut() = rpm_ranges.clone();

        // Second pass: parse FAN lines to build Fan structs.
        let mut fans = Vec::new();
        for line in output.lines() {
            if !line.starts_with("FAN|") {
                continue;
            }
            let parts: Vec<&str> = line.split('|').collect();
            if parts.len() < 5 {
                continue;
            }

            let fan_id: u32 = parts[1].trim().parse().unwrap_or(0);
            let speed_rpm: u32 = parts[3].trim().parse().unwrap_or(0);
            let temp: u32 = parts[4].trim().parse().unwrap_or(0);

            let label = match fan_id {
                0 => "CPU Fan".to_string(),
                1 => "GPU Fan".to_string(),
                n => format!("Fan {n}"),
            };

            let range = rpm_ranges.get(&fan_id);
            let curves = curves_by_fan.remove(&fan_id).unwrap_or_default();

            fans.push(Fan {
                id: format!("fan{fan_id}"),
                label: format!("{label} ({temp}\u{00B0}C)"),
                speed_rpm,
                pwm: Some(self.rpm_to_pwm(fan_id, speed_rpm)),
                controllable: true,
                min_rpm: range.map(|r| r.min_rpm),
                max_rpm: range.map(|r| r.max_rpm),
                curves,
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
            let target_rpm = self.pwm_to_rpm(numeric_id, pwm);
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
        let script =
            "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
             $fm.Fan_Set_FullSpeed(0)";
        Self::ps_command(script)?;
        Ok(())
    }

    fn get_fan_curves(&self) -> Result<Vec<FanCurve>, FanControlError> {
        // Dedicated query for just the table data (no speed/temp reads).
        let script =
            "$tables = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_TABLE_DATA; \
             foreach ($t in $tables) { \
               $fid = $t.Fan_Id; \
               $sid = $t.Sensor_ID; \
               $active = if ($t.Active) { '1' } else { '0' }; \
               $speeds = ($t.FanTable_Data -join ','); \
               $temps = ($t.SensorTable_Data -join ','); \
               $minSpd = ($t.FanTable_Data | Measure-Object -Minimum).Minimum; \
               $maxSpd = ($t.FanTable_Data | Measure-Object -Maximum).Maximum; \
               $minTmp = ($t.SensorTable_Data | Measure-Object -Minimum).Minimum; \
               $maxTmp = ($t.SensorTable_Data | Measure-Object -Maximum).Maximum; \
               Write-Output \"$fid|$sid|$active|$minSpd|$maxSpd|$minTmp|$maxTmp|$speeds|$temps\" \
             }";

        let output = Self::ps_command(script)?;
        let mut curves = Vec::new();

        for line in output.lines() {
            let parts: Vec<&str> = line.split('|').collect();
            if parts.len() < 9 {
                continue;
            }

            let fan_id: u32 = parts[0].trim().parse().unwrap_or(0);
            let sensor_id: u32 = parts[1].trim().parse().unwrap_or(0);
            let active = parts[2].trim() == "1";
            let min_speed: u32 = parts[3].trim().parse().unwrap_or(0);
            let max_speed: u32 = parts[4].trim().parse().unwrap_or(0);
            let min_temp: u32 = parts[5].trim().parse().unwrap_or(0);
            let max_temp: u32 = parts[6].trim().parse().unwrap_or(0);

            let speeds: Vec<u32> = parts[7]
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect();
            let temps: Vec<u32> = parts[8]
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect();

            let point_count = speeds.len().min(temps.len());
            let points: Vec<FanCurvePoint> = (0..point_count)
                .map(|i| FanCurvePoint {
                    temperature: temps[i],
                    fan_speed: speeds[i],
                })
                .collect();

            curves.push(FanCurve {
                fan_id,
                sensor_id,
                min_speed,
                max_speed,
                min_temp,
                max_temp,
                points,
                active,
            });
        }

        Ok(curves)
    }
}

/// Parse a fan ID string like "fan0" or "fan1" into a numeric ID.
fn parse_fan_id(fan_id: &str) -> Result<u32, FanControlError> {
    fan_id
        .strip_prefix("fan")
        .and_then(|n| n.parse::<u32>().ok())
        .ok_or_else(|| FanControlError::FanNotFound(fan_id.to_string()))
}
