//! End-to-end tests against a real MBTiles file.
//!
//! `tests/data/coast.mbtiles` was produced by the project's own pipeline —
//! `tippecanoe -Z10 -z12 -l chart` over `tests/data/coast.geojson`, which is
//! committed beside it so the fixture can be regenerated and the two cannot
//! quietly drift. 32 KB, eleven tiles.
//!
//! These are deliberately not unit tests. Everything in the unit suites is
//! checked against fixtures this project built itself, which proves the
//! decoder is self-consistent and proves nothing about whether it agrees
//! with tippecanoe. These do: they assert against bytes written by a
//! different program, and they are the only tests that would catch a
//! misreading of the TMS row convention or the gzip layer.

// A panic in a test is the failure report, so the workspace's ban on
// `unwrap` and `expect` does not apply here. This is the integration-test
// crate's own copy of the allowance in charts/src/lib.rs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use rutter_charts::{mvt, ChartSource, MbTiles};
use rutter_core::chart::{self, TileId};
use rutter_core::LatLon;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/coast.mbtiles");

/// The source GeoJSON's extent, from its own coordinates.
const WEST: f64 = -5.10;
const EAST: f64 = -4.95;
const SOUTH: f64 = 50.05;
const NORTH: f64 = 50.14;

fn open() -> MbTiles {
    MbTiles::open(FIXTURE).expect("fixture should open")
}

#[test]
fn opens_a_file_whose_tiles_table_is_a_view() {
    // tippecanoe writes `map` and `images` tables with a `tiles` view over
    // them, so that identical tiles are stored once. A reader that checks
    // for a `tiles` *table* rejects this file.
    let db = open();
    assert_eq!(db.tile_count().expect("count"), 11);
}

#[test]
fn reads_the_metadata_tippecanoe_wrote() {
    let meta = open().metadata().clone();

    assert_eq!(meta.format, "pbf");
    assert!(meta.is_vector(), "pbf must be recognised as vector");
    assert_eq!(meta.min_zoom, 10);
    assert_eq!(meta.max_zoom, 12);

    let bounds = meta.bounds.expect("tippecanoe writes bounds");
    // west, south, east, north — and tippecanoe rounds to six places.
    assert!((bounds[0] - WEST).abs() < 1e-5, "west {}", bounds[0]);
    assert!((bounds[1] - SOUTH).abs() < 1e-5, "south {}", bounds[1]);
    assert!((bounds[2] - EAST).abs() < 1e-5, "east {}", bounds[2]);
    assert!((bounds[3] - NORTH).abs() < 1e-5, "north {}", bounds[3]);
}

#[test]
fn zoom_clamping_follows_the_file() {
    let db = open();
    assert_eq!(db.clamp_zoom(2), 10, "below the range");
    assert_eq!(db.clamp_zoom(11), 11, "inside the range");
    assert_eq!(
        db.clamp_zoom(18),
        12,
        "above the range — stretch, do not blank"
    );
}

#[test]
fn the_tile_containing_the_data_is_the_one_stored() {
    // This is the test that catches a TMS/XYZ mix-up. The fixture's z10
    // tiles sit at TMS rows 676 and 677, which are slippy y 347 and 346
    // (1023 − row). The source area straddles that boundary, so the exact
    // half the centroid falls in is not the interesting fact — that both
    // stored rows are reachable by the y values core computes is.
    let db = open();

    let centre = LatLon::new((NORTH + SOUTH) / 2.0, (EAST + WEST) / 2.0);
    let id = TileId::containing(centre, 10);
    assert_eq!(id.x, 497, "longitude should put the data in column 497");
    assert!(id.y == 346 || id.y == 347, "centre landed in y={}", id.y);

    let raw = db
        .raw_tile(id)
        .expect("query")
        .expect("tile should be present");
    assert!(!raw.is_empty());

    // Both stored rows, addressed as slippy y. If the flip were dropped or
    // applied twice, at most one of these could resolve.
    for y in [346, 347] {
        let id = TileId { z: 10, x: 497, y };
        assert!(
            db.raw_tile(id).expect("query").is_some(),
            "{id:?} corresponds to a stored row and should be present"
        );
    }

    // And a row the file does not hold must be absent — otherwise the two
    // assertions above would also pass for a reader that ignores y.
    assert!(
        db.raw_tile(TileId {
            z: 10,
            x: 497,
            y: 300
        })
        .expect("query")
        .is_none(),
        "y=300 is not in the file"
    );
}

