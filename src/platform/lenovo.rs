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
use crate::fan::{CustomFanCurve, Fan, FanCurve, FanCurvePoint, MAX_STEP_VALUE};

/// Last-resort RPM range, used only when a fan has no table entry at all.
///
/// These are the Legion 82RG's measured values. Every other model reaches them
/// only if its firmware reports neither `CurrentFanMinSpeed`/`CurrentFanMaxSpeed`
/// nor any `FanTable_Data`, which is logged when it happens.
const DEFAULT_MIN_RPM: u32 = 1600;
const DEFAULT_MAX_RPM: u32 = 4800;

/// Per-fan RPM range.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FanRpmRange {
    min_rpm: u32,
    max_rpm: u32,
}

/// One parsed `TABLE|` line: the curve plus both candidate RPM ranges.
///
/// The two ranges are kept apart because they mean different things.
/// `table_span` is the min/max of `FanTable_Data` — how far *this curve*
/// reaches. `firmware_range` is `CurrentFanMinSpeed`/`CurrentFanMaxSpeed` as
/// reported by `LENOVO_FAN_TABLE_DATA` — what *the fan* can do, which is the
/// honest input for PWM conversion. It is `None` on firmware that omits the
/// properties.
#[derive(Debug)]
struct TableEntry {
    curve: FanCurve,
    table_span: FanRpmRange,
    firmware_range: Option<FanRpmRange>,
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
///
/// A degenerate range (`max_rpm <= min_rpm`) yields `min_rpm` for every input
/// rather than panicking: `max_rpm - min_rpm` is u32 subtraction, and an
/// inverted range reaches this from a `TABLE|` line whose speed fields parse
/// unevenly — field 4 valid, field 5 not, giving `min > max`. There is no
/// proportional answer over an empty range, so the conservative end is the
/// honest one.
fn pwm_to_rpm(min_rpm: u32, max_rpm: u32, pwm: u8) -> u32 {
    if max_rpm <= min_rpm {
        return min_rpm;
    }
    let ratio = pwm as f64 / 255.0;
    min_rpm + (ratio * (max_rpm - min_rpm) as f64) as u32
}

/// Map RPM back to approximate PWM (0-255) using the given range.
///
/// Degenerate ranges are handled as in [`pwm_to_rpm`]. The `rpm <= min_rpm`
/// guard already returns before the subtraction when the range is inverted,
/// but the check is explicit so the function does not depend on that ordering
/// holding after a later edit.
fn rpm_to_pwm(min_rpm: u32, max_rpm: u32, rpm: u32) -> u8 {
    if max_rpm <= min_rpm {
        return 0;
    }
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

/// Parse a single `TABLE|...` line into a `TableEntry`.
///
/// Fields 10 and 11 carry the firmware-reported fan range and are optional:
/// older output and firmware without the properties simply end at field 9 or
/// leave the fields empty, which yields `firmware_range: None`.
///
/// Returns `None` if the line is malformed or too short.
fn parse_table_line(line: &str) -> Option<TableEntry> {
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

    let table_span = FanRpmRange {
        min_rpm: min_speed,
        max_rpm: max_speed,
    };

    // Both properties must parse for the pair to be usable — half a range is
    // worse than none, since the missing half would silently take a value from
    // a different source.
    let firmware_range = match (
        parts.get(10).and_then(|v| v.trim().parse::<u32>().ok()),
        parts.get(11).and_then(|v| v.trim().parse::<u32>().ok()),
    ) {
        (Some(min_rpm), Some(max_rpm)) if min_rpm < max_rpm => {
            Some(FanRpmRange { min_rpm, max_rpm })
        }
        _ => None,
    };

    Some(TableEntry {
        curve,
        table_span,
        firmware_range,
    })
}

/// Widen `slot` to cover `candidate`.
fn merge_range(slot: &mut FanRpmRange, candidate: &FanRpmRange) {
    slot.min_rpm = slot.min_rpm.min(candidate.min_rpm);
    slot.max_rpm = slot.max_rpm.max(candidate.max_rpm);
}

/// Aggregate per-fan RPM ranges from parsed table entries.
///
/// The two sources are tiered, never blended: if any entry for a fan carries a
/// firmware-reported range, only firmware ranges are merged for that fan, and a
/// table span may not widen it.
///
/// The reason is provenance, not a known failure. `CurrentFanMinSpeed` and
/// `CurrentFanMaxSpeed` are the firmware stating what the fan can do. A table
/// span is an inference from curve data about what one curve happens to reach,
/// and on a model whose curve does not span the fan's full range the two
/// legitimately differ — which is the whole point of reading the firmware
/// values. Merging them would yield a range that is neither source's claim.
///
/// Fans with no firmware range fall back to the span of their own table data,
/// which is still live and model-specific. `DEFAULT_MIN_RPM`/`DEFAULT_MAX_RPM`
/// are reached only by a fan with no table entry at all, handled by the caller.
fn build_fan_ranges(entries: &[TableEntry]) -> HashMap<u32, FanRpmRange> {
    let mut firmware: HashMap<u32, FanRpmRange> = HashMap::new();
    let mut spans: HashMap<u32, FanRpmRange> = HashMap::new();

    for entry in entries {
        let fan_id = entry.curve.fan_id;

        if let Some(range) = &entry.firmware_range {
            firmware
                .entry(fan_id)
                .and_modify(|slot| merge_range(slot, range))
                .or_insert_with(|| range.clone());
        }

        spans
            .entry(fan_id)
            .and_modify(|slot| merge_range(slot, &entry.table_span))
            .or_insert_with(|| entry.table_span.clone());
    }

    for (fan_id, range) in &firmware {
        debug!(
            "fan{fan_id}: RPM range {}-{} from CurrentFanMin/MaxSpeed",
            range.min_rpm, range.max_rpm
        );
    }

    let mut ranges = firmware;
    for (fan_id, span) in spans {
        if let std::collections::hash_map::Entry::Vacant(slot) = ranges.entry(fan_id) {
            debug!(
                "fan{fan_id}: firmware reports no CurrentFanMin/MaxSpeed, \
                 falling back to table span {}-{} RPM",
                span.min_rpm, span.max_rpm
            );
            slot.insert(span);
        }
    }

    ranges
}

/// Parse every `TABLE|` line of discover output into per-fan curves and ranges.
///
/// Shared by `discover()` and its tests so the aggregation is exercised as
/// written rather than as re-implemented.
fn parse_tables(output: &str) -> (HashMap<u32, Vec<FanCurve>>, HashMap<u32, FanRpmRange>) {
    let mut entries: Vec<TableEntry> = Vec::new();

    for line in output.lines() {
        if !line.starts_with("TABLE|") {
            continue;
        }
        let Some(entry) = parse_table_line(line) else {
            warn!("TABLE line too short: {line}");
            continue;
        };

        debug!(
            "TABLE: fan={} sensor={} active={} speed={}-{} temp={}-{} points={} firmware_range={:?}",
            entry.curve.fan_id,
            entry.curve.sensor_id,
            entry.curve.active,
            entry.curve.min_speed,
            entry.curve.max_speed,
            entry.curve.min_temp,
            entry.curve.max_temp,
            entry.curve.points.len(),
            entry.firmware_range,
        );
        entries.push(entry);
    }

    let ranges = build_fan_ranges(&entries);

    let mut curves_by_fan: HashMap<u32, Vec<FanCurve>> = HashMap::new();
    for entry in entries {
        curves_by_fan
            .entry(entry.curve.fan_id)
            .or_default()
            .push(entry.curve);
    }

    (curves_by_fan, ranges)
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
// Custom fan curve encoding and validation (pure — no I/O)
// ---------------------------------------------------------------------------

/// Size of the Fan_Set_Table byte buffer.
const FAN_TABLE_BUFFER_SIZE: usize = 64;

/// Encode a CustomFanCurve into the 64-byte array expected by Fan_Set_Table.
///
/// Layout:
///   [0]     FSTM = 1 (write mode)
///   [1]     FSID = 0 (sensor set ID)
///   [2..6]  FSTL = 0x00000000 (uint32 LE, always zero)
///   [6..26] FSS0–FSS9: 10 × uint16 LE speed step indices
///   [26..64] zero padding
fn encode_fan_table_bytes(curve: &CustomFanCurve) -> [u8; FAN_TABLE_BUFFER_SIZE] {
    let mut bytes = [0u8; FAN_TABLE_BUFFER_SIZE];
    bytes[0] = 1; // FSTM: write mode
    bytes[1] = 0; // FSID: sensor set ID
                  // bytes[2..6] already zero (FSTL)
    for (i, &step) in curve.steps.iter().enumerate() {
        let offset = 6 + i * 2;
        let value = step as u16;
        bytes[offset] = (value & 0xFF) as u8;
        bytes[offset + 1] = (value >> 8) as u8;
    }
    bytes
}

/// Validate a custom curve's step values, enforcing safety constraints.
///
/// Rules:
///   - All steps must be in range 0–10
///   - Steps must be non-decreasing (no "death valley" curves)
///   - Step 7 must be ≥ 1 (upstream parity — see the caveat below)
///   - Step 8 must be ≥ 3 (high-temp safety minimum)
///   - Step 9 must be ≥ 5 (max-temp safety minimum)
///
/// Safety minimums match LenovoLegionToolkit's **GodMode V1** table,
/// `[0,0,0,0,0,0,0,1,3,5]`, element for element: V1's own floors for steps 0–6
/// are zero, so declining to floor them is V1 parity, not a third scheme.
///
/// LLT keeps a second, stricter table for GodMode V2 —
/// `[1,1,1,1,1,1,1,1,3,5]`, which forbids 0 anywhere — and selects between
/// them by SmartFan/LegionZone version. **The 82RG is V1**, measured for
/// issue #25 on 2026-08-12: `SmartFanVersion = 5` and `LegionZoneVersion = 2`
/// both land in V1's range independently, the power-mode mask `0x10007` has
/// bit 16 set so GodMode is supported, and BIOS `JUCN68WW` is outside LLT's
/// V1 blocklist. Taking V1's table is therefore the measured choice on this
/// hardware, not a permissive default.
///
/// It remains the safer choice on hardware this has not been measured on: if
/// such a machine turns out to be V2, the firmware rejects the curve and the
/// user sees an error at the WMI boundary, which is a better failure than
/// silently refusing curves the hardware would have accepted.
///
/// **What the step 7 floor does and does not claim.** Both tables require ≥ 1
/// at step 7, which is the whole justification for enforcing it now — it is
/// upstream parity under either. It is *not* known to be an off-versus-on
/// guarantee. Whether step value 0 means "fans off" or the lowest table entry
/// (~1600 RPM on the 82RG) is exactly the open question in issue #18, and this
/// crate documents both readings: `CustomFanCurve` describes steps as direct
/// indices where 0 → 1600 RPM, while `MAX_STEP_VALUE` of 10 makes an
/// eleven-value scale over a ten-entry table, which fits 0 = off. Do not read
/// a thermal guarantee into this floor until #18 settles it.
///
/// The non-decreasing rule is ours, not upstream's — LLT enforces no
/// monotonicity at all. It is kept as a deliberate safety choice.
pub(crate) fn validate_custom_curve(curve: &CustomFanCurve) -> Result<(), FanControlError> {
    for (i, &step) in curve.steps.iter().enumerate() {
        if step > MAX_STEP_VALUE {
            return Err(FanControlError::Platform(format!(
                "step {i} value {step} exceeds maximum {MAX_STEP_VALUE}"
            )));
        }
    }

    // Non-decreasing constraint
    for i in 1..10 {
        if curve.steps[i] < curve.steps[i - 1] {
            return Err(FanControlError::Platform(format!(
                "steps must be non-decreasing: step[{i}]={} < step[{}]={}",
                curve.steps[i],
                i - 1,
                curve.steps[i - 1]
            )));
        }
    }

    // High-temperature safety minimums
    if curve.steps[7] < 1 {
        return Err(FanControlError::Platform(format!(
            "step 7 (approaching high temp) must be >= 1 for safety, got {}",
            curve.steps[7]
        )));
    }
    if curve.steps[8] < 3 {
        return Err(FanControlError::Platform(format!(
            "step 8 (high temp) must be >= 3 for safety, got {}",
            curve.steps[8]
        )));
    }
    if curve.steps[9] < 5 {
        return Err(FanControlError::Platform(format!(
            "step 9 (max temp) must be >= 5 for safety, got {}",
            curve.steps[9]
        )));
    }

    Ok(())
}

/// Format a byte array as a PowerShell byte array literal: `@(1,0,0,...)`.
fn format_ps_byte_array(bytes: &[u8]) -> String {
    let values: Vec<String> = bytes.iter().map(|b| b.to_string()).collect();
    format!("@({})", values.join(","))
}

// ---------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------

/// Lenovo Legion fan controller backed by vendor-specific WMI classes.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub struct LenovoFanController {
    /// Per-fan RPM ranges, populated on first discover().
    fan_ranges: std::cell::RefCell<HashMap<u32, FanRpmRange>>,
    /// Whether the hardcoded-fallback warning has already been emitted.
    warned_default_range: std::cell::Cell<bool>,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl LenovoFanController {
    pub fn new() -> Self {
        Self {
            fan_ranges: std::cell::RefCell::new(HashMap::new()),
            warned_default_range: std::cell::Cell::new(false),
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
        //   TABLE|fan_id|sensor_id|active|min_speed|max_speed|min_temp|max_temp|speeds_csv|temps_csv|fan_min|fan_max
        //
        // The trailing fan_min/fan_max are CurrentFanMinSpeed/CurrentFanMaxSpeed,
        // read defensively: they are absent on some firmware and arrive empty.
        // DefaultFanMaxSpeed is deliberately not read — it does not exist on the
        // 82RG, so LLT's GetDefaultFanMaxSpeedAsync would fail here (see #25).
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
               $fanMin = ($t.Properties | Where-Object { $_.Name -eq 'CurrentFanMinSpeed' }).Value; \
               $fanMax = ($t.Properties | Where-Object { $_.Name -eq 'CurrentFanMaxSpeed' }).Value; \
               Write-Output \"TABLE|$fid|$sid|$active|$minSpd|$maxSpd|$minTmp|$maxTmp|$speeds|$temps|$fanMin|$fanMax\" \
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
        let (mut curves_by_fan, rpm_ranges) = parse_tables(&output);

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
                // `min_rpm: None` means no table entry matched this fan, so its
                // PWM conversion is running on the hardcoded 82RG constants.
                // Warned once per process rather than per poll: `discover()` is
                // called every 1.5s by the GUI worker.
                if fan.min_rpm.is_none() && !self.warned_default_range.replace(true) {
                    warn!(
                        "{}: no fan table entry; falling back to {DEFAULT_MIN_RPM}-{DEFAULT_MAX_RPM} RPM. \
                         PWM values will be approximate on this hardware.",
                        fan.id
                    );
                }
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

    fn set_custom_curve(&self, curve: &CustomFanCurve) -> Result<(), FanControlError> {
        validate_custom_curve(curve)?;

        // Ensure SmartFanMode is set to Custom (255) — required for Fan_Set_Table.
        // Mode values: 1=Quiet, 2=Balanced, 3=Performance, 255=Custom.
        match self.get_smart_fan_mode()? {
            Some(255) => {
                debug!("SmartFanMode already Custom (255)");
            }
            Some(mode) => {
                warn!("SmartFanMode is {mode}, switching to Custom (255) for fan curve write");
                self.set_smart_fan_mode(255)?;
            }
            None => {
                warn!("Could not read SmartFanMode, attempting Fan_Set_Table anyway");
            }
        }

        let bytes = encode_fan_table_bytes(curve);
        let ps_array = format_ps_byte_array(&bytes);
        info!(
            "set_custom_curve: fan_id={} sensor_id={} steps={:?}",
            curve.fan_id, curve.sensor_id, curve.steps
        );

        let script = format!(
            "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
             [byte[]]$table = {ps_array}; \
             $fm.Fan_Set_Table($table)"
        );
        Self::ps_command(&script)?;
        info!("Fan_Set_Table called successfully");
        Ok(())
    }

    fn get_smart_fan_mode(&self) -> Result<Option<u32>, FanControlError> {
        let script = "$gz = Get-WmiObject -Namespace root/WMI -Class LENOVO_GAMEZONE_DATA; \
             $result = $gz.GetSmartFanMode(); \
             $result.Properties | ForEach-Object { \
               if ($_.Value -ne $null -and $_.Name -ne '__PATH' -and $_.Name -ne '__GENUS' -and \
                   $_.Name -ne '__CLASS' -and $_.Name -ne '__SUPERCLASS' -and \
                   $_.Name -ne '__DYNASTY' -and $_.Name -ne '__RELPATH' -and \
                   $_.Name -ne '__PROPERTY_COUNT' -and $_.Name -ne '__DERIVATION' -and \
                   $_.Name -ne '__SERVER' -and $_.Name -ne '__NAMESPACE') { \
                 Write-Output \"$($_.Name)|$($_.Value)\" \
               } \
             }";

        let output = Self::ps_command(script)?;
        // Parse "PropertyName|Value" lines to find the mode value
        for line in output.lines() {
            if let Some((name, value_str)) = line.split_once('|') {
                let name_lower = name.trim().to_lowercase();
                if name_lower == "mode" || name_lower == "data" || name_lower == "smartfanmode" {
                    if let Ok(value) = value_str.trim().parse::<u32>() {
                        debug!("SmartFanMode: {name}={value}");
                        return Ok(Some(value));
                    }
                }
            }
        }

        warn!("Could not determine SmartFanMode from output: {output}");
        Ok(None)
    }

    fn set_smart_fan_mode(&self, mode: u32) -> Result<(), FanControlError> {
        info!("set_smart_fan_mode({mode})");
        let script = format!(
            "$gz = Get-WmiObject -Namespace root/WMI -Class LENOVO_GAMEZONE_DATA; \
             $gz.SetSmartFanMode({mode})"
        );
        Self::ps_command(&script)?;
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
        let TableEntry {
            curve,
            table_span,
            firmware_range,
        } = parse_table_line(line).expect("should parse");
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
        assert_eq!(table_span.min_rpm, 1600);
        assert_eq!(table_span.max_rpm, 4800);
        // No trailing fields: firmware range is absent, not inferred.
        assert_eq!(firmware_range, None);
    }

    #[test]
    fn parse_table_line_inactive() {
        let line = "TABLE|1|4|0|1800|4800|63|95|1800,2400,3200,4800|63,73,85,95";
        let entry = parse_table_line(line).expect("should parse");
        assert_eq!(entry.curve.fan_id, 1);
        assert!(!entry.curve.active);
        assert_eq!(entry.curve.points.len(), 4);
    }

    #[test]
    fn parse_table_line_too_short() {
        assert!(parse_table_line("TABLE|0|3|1|1600").is_none());
        assert!(parse_table_line("").is_none());
    }

    // -- firmware-reported fan range (#30) ----------------------------------

    #[test]
    fn parse_table_line_reads_firmware_range() {
        // Measured 82RG shape: CurrentFanMinSpeed/CurrentFanMaxSpeed trailing.
        let line = "TABLE|0|3|1|1600|4800|58|100|1600,4800|58,100|1600|4800";
        let entry = parse_table_line(line).expect("should parse");
        assert_eq!(
            entry.firmware_range,
            Some(FanRpmRange {
                min_rpm: 1600,
                max_rpm: 4800
            })
        );
    }

    #[test]
    fn parse_table_line_firmware_range_absent() {
        // Firmware without the properties: PowerShell interpolates $null as an
        // empty string, so the fields are present but blank. This is the
        // fallback path -- it must stay exercised.
        let line = "TABLE|0|3|1|1600|4800|58|100|1600,4800|58,100||";
        let entry = parse_table_line(line).expect("should parse");
        assert_eq!(entry.firmware_range, None);
        assert_eq!(entry.table_span.min_rpm, 1600);
    }

    #[test]
    fn parse_table_line_firmware_range_partial_is_rejected() {
        // Half a range would silently take its other half from another source.
        let with_min = "TABLE|0|3|1|1600|4800|58|100|1600,4800|58,100|1600|";
        let with_max = "TABLE|0|3|1|1600|4800|58|100|1600,4800|58,100||4800";
        assert_eq!(
            parse_table_line(with_min)
                .expect("should parse")
                .firmware_range,
            None
        );
        assert_eq!(
            parse_table_line(with_max)
                .expect("should parse")
                .firmware_range,
            None
        );
    }

    #[test]
    fn parse_table_line_firmware_range_inverted_is_rejected() {
        // min >= max would make rpm_to_pwm divide by zero or underflow.
        let line = "TABLE|0|3|1|1600|4800|58|100|1600,4800|58,100|4800|4800";
        assert_eq!(
            parse_table_line(line).expect("should parse").firmware_range,
            None
        );
    }

    #[test]
    fn conversions_survive_an_inverted_range() {
        // A TABLE| line whose speed fields parse unevenly -- field 4 valid,
        // field 5 not -- produces min=4800, max=0. Before the guard this
        // panicked with "attempt to subtract with overflow" at the u32
        // subtraction in pwm_to_rpm, reached through build_fan_ranges.
        let line = "TABLE|0|3|1|4800|notanumber|58|100|1600,4800|58,100||";
        let entry = parse_table_line(line).expect("should parse");
        assert_eq!(entry.table_span.min_rpm, 4800);
        assert_eq!(entry.table_span.max_rpm, 0);

        let ranges = build_fan_ranges(&[entry]);
        let range = ranges.get(&0).expect("fan 0 present");

        assert_eq!(pwm_to_rpm(range.min_rpm, range.max_rpm, 128), 4800);
        assert_eq!(pwm_to_rpm(range.min_rpm, range.max_rpm, 255), 4800);
        assert_eq!(rpm_to_pwm(range.min_rpm, range.max_rpm, 2000), 0);
    }

    #[test]
    fn conversions_survive_a_zero_width_range() {
        // Both speed fields unparseable gives min == max == 0, the shape an
        // empty FanTable_Data would produce via Measure-Object.
        assert_eq!(pwm_to_rpm(0, 0, 200), 0);
        assert_eq!(rpm_to_pwm(0, 0, 2000), 0);
        assert_eq!(pwm_to_rpm(1600, 1600, 200), 1600);
        assert_eq!(rpm_to_pwm(1600, 1600, 2000), 0);
    }

    #[test]
    fn parse_table_line_measured_82rg() {
        // Captured verbatim from a Legion 82RG on 2026-08-16 by running the
        // discover script's TABLE| fragment elevated against real firmware.
        // Pins the shape this parser is written against, so a future change to
        // the PowerShell side that alters the line is caught here.
        let measured = "TABLE|0|3|1|1600|4800|58|95|\
1600,1800,2000,2200,2800,3400,3700,4200,4400,4800|58,58,58,58,67,73,80,84,86,95|1600|4800";
        let entry = parse_table_line(measured).expect("should parse");

        assert_eq!(entry.curve.fan_id, 0);
        assert_eq!(entry.curve.sensor_id, 3);
        assert_eq!(entry.curve.points.len(), 10);
        assert_eq!(
            entry.firmware_range,
            Some(FanRpmRange {
                min_rpm: 1600,
                max_rpm: 4800
            })
        );
    }

    #[test]
    fn build_fan_ranges_prefers_firmware_over_table_span() {
        // A curve that does not reach the fan's floor must not lower the range
        // the firmware itself reported. Modelled here as a span starting at 0,
        // the widest possible disagreement between the two sources.
        let output = "\
TABLE|0|3|1|0|3000|58|100|0,3000|58,100|1600|4800
TABLE|0|0|0|0|2000|58|100|0,2000|58,100|1600|4800";
        let (_, ranges) = parse_tables(output);
        assert_eq!(
            ranges.get(&0),
            Some(&FanRpmRange {
                min_rpm: 1600,
                max_rpm: 4800
            })
        );
    }

    #[test]
    fn build_fan_ranges_mixed_provenance_ignores_spans() {
        // One entry for fan 0 reports the properties and one does not. The
        // span from the second must not drag the merged range outward.
        let output = "\
TABLE|0|3|1|1200|5400|58|100|1200,5400|58,100||
TABLE|0|0|0|1600|4800|58|100|1600,4800|58,100|1600|4800";
        let (_, ranges) = parse_tables(output);
        assert_eq!(
            ranges.get(&0),
            Some(&FanRpmRange {
                min_rpm: 1600,
                max_rpm: 4800
            })
        );
    }

    #[test]
    fn build_fan_ranges_falls_back_to_table_span() {
        // No firmware properties anywhere: the widest table span wins, which is
        // still live data and beats the hardcoded 82RG constants.
        let output = "\
TABLE|1|4|1|1800|4800|63|95|1800,4800|63,95||
TABLE|1|0|0|1500|5000|63|95|1500,5000|63,95||";
        let (_, ranges) = parse_tables(output);
        assert_eq!(
            ranges.get(&1),
            Some(&FanRpmRange {
                min_rpm: 1500,
                max_rpm: 5000
            })
        );
    }

    #[test]
    fn parse_fan_line_without_table_entry_uses_constants() {
        // A fan with no TABLE line at all: reports no range, and PWM is derived
        // from DEFAULT_MIN_RPM/DEFAULT_MAX_RPM. discover() warns on this path.
        let ranges: HashMap<u32, FanRpmRange> = HashMap::new();
        let mut curves: HashMap<u32, Vec<FanCurve>> = HashMap::new();
        let fan =
            parse_fan_line("FAN|0|3|1600|45", &ranges, &mut curves, false).expect("should parse");
        assert_eq!(fan.min_rpm, None);
        assert_eq!(fan.max_rpm, None);
        assert_eq!(
            fan.pwm,
            Some(rpm_to_pwm(DEFAULT_MIN_RPM, DEFAULT_MAX_RPM, 1600))
        );
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

    // -- encode_fan_table_bytes ---------------------------------------------

    #[test]
    fn encode_fan_table_bytes_header() {
        let curve = CustomFanCurve {
            fan_id: 0,
            sensor_id: 3,
            steps: [0, 0, 0, 0, 0, 0, 0, 0, 3, 5],
        };
        let bytes = encode_fan_table_bytes(&curve);
        assert_eq!(bytes[0], 1, "FSTM should be 1");
        assert_eq!(bytes[1], 0, "FSID should be 0");
        assert_eq!(&bytes[2..6], &[0, 0, 0, 0], "FSTL should be zero");
    }

    #[test]
    fn encode_fan_table_bytes_step_values() {
        let curve = CustomFanCurve {
            fan_id: 0,
            sensor_id: 3,
            steps: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        };
        let bytes = encode_fan_table_bytes(&curve);
        // Each step is encoded as uint16 LE starting at offset 6
        for i in 0..10 {
            let offset = 6 + i * 2;
            let value = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
            assert_eq!(value, (i + 1) as u16, "FSS{i} should be {}", i + 1);
        }
    }

    #[test]
    fn encode_fan_table_bytes_padding_is_zero() {
        let curve = CustomFanCurve {
            fan_id: 0,
            sensor_id: 3,
            steps: [10, 10, 10, 10, 10, 10, 10, 10, 10, 10],
        };
        let bytes = encode_fan_table_bytes(&curve);
        assert_eq!(bytes.len(), 64);
        // Bytes 26..64 should all be zero
        for (i, &byte) in bytes[26..64].iter().enumerate() {
            assert_eq!(byte, 0, "padding byte at offset {} should be 0", 26 + i);
        }
    }

    #[test]
    fn encode_fan_table_bytes_all_zeros() {
        let curve = CustomFanCurve {
            fan_id: 0,
            sensor_id: 0,
            steps: [0; 10],
        };
        let bytes = encode_fan_table_bytes(&curve);
        assert_eq!(bytes[0], 1, "FSTM always 1");
        for i in 0..10 {
            let offset = 6 + i * 2;
            assert_eq!(bytes[offset], 0);
            assert_eq!(bytes[offset + 1], 0);
        }
    }

    #[test]
    fn encode_fan_table_bytes_max_step_value() {
        let curve = CustomFanCurve {
            fan_id: 1,
            sensor_id: 4,
            steps: [10; 10],
        };
        let bytes = encode_fan_table_bytes(&curve);
        for i in 0..10 {
            let offset = 6 + i * 2;
            let value = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
            assert_eq!(value, 10);
        }
    }

    // -- validate_custom_curve -----------------------------------------------

    #[test]
    fn validate_custom_curve_llt_v2_minimum() {
        let curve = CustomFanCurve {
            fan_id: 0,
            sensor_id: 3,
            steps: [1, 1, 1, 1, 1, 1, 1, 1, 3, 5],
        };
        assert!(validate_custom_curve(&curve).is_ok());
    }

    #[test]
    fn validate_custom_curve_all_max() {
        let curve = CustomFanCurve {
            fan_id: 0,
            sensor_id: 3,
            steps: [10; 10],
        };
        assert!(validate_custom_curve(&curve).is_ok());
    }

    #[test]
    fn validate_custom_curve_ascending() {
        let curve = CustomFanCurve {
            fan_id: 0,
            sensor_id: 3,
            steps: [0, 1, 2, 3, 4, 5, 6, 7, 8, 10],
        };
        assert!(validate_custom_curve(&curve).is_ok());
    }

    #[test]
    fn validate_custom_curve_flat_then_ramp() {
        // Steps 0–6 may sit at 0 (fans off at idle), but step 7 now carries a
        // floor of 1 per LLT's GodMode V1 minimum table.
        let curve = CustomFanCurve {
            fan_id: 0,
            sensor_id: 3,
            steps: [0, 0, 0, 0, 0, 0, 0, 1, 5, 10],
        };
        assert!(validate_custom_curve(&curve).is_ok());
    }

    #[test]
    fn validate_custom_curve_step_exceeds_max() {
        let curve = CustomFanCurve {
            fan_id: 0,
            sensor_id: 3,
            steps: [1, 1, 1, 1, 1, 1, 1, 1, 3, 11],
        };
        let err = validate_custom_curve(&curve).unwrap_err();
        assert!(err.to_string().contains("exceeds maximum"));
    }

    #[test]
    fn validate_custom_curve_decreasing_steps() {
        let curve = CustomFanCurve {
            fan_id: 0,
            sensor_id: 3,
            steps: [5, 4, 3, 2, 1, 1, 1, 1, 3, 5],
        };
        let err = validate_custom_curve(&curve).unwrap_err();
        assert!(err.to_string().contains("non-decreasing"));
    }

    #[test]
    fn validate_custom_curve_step8_too_low() {
        // Step 7 is held at its floor so this isolates the step 8 violation.
        let curve = CustomFanCurve {
            fan_id: 0,
            sensor_id: 3,
            steps: [0, 0, 0, 0, 0, 0, 0, 1, 2, 5],
        };
        let err = validate_custom_curve(&curve).unwrap_err();
        assert!(err.to_string().contains("step 8"));
    }

    #[test]
    fn validate_custom_curve_step9_too_low() {
        // Step 7 is held at its floor so this isolates the step 9 violation.
        let curve = CustomFanCurve {
            fan_id: 0,
            sensor_id: 3,
            steps: [0, 0, 0, 0, 0, 0, 0, 1, 3, 4],
        };
        let err = validate_custom_curve(&curve).unwrap_err();
        assert!(err.to_string().contains("step 9"));
    }

    #[test]
    fn validate_custom_curve_single_decrease_at_end() {
        let curve = CustomFanCurve {
            fan_id: 0,
            sensor_id: 3,
            steps: [1, 2, 3, 4, 5, 6, 7, 8, 10, 9],
        };
        let err = validate_custom_curve(&curve).unwrap_err();
        assert!(err.to_string().contains("non-decreasing"));
    }

    // -- format_ps_byte_array ------------------------------------------------

    #[test]
    fn format_ps_byte_array_simple() {
        let bytes = [1u8, 0, 0, 0, 0, 0];
        assert_eq!(format_ps_byte_array(&bytes), "@(1,0,0,0,0,0)");
    }

    #[test]
    fn format_ps_byte_array_full_buffer() {
        let curve = CustomFanCurve {
            fan_id: 0,
            sensor_id: 3,
            steps: [1, 1, 1, 1, 1, 1, 1, 1, 3, 5],
        };
        let bytes = encode_fan_table_bytes(&curve);
        let ps = format_ps_byte_array(&bytes);
        assert!(ps.starts_with("@("));
        assert!(ps.ends_with(')'));
        // Should have 64 comma-separated values
        let values: Vec<&str> = ps
            .trim_start_matches("@(")
            .trim_end_matches(')')
            .split(',')
            .collect();
        assert_eq!(values.len(), 64);
    }

    // -- integration: full discover output ----------------------------------

    #[test]
    fn parse_full_discover_output() {
        // Shape as emitted by discover() on the 82RG, including the trailing
        // CurrentFanMinSpeed/CurrentFanMaxSpeed fields.
        let output = "\
FULLSPEED|0
TABLE|0|3|1|1600|4800|58|100|1600,2100,2700,3400,4200,4800|58,63,68,73,85,100|1600|4800
TABLE|0|0|0|1600|4800|58|100|1600,2100,2700,3400,4200,4800|58,63,68,73,85,100|1600|4800
TABLE|1|4|1|1800|4800|63|95|1800,2400,3200,4800|63,73,85,95|1600|4800
FAN|0|3|2100|45
FAN|1|4|0|31";

        let full_speed = parse_fullspeed(output);
        assert!(!full_speed);

        let (mut curves_by_fan, rpm_ranges) = parse_tables(output);

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

        // Fan 1's table span starts at 1800, but the firmware reports 1600 and
        // the firmware wins.
        assert_eq!(fans[1].min_rpm, Some(1600));
        assert_eq!(fans[1].max_rpm, Some(4800));
    }

    #[test]
    fn validate_custom_curve_step7_too_low() {
        // Step 7 is the lowest step carrying a floor. LLT's V1 and V2 minimum
        // tables disagree about steps 0–6 but both require >= 1 here, so this
        // bound holds whichever table the hardware falls under.
        let curve = CustomFanCurve {
            fan_id: 0,
            sensor_id: 3,
            steps: [0, 0, 0, 0, 0, 0, 0, 0, 3, 5],
        };
        let err = validate_custom_curve(&curve).unwrap_err();
        assert!(err.to_string().contains("step 7"));
    }
}
