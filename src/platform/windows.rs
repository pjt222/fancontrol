//! Windows fan controller backend using WMI (Win32_Fan).
//!
//! This module queries the `Win32_Fan` WMI class under `root\cimv2` to
//! discover fans and read their speed.  Actual PWM control is **not**
//! possible through the standard WMI fan class — vendor-specific WMI
//! namespaces or BIOS interfaces (Dell, ASUS, Lenovo, etc.) are required
//! for write access.

use serde::Deserialize;
use wmi::{COMLibrary, WMIConnection};

use super::FanController;
use crate::errors::FanControlError;
use crate::fan::Fan;

/// Detect whether this machine is a Lenovo system.
pub fn is_lenovo() -> bool {
    let com = match COMLibrary::new() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let wmi = match WMIConnection::new(com) {
        Ok(w) => w,
        Err(_) => return false,
    };

    #[derive(Deserialize)]
    #[serde(rename = "Win32_ComputerSystem")]
    #[serde(rename_all = "PascalCase")]
    struct ComputerSystem {
        manufacturer: String,
    }

    let results: Vec<ComputerSystem> = wmi
        .raw_query("SELECT Manufacturer FROM Win32_ComputerSystem")
        .unwrap_or_default();

    results
        .first()
        .map(|cs| cs.manufacturer.to_uppercase().contains("LENOVO"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// WMI data model
// ---------------------------------------------------------------------------

/// Maps to the WMI `Win32_Fan` class (root\cimv2).
///
/// Only the fields we actually use are included; `serde` will silently
/// ignore any extra properties returned by WMI.
#[derive(Deserialize, Debug)]
#[serde(rename = "Win32_Fan")]
#[serde(rename_all = "PascalCase")]
struct Win32Fan {
    /// WMI DeviceID — used as the unique fan identifier.
    #[serde(rename = "DeviceID")]
    device_id: String,

    /// Human-readable name assigned by the firmware / driver.
    name: String,

    /// Desired rotational speed reported by the firmware (RPM).
    /// Not every BIOS populates this field, so it is optional.
    desired_speed: Option<u32>,

    /// Indicates whether the fan uses active cooling (i.e. variable speed).
    /// When `true` the hardware *may* support PWM — but the standard WMI
    /// class does not expose a write interface.
    active_cooling: Option<bool>,
}

// ---------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------

/// Windows implementation of [`FanController`] backed by WMI.
pub struct WindowsFanController {
    wmi_connection: WMIConnection,
}

impl WindowsFanController {
    /// Create a new controller.
    ///
    /// Initialises COM and connects to the `root\cimv2` WMI namespace.
    pub fn new() -> Result<Self, FanControlError> {
        let com_library = COMLibrary::new().map_err(|e| {
            FanControlError::Platform(format!("failed to initialise COM library: {e}"))
        })?;
        let wmi_connection = WMIConnection::new(com_library).map_err(|e| {
            FanControlError::Platform(format!("failed to connect to WMI (root\\cimv2): {e}"))
        })?;

        Ok(Self { wmi_connection })
    }

    // -- internal helpers ---------------------------------------------------

    /// Execute a raw WQL query and return the deserialised results.
    fn query_fans(&self) -> Result<Vec<Win32Fan>, FanControlError> {
        let results: Vec<Win32Fan> = self
            .wmi_connection
            .raw_query("SELECT DeviceID, Name, DesiredSpeed, ActiveCooling FROM Win32_Fan")
            .map_err(|error| {
                FanControlError::Platform(format!("WMI query for Win32_Fan failed: {error}"))
            })?;

        Ok(results)
    }

    /// Convert a [`Win32Fan`] WMI record into our domain [`Fan`] struct.
    fn win32_fan_to_fan(wmi_fan: &Win32Fan) -> Fan {
        let speed_rpm = wmi_fan.desired_speed.unwrap_or(0);
        let is_controllable = wmi_fan.active_cooling.unwrap_or(false);

        Fan {
            id: wmi_fan.device_id.clone(),
            label: wmi_fan.name.clone(),
            speed_rpm,
            pwm: None, // WMI does not expose a PWM duty-cycle value
            controllable: is_controllable,
            min_rpm: None,
            max_rpm: None,
            curves: Vec::new(),
            full_speed_active: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Trait implementation
// ---------------------------------------------------------------------------

impl FanController for WindowsFanController {
    /// Discover all fans visible through the `Win32_Fan` WMI class.
    ///
    /// Returns an empty `Vec` when no fan objects are reported by the
    /// firmware — this is common on desktops whose BIOS does not publish
    /// WMI fan data.
    fn discover(&self) -> Result<Vec<Fan>, FanControlError> {
        let wmi_fans = self.query_fans()?;

        let fans = wmi_fans.iter().map(Self::win32_fan_to_fan).collect();

        Ok(fans)
    }

    /// Read the current speed (RPM) for the fan identified by `fan_id`.
    ///
    /// Re-queries WMI so the value is as fresh as the firmware reports.
    fn get_speed(&self, fan_id: &str) -> Result<u32, FanControlError> {
        let wmi_fans = self.query_fans()?;

        let matching_fan = wmi_fans
            .iter()
            .find(|fan| fan.device_id == fan_id)
            .ok_or_else(|| FanControlError::FanNotFound(fan_id.to_owned()))?;

        Ok(matching_fan.desired_speed.unwrap_or(0))
    }

    /// Attempt to set the PWM duty cycle for a fan.
    ///
    /// The standard `Win32_Fan` WMI class is **read-only** — it does not
    /// provide a method to change fan speed.  This implementation always
    /// returns [`FanControlError::NotControllable`] with guidance on
    /// vendor-specific alternatives.
    fn set_pwm(&self, fan_id: &str, _pwm: u8) -> Result<(), FanControlError> {
        // Even though we cannot set PWM, we validate that the fan exists
        // first so the caller gets the most specific error possible.
        let wmi_fans = self.query_fans()?;

        let fan_exists = wmi_fans.iter().any(|fan| fan.device_id == fan_id);

        if !fan_exists {
            return Err(FanControlError::FanNotFound(fan_id.to_owned()));
        }

        Err(FanControlError::NotControllable(format!(
            "Win32_Fan WMI class is read-only. \
             To control fan speed on Windows, use a vendor-specific interface \
             such as Dell BIOS WMI (root\\dcim\\sysman), ASUS WMI (via \
             atkexSvc), or a hardware monitoring tool like FanControl by Rem0o."
        )))
    }
}
