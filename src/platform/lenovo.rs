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

use log::{debug, error, info, warn};

use super::FanController;
use crate::errors::FanControlError;
use crate::fan::{validate_custom_curve, CustomFanCurve, Fan, FanCurve, FanCurvePoint};

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

/// Parse the single integer `Current_State_Type` that the lighting read
/// prints. PowerShell writes the property bare (`2`), possibly with a trailing
/// newline; anything else (an error text that reached stdout, an empty
/// result from a firmware that lacks the id) is `None`.
fn parse_lighting_state(output: &str) -> Option<u32> {
    let mut lines = output.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next()?;
    if lines.next().is_some() {
        return None;
    }
    first.parse::<u32>().ok()
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

// ---------------------------------------------------------------------------
// Custom SmartFanMode entry, guarded
// ---------------------------------------------------------------------------

/// SmartFanMode value meaning "Custom", the mode `Fan_Set_Table` requires.
///
/// Re-exported from [`crate::fan::smart_fan_mode`] so this module reads
/// naturally; the definition lives there because the TUI needs it on platforms
/// where this backend is not compiled.
pub(crate) use crate::fan::smart_fan_mode::CUSTOM as SMART_FAN_MODE_CUSTOM;

/// Second choice when the previous mode cannot be restored after a failed
/// write: any mode other than Custom is safe, and this is the one that behaves
/// at every temperature.
pub(crate) use crate::fan::smart_fan_mode::SAFE_FALLBACK as SMART_FAN_MODE_SAFE_FALLBACK;

/// What one curve-write transaction reported about itself.
///
/// The PowerShell side emits tagged lines rather than throwing, so a failure
/// still returns its stdout. Throwing would make the exit code non-zero, and
/// [`LenovoFanController::ps_command`] discards stdout in that case — losing the
/// `RESTORED|` line in exactly the failure this exists to report.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CurveTransaction {
    /// Mode observed before anything was changed.
    pub prev_mode: Option<u32>,
    /// Mode read back after switching to Custom. `None` if never reported.
    pub mode_set: Option<u32>,
    /// `Some(true)` on a successful `Fan_Set_Table`, `Some(false)` on failure,
    /// `None` if the script did not get that far.
    pub table_write_ok: Option<bool>,
    /// The exception message behind a `Some(false)`, when the script had one.
    /// Any throw inside the `try` lands here, not only `Fan_Set_Table`'s, so
    /// the text says which step failed.
    pub table_write_error: Option<String>,
    /// Mode PowerShell restored in its `finally`, if it restored one.
    pub restored: Option<u32>,
    /// Mode observed last of all. The authoritative statement of where the
    /// machine was left.
    pub final_mode: Option<u32>,
}

impl CurveTransaction {
    /// True when the curve is in place *and active*: the write succeeded and,
    /// if the mode was read back at all, it read back as Custom.
    ///
    /// The script refuses to write when the read-back is not Custom, so this
    /// is belt and braces on the Rust side. A missing read-back does not block
    /// a commit: absence of that line is not evidence the switch failed, and
    /// letting it out-rank `TABLEWRITE|OK` would undo good writes on noise.
    pub fn committed(&self) -> bool {
        self.table_write_ok == Some(true)
            && !matches!(self.mode_set, Some(mode) if mode != SMART_FAN_MODE_CUSTOM)
    }

    /// True when the machine is known not to be sitting in Custom mode without a
    /// curve — either the write landed, or the mode was restored.
    ///
    /// Deliberately requires positive evidence. Missing or unparseable output
    /// returns false, so the Rust-side guard stays armed and restores.
    pub fn is_safe(&self) -> bool {
        if self.table_write_ok == Some(true) {
            return true;
        }
        match self.final_mode {
            Some(mode) => mode != SMART_FAN_MODE_CUSTOM,
            None => false,
        }
    }
}

/// Build the single PowerShell invocation that switches to Custom mode and
/// writes the table.
///
/// Extracted from its caller so the generated script can be parse-checked and
/// asserted on without hardware. It is one long semicolon-joined string built by
/// `format!`, where a syntax slip breaks every `set-curve` at runtime and
/// nowhere earlier.
fn build_curve_transaction_script(previous_mode: u32, ps_array: &str) -> String {
    format!(
        "$ErrorActionPreference = 'Stop'; \
             $gz = Get-WmiObject -Namespace root/WMI -Class LENOVO_GAMEZONE_DATA; \
             $fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
             $prev = {previous_mode}; \
             Write-Output \"PREVMODE|$prev\"; \
             $committed = $false; \
             try {{ \
               if ($prev -ne {custom}) {{ $gz.SetSmartFanMode({custom}) }}; \
               $now = ($gz.GetSmartFanMode()).Data; \
               Write-Output \"MODESET|$now\"; \
               if ($now -ne {custom}) {{ throw \"SmartFanMode read back $now after selecting {custom}\" }}; \
               [byte[]]$table = {ps_array}; \
               $fm.Fan_Set_Table($table); \
               Write-Output 'TABLEWRITE|OK'; \
               $committed = $true \
             }} catch {{ \
               Write-Output \"TABLEWRITE|ERR|$($_.Exception.Message)\" \
             }} finally {{ \
               if ((-not $committed) -and ($prev -ne {custom})) {{ \
                 try {{ $gz.SetSmartFanMode($prev); Write-Output \"RESTORED|$prev\" }} \
                 catch {{ Write-Output 'RESTORED|FAIL' }} \
               }}; \
               $f = ($gz.GetSmartFanMode()).Data; \
               Write-Output \"FINALMODE|$f\" \
             }}",
        custom = SMART_FAN_MODE_CUSTOM,
    )
}

