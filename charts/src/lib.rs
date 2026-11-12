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

pub mod imagery;
pub mod mbtiles;
pub mod mvt;

pub use imagery::is_blank_tile;
pub use imagery::ImagerySource;
pub use mbtiles::{MbTiles, Metadata};

use rutter_core::chart::{self, ChartStyle, StyledArea, StyledLine, StyledPoint, TileId, WorldXY};
use rutter_core::triangulate::triangulate;
use rutter_core::View;

/// Classify an S-57 object class — or any other layer name — into a style.
///
/// S-57 defines over 170 object classes with six-letter acronyms. These are
/// the ones that carry most of a chart's meaning; everything else falls
/// through to [`ChartStyle::Other`] and still draws, just undistinguished.
///
/// Matching is case-insensitive because tooling disagrees: GDAL's S-57
/// driver gives upper case, hand-made GeoJSON often does not.
pub fn style_for_layer(name: &str) -> ChartStyle {
    // Upper-cased into a small buffer rather than allocating: this is called
    // once per layer per tile, which at a few hundred tiles is often enough
    // to matter and never enough to be interesting.
    let mut buf = [0u8; 16];
    let bytes = name.as_bytes();
    let n = bytes.len().min(buf.len());
    for i in 0..n {
        buf[i] = bytes[i].to_ascii_uppercase();
    }
    let upper = &buf[..n];

    match upper {
        b"COALNE" | b"SLCONS" => ChartStyle::Coastline,
        b"LNDARE" | b"LNDRGN" | b"LNDELV" => ChartStyle::Land,
        b"DEPARE" | b"DRGARE" | b"UNSARE" => ChartStyle::DepthArea,
        b"DEPCNT" => ChartStyle::DepthContour,
        b"SOUNDG" => ChartStyle::Sounding,
        b"LIGHTS" => ChartStyle::Light,
        b"WRECKS" | b"OBSTRN" | b"UWTROC" | b"ROCKS" => ChartStyle::Danger,
        b"ACHARE" | b"RESARE" | b"TSSLPT" | b"CBLARE" | b"PIPARE" | b"MIPARE" => ChartStyle::Area,
        // Buoys and beacons each come in half a dozen categories — lateral,
        // cardinal, isolated danger, safe water, special purpose — all of
        // which are the same symbol family to a renderer this coarse.
        _ if upper.starts_with(b"BOY") => ChartStyle::Buoy,
        _ if upper.starts_with(b"BCN") => ChartStyle::Beacon,
        _ => ChartStyle::Other,
    }
}

