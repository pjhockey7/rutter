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

/// One forecast hour: wind and air temperature over the same grid.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// Hours after the model run this is valid for.
    pub hour: i32,
    pub wind: WindField,
    /// Air temperature at 2 m, Celsius.
    pub temperature: Grid,
}

/// A whole forecast: several hours over one area.
///
/// Read from the file `rutter weather` writes. That file exists because the
/// alternative is a GRIB2 decoder, and GRIB2 is a container format with
/// twelve template families and a bitmap-packing scheme that is a project in
/// itself — while the part this app needs from it is "a rectangle of floats
/// per parameter per hour". So GDAL does the decoding on a desktop, at fetch
/// time, exactly as it already does for S-57 charts, and what reaches the
/// boat is the rectangle.
///
/// The format is deliberately boring: a fixed 56-byte header then the
/// values, little-endian throughout. Parsing it is pure arithmetic over a
/// byte slice, which is why it lives in this crate; opening the file is the
/// caller's job.
#[derive(Debug, Clone, PartialEq)]
pub struct Forecast {
    /// Unix time of the model run that produced this.
    pub run: i64,
    pub steps: Vec<Step>,
}

/// What the writer and reader agree on.
const MAGIC: &[u8; 8] = b"RUTTERWX";
/// Bumped if the layout changes, so an old file is refused rather than
/// misread as a new one.
const FORMAT_VERSION: u16 = 1;
const HEADER_BYTES: usize = 56;

#[derive(Debug, Clone, PartialEq)]
pub enum ForecastError {
    /// Not one of our files.
    NotAForecast,
    /// A version this build does not know.
    Version(u16),
    /// The file is shorter than its own header says it should be.
    Truncated { expected: usize, got: usize },
    /// A grid inside it does not describe a grid.
    Grid(GridError),
    /// No forecast hours at all.
    Empty,
}

impl core::fmt::Display for ForecastError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotAForecast => {
                write!(f, "not a rutter forecast — build one with `rutter weather`")
            }
            Self::Version(v) => write!(f, "forecast format version {v} is not supported"),
            Self::Truncated { expected, got } => {
                write!(f, "forecast is {got} bytes, expected at least {expected}")
            }
            Self::Grid(e) => write!(f, "{e}"),
            Self::Empty => write!(f, "forecast holds no hours"),
        }
    }
}

impl std::error::Error for ForecastError {}

impl From<GridError> for ForecastError {
    fn from(e: GridError) -> Self {
        Self::Grid(e)
    }
}

