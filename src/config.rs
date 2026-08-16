// put id:"config_load", label:"Load Config (JSON)", output:"config.internal"
// put id:"config_save", label:"Save Config (JSON)", input:"config.internal"

//! Persistent configuration for custom fan curves.
//!
//! Stores `fancontrol.json` next to the executable (same directory as
//! `fancontrol.log`). Gracefully falls back to defaults on missing or
//! malformed files.

use std::path::PathBuf;

use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::fan::{validate_custom_curve, CustomFanCurve};

/// Persistent configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Saved custom fan curves to re-apply on startup.
    #[serde(default)]
    pub custom_curves: Vec<CustomFanCurve>,

    /// Automatically switch to Custom SmartFanMode when applying saved curves.
    #[serde(default = "default_true")]
    pub auto_smart_fan_mode: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            custom_curves: Vec::new(),
            auto_smart_fan_mode: true,
        }
    }
}

/// Path to the config file next to the executable.
pub fn config_path() -> PathBuf {
    std::env::current_exe()
        .unwrap_or_default()
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("fancontrol.json")
}

/// Report saved curves that fail the current safety limits, naming the file.
///
/// `Config` deserializes straight into `[u8; 10]`, so a hand-edited file can
/// carry any value from 0 to 255 and any ordering. Nothing here rejects or
/// repairs those — that is the caller's business, and the TUI sanitizes them
/// before applying. The point is that the user learns *which file* holds the
/// offending curve, from whichever front-end they happen to be running, rather
/// than only from the TUI's status line.
///
/// The config is deliberately not corrected on disk; see the persistence policy
/// note above `enforce_safety_minimums` in `src/tui.rs`.
///
/// Returns how many curves were reported, so the behaviour is testable rather
/// than only observable in the log.
fn report_invalid_curves(config: &Config, path: &std::path::Path) -> usize {
    let mut reported = 0;
    for curve in &config.custom_curves {
        if let Err(error) = validate_custom_curve(curve) {
            reported += 1;
            warn!(
                "Saved curve fan{}->sensor{} in {} is outside current safety limits ({}). \
                 It will be adjusted in memory before use; the file is left unchanged.",
                curve.fan_id,
                curve.sensor_id,
                path.display(),
                error
            );
        }
    }
    reported
}

/// Load configuration from disk. Returns defaults on any error.
pub fn load_config() -> Config {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(contents) => match serde_json::from_str::<Config>(&contents) {
            Ok(config) => {
                info!("Loaded config from {}", path.display());
                let _ = report_invalid_curves(&config, &path);
                config
            }
            Err(error) => {
                warn!("Malformed config at {}: {error}", path.display());
                Config::default()
            }
        },
        Err(_) => Config::default(),
    }
}

/// Save configuration to disk.
pub fn save_config(config: &Config) -> Result<(), std::io::Error> {
    let path = config_path();
    let json = serde_json::to_string_pretty(config).map_err(std::io::Error::other)?;
    std::fs::write(&path, json)?;
    info!("Saved config to {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_empty_curves() {
        let config = Config::default();
        assert!(config.custom_curves.is_empty());
        assert!(config.auto_smart_fan_mode);
    }

    #[test]
    fn roundtrip_serialize() {
        let config = Config {
            custom_curves: vec![CustomFanCurve {
                fan_id: 0,
                sensor_id: 3,
                steps: [1, 1, 1, 1, 2, 4, 6, 7, 8, 10],
            }],
            auto_smart_fan_mode: true,
        };
        let json = serde_json::to_string_pretty(&config).unwrap();
        let loaded: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.custom_curves.len(), 1);
        assert_eq!(loaded.custom_curves[0].fan_id, 0);
        assert_eq!(
            loaded.custom_curves[0].steps,
            [1, 1, 1, 1, 2, 4, 6, 7, 8, 10]
        );
    }

    #[test]
    fn load_empty_json_returns_defaults() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(config.custom_curves.is_empty());
        assert!(config.auto_smart_fan_mode);
    }

    #[test]
    fn load_config_from_nonexistent_returns_default() {
        // config_path() points to exe dir — won't exist in test environment
        let config = load_config();
        assert!(config.custom_curves.is_empty());
    }

    #[test]
    fn report_invalid_curves_counts_only_the_invalid() {
        let config = Config {
            custom_curves: vec![
                // Valid: meets every floor and is non-decreasing.
                CustomFanCurve {
                    fan_id: 0,
                    sensor_id: 3,
                    steps: [0, 0, 0, 0, 0, 0, 0, 1, 3, 5],
                },
                // Invalid: step 7 below its floor of 1.
                CustomFanCurve {
                    fan_id: 1,
                    sensor_id: 4,
                    steps: [0, 0, 0, 0, 0, 0, 0, 0, 3, 5],
                },
                // Invalid: out of range, the hand-edited-file case.
                CustomFanCurve {
                    fan_id: 2,
                    sensor_id: 5,
                    steps: [255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
                },
            ],
            auto_smart_fan_mode: true,
        };
        assert_eq!(
            report_invalid_curves(&config, std::path::Path::new("/tmp/fancontrol.json")),
            2
        );
    }

    #[test]
    fn report_invalid_curves_is_silent_on_a_clean_config() {
        // Positive control for the test above: the same call must return 0
        // when nothing is wrong, or the count proves nothing.
        let config = Config {
            custom_curves: vec![CustomFanCurve {
                fan_id: 0,
                sensor_id: 3,
                steps: [1, 1, 1, 1, 2, 4, 6, 7, 8, 10],
            }],
            auto_smart_fan_mode: true,
        };
        assert_eq!(
            report_invalid_curves(&config, std::path::Path::new("/tmp/fancontrol.json")),
            0
        );
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fancontrol.json");
        let config = Config {
            custom_curves: vec![
                CustomFanCurve {
                    fan_id: 0,
                    sensor_id: 3,
                    steps: [1, 1, 1, 1, 2, 4, 6, 7, 8, 10],
                },
                CustomFanCurve {
                    fan_id: 1,
                    sensor_id: 4,
                    steps: [0, 0, 1, 2, 3, 5, 7, 8, 9, 10],
                },
            ],
            auto_smart_fan_mode: false,
        };
        let json = serde_json::to_string_pretty(&config).unwrap();
        std::fs::write(&path, json).unwrap();
        let loaded: Config =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.custom_curves.len(), 2);
        assert_eq!(loaded.custom_curves[0], config.custom_curves[0]);
        assert_eq!(loaded.custom_curves[1], config.custom_curves[1]);
        assert!(!loaded.auto_smart_fan_mode);
    }
}
