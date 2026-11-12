//! NMEA 0183: what boat instruments say.
//!
//! One ASCII line per message, and the whole protocol is visible in an
//! example:
//!
//! ```text
//! $GPRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W*6A
//!  │ │   │      │ │          │          │     │      │      │
//!  │ │   │      │ │          │          │     │      │      └ magnetic variation
//!  │ │   │      │ │          │          │     │      └ date, ddmmyy
//!  │ │   │      │ │          │          │     └ course over ground, true
//!  │ │   │      │ │          │          └ speed over ground, knots
//!  │ │   │      │ └ latitude └ longitude
//!  │ │   │      └ status: A valid, V warning
//!  │ │   └ time, hhmmss
//!  │ └ sentence type
//!  └ talker: GP GPS, GN multi-constellation, SD sounder, WI wind, II instruments
//! ```
//!
//! This is a parser and nothing else: no sockets, no serial port, no clock.
//! It turns a line into a [`Sentence`], and [`Vessel`] accumulates those
//! into the current picture of the boat. Where the lines come from — TCP,
//! UDP, a USB receiver, a phone relaying its own fix — is the platform's
//! business.
//!
//! Four things about real NMEA that are easy to get wrong, and all four have
//! a test below:
//!
//! * **Positions are degrees and decimal *minutes*,** `ddmm.mmmm`, not
//!   decimal degrees. Reading `4807.038` as 48.07038° puts you 60 miles
//!   away, which is the kind of error that looks plausible on a zoomed-out
//!   chart.
//! * **The checksum matters.** It is an XOR of everything between the `$`
//!   and the `*`. A line that fails it is discarded rather than used: a
//!   corrupted position is far worse than no position, because the boat
//!   moves to it.
//! * **Fields are routinely empty.** A receiver with no fix still sends
//!   RMC, just with blanks, and `A`/`V` is the field that says so. Parsing
//!   the blanks as zero puts the boat on Null Island.
//! * **The talker varies.** The same sentence arrives as `$GPRMC` from a
//!   GPS, `$GNRMC` from a multi-constellation receiver and `$IIRMC` from an
//!   instrument bus, so dispatch is on the last three characters.
//!
//! Not handled here: AIS, which arrives as `!AIVDM` carrying a six-bit
//! packed binary payload and is a decoder in its own right.

use crate::geo::LatLon;

/// The sentences this build understands.
///
/// Anything else is ignored rather than refused — every device emits
/// proprietary sentences, and a chart plotter that fell over on an
/// unrecognised one would be useless on a real boat.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sentence {
    /// Recommended minimum: position, speed, course, and whether to believe
    /// them. The one sentence almost every receiver sends.
    Rmc {
        /// `None` when the receiver reports no fix.
        position: Option<LatLon>,
        /// Speed over ground, knots.
        sog_kn: Option<f64>,
        /// Course over ground, degrees true.
        cog_deg: Option<f64>,
        /// Seconds since midnight UTC, from the time field.
        time_of_day: Option<u32>,
        valid: bool,
    },
    /// Fix data: position with the quality indicator and satellite count.
    Gga {
        position: Option<LatLon>,
        /// 0 is no fix, 1 GPS, 2 differential, 4/5 RTK.
        quality: u8,
        satellites: Option<u8>,
        /// Horizontal dilution of precision; smaller is better.
        hdop: Option<f64>,
    },
    /// Course and speed, with no position.
    Vtg {
        cog_deg: Option<f64>,
        sog_kn: Option<f64>,
    },
    /// Position only.
    Gll {
        position: Option<LatLon>,
        valid: bool,
    },
    /// Depth below the transducer, metres.
    Dbt { metres: Option<f64> },
    /// Depth below the transducer plus the offset to the waterline or keel.
    Dpt {
        metres: Option<f64>,
        /// Positive to the waterline, negative to the keel.
        offset_m: Option<f64>,
    },
    /// Wind speed and angle, apparent or true.
    Mwv {
        /// Degrees; relative to the bow when apparent, true when not.
        angle_deg: Option<f64>,
        speed_kn: Option<f64>,
        /// False means apparent — relative to the moving boat.
        true_wind: bool,
        valid: bool,
    },
    /// Heading, true.
    Hdt { deg: Option<f64> },
    /// Heading, magnetic, with the deviation and variation to correct it.
    Hdg {
        magnetic_deg: Option<f64>,
        variation_deg: Option<f64>,
    },
}

