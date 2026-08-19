// put id:"fan_structs", label:"Fan/FanCurve Data Structs", node_type:"database"

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::errors::FanControlError;

/// A single temperature→RPM point in a fan curve.
#[derive(Debug, Clone, Serialize)]
pub struct FanCurvePoint {
    /// Temperature threshold in degrees Celsius.
    pub temperature: u32,
    /// Target fan speed in RPM at this temperature.
    pub fan_speed: u32,
}

/// A fan curve mapping sensor temperatures to fan speeds.
///
/// Each curve binds one fan to one sensor. The EC takes the maximum speed
/// demanded across all sensor curves for a given fan.
#[derive(Debug, Clone, Serialize)]
pub struct FanCurve {
    pub fan_id: u32,
    pub sensor_id: u32,
    pub min_speed: u32,
    pub max_speed: u32,
    pub min_temp: u32,
    pub max_temp: u32,
    pub points: Vec<FanCurvePoint>,
    pub active: bool,
}

/// Represents a single fan discovered on the system.
#[derive(Debug, Clone, Serialize)]
pub struct Fan {
    /// Unique identifier (e.g. "hwmon2/fan1" on Linux, WMI instance path on Windows)
    pub id: String,
    /// Human-readable label (e.g. "CPU Fan", "Chassis Fan #1")
    pub label: String,
    /// Current speed in RPM
    pub speed_rpm: u32,
    /// PWM duty cycle 0–255 (if controllable)
    pub pwm: Option<u8>,
    /// Whether this fan supports speed control
    pub controllable: bool,
    /// Minimum RPM the fan itself can run at, when the platform reports one.
    ///
    /// On Lenovo this is `CurrentFanMinSpeed`, falling back to the span of the
    /// fan's curve table — which is a different quantity, so the fallback is
    /// an approximation rather than an equivalent.
    pub min_rpm: Option<u32>,
    /// Maximum RPM the fan itself can run at, when the platform reports one.
    ///
    /// Lenovo's `CurrentFanMaxSpeed`. Note that full speed mode exceeds it:
    /// 5400 RPM has been observed against a reported maximum of 4800.
    pub max_rpm: Option<u32>,
    /// Fan curves from EC table data (if available).
    pub curves: Vec<FanCurve>,
    /// Whether full speed mode is currently active (Lenovo-specific).
    pub full_speed_active: bool,
}

/// Maximum allowed value for a speed step index.
///
/// Lives here rather than in the Lenovo backend because the TUI's step
/// sanitizer needs it too, and that module is compiled on every platform while
/// the backend is not. One definition, so the two cannot drift.
pub const MAX_STEP_VALUE: u8 = 10;

/// SmartFanMode values, defined once.
///
/// Lives beside [`MINIMUM_STEPS`] for the same reason: the TUI compiles on every
/// platform while the Lenovo backend does not, and both need these. The literals
/// were previously repeated across `tui.rs` and `lenovo.rs`, which is how a
/// value this consequential drifts.
///
/// The bare number `255` means three unrelated things in this codebase — Custom
/// mode here, full speed in `set_pwm`, and an out-of-range step sentinel in the
/// TUI's tests — so reading it correctly depends entirely on context. Naming it
/// removes that.
///
/// **Custom is 255, verified by read-back on the 82RG, 2026-08-19.**
/// `scripts/probe-set-table.ps1` uses 3 and labels it Custom; 3 is Performance,
/// and that probe ran on a machine already in mode 3, so it never changed the
/// mode and never met `Fan_Set_Table`'s prerequisite. `scripts/` is frozen
/// history — do not take the value from there.
pub mod smart_fan_mode {
    pub const QUIET: u32 = 1;
    pub const BALANCED: u32 = 2;
    pub const PERFORMANCE: u32 = 3;

    /// The mode `Fan_Set_Table` requires.
    ///
    /// Dangerous on its own: with no curve loaded, Custom mode **stops the fans
    /// and keeps them stopped under load** — measured at 0 RPM across 61–67 °C
    /// with every thread pinned. Never select it without a curve write
    /// immediately following, guarded so that a failure unwinds it.
    pub const CUSTOM: u32 = 255;

    /// Where to return when Custom mode has to be abandoned.
    ///
    /// Balanced rather than the BIOS default because there is no "unset" mode to
    /// return to, and Balanced is the mode that behaves reasonably at any
    /// temperature. Must never equal [`CUSTOM`]; a test pins that.
    pub const SAFE_FALLBACK: u32 = BALANCED;
}

/// Human-readable name for a SmartFanMode value.
///
/// Returns a label for `None` and for unrecognised values rather than failing:
/// this feeds status displays, where an unknown mode is information, not an
/// error.
pub fn smart_fan_mode_label(mode: Option<u32>) -> &'static str {
    match mode {
        Some(smart_fan_mode::QUIET) => "Quiet",
        Some(smart_fan_mode::BALANCED) => "Balanced",
        Some(smart_fan_mode::PERFORMANCE) => "Performance",
        Some(smart_fan_mode::CUSTOM) => "Custom",
        Some(_) => "Unknown",
        None => "N/A",
    }
}

