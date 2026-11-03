//! Web Mercator projection and slippy-map tile arithmetic.
//!
//! This is the bridge between the two coordinate systems the app lives in:
//! degrees, which is how positions arrive from GPS and how charts are
//! indexed, and screen space, which is what the renderer wants.
//!
//! Web Mercator (EPSG:3857) is used because it is what MBTiles, every raster
//! tile server and every vector tile scheme are built on. It is not a kind
//! projection — area inflates badly toward the poles — but it is conformal,
//! so a rhumb line is straight and angles are true locally, which is
//! precisely what a navigator wants from a chart.
//!
//! Normalised world coordinates run 0..1 from the north-west corner: x east,
//! y south. Multiplying by `2^zoom` gives tile coordinates, which is the one
//! fact that makes the rest of this module trivial.

use crate::geo::LatLon;

/// The latitude where Web Mercator is truncated, north and south.
///
/// y tends to infinity at the poles, so every implementation cuts off. This
/// value is the convention, chosen so the world is exactly square.
pub const MAX_LATITUDE: f64 = 85.051_128_779_806_59;

/// A position in normalised world space: 0..1, north-west origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorldXY {
    pub x: f64,
    pub y: f64,
}

/// Project a position into normalised world space.
///
/// Latitude is clamped to [`MAX_LATITUDE`]; a caller north of that gets the
/// truncation latitude rather than an infinity.
pub fn project(pos: LatLon) -> WorldXY {
    let lat = pos.lat.clamp(-MAX_LATITUDE, MAX_LATITUDE).to_radians();
    let x = (pos.lon + 180.0) / 360.0;

    // The Mercator y integral, normalised and flipped so y grows southward.
    let y =
        0.5 - ((std::f64::consts::FRAC_PI_4 + lat / 2.0).tan().ln()) / (2.0 * std::f64::consts::PI);

    WorldXY { x, y }
}

/// Invert [`project`].
pub fn unproject(w: WorldXY) -> LatLon {
    let lon = w.x * 360.0 - 180.0;
    let n = std::f64::consts::PI * (1.0 - 2.0 * w.y);
    let lat = n.sinh().atan().to_degrees();
    LatLon { lat, lon }
}

/// A slippy-map tile address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileId {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

impl TileId {
    /// Number of tiles along one edge of the world at this zoom.
    pub fn grid_size(z: u8) -> u32 {
        1u32 << z
    }

    /// The tile containing `pos` at zoom `z`.
    pub fn containing(pos: LatLon, z: u8) -> TileId {
        let n = Self::grid_size(z) as f64;
        let w = project(pos);
        // The clamp catches lon == 180.0 exactly, which projects to x == 1.0
        // and would otherwise index one tile past the edge.
        let max = Self::grid_size(z) - 1;
        TileId {
            z,
            x: ((w.x * n).floor() as i64).clamp(0, max as i64) as u32,
            y: ((w.y * n).floor() as i64).clamp(0, max as i64) as u32,
        }
    }

    /// North-west and south-east corners of this tile.
    pub fn bounds(&self) -> (LatLon, LatLon) {
        let n = Self::grid_size(self.z) as f64;
        let nw = unproject(WorldXY {
            x: self.x as f64 / n,
            y: self.y as f64 / n,
        });
        let se = unproject(WorldXY {
            x: (self.x + 1) as f64 / n,
            y: (self.y + 1) as f64 / n,
        });
        (nw, se)
    }

    /// The tile one zoom level out that contains this one.
    pub fn parent(&self) -> Option<TileId> {
        self.z.checked_sub(1).map(|z| TileId {
            z,
            x: self.x / 2,
            y: self.y / 2,
        })
    }

    /// Whether the address is in range for its zoom level.
    pub fn is_valid(&self) -> bool {
        let n = Self::grid_size(self.z);
        self.z <= 30 && self.x < n && self.y < n
    }
}

/// Every tile at zoom `z` intersecting the box from `nw` to `se`.
///
/// Returned in row-major order, north to south. This is the renderer's
/// per-frame question: given the viewport, what do I need to have loaded.
pub fn tiles_in_view(nw: LatLon, se: LatLon, z: u8) -> Vec<TileId> {
    let a = TileId::containing(nw, z);
    let b = TileId::containing(se, z);

    let (x0, x1) = (a.x.min(b.x), a.x.max(b.x));
    let (y0, y1) = (a.y.min(b.y), a.y.max(b.y));

    let mut out = Vec::with_capacity(((x1 - x0 + 1) * (y1 - y0 + 1)) as usize);
    for y in y0..=y1 {
        for x in x0..=x1 {
            out.push(TileId { z, x, y });
        }
    }
    out
}

