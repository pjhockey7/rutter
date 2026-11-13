//! Where the boat has been.
//!
//! A route is where you meant to go; a track is where you went. It is
//! worth keeping for three separate reasons, and they pull the design in
//! the same direction:
//!
//! * **Getting back out.** The commonest real use. You felt your way into
//!   an anchorage on a rising tide in poor visibility, and the safest
//!   water out is the water you came in through — because you know it
//!   floated you.
//! * **A log.** Where you were and when, which is what a passage log is,
//!   and what an insurer or a harbourmaster may ask for.
//! * **Seeing the set.** A track beside its route shows what the tide did
//!   to you, which no instantaneous reading does.
//!
//! ## Why thinning is the whole problem
//!
//! A GPS gives a fix every second. Twelve hours is 43,200 of them, and
//! almost all are redundant: a straight run needs two points to describe
//! it and the rest are noise about the same line. Keeping them all costs
//! memory, makes the file unreadable by other software, and — worse —
//! buries the turns, which are the part that matters, in a haze of
//! jitter.
//!
//! So this thins as it records, with an *opening window*:
//!
//! 1. Hold every fix since the last committed point as pending.
//! 2. When a new one arrives, measure how far each pending fix lies from
//!    the straight line between the last committed point and the newest.
//! 3. While they all lie close to it, they say nothing the line does not.
//!    Keep holding them.
//! 4. As soon as one is too far off, it is a corner: commit the *worst*
//!    of them and start the window again from there.
//!
//! That is Douglas–Peucker run forwards, and the window is what makes it
//! work. A single point of lookahead — which is what this did first, and
//! what a test caught — cannot see a corner: the fix immediately after a
//! right-angle turn is only nine metres off the line it left, so the
//! corner is discarded as redundant before its deviation has a chance to
//! grow, and the turn vanishes. Holding the window means the corner is
//! still there to be committed when the evidence arrives.
//!
//! A simpler "one point every N metres" rule fails the other way round —
//! it rounds off every corner *and* still records a straight mile in sixty
//! pieces.
//!
//! ## Why segments, and not one long line
//!
//! A GPS that drops out for ten minutes and comes back leaves a gap. Join
//! it and the track shows you crossing water you never crossed, in a
//! straight line, at whatever speed the gap implies — which is exactly the
//! shape of a track that went *through* something. So a gap starts a new
//! segment and the chart shows a break, which is the truth: nobody knows
//! what happened in between.

use crate::geo::metres_to_nm;
use crate::route::Leg;
use crate::LatLon;

/// One recorded position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fix {
    pub at: LatLon,
    /// Unix seconds, as the fix itself was stamped — not when it was
    /// recorded. A cached position from a phone is minutes old, and a log
    /// that said otherwise would be wrong about the one thing a log is for.
    pub unix: i64,
}

/// Where the boat has been, in runs of continuous recording.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Track {
    /// Each run of recording, oldest first. A gap in the fixes starts a
    /// new one.
    pub segments: Vec<Vec<Fix>>,
    /// Fixes since the last committed point, oldest first.
    ///
    /// The window. Any of these may yet turn out to be a corner, so none
    /// can be thrown away until the line past it is known. They are all
    /// within tolerance of the line from the last committed point to the
    /// newest, which is why drawing only that line loses nothing.
    pending: Vec<Fix>,
    /// How many times the tolerance has been doubled to stay inside the
    /// point cap.
    ///
    /// It has to escalate, and the first version did not: re-thinning at
    /// a fixed twice-the-tolerance works once and then never again, since
    /// the track is already thinned to it — so the cap leaked and the
    /// track grew slowly without bound. A longer passage is now simply
    /// described more coarsely, which is the right trade.
    coarsening: u32,
}

/// Most fixes to hold in the window before forcing a commit.
///
/// Bounds both the memory and the work: finding the worst pending point
/// is linear in the window, so an unbounded one on a long straight
/// passage would grow without limit in both. A thousand fixes at one a
/// second is seventeen minutes, which is a mile and a half at six knots —
/// committing a point there costs nothing and is invisible.
const MAX_PENDING: usize = 1_024;

