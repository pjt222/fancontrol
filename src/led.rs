// put id:"led_indicator", label:"Power-button LED indicator (read id 4, fall back to mode)", input:"lighting_state.internal, smart_fan_mode.internal", output:"led_indicator.internal"

//! The power-button LED indicator: what colour the button shows, and where
//! that knowledge came from.
//!
//! Two tables are measured on the Legion 82RG, both through SmartFanMode in the
//! attended run of `tools/Get-LenovoLighting.ps1` on 2026-09-02 15:27 (issue
//! #44 comments 5510526944 and 5510719791):
//!
//! | SmartFanMode | `Lighting_Id 4 -> Current_State_Type` | operator's colour |
//! |---|---|---|
//! | 1 Quiet | 0 | `blue` |
//! | 2 Balanced | 1 | `white` |
//! | 3 Performance | 2 | `red` |
//! | 255 Custom | 3 | `all thre (at least red and blue)` |
//!
//! The index and the register carry different information. Measured 2026-09-03
//! 15:07, run 2 of `tools/Watch-LenovoLightingVsMode.ps1` (issue #44 comment
//! 5526430479, corrected in 5526685189): with Performance selected and the AC
//! adapter pulled, `GetSmartFanMode` kept reading 3 while id 4 read 1 and the
//! operator saw the button white; on replug id 4 read 2 again and the button
//! was red. The same happened at 16:50 with a USB-C dock powering the machine
//! (Windows reporting external power), so the button keys on the barrel
//! adapter, not on external power. The register reports the *selected* mode
//! and id 4 the mode the button *shows*. The indicator therefore reads id 4 and falls back to the
//! register only when the read fails, labelling the fallback as derived.
//!
//! What is not measured, and what the labels must not claim: which physical
//! LED id 4 describes (its index matched the operator's colour in every
//! observed state, across the mode and the adapter, which is a correlation, not
//! an identification), whether fans and power limits follow the button on
//! battery, and what any index other than 0 to 3 means.

use crate::fan::smart_fan_mode;

/// The `Lighting_Id` whose `Current_State_Type` tracks the power-button colour.
///
/// One of the two real ids on the 82RG (`LENOVO_LIGHTING_DATA` lists six, four
/// of them `Lighting_Id = 255` placeholders). Read with
/// `Get_Lighting_Current_Status`; never written, see
/// `tests/lighting_setter_forbidden.rs`.
pub const POWER_BUTTON_LIGHTING_ID: u32 = 4;

/// A colour the power button has been seen to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedColour {
    Blue,
    White,
    Red,
    /// More than one colour lit at once. The operator wrote "all thre (at
    /// least red and blue)" for Custom; no single colour name is measured.
    Multi,
}

impl LedColour {
    /// Colour for a `Lighting_Id 4 -> Current_State_Type` index. Measured
    /// 2026-09-02 15:27 by composition through the mode, and once more on
    /// 2026-09-03 when index 1 and `white` appeared together with the register
    /// at 3. Indices outside 0 to 3 have never been read and map to `None`.
    pub fn from_state_index(index: u32) -> Option<Self> {
        match index {
            0 => Some(Self::Blue),
            1 => Some(Self::White),
            2 => Some(Self::Red),
            3 => Some(Self::Multi),
            _ => None,
        }
    }

    /// Colour the button showed for a SmartFanMode with the barrel adapter in.
    /// Measured 2026-09-02 15:27 (operator's report per mode). Without the
    /// barrel (on battery, and on USB-C dock power) with Performance selected
    /// the button showed white, not red, so this table is the fallback, not
    /// the source.
    pub fn from_smart_fan_mode(mode: u32) -> Option<Self> {
        match mode {
            smart_fan_mode::QUIET => Some(Self::Blue),
            smart_fan_mode::BALANCED => Some(Self::White),
            smart_fan_mode::PERFORMANCE => Some(Self::Red),
            smart_fan_mode::CUSTOM => Some(Self::Multi),
            _ => None,
        }
    }

    /// Lower-case colour word, as the operator reported it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Blue => "blue",
            Self::White => "white",
            Self::Red => "red",
            Self::Multi => "multi",
        }
    }

    /// An RGB approximation for rendering. The operator's words are the
    /// measurement; these numbers are a display choice.
    pub fn rgb(self) -> (u8, u8, u8) {
        match self {
            Self::Blue => (70, 130, 255),
            Self::White => (235, 235, 235),
            Self::Red => (255, 70, 70),
            Self::Multi => (200, 90, 220),
        }
    }
}

/// Where the indicator's colour came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedSource {
    /// `Lighting_Id 4` was read and its index is one of the four measured.
    Read,
    /// The read failed or returned an unmeasured index; the colour is the one
    /// measured for the SmartFanMode with the barrel adapter in, which is wrong
    /// without it (battery or USB-C dock power) with Performance selected.
    DerivedFromMode,
}

/// The resolved indicator: a colour, its source, and the raw readings so the
/// display can say exactly what was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedIndicator {
    pub colour: Option<LedColour>,
    pub source: LedSource,
    /// The `Current_State_Type` as read, whether or not it mapped to a colour.
    pub state_index: Option<u32>,
    /// The SmartFanMode as read.
    pub mode: Option<u32>,
}

impl LedIndicator {
    /// Resolve from the two readings. The read index wins when it is one of
    /// the four measured; otherwise the mode's colour is used and labelled
    /// derived; with neither, `colour` is `None`.
    pub fn resolve(state_index: Option<u32>, mode: Option<u32>) -> Self {
        if let Some(colour) = state_index.and_then(LedColour::from_state_index) {
            return Self {
                colour: Some(colour),
                source: LedSource::Read,
                state_index,
                mode,
            };
        }
        Self {
            colour: mode.and_then(LedColour::from_smart_fan_mode),
            source: LedSource::DerivedFromMode,
            state_index,
            mode,
        }
    }

