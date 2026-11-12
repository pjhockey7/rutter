//! Look at an MBTiles file, and throw away what you did not want.
//!
//! A download is a blunt instrument: you mark the screen, and the screen
//! is a rectangle. Mark a view that reaches inland and you fetch a great
//! deal of desert along with the coast — one real download took 617 MB
//! across 134°W to 111°W because the view happened to be zoomed out.
//!
//! So this is the other half of downloading: seeing what a file holds, and
//! removing the parts of it you have no use for. It works on a file the
//! app is not running against — SQLite would mostly cope, but "mostly" is
//! not a basis for the file holding your chart.
//!
//! ```text
//! rutter tiles FILE                           # what is in there
//! rutter tiles FILE --box 32.4,-118.8,33.8,-117.0
//! rutter tiles FILE --zoom 6,14
//! rutter tiles FILE --blanks
//! rutter tiles FILE --box ... --dry-run     # say what it would take
//! ```
//!
//! Nothing is removed without one of the three flags, and `--dry-run` will
//! tell you what a trim would cost before it costs it.

// A maintenance tool, not the app: a panic here is a message to a person
// at a terminal, which is the right way for this to fail.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use rutter_core::chart::{self, TileId};
use rutter_core::LatLon;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        usage();
        std::process::exit(64);
    };

    let mut keep_box: Option<(LatLon, LatLon)> = None;
    let mut keep_zoom: Option<(u8, u8)> = None;
    let mut drop_blanks = false;
    let mut dry_run = false;

    let mut rest = args[1..].iter();
    while let Some(flag) = rest.next() {
        match flag.as_str() {
            "--box" => {
                let text = rest.next().expect("--box needs south,west,north,east");
                keep_box = Some(parse_box(text));
            }
            "--zoom" => {
                let text = rest.next().expect("--zoom needs min,max");
                let (low, high) = text.split_once(',').expect("--zoom wants min,max");
                let (low, high) = (parse_zoom(low), parse_zoom(high));
                keep_zoom = Some((low.min(high), high.max(low)));
            }
            "--blanks" => drop_blanks = true,
            "--dry-run" | "-n" => dry_run = true,
            other => {
                eprintln!("unknown option {other}");
                usage();
                std::process::exit(64);
            }
        }
    }

    let db = rusqlite::Connection::open(path).expect("open the file");
    report(&db, path);

    if keep_box.is_none() && keep_zoom.is_none() && !drop_blanks {
        println!();
        println!("Nothing removed. Pass --box, --zoom or --blanks to trim it.");
        return;
    }

    let doomed = select_doomed(&db, keep_box, keep_zoom, drop_blanks);
    if doomed.is_empty() {
        println!();
        println!("Nothing to remove.");
        return;
    }

    println!();
    if dry_run {
        println!(
            "would remove {} tiles, leaving {}",
            thousands(doomed.len() as u64),
            thousands(count_tiles(&db).saturating_sub(doomed.len() as u64))
        );
        println!("run it again without --dry-run to do it");
        return;
    }

    println!("removing {} tiles", thousands(doomed.len() as u64));
    remove(&db, &doomed);
    retune_metadata(&db, keep_box);

    // Reclaiming the space is the point. Without this the file stays the
    // size it was and the deletion looks like it did nothing.
    println!("compacting (this takes a moment on a large file)");
    db.execute_batch("VACUUM").expect("vacuum");

    println!();
    report(&db, path);
}

fn usage() {
    eprintln!("usage: tiles FILE [--box s,w,n,e] [--zoom min,max] [--blanks] [--dry-run]");
    eprintln!();
    eprintln!("  --box      keep only tiles touching this area");
    eprintln!("  --zoom     keep only these zoom levels");
    eprintln!("  --blanks   remove tiles that carry no picture");
    eprintln!("  --dry-run  say what would go, and stop");
    eprintln!();
    eprintln!("With no flags it only reports. Close the app first.");
}

/// How many tiles the file holds.
fn count_tiles(db: &rusqlite::Connection) -> u64 {
    db.query_row("SELECT count(*) FROM tiles", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0) as u64
}

fn parse_box(text: &str) -> (LatLon, LatLon) {
    let parts: Vec<f64> = text
        .split(',')
        .map(|v| v.trim().parse().expect("--box wants four numbers"))
        .collect();
    assert_eq!(parts.len(), 4, "--box wants south,west,north,east");
    let (south, west, north, east) = (parts[0], parts[1], parts[2], parts[3]);
    assert!(south < north, "--box south must be below north");
    assert!(west < east, "--box west must be left of east");
    // North-west and south-east, which is the order everything else here
    // takes a rectangle in.
    (LatLon::new(north, west), LatLon::new(south, east))
}

fn parse_zoom(text: &str) -> u8 {
    text.trim().parse().expect("a zoom level is a small number")
}

