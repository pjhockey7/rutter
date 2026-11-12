//! The GPS path, end to end, against the synthetic feed.
//!
//! The unit tests in `rutter_app::nmea` send sentences this file wrote, so
//! they prove the reader agrees with itself. This one runs
//! `scripts/nmea-feed.py` — the thing actually used to look at the boat on
//! screen — and checks the app tracks it. If the simulator and the parser
//! ever disagree about where a boat is, every manual check done with the
//! simulator afterwards is worthless, which is exactly the kind of error
//! that hides for months.
//!
//! The feed corrupts one sentence in forty on purpose, so this also checks
//! the thing most worth checking: that a feed with bad bytes in it still
//! produces a boat, and that the bad bytes are refused rather than
//! believed.

// A panic here *is* the failure report, as in every other test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rutter_app::nmea::{Endpoint, Link, Receiver};

/// The simulator's default starting position: the entrance to San Diego
/// Bay, which is also where the charts under development are.
const START: (f64, f64) = (32.68, -117.23);

/// A child process that is killed when it goes out of scope.
///
/// Without this a failing assertion leaves a Python process sending
/// datagrams into the void for the rest of the session, and a second run
/// then fails to bind the port it is still holding.
struct Feed(Child);

impl Drop for Feed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn have_python() -> bool {
    Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Wait for something to become true of the feed, polling it.
fn wait_for(nmea: &mut Receiver, what: &str, mut done: impl FnMut(&Receiver) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        nmea.poll();
        if done(nmea) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "timed out waiting for {what}; link is {}, {} read / {} refused",
        nmea.status(),
        nmea.report().accepted,
        nmea.report().rejected
    );
}

#[test]
fn the_simulator_and_the_parser_agree_about_where_the_boat_is() {
    if !have_python() {
        eprintln!("no python3; skipping the synthetic feed test");
        return;
    }

    // UDP and port 0 would be ideal, but the simulator sends rather than
    // listens, so it has to be told a port. Bound and released first to
    // find one that is free, which is the same trick the unit tests use.
    let port = {
        let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("probe");
        probe.local_addr().expect("addr").port()
    };

    let mut nmea =
        Receiver::spawn(Endpoint::parse(&format!("udp://127.0.0.1:{port}")).expect("endpoint"))
            .expect("reader thread");
    wait_for(&mut nmea, "the socket to bind", |n| {
        matches!(n.report().link, Link::Open(_))
    });

    let _feed = Feed(
        Command::new("python3")
            .args([
                "scripts/nmea-feed.py",
                "--port",
                &port.to_string(),
                "--course",
                "210",
                "--speed",
                "6.0",
            ])
            .current_dir(repository_root())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the feed"),
    );

    wait_for(&mut nmea, "a fix", |n| n.vessel().position.is_some());

    let vessel = *nmea.vessel();
    let fix = vessel.position.expect("a fix").value;

    // Where the simulator says it starts. A degrees-and-minutes bug on
    // either side shows here as roughly a tenth of a degree, so the
    // tolerance is tight enough to catch one and loose enough to allow the
    // boat having moved a few seconds' worth.
    assert!(
        (fix.lat - START.0).abs() < 0.01 && (fix.lon - START.1).abs() < 0.01,
        "the boat is at {fix:?}, not {START:?}"
    );

    // Everything a full instrument feed carries, which is the point of
    // reading NMEA rather than only a phone's position.
    assert!(
        vessel.sog_kn.is_some_and(|s| (s.value - 6.0).abs() < 0.2),
        "speed: {:?}",
        vessel.sog_kn
    );
    assert!(vessel.cog_deg.is_some(), "no course");
    assert!(vessel.heading_deg.is_some(), "no compass heading");
    assert!(vessel.depth_m.is_some(), "no depth");
    assert!(vessel.apparent_wind.is_some(), "no wind");
    assert_eq!(vessel.fix_quality, 2, "the feed states a differential fix");
    assert_eq!(vessel.satellites, Some(11));

    // Heading and course over ground must not be the same number: the
    // simulator crabs four degrees, and a reader that filled both from the
    // same sentence would pass every other assertion here.
    let (cog, hdg) = (
        vessel.cog_deg.expect("course").value,
        vessel.heading_deg.expect("heading").value,
    );
    let gap = (cog - hdg).abs() % 360.0;
    assert!(
        gap.min(360.0 - gap) > 1.0,
        "course {cog} and heading {hdg} are the same number"
    );

    // And the boat has to actually move. A reader that latched the first
    // fix and never updated would satisfy everything above.
    let was = fix;
    wait_for(&mut nmea, "the boat to move", |n| {
        n.vessel()
            .position
            .is_some_and(|p| p.value.distance_to(was) > 5.0)
    });

    // Six knots on 210 is south and west. Checked because a sign error in
    // either hemisphere is the one position bug that looks plausible.
    let now = nmea.vessel().position.expect("a fix").value;
    assert!(now.lat < was.lat, "steering south but the latitude rose");
    assert!(now.lon < was.lon, "steering west but the longitude rose");
}

#[test]
fn a_feed_with_corrupt_sentences_still_produces_a_boat() {
    if !have_python() {
        eprintln!("no python3; skipping the corrupt feed test");
        return;
    }

    let port = {
        let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("probe");
        probe.local_addr().expect("addr").port()
    };

    let mut nmea =
        Receiver::spawn(Endpoint::parse(&format!("udp://127.0.0.1:{port}")).expect("endpoint"))
            .expect("reader thread");
    wait_for(&mut nmea, "the socket to bind", |n| {
        matches!(n.report().link, Link::Open(_))
    });

    // Every sentence mangled after its checksum was computed: the shape of
    // a dropped bit on an RS-422 run through a bilge. Nothing should be
    // believed and nothing should crash.
    let _feed = Feed(
        Command::new("python3")
            .args([
                "scripts/nmea-feed.py",
                "--port",
                &port.to_string(),
                "--corrupt-every",
                "1",
            ])
            .current_dir(repository_root())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the feed"),
    );

    wait_for(&mut nmea, "sentences to be refused", |n| {
        n.report().rejected >= 6
    });
    assert!(
        nmea.vessel().position.is_none(),
        "a corrupt feed produced a position: {:?}",
        nmea.vessel().position
    );
    assert_eq!(
        nmea.report().accepted,
        0,
        "something in an all-corrupt feed was accepted"
    );
}

/// The repository root, so the scripts directory can be found however the
/// test was invoked.
fn repository_root() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR is this crate's directory, which is one level in.
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf()
}
