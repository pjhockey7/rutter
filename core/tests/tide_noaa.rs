//! The tide predictor against NOAA's own published predictions.
//!
//! Everything else about the tide code can be checked for self-consistency
//! — that highs and lows alternate, that the period is 12.42 hours, that
//! springs exceed neaps. None of that catches a wrong answer: a predictor
//! with a mismatched epoch produces a perfectly well-formed tide at the
//! wrong time, and one with a quadrant mistake in the nodal correction
//! produces the right tide for nine years and the wrong one for the next
//! nine. Both of those happened while this was being written.
//!
//! So this test does the only thing that settles it: predicts from the
//! published harmonic constituents and compares against the heights NOAA
//! publishes for the same instants, at five epochs spread across the Moon's
//! 18.6-year nodal cycle.
//!
//! Regenerate the fixture with:
//!
//! ```text
//! python3 scripts/tide-fixture.py 9410170 > core/tests/data/tide/9410170.txt
//! ```

// A malformed fixture is a broken test, not a runtime condition to recover
// from, so panicking on it is the right behaviour here. `expect_fun_call` is
// allowed with it: the message names the offending line, and building that
// eagerly per line costs nothing in a test that runs in milliseconds.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::expect_fun_call)]

use rutter_core::tide::{Constituent, Extreme, Harmonic, Kind, Station};

const FIXTURE: &str = include_str!("data/tide/9410170.txt");

/// What NOAA says the water does, parsed out of the fixture.
struct Reference {
    station: Station,
    heights: Vec<(i64, f64)>,
    extremes: Vec<Extreme>,
}

fn parse(text: &str) -> Reference {
    let mut station = Station::default();
    let mut heights = Vec::new();
    let mut extremes = Vec::new();

    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut field = line.split_whitespace();
        let at = |what: &str| format!("{what} on fixture line {}", number + 1);

        match field.next().expect("a non-empty line has a first field") {
            "station" => {
                let mut next = |what: &str| field.next().expect(&at(what)).to_owned();
                station.id = next("station id");
                station.lat = next("latitude").parse().expect(&at("latitude"));
                station.lon = next("longitude").parse().expect(&at("longitude"));
                station.datum_m = next("datum").parse().expect(&at("datum"));
                // The name is the rest of the line, spaces and all.
                station.name = field.collect::<Vec<_>>().join(" ");
            }
            "constituent" => {
                let name = field.next().expect(&at("constituent name"));
                let harmonic = Harmonic::from_name(name)
                    .unwrap_or_else(|| panic!("{} is not a known constituent", at(name)));
                station.constituents.push(Constituent {
                    harmonic,
                    amplitude_m: field
                        .next()
                        .expect(&at("amplitude"))
                        .parse()
                        .expect(&at("amplitude")),
                    phase_deg: field
                        .next()
                        .expect(&at("phase"))
                        .parse()
                        .expect(&at("phase")),
                });
            }
            "prediction" => {
                let unix: i64 = field
                    .next()
                    .expect(&at("timestamp"))
                    .parse()
                    .expect(&at("timestamp"));
                let metres: f64 = field
                    .next()
                    .expect(&at("height"))
                    .parse()
                    .expect(&at("height"));
                heights.push((unix, metres));
            }
            "extreme" => {
                let unix: i64 = field
                    .next()
                    .expect(&at("timestamp"))
                    .parse()
                    .expect(&at("timestamp"));
                let kind = match field.next().expect(&at("kind")) {
                    "H" => Kind::High,
                    "L" => Kind::Low,
                    other => panic!("{}: unknown extreme kind {other}", at("kind")),
                };
                let metres: f64 = field
                    .next()
                    .expect(&at("height"))
                    .parse()
                    .expect(&at("height"));
                extremes.push(Extreme {
                    unix,
                    height_m: metres,
                    kind,
                });
            }
            other => panic!("{}: unknown record {other}", at("record")),
        }
    }

    Reference {
        station,
        heights,
        extremes,
    }
}

