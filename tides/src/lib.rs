//! Tide stations, read from the database `rutter tides` builds.
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

use rutter_core::tide::{Constituent, Harmonic, Station};
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

    fn constituents(&self, station: &str) -> Result<Vec<Constituent>, Error> {
        let mut statement = self.db.prepare(
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
            let (name, amplitude_m, phase_deg) = row?;
            // NOAA occasionally lists a constituent the table here does not
            // carry. Dropping one small term is a fraction of a centimetre;
            // refusing the whole station over it would be worse.
            match Harmonic::from_name(&name) {
                Some(harmonic) => out.push(Constituent {
                    harmonic,
                    amplitude_m,
                    phase_deg,
                }),
                None => log::debug!("station {station}: unknown constituent {name}, ignored"),
            }
        }
        Ok(out)
    }
}