/// The attribute a point feature's label should come from, by style.
///
/// Soundings carry DEPTH (GDAL's S-57 driver synthesises it from the vertex
/// Z when ADD_SOUNDG_DEPTH is on); wrecks and obstructions carry VALSOU, the
/// sounded depth over them.
fn value_attribute(style: ChartStyle) -> &'static [&'static str] {
    match style {
        ChartStyle::Sounding => &["DEPTH", "depth"],
        ChartStyle::Danger => &["VALSOU", "valsou", "DEPTH", "depth"],
        ChartStyle::Light => &["HEIGHT", "height"],
        // DRVAL1 is a depth area's *shallowest* depth, which is the one
        // that decides whether a boat crosses it. DRVAL2, the deepest, is
        // not read: knowing an area bottoms out at 91 m does not help when
        // part of it is 1.8 m.
        //
        // Drying areas carry DRVAL1 negative — ground that uncovers is
        // above chart datum — and the clearance arithmetic wants exactly
        // that sign, so it is passed through unchanged.
        ChartStyle::DepthArea => &["DRVAL1", "drval1"],
        ChartStyle::DepthContour => &["VALDCO", "valdco"],
        _ => &[],
    }
}

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
    /// The names of the layers this tile's features came from, indexed by
    /// `StyledLine::layer` and friends.
    pub layer_names: Vec<String>,
    /// Areas whose polygon could not be tiled exactly, so carry no fill.
    ///
    /// Reported rather than swallowed. The triangulator refuses a ring it
    /// cannot cover precisely — a self-overlapping one, which tile clipping
    /// occasionally produces — and the result is a shoal that is outlined
    /// but not shaded. With the under-keel check on that is an unshaded
    /// hazard, so the count has to reach the navigator instead of a log.
    pub unfilled: usize,
    /// Filled regions — depth areas, land — as triangles.
    ///
    /// Triangulated here, at decode time, rather than in the renderer: it
    /// is pure geometry, so it belongs on the portable side, and doing it
    /// per tile means the result is cached with the tile instead of being
    /// recomputed on every pan.
    pub areas: Vec<StyledArea>,
    /// Polylines: linestrings as they came, plus one entry per polygon ring.
    /// Each carries the style of the layer it came from, which is the only
    /// reason the multi-layer pipeline exists.
    pub lines: Vec<StyledLine>,
    /// Point features — soundings, buoys, beacons — with their depth or
    /// height where the chart recorded one.
    pub points: Vec<StyledPoint>,
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
            layer_names: Vec::new(),
            unfilled: 0,
            areas: Vec::new(),
            lines: Vec::new(),
            points: Vec::new(),
        };

        for layer in &tile.layers {
            if !layers.is_empty() && !layers.contains(&layer.name.as_str()) {
                continue;
            }

            // Recorded so that `ChartSource::collect` can filter by layer
            // without this tile being decoded again.
            let layer_index = u16::try_from(out.layer_names.len()).unwrap_or(u16::MAX);
            out.layer_names.push(layer.name.clone());

            let style = style_for_layer(&layer.name);
            let value_keys = value_attribute(style);

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
                // A sounding's depth, a wreck's clearance, a light's height
                // — whichever this style labels itself with.
                let value = value_keys
                    .iter()
                    .find_map(|k| feature.attribute(k))
                    .and_then(mvt::Value::as_f64);

                match &feature.geometry {
                    mvt::Geometry::Points(points) => {
                        out.points
                            .extend(points.iter().copied().map(|p| StyledPoint {
                                style,
                                layer: layer_index,
                                at: to_world(p),
                                value,
                            }));
                    }
                    mvt::Geometry::Lines(lines) => {
                        for line in lines {
                            if line.len() >= 2 {
                                out.lines.push(StyledLine {
                                    style,
                                    layer: layer_index,
                                    points: line.iter().copied().map(to_world).collect(),
                                    value,
                                });
                            }
                        }
                    }
                    mvt::Geometry::Polygons(polygons) => {
                        for polygon in polygons {
                            // Outlines first: a chart is read by its
                            // boundaries, and the fill is the tint behind
                            // them.
                            for ring in polygon.rings() {
                                if ring.len() >= 2 {
                                    out.lines.push(StyledLine {
                                        style,
                                        layer: layer_index,
                                        points: ring.iter().copied().map(to_world).collect(),
                                        value,
                                    });
                                }
                            }

                            // Then the fill, for the classes that are read
                            // as regions. An advisory area or a traffic
                            // scheme is a boundary with a label, and
                            // flooding it with colour would bury the chart
                            // under it.
                            if !matches!(style, ChartStyle::Land | ChartStyle::DepthArea) {
                                continue;
                            }

                            let exterior: Vec<WorldXY> =
                                polygon.exterior.iter().copied().map(to_world).collect();
                            let holes: Vec<Vec<WorldXY>> = polygon
                                .holes
                                .iter()
                                .map(|h| h.iter().copied().map(to_world).collect())
                                .collect();

                            let triangles = triangulate(&exterior, &holes);
                            if triangles.is_empty() {
                                // The triangulator refuses a polygon it
                                // cannot tile exactly rather than filling
                                // over a hole. The outline is still drawn,
                                // so the area does not vanish — it just
                                // goes untinted.
                                log::debug!(
                                    "{}: {} could not be filled ({} points, {} holes)",
                                    id.z,
                                    layer.name,
                                    exterior.len(),
                                    holes.len()
                                );
                                out.unfilled += 1;
                                continue;
                            }
                            out.areas.push(StyledArea {
                                style,
                                layer: layer_index,
                                value,
                                triangles,
                            });
                        }
                    }
                    mvt::Geometry::Unknown => {}
                }
            }
        }

        // Draw order: land and depth areas under the marks that sit on them.
        // A stable sort, so features within one style keep the order the
        // chart gave them.
        out.areas.sort_by_key(|a| a.style.depth_order());
        out.lines.sort_by_key(|l| l.style.depth_order());
        out.points.sort_by_key(|p| p.style.depth_order());

        out
    }

    /// Whether there is anything to draw.
    pub fn is_empty(&self) -> bool {
        self.areas.is_empty() && self.lines.is_empty() && self.points.is_empty()
    }

    /// Total vertices across every polyline, for sizing a GPU buffer.
    pub fn line_vertex_count(&self) -> usize {
        self.lines.iter().map(|l| l.points.len()).sum()
    }

    /// Which styles appear in this tile, in draw order. Useful for reporting
    /// what a chart actually contains, which is otherwise guesswork.
    pub fn styles(&self) -> Vec<ChartStyle> {
        collect_styles(&self.lines, &self.points)
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

    /// Layer names currently drawn, or empty for "all of them".
    pub fn selected_layers(&self) -> &[String] {
        &self.layers
    }

    /// Replace the layer filter.
    ///
    /// Clears the decoded-tile cache, because every cached tile was decoded
    /// under the old filter and would otherwise keep showing layers the
    /// caller just switched off. That is the entire subtlety here, and
    /// forgetting it gives a layer picker that appears to do nothing until
    /// you pan.
    ///
    /// Returns true if the selection actually changed, so a caller can skip
    /// the reupload when a redundant toggle arrives.
    pub fn set_layers(&mut self, layers: &[String]) -> bool {
        if self.layers == layers {
            return false;
        }
        self.layers = layers.to_vec();
        // Deliberately *not* clearing the cache. Tiles are decoded with
        // every layer and filtered in `collect`, so a change of selection
        // costs one re-upload of vertices and no decoding at all.
        true
    }

    /// Turn one layer on or off, leaving the rest as they are.
    ///
    /// An empty selection means "all layers", so switching one off from that
    /// state has to first expand the implicit set into an explicit one —
    /// otherwise hiding a single layer would hide everything else with it.
    pub fn toggle_layer(&mut self, name: &str, visible: bool) -> bool {
        let mut selection = if self.layers.is_empty() {
            self.file
                .metadata()
                .layers
                .iter()
                .map(|l| l.name.clone())
                .collect::<Vec<_>>()
        } else {
            self.layers.clone()
        };

        if visible {
            if !selection.iter().any(|l| l == name) {
                selection.push(name.to_owned());
            }
        } else {
            selection.retain(|l| l != name);
        }

        self.set_layers(&selection)
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

        // Decoded with every layer, whatever the current selection.
        //
        // The selection used to be applied here, which meant the cache was
        // only valid for one selection and `set_layers` had to empty it.
        // Hiding a layer then threw away every decoded tile and re-decoded
        // and re-triangulated the whole viewport — hundreds of milliseconds
        // to show *less*. Filtering happens in `collect` instead, and the
        // cache survives.
        //
        // This costs nothing in the common case: the app opens with no
        // selection, which already decoded everything.
        let decoded = match self.file.tile(id)? {
            Some(tile) => ChartTile::from_mvt(id, &tile, &[]),
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
            areas: Vec::new(),
            unfilled: 0,
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

        // Through `self.tile`, so the decoded-tile cache is actually used.
        //
        // This went through `self.file.tile` directly at first, which
        // bypassed the cache completely — so every change of tile set
        // re-queried SQLite, re-gunzipped and re-decoded protobuf for
        // geometry that was already decoded, and the cache that exists for
        // exactly this was dead weight. A test asserting the cache is
        // non-empty after a collect caught it.
        let ids = out.tiles.clone();
        // Cloned before the loop, because `self.tile` borrows `self`
        // mutably to fill the cache.
        let selection = self.layers.clone();
        for id in ids {
            match self.tile(id) {
                Ok(tile) => {
                    // Which of this tile's layers are wanted, worked out
                    // once per tile rather than once per feature — a tile
                    // holds thousands of features and a dozen layers.
                    let wanted: Vec<bool> = tile
                        .layer_names
                        .iter()
                        .map(|name| selection.is_empty() || selection.iter().any(|s| s == name))
                        .collect();
                    let keep = |layer: u16| wanted.get(usize::from(layer)).copied().unwrap_or(true);

                    out.areas
                        .extend(tile.areas.iter().filter(|a| keep(a.layer)).cloned());
                    out.unfilled += tile.unfilled;
                    out.lines
                        .extend(tile.lines.iter().filter(|l| keep(l.layer)).cloned());
                    out.points
                        .extend(tile.points.iter().filter(|p| keep(p.layer)).copied());
                }
                Err(e) => out.failed.push((id, e.to_string())),
            }
        }

        // Re-sort across tiles. Each tile was ordered on its own, but
        // concatenating them interleaves the styles again — and the next
        // tile's depth area drawn over this tile's soundings is exactly the
        // kind of wrongness that reads as a renderer bug.
        out.areas.sort_by_key(|a| a.style.depth_order());
        out.lines.sort_by_key(|l| l.style.depth_order());
        out.points.sort_by_key(|p| p.style.depth_order());

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
    pub areas: Vec<StyledArea>,
    /// Areas across this view that carry no fill. See [`ChartTile::unfilled`].
    pub unfilled: usize,
    pub lines: Vec<StyledLine>,
    pub points: Vec<StyledPoint>,
    /// Tiles that would not decode, with the reason. Empty in the normal
    /// case; non-empty means the file is damaged, not that the view is wrong.
    pub failed: Vec<(TileId, String)>,
    /// How many tiles the viewport wanted, when that exceeded
    /// [`ChartSource::MAX_TILES`] and nothing was decoded.
    pub over_budget: Option<usize>,
}

impl Collected {
    pub fn is_empty(&self) -> bool {
        self.areas.is_empty() && self.lines.is_empty() && self.points.is_empty()
    }

    /// Which feature types this viewport actually contains, in draw order.
    pub fn styles(&self) -> Vec<ChartStyle> {
        collect_styles(&self.lines, &self.points)
    }
}

/// Distinct styles present in some geometry, in draw order.
fn collect_styles(lines: &[StyledLine], points: &[StyledPoint]) -> Vec<ChartStyle> {
    let mut seen: Vec<ChartStyle> = Vec::new();
    for style in lines
        .iter()
        .map(|l| l.style)
        .chain(points.iter().map(|p| p.style))
    {
        if !seen.contains(&style) {
            seen.push(style);
        }
    }
    seen.sort_by_key(|s| s.depth_order());
    seen
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
        approx(chart.points[0].at.x, 1.0 / 4.0, 1e-12);
        approx(chart.points[0].at.y, 3.0 / 4.0, 1e-12);
    }

    #[test]
    fn tile_extent_maps_to_the_next_corner() {
        // Local (extent, extent) is the south-east corner, i.e. the origin
        // of the tile one along and one down.
        let id = TileId { z: 2, x: 1, y: 3 };
        let chart = ChartTile::from_mvt(id, &point_tile([4096, 4096], 4096), &[]);

        approx(chart.points[0].at.x, 2.0 / 4.0, 1e-12);
        approx(chart.points[0].at.y, 4.0 / 4.0, 1e-12);
    }

    #[test]
    fn a_non_default_extent_scales_correctly() {
        // Half way across a tile, whatever the extent claims to be.
        let id = TileId { z: 1, x: 0, y: 0 };
        for extent in [256u32, 512, 4096, 16384] {
            let half = (extent / 2) as i32;
            let chart = ChartTile::from_mvt(id, &point_tile([half, half], extent), &[]);
            approx(chart.points[0].at.x, 0.25, 1e-12);
            approx(chart.points[0].at.y, 0.25, 1e-12);
        }
    }

    #[test]
    fn zoom_zero_spans_the_whole_world() {
        let id = TileId { z: 0, x: 0, y: 0 };
        let chart = ChartTile::from_mvt(id, &point_tile([4096, 4096], 4096), &[]);
        approx(chart.points[0].at.x, 1.0, 1e-12);
        approx(chart.points[0].at.y, 1.0, 1e-12);
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
        assert!(chart.points[0].at.x.is_finite());
        assert!(chart.points[0].at.y.is_finite());
    }
}
