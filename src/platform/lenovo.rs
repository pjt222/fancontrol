// put id:"lenovo_discover", label:"Lenovo Discovery (PowerShell)", output:"fan_list.internal, fan_curves.internal, rpm_ranges.internal"
// put id:"lenovo_ps", label:"PowerShell WMI Subprocess", input:"wmi_script.internal", output:"ps_stdout.internal", node_type:"subprocess"
// put id:"lenovo_parse", label:"Parse TABLE|FAN|FULLSPEED", input:"ps_stdout.internal", output:"fan_list.internal"
// put id:"lenovo_set", label:"Set Fan Speed (WMI)", input:"pwm_command.internal"

//! Lenovo Legion fan controller backend using vendor-specific WMI.
//!
//! Uses `LENOVO_FAN_METHOD` and `LENOVO_FAN_TABLE_DATA` in the `root\WMI`
//! namespace. WMI method calls are performed via PowerShell subprocess since
//! the `wmi` crate only supports queries, not method invocation.

use std::collections::HashMap;
use std::process::Command;

use log::{debug, info, warn};

use super::FanController;
use crate::errors::FanControlError;
use crate::fan::{Fan, FanCurve, FanCurvePoint};

/// Fallback RPM range used when table data is unavailable.
const DEFAULT_MIN_RPM: u32 = 1600;
const DEFAULT_MAX_RPM: u32 = 4800;

/// Per-fan RPM range learned from table data.
#[derive(Debug, Clone)]
struct FanRpmRange {
    min_rpm: u32,
    max_rpm: u32,
}

// ---------------------------------------------------------------------------
// Pure parsing functions (no I/O — testable on any platform)
// ---------------------------------------------------------------------------

/// Parse a fan ID string like "fan0" or "fan1" into a numeric ID.
fn parse_fan_id(fan_id: &str) -> Result<u32, FanControlError> {
    fan_id
        .strip_prefix("fan")
        .and_then(|n| n.parse::<u32>().ok())
        .ok_or_else(|| FanControlError::FanNotFound(fan_id.to_string()))
}

/// Map PWM (0-255) to RPM using the given range.
fn pwm_to_rpm(min_rpm: u32, max_rpm: u32, pwm: u8) -> u32 {
    let ratio = pwm as f64 / 255.0;
    min_rpm + (ratio * (max_rpm - min_rpm) as f64) as u32
}

/// Map RPM back to approximate PWM (0-255) using the given range.
fn rpm_to_pwm(min_rpm: u32, max_rpm: u32, rpm: u32) -> u8 {
    if rpm <= min_rpm {
        return 0;
    }
    if rpm >= max_rpm {
        return 255;
    }
    let ratio = (rpm - min_rpm) as f64 / (max_rpm - min_rpm) as f64;
    (ratio * 255.0) as u8
}

/// Scan discover output for the FULLSPEED| line and return its value.
fn parse_fullspeed(output: &str) -> bool {
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("FULLSPEED|") {
            return value.trim() == "1";
        }
    }
    false
}

