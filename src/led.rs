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
//! (Windows reporting external power; comment 5527617570), so the button keys
//! on the barrel adapter, not on external power. The register reports the
//! *selected* mode and id 4 the mode the button *shows*. The indicator
//! therefore reads id 4 and falls back to the register only when the read
//! returns nothing, labelling the fallback as derived.
//!
//! What is not measured, and what the labels must not claim: which physical
//! LED id 4 describes (its index matched the operator's colour in every
//! observed state, across the mode and the power source, which is a
//! correlation, not an identification), whether fans and power limits follow
//! the button without the barrel, and what any index other than 0 to 3 means.

use serde_json::{json, Value};

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
    /// 2026-09-02 15:27 by composition through the mode, and again on
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

/// Where the indicator's colour came from, or why there is none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedSource {
    /// `Lighting_Id 4` was read and its index is one of the four measured.
    Read,
    /// The lighting read returned nothing; the colour is the one measured for
    /// the SmartFanMode with the barrel adapter in, which is wrong without it
    /// (battery or USB-C dock power) with Performance selected.
    DerivedFromMode,
    /// The lighting read returned an index outside the four measured. No
    /// colour is painted: the device is in a state the tables do not cover,
    /// and in the one measured disagreement the index was right and the
    /// register wrong, so the register's colour is the weaker guess exactly
    /// here. The index itself is shown.
    UnmeasuredIndex,
    /// Neither a lighting index nor a known mode was read: nothing to show.
    Unavailable,
}

