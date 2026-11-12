//! Harmonic tide prediction.
//!
//! A tide is a sum of cosines. Each constituent is one astronomical driver —
//! the Moon's principal semidiurnal pull, the Sun's, and so on — with a
//! speed fixed by orbital mechanics and an amplitude and phase fixed by
//! local geography:
//!
//! ```text
//! h(t) = Z0 + Σ fᵢ · Aᵢ · cos(Vᵢ(t) + uᵢ − κᵢ)
//! ```
//!
//! `A` and `κ` are the asset. They are per-station, measured over years, and
//! NOAA publishes them for US waters — 37 constituents per station, which is
//! a few hundred bytes. Everything else here is arithmetic, which is why a
//! boat with no signal can still know the tide: fetch the constituents once,
//! predict forever.
//!
//! The three terms that are *not* the published numbers:
//!
//! * `V` is the equilibrium argument — where the relevant bodies actually
//!   are at time `t`. Polynomials in the mean longitudes, [`Astro`].
//! * `f` is the node factor and `u` the nodal phase correction. Both track
//!   the 18.6-year regression of the Moon's ascending node. Omitting them
//!   costs 4.7 cm RMS at San Diego; including them costs 1.0 cm, which is
//!   the floor set by the constituents NOAA has and we cannot separate.
//! * `Z0` is the height of mean sea level above chart datum, so that a
//!   prediction reads as a sounding does.
//!
//! Everything in here was checked against NOAA's own published predictions
//! rather than against a textbook; see `core/tests/tide_noaa.rs`. That
//! matters because several of these formulae are easy to get subtly wrong in
//! ways a plausibility check will not catch — a mismatched epoch between the
//! polynomials and the lunar-time term puts a flat 6.6° into every
//! constituent, which looks like a small phase error and is actually a
//! wrong tide.

/// Seconds from the Unix epoch back to 1900-01-01T00:00:00Z.
///
/// The astronomical polynomials below are referenced to that instant, and
/// so is the lunar-time term. They *must* agree: referencing the
/// polynomials to 1899-12-31 12:00 (Schureman's own tabulation epoch, and
/// the one most references quote) while measuring hours from midnight puts
/// half a day of lunar motion — 6.6° — into `s`, which then leaks into
/// every constituent in proportion to its `s` coefficient.
const EPOCH_UNIX: i64 = -2_208_988_800;

const SECONDS_PER_HOUR: f64 = 3600.0;
/// Julian century, in hours.
const HOURS_PER_CENTURY: f64 = 36525.0 * 24.0;

/// Obliquity of the ecliptic, degrees.
const OBLIQUITY: f64 = 23.4397;
/// Inclination of the Moon's orbit to the ecliptic, degrees.
const LUNAR_INCLINATION: f64 = 5.1453;

/// Hours from the epoch, as a float. Fractional seconds are not meaningful
/// for a tide, but keeping the whole thing in hours keeps every speed in its
/// published units of degrees per hour.
fn hours_since_epoch(unix: i64) -> f64 {
    (unix - EPOCH_UNIX) as f64 / SECONDS_PER_HOUR
}

/// The mean longitudes a constituent's argument is built from, in degrees.
///
/// Not reduced to a range: the arguments are only ever used inside a cosine,
/// and reducing each term separately would cost accuracy for nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Astro {
    /// Lunar time, `15·t + h − s`. The hour angle of the mean moon.
    pub tau: f64,
    /// Mean longitude of the Moon.
    pub s: f64,
    /// Mean longitude of the Sun.
    pub h: f64,
    /// Mean longitude of the lunar perigee.
    pub p: f64,
    /// Mean longitude of the Moon's ascending node.
    pub n: f64,
    /// Mean longitude of the solar perigee.
    pub p1: f64,
}

impl Astro {
    /// Evaluate the polynomials at a Unix time.
    pub fn at(unix: i64) -> Self {
        let hours = hours_since_epoch(unix);
        let t = hours / HOURS_PER_CENTURY; // Julian centuries

        let s = 277.0247 + 481267.8906 * t + 0.0020 * t * t;
        let h = 280.1895 + 36000.7689 * t + 0.0003 * t * t;
        let p = 334.3853 + 4069.0340 * t - 0.0103 * t * t;
        let n = 259.1568 - 1934.1420 * t + 0.0021 * t * t;
        let p1 = 281.2209 + 1.7192 * t + 0.0005 * t * t;

        Self {
            tau: 15.0 * hours + h - s,
            s,
            h,
            p,
            n,
            p1,
        }
    }
}

/// Rates of change of the six arguments, degrees per hour.
///
/// Only used to derive each constituent's speed from its Doodson numbers,
/// which is how [`Harmonic::speed_deg_per_hour`] is checked against the
/// speeds NOAA publishes.
const RATES: [f64; 6] = [
    14.492_052_11, // tau
    0.549_016_53,  // s
    0.041_068_64,  // h
    0.004_641_83,  // p
    -0.002_206_41, // N'
    0.000_001_96,  // p1
];

/// Which nodal correction a constituent takes.
///
/// Most constituents share one of a handful of rules, because the node
/// enters through the Moon's orbit and not through the individual term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Correction {
    /// Solar: the lunar node does not touch it. `f = 1`, `u = 0`.
    Solar,
    /// The lunar semidiurnal group.
    Lunar2,
    /// Lunar semidiurnal with the sign of `u` reversed (2SM2).
    Lunar2Inverted,
    /// The lunar diurnal group.
    Lunar1,
    K1,
    K2,
    J1,
    Oo1,
    Mm,
    Mf,
    M3,
    /// `f_M2^n`, `u = n·u_M2` — the overtides and the lunar compounds.
    Lunar2Power(u8),
    /// M2 × K1.
    Mk3,
    /// M2² ÷ K1.
    TwoMk3,
}

