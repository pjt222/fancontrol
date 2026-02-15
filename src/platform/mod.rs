#[cfg(any(target_os = "windows", test))]
mod lenovo;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

use crate::errors::FanControlError;
use crate::fan::{Fan, FanCurve, FanCurvePoint};

/// Validate a fan curve for safety.
///
/// Ensures:
/// - At least 2 points
/// - Temperatures are strictly increasing
/// - Fan speeds are non-decreasing (RPM must not drop as temperature rises)
/// - The highest temperature point has a reasonably high RPM (>= 50% of max_speed)
/// - All temperatures and speeds are within plausible ranges
pub fn validate_curve(curve: &FanCurve) -> Result<(), FanControlError> {
    if curve.points.len() < 2 {
        return Err(FanControlError::InvalidCurve(
            "curve must have at least 2 points".to_string(),
        ));
    }

    for (i, point) in curve.points.iter().enumerate() {
        if point.temperature > 150 {
            return Err(FanControlError::InvalidCurve(format!(
                "point {} has unreasonable temperature {}°C (max 150)",
                i, point.temperature
            )));
        }
    }

    // Check temperatures are strictly increasing.
    for i in 1..curve.points.len() {
        if curve.points[i].temperature <= curve.points[i - 1].temperature {
            return Err(FanControlError::InvalidCurve(format!(
                "temperatures must be strictly increasing: {}°C at point {} is not greater than {}°C at point {}",
                curve.points[i].temperature, i, curve.points[i - 1].temperature, i - 1
            )));
        }
    }

    // Check fan speeds are non-decreasing.
    for i in 1..curve.points.len() {
        if curve.points[i].fan_speed < curve.points[i - 1].fan_speed {
            return Err(FanControlError::InvalidCurve(format!(
                "fan speed must not decrease as temperature rises: {} RPM at {}°C is less than {} RPM at {}°C",
                curve.points[i].fan_speed, curve.points[i].temperature,
                curve.points[i - 1].fan_speed, curve.points[i - 1].temperature
            )));
        }
    }

    // Safety: the highest temperature point must have a high enough RPM.
    // At least 50% of max_speed to prevent overheating.
    if curve.max_speed > 0 {
        let last_point = &curve.points[curve.points.len() - 1];
        let min_safe_rpm = curve.max_speed / 2;
        if last_point.fan_speed < min_safe_rpm {
            return Err(FanControlError::InvalidCurve(format!(
                "highest temperature point ({}\u{00B0}C) has only {} RPM; must be at least {} RPM (50% of max {})",
                last_point.temperature, last_point.fan_speed, min_safe_rpm, curve.max_speed
            )));
        }
    }

    Ok(())
}

