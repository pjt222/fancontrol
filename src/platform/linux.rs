use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::errors::FanControlError;
use crate::fan::Fan;
use super::FanController;

const HWMON_BASE: &str = "/sys/class/hwmon";

/// Linux fan controller backed by sysfs/hwmon.
///
/// Discovers fans by scanning `/sys/class/hwmon/hwmon*/fan*_input` and
/// exposes RPM reading and PWM-based speed control.
pub struct LinuxFanController {
    hwmon_base: PathBuf,
}

impl LinuxFanController {
    /// Create a new controller that reads from the default sysfs path.
    pub fn new() -> Self {
        Self {
            hwmon_base: PathBuf::from(HWMON_BASE),
        }
    }

    /// Create a controller rooted at a custom path (useful for testing).
    #[cfg(test)]
    fn with_base(hwmon_base: PathBuf) -> Self {
        Self { hwmon_base }
    }

    /// Resolve the sysfs paths for a given fan id.
    ///
    /// A fan id has the form `"hwmon{N}/fan{M}"`. This function returns the
    /// directory for hwmon{N} and the fan index M as a string.
    fn resolve_fan_paths(&self, fan_id: &str) -> Result<(PathBuf, String), FanControlError> {
        let parts: Vec<&str> = fan_id.split('/').collect();
        if parts.len() != 2 {
            return Err(FanControlError::FanNotFound(fan_id.to_string()));
        }

        let hwmon_dir = self.hwmon_base.join(parts[0]);
        let fan_name = parts[1]; // e.g. "fan1"

        let fan_index = fan_name
            .strip_prefix("fan")
            .ok_or_else(|| FanControlError::FanNotFound(fan_id.to_string()))?;

        // Verify the fan input file actually exists.
        let input_path = hwmon_dir.join(format!("fan{}_input", fan_index));
        if !input_path.exists() {
            return Err(FanControlError::FanNotFound(fan_id.to_string()));
        }

        Ok((hwmon_dir, fan_index.to_string()))
    }
}

impl FanController for LinuxFanController {
    fn discover(&self) -> Result<Vec<Fan>, FanControlError> {
        let mut fans = Vec::new();

        let hwmon_entries = match fs::read_dir(&self.hwmon_base) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(fans),
            Err(error) => return Err(map_io_error(error, &self.hwmon_base)),
        };

        // Collect and sort hwmon directories for deterministic ordering.
        let mut hwmon_dirs: Vec<PathBuf> = hwmon_entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.starts_with("hwmon"))
                    .unwrap_or(false)
            })
            .collect();
        hwmon_dirs.sort();

        for hwmon_dir in hwmon_dirs {
            let hwmon_name = hwmon_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("hwmon?")
                .to_string();

            let discovered_fans = discover_fans_in_hwmon(&hwmon_dir, &hwmon_name)?;
            fans.extend(discovered_fans);
        }

        Ok(fans)
    }

    fn get_speed(&self, fan_id: &str) -> Result<u32, FanControlError> {
        let (hwmon_dir, fan_index) = self.resolve_fan_paths(fan_id)?;
        let input_path = hwmon_dir.join(format!("fan{}_input", fan_index));
        read_sysfs_u32(&input_path)
    }

    fn set_pwm(&self, fan_id: &str, pwm: u8) -> Result<(), FanControlError> {
        let (hwmon_dir, fan_index) = self.resolve_fan_paths(fan_id)?;

        let pwm_path = hwmon_dir.join(format!("pwm{}", fan_index));
        let pwm_enable_path = hwmon_dir.join(format!("pwm{}_enable", fan_index));

        // Verify PWM control file exists.
        if !pwm_path.exists() {
            return Err(FanControlError::NotControllable(fan_id.to_string()));
        }

        // Switch to manual mode (value "1") before writing the duty cycle.
        write_sysfs_value(&pwm_enable_path, "1").map_err(|error| match error {
            FanControlError::Io(ref io_error) if io_error.kind() == ErrorKind::PermissionDenied => {
                FanControlError::PermissionDenied(format!(
                    "cannot enable manual PWM control for '{}': run as root or adjust permissions",
                    fan_id
                ))
            }
            other => other,
        })?;

        // Write the PWM duty cycle (0-255).
        write_sysfs_value(&pwm_path, &pwm.to_string()).map_err(|error| match error {
            FanControlError::Io(ref io_error) if io_error.kind() == ErrorKind::PermissionDenied => {
                FanControlError::PermissionDenied(format!(
                    "cannot write PWM value for '{}': run as root or adjust permissions",
                    fan_id
                ))
            }
            other => other,
        })?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Discover all fans under a single hwmon directory.
fn discover_fans_in_hwmon(
    hwmon_dir: &Path,
    hwmon_name: &str,
) -> Result<Vec<Fan>, FanControlError> {
    let mut fans = Vec::new();

    let entries = match fs::read_dir(hwmon_dir) {
        Ok(entries) => entries,
        Err(error) => return Err(map_io_error(error, hwmon_dir)),
    };

    // Find all fan*_input files and sort by index for stable ordering.
    let mut fan_inputs: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let file_name = entry.file_name().to_string_lossy().to_string();
            if file_name.starts_with("fan") && file_name.ends_with("_input") {
                Some(file_name)
            } else {
                None
            }
        })
        .collect();
    fan_inputs.sort();

    for input_file in fan_inputs {
        // Extract the fan index, e.g. "fan1_input" -> "1".
        let fan_index = input_file
            .strip_prefix("fan")
            .and_then(|remainder| remainder.strip_suffix("_input"))
            .unwrap_or("0");

        let fan_id = format!("{}/fan{}", hwmon_name, fan_index);

        let label = read_fan_label(hwmon_dir, fan_index);
        let speed_rpm = read_sysfs_u32(&hwmon_dir.join(&input_file)).unwrap_or(0);
        let (controllable, current_pwm) = read_pwm_state(hwmon_dir, fan_index);

        fans.push(Fan {
            id: fan_id,
            label,
            speed_rpm,
            pwm: current_pwm,
            controllable,
        });
    }

    Ok(fans)
}