/// One row of the constituent table.
struct Row {
    name: &'static str,
    /// Coefficients of (tau, s, h, p, N', p1).
    doodson: [i8; 6],
    /// Phase offset, in units of 90°.
    offset: i8,
    correction: Correction,
}

/// The 37 constituents NOAA publishes per station.
///
/// Each Doodson row was checked by rebuilding the constituent's speed from
/// [`RATES`] and comparing against NOAA's published speed; see
/// `doodson_rows_reproduce_published_speeds`. That test is the reason to
/// trust this table — the rows are easy to mistype and a wrong row produces
/// a tide that is wrong only at some times.
const TABLE: &[Row] = &[
    // name       tau   s   h   p  N' p1  off  correction
    row("M2", [2, 0, 0, 0, 0, 0], 0, Correction::Lunar2),
    row("S2", [2, 2, -2, 0, 0, 0], 0, Correction::Solar),
    row("N2", [2, -1, 0, 1, 0, 0], 0, Correction::Lunar2),
    row("K1", [1, 1, 0, 0, 0, 0], 1, Correction::K1),
    row("M4", [4, 0, 0, 0, 0, 0], 0, Correction::Lunar2Power(2)),
    row("O1", [1, -1, 0, 0, 0, 0], -1, Correction::Lunar1),
    row("M6", [6, 0, 0, 0, 0, 0], 0, Correction::Lunar2Power(3)),
    row("MK3", [3, 1, 0, 0, 0, 0], 1, Correction::Mk3),
    row("S4", [4, 4, -4, 0, 0, 0], 0, Correction::Solar),
    row("MN4", [4, -1, 0, 1, 0, 0], 0, Correction::Lunar2Power(2)),
    row("NU2", [2, -1, 2, -1, 0, 0], 0, Correction::Lunar2),
    row("S6", [6, 6, -6, 0, 0, 0], 0, Correction::Solar),
    row("MU2", [2, -2, 2, 0, 0, 0], 0, Correction::Lunar2),
    row("2N2", [2, -2, 0, 2, 0, 0], 0, Correction::Lunar2),
    row("OO1", [1, 3, 0, 0, 0, 0], 1, Correction::Oo1),
    row("LAM2", [2, 1, -2, 1, 0, 0], 2, Correction::Lunar2),
    // S1's offset is 0: it is a solar term and carries no 90° step.
    row("S1", [1, 1, -1, 0, 0, 0], 0, Correction::Solar),
    // M1 and L2 split on the lunar perigee. Schureman gives each its own
    // f and u; both were tried and came out *worse* than simply taking the
    // parent group's correction (1.5 cm RMS against 1.0 cm), so the parent
    // is what is used. Between them they carry 25 mm of a 1.5 m tide, so
    // the approximation is worth less than the risk of a subtly wrong
    // perigee formula.
    row("M1", [1, 0, 0, 1, 0, 0], 1, Correction::Lunar1),
    row("J1", [1, 2, 0, -1, 0, 0], 1, Correction::J1),
    row("MM", [0, 1, 0, -1, 0, 0], 0, Correction::Mm),
    row("SSA", [0, 0, 2, 0, 0, 0], 0, Correction::Solar),
    row("SA", [0, 0, 1, 0, 0, 0], 0, Correction::Solar),
    row("MSF", [0, 2, -2, 0, 0, 0], 0, Correction::Lunar2),
    row("MF", [0, 2, 0, 0, 0, 0], 0, Correction::Mf),
    row("RHO", [1, -2, 2, -1, 0, 0], -1, Correction::Lunar1),
    row("Q1", [1, -2, 0, 1, 0, 0], -1, Correction::Lunar1),
    row("T2", [2, 2, -3, 0, 0, 1], 0, Correction::Solar),
    row("R2", [2, 2, -1, 0, 0, -1], 2, Correction::Solar),
    row("2Q1", [1, -3, 0, 2, 0, 0], -1, Correction::Lunar1),
    row("P1", [1, 1, -2, 0, 0, 0], -1, Correction::Solar),
    row("2SM2", [2, 4, -4, 0, 0, 0], 0, Correction::Lunar2Inverted),
    row("M3", [3, 0, 0, 0, 0, 0], 0, Correction::M3),
    row("L2", [2, 1, 0, -1, 0, 0], 2, Correction::Lunar2),
    row("2MK3", [3, -1, 0, 0, 0, 0], -1, Correction::TwoMk3),
    row("K2", [2, 2, 0, 0, 0, 0], 0, Correction::K2),
    row("M8", [8, 0, 0, 0, 0, 0], 0, Correction::Lunar2Power(4)),
    row("MS4", [4, 2, -2, 0, 0, 0], 0, Correction::Lunar2),
];

const fn row(name: &'static str, doodson: [i8; 6], offset: i8, correction: Correction) -> Row {
    Row {
        name,
        doodson,
        offset,
        correction,
    }
}

/// One of the constituents in [`TABLE`], by index.
///
/// An index rather than a 37-variant enum so that the table stays the single
/// place a constituent is described. Constructing one is only possible
/// through [`Harmonic::from_name`], so the index is always in range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Harmonic(u8);