/// How the recorder decides what to keep.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thinning {
    /// How far off the line a point must be to be worth keeping, metres.
    ///
    /// Above GPS noise and below anything navigationally interesting. Set
    /// it below the receiver's own scatter and the track records jitter;
    /// set it much above and a real dog-leg gets straightened out.
    pub tolerance_m: f64,
    /// Longer than this between fixes and the track breaks, seconds.
    pub gap_secs: i64,
    /// Most points to hold, across every segment.
    ///
    /// On reaching it the whole track is thinned again at twice the
    /// tolerance rather than having its beginning dropped: losing where
    /// you started is worse than describing it more coarsely.
    pub max_points: usize,
}

impl Default for Thinning {
    fn default() -> Self {
        Self {
            // A consumer GPS scatters three to five metres, so ten is
            // comfortably outside the noise and well inside a boat length
            // of the truth.
            tolerance_m: 10.0,
            // At six knots a minute is 185 m. Beyond that nobody can say
            // what happened in between, so the track should not pretend.
            gap_secs: 60,
            // A day's sailing thinned properly is a few hundred points;
            // this is the backstop against a receiver that reports
            // nonsense, and it is still a small file.
            max_points: 20_000,
        }
    }
}

impl Track {
    /// Offer a fix to the track. True if the shape actually changed.
    ///
    /// Called for every fix — a few a second — so it has to be cheap and
    /// has to say when nothing happened, or the drawing and the saving
    /// would both run on every one.
    pub fn record(&mut self, fix: Fix, rules: &Thinning) -> bool {
        // Out-of-order or repeated fixes. A receiver that re-sends, or a
        // clock that steps backwards, must not make the track double back
        // on itself.
        if let Some(last) = self.newest() {
            if fix.unix < last.unix {
                return false;
            }
            // A gap: nobody knows what happened, so the line must break.
            if fix.unix - last.unix > rules.gap_secs {
                self.close_window();
                self.segments.push(vec![fix]);
                return true;
            }
        }

        if self.segments.is_empty() {
            self.segments.push(vec![fix]);
            return true;
        }

        self.pending.push(fix);

        // The line the window is measured against: from the last point
        // actually committed, to the newest fix.
        let Some(kept) = self.last_committed() else {
            return true;
        };
        let Some(&newest) = self.pending.last() else {
            return true;
        };
        if self.pending.len() < 2 {
            // Nothing between the two ends to judge yet.
            return true;
        }

        let line = Leg {
            from: kept.at,
            to: newest.at,
            to_index: 0,
        };
        let tolerance = self.tolerance(rules);

        // The worst of the interior points. The worst rather than the
        // first over the line, because that is the one the shape turns
        // about — committing an earlier one would leave the real corner
        // still pending and still liable to be smoothed away.
        let mut worst = 0.0;
        let mut worst_at = 0;
        for (index, point) in self.pending.iter().enumerate().take(self.pending.len() - 1) {
            let off = line.cross_track_m(point.at).abs();
            if off > worst {
                worst = off;
                worst_at = index;
            }
        }

        let forced = self.pending.len() > MAX_PENDING;
        if worst > tolerance || forced {
            if let Some(segment) = self.segments.last_mut() {
                segment.push(self.pending[worst_at]);
            }
            // Everything up to and including the committed point is now
            // described by the line through it.
            self.pending.drain(..=worst_at);

            // Coarsen until the cap actually holds. Looped rather than
            // doubled once, so one pass is always enough — and bounded,
            // because a tolerance of a thousand kilometres describes
            // nothing and there is no point going further.
            while self.len() > rules.max_points && self.coarsening < 20 {
                self.coarsening += 1;
                let _ = self.rethin(self.tolerance(rules));
            }
        }
        true
    }

    /// The tolerance in force, which coarsens as the track grows.
    fn tolerance(&self, rules: &Thinning) -> f64 {
        rules.tolerance_m * 2f64.powi(self.coarsening as i32)
    }

    /// The last point actually written into a segment.
    fn last_committed(&self) -> Option<Fix> {
        self.segments.last().and_then(|s| s.last()).copied()
    }

    /// Stop recording cleanly, committing whatever was held.
    ///
    /// Called when the feed goes away or the app closes, so the last leg
    /// of a passage is not lost to the lookahead.
    pub fn finish(&mut self) {
        self.close_window();
    }

    /// Commit the end of the window, so a track does not stop short of
    /// where the boat actually got to.
    fn close_window(&mut self) {
        let last = self.pending.last().copied();
        self.pending.clear();
        if let Some(last) = last {
            if let Some(segment) = self.segments.last_mut() {
                if segment.last() != Some(&last) {
                    segment.push(last);
                }
            } else {
                self.segments.push(vec![last]);
            }
        }
    }

