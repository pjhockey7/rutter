//! Reading MBTiles files.
//!
//! MBTiles is a SQLite database with an agreed set of tables. It is the
//! format `rutter charts` produces and the format essentially every offline
//! map tool speaks, which is why it is the one this app reads.
//!
//! Three details are the whole reason this module is more than four lines of
//! SQL.
//!
//! **`tiles` may be a view, not a table.** The specification describes a
//! `tiles` table, but tippecanoe — and MapBox's own tooling — instead write
//! `map` and `images` tables with a `tiles` view joining them, so that
//! identical tiles are stored once. Querying `tiles` works either way;
//! querying `images` directly does not.
//!
//! **Rows are numbered from the south.** MBTiles inherited TMS numbering,
//! where row 0 is the bottom of the world. Slippy-map tile y, which is what
//! [`TileId`] holds and what every tile URL uses, counts from the top. The
//! conversion is `row = 2^zoom - 1 - y`, and getting it wrong gives a chart
//! that is plausibly wrong — correct tiles in the wrong places — rather than
//! obviously wrong.
//!
//! **Vector tiles are gzipped.** The `format` metadata says `pbf`, and the
//! blobs are gzip streams. Nothing in the format states this; it is
//! convention, so the gzip magic is sniffed rather than assumed.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use rutter_core::chart::TileId;

use crate::mvt;

/// An open MBTiles file.
#[derive(Debug)]
pub struct MbTiles {
    db: Connection,
    meta: Metadata,
}

/// What the file says about itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metadata {
    pub name: String,
    /// `pbf` for vector tiles, `png`/`jpg`/`webp` for raster.
    pub format: String,
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// West, south, east, north in degrees, if declared.
    pub bounds: Option<[f64; 4]>,
}

impl Metadata {
    /// Whether this file holds vector tiles, which is all the renderer
    /// currently draws.
    pub fn is_vector(&self) -> bool {
        self.format.eq_ignore_ascii_case("pbf")
            || self.format.eq_ignore_ascii_case("mvt")
            || self.format.eq_ignore_ascii_case("pbf.gz")
    }
}

/// What can go wrong opening or reading a chart file.
#[derive(Debug)]
pub enum Error {
    /// The file could not be opened, or a query failed.
    Database(rusqlite::Error),
    /// The file opened but has no `metadata` or no `tiles` — so it is a
    /// SQLite database that is not an MBTiles file.
    NotMbTiles(&'static str),
    /// A tile blob would not decompress.
    Decompress(std::io::Error),
    /// A tile blob decompressed but would not decode.
    Mvt(mvt::Error),
    /// Raster tiles, which this reader understands the container of but
    /// cannot yet draw.
    NotVector(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(e) => write!(f, "{e}"),
            Self::NotMbTiles(what) => {
                write!(f, "not an MBTiles file: no {what}")
            }
            Self::Decompress(e) => write!(f, "tile will not decompress: {e}"),
            Self::Mvt(e) => write!(f, "tile will not decode: {e}"),
            Self::NotVector(format) => write!(
                f,
                "tile format '{format}' is raster; only vector tiles can be drawn yet"
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e)
    }
}

impl MbTiles {
    /// Open a chart file read-only.
    ///
    /// Read-only matters for more than tidiness: charts live on shared
    /// storage on Android, often on a card that may be mounted read-only,
    /// and SQLite will happily fail to create its write-ahead log on a path
    /// it cannot write to.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let db = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;

        // Confirm the shape before trusting it, so a wrong file gives a
        // clear error rather than a confusing empty chart.
        if !Self::has_object(&db, "metadata")? {
            return Err(Error::NotMbTiles("metadata table"));
        }
        if !Self::has_object(&db, "tiles")? {
            return Err(Error::NotMbTiles("tiles table or view"));
        }

        let meta = Self::read_metadata(&db)?;
        Ok(Self { db, meta })
    }

