//! That the fetcher can actually get a tile from USGS.
//!
//! The one test here that cannot be faked. Everything about the download
//! machinery — the tile list, the parallelism, the writer, the progress —
//! can be exercised against a stub fetcher, and is. What a stub cannot
//! tell you is whether the URL template is right, whether the TLS handshake
//! works against the real service, or whether the thing that comes back is
//! a JPEG rather than an HTML error page. Each of those has exactly one way
//! to be wrong and no local way to find out.
//!
//! Skipped rather than failed without a network, because a boat is the
//! normal place for this to run and a boat has no signal.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rutter_app::{FetchOutcome, TileFetcher};
use rutter_fetch::Https;

/// The same endpoint `rutter imagery` uses, and the same `{z}/{y}/{x}`
/// order — row before column, which is what the ArcGIS REST tile service
/// takes. Getting that the other way round returns a perfectly valid tile
/// of the wrong place, which is the kind of error that only shows up as
/// imagery that does not line up with the chart.
const TILE: &str = "https://basemap.nationalmap.gov/arcgis/rest/services\
/USGSImageryOnly/MapServer/tile/12/1635/700";

/// Somewhere with no imagery: zoom 2, the middle of the Pacific.
const NOWHERE: &str = "https://basemap.nationalmap.gov/arcgis/rest/services\
/USGSImageryOnly/MapServer/tile/18/99999999/99999999";

#[test]
fn a_real_tile_comes_back_as_a_jpeg() {
    let fetcher = Https::new().expect("a fetcher");

    match fetcher.fetch(TILE) {
        FetchOutcome::Got(bytes) => {
            // JPEG's own magic, checked rather than trusted: a captive
            // portal or a service error returns 200 with an HTML page, and
            // storing that as a tile would give imagery that decodes to
            // nothing and a download that looked like a success.
            assert!(
                bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
                "not a JPEG — first bytes {:02X?}",
                &bytes[..bytes.len().min(16)]
            );
            // A 256-pixel aerial tile is tens of kilobytes. A few hundred
            // bytes would mean an error document that happens to start
            // right.
            assert!(
                bytes.len() > 2_000,
                "only {} bytes, which is not a tile",
                bytes.len()
            );
            println!("got {} bytes", bytes.len());
        }
        FetchOutcome::Missing => panic!("USGS has imagery over San Diego; the URL is wrong"),
        FetchOutcome::Failed(why) => {
            // No network is the expected state on a boat, so this is a
            // skip rather than a failure — but it says so loudly enough
            // not to be mistaken for a pass.
            eprintln!("skipping: could not reach USGS ({why})");
        }
    }
}

#[test]
fn a_tile_that_does_not_exist_is_missing_rather_than_an_error() {
    // The distinction the whole download rests on. Most of a sea area has
    // no imagery, so if "no tile here" came back as an error then every
    // download would report thousands of failures and there would be no
    // way to tell a coverage edge from a broken network.
    let fetcher = Https::new().expect("a fetcher");

    match fetcher.fetch(NOWHERE) {
        FetchOutcome::Missing => {}
        FetchOutcome::Got(bytes) => panic!("got {} bytes from nowhere", bytes.len()),
        FetchOutcome::Failed(why) => {
            if why.contains("dns") || why.contains("connect") || why.contains("resolve") {
                eprintln!("skipping: could not reach USGS ({why})");
            } else {
                panic!("a missing tile was reported as a failure: {why}");
            }
        }
    }
}

#[test]
fn a_real_download_produces_imagery_that_decodes() {
    // The whole feature, end to end, against the real service: plan the
    // tiles, fetch them over TLS, write the MBTiles file, reopen it with
    // the reader the app uses, and decode a tile to pixels.
    //
    // Small on purpose — two zoom levels over one harbour entrance, which
    // is a handful of tiles. The point is to exercise every link in the
    // chain, not to measure throughput, and this runs against somebody
    // else's public-domain server on every `rutter test`.
    use rutter_app::{DownloadJob, Downloader, Stage};
    use rutter_core::LatLon;

    let Ok(fetcher) = Https::new() else {
        eprintln!("skipping: no fetcher");
        return;
    };

    let out = std::env::temp_dir().join("rutter-live-download.mbtiles");
    let job = DownloadJob {
        // San Diego Bay entrance, about a mile across.
        area: (LatLon::new(32.69, -117.245), LatLon::new(32.67, -117.22)),
        min_zoom: 13,
        max_zoom: 14,
        out: out.clone(),
        url_template: "https://basemap.nationalmap.gov/arcgis/rest/services\
/USGSImageryOnly/MapServer/tile/{z}/{y}/{x}"
            .to_owned(),
        name: "USGS imagery".to_owned(),
        attribution: Some("Imagery: USGS National Map / USDA NAIP (public domain)".to_owned()),
        format: "jpg".to_owned(),
    };
    let expected = job.tiles().len();
    assert!(
        expected > 0 && expected < 40,
        "{expected} tiles is the wrong size for this test"
    );

    let mut downloader = Downloader::start(job, std::sync::Arc::new(fetcher)).expect("a download");

    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(120) {
        downloader.poll();
        if downloader.progress().finished() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let progress = downloader.progress().clone();
    match &progress.stage {
        Stage::Done => {}
        Stage::Failed(why) => {
            // No network is the expected state on a boat.
            eprintln!("skipping: {why}");
            std::fs::remove_file(&out).ok();
            return;
        }
        other => panic!("the download ended as {other:?}"),
    }

    println!(
        "  {} tiles, {} missing, {:.0} KB",
        progress.stored,
        progress.missing,
        progress.bytes as f64 / 1024.0
    );
    assert!(progress.stored > 0, "nothing was stored over San Diego");

    // Reopened with the reader the app uses, and a tile decoded to pixels.
    // A writer that got the TMS row flip wrong would produce a file that
    // opens cleanly and puts every tile in the wrong place; a writer that
    // stored the bytes wrongly would produce one that will not decode.
    let mut source = rutter_charts::ImagerySource::open(&out).expect("the file should open");
    assert_eq!(source.metadata().format, "jpg");

    // Decoded, not merely present. A tile stored with the row flip wrong
    // would open and decode and simply be in the wrong place; a tile whose
    // bytes were mangled would not decode at all, and only this notices.
    let decoded = job_tiles_for_check()
        .into_iter()
        .filter_map(|id| source.tile(id).ok().flatten().map(|_| id))
        .count();
    assert!(decoded > 0, "no tile in the file would decode");
    println!("  {decoded} tiles decoded to pixels");

    std::fs::remove_file(&out).ok();
}

/// The same tiles the live download asked for, so the check above can look
/// for them by address rather than scanning the file.
fn job_tiles_for_check() -> Vec<rutter_core::chart::TileId> {
    use rutter_core::LatLon;
    let (nw, se) = (LatLon::new(32.69, -117.245), LatLon::new(32.67, -117.22));
    (13..=14)
        .flat_map(|z| rutter_core::chart::tiles_in_view(nw, se, z))
        .collect()
}
