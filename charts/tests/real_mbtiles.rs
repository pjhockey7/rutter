//! End-to-end tests against a real MBTiles file.
//!
//! `tests/data/coast.mbtiles` is produced by `tests/data/regenerate.sh` from
//! the per-layer GeoJSON in `tests/data/src/`, committed beside it so the
//! fixture can be rebuilt and the two cannot quietly drift. 32 KB.
//!
//! The sources are named after S-57 object classes — COALNE, LNDARE, DEPARE,
//! SOUNDG, BOYSPP — because the fixture's job is to look like what a real
//! ENC cell produces: several named layers, each with its own attributes. A
//! single-layer fixture let a layer-flattening bug sit unnoticed in the
//! pipeline for a while, which is why it is shaped this way now.
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
use rutter_core::chart::{self, ChartStyle, TileId};
use rutter_core::{LatLon, View};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/coast.mbtiles");

/// The sources' combined extent, from their own coordinates. LNDARE reaches
/// furthest north, DEPARE furthest south.
const WEST: f64 = -5.10;
const EAST: f64 = -4.95;
const SOUTH: f64 = 50.05;
const NORTH: f64 = 50.145;

fn open() -> MbTiles {
    MbTiles::open(FIXTURE).expect("fixture should open")
}

#[test]
fn opens_a_file_whose_tiles_table_is_a_view() {
    // tippecanoe writes `map` and `images` tables with a `tiles` view over
    // them, so that identical tiles are stored once. A reader that checks
    // for a `tiles` *table* rejects this file.
    let db = open();
    assert!(db.tile_count().expect("count") > 0);
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
    // Several named layers, not one flattened blob. This is the assertion
    // that would have caught the pipeline bug.
    assert!(
        tile.layers.len() > 1,
        "expected several S-57 layers, got {:?}",
        tile.layers.iter().map(|l| &l.name).collect::<Vec<_>>()
    );
    for layer in &tile.layers {
        assert_eq!(layer.version, 2);
        assert_eq!(layer.extent, 4096);
        assert!(!layer.features.is_empty(), "{} is empty", layer.name);
    }
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
                    if let Some(v) = feature.attribute("DRVAL1").and_then(mvt::Value::as_f64) {
                        // Two depth areas: the 5-10 m band and the 10-20 m one.
                        assert!(
                            v == 5.0 || v == 10.0,
                            "DRVAL1 should round-trip exactly, got {v}"
                        );
                        found_drval = true;
                    }
                    if let Some(v) = feature.attribute("DEPTH").and_then(mvt::Value::as_f64) {
                        assert!(
                            [7.3, 12.1, 18.4, 4.2].iter().any(|e| (e - v).abs() < 1e-6),
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
                .flat_map(|l| l.points.iter().copied())
                .chain(chart.points.iter().map(|p| p.at));

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
fn a_layer_filter_selects_one_class() {
    let centre = LatLon::new((NORTH + SOUTH) / 2.0, (EAST + WEST) / 2.0);

    // DEPARE is present, so filtering to it must leave only depth areas.
    let mut only_depths = ChartSource::open(FIXTURE, &["DEPARE"]).expect("open");
    let got = only_depths.collect(&View::new(centre, 12.0, 1280, 800));
    assert!(!got.is_empty(), "DEPARE should be in the fixture");
    assert_eq!(
        got.styles(),
        vec![ChartStyle::DepthArea],
        "filtering to DEPARE should leave only depth areas"
    );

    // And a class that is absent must yield nothing.
    let mut nonsense = ChartSource::open(FIXTURE, &["NOSUCHLAYER"]).expect("open");
    let none = nonsense.collect(&View::new(centre, 12.0, 1280, 800));
    assert!(none.is_empty(), "an absent layer should yield nothing");
}

#[test]
fn s57_layer_names_are_classified_into_styles() {
    use rutter_charts::style_for_layer;

    // The fixture's own layers, which is what actually gets drawn.
    assert_eq!(style_for_layer("COALNE"), ChartStyle::Coastline);
    assert_eq!(style_for_layer("LNDARE"), ChartStyle::Land);
    assert_eq!(style_for_layer("DEPARE"), ChartStyle::DepthArea);
    assert_eq!(style_for_layer("SOUNDG"), ChartStyle::Sounding);
    assert_eq!(style_for_layer("BOYSPP"), ChartStyle::Buoy);

    // Buoy and beacon categories collapse to one family each.
    for b in ["BOYLAT", "BOYCAR", "BOYISD", "BOYSAW", "BOYINB"] {
        assert_eq!(style_for_layer(b), ChartStyle::Buoy, "{b}");
    }
    for b in ["BCNLAT", "BCNCAR", "BCNISD", "BCNSPP"] {
        assert_eq!(style_for_layer(b), ChartStyle::Beacon, "{b}");
    }

    // The classes that matter most.
    assert_eq!(style_for_layer("WRECKS"), ChartStyle::Danger);
    assert_eq!(style_for_layer("OBSTRN"), ChartStyle::Danger);
    assert_eq!(style_for_layer("UWTROC"), ChartStyle::Danger);
    assert_eq!(style_for_layer("LIGHTS"), ChartStyle::Light);
    assert_eq!(style_for_layer("DEPCNT"), ChartStyle::DepthContour);

    // Case-insensitive, because tooling disagrees about it.
    assert_eq!(style_for_layer("coalne"), ChartStyle::Coastline);
    assert_eq!(style_for_layer("CoAlNe"), ChartStyle::Coastline);

    // Unknown names still draw, just undistinguished — and a name longer
    // than the classifier's internal buffer must not panic or, worse,
    // misclassify into something that looks safe.
    assert_eq!(style_for_layer("SOMETHINGELSE"), ChartStyle::Other);
    assert_eq!(style_for_layer(""), ChartStyle::Other);
    assert_eq!(style_for_layer(&"X".repeat(200)), ChartStyle::Other);
}

#[test]
fn the_fixture_carries_every_style_and_keeps_draw_order() {
    let mut source = ChartSource::open(FIXTURE, &[]).expect("open");
    let centre = LatLon::new((NORTH + SOUTH) / 2.0, (EAST + WEST) / 2.0);
    let got = source.collect(&View::new(centre, 12.0, 1280, 800));

    let styles = got.styles();
    for expected in [
        ChartStyle::Land,
        ChartStyle::DepthArea,
        ChartStyle::Coastline,
        ChartStyle::Sounding,
        ChartStyle::Buoy,
    ] {
        assert!(
            styles.contains(&expected),
            "{expected:?} missing from {styles:?}"
        );
    }

    // Draw order must be non-decreasing, or the renderer paints land over
    // the marks that sit on top of it.
    let orders: Vec<u8> = got.lines.iter().map(|l| l.style.depth_order()).collect();
    assert!(
        orders.windows(2).all(|w| w[0] <= w[1]),
        "lines are not in draw order: {orders:?}"
    );

    // Soundings must carry their depths the whole way through.
    let depths: Vec<f64> = got
        .points
        .iter()
        .filter(|p| p.style == ChartStyle::Sounding)
        .filter_map(|p| p.value)
        .collect();
    assert!(!depths.is_empty(), "soundings should arrive with depths");
    for d in depths {
        assert!(
            [7.3, 12.1, 18.4, 4.2].iter().any(|e| (e - d).abs() < 1e-6),
            "unexpected sounding depth {d}"
        );
    }
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

    // Every vertex must be inside the unit square and near the source data.
    const SLACK: f64 = 0.25;
    for world in got
        .lines
        .iter()
        .flat_map(|l| l.points.iter())
        .chain(got.points.iter().map(|p| &p.at))
    {
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

#[test]
fn metadata_lists_the_layers_in_the_file() {
    let meta = open().metadata().clone();

    let names: Vec<&str> = meta.layers.iter().map(|l| l.name.as_str()).collect();
    for expected in ["COALNE", "LNDARE", "DEPARE", "SOUNDG", "BOYSPP"] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
    assert_eq!(meta.layers.len(), 5, "fixture has five layers: {names:?}");

    // Attributes come through too, which is what a picker shows as detail
    // and what a label will eventually read.
    let depare = meta
        .layers
        .iter()
        .find(|l| l.name == "DEPARE")
        .expect("DEPARE should be listed");
    assert!(
        depare.fields.iter().any(|f| f == "DRVAL1"),
        "DEPARE fields were {:?}",
        depare.fields
    );
}

#[test]
fn toggling_a_layer_off_hides_only_that_layer() {
    let mut source = ChartSource::open(FIXTURE, &[]).expect("open");
    let centre = LatLon::new((NORTH + SOUTH) / 2.0, (EAST + WEST) / 2.0);
    let view = View::new(centre, 12.0, 1280, 800);

    let before = source.collect(&view).styles();
    assert!(before.contains(&ChartStyle::Sounding));
    assert!(before.contains(&ChartStyle::Coastline));

    // Switching SOUNDG off from the implicit "all layers" state must not
    // take everything else with it.
    assert!(
        source.toggle_layer("SOUNDG", false),
        "should report a change"
    );
    let after = source.collect(&view).styles();
    assert!(
        !after.contains(&ChartStyle::Sounding),
        "soundings should be hidden, got {after:?}"
    );
    assert!(
        after.contains(&ChartStyle::Coastline),
        "the coastline should survive, got {after:?}"
    );

    // And back on again.
    assert!(source.toggle_layer("SOUNDG", true));
    let restored = source.collect(&view).styles();
    assert!(restored.contains(&ChartStyle::Sounding));
}

#[test]
fn a_redundant_toggle_reports_no_change() {
    let mut source = ChartSource::open(FIXTURE, &[]).expect("open");
    assert!(
        source.toggle_layer("SOUNDG", false),
        "first off is a change"
    );
    assert!(
        !source.toggle_layer("SOUNDG", false),
        "switching it off twice is not a change"
    );
}

#[test]
fn changing_the_filter_invalidates_the_cache() {
    // The bug this guards: tiles cached under the old filter keep their old
    // geometry, so a picker appears to do nothing until the view moves.
    let mut source = ChartSource::open(FIXTURE, &[]).expect("open");
    let centre = LatLon::new((NORTH + SOUTH) / 2.0, (EAST + WEST) / 2.0);
    let view = View::new(centre, 12.0, 1280, 800);

    source.collect(&view);
    assert!(
        source.cached() > 0,
        "tiles should be cached after a collect"
    );

    source.set_layers(&["COALNE".to_owned()]);
    assert_eq!(
        source.cached(),
        0,
        "the cache must be dropped on a filter change"
    );

    let only_coast = source.collect(&view).styles();
    assert_eq!(only_coast, vec![ChartStyle::Coastline]);
}

#[test]
fn the_tile_range_script_agrees_with_core() {
    // `scripts/tile-range.py` reimplements Web Mercator so the imagery
    // fetcher can enumerate tiles before anything is compiled. Two
    // implementations of the same projection is a real hazard: if they
    // disagree, the fetcher downloads tiles the renderer never asks for and
    // the imagery silently has holes. This pins them together.
    use std::process::Command;

    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/tile-range.py");

    // The San Diego Bay ENC extent, at a zoom with a manageable tile count.
    let (south, west, north, east, zoom) = (32.594, -117.253, 32.744, -117.090, 13u8);

    let output = Command::new("python3")
        .arg(script)
        .args([
            south.to_string(),
            west.to_string(),
            north.to_string(),
            east.to_string(),
            zoom.to_string(),
        ])
        .output()
        .expect("python3 should be on PATH inside the dev shell");

    assert!(
        output.status.success(),
        "tile-range.py failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut from_script: Vec<TileId> = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(f.len(), 4, "expected 'z x y row', got {line:?}");
        let (z, x, y, row) = (
            f[0].parse::<u8>().expect("zoom"),
            f[1].parse::<u32>().expect("x"),
            f[2].parse::<u32>().expect("y"),
            f[3].parse::<u32>().expect("row"),
        );
        // The script also emits the TMS row; it must match the flip core's
        // reader applies, or the fetcher writes rows the reader cannot find.
        assert_eq!(
            row,
            TileId::grid_size(z) - 1 - y,
            "script's TMS row disagrees with 2^z - 1 - y"
        );
        from_script.push(TileId { z, x, y });
    }

    let from_core = chart::tiles_in_view(LatLon::new(north, west), LatLon::new(south, east), zoom);

    assert!(!from_script.is_empty(), "the script produced no tiles");
    assert_eq!(
        from_script, from_core,
        "the script and rutter-core disagree about which tiles cover the box"
    );
}

// ── Raster imagery ──────────────────────────────────────────────────────────
//
// These run against `build/charts/imagery.mbtiles`, which `rutter imagery`
// produces from USGS NAIP. It is not committed — aerial imagery is megabytes
// and regenerable — so they skip when it is absent rather than fail. Build it
// with:
//
//     rutter noaa US5CA72M
//     rutter imagery --chart build/charts/noaa.mbtiles --zoom 13

fn imagery_path() -> Option<std::path::PathBuf> {
    let path = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../build/charts/imagery.mbtiles"
    ))
    .to_path_buf();
    path.exists().then_some(path)
}

#[test]
fn reads_real_usgs_imagery() {
    use rutter_charts::ImagerySource;

    let Some(path) = imagery_path() else {
        eprintln!("skipping: no build/charts/imagery.mbtiles — see the note above");
        return;
    };

    let mut source = ImagerySource::open(&path).expect("imagery should open");
    let meta = source.metadata().clone();

    assert!(
        meta.is_raster(),
        "format {:?} should be raster",
        meta.format
    );
    assert!(!meta.is_vector());
    assert!(
        meta.attribution
            .as_deref()
            .is_some_and(|a| a.contains("USGS")),
        "the USGS credit should be recorded: {:?}",
        meta.attribution
    );

    // The tile over the mouth of San Diego Bay.
    let id = TileId {
        z: 13,
        x: 1428,
        y: 3308,
    };
    let tile = source
        .tile(id)
        .expect("decode")
        .expect("that tile was downloaded");

    assert_eq!(tile.id, id);
    assert_eq!((tile.width, tile.height), (256, 256));
    assert!(
        tile.is_consistent(),
        "RGBA buffer length must match the size"
    );

    // Aerial imagery of a harbour is not a blank frame. A decoder that
    // silently produced zeros would pass every structural check above.
    let distinct: std::collections::HashSet<[u8; 3]> = tile
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| [p[0], p[1], p[2]])
        .collect();
    assert!(
        distinct.len() > 100,
        "only {} distinct colours — this does not look like a photograph",
        distinct.len()
    );

    // Every pixel opaque: these are JPEGs with no alpha.
    assert!(
        tile.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255),
        "imagery should decode fully opaque"
    );
}

#[test]
fn imagery_rejects_a_vector_file() {
    use rutter_charts::ImagerySource;

    // Handing the vector chart to the imagery reader must say so plainly
    // rather than open and then draw nothing.
    let err = ImagerySource::open(FIXTURE).expect_err("a vector file is not imagery");
    assert!(
        matches!(err, rutter_charts::mbtiles::Error::NotRaster(_)),
        "expected NotRaster, got {err}"
    );
}

#[test]
fn imagery_collect_covers_a_view_and_caches() {
    use rutter_charts::ImagerySource;
    use rutter_core::View;

    let Some(path) = imagery_path() else {
        eprintln!("skipping: no build/charts/imagery.mbtiles");
        return;
    };

    let mut source = ImagerySource::open(&path).expect("open");
    let view = View::new(LatLon::new(32.69, -117.20), 13.0, 1280, 800);

    let tiles = source.collect(&view);
    assert!(!tiles.is_empty(), "the view is over the downloaded area");
    assert!(tiles.iter().all(|t| t.is_consistent()));
    assert!(source.cached() > 0, "decoded tiles should be cached");

    // A second pass must be served from the cache, not re-decoded.
    let before = source.cached();
    let again = source.collect(&view);
    assert_eq!(tiles.len(), again.len());
    assert_eq!(
        source.cached(),
        before,
        "a repeat view should not grow the cache"
    );
}

#[test]
fn imagery_survives_zooming_out() {
    use rutter_charts::ImagerySource;
    use rutter_core::View;

    let Some(path) = imagery_path() else {
        eprintln!("skipping: no build/charts/imagery.mbtiles");
        return;
    };

    let mut source = ImagerySource::open(&path).expect("open");
    let deepest = source.metadata().max_zoom;

    // Zooming out used to blank the imagery. With only the deepest level in
    // the file, a shallower view clamps back up to it, the viewport then
    // covers hundreds of tiles at that zoom, `collect` goes over its budget
    // and returns nothing — sea colour and a debug log. `rutter imagery`
    // fetches a pyramid down to zoom 6 so each level is drawn at its own
    // scale, and this is the assertion that says so.
    //
    // Four levels out from the deepest is a 16x16-tile area at the deepest
    // zoom: comfortably over the budget, so a single-level file fails here.
    for out in 1..=4 {
        let zoom = f64::from(deepest.saturating_sub(out));
        let view = View::new(LatLon::new(32.69, -117.20), zoom, 1280, 800);
        let tiles = source.collect(&view);

        assert!(
            !tiles.is_empty(),
            "zoom {zoom} (level {out} out from {deepest}) drew no imagery — \
             is the file a single level? refetch with `rutter imagery`"
        );
        assert!(
            tiles.len() <= ImagerySource::MAX_TILES,
            "zoom {zoom} wanted {} tiles, over the budget",
            tiles.len()
        );
    }
}

#[test]
fn imagery_survives_zooming_in_past_the_deepest_level() {
    use rutter_charts::ImagerySource;
    use rutter_core::View;

    let Some(path) = imagery_path() else {
        eprintln!("skipping: no build/charts/imagery.mbtiles");
        return;
    };

    let mut source = ImagerySource::open(&path).expect("open");
    let deepest = source.metadata().max_zoom;

    // The other half of the same problem. Past the deepest stored level
    // there is nothing more to fetch, so the right behaviour is to keep
    // drawing the deepest tiles magnified — blurry, but a blurry photograph
    // of the right place beats no photograph. The clamp is what makes that
    // happen, and dropping it would blank the imagery exactly when you have
    // zoomed in to look at something.
    for past in 1..=3 {
        let zoom = f64::from(deepest + past);
        let view = View::new(LatLon::new(32.69, -117.20), zoom, 1280, 800);
        let tiles = source.collect(&view);

        assert!(
            !tiles.is_empty(),
            "zoom {zoom} ({past} past the deepest level {deepest}) drew no imagery"
        );
        assert!(
            tiles.iter().all(|t| t.id.z == deepest),
            "tiles past the deepest level should come from it, magnified"
        );
    }
}
