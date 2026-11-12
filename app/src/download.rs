//! Fetching an area's tiles, on threads of its own.
//!
//! The chart pipeline runs on a desktop with GDAL and tippecanoe, which is
//! why `rutter noaa` cannot run on a phone. Imagery is different: it is
//! plain HTTPS requests for pre-rendered tiles, so the boat can fetch its
//! own — which is the difference between an app an end user can use and one
//! that needs a developer to prepare its data.
//!
//! ## Where the parts live, and why
//!
//! * **What to fetch** is [`rutter_core::chart`]: tile addressing is pure
//!   arithmetic and belongs in the layer with no dependencies.
//! * **How to fetch** is [`TileFetcher`], a trait, here.
//! * **The actual HTTP** is somebody else's: `rutter-fetch` has a rustls
//!   implementation, and a platform that would rather use the operating
//!   system's own stack implements the trait instead.
//!
//! The trait is the point. It keeps a TLS stack out of this crate and out
//! of the core, and it means swapping Android to `HttpsURLConnection`
//! later — for the device's own certificate store, and to honour the
//! captive portals a marina puts in the way — is a contained change rather
//! than a rewrite.
//!
//! ## What a download has to survive
//!
//! * **Missing tiles.** The service has no imagery over open water and
//!   none past the edge of its coverage, so a 404 is the ordinary case and
//!   not a failure. Counted, passed over, and reported separately from
//!   real errors, because "nothing downloaded" and "everything was missing"
//!   are different diagnoses.
//! * **Being cancelled.** A hundred-and-twenty-megabyte download started
//!   by mistake must stop at once, not at the end.
//! * **Being interrupted.** Fetching runs deepest zoom first, so a
//!   download that stops early leaves the detail that was wanted rather
//!   than only the overviews.
//! * **A slow link.** Marina WiFi is slow rather than absent. The work is
//!   spread across a few threads because the limit is latency per tile, not
//!   bandwidth — one connection at a time would make a five-thousand-tile
//!   fetch an afternoon.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;

use rutter_charts::mbtiles::{Metadata, Writer};
use rutter_core::chart::TileId;
use rutter_core::LatLon;

/// What came back for one tile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    /// The tile's bytes, exactly as the server sent them.
    Got(Vec<u8>),
    /// The service has no tile here. Ordinary, not an error.
    Missing,
    /// Something went wrong, with the reason.
    Failed(String),
}

/// Something that can fetch one tile.
///
/// `Send + Sync` because the download spreads tiles across several threads
/// that share one fetcher — which is what lets an HTTP implementation pool
/// its connections, the single biggest difference to how long a download
/// takes.
pub trait TileFetcher: Send + Sync + std::fmt::Debug {
    /// A word for the panel: what this fetches over.
    fn describe(&self) -> String;

    /// Fetch one tile by URL.
    fn fetch(&self, url: &str) -> FetchOutcome;
}

