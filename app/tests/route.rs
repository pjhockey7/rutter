//! A route, followed by a boat, through the panel the interface reads.
//!
//! `rutter_core::route` has the arithmetic and tests it thoroughly. What
//! this checks is the wiring: that a route drawn by hand and a position
//! arriving over NMEA produce the three numbers the whole feature exists
//! for — a course to steer, how far off the line you are and which way to
//! correct, and when you will get there.
//!
//! The way this goes wrong is not a wrong number. It is a number that is
//! simply absent, or stale, or computed against the wrong leg, with
//! everything still rendering perfectly.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use rutter_app::{App, Position, Sources};

/// Shelter Island, the bay entrance, and Avalon: the passage this was
/// written for.
const SHELTER_ISLAND: (f64, f64) = (32.7100, -117.2300);
const BAY_ENTRANCE: (f64, f64) = (32.6700, -117.2350);
const AVALON: (f64, f64) = (33.3450, -118.3250);

fn bare_app() -> App {
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
            data_dir: None,
        },
        None,
    )
}

/// An app listening on a free UDP port, and the address to feed it.
fn app_with_gps() -> (App, std::net::SocketAddr) {
    let port = {
        let probe = UdpSocket::bind("127.0.0.1:0").expect("probe");
        probe.local_addr().expect("addr").port()
    };
    let app = App::new(
        Sources {
            charts: None,
            imagery: None,
            tides: None,
            weather: None,
            nmea: Some(
                rutter_app::NmeaEndpoint::parse(&format!("udp://127.0.0.1:{port}"))
                    .expect("endpoint"),
            ),
            gps: None,
            fetcher: None,
            imagery_out: None,
            siren: None,
            keepalive: None,
            data_dir: None,
        },
        None,
    );
    (
        app,
        format!("127.0.0.1:{port}").parse().expect("an address"),
    )
}

/// Build an RMC sentence with a correct checksum.
///
/// Written here rather than taken from a fixture because the position has
/// to be chosen per test, and because degrees-and-decimal-minutes is the
/// one encoding in NMEA that is easy to get wrong in a way that still
/// parses.
fn rmc(lat: f64, lon: f64, sog_kn: f64, cog_deg: f64) -> String {
    let dm = |value: f64, positive: char, negative: char| {
        let hemisphere = if value >= 0.0 { positive } else { negative };
        let magnitude = value.abs();
        let degrees = magnitude.trunc();
        let minutes = (magnitude - degrees) * 60.0;
        (format!("{degrees:.0}{minutes:07.4}"), hemisphere)
    };
    let (lat_text, ns) = dm(lat, 'N', 'S');
    let (lon_text, ew) = dm(lon, 'E', 'W');
    let body = format!(
        "GPRMC,123519,A,{lat_text},{ns},0{lon_text},{ew},{sog_kn:05.1},{cog_deg:05.1},230394,,"
    );
    let checksum = body.bytes().fold(0u8, |acc, b| acc ^ b);
    format!("${body}*{checksum:02X}\r\n")
}

/// Feed one position in and wait for the app to take *that* position.
///
/// Waiting for "a fix exists" is not enough and was wrong here first time:
/// the second call returned immediately on the fix from the first, so the
/// test moved the boat and then measured the old place. Everything still
/// rendered; the assertion simply described somewhere the boat had left.
fn send_fix(app: &mut App, to: std::net::SocketAddr, lat: f64, lon: f64, sog: f64, cog: f64) {
    let sender = UdpSocket::bind("127.0.0.1:0").expect("sender");
    let sentence = rmc(lat, lon, sog, cog);

    // What the panel will read once this position has landed. Compared as
    // the formatted string because that is what the app exposes without a
    // renderer, and it is to a hundredth of a minute — far finer than any
    // distance these tests move the boat.
    let wanted = rutter_app::format_position(Position::new(lat, lon));

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        sender
            .send_to(sentence.as_bytes(), to)
            .expect("send the fix");
        app.poll_nmea();
        if app.vessel_state().fix.as_deref() == Some(wanted.as_str()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "the app never took {wanted}: it has {:?} on {}",
        app.vessel_state().fix,
        app.vessel_state().link
    );
}

