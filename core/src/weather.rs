//! Gridded weather fields: wind and temperature.
//!
//! GRIB2 — what every forecast model ships in — decodes to exactly this: a
//! regular latitude/longitude grid of one scalar per point, per parameter,
//! per forecast hour. Decoding the container is a parsing job for later; the
//! part that matters to everything downstream is sampling the grid at an
//! arbitrary position, which is what lives here.
//!
//! Wind arrives as two scalar grids, not as speed and direction: a U
//! component (eastward) and a V component (northward). That is not a quirk
//! of the format, it is the only way to interpolate wind correctly. Averaging
//! directions directly is wrong — halfway between 350° and 010° is 0°, but
//! the arithmetic mean is 180°, pointing exactly backwards. Interpolating the
//! components and converting afterwards has no such discontinuity, so
//! [`WindField`] keeps them separate right up to the moment it is asked for a
//! bearing.

use crate::geo::{normalise_deg, LatLon};

/// A regular lat/lon grid of one scalar parameter.
///
/// Row-major from the north-west corner: the first value is at
/// (`north`, `west`), x runs east, y runs south.
#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    /// Latitude of the top row, degrees.
    pub north: f64,
    /// Longitude of the left column, degrees.
    pub west: f64,
    /// Spacing between rows, degrees (positive; rows run southward).
    pub dlat: f64,
    /// Spacing between columns, degrees (positive; columns run eastward).
    pub dlon: f64,
    pub nx: usize,
    pub ny: usize,
    pub values: Vec<f32>,
}

impl Grid {
    /// Build a grid, checking that the data length matches the dimensions.
    pub fn new(
        north: f64,
        west: f64,
        dlat: f64,
        dlon: f64,
        nx: usize,
        ny: usize,
        values: Vec<f32>,
    ) -> Result<Self, GridError> {
        if nx == 0 || ny == 0 {
            return Err(GridError::Empty);
        }
        if dlat <= 0.0 || dlon <= 0.0 {
            return Err(GridError::BadSpacing { dlat, dlon });
        }
        if values.len() != nx * ny {
            return Err(GridError::LengthMismatch {
                expected: nx * ny,
                got: values.len(),
            });
        }
        Ok(Self {
            north,
            west,
            dlat,
            dlon,
            nx,
            ny,
            values,
        })
    }

    /// Value at a grid node, or `None` if the indices are off the grid.
    pub fn at(&self, x: usize, y: usize) -> Option<f32> {
        if x < self.nx && y < self.ny {
            Some(self.values[y * self.nx + x])
        } else {
            None
        }
    }

    /// Southern and eastern edges of the covered area.
    pub fn south(&self) -> f64 {
        self.north - self.dlat * (self.ny - 1) as f64
    }

    pub fn east(&self) -> f64 {
        self.west + self.dlon * (self.nx - 1) as f64
    }

    /// Whether `pos` falls inside the covered area.
    pub fn contains(&self, pos: LatLon) -> bool {
        pos.lat <= self.north + 1e-9
            && pos.lat >= self.south() - 1e-9
            && pos.lon >= self.west - 1e-9
            && pos.lon <= self.east() + 1e-9
    }

