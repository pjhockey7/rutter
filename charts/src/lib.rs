//! Chart data: reading MBTiles and turning vector tiles into drawable
//! geometry.
//!
//! Separate from `rutter-core` because it needs SQLite and a decompressor,
//! and core's whole value is having no dependencies to fail to
//! cross-compile. Separate from `rutter-render` because deciding *what* a
//! chart contains is not the same job as putting triangles on a GPU, and
//! only one of those two can be tested without a screen.
//!
//! The pipeline, end to end:
//!
//! ```text
//! ENC/S-57, Shapefile, GeoJSON     (someone else's data)
//!   → ogr2ogr → tippecanoe         (`rutter charts`)
//!   → MBTiles                      SQLite + gzipped protobuf
//!   → mbtiles::MbTiles             this crate: query, gunzip
//!   → mvt::Tile                    this crate: protobuf, geometry
//!   → ChartTile                    this crate: world coordinates
//!   → rutter-render                lines on a surface
//! ```
//!
//! The conversion worth naming is the last one here. MVT geometry is in
//! tile-local integers, 0..extent, relative to one tile's corner.
//! Everything above the renderer speaks normalised world coordinates — the
//! 0..1 Web Mercator square. [`ChartTile`] does that conversion once, on
//! load, so the render path per frame is a buffer copy rather than
//! arithmetic over every point.

// The workspace bans `unwrap` and `expect` because a panic is a crash on a
// phone in the rain with no stack trace. Inside tests a panic IS the failure
// report, so they are allowed there and nowhere else. The integration tests
// in tests/ are a separate crate and carry their own copy of this.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod mbtiles;
pub mod mvt;

pub use mbtiles::{MbTiles, Metadata};

use rutter_core::chart::{self, TileId, WorldXY};
use rutter_core::View;

/// One tile's geometry, converted to world coordinates and flattened into
/// the two things the renderer can draw.
///
/// Polygons arrive as their ring outlines rather than filled areas. Filling
/// needs triangulation, which is a real piece of work and not yet done — and
/// a chart drawn as outlines is legible in the meantime, because depth areas
/// and coastlines are mostly *read* as their boundaries anyway.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChartTile {
    pub id: TileId,
    /// Polylines: linestrings as they came, plus one entry per polygon ring.
    pub lines: Vec<Vec<WorldXY>>,
    /// Point features — soundings, buoys, beacons.
    pub points: Vec<WorldXY>,
}

impl ChartTile {
    /// Convert a decoded vector tile into world coordinates.
    ///
    /// `layers` filters by layer name; an empty slice takes everything.
    /// Chart data is layered by S-57 object class, and a renderer usually
    /// wants only some of them.
    pub fn from_mvt(id: TileId, tile: &mvt::Tile, layers: &[&str]) -> Self {
        let mut out = Self {
            id,
            lines: Vec::new(),
            points: Vec::new(),
        };

        for layer in &tile.layers {
            if !layers.is_empty() && !layers.contains(&layer.name.as_str()) {
                continue;
            }

            // Tile-local integers to world units. The tile's own corner,
            // plus the local coordinate as a fraction of the tile's extent,
            // all divided by the number of tiles across the world.
            let grid = f64::from(TileId::grid_size(id.z));
            let extent = f64::from(layer.extent.max(1));
            let to_world = |p: [i32; 2]| WorldXY {
                x: (f64::from(id.x) + f64::from(p[0]) / extent) / grid,
                y: (f64::from(id.y) + f64::from(p[1]) / extent) / grid,
            };

            for feature in &layer.features {
                match &feature.geometry {
                    mvt::Geometry::Points(points) => {
                        out.points.extend(points.iter().copied().map(to_world));
                    }
                    mvt::Geometry::Lines(lines) => {
                        for line in lines {
                            if line.len() >= 2 {
                                out.lines.push(line.iter().copied().map(to_world).collect());
                            }
                        }
                    }
                    mvt::Geometry::Polygons(polygons) => {
                        for polygon in polygons {
                            for ring in polygon.rings() {
                                if ring.len() >= 2 {
                                    out.lines.push(ring.iter().copied().map(to_world).collect());
                                }
                            }
                        }
                    }
                    mvt::Geometry::Unknown => {}
                }
            }
        }

        out
    }