/// Reference points that sit more than a day apart start a new window.
///
/// The windows are the whole point — each is at a different place in the
/// nodal cycle — so errors are reported per window rather than pooled. A
/// nodal bug shows up as one window being far worse than the others, which
/// a single aggregate number would hide.
fn windows(heights: &[(i64, f64)]) -> Vec<&[(i64, f64)]> {
    let mut out = Vec::new();
    let mut start = 0;
    for i in 1..heights.len() {
        if heights[i].0 - heights[i - 1].0 > 86_400 {
            out.push(&heights[start..i]);
            start = i;
        }
    }
    if start < heights.len() {
        out.push(&heights[start..]);
    }
    out
}

#[test]
fn the_fixture_is_whole() {
    // A truncated or half-written fixture would quietly weaken every
    // assertion below, so the shape is checked before it is used.
    let r = parse(FIXTURE);

    assert_eq!(r.station.id, "9410170");
    assert!(r.station.name.contains("San Diego"), "{}", r.station.name);
    assert!((32.0..33.0).contains(&r.station.lat), "{}", r.station.lat);
    assert!(
        (-118.0..-117.0).contains(&r.station.lon),
        "{}",
        r.station.lon
    );
    assert!(r.station.datum_m > 0.5, "datum {}", r.station.datum_m);

    assert!(
        r.station.constituents.len() >= 30,
        "only {} constituents",
        r.station.constituents.len()
    );
    assert!(
        r.heights.len() >= 1000,
        "only {} reference heights",
        r.heights.len()
    );
    assert!(
        r.extremes.len() >= 150,
        "only {} reference extremes",
        r.extremes.len()
    );

    // M2 has to be there and has to dominate, or this is not a tide.
    let m2 = r
        .station
        .constituents
        .iter()
        .find(|c| c.harmonic.name() == "M2")
        .expect("no M2");
    let biggest = r
        .station
        .constituents
        .iter()
        .map(|c| c.amplitude_m)
        .fold(f64::MIN, f64::max);
    assert_eq!(m2.amplitude_m, biggest, "M2 should be the largest term");

    let spans = windows(&r.heights);
    assert!(
        spans.len() >= 4,
        "the fixture should span several epochs, found {}",
        spans.len()
    );

    // The windows must actually be spread across the nodal cycle, or they
    // test one phase of it five times over. The node regresses ~19.3°/yr,
    // so a decent spread means many years between first and last.
    let first = r.heights.first().expect("non-empty").0;
    let last = r.heights.last().expect("non-empty").0;
    let years = (last - first) as f64 / (365.2422 * 86_400.0);
    assert!(
        years > 12.0,
        "windows span only {years:.1} years; the nodal cycle is 18.6"
    );
}

#[test]
fn predicted_heights_match_noaa() {
    let r = parse(FIXTURE);

    // NOAA publishes heights to the centimetre, and the fit of these same
    // 32 constituents to NOAA's curve leaves about 1 cm of constituents it
    // models and we cannot separate. So 1 cm is the floor, not a target,
    // and 2.5 cm of RMS is the honest bar: loose enough not to be chasing
    // NOAA's rounding, tight enough that every bug found so far — a 6.6°
    // epoch mismatch, a missing node factor, a quadrant error — blows
    // straight through it.
    const RMS_LIMIT: f64 = 0.025;
    const MAX_LIMIT: f64 = 0.08;

    let mut worst_window = 0.0f64;
    let mut pooled = Vec::new();

    for span in windows(&r.heights) {
        let errors: Vec<f64> = span
            .iter()
            .map(|&(unix, observed)| r.station.height_at(unix) - observed)
            .collect();

        let n = errors.len() as f64;
        let rms = (errors.iter().map(|e| e * e).sum::<f64>() / n).sqrt();
        let bias = errors.iter().sum::<f64>() / n;
        let max = errors.iter().fold(0.0f64, |a, e| a.max(e.abs()));

        // Printed so that a failure says *which* epoch went wrong, which is
        // the difference between "the maths is broken" and "the nodal
        // correction is broken".
        println!(
            "window at unix {}: n={} rms={:.4} bias={:+.4} max={:.4}",
            span[0].0,
            span.len(),
            rms,
            bias,
            max
        );

        assert!(
            rms < RMS_LIMIT,
            "window at unix {} has rms {rms:.4} m, over the {RMS_LIMIT} m limit",
            span[0].0
        );
        assert!(
            max < MAX_LIMIT,
            "window at unix {} has a {max:.4} m error, over the {MAX_LIMIT} m limit",
            span[0].0
        );
        // A bias is a datum problem, not a phase problem, and it would
        // survive even a correct nodal correction.
        assert!(
            bias.abs() < 0.01,
            "window at unix {} is biased {bias:+.4} m",
            span[0].0
        );

        worst_window = worst_window.max(rms);
        pooled.extend(errors);
    }

    let n = pooled.len() as f64;
    let rms = (pooled.iter().map(|e| e * e).sum::<f64>() / n).sqrt();
    println!(
        "pooled over {} points: rms={:.4} m, worst window {:.4} m",
        pooled.len(),
        rms,
        worst_window
    );
    assert!(rms < RMS_LIMIT, "pooled rms {rms:.4} m");

    // No window may be wildly worse than the best. A nodal-correction bug
    // that happens to be mild in the epochs sampled would still show as a
    // spread, and this is what would notice.
    assert!(
        worst_window < 4.0 * rms,
        "one window ({worst_window:.4} m) is far worse than the pooled {rms:.4} m"
    );
}