/// Whether a response is worth storing as a tile.
///
/// Two separate questions, and both have cost something to learn:
///
/// 1. **Is it an image at all?** By magic bytes rather than by
///    `Content-Type`, because a service returning the wrong thing is
///    unlikely to label it honestly — and because this is what stops a
///    captive portal's HTML login page being stored as a chart tile.
///
/// 2. **Does it carry a picture?** The USGS endpoint answers an area it
///    has no imagery for with HTTP 200 and a fully transparent PNG, not a
///    404. See [`rutter_charts::is_blank_tile`] for why neither size nor
///    format can be used to tell those apart.
///
/// The format is deliberately *not* required to match the job's declared
/// one. Real tiles at the edge of coverage come back as PNG where the
/// interior is JPEG, because they are part imagery and part transparent.
fn worth_storing(bytes: &[u8]) -> bool {
    let is_image = bytes.starts_with(&[0xFF, 0xD8, 0xFF])
        || bytes.starts_with(&[0x89, b'P', b'N', b'G'])
        || (bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP");

    is_image && !rutter_charts::is_blank_tile(bytes)
}

/// How many tiles are fetched at once.
///
/// The constraint is latency rather than bandwidth: each tile is a request
/// whose round trip dominates its transfer, so a handful in flight is most
/// of the available speed-up. Six rather than sixty because this is a
/// public-domain service run on public money, and a boat should not open
/// sixty connections to it.
const FETCH_THREADS: usize = 6;

/// What to fetch, and where to put it.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    /// North-west and south-east corners of the area.
    pub area: (LatLon, LatLon),
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// Where to write the MBTiles file.
    pub out: std::path::PathBuf,
    /// The tile service, as a template with `{z}`, `{x}` and `{y}` in it.
    pub url_template: String,
    /// What to record as the file's name and credit.
    pub name: String,
    pub attribution: Option<String>,
    /// What the service is expected to return: `jpg`, `png`, `webp`.
    ///
    /// Checked against every response, and the reason is specific. The
    /// USGS endpoint answers an area it has no imagery for with **HTTP
    /// 200 and an 872-byte transparent PNG**, not a 404 — so a download
    /// over open water stored half its tiles as blanks, counted them as
    /// successes, and declared the file full of JPEGs. Nothing errored;
    /// the only visible symptom was a tile that would not decode opaque.
    pub format: String,
}

impl Job {
    /// Every tile this job covers, deepest zoom first.
    ///
    /// Deepest first so that an interrupted download leaves the detail
    /// that was asked for. The overviews are a quarter of the tiles of the
    /// level below them and are quick to pick up afterwards; the finest
    /// level is three quarters of the work and the reason for the download.
    pub fn tiles(&self) -> Vec<TileId> {
        let (nw, se) = self.area;
        let (low, high) = (
            self.min_zoom.min(self.max_zoom),
            self.max_zoom.max(self.min_zoom),
        );
        (low..=high)
            .rev()
            .flat_map(|z| rutter_core::chart::tiles_in_view(nw, se, z))
            .collect()
    }

    /// The URL for one tile.
    ///
    /// `{y}` before `{x}` is a real difference between services — the USGS
    /// ArcGIS endpoint takes row then column — so the order lives in the
    /// template rather than being assumed here.
    pub fn url_for(&self, id: TileId) -> String {
        self.url_template
            .replace("{z}", &id.z.to_string())
            .replace("{x}", &id.x.to_string())
            .replace("{y}", &id.y.to_string())
    }
}

/// Where a download has got to.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub stage: Stage,
    /// Tiles accounted for: stored, missing or failed.
    pub done: usize,
    pub total: usize,
    /// Tiles actually written, and how many bytes they came to.
    pub stored: usize,
    pub bytes: u64,
    /// Tiles the service had none of. Normal over open water.
    pub missing: usize,
    /// Tiles that failed for a reason worth reporting.
    pub failed: usize,
}