    /// The newest fix known, committed or not.
    pub fn newest(&self) -> Option<Fix> {
        self.pending
            .last()
            .copied()
            .or_else(|| self.last_committed())
    }

    /// Points held, across every segment, including the candidate.
    /// Points held, across every segment.
    ///
    /// The window is not counted: it is working state, not track. Only
    /// one of its points will survive, and which one is not yet known.
    pub fn len(&self) -> usize {
        self.segments.iter().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&mut self) {
        self.segments.clear();
        self.pending.clear();
        self.coarsening = 0;
    }

    /// Distance travelled along the track, metres.
    ///
    /// Summed within segments only. Measuring across a gap would add a
    /// distance nobody knows was travelled — and it is usually the longest
    /// "leg" in the track, so it would dominate the total.
    pub fn distance_m(&self) -> f64 {
        let mut total = 0.0;
        for segment in &self.segments {
            for pair in segment.windows(2) {
                total += pair[0].at.distance_to(pair[1].at);
            }
        }
        // The open window counts to its newest point: that is where the
        // boat is, and a distance that stopped at the last corner would
        // read short by up to a mile on a straight run.
        if let (Some(newest), Some(kept)) = (self.pending.last(), self.last_committed()) {
            total += kept.at.distance_to(newest.at);
        }
        total
    }

    pub fn distance_nm(&self) -> f64 {
        metres_to_nm(self.distance_m())
    }

    /// When recording started and when it last had a fix.
    pub fn span(&self) -> Option<(i64, i64)> {
        let first = self.segments.iter().flatten().next()?.unix;
        Some((first, self.newest()?.unix))
    }

    /// Every segment as a list of positions, for drawing.
    ///
    /// The candidate is appended to the last one, so the drawn track
    /// reaches the boat rather than stopping at the most recent corner —
    /// which could be a mile back on a straight run.
    pub fn lines(&self) -> Vec<Vec<LatLon>> {
        let mut out: Vec<Vec<LatLon>> = self
            .segments
            .iter()
            .map(|segment| segment.iter().map(|f| f.at).collect())
            .collect();
        // Only the newest pending point, not the whole window: every
        // other one is within tolerance of the line to it, so drawing
        // them would add vertices and no shape.
        if let (Some(newest), Some(last)) = (self.pending.last(), out.last_mut()) {
            if last.last() != Some(&newest.at) {
                last.push(newest.at);
            }
        }
        out
    }

    /// Thin the whole track again at a coarser tolerance.
    ///
    /// Applied when the point cap is reached. Every segment is thinned in
    /// place and its ends are always kept, so the track still begins and
    /// finishes where it did.
    /// Returns how many points were dropped, for a caller that wants to
    /// say so — this crate has no logging of its own, which is the point
    /// of it having no dependencies.
    fn rethin(&mut self, tolerance_m: f64) -> usize {
        let before = self.len();
        for segment in &mut self.segments {
            *segment = thin(segment, tolerance_m);
        }
        before.saturating_sub(self.len())
    }
}