impl Harmonic {
    /// Look up a constituent by its Darwin symbol, as NOAA spells it.
    ///
    /// Case-insensitive, because the spelling varies between NOAA's own
    /// endpoints ("RHO" in one place, "rho" in another).
    pub fn from_name(name: &str) -> Option<Self> {
        TABLE
            .iter()
            .position(|r| r.name.eq_ignore_ascii_case(name))
            .map(|i| Self(i as u8))
    }

    pub fn name(self) -> &'static str {
        self.row().name
    }

    /// Every constituent this build understands.
    pub fn all() -> impl Iterator<Item = Self> {
        (0..TABLE.len()).map(|i| Self(i as u8))
    }

    fn row(self) -> &'static Row {
        &TABLE[self.0 as usize]
    }

    /// Angular speed in degrees per hour, derived from the Doodson numbers.
    ///
    /// Derived rather than stored so that there is only one description of
    /// each constituent. The test against NOAA's published speeds is what
    /// makes that safe.
    pub fn speed_deg_per_hour(self) -> f64 {
        self.row()
            .doodson
            .iter()
            .zip(RATES)
            .map(|(&c, rate)| f64::from(c) * rate)
            .sum()
    }

    /// The equilibrium argument `V`, degrees, at the given time.
    pub fn equilibrium(self, a: &Astro) -> f64 {
        let d = self.row().doodson;
        f64::from(d[0]) * a.tau
            + f64::from(d[1]) * a.s
            + f64::from(d[2]) * a.h
            + f64::from(d[3]) * a.p
            + f64::from(d[4]) * -a.n
            + f64::from(d[5]) * a.p1
            + f64::from(self.row().offset) * 90.0
    }

    /// The node factor `f` and nodal phase correction `u`, degrees.
    pub fn nodal(self, a: &Astro) -> (f64, f64) {
        self.nodal_with(&Nodal::at(a))
    }

    /// The same, against an already-computed [`Nodal`].
    ///
    /// [`Nodal::at`] costs an `acos`, two `atan2`s and a pile of
    /// trigonometry, and it is identical for every constituent at a given
    /// instant — it describes where the Moon's node is, not which term is
    /// being evaluated. Calling `nodal` per constituent therefore did that
    /// work thirty-odd times per height, and a tide curve is hundreds of
    /// heights. Hoisting it out is most of what made the panel cheap.
    fn nodal_with(self, n: &Nodal) -> (f64, f64) {
        match self.row().correction {
            Correction::Solar => (1.0, 0.0),
            Correction::Lunar2 => (n.f_m2, n.u_m2),
            Correction::Lunar2Inverted => (n.f_m2, -n.u_m2),
            Correction::Lunar1 => (n.f_o1, n.u_o1),
            Correction::K1 => (n.f_k1, n.u_k1),
            Correction::K2 => (n.f_k2, n.u_k2),
            Correction::J1 => (n.sin_2i / 0.7214, -n.nu.to_degrees()),
            Correction::Oo1 => (
                n.sin_i * (n.i / 2.0).sin().powi(2) / 0.0164,
                (-2.0 * n.xi - n.nu).to_degrees(),
            ),
            Correction::Mm => ((2.0 / 3.0 - n.sin_i * n.sin_i) / 0.5021, 0.0),
            Correction::Mf => (n.sin_i * n.sin_i / 0.1578, (-2.0 * n.xi).to_degrees()),
            // u_M3 is 3ξ − 3ν, and u_M2 is 2ξ − 2ν, so one is one and a
            // half times the other.
            Correction::M3 => ((n.i / 2.0).cos().powi(6) / 0.8758, 1.5 * n.u_m2),
            Correction::Lunar2Power(k) => (n.f_m2.powi(i32::from(k)), f64::from(k) * n.u_m2),
            Correction::Mk3 => (n.f_m2 * n.f_k1, n.u_m2 + n.u_k1),
            Correction::TwoMk3 => (n.f_m2 * n.f_m2 * n.f_k1, 2.0 * n.u_m2 - n.u_k1),
        }
    }
}

/// The slowly-varying quantities every nodal correction is built from.
///
/// All of these move on the 18.6-year cycle of the Moon's node, so they are
/// computed once per prediction rather than once per constituent.
#[derive(Debug, Clone, Copy)]
struct Nodal {
    /// Inclination of the Moon's orbit to the equator, radians.
    i: f64,
    sin_i: f64,
    sin_2i: f64,
    /// Longitude of the intersection in the Moon's orbit, from the node.
    xi: f64,
    /// Right ascension of the intersection.
    nu: f64,
    f_m2: f64,
    u_m2: f64,
    f_o1: f64,
    u_o1: f64,
    f_k1: f64,
    u_k1: f64,
    f_k2: f64,
    u_k2: f64,
}

/// Reduce an angle in radians to `(-pi, pi]`.
fn wrap_pi(radians: f64) -> f64 {
    let turned = (radians + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU);
    turned - std::f64::consts::PI
}

