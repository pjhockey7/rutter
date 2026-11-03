//! The tide-station reader, against databases built here and by `rutter`.
//!
//! The fixtures are built in-test rather than committed: the schema is four
//! tables and writing it out here keeps the reader's expectations and the
//! writer's output visible side by side. The real-file tests at the bottom
//! run against whatever `rutter tides` produced, and skip when it has not
//! been run — the same arrangement as the imagery tests.

// A fixture that will not build is a broken test, not a condition to
// recover from.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use rutter_core::tide::Kind;
use rutter_core::LatLon;
use rutter_tides::{Error, TideStations};

/// Somewhere to put a fixture, named after the test that wants it.
fn scratch(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "rutter-tides-test-{name}-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

/// One constituent for a fixture: name, amplitude in metres, phase.
type FixtureConstituent<'a> = (&'a str, f64, f64);
/// One station: id, name, latitude, longitude, Z0, constituents.
type FixtureStation<'a> = (
    &'a str,
    &'a str,
    f64,
    f64,
    f64,
    &'a [FixtureConstituent<'a>],
);

/// Build a database with the same schema `scripts/tide-stations.py` writes.
fn fixture(path: &Path, stations: &[FixtureStation<'_>]) {
    let db = rusqlite::Connection::open(path).expect("create fixture");
    db.execute_batch(
        "CREATE TABLE station (
            id TEXT PRIMARY KEY, name TEXT NOT NULL,
            lat REAL NOT NULL, lon REAL NOT NULL, datum_m REAL NOT NULL);
         CREATE TABLE constituent (
            station TEXT NOT NULL, name TEXT NOT NULL,
            amplitude_m REAL NOT NULL, phase_deg REAL NOT NULL,
            PRIMARY KEY (station, name));
         CREATE TABLE metadata (name TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO metadata VALUES ('attribution', 'Tides: NOAA CO-OPS (public domain)');",
    )
    .expect("schema");

    for (id, name, lat, lon, datum, constituents) in stations {
        db.execute(
            "INSERT INTO station VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, name, lat, lon, datum],
        )
        .expect("insert station");
        for (c, amplitude, phase) in *constituents {
            db.execute(
                "INSERT INTO constituent VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, c, amplitude, phase],
            )
            .expect("insert constituent");
        }
    }
}

/// San Diego's four largest terms, enough to make a recognisable tide.
const SAN_DIEGO: &[FixtureConstituent<'static>] = &[
    ("M2", 0.541934, 143.300),
    ("K1", 0.337109, 208.400),
    ("S2", 0.224942, 140.400),
    ("O1", 0.214055, 192.600),
];

#[test]
fn reads_stations_and_their_constituents() {
    let path = scratch("reads");
    fixture(
        &path,
        &[
            (
                "9410170",
                "SAN DIEGO (Broadway)",
                32.7156,
                -117.1767,
                0.8961,
                SAN_DIEGO,
            ),
            (
                "9410079",
                "Avalon, Santa Catalina Island",
                33.345,
                -118.325,
                0.84,
                SAN_DIEGO,
            ),
        ],
    );

    let db = TideStations::open(&path).expect("open");
    assert_eq!(db.count().expect("count"), 2);
    assert_eq!(db.attribution(), Some("Tides: NOAA CO-OPS (public domain)"));

    let all = db.all().expect("all");
    assert_eq!(all.len(), 2);
    // Ordered by id, so Avalon comes first.
    assert_eq!(all[0].id, "9410079");
    assert_eq!(all[1].name, "SAN DIEGO (Broadway)");

    let sd = &all[1];
    assert_eq!(sd.constituents.len(), 4);
    // Largest first, which is what the query asks for.
    assert_eq!(sd.constituents[0].harmonic.name(), "M2");
    assert!(sd.constituents[0].amplitude_m > sd.constituents[1].amplitude_m);
    assert!((sd.datum_m - 0.8961).abs() < 1e-9);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_station_read_back_predicts_a_tide() {
    // The point of the whole crate: what comes out of the database has to be
    // something `rutter-core` can predict with. A reader that dropped the
    // phases, or the datum, or scaled the amplitudes wrongly would still
    // return a Station — so this checks the water actually moves, around
    // the right level, with the right period.
    let path = scratch("predicts");
    fixture(
        &path,
        &[(
            "9410170",
            "SAN DIEGO (Broadway)",
            32.7156,
            -117.1767,
            0.8961,
            SAN_DIEGO,
        )],
    );

    let db = TideStations::open(&path).expect("open");
    let station = db
        .nearest(LatLon::new(32.7, -117.2))
        .expect("nearest")
        .expect("a station");

    // 2026-03-01T00:00:00Z, a day.
    let start = 1_772_323_200;
    let curve = station.curve(start, start + 86_400, 600);
    assert_eq!(curve.len(), 145);

    let high = curve.iter().map(|p| p.1).fold(f64::MIN, f64::max);
    let low = curve.iter().map(|p| p.1).fold(f64::MAX, f64::min);

    // San Diego's range is around 1.5-2.5 m; anything outside that is a
    // units or datum mistake rather than an unusual day.
    assert!(
        (1.0..3.5).contains(&(high - low)),
        "range {:.3} m is not a San Diego tide",
        high - low
    );
    // Soundings are from lowest water, so a prediction should rarely go
    // below it and never go far below.
    assert!(low > -0.6, "low water {low:.3} m below chart datum");
    assert!(high < 3.0, "high water {high:.3} m above chart datum");

    // Mixed semidiurnal: four turning points in a day, give or take one.
    let extremes = station.extremes(start, start + 86_400);
    assert!(
        (3..=5).contains(&extremes.len()),
        "{} turning points in a day",
        extremes.len()
    );
    assert!(extremes.iter().any(|e| e.kind == Kind::High));
    assert!(extremes.iter().any(|e| e.kind == Kind::Low));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn in_box_selects_by_position() {
    let path = scratch("inbox");
    fixture(
        &path,
        &[
            (
                "9410170",
                "SAN DIEGO (Broadway)",
                32.7156,
                -117.1767,
                0.8961,
                SAN_DIEGO,
            ),
            (
                "9410079",
                "Avalon, Santa Catalina Island",
                33.345,
                -118.325,
                0.84,
                SAN_DIEGO,
            ),
            ("9410840", "Santa Monica", 34.0083, -118.5, 0.85, SAN_DIEGO),
        ],
    );
    let db = TideStations::open(&path).expect("open");

    // A box over San Diego Bay only.
    let found = db
        .in_box(LatLon::new(32.9, -117.4), LatLon::new(32.5, -117.0))
        .expect("in_box");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, "9410170");

    // One spanning the crossing picks up Avalon too, but not Santa Monica.
    let found = db
        .in_box(LatLon::new(33.7, -118.7), LatLon::new(32.4, -117.0))
        .expect("in_box");
    let ids: Vec<&str> = found.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["9410079", "9410170"]);

    // Nothing at all is empty, not an error.
    let found = db
        .in_box(LatLon::new(10.0, -10.0), LatLon::new(9.0, -9.0))
        .expect("in_box");
    assert!(found.is_empty());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn nearest_picks_the_closest_not_the_first() {
    let path = scratch("nearest");
    fixture(
        &path,
        &[
            // Deliberately ordered so that the closest is not first by id.
            (
                "9410032",
                "Wilson Cove, San Clemente Island",
                33.005,
                -118.557,
                0.83,
                SAN_DIEGO,
            ),
            (
                "9410170",
                "SAN DIEGO (Broadway)",
                32.7156,
                -117.1767,
                0.8961,
                SAN_DIEGO,
            ),
        ],
    );
    let db = TideStations::open(&path).expect("open");

    let at_san_diego = db
        .nearest(LatLon::new(32.70, -117.20))
        .expect("nearest")
        .expect("a station");
    assert_eq!(at_san_diego.id, "9410170");

    let off_san_clemente = db
        .nearest(LatLon::new(33.00, -118.50))
        .expect("nearest")
        .expect("a station");
    assert_eq!(off_san_clemente.id, "9410032");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_unknown_constituent_is_dropped_not_fatal() {
    // NOAA's spelling is not guaranteed stable and a station may carry a
    // term this build has never heard of. Losing one small term is
    // sub-centimetre; refusing the station would lose the tide entirely.
    let path = scratch("unknown");
    fixture(
        &path,
        &[(
            "9410170",
            "SAN DIEGO (Broadway)",
            32.7156,
            -117.1767,
            0.8961,
            &[
                ("M2", 0.541934, 143.300),
                ("NOT_A_CONSTITUENT", 0.01, 12.0),
                ("K1", 0.337109, 208.400),
            ],
        )],
    );

    let db = TideStations::open(&path).expect("open");
    let all = db.all().expect("all");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].constituents.len(), 2);
    assert!(all[0]
        .constituents
        .iter()
        .all(|c| ["M2", "K1"].contains(&c.harmonic.name())));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_station_with_nothing_usable_is_skipped() {
    let path = scratch("useless");
    fixture(
        &path,
        &[
            (
                "9410170",
                "SAN DIEGO (Broadway)",
                32.7156,
                -117.1767,
                0.8961,
                SAN_DIEGO,
            ),
            (
                "BOGUS01",
                "All unknown terms",
                32.0,
                -117.0,
                0.8,
                &[("WHAT", 1.0, 0.0)],
            ),
        ],
    );

    let db = TideStations::open(&path).expect("open");
    // `count` is the raw row count; `all` is what can actually be predicted.
    assert_eq!(db.count().expect("count"), 2);
    let all = db.all().expect("all");
    assert_eq!(all.len(), 1, "the unusable station should be dropped");
    assert_eq!(all[0].id, "9410170");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_sqlite_file_that_is_not_a_tide_database_says_so() {
    // Handing `rutter run` a chart where it wanted tides is an easy
    // mistake, and "no stations" is a poor way to report it.
    let path = scratch("wrong-kind");
    let db = rusqlite::Connection::open(&path).expect("create");
    db.execute_batch("CREATE TABLE tiles (zoom_level integer);")
        .expect("schema");
    drop(db);

    match TideStations::open(&path) {
        Err(Error::NotATideDatabase) => {}
        Err(other) => panic!("expected NotATideDatabase, got {other}"),
        Ok(_) => panic!("a tiles table is not a tide database"),
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_missing_file_is_an_error_not_a_panic() {
    let path = scratch("absent");
    assert!(matches!(TideStations::open(&path), Err(Error::Sqlite(_))));
}

// ── Against the real database ───────────────────────────────────────────────
//
// These run on `build/charts/tides.sqlite`, which `rutter tides` produces
// from NOAA. It is not committed — it is fetched data, and the fixture that
// pins the maths lives in core/tests/data/tide/ instead. Build it with:
//
//     rutter tides --near 32.68,-117.23 --radius 80

fn real_database() -> Option<PathBuf> {
    let path = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../build/charts/tides.sqlite"
    ));
    path.exists().then_some(path)
}

#[test]
fn reads_the_real_noaa_database() {
    let Some(path) = real_database() else {
        eprintln!("skipping: no build/charts/tides.sqlite — see the note above");
        return;
    };

    let db = TideStations::open(&path).expect("open");
    let all = db.all().expect("all");
    assert!(!all.is_empty(), "the database should hold stations");

    for station in &all {
        assert!(!station.id.is_empty());
        assert!(!station.name.is_empty());
        assert!((-90.0..=90.0).contains(&station.lat), "{}", station.name);
        assert!((-180.0..=180.0).contains(&station.lon), "{}", station.name);
        // Every NOAA station in this region has MSL well above MLLW.
        assert!(
            (0.2..3.0).contains(&station.datum_m),
            "{} has Z0 {}",
            station.name,
            station.datum_m
        );
        assert!(
            station.constituents.len() >= 15,
            "{} has only {} constituents",
            station.name,
            station.constituents.len()
        );
        // M2 should dominate everywhere on this coast.
        assert_eq!(
            station.constituents[0].harmonic.name(),
            "M2",
            "{} is not M2-dominated",
            station.name
        );
    }

    println!("{} stations:", all.len());
    for s in &all {
        println!(
            "  {} {:<40} {:2} constituents  Z0 {:.3} m",
            s.id,
            s.name,
            s.constituents.len(),
            s.datum_m
        );
    }
}

#[test]
fn every_real_station_predicts_a_plausible_tide() {
    let Some(path) = real_database() else {
        eprintln!("skipping: no build/charts/tides.sqlite");
        return;
    };

    let db = TideStations::open(&path).expect("open");
    // A week from 2026-03-01T00:00:00Z.
    let start = 1_772_323_200;
    let week = 7 * 86_400;

    for station in db.all().expect("all") {
        let curve = station.curve(start, start + week, 900);
        let high = curve.iter().map(|p| p.1).fold(f64::MIN, f64::max);
        let low = curve.iter().map(|p| p.1).fold(f64::MAX, f64::min);

        assert!(
            (1.2..3.5).contains(&(high - low)),
            "{}: a week's range of {:.2} m is not a southern California tide",
            station.name,
            high - low
        );
        assert!(low > -1.0, "{}: low water {low:.2} m", station.name);

        // Mixed semidiurnal all along this coast: close to two highs and
        // two lows a day, so about 28 turning points in a week.
        let extremes = station.extremes(start, start + week);
        assert!(
            (22..=32).contains(&extremes.len()),
            "{}: {} turning points in a week",
            station.name,
            extremes.len()
        );
        for pair in extremes.windows(2) {
            assert_ne!(
                pair[0].kind, pair[1].kind,
                "{}: two {:?} waters in a row",
                station.name, pair[0].kind
            );
        }
    }
}
