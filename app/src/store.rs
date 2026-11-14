//! Keeping things between launches.
//!
//! Until now nothing survived closing the app: the draft, the layer
//! choices, the GPS address and — once routes arrived — a whole planned
//! passage. That last one is what made this urgent. You plan the night
//! before and sail in the morning, and a plotter that forgets the plan
//! overnight is not one you would plan with.
//!
//! Two files, in two formats, for two different reasons.
//!
//! ## Settings: `settings.conf`
//!
//! `key = value`, one per line, `#` for comments. Hand-rolled rather than
//! TOML or JSON, which would each mean a serialisation dependency for
//! about twenty flat values. It is also the format most likely to be
//! *fixable*: a settings file that will not load is a thing someone has to
//! repair on a boat with no internet, and a text editor is the only tool
//! they are guaranteed to have.
//!
//! Unknown keys are kept and written back. A file written by a newer build
//! and read by an older one must not silently lose the newer build's
//! settings — that is how you end up resetting your draft by downgrading.
//!
//! ## Routes: GPX
//!
//! Not a format of our own, deliberately. GPX is what every plotter,
//! every handheld and every chart program on earth reads and writes, so a
//! route saved here can be given to a friend with a Garmin, and a route
//! off a friend's Garmin can be sailed here. Inventing a format would
//! make this app an island.
//!
//! The XML is read with a small tolerant scanner rather than a parser
//! crate. GPX in practice is a narrow shape — elements with `lat` and
//! `lon` attributes and an optional `<name>` child — and what varies
//! between writers is namespace prefixes, self-closing tags and a great
//! deal of extra content to ignore. See [`read_gpx`] for exactly what is
//! handled and what is not; the limits are stated rather than hoped for.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rutter_core::route::{Route, Waypoint};
use rutter_core::track::{self, Track};
use rutter_core::LatLon;

/// Where things are kept.
#[derive(Debug, Clone)]
pub struct Store {
    directory: PathBuf,
}

/// Everything worth remembering that is not a route.
///
/// Flat on purpose: a flat list of named values is what the file format
/// can express, and keeping the struct the same shape means there is no
/// mapping layer to get wrong.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub draft_m: f64,
    pub safety_m: f64,
    pub draft_check: bool,
    pub imagery_visible: bool,
    pub imagery_opacity: f32,
    pub tide_visible: bool,
    pub currents_visible: bool,
    pub wind_visible: bool,
    pub temperature_visible: bool,
    pub temperature_opacity: f32,
    pub vessel_visible: bool,
    pub route_visible: bool,
    /// Where to read NMEA from, as typed. Empty for none.
    pub nmea: String,
    /// Chart layers hidden by name. The *hidden* ones rather than the
    /// visible ones, because a chart with new layers in it should show
    /// them: a file listing what to show would silently hide anything the
    /// chart gained since it was written.
    pub hidden_layers: Vec<String>,
    /// What lengths are shown in — see [`rutter_core::units::Length`].
    pub units: rutter_core::units::Length,
    /// Whether times are shown locally rather than in UTC.
    pub local_time: bool,
    /// Keys this build did not recognise, kept so writing the file back
    /// does not discard a newer build's settings.
    ///
    /// Public so a caller can build a `Settings` from its own state while
    /// carrying these forward — which is the only way the keeping works:
    /// a snapshot that dropped them would write them away on the first
    /// change to anything else.
    pub unknown: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            draft_m: 1.8,
            safety_m: 0.5,
            draft_check: false,
            imagery_visible: true,
            imagery_opacity: 0.75,
            tide_visible: true,
            currents_visible: true,
            wind_visible: true,
            temperature_visible: true,
            temperature_opacity: 0.30,
            vessel_visible: true,
            route_visible: true,
            nmea: String::new(),
            hidden_layers: Vec::new(),
            // Metres and UTC: what every chart, almanac and weather
            // product is published in, so the default matches the source
            // data and a change is a deliberate act.
            units: rutter_core::units::Length::Metres,
            local_time: false,
            unknown: BTreeMap::new(),
        }
    }
}

impl Store {
    /// A store in a directory, creating it if need be.
    ///
    /// `None` if the directory cannot be made, which on a sandboxed
    /// platform handed the wrong path is the normal failure. Not an error
    /// worth stopping for: an app that remembers nothing still charts.
    pub fn open(directory: impl Into<PathBuf>) -> Option<Self> {
        let directory = directory.into();
        if let Err(e) = std::fs::create_dir_all(&directory) {
            log::error!("cannot keep settings in {}: {e}", directory.display());
            return None;
        }
        log::info!("settings and routes in {}", directory.display());
        Some(Self { directory })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn settings_path(&self) -> PathBuf {
        self.directory.join("settings.conf")
    }

    fn route_path(&self) -> PathBuf {
        self.directory.join("route.gpx")
    }

    fn track_path(&self) -> PathBuf {
        self.directory.join("track.gpx")
    }

    /// Read the settings, falling back to the defaults for anything
    /// missing or unreadable.
    ///
    /// A damaged file loses the settings in it and nothing else. Refusing
    /// to start over a bad line would be the wrong trade — one mistyped
    /// number should not cost you the chart.
    pub fn settings(&self) -> Settings {
        let path = self.settings_path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Settings::default();
        };
        Settings::parse(&text)
    }