/// Per-step minimum values — LenovoLegionToolkit's **GodMode V1** table.
///
/// The floors are data, and this is the only place they are written. Both the
/// validator ([`validate_custom_curve`]) and the TUI's sanitizer read from
/// here, so the rule that rejects a curve and the rule that repairs one cannot
/// disagree. They previously did: a doc comment cited LLT's V2 table while the
/// code enforced no floor at all below step 8.
///
/// LLT keeps a stricter V2 table, `[1,1,1,1,1,1,1,1,3,5]`, which forbids 0
/// anywhere. The 82RG measured as V1 for issue #25, so this is the measured
/// choice on that hardware rather than a permissive default — see
/// [`validate_custom_curve`] for the measurements and for what the step 7
/// floor does and does not claim.
pub const MINIMUM_STEPS: [u8; 10] = [0, 0, 0, 0, 0, 0, 0, 1, 3, 5];

/// Name of the temperature band a step governs, for validation messages.
///
/// Only indices 7–9 can reach a message, since every floor below them is 0.
/// The fallback deliberately names no specific band: the firmware's actual
/// band boundaries are not established here.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn step_band_label(index: usize) -> &'static str {
    match index {
        7 => "approaching high temp",
        8 => "high temp",
        9 => "max temp",
        _ => "low temp band",
    }
}

/// A user-defined custom fan curve to write to the EC via Fan_Set_Table.
///
/// The `steps` array contains 10 speed step indices (0–10 scale) that index
/// into the hardware's FanSpeeds array from `LENOVO_FAN_TABLE_DATA`.
/// For example, on an 82RG with FanSpeeds = [1600,1800,...,4800]:
///   step index 0 → 1600 RPM, step index 9 → 4800 RPM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub struct CustomFanCurve {
    /// Fan identifier (0 = CPU fan, 1 = GPU fan on V1 hardware).
    pub fan_id: u32,
    /// Sensor identifier (3 = CPU temp, 4 = GPU temp on V1 hardware).
    pub sensor_id: u32,
    /// 10 speed step indices, each 0–10. These are indices into the
    /// FanSpeeds array from LENOVO_FAN_TABLE_DATA, NOT RPM values.
    pub steps: [u8; 10],
}

/// Validate a custom curve's step values, enforcing safety constraints.
///
/// Rules:
///   - All steps must be in range 0–10 ([`MAX_STEP_VALUE`])
///   - Steps must be non-decreasing (no "death valley" curves)
///   - Each step must meet its floor in [`MINIMUM_STEPS`]: 1 at step 7,
///     3 at step 8, 5 at step 9, and 0 below that
///
/// Validation is element-wise against [`MINIMUM_STEPS`], matching LLT's own
/// check (`AbstractGodModeController.IsValidFanTableAsync`): valid iff
/// `minimum[i] <= step[i] <= 10` for every index. V1's floors for steps 0–6
/// are zero, so declining to floor them is V1 parity, not a third scheme.
///
/// LLT selects between its V1 and V2 tables by SmartFan/LegionZone version.
/// **The 82RG is V1**, measured for issue #25 on 2026-08-12:
/// `SmartFanVersion = 5` and `LegionZoneVersion = 2` both land in V1's range
/// independently, the power-mode mask `0x10007` has bit 16 set so GodMode is
/// supported, and BIOS `JUCN68WW` is outside LLT's V1 blocklist. Taking V1's
/// table is therefore the measured choice on this hardware, not a permissive
/// default.
///
/// It remains the safer choice on hardware this has not been measured on: if
/// such a machine turns out to be V2, the firmware rejects the curve and the
/// user sees an error at the WMI boundary, which is a better failure than
/// silently refusing curves the hardware would have accepted.
///
/// **What the step 7 floor does and does not claim.** Both tables require ≥ 1
/// at step 7, which is the whole justification for enforcing it — it is
/// upstream parity under either. It is *not* known to be an off-versus-on
/// guarantee. Whether step value 0 means "fans off" or the lowest table entry
/// (~1600 RPM on the 82RG) is exactly the open question in issue #18, and this
/// crate documents both readings: [`CustomFanCurve`] describes steps as direct
/// indices where 0 → 1600 RPM, while [`MAX_STEP_VALUE`] of 10 makes an
/// eleven-value scale over a ten-entry table, which fits 0 = off. Do not read
/// a thermal guarantee into this floor until #18 settles it.
///
/// The non-decreasing rule is ours, not upstream's — LLT enforces no
/// monotonicity at all. It is kept as a deliberate safety choice.
///
/// Lives here beside [`CustomFanCurve`] rather than in the Lenovo backend so
/// that it compiles on every platform, and so the TUI sanitizer can assert its
/// output against the real validator instead of a hand-copy of the rules.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn validate_custom_curve(curve: &CustomFanCurve) -> Result<(), FanControlError> {
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

    // Per-step safety minimums, element-wise against the V1 table.
    for (i, (&step, &minimum)) in curve.steps.iter().zip(MINIMUM_STEPS.iter()).enumerate() {
        if step < minimum {
            return Err(FanControlError::Platform(format!(
                "step {i} ({}) must be >= {minimum} for safety, got {step}",
                step_band_label(i)
            )));
        }
    }

    Ok(())
}

