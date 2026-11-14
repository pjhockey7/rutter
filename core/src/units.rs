//! What a length is called when it is shown to someone.
//!
//! Everything inside this crate is metres, and stays metres: a chart is
//! metres, the tide model is metres, the clearance arithmetic is metres.
//! This is the last step before a number reaches a person, and nothing
//! upstream of the panel should ever hold a converted value — a figure
//! that might be feet is a figure that will eventually be compared
//! against one that is metres.
//!
//! ## Why three
//!
//! US charts print feet. British and Canadian charts printed fathoms for
//! a century and plenty of boats still carry them. The rest of the world,
//! and every modern survey, is metres. A plotter that cannot match the
//! paper on the chart table is a plotter you have to do arithmetic
//! against at exactly the moment you should not be doing arithmetic.

/// Which unit lengths are shown in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Length {
    #[default]
    Metres,
    Feet,
    Fathoms,
}

/// Feet in a metre. The international foot, exactly 0.3048 m.
const FEET_PER_METRE: f64 = 1.0 / 0.3048;

/// Metres in a fathom: six feet, exactly.
const METRES_PER_FATHOM: f64 = 1.828_8;

impl Length {
    /// In the order they appear in the panel.
    pub const ALL: [Self; 3] = [Self::Metres, Self::Feet, Self::Fathoms];

    /// What to call it in a list of choices.
    pub fn label(self) -> &'static str {
        match self {
            Self::Metres => "Metres",
            Self::Feet => "Feet",
            Self::Fathoms => "Fathoms",
        }
    }

    /// What follows the number.
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Metres => "m",
            Self::Feet => "ft",
            Self::Fathoms => "fm",
        }
    }

    /// As stored in the settings file.
    pub fn key(self) -> &'static str {
        match self {
            Self::Metres => "metres",
            Self::Feet => "feet",
            Self::Fathoms => "fathoms",
        }
    }

    /// Read back from the settings file, forgivingly — someone editing it
    /// by hand will write `meters`, and being right about the spelling of
    /// a word is not the point.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "metres" | "meters" | "metre" | "meter" | "m" => Some(Self::Metres),
            "feet" | "foot" | "ft" => Some(Self::Feet),
            "fathoms" | "fathom" | "fm" => Some(Self::Fathoms),
            _ => None,
        }
    }

    /// The unit to use for a distance *across* the water rather than down
    /// through it.
    ///
    /// Fathoms are a sounding unit. Nobody gives a swing radius or a GPS
    /// accuracy in fathoms, and "within 27 fathoms" reads as a mistake
    /// even though the arithmetic is right — so a boat set to fathoms
    /// gets feet for the horizontal, which is what the same person would
    /// say out loud.
    pub fn horizontal(self) -> Self {
        match self {
            Self::Fathoms => Self::Feet,
            other => other,
        }
    }

    /// Metres in this unit.
    pub fn from_metres(self, metres: f64) -> f64 {
        match self {
            Self::Metres => metres,
            Self::Feet => metres * FEET_PER_METRE,
            Self::Fathoms => metres / METRES_PER_FATHOM,
        }
    }

    /// This unit back to metres, for a control that is edited in it.
    pub fn to_metres(self, value: f64) -> f64 {
        match self {
            Self::Metres => value,
            Self::Feet => value / FEET_PER_METRE,
            Self::Fathoms => value * METRES_PER_FATHOM,
        }
    }

    /// Decimals worth showing.
    ///
    /// Not a constant, because the units are not the same size. A foot is
    /// 0.305 m and a fathom 1.83 m, so the same number of decimals means
    /// a very different reading in each — whole feet would be a coarser
    /// step than metres to one place, and a 1.80 m draft and a 1.95 m
    /// draft would print identically.
    ///
    /// What the places are chosen against is not metres, though: it is
    /// the decision being made. A sounding has to be readable to about
    /// [`COARSE_LIMIT_M`], and a clearance — where the whole question is
    /// whether one number is above another — to about
    /// [`FINE_LIMIT_M`]. Those are the limits the test holds this to, and
    /// they are what makes a unit change safe rather than merely tidy.
    fn places(self, fine: bool) -> usize {
        match (self, fine) {
            (Self::Metres, false) => 1,
            (Self::Metres, true) => 2,
            (Self::Feet, _) => 1,
            (Self::Fathoms, false) => 1,
            (Self::Fathoms, true) => 2,
        }
    }

    /// A length with its unit, at the precision a panel wants.
    pub fn show(self, metres: f64) -> String {
        self.format(metres, false)
    }

    /// The same, one decimal finer — for clearance, where the question is
    /// whether a number is above or below another one and the difference
    /// can be small.
    pub fn show_precise(self, metres: f64) -> String {
        self.format(metres, true)
    }

    /// With an explicit sign, for a tide height which can be below datum.
    pub fn show_signed(self, metres: f64) -> String {
        let value = self.from_metres(metres);
        format!(
            "{:+.*} {}",
            self.places(true),
            round_to(value, self.places(true)),
            self.suffix()
        )
    }

    fn format(self, metres: f64, fine: bool) -> String {
        let places = self.places(fine);
        format!(
            "{:.*} {}",
            places,
            round_to(self.from_metres(metres), places),
            self.suffix()
        )
    }
}

