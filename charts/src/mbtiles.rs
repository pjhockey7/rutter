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

/// One vector layer the file declares.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayerInfo {
    /// The layer's name — an S-57 object class for ENC-derived charts.
    pub name: String,
    /// Attribute names the layer's features carry.
    pub fields: Vec<String>,
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
    /// Every vector layer in the file, in the order declared.
    ///
    /// This is what makes a layer picker possible: a real ENC-derived chart
    /// has sixty or more object classes and no caller can know which in
    /// advance. Read from the `json` metadata row, which tippecanoe and
    /// every other writer populates.
    pub layers: Vec<LayerInfo>,
    /// Credit line the file asks to be shown, if any.
    ///
    /// Imagery usually carries one. `rutter imagery` writes the USGS/USDA
    /// credit here even though the data is public domain, because the
    /// courtesy is owed regardless of what the licence compels.
    pub attribution: Option<String>,
}

impl Metadata {
    /// Whether this file holds vector tiles.
    pub fn is_vector(&self) -> bool {
        self.format.eq_ignore_ascii_case("pbf")
            || self.format.eq_ignore_ascii_case("mvt")
            || self.format.eq_ignore_ascii_case("pbf.gz")
    }

    /// Whether this file holds raster imagery.
    ///
    /// Not simply `!is_vector()`: an empty or unrecognised format should be
    /// neither, so a mystery file is rejected by both readers rather than
    /// accepted by one of them.
    pub fn is_raster(&self) -> bool {
        ["jpg", "jpeg", "png", "webp"]
            .iter()
            .any(|f| self.format.eq_ignore_ascii_case(f))
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
    /// Raster tiles where vector ones were expected.
    NotVector(String),
    /// Vector tiles where raster ones were expected.
    NotRaster(String),
    /// A raster tile would not decode.
    Image(image::ImageError),
    /// A file could not be created or removed. Only writing can hit this.
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::NotMbTiles(what) => {
                write!(f, "not an MBTiles file: no {what}")
            }
            Self::Decompress(e) => write!(f, "tile will not decompress: {e}"),
            Self::Mvt(e) => write!(f, "tile will not decode: {e}"),
            Self::NotVector(format) => write!(
                f,
                "tile format '{format}' is raster, but a vector chart was expected"
            ),
            Self::NotRaster(format) => write!(
                f,
                "tile format '{format}' is vector, but raster imagery was expected"
            ),
            Self::Image(e) => write!(f, "tile image will not decode: {e}"),
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
                "json" => meta.layers = parse_layers(&value),
                "attribution" => meta.attribution = Some(value),
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

/// Writing an MBTiles file, a tile at a time.
///
/// Reading has always been here; writing arrived when the app grew the
/// ability to fetch its own imagery. The schema is the same one
/// `rutter imagery` creates from the shell, and deliberately so: a file
/// built on the boat and a file built on a desktop have to be
/// interchangeable, or you cannot pre-load one and top it up with the
/// other.
///
/// One connection, one writer. SQLite will take concurrent readers but
/// serialises writes anyway, so fetching in parallel and inserting from a
/// single thread is both simpler and no slower.
#[derive(Debug)]
pub struct Writer {
    db: rusqlite::Connection,
    /// Tiles inserted since the last commit. Batched because a commit per
    /// tile means an fsync per tile, which on a phone's flash turns a
    /// five-thousand-tile download into a very long afternoon.
    pending: usize,
}

impl Writer {
    /// How many tiles go in one transaction.
    const BATCH: usize = 256;

    /// Create a new file, replacing any existing one.
    ///
    /// Replacing rather than appending: a half-written file from an
    /// interrupted download that then got added to would be a file nobody
    /// could reason about. Resuming is worth having later, and it wants
    /// explicit intent rather than being the accident of reopening.
    pub fn create(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        if path.exists() {
            std::fs::remove_file(path).map_err(Error::Io)?;
        }

        let db = rusqlite::Connection::open(path)?;

        // The schema every MBTiles reader expects, including this crate's
        // own. Spelled out rather than copied from a template so the
        // indexes are not forgotten: without the unique index on
        // (zoom_level, tile_column, tile_row), `INSERT OR REPLACE` cannot
        // replace anything and a retried tile is stored twice.
        db.execute_batch(
            "CREATE TABLE metadata (name text, value text);
             CREATE UNIQUE INDEX metadata_name ON metadata (name);
             CREATE TABLE tiles (zoom_level integer, tile_column integer,
                                 tile_row integer, tile_data blob);
             CREATE UNIQUE INDEX tile_index ON tiles
                                 (zoom_level, tile_column, tile_row);",
        )?;

        // Write-ahead logging and a relaxed sync. Both are safe for this
        // job and both matter on a phone: the file is a cache that can be
        // rebuilt by downloading again, so trading durability-on-power-loss
        // for a few hundred fewer fsyncs is the right way round.
        db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
        db.execute_batch("BEGIN")?;

        Ok(Self { db, pending: 0 })
    }

    /// Store one tile's bytes, exactly as the server sent them.
    ///
    /// Stored unmodified: a JPEG from USGS is already compressed and
    /// re-encoding it would cost quality for nothing.
    pub fn insert(&mut self, id: TileId, bytes: &[u8]) -> Result<(), Error> {
        self.db
            .prepare_cached(
                "INSERT OR REPLACE INTO tiles \
                 (zoom_level, tile_column, tile_row, tile_data) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(rusqlite::params![id.z, id.x, tms_row(id), bytes])?;

        self.pending += 1;
        if self.pending >= Self::BATCH {
            self.commit()?;
        }
        Ok(())
    }

    /// Commit what is pending and open a new transaction.
    pub fn commit(&mut self) -> Result<(), Error> {
        self.db.execute_batch("COMMIT; BEGIN")?;
        self.pending = 0;
        Ok(())
    }

    /// Write the metadata rows and close the file.
    ///
    /// Done at the end rather than the start, because the zoom range and
    /// the bounds are only known once the fetching is over — a download
    /// that stopped early should describe what it actually holds rather
    /// than what was asked for.
    pub fn finish(mut self, metadata: &Metadata) -> Result<(), Error> {
        self.commit()?;

        let mut rows: Vec<(&str, String)> = vec![
            ("name", metadata.name.clone()),
            ("format", metadata.format.clone()),
            ("type", "baselayer".to_owned()),
            ("minzoom", metadata.min_zoom.to_string()),
            ("maxzoom", metadata.max_zoom.to_string()),
        ];
        if let Some([west, south, east, north]) = metadata.bounds {
            rows.push((
                "bounds",
                format!("{west:.6},{south:.6},{east:.6},{north:.6}"),
            ));
        }
        if let Some(credit) = &metadata.attribution {
            rows.push(("attribution", credit.clone()));
        }

        for (name, value) in rows {
            self.db.execute(
                "INSERT OR REPLACE INTO metadata VALUES (?1, ?2)",
                rusqlite::params![name, value],
            )?;
        }

        self.db.execute_batch("COMMIT")?;
        Ok(())
    }

    /// Tiles stored so far.
    pub fn count(&self) -> Result<u64, Error> {
        let n: i64 = self
            .db
            .query_row("SELECT count(*) FROM tiles", [], |r| r.get(0))?;
        Ok(n as u64)
    }
}

/// Fold one MBTiles file's tiles into another, and delete the first.
///
/// What makes a download *add* coverage instead of replacing it. The
/// alternative — writing straight to the file you already have — destroys
/// it: a download of one harbour would wipe a whole coast, and the first
/// time that happened here it cost 35 MB of harbour detail that had taken
/// an afternoon to fetch.
///
/// `target` is created if it does not exist. Tiles already there are
/// overwritten by the new ones, which is the right way round: a tile
/// fetched today is newer than one fetched last year.
///
/// Neither file may be open elsewhere. SQLite would mostly cope, but
/// "mostly" is not a basis for the file that holds the chart — the caller
/// closes its reader first.
pub fn merge(part: &Path, target: &Path) -> Result<(), Error> {
    // Opened read-write and created if absent, so the first download into
    // a fresh install works the same way as the tenth.
    let db = rusqlite::Connection::open(target)?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS metadata (name text, value text);
         CREATE UNIQUE INDEX IF NOT EXISTS metadata_name ON metadata (name);
         CREATE TABLE IF NOT EXISTS tiles (zoom_level integer, tile_column integer,
                                           tile_row integer, tile_data blob);
         CREATE UNIQUE INDEX IF NOT EXISTS tile_index ON tiles
                                           (zoom_level, tile_column, tile_row);",
    )?;

    // One statement rather than a read-and-insert loop: SQLite copies the
    // blobs itself without them passing through this process, which for
    // half a gigabyte of imagery is the difference between seconds and
    // minutes.
    let attached = part.to_string_lossy().replace('\'', "''");
    db.execute_batch(&format!("ATTACH '{attached}' AS incoming"))?;

    let merged = db.execute_batch(
        "BEGIN;
         INSERT OR REPLACE INTO tiles
            SELECT zoom_level, tile_column, tile_row, tile_data FROM incoming.tiles;
         COMMIT;",
    );
    // Detached whether or not the insert worked, or the handle keeps the
    // part file open and it cannot be removed.
    let detached = db.execute_batch("DETACH incoming");
    merged?;
    detached?;

    // The metadata has to describe the union, not the newcomer. A file
    // holding z6 to z16 that claimed z6 to z12 because the last download
    // stopped there would have its detail become unreachable: the reader
    // clamps requests to the stated range.
    let (min_zoom, max_zoom): (Option<i64>, Option<i64>) = db.query_row(
        "SELECT min(zoom_level), max(zoom_level) FROM tiles",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if let (Some(low), Some(high)) = (min_zoom, max_zoom) {
        for (name, value) in [("minzoom", low), ("maxzoom", high)] {
            db.execute(
                "INSERT OR REPLACE INTO metadata VALUES (?1, ?2)",
                rusqlite::params![name, value.to_string()],
            )?;
        }
    }

    // Anything the target has no answer for is taken from the newcomer:
    // on a first download that supplies the name, format and credit, and
    // afterwards it leaves them alone.
    let incoming_meta = read_metadata_rows(part)?;
    for (name, value) in incoming_meta {
        if name == "bounds" || name == "minzoom" || name == "maxzoom" {
            continue;
        }
        db.execute(
            "INSERT OR IGNORE INTO metadata VALUES (?1, ?2)",
            rusqlite::params![name, value],
        )?;
    }

    widen_bounds(&db, part)?;

    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").ok();
    drop(db);

    std::fs::remove_file(part).map_err(Error::Io)?;
    Ok(())
}

/// Every metadata row of a file, without opening it as a chart.
fn read_metadata_rows(path: &Path) -> Result<Vec<(String, String)>, Error> {
    let db = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut stmt = db.prepare("SELECT name, value FROM metadata")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Grow the target's declared bounds to contain the newcomer's.
///
/// Only ever grows. A download of one harbour must not shrink the bounds
/// of a file covering a coast — a reader that trusts the bounds would then
/// refuse to look at imagery that is sitting right there.
fn widen_bounds(db: &rusqlite::Connection, part: &Path) -> Result<(), Error> {
    let parse = |text: &str| -> Option<[f64; 4]> {
        let mut values = text.split(',').map(str::trim).map(str::parse::<f64>);
        let mut out = [0.0; 4];
        for slot in &mut out {
            *slot = values.next()?.ok()?;
        }
        Some(out)
    };

    let incoming = read_metadata_rows(part)?
        .into_iter()
        .find(|(name, _)| name == "bounds")
        .and_then(|(_, value)| parse(&value));
    let Some(incoming) = incoming else {
        return Ok(());
    };

    let existing = db
        .query_row(
            "SELECT value FROM metadata WHERE name = 'bounds'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|value| parse(&value));

    // West, south, east, north.
    let widened = match existing {
        Some(have) => [
            have[0].min(incoming[0]),
            have[1].min(incoming[1]),
            have[2].max(incoming[2]),
            have[3].max(incoming[3]),
        ],
        None => incoming,
    };

    db.execute(
        "INSERT OR REPLACE INTO metadata VALUES ('bounds', ?1)",
        rusqlite::params![format!(
            "{:.6},{:.6},{:.6},{:.6}",
            widened[0], widened[1], widened[2], widened[3]
        )],
    )?;
    Ok(())
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

/// Parse layer names out of the `json` metadata row.
///
/// The row holds a `vector_layers` array of `{id, fields}` objects. Parsed by
/// hand rather than with serde: it is one shape, read once per file, and
/// adding a derive-heavy dependency to a crate that cross-compiles to four
/// Android ABIs for this is a poor trade.
///
/// A malformed or absent row yields no layers, which degrades to "show
/// everything" rather than to an error — a chart that draws without a layer
/// list is far better than one that refuses to open.
fn parse_layers(value: &str) -> Vec<LayerInfo> {
    let Some(start) = value.find("\"vector_layers\"") else {
        return Vec::new();
    };
    let tail = &value[start..];

    let mut out = Vec::new();
    let mut rest = tail;

    // Each layer is an object with an "id". Walk them in order.
    while let Some(at) = rest.find("\"id\"") {
        rest = &rest[at + 4..];
        let Some(name) = next_json_string(rest) else {
            break;
        };

        // Fields for this layer, if the object declares them before the next
        // layer's id begins.
        let next_id = rest.find("\"id\"").unwrap_or(rest.len());
        let scope = &rest[..next_id];
        let fields = match scope.find("\"fields\"") {
            Some(f) => json_object_keys(&scope[f + 8..]),
            None => Vec::new(),
        };

        out.push(LayerInfo { name, fields });
    }
    out
}

/// The first JSON string value after a colon in `s`.
fn next_json_string(s: &str) -> Option<String> {
    let colon = s.find(':')?;
    let rest = &s[colon + 1..];
    let open = rest.find('"')?;
    let after = &rest[open + 1..];
    let close = after.find('"')?;
    Some(after[..close].to_owned())
}

/// Keys of the first JSON object in `s`.
fn json_object_keys(s: &str) -> Vec<String> {
    let Some(open) = s.find('{') else {
        return Vec::new();
    };
    let body = &s[open + 1..];
    let end = body.find('}').unwrap_or(body.len());
    let body = &body[..end];

    let mut keys = Vec::new();
    let mut rest = body;
    while let Some(q) = rest.find('"') {
        let after = &rest[q + 1..];
        let Some(close) = after.find('"') else { break };
        keys.push(after[..close].to_owned());
        // Skip past this key's value to the next key.
        let remainder = &after[close + 1..];
        match remainder.find(',') {
            Some(c) => rest = &remainder[c + 1..],
            None => break,
        }
    }
    keys
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