impl fmt::Display for Fan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let control_status = if self.controllable {
            "controllable"
        } else {
            "read-only"
        };
        write!(
            f,
            "{}: {} RPM [{}]",
            self.label, self.speed_rpm, control_status
        )
    }
}

// ---------------------------------------------------------------------------
// Tests — validation is pure, so these run on every platform
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn curve(steps: [u8; 10]) -> CustomFanCurve {
        CustomFanCurve {
            fan_id: 0,
            sensor_id: 3,
            steps,
        }
    }

    #[test]
    fn minimum_steps_is_llt_godmode_v1() {
        // The floors are data now, so pin the data itself. V2's table is
        // [1,1,1,1,1,1,1,1,3,5]; picking it up by accident would silently
        // forbid the idle-off curves V1 permits.
        assert_eq!(MINIMUM_STEPS, [0, 0, 0, 0, 0, 0, 0, 1, 3, 5]);
    }

    #[test]
    fn safe_fallback_is_never_custom() {
        // The entire restore path assumes it can leave Custom mode by selecting
        // SAFE_FALLBACK. If the two were ever equal, every "restore" would be a
        // no-op that leaves the fans stopped.
        assert_ne!(smart_fan_mode::SAFE_FALLBACK, smart_fan_mode::CUSTOM);
    }

    #[test]
    fn smart_fan_mode_labels() {
        assert_eq!(smart_fan_mode_label(Some(1)), "Quiet");
        assert_eq!(smart_fan_mode_label(Some(2)), "Balanced");
        assert_eq!(smart_fan_mode_label(Some(3)), "Performance");
        assert_eq!(smart_fan_mode_label(Some(255)), "Custom");
        assert_eq!(smart_fan_mode_label(Some(7)), "Unknown");
        assert_eq!(smart_fan_mode_label(None), "N/A");
    }

    #[test]
    fn custom_mode_value_is_255() {
        // Pinned: scripts/probe-set-table.ps1 uses 3 and calls it Custom.
        // Verified by read-back on the 82RG, 2026-08-19.
        assert_eq!(smart_fan_mode::CUSTOM, 255);
    }

    #[test]
    fn minimum_steps_is_itself_a_valid_curve() {
        // The floor table must pass its own validator, or the sanitizer could
        // produce a curve the validator rejects. It is non-decreasing and
        // within range, so this holds -- but it holds by construction, and a
        // future edit to the table could break it silently.
        assert!(validate_custom_curve(&curve(MINIMUM_STEPS)).is_ok());
    }

    #[test]
    fn step7_error_message_is_the_documented_one() {
        // README.md and CHANGELOG.md quote this string verbatim as the
        // breaking-change signature. Rewording it without updating both would
        // leave the documentation describing an error that no longer exists.
        let err = validate_custom_curve(&curve([0, 0, 0, 0, 0, 0, 0, 0, 3, 5])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "platform error: step 7 (approaching high temp) must be >= 1 for safety, got 0"
        );
    }

    #[test]
    fn step8_and_step9_error_messages() {
        let err = validate_custom_curve(&curve([0, 0, 0, 0, 0, 0, 0, 1, 2, 5])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "platform error: step 8 (high temp) must be >= 3 for safety, got 2"
        );

        let err = validate_custom_curve(&curve([0, 0, 0, 0, 0, 0, 0, 1, 3, 4])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "platform error: step 9 (max temp) must be >= 5 for safety, got 4"
        );
    }

    #[test]
    fn accepts_the_v1_minimum_and_the_v2_minimum() {
        assert!(validate_custom_curve(&curve([0, 0, 0, 0, 0, 0, 0, 1, 3, 5])).is_ok());
        assert!(validate_custom_curve(&curve([1, 1, 1, 1, 1, 1, 1, 1, 3, 5])).is_ok());
    }

    #[test]
    fn rejects_out_of_range_and_decreasing() {
        let err = validate_custom_curve(&curve([1, 1, 1, 1, 1, 1, 1, 1, 3, 11])).unwrap_err();
        assert!(err.to_string().contains("exceeds maximum"));

        let err = validate_custom_curve(&curve([5, 4, 3, 2, 1, 1, 1, 1, 3, 5])).unwrap_err();
        assert!(err.to_string().contains("non-decreasing"));
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
}