    /// Whether a table *or view* of this name exists. Both, because `tiles`
    /// is legitimately either.
    fn has_object(db: &Connection, name: &str) -> Result<bool, rusqlite::Error> {
        let count: i64 = db.query_row(
            "SELECT count(*) FROM sqlite_master \
             WHERE type IN ('table','view') AND name = ?1",
            [name],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    fn read_metadata(db: &Connection) -> Result<Metadata, Error> {
        let mut meta = Metadata::default();

        let mut stmt = db.prepare("SELECT name, value FROM metadata")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        // Zoom bounds are filled in from the tiles themselves below if the
        // metadata omits them, so start from a range that cannot match.
        let mut min_zoom = None;
        let mut max_zoom = None;

        for row in rows {
            let (key, value) = row?;
            match key.as_str() {
                "name" => meta.name = value,
                "format" => meta.format = value,
                "minzoom" => min_zoom = value.parse().ok(),
                "maxzoom" => max_zoom = value.parse().ok(),
                "bounds" => meta.bounds = parse_bounds(&value),
                _ => {}
            }
        }

        // Some writers omit the zoom range. Ask the tiles instead of
        // guessing, because a wrong range means tiles that exist are never
        // requested.
        if min_zoom.is_none() || max_zoom.is_none() {
            let found: (Option<i64>, Option<i64>) = db.query_row(
                "SELECT min(zoom_level), max(zoom_level) FROM tiles",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            min_zoom = min_zoom.or_else(|| found.0.map(|z| z as u8));
            max_zoom = max_zoom.or_else(|| found.1.map(|z| z as u8));
        }

        meta.min_zoom = min_zoom.unwrap_or(0);
        meta.max_zoom = max_zoom.unwrap_or(0);

        if meta.format.is_empty() {
            // The default in the spec.
            meta.format = "png".to_owned();
        }

        Ok(meta)
    }

    pub fn metadata(&self) -> &Metadata {
        &self.meta
    }

    /// Clamp a wanted zoom into the range this file actually holds.
    ///
    /// Asking for zoom 18 from a file that stops at 14 should draw the
    /// zoom-14 tiles stretched, not draw nothing.
    pub fn clamp_zoom(&self, zoom: u8) -> u8 {
        zoom.clamp(self.meta.min_zoom, self.meta.max_zoom)
    }

    /// Fetch one tile's raw stored bytes, still compressed.
    ///
    /// `None` means there is no such tile, which is ordinary: a chart covers
    /// a harbour, not the planet, and most of the grid is empty.
    pub fn raw_tile(&self, id: TileId) -> Result<Option<Vec<u8>>, Error> {
        let row = tms_row(id);

        let mut stmt = self.db.prepare_cached(
            "SELECT tile_data FROM tiles \
             WHERE zoom_level = ?1 AND tile_column = ?2 AND tile_row = ?3",
        )?;

        let mut rows = stmt.query(rusqlite::params![id.z, id.x, row])?;
        match rows.next()? {
            Some(r) => Ok(Some(r.get(0)?)),
            None => Ok(None),
        }
    }

    /// Fetch and decode one vector tile.
    pub fn tile(&self, id: TileId) -> Result<Option<mvt::Tile>, Error> {
        if !self.meta.is_vector() {
            return Err(Error::NotVector(self.meta.format.clone()));
        }

        let Some(raw) = self.raw_tile(id)? else {
            return Ok(None);
        };

        let bytes = maybe_gunzip(&raw).map_err(Error::Decompress)?;
        mvt::Tile::decode(&bytes).map(Some).map_err(Error::Mvt)
    }

    /// Every tile address present at a zoom level.
    ///
    /// Useful for `rutter doctor`-style reporting and for tests; a renderer
    /// should ask for the tiles its viewport covers instead.
    pub fn tiles_at(&self, zoom: u8) -> Result<Vec<TileId>, Error> {
        let mut stmt = self
            .db
            .prepare("SELECT tile_column, tile_row FROM tiles WHERE zoom_level = ?1")?;

        let rows = stmt.query_map([zoom], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (column, tms) = row?;
            out.push(from_tms(zoom, column as u32, tms as u32));
        }
        Ok(out)
    }

    /// How many tiles the file holds, across all zooms.
    pub fn tile_count(&self) -> Result<u64, Error> {
        let n: i64 = self
            .db
            .query_row("SELECT count(*) FROM tiles", [], |r| r.get(0))?;
        Ok(n as u64)
    }
}

/// Slippy y to TMS row.
fn tms_row(id: TileId) -> u32 {
    TileId::grid_size(id.z)
        .saturating_sub(1)
        .saturating_sub(id.y)
}

/// TMS row back to a slippy-map address.
fn from_tms(zoom: u8, column: u32, row: u32) -> TileId {
    TileId {
        z: zoom,
        x: column,
        y: TileId::grid_size(zoom)
            .saturating_sub(1)
            .saturating_sub(row),
    }
}

/// Gunzip if it is gzipped, pass through if it is not.
///
/// Sniffed rather than assumed: the MBTiles spec does not say vector tiles
/// are compressed, writers disagree, and a chart that fails to open because
/// someone wrote uncompressed protobuf is an avoidable failure.
fn maybe_gunzip(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

    if data.len() < 2 || data[..2] != GZIP_MAGIC {
        return Ok(data.to_vec());
    }

    use std::io::Read;
    let mut out = Vec::with_capacity(data.len() * 4);
    flate2::read::GzDecoder::new(data).read_to_end(&mut out)?;
    Ok(out)
}

/// Parse the `bounds` metadata: `west,south,east,north`.
fn parse_bounds(value: &str) -> Option<[f64; 4]> {
    let mut it = value.split(',').map(|s| s.trim().parse::<f64>());
    let mut out = [0.0; 4];
    for slot in &mut out {
        *slot = it.next()?.ok()?;
    }
    // A fifth component means this is not a bounds string.
    if it.next().is_some() {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tms_conversion_flips_the_y_axis() {
        // At zoom 10 the grid is 1024 rows, so y=347 is stored as row 676.
        // This is the exact pair tippecanoe produced for the test fixture.
        assert_eq!(
            tms_row(TileId {
                z: 10,
                x: 497,
                y: 347
            }),
            676
        );
        assert_eq!(
            from_tms(10, 497, 676),
            TileId {
                z: 10,
                x: 497,
                y: 347
            }
        );
    }

    #[test]
    fn tms_conversion_is_its_own_inverse() {
        for z in [0u8, 1, 5, 14, 20] {
            let n = TileId::grid_size(z);
            for y in [0, n / 3, n.saturating_sub(1)] {
                let id = TileId {
                    z,
                    x: 7 % n.max(1),
                    y,
                };
                let round = from_tms(z, id.x, tms_row(id));
                assert_eq!(round, id, "zoom {z} y {y}");
            }
        }
    }

    #[test]
    fn zoom_zero_has_one_row() {
        assert_eq!(tms_row(TileId { z: 0, x: 0, y: 0 }), 0);
    }

    #[test]
    fn bounds_parse() {
        assert_eq!(
            parse_bounds("-5.100000,50.050000,-4.950000,50.140000"),
            Some([-5.1, 50.05, -4.95, 50.14])
        );
        assert_eq!(
            parse_bounds(" -1 , 2 , 3 , 4 "),
            Some([-1.0, 2.0, 3.0, 4.0])
        );
        assert_eq!(parse_bounds("1,2,3"), None, "too few components");
        assert_eq!(parse_bounds("1,2,3,4,5"), None, "too many components");
        assert_eq!(parse_bounds("not,a,bounds,string"), None);
        assert_eq!(parse_bounds(""), None);
    }

    #[test]
    fn uncompressed_data_passes_through() {
        let plain = b"not gzip at all".to_vec();
        assert_eq!(maybe_gunzip(&plain).expect("passthrough"), plain);
        // Too short to hold the magic.
        assert_eq!(maybe_gunzip(&[0x1f]).expect("passthrough"), vec![0x1f]);
        assert_eq!(maybe_gunzip(&[]).expect("passthrough"), Vec::<u8>::new());
    }

    #[test]
    fn gzipped_data_is_inflated() {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(b"a coastline").expect("write");
        let gz = enc.finish().expect("finish");

        assert_eq!(&gz[..2], &[0x1f, 0x8b], "fixture should be gzip");
        assert_eq!(maybe_gunzip(&gz).expect("inflate"), b"a coastline");
    }

    #[test]
    fn truncated_gzip_is_an_error() {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(b"a coastline that gets cut off")
            .expect("write");
        let gz = enc.finish().expect("finish");

        assert!(maybe_gunzip(&gz[..gz.len() / 2]).is_err());
    }

    #[test]
    fn vector_formats_are_recognised() {
        for format in ["pbf", "PBF", "mvt", "pbf.gz"] {
            let m = Metadata {
                format: format.into(),
                ..Metadata::default()
            };
            assert!(m.is_vector(), "{format} should be vector");
        }
        for format in ["png", "jpg", "webp", ""] {
            let m = Metadata {
                format: format.into(),
                ..Metadata::default()
            };
            assert!(!m.is_vector(), "{format} should not be vector");
        }
    }
}