    /// Bilinearly interpolated value at `pos`, or `None` outside the grid.
    ///
    /// Bilinear rather than nearest-neighbour because forecast grids are
    /// coarse — half a degree is 30 nautical miles — and nearest-neighbour
    /// sampling of a 30-mile grid produces visible blocks and a wind barb
    /// that snaps as the boat crosses a cell boundary.
    pub fn sample(&self, pos: LatLon) -> Option<f32> {
        if !self.contains(pos) {
            return None;
        }

        // Fractional node coordinates, clamped so a position exactly on the
        // southern or eastern edge lands on the last cell rather than past it.
        let fx = ((pos.lon - self.west) / self.dlon).clamp(0.0, (self.nx - 1) as f64);
        let fy = ((self.north - pos.lat) / self.dlat).clamp(0.0, (self.ny - 1) as f64);

        let x0 = (fx.floor() as usize).min(self.nx - 1);
        let y0 = (fy.floor() as usize).min(self.ny - 1);
        let x1 = (x0 + 1).min(self.nx - 1);
        let y1 = (y0 + 1).min(self.ny - 1);

        let tx = (fx - x0 as f64) as f32;
        let ty = (fy - y0 as f64) as f32;

        let v00 = self.at(x0, y0)?;
        let v10 = self.at(x1, y0)?;
        let v01 = self.at(x0, y1)?;
        let v11 = self.at(x1, y1)?;

        let top = v00 + (v10 - v00) * tx;
        let bottom = v01 + (v11 - v01) * tx;
        Some(top + (bottom - top) * ty)
    }
}

// No Eq: BadSpacing carries f64s, which are only PartialEq.
#[derive(Debug, Clone, PartialEq)]
pub enum GridError {
    Empty,
    BadSpacing { dlat: f64, dlon: f64 },
    LengthMismatch { expected: usize, got: usize },
}

impl std::fmt::Display for GridError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "grid has a zero dimension"),
            Self::BadSpacing { dlat, dlon } => {
                write!(
                    f,
                    "grid spacing must be positive, got dlat={dlat} dlon={dlon}"
                )
            }
            Self::LengthMismatch { expected, got } => {
                write!(f, "grid needs {expected} values, got {got}")
            }
        }
    }
}

impl std::error::Error for GridError {}

/// Wind as its two components, each on the same grid.
#[derive(Debug, Clone, PartialEq)]
pub struct WindField {
    /// Eastward component, m/s.
    pub u: Grid,
    /// Northward component, m/s.
    pub v: Grid,
}

impl WindField {
    /// Pair two grids, checking they describe the same geometry.
    pub fn new(u: Grid, v: Grid) -> Result<Self, GridError> {
        let same = u.nx == v.nx
            && u.ny == v.ny
            && (u.north - v.north).abs() < 1e-9
            && (u.west - v.west).abs() < 1e-9
            && (u.dlat - v.dlat).abs() < 1e-12
            && (u.dlon - v.dlon).abs() < 1e-12;
        if !same {
            return Err(GridError::LengthMismatch {
                expected: u.nx * u.ny,
                got: v.nx * v.ny,
            });
        }
        Ok(Self { u, v })
    }

    /// Wind at `pos`, or `None` outside the grid.
    pub fn sample(&self, pos: LatLon) -> Option<Wind> {
        let u = self.u.sample(pos)?;
        let v = self.v.sample(pos)?;
        Some(Wind::from_components(u, v))
    }
}

/// A wind vector, in both the forms the app needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Wind {
    /// Eastward component, m/s.
    pub u: f32,
    /// Northward component, m/s.
    pub v: f32,
}

impl Wind {
    pub const fn from_components(u: f32, v: f32) -> Self {
        Self { u, v }
    }

    /// Speed in metres per second.
    pub fn speed_ms(&self) -> f64 {
        ((self.u as f64).powi(2) + (self.v as f64).powi(2)).sqrt()
    }

    /// Speed in knots.
    pub fn speed_kn(&self) -> f64 {
        self.speed_ms() * 1.943_844_49
    }

    /// Direction the wind blows *from*, degrees true.
    ///
    /// Meteorological convention, which is what a forecast means by "a
    /// northerly": a wind from the north blows southward, so its V component
    /// is negative and this reads 0/360. Getting this backwards is the
    /// classic GRIB bug.
    pub fn direction_from_deg(&self) -> f64 {
        normalise_deg((self.u as f64).atan2(self.v as f64).to_degrees() + 180.0)
    }

    /// Direction the wind blows *towards*, degrees true. What an arrow glyph
    /// on the chart points at.
    pub fn direction_to_deg(&self) -> f64 {
        normalise_deg(self.direction_from_deg() + 180.0)
    }