/// Why a line was not usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    /// No leading `$` or `!`.
    NotASentence,
    /// The checksum is present and does not match.
    Checksum { stated: u8, computed: u8 },
    /// Understood the shape, but not this sentence type.
    Unsupported,
    /// Too few fields to mean anything.
    Truncated,
}

/// The XOR checksum of a sentence's body.
fn checksum(body: &str) -> u8 {
    body.bytes().fold(0u8, |acc, b| acc ^ b)
}

/// Split a line into its body and stated checksum, validating the latter.
///
/// A missing `*xx` is accepted: some devices and most log files omit it, and
/// refusing those would reject data that is otherwise perfectly good. A
/// *wrong* one is never accepted.
fn body_of(line: &str) -> Result<&str, Invalid> {
    let line = line.trim();
    let rest = line
        .strip_prefix('$')
        .or_else(|| line.strip_prefix('!'))
        .ok_or(Invalid::NotASentence)?;

    match rest.split_once('*') {
        Some((body, stated)) => {
            // Exactly two hex digits; anything else is a corrupt line.
            let stated = u8::from_str_radix(stated.get(..2).unwrap_or(""), 16).map_err(|_| {
                Invalid::Checksum {
                    stated: 0,
                    computed: checksum(body),
                }
            })?;
            let computed = checksum(body);
            if stated == computed {
                Ok(body)
            } else {
                Err(Invalid::Checksum { stated, computed })
            }
        }
        None => Ok(rest),
    }
}

/// A field, or `None` if it is absent or empty.
fn field<'a>(fields: &[&'a str], index: usize) -> Option<&'a str> {
    fields
        .get(index)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
}

fn number(fields: &[&str], index: usize) -> Option<f64> {
    field(fields, index)?.parse().ok()
}

/// Degrees and decimal minutes with a hemisphere, as NMEA writes a position.
///
/// `ddmm.mmmm` for latitude and `dddmm.mmmm` for longitude: the degrees are
/// however many digits precede the last two before the point. Returning
/// `None` for anything malformed rather than guessing is deliberate — half
/// a parsed position is a wrong position.
fn degrees_minutes(value: &str, hemisphere: &str) -> Option<f64> {
    let point = value.find('.').unwrap_or(value.len());
    if point < 3 {
        return None;
    }
    let (degrees, minutes) = value.split_at(point - 2);
    let degrees: f64 = degrees.parse().ok()?;
    let minutes: f64 = minutes.parse().ok()?;
    if !(0.0..60.0).contains(&minutes) {
        return None;
    }

    let magnitude = degrees + minutes / 60.0;
    match hemisphere {
        "N" | "E" => Some(magnitude),
        "S" | "W" => Some(-magnitude),
        _ => None,
    }
}

/// A position from a (lat, N/S, lon, E/W) run of four fields.
fn position(fields: &[&str], at: usize) -> Option<LatLon> {
    let lat = degrees_minutes(field(fields, at)?, field(fields, at + 1)?)?;
    let lon = degrees_minutes(field(fields, at + 2)?, field(fields, at + 3)?)?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    Some(LatLon::new(lat, lon))
}

/// `hhmmss` or `hhmmss.sss` to seconds since midnight.
fn time_of_day(value: &str) -> Option<u32> {
    if value.len() < 6 {
        return None;
    }
    let hours: u32 = value.get(..2)?.parse().ok()?;
    let minutes: u32 = value.get(2..4)?.parse().ok()?;
    let seconds: u32 = value.get(4..6)?.parse().ok()?;
    if hours > 23 || minutes > 59 || seconds > 60 {
        return None;
    }
    Some(hours * 3600 + minutes * 60 + seconds)
}