/// Read a fan label from `fan{N}_label`, falling back to `"Fan {N}"`.
fn read_fan_label(hwmon_dir: &Path, fan_index: &str) -> String {
    let label_path = hwmon_dir.join(format!("fan{}_label", fan_index));
    match fs::read_to_string(&label_path) {
        Ok(content) => {
            let trimmed = content.trim().to_string();
            if trimmed.is_empty() {
                format!("Fan {}", fan_index)
            } else {
                trimmed
            }
        }
        Err(_) => format!("Fan {}", fan_index),
    }
}

/// Check whether PWM control is available for a fan and read its current value.
///
/// Returns `(controllable, current_pwm)`. A fan is considered controllable when
/// the `pwm{N}` file exists and is writable.
fn read_pwm_state(hwmon_dir: &Path, fan_index: &str) -> (bool, Option<u8>) {
    let pwm_path = hwmon_dir.join(format!("pwm{}", fan_index));

    if !pwm_path.exists() {
        return (false, None);
    }

    let current_pwm = read_sysfs_u32(&pwm_path).ok().map(|value| value as u8);

    // Check writability by inspecting file metadata.
    let writable = fs::metadata(&pwm_path)
        .map(|metadata| !metadata.permissions().readonly())
        .unwrap_or(false);

    (writable, current_pwm)
}

/// Read a sysfs file and parse its content as a `u32`.
fn read_sysfs_u32(path: &Path) -> Result<u32, FanControlError> {
    let content = fs::read_to_string(path).map_err(|error| map_io_error(error, path))?;
    content
        .trim()
        .parse::<u32>()
        .map_err(|parse_error| {
            FanControlError::Platform(format!(
                "failed to parse '{}' from {}: {}",
                content.trim(),
                path.display(),
                parse_error
            ))
        })
}

/// Write a string value to a sysfs file.
fn write_sysfs_value(path: &Path, value: &str) -> Result<(), FanControlError> {
    fs::write(path, value).map_err(|error| map_io_error(error, path))?;
    Ok(())
}