#[test]
fn a_route_is_a_plan_whether_or_not_anyone_is_following_it() {
    // Drawn and edited with no GPS at all, which is how a passage is
    // planned the night before.
    let mut app = bare_app();

    let empty = app.route_state();
    assert!(empty.waypoints.is_empty());
    assert!(empty.length.is_none(), "no legs, no length");
    assert!(empty.to_waypoint.is_none(), "nothing to follow yet");

    app.add_waypoint(Position::new(SHELTER_ISLAND.0, SHELTER_ISLAND.1));
    app.add_waypoint(Position::new(BAY_ENTRANCE.0, BAY_ENTRANCE.1));
    app.add_waypoint(Position::new(AVALON.0, AVALON.1));

    let state = app.route_state();
    assert_eq!(state.waypoints.len(), 3);
    assert_eq!(state.waypoints[0].name, "001");

    // The first waypoint has no leg into it; the others do, with a course
    // and a distance.
    assert!(
        state.waypoints[0].leg.is_empty(),
        "the first waypoint cannot have a leg before it"
    );
    assert!(
        state.waypoints[1].leg.contains('°') && state.waypoints[1].leg.contains("nm"),
        "leg reads {:?}",
        state.waypoints[1].leg
    );

    let length = state.length.expect("a length");
    assert!(length.contains("nm"), "{length:?}");

    // Editing. Removing the middle one must leave a two-waypoint route
    // rather than a corrupt one.
    app.remove_waypoint(1);
    let state = app.route_state();
    assert_eq!(state.waypoints.len(), 2);
    assert_eq!(state.waypoints[1].name, "003", "names are not renumbered");

    // Out of range is ignored: the panel builds its list a frame before
    // the click arrives, so the index can be stale.
    app.remove_waypoint(99);
    assert_eq!(app.route_state().waypoints.len(), 2);

    app.clear_route();
    assert!(app.route_state().waypoints.is_empty());
}

#[test]
fn one_waypoint_is_a_destination_not_a_passage() {
    let mut app = bare_app();
    app.add_waypoint(Position::new(AVALON.0, AVALON.1));

    let state = app.route_state();
    assert_eq!(state.waypoints.len(), 1);
    assert!(
        state.length.is_none(),
        "one waypoint has no legs to measure"
    );
    assert!(state.waypoints[0].leg.is_empty());
}

#[test]
fn following_a_route_gives_a_course_an_offset_and_an_arrival() {
    let (mut app, to) = app_with_gps();
    app.add_waypoint(Position::new(SHELTER_ISLAND.0, SHELTER_ISLAND.1));
    app.add_waypoint(Position::new(BAY_ENTRANCE.0, BAY_ENTRANCE.1));
    app.add_waypoint(Position::new(AVALON.0, AVALON.1));

    // On the first leg, a little east of the line, making six knots south.
    send_fix(&mut app, to, 32.6900, -117.2320, 6.0, 190.0);

    let state = app.route_state();

    let to_waypoint = state.to_waypoint.expect("a course and distance");
    assert!(
        to_waypoint.contains('°') && to_waypoint.contains("nm"),
        "{to_waypoint:?}"
    );
    // Named, so there is no doubt which mark the numbers refer to.
    assert!(to_waypoint.starts_with("002"), "{to_waypoint:?}");

    let cross_track = state.cross_track.expect("a cross-track");
    assert!(
        cross_track.contains("off track") || cross_track == "on the line",
        "{cross_track:?}"
    );

    let made_good = state.made_good.expect("speed made good");
    assert!(made_good.contains("kn made good"), "{made_good:?}");

    let eta = state.eta.expect("an arrival");
    assert!(eta.contains("to run"), "{eta:?}");
    assert!(
        eta.contains('Z'),
        "the arrival should carry its zone: {eta:?}"
    );
}

#[test]
fn the_correction_points_the_opposite_way_to_the_error() {
    // The one that matters most and is easiest to get backwards. Getting
    // it the wrong way round doubles the error rather than closing it, and
    // the number still looks perfectly reasonable on screen.
    //
    // A leg running due south, so east of track is unambiguous.
    let (mut app, to) = app_with_gps();
    app.add_waypoint(Position::new(33.0, -117.0));
    app.add_waypoint(Position::new(32.0, -117.0));

    // East of a southbound track is *left* of it — so the correction is
    // to starboard.
    send_fix(&mut app, to, 32.5, -116.9, 6.0, 180.0);
    let east = app.route_state();
    let correction = east.correction.clone().expect("a correction");
    assert_eq!(
        correction, "steer right",
        "east of a southbound leg: {:?} / {:?}",
        east.cross_track, correction
    );

    // And west of it is the other way.
    send_fix(&mut app, to, 32.5, -117.1, 6.0, 180.0);
    let west = app.route_state();
    assert_eq!(
        west.correction.clone().expect("a correction"),
        "steer left",
        "west of a southbound leg: {:?}",
        west.cross_track
    );
}