/// Parse one line.
pub fn parse(line: &str) -> Result<Sentence, Invalid> {
    let body = body_of(line)?;
    let fields: Vec<&str> = body.split(',').collect();
    let address = fields.first().copied().ok_or(Invalid::Truncated)?;
    if address.len() < 3 {
        return Err(Invalid::Truncated);
    }

    // The last three characters, so that GP, GN, GL, II, SD and WI talkers
    // all reach the same arm.
    let kind = &address[address.len() - 3..];

    match kind {
        "RMC" => Ok(Sentence::Rmc {
            valid: field(&fields, 2) == Some("A"),
            position: position(&fields, 3),
            sog_kn: number(&fields, 7),
            cog_deg: number(&fields, 8),
            time_of_day: field(&fields, 1).and_then(time_of_day),
        }),
        "GGA" => Ok(Sentence::Gga {
            position: position(&fields, 2),
            quality: number(&fields, 6).unwrap_or(0.0) as u8,
            satellites: number(&fields, 7).map(|n| n as u8),
            hdop: number(&fields, 8),
        }),
        "VTG" => Ok(Sentence::Vtg {
            cog_deg: number(&fields, 1),
            sog_kn: number(&fields, 5),
        }),
        "GLL" => Ok(Sentence::Gll {
            position: position(&fields, 1),
            valid: field(&fields, 6) == Some("A"),
        }),
        "DBT" => Ok(Sentence::Dbt {
            // Three units in one sentence — feet, metres, fathoms. Metres is
            // field 3, and taking it rather than converting from feet avoids
            // a rounding trip through a unit nobody wanted.
            metres: number(&fields, 3).or_else(|| number(&fields, 1).map(|f| f * 0.3048)),
        }),
        "DPT" => Ok(Sentence::Dpt {
            metres: number(&fields, 1),
            offset_m: number(&fields, 2),
        }),
        "MWV" => Ok(Sentence::Mwv {
            angle_deg: number(&fields, 1),
            speed_kn: match field(&fields, 4) {
                // The unit is stated per sentence and is not always knots.
                Some("N") => number(&fields, 3),
                Some("M") => number(&fields, 3).map(|v| v * 1.943_844),
                Some("K") => number(&fields, 3).map(|v| v * 0.539_957),
                _ => number(&fields, 3),
            },
            true_wind: field(&fields, 2) == Some("T"),
            valid: field(&fields, 5) == Some("A"),
        }),
        "HDT" => Ok(Sentence::Hdt {
            deg: number(&fields, 1),
        }),
        "HDG" => Ok(Sentence::Hdg {
            magnetic_deg: number(&fields, 1),
            // East variation is added, west subtracted.
            variation_deg: number(&fields, 4).map(|v| {
                if field(&fields, 5) == Some("W") {
                    -v
                } else {
                    v
                }
            }),
        }),
        _ => Err(Invalid::Unsupported),
    }
}

/// What the instruments currently say about the boat.
///
/// Accumulated rather than taken from one sentence, because no single
/// sentence has everything: position and speed come from RMC, depth from a
/// sounder, wind from a masthead unit, each at its own rate. Every field
/// carries the time it was set so a stale one can be told from a live one —
/// an instrument that stops transmitting leaves its last value sitting
/// there looking current, which is the failure a navigator least wants.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Vessel {
    pub position: Option<Stamped<LatLon>>,
    /// Course over ground, degrees true.
    pub cog_deg: Option<Stamped<f64>>,
    /// Speed over ground, knots.
    pub sog_kn: Option<Stamped<f64>>,
    /// Heading, degrees true — from a compass, not from movement.
    pub heading_deg: Option<Stamped<f64>>,
    /// Depth below the transducer, metres.
    pub depth_m: Option<Stamped<f64>>,
    /// Apparent wind: angle relative to the bow, and speed.
    pub apparent_wind: Option<Stamped<(f64, f64)>>,
    /// Satellites used, when a GGA said.
    pub satellites: Option<u8>,
    /// GGA's fix quality: 0 none, 1 GPS, 2 differential, 4/5 RTK.
    pub fix_quality: u8,
    /// Horizontal accuracy in metres, when the source states one.
    ///
    /// Always `None` from NMEA: a GGA carries HDOP, which is a unitless
    /// multiplier on an unstated receiver error and *not* a distance.
    /// Converting one to the other needs a number only the receiver's
    /// maker knows, so claiming metres from HDOP would be an invention.
    /// A phone's own GPS does state metres, which is one of the reasons
    /// its fixes are not pushed through the NMEA parser to get here.
    pub accuracy_m: Option<Stamped<f64>>,
}