impl LedSource {
    /// Stable machine-readable name, used in the `led --json` output.
    pub fn key(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::DerivedFromMode => "derived_from_mode",
            Self::UnmeasuredIndex => "unmeasured_index",
            Self::Unavailable => "unavailable",
        }
    }
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
    /// Resolve from the two readings. A measured index wins. An index outside
    /// the measured four gives no colour and says so. With no index at all,
    /// a known mode gives the barrel-in colour labelled derived; with neither,
    /// nothing.
    pub fn resolve(state_index: Option<u32>, mode: Option<u32>) -> Self {
        let (colour, source) = match state_index {
            Some(index) => match LedColour::from_state_index(index) {
                Some(colour) => (Some(colour), LedSource::Read),
                None => (None, LedSource::UnmeasuredIndex),
            },
            None => match mode.and_then(LedColour::from_smart_fan_mode) {
                Some(colour) => (Some(colour), LedSource::DerivedFromMode),
                None => (None, LedSource::Unavailable),
            },
        };
        Self {
            colour,
            source,
            state_index,
            mode,
        }
    }

    /// Short label for a title bar: `white`, `white (derived)`, `unknown
    /// (index 7)`, or `N/A` when nothing was readable.
    pub fn short_label(&self) -> String {
        match (self.source, self.colour, self.state_index) {
            (LedSource::Read, Some(colour), _) => colour.label().to_string(),
            (LedSource::DerivedFromMode, Some(colour), _) => {
                format!("{} (derived)", colour.label())
            }
            (LedSource::UnmeasuredIndex, _, Some(index)) => format!("unknown (index {index})"),
            _ => "N/A".to_string(),
        }
    }

    /// One sentence for a CLI or a log line, naming the source honestly.
    pub fn describe(&self) -> String {
        let mode_text = match self.mode {
            Some(mode) => format!(
                "SmartFanMode {mode} ({})",
                crate::fan::smart_fan_mode_label(Some(mode))
            ),
            None => "SmartFanMode not readable".to_string(),
        };
        let index_text = match self.state_index {
            Some(index) => format!("Lighting_Id {POWER_BUTTON_LIGHTING_ID} index {index}"),
            None => format!("Lighting_Id {POWER_BUTTON_LIGHTING_ID} not readable"),
        };
        match (self.source, self.colour) {
            (LedSource::Read, Some(colour)) => format!(
                "button colour {} (read: {index_text}); {mode_text}",
                colour.label()
            ),
            (LedSource::DerivedFromMode, Some(colour)) => format!(
                "button colour {} (derived from the mode; {index_text}); {mode_text}",
                colour.label()
            ),
            (LedSource::UnmeasuredIndex, _) => format!(
                "button colour unknown ({index_text} is outside the measured 0 to 3); {mode_text}"
            ),
            _ => format!("button colour unknown ({index_text}); {mode_text}"),
        }
    }

    /// The `led --json` document. Keys are emitted in alphabetical order by
    /// `serde_json` (no `preserve_order`), and a test pins the exact line.
    pub fn to_json(self) -> Value {
        json!({
            "colour": self.colour.map(LedColour::label),
            "lighting_id": POWER_BUTTON_LIGHTING_ID,
            "smart_fan_mode": self.mode,
            "source": self.source.key(),
            "state_index": self.state_index,
        })
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
    fn the_two_tables_agree_with_the_barrel_adapter_in() {
        // Index i was read in the mode at position i of this list, barrel in.
        for (index, mode) in [(0, 1), (1, 2), (2, 3), (3, 255)] {
            assert_eq!(
                LedColour::from_state_index(index),
                LedColour::from_smart_fan_mode(mode)
            );
        }
    }

    #[test]
    fn a_read_index_wins_over_the_mode() {
        // The barrel-out case, measured 2026-09-03 15:07 and 16:50: register
        // 3, index 1, button white. The indicator must say white, not red.
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
    fn an_unmeasured_index_paints_no_colour_and_shows_the_index() {
        let led = LedIndicator::resolve(Some(7), Some(2));
        assert_eq!(led.colour, None);
        assert_eq!(led.source, LedSource::UnmeasuredIndex);
        assert_eq!(led.state_index, Some(7));
        assert_eq!(led.short_label(), "unknown (index 7)");
        assert!(led
            .describe()
            .contains("index 7 is outside the measured 0 to 3"));
        assert!(led.describe().contains("SmartFanMode 2 (Balanced)"));
    }

    #[test]
    fn nothing_readable_is_unavailable_not_derived() {
        let led = LedIndicator::resolve(None, None);
        assert_eq!(led.colour, None);
        assert_eq!(led.source, LedSource::Unavailable);
        assert_eq!(led.short_label(), "N/A");
        assert!(led.describe().contains("not readable"));
    }

    #[test]
    fn an_unknown_mode_without_an_index_is_unavailable() {
        let led = LedIndicator::resolve(None, Some(7));
        assert_eq!(led.colour, None);
        assert_eq!(led.source, LedSource::Unavailable);
        assert_eq!(led.short_label(), "N/A");
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
    fn json_line_is_exactly_what_the_readme_shows() {
        // The README quotes this line as the --json contract; serde_json
        // emits keys alphabetically, and this pins both the keys and the order.
        let led = LedIndicator::resolve(Some(1), Some(3));
        assert_eq!(
            led.to_json().to_string(),
            r#"{"colour":"white","lighting_id":4,"smart_fan_mode":3,"source":"read","state_index":1}"#
        );
    }

    #[test]
    fn json_reports_unavailable_when_nothing_was_read() {
        let led = LedIndicator::resolve(None, None);
        assert_eq!(
            led.to_json().to_string(),
            r#"{"colour":null,"lighting_id":4,"smart_fan_mode":null,"source":"unavailable","state_index":null}"#
        );
        let unmeasured = LedIndicator::resolve(Some(7), Some(3));
        assert_eq!(
            unmeasured.to_json().to_string(),
            r#"{"colour":null,"lighting_id":4,"smart_fan_mode":3,"source":"unmeasured_index","state_index":7}"#
        );
    }

    #[test]
    fn the_lighting_id_is_the_measured_one() {
        assert_eq!(POWER_BUTTON_LIGHTING_ID, 4);
    }
}
