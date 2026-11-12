//! Tide and tidal-current stations, read from the database `rutter tides`
//! builds.
//!
//! The prediction itself is in `rutter-core`, which has no dependencies and
//! so can go anywhere. This crate is only the storage half: it knows about
//! SQLite and about NOAA's spelling of things, and it hands back
//! [`rutter_core::tide::Station`] values that know nothing about either.
//!
//! The split is the same one `rutter-charts` makes against the renderer, and
//! for the same reason: the part that has to work identically on four
//! Android ABIs should depend on as little as possible.

use std::path::Path;

use rutter_core::tide::{Constituent, CurrentStation, Harmonic, Station};
use rutter_core::LatLon;

mod error;
pub use error::Error;

/// An open tide-station database.
#[derive(Debug)]
pub struct TideStations {
    db: rusqlite::Connection,
    attribution: Option<String>,
}

impl TideStations {
    /// Open a database built by `rutter tides`.
    ///
    /// Read-only: nothing in the app writes stations, and opening read-only
    /// means a database on a read-only filesystem — which is where an
    /// Android asset ends up — still works.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let db = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_URI
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;

        // Checked up front so that a chart or an imagery file handed to the
        // wrong reader says what it is, rather than returning no stations
        // and leaving the screen mysteriously bare.
        let has_tables: bool = db.query_row(
            "SELECT count(*) = 2 FROM sqlite_master
              WHERE type = 'table' AND name IN ('station', 'constituent')",
            [],
            |row| row.get(0),
        )?;
        if !has_tables {
            return Err(Error::NotATideDatabase);
        }

        let attribution = db
            .query_row(
                "SELECT value FROM metadata WHERE name = 'attribution'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok();

        Ok(Self { db, attribution })
    }

    /// The credit line the file carries, if any.
    pub fn attribution(&self) -> Option<&str> {
        self.attribution.as_deref()
    }

    /// How many stations the file holds.
    pub fn count(&self) -> Result<usize, Error> {
        Ok(self
            .db
            .query_row("SELECT count(*) FROM station", [], |row| {
                row.get::<_, i64>(0)
            })? as usize)
    }

    /// Every station, with its constituents.
    pub fn all(&self) -> Result<Vec<Station>, Error> {
        self.query("1 = 1", [])
    }

    /// Stations inside a latitude/longitude box.
    ///
    /// `nw` and `se` are the corners the chart code uses, so north-west and
    /// south-east: latitude descends and longitude ascends between them.
    pub fn in_box(&self, nw: LatLon, se: LatLon) -> Result<Vec<Station>, Error> {
        self.query(
            "lat BETWEEN ?1 AND ?2 AND lon BETWEEN ?3 AND ?4",
            rusqlite::params![se.lat, nw.lat, nw.lon, se.lon],
        )
    }

    /// The station closest to a position, by great-circle distance.
    ///
    /// Ordering is done here rather than in SQL because the distance is a
    /// spherical one and SQLite has no trigonometry worth relying on. The
    /// station count is in the dozens, so the whole list is cheap to sort.
    pub fn nearest(&self, to: LatLon) -> Result<Option<Station>, Error> {
        let mut best: Option<(f64, Station)> = None;
        for station in self.all()? {
            let distance = to.distance_to(LatLon::new(station.lat, station.lon));
            if best.as_ref().is_none_or(|(d, _)| distance < *d) {
                best = Some((distance, station));
            }
        }
        Ok(best.map(|(_, s)| s))
    }

    fn query<P: rusqlite::Params>(
        &self,
        where_clause: &str,
        params: P,
    ) -> Result<Vec<Station>, Error> {
        let sql = format!(
            "SELECT id, name, lat, lon, datum_m FROM station WHERE {where_clause} ORDER BY id"
        );
        let mut statement = self.db.prepare(&sql)?;
        let rows = statement.query_map(params, |row| {
            Ok(Station {
                id: row.get(0)?,
                name: row.get(1)?,
                lat: row.get(2)?,
                lon: row.get(3)?,
                datum_m: row.get(4)?,
                constituents: Vec::new(),
            })
        })?;

        let mut out = Vec::new();
        for station in rows {
            let mut station = station?;
            station.constituents = self.constituents(&station.id)?;
            // A station with no constituents this build understands would
            // predict a flat sea, which is worse than not offering it.
            if station.constituents.is_empty() {
                log::warn!(
                    "tide station {} has no usable constituents, skipping",
                    station.id
                );
                continue;
            }
            out.push(station);
        }
        Ok(out)
    }

