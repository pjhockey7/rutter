//! rutter's portable core: charts, weather and tides.
//!
//! Everything in this crate is pure computation over data. No windowing, no
//! GPU, no filesystem, no clock, no platform assumption — which is the whole
//! point. This is the layer that is identical on Android, Linux, iOS, Windows
//! and macOS, and the layer where the navigation is actually correct or not.
//! It is `#![no_std]`-shaped in spirit if not in declaration: the only reason
//! `std` is linked is `Vec` and `String`.
//!
//! The split follows the firmware convention this project is modelled on:
//!
//! * `rutter-core` — application logic. Portable, and the only crate with
//!   meaningful tests.
//! * `rutter-render` — drawing, via wgpu. Portable in source but needs a GPU
//!   and a surface, so it is tested by looking at it.
//! * `rutter-linux`, `rutter-android` — the board support. Window creation,
//!   event loop, file paths, permissions, lifecycle. Deliberately thin, and
//!   deliberately the only place a platform name appears.
//!
//! The rule that keeps this honest: a dependency may point down that list,
//! never up. If the core ever needs to know which platform it is on, the
//! abstraction is wrong.

// The workspace bans `unwrap` and `expect` because a panic is a crash on a
// phone in the rain with no stack trace. Inside tests a panic IS the failure
// report, so they are allowed there and nowhere else.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod chart;
pub mod geo;
pub mod tide;
pub mod weather;

pub use geo::LatLon;

/// What the app is looking at: a centre, a zoom and a viewport size.
///
/// Held in the core rather than the renderer because panning and zooming are
/// logic, not drawing, and because the same state has to survive an Android
/// activity being destroyed and recreated under it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub centre: LatLon,
    /// Fractional slippy-map zoom. Fractional so pinch gestures are smooth;
    /// tile selection floors it.
    pub zoom: f64,
    pub width_px: u32,
    pub height_px: u32,
}

impl View {
    /// Zoom bounds. Below 2 the world is smaller than a phone screen; above
    /// 20 no chart has data.
    pub const MIN_ZOOM: f64 = 2.0;
    pub const MAX_ZOOM: f64 = 20.0;

    pub fn new(centre: LatLon, zoom: f64, width_px: u32, height_px: u32) -> Self {
        Self {
            centre,
            zoom: zoom.clamp(Self::MIN_ZOOM, Self::MAX_ZOOM),
            width_px,
            height_px,
        }
    }

    /// The integer tile zoom to fetch for this view.
    pub fn tile_zoom(&self) -> u8 {
        self.zoom.floor() as u8
    }

    /// North-west and south-east corners of the visible area.
    pub fn bounds(&self) -> (LatLon, LatLon) {
        let scale = 256.0 * (1u64 << self.tile_zoom()) as f64 * 2f64.powf(self.zoom.fract());
        let half_w = self.width_px as f64 / 2.0 / scale;
        let half_h = self.height_px as f64 / 2.0 / scale;

        let c = chart::project(self.centre);
        let nw = chart::unproject(chart::WorldXY {
            x: (c.x - half_w).clamp(0.0, 1.0),
            y: (c.y - half_h).clamp(0.0, 1.0),
        });
        let se = chart::unproject(chart::WorldXY {
            x: (c.x + half_w).clamp(0.0, 1.0),
            y: (c.y + half_h).clamp(0.0, 1.0),
        });
        (nw, se)
    }

    /// The tiles this view needs loaded.
    pub fn visible_tiles(&self) -> Vec<chart::TileId> {
        let (nw, se) = self.bounds();
        chart::tiles_in_view(nw, se, self.tile_zoom())
    }

    /// Change zoom, keeping it in range.
    pub fn zoomed(&self, delta: f64) -> Self {
        Self {
            zoom: (self.zoom + delta).clamp(Self::MIN_ZOOM, Self::MAX_ZOOM),
            ..*self
        }
    }
}

impl Default for View {
    fn default() -> Self {
        // Falmouth, for want of anywhere better to start.
        Self::new(LatLon::new(50.1531, -5.0659), 12.0, 1280, 720)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_is_clamped_both_ways() {
        let v = View::new(LatLon::new(0.0, 0.0), 99.0, 800, 600);
        assert_eq!(v.zoom, View::MAX_ZOOM);

        let v = View::new(LatLon::new(0.0, 0.0), -5.0, 800, 600);
        assert_eq!(v.zoom, View::MIN_ZOOM);

        assert_eq!(v.zoomed(100.0).zoom, View::MAX_ZOOM);
        assert_eq!(v.zoomed(-100.0).zoom, View::MIN_ZOOM);
    }

    #[test]
    fn tile_zoom_floors() {
        let v = View::new(LatLon::new(0.0, 0.0), 12.7, 800, 600);
        assert_eq!(v.tile_zoom(), 12);
    }

    #[test]
    fn bounds_surround_the_centre() {
        let v = View::default();
        let (nw, se) = v.bounds();
        assert!(nw.lat > v.centre.lat, "centre not below the north edge");
        assert!(se.lat < v.centre.lat, "centre not above the south edge");
        assert!(nw.lon < v.centre.lon, "centre not east of the west edge");
        assert!(se.lon > v.centre.lon, "centre not west of the east edge");
    }

    #[test]
    fn zooming_in_narrows_the_view() {
        let out = View::new(LatLon::new(50.0, -5.0), 8.0, 800, 600);
        let r#in = out.zoomed(4.0);

        let span = |v: &View| {
            let (nw, se) = v.bounds();
            nw.lat - se.lat
        };
        assert!(
            span(&r#in) < span(&out),
            "zooming in did not narrow the view"
        );
    }

    #[test]
    fn a_view_always_needs_at_least_one_tile() {
        for zoom in [2.0, 6.0, 12.5, 20.0] {
            let v = View::new(LatLon::new(50.0, -5.0), zoom, 1080, 2400);
            let tiles = v.visible_tiles();
            assert!(!tiles.is_empty(), "no tiles at zoom {zoom}");
            assert!(
                tiles.iter().all(|t| t.is_valid()),
                "invalid tile at zoom {zoom}"
            );
        }
    }

    #[test]
    fn the_centre_tile_is_always_in_the_visible_set() {
        let v = View::default();
        let centre = chart::TileId::containing(v.centre, v.tile_zoom());
        assert!(v.visible_tiles().contains(&centre));
    }

    #[test]
    fn a_whole_world_view_does_not_run_off_the_edge() {
        // Zoomed right out at the antimeridian: clamping must keep every
        // tile address in range rather than wrapping.
        let v = View::new(LatLon::new(0.0, 180.0), 2.0, 4000, 4000);
        assert!(v.visible_tiles().iter().all(|t| t.is_valid()));
    }
}