impl Forecast {
    /// Decode the file `rutter weather` writes.
    pub fn parse(bytes: &[u8]) -> Result<Self, ForecastError> {
        if bytes.len() < HEADER_BYTES || &bytes[..8] != MAGIC {
            return Err(ForecastError::NotAForecast);
        }
        let version = u16::from_le_bytes([bytes[8], bytes[9]]);
        if version != FORMAT_VERSION {
            return Err(ForecastError::Version(version));
        }

        let u16_at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]) as usize;
        let f64_at = |o: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&bytes[o..o + 8]);
            f64::from_le_bytes(b)
        };

        let nx = u16_at(10);
        let ny = u16_at(12);
        let step_count = u16_at(14);
        let mut run_bytes = [0u8; 8];
        run_bytes.copy_from_slice(&bytes[16..24]);
        let run = i64::from_le_bytes(run_bytes);
        let (north, west, dlat, dlon) = (f64_at(24), f64_at(32), f64_at(40), f64_at(48));

        if step_count == 0 {
            return Err(ForecastError::Empty);
        }

        // Three parameters per hour — u, v, temperature — plus the hour.
        let per_step = 4 + 3 * nx * ny * 4;
        let expected = HEADER_BYTES + step_count * per_step;
        if bytes.len() < expected {
            return Err(ForecastError::Truncated {
                expected,
                got: bytes.len(),
            });
        }

        let floats = |from: usize, count: usize| -> Vec<f32> {
            (0..count)
                .map(|i| {
                    let o = from + i * 4;
                    f32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]])
                })
                .collect()
        };

        let mut steps = Vec::with_capacity(step_count);
        let nodes = nx * ny;
        for index in 0..step_count {
            let base = HEADER_BYTES + index * per_step;
            let hour = i32::from_le_bytes([
                bytes[base],
                bytes[base + 1],
                bytes[base + 2],
                bytes[base + 3],
            ]);
            let values = base + 4;

            let grid = |which: usize| {
                Grid::new(
                    north,
                    west,
                    dlat,
                    dlon,
                    nx,
                    ny,
                    floats(values + which * nodes * 4, nodes),
                )
            };

            steps.push(Step {
                hour,
                wind: WindField::new(grid(0)?, grid(1)?)?,
                temperature: grid(2)?,
            });
        }

        Ok(Self { run, steps })
    }

    /// Encode, so that a test can round-trip a forecast without a network.
    ///
    /// The writer is `scripts/weather-grid.py`; this exists to check the
    /// reader against something, and to document the layout in code rather
    /// than only in a comment.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());

        let (nx, ny) = match self.steps.first() {
            Some(s) => (s.temperature.nx as u16, s.temperature.ny as u16),
            None => (0, 0),
        };
        out.extend_from_slice(&nx.to_le_bytes());
        out.extend_from_slice(&ny.to_le_bytes());
        out.extend_from_slice(&(self.steps.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.run.to_le_bytes());

        let first = self.steps.first().map(|s| &s.temperature);
        for value in [
            first.map_or(0.0, |g| g.north),
            first.map_or(0.0, |g| g.west),
            first.map_or(1.0, |g| g.dlat),
            first.map_or(1.0, |g| g.dlon),
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }

        for step in &self.steps {
            out.extend_from_slice(&step.hour.to_le_bytes());
            for grid in [&step.wind.u, &step.wind.v, &step.temperature] {
                for value in &grid.values {
                    out.extend_from_slice(&value.to_le_bytes());
                }
            }
        }
        out
    }

    /// The step closest to a time, and how stale it is.
    ///
    /// A forecast is only as good as its age, so the caller is told: an
    /// hour's drift is ordinary, a day's means the file should be refetched
    /// before anyone steers by it.
    pub fn nearest(&self, unix: i64) -> Option<(&Step, i64)> {
        self.steps
            .iter()
            .map(|s| {
                let valid = self.run + i64::from(s.hour) * 3600;
                (s, unix - valid)
            })
            .min_by_key(|(_, offset)| offset.abs())
    }

    /// When a step is valid for, in Unix seconds.
    pub fn valid_at(&self, step: &Step) -> i64 {
        self.run + i64::from(step.hour) * 3600
    }

    /// The first and last times this forecast speaks for.
    ///
    /// The range a time slider may offer: past the last step there is no
    /// wind to show, and showing the last step forever would present a
    /// three-day-old guess as a forecast for next week.
    pub fn span(&self) -> Option<(i64, i64)> {
        let first = self.steps.first()?;
        let last = self.steps.last()?;
        Some((self.valid_at(first), self.valid_at(last)))
    }

    /// The two steps either side of a time, and how far between them.
    ///
    /// `None` outside the forecast's span — deliberately, rather than
    /// clamping to an end. A slider dragged past the horizon must show
    /// nothing, because the alternative is the last step pretending to be
    /// a forecast for a time nobody modelled.
    ///
    /// The fraction is 0 at the earlier step and 1 at the later one. When
    /// the time lands exactly on a step, both are that step.
    fn bracket(&self, unix: i64) -> Option<(&Step, &Step, f32)> {
        let (first, last) = self.span()?;
        if unix < first || unix > last {
            return None;
        }
        // Steps are in file order, which the writer emits ascending. Linear
        // over at most a couple of dozen is nothing, and a binary search
        // would need the order to be an invariant rather than a habit.
        let mut before = self.steps.first()?;
        for step in &self.steps {
            if self.valid_at(step) <= unix {
                before = step;
            } else {
                let (a, b) = (self.valid_at(before), self.valid_at(step));
                // Guard the division rather than assume strictly
                // increasing hours: a malformed file with two steps at the
                // same hour would otherwise divide by zero.
                let span = (b - a) as f32;
                let fraction = if span > 0.0 {
                    (unix - a) as f32 / span
                } else {
                    0.0
                };
                return Some((before, step, fraction.clamp(0.0, 1.0)));
            }
        }
        Some((before, before, 0.0))
    }

    /// Wind at a place and a time, interpolated between forecast hours.
    ///
    /// Interpolated in *components*, not in speed and direction. Halfway
    /// between a wind from 350 and one from 010 is a wind from 0; the
    /// arithmetic mean of the bearings is 180, pointing exactly backwards.
    /// Keeping u and v separate all the way from the GRIB file to here is
    /// what makes this a two-line function instead of a special case.
    pub fn wind_at(&self, unix: i64, at: crate::LatLon) -> Option<Wind> {
        let (before, after, fraction) = self.bracket(unix)?;
        let a = before.wind.sample(at)?;
        let b = after.wind.sample(at)?;
        Some(Wind::from_components(
            a.u + (b.u - a.u) * fraction,
            a.v + (b.v - a.v) * fraction,
        ))
    }

    /// Air temperature at a place and a time, Celsius.
    pub fn temperature_at(&self, unix: i64, at: crate::LatLon) -> Option<f32> {
        let (before, after, fraction) = self.bracket(unix)?;
        let a = before.temperature.sample(at)?;
        let b = after.temperature.sample(at)?;
        Some(a + (b - a) * fraction)
    }

    /// The area covered, as north-west and south-east corners.
    pub fn bounds(&self) -> Option<(crate::LatLon, crate::LatLon)> {
        let g = &self.steps.first()?.temperature;
        Some((
            crate::LatLon::new(g.north, g.west),
            crate::LatLon::new(g.south(), g.east()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A forecast over 32-33°N, 118-117°W: 3x3 nodes, two hours, with a
    /// steady westerly that strengthens.
    fn a_forecast() -> Forecast {
        let grid = |v: f32| Grid::new(33.0, -118.0, 0.5, 0.5, 3, 3, vec![v; 9]).expect("grid");
        Forecast {
            // 2026-03-01T00:00:00Z.
            run: 1_772_323_200,
            steps: vec![
                Step {
                    hour: 0,
                    // Blowing towards the east: positive u, zero v.
                    wind: WindField::new(grid(5.0), grid(0.0)).expect("wind"),
                    temperature: grid(16.0),
                },
                Step {
                    hour: 3,
                    wind: WindField::new(grid(10.0), grid(0.0)).expect("wind"),
                    temperature: grid(17.0),
                },
            ],
        }
    }

    #[test]
    fn a_forecast_round_trips_through_its_file_format() {
        let original = a_forecast();
        let parsed = Forecast::parse(&original.to_bytes()).expect("parse");
        assert_eq!(parsed, original);
    }

    #[test]
    fn the_header_is_the_size_the_parser_assumes() {
        // The one number that cannot drift: the writer in
        // scripts/weather-grid.py hard-codes the same offsets.
        let bytes = a_forecast().to_bytes();
        assert_eq!(&bytes[..8], MAGIC);
        let per_step = 4 + 3 * 9 * 4;
        assert_eq!(bytes.len(), HEADER_BYTES + 2 * per_step);
    }

    #[test]
    fn something_that_is_not_a_forecast_is_refused() {
        assert_eq!(Forecast::parse(b""), Err(ForecastError::NotAForecast));
        assert_eq!(
            Forecast::parse(b"not a forecast at all, but long enough to have a header....."),
            Err(ForecastError::NotAForecast)
        );
    }

    #[test]
    fn a_future_format_version_is_refused_rather_than_misread() {
        let mut bytes = a_forecast().to_bytes();
        bytes[8] = 99;
        assert_eq!(Forecast::parse(&bytes), Err(ForecastError::Version(99)));
    }

    #[test]
    fn a_truncated_file_says_so() {
        let full = a_forecast().to_bytes();
        let cut = &full[..full.len() - 10];
        match Forecast::parse(cut) {
            Err(ForecastError::Truncated { expected, got }) => {
                assert_eq!(got, cut.len());
                assert_eq!(expected, full.len());
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[test]
    fn wind_survives_the_round_trip_as_wind() {
        // Not just the bytes: the thing that comes back has to answer the
        // same question. A u/v swap or a sign flip would round-trip
        // perfectly and point the wind the wrong way.
        let parsed = Forecast::parse(&a_forecast().to_bytes()).expect("parse");
        let here = LatLon::new(32.5, -117.5);
        let wind = parsed.steps[0].wind.sample(here).expect("inside the grid");

        assert!((wind.speed_ms() - 5.0).abs() < 1e-6);
        // u positive, v zero: blowing towards the east, so *from* the west.
        assert!(
            (wind.direction_from_deg() - 270.0).abs() < 1e-6,
            "a westerly should read 270, got {}",
            wind.direction_from_deg()
        );
        assert!((wind.direction_to_deg() - 90.0).abs() < 1e-6);
    }

    #[test]
    fn the_nearest_step_is_chosen_and_its_staleness_reported() {
        let f = a_forecast();
        let run = f.run;

        // At the run time: hour 0, not stale.
        let (step, drift) = f.nearest(run).expect("a step");
        assert_eq!(step.hour, 0);
        assert_eq!(drift, 0);

        // One hour in, hour 0 is an hour behind and hour 3 is two ahead,
        // so hour 0 still wins.
        let (step, drift) = f.nearest(run + 3600).expect("a step");
        assert_eq!(step.hour, 0);
        assert_eq!(drift, 3600);

        // Two hours in, the midpoint is passed and hour 3 is closer. The
        // drift is negative: the step is valid in the future.
        let (step, drift) = f.nearest(run + 2 * 3600).expect("a step");
        assert_eq!(step.hour, 3);
        assert_eq!(drift, -3600);

        // Long after the last hour, the staleness is what warns the caller.
        let (step, drift) = f.nearest(run + 100 * 3600).expect("a step");
        assert_eq!(step.hour, 3);
        assert!(drift > 90 * 3600, "drift {drift}");
    }

    #[test]
    fn the_bounds_are_the_grid_the_file_carries() {
        let (nw, se) = a_forecast().bounds().expect("bounds");
        assert!((nw.lat - 33.0).abs() < 1e-9);
        assert!((nw.lon + 118.0).abs() < 1e-9);
        // 3 nodes at 0.5 degrees spans 1 degree.
        assert!((se.lat - 32.0).abs() < 1e-9);
        assert!((se.lon + 117.0).abs() < 1e-9);
    }

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

    // ── Time interpolation ──────────────────────────────────────────────

    /// A forecast whose wind swings across north between two hours.
    ///
    /// The case that decides whether the interpolation is done in the
    /// right space: from 350 degrees to from 010 degrees. Halfway is from
    /// 000; the mean of the bearings is 180, which points at the opposite
    /// horizon.
    fn a_backing_forecast() -> Forecast {
        let grid = |v: f32| Grid::new(33.0, -118.0, 0.5, 0.5, 3, 3, vec![v; 9]).expect("grid");
        // A wind *from* 350 is blowing towards 170: u = sin(170), v = cos(170).
        let from = |bearing_deg: f32, speed: f32| {
            let towards = (bearing_deg + 180.0).to_radians();
            (speed * towards.sin(), speed * towards.cos())
        };
        let (u0, v0) = from(350.0, 10.0);
        let (u1, v1) = from(10.0, 10.0);
        Forecast {
            run: 1_772_323_200,
            steps: vec![
                Step {
                    hour: 0,
                    wind: WindField::new(grid(u0), grid(v0)).expect("wind"),
                    temperature: grid(16.0),
                },
                Step {
                    hour: 3,
                    wind: WindField::new(grid(u1), grid(v1)).expect("wind"),
                    temperature: grid(16.0),
                },
            ],
        }
    }

    #[test]
    fn the_span_is_the_first_and_last_hour() {
        let f = a_forecast();
        let (first, last) = f.span().expect("a span");
        assert_eq!(first, f.run);
        assert_eq!(last, f.run + 3 * 3600);
    }

    #[test]
    fn wind_interpolates_between_the_hours() {
        // The old `nearest` snapped to a step, so the wind jumped every 90
        // minutes. A slider needs it continuous.
        let f = a_forecast();
        let at = LatLon::new(32.5, -117.5);

        let speed = |offset: i64| {
            f.wind_at(f.run + offset, at)
                .expect("inside the forecast")
                .speed_kn()
        };

        // 5 m/s at hour 0, 10 at hour 3, so 7.5 at the midpoint.
        let start = speed(0);
        let middle = speed(90 * 60);
        let end = speed(3 * 3600);
        assert!((start - 9.7).abs() < 0.1, "hour 0 was {start} kn");
        assert!((end - 19.4).abs() < 0.1, "hour 3 was {end} kn");
        assert!(
            (middle - (start + end) / 2.0).abs() < 0.1,
            "the midpoint was {middle}, not halfway between {start} and {end}"
        );

        // And monotonic across the whole span, with no step.
        let mut last = 0.0;
        for minutes in 0..=180 {
            let now = speed(minutes * 60);
            assert!(
                now >= last - 1e-3,
                "the wind went backwards at {minutes} min"
            );
            last = now;
        }
    }

    #[test]
    fn a_wind_backing_through_north_does_not_reverse_halfway() {
        // The whole reason the grids store u and v rather than speed and
        // direction. Interpolating bearings would put this wind on the
        // nose at the midpoint.
        let f = a_backing_forecast();
        let at = LatLon::new(32.5, -117.5);
        let middle = f
            .wind_at(f.run + 90 * 60, at)
            .expect("inside")
            .direction_from_deg();

        // From 350 to from 010 is 20 degrees of backing; halfway is north.
        let off_north = middle.min(360.0 - middle);
        assert!(
            off_north < 1.0,
            "halfway the wind came from {middle} degrees"
        );

        // And the speed must not sag through the turn by more than the
        // geometry demands: two 10 kn winds 20 degrees apart average to
        // 10*cos(10) = 9.85.
        let speed = f.wind_at(f.run + 90 * 60, at).expect("inside").speed_kn();
        let ends = f.wind_at(f.run, at).expect("inside").speed_kn();
        assert!(
            speed > ends * 0.98,
            "the wind sagged from {ends} to {speed} through the turn"
        );
    }

    #[test]
    fn temperature_interpolates_too() {
        let f = a_forecast();
        let at = LatLon::new(32.5, -117.5);
        let start = f.temperature_at(f.run, at).expect("inside");
        let middle = f.temperature_at(f.run + 90 * 60, at).expect("inside");
        assert!((start - 16.0).abs() < 1e-3, "hour 0 was {start}");
        assert!((middle - 16.5).abs() < 1e-3, "the midpoint was {middle}");
    }

    #[test]
    fn nothing_is_claimed_outside_the_forecast() {
        // A slider dragged past the horizon has to show nothing. Clamping
        // to the last step would present a modelled hour as a forecast for
        // a time nobody modelled, which is the one thing a forecast layer
        // must not do.
        let f = a_forecast();
        let at = LatLon::new(32.5, -117.5);
        assert_eq!(f.wind_at(f.run - 1, at), None, "before the run");
        assert_eq!(
            f.wind_at(f.run + 3 * 3600 + 1, at),
            None,
            "past the horizon"
        );
        assert_eq!(f.temperature_at(f.run + 10 * 3600, at), None);

        // And outside the *area*, in time but not in place.
        assert_eq!(f.wind_at(f.run, LatLon::new(0.0, 0.0)), None);
    }

    #[test]
    fn a_single_step_forecast_still_answers_for_its_own_hour() {
        // A fetch where only one hour was published. The bracket has
        // nothing to interpolate towards, and must not divide by zero.
        let grid = |v: f32| Grid::new(33.0, -118.0, 0.5, 0.5, 3, 3, vec![v; 9]).expect("grid");
        let f = Forecast {
            run: 1_772_323_200,
            steps: vec![Step {
                hour: 6,
                wind: WindField::new(grid(5.0), grid(0.0)).expect("wind"),
                temperature: grid(16.0),
            }],
        };
        let at = LatLon::new(32.5, -117.5);
        let valid = f.run + 6 * 3600;
        assert!(f.wind_at(valid, at).is_some(), "its own hour");
        assert_eq!(f.wind_at(valid - 1, at), None);
        assert_eq!(f.wind_at(valid + 1, at), None);
    }

    #[test]
    fn an_empty_forecast_has_no_span_and_no_wind() {
        let f = Forecast {
            run: 1_772_323_200,
            steps: Vec::new(),
        };
        assert_eq!(f.span(), None);
        assert_eq!(f.wind_at(f.run, LatLon::new(32.5, -117.5)), None);
    }
}