#[test]
fn predicted_high_and_low_waters_match_noaa() {
    let r = parse(FIXTURE);

    // NOAA publishes turning points to the minute. Ours come from bisecting
    // the derivative, so the limit here is the height error divided by how
    // fast the water is moving near a turning point — which is slow, so a
    // centimetre of height becomes several minutes of time. Twelve minutes
    // is comfortable without being meaningless.
    const TIME_LIMIT: i64 = 12 * 60;
    const HEIGHT_LIMIT: f64 = 0.08;

    let mut matched = 0;
    let mut worst_time = 0i64;
    let mut worst_height = 0.0f64;

    for span in windows(&r.heights) {
        let from = span[0].0;
        let to = span[span.len() - 1].0;
        let ours = r.station.extremes(from, to);

        for published in r.extremes.iter().filter(|e| e.unix >= from && e.unix <= to) {
            let nearest = ours
                .iter()
                .min_by_key(|e| (e.unix - published.unix).abs())
                .unwrap_or_else(|| {
                    panic!("no predicted extremes at all in the window from {from}")
                });

            let off_by = (nearest.unix - published.unix).abs();
            let height_off = (nearest.height_m - published.height_m).abs();

            assert_eq!(
                nearest.kind, published.kind,
                "at unix {} NOAA says {:?}, we say {:?}",
                published.unix, published.kind, nearest.kind
            );
            assert!(
                off_by <= TIME_LIMIT,
                "the {:?} at unix {} came out {} s off",
                published.kind,
                published.unix,
                off_by
            );
            assert!(
                height_off <= HEIGHT_LIMIT,
                "the {:?} at unix {} came out {height_off:.4} m off",
                published.kind,
                published.unix
            );

            matched += 1;
            worst_time = worst_time.max(off_by);
            worst_height = worst_height.max(height_off);
        }
    }

    println!(
        "{matched} published high/low waters matched; worst {} s and {:.4} m",
        worst_time, worst_height
    );
    assert!(matched >= 150, "only matched {matched} extremes");
}

#[test]
fn every_published_extreme_has_exactly_one_of_ours() {
    // The height test could pass while `extremes` invented spurious
    // turning points or merged two into one, so the counts are checked
    // against NOAA's independently.
    let r = parse(FIXTURE);

    for span in windows(&r.heights) {
        let from = span[0].0;
        let to = span[span.len() - 1].0;

        let theirs = r
            .extremes
            .iter()
            .filter(|e| e.unix >= from && e.unix <= to)
            .count();
        let ours = r.station.extremes(from, to).len();

        // Allow one at each end: a turning point within minutes of the
        // window edge may fall either side of it.
        let difference = (ours as i64 - theirs as i64).abs();
        assert!(
            difference <= 2,
            "window from {from}: NOAA has {theirs} turning points, we found {ours}"
        );
    }
}