/// A position report from something that is not NMEA.
///
/// A phone's location service hands over a structure, not a sentence.
/// Formatting it into `$GPRMC,…`, computing a checksum, and then
/// immediately verifying that checksum to parse it back would be a round
/// trip through a text format for no gain — and a lossy one, because NMEA
/// has nowhere to put the accuracy. So it fills a [`Vessel`] directly.
///
/// This lives beside `Vessel` rather than in a module of its own because
/// `Vessel` is what it is for, and `Vessel` is transport-independent even
/// though it is spelled in the NMEA module.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fix {
    pub at: LatLon,
    /// Speed over ground in knots, if the source measured one. A phone
    /// reports metres per second; converting is the platform's job, so
    /// that every number reaching here is in the unit the panel shows.
    pub sog_kn: Option<f64>,
    /// Course over ground, degrees true.
    pub cog_deg: Option<f64>,
    /// Horizontal accuracy in metres.
    pub accuracy_m: Option<f64>,
    /// Satellites used, if known.
    pub satellites: Option<u8>,
    /// When the *source* says the fix was taken, in Unix seconds.
    ///
    /// Not when it was read. A phone's cached location can be minutes old
    /// — that is the normal state of a location service that nothing has
    /// asked for updates from — and stamping it with the time it was
    /// collected would present a fix from the last anchorage as current.
    pub taken: i64,
}

/// A value and when it arrived.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stamped<T> {
    pub value: T,
    /// Unix seconds, as the caller reckons them. This module has no clock.
    pub at: i64,
}

impl<T> Stamped<T> {
    pub fn new(value: T, at: i64) -> Self {
        Self { value, at }
    }

    /// Whether this is older than `limit` seconds at time `now`.
    pub fn stale(&self, now: i64, limit: i64) -> bool {
        now - self.at > limit
    }
}

impl Vessel {
    /// Fold one sentence in. `now` is the arrival time, in Unix seconds.
    ///
    /// Invalid sentences are *not* applied: an RMC with status V, or a GLL
    /// with no fix, leaves the previous position alone rather than clearing
    /// it. Losing a fix momentarily should not make the boat vanish from
    /// the chart — it should make the position go stale, which the
    /// timestamps express.
    /// Take a fix from a source that is not NMEA.
    ///
    /// Stamped with the fix's own time, not the time it arrived: see
    /// [`Fix::taken`]. Everything that reads staleness off `Stamped::at`
    /// therefore tells the truth about a cached position without needing
    /// to know where it came from.
    ///
    /// Quality is reported as 1 — a plain fix — rather than invented. A
    /// phone will not say whether it is differential, and claiming it was
    /// would make the panel more confident than the data.
    pub fn apply_fix(&mut self, fix: Fix) {
        self.position = Some(Stamped::new(fix.at, fix.taken));
        if let Some(v) = fix.sog_kn {
            self.sog_kn = Some(Stamped::new(v, fix.taken));
        }
        if let Some(v) = fix.cog_deg {
            self.cog_deg = Some(Stamped::new(v, fix.taken));
        }
        if let Some(v) = fix.accuracy_m {
            self.accuracy_m = Some(Stamped::new(v, fix.taken));
        }
        if fix.satellites.is_some() {
            self.satellites = fix.satellites;
        }
        self.fix_quality = self.fix_quality.max(1);
    }