    pub fn save_settings(&self, settings: &Settings) {
        write_atomically(&self.settings_path(), &settings.to_text());
    }

    /// The saved route, if there is one.
    pub fn route(&self) -> Option<Route> {
        let text = std::fs::read_to_string(self.route_path()).ok()?;
        let route = read_gpx(&text);
        (!route.waypoints.is_empty()).then_some(route)
    }

    /// Save a route. An empty one removes the file rather than writing a
    /// route with nothing in it.
    pub fn save_route(&self, route: &Route) {
        let path = self.route_path();
        if route.waypoints.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        write_atomically(&path, &write_gpx(route));
    }
}

/// Write via a temporary file and rename.
///
/// A settings file truncated by a power cut mid-write is a settings file
/// that will not load, and the rename makes the swap atomic — either the
/// old contents or the new, never half of either. The same reason the
/// Android asset extraction does it.
fn write_atomically(path: &Path, contents: &str) {
    let partial = path.with_extension("partial");
    if let Err(e) = std::fs::write(&partial, contents) {
        log::error!("writing {}: {e}", partial.display());
        return;
    }
    if let Err(e) = std::fs::rename(&partial, path) {
        log::error!("renaming into {}: {e}", path.display());
        let _ = std::fs::remove_file(&partial);
    }
}

impl Settings {
    /// Read the `key = value` form.
    fn parse(text: &str) -> Self {
        let mut found: BTreeMap<String, String> = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                log::warn!("settings: ignoring {line:?}, which has no '='");
                continue;
            };
            found.insert(key.trim().to_owned(), value.trim().to_owned());
        }

        let mut out = Self::default();

        // Each read takes the file's value only if it parses. A mistyped
        // number falls back to the default rather than to zero — a draft
        // of zero would quietly declare the boat able to cross anything.
        let number = |found: &BTreeMap<String, String>, key: &str, fallback: f64| -> f64 {
            found
                .get(key)
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| v.is_finite())
                .unwrap_or(fallback)
        };
        let flag = |found: &BTreeMap<String, String>, key: &str, fallback: bool| -> bool {
            match found.get(key).map(String::as_str) {
                Some("true" | "yes" | "on" | "1") => true,
                Some("false" | "no" | "off" | "0") => false,
                _ => fallback,
            }
        };

        out.draft_m = number(&found, "draft_m", out.draft_m);
        out.safety_m = number(&found, "safety_m", out.safety_m);
        out.draft_check = flag(&found, "draft_check", out.draft_check);
        out.imagery_visible = flag(&found, "imagery_visible", out.imagery_visible);
        out.imagery_opacity = number(&found, "imagery_opacity", f64::from(out.imagery_opacity))
            .clamp(0.0, 1.0) as f32;
        out.tide_visible = flag(&found, "tide_visible", out.tide_visible);
        out.currents_visible = flag(&found, "currents_visible", out.currents_visible);
        out.wind_visible = flag(&found, "wind_visible", out.wind_visible);
        out.temperature_visible = flag(&found, "temperature_visible", out.temperature_visible);
        out.temperature_opacity = number(
            &found,
            "temperature_opacity",
            f64::from(out.temperature_opacity),
        )
        .clamp(0.0, 1.0) as f32;
        out.vessel_visible = flag(&found, "vessel_visible", out.vessel_visible);
        out.route_visible = flag(&found, "route_visible", out.route_visible);
        out.nmea = found.get("nmea").cloned().unwrap_or_default();
        out.units = found
            .get("units")
            .and_then(|text| rutter_core::units::Length::parse(text))
            .unwrap_or(out.units);
        out.local_time = flag(&found, "local_time", out.local_time);
        out.hidden_layers = found
            .get("hidden_layers")
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        // Whatever is left is from a newer build. Kept so writing the file
        // back does not throw away settings this one does not know about.
        let known = Self::KNOWN_KEYS;
        out.unknown = found
            .into_iter()
            .filter(|(key, _)| !known.contains(&key.as_str()))
            .collect();

        out
    }

    const KNOWN_KEYS: [&'static str; 16] = [
        "draft_m",
        "safety_m",
        "draft_check",
        "imagery_visible",
        "imagery_opacity",
        "tide_visible",
        "currents_visible",
        "wind_visible",
        "temperature_visible",
        "temperature_opacity",
        "vessel_visible",
        "route_visible",
        "nmea",
        "hidden_layers",
        "units",
        "local_time",
    ];

    fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str("# rutter settings. Edited by hand is fine.\n");
        out.push_str("# Anything unparseable falls back to the default.\n\n");

        out.push_str("# Your boat, and the water you want under it.\n");
        out.push_str(&format!("draft_m = {:.3}\n", self.draft_m));
        out.push_str(&format!("safety_m = {:.3}\n", self.safety_m));
        out.push_str(&format!("draft_check = {}\n", self.draft_check));

        out.push_str("\n# Layers.\n");
        for (key, value) in [
            ("imagery_visible", self.imagery_visible),
            ("tide_visible", self.tide_visible),
            ("currents_visible", self.currents_visible),
            ("wind_visible", self.wind_visible),
            ("temperature_visible", self.temperature_visible),
            ("vessel_visible", self.vessel_visible),
            ("route_visible", self.route_visible),
        ] {
            out.push_str(&format!("{key} = {value}\n"));
        }
        out.push_str(&format!("imagery_opacity = {:.3}\n", self.imagery_opacity));
        out.push_str(&format!(
            "temperature_opacity = {:.3}\n",
            self.temperature_opacity
        ));

        out.push_str("\n# How things are shown. Units are metres, feet or fathoms;\n");
        out.push_str("# local_time swaps UTC for whatever the system's zone is.\n");
        out.push_str(&format!("units = {}\n", self.units.key()));
        out.push_str(&format!("local_time = {}\n", self.local_time));

        out.push_str("\n# Chart layers to keep hidden, by name.\n");
        out.push_str(&format!(
            "hidden_layers = {}\n",
            self.hidden_layers.join(",")
        ));

        out.push_str("\n# Where to read NMEA 0183 from. Empty for none.\n");
        out.push_str(&format!("nmea = {}\n", self.nmea));

        if !self.unknown.is_empty() {
            out.push_str("\n# Written by a newer build of rutter, kept as found.\n");
            for (key, value) in &self.unknown {
                out.push_str(&format!("{key} = {value}\n"));
            }
        }
        out
    }
}