impl Nodal {
    fn at(a: &Astro) -> Self {
        let omega = OBLIQUITY.to_radians();
        let inc = LUNAR_INCLINATION.to_radians();

        // Reduce the node to one turn before taking its sine, and keep the
        // wrapped value: `xi` is a difference against it, so an unreduced
        // `n` would give an `xi` thousands of degrees from the right answer.
        let n_deg = a.n.rem_euclid(360.0);
        let n = n_deg.to_radians();

        let i = (inc.cos() * omega.cos() - inc.sin() * omega.sin() * n.cos()).acos();
        let sin_i = i.sin();
        let sin_2i = (2.0 * i).sin();

        // The spherical triangle equinox-node-intersection. The known parts
        // are the side N (along the ecliptic) and the angles at its ends —
        // the obliquity at the equinox, the lunar inclination at the node.
        // Wanted are the other two sides: the arc along the Moon's orbit
        // from the node to the intersection, and the arc along the equator
        // from the equinox to the intersection.
        //
        // Napier's analogies give their half-sum and half-difference. Using
        // the law of sines and an `asin` instead looks simpler and is
        // wrong: `asin` returns the principal value, so the arcs come out
        // in the wrong quadrant for every node longitude between 90° and
        // 270°. That is half of the 18.6-year cycle, and the symptom is a
        // tide that is correct for nine years and badly wrong for nine —
        // which is why the envelope test below exists.
        let half = n / 2.0;
        let sum = (((omega - inc) / 2.0).cos() * half.sin())
            .atan2(((omega + inc) / 2.0).cos() * half.cos());
        let difference = (((omega - inc) / 2.0).sin() * half.sin())
            .atan2(((omega + inc) / 2.0).sin() * half.cos());
        let orbital = sum + difference;
        let equatorial = sum - difference;

        // `nu` is the equatorial arc. `xi` is the node *minus* the orbital
        // arc, not the arc itself: the arc runs out to 23° while xi stays
        // inside 12°, and using the arc directly puts about 30° into u_M2 —
        // a two-hour error in the time of high water.
        //
        // Both are wrapped to a half turn. Every use below has an integer
        // coefficient, so a whole turn would be free anyway, but the
        // envelope test reads these directly.
        let nu = wrap_pi(equatorial);
        let xi = wrap_pi(n - orbital);

        // K1 and K2 are lunisolar: the solar half does not feel the node, so
        // they take a blended argument rather than `nu` itself.
        let nu_prime = (sin_2i * nu.sin()).atan2(sin_2i * nu.cos() + 0.3347);
        let nu_double = 0.5
            * (sin_i * sin_i * (2.0 * nu).sin()).atan2(sin_i * sin_i * (2.0 * nu).cos() + 0.0727);

        let cos_half_i = (i / 2.0).cos();

        Self {
            i,
            sin_i,
            sin_2i,
            xi,
            nu,
            // Schureman's reduction-to-mean constants, so that each `f` is
            // 1.0 at mean obliquity rather than at the cycle's extreme.
            f_m2: cos_half_i.powi(4) / 0.9154,
            u_m2: (2.0 * xi - 2.0 * nu).to_degrees(),
            f_o1: sin_i * cos_half_i.powi(2) / 0.3800,
            u_o1: (2.0 * xi - nu).to_degrees(),
            f_k1: (0.8965 * sin_2i * sin_2i + 0.6001 * sin_2i * nu.cos() + 0.1006).sqrt(),
            u_k1: -nu_prime.to_degrees(),
            f_k2: (19.0444 * sin_i.powi(4) + 2.7702 * sin_i * sin_i * (2.0 * nu).cos() + 0.0981)
                .sqrt(),
            u_k2: -2.0 * nu_double.to_degrees(),
        }
    }
}

/// One harmonic constituent at one station.
#[derive(Debug, Clone, PartialEq)]
pub struct Constituent {
    pub harmonic: Harmonic,
    /// Amplitude, in whatever the station measures.
    ///
    /// Metres of height for a tide station; knots along the major axis for
    /// a current station. The harmonic arithmetic does not care which —
    /// it is the same sum either way, which is why [`harmonic_sum`] is
    /// shared and why this field is not named for a unit.
    pub amplitude: f64,
    /// Phase lag on the equilibrium argument at Greenwich, degrees.
    ///
    /// NOAA calls this `phase_GMT`. It is a lag, so it is subtracted.
    pub phase_deg: f64,
}

/// A tidal station: a datum plus its constituents.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Station {
    /// NOAA's station identifier, e.g. "9410170".
    pub id: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    /// Height of mean sea level above chart datum, metres.
    ///
    /// For a NOAA station this is `MSL − MLLW`: the harmonic sum is about
    /// mean sea level, while a chart's soundings and a navigator's clearance
    /// are about lowest water. Getting this wrong offsets every prediction
    /// by a constant, which is the one error that *is* easy to spot.
    pub datum_m: f64,
    pub constituents: Vec<Constituent>,
}

/// The harmonic sum at an instant: `Σ f·A·cos(V + u − κ)`.
///
/// The whole of tidal prediction, in six lines. Shared between heights and
/// currents because it is literally the same calculation — a tide station
/// and a current station differ in what the amplitude *means*, not in how
/// the terms combine. Sharing it means the node factors, the epoch and the
/// quadrant handling are validated once rather than twice.
pub fn harmonic_sum(constituents: &[Constituent], unix: i64) -> f64 {
    let a = Astro::at(unix);
    // Both of these are properties of the instant rather than of any one
    // constituent, so they are computed once per sum and not once per
    // term. See [`Harmonic::nodal_with`] — hoisting this out was a sixfold
    // speedup.
    let n = Nodal::at(&a);
    constituents
        .iter()
        .map(|c| {
            let (f, u) = c.harmonic.nodal_with(&n);
            let angle = (c.harmonic.equilibrium(&a) + u - c.phase_deg).to_radians();
            f * c.amplitude * angle.cos()
        })
        .sum()
}

