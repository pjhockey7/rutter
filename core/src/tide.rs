//! Harmonic tide prediction.
//!
//! A tide is modelled as a sum of cosines. Each constituent is one
//! astronomical driver — the Moon's principal semidiurnal pull, the Sun's,
//! and so on — with a speed fixed by orbital mechanics and an amplitude and
//! phase fixed by local geography. Predicting a tide is then just evaluating
//! the sum at a time:
//!
//! ```text
//! h(t) = Z0 + Σ Aᵢ · cos(σᵢ·t − φᵢ)
//! ```
//!
//! The arithmetic is the easy part. The amplitudes and phases are the asset,
//! and they are per-station, measured over years. NOAA publishes them for US
//! waters; other hydrographic offices vary in how freely they do.
//!
//! What is deliberately *not* here: nodal corrections, which modulate
//! amplitude and phase over the Moon's 18.6-year regression. Omitting them
//! costs a few percent in amplitude. Fine for a passage plan, not for
//! clearing a bar at low water on a spring tide.

/// One harmonic constituent at one station.
#[derive(Debug, Clone, PartialEq)]
pub struct Constituent {
    /// Darwin symbol, e.g. "M2".
    pub name: String,
    /// Amplitude in metres.
    pub amplitude: f64,
    /// Phase lag in degrees.
    pub phase_deg: f64,
    /// Angular speed in degrees per hour.
    pub speed_deg_per_hour: f64,
}

/// Speeds of the major constituents, degrees per hour.
///
/// These are properties of the solar system, identical everywhere; only
/// amplitude and phase are local.
pub mod speed {
    /// Principal lunar semidiurnal — the dominant term almost everywhere.
    pub const M2: f64 = 28.984_104_2;
    /// Principal solar semidiurnal. Its beat against M2 gives springs and neaps.
    pub const S2: f64 = 30.0;
    /// Larger lunar elliptic semidiurnal.
    pub const N2: f64 = 28.439_729_5;
    /// Luni-solar declinational diurnal.
    pub const K1: f64 = 15.041_068_6;
    /// Principal lunar diurnal.
    pub const O1: f64 = 13.943_035_6;
    /// Principal solar diurnal.
    pub const P1: f64 = 14.958_931_4;
}

/// A tidal station: a datum plus its constituents.
#[derive(Debug, Clone, Default)]
pub struct Station {
    pub name: String,
    /// Mean water level above chart datum, metres.
    pub datum_m: f64,
    pub constituents: Vec<Constituent>,
}

impl Station {
    /// Height above chart datum, metres, at `hours` after the epoch the
    /// phases are referenced to.
    pub fn height_at(&self, hours: f64) -> f64 {
        self.datum_m
            + self
                .constituents
                .iter()
                .map(|c| {
                    let angle = (c.speed_deg_per_hour * hours - c.phase_deg).to_radians();
                    c.amplitude * angle.cos()
                })
                .sum::<f64>()
    }

    /// Sample the curve at a fixed step, yielding `(hours, metres)`.
    pub fn curve(&self, from_hours: f64, to_hours: f64, step_hours: f64) -> Vec<(f64, f64)> {
        if step_hours <= 0.0 || to_hours < from_hours {
            return Vec::new();
        }
        let n = ((to_hours - from_hours) / step_hours).floor() as usize;
        (0..=n)
            .map(|i| {
                let t = from_hours + i as f64 * step_hours;
                (t, self.height_at(t))
            })
            .collect()
    }

