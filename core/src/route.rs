//! Waypoints, legs, and how far off the line you are.
//!
//! A route is an ordered list of places and the straight legs between
//! them. That is deliberately all it is: there is no network to search on
//! the sea, so for a coastal passage the path is obvious and what you
//! actually want from the instrument is the ability to *execute* one.
//!
//! Which is three numbers, and none of them is the path:
//!
//! * **Bearing and distance** per leg, so there is a compass course to
//!   steer and a run time to expect — sailable with a compass and a watch
//!   if the electronics stop, which is what a rutter was for.
//! * **Cross-track error**: how far off the intended line you have been
//!   pushed. The important one. Point a boat at an island and you do not
//!   go there — wind sets you one way and current another — and without a
//!   line to measure against you sail a long curve and never notice,
//!   because the bearing to the destination quietly updates as you drift.
//!   The line was not arbitrary: it was drawn to clear something.
//! * **Time to go**, which is really a question about tide. "Leave at 0630
//!   and carry the ebb out" is a plan; "leave in the morning" is not.
//!
//! ## Why legs are straight, and stay straight
//!
//! An autorouter, if one is ever added, does not produce curves — it
//! produces *more waypoints*. Its output is a polyline, so the model here
//! is the one it would fill in. And legs want to stay straight regardless:
//! a curve has no single course for the helm or the autopilot to steer,
//! and cross-track error is distance from a line.
//!
//! The one real curve in navigation is the great circle, and even that is
//! sailed as a succession of straight rhumb-line legs with the course
//! recomputed along the way. See [`Leg::bearing_deg`] for which of the two
//! this uses and why.
//!
//! ## Why waypoints and not legs are stored
//!
//! Legs are derived. Store legs instead and inserting a waypoint into the
//! middle of a passage becomes surgery on both neighbours.

use crate::geo::{metres_to_nm, normalise_deg, EARTH_RADIUS_M};
use crate::LatLon;

/// One place on a route.
#[derive(Debug, Clone, PartialEq)]
pub struct Waypoint {
    pub at: LatLon,
    /// What it is called. Plotters traditionally number them; a name is
    /// more use when reading a plan back — "Zuniga" rather than "003".
    pub name: String,
    /// How close counts as arrived, metres.
    ///
    /// Per waypoint because the right answer differs: a tenth of a mile is
    /// fine in open water and far too much in a dredged channel where the
    /// turn has to happen at the mark.
    pub arrival_m: f64,
}

impl Waypoint {
    /// A tenth of a nautical mile, which is what most plotters default to.
    pub const DEFAULT_ARRIVAL_M: f64 = 185.2;

    pub fn new(at: LatLon, name: impl Into<String>) -> Self {
        Self {
            at,
            name: name.into(),
            arrival_m: Self::DEFAULT_ARRIVAL_M,
        }
    }
}

/// An ordered passage.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Route {
    pub name: String,
    pub waypoints: Vec<Waypoint>,
}

/// One straight piece of a route.
///
/// Derived rather than stored. Cheap to make — two trig calls — and
/// deriving it means a route can never hold a leg that disagrees with the
/// waypoints either end of it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Leg {
    pub from: LatLon,
    pub to: LatLon,
    /// Index of the waypoint this leg ends at, which is the one being
    /// steered for.
    pub to_index: usize,
}

impl Leg {
    /// The course to steer, degrees true.
    ///
    /// The **rhumb** bearing, not the great-circle one: a rhumb line is
    /// the track of a constant heading, which is the only kind a helm or
    /// an autopilot can hold. The great-circle bearing changes
    /// continuously along the leg, so steering it would mean steering a
    /// number that is wrong everywhere except the instant it was computed.
    ///
    /// Over coastal distances the two agree closely —
    /// `the_two_bearings_agree_over_a_coastal_leg` measures it — and the
    /// difference only becomes navigationally real on an ocean crossing.
    pub fn bearing_deg(&self) -> f64 {
        self.from.rhumb_bearing_to(self.to)
    }

    /// How long the leg is, metres, along the track that is steered.
    pub fn length_m(&self) -> f64 {
        self.from.rhumb_distance_to(self.to)
    }

    pub fn length_nm(&self) -> f64 {
        metres_to_nm(self.length_m())
    }

