//! Positions, distances and bearings.
//!
//! Both the great-circle and rhumb-line forms are here because they answer
//! different questions. A great circle is the shortest path but its bearing
//! changes continuously; a rhumb line holds one bearing the whole way, which
//! is what you actually steer and what a Mercator chart draws as a straight
//! line. For passages of a few tens of miles the difference is small; across
//! an ocean it is not.
//!
//! Everything is WGS84 spherical. That is accurate to roughly 0.3% against
//! the ellipsoid, which is well inside the error of steering by hand, but it
//! is the wrong basis for survey work.

/// Mean Earth radius in metres (IUGG).
pub const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// A geographic position in degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatLon {
    pub lat: f64,
    pub lon: f64,
}

impl LatLon {
    pub const fn new(lat: f64, lon: f64) -> Self {
        Self { lat, lon }
    }

    fn lat_rad(&self) -> f64 {
        self.lat.to_radians()
    }

    fn lon_rad(&self) -> f64 {
        self.lon.to_radians()
    }

    /// Great-circle distance in metres.
    pub fn distance_to(&self, other: LatLon) -> f64 {
        let (p1, p2) = (self.lat_rad(), other.lat_rad());
        let dp = p2 - p1;
        let dl = other.lon_rad() - self.lon_rad();

        // Haversine: numerically better than the spherical law of cosines for
        // the short distances that dominate coastal navigation.
        let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
        2.0 * a.sqrt().asin() * EARTH_RADIUS_M
    }

    /// Initial great-circle bearing in degrees true, 0..360.
    ///
    /// "Initial" matters: follow a great circle and this value drifts, so it
    /// is the heading to set now, not for the whole leg.
    pub fn bearing_to(&self, other: LatLon) -> f64 {
        let (p1, p2) = (self.lat_rad(), other.lat_rad());
        let dl = other.lon_rad() - self.lon_rad();

        let y = dl.sin() * p2.cos();
        let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
        normalise_deg(y.atan2(x).to_degrees())
    }

    /// Constant bearing in degrees true for the rhumb line to `other`.
    pub fn rhumb_bearing_to(&self, other: LatLon) -> f64 {
        let (p1, p2) = (self.lat_rad(), other.lat_rad());
        let mut dl = other.lon_rad() - self.lon_rad();

        let dpsi = stretched_latitude_delta(p1, p2);

        // Always take the shorter way round the planet.
        if dl.abs() > std::f64::consts::PI {
            dl = if dl > 0.0 {
                -(2.0 * std::f64::consts::PI - dl)
            } else {
                2.0 * std::f64::consts::PI + dl
            };
        }
        normalise_deg(dl.atan2(dpsi).to_degrees())
    }

    /// Distance in metres along the rhumb line to `other`.
    ///
    /// Longer than the great circle, and the figure a chart's plotted course
    /// actually corresponds to.
    pub fn rhumb_distance_to(&self, other: LatLon) -> f64 {
        let (p1, p2) = (self.lat_rad(), other.lat_rad());
        let dp = p2 - p1;
        let mut dl = (other.lon_rad() - self.lon_rad()).abs();
        if dl > std::f64::consts::PI {
            dl = 2.0 * std::f64::consts::PI - dl;
        }

        let dpsi = stretched_latitude_delta(p1, p2);

        // q is cos(latitude) scaled for the Mercator stretch. As the course
        // approaches due east/west dpsi tends to zero and the ratio becomes
        // ill-conditioned, so fall back to cos(lat) there.
        let q = if dpsi.abs() > 1e-12 {
            dp / dpsi
        } else {
            p1.cos()
        };

        (dp * dp + q * q * dl * dl).sqrt() * EARTH_RADIUS_M
    }
}

/// Difference in "stretched" (Mercator) latitude between two latitudes.
fn stretched_latitude_delta(p1: f64, p2: f64) -> f64 {
    let quarter_pi = std::f64::consts::FRAC_PI_4;
    ((p2 / 2.0 + quarter_pi).tan() / (p1 / 2.0 + quarter_pi).tan()).ln()
}

/// Wrap degrees into 0..360.
pub fn normalise_deg(deg: f64) -> f64 {
    let d = deg % 360.0;
    if d < 0.0 {
        d + 360.0
    } else {
        d
    }
}

/// Metres to nautical miles.
pub fn metres_to_nm(m: f64) -> f64 {
    m / 1852.0
}

#[cfg(test)]
mod tests {
    use super::*;

    // Lizard Point to Île d'Ouessant, a leg with published figures.
    const LIZARD: LatLon = LatLon::new(49.9599, -5.2049);
    const OUESSANT: LatLon = LatLon::new(48.4603, -5.0959);

    fn approx(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() < tol, "expected {b}, got {a}");
    }

    #[test]
    fn distance_to_self_is_zero() {
        approx(LIZARD.distance_to(LIZARD), 0.0, 1e-6);
        approx(LIZARD.rhumb_distance_to(LIZARD), 0.0, 1e-6);
    }

    #[test]
    fn one_degree_of_latitude_is_sixty_nm() {
        let a = LatLon::new(0.0, 0.0);
        let b = LatLon::new(1.0, 0.0);
        approx(metres_to_nm(a.distance_to(b)), 60.0, 0.1);
    }

    #[test]
    fn due_south_bearings_agree() {
        // On a meridian the great circle and the rhumb line are the same
        // line, so both bearings must read due south.
        let a = LatLon::new(50.0, -5.0);
        let b = LatLon::new(48.0, -5.0);
        approx(a.bearing_to(b), 180.0, 1e-6);
        approx(a.rhumb_bearing_to(b), 180.0, 1e-6);
    }

    #[test]
    fn due_east_rhumb_holds_090() {
        // Along a parallel the rhumb line is due east the whole way, while
        // the great circle's initial bearing is not. This is the case that
        // breaks if the dpsi -> 0 branch is wrong.
        let a = LatLon::new(50.0, -5.0);
        let b = LatLon::new(50.0, -3.0);
        approx(a.rhumb_bearing_to(b), 90.0, 1e-6);
        assert!(
            a.bearing_to(b) < 90.0,
            "great circle should bear north of east"
        );
    }

    #[test]
    fn rhumb_is_never_shorter_than_great_circle() {
        let gc = LIZARD.distance_to(OUESSANT);
        let rl = LIZARD.rhumb_distance_to(OUESSANT);
        assert!(rl >= gc - 1e-6, "rhumb {rl} shorter than great circle {gc}");
    }

    #[test]
    fn channel_crossing_is_about_ninety_nm() {
        approx(metres_to_nm(LIZARD.distance_to(OUESSANT)), 90.0, 1.0);
    }

    #[test]
    fn bearings_are_normalised() {
        let a = LatLon::new(50.0, 0.0);
        let b = LatLon::new(51.0, -1.0);
        let brg = a.bearing_to(b);
        assert!((0.0..360.0).contains(&brg), "bearing {brg} out of range");
        assert!(brg > 270.0, "north-west leg should bear north of west");
    }

    #[test]
    fn antimeridian_takes_the_short_way() {
        let a = LatLon::new(0.0, 179.0);
        let b = LatLon::new(0.0, -179.0);
        // Two degrees apart across the dateline, not 358.
        approx(metres_to_nm(a.rhumb_distance_to(b)), 120.0, 1.0);
        approx(a.rhumb_bearing_to(b), 90.0, 1e-6);
    }
}
