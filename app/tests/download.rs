//! The download, against a fetcher that never touches the network.
//!
//! Everything here is deterministic on purpose. The one thing a stub
//! cannot check — whether the real service answers, and with a JPEG — is
//! checked in `rutter-fetch`'s own live test. What this checks is the part
//! that is easy to get wrong and impossible to notice: that the tile list
//! is right, that six threads writing through one connection produce a
//! readable file, that a 404 over open water is not a failure, that
//! cancelling actually stops, and that a download which got nothing says
//! so rather than leaving a valid empty file behind.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rutter_app::{DownloadJob, Downloader, FetchOutcome, Stage, TileFetcher};
use rutter_core::LatLon;

/// Smallest thing that decodes as a JPEG to anything that checks the
/// magic. The content does not matter — nothing in the download path
/// decodes a tile, it only stores the bytes.
const FAKE_JPEG: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0xFF, 0xD9,
];

/// A 1x1 fully transparent PNG: what the USGS endpoint returns, with
/// HTTP 200, for an area it has no imagery of.
const BLANK_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0x60, 0x00, 0x02, 0x00,
    0x00, 0x05, 0x00, 0x01, 0xE9, 0xFA, 0xDC, 0xD8, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44,
    0xAE, 0x42, 0x60, 0x82,
];

/// A 1x1 PNG with one opaque black pixel: a real tile at the edge of
/// coverage, which comes back as PNG precisely because it is part imagery
/// and part transparent. A rule that rejected PNG outright threw 848 of
/// these away from one real download.
const EDGE_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0x10, 0x32, 0x09, 0xFB,
    0x0F, 0x00, 0x02, 0x94, 0x01, 0x9C, 0xC2, 0x05, 0x54, 0x4F, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// A fetcher that answers from a rule rather than a network.
#[derive(Debug)]
struct Stub {
    /// How many fetches have been asked for, so the tile count can be
    /// checked and the parallelism observed.
    asked: AtomicUsize,
    /// What to do with each request, by index.
    behaviour: Behaviour,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Behaviour {
    /// Every tile comes back.
    AlwaysGot,
    /// Nothing exists, as over mid-ocean.
    AlwaysMissing,
    /// Every request errors, as with no network.
    AlwaysFailing,
    /// One tile in three comes back, which is what a coastline looks like.
    Patchy,
    /// Slow enough that a cancellation can be observed mid-download.
    Slow,
    /// Answers with a transparent PNG, which is what USGS actually does
    /// for an area it has no imagery of — HTTP 200, 872 bytes, no 404.
    BlankPng,
    /// Answers with a real PNG that has an opaque pixel: a coverage-edge
    /// tile, which must be kept.
    EdgePng,
}

impl Stub {
    fn new(behaviour: Behaviour) -> Arc<Self> {
        Arc::new(Self {
            asked: AtomicUsize::new(0),
            behaviour,
        })
    }
}

impl TileFetcher for Stub {
    fn describe(&self) -> String {
        format!("stub ({:?})", self.behaviour)
    }