/// Parse the tagged output of the curve-write transaction.
///
/// Unknown lines are ignored rather than rejected: PowerShell writes warnings
/// and progress records into the same stream, and a strict parser would fail on
/// noise that says nothing about the outcome.
pub(crate) fn parse_curve_transaction(stdout: &str) -> CurveTransaction {
    let mut tx = CurveTransaction::default();
    for line in stdout.lines() {
        let Some((tag, value)) = line.trim().split_once('|') else {
            continue;
        };
        let value = value.trim();
        match tag.trim() {
            "PREVMODE" => tx.prev_mode = value.parse().ok(),
            "MODESET" => tx.mode_set = value.parse().ok(),
            "TABLEWRITE" => {
                let ok = value == "OK";
                tx.table_write_ok = Some(ok);
                if !ok {
                    // "ERR|<message>" from the catch block; a bare "ERR" from
                    // an older script parses to no message rather than to the
                    // literal word.
                    tx.table_write_error = value
                        .strip_prefix("ERR|")
                        .filter(|m| !m.is_empty())
                        .map(str::to_string);
                }
            }
            // RESTORED|FAIL parses to None, which correctly reads as "no mode
            // was restored" rather than as a restore to some unknown mode.
            "RESTORED" => tx.restored = value.parse().ok(),
            "FINALMODE" => tx.final_mode = value.parse().ok(),
            _ => {}
        }
    }
    tx
}

/// The mode operations the Custom-mode guard needs.
///
/// A trait rather than direct calls so the guard is testable: the real
/// implementation shells out to PowerShell and needs Lenovo hardware, while the
/// behaviour worth testing — that a failed curve write cannot leave Custom mode
/// selected — is pure control flow.
pub(crate) trait SmartFanModeIo {
    fn read_mode(&self) -> Result<Option<u32>, FanControlError>;
    fn write_mode(&self, mode: u32) -> Result<(), FanControlError>;
    /// Last resort when no mode can be selected. `Fan_Set_FullSpeed(1)`
    /// overrides the curve and is confirmed working on this firmware. It is a
    /// different WMI method on the *same* PowerShell channel, so it survives a
    /// method-specific refusal but not a channel-level failure — and it masks
    /// the fans-off state rather than clearing it: the machine stays in Custom
    /// with no curve, and disabling full speed later would stop the fans.
    /// Every message on this path has to say so.
    fn emergency_full_speed(&self) -> Result<(), FanControlError>;
}

/// Restores the previous SmartFanMode on drop unless disarmed.
///
/// Why this is a guard and not a cleanup branch: Custom mode with no curve
/// loaded **stops the fans and keeps them stopped under load**. Measured on the
/// 82RG on 2026-08-19 — 0 RPM sustained across 61–67 °C with every thread
/// pinned, where the same machine held 2200 RPM in Performance. Entering Custom
/// mode and then failing to write a curve is therefore a thermal event, not a
/// failed operation, and every early return between those two points has to
/// unwind. A hand-written branch protects only the returns someone remembered.
pub(crate) struct CustomModeGuard<'a, T: SmartFanModeIo> {
    io: &'a T,
    pub(crate) restore_to: u32,
    armed: bool,
}

// The guard is only a guard because `Drop` runs on every early return and on
// panic. `panic = "abort"` would remove the panic half silently, so refuse to
// build that way rather than leave a comment and hope.
#[cfg(panic = "abort")]
compile_error!(
    "CustomModeGuard relies on unwinding; panic = \"abort\" disables the fans-off backstop"
);

/// What became of an attempt to leave Custom mode after a failed curve write.
///
/// Returned by [`CustomModeGuard::restore_now`] so the caller can put the
/// machine's *actual* final state into the error the user sees, rather than
/// the state the transaction reported before any restore ran.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RestoreOutcome {
    /// The previous mode is selected again.
    Restored(u32),
    /// The previous mode could not be selected; this one could. Out of Custom,
    /// which is what matters, but not where the user was.
    FellBackTo(u32),
    /// No mode could be selected. Full speed is engaged and the machine is
    /// still in Custom with no curve: disabling full speed would stop the fans.
    FullSpeedEngaged,
    /// Nothing worked. The machine is in Custom with no curve.
    Stranded,
}

impl std::fmt::Display for RestoreOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Restored(mode) => write!(f, "SmartFanMode restored to {mode}"),
            Self::FellBackTo(mode) => write!(
                f,
                "the previous SmartFanMode could not be restored; selected {mode} ({}) instead",
                crate::fan::smart_fan_mode_label(Some(*mode))
            ),
            Self::FullSpeedEngaged => write!(
                f,
                "no SmartFanMode could be selected, so FULL SPEED is engaged as a last resort. \
                 The machine is still in Custom mode with no curve: select another power mode \
                 (Fn+Q, or a successful set-curve) BEFORE disabling full speed, or the fans will stop"
            ),
            Self::Stranded => write!(
                f,
                "no SmartFanMode could be selected and full speed could not be engaged. \
                 THE FANS MAY BE STOPPED. Select another power mode now (Fn+Q)"
            ),
        }
    }
}

/// Select a mode, then confirm by reading back that the machine has left
/// Custom. Returns the mode read back, or `None` if anything short of that
/// positive evidence happened.
///
/// A write that returns success is not evidence on its own. This firmware
/// ignores `Fan_SetCurrentFanSpeed` without error, and the transaction's own
/// read-back check exists because `SetSmartFanMode` might do the same. A
/// ladder that trusted the write would report "restored" with the machine
/// still in Custom and no curve, in exactly the shape it was built to prevent.
fn select_and_confirm<T: SmartFanModeIo>(io: &T, mode: u32) -> Option<u32> {
    if let Err(e) = io.write_mode(mode) {
        error!("FAILED to select SmartFanMode {mode}: {e}");
        return None;
    }
    match io.read_mode() {
        Ok(Some(now)) if now != SMART_FAN_MODE_CUSTOM => {
            if now != mode {
                warn!("asked for SmartFanMode {mode}, read back {now}; out of Custom, which is what matters");
            }
            Some(now)
        }
        Ok(Some(now)) => {
            error!("SetSmartFanMode({mode}) returned success but the mode still reads {now}; not trusting it");
            None
        }
        Ok(None) => {
            error!("SetSmartFanMode({mode}) returned success but the mode could not be read back; not trusting it");
            None
        }
        Err(e) => {
            error!("SetSmartFanMode({mode}) returned success but the read-back failed: {e}; not trusting it");
            None
        }
    }
}