    /// Beaufort force, 0–12.
    pub fn beaufort(&self) -> u8 {
        const UPPER_KN: [f64; 12] = [
            1.0, 3.0, 6.0, 10.0, 16.0, 21.0, 27.0, 33.0, 40.0, 47.0, 55.0, 63.0,
        ];
        let kn = self.speed_kn();
        UPPER_KN.iter().position(|&u| kn < u).unwrap_or(12) as u8
    }
}

/// Kelvin to Celsius. GRIB temperature fields are always Kelvin.
pub fn kelvin_to_celsius(k: f32) -> f32 {
    k - 273.15
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3×3 grid over 50–52°N, 5°W–3°W, value == the node index.
    fn ramp() -> Grid {
        Grid::new(
            52.0,
            -5.0,
            1.0,
            1.0,
            3,
            3,
            (0..9).map(|i| i as f32).collect(),
        )
        .unwrap()
    }

    fn approx(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() < tol, "expected {b}, got {a}");
    }

    #[test]
    fn rejects_malformed_grids() {
        assert_eq!(
            Grid::new(0.0, 0.0, 1.0, 1.0, 0, 3, vec![]),
            Err(GridError::Empty)
        );
        assert_eq!(
            Grid::new(0.0, 0.0, 1.0, 1.0, 2, 2, vec![1.0, 2.0]),
            Err(GridError::LengthMismatch {
                expected: 4,
                got: 2
            })
        );
        assert!(matches!(
            Grid::new(0.0, 0.0, 0.0, 1.0, 2, 2, vec![0.0; 4]),
            Err(GridError::BadSpacing { .. })
        ));
    }

    #[test]
    fn edges_are_where_they_claim() {
        let g = ramp();
        approx(g.south(), 50.0, 1e-12);
        approx(g.east(), -3.0, 1e-12);
    }

    #[test]
    fn nodes_sample_exactly() {
        let g = ramp();
        // North-west corner is value 0, south-east is 8.
        approx(g.sample(LatLon::new(52.0, -5.0)).unwrap() as f64, 0.0, 1e-5);
        approx(g.sample(LatLon::new(52.0, -3.0)).unwrap() as f64, 2.0, 1e-5);
        approx(g.sample(LatLon::new(50.0, -5.0)).unwrap() as f64, 6.0, 1e-5);
        approx(g.sample(LatLon::new(50.0, -3.0)).unwrap() as f64, 8.0, 1e-5);
        approx(g.sample(LatLon::new(51.0, -4.0)).unwrap() as f64, 4.0, 1e-5);
    }

    #[test]
    fn midpoints_interpolate() {
        let g = ramp();
        // Halfway between nodes 0 and 1 along the top row.
        approx(g.sample(LatLon::new(52.0, -4.5)).unwrap() as f64, 0.5, 1e-5);
        // Centre of the north-west cell: mean of 0, 1, 3, 4.
        approx(g.sample(LatLon::new(51.5, -4.5)).unwrap() as f64, 2.0, 1e-5);
    }

    #[test]
    fn outside_the_grid_is_none() {
        let g = ramp();
        assert!(
            g.sample(LatLon::new(53.0, -4.0)).is_none(),
            "north of the grid"
        );
        assert!(
            g.sample(LatLon::new(49.0, -4.0)).is_none(),
            "south of the grid"
        );
        assert!(
            g.sample(LatLon::new(51.0, -6.0)).is_none(),
            "west of the grid"
        );
        assert!(
            g.sample(LatLon::new(51.0, -2.0)).is_none(),
            "east of the grid"
        );
    }

    #[test]
    fn indexing_is_row_major_from_the_north_west() {
        let g = ramp();
        assert_eq!(g.at(0, 0), Some(0.0));
        assert_eq!(g.at(2, 0), Some(2.0));
        assert_eq!(g.at(0, 2), Some(6.0));
        assert_eq!(g.at(3, 0), None);
        assert_eq!(g.at(0, 3), None);
    }

    #[test]
    fn wind_speed_is_the_vector_magnitude() {
        let w = Wind::from_components(3.0, 4.0);
        approx(w.speed_ms(), 5.0, 1e-9);
        approx(w.speed_kn(), 9.719, 1e-3);
    }

    #[test]
    fn meteorological_direction_convention() {
        // A northerly blows from the north, i.e. southward: v negative.
        approx(
            Wind::from_components(0.0, -5.0).direction_from_deg(),
            0.0,
            1e-6,
        );
        // A southerly blows northward: v positive.
        approx(
            Wind::from_components(0.0, 5.0).direction_from_deg(),
            180.0,
            1e-6,
        );
        // A westerly blows eastward: u positive.
        approx(
            Wind::from_components(5.0, 0.0).direction_from_deg(),
            270.0,
            1e-6,
        );
        // An easterly blows westward: u negative.
        approx(
            Wind::from_components(-5.0, 0.0).direction_from_deg(),
            90.0,
            1e-6,
        );
    }

    #[test]
    fn direction_to_is_the_reciprocal() {
        for (u, v) in [(3.0, 4.0), (-2.0, 7.0), (0.0, -1.0), (-5.0, -5.0)] {
            let w = Wind::from_components(u, v);
            let diff = (w.direction_to_deg() - w.direction_from_deg()).abs();
            approx(diff.min(360.0 - diff), 180.0, 1e-6);
        }
    }

    #[test]
    fn beaufort_brackets() {
        let kn_to_ms = |kn: f64| (kn / 1.943_844_49) as f32;
        let force = |kn: f64| Wind::from_components(0.0, kn_to_ms(kn)).beaufort();

        assert_eq!(force(0.0), 0, "calm");
        assert_eq!(force(2.0), 1, "light air");
        assert_eq!(force(12.0), 4, "moderate breeze");
        assert_eq!(force(30.0), 7, "near gale");
        assert_eq!(force(45.0), 9, "strong gale");
        assert_eq!(force(80.0), 12, "hurricane force");
    }

    #[test]
    fn wind_field_interpolates_components_not_angles() {
        // Two adjacent nodes with wind from 350° and from 010°. Interpolating
        // the bearings would give 180° — exactly backwards. Interpolating the
        // components must give due north.
        let from = |deg: f64, speed: f64| {
            let r = (deg + 180.0).to_radians();
            ((speed * r.sin()) as f32, (speed * r.cos()) as f32)
        };
        let (u0, v0) = from(350.0, 10.0);
        let (u1, v1) = from(10.0, 10.0);

        let u = Grid::new(50.0, -5.0, 1.0, 1.0, 2, 1, vec![u0, u1]).unwrap();
        let v = Grid::new(50.0, -5.0, 1.0, 1.0, 2, 1, vec![v0, v1]).unwrap();
        let field = WindField::new(u, v).unwrap();

        let w = field.sample(LatLon::new(50.0, -4.5)).unwrap();
        let d = w.direction_from_deg();
        assert!(d < 1.0 || d > 359.0, "expected due north, got {d}");
    }

    #[test]
    fn mismatched_component_grids_are_rejected() {
        let u = Grid::new(50.0, -5.0, 1.0, 1.0, 2, 2, vec![0.0; 4]).unwrap();
        let v = Grid::new(60.0, -5.0, 1.0, 1.0, 2, 2, vec![0.0; 4]).unwrap();
        assert!(
            WindField::new(u, v).is_err(),
            "different origins must not pair"
        );
    }

    #[test]
    fn temperature_converts() {
        approx(kelvin_to_celsius(273.15) as f64, 0.0, 1e-4);
        approx(kelvin_to_celsius(293.15) as f64, 20.0, 1e-4);
    }
}
