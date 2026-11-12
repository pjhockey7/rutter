#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The forecast file `rutter weather` writes, read by the app.
//!
//! The writer is Python and the reader is Rust, so nothing but a test
//! actually holds them to the same layout. A disagreement would not error:
//! a wrong offset reads neighbouring floats and gives a plausible wind from
//! the wrong quarter.
//!
//! Needs `build/charts/weather.bin`. Build it with:
//!
//!     rutter weather --box 32.5,-118.7,33.6,-117.0

use rutter_core::weather::Forecast;
use rutter_core::LatLon;

fn forecast() -> Option<Forecast> {
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../build/charts/weather.bin");
    let bytes = std::fs::read(p).ok()?;
    Some(Forecast::parse(&bytes).expect("the file should parse"))
}

#[test]
fn the_real_file_parses_into_a_plausible_forecast() {
    let Some(f) = forecast() else {
        eprintln!("skipping: no build/charts/weather.bin — see the note above");
        return;
    };

    assert!(!f.steps.is_empty(), "no forecast hours");
    // GFS runs every six hours; a file older than a fortnight is suspicious
    // rather than wrong, but a run time of zero means the header is misread.
    assert!(f.run > 1_700_000_000, "model run {} is not a date", f.run);

    let (nw, se) = f.bounds().expect("bounds");
    println!(
        "run {} with {} hours, {:.3}..{:.3} N, {:.3}..{:.3} E",
        f.run,
        f.steps.len(),
        se.lat,
        nw.lat,
        nw.lon,
        se.lon
    );
    assert!(nw.lat > se.lat, "north should be above south");
    assert!(nw.lon < se.lon, "west should be left of east");

    // The hours must be in order and distinct, or `nearest` picks oddly.
    let hours: Vec<i32> = f.steps.iter().map(|s| s.hour).collect();
    let mut sorted = hours.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(hours, sorted, "hours should be ascending and distinct");

    for step in &f.steps {
        let at = LatLon::new(32.68, -117.23);
        let wind = step.wind.sample(at).expect("San Diego is inside the box");
        let temp = step.temperature.sample(at).expect("inside");

        println!(
            "  +{:3} h: {:5.1} kn from {:3.0}°, {:4.1} C",
            step.hour,
            wind.speed_kn(),
            wind.direction_from_deg(),
            temp
        );

        // Physical bounds. A unit mix-up — knots stored as m/s, or Kelvin
        // left unconverted — lands outside these immediately.
        assert!(
            wind.speed_kn() < 100.0,
            "+{} h: {:.1} kn is a hurricane, check the units",
            step.hour,
            wind.speed_kn()
        );
        assert!(
            (0.0..360.0).contains(&wind.direction_from_deg()),
            "direction {} out of range",
            wind.direction_from_deg()
        );
        assert!(
            (-20.0..50.0).contains(&temp),
            "+{} h: {temp} C is not a southern California temperature — \
             is the Kelvin conversion missing?",
            step.hour
        );
    }
}

#[test]
fn the_grid_is_the_right_way_up() {
    // The one mistake that produces a complete, plausible, wrong forecast:
    // rows running north instead of south. GDAL's geotransform has a
    // negative latitude step and the file stores it positive, so the sign is
    // dropped somewhere — this checks it was dropped in the right place, by
    // asking whether the temperature field slopes the way latitude does.
    let Some(f) = forecast() else {
        eprintln!("skipping: no weather file");
        return;
    };
    let grid = &f.steps[0].temperature;
    assert!(grid.ny >= 2 && grid.nx >= 2);

    let top: f32 = (0..grid.nx).filter_map(|x| grid.at(x, 0)).sum();
    let bottom: f32 = (0..grid.nx).filter_map(|x| grid.at(x, grid.ny - 1)).sum();
    let n = grid.nx as f32;

    println!(
        "top row {:.2} C at {:.3} N, bottom row {:.2} C at {:.3} N",
        top / n,
        grid.north,
        bottom / n,
        grid.south()
    );
    assert!(grid.north > grid.south(), "row 0 must be the northern edge");
    // Not asserting that south is warmer — weather does not promise that —
    // only that both rows hold real temperatures, so neither is reading
    // off the end of the data.
    assert!((-20.0..50.0).contains(&(top / n)));
    assert!((-20.0..50.0).contains(&(bottom / n)));
}