impl Progress {
    /// How far along, 0 to 1.
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        (self.done as f32 / self.total as f32).clamp(0.0, 1.0)
    }

    /// Whether this download has stopped, however it stopped.
    pub fn finished(&self) -> bool {
        matches!(
            self.stage,
            Stage::Done | Stage::Cancelled | Stage::Failed(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// Working out the tile list and creating the file.
    Preparing,
    /// Fetching, with the zoom currently being worked on.
    Fetching { zoom: u8 },
    /// Writing the metadata and closing the file.
    Finishing,
    /// Finished and usable.
    Done,
    /// Stopped on request. The file is left in place: a partial download is
    /// still imagery, and throwing it away would waste what was fetched.
    Cancelled,
    /// Could not be finished, with the reason.
    Failed(String),
}

/// A download running somewhere else.
#[derive(Debug)]
pub struct Downloader {
    from_worker: mpsc::Receiver<Progress>,
    last: Progress,
    cancel: Arc<AtomicBool>,
    /// What is being fetched, so the panel can say so after it finishes.
    job: Job,
}

impl Downloader {
    /// Start fetching. `None` if the thread could not be started.
    pub fn start(job: Job, fetcher: Arc<dyn TileFetcher>) -> Option<Self> {
        let (to_app, from_worker) = mpsc::channel::<Progress>();
        let cancel = Arc::new(AtomicBool::new(false));

        let total = job.tiles().len();
        let started = Progress {
            stage: Stage::Preparing,
            done: 0,
            total,
            stored: 0,
            bytes: 0,
            missing: 0,
            failed: 0,
        };

        let shared = Arc::new(job.clone());
        let moved = (shared, cancel.clone(), started.clone());
        if let Err(e) = thread::Builder::new()
            .name("rutter-download".to_owned())
            .spawn(move || {
                let (job, cancel, progress) = moved;
                run(&job, &fetcher, &cancel, progress, &to_app);
            })
        {
            log::error!("could not start the download: {e}");
            return None;
        }

        log::info!(
            "download: {total} tiles z{}..{} to {}",
            job.min_zoom,
            job.max_zoom,
            job.out.display()
        );
        Some(Self {
            from_worker,
            last: started,
            cancel,
            job,
        })
    }

    /// Take whatever has arrived. Never blocks. True if anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(progress) = self.from_worker.try_recv() {
            self.last = progress;
            changed = true;
        }
        changed
    }

    pub fn progress(&self) -> &Progress {
        &self.last
    }

    pub fn job(&self) -> &Job {
        &self.job
    }

    /// Ask it to stop. The threads notice within one tile each.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for Downloader {
    fn drop(&mut self) {
        // Dropping the handle stops the work. Without this a download
        // started by accident would carry on to the end of a hundred
        // megabytes with nothing watching it.
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// One tile's worth of result, on its way to the writer.
struct Fetched {
    id: TileId,
    outcome: FetchOutcome,
}

/// The download's own thread: fetch on several threads, write on this one.
fn run(
    job: &Arc<Job>,
    fetcher: &Arc<dyn TileFetcher>,
    cancel: &Arc<AtomicBool>,
    mut progress: Progress,
    to_app: &mpsc::Sender<Progress>,
) {
    let publish = |progress: &Progress| to_app.send(progress.clone()).is_ok();

    let tiles = Arc::new(job.tiles());
    progress.total = tiles.len();
    if tiles.is_empty() {
        progress.stage = Stage::Failed("that area has no tiles in it".to_owned());
        let _ = publish(&progress);
        return;
    }

    // Created before any fetching, so a path that cannot be written fails
    // immediately rather than after ten minutes of downloading.
    let mut writer = match Writer::create(&job.out) {
        Ok(writer) => writer,
        Err(e) => {
            progress.stage = Stage::Failed(format!("{}: {e}", job.out.display()));
            let _ = publish(&progress);
            return;
        }
    };

    progress.stage = Stage::Fetching { zoom: tiles[0].z };
    if !publish(&progress) {
        return;
    }

    // A shared index into the tile list rather than a queue: the list is
    // fixed and read-only, so an atomic counter is all the coordination
    // needed and there is nothing to lock.
    let next = Arc::new(AtomicUsize::new(0));
    let (results, incoming) = mpsc::channel::<Fetched>();

    let mut threads = Vec::new();
    for _ in 0..FETCH_THREADS.min(tiles.len()) {
        // Each thread gets its own handle on the shared list, the shared
        // counter, the job and the fetcher. Everything crossing the
        // boundary is an `Arc`, which is why none of this needs a lifetime
        // argument or a scoped thread — and why the fetcher can pool its
        // connections across all of them.
        let tiles = tiles.clone();
        let next = next.clone();
        let cancel = cancel.clone();
        let results = results.clone();
        let job = job.clone();
        let fetcher = fetcher.clone();
        match thread::Builder::new()
            .name("rutter-fetch".to_owned())
            .spawn(move || {
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    // An atomic increment is the whole of the work
                    // handout: the list is read-only, so there is nothing
                    // to lock and no queue to drain.
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&id) = tiles.get(index) else {
                        return;
                    };
                    let outcome = fetcher.fetch(&job.url_for(id));
                    if results.send(Fetched { id, outcome }).is_err() {
                        return;
                    }
                }
            }) {
            Ok(handle) => threads.push(handle),
            Err(e) => log::warn!("download: one fetch thread did not start: {e}"),
        }
    }
    // Dropped so that `incoming` ends when the last fetch thread does.
    drop(results);

    let mut first_error: Option<String> = None;
    for Fetched { id, outcome } in incoming {
        match outcome {
            // Not an image, or an image with nothing in it. Counted as
            // missing rather than failed, because that is what it means:
            // the service answered, and its answer was "nothing here".
            FetchOutcome::Got(bytes) if !worth_storing(&bytes) => {
                progress.missing += 1;
                if progress.missing <= 3 {
                    log::debug!(
                        "download: {id:?} came back as {} bytes carrying no picture",
                        bytes.len()
                    );
                }
            }
            FetchOutcome::Got(bytes) => {
                progress.bytes += bytes.len() as u64;
                match writer.insert(id, &bytes) {
                    Ok(()) => progress.stored += 1,
                    Err(e) => {
                        // A write failure is not like a fetch failure: the
                        // disk is full or the file has gone, and carrying
                        // on would produce a file that looks complete.
                        progress.stage = Stage::Failed(format!("writing {id:?}: {e}"));
                        cancel.store(true, Ordering::Relaxed);
                        let _ = publish(&progress);
                        break;
                    }
                }
            }
            FetchOutcome::Missing => progress.missing += 1,
            FetchOutcome::Failed(why) => {
                progress.failed += 1;
                if first_error.is_none() {
                    log::warn!("download: {id:?}: {why}");
                    first_error = Some(why);
                }
            }
        }

        progress.done += 1;
        progress.stage = Stage::Fetching { zoom: id.z };

        // Published every few tiles rather than every one: at six threads
        // this is dozens a second, and a progress bar does not need to be
        // told about each.
        if progress.done.is_multiple_of(16) && !publish(&progress) {
            cancel.store(true, Ordering::Relaxed);
            break;
        }
    }

    for handle in threads {
        let _ = handle.join();
    }

    if matches!(progress.stage, Stage::Failed(_)) {
        let _ = publish(&progress);
        return;
    }

    progress.stage = if cancel.load(Ordering::Relaxed) {
        Stage::Cancelled
    } else {
        Stage::Finishing
    };
    let _ = publish(&progress);

    // The metadata describes what the file actually holds, not what was
    // asked for — a download that stopped early should not claim a zoom
    // range it never reached.
    let stored_zooms = tiles
        .iter()
        .take(progress.done)
        .fold(None::<(u8, u8)>, |range, id| {
            Some(match range {
                None => (id.z, id.z),
                Some((low, high)) => (low.min(id.z), high.max(id.z)),
            })
        })
        .unwrap_or((job.min_zoom, job.max_zoom));

    let (nw, se) = job.area;
    let metadata = Metadata {
        name: job.name.clone(),
        format: job.format.clone(),
        min_zoom: stored_zooms.0,
        max_zoom: stored_zooms.1,
        bounds: Some([nw.lon, se.lat, se.lon, nw.lat]),
        layers: Vec::new(),
        attribution: job.attribution.clone(),
    };

    if let Err(e) = writer.finish(&metadata) {
        progress.stage = Stage::Failed(format!("{}: {e}", job.out.display()));
    } else if progress.stored == 0 {
        // Nothing at all, which is a different problem from a slow
        // download and deserves naming: almost always an area outside the
        // service's coverage.
        progress.stage = Stage::Failed(match first_error {
            Some(why) => format!("nothing downloaded — {why}"),
            None => "nothing downloaded — is that area inside the coverage?".to_owned(),
        });
    } else if !matches!(progress.stage, Stage::Cancelled) {
        progress.stage = Stage::Done;
    }

    log::info!(
        "download: {} stored, {} missing, {} failed, {:.1} MB",
        progress.stored,
        progress.missing,
        progress.failed,
        progress.bytes as f64 / 1_048_576.0
    );
    let _ = publish(&progress);
}