/// Douglas–Peucker over a whole segment.
///
/// Iterative rather than recursive: a segment at the point cap is twenty
/// thousand fixes deep, and a recursive implementation on a pathological
/// one — a track that doubles back on itself repeatedly — recurses once
/// per point. A phone's stack does not have twenty thousand frames in it.
fn thin(points: &[Fix], tolerance_m: f64) -> Vec<Fix> {
    if points.len() < 3 {
        return points.to_vec();
    }

    // Which points survive. The two ends always do: a thinning that moved
    // where a track started would be rewriting history.
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;

    let mut pending = vec![(0usize, points.len() - 1)];
    while let Some((first, last)) = pending.pop() {
        if last <= first + 1 {
            continue;
        }
        let line = Leg {
            from: points[first].at,
            to: points[last].at,
            to_index: 0,
        };

        let mut worst = 0.0;
        let mut worst_at = first;
        for (index, point) in points.iter().enumerate().take(last).skip(first + 1) {
            let off = line.cross_track_m(point.at).abs();
            if off > worst {
                worst = off;
                worst_at = index;
            }
        }

        if worst > tolerance_m {
            keep[worst_at] = true;
            pending.push((first, worst_at));
            pending.push((worst_at, last));
        }
    }

    points
        .iter()
        .zip(keep)
        .filter_map(|(point, keep)| keep.then_some(*point))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: i64 = 1_772_323_200;

    fn rules() -> Thinning {
        Thinning::default()
    }

    /// A fix `seconds` in, at a position.
    fn fix(lat: f64, lon: f64, seconds: i64) -> Fix {
        Fix {
            at: LatLon::new(lat, lon),
            unix: START + seconds,
        }
    }

    #[test]
    fn a_straight_run_collapses_to_its_ends() {
        // The whole point of thinning. Six knots for an hour due north is
        // 3,600 fixes describing one straight line, and a track that kept
        // them all would be unreadable by anything else and would bury
        // every turn in the rest of the passage.
        let mut track = Track::default();
        for second in 0..600 {
            // Due north at about six knots: 0.001 degrees of latitude is
            // 111 m, so 3 m a second.
            let lat = 32.0 + 0.0000278 * second as f64;
            track.record(fix(lat, -117.0, second), &rules());
        }
        track.finish();

        assert!(
            track.len() <= 4,
            "a straight line came out as {} points",
            track.len()
        );
        // And it still covers the ground: ten minutes at six knots is a
        // nautical mile.
        assert!(
            (track.distance_nm() - 1.0).abs() < 0.1,
            "{:.3} nm over ten minutes at six knots",
            track.distance_nm()
        );
    }

    #[test]
    fn a_corner_is_kept() {
        // The other half: thinning must not round off turns, which are
        // the part of a track worth having.
        let mut track = Track::default();
        // North for a mile.
        for second in 0..100 {
            track.record(fix(32.0 + 0.0001 * second as f64, -117.0, second), &rules());
        }
        // Then east for a mile.
        for second in 100..200 {
            track.record(
                fix(32.01, -117.0 + 0.0001 * (second - 100) as f64, second),
                &rules(),
            );
        }
        track.finish();

        assert!(
            track.len() >= 3,
            "a right-angle turn came out as {} points",
            track.len()
        );
        // The corner is in there: some point is near the turn.
        let corner = LatLon::new(32.01, -117.0);
        let nearest = track
            .segments
            .iter()
            .flatten()
            .map(|f| f.at.distance_to(corner))
            .fold(f64::MAX, f64::min);
        assert!(
            nearest < 50.0,
            "the nearest recorded point to the corner is {nearest:.0} m away"
        );
    }

    #[test]
    fn gps_jitter_does_not_become_a_track() {
        // A boat tied up still produces a fix a second, each a few metres
        // from the last. Recorded naively that is a track of hundreds of
        // points and a mile of "distance travelled" overnight.
        let mut track = Track::default();
        for second in 0..600 {
            // Noise of a few metres, deterministic so the test is too.
            let wobble = ((second as f64) * 1.7).sin() * 0.00003;
            let other = ((second as f64) * 2.3).cos() * 0.00003;
            track.record(fix(32.0 + wobble, -117.0 + other, second), &rules());
        }
        track.finish();

        assert!(
            track.len() < 60,
            "jitter produced {} points in ten minutes",
            track.len()
        );
        assert!(
            track.distance_nm() < 0.5,
            "a moored boat travelled {:.2} nm",
            track.distance_nm()
        );
    }

    #[test]
    fn a_gap_breaks_the_line_rather_than_crossing_it() {
        // The failure this prevents: a GPS that drops out for ten minutes
        // and comes back leaves a straight line across water nobody
        // crossed — which is exactly the shape of a track that went
        // *through* something.
        let mut track = Track::default();
        for second in 0..60 {
            track.record(fix(32.0 + 0.0001 * second as f64, -117.0, second), &rules());
        }
        // Ten minutes later, miles away.
        for second in 660..720 {
            track.record(
                fix(32.2 + 0.0001 * (second - 660) as f64, -117.2, second),
                &rules(),
            );
        }
        track.finish();

        assert_eq!(track.segments.len(), 2, "the gap did not break the track");
        // And the distance does not include the jump, which would be the
        // longest "leg" in the track and would dominate the total.
        assert!(
            track.distance_nm() < 2.0,
            "the gap was counted as distance travelled: {:.1} nm",
            track.distance_nm()
        );
    }

    #[test]
    fn a_clock_that_steps_backwards_does_not_fold_the_track() {
        let mut track = Track::default();
        for second in 0..30 {
            track.record(fix(32.0 + 0.0001 * second as f64, -117.0, second), &rules());
        }
        let before = track.len();
        let shape = track.lines();

        // A fix from a minute ago, which a re-sending receiver produces.
        assert!(
            !track.record(fix(32.5, -117.5, -60), &rules()),
            "an out-of-order fix was accepted"
        );
        assert_eq!(track.len(), before);
        assert_eq!(track.lines(), shape, "the track changed shape anyway");
    }

    #[test]
    fn the_drawn_track_reaches_the_boat() {
        // The lookahead holds one fix back, and on a straight run the last
        // committed point can be a mile astern. A track that visibly
        // stopped short of the boat would look broken.
        let mut track = Track::default();
        for second in 0..300 {
            track.record(
                fix(32.0 + 0.0000278 * second as f64, -117.0, second),
                &rules(),
            );
        }

        let newest = track.newest().expect("a newest fix");
        let drawn = track.lines();
        let last_drawn = *drawn.last().and_then(|s| s.last()).expect("a drawn point");
        assert_eq!(
            last_drawn, newest.at,
            "the drawn track stops short of the boat"
        );
    }

    #[test]
    fn the_point_cap_coarsens_rather_than_forgetting_the_start() {
        // Losing where you started is worse than describing it coarsely —
        // getting back out the way you came in is the commonest reason to
        // keep a track at all.
        let mut rules = rules();
        rules.max_points = 50;
        rules.tolerance_m = 1.0;

        let mut track = Track::default();
        // A wandering course, so almost every point is a corner at a
        // one-metre tolerance.
        for second in 0..4000 {
            let t = second as f64;
            track.record(
                fix(
                    32.0 + 0.0002 * t / 100.0 + (t / 7.0).sin() * 0.00008,
                    -117.0 + (t / 11.0).cos() * 0.00008,
                    second,
                ),
                &rules,
            );
        }
        track.finish();

        // The cap actually holds. It leaked in the first version:
        // re-thinning used a fixed twice-the-tolerance, which works once
        // and then never again because the track is already thinned to
        // it, so the point count crept up for ever.
        assert!(
            track.len() <= rules.max_points,
            "the cap did not hold: {} points for a cap of {}",
            track.len(),
            rules.max_points
        );
        // And the beginning is still there.
        let first = track
            .segments
            .first()
            .and_then(|s| s.first())
            .expect("a start");
        assert!(
            (first.unix - START).abs() < 60,
            "the track now starts {} seconds in",
            first.unix - START
        );
    }

    #[test]
    fn thinning_never_moves_the_ends() {
        let points: Vec<Fix> = (0..200)
            .map(|second| {
                fix(
                    32.0 + 0.0001 * second as f64,
                    -117.0 + ((second as f64) / 9.0).sin() * 0.0005,
                    second,
                )
            })
            .collect();

        for tolerance in [1.0, 10.0, 100.0, 10_000.0] {
            let thinned = thin(&points, tolerance);
            assert_eq!(thinned.first(), points.first(), "at {tolerance} m");
            assert_eq!(thinned.last(), points.last(), "at {tolerance} m");
            assert!(thinned.len() >= 2);
            assert!(thinned.len() <= points.len());
            // And the order is preserved, which an index-based filter can
            // get wrong in a way that looks like a track crossing itself.
            for pair in thinned.windows(2) {
                assert!(pair[0].unix < pair[1].unix, "out of order at {tolerance} m");
            }
        }
    }

    #[test]
    fn an_empty_or_tiny_track_answers_without_panicking() {
        let mut track = Track::default();
        assert!(track.is_empty());
        assert_eq!(track.len(), 0);
        assert_eq!(track.distance_m(), 0.0);
        assert_eq!(track.span(), None);
        assert!(track.lines().is_empty());
        assert_eq!(track.newest(), None);
        track.finish();
        assert!(track.is_empty());

        track.record(fix(32.0, -117.0, 0), &rules());
        assert_eq!(track.len(), 1);
        assert_eq!(track.distance_m(), 0.0, "one point is no distance");
        assert!(track.span().is_some());

        track.clear();
        assert!(track.is_empty());
    }

    #[test]
    fn the_span_runs_from_the_first_fix_to_the_last() {
        let mut track = Track::default();
        for second in [0, 10, 20, 400, 410] {
            track.record(fix(32.0 + 0.001 * second as f64, -117.0, second), &rules());
        }
        track.finish();
        let (from, to) = track.span().expect("a span");
        assert_eq!(from, START);
        assert_eq!(to, START + 410);
    }
}