#[test]
fn stored_tiles_are_gzipped_and_decode_anyway() {
    let db = open();
    let id = TileId {
        z: 10,
        x: 497,
        y: 347,
    };

    let raw = db.raw_tile(id).expect("query").expect("present");
    assert_eq!(&raw[..2], &[0x1f, 0x8b], "tippecanoe gzips vector tiles");

    // The same bytes, fed to the protobuf decoder without inflating, must
    // not be mistaken for a valid tile — otherwise the gunzip step could be
    // silently skipped and no test would notice.
    assert!(
        mvt::Tile::decode(&raw)
            .map(|t| t.layers.is_empty())
            .unwrap_or(true),
        "gzip bytes must not decode as a populated tile"
    );

    let tile = db.tile(id).expect("decode").expect("present");
    assert_eq!(tile.layers.len(), 1);
    assert_eq!(tile.layers[0].name, "chart");
    assert_eq!(tile.layers[0].version, 2);
    assert_eq!(tile.layers[0].extent, 4096);
    assert!(!tile.layers[0].features.is_empty());
}

#[test]
fn absent_tiles_are_none_not_an_error() {
    let db = open();
    // Mid-Pacific, nowhere near Cornwall.
    assert!(db
        .tile(TileId { z: 10, x: 1, y: 1 })
        .expect("query")
        .is_none());
    // Past the end of the grid entirely.
    assert!(db
        .tile(TileId {
            z: 10,
            x: 9999,
            y: 9999
        })
        .expect("query")
        .is_none());
    // A zoom the file does not hold.
    assert!(db
        .tile(TileId {
            z: 14,
            x: 7961,
            y: 5552
        })
        .expect("query")
        .is_none());
}

#[test]
fn every_geometry_type_from_the_source_survives_the_round_trip() {
    // The source has a LineString, a Polygon and two Points. They are spread
    // across tiles, so gather everything at the fixture's deepest zoom.
    let db = open();

    let mut points = 0;
    let mut lines = 0;
    let mut polygons = 0;

    for id in db.tiles_at(12).expect("list") {
        let tile = db.tile(id).expect("decode").expect("listed tiles exist");
        for layer in &tile.layers {
            for feature in &layer.features {
                match &feature.geometry {
                    mvt::Geometry::Points(p) => points += p.len(),
                    mvt::Geometry::Lines(l) => lines += l.len(),
                    mvt::Geometry::Polygons(p) => polygons += p.len(),
                    mvt::Geometry::Unknown => panic!("tippecanoe should not emit type 0"),
                }
            }
        }
    }

    assert!(points > 0, "the two soundings should appear somewhere");
    assert!(lines > 0, "the coastline should appear somewhere");
    assert!(polygons > 0, "the depth area should appear somewhere");
}

#[test]
fn attributes_survive_the_round_trip() {
    let db = open();

    // The source tags the depth area with drval1=5, drval2=10 and the
    // soundings with depths 7.3 and 12.1.
    let mut found_drval = false;
    let mut found_depth = false;

    for zoom in 10..=12 {
        for id in db.tiles_at(zoom).expect("list") {
            let tile = db.tile(id).expect("decode").expect("present");
            for layer in &tile.layers {
                for feature in &layer.features {
                    if let Some(v) = feature.attribute("drval1").and_then(mvt::Value::as_f64) {
                        assert_eq!(v, 5.0, "drval1 should round-trip exactly");
                        found_drval = true;
                    }
                    if let Some(v) = feature.attribute("depth").and_then(mvt::Value::as_f64) {
                        assert!(
                            (v - 7.3).abs() < 1e-6 || (v - 12.1).abs() < 1e-6,
                            "unexpected depth {v}"
                        );
                        found_depth = true;
                    }
                }
            }
        }
    }

    assert!(found_drval, "polygon attributes should survive");
    assert!(found_depth, "point attributes should survive");
}

#[test]
fn geometry_lands_inside_the_source_bounds() {
    // The real check on the coordinate conversion: every decoded vertex,
    // projected back to degrees, must fall within the area the source
    // GeoJSON covers. A wrong extent divisor, a missed grid division or a
    // flipped axis all put points outside this box.
    //
    // Tiles are clipped with a buffer, so simplification can push a vertex
    // slightly past the data's own edge. A quarter degree is far wider than
    // any buffer and far narrower than a neighbouring tile.
    const SLACK: f64 = 0.25;

    let db = open();
    let mut checked = 0;

    for zoom in 10..=12 {
        for id in db.tiles_at(zoom).expect("list") {
            let tile = db.tile(id).expect("decode").expect("present");
            let chart = rutter_charts::ChartTile::from_mvt(id, &tile, &[]);

            let vertices = chart
                .lines
                .iter()
                .flatten()
                .copied()
                .chain(chart.points.iter().copied());

            for world in vertices {
                assert!(
                    (0.0..=1.0).contains(&world.x) && (0.0..=1.0).contains(&world.y),
                    "world coordinate {world:?} outside the unit square"
                );

                let pos = chart::unproject(world);
                assert!(
                    pos.lon >= WEST - SLACK && pos.lon <= EAST + SLACK,
                    "longitude {} outside the source bounds (zoom {zoom})",
                    pos.lon
                );
                assert!(
                    pos.lat >= SOUTH - SLACK && pos.lat <= NORTH + SLACK,
                    "latitude {} outside the source bounds (zoom {zoom})",
                    pos.lat
                );
                checked += 1;
            }
        }
    }

    assert!(
        checked > 10,
        "expected real geometry, checked only {checked} vertices"
    );
}