    fn fetch(&self, url: &str) -> FetchOutcome {
        let n = self.asked.fetch_add(1, Ordering::Relaxed);

        // The URL has to have been filled in. A template that still has
        // its braces would fetch nothing from a real service, and the
        // stub is the only place that can notice.
        assert!(
            !url.contains('{') && !url.contains('}'),
            "the URL template was not filled in: {url}"
        );
        assert!(url.starts_with("https://"), "not a URL: {url}");

        match self.behaviour {
            Behaviour::AlwaysGot => FetchOutcome::Got(FAKE_JPEG.to_vec()),
            Behaviour::AlwaysMissing => FetchOutcome::Missing,
            Behaviour::AlwaysFailing => FetchOutcome::Failed("no network".to_owned()),
            Behaviour::Patchy => {
                if n.is_multiple_of(3) {
                    FetchOutcome::Got(FAKE_JPEG.to_vec())
                } else {
                    FetchOutcome::Missing
                }
            }
            Behaviour::Slow => {
                std::thread::sleep(Duration::from_millis(20));
                FetchOutcome::Got(FAKE_JPEG.to_vec())
            }
            Behaviour::BlankPng => FetchOutcome::Got(BLANK_PNG.to_vec()),
            Behaviour::EdgePng => FetchOutcome::Got(EDGE_PNG.to_vec()),
        }
    }
}

/// A job over San Diego Bay entrance, writing into a scratch directory.
fn job(out: &std::path::Path, min_zoom: u8, max_zoom: u8) -> DownloadJob {
    DownloadJob {
        // A small box, so the tile count stays in the dozens at the zooms
        // these tests use.
        area: (LatLon::new(32.72, -117.26), LatLon::new(32.66, -117.20)),
        min_zoom,
        max_zoom,
        out: out.to_path_buf(),
        url_template: "https://example.invalid/tiles/{z}/{y}/{x}".to_owned(),
        name: "test imagery".to_owned(),
        attribution: Some("nobody".to_owned()),
        format: "jpg".to_owned(),
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("rutter-download-tests");
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir.join(name)
}

/// Run a download to completion, or panic saying where it got to.
fn finish(mut downloader: Downloader) -> rutter_app::Progress {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        downloader.poll();
        if downloader.progress().finished() {
            return downloader.progress().clone();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the download did not finish: {:?}", downloader.progress());
}

#[test]
fn a_download_writes_a_file_the_reader_can_open() {
    let out = scratch("complete.mbtiles");
    let plan = job(&out, 12, 14);
    let expected = plan.tiles().len();
    assert!(
        expected > 3,
        "only {expected} tiles; the box is too small to test"
    );

    let fetcher = Stub::new(Behaviour::AlwaysGot);
    let downloader = Downloader::start(plan, fetcher.clone()).expect("a download");
    let progress = finish(downloader);

    assert_eq!(progress.stage, Stage::Done, "{progress:?}");
    assert_eq!(progress.stored, expected);
    assert_eq!(progress.missing, 0);
    assert_eq!(progress.failed, 0);
    assert_eq!(progress.done, expected);
    assert!((progress.fraction() - 1.0).abs() < 1e-6);
    assert_eq!(fetcher.asked.load(Ordering::Relaxed), expected);

    // The point of the whole exercise: what comes out has to be readable
    // by the same code that reads a file built on a desktop. A writer that
    // got the TMS row flip wrong would produce a perfectly valid MBTiles
    // file whose tiles were all vertically mirrored in position.
    let source = rutter_charts::ImagerySource::open(&out).expect("the file should open");
    let meta = source.metadata();
    assert_eq!(meta.format, "jpg");
    assert_eq!(meta.min_zoom, 12);
    assert_eq!(meta.max_zoom, 14);
    assert_eq!(meta.attribution.as_deref(), Some("nobody"));
    assert!(meta.bounds.is_some(), "no bounds recorded");

    std::fs::remove_file(&out).ok();
}

#[test]
fn every_tile_in_the_range_is_asked_for_exactly_once() {
    // A duplicate fetch is wasted bandwidth on a slow link; a missed one
    // is a hole in the imagery. Neither shows up as an error.
    let out = scratch("once.mbtiles");
    let plan = job(&out, 10, 13);
    let tiles = plan.tiles();

    let mut sorted = tiles.clone();
    sorted.sort_by_key(|t| (t.z, t.x, t.y));
    let before = sorted.len();
    sorted.dedup();
    assert_eq!(before, sorted.len(), "the tile list repeats itself");

    // Deepest first, so an interrupted download keeps the detail.
    assert_eq!(tiles.first().map(|t| t.z), Some(13));
    assert_eq!(tiles.last().map(|t| t.z), Some(10));

    let fetcher = Stub::new(Behaviour::AlwaysGot);
    let progress = finish(Downloader::start(plan, fetcher.clone()).expect("a download"));
    assert_eq!(progress.stored, before);
    assert_eq!(fetcher.asked.load(Ordering::Relaxed), before);

    std::fs::remove_file(&out).ok();
}

#[test]
fn missing_tiles_are_not_failures() {
    // Most of a sea area has no imagery. If a 404 counted as an error then
    // every download would report thousands of failures and there would be
    // no way to tell a coverage edge from a broken network.
    let out = scratch("patchy.mbtiles");
    let plan = job(&out, 13, 14);
    let expected = plan.tiles().len();

    let progress =
        finish(Downloader::start(plan, Stub::new(Behaviour::Patchy)).expect("a download"));

    assert_eq!(progress.stage, Stage::Done, "{progress:?}");
    assert_eq!(
        progress.failed, 0,
        "a missing tile was counted as a failure"
    );
    assert!(progress.missing > 0, "the stub should have withheld some");
    assert!(progress.stored > 0, "the stub should have supplied some");
    assert_eq!(progress.stored + progress.missing, expected);

    std::fs::remove_file(&out).ok();
}

#[test]
fn a_download_that_got_nothing_says_so() {
    // The failure that would otherwise be silent: an area outside the
    // service's coverage produces a technically valid MBTiles file with no
    // tiles in it, which then opens fine and draws nothing. Someone would
    // conclude the renderer was broken.
    let out = scratch("empty.mbtiles");
    let progress = finish(
        Downloader::start(job(&out, 13, 14), Stub::new(Behaviour::AlwaysMissing))
            .expect("a download"),
    );

    match &progress.stage {
        Stage::Failed(why) => {
            assert!(
                why.contains("nothing downloaded"),
                "unhelpful reason: {why}"
            );
            assert!(why.contains("coverage"), "no hint at the cause: {why}");
        }
        other => panic!("an empty download reported {other:?}"),
    }
    assert_eq!(progress.stored, 0);

    std::fs::remove_file(&out).ok();
}

#[test]
fn a_failing_network_is_reported_as_a_failure_not_as_missing_imagery() {
    let out = scratch("failing.mbtiles");
    let plan = job(&out, 13, 14);
    let expected = plan.tiles().len();

    let progress =
        finish(Downloader::start(plan, Stub::new(Behaviour::AlwaysFailing)).expect("a download"));

    assert_eq!(progress.failed, expected);
    assert_eq!(progress.missing, 0);
    assert_eq!(progress.stored, 0);
    match &progress.stage {
        // The reason from the fetcher has to survive all the way to the
        // panel, or the user is told "nothing downloaded" when the real
        // answer is "no network".
        Stage::Failed(why) => assert!(why.contains("no network"), "lost the reason: {why}"),
        other => panic!("a failing network reported {other:?}"),
    }

    std::fs::remove_file(&out).ok();
}

#[test]
fn cancelling_stops_it_and_keeps_what_was_fetched() {
    // A hundred-and-twenty-megabyte download started by mistake has to
    // stop at once, and what it already fetched is still imagery — binning
    // it would waste the one expensive thing about the operation.
    let out = scratch("cancelled.mbtiles");
    let plan = job(&out, 13, 15);
    let total = plan.tiles().len();
    assert!(total > 20, "only {total} tiles; too few to cancel mid-way");

    let mut downloader = Downloader::start(plan, Stub::new(Behaviour::Slow)).expect("a download");

    // Let it get going, then stop it.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        downloader.poll();
        if downloader.progress().done > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    downloader.cancel();

    let progress = finish(downloader);
    assert_eq!(progress.stage, Stage::Cancelled, "{progress:?}");
    assert!(
        progress.done < total,
        "it ran to completion anyway: {} of {total}",
        progress.done
    );

    // And what it got is a usable file, with metadata describing what is
    // actually in it rather than what was asked for.
    let source = rutter_charts::ImagerySource::open(&out).expect("the partial file should open");
    assert!(
        source.metadata().max_zoom <= 15,
        "claimed a zoom it never reached"
    );

    std::fs::remove_file(&out).ok();
}

#[test]
fn dropping_the_handle_stops_the_download() {
    // Otherwise a download started by accident carries on to the end of a
    // hundred megabytes with nothing watching it.
    let out = scratch("dropped.mbtiles");
    let fetcher = Stub::new(Behaviour::Slow);
    let plan = job(&out, 13, 15);
    let total = plan.tiles().len();

    {
        let mut downloader = Downloader::start(plan, fetcher.clone()).expect("a download");
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            downloader.poll();
            if downloader.progress().done > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    // Give the threads a moment to notice, then check the count stops
    // climbing.
    std::thread::sleep(Duration::from_millis(300));
    let settled = fetcher.asked.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(300));
    let later = fetcher.asked.load(Ordering::Relaxed);

    assert_eq!(settled, later, "it kept fetching after being dropped");
    assert!(
        later < total,
        "it fetched everything anyway: {later} of {total}"
    );

    std::fs::remove_file(&out).ok();
}

#[test]
fn progress_only_goes_forwards() {
    // A bar that jumps backwards reads as a bug even when the download is
    // fine, and `done` is incremented from one thread but reported from
    // another.
    let out = scratch("monotonic.mbtiles");
    let mut downloader =
        Downloader::start(job(&out, 13, 15), Stub::new(Behaviour::Slow)).expect("a download");

    let mut highest = 0.0_f32;
    let mut done = 0;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        downloader.poll();
        let progress = downloader.progress();
        assert!(
            progress.fraction() >= highest - 1e-6,
            "went backwards: {} after {highest}",
            progress.fraction()
        );
        assert!(progress.done >= done, "the count went down");
        assert!(
            progress.fraction() <= 1.0,
            "over a hundred per cent: {}",
            progress.fraction()
        );
        highest = progress.fraction();
        done = progress.done;
        if progress.finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(7));
    }

    std::fs::remove_file(&out).ok();
}

// ── Through the app, as the panel's button drives it ────────────────────

/// An app with a stub fetcher and nowhere real to write.
fn app_with(fetcher: Arc<dyn TileFetcher>, out: &std::path::Path) -> rutter_app::App {
    let mut app = rutter_app::App::new(
        rutter_app::Sources {
            charts: None,
            imagery: None,
            tides: None,
            weather: None,
            nmea: None,
            gps: None,
            fetcher: Some(fetcher),
            imagery_out: Some(out.to_path_buf()),
            data_dir: None,
        },
        None,
    );
    // A small view over the harbour entrance, zoomed in enough that the
    // marked area is a handful of tiles.
    app.view.centre = rutter_app::Position::new(32.68, -117.23);
    app.view.zoom = 14.0;
    app
}

#[test]
fn the_app_will_not_start_a_download_with_no_area_marked() {
    let out = scratch("app-no-area.mbtiles");
    let mut app = app_with(Stub::new(Behaviour::AlwaysGot), &out);

    assert!(
        !app.start_download(),
        "it started a download of nothing in particular"
    );
    assert!(app.download_state().progress.is_none());
    // And the panel says fetching is possible, which is a different thing
    // from there being something to fetch.
    assert!(app.download_state().in_app);
}

#[test]
fn a_platform_with_no_fetcher_says_so_rather_than_offering_a_button() {
    let out = scratch("app-no-fetcher.mbtiles");
    let mut app = rutter_app::App::new(
        rutter_app::Sources {
            charts: None,
            imagery: None,
            tides: None,
            weather: None,
            nmea: None,
            gps: None,
            fetcher: None,
            imagery_out: Some(out),
            data_dir: None,
        },
        None,
    );
    app.mark_view_for_download();

    let state = app.download_state();
    assert!(!state.in_app, "claimed it could fetch with no fetcher");
    assert!(!state.commands.is_empty(), "no commands to fall back on");
    assert!(!app.start_download());
}

#[test]
fn a_finished_download_opens_itself() {
    // Most of the point of fetching on the device: imagery that needed the
    // app restarted before it appeared would feel broken however well the
    // download worked.
    let out = scratch("app-reopen.mbtiles");
    let mut app = app_with(Stub::new(Behaviour::AlwaysGot), &out);

    app.set_download_zoom(13, 14);
    app.mark_view_for_download();
    assert!(app.start_download(), "the download did not start");

    // Running: the panel has a bar to draw.
    let running = app.download_state().progress.expect("progress");
    assert!(running.running);

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        app.poll_download();
        if app.download_state().outcome.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let state = app.download_state();
    let outcome = state.outcome.expect("an outcome");
    assert!(outcome.contains("tiles"), "unhelpful outcome: {outcome:?}");
    // The handle is gone, so the panel offers the button again rather than
    // a stalled bar.
    assert!(
        state.progress.is_none(),
        "the download is still showing as running"
    );

    // And the imagery it fetched is the imagery now open.
    assert!(
        app.panel_imagery_available(),
        "the download did not open what it fetched"
    );

    std::fs::remove_file(&out).ok();
}

#[test]
fn starting_a_second_download_while_one_runs_is_refused() {
    // Two at once would compete for the same slow link and both finish
    // later than running them in turn.
    let out = scratch("app-one-at-a-time.mbtiles");
    let mut app = app_with(Stub::new(Behaviour::Slow), &out);
    app.set_download_zoom(13, 15);
    app.mark_view_for_download();

    assert!(app.start_download());
    assert!(!app.start_download(), "it started a second download");

    app.cancel_download();
    std::fs::remove_file(&out).ok();
}

// ── Downloads add coverage; they never replace it ───────────────────────

#[test]
fn a_second_download_adds_to_the_first_instead_of_replacing_it() {
    // The bug this pins cost a real 35 MB of harbour imagery. The download
    // wrote straight to the file the app already had, and `Writer::create`
    // deletes what is there — so fetching a zoomed-out view at z6..12
    // silently destroyed the z13..16 detail underneath it. Nothing errored
    // and nothing warned; the harbour simply went blurry.
    let out = scratch("accumulate.mbtiles");
    std::fs::remove_file(&out).ok();

    // Deep over a small area: the harbour detail.
    let mut app = app_with(Stub::new(Behaviour::AlwaysGot), &out);
    app.view.centre = rutter_app::Position::new(32.68, -117.23);
    app.view.zoom = 15.0;
    app.set_download_zoom(14, 15);
    app.mark_view_for_download();
    assert!(app.start_download());
    run_to_completion(&mut app);

    let after_first = tile_count(&out);
    assert!(after_first > 0, "the first download stored nothing");

    // Then shallow over a wide one: the overview, as if someone zoomed
    // out and hit Download again.
    app.view.zoom = 8.0;
    app.set_download_zoom(7, 8);
    app.mark_view_for_download();
    assert!(app.start_download());
    run_to_completion(&mut app);

    let after_second = tile_count(&out);
    assert!(
        after_second > after_first,
        "the second download left {after_second} tiles where the first had \
         {after_first} — it replaced rather than added"
    );

    // And the file still describes everything it holds. A merged file that
    // kept only the newcomer's zoom range would make the detail
    // unreachable: the reader clamps requests to the stated range, so the
    // tiles would be sitting there and never asked for.
    let source = rutter_charts::ImagerySource::open(&out).expect("open");
    let meta = source.metadata();
    assert_eq!(meta.min_zoom, 7, "the shallow end was lost");
    assert_eq!(meta.max_zoom, 15, "the detail was lost");

    // Bounds have to cover both, for the same reason.
    let bounds = meta.bounds.expect("bounds");
    assert!(
        bounds[2] - bounds[0] > 0.5,
        "bounds shrank to the last download: {bounds:?}"
    );

    std::fs::remove_file(&out).ok();
}

#[test]
fn a_download_leaves_no_part_file_behind() {
    // A stray part file would be mistaken for a resumable download later,
    // and on a phone it is a hundred megabytes nobody can account for.
    let out = scratch("tidy.mbtiles");
    std::fs::remove_file(&out).ok();

    let mut app = app_with(Stub::new(Behaviour::AlwaysGot), &out);
    app.set_download_zoom(13, 14);
    app.mark_view_for_download();
    assert!(app.start_download());
    run_to_completion(&mut app);

    let part = out.with_file_name(format!(
        "{}.part",
        out.file_name().unwrap().to_string_lossy()
    ));
    assert!(!part.exists(), "{} was left behind", part.display());

    std::fs::remove_file(&out).ok();
}

#[test]
fn a_download_that_got_nothing_leaves_the_existing_imagery_alone() {
    // The worst version of the original bug: an area outside the coverage
    // wipes good imagery and replaces it with nothing at all.
    let out = scratch("untouched.mbtiles");
    std::fs::remove_file(&out).ok();

    let mut app = app_with(Stub::new(Behaviour::AlwaysGot), &out);
    app.set_download_zoom(13, 14);
    app.mark_view_for_download();
    assert!(app.start_download());
    run_to_completion(&mut app);
    let before = tile_count(&out);
    assert!(before > 0);

    // Now one that finds nothing.
    let mut app = app_with(Stub::new(Behaviour::AlwaysMissing), &out);
    app.set_download_zoom(13, 14);
    app.mark_view_for_download();
    assert!(app.start_download());
    run_to_completion(&mut app);

    assert_eq!(
        tile_count(&out),
        before,
        "an empty download disturbed the imagery that was there"
    );

    std::fs::remove_file(&out).ok();
}

/// Drive an app's download to its end.
fn run_to_completion(app: &mut rutter_app::App) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        app.poll_download();
        if app.download_state().progress.is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the download did not finish");
}