    /// How far off the leg a position is, metres. Positive is to the right
    /// of the track, looking from `from` towards `to`.
    ///
    /// Signed on purpose: "0.4 miles off" tells you to do something and
    /// not which way, and the wrong way doubles the error. Right-positive
    /// matches every plotter and the usual "steer left to correct" reading.
    ///
    /// Computed against the great-circle track rather than the rhumb line.
    /// The exact rhumb-line equivalent needs Mercator projection
    /// arithmetic, and over a coastal leg the two tracks differ by far
    /// less than a GPS fix does — which the tests measure rather than
    /// assert.
    pub fn cross_track_m(&self, position: LatLon) -> f64 {
        let distance = self.from.distance_to(position);
        if distance == 0.0 {
            return 0.0;
        }
        let to_position = self.from.bearing_to(position).to_radians();
        let to_end = self.from.bearing_to(self.to).to_radians();

        // The spherical cross-track formula. `asin` of a product of sines,
        // so it is well behaved for every angle rather than only small
        // ones — a small-angle approximation would drift at the far end of
        // a long leg, which is exactly where being off track matters.
        ((distance / EARTH_RADIUS_M).sin() * (to_position - to_end).sin()).asin() * EARTH_RADIUS_M
    }

    /// How far along the leg a position has got, metres.
    ///
    /// Can be negative — before the start — or longer than the leg, past
    /// the end. Both are real states and neither is clamped: a negative
    /// value means you have not reached the start of the leg yet, which is
    /// the normal state at the beginning of a passage, and an overshoot is
    /// how the leg knows you have gone past the mark.
    pub fn along_track_m(&self, position: LatLon) -> f64 {
        let distance = self.from.distance_to(position);
        if distance == 0.0 {
            return 0.0;
        }
        let cross = self.cross_track_m(position);

        // acos of a ratio that rounding can push just outside [-1, 1],
        // which would come back NaN and silently poison every number
        // downstream. Clamped.
        let ratio = (distance / EARTH_RADIUS_M).cos() / (cross / EARTH_RADIUS_M).cos();
        let along = ratio.clamp(-1.0, 1.0).acos() * EARTH_RADIUS_M;

        // `acos` has no sign, so the direction comes from whether the
        // position is ahead of the start or behind it.
        let to_position = self.from.bearing_to(position);
        let to_end = self.from.bearing_to(self.to);
        let off_axis = (to_position - to_end).abs() % 360.0;
        let off_axis = off_axis.min(360.0 - off_axis);
        if off_axis > 90.0 {
            -along
        } else {
            along
        }
    }
}

impl Route {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            waypoints: Vec::new(),
        }
    }

    /// The legs between consecutive waypoints.
    ///
    /// Empty for a route of fewer than two: one waypoint is a destination
    /// rather than a passage, and it is still worth having — "go to here"
    /// is the commonest thing anyone asks of a plotter.
    pub fn legs(&self) -> Vec<Leg> {
        self.waypoints
            .windows(2)
            .enumerate()
            .map(|(index, pair)| Leg {
                from: pair[0].at,
                to: pair[1].at,
                to_index: index + 1,
            })
            .collect()
    }

    /// Total length along the track, metres.
    pub fn length_m(&self) -> f64 {
        self.legs().iter().map(Leg::length_m).sum()
    }

    pub fn length_nm(&self) -> f64 {
        metres_to_nm(self.length_m())
    }

    /// The leg nearest a position, by how far off it you are.
    ///
    /// How a plotter decides which leg you are on when it has not been
    /// told. Only legs you are actually *beside* are considered — a leg
    /// whose perpendicular you are before or past is rejected however
    /// close its line happens to pass — because otherwise a long leg
    /// running parallel a mile away wins over the short one you are on.
    pub fn nearest_leg(&self, position: LatLon) -> Option<Leg> {
        let legs = self.legs();
        legs.iter()
            .filter(|leg| {
                let along = leg.along_track_m(position);
                along >= 0.0 && along <= leg.length_m()
            })
            .min_by(|a, b| {
                let (x, y) = (
                    a.cross_track_m(position).abs(),
                    b.cross_track_m(position).abs(),
                );
                x.partial_cmp(&y).unwrap_or(core::cmp::Ordering::Equal)
            })
            .copied()
            // Beside no leg at all — before the first waypoint, or past
            // the last — so fall back to whichever waypoint is closest,
            // and the leg that ends at it.
            .or_else(|| {
                legs.iter()
                    .min_by(|a, b| {
                        let (x, y) = (position.distance_to(a.to), position.distance_to(b.to));
                        x.partial_cmp(&y).unwrap_or(core::cmp::Ordering::Equal)
                    })
                    .copied()
            })
    }
}