/// The coarsest a depth may read and still be worth printing: a fifth of
/// a metre, about eight inches.
pub const COARSE_LIMIT_M: f64 = 0.2;

/// The coarsest a *clearance* may read. Five centimetres, two inches —
/// fine enough that rounding never turns a boat that fits into one that
/// does not, or the reverse.
pub const FINE_LIMIT_M: f64 = 0.05;

/// Round away from a value that would print as a negative zero.
///
/// `format!("{:.1}", -0.04)` is `"-0.0"`, which on a tide readout means
/// "below datum" and is simply wrong at four centimetres above it.
fn round_to(value: f64, places: usize) -> f64 {
    let scale = 10f64.powi(places as i32);
    let rounded = (value * scale).round() / scale;
    if rounded == 0.0 {
        0.0
    } else {
        rounded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_conversions_are_the_defined_ones() {
        // A foot is exactly 0.3048 m and a fathom exactly six feet. These
        // are definitions, not measurements, so they should be exact to
        // floating point rather than approximately right.
        assert!((Length::Feet.from_metres(0.3048) - 1.0).abs() < 1e-12);
        assert!((Length::Fathoms.from_metres(1.8288) - 1.0).abs() < 1e-12);
        assert!((Length::Fathoms.from_metres(0.3048 * 6.0) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_round_trip_through_a_unit_returns_the_same_metres() {
        // A slider is edited in the shown unit and stored in metres, so a
        // value that drifts on every redraw would walk a draft setting
        // away from what was typed.
        for unit in Length::ALL {
            for metres in [0.0, 0.5, 1.8, 9.9974, 123.456] {
                let there_and_back = unit.to_metres(unit.from_metres(metres));
                assert!(
                    (there_and_back - metres).abs() < 1e-9,
                    "{unit:?} {metres} -> {there_and_back}"
                );
            }
        }
    }

    #[test]
    fn the_chart_table_numbers_come_out_right() {
        // 32.8 ft is the depth that caught this out once before: it is
        // 9.9974 m, not 9.9986.
        assert_eq!(Length::Feet.show(9.997_44), "32.8 ft");
        assert_eq!(Length::Metres.show(9.997_44), "10.0 m");
        // Six feet is one fathom, which is the whole point of the unit.
        assert_eq!(Length::Fathoms.show(1.8288), "1.0 fm");
        assert_eq!(Length::Fathoms.show(18.288), "10.0 fm");
    }

    #[test]
    fn fathoms_are_not_used_for_distance_over_the_ground() {
        // "within 27 fathoms" is arithmetically right and reads as a bug.
        assert_eq!(Length::Fathoms.horizontal(), Length::Feet);
        assert_eq!(Length::Metres.horizontal(), Length::Metres);
        assert_eq!(Length::Feet.horizontal(), Length::Feet);
    }

    #[test]
    fn nothing_prints_as_minus_zero() {
        // A tide four centimetres above datum showing "-0.0 m" says it is
        // below datum, which is the one thing the sign is there to say.
        assert_eq!(Length::Metres.show_signed(-0.004), "+0.00 m");
        assert_eq!(Length::Metres.show_signed(0.004), "+0.00 m");
        assert_eq!(Length::Feet.show(-0.01), "0.0 ft");
        // And a real negative keeps its sign.
        assert_eq!(Length::Metres.show_signed(-0.4), "-0.40 m");
    }

    #[test]
    fn every_unit_reads_finely_enough_to_decide_on() {
        // The rule that sets the decimal places, and the reason whole
        // feet are not used: a foot is 0.305 m, so "6 ft" would cover
        // everything from a 1.68 m draft to a 1.98 m draft.
        //
        // The limits are absolute rather than relative to metres, because
        // what matters is whether the number can be acted on — not
        // whether it happens to match what another unit would show.
        for fine in [false, true] {
            let limit = if fine { FINE_LIMIT_M } else { COARSE_LIMIT_M };
            for unit in Length::ALL {
                let step_m = unit.to_metres(10f64.powi(-(unit.places(fine) as i32)));
                assert!(
                    step_m <= limit + 1e-12,
                    "{unit:?} fine={fine} steps by {step_m:.4} m, past the {limit} m limit"
                );
            }
        }
        assert_eq!(Length::Metres.show(3.15), "3.2 m");
        assert_eq!(Length::Feet.show(3.15), "10.3 ft");
        assert_eq!(Length::Fathoms.show_precise(3.15), "1.72 fm");
    }

    #[test]
    fn the_settings_spelling_survives_a_round_trip() {
        for unit in Length::ALL {
            assert_eq!(Length::parse(unit.key()), Some(unit), "{unit:?}");
        }
        // And what somebody would actually type.
        assert_eq!(Length::parse("meters"), Some(Length::Metres));
        assert_eq!(Length::parse("  FEET "), Some(Length::Feet));
        assert_eq!(Length::parse("furlongs"), None);
    }
}