/// What the file holds, by zoom.
fn report(db: &rusqlite::Connection, path: &str) {
    let mut statement = db
        .prepare(
            "SELECT zoom_level, count(*), sum(length(tile_data))
               FROM tiles GROUP BY zoom_level ORDER BY zoom_level",
        )
        .expect("the file should have a tiles table");
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .expect("read the summary");

    let mut by_zoom: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for row in rows {
        let (z, count, bytes) = row.expect("a row");
        by_zoom.insert(z, (count, bytes));
    }

    let total: i64 = by_zoom.values().map(|(n, _)| n).sum();
    let bytes: i64 = by_zoom.values().map(|(_, b)| b).sum();
    println!(
        "{path}: {} tiles, {}",
        thousands(total as u64),
        megabytes(bytes as u64)
    );

    for (z, (count, bytes)) in &by_zoom {
        // Each level is about four times the one above it, so the bar
        // says more about the shape of the file than the numbers do.
        let share = if total > 0 { count * 40 / total } else { 0 };
        println!(
            "  z{z:<3} {:>9} tiles  {:>9}  {}",
            thousands(*count as u64),
            megabytes(*bytes as u64),
            "#".repeat(share.max(0) as usize)
        );
    }

    if let Ok(bounds) = db.query_row(
        "SELECT value FROM metadata WHERE name = 'bounds'",
        [],
        |row| row.get::<_, String>(0),
    ) {
        println!("  bounds {bounds}  (west,south,east,north)");
    }
}

/// Which tiles have to go.
fn select_doomed(
    db: &rusqlite::Connection,
    keep_box: Option<(LatLon, LatLon)>,
    keep_zoom: Option<(u8, u8)>,
    drop_blanks: bool,
) -> Vec<(i64, i64, i64)> {
    let mut statement = db
        .prepare("SELECT zoom_level, tile_column, tile_row, tile_data FROM tiles")
        .expect("read the tiles");
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .expect("read the tiles");

    // The addresses to keep, worked out once per zoom rather than per
    // tile: `tiles_in_view` is the same function the renderer uses to
    // decide what to draw, so "what the box covers" means exactly the same
    // thing here as it does on screen.
    let mut wanted: BTreeMap<u8, std::collections::HashSet<(u32, u32)>> = BTreeMap::new();
    if let Some((nw, se)) = keep_box {
        for z in 0..=22u8 {
            wanted.insert(
                z,
                chart::tiles_in_view(nw, se, z)
                    .into_iter()
                    .map(|id| (id.x, id.y))
                    .collect(),
            );
        }
    }

    let mut doomed = Vec::new();
    let mut by_reason = [0usize; 3];
    for row in rows {
        let (z, x, tms, bytes) = row.expect("a row");
        let zoom = z as u8;

        if let Some((low, high)) = keep_zoom {
            if zoom < low || zoom > high {
                by_reason[0] += 1;
                doomed.push((z, x, tms));
                continue;
            }
        }

        if let Some(inside) = wanted.get(&zoom) {
            // Stored rows are TMS, counted from the south; the tile maths
            // is slippy, counted from the north.
            let y = TileId::grid_size(zoom)
                .saturating_sub(1)
                .saturating_sub(tms as u32);
            if !inside.contains(&(x as u32, y)) {
                by_reason[1] += 1;
                doomed.push((z, x, tms));
                continue;
            }
        }

        if drop_blanks && rutter_charts::is_blank_tile(&bytes) {
            by_reason[2] += 1;
            doomed.push((z, x, tms));
        }
    }

    for (count, why) in by_reason.iter().zip([
        "outside the zoom range",
        "outside the area",
        "carrying no picture",
    ]) {
        if *count > 0 {
            println!("  {} {why}", thousands(*count as u64));
        }
    }
    doomed
}

fn remove(db: &rusqlite::Connection, doomed: &[(i64, i64, i64)]) {
    let mut delete = db
        .prepare("DELETE FROM tiles WHERE zoom_level = ?1 AND tile_column = ?2 AND tile_row = ?3")
        .expect("prepare the delete");
    db.execute_batch("BEGIN").expect("begin");
    for (z, x, y) in doomed {
        delete
            .execute(rusqlite::params![z, x, y])
            .expect("delete a tile");
    }
    drop(delete);
    db.execute_batch("COMMIT").expect("commit");
}

/// Make the file's description match what is left in it.
///
/// A file that still claims a zoom range it no longer has makes its own
/// tiles unreachable: the reader clamps requests to the stated range.
fn retune_metadata(db: &rusqlite::Connection, trimmed_to: Option<(LatLon, LatLon)>) {
    let range: (Option<i64>, Option<i64>) = db
        .query_row(
            "SELECT min(zoom_level), max(zoom_level) FROM tiles",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("the remaining zoom range");

    if let (Some(low), Some(high)) = range {
        for (name, value) in [("minzoom", low), ("maxzoom", high)] {
            db.execute(
                "INSERT OR REPLACE INTO metadata VALUES (?1, ?2)",
                rusqlite::params![name, value.to_string()],
            )
            .expect("record the zoom range");
        }
    }

    if let Some((nw, se)) = trimmed_to {
        db.execute(
            "INSERT OR REPLACE INTO metadata VALUES ('bounds', ?1)",
            rusqlite::params![format!(
                "{:.6},{:.6},{:.6},{:.6}",
                nw.lon, se.lat, se.lon, nw.lat
            )],
        )
        .expect("record the bounds");
    }
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn megabytes(bytes: u64) -> String {
    let mb = bytes as f64 / 1_048_576.0;
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}