/// Leave Custom mode by whatever works, and say what worked.
///
/// One function for both the explicit path ([`CustomModeGuard::restore_now`])
/// and the `Drop` path, so the escalation ladder cannot drift between them:
/// the previous mode, then [`SMART_FAN_MODE_SAFE_FALLBACK`], then full speed.
/// Each rung is confirmed by read-back, never by the write's return value.
///
/// Cost, accepted against #45: every call here is an unbounded subprocess, so
/// a wedged WMI makes this ladder hang for up to five calls, on the worker
/// thread when reached from `Drop`. The extra rung lengthens a hang that is
/// already unbounded; it does not create one. A timeout belongs in
/// `ps_command`, once, not per rung.
fn restore_and_escalate<T: SmartFanModeIo>(io: &T, restore_to: u32) -> RestoreOutcome {
    warn!(
        "curve write did not complete; restoring SmartFanMode to {restore_to} so the fans are not left stopped"
    );
    if let Some(now) = select_and_confirm(io, restore_to) {
        info!("SmartFanMode restored to {now}");
        return RestoreOutcome::Restored(now);
    }

    if restore_to != SMART_FAN_MODE_SAFE_FALLBACK {
        // Second attempt at leaving Custom, aimed at the mode that behaves at
        // any temperature. Same channel as the write that just failed, so it
        // only helps with a transient or a mode-specific refusal -- but
        // leaving Custom by any route beats masking the state with noise.
        if let Some(now) = select_and_confirm(io, SMART_FAN_MODE_SAFE_FALLBACK) {
            let outcome = RestoreOutcome::FellBackTo(now);
            warn!("{outcome}");
            return outcome;
        }
    }

    // The write and every restore have failed: Custom mode with no curve, fans
    // off, possibly under load. Noise is the correct failure mode here. But it
    // masks the state rather than clearing it, and the log has to say so,
    // because Fan_Set_FullSpeed(0) is exactly what a user does to silence fans.
    match io.emergency_full_speed() {
        Ok(()) => {
            let outcome = RestoreOutcome::FullSpeedEngaged;
            error!("{outcome}");
            outcome
        }
        Err(e) => {
            let outcome = RestoreOutcome::Stranded;
            error!("could not engage full speed: {e}. {outcome}");
            outcome
        }
    }
}

impl<T: SmartFanModeIo> CustomModeGuard<'_, T> {
    /// Give up the restore, once the curve is safely in place.
    fn disarm(&mut self) {
        self.armed = false;
    }

    /// Restore now, and report what happened.
    ///
    /// For the path where the caller is about to build an error message: the
    /// message should describe where the machine *is*, which is only known
    /// after the restore has run. Disarms first, so the `Drop` that follows
    /// cannot fire the ladder a second time.
    fn restore_now(mut self) -> RestoreOutcome {
        self.armed = false;
        restore_and_escalate(self.io, self.restore_to)
    }
}

impl<T: SmartFanModeIo> Drop for CustomModeGuard<'_, T> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Early return or panic between arming and disarming. The outcome is
        // logged inside; there is nobody left to hand it to.
        let _ = restore_and_escalate(self.io, self.restore_to);
    }
}

/// Read the current mode and arm a guard against it, **without changing
/// anything**.
///
/// Read-only by design, and that is the point. The switch into Custom mode
/// belongs inside the write transaction, where PowerShell's `finally` can unwind
/// it. Doing the switch here instead would reopen the window this design exists
/// to close: the process could die between the switch and the transaction, with
/// nothing anywhere running to restore the mode. An earlier revision of this
/// function did exactly that.
///
/// Returns `None` when the machine is *already* in Custom mode. That case needs
/// no guard and must not get one: the fans-off state, if it exists, predates
/// this call, and "restoring" would mean inventing a mode the user never chose.
fn arm_custom_mode_guard<T: SmartFanModeIo>(
    io: &T,
) -> Result<Option<CustomModeGuard<'_, T>>, FanControlError> {
    match io.read_mode()? {
        Some(SMART_FAN_MODE_CUSTOM) => {
            debug!("SmartFanMode already Custom ({SMART_FAN_MODE_CUSTOM})");
            Ok(None)
        }
        Some(previous) => {
            debug!(
                "SmartFanMode is {previous}; the transaction will switch to Custom ({SMART_FAN_MODE_CUSTOM})"
            );
            Ok(Some(CustomModeGuard {
                io,
                restore_to: previous,
                armed: true,
            }))
        }
        None => {
            // Previously this warned and carried on. That is the worst available
            // option: it enters the fans-off state with no recorded mode to
            // return to, so neither the guard nor the user can undo it. Refusing
            // leaves the machine on its BIOS curve, which is always safe.
            Err(FanControlError::Platform(
                "cannot read SmartFanMode, so there is no mode to restore if the curve write \
                 fails; refusing to enter Custom mode (Custom with no curve stops the fans)"
                    .to_string(),
            ))
        }
    }
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
/// Consecutive lighting-read failures after which the read is suspended.
///
/// A firmware without `LENOVO_LIGHTING_METHOD` fails every time, and the TUI
/// and GUI poll every 1.5 s, so without this each poll would pay for a
/// PowerShell process that cannot succeed.
const LIGHTING_FAILURES_BEFORE_SUSPEND: u32 = 3;