/// Where the boat is in relation to a route being followed.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    /// The leg being steered.
    pub leg: Leg,
    /// Metres off the intended line; positive is right of track.
    pub cross_track_m: f64,
    /// Distance to the waypoint at the end of this leg, metres.
    pub to_waypoint_m: f64,
    /// Course to steer for that waypoint now, degrees true.
    ///
    /// The bearing to the *waypoint*, not the leg's own course: once you
    /// are off track those differ, and this is the one that closes the
    /// gap. The leg's course is on [`Leg::bearing_deg`] for comparison.
    pub bearing_deg: f64,
    /// Distance from here to the end of the route along the track, metres.
    pub remaining_m: f64,
    /// Speed actually being made towards the waypoint, knots.
    ///
    /// Can be negative, and that is the point: six knots through the water
    /// on a course thirty degrees off is five knots of progress, and six
    /// knots the wrong side of ninety degrees is going backwards. A speed
    /// readout alone cannot tell you that.
    pub vmg_kn: Option<f64>,
    /// Seconds until the waypoint at this speed, and until the end.
    ///
    /// `None` when not making way towards it — an arrival time computed
    /// from a standstill is infinity, and from a negative VMG is a lie.
    pub to_waypoint_secs: Option<i64>,
    pub to_end_secs: Option<i64>,
    /// Whether the boat is inside the waypoint's arrival radius, or has
    /// crossed the perpendicular at it.
    pub arrived: bool,
}