    /// Every current station in the file.
    ///
    /// Empty rather than an error when the file predates the current
    /// tables: an older database is still a perfectly good tide database,
    /// and refusing to open it would cost the heights to gain nothing.
    pub fn currents(&self) -> Result<Vec<CurrentStation>, Error> {
        if !self.has_currents()? {
            log::info!("this tide database carries no current stations");
            return Ok(Vec::new());
        }

        let mut statement = self.db.prepare(
            "SELECT id, bin, name, lat, lon, flood_deg, ebb_deg, depth_m, mean_kn
               FROM current_station ORDER BY id, bin",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                CurrentStation {
                    id: row.get(0)?,
                    name: row.get(2)?,
                    lat: row.get(3)?,
                    lon: row.get(4)?,
                    flood_deg: row.get(5)?,
                    ebb_deg: row.get(6)?,
                    depth_m: row.get(7)?,
                    mean_kn: row.get(8)?,
                    constituents: Vec::new(),
                },
                row.get::<_, i64>(1)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (mut station, bin) = row?;
            station.constituents = self.current_constituents(&station.id, bin)?;
            // A station with no constituents would predict its mean set
            // and nothing else — a current that never turns, which for
            // anywhere but a river is a lie rather than a simplification.
            if station.constituents.is_empty() {
                log::warn!(
                    "current station {} bin {bin} has no usable constituents, skipping",
                    station.id
                );
                continue;
            }
            out.push(station);
        }
        Ok(out)
    }

    /// Whether this file has the current tables at all.
    fn has_currents(&self) -> Result<bool, Error> {
        Ok(self.db.query_row(
            "SELECT count(*) = 2 FROM sqlite_master
              WHERE type = 'table'
                AND name IN ('current_station', 'current_constituent')",
            [],
            |row| row.get(0),
        )?)
    }

    fn current_constituents(&self, station: &str, bin: i64) -> Result<Vec<Constituent>, Error> {
        let mut statement = self.db.prepare(
            "SELECT name, amplitude_kn, phase_deg FROM current_constituent
              WHERE station = ?1 AND bin = ?2 ORDER BY amplitude_kn DESC",
        )?;
        let rows = statement.query_map(rusqlite::params![station, bin], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (name, amplitude, phase_deg) = row?;
            match Harmonic::from_name(&name) {
                Some(harmonic) => out.push(Constituent {
                    harmonic,
                    amplitude,
                    phase_deg,
                }),
                None => {
                    log::debug!("current station {station}: unknown constituent {name}, ignored");
                }
            }
        }
        Ok(out)
    }

    fn constituents(&self, station: &str) -> Result<Vec<Constituent>, Error> {
        let mut statement = self.db.prepare(
            // The column keeps its unit in its name because here it
            // really is metres; `Constituent::amplitude` is deliberately
            // unitless because the same struct carries knots for a
            // current.
            "SELECT name, amplitude_m, phase_deg FROM constituent
              WHERE station = ?1 ORDER BY amplitude_m DESC",
        )?;
        let rows = statement.query_map([station], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (name, amplitude, phase_deg) = row?;
            // NOAA occasionally lists a constituent the table here does not
            // carry. Dropping one small term is a fraction of a centimetre;
            // refusing the whole station over it would be worse.
            match Harmonic::from_name(&name) {
                Some(harmonic) => out.push(Constituent {
                    harmonic,
                    amplitude,
                    phase_deg,
                }),
                None => log::debug!("station {station}: unknown constituent {name}, ignored"),
            }
        }
        Ok(out)
    }
}