/// While suspended, one read in this many is still attempted, so a transient
/// WMI failure does not disable the indicator for the rest of the process.
/// At the 1.5 s poll that is about one attempt a minute.
const LIGHTING_RETRY_EVERY: u32 = 40;

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub struct LenovoFanController {
    /// Per-fan RPM ranges, populated on first discover().
    fan_ranges: std::cell::RefCell<HashMap<u32, FanRpmRange>>,
    /// Whether the hardcoded-fallback warning has already been emitted.
    warned_default_range: std::cell::Cell<bool>,
    /// Consecutive failures of the lighting read; reset by a success.
    lighting_failures: std::cell::Cell<u32>,
    /// Reads skipped while the lighting read is suspended.
    lighting_skips: std::cell::Cell<u32>,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl LenovoFanController {
    pub fn new() -> Self {
        Self {
            fan_ranges: std::cell::RefCell::new(HashMap::new()),
            warned_default_range: std::cell::Cell::new(false),
            lighting_failures: std::cell::Cell::new(0),
            lighting_skips: std::cell::Cell::new(0),
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

    /// Count a lighting-read failure; warn once when the read is suspended.
    fn note_lighting_failure(&self, lighting_id: u32, reason: &str) {
        let failures = self.lighting_failures.get() + 1;
        self.lighting_failures.set(failures);
        if failures == LIGHTING_FAILURES_BEFORE_SUSPEND {
            warn!(
                "Lighting_Id {lighting_id} read failed {failures} times in a row ({reason}); \
                 suspending the read, retrying one in {LIGHTING_RETRY_EVERY}. The LED indicator \
                 falls back to the colour derived from SmartFanMode, labelled as derived."
            );
        } else {
            debug!("Lighting_Id {lighting_id} read failed ({failures}): {reason}");
        }
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

/// Bridges the guard's small trait onto the controller's real WMI calls.
impl SmartFanModeIo for LenovoFanController {
    fn read_mode(&self) -> Result<Option<u32>, FanControlError> {
        FanController::get_smart_fan_mode(self)
    }

    fn write_mode(&self, mode: u32) -> Result<(), FanControlError> {
        FanController::set_smart_fan_mode(self, mode)
    }

    fn emergency_full_speed(&self) -> Result<(), FanControlError> {
        let script = "$fm = Get-WmiObject -Namespace root/WMI -Class LENOVO_FAN_METHOD; \
             $fm.Fan_Set_FullSpeed(1)";
        Self::ps_command(script)?;
        Ok(())
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

        // Read the mode and arm the Rust-side guard. This does NOT change the
        // mode: the switch happens inside the transaction below, so the interval
        // during which the machine sits in Custom without a curve lies entirely
        // within one process that has a `finally`. The guard is the backstop for
        // what that cannot cover -- the subprocess failing to launch, dying
        // outright, or returning output we cannot read.
        let mut guard = arm_custom_mode_guard(self)?;
        let previous_mode = guard
            .as_ref()
            .map(|g| g.restore_to)
            .unwrap_or(SMART_FAN_MODE_CUSTOM);

        let bytes = encode_fan_table_bytes(curve);
        let ps_array = format_ps_byte_array(&bytes);
        info!(
            "set_custom_curve: fan_id={} sensor_id={} steps={:?}",
            curve.fan_id, curve.sensor_id, curve.steps
        );

        // Mode switch and table write in ONE invocation, so the interval during
        // which the machine sits in Custom mode with no curve is bounded by a
        // single process with a `finally` clause -- rather than spanning two
        // subprocess calls, where nothing at all runs if this process dies
        // between them.
        //
        // No throw escapes the script: the one inside its `try` is caught and
        // reported through the tags. An escaping throw would set a non-zero
        // exit code, and `ps_command` discards stdout in that case, which would
        // lose the very lines describing the failure.
        let script = build_curve_transaction_script(previous_mode, &ps_array);

        // If PowerShell itself fails -- no launch, or a non-zero exit because
        // something outside the try threw -- there are no tags to read. The
        // guard would restore on drop anyway; restoring explicitly here puts
        // the outcome into the error, as the tagged paths below do.
        let output = match Self::ps_command(&script) {
            Ok(output) => output,
            Err(e) => {
                let aftermath = match guard.take() {
                    Some(g) => format!("; {}", g.restore_now()),
                    None => String::new(),
                };
                return Err(FanControlError::Platform(format!(
                    "curve write transaction did not run to completion: {e}{aftermath}"
                )));
            }
        };
        let transaction = parse_curve_transaction(&output);
        debug!("curve transaction: {transaction:?}");

        // Disarm on positive evidence only. Otherwise restore *now*, through
        // the same ladder `Drop` uses, so the error below describes where the
        // machine ended up rather than where the transaction left it.
        let aftermath = if transaction.is_safe() {
            if let Some(g) = guard.as_mut() {
                g.disarm();
            }
            match (transaction.restored, transaction.final_mode) {
                (Some(restored), _) => {
                    format!("; the transaction restored SmartFanMode to {restored}")
                }
                (None, Some(final_mode)) => format!("; SmartFanMode is {final_mode}"),
                (None, None) => String::new(),
            }
        } else {
            match guard.take() {
                Some(g) => format!("; {}", g.restore_now()),
                None => {
                    // No guard because the machine was already in Custom before
                    // this call. Nothing was switched, so nothing is restored:
                    // the EC keeps running whatever table it last received,
                    // which may be a good curve or the fans-off state, and this
                    // code cannot tell which. Say so, rather than select a mode
                    // the user never chose on a possibly transient failure.
                    warn!(
                        "curve write failed with SmartFanMode already Custom; left as found. \
                         The EC runs the last table it received, which may stop the fans"
                    );
                    format!(
                        "; SmartFanMode was already Custom ({SMART_FAN_MODE_CUSTOM}) before this \
                         call and is left there. The EC runs the last table it received; select \
                         another power mode if the fans are stopped"
                    )
                }
            }
        };

        if transaction.committed() {
            info!("Fan_Set_Table committed; SmartFanMode is Custom with a curve loaded");
            return Ok(());
        }

        let reason = match (transaction.table_write_ok, transaction.mode_set) {
            // committed() already returned for Some(true) with a Custom or
            // missing read-back, so this arm is the wrong-mode case only.
            (Some(true), Some(mode)) => format!(
                "SmartFanMode read back {mode} after selecting Custom ({SMART_FAN_MODE_CUSTOM}); \
                 the table was written in the wrong mode and is not active"
            ),
            (Some(false), _) => match &transaction.table_write_error {
                // "curve write", not "Fan_Set_Table": the read-back check
                // throws into the same catch, and then the table method was
                // never reached. The message says which step threw.
                Some(message) => format!("curve write failed: {message}"),
                None => "curve write failed".to_string(),
            },
            _ => format!("curve write reported no outcome. Raw output: {output}"),
        };
        Err(FanControlError::Platform(format!("{reason}{aftermath}")))
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

    fn get_lighting_state(&self, lighting_id: u32) -> Result<Option<u32>, FanControlError> {
        // Suspended after repeated failures; still try one read in every
        // LIGHTING_RETRY_EVERY so a transient fault does not stick.
        if self.lighting_failures.get() >= LIGHTING_FAILURES_BEFORE_SUSPEND {
            let skips = self.lighting_skips.get() + 1;
            self.lighting_skips.set(skips);
            if !skips.is_multiple_of(LIGHTING_RETRY_EVERY) {
                return Ok(None);
            }
            debug!("lighting read suspended; retrying once (skip {skips})");
        }

        // The getter only. The setter, Set_Lighting_Current_Status, is never
        // called anywhere in this repository; a cargo test enforces that.
        let script = format!(
            "$lm = Get-WmiObject -Namespace root/WMI -Class LENOVO_LIGHTING_METHOD; \
             ($lm.Get_Lighting_Current_Status({lighting_id})).Current_State_Type"
        );
        match Self::ps_command(&script) {
            Ok(output) => match parse_lighting_state(&output) {
                Some(index) => {
                    self.lighting_failures.set(0);
                    debug!("Lighting_Id {lighting_id} Current_State_Type = {index}");
                    Ok(Some(index))
                }
                None => {
                    self.note_lighting_failure(
                        lighting_id,
                        &format!("unparseable output {output:?}"),
                    );
                    Ok(None)
                }
            },
            Err(e) => {
                self.note_lighting_failure(lighting_id, &e.to_string());
                Ok(None)
            }
        }
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
    use std::cell::{Cell, RefCell};

    // -- Custom-mode guard --------------------------------------------------
    //
    // These cover the hazard measured on 2026-08-19: Custom mode with no curve
    // loaded stops the fans and keeps them stopped under load. The property
    // under test is that no failure path can leave Custom mode selected without
    // a curve behind it.

    /// Records every mode operation, and can be told to fail any of them.
    ///
    /// A successful write updates `current`, so a read-back sees it, unless
    /// `writes_ignored` is set: then writes return `Ok` and change nothing,
    /// which is the silent-ignore shape this firmware shows elsewhere.
    struct FakeModeIo {
        current: Cell<Option<u32>>,
        calls: RefCell<Vec<String>>,
        write_fails: bool,
        /// Fail writes of this one mode only, so the fallback rung of the
        /// restore ladder can be reached without failing every write.
        write_fails_only_for: Option<u32>,
        /// Writes succeed but leave the mode unchanged.
        writes_ignored: bool,
        full_speed_fails: bool,
    }

    impl FakeModeIo {
        fn in_mode(mode: u32) -> Self {
            Self {
                current: Cell::new(Some(mode)),
                calls: RefCell::new(Vec::new()),
                write_fails: false,
                write_fails_only_for: None,
                writes_ignored: false,
                full_speed_fails: false,
            }
        }

        fn unreadable() -> Self {
            Self {
                current: Cell::new(None),
                calls: RefCell::new(Vec::new()),
                write_fails: false,
                write_fails_only_for: None,
                writes_ignored: false,
                full_speed_fails: false,
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl SmartFanModeIo for FakeModeIo {
        fn read_mode(&self) -> Result<Option<u32>, FanControlError> {
            self.calls.borrow_mut().push("read".to_string());
            Ok(self.current.get())
        }

        fn write_mode(&self, mode: u32) -> Result<(), FanControlError> {
            self.calls.borrow_mut().push(format!("write({mode})"));
            if self.write_fails || self.write_fails_only_for == Some(mode) {
                return Err(FanControlError::Platform("simulated failure".to_string()));
            }
            if !self.writes_ignored {
                self.current.set(Some(mode));
            }
            Ok(())
        }

        fn emergency_full_speed(&self) -> Result<(), FanControlError> {
            self.calls.borrow_mut().push("full_speed".to_string());
            if self.full_speed_fails {
                return Err(FanControlError::Platform("simulated failure".to_string()));
            }
            Ok(())
        }
    }

    #[test]
    fn dropping_an_armed_guard_restores_the_previous_mode() {
        // The core hazard: the curve write fails, so the guard must unwind
        // Custom mode rather than leave the fans stopped.
        let io = FakeModeIo::in_mode(3);
        {
            let _guard = arm_custom_mode_guard(&io).unwrap().expect("guard expected");
            // no disarm: stands in for a failed Fan_Set_Table
        }
        assert_eq!(
            io.calls(),
            vec!["read", "write(3)", "read"],
            "arming only reads; the sole write is the guard's own restore"
        );
    }

    #[test]
    fn a_disarmed_guard_leaves_custom_mode_in_place() {
        let io = FakeModeIo::in_mode(2);
        {
            let mut guard = arm_custom_mode_guard(&io).unwrap().expect("guard expected");
            guard.disarm(); // stands in for a successful write
        }
        assert_eq!(
            io.calls(),
            vec!["read"],
            "a successful write leaves Custom selected, touching nothing on the way out"
        );
    }

    #[test]
    fn already_in_custom_mode_yields_no_guard() {
        // The fans-off state, if any, predates this call. Restoring would mean
        // selecting a mode the user never chose.
        let io = FakeModeIo::in_mode(SMART_FAN_MODE_CUSTOM);
        {
            let guard = arm_custom_mode_guard(&io).unwrap();
            assert!(guard.is_none(), "no guard when already in Custom");
        }
        assert_eq!(io.calls(), vec!["read"], "no mode write of any kind");
    }

    #[test]
    fn an_unreadable_mode_refuses_to_enter_custom() {
        // Entering Custom with no recorded mode to return to is unrecoverable:
        // neither the guard nor the user knows what to restore.
        let io = FakeModeIo::unreadable();
        let result = arm_custom_mode_guard(&io);
        assert!(result.is_err(), "must refuse rather than enter blind");
        assert_eq!(
            io.calls(),
            vec!["read"],
            "the mode must not be changed when it could not be read"
        );
    }

    #[test]
    fn a_failed_restore_falls_back_to_full_speed() {
        // Write failed and every restore failed, so the machine is in Custom
        // with no curve. Full speed overrides the curve, so noise beats a
        // stopped fan -- after one more attempt at leaving Custom by any route.
        let mut io = FakeModeIo::in_mode(1);
        io.write_fails = true;
        {
            // Arming only reads, so it would succeed here. Construct the guard
            // directly to exercise the drop path in isolation from arming.
            let _guard = CustomModeGuard {
                io: &io,
                restore_to: 1,
                armed: true,
            };
        }
        assert_eq!(
            io.calls(),
            vec!["write(1)", "write(2)", "full_speed"],
            "previous mode, then SAFE_FALLBACK, then full speed"
        );
    }

    #[test]
    fn a_failed_restore_does_not_retry_the_same_mode() {
        // When the previous mode already is SAFE_FALLBACK there is no second
        // rung to try; go straight to full speed.
        let mut io = FakeModeIo::in_mode(SMART_FAN_MODE_SAFE_FALLBACK);
        io.write_fails = true;
        {
            let _guard = CustomModeGuard {
                io: &io,
                restore_to: SMART_FAN_MODE_SAFE_FALLBACK,
                armed: true,
            };
        }
        assert_eq!(io.calls(), vec!["write(2)", "full_speed"]);
    }

    #[test]
    fn arming_the_guard_never_changes_the_mode() {
        // The regression this pins: an earlier revision switched into Custom
        // here, before the transaction ran. That reopened the window the single
        // PowerShell invocation exists to close -- process death between the
        // switch and the transaction left the fans stopped with nothing running
        // to restore them. Arming must read, and nothing else.
        for entry_mode in [1u32, 2, 3] {
            let io = FakeModeIo::in_mode(entry_mode);
            let guard = arm_custom_mode_guard(&io).unwrap();
            std::mem::forget(guard); // so Drop cannot add a write
            assert_eq!(io.calls(), vec!["read"], "arming must not write");
        }
    }

    #[test]
    fn the_restore_target_is_never_custom() {
        // Restoring to Custom would be a no-op that leaves the fans stopped.
        for entry_mode in [1u32, 2, 3] {
            let io = FakeModeIo::in_mode(entry_mode);
            let guard = arm_custom_mode_guard(&io).unwrap().expect("guard expected");
            assert_ne!(guard.restore_to, SMART_FAN_MODE_CUSTOM);
            std::mem::forget(guard);
        }
    }

    // -- explicit restore, for the error message ----------------------------
    //
    // Review finding, 2026-09-02: the error the user saw was formatted before
    // the guard dropped, so on the worst path it stated the opposite of the
    // machine's final state. restore_now() runs the same ladder as Drop and
    // hands back what happened, so the message can say where the machine is.

    #[test]
    fn restore_now_restores_reports_and_disarms() {
        let io = FakeModeIo::in_mode(3);
        let outcome = {
            let guard = arm_custom_mode_guard(&io).unwrap().expect("guard expected");
            guard.restore_now()
            // guard is consumed here; its Drop runs disarmed
        };
        assert_eq!(outcome, RestoreOutcome::Restored(3));
        assert_eq!(
            io.calls(),
            vec!["read", "write(3)", "read"],
            "exactly one restore: restore_now disarms, so Drop adds nothing"
        );
    }

    #[test]
    fn restore_now_falls_back_to_safe_mode_when_the_previous_mode_is_refused() {
        let mut io = FakeModeIo::in_mode(3);
        io.write_fails_only_for = Some(3);
        let outcome = CustomModeGuard {
            io: &io,
            restore_to: 3,
            armed: true,
        }
        .restore_now();
        assert_eq!(
            outcome,
            RestoreOutcome::FellBackTo(SMART_FAN_MODE_SAFE_FALLBACK)
        );
        assert_eq!(io.calls(), vec!["write(3)", "write(2)", "read"]);
    }

    #[test]
    fn restore_now_reports_full_speed_when_no_mode_can_be_selected() {
        let mut io = FakeModeIo::in_mode(1);
        io.write_fails = true;
        let outcome = CustomModeGuard {
            io: &io,
            restore_to: 1,
            armed: true,
        }
        .restore_now();
        assert_eq!(outcome, RestoreOutcome::FullSpeedEngaged);
        assert_eq!(io.calls(), vec!["write(1)", "write(2)", "full_speed"]);
    }

    #[test]
    fn restore_now_reports_stranded_when_nothing_works() {
        let mut io = FakeModeIo::in_mode(3);
        io.write_fails = true;
        io.full_speed_fails = true;
        let outcome = CustomModeGuard {
            io: &io,
            restore_to: 3,
            armed: true,
        }
        .restore_now();
        assert_eq!(outcome, RestoreOutcome::Stranded);
    }

    #[test]
    fn a_restore_that_still_reads_custom_is_not_trusted() {
        // Second review, 2026-09-02: the ladder took a successful write as a
        // successful restore, the very shape the transaction's read-back check
        // exists for. Writes here return Ok and change nothing; the machine
        // stays in Custom. The ladder must read back, disbelieve, and escalate
        // rather than report "restored".
        let mut io = FakeModeIo::in_mode(SMART_FAN_MODE_CUSTOM);
        io.writes_ignored = true;
        let outcome = CustomModeGuard {
            io: &io,
            restore_to: 3,
            armed: true,
        }
        .restore_now();
        assert_eq!(outcome, RestoreOutcome::FullSpeedEngaged);
        assert_eq!(
            io.calls(),
            vec!["write(3)", "read", "write(2)", "read", "full_speed"],
            "every rung is confirmed by read-back before it counts"
        );
    }

    #[test]
    fn a_restore_that_cannot_be_read_back_is_not_trusted() {
        // Absence of evidence is not evidence of safety, on this path as on
        // the transaction's.
        let mut io = FakeModeIo::unreadable();
        io.writes_ignored = true;
        let outcome = CustomModeGuard {
            io: &io,
            restore_to: 3,
            armed: true,
        }
        .restore_now();
        assert_eq!(outcome, RestoreOutcome::FullSpeedEngaged);
    }

    #[test]
    fn a_restore_reports_the_mode_read_back_not_the_mode_requested() {
        // Out of Custom is what matters; the message should still say where
        // the machine actually is.
        let io = FakeModeIo::in_mode(3);
        let outcome = CustomModeGuard {
            io: &io,
            restore_to: 1,
            armed: true,
        }
        .restore_now();
        assert_eq!(outcome, RestoreOutcome::Restored(1));
        assert_eq!(io.current.get(), Some(1));
    }

    #[test]
    fn the_full_speed_outcome_tells_the_user_not_to_disable_it() {
        // Full speed masks the fans-off state rather than clearing it: the
        // machine is still in Custom with no curve, and Fan_Set_FullSpeed(0) is
        // exactly what a user does to silence fans. This text is the only place
        // the user learns that, so pin it.
        let text = RestoreOutcome::FullSpeedEngaged.to_string();
        assert!(text.contains("BEFORE disabling full speed"), "{text}");
        assert!(text.contains("still in Custom"), "{text}");
    }

    // -- generated transaction script ---------------------------------------

    /// Print the script for parse-checking against a real Windows PowerShell:
    /// `cargo test -- --ignored --nocapture emit_curve_transaction_script`
    #[test]
    #[ignore = "prints the script for external parse-checking"]
    fn emit_curve_transaction_script() {
        println!("{}", build_curve_transaction_script(3, "@(1,0,0,0,0,0)"));
    }

    #[test]
    fn script_throws_only_inside_the_try() {
        // A throw that escapes sets a non-zero exit code, and ps_command
        // discards stdout in that case -- losing the RESTORED| line in exactly
        // the failure it exists to report. A throw *inside* the try is caught
        // and reported through the tags; that is how the read-back check works.
        let script = build_curve_transaction_script(3, "@(1,0)");
        let try_at = script.find("try {").expect("a try block");
        let catch_at = script.find("} catch {").expect("a catch block");
        let throws: Vec<usize> = script.match_indices("throw").map(|(at, _)| at).collect();
        assert!(!throws.is_empty(), "the read-back check throws");
        for at in throws {
            assert!(
                at > try_at && at < catch_at,
                "throw at {at} is outside the try block ({try_at}..{catch_at})"
            );
        }
    }

    #[test]
    fn script_checks_the_mode_read_back_before_writing_the_table() {
        // Review finding, 2026-09-02: MODESET was emitted and never checked, so
        // a silently ignored mode switch would write the table in the wrong
        // mode and report success. This firmware already ignores
        // Fan_SetCurrentFanSpeed without error, so that shape is real.
        let script = build_curve_transaction_script(3, "@(1,0)");
        let check_at = script
            .find("if ($now -ne 255) { throw")
            .expect("a read-back check");
        let write_at = script.find("Fan_Set_Table").expect("the table write");
        assert!(check_at < write_at, "the check must precede the write");
    }

    #[test]
    fn script_reports_the_exception_message() {
        // Any throw in the try lands in the catch, not only Fan_Set_Table's, so
        // the tag carries the message to say which step failed.
        let script = build_curve_transaction_script(3, "@(1,0)");
        assert!(script.contains("TABLEWRITE|ERR|$($_.Exception.Message)"));
    }

    #[test]
    fn script_restores_in_finally_not_catch() {
        let script = build_curve_transaction_script(3, "@(1,0)");
        let finally_at = script.find("finally").expect("a finally block");
        let restore_at = script.find("RESTORED|").expect("a restore");
        assert!(
            restore_at > finally_at,
            "the restore must live in finally, so it runs on paths catch does not anticipate"
        );
    }

    #[test]
    fn script_skips_the_switch_when_already_custom() {
        // Re-entering Custom from Custom is not a transition; treating it as one
        // would aim the restore at Custom, making it a no-op. The guard
        // condition is present for every previous mode; what makes it a skip
        // is $prev being 255, so that assertion is the one that carries weight.
        let script = build_curve_transaction_script(SMART_FAN_MODE_CUSTOM, "@(1,0)");
        assert!(script.contains("$prev = 255;"));
        assert!(script.contains("if ($prev -ne 255)"));
        assert!(
            build_curve_transaction_script(3, "@(1,0)").contains("$prev = 3;"),
            "the previous mode is embedded, not hardcoded"
        );
    }

    #[test]
    fn script_emits_every_tag_the_parser_reads() {
        // Pins the two halves together: a tag renamed on one side only would
        // leave is_safe() silently reading None forever.
        let script = build_curve_transaction_script(2, "@(1,0)");
        for tag in [
            "PREVMODE|",
            "MODESET|",
            "TABLEWRITE|",
            "RESTORED|",
            "FINALMODE|",
        ] {
            assert!(script.contains(tag), "script must emit {tag}");
        }
    }

    #[test]
    fn script_embeds_the_byte_array_verbatim() {
        let script = build_curve_transaction_script(3, "@(1,0,0,7,7)");
        assert!(script.contains("[byte[]]$table = @(1,0,0,7,7);"));
    }

    // -- curve transaction parsing -----------------------------------------

    #[test]
    fn transaction_full_success_is_safe() {
        let tx = parse_curve_transaction("PREVMODE|3\nMODESET|255\nTABLEWRITE|OK\nFINALMODE|255\n");
        assert_eq!(tx.prev_mode, Some(3));
        assert_eq!(tx.mode_set, Some(255));
        assert_eq!(tx.table_write_ok, Some(true));
        assert_eq!(tx.restored, None);
        assert!(
            tx.is_safe(),
            "a committed write leaves Custom mode legitimately"
        );
    }

    #[test]
    fn transaction_failed_write_but_restored_is_safe() {
        // PowerShell's finally already put the machine back, so the Rust guard
        // must stand down even though the operation failed.
        let tx = parse_curve_transaction(
            "PREVMODE|3\nMODESET|255\nTABLEWRITE|ERR\nRESTORED|3\nFINALMODE|3\n",
        );
        assert_eq!(tx.table_write_ok, Some(false));
        assert_eq!(tx.restored, Some(3));
        assert!(
            tx.is_safe(),
            "restored out of Custom, so nothing is stranded"
        );
    }

    #[test]
    fn transaction_failed_write_and_failed_restore_is_not_safe() {
        // The dangerous case: still in Custom, no curve. The guard must fire.
        let tx = parse_curve_transaction(
            "PREVMODE|3\nMODESET|255\nTABLEWRITE|ERR\nRESTORED|FAIL\nFINALMODE|255\n",
        );
        assert_eq!(tx.restored, None, "FAIL must not parse as a restored mode");
        assert!(!tx.is_safe());
    }

    #[test]
    fn transaction_truncated_output_is_not_safe() {
        // Says the mode was switched and nothing more. Absence of evidence is
        // not evidence of safety.
        let tx = parse_curve_transaction("PREVMODE|3\nMODESET|255\n");
        assert_eq!(tx.table_write_ok, None);
        assert!(!tx.is_safe());
    }

    #[test]
    fn transaction_empty_output_is_not_safe() {
        assert!(!parse_curve_transaction("").is_safe());
    }

    #[test]
    fn transaction_ignores_unrelated_lines() {
        let tx = parse_curve_transaction(
            "WARNING: something\nPREVMODE|2\nrandom noise\nTABLEWRITE|OK\nFINALMODE|255\n",
        );
        assert_eq!(tx.prev_mode, Some(2));
        assert_eq!(tx.table_write_ok, Some(true));
    }

    #[test]
    fn transaction_write_ok_outranks_a_missing_final_mode() {
        // If the write committed, Custom mode is what the user asked for, even
        // when the closing read never arrived.
        let tx = parse_curve_transaction("TABLEWRITE|OK\n");
        assert!(tx.is_safe());
    }

    #[test]
    fn transaction_written_outside_custom_is_not_committed() {
        // The switch was silently ignored: the read-back says 3, the table
        // write did not throw. The curve is not active, so this is a failure --
        // but the machine is not in Custom either, so nothing needs restoring.
        // The script can no longer emit this shape, since it throws on a
        // non-Custom read-back before writing; this pins the Rust side as belt
        // and braces should the script ever regress.
        let tx = parse_curve_transaction("PREVMODE|3\nMODESET|3\nTABLEWRITE|OK\nFINALMODE|3\n");
        assert!(
            !tx.committed(),
            "a table written outside Custom is not active"
        );
        assert!(tx.is_safe(), "and the machine is on its BIOS curve");
    }

    #[test]
    fn transaction_missing_read_back_does_not_block_a_commit() {
        // Belt and braces must not out-rank the belt: a missing MODESET line
        // is not evidence the switch failed. The script-side check is primary.
        let tx = parse_curve_transaction("TABLEWRITE|OK\n");
        assert!(tx.committed());
    }

    #[test]
    fn transaction_captures_the_error_message() {
        let tx = parse_curve_transaction(
            "PREVMODE|3\nMODESET|255\nTABLEWRITE|ERR|Fan_Set_Table: Invalid parameter\nRESTORED|3\nFINALMODE|3\n",
        );
        assert_eq!(tx.table_write_ok, Some(false));
        assert_eq!(
            tx.table_write_error.as_deref(),
            Some("Fan_Set_Table: Invalid parameter")
        );
        assert!(tx.is_safe());
    }

    #[test]
    fn transaction_bare_err_has_no_message() {
        // Output from a script that predates the message suffix.
        let tx = parse_curve_transaction("TABLEWRITE|ERR\n");
        assert_eq!(tx.table_write_ok, Some(false));
        assert_eq!(tx.table_write_error, None);
    }

    #[test]
    fn custom_mode_constant_is_255_not_3() {
        // Pinned deliberately. `scripts/probe-set-table.ps1` uses 3 and calls it
        // Custom; 3 is Performance. Verified by read-back on the 82RG,
        // 2026-08-19.
        assert_eq!(SMART_FAN_MODE_CUSTOM, 255);
    }

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
    fn parse_lighting_state_reads_a_bare_index() {
        assert_eq!(parse_lighting_state("2"), Some(2));
        assert_eq!(parse_lighting_state("1\r\n"), Some(1));
        assert_eq!(parse_lighting_state("  3  \n"), Some(3));
        assert_eq!(parse_lighting_state("0"), Some(0));
    }

    #[test]
    fn parse_lighting_state_rejects_anything_else() {
        assert_eq!(parse_lighting_state(""), None);
        assert_eq!(parse_lighting_state("\n"), None);
        assert_eq!(parse_lighting_state("Ausnahme beim Aufrufen"), None);
        assert_eq!(parse_lighting_state("1\n2"), None);
        assert_eq!(parse_lighting_state("-1"), None);
    }

    #[test]
    fn lighting_read_suspends_after_repeated_failures_and_retries_periodically() {
        // Drive the counters the way get_lighting_state does, without WMI.
        let controller = LenovoFanController::new();
        for _ in 0..LIGHTING_FAILURES_BEFORE_SUSPEND {
            controller.note_lighting_failure(4, "test");
        }
        assert_eq!(
            controller.lighting_failures.get(),
            LIGHTING_FAILURES_BEFORE_SUSPEND
        );
        // The suspension logic lives at the top of get_lighting_state and
        // reads these two cells; check the arithmetic it relies on.
        let mut attempted = 0;
        for _ in 0..(LIGHTING_RETRY_EVERY * 2) {
            let skips = controller.lighting_skips.get() + 1;
            controller.lighting_skips.set(skips);
            if skips.is_multiple_of(LIGHTING_RETRY_EVERY) {
                attempted += 1;
            }
        }
        assert_eq!(attempted, 2);
    }

    #[test]
    fn lighting_read_control_flow_suspends_and_stops_spawning() {
        // On a host without powershell.exe (the Linux test runner) every read
        // fails at launch, which drives the real path through
        // get_lighting_state rather than the arithmetic alone. If this ever
        // runs where powershell.exe exists, the first assertion below fails
        // loudly instead of silently testing something else.
        let controller = LenovoFanController::new();
        let first = controller.get_lighting_state(4);
        assert!(
            matches!(first, Ok(None)),
            "expected Ok(None) from a failed read"
        );
        assert_eq!(
            controller.lighting_failures.get(),
            1,
            "a failed read must count"
        );
        for _ in 1..LIGHTING_FAILURES_BEFORE_SUSPEND {
            assert!(matches!(controller.get_lighting_state(4), Ok(None)));
        }
        assert_eq!(
            controller.lighting_failures.get(),
            LIGHTING_FAILURES_BEFORE_SUSPEND
        );
        // Suspended: the next calls return without attempting a read, which
        // shows as the failure count holding still while skips advance.
        for expected_skips in 1..LIGHTING_RETRY_EVERY {
            assert!(matches!(controller.get_lighting_state(4), Ok(None)));
            assert_eq!(controller.lighting_skips.get(), expected_skips);
            assert_eq!(
                controller.lighting_failures.get(),
                LIGHTING_FAILURES_BEFORE_SUSPEND
            );
        }
        // The retry call attempts a read again (and fails again here).
        assert!(matches!(controller.get_lighting_state(4), Ok(None)));
        assert_eq!(controller.lighting_skips.get(), LIGHTING_RETRY_EVERY);
        assert_eq!(
            controller.lighting_failures.get(),
            LIGHTING_FAILURES_BEFORE_SUSPEND + 1
        );
    }

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
