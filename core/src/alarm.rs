//! Watching for the two things that happen while you are not looking.
//!
//! An anchor dragging at three in the morning, and water shoaling under
//! the keel. Both share a shape: a condition that is usually false, that
//! nobody is watching for, and that matters enough to wake someone up.
//!
//! Which makes the hard part *not* the condition. It is everything around
//! it:
//!
//! * **A single bad fix must not wake the boat.** A GPS throws an outlier
//!   occasionally — a reflection off a cliff, a moment of poor geometry —
//!   and a naive radius test fires on it. An alarm that cries wolf once a
//!   night gets switched off, and then it is not there on the night it
//!   matters. So a breach has to persist across several consecutive fixes
//!   before it counts.
//! * **It must not chatter on the boundary.** Swinging to a tide with the
//!   radius set a little tight means crossing it every few minutes. The
//!   alarm clears at a *smaller* radius than it fires at, so a boat
//!   hovering on the line triggers once rather than forty times.
//! * **No reading is not a reading of zero.** A depth sounder that loses
//!   bottom in a swell reports nothing, or reports zero. Treating zero as
//!   "no water" fires the shallow alarm in the middle of an ocean;
//!   treating a stale reading as current means the alarm quietly stops
//!   watching when the sounder fails, which is worse.
//! * **Acknowledging silences, it does not cancel.** The condition is
//!   still true. Silencing the noise and leaving the warning on screen is
//!   the only honest behaviour — and it must sound again if the state
//!   clears and returns.

use crate::LatLon;

/// Where the anchor went down, and how far the boat may go from it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Anchor {
    pub at: LatLon,
    /// How far the boat may legitimately swing, metres.
    ///
    /// Scope plus the boat's length plus some slack — not the scope
    /// alone. A boat lying to 30 m of chain in 5 m of water swings on a
    /// circle of nearly 30 m radius, and its stern is a boat-length
    /// further out than that.
    pub radius_m: f64,
}

/// What to watch for.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Watching {
    /// The anchor, once it is set. `None` means not anchored.
    pub anchor: Option<Anchor>,
    /// Alarm below this depth under the transducer, metres. `None` means
    /// not watching the depth.
    pub shallow_m: Option<f64>,
}

/// Which alarm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// The boat has left the circle it was anchored in.
    AnchorDrag,
    /// There is less water under the transducer than asked for.
    Shallow,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::AnchorDrag => "Anchor dragging",
            Self::Shallow => "Shallow water",
        }
    }
}

/// An alarm that is currently true.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Active {
    pub kind: Kind,
    /// How far out of the circle, or how much water there is. The number
    /// the reading turns on, so the panel can say it without recomputing.
    pub value_m: f64,
    /// Whether the noise has been silenced. The warning stays either way.
    pub acknowledged: bool,
}

/// How twitchy to be.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sensitivity {
    /// Consecutive breaching readings before an alarm fires.
    ///
    /// At a fix a second this is seconds of delay, bought to stop a
    /// single GPS outlier waking the boat. An alarm that cries wolf gets
    /// switched off, and then it is not there on the night it matters.
    pub confirmations: u32,
    /// How far back inside the limit the reading must come before the
    /// alarm clears — the hysteresis.
    ///
    /// As a fraction of the radius for the anchor, and as metres for the
    /// depth. Without it a boat swinging on the line triggers over and
    /// over.
    pub clear_fraction: f64,
    pub clear_margin_m: f64,
    /// A reading older than this is no reading at all, seconds.
    pub stale_secs: i64,
}

impl Default for Sensitivity {
    fn default() -> Self {
        Self {
            // Three seconds at a fix a second. Long enough to reject an
            // outlier, short enough that a boat actually dragging has not
            // gone anywhere.
            confirmations: 3,
            // Clears at nine tenths of the radius.
            clear_fraction: 0.9,
            // Half a metre of extra water before the depth alarm rests.
            clear_margin_m: 0.5,
            // A sounder that stops reporting must stop being believed,
            // and ten seconds is several missed pings.
            stale_secs: 10,
        }
    }
}

/// One alarm's running state.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct State {
    breaches: u32,
    firing: bool,
    acknowledged: bool,
}