    pub fn apply(&mut self, sentence: Sentence, now: i64) {
        match sentence {
            Sentence::Rmc {
                position,
                sog_kn,
                cog_deg,
                valid,
                ..
            } => {
                if !valid {
                    return;
                }
                if let Some(p) = position {
                    self.position = Some(Stamped::new(p, now));
                }
                if let Some(v) = sog_kn {
                    self.sog_kn = Some(Stamped::new(v, now));
                }
                if let Some(v) = cog_deg {
                    self.cog_deg = Some(Stamped::new(v, now));
                }
            }
            Sentence::Gga {
                position,
                quality,
                satellites,
                ..
            } => {
                self.fix_quality = quality;
                self.satellites = satellites;
                // Quality 0 means no fix, whatever the position field says.
                if quality > 0 {
                    if let Some(p) = position {
                        self.position = Some(Stamped::new(p, now));
                    }
                }
            }
            Sentence::Vtg { cog_deg, sog_kn } => {
                if let Some(v) = cog_deg {
                    self.cog_deg = Some(Stamped::new(v, now));
                }
                if let Some(v) = sog_kn {
                    self.sog_kn = Some(Stamped::new(v, now));
                }
            }
            Sentence::Gll { position, valid } => {
                if valid {
                    if let Some(p) = position {
                        self.position = Some(Stamped::new(p, now));
                    }
                }
            }
            Sentence::Dbt { metres } | Sentence::Dpt { metres, .. } => {
                if let Some(v) = metres {
                    self.depth_m = Some(Stamped::new(v, now));
                }
            }
            Sentence::Mwv {
                angle_deg,
                speed_kn,
                true_wind,
                valid,
            } => {
                // Only apparent wind is kept. True wind from an instrument
                // is computed from apparent plus the boat's own motion, and
                // mixing the two in one field would make the number mean
                // different things at different moments.
                if valid && !true_wind {
                    if let (Some(a), Some(s)) = (angle_deg, speed_kn) {
                        self.apparent_wind = Some(Stamped::new((a, s), now));
                    }
                }
            }
            Sentence::Hdt { deg } => {
                if let Some(v) = deg {
                    self.heading_deg = Some(Stamped::new(v, now));
                }
            }
            Sentence::Hdg {
                magnetic_deg,
                variation_deg,
            } => {
                // Magnetic to true, when the variation is given. Without it
                // the magnetic heading is kept as-is rather than discarded:
                // on this coast the variation is about 11 degrees, which is
                // worth having wrong rather than not having at all.
                if let Some(v) = magnetic_deg {
                    let true_deg = crate::geo::normalise_deg(v + variation_deg.unwrap_or(0.0));
                    self.heading_deg = Some(Stamped::new(true_deg, now));
                }
            }
        }
    }