    /// Short label for a title bar: `white`, `white (derived)`, `unknown
    /// (index 7)`, or `N/A` when nothing was readable.
    pub fn short_label(&self) -> String {
        match (self.colour, self.source, self.state_index) {
            (Some(colour), LedSource::Read, _) => colour.label().to_string(),
            (Some(colour), LedSource::DerivedFromMode, Some(index)) => {
                format!("{} (derived; index {index} unmeasured)", colour.label())
            }
            (Some(colour), LedSource::DerivedFromMode, None) => {
                format!("{} (derived)", colour.label())
            }
            (None, _, Some(index)) => format!("unknown (index {index})"),
            (None, _, None) => "N/A".to_string(),
        }
    }

    /// One sentence for a CLI or a log line, naming the source honestly.
    pub fn describe(&self) -> String {
        let index_text = match self.state_index {
            Some(index) => format!("Lighting_Id {POWER_BUTTON_LIGHTING_ID} index {index}"),
            None => format!("Lighting_Id {POWER_BUTTON_LIGHTING_ID} not readable"),
        };
        let mode_text = match self.mode {
            Some(mode) => format!(
                "SmartFanMode {mode} ({})",
                crate::fan::smart_fan_mode_label(Some(mode))
            ),
            None => "SmartFanMode not readable".to_string(),
        };
        match (self.colour, self.source) {
            (Some(colour), LedSource::Read) => format!(
                "button colour {} (read: {index_text}); {mode_text}",
                colour.label()
            ),
            (Some(colour), LedSource::DerivedFromMode) => format!(
                "button colour {} (derived from the mode; {index_text}); {mode_text}",
                colour.label()
            ),
            (None, _) => format!("button colour unknown ({index_text}); {mode_text}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_index_table_matches_the_2026_09_02_measurement() {
        assert_eq!(LedColour::from_state_index(0), Some(LedColour::Blue));
        assert_eq!(LedColour::from_state_index(1), Some(LedColour::White));
        assert_eq!(LedColour::from_state_index(2), Some(LedColour::Red));
        assert_eq!(LedColour::from_state_index(3), Some(LedColour::Multi));
        assert_eq!(LedColour::from_state_index(4), None);
        assert_eq!(LedColour::from_state_index(255), None);
    }

    #[test]
    fn mode_table_matches_the_2026_09_02_measurement() {
        assert_eq!(LedColour::from_smart_fan_mode(1), Some(LedColour::Blue));
        assert_eq!(LedColour::from_smart_fan_mode(2), Some(LedColour::White));
        assert_eq!(LedColour::from_smart_fan_mode(3), Some(LedColour::Red));
        assert_eq!(LedColour::from_smart_fan_mode(255), Some(LedColour::Multi));
        assert_eq!(LedColour::from_smart_fan_mode(0), None);
        assert_eq!(LedColour::from_smart_fan_mode(4), None);
    }

    #[test]
    fn the_two_tables_agree_on_ac_power() {
        // Index i was read in the mode at position i of this list, on AC.
        for (index, mode) in [(0, 1), (1, 2), (2, 3), (3, 255)] {
            assert_eq!(
                LedColour::from_state_index(index),
                LedColour::from_smart_fan_mode(mode)
            );
        }
    }

    #[test]
    fn a_read_index_wins_over_the_mode() {
        // The battery case, measured 2026-09-03 15:07: register 3, index 1,
        // button white. The indicator must say white, not red.
        let led = LedIndicator::resolve(Some(1), Some(3));
        assert_eq!(led.colour, Some(LedColour::White));
        assert_eq!(led.source, LedSource::Read);
        assert_eq!(led.short_label(), "white");
    }

    #[test]
    fn a_failed_read_falls_back_to_the_mode_and_says_so() {
        let led = LedIndicator::resolve(None, Some(3));
        assert_eq!(led.colour, Some(LedColour::Red));
        assert_eq!(led.source, LedSource::DerivedFromMode);
        assert_eq!(led.short_label(), "red (derived)");
    }

    #[test]
    fn an_unmeasured_index_falls_back_but_stays_visible() {
        let led = LedIndicator::resolve(Some(7), Some(2));
        assert_eq!(led.colour, Some(LedColour::White));
        assert_eq!(led.source, LedSource::DerivedFromMode);
        assert_eq!(led.state_index, Some(7));
        assert_eq!(led.short_label(), "white (derived; index 7 unmeasured)");
    }

    #[test]
    fn an_unmeasured_index_with_no_mode_is_unknown_not_a_colour() {
        let led = LedIndicator::resolve(Some(7), None);
        assert_eq!(led.colour, None);
        assert_eq!(led.short_label(), "unknown (index 7)");
    }

    #[test]
    fn nothing_readable_is_not_available() {
        let led = LedIndicator::resolve(None, None);
        assert_eq!(led.colour, None);
        assert_eq!(led.short_label(), "N/A");
        assert!(led.describe().contains("not readable"));
    }

    #[test]
    fn describe_names_the_source() {
        let read = LedIndicator::resolve(Some(1), Some(3));
        assert!(read
            .describe()
            .starts_with("button colour white (read: Lighting_Id 4 index 1)"));
        assert!(read.describe().contains("SmartFanMode 3 (Performance)"));
        let derived = LedIndicator::resolve(None, Some(1));
        assert!(derived
            .describe()
            .starts_with("button colour blue (derived from the mode; Lighting_Id 4 not readable)"));
    }

    #[test]
    fn the_lighting_id_is_the_measured_one() {
        assert_eq!(POWER_BUTTON_LIGHTING_ID, 4);
    }
}