impl Station {
    /// Height above chart datum, metres, at a Unix time.
    pub fn height_at(&self, unix: i64) -> f64 {
        self.datum_m + harmonic_sum(&self.constituents, unix)
    }

    /// Sample the curve at a fixed step, yielding `(unix, metres)`.
    pub fn curve(&self, from: i64, to: i64, step_secs: i64) -> Vec<(i64, f64)> {
        if step_secs <= 0 || to < from {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut t = from;
        while t <= to {
            out.push((t, self.height_at(t)));
            t += step_secs;
        }
        out
    }

    /// Turning points over a window: the high and low waters.
    ///
    /// Found by sampling and keeping sign changes in the slope, then
    /// refining each by bisection on the derivative. The step has to be
    /// well inside the shortest constituent period or a peak is missed;
    /// fifteen minutes is comfortable against M8, the fastest term NOAA
    /// publishes, whose period is three hours.
    pub fn extremes(&self, from: i64, to: i64) -> Vec<Extreme> {
        const STEP: i64 = 15 * 60;
        // A minute either side. Small enough that the slope is local, large
        // enough that it is not reading f64 noise.
        const EPS: i64 = 60;

        let slope =
            |t: i64| (self.height_at(t + EPS) - self.height_at(t - EPS)) / (2.0 * EPS as f64);

        let mut out = Vec::new();
        let mut t = from;
        let mut prev = slope(t);

        while t < to {
            let next_t = (t + STEP).min(to);
            let next = slope(next_t);

            if prev.signum() != next.signum() {
                let (mut lo, mut hi) = (t, next_t);
                // Bisect to the second; 15 minutes needs ten halvings.
                while hi - lo > 1 {
                    let mid = lo + (hi - lo) / 2;
                    if slope(lo).signum() == slope(mid).signum() {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                let at = lo + (hi - lo) / 2;
                out.push(Extreme {
                    unix: at,
                    height_m: self.height_at(at),
                    // Classified from the slope *after* the turning point,
                    // not before it. When a peak lands exactly on a sample
                    // the slope there is exactly zero, and a `prev > 0.0`
                    // test reads +0.0 as falling and labels a high water a
                    // low. The far-side slope is never degenerate.
                    kind: if next < 0.0 { Kind::High } else { Kind::Low },
                });
            }

            prev = next;
            t = next_t;
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    High,
    Low,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Extreme {
    pub unix: i64,
    pub height_m: f64,
    pub kind: Kind,
}

/// A tidal current station: a flood axis plus its constituents.
///
/// NOAA publishes about 4,500 of these, and they are predicted by exactly
/// the same harmonic sum as a tide height — the amplitudes are knots along
/// the channel instead of metres of rise. Which is why this type shares
/// [`harmonic_sum`] and [`Constituent`] rather than restating them.
///
/// ## Why this is a station and not a field
///
/// A current follows the channel it is in. Two stations a mile apart can
/// have the flood running in quite different directions, and halfway
/// between them the water may be going sideways or there may be no water
/// at all. So this predicts the current *at a place NOAA measured one* and
/// nothing is interpolated between them: an arrow per station, which is
/// what a tidal current chart has always been.
///
/// A genuine gridded flow field needs a hydrodynamic model — NOAA's WCOFS
/// covers this coast — and is a quite separate thing to ingest.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CurrentStation {
    /// NOAA's identifier, e.g. "PCT0031". Not the same namespace as the
    /// tide stations' numeric ids.
    pub id: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    /// Where the flood sets, degrees true.
    ///
    /// The flood is the rising-tide direction, usually up a channel or
    /// into a harbour. The harmonic sum is signed along this axis.
    pub flood_deg: f64,
    /// Where the ebb sets, degrees true.
    ///
    /// Usually the reciprocal of the flood, but NOAA publishes it
    /// separately because in a bend it is not: the water takes the inside
    /// of the curve going one way and the outside coming back.
    pub ebb_deg: f64,
    /// Depth of the bin this prediction describes, metres below the
    /// surface. `None` for a surface station.
    ///
    /// Worth carrying because a deep bin in a stratified estuary can be
    /// running the other way from the surface, and a boat cares about the
    /// surface.
    pub depth_m: Option<f64>,
    /// A constant set, knots, added to the harmonic sum.
    ///
    /// Zero at nearly every station. Not zero in a river, where the net
    /// seaward flow never reverses however the tide runs — and a predictor
    /// that dropped it would show slack water in a current that is still
    /// setting a knot and a half.
    pub mean_kn: f64,
    /// Amplitudes in knots along the flood axis. See [`Constituent`].
    pub constituents: Vec<Constituent>,
}

/// What the water is doing at an instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Current {
    /// How fast, knots. Never negative — the direction carries the sign.
    pub speed_kn: f64,
    /// Where the water is going, degrees true.
    ///
    /// Where it is *going*, not where it comes from. The opposite
    /// convention to wind, and deliberately so: this is the convention for
    /// a current on every chart and in every tidal atlas, and quietly
    /// flipping it would put a two-knot set on the wrong bow.
    pub toward_deg: f64,
    /// Whether this is the flood. False on the ebb, and on slack water,
    /// where the direction means little anyway.
    pub flooding: bool,
}

impl CurrentStation {
    /// Speed along the flood axis, knots. Positive floods, negative ebbs.
    ///
    /// The raw signed quantity, before it is turned into a direction. Kept
    /// public because the turning points are found on it: a maximum of the
    /// signed speed is peak flood, a minimum is peak ebb, and a zero is
    /// slack water, and all three are obvious on the signed value and
    /// awkward on the absolute one.
    pub fn along_axis_kn(&self, unix: i64) -> f64 {
        self.mean_kn + harmonic_sum(&self.constituents, unix)
    }

    /// The current at a Unix time.
    pub fn current_at(&self, unix: i64) -> Current {
        let along = self.along_axis_kn(unix);
        let flooding = along >= 0.0;
        Current {
            speed_kn: along.abs(),
            toward_deg: if flooding {
                self.flood_deg
            } else {
                self.ebb_deg
            },
            flooding,
        }
    }

    /// Sample the signed along-axis speed, yielding `(unix, knots)`.
    pub fn curve(&self, from: i64, to: i64, step_secs: i64) -> Vec<(i64, f64)> {
        if step_secs <= 0 || to < from {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut t = from;
        while t <= to {
            out.push((t, self.along_axis_kn(t)));
            t += step_secs;
        }
        out
    }

    /// Slack waters and peak flows over a window, in time order.
    ///
    /// Both kinds, because they answer different questions. Slack is when
    /// to pass through a narrows; peak flood and ebb are what you fight or
    /// ride, and when. A tidal current table prints exactly these four
    /// events a day, in this order.
    pub fn events(&self, from: i64, to: i64) -> Vec<CurrentEvent> {
        /// Fifteen minutes, as for the tide: comfortably inside the period
        /// of M8, the fastest term NOAA publishes.
        const STEP: i64 = 15 * 60;
        const EPS: i64 = 60;

        if to <= from {
            return Vec::new();
        }

        let slope = |t: i64| {
            (self.along_axis_kn(t + EPS) - self.along_axis_kn(t - EPS)) / (2.0 * EPS as f64)
        };

        // Bisect for a sign change in whatever `f` is, to the second.
        let refine = |f: &dyn Fn(i64) -> f64, mut lo: i64, mut hi: i64| {
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                if f(lo).signum() == f(mid).signum() {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            hi
        };

        let mut out: Vec<CurrentEvent> = Vec::new();
        let mut t = from;
        let (mut prev_slope, mut prev_value) = (slope(t), self.along_axis_kn(t));

        while t < to {
            let next_t = (t + STEP).min(to);
            let next_slope = slope(next_t);
            let next_value = self.along_axis_kn(next_t);

            // A turning point in the signed speed: peak flood or peak ebb.
            if prev_slope.signum() != next_slope.signum() {
                let at = refine(&slope, t, next_t);
                let along = self.along_axis_kn(at);
                out.push(CurrentEvent {
                    unix: at,
                    speed_kn: along.abs(),
                    toward_deg: if along >= 0.0 {
                        self.flood_deg
                    } else {
                        self.ebb_deg
                    },
                    kind: if along >= 0.0 {
                        CurrentKind::MaxFlood
                    } else {
                        CurrentKind::MaxEbb
                    },
                });
            }

            // A zero crossing: slack water. In a river with a net flow
            // there may be none at all, which is correct and is why this
            // is a sign test rather than an assumed four-a-day.
            if prev_value.signum() != next_value.signum() {
                let value = |t: i64| self.along_axis_kn(t);
                let at = refine(&value, t, next_t);
                out.push(CurrentEvent {
                    unix: at,
                    speed_kn: 0.0,
                    // At slack the water is not going anywhere, so naming a
                    // direction would be inventing one. The flood axis is
                    // reported for orientation and the speed says it is
                    // nothing.
                    toward_deg: self.flood_deg,
                    kind: CurrentKind::Slack,
                });
            }

            t = next_t;
            prev_slope = next_slope;
            prev_value = next_value;
        }

        out.sort_by_key(|e| e.unix);
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrentKind {
    /// The strongest flood of this cycle.
    MaxFlood,
    /// The strongest ebb.
    MaxEbb,
    /// The current has stopped and is about to turn.
    Slack,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CurrentEvent {
    pub unix: i64,
    /// Knots. Zero at slack.
    pub speed_kn: f64,
    pub toward_deg: f64,
    pub kind: CurrentKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NOAA's published speeds, degrees per hour, for all 37 constituents.
    ///
    /// Taken verbatim from
    /// `api.tidesandcurrents.noaa.gov/mdapi/prod/webapi/stations/9410170/harcon.json`.
    /// They are a property of the solar system, so they are the same at
    /// every station and make an independent check on the Doodson table.
    const PUBLISHED_SPEEDS: &[(&str, f64)] = &[
        ("M2", 28.984104),
        ("S2", 30.0),
        ("N2", 28.43973),
        ("K1", 15.041069),
        ("M4", 57.96821),
        ("O1", 13.943035),
        ("M6", 86.95232),
        ("MK3", 44.025173),
        ("S4", 60.0),
        ("MN4", 57.423832),
        ("NU2", 28.512583),
        ("S6", 90.0),
        ("MU2", 27.968208),
        ("2N2", 27.895355),
        ("OO1", 16.139101),
        ("LAM2", 29.455626),
        ("S1", 15.0),
        ("M1", 14.496694),
        ("J1", 15.585444),
        ("MM", 0.544375),
        ("SSA", 0.082137),
        ("SA", 0.041069),
        ("MSF", 1.015896),
        ("MF", 1.098033),
        ("RHO", 13.471515),
        ("Q1", 13.398661),
        ("T2", 29.958933),
        ("R2", 30.041067),
        ("2Q1", 12.854286),
        ("P1", 14.958931),
        ("2SM2", 31.015896),
        ("M3", 43.47616),
        ("L2", 29.528479),
        ("2MK3", 42.92714),
        ("K2", 30.082138),
        ("M8", 115.93642),
        ("MS4", 58.984104),
    ];

    #[test]
    fn the_table_holds_every_constituent_noaa_publishes() {
        assert_eq!(TABLE.len(), PUBLISHED_SPEEDS.len());
        for (name, _) in PUBLISHED_SPEEDS {
            assert!(
                Harmonic::from_name(name).is_some(),
                "{name} is published but not in the table"
            );
        }
    }

    #[test]
    fn doodson_rows_reproduce_published_speeds() {
        // The whole point of deriving speed from the Doodson numbers rather
        // than storing it: a mistyped row cannot survive this.
        let mut worst = 0.0f64;
        for (name, published) in PUBLISHED_SPEEDS {
            let h = Harmonic::from_name(name).expect("in the table");
            let err = (h.speed_deg_per_hour() - published).abs();
            assert!(
                err < 1e-4,
                "{name}: doodson gives {}, NOAA publishes {published}",
                h.speed_deg_per_hour()
            );
            worst = worst.max(err);
        }
        assert!(worst < 1e-4, "worst speed mismatch {worst}");
    }

    #[test]
    fn names_are_unique_and_round_trip() {
        for h in Harmonic::all() {
            assert_eq!(Harmonic::from_name(h.name()), Some(h));
        }
        let mut names: Vec<&str> = Harmonic::all().map(|h| h.name()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate constituent name");
    }

    #[test]
    fn lookup_is_case_insensitive() {
        assert_eq!(Harmonic::from_name("m2"), Harmonic::from_name("M2"));
        assert_eq!(Harmonic::from_name("rho"), Harmonic::from_name("RHO"));
        assert_eq!(Harmonic::from_name("not a constituent"), None);
    }

    /// 2026-03-01T00:00:00Z.
    const T2026: i64 = 1_772_323_200;

    #[test]
    fn the_epoch_constant_is_1900() {
        // 70 years, 17 of them leap, before the Unix epoch.
        assert_eq!(EPOCH_UNIX, -(70 * 365 + 17) * 86400);
        assert_eq!(hours_since_epoch(EPOCH_UNIX), 0.0);
    }

    #[test]
    fn node_factors_stay_in_their_known_ranges() {
        // The envelopes below are the published ones for a full nodal
        // cycle, and they are tight on purpose. A quadrant mistake in the
        // spherical triangle keeps ξ and ν plausible near a node longitude
        // of zero and sends them to ±180° in between — correct tides for
        // nine years, badly wrong for the next nine. Loose bounds would
        // have let that through; these did not.
        //
        // Sampled every five weeks over sixty years, so more than three
        // full cycles and every part of each.
        for k in 0..620 {
            let unix = T2026 + k * 35 * 86400;
            let a = Astro::at(unix);
            let n = Nodal::at(&a);
            let at = |label: &str, v: f64, lo: f64, hi: f64| {
                assert!(
                    v >= lo && v <= hi,
                    "{label} = {v} outside {lo}..{hi} at unix {unix}"
                );
            };

            at("I", n.i.to_degrees(), 18.28, 28.60);
            at("xi", n.xi.to_degrees(), -12.00, 12.00);
            at("nu", n.nu.to_degrees(), -13.04, 13.04);
            at("f_M2", n.f_m2, 0.9632, 1.0380);
            at("u_M2", n.u_m2, -2.14, 2.14);
            at("f_O1", n.f_o1, 0.8051, 1.1825);
            at("u_O1", n.u_o1, -10.96, 10.96);
            at("f_K1", n.f_k1, 0.8813, 1.1127);
            at("u_K1", n.u_k1, -8.91, 8.91);
            at("f_K2", n.f_k2, 0.7455, 1.3155);
            at("u_K2", n.u_k2, -17.84, 17.84);

            for h in Harmonic::all() {
                let (f, u) = h.nodal(&a);
                assert!(f.is_finite() && f > 0.0, "{} f={f}", h.name());
                assert!(u.is_finite(), "{} u={u}", h.name());
            }
        }
    }

    #[test]
    fn the_node_factors_are_continuous_across_the_cycle() {
        // The quadrant bug showed up as a 360° step in ξ. Nothing here is
        // allowed to jump: these quantities move on an 18.6-year timescale,
        // so a day apart they must be almost identical.
        let mut previous: Option<(f64, f64, f64)> = None;
        for k in 0..7000 {
            let a = Astro::at(T2026 + k * 86400);
            let n = Nodal::at(&a);
            if let Some((xi, nu, u_m2)) = previous {
                assert!(
                    (n.xi.to_degrees() - xi).abs() < 0.1,
                    "xi jumped from {xi} to {}",
                    n.xi.to_degrees()
                );
                assert!(
                    (n.nu.to_degrees() - nu).abs() < 0.1,
                    "nu jumped from {nu} to {}",
                    n.nu.to_degrees()
                );
                assert!(
                    (n.u_m2 - u_m2).abs() < 0.1,
                    "u_M2 jumped from {u_m2} to {}",
                    n.u_m2
                );
            }
            previous = Some((n.xi.to_degrees(), n.nu.to_degrees(), n.u_m2));
        }
    }

    #[test]
    fn solar_constituents_ignore_the_node() {
        let a = Astro::at(T2026);
        for name in ["S2", "P1", "SA", "SSA", "T2", "R2", "S1", "S4", "S6"] {
            let (f, u) = Harmonic::from_name(name).expect("in table").nodal(&a);
            assert_eq!((f, u), (1.0, 0.0), "{name} should be unaffected");
        }
    }

    #[test]
    fn the_lunar_semidiurnal_group_shares_one_correction() {
        // Measuring f and u out of NOAA's own predictions showed M2, N2,
        // NU2, 2N2 and MU2 all landing on the same correction. If a future
        // edit gives one of them its own rule, this says so.
        let a = Astro::at(T2026);
        let m2 = Harmonic::from_name("M2").expect("in table").nodal(&a);
        for name in ["N2", "NU2", "2N2", "MU2", "LAM2", "MSF", "L2"] {
            let other = Harmonic::from_name(name).expect("in table").nodal(&a);
            assert_eq!(m2, other, "{name} should share M2's correction");
        }
    }

    fn m2_only() -> Station {
        Station {
            id: "test".into(),
            name: "test".into(),
            lat: 0.0,
            lon: 0.0,
            datum_m: 3.0,
            constituents: vec![Constituent {
                harmonic: Harmonic::from_name("M2").expect("in table"),
                amplitude: 2.0,
                phase_deg: 0.0,
            }],
        }
    }

    fn approx(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() < tol, "expected {b}, got {a}");
    }

    #[test]
    fn datum_only_station_is_flat() {
        let s = Station {
            datum_m: 1.5,
            ..Station::default()
        };
        approx(s.height_at(T2026), 1.5, 1e-12);
        approx(s.height_at(T2026 + 133_700), 1.5, 1e-12);
    }

    #[test]
    fn m2_period_is_about_12_42_hours() {
        let s = m2_only();
        let period = 360.0
            / Harmonic::from_name("M2")
                .expect("in table")
                .speed_deg_per_hour();
        approx(period, 12.42, 0.01);
        let secs = (period * 3600.0).round() as i64;
        // One period later the height repeats. Not exactly: f and u drift,
        // but over twelve hours that is microscopic.
        approx(s.height_at(T2026 + secs), s.height_at(T2026), 1e-4);
    }

    #[test]
    fn m2_range_is_twice_its_amplitude() {
        let s = m2_only();
        let c = s.curve(T2026, T2026 + 13 * 3600, 60);
        let hi = c.iter().map(|p| p.1).fold(f64::MIN, f64::max);
        let lo = c.iter().map(|p| p.1).fold(f64::MAX, f64::min);
        // f_M2 is not exactly 1, so the range is 4·f rather than 4.
        approx(hi - lo, 4.0, 0.2);
    }

    #[test]
    fn extremes_alternate_and_match_the_curve() {
        let s = m2_only();
        let ex = s.extremes(T2026, T2026 + 26 * 3600);

        assert!(
            ex.len() >= 4,
            "expected at least four turning points, got {}",
            ex.len()
        );
        for pair in ex.windows(2) {
            assert_ne!(pair[0].kind, pair[1].kind, "highs and lows must alternate");
        }
        for e in &ex {
            approx(e.height_m, s.height_at(e.unix), 1e-6);
        }
        // Highs above the datum, lows below it.
        for e in &ex {
            match e.kind {
                Kind::High => assert!(e.height_m > 4.0, "high at {}", e.height_m),
                Kind::Low => assert!(e.height_m < 2.0, "low at {}", e.height_m),
            }
        }
    }

    #[test]
    fn springs_exceed_neaps_when_m2_and_s2_combine() {
        // M2 and S2 reinforce every ~14.8 days and cancel between: the
        // spring/neap cycle. A station with both must show a bigger range
        // at the reinforcement than at the cancellation.
        let s = Station {
            id: "springs".into(),
            name: "springs".into(),
            lat: 0.0,
            lon: 0.0,
            datum_m: 3.0,
            constituents: vec![
                Constituent {
                    harmonic: Harmonic::from_name("M2").expect("in table"),
                    amplitude: 2.0,
                    phase_deg: 0.0,
                },
                Constituent {
                    harmonic: Harmonic::from_name("S2").expect("in table"),
                    amplitude: 0.7,
                    phase_deg: 0.0,
                },
            ],
        };

        let range_over = |from: i64, hours: i64| {
            let c = s.curve(from, from + hours * 3600, 180);
            let hi = c.iter().map(|p| p.1).fold(f64::MIN, f64::max);
            let lo = c.iter().map(|p| p.1).fold(f64::MAX, f64::min);
            hi - lo
        };

        // Find a spring and a neap by scanning a full beat period rather
        // than assuming where they fall: the phases are referenced to the
        // equilibrium argument, so t=0 is not springs any more.
        let mut biggest = f64::MIN;
        let mut smallest = f64::MAX;
        for day in 0..30 {
            let r = range_over(T2026 + day * 86400, 13);
            biggest = biggest.max(r);
            smallest = smallest.min(r);
        }
        assert!(
            biggest > smallest * 1.5,
            "springs {biggest} should dwarf neaps {smallest}"
        );
    }

    #[test]
    fn curve_rejects_nonsense_windows() {
        let s = m2_only();
        assert!(s.curve(T2026 + 100, T2026, 60).is_empty());
        assert!(s.curve(T2026, T2026 + 100, 0).is_empty());
        assert!(s.curve(T2026, T2026 + 100, -60).is_empty());
    }
}