impl Store {
    /// The recorded track, if there is one.
    pub fn track(&self) -> Track {
        std::fs::read_to_string(self.track_path())
            .ok()
            .map(|text| read_track_gpx(&text))
            .unwrap_or_default()
    }

    /// Save a track. An empty one removes the file.
    pub fn save_track(&self, track: &Track) {
        let path = self.track_path();
        if track.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        write_atomically(&path, &write_track_gpx(track));
    }
}

/// Render a track as GPX 1.1.
///
/// `<trkseg>` per run, which is what the element is for: a reader that
/// honours it will draw the breaks rather than joining across water
/// nobody crossed.
pub fn write_track_gpx(track: &Track) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<gpx version=\"1.1\" creator=\"rutter\" \
         xmlns=\"http://www.topografix.com/GPX/1/1\">\n",
    );
    out.push_str("  <trk>\n    <name>rutter track</name>\n");
    for segment in &track.segments {
        out.push_str("    <trkseg>\n");
        for fix in segment {
            // Time as well as position: a track without times is a shape,
            // and what makes it a log is when you were where.
            out.push_str(&format!(
                "      <trkpt lat=\"{:.6}\" lon=\"{:.6}\"><time>{}</time></trkpt>\n",
                fix.at.lat,
                fix.at.lon,
                iso8601(fix.unix)
            ));
        }
        out.push_str("    </trkseg>\n");
    }
    out.push_str("  </trk>\n</gpx>\n");
    out
}

/// Read a track back, segments and all.
///
/// Uses the same tolerant scanner as the route reader, and the same
/// limits apply. A point with no usable time gets the one before it plus
/// a second rather than the epoch: a 1970 timestamp in the middle of a
/// track would make the span nonsense and every gap test fail.
pub fn read_track_gpx(text: &str) -> Track {
    let mut track = Track::default();
    let mut previous = 0i64;

    for segment_text in segments_of(text) {
        let mut segment = Vec::new();
        for (at, time) in points_with_time(&segment_text, "trkpt") {
            let unix = time.unwrap_or(previous.saturating_add(1));
            previous = unix;
            segment.push(track::Fix { at, unix });
        }
        if !segment.is_empty() {
            track.segments.push(segment);
        }
    }
    track
}

/// Each `<trkseg>`'s inner text. One pseudo-segment for a file with none,
/// so a track written without them still loads as a single run.
fn segments_of(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = find_element(rest, "trkseg") {
        let after = &rest[start..];
        let Some(open_end) = after.find('>') else {
            break;
        };
        let end = find_close(&after[open_end..], "trkseg").map_or(after.len(), |i| open_end + i);
        out.push(after[open_end..end].to_owned());
        rest = &after[end.min(after.len())..];
    }
    if out.is_empty() {
        out.push(text.to_owned());
    }
    out
}

/// Positions and times of one element name.
fn points_with_time(text: &str, tag: &str) -> Vec<(LatLon, Option<i64>)> {
    let mut out = Vec::new();
    let mut rest = text;

    while let Some(start) = find_element(rest, tag) {
        let after = &rest[start..];
        let Some(tag_end) = after.find('>') else {
            break;
        };
        let attributes = &after[..tag_end];
        let self_closing = attributes.trim_end().ends_with('/');
        let body = if self_closing {
            ""
        } else {
            let from = tag_end + 1;
            let end = find_close(&after[from..], tag).map_or(after.len(), |i| from + i);
            &after[from..end]
        };

        if let (Some(lat), Some(lon)) = (attribute(attributes, "lat"), attribute(attributes, "lon"))
        {
            let time = element_text(body, "time").and_then(|t| parse_iso8601(&t));
            out.push((LatLon::new(lat, lon), time));
        }
        rest = &after[tag_end..];
    }
    out
}

