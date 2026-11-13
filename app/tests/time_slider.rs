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
            fetcher: None,
            imagery_out: None,
            siren: None,
            keepalive: None,
            data_dir: None,
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

// ── Scoped time, for a boat with more than one screen ───────────────────

#[test]
fn an_unlinked_screen_keeps_its_own_time() {
    // The point of the whole scope mechanism: one display scrubbed forward
    // for planning while the one you steer by stays where you put it.
    let Some(mut app) = with_stations() else {
        return;
    };
    assert!(app.time_linked(), "screens should start linked");

    // Unlinking must not move the chart — it only stops it moving with the
    // others. A screen that jumped when you unlinked it would be useless
    // for the thing unlinking is for.
    slide(&mut app, 4);
    let before = app.time_state().offset_secs;
    app.set_time_linked(false);
    assert!(!app.time_linked());
    assert_eq!(
        app.time_state().offset_secs,
        before,
        "unlinking moved the time"
    );

    // And it still takes its own slider.
    app.set_time_offset(2 * 3600);
    assert_eq!(app.time_state().offset_secs, 2 * 3600);

    // Relinking picks the shared offset back up, which is still where the
    // linked screens left it.
    app.set_time_linked(true);
    assert_eq!(
        app.time_state().offset_secs,
        before,
        "relinking did not return to the shared clock"
    );
}

#[test]
fn now_works_in_either_scope() {
    // Whichever scope a screen is in, the Now button has to get it back to
    // the present — that is the one control that must never be subtle.
    let Some(mut app) = with_stations() else {
        return;
    };
    for linked in [true, false] {
        app.set_time_linked(linked);
        app.set_time_offset(5 * 3600);
        assert_ne!(app.time_state().offset_secs, 0);
        app.set_time_offset(0);
        assert_eq!(
            app.time_state().offset_secs,
            0,
            "Now failed with linked = {linked}"
        );
        assert!(app.time_state().at_now);
    }
}

#[test]
fn nudging_steps_from_wherever_the_screen_is() {
    let Some(mut app) = with_stations() else {
        return;
    };
    app.set_time_linked(false);
    app.set_time_offset(3600);
    app.nudge_time(1800);
    assert_eq!(app.time_state().offset_secs, 5400);
    app.nudge_time(-5400);
    assert_eq!(app.time_state().offset_secs, 0);
}

// ── The download area ───────────────────────────────────────────────────

#[test]
fn marking_the_view_describes_what_it_would_cost() {
    let Some(mut app) = with_stations() else {
        return;
    };
    look_at(&mut app, 32.68, -117.23);

    // Nothing marked: no area, no estimate, and nothing to run.
    let empty = app.download_state();
    assert!(empty.area.is_none());
    assert!(empty.commands.is_empty());
    assert!(
        !empty.in_app,
        "the panel should not claim it can fetch this itself yet"
    );

    app.mark_view_for_download();
    let marked = app.download_state();

    let area = marked.area.expect("an area");
    assert!(area.contains("32."), "the area reads {area:?}");
    assert!(area.contains("-117."), "the area reads {area:?}");
    assert!(marked.span.is_some(), "no span");

    // Both estimates, because imagery and vector tiles differ by a factor
    // of twenty in size and the chart stops at a lower zoom.
    let imagery = marked.imagery_estimate.expect("an imagery estimate");
    let chart = marked.chart_estimate.expect("a chart estimate");
    assert!(imagery.contains("tiles"), "{imagery:?}");
    assert!(chart.contains("tiles"), "{chart:?}");

    // A vector chart holds a fixed amount of detail, so tiling it as deep
    // as aerial imagery subdivides the same lines and inflates the
    // estimate several-fold. With no chart open the fallback has to be an
    // ENC's ceiling, not the imagery's.
    assert!(
        !chart.contains("z16"),
        "the chart estimate claims imagery-depth detail: {chart:?}"
    );

    // The commands have to carry the box, or they are decoration.
    assert!(!marked.commands.is_empty());
    for command in &marked.commands {
        assert!(command.contains("--box"), "{command:?}");
        assert!(command.starts_with("rutter "), "{command:?}");
    }

    // Printed as well as asserted: the estimate rests on an average tile
    // size, and a wrong constant would pass every assertion here while
    // telling someone 200 MB before downloading two gigabytes onto a
    // phone. Worth being able to read the actual numbers.
    println!("  area    {area}");
    println!("  span    {}", marked.span.unwrap_or_default());
    println!("  {imagery}");
    println!("  {chart}");

    app.clear_download_area();
    assert!(app.download_state().area.is_none());
}

#[test]
fn an_extra_zoom_level_roughly_quadruples_the_job() {
    // The property that makes the estimate worth showing at all: the
    // finest level in the range is most of the download. Someone who
    // drags the slider from 14 to 16 should be told it is sixteen times
    // the work, not left to find out.
    let Some(mut app) = with_stations() else {
        return;
    };
    look_at(&mut app, 32.68, -117.23);
    app.mark_view_for_download();

    app.set_download_zoom(10, 12);
    let shallow = app.download_state();
    app.set_download_zoom(10, 13);
    let deeper = app.download_state();

    let count = |s: &str| -> u64 {
        s.chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0)
    };
    // Parsed back out of the sentence, which also checks the thousands
    // separator has not mangled it.
    let a = count(
        shallow
            .imagery_estimate
            .as_deref()
            .unwrap_or("")
            .split(" tiles")
            .next()
            .unwrap_or(""),
    );
    let b = count(
        deeper
            .imagery_estimate
            .as_deref()
            .unwrap_or("")
            .split(" tiles")
            .next()
            .unwrap_or(""),
    );
    assert!(a > 0 && b > a, "{a} tiles then {b}");
    assert!(
        b > a * 2,
        "one more zoom level only went from {a} to {b} tiles"
    );
}

#[test]
fn the_zoom_range_cannot_be_inverted() {
    let Some(mut app) = with_stations() else {
        return;
    };
    look_at(&mut app, 32.68, -117.23);
    app.mark_view_for_download();

    // Dragging the coarse slider past the fine one pushes rather than
    // inverts, or the tile count would come out as zero and the panel
    // would promise a free download.
    app.set_download_zoom(14, 8);
    let state = app.download_state();
    assert!(
        state.min_zoom <= state.max_zoom,
        "range is {}..{}",
        state.min_zoom,
        state.max_zoom
    );
    assert!(
        state
            .imagery_estimate
            .is_some_and(|e| !e.contains(" 0 tiles")),
        "an inverted range produced an empty download"
    );
}