#[test]
fn listed_tiles_round_trip_through_the_tms_conversion() {
    let db = open();
    for zoom in 10..=12 {
        let ids = db.tiles_at(zoom).expect("list");
        assert!(!ids.is_empty(), "zoom {zoom} should hold tiles");
        for id in ids {
            assert_eq!(id.z, zoom);
            assert!(id.is_valid(), "{id:?} is not a valid address");
            // Every listed tile must be fetchable by the address it was
            // listed under — which only holds if the flip is symmetric.
            assert!(
                db.raw_tile(id).expect("query").is_some(),
                "{id:?} was listed but cannot be fetched"
            );
        }
    }
}

#[test]
fn source_caches_decoded_tiles_and_evicts() {
    let mut source = ChartSource::open(FIXTURE, &[]).expect("open");
    let id = TileId {
        z: 10,
        x: 497,
        y: 347,
    };

    assert_eq!(source.cached(), 0);
    let first = source.tile(id).expect("decode").clone();
    assert_eq!(source.cached(), 1);

    // Second fetch must come from the cache and be identical.
    let second = source.tile(id).expect("cached").clone();
    assert_eq!(first, second);
    assert_eq!(source.cached(), 1, "a repeat fetch must not add an entry");

    // Absent tiles are cached too, so open water does not re-query SQLite
    // every frame.
    source.tile(TileId { z: 10, x: 1, y: 1 }).expect("absent");
    assert_eq!(source.cached(), 2);

    // The cache is bounded.
    for x in 0..(ChartSource::DEFAULT_CAPACITY as u32 + 20) {
        source.tile(TileId { z: 10, x, y: 500 }).expect("query");
    }
    assert!(
        source.cached() <= ChartSource::DEFAULT_CAPACITY,
        "cache grew to {} past its {} limit",
        source.cached(),
        ChartSource::DEFAULT_CAPACITY
    );

    source.clear_cache();
    assert_eq!(source.cached(), 0);
}

#[test]
fn a_layer_filter_that_matches_nothing_yields_nothing() {
    let mut source = ChartSource::open(FIXTURE, &["DEPARE"]).expect("open");
    // The fixture's single layer is called "chart", not "DEPARE".
    let tile = source
        .tile(TileId {
            z: 10,
            x: 497,
            y: 347,
        })
        .expect("decode");
    assert!(
        tile.is_empty(),
        "filter should exclude the only layer present"
    );
}