/// The watchkeeper.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Watch {
    pub watching: Watching,
    pub sensitivity: Sensitivity,
    anchor: State,
    shallow: State,
    active: Vec<Active>,
}

/// What a reading consists of.
///
/// Both halves optional and separately stamped, because they arrive from
/// different instruments at different rates and either can fail on its
/// own — a GPS can have a fix while the sounder has lost bottom.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Reading {
    pub position: Option<(LatLon, i64)>,
    pub depth_m: Option<(f64, i64)>,
}

impl Watch {
    /// Take a reading. Returns true if the set of alarms changed, which
    /// is when something needs to be said or sounded.
    pub fn update(&mut self, reading: &Reading, now: i64) -> bool {
        let before = self.active.clone();

        self.update_anchor(reading, now);
        self.update_shallow(reading, now);

        self.active.clear();
        if self.anchor.firing {
            self.active.push(Active {
                kind: Kind::AnchorDrag,
                value_m: self.anchor_excursion(reading, now).unwrap_or(0.0),
                acknowledged: self.anchor.acknowledged,
            });
        }
        if self.shallow.firing {
            self.active.push(Active {
                kind: Kind::Shallow,
                value_m: reading.depth_m.map_or(0.0, |(d, _)| d),
                acknowledged: self.shallow.acknowledged,
            });
        }

        before != self.active
    }

    /// Alarms currently true.
    pub fn active(&self) -> &[Active] {
        &self.active
    }

    /// Whether anything should be making a noise right now.
    pub fn sounding(&self) -> bool {
        self.active.iter().any(|a| !a.acknowledged)
    }

    /// Silence the noise. The warnings stay while the conditions do, and
    /// an alarm that clears and returns sounds again.
    pub fn acknowledge(&mut self) {
        self.anchor.acknowledged = true;
        self.shallow.acknowledged = true;
        for alarm in &mut self.active {
            alarm.acknowledged = true;
        }
    }

    /// Drop the anchor here, with this swinging room.
    pub fn set_anchor(&mut self, at: LatLon, radius_m: f64) {
        self.watching.anchor = Some(Anchor {
            at,
            // A radius of nothing would fire the instant the GPS twitched.
            radius_m: radius_m.max(5.0),
        });
        self.anchor = State::default();
    }

    pub fn weigh_anchor(&mut self) {
        self.watching.anchor = None;
        self.anchor = State::default();
        self.active.retain(|a| a.kind != Kind::AnchorDrag);
    }

    pub fn set_shallow(&mut self, metres: Option<f64>) {
        self.watching.shallow_m = metres.filter(|m| *m > 0.0);
        self.shallow = State::default();
        if self.watching.shallow_m.is_none() {
            self.active.retain(|a| a.kind != Kind::Shallow);
        }
    }

    /// How far outside the circle the boat is, metres. `None` if there is
    /// no anchor set or no usable fix.
    fn anchor_excursion(&self, reading: &Reading, now: i64) -> Option<f64> {
        let anchor = self.watching.anchor?;
        let (at, stamped) = reading.position?;
        if now - stamped > self.sensitivity.stale_secs {
            return None;
        }
        Some(at.distance_to(anchor.at))
    }

    fn update_anchor(&mut self, reading: &Reading, now: i64) {
        let Some(anchor) = self.watching.anchor else {
            self.anchor = State::default();
            return;
        };

        let Some(distance) = self.anchor_excursion(reading, now) else {
            // No usable fix. The alarm neither fires nor clears: the boat
            // may be dragging right now and nobody can tell, so an alarm
            // already sounding must keep sounding. Silently clearing on
            // a lost fix is how an anchor watch fails quietly.
            self.anchor.breaches = 0;
            return;
        };

        if self.anchor.firing {
            // Clearing takes coming back well inside, not merely to the
            // line — otherwise a boat swinging to the tide on the
            // boundary alarms every few minutes all night.
            if distance < anchor.radius_m * self.sensitivity.clear_fraction {
                self.anchor = State::default();
            }
            return;
        }

        if distance > anchor.radius_m {
            self.anchor.breaches += 1;
            if self.anchor.breaches >= self.sensitivity.confirmations {
                self.anchor.firing = true;
                self.anchor.acknowledged = false;
            }
        } else {
            self.anchor.breaches = 0;
        }
    }