/// Build a `FanCurve` from user-supplied temperature→RPM pairs, filling in
/// metadata from the original curve (if available) or using sensible defaults.
pub fn build_curve_from_points(
    fan_id: u32,
    sensor_id: u32,
    points: Vec<FanCurvePoint>,
    reference: Option<&FanCurve>,
) -> FanCurve {
    let speeds: Vec<u32> = points.iter().map(|p| p.fan_speed).collect();
    let temps: Vec<u32> = points.iter().map(|p| p.temperature).collect();

    let min_speed = reference
        .map(|r| r.min_speed)
        .unwrap_or_else(|| speeds.iter().copied().min().unwrap_or(0));
    let max_speed = reference
        .map(|r| r.max_speed)
        .unwrap_or_else(|| speeds.iter().copied().max().unwrap_or(0));
    let min_temp = reference
        .map(|r| r.min_temp)
        .unwrap_or_else(|| temps.iter().copied().min().unwrap_or(0));
    let max_temp = reference
        .map(|r| r.max_temp)
        .unwrap_or_else(|| temps.iter().copied().max().unwrap_or(0));

    FanCurve {
        fan_id,
        sensor_id,
        min_speed,
        max_speed,
        min_temp,
        max_temp,
        points,
        active: true,
    }
}

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

    /// Write a custom fan curve to the EC for a specific fan/sensor pair.
    /// The curve is validated before writing.
    fn set_fan_curve(&self, _curve: &FanCurve) -> Result<(), FanControlError> {
        Err(FanControlError::Platform(
            "setting fan curves not supported on this platform".to_string(),
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

// ---------------------------------------------------------------------------
// Tests for validate_curve and build_curve_from_points
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fan::{FanCurve, FanCurvePoint};

    fn make_curve(points: Vec<(u32, u32)>, max_speed: u32) -> FanCurve {
        FanCurve {
            fan_id: 0,
            sensor_id: 3,
            min_speed: points.first().map(|p| p.1).unwrap_or(0),
            max_speed,
            min_temp: points.first().map(|p| p.0).unwrap_or(0),
            max_temp: points.last().map(|p| p.0).unwrap_or(0),
            points: points
                .into_iter()
                .map(|(t, s)| FanCurvePoint {
                    temperature: t,
                    fan_speed: s,
                })
                .collect(),
            active: true,
        }
    }

    #[test]
    fn validate_curve_valid() {
        let curve = make_curve(
            vec![
                (58, 1600),
                (63, 2100),
                (68, 2700),
                (73, 3400),
                (85, 4200),
                (100, 4800),
            ],
            4800,
        );
        assert!(validate_curve(&curve).is_ok());
    }

    #[test]
    fn validate_curve_minimum_points() {
        let curve = make_curve(vec![(50, 1600), (100, 4800)], 4800);
        assert!(validate_curve(&curve).is_ok());
    }

    #[test]
    fn validate_curve_too_few_points() {
        let curve = make_curve(vec![(50, 1600)], 4800);
        let err = validate_curve(&curve).unwrap_err();
        assert!(err.to_string().contains("at least 2 points"));
    }

    #[test]
    fn validate_curve_empty_points() {
        let curve = make_curve(vec![], 4800);
        let err = validate_curve(&curve).unwrap_err();
        assert!(err.to_string().contains("at least 2 points"));
    }

    #[test]
    fn validate_curve_non_increasing_temps() {
        let curve = make_curve(vec![(60, 1600), (55, 2100), (70, 3200)], 4800);
        let err = validate_curve(&curve).unwrap_err();
        assert!(err.to_string().contains("strictly increasing"));
    }

    #[test]
    fn validate_curve_equal_temps() {
        let curve = make_curve(vec![(60, 1600), (60, 2100), (70, 3200)], 4800);
        let err = validate_curve(&curve).unwrap_err();
        assert!(err.to_string().contains("strictly increasing"));
    }

    #[test]
    fn validate_curve_decreasing_speed() {
        let curve = make_curve(vec![(50, 3000), (70, 2000), (90, 4800)], 4800);
        let err = validate_curve(&curve).unwrap_err();
        assert!(err.to_string().contains("must not decrease"));
    }

    #[test]
    fn validate_curve_unsafe_high_temp_low_rpm() {
        // Last point at 100°C with only 1000 RPM when max is 4800 — unsafe
        let curve = make_curve(vec![(50, 800), (100, 1000)], 4800);
        let err = validate_curve(&curve).unwrap_err();
        assert!(err.to_string().contains("50%"));
    }

    #[test]
    fn validate_curve_unreasonable_temp() {
        let curve = make_curve(vec![(50, 1600), (200, 4800)], 4800);
        let err = validate_curve(&curve).unwrap_err();
        assert!(err.to_string().contains("unreasonable temperature"));
    }

    #[test]
    fn validate_curve_equal_speeds_allowed() {
        // Flat curve (same RPM at different temps) is valid
        let curve = make_curve(vec![(50, 3000), (60, 3000), (70, 3000)], 3000);
        assert!(validate_curve(&curve).is_ok());
    }

    #[test]
    fn build_curve_from_points_no_reference() {
        let points = vec![
            FanCurvePoint {
                temperature: 50,
                fan_speed: 1600,
            },
            FanCurvePoint {
                temperature: 80,
                fan_speed: 4800,
            },
        ];
        let curve = build_curve_from_points(0, 3, points, None);
        assert_eq!(curve.fan_id, 0);
        assert_eq!(curve.sensor_id, 3);
        assert_eq!(curve.min_speed, 1600);
        assert_eq!(curve.max_speed, 4800);
        assert_eq!(curve.min_temp, 50);
        assert_eq!(curve.max_temp, 80);
        assert!(curve.active);
        assert_eq!(curve.points.len(), 2);
    }

    #[test]
    fn build_curve_from_points_with_reference() {
        let reference = make_curve(vec![(58, 1600), (100, 4800)], 4800);
        let points = vec![
            FanCurvePoint {
                temperature: 55,
                fan_speed: 2000,
            },
            FanCurvePoint {
                temperature: 90,
                fan_speed: 4500,
            },
        ];
        let curve = build_curve_from_points(0, 3, points, Some(&reference));
        // Should use reference metadata
        assert_eq!(curve.min_speed, 1600);
        assert_eq!(curve.max_speed, 4800);
        assert_eq!(curve.min_temp, 58);
        assert_eq!(curve.max_temp, 100);
    }

    #[test]
    fn fan_curve_serde_roundtrip() {
        let curve = make_curve(vec![(58, 1600), (63, 2100), (100, 4800)], 4800);
        let json = serde_json::to_string(&curve).expect("serialize");
        let deserialized: FanCurve = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.fan_id, curve.fan_id);
        assert_eq!(deserialized.sensor_id, curve.sensor_id);
        assert_eq!(deserialized.points.len(), curve.points.len());
        assert_eq!(deserialized.points[0].temperature, 58);
        assert_eq!(deserialized.points[2].fan_speed, 4800);
    }

    #[test]
    fn fan_curves_vec_serde_roundtrip() {
        let curves = vec![
            make_curve(vec![(58, 1600), (100, 4800)], 4800),
            make_curve(vec![(63, 1800), (95, 4800)], 4800),
        ];
        let json = serde_json::to_string_pretty(&curves).expect("serialize");
        let deserialized: Vec<FanCurve> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.len(), 2);
        assert_eq!(deserialized[0].points.len(), 2);
        assert_eq!(deserialized[1].points.len(), 2);
    }
}
