//! Whether the boat floats here, now.
//!
//! A chart's soundings are the depth at the lowest water normally expected —
//! chart datum — and the tide is how much more (or less) there is at a given
//! moment. Neither number alone answers the question a navigator actually
//! has, which is not "how deep is it there" but "where can I not go, and
//! when does that change".
//!
//! ```text
//! water now  = charted depth + tide now
//! water then = charted depth + lowest tide in the window
//! needed     = draft + the margin you are willing to keep
//! ```
//!
//! Three outcomes follow, and the middle one is the point of the exercise:
//! somewhere that is fine at this moment and will not be in six hours is
//! exactly what a static chart cannot tell you.
//!
//! All depths are metres. US ENCs are published in metres — the whole
//! downloaded set reports `DSPM_DUNI = 1`, and the depth bands come out as
//! 1.8/3.6/5.4/9.1/18.2 m, which are 6/12/18/30/60 ft exactly — but a chart
//! from elsewhere might not be, and `rutter charts` checks rather than
//! assumes.
//!
//! What this deliberately does *not* model: barometric pressure, wind setup
//! and swell, none of which are in a harmonic prediction and any of which
//! can move the real water level by 0.2–0.3 m. That is a reason to keep a
//! margin, and a reason the margin is the navigator's to choose.

/// What the boat needs under it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Draft {
    /// Deepest part of the hull below the waterline, metres.
    pub draft_m: f64,
    /// Water the navigator wants under the keel on top of that, metres.
    ///
    /// Separate from the draft rather than folded into it so that the panel
    /// can show both, and because they are different kinds of number: one
    /// is a property of the boat and the other is a judgement about the day.
    pub safety_m: f64,
}

impl Draft {
    /// Total water needed, metres.
    ///
    /// Negative inputs are clamped away: a draft cannot be negative, and a
    /// negative margin would quietly turn the check into "tell me where I
    /// have already run aground".
    pub fn needed_m(&self) -> f64 {
        self.draft_m.max(0.0) + self.safety_m.max(0.0)
    }
}

impl Default for Draft {
    fn default() -> Self {
        // A small sailing boat, and a margin big enough to cover the weather
        // effects the harmonic prediction does not model. Something has to
        // be the default; this one errs shallow-cautious.
        Self {
            draft_m: 1.5,
            safety_m: 0.5,
        }
    }
}

/// How a charted depth stands against the boat and the tide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Clearance {
    /// Enough water now and through the window.
    Clear,
    /// Enough now, not at the window's low water.
    ///
    /// The state worth drawing: it is invisible on a paper chart and it is
    /// the one that strands people who entered on a falling tide.
    TightAtLow,
    /// Not enough water now.
    Foul,
}

/// The tide either side of the question, metres above chart datum.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TideWindow {
    /// Height now.
    pub now_m: f64,
    /// Lowest height between now and the end of the window.
    pub lowest_m: f64,
}

/// Assess one charted depth.
///
/// `charted_m` is the depth below chart datum as the chart prints it, so a
/// drying height — somewhere that is above datum and uncovers — is negative.
/// The arithmetic handles that without a special case, which is the reason
/// to keep the sign convention the chart's rather than inventing one.
pub fn assess(draft: &Draft, charted_m: f64, tide: &TideWindow) -> Clearance {
    let needed = draft.needed_m();
    if charted_m + tide.now_m < needed {
        Clearance::Foul
    } else if charted_m + tide.lowest_m < needed {
        Clearance::TightAtLow
    } else {
        Clearance::Clear
    }
}

/// How much water there is over a charted depth right now, metres.
///
/// Separate from [`assess`] because the number is worth showing even when
/// the verdict is "clear" — a readout of 4.2 m tells you more than a colour.
pub fn water_now_m(charted_m: f64, tide: &TideWindow) -> f64 {
    charted_m + tide.now_m
}

/// A tally of one view's worth of soundings, for the panel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub clear: usize,
    pub tight_at_low: usize,
    pub foul: usize,
}

impl Tally {
    pub fn count(&mut self, clearance: Clearance) {
        match clearance {
            Clearance::Clear => self.clear += 1,
            Clearance::TightAtLow => self.tight_at_low += 1,
            Clearance::Foul => self.foul += 1,
        }
    }

    pub fn total(&self) -> usize {
        self.clear + self.tight_at_low + self.foul
    }

