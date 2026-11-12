//! The tidal current predictor against NOAA's own published predictions.
//!
//! The currents share their arithmetic with the heights — the same
//! `Σ f·A·cos(V + u − κ)`, validated to 1 cm against NOAA across 2017–2035
//! by `tide_noaa.rs`. So why a second test?
//!
//! Because three things differ, and each is a way to be confidently wrong:
//!
//! * **The amplitudes are knots along a channel**, not metres of rise. A
//!   unit slip is a factor of two or fifty and would be obvious — but a
//!   *conversion* slip, cm/s read as m/s, is a factor of a hundred, and a
//!   factor of a hundred on a tenth of a knot still looks like a current.
//! * **There is a mean set.** `majorMeanSpeed` is a constant that never
//!   reverses: −0.09 kn at San Diego Bay Entrance. Dropping it moves slack
//!   water, which is precisely the number anyone reads a current table for.
//! * **Flood and ebb are published separately and are not reciprocal.** At
//!   this station the flood sets 355 and the ebb 173 — 178 degrees apart.
//!   Assuming `flood + 180` would put the ebb two degrees out here and
//!   much further out in a bend.
//!
//! The reference is 240 hourly values at five epochs spread across the
//! Moon's 18.6-year nodal cycle, because a node-factor error is invisible
//! for years at a time.
//!
//! Regenerate the fixture with:
//!
//! ```text
//! python3 scripts/current-fixture.py PCT0031 \
//!     > core/tests/data/current/PCT0031.txt
//! ```

// A malformed fixture is a broken test, not a runtime condition, so
// panicking on it is right here.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::expect_fun_call)]

use rutter_core::tide::{Constituent, CurrentKind, CurrentStation, Harmonic};

const FIXTURE: &str = include_str!("data/current/PCT0031.txt");

/// What NOAA says the water does, parsed out of the fixture.
struct Reference {
    station: CurrentStation,
    /// `(unix, knots along the flood axis)`, signed.
    velocities: Vec<(i64, f64)>,
}

fn parse(text: &str) -> Reference {
    let mut station = CurrentStation::default();
    let mut velocities = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut field = line.split_whitespace();
        let keyword = field.next().expect("a keyword");
        let number = |f: Option<&str>| -> f64 {
            f.expect(&format!("a number in {line:?}"))
                .parse()
                .expect(&format!("a number in {line:?}"))
        };

        match keyword {
            "station" => {
                station.id = field.next().expect("an id").to_owned();
                station.lat = number(field.next());
                station.lon = number(field.next());
                station.flood_deg = number(field.next());
                station.ebb_deg = number(field.next());
                station.mean_kn = number(field.next());
                station.name = field.collect::<Vec<_>>().join(" ");
            }
            "depth" => station.depth_m = Some(number(field.next())),
            "constituent" => {
                let name = field.next().expect("a constituent name");
                let amplitude = number(field.next());
                let phase_deg = number(field.next());
                // A constituent this crate does not know is a gap in the
                // table, not something to skip quietly: the prediction
                // would then be short a term and still look plausible.
                let harmonic = Harmonic::from_name(name)
                    .expect(&format!("{name} is not in the constituent table"));
                station.constituents.push(Constituent {
                    harmonic,
                    amplitude,
                    phase_deg,
                });
            }
            "velocity" => {
                let unix = number(field.next()) as i64;
                velocities.push((unix, number(field.next())));
            }
            other => panic!("unexpected keyword {other:?} in the fixture"),
        }
    }

    assert!(!station.constituents.is_empty(), "no constituents parsed");
    assert!(!velocities.is_empty(), "no reference velocities parsed");
    Reference {
        station,
        velocities,
    }
}

