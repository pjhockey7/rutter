//! What a frame costs, with bounds loose enough to be about regressions.
//!
//! These exist because panning and zooming went from smooth to unusable and
//! the cause was not in the renderer. Two things were wrong, and both were
//! invisible to every other test:
//!
//! * `refresh_charts` collected the whole viewport's geometry and *then*
//!   compared the tile list, so an unchanged view still decoded and cloned
//!   thousands of features sixty times a second.
//! * the workspace's own code was built unoptimised by `rutter run`, which
//!   made a tile-set change cost 0.8 to 2.8 seconds instead of 0.1 to 0.3.
//!
//! The bounds below are generous — a slow machine should not fail them —
//! but either of those two regressions blows straight through them.
//!
//! Needs `build/charts/noaa.mbtiles`. These are skipped without it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Instant;

use rutter_charts::ChartSource;
use rutter_core::{LatLon, View};

fn chart() -> Option<&'static str> {
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../build/charts/noaa.mbtiles");
    std::path::Path::new(p).exists().then_some(p)
}

/// San Diego Bay, which is where the chart is dense.
fn view_at(zoom: f64) -> View {
    View::new(LatLon::new(32.6836, -117.2206), zoom, 1280, 800)
}

#[test]
fn a_repeat_collect_is_cheap_enough_to_do_per_frame() {
    // Not that it *should* be done per frame — the app checks the tile list
    // first — but it is the fallback cost if that check is ever lost, and it
    // is what made the difference between smooth and unusable.
    let Some(p) = chart() else {
        eprintln!("skipping: no build/charts/noaa.mbtiles");
        return;
    };
    let mut source = ChartSource::open(p, &[]).unwrap();
    let max = f64::from(source.metadata().max_zoom);
    let view = view_at(max);

    let warm = source.collect(&view);
    let runs = 10;
    let started = Instant::now();
    for _ in 0..runs {
        let _ = source.collect(&view);
    }
    let per_frame = started.elapsed().as_secs_f64() * 1000.0 / f64::from(runs);

    println!(
        "{} tiles, {} lines, {} points, {} areas: {per_frame:.2} ms per repeat collect",
        warm.tiles.len(),
        warm.lines.len(),
        warm.points.len(),
        warm.areas.len()
    );
    assert!(
        per_frame < 40.0,
        "a cached collect took {per_frame:.1} ms, which is more than two \
         frames at 60 Hz — is the decoded-tile cache working?"
    );
}

#[test]
fn a_tile_set_change_does_not_freeze_the_app() {
    // A zoom always changes the tile set, so this is the cost of every zoom
    // step: a cold decode and triangulation of everything newly visible.
    // Unoptimised this was 2.8 seconds at the shallow end.
    let Some(p) = chart() else {
        eprintln!("skipping: no chart");
        return;
    };
    let max = f64::from(ChartSource::open(p, &[]).unwrap().metadata().max_zoom);

    for out in 0..3 {
        // A fresh source each time, so nothing is cached.
        let mut source = ChartSource::open(p, &[]).unwrap();
        let zoom = max - f64::from(out);
        let view = view_at(zoom);

        let started = Instant::now();
        let got = source.collect(&view);
        let cold = started.elapsed().as_secs_f64() * 1000.0;

        let started = Instant::now();
        let _ = source.collect(&view);
        let warm = started.elapsed().as_secs_f64() * 1000.0;

        println!(
            "z{zoom}: {} tiles, {} areas -> cold {cold:.1} ms, warm {warm:.1} ms",
            got.tiles.len(),
            got.areas.len()
        );
        assert!(
            cold < 1500.0,
            "a tile-set change at zoom {zoom} took {cold:.0} ms — an \
             unoptimised build of this workspace is the usual cause"
        );
    }
}
