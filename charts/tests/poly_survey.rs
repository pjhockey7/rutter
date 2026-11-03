//! The triangulator against real chart polygons.
//!
//! The unit tests in `rutter-core` use shapes chosen to exercise the
//! algorithm. These use shapes chosen by tippecanoe, which is a different
//! and less cooperative author: rings up to 520 points, repeated vertices,
//! up to six holes, and whatever tile clipping did to the boundary.
//!
//! The check is area conservation. If the triangles of a polygon sum to the
//! polygon's own area — exterior minus holes — they tile it exactly: too few
//! and there is a gap, too many and they overlap, and a hole filled over
//! shows up immediately as a surplus. That one invariant catches essentially
//! every way this can go wrong.
//!
//! Needs `build/charts/noaa.mbtiles`. Build it with:
//!
//!     rutter noaa --near 32.68,-117.23

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rutter_charts::mbtiles::MbTiles;
use rutter_charts::mvt;
use rutter_core::chart::{TileId, WorldXY};
use rutter_core::triangulate::{ring_area, triangulate};

/// The four tiles over San Diego Bay at whatever the chart's deepest zoom is.
///
/// Taken from the file rather than hard-coded. It was hard-coded to 13 at
/// first, which is what a San-Diego-only fetch produces — and the test broke
/// the moment the chart was rebuilt over a wider area, where tippecanoe
/// chooses a shallower maximum. The zoom is the chart's business, not this
/// test's.
fn bay_tiles(file: &MbTiles) -> Vec<TileId> {
    let zoom = file.metadata().max_zoom;
    let n = f64::from(1u32 << zoom);
    let lat = 32.6836_f64.to_radians();
    let x = ((-117.2206 + 180.0) / 360.0 * n) as u32;
    let y = ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n) as u32;
    vec![
        TileId { z: zoom, x, y },
        TileId {
            z: zoom,
            x: x + 1,
            y,
        },
        TileId {
            z: zoom,
            x,
            y: y + 1,
        },
        TileId {
            z: zoom,
            x: x + 1,
            y: y + 1,
        },
    ]
}

fn chart() -> Option<MbTiles> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../build/charts/noaa.mbtiles");
    std::path::Path::new(path)
        .exists()
        .then(|| MbTiles::open(path).expect("open"))
}

/// Every polygon of one layer across the bay tiles, in world coordinates.
fn polygons(file: &MbTiles, layer_name: &str) -> Vec<(Vec<WorldXY>, Vec<Vec<WorldXY>>)> {
    let mut out = Vec::new();
    for id in bay_tiles(file) {
        let Some(tile) = file.tile(id).expect("tile") else {
            continue;
        };
        for layer in &tile.layers {
            if layer.name != layer_name {
                continue;
            }
            let grid = f64::from(TileId::grid_size(id.z));
            let extent = f64::from(layer.extent.max(1));
            let to_world = |p: [i32; 2]| WorldXY {
                x: (f64::from(id.x) + f64::from(p[0]) / extent) / grid,
                y: (f64::from(id.y) + f64::from(p[1]) / extent) / grid,
            };
            for feature in &layer.features {
                if let mvt::Geometry::Polygons(ps) = &feature.geometry {
                    for poly in ps {
                        out.push((
                            poly.exterior.iter().copied().map(to_world).collect(),
                            poly.holes
                                .iter()
                                .map(|h| h.iter().copied().map(to_world).collect())
                                .collect(),
                        ));
                    }
                }
            }
        }
    }
    out
}

fn triangle_area(t: &[WorldXY; 3]) -> f64 {
    ((t[1].x - t[0].x) * (t[2].y - t[0].y) - (t[1].y - t[0].y) * (t[2].x - t[0].x)).abs() / 2.0
}

#[test]
fn real_depth_areas_are_tiled_exactly() {
    let Some(file) = chart() else {
        eprintln!("skipping: no build/charts/noaa.mbtiles — see the note above");
        return;
    };

    let polys = polygons(&file, "DEPARE");
    assert!(
        polys.len() > 50,
        "only {} depth-area polygons found",
        polys.len()
    );

    let mut dropped = 0usize;
    let mut with_holes = 0usize;
    let mut triangles = 0usize;
    let mut worst_error = 0.0f64;

    for (exterior, holes) in &polys {
        if !holes.is_empty() {
            with_holes += 1;
        }
        let expected = ring_area(exterior) - holes.iter().map(|h| ring_area(h)).sum::<f64>();
        let t = triangulate(exterior, holes);

        if t.is_empty() {
            // Dropping a polygon is the designed failure, not a test
            // failure — but it is counted, and too many would mean the
            // algorithm is not actually coping.
            dropped += 1;
            continue;
        }
        triangles += t.len();

        let got: f64 = t.iter().map(triangle_area).sum();
        // Relative, because world-unit areas down here are around 1e-11.
        let error = if expected > 0.0 {
            (got - expected).abs() / expected
        } else {
            0.0
        };
        assert!(
            error < 1e-6,
            "a {}-point polygon with {} holes came out {:.3}% wrong \
             ({got:e} against {expected:e})",
            exterior.len(),
            holes.len(),
            error * 100.0
        );
        worst_error = worst_error.max(error);
    }

    println!(
        "{} depth areas ({with_holes} with holes) -> {triangles} triangles, \
         {dropped} dropped, worst area error {:.3e}",
        polys.len(),
        worst_error
    );

    // The whole point is to fill these, so most of them have to survive.
    let kept = polys.len() - dropped;
    assert!(
        kept * 10 >= polys.len() * 9,
        "dropped {dropped} of {} polygons, which is too many to call this \
         a fill",
        polys.len()
    );
}

#[test]
fn real_land_areas_are_tiled_exactly() {
    // Land is the other big filled class, and its rings are the messiest on
    // the chart — coastline detail, islands as holes in nothing, and spurs
    // left by simplification.
    let Some(file) = chart() else {
        eprintln!("skipping: no chart");
        return;
    };

    let polys = polygons(&file, "LNDARE");
    if polys.is_empty() {
        eprintln!("skipping: no LNDARE polygons in these tiles");
        return;
    }

    let mut dropped = 0usize;
    for (exterior, holes) in &polys {
        let expected = ring_area(exterior) - holes.iter().map(|h| ring_area(h)).sum::<f64>();
        let t = triangulate(exterior, holes);
        if t.is_empty() {
            dropped += 1;
            continue;
        }
        let got: f64 = t.iter().map(triangle_area).sum();
        if expected > 0.0 {
            assert!(
                (got - expected).abs() / expected < 1e-6,
                "a {}-point land polygon came out wrong",
                exterior.len()
            );
        }
    }
    println!("{} land areas, {dropped} dropped", polys.len());
    assert!(
        dropped * 5 <= polys.len(),
        "dropped {dropped} land polygons"
    );
}

#[test]
fn triangulating_a_tile_is_fast_enough_to_do_on_load() {
    // It happens once per change of visible tile set, not per frame, but a
    // pan across a harbour changes that set often enough that a second
    // would be felt.
    let Some(file) = chart() else {
        eprintln!("skipping: no chart");
        return;
    };

    let polys = polygons(&file, "DEPARE");
    let started = std::time::Instant::now();
    let mut triangles = 0usize;
    for (exterior, holes) in &polys {
        triangles += triangulate(exterior, holes).len();
    }
    let elapsed = started.elapsed();

    println!(
        "{} polygons -> {triangles} triangles in {:?}",
        polys.len(),
        elapsed
    );
    assert!(
        elapsed.as_millis() < 500,
        "took {elapsed:?} for {} polygons",
        polys.len()
    );
}