#[test]
fn on_the_line_asks_for_no_correction() {
    let (mut app, to) = app_with_gps();
    app.add_waypoint(Position::new(33.0, -117.0));
    app.add_waypoint(Position::new(32.0, -117.0));

    send_fix(&mut app, to, 32.5, -117.0, 6.0, 180.0);
    let state = app.route_state();
    assert_eq!(state.cross_track.as_deref(), Some("on the line"));
    assert!(
        state.correction.is_none(),
        "a correction was offered for being on the line: {:?}",
        state.correction
    );
}

#[test]
fn choosing_a_waypoint_by_hand_sticks() {
    // A plotter that reselects the leg every second hops between legs
    // where a passage doubles back, and the course to steer then
    // alternates between two answers a hundred degrees apart. Being told
    // has to override being near.
    let (mut app, to) = app_with_gps();
    app.add_waypoint(Position::new(SHELTER_ISLAND.0, SHELTER_ISLAND.1));
    app.add_waypoint(Position::new(BAY_ENTRANCE.0, BAY_ENTRANCE.1));
    app.add_waypoint(Position::new(AVALON.0, AVALON.1));

    send_fix(&mut app, to, 32.6900, -117.2320, 6.0, 190.0);
    let chosen_for_us = app.route_state();
    assert!(
        !chosen_for_us.leg_chosen_by_hand,
        "nothing was chosen by hand yet"
    );
    let automatic = chosen_for_us
        .waypoints
        .iter()
        .position(|w| w.active)
        .expect("something active");

    // Skip ahead to Avalon.
    app.apply_route_selection(2);
    let by_hand = app.route_state();
    assert!(by_hand.leg_chosen_by_hand);
    assert!(by_hand.waypoints[2].active, "Avalon should be active");
    assert_ne!(automatic, 2, "the test needs the choice to differ");

    // The numbers follow the choice, not the position.
    let to_waypoint = by_hand.to_waypoint.expect("a course");
    assert!(to_waypoint.starts_with("003"), "{to_waypoint:?}");

    // And it stays chosen across further fixes, which is the whole point.
    send_fix(&mut app, to, 32.6890, -117.2325, 6.0, 190.0);
    assert!(app.route_state().waypoints[2].active, "the choice was lost");

    // Handing it back works.
    app.follow_nearest_leg();
    let back = app.route_state();
    assert!(!back.leg_chosen_by_hand);
    assert!(back.waypoints[automatic].active);
}

#[test]
fn editing_the_route_releases_a_hand_chosen_leg() {
    // An index names a waypoint. Delete one and the index names a
    // different waypoint, so the plotter would quietly start steering for
    // somewhere nobody asked for.
    let (mut app, to) = app_with_gps();
    for at in [SHELTER_ISLAND, BAY_ENTRANCE, AVALON] {
        app.add_waypoint(Position::new(at.0, at.1));
    }
    send_fix(&mut app, to, 32.6900, -117.2320, 6.0, 190.0);

    app.apply_route_selection(2);
    assert!(app.route_state().leg_chosen_by_hand);

    app.remove_waypoint(0);
    assert!(
        !app.route_state().leg_chosen_by_hand,
        "the hand-chosen leg survived an edit that renumbered the waypoints"
    );
}

#[test]
fn a_stale_fix_stops_the_route_being_followed() {
    // The route stays drawn and editable, but the numbers about following
    // it have to go. A cross-track from a position minutes old is worse
    // than none: it looks live and is not.
    let (mut app, to) = app_with_gps();
    app.add_waypoint(Position::new(33.0, -117.0));
    app.add_waypoint(Position::new(32.0, -117.0));
    send_fix(&mut app, to, 32.5, -116.9, 6.0, 180.0);

    assert!(app.route_state().to_waypoint.is_some());

    // The fix is stamped with its arrival, so there is no way to age it
    // from here without waiting. What can be checked is that the route
    // itself survives losing the feed entirely.
    app.apply_disconnect_nmea();
    let state = app.route_state();
    assert_eq!(state.waypoints.len(), 2, "the route was lost with the feed");
    assert!(
        state.to_waypoint.is_none(),
        "still claiming to follow a route with no GPS: {:?}",
        state.to_waypoint
    );
    assert!(state.cross_track.is_none());
    assert!(state.length.is_some(), "the plan is still a plan");
}