/// Tiles in a file, or zero if it is not there.
///
/// Counted straight out of the database rather than through the reader,
/// because the thing under test is partly whether the reader's declared
/// zoom range still matches what is stored — so trusting that range to do
/// the counting would hide the bug.
fn tile_count(path: &std::path::Path) -> usize {
    if !path.exists() {
        return 0;
    }
    let file = rutter_charts::mbtiles::MbTiles::open(path).expect("open");
    let mut seen = 0;
    for z in 0..=22 {
        seen += file.tiles_at(z).map(|t| t.len()).unwrap_or(0);
    }
    seen
}

#[test]
fn a_blank_png_is_not_stored_as_imagery() {
    // The bug this pins was invisible and expensive. The USGS endpoint
    // answers an uncovered area with HTTP 200 and a transparent PNG rather
    // than a 404, so a download over open water stored more than half its
    // tiles as 872-byte blanks, reported them as successes, and wrote
    // `format = jpg` over a file that was half PNG. The only symptom was a
    // tile that would not decode opaque — and that only surfaced because
    // an unrelated test went looking.
    let out = scratch("blank.mbtiles");
    std::fs::remove_file(&out).ok();

    let plan = job(&out, 13, 14);
    let expected = plan.tiles().len();
    let progress =
        finish(Downloader::start(plan, Stub::new(Behaviour::BlankPng)).expect("a download"));

    assert_eq!(
        progress.missing, expected,
        "blanks were taken for imagery: {} stored",
        progress.stored
    );
    assert_eq!(progress.stored, 0);
    assert_eq!(progress.bytes, 0, "blank bytes were counted as downloaded");

    // And with nothing real stored it says so, rather than leaving a file
    // that opens cleanly and draws nothing.
    match &progress.stage {
        Stage::Failed(why) => assert!(why.contains("nothing downloaded"), "{why}"),
        other => panic!("a download of pure blanks reported {other:?}"),
    }

    std::fs::remove_file(&out).ok();
}