/// `YYYY-MM-DDTHH:MM:SSZ`, which is what GPX wants.
///
/// Written by hand because the alternative is a date-time dependency for
/// one format in one direction, and because the arithmetic is the civil
/// calendar's and not hard: days since the epoch, converted with the
/// standard algorithm.
fn iso8601(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let seconds = unix.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

fn parse_iso8601(text: &str) -> Option<i64> {
    // Tolerant of the forms writers actually emit: a trailing Z or not, a
    // fractional second or not. Anything else is declined rather than
    // guessed at.
    let text = text.trim();
    let (date, rest) = text.split_once('T')?;
    let time = rest.trim_end_matches('Z');
    let time = time.split('.').next().unwrap_or(time);
    // An offset other than Z is not handled; saying so beats being an
    // hour out without saying anything.
    if time.contains('+') {
        return None;
    }

    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;

    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = time_parts.next().unwrap_or("0").parse().ok()?;

    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Howard Hinnant's `days_from_civil`, which is the standard way to do
/// this without a calendar library and is exact for every year.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Its inverse.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Render a route as GPX 1.1.
pub fn write_gpx(route: &Route) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<gpx version=\"1.1\" creator=\"rutter\" \
         xmlns=\"http://www.topografix.com/GPX/1/1\">\n",
    );
    out.push_str("  <rte>\n");
    out.push_str(&format!("    <name>{}</name>\n", escape(&route.name)));
    for waypoint in &route.waypoints {
        // Six decimals is about a tenth of a metre, which is finer than
        // any GPS and finer than anyone taps.
        out.push_str(&format!(
            "    <rtept lat=\"{:.6}\" lon=\"{:.6}\">\n      <name>{}</name>\n    </rtept>\n",
            waypoint.at.lat,
            waypoint.at.lon,
            escape(&waypoint.name)
        ));
    }
    out.push_str("  </rte>\n</gpx>\n");
    out
}

/// Pull a route out of GPX.
///
/// A scanner rather than a parser, and these are its limits, stated
/// plainly because an interchange format that half works is worse than one
/// that fails loudly:
///
/// * It reads `<rtept>`, `<wpt>` and `<trkpt>` elements, with or without a
///   namespace prefix, self-closing or not. Route points first; if there
///   are none it falls back to track points and then to loose waypoints,
///   so a file containing only a recorded track still loads as something
///   you can follow.
/// * It takes `lat` and `lon` attributes in either order, single or double
///   quoted, and the first `<name>` inside the element if there is one.
/// * It ignores everything else — elevation, time, symbols, extensions —
///   and it does not understand CDATA or numeric character references.
/// * A point whose latitude or longitude will not parse is skipped, not
///   defaulted. A waypoint silently at zero, zero is in the Gulf of
///   Guinea, and a route through there is the kind of error that looks
///   like a bug in the renderer.
pub fn read_gpx(text: &str) -> Route {
    let mut route = Route::new(route_name(text).unwrap_or_else(|| "Route".to_owned()));

    for tag in ["rtept", "trkpt", "wpt"] {
        let points = points_named(text, tag);
        if !points.is_empty() {
            route.waypoints = points;
            break;
        }
    }
    route
}

/// The `<name>` directly inside the first `<rte>`, if any.
fn route_name(text: &str) -> Option<String> {
    let start = find_element(text, "rte")?;
    let inside = &text[start..];
    // Only before the first point, so a waypoint's name is not mistaken
    // for the route's.
    let limit = find_element(inside, "rtept").unwrap_or(inside.len());
    element_text(&inside[..limit], "name")
}

/// Every point of one element name.
fn points_named(text: &str, tag: &str) -> Vec<Waypoint> {
    let mut out = Vec::new();
    let mut rest = text;
    let mut number = 0;

    while let Some(start) = find_element(rest, tag) {
        let after = &rest[start..];
        // The element's own tag ends at the first '>'; attributes live
        // inside it and the name, if any, after it.
        let Some(tag_end) = after.find('>') else {
            break;
        };
        let attributes = &after[..tag_end];

        // The element's own closing tag, not merely the next `</`. The
        // first version took the next one, which is `</name>` — so it cut
        // the body off before the very tag it then went looking for, and
        // every waypoint came back numbered instead of named.
        let self_closing = attributes.trim_end().ends_with('/');
        let body = if self_closing {
            ""
        } else {
            let from = tag_end + 1;
            let end = find_close(&after[from..], tag).map_or(after.len(), |i| from + i);
            &after[from..end]
        };

        number += 1;
        match (attribute(attributes, "lat"), attribute(attributes, "lon")) {
            (Some(lat), Some(lon)) => {
                let name = element_text(body, "name")
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| format!("{number:03}"));
                out.push(Waypoint::new(LatLon::new(lat, lon), name));
            }
            // Skipped rather than defaulted: a point at zero, zero sits in
            // the Gulf of Guinea and a route through it looks like a
            // rendering fault rather than a bad file.
            _ => log::warn!("gpx: a <{tag}> without usable lat/lon, skipped"),
        }

        rest = &after[tag_end..];
    }
    out
}

/// Where the closing tag for this name is, ignoring any namespace prefix.
fn find_close(text: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(offset) = text[from..].find("</") {
        let at = from + offset;
        let after = &text[at + 2..];
        let end = after
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(after.len());
        let local = after[..end].rsplit(':').next().unwrap_or("");
        if local.eq_ignore_ascii_case(name) {
            return Some(at);
        }
        from = at + 2;
    }
    None
}