/// Map an `std::io::Error` to the appropriate `FanControlError` variant,
/// converting `PermissionDenied` errors to a descriptive message.
fn map_io_error(error: std::io::Error, path: &Path) -> FanControlError {
    match error.kind() {
        ErrorKind::PermissionDenied => {
            FanControlError::PermissionDenied(format!("{}: {}", path.display(), error))
        }
        _ => FanControlError::Io(error),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// Helper: build a fake hwmon tree under a temp directory.
    struct FakeHwmon {
        root: TempDir,
    }

    impl FakeHwmon {
        fn new() -> Self {
            Self {
                root: TempDir::new().expect("failed to create temp dir"),
            }
        }

        fn base_path(&self) -> PathBuf {
            self.root.path().to_path_buf()
        }

        /// Create a fan input file: hwmon{hwmon}/fan{fan}_input with the given RPM.
        fn add_fan(&self, hwmon_index: u32, fan_index: u32, rpm: u32) -> &Self {
            let hwmon_dir = self.root.path().join(format!("hwmon{}", hwmon_index));
            fs::create_dir_all(&hwmon_dir).unwrap();
            fs::write(
                hwmon_dir.join(format!("fan{}_input", fan_index)),
                rpm.to_string(),
            )
            .unwrap();
            self
        }

        /// Add a label file for a fan.
        fn add_label(&self, hwmon_index: u32, fan_index: u32, label: &str) -> &Self {
            let hwmon_dir = self.root.path().join(format!("hwmon{}", hwmon_index));
            fs::create_dir_all(&hwmon_dir).unwrap();
            fs::write(
                hwmon_dir.join(format!("fan{}_label", fan_index)),
                format!("{}\n", label),
            )
            .unwrap();
            self
        }

        /// Add writable PWM files for a fan.
        fn add_pwm(&self, hwmon_index: u32, fan_index: u32, current_pwm: u8) -> &Self {
            let hwmon_dir = self.root.path().join(format!("hwmon{}", hwmon_index));
            fs::create_dir_all(&hwmon_dir).unwrap();

            let pwm_path = hwmon_dir.join(format!("pwm{}", fan_index));
            fs::write(&pwm_path, current_pwm.to_string()).unwrap();
            fs::set_permissions(&pwm_path, fs::Permissions::from_mode(0o644)).unwrap();

            let enable_path = hwmon_dir.join(format!("pwm{}_enable", fan_index));
            fs::write(&enable_path, "2").unwrap();
            fs::set_permissions(&enable_path, fs::Permissions::from_mode(0o644)).unwrap();

            self
        }

        /// Add a read-only PWM file (fan exists but is not controllable).
        fn add_readonly_pwm(&self, hwmon_index: u32, fan_index: u32, current_pwm: u8) -> &Self {
            let hwmon_dir = self.root.path().join(format!("hwmon{}", hwmon_index));
            fs::create_dir_all(&hwmon_dir).unwrap();

            let pwm_path = hwmon_dir.join(format!("pwm{}", fan_index));
            fs::write(&pwm_path, current_pwm.to_string()).unwrap();
            fs::set_permissions(&pwm_path, fs::Permissions::from_mode(0o444)).unwrap();

            self
        }
    }

    #[test]
    fn discover_no_hwmon_directory() {
        let temp_dir = TempDir::new().unwrap();
        let nonexistent = temp_dir.path().join("no_such_dir");
        let controller = LinuxFanController::with_base(nonexistent);
        let fans = controller.discover().unwrap();
        assert!(fans.is_empty());
    }

    #[test]
    fn discover_empty_hwmon() {
        let fake = FakeHwmon::new();
        let controller = LinuxFanController::with_base(fake.base_path());
        let fans = controller.discover().unwrap();
        assert!(fans.is_empty());
    }

    #[test]
    fn discover_single_fan_without_label() {
        let fake = FakeHwmon::new();
        fake.add_fan(0, 1, 1200);
        let controller = LinuxFanController::with_base(fake.base_path());

        let fans = controller.discover().unwrap();
        assert_eq!(fans.len(), 1);
        assert_eq!(fans[0].id, "hwmon0/fan1");
        assert_eq!(fans[0].label, "Fan 1");
        assert_eq!(fans[0].speed_rpm, 1200);
        assert_eq!(fans[0].pwm, None);
        assert!(!fans[0].controllable);
    }

    #[test]
    fn discover_fan_with_label() {
        let fake = FakeHwmon::new();
        fake.add_fan(2, 1, 950);
        fake.add_label(2, 1, "CPU Fan");
        let controller = LinuxFanController::with_base(fake.base_path());

        let fans = controller.discover().unwrap();
        assert_eq!(fans.len(), 1);
        assert_eq!(fans[0].label, "CPU Fan");
    }

    #[test]
    fn discover_controllable_fan() {
        let fake = FakeHwmon::new();
        fake.add_fan(1, 1, 800);
        fake.add_pwm(1, 1, 128);
        let controller = LinuxFanController::with_base(fake.base_path());

        let fans = controller.discover().unwrap();
        assert_eq!(fans.len(), 1);
        assert!(fans[0].controllable);
        assert_eq!(fans[0].pwm, Some(128));
    }

    #[test]
    fn discover_readonly_pwm_fan() {
        let fake = FakeHwmon::new();
        fake.add_fan(1, 1, 800);
        fake.add_readonly_pwm(1, 1, 200);
        let controller = LinuxFanController::with_base(fake.base_path());

        let fans = controller.discover().unwrap();
        assert_eq!(fans.len(), 1);
        assert!(!fans[0].controllable);
        assert_eq!(fans[0].pwm, Some(200));
    }

    #[test]
    fn discover_multiple_fans_across_hwmon() {
        let fake = FakeHwmon::new();
        fake.add_fan(0, 1, 1100);
        fake.add_fan(0, 2, 1050);
        fake.add_fan(1, 1, 900);
        let controller = LinuxFanController::with_base(fake.base_path());

        let fans = controller.discover().unwrap();
        assert_eq!(fans.len(), 3);
        assert_eq!(fans[0].id, "hwmon0/fan1");
        assert_eq!(fans[1].id, "hwmon0/fan2");
        assert_eq!(fans[2].id, "hwmon1/fan1");
    }

    #[test]
    fn get_speed_reads_current_rpm() {
        let fake = FakeHwmon::new();
        fake.add_fan(2, 1, 1500);
        let controller = LinuxFanController::with_base(fake.base_path());

        let speed = controller.get_speed("hwmon2/fan1").unwrap();
        assert_eq!(speed, 1500);
    }

    #[test]
    fn get_speed_nonexistent_fan() {
        let fake = FakeHwmon::new();
        let controller = LinuxFanController::with_base(fake.base_path());

        let result = controller.get_speed("hwmon99/fan1");
        assert!(matches!(result, Err(FanControlError::FanNotFound(_))));
    }

    #[test]
    fn get_speed_invalid_id_format() {
        let fake = FakeHwmon::new();
        let controller = LinuxFanController::with_base(fake.base_path());

        let result = controller.get_speed("invalid_id");
        assert!(matches!(result, Err(FanControlError::FanNotFound(_))));
    }

    #[test]
    fn set_pwm_writes_enable_and_value() {
        let fake = FakeHwmon::new();
        fake.add_fan(0, 1, 1000);
        fake.add_pwm(0, 1, 255);
        let controller = LinuxFanController::with_base(fake.base_path());

        controller.set_pwm("hwmon0/fan1", 128).unwrap();

        let hwmon_dir = fake.base_path().join("hwmon0");
        let enable_value = fs::read_to_string(hwmon_dir.join("pwm1_enable")).unwrap();
        assert_eq!(enable_value, "1");

        let pwm_value = fs::read_to_string(hwmon_dir.join("pwm1")).unwrap();
        assert_eq!(pwm_value, "128");
    }

    #[test]
    fn set_pwm_not_controllable() {
        let fake = FakeHwmon::new();
        fake.add_fan(0, 1, 1000);
        // No PWM file created — fan is not controllable.
        let controller = LinuxFanController::with_base(fake.base_path());

        let result = controller.set_pwm("hwmon0/fan1", 128);
        assert!(matches!(result, Err(FanControlError::NotControllable(_))));
    }

    #[test]
    fn set_pwm_zero_and_max() {
        let fake = FakeHwmon::new();
        fake.add_fan(0, 1, 1000);
        fake.add_pwm(0, 1, 128);
        let controller = LinuxFanController::with_base(fake.base_path());

        controller.set_pwm("hwmon0/fan1", 0).unwrap();
        let pwm_value = fs::read_to_string(fake.base_path().join("hwmon0/pwm1")).unwrap();
        assert_eq!(pwm_value, "0");

        controller.set_pwm("hwmon0/fan1", 255).unwrap();
        let pwm_value = fs::read_to_string(fake.base_path().join("hwmon0/pwm1")).unwrap();
        assert_eq!(pwm_value, "255");
    }
}
