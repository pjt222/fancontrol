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

use crate::fan::CustomFanCurve;

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

/// Load configuration from disk. Returns defaults on any error.
pub fn load_config() -> Config {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(contents) => match serde_json::from_str(&contents) {
            Ok(config) => {
                info!("Loaded config from {}", path.display());
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