/// Where an element with this name starts, ignoring any namespace prefix.
fn find_element(text: &str, name: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(offset) = text[from..].find('<') {
        let open = from + offset;
        let after = &text[open + 1..];

        // Skip the closing tag, the declaration and comments.
        if after.starts_with('/') || after.starts_with('?') || after.starts_with('!') {
            from = open + 1;
            continue;
        }

        // The element's own name, up to whitespace, '/' or '>'.
        let end = after
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(after.len());
        let element = &after[..end];
        // Namespace prefixes are commonplace in real files: `gpx:rtept`.
        let local = element.rsplit(':').next().unwrap_or(element);

        if local.eq_ignore_ascii_case(name) {
            return Some(open);
        }
        from = open + 1;
        if from >= bytes.len() {
            break;
        }
    }
    None
}

/// One attribute's value as a number, in either quote style.
fn attribute(tag: &str, name: &str) -> Option<f64> {
    let mut from = 0;
    while let Some(offset) = tag[from..].find(name) {
        let at = from + offset;
        // Must be a whole attribute name: `lat` and not the tail of
        // something else.
        let before_ok = at == 0
            || tag[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace() || c == '<');
        let rest = &tag[at + name.len()..];
        let rest_trimmed = rest.trim_start();

        if before_ok && rest_trimmed.starts_with('=') {
            let value = rest_trimmed[1..].trim_start();
            let quote = value.chars().next()?;
            if quote == '"' || quote == '\'' {
                let end = value[1..].find(quote)? + 1;
                return value[1..end].trim().parse().ok();
            }
        }
        from = at + name.len();
    }
    None
}

/// The text inside the first element of this name.
fn element_text(text: &str, name: &str) -> Option<String> {
    let start = find_element(text, name)?;
    let after = &text[start..];
    let open_end = after.find('>')?;
    // A self-closing element has no text.
    if after[..open_end].ends_with('/') {
        return None;
    }
    let close = after[open_end..].find("</")?;
    Some(unescape(after[open_end + 1..open_end + close].trim()))
}

