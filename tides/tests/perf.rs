//! What the tide panel costs per frame.
//!
//! The panel asks for a 24-hour curve and the next four turning points.
//! Both were computed every frame, and `Station::height_at` recomputed the
//! nodal corrections once per constituent rather than once per instant —
//! thirty-odd `acos`/`atan2` evaluations that are identical for every term.
//! Together that was several milliseconds a frame for numbers that change
//! once a second.
//!
//! The app now caches the readings per second and holds the station list in
//! memory. These bounds guard the arithmetic underneath, which is what made
//! caching affordable in the first place.
//!
//! Needs `build/charts/tides.sqlite`; skipped without it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Instant;

use rutter_core::LatLon;
use rutter_tides::TideStations;

/// 2026-03-01T00:00:00Z.
const WHEN: i64 = 1_772_323_200;

fn millis_per(runs: u32, started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0 / f64::from(runs)
}

#[test]
fn predicting_a_tide_is_cheap() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../build/charts/tides.sqlite");
    if !std::path::Path::new(path).exists() {
        eprintln!("skipping: no build/charts/tides.sqlite");
        return;
    }

    let db = TideStations::open(path).unwrap();
    let station = db
        .nearest(LatLon::new(32.7, -117.2))
        .unwrap()
        .expect("a station");

    let runs = 20;
    let started = Instant::now();
    for _ in 0..runs {
        let _ = station.height_at(WHEN);
    }
    let one = millis_per(runs, started);

    let started = Instant::now();
    for _ in 0..runs {
        let _ = station.curve(WHEN - 3 * 3600, WHEN + 21 * 3600, 720);
    }
    let curve = millis_per(runs, started);

    let started = Instant::now();
    for _ in 0..runs {
        let _ = station.extremes(WHEN, WHEN + 21 * 3600);
    }
    let extremes = millis_per(runs, started);

    println!(
        "height_at {one:.4} ms, 120-sample curve {curve:.3} ms, \
         21 h of extremes {extremes:.3} ms"
    );

    // Each of these was six times slower when `Nodal` was recomputed per
    // constituent. The bounds are loose; the regression was not subtle.
    assert!(one < 0.05, "one height took {one:.4} ms");
    assert!(curve < 2.0, "a 120-sample curve took {curve:.2} ms");
    assert!(extremes < 4.0, "21 h of extremes took {extremes:.2} ms");
}

#[test]
fn loading_every_station_is_a_startup_cost_not_a_frame_cost() {
    // The app reads these once and keeps them. The bound is here to say
    // that doing so is cheap, not to permit calling it per frame — a
    // station is a SQL query plus one more for its constituents.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../build/charts/tides.sqlite");
    if !std::path::Path::new(path).exists() {
        eprintln!("skipping: no database");
        return;
    }

    let db = TideStations::open(path).unwrap();
    let started = Instant::now();
    let all = db.all().unwrap();
    let once = started.elapsed().as_secs_f64() * 1000.0;

    println!("{} stations loaded in {once:.2} ms", all.len());
    assert!(!all.is_empty());
    assert!(
        once < 200.0,
        "loading {} stations took {once:.0} ms",
        all.len()
    );
}