    /// Whether there is anything the navigator should look at.
    pub fn any_concern(&self) -> bool {
        self.foul > 0 || self.tight_at_low > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boat() -> Draft {
        Draft {
            draft_m: 1.5,
            safety_m: 0.5,
        }
    }

    /// San Diego on 2026-11-03: +0.21 m now, dropping to +0.12 m at the
    /// next low. Real numbers, so the test reads like the situation.
    fn san_diego() -> TideWindow {
        TideWindow {
            now_m: 0.209,
            lowest_m: 0.116,
        }
    }

    #[test]
    fn needed_is_draft_plus_margin() {
        assert_eq!(boat().needed_m(), 2.0);
    }

    #[test]
    fn negative_inputs_cannot_shrink_what_is_needed() {
        // A negative margin would invert the whole check, so it is clamped
        // rather than trusted. Same for a draft typed with a stray minus.
        let d = Draft {
            draft_m: 1.5,
            safety_m: -5.0,
        };
        assert_eq!(d.needed_m(), 1.5);
        let d = Draft {
            draft_m: -1.5,
            safety_m: 0.5,
        };
        assert_eq!(d.needed_m(), 0.5);
    }

    #[test]
    fn deep_water_is_clear() {
        // A real sounding from US5SANCD, well outside the channel.
        assert_eq!(assess(&boat(), 23.7, &san_diego()), Clearance::Clear);
    }

    #[test]
    fn a_shoal_shallower_than_the_boat_is_foul() {
        // The 1.2 m sounding inside the bay entrance: 1.2 + 0.21 = 1.41 m,
        // against 2.0 m needed.
        assert_eq!(assess(&boat(), 1.2, &san_diego()), Clearance::Foul);
    }

    #[test]
    fn the_interesting_case_is_clear_now_and_not_at_low_water() {
        // 1.95 m charted: 2.16 m now, which clears; 2.07 m at the low,
        // which also clears. Drop the tide further and it does not.
        let falling = TideWindow {
            now_m: 0.209,
            lowest_m: -0.112, // the real spring low this week
        };
        assert_eq!(assess(&boat(), 1.95, &falling), Clearance::TightAtLow);
        // And it would have read as perfectly fine on the chart alone.
        assert!(water_now_m(1.95, &falling) > boat().needed_m());
    }

    #[test]
    fn a_negative_tide_takes_water_away() {
        // Chart datum is an average of low waters, not a floor. When the
        // tide goes below it there is *less* water than the chart prints,
        // and that has to come out of the arithmetic rather than be a
        // special case.
        let spring_low = TideWindow {
            now_m: -0.112,
            lowest_m: -0.112,
        };
        let shallow = Draft {
            draft_m: 1.0,
            safety_m: 0.3,
        };
        // 1.35 m charted is 1.35 clear of datum, which beats the 1.3 m
        // needed — until the tide is 11 cm below datum.
        assert_eq!(
            assess(
                &shallow,
                1.35,
                &TideWindow {
                    now_m: 0.0,
                    lowest_m: 0.0
                }
            ),
            Clearance::Clear
        );
        assert_eq!(assess(&shallow, 1.35, &spring_low), Clearance::Foul);
    }

    #[test]
    fn a_drying_height_is_negative_and_foul() {
        // DEPARE carries DRVAL1 = -1.5 for ground that uncovers. It must
        // never read as clear, whatever the tide, unless the tide genuinely
        // covers it deeply enough.
        let drying = -1.5;
        assert_eq!(assess(&boat(), drying, &san_diego()), Clearance::Foul);
        // Even a big tide only just floats a 2 m boat over it.
        let big = TideWindow {
            now_m: 2.0,
            lowest_m: 2.0,
        };
        assert_eq!(assess(&boat(), drying, &big), Clearance::Foul);
        let huge = TideWindow {
            now_m: 4.0,
            lowest_m: 4.0,
        };
        assert_eq!(assess(&boat(), drying, &huge), Clearance::Clear);
    }

    #[test]
    fn exactly_enough_water_is_clear_not_foul() {
        // The boundary is inclusive: needing 2.0 m and having exactly 2.0 m
        // is not an alarm. Chart soundings are rounded anyway, which is what
        // the safety margin is for.
        let flat = TideWindow {
            now_m: 0.0,
            lowest_m: 0.0,
        };
        assert_eq!(assess(&boat(), 2.0, &flat), Clearance::Clear);
        // A millimetre less is.
        assert_eq!(assess(&boat(), 1.999, &flat), Clearance::Foul);
    }

    #[test]
    fn foul_wins_over_tight_when_both_would_apply() {
        // Somewhere that is already too shallow is Foul, not TightAtLow —
        // the worse verdict has to be the one shown.
        let falling = TideWindow {
            now_m: 0.0,
            lowest_m: -0.5,
        };
        assert_eq!(assess(&boat(), 0.5, &falling), Clearance::Foul);
    }

    #[test]
    fn a_rising_tide_never_produces_tight_at_low() {
        // `lowest_m` is the minimum over the window including now, so it can
        // never exceed `now_m`. If it somehow did, the result must not be
        // a nonsense state.
        let rising = TideWindow {
            now_m: 0.0,
            lowest_m: 1.0,
        };
        assert_eq!(assess(&boat(), 2.5, &rising), Clearance::Clear);
        assert_eq!(assess(&boat(), 1.0, &rising), Clearance::Foul);
    }

    #[test]
    fn the_tally_counts_and_reports_concern() {
        let mut t = Tally::default();
        assert!(!t.any_concern());
        assert_eq!(t.total(), 0);

        t.count(Clearance::Clear);
        t.count(Clearance::Clear);
        assert!(!t.any_concern(), "all clear is not a concern");

        t.count(Clearance::TightAtLow);
        assert!(t.any_concern(), "tight at low water is worth showing");

        t.count(Clearance::Foul);
        assert_eq!(
            t,
            Tally {
                clear: 2,
                tight_at_low: 1,
                foul: 1
            }
        );
        assert_eq!(t.total(), 4);
    }
}