#[test]
fn a_png_with_a_picture_in_it_is_kept() {
    // The correction to the fix above, which cost 848 real tiles to learn.
    // Tiles at the edge of coverage come back as PNG *because* they are
    // part imagery and part transparent, so a format rule throws away
    // exactly the tiles along every coastline.
    let out = scratch("edge.mbtiles");
    std::fs::remove_file(&out).ok();

    let plan = job(&out, 13, 13);
    let expected = plan.tiles().len();
    let progress =
        finish(Downloader::start(plan, Stub::new(Behaviour::EdgePng)).expect("a download"));

    assert_eq!(
        progress.stored, expected,
        "coverage-edge PNGs were discarded: {} missing",
        progress.missing
    );
    assert_eq!(progress.missing, 0);
    assert_eq!(progress.stage, Stage::Done);

    std::fs::remove_file(&out).ok();
}

#[test]
fn the_declared_format_is_what_was_actually_stored() {
    let out = scratch("format.mbtiles");
    std::fs::remove_file(&out).ok();

    let plan = job(&out, 13, 13);
    finish(Downloader::start(plan, Stub::new(Behaviour::AlwaysGot)).expect("a download"));

    let source = rutter_charts::ImagerySource::open(&out).expect("open");
    assert_eq!(source.metadata().format, "jpg");

    // Every stored tile carries a picture. Not every one is the declared
    // format — a coverage-edge tile is legitimately PNG in a file of
    // JPEGs — but none of them may be blank.
    let file = rutter_charts::mbtiles::MbTiles::open(&out).expect("open");
    for id in file.tiles_at(13).expect("tiles") {
        let bytes = file.raw_tile(id).expect("read").expect("present");
        assert!(
            !rutter_charts::is_blank_tile(&bytes),
            "{id:?} is a blank tile"
        );
    }

    std::fs::remove_file(&out).ok();
}