#[test]
fn rejects_a_sqlite_file_that_is_not_mbtiles() {
    let path = std::env::temp_dir().join("rutter-not-mbtiles.sqlite");
    let _ = std::fs::remove_file(&path);
    {
        let db = rusqlite::Connection::open(&path).expect("create");
        db.execute("CREATE TABLE unrelated (x INTEGER)", [])
            .expect("create table");
    }

    let err = MbTiles::open(&path).expect_err("should be rejected");
    assert!(
        matches!(err, rutter_charts::mbtiles::Error::NotMbTiles(_)),
        "expected NotMbTiles, got {err}"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn rejects_a_file_that_is_not_a_database() {
    let path = std::env::temp_dir().join("rutter-not-a-db.mbtiles");
    std::fs::write(&path, b"this is not SQLite").expect("write");
    assert!(MbTiles::open(&path).is_err());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn collect_gathers_a_viewport_from_the_real_file() {
    use rutter_core::View;

    let mut source = ChartSource::open(FIXTURE, &[]).expect("open");

    // A view over the data, at a zoom the file holds.
    let centre = LatLon::new((NORTH + SOUTH) / 2.0, (EAST + WEST) / 2.0);
    let view = View::new(centre, 12.0, 1280, 800);

    let got = source.collect(&view);

    assert!(
        !got.tiles.is_empty(),
        "a view over the data must want tiles"
    );
    assert!(
        got.failed.is_empty(),
        "nothing should fail to decode: {:?}",
        got.failed
    );
    assert!(!got.is_empty(), "the fixture's geometry should be found");
    assert_eq!(got.line_slices().len(), got.lines.len());

    // Every vertex must be inside the unit square and near the source data.
    const SLACK: f64 = 0.25;
    for world in got.lines.iter().flatten().chain(got.points.iter()) {
        let pos = chart::unproject(*world);
        assert!(
            pos.lon >= WEST - SLACK && pos.lon <= EAST + SLACK,
            "longitude {} is outside the data",
            pos.lon
        );
        assert!(
            pos.lat >= SOUTH - SLACK && pos.lat <= NORTH + SLACK,
            "latitude {} is outside the data",
            pos.lat
        );
    }
}

#[test]
fn collect_clamps_the_zoom_to_what_the_file_holds() {
    use rutter_core::View;

    let mut source = ChartSource::open(FIXTURE, &[]).expect("open");
    let centre = LatLon::new((NORTH + SOUTH) / 2.0, (EAST + WEST) / 2.0);

    // Zoom 18 is past the fixture's maximum of 12. Without clamping this
    // asks for tiles that do not exist and the screen goes blank.
    let deep = source.collect(&View::new(centre, 18.0, 1280, 800));
    assert!(
        deep.tiles.iter().all(|t| t.z == 12),
        "zoom should clamp to 12"
    );
    assert!(
        !deep.is_empty(),
        "a zoomed-in view must still draw something"
    );

    // And below the minimum. The zoom still clamps up to 10, but a zoom-3
    // viewport covers a continent, so at tile zoom 10 that is tens of
    // thousands of tiles — which must be refused rather than attempted.
    let shallow = source.collect(&View::new(centre, 3.0, 1280, 800));
    assert!(
        shallow.tiles.iter().all(|t| t.z == 10),
        "zoom should clamp to 10"
    );
    assert!(
        shallow.over_budget.is_some(),
        "a zoom-3 view of a zoom-10 chart wants {} tiles and must be refused",
        shallow.tiles.len()
    );
    assert!(
        shallow.is_empty(),
        "nothing should be decoded when over budget"
    );
}

#[test]
fn a_matching_zoom_is_well_inside_the_tile_budget() {
    use rutter_core::View;

    // The guard must not fire in normal use. A phone-shaped viewport at the
    // chart's own zoom should want a few dozen tiles, not hundreds.
    let mut source = ChartSource::open(FIXTURE, &[]).expect("open");
    let centre = LatLon::new((NORTH + SOUTH) / 2.0, (EAST + WEST) / 2.0);

    for (w, h) in [(1080u32, 2400u32), (2400, 1080), (1280, 800), (3840, 2160)] {
        let got = source.collect(&View::new(centre, 12.0, w, h));
        assert!(
            got.over_budget.is_none(),
            "{w}x{h} wanted {} tiles, over the {} budget",
            got.tiles.len(),
            ChartSource::MAX_TILES
        );
        // The viewport in 256-pixel steps, plus a tile of slack per edge.
        // 4K wants 144; this is the figure the budget has to clear.
        let expected = ((w / 256 + 2) * (h / 256 + 2)) as usize;
        assert!(
            got.tiles.len() <= expected,
            "{w}x{h} wanted {} tiles, more than the {expected} the viewport implies",
            got.tiles.len()
        );
    }
}

#[test]
fn collect_over_open_water_yields_nothing_and_no_errors() {
    use rutter_core::View;

    let mut source = ChartSource::open(FIXTURE, &[]).expect("open");
    // Mid-Atlantic, far from the fixture.
    let view = View::new(LatLon::new(30.0, -40.0), 12.0, 1280, 800);

    let got = source.collect(&view);
    assert!(!got.tiles.is_empty(), "tiles are still requested");
    assert!(got.is_empty(), "but there is no geometry there");
    assert!(got.failed.is_empty(), "absent is not an error");
}

#[test]
fn collect_is_stable_for_the_same_view() {
    use rutter_core::View;

    let mut source = ChartSource::open(FIXTURE, &[]).expect("open");
    let centre = LatLon::new((NORTH + SOUTH) / 2.0, (EAST + WEST) / 2.0);
    let view = View::new(centre, 12.0, 1280, 800);

    let first = source.collect(&view);
    let second = source.collect(&view);

    // The tile list is what the platform compares to decide whether to
    // reupload, so it must be deterministic.
    assert_eq!(first.tiles, second.tiles);
    assert_eq!(first.lines.len(), second.lines.len());
    assert_eq!(first.points.len(), second.points.len());
}
