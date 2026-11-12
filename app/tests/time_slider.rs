//! That the time slider drives every layer it should, and nothing it should not.
//!
//! The slider is one `i64` applied in one place, which is the whole design
//! — but "one place" is only true if every time-dependent layer actually
//! reads it. A layer that quietly kept using the clock would look right at
//! now and be wrong everywhere else on the slider, which is the hardest
//! kind of error to notice: the chart still draws, the numbers still look
//! like numbers, and only the relationship between them is broken.
//!
//! So this drags the slider and checks that each layer moved, through the
//! same `PanelState` the interface reads.
//!
//! The one layer that must *not* move is the boat. A GPS says where the
//! boat is; no slider can say where it will be, and a plotter that slid
//! the boat into the future while the water moved with it would be worse
//! than one with no slider at all.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rutter_app::{App, Sources};

/// The chart and station files this project is developed against.
const TIDES: &str = "../build/charts/tides.sqlite";

/// Open an app with the real tide and current stations, if they are there.
///
/// A fresh clone has no `build/`, so this skips rather than fails — the
/// same rule the other fixture-backed tests here follow.
fn with_stations() -> Option<App> {
    if !std::path::Path::new(TIDES).exists() {
        eprintln!("no {TIDES}; skipping. Run `rutter tides` to generate it.");
        return None;
    }
    let tides = rutter_tides::TideStations::open(TIDES).expect("open the tide database");
    Some(App::new(
        Sources {
            charts: None,
            imagery: None,
            tides: Some(tides),
            weather: None,
            nmea: None,
            gps: None,
        },
        Some(2.0),
    ))
}

/// Move the slider by hand, as the panel's action would, and recompute
/// what depends on it.
///
/// `refresh_tide_cache` is what `App::refresh` calls each frame; calling it
/// directly is how this test gets at the tide without a GPU.
fn slide(app: &mut App, hours: i64) {
    app.set_time_offset(hours * 3600);
    app.refresh_tide_cache();
}

/// Put the view over the water the stations are in.
fn look_at(app: &mut App, lat: f64, lon: f64) {
    app.view.centre = rutter_app::Position::new(lat, lon);
    app.view.zoom = 12.0;
    app.refresh_tide_cache();
}

#[test]
fn the_tide_changes_when_the_slider_moves() {
    let Some(mut app) = with_stations() else {
        return;
    };
    // San Diego Bay entrance, where the stations are.
    look_at(&mut app, 32.68, -117.23);

    let now = app.tide_window().expect("a tide at now").now_m;

    // Six hours is about half a tidal cycle, so the height must have moved
    // substantially — a tide that changed by a centimetre over six hours
    // would mean the slider was not reaching the predictor.
    slide(&mut app, 6);
    let later = app.tide_window().expect("a tide six hours out").now_m;
    assert!(
        (later - now).abs() > 0.3,
        "the tide went from {now:.3} m to {later:.3} m over six hours"
    );

    // And back. The predictor is a pure function of time, so returning the
    // slider must return the number exactly.
    slide(&mut app, 0);
    let back = app.tide_window().expect("a tide at now again").now_m;
    assert!(
        (back - now).abs() < 1e-9,
        "returning to now gave {back:.6} m instead of {now:.6} m"
    );
}

#[test]
fn the_under_keel_window_follows_the_slider() {
    let Some(mut app) = with_stations() else {
        return;
    };
    look_at(&mut app, 32.68, -117.23);

    // The window is "the shown time, and the lowest water in the twelve
    // hours after it". Both halves have to follow the slider, and the
    // second is the one that could silently not: if the low-water search
    // kept looking from the real now, the clearance shading would answer
    // a different question from the one the slider asked.
    //
    // Checked by sampling across a day rather than by comparing two
    // offsets, because two offsets is not a valid test: a six-hour shift
    // moves the window but can easily leave the same low water inside it,
    // so the lowest height legitimately does not change. What cannot
    // happen, if the search is tracking, is for it to be the *same* over
    // a whole day.
    let mut lows = Vec::new();
    for hours in 0..12 {
        slide(&mut app, hours * 2);
        let window = app.tide_window().expect("a tide window");
        // The lowest water in a window is never above its start.
        assert!(
            window.lowest_m <= window.now_m + 1e-9,
            "at +{}h the lowest {:.3} m is above the start {:.3} m",
            hours * 2,
            window.lowest_m,
            window.now_m
        );
        lows.push((window.lowest_m * 1000.0) as i64);
    }

    lows.sort_unstable();
    lows.dedup();
    assert!(
        lows.len() > 3,
        "only {} distinct low waters across a day of offsets, so the search \
         is not following the slider",
        lows.len()
    );
}

#[test]
fn the_current_changes_when_the_slider_moves() {
    let Some(mut app) = with_stations() else {
        return;
    };
    look_at(&mut app, 32.68, -117.23);

    let state = app.current_state();
    if !state.available {
        eprintln!("no harmonic current stations in this database; skipping");
        return;
    }
    let now = state.now.clone().expect("a current at now");

    // Three hours is roughly flood to ebb at a semidiurnal entrance, so
    // the sentence should read differently.
    slide(&mut app, 3);
    let later = app
        .current_state()
        .now
        .clone()
        .expect("a current three hours out");
    assert_ne!(
        now, later,
        "the current reads the same three hours apart, so the slider is not reaching it"
    );

    slide(&mut app, 0);
    assert_eq!(
        app.current_state().now.as_deref(),
        Some(now.as_str()),
        "returning to now did not restore the current"
    );
}

#[test]
fn the_readout_says_when_it_is_not_now() {
    let Some(mut app) = with_stations() else {
        return;
    };

    let at_now = app.time_state();
    assert!(at_now.at_now, "a fresh app should be showing now");
    assert!(
        at_now.away.is_empty(),
        "at now there is nothing to say about the offset, got {:?}",
        at_now.away
    );

    slide(&mut app, 3);
    let moved = app.time_state();
    assert!(
        !moved.at_now,
        "the slider moved but the panel says it is now"
    );
    assert!(
        moved.away.contains("3h"),
        "the offset reads {:?}",
        moved.away
    );
    assert_ne!(
        moved.shown, at_now.shown,
        "the clock readout did not change"
    );
}

#[test]
fn the_slider_is_clamped_to_what_it_offers() {
    let Some(mut app) = with_stations() else {
        return;
    };
    let span = app.time_state();

    // A week out, when the slider only offers hours. Clamped rather than
    // accepted: the panel would otherwise show a handle pinned to one end
    // while the chart was somewhere else entirely.
    app.set_time_offset(7 * 86_400);
    let after = app.time_state();
    assert_eq!(after.offset_secs, span.max_secs);

    app.set_time_offset(-7 * 86_400);
    assert_eq!(app.time_state().offset_secs, span.min_secs);
}

#[test]
fn the_boat_does_not_move_with_the_slider() {
    // The one layer the slider must never touch. There is no GPS in this
    // test, so the check is that the vessel state is unaffected — a
    // future version that interpolated a position forward along the
    // course would fail here, which is the intent.
    let Some(mut app) = with_stations() else {
        return;
    };
    let before = app.vessel_state();
    slide(&mut app, 6);
    let after = app.vessel_state();
    assert_eq!(
        before, after,
        "the slider changed what the panel says about the boat"
    );
}
