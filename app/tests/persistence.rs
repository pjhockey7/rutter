//! That a plan survives closing the app.
//!
//! The thing that made this urgent: you draw a passage the night before
//! and sail it in the morning, and until this existed the route died with
//! the process. So what is checked here is the round trip through a real
//! directory on disk — build an app, change things, drop it, build another
//! pointing at the same place, and see what came back.
//!
//! The failure mode is quiet. Nothing errors when settings are not saved;
//! the app simply comes up with the defaults and you notice your draft is
//! wrong when the shading looks odd, or you notice the route is gone when
//! you go looking for it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rutter_app::{App, Position, Sources};

/// A directory of this test's own, emptied first.
fn scratch(name: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!("rutter-persistence-{name}"));
    let _ = std::fs::remove_dir_all(&directory);
    directory
}

fn app_in(directory: &std::path::Path) -> App {
    App::new(
        Sources {
            charts: None,
            imagery: None,
            tides: None,
            weather: None,
            nmea: None,
            gps: None,
            fetcher: None,
            imagery_out: None,
            siren: None,
            keepalive: None,
            data_dir: Some(directory.to_path_buf()),
        },
        None,
    )
}

/// `App::refresh` is what saves, and it needs a renderer. The save itself
/// does not, so this reaches the same code by the same route a frame does.
fn save(app: &mut App) {
    app.save_for_tests();
}

