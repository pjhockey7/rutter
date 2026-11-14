//! The alarms, from a GPS feed through to the noise.
//!
//! `rutter_core::alarm` tests the conditions thoroughly. What this checks
//! is that a position arriving over NMEA actually reaches the watch, that
//! the siren is asked to sound, and that the panel says what is wrong —
//! and, as importantly, that a platform which cannot make a noise says so
//! rather than failing silently.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::UdpSocket;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rutter_app::keepalive::Keepalive;
use rutter_app::siren::{Siren, Urgency};
use rutter_app::{App, Position, Sources};

/// A siren that counts instead of sounding.
#[derive(Debug)]
struct Counter {
    sounded: AtomicUsize,
    available: bool,
}

impl Counter {
    fn new(available: bool) -> Arc<Self> {
        Arc::new(Self {
            sounded: AtomicUsize::new(0),
            available,
        })
    }
}

impl Siren for Counter {
    fn sound(&self, _urgency: Urgency) {
        self.sounded.fetch_add(1, Ordering::Relaxed);
    }
    fn available(&self) -> bool {
        self.available
    }
    fn describe(&self) -> String {
        "counter".to_owned()
    }
}

/// A keepalive that records what it was asked for.
///
/// Stands in for the Android foreground service, which cannot be run
/// here. What it checks is the thing that is actually ours: that the hold
/// is taken when a watch is armed, re-announced when what is being
/// watched changes, and dropped when nothing is armed — because a
/// notification that outlives the watch is one people learn to ignore.
#[derive(Debug, Default)]
struct Log {
    calls: std::sync::Mutex<Vec<String>>,
}

impl Log {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("the log").clone()
    }
}

impl Keepalive for Log {
    fn hold(&self, reason: &str) {
        self.calls
            .lock()
            .expect("the log")
            .push(format!("hold: {reason}"));
    }
    fn release(&self) {
        self.calls
            .lock()
            .expect("the log")
            .push("release".to_owned());
    }
    fn describe(&self) -> String {
        "a log".to_owned()
    }
}

fn app_with(siren: Option<Arc<dyn Siren>>) -> (App, std::net::SocketAddr) {
    let (app, to, _) = app_with_both(siren, None);
    (app, to)
}

fn app_with_both(
    siren: Option<Arc<dyn Siren>>,
    keepalive: Option<Arc<dyn Keepalive>>,
) -> (App, std::net::SocketAddr, ()) {
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
            siren,
            keepalive,
            zone: None,
            data_dir: None,
        },
        None,
    );
    (
        app,
        format!("127.0.0.1:{port}").parse().expect("an address"),
        (),
    )
}

/// An RMC with a correct checksum, and a DBT if a depth is wanted.
fn sentences(lat: f64, lon: f64, depth_m: Option<f64>) -> String {
    let dm = |value: f64, positive: char, negative: char, width: usize| {
        let hemisphere = if value >= 0.0 { positive } else { negative };
        let magnitude = value.abs();
        let degrees = magnitude.trunc();
        let minutes = (magnitude - degrees) * 60.0;
        (
            format!("{:0width$.0}{minutes:07.4}", degrees, width = width),
            hemisphere,
        )
    };
    let (lat_text, ns) = dm(lat, 'N', 'S', 2);
    let (lon_text, ew) = dm(lon, 'E', 'W', 3);

    let with_checksum = |body: String| {
        let sum = body.bytes().fold(0u8, |acc, b| acc ^ b);
        format!("${body}*{sum:02X}\r\n")
    };

    let mut out = with_checksum(format!(
        "GPRMC,123519,A,{lat_text},{ns},{lon_text},{ew},000.5,180.0,230394,,"
    ));
    if let Some(metres) = depth_m {
        out.push_str(&with_checksum(format!(
            "SDDBT,{:.1},f,{metres:.1},M,{:.1},F",
            metres / 0.3048,
            metres / 1.8288
        )));
    }
    out
}