#[test]
fn the_prediction_matches_noaa_across_the_nodal_cycle() {
    let Reference {
        station,
        velocities,
    } = parse(FIXTURE);

    let mut worst = 0.0_f64;
    let mut worst_at = 0;
    let mut sum_squared = 0.0;

    for &(unix, published) in &velocities {
        let ours = station.along_axis_kn(unix);
        let error = (ours - published).abs();
        sum_squared += error * error;
        if error > worst {
            worst = error;
            worst_at = unix;
        }
    }

    let rms = (sum_squared / velocities.len() as f64).sqrt();

    // NOAA publishes these to 0.1 cm/s, which is 0.002 kn, so the floor on
    // agreement is the rounding in the reference itself. Two hundredths of
    // a knot is well inside that and far inside anything a navigator could
    // act on — a boat cannot steer to a fiftieth of a knot.
    assert!(
        rms < 0.02,
        "RMS error {rms:.4} kn over {} values; worst {worst:.4} kn at {worst_at}",
        velocities.len()
    );
    assert!(
        worst < 0.05,
        "worst error {worst:.4} kn at {worst_at} (RMS {rms:.4} kn)"
    );

    // And the agreement must hold at every epoch, not on average. A node
    // factor error is small in some years and large in others, so a good
    // mean can hide a bad decade.
    for window in velocities.chunks(48) {
        let (first, _) = window[0];
        let epoch_worst = window
            .iter()
            .map(|&(unix, published)| (station.along_axis_kn(unix) - published).abs())
            .fold(0.0_f64, f64::max);
        assert!(
            epoch_worst < 0.05,
            "worst error {epoch_worst:.4} kn in the window starting {first}"
        );
    }

    println!(
        "{} values, RMS {:.4} kn, worst {:.4} kn",
        velocities.len(),
        rms,
        worst
    );
}

#[test]
fn the_mean_set_is_not_dropped() {
    // The failure this guards: `majorMeanSpeed` ignored. It is small — a
    // tenth of a knot here — but it is a *constant*, so it does not cancel
    // over a cycle. It moves slack water, which is the one instant anyone
    // reads a current table for.
    let Reference {
        station,
        velocities,
    } = parse(FIXTURE);
    assert!(
        station.mean_kn.abs() > 0.001,
        "this fixture has no mean set, so it cannot test for one"
    );

    let without = CurrentStation {
        mean_kn: 0.0,
        ..station.clone()
    };

    let worst = velocities
        .iter()
        .map(|&(unix, published)| (without.along_axis_kn(unix) - published).abs())
        .fold(0.0_f64, f64::max);
    assert!(
        worst > 0.05,
        "dropping the mean set changed nothing, so the test proves nothing"
    );
}

#[test]
fn the_flood_and_ebb_are_the_published_directions() {
    // Not `flood + 180`. NOAA publishes 355 and 173 for this station: the
    // channel is not quite straight, and the water takes a different line
    // on the way out.
    let Reference { station, .. } = parse(FIXTURE);
    assert_eq!(station.flood_deg, 355.0);
    assert_eq!(station.ebb_deg, 173.0);

    let reciprocal = (station.flood_deg + 180.0) % 360.0;
    assert!(
        (station.ebb_deg - reciprocal).abs() > 1.0,
        "this station's ebb is reciprocal, so it cannot test the distinction"
    );

    // And the resolved current uses whichever applies.
    let Reference { velocities, .. } = parse(FIXTURE);
    let flooding = velocities
        .iter()
        .find(|&&(_, v)| v > 0.2)
        .expect("a flood somewhere in the reference");
    let ebbing = velocities
        .iter()
        .find(|&&(_, v)| v < -0.2)
        .expect("an ebb somewhere in the reference");

    assert_eq!(station.current_at(flooding.0).toward_deg, 355.0);
    assert!(station.current_at(flooding.0).flooding);
    assert_eq!(station.current_at(ebbing.0).toward_deg, 173.0);
    assert!(!station.current_at(ebbing.0).flooding);

    // Speed is never negative; the direction carries the sign.
    for &(unix, _) in &velocities {
        assert!(
            station.current_at(unix).speed_kn >= 0.0,
            "a negative speed at {unix}"
        );
    }
}