    /// Whether there is anything to draw.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.points.is_empty()
    }

    /// Total vertices across every polyline, for sizing a GPU buffer.
    pub fn line_vertex_count(&self) -> usize {
        self.lines.iter().map(Vec::len).sum()
    }
}

/// An open chart file plus a decoded-tile cache.
///
/// The cache is why this type exists rather than calling [`MbTiles`]
/// directly. A pan redraws the same tiles every frame; decoding gzip and
/// protobuf 60 times a second for geometry that has not changed would burn
/// the battery this app is supposed to respect.
#[derive(Debug)]
pub struct ChartSource {
    file: MbTiles,
    cache: Vec<(TileId, ChartTile)>,
    capacity: usize,
    layers: Vec<String>,
}

impl ChartSource {
    /// Tiles held decoded. A phone viewport covers a few dozen; this leaves
    /// room for a pan's worth of history without unbounded growth.
    pub const DEFAULT_CAPACITY: usize = 128;

    /// Most tiles [`Self::collect`] will decode for one view.
    ///
    /// Sized to separate two cases that differ by orders of magnitude. At a
    /// matching zoom the count is the viewport in 256-pixel steps: a phone
    /// wants ~20, a 4K display 144, an 8K display about 510. When the
    /// requested zoom is far below the file's range the count is the whole
    /// world at the file's zoom — thousands to a million.
    ///
    /// 1024 sits in the gap. No real display reaches it; a zoom mismatch of
    /// even three levels sails past it.
    pub const MAX_TILES: usize = 1024;

    /// Open a chart file.
    ///
    /// `layers` names the layers to keep, or empty for all.
    pub fn open(
        path: impl AsRef<std::path::Path>,
        layers: &[&str],
    ) -> Result<Self, mbtiles::Error> {
        let file = MbTiles::open(path)?;
        Ok(Self {
            file,
            cache: Vec::new(),
            capacity: Self::DEFAULT_CAPACITY,
            layers: layers.iter().map(|s| (*s).to_owned()).collect(),
        })
    }

    pub fn metadata(&self) -> &Metadata {
        self.file.metadata()
    }

    pub fn clamp_zoom(&self, zoom: u8) -> u8 {
        self.file.clamp_zoom(zoom)
    }

    /// Get a tile, decoding and caching it if needed.
    ///
    /// A tile that is absent from the file is cached as empty, so a viewport
    /// over open water does not re-query SQLite every frame for tiles that
    /// will never exist.
    pub fn tile(&mut self, id: TileId) -> Result<&ChartTile, mbtiles::Error> {
        if let Some(index) = self.cache.iter().position(|(cached, _)| *cached == id) {
            // Move to the back, so eviction takes the least recently used.
            let entry = self.cache.remove(index);
            self.cache.push(entry);
            return Ok(&self.cache[self.cache.len() - 1].1);
        }

        let names: Vec<&str> = self.layers.iter().map(String::as_str).collect();
        let decoded = match self.file.tile(id)? {
            Some(tile) => ChartTile::from_mvt(id, &tile, &names),
            None => ChartTile {
                id,
                ..ChartTile::default()
            },
        };

        if self.cache.len() >= self.capacity {
            self.cache.remove(0);
        }
        self.cache.push((id, decoded));
        Ok(&self.cache[self.cache.len() - 1].1)
    }

    /// Decoded tiles currently held.
    pub fn cached(&self) -> usize {
        self.cache.len()
    }