/// Ground resolution in metres per pixel, for 256-pixel tiles at zoom `z`
/// and latitude `lat`.
///
/// Used to pick a zoom level for a wanted scale, and to size the chart's
/// distance scale bar.
pub fn metres_per_pixel(lat: f64, z: u8) -> f64 {
    let circumference = 2.0 * std::f64::consts::PI * crate::geo::EARTH_RADIUS_M;
    circumference * lat.to_radians().cos() / (256.0 * TileId::grid_size(z) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() < tol, "expected {b}, got {a}");
    }

    #[test]
    fn null_island_is_the_centre_of_the_world() {
        let w = project(LatLon::new(0.0, 0.0));
        approx(w.x, 0.5, 1e-12);
        approx(w.y, 0.5, 1e-12);
    }

    #[test]
    fn corners_map_to_the_unit_square() {
        let nw = project(LatLon::new(MAX_LATITUDE, -180.0));
        approx(nw.x, 0.0, 1e-9);
        approx(nw.y, 0.0, 1e-9);

        let se = project(LatLon::new(-MAX_LATITUDE, 180.0));
        approx(se.x, 1.0, 1e-9);
        approx(se.y, 1.0, 1e-9);
    }

    #[test]
    fn projection_round_trips() {
        for pos in [
            LatLon::new(0.0, 0.0),
            LatLon::new(50.0, -5.0),
            LatLon::new(-33.86, 151.21),
            LatLon::new(60.0, 179.9),
        ] {
            let back = unproject(project(pos));
            approx(back.lat, pos.lat, 1e-9);
            approx(back.lon, pos.lon, 1e-9);
        }
    }

    #[test]
    fn poles_clamp_instead_of_diverging() {
        let w = project(LatLon::new(90.0, 0.0));
        assert!(w.y.is_finite(), "north pole projected to {}", w.y);
        approx(w.y, 0.0, 1e-9);

        let w = project(LatLon::new(-90.0, 0.0));
        assert!(w.y.is_finite());
        approx(w.y, 1.0, 1e-9);
    }

    #[test]
    fn zoom_zero_is_a_single_tile() {
        assert_eq!(TileId::grid_size(0), 1);
        let t = TileId::containing(LatLon::new(50.0, -5.0), 0);
        assert_eq!(t, TileId { z: 0, x: 0, y: 0 });
    }

    #[test]
    fn quadrants_at_zoom_one() {
        // North-west quadrant is (0,0); south-east is (1,1).
        assert_eq!(
            TileId::containing(LatLon::new(45.0, -90.0), 1),
            TileId { z: 1, x: 0, y: 0 }
        );
        assert_eq!(
            TileId::containing(LatLon::new(45.0, 90.0), 1),
            TileId { z: 1, x: 1, y: 0 }
        );
        assert_eq!(
            TileId::containing(LatLon::new(-45.0, -90.0), 1),
            TileId { z: 1, x: 0, y: 1 }
        );
        assert_eq!(
            TileId::containing(LatLon::new(-45.0, 90.0), 1),
            TileId { z: 1, x: 1, y: 1 }
        );
    }

    #[test]
    fn edge_longitude_stays_in_range() {
        // lon == 180 projects to x == 1.0 exactly; without the clamp this
        // indexes a tile that does not exist.
        for z in [1u8, 8, 14] {
            let t = TileId::containing(LatLon::new(0.0, 180.0), z);
            assert!(t.is_valid(), "{t:?} out of range");
            let t = TileId::containing(LatLon::new(90.0, 180.0), z);
            assert!(t.is_valid(), "{t:?} out of range");
        }
    }

    #[test]
    fn a_tile_contains_its_own_centre() {
        let t = TileId {
            z: 12,
            x: 2035,
            y: 1374,
        };
        let (nw, se) = t.bounds();
        let centre = LatLon::new((nw.lat + se.lat) / 2.0, (nw.lon + se.lon) / 2.0);
        assert_eq!(TileId::containing(centre, 12), t);
    }

    #[test]
    fn bounds_are_oriented_north_west_to_south_east() {
        let (nw, se) = TileId { z: 6, x: 31, y: 21 }.bounds();
        assert!(
            nw.lat > se.lat,
            "north edge {} not above south {}",
            nw.lat,
            se.lat
        );
        assert!(
            nw.lon < se.lon,
            "west edge {} not left of east {}",
            nw.lon,
            se.lon
        );
    }

    #[test]
    fn parent_contains_child() {
        let child = TileId {
            z: 10,
            x: 511,
            y: 340,
        };
        let parent = child.parent().unwrap();
        assert_eq!(
            parent,
            TileId {
                z: 9,
                x: 255,
                y: 170
            }
        );

        let (cnw, cse) = child.bounds();
        let (pnw, pse) = parent.bounds();
        assert!(pnw.lat >= cnw.lat - 1e-9 && pse.lat <= cse.lat + 1e-9);
        assert!(pnw.lon <= cnw.lon + 1e-9 && pse.lon >= cse.lon - 1e-9);

        assert!(TileId { z: 0, x: 0, y: 0 }.parent().is_none());
    }

    #[test]
    fn view_covers_the_box_and_nothing_more() {
        // A small box inside one tile asks for exactly that tile.
        let t = TileId {
            z: 14,
            x: 8142,
            y: 5497,
        };
        let (nw, se) = t.bounds();
        let inset = LatLon::new(nw.lat - 0.0001, nw.lon + 0.0001);
        let inset2 = LatLon::new(se.lat + 0.0001, se.lon - 0.0001);
        assert_eq!(tiles_in_view(inset, inset2, 14), vec![t]);

        // The whole world at zoom 2 is sixteen tiles.
        let all = tiles_in_view(
            LatLon::new(MAX_LATITUDE, -180.0),
            LatLon::new(-MAX_LATITUDE, 180.0),
            2,
        );
        assert_eq!(all.len(), 16);
        assert!(all.iter().all(|t| t.is_valid()));
    }

    #[test]
    fn resolution_halves_each_zoom_level() {
        let a = metres_per_pixel(0.0, 10);
        let b = metres_per_pixel(0.0, 11);
        approx(a / b, 2.0, 1e-9);

        // Zoom 0 at the equator is the much-quoted ~156 km/px.
        approx(metres_per_pixel(0.0, 0), 156_412.0, 50.0);
    }

    #[test]
    fn resolution_shrinks_with_latitude() {
        assert!(metres_per_pixel(60.0, 12) < metres_per_pixel(0.0, 12));
    }
}