impl Route {
    /// Work out where the boat is on this route.
    ///
    /// `leg` is the one being followed; `None` picks the nearest. Being
    /// told is better than guessing — a plotter that reselects the leg
    /// every second will hop to a parallel one when a passage doubles
    /// back — so the caller normally advances the leg itself and passes it
    /// in.
    pub fn progress(
        &self,
        position: LatLon,
        leg: Option<Leg>,
        sog_kn: Option<f64>,
        cog_deg: Option<f64>,
    ) -> Option<Progress> {
        let leg = leg.or_else(|| self.nearest_leg(position))?;
        let legs = self.legs();

        let to_waypoint_m = position.rhumb_distance_to(leg.to);
        let bearing_deg = normalise_deg(position.rhumb_bearing_to(leg.to));

        // Everything after this leg, in full, plus what is left of it.
        let remaining_m = to_waypoint_m
            + legs
                .iter()
                .filter(|l| l.to_index > leg.to_index)
                .map(Leg::length_m)
                .sum::<f64>();

        let vmg_kn = match (sog_kn, cog_deg) {
            (Some(sog), Some(cog)) => {
                let off = (cog - bearing_deg).to_radians();
                Some(sog * off.cos())
            }
            // Speed with no course is no use here: it could be towards the
            // mark or away from it.
            _ => None,
        };

        let seconds_for = |metres: f64| -> Option<i64> {
            let vmg = vmg_kn?;
            // A tenth of a knot is the floor: below that the arithmetic
            // produces times in weeks, which is not an estimate.
            if vmg < 0.1 {
                return None;
            }
            Some((metres_to_nm(metres) / vmg * 3600.0) as i64)
        };

        let along = leg.along_track_m(position);
        Some(Progress {
            leg,
            cross_track_m: leg.cross_track_m(position),
            to_waypoint_m,
            bearing_deg,
            remaining_m,
            vmg_kn,
            to_waypoint_secs: seconds_for(to_waypoint_m),
            to_end_secs: seconds_for(remaining_m),
            // Either inside the circle, or past the line through the
            // waypoint at right angles to the leg. Both, because each
            // misses a case the other catches: a boat that cuts the
            // corner wide never enters the circle, and a boat creeping up
            // slowly in a cross-current may sit inside the circle for
            // minutes before crossing the perpendicular.
            arrived: to_waypoint_m
                <= self
                    .waypoints
                    .get(leg.to_index)
                    .map_or(Waypoint::DEFAULT_ARRIVAL_M, |w| w.arrival_m)
                || along >= leg.length_m(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shelter Island, out of San Diego Bay, and on to Catalina. The
    /// passage this was written for.
    const SHELTER_ISLAND: LatLon = LatLon::new(32.7100, -117.2300);
    const BAY_ENTRANCE: LatLon = LatLon::new(32.6700, -117.2350);
    const OFF_POINT_LOMA: LatLon = LatLon::new(32.6600, -117.2700);
    const AVALON: LatLon = LatLon::new(33.3450, -118.3250);

    fn passage() -> Route {
        Route {
            name: "Catalina".to_owned(),
            waypoints: vec![
                Waypoint::new(SHELTER_ISLAND, "Shelter Island"),
                Waypoint::new(BAY_ENTRANCE, "Bay entrance"),
                Waypoint::new(OFF_POINT_LOMA, "Off Point Loma"),
                Waypoint::new(AVALON, "Avalon"),
            ],
        }
    }

    fn approx(a: f64, b: f64, tol: f64, what: &str) {
        assert!((a - b).abs() < tol, "{what}: expected {b}, got {a}");
    }

    #[test]
    fn a_passage_has_a_leg_between_each_pair() {
        let route = passage();
        let legs = route.legs();
        assert_eq!(legs.len(), 3, "four waypoints make three legs");
        assert_eq!(legs[0].from, SHELTER_ISLAND);
        assert_eq!(legs[2].to, AVALON);
        // The index names the waypoint being steered for, which is what
        // the panel highlights and what arrival is measured against.
        assert_eq!(legs[0].to_index, 1);
        assert_eq!(legs[2].to_index, 3);
    }

    #[test]
    fn a_route_too_short_to_have_legs_does_not_pretend_to() {
        assert!(Route::default().legs().is_empty());
        assert_eq!(Route::default().length_m(), 0.0);

        let one = Route {
            name: "go here".to_owned(),
            waypoints: vec![Waypoint::new(AVALON, "Avalon")],
        };
        assert!(one.legs().is_empty(), "one waypoint is not a passage");
        assert_eq!(one.length_m(), 0.0);
        // And asking for progress on it must not panic.
        assert!(one.progress(SHELTER_ISLAND, None, None, None).is_none());
    }

    #[test]
    fn the_passage_is_about_the_right_length() {
        // San Diego to Avalon is a little over sixty nautical miles, which
        // is the one number a person can check this against by eye.
        let nm = passage().length_nm();
        assert!(
            (55.0..75.0).contains(&nm),
            "the passage came out {nm:.1} nm"
        );
    }

    #[test]
    fn the_two_bearings_agree_over_a_coastal_leg() {
        // The rhumb line is steered and the great circle is what the
        // cross-track formula measures against. Mixing them is only
        // acceptable if they agree to well inside a GPS fix, so this
        // measures the disagreement rather than asserting it away.
        let leg = Leg {
            from: OFF_POINT_LOMA,
            to: AVALON,
            to_index: 1,
        };
        let rhumb = leg.bearing_deg();
        let great = leg.from.bearing_to(leg.to);
        let gap = (rhumb - great).abs();
        assert!(
            gap < 0.5,
            "rhumb {rhumb:.3} and great circle {great:.3} differ by {gap:.3} degrees"
        );

        // And the distances likewise: a sixty-mile leg should agree to
        // within a cable.
        let difference = (leg.length_m() - leg.from.distance_to(leg.to)).abs();
        assert!(
            difference < 200.0,
            "the two tracks differ in length by {difference:.0} m"
        );
    }

    #[test]
    fn being_on_the_line_is_zero_cross_track() {
        let leg = Leg {
            from: OFF_POINT_LOMA,
            to: AVALON,
            to_index: 1,
        };
        approx(leg.cross_track_m(OFF_POINT_LOMA), 0.0, 1.0, "at the start");
        approx(leg.cross_track_m(AVALON), 0.0, 1.0, "at the end");

        // And halfway along, which is the case that catches a formula
        // that only happens to work at the ends.
        let middle = LatLon::new(
            (OFF_POINT_LOMA.lat + AVALON.lat) / 2.0,
            (OFF_POINT_LOMA.lon + AVALON.lon) / 2.0,
        );
        assert!(
            leg.cross_track_m(middle).abs() < 2_000.0,
            "the midpoint of a straight leg should be near the line, got {:.0} m",
            leg.cross_track_m(middle)
        );
    }

    #[test]
    fn cross_track_is_signed_right_positive() {
        // The sign is the whole value of the number: knowing you are
        // 0.4 nm off without knowing which side means a fifty per cent
        // chance of doubling it.
        //
        // A leg due north, so left and right are plainly west and east.
        let leg = Leg {
            from: LatLon::new(32.0, -117.0),
            to: LatLon::new(33.0, -117.0),
            to_index: 1,
        };

        let east = leg.cross_track_m(LatLon::new(32.5, -116.9));
        let west = leg.cross_track_m(LatLon::new(32.5, -117.1));
        assert!(east > 0.0, "east of a northbound track is right: {east:.0}");
        assert!(west < 0.0, "west of a northbound track is left: {west:.0}");

        // Symmetric, and about the right size: a tenth of a degree of
        // longitude at 32.5 degrees north is roughly 9.4 km.
        approx(east, -west, 50.0, "symmetry about the track");
        approx(
            east.abs(),
            9_380.0,
            400.0,
            "a tenth of a degree of longitude",
        );
    }

    #[test]
    fn along_track_runs_from_zero_to_the_leg_length() {
        let leg = Leg {
            from: LatLon::new(32.0, -117.0),
            to: LatLon::new(33.0, -117.0),
            to_index: 1,
        };
        approx(leg.along_track_m(leg.from), 0.0, 1.0, "at the start");
        approx(
            leg.along_track_m(leg.to),
            leg.length_m(),
            300.0,
            "at the end",
        );
        approx(
            leg.along_track_m(LatLon::new(32.5, -117.0)),
            leg.length_m() / 2.0,
            300.0,
            "halfway",
        );
    }

    #[test]
    fn along_track_is_negative_before_the_start_and_over_at_the_end() {
        // Neither is clamped, and both are needed: before the start is the
        // normal state at the beginning of a passage, and the overshoot is
        // how a leg knows the mark has been passed even by a boat that cut
        // the corner and never came near it.
        let leg = Leg {
            from: LatLon::new(32.0, -117.0),
            to: LatLon::new(33.0, -117.0),
            to_index: 1,
        };
        assert!(
            leg.along_track_m(LatLon::new(31.9, -117.0)) < 0.0,
            "south of a northbound leg's start should be negative"
        );
        assert!(
            leg.along_track_m(LatLon::new(33.1, -117.0)) > leg.length_m(),
            "north of its end should be past the length"
        );
    }

    #[test]
    fn the_leg_being_followed_is_the_one_you_are_beside() {
        // The failure this guards: picking the leg by distance to its line
        // alone. A passage that doubles back has two legs whose lines pass
        // close to each other, and a plotter that reselects every second
        // flips between them — the bearing to steer then alternates
        // between two courses a hundred degrees apart.
        let route = passage();

        // Just outside the bay entrance, on the second leg.
        let on_second = LatLon::new(32.6650, -117.2500);
        let leg = route.nearest_leg(on_second).expect("a leg");
        assert_eq!(leg.to_index, 2, "should be steering for Point Loma");

        // Well up the first leg instead.
        let on_first = LatLon::new(32.6950, -117.2320);
        assert_eq!(
            route.nearest_leg(on_first).map(|l| l.to_index),
            Some(1),
            "should be steering for the bay entrance"
        );
    }

    #[test]
    fn somewhere_beside_no_leg_at_all_still_gets_an_answer() {
        // Miles off the route entirely — which happens, and must produce a
        // leg to steer rather than nothing at all.
        let route = passage();
        let adrift = LatLon::new(32.0000, -118.0000);
        assert!(
            route.nearest_leg(adrift).is_some(),
            "a position off the route still needs something to steer for"
        );
    }

    #[test]
    fn progress_counts_the_whole_route_not_just_this_leg() {
        let route = passage();
        // At the start, with no leg nominated.
        let progress = route
            .progress(SHELTER_ISLAND, None, Some(6.0), None)
            .expect("progress");

        approx(
            metres_to_nm(progress.remaining_m),
            route.length_nm(),
            0.5,
            "the whole route remains at the start",
        );
        assert_eq!(progress.leg.to_index, 1);

        // And on the last leg only that leg remains.
        let last = route.legs()[2];
        let near_avalon = LatLon::new(33.3000, -118.3000);
        let progress = route
            .progress(near_avalon, Some(last), Some(6.0), Some(315.0))
            .expect("progress");
        assert!(
            metres_to_nm(progress.remaining_m) < 10.0,
            "{:.1} nm remaining within sight of Avalon",
            metres_to_nm(progress.remaining_m)
        );
    }

    #[test]
    fn made_good_is_the_speed_towards_the_mark_not_through_the_water() {
        // A speed readout alone cannot tell you that six knots on the
        // wrong heading is no progress. This is the number that can.
        let route = passage();
        let leg = route.legs()[0];
        let position = LatLon::new(32.7000, -117.2310);
        let straight_at_it = route
            .progress(position, Some(leg), Some(6.0), None)
            .expect("progress");
        // With no course there is nothing to resolve the speed onto.
        assert_eq!(straight_at_it.vmg_kn, None);

        let bearing = straight_at_it.bearing_deg;

        // Dead on the mark: all of it counts.
        let on_course = route
            .progress(position, Some(leg), Some(6.0), Some(bearing))
            .expect("progress");
        approx(on_course.vmg_kn.expect("vmg"), 6.0, 0.01, "on course");

        // Sixty degrees off: half of it.
        let off = route
            .progress(position, Some(leg), Some(6.0), Some(bearing + 60.0))
            .expect("progress");
        approx(off.vmg_kn.expect("vmg"), 3.0, 0.05, "sixty degrees off");

        // And the wrong way entirely is negative, which a plotter has to
        // be willing to say.
        let backwards = route
            .progress(position, Some(leg), Some(6.0), Some(bearing + 180.0))
            .expect("progress");
        assert!(
            backwards.vmg_kn.expect("vmg") < -5.9,
            "going away from the mark should read negative, got {:?}",
            backwards.vmg_kn
        );
    }

    #[test]
    fn no_arrival_time_is_offered_when_none_can_be_had() {
        // An ETA computed from a standstill is infinity and from a
        // negative speed is a lie. Both have to be declined rather than
        // printed.
        let route = passage();
        let leg = route.legs()[0];
        let position = LatLon::new(32.7000, -117.2310);
        let bearing = route
            .progress(position, Some(leg), None, None)
            .expect("progress")
            .bearing_deg;

        let stopped = route
            .progress(position, Some(leg), Some(0.0), Some(bearing))
            .expect("progress");
        assert_eq!(stopped.to_waypoint_secs, None);
        assert_eq!(stopped.to_end_secs, None);

        let wrong_way = route
            .progress(position, Some(leg), Some(6.0), Some(bearing + 180.0))
            .expect("progress");
        assert_eq!(wrong_way.to_waypoint_secs, None);

        // Making way, there is an answer, and it is the right order of
        // magnitude: the whole passage at six knots is about eleven hours.
        let going = route
            .progress(SHELTER_ISLAND, None, Some(6.0), Some(bearing))
            .expect("progress");
        let hours = going.to_end_secs.expect("an eta") as f64 / 3600.0;
        assert!(
            (8.0..16.0).contains(&hours),
            "the passage came out at {hours:.1} hours at six knots"
        );
    }

    #[test]
    fn arrival_is_either_the_circle_or_the_perpendicular() {
        // Both tests, because each misses a case the other catches. A boat
        // cutting the corner wide never enters the circle; a boat creeping
        // up in a cross-current sits inside it for minutes before crossing
        // the line through the mark.
        let route = Route {
            name: "two".to_owned(),
            waypoints: vec![
                Waypoint::new(LatLon::new(32.0, -117.0), "start"),
                Waypoint::new(LatLon::new(33.0, -117.0), "finish"),
            ],
        };
        let leg = route.legs()[0];

        let short_of_it = route
            .progress(LatLon::new(32.9, -117.0), Some(leg), None, None)
            .expect("progress");
        assert!(!short_of_it.arrived, "six miles off is not arrived");

        // Inside the circle.
        let close = route
            .progress(LatLon::new(32.9990, -117.0), Some(leg), None, None)
            .expect("progress");
        assert!(close.arrived, "inside the arrival radius");

        // Past the perpendicular but a long way to one side: never came
        // near the circle, but the mark is behind.
        let wide = route
            .progress(LatLon::new(33.05, -117.2), Some(leg), None, None)
            .expect("progress");
        assert!(
            wide.arrived,
            "past the mark should count as arrived even cutting wide"
        );
    }

    #[test]
    fn a_tighter_arrival_radius_is_respected() {
        // A tenth of a mile is fine offshore and far too much in a
        // dredged channel where the turn has to happen at the mark.
        let mut route = Route {
            name: "channel".to_owned(),
            waypoints: vec![
                Waypoint::new(LatLon::new(32.0, -117.0), "start"),
                Waypoint::new(LatLon::new(33.0, -117.0), "turn"),
            ],
        };
        route.waypoints[1].arrival_m = 20.0;
        let leg = route.legs()[0];

        // 0.001 degrees of latitude is about 111 m: inside the default
        // radius, outside a twenty-metre one.
        let position = LatLon::new(32.9990, -117.0);
        assert!(
            !route
                .progress(position, Some(leg), None, None)
                .expect("progress")
                .arrived,
            "a twenty-metre radius should not have been reached at 111 m"
        );

        route.waypoints[1].arrival_m = Waypoint::DEFAULT_ARRIVAL_M;
        assert!(
            route
                .progress(position, Some(leg), None, None)
                .expect("progress")
                .arrived,
            "the default radius should have been reached"
        );
    }

    #[test]
    fn the_bearing_to_steer_closes_the_gap_rather_than_repeating_the_leg() {
        // Once off track the leg's own course no longer gets you to the
        // mark. The bearing reported has to be to the waypoint.
        let leg = Leg {
            from: LatLon::new(32.0, -117.0),
            to: LatLon::new(33.0, -117.0),
            to_index: 1,
        };
        let route = Route {
            name: "north".to_owned(),
            waypoints: vec![
                Waypoint::new(leg.from, "start"),
                Waypoint::new(leg.to, "finish"),
            ],
        };

        // Half a degree of longitude east of the track, halfway along.
        let off_to_the_east = LatLon::new(32.5, -116.5);
        let progress = route
            .progress(off_to_the_east, Some(leg), None, None)
            .expect("progress");

        approx(leg.bearing_deg(), 0.0, 0.5, "the leg runs due north");
        assert!(
            progress.bearing_deg > 300.0 && progress.bearing_deg < 350.0,
            "from east of track the mark should bear north-west, got {:.0}",
            progress.bearing_deg
        );
        assert!(progress.cross_track_m > 0.0, "east of track is positive");
    }

    #[test]
    fn nothing_blows_up_at_a_zero_length_leg() {
        // Two waypoints in the same place, which a fat-fingered tap on a
        // phone produces easily.
        let route = Route {
            name: "stuck".to_owned(),
            waypoints: vec![
                Waypoint::new(AVALON, "here"),
                Waypoint::new(AVALON, "also here"),
            ],
        };
        let leg = route.legs()[0];
        assert_eq!(leg.length_m(), 0.0);
        assert_eq!(leg.cross_track_m(AVALON), 0.0);
        assert_eq!(leg.along_track_m(AVALON), 0.0);

        let progress = route
            .progress(AVALON, Some(leg), Some(6.0), Some(0.0))
            .expect("progress");
        assert!(progress.cross_track_m.is_finite());
        assert!(progress.remaining_m.is_finite());
        assert!(progress.arrived, "already there");
    }

    #[test]
    fn every_number_stays_finite_all_over_the_world() {
        // The arithmetic has `asin`, `acos` and a division by a cosine in
        // it, each of which has a way of producing NaN at an awkward
        // latitude — and a NaN here poisons the bearing, the ETA and the
        // cross-track at once, silently.
        let places = [
            LatLon::new(0.0, 0.0),
            LatLon::new(0.0, 180.0),
            LatLon::new(89.9, 0.0),
            LatLon::new(-89.9, 179.9),
            LatLon::new(51.5, -0.1),
            LatLon::new(-33.9, 151.2),
            LatLon::new(32.68, -117.23),
        ];

        for &from in &places {
            for &to in &places {
                let leg = Leg {
                    from,
                    to,
                    to_index: 1,
                };
                let route = Route {
                    name: "probe".to_owned(),
                    waypoints: vec![Waypoint::new(from, "a"), Waypoint::new(to, "b")],
                };
                for &position in &places {
                    assert!(
                        leg.cross_track_m(position).is_finite(),
                        "cross track {from:?} -> {to:?} at {position:?}"
                    );
                    assert!(
                        leg.along_track_m(position).is_finite(),
                        "along track {from:?} -> {to:?} at {position:?}"
                    );
                    let progress = route
                        .progress(position, Some(leg), Some(6.0), Some(45.0))
                        .expect("progress");
                    assert!(progress.bearing_deg.is_finite());
                    assert!(progress.remaining_m.is_finite());
                    assert!(progress.vmg_kn.is_none_or(f64::is_finite));
                }
            }
        }
    }
}