#[test]
fn the_current_is_as_strong_as_noaa_says_it_gets() {
    // A sanity check in the units a navigator uses. San Diego Bay entrance
    // runs a bit under three knots at springs; if this came out at 0.03 or
    // at 150 the conversion from cm/s would be wrong by a factor of a
    // hundred in one direction or the other, and the RMS test above would
    // catch it — but this says so in a way a human can check against a
    // tidal current table.
    let Reference { station, .. } = parse(FIXTURE);
    let peak = station
        .curve(1_772_323_200, 1_772_323_200 + 30 * 86_400, 600)
        .into_iter()
        .map(|(_, v)| v.abs())
        .fold(0.0_f64, f64::max);
    assert!(
        (1.5..4.0).contains(&peak),
        "peak current over a month was {peak:.2} kn, which is not a harbour entrance"
    );
}

#[test]
fn slack_water_and_the_peaks_alternate() {
    // A current table reads slack, max, slack, max. Anything else means
    // the turning-point search is finding noise or missing events.
    let Reference { station, .. } = parse(FIXTURE);
    let from = 1_772_323_200;
    let events = station.events(from, from + 3 * 86_400);

    assert!(
        events.len() > 15,
        "only {} events in three days; a semidiurnal current has about 24",
        events.len()
    );

    let mut previous_was_slack = None;
    for event in &events {
        let is_slack = event.kind == CurrentKind::Slack;
        if let Some(was_slack) = previous_was_slack {
            assert_ne!(
                is_slack,
                was_slack,
                "two {} in a row at {}",
                if is_slack { "slacks" } else { "peaks" },
                event.unix
            );
        }
        previous_was_slack = Some(is_slack);

        match event.kind {
            // Slack means stopped. Found by bisection to the second, so it
            // should be very close to zero indeed.
            CurrentKind::Slack => assert!(
                event.speed_kn < 0.01,
                "slack at {} was running {:.3} kn",
                event.unix,
                event.speed_kn
            ),
            // A peak is a real current, and sets along the right axis.
            CurrentKind::MaxFlood => {
                assert!(event.speed_kn > 0.1, "a feeble flood at {}", event.unix);
                assert_eq!(event.toward_deg, station.flood_deg);
            }
            CurrentKind::MaxEbb => {
                assert!(event.speed_kn > 0.1, "a feeble ebb at {}", event.unix);
                assert_eq!(event.toward_deg, station.ebb_deg);
            }
        }
    }

    // Floods and ebbs must both occur, and alternate with each other.
    let peaks: Vec<CurrentKind> = events
        .iter()
        .map(|e| e.kind)
        .filter(|k| *k != CurrentKind::Slack)
        .collect();
    for pair in peaks.windows(2) {
        assert_ne!(pair[0], pair[1], "two peaks of the same sign in a row");
    }
}

#[test]
fn a_station_with_no_constituents_predicts_its_mean_and_nothing_else() {
    // The degenerate case, which is what a river gauge with only a net
    // flow would look like. It must not panic and must not invent a tide.
    let river = CurrentStation {
        mean_kn: -1.4,
        flood_deg: 10.0,
        ebb_deg: 190.0,
        ..CurrentStation::default()
    };
    let current = river.current_at(1_772_323_200);
    assert!((current.speed_kn - 1.4).abs() < 1e-9);
    assert!(!current.flooding, "a net seaward flow is an ebb");
    assert_eq!(current.toward_deg, 190.0);

    // And it never turns, so there is no slack water to report.
    let events = river.events(1_772_323_200, 1_772_323_200 + 2 * 86_400);
    assert!(
        events.iter().all(|e| e.kind != CurrentKind::Slack),
        "a river with no reversing tide reported slack water"
    );
}