/// Feed one reading repeatedly until a condition holds, or give up.
fn feed_until(
    app: &mut App,
    to: std::net::SocketAddr,
    lat: f64,
    lon: f64,
    depth_m: Option<f64>,
    seconds: u64,
    mut done: impl FnMut(&App) -> bool,
) -> bool {
    let sender = UdpSocket::bind("127.0.0.1:0").expect("sender");
    let text = sentences(lat, lon, depth_m);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        let _ = sender.send_to(text.as_bytes(), to);
        app.poll_nmea();
        if done(app) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn dragging_sounds_the_siren_and_says_what_is_wrong() {
    let siren = Counter::new(true);
    let (mut app, to) = app_with(Some(siren.clone()));

    // Sit still, then set the watch where the boat is.
    assert!(
        feed_until(&mut app, to, 32.71, -117.23, None, 10, |a| a
            .vessel_state()
            .fix
            .is_some()),
        "no fix arrived"
    );
    assert!(app.set_anchor(40.0), "the anchor watch would not set");
    assert!(app.anchored());
    assert!(app.alarm_state().active.is_empty(), "it fired immediately");

    // Drag: 0.002 degrees of latitude is about 220 m.
    assert!(
        feed_until(&mut app, to, 32.712, -117.23, None, 15, |a| !a
            .alarm_state()
            .active
            .is_empty()),
        "dragging 220 m did not fire the alarm"
    );

    let state = app.alarm_state();
    assert_eq!(state.active.len(), 1, "{:?}", state.active);
    assert!(
        state.active[0].contains("Anchor dragging"),
        "{:?}",
        state.active
    );
    // The distance, so the banner says how bad it is rather than only
    // that something is.
    assert!(state.active[0].contains('m'), "{:?}", state.active);
    assert!(state.sounding);
    assert!(state.audible);
    assert!(
        siren.sounded.load(Ordering::Relaxed) > 0,
        "the siren was never asked to sound"
    );

    // Silencing stops the noise and keeps the warning.
    app.acknowledge_alarms();
    let silenced = app.alarm_state();
    assert!(!silenced.sounding, "it is still sounding after Silence");
    assert!(
        !silenced.active.is_empty(),
        "Silence cancelled the warning as well as the noise"
    );

    // Weighing the anchor ends it.
    app.weigh_anchor();
    assert!(app.alarm_state().active.is_empty());
    assert!(!app.anchored());
}

#[test]
fn the_siren_repeats_rather_than_screaming() {
    // Called from the frame loop, which runs far faster than once a
    // second. Sounding on every poll would overlap the tone with itself
    // into a continuous noise nobody could think through.
    let siren = Counter::new(true);
    let (mut app, to) = app_with(Some(siren.clone()));

    feed_until(&mut app, to, 32.71, -117.23, None, 10, |a| {
        a.vessel_state().fix.is_some()
    });
    app.set_anchor(40.0);
    feed_until(&mut app, to, 32.712, -117.23, None, 15, |a| {
        !a.alarm_state().active.is_empty()
    });

    let after_firing = siren.sounded.load(Ordering::Relaxed);
    // Poll hard for a second: at the repeat interval that is at most one
    // more sounding.
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(900) {
        app.poll_nmea();
    }
    let extra = siren.sounded.load(Ordering::Relaxed) - after_firing;
    assert!(
        extra <= 1,
        "it sounded {extra} times in under a second of polling"
    );
}

#[test]
fn shoaling_fires_the_depth_alarm() {
    let siren = Counter::new(true);
    let (mut app, to) = app_with(Some(siren.clone()));

    app.set_shallow_alarm(Some(3.0));
    // Plenty of water: nothing happens.
    assert!(
        !feed_until(&mut app, to, 32.71, -117.23, Some(12.0), 3, |a| !a
            .alarm_state()
            .active
            .is_empty()),
        "twelve metres fired a three-metre alarm"
    );

    assert!(
        feed_until(&mut app, to, 32.71, -117.23, Some(1.8), 15, |a| !a
            .alarm_state()
            .active
            .is_empty()),
        "1.8 m under a 3 m alarm did not fire"
    );
    let state = app.alarm_state();
    assert!(
        state.active[0].contains("Shallow water"),
        "{:?}",
        state.active
    );
    assert!(
        state.active[0].contains("transducer"),
        "it should say what the depth is measured from: {:?}",
        state.active
    );
}

#[test]
fn a_platform_that_cannot_make_a_noise_says_so() {
    // The one thing a silent alarm must never do is fail silently. A
    // person needs to find this out while setting the alarm up, not at
    // anchor at three in the morning.
    for siren in [None, Some(Counter::new(false) as Arc<dyn Siren>)] {
        let (app, _) = app_with(siren);
        let state = app.alarm_state();
        assert!(
            !state.audible,
            "it claimed to be audible with no working siren"
        );
        assert!(!state.siren.is_empty(), "it should say what it tried");
    }

    let (app, _) = app_with(Some(Counter::new(true)));
    assert!(app.alarm_state().audible);
}

#[test]
fn the_watch_cannot_be_set_without_a_fix() {
    // Anchoring at wherever the chart happened to be scrolled would watch
    // the wrong circle entirely.
    let (mut app, _) = app_with(Some(Counter::new(true)));
    assert!(!app.set_anchor(40.0), "it anchored with no fix");
    assert!(!app.anchored());
    assert!(app.alarm_state().active.is_empty());
}

#[test]
fn an_alarm_keeps_sounding_when_the_feed_is_pulled() {
    // The quiet failure: the boat is dragging, the GPS is unplugged, and
    // an alarm that cleared on no data would stop just when nobody can
    // tell what is happening.
    let siren = Counter::new(true);
    let (mut app, to) = app_with(Some(siren.clone()));

    feed_until(&mut app, to, 32.71, -117.23, None, 10, |a| {
        a.vessel_state().fix.is_some()
    });
    app.set_anchor(40.0);
    feed_until(&mut app, to, 32.712, -117.23, None, 15, |a| {
        !a.alarm_state().active.is_empty()
    });
    assert!(app.alarm_state().sounding);

    app.apply_disconnect_nmea();
    for _ in 0..50 {
        app.poll_nmea();
    }
    assert!(
        app.alarm_state().sounding,
        "the alarm went quiet when the GPS was unplugged"
    );
}

#[test]
fn nothing_armed_means_nothing_fires_however_far_the_boat_goes() {
    let siren = Counter::new(true);
    let (mut app, to) = app_with(Some(siren.clone()));

    feed_until(&mut app, to, 0.0, 0.0, Some(0.2), 5, |a| {
        a.vessel_state().fix.is_some()
    });
    feed_until(&mut app, to, 60.0, 30.0, Some(0.2), 3, |_| false);

    assert!(app.alarm_state().active.is_empty());
    assert_eq!(
        siren.sounded.load(Ordering::Relaxed),
        0,
        "it made a noise with nothing armed"
    );
}

#[test]
fn a_waypoint_can_still_be_placed_while_an_alarm_is_firing() {
    // An alarm must not take the app over. Everything else has to keep
    // working while it sounds, because what you do about it is usually
    // navigate.
    let (mut app, to) = app_with(Some(Counter::new(true)));
    feed_until(&mut app, to, 32.71, -117.23, None, 10, |a| {
        a.vessel_state().fix.is_some()
    });
    app.set_anchor(40.0);
    feed_until(&mut app, to, 32.712, -117.23, None, 15, |a| {
        !a.alarm_state().active.is_empty()
    });

    app.add_waypoint(Position::new(32.70, -117.24));
    assert_eq!(app.route_state().waypoints.len(), 1);
    assert!(!app.alarm_state().active.is_empty(), "the alarm was lost");
}

#[test]
fn the_watch_runs_without_anything_drawing_or_polling() {
    // The reason the watch lives on the reader thread. It was first
    // evaluated from the frame loop, which means that with the screen off
    // — the only state an anchor watch exists for — nothing draws,
    // nothing polls, and nothing watches.
    //
    // So this arms the watch, then never calls `poll_nmea` again. The
    // alarm has to fire anyway.
    let siren = Counter::new(true);
    let (mut app, to) = app_with(Some(siren.clone()));

    feed_until(&mut app, to, 32.71, -117.23, None, 10, |a| {
        a.vessel_state().fix.is_some()
    });
    assert!(app.set_anchor(40.0));

    // Drag, with the app's own polling stopped dead.
    let sender = UdpSocket::bind("127.0.0.1:0").expect("sender");
    let dragged = sentences(32.712, -117.23, None);
    let start = Instant::now();
    let mut sounded = 0;
    while start.elapsed() < Duration::from_secs(15) {
        let _ = sender.send_to(dragged.as_bytes(), to);
        sounded = siren.sounded.load(Ordering::Relaxed);
        if sounded > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(
        sounded > 0,
        "the alarm never sounded with nothing drawing — it is still \
         tied to the frame loop"
    );

    // And the state is readable from this side afterwards, which is what
    // puts the banner on screen when someone does look.
    assert!(
        !app.alarm_state().active.is_empty(),
        "the watch fired but the panel cannot see it"
    );
}

#[test]
fn reconnecting_to_a_different_feed_keeps_the_watch() {
    // The watch is shared rather than owned by a receiver, so changing
    // feeds — the boat's instruments coming on, say — must not silently
    // disarm the anchor alarm.
    let siren = Counter::new(true);
    let (mut app, to) = app_with(Some(siren.clone()));

    feed_until(&mut app, to, 32.71, -117.23, None, 10, |a| {
        a.vessel_state().fix.is_some()
    });
    app.set_anchor(40.0);
    app.set_shallow_alarm(Some(3.0));

    // A different port, as reconnecting would.
    let other = {
        let probe = UdpSocket::bind("127.0.0.1:0").expect("probe");
        probe.local_addr().expect("addr").port()
    };
    app.apply_connect_nmea(&format!("udp://127.0.0.1:{other}"));

    let state = app.alarm_state();
    assert!(state.anchored, "the anchor watch was lost on reconnect");
    assert_eq!(state.shallow_m, Some(3.0), "the depth alarm was lost");
}

#[test]
fn the_hold_is_taken_while_a_watch_stands_and_dropped_when_it_ends() {
    // On a phone this is the difference between an anchor watch and a
    // process the system kills twenty minutes after the screen goes off.
    let log = Log::new();
    let (mut app, to, ()) = app_with_both(None, Some(log.clone()));

    assert!(log.calls().is_empty(), "held something before arming");
    assert!(
        !app.alarm_state().held,
        "it claims to be holding before anything is armed"
    );

    // A depth alarm needs no fix, so arm that first and check the hold is
    // taken on the strength of it alone.
    app.set_shallow_alarm(Some(3.0));
    assert_eq!(log.calls(), ["hold: Shallow alarm at 3.0 m"]);
    assert!(app.alarm_state().held);

    // Now the anchor as well: the same hold, re-announced, because what
    // the notification says has changed.
    assert!(
        feed_until(&mut app, to, 32.71, -117.23, Some(8.0), 10, |a| a
            .vessel_state()
            .fix
            .is_some()),
        "no fix arrived"
    );
    assert!(app.set_anchor(40.0));
    assert_eq!(
        log.calls(),
        [
            "hold: Shallow alarm at 3.0 m",
            "hold: Anchor watch and shallow alarm at 3.0 m",
        ]
    );

    // One disarmed, one still standing: still held.
    app.set_shallow_alarm(None);
    assert_eq!(log.calls().len(), 3);
    assert_eq!(log.calls()[2], "hold: Anchor watch standing");
    assert!(app.alarm_state().held);

    // And nothing armed: let go.
    app.weigh_anchor();
    assert_eq!(log.calls()[3], "release");
    assert!(!app.alarm_state().held);
}

#[test]
fn arming_the_same_watch_twice_does_not_re_announce_it() {
    // The radius slider re-sets the anchor on every pixel of movement.
    // Each of those is a `startForeground` call on Android, and a
    // notification that flickers while you drag a slider is a bug people
    // can see.
    let log = Log::new();
    let (mut app, to, ()) = app_with_both(None, Some(log.clone()));

    assert!(
        feed_until(&mut app, to, 32.71, -117.23, None, 10, |a| a
            .vessel_state()
            .fix
            .is_some()),
        "no fix arrived"
    );

    assert!(app.set_anchor(40.0));
    for radius in [41.0, 42.0, 43.0] {
        assert!(app.set_anchor(radius));
    }
    assert_eq!(
        log.calls(),
        ["hold: Anchor watch standing"],
        "the hold was re-announced for a change the notification never shows"
    );

    // Weighing twice is likewise one release, not two.
    app.weigh_anchor();
    app.weigh_anchor();
    assert_eq!(log.calls().len(), 2, "{:?}", log.calls());
}

#[test]
fn a_platform_with_nothing_to_hold_still_watches() {
    // A desktop passes `keepalive: None`. That must not stop an alarm —
    // it only changes what the panel says about the screen going off.
    let siren = Counter::new(true);
    let (mut app, to) = app_with(Some(siren.clone()));
    assert!(
        feed_until(&mut app, to, 32.71, -117.23, Some(2.0), 10, |a| a
            .vessel_state()
            .fix
            .is_some()),
        "no fix arrived"
    );
    app.set_shallow_alarm(Some(3.0));

    let state = app.alarm_state();
    assert!(!state.held, "it held something with no keepalive");
    assert_eq!(state.keepalive, "nothing");

    assert!(
        feed_until(&mut app, to, 32.71, -117.23, Some(2.0), 10, |a| a
            .alarm_state()
            .sounding),
        "the shallow alarm never sounded without a keepalive"
    );
}