    /// Whether there is a position recent enough to steer by.
    pub fn has_fix(&self, now: i64, limit: i64) -> bool {
        self.position.is_some_and(|p| !p.stale(now, limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real RMC, with the checksum it actually carries.
    const RMC: &str = "$GPRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W*6A";

    #[test]
    fn the_worked_example_parses() {
        let Ok(Sentence::Rmc {
            position,
            sog_kn,
            cog_deg,
            time_of_day,
            valid,
        }) = parse(RMC)
        else {
            panic!("should parse as RMC");
        };

        assert!(valid);
        let p = position.expect("a position");
        // 4807.038 is 48 degrees 7.038 minutes, which is 48.1173 — not
        // 48.07038, which is the mistake this asserts against.
        assert!((p.lat - 48.117_30).abs() < 1e-5, "latitude {}", p.lat);
        assert!((p.lon - 11.516_666).abs() < 1e-5, "longitude {}", p.lon);
        assert_eq!(sog_kn, Some(22.4));
        assert_eq!(cog_deg, Some(84.4));
        assert_eq!(time_of_day, Some(12 * 3600 + 35 * 60 + 19));
    }

    #[test]
    fn a_bad_checksum_is_refused() {
        // One digit changed in the body, so the stated checksum no longer
        // matches. Accepting this would move the boat 60 miles.
        let corrupted = RMC.replace("4807.038", "4907.038");
        match parse(&corrupted) {
            Err(Invalid::Checksum { .. }) => {}
            other => panic!("expected a checksum failure, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_checksum_is_accepted() {
        // Plenty of devices and nearly every log file omit it.
        let (body, _) = RMC.split_once('*').expect("has a checksum");
        assert!(matches!(parse(body), Ok(Sentence::Rmc { .. })));
    }

    #[test]
    fn a_checksum_that_is_not_hex_is_refused() {
        let nonsense = RMC.replace("*6A", "*ZZ");
        assert!(matches!(parse(&nonsense), Err(Invalid::Checksum { .. })));
    }

    #[test]
    fn every_talker_reaches_the_same_arm() {
        // The same sentence from a GPS, a multi-constellation receiver and
        // an instrument bus. Matching on the whole address would take only
        // the first.
        for talker in ["GP", "GN", "GL", "II", "IN"] {
            let line = format!("${talker}RMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,,");
            assert!(
                matches!(parse(&line), Ok(Sentence::Rmc { .. })),
                "{talker} should parse"
            );
        }
    }

    #[test]
    fn a_receiver_with_no_fix_does_not_report_null_island() {
        // Status V, and the position fields blank — which is exactly what a
        // cold receiver sends. Parsing the blanks as zero would put the boat
        // in the Gulf of Guinea.
        let line = "$GPRMC,,V,,,,,,,,,,N";
        let Ok(Sentence::Rmc {
            position, valid, ..
        }) = parse(line)
        else {
            panic!("should still parse");
        };
        assert!(!valid);
        assert_eq!(position, None);

        // And applying it must not move the boat.
        let mut vessel = Vessel::default();
        vessel.apply(parse(RMC).expect("valid"), 1000);
        let before = vessel.position;
        vessel.apply(parse(line).expect("parses"), 1001);
        assert_eq!(
            vessel.position, before,
            "losing a fix must not move the boat"
        );
    }

    #[test]
    fn southern_and_western_hemispheres_are_negative() {
        let line = "$GPGLL,3340.000,S,15112.000,E,123519,A";
        let Ok(Sentence::Gll { position, valid }) = parse(line) else {
            panic!("should parse");
        };
        assert!(valid);
        let p = position.expect("a position");
        assert!((p.lat + 33.666_666).abs() < 1e-5, "{}", p.lat);
        assert!((p.lon - 151.2).abs() < 1e-5, "{}", p.lon);

        let west = "$GPGLL,3240.800,N,11713.800,W,123519,A";
        let Ok(Sentence::Gll { position, .. }) = parse(west) else {
            panic!("should parse");
        };
        let p = position.expect("a position");
        // San Diego: 32 degrees 40.8 minutes north, 117 degrees 13.8 west.
        assert!((p.lat - 32.68).abs() < 1e-6, "{}", p.lat);
        assert!((p.lon + 117.23).abs() < 1e-6, "{}", p.lon);
    }

    #[test]
    fn a_longitude_has_three_degree_digits() {
        // The one asymmetry in the format: latitude is ddmm.mmm and
        // longitude dddmm.mmm, so the split cannot be at a fixed offset.
        assert_eq!(degrees_minutes("00100.000", "E"), Some(1.0));
        assert_eq!(degrees_minutes("0100.000", "N"), Some(1.0));
        assert_eq!(
            degrees_minutes("17959.999", "E").map(|v| v > 179.99),
            Some(true)
        );
    }

    #[test]
    fn malformed_positions_are_refused_rather_than_guessed() {
        assert_eq!(degrees_minutes("", "N"), None);
        assert_eq!(
            degrees_minutes("12", "N"),
            None,
            "too short to hold minutes"
        );
        assert_eq!(degrees_minutes("4807.038", "X"), None, "no such hemisphere");
        assert_eq!(
            degrees_minutes("4860.000", "N"),
            None,
            "60 minutes is not a thing"
        );
        assert_eq!(degrees_minutes("abcd.efg", "N"), None);
    }

    #[test]
    fn depth_comes_from_whichever_unit_is_present() {
        // DBT carries feet, metres and fathoms. Metres is preferred; feet
        // is the fallback because some sounders leave the metres field out.
        let both = "$SDDBT,32.8,f,10.0,M,5.5,F";
        assert_eq!(parse(both), Ok(Sentence::Dbt { metres: Some(10.0) }));

        let feet_only = "$SDDBT,32.8,f,,M,,F";
        let Ok(Sentence::Dbt { metres }) = parse(feet_only) else {
            panic!("should parse");
        };
        // 32.8 ft is 9.9974 m on the international foot of exactly
        // 0.3048 m, which is the one NMEA means.
        assert!((metres.expect("depth") - 9.997_4).abs() < 1e-3);
    }

    #[test]
    fn wind_units_are_honoured() {
        // The same angle and number mean different speeds depending on the
        // unit field, which is per sentence.
        let knots = "$WIMWV,045.0,R,10.0,N,A";
        let Ok(Sentence::Mwv { speed_kn, .. }) = parse(knots) else {
            panic!("parses");
        };
        assert_eq!(speed_kn, Some(10.0));

        let metres = "$WIMWV,045.0,R,10.0,M,A";
        let Ok(Sentence::Mwv { speed_kn, .. }) = parse(metres) else {
            panic!("parses");
        };
        assert!((speed_kn.expect("speed") - 19.438).abs() < 1e-2);
    }

    #[test]
    fn true_wind_is_not_mistaken_for_apparent() {
        let mut vessel = Vessel::default();
        vessel.apply(parse("$WIMWV,045.0,T,10.0,N,A").expect("parses"), 100);
        assert_eq!(
            vessel.apparent_wind, None,
            "a true-wind sentence must not land in the apparent field"
        );

        vessel.apply(parse("$WIMWV,045.0,R,10.0,N,A").expect("parses"), 100);
        assert_eq!(vessel.apparent_wind.map(|s| s.value), Some((45.0, 10.0)));
    }

    #[test]
    fn magnetic_heading_is_corrected_to_true() {
        let mut vessel = Vessel::default();
        // 11 degrees east variation: true is magnetic plus the variation.
        vessel.apply(parse("$IIHDG,090.0,,,11.0,E").expect("parses"), 100);
        assert_eq!(vessel.heading_deg.map(|s| s.value), Some(101.0));

        // West is subtracted, and the result wraps.
        vessel.apply(parse("$IIHDG,005.0,,,11.0,W").expect("parses"), 101);
        assert_eq!(vessel.heading_deg.map(|s| s.value), Some(354.0));
    }

    #[test]
    fn gga_with_no_fix_is_not_believed() {
        // Quality 0 with a position in the fields is a receiver that has
        // not acquired yet, and the position is whatever it last guessed.
        let mut vessel = Vessel::default();
        vessel.apply(
            parse("$GPGGA,123519,4807.038,N,01131.000,E,0,00,,,M,,M,,").expect("parses"),
            100,
        );
        assert_eq!(vessel.position, None);
        assert_eq!(vessel.fix_quality, 0);

        vessel.apply(
            parse("$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,").expect("ok"),
            101,
        );
        assert!(vessel.position.is_some());
        assert_eq!(vessel.fix_quality, 1);
        assert_eq!(vessel.satellites, Some(8));
    }

    #[test]
    fn unknown_sentences_are_ignored_not_fatal() {
        // Every device emits proprietary sentences. Refusing the stream
        // over one would make the app useless on a real boat.
        assert_eq!(parse("$PGRMZ,93,f,3"), Err(Invalid::Unsupported));
        assert_eq!(
            parse("$GPGSV,3,1,11,03,03,111,00"),
            Err(Invalid::Unsupported)
        );
        // AIS, which is a decoder of its own and deliberately absent.
        assert_eq!(
            parse("!AIVDM,1,1,,B,177KQJ5000G?tO`K>RA1wUbN0TKH,0"),
            Err(Invalid::Unsupported)
        );
    }

    #[test]
    fn rubbish_is_not_a_sentence() {
        assert_eq!(parse(""), Err(Invalid::NotASentence));
        assert_eq!(parse("hello"), Err(Invalid::NotASentence));
        assert_eq!(parse("$"), Err(Invalid::Truncated));
        // A talker ID with no sentence type behind it. Truncated rather
        // than unsupported: there is nothing here to not support.
        assert_eq!(parse("$GP"), Err(Invalid::Truncated));
    }

    #[test]
    fn a_device_fix_is_stamped_with_when_it_was_taken() {
        // The failure this guards: a phone's cached location can be
        // minutes old, and stamping it with the moment it was read would
        // present the last anchorage as the present position — with no
        // staleness warning, because by arrival time it would be new.
        let mut vessel = Vessel::default();
        vessel.apply_fix(Fix {
            at: LatLon::new(32.68, -117.23),
            sog_kn: Some(5.2),
            cog_deg: Some(184.0),
            accuracy_m: Some(4.5),
            satellites: Some(9),
            taken: 1_000,
        });

        let fix = vessel.position.expect("a fix");
        assert_eq!(fix.at, 1_000, "stamped with the wrong time");
        // Read a hundred seconds later, it is stale — which is the whole
        // point of carrying the taken time rather than the arrival time.
        assert!(
            !vessel.has_fix(1_100, 5),
            "an old cached fix read as current"
        );
        assert!(vessel.has_fix(1_002, 5), "two seconds old is still a fix");
        assert_eq!(vessel.accuracy_m.map(|a| a.value), Some(4.5));
        assert_eq!(vessel.fix_quality, 1, "a plain fix, not an invented one");
    }

    #[test]
    fn a_device_fix_does_not_erase_what_the_instruments_said() {
        // A phone gives position, speed and course. It knows nothing about
        // the depth under the keel or the wind at the masthead, and a fix
        // arriving must not blank them.
        let mut vessel = Vessel::default();
        vessel.apply(parse("$SDDBT,32.8,f,10.0,M,5.5,F").expect("depth"), 500);
        vessel.apply_fix(Fix {
            at: LatLon::new(32.68, -117.23),
            sog_kn: None,
            cog_deg: None,
            accuracy_m: None,
            satellites: None,
            taken: 1_000,
        });
        assert!(vessel.depth_m.is_some(), "the depth was lost");
        assert!(vessel.position.is_some());
        assert!(vessel.sog_kn.is_none(), "a speed was invented");
    }

    #[test]
    fn nmea_never_claims_an_accuracy_in_metres() {
        // HDOP is a unitless multiplier, not a distance. Presenting it as
        // metres would be a confident number with no basis.
        let mut vessel = Vessel::default();
        vessel.apply(
            parse("$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,").expect("gga"),
            1_000,
        );
        assert!(vessel.position.is_some(), "the fix itself should be taken");
        assert_eq!(vessel.accuracy_m, None, "HDOP was presented as metres");
    }

    #[test]
    fn staleness_is_measured_from_arrival() {
        let mut vessel = Vessel::default();
        vessel.apply(parse(RMC).expect("valid"), 1_000);

        assert!(vessel.has_fix(1_002, 5), "two seconds old is fine");
        assert!(!vessel.has_fix(1_010, 5), "ten seconds old is not");

        // The value is still there — it has not been cleared, only aged.
        // A boat that vanishes from the chart the moment a sentence is
        // missed is worse than one whose marker is shown as stale.
        assert!(vessel.position.is_some());
    }
}