/// Parse a single `TABLE|...` line into a `FanCurve` and `FanRpmRange`.
///
/// Returns `None` if the line is malformed or too short.
fn parse_table_line(line: &str) -> Option<(FanCurve, FanRpmRange)> {
    let parts: Vec<&str> = line.split('|').collect();
    if parts.len() < 10 {
        return None;
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

    let range = FanRpmRange {
        min_rpm: min_speed,
        max_rpm: max_speed,
    };

    Some((curve, range))
}

/// Parse a single `FAN|...` line into a `Fan` struct.
///
/// Uses the provided RPM ranges and curve data. Returns `None` if malformed.
fn parse_fan_line(
    line: &str,
    rpm_ranges: &HashMap<u32, FanRpmRange>,
    curves_by_fan: &mut HashMap<u32, Vec<FanCurve>>,
    full_speed_active: bool,
) -> Option<Fan> {
    let parts: Vec<&str> = line.split('|').collect();
    if parts.len() < 5 {
        return None;
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
    let (min_rpm, max_rpm) = match range {
        Some(r) => (r.min_rpm, r.max_rpm),
        None => (DEFAULT_MIN_RPM, DEFAULT_MAX_RPM),
    };
    let curves = curves_by_fan.remove(&fan_id).unwrap_or_default();

    Some(Fan {
        id: format!("fan{fan_id}"),
        label: format!("{label} ({temp}\u{00B0}C)"),
        speed_rpm,
        pwm: Some(rpm_to_pwm(min_rpm, max_rpm, speed_rpm)),
        controllable: true,
        min_rpm: range.map(|r| r.min_rpm),
        max_rpm: range.map(|r| r.max_rpm),
        curves,
        full_speed_active,
    })
}

// ---------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------

/// Lenovo Legion fan controller backed by vendor-specific WMI classes.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub struct LenovoFanController {
    /// Per-fan RPM ranges, populated on first discover().
    fan_ranges: std::cell::RefCell<HashMap<u32, FanRpmRange>>,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
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

    /// Resolve RPM range for a fan, falling back to defaults.
    fn fan_rpm_range(&self, fan_numeric_id: u32) -> (u32, u32) {
        let ranges = self.fan_ranges.borrow();
        match ranges.get(&fan_numeric_id) {
            Some(range) => (range.min_rpm, range.max_rpm),
            None => (DEFAULT_MIN_RPM, DEFAULT_MAX_RPM),
        }
    }
}

impl FanController for LenovoFanController {
    fn discover(&self) -> Result<Vec<Fan>, FanControlError> {
        // Single PowerShell invocation: discover fans, read speeds, temps,
        // full fan table data (curves + RPM ranges), and full speed status.
        //
        // Output format:
        //   FULLSPEED|0/1
        //   FAN|fan_id|sensor_id|speed|temp          — one per fan (best sensor)
        //   TABLE|fan_id|sensor_id|active|min_speed|max_speed|min_temp|max_temp|speeds_csv|temps_csv
        let script =
            "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
             $tables = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_TABLE_DATA; \
             $fs = ($fm.Fan_Get_FullSpeed()).Status; \
             $fsVal = if ($fs) { '1' } else { '0' }; \
             Write-Output \"FULLSPEED|$fsVal\"; \
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

        let full_speed_active = parse_fullspeed(&output);
        debug!("full_speed_active = {full_speed_active}");

        // First pass: parse TABLE lines to build curves and RPM ranges.
        let mut curves_by_fan: HashMap<u32, Vec<FanCurve>> = HashMap::new();
        let mut rpm_ranges: HashMap<u32, FanRpmRange> = HashMap::new();

        for line in output.lines() {
            if !line.starts_with("TABLE|") {
                continue;
            }
            let Some((curve, range)) = parse_table_line(line) else {
                warn!("TABLE line too short: {line}");
                continue;
            };

            let fan_id = curve.fan_id;
            debug!(
                "TABLE: fan={} sensor={} active={} speed={}-{} temp={}-{} points={}",
                curve.fan_id,
                curve.sensor_id,
                curve.active,
                curve.min_speed,
                curve.max_speed,
                curve.min_temp,
                curve.max_temp,
                curve.points.len()
            );

            curves_by_fan.entry(fan_id).or_default().push(curve);

            // Update per-fan RPM range (take the widest range across curves).
            let existing = rpm_ranges.entry(fan_id).or_insert(range.clone());
            if range.min_rpm < existing.min_rpm {
                existing.min_rpm = range.min_rpm;
            }
            if range.max_rpm > existing.max_rpm {
                existing.max_rpm = range.max_rpm;
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
            if let Some(fan) =
                parse_fan_line(line, &rpm_ranges, &mut curves_by_fan, full_speed_active)
            {
                fans.push(fan);
            }
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
            let script = "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
                 $fm.Fan_Set_FullSpeed(1)";
            Self::ps_command(script)?;
        } else if pwm == 0 {
            info!("set_pwm({fan_id}, 0) -> Fan_Set_FullSpeed(0) [auto]");
            let script = "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
                 $fm.Fan_Set_FullSpeed(0)";
            Self::ps_command(script)?;
        } else {
            let (min_rpm, max_rpm) = self.fan_rpm_range(numeric_id);
            let target_rpm = pwm_to_rpm(min_rpm, max_rpm, pwm);
            info!("set_pwm({fan_id}, {pwm}) -> Fan_SetCurrentFanSpeed({numeric_id}, {target_rpm})");
            let script = format!(
                "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
                 $fm.Fan_SetCurrentFanSpeed({numeric_id}, {target_rpm})"
            );
            Self::ps_command(&script)?;
        }

        Ok(())
    }

    fn get_fan_curves(&self) -> Result<Vec<FanCurve>, FanControlError> {
        // Dedicated query for just the table data (no speed/temp reads).
        let script = "$tables = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_TABLE_DATA; \
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
            // get_fan_curves output has no TABLE| prefix — parts start at index 0.
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

    fn set_fan_curve(&self, curve: &FanCurve) -> Result<(), FanControlError> {
        use super::validate_curve;

        validate_curve(curve)?;

        let speeds: Vec<String> = curve
            .points
            .iter()
            .map(|p| p.fan_speed.to_string())
            .collect();
        let temps: Vec<String> = curve
            .points
            .iter()
            .map(|p| p.temperature.to_string())
            .collect();

        let speeds_csv = speeds.join(",");
        let temps_csv = temps.join(",");

        info!(
            "set_fan_curve: fan={} sensor={} speeds=[{}] temps=[{}]",
            curve.fan_id, curve.sensor_id, speeds_csv, temps_csv
        );

        // Build a PowerShell script that writes the fan curve via
        // Fan_Set_Table. The method takes (Fan_ID, Sensor_ID, FanTable_Data,
        // SensorTable_Data) as byte arrays matching LENOVO_FAN_TABLE_DATA.
        let script = format!(
            "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
             $speeds = @({speeds_csv}); \
             $temps = @({temps_csv}); \
             $fm.Fan_Set_Table({fan_id}, {sensor_id}, $speeds, $temps)",
            fan_id = curve.fan_id,
            sensor_id = curve.sensor_id,
        );

        Self::ps_command(&script)?;
        info!(
            "set_fan_curve: successfully wrote curve for fan {} sensor {}",
            curve.fan_id, curve.sensor_id
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests — pure parsing functions, runnable on any platform (no WMI needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_fan_id -------------------------------------------------------

    #[test]
    fn parse_fan_id_valid() {
        assert_eq!(parse_fan_id("fan0").unwrap(), 0);
        assert_eq!(parse_fan_id("fan1").unwrap(), 1);
        assert_eq!(parse_fan_id("fan99").unwrap(), 99);
    }

    #[test]
    fn parse_fan_id_invalid() {
        assert!(parse_fan_id("hwmon0").is_err());
        assert!(parse_fan_id("fan").is_err());
        assert!(parse_fan_id("").is_err());
        assert!(parse_fan_id("fan-1").is_err());
        assert!(parse_fan_id("Fan0").is_err());
    }

    // -- pwm_to_rpm / rpm_to_pwm -------------------------------------------

    #[test]
    fn pwm_to_rpm_boundaries() {
        // PWM 0 → min RPM
        assert_eq!(pwm_to_rpm(1600, 4800, 0), 1600);
        // PWM 255 → max RPM
        assert_eq!(pwm_to_rpm(1600, 4800, 255), 4800);
    }

    #[test]
    fn pwm_to_rpm_midrange() {
        // PWM 128 ≈ mid-range
        let mid = pwm_to_rpm(1600, 4800, 128);
        assert!(mid > 1600 && mid < 4800, "mid was {mid}");
    }

    #[test]
    fn pwm_to_rpm_custom_range() {
        assert_eq!(pwm_to_rpm(2000, 5400, 0), 2000);
        assert_eq!(pwm_to_rpm(2000, 5400, 255), 5400);
    }

    #[test]
    fn rpm_to_pwm_boundaries() {
        // At or below min → 0
        assert_eq!(rpm_to_pwm(1600, 4800, 1600), 0);
        assert_eq!(rpm_to_pwm(1600, 4800, 0), 0);
        // At or above max → 255
        assert_eq!(rpm_to_pwm(1600, 4800, 4800), 255);
        assert_eq!(rpm_to_pwm(1600, 4800, 9999), 255);
    }

    #[test]
    fn rpm_to_pwm_midrange() {
        let mid_rpm = 3200; // exactly halfway in 1600..4800
        let pwm = rpm_to_pwm(1600, 4800, mid_rpm);
        assert!(pwm > 100 && pwm < 160, "pwm was {pwm}");
    }

    #[test]
    fn pwm_rpm_roundtrip() {
        // pwm → rpm → pwm should be close to the original
        let original_pwm: u8 = 100;
        let rpm = pwm_to_rpm(1600, 4800, original_pwm);
        let recovered_pwm = rpm_to_pwm(1600, 4800, rpm);
        let diff = (original_pwm as i16 - recovered_pwm as i16).unsigned_abs();
        assert!(
            diff <= 1,
            "original={original_pwm} recovered={recovered_pwm}"
        );
    }

    // -- parse_fullspeed ----------------------------------------------------

    #[test]
    fn parse_fullspeed_active() {
        assert!(parse_fullspeed("FULLSPEED|1\nFAN|0|3|2100|45"));
    }

    #[test]
    fn parse_fullspeed_inactive() {
        assert!(!parse_fullspeed("FULLSPEED|0\nFAN|0|3|2100|45"));
    }

    #[test]
    fn parse_fullspeed_missing() {
        assert!(!parse_fullspeed("FAN|0|3|2100|45"));
    }

    // -- parse_table_line ---------------------------------------------------

    #[test]
    fn parse_table_line_valid() {
        let line = "TABLE|0|3|1|1600|4800|58|100|1600,2100,2700,3400,4200,4800|58,63,68,73,85,100";
        let (curve, range) = parse_table_line(line).expect("should parse");
        assert_eq!(curve.fan_id, 0);
        assert_eq!(curve.sensor_id, 3);
        assert!(curve.active);
        assert_eq!(curve.min_speed, 1600);
        assert_eq!(curve.max_speed, 4800);
        assert_eq!(curve.min_temp, 58);
        assert_eq!(curve.max_temp, 100);
        assert_eq!(curve.points.len(), 6);
        assert_eq!(curve.points[0].temperature, 58);
        assert_eq!(curve.points[0].fan_speed, 1600);
        assert_eq!(curve.points[5].temperature, 100);
        assert_eq!(curve.points[5].fan_speed, 4800);
        assert_eq!(range.min_rpm, 1600);
        assert_eq!(range.max_rpm, 4800);
    }

    #[test]
    fn parse_table_line_inactive() {
        let line = "TABLE|1|4|0|1800|4800|63|95|1800,2400,3200,4800|63,73,85,95";
        let (curve, _) = parse_table_line(line).expect("should parse");
        assert_eq!(curve.fan_id, 1);
        assert!(!curve.active);
        assert_eq!(curve.points.len(), 4);
    }

    #[test]
    fn parse_table_line_too_short() {
        assert!(parse_table_line("TABLE|0|3|1|1600").is_none());
        assert!(parse_table_line("").is_none());
    }

    // -- parse_fan_line -----------------------------------------------------

    #[test]
    fn parse_fan_line_valid() {
        let line = "FAN|0|3|2100|45";
        let mut ranges = HashMap::new();
        ranges.insert(
            0,
            FanRpmRange {
                min_rpm: 1600,
                max_rpm: 4800,
            },
        );
        let mut curves = HashMap::new();

        let fan = parse_fan_line(line, &ranges, &mut curves, false).expect("should parse");
        assert_eq!(fan.id, "fan0");
        assert!(fan.label.contains("CPU Fan"));
        assert!(fan.label.contains("45"));
        assert_eq!(fan.speed_rpm, 2100);
        assert!(fan.pwm.is_some());
        assert!(fan.controllable);
        assert!(!fan.full_speed_active);
        assert_eq!(fan.min_rpm, Some(1600));
        assert_eq!(fan.max_rpm, Some(4800));
    }

    #[test]
    fn parse_fan_line_gpu() {
        let line = "FAN|1|4|3200|52";
        let ranges = HashMap::new();
        let mut curves = HashMap::new();

        let fan = parse_fan_line(line, &ranges, &mut curves, true).expect("should parse");
        assert_eq!(fan.id, "fan1");
        assert!(fan.label.contains("GPU Fan"));
        assert!(fan.full_speed_active);
        // No range data → defaults used, no min/max reported
        assert_eq!(fan.min_rpm, None);
        assert_eq!(fan.max_rpm, None);
    }

    #[test]
    fn parse_fan_line_too_short() {
        let ranges = HashMap::new();
        let mut curves = HashMap::new();
        assert!(parse_fan_line("FAN|0|3", &ranges, &mut curves, false).is_none());
        assert!(parse_fan_line("", &ranges, &mut curves, false).is_none());
    }

    // -- integration: full discover output ----------------------------------

    #[test]
    fn parse_full_discover_output() {
        let output = "\
FULLSPEED|0
TABLE|0|3|1|1600|4800|58|100|1600,2100,2700,3400,4200,4800|58,63,68,73,85,100
TABLE|0|0|0|1600|4800|58|100|1600,2100,2700,3400,4200,4800|58,63,68,73,85,100
TABLE|1|4|1|1800|4800|63|95|1800,2400,3200,4800|63,73,85,95
FAN|0|3|2100|45
FAN|1|4|0|31";

        let full_speed = parse_fullspeed(output);
        assert!(!full_speed);

        let mut curves_by_fan: HashMap<u32, Vec<FanCurve>> = HashMap::new();
        let mut rpm_ranges: HashMap<u32, FanRpmRange> = HashMap::new();

        for line in output.lines() {
            if !line.starts_with("TABLE|") {
                continue;
            }
            let Some((curve, range)) = parse_table_line(line) else {
                continue;
            };
            let fan_id = curve.fan_id;
            curves_by_fan.entry(fan_id).or_default().push(curve);
            let existing = rpm_ranges.entry(fan_id).or_insert(range.clone());
            if range.min_rpm < existing.min_rpm {
                existing.min_rpm = range.min_rpm;
            }
            if range.max_rpm > existing.max_rpm {
                existing.max_rpm = range.max_rpm;
            }
        }

        // Fan 0 has 2 table entries, fan 1 has 1
        assert_eq!(curves_by_fan.get(&0).unwrap().len(), 2);
        assert_eq!(curves_by_fan.get(&1).unwrap().len(), 1);

        let mut fans = Vec::new();
        for line in output.lines() {
            if !line.starts_with("FAN|") {
                continue;
            }
            if let Some(fan) = parse_fan_line(line, &rpm_ranges, &mut curves_by_fan, full_speed) {
                fans.push(fan);
            }
        }

        assert_eq!(fans.len(), 2);
        assert_eq!(fans[0].id, "fan0");
        assert_eq!(fans[0].speed_rpm, 2100);
        assert_eq!(fans[0].curves.len(), 2);
        assert_eq!(fans[1].id, "fan1");
        assert_eq!(fans[1].speed_rpm, 0);
        assert_eq!(fans[1].curves.len(), 1);
    }
}