    fn update_shallow(&mut self, reading: &Reading, now: i64) {
        let Some(limit) = self.watching.shallow_m else {
            self.shallow = State::default();
            return;
        };

        // No reading is not a reading of zero. A sounder that loses
        // bottom in a swell reports nothing or reports zero, and treating
        // that as no water fires the alarm in mid-ocean.
        let depth = reading.depth_m.and_then(|(metres, stamped)| {
            let fresh = now - stamped <= self.sensitivity.stale_secs;
            (fresh && metres > 0.0).then_some(metres)
        });

        let Some(depth) = depth else {
            // As with the anchor: an alarm already sounding keeps
            // sounding. The sounder failing is not evidence of deep water.
            self.shallow.breaches = 0;
            return;
        };

        if self.shallow.firing {
            if depth > limit + self.sensitivity.clear_margin_m {
                self.shallow = State::default();
            }
            return;
        }

        if depth < limit {
            self.shallow.breaches += 1;
            if self.shallow.breaches >= self.sensitivity.confirmations {
                self.shallow.firing = true;
                self.shallow.acknowledged = false;
            }
        } else {
            self.shallow.breaches = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANCHORAGE: LatLon = LatLon::new(32.7100, -117.2300);
    const START: i64 = 1_772_323_200;

    /// A position `metres` north of the anchorage.
    fn north_of(metres: f64) -> LatLon {
        // A degree of latitude is 111,320 m, near enough for a test.
        LatLon::new(ANCHORAGE.lat + metres / 111_320.0, ANCHORAGE.lon)
    }

    fn anchored() -> Watch {
        let mut watch = Watch::default();
        watch.set_anchor(ANCHORAGE, 40.0);
        watch
    }

    /// Feed `count` fixes at a position, one a second.
    fn feed(watch: &mut Watch, at: LatLon, count: u32) {
        for second in 0..count {
            let now = START + i64::from(second);
            watch.update(
                &Reading {
                    position: Some((at, now)),
                    depth_m: None,
                },
                now,
            );
        }
    }

    #[test]
    fn swinging_inside_the_circle_is_not_dragging() {
        let mut watch = anchored();
        for second in 0..600 {
            // Round and round on a 35 m radius, which is inside 40.
            let angle = f64::from(second) / 30.0;
            let at = LatLon::new(
                ANCHORAGE.lat + 35.0 * angle.cos() / 111_320.0,
                ANCHORAGE.lon + 35.0 * angle.sin() / (111_320.0 * 0.84),
            );
            let now = START + i64::from(second);
            watch.update(
                &Reading {
                    position: Some((at, now)),
                    depth_m: None,
                },
                now,
            );
        }
        assert!(watch.active().is_empty(), "{:?}", watch.active());
    }

    #[test]
    fn one_bad_fix_does_not_wake_the_boat() {
        // The failure that gets an alarm switched off. A GPS throws an
        // outlier now and then — a reflection off a cliff — and an alarm
        // that fires on one is an alarm nobody leaves armed.
        let mut watch = anchored();
        feed(&mut watch, north_of(10.0), 10);

        // One fix two hundred metres away, then back.
        let now = START + 11;
        watch.update(
            &Reading {
                position: Some((north_of(200.0), now)),
                depth_m: None,
            },
            now,
        );
        assert!(
            watch.active().is_empty(),
            "a single outlier fired the alarm"
        );

        feed(&mut watch, north_of(10.0), 5);
        assert!(watch.active().is_empty());
    }

    #[test]
    fn actually_dragging_fires() {
        let mut watch = anchored();
        feed(&mut watch, north_of(10.0), 10);
        assert!(watch.active().is_empty());

        // Out past the radius and staying there.
        feed(&mut watch, north_of(80.0), 5);

        let active = watch.active();
        assert_eq!(active.len(), 1, "{active:?}");
        assert_eq!(active[0].kind, Kind::AnchorDrag);
        assert!(!active[0].acknowledged, "a new alarm starts unsilenced");
        assert!(watch.sounding());
        assert!(
            (active[0].value_m - 80.0).abs() < 5.0,
            "it should say how far out: {}",
            active[0].value_m
        );
    }

    #[test]
    fn it_takes_the_stated_number_of_confirmations() {
        let mut watch = anchored();
        watch.sensitivity.confirmations = 3;

        for count in 1..3 {
            let mut watch = watch.clone();
            feed(&mut watch, north_of(80.0), count);
            assert!(
                watch.active().is_empty(),
                "{count} breaching fixes should not be enough"
            );
        }
        feed(&mut watch, north_of(80.0), 3);
        assert!(!watch.active().is_empty(), "three should be");
    }

    #[test]
    fn a_boat_on_the_boundary_alarms_once_not_forty_times() {
        // Hysteresis. Swinging to a tide with the radius set a little
        // tight means crossing it every few minutes all night.
        let mut watch = anchored();
        feed(&mut watch, north_of(45.0), 5);
        assert!(!watch.active().is_empty(), "should be firing");
        watch.acknowledge();
        assert!(!watch.sounding());

        // Back to just inside the radius — but not far inside.
        feed(&mut watch, north_of(39.0), 5);
        assert!(
            !watch.active().is_empty(),
            "a boat still on the line should not have cleared"
        );
        assert!(!watch.sounding(), "and it should still be silenced");

        // Well back inside, and it clears.
        feed(&mut watch, north_of(20.0), 3);
        assert!(watch.active().is_empty(), "it should have cleared by 20 m");
    }

    #[test]
    fn an_alarm_that_clears_and_returns_sounds_again() {
        // Acknowledging silences *this* alarm, not every future one.
        let mut watch = anchored();
        feed(&mut watch, north_of(80.0), 5);
        watch.acknowledge();
        assert!(!watch.sounding());

        feed(&mut watch, north_of(5.0), 5);
        assert!(watch.active().is_empty(), "it should have cleared");

        feed(&mut watch, north_of(80.0), 5);
        assert!(
            watch.sounding(),
            "dragging again after clearing has to make a noise again"
        );
    }

    #[test]
    fn losing_the_fix_does_not_silently_clear_an_alarm() {
        // The quiet failure: the boat is dragging, the GPS drops out, and
        // an alarm that cleared on no data would stop just when nobody
        // can tell what is happening.
        let mut watch = anchored();
        feed(&mut watch, north_of(80.0), 5);
        assert!(watch.sounding());

        for second in 0..20 {
            let now = START + 100 + i64::from(second);
            watch.update(&Reading::default(), now);
        }
        assert!(
            watch.sounding(),
            "the alarm went quiet when the fix was lost"
        );
    }

    #[test]
    fn a_stale_fix_counts_as_no_fix() {
        let mut watch = anchored();
        watch.sensitivity.stale_secs = 10;

        // A fix from a minute ago, with the boat apparently well outside.
        // It must not fire: that position says nothing about now.
        for second in 0..10 {
            let now = START + 100 + i64::from(second);
            watch.update(
                &Reading {
                    position: Some((north_of(500.0), START)),
                    depth_m: None,
                },
                now,
            );
        }
        assert!(
            watch.active().is_empty(),
            "a minute-old position fired the alarm"
        );
    }

    #[test]
    fn weighing_the_anchor_stops_the_watch() {
        let mut watch = anchored();
        feed(&mut watch, north_of(80.0), 5);
        assert!(watch.sounding());

        watch.weigh_anchor();
        assert!(watch.active().is_empty());
        assert!(!watch.sounding());

        // And sailing away does not start it again.
        feed(&mut watch, north_of(5_000.0), 10);
        assert!(watch.active().is_empty());
    }

    #[test]
    fn a_radius_of_nothing_is_refused() {
        // Zero would fire on the GPS's own scatter the instant it was set.
        let mut watch = Watch::default();
        watch.set_anchor(ANCHORAGE, 0.0);
        assert!(
            watch.watching.anchor.expect("an anchor").radius_m >= 5.0,
            "a zero radius was accepted"
        );
    }

    // ── Depth ───────────────────────────────────────────────────────────

    fn sounding_at(watch: &mut Watch, metres: f64, count: u32) {
        for second in 0..count {
            let now = START + i64::from(second);
            watch.update(
                &Reading {
                    position: None,
                    depth_m: Some((metres, now)),
                },
                now,
            );
        }
    }

    #[test]
    fn shoaling_past_the_limit_fires() {
        let mut watch = Watch::default();
        watch.set_shallow(Some(3.0));

        sounding_at(&mut watch, 8.0, 5);
        assert!(watch.active().is_empty());

        sounding_at(&mut watch, 2.4, 5);
        let active = watch.active();
        assert_eq!(active.len(), 1, "{active:?}");
        assert_eq!(active[0].kind, Kind::Shallow);
        assert!(
            (active[0].value_m - 2.4).abs() < 1e-6,
            "it should say the depth"
        );
    }

    #[test]
    fn a_sounder_that_lost_bottom_is_not_no_water() {
        // The one that would fire in the middle of an ocean. A sounder
        // out of its range reports nothing, or reports zero, and zero is
        // not shallow — it is unknown.
        let mut watch = Watch::default();
        watch.set_shallow(Some(3.0));

        sounding_at(&mut watch, 0.0, 20);
        assert!(
            watch.active().is_empty(),
            "a zero reading fired the shallow alarm"
        );

        // And no reading at all likewise.
        for second in 0..20 {
            let now = START + i64::from(second);
            watch.update(&Reading::default(), now);
        }
        assert!(watch.active().is_empty());
    }

    #[test]
    fn a_sounder_that_stops_does_not_silence_a_firing_alarm() {
        let mut watch = Watch::default();
        watch.set_shallow(Some(3.0));
        sounding_at(&mut watch, 2.0, 5);
        assert!(watch.sounding());

        // It stops reporting. The water did not get deeper.
        for second in 0..30 {
            let now = START + 100 + i64::from(second);
            watch.update(&Reading::default(), now);
        }
        assert!(
            watch.sounding(),
            "the alarm went quiet when the sounder did"
        );
    }

    #[test]
    fn the_depth_alarm_has_hysteresis_too() {
        let mut watch = Watch::default();
        watch.set_shallow(Some(3.0));
        watch.sensitivity.clear_margin_m = 0.5;

        sounding_at(&mut watch, 2.0, 5);
        assert!(!watch.active().is_empty());

        // Back to just over the limit: still firing, because a boat in a
        // swell crosses it with every wave.
        sounding_at(&mut watch, 3.2, 5);
        assert!(
            !watch.active().is_empty(),
            "it cleared on the first reading back over the line"
        );

        sounding_at(&mut watch, 4.0, 5);
        assert!(
            watch.active().is_empty(),
            "half a metre clear should clear it"
        );
    }

    #[test]
    fn both_alarms_can_be_true_at_once() {
        let mut watch = anchored();
        watch.set_shallow(Some(3.0));

        for second in 0..6 {
            let now = START + i64::from(second);
            watch.update(
                &Reading {
                    position: Some((north_of(90.0), now)),
                    depth_m: Some((1.5, now)),
                },
                now,
            );
        }
        let active = watch.active();
        assert_eq!(active.len(), 2, "{active:?}");
        assert!(active.iter().any(|a| a.kind == Kind::AnchorDrag));
        assert!(active.iter().any(|a| a.kind == Kind::Shallow));
    }

    #[test]
    fn nothing_armed_means_nothing_fires() {
        let mut watch = Watch::default();
        for second in 0..50 {
            let now = START + i64::from(second);
            watch.update(
                &Reading {
                    position: Some((north_of(10_000.0), now)),
                    depth_m: Some((0.1, now)),
                },
                now,
            );
        }
        assert!(watch.active().is_empty());
        assert!(!watch.sounding());
    }

    #[test]
    fn a_change_is_reported_so_nothing_has_to_poll() {
        let mut watch = anchored();
        let now = START;
        // Nothing happening.
        assert!(!watch.update(
            &Reading {
                position: Some((north_of(5.0), now)),
                depth_m: None
            },
            now
        ));

        // The fix that tips it over reports a change.
        let mut changed = false;
        for second in 0..5 {
            let now = START + i64::from(second);
            changed |= watch.update(
                &Reading {
                    position: Some((north_of(90.0), now)),
                    depth_m: None,
                },
                now,
            );
        }
        assert!(changed, "the alarm firing was not reported as a change");
    }
}
