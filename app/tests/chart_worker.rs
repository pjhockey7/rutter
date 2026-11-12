//! The chart worker's plumbing.
//!
//! The failure mode this guards is quiet: if answers stop being delivered,
//! or are delivered but never matched to their request, the chart simply
//! stays as it was. Nothing errors, nothing logs, and panning looks like it
//! works until you notice the geometry is from a minute ago.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use rutter_app::ChartWorker;
use rutter_charts::ChartSource;
use rutter_core::{LatLon, View};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../charts/tests/data/coast.mbtiles"
);

/// The fixture covers a patch of Cornwall.
fn view(zoom: f64) -> View {
    View::new(LatLon::new(50.0975, -5.025), zoom, 1280, 800)
}

fn worker() -> ChartWorker {
    ChartWorker::spawn(ChartSource::open(FIXTURE, &[]).expect("open the fixture"))
        .expect("the worker thread should start")
}

/// Poll for an answer, with a ceiling so a broken worker fails rather than
/// hangs the suite.
fn wait_for(worker: &mut ChartWorker) -> Option<Box<rutter_charts::Collected>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(got) = worker.take_answer() {
            return Some(got);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    None
}

#[test]
fn the_metadata_is_available_without_waiting() {
    // The main thread needs the zoom range to choose tiles and the bounds
    // to open on the chart. Both have to be readable immediately, or the
    // first frame would have to block on the worker — which is the thing
    // this design exists to avoid.
    let worker = worker();
    let meta = worker.metadata();
    assert_eq!(meta.min_zoom, 10);
    assert_eq!(meta.max_zoom, 12);
    assert!(meta.bounds.is_some());
    assert!(!meta.layers.is_empty());

    assert_eq!(worker.clamp_zoom(2), 10, "below the file's range");
    assert_eq!(worker.clamp_zoom(18), 12, "above it");
}

#[test]
fn a_request_is_answered_with_geometry() {
    let mut worker = worker();
    assert!(!worker.waiting(), "nothing asked for yet");

    assert!(worker.request(&view(12.0), &[]));
    assert!(worker.waiting(), "a request is outstanding");

    let got = wait_for(&mut worker).expect("the worker should answer");
    assert!(!worker.waiting(), "the answer clears the request");
    assert!(!got.tiles.is_empty(), "the view covers tiles");
    assert!(
        !got.lines.is_empty(),
        "the fixture has coastline and depth areas"
    );
    assert!(got.failed.is_empty(), "{:?}", got.failed);
}

#[test]
fn nothing_is_delivered_before_it_is_asked_for() {
    let mut worker = worker();
    assert!(
        worker.take_answer().is_none(),
        "an answer appeared without a request"
    );
}

#[test]
fn only_the_newest_request_is_answered() {
    // A drag queues viewports faster than they can be decoded. Serving the
    // stale ones would make the chart snap back to where the drag was,
    // which is worse than it arriving late.
    let mut worker = worker();

    worker.request(&view(10.0), &[]);
    worker.request(&view(11.0), &[]);
    worker.request(&view(12.0), &[]);

    let got = wait_for(&mut worker).expect("an answer");
    // Zoom 12 is the deepest, so its tile set is the largest of the three;
    // whichever answer arrives, it has to be the last one asked for.
    let deepest = got.tiles.iter().map(|t| t.z).max().expect("tiles");
    assert_eq!(deepest, 12, "the answer should be for the newest request");

    // And nothing else is still queued up behind it.
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        worker.take_answer().is_none(),
        "stale answers should be dropped, not delivered later"
    );
}

#[test]
fn the_layer_selection_travels_with_the_request() {
    // The source lives on the worker, so a selection change can only reach
    // it by riding along with a request. If that broke, the layer picker
    // would silently do nothing.
    let mut worker = worker();

    worker.request(&view(12.0), &[]);
    let all = wait_for(&mut worker).expect("an answer").styles();
    assert!(all.len() > 1, "expected several styles, got {all:?}");

    worker.request(&view(12.0), &["COALNE".to_owned()]);
    let coast = wait_for(&mut worker).expect("an answer").styles();
    assert_eq!(
        coast,
        vec![rutter_core::chart::ChartStyle::Coastline],
        "only the selected layer should come back"
    );
}

#[test]
fn the_worker_stops_when_it_is_dropped() {
    // The thread holds a SQLite connection and a tile cache. Leaking one
    // per chart would matter on Android, where the activity comes and goes.
    let mut worker = worker();
    worker.request(&view(12.0), &[]);
    let _ = wait_for(&mut worker);
    drop(worker);
    // Nothing to assert directly — the thread exits when its channel closes
    // and this test exists so that a change which parks it forever (a
    // `loop` on a timeout, say) shows up as a hung suite.
}