    /// Drop everything cached. For a chart file being swapped underneath.
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    /// Gather every visible tile's geometry for one view.
    ///
    /// This lives here rather than in each platform shim because it is the
    /// same work on every target — pick the tiles, clamp the zoom to what the
    /// file holds, concatenate the geometry — and because here it can be
    /// tested against a real chart file without a window.
    ///
    /// A tile that fails to decode is logged by its caller and skipped: one
    /// corrupt blob in the middle of a file still leaves the rest navigable.
    ///
    /// If the viewport covers more than [`Self::MAX_TILES`] tiles, nothing is
    /// decoded and `over_budget` says how many were wanted. See the constant
    /// for why that case exists at all.
    pub fn collect(&mut self, view: &View) -> Collected {
        // Ask for a zoom the file actually holds. Without this, zooming past
        // the file's maximum silently empties the screen.
        let zoom = self.clamp_zoom(view.tile_zoom());
        let (nw, se) = view.bounds();
        let tiles = chart::tiles_in_view(nw, se, zoom);

        let mut out = Collected {
            tiles,
            lines: Vec::new(),
            points: Vec::new(),
            failed: Vec::new(),
            over_budget: None,
        };

        // Clamping the zoom *up* — a view at zoom 3 against a harbour chart
        // whose minimum is 10 — asks for every zoom-10 tile across a
        // continent-wide viewport, which is tens of thousands of SQLite
        // queries. Refusing is the only safe answer: the alternative is a
        // frozen app, and a harbour chart at continent zoom has nothing
        // legible to show anyway.
        if out.tiles.len() > Self::MAX_TILES {
            out.over_budget = Some(out.tiles.len());
            return out;
        }

        for id in &out.tiles {
            match self.file.tile(*id) {
                Ok(Some(tile)) => {
                    let names: Vec<&str> = self.layers.iter().map(String::as_str).collect();
                    let decoded = ChartTile::from_mvt(*id, &tile, &names);
                    out.lines.extend(decoded.lines);
                    out.points.extend(decoded.points);
                }
                Ok(None) => {}
                Err(e) => out.failed.push((*id, e.to_string())),
            }
        }

        out
    }
}

/// The geometry of one viewport's worth of tiles.
#[derive(Debug, Clone, Default)]
pub struct Collected {
    /// The tiles asked for, in the order they were requested. Compare this
    /// against the previous call to decide whether a reupload is needed —
    /// most of a drag stays within the same tiles.
    pub tiles: Vec<TileId>,
    pub lines: Vec<Vec<WorldXY>>,
    pub points: Vec<WorldXY>,
    /// Tiles that would not decode, with the reason. Empty in the normal
    /// case; non-empty means the file is damaged, not that the view is wrong.
    pub failed: Vec<(TileId, String)>,
    /// How many tiles the viewport wanted, when that exceeded
    /// [`ChartSource::MAX_TILES`] and nothing was decoded.
    pub over_budget: Option<usize>,
}