/// The five entities XML requires. Nothing fancier: a numeric reference in
/// a waypoint name is rare enough to leave as written rather than guess
/// at, and leaving it visible is better than mangling it.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn unescape(text: &str) -> String {
    // Ampersand last, or `&amp;lt;` would come out as `<`.
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passage() -> Route {
        Route {
            name: "Catalina".to_owned(),
            waypoints: vec![
                Waypoint::new(LatLon::new(32.7100, -117.2300), "Shelter Island"),
                Waypoint::new(LatLon::new(32.6700, -117.2350), "Bay entrance"),
                Waypoint::new(LatLon::new(33.3450, -118.3250), "Avalon"),
            ],
        }
    }

    #[test]
    fn a_route_survives_the_round_trip() {
        let before = passage();
        let after = read_gpx(&write_gpx(&before));

        assert_eq!(after.name, before.name);
        assert_eq!(after.waypoints.len(), before.waypoints.len());
        for (a, b) in after.waypoints.iter().zip(&before.waypoints) {
            assert_eq!(a.name, b.name);
            // Six decimals is a tenth of a metre.
            assert!(
                (a.at.lat - b.at.lat).abs() < 1e-6,
                "{:?} vs {:?}",
                a.at,
                b.at
            );
            assert!(
                (a.at.lon - b.at.lon).abs() < 1e-6,
                "{:?} vs {:?}",
                a.at,
                b.at
            );
        }
    }

    #[test]
    fn the_output_is_gpx_a_plotter_would_recognise() {
        let text = write_gpx(&passage());
        assert!(text.starts_with("<?xml"), "no declaration");
        assert!(
            text.contains("http://www.topografix.com/GPX/1/1"),
            "no GPX namespace, which some readers insist on"
        );
        assert!(text.contains("version=\"1.1\""));
        assert!(text.contains("<rte>") && text.contains("</rte>"));
        assert_eq!(text.matches("<rtept").count(), 3);
        // Negative longitudes have to survive as negative. A sign lost
        // here puts San Diego in China.
        assert!(text.contains("lon=\"-117.230000\""), "{text}");
    }

    #[test]
    fn a_namespaced_self_closing_file_from_another_plotter_loads() {
        // What real files look like: a prefix on every element, points
        // that close themselves, attributes in the other order, and a
        // great deal to ignore.
        let text = r#"<?xml version="1.0"?>
<gpx:gpx xmlns:gpx="http://www.topografix.com/GPX/1/1" version="1.1" creator="Some Plotter">
  <gpx:metadata><gpx:name>not the route name</gpx:name></gpx:metadata>
  <gpx:rte>
    <gpx:name>Across the channel</gpx:name>
    <gpx:extensions><thing>ignored</thing></gpx:extensions>
    <gpx:rtept lon='-117.2300' lat='32.7100'><gpx:name>START</gpx:name></gpx:rtept>
    <gpx:rtept lat="32.6700" lon="-117.2350"/>
    <gpx:rtept lat="33.3450" lon="-118.3250">
      <gpx:ele>0.0</gpx:ele>
      <gpx:name>AVALON</gpx:name>
      <gpx:sym>Anchor</gpx:sym>
    </gpx:rtept>
  </gpx:rte>
</gpx:gpx>"#;

        let route = read_gpx(text);
        assert_eq!(route.waypoints.len(), 3, "{:?}", route.waypoints);
        assert_eq!(route.name, "Across the channel");
        assert_eq!(route.waypoints[0].name, "START");
        // The self-closing one has no name, so it gets its number.
        assert_eq!(route.waypoints[1].name, "002");
        assert_eq!(route.waypoints[2].name, "AVALON");

        assert!((route.waypoints[0].at.lat - 32.71).abs() < 1e-6);
        assert!((route.waypoints[0].at.lon + 117.23).abs() < 1e-6);
        assert!((route.waypoints[2].at.lat - 33.345).abs() < 1e-6);
    }

    #[test]
    fn a_recorded_track_loads_as_something_followable() {
        // A file with no route in it at all, which is what you get when
        // someone sends you where they went rather than where they meant
        // to go.
        let text = r#"<gpx version="1.1"><trk><name>Yesterday</name><trkseg>
            <trkpt lat="32.71" lon="-117.23"/>
            <trkpt lat="32.67" lon="-117.235"/>
        </trkseg></trk></gpx>"#;
        let route = read_gpx(text);
        assert_eq!(route.waypoints.len(), 2);
    }

    #[test]
    fn loose_waypoints_load_when_there_is_nothing_better() {
        let text = r#"<gpx version="1.1">
            <wpt lat="32.71" lon="-117.23"><name>A buoy</name></wpt>
            <wpt lat="32.67" lon="-117.235"><name>Another</name></wpt>
        </gpx>"#;
        let route = read_gpx(text);
        assert_eq!(route.waypoints.len(), 2);
        assert_eq!(route.waypoints[0].name, "A buoy");
    }

    #[test]
    fn route_points_win_over_track_points() {
        // A file with both: the route is what was intended and the track
        // is what happened, and the intention is what you want to follow.
        let text = r#"<gpx version="1.1">
            <rte><rtept lat="1.0" lon="2.0"/></rte>
            <trk><trkseg>
              <trkpt lat="3.0" lon="4.0"/><trkpt lat="5.0" lon="6.0"/>
            </trkseg></trk>
        </gpx>"#;
        let route = read_gpx(text);
        assert_eq!(route.waypoints.len(), 1, "the track was preferred");
        assert!((route.waypoints[0].at.lat - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_point_with_no_usable_position_is_dropped_not_defaulted() {
        // Zero, zero is in the Gulf of Guinea. A route through there is
        // the kind of error that looks like a rendering fault.
        let text = r#"<gpx version="1.1"><rte>
            <rtept lat="32.71" lon="-117.23"/>
            <rtept lat="not a number" lon="-117.0"/>
            <rtept lon="-117.0"/>
            <rtept lat="32.60" lon="-117.30"/>
        </rte></gpx>"#;
        let route = read_gpx(text);
        assert_eq!(route.waypoints.len(), 2, "{:?}", route.waypoints);
        for waypoint in &route.waypoints {
            assert!(waypoint.at.lat > 30.0, "{:?} slipped through", waypoint.at);
        }
    }

    #[test]
    fn nothing_useful_in_the_file_is_an_empty_route_not_a_panic() {
        for text in [
            "",
            "not xml at all",
            "<gpx></gpx>",
            "<gpx><rte></rte></gpx>",
            "<gpx><rte><rtept",
            "<<<>>>",
            "<gpx><rtept lat=\"\" lon=\"\"/></gpx>",
        ] {
            let route = read_gpx(text);
            assert!(
                route.waypoints.is_empty(),
                "{text:?} produced {:?}",
                route.waypoints
            );
        }
    }

    #[test]
    fn names_with_xml_characters_survive() {
        let route = Route {
            name: "Bob & Alice's <trip>".to_owned(),
            waypoints: vec![Waypoint::new(
                LatLon::new(32.0, -117.0),
                "\"Quoted\" & <odd>",
            )],
        };
        let text = write_gpx(&route);
        // Written escaped, so the file is valid XML.
        assert!(
            !text.contains("& Alice"),
            "a bare ampersand got out: {text}"
        );
        assert!(text.contains("&amp;"), "{text}");

        let back = read_gpx(&text);
        assert_eq!(back.name, route.name);
        assert_eq!(back.waypoints[0].name, route.waypoints[0].name);
    }

    #[test]
    fn settings_round_trip() {
        let mut before = Settings {
            draft_m: 2.1,
            safety_m: 0.3,
            draft_check: true,
            imagery_visible: false,
            imagery_opacity: 0.4,
            temperature_opacity: 0.6,
            nmea: "tcp://192.168.1.50:10110".to_owned(),
            hidden_layers: vec!["DEPARE".to_owned(), "SOUNDG".to_owned()],
            ..Settings::default()
        };
        before.wind_visible = false;

        let after = Settings::parse(&before.to_text());
        assert_eq!(after, before);
    }

    #[test]
    fn a_damaged_settings_file_falls_back_rather_than_to_zero() {
        // A draft of zero would quietly declare the boat able to cross
        // anything, which is the worst possible way for a bad line to
        // fail.
        let text = "draft_m = banana\nsafety_m =\ndraft_check = perhaps\nnonsense\n";
        let settings = Settings::parse(text);
        let defaults = Settings::default();
        assert_eq!(settings.draft_m, defaults.draft_m);
        assert_eq!(settings.safety_m, defaults.safety_m);
        assert_eq!(settings.draft_check, defaults.draft_check);
    }

    #[test]
    fn an_infinite_draft_is_not_accepted() {
        let settings = Settings::parse("draft_m = inf\nsafety_m = NaN\n");
        assert!(settings.draft_m.is_finite());
        assert!(settings.safety_m.is_finite());
    }

    #[test]
    fn opacities_are_clamped_to_something_drawable() {
        let settings = Settings::parse("imagery_opacity = 7\ntemperature_opacity = -3\n");
        assert_eq!(settings.imagery_opacity, 1.0);
        assert_eq!(settings.temperature_opacity, 0.0);
    }

    #[test]
    fn a_newer_builds_settings_are_not_thrown_away() {
        // Downgrading must not reset what the newer build was keeping.
        // Otherwise running an older binary once costs you settings you
        // never touched.
        // `units` was the example here once and has since become a key
        // this build knows, which would have made the test pass without
        // testing anything. Replaced with keys that are still unknown.
        let text = "draft_m = 2.0\nnight_mode = true\nmagnetic_bearings = true\n";
        let settings = Settings::parse(text);
        let written = settings.to_text();
        assert!(written.contains("night_mode = true"), "{written}");
        assert!(written.contains("magnetic_bearings = true"), "{written}");
        // And it round-trips again rather than accumulating duplicates.
        let again = Settings::parse(&written).to_text();
        assert_eq!(again.matches("night_mode").count(), 1, "{again}");
    }

    #[test]
    fn how_things_are_shown_survives_a_restart() {
        // The whole point of putting these in the settings file: a boat
        // set to feet should not come up in metres tomorrow, because a
        // depth in the wrong unit is not a cosmetic problem.
        use rutter_core::units::Length;

        for units in Length::ALL {
            for local_time in [false, true] {
                let settings = Settings {
                    units,
                    local_time,
                    ..Settings::default()
                };
                let again = Settings::parse(&settings.to_text());
                assert_eq!(again.units, units, "{units:?}");
                assert_eq!(again.local_time, local_time, "{units:?} {local_time}");
            }
        }
    }

    #[test]
    fn an_unreadable_unit_falls_back_rather_than_guessing() {
        // Hand-edited files are expected — the header says so. A unit
        // nobody can parse must leave the default standing, not pick one.
        let settings = Settings::parse("units = furlongs\nlocal_time = perhaps\n");
        assert_eq!(settings.units, Settings::default().units);
        assert_eq!(settings.local_time, Settings::default().local_time);
    }

    #[test]
    fn flags_accept_the_spellings_a_person_would_type() {
        for text in ["draft_check = yes", "draft_check = on", "draft_check = 1"] {
            assert!(Settings::parse(text).draft_check, "{text}");
        }
        for text in ["draft_check = no", "draft_check = off", "draft_check = 0"] {
            assert!(!Settings::parse(text).draft_check, "{text}");
        }
    }

    #[test]
    fn hidden_layers_are_stored_rather_than_visible_ones() {
        // The direction matters. A file listing what to *show* would
        // silently hide every layer a chart gained since it was written —
        // so a new chart would come up half blank with no explanation.
        let settings = Settings::parse("hidden_layers = DEPARE, SOUNDG ,\n");
        assert_eq!(settings.hidden_layers, vec!["DEPARE", "SOUNDG"]);

        let empty = Settings::parse("hidden_layers =\n");
        assert!(empty.hidden_layers.is_empty());
    }

    // ── Tracks ─────────────────────────────────────────────────────────

    fn a_track() -> Track {
        let mut track = Track::default();
        let rules = track::Thinning::default();
        // Two runs with a gap between them, so the segments are tested
        // and not just the points.
        for second in 0..40 {
            track.record(
                track::Fix {
                    at: LatLon::new(32.70 + 0.0005 * f64::from(second), -117.23),
                    unix: 1_772_323_200 + i64::from(second),
                },
                &rules,
            );
        }
        for second in 600..640 {
            track.record(
                track::Fix {
                    at: LatLon::new(32.60, -117.30 + 0.0005 * f64::from(second - 600)),
                    unix: 1_772_323_200 + i64::from(second),
                },
                &rules,
            );
        }
        track.finish();
        track
    }

    #[test]
    fn a_track_survives_the_round_trip_with_its_breaks() {
        let before = a_track();
        assert_eq!(before.segments.len(), 2, "the fixture needs two runs");

        let after = read_track_gpx(&write_track_gpx(&before));
        assert_eq!(
            after.segments.len(),
            before.segments.len(),
            "the break between runs was lost, which would draw the boat \
             crossing water it never crossed"
        );
        assert_eq!(after.len(), before.len());

        for (a, b) in after
            .segments
            .iter()
            .flatten()
            .zip(before.segments.iter().flatten())
        {
            assert!((a.at.lat - b.at.lat).abs() < 1e-6);
            assert!((a.at.lon - b.at.lon).abs() < 1e-6);
            // The times are what make it a log rather than a shape.
            assert_eq!(a.unix, b.unix, "a timestamp changed");
        }
    }

    #[test]
    fn the_track_output_uses_trkseg_so_other_readers_see_the_breaks() {
        let text = write_track_gpx(&a_track());
        assert_eq!(text.matches("<trkseg>").count(), 2, "{text}");
        assert!(text.contains("<trkpt"), "{text}");
        assert!(text.contains("<time>"), "no times, so it is not a log");
        assert!(text.contains("http://www.topografix.com/GPX/1/1"));
    }

    #[test]
    fn a_time_round_trips_through_iso8601() {
        // Hand-written calendar arithmetic, so it is worth checking
        // against dates a leap-year mistake would get wrong.
        for (unix, expected) in [
            (0_i64, "1970-01-01T00:00:00Z"),
            (1_772_323_200, "2026-03-01T00:00:00Z"),
            // 29 February 2024, which a naive implementation misses.
            (1_709_164_800, "2024-02-29T00:00:00Z"),
            // 1 March 1900, which was *not* a leap year — the case a
            // naive "every fourth year" rule gets wrong, and the one my
            // own expected value here was a week out on before being
            // checked against an independent calculation.
            (-2_203_891_200, "1900-03-01T00:00:00Z"),
            (1_767_225_599, "2025-12-31T23:59:59Z"),
        ] {
            assert_eq!(iso8601(unix), expected, "formatting {unix}");
            assert_eq!(parse_iso8601(expected), Some(unix), "parsing {expected}");
        }
    }

    #[test]
    fn the_time_forms_other_writers_emit_are_accepted() {
        let wanted = 1_772_323_200;
        for text in [
            "2026-03-01T00:00:00Z",
            "2026-03-01T00:00:00",
            "2026-03-01T00:00:00.000Z",
            " 2026-03-01T00:00:00Z ",
        ] {
            assert_eq!(parse_iso8601(text), Some(wanted), "{text:?}");
        }
        // And what is not handled is declined rather than guessed at —
        // being an hour out without saying so is worse than refusing.
        for text in ["2026-03-01T00:00:00+01:00", "not a time", "", "2026-03-01"] {
            assert_eq!(parse_iso8601(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_track_with_no_times_still_loads_in_order() {
        // Some writers omit them. The points are still a shape worth
        // having, and the times must at least not go backwards — a 1970
        // stamp in the middle would make every gap test fire.
        let text = r#"<gpx version="1.1"><trk><trkseg>
            <trkpt lat="32.70" lon="-117.23"/>
            <trkpt lat="32.71" lon="-117.23"/>
            <trkpt lat="32.72" lon="-117.23"/>
        </trkseg></trk></gpx>"#;
        let track = read_track_gpx(text);
        assert_eq!(track.len(), 3);
        let times: Vec<i64> = track.segments.iter().flatten().map(|f| f.unix).collect();
        for pair in times.windows(2) {
            assert!(pair[0] < pair[1], "times went backwards: {times:?}");
        }
    }

    #[test]
    fn a_track_without_segments_loads_as_one_run() {
        let text = r#"<gpx version="1.1"><trk>
            <trkpt lat="32.70" lon="-117.23"/><trkpt lat="32.71" lon="-117.23"/>
        </trk></gpx>"#;
        let track = read_track_gpx(text);
        assert_eq!(track.segments.len(), 1);
        assert_eq!(track.len(), 2);
    }

    #[test]
    fn nothing_in_the_file_is_an_empty_track_not_a_panic() {
        for text in [
            "",
            "not xml",
            "<gpx/>",
            "<gpx><trk><trkseg></trkseg></trk></gpx>",
        ] {
            assert!(read_track_gpx(text).is_empty(), "{text:?}");
        }
    }

    #[test]
    fn a_store_writes_and_reads_what_it_was_given() {
        let directory = std::env::temp_dir().join(format!("rutter-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let store = Store::open(&directory).expect("a store");

        // Nothing saved yet.
        assert_eq!(store.settings(), Settings::default());
        assert!(store.route().is_none());

        let settings = Settings {
            draft_m: 2.4,
            ..Settings::default()
        };
        store.save_settings(&settings);
        assert_eq!(store.settings(), settings);

        store.save_route(&passage());
        let read_back = store.route().expect("a route");
        assert_eq!(read_back.waypoints.len(), 3);
        assert_eq!(read_back.name, "Catalina");

        // An empty route removes the file rather than leaving an empty
        // one to be loaded as a route with nothing in it.
        store.save_route(&Route::default());
        assert!(store.route().is_none());

        // And the track, which is a separate file so a route and a
        // track cannot clobber one another.
        store.save_track(&a_track());
        let track = store.track();
        assert_eq!(track.segments.len(), 2);
        store.save_track(&Track::default());
        assert!(store.track().is_empty());

        // No partial files left behind.
        let strays: Vec<_> = std::fs::read_dir(&directory)
            .expect("read the directory")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("partial"))
            .collect();
        assert!(strays.is_empty(), "left {strays:?} behind");

        let _ = std::fs::remove_dir_all(&directory);
    }
}