    /// Turning points of the curve over a window: the high and low waters.
    ///
    /// Found by sampling and keeping sign changes in the slope, then
    /// refining each by bisection on the derivative. Sampling finer than the
    /// shortest constituent period is what stops a peak being missed; the
    /// default step is well inside that for the diurnal and semidiurnal
    /// terms above.
    pub fn extremes(&self, from_hours: f64, to_hours: f64) -> Vec<Extreme> {
        const STEP: f64 = 0.25;
        const EPS: f64 = 1e-4;

        let slope = |t: f64| (self.height_at(t + EPS) - self.height_at(t - EPS)) / (2.0 * EPS);

        let mut out = Vec::new();
        let mut t = from_hours;
        let mut prev = slope(t);

        while t < to_hours {
            let next_t = (t + STEP).min(to_hours);
            let next = slope(next_t);

            if prev.signum() != next.signum() {
                // Bisect on the derivative to place the turning point.
                let (mut lo, mut hi) = (t, next_t);
                for _ in 0..40 {
                    let mid = (lo + hi) / 2.0;
                    if slope(lo).signum() == slope(mid).signum() {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                let at = (lo + hi) / 2.0;
                out.push(Extreme {
                    hours: at,
                    height_m: self.height_at(at),
                    // Classified from the slope *after* the turning point,
                    // not before it. When a peak lands exactly on a sample —
                    // which it does at t=0 for any constituent with zero
                    // phase — the slope there is exactly zero, and a
                    // `prev > 0.0` test reads +0.0 as falling and labels a
                    // high water a low. The far-side slope is a quarter of
                    // an hour away from the peak and never degenerate.
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
    pub hours: f64,
    pub height_m: f64,
    pub kind: Kind,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m2_only() -> Station {
        Station {
            name: "test".into(),
            datum_m: 3.0,
            constituents: vec![Constituent {
                name: "M2".into(),
                amplitude: 2.0,
                phase_deg: 0.0,
                speed_deg_per_hour: speed::M2,
            }],
        }
    }

    fn approx(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() < tol, "expected {b}, got {a}");
    }

    #[test]
    fn datum_only_station_is_flat() {
        let s = Station {
            name: "flat".into(),
            datum_m: 1.5,
            constituents: vec![],
        };
        approx(s.height_at(0.0), 1.5, 1e-12);
        approx(s.height_at(37.2), 1.5, 1e-12);
    }

    #[test]
    fn m2_starts_at_high_water() {
        // Zero phase means the cosine peaks at t=0.
        approx(m2_only().height_at(0.0), 5.0, 1e-9);
    }

    #[test]
    fn m2_period_is_about_12_42_hours() {
        let s = m2_only();
        let period = 360.0 / speed::M2;
        approx(period, 12.42, 0.01);
        // One period later the height repeats.
        approx(s.height_at(period), s.height_at(0.0), 1e-6);
    }

    #[test]
    fn low_water_is_half_a_period_after_high() {
        let s = m2_only();
        approx(s.height_at(360.0 / speed::M2 / 2.0), 1.0, 1e-6);
    }

    #[test]
    fn extremes_alternate_and_match_the_curve() {
        let s = m2_only();
        let ex = s.extremes(0.0, 26.0);

        assert!(
            ex.len() >= 4,
            "expected at least four turning points, got {}",
            ex.len()
        );
        for pair in ex.windows(2) {
            assert_ne!(pair[0].kind, pair[1].kind, "highs and lows must alternate");
        }
        for e in &ex {
            approx(e.height_m, s.height_at(e.hours), 1e-6);
            match e.kind {
                Kind::High => approx(e.height_m, 5.0, 1e-3),
                Kind::Low => approx(e.height_m, 1.0, 1e-3),
            }
        }
    }

    #[test]
    fn springs_exceed_neaps_when_m2_and_s2_combine() {
        // M2 and S2 reinforce every ~14.8 days and cancel between: the
        // spring/neap cycle. A station with both must show a bigger range at
        // the reinforcement than at the cancellation.
        let s = Station {
            name: "springs".into(),
            datum_m: 3.0,
            constituents: vec![
                Constituent {
                    name: "M2".into(),
                    amplitude: 2.0,
                    phase_deg: 0.0,
                    speed_deg_per_hour: speed::M2,
                },
                Constituent {
                    name: "S2".into(),
                    amplitude: 0.7,
                    phase_deg: 0.0,
                    speed_deg_per_hour: speed::S2,
                },
            ],
        };

        let range_over = |from: f64, to: f64| {
            let c = s.curve(from, to, 0.05);
            let hi = c.iter().map(|p| p.1).fold(f64::MIN, f64::max);
            let lo = c.iter().map(|p| p.1).fold(f64::MAX, f64::min);
            hi - lo
        };

        // In phase at t=0; the beat period is ~354.4 h, so half of that is neaps.
        let springs = range_over(0.0, 13.0);
        let neaps = range_over(177.0, 190.0);
        assert!(
            springs > neaps * 1.5,
            "springs {springs} should dwarf neaps {neaps}"
        );
    }

    #[test]
    fn curve_rejects_nonsense_windows() {
        let s = m2_only();
        assert!(s.curve(10.0, 0.0, 0.5).is_empty());
        assert!(s.curve(0.0, 10.0, 0.0).is_empty());
    }
}