#[test]
fn a_planned_passage_is_there_in_the_morning() {
    let directory = scratch("route");

    {
        let mut app = app_in(&directory);
        assert!(
            app.route_state().waypoints.is_empty(),
            "a fresh directory should have no route in it"
        );

        app.add_waypoint(Position::new(32.7100, -117.2300));
        app.add_waypoint(Position::new(32.6700, -117.2350));
        app.add_waypoint(Position::new(33.3450, -118.3250));
        save(&mut app);
    }

    let app = app_in(&directory);
    let state = app.route_state();
    assert_eq!(state.waypoints.len(), 3, "the passage did not come back");
    assert_eq!(state.waypoints[0].name, "001");
    // The positions, not just the count: a route that came back with the
    // right number of waypoints in the wrong places would be worse than
    // one that came back empty.
    assert!(
        state.waypoints[0].at.contains("32°42.60'N"),
        "first waypoint reads {:?}",
        state.waypoints[0].at
    );
    assert!(
        state.waypoints[2].at.contains("118°19.50'W"),
        "last waypoint reads {:?}",
        state.waypoints[2].at
    );
    assert!(state.length.is_some(), "no legs came back");

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn it_is_written_as_gpx_another_plotter_could_read() {
    // The whole reason for not inventing a format. If this stops being
    // GPX, a route can no longer be handed to anyone.
    let directory = scratch("gpx");
    {
        let mut app = app_in(&directory);
        app.add_waypoint(Position::new(32.71, -117.23));
        app.add_waypoint(Position::new(33.345, -118.325));
        save(&mut app);
    }

    let text = std::fs::read_to_string(directory.join("route.gpx")).expect("a route file");
    assert!(text.starts_with("<?xml"), "{text}");
    assert!(text.contains("http://www.topografix.com/GPX/1/1"), "{text}");
    assert_eq!(text.matches("<rtept").count(), 2);
    // The sign has to survive. A longitude that lost its minus puts San
    // Diego in China.
    assert!(text.contains("lon=\"-117.230000\""), "{text}");

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn the_draft_and_the_layer_choices_come_back() {
    let directory = scratch("settings");

    {
        let mut app = app_in(&directory);
        app.set_draft(rutter_core::depth::Draft {
            draft_m: 2.35,
            safety_m: 0.4,
        });
        app.set_draft_check(true);
        app.toggle_imagery(); // on by default, so this turns it off
        save(&mut app);
    }

    let app = app_in(&directory);
    let draft = app.draft_for_tests();
    assert!(
        (draft.draft_m - 2.35).abs() < 1e-6,
        "the draft came back as {}",
        draft.draft_m
    );
    assert!((draft.safety_m - 0.4).abs() < 1e-6);
    assert!(app.draft_check_for_tests(), "the under-keel check was lost");
    assert!(
        !app.imagery_visible_for_tests(),
        "the imagery came back visible after being switched off"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn an_empty_route_leaves_no_file_to_load() {
    // Clearing a route has to clear it. A leftover file would mean the
    // route came back from the dead on the next launch.
    let directory = scratch("cleared");
    {
        let mut app = app_in(&directory);
        app.add_waypoint(Position::new(32.71, -117.23));
        save(&mut app);
        assert!(directory.join("route.gpx").exists());

        app.clear_route();
        save(&mut app);
    }

    assert!(
        !directory.join("route.gpx").exists(),
        "the route file outlived the route"
    );
    assert!(app_in(&directory).route_state().waypoints.is_empty());

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn nothing_is_rewritten_when_nothing_changed() {
    // Saving runs every frame. If it wrote unconditionally that would be
    // sixty file writes a second on a phone, for files whose whole point
    // is that they persist rather than that they are fresh.
    let directory = scratch("idle");
    let mut app = app_in(&directory);
    app.add_waypoint(Position::new(32.71, -117.23));
    save(&mut app);

    let path = directory.join("route.gpx");
    let first = std::fs::metadata(&path).expect("metadata").modified().ok();

    // Long enough that a rewrite would show a different timestamp on any
    // filesystem worth the name.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    for _ in 0..20 {
        save(&mut app);
    }

    let second = std::fs::metadata(&path).expect("metadata").modified().ok();
    assert_eq!(first, second, "an unchanged route was written again");

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_damaged_settings_file_does_not_stop_the_app() {
    // One mistyped number should not cost you the chart.
    let directory = scratch("damaged");
    std::fs::create_dir_all(&directory).expect("create");
    std::fs::write(
        directory.join("settings.conf"),
        "draft_m = banana\nthis line has no equals\n\0\n",
    )
    .expect("write a bad file");
    std::fs::write(directory.join("route.gpx"), "not xml at all").expect("write a bad route");

    let app = app_in(&directory);
    // Fell back rather than to zero: a draft of zero would quietly
    // declare the boat able to cross anything.
    assert!(
        app.draft_for_tests().draft_m > 0.5,
        "the draft came back as {}",
        app.draft_for_tests().draft_m
    );
    assert!(app.route_state().waypoints.is_empty());

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn no_data_directory_means_no_memory_and_no_complaint() {
    // A sandboxed platform handed nowhere to write still has to chart.
    let mut app = App::new(
        Sources {
            charts: None,
            imagery: None,
            tides: None,
            weather: None,
            nmea: None,
            gps: None,
            fetcher: None,
            imagery_out: None,
            siren: None,
            keepalive: None,
            data_dir: None,
        },
        None,
    );
    app.add_waypoint(Position::new(32.71, -117.23));
    save(&mut app);
    assert_eq!(
        app.route_state().waypoints.len(),
        1,
        "the route still works"
    );
}

#[test]
fn an_explicit_draft_beats_a_saved_one() {
    // The first version applied the settings *after* the command-line
    // flag, so `--draft 2.6` was silently discarded and last week's saved
    // value quietly stood. Nothing errored; the shading was simply
    // answering a question about a different boat.
    let directory = scratch("precedence");

    {
        let mut app = app_in(&directory);
        app.set_draft(rutter_core::depth::Draft {
            draft_m: 1.2,
            safety_m: 0.5,
        });
        save(&mut app);
    }

    // Reopened with a flag: the flag wins.
    let with_flag = App::new(
        Sources {
            charts: None,
            imagery: None,
            tides: None,
            weather: None,
            nmea: None,
            gps: None,
            fetcher: None,
            imagery_out: None,
            siren: None,
            keepalive: None,
            data_dir: Some(directory.clone()),
        },
        Some(2.6),
    );
    assert!(
        (with_flag.draft_for_tests().draft_m - 2.6).abs() < 1e-6,
        "the flag was discarded: draft is {}",
        with_flag.draft_for_tests().draft_m
    );
    assert!(
        with_flag.draft_check_for_tests(),
        "giving a draft should switch the check on"
    );

    // And without one, the saved value stands.
    let without = app_in(&directory);
    assert!(
        (without.draft_for_tests().draft_m - 1.2).abs() < 1e-6,
        "the saved draft was lost: {}",
        without.draft_for_tests().draft_m
    );

    let _ = std::fs::remove_dir_all(&directory);
}