impl Collected {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.points.is_empty()
    }

    /// The polylines as slices, which is the shape a renderer wants.
    pub fn line_slices(&self) -> Vec<&[WorldXY]> {
        self.lines.iter().map(Vec::as_slice).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point_tile(local: [i32; 2], extent: u32) -> mvt::Tile {
        mvt::Tile {
            layers: vec![mvt::Layer {
                name: "chart".into(),
                version: 2,
                extent,
                features: vec![mvt::Feature {
                    id: None,
                    geometry: mvt::Geometry::Points(vec![local]),
                    attributes: vec![],
                }],
            }],
        }
    }

    fn approx(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() < tol, "expected {b}, got {a}");
    }

    #[test]
    fn tile_origin_maps_to_the_tile_corner() {
        // Local (0,0) is the tile's own north-west corner.
        let id = TileId { z: 2, x: 1, y: 3 };
        let chart = ChartTile::from_mvt(id, &point_tile([0, 0], 4096), &[]);

        assert_eq!(chart.points.len(), 1);
        approx(chart.points[0].x, 1.0 / 4.0, 1e-12);
        approx(chart.points[0].y, 3.0 / 4.0, 1e-12);
    }

    #[test]
    fn tile_extent_maps_to_the_next_corner() {
        // Local (extent, extent) is the south-east corner, i.e. the origin
        // of the tile one along and one down.
        let id = TileId { z: 2, x: 1, y: 3 };
        let chart = ChartTile::from_mvt(id, &point_tile([4096, 4096], 4096), &[]);

        approx(chart.points[0].x, 2.0 / 4.0, 1e-12);
        approx(chart.points[0].y, 4.0 / 4.0, 1e-12);
    }

    #[test]
    fn a_non_default_extent_scales_correctly() {
        // Half way across a tile, whatever the extent claims to be.
        let id = TileId { z: 1, x: 0, y: 0 };
        for extent in [256u32, 512, 4096, 16384] {
            let half = (extent / 2) as i32;
            let chart = ChartTile::from_mvt(id, &point_tile([half, half], extent), &[]);
            approx(chart.points[0].x, 0.25, 1e-12);
            approx(chart.points[0].y, 0.25, 1e-12);
        }
    }

    #[test]
    fn zoom_zero_spans_the_whole_world() {
        let id = TileId { z: 0, x: 0, y: 0 };
        let chart = ChartTile::from_mvt(id, &point_tile([4096, 4096], 4096), &[]);
        approx(chart.points[0].x, 1.0, 1e-12);
        approx(chart.points[0].y, 1.0, 1e-12);
    }

    #[test]
    fn layer_filter_selects() {
        let tile = mvt::Tile {
            layers: vec![
                mvt::Layer {
                    name: "COALNE".into(),
                    version: 2,
                    extent: 4096,
                    features: vec![mvt::Feature {
                        id: None,
                        geometry: mvt::Geometry::Points(vec![[0, 0]]),
                        attributes: vec![],
                    }],
                },
                mvt::Layer {
                    name: "SOUNDG".into(),
                    version: 2,
                    extent: 4096,
                    features: vec![mvt::Feature {
                        id: None,
                        geometry: mvt::Geometry::Points(vec![[1, 1]]),
                        attributes: vec![],
                    }],
                },
            ],
        };
        let id = TileId { z: 0, x: 0, y: 0 };

        assert_eq!(
            ChartTile::from_mvt(id, &tile, &[]).points.len(),
            2,
            "empty filter takes all"
        );
        assert_eq!(ChartTile::from_mvt(id, &tile, &["COALNE"]).points.len(), 1);
        assert_eq!(ChartTile::from_mvt(id, &tile, &["SOUNDG"]).points.len(), 1);
        assert!(ChartTile::from_mvt(id, &tile, &["DEPARE"]).is_empty());
    }

    #[test]
    fn polygon_rings_become_lines() {
        let tile = mvt::Tile {
            layers: vec![mvt::Layer {
                name: "DEPARE".into(),
                version: 2,
                extent: 4096,
                features: vec![mvt::Feature {
                    id: None,
                    geometry: mvt::Geometry::Polygons(vec![mvt::Polygon {
                        exterior: vec![[0, 0], [100, 0], [100, 100], [0, 100], [0, 0]],
                        holes: vec![vec![[10, 10], [10, 20], [20, 20], [20, 10], [10, 10]]],
                    }]),
                    attributes: vec![],
                }],
            }],
        };

        let chart = ChartTile::from_mvt(TileId { z: 0, x: 0, y: 0 }, &tile, &[]);
        assert_eq!(chart.lines.len(), 2, "exterior and hole are both drawn");
        assert_eq!(chart.line_vertex_count(), 10);
        assert!(chart.points.is_empty());
    }

    #[test]
    fn degenerate_lines_are_dropped() {
        // A single-point "line" has nothing to draw between.
        let tile = mvt::Tile {
            layers: vec![mvt::Layer {
                name: "x".into(),
                version: 2,
                extent: 4096,
                features: vec![mvt::Feature {
                    id: None,
                    geometry: mvt::Geometry::Lines(vec![vec![[5, 5]]]),
                    attributes: vec![],
                }],
            }],
        };
        assert!(ChartTile::from_mvt(TileId { z: 0, x: 0, y: 0 }, &tile, &[]).is_empty());
    }

    #[test]
    fn unknown_geometry_contributes_nothing() {
        let tile = mvt::Tile {
            layers: vec![mvt::Layer {
                name: "x".into(),
                version: 2,
                extent: 4096,
                features: vec![mvt::Feature {
                    id: None,
                    geometry: mvt::Geometry::Unknown,
                    attributes: vec![],
                }],
            }],
        };
        assert!(ChartTile::from_mvt(TileId { z: 0, x: 0, y: 0 }, &tile, &[]).is_empty());
    }

    #[test]
    fn a_zero_extent_layer_does_not_divide_by_zero() {
        // mvt rejects extent 0 at decode, but ChartTile takes an already
        // built Tile and must not trust it.
        let id = TileId { z: 0, x: 0, y: 0 };
        let chart = ChartTile::from_mvt(id, &point_tile([1, 1], 0), &[]);
        assert!(chart.points[0].x.is_finite());
        assert!(chart.points[0].y.is_finite());
    }
}
